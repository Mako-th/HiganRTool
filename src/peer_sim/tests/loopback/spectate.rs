//! 観戦の組み上がりの検査: 同じプロセスに peer_sim のホスト（観戦のポートつき）・ゲスト・観戦者を 3 本のスレッドで loopback の実 UDP に
//! 立てて回す。`tests/loopback.rs` の子モジュール（実時間の検査を `build.bat` の `--ignored` の回に載せるため）。
//! 部品の検査は `higanr_core::spectate` の中にある。
//!
//! 合格の不変量:
//! - 観戦者のマッチの語の列が、両対戦者の列と全行一致する（ロス無し・決め打ちのロス・固定 seed の乱れ。観戦者の送りにも掛ける）
//! - SELECT（両席）・選択の押下・seg の始まり・終わりが、対戦者の側の値と順に一致する
//! - 遅れて入った観戦者は、いまの seg の始まり（`Seg(k)`。その後ろにその seg の選択の押下・SELECT が続く。最初の seg はリングバッファの先頭 =
//!   `Seg(0)`）から流れる
//! - 観戦者が黙っても、対戦者の列はロス無しの走行と同じで、stall も増えず、走行の終わりを引き延ばされない
//! - ゲストの役の走行は何も流さない
//!
//! 実時間で走る（1 本 7〜15 秒）。

use std::net::{SocketAddr, UdpSocket};
use std::thread;

use higanr_core::auth::Secret;
use higanr_core::wire::{ByeReason, Seat, StepKind};
use peer_sim::compare_spectate;
use peer_sim::fault::FaultConfig;
use peer_sim::record;
use peer_sim::sim::{self, End, Rec, Report, SimConfig};
use peer_sim::spectate_host::SpectateHostOpts;
use peer_sim::watch::{self, WEvent, WFrame, WatchConfig, WatchEnd, WatchReport};

use super::{FRAMES, SEED_A, SEED_B, SEGS, assert_pair_ok, baseline, configs, random_fault};

/// 観戦用のコードの session・秘密（対戦の `0x5EED_0001`・`cli::DEFAULT_SECRET` とは別の値）。
const SPEC_SESSION: u32 = 0x5EC7_0002;
const SPEC_SECRET: Secret = Secret(*b"spectate-loop-01");

struct Trio {
    a: Report,
    b: Report,
    w: WatchReport,
}

fn watch_cfg(host: SocketAddr) -> WatchConfig {
    WatchConfig {
        host: vec![host],
        session_id: SPEC_SESSION,
        secret: SPEC_SECRET,
        // 配る側（peer_sim の `--spectate-host`）の公開鍵（観戦用のコードを使わない経路の式）。
        host_public: peer_sim::keys::spectate_host_public(&SPEC_SECRET, SPEC_SESSION),
        name: "watcher".to_string(),
        nonce: 0x57A7_C4E2,
        fault: FaultConfig::default(),
        start_after_ms: 0,
        silent_end_ms: 3_000,
        max_ms: 60_000,
    }
}

/// ホスト A（観戦のポートつき）・ゲスト B・観戦者 W を回す。`spec_fault` は観戦者への送りに、`wcfg` の `fault` は観戦者の送りに
/// 掛かる。`spectate_on_guest` なら観戦のポートはゲスト B に付ける（ホストでない走行が何も流さないことの検査）。
fn run_trio(
    fa: FaultConfig,
    fb: FaultConfig,
    spec_fault: FaultConfig,
    edit: impl FnOnce(&mut WatchConfig),
    spectate_on_guest: bool,
) -> Trio {
    run_trio_with(fa, fb, spec_fault, edit, spectate_on_guest, |_, _| {})
}

