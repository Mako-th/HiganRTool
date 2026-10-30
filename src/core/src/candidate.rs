//! 候補アドレスと経路の選び方。候補はhost（LAN）/ 外側IPv4 / IPv6に分け、優先はIPv6 → LAN → 外側IPv4。
//! 全部の候補へHELLOを送り、最初に返答が来た経路を採る。接続コードには候補の列を載せる。
//!
//! pureな決まりだけを持つ（候補の種別と優先・列の上限と重なりのガード・最初に返答が来た経路）。
//! ソケットと送信部分はdll（`net_udp.rs`）とpeer_simの仕事。
//! 「返答」の判定と`Paths::heard_back`を呼ぶタイミングは接続のセッション（`link.rs`）の仕事。
//! ゲームスレッドで呼ばれるのでヒープを使わない（列は固定の配列 ＋ 個数）。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// 1つの接続コードに載せる候補の上限。
/// 普通のPCで出てくる候補はLANのアドレス1〜3 ＋ 外側IPv4 1 ＋ IPv6 1〜2で5〜6個なので8あれば足りる。
/// HELLOを全部の候補へ20 msごとに送っても1秒400通。コードはbase64で約300文字（`code.rs`）。
/// 読む側のガードにもなる（上限を越える数を書いたコードは中を読む前に断る）。
pub const MAX_CANDIDATES: usize = 8;

/// 候補の種別（接続コードのバイトの値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandKind {
    /// 同じPCの中（127.0.0.0/8と`::1`）。検査用（既定の`scope = loopback`ではこれだけで接続する）。
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

    /// 優先（小さい方が先）。IPv6 → LAN → 外側IPv4。loopbackは検査用で本物の候補と並ぶことは無いので先頭にする。
    pub fn priority(self) -> u8 {
        match self {
            CandKind::Loopback => 0,
            CandKind::Ipv6 => 1,
            CandKind::Host => 2,
            CandKind::External => 3,
        }
    }
}

/// IPv4の私用・リンクローカル・CGNATのアドレスか（LANの候補と見る）。RFC 1918・RFC 3927（169.254/16）・RFC 6598（100.64/10）。
fn is_lan_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_link_local() || (o[0] == 100 && (o[1] & 0xC0) == 64)
}

/// インターネット上の相手に通じるIPv6のアドレスかどうか（自分の接続コードの候補に載せてよいか）。
///
/// 2000::/3（グローバルユニキャスト）だけを通し、通じない範囲を外す: 2001::/23（IETFの特別用途。通常の割り当ては2001:200::/23から）、2001:db8::/32と3fff::/20（文書用）、2002::/16（6to4。経路が相手の中継に依る）。
/// 外すのは自分が載せる側だけ。相手のコードの候補を読むガード（`Candidate::new`・`guard`）は変えない。
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

/// 自分のIPv6のアドレスのうち`is_global_unicast_v6`を通りポートが0でないものだけを候補にする。一時アドレスか固定かは区別しない。
pub fn own_ipv6_candidate(addr: SocketAddr) -> Option<Candidate> {
    let SocketAddr::V6(v6) = addr else {
        return None;
    };
    if !is_global_unicast_v6(*v6.ip()) {
        return None;
    }
    Candidate::new(CandKind::Ipv6, canonical_endpoint(addr)).ok()
}

/// アドレスとポートだけを持つ形にする（flowinfoとscope_idを0にする）。
/// OSが`recv_from`で返すIPv6の送信元はscope_idなどを持つことがある。`SocketAddr`の`==`はそれも比べる。
/// コードの候補と食い違わないように、受けた送信元はこの形で持つ。
pub fn canonical_endpoint(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip(), a.port())
}

/// 2つの送信元・宛先が同じエンドポイントかどうか。アドレスとポートだけを比べる。候補の列・学んだ候補のガードは`==`でなくこれを使う。
/// IPv4を埋め込んだIPv6（`::ffff:a.b.c.d`）はIPv4とは別のエンドポイントとして扱う。
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
    /// 種別とアドレスが合わない（loopbackと書いてLANのアドレス、未指定アドレス、IPv4を埋め込んだIPv6など）。
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

