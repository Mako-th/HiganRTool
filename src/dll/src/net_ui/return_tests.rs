//! 対戦の後に接続が終わったときの戻り道（`NetUi::on_lost` → `tick` の先頭）の検査。偽のゲーム `RetGame` は、一時停止
//! （`0x00434740`）・終わりのメニュー（`0x004355F0`）・MainMenu の画面 1・6・7・8・14・15・16 を、逆アセンブルの処理の順
//! （入力の地点の feed → その tick のハンドラが押した瞬間を読む）で動かす。ゲームのメモリへは何も書かれないこと
//! （`write_menu_u32` が呼ばれない）も見る。
use std::cell::Cell;

use higanr_core::lockstep::Role;
use higanr_core::menu_model::DECIDE_BITS;
use higanr_core::net_menu::{T_AGAIN, T_RETURN_LOST, T_RETURN_PEER_LEFT, T_RETURN_SELF_CUT};
use higanr_core::return_flow::{HOLD_TICKS, MenuState, PlayView, end_menu, pause};
use higanr_core::selection::Agreed;
use higanr_core::ui_entry::{YOKAI_CURSOR, cursor_after_move};
use higanr_core::wire::{Mode, Select};

use super::*;
use crate::net_select::MenuView;
use crate::netplay::BadValue;

const Z: u16 = 0x0001;
const X: u16 = 0x0002;
const ESC: u16 = 0x0008;
const UP: u16 = 0x0010;
const DOWN: u16 = 0x0020;
const Q: u16 = pause::KEY_TO_TITLE;

thread_local! {
    static CHIMES: Cell<u32> = const { Cell::new(0) };
}

fn count_chime() {
    CHIMES.with(|c| c.set(c.get() + 1));
}

fn chimes() -> u32 {
    CHIMES.with(Cell::get)
}

#[derive(Debug, Clone)]
struct RetGame {
    ims: [InputState; 3],
    scene: u32,
    next: u32,
    loading: u32,
    paused: bool,
    pause: MenuState,
    end_menu: bool,
    end: MenuState,
    skip_mark: bool,
    screen: u32,
    sub: u32,
    trans: u32,
    anim: u32,
    cursor: u32,
    decided: [u32; 2],
    /// MainMenu の作り直しの後、画面 1 → 6 → 7 → 8 を押下なしで進む。
    auto_walk: bool,
    /// この tick の人の語と、その tick に UI が feed した 3 本。
    user: u16,
    cur_feed: [Option<u16>; 3],
    menu_words_reads: u32,
    menu_writes: u32,
    resumed: u32,
    retried: u32,
    rematch: u32,
    mode_decides: u32,
    frozen: u32,
}

impl RetGame {
    fn in_match() -> RetGame {
        RetGame {
            ims: [InputState::new(); 3],
            scene: SCENE_MATCH,
            next: SCENE_MATCH,
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
            auto_walk: false,
            user: 0,
            cur_feed: [None; 3],
            menu_words_reads: 0,
            menu_writes: 0,
            resumed: 0,
            retried: 0,
            rematch: 0,
            mode_decides: 0,
            frozen: 0,
        }
    }

    fn on_screen(screen: u32, cursor: u32) -> RetGame {
        RetGame {
            scene: SCENE_MENU,
            next: SCENE_MENU,
            screen,
            sub: 1,
            anim: 20,
            cursor,
            ..RetGame::in_match()
        }
    }

    fn at_yokai(&self) -> bool {
        self.scene == SCENE_MENU && self.screen == 6 && self.cursor == YOKAI_CURSOR as u32
    }

    fn set_screen(&mut self, s: u32) {
        self.screen = s;
        self.sub = 0;
        self.trans = 3;
        self.anim = 0;
    }

