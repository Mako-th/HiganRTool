//! ネットワークのガード。宛先と待ち受けのアドレスを、ネットワークの範囲（dll の ini の `scope`、peer_sim の `--scope`）が許すものに限る。peer_sim と dll の通信層が共有する。
//!
//! 範囲は 3 つで、広い方は狭い方を含む。
//! - `loopback`（既定）… 127.0.0.0/8 と `::1` だけ。ini に `scope` を書き忘れても外へ出ないよう既定は狭い方。
//! - `lan` … loopback ＋ 私設のアドレス（IPv4 の 10/8・172.16/12・192.168/16 = RFC 1918、169.254/16 = RFC 3927、
//!   IPv6 の fc00::/7 = RFC 4193、fe80::/10 = RFC 4291）。100.64.0.0/10（CGNAT）は事業者の中のアドレスなので入れない。
//! - `internet` … 上に加えて外のアドレス。
//!
//! どの範囲でも断るもの: 未指定アドレス（`0.0.0.0` / `::`）と 0.0.0.0/8、マルチキャスト（224.0.0.0/4・ff00::/8。ただし UPnP の SSDP の
//! 送り先 239.255.255.250 だけは、このガードを通さず dll の `upnp` が直接使う）、240.0.0.0/4（予約。ブロードキャストを含む）、IPv4 を埋めた IPv6（`::ffff:a.b.c.d` と `::a.b.c.d`。IPv4 の判定を迂回するため）、宛先のポート 0。
//! 待ち受けを未指定アドレスにしないのは、接続コードに載せるアドレスにならず、範囲の外のアドレスでも待つ形になるため。LAN・外で使うときは待ち受けに
//! 自分のそのネットワークのアドレスを書く。
//!
//! 名前（`localhost` など）は `check_*` と `allows` では受けない（DNS の問い合わせ自体が外への通信になる）。STUN の問い合わせ先だけは
//! 名前でも書ける: `check_host_name` は文字の形だけを見て、引くのは開くときの読み取りのスレッドで、引けたアドレスをこのガードへ通す
//! （dll の `net_udp::resolve_stun`）。範囲 loopback で引く名前は `localhost` だけ。送る直前のガード（dll の `Outlet::send`、peer_sim の
//! `Tx::raw`）はゲームのスレッドなので、確保しない `allows` を使う。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// ネットワークの範囲。並びは狭い順（`Ord` で「この範囲に収まるか」を比べる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum Scope {
    #[default]
    Loopback,
    Lan,
    Internet,
}

impl Scope {
    pub const NAMES: &'static str = "loopback / lan / internet";

    /// 大文字や別の綴りは断る。黙って既定に落とすと、`scope = LAN` と書いても loopback のまま開くため。
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
            Scope::Lan => "lan",
            Scope::Internet => "internet",
        }
    }
}

/// loopback のアドレスか。IPv4 は 127.0.0.0/8、IPv6 は `::1` だけ。
/// `::ffff:127.0.0.1` と `0.0.0.0` / `::` は断る（未指定アドレスで待つと LAN からも受けられてしまう）。
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// アドレスが入るいちばん狭い範囲。`None` はどの範囲でも断るアドレス。
pub fn reach(ip: IpAddr) -> Option<Scope> {
    match ip {
        IpAddr::V4(v4) => reach_v4(v4),
        IpAddr::V6(v6) => reach_v6(v6),
    }
}

