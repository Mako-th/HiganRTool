//! VPN の仮想 LAN（Radmin VPN・Hamachi・ZeroTier）のネットワークの範囲（pure）。
//!
//! これらのアダプターは私設でないアドレス（Radmin は 26/8、Hamachi は 25/8、ZeroTier は選んだ範囲）を持つことがあるので、
//! 範囲 lan（`guard`）には入らない。アダプターの名前でツールを見分け、そのアダプターのネットワーク 1 つだけを範囲 lan に足す。
//! 名前で決め打ちにするのは、会社の VPN のようにインターネットから届くアドレスを持つ仮想のアダプターで待たないため。

use std::net::Ipv4Addr;

/// 使うネットワークの長さ（OnLinkPrefixLength）の下限と上限。これより広い（/8 未満）・狭い（/31 以上）ものは使わない。
pub const PREFIX_MIN: u8 = 8;
pub const PREFIX_MAX: u8 = 30;

/// IPv4 のネットワーク（先頭のアドレスと長さ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Net4 {
    base: u32,
    prefix: u8,
}

/// `prefix` の上位ビットが立ったマスク。`prefix` は `PREFIX_MIN..=PREFIX_MAX`（`Net4::new` が保つ）。
fn mask(prefix: u8) -> u32 {
    u32::MAX << (32 - u32::from(prefix))
}

impl Net4 {
    /// `ip` が属するネットワーク。長さが `PREFIX_MIN..=PREFIX_MAX` の外なら `None`。
    pub fn new(ip: Ipv4Addr, prefix: u8) -> Option<Net4> {
        if !(PREFIX_MIN..=PREFIX_MAX).contains(&prefix) {
            return None;
        }
        Some(Net4 {
            base: u32::from(ip) & mask(prefix),
            prefix,
        })
    }

    pub fn prefix(self) -> u8 {
        self.prefix
    }

    /// ネットワークの中の 1 台のアドレスか。ネットワークのアドレス（ホスト部が全部 0）とブロードキャスト（全部 1）は含めない。
    pub fn contains_host(self, ip: Ipv4Addr) -> bool {
        let v = u32::from(ip);
        let m = mask(self.prefix);
        let host = v & !m;
        v & m == self.base && host != 0 && host != !m
    }

    /// 原子変数に載せる形（bit 40 = 有効・bit 32..40 = 長さ・下位 32 bit = 先頭のアドレス）。
    pub fn pack(self) -> u64 {
        1 << 40 | u64::from(self.prefix) << 32 | u64::from(self.base)
    }

    /// `pack` の逆。有効の印が無い・長さが範囲の外なら `None`。
    pub fn unpack(w: u64) -> Option<Net4> {
        if w >> 40 & 1 == 0 {
            return None;
        }
        Net4::new(Ipv4Addr::from(w as u32), (w >> 32) as u8)
    }
}

/// 名前で見分けるツール。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VpnTool {
    Radmin,
    Hamachi,
    ZeroTier,
}

impl VpnTool {
    /// アダプターの説明か接続の名前（小文字にして）に `radmin`・`hamachi`・`zerotier` が入るもの。どちらも画面にもログにも出さない。
    pub fn of_names(description: &str, name: &str) -> Option<VpnTool> {
        let has = |w: &str| {
            description.to_ascii_lowercase().contains(w) || name.to_ascii_lowercase().contains(w)
        };
        if has("radmin") {
            Some(VpnTool::Radmin)
        } else if has("hamachi") {
            Some(VpnTool::Hamachi)
        } else if has("zerotier") {
            Some(VpnTool::ZeroTier)
        } else {
            None
        }
    }

    /// 本ログに書くツールの名前（製品の綴り）。
    pub fn name(self) -> &'static str {
        match self {
            VpnTool::Radmin => "Radmin VPN",
            VpnTool::Hamachi => "Hamachi",
            VpnTool::ZeroTier => "ZeroTier",
        }
    }

    /// 札に詰める番号（0 は無し）。
    pub fn code(self) -> u8 {
        match self {
            VpnTool::Radmin => 1,
            VpnTool::Hamachi => 2,
            VpnTool::ZeroTier => 3,
        }
    }

    pub fn from_code(c: u8) -> Option<VpnTool> {
        match c {
            1 => Some(VpnTool::Radmin),
            2 => Some(VpnTool::Hamachi),
            3 => Some(VpnTool::ZeroTier),
            _ => None,
        }
    }
}

