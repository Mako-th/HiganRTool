//! 候補アドレスと経路の選び方。候補は host（LAN）/ 外側 IPv4 / IPv6 に分け、優先は IPv6 → LAN → 外側 IPv4。
//! 全部の候補へ HELLO を送り、最初に返しが来た経路を採る。接続コードには候補の列を載せる。
//!
//! pure な決まりだけを持つ（候補の種別と優先、列の上限と重なりのガード、最初に返しが来た経路）。ソケットと送信部分は dll
//! （`net_udp.rs`）と peer_sim、「返し」の判定と `Paths::heard_back` を呼ぶ時機は接続のセッション（`link.rs`）の仕事。
//! ゲームスレッドで呼ばれるので、ヒープを使わない（列は固定の配列 ＋ 個数）。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// 1 つの接続コードに載せる候補の上限。普通の PC で出てくる候補は LAN のアドレス 1〜3 ＋ 外側 IPv4 1 ＋ IPv6 1〜2 で 5〜6 個なので
/// 8 あれば足りる。HELLO を全部の候補へ 20 ms ごとに送っても 1 秒 400 通、コードは base64 で約 300 文字（`code.rs`）。
/// 読む側のガードにもなる（上限を越える数を書いたコードは、中を読む前に断る）。
pub const MAX_CANDIDATES: usize = 8;

/// 候補の種別（接続コードのバイトの値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandKind {
    /// 同じ PC の中（127.0.0.0/8 と `::1`）。検査用（既定の `scope = loopback` ではこれだけで繋ぐ）。
    Loopback = 1,
    Host = 2,
    External = 3,
    Ipv6 = 4,
}

impl CandKind {
    pub fn from_u8(v: u8) -> Option<CandKind> {
        match v {
            1 => Some(CandKind::Loopback),
            2 => Some(CandKind::Host),
            3 => Some(CandKind::External),
            4 => Some(CandKind::Ipv6),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// 優先（小さい方が先）。IPv6 → LAN → 外側 IPv4。loopback は検査用で、本物の候補と並ぶことは無いので先頭に置く。
    pub fn priority(self) -> u8 {
        match self {
            CandKind::Loopback => 0,
            CandKind::Ipv6 => 1,
            CandKind::Host => 2,
            CandKind::External => 3,
        }
    }
}

/// IPv4 の私用・リンクローカル・CGNAT のアドレスか（LAN の候補と見る）。RFC 1918・RFC 3927（169.254/16）・RFC 6598（100.64/10）。
fn is_lan_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_link_local() || (o[0] == 100 && (o[1] & 0xC0) == 64)
}

/// 世界の相手に通じる IPv6 のアドレスか（自分の接続コードの候補に載せてよいか）。
///
/// 2000::/3（グローバルユニキャスト）だけを通し、その中の通じない枠を外す: 2001::/23（IETF の特別用途。Teredo・ベンチマークなど。
/// 通常の割り当ては 2001:200::/23 から）、2001:db8::/32（文書用）、2002::/16（6to4。経路が相手の中継に依る）、
/// 3fff::/20（文書用）。外すのは自分が載せる側だけで、相手のコードの候補を読むガード（`Candidate::new`・`guard`）は変えない。
pub fn is_global_unicast_v6(ip: Ipv6Addr) -> bool {
    let [s0, s1, ..] = ip.segments();
    if s0 & 0xE000 != 0x2000 {
        return false;
    }
    let special_2001 = s0 == 0x2001 && s1 < 0x0200;
    let doc_2001 = s0 == 0x2001 && s1 == 0x0DB8;
    let six_to_four = s0 == 0x2002;
    let doc_3fff = s0 == 0x3FFF && s1 & 0xF000 == 0;
    !(special_2001 || doc_2001 || six_to_four || doc_3fff)
}

/// 自分の IPv6 のアドレスを、`is_global_unicast_v6` を通りポートが 0 でないときだけ候補にする。一時アドレスか固定かは区別しない。
pub fn own_ipv6_candidate(addr: SocketAddr) -> Option<Candidate> {
    let SocketAddr::V6(v6) = addr else {
        return None;
    };
    if !is_global_unicast_v6(*v6.ip()) {
        return None;
    }
    Candidate::new(CandKind::Ipv6, canonical_endpoint(addr)).ok()
}

/// アドレスとポートだけを持つ形にする（flowinfo と scope_id を 0 にする）。OS が `recv_from` で返す IPv6 の送り元は
/// scope_id などを持つことがあり、`SocketAddr` の `==` はそれも比べる。コードの候補と食い違わないよう、受けた送り元はこの形で持つ。
pub fn canonical_endpoint(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip(), a.port())
}

