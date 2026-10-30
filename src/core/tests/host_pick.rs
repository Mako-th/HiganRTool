//! ホストの決め方（`host_pick.rs`）の検査。2つの`Link`を偽のネットワークで接続して見る。公開のAPIだけを使う。
//!
//! ネットワークはµsの模型。送り手ごとの上りの太さで1通ずつ順に送り出す（前の便が出終わるまで次は出ない）。届くまでには片道の遅れを足す。
//! 台Aはrole_nonceが大きい（取りまとめ役）。台Bは小さい。どちらがホストになるかが台の番号と揃わない筋（Bがホスト）を必ず含める。

use std::net::SocketAddr;

use higanr_core::auth::Secret;
use higanr_core::handshake::{self, Established, Name};
use higanr_core::host_pick::{self, Basis, Setup};
use higanr_core::link::{Config, Link, Outlet};
use higanr_core::lockstep::Role;
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::seal::KeySeed;
use higanr_core::wire::{Mode, Seat};

const SID: u32 = 0x4B5A_0021;
const SECRET: Secret = Secret([0x2A; 16]);
const NONCE_A: u64 = 0xF000_0000_0000_0007;
const NONCE_B: u64 = 0x0000_0000_0000_0003;

fn addr(i: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 42_000 + i as u16))
}

fn open() -> NatMark {
    NatMark {
        symmetry: Symmetry::No,
        upnp: false,
    }
}

fn sym() -> NatMark {
    NatMark {
        symmetry: Symmetry::Yes,
        upnp: false,
    }
}

const UNKNOWN: NatMark = NatMark::UNKNOWN;

/// 便の種類（落とし方を決めるため。読めない便は`Other`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Probe,
    Report,
    Agree,
    Other,
}

/// 台の設定。`up`は上りの太さ（B/s）。
#[derive(Clone, Copy)]
struct Side {
    up: u64,
    pick: Option<Setup>,
    /// rollbackを望むときのD（Noneならdelay方式）。
    rb: Option<u8>,
    latency: u8,
    /// ホストになったときに座るサイド（先にいたかを名乗らない経路はホストの希望が通る）。
    seat: Seat,
}

fn side(up: u64, nat: NatMark) -> Side {
    Side {
        up,
        pick: Some(Setup::new(nat)),
        rb: None,
        latency: 2,
        seat: Seat::P1,
    }
}

struct Net {
    /// (届くµs, 通し, 宛先の台, バイト列)。
    q: Vec<(u64, u64, usize, Vec<u8>)>,
    seq: u64,
    /// 台ごとの上りが空くµs。
    busy: [u64; 2],
    up: [u64; 2],
    one_way_us: u64,
    /// 便を落とすかどうか（送り手の台, 種類, その種類の何通目）。
    drop: Box<dyn FnMut(usize, Kind, u32) -> bool>,
    nth: [[u32; 4]; 2],
    /// 台ごとの種類ごとの送った数。
    sent: [[u32; 4]; 2],
}

struct Out<'a> {
    net: &'a mut Net,
    from: usize,
    now_us: u64,
}

fn kind_of(bytes: &[u8]) -> Kind {
    match bytes.get(5).copied() {
        Some(22) => Kind::Probe,
        Some(23) => Kind::Report,
        Some(10) => Kind::Agree,
        _ => Kind::Other,
    }
}

impl Outlet for Out<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let dest = usize::from(to == addr(1));
        assert_ne!(dest, self.from, "自分へ送った");
        let n = &mut self.net;
        let kind = kind_of(bytes);
        let nth = n.nth[self.from][kind as usize];
        n.nth[self.from][kind as usize] += 1;
        n.sent[self.from][kind as usize] += 1;
        // 上りは便を順に送り出す（落ちる便も上りは使う）。
        let start = n.busy[self.from].max(self.now_us);
        let done = start + bytes.len() as u64 * 1_000_000 / n.up[self.from];
        n.busy[self.from] = done;
        if (n.drop)(self.from, kind, nth) {
            return;
        }
        n.q.push((done + n.one_way_us, n.seq, dest, bytes.to_vec()));
        n.seq += 1;
    }
}

