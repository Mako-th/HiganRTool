//! 観戦のホストの検査（本物のUDP・受信スレッド・リングバッファ。観戦者はcoreの`Watcher`を別のスレッドで走らせる）。
use super::*;
use crate::net_udp;
use crate::netplay::Net;
use crate::spectate_testkit::{Behavior, NOACK_WINDOW, Observer};
use higanr_core::auth::AuthKey;
use higanr_core::candidate::Candidates;
use higanr_core::wire::{
    self, MAX_DATAGRAM, MAX_STREAM_RECORDS, Message, Seat, Select, Step, StepKind,
};
use std::time::{Duration, Instant};

pub(super) const MATCH_SESSION: u32 = 0x5EED_0001;
const MATCH_SECRET: Secret = Secret([0x42; 16]);

pub(super) fn lo(port: u16) -> SocketAddr {
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

/// ゲームのスレッドが書く記録の並び（seg 0の選択 → マッチ → seg 1のマッチ → 終わり）。`frames`はseg 0のマッチの長さ。
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

/// 受信スレッドが観戦のホストを取得するまで待つ（設定した直後に届いたJOINは対戦のセッションへ回るので黙る観戦者が入れなくなる）。
pub(super) fn wait_active(rx: &net_udp::RecvThread) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rx.spectate_active() {
        assert!(
            Instant::now() < deadline,
            "受信スレッドが観戦のホストを取らない"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(super) fn wait_joined(rx: &net_udp::RecvThread, n: u64) {
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

/// 対戦の相手（この検査のソケット）が送る対戦のsession・鍵のPING。受信スレッドはsessionで経路を分けるだけで中を開かない。そのため封じる鍵は検査の値でよい。
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
    // 受け待ちの上限を短くしない（1 msにすると対戦の相手のパケットが20走行に1走行ほど落ちた。`net_udp::RECV_POLL`）。
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
    // 観戦者が入ってから書く（遅れて入った観戦者はいまのsegの始まりから流れる）。
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
    // ACKを返さない観戦者が最初の1通で止まるのが見えるよう、ウィンドウより多く積む。
    let want = script(NOACK_WINDOW as u32 + 200);
    assert!(want.len() > NOACK_WINDOW + 200);
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
    // 普通の観戦者を先に待つ（全部が届くまで待つ間にACKを返さない観戦者へもウィンドウぶんが届き終える）。
    for o in observers.into_iter().rev() {
        match o.behavior {
            Behavior::Normal => {
                assert!(o.wait_end(Duration::from_secs(10)));
                normal = Some(o.finish());
            }
            Behavior::NoAck => {
                let s = o.finish();
                assert!(
                    (1..=MAX_STREAM_RECORDS).contains(&s.recs.len()),
                    "ACK を返さない観戦者は記録の最初の 1 通だけ受け、越えない: {}",
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

/// 開発用の`[spectate] fake_uplink`はiniの経路の観戦のホストの送り手（SpectateSender）に届く。
#[test]
fn the_dev_fake_uplink_reaches_the_sender_of_the_ini_path() {
    let ini = |extra: &str| {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:9\nsession = {MATCH_SESSION}\n[spectate]\nenable = 1\n{extra}"
        ))
        .unwrap()
        .unwrap()
    };
    let p = prepare(&ini("fake_uplink = 77000\n"), lo(47_002)).unwrap();
    assert_eq!(p.sender.fake_uplink(), Some(77_000));
    let p = prepare(&ini(""), lo(47_002)).unwrap();
    assert_eq!(p.sender.fake_uplink(), None);
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
        pending.is_none() && note.contains("観戦は使いません") && note_has_problem(&note),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_spectate_setup_failure_phrases_are_seen_as_problems_and_a_success_is_not() {
    for phrase in [
        " / 観戦は使いません: コードの保存先か受信ソケットのアドレスが不明",
        " / 観戦は使いません: 検査の理由",
        " / 観戦の配信を開始できませんでした（送り手の登録が重複）",
        " / 検査。ただし、コードを書けませんでした: 検査の理由",
    ] {
        assert!(note_has_problem(phrase), "{phrase}");
    }
    assert!(
        !note_has_problem(" / 観戦の配信側を用意しました（検査）"),
        "成功の句を失敗と読んだ"
    );
    assert!(!note_has_problem(""));
}

/// `tick`を検査が直に呼ぶ（受信スレッドの周回を待たない）のでリングバッファの溢れの形が決まる。
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

/// 集計の数を3通り設定する（自分で抜けた2・応答が無く外した5・リングバッファの溢れ7）。語と数がペアになって出る。
#[test]
fn the_snapshot_line_tells_leaving_by_bye_from_being_dropped_for_silence() {
    let mut s = Snapshot::default();
    s.host.left = 2;
    s.host.silenced = 5;
    s.host.overflows = 7;
    let text = s.text();
    assert!(
        text.contains("自分で抜けた 2 / 応答が無く外した 5 / リングバッファの溢れで全員外した 7"),
        "{text}"
    );
}

#[test]
fn the_snapshot_line_counts_acks_dropped_for_a_missing_cookie() {
    let mut s = Snapshot::default();
    s.host.acks = 9;
    s.host.acks_clamped = 1;
    s.host.sacks_clamped = 2;
    s.host.cookie_refused = 4;
    let text = s.text();
    assert!(
        text.contains(
            "受けた ACK 9（積んだより先の嘘 1・受け取れた範囲の嘘 2・クッキーを返さず捨てた 4）"
        ),
        "{text}"
    );
}

// ---------------- 穴あけ ----------------
//
// `SpectateSender::tick` / `on_datagram`を検査が直に呼ぶので時刻`now`を自分で決められる。宛先は本物のUDP（loopback）のソケット。

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

/// ソケットに届いているものを来なくなるまで数える。戻りは（穴あけの印HGRp, それ以外）。
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

/// 観戦用のコード（`SPEC_SESSION`・`SPEC_SECRET`）で作ったJOIN（`secret`を替えるとタグが合わない）。
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

/// 受け取りの待ちを短くした穴あけの宛先（複数を数える検査用）。
fn quick_target() -> UdpSocket {
    let s = UdpSocket::bind(lo(0)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    s
}

#[test]
fn a_request_for_another_viewer_adds_a_plan_and_a_repeat_restarts_only_that_viewers_plan() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let (t1, t2) = (punch_target(), punch_target());
    let (a1, a2) = (t1.local_addr().unwrap(), t2.local_addr().unwrap());
    shared.request_punch(cands_of(&[a1]));
    sender.tick(&sock, 0);
    sender.tick(&sock, 200);
    assert_eq!(punches_seen(&t1), 2);
    shared.request_punch(cands_of(&[a2]));
    for k in 0..9 {
        sender.tick(&sock, 400 + k * 200);
    }
    assert_eq!(punches_seen(&t1), 3, "1 人目の予定が後の頼みで消えた");
    assert_eq!(punches_seen(&t2), 5, "2 人目の予定は 5 回");
    // 同じ観戦者の頼み直し（候補が重なる）はその人の予定を作り直し、ほかの人の予定は動かさない。
    shared.request_punch(cands_of(&[a1]));
    for k in 0..9 {
        sender.tick(&sock, 3_000 + k * 200);
    }
    assert_eq!(punches_seen(&t1), 5, "頼み直しで 5 回やり直す");
    assert_eq!(punches_seen(&t2), 0, "別の観戦者の予定が動いた");
    let p = sender.punch_counts;
    assert_eq!((p.started, p.evicted), (3, 0));
    assert_eq!(shared.punch_generation(), 3);
}

#[test]
fn the_receive_thread_path_writes_the_snapshot_once_per_round_not_per_datagram() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    sender.on_recv(&sock, lo(40_002), &join_bytes(SPEC_SECRET, 1), 1_000_000);
    assert_eq!(shared.get().host.joined, 0, "便ごとに集計を書いた");
    sender.tick(&sock, 1_000);
    let s = shared.get();
    assert_eq!((s.host.joined, s.spectators), (1, 1), "tick が書いていない");
}

#[test]
fn a_join_after_the_plan_has_finished_is_not_counted_as_a_stop() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let t = quick_target();
    let a = t.local_addr().unwrap();
    shared.request_punch(cands_of(&[a]));
    for k in 0..9 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    sender.on_datagram(&sock, a, &join_bytes(SPEC_SECRET, 1), 2_000);
    assert_eq!(drain_seen(&t).0, 5, "5 回送り切った");
    assert_eq!(
        sender.punch_counts.stopped, 0,
        "送り終えた予定を止めたと数えた"
    );
    assert_eq!(shared.get().host.joined, 1, "JOIN が通っていない");
}

#[test]
fn viewers_sharing_one_public_candidate_get_separate_plans_and_a_reordered_repeat_is_one_viewer() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let (public, lan_a, lan_b) = (quick_target(), quick_target(), quick_target());
    let addr = |t: &UdpSocket| t.local_addr().unwrap();
    let (p, a, b) = (addr(&public), addr(&lan_a), addr(&lan_b));
    shared.request_punch(cands_of(&[p, a]));
    shared.request_punch(cands_of(&[p, b]));
    for k in 0..9 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    assert_eq!(punches_seen(&public), 10, "公開側の候補へは 2 人分");
    assert_eq!(
        punches_seen(&lan_a),
        5,
        "1 人目の予定が 2 人目に作り直された"
    );
    assert_eq!(punches_seen(&lan_b), 5);
    assert_eq!(sender.punch_counts.evicted, 0);
    shared.request_punch(cands_of(&[a, p]));
    for k in 0..9 {
        sender.tick(&sock, 3_000 + k * PUNCH_INTERVAL_MS);
    }
    assert_eq!(
        punches_seen(&public),
        5,
        "順の違う同じ集合は 1 人の頼み直し"
    );
    assert_eq!(punches_seen(&lan_a), 5);
    assert_eq!(punches_seen(&lan_b), 0, "もう 1 人の予定が動いた");
}

#[test]
fn several_requests_before_one_tick_all_become_plans() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let ts: Vec<UdpSocket> = (0..PUNCH_PLANS).map(|_| quick_target()).collect();
    for t in &ts {
        shared.request_punch(cands_of(&[t.local_addr().unwrap()]));
    }
    sender.tick(&sock, 0);
    for (i, t) in ts.iter().enumerate() {
        assert_eq!(punches_seen(t), 1, "{i} 人目の最初の回");
    }
    assert_eq!(sender.punch_counts.started, PUNCH_PLANS as u32);
    assert_eq!(sender.punch_counts.evicted, 0);
}

#[test]
fn with_every_plan_busy_the_one_furthest_along_makes_room_and_finished_plans_are_reused_first() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let ts: Vec<UdpSocket> = (0..=PUNCH_PLANS).map(|_| quick_target()).collect();
    let req = |i: usize| shared.request_punch(cands_of(&[ts[i].local_addr().unwrap()]));
    // 0番だけ先に始め、3回送ったところで残りの9人が加わる（枠が全部埋まる）。
    req(0);
    for k in 0..3 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    for i in 1..PUNCH_PLANS {
        req(i);
    }
    sender.tick(&sock, 3 * PUNCH_INTERVAL_MS);
    assert_eq!(sender.punch_counts.evicted, 0);
    // 11人目: 送り終えた予定が無いのでいちばん先まで送った0番の予定（4回送った）を押し出す。
    req(PUNCH_PLANS);
    for k in 4..14 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    let seen: Vec<usize> = ts.iter().map(punches_seen).collect();
    let mut want = vec![5; PUNCH_PLANS + 1];
    want[0] = 4;
    assert_eq!(seen, want, "押し出されたのは 0 番だけ");
    assert_eq!(sender.punch_counts.evicted, 1);
    // 全部送り終えた後の頼みは送り終えた枠を使い、押し出さない。
    let late = quick_target();
    shared.request_punch(cands_of(&[late.local_addr().unwrap()]));
    for k in 20..30 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    assert_eq!(punches_seen(&late), 5);
    assert_eq!(
        sender.punch_counts.evicted, 1,
        "送り終えた枠があるのに押し出した"
    );
}

#[test]
fn a_joining_viewer_stops_only_its_own_plan_among_several() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let (t1, t2) = (quick_target(), quick_target());
    let (a1, a2) = (t1.local_addr().unwrap(), t2.local_addr().unwrap());
    shared.request_punch(cands_of(&[a1]));
    shared.request_punch(cands_of(&[a2]));
    sender.tick(&sock, 0);
    sender.on_datagram(&sock, a1, &join_bytes(SPEC_SECRET, 1), 10);
    for k in 1..9 {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    let (p1, _) = drain_seen(&t1);
    assert_eq!(p1, 1, "JOIN が来た観戦者の予定は止まる");
    assert_eq!(punches_seen(&t2), 5, "別の観戦者の予定は続く");
    assert_eq!(sender.punch_counts.stopped, 1);
}

/// 測りのペアPAIR（ホストとの鍵で封じた1,200 B）。`peek_type`が見分ける形。中身を開くのは番を持つ観戦者の鍵だけ。
fn pair_bytes() -> Vec<u8> {
    let dir = higanr_core::seal::DirKeys::from_raw([0x51; 32], [0x51; 32]);
    let mut b = [0u8; MAX_DATAGRAM];
    let n = wire::seal(
        &dir.sealer(higanr_core::seal::Sender::Spectator),
        &higanr_core::seal::Seq::new(),
        SPEC_SESSION,
        &Message::SpectatePair(wire::SpectatePair {
            turn: 1,
            pair: 0,
            second: false,
        }),
        &mut b,
    )
    .expect("封じられる");
    b[..n].to_vec()
}

#[test]
fn a_pair_read_by_the_receive_thread_goes_to_the_measure_and_one_passed_as_a_plain_datagram_is_unexpected()
 {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let from = lo(40_001);
    let pair = pair_bytes();
    assert_eq!(wire::peek_type(&pair), PeekedType::SpectatePair);
    sender.on_recv(&sock, from, &pair, 5_000_000);
    sender.tick(&sock, 5_000);
    let h = shared.get().host;
    assert_eq!(
        (h.pairs_dropped, h.unexpected, h.pairs),
        (1, 0, 0),
        "番を持たない観戦者の組は、開かずに捨てた組として数える: {h:?}"
    );
    sender.on_datagram(&sock, from, &pair, 5_000);
    let h = shared.get().host;
    assert_eq!(
        (h.pairs_dropped, h.unexpected),
        (1, 1),
        "ms の時刻の入口は組を受けない: {h:?}"
    );
}

#[test]
fn the_snapshot_lines_add_the_tree_and_measure_rows_only_when_they_have_numbers() {
    let mut s = Snapshot::default();
    assert_eq!(s.tree_text(), None);
    assert_eq!(s.measure_text(), None);
    assert_eq!(s.lines(false), s.text(), "数が無いうちは今までの行のまま");
    s.relays = 2;
    s.host.intros = 5;
    s.host.intro_resends = 1;
    s.host.intros_expired = 2;
    s.host.admit_resends = 3;
    s.host.reserved_used = 4;
    s.host.rehomes_sent = 6;
    s.host.relay_jumps = 7;
    s.host.descendants_clamped = 8;
    s.host.not_admitted = 9;
    s.host.punches_sent = 10;
    s.host.measures = 11;
    s.host.measure_resends = 12;
    s.host.uplinks_measured = 13;
    s.host.uplinks_unmeasured = 14;
    s.host.pairs = 15;
    s.host.pairs_dropped = 16;
    assert_eq!(
        s.tree_text().unwrap(),
        "観戦者のツリー: 中継役 2 人 / 中継役への振り分け 5（再送 1・期限切れ 2） / 受け入れ依頼の再送 3 / 確保済みの枠での受け入れ 4 / 中継役が子へ送った付け替え指示 6 / 中継役の記録の積み直し 7 / 子の数の申告の上限超え 8 / 依頼のない JOIN 9 / 外した中継役の子へ送った穴あけパケット 10"
    );
    assert_eq!(
        s.measure_text().unwrap(),
        "上り回線速度の測定: 観戦者へ測定依頼を送った 11（測定依頼の再送 12） / 速度の確定 13・測定不能 14 / 観戦者から受けた測定パケット 15・捨てた測定パケット 16"
    );
    let lines = s.lines(true);
    let rows: Vec<&str> = lines.lines().collect();
    assert_eq!(rows.len(), 4, "{lines}");
    assert!(rows[1].starts_with("観戦者のツリー:") && rows[2].starts_with("上り回線速度の測定:"));
    assert!(rows[3].starts_with("観戦帯域の調整:"));
    // 片方だけに数があればその行だけ。
    let only_measure = Snapshot {
        host: HostCounts {
            measures: 1,
            ..HostCounts::default()
        },
        ..Snapshot::default()
    };
    assert_eq!(only_measure.tree_text(), None);
    assert!(only_measure.measure_text().is_some());
    // 数が1つだけでも行は出る。
    let one = |f: fn(&mut Snapshot)| {
        let mut s = Snapshot::default();
        f(&mut s);
        (s.tree_text().is_some(), s.measure_text().is_some())
    };
    assert_eq!(one(|s| s.relays = 1), (true, false));
    assert_eq!(one(|s| s.host.rehomes_sent = 1), (true, false));
    assert_eq!(one(|s| s.host.relay_jumps = 1), (true, false));
    assert_eq!(one(|s| s.host.punches_sent = 1), (true, false));
    assert_eq!(one(|s| s.host.pairs_dropped = 1), (false, true));
    assert_eq!(one(|s| s.host.uplinks_unmeasured = 1), (false, true));
    assert_eq!(one(|s| s.host.swaps = 1), (true, false));
    assert_eq!(one(|s| s.host.swap_freed = 1), (true, false));
}

#[test]
fn the_tree_row_adds_the_swaps_only_when_there_are_some() {
    let mut s = Snapshot {
        relays: 1,
        ..Snapshot::default()
    };
    let plain = s.tree_text().unwrap();
    assert!(!plain.contains("入れ替え"), "{plain}");
    s.host.swaps = 3;
    s.host.swap_freed = 2;
    let t = s.tree_text().unwrap();
    assert_eq!(
        t,
        format!("{plain} / マッチの切れ目の入れ替え 3（空いた直接接続の枠 2）")
    );
    assert!(!s.text().contains("入れ替え"), "silenced の行に混ぜた");
}

// ---------------- IPv4とIPv6の両方の経路 ----------------
//
// 観戦者は観戦用のコードのIPv4・IPv6の全候補へJOINを送り、最初にSTREAMが返った経路に決める。
// ホストはIPv6の受信スレッドでも観戦者のパケットを受け、返事は宛先のアドレスファミリのソケットから出す。
// IPv6のloopback（`::1`）が使えないPCでは、その旨を標準エラーに書いて検査を測らずに戻る。

fn v6lo(port: u16) -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), port)
}