/// 2 つの送り元・宛先が同じエンドポイントか。アドレスとポートだけを比べる。候補の列・学んだ候補のガードは `==` でなくこれを使う。
/// IPv4 を埋め込んだ IPv6（`::ffff:a.b.c.d`）は IPv4 とは別のエンドポイントとして扱う。
pub fn same_endpoint(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && a.ip() == b.ip()
}

/// アドレスから種別を推す。
pub fn kind_of(addr: SocketAddr) -> CandKind {
    match addr.ip() {
        ip if ip.is_loopback() => CandKind::Loopback,
        IpAddr::V6(_) => CandKind::Ipv6,
        IpAddr::V4(v4) if is_lan_v4(v4) => CandKind::Host,
        IpAddr::V4(_) => CandKind::External,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandError {
    Full,
    Duplicate,
    PortZero,
    /// 種別とアドレスが合わない（loopback と書いて LAN のアドレス、未指定アドレス、IPv4 を埋め込んだ IPv6 など）。
    KindMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub kind: CandKind,
    pub addr: SocketAddr,
}

impl Candidate {
    pub fn new(kind: CandKind, addr: SocketAddr) -> Result<Candidate, CandError> {
        if addr.port() == 0 {
            return Err(CandError::PortZero);
        }
        let ip = addr.ip();
        let ok = !ip.is_unspecified()
            && match (kind, ip) {
                (CandKind::Loopback, ip) => ip.is_loopback(),
                (CandKind::Host | CandKind::External, IpAddr::V4(v4)) => !v4.is_loopback(),
                (CandKind::Ipv6, IpAddr::V6(v6)) => {
                    !v6.is_loopback() && v6.to_ipv4_mapped().is_none()
                }
                _ => false,
            };
        if ok {
            Ok(Candidate { kind, addr })
        } else {
            Err(CandError::KindMismatch)
        }
    }

    pub fn guess(addr: SocketAddr) -> Result<Candidate, CandError> {
        Candidate::new(kind_of(addr), addr)
    }
}

const PLACEHOLDER: Candidate = Candidate {
    kind: CandKind::Loopback,
    addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidates {
    items: [Candidate; MAX_CANDIDATES],
    len: u8,
}

impl Default for Candidates {
    fn default() -> Self {
        Candidates::EMPTY
    }
}

impl Candidates {
    pub const EMPTY: Candidates = Candidates {
        items: [PLACEHOLDER; MAX_CANDIDATES],
        len: 0,
    };

    pub fn from_addrs(addrs: &[SocketAddr]) -> Result<Candidates, CandError> {
        let mut c = Candidates::EMPTY;
        for &a in addrs {
            c.push(Candidate::guess(a)?)?;
        }
        Ok(c)
    }

    pub fn push(&mut self, c: Candidate) -> Result<(), CandError> {
        if self.contains(c.addr) {
            return Err(CandError::Duplicate);
        }
        let n = usize::from(self.len);
        if n >= MAX_CANDIDATES {
            return Err(CandError::Full);
        }
        let at = self
            .as_slice()
            .iter()
            .position(|x| x.kind.priority() > c.kind.priority())
            .unwrap_or(n);
        let mut i = n;
        while i > at {
            if let Some(prev) = self.items.get(i - 1).copied()
                && let Some(slot) = self.items.get_mut(i)
            {
                *slot = prev;
            }
            i -= 1;
        }
        if let Some(slot) = self.items.get_mut(at) {
            *slot = c;
        }
        self.len = self.len.saturating_add(1);
        Ok(())
    }

    pub fn as_slice(&self) -> &[Candidate] {
        self.items.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn contains(&self, addr: SocketAddr) -> bool {
        self.as_slice().iter().any(|c| same_endpoint(c.addr, addr))
    }
}

/// 学んでよい送り元の数の上限。学ぶのは、相手のルーターがポートを付け替えたときの「コードに無いアドレス」で、普通は 1、
/// 続いても 2〜3。4 あれば足り、固定長のまま小さく保てる。学んだアドレスへも HELLO を 20 ms ごとに送るので送る量の天井にもなる。
/// 越えたら学ばずに数える（`link::RecvCounts::learn_refused`）。
pub const MAX_LEARNED: usize = 4;

const PLACEHOLDER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnError {
    Full,
    /// 候補として使えないアドレス（ポート 0・未指定アドレス・IPv4 を埋め込んだ IPv6 など）。
    Unusable,
}

/// 学んだ送り元の列（入れた順）。コードの候補とは別に持つ: 混ぜると、手順を作り直したときに学んだものだけを消せない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Learned {
    addrs: [SocketAddr; MAX_LEARNED],
    len: u8,
}

impl Default for Learned {
    fn default() -> Self {
        Learned::EMPTY
    }
}

impl Learned {
    pub const EMPTY: Learned = Learned {
        addrs: [PLACEHOLDER_ADDR; MAX_LEARNED],
        len: 0,
    };

    pub fn as_slice(&self) -> &[SocketAddr] {
        self.addrs.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn contains(&self, addr: SocketAddr) -> bool {
        self.as_slice().iter().any(|a| same_endpoint(*a, addr))
    }

    /// 入れる。候補として使えないアドレスと上限を越えるものは入れない。すでにあるものは Ok。
    fn push(&mut self, addr: SocketAddr) -> Result<(), LearnError> {
        if Candidate::guess(addr).is_err() {
            return Err(LearnError::Unusable);
        }
        if self.contains(addr) {
            return Ok(());
        }
        let n = usize::from(self.len);
        let Some(slot) = self.addrs.get_mut(n) else {
            return Err(LearnError::Full);
        };
        *slot = canonical_endpoint(addr);
        self.len = self.len.saturating_add(1);
        Ok(())
    }
}

/// 経路の選び方: 相手の候補の列と、最初に返しが来た経路。
///
/// - 受けるのは候補のどれかから来たものだけ（`accepts`）。例外: 繋がる前の接続の手順の 1 通（HELLO・HELLO_ACK・AGREE）で
///   タグまで通ったものは、その送り元を学んだ候補として足す（`learn`。ICE の peer reflexive と同じ考え。相手のルーターがポートを
///   付け替えると HELLO はコードのアドレスと別のポートから届く）。学んだアドレスへも HELLO を送り、受けにも PONG の宛先にも使う。
///   経路の外の第三者はタグを作れず学ばせられない。経路の上にいる者が本物の HELLO を別の送り元から送り直せば学ばせられる（ICE と同じ性質）。
/// - 経路を決める前は全部の候補（学んだものを含む）へ送り、決めた後はその経路へだけ送る（決めるのは `heard_back` の最初の 1 回）。
/// - 決めた後も候補のどれかから来たものは受ける。相手から見た経路と自分から見た経路は同じとは限らず、決めた経路以外を捨てると
///   片方だけ届かなくなる筋がある。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paths {
    cands: Candidates,
    learned: Learned,
    chosen: Option<SocketAddr>,
}

impl Paths {
    pub fn new(cands: Candidates) -> Paths {
        Paths {
            cands,
            learned: Learned::EMPTY,
            chosen: None,
        }
    }

    /// 接続コードの候補だけ（学んだものは含まない）。手順を最初から作り直すときの種になる。
    pub fn candidates(&self) -> &Candidates {
        &self.cands
    }

    pub fn learned(&self) -> &Learned {
        &self.learned
    }

    pub fn accepts(&self, from: SocketAddr) -> bool {
        self.cands.contains(from) || self.learned.contains(from)
    }

    /// 候補に無い送り元 `from` を、学んだ候補として足す。繋がる前の接続の手順の 1 通がタグまで通った後だけ呼ぶ（ガードは `link.rs`）。
    pub fn learn(&mut self, from: SocketAddr) -> Result<(), LearnError> {
        if self.accepts(from) {
            return Ok(());
        }
        self.learned.push(from)
    }

    pub fn chosen_is_learned(&self) -> bool {
        self.chosen.is_some_and(|c| self.learned.contains(c))
    }

    /// 採った経路が学んだ候補なら、経路の選びだけ戻す（学んだ列は残す）。手順を引き継いで作り直す（`Restart::Resume`）ときに呼ぶ:
    /// 学んだアドレスはルーターが付け替えるアドレスで、時間切れになったならもう死んでいることがあり、戻さないと死んだアドレスへだけ送り続ける。
    pub fn unchoose_learned(&mut self) -> bool {
        if self.chosen_is_learned() {
            self.chosen = None;
            return true;
        }
        false
    }

    /// 相手が自分を聞いた印（HELLO への返し・AGREE）が `from` から届いた。まだ決めていなければその経路に決め、true を返す。
    pub fn heard_back(&mut self, from: SocketAddr) -> bool {
        if self.chosen.is_none() && self.accepts(from) {
            self.chosen = Some(from);
            return true;
        }
        false
    }

    pub fn chosen(&self) -> Option<SocketAddr> {
        self.chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn kinds_are_guessed_from_addresses() {
        for (a, k) in [
            ("127.0.0.1:1", CandKind::Loopback),
            ("127.9.9.9:1", CandKind::Loopback),
            ("[::1]:1", CandKind::Loopback),
            ("192.168.1.2:1", CandKind::Host),
            ("10.1.2.3:1", CandKind::Host),
            ("172.16.0.1:1", CandKind::Host),
            ("172.31.255.1:1", CandKind::Host),
            ("169.254.3.4:1", CandKind::Host),
            ("100.64.0.1:1", CandKind::Host),
            ("100.127.255.1:1", CandKind::Host),
            ("172.32.0.1:1", CandKind::External),
            ("100.128.0.1:1", CandKind::External),
            ("203.0.113.9:1", CandKind::External),
            ("[2001:db8::1]:1", CandKind::Ipv6),
        ] {
            assert_eq!(kind_of(sa(a)), k, "{a}");
        }
    }

    #[test]
    fn candidate_gate() {
        use CandKind::*;
        for (k, a) in [
            (Loopback, "127.0.0.1:7000"),
            (Loopback, "[::1]:7000"),
            (Host, "192.168.0.2:7000"),
            (External, "203.0.113.5:7000"),
            (Ipv6, "[2001:db8::5]:7000"),
        ] {
            assert!(Candidate::new(k, sa(a)).is_ok(), "{k:?} {a}");
        }
        for (k, a, e) in [
            (Loopback, "192.168.0.2:7000", CandError::KindMismatch),
            (Host, "127.0.0.1:7000", CandError::KindMismatch),
            (External, "[2001:db8::5]:7000", CandError::KindMismatch),
            (Ipv6, "203.0.113.5:7000", CandError::KindMismatch),
            (Ipv6, "[::ffff:203.0.113.5]:7000", CandError::KindMismatch),
            (Ipv6, "[::1]:7000", CandError::KindMismatch),
            (External, "0.0.0.0:7000", CandError::KindMismatch),
            (Ipv6, "[::]:7000", CandError::KindMismatch),
            (Loopback, "127.0.0.1:0", CandError::PortZero),
        ] {
            assert_eq!(Candidate::new(k, sa(a)), Err(e), "{k:?} {a}");
        }
    }

    fn v6(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    /// 期待値は RFC の区切りから書いた（判定の式から作らない）。表は「通す」「断る」を同じ長さで並べ、全部断る・全部通す実装を落とす。
    #[test]
    fn only_global_unicast_v6_outside_the_special_blocks_is_ours_to_publish() {
        for ok in [
            "2000::1",
            "2001:200::1",  // 2001::/23 の外の最初（APNIC の 2001:200::/23）
            "2001:4860::1", // 通常の割り当て
            "2003::1",      // 2002::/16 の隣
            "2400:cb00::1",
            "2a00:1450::1",
            "3fff:1000::1",                            // 文書用 3fff::/20 の外の最初
            "3ffe::1",                                 // 3fff の隣
            "3fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", // 2000::/3 の最後
        ] {
            assert!(is_global_unicast_v6(v6(ok)), "通すはずの {ok} を断った");
        }
        for ng in [
            "2001::1",                              // Teredo（RFC 4380。2001::/32）
            "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredo の実際の形
            "2001:2::1",                            // ベンチマーク 2001:2::/48
            "2001:db8::1",                          // 文書用（RFC 3849）
            "2001:1ff:ffff::1",                     // 2001::/23 の最後
            "2002::1",                              // 6to4（RFC 3056）
            "2002:c000:204::1",
            "2002:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff::1",          // 文書用（RFC 9637。3fff::/20）
            "3fff:fff:ffff::1", // 3fff::/20 の最後
            "::1",
            "::",
            "fe80::1",
            "fe80::1234:5678:9abc:def0",
            "fc00::1",
            "fd12:3456:789a::1",
            "ff02::1",
            "ff0e::1",
            "::ffff:203.0.113.5",
            "::203.0.113.5",
            "1fff:ffff::1", // 2000::/3 の直前
            "4000::1",      // 2000::/3 の直後
            "5f00::1",
            "8000::1",
        ] {
            assert!(
                !is_global_unicast_v6(v6(ng)),
                "載せてはいけない {ng} を通した"
            );
        }
    }

    #[test]
    fn an_own_ipv6_candidate_is_made_only_from_a_global_address_with_a_port() {
        let c = own_ipv6_candidate(sa("[2400:cb00::7]:47001"))
            .expect("世界に通じるアドレスは候補になる");
        assert_eq!(c.kind, CandKind::Ipv6);
        assert_eq!(c.addr, sa("[2400:cb00::7]:47001"));
        let dirty = SocketAddr::V6(std::net::SocketAddrV6::new(v6("2400:cb00::7"), 47001, 9, 4));
        assert_eq!(
            own_ipv6_candidate(dirty).map(|c| c.addr),
            Some(sa("[2400:cb00::7]:47001")),
            "scope_id・flowinfo を持ち越した"
        );
        for bad in [
            "[2001::1]:47001",
            "[2002::1]:47001",
            "[2001:db8::5]:47001",
            "[fe80::1]:47001",
            "[fd00::1]:47001",
            "[::1]:47001",
            "[::]:47001",
            "[::ffff:203.0.113.5]:47001",
            "203.0.113.5:47001",
            "192.168.0.2:47001",
            "[2400:cb00::7]:0",
        ] {
            assert_eq!(own_ipv6_candidate(sa(bad)), None, "{bad}");
        }
    }

    #[test]
    fn endpoints_are_compared_by_address_and_port_only() {
        let base = sa("[2400:cb00::7]:47001");
        let with_scope = SocketAddr::V6(std::net::SocketAddrV6::new(
            v6("2400:cb00::7"),
            47001,
            0,
            12,
        ));
        let with_flow = SocketAddr::V6(std::net::SocketAddrV6::new(
            v6("2400:cb00::7"),
            47001,
            0xABCDE,
            0,
        ));
        assert_ne!(
            base, with_scope,
            "この検査の前提: std の == は scope_id を見る"
        );
        assert_ne!(
            base, with_flow,
            "この検査の前提: std の == は flowinfo を見る"
        );
        assert!(same_endpoint(base, with_scope));
        assert!(same_endpoint(with_flow, base));
        assert!(same_endpoint(with_flow, with_scope));
        assert_eq!(canonical_endpoint(with_scope), base);
        assert_eq!(canonical_endpoint(with_flow), base);
        assert!(
            !same_endpoint(base, sa("[2400:cb00::7]:47002")),
            "ポートの違い"
        );
        assert!(
            !same_endpoint(base, sa("[2400:cb00::8]:47001")),
            "アドレスの違い"
        );
        assert!(
            !same_endpoint(sa("203.0.113.5:1"), sa("[::ffff:203.0.113.5]:1")),
            "IPv4 と、それを埋め込んだ IPv6 は別のエンドポイント"
        );
        assert!(same_endpoint(sa("203.0.113.5:1"), sa("203.0.113.5:1")));
        assert_eq!(
            canonical_endpoint(sa("203.0.113.5:1")),
            sa("203.0.113.5:1"),
            "IPv4 を変えた"
        );
    }

    #[test]
    fn the_candidate_and_learned_gates_see_ipv6_sources_by_address_and_port() {
        let code = sa("[2400:cb00::7]:47001");
        let mut p = Paths::new(Candidates::from_addrs(&[sa("203.0.113.9:7000"), code]).unwrap());
        let echoed = SocketAddr::V6(std::net::SocketAddrV6::new(
            v6("2400:cb00::7"),
            47001,
            0x1234,
            7,
        ));
        assert!(
            p.accepts(echoed),
            "scope_id 付きの送り元をコードの候補と見ない"
        );
        assert!(
            p.heard_back(echoed),
            "コードの候補の返しで経路を決められない"
        );
        assert_eq!(p.learn(echoed), Ok(()));
        assert!(p.learned().is_empty(), "コードの候補を学んだ候補に入れた");
        assert!(
            Candidates::from_addrs(&[code, echoed]).is_err(),
            "同じエンドポイントが scope_id だけ違うからと 2 度入った"
        );

        let moved = SocketAddr::V6(std::net::SocketAddrV6::new(v6("2400:cb00::7"), 47099, 0, 5));
        let mut q = Paths::new(Candidates::from_addrs(&[code]).unwrap());
        assert!(!q.accepts(moved));
        assert_eq!(q.learn(moved), Ok(()));
        assert!(q.learned().contains(sa("[2400:cb00::7]:47099")));
        assert!(q.accepts(sa("[2400:cb00::7]:47099")));
        assert_eq!(q.learned().as_slice(), [sa("[2400:cb00::7]:47099")]);
        assert_eq!(q.learn(sa("[2400:cb00::7]:47099")), Ok(()));
        assert_eq!(q.learned().len(), 1, "scope_id の有無で 2 度学んだ");
        assert!(
            q.accepts(moved),
            "scope_id 付きの送り元を学んだ候補と見ない"
        );
        assert!(q.learned().contains(moved));
        assert_eq!(q.learn(moved), Ok(()));
        assert_eq!(q.learned().len(), 1, "scope_id 付きで 2 度目を学んだ");
        assert_eq!(
            q.learn(sa("[::ffff:203.0.113.9]:7005")),
            Err(LearnError::Unusable)
        );
    }

    #[test]
    fn candidates_are_kept_in_priority_order() {
        let c = Candidates::from_addrs(&[
            sa("203.0.113.5:1"),
            sa("192.168.0.2:1"),
            sa("[2001:db8::1]:1"),
            sa("10.0.0.2:1"),
            sa("198.51.100.7:1"),
            sa("127.0.0.1:1"),
        ])
        .unwrap();
        let got: Vec<String> = c.as_slice().iter().map(|x| x.addr.to_string()).collect();
        assert_eq!(
            got,
            [
                "127.0.0.1:1",
                "[2001:db8::1]:1",
                "192.168.0.2:1",
                "10.0.0.2:1",
                "203.0.113.5:1",
                "198.51.100.7:1"
            ]
        );
        let mut full = Candidates::EMPTY;
        for p in 1..=MAX_CANDIDATES as u16 {
            full.push(Candidate::guess(SocketAddr::from(([127, 0, 0, 1], p))).unwrap())
                .unwrap();
        }
        assert_eq!(
            full.push(Candidate::guess(sa("127.0.0.1:999")).unwrap()),
            Err(CandError::Full)
        );
        assert_eq!(
            Candidates::from_addrs(&[sa("127.0.0.1:5"), sa("127.0.0.1:5")]),
            Err(CandError::Duplicate)
        );
    }

    #[test]
    fn first_answer_from_a_candidate_decides_the_path() {
        let c = Candidates::from_addrs(&[sa("127.0.0.1:1"), sa("127.0.0.2:2")]).unwrap();
        let mut p = Paths::new(c);
        assert!(p.accepts(sa("127.0.0.2:2")));
        assert!(!p.accepts(sa("127.0.0.3:3")));
        assert!(!p.heard_back(sa("127.0.0.3:3")), "候補にない送り元で決めた");
        assert_eq!(p.chosen(), None);
        assert!(p.heard_back(sa("127.0.0.2:2")));
        assert!(!p.heard_back(sa("127.0.0.1:1")), "決め直した");
        assert_eq!(p.chosen(), Some(sa("127.0.0.2:2")));
        assert!(p.accepts(sa("127.0.0.1:1")));
    }

    #[test]
    fn learned_sources_are_accepted_but_kept_apart_from_the_code_candidates() {
        let c = Candidates::from_addrs(&[sa("203.0.113.9:7000")]).unwrap();
        let mut p = Paths::new(c);
        let stranger = sa("203.0.113.9:7001");
        assert!(!p.accepts(stranger));
        assert!(
            !p.heard_back(stranger),
            "学ぶ前は候補に無い送り元で決まらない"
        );
        assert_eq!(p.learn(stranger), Ok(()));
        assert!(p.accepts(stranger));
        assert!(p.learned().contains(stranger));
        assert_eq!(p.candidates().len(), 1, "コードの候補に混ざった");
        assert!(!p.chosen_is_learned());
        assert!(p.heard_back(stranger), "学んだ送り元で経路を決められない");
        assert!(p.chosen_is_learned());
        assert_eq!(p.learn(stranger), Ok(()));
        assert_eq!(p.learn(sa("203.0.113.9:7000")), Ok(()));
        assert_eq!(p.learned().len(), 1);
        for bad in ["203.0.113.9:0", "0.0.0.0:7005", "[::ffff:203.0.113.9]:7005"] {
            assert_eq!(p.learn(sa(bad)), Err(LearnError::Unusable), "{bad}");
        }
        for i in 1..MAX_LEARNED as u16 {
            assert_eq!(
                p.learn(SocketAddr::from(([203, 0, 113, 9], 7100 + i))),
                Ok(())
            );
        }
        assert_eq!(p.learned().len(), MAX_LEARNED);
        let over = sa("203.0.113.9:7200");
        assert_eq!(p.learn(over), Err(LearnError::Full));
        assert!(!p.accepts(over), "断ったのに受ける");
        assert!(p.accepts(stranger), "上限で断った後、学んだものを忘れた");
        let fresh = Paths::new(*p.candidates());
        assert!(!fresh.accepts(stranger) && fresh.learned().is_empty());
    }
}