/// `VpnSet` に入れる数の上限（3 つのツールのうち ZeroTier は複数のネットワークを同時に持てる）。
pub const VPN_SET_MAX: usize = 8;

/// 自分の PC の VPN のネットワークの集まり（固定長。アドレスの入力の判定に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VpnSet([Option<Net4>; VPN_SET_MAX]);

impl VpnSet {
    pub const NONE: VpnSet = VpnSet([None; VPN_SET_MAX]);

    /// 重なりを除き、`VPN_SET_MAX` 個まで入れる。
    pub fn from_nets(nets: impl IntoIterator<Item = Net4>) -> VpnSet {
        let mut s = VpnSet::NONE;
        let mut n = 0;
        for net in nets {
            if n < VPN_SET_MAX && !s.0.contains(&Some(net)) {
                s.0[n] = Some(net);
                n += 1;
            }
        }
        s
    }

    /// `ip` を含むネットワーク。
    pub fn find(&self, ip: Ipv4Addr) -> Option<Net4> {
        self.0
            .iter()
            .flatten()
            .find(|n| n.contains_host(ip))
            .copied()
    }

    pub fn to_words(&self) -> [u64; VPN_SET_MAX] {
        self.0.map(|n| n.map_or(0, Net4::pack))
    }

    pub fn from_words(w: &[u64; VPN_SET_MAX]) -> VpnSet {
        VpnSet(w.map(Net4::unpack))
    }

