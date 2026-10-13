//! 長く続く選択の画面に遅れて入った観戦者（実回線の試験で「キャラ選択の最中にルームに入ると、対戦に進まない」）と、初めから観戦している
//! 観戦者が「キャラ変更」で別のキャラを選ぶ 2 回目の選択を追う検査。`watch_tests.rs` の `Rig`（ホスト・仮のネットワーク・観戦者）と偽のゲームを
//! そのまま使う。
//!
//! 原因: 選択の画面でホストが積む `Menu`（その tick の両席の語）は 1 tick に 1 つ。遅れて入った観戦者は seg の始まりから 1 tick に 1 つずつ
//! 使うので、ホストが選択の画面に居た時間（実機の回は画面 8 に 62 秒）がそのまま観戦者の遅れになり、その間はマッチに進まない。
//!
//! 比べるもの: マッチの最初のフレームを使った tick（ホストと観戦者）の差、マッチの語・押下の列・決めたキャラ・マップ（観戦者の列が
//! ホストと全行一致するか）。
use std::cell::Cell;

use higanr_core::input_model::keys::{LEFT, RIGHT};

use super::*;

/// 画面に長くとどまる tick の数（60 Hz で 25 秒。実機の回は 62 秒）。観戦者はその半ばで入る。
const IDLE: u32 = 1_500;
/// 観戦者が入ってから、ホストのマッチの最初のフレームより何 tick 遅れてよいか（繋がり・押下の再生・余裕待ちの分。2.5 秒）。
const LAG_OK_TICKS: u32 = 150;
const MATCH_FRAMES: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Idle {
    Difficulty,
    Chara,
    CharaAfterCancel,
    Map,
}

impl Idle {
    fn screen(self) -> u32 {
        match self {
            Idle::Difficulty => 7,
            Idle::Chara | Idle::CharaAfterCancel => 8,
            Idle::Map => 16,
        }
    }
}

/// 台本。1 回目の選択は `idle0` の画面で `IDLE` tick 止まる（None は止まらない）。2 回目の選択（1 マッチ終えてキャラ選択から）は、
/// 両者とも 1 回目と違うキャラを選ぶ（`idle1` が真ならキャラ選択で `IDLE` tick 止まる）。A（ホスト 1P）がカーソルを動かして決め、
/// B（ゲスト 2P）は A の後に決める。
fn plans_with(idle0: Option<Idle>, idle1: bool) -> (Vec<Act>, Vec<Act>) {
    let wait = |here: Idle, base: u32| if idle0 == Some(here) { IDLE } else { base };
    let mut a = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, wait(Idle::Difficulty, 20), Z, 1),
    ];
    let mut b = vec![act(0, 6, 30, Z, 1)];
    match idle0 {
        Some(Idle::Chara) => {
            a.extend([
                act(0, 8, IDLE, RIGHT, 1),
                act(0, 8, 3, RIGHT, 1),
                act(0, 8, 3, Z, 1),
            ]);
            b.extend([act(0, 8, IDLE + 30, LEFT, 1), act(0, 8, 3, Z | SHIFT, 1)]);
        }
        Some(Idle::CharaAfterCancel) => {
            a.extend([
                act(0, 8, 12, RIGHT, 1),
                act(0, 8, 3, Z, 1),
                act(0, 8, 20, X, 1),
                act(0, 8, IDLE, Z, 1),
            ]);
            b.extend([act(0, 8, 15, LEFT, 1), act(0, 8, IDLE + 100, Z | SHIFT, 1)]);
        }
        _ => {
            a.extend([
                act(0, 8, 12, RIGHT, 1),
                act(0, 8, 3, RIGHT, 1),
                act(0, 8, 3, Z, 1),
            ]);
            b.extend([act(0, 8, 15, LEFT, 1), act(0, 8, 30, Z | SHIFT, 1)]);
        }
    }
    a.push(act(0, 16, wait(Idle::Map, 8), Z, 1));
    let n = if idle1 { IDLE } else { 12 };
    a.extend([
        act(1, 8, n, RIGHT, 1),
        act(1, 8, 3, RIGHT, 1),
        act(1, 8, 3, Z, 1),
        act(1, 16, 8, Z, 1),
    ]);
    b.extend([
        act(1, 8, n + 20, LEFT, 1),
        act(1, 8, 3, LEFT, 1),
        act(1, 8, 30, Z | SHIFT, 1),
    ]);
    (a, b)
}

