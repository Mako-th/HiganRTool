//! 組み上がりの検査: 偽のゲームを回すスレッドと peer_sim（`sim::run`）のスレッドを同じプロセスで loopback に繋ぐ。dll の側は
//! 本物の通信層（`net_udp`: ソケット・受信スレッド・リング・受信のイベントと高分解能の待機タイマー）と `NetPlay`。
//!
//! 本物の UDP の上で、接続 → 止める地点 → SELECT → lockstep（再戦で seg を進める）→ BYE を通し、dll と peer_sim の
//! (seg, frame, 1P, 2P) の列と CHECK（入力の列のダイジェスト）が一致すること、ローカル入力を描画だけのフレームの後に読み直さないこと、
//! ゲームスレッドの `on_input` が確保しないことを見る。付いていく形の 2 本（`peer_sim_follows_*`）は、マッチごとに長さの違う偽のゲーム
//! （peer_sim は長さを知らない）で、再戦とメニューへ戻っての次の選択を通す。`peer_sim_menu_plan_walks_*` は選択の画面を持つ偽のゲーム
//! （`menu_tests::MenuGame`）と選択の台本で動く peer_sim で、ネットの選択を 2 回通す。
//!
//! 実時間で走る（1 本 10 秒ほど）ので `#[ignore]`。全件の回（`build.bat`）が `--ignored` で走らせる。
use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use higanr_core::handshake::{self, Name};
use higanr_core::link;
use higanr_core::wire::{ByeReason, Seat};
use peer_sim::fault::FaultConfig;
use peer_sim::sim::{self, LatStep, SimConfig};

use super::fake::{FRAME_US, FakeGame};
use super::*;
use crate::net_udp;
use crate::spectate_testkit::{Behavior, Seen, attach, expected_selects, fold};

const SID: u32 = 0x5EED_3001;
const SEGS: u32 = 3;
const FRAMES: u32 = 120;
/// 付いていく形のマッチの長さ（1 回目の選択で 2 マッチ、メニューへ戻って 2 回目の選択で 2 マッチ）。
const FOLLOW_LENGTHS: [u32; 4] = [150, 45, 200, 90];

struct Case {
    dll_is_host: bool,
    follow: bool,
    lengths: Vec<u32>,
    segs_per_session: u32,
    sessions: u32,
    /// dll の latency の切替の予定（ホストのとき）。
    dll_lat_plan: Vec<(u64, u8)>,
    /// peer_sim の切替の台本（ホストのとき）と、送りの壊し方。
    sim_lat: Vec<LatStep>,
    sim_fault: FaultConfig,
    /// dll は ini に秘密を書かずに開き（`net_udp::prepare_link` が OS の乱数で引き、偽の STUN に問い合わせ、自分の接続コードを作る）、
    /// peer_sim はそのコードから session・秘密・候補を取る（peer_sim の `--code` と同じ読み方）。
    via_code: bool,
    /// dll の受信ソケットに繋ぐ観戦者（空なら観戦のホストを置かない）。
    spectators: Vec<Behavior>,
    /// 遅れの量の選び: 空でなければ dll の `Config::delay_pick` を立て、dll の使ったフレームの数がこの範囲の間だけ、
    /// Ctrl+その数字を押し続ける（範囲の始まりで 1 回の押下）。
    pick_windows: Vec<(usize, usize, u8)>,
}

