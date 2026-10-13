//! 組み上がったものが動くかの検査: 同じプロセスに 2 つの peer_sim を 2 本のスレッドで loopback に立て、ロス無し・決め打ちのロス・
//! 固定 seed のロス（落とす・遅らせる・重ねる）で走らせる。部品の検査は `higanr_core` の中にある。
//!
//! 合格の不変量:
//! - 両者が全部の seg を終える（接続 → 選択 → lockstep → BYE(終了)）
//! - 両者の (seg, frame, 1P, 2P, delay) の列が一致し、ロス無しの走行の列とも一致する
//! - 列の先頭（各 seg の最初の切替を使うまで）が、台本のワードから手で組み立てた参照と一致する
//!   （両者が同じ誤りをしても、ロスの有無で変わらない誤りでも落ちるように）
//! - latency の切替の要求が全部受け入れられ、壊し方が本当に効いている（落とした・遅らせた・重ねた数が 0 でない）
//! - 選択の台本（`menu_*`）: 両者に入った押下の列が台本から手で書いた列と一致し、揃った SELECT が模型の値で、
//!   マッチの列が一致する（乱れの下でも）。模型に合わない押下では BYE(取消) でやめる
//!
//! 実時間で走る（1 本 5〜10 秒）。検査どうしは並んで実行され、ロス無しの走行は `OnceLock` で 1 回だけ回す。

use std::net::UdpSocket;
use std::sync::OnceLock;
use std::thread;
use std::time::Instant;

use higanr_core::auth::AuthKey;
use higanr_core::handshake::{self, FailReason, ModeWish, Name};
use higanr_core::link::{self, Link, Outlet};
use higanr_core::lockstep::Role;
use higanr_core::menu_model;
use higanr_core::selection::Press;
use higanr_core::stun::QueryState;
use higanr_core::wire::Mode;
use higanr_core::wire::{self, ByeReason, MAX_DATAGRAM, Message, Seat, StepKind};
use peer_sim::cli;
use peer_sim::fault::FaultConfig;
use peer_sim::menu_plan;
use peer_sim::sim::{self, End, LatStep, Rec, Report, SimConfig, SimError};
use peer_sim::stun_fake::StunFake;

// 観戦の組み上がりの検査。`build.bat` が走らせる `--test loopback --ignored` に載せるため、このファイルの子モジュールにしてある
// （`tests/` の直下に別のファイルを置くと、`build.bat` に足すまで回らない）。
#[path = "loopback/spectate.rs"]
mod spectate;

const SEGS: u32 = 2;
const FRAMES: u32 = 150;
const SEED_A: u64 = 0xA11CE;
const SEED_B: u64 = 0xB0B;

/// 台 A（nonce が大きいのでホスト。席は 2P —— 役割と席をわざと入れ違いにして、1P / 2P の並べ違いを見えるようにする）と
/// 台 B（ゲスト、1P）の設定。希望 latency は 2 と 3 なので、始めの delay は 3。
fn configs(fa: FaultConfig, fb: FaultConfig) -> (SimConfig, SimConfig, UdpSocket, UdpSocket) {
    let sa = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let sb = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let base = SimConfig {
        peer: sb.local_addr().unwrap(),
        extra_peers: vec![],
        secret: cli::DEFAULT_SECRET,
        stun: None,
        name: "sim-a".to_string(),
        session_id: 0x5EED_0001,
        role_nonce: 0xF000_0000_0000_0001,
        latency: 2,
        host_seat: Seat::P2,
        seed: SEED_A,
        follow: false,
        word_mask: 0xFFFF,
        life: 10,
        poll_ms: 1,
        segs: SEGS,
        frames_per_seg: FRAMES,
        // 読み込みの長さを両者で違えて、先に読み込みを終えた側の INPUT を相手が読み込み中に受ける形にする。
        load_ms: 30,
        // seg 0 で 3 → 5（増やす）、seg 1 で 5 → 1（減らす）。seg をまたいで delay を引き継ぐ形も通る。
        lat_script: vec![
            LatStep {
                seg: 0,
                frame: 40,
                delay: 5,
            },
            LatStep {
                seg: 1,
                frame: 60,
                delay: 1,
            },
        ],
        fault: fa,
        max_ms: 90_000,
    };
    let b = SimConfig {
        peer: sa.local_addr().unwrap(),
        name: "sim-b".to_string(),
        role_nonce: 0x0000_0000_0000_0002,
        latency: 3,
        host_seat: Seat::P1,
        seed: SEED_B,
        load_ms: 250,
        fault: fb,
        ..base.clone()
    };
    (base, b, sa, sb)
}

fn run_pair(fa: FaultConfig, fb: FaultConfig) -> (Report, Report) {
    let (ca, cb, sa, sb) = configs(fa, fb);
    let ta = thread::spawn(move || sim::run(&ca, sa));
    let tb = thread::spawn(move || sim::run(&cb, sb));
    let ra = ta
        .join()
        .expect("A のスレッドがパニックしない")
        .expect("A が走る");
    let rb = tb
        .join()
        .expect("B のスレッドがパニックしない")
        .expect("B が走る");
    (ra, rb)
}

/// 両者が終え、列が一致し、切替が全部受け入れられたこと。
fn assert_pair_ok(ra: &Report, rb: &Report, tag: &str) {
    for (r, who) in [(ra, "A"), (rb, "B")] {
        assert!(
            matches!(r.end, End::Finished { .. }),
            "{tag} {who}: {:?}\n{}",
            r.end,
            peer_sim::record::summary(r)
        );
        assert_eq!(r.recs.len(), (SEGS * FRAMES) as usize, "{tag} {who}");
        assert_eq!(r.delay_start, Some(3), "{tag} {who}: 希望の大きい方");
    }
    assert_eq!(ra.role, Some(Role::Host), "{tag}");
    assert_eq!(rb.role, Some(Role::Guest), "{tag}");
    assert_eq!(
        (ra.seat, rb.seat),
        (Some(Seat::P2), Some(Seat::P1)),
        "{tag}"
    );
    assert_eq!(
        (ra.peer_name.as_str(), rb.peer_name.as_str()),
        ("sim-b", "sim-a")
    );
    assert_eq!(ra.recs, rb.recs, "{tag}: 両者の列が食い違う");
    assert_eq!(ra.lat_results.len(), 2, "{tag}");
    assert!(
        ra.lat_results.iter().all(|l| l.result.is_ok()),
        "{tag}: {:?}",
        ra.lat_results
    );
    let last0 = ra.recs.iter().rfind(|r| r.seg == 0).unwrap();
    let last1 = ra.recs.last().unwrap();
    assert_eq!((last0.delay, last1.delay), (5, 1), "{tag}");
    for r in [ra, rb] {
        assert_eq!(
            r.lockstep.unwrap().conflicts,
            0,
            "{tag}: 同じフレームに違う語"
        );
    }
}

/// 台本のワードから手で組み立てた参照と、各 seg の最初の切替を使うフレームまでを比べる。比べた数を返す。
///
/// 切替を使うまでは delay がその seg の始めの値 d のまま（使ったフレームの後の delay で見分ける）なので、フレーム f のワードは
/// capture_frame f − d で取ったもの（f < d なら両者とも 0）。切替を使うフレームそのもの（delay が変わった行）も、
/// 取ったのは古い d のときなので同じ式で比べられる。
fn check_reference(recs: &[Rec]) -> usize {
    let mut checked = 0;
    let mut start = 3u8;
    for seg in 0..SEGS as u16 {
        let rows: Vec<&Rec> = recs.iter().filter(|r| r.seg == seg).collect();
        let d = u32::from(start);
        for r in &rows {
            let (host, guest) = if r.frame < d {
                (0, 0)
            } else {
                (
                    sim::word(SEED_A, seg, r.frame - d),
                    sim::word(SEED_B, seg, r.frame - d),
                )
            };
            assert_eq!((r.p1, r.p2), (guest, host), "seg {seg} frame {}", r.frame);
            checked += 1;
            if r.delay != start {
                break;
            }
        }
        start = rows.last().map_or(start, |r| r.delay);
    }
    checked
}