fn v6_available(what: &str) -> bool {
    match UdpSocket::bind(v6lo(0)) {
        Ok(_) => true,
        Err(e) => {
            eprintln!(
                "IPV6-UNMEASURED: {what}: この PC では [::1] を bind できない（{e}）。この検査は測れていない"
            );
            false
        }
    }
}

/// どこも待っていないIPv4のloopbackのアドレス（届かない候補）。
fn dead_v4() -> SocketAddr {
    let s = UdpSocket::bind(lo(0)).unwrap();
    s.local_addr().unwrap()
}

struct DualHost {
    rx: net_udp::RecvThread,
    ring: Arc<SpectateRing>,
    v4: SocketAddr,
    v6: SocketAddr,
    code_text: String,
    // 受信ソケットを閉じないために持つ。
    net: net_udp::GameNet,
    _peer: UdpSocket,
}

/// IPv4とIPv6の受信ソケットを持つホストを作り、観戦のホストを組み込む。
fn dual_host() -> DualHost {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let v4 = net.local_addr().unwrap();
    let v6 = net.open_ipv6(v6lo(0)).unwrap();
    let p = prepare(&settings(peer_addr, true), v4).unwrap();
    let ring = Arc::clone(&p.ring);
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    DualHost {
        rx,
        ring,
        v4,
        v6,
        code_text: p.code_text,
        net,
        _peer: peer,
    }
}