    /// 1 tick: UI に入力を握らせ、握った feed（握らなければゲーム自身が読む人の語）でゲームを 1 フレーム進める。
    fn step(&mut self, ui: &mut NetUi, f: &UiFacts, user: u16) -> UiOut {
        self.user = user;
        self.cur_feed = [None; 3];
        self.menu_words_reads = 0;
        let out = ui.tick(self, f);
        assert!(self.menu_words_reads <= 1, "人の語を 1 tick に 2 回読んだ");
        if out.save_quit {
            // 呼び手（`script_win::return_save_quit_tick`）が保存の画面を飛ばす印を立てる。
            self.skip_mark = true;
        }
        let words = match out.action {
            Some(Action::RenderOnly) => {
                assert_eq!(self.cur_feed, [None; 3], "描画だけのフレームで feed した");
                self.frozen += 1;
                return out;
            }
            Some(_) => match self.cur_feed {
                [Some(a), Some(b), Some(c)] => [a, b, c],
                other => panic!("握ったのに 3 本を feed していない: {other:?}"),
            },
            None => {
                assert_eq!(self.cur_feed, [None; 3], "握らないのに feed した");
                [user, 0, user]
            }
        };
        self.frame(words);
        out
    }

    fn frame(&mut self, words: [u16; 3]) {
        for (im, w) in self.ims.iter_mut().zip(words) {
            im.feed(w);
        }
        let p0 = self.ims[0].pressed;
        let p2 = self.ims[2].pressed;
        let edge2 = p2 | self.ims[2].repeat;
        if self.next != self.scene {
            if self.next == 10 {
                self.next = SCENE_MATCH;
                self.loading = 20;
                return;
            }
            self.scene = SCENE_MENU;
            self.next = SCENE_MENU;
            self.paused = false;
            self.end_menu = false;
            if self.auto_walk {
                self.screen = 1;
                self.sub = 0;
                self.trans = 1;
            } else {
                self.set_screen(14);
            }
            return;
        }
        if self.scene == SCENE_MATCH {
            self.match_frame(p2);
        } else {
            self.menu_frame(p0, p2, edge2);
        }
    }

    fn match_frame(&mut self, p: u16) {
        if self.loading > 0 {
            self.loading -= 1;
            return;
        }
        if self.paused {
            self.pause_menu(p);
        } else if self.end_menu {
            self.end_menu_frame(p);
        } else if p & ESC != 0 {
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
        let ready = m.frames >= 4;
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
                self.next = SCENE_MENU;
                self.auto_walk = true;
            }
            9 if m.frames >= 20 => {
                m.state = INIT;
                self.paused = false;
                self.retried += 1;
            }
            _ => {}
        }
        self.pause.frames += 1;
    }

    fn end_menu_frame(&mut self, p: u16) {
        use end_menu::*;
        let m = &mut self.end;
        let ready = m.frames >= 4;
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
                        self.next = SCENE_MENU;
                        self.auto_walk = true;
                    }
                    _ => self.next = REQ_SAVE_AND_QUIT,
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
                self.cursor =
                    higanr_core::ui_entry::cursor_after_move_n(self.cursor as i32, edge2, 8) as u32;
                if self.anim >= 10 && p2 & DECIDE_BITS != 0 && self.cursor == 2 {
                    self.set_screen(6);
                    self.cursor = 0;
                }
            }
            6 => {
                self.cursor = cursor_after_move(self.cursor as i32, edge2) as u32;
                if p2 & DECIDE_BITS != 0 {
                    self.mode_decides += 1;
                }
            }
            7 if p2 & X != 0 => {
                self.set_screen(6);
                self.cursor = 0;
            }
            8 if p0 & X != 0 => {
                if self.decided[0] != 0 {
                    self.decided[0] = 0;
                } else {
                    self.set_screen(7);
                }
            }
            16 if p2 & X != 0 => self.set_screen(8),
            _ => {}
        }
    }
}

