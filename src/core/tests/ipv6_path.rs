//! IPv6 の経路の組み上がりの検査。公開の入口（`Link`）だけを使う。
//!
//! 2 つの `Link` を偽のネットワーク（1 ms ごとに進む固定の模型）で繋ぐ。各 `Link` は IPv4 のソケットと IPv6 のソケットを 1 つずつ持ち、宛先の族と
//! 同じ族のソケットから出て、相手の同じ族のソケットへ着く。族ごとに「生きている / 全部捨てる」を切り替えられる。IPv6 の送り元は
//! OS が返す形をまねて flowinfo と scope_id を付けて渡す（`SocketAddr` の `==` はそれらも比べるので、アドレスとポートだけで
//! 比べるガードが崩れると繋がらない）。アドレスは文書用の範囲（203.0.113.0/24）と、2400:cb00:: の架空の IPv6。

use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::{Candidates, same_endpoint};
use higanr_core::handshake::{self, Name};
use higanr_core::link::{Config, Link, Outlet, Recv};
use higanr_core::wire::{self, Hello, MAX_DATAGRAM, Message, Mode, Seat};

const SID: u32 = 0x1A2B_3C4D;
const SECRET: Secret = Secret([0x5E; 16]);

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn a_v4() -> SocketAddr {
    sa("203.0.113.1:42001")
}
fn b_v4() -> SocketAddr {
    sa("203.0.113.2:42002")
}
fn a_v6() -> SocketAddr {
    sa("[2400:cb00::a]:42001")
}
fn b_v6() -> SocketAddr {
    sa("[2400:cb00::b]:42002")
}

/// OS が返す形をまねた IPv6 の送り元（flowinfo・scope_id が 0 でない）。
fn os_v6(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(*v6.ip(), v6.port(), 0x0A_BCDE, 11)),
        v4 => v4,
    }
}

fn hs_cfg(nonce: u64) -> handshake::Config {
    handshake::Config {
        session_id: SID,
        role_nonce: nonce,
        latency: 2,
        name: Name::new("t"),
        host_seat: Seat::P1,
    }
}

fn link(peers: &[SocketAddr], nonce: u64) -> Link {
    Link::new(
        Config {
            handshake: hs_cfg(nonce),
            secret: SECRET,
            peers: Candidates::from_addrs(peers).unwrap(),
            key_seed: higanr_core::seal::KeySeed([nonce as u8; 32]),
        },
        0,
    )
}

fn key() -> AuthKey {
    AuthKey::derive(&SECRET, SID)
}

fn enc(msg: &Message) -> Vec<u8> {
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(&key(), SID, msg, &mut buf);
    buf[..n].to_vec()
}

fn hello(nonce: u64) -> Message {
    let mut name = [0u8; wire::NAME_MAX];
    name[0] = b'x';
    Message::Hello(Hello {
        proto_ver: wire::PROTO_VER,
        latency: 2,
        name_len: 1,
        mode: Mode::Delay,
        can_rollback: false,
        listable: false,
        role_nonce: nonce,
        name,
        seat_wish: wire::SeatWish::P1,
        arrival: wire::Arrival::Unknown,
        build: higanr_core::update::BUILD_NUMBER,
        public: higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed(
            [nonce as u8; 32],
        ))
        .public(),
    })
}