/// 観戦用のコードの候補だけを替えたコード（session・秘密の乱数・鍵は同じ）。
fn code_with(code_text: &str, cands: &[SocketAddr]) -> String {
    let c = code::read_spectate(code_text, net_udp::now_unix()).unwrap();
    code::write_spectate(
        &Code {
            candidates: cands_of(cands),
            ..c.code
        },
        &c.host_public,
    )
    .unwrap()
}

fn watcher_for(code_text: &str, nonce: u64) -> Watcher {
    let c = code::read_spectate(code_text, net_udp::now_unix()).unwrap();
    Watcher::new(WatcherConfig {
        session_id: c.code.session_id,
        secret: c.code.secret.unwrap(),
        nonce,
        name: Name::new("viewer"),
        host: c.code.candidates,
        host_public: c.host_public,
        key_seed: net_udp::draw_key_seed().unwrap(),
    })
}

/// 観戦者の側を`limit`まで回す（本物のソケット。`GameNet`が宛先のアドレスファミリでソケットを選ぶ）。`done`が真になったらtrue。
fn drive(
    net: &mut net_udp::GameNet,
    w: &mut Watcher,
    limit: Duration,
    mut done: impl FnMut(&mut Watcher) -> bool,
) -> bool {
    let t0 = Instant::now();
    let mut buf = [0u8; MAX_DATAGRAM];
    while t0.elapsed() < limit {
        let now = t0.elapsed().as_millis() as u32;
        while let Some((from, n, _)) = net.recv(&mut buf) {
            w.on_datagram(from, &buf[..n], now, net);
        }
        w.poll(now, net);
        if done(w) {
            return true;
        }
        net.wait(2_000);
    }
    false
}