impl Game for RetGame {
    fn scenes(&mut self) -> (u32, u32) {
        (self.scene, self.next)
    }
    fn local_input(&mut self, _seat: Seat) -> u16 {
        self.user
    }
    fn menu_words(&mut self, _seat: Seat) -> [u16; 3] {
        self.menu_words_reads += 1;
        [self.user, 0, self.user]
    }
    fn feed(&mut self, index: u32, word: u16) {
        if let Some(s) = self.cur_feed.get_mut(index as usize) {
            assert!(s.is_none(), "同じ tick に index {index} を 2 回 feed した");
            *s = Some(word);
        }
    }
    fn input_state(&mut self, index: usize) -> InputState {
        self.ims[index]
    }
    fn own_select(&mut self, _s: Seat, _r: Role) -> Select {
        unreachable!("戻り道の検査は止める地点へ行かない")
    }
    fn apply_agreed(&mut self, _p: Seat, _a: &Agreed) -> Result<(), BadValue> {
        unreachable!()
    }
    fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
        unreachable!()
    }
    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        (0, 0, 0, 0)
    }
    fn menu_view(&mut self) -> Option<MenuView> {
        (self.scene == SCENE_MENU).then_some(MenuView {
            screen: self.screen,
            sub: self.sub,
            trans: self.trans,
            cursor: self.cursor,
            decided: self.decided,
            ..MenuView::default()
        })
    }
    fn write_menu_u32(&mut self, _offset: u32, _value: u32) -> bool {
        self.menu_writes += 1;
        false
    }
    fn play_view(&mut self) -> Option<PlayView> {
        (self.scene == SCENE_MATCH).then_some(PlayView {
            loading: self.loading > 0,
            paused: self.paused,
            end_menu: self.end_menu,
            pause: self.paused.then_some(self.pause),
            end: self.end_menu.then_some(self.end),
        })
    }
}

fn returning_ui() -> (NetUi, &'static Board) {
    let b: &'static Board = Box::leak(Box::new(Board::new()));
    let ui = NetUi::new(
        Shape {
            mode: Mode::Rollback,
            delay: 1,
        },
        Arc::new(UiLink::new()),
        b,
    )
    .with_return_flow(Some(count_chime));
    (ui, b)
}

fn finished() -> UiFacts {
    UiFacts {
        conn: Conn::Finished,
        seat: Seat::P1,
        now: 1_000_000,
        net: NetPhase::Open,
        stall: None,
    }
}

fn row(b: &Board, i: usize) -> String {
    let mut buf = [0u8; MAX_TEXT_UTF8];
    b.read_row(i, &mut buf)
        .map(|(t, _)| t.to_string())
        .unwrap_or_default()
}

/// 人は `every` tick ごとに、そのときの画面に合う 1 つを押す（一時停止の「タイトルへ戻る」→ 確かめの「はい」・終わりのメニューの決定）。
fn tapper(g: &RetGame, t: u32) -> u16 {
    if !t.is_multiple_of(8) || g.scene != SCENE_MATCH {
        return 0;
    }
    if g.paused {
        return match g.pause.state {
            pause::TITLE | pause::YES => Z,
            pause::NO => DOWN,
            _ => 0,
        };
    }
    if g.end_menu && g.end.state == end_menu::SAVE {
        return Z;
    }
    0
}

fn run(ui: &mut NetUi, g: &mut RetGame, f: &UiFacts, ticks: u32) -> u32 {
    let mut save_quits = 0;
    for t in 0..ticks {
        if !ui.returning() {
            break;
        }
        let u = tapper(g, t);
        if g.step(ui, f, u).save_quit {
            save_quits += 1;
        }
    }
    save_quits
}