fn games_with(idle0: Option<Idle>, idle1: bool, matches: u32) -> (MenuGame, MenuGame, MenuGame) {
    let (pa, pb) = plans_with(idle0, idle1);
    let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    for g in [&mut a, &mut b] {
        g.max_matches = matches;
        g.match_frames = MATCH_FRAMES;
    }
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    let w = watcher_game(MATCH_FRAMES, matches, 0);
    (a, b, w)
}

struct Probe {
    a: MenuGame,
    b: MenuGame,
    npa: NetPlay,
    rig: Rig,
    a_first: Vec<u32>,
    w_first: Vec<u32>,
    joined_at: Option<u32>,
    /// 観戦者が追いつくために `Menu` を捨てた最後の tick（止まった = ホストの積んだ分を受け終えて追いついた）。
    last_skip_at: Option<u32>,
}

/// `run_rig` と同じ組み立て（A と B を回し、W を付ける）で、マッチの最初のフレームを使った tick を記録する。
fn run_probe(
    a: MenuGame,
    b: MenuGame,
    w: MenuGame,
    start_when: &dyn Fn(&MenuGame) -> bool,
    ticks: u32,
) -> Probe {
    run_probe_with(a, b, w, start_when, ticks, 5_000)
}

fn run_probe_with(
    mut a: MenuGame,
    mut b: MenuGame,
    w: MenuGame,
    start_when: &dyn Fn(&MenuGame) -> bool,
    ticks: u32,
    spec_delay_us: u64,
) -> Probe {
    let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), 8_000);
    let cfg = Config {
        check: CheckSource::InputDigest,
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        ..Config::default()
    };
    let mut npa = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
    let mut rig = Rig::new(w, spec_delay_us);
    npa.set_spectate(Arc::clone(&rig.ring));
    let mut npb = NetPlay::new(link_cfg(1, 1), cfg, &mut nb);
    let mut slot = clock.get();
    let mut closed = false;
    let mut after_close = 0u32;
    let mut after_finish = 0u32;
    let (mut a_first, mut w_first) = (Vec::new(), Vec::new());
    let mut joined_at = None;
    let (mut last_skip_at, mut seen_skipped) = (None, 0u64);
    let mf = MATCH_FRAMES as usize;
    for i in 0..ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        a.tick(&mut npa, &mut na);
        clock.set(clock.get().max(slot + FRAME_US / 2));
        b.tick(&mut npb, &mut nb);
        let start = start_when(&a);
        if start && joined_at.is_none() {
            joined_at = Some(i);
        }
        rig.hook(clock.get(), start);
        let skipped = rig.wp.stats().menu_skipped;
        if skipped != seen_skipped {
            seen_skipped = skipped;
            last_skip_at = Some(i);
        }
        while a_first.len() * mf < a.played.len() {
            a_first.push(i);
        }
        while w_first.len() * mf < rig.wg.played.len() {
            w_first.push(i);
        }
        slot += FRAME_US;
        let back = |g: &MenuGame| {
            g.matches >= g.max_matches
                && g.cur == 1
                && g.screen == 8
                && !g.pending_auto8
                && g.plan_done()
        };
        if !closed && back(&a) && back(&b) {
            npa.close(ByeReason::Finished);
            closed = true;
        }
        if closed {
            after_close += 1;
            if rig.finished() {
                after_finish += 1;
            }
            if after_finish > 5 || after_close > 6_000 {
                break;
            }
        }
    }
    Probe {
        a,
        b,
        npa,
        rig,
        a_first,
        w_first,
        joined_at,
        last_skip_at,
    }
}