/// 観戦者の側のソケット（IPv4と、`v6`ならIPv6）。
fn viewer_net(v6: bool) -> (net_udp::GameNet, net_udp::RecvThread) {
    let (net, rx) = net_udp::open(lo(0), &[lo(9)]).unwrap();
    if v6 {
        net.open_ipv6(v6lo(0)).unwrap();
    }
    (net, rx)
}

/// ホストへ記録を書いて観戦者がそれを全部受け取るまで回す。
fn deliver_script(h: &DualHost, vnet: &mut net_udp::GameNet, w: &mut Watcher) {
    let want = script(60);
    for r in &want {
        assert!(h.ring.push(*r));
    }
    let mut got = Vec::new();
    let ok = drive(vnet, w, Duration::from_secs(8), |w| {
        while let Some(r) = w.pop() {
            got.push(r);
        }
        got.len() == want.len()
    });
    assert!(
        ok,
        "記録が全部届かない: {} / {} {:?}",
        got.len(),
        want.len(),
        h.rx.spectate_snapshot()
    );
    assert_eq!(got, want);
}

#[test]
fn a_watcher_joins_over_ipv6_when_the_ipv4_candidate_does_not_answer() {
    if !v6_available("IPv6 だけ届く観戦") {
        return;
    }
    let h = dual_host();
    let text = code_with(&h.code_text, &[dead_v4(), h.v6]);
    let (mut vnet, _vrx) = viewer_net(true);
    let mut w = watcher_for(&text, 0x61);
    assert!(
        drive(&mut vnet, &mut w, Duration::from_secs(8), |w| w.is_joined()),
        "IPv6 の候補だけが届くのに繋がらない: {:?} {:?}",
        w.counts(),
        h.rx.spectate_snapshot()
    );
    assert_eq!(w.host_addr(), Some(h.v6), "IPv6 の経路に決まっていない");
    deliver_script(&h, &mut vnet, &mut w);
    let snap = h.rx.spectate_snapshot().unwrap();
    assert_eq!(
        (snap.spectators, snap.spectators_v6),
        (1, 1),
        "IPv6 の経路の観戦者として数える: {snap:?}"
    );
    assert_eq!(snap.via.recv_v4, 0, "IPv4 には何も届いていないはず");
    assert_eq!(
        snap.send.errors, 0,
        "族の違うソケットから送って誤りになった: {snap:?}"
    );
    assert!(snap.via.recv_v6 >= 1 && snap.send.sent_v6 >= 1, "{snap:?}");
    let text = snap.text();
    assert!(text.contains("うち IPv6 の経路 1 人"), "{text}");
    assert!(
        !text.contains("::1") && !text.contains("127.0.0.1"),
        "行にアドレスが出た: {text}"
    );
}

