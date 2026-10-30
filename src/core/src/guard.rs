//! ネットワークのガード。宛先と待ち受けのアドレスをネットワークの範囲（dllのiniの`scope`・peer_simの`--scope`）が許すものに限る。
//! peer_simとdllの通信層が共有する。
//!
//! 範囲は次の3つ。広い方は狭い方を含む。
//! - `loopback`（既定。iniに`scope`を書き忘れても外へ出ない）
//! - `lan`（loopback ＋ 私設のアドレス。100.64.0.0/10のCGNATは事業者の中のアドレスなので含めない）
//! - `internet`（上に加えて外のアドレス）
//!
//! `lan`の仲間に`LanVpn`（VPNの接続で開くときだけ。iniでは選べない）がある。`lan`にそのアダプターのネットワーク1つ（私設でないRadmin VPNの26/8など。`vpn_net`）を足した範囲。
//! どの範囲でも断るもの: 未指定アドレスと0.0.0.0/8、マルチキャスト（UPnPのSSDPの239.255.255.250だけはdllの`upnp`が直接使う）、240.0.0.0/4、IPv4を埋めたIPv6（IPv4の判定を迂回するため）、宛先のポート0。
//! 待ち受けを未指定アドレスにしないのは次の理由による。接続コードに載せるアドレスにならない。範囲の外のアドレスでも待つ形になる。
//! 名前は`check_*`と`allows`では受けない（DNSの問い合わせ自体が外への通信になる）。STUNの問い合わせ先だけは名前でも指定できる（`check_host_name`は文字の形だけを見る）。
//! 送る直前のガード（dllの`Outlet::send`・peer_simの`Tx::raw`）はゲームのスレッドで走る。そのため確保しない`allows`を使う。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::vpn_net::Net4;

/// ネットワークの範囲。並びは狭い順（`Ord`で「この範囲に収まるか」を比べる）。
/// `LanVpn`は`Lan`と`Internet`の間にある。
/// そのため`reach`の戻り値（Loopback・Lan・Internetのどれか）と比べると`Lan`以下だけを通す。
/// 足したネットワークは`admits`が別に見る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum Scope {
    #[default]
    Loopback,
    Lan,
    /// `Lan` ＋ このネットワークの中の1台（ネットワークのアドレスとブロードキャストは除く）。
    LanVpn(Net4),
    Internet,
}

impl Scope {
    pub const NAMES: &'static str = "loopback / lan / internet";

    /// 大文字や別の綴りは断る。黙って既定に落とすと`scope = LAN`と書いてもloopbackのまま開くため。
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "loopback" => Some(Scope::Loopback),
            "lan" => Some(Scope::Lan),
            "internet" => Some(Scope::Internet),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::Loopback => "loopback",
            Scope::Lan | Scope::LanVpn(_) => "lan",
            Scope::Internet => "internet",
        }
    }

    /// LANの接続で開く範囲。自分のアドレス`local`が私設でなくVPNのネットワーク`vpn`の中にあるときだけ`LanVpn`。
    /// それ以外（私設のLAN・VPNのネットワークの外・VPNの指定なし）は`Lan`のまま。VPNでない接続は変えない。
    pub fn lan_via(local: Ipv4Addr, vpn: Option<Net4>) -> Scope {
        match vpn {
            Some(n) if !is_lan_v4(local) && n.contains_host(local) => Scope::LanVpn(n),
            _ => Scope::Lan,
        }
    }
}

/// loopbackのアドレスかどうか。IPv4は127.0.0.0/8。IPv6は`::1`だけ。
/// `::ffff:127.0.0.1`と`0.0.0.0` / `::`は断る（未指定アドレスで待つとLANからも受けられてしまう）。
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// アドレスが入るいちばん狭い範囲。`None`はどの範囲でも断るアドレス。
pub fn reach(ip: IpAddr) -> Option<Scope> {
    match ip {
        IpAddr::V4(v4) => reach_v4(v4),
        IpAddr::V6(v6) => reach_v6(v6),
    }
}