fn make_link(i: usize, s: Side, start: u32) -> Link {
    let nonce = if i == 0 { NONCE_A } else { NONCE_B };
    let hs = handshake::Config {
        session_id: SID,
        role_nonce: nonce,
        latency: s.latency,
        name: Name::new(if i == 0 { "A" } else { "B" }),
        host_seat: s.seat,
    };
    let mut l = Link::new(
        Config::single(hs, SECRET, addr(1 - i), KeySeed([0x40 + i as u8; 32])),
        start,
    );
    if let Some(d) = s.rb {
        l.set_mode(handshake::ModeWish::rollback(d), 3);
    }
    l.set_host_pick(s.pick);
    l
}

struct Run {
    est: [Established; 2],
    /// 台ごとの接続したms。
    at: [u32; 2],
    sent: [[u32; 4]; 2],
}

/// 1 msごとに: 届いた便を宛先の台へ渡す（受けた時刻はµs）→ 両台の`poll`。両台が接続したら止める。
fn run(sides: [Side; 2], one_way_ms: u32, drop: Box<dyn FnMut(usize, Kind, u32) -> bool>) -> Run {
    run_from(sides, one_way_ms, drop, [0, 0])
}

/// 台ごとに始めるmsを決められる形の`run`。始める前に届いた便は落ちる（ポートがまだ開いていない）。
fn run_from(
    sides: [Side; 2],
    one_way_ms: u32,
    drop: Box<dyn FnMut(usize, Kind, u32) -> bool>,
    starts: [u32; 2],
) -> Run {
    let mut links = [
        make_link(0, sides[0], starts[0]),
        make_link(1, sides[1], starts[1]),
    ];
    let mut net = Net {
        q: Vec::new(),
        seq: 0,
        busy: [0; 2],
        up: [sides[0].up, sides[1].up],
        one_way_us: u64::from(one_way_ms) * 1_000,
        drop,
        nth: [[0; 4]; 2],
        sent: [[0; 4]; 2],
    };
    let mut at = [None; 2];
    for t in 0..20_000u32 {
        let now_us = u64::from(t) * 1_000;
        let mut due: Vec<(u64, u64, usize, Vec<u8>)> = Vec::new();
        let mut i = 0;
        while i < net.q.len() {
            if net.q[i].0 <= now_us {
                due.push(net.q.swap_remove(i));
            } else {
                i += 1;
            }
        }
        due.sort_by_key(|x| (x.0, x.1));
        for (rx, _, to, bytes) in due {
            if t < starts[to] {
                continue;
            }
            let mut o = Out {
                net: &mut net,
                from: to,
                now_us,
            };
            links[to].on_datagram_rx_us(addr(1 - to), &bytes, t, rx, false, &mut o);
        }
        for (k, l) in links.iter_mut().enumerate() {
            if t < starts[k] {
                continue;
            }
            let mut o = Out {
                net: &mut net,
                from: k,
                now_us,
            };
            l.poll(t, &mut o);
            if at[k].is_none() && l.connected().is_some() {
                at[k] = Some(t);
            }
        }
        if at.iter().all(Option::is_some) {
            break;
        }
    }
    let est = [0, 1].map(|k| {
        links[k]
            .connected()
            .unwrap_or_else(|| panic!("台 {k} が接続しない: {:?}", links[k].handshake_state()))
            .established()
    });
    Run {
        est,
        at: at.map(|a| a.expect("接続した")),
        sent: net.sent,
    }
}

fn keep_all() -> Box<dyn FnMut(usize, Kind, u32) -> bool> {
    Box::new(|_, _, _| false)
}

/// 両台の結果が食い違わないことと、ホストが`host`（台の番号）であることを見る。サイド・delay・D・決め手の材料も両台で同じ。
fn check(r: &Run, host: usize, tag: &str) {
    let [a, b] = r.est;
    assert_eq!(
        r.est[host].role,
        Role::Host,
        "{tag}: 台 {host} がホストでない"
    );
    assert_eq!(r.est[1 - host].role, Role::Guest, "{tag}");
    assert_eq!(a.host_seat, b.host_seat, "{tag}: host_seat");
    assert_ne!(a.seat, b.seat, "{tag}: 席");
    assert_eq!(r.est[host].seat, a.host_seat, "{tag}: ホストの席");
    assert_eq!((a.mode, a.delay), (b.mode, b.delay), "{tag}: 形と delay");
    assert_eq!(a.pick.active, b.pick.active, "{tag}");
    assert_eq!(a.pick.basis, b.pick.basis, "{tag}: 決め手");
    assert_eq!(
        (
            a.pick.own_up,
            a.pick.peer_up,
            a.pick.own_nat,
            a.pick.peer_nat
        ),
        (
            b.pick.peer_up,
            b.pick.own_up,
            b.pick.peer_nat,
            b.pick.own_nat
        ),
        "{tag}: 材料が入れ違いで同じでない"
    );
}