#[test]
fn when_both_paths_answer_the_first_one_wins_and_the_other_is_dropped_and_left_alone() {
    if !v6_available("両方届く観戦") {
        return;
    }
    let h = dual_host();
    let text = code_with(&h.code_text, &[h.v4, h.v6]);
    let (mut vnet, _vrx) = viewer_net(true);
    let mut w = watcher_for(&text, 0x62);
    assert!(
        drive(&mut vnet, &mut w, Duration::from_secs(8), |w| w.is_joined()),
        "繋がらない: {:?}",
        h.rx.spectate_snapshot()
    );
    let chosen_v6 = w.host_addr().unwrap().is_ipv6();
    // 余った経路の枠が外れて1人になる（返事を待つ間は両方の枠が立っていてよい）。
    let settled = drive(&mut vnet, &mut w, Duration::from_secs(5), |_| {
        h.rx.spectate_snapshot().is_some_and(|s| s.spectators == 1)
    });
    assert!(
        settled,
        "枠が 1 つにならない: {:?}",
        h.rx.spectate_snapshot()
    );
    std::thread::sleep(Duration::from_millis(300));
    let before = h.rx.spectate_snapshot().unwrap();
    deliver_script(&h, &mut vnet, &mut w);
    drive(&mut vnet, &mut w, Duration::from_millis(1_200), |_| false);
    let after = h.rx.spectate_snapshot().unwrap();
    let (chosen, other) = if chosen_v6 {
        (
            after.via.recv_v6 - before.via.recv_v6,
            after.via.recv_v4 - before.via.recv_v4,
        )
    } else {
        (
            after.via.recv_v4 - before.via.recv_v4,
            after.via.recv_v6 - before.via.recv_v6,
        )
    };
    assert!(chosen >= 1, "選んだ経路から ACK が来ていない: {after:?}");
    assert_eq!(other, 0, "選ばなかった経路へ送り続けている: {after:?}");
    assert_eq!(after.spectators, 1, "{after:?}");
    assert_eq!(
        after.send.errors, 0,
        "族の違うソケットから送って誤りになった: {after:?}"
    );
    assert_eq!(after.spectators_v6, u32::from(chosen_v6), "{after:?}");
}