fn run_trio_with(
    fa: FaultConfig,
    fb: FaultConfig,
    spec_fault: FaultConfig,
    edit: impl FnOnce(&mut WatchConfig),
    spectate_on_guest: bool,
    cfg_edit: impl FnOnce(&mut SimConfig, &mut SimConfig),
) -> Trio {
    let (mut ca, mut cb, sa, sb) = configs(fa, fb);
    cfg_edit(&mut ca, &mut cb);
    let spec_sock = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let host_addr = spec_sock.local_addr().unwrap();
    let opts = SpectateHostOpts {
        sock: spec_sock,
        session_id: SPEC_SESSION,
        secret: SPEC_SECRET,
        // 観戦者（`watch_cfg`）がコードを使わず式で公開鍵を作る検査なので、固定の種（1 回きりの実行だけ）。
        key_seed: peer_sim::keys::spectate_host_seed(&SPEC_SECRET, SPEC_SESSION),
        fault: spec_fault,
    };
    let mut wc = watch_cfg(host_addr);
    edit(&mut wc);
    let wsock = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let (opts_a, opts_b) = if spectate_on_guest {
        (None, Some(opts))
    } else {
        (Some(opts), None)
    };
    let ta = thread::spawn(move || {
        let o = sim::RunOpts {
            spectate: opts_a.as_ref(),
            ..sim::RunOpts::default()
        };
        sim::run_opts(&ca, sa, o)
    });
    let tb = thread::spawn(move || {
        let o = sim::RunOpts {
            spectate: opts_b.as_ref(),
            ..sim::RunOpts::default()
        };
        sim::run_opts(&cb, sb, o)
    });
    let tw = thread::spawn(move || watch::run(&wc, wsock, higanr_core::guard::Scope::Loopback));
    let a = ta.join().expect("A がパニックしない").expect("A が走る");
    let b = tb.join().expect("B がパニックしない").expect("B が走る");
    let w = tw
        .join()
        .expect("観戦者がパニックしない")
        .expect("観戦者が走る");
    Trio { a, b, w }
}

fn rows(recs: &[Rec]) -> Vec<WFrame> {
    recs.iter()
        .map(|r| WFrame {
            seg: r.seg,
            frame: r.frame,
            p1: r.p1,
            p2: r.p2,
        })
        .collect()
}

/// 観戦者の列が対戦者の列と全行同じか。食い違えば、長さと最初の食い違いの行だけを出す（列の全部を出さない）。
fn assert_same_frames(got: &[WFrame], want: &[WFrame], tag: &str) {
    if got == want {
        return;
    }
    let at = got.iter().zip(want).position(|(g, w)| g != w);
    panic!(
        "{tag}: 観戦者 {} 行 / 対戦者 {} 行 / 最初の食い違い {at:?}: 観戦者 {:?} / 対戦者 {:?}",
        got.len(),
        want.len(),
        at.and_then(|i| got.get(i)),
        at.and_then(|i| want.get(i)),
    );
}

/// マッチの語以外の記録の並び（MENU は数が変わるので、種類の並びから除く）。
fn kinds(w: &WatchReport) -> Vec<String> {
    w.events
        .iter()
        .filter(|(_, e)| !matches!(e, WEvent::Menu { .. }))
        .map(|(_, e)| match e {
            WEvent::Seg(s) => format!("seg{s}"),
            WEvent::Select { seat, .. } => {
                format!("select{}", if *seat == Seat::P1 { 1 } else { 2 })
            }
            WEvent::Step(_) => "step".to_string(),
            WEvent::Name { seat, name } => {
                format!(
                    "name{}={}",
                    if *seat == Seat::P1 { 1 } else { 2 },
                    name.as_str()
                )
            }
            WEvent::End(r) => format!("end{r:?}"),
            WEvent::Menu { .. } => unreachable!(),
        })
        .collect()
}