/// `screen` の画面に入ってから `after` tick 経ったら入る（ホスト A を見る）。画面に入った tick は最初に見たときに控える。
fn join_in_screen(screen: u32, matches: u32, after: u32) -> impl Fn(&MenuGame) -> bool {
    let seen = Cell::new(0u32);
    move |a: &MenuGame| {
        if a.cur == 1 && a.screen == screen && a.matches == matches && seen.get() == 0 {
            seen.set(a.tick_no);
        }
        seen.get() != 0 && a.tick_no >= seen.get() + after
    }
}

/// 観戦者が見たマッチ（ホストの `skip` マッチ目から）の語・押下・キャラ・マップがホストと全行一致し、マッチの最初のフレームを
/// ホストから `LAG_OK_TICKS` tick 以内に使い、ホストの終わりで終え、自分の入力が届かず、読まれない押下も無いこと。
fn assert_caught_up(p: &Probe, skip: usize, tag: &str) {
    let (wp, wg) = (&p.rig.wp, &p.rig.wg);
    let s = wp.stats();
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{tag}: {s:?}"
    );
    let n = p.w_first.len();
    assert!(n >= 1, "{tag}: 観戦者が試合に進んでいない {s:?}");
    assert_eq!(
        p.a_first.len() - skip,
        n,
        "{tag}: ホストの試合の数と観戦者の試合の数 {s:?}"
    );
    for (j, (w, a)) in p.w_first.iter().zip(&p.a_first[skip..]).enumerate() {
        eprintln!(
            "{tag}: {} 試合目の遅れ {} tick（入ったのは {:?} tick 目・捨てた Menu {}）",
            j + 1 + skip,
            w.saturating_sub(*a),
            p.joined_at,
            s.menu_skipped
        );
        assert!(
            w.saturating_sub(*a) <= LAG_OK_TICKS,
            "{tag}: {} 試合目の最初のフレームが、ホストの {a} tick 目から {} tick 遅れた（観戦者が入ったのは {:?} tick 目）{s:?}",
            j + 1 + skip,
            w.saturating_sub(*a),
            p.joined_at
        );
    }
    let first = skip * MATCH_FRAMES as usize;
    assert_eq!(wg.played, p.a.played[first..], "{tag}: 試合の語（A）");
    assert_eq!(wg.played, p.b.played[first..], "{tag}: 試合の語（B）");
    let a_steps: Vec<_> = p.npa.step_log().iter().map(|s| s.step).collect();
    let w_steps: Vec<_> = wp.step_log().iter().map(|s| s.step).collect();
    assert!(!w_steps.is_empty(), "{tag}: 押下を入れていない");
    assert_eq!(
        w_steps[..],
        a_steps[a_steps.len() - w_steps.len()..],
        "{tag}: 入った押下の列（ホストの最後の seg の先最初から）"
    );
    let ch = p.a.decided_chars.len() - wg.decided_chars.len();
    assert_eq!(
        wg.decided_chars[..],
        p.a.decided_chars[ch..],
        "{tag}: 決めたキャラ"
    );
    assert_eq!(
        wg.decided_maps[..],
        p.a.decided_maps[p.a.decided_maps.len() - wg.decided_maps.len()..],
        "{tag}: 決めたマップ"
    );
    assert_eq!(wg.watched.len(), n, "{tag}: 止める地点の数");
    for (k, pair) in wg.watched.iter().enumerate() {
        assert_eq!(
            [pair[0].chara, pair[1].chara],
            p.a.decided_chars[ch + k],
            "{tag}: {} 回目の止める地点で書いたキャラ",
            k + 1
        );
    }
    assert_eq!(wg.ignored_presses, 0, "{tag}: 読まれなかった押下");
    assert_eq!(p.rig.leaks, 0, "{tag}: 自分の入力が届いた");
    assert_eq!(p.rig.allocs_joined, 0, "{tag}: on_input が確保した");
    assert_eq!(
        (s.frame_gaps, s.jumps, s.stray_records),
        (0, 0, 0),
        "{tag}: {s:?}"
    );
}