#[test]
fn a_cut_in_the_match_freezes_then_locks_the_pause_and_lands_on_yokai() {
    let (mut ui, b) = returning_ui();
    let mut g = RetGame::in_match();
    let c0 = chimes();
    ui.on_lost(Why::Lost);
    assert_eq!(chimes(), c0 + 1, "音を 1 回");
    for t in 0..HOLD_TICKS {
        let out = g.step(&mut ui, &finished(), Z | ESC | Q | UP);
        assert_eq!(out.action, Some(Action::RenderOnly), "tick {t}");
    }
    assert_eq!(
        row(b, ROW_TL0),
        T_RETURN_LOST.ja,
        "試合の場面に理由の行が出ていない"
    );
    assert_eq!(row(b, ROW_TL1), T_AGAIN.ja);
    assert_ne!(b.show() & SHOW_TL0, 0);
    // 押しっぱなしの Esc・Q・決定は、一時停止を開いた後も押下にならない。
    for _ in 0..30 {
        g.step(&mut ui, &finished(), Z | ESC | Q);
    }
    assert!(g.paused);
    assert_eq!(g.pause.state, pause::TITLE);
    assert_eq!(g.resumed, 0);
    let sq = run(&mut ui, &mut g, &finished(), 3_000);
    assert!(g.at_yokai(), "{g:?}");
    assert_eq!(
        (g.resumed, g.retried, g.rematch, g.mode_decides),
        (0, 0, 0, 0)
    );
    assert_eq!(sq, 0);
    assert_eq!(g.menu_writes, 0, "MainMenu に書いた");
    let st = ui.stats();
    assert_eq!(
        (st.returns, st.return_arrived, st.return_gave_up),
        (1, 1, 0)
    );
    // 着いた後はいつもの画面 6（決定で入口のメニューが開く）。理由の行は開くまで残る。
    g.step(&mut ui, &finished(), 0);
    assert_eq!(row(b, ROW_TL0), T_RETURN_LOST.ja);
    g.step(&mut ui, &finished(), Z);
    g.step(&mut ui, &finished(), 0);
    assert!(ui.menu_active(), "着いた後の決定で入口のメニューが開かない");
    assert_eq!(g.mode_decides, 0, "決定がゲームへ漏れた");
    g.step(&mut ui, &finished(), 0);
    assert_eq!(ui.lost, None, "メニューを開いた後も理由の行を持っている");
}

#[test]
fn a_cut_at_the_end_menu_saves_and_quits_skips_the_save_screen_and_walks_from_the_title() {
    for why in [Why::PeerLeft, Why::Lost] {
        let (mut ui, b) = returning_ui();
        let mut g = RetGame::in_match();
        g.end_menu = true;
        ui.on_lost(why);
        let sq = run(&mut ui, &mut g, &finished(), 4_000);
        assert_eq!(sq, 1, "{why:?}: 保存して終了を伝えるのは 1 回");
        assert!(g.at_yokai(), "{why:?}: {g:?}");
        assert_eq!((g.rematch, g.mode_decides), (0, 0));
        let want = if why == Why::PeerLeft {
            T_RETURN_PEER_LEFT.ja
        } else {
            T_RETURN_LOST.ja
        };
        assert_eq!(row(b, ROW_TL0), want);
    }
}

#[test]
fn a_cut_in_the_selection_cancels_back_to_screen_six() {
    for (screen, decided) in [(8, [0, 0]), (8, [1, 1]), (16, [0, 0]), (7, [0, 0])] {
        let (mut ui, _b) = returning_ui();
        let mut g = RetGame::on_screen(screen, 3);
        g.decided = decided;
        ui.on_lost(Why::PeerLeft);
        for _ in 0..1_500 {
            if !ui.returning() {
                break;
            }
            // 人が取消と決定を押し続けても、戻り道の押し方は変わらない。
            g.step(&mut ui, &finished(), X | Z);
        }
        assert!(ui.returning(), "画面 {screen}: 決定を押したまま手を返した");
        for _ in 0..10 {
            g.step(&mut ui, &finished(), 0);
        }
        assert!(!ui.returning(), "画面 {screen}");
        assert!(g.at_yokai(), "画面 {screen}: {g:?}");
        assert_eq!(g.mode_decides, 0);
        assert!(!ui.menu_active());
    }
}

#[test]
fn the_flow_survives_the_teardown_and_keeps_going_on_the_menu_only_path() {
    let (mut ui, b) = returning_ui();
    let mut g = RetGame::on_screen(8, 0);
    ui.on_lost(Why::PeerLeft);
    for _ in 0..(HOLD_TICKS + 5) {
        g.step(&mut ui, &finished(), 0);
    }
    ui.after_teardown(Conn::Finished);
    let after = UiFacts {
        conn: Conn::Waiting,
        net: NetPhase::Closed,
        ..finished()
    };
    assert!(ui.returning(), "片づけで戻り道が消えた");
    for _ in 0..600 {
        if !ui.returning() {
            break;
        }
        g.step(&mut ui, &after, 0);
    }
    assert!(g.at_yokai(), "{g:?}");
    g.step(&mut ui, &after, 0);
    assert_eq!(
        row(b, ROW_TL0),
        T_RETURN_PEER_LEFT.ja,
        "片づけた後の理由の行"
    );
}

