//! ネットワークアダプターの一覧から、LAN で使う自分の IPv4 アドレスを選ぶ（pure）。
//!
//! `bind = auto:ポート` は外へ出る経路の送り元を引く（`net_udp::probe_route_ip`）。デフォルト経路の無い PC（ルーターなしの直結・
//! 169.254 だけ）ではそれが引けないので、そのときだけ一覧から選ぶ。LAN のホストが「自分のアドレスを全部見せる」ときも同じ一覧を使う。
//! 一覧を引くのは dll の Win32 の層（`adapters_win`）で、ここは決まった入力から選ぶ判断だけを持つ。

use std::net::Ipv4Addr;

use crate::guard;

/// `IP_ADAPTER_ADDRESSES` の `IfType` のうち、物理のアダプター（有線・無線）として扱うもの。
/// 6 = Ethernet、62 = Fast Ethernet、69 = Fibre Ethernet、71 = IEEE 802.11（無線）、117 = Gigabit Ethernet（IANA ifType）。
/// それ以外（24 = loopback、23 = PPP、131 = トンネル、53 = 仮想など）は仮想として外す。
const PHYSICAL_IF_TYPES: [u32; 5] = [6, 62, 69, 71, 117];

/// 仮想のアダプターの説明に入る語（小文字で比べる）。有線に見える仮想のアダプター（Hyper-V の vEthernet・VMware・VirtualBox・VPN の
/// TAP など）は `IfType` が Ethernet なので、説明でも見分ける。一覧は推定（OS の説明の文字列は製品ごとに違うため、漏れうる）。
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

/// Hyper-V のホストの仮想 NIC の説明に入る語（`VIRTUAL_WORDS` の一部。ゲートウェイがあれば仮想と見なさない）。
const HYPERV_WORDS: [&str; 2] = ["hyper-v", "vethernet"];

/// 仮想マシンの製品が割り当てる MAC アドレスの上位 3 バイト（OUI）: VMware（00:50:56・00:0C:29・00:05:69）・VirtualBox（08:00:27）・
/// Hyper-V（00:15:5D）・Parallels（00:1C:42）・Xen（00:16:3E）。
const VIRTUAL_OUIS: [[u8; 3]; 7] = [
    [0x00, 0x50, 0x56],
    [0x00, 0x0C, 0x29],
    [0x00, 0x05, 0x69],
    [0x08, 0x00, 0x27],
    [0x00, 0x15, 0x5D],
    [0x00, 0x1C, 0x42],
    [0x00, 0x16, 0x3E],
];

/// アダプターの IPv4 アドレス 1 つぶんの材料（アドレスごとに 1 つ。同じアダプターのアドレスが 2 つなら 2 つ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// 説明（`Description`）。仮想かどうかの見分けにだけ使い、画面にもログにも出さない。
    pub description: String,
    /// `IfType`。
    pub if_type: u32,
    /// MAC アドレスの上位 3 バイト（無ければ 0）。
    pub oui: [u8; 3],
    /// 動作状態が Up か。
    pub up: bool,
    /// アドレスの重複検出が済んで使える状態（Preferred）か。重複検出の途中（Tentative。直結で 169.254 を自分に割り当てている間）や
    /// 重複・非推奨のアドレスは偽。
    pub preferred: bool,
    pub ip: Ipv4Addr,
    /// ゲートウェイを持つか。
    pub gateway: bool,
    /// インターフェースのメトリック（小さいほど優先）。
    pub metric: u32,
}

impl Adapter {
    /// 仮想のアダプター（トンネル・VPN・仮想マシン・loopback）か。
    pub fn is_virtual(&self) -> bool {
        if !PHYSICAL_IF_TYPES.contains(&self.if_type) {
            return true;
        }
        let d = self.description.to_ascii_lowercase();
        // Hyper-V の外部スイッチを作った PC は、物理の NIC のアドレスが vEthernet（説明は Hyper-V の仮想の Ethernet）へ移る。ゲートウェイを
        // 持つものは本物の LAN とみなす（既定のスイッチ・内部のスイッチ・WSL の vEthernet はゲートウェイを持たない。推定で、実物では確かめていない）。
        if self.gateway && HYPERV_WORDS.iter().any(|w| d.contains(w)) {
            return false;
        }
        if VIRTUAL_OUIS.contains(&self.oui) {
            return true;
        }
        VIRTUAL_WORDS.iter().any(|w| d.contains(w))
    }

