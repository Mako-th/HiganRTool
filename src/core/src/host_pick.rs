//! ホストの決め方。接続の手順の中で穴あけしやすく上りの太い方をホストにする（ホストは観戦の記録を作り、サイド・ディレイ・Dを決める側）。
//!
//! 相手のHELLOを受けたらその送り元へだけPROBEを`PROBE_COUNT`通続けて送る。PROBEを受けた側は届いた列（`Train`）の幅から相手の上りの太さを見積もる。
//! 取りまとめ役（role_nonceの大きい方。AGREEを送る）が両者の値から`decide`で決め、決めた値（`Pick`）をAGREEに載せる。相手は同じ式で検証する。
//! 決める順: NATの種類（片方だけが相手から届かない型ならもう片方）→ 上りの太さ（`RATIO_NUM / RATIO_DEN`倍以上の差）→ role_nonce。

use crate::nat::NatMark;
use crate::wire::MAX_DATAGRAM;

/// 1回の接続で送るPROBEの数（間隔を空けずに続けて送る列）。
pub const PROBE_COUNT: u8 = 10;
/// 1つの`Link`の一生で送るPROBEの列の上限。
/// 鍵の替え直しのHELLOを偽の送り元から重ねて送られても、1通112 Bで12 KBの列が送り元へ向かい続けないようにする。
/// 越えたら列を送らず、ホストは測らずに決まる（`REPORT_WAIT_MS`後）。
pub const MAX_PROBE_TRAINS: u32 = 4;
/// PROBE 1通の長さ（B。HMACのタグ込み）。経路MTUを割らない上限いっぱいにして列の幅を太さに比べて大きくする。
pub const PROBE_LEN: usize = MAX_DATAGRAM;
/// 受け手が列を待つ上限（最初の1通が届いてからms）。0.4 Mbpsでも10通は約240 msで届き切る（計算）。
pub const PROBE_WINDOW_MS: u32 = 300;
/// 取りまとめ役が相手のHELLOを受けてからゲストの値（PROBE_RESULT）を待つ上限（ms）。来なければ測れない扱いで決める。
pub const REPORT_WAIT_MS: u32 = 500;
/// ゲストがPROBE_RESULTを送り直す間隔（ms。AGREEが届くまで）。
pub const REPORT_INTERVAL_MS: u32 = 20;
/// 列の幅の下限（µs）。これより短い（全部が同じmsに届いた）列は、この幅で届いたとして割る。
/// 受信のスレッドが遅れて溜まったパケットをまとめて読むと幅が潰れ、そのまま割ると太さがいくらでも大きく出るため。
pub const MIN_WIDTH_US: u64 = 1_000;
/// 見積もりの上限（B/s）。10通の列（番号の差9）が`MIN_WIDTH_US`で届いた太さ（10.8 MB/s。約86 Mbps）。
/// これより太い同士は差が出ず、role_nonceで決まる。太いと細いのペアは太い方になる。
pub const UP_CAP: u32 =
    ((PROBE_COUNT as u64 - 1) * PROBE_LEN as u64 * 1_000_000 / MIN_WIDTH_US) as u32;
/// 太い方が細い方の5/4（1.25）倍以上なら太い方をホストにする。
pub const RATIO_NUM: u64 = 5;
pub const RATIO_DEN: u64 = 4;

/// ホストの決め方を使う接続の設定（`Handshake::set_host_pick`）。使わない接続（LAN・nonceを決めた実行）は設定しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    /// 自分のネットワーク種別のフラグ（HELLOに載せる）。
    pub nat: NatMark,
    /// 検査用: 相手の列が2通以上届いたら見積もりの代わりにこの値（B/s）を使う。loopbackでは両者の太さが同じなので片方を太く見せる。
    pub fake_up: Option<u32>,
}

impl Setup {
    pub fn new(nat: NatMark) -> Setup {
        Setup { nat, fake_up: None }
    }
}

/// 相手から届いたPROBEの列。通し番号のいちばん小さい便といちばん大きい便の到着の差（幅）と番号の差から太さを見積もる。
/// 途中の便が落ちても番号の差で割るので細く出ない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Train {
    /// 届いた通し番号（ビットi = 番号i）。重なった便は数えない。
    seen: u16,
    /// 最初に届いた便の時刻（µs。待つ上限の起点）。
    first_us: Option<u64>,
    /// （通し番号, 到着のµs）のいちばん小さい番号といちばん大きい番号。
    lo: Option<(u8, u64)>,
    hi: Option<(u8, u64)>,
}

