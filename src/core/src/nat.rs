//! 回線の型の印（接続コードに 1 B で載せる）と、つながらなかったときの文の選び分け（pure）。
//!
//! 印は 2 つ: 宛先ごとに外側のポートが変わる型（シンメトリック NAT）か（2 か所目の STUN の比べ）と、UPnP で待ち受けの
//! ポートを割り当てられたか。相手の印は相手のコードから読み、自分の印は開くときに決めて自分のコードに載せる。

/// 宛先ごとに外側のポートが変わる型か。比べられなかった（STUN が無答など）ときは `Unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Symmetry {
    #[default]
    Unknown,
    No,
    Yes,
}

impl Symmetry {
    /// 2 か所目の STUN の比べ（`stun::Mapping::depends_on_destination`。比べられていなければ `None`）から。
    pub fn from_depends(v: Option<bool>) -> Symmetry {
        match v {
            None => Symmetry::Unknown,
            Some(false) => Symmetry::No,
            Some(true) => Symmetry::Yes,
        }
    }

    fn bits(self) -> u8 {
        match self {
            Symmetry::Unknown => 0,
            Symmetry::No => 1,
            Symmetry::Yes => 2,
        }
    }

    fn from_bits(b: u8) -> Option<Symmetry> {
        match b {
            0 => Some(Symmetry::Unknown),
            1 => Some(Symmetry::No),
            2 => Some(Symmetry::Yes),
            _ => None,
        }
    }
}

/// 接続コードに載せる回線の型の印。コード中の 1 B: 下位 2 bit = `Symmetry`（3 は未定義）、bit 2 = UPnP で割り当てられた。
/// 残りの bit は 0 でなければ断る（同じ中身に 2 通りの文字を許さない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NatMark {
    pub symmetry: Symmetry,
    /// UPnP で待ち受けのポートを割り当てられた（自分の候補に割り当てた外側のポートが載っている）。
    pub upnp: bool,
}

const UPNP_BIT: u8 = 0b100;
const SYMMETRY_MASK: u8 = 0b011;

impl NatMark {
    /// 型が分からず、UPnP も使っていない。
    pub const UNKNOWN: NatMark = NatMark {
        symmetry: Symmetry::Unknown,
        upnp: false,
    };

    pub fn to_byte(self) -> u8 {
        self.symmetry.bits() | if self.upnp { UPNP_BIT } else { 0 }
    }

    /// 定義の外の bit が立っていれば `None`。
    pub fn from_byte(b: u8) -> Option<NatMark> {
        if b & !(SYMMETRY_MASK | UPNP_BIT) != 0 {
            return None;
        }
        Some(NatMark {
            symmetry: Symmetry::from_bits(b & SYMMETRY_MASK)?,
            upnp: b & UPNP_BIT != 0,
        })
    }

    /// ログに出す句（アドレス・ポートの値は含まない）。
    pub fn text(self) -> String {
        format!(
            "宛先ごとに外側のポートが変わる型: {} / UPnP で割り当て: {}",
            match self.symmetry {
                Symmetry::Unknown => "不明",
                Symmetry::No => "いいえ",
                Symmetry::Yes => "はい",
            },
            if self.upnp { "あり" } else { "なし" }
        )
    }

    /// 直接つなぐ経路が塞がっている型か。シンメトリックでも、UPnP でポートを開けていれば相手の HELLO はそのポートに届くので塞がっていない。
    fn blocked(self) -> bool {
        self.symmetry == Symmetry::Yes && !self.upnp
    }
}

/// つながらなかったときに出す文の種類（番号は README「接続できないとき」の項と同じ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 1: 型が分からない（下の 3 つに当たらない）。
    Unknown = 1,
    /// 2: 相手がシンメトリック NAT で、こちらは UPnP を使えていない。
    PeerSymmetricNoUpnp = 2,
    /// 3: こちらがシンメトリック NAT（相手は違う・不明・UPnP で開けている）。
    OwnSymmetric = 3,
    /// 4: 両方がシンメトリック NAT。
    BothSymmetric = 4,
}

