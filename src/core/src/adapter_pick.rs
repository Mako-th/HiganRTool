//! ネットワークアダプターの一覧からLANで使う自分のIPv4アドレスを選ぶ（pure）。
//!
//! `bind = auto:ポート`は外へ出る経路の送信元を取得する（`net_udp::probe_route_ip`）。
//! デフォルト経路の無いPC（ルーターなしの直結・169.254だけ）ではそれを取得できない。そのときだけ一覧から選ぶ。
//! LANのホストが「自分のアドレスを全部見せる」ときも同じ一覧を使う。
//! 一覧を取得するのはdllのWin32の層（`adapters_win`）。ここは決まった入力から選ぶ判断だけを持つ。

use std::net::{IpAddr, Ipv4Addr};

use crate::guard::{self, Scope};
use crate::vpn_net::{Net4, VpnSet, VpnTool};

/// `IP_ADAPTER_ADDRESSES`の`IfType`のうち物理のアダプター（有線・無線）として扱うもの。
/// 6 = Ethernet、62 = Fast Ethernet、69 = Fibre Ethernet、71 = IEEE 802.11（無線）、117 = Gigabit Ethernet（IANA ifType）。
/// それ以外（24 = loopback、23 = PPP、131 = トンネル、53 = 仮想など）は仮想として外す。
const PHYSICAL_IF_TYPES: [u32; 5] = [6, 62, 69, 71, 117];

/// 仮想のアダプターの説明に含まれる語（小文字で比べる）。
/// 有線に見える仮想のアダプター（Hyper-VのvEthernet・VMware・VirtualBox・VPNのTAPなど）は`IfType`がEthernetなので説明でも見分ける。
/// 一覧は推定（OSの説明の文字列は製品ごとに違うため漏れうる）。
const VIRTUAL_WORDS: [&str; 18] = [
    "virtual",
    "vmware",
    "vbox",
    "hyper-v",
    "vethernet",
    "tap-",
    "tap ",
    "tun ",
    "wireguard",
    "vpn",
    "loopback",
    "pseudo",
    "npcap",
    "wan miniport",
    "tailscale",
    "zerotier",
    "docker",
    "bluetooth",
];

/// Hyper-Vのホストの仮想NICの説明に含まれる語（`VIRTUAL_WORDS`の一部。ゲートウェイがあれば仮想と見なさない）。
const HYPERV_WORDS: [&str; 2] = ["hyper-v", "vethernet"];

/// 仮想マシンの製品が割り当てるMACアドレスの上位3バイト（OUI）。
/// VMware（00:50:56・00:0C:29・00:05:69）・VirtualBox（08:00:27）・Hyper-V（00:15:5D）・Parallels（00:1C:42）・Xen（00:16:3E）。
const VIRTUAL_OUIS: [[u8; 3]; 7] = [
    [0x00, 0x50, 0x56],
    [0x00, 0x0C, 0x29],
    [0x00, 0x05, 0x69],
    [0x08, 0x00, 0x27],
    [0x00, 0x15, 0x5D],
    [0x00, 0x1C, 0x42],
    [0x00, 0x16, 0x3E],
];

/// アダプターのIPv4アドレス1つぶんの材料（アドレスごとに1つ。同じアダプターのアドレスが2つなら2つ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// 説明（`Description`）。仮想かどうかの見分けにだけ使う。画面にもログにも出さない。
    pub description: String,
    /// 接続の名前（`FriendlyName`）。VPNのツールの見分けにだけ使う。画面にもログにも出さない。
    pub name: String,
    /// アドレスのネットワークの長さ（`OnLinkPrefixLength`。分からなければ0）。
    pub prefix: u8,
    /// `IfType`。
    pub if_type: u32,
    /// MACアドレスの上位3バイト（無ければ0）。
    pub oui: [u8; 3],
    /// 動作状態がUpかどうか。
    pub up: bool,
    /// アドレスの重複検出が済んで使える状態（Preferred）かどうか。
    /// 重複検出の途中（Tentative。直結で169.254を自分に割り当てている間）・重複・非推奨のアドレスは偽。
    pub preferred: bool,
    pub ip: Ipv4Addr,
    /// ゲートウェイを持つかどうか。
    pub gateway: bool,
    /// インターフェースのメトリック（小さいほど優先）。
    pub metric: u32,
}

