use super::*;
use crate::ui_entry::{cursor_after_move, cursor_after_move_n};

const ENTER: u16 = 0x1000;
const Q: u16 = pause::KEY_TO_TITLE;

/// ゲームの模型。逆アセンブルの処理の順そのまま（入力の地点の feed → その tick のハンドラが `+0x06`（押した瞬間）を読む）。
/// 場面 2 は一時停止（`0x00434740`）と終わりのメニュー（`0x004355F0`）、場面 1 は MainMenu の画面 1・6・7・8・14・15・16。
#[derive(Debug, Clone)]
struct Sim {
    ims: [InputState; 3],
    scene: u32,
    next: u32,
    loading: u32,
    paused: bool,
    pause: MenuState,
    end_menu: bool,
    end: MenuState,
    /// 画面 15 で保存を飛ばす印（`save_skip`）が立っている。
    skip_mark: bool,
    screen: u32,
    sub: u32,
    trans: u32,
    /// `[M+0x1B230]`（タイトルの決定は 10 以上で読む —— `0x0042A127`）。
    anim: u32,
    cursor: i32,
    decided: [u32; 2],
    extra: bool,
    /// MainMenu の作り直しの後、画面 1 → 6 → 7 → 8 を押下なしで進む（`[M+0x1B23C]`）。
    auto_walk: bool,
    /// 起きてはいけないことの記録。
    resumed: u32,
    retried: u32,
    rematch: u32,
    chara_change: u32,
    leaked: u32,
    wrong_decide: u32,
    /// 画面 6 で決定が押された（カーソルを問わず）。
    mode_decides: u32,
    x_presses: u32,
    frozen: u32,
}

const MATCH: u32 = 2;
const MENU: u32 = 1;

impl Sim {
    fn in_match() -> Sim {
        Sim {
            ims: [InputState::new(); 3],
            scene: MATCH,
            next: MATCH,
            loading: 0,
            paused: false,
            pause: MenuState {
                state: 0,
                frames: 0,
            },
            end_menu: false,
            end: MenuState {
                state: 0,
                frames: 0,
            },
            skip_mark: false,
            screen: 0,
            sub: 0,
            trans: 0,
            anim: 0,
            cursor: 0,
            decided: [0; 2],
            extra: true,
            auto_walk: false,
            resumed: 0,
            retried: 0,
            rematch: 0,
            chara_change: 0,
            leaked: 0,
            wrong_decide: 0,
            mode_decides: 0,
            x_presses: 0,
            frozen: 0,
        }
    }

    fn on_screen(screen: u32, cursor: i32) -> Sim {
        Sim {
            scene: MENU,
            next: MENU,
            screen,
            sub: 1,
            cursor,
            anim: 20,
            ..Sim::in_match()
        }
    }

    fn facts(&self) -> Facts {
        Facts {
            in_match: self.scene == MATCH && self.next == MATCH,
            in_menu: self.scene == MENU && self.next == MENU,
            save_quit: self.scene == MATCH && self.next == 6,
            play: (self.scene == MATCH).then_some(PlayView {
                loading: self.loading > 0,
                paused: self.paused,
                end_menu: self.end_menu,
                pause: Some(self.pause),
                end: Some(self.end),
            }),
            menu: (self.scene == MENU && self.next != MATCH).then_some(Screen {
                id: self.screen,
                sub: self.sub,
                trans: self.trans,
                cursor: self.cursor,
                decided: self.decided,
            }),
        }
    }

    fn set_screen(&mut self, s: u32) {
        self.screen = s;
        self.sub = 0;
        self.trans = 3;
        self.anim = 0;
    }