#[test]
fn leaving_by_the_confirm_box_and_runs_without_the_flow_do_not_return() {
    let (mut ui, _b) = returning_ui();
    let c0 = chimes();
    ui.left_by_menu = true;
    ui.on_lost(Why::Lost);
    assert!(!ui.returning(), "確かめの枠で抜けたのに戻した");
    assert_eq!(chimes(), c0);
    // 印は 1 回で消える。相手の BYE が先に来たときは戻す。
    ui.left_by_menu = true;
    ui.on_lost(Why::PeerLeft);
    assert!(ui.returning());
    // 戻り道を置かない実行（offer / answer・観戦の実行・ほかの検査）は今までどおり。
    let link = Arc::new(UiLink::new());
    let b: &'static Board = Box::leak(Box::new(Board::new()));
    let mut plain = NetUi::new(
        Shape {
            mode: Mode::Rollback,
            delay: 1,
        },
        link,
        b,
    );
    plain.on_lost(Why::PeerLeft);
    assert!(!plain.returning());
    let mut g = RetGame::in_match();
    assert_eq!(g.step(&mut plain, &finished(), ESC).action, None);
    assert_eq!(row(b, ROW_TL0), "");
}

#[test]
fn a_self_cut_does_not_chime_but_still_returns_with_its_own_line() {
    let (mut ui, b) = returning_ui();
    let c0 = chimes();
    ui.on_lost(Why::SelfCut);
    assert_eq!(chimes(), c0);
    assert!(ui.returning());
    let mut g = RetGame::on_screen(7, 0);
    g.step(&mut ui, &finished(), 0);
    assert_eq!(row(b, ROW_TL0), T_RETURN_SELF_CUT.ja);
}

#[test]
fn staying_at_the_title_after_ones_own_save_and_quit_touches_nothing() {
    // 自分で「保存して終了」を選んだときは `NetPlay::take_lost` が理由を返さない（`netplay::tests`）ので、戻り道は始まらない。
    let (mut ui, _b) = returning_ui();
    let mut g = RetGame::on_screen(1, 0);
    for _ in 0..200 {
        let out = g.step(&mut ui, &finished(), 0);
        assert_eq!(out.action, None, "タイトルで入力を握った");
    }
    assert_eq!((g.screen, g.cursor), (1, 0));
}

#[test]
fn unreadable_match_menus_leave_the_game_to_its_own_input() {
    let (mut ui, _b) = returning_ui();
    let mut g = RetGame::in_match();
    g.paused = true;
    g.pause.state = 77;
    ui.on_lost(Why::Lost);
    for _ in 0..HOLD_TICKS {
        g.step(&mut ui, &finished(), 0);
    }
    let out = g.step(&mut ui, &finished(), ESC);
    assert_eq!(out.action, None, "読めない状態で入力を握った");
    assert!(!ui.returning());
    assert_eq!(ui.stats().return_gave_up, 1);
}