impl Adapter {
    /// 仮想のアダプター（トンネル・VPN・仮想マシン・loopback）かどうか。
    pub fn is_virtual(&self) -> bool {
        if !PHYSICAL_IF_TYPES.contains(&self.if_type) || self.vpn_tool().is_some() {
            return true;
        }
        let d = self.description.to_ascii_lowercase();
        // Hyper-Vの外部スイッチを作ったPCでは物理のNICのアドレスがvEthernet（説明はHyper-Vの仮想のEthernet）へ移る。
        // ゲートウェイを持つものは本物のLANとみなす。
        // 既定のスイッチ・内部のスイッチ・WSLのvEthernetはゲートウェイを持たない（推定。実物では検証していない）。
        if self.gateway && HYPERV_WORDS.iter().any(|w| d.contains(w)) {
            return false;
        }
        if VIRTUAL_OUIS.contains(&self.oui) {
            return true;
        }
        VIRTUAL_WORDS.iter().any(|w| d.contains(w))
    }

    /// LANで使える候補かどうか: Up・アドレスが使える状態・仮想でない・LANの範囲（私設かリンクローカル。`guard::is_lan_v4`）。
    pub fn is_lan_candidate(&self) -> bool {
        self.usable_lan() && !self.is_virtual()
    }

    /// 名前で見分けたVPNのツール（Radmin VPN・Hamachi・ZeroTier）。
    pub fn vpn_tool(&self) -> Option<VpnTool> {
        VpnTool::of_names(&self.description, &self.name)
    }

    /// VPNのツールのアダプターにある私設でない（インターネットの範囲の）アドレスのネットワーク。長さが`vpn_net::PREFIX_MIN..=PREFIX_MAX`の外なら無し。
    /// 私設のアドレスは範囲lanに入るので追加する必要が無い。そのため無しにする。
    fn vpn_net(&self) -> Option<Net4> {
        self.vpn_tool()?;
        if guard::reach(IpAddr::V4(self.ip)) != Some(Scope::Internet) {
            return None;
        }
        Net4::new(self.ip, self.prefix)
    }

    /// 仮想かどうかを問わずLANの範囲で使えるアドレスかどうか。
    fn usable_lan(&self) -> bool {
        self.up && self.preferred && guard::is_lan_v4(self.ip)
    }

    /// LANのホストが受付用受信ソケットを開けるアドレスかどうか: 使えるLANのアドレス、またはVPNのツールの私設でないアドレス（ネットワークが取れるもの）。
    fn listable(&self) -> bool {
        self.usable_lan() || (self.up && self.preferred && self.vpn_net().is_some())
    }

    /// ホストの一覧の並び: 0 = 本物のLAN・1 = VPNのツール・2 = ほかの仮想。
    fn class(&self) -> u8 {
        if self.vpn_tool().is_some() {
            1
        } else if self.is_virtual() {
            2
        } else {
            0
        }
    }

    /// 優先の高さ（大きいほど優先）: ゲートウェイがあるものが先。次にメトリックが小さいもの。
    fn rank(&self) -> (bool, std::cmp::Reverse<u32>) {
        (self.gateway, std::cmp::Reverse(self.metric))
    }
}

/// LANのホストが受付用受信ソケットを開く自分のアドレス1つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostAddr {
    pub ip: Ipv4Addr,
    /// VPNのツールのアダプターのアドレスならそのツール（ホストの待機の画面でアドレスに名前を添える）。
    pub tool: Option<VpnTool>,
    /// 私設でないVPNのアドレスならそのネットワーク。
    /// 受付用受信ソケットは同じネットワークから来た申し込みだけを読む。ゲームのネットワークはこれを足した範囲で開く。
    /// 私設のアドレスは`None`。今までどおり範囲lanの送信元を読む。
    pub net: Option<Net4>,
}