fn lo() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// dll がホストなら peer_sim の nonce を小さく。席はホストが 2P（役割と席を入れ違いにする）。
fn run(case: Case) -> Outcome {
    let dll_is_host = case.dll_is_host;
    let total: u32 = case.lengths.iter().sum();
    let segs = case.lengths.len() as u32;
    let psock = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let paddr = psock.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(), &[paddr]).expect("通信層を開ける");
    assert!(
        net.high_resolution_wait(),
        "高分解能の待機タイマーが作れない"
    );
    let daddr = net.local_addr().unwrap();
    let (dll_nonce, sim_nonce) = if dll_is_host {
        (0xF000, 0x0001)
    } else {
        (0x0001, 0xF000)
    };
    let dll_link = link::Config {
        handshake: handshake::Config {
            session_id: SID,
            role_nonce: dll_nonce,
            latency: 3,
            name: Name::new("dll"),
            host_seat: Seat::P2,
        },
        secret: peer_sim::cli::DEFAULT_SECRET,
        peers: higanr_core::candidate::Candidates::from_addrs(&[paddr]).unwrap(),
        key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
    };
    let (dll_link, sim_peers, sim_secret) = if case.via_code {
        let stun = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let ini = format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = {paddr}\nsession = {SID}\ndelay = 3\nhost_seat = 2\nname = dll\nstun = {}\n",
            stun.addr()
        );
        let s = crate::net_config::from_text(&ini).unwrap().unwrap();
        assert_eq!(s.secret, None, "検査の前提: ini に秘密が無い");
        let (lc, note) = net_udp::prepare_link(&s, &mut net, dll_nonce).unwrap();
        assert_eq!(
            lc.handshake, dll_link.handshake,
            "ini の値がセッションへ届いていない"
        );
        assert!(
            note.contains(&format!("見えた自分のアドレス {daddr}")),
            "{note}"
        );
        let text = note.rsplit(": ").next().unwrap_or("");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let code = higanr_core::code::read_initiator(text, now).expect("dll の接続コードが読める");
        assert_eq!(code.session_id, SID);
        assert_eq!(
            code.secret,
            Some(lc.secret),
            "コードの秘密と dll のセッションの秘密が違う"
        );
        assert_ne!(
            lc.secret,
            peer_sim::cli::DEFAULT_SECRET,
            "OS の乱数で引いていない"
        );
        let peers: Vec<SocketAddr> = code.candidates.as_slice().iter().map(|c| c.addr).collect();
        (
            lc,
            peers,
            code.secret.unwrap_or(peer_sim::cli::DEFAULT_SECRET),
        )
    } else {
        (dll_link, vec![daddr], peer_sim::cli::DEFAULT_SECRET)
    };
    assert_eq!(
        sim_peers.first(),
        Some(&daddr),
        "dll の受信ソケットが候補の先頭に無い"
    );

    let scfg = SimConfig {
        peer: daddr,
        extra_peers: sim_peers[1..].to_vec(),
        secret: sim_secret,
        stun: None,
        name: "sim".to_string(),
        session_id: SID,
        role_nonce: sim_nonce,
        latency: 2,
        host_seat: Seat::P2,
        seed: 0x51,
        follow: case.follow,
        word_mask: SEND_MASK,
        life: 3,
        poll_ms: 1,
        segs: SEGS,
        frames_per_seg: FRAMES,
        load_ms: if case.follow { 0 } else { 300 },
        lat_script: case.sim_lat.clone(),
        fault: case.sim_fault,
        max_ms: 60_000,
    };
    let sim_thread = thread::spawn(move || sim::run(&scfg, psock));

    let mut np = NetPlay::new(
        dll_link,
        Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            lat_plan: LatPlan::new(&case.dll_lat_plan).expect("予定"),
            require_net_selection: false,
            delay_pick: !case.pick_windows.is_empty(),
            ..Config::default()
        },
        &mut net,
    );
    let mut g = FakeGame::new(0xD11, FRAMES, case.segs_per_session, case.sessions);
    g.seg_frames = case.lengths.clone();
    g.menu_before_decide = 30;
    let observers = if case.spectators.is_empty() {
        Vec::new()
    } else {
        attach(&mut np, &rx, daddr, SID, &case.spectators)
    };

    let t0 = Instant::now();
    let mut next = t0;
    let mut closed = false;
    let mut alloc = 0u64;
    let mut first_choice: Option<u8> = None;
    loop {
        let played = g.recs.len();
        g.digit_held = Some(
            case.pick_windows
                .iter()
                .find(|(s, e, _)| (*s..*e).contains(&played))
                .map_or(0, |w| (1u16 << w.2) | crate::digit_keys::CTRL_BIT),
        );
        g.begin();
        let before = crate::test_alloc::count();
        let a = np.on_input(&mut g, &mut net);
        alloc += crate::test_alloc::count() - before;
        g.after(a, &mut np);
        if first_choice.is_none() {
            first_choice = np.delay_pick().and_then(|p| p.chosen());
        }
        if g.done && !closed {
            np.close(ByeReason::Finished);
            closed = true;
        }
        if closed && sim_thread.is_finished() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(40),
            "終わらない: {:?} {:?} {:?}",
            np.phase(),
            np.end(),
            np.stats()
        );
        next += Duration::from_micros(FRAME_US);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
    let rep = sim_thread
        .join()
        .expect("peer_sim のスレッドがパニックしない")
        .expect("peer_sim が走る");
    let spectated: Vec<(Behavior, Seen)> = observers
        .into_iter()
        .map(|o| {
            if o.behavior == Behavior::Normal {
                o.wait_end(Duration::from_secs(5));
            }
            (o.behavior, o.finish())
        })
        .collect();
    let snapshot = rx.spectate_snapshot();
    let rs = rx.stop();

    let role = match (dll_is_host, case.follow) {
        (true, false) => "dll=host",
        (false, false) => "dll=guest",
        (true, true) => "dll=host 付いていく形",
        (false, true) => "dll=guest 付いていく形",
    };
    assert!(
        matches!(rep.end, sim::End::Finished { peer_bye: true }),
        "{role}: peer_sim {:?}\n{}",
        rep.end,
        peer_sim::record::summary(&rep)
    );
    let theirs: Vec<FrameRec> = rep
        .recs
        .iter()
        .map(|r| FrameRec {
            seg: r.seg,
            frame: r.frame,
            p1: r.p1,
            p2: r.p2,
        })
        .collect();
    assert_eq!(theirs.len(), total as usize, "{role}");
    assert_eq!(g.recs, theirs, "{role}: dll と peer_sim の列が違う");
    let dll_delays: Vec<u8> = g.timings.iter().map(|t| t.delay).collect();
    let sim_delays: Vec<u8> = rep.recs.iter().map(|r| r.delay).collect();
    assert_eq!(dll_delays, sim_delays, "{role}: delay の列が違う");
    let switches = (!case.dll_lat_plan.is_empty() || !case.sim_lat.is_empty())
        .then(|| dll_delays.windows(2).filter(|w| w[0] != w[1]).count());
    if let Some(n) = switches {
        let planned = if dll_is_host {
            case.dll_lat_plan.len()
        } else {
            case.sim_lat.len()
        };
        assert_eq!(n, planned, "{role}: 切り替わった数");
        assert_eq!(np.stats().switches as usize, planned, "{role}");
        assert!(
            rep.lat_results.iter().all(|r| r.result.is_ok()),
            "{role}: {:?}",
            rep.lat_results
        );
    }
    if !case.pick_windows.is_empty() {
        let p = np.delay_pick().expect("選びを持っている");
        assert!(p.is_host(), "{role}");
        let r0 = first_choice.expect("最初の試合の始まりで初めの値が決まっていない");
        eprintln!(
            "{role}: 遅れの量の選び 初めの値 {r0} / RTT {:?} ms / {p:?}",
            np.connected().and_then(|c| c.liveness().rtt_ms())
        );
        assert!((1..=6).contains(&r0), "{role}: 推奨値 {r0}");
        assert_eq!((p.chosen(), p.digits()), (Some(2), 2), "{role}: {p:?}");
        let mut seen = vec![dll_delays[0]];
        for d in &dll_delays {
            if seen.last() != Some(d) {
                seen.push(*d);
            }
        }
        assert_eq!(
            seen.first(),
            Some(&3),
            "{role}: AGREE の 3 で始まらない {seen:?}"
        );
        if r0 != 3 {
            assert_eq!(seen.get(1), Some(&r0), "{role}: 推奨値へ移らない {seen:?}");
        }
        // 推奨値が 5 のときは 5 の押下で値が動かず、見分けられない（押下の数は上の `digits` が見る）。
        if r0 != 5 {
            assert!(
                seen.contains(&5),
                "{role}: 1 つ目の押下の 5 へ移らない {seen:?}"
            );
        }
        assert_eq!(seen.last(), Some(&2), "{role}: {seen:?}");
    }
    let c = np.connected().expect("繋がった");
    let checks: u32 = case.lengths.iter().map(|&n| n.div_ceil(30)).sum();
    let dll_checks = if case.sim_fault == FaultConfig::default() {
        checks
    } else {
        checks / 2
    };
    assert!(
        c.counts().check_matches >= dll_checks && rep.check_matches >= checks,
        "{role}: CHECK の一致 dll {} / peer_sim {}",
        c.counts().check_matches,
        rep.check_matches
    );
    assert_eq!(g.local_reads, u64::from(total), "{role}: 読んだ回数");
    assert_eq!(np.stats().segs_started, segs);
    if case.follow {
        let rematches = (case.segs_per_session - 1) * case.sessions;
        assert_eq!(
            rep.follow,
            Some(sim::FollowCounts {
                next_seg: rematches,
                peer_selecting: case.sessions - 1,
                ignored: 0
            }),
            "{role}"
        );
        assert_eq!(g.agreed.len(), case.sessions as usize, "{role}: 選択の回数");
    }
    assert!(
        !g.agreed.is_empty() && g.agreed.iter().all(|(_, a)| a.peer.life == 3),
        "{role}: peer_sim の初期ライフが届かない: {:?}",
        g.agreed
    );
    assert!(
        matches!(
            np.end(),
            Some(End::Closed(ByeReason::Finished)) | Some(End::PeerBye(ByeReason::Finished))
        ),
        "{role}: {:?}",
        np.end()
    );
    assert_eq!(alloc, 0, "{role}: on_input が確保した");
    assert_eq!(rs.ring.dropped_full, 0);
    let hello = np.link_hello_sent();
    assert!(
        hello[0] >= 1 && hello[1..].iter().all(|&n| n == 0),
        "{role}: 送った HELLO {hello:?}"
    );
    assert!(u64::from(hello[0]) <= net.send_counts().0, "{role}");
    assert_eq!(np.link_peer_candidates(), [1, 0, 0, 0], "{role}");
    assert_eq!(rs.diag.send_errors, [0; 4], "{role}: 送りの誤り");
    assert!(
        !rs.diag.keep.installed,
        "{role}: loopback で聞き直しを置いた"
    );
    let dll_role = np.connected().expect("繋がった").role();
    for (b, seen) in &spectated {
        if *b == Behavior::Normal {
            assert_normal_observer(role, seen, &mut g, dll_role, &theirs);
        }
    }
    eprintln!(
        "{role}: dll {:?} / 受信 {:?} / 送り {:?} / peer_sim stalls {} max_run {} / 観戦 {:?}",
        np.stats(),
        rs,
        net.send_counts(),
        rep.stalls,
        rep.max_stall_run,
        snapshot.map(|s| s.text())
    );
    Outcome {
        dll_stalls: np.stats().stalls,
        dll_max_stall_run: np.stats().max_stall_run,
        played: g.recs.clone(),
        delays: dll_delays,
        spectated,
        snapshot,
        checks: c.counts().check_matches,
    }
}

