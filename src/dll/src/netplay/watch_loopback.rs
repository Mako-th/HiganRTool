//! 観戦者の側の組み上がりの検査のうち、本物の UDP の上（loopback）で実時間に走るもの。dll の観戦者（`WatchPlay` ＋
//! 本物の通信層 `net_udp`）を、
//! - peer_sim のホスト（`--spectate-host`。選択の台本つき）と peer_sim のゲストに繋ぐ（`a_dll_watcher_follows_peer_sim_players_*`）、
//! - dll のホスト（偽のゲーム）と peer_sim のゲストの対戦に繋ぎ、観戦者への遅れを測り、観戦者がいても対戦者の stall が
//!   増えないかを見る（`the_watch_lag_*`・`a_watcher_does_not_add_stalls_to_the_players`）。
//!
//! 実時間で走る（1 本 10〜30 秒）ので `#[ignore]`。全件の回（`build.bat`）が `--ignored` で走らせる。
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use higanr_core::candidate::Candidates;
use higanr_core::handshake::{self, Name};
use higanr_core::input_model::MATCH_SEND_MASK;
use higanr_core::link;
use higanr_core::spectate::WatcherConfig;
use higanr_core::wire::{ByeReason, Seat};
use peer_sim::fault::FaultConfig;
use peer_sim::menu_plan;
use peer_sim::sim::{self, RunOpts, SimConfig};
use peer_sim::spectate_host::SpectateHostOpts;

use super::fake::{FRAME_US, FakeGame};
use super::menu_tests::MenuGame;
use super::watch_tests::{SPEC_SECRET, SPEC_SESSION, watcher_game};
use super::*;
use crate::net_udp;
use crate::spectate_testkit::attach_with_code;
use crate::spectate_watch::{
    WATCH_MARGIN_FRAMES, WatchConfig, WatchEnd, WatchPlay, WatchSlot, WatchStats,
};

const SEED_A: u64 = 0xA11CE;
const SEED_B: u64 = 0xB0B;

fn lo() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// 選択の台本（peer_sim の `tests/loopback.rs` の `MENU_HOST` / `MENU_GUEST` と同じ。あちらは exe の外へ出していないのでコピーした）。
const MENU_HOST: &str = "\
screen=7 press=cancel                    # step 0: 難易度の画面の取消で選択の外へ（両者とも）
screen=8 word=RIGHT+X hold=5             # X は画面を移るビットなので落ち、RIGHT（0x80）だけが届く
screen=8 wait=40 press=decide cursor=2   # step 2: 1P が決める
screen=8 wait=10 press=cancel            # step 3: 決めた 1P の取り消し
screen=8 press=decide cursor=7 shift=1   # step 4: 1P が決め直す
screen=16 press=decide value=15          # step 6: マップ −2
";
const MENU_GUEST: &str = "\
screen=6 enter                           # 自分から次の選択に入る（ホストは Recv::PeerSelecting で付いてくる）
screen=7 press=decide value=3            # step 1
screen=8 word=0x44 hold=5                # LEFT+SHIFT
screen=8 wait=180 press=decide cursor=9  # step 5: 2P が決める（ホストの step 4 より十分後）
";

struct Watched {
    wg: MenuGame,
    wp: WatchPlay,
    /// 繋がった後の tick の確保（0 のはず）。
    allocs_joined: u64,
    local_at_join: Option<u32>,
    leaks: u32,
}