#[test]
fn a_host_with_an_ipv6_socket_still_serves_an_ipv4_only_watcher() {
    if !v6_available("IPv4 だけの観戦者") {
        return;
    }
    let h = dual_host();
    let text = code_with(&h.code_text, &[h.v4]);
    let (mut vnet, _vrx) = viewer_net(false);
    let mut w = watcher_for(&text, 0x63);
    assert!(
        drive(&mut vnet, &mut w, Duration::from_secs(8), |w| w.is_joined()),
        "繋がらない: {:?}",
        h.rx.spectate_snapshot()
    );
    assert_eq!(w.host_addr(), Some(h.v4));
    deliver_script(&h, &mut vnet, &mut w);
    let snap = h.rx.spectate_snapshot().unwrap();
    assert_eq!((snap.spectators, snap.spectators_v6), (1, 0), "{snap:?}");
    assert_eq!((snap.via.recv_v6, snap.send.sent_v6), (0, 0), "{snap:?}");
    assert_eq!(snap.send.errors, 0, "送りの誤り: {snap:?}");
    // コードにIPv6の候補が先頭にあっても観戦者がIPv6のソケットを持たなければIPv4で接続する。
    let both = code_with(&h.code_text, &[h.v6, h.v4]);
    let mut w2 = watcher_for(&both, 0x64);
    let (mut vnet2, _vrx2) = viewer_net(false);
    assert!(
        drive(&mut vnet2, &mut w2, Duration::from_secs(8), |w| w
            .is_joined()),
        "IPv6 の候補が先頭でも IPv4 で繋がる"
    );
    assert_eq!(w2.host_addr(), Some(h.v4));
}

#[test]
fn punches_leave_from_the_socket_of_the_destinations_family() {
    if !v6_available("族ごとの穴あけ") {
        return;
    }
    let (mut sender, sock4, shared) = punch_sender(Scope::Loopback);
    let sock6 = UdpSocket::bind(v6lo(0)).unwrap();
    let t4 = punch_target();
    let t6 = UdpSocket::bind(v6lo(0)).unwrap();
    t6.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    shared.request_punch(cands_of(&[
        t4.local_addr().unwrap(),
        t6.local_addr().unwrap(),
    ]));
    let socks = Socks {
        primary: &sock4,
        v6: Some(&sock6),
    };
    for k in 0..u32::from(PUNCH_ROUNDS) {
        sender.tick(socks, k * PUNCH_INTERVAL_MS);
    }
    assert_eq!(punches_seen(&t4), 5, "IPv4 の宛先へ");
    let mut buf = [0u8; 64];
    let mut n6 = 0;
    while let Ok((len, from)) = t6.recv_from(&mut buf) {
        assert_eq!(&buf[..len], &PUNCH_PACKET[..]);
        assert_eq!(
            from,
            sock6.local_addr().unwrap(),
            "IPv6 のソケットから出ていない"
        );
        n6 += 1;
    }
    assert_eq!(n6, 5, "IPv6 の宛先へ");
    assert_eq!(
        (sender.send.sent, sender.send.sent_v6, sender.send.errors),
        (10, 5, 0)
    );
}

/// 隣の検査が範囲の中のIPv6には送れることを見る。
#[test]
fn punches_to_an_ipv6_destination_outside_the_scope_are_refused_at_the_door() {
    let (mut sender, sock, shared) = punch_sender(Scope::Loopback);
    let far6: SocketAddr = "[2606:4700::1]:4000".parse().unwrap();
    shared.request_punch(cands_of(&[far6]));
    for k in 0..u32::from(PUNCH_ROUNDS) {
        sender.tick(&sock, k * PUNCH_INTERVAL_MS);
    }
    assert_eq!(sender.send.refused, 5, "範囲の外へは 1 通も送らず、数える");
    assert_eq!(
        (sender.send.sent, sender.send.sent_v6, sender.send.errors),
        (0, 0, 0)
    );
}