struct Outcome {
    /// 一致した CHECK の数（dll の側）。
    checks: u32,
    dll_stalls: u64,
    dll_max_stall_run: u32,
    played: Vec<FrameRec>,
    delays: Vec<u8>,
    spectated: Vec<(Behavior, Seen)>,
    snapshot: Option<crate::spectate_host::Snapshot>,
}

/// 普通の観戦者が受けた記録の列を、dll と peer_sim の使った列と全行比べる（再戦をまたぐ seg を含む）。
fn assert_normal_observer(
    role: &str,
    seen: &Seen,
    g: &mut FakeGame,
    dll_role: Role,
    peer_sim: &[FrameRec],
) {
    assert!(seen.joined, "{role}: 観戦者が入れていない");
    assert!(
        seen.recs
            .iter()
            .enumerate()
            .all(|(i, (idx, _))| *idx == i as u32),
        "{role}: 観戦者の通し番号が 0 から連続でない"
    );
    let recs: Vec<Record> = seen.recs.iter().map(|(_, r)| *r).collect();
    let f = fold(&recs);
    assert_eq!(recs.first(), Some(&Record::Seg { seg: 0 }), "{role}");
    assert_eq!(f.ends, vec![ByeReason::Finished], "{role}: 終わり");
    assert_eq!(
        recs.last(),
        Some(&Record::End(ByeReason::Finished)),
        "{role}"
    );
    assert_eq!(
        f.segs,
        (0..f.segs.len() as u16).collect::<Vec<_>>(),
        "{role}: seg の先頭が 1 ずつでない"
    );
    let dll: Vec<(u16, u32, u16, u16)> = g
        .recs
        .iter()
        .map(|r| (r.seg, r.frame, r.p1, r.p2))
        .collect();
    let sim: Vec<(u16, u32, u16, u16)> = peer_sim
        .iter()
        .map(|r| (r.seg, r.frame, r.p1, r.p2))
        .collect();
    assert!(!dll.is_empty());
    assert_eq!(f.frames, dll, "{role}: 観戦者の列と dll の列が違う");
    assert_eq!(f.frames, sim, "{role}: 観戦者の列と peer_sim の列が違う");
    let mut want = Vec::new();
    for (peer_seat, a) in g.agreed.clone() {
        let own_seat = if peer_seat == Seat::P1 {
            Seat::P2
        } else {
            Seat::P1
        };
        let own = g.own_select(own_seat, dll_role);
        want.extend(expected_selects(own_seat, own, &a));
    }
    let got: Vec<Record> = f
        .selects
        .iter()
        .map(|&(seat, sel)| Record::Select { seat, sel })
        .collect();
    assert_eq!(got, want, "{role}: SELECT の記録");
    assert!(!got.is_empty());
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn dll_as_host_plays_against_peer_sim() {
    run(fixed_case(true));
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn dll_as_guest_plays_against_peer_sim() {
    run(fixed_case(false));
}

/// 組み上がり: ini に秘密が無いとき、dll が OS の乱数で秘密を引き、自分の接続コードに秘密・session・受信ソケット（STUN で見えたアドレス）を載せ、
/// peer_sim がそのコードから dll と繋がれる。マッチの列は `dll_as_host_plays_against_peer_sim` と同じ形で見る。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn dll_initiates_with_an_os_secret_and_peer_sim_joins_by_its_code() {
    run(Case {
        via_code: true,
        ..fixed_case(true)
    });
}

/// 相手が後から起きても繋がる: dll の接続の手順が 10 秒の時間切れでやめたまま作り直さない、作り直しを数えない、相手のポートがまだ開いていない
/// 間の送り（ICMP の port unreachable が `WSAECONNRESET` で返る）で受信スレッドが止まる、を見る。peer_sim のポートは起きるまで閉じておく
/// （番号だけ先に取って離し、15 秒後に同じ番号で開き直す）。実時間で約 20 秒。逆向き（peer_sim が先に起きて dll が遅れる）は peer_sim の側に作り直しの入口が要る。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_sim_waking_15_seconds_late_still_connects() {
    const LATE: Duration = Duration::from_secs(15);
    const FRAMES_LATE: u32 = 120;
    let paddr = UdpSocket::bind(lo())
        .expect("loopback に bind できる")
        .local_addr()
        .unwrap();
    let (mut net, rx) = net_udp::open(lo(), &[paddr]).expect("通信層を開ける");
    let daddr = net.local_addr().unwrap();
    let scfg = SimConfig {
        peer: daddr,
        extra_peers: vec![],
        secret: peer_sim::cli::DEFAULT_SECRET,
        stun: None,
        name: "sim".to_string(),
        session_id: SID,
        role_nonce: 0x0001,
        latency: 2,
        host_seat: Seat::P2,
        seed: 0x51,
        follow: false,
        word_mask: SEND_MASK,
        life: 3,
        poll_ms: 1,
        segs: 1,
        frames_per_seg: FRAMES_LATE,
        load_ms: 300,
        lat_script: vec![],
        fault: FaultConfig::default(),
        max_ms: 60_000,
    };
    let sim_thread = thread::spawn(move || {
        thread::sleep(LATE);
        let psock = UdpSocket::bind(paddr).expect("同じ番号で開き直せる");
        sim::run(&scfg, psock)
    });
    let mut np = NetPlay::new(
        link::Config {
            handshake: handshake::Config {
                session_id: SID,
                role_nonce: 0xF000,
                latency: 2,
                name: Name::new("dll"),
                host_seat: Seat::P2,
            },
            secret: peer_sim::cli::DEFAULT_SECRET,
            peers: higanr_core::candidate::Candidates::from_addrs(&[paddr]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        },
        Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..Config::default()
        },
        &mut net,
    );
    let mut g = FakeGame::new(0xD15, FRAMES_LATE, 1, 1);
    g.menu_before_decide = 30;
    let t0 = Instant::now();
    let mut next = t0;
    let mut closed = false;
    let mut connected_after: Option<Duration> = None;
    loop {
        g.do_frame(&mut np, &mut net);
        if connected_after.is_none() && np.connected().is_some() {
            connected_after = Some(t0.elapsed());
        }
        if g.done && !closed {
            np.close(ByeReason::Finished);
            closed = true;
        }
        if closed && sim_thread.is_finished() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "終わらない: {:?} {:?} {:?}",
            np.phase(),
            np.end(),
            np.stats()
        );
        next += Duration::from_micros(FRAME_US);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
    let rep = sim_thread
        .join()
        .expect("peer_sim のスレッドがパニックしない")
        .expect("peer_sim が走る");
    let rs = rx.stop();
    let summary = peer_sim::record::summary(&rep);
    assert!(
        matches!(rep.end, sim::End::Finished { peer_bye: true }),
        "peer_sim {:?}\n{summary}",
        rep.end
    );
    let at = connected_after.expect("繋がった");
    assert!(
        at >= LATE && at < LATE + Duration::from_secs(2),
        "繋がった時刻 {at:?}"
    );
    let st = np.stats();
    assert_eq!(st.hs_restarts, 1, "{st:?}");
    assert_eq!(
        st.hs_restart_last,
        Some((
            handshake::FailReason::NothingReceived,
            handshake::Restart::Fresh
        ))
    );
    let theirs: Vec<FrameRec> = rep
        .recs
        .iter()
        .map(|r| FrameRec {
            seg: r.seg,
            frame: r.frame,
            p1: r.p1,
            p2: r.p2,
        })
        .collect();
    assert_eq!(g.recs.len(), FRAMES_LATE as usize);
    assert_eq!(g.recs, theirs, "dll と peer_sim の列が違う\n{summary}");
    eprintln!("M2-5 遅れて起きる相手: {at:?} で繋がった / dll {st:?} / 受信 {rs:?}");
}