/// 候補1つのバイト列の最大（種別1・family 1・IPv6 16・ポート2）。
pub const CAND_WIRE_MAX: usize = 20;
/// 候補の並び（個数1 B ＋ 候補）のバイト列の最大。
pub const LIST_WIRE_MAX: usize = 1 + MAX_CANDIDATES * CAND_WIRE_MAX;

/// 候補の並びを読めなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListError {
    /// バイトが足りない（`need`は読もうとした所の終わり。並びの先頭から数える）。
    Short { need: usize },
    /// 個数が`MAX_CANDIDATES`を越える（中は個数）。
    TooMany(u8),
    /// 候補`index`の中身がおかしい（知らない種別・familyは`err`がNone）。
    Bad { index: u8, err: Option<CandError> },
}

/// 候補の並びを接続コードと観戦者へのパケットに共通の形で`out`へ書き、書いたバイト数を返す。
/// 形: 個数u8 ‖ 候補ごとに 種別u8・family u8（4か6）・アドレス4か16 B・ポートu16 LE。確保しない。
pub fn write_list(c: &Candidates, out: &mut [u8; LIST_WIRE_MAX]) -> usize {
    // 個数は`MAX_CANDIDATES`以下（`Candidates`が保つ）なので`LIST_WIRE_MAX`の中に収まる。
    out[0] = c.len() as u8;
    let mut at = 1;
    for cand in c.as_slice() {
        out[at] = cand.kind.to_u8();
        at += 1;
        match cand.addr.ip() {
            IpAddr::V4(ip) => {
                out[at] = 4;
                out[at + 1..at + 5].copy_from_slice(&ip.octets());
                at += 5;
            }
            IpAddr::V6(ip) => {
                out[at] = 6;
                out[at + 1..at + 17].copy_from_slice(&ip.octets());
                at += 17;
            }
        }
        out[at..at + 2].copy_from_slice(&cand.addr.port().to_le_bytes());
        at += 2;
    }
    at
}

/// [`write_list`]の形の並びを`b`の先頭から読み、並びと読んだバイト数を返す（後ろの余りは呼び出し側が見る）。個数0は通す。
/// 重なり・種別とアドレスの食い違いは`Candidates::push`・`Candidate::new`と同じに断る。確保しない。
pub fn read_list(b: &[u8]) -> Result<(Candidates, usize), ListError> {
    let take = |at: usize, n: usize| b.get(at..at + n).ok_or(ListError::Short { need: at + n });
    let count = take(0, 1)?[0];
    if usize::from(count) > MAX_CANDIDATES {
        return Err(ListError::TooMany(count));
    }
    let mut list = Candidates::EMPTY;
    let mut at = 1;
    for index in 0..count {
        let bad = |err| ListError::Bad { index, err };
        let kind = CandKind::from_u8(take(at, 1)?[0]).ok_or(bad(None))?;
        let ip = match take(at + 1, 1)?[0] {
            4 => {
                let mut o = [0u8; 4];
                o.copy_from_slice(take(at + 2, 4)?);
                at += 6;
                IpAddr::V4(Ipv4Addr::from(o))
            }
            6 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(take(at + 2, 16)?);
                at += 18;
                IpAddr::V6(Ipv6Addr::from(o))
            }
            _ => return Err(bad(None)),
        };
        let p = take(at, 2)?;
        let port = u16::from_le_bytes([p[0], p[1]]);
        at += 2;
        let c = Candidate::new(kind, SocketAddr::new(ip, port)).map_err(|e| bad(Some(e)))?;
        list.push(c).map_err(|e| bad(Some(e)))?;
    }
    Ok((list, at))
}

/// 学んでよい送信元の数の上限。
/// 学ぶのは相手のルーターがポートを付け替えたときの「コードに無いアドレス」。普通は1つ。続いても2〜3つ。
/// 4あれば足りて固定長のまま小さく保てる。学んだアドレスへもHELLOを20 msごとに送るので送る量の天井にもなる。
/// 越えたら学ばずに数える（`link::RecvCounts::learn_refused`）。
pub const MAX_LEARNED: usize = 4;