fn reach_v4(ip: Ipv4Addr) -> Option<Scope> {
    let o = ip.octets();
    // 0.0.0.0/8（RFC 1122の「このネットワーク」）・224.0.0.0/4（マルチキャスト）・240.0.0.0/4（予約と255.255.255.255）。
    if o[0] == 0 || ip.is_multicast() || o[0] >= 240 {
        None
    } else if ip.is_loopback() {
        Some(Scope::Loopback)
    } else if ip.is_private() || ip.is_link_local() {
        Some(Scope::Lan)
    } else {
        Some(Scope::Internet)
    }
}

/// 家の中（私設・リンクローカル。loopbackは含まない）のIPv4かどうか。UPnPを頼む待ち受けアドレスの判定。
pub fn is_lan_v4(ip: Ipv4Addr) -> bool {
    reach_v4(ip) == Some(Scope::Lan)
}

/// 家の中かloopbackのIPv4かどうか。UPnPの機器のURLに許すアドレス（loopbackは検査の偽の機器が127.0.0.1で待つため）。
pub fn is_home_v4(ip: Ipv4Addr) -> bool {
    reach_v4(ip).is_some_and(|r| r <= Scope::Lan)
}

/// 世界に通じるIPv4かどうか。
/// `Internet`の範囲から次を除く: 事業者の中のアドレス（100.64.0.0/10）・IETFの予約（192.0.0.0/24）・機器の試験用（198.18.0.0/15）・文書用（192.0.2.0/24・198.51.100.0/24・203.0.113.0/24）。
pub fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    reach_v4(ip) == Some(Scope::Internet)
        && !(o[0] == 100 && o[1] & 0xC0 == 64)
        && !(o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        && !(o[0] == 198 && (o[1] & 0xFE == 18 || (o[1] == 51 && o[2] == 100)))
        && !(o[0] == 203 && o[1] == 0 && o[2] == 113)
}

fn reach_v6(ip: Ipv6Addr) -> Option<Scope> {
    // `to_ipv4`は`::1`も`0.0.0.1`と読んでSomeを返すので`::1`を先に見る。
    if ip.is_loopback() {
        return Some(Scope::Loopback);
    }
    let head = ip.segments()[0];
    // `to_ipv4`がSomeになるのは`::/96`と`::ffff:0:0/96`（IPv4を埋め込んだ形）。
    if ip.is_unspecified() || ip.is_multicast() || ip.to_ipv4().is_some() {
        None
    } else if head & 0xFE00 == 0xFC00 || head & 0xFFC0 == 0xFE80 {
        Some(Scope::Lan)
    } else {
        Some(Scope::Internet)
    }
}

/// 範囲`scope`がアドレス`ip`（いちばん狭い範囲は`r`）を通すかどうか。`LanVpn`は足したネットワークの中の1台も通す。
fn admits(scope: Scope, r: Scope, ip: IpAddr) -> bool {
    r <= scope || matches!((scope, ip), (Scope::LanVpn(n), IpAddr::V4(v4)) if n.contains_host(v4))
}

/// 送ってよい宛先かどうか（確保しない。送る直前のガード）。
pub fn allows(scope: Scope, addr: SocketAddr) -> bool {
    addr.port() != 0 && reach(addr.ip()).is_some_and(|r| admits(scope, r, addr.ip()))
}

fn check_reach(addr: SocketAddr, scope: Scope, what: &str) -> Result<(), String> {
    match reach(addr.ip()) {
        None => Err(format!(
            "{what} {addr} は送り先・受信ソケットとして無効なアドレス（全アドレス・マルチキャスト・ブロードキャスト・IPv4 を埋め込んだ IPv6 など）なので、どのネットワークの範囲でも拒否します"
        )),
        Some(r) if !admits(scope, r, addr.ip()) => Err(format!(
            "{what} {addr} はネットワークの範囲 scope = {} の外（このアドレスには scope = {} 以上が必要）なので拒否します",
            scope.name(),
            r.name()
        )),
        Some(_) => Ok(()),
    }
}