fn fixed_case(dll_is_host: bool) -> Case {
    Case {
        dll_is_host,
        follow: false,
        lengths: vec![FRAMES; SEGS as usize],
        segs_per_session: SEGS,
        sessions: 1,
        dll_lat_plan: vec![],
        sim_lat: vec![],
        sim_fault: FaultConfig::default(),
        via_code: false,
        spectators: vec![],
        pick_windows: vec![],
    }
}

fn follow_case(dll_is_host: bool) -> Case {
    Case {
        dll_is_host,
        follow: true,
        lengths: FOLLOW_LENGTHS.to_vec(),
        segs_per_session: 2,
        sessions: 2,
        dll_lat_plan: vec![],
        sim_lat: vec![],
        sim_fault: FaultConfig::default(),
        via_code: false,
        spectators: vec![],
        pick_windows: vec![],
    }
}

/// 遅れの量をホストが決める: dll がホストで、Ctrl+数字で delay 方式の遅れを変えたとき、peer_sim（ゲスト。付いていく形・
/// 送りを固定 seed で乱す）と使った後の delay の列・マッチの列が一致する。期間 1 は seg 0 の 60〜71（5）、期間 2 は seg 2 の 25〜32（2）（マッチの長さ 150・45・200・90）。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_sim_follows_the_dll_host_digit_keys() {
    run(Case {
        pick_windows: vec![(60, 72, 5), (220, 228, 2)],
        sim_fault: FaultConfig {
            seed: 0x10_02,
            drop_pct: 5,
            delay_pct: 5,
            dup_pct: 5,
            delay_max_ms: 20,
            ..FaultConfig::default()
        },
        ..follow_case(true)
    });
}

/// dll がホストで予定どおりに latency を切り替えたとき（増やす・減らす・次の seg で増やす）、peer_sim（付いていく形）と使った後の delay の列・
/// マッチの列が一致する。固定 seed の乱れの下でも止まらない。切替はマッチの終わりから離して置く。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_sim_follows_dll_host_latency_switches_under_seeded_losses() {
    run(Case {
        dll_lat_plan: vec![(60, 5), (170, 1), (260, 4)],
        sim_fault: FaultConfig {
            seed: 0x4D24,
            drop_pct: 10,
            delay_pct: 10,
            dup_pct: 5,
            delay_max_ms: 30,
            ..FaultConfig::default()
        },
        ..follow_case(true)
    });
}

/// dll がゲストで、peer_sim（ホスト。付いていく形）が台本どおりに切り替えたとき、両者の delay の列・マッチの列が一致する。決め打ちのロス（13 通ごとに 3 通続けて落とす）の下でも止まらない。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn dll_guest_follows_peer_sim_host_latency_switches_under_fixed_losses() {
    run(Case {
        sim_lat: vec![
            LatStep {
                seg: 0,
                frame: 40,
                delay: 6,
            },
            LatStep {
                seg: 0,
                frame: 100,
                delay: 2,
            },
            LatStep {
                seg: 2,
                frame: 30,
                delay: 5,
            },
        ],
        sim_fault: FaultConfig {
            drop_every: 13,
            drop_run: 3,
            ..FaultConfig::default()
        },
        ..follow_case(false)
    });
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_sim_follows_dll_as_host_through_rematch_and_menu() {
    run(follow_case(true));
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_sim_follows_dll_as_guest_through_rematch_and_menu() {
    run(follow_case(false));
}

/// 観戦者: dll がホスト（席は 2P）で、普通の観戦者が受けた列が、dll と peer_sim の使ったマッチの語（再戦と、メニューへ戻って次の選択に入る seg をまたぐ 5 つの seg）と
/// 全行一致する。seg の始まり・SELECT・終わりが欠けたり順が違ったりせず、観戦者が 2 人でも流れ、観戦のパケットが対戦のセッションへ回らない。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn spectators_of_the_dll_host_see_the_same_match_as_the_dll_and_peer_sim() {
    let out = run(Case {
        spectators: vec![Behavior::Normal, Behavior::Normal],
        ..follow_case(true)
    });
    assert_eq!(out.spectated.len(), 2);
    let snap = out.snapshot.expect("観戦の集計");
    assert_eq!(snap.host.joined, 2, "{snap:?}");
    assert_eq!((snap.ring.gaps, snap.ring.dropped_full), (0, 0), "{snap:?}");
}