    /// 1 フレーム。`out` は呼び手の答え、`user` は人の語（`Pass` ならゲーム自身がそれを読む）。
    fn frame(&mut self, out: Out, user: u16) {
        let words = match out {
            Out::Freeze => {
                self.frozen += 1;
                return;
            }
            Out::Feed(w) => w,
            Out::Pass => [user, 0, user],
        };
        for (im, w) in self.ims.iter_mut().zip(words) {
            im.feed(w);
        }
        let p0 = self.ims[0].pressed;
        let p2 = self.ims[2].pressed;
        let edge2 = p2 | self.ims[2].repeat;
        // 場面の切替は更新の後半（今 ← 次）。
        let switching = self.next != self.scene;
        match self.scene {
            MATCH if !switching => self.match_frame(p2),
            MENU if !switching => self.menu_frame(p0, p2, edge2),
            _ => {}
        }
        if switching {
            if self.next == 10 {
                // 再戦: 場面 2 を作り直して読み込む。
                self.next = MATCH;
                self.loading = 30;
                return;
            }
            // 要求 1 と 6 はどちらも MainMenu を作る（6 は画面 14 から）。
            self.scene = MENU;
            self.next = MENU;
            self.paused = false;
            self.end_menu = false;
            if self.auto_walk {
                // 1 → 6 → 7 → 8 を 2 フレームで（実機の記録 M1d §7-1）。最初のフレームは画面 1 の作る途中。
                self.screen = 1;
                self.sub = 0;
                self.trans = 1;
            } else {
                self.set_screen(14);
            }
        }
    }

    fn match_frame(&mut self, p2: u16) {
        if self.loading > 0 {
            self.loading -= 1;
            return;
        }
        if self.paused {
            self.pause_menu(p2);
        } else if self.end_menu {
            self.end_menu_frame(p2);
        } else if p2 & ESC != 0 {
            // GameManager の tick（`0x0041AB8C`）。メニューは次のフレームから。
            self.paused = true;
        }
    }

    fn pause_menu(&mut self, p: u16) {
        use pause::*;
        let m = &mut self.pause;
        if p & ESC != 0 && m.state != RESUMING {
            m.state = RESUMING;
            m.frames = 0;
        }
        if p & Q != 0 && m.state != LEAVING {
            m.state = LEAVING;
            m.frames = 0;
        }
        let ready = m.frames >= MENU_READ_FRAMES;
        match m.state {
            INIT => {
                m.state = RESUME;
                m.frames = 0;
            }
            RESUME if ready => {
                if p & (UP | DOWN) != 0 {
                    m.state = TITLE;
                }
                if p & DECIDE_BITS != 0 {
                    m.state = RESUMING;
                    m.frames = 0;
                }
            }
            TITLE if ready => {
                if p & (UP | DOWN) != 0 {
                    m.state = RESUME;
                }
                if p & DECIDE_BITS != 0 {
                    m.state = NO;
                    m.frames = 0;
                }
            }
            RESUMING if m.frames >= 20 => {
                m.state = INIT;
                self.paused = false;
                self.resumed += 1;
            }
            YES | NO if ready => {
                let orig = m.state;
                if p & (UP | DOWN) != 0 {
                    m.state = if orig == YES { NO } else { YES };
                }
                if p & DECIDE_BITS != 0 {
                    // 決定は上下の後の状態を読む（`0x00434C3D` / `0x00434D5D`）。
                    if orig == YES {
                        m.state = if m.state == YES { LEAVING } else { 9 };
                    } else if m.state == NO {
                        m.state = TITLE;
                    }
                    m.frames = 0;
                }
            }
            LEAVING if m.frames >= 20 => {
                m.state = INIT;
                self.next = MENU;
                self.auto_walk = true;
            }
            9 if m.frames >= 20 => {
                m.state = INIT;
                self.retried += 1;
                self.paused = false;
            }
            _ => {}
        }
        self.pause.frames += 1;
    }

    fn end_menu_frame(&mut self, p: u16) {
        use end_menu::*;
        let m = &mut self.end;
        let ready = m.frames >= MENU_READ_FRAMES;
        match m.state {
            INIT => {
                m.state = REMATCH;
                m.frames = 0;
            }
            REMATCH | CHARA | SAVE if ready => {
                let orig = m.state;
                if p & UP != 0 {
                    m.state = [0, SAVE, REMATCH, CHARA][orig as usize];
                }
                if p & DOWN != 0 {
                    m.state = [0, CHARA, SAVE, REMATCH][orig as usize];
                }
                if p & DECIDE_BITS != 0 {
                    // 決定は振り分けた元の状態で決まる（`0x0043577F` / `0x004358CD` / `0x004359ED`）。
                    m.state = orig + 3;
                    m.frames = 0;
                }
            }
            4..=6 if m.frames >= 20 => {
                match m.state {
                    4 => {
                        self.rematch += 1;
                        self.next = 10;
                    }
                    5 => {
                        self.chara_change += 1;
                        self.next = MENU;
                        self.auto_walk = true;
                    }
                    _ => {
                        self.next = 6;
                    }
                }
                m.state = INIT;
                self.end_menu = false;
            }
            _ => {}
        }
        self.end.frames += 1;
    }