impl Train {
    /// 1通を受ける。待つ上限を過ぎた便・番号の外・重なった便は使わない（使ったらtrue）。
    pub fn on_probe(&mut self, seq: u8, at_us: u64) -> bool {
        if seq >= PROBE_COUNT || self.seen & (1 << seq) != 0 {
            return false;
        }
        if let Some(f) = self.first_us
            && at_us.saturating_sub(f) > u64::from(PROBE_WINDOW_MS) * 1_000
        {
            return false;
        }
        self.first_us.get_or_insert(at_us);
        self.seen |= 1 << seq;
        if self.lo.is_none_or(|(s, _)| seq < s) {
            self.lo = Some((seq, at_us));
        }
        if self.hi.is_none_or(|(s, _)| seq > s) {
            self.hi = Some((seq, at_us));
        }
        true
    }

    /// 届いた数。
    pub fn count(&self) -> u32 {
        self.seen.count_ones()
    }

    /// 列を締めてよいかどうか（最後の番号が届いた・最初の1通から待つ上限が過ぎた）。1通も届いていなければ締めない。
    pub fn closed(&self, now_us: u64) -> bool {
        let Some(f) = self.first_us else {
            return false;
        };
        // 最後の番号が先に届いてもほかの便が後から届きうるので締めない。
        self.count() == u32::from(PROBE_COUNT)
            || now_us.saturating_sub(f) >= u64::from(PROBE_WINDOW_MS) * 1_000
    }

    /// 相手の上りの見積もり（B/s）。届いたのが1通以下・番号と到着の順が逆なら0（測れない）。
    /// 幅は`MIN_WIDTH_US`を下限にして割るので、全部の番号がそろった列の上限は`UP_CAP`。
    pub fn estimate(&self) -> u32 {
        let (Some((ls, lt)), Some((hs, ht))) = (self.lo, self.hi) else {
            return 0;
        };
        if hs <= ls || ht < lt {
            return 0;
        }
        let width = (ht - lt).max(MIN_WIDTH_US);
        let bytes = u64::from(hs - ls) * PROBE_LEN as u64;
        (bytes * 1_000_000 / width).clamp(1, u64::from(UP_CAP)) as u32
    }

    /// `estimate`に検査用の値を当てたもの（`Setup::fake_up`。2通以上届いたときだけ）。
    pub fn estimate_with(&self, fake: Option<u32>) -> u32 {
        match fake {
            Some(v) if self.count() >= 2 => v,
            _ => self.estimate(),
        }
    }
}

/// どれで決まったか（本ログに出す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// 片方だけが相手から届かない型（シンメトリックNATでUPnPなし）だった。
    Nat,
    /// 上りの太さに`RATIO_NUM / RATIO_DEN`倍以上の差があった。
    Uplink,
    /// どちらでも決まらず、role_nonceの大きい方（取りまとめ役）。
    Nonce,
}

/// 片方の値。`up`は相手が測ったその人の上り（B/s。0 = 測れない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub up: u32,
    pub nat: NatMark,
}

/// 取りまとめ役`lead`と相手`other`の値からホストが取りまとめ役かどうか（と決め手）を返す。両者が同じ値から同じ答えを出す（ゲストはAGREEの検証に使う）。
pub fn decide(lead: Facts, other: Facts) -> (bool, Basis) {
    // 型が分からない側があればNATでは決めない。
    match (lead.nat.reachable(), other.nat.reachable()) {
        (Some(true), Some(false)) => return (true, Basis::Nat),
        (Some(false), Some(true)) => return (false, Basis::Nat),
        _ => {}
    }
    if lead.up > 0 && other.up > 0 {
        let (l, o) = (u64::from(lead.up), u64::from(other.up));
        if o * RATIO_DEN >= l * RATIO_NUM {
            return (false, Basis::Uplink);
        }
        if l * RATIO_DEN >= o * RATIO_NUM {
            return (true, Basis::Uplink);
        }
    }
    (true, Basis::Nonce)
}

