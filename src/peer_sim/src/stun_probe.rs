//! `--stun-probe <宛先1> <宛先2>`（開発用。走らせずに STUN の 2 か所へ 1 つの受信ソケットから問い合わせて終わる）。
//!
//! 家庭のルーターが、外へ出る通信の外側のポートを宛先ごとに変えるか（RFC 4787 の endpoint-dependent mapping）を、本物のゲームを
//! 起こさずに見る。同じ受信ソケットから 2 か所の STUN へ聞いて、見えたアドレスのポートが同じなら「宛先に依らない割り当て」（STUN で見えた
//! アドレスを接続コードに載せる方法が成り立つ）、違えば「宛先ごとに変わる割り当て」（STUN で見えたアドレスは相手に使えない）。
//!
//! 走らせ方は cli の `--stun-probe`（範囲 `internet` と一緒にだけ書ける。外の STUN サーバーへ問い合わせる道具なので、範囲を広げた
//! ことを引数に書かせる。本物の STUN サーバーへ問い合わせてよいのは許可を得た後だけ）。検査は loopback の偽の STUN（`stun_fake`）
//! 2 つだけへ問い合わせる。
//!
//! STUN の読み書きと送り直しは core の `stun::Query`、宛先のガードと名前の文字の形は `guard`。名前を引く待ちの上限は dll の
//! `net_udp::lookup_with_timeout` と同じ考え方（OS の名前引きは中断できないので、別のスレッドで引いて上限までしか待たない。dll は
//! cdylib なのでここから呼べず、同じ形を持つ）。

use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use higanr_core::guard::{self, Scope};
use higanr_core::stun::{self, Mapping, Query, QueryState, TransactionId};
use higanr_core::wire::{self, Datagram};

use crate::sim::mix;

/// 名前を引く待ちの上限（dll の `net_udp::DNS_TIMEOUT` と同じ 2 秒）。
pub const DNS_TIMEOUT: Duration = Duration::from_secs(2);

const RECV_POLL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `IP:ポート`。読むときに範囲のガードを通してある。
    Addr(SocketAddr),
    /// `名前:ポート`（例 `stun.cloudflare.com:3478`）。名前は文字の形だけ見てある。範囲のガードは引いた後のアドレスに掛ける。
    Name { host: String, port: u16 },
}

impl Target {
    /// 引数の値を読む。アドレスは範囲のガード（`guard::check_peer`）、名前は文字の形（`guard::check_host_name`）とポート 1〜65535。
    pub fn parse(v: &str, scope: Scope) -> Result<Target, String> {
        if let Ok(a) = v.parse::<SocketAddr>() {
            guard::check_peer(a, scope)?;
            return Ok(Target::Addr(a));
        }
        let hint = "IP:ポート か 名前:ポート（例 stun.cloudflare.com:3478）で";
        let (host, p) = v
            .rsplit_once(':')
            .ok_or_else(|| format!("--stun-probe の値 {v:?} が読めない。{hint}"))?;
        let port = if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            p.parse::<u16>().ok().filter(|&n| n != 0)
        }
        .ok_or_else(|| format!("--stun-probe の値 {v:?} のポート {p:?} は 1〜65535。{hint}"))?;
        guard::check_host_name(host)
            .map_err(|e| format!("--stun-probe の値 {v:?}: {e}。{hint}"))?;
        Ok(Target::Name {
            host: host.to_string(),
            port,
        })
    }

    pub fn text(&self) -> String {
        match self {
            Target::Addr(a) => a.to_string(),
            Target::Name { host, port } => format!("{host}:{port}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub bind: SocketAddr,
    pub scope: Scope,
    pub targets: [Target; 2],
}

pub fn lookup_with_timeout(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<Vec<SocketAddr>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = host.to_string();
    let owned = host.to_string();
    let spawned = std::thread::Builder::new()
        .name("stun-probe-dns".into())
        .spawn(move || {
            let r = (owned.as_str(), port)
                .to_socket_addrs()
                .map(|it| it.collect::<Vec<_>>())
                .map_err(|e| format!("名前 {owned} を引けない: {e}"));
            let _ = tx.send(r);
        });
    if let Err(e) = spawned {
        return Err(format!("名前引きのスレッドを起こせない: {e}"));
    }
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        Err(format!(
            "名前 {name} を {} 秒で引けない（打ち切った）",
            timeout.as_secs_f32()
        ))
    })
}