/// 観戦が dll の役割に依らない（ゲストの席・ゲストの STEP の経路でも積む。席の取り違えが無い）。dll がゲスト 1P・peer_sim がホスト。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn spectators_of_the_dll_guest_see_the_same_match_as_the_dll_and_peer_sim() {
    run(Case {
        spectators: vec![Behavior::Normal],
        ..follow_case(false)
    });
}

/// 対戦者への遅延の増分 0: 観戦者が黙る・ACK を返さない・途中で消えるとき、dll の使った列・delay の列が観戦者なしの実行と変わらず、
/// stall が観戦者なしより増えない（ゲームのスレッドが観戦者を待たない・受信スレッドが詰まらない）。普通の観戦者は巻き込まれず、
/// 黙った観戦者への送り直しはある。観戦者なしの実行と観戦者 4 人（普通・ACK なし・黙る・40 記録で消える）の実行を続けて走らせて比べる。
/// stall は実時間の値（peer_sim の読み込み 300 ms を待つ分が大半）なので、増えた分が読み込みの揺らぎより大きいかで見る。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn silent_slow_and_vanishing_spectators_do_not_change_the_dll_or_its_stalls() {
    let base = run(fixed_case(true));
    let spec = run(Case {
        spectators: vec![
            Behavior::Normal,
            Behavior::NoAck,
            Behavior::Silent,
            Behavior::Vanish(40),
        ],
        ..fixed_case(true)
    });
    assert_eq!(spec.played, base.played, "dll の使った列が観戦者で変わった");
    assert_eq!(spec.delays, base.delays, "delay の列が観戦者で変わった");
    eprintln!(
        "stall: 観戦者なし {} 回（最長 {}）/ 観戦者 4 人 {} 回（最長 {}）",
        base.dll_stalls, base.dll_max_stall_run, spec.dll_stalls, spec.dll_max_stall_run
    );
    assert!(
        spec.dll_stalls <= base.dll_stalls + STALL_JITTER
            && spec.dll_max_stall_run <= base.dll_max_stall_run + STALL_JITTER as u32,
        "観戦者ありで stall が増えた: {} 回（最長 {}）> {} 回（最長 {}）+ {STALL_JITTER}",
        spec.dll_stalls,
        spec.dll_max_stall_run,
        base.dll_stalls,
        base.dll_max_stall_run
    );
    for (b, seen) in &spec.spectated {
        match b {
            Behavior::Normal => {}
            Behavior::NoAck => assert!(
                (1..=crate::spectate_testkit::NOACK_WINDOW).contains(&seen.recs.len()),
                "ACK を返さない観戦者が送りのウィンドウを越えて受けた: {}",
                seen.recs.len()
            ),
            Behavior::Silent => assert_eq!(seen.datagrams, 0),
            Behavior::Vanish(n) => assert!(seen.recs.len() >= *n),
        }
    }
    let snap = spec.snapshot.expect("観戦の集計");
    assert!(snap.host.resends > 0, "{snap:?}");
    assert_eq!(snap.host.joined, 4, "{snap:?}");
}

/// 観戦者の上限（`MAX_SPECTATORS`）。上限を下げると、この人数が入れずに検査が落ちる。
const FULL_HOUSE: usize = 10;

/// 観戦者が上限の 10 人全員普通に観戦しても、対戦が観戦者なしの実行と変わらない: dll の使った列・delay の列が同じで、
/// CHECK が一致し続け（ずれれば BYE で終わる）、stall が増えない。全員がマッチの語・SELECT・
/// 再戦をまたぐ seg を対戦者と全行一致で受ける（`run` の中の `assert_normal_observer`）。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_full_house_of_spectators_does_not_change_the_dll_or_its_stalls() {
    let base = run(fixed_case(true));
    let spec = run(Case {
        spectators: vec![Behavior::Normal; FULL_HOUSE],
        ..fixed_case(true)
    });
    assert_eq!(spec.played, base.played, "dll の使った列が観戦者で変わった");
    assert_eq!(spec.delays, base.delays, "delay の列が観戦者で変わった");
    eprintln!(
        "stall: 観戦者なし {} 回（最長 {}）/ 観戦者 {} 人 {} 回（最長 {}）\nCHECK 一致 dll 側 {} / {}",
        base.dll_stalls,
        base.dll_max_stall_run,
        FULL_HOUSE,
        spec.dll_stalls,
        spec.dll_max_stall_run,
        base.checks,
        spec.checks,
    );
    assert!(
        spec.dll_stalls <= base.dll_stalls + STALL_JITTER
            && spec.dll_max_stall_run <= base.dll_max_stall_run + STALL_JITTER as u32,
        "観戦者ありで stall が増えた"
    );
    let snap = spec.snapshot.expect("観戦の集計");
    assert_eq!(snap.host.joined, FULL_HOUSE as u64);
    assert_eq!(snap.host.refused_full, 0, "{snap:?}");
    assert_eq!((snap.ring.gaps, snap.ring.dropped_full), (0, 0), "{snap:?}");
    // 再戦と選択をまたぐ実行でも、全員が対戦者と全行一致する。
    let full = run(Case {
        spectators: vec![Behavior::Normal; FULL_HOUSE],
        ..follow_case(true)
    });
    assert_eq!(full.spectated.len(), FULL_HOUSE);
}

/// stall の回数の揺らぎとして許す幅。同じ PC で観戦者なしを 7 回走らせて stall は 24〜35 回（最長の連続はどれも 15）、観戦者 4 人ありを 6 回走らせて
/// 24〜32 回（最長 14〜15）だった（実測）。観戦者をゲームのスレッドが待てば 1 フレームごとに stall が出て数百回増えるので、揺らぎの最大の差（11）を越える 12 を許す。
const STALL_JITTER: u64 = 12;

/// 選択の画面を持つ偽のゲーム（`menu_tests::MenuGame`。dll がホスト 1P）と、選択の台本で動く peer_sim（`--follow --menu`。ゲスト 2P）で、
/// 画面を移る押下の列・ゲストの要求（キャラの決定・マップ −2）・止める地点での −2 の置き換え・マッチから戻った 2 回目の選択（キャラ選択から）・
/// 同じキャラの色の決まりが両者で一致し、マッチの列が揃い、`on_input` が確保しない。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_sim_menu_plan_walks_the_selection_with_the_dll() {
    menu_plan_walk(false);
}

/// 上の実行の間、観戦者が受けた列が、dll の入った押下（12 個。合わせを含む）・マッチの語（180 行）・選択の tick の数（Menu の記録）・
/// SELECT（止める地点ごとに 1P → 2P。2P は peer_sim が決めたキャラ・色）と全部一致する。観戦の経路は押下の値を変えない。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_spectator_sees_the_selection_presses_and_match_of_the_menu_plan_walk() {
    menu_plan_walk(true);
}