/// AGREEに載せる、どちらがホストかと決めるのに使った値（送り手 = 取りまとめ役、受け手 = ゲスト）。ホストの決め方を使わない接続は`NONE`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    /// ホストがAGREEの送り手（取りまとめ役）かどうか。
    pub host_is_sender: bool,
    /// 送り手の上り（受け手が測った値。届かなければ0）。
    pub sender_up: u32,
    /// 受け手の上り（送り手が測った値）。
    pub receiver_up: u32,
    pub sender_nat: NatMark,
    pub receiver_nat: NatMark,
}

impl Pick {
    pub const NONE: Pick = Pick {
        host_is_sender: true,
        sender_up: 0,
        receiver_up: 0,
        sender_nat: NatMark::UNKNOWN,
        receiver_nat: NatMark::UNKNOWN,
    };

    /// 両者の値から決めた`Pick`。
    pub fn decided(sender: Facts, receiver: Facts) -> Pick {
        Pick {
            host_is_sender: decide(sender, receiver).0,
            sender_up: sender.up,
            receiver_up: receiver.up,
            sender_nat: sender.nat,
            receiver_nat: receiver.nat,
        }
    }

    fn facts(&self) -> (Facts, Facts) {
        (
            Facts {
                up: self.sender_up,
                nat: self.sender_nat,
            },
            Facts {
                up: self.receiver_up,
                nat: self.receiver_nat,
            },
        )
    }

    /// 載っている値から`decide`で出る答えと`host_is_sender`が合うかどうか。
    pub fn consistent(&self) -> bool {
        let (s, r) = self.facts();
        decide(s, r).0 == self.host_is_sender
    }

    pub fn basis(&self) -> Basis {
        let (s, r) = self.facts();
        decide(s, r).1
    }

    /// 自分から見た形（`i_sent`は自分がAGREEの送り手かどうか、`active`は両者が決め方を使ったかどうか）。
    pub fn note(&self, i_sent: bool, active: bool) -> PickNote {
        let (own, peer) = if i_sent {
            self.facts()
        } else {
            let (s, r) = self.facts();
            (r, s)
        };
        PickNote {
            active,
            basis: self.basis(),
            own_up: own.up,
            peer_up: peer.up,
            own_nat: own.nat,
            peer_nat: peer.nat,
        }
    }
}

/// 接続したときにどちらがホストに決まったかの材料（`Established::pick`。本ログの1行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickNote {
    /// 両者が決め方を使った（PROBEを交わした）。偽ならホストはrole_nonceの大きい方で、値は使っていない。
    pub active: bool,
    pub basis: Basis,
    /// 自分の上り（相手が測った値。B/s。0 = 測れない）。
    pub own_up: u32,
    /// 相手の上り（自分が測った値）。
    pub peer_up: u32,
    pub own_nat: NatMark,
    pub peer_nat: NatMark,
}