/// 待ち受けのガード。ポート0はOSが選ぶので通す。
pub fn check_bind(addr: SocketAddr, scope: Scope, what: &str) -> Result<(), String> {
    check_reach(addr, scope, what)
}

/// 宛先のガード（相手の候補・STUNの問い合わせ先）。ポート0も断る。
pub fn check_peer(addr: SocketAddr, scope: Scope) -> Result<(), String> {
    check_reach(addr, scope, "宛先")?;
    if addr.port() == 0 {
        return Err(format!("宛先 {addr} のポートが 0 なので送信できません"));
    }
    Ok(())
}

/// 範囲を持たない検査用のツール（peer_simの偽のSTUNの応答役）が使うloopbackだけのガード。
pub fn check_loopback(addr: SocketAddr, what: &str) -> Result<(), String> {
    if is_loopback(addr.ip()) {
        Ok(())
    } else {
        Err(format!(
            "{what} {addr} は loopback（127.0.0.0/8 と ::1）ではないので断る"
        ))
    }
}

/// DNSの名前として読める文字かどうか（形だけ見て名前解決はしない）。
/// ホスト名の規則（RFC 1123・1035）に従い最後のラベルが数字だけの名前（`1.2.3`）は断る。
/// IPの書き損じを名前解決するとOSが別のアドレスと読むことがある。
/// `_`・非ASCII・空のラベル・`:`も断る。
pub fn check_host_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("名前が空です".to_string());
    }
    if name.len() > 253 {
        return Err(format!(
            "名前が長すぎます（{} 文字。253 文字まで）",
            name.len()
        ));
    }
    let mut last = "";
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!(
                "名前 {name:?} のラベル {label:?} は 1〜63 文字にしてください（空のラベル・末尾の . は書けません）"
            ));
        }
        if let Some(c) = label
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
        {
            return Err(format!(
                "名前 {name:?} に使えない文字 {c:?} があります（英数字と - だけ。国際化ドメインは xn-- の形にしてください）"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "名前 {name:?} のラベル {label:?} が - で始まるか終わっています"
            ));
        }
        last = label;
    }
    if last.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "名前 {name:?} の最後のラベルが数字だけです（IP アドレスの書き損じの可能性。アドレスは IP:ポート で書いてください）"
        ));
    }
    Ok(())
}

pub fn is_localhost_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("localhost")
}

#[cfg(test)]
mod tests {
    //! 範囲ごとに通すものと断るものを同じ表に並べる（片方だけだと全部断る・全部通す実装でも通る）。
    //! 期待値はRFCのアドレスの区切りから書いた（`reach`の式から作らない）。
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    use Scope::{Internet, Lan, Loopback};