fn menu_plan_walk(spectate: bool) {
    use super::menu_tests::{CHARA_TABLE, MenuGame, act};
    use higanr_core::input_model::keys::{RIGHT, UP, Z};
    use higanr_core::menu_model::chara_value;
    use higanr_core::wire::{Step, StepKind};

    let psock = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let paddr = psock.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(), &[paddr]).expect("通信層を開ける");
    let daddr = net.local_addr().unwrap();
    let plan = peer_sim::menu_plan::parse(concat!(
        "screen=8 wait=40 word=RIGHT\n",
        "screen=8 wait=60 press=decide cursor=4\n",
        "screen=16 wait=20 press=decide value=15\n",
        "screen=8 wait=40 press=decide cursor=2\n",
    ))
    .expect("台本が読める");
    let scfg = SimConfig {
        peer: daddr,
        extra_peers: vec![],
        secret: peer_sim::cli::DEFAULT_SECRET,
        stun: None,
        name: "sim".to_string(),
        session_id: SID,
        role_nonce: 0x0001,
        latency: 2,
        host_seat: Seat::P1,
        seed: 0x51,
        follow: true,
        word_mask: SEND_MASK,
        life: 3,
        poll_ms: 1,
        segs: 1,
        frames_per_seg: 1,
        load_ms: 0,
        lat_script: vec![],
        fault: FaultConfig::default(),
        max_ms: 60_000,
    };
    let sim_thread = thread::spawn(move || sim::run_with_menu(&scfg, psock, Some(&plan)));

    let mut np = NetPlay::new(
        link::Config {
            handshake: handshake::Config {
                session_id: SID,
                role_nonce: 0xF000,
                latency: 2,
                name: Name::new("dll"),
                host_seat: Seat::P1,
            },
            secret: peer_sim::cli::DEFAULT_SECRET,
            peers: higanr_core::candidate::Candidates::from_addrs(&[paddr]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        },
        Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..Config::default()
        },
        &mut net,
    );
    let plan_a = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 12, Z, 1),
        act(0, 8, 12, RIGHT, 1),
        act(0, 8, 3, Z, 1),
        act(1, 8, 9, Z, 1),
        act(1, 16, 12, UP, 1),
        act(1, 16, 3, Z, 1),
    ];
    let mut g = MenuGame::new(0, plan_a, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    g.max_matches = 2;
    g.match_frames = 90;
    let observers = if spectate {
        attach(&mut np, &rx, daddr, SID, &[Behavior::Normal])
    } else {
        Vec::new()
    };

    let t0 = Instant::now();
    let mut next = t0;
    let mut closed = false;
    loop {
        g.tick(&mut np, &mut net);
        if !closed && g.matches >= 2 && g.cur == 1 && g.screen == 8 && g.plan_done() {
            np.close(ByeReason::Finished);
            closed = true;
        }
        if closed && sim_thread.is_finished() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(40),
            "終わらない: {:?} {:?} {:?} 画面 {:?}",
            np.phase(),
            np.end(),
            np.stats(),
            g.screens
        );
        next += Duration::from_micros(FRAME_US);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
    let rep = sim_thread
        .join()
        .expect("peer_sim のスレッドがパニックしない")
        .expect("peer_sim が走る");
    let spectated: Vec<Seen> = observers
        .into_iter()
        .map(|o| {
            o.wait_end(Duration::from_secs(5));
            o.finish()
        })
        .collect();
    let rs = rx.stop();
    let summary = peer_sim::record::summary(&rep);
    assert!(
        matches!(rep.end, sim::End::Finished { peer_bye: true }),
        "peer_sim {:?}\n{summary}",
        rep.end
    );
    let st = |step: u16, kind: StepKind, value: u8, screen: u8| Step {
        step,
        kind,
        value,
        screen,
    };
    let d = StepKind::Decide;
    let al = StepKind::Align;
    let expected = vec![
        st(0, al, 1, 7),
        st(1, d, 1, 7),
        st(2, al, 0x83, 8),
        st(3, d, chara_value(0, false, 4), 8),
        st(4, d, chara_value(1, false, 4), 8),
        st(5, al, 14, 16),
        st(6, d, 15, 16),
        st(7, al, 0x44, 8),
        st(8, d, chara_value(0, false, 4), 8),
        st(9, d, chara_value(1, false, 2), 8),
        st(10, al, 14, 16),
        st(11, d, 13, 16),
    ];
    let dll_steps: Vec<Step> = np.step_log().iter().map(|r| r.step).collect();
    assert_eq!(dll_steps, expected, "dll の入った押下\n{summary}");
    assert_eq!(rep.presses, expected, "peer_sim の入った押下\n{summary}");
    assert_eq!(g.ignored_presses, 0, "偽のゲームが読まなかった押下");
    assert_eq!(g.agreed.len(), 2, "{summary}");
    let (a0, a1) = (g.agreed[0], g.agreed[1]);
    assert_eq!((a0.difficulty, a0.map), (1, 10));
    assert_eq!(
        (a0.peer.chara, a0.peer.color, a0.peer.life),
        (CHARA_TABLE[4], 1, 3)
    );
    assert_eq!(g.decided_chars[0], [CHARA_TABLE[4], CHARA_TABLE[4]]);
    assert_eq!((a1.difficulty, a1.map), (1, 13));
    assert_eq!((a1.peer.chara, a1.peer.color), (CHARA_TABLE[2], 0));
    assert_eq!(g.decided_maps, vec![-2, 13], "ゲームが決めたマップ");
    let last = rep.agreed.expect("peer_sim の揃った値");
    assert_eq!(
        (last.map, last.difficulty, last.peer.chara),
        (13, 1, CHARA_TABLE[4]),
        "{summary}"
    );
    let model = rep
        .menu
        .as_ref()
        .and_then(|m| m.model)
        .expect("peer_sim の模型");
    assert_eq!(
        (model.screen(), model.decided(0), model.decided(1)),
        (8, None, None),
        "{summary}"
    );
    assert_eq!(
        rep.follow,
        Some(sim::FollowCounts {
            next_seg: 0,
            peer_selecting: 2,
            ignored: 0
        }),
        "{summary}"
    );
    let theirs: Vec<(u16, u32, u16, u16)> = rep
        .recs
        .iter()
        .map(|r| (r.seg, r.frame, r.p1, r.p2))
        .collect();
    assert_eq!(g.played, theirs, "dll と peer_sim の試合の列");
    assert_eq!(g.played.len(), 2 * 90);
    assert_eq!(g.on_input_allocs, 0, "on_input が確保した");
    assert_eq!(rs.ring.dropped_full, 0);
    for seen in &spectated {
        let recs: Vec<Record> = seen.recs.iter().map(|(_, r)| *r).collect();
        let f = fold(&recs);
        assert!(
            seen.recs
                .iter()
                .enumerate()
                .all(|(k, (idx, _))| *idx == k as u32),
            "観戦者の通し番号が連続でない"
        );
        assert_eq!(f.steps, expected, "観戦者の押下の列");
        assert_eq!(f.frames, g.played, "観戦者の試合の列");
        assert_eq!(
            f.menus.len() as u64,
            np.stats().menu_frames,
            "選択の tick の数"
        );
        assert_eq!(f.segs, vec![0, 1, 2], "seg の先頭");
        assert_eq!(f.ends, vec![ByeReason::Finished]);
        for (k, r) in recs.iter().enumerate() {
            if matches!(r, Record::Step(_)) {
                assert!(
                    matches!(recs.get(k + 1), Some(Record::Menu { .. })),
                    "Step の次が Menu でない"
                );
            }
        }
        assert_eq!(f.selects.len(), 4);
        for (pair, a) in f.selects.chunks(2).zip(&g.agreed) {
            assert_eq!((pair[0].0, pair[1].0), (Seat::P1, Seat::P2));
            for (_, s) in pair {
                assert_eq!((s.map, s.difficulty), (a.map, a.difficulty));
                assert_eq!(s.rng_seed, a.rng_seed);
            }
            assert_eq!(
                (pair[1].1.chara, pair[1].1.color, pair[1].1.life),
                (a.peer.chara, a.peer.color, a.peer.life)
            );
        }
        let (own, agreed) = np.selects();
        let n = f.selects.len();
        let last: Vec<Record> = f.selects[n - 2..]
            .iter()
            .map(|&(seat, sel)| Record::Select { seat, sel })
            .collect();
        assert_eq!(
            last,
            expected_selects(Seat::P1, own.unwrap(), &agreed.unwrap()),
            "最後の SELECT"
        );
    }
    eprintln!(
        "M2-3 loopback: dll {:?} / peer_sim の押下 {} 個",
        np.stats(),
        rep.presses.len()
    );
}