#[derive(Default)]
struct Q(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Q {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

type PerFamily = [u32; 2];

fn fam(a: SocketAddr) -> usize {
    usize::from(a.is_ipv6())
}

/// 2 つの `Link` と偽のネットワーク。A の相手の候補は `[B の IPv4, B の IPv6]`、B は逆。
struct Net {
    a: Link,
    b: Link,
    qa: Q,
    qb: Q,
    now: u32,
    alive: [bool; 2],
    delivered: PerFamily,
    dropped: PerFamily,
}

impl Net {
    fn new(a_nonce: u64, b_nonce: u64, v4_alive: bool, v6_alive: bool) -> Net {
        Net {
            a: link(&[b_v4(), b_v6()], a_nonce),
            b: link(&[a_v4(), a_v6()], b_nonce),
            qa: Q::default(),
            qb: Q::default(),
            now: 0,
            alive: [v4_alive, v6_alive],
            delivered: [0; 2],
            dropped: [0; 2],
        }
    }

    fn step(&mut self) {
        self.now += 1;
        let t = self.now;
        self.a.poll(t, &mut self.qa);
        self.b.poll(t, &mut self.qb);
        for _ in 0..4 {
            let from_a = std::mem::take(&mut self.qa.0);
            let from_b = std::mem::take(&mut self.qb.0);
            if from_a.is_empty() && from_b.is_empty() {
                break;
            }
            for (to, bytes) in from_a {
                let src = if to == b_v4() {
                    a_v4()
                } else if to == b_v6() {
                    a_v6()
                } else {
                    panic!("A が候補でない宛先へ送った: {to}");
                };
                let f = fam(to);
                if self.alive[f] {
                    self.delivered[f] += 1;
                    self.b.on_datagram(os_v6(src), &bytes, t, &mut self.qb);
                } else {
                    self.dropped[f] += 1;
                }
            }
            for (to, bytes) in from_b {
                let src = if to == a_v4() {
                    b_v4()
                } else if to == a_v6() {
                    b_v6()
                } else {
                    panic!("B が候補でない宛先へ送った: {to}");
                };
                let f = fam(to);
                if self.alive[f] {
                    self.delivered[f] += 1;
                    self.a.on_datagram(os_v6(src), &bytes, t, &mut self.qa);
                } else {
                    self.dropped[f] += 1;
                }
            }
        }
    }

    fn run_until_connected(&mut self, max_ms: u32) {
        for _ in 0..max_ms {
            self.step();
            if self.a.connected().is_some() && self.b.connected().is_some() {
                break;
            }
        }
        for _ in 0..max_ms.min(1_500) {
            self.step();
        }
    }

    fn connected(&self) -> bool {
        self.a.connected().is_some() && self.b.connected().is_some()
    }
}

fn path_is(l: &Link, want: SocketAddr) -> bool {
    l.path().is_some_and(|p| same_endpoint(p, want))
}

#[test]
fn two_links_connect_over_ipv6_when_every_ipv4_candidate_is_dead() {
    for (a_nonce, b_nonce) in [(1u64, 2u64), (9, 2)] {
        let tag = format!("A {a_nonce} / B {b_nonce}");
        let mut net = Net::new(a_nonce, b_nonce, false, true);
        net.run_until_connected(3_000);
        assert!(
            net.connected(),
            "{tag}: 繋がらない（A {:?} / B {:?}）",
            net.a.handshake_state(),
            net.b.handshake_state()
        );
        assert!(
            path_is(&net.a, b_v6()),
            "{tag}: A の経路 {:?}",
            net.a.path()
        );
        assert!(
            path_is(&net.b, a_v6()),
            "{tag}: B の経路 {:?}",
            net.b.path()
        );
        assert!(
            !net.a.path_is_learned() && !net.b.path_is_learned(),
            "{tag}"
        );
        let (ca, cb) = (net.a.recv_counts(), net.b.recv_counts());
        assert_eq!((ca.learned, ca.foreign), (0, 0), "{tag}: {ca:?}");
        assert_eq!((cb.learned, cb.foreign), (0, 0), "{tag}: {cb:?}");
        assert!(
            net.dropped[0] > 0,
            "{tag}: IPv4 のアドレスへ送っていない（候補の全部へ送る形が崩れた）"
        );
        assert!(net.delivered[1] > 0, "{tag}: IPv6 で何も届いていない");
        assert_eq!(net.delivered[0], 0, "{tag}: 死んでいるはずの IPv4 に届いた");
        assert!(
            net.a
                .connected()
                .and_then(|c| c.liveness().rtt_ms())
                .is_some(),
            "{tag}: A に RTT が無い（IPv6 からの PONG を受けていない）"
        );
        assert!(
            net.b
                .connected()
                .and_then(|c| c.liveness().rtt_ms())
                .is_some(),
            "{tag}: B に RTT が無い"
        );
    }
}

#[test]
fn two_links_still_connect_over_ipv4_when_every_ipv6_candidate_is_dead() {
    for (a_nonce, b_nonce) in [(1u64, 2u64), (9, 2)] {
        let tag = format!("A {a_nonce} / B {b_nonce}");
        let mut net = Net::new(a_nonce, b_nonce, true, false);
        net.run_until_connected(3_000);
        assert!(net.connected(), "{tag}: IPv4 で繋がらない");
        assert!(
            path_is(&net.a, b_v4()),
            "{tag}: A の経路 {:?}",
            net.a.path()
        );
        assert!(
            path_is(&net.b, a_v4()),
            "{tag}: B の経路 {:?}",
            net.b.path()
        );
        assert!(net.dropped[1] > 0, "{tag}: IPv6 のアドレスへ送っていない");
        assert_eq!(net.delivered[1], 0, "{tag}");
        assert!(
            net.a
                .connected()
                .and_then(|c| c.liveness().rtt_ms())
                .is_some(),
            "{tag}"
        );
    }
}

#[test]
fn with_both_families_alive_the_link_connects_and_prefers_ipv6() {
    let mut net = Net::new(1, 2, true, true);
    net.run_until_connected(3_000);
    assert!(net.connected());
    assert!(path_is(&net.a, b_v6()), "A の経路 {:?}", net.a.path());
    assert!(path_is(&net.b, a_v6()), "B の経路 {:?}", net.b.path());
    assert!(
        net.delivered[0] > 0 && net.delivered[1] > 0,
        "{:?}",
        net.delivered
    );
    let before = net.delivered;
    for _ in 0..1_000 {
        net.step();
    }
    assert_eq!(
        net.delivered[0], before[0],
        "繋がった後も IPv4 へ送っている"
    );
    assert!(
        net.delivered[1] > before[1],
        "繋がった後に IPv6 で何も届かない"
    );
}

#[test]
fn an_unlisted_ipv6_source_with_a_good_tag_is_learned_and_a_mapped_one_is_not() {
    let moved = sa("[2400:cb00::b]:42099");
    let mut a = link(&[b_v6()], 1);
    let mut q = Q::default();
    a.poll(1, &mut q);
    q.0.clear();

    let r = a.on_datagram(os_v6(b_v6()), &enc(&hello(2)), 2, &mut q);
    assert_eq!(r, Recv::Nothing);
    let c = a.recv_counts();
    assert_eq!(
        (c.learned, c.foreign),
        (0, 0),
        "候補を学んだ・捨てた: {c:?}"
    );
    q.0.clear();

    let mapped = SocketAddr::new(
        IpAddr::V6("::ffff:203.0.113.9".parse::<Ipv6Addr>().unwrap()),
        7005,
    );
    a.on_datagram(mapped, &enc(&hello(2)), 3, &mut q);
    let c = a.recv_counts();
    assert_eq!((c.learned, c.foreign), (0, 1), "{c:?}");
    assert!(q.0.is_empty(), "学ばなかった送り元へ返した");

    a.on_datagram(os_v6(moved), &enc(&hello(2)), 4, &mut q);
    let c = a.recv_counts();
    assert_eq!((c.learned, c.foreign), (1, 1), "{c:?}");
    let replies: Vec<SocketAddr> =
        q.0.iter()
            .filter(|(_, b)| {
                matches!(
                    wire::decode(&key(), None, b).map(|p| p.msg),
                    Ok(Message::HelloAck { .. })
                )
            })
            .map(|(to, _)| *to)
            .collect();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert!(
        replies.iter().all(|r| same_endpoint(*r, moved)),
        "{replies:?}"
    );
    a.on_datagram(os_v6(moved), &enc(&hello(2)), 5, &mut q);
    assert_eq!(a.recv_counts().learned, 1);
    q.0.clear();
    for t in 6..=60 {
        a.poll(t, &mut q);
    }
    let to: Vec<SocketAddr> =
        q.0.iter()
            .filter(|(_, b)| {
                matches!(
                    wire::decode(&key(), None, b).map(|p| p.msg),
                    Ok(Message::Hello(_))
                )
            })
            .map(|(to, _)| *to)
            .collect();
    assert!(to.iter().any(|d| same_endpoint(*d, b_v6())), "{to:?}");
    assert!(
        to.iter().any(|d| same_endpoint(*d, moved)),
        "学んだ IPv6 のアドレスへ送らない: {to:?}"
    );
    assert!(a.hello_sent_learned() > 0);
}