    /// (宛先, この範囲から通る。Noneはどの範囲でも断る)。各区切りの内側の端と外側の隣を並べる。
    const TABLE: &[(&str, Option<Scope>)] = &[
        ("127.0.0.1:7000", Some(Loopback)),
        ("127.5.6.7:1", Some(Loopback)),
        ("127.255.255.254:65535", Some(Loopback)),
        ("[::1]:7000", Some(Loopback)),
        // 私設のIPv4（RFC 1918・3927）。端と、区切りの外側の隣。
        ("10.0.0.1:7000", Some(Lan)),
        ("10.255.255.254:7000", Some(Lan)),
        ("11.0.0.1:7000", Some(Internet)),
        ("9.255.255.255:7000", Some(Internet)),
        ("172.16.0.1:7000", Some(Lan)),
        ("172.20.1.2:7000", Some(Lan)),
        ("172.31.255.254:7000", Some(Lan)),
        ("172.15.255.254:7000", Some(Internet)),
        ("172.32.0.1:7000", Some(Internet)),
        ("192.168.0.1:7000", Some(Lan)),
        ("192.168.255.254:7000", Some(Lan)),
        ("192.169.0.1:7000", Some(Internet)),
        ("192.167.255.254:7000", Some(Internet)),
        ("169.254.1.2:7000", Some(Lan)),
        ("169.253.1.2:7000", Some(Internet)),
        ("169.255.1.2:7000", Some(Internet)),
        ("100.64.0.1:7000", Some(Internet)),
        ("192.0.2.1:9", Some(Internet)),
        ("128.0.0.1:7000", Some(Internet)),
        ("203.0.113.9:3478", Some(Internet)),
        ("223.255.255.254:7000", Some(Internet)),
        // 私設のIPv6（RFC 4193・4291）
        ("[fc00::1]:7000", Some(Lan)),
        ("[fd12:3456::1]:7000", Some(Lan)),
        ("[fdff:ffff::1]:7000", Some(Lan)),
        ("[fe80::1]:7000", Some(Lan)),
        ("[febf::1]:7000", Some(Lan)),
        ("[fbff::1]:7000", Some(Internet)),
        ("[fec0::1]:7000", Some(Internet)),
        ("[2001:db8::1]:7000", Some(Internet)),
        ("[2a00::1]:7000", Some(Internet)),
        // どの範囲でも断る
        ("0.0.0.0:7000", None),
        ("0.1.2.3:7000", None),
        ("224.0.0.1:7000", None),
        ("239.255.255.250:1900", None),
        ("240.0.0.1:7000", None),
        ("255.255.255.255:7000", None),
        ("[::]:7000", None),
        ("[ff02::1]:7000", None),
        ("[ff0e::1]:7000", None),
        ("[::ffff:127.0.0.1]:7000", None),
        ("[::ffff:192.168.1.2]:7000", None),
        ("[::ffff:203.0.113.9]:7000", None),
        ("[::192.168.1.2]:7000", None),
    ];

    #[test]
    fn each_scope_passes_exactly_its_table_rows() {
        let mut counts = [0usize; 4];
        for &(a, least) in TABLE {
            let addr = sa(a);
            for scope in [Loopback, Lan, Internet] {
                let want = least.is_some_and(|l| l <= scope);
                assert_eq!(allows(scope, addr), want, "{a} / {scope:?}");
                let r = check_peer(addr, scope);
                assert_eq!(r.is_ok(), want, "{a} / {scope:?}: {r:?}");
                assert_eq!(
                    check_bind(addr, scope, "受信ソケット").is_ok(),
                    want,
                    "{a} / {scope:?}"
                );
            }
            counts[least.map_or(3, |l| match l {
                Loopback => 0,
                Lan => 1,
                Internet => 2,
                Scope::LanVpn(_) => unreachable!("表の最小の範囲に LanVpn は無い"),
            })] += 1;
        }
        // 表から行が消えても素通りしないよう行の数を縛る。
        assert_eq!(counts, [4, 13, 17, 13], "表の行の数");
    }

    #[test]
    fn refusals_say_why() {
        let e = check_peer(sa("192.168.1.2:7000"), Loopback).unwrap_err();
        assert!(
            e.contains("scope = loopback") && e.contains("scope = lan"),
            "{e}"
        );
        let e = check_peer(sa("203.0.113.9:3478"), Lan).unwrap_err();
        assert!(
            e.contains("scope = lan") && e.contains("scope = internet"),
            "{e}"
        );
        let e = check_peer(sa("224.0.0.1:7000"), Internet).unwrap_err();
        assert!(e.contains("どのネットワークの範囲でも"), "{e}");
    }