/// dll がゲスト 1P・peer_sim がホスト 2P（`--follow --menu`）のネットの選択で、dll の押下（難易度・キャラ・マップ）が要求としてホストに入り、
/// 入った押下の列・揃った値・マッチの列が両者で一致する。マップは具体の値だけ（ゲストの −2 は peer_sim のホストが置き換えない）。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn the_dll_as_guest_walks_the_selection_with_a_host_peer_sim() {
    use super::menu_tests::{MenuGame, act};
    use higanr_core::input_model::keys::{RIGHT, UP, Z};
    use higanr_core::wire::Step;

    let psock = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let paddr = psock.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(), &[paddr]).expect("通信層を開ける");
    let daddr = net.local_addr().unwrap();
    let plan = peer_sim::menu_plan::parse(concat!(
        "screen=8 wait=40 word=RIGHT\n",
        "screen=8 wait=60 press=decide cursor=4\n",
        "screen=16 wait=20 press=decide value=12\n",
        "screen=8 wait=40 press=decide cursor=2\n",
    ))
    .expect("台本が読める");
    let scfg = SimConfig {
        peer: daddr,
        extra_peers: vec![],
        secret: peer_sim::cli::DEFAULT_SECRET,
        stun: None,
        name: "sim".to_string(),
        session_id: SID,
        role_nonce: 0xF000,
        latency: 2,
        host_seat: Seat::P2,
        seed: 0x51,
        follow: true,
        word_mask: SEND_MASK,
        life: 3,
        poll_ms: 1,
        segs: 1,
        frames_per_seg: 1,
        load_ms: 0,
        lat_script: vec![],
        fault: FaultConfig::default(),
        max_ms: 60_000,
    };
    let sim_thread = thread::spawn(move || sim::run_with_menu(&scfg, psock, Some(&plan)));
    let mut np = NetPlay::new(
        link::Config {
            handshake: handshake::Config {
                session_id: SID,
                role_nonce: 0x0001,
                latency: 2,
                name: Name::new("dll"),
                host_seat: Seat::P1,
            },
            secret: peer_sim::cli::DEFAULT_SECRET,
            peers: higanr_core::candidate::Candidates::from_addrs(&[paddr]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        },
        Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..Config::default()
        },
        &mut net,
    );
    let plan_a = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 12, Z, 1),
        act(0, 8, 12, RIGHT, 1),
        act(0, 8, 3, Z, 1),
        act(1, 8, 9, Z, 1),
        act(1, 16, 12, UP, 1),
        act(1, 16, 3, Z, 1),
    ];
    let mut g = MenuGame::new(0, plan_a, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    g.max_matches = 2;
    g.match_frames = 90;
    let t0 = Instant::now();
    let mut next = t0;
    let mut closed = false;
    loop {
        g.tick(&mut np, &mut net);
        if !closed && g.matches >= 2 && g.cur == 1 && g.screen == 8 && g.plan_done() {
            np.close(ByeReason::Finished);
            closed = true;
        }
        if closed && sim_thread.is_finished() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(40),
            "終わらない: {:?} {:?} {:?} 画面 {:?}",
            np.phase(),
            np.end(),
            np.stats(),
            g.screens
        );
        next += Duration::from_micros(FRAME_US);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
    let rep = sim_thread
        .join()
        .expect("peer_sim のスレッドがパニックしない")
        .expect("peer_sim が走る");
    let rs = rx.stop();
    let summary = peer_sim::record::summary(&rep);
    assert!(
        matches!(rep.end, sim::End::Finished { peer_bye: true }),
        "peer_sim {:?}\n{summary}",
        rep.end
    );
    assert_eq!(np.connected().map(|c| c.role()), Some(Role::Guest));
    let dll_steps: Vec<Step> = np.step_log().iter().map(|r| r.step).collect();
    assert_eq!(dll_steps, rep.presses, "両者の入った押下\n{summary}");
    let real = dll_steps
        .iter()
        .filter(|s| s.kind != StepKind::Align)
        .count();
    assert_eq!((real, dll_steps.len() - real), (7, 5), "{dll_steps:?}");
    assert_eq!(
        np.step_log().iter().filter(|r| r.own).count(),
        4,
        "{dll_steps:?}"
    );
    assert_eq!(g.agreed.len(), 2, "{summary}");
    assert_eq!((g.agreed[0].map, g.agreed[1].map), (12, 13), "{summary}");
    assert_eq!(g.decided_maps, vec![12, 13], "ゲームが決めたマップ");
    let theirs: Vec<(u16, u32, u16, u16)> = rep
        .recs
        .iter()
        .map(|r| (r.seg, r.frame, r.p1, r.p2))
        .collect();
    assert_eq!(g.played, theirs, "dll と peer_sim の試合の列");
    assert_eq!(g.played.len(), 2 * 90);
    assert_eq!(g.on_input_allocs, 0, "on_input が確保した");
    assert_eq!(rs.ring.dropped_full, 0);
}