/// 全部の seg を先頭から観戦した走行の、共通の合格条件（ロス無し・乱れの下で同じ）。
fn assert_full_watch(t: &Trio, tag: &str) {
    let (a, b, w) = (&t.a, &t.b, &t.w);
    assert_pair_ok(a, b, tag);
    assert_eq!(
        w.end,
        WatchEnd::HostEnded(ByeReason::Finished),
        "{tag}: {}",
        watch::summary(w)
    );
    assert!(w.joined, "{tag}");
    assert_eq!(w.frames_without_seg, 0, "{tag}");
    assert_eq!(w.jumps, 0, "{tag}: 頭から観戦したのに番号が飛んだ");
    assert_eq!(w.frames.len(), (SEGS * FRAMES) as usize, "{tag}");
    assert_same_frames(&w.frames, &rows(&a.recs), &format!("{tag}: ホストの列"));
    assert_same_frames(&w.frames, &rows(&b.recs), &format!("{tag}: ゲストの列"));
    let watch_rows = compare_spectate::parse_watch_frames(&watch::frames_csv(w)).unwrap();
    for (who, r) in [("A", a), ("B", b)] {
        let player = compare_spectate::parse_player(&record::log_text(r, "test")).unwrap();
        let o = compare_spectate::compare(&watch_rows, &player);
        assert!(
            o.ok && o.offset == Some(0) && o.tail_left == Some(0),
            "{tag} {who}: {o:?}"
        );
    }
    // 記録の順と中身（dll と同じ並び: `Seg(k)` の後ろにその seg の押下・SELECT、再戦の seg1 は `Seg` の直後がマッチの語。最後の seg を出る
    // ときも次の `Seg`（seg2）を積み、その後ろに終わり）。
    assert_eq!(
        kinds(w),
        [
            "seg0",
            "name1=sim-b",
            "name2=sim-a",
            "step",
            "select1",
            "select2",
            "seg1",
            "name1=sim-b",
            "name2=sim-a",
            "seg2",
            "name1=sim-b",
            "name2=sim-a",
            "endFinished"
        ],
        "{tag}"
    );
    let steps: Vec<_> = w
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            WEvent::Step(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(steps, a.presses, "{tag}: 押下が違う");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].kind, StepKind::Decide);
    let sels: Vec<_> = w
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            WEvent::Select { seat, sel } => Some((*seat, *sel)),
            _ => None,
        })
        .collect();
    let (s1, s2) = (sels[0].1, sels[1].1);
    assert_eq!((sels[0].0, sels[1].0), (Seat::P1, Seat::P2), "{tag}");
    assert_eq!(u64::from(s1.chara), SEED_B % 16, "{tag}: 1P はゲスト B");
    assert_eq!(u64::from(s2.chara), SEED_A % 16, "{tag}: 2P はホスト A");
    assert_eq!((s1.life, s2.life), (10, 10), "{tag}");
    assert_eq!(
        (s1.map, s1.difficulty, s1.rng_seed, s1.cpu_1p, s1.cpu_2p),
        (s2.map, s2.difficulty, s2.rng_seed, s2.cpu_1p, s2.cpu_2p),
        "{tag}: 共通の欄が両席で同じ"
    );
    assert_eq!(s1.rng_seed, SEED_A as u16, "{tag}: シードはホストの値");
    assert_eq!(s1.match_no, s2.match_no);
    assert!(
        w.events
            .iter()
            .any(|(_, e)| matches!(e, WEvent::Menu { .. })),
        "{tag}: MENU の語が無い"
    );
    let sp = a
        .spectate
        .as_ref()
        .expect("ホストに観戦の受信ソケットの報告");
    assert!(sp.active, "{tag}");
    assert_eq!(w.records as usize, w.frames.len() + w.events.len(), "{tag}");
    assert_eq!(
        u64::from(sp.tail),
        u64::from(w.records),
        "{tag}: 積んだ数と取り出した数"
    );
    assert_eq!(sp.counts.pushed, u64::from(w.records), "{tag}");
    assert_eq!(
        (sp.counts.overflows, sp.counts.push_dropped),
        (0, 0),
        "{tag}"
    );
    assert_eq!(sp.counts.joined, 1, "{tag}");
    assert!(b.spectate.is_none(), "{tag}");
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn spectator_sees_both_players_words_lossless() {
    let t = run_trio(
        FaultConfig::default(),
        FaultConfig::default(),
        FaultConfig::default(),
        |_| {},
        false,
    );
    assert_full_watch(&t, "ロス無し");
    // ロスが無いので、ホストの送り直しはほぼ要らない（ウィンドウと ACK が噛み合っている）。BYE を受けて観戦者を外した。0 を求めると、別の建て直しが
    // 同じ機械で走る負荷の下でスレッドが 4 ms 遅れたときに 1〜12 回出て落ちるので、上限は 40。ACK が返らない・噛み合わないと、ウィンドウが埋まる
    // までの 1 秒で約 8 ms ごとに送り直す（黙った観戦者で 7 秒に約 160 回）ので、上限はそれより十分下。
    let sp = t.a.spectate.as_ref().unwrap();
    assert!(sp.counts.resends <= 40, "{sp:?}");
    assert_eq!(sp.counts.left, 1, "{sp:?}");
    assert!(sp.spectators.is_empty(), "{sp:?}");
    assert_eq!(
        t.a.recs,
        baseline().0.recs,
        "観戦者がいても対戦者の列は同じ"
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn spectator_survives_fixed_loss_both_ways() {
    let spec = FaultConfig {
        drop_every: 5,
        drop_run: 2,
        ..FaultConfig::default()
    };
    let t = run_trio(
        FaultConfig::default(),
        FaultConfig::default(),
        spec,
        |c| {
            c.fault = FaultConfig {
                drop_every: 4,
                drop_run: 1,
                ..FaultConfig::default()
            }
        },
        false,
    );
    assert_full_watch(&t, "観戦者の決め打ちのロス");
    let sp = t.a.spectate.as_ref().unwrap();
    assert!(
        sp.faults.dropped > 0 && t.w.faults.dropped > 0,
        "{sp:?} {:?}",
        t.w.faults
    );
    assert!(
        sp.counts.resends > 0,
        "落としたのに送り直していない: {sp:?}"
    );
    assert_eq!(t.a.recs, baseline().0.recs);
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn spectator_survives_random_fault_both_ways() {
    let t = run_trio(
        FaultConfig::default(),
        FaultConfig::default(),
        random_fault(41),
        |c| c.fault = random_fault(42),
        false,
    );
    assert_full_watch(&t, "観戦者の乱れ seed 41/42");
    let sp = t.a.spectate.as_ref().unwrap();
    let f = sp.faults;
    assert!(f.dropped > 0 && f.delayed > 0 && f.dupped > 0, "{f:?}");
    assert_eq!(t.a.recs, baseline().0.recs);
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn late_join_in_a_rematch_starts_at_the_seg_record() {
    let t = run_trio(
        FaultConfig::default(),
        FaultConfig::default(),
        FaultConfig::default(),
        |c| c.start_after_ms = 4_000,
        false,
    );
    let (a, b, w) = (&t.a, &t.b, &t.w);
    assert_pair_ok(a, b, "遅れた JOIN");
    assert_eq!(
        w.end,
        WatchEnd::HostEnded(ByeReason::Finished),
        "{}",
        watch::summary(w)
    );
    assert_eq!(w.frames.len(), FRAMES as usize, "{}", watch::summary(w));
    assert_same_frames(&w.frames, &rows(&a.recs[FRAMES as usize..]), "遅れた JOIN");
    assert_eq!(w.frames.first().map(|f| (f.seg, f.frame)), Some((1, 0)));
    // 記録は「seg 1 の始まり（と名前）・次の seg 2 の始まり（と名前）・終わり」だけ（名前は各 seg の `Seg` の直後。遅れて入った観戦者も受ける）。
    // seg 1 は再戦で押下・SELECT が無く（`Seg` 1 の直後がマッチの語）、seg 0 の記録（`Seg` 0・押下・SELECT）は seg 1 の先頭より前で、受けない。
    assert_eq!(
        kinds(w),
        [
            "seg1",
            "name1=sim-b",
            "name2=sim-a",
            "seg2",
            "name1=sim-b",
            "name2=sim-a",
            "endFinished"
        ]
    );
    let watch_rows = compare_spectate::parse_watch_frames(&watch::frames_csv(w)).unwrap();
    let player = compare_spectate::parse_player(&record::log_text(a, "test")).unwrap();
    let o = compare_spectate::compare(&watch_rows, &player);
    assert!(
        o.ok && o.offset == Some(FRAMES as usize) && o.starts_at_seg_head && o.tail_left == Some(0),
        "{o:?}"
    );
    let sp = a.spectate.as_ref().unwrap();
    assert_eq!(sp.counts.joined, 1);
    assert_eq!(a.recs, baseline().0.recs);
}

/// 両者の読み込みの長さを揃える（`configs` は A 30 ms・B 250 ms で、先に読み込みを終えた側が 13〜14 フレーム stall する。その stall は
/// 走るたびに 0〜14 フレームぶれる（実測、観戦者なしの同じ走行を 4 回で A 25・39・25・27、B 0・13・0・1）ので、観戦者が stall を
/// 増やさないかを見る検査では揃える）。
fn equal_load(a: &mut SimConfig, b: &mut SimConfig) {
    a.load_ms = 100;
    b.load_ms = 100;
}

fn run_pair_equal_load() -> (Report, Report) {
    let (mut ca, mut cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    equal_load(&mut ca, &mut cb);
    let ta = thread::spawn(move || sim::run(&ca, sa));
    let tb = thread::spawn(move || sim::run(&cb, sb));
    (
        ta.join().expect("A がパニックしない").expect("A が走る"),
        tb.join().expect("B がパニックしない").expect("B が走る"),
    )
}

/// 観戦者なしの走行との stall の差の許し（フレーム）。読み込みを揃えても、両者がマッチに入る時刻の位相のぶれで、観戦者なしの走行どうしでも
/// 合計が 0〜13 フレームぶれる（実測、読み込みを揃えた走行で A 0・9・12・0、B 0・11・13・0）。観戦の仕事が対戦の周回を遅らせる変異
/// （観戦のポートの周回ごとに 20 ms 眠る）は、読み込みを揃えない走行で A 28 → 105・B 3 → 79 と、ぶれの数倍になった。なので許しはぶれの倍の
/// 30 に置く（位相のぶれが見えなくなる細かい遅れは、この検査では見つからない）。
const STALL_SLACK: u32 = 30;

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn silent_spectator_does_not_change_the_players() {
    let t = run_trio_with(
        FaultConfig::default(),
        FaultConfig::default(),
        FaultConfig::default(),
        |c| {
            c.fault = FaultConfig {
                drop_after: 3,
                ..FaultConfig::default()
            };
            c.max_ms = 9_000;
        },
        false,
        equal_load,
    );
    let (a, b) = (&t.a, &t.b);
    assert_pair_ok(a, b, "黙った観戦者");
    let base = run_pair_equal_load();
    assert_eq!(a.recs, base.0.recs, "観戦者が黙ったら対戦者の列が変わった");
    assert_eq!(b.recs, base.1.recs);
    eprintln!(
        "stall（合計, 最大連続）: 黙った観戦者つき A {:?} B {:?} / 観戦者なし A {:?} B {:?}",
        (a.stalls, a.max_stall_run),
        (b.stalls, b.max_stall_run),
        (base.0.stalls, base.0.max_stall_run),
        (base.1.stalls, base.1.max_stall_run)
    );
    for (who, with, without) in [("A", a, &base.0), ("B", b, &base.1)] {
        assert!(
            with.stalls <= without.stalls + STALL_SLACK,
            "{who}: 観戦者が黙ったら stall が増えた（{} → {}）",
            without.stalls,
            with.stalls
        );
    }
    assert_eq!(a.end, End::Finished { peer_bye: true });
    let sp = a.spectate.as_ref().unwrap();
    assert!(sp.counts.resends > 0, "{sp:?}");
    // 4 ms ごとのまま送り直すと 7 秒で 1,750 通。core は、ウィンドウ（ack から 64 記録）が埋まるまでの約 1 秒は新しい記録が積まれるたびに間隔を
    // 4 ms へ戻す（約 8 ms ごと。同じ条件の模擬で 7 秒に約 160 回）が、埋まった後は倍々（上限 250 ms）。
    assert!(
        sp.counts.resends < 400,
        "送り直しの間隔が伸びていない: {sp:?}"
    );
    // 渡し終えの待ちの上限（`DRAIN_MS` = 1 秒）に余裕を足した値を、定数をコピーせず数で書く（定数を壊す変異で恒真にしない）。
    assert!(sp.drain_ms <= 1_300, "{sp:?}");
    assert_eq!(
        sp.spectators.len(),
        1,
        "黙った観戦者はまだ外れていない（10 秒未満）"
    );
    assert!(
        sp.spectators[0].1 < sp.tail,
        "黙った観戦者が受け取り終えている: {sp:?}"
    );
    assert!(t.w.frames.len() < (SEGS * FRAMES) as usize);
    assert_eq!(t.w.end, WatchEnd::Deadline, "{}", watch::summary(&t.w));
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn late_join_in_the_first_seg_starts_at_the_ring_head() {
    let t = run_trio(
        FaultConfig::default(),
        FaultConfig::default(),
        FaultConfig::default(),
        |c| c.start_after_ms = 1_500,
        false,
    );
    let (a, b, w) = (&t.a, &t.b, &t.w);
    assert_pair_ok(a, b, "最初の seg の途中の JOIN");
    assert_eq!(
        w.end,
        WatchEnd::HostEnded(ByeReason::Finished),
        "{}",
        watch::summary(w)
    );
    assert_eq!(
        kinds(w),
        [
            "seg0",
            "name1=sim-b",
            "name2=sim-a",
            "step",
            "select1",
            "select2",
            "seg1",
            "name1=sim-b",
            "name2=sim-a",
            "seg2",
            "name1=sim-b",
            "name2=sim-a",
            "endFinished"
        ]
    );
    assert_eq!(w.frames.len(), (SEGS * FRAMES) as usize);
    assert_same_frames(&w.frames, &rows(&a.recs), "最初の seg の途中の JOIN");
    let steps: Vec<_> = w
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            WEvent::Step(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert_eq!(steps, a.presses);
    let selects = w
        .events
        .iter()
        .filter(|(_, e)| matches!(e, WEvent::Select { .. }))
        .count();
    assert_eq!(selects, 2, "両席の SELECT");
    assert_eq!(
        w.events.first().map(|(idx, _)| *idx),
        Some(0),
        "最初の記録の番号が 0 でない"
    );
    assert_eq!(a.spectate.as_ref().unwrap().counts.joined, 1);
    assert_eq!(a.recs, baseline().0.recs);
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_guest_run_streams_nothing() {
    let t = run_trio(
        FaultConfig::default(),
        FaultConfig::default(),
        FaultConfig::default(),
        |c| c.silent_end_ms = 2_500,
        true,
    );
    assert_pair_ok(&t.a, &t.b, "ゲストの観戦の受信ソケット");
    let sp = t.b.spectate.as_ref().expect("ゲストにも報告の欄はある");
    assert!(!sp.active, "{sp:?}");
    assert_eq!((sp.counts.pushed, sp.tail), (0, 0), "{sp:?}");
    assert!(t.a.spectate.is_none());
    assert!(t.w.joined, "{}", watch::summary(&t.w));
    assert_eq!((t.w.records, t.w.frames.len()), (0, 0));
    assert_eq!(t.w.end, WatchEnd::HostSilent, "{}", watch::summary(&t.w));
}

// ================= 観戦者を中継越しに繋ぐ（`--rewrite-spectate-code` ＋ `--shape`） =================
//
// 本物の花映塚の観戦者（dll）は、観戦用のコードの候補（＝ホストの受信ソケット）へ直に JOIN する。中継越しにするには、コードの候補を
// 中継のポートへ書き換えて観戦者へ渡す。ここは peer_sim のホスト・ゲスト・観戦者で同じ形を回す: ホストの観戦のポートが中継の A（`--a`）、
// 観戦者の受信ソケットが中継の B（`--b`）で、観戦者の宛先は中継の B 向けのポート（`--via-b`）。

/// 観戦者と観戦のポートの間の中継（向きごと）。A→B はホスト→観戦者の記録の流れ（STREAM）、B→A は観戦者の JOIN・ACK。
fn spectator_relay_faults(seed: u64, drop_pct: u8, jitter_ms: u32) -> FaultConfig {
    FaultConfig {
        seed,
        one_way_ms: 30,
        drop_pct,
        jitter_ms,
        ..FaultConfig::default()
    }
}

#[derive(Clone, Copy)]
enum Aim {
    /// B 向けのポート（`--via-b`）。観戦者（B）の送り元が「そのポートの相手」なので運ばれる。正しい向き。
    ViaB,
    /// A 向けのポート（`--via-a`）。観戦者の送り元は A（ホスト）ではないので、中継が送り元違いとして捨てる。
    ViaA,
}

/// ホスト（観戦のポートつき）・ゲスト・観戦者を回し、観戦者だけを中継越しにする。観戦者の宛先は、ホストの観戦用のコードを
/// `spectate_code::rewrite` で書き換え（`aim` = 候補にする中継のポート）、観戦者の側の読み（`code::read_spectate`）で読んだ
/// ものから取る（dll と同じ経路）。
fn run_trio_via_relay(
    a2b: FaultConfig,
    b2a: FaultConfig,
    aim: Aim,
    edit: impl FnOnce(&mut WatchConfig),
) -> (Trio, peer_sim::shape::Report) {
    use peer_sim::shape;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (ca, cb, sa, sb) = configs(FaultConfig::default(), FaultConfig::default());
    let spec_sock = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let host_addr = spec_sock.local_addr().unwrap();
    let wsock = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let va = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let vb = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    let args = shape::Args {
        a: host_addr,
        b: wsock.local_addr().unwrap(),
        via_a: va.local_addr().unwrap(),
        via_b: vb.local_addr().unwrap(),
        a2b,
        b2a,
        max_sec: 120,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // コードを使う経路: 配る側の種は走るたびの新しい種で、コードの公開鍵がその種の公開鍵（観戦者はコードから読む）。
    let host_seed = peer_sim::keys::fresh_seed();
    let cli_spec = peer_sim::cli::SpectateCli {
        bind: host_addr,
        session_id: SPEC_SESSION,
        secret: SPEC_SECRET,
        key_seed: host_seed,
    };
    let original = peer_sim::cli::spectate_code(&cli_spec, "host", now).expect("コードが書ける");
    let target = match aim {
        Aim::ViaB => args.via_b,
        Aim::ViaA => args.via_a,
    };
    let rewritten = peer_sim::spectate_code::rewrite(
        &original,
        target,
        higanr_core::guard::Scope::Loopback,
        now,
    )
    .expect("書き換えられる");
    let sc = higanr_core::code::read_spectate(&rewritten.text, now).expect("観戦者の読みで読める");
    let c = sc.code;
    let mut wc = watch_cfg(host_addr);
    wc.host_public = sc.host_public;
    wc.host = c.candidates.as_slice().iter().map(|x| x.addr).collect();
    wc.session_id = c.session_id;
    wc.secret = c.secret.expect("観戦用のコードは秘密を持つ");
    assert_eq!(wc.host, vec![target], "観戦者の宛先は書き換えた 1 つだけ");
    edit(&mut wc);

    let opts = SpectateHostOpts {
        sock: spec_sock,
        session_id: SPEC_SESSION,
        secret: SPEC_SECRET,
        key_seed: host_seed,
        fault: FaultConfig::default(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let st = Arc::clone(&stop);
    let relay = thread::spawn(move || {
        shape::run_with_sockets(&args, va, vb, &st, &mut |_| {}).expect("中継が走る")
    });
    let ta = thread::spawn(move || {
        let o = sim::RunOpts {
            spectate: Some(&opts),
            ..sim::RunOpts::default()
        };
        sim::run_opts(&ca, sa, o)
    });
    let tb = thread::spawn(move || sim::run(&cb, sb));
    let tw = thread::spawn(move || watch::run(&wc, wsock, higanr_core::guard::Scope::Loopback));
    let a = ta.join().expect("A がパニックしない").expect("A が走る");
    let b = tb.join().expect("B がパニックしない").expect("B が走る");
    let w = tw
        .join()
        .expect("観戦者がパニックしない")
        .expect("観戦者が走る");
    stop.store(true, Ordering::Relaxed);
    let rr = relay.join().expect("中継がパニックしない");
    (Trio { a, b, w }, rr)
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn spectator_through_a_relay_with_a_rewritten_code_sees_the_same_words() {
    let (t, rr) = run_trio_via_relay(
        spectator_relay_faults(51, 12, 60),
        spectator_relay_faults(52, 8, 40),
        Aim::ViaB,
        |_| {},
    );
    assert_full_watch(&t, "観戦者を中継越しに（書き換えたコード）");
    let sp = t.a.spectate.as_ref().unwrap();
    assert_eq!(sp.faults.dropped, 0, "{sp:?}");
    for (c, dir) in [
        (&rr.a2b, "A→B（ホスト→観戦者）"),
        (&rr.b2a, "B→A（観戦者→ホスト）"),
    ] {
        assert!(c.received > 50, "{dir}: {c:?}");
        assert!(
            c.dropped > 0 && c.jittered > 0,
            "{dir}: 乱れが効いていない {c:?}"
        );
        assert_eq!((c.foreign, c.send_errors), (0, 0), "{dir}: {c:?}");
    }
    // 落とされた STREAM を、ホストが送り直した（経路が走った）。観戦者は同じ記録を重ねて受けることがある。
    assert!(
        sp.counts.resends > 0,
        "中継が落としたのに送り直していない: {sp:?} {:?}",
        rr.a2b
    );
    // 観戦者が受けた STREAM に、中継が中身を変えたものは無い（タグの誤り・読めない）。
    let wc = t.w.counts;
    assert_eq!(
        (wc.bad_tag, wc.undecodable, wc.foreign, wc.wrong_session),
        (0, 0, 0, 0),
        "{wc:?}"
    );
    assert_eq!(
        sp.counts.bad_tag + sp.counts.undecodable + sp.counts.unknown_spectator,
        0,
        "{sp:?}"
    );
    eprintln!(
        "観戦者の中継越し: ホストの送り直し {}・観戦者が捨てた重複 {}・{}",
        sp.counts.resends,
        t.w.asm.dup,
        peer_sim::shape::line(rr.elapsed_ms, &rr.a2b, &rr.b2a)
    );
}

#[test]
#[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
fn a_code_aimed_at_the_hosts_side_of_the_relay_never_joins() {
    let (t, rr) = run_trio_via_relay(
        spectator_relay_faults(53, 0, 0),
        spectator_relay_faults(54, 0, 0),
        Aim::ViaA,
        |c| c.max_ms = 6_000,
    );
    assert_pair_ok(&t.a, &t.b, "観戦者が繋がらなくても対戦は通る");
    assert!(!t.w.joined, "{}", watch::summary(&t.w));
    assert_eq!(t.w.end, WatchEnd::Deadline, "{}", watch::summary(&t.w));
    let sp = t.a.spectate.as_ref().unwrap();
    assert_eq!(sp.counts.joined, 0, "{sp:?}");
    assert!(
        rr.a2b.foreign > 0 && rr.a2b.received == 0,
        "JOIN が送り元違いとして捨てられていない: {:?}",
        rr.a2b
    );
    assert_eq!((rr.b2a.received, rr.b2a.foreign), (0, 0), "{:?}", rr.b2a);
}