fn reach_v4(ip: Ipv4Addr) -> Option<Scope> {
    let o = ip.octets();
    // 0.0.0.0/8（RFC 1122 の「このネットワーク」）・224.0.0.0/4（マルチキャスト）・240.0.0.0/4（予約と 255.255.255.255）。
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

/// 家の中（私設・リンクローカル。loopback は含まない）の IPv4 か。UPnP を頼む待ち受けアドレスの判定。
pub fn is_lan_v4(ip: Ipv4Addr) -> bool {
    reach_v4(ip) == Some(Scope::Lan)
}

/// 家の中か loopback の IPv4 か。UPnP の機器の URL に許すアドレス（loopback は検査の偽の機器が 127.0.0.1 で待つため）。
pub fn is_home_v4(ip: Ipv4Addr) -> bool {
    reach_v4(ip).is_some_and(|r| r <= Scope::Lan)
}

/// 世界に通じる IPv4 か。`Internet` の範囲から、事業者の中のアドレス（100.64.0.0/10）・IETF の予約（192.0.0.0/24）・
/// 機器の試験用（198.18.0.0/15）・文書用（192.0.2.0/24・198.51.100.0/24・203.0.113.0/24）を除く。
pub fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    reach_v4(ip) == Some(Scope::Internet)
        && !(o[0] == 100 && o[1] & 0xC0 == 64)
        && !(o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        && !(o[0] == 198 && (o[1] & 0xFE == 18 || (o[1] == 51 && o[2] == 100)))
        && !(o[0] == 203 && o[1] == 0 && o[2] == 113)
}

fn reach_v6(ip: Ipv6Addr) -> Option<Scope> {
    // `to_ipv4` は `::1` も `0.0.0.1` と読んで Some を返すので、`::1` を先に見る。
    if ip.is_loopback() {
        return Some(Scope::Loopback);
    }
    let head = ip.segments()[0];
    // `to_ipv4` が Some になるのは `::/96` と `::ffff:0:0/96`（IPv4 を埋め込んだ形）。
    if ip.is_unspecified() || ip.is_multicast() || ip.to_ipv4().is_some() {
        None
    } else if head & 0xFE00 == 0xFC00 || head & 0xFFC0 == 0xFE80 {
        Some(Scope::Lan)
    } else {
        Some(Scope::Internet)
    }
}

/// 送ってよい宛先か（確保しない。送る直前のガード）。
pub fn allows(scope: Scope, addr: SocketAddr) -> bool {
    addr.port() != 0 && reach(addr.ip()).is_some_and(|r| r <= scope)
}

fn check_reach(addr: SocketAddr, scope: Scope, what: &str) -> Result<(), String> {
    match reach(addr.ip()) {
        None => Err(format!(
            "{what} {addr} は送り先・受信ソケットとして意味の無いアドレス（全部のアドレス・マルチキャスト・ブロードキャスト・IPv4 を埋めた IPv6 など）なので、どのネットワークの範囲でも断る"
        )),
        Some(r) if r > scope => Err(format!(
            "{what} {addr} はネットワークの範囲 scope = {} の外（このアドレスには scope = {} 以上が要る）なので断る",
            scope.name(),
            r.name()
        )),
        Some(_) => Ok(()),
    }
}

/// 待ち受けのガード。ポート 0 は OS が選ぶので通す。
pub fn check_bind(addr: SocketAddr, scope: Scope, what: &str) -> Result<(), String> {
    check_reach(addr, scope, what)
}

/// 宛先のガード（相手の候補・STUN の問い合わせ先）。ポート 0 も断る。
pub fn check_peer(addr: SocketAddr, scope: Scope) -> Result<(), String> {
    check_reach(addr, scope, "宛先")?;
    if addr.port() == 0 {
        return Err(format!("宛先 {addr} のポートが 0 なので送れない"));
    }
    Ok(())
}

/// 範囲を持たない検査用の道具（peer_sim の偽の STUN の応答役）が使う、loopback だけのガード。
pub fn check_loopback(addr: SocketAddr, what: &str) -> Result<(), String> {
    if is_loopback(addr.ip()) {
        Ok(())
    } else {
        Err(format!(
            "{what} {addr} は loopback（127.0.0.0/8 と ::1）ではないので断る"
        ))
    }
}

/// DNS の名前として読める文字か（形だけ。引かない）。ホスト名の規則（RFC 1123・1035）に従い、最後のラベルが数字だけの
/// 名前（`1.2.3`）は断る（IP の書き損じを名前として引くと OS が別のアドレスと読むことがある）。`_`・非 ASCII・
/// 空のラベル・`:` も断る。
pub fn check_host_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("名前が空".to_string());
    }
    if name.len() > 253 {
        return Err(format!(
            "名前が長すぎる（{} 文字。253 文字まで）",
            name.len()
        ));
    }
    let mut last = "";
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!(
                "名前 {name:?} のラベル {label:?} は 1〜63 文字で（空のラベル・末尾の . は書けない）"
            ));
        }
        if let Some(c) = label
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
        {
            return Err(format!(
                "名前 {name:?} に使えない文字 {c:?} がある（英数字と - だけ。国際化ドメインは xn-- の形で）"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "名前 {name:?} のラベル {label:?} が - で始まるか終わる"
            ));
        }
        last = label;
    }
    if last.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "名前 {name:?} の最後のラベルが数字だけ（IP アドレスの書き損じか。アドレスは IP:ポート で書く）"
        ));
    }
    Ok(())
}

pub fn is_localhost_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("localhost")
}

#[cfg(test)]
mod tests {
    //! 範囲ごとに通すものと断るものを同じ表に並べる（片方だけだと、全部断る・全部通す実装でも通る）。
    //! 期待値は RFC のアドレスの区切りから書いた（`reach` の式から作らない）。
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    use Scope::{Internet, Lan, Loopback};

    /// (宛先, この範囲から通る。None はどの範囲でも断る)。各区切りの内側の端と外側の隣を並べる。
    const TABLE: &[(&str, Option<Scope>)] = &[
        ("127.0.0.1:7000", Some(Loopback)),
        ("127.5.6.7:1", Some(Loopback)),
        ("127.255.255.254:65535", Some(Loopback)),
        ("[::1]:7000", Some(Loopback)),
        // 私設の IPv4（RFC 1918・3927）。端と、区切りの外側の隣。
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
        // 私設の IPv6（RFC 4193・4291）
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
            counts[least.map_or(3, |l| l as usize)] += 1;
        }
        // 表から行が消えても素通りしないよう、行の数を縛る。
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
            // 254 文字
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