struct HeadRun {
    recs: Vec<FrameRec>,
    timings: Vec<FrameTiming>,
    stats: Stats,
}

fn two_dll_run(cap: u32, phase_us: u64, frames: u32) -> [HeadRun; 2] {
    let (s1, s2) = (
        UdpSocket::bind(lo()).unwrap(),
        UdpSocket::bind(lo()).unwrap(),
    );
    let (pa, pb) = (s1.local_addr().unwrap(), s2.local_addr().unwrap());
    drop((s1, s2));
    let spawn = |me: SocketAddr, peer: SocketAddr, nonce: u64, start_us: u64, seed: u64| {
        thread::spawn(move || {
            let (mut net, rx) = net_udp::open(me, &[peer]).expect("通信層を開ける");
            let lc = link::Config {
                handshake: handshake::Config {
                    session_id: SID,
                    role_nonce: nonce,
                    latency: 1,
                    name: Name::new("d"),
                    host_seat: Seat::P2,
                },
                secret: peer_sim::cli::DEFAULT_SECRET,
                peers: higanr_core::candidate::Candidates::from_addrs(&[peer]).unwrap(),
                key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
            };
            let mut np = NetPlay::new(
                lc,
                Config {
                    check: CheckSource::InputDigest,
                    head_wait_cap_us: cap,
                    ..Config::default()
                },
                &mut net,
            );
            let mut g = FakeGame::new(seed, frames, 1, 1);
            g.menu_before_decide = 30;
            thread::sleep(Duration::from_micros(start_us));
            let t0 = Instant::now();
            let mut next = t0;
            let (mut closed, mut after_end) = (false, 0u32);
            loop {
                np.head_wait(&mut net);
                g.begin();
                let a = np.on_input(&mut g, &mut net);
                g.after(a, &mut np);
                if g.done && !closed {
                    np.close(ByeReason::Finished);
                    closed = true;
                }
                if np.phase() == Phase::Ended {
                    after_end += 1;
                    if after_end > 40 {
                        break;
                    }
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(40),
                    "終わらない: {:?} {:?} {:?}",
                    np.phase(),
                    np.end(),
                    np.stats()
                );
                next += Duration::from_micros(FRAME_US);
                while let Some(d) = next.checked_duration_since(Instant::now()) {
                    if d > Duration::from_millis(2) {
                        thread::sleep(d - Duration::from_millis(1));
                    } else {
                        std::hint::spin_loop();
                    }
                }
            }
            rx.stop();
            HeadRun {
                recs: g.recs.clone(),
                timings: g.timings.clone(),
                stats: np.stats(),
            }
        })
    };
    let a = spawn(pa, pb, 0xF000, 0, 0xA);
    let b = spawn(pb, pa, 0x0001, phase_us, 0xB);
    [
        a.join().expect("A のスレッドがパニックしない"),
        b.join().expect("B のスレッドがパニックしない"),
    ]
}

/// 始まりの待ち（別スレッドの 2 本の dll・本物の UDP・実時間）: 始まりの待ちの有無で確定した入力の列が変わらず両者で一致し、先に進む側
/// （B の格子が A より `F + 8 ms` 遅れ、遅れ 1 なので片方が毎フレーム 8 ms 前後待つ形）の待ちが始まりへ移り、待たなくてよい側は始まりで待たない。
/// 待つ側が A か B かは選択の同期が決めるので、切った実行で待った側を見て決める。待ちの起きる形で走っている証拠を先に見る。
#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn head_wait_moves_the_leading_dlls_wait_before_its_input_over_real_udp() {
    const F: u32 = 120;
    let phase_us = FRAME_US + 8_000;
    let off = two_dll_run(0, phase_us, F);
    let on = two_dll_run(HEAD_WAIT_CAP_US, phase_us, F);
    for r in off.iter().chain(on.iter()) {
        assert_eq!(r.recs.len(), F as usize, "{:?}", r.stats);
    }
    assert_eq!(off[0].recs, off[1].recs, "切った実行で両者の列が違う");
    assert_eq!(on[0].recs, on[1].recs, "入れた実行で両者の列が違う");
    assert_eq!(
        on[0].recs, off[0].recs,
        "先頭の待ちで確定した入力の列が変わった"
    );
    let sum = |r: &HeadRun, f: fn(&FrameTiming) -> u32| -> u64 {
        r.timings.iter().map(|t| u64::from(f(t))).sum()
    };
    let wait = |t: &FrameTiming| t.wait_us;
    let head = |t: &FrameTiming| t.head_wait_us;
    let lead = usize::from(sum(&off[1], wait) > sum(&off[0], wait));
    let rest = 1 - lead;
    let (w0l, w0r) = (sum(&off[lead], wait), sum(&off[rest], wait));
    let names = ["A", "B"];
    eprintln!(
        "切った実行: 待った側 {} の入力の後の待ち {w0l} µs（{F} フレーム）/ もう一方 {w0r} µs / 先頭 {} {}",
        names[lead],
        sum(&off[0], head),
        sum(&off[1], head)
    );
    assert!(
        w0l > 120 * 3_000,
        "切った実行で待ちが起きていない（検査が待ちの起きる形でない）: {w0l} µs / {w0r} µs"
    );
    assert!(
        w0r * 4 < w0l,
        "両者が待っている（位相の差が 1 方向でない）: {w0l} / {w0r}"
    );
    assert_eq!(sum(&off[0], head) + sum(&off[1], head), 0);
    assert_eq!(off[0].stats.head_waits + off[1].stats.head_waits, 0);
    let (h1l, w1l, h1r, w1r) = (
        sum(&on[lead], head),
        sum(&on[lead], wait),
        sum(&on[rest], head),
        sum(&on[rest], wait),
    );
    eprintln!(
        "入れた実行: {} の先頭の待ち {h1l} µs + 入力の後の待ち {w1l} µs（先頭で待った {} 回・上限に当たった {} 回）/ もう一方の先頭 {h1r} µs + 入力の後 {w1r} µs",
        names[lead], on[lead].stats.head_waits, on[lead].stats.head_wait_caps
    );
    assert!(
        h1l * 2 > w0l,
        "待った側の待ちが先頭へ移っていない: 先頭 {h1l} / 切った実行の入力の後 {w0l}"
    );
    assert!(w1l * 4 < w0l, "入力の後の待ちが減っていない: {w1l} / {w0l}");
    assert!(
        h1r * 10 < h1l,
        "待たなくてよい側が先頭で待った: {h1r} / {h1l}"
    );
    assert_eq!(on[lead].stats.head_wait_caps, 0, "上限に当たり続けた");
}