    fn menu_frame(&mut self, p0: u16, p2: u16, edge2: u16) {
        if self.trans > 0 {
            self.trans -= 1;
            if self.trans == 0 {
                self.sub = 0;
            }
            return;
        }
        if self.sub == 0 {
            if self.auto_walk && self.screen == 1 {
                self.screen = 8;
                self.trans = 1;
                self.auto_walk = false;
                return;
            }
            if self.screen == 14 {
                self.set_screen(15);
                return;
            }
            if self.screen == 15 && self.skip_mark {
                self.skip_mark = false;
                self.set_screen(1);
                self.cursor = 0;
                return;
            }
            self.anim += 1;
            if self.anim >= 3 {
                self.sub = 1;
                self.anim = 0;
            }
            return;
        }
        self.anim += 1;
        match self.screen {
            1 => {
                let before = self.cursor;
                let c = cursor_after_move_n(self.cursor, edge2, TITLE_ITEMS);
                self.cursor = if c != before && !self.extra && c == 1 {
                    // 未解放の Extra を飛ばす（`0x0042A0D1`: 動いた向き + 1）。
                    if c < before || (before == TITLE_ITEMS - 1 && c == 0) {
                        0
                    } else {
                        2
                    }
                } else {
                    c
                };
                if self.anim >= 10 && p2 & DECIDE_BITS != 0 {
                    if self.cursor == TITLE_MATCH_ITEM {
                        self.set_screen(6);
                        self.cursor = 0;
                    } else {
                        self.wrong_decide += 1;
                    }
                }
            }
            6 => {
                // 上下の処理の後のカーソルで決定を読む（`ui_entry` の模型）。
                self.cursor = cursor_after_move(self.cursor, edge2);
                if p2 & DECIDE_BITS != 0 {
                    self.mode_decides += 1;
                    if self.cursor == YOKAI_CURSOR {
                        self.leaked += 1;
                    }
                }
            }
            7 if p2 & X != 0 => {
                self.x_presses += 1;
                self.set_screen(6);
                self.cursor = 0;
            }
            8 if p0 & X != 0 => {
                self.x_presses += 1;
                if self.decided[0] != 0 {
                    self.decided[0] = 0;
                } else {
                    self.set_screen(7);
                }
            }
            16 if p2 & X != 0 => {
                self.x_presses += 1;
                self.set_screen(8);
            }
            _ => {}
        }
    }
}

struct Rd<'a> {
    sim: &'a Sim,
    raw: u16,
    raw_reads: u32,
}

impl Reads for Rd<'_> {
    fn raw(&mut self) -> u16 {
        self.raw_reads += 1;
        self.raw
    }
    fn im(&mut self, index: usize) -> InputState {
        self.sim.ims[index]
    }
}

struct Run {
    sim: Sim,
    flow: Flow,
    finish: Option<Finish>,
    save_quits: u32,
    ticks: u32,
    outs: Vec<Out>,
}

impl Run {
    fn new(sim: Sim) -> Run {
        Run {
            sim,
            flow: Flow::new(),
            finish: None,
            save_quits: 0,
            ticks: 0,
            outs: Vec::new(),
        }
    }

    /// 1 tick: 答えを取り、`save_quit` なら呼び手と同じに保存を飛ばす印を立て、ゲームを 1 フレーム進める。
    fn tick(&mut self, user: u16) -> Step {
        let f = self.sim.facts();
        let mut rd = Rd {
            sim: &self.sim,
            raw: user,
            raw_reads: 0,
        };
        let s = if self.finish.is_some() {
            Step {
                out: Out::Pass,
                save_quit: false,
                finish: None,
            }
        } else {
            self.flow.step(&f, &mut rd)
        };
        assert!(rd.raw_reads <= 1, "人の語を 1 tick に 2 回読んだ");
        if s.save_quit {
            self.save_quits += 1;
            self.sim.skip_mark = true;
        }
        if let Some(fin) = s.finish {
            self.finish = Some(fin);
        }
        self.outs.push(s.out);
        self.sim.frame(s.out, user);
        self.ticks += 1;
        s
    }