    /// LAN で使える候補か: Up・アドレスが使える状態・仮想でない・LAN の範囲（私設かリンクローカル。`guard::is_lan_v4`）。
    pub fn is_lan_candidate(&self) -> bool {
        self.up && self.preferred && !self.is_virtual() && guard::is_lan_v4(self.ip)
    }

    /// 優先の高さ（大きいほど優先）: ゲートウェイあり、次にメトリックが小さい。
    fn rank(&self) -> (bool, std::cmp::Reverse<u32>) {
        (self.gateway, std::cmp::Reverse(self.metric))
    }
}

/// 選べなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// LAN で使えるアドレスが 1 つも無い。
    NoCandidate,
    /// いちばん優先のアドレスが複数あって、どれか決められない（直結の相手が違う・メトリックが同じ有線と無線など）。
    Tie(Vec<Ipv4Addr>),
}

impl Refusal {
    /// ログと画面に出す理由（次にすること「アドレスを手で書く」は呼び手が足す）。
    pub fn text(&self) -> String {
        match self {
            Refusal::NoCandidate => {
                "アダプターの一覧に LAN で使える IPv4 アドレスが無い".to_string()
            }
            Refusal::Tie(v) => {
                let list: Vec<String> = v.iter().map(Ipv4Addr::to_string).collect();
                format!(
                    "アダプターの一覧に同じ優先のアドレスが複数あって決められない（{}）",
                    list.join("・")
                )
            }
        }
    }
}

/// LAN で使える自分のアドレスを、優先の高い順に並べる（同じアドレスは 1 つ）。LAN のホストが自分のアドレスを全部見せるときに使う。
pub fn lan_addresses(all: &[Adapter]) -> Vec<Ipv4Addr> {
    let mut c: Vec<&Adapter> = all.iter().filter(|a| a.is_lan_candidate()).collect();
    c.sort_by_key(|a| std::cmp::Reverse(a.rank()));
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for a in c {
        if !out.contains(&a.ip) {
            out.push(a.ip);
        }
    }
    out
}

/// `bind = auto` が経路を引けないときの、LAN で使う自分のアドレスの選び方。ゲートウェイを持つものを先に、次にメトリックの小さいもの。
/// いちばん優先のものが複数あれば選ばない（間違った側で待つより、手で書いてもらう）。
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

    /// 物理の有線で、Up・使える状態・ゲートウェイあり・メトリック 25 の見本。
    fn wired(addr: &str) -> Adapter {
        Adapter {
            description: "Realtek PCIe GbE Family Controller".to_string(),
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

    /// 直結（ルーターなし）: ゲートウェイは無く、169.254 を自分に割り当てている。
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
        // 直結が 2 本（相手が別々）も選べない。
        assert!(matches!(
            choose(&[direct("169.254.1.2"), direct("169.254.9.9")]),
            Err(Refusal::Tie(_))
        ));
        // 同じアドレスが 2 つの項目に出ても、決められる（重複は数えない）。
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
        // 有線に見える仮想: 説明の語で外す・MAC の製品の印で外す（Hyper-V の既定のスイッチはゲートウェイを持たない）。
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
        // 本物の有線があれば、仮想が混ざっていても本物を選ぶ。
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

    /// 壊れたら落ちる: Hyper-V の外部スイッチを作った PC（本物の LAN のアドレスが vEthernet に載る）でホストのアドレスが 1 つも出ない、
    /// ゲートウェイの無い vEthernet（既定のスイッチ・WSL）やほかの製品の仮想 NIC までゲートウェイで候補になる。
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
            vec![ip("192.168.0.10")]
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

    #[test]
    fn the_refusal_text_names_the_tied_addresses_only_for_a_tie() {
        assert!(!Refusal::NoCandidate.text().contains('('));
        let t = Refusal::Tie(vec![ip("192.168.0.10"), ip("192.168.0.20")]).text();
        assert!(t.contains("192.168.0.10・192.168.0.20"), "{t}");
    }
}