    #[test]
    fn peer_port_zero_is_refused_in_every_scope_but_bind_port_zero_is_not() {
        for scope in [Loopback, Lan, Internet] {
            assert!(check_peer(sa("127.0.0.1:0"), scope).is_err());
            assert!(!allows(scope, sa("127.0.0.1:0")));
            assert_eq!(check_peer(sa("127.0.0.1:1"), scope), Ok(()));
            assert_eq!(check_bind(sa("127.0.0.1:0"), scope, "受信ソケット"), Ok(()));
        }
        assert!(check_peer(sa("10.0.0.1:1"), Loopback).is_err());
        assert!(check_peer(sa("10.0.0.1:0"), Lan).is_err());
    }

    #[test]
    fn scope_names_round_trip_and_unknown_values_are_refused() {
        for s in [Loopback, Lan, Internet] {
            assert_eq!(Scope::parse(s.name()), Some(s));
        }
        for bad in ["", "LAN", "Loopback", "wan", "local", "any", "internet "] {
            assert_eq!(Scope::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Scope::default(), Loopback);
        assert!(Loopback < Lan && Lan < Internet);
    }

    #[test]
    fn lan_home_and_global_v4_are_cut_at_the_rfc_borders() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        // (アドレス, is_lan_v4, is_home_v4, is_global_v4)
        let rows = [
            ("192.168.0.7", true, true, false),
            ("10.255.255.254", true, true, false),
            ("172.16.0.1", true, true, false),
            ("172.32.0.1", false, false, true),
            ("169.254.1.2", true, true, false),
            ("127.0.0.1", false, true, false),
            ("100.64.0.1", false, false, false),
            ("100.127.255.254", false, false, false),
            ("100.128.0.1", false, false, true),
            ("100.63.255.254", false, false, true),
            ("192.0.0.8", false, false, false),
            ("192.0.1.1", false, false, true),
            ("192.0.2.1", false, false, false),
            ("198.17.255.254", false, false, true),
            ("198.18.0.1", false, false, false),
            ("198.19.255.254", false, false, false),
            ("198.20.0.1", false, false, true),
            ("198.51.100.7", false, false, false),
            ("198.51.101.7", false, false, true),
            ("203.0.113.9", false, false, false),
            ("203.0.112.9", false, false, true),
            ("8.8.4.4", false, false, true),
            ("0.1.2.3", false, false, false),
            ("224.0.0.251", false, false, false),
            ("255.255.255.255", false, false, false),
        ];
        for (a, lan, home, global) in rows {
            assert_eq!(
                (is_lan_v4(ip(a)), is_home_v4(ip(a)), is_global_v4(ip(a))),
                (lan, home, global),
                "{a}"
            );
        }
    }

    fn net(a: &str, p: u8) -> Net4 {
        Net4::new(a.parse().unwrap(), p).unwrap()
    }

    #[test]
    fn lan_vpn_adds_exactly_one_network_to_lan() {
        let radmin = Scope::LanVpn(net("26.1.2.3", 8));
        let zt = Scope::LanVpn(net("28.7.7.9", 24));
        // (宛先, radminの26/8, zerotierの28.7.7.0/24, 範囲lanだけ)
        let rows = [
            ("26.0.0.1:7000", true, false, false),
            ("26.12.34.56:1", true, false, false),
            ("26.255.255.254:65535", true, false, false),
            ("26.0.0.0:7000", false, false, false),
            ("26.255.255.255:7000", false, false, false),
            ("25.255.255.254:7000", false, false, false),
            ("27.0.0.1:7000", false, false, false),
            ("28.7.7.1:7000", false, true, false),
            ("28.7.7.254:7000", false, true, false),
            ("28.7.7.0:7000", false, false, false),
            ("28.7.7.255:7000", false, false, false),
            ("28.7.6.1:7000", false, false, false),
            ("28.7.8.1:7000", false, false, false),
            ("192.168.0.1:7000", true, true, true),
            ("10.0.0.1:7000", true, true, true),
            ("169.254.1.2:7000", true, true, true),
            ("127.0.0.1:7000", true, true, true),
            ("8.8.8.8:7000", false, false, false),
            ("100.64.0.1:7000", false, false, false),
            ("[fd00::1]:7000", true, true, true),
            ("[2001:db8::1]:7000", false, false, false),
            ("[::ffff:26.0.0.1]:7000", false, false, false),
            ("26.0.0.1:0", false, false, false),
        ];
        for (a, r, z, l) in rows {
            let addr = sa(a);
            for (scope, want) in [(radmin, r), (zt, z), (Lan, l)] {
                assert_eq!(allows(scope, addr), want, "{a} / {scope:?}");
                let p = check_peer(addr, scope);
                assert_eq!(p.is_ok(), want, "{a} / {scope:?}: {p:?}");
                if addr.port() != 0 {
                    let b = check_bind(addr, scope, "受信ソケット");
                    assert_eq!(b.is_ok(), want, "{a} / {scope:?}: {b:?}");
                }
            }
        }
        // 名前はlanのまま（ログとエラーの文が「scope = lan」で揃う）。順序はlanとinternetの間。
        assert_eq!(radmin.name(), "lan");
        assert!(Lan < radmin && radmin < Internet);
    }