#[test]
fn a_watcher_joining_in_the_middle_of_a_long_selection_screen_reaches_the_match_with_the_host() {
    for idle in [
        Idle::Difficulty,
        Idle::Chara,
        Idle::CharaAfterCancel,
        Idle::Map,
    ] {
        let (a, b, w) = games_with(Some(idle), false, 2);
        let when = join_in_screen(idle.screen(), 0, IDLE / 2);
        let p = run_probe(a, b, w, &when, 12_000);
        assert!(p.joined_at.is_some(), "{idle:?}: 観戦者を起こせていない");
        assert_caught_up(&p, 0, &format!("{idle:?} の半ばで入る"));
        // 追いつきの経路を通った（`Menu` の大半を捨てた）。通っていなければ上の遅れの検査は別の理由で通っている。
        let skipped = p.rig.wp.stats().menu_skipped;
        assert!(skipped >= 400, "{idle:?}: 捨てた Menu {skipped}");
    }
}

#[test]
fn a_watcher_from_the_start_follows_a_second_selection_with_other_characters() {
    let (a, b, w) = games_with(None, false, 2);
    let p = run_probe(a, b, w, &|_| true, 12_000);
    assert_caught_up(&p, 0, "初めから観戦（2 回目の選択でキャラを変える）");
    assert_eq!(p.a.decided_chars.len(), 2, "選択が 2 回通っていない");
    assert_ne!(
        p.a.decided_chars[0], p.a.decided_chars[1],
        "2 回目のキャラが 1 回目と同じ（検査の前提）"
    );
    let s = p.rig.wp.stats();
    assert_eq!((s.menu_walked, s.menu_skipped), (0, 0), "{s:?}");
}

#[test]
fn a_watcher_joining_in_the_middle_of_a_second_selection_follows_it() {
    let (a, b, w) = games_with(None, true, 2);
    let when = join_in_screen(8, 1, IDLE / 2);
    let p = run_probe(a, b, w, &when, 12_000);
    assert!(p.joined_at.is_some(), "観戦者を起こせていない");
    assert_caught_up(&p, 1, "2 回目の選択の半ばで入る");
    let s = p.rig.wp.stats();
    assert!(s.menu_walked >= 1, "{s:?}");
    assert!(s.menu_skipped >= 400, "{s:?}");
}

#[test]
fn a_watcher_catches_up_over_a_slow_line_before_the_host_decides() {
    let (a, b, w) = games_with(Some(Idle::Chara), false, 2);
    let when = join_in_screen(8, 0, IDLE / 2);
    let p = run_probe_with(a, b, w, &when, 12_000, 100_000);
    assert!(p.joined_at.is_some(), "観戦者を起こせていない");
    assert_caught_up(&p, 0, "片道 100 ms");
    // 入ってから、ホストが seg の始まりから積んだ 780 ほどの `Menu` を受け終える（最後に捨てる）まで: ウィンドウ 64 記録を 1 往復（片道 100 ms ＋
    // 観戦者の 17 ms の周回）ずつ受けるので 13 往復 ≈ 3 秒の見込み（外挿）。上限は余裕を見て 400 tick（約 6.7 秒）。
    let (j, c) = (p.joined_at.unwrap_or(0), p.last_skip_at);
    eprintln!(
        "片道 100 ms: 入ってから最後の Menu を捨てるまで {:?} tick",
        c.map(|c| c - j)
    );
    assert!(
        c.is_some_and(|c| c > j && c - j <= 400),
        "追いつきが遅い（入ったのは {j} tick 目・最後に捨てたのは {c:?} tick 目）"
    );
}