/// 組み上がり: 2 台の `NetPlay` を偽のネットワークでつなぎ、片方が閉じたら、もう片方の `net_input` が `take_lost` を受けて戻り始める。
#[test]
fn net_input_starts_the_flow_when_the_peer_leaves_after_the_match_was_made() {
    use crate::netplay::fake::{FRAME_US, FakeNet};
    let (a, bb) = (
        std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
        std::net::SocketAddr::from(([127, 0, 0, 1], 2)),
    );
    let (mut na, mut nb, clock) = FakeNet::pair(a, bb, 1_000);
    let cfg = crate::netplay::Config {
        check: crate::netplay::CheckSource::InputDigest,
        ..crate::netplay::Config::default()
    };
    let link = |peer: u16, nonce: u64| {
        higanr_core::link::Config::single(
            higanr_core::handshake::Config {
                session_id: 9,
                role_nonce: nonce,
                latency: 2,
                name: higanr_core::handshake::Name::new("t"),
                host_seat: Seat::P1,
            },
            higanr_core::auth::Secret([5; 16]),
            std::net::SocketAddr::from(([127, 0, 0, 1], peer)),
            higanr_core::seal::KeySeed([0x4B; 32]),
        )
    };
    let mut np_a = NetPlay::new(link(2, 0xF000), cfg, &mut na);
    let mut np_b = NetPlay::new(link(1, 1), cfg, &mut nb);
    let (mut ui_a, _) = returning_ui();
    let (mut ui_b, _) = returning_ui();
    // どちらも Match の画面以外（画面 7）にいる。戻り道が無ければ入力に触らない画面。
    let mut ga = RetGame::on_screen(7, 0);
    let mut gb = RetGame::on_screen(7, 0);
    let mut tick = |np_a: &mut NetPlay, np_b: &mut NetPlay, ui_a: &mut NetUi, ui_b: &mut NetUi| {
        ga.cur_feed = [None; 3];
        gb.cur_feed = [None; 3];
        net_input(Some(ui_a), &mut ga, np_a, &mut na, 1_000_000);
        net_input(Some(ui_b), &mut gb, np_b, &mut nb, 1_000_000);
        clock.set(clock.get() + FRAME_US);
    };
    for _ in 0..300 {
        tick(&mut np_a, &mut np_b, &mut ui_a, &mut ui_b);
        if np_a.phase() == Phase::Selecting && np_b.phase() == Phase::Selecting {
            break;
        }
    }
    assert_eq!(np_b.phase(), Phase::Selecting, "検査の前提: 成立した");
    assert!(!ui_b.returning());
    let c0 = chimes();
    np_a.close(ByeReason::Cancelled);
    for _ in 0..30 {
        tick(&mut np_a, &mut np_b, &mut ui_a, &mut ui_b);
    }
    assert_eq!(
        np_b.end(),
        Some(crate::netplay::End::PeerBye(ByeReason::Cancelled))
    );
    assert!(ui_b.returning(), "相手が抜けたのに戻り始めない");
    assert_eq!(ui_b.lost, Some(Why::PeerLeft));
    // 閉じた側は自分から閉じた BYE(取消)（確かめの枠ではない）なので、接続が切れた扱いで戻る。
    assert!(ui_a.returning());
    assert_eq!(chimes(), c0 + 2);
    assert_eq!(np_b.take_lost(), None, "2 回目も理由を返した");
}

thread_local! {
    static LOGGED: std::cell::RefCell<Vec<UiStats>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn record_log(st: &UiStats) {
    LOGGED.with(|l| l.borrow_mut().push(*st));
}

fn logged() -> Vec<UiStats> {
    LOGGED.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// 壊れたら落ちる: 戻り道の始まり・到着・途中でやめた理由が、本ログへ出す仕事に渡らない（実機の回で合否が読めない）。
#[test]
fn the_return_log_hears_the_start_the_arrival_and_the_reason_for_giving_up() {
    let _ = logged();
    let (ui, _b) = returning_ui();
    let mut ui = ui.with_return_log(Some(record_log));
    let mut g = RetGame::in_match();
    ui.on_lost(Why::PeerLeft);
    let first = logged();
    assert_eq!(first.len(), 1);
    assert_eq!((first[0].returns, first[0].return_arrived), (1, 0));
    run(&mut ui, &mut g, &finished(), 3_000);
    assert!(g.at_yokai(), "{g:?}");
    let after = logged();
    assert_eq!(after.len(), 1, "到着は 1 回だけ伝える");
    let s = after[0];
    assert_eq!((s.returns, s.return_arrived, s.return_gave_up), (1, 1, 0));
    assert_eq!(s.return_why, 0);

    // 読めない状態で諦めた: 理由の印が残る。
    let (ui, _b) = returning_ui();
    let mut ui = ui.with_return_log(Some(record_log));
    let mut g = RetGame::in_match();
    g.paused = true;
    g.pause.state = 77;
    ui.on_lost(Why::Lost);
    for _ in 0..HOLD_TICKS {
        g.step(&mut ui, &finished(), 0);
    }
    g.step(&mut ui, &finished(), ESC);
    let all = logged();
    let last = all.last().unwrap();
    assert_eq!((last.returns, last.return_gave_up), (1, 1));
    assert_eq!(
        last.return_why,
        higanr_core::return_flow::GiveUp::PauseState(77).encode()
    );
}