/// IPv4の待ちの位相が偶然合って通ることがないよう、20回のうち17回以上が15 ms以内に収まることを条件にする。
#[test]
#[ignore = "実時間の 15 ms を測る(他の検査と処理を取り合うと揺れる)ので単独の組で走らせる。全件の回で --ignored の直列の組に入る"]
fn a_match_datagram_on_the_ipv6_socket_drains_the_spectate_ring_at_once() {
    if !v6_available("IPv6 の周回") {
        return;
    }
    let h = dual_host();
    let peer6 = UdpSocket::bind(v6lo(0)).unwrap();
    let pushed = || h.rx.spectate_snapshot().map_or(0, |s| s.host.pushed);
    assert!(h.ring.push(Record::Seg { seg: 0 }));
    let (mut quick, mut slowest) = (0, Duration::ZERO);
    for i in 0..20u32 {
        // 前の記録が空になるのを待ってから1件積む（他の周回が先に空にした分は数えない）。
        let settle = Instant::now();
        while u64::from(h.ring.counts().pushed) != pushed()
            && settle.elapsed() < Duration::from_secs(1)
        {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(7 + u64::from(i % 5) * 9));
        let before = pushed();
        assert!(h.ring.push(Record::Frame {
            frame: i,
            p1: 1,
            p2: 2
        }));
        let t = Instant::now();
        peer6.send_to(&match_ping(i), h.v6).unwrap();
        while pushed() == before && t.elapsed() < Duration::from_millis(300) {
            std::thread::yield_now();
        }
        let took = t.elapsed();
        slowest = slowest.max(took);
        if took <= Duration::from_millis(15) {
            quick += 1;
        }
    }
    assert!(
        quick >= 17,
        "IPv6 の便が来ても観戦のリングバッファを空にしない: 15 ms 以内 {quick} / 20（最も遅い {slowest:?}）"
    );
}

/// 観戦者の側のJOIN(`Watcher::poll`が出すもの)を宛先つきで集める。
fn join_packets(code_text: &str, nonce: u64) -> Vec<(SocketAddr, Vec<u8>)> {
    struct Collect(Vec<(SocketAddr, Vec<u8>)>);
    impl Outlet for Collect {
        fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
            self.0.push((to, bytes.to_vec()));
        }
    }
    let mut w = watcher_for(code_text, nonce);
    let mut out = Collect(Vec::new());
    w.poll(0, &mut out);
    out.0
}