/// 自分のPCの私設でないVPNのアドレス1つ（申し込む側が利用者の打ったアドレスに申し込んでよいかを決める材料）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VpnLink {
    pub ip: Ipv4Addr,
    pub tool: VpnTool,
    pub net: Net4,
}

/// Upで使える状態のVPNのツールのアダプターにある私設でないアドレス（同じアドレスは1つ）。
pub fn vpn_links(all: &[Adapter]) -> Vec<VpnLink> {
    let mut out: Vec<VpnLink> = Vec::new();
    for a in all.iter().filter(|a| a.up && a.preferred) {
        if let (Some(tool), Some(net)) = (a.vpn_tool(), a.vpn_net())
            && !out.iter().any(|l| l.ip == a.ip)
        {
            out.push(VpnLink {
                ip: a.ip,
                tool,
                net,
            });
        }
    }
    out
}

/// 選べなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// LANで使えるアドレスが1つも無い。
    NoCandidate,
    /// いちばん優先のアドレスが複数あってどれか決められない（直結の相手が違う・メトリックが同じ有線と無線など）。
    Tie(Vec<Ipv4Addr>),
}

impl Refusal {
    /// ログと画面に出す理由（次にすること「アドレスを手で書く」は呼び手が足す）。
    pub fn text(&self) -> String {
        match self {
            Refusal::NoCandidate => {
                "アダプターの一覧に LAN で使える IPv4 アドレスがありません".to_string()
            }
            Refusal::Tie(v) => {
                let list: Vec<String> = v.iter().map(Ipv4Addr::to_string).collect();
                format!(
                    "アダプターの一覧に同じ優先度のアドレスが複数あり、決められません（{}）",
                    list.join("・")
                )
            }
        }
    }
}

/// `vpn_links`のネットワークをアドレスの入力の判定に渡す形にする。
pub fn vpn_set(links: &[VpnLink]) -> VpnSet {
    VpnSet::from_nets(links.iter().map(|l| l.net))
}

/// LANのホストが受付用受信ソケットを開く自分のアドレスを並べて返す（同じアドレスは1つ）。
/// LANのホストが自分のアドレスを全部見せるときに使う。
/// 並びは本物のLAN → VPNのツール（Radmin VPN・Hamachi・ZeroTier）→ ほかの仮想の順で、それぞれ優先の高い順。
/// ほかの仮想は仮想マシンの中のNIC・ホストのVMnetなど（同じPCの仮想マシンとLANで対戦できるように含める）。
/// VPNのツールの私設でないアドレスはネットワークの長さが取れるものだけ追加する。
pub fn host_addresses(all: &[Adapter]) -> Vec<HostAddr> {
    let mut c: Vec<&Adapter> = all.iter().filter(|a| a.listable()).collect();
    c.sort_by_key(|a| (a.class(), std::cmp::Reverse(a.rank())));
    let mut out: Vec<HostAddr> = Vec::new();
    for a in c {
        if !out.iter().any(|h| h.ip == a.ip) {
            out.push(HostAddr {
                ip: a.ip,
                tool: a.vpn_tool(),
                net: a.vpn_net(),
            });
        }
    }
    out
}