    fn until_done(&mut self, mut user: impl FnMut(&Sim, u32) -> u16, limit: u32) {
        for t in 0..limit {
            if self.finish.is_some() {
                return;
            }
            let u = user(&self.sim, t);
            self.tick(u);
        }
    }

    fn at_yokai(&self) -> bool {
        self.sim.scene == MENU && self.sim.screen == 6 && self.sim.cursor == YOKAI_CURSOR
    }
}

/// 決まった種の擬似乱数（xorshift）。
fn rng(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn the_hold_freezes_the_match_and_drops_every_key() {
    let mut r = Run::new(Sim::in_match());
    for t in 0..HOLD_TICKS {
        let s = r.tick(Z | ESC | Q | UP | ENTER);
        assert_eq!(s.out, Out::Freeze, "tick {t}");
    }
    assert_eq!(r.sim.frozen, HOLD_TICKS);
    assert!(!r.sim.paused);
    // 読み込み中の試合とメニューの場面は止めずに 0 を流す（読み込みを止めない）。
    let mut sim = Sim::in_match();
    sim.loading = 50;
    let mut r = Run::new(sim);
    assert_eq!(r.tick(Z).out, Out::Feed(ZERO));
    let mut r = Run::new(Sim::on_screen(8, 0));
    assert_eq!(r.tick(X).out, Out::Feed(ZERO));
}

#[test]
fn after_the_hold_it_opens_the_pause_and_moves_the_cursor_off_resume() {
    let mut r = Run::new(Sim::in_match());
    r.until_done(|_, _| 0, HOLD_TICKS + 20);
    assert!(r.sim.paused, "一時停止が開かない");
    assert_eq!(
        r.sim.pause.state,
        pause::TITLE,
        "カーソルが「タイトルへ戻る」へ動かない"
    );
    assert_eq!(r.sim.resumed, 0);
    assert_eq!(r.finish, None);
}

/// 人が何を押しても、再開・やり直し・再戦・キャラの変更・画面 6 の決定にならない。
#[test]
fn mashing_keys_never_resumes_retries_or_decides_on_screen_six() {
    let keys = [
        0,
        Z,
        X,
        ESC,
        Q,
        UP,
        DOWN,
        ENTER,
        Z | UP,
        Z | DOWN,
        ESC | Z,
        Q | Z,
    ];
    let mut reached = 0;
    for seed0 in 1..=40u64 {
        for start_end_menu in [false, true] {
            let mut sim = Sim::in_match();
            if start_end_menu {
                sim.end_menu = true;
            }
            let mut r = Run::new(sim);
            let mut seed = seed0 * 0x9E37_79B9;
            r.until_done(
                |_, _| {
                    let a = keys[(rng(&mut seed) % keys.len() as u64) as usize];
                    let b = keys[(rng(&mut seed) % keys.len() as u64) as usize];
                    a | b
                },
                6_000,
            );
            let s = &r.sim;
            assert_eq!(
                (s.resumed, s.retried, s.rematch, s.chara_change, s.leaked),
                (0, 0, 0, 0, 0),
                "種 {seed0} 終わりのメニュー {start_end_menu}"
            );
            assert_eq!(s.mode_decides, 0, "画面 6 で決定が押された（種 {seed0}）");
            if r.finish == Some(Finish::Arrived) {
                assert!(r.at_yokai());
                reached += 1;
            }
        }
    }
    // 押し続ければいつかは「タイトルへ戻る」→「はい」・保存して終了を選ぶ。0 なら検査が空。
    assert!(reached > 20, "着いた回 {reached} / 80");
}

/// 人の押し方: 一時停止の「タイトルへ戻る」で決定 →「いいえ」で決定（戻る）→ もう一度決定 → 下 →「はい」で決定。
#[test]
fn in_the_pause_no_goes_back_to_the_title_item_and_yes_leaves_to_screen_six() {
    let mut r = Run::new(Sim::in_match());
    r.until_done(|_, _| 0, HOLD_TICKS + 20);
    assert_eq!(r.sim.pause.state, pause::TITLE);
    let press = |r: &mut Run, k: u16| {
        r.tick(k);
        for _ in 0..6 {
            r.tick(0);
        }
    };
    press(&mut r, Z);
    assert_eq!(r.sim.pause.state, pause::NO);
    press(&mut r, ESC);
    assert_eq!(r.sim.pause.state, pause::NO, "Esc が再開へ通った");
    press(&mut r, Z);
    assert_eq!(
        r.sim.pause.state,
        pause::TITLE,
        "「いいえ」の決定が「タイトルへ戻る」へ戻らない"
    );
    // 押した tick の直後に見る（「再開」へ動いても、こちらが次の tick に下で戻すので後では見分けられない）。
    for k in [UP, DOWN, UP | DOWN] {
        r.tick(k);
        assert_eq!(r.sim.pause.state, pause::TITLE, "{k:#x} で「再開」へ動いた");
        for _ in 0..6 {
            r.tick(0);
        }
    }
    press(&mut r, Z);
    press(&mut r, DOWN);
    assert_eq!(r.sim.pause.state, pause::YES);
    press(&mut r, UP | Z);
    assert_eq!(
        r.sim.pause.state,
        pause::LEAVING,
        "上と決定が同じ tick: 決定だけを通す（やり直しにしない）"
    );
    r.until_done(|_, _| 0, 2_000);
    assert_eq!(r.finish, Some(Finish::Arrived), "{:?}", r.sim);
    assert!(r.at_yokai());
    assert_eq!(r.sim.x_presses, 2, "画面 8 → 7 → 6 の取消 2 回");
    assert_eq!((r.sim.resumed, r.sim.retried), (0, 0));
    assert_eq!(r.save_quits, 0);
}

#[test]
fn in_the_end_menu_only_save_and_quit_is_reachable_and_it_walks_from_the_title() {
    for extra in [true, false] {
        for start in [end_menu::INIT, end_menu::REMATCH, end_menu::CHARA] {
            let mut sim = Sim::in_match();
            sim.end_menu = true;
            sim.end.state = start;
            sim.extra = extra;
            let mut r = Run::new(sim);
            r.until_done(|_, _| 0, HOLD_TICKS + 30);
            assert_eq!(r.sim.end.state, end_menu::SAVE, "開始 {start}");
            // 決定を押しっぱなしにしても、押した瞬間の 1 回だけ。
            r.until_done(|_, t| if t < 30 { Z } else { 0 }, 40);
            r.until_done(|_, _| 0, 3_000);
            assert_eq!(
                r.finish,
                Some(Finish::Arrived),
                "extra {extra} 開始 {start}"
            );
            assert!(r.at_yokai());
            assert_eq!(r.save_quits, 1, "保存して終了を伝えるのは 1 回");
            assert_eq!(
                (r.sim.rematch, r.sim.chara_change, r.sim.wrong_decide),
                (0, 0, 0)
            );
        }
    }
}

#[test]
fn the_title_walk_reaches_match_with_and_without_extra() {
    for extra in [true, false] {
        for start in 0..TITLE_ITEMS {
            let mut sim = Sim::on_screen(1, start);
            sim.extra = extra;
            if !extra && start == 1 {
                continue;
            }
            let mut r = Run::new(sim);
            r.until_done(|_, _| 0, 1_000);
            assert_eq!(
                r.finish,
                Some(Finish::Arrived),
                "extra {extra} 開始 {start}"
            );
            assert_eq!(r.sim.wrong_decide, 0, "extra {extra} 開始 {start}");
            if !extra {
                assert_ne!(r.sim.cursor, 1);
            }
        }
    }
}

#[test]
fn cancels_on_the_selection_screens_follow_the_seat_flags() {
    // (画面, 1P・2P の旗, 取消の回数)
    for (screen, decided, xs) in [
        (8, [0, 0], 2),
        (8, [1, 0], 3),
        (8, [0, 1], 2),
        (8, [1, 1], 3),
        (16, [0, 0], 3),
        (7, [0, 0], 1),
    ] {
        let mut sim = Sim::on_screen(screen, 3);
        sim.decided = decided;
        let mut r = Run::new(sim);
        // 取消と決定を押し続けても数は変わらない。画面 6 では決定を離すまで手を返さない。
        r.until_done(|_, t| if t < 300 { X | Z } else { 0 }, 1_000);
        assert_eq!(
            r.finish,
            Some(Finish::Arrived),
            "画面 {screen} 旗 {decided:?}"
        );
        assert_eq!(r.sim.x_presses, xs, "画面 {screen} 旗 {decided:?}");
        assert!(r.at_yokai());
        assert_eq!(r.sim.mode_decides, 0);
    }
}

#[test]
fn screen_six_walks_the_shorter_way_to_four_and_never_presses_decide() {
    for start in 0..MODE_ITEMS {
        let mut r = Run::new(Sim::on_screen(6, start));
        r.until_done(|_, _| 0, 500);
        assert_eq!(r.finish, Some(Finish::Arrived), "開始 {start}");
        assert!(r.at_yokai());
        let moves = r
            .outs
            .iter()
            .filter(|o| matches!(o, Out::Feed(w) if w[2] & (UP | DOWN) != 0))
            .count();
        let want = (start - YOKAI_CURSOR)
            .rem_euclid(MODE_ITEMS)
            .min((YOKAI_CURSOR - start).rem_euclid(MODE_ITEMS));
        assert_eq!(moves as i32, want, "開始 {start}");
        for o in &r.outs {
            if let Out::Feed(w) = o {
                assert_eq!(w[2] & DECIDE_BITS, 0);
            }
        }
    }
    // 決定を押したまま着いたら、離すまで手を返さない（返すと次の tick に入口のメニューが開く）。
    let mut r = Run::new(Sim::on_screen(6, 4));
    r.until_done(|_, _| Z, 400);
    assert_eq!(r.finish, None, "決定を押したまま手を返した");
    r.until_done(|_, _| 0, 5);
    assert_eq!(r.finish, Some(Finish::Arrived));
    assert_eq!(r.sim.mode_decides, 0);
}

#[test]
fn the_save_screen_is_left_to_the_player_when_it_is_not_skipped() {
    let mut sim = Sim::on_screen(15, 0);
    sim.sub = 1;
    let mut r = Run::new(sim);
    r.until_done(|_, _| 0, HOLD_TICKS + SETTLE_TICKS + 5);
    assert_eq!(r.tick(Z).out, Out::Pass);
    assert_eq!(r.finish, None);
}

#[test]
fn values_out_of_range_give_up_without_feeding() {
    let check = |mut sim: Sim, why: GiveUp| {
        let mut r = Run::new(Sim::in_match());
        r.until_done(|_, _| 0, HOLD_TICKS);
        r.sim = {
            sim.ims = r.sim.ims;
            sim
        };
        let s = r.tick(0);
        assert_eq!(s.out, Out::Pass);
        assert_eq!(s.finish, Some(Finish::GaveUp(why)));
        assert!(r.flow.is_done());
        assert_eq!(r.tick(Z).out, Out::Pass);
    };
    let mut s = Sim::in_match();
    s.paused = true;
    s.pause.state = pause::STATES;
    check(s, GiveUp::PauseState(pause::STATES));
    let mut s = Sim::in_match();
    s.end_menu = true;
    s.end.state = end_menu::STATES;
    check(s, GiveUp::EndState(end_menu::STATES));

    // 試合の場面のメニューが読めない。
    let mut f = Flow::new();
    let mut facts = Sim::in_match().facts();
    let sim = Sim::in_match();
    let mut rd = Rd {
        sim: &sim,
        raw: 0,
        raw_reads: 0,
    };
    for _ in 0..HOLD_TICKS {
        f.step(&facts, &mut rd);
    }
    facts.play = None;
    assert_eq!(
        f.step(&facts, &mut rd).finish,
        Some(Finish::GaveUp(GiveUp::Unreadable))
    );
    // 画面 6 のカーソルが範囲の外・戻り道に無い画面。
    for (screen, cursor, why) in [
        (6, 5, GiveUp::Unreadable),
        (6, -1, GiveUp::Unreadable),
        (1, 8, GiveUp::Unreadable),
        (3, 0, GiveUp::Elsewhere(3)),
    ] {
        let mut r = Run::new(Sim::on_screen(screen, cursor));
        r.until_done(|_, _| 0, 400);
        assert_eq!(r.finish, Some(Finish::GaveUp(why)), "画面 {screen}");
    }
}

#[test]
fn presses_that_do_nothing_give_up_instead_of_holding_the_input_forever() {
    // 一時停止が開かない（読み込みでも終わりのメニューでもないのに、一時停止のバイトが立たない）。
    let mut f = Flow::new();
    let facts = Sim::in_match().facts();
    let sim = Sim::in_match();
    let mut rd = Rd {
        sim: &sim,
        raw: 0,
        raw_reads: 0,
    };
    let mut fin = None;
    let mut n = 0;
    while fin.is_none() && n < 2_000 {
        fin = f.step(&facts, &mut rd).finish;
        n += 1;
    }
    assert_eq!(fin, Some(Finish::GaveUp(GiveUp::NoPause)));
    assert!(n > HOLD_TICKS + OPEN_TICKS, "早すぎる {n}");
    // 一時停止のカーソルが「再開」から動かない。
    let mut s = Sim::in_match();
    s.paused = true;
    s.pause = MenuState {
        state: pause::RESUME,
        frames: 50,
    };
    let mut f = Flow::new();
    let facts = s.facts();
    let mut rd = Rd {
        sim: &s,
        raw: 0,
        raw_reads: 0,
    };
    let mut fin = None;
    for _ in 0..1_000 {
        fin = fin.or(f.step(&facts, &mut rd).finish);
    }
    assert_eq!(fin, Some(Finish::GaveUp(GiveUp::Stuck)));
    // 選択の画面が押下を読まないまま（移る途中が終わらない）。
    let mut s = Sim::on_screen(8, 0);
    s.trans = 5;
    let mut f = Flow::new();
    let facts = s.facts();
    let mut rd = Rd {
        sim: &s,
        raw: 0,
        raw_reads: 0,
    };
    let mut fin = None;
    for _ in 0..2_000 {
        fin = fin.or(f.step(&facts, &mut rd).finish);
    }
    assert_eq!(fin, Some(Finish::GaveUp(GiveUp::Stuck)));
}

#[test]
fn a_rematch_decided_before_the_flow_ends_in_a_new_match_that_is_left_by_the_pause() {
    let mut sim = Sim::in_match();
    sim.end_menu = true;
    sim.end = MenuState {
        state: 4,
        frames: 0,
    };
    let mut r = Run::new(sim);
    // 人は 8 tick ごとに 1 回押す（押しっぱなしは押下にならない）。
    r.until_done(
        |s, t| {
            if t % 8 != 0 || !s.paused {
                return 0;
            }
            match s.pause.state {
                pause::TITLE | pause::YES => Z,
                pause::NO => DOWN,
                _ => 0,
            }
        },
        3_000,
    );
    assert_eq!(r.finish, Some(Finish::Arrived));
    assert_eq!(r.sim.rematch, 1);
    assert_eq!(r.save_quits, 0);
}

#[test]
fn the_reason_lines_name_the_cause_and_keep_the_next_step() {
    use crate::lang::Lang;
    use crate::net_menu::{
        LineBuf, T_AGAIN, T_RETURN_LOST, T_RETURN_PEER_LEFT, T_RETURN_SELF_CUT, return_lines,
    };
    use crate::ui_layout::RowStyle;
    for (why, tx) in [
        (Why::PeerLeft, T_RETURN_PEER_LEFT),
        (Why::Lost, T_RETURN_LOST),
        (Why::SelfCut, T_RETURN_SELF_CUT),
    ] {
        for lang in [Lang::Ja, Lang::En] {
            let mut out = [LineBuf::new(); 2];
            let mut st = [RowStyle::Normal; 2];
            assert_eq!(return_lines(why, lang, &mut out, &mut st), 2);
            assert_eq!(out[0].as_str(), tx.get(lang), "{why:?}");
            assert_eq!(out[1].as_str(), T_AGAIN.get(lang));
            assert_eq!(st, [RowStyle::Warn; 2]);
        }
    }
}

/// 入力止めの前から押しっぱなしの決定は、ロックが人の語を通し始めても押下にならない（押しっぱなしで何かを選ばない）。
#[test]
fn keys_held_since_the_hold_never_become_presses_in_the_locks() {
    for end in [false, true] {
        let mut sim = Sim::in_match();
        sim.end_menu = end;
        let mut r = Run::new(sim);
        r.until_done(|_, _| Z | ENTER, HOLD_TICKS + 200);
        if end {
            assert_eq!(r.sim.end.state, end_menu::SAVE, "押しっぱなしで決めた");
            assert_eq!(r.save_quits, 0);
        } else {
            assert_eq!(r.sim.pause.state, pause::TITLE, "押しっぱなしで決めた");
        }
        // 離してから押し直すと効く。
        r.tick(0);
        r.tick(Z);
        for _ in 0..3 {
            r.tick(0);
        }
        if end {
            assert_eq!(r.sim.end.state, 6);
        } else {
            assert_eq!(r.sim.pause.state, pause::NO);
        }
    }
}

/// 「保存して終了」の切替（今 2・次 6）を何 tick 見ても、伝えるのは 1 回。
#[test]
fn save_quit_is_reported_once_even_if_the_switch_is_seen_twice() {
    let mut f = Flow::new();
    let sim = Sim::in_match();
    let mut rd = Rd {
        sim: &sim,
        raw: 0,
        raw_reads: 0,
    };
    let mut facts = sim.facts();
    for _ in 0..HOLD_TICKS {
        f.step(&facts, &mut rd);
    }
    facts.in_match = false;
    facts.save_quit = true;
    let n = (0..5).filter(|_| f.step(&facts, &mut rd).save_quit).count();
    assert_eq!(n, 1);
}

/// 一時停止が開かない間（会話の間など）は、人の決定の押した瞬間だけを通し、押している限り諦めない。
#[test]
fn while_the_pause_cannot_open_the_players_decide_passes_and_keeps_the_flow() {
    let sim = Sim::in_match();
    let facts = sim.facts();
    let mut f = Flow::new();
    let mut fin = None;
    let mut passed = 0;
    for t in 0..(HOLD_TICKS + 4 * OPEN_TICKS) {
        let raw = if t > HOLD_TICKS && t % 200 == 0 { Z } else { 0 };
        let mut rd = Rd {
            sim: &sim,
            raw,
            raw_reads: 0,
        };
        let s = f.step(&facts, &mut rd);
        fin = fin.or(s.finish);
        if let Out::Feed(w) = s.out {
            assert_eq!(w[0] | w[1], 0, "選手の席に入れた");
            assert_eq!(w[2] & !(ESC | Z), 0);
            if w[2] & Z != 0 {
                assert_eq!(raw, Z, "人が押していないのに決定を入れた");
                passed += 1;
            }
        }
    }
    assert_eq!(fin, None, "決定を押しているのに諦めた");
    assert!(passed >= 4, "決定を通した回数 {passed}");
}

/// 壊れたら落ちる: 諦めた理由の印が理由を取り違える（種類の入れ替え・番号の欠け）か、全部の理由が別々の印にならない。
#[test]
fn every_give_up_reason_has_its_own_code_and_a_sentence() {
    let all = [
        GiveUp::Unreadable,
        GiveUp::PauseState(77),
        GiveUp::EndState(9),
        GiveUp::Stuck,
        GiveUp::NoPause,
        GiveUp::NoMenu,
        GiveUp::Elsewhere(12),
    ];
    let codes: Vec<u32> = all.iter().map(|g| g.encode()).collect();
    for (i, a) in codes.iter().enumerate() {
        assert_ne!(*a, 0, "{:?}", all[i]);
        assert!(!codes[i + 1..].contains(a), "{:?} と印が重なる", all[i]);
    }
    assert!(GiveUp::describe(GiveUp::PauseState(77).encode()).contains("77"));
    assert!(GiveUp::describe(GiveUp::EndState(9).encode()).contains("終わりのメニュー"));
    assert!(GiveUp::describe(GiveUp::Elsewhere(12).encode()).contains("12"));
    assert_eq!(GiveUp::describe(0), "なし");
    assert!(GiveUp::describe(0xFF00_0001).contains("0xFF000001"));
}