/// 観戦者（偽のゲーム W ＋ 本物の通信層）を、終わる（か 60 秒）まで 60 Hz で回す。
fn drive_menu_watcher(host: SocketAddr, mut wg: MenuGame, wait_budget_us: u32) -> Watched {
    let (mut net, rx) = net_udp::open_for_code(lo()).expect("通信層を開ける");
    let slot = Arc::new(WatchSlot::new());
    slot.offer(WatcherConfig {
        session_id: SPEC_SESSION,
        secret: SPEC_SECRET,
        nonce: 0x1234,
        name: Name::new("dll-watch"),
        host: Candidates::from_addrs(&[host]).expect("loopback"),
        host_public: peer_sim::keys::spectate_host_public(&SPEC_SECRET, SPEC_SESSION),
        key_seed: crate::net_udp::draw_key_seed().expect("OS の乱数が引ける"),
    });
    let mut wp = WatchPlay::new(
        slot,
        WatchConfig {
            wait_budget_us,
            margin_frames: WATCH_MARGIN_FRAMES,
            require_net_selection: true,
        },
    );
    let (mut allocs_joined, mut local_at_join, mut leaks) = (0u64, None, 0u32);
    let t0 = Instant::now();
    let mut next = t0;
    let mut after_end = 0u32;
    loop {
        let joined = wp.is_joined();
        let (calls, alloc) = (wg.local_input_calls, wg.on_input_allocs);
        let in_sel = wg.cur == 1 && matches!(wg.screen, 7 | 8 | 16);
        let in_match = wg.cur == 2;
        let ended = wp.end().is_some();
        let a = wg.tick(&mut wp, &mut net);
        if joined {
            local_at_join.get_or_insert(calls);
            allocs_joined += wg.on_input_allocs - alloc;
            if a == Action::Local && !ended && wp.end().is_none() && (in_sel || in_match) {
                leaks += 1;
            }
        }
        if wp.phase() == Phase::Ended {
            after_end += 1;
            if after_end > 5 {
                break;
            }
        }
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "観戦者が終わらない: {:?} {:?} {:?}",
            wp.phase(),
            wp.end(),
            wp.stats()
        );
        next += Duration::from_micros(FRAME_US);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
    let rs = rx.stop();
    assert_eq!(rs.ring.dropped_full, 0, "観戦者の受信のリングが溢れた");
    Watched {
        wg,
        wp,
        allocs_joined,
        local_at_join,
        leaks,
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_dll_watcher_follows_peer_sim_players_through_selection_and_rematches() {
    let sa = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let sb = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let spec_sock = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let spec_addr = spec_sock.local_addr().unwrap();
    let ca = SimConfig {
        peer: sb.local_addr().unwrap(),
        extra_peers: vec![],
        secret: peer_sim::cli::DEFAULT_SECRET,
        stun: None,
        name: "sim-a".to_string(),
        session_id: 0x5EED_0001,
        role_nonce: 0xF000_0000_0000_0001,
        latency: 2,
        host_seat: Seat::P1,
        seed: SEED_A,
        follow: false,
        word_mask: 0xFFFF,
        life: 10,
        poll_ms: 1,
        segs: 3,
        frames_per_seg: 120,
        load_ms: 0,
        lat_script: vec![],
        fault: FaultConfig::default(),
        max_ms: 60_000,
    };
    let cb = SimConfig {
        peer: sa.local_addr().unwrap(),
        name: "sim-b".to_string(),
        role_nonce: 2,
        latency: 3,
        seed: SEED_B,
        life: 3,
        ..ca.clone()
    };
    let ta = thread::spawn(move || {
        let plan = menu_plan::parse(MENU_HOST).expect("ホストの台本が読める");
        let opts = SpectateHostOpts {
            sock: spec_sock,
            session_id: SPEC_SESSION,
            secret: SPEC_SECRET,
            // 観戦者が式で公開鍵を作る検査（上の `host_public`）なので、固定の種。
            key_seed: peer_sim::keys::spectate_host_seed(&SPEC_SECRET, SPEC_SESSION),
            fault: FaultConfig::default(),
        };
        sim::run_opts(
            &ca,
            sa,
            RunOpts {
                menu: Some(&plan),
                spectate: Some(&opts),
                ..RunOpts::default()
            },
        )
    });
    let tb = thread::spawn(move || {
        let plan = menu_plan::parse(MENU_GUEST).expect("ゲストの台本が読める");
        sim::run_with_menu(&cb, sb, Some(&plan))
    });
    let w = drive_menu_watcher(spec_addr, watcher_game(120, 3, 2), WAIT_BUDGET_US);
    let ra = ta.join().expect("A がパニックしない").expect("A が走る");
    let rb = tb.join().expect("B がパニックしない").expect("B が走る");
    let summary = peer_sim::record::summary(&ra);
    assert!(
        matches!(ra.end, sim::End::Finished { peer_bye: true }),
        "{summary}"
    );

    let rows = |r: &[sim::Rec]| {
        r.iter()
            .map(|x| (x.seg, x.frame, x.p1, x.p2))
            .collect::<Vec<_>>()
    };
    assert_eq!(ra.recs.len(), 360);
    assert_eq!(rows(&ra.recs), rows(&rb.recs));
    assert_eq!(
        w.wg.played.len(),
        360,
        "観戦者が試合を使えていない: 終わり {:?} / {:?} / 画面 {:?}\n{summary}",
        w.wp.end(),
        w.wp.stats(),
        w.wg.screens
    );
    assert_eq!(w.wg.played, rows(&ra.recs), "観戦者の列とホストの列");
    let steps: Vec<_> = w.wp.step_log().iter().map(|s| s.step).collect();
    assert_eq!(steps, ra.presses, "入った押下の列\n{summary}");
    // キャラ選択（8）は合わせない（難易度・マップの合わせだけが入る）。
    assert_eq!(steps.len(), 10);
    assert_eq!(w.wg.watched.len(), 1);
    let [s1, s2] = w.wg.watched[0];
    assert_eq!(
        (s1.chara, s1.color, s1.life, s2.chara, s2.color, s2.life),
        (0x0E, 1, 10, 0x07, 0, 3)
    );
    let aa = ra.agreed.expect("A の揃った値");
    for s in [s1, s2] {
        assert_eq!(
            (s.map, s.difficulty, s.rng_seed),
            (aa.map, aa.difficulty, aa.rng_seed)
        );
    }
    assert_eq!(s1.rng_seed, SEED_A as u16);
    assert_eq!(w.wg.decided_chars, vec![[0x0E, 0x07]]);
    assert_eq!(w.wg.decided_maps, vec![-2]);
    let sp = ra.spectate.expect("観戦の部分の報告");
    assert_eq!(sp.counts.joined, 1, "{sp:?}");
    assert_eq!(
        (
            sp.counts.unexpected,
            sp.counts.bad_tag,
            sp.counts.undecodable
        ),
        (0, 0, 0),
        "{sp:?}"
    );
    assert!(
        sp.counts.left >= 1,
        "観戦者の BYE でホストが枠を空けた: {sp:?}"
    );
    let st: WatchStats = w.wp.stats();
    assert_eq!(
        w.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{st:?}"
    );
    assert_eq!(
        (st.frame_gaps, st.jumps, st.matches_started),
        (0, 0, 3),
        "{st:?}"
    );
    assert_eq!(Some(w.wg.local_input_calls), w.local_at_join);
    assert_eq!(w.leaks, 0);
    assert_eq!(w.allocs_joined, 0);
    assert_eq!(w.wg.ignored_presses, 0);
    eprintln!(
        "M6-3 peer_sim 対 peer_sim + dll 観戦者: 使った {} フレーム、描画だけ {}（最長 {}）、持ち越し {}、余裕の分布 {:?}、最大 {} µs",
        st.played, st.stalls, st.max_stall_run, st.menu_held, st.margin_hist, st.margin_max_us
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_dll_watcher_still_in_a_match_ends_as_host_ended_when_the_host_leaves_the_last_seg() {
    let sa = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let sb = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let spec_sock = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let spec_addr = spec_sock.local_addr().unwrap();
    let ca = SimConfig {
        peer: sb.local_addr().unwrap(),
        extra_peers: vec![],
        secret: peer_sim::cli::DEFAULT_SECRET,
        stun: None,
        name: "sim-a".to_string(),
        session_id: 0x5EED_0002,
        role_nonce: 0xF000_0000_0000_0001,
        latency: 2,
        host_seat: Seat::P1,
        seed: SEED_A,
        follow: false,
        word_mask: 0xFFFF,
        life: 10,
        poll_ms: 1,
        segs: 1,
        frames_per_seg: 120,
        load_ms: 0,
        lat_script: vec![],
        fault: FaultConfig::default(),
        max_ms: 60_000,
    };
    let cb = SimConfig {
        peer: sa.local_addr().unwrap(),
        name: "sim-b".to_string(),
        role_nonce: 2,
        latency: 3,
        seed: SEED_B,
        life: 3,
        ..ca.clone()
    };
    let ta = thread::spawn(move || {
        let plan = menu_plan::parse(MENU_HOST).expect("ホストの台本が読める");
        let opts = SpectateHostOpts {
            sock: spec_sock,
            session_id: SPEC_SESSION,
            secret: SPEC_SECRET,
            // 観戦者が式で公開鍵を作る検査（上の `host_public`）なので、固定の種。
            key_seed: peer_sim::keys::spectate_host_seed(&SPEC_SECRET, SPEC_SESSION),
            fault: FaultConfig::default(),
        };
        sim::run_opts(
            &ca,
            sa,
            RunOpts {
                menu: Some(&plan),
                spectate: Some(&opts),
                ..RunOpts::default()
            },
        )
    });
    let tb = thread::spawn(move || {
        let plan = menu_plan::parse(MENU_GUEST).expect("ゲストの台本が読める");
        sim::run_with_menu(&cb, sb, Some(&plan))
    });
    let w = drive_menu_watcher(spec_addr, watcher_game(u32::MAX, 1, 0), WAIT_BUDGET_US);
    let ra = ta.join().expect("A がパニックしない").expect("A が走る");
    let _ = tb.join().expect("B がパニックしない").expect("B が走る");
    let summary = peer_sim::record::summary(&ra);
    assert_eq!(ra.recs.len(), 120, "{summary}");
    let rows: Vec<_> = ra
        .recs
        .iter()
        .map(|x| (x.seg, x.frame, x.p1, x.p2))
        .collect();
    assert_eq!(
        w.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{:?}\n{summary}",
        w.wp.stats()
    );
    assert_eq!(w.wg.played, rows, "最後のフレームまで使い終える");
    assert_eq!(w.leaks, 0);
}

const SID: u32 = 0x5EED_3001;
const FRAMES: u32 = 120;
/// 付いていく形のマッチの長さ（`loopback.rs` の `FOLLOW_LENGTHS` と同じ）。
const LENGTHS: [u32; 4] = [150, 45, 200, 90];

struct Play {
    recs: Vec<FrameRec>,
    times: Vec<Instant>,
    stalls: u64,
    max_stall_run: u32,
    watch: Option<WatchRun>,
    /// 観戦のホストの数（送った通数・送り直し。観戦者を付けた実行だけ）。
    spec: Option<higanr_core::spectate::HostCounts>,
}

struct WatchRun {
    recs: Vec<FrameRec>,
    times: Vec<Instant>,
    margins: Vec<Option<u32>>,
    stats: WatchStats,
    end: Option<WatchEnd>,
}

/// 走らせ方。`follow` = peer_sim が dll に付いていく形（マッチごとに長さが違い、メニューへ戻って次の選択に入る。485 フレーム。
/// dll の側はいつも語が揃っているので stall は 0）。偽 = peer_sim が先に進む形（3 マッチ 360 フレーム。peer_sim の読み込み
/// 300 ms を dll が待つので stall が出る）。
#[derive(Clone, Copy)]
struct Scenario {
    follow: bool,
}

impl Scenario {
    fn lengths(self) -> Vec<u32> {
        if self.follow {
            LENGTHS.to_vec()
        } else {
            vec![FRAMES; 3]
        }
    }
}

fn fake(seed: u64, sc: Scenario) -> FakeGame {
    let (per_session, sessions) = if sc.follow { (2, 2) } else { (3, 1) };
    let mut g = FakeGame::new(seed, FRAMES, per_session, sessions);
    g.seg_frames = sc.lengths();
    g.menu_before_decide = 30;
    g
}

/// dll のホスト（席は 2P）と peer_sim のゲスト（付いていく形）を実時間で回す。`with_watcher` なら、ホストの観戦のポートに
/// dll の観戦者（別のスレッド。偽のゲームは選択の画面を持たない v0）を繋ぐ。
fn play(with_watcher: bool, sc: Scenario) -> Play {
    let psock = UdpSocket::bind(lo()).expect("loopback に bind できる");
    let paddr = psock.local_addr().unwrap();
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
        follow: sc.follow,
        word_mask: MATCH_SEND_MASK,
        life: 3,
        poll_ms: 1,
        segs: 3,
        frames_per_seg: FRAMES,
        load_ms: if sc.follow { 0 } else { 300 },
        lat_script: vec![],
        fault: FaultConfig::default(),
        max_ms: 60_000,
    };
    let sim_thread = thread::spawn(move || sim::run(&scfg, psock));
    let mut np = NetPlay::new(
        link::Config {
            handshake: handshake::Config {
                session_id: SID,
                role_nonce: 0xF000,
                latency: 3,
                name: Name::new("dll"),
                host_seat: Seat::P2,
            },
            secret: peer_sim::cli::DEFAULT_SECRET,
            peers: Candidates::from_addrs(&[paddr]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        },
        Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            lat_plan: LatPlan::default(),
            require_net_selection: false,
            ..Config::default()
        },
        &mut net,
    );
    let mut g = fake(0xD11, sc);
    let watch_thread = if with_watcher {
        let (_, code_text) = attach_with_code(&mut np, &rx, daddr, SID, &[]);
        let now = net_udp::now_unix();
        let spec =
            higanr_core::code::read_spectate(&code_text, now).expect("観戦用のコードが読める");
        let c = spec.code;
        let slot = Arc::new(WatchSlot::new());
        slot.offer(WatcherConfig {
            session_id: c.session_id,
            secret: c.secret.expect("秘密"),
            nonce: 0x77,
            name: Name::new("dll-watch"),
            host: c.candidates,
            host_public: spec.host_public,
            key_seed: crate::net_udp::draw_key_seed().expect("OS の乱数が引ける"),
        });
        Some(thread::spawn(move || watch_fake(slot, sc)))
    } else {
        None
    };
    if with_watcher {
        let deadline = Instant::now() + Duration::from_secs(5);
        while rx.spectate_snapshot().map_or(0, |s| s.host.joined) < 1 {
            assert!(Instant::now() < deadline, "観戦者が入らない");
            thread::sleep(Duration::from_millis(1));
        }
    }
    let t0 = Instant::now();
    let mut next = t0;
    let mut closed = false;
    let mut times = Vec::new();
    loop {
        let a = g.do_frame(&mut np, &mut net);
        if matches!(a, Action::Played(_)) {
            times.push(Instant::now());
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
    assert!(
        matches!(rep.end, sim::End::Finished { peer_bye: true }),
        "peer_sim {:?}",
        rep.end
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
    assert_eq!(g.recs, theirs, "dll と peer_sim の列");
    let watch = watch_thread.map(|t| t.join().expect("観戦者のスレッドがパニックしない"));
    let spec = rx.spectate_snapshot().map(|s| s.host);
    let _ = rx.stop();
    Play {
        recs: g.recs.clone(),
        times,
        stalls: np.stats().stalls,
        max_stall_run: np.stats().max_stall_run,
        watch,
        spec,
    }
}

/// dll の観戦者を偽のゲーム（v0。マッチごとの長さもホストと同じ）で終わりまで回す。
fn watch_fake(slot: Arc<WatchSlot>, sc: Scenario) -> WatchRun {
    let (mut net, rx) = net_udp::open_for_code(lo()).expect("通信層を開ける");
    let mut wp = WatchPlay::new(
        slot,
        WatchConfig {
            wait_budget_us: WAIT_BUDGET_US,
            margin_frames: WATCH_MARGIN_FRAMES,
            require_net_selection: false,
        },
    );
    let mut g = fake(0xD22, sc);
    let (mut times, mut margins) = (Vec::new(), Vec::new());
    let t0 = Instant::now();
    let mut next = t0;
    let mut after_end = 0u32;
    loop {
        let a = g.do_frame(&mut wp, &mut net);
        if matches!(a, Action::Played(_)) {
            times.push(Instant::now());
            margins.push(wp.last_timing().margin_us);
        }
        if wp.phase() == Phase::Ended {
            after_end += 1;
            if after_end > 5 {
                break;
            }
        }
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "観戦者が終わらない: {:?} {:?} {:?}",
            wp.phase(),
            wp.end(),
            wp.stats()
        );
        next += Duration::from_micros(FRAME_US);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
    let _ = rx.stop();
    WatchRun {
        recs: g.recs.clone(),
        times,
        margins,
        stats: wp.stats(),
        end: wp.end(),
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(i).copied().unwrap_or(0)
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn the_watch_lag_is_a_margin_of_frames_and_the_columns_match() {
    let p = play(true, Scenario { follow: true });
    let w = p.watch.expect("観戦者");
    assert_eq!(w.recs, p.recs, "観戦者の列とホストの列");
    assert_eq!(p.recs.len(), LENGTHS.iter().sum::<u32>() as usize);
    assert_eq!(
        w.end,
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{:?}",
        w.stats
    );
    assert_eq!(w.times.len(), p.times.len());
    let mut lag: Vec<u64> = w
        .times
        .iter()
        .zip(&p.times)
        .map(|(wt, ht)| wt.saturating_duration_since(*ht).as_micros() as u64)
        .collect();
    lag.sort_unstable();
    let mut margin: Vec<u64> = w.margins.iter().flatten().map(|&m| u64::from(m)).collect();
    margin.sort_unstable();
    let s = w.stats;
    eprintln!(
        "観戦者への遅れ（loopback、{} フレーム）: ホストが使ってから観戦者が使うまで p50 {} µs / p95 {} µs / 最大 {} µs。余裕（記録が届いてから使うまで）p50 {} µs / p95 {} µs / 最大 {} µs。観戦者の描画だけのフレーム {} 回（最長の連続 {}）・余裕待ち {} tick・使う直前に溜まっていた記録の最大 {}",
        lag.len(),
        pct(&lag, 0.5),
        pct(&lag, 0.95),
        lag.last().copied().unwrap_or(0),
        pct(&margin, 0.5),
        pct(&margin, 0.95),
        margin.last().copied().unwrap_or(0),
        s.stalls,
        s.max_stall_run,
        s.start_hold_ticks,
        s.backlog_max
    );
    assert!(
        pct(&lag, 0.5) < 150_000,
        "遅れの中央値が大きい: {} µs",
        pct(&lag, 0.5)
    );
    // 実時間の loopback の 1 走行の値（変える前は送った 1,369 通のうち送り直し 877、変えた後は 492 通のうち 0。観戦者 1 人・494 記録）。
    let h = p.spec.expect("観戦のホストの数");
    eprintln!(
        "ホストの観戦の送り（loopback、観戦者 1 人・{} 記録）: 送った {} 通・送り直し {}（送り直し / 送った通数 = {:.3}）",
        h.pushed,
        h.packets_sent,
        h.resends,
        h.resends as f64 / h.packets_sent.max(1) as f64
    );
    assert!(
        h.resends * 20 <= h.packets_sent,
        "送り直しが多い: 送った {} 通のうち {}",
        h.packets_sent,
        h.resends
    );
    assert_eq!((s.frame_gaps, s.jumps), (0, 0), "{s:?}");
    assert_eq!(s.matches_started, 4, "{s:?}");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_watcher_does_not_add_stalls_to_the_players() {
    let sc = Scenario { follow: false };
    let base: Vec<Play> = (0..3).map(|_| play(false, sc)).collect();
    let with: Vec<Play> = (0..3).map(|_| play(true, sc)).collect();
    for p in &with {
        assert_eq!(p.recs, base[0].recs, "対戦者の列が観戦者で変わった");
        // 観戦者が本当に付いていた（付いていなければ「増えない」は何も言っていない）。
        let w = p.watch.as_ref().expect("観戦者");
        assert_eq!(w.recs, p.recs, "観戦者の列");
        assert_eq!(w.end, Some(WatchEnd::HostEnded(ByeReason::Finished)));
    }
    assert!(base.iter().all(|p| p.watch.is_none()));
    let stalls = |v: &[Play]| v.iter().map(|p| p.stalls).collect::<Vec<_>>();
    let runs = |v: &[Play]| {
        v.iter()
            .map(|p| u64::from(p.max_stall_run))
            .collect::<Vec<_>>()
    };
    eprintln!(
        "対戦者の stall（dll のホスト。回数 / 最長の連続）: 観戦者なし {:?} / {:?}、観戦者あり {:?} / {:?}",
        stalls(&base),
        runs(&base),
        stalls(&with),
        runs(&with)
    );
    let (bm, wm) = (
        stalls(&base).into_iter().max().unwrap_or(0),
        stalls(&with).into_iter().max().unwrap_or(0),
    );
    let (br, wr) = (
        runs(&base).into_iter().max().unwrap_or(0),
        runs(&with).into_iter().max().unwrap_or(0),
    );
    assert!(
        wm <= bm + STALL_JITTER && wr <= br + STALL_JITTER,
        "観戦者ありで stall が増えた: {wm} 回（最長 {wr}）> {bm} 回（最長 {br}）+ {STALL_JITTER}"
    );
}

/// stall の回数の揺らぎとして許す幅。`loopback.rs` の `STALL_JITTER`（12）と同じ考え方: 観戦者を対戦者のゲームのスレッドが
/// 待てば 1 フレームごとに stall が出て数百回増えるので、揺らぎの最大の差を越える値。実測では観戦者なし・ありとも
/// 6 走行ずつ全部 24 回（最長 15）で揺れは 0、観戦者なしの別の測定は 24〜35 回で、12 はその最大の差（11）を越える値。
const STALL_JITTER: u64 = 12;