/// `bind = auto`が経路を取得できないときのLANで使う自分のアドレスの選び方。
/// ゲートウェイを持つものを先にして次にメトリックの小さいものを選ぶ。
/// いちばん優先のものが複数あれば選ばない（間違ったアドレスで待ち受けるより利用者に手で書いてもらう）。
pub fn choose(all: &[Adapter]) -> Result<Ipv4Addr, Refusal> {
    let mut c: Vec<&Adapter> = all.iter().filter(|a| a.is_lan_candidate()).collect();
    let Some(best) = c.iter().map(|a| a.rank()).max() else {
        return Err(Refusal::NoCandidate);
    };
    c.retain(|a| a.rank() == best);
    let mut ips: Vec<Ipv4Addr> = Vec::new();
    for a in c {
        if !ips.contains(&a.ip) {
            ips.push(a.ip);
        }
    }
    match ips.as_slice() {
        [one] => Ok(*one),
        _ => Err(Refusal::Tie(ips)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETHERNET: u32 = 6;
    const WIFI: u32 = 71;
    const TUNNEL: u32 = 131;
    const LOOPBACK: u32 = 24;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    /// 物理の有線で、Up・使える状態・ゲートウェイあり・メトリック25の見本。
    /// 一覧の並び（ホストの画面に出る順）のアドレスだけ。
    fn lan_addresses(all: &[Adapter]) -> Vec<Ipv4Addr> {
        host_addresses(all).iter().map(|h| h.ip).collect()
    }

    fn wired(addr: &str) -> Adapter {
        Adapter {
            description: "Realtek PCIe GbE Family Controller".to_string(),
            name: "イーサネット".to_string(),
            prefix: 24,
            if_type: ETHERNET,
            oui: [0x3C, 0x52, 0x82],
            up: true,
            preferred: true,
            ip: ip(addr),
            gateway: true,
            metric: 25,
        }
    }

    fn wifi(addr: &str) -> Adapter {
        Adapter {
            description: "Intel(R) Wi-Fi 6 AX201 160MHz".to_string(),
            if_type: WIFI,
            oui: [0x7C, 0x2A, 0x31],
            metric: 50,
            ..wired(addr)
        }
    }

    /// 直結（ルーターなし）: ゲートウェイは無い。169.254を自分に割り当てている。
    fn direct(addr: &str) -> Adapter {
        Adapter {
            gateway: false,
            metric: 25,
            ..wired(addr)
        }
    }

    #[test]
    fn a_direct_link_alone_is_picked_although_it_has_no_gateway() {
        assert_eq!(choose(&[direct("169.254.20.5")]), Ok(ip("169.254.20.5")));
    }

    #[test]
    fn wired_beats_wifi_by_metric_and_both_are_listed_for_the_host_screen() {
        let list = [wifi("192.168.0.20"), wired("192.168.0.10")];
        assert_eq!(choose(&list), Ok(ip("192.168.0.10")));
        assert_eq!(
            lan_addresses(&list),
            vec![ip("192.168.0.10"), ip("192.168.0.20")],
            "有線が先、無線が後"
        );
    }

    #[test]
    fn a_gateway_beats_a_lower_metric_without_one() {
        let mut direct_wired = direct("169.254.1.2");
        direct_wired.metric = 5;
        assert_eq!(
            choose(&[direct_wired, wifi("192.168.0.20")]),
            Ok(ip("192.168.0.20"))
        );
    }

    #[test]
    fn equal_top_candidates_are_not_chosen() {
        let mut a = wired("192.168.0.10");
        let mut b = wifi("192.168.0.20");
        a.metric = 30;
        b.metric = 30;
        assert_eq!(
            choose(&[a.clone(), b.clone()]),
            Err(Refusal::Tie(vec![ip("192.168.0.10"), ip("192.168.0.20")]))
        );
        // 直結が2本（相手が別々）も選べない。
        assert!(matches!(
            choose(&[direct("169.254.1.2"), direct("169.254.9.9")]),
            Err(Refusal::Tie(_))
        ));
        // 同じアドレスが2つの項目に出ても決められる（重複は数えない）。
        assert_eq!(choose(&[a.clone(), a]), Ok(ip("192.168.0.10")));
    }

    #[test]
    fn a_vpn_or_virtual_machine_adapter_alone_gives_nothing() {
        let tunnel = Adapter {
            description: "WireGuard Tunnel".to_string(),
            if_type: TUNNEL,
            ..wired("10.6.0.2")
        };
        let loopback = Adapter {
            description: "Software Loopback Interface 1".to_string(),
            if_type: LOOPBACK,
            ..wired("127.0.0.1")
        };
        // 有線に見える仮想: 説明の語で外す・MACのOUIで外す（Hyper-Vの既定のスイッチはゲートウェイを持たない）。
        let hyperv = Adapter {
            description: "Hyper-V Virtual Ethernet Adapter".to_string(),
            gateway: false,
            ..wired("172.28.0.1")
        };
        let vbox = Adapter {
            description: "Intel(R) PRO/1000 MT Desktop Adapter".to_string(),
            oui: [0x08, 0x00, 0x27],
            ..wired("192.168.56.1")
        };
        let tap = Adapter {
            description: "TAP-Windows Adapter V9".to_string(),
            ..wired("10.8.0.6")
        };
        for a in [&tunnel, &loopback, &hyperv, &vbox, &tap] {
            assert!(a.is_virtual(), "{a:?}");
            assert_eq!(choose(std::slice::from_ref(a)), Err(Refusal::NoCandidate));
        }
        assert_eq!(
            choose(&[tunnel, loopback, hyperv, vbox, tap]),
            Err(Refusal::NoCandidate)
        );
        // 本物の有線があれば仮想が混ざっていても本物を選ぶ。
        let real = wired("192.168.0.10");
        assert!(!real.is_virtual());
        assert_eq!(
            choose(&[
                Adapter {
                    description: "VMware Virtual Ethernet Adapter for VMnet8".to_string(),
                    metric: 1,
                    ..wired("192.168.153.1")
                },
                real
            ]),
            Ok(ip("192.168.0.10"))
        );
    }

    #[test]
    fn a_hyperv_external_switch_with_a_gateway_counts_as_the_real_lan() {
        let external = Adapter {
            description: "Hyper-V Virtual Ethernet Adapter #2".to_string(),
            oui: [0x00, 0x15, 0x5D],
            ..wired("192.168.0.10")
        };
        let wsl = Adapter {
            description: "Hyper-V Virtual Ethernet Adapter #3".to_string(),
            oui: [0x00, 0x15, 0x5D],
            gateway: false,
            ..wired("172.20.16.1")
        };
        assert!(!external.is_virtual());
        assert!(wsl.is_virtual());
        assert_eq!(
            lan_addresses(&[wsl.clone(), external.clone()]),
            vec![ip("192.168.0.10"), ip("172.20.16.1")],
            "本物の LAN が先"
        );
        assert_eq!(choose(&[wsl, external]), Ok(ip("192.168.0.10")));
        let vmware = Adapter {
            description: "VMware Virtual Ethernet Adapter for VMnet8".to_string(),
            ..wired("192.168.153.1")
        };
        assert!(
            vmware.is_virtual(),
            "ほかの製品はゲートウェイがあっても仮想"
        );
        let vpn = Adapter {
            description: "TAP-Windows Adapter V9".to_string(),
            oui: [0x00, 0xFF, 0x01],
            ..wired("10.8.0.6")
        };
        assert!(vpn.is_virtual());
    }

    /// NICがVMwareの1つだけの仮想マシンでもホストのアドレスは0個にならない。
    #[test]
    fn virtual_adapters_are_listed_after_the_real_ones_but_never_chosen() {
        let guest_nic = Adapter {
            description: "Intel(R) 82574L Gigabit Network Connection".to_string(),
            oui: [0x00, 0x0C, 0x29],
            ..wired("192.168.88.128")
        };
        assert!(guest_nic.is_virtual());
        assert_eq!(
            lan_addresses(std::slice::from_ref(&guest_nic)),
            vec![ip("192.168.88.128")]
        );
        assert_eq!(choose(&[guest_nic]), Err(Refusal::NoCandidate));

        let vmnet = |addr: &str, n: u32| Adapter {
            description: format!("VMware Virtual Ethernet Adapter for VMnet{n}"),
            oui: [0x00, 0x50, 0x56],
            gateway: false,
            metric: 1,
            ..wired(addr)
        };
        let host = [
            vmnet("192.168.192.1", 1),
            vmnet("192.168.88.1", 8),
            wired("10.20.30.133"),
        ];
        assert_eq!(
            lan_addresses(&host),
            vec![ip("10.20.30.133"), ip("192.168.192.1"), ip("192.168.88.1")]
        );
        assert_eq!(choose(&host), Ok(ip("10.20.30.133")));
        // ゲートウェイを持ちメトリックも小さい仮想（VPNなど）でも本物の後ろ。
        let vpn = Adapter {
            description: "TAP-Windows Adapter V9".to_string(),
            metric: 1,
            ..wired("10.8.0.6")
        };
        assert_eq!(
            lan_addresses(&[vpn, wifi("192.168.0.20")]),
            vec![ip("192.168.0.20"), ip("10.8.0.6")]
        );

        // LANの外・使えない状態のものは仮想でも出さない。
        let down = Adapter {
            up: false,
            ..vmnet("192.168.50.1", 2)
        };
        let loopback = Adapter {
            description: "Software Loopback Interface 1".to_string(),
            if_type: LOOPBACK,
            ..wired("127.0.0.1")
        };
        assert!(lan_addresses(&[down, loopback]).is_empty());
    }

    #[test]
    fn a_tentative_link_local_address_or_a_down_adapter_is_not_used() {
        let tentative = Adapter {
            preferred: false,
            ..direct("169.254.20.5")
        };
        assert_eq!(choose(&[tentative]), Err(Refusal::NoCandidate));
        let down = Adapter {
            up: false,
            ..wired("192.168.0.10")
        };
        assert_eq!(choose(&[down]), Err(Refusal::NoCandidate));
        assert_eq!(choose(&[]), Err(Refusal::NoCandidate));
    }

    #[test]
    fn only_lan_ranges_are_candidates() {
        for (addr, want) in [
            ("10.1.2.3", true),
            ("172.16.0.5", true),
            ("172.31.255.1", true),
            ("192.168.1.1", true),
            ("169.254.0.7", true),
            ("172.32.0.1", false),
            ("100.64.0.9", false),
            ("8.8.8.8", false),
            ("127.0.0.1", false),
            ("0.0.0.0", false),
        ] {
            assert_eq!(wired(addr).is_lan_candidate(), want, "{addr}");
        }
        assert_eq!(
            choose(&[wired("203.0.113.5")]),
            Err(Refusal::NoCandidate),
            "グローバルのアドレスは LAN の候補にしない"
        );
    }

    /// VPNのツールのアダプター（有線に見えるEthernet・ゲートウェイ無し・メトリックは小さい）。
    fn vpn_adapter(description: &str, name: &str, addr: &str, prefix: u8, metric: u32) -> Adapter {
        Adapter {
            description: description.to_string(),
            name: name.to_string(),
            prefix,
            oui: [0x7A, 0x79, 0x01],
            gateway: false,
            metric,
            ..wired(addr)
        }
    }

    fn radmin(addr: &str) -> Adapter {
        vpn_adapter(
            "Famatech Radmin VPN Ethernet Adapter",
            "Radmin VPN",
            addr,
            8,
            2,
        )
    }

    fn hamachi(addr: &str) -> Adapter {
        vpn_adapter(
            "LogMeIn Hamachi Virtual Ethernet Adapter",
            "Hamachi",
            addr,
            8,
            3,
        )
    }

    fn zerotier(addr: &str, prefix: u8) -> Adapter {
        vpn_adapter(
            "ZeroTier Virtual Port",
            "ZeroTier One [8056c2e21c000001]",
            addr,
            prefix,
            4,
        )
    }

    fn net(addr: &str, prefix: u8) -> Net4 {
        Net4::new(ip(addr), prefix).unwrap()
    }

    /// VMnetがVPNより前に出ると上限の8個からVPNが落ちる。
    #[test]
    fn vpn_tools_are_listed_with_their_network_between_the_real_lan_and_other_virtual_adapters() {
        let vmnet = Adapter {
            description: "VMware Virtual Ethernet Adapter for VMnet8".to_string(),
            oui: [0x00, 0x50, 0x56],
            gateway: false,
            metric: 1,
            ..wired("192.168.88.1")
        };
        let list = [
            vmnet,
            zerotier("28.9.9.9", 24),
            hamachi("25.1.2.3"),
            radmin("26.12.34.56"),
            wired("192.168.0.10"),
        ];
        let got = host_addresses(&list);
        assert_eq!(
            got,
            vec![
                HostAddr {
                    ip: ip("192.168.0.10"),
                    tool: None,
                    net: None
                },
                HostAddr {
                    ip: ip("26.12.34.56"),
                    tool: Some(VpnTool::Radmin),
                    net: Some(net("26.0.0.0", 8))
                },
                HostAddr {
                    ip: ip("25.1.2.3"),
                    tool: Some(VpnTool::Hamachi),
                    net: Some(net("25.0.0.0", 8))
                },
                HostAddr {
                    ip: ip("28.9.9.9"),
                    tool: Some(VpnTool::ZeroTier),
                    net: Some(net("28.9.9.0", 24))
                },
                HostAddr {
                    ip: ip("192.168.88.1"),
                    tool: None,
                    net: None
                },
            ]
        );
        // ツール自体は仮想として扱う（`choose`の bind = auto の代わりに選ばれない）。
        for a in [
            radmin("26.1.1.1"),
            hamachi("25.1.1.1"),
            zerotier("28.1.1.1", 24),
        ] {
            assert!(a.is_virtual() && !a.is_lan_candidate(), "{a:?}");
            assert_eq!(choose(&[a]), Err(Refusal::NoCandidate));
        }
    }

    /// Tailscale・Down・重複検出の途中・物理の世界のアドレスは使わない。
    #[test]
    fn only_named_vpn_tools_with_a_usable_network_are_listed_outside_the_private_ranges() {
        let corp = vpn_adapter(
            "Acme Corporate VPN Virtual Adapter",
            "Acme VPN",
            "25.1.2.3",
            8,
            1,
        );
        let tailscale = vpn_adapter("Tailscale Tunnel", "Tailscale", "100.101.102.103", 10, 1);
        let physical = wired("26.1.2.3");
        assert!(lan_addresses(&[corp, tailscale, physical]).is_empty());
        for prefix in [0, 7, 31, 32] {
            for a in [
                radmin("26.1.2.3"),
                hamachi("25.1.2.3"),
                zerotier("28.1.2.3", 24),
            ] {
                let a = Adapter { prefix, ..a };
                assert!(
                    lan_addresses(std::slice::from_ref(&a)).is_empty(),
                    "/{prefix} {a:?}"
                );
            }
        }
        for prefix in [8, 9, 24, 30] {
            let a = Adapter {
                prefix,
                ..radmin("26.1.2.3")
            };
            assert_eq!(lan_addresses(&[a]), vec![ip("26.1.2.3")], "/{prefix}");
        }
        let down = Adapter {
            up: false,
            ..radmin("26.1.2.3")
        };
        let tentative = Adapter {
            preferred: false,
            ..radmin("26.1.2.3")
        };
        assert!(lan_addresses(&[down, tentative]).is_empty());
        // ツールの名前が接続の名前だけにあっても使う（説明は物理のNICの名前のまま）。
        let by_name = Adapter {
            name: "Hamachi".to_string(),
            ..wired("25.4.5.6")
        };
        assert!(by_name.is_virtual());
        assert_eq!(lan_addresses(&[by_name]), vec![ip("25.4.5.6")]);
        // 使えないアドレス（loopback・マルチキャスト・0.x）はツールの名前があっても出さない。
        for bad in ["127.0.0.1", "224.0.0.9", "0.1.2.3", "255.255.255.255"] {
            assert!(lan_addresses(&[radmin(bad)]).is_empty(), "{bad}");
        }
    }

    /// 私設のアドレスにネットワークが付くと受付用受信ソケットがlan全体でなく狭い送信元だけを読む。今までの振る舞いが変わる。
    #[test]
    fn a_private_vpn_address_keeps_its_tool_name_but_no_network() {
        let got = host_addresses(&[zerotier("10.147.17.5", 24), wired("192.168.0.10")]);
        assert_eq!(
            got,
            vec![
                HostAddr {
                    ip: ip("192.168.0.10"),
                    tool: None,
                    net: None
                },
                HostAddr {
                    ip: ip("10.147.17.5"),
                    tool: Some(VpnTool::ZeroTier),
                    net: None
                },
            ]
        );
        // 長さが取れなくても私設のアドレスは今までどおり出る。
        let odd = Adapter {
            prefix: 0,
            ..zerotier("10.147.17.5", 24)
        };
        assert_eq!(lan_addresses(&[odd]), vec![ip("10.147.17.5")]);
        assert!(vpn_links(&[zerotier("10.147.17.5", 24)]).is_empty());
    }

    #[test]
    fn vpn_links_are_the_up_non_private_tool_addresses_and_make_the_set() {
        let corp = vpn_adapter("Acme Corporate VPN", "Acme", "25.1.2.3", 8, 1);
        // 使えない状態のものはほかの項目と同じアドレスにせず別のアドレスにする（重なりの除外に紛れないように）。
        let down = Adapter {
            up: false,
            ..hamachi("25.8.8.8")
        };
        let tentative = Adapter {
            preferred: false,
            ..zerotier("28.5.5.5", 24)
        };
        let links = vpn_links(&[
            radmin("26.12.34.56"),
            radmin("26.12.34.56"),
            hamachi("25.1.2.3"),
            zerotier("28.9.9.9", 24),
            zerotier("10.147.17.5", 24),
            corp,
            down,
            tentative,
            Adapter {
                prefix: 7,
                ..radmin("26.7.7.7")
            },
            wired("192.168.0.10"),
        ]);
        assert_eq!(
            links,
            vec![
                VpnLink {
                    ip: ip("26.12.34.56"),
                    tool: VpnTool::Radmin,
                    net: net("26.0.0.0", 8)
                },
                VpnLink {
                    ip: ip("25.1.2.3"),
                    tool: VpnTool::Hamachi,
                    net: net("25.0.0.0", 8)
                },
                VpnLink {
                    ip: ip("28.9.9.9"),
                    tool: VpnTool::ZeroTier,
                    net: net("28.9.9.0", 24)
                },
            ]
        );
        let set = vpn_set(&links);
        assert_eq!(set.len(), 3);
        assert_eq!(set.find(ip("26.200.0.1")), Some(net("26.0.0.0", 8)));
        assert_eq!(set.find(ip("25.200.0.1")), Some(net("25.0.0.0", 8)));
        assert_eq!(set.find(ip("28.9.9.77")), Some(net("28.9.9.0", 24)));
        assert_eq!(set.find(ip("28.9.10.77")), None);
        assert_eq!(set.find(ip("27.0.0.1")), None);
        assert!(vpn_set(&[]).is_empty());
    }

    #[test]
    fn the_refusal_text_names_the_tied_addresses_only_for_a_tie() {
        assert!(!Refusal::NoCandidate.text().contains('('));
        let t = Refusal::Tie(vec![ip("192.168.0.10"), ip("192.168.0.20")]).text();
        assert!(t.contains("192.168.0.10・192.168.0.20"), "{t}");
    }
}