const PLACEHOLDER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnError {
    Full,
    /// 候補として使えないアドレス（ポート0・未指定アドレス・IPv4を埋め込んだIPv6など）。
    Unusable,
}

/// 学んだ送信元の列（追加した順）。コードの候補とは別に持つ。混ぜると手順を作り直したときに学んだものだけを消せない。
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

    /// 追加する。候補として使えないアドレスと上限を越えるものは追加しない。すでにあるものはOk。
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

/// 経路の選び方: 相手の候補の列と最初に返答が来た経路。
///
/// - 受けるのは候補のどれかから来たものだけ（`accepts`）。
///   例外は接続前の手順の1通のうち、自分の今のセッションのHELLOへのHELLO_ACKと封じたAGREE。
///   どちらも送信元を学んだ候補として足す（`learn`。ICEのpeer reflexiveと同じ考え）。
///   相手のルーターがポートを付け替えると相手のパケットはコードのアドレスと別のポートから届く。
///   HELLOは盗み見たものを誰でも再送できるので学ばず、その送信元へ1回だけ返す（`link.rs`）。学んだアドレスへもHELLOを送る。
/// - 経路の決定前は全部の候補（学んだものを含む）へ送る。決定後はその経路へだけ送る（決めるのは`heard_back`の最初の1回）。
///   相手がコードの候補にいると分かれば学んだ送信元より候補を採る（`heard_back`の`peer_at_code`と`prefer_code`）。
/// - 決定後も候補のどれかから来たものは受ける。相手から見た経路と自分から見た経路は同じとは限らない。決定済みの経路以外を捨てると片方だけ届かなくなることがある。
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

    /// 接続コードの候補だけ（学んだものは含まない）。手順を最初から作り直すときの元になる。
    pub fn candidates(&self) -> &Candidates {
        &self.cands
    }

    pub fn learned(&self) -> &Learned {
        &self.learned
    }

    pub fn accepts(&self, from: SocketAddr) -> bool {
        self.cands.contains(from) || self.learned.contains(from)
    }

    /// 候補に無い送信元`from`を学んだ候補として足す。接続前の手順の1通がタグまで通った後だけ呼ぶ（ガードは`link.rs`）。
    pub fn learn(&mut self, from: SocketAddr) -> Result<(), LearnError> {
        if self.accepts(from) {
            return Ok(());
        }
        self.learned.push(from)
    }

    pub fn chosen_is_learned(&self) -> bool {
        self.chosen.is_some_and(|c| self.learned.contains(c))
    }

    /// 採った経路が学んだ候補なら経路の選択だけ戻す（学んだ列は残す）。
    /// 手順を引き継いで作り直す（`Restart::Resume`）ときに接続のセッション（`link.rs`）が呼ぶ。
    /// 学んだアドレスはルーターが付け替えるアドレスで、時間切れになったならもう死んでいることがある。戻さないと死んだアドレスへだけ送り続ける。
    pub fn unchoose_learned(&mut self) -> bool {
        if self.chosen_is_learned() {
            self.chosen = None;
            return true;
        }
        false
    }

    /// 相手が自分を聞いた証拠（HELLOへの返答・AGREE）が`from`から届いた。経路がまだ決定していなければその経路に決めてtrueを返す。
    /// `peer_at_code`は相手のパケットが接続コードの候補から届いていること。そのときは学んだ送信元には決めない。
    /// 証拠はタグか封で相手のものと分かるが、送信元は分からない（盗み見た証拠を別のアドレスから再送できる）。
    pub fn heard_back(&mut self, from: SocketAddr, peer_at_code: bool) -> bool {
        if self.chosen.is_some() || !self.accepts(from) {
            return false;
        }
        if peer_at_code && !self.cands.contains(from) {
            return false;
        }
        self.chosen = Some(from);
        true
    }

    /// 経路が学んだ送信元に決定済みのときに接続コードの候補`from`から相手が自分を聞いた証拠が届いたら、候補へ決め直してtrueを返す（学んだ方は盗み見た証拠を再送されたものかもしれない）。
    /// 接続する前だけ呼ぶ（接続の経路は接続したときに決まる）。
    pub fn prefer_code(&mut self, from: SocketAddr) -> bool {
        if self.chosen_is_learned() && self.cands.contains(from) {
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

    /// 期待値はRFCの区切りから書いた（判定の式から作らない）。表は「通す」「断る」を同じ長さで並べ、全部断る・全部通す実装を落とす。
    #[test]
    fn only_global_unicast_v6_outside_the_special_blocks_is_ours_to_publish() {
        for ok in [
            "2000::1",
            "2001:200::1",  // 2001::/23の外の最初（APNICの2001:200::/23）
            "2001:4860::1", // 通常の割り当て
            "2003::1",      // 2002::/16の隣
            "2400:cb00::1",
            "2a00:1450::1",
            "3fff:1000::1",                            // 文書用3fff::/20の外の最初
            "3ffe::1",                                 // 3fffの隣
            "3fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", // 2000::/3の最後
        ] {
            assert!(is_global_unicast_v6(v6(ok)), "通すはずの {ok} を断った");
        }
        for ng in [
            "2001::1",                              // Teredo（RFC 4380。2001::/32）
            "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredoの実際の形
            "2001:2::1",                            // ベンチマーク2001:2::/48
            "2001:db8::1",                          // 文書用（RFC 3849）
            "2001:1ff:ffff::1",                     // 2001::/23の最後
            "2002::1",                              // 6to4（RFC 3056）
            "2002:c000:204::1",
            "2002:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff::1",          // 文書用（RFC 9637。3fff::/20）
            "3fff:fff:ffff::1", // 3fff::/20の最後
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
            "1fff:ffff::1", // 2000::/3の直前
            "4000::1",      // 2000::/3の直後
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
            p.heard_back(echoed, false),
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
        assert!(
            !p.heard_back(sa("127.0.0.3:3"), false),
            "候補にない送り元で決めた"
        );
        assert_eq!(p.chosen(), None);
        assert!(p.heard_back(sa("127.0.0.2:2"), false));
        assert!(!p.heard_back(sa("127.0.0.1:1"), false), "決め直した");
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
            !p.heard_back(stranger, false),
            "学ぶ前は候補に無い送り元で決まらない"
        );
        assert_eq!(p.learn(stranger), Ok(()));
        assert!(p.accepts(stranger));
        assert!(p.learned().contains(stranger));
        assert_eq!(p.candidates().len(), 1, "コードの候補に混ざった");
        assert!(!p.chosen_is_learned());
        assert!(
            p.heard_back(stranger, false),
            "学んだ送り元で経路を決められない"
        );
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

    #[test]
    fn a_code_candidate_wins_over_a_learned_source_once_the_peer_is_seen_at_the_code() {
        let code = sa("203.0.113.9:7000");
        let relay = sa("198.51.100.4:9000");
        let mut p = Paths::new(Candidates::from_addrs(&[code]).unwrap());
        assert_eq!(p.learn(relay), Ok(()));
        assert!(
            !p.heard_back(relay, true),
            "相手がコードの候補にいるのに学んだ送り元で決めた"
        );
        assert_eq!(p.chosen(), None);
        assert!(p.heard_back(code, true));
        assert_eq!(p.chosen(), Some(code));
        assert!(!p.prefer_code(code), "候補に決めた経路を決め直した");

        let mut q = Paths::new(Candidates::from_addrs(&[code]).unwrap());
        assert_eq!(q.learn(relay), Ok(()));
        assert!(!q.prefer_code(code), "経路を決める前に決めた");
        assert!(q.heard_back(relay, false));
        assert!(!q.prefer_code(relay), "学んだ送り元へ決め直した");
        assert!(q.prefer_code(code));
        assert_eq!(q.chosen(), Some(code));
        assert!(!q.chosen_is_learned());
    }
}