const MBPS: u64 = 125_000;

#[test]
fn the_fatter_side_hosts_even_when_its_nonce_is_smaller() {
    // B（nonceが小さい）が20倍太い。
    let r = run(
        [side(MBPS, open()), side(20 * MBPS, open())],
        15,
        keep_all(),
    );
    check(&r, 1, "B が太い");
    let b = r.est[1].pick;
    assert!(b.active && b.basis == Basis::Uplink, "{b:?}");
    // 見積もりは送り出しの太さ（列の幅 ÷ 番号の差）。模型の到着はµsなので1%の中に入る。
    let near = |got: u32, want: u64| (got as u64).abs_diff(want) * 100 <= want;
    assert!(near(b.own_up, 20 * MBPS), "B の上りの見積もり {}", b.own_up);
    assert!(near(b.peer_up, MBPS), "A の上りの見積もり {}", b.peer_up);
    // 80 Mbpsの列（幅1.08 ms）も測れる。到着をmsで丸めると1 msか2 msになるので1%に入らない。
    let r = run(
        [side(8 * MBPS, open()), side(80 * MBPS, open())],
        15,
        keep_all(),
    );
    check(&r, 1, "B が 80 Mbps");
    assert!(near(r.est[1].pick.own_up, 80 * MBPS), "{:?}", r.est[1].pick);
    // 逆向き: Aが太ければA。
    let r = run(
        [side(20 * MBPS, open()), side(MBPS, open())],
        15,
        keep_all(),
    );
    check(&r, 0, "A が太い");
    assert_eq!(r.est[0].pick.basis, Basis::Uplink);
}

/// 列が1 ms未満で届き切る太さ（約86 Mbpsより上）は上限の値: 太い同士はrole_nonce、太いと細いは太い方。
#[test]
fn lines_above_the_cap_tie_with_each_other_and_beat_thin_ones() {
    let r = run(
        [side(200 * MBPS, open()), side(1000 * MBPS, open())],
        15,
        keep_all(),
    );
    check(&r, 0, "200 Mbps と 1 Gbps");
    let a = r.est[0].pick;
    assert_eq!(
        (a.own_up, a.peer_up, a.basis),
        (host_pick::UP_CAP, host_pick::UP_CAP, Basis::Nonce)
    );
    let r = run(
        [side(MBPS, open()), side(1000 * MBPS, open())],
        15,
        keep_all(),
    );
    check(&r, 1, "1 Mbps と 1 Gbps");
    assert_eq!(r.est[1].pick.own_up, host_pick::UP_CAP);
    assert_eq!(r.est[1].pick.basis, Basis::Uplink);
}

#[test]
fn a_small_difference_keeps_the_nonce() {
    let r = run(
        [side(10 * MBPS, open()), side(11 * MBPS, open())],
        15,
        keep_all(),
    );
    check(&r, 0, "1.1 倍");
    assert_eq!(r.est[0].pick.basis, Basis::Nonce);
    assert!(
        r.est[0].pick.own_up > 0 && r.est[0].pick.peer_up > 0,
        "両方測れている"
    );
}

#[test]
fn every_lost_part_still_gives_one_answer() {
    // (筋, 落とす便, Bが太いので測れればホストはBだが、測れない筋はnonceのA)
    type Drop = fn(usize, Kind, u32) -> bool;
    let cases: [(&str, Drop, usize); 6] = [
        (
            "値の便が全部落ちる",
            |f, k, _| f == 1 && k == Kind::Report,
            0,
        ),
        (
            "値の便の最初の 3 通が落ちる",
            |f, k, n| f == 1 && k == Kind::Report && n < 3,
            1,
        ),
        (
            "A の列が全部落ちる（A の上りは測れない）",
            |f, k, _| f == 0 && k == Kind::Probe,
            0,
        ),
        (
            "B の列が全部落ちる（B の上りは測れない）",
            |f, k, _| f == 1 && k == Kind::Probe,
            0,
        ),
        ("両方の列が全部落ちる", |_, k, _| k == Kind::Probe, 0),
        (
            "両方の列の途中と最後が落ちる",
            |_, k, n| k == Kind::Probe && n % 3 != 1,
            1,
        ),
    ];
    for (tag, d, host) in cases {
        let r = run(
            [side(MBPS, open()), side(20 * MBPS, open())],
            15,
            Box::new(d),
        );
        check(&r, host, tag);
        assert_eq!(r.sent[0][Kind::Probe as usize], 10, "{tag}: A の列");
        assert_eq!(r.sent[1][Kind::Probe as usize], 10, "{tag}: B の列");
    }
}

