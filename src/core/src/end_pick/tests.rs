use super::model::EndMenu;
use super::*;
use crate::input_model::InputState;
use crate::input_model::keys::{ESC, X};

const ENTER: u16 = 0x1000;

/// ステートマシンとゲームのモデルをつないだもの（入力の地点のfeed → そのtickのメニューの更新）。
#[derive(Debug, Clone, Copy)]
struct Rig {
    pick: EndPick,
    menu: EndMenu,
    im2: InputState,
    paused: bool,
    tick: u32,
    closed: Option<(u32, u8)>,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            pick: EndPick::new(),
            menu: EndMenu::default(),
            im2: InputState::new(),
            paused: false,
            tick: 0,
            closed: None,
        }
    }

    fn view(&self) -> View {
        View {
            shown: self.menu.shown,
            paused: self.paused,
            state: self.menu.state,
            frames: self.menu.frames,
        }
    }

    fn step(&mut self, p1: u16, p2: u16, confirmed: bool) -> Out {
        let out = self.pick.step(self.view(), p1, p2, confirmed);
        self.im2.feed(out.word2);
        // 一時停止の間も終わりのメニューは更新される（`0x00435BB3`）。
        if let Some(item) = self.menu.frame(self.im2.pressed) {
            self.closed.get_or_insert((self.tick, item));
        }
        self.tick += 1;
        out
    }

    /// メニューを出して人のキーを読む数え（`MENU_READ_FRAMES`）まで何も押さずに進める。
    fn ready() -> Rig {
        let mut r = Rig::new();
        r.menu = EndMenu::open(0);
        while r.menu.frames < MENU_READ_FRAMES || r.menu.state == end_menu::INIT {
            r.step(0, 0, true);
        }
        r
    }

    /// 1サイドが`key`を1 tick押して1 tick離す。
    fn tap(&mut self, seat: usize, key: u16) {
        let w = |s: usize| if s == seat { key } else { 0 };
        self.step(w(0), w(1), true);
        self.step(0, 0, true);
    }
}

fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn a_decided_item_is_reached_from_every_cursor_and_count_within_two_reading_ticks() {
    let mut cells = 0;
    for state in end_menu::REMATCH..=end_menu::SAVE {
        for t in 0..3u8 {
            for frames in 0..=30u32 {
                let mut r = Rig::new();
                r.menu = EndMenu {
                    shown: true,
                    state,
                    frames,
                };
                r.pick.decided = Some(t);
                let wait = MENU_READ_FRAMES.saturating_sub(frames);
                let mut seen = Vec::new();
                for _ in 0..wait + 2 {
                    r.step(0, 0, true);
                    seen.push(r.menu.state);
                    if r.menu.state >= 4 {
                        break;
                    }
                }
                let ctx = format!("状態 {state}・目的 {t}・数え {frames}: {seen:?}");
                assert_eq!(r.menu.state, u32::from(t) + 4, "{ctx}");
                assert!(
                    seen.iter()
                        .all(|&s| (1..=3).contains(&s) || s == u32::from(t) + 4),
                    "{ctx}"
                );
                for _ in 0..CLOSE_FRAMES + 1 {
                    r.step(0, 0, true);
                }
                assert_eq!(r.closed.map(|c| c.1), Some(t), "{ctx}");
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 3 * 3 * 31);
}

#[test]
fn nothing_is_read_or_pressed_outside_the_reading_states_counts_and_pause() {
    // (状態, 数え, 一時停止)。数え20の状態0は閉じた後に数えが残った形（`0x435669`だけが0に戻す）。
    let cases = [
        (end_menu::INIT, 20, false),
        (end_menu::INIT, 0, false),
        (4, 20, false),
        (5, 3, false),
        (6, 25, false),
        (end_menu::REMATCH, 3, false),
        (end_menu::CHARA, 0, false),
        (end_menu::SAVE, MENU_READ_FRAMES, true),
        (end_menu::REMATCH, 30, true),
    ];
    for (state, frames, paused) in cases {
        for decided in [None, Some(SAVE_ITEM)] {
            let mut p = EndPick::new();
            p.decided = decided;
            let before = p;
            let v = View {
                shown: true,
                paused,
                state,
                frames,
            };
            for w in [Z, ENTER, UP, DOWN, Z | DOWN, UP | DOWN | Z] {
                let mut q = before;
                let out = q.step(v, w, w, true);
                let ctx = format!("状態 {state}・数え {frames}・一時停止 {paused}・語 {w:#x}");
                assert_eq!(out.word2 & MENU_KEYS, 0, "{ctx}");
                assert_eq!(out.cursor_se, 0, "{ctx}");
                assert_eq!(out.decided, None, "{ctx}");
                assert_eq!(
                    (q.cursor, q.confirmed),
                    (before.cursor, before.confirmed),
                    "{ctx}"
                );
                assert_eq!(q.prev, [w, w], "{ctx}: 前の語は毎 tick 更新する");
            }
        }
    }
}

#[test]
fn a_key_held_since_before_the_menu_neither_confirms_nor_moves() {
    for key in [Z, ENTER, DOWN, UP] {
        let mut r = Rig::new();
        for _ in 0..5 {
            r.step(key, key, true);
        }
        r.menu = EndMenu::open(20);
        for _ in 0..60 {
            let out = r.step(key, key, true);
            assert_eq!(out.cursor_se, 0, "{key:#x}");
        }
        assert_eq!(r.pick.confirmed, [None; 2], "{key:#x}");
        assert_eq!(r.pick.cursor, [REMATCH_ITEM; 2], "{key:#x}");
        assert_eq!(
            r.menu.state,
            end_menu::REMATCH,
            "{key:#x}: 人のキーがゲームへ届いた"
        );
        // 離して押し直せば効く。
        r.step(0, 0, true);
        r.step(Z, 0, true);
        assert_eq!(r.pick.confirmed[0], Some(REMATCH_ITEM), "{key:#x}");
    }
}

#[test]
fn the_lower_item_wins_save_and_quit_goes_at_once_and_a_confirmed_seat_can_pick_again() {
    // 2人とも確定したら下の項目。
    for (a, b) in [(0u8, 1u8), (1, 0), (0, 0), (1, 1), (2, 0)] {
        let mut r = Rig::ready();
        for _ in 0..a {
            r.tap(0, DOWN);
        }
        for _ in 0..b {
            r.tap(1, DOWN);
        }
        r.tap(0, Z);
        if a != SAVE_ITEM {
            assert_eq!(r.pick.decided, None, "({a}, {b}): 片方だけで決まった");
        }
        r.tap(1, Z);
        for _ in 0..CLOSE_FRAMES + 4 {
            r.step(0, 0, true);
        }
        assert_eq!(r.closed.map(|c| c.1), Some(a.max(b)), "({a}, {b})");
    }
    // 「リプレイを保存して終了」は相手を待たない（上で1手）。
    let mut r = Rig::ready();
    r.tap(1, UP);
    assert_eq!(r.pick.cursor[1], SAVE_ITEM);
    r.step(0, Z, true);
    assert_eq!(r.pick.decided, Some(SAVE_ITEM));
    for _ in 0..CLOSE_FRAMES + 4 {
        r.step(0, 0, true);
    }
    assert_eq!(r.closed.map(|c| c.1), Some(SAVE_ITEM));
    // 確定の後に動かすと途中に戻る。相手が確定しても決まらない。確定し直せば決まる。
    let mut r = Rig::ready();
    r.tap(0, Z);
    assert_eq!(r.pick.confirmed[0], Some(REMATCH_ITEM));
    r.tap(0, DOWN);
    assert_eq!(r.pick.confirmed[0], None);
    r.tap(1, DOWN);
    r.tap(1, Z);
    assert_eq!(r.pick.decided, None);
    r.tap(0, Z);
    assert_eq!(r.pick.decided, Some(CHARA_ITEM));
    // 決まった後の人のキーは何も変えない。
    r.step(DOWN, DOWN, true);
    assert_eq!(r.pick.cursor, [CHARA_ITEM, CHARA_ITEM]);
    for _ in 0..CLOSE_FRAMES + 4 {
        r.step(0, 0, true);
    }
    assert_eq!(r.closed.map(|c| c.1), Some(CHARA_ITEM));
}

#[test]
fn one_word_with_up_down_and_decide_follows_the_games_order() {
    // 上と下が同じワードに入っていたら下が後に書く。決定はそのtickの頭の項目。
    let mut r = Rig::ready();
    r.step(UP | DOWN, 0, true);
    assert_eq!(r.pick.cursor[0], CHARA_ITEM);
    r.step(0, 0, true);
    r.step(UP | Z, 0, true);
    assert_eq!(r.pick.confirmed[0], Some(CHARA_ITEM));
    assert_eq!(r.pick.cursor[0], CHARA_ITEM);
}

#[test]
fn outside_the_menu_the_merged_word_goes_through_and_nothing_is_shown() {
    let mut p = EndPick::new();
    for n in 0..2_000u64 {
        let (p1, p2) = (mix(n) as u16 & 0x01FF, mix(n + 7_777) as u16 & 0x01FF);
        let out = p.step(View::default(), p1, p2, n % 3 != 0);
        assert_eq!(out.word2, p1 | p2, "{n}");
        assert_eq!(out.show, Show::NONE, "{n}");
        assert_eq!((out.cursor_se, out.decided), (0, None), "{n}");
    }
    // メニューの外のtickはサイドの状態を捨てる（前のワードだけ残す）。
    let mut r = Rig::ready();
    r.tap(0, DOWN);
    r.tap(1, Z);
    r.menu = EndMenu::default();
    r.step(X, ESC, true);
    assert_eq!(
        r.pick,
        EndPick {
            prev: [X, ESC],
            ..EndPick::new()
        }
    );
}

#[test]
fn the_marks_follow_each_seat_and_the_paint_turns_to_the_decided_item() {
    let mut r = Rig::ready();
    let out = r.step(0, DOWN, true);
    assert_eq!(out.show.marks, [Mark::Picking(0), Mark::Picking(1)]);
    assert_eq!(out.show.paint, Paint::Choosing);
    assert_eq!(out.cursor_se, 1);
    r.step(0, 0, true);
    let out = r.step(Z, 0, true);
    assert_eq!(out.show.marks, [Mark::Confirmed(0), Mark::Picking(1)]);
    let out = r.step(0, Z, true);
    assert_eq!(out.decided, Some(CHARA_ITEM));
    assert_eq!(out.word2 & MENU_KEYS, DOWN, "決めた tick に 1 手目を押す");
    assert_eq!(out.show.paint, Paint::Choosing);
    let out = r.step(0, 0, true);
    assert_eq!(out.word2 & MENU_KEYS, Z);
    assert_eq!(
        out.show.paint,
        Paint::Decided(CHARA_ITEM),
        "決定を押した tick から"
    );
    let out = r.step(0, 0, true);
    assert_eq!(r.menu.state, 5);
    assert_eq!(out.show.paint, Paint::Decided(CHARA_ITEM));
    assert_eq!(out.word2 & MENU_KEYS, 0);
}

#[test]
fn the_blink_starts_dark_and_shows_at_least_two_dark_runs_before_the_scene_changes() {
    let lit = |since| item_rgb(Paint::Decided(1), 1, since, true) == Some(RGB_LIT);
    assert!(!lit(0), "決定の tick は消えている（白からいきなり変わる）");
    let dark_runs = (0..CLOSE_FRAMES)
        .filter(|&f| !lit(f) && (f == 0 || lit(f - 1)))
        .count();
    let lit_runs = (0..CLOSE_FRAMES)
        .filter(|&f| lit(f) && f > 0 && !lit(f - 1))
        .count();
    assert!(dark_runs >= 2 && lit_runs >= 2, "{dark_runs} / {lit_runs}");
    assert_eq!(
        item_rgb(Paint::Decided(1), 1, BLINK_FRAMES, false),
        Some(RGB_LIT)
    );
    assert_eq!(
        item_rgb(Paint::Decided(1), 1, BLINK_FRAMES - 1, true),
        Some(RGB_DIM)
    );
    for since in 0..CLOSE_FRAMES {
        for (paint, item) in [
            (Paint::Decided(1), 0),
            (Paint::Decided(1), 2),
            (Paint::Choosing, 1),
        ] {
            assert_eq!(item_rgb(paint, item, since, true), Some(RGB_LIT));
            assert_eq!(item_rgb(paint, item, since, false), Some(RGB_DIM));
        }
        assert_eq!(item_rgb(Paint::Game, 1, since, true), None);
    }
}

#[test]
fn predicted_ticks_neither_press_nor_sound_nor_tell() {
    let mut r = Rig::ready();
    r.step(0, 0, true);
    let out = r.step(DOWN, 0, false);
    assert_eq!(out.cursor_se, 0);
    assert_eq!(
        r.pick.cursor[0], CHARA_ITEM,
        "予測の tick も仮のカーソルは進める"
    );
    r.step(0, 0, false);
    r.step(Z, 0, false);
    let out = r.step(0, Z, false);
    assert_eq!(r.pick.decided, Some(CHARA_ITEM));
    assert_eq!(out.decided, None);
    for _ in 0..5 {
        let out = r.step(0, 0, false);
        assert_eq!(out.word2 & MENU_KEYS, 0, "予測の tick で差し込んだ");
    }
    assert_eq!(r.menu.state, end_menu::REMATCH);
    let out = r.step(0, 0, true);
    assert_eq!(out.word2 & MENU_KEYS, DOWN, "確定の tick で始める");
}

/// 人のワードの列（押す・離すが混ざる。EscとXも含む）。
fn words(seed: u64, n: u32) -> Vec<(u16, u16)> {
    let keys = [0, 0, 0, UP, DOWN, Z, ENTER, X, ESC, UP | Z, DOWN | UP];
    (0..u64::from(n))
        .map(|i| {
            let a = keys[(mix(seed ^ i) % keys.len() as u64) as usize];
            let b = keys[(mix(seed ^ i ^ 0xB0B) % keys.len() as u64) as usize];
            (a, b)
        })
        .collect()
}

#[test]
fn a_copy_run_with_wrong_predictions_and_put_back_gives_the_same_bytes_as_a_straight_run() {
    let mut runs = 0;
    for seed in 0..40u64 {
        let plan = words(seed, 120);
        let open_at = 10 + (seed % 7) as usize;
        let tick = |r: &mut Rig, i: usize, (p1, p2): (u16, u16), confirmed: bool| {
            if i == open_at {
                r.menu = EndMenu::open(20);
            }
            r.step(p1, p2, confirmed)
        };
        let mut straight = Rig::new();
        let want: Vec<Out> = plan
            .iter()
            .enumerate()
            .map(|(i, &w)| tick(&mut straight, i, w, true))
            .collect();
        for cut in [open_at, open_at + 6, open_at + 13, open_at + 30] {
            for ahead in [1usize, 4, 16] {
                let mut r = Rig::new();
                let mut got = Vec::new();
                for (i, &w) in plan.iter().enumerate().take(cut) {
                    got.push(tick(&mut r, i, w, true));
                }
                let copy = r;
                // 相手のワードは直前の確定の繰り返し（2Pを予測）。
                let held = plan[cut - 1].1;
                for (i, &(p1, _)) in plan.iter().enumerate().skip(cut).take(ahead) {
                    tick(&mut r, i, (p1, held), false);
                }
                r = copy;
                for (i, &w) in plan.iter().enumerate().skip(cut) {
                    got.push(tick(&mut r, i, w, true));
                }
                assert_eq!(got, want, "種 {seed}・切れ目 {cut}・予測 {ahead}");
                assert_eq!(r.closed, straight.closed, "種 {seed}・切れ目 {cut}");
                runs += 1;
            }
        }
        assert!(
            straight.closed.is_some(),
            "種 {seed}: メニューが閉じない（検査の前提）"
        );
    }
    assert_eq!(runs, 40 * 4 * 3);
}

#[test]
fn a_confirm_sounds_decide_taking_it_back_sounds_cancel_and_the_deciding_confirm_is_left_to_the_game()
 {
    let se = |o: &Out| (o.cursor_se, o.decide_se, o.cancel_se);
    let mut r = Rig::ready();
    r.step(0, 0, true);
    assert_eq!(se(&r.step(Z, 0, true)), (0, 1, 0), "1P の確定");
    r.step(0, 0, true);
    assert_eq!(
        se(&r.step(Z, 0, true)),
        (0, 0, 0),
        "確定のまま押し直しても鳴らさない"
    );
    r.step(0, 0, true);
    assert_eq!(
        se(&r.step(DOWN, 0, true)),
        (0, 0, 1),
        "とりやめはカーソルの音の代わり"
    );
    r.step(0, 0, true);
    assert_eq!(
        se(&r.step(UP, 0, true)),
        (1, 0, 0),
        "確定していない移動はカーソル"
    );
    r.step(0, 0, true);
    assert_eq!(
        se(&r.step(Z, 0, false)),
        (0, 0, 0),
        "予測の tick は鳴らさない"
    );
    r.step(0, 0, true);
    let out = r.step(0, Z, true);
    assert_eq!(out.decided, Some(REMATCH_ITEM), "検査の前提");
    assert_eq!(se(&out), (0, 0, 0), "決まる確定はゲームが鳴らす");
    // 2人が同じtickに、片方は確定・片方はとりやめ。
    let mut r = Rig::ready();
    r.step(0, Z, true);
    r.step(0, 0, true);
    let out = r.step(Z, DOWN, true);
    assert_eq!(out.decided, None);
    assert_eq!(se(&out), (0, 1, 1));
    assert_eq!(
        out.sounds().collect::<Vec<_>>(),
        vec![Se::Decide, Se::Cancel]
    );
}