/// 文を選ぶ。`own` は自分の今の印（シンメトリックかは 2 か所目の STUN の今の結果、`upnp` は割り当てられているか）、
/// `peer` は相手のコードの印（まだ読んでいなければ `None`）。
///
/// 自分か相手が UPnP でポートを開けているなら、そちら側はシンメトリックでも塞がっているとは見なさない
/// （開けたポートへは相手から届くので、それでもつながらないなら原因は型でなく不明）。
pub fn choose(own: NatMark, peer: Option<NatMark>) -> FailKind {
    let peer = peer.unwrap_or(NatMark::UNKNOWN);
    match (own.blocked(), peer.blocked()) {
        (true, true) => FailKind::BothSymmetric,
        (true, false) => FailKind::OwnSymmetric,
        (false, true) if !own.upnp => FailKind::PeerSymmetricNoUpnp,
        _ => FailKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SYM: [Symmetry; 3] = [Symmetry::Unknown, Symmetry::No, Symmetry::Yes];

    fn mark(symmetry: Symmetry, upnp: bool) -> NatMark {
        NatMark { symmetry, upnp }
    }

    #[test]
    fn the_byte_round_trips_and_undefined_bits_are_refused() {
        let mut seen = std::collections::HashSet::new();
        for s in ALL_SYM {
            for upnp in [false, true] {
                let m = mark(s, upnp);
                assert_eq!(NatMark::from_byte(m.to_byte()), Some(m), "{m:?}");
                assert!(seen.insert(m.to_byte()), "{m:?} のバイトが重なった");
            }
        }
        assert_eq!(NatMark::UNKNOWN.to_byte(), 0);
        assert_eq!(mark(Symmetry::Yes, true).to_byte(), 0b110);
        // 型の値 3 と、定義の外の bit は断る。256 通りのうち読めるのは 6 通りだけ。
        let readable = (0..=255u8)
            .filter(|&b| NatMark::from_byte(b).is_some())
            .count();
        assert_eq!(readable, 6);
        for b in [0b011, 0b111, 0b1000, 0x80, 0xFF] {
            assert_eq!(NatMark::from_byte(b), None, "{b:#b}");
        }
    }

    #[test]
    fn the_log_text_names_both_marks() {
        let t = NatMark {
            symmetry: Symmetry::Yes,
            upnp: true,
        }
        .text();
        assert!(t.contains("はい") && t.contains("あり"), "{t}");
        let t = NatMark::UNKNOWN.text();
        assert!(t.contains("不明") && t.contains("なし"), "{t}");
    }

    #[test]
    fn symmetry_follows_the_second_stun_compare() {
        assert_eq!(Symmetry::from_depends(None), Symmetry::Unknown);
        assert_eq!(Symmetry::from_depends(Some(false)), Symmetry::No);
        assert_eq!(Symmetry::from_depends(Some(true)), Symmetry::Yes);
    }

    /// 自分 × 相手 × UPnP の可否の主な組を手で書いた答えで見て、全部の組み合わせ（3 × 2 × 3 × 2 = 36 通り）と相手を読んでいない 6 通りも
    /// 別の書き方の規則と突き合わせる。
    #[test]
    fn the_failure_kind_table() {
        use FailKind::{BothSymmetric, OwnSymmetric, PeerSymmetricNoUpnp};
        use Symmetry::{No, Yes};
        const U: Symmetry = Symmetry::Unknown;
        const UNK: FailKind = FailKind::Unknown;
        // (自分の型, 自分の UPnP, 相手の型, 相手の UPnP, 答え)
        let table = [
            // 両方が塞がっている型 → 4（UPnP を使えていない同士）
            (Yes, false, Yes, false, BothSymmetric),
            // 相手だけシンメトリック: こちらが UPnP を使えていなければ 2
            (No, false, Yes, false, PeerSymmetricNoUpnp),
            (U, false, Yes, false, PeerSymmetricNoUpnp),
            // こちらが UPnP を使えていれば、型が原因とは言えない → 1
            (No, true, Yes, false, UNK),
            (U, true, Yes, false, UNK),
            // 相手が UPnP でポートを開けていれば、相手は塞がっていない
            (No, false, Yes, true, UNK),
            (Yes, false, Yes, true, OwnSymmetric),
            // こちらだけシンメトリック → 3
            (Yes, false, No, false, OwnSymmetric),
            (Yes, false, U, false, OwnSymmetric),
            (Yes, false, No, true, OwnSymmetric),
            // こちらが UPnP でポートを開けていれば、こちらは塞がっていない
            (Yes, true, No, false, UNK),
            (Yes, true, Yes, false, UNK),
            (Yes, true, Yes, true, UNK),
            // どちらもシンメトリックでない → 1
            (No, false, No, false, UNK),
            (U, false, U, false, UNK),
            (No, true, No, true, UNK),
        ];
        for (os, ou, ps, pu, want) in table {
            assert_eq!(
                choose(mark(os, ou), Some(mark(ps, pu))),
                want,
                "自分 {os:?} upnp={ou} / 相手 {ps:?} upnp={pu}"
            );
        }
        // 全部の組み合わせを、表の規則と別の書き方（塞がり方の真偽）で突き合わせる。
        for os in ALL_SYM {
            for ou in [false, true] {
                for ps in ALL_SYM {
                    for pu in [false, true] {
                        let own_blocked = os == Yes && !ou;
                        let peer_blocked = ps == Yes && !pu;
                        let want = if own_blocked && peer_blocked {
                            BothSymmetric
                        } else if own_blocked {
                            OwnSymmetric
                        } else if peer_blocked && !ou {
                            PeerSymmetricNoUpnp
                        } else {
                            UNK
                        };
                        assert_eq!(choose(mark(os, ou), Some(mark(ps, pu))), want);
                    }
                }
            }
        }
        // 相手のコードをまだ読んでいない: 相手は不明として扱う。
        for os in ALL_SYM {
            for ou in [false, true] {
                let want = if os == Yes && !ou { OwnSymmetric } else { UNK };
                assert_eq!(choose(mark(os, ou), None), want, "{os:?} {ou}");
            }
        }
    }

    #[test]
    fn the_kind_numbers_match_the_readme_items() {
        assert_eq!(FailKind::Unknown as u8, 1);
        assert_eq!(FailKind::PeerSymmetricNoUpnp as u8, 2);
        assert_eq!(FailKind::OwnSymmetric as u8, 3);
        assert_eq!(FailKind::BothSymmetric as u8, 4);
    }
}