#[test]
fn a_report_after_the_wait_falls_to_the_nonce() {
    // 片道300 ms: AがBのHELLOを受けてからBの値が届くまで2片道 ＋ 列の幅 > 500 ms。
    let r = run(
        [side(MBPS, open()), side(20 * MBPS, open())],
        300,
        keep_all(),
    );
    check(&r, 0, "待ちが切れる");
    let a = r.est[0].pick;
    assert_eq!((a.own_up, a.basis), (0, Basis::Nonce), "{a:?}");
    assert!(a.peer_up > 0, "A は B の列を測れている");
    // 同じ片道100 msなら間に合い、Bがホスト。
    let r = run(
        [side(MBPS, open()), side(20 * MBPS, open())],
        100,
        keep_all(),
    );
    check(&r, 1, "片道 100 ms");
}

#[test]
fn nat_kinds_come_before_the_uplink() {
    // (Aの型, Bの型, Aの太さ, Bの太さ, ホスト, 決め手)
    let cases = [
        (sym(), open(), 20 * MBPS, MBPS, 1, Basis::Nat),
        (open(), sym(), MBPS, 20 * MBPS, 0, Basis::Nat),
        (sym(), sym(), MBPS, 20 * MBPS, 1, Basis::Uplink),
        (UNKNOWN, sym(), MBPS, 20 * MBPS, 1, Basis::Uplink),
        (open(), UNKNOWN, 20 * MBPS, MBPS, 0, Basis::Uplink),
        // シンメトリックでもUPnPで開けていれば届く型。
        (
            NatMark {
                symmetry: Symmetry::Yes,
                upnp: true,
            },
            sym(),
            MBPS,
            20 * MBPS,
            0,
            Basis::Nat,
        ),
    ];
    for (na, nb, ua, ub, host, basis) in cases {
        let tag = format!("{na:?} / {nb:?}");
        let r = run([side(ua, na), side(ub, nb)], 15, keep_all());
        check(&r, host, &tag);
        assert_eq!(r.est[0].pick.basis, basis, "{tag}");
        assert_eq!(
            (r.est[0].pick.own_nat, r.est[0].pick.peer_nat),
            (na, nb),
            "{tag}"
        );
    }
}

#[test]
fn the_decided_host_brings_its_own_seat_and_rollback_d() {
    // 両者が1Pを望み（名乗らない経路はホストの希望が通る）、rollbackのDはA 2・B 5。
    let mut a = side(MBPS, open());
    let mut b = side(20 * MBPS, open());
    a.rb = Some(2);
    b.rb = Some(5);
    let r = run([a, b], 15, keep_all());
    check(&r, 1, "B がホスト");
    assert_eq!(r.est[1].seat, Seat::P1);
    assert_eq!((r.est[0].mode, r.est[0].delay), (Mode::Rollback, 5));
    // AがホストならAの値。
    let (a2, b2) = (Side { up: 20 * MBPS, ..a }, Side { up: MBPS, ..b });
    let r = run([a2, b2], 15, keep_all());
    check(&r, 0, "A がホスト");
    assert_eq!(r.est[0].seat, Seat::P1);
    assert_eq!((r.est[1].mode, r.est[1].delay), (Mode::Rollback, 2));
    // 希望が違う（Aは2P、Bは1P）: host_seatは決まったホストの希望。
    for (ua, ub, host, want) in [
        (MBPS, 20 * MBPS, 1, Seat::P1),
        (20 * MBPS, MBPS, 0, Seat::P2),
    ] {
        let (mut a4, mut b4) = (side(ua, open()), side(ub, open()));
        a4.seat = Seat::P2;
        b4.seat = Seat::P1;
        let r = run([a4, b4], 15, keep_all());
        check(&r, host, "希望が違う");
        assert_eq!(r.est[1 - host].host_seat, want);
        assert_eq!((r.est[0].seat, r.est[1].seat), (Seat::P2, Seat::P1));
    }
    // delay方式は両者の希望の大きい方（ホストに依らない）。
    let (mut a3, mut b3) = (side(MBPS, open()), side(20 * MBPS, open()));
    a3.latency = 6;
    b3.latency = 1;
    let r = run([a3, b3], 15, keep_all());
    check(&r, 1, "delay 方式");
    assert_eq!((r.est[0].mode, r.est[0].delay), (Mode::Delay, 6));
}