/// 問い合わせ先をアドレスにする。名前は `lookup` で引き、引けたアドレスのうち受信ソケットと同じ族（`want_v4`）で範囲の中の最初の 1 つ。範囲
/// loopback では `localhost` 以外の名前を引かない（DNS の問い合わせは外への通信）。dll の `net_udp::resolve_stun` と同じ選び方。
pub fn resolve(
    target: &Target,
    scope: Scope,
    want_v4: bool,
    lookup: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
) -> Result<SocketAddr, String> {
    let (host, port) = match target {
        Target::Addr(a) => return Ok(*a),
        Target::Name { host, port } => (host.as_str(), *port),
    };
    if scope == Scope::Loopback && !guard::is_localhost_name(host) {
        return Err(format!(
            "scope = loopback では localhost 以外の名前 {host} を引かない（DNS の問い合わせは外への通信）"
        ));
    }
    let found = lookup(host, port)?;
    found
        .iter()
        .copied()
        .find(|a| a.is_ipv4() == want_v4 && guard::check_peer(*a, scope).is_ok())
        .ok_or_else(|| {
            format!(
                "名前 {host} を引いたアドレス {found:?} に、受信ソケットと同じ族（{}）で scope = {} の中のものが無い",
                if want_v4 { "IPv4" } else { "IPv6" },
                scope.name()
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// アドレスが同じ（宛先に依らない割り当て）。
    Same,
    /// ポートが違う（宛先ごとに変わる割り当て。IP が違っても、ポートが違えばこれ）。
    PortDiffers,
    /// ポートは同じで IP だけ違う（宛先によって出口の IP が変わる回線）。
    IpDiffers,
}

/// 2 つの「見えたアドレス」を比べる。判定そのものは core の `stun::compare_mappings`（dll の診断と同じ）。ここはその 4 通りを、この
/// 道具の 3 通りの言い方にコピーするだけ。族が違えば None。
pub fn verdict(a: SocketAddr, b: SocketAddr) -> Option<Verdict> {
    Some(match stun::compare_mappings(a, b)? {
        Mapping::Same => Verdict::Same,
        Mapping::AddrOnly => Verdict::IpDiffers,
        Mapping::PortOnly | Mapping::Both => Verdict::PortDiffers,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub server: SocketAddr,
    pub mapped: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// 開いた受信ソケット（`--bind` のポートが 0 のとき、OS が選んだアドレス）。
    pub local: SocketAddr,
    pub answers: [Answer; 2],
    pub verdict: Verdict,
}

/// 1 か所へ問い合わせる（`stun::Query`。250 ms ごとに 8 回、計 2 秒で打ち切る）。`sock` から出し、`server` から来た STUN の
/// 応答だけを `Query` へ渡す（ほかの送り元・STUN でないものは捨てる）。宛先は範囲の中だけ（送る前にガードを通す）。
pub fn query(
    sock: &UdpSocket,
    server: SocketAddr,
    scope: Scope,
    txn: TransactionId,
) -> Result<SocketAddr, String> {
    guard::check_peer(server, scope)?;
    sock.set_read_timeout(Some(RECV_POLL))
        .map_err(|e| format!("受信の待ちの上限を置けない: {e}"))?;
    let mut q = Query::new(txn);
    let t0 = Instant::now();
    let mut buf = [0u8; 1500];
    loop {
        let now = (t0.elapsed().as_millis() & u128::from(u32::MAX)) as u32;
        if let Some(req) = q.poll_send(now) {
            sock.send_to(&req, server)
                .map_err(|e| format!("STUN の問い合わせを送れない（{server}）: {e}"))?;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let bytes = buf.get(..n).unwrap_or(&[]);
                if from == server && wire::classify(bytes) == Datagram::Stun {
                    q.on_datagram(bytes);
                }
            }
            // 待ちの上限、または前の送りへの ICMP（Windows は次の受信に WSAECONNRESET で出す）。どちらも次の周回で
            // 送り直しか打ち切りが決まる。
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) => return Err(format!("STUN の応答を受けられない: {e}")),
        }
        match q.state() {
            QueryState::Done(a) => return Ok(a),
            QueryState::TimedOut => {
                return Err(format!(
                    "STUN の答えが {} 回の問い合わせで返らない（{server}）",
                    higanr_core::stun::QUERY_TRIES
                ));
            }
            QueryState::Waiting => {}
        }
    }
}

/// transaction id（96 bit）。宛先の番号ごとに変える。peer_sim は OS の乱数を持たない（std だけ）ので、時刻とプロセス番号から作る。
/// この道具の問い合わせは推し量られて偽の応答を差し込まれても、見えたアドレスの表示が違うだけで害は無い。
fn probe_txn(index: u64) -> TransactionId {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let a = mix(t ^ index);
    let b = mix(a ^ u64::from(std::process::id()));
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes()[..4]);
    out
}

/// 1 つの受信ソケット `sock`（`args.bind` で開いたもの）から 2 か所へ順に問い合わせ、見えたアドレスを比べる。片方でも答えが
/// 無ければ Err（比べられないので、黙って「同じ」にしない）。`lookup` は名前を引く道具（検査は注入する）。
pub fn run(
    args: &Args,
    sock: &UdpSocket,
    lookup: impl Fn(&str, u16) -> Result<Vec<SocketAddr>, String>,
) -> Result<Report, String> {
    let local = sock
        .local_addr()
        .map_err(|e| format!("受信ソケットが分からない: {e}"))?;
    guard::check_bind(local, args.scope, "受信ソケット")?;
    let want_v4 = local.is_ipv4();
    let servers = [
        resolve(&args.targets[0], args.scope, want_v4, &lookup)?,
        resolve(&args.targets[1], args.scope, want_v4, &lookup)?,
    ];
    // 同じ宛先へ 2 回聞くと、宛先に依る割り当てでも必ず「同じ」になる（比べる意味が無い）。
    if servers[0] == servers[1] {
        return Err(format!(
            "2 つの宛先が同じアドレス {}（別々の STUN サーバーを書く。宛先が同じだと割り当てが宛先に依るかを比べられない）",
            servers[0]
        ));
    }
    let mut mapped = [servers[0]; 2];
    for (i, (&server, m)) in servers.iter().zip(mapped.iter_mut()).enumerate() {
        *m = query(sock, server, args.scope, probe_txn(i as u64))?;
    }
    let verdict = verdict(mapped[0], mapped[1]).ok_or_else(|| {
        format!(
            "2 つの答えのアドレスの族が違う（{} と {}）。比べられない",
            mapped[0], mapped[1]
        )
    })?;
    Ok(Report {
        local,
        answers: [
            Answer {
                server: servers[0],
                mapped: mapped[0],
            },
            Answer {
                server: servers[1],
                mapped: mapped[1],
            },
        ],
        verdict,
    })
}

pub fn report_text(args: &Args, r: &Report) -> String {
    let mut s = String::new();
    for (i, (t, a)) in args.targets.iter().zip(r.answers.iter()).enumerate() {
        s.push_str(&format!(
            "stun-probe: 宛先{} {}（{}）から見えたアドレス {}（受信ソケット {}）\n",
            i + 1,
            t.text(),
            a.server,
            a.mapped,
            r.local
        ));
    }
    let [a, b] = r.answers;
    s.push_str(&match r.verdict {
        Verdict::Same => format!(
            "stun-probe: ポートが同じ（宛先に依らない割り当て）: {}\n",
            a.mapped.port()
        ),
        Verdict::PortDiffers => format!(
            "stun-probe: ポートが違う（宛先ごとに変わる割り当て）: {} と {}\n",
            a.mapped.port(),
            b.mapped.port()
        ),
        Verdict::IpDiffers => format!(
            "stun-probe: ポートは同じ {} だが IP が違う（宛先で出口の IP が変わる）: {} と {}\n",
            a.mapped.port(),
            a.mapped.ip(),
            b.mapped.ip()
        ),
    });
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stun_fake::StunFake;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn args_for(a: &Target, b: &Target) -> Args {
        Args {
            bind: sa("127.0.0.1:0"),
            scope: Scope::Internet,
            targets: [a.clone(), b.clone()],
        }
    }

    fn wait_answered(f: &StunFake) {
        let t0 = Instant::now();
        while f.answered() == 0 && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn no_lookup(_: &str, _: u16) -> Result<Vec<SocketAddr>, String> {
        Err("名前を引いた".to_string())
    }

    #[test]
    fn verdict_compares_the_port_first() {
        let a = sa("203.0.113.5:40000");
        assert_eq!(verdict(a, sa("203.0.113.5:40000")), Some(Verdict::Same));
        assert_eq!(
            verdict(a, sa("203.0.113.5:40001")),
            Some(Verdict::PortDiffers)
        );
        assert_eq!(
            verdict(a, sa("198.51.100.7:40001")),
            Some(Verdict::PortDiffers)
        );
        assert_eq!(
            verdict(a, sa("198.51.100.7:40000")),
            Some(Verdict::IpDiffers)
        );
        assert_eq!(verdict(a, sa("[2001:db8::7]:40000")), None);
    }

    #[test]
    fn targets_are_gated_by_shape_and_scope() {
        assert_eq!(
            Target::parse("203.0.113.9:3478", Scope::Internet),
            Ok(Target::Addr(sa("203.0.113.9:3478")))
        );
        assert_eq!(
            Target::parse("stun.cloudflare.com:3478", Scope::Internet),
            Ok(Target::Name {
                host: "stun.cloudflare.com".to_string(),
                port: 3478
            })
        );
        for bad in [
            "203.0.113.9:0",
            "224.0.0.1:3478",
            "stun.cloudflare.com",
            "stun.cloudflare.com:0",
            "stun.cloudflare.com:70000",
            "stun.cloudflare.com:",
            "stun_x.example:3478",
            "1.2.3:3478",
            ":3478",
        ] {
            assert!(
                Target::parse(bad, Scope::Internet).is_err(),
                "{bad} を通した"
            );
        }
        assert!(Target::parse("203.0.113.9:3478", Scope::Lan).is_err());
    }

    #[test]
    fn resolve_picks_a_same_family_address_inside_the_scope() {
        let name = Target::Name {
            host: "stun.test.example".to_string(),
            port: 3478,
        };
        let wan = sa("203.0.113.1:3478");
        let wan6 = sa("[2001:db8::1]:3478");
        let mcast = sa("224.0.0.1:3478");
        assert_eq!(
            resolve(&name, Scope::Internet, true, |_, _| Ok(vec![
                wan6, mcast, wan
            ])),
            Ok(wan)
        );
        assert_eq!(
            resolve(&name, Scope::Internet, false, |_, _| Ok(vec![wan, wan6])),
            Ok(wan6)
        );
        assert!(resolve(&name, Scope::Lan, true, |_, _| Ok(vec![wan])).is_err());
        let e = resolve(&name, Scope::Internet, true, |_, _| Err("合成".to_string()));
        assert_eq!(e, Err("合成".to_string()));
        let e = resolve(&name, Scope::Loopback, true, |_, _| panic!("名前を引いた")).unwrap_err();
        assert!(e.contains("localhost 以外"), "{e}");
    }

    #[test]
    fn two_fakes_give_same_or_different_ports() {
        let f1 = StunFake::spawn().unwrap();
        let f2 = StunFake::spawn().unwrap();
        let (t1, t2) = (Target::Addr(f1.addr()), Target::Addr(f2.addr()));
        let a = args_for(&t1, &t2);
        let sock = UdpSocket::bind(a.bind).unwrap();
        let r = run(&a, &sock, no_lookup).unwrap();
        assert_eq!(r.verdict, Verdict::Same);
        assert_eq!(r.answers[0].mapped, r.local);
        assert_eq!(r.answers[1].mapped, r.local);
        wait_answered(&f1);
        wait_answered(&f2);
        assert_eq!((f1.answered(), f2.answered()), (1, 1));
        let text = report_text(&a, &r);
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(
            text.contains("ポートが同じ（宛先に依らない割り当て）")
                && !text.contains("ポートが違う"),
            "{text}"
        );

        let g2 = StunFake::spawn_shifted(9).unwrap();
        let a = args_for(&t1, &Target::Addr(g2.addr()));
        let sock = UdpSocket::bind(a.bind).unwrap();
        let r = run(&a, &sock, no_lookup).unwrap();
        assert_eq!(r.verdict, Verdict::PortDiffers);
        assert_eq!(r.answers[0].mapped.port(), r.local.port());
        assert_eq!(r.answers[1].mapped.port(), r.local.port() + 9);
        let text = report_text(&a, &r);
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(
            text.contains(&format!(
                "ポートが違う（宛先ごとに変わる割り当て）: {} と {}",
                r.local.port(),
                r.local.port() + 9
            )) && !text.contains("ポートが同じ"),
            "{text}"
        );
    }

    #[test]
    fn a_name_target_is_resolved_and_queried() {
        let f1 = StunFake::spawn().unwrap();
        let f2 = StunFake::spawn().unwrap();
        let t1 =
            Target::parse(&format!("localhost:{}", f1.addr().port()), Scope::Internet).unwrap();
        let t2 = Target::Addr(f2.addr());
        let a = args_for(&t1, &t2);
        let sock = UdpSocket::bind(a.bind).unwrap();
        let r = run(&a, &sock, |h, p| lookup_with_timeout(h, p, DNS_TIMEOUT)).unwrap();
        assert_eq!(r.answers[0].server, f1.addr());
        assert_eq!(r.verdict, Verdict::Same);
    }

    #[test]
    fn identical_destinations_and_silent_servers_are_errors() {
        let f1 = StunFake::spawn().unwrap();
        let t1 = Target::Addr(f1.addr());
        let a = args_for(&t1, &t1);
        let sock = UdpSocket::bind(a.bind).unwrap();
        let e = run(&a, &sock, no_lookup).unwrap_err();
        assert!(e.contains("同じアドレス"), "{e}");
        assert_eq!(f1.answered(), 0, "比べないのに問い合わせた");

        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a = args_for(&t1, &Target::Addr(silent.local_addr().unwrap()));
        let sock = UdpSocket::bind(a.bind).unwrap();
        let t0 = Instant::now();
        let e = run(&a, &sock, no_lookup).unwrap_err();
        assert!(e.contains("答えが") && e.contains("返らない"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(6), "打ち切りが遅い");
        wait_answered(&f1);
        assert_eq!(f1.answered(), 1, "1 つ目には聞いた");
    }
}