impl PickNote {
    pub const NONE: PickNote = PickNote {
        active: false,
        basis: Basis::Nonce,
        own_up: 0,
        peer_up: 0,
        own_nat: NatMark::UNKNOWN,
        peer_nat: NatMark::UNKNOWN,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nat::Symmetry;

    const MS: u64 = 1_000;

    fn nat(symmetry: Symmetry, upnp: bool) -> NatMark {
        NatMark { symmetry, upnp }
    }

    fn open() -> NatMark {
        nat(Symmetry::No, false)
    }

    fn sym() -> NatMark {
        nat(Symmetry::Yes, false)
    }

    fn f(up: u32, nat: NatMark) -> Facts {
        Facts { up, nat }
    }

    /// （通し番号, 到着のµs）を順に受けた列（到着の順は番号の順と違ってよい）。
    fn train(arrivals: &[(u8, u64)]) -> Train {
        let mut t = Train::default();
        for &(s, at) in arrivals {
            t.on_probe(s, at);
        }
        t
    }

    #[test]
    fn a_full_train_gives_the_bytes_over_the_width() {
        // 1,200 Bを2 msごと: 600,000 B/s。
        let t = train(
            &(0..10)
                .map(|i| (i, 5_000 + u64::from(i) * 2 * MS))
                .collect::<Vec<_>>(),
        );
        assert_eq!(t.count(), 10);
        assert_eq!(t.estimate(), 600_000);
    }

    #[test]
    fn losing_middle_or_end_probes_does_not_make_the_line_look_thinner() {
        let full: Vec<(u8, u64)> = (0..10).map(|i| (i, u64::from(i) * 3 * MS)).collect();
        let want = train(&full).estimate();
        assert_eq!(want, 400_000);
        // 途中の1・4・5・7と最後の9を落とす。
        let lossy: Vec<(u8, u64)> = full
            .iter()
            .copied()
            .filter(|(s, _)| ![1, 4, 5, 7, 9].contains(s))
            .collect();
        assert_eq!(train(&lossy).estimate(), want);
        // 最初の0を落としても同じ。
        let head: Vec<(u8, u64)> = full.iter().copied().filter(|(s, _)| *s != 0).collect();
        assert_eq!(train(&head).estimate(), want);
    }

    #[test]
    fn one_probe_or_a_reversed_order_cannot_be_measured_and_short_widths_divide_by_1_ms() {
        assert_eq!(UP_CAP, 10_800_000);
        assert_eq!(train(&[]).estimate(), 0);
        assert_eq!(train(&[(3, 10 * MS)]).estimate(), 0);
        // 全部が同じms（幅0.9 ms）に届いた10通は1 msで割って上限の値。
        let same: Vec<(u8, u64)> = (0..10).map(|i| (i, 10 * MS + u64::from(i) * 100)).collect();
        assert_eq!(train(&same).estimate(), UP_CAP);
        // 番号の差が小さい列は同じµsに届いても差のぶんだけ（差3なら3,600 B ÷ 1 ms、差1なら1,200 B ÷ 1 ms）。
        assert_eq!(train(&[(2, 10 * MS), (5, 10 * MS)]).estimate(), 3_600_000);
        assert_eq!(
            train(&[(4, 10 * MS), (5, 10 * MS + 10)]).estimate(),
            1_200_000
        );
        // 幅がちょうど1 ms・それより長い列はそのまま割る。
        assert_eq!(train(&[(0, 10 * MS), (9, 11 * MS)]).estimate(), UP_CAP);
        assert_eq!(train(&[(0, 10 * MS), (3, 11 * MS)]).estimate(), 3_600_000);
        assert_eq!(train(&[(0, 10 * MS), (3, 13 * MS)]).estimate(), 1_200_000);
        // 番号と到着の順が逆（入れ替わった）なら測れない。
        assert_eq!(train(&[(9, 10 * MS), (0, 20 * MS)]).estimate(), 0);
        assert_eq!(train(&[(9, 10 * MS), (0, 10 * MS + 500)]).estimate(), 0);
    }

    #[test]
    fn two_capped_lines_keep_the_nonce_and_a_capped_line_beats_a_thin_one() {
        assert_eq!(
            decide(f(UP_CAP, open()), f(UP_CAP, open())),
            (true, Basis::Nonce)
        );
        assert_eq!(
            decide(f(1_000_000, open()), f(UP_CAP, open())),
            (false, Basis::Uplink)
        );
        assert_eq!(
            decide(f(UP_CAP, open()), f(1_000_000, open())),
            (true, Basis::Uplink)
        );
    }

    #[test]
    fn duplicates_late_and_out_of_range_probes_are_not_used() {
        let mut t = train(&[(0, 0), (5, 5 * MS)]);
        assert!(!t.on_probe(5, 50 * MS), "重なった便");
        assert!(!t.on_probe(PROBE_COUNT, 6 * MS), "番号の外");
        assert!(
            !t.on_probe(9, u64::from(PROBE_WINDOW_MS) * MS + 1),
            "待つ上限を過ぎた便"
        );
        assert!(
            t.on_probe(9, u64::from(PROBE_WINDOW_MS) * MS),
            "上限ちょうどは使う"
        );
        assert_eq!(t.count(), 3);
    }

    #[test]
    fn the_train_closes_when_all_ten_arrive_or_after_the_window() {
        let mut t = Train::default();
        assert!(!t.closed(u64::MAX), "1 通も届いていなければ締めない");
        t.on_probe(0, 100 * MS);
        assert!(!t.closed(100 * MS + 299 * MS));
        assert!(t.closed(100 * MS + 300 * MS));
        // 最後の番号が先に届いても10通そろうまで締めない（順の入れ替わり）。そろえばすぐ締める。
        let mut t = Train::default();
        t.on_probe(PROBE_COUNT - 1, 100 * MS);
        for s in 0..PROBE_COUNT - 1 {
            assert!(!t.closed(101 * MS), "{s} 通で締めた");
            t.on_probe(s, 101 * MS);
        }
        assert!(t.closed(101 * MS), "そろったのに締めない");
        // 1通落ちた列は待つ上限まで締めない。
        let mut t = Train::default();
        for s in (0..PROBE_COUNT).filter(|&s| s != 4) {
            t.on_probe(s, 100 * MS);
        }
        assert!(!t.closed(100 * MS + 299 * MS));
        assert!(t.closed(100 * MS + 300 * MS));
    }

    #[test]
    fn the_fake_value_needs_two_probes() {
        assert_eq!(train(&[(0, 0)]).estimate_with(Some(77)), 0);
        assert_eq!(train(&[(0, 0), (1, 0)]).estimate_with(Some(77)), 77);
        assert_eq!(train(&[(0, 0), (9, 9 * MS)]).estimate_with(None), 1_200_000);
    }

    #[test]
    fn nat_comes_before_the_uplink() {
        // 取りまとめ役が太くても届かない型なら相手。
        assert_eq!(
            decide(f(10_000_000, sym()), f(100_000, open())),
            (false, Basis::Nat)
        );
        assert_eq!(
            decide(f(100_000, open()), f(10_000_000, sym())),
            (true, Basis::Nat)
        );
        // UPnPで開けていれば届く。
        assert_eq!(
            decide(f(100_000, nat(Symmetry::Yes, true)), f(10_000_000, sym())),
            (true, Basis::Nat)
        );
        assert_eq!(
            decide(
                f(100_000, nat(Symmetry::Unknown, true)),
                f(10_000_000, sym())
            ),
            (true, Basis::Nat)
        );
    }

    #[test]
    fn both_symmetric_or_an_unknown_kind_falls_to_the_uplink() {
        let unknown = NatMark::UNKNOWN;
        for (a, b) in [
            (sym(), sym()),
            (unknown, sym()),
            (sym(), unknown),
            (unknown, unknown),
        ] {
            assert_eq!(
                decide(f(100_000, a), f(200_000, b)),
                (false, Basis::Uplink),
                "{a:?} {b:?}"
            );
        }
    }

    #[test]
    fn the_uplink_needs_a_quarter_more_and_both_values() {
        // ちょうど1.25倍は差あり、それ未満はrole_nonce。
        assert_eq!(
            decide(f(400_000, open()), f(500_000, open())),
            (false, Basis::Uplink)
        );
        assert_eq!(
            decide(f(400_000, open()), f(499_999, open())),
            (true, Basis::Nonce)
        );
        assert_eq!(
            decide(f(500_000, open()), f(400_000, open())),
            (true, Basis::Uplink)
        );
        // 片方でも測れなければrole_nonce。
        assert_eq!(
            decide(f(0, open()), f(9_000_000, open())),
            (true, Basis::Nonce)
        );
        assert_eq!(
            decide(f(9_000_000, open()), f(0, open())),
            (true, Basis::Nonce)
        );
        // 非常に太い値でも溢れない。
        assert_eq!(
            decide(f(u32::MAX, open()), f(1, open())),
            (true, Basis::Uplink)
        );
        assert_eq!(
            decide(f(1, open()), f(u32::MAX, open())),
            (false, Basis::Uplink)
        );
    }

    #[test]
    fn a_pick_is_consistent_only_with_its_own_verdict() {
        let p = Pick::decided(f(100_000, open()), f(900_000, open()));
        assert!(!p.host_is_sender && p.consistent());
        assert_eq!(p.basis(), Basis::Uplink);
        let flipped = Pick {
            host_is_sender: true,
            ..p
        };
        assert!(!flipped.consistent());
        assert!(Pick::NONE.consistent() && Pick::NONE.basis() == Basis::Nonce);
    }
}