#[test]
fn without_both_sides_no_probe_is_sent_and_no_wait_is_added() {
    let mut off = side(20 * MBPS, open());
    off.pick = None;
    let base = run([side(MBPS, open()), off], 15, keep_all());
    check(&base, 0, "B が使わない");
    assert!(!base.est[0].pick.active);
    assert_eq!(base.sent[0][Kind::Probe as usize], 0);
    assert_eq!(base.sent[1][Kind::Probe as usize], 0);
    assert_eq!(base.sent[1][Kind::Report as usize], 0);
    let mut off_a = side(MBPS, open());
    off_a.pick = None;
    let both_off = run([off_a, off], 15, keep_all());
    assert_eq!(base.at, both_off.at, "片方だけ使うときに待ちが増えた");
}

#[test]
fn random_losses_never_split_the_roles() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut hosts = [0u32; 2];
    for round in 0..60 {
        let s = next();
        let ups = [MBPS * (1 + s % 30), MBPS * (1 + (s >> 8) % 30)];
        let nats = [
            [open(), sym(), UNKNOWN][(s >> 16) as usize % 3],
            [open(), sym(), UNKNOWN][(s >> 20) as usize % 3],
        ];
        let one_way = 5 + (s >> 24) as u32 % 200;
        let mut rng = next();
        let drop: Box<dyn FnMut(usize, Kind, u32) -> bool> = Box::new(move |_, _, _| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % 4 == 0
        });
        let r = run(
            [side(ups[0], nats[0]), side(ups[1], nats[1])],
            one_way,
            drop,
        );
        let host = usize::from(r.est[1].role == Role::Host);
        check(&r, host, &format!("回 {round}"));
        hosts[host] += 1;
    }
    // 効く期間: 60回のうち両方の台がホストになった回がある（台の番号で決まっていない）。
    assert!(hosts[0] > 0 && hosts[1] > 0, "{hosts:?}");
}

/// 接続までの時間の増分（模型）。決め方を使わない場合と比べる。
/// 近い相手（片道15 ms）と遠い相手（片道150 ms）のそれぞれで、同時に始める場合とBが1片道遅れて始める場合を出す。
/// 細い側は1 Mbpsか0.4 Mbps、太い側は8 Mbps。
#[test]
fn connect_time_increase_in_the_model() {
    const KBPS400: u64 = 50_000;
    let mut rows: Vec<(u32, u64, u64, bool)> = vec![
        (15, 8 * MBPS, MBPS, false),
        (15, 80 * MBPS, 80 * MBPS, false),
        (150, 8 * MBPS, MBPS, false),
    ];
    for one_way in [15, 150] {
        for thin in [MBPS, KBPS400] {
            rows.push((one_way, thin, 8 * MBPS, true));
            rows.push((one_way, 8 * MBPS, thin, true));
        }
    }
    for (one_way, ua, ub, late) in rows {
        let starts = [0, if late { one_way } else { 0 }];
        let on = run_from(
            [side(ua, open()), side(ub, open())],
            one_way,
            keep_all(),
            starts,
        );
        let (mut a, mut b) = (side(ua, open()), side(ub, open()));
        a.pick = None;
        b.pick = None;
        let off = run_from([a, b], one_way, keep_all(), starts);
        let inc: Vec<i64> = (0..2)
            .map(|k| i64::from(on.at[k]) - i64::from(off.at[k]))
            .collect();
        eprintln!(
            "片道 {one_way} ms・上り A {:.1} / B {:.1} Mbps・B が遅れて始める {late}: 接続 {:?} ms（使わない {:?} ms）。増分 {inc:?} ms",
            ua as f64 * 8.0 / 1e6,
            ub as f64 * 8.0 / 1e6,
            on.at,
            off.at
        );
        // 増える待ちは取りまとめ役が値を待つ上限（500 ms）とAGREEの片道を越えない。
        assert!(
            inc.iter().all(|&d| {
                d >= 0 && d <= i64::from(host_pick::REPORT_WAIT_MS) + i64::from(one_way)
            }),
            "{inc:?}"
        );
    }
}
