//! 観戦のホストの検査（本物の UDP・受信スレッド・リングバッファ。観戦者は core の `Watcher` を別のスレッドで走らせる）。
use super::*;
use crate::net_udp;
use crate::netplay::Net;
use crate::spectate_testkit::{Behavior, Observer};
use higanr_core::auth::AuthKey;
use higanr_core::candidate::Candidates;
use higanr_core::wire::{self, MAX_DATAGRAM, Message, Seat, Select, Step, StepKind};
use std::time::{Duration, Instant};

pub(super) const MATCH_SESSION: u32 = 0x5EED_0001;
const MATCH_SECRET: Secret = Secret([0x42; 16]);

fn lo(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn settings(peer: SocketAddr, spectate: bool) -> NetSettings {
    let extra = if spectate {
        "[spectate]\nenable = 1\n"
    } else {
        ""
    };
    crate::net_config::from_text(&format!(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = {peer}\nsession = {MATCH_SESSION}\n{extra}"
    ))
    .unwrap()
    .unwrap()
}

fn sel(seed: u16) -> Select {
    Select {
        match_no: 3,
        chara: 4,
        color: 1,
        life: 10,
        charge: 2,
        map: 5,
        difficulty: 1,
        cpu_1p: 0,
        cpu_2p: 0,
        background: 0,
        rng_seed: seed,
        option_digest: 0xABCD,
    }
}

/// ゲームのスレッドが書く記録の並び（seg 0 の選択 → マッチ → seg 1 のマッチ → 終わり）。`frames` は seg 0 のマッチの長さ。
fn script(frames: u32) -> Vec<Record> {
    let mut v = vec![
        Record::Seg { seg: 0 },
        Record::Menu { p1: 0, p2: 0 },
        Record::Step(Step {
            step: 0,
            kind: StepKind::Decide,
            value: 1,
            screen: 7,
        }),
        Record::Menu { p1: 0x11, p2: 0x22 },
        Record::Select {
            seat: Seat::P1,
            sel: sel(9),
        },
        Record::Select {
            seat: Seat::P2,
            sel: sel(9),
        },
    ];
    v.extend((0..frames).map(|n| Record::Frame {
        frame: n,
        p1: (n as u16) & 0xFF,
        p2: (n as u16).wrapping_mul(3) & 0xFF,
    }));
    v.push(Record::Seg { seg: 1 });
    v.extend((0..30).map(|n| Record::Frame {
        frame: n,
        p1: 1,
        p2: 2,
    }));
    v.push(Record::End(ByeReason::Finished));
    v
}

/// 受信スレッドが観戦のホストを取るまで待つ（置いた直後に届いた JOIN は対戦のセッションへ回るので、黙る観戦者が入れなくなる）。
fn wait_active(rx: &net_udp::RecvThread) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rx.spectate_active() {
        assert!(
            Instant::now() < deadline,
            "受信スレッドが観戦のホストを取らない"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_joined(rx: &net_udp::RecvThread, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while rx.spectate_snapshot().map_or(0, |s| s.host.joined) < n {
        assert!(
            Instant::now() < deadline,
            "観戦者が {n} 人入らない: {:?}",
            rx.spectate_snapshot()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("higanr_spectate_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// 対戦の相手（この検査のソケット）が送る、対戦の session・鍵の PING。受信スレッドは session で経路を分けるだけで中を開かないので、封じる鍵は検査の値でよい。
pub(super) fn match_ping(seq: u32) -> Vec<u8> {
    let _ = AuthKey::derive(&MATCH_SECRET, MATCH_SESSION);
    let dir = higanr_core::seal::DirKeys::from_raw([0x4D; 32], [0x4D; 32]);
    let mut b = [0u8; MAX_DATAGRAM];
    let n = wire::seal(
        &dir.sealer(higanr_core::seal::Sender::Link),
        &higanr_core::seal::Seq::new(),
        MATCH_SESSION,
        &Message::Ping { send_time: seq },
        &mut b,
    )
    .expect("封じられる");
    b[..n].to_vec()
}

#[test]
fn every_record_the_game_wrote_reaches_a_watcher_in_order_and_match_packets_still_pass() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer_addr, true), me).unwrap();
    assert_ne!(p.sender.session_id(), MATCH_SESSION);
    let ring = Arc::clone(&p.ring);
    let spectate_session = p.sender.session_id();
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    // 受け待ちの上限を短くしない（1 ms にすると、対戦の相手のパケットが 20 走行に 1 走行ほど落ちた。`net_udp::RECV_POLL`）。
    assert_eq!(
        net.recv_timeout(),
        Some(net_udp::RECV_POLL),
        "観戦のホストを置いたら受け待ちの上限が変わった"
    );
    assert!(
        !rx.install_spectate(prepare(&settings(peer_addr, true), me).unwrap().sender),
        "2 つ目を受け付けた"
    );
    let obs = Observer::spawn(&p.code_text, Behavior::Normal, 7);
    // 観戦者が入ってから書く（遅れて入った観戦者は、いまの seg の始まりから流れる）。
    wait_joined(&rx, 1);
    let want = script(500);
    for r in &want {
        assert!(ring.push(*r), "{r:?}");
    }
    for i in 0..3 {
        peer.send_to(&match_ping(i), me).unwrap();
    }
    assert!(
        obs.wait_end(Duration::from_secs(10)),
        "観戦者が終わりまで受けない"
    );
    let seen = obs.finish();
    assert!(seen.joined);
    assert_eq!(
        seen.recs.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        (0..want.len() as u32).collect::<Vec<_>>(),
        "通し番号が連続でない"
    );
    assert_eq!(
        seen.recs.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
        want,
        "観戦者が受けた列が書いた列と違う"
    );
    let mut buf = [0u8; MAX_DATAGRAM];
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got = Vec::new();
    while got.len() < 3 && Instant::now() < deadline {
        while let Some((from, n, _)) = net.recv(&mut buf) {
            got.push((from, wire::peek_session(&buf[..n])));
        }
        net.wait(2_000);
    }
    while let Some((from, n, _)) = net.recv(&mut buf) {
        got.push((from, wire::peek_session(&buf[..n])));
    }
    assert_eq!(
        got,
        vec![(peer_addr, Some(MATCH_SESSION)); 3],
        "ゲームのリングに届いたもの（観戦のものが混ざる・対戦のものが欠ける）"
    );
    assert_ne!(spectate_session, MATCH_SESSION);
    let snap = rx.spectate_snapshot().expect("集計がある");
    assert_eq!(snap.host.joined, 1, "{snap:?}");
    assert_eq!(snap.host.pushed as usize, want.len(), "{snap:?}");
    assert!(snap.host.records_sent as usize >= want.len(), "{snap:?}");
    assert_eq!((snap.ring.gaps, snap.ring.dropped_full), (0, 0));
    let text = snap.text();
    assert!(
        text.contains("観戦者 1 人") && text.contains("入った 1"),
        "{text}"
    );
}

#[test]
fn without_a_spectate_host_a_watchers_packets_are_not_answered() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer_addr, true), me).unwrap();
    drop(p.sender);
    let obs = Observer::spawn(&p.code_text, Behavior::Normal, 7);
    std::thread::sleep(Duration::from_millis(600));
    let seen = obs.finish();
    assert_eq!(seen.datagrams, 0, "観戦のホストが無いのに何かが返った");
    assert!(!seen.joined);
    assert!(
        seen.counts.joins_sent >= 1,
        "観戦者が JOIN を送っていない: {:?}",
        seen.counts
    );
    assert_eq!(net.send_counts(), (0, 0));
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut n_rx = 0;
    while net.recv(&mut buf).is_some() {
        n_rx += 1;
    }
    assert!(n_rx >= 1, "観戦者の JOIN がゲームのリングへ届かない");
    assert!(rx.spectate_snapshot().is_none());
}

#[test]
fn silent_noack_and_vanishing_watchers_do_not_stop_the_receive_thread() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer_addr, true), me).unwrap();
    let ring = Arc::clone(&p.ring);
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    let observers = [
        Observer::spawn(&p.code_text, Behavior::Silent, 1),
        Observer::spawn(&p.code_text, Behavior::NoAck, 2),
        Observer::spawn(&p.code_text, Behavior::Vanish(10), 3),
        Observer::spawn(&p.code_text, Behavior::Normal, 4),
    ];
    wait_joined(&rx, 4);
    let want = script(400);
    for r in &want {
        assert!(ring.push(*r));
    }
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut received = 0u32;
    for i in 0..200 {
        peer.send_to(&match_ping(i), me).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        while net.recv(&mut buf).is_some() {
            received += 1;
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    while net.recv(&mut buf).is_some() {
        received += 1;
    }
    assert_eq!(
        received,
        200,
        "対戦のパケットが欠けた: {:?} {:?}",
        rx.stats(),
        rx.spectate_snapshot()
    );
    let mut normal = None;
    for o in observers {
        match o.behavior {
            Behavior::Normal => {
                assert!(o.wait_end(Duration::from_secs(10)));
                normal = Some(o.finish());
            }
            Behavior::NoAck => {
                let s = o.finish();
                assert!(
                    (1..=crate::spectate_testkit::NOACK_WINDOW).contains(&s.recs.len()),
                    "ACK を返さないのに送りのウィンドウを越えて届いた: {}",
                    s.recs.len()
                );
            }
            Behavior::Silent => {
                let s = o.finish();
                assert_eq!(s.datagrams, 0);
            }
            Behavior::Vanish(n) => {
                let s = o.finish();
                assert!(s.recs.len() >= n);
            }
        }
    }
    let normal = normal.unwrap();
    assert_eq!(normal.recs.len(), want.len(), "普通の観戦者が全部受けない");
    let snap = rx.spectate_snapshot().unwrap();
    assert!(
        snap.host.resends > 0,
        "黙った観戦者への送り直しが無い: {snap:?}"
    );
}

#[test]
fn the_spectate_session_never_equals_the_match_session() {
    let s = settings(lo(9), true);
    let mut draws = [MATCH_SESSION, MATCH_SESSION, 0x1234_5678].into_iter();
    let p = prepare_with(
        &s,
        lo(47001),
        1_000,
        || Ok(draws.next().unwrap()),
        || Ok(Secret([7; 16])),
    )
    .unwrap();
    assert_eq!(p.sender.session_id(), 0x1234_5678);
    let never = prepare_with(
        &s,
        lo(47001),
        1_000,
        || Ok(MATCH_SESSION),
        || Ok(Secret([7; 16])),
    );
    assert!(never.is_err());
}

#[test]
fn two_hosts_with_the_same_secret_and_session_get_different_keys() {
    let s = settings(lo(9), true);
    let one = || {
        prepare_with(
            &s,
            lo(47001),
            1_000,
            || Ok(0x1234_5678),
            || Ok(Secret([7; 16])),
        )
        .unwrap()
    };
    let (a, b) = (one(), one());
    assert_eq!(a.sender.session_id(), b.sender.session_id());
    assert_ne!(
        a.code_text, b.code_text,
        "秘密と session が同じホストの観戦用のコードが同じ（鍵の種が固定）"
    );
    let public_of = |text: &str| {
        higanr_core::code::read_spectate(text, 1_000)
            .expect("観戦用のコードが読める")
            .host_public
    };
    let (pa, pb) = (public_of(&a.code_text), public_of(&b.code_text));
    assert_ne!(pa, pb, "公開鍵が同じ");
    assert_ne!(pa, [0u8; 32], "公開鍵が空");
}

#[test]
fn arm_and_commit_write_a_spectate_code_only_when_enabled() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let link_cfg = higanr_core::link::Config {
        handshake: higanr_core::handshake::Config {
            session_id: MATCH_SESSION,
            role_nonce: 5,
            latency: 2,
            name: Name::new("m"),
            host_seat: Seat::P1,
        },
        secret: MATCH_SECRET,
        peers: Candidates::from_addrs(&[peer_addr]).unwrap(),
        key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
    };
    let mut np = NetPlay::new(link_cfg, crate::netplay::Config::default(), &mut net);
    let off = temp_dir("off");
    let (pending, note) = arm(
        &settings(peer_addr, false),
        Some(off.clone()),
        Some(me),
        &mut np,
    );
    assert!(pending.is_none() && note.is_empty(), "{note}");
    assert!(!off.exists());
    let dir = temp_dir("on");
    let (pending, note) = arm(
        &settings(peer_addr, true),
        Some(dir.clone()),
        Some(me),
        &mut np,
    );
    assert!(note.is_empty(), "{note}");
    let line = pending.expect("用意できる").commit(&rx);
    assert!(line.contains(SPECTATE_CODE_FILE), "{line}");
    let text = std::fs::read_to_string(dir.join(SPECTATE_CODE_FILE)).unwrap();
    let now = crate::net_udp::now_unix();
    let sc = code::read_spectate(text.trim(), now).expect("観戦用のコードとして読める");
    assert_ne!(sc.host_public, [0; 32]);
    let c = sc.code;
    assert_ne!(c.session_id, MATCH_SESSION);
    assert_ne!(c.secret, Some(MATCH_SECRET));
    assert_eq!(c.candidates.as_slice().first().map(|x| x.addr), Some(me));
    assert!(
        code::read_initiator(text.trim(), now).is_err(),
        "対戦のコードとして読めた"
    );
    assert!(rx.spectate_snapshot().is_some());
    write_code_file(&dir, "OLD").unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join(SPECTATE_CODE_FILE)).unwrap(),
        "OLD\r\n"
    );
    let (pending, note) = arm(&settings(peer_addr, true), None, Some(me), &mut np);
    assert!(
        pending.is_none() && note.contains("観戦は使わない"),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tick` を検査が直に呼ぶ（受信スレッドの周回を待たない）ので、リングバッファの溢れの形が決まる。
#[test]
fn an_overflowed_ring_tells_the_host_the_stream_was_cut_before_the_new_seg() {
    let sock = UdpSocket::bind(lo(0)).unwrap();
    let ring = Arc::new(SpectateRing::with_len(4));
    let mut sender = SpectateSender::new(
        HostConfig {
            session_id: 5,
            secret: Secret([1; 16]),
            key_seed: higanr_core::seal::KeySeed([0x48; 32]),
        },
        Arc::clone(&ring),
        Scope::Loopback,
        Arc::new(Shared::default()),
    );
    let f = |n: u32| Record::Frame {
        frame: n,
        p1: 1,
        p2: 2,
    };
    assert!(ring.push(Record::Seg { seg: 0 }));
    for n in 0..3 {
        assert!(ring.push(f(n)));
    }
    assert!(!ring.push(f(3)), "リングバッファが溢れない");
    sender.tick(&sock, 0);
    assert_eq!(sender.host.tail(), 4, "溢れる前の 4 記録");
    assert!(!ring.push(f(4)), "溢れの後の Seg でない記録を入れた");
    assert!(ring.push(Record::Seg { seg: 1 }));
    assert!(ring.push(f(0)));
    sender.tick(&sock, 1);
    assert_eq!(
        (sender.host.tail(), sender.host.seg_start()),
        (7, 5),
        "End(Cancelled) が Seg の前に入っていない"
    );
}

/// パニックは検査用のフック（`panic_on_tick`）で観戦のホストの最初の周回に起こす。
#[test]
fn a_panic_in_the_spectate_host_stops_only_the_spectating() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let mut p = prepare(&settings(peer_addr, true), me).unwrap();
    p.sender.panic_on_tick = true;
    assert!(rx.install_spectate(p.sender));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rx.spectate_snapshot().is_some_and(|s| s.stopped) {
        assert!(
            Instant::now() < deadline,
            "パニックが集計に出ない（受信スレッドが止まったか、包んでいない）"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    for i in 0..20 {
        peer.send_to(&match_ping(i), me).unwrap();
    }
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut received = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while received < 20 && Instant::now() < deadline {
        while net.recv(&mut buf).is_some() {
            received += 1;
        }
        net.wait(2_000);
    }
    assert_eq!(received, 20, "パニックの後に対戦のパケットが欠けた");
    let text = rx.spectate_snapshot().unwrap().text();
    assert!(text.contains("パニックで観戦を止めた"), "{text}");
}

/// 数えを 3 通りに置く（自分で抜けた 2・黙って外した 5・リングバッファの溢れ 7）→ 語と数が対になって出る。
#[test]
fn the_snapshot_line_tells_leaving_by_bye_from_being_dropped_for_silence() {
    let mut s = Snapshot::default();
    s.host.left = 2;
    s.host.silenced = 5;
    s.host.overflows = 7;
    let text = s.text();
    assert!(
        text.contains("自分で抜けた 2 / 黙って外した 5 / リングバッファの溢れで全員外した 7"),
        "{text}"
    );
}

// ---------------- 穴あけ ----------------
//
// `SpectateSender::tick` / `on_datagram` を検査が直に呼ぶので、時刻 `now` を自分で決められる。宛先は本物の UDP（loopback）のソケット。

use higanr_core::spectate::{
    PUNCH_INTERVAL_MS, PUNCH_PACKET, PUNCH_ROUNDS, Watcher, WatcherConfig,
};

const SPEC_SESSION: u32 = 0x5EED_0A01;
const SPEC_SECRET: Secret = Secret([0x33; 16]);

fn punch_sender(scope: Scope) -> (SpectateSender, UdpSocket, Arc<Shared>) {
    let sock = UdpSocket::bind(lo(0)).unwrap();
    let shared = Arc::new(Shared::default());
    let sender = SpectateSender::new(
        HostConfig {
            session_id: SPEC_SESSION,
            secret: SPEC_SECRET,
            key_seed: higanr_core::seal::KeySeed([0x48; 32]),
        },
        Arc::new(SpectateRing::with_len(64)),
        scope,
        Arc::clone(&shared),
    );
    (sender, sock, shared)
}

fn punch_target() -> UdpSocket {
    let s = UdpSocket::bind(lo(0)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    s
}

/// ソケットに届いているものを、来なくなるまで数える。戻りは（穴あけの印 HGRp、それ以外）。
fn drain_seen(s: &UdpSocket) -> (usize, usize) {
    let (mut punches, mut others) = (0, 0);
    let mut buf = [0u8; 2048];
    while let Ok((len, _)) = s.recv_from(&mut buf) {
        if buf[..len] == PUNCH_PACKET {
            punches += 1;
        } else {
            others += 1;
        }
    }
    (punches, others)
}

/// ソケットに届いている穴あけの印の数（それ以外が混ざっていたら落ちる）。
fn punches_seen(s: &UdpSocket) -> usize {
    let (p, o) = drain_seen(s);
    assert_eq!(o, 0, "穴あけの印以外が届いた");
    p
}

fn cands_of(addrs: &[SocketAddr]) -> Candidates {
    Candidates::from_addrs(addrs).unwrap()
}

/// 観戦用のコード（`SPEC_SESSION`・`SPEC_SECRET`）で作った JOIN（`secret` を替えるとタグが合わない）。
fn join_bytes(secret: Secret, nonce: u64) -> Vec<u8> {
    struct Collect(Vec<Vec<u8>>);
    impl Outlet for Collect {
        fn send(&mut self, _to: SocketAddr, bytes: &[u8]) {
            self.0.push(bytes.to_vec());
        }
    }
    let mut w = Watcher::new(WatcherConfig {
        session_id: SPEC_SESSION,
        secret,
        nonce,
        name: Name::new("viewer"),
        host: cands_of(&[lo(9)]),
        host_public: higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed(
            [0x48; 32],
        ))
        .public(),
        key_seed: higanr_core::seal::KeySeed([nonce as u8; 32]),
    });
    let mut out = Collect(Vec::new());
    w.poll(0, &mut out);
    assert_eq!(out.0.len(), 1, "JOIN が 1 通出る");
    out.0.remove(0)
}

#[test]
fn a_punch_request_makes_the_receive_thread_send_five_rounds_to_every_candidate() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let (t1, t2) = (punch_target(), punch_target());
    sender.tick(&sock, 500);
    assert_eq!((punches_seen(&t1), punches_seen(&t2)), (0, 0));
    shared.request_punch(cands_of(&[
        t1.local_addr().unwrap(),
        t2.local_addr().unwrap(),
    ]));
    sender.tick(&sock, 1_000);
    assert_eq!((punches_seen(&t1), punches_seen(&t2)), (1, 1), "最初の回");
    sender.tick(&sock, 1_000 + PUNCH_INTERVAL_MS - 1);
    assert_eq!((punches_seen(&t1), punches_seen(&t2)), (0, 0), "間隔の内");
    for k in 1..u32::from(PUNCH_ROUNDS) {
        sender.tick(&sock, 1_000 + k * PUNCH_INTERVAL_MS);
        assert_eq!((punches_seen(&t1), punches_seen(&t2)), (1, 1), "{k} 回目");
    }
    for k in 5..9 {
        sender.tick(&sock, 1_000 + k * PUNCH_INTERVAL_MS);
    }
    assert_eq!((punches_seen(&t1), punches_seen(&t2)), (0, 0), "5 回の後");
    let p = sender.punch_counts;
    assert_eq!(
        (p.started, p.sent, p.stopped),
        (1, 2 * u32::from(PUNCH_ROUNDS), 0)
    );
}

#[test]
fn the_viewers_join_stops_only_its_own_punches_and_a_forged_join_stops_nothing() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let (t1, t2, t3) = (punch_target(), punch_target(), punch_target());
    let (a1, a2, a3) = (
        t1.local_addr().unwrap(),
        t2.local_addr().unwrap(),
        t3.local_addr().unwrap(),
    );
    shared.request_punch(cands_of(&[a1, a2, a3]));
    sender.tick(&sock, 0);
    sender.on_datagram(&sock, a1, &join_bytes(SPEC_SECRET, 1), 10);
    sender.on_datagram(&sock, a2, &join_bytes(Secret([0x99; 16]), 2), 10);
    sender.on_datagram(&sock, a1, &join_bytes(SPEC_SECRET, 1), 20);
    for k in 1..9 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    let (p1, others1) = drain_seen(&t1);
    assert_eq!(p1, 1, "JOIN が来たアドレスは最初の 1 通で止まる");
    assert!(
        others1 >= 1,
        "正しい JOIN には観戦のホストが返す（入れた印。これが無いと「止まった」が「JOIN が通らなかった」と区別できない）"
    );
    assert_eq!(
        punches_seen(&t2),
        5,
        "偽の JOIN では止まらない（5 回送り切る）"
    );
    assert_eq!(punches_seen(&t3), 5, "別のアドレスの JOIN では止まらない");
    let p = sender.punch_counts;
    assert_eq!(p.stopped, 1, "止めた回数: {p:?}");
    assert_eq!(
        shared.get().host.joined,
        1,
        "入ったのは正しい JOIN の 1 人だけ"
    );
}

#[test]
fn punches_to_a_destination_outside_the_scope_are_refused_at_the_door() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let near = punch_target();
    let far: SocketAddr = "203.0.113.9:4000".parse().unwrap();
    shared.request_punch(cands_of(&[near.local_addr().unwrap(), far]));
    for k in 0..u32::from(PUNCH_ROUNDS) {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    assert_eq!(punches_seen(&near), 5, "範囲の中の候補には送る");
    assert_eq!(sender.send.refused, 5, "範囲の外へは 1 通も送らず、数える");
    assert_eq!(sender.send.sent, 5, "送れたのは範囲の中の分だけ");
    assert_eq!(sender.punch_counts.sent, 10, "予定は 2 候補 × 5 回");
}

#[test]
fn a_newer_request_replaces_the_plan_and_an_old_request_is_taken_once() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let (t1, t2) = (punch_target(), punch_target());
    shared.request_punch(cands_of(&[t1.local_addr().unwrap()]));
    sender.tick(&sock, 0);
    sender.tick(&sock, 200);
    assert_eq!(punches_seen(&t1), 2);
    shared.request_punch(cands_of(&[t2.local_addr().unwrap()]));
    for k in 0..9 {
        sender.tick(&sock, 400 + k * 200);
    }
    assert_eq!(punches_seen(&t1), 0, "古い予定が残っている");
    assert_eq!(punches_seen(&t2), 5, "新しい予定は 5 回");
    assert_eq!(sender.punch_counts.started, 2);
    assert_eq!(shared.punch_generation(), 2);
}