fn baseline() -> &'static (Report, Report) {
    static B: OnceLock<(Report, Report)> = OnceLock::new();
    B.get_or_init(|| run_pair(FaultConfig::default(), FaultConfig::default()))
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn lossless_pair_finishes_and_matches_reference() {
    let (ra, rb) = baseline();
    assert_pair_ok(ra, rb, "ロス無し");
    let n = check_reference(&ra.recs);
    // seg 0 は切替を使う 43 フレーム目まで（40 + 3）、seg 1 は 65 フレーム目まで（60 + 5）。
    assert_eq!(n, 44 + 66, "参照と比べた数");
    for (r, who) in [(ra, "A"), (rb, "B")] {
        // CHECK はフレーム 0, 30, 60, 90, 120 の seg ごとに 5 回（2 seg で 10 回）。ロス無しなら全部比べる。読み込みの短い側が
        // 次の seg の frame 0 の CHECK を、相手がまだ前の seg の最後の数フレームにいる間に送っても、相手は seg + 1 の値として
        // 持っておく（`check.rs`）。
        assert_eq!(r.check_matches, 10, "{who}");
        assert_eq!(r.check_dropped, 0, "{who}");
        assert_eq!(r.end, End::Finished { peer_bye: true }, "{who}");
        assert_eq!(r.faults.dropped + r.faults.delayed + r.faults.dupped, 0);
        assert_eq!(
            r.arrival_to_apply_us.len(),
            (SEGS * FRAMES) as usize - 3 - 5,
            "{who}"
        );
        assert!(r.rtt_ms.is_some(), "{who}: PONG が返っていない");
        let c = r.counts.recv;
        assert_eq!(
            (
                c.bad_tag,
                c.decode_errors,
                c.wrong_session,
                c.foreign,
                c.self_bounce
            ),
            (0, 0, 0, 0, 0),
            "{who}: {c:?}"
        );
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn fixed_loss_pattern_matches_lossless_run() {
    let fa = FaultConfig {
        drop_every: 7,
        drop_run: 3,
        ..FaultConfig::default()
    };
    let fb = FaultConfig {
        drop_every: 5,
        drop_run: 1,
        ..FaultConfig::default()
    };
    let (ra, rb) = run_pair(fa, fb);
    assert_pair_ok(&ra, &rb, "決め打ち");
    assert_eq!(ra.recs, baseline().0.recs, "ロス無しの走行と食い違う");
    assert!(
        ra.faults.dropped > 0 && rb.faults.dropped > 0,
        "落としていない"
    );
}

fn random_fault(seed: u64) -> FaultConfig {
    FaultConfig {
        seed,
        drop_pct: 15,
        delay_pct: 10,
        dup_pct: 5,
        delay_max_ms: 40,
        ..FaultConfig::default()
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn random_loss_seed_1_matches_lossless_run() {
    let (ra, rb) = run_pair(random_fault(1), random_fault(1001));
    assert_pair_ok(&ra, &rb, "乱れ seed 1");
    assert_eq!(ra.recs, baseline().0.recs, "ロス無しの走行と食い違う");
    for r in [&ra, &rb] {
        let f = r.faults;
        assert!(f.dropped > 0 && f.delayed > 0 && f.dupped > 0, "{f:?}");
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn random_loss_seed_2_matches_lossless_run() {
    let (ra, rb) = run_pair(random_fault(2), random_fault(2002));
    assert_pair_ok(&ra, &rb, "乱れ seed 2");
    assert_eq!(ra.recs, baseline().0.recs, "ロス無しの走行と食い違う");
}

/// 付いていく側（`follow`）と、決めたフレーム数で seg を終える側（先に立つ側）を繋ぐ。
/// 先に立つ側は latency を切り替えない（付いていく形の条件。`sim.rs` のモジュール doc）。
fn run_follow(follower_is_host: bool, ff: FaultConfig, poll_ms: u32) -> (Report, Report) {
    let (mut lead, mut fol, sl, sf) = configs(FaultConfig::default(), ff);
    fol.poll_ms = poll_ms;
    lead.load_ms = 0;
    lead.lat_script.clear();
    lead.segs = 3;
    lead.frames_per_seg = 97;
    fol.follow = true;
    fol.lat_script.clear();
    fol.load_ms = 0;
    fol.segs = 1;
    fol.frames_per_seg = 5;
    if follower_is_host {
        std::mem::swap(&mut lead.role_nonce, &mut fol.role_nonce);
    }
    let tl = thread::spawn(move || sim::run(&lead, sl));
    let tf = thread::spawn(move || sim::run(&fol, sf));
    let rl = tl.join().expect("パニックしない").expect("走る");
    let rf = tf.join().expect("パニックしない").expect("走る");
    (rl, rf)
}

/// `follower_sends_drop` は付いていく側の送りを落とす実行。そのときは付いていく側の BYE(終了) が、最初の 1 通も先に立つ側の
/// PING への返し（`LINGER_MS` の間の 2〜3 回）も全部落ちることがあり、先に立つ側は 10 秒の沈黙で `Finished { peer_bye: false }` に
/// なる。付いていく側は相手の BYE を受けてから閉じるので、いつも `peer_bye: true`。
fn assert_follow_ok(rl: &Report, rf: &Report, tag: &str, follower_sends_drop: bool) {
    let sum = |r: &Report| peer_sim::record::summary(r);
    assert_eq!(
        rf.end,
        End::Finished { peer_bye: true },
        "{tag} 付\n{}",
        sum(rf)
    );
    if follower_sends_drop {
        assert!(
            matches!(rl.end, End::Finished { .. }),
            "{tag} 先\n{}",
            sum(rl)
        );
    } else {
        assert_eq!(
            rl.end,
            End::Finished { peer_bye: true },
            "{tag} 先\n{}",
            sum(rl)
        );
    }
    assert_eq!(rl.recs.len(), 3 * 97, "{tag}");
    assert_eq!(
        rf.recs, rl.recs,
        "{tag}: 付いていく側の列が先に立つ側と違う"
    );
    assert_eq!(
        rf.follow,
        Some(sim::FollowCounts {
            next_seg: 2,
            peer_selecting: 0,
            ignored: 0
        }),
        "{tag}"
    );
    assert_eq!(rl.follow, None);
    assert_eq!(rf.check_matches, 12, "{tag}");
    assert_eq!(rf.lockstep.unwrap().conflicts, 0, "{tag}");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn follower_ends_each_seg_where_the_leader_does() {
    let (rl, rf) = run_follow(false, FaultConfig::default(), 20);
    assert_follow_ok(&rl, &rf, "付いていく側がゲスト・受信をまとめる", false);
    assert_eq!(rf.role, Some(Role::Guest));
    let burst = FaultConfig {
        drop_every: 11,
        drop_run: 6,
        ..FaultConfig::default()
    };
    let (rl, rf) = run_follow(true, burst, 1);
    assert_follow_ok(&rl, &rf, "付いていく側がホスト・送りを続けて落とす", true);
    assert_eq!(rf.role, Some(Role::Host));
    assert!(rf.faults.dropped > 100, "{:?}", rf.faults);
}

/// 先に立つ側（自分の時計で進む。dll の役）と、付いていく側（`--follow`。実機の回の peer_sim の役）を delay 2 で繋ぎ、
/// 付いていく側の送りを全部 `one_way_ms` だけ遅らせる。先に立つ側の stall は、付いていく側のワードが枠に間に合わなかった数。
fn run_lagging_follower(one_way_ms: u32) -> (Report, Report) {
    let (mut lead, mut fol, sl, sf) = configs(
        FaultConfig::default(),
        FaultConfig {
            one_way_ms,
            ..FaultConfig::default()
        },
    );
    for c in [&mut lead, &mut fol] {
        c.latency = 2;
        c.load_ms = 0;
        c.lat_script.clear();
    }
    lead.segs = 1;
    lead.frames_per_seg = LAG_FRAMES;
    fol.follow = true;
    fol.segs = 1;
    fol.frames_per_seg = 5;
    let tl = thread::spawn(move || sim::run(&lead, sl));
    let tf = thread::spawn(move || sim::run(&fol, sf));
    let rl = tl.join().expect("パニックしない").expect("走る");
    let rf = tf.join().expect("パニックしない").expect("走る");
    (rl, rf)
}

const LAG_FRAMES: u32 = 180;

/// 一定の遅れの実行で、両者が終え、列が一致し、両者の RTT が片道の遅れ `n` ms を含むこと。
fn assert_lag_ok(rl: &Report, rf: &Report, n: u32) {
    let sum = |r: &Report| peer_sim::record::summary(r);
    for (r, who) in [(rl, "先"), (rf, "付")] {
        assert_eq!(
            r.end,
            End::Finished { peer_bye: true },
            "{n} ms {who}\n{}",
            sum(r)
        );
        assert_eq!(r.delay_start, Some(2), "{n} ms {who}");
        // RTT は片道の遅れ（付いていく側の送り）を 1 回だけ含む: 先に立つ側の PING への PONG、付いていく側の PING。上は 1 ms ごとの
        // 取り出しと送り出しの間隔の分（loopback の実測は数 ms。枠は広めに 60 ms —— 30 ms では、別の建て直しが走る負荷の下で 20 ms の
        // 実行が RTT 59 ms になって落ちた）。遅れが両向きに掛かる壊れ方（RTT が 2 倍）を捕まえるのは 120 ms の実行
        // （2 倍なら 240 ms で枠 120〜180 の外）。
        let rtt = r.rtt_ms.expect("PONG が返った");
        assert!((n..=n + 60).contains(&rtt), "{n} ms {who}: RTT {rtt} ms");
    }
    assert_eq!(rl.one_way_ms, 0);
    assert_eq!(rf.one_way_ms, n);
    assert_eq!(rl.recs.len(), LAG_FRAMES as usize, "{n} ms");
    assert_eq!(rf.recs, rl.recs, "{n} ms: 列が食い違う");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn one_way_delay_beyond_the_delay_window_stalls_the_leader() {
    // 下: 付いていく側のワードは枠に間に合う。stall は OS の起こし遅れの分だけ。枠は 180 フレームのうち 30 まで（別の建て直しが走る
    // 負荷の下で 15 が出て落ちた）。上の側（120 ms）は半分（90）以上を求めるので、30 でも両者は離れている。遅れが両向きに掛かる
    // 壊れ方は RTT の枠が捕まえる。
    let (rl, rf) = run_lagging_follower(20);
    assert_lag_ok(&rl, &rf, 20);
    assert!(
        rl.stalls <= 30,
        "20 ms で stall {}\n{}",
        rl.stalls,
        peer_sim::record::summary(&rl)
    );
    // 上: 4 フレーム（delay 2 ＋ 2）ごとに 120 ms 以上かかるので、枠の半分以上が stall する（実測で 180 フレームのうち 180）。
    // 下限は余裕を見て 4 分の 1。
    let (rl, rf) = run_lagging_follower(120);
    assert_lag_ok(&rl, &rf, 120);
    assert!(
        rl.stalls >= LAG_FRAMES / 4,
        "120 ms で stall {}\n{}",
        rl.stalls,
        peer_sim::record::summary(&rl)
    );
    eprintln!(
        "一定の遅れ 120 ms: 先に立つ側の stall {} 回（{} フレームのうち。最大連続 {}）",
        rl.stalls, LAG_FRAMES, rl.max_stall_run
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn pair_at_240_fps_plays_on_the_faster_frame() {
    const FRAMES_240: u32 = 480;
    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    for c in [&mut ca, &mut cb] {
        c.segs = 1;
        c.frames_per_seg = FRAMES_240;
        c.load_ms = 0;
        c.lat_script.clear();
    }
    let opts = sim::RunOpts {
        fps: 240,
        ..sim::RunOpts::default()
    };
    let ta = thread::spawn(move || sim::run_opts(&ca, sa, opts));
    let tb = thread::spawn(move || sim::run_opts(&cb, sb, opts));
    let ra = ta.join().expect("パニックしない").expect("走る");
    let rb = tb.join().expect("パニックしない").expect("走る");
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        let s = peer_sim::record::summary(r);
        assert_eq!(r.end, End::Finished { peer_bye: true }, "{who}\n{s}");
        assert_eq!(r.fps, 240);
        assert!(s.contains("★検査用: ペース 240 fps"), "{s}");
        // 経過 = 接続と選択（数十 ms）＋ マッチ ＋ 終わりの待ち（`LINGER_MS`）。マッチは 240 fps で 2 秒（stall で延びる）、
        // 60 fps なら 8 秒。
        let play_ms = r.elapsed_ms.saturating_sub(sim::LINGER_MS);
        assert!(
            (2_000..6_000).contains(&play_ms),
            "{who}: 経過 {} ms（終わりの待ちを除いて {play_ms} ms）\n{s}",
            r.elapsed_ms
        );
    }
    assert_eq!(ra.recs.len(), FRAMES_240 as usize);
    assert_eq!(ra.recs, rb.recs, "列が食い違う");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn run_refuses_non_loopback_peer_before_sending() {
    let (mut ca, _, sa, _sb) = configs(FaultConfig::default(), FaultConfig::default());
    // 理由の文: 範囲の外は範囲の名前（`run` は既定の loopback）を、意味の無いアドレスは「どの範囲でも」を言う。
    for (p, why) in [
        ("192.0.2.1:9", "scope = loopback"),
        ("[::ffff:127.0.0.1]:9", "どのネットワークの範囲でも"),
        ("0.0.0.0:9", "どのネットワークの範囲でも"),
    ] {
        ca.peer = p.parse().unwrap();
        let s = sa.try_clone().unwrap();
        match sim::run(&ca, s) {
            Err(SimError::Refused(e)) => assert!(e.contains(why), "{p}: {e}"),
            other => panic!("{p}: 断らなかった: {:?}", other.map(|r| r.end)),
        }
    }
    // 対照: 同じ設定で宛先だけ loopback にすると断らない（ガードが全部を断っているのではない）。相手がいないので繋がらないが、
    // 打ち切りを短くして実行を始めることだけを見る。
    ca.peer = "127.0.0.1:9".parse().unwrap();
    ca.max_ms = 50;
    let r = sim::run(&ca, sa).expect("loopback の宛先は断らない");
    assert_eq!(r.end, End::Deadline);
    assert!(r.counts.sends > 0, "走り出して HELLO を送った");
    assert!(!r.deadline_bye);
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn peer_going_silent_after_connecting_ends_with_disconnect() {
    // B の送りを 120 通目の後から全部落とす（繋がってマッチに入った後。ロス無しの走行で B が送るのは全部で 350 通ほど、
    // 接続と選択は数十通で済む）。
    let fb = FaultConfig {
        drop_after: 120,
        ..FaultConfig::default()
    };
    let (ra, rb) = run_pair(FaultConfig::default(), fb);
    assert_eq!(ra.end, End::Dead, "{}", peer_sim::record::summary(&ra));
    assert_eq!(
        rb.end,
        End::PeerBye(ByeReason::Disconnected),
        "{}",
        peer_sim::record::summary(&rb)
    );
    assert!(
        ra.role.is_some() && !ra.recs.is_empty(),
        "試合に入る前に黙った（検査の前提が崩れた）"
    );
    assert!(ra.recs.len() < (SEGS * FRAMES) as usize);
    assert!(ra.elapsed_ms >= 10_000, "{}", ra.elapsed_ms);
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_peer_of_another_version_is_refused_on_both_sides() {
    let fb = FaultConfig {
        claim_ver: Some(0),
        ..FaultConfig::default()
    };
    let (ra, rb) = run_pair(FaultConfig::default(), fb);
    let now = wire::PROTO_VER;
    assert_eq!(
        ra.end,
        End::HandshakeFailed(FailReason::OtherVersion { theirs: 0 }),
        "{}",
        peer_sim::record::summary(&ra)
    );
    assert_eq!(
        rb.end,
        End::HandshakeFailed(FailReason::OtherVersion { theirs: now }),
        "{}",
        peer_sim::record::summary(&rb)
    );
    for r in [&ra, &rb] {
        assert!(r.role.is_none() && r.recs.is_empty(), "繋がった");
        assert!(
            r.elapsed_ms < handshake::TIMEOUT_MS / 2,
            "時間切れを待った: {} ms",
            r.elapsed_ms
        );
    }
    let sa = peer_sim::record::summary(&ra);
    let sb = peer_sim::record::summary(&rb);
    assert!(sa.contains(&format!("（自分 {now}・相手 0）")), "{sa}");
    assert!(sb.contains(&format!("（自分 0・相手 {now}）")), "{sb}");
    assert!(sa.contains("同じ zip"), "{sa}");
    eprintln!(
        "A（{} ms）: {sa}
B（{} ms）: {sb}",
        ra.elapsed_ms, rb.elapsed_ms
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn unanswered_peer_times_out_in_handshake() {
    // B は A へ何も届けない（100% 落とす）。A は何も受けないので「1 通も届かない」、B は A の HELLO を受けて役割を決めるが、
    // 返しが全部落ちるので「届いたが繋がらない」。
    let fb = FaultConfig {
        drop_pct: 100,
        ..FaultConfig::default()
    };
    let (ra, rb) = run_pair(FaultConfig::default(), fb);
    assert_eq!(ra.end, End::HandshakeFailed(FailReason::NothingReceived));
    assert_eq!(rb.end, End::HandshakeFailed(FailReason::NoHandshake));
    assert!(
        (10_000..12_000).contains(&ra.elapsed_ms),
        "{}",
        ra.elapsed_ms
    );
    for r in [&ra, &rb] {
        assert_eq!((r.retry_window_ms, r.restarts.count), (None, 0));
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn wait_window_keeps_the_handshake_until_a_late_peer_starts() {
    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    for c in [&mut ca, &mut cb] {
        c.segs = 1;
        c.frames_per_seg = 60;
        c.load_ms = 0;
        c.lat_script.clear();
        c.max_ms = 40_000;
    }
    let b_addr = sb.local_addr().unwrap();
    drop(sb);
    let opts = sim::RunOpts {
        wait_ms: Some(15_000),
        ..sim::RunOpts::default()
    };
    let ta = thread::spawn(move || sim::run_opts(&ca, sa, opts));
    thread::sleep(std::time::Duration::from_millis(11_000));
    let sb = UdpSocket::bind(b_addr).expect("B のソケットを同じアドレスで開き直せる");
    let tb = thread::spawn(move || sim::run(&cb, sb));
    let ra = ta.join().expect("パニックしない").expect("走る");
    let rb = tb.join().expect("パニックしない").expect("走る");
    let sum = |r: &Report| peer_sim::record::summary(r);
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        assert_eq!(r.end, End::Finished { peer_bye: true }, "{who}\n{}", sum(r));
    }
    assert_eq!(ra.retry_window_ms, Some(15_000));
    assert_eq!(
        ra.restarts,
        link::Restarts {
            count: 1,
            last: Some((FailReason::NothingReceived, handshake::Restart::Fresh))
        },
        "{}",
        sum(&ra)
    );
    assert!(sum(&ra).contains("作り直した 1 回"), "{}", sum(&ra));
    assert!(ra.elapsed_ms >= 11_000, "{}", ra.elapsed_ms);
    assert_eq!((rb.retry_window_ms, rb.restarts.count), (None, 0));
    assert_eq!(ra.recs.len(), 60);
    assert_eq!(ra.recs, rb.recs, "列が食い違う");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn deadline_sends_one_finished_bye_and_the_follower_closes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    let (a_addr, b_addr) = (sa.local_addr().unwrap(), sb.local_addr().unwrap());
    let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
    let r_addr = relay.local_addr().unwrap();
    for c in [&mut ca, &mut cb] {
        c.peer = r_addr;
        c.load_ms = 0;
        c.lat_script.clear();
    }
    ca.segs = 1;
    ca.frames_per_seg = 100_000;
    ca.max_ms = 3_000;
    cb.follow = true;
    cb.segs = 1;
    cb.frames_per_seg = 5;
    cb.max_ms = 20_000;
    let stop = Arc::new(AtomicBool::new(false));
    // バージョン 5: BYE は封じた便なので、中継（ネットワーク）からは理由が読めない。ヘッダの type（平文）で数えるだけにし、理由は B の終わり方
    // （相手の BYE(終了) を受けて閉じた）で見る。
    let byes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let relay_thread = {
        let (stop, byes) = (stop.clone(), byes.clone());
        thread::spawn(move || {
            relay
                .set_read_timeout(Some(std::time::Duration::from_millis(1)))
                .unwrap();
            let mut buf = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                let Ok((n, from)) = relay.recv_from(&mut buf) else {
                    continue;
                };
                let bytes = &buf[..n];
                let to = if from == a_addr {
                    if bytes.len() > wire::HEADER_LEN
                        && bytes.starts_with(&wire::MAGIC)
                        && bytes[5] == 15
                    {
                        byes.lock().unwrap().push(bytes[5]);
                    }
                    b_addr
                } else if from == b_addr {
                    a_addr
                } else {
                    continue;
                };
                let _ = relay.send_to(bytes, to);
            }
        })
    };
    let ta = thread::spawn(move || sim::run(&ca, sa));
    let tb = thread::spawn(move || sim::run(&cb, sb));
    let ra = ta.join().expect("パニックしない").expect("走る");
    let rb = tb.join().expect("パニックしない").expect("走る");
    stop.store(true, Ordering::Relaxed);
    relay_thread.join().unwrap();
    let sum = |r: &Report| peer_sim::record::summary(r);

    assert_eq!(ra.end, End::Deadline, "{}", sum(&ra));
    assert!(ra.deadline_bye, "{}", sum(&ra));
    assert!(
        sum(&ra).contains("BYE(終了) を 1 通出して終えた"),
        "{}",
        sum(&ra)
    );
    assert!(
        !ra.recs.is_empty(),
        "試合に入る前に打ち切った（検査の前提が崩れた）"
    );
    assert_eq!(byes.lock().unwrap().len(), 1, "A が出した BYE の数");
    // B は相手の BYE(終了) を受けて閉じた（10 秒の沈黙で切れたのではない —— それなら `Dead` で 13 秒を越える）。
    assert_eq!(rb.end, End::Finished { peer_bye: true }, "{}", sum(&rb));
    assert!(rb.elapsed_ms < 9_000, "{}\n{}", rb.elapsed_ms, sum(&rb));
    // 使ったフレームの列は、短い方の長さまで一致する（打ち切りの時点で相手のワードがどこまで届いていたかで長さが違う）。
    let n = ra.recs.len().min(rb.recs.len());
    assert!(n > 0);
    assert_eq!(ra.recs[..n], rb.recs[..n], "列が食い違う");
}

// ================= fault matrix =================

/// 攻撃者の送る毒（1 組）。`key` は本物の鍵（「本物のパケットを途中で書き換えた」形を作るためだけに使う）。
/// 返すのは (宛先, 攻撃者のポートから送るか（false は候補にないポートから）, バイト列, 種類)。
fn poison_set(
    key: &AuthKey,
    sid: u32,
    a: std::net::SocketAddr,
    b: std::net::SocketAddr,
) -> Vec<(std::net::SocketAddr, bool, Vec<u8>)> {
    // バージョン 5: 攻撃者は接続コードの秘密を知っていても、使い捨ての鍵（向きごとの鍵）は知らない。HMAC の便（HELLO）は本物を作れるので、
    // それを途中で書き換えた形を毒にし、封じる便（PING）は当て推量の鍵で封じる。
    let mut buf = [0u8; MAX_DATAGRAM];
    let hello = Message::Hello(wire::Hello {
        proto_ver: wire::PROTO_VER,
        latency: 1,
        name_len: 0,
        mode: wire::Mode::Delay,
        can_rollback: false,
        listable: false,
        role_nonce: 0x0A77_AC4E,
        name: [0; wire::NAME_MAX],
        seat_wish: wire::SeatWish::P1,
        arrival: wire::Arrival::Unknown,
        build: higanr_core::update::BUILD_NUMBER,
        public: [0x77; 32],
    });
    let n = wire::encode(key, sid, &hello, &mut buf);
    let good_hello = buf[..n].to_vec();
    let mut flipped = good_hello.clone();
    flipped[wire::HEADER_LEN + 1] ^= 0x04;
    let truncated = good_hello[..n - 1].to_vec();
    let guess = higanr_core::seal::DirKeys::from_raw([0xA7; 32], [0xA7; 32]);
    let seq = higanr_core::seal::Seq::new();
    let mut sealed = |sid: u32| {
        let ping = Message::Ping { send_time: 42 };
        let sealer = guess.sealer(higanr_core::seal::Sender::Link);
        let n = wire::seal(&sealer, &seq, sid, &ping, &mut buf).expect("封じられる");
        buf[..n].to_vec()
    };
    let other_key = sealed(sid);
    let old = sid ^ 0x0001_0000;
    let old_session = sealed(old);
    let good = sealed(sid);
    vec![
        (a, true, flipped),
        (a, true, truncated),
        (a, true, other_key),
        (a, true, old_session),
        (a, false, good.clone()),
        (b, false, good),
    ]
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn fault_matrix_over_loopback_drops_poisons_and_matches_lossless_run() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    let a_addr = sa.local_addr().unwrap();
    let b_addr = sb.local_addr().unwrap();
    let atk = UdpSocket::bind("127.0.0.1:0").unwrap();
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
    let atk_addr = atk.local_addr().unwrap();
    let stun = StunFake::spawn().unwrap();
    ca.peer = atk_addr;
    ca.extra_peers = vec![a_addr, b_addr];
    ca.stun = Some(stun.addr());
    cb.stun = Some(stun.addr());
    let sid = ca.session_id;

    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU32::new(0));
    let hellos_at_atk = Arc::new(AtomicU32::new(0));
    let attacker = {
        let (stop, sent, hellos) = (stop.clone(), sent.clone(), hellos_at_atk.clone());
        thread::spawn(move || {
            let key = AuthKey::derive(&cli::DEFAULT_SECRET, sid);
            atk.set_nonblocking(true).unwrap();
            let mut rb = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                for (to, from_atk, bytes) in poison_set(&key, sid, a_addr, b_addr) {
                    let s = if from_atk { &atk } else { &stranger };
                    if s.send_to(&bytes, to).is_ok() {
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                while let Ok((n, _)) = atk.recv_from(&mut rb) {
                    if n > 0 {
                        hellos.fetch_add(1, Ordering::Relaxed);
                    }
                }
                thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let ta = thread::spawn(move || sim::run(&ca, sa));
    let tb = thread::spawn(move || sim::run(&cb, sb));
    let ra = ta.join().expect("A がパニックしない").expect("A が走る");
    let rb = tb.join().expect("B がパニックしない").expect("B が走る");
    stop.store(true, Ordering::Relaxed);
    attacker.join().unwrap();

    assert_pair_ok(&ra, &rb, "fault matrix");
    assert_eq!(ra.recs, baseline().0.recs, "ロス無しの走行と食い違う");
    assert_eq!(ra.path, Some(b_addr), "相手でない候補を経路に採った");
    assert_eq!(rb.path, Some(a_addr));
    assert_eq!(
        ra.stun,
        Some(QueryState::Done(a_addr)),
        "STUN で自分のアドレスが取れない"
    );
    assert_eq!(rb.stun, Some(QueryState::Done(b_addr)));
    assert!(stun.answered() >= 2, "偽の応答役が答えていない");
    assert!(
        hellos_at_atk.load(Ordering::Relaxed) > 0,
        "全部の候補へ HELLO を送っていない"
    );
    let rounds = u64::from(sent.load(Ordering::Relaxed)) / 6;
    assert!(
        rounds >= 100,
        "毒を送った回数 {rounds}（効く期間が狭すぎる）"
    );
    let (a, b) = (ra.counts.recv, rb.counts.recv);
    println!(
        "fault matrix: 毒 {rounds} 組 / 攻撃者のアドレスに届いた HELLO {} / A {a:?} / B {b:?}",
        hellos_at_atk.load(Ordering::Relaxed)
    );
    // 走行が終わった後に届いた毒は数えられないので、下限で見る（送った組の数の半分以上 —— 走行の長さの大半）。種類ごとに、
    // 数えた先が取り違えられていないこと（例: 改ざんを decode_errors に数えていない）も見る。
    assert!(
        a.bad_tag >= 3 * rounds / 2,
        "A の bad_tag {a:?}（{rounds} 組）"
    );
    assert!(a.wrong_session >= rounds / 2, "A の wrong_session {a:?}");
    assert!(a.foreign >= rounds / 2, "A の foreign {a:?}");
    assert!(b.foreign >= rounds / 2, "B の foreign {b:?}");
    assert!(a.self_bounce >= 1, "A の跳ね返り {a:?}");
    assert_eq!(a.decode_errors + b.decode_errors, 0, "{a:?} {b:?}");
    assert_eq!(
        (b.bad_tag, b.wrong_session, b.self_bounce),
        (0, 0, 0),
        "{b:?}"
    );
    assert_eq!(
        ra.handshake.wrong_session + ra.handshake.stale_nonce,
        0,
        "{:?}",
        ra.handshake
    );
    assert_eq!(ra.counts.send_refused + rb.counts.send_refused, 0);
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_peer_behind_a_moved_port_connects_by_learning_the_source() {
    for wrong_side_is_a in [true, false] {
        let tag = if wrong_side_is_a {
            "外れたのは A（ホスト）"
        } else {
            "外れたのは B（ゲスト）"
        };
        let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
        let a_addr = sa.local_addr().unwrap();
        let b_addr = sb.local_addr().unwrap();
        let stale = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stale_addr = stale.local_addr().unwrap();
        if wrong_side_is_a {
            ca.peer = stale_addr;
        } else {
            cb.peer = stale_addr;
        }
        let ta = thread::spawn(move || sim::run(&ca, sa));
        let tb = thread::spawn(move || sim::run(&cb, sb));
        let ra = ta.join().expect("A がパニックしない").expect("A が走る");
        let rb = tb.join().expect("B がパニックしない").expect("B が走る");
        assert_pair_ok(&ra, &rb, tag);
        assert_eq!(
            ra.recs,
            baseline().0.recs,
            "{tag}: ロス無しの走行と食い違う"
        );
        let (wrong, wrong_report, right_report, want) = if wrong_side_is_a {
            ("A", &ra, &rb, b_addr)
        } else {
            ("B", &rb, &ra, a_addr)
        };
        assert_eq!(
            wrong_report.path,
            Some(want),
            "{tag}: 経路が学んだ送り元でない"
        );
        let c = wrong_report.counts.recv;
        assert_eq!((c.learned, c.learn_refused), (1, 0), "{tag}: {wrong} {c:?}");
        assert_eq!(
            right_report.counts.recv.learned, 0,
            "{tag}: コードどおりの側が学んだ"
        );
        drop(stale);
    }
}

// ================= 選択の台本（`--menu`） =================
//
// 台 A はホスト 1P、台 B はゲスト 2P（dll がホスト 1P、peer_sim がゲスト 2P の実機の形）。両者とも選択の台本で押し、
// 7 → 6（選択の外）→ 7 → 8 → 16 を通ってから、決めたフレーム数のマッチ（lockstep）に入る。

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
screen=7 press=decide value=3            # step 1: 入った直後はホストがまだ来ていないので捨てられ、30 フレーム後に押し直す
screen=8 word=0x44 hold=5                # LEFT+SHIFT
screen=8 wait=180 press=decide cursor=9  # step 5: 2P が決める（ホストの step 4 より十分後）
";

/// 両者に入るはずの押下の列 (step, kind, value, screen)。value はプロトコルの行の形から手で書いた（キャラ: bit0 = 席、bit1 = Shift、
/// bit2〜5 = カーソル。合わせ: 難易度 = カーソル、キャラ = 1P のカーソル | 2P のカーソル << 4、マップ = 一覧の値。ホストの peer_sim が
/// 出す値は `menu_plan::SIM_ALIGN_*`）。画面に入るたび（選択の外から入り直した 7 も、8 へ移った後も、16 へ移った後も）、
/// その画面の最初の押下より先にホストの合わせが 1 つ入る。
fn menu_expected_presses() -> Vec<(u16, StepKind, u8, u8)> {
    use StepKind::{Align as A, Cancel as C, Decide as D};
    vec![
        (0, A, 2, 7),
        (1, C, 0, 7),
        (2, A, 2, 7),
        (3, D, 3, 7),
        (4, A, 0x64, 8), // 1P のカーソル 4・2P のカーソル 6
        (5, D, 0x08, 8), // 1P・カーソル 2
        (6, C, 0x00, 8), // 1P の取消（カーソル 0）
        (7, D, 0x1E, 8), // 1P・Shift・カーソル 7
        (8, D, 0x25, 8), // 2P・カーソル 9
        (9, A, 14, 16),
        (10, D, 15, 16),
    ]
}

const MENU_SEGS: u32 = 1;
const MENU_FRAMES: u32 = 120;

fn run_menu_pair(fa: FaultConfig, fb: FaultConfig) -> (Report, Report) {
    let (mut ca, mut cb, sa, sb) = configs(fa, fb);
    for c in [&mut ca, &mut cb] {
        c.host_seat = Seat::P1;
        c.lat_script.clear();
        c.segs = MENU_SEGS;
        c.frames_per_seg = MENU_FRAMES;
        c.load_ms = 0;
        c.max_ms = 40_000;
    }
    // 初期ライフを違えて、相手の SELECT が自分の席の値として届いたかを見分ける。
    cb.life = 3;
    let pa = menu_plan::parse(MENU_HOST).expect("ホストの台本が読める");
    let pb = menu_plan::parse(MENU_GUEST).expect("ゲストの台本が読める");
    let ta = thread::spawn(move || sim::run_with_menu(&ca, sa, Some(&pa)));
    let tb = thread::spawn(move || sim::run_with_menu(&cb, sb, Some(&pb)));
    let ra = ta.join().expect("パニックしない").expect("A が走る");
    let rb = tb.join().expect("パニックしない").expect("B が走る");
    (ra, rb)
}

fn menu_baseline() -> &'static (Report, Report) {
    static B: OnceLock<(Report, Report)> = OnceLock::new();
    B.get_or_init(|| run_menu_pair(FaultConfig::default(), FaultConfig::default()))
}

fn assert_menu_ok(ra: &Report, rb: &Report, tag: &str) {
    for (r, who) in [(ra, "A"), (rb, "B")] {
        assert_eq!(
            r.end,
            End::Finished { peer_bye: true },
            "{tag} {who}\n{}",
            peer_sim::record::summary(r)
        );
    }
    assert_eq!((ra.role, rb.role), (Some(Role::Host), Some(Role::Guest)));
    assert_eq!((ra.seat, rb.seat), (Some(Seat::P1), Some(Seat::P2)));
    let got: Vec<(u16, StepKind, u8, u8)> = ra
        .presses
        .iter()
        .map(|s| (s.step, s.kind, s.value, s.screen))
        .collect();
    assert_eq!(got, menu_expected_presses(), "{tag}");
    assert_eq!(ra.presses, rb.presses, "{tag}: 両者の押下の列が違う");

    // 揃った SELECT。キャラはカーソル → キャラの表から手で引いた値: カーソル 7 → 0x0E、カーソル 9 → 0x07。違うキャラなので色は Shift のまま。
    let aa = ra.agreed.expect("A の SELECT が揃った");
    let ab = rb.agreed.expect("B の SELECT が揃った");
    for ag in [&aa, &ab] {
        assert_eq!((ag.difficulty, ag.map), (3, 0xFE), "{tag}");
        assert_eq!(ag.rng_seed, SEED_A as u16, "{tag}: シードはホスト（A）の値");
        assert!(
            !ag.mismatch.map && !ag.mismatch.difficulty,
            "{tag}: {:?}",
            ag.mismatch
        );
    }
    assert_eq!(
        (aa.peer.chara, aa.peer.color, aa.peer.life),
        (0x07, 0, 3),
        "{tag}"
    );
    assert_eq!(
        (ab.peer.chara, ab.peer.color, ab.peer.life),
        (0x0E, 1, 10),
        "{tag}"
    );
    assert_eq!((ab.peer.map, ab.peer.difficulty), (0xFE, 3), "{tag}");

    let ma = ra.menu.as_ref().expect("A の台本の記録");
    let mb = rb.menu.as_ref().expect("B の台本の記録");
    assert_eq!((ma.done, ma.items), (6, 6), "{tag}");
    assert_eq!((mb.done, mb.items), (4, 4), "{tag}");
    assert_eq!((ma.left_to_mode, mb.left_to_mode), (1, 1), "{tag}");
    assert_eq!((ma.selections, mb.selections), (2, 2), "{tag}");
    // ゲストは、画面の合わせが届いてから要求するので、ホストがまだ来ていない間の要求が捨てられて押し直す形は起きない。押し直しの数は
    // 見ない: 機械が混んでフレームが遅れると、ホストの押下が同じ step を先に取ってゲストの要求が捨てられる（ホストを採る決まり）
    // ことがあり、数は実行ごとに変わる。押し直しの働きは `menu_plan` の単体検査が見る。
    assert_eq!(ma.retries, 0, "{tag}: ホストは押し直しを数えない");
    for m in [ma, mb] {
        let md = m.model.expect("模型");
        assert_eq!(
            (md.screen(), md.difficulty(), md.map_item()),
            (16, Some(3), Some(15)),
            "{tag}"
        );
        assert_eq!(
            md.decided(0).map(|d| (d.chara, d.color)),
            Some((0x0E, 1)),
            "{tag}"
        );
        assert_eq!(
            md.decided(1).map(|d| (d.chara, d.color)),
            Some((0x07, 0)),
            "{tag}"
        );
    }
    // MENU のワード: ゲストのワード（B。合わせを待ってから出す）は、キャラの画面の合わせ（step 4）の後の step 5 で A に届く。ホストのワード
    // （A。ホストは待たない）は、方向が立っている間は合わせを出さないので、合わせの前の step 4 の間に B へ届く（実機のゲストなら、
    // 合わせの前のワードは入れずに捨てる）。画面を移るビットは落ちている。
    assert!(
        ma.remote_words.contains(&(5, 0x44)),
        "{tag}: {:?}",
        ma.remote_words
    );
    assert!(
        mb.remote_words.contains(&(4, 0x80)),
        "{tag}: {:?}",
        mb.remote_words
    );
    for (_, w) in ma.remote_words.iter().chain(&mb.remote_words) {
        assert_eq!(
            w & 0x100B,
            0,
            "{tag}: 画面を移るビットが MENU のワードに乗った"
        );
    }

    assert_eq!(ra.recs.len(), (MENU_SEGS * MENU_FRAMES) as usize, "{tag}");
    assert_eq!(ra.recs, rb.recs, "{tag}: 試合の列が違う");
    for r in [ra, rb] {
        assert!(r.check_matches > 0, "{tag}");
        assert_eq!(r.lockstep.unwrap().conflicts, 0, "{tag}");
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn menu_plans_walk_7_8_16_and_play() {
    let (ra, rb) = menu_baseline();
    assert_menu_ok(ra, rb, "ロス無し");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn menu_plans_under_random_loss_match_lossless_run() {
    let (ra, rb) = run_menu_pair(random_fault(3), random_fault(3003));
    assert_menu_ok(&ra, &rb, "乱れ seed 3");
    let (ba, _) = menu_baseline();
    assert_eq!(ra.presses, ba.presses, "ロス無しの走行と押下の列が違う");
    assert_eq!(ra.agreed, ba.agreed, "ロス無しの走行と揃った値が違う");
    assert_eq!(ra.recs, ba.recs, "ロス無しの走行と試合の列が違う");
    for r in [&ra, &rb] {
        let f = r.faults;
        assert!(f.dropped > 0 && f.delayed > 0 && f.dupped > 0, "{f:?}");
    }
}

struct Direct<'a> {
    sock: &'a UdpSocket,
    peer: std::net::SocketAddr,
}

impl Outlet for Direct<'_> {
    fn send(&mut self, to: std::net::SocketAddr, bytes: &[u8]) {
        assert_eq!(to, self.peer, "候補は 1 つなのに別の宛先へ送った");
        let _ = self.sock.send_to(bytes, to);
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn menu_mismatched_screen_ends_with_cancel_bye() {
    for (screen, diverges) in [(8u8, true), (7u8, false)] {
        let (ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
        cb.max_ms = 8_000;
        let pb = menu_plan::parse("screen=8 word=LEFT hold=1\n").unwrap();
        let tb = thread::spawn(move || sim::run_with_menu(&cb, sb, Some(&pb)));

        let guest_addr = ca.peer;
        sa.set_nonblocking(true).unwrap();
        let t0 = Instant::now();
        let now = || t0.elapsed().as_millis() as u32;
        let mut lk = Link::new(
            link::Config::single(
                handshake::Config {
                    session_id: 0x5EED_0001,
                    role_nonce: u64::MAX,
                    latency: 2,
                    name: Name::new("fake-host"),
                    host_seat: Seat::P1,
                },
                cli::DEFAULT_SECRET,
                guest_addr,
                peer_sim::keys::fresh_seed(),
            ),
            now(),
        );
        let mut buf = [0u8; 2048];
        let mut entered = false;
        let mut pressed = false;
        let mut next_frame = 0u32;
        let mut peer_bye = None;
        while now() < 6_000 {
            let n = now();
            let mut out = Direct {
                sock: &sa,
                peer: guest_addr,
            };
            while let Ok((len, from)) = sa.recv_from(&mut buf) {
                lk.on_datagram(from, &buf[..len], n, &mut out);
            }
            lk.poll(n, &mut out);
            if let Some(c) = lk.connected_mut() {
                if !entered {
                    c.enter_selection();
                    entered = true;
                }
                peer_bye = c.liveness().peer_bye();
                if peer_bye.is_some() {
                    break;
                }
                if n >= next_frame {
                    next_frame = n + 16;
                    let press = (!pressed && c.selection().peer_present()).then_some(Press {
                        kind: StepKind::Decide,
                        value: 1,
                        screen,
                    });
                    let fo = c.selection_frame(0, press, true, &mut out, |_| link::SelExit::Stay);
                    pressed |= fo.apply.is_some();
                }
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let rb = tb.join().expect("パニックしない").expect("走る");
        let md = rb.menu.as_ref().and_then(|m| m.model).expect("模型");
        if diverges {
            assert!(
                matches!(
                    rb.end,
                    End::MenuDiverged {
                        mismatch: menu_model::Mismatch::Screen { model: 7, step: 8 },
                        ..
                    }
                ),
                "{}",
                peer_sim::record::summary(&rb)
            );
            assert_eq!(
                peer_bye,
                Some(ByeReason::Cancelled),
                "偽のホストが BYE(取消) を受けた"
            );
            assert_eq!(md.screen(), 7);
        } else {
            assert!(pressed);
            assert_eq!(md.screen(), 8, "{}", peer_sim::record::summary(&rb));
            assert!(!matches!(rb.end, End::MenuDiverged { .. }), "{:?}", rb.end);
        }
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn jittered_one_way_delay_keeps_both_sequences_equal() {
    let jf = |seed| FaultConfig {
        seed,
        one_way_ms: 30,
        jitter_ms: 40,
        ..FaultConfig::default()
    };
    let (ra, rb) = run_pair(jf(11), jf(1011));
    assert_pair_ok(&ra, &rb, "揺らぎ");
    assert_eq!(ra.recs, baseline().0.recs, "ロス無しの走行と食い違う");
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        assert_eq!(
            r.check_matches,
            10,
            "{who}\n{}",
            peer_sim::record::summary(r)
        );
        assert_eq!(r.check_dropped, 0, "{who}");
        assert_eq!(r.one_way_ms, 30, "{who}");
        assert_eq!(r.faults.dropped + r.faults.delayed + r.faults.dupped, 0);
        assert!(
            r.faults.jittered * 10 > r.faults.decided * 9,
            "{who}: 揺らぎを足した送り {} / 運命を決めた送り {}",
            r.faults.jittered,
            r.faults.decided
        );
        eprintln!(
            "揺らぎ 30+0〜40 ms {who}: stall {} 回（{} フレームのうち。最大連続 {}）、RTT {:?} ms",
            r.stalls,
            SEGS * FRAMES,
            r.max_stall_run,
            r.rtt_ms
        );
    }
}

// ================= 遅延とロスを揺らす中継（`--shape`） =================
//
// 本物の花映塚 2 台（dll 同士）の間に挟む中継を、peer_sim 2 本の間に挟んで見る。両者は乱れなしで、中継だけが向きごとに別の乱数で
// 落とす・遅らせる・重ねる・一定の遅れを掛ける。両者は相手を中継のポート（`via_a` / `via_b`）として見る。

fn relay_faults(seed: u64) -> FaultConfig {
    FaultConfig {
        seed,
        one_way_ms: 20,
        drop_pct: 2,
        jitter_ms: 30,
        ..FaultConfig::default()
    }
}

fn run_pair_via_relay(
    a2b: FaultConfig,
    b2a: FaultConfig,
) -> (Report, Report, peer_sim::shape::Report) {
    use peer_sim::shape;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    let va = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let vb = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let args = shape::Args {
        a: sa.local_addr().unwrap(),
        b: sb.local_addr().unwrap(),
        via_a: va.local_addr().unwrap(),
        via_b: vb.local_addr().unwrap(),
        a2b,
        b2a,
        max_sec: 120,
    };
    // A の相手は中継の A 向けのポート、B の相手は B 向けのポート（dll の ini の peer と同じ向き）。
    ca.peer = args.via_a;
    cb.peer = args.via_b;
    let stop = Arc::new(AtomicBool::new(false));
    let st = Arc::clone(&stop);
    let relay = thread::spawn(move || {
        shape::run_with_sockets(&args, va, vb, &st, &mut |_| {}).expect("中継が走る")
    });
    let ta = thread::spawn(move || sim::run(&ca, sa));
    let tb = thread::spawn(move || sim::run(&cb, sb));
    let ra = ta.join().expect("A がパニックしない").expect("A が走る");
    let rb = tb.join().expect("B がパニックしない").expect("B が走る");
    stop.store(true, Ordering::Relaxed);
    let rr = relay.join().expect("中継がパニックしない");
    (ra, rb, rr)
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn relay_with_lag_and_loss_keeps_lockstep_and_checks_equal() {
    let (ra, rb, rr) = run_pair_via_relay(relay_faults(11), relay_faults(12));
    assert_pair_ok(&ra, &rb, "中継越し");
    assert_eq!(ra.recs, baseline().0.recs, "ロス無しの走行と食い違う");
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        assert!(
            r.check_matches > 0,
            "{who}: CHECK が 1 度も比べられていない"
        );
        let c = r.counts.recv;
        assert_eq!(
            c.foreign, 0,
            "{who}: 相手のアドレス以外から受けた（中継のポートが相手のはず）"
        );
        assert_eq!(
            c.bad_tag + c.decode_errors,
            0,
            "{who}: 中継が中身を変えた {c:?}"
        );
    }
    for (c, dir) in [(&rr.a2b, "A→B"), (&rr.b2a, "B→A")] {
        assert!(c.received > 200, "{dir}: 受けた {}", c.received);
        assert!(
            c.dropped > 0 && c.jittered * 10 > c.received * 8,
            "{dir}: 乱れが効いていない {c:?}"
        );
        assert_eq!((c.foreign, c.send_errors), (0, 0), "{dir}: {c:?}");
        // 出したコピーは「受けた − 落とした + 重ねた」を越えない（落とした通を出していれば越える）。下は、止めた時点で遅れの途中だったコピー
        // （最後の数十 ms の送り）の分だけ少ない。
        let expect = c.received - c.dropped + c.dupped;
        assert!(
            c.emitted <= expect && expect - c.emitted <= 15,
            "{dir}: 出した数が合わない（期待 {expect}）{c:?}"
        );
    }
    // 中継が受けた通数は、両者が送った通数とほぼ同じ（終わりの数 ms に相手の最後の送りを読み切らずに止まる分だけ少ない）。
    for (got, sent, who) in [
        (rr.a2b.received, ra.counts.sends, "A"),
        (rr.b2a.received, rb.counts.sends, "B"),
    ] {
        assert!(
            got <= sent && sent - got <= 10,
            "{who} の送り {sent} 通に対し、中継が受けたのは {got} 通"
        );
    }
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        assert!(
            r.rtt_ms.expect("PONG が返った") >= 40,
            "{who}: RTT {:?}",
            r.rtt_ms
        );
    }
    eprintln!("{}", peer_sim::shape::line(rr.elapsed_ms, &rr.a2b, &rr.b2a));
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn relay_dropping_only_b_to_a_leaves_a_deaf_and_b_unanswered() {
    let b2a = FaultConfig {
        drop_pct: 100,
        ..FaultConfig::default()
    };
    let (ra, rb, rr) = run_pair_via_relay(FaultConfig::default(), b2a);
    assert_eq!(ra.end, End::HandshakeFailed(FailReason::NothingReceived));
    assert_eq!(rb.end, End::HandshakeFailed(FailReason::NoHandshake));
    let (x, y) = (rr.a2b, rr.b2a);
    assert!(
        x.received > 0 && x.dropped == 0 && x.emitted == x.received,
        "{x:?}"
    );
    assert!(
        y.received > 0 && y.dropped == y.received && y.emitted == 0,
        "{y:?}"
    );
}

// ================= rollback のモード（`--mode rollback`） =================
//
// 取り決めで rollback に決まった 2 本を、揺らぐ遅れとロスの中継（`--shape`）越しに繋ぐ。latency の切替の台本は持たない
// （rollback のモードでは切替を出さない）。確定した列は、同じ語の delay 方式（delay = D）の列と同じはず。

fn run_mode_pair_via_relay(
    a2b: FaultConfig,
    b2a: FaultConfig,
    wa: ModeWish,
    wb: ModeWish,
) -> (Report, Report, peer_sim::shape::Report) {
    run_mode_pair_via_relay_sync(a2b, b2a, wa, wb, true)
}

fn run_mode_pair_via_relay_sync(
    a2b: FaultConfig,
    b2a: FaultConfig,
    wa: ModeWish,
    wb: ModeWish,
    time_sync: bool,
) -> (Report, Report, peer_sim::shape::Report) {
    use peer_sim::shape;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    ca.lat_script.clear();
    cb.lat_script.clear();
    let va = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let vb = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let args = shape::Args {
        a: sa.local_addr().unwrap(),
        b: sb.local_addr().unwrap(),
        via_a: va.local_addr().unwrap(),
        via_b: vb.local_addr().unwrap(),
        a2b,
        b2a,
        max_sec: 120,
    };
    ca.peer = args.via_a;
    cb.peer = args.via_b;
    let stop = Arc::new(AtomicBool::new(false));
    let st = Arc::clone(&stop);
    let relay = thread::spawn(move || {
        shape::run_with_sockets(&args, va, vb, &st, &mut |_| {}).expect("中継が走る")
    });
    let opts = |mode| sim::RunOpts {
        mode,
        time_sync,
        ..sim::RunOpts::default()
    };
    let (oa, ob) = (opts(wa), opts(wb));
    let ta = thread::spawn(move || sim::run_opts(&ca, sa, oa));
    let tb = thread::spawn(move || sim::run_opts(&cb, sb, ob));
    let ra = ta.join().expect("A がパニックしない").expect("A が走る");
    let rb = tb.join().expect("B がパニックしない").expect("B が走る");
    stop.store(true, Ordering::Relaxed);
    let rr = relay.join().expect("中継がパニックしない");
    (ra, rb, rr)
}

/// delay が seg を通して `d` のままの実行の参照（台本のワードから手で組み立てる。切替が無いので全部の行を見る）。
/// ホスト（A）は 2P、ゲスト（B）は 1P。フレーム f のワードは capture_frame f − d で取ったもの（f < d なら 0）。
fn assert_fixed_delay_reference(recs: &[Rec], d: u8, tag: &str) {
    let d32 = u32::from(d);
    let mut n = 0;
    for seg in 0..SEGS as u16 {
        let rows: Vec<&Rec> = recs.iter().filter(|r| r.seg == seg).collect();
        assert_eq!(rows.len(), FRAMES as usize, "{tag}: seg {seg} の行数");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.frame, i as u32, "{tag}: seg {seg} のフレームの並び");
            let (host, guest) = if r.frame < d32 {
                (0, 0)
            } else {
                (
                    sim::word(SEED_A, seg, r.frame - d32),
                    sim::word(SEED_B, seg, r.frame - d32),
                )
            };
            assert_eq!(
                (r.p1, r.p2),
                (guest, host),
                "{tag}: seg {seg} frame {}",
                r.frame
            );
            assert_eq!(r.delay, d, "{tag}: 記録の delay の欄");
            n += 1;
        }
    }
    assert_eq!(n, (SEGS * FRAMES) as usize, "{tag}");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn rollback_pair_over_a_jittering_relay_confirms_the_same_rows() {
    // 向きごとに片道 20 ms ＋ コピー 1 通ごとに 0〜30 ms の揺らぎ ＋ 2 % のロス（`relay_faults`）。D = 1（17 ms）より長い片道なので、
    // 相手のワードはたいてい間に合わず予測で進む。
    let (ra, rb, rr) = run_mode_pair_via_relay(
        relay_faults(21),
        relay_faults(22),
        ModeWish::rollback(1),
        ModeWish::rollback(4),
    );
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        assert!(
            matches!(r.end, End::Finished { .. }),
            "{who}: {:?}\n{}",
            r.end,
            peer_sim::record::summary(r)
        );
        assert_eq!(r.mode, Some(Mode::Rollback), "{who}");
        assert_eq!(r.delay_start, Some(1), "{who}: D はホスト（A）の値");
        assert!(
            r.check_matches > 0,
            "{who}: CHECK が 1 度も比べられていない"
        );
        let s = r.rollback.expect("rollback の数");
        assert_eq!(s.faults, 0, "{who}: {s:?}");
        assert!(s.max_ahead_seen <= 8, "{who}: N − F が R を越えた {s:?}");
        eprintln!(
            "rollback 中継越し {who}: stall {} 回（{} フレームのうち）/ {s:?}",
            r.stalls,
            SEGS * FRAMES
        );
    }
    // 予測と進め直しは、少なくとも片方で本当に起きた（揺らぎを掛けないと 0 のまま通る）。両方ではない: 読み込みの短い A
    // （`configs` の load_ms 30 と 250）が先に seg に入り、R まで先へ進んだまま止まり止まり走るので、B には A のワードがいつも先に届く。
    // 時刻合わせで揃うが、片道 20 ms は D = 1（17 ms）より長いので、揃った後も予測は残る。
    let (sa, sb) = (ra.rollback.unwrap(), rb.rollback.unwrap());
    assert!(
        sa.live_predicted + sb.live_predicted > 0
            && sa.resims + sb.resims > 0
            && sa.resim_confirmed_ticks + sb.resim_confirmed_ticks > 0,
        "予測と進め直しが起きていない A {sa:?} / B {sb:?}"
    );
    assert_eq!(ra.role, Some(Role::Host));
    assert_eq!(ra.recs, rb.recs, "両者の確定の列が食い違う");
    assert_fixed_delay_reference(&ra.recs, 1, "rollback");
    for (c, dir) in [(&rr.a2b, "A→B"), (&rr.b2a, "B→A")] {
        assert!(
            c.dropped > 0 && c.jittered * 10 > c.received * 8,
            "{dir}: 乱れが効いていない {c:?}"
        );
    }
    eprintln!("{}", peer_sim::shape::line(rr.elapsed_ms, &rr.a2b, &rr.b2a));
}

fn predicted_share(s: &higanr_core::rollback::Stats) -> (u32, u32) {
    (
        s.live_predicted,
        s.live_confirmed + s.live_unconfirmed_real + s.live_predicted,
    )
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn rollback_time_sync_lowers_the_early_starters_prediction_over_loopback() {
    let mut share = Vec::new();
    for sync in [false, true] {
        let (ra, rb, _) = run_mode_pair_via_relay_sync(
            FaultConfig::default(),
            FaultConfig::default(),
            ModeWish::rollback(1),
            ModeWish::rollback(1),
            sync,
        );
        for (r, who) in [(&ra, "A"), (&rb, "B")] {
            assert!(
                matches!(r.end, End::Finished { .. }),
                "sync={sync} {who}: {:?}\n{}",
                r.end,
                peer_sim::record::summary(r)
            );
            assert_eq!(r.mode, Some(Mode::Rollback), "{who}");
            assert!(r.check_matches > 0, "sync={sync} {who}");
        }
        assert_eq!(ra.recs, rb.recs, "sync={sync}: 両者の確定の列が食い違う");
        assert_fixed_delay_reference(&ra.recs, 1, "rollback 時刻合わせ");
        let (sa, sb) = (ra.rollback.unwrap(), rb.rollback.unwrap());
        assert_eq!((sa.faults, sb.faults), (0, 0));
        let (pa, na) = predicted_share(&sa);
        let (pb, nb) = predicted_share(&sb);
        eprintln!(
            "sync={sync}: A(先) 予測 {pa} / 入力の地点の更新 {na}・進め直しの予測の更新 {}・時刻合わせの待ち {}・stall {} / B(後ろ) 予測 {pb} / {nb}・待ち {}・stall {}",
            sa.resim_predicted_ticks, sa.waits_sync, ra.stalls, sb.waits_sync, rb.stalls
        );
        if sync {
            assert!(sa.waits_sync > 0, "先の側が待っていない {sa:?}");
        } else {
            assert_eq!((sa.waits_sync, sb.waits_sync), (0, 0));
        }
        share.push((pa, na, sa.resim_predicted_ticks));
    }
    let ((p0, n0, r0), (p1, n1, r1)) = (share[0], share[1]);
    assert!(
        u64::from(p1) * u64::from(n0) < u64::from(p0) * u64::from(n1),
        "予測の割合が減っていない: なし {p0} / {n0}、あり {p1} / {n1}"
    );
    assert!(
        r1 < r0,
        "予測の進め直しの更新が減っていない: なし {r0}、あり {r1}"
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn rollback_wish_against_a_delay_peer_plays_delay() {
    let (ra, rb, _) = run_mode_pair_via_relay(
        FaultConfig::default(),
        FaultConfig::default(),
        ModeWish::rollback(1),
        ModeWish::DELAY,
    );
    for (r, who) in [(&ra, "A"), (&rb, "B")] {
        assert!(matches!(r.end, End::Finished { .. }), "{who}: {:?}", r.end);
        assert_eq!(r.mode, Some(Mode::Delay), "{who}");
        assert_eq!(r.delay_start, Some(3), "{who}: 希望 latency の大きい方");
        assert_eq!(r.rollback, None, "{who}");
    }
    assert_eq!(ra.recs, rb.recs);
    assert_fixed_delay_reference(&ra.recs, 3, "delay に落ちた");
}

// ================= 付いていく形の rollback のモード（`--follow --mode rollback`） =================
//
// seg の長さを決める側（`--frames`。dll の役）と、付いていく側（`--follow`。実機の回の peer_sim の役）を、両者が rollback を望んで
// 繋ぐ。壊すのは付いていく側の送りだけ（決める側から付いていく側への向きは dll → peer_sim の向きで、delay 方式の付いていく形の
// 検査もそうしている）。

struct RbFollow {
    /// 付いていく側をホストにするか（役と席の入れ違い）。
    follower_is_host: bool,
    lead_frames: u32,
    /// 決める側の読み込みの長さ（次の seg の INPUT が最後のワードより後に届く間隔）。
    lead_load_ms: u32,
    ff: FaultConfig,
}

const RB_FOLLOW_SEGS: u32 = 3;

fn run_rollback_follow(o: &RbFollow) -> (Report, Report) {
    let (mut lead, mut fol, sl, sf) = configs(FaultConfig::default(), o.ff);
    lead.load_ms = o.lead_load_ms;
    lead.lat_script.clear();
    lead.segs = RB_FOLLOW_SEGS;
    lead.frames_per_seg = o.lead_frames;
    fol.follow = true;
    fol.lat_script.clear();
    fol.load_ms = 0;
    fol.segs = 1;
    fol.frames_per_seg = 5;
    if o.follower_is_host {
        std::mem::swap(&mut lead.role_nonce, &mut fol.role_nonce);
    }
    // 決める側は時刻合わせを切る: 先へ進めて（R まで）予測させ、seg の最後のフレームの更新が予測になる形を作る（そうでない実行では、
    // 決める側が seg の最後を待つ間に次のフレームを取る不具合を通り抜ける）。
    let lead_opts = sim::RunOpts {
        mode: ModeWish::rollback(1),
        time_sync: false,
        ..sim::RunOpts::default()
    };
    let fol_opts = sim::RunOpts {
        mode: ModeWish::rollback(1),
        ..sim::RunOpts::default()
    };
    let tl = thread::spawn(move || sim::run_opts(&lead, sl, lead_opts));
    let tf = thread::spawn(move || sim::run_opts(&fol, sf, fol_opts));
    let rl = tl.join().expect("パニックしない").expect("走る");
    let rf = tf.join().expect("パニックしない").expect("走る");
    (rl, rf)
}

/// 両者が終え、確定の列が一致し、台本のワードから手で組んだ参照（D = 1。決める側 = A = 2P・SEED_A、付いていく側 = 1P・SEED_B）と
/// 一致し、付いていく側が次の seg の INPUT で 2 回 seg を進めたこと。`lead_holds` は、決める側が「最後のフレームの更新を予測で
/// 進めて待ち、確定して seg を出た」数（`Stats::holds_ended`）の下限。全部の seg の終わりがこの形になった実行だけを見る。
///
/// 形（`holds_ended >= lead_holds`）は、ここでは確かめず決める側の Stats を返す。決める側のスレッドが約 13 ms 遅れると最後の
/// フレームの待ちが出ない実行があるので、形の判定は `run_rb_follow_until_shape` が実行し直しで行う（表明は緩めない）。
fn assert_rb_follow_ok(
    rl: &Report,
    rf: &Report,
    o: &RbFollow,
    tag: &str,
) -> higanr_core::rollback::Stats {
    let sum = |r: &Report| peer_sim::record::summary(r);
    for (r, who) in [(rl, "決"), (rf, "付")] {
        assert!(
            matches!(r.end, End::Finished { .. }),
            "{tag} {who}: {:?}\n決める側の行 {} 付いていく側の行 {}\n付いていく側\n{}",
            r.end,
            rl.recs.len(),
            rf.recs.len(),
            sum(rf)
        );
        assert_eq!(r.mode, Some(Mode::Rollback), "{tag} {who}");
        assert_eq!(r.delay_start, Some(1), "{tag} {who}: D はホストの値");
        assert_eq!(r.rollback.expect("rollback の数").faults, 0, "{tag} {who}");
    }
    assert_eq!(
        rf.end,
        End::Finished { peer_bye: true },
        "{tag}\n{}",
        sum(rf)
    );
    let frames = o.lead_frames;
    assert_eq!(
        rl.recs.len(),
        (RB_FOLLOW_SEGS * frames) as usize,
        "{tag} 決める側の行数"
    );
    assert_eq!(rf.recs, rl.recs, "{tag}: 付いていく側の列が決める側と違う");
    for seg in 0..RB_FOLLOW_SEGS as u16 {
        let rows: Vec<&Rec> = rf.recs.iter().filter(|r| r.seg == seg).collect();
        assert_eq!(rows.len(), frames as usize, "{tag}: seg {seg} の行数");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.frame, i as u32, "{tag}: seg {seg} のフレームの並び");
            let (host, guest) = if r.frame < 1 {
                (0, 0)
            } else {
                (
                    sim::word(SEED_A, seg, r.frame - 1),
                    sim::word(SEED_B, seg, r.frame - 1),
                )
            };
            assert_eq!(
                (r.p1, r.p2, r.delay),
                (guest, host, 1),
                "{tag}: seg {seg} frame {}",
                r.frame
            );
        }
    }
    assert_eq!(
        rf.follow,
        Some(sim::FollowCounts {
            next_seg: 2,
            peer_selecting: 0,
            ignored: 0
        }),
        "{tag}"
    );
    assert_eq!(rl.follow, None);
    // CHECK は seg ごとに 30 フレームおき（0, 30, …）。付いていく側の CHECK も確定の列から作るので全部合う。数えるのは付いていく側だけ:
    // 決める側は、遅れて届く付いていく側の最後の CHECK を待たずに終えることがある。
    assert_eq!(
        rf.check_matches,
        RB_FOLLOW_SEGS * frames.div_ceil(30),
        "{tag}: CHECK の比べた数"
    );
    let (sl, sf) = (rl.rollback.unwrap(), rf.rollback.unwrap());
    assert_eq!(
        (sf.live_predicted, sf.resims, sf.live_unconfirmed_real),
        (0, 0, 0),
        "{tag}: 付いていく側が予測した {sf:?}"
    );
    eprintln!(
        "{tag}: 決める側 hold {} / 予測 {} / 進め直し {} / stall {}、付いていく側の送り {:?}",
        sl.holds_set, sl.live_predicted, sl.resims, rl.stalls, rf.faults
    );
    sl
}

const RB_FOLLOW_ATTEMPTS: u32 = 3;

/// `run_rollback_follow` を、決める側が全部の seg の最後のフレームを予測で更新して待つ形（`holds_ended >= RB_FOLLOW_SEGS`）が出るまで
/// 最大 `RB_FOLLOW_ATTEMPTS` 回走らせる。毎回の実行で `assert_rb_follow_ok` と `extra`（行の一致以外の表明）を全部確かめる。
/// 3 回とも足りなければ 3 回分の holds_* を出して落とす。
fn run_rb_follow_until_shape(o: &RbFollow, tag: &str, extra: impl Fn(&Report, &Report)) {
    let mut seen: Vec<String> = Vec::new();
    for attempt in 1..=RB_FOLLOW_ATTEMPTS {
        let (rl, rf) = run_rollback_follow(o);
        let sl = assert_rb_follow_ok(&rl, &rf, o, tag);
        extra(&rl, &rf);
        if sl.holds_ended >= RB_FOLLOW_SEGS {
            return;
        }
        seen.push(format!(
            "{attempt} 回目: holds_set {} / holds_ended {} / holds_released {} / holds_rearmed {}  {sl:?}",
            sl.holds_set, sl.holds_ended, sl.holds_released, sl.holds_rearmed
        ));
    }
    panic!(
        "{tag}: 決める側が全部の seg の最後のフレームを予測で更新して待つ形（holds_ended >= {RB_FOLLOW_SEGS}）に {RB_FOLLOW_ATTEMPTS} 回走っても\
         ならなかった（この検査が見たい形が足りない）\n{}",
        seen.join("\n")
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn rollback_follower_ends_each_seg_where_the_leader_does() {
    // 片道 40 ms の遅れ（付いていく側の送りだけ）で、決める側は相手のワードを待たずに先へ進み、seg の最後を予測で更新する。
    let slow = FaultConfig {
        one_way_ms: 40,
        ..FaultConfig::default()
    };
    for (follower_is_host, lead_frames, lead_load_ms, tag) in [
        (false, 97, 0, "決める側がホスト・97 フレーム・読み込み 0"),
        (
            false,
            151,
            30,
            "決める側がホスト・151 フレーム・読み込み 30",
        ),
        (true, 97, 0, "付いていく側がホスト・97 フレーム"),
    ] {
        let o = RbFollow {
            follower_is_host,
            lead_frames,
            lead_load_ms,
            ff: slow,
        };
        run_rb_follow_until_shape(&o, tag, |_rl, rf| {
            assert_eq!(
                rf.role,
                Some(if follower_is_host {
                    Role::Host
                } else {
                    Role::Guest
                }),
                "{tag}"
            );
        });
    }
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn rollback_follower_survives_a_jittering_lossy_send() {
    let ff = FaultConfig {
        seed: 31,
        one_way_ms: 30,
        jitter_ms: 20,
        drop_pct: 15,
        ..FaultConfig::default()
    };
    let o = RbFollow {
        follower_is_host: false,
        lead_frames: 121,
        lead_load_ms: 30,
        ff,
    };
    run_rb_follow_until_shape(&o, "揺れ・落とし", |_rl, rf| {
        assert!(
            rf.faults.dropped > 0 && rf.faults.jittered > 0,
            "乱れが効いていない {:?}",
            rf.faults
        );
    });
}