    pub fn len(&self) -> usize {
        self.0.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    /// 壊れたら落ちる: 長さの範囲（8〜30）の端の取り違え、先頭のアドレスに丸めない、ホスト部が全部 0 / 全部 1 のアドレスを 1 台と数える、
    /// ネットワークの外の隣を中と数える。
    #[test]
    fn a_net_holds_the_hosts_between_its_network_and_broadcast_addresses() {
        for p in [0, 1, 7, 31, 32, 255] {
            assert_eq!(Net4::new(ip("26.1.2.3"), p), None, "/{p}");
        }
        let radmin = Net4::new(ip("26.12.34.56"), 8).unwrap();
        assert_eq!(radmin.prefix(), 8);
        assert_eq!(radmin, Net4::new(ip("26.0.0.9"), 8).unwrap(), "丸め");
        for (a, want) in [
            ("26.0.0.1", true),
            ("26.255.255.254", true),
            ("26.12.34.56", true),
            ("26.0.0.0", false),
            ("26.255.255.255", false),
            ("25.255.255.254", false),
            ("27.0.0.1", false),
            ("192.168.0.1", false),
        ] {
            assert_eq!(radmin.contains_host(ip(a)), want, "{a}");
        }
        let small = Net4::new(ip("10.147.17.37"), 30).unwrap();
        for (a, want) in [
            ("10.147.17.36", false),
            ("10.147.17.37", true),
            ("10.147.17.38", true),
            ("10.147.17.39", false),
            ("10.147.17.40", false),
            ("10.147.17.35", false),
        ] {
            assert_eq!(small.contains_host(ip(a)), want, "{a}");
        }
    }

    #[test]
    fn a_net_survives_the_atomic_word_and_garbage_words_are_refused() {
        for (a, p) in [
            ("26.1.2.3", 8),
            ("25.9.9.9", 8),
            ("10.1.2.3", 30),
            ("172.20.4.5", 16),
        ] {
            let n = Net4::new(ip(a), p).unwrap();
            assert_eq!(Net4::unpack(n.pack()), Some(n), "{a}/{p}");
        }
        assert_eq!(Net4::unpack(0), None, "有効の印が無い");
        assert_eq!(Net4::unpack(1 << 40 | 31 << 32 | 0x1A00_0000), None);
        assert_eq!(Net4::unpack(1 << 40 | 7 << 32 | 0x1A00_0000), None);
    }

    /// 壊れたら落ちる: 説明か接続の名前のどちらかだけに名前があるときに見逃す、大文字を見逃す、名前の無いアダプターをツールと決める。
    #[test]
    fn a_tool_is_known_by_the_description_or_the_connection_name() {
        for (d, n, want) in [
            (
                "Famatech Radmin VPN Ethernet Adapter",
                "Radmin VPN",
                Some(VpnTool::Radmin),
            ),
            (
                "Famatech Radmin VPN Ethernet Adapter",
                "",
                Some(VpnTool::Radmin),
            ),
            ("", "イーサネット 3 RADMIN", Some(VpnTool::Radmin)),
            (
                "LogMeIn Hamachi Virtual Ethernet Adapter",
                "Hamachi",
                Some(VpnTool::Hamachi),
            ),
            ("", "hamachi", Some(VpnTool::Hamachi)),
            ("ZeroTier Virtual Port", "", Some(VpnTool::ZeroTier)),
            (
                "Intel(R) Ethernet",
                "ZeroTier One [8056c2e21c000001]",
                Some(VpnTool::ZeroTier),
            ),
            ("TAP-Windows Adapter V9", "OpenVPN", None),
            ("WireGuard Tunnel", "Tailscale", None),
            ("Realtek PCIe GbE Family Controller", "イーサネット", None),
            ("", "", None),
        ] {
            assert_eq!(VpnTool::of_names(d, n), want, "{d:?} / {n:?}");
        }
        for t in [VpnTool::Radmin, VpnTool::Hamachi, VpnTool::ZeroTier] {
            assert_eq!(VpnTool::from_code(t.code()), Some(t));
        }
        assert_eq!(VpnTool::from_code(0), None);
        assert_eq!(VpnTool::from_code(4), None);
    }

    /// 壊れたら落ちる: ツールの名前が空・別の綴り・アドレスの形（数字と `.`）になる（本ログの VPN の行に書く名前）。
    #[test]
    fn the_tool_names_are_the_product_names_without_an_address() {
        assert_eq!(VpnTool::Radmin.name(), "Radmin VPN");
        assert_eq!(VpnTool::Hamachi.name(), "Hamachi");
        assert_eq!(VpnTool::ZeroTier.name(), "ZeroTier");
        for t in [VpnTool::Radmin, VpnTool::Hamachi, VpnTool::ZeroTier] {
            assert!(!t.name().chars().any(|c| c.is_ascii_digit() || c == '.'));
        }
    }

    /// 壊れたら落ちる: 重なりを数えて枠を食う、上限を越えて入れる、集まりの外のアドレスや網のアドレスを見つける、語の往復で落ちる。
    #[test]
    fn a_set_dedupes_caps_finds_and_survives_words() {
        let a = Net4::new(ip("26.0.0.1"), 8).unwrap();
        let b = Net4::new(ip("25.0.0.1"), 8).unwrap();
        let s = VpnSet::from_nets([a, b, a]);
        assert_eq!(s.len(), 2);
        assert_eq!(s.find(ip("26.9.9.9")), Some(a));
        assert_eq!(s.find(ip("25.9.9.9")), Some(b));
        assert_eq!(s.find(ip("26.0.0.0")), None);
        assert_eq!(s.find(ip("24.9.9.9")), None);
        assert_eq!(VpnSet::from_words(&s.to_words()), s);
        assert!(VpnSet::NONE.is_empty() && VpnSet::default().is_empty());
        assert_eq!(VpnSet::NONE.find(ip("26.9.9.9")), None);
        let many = (0..20).map(|i| Net4::new(Ipv4Addr::new(30 + i, 0, 0, 1), 8).unwrap());
        let full = VpnSet::from_nets(many);
        assert_eq!(full.len(), VPN_SET_MAX);
        assert_eq!(full.find(ip("30.1.1.1")).map(Net4::prefix), Some(8));
        assert_eq!(full.find(ip("38.1.1.1")), None, "上限の次は入らない");
    }
}