    #[test]
    fn lan_via_widens_only_for_a_non_private_address_inside_the_vpn_network() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let r = net("26.0.0.1", 8);
        assert_eq!(Scope::lan_via(ip("26.5.6.7"), Some(r)), Scope::LanVpn(r));
        assert_eq!(Scope::lan_via(ip("26.5.6.7"), None), Lan);
        assert_eq!(
            Scope::lan_via(ip("25.5.6.7"), Some(r)),
            Lan,
            "ネットワークの外"
        );
        assert_eq!(Scope::lan_via(ip("26.0.0.0"), Some(r)), Lan);
        assert_eq!(Scope::lan_via(ip("192.168.0.9"), None), Lan);
        // 私設のアドレスはVPNのネットワークを渡されても範囲lanのまま。
        let big = net("10.1.2.3", 8);
        assert_eq!(Scope::lan_via(ip("10.1.2.3"), Some(big)), Lan);
        assert_eq!(Scope::lan_via(ip("127.0.0.1"), None), Lan);
    }

    #[test]
    fn check_loopback_stays_loopback_only() {
        assert_eq!(check_loopback(sa("127.0.0.1:7000"), "受信ソケット"), Ok(()));
        assert_eq!(check_loopback(sa("[::1]:7000"), "受信ソケット"), Ok(()));
        for a in [
            "192.168.1.2:7000",
            "0.0.0.0:7000",
            "[::ffff:127.0.0.1]:7000",
        ] {
            let r = check_loopback(sa(a), "受信ソケット");
            assert!(r.unwrap_err().contains("loopback"), "{a}");
        }
    }

    #[test]
    fn host_names_are_checked_by_shape() {
        for ok in [
            "localhost",
            "stun.cloudflare.com",
            "a-b.example.net",
            "xn--eckwd4c7c.example",
            "stun1.l.google.com",
            "a",
            "3com.example",
            "a".repeat(63).as_str(),
            // 63*3 + 61 + 区切り 3 = 253
            format!("{0}.{0}.{0}.{1}", "a".repeat(63), "b".repeat(61)).as_str(),
        ] {
            assert_eq!(check_host_name(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            ".",
            "a..b",
            "stun.example.",
            ".example",
            "-a.example",
            "a-.example",
            "a_b.example",
            "a b.example",
            "スタン.example",
            "::1",
            "[::1]",
            "a:b",
            "1.2.3",
            "256.1.1.1",
            "192.168.1",
            "example.123",
            "a".repeat(64).as_str(),
            // 254文字
            format!("{0}.{0}.{0}.{1}", "a".repeat(63), "b".repeat(62)).as_str(),
        ] {
            assert!(check_host_name(bad).is_err(), "通した: {bad:?}");
        }
        assert!(is_localhost_name("localhost") && is_localhost_name("LocalHost"));
        assert!(
            !is_localhost_name("localhost.example") && !is_localhost_name("stun.cloudflare.com")
        );
    }
}