#[test]
fn a_busy_spectate_host_never_holds_up_the_receive_threads_and_keeps_the_watchers_packets() {
    if !v6_available("錠を待たない") {
        return;
    }
    let mut h = dual_host();
    let text = code_with(&h.code_text, &[h.v4, h.v6]);
    let peer4 = UdpSocket::bind(lo(0)).unwrap();
    let peer6 = UdpSocket::bind(v6lo(0)).unwrap();
    let guard = h.rx.hold_spectate_sender();
    // 観戦者のJOIN(IPv4・IPv6)と、対戦のPINGを5通ずつ。
    for (to, bytes) in join_packets(&text, 0x71) {
        let sock: &UdpSocket = if to.is_ipv6() { &peer6 } else { &peer4 };
        sock.send_to(&bytes, to).unwrap();
    }
    for i in 0..5 {
        peer4.send_to(&match_ping(i), h.v4).unwrap();
        peer6.send_to(&match_ping(i), h.v6).unwrap();
    }
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut got = 0;
    let t0 = Instant::now();
    while got < 10 && t0.elapsed() < Duration::from_secs(2) {
        while h.net.recv(&mut buf).is_some() {
            got += 1;
        }
        h.net.wait(2_000);
    }
    assert_eq!(got, 10, "観戦のホストの錠を待って、対戦の便が止まった");
    assert_eq!(
        h.rx.spectate_snapshot().map_or(0, |s| s.host.joined),
        0,
        "錠を持たれている間に観戦のホストが動いた"
    );
    drop(guard);
    // 離した後の周回で積んであったJOINを処理する(失われていない)。
    let t0 = Instant::now();
    while h.rx.spectate_snapshot().map_or(0, |s| s.host.joined) < 1 {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "積んだ JOIN が処理されない: {:?}",
            h.rx.spectate_snapshot()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let snap = h.rx.spectate_snapshot().unwrap();
    assert_eq!(
        (snap.via.recv_v4, snap.via.recv_v6, snap.via.queue_dropped),
        (1, 1, 0),
        "{snap:?}"
    );
    h.net.wait(1_000);
}

#[test]
fn the_queue_behind_a_busy_lock_is_bounded_and_counts_what_it_drops() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer_addr, true), me).unwrap();
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    let join = join_packets(&p.code_text, 0x72).remove(0).1;
    let guard = rx.hold_spectate_sender();
    const SENT: u32 = 100;
    for _ in 0..SENT {
        peer.send_to(&join, me).unwrap();
    }
    let cap = crate::net_ring::RING_SLOTS as u32;
    let t0 = Instant::now();
    while rx.spectate_snapshot().map_or(0, |s| s.via.queue_dropped) < SENT - cap {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "溢れた便を数えない: {:?}",
            rx.spectate_snapshot()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(guard);
    let t0 = Instant::now();
    while rx.spectate_snapshot().map_or(0, |s| s.via.recv_v4) < cap {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "積んだ分が処理されない: {:?}",
            rx.spectate_snapshot()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let snap = rx.spectate_snapshot().unwrap();
    assert_eq!(
        snap.via.recv_v4 + snap.via.queue_dropped,
        SENT,
        "受けた便が処理されたか捨てて数えたかのどちらかでない: {snap:?}"
    );
    assert!(snap.text().contains("錠待ちの列が溢れて捨てた"));
    drop(net);
}

/// ゲームのスレッドが書いた対戦の往復の要約（`Net::note_delay`）が受信スレッドの観戦者への送信に届く。
/// 往復が延びていれば送信を止める（遅れた観戦者へ記録を速く送る追いつきの送信が無いので半分にしない）。延びていない要約と「サンプルなし」では下げない。
#[test]
fn the_delay_noted_on_the_game_thread_reaches_the_spectate_sender() {
    use higanr_core::rtt_probe::DelaySnapshot;
    use higanr_core::spectate_rate::FULL_PPM;
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer_addr, true), me).unwrap();
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    let level = |rx: &net_udp::RecvThread| {
        rx.hold_spectate_sender()
            .as_ref()
            .expect("観戦のホスト")
            .throttle_state()
    };
    let note = |net: &mut net_udp::GameNet, recent: u16| {
        let at = (net.now_us() / 1_000) as u32;
        net.note_delay(Some(DelaySnapshot {
            at,
            queue_ms: recent - 30,
            fresh_ms: recent - 30,
            base_ms: 30,
        }));
    };
    // 延びていない要約・サンプルなしを0.3秒渡し続けても下げない（受信スレッドは50 msごとには回る）。
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(300) {
        note(&mut net, 35);
        std::thread::sleep(Duration::from_millis(5));
        net.note_delay(None);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(level(&rx), (FULL_PPM, false), "延びていないのに下げた");
    let t0 = Instant::now();
    loop {
        note(&mut net, 80);
        // 遅れた観戦者へ記録を速く送る追いつきの送信が無いので半分にせず止める。
        if level(&rx) == (FULL_PPM, true) {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "往復の延びが観戦の送りに届かない"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(net);
}

/// ゲームのスレッドが書いた自分の上り（`Net::note_host_uplink`）が受信スレッドの観戦者への送信に届く。
#[test]
fn the_own_uplink_noted_on_the_game_thread_reaches_the_spectate_sender() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer_addr, true), me).unwrap();
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    let uplink = |rx: &net_udp::RecvThread| {
        rx.hold_spectate_sender()
            .as_ref()
            .expect("観戦のホスト")
            .host_uplink()
    };
    assert_eq!(uplink(&rx), 0, "渡す前から値がある");
    net.note_host_uplink(123_456);
    let t0 = Instant::now();
    while uplink(&rx) != 123_456 {
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "自分の上りが観戦の送りに届かない"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(net);
}

#[test]
fn stopping_the_receive_threads_drops_the_spectate_host_even_while_the_game_net_lives() {
    let peer = UdpSocket::bind(lo(0)).unwrap();
    let (net, rx) = net_udp::open(lo(0), &[peer.local_addr().unwrap()]).unwrap();
    let me = net.local_addr().unwrap();
    let p = prepare(&settings(peer.local_addr().unwrap(), true), me).unwrap();
    assert!(rx.install_spectate(p.sender));
    wait_active(&rx);
    let shared = rx.spectate_shared().unwrap();
    let held = Arc::strong_count(&shared);
    rx.stop();
    assert_eq!(
        Arc::strong_count(&shared),
        held - 1,
        "止めた後も観戦のホストが残っている"
    );
    drop(net);
}

#[test]
fn the_rate_line_is_written_only_in_the_detail_log() {
    let snap = Snapshot {
        rate: RateSnap {
            throttle: ThrottleCounts {
                cuts: 3,
                pauses: 2,
                resumes: 1,
                paused_ms: 4_567,
            },
            level_ppm: 125_000,
            paused: true,
            delays: 890,
        },
        ..Snapshot::default()
    };
    let want = "観戦帯域の調整: 半減 3 回 / 停止 2 回 / 再開 1 回 / 停止の合計 4567 ms / RTT のサンプル 890 / 現在の割合 12.5%（停止中）";
    assert_eq!(snap.rate_text(), want);
    assert_eq!(snap.lines(true), format!("{}\n{want}", snap.text()));
    assert_eq!(
        snap.lines(false),
        snap.text(),
        "簡潔のログに調整の行を出した"
    );
    assert!(!snap.text().contains("観戦帯域の調整"));
}
