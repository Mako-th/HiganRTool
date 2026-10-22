//! マッチの中の一時停止（`pause.rs`）・マッチの外の Esc・照合の検査。ポーズのメニュー（`0x00434740`）と GameManager の模型（`Model`）を
//! ホストのゲームと観戦者のゲームに 1 つずつ持たせる。模型の振る舞いは逆アセンブルと実機の検証のとおり:
//! - 優先度 1 のポーズのメニューが先に走る: 先頭で `[2]+0x06` の Esc は状態 3（状態 3 なら何もしない）、Q は状態 8 へ。状態 3 で数えが 20 以上なら閉じ、
//!   状態 8 で 20 以上なら閉じて次の要求 1。最後に数えを 1 足す。
//! - 優先度 2 の GameManager: ポーズでも終わりのメニューでもなく `[2]+0x06 & 8` なら開いて止める。開いている間は止める。どちらでもなければ試合が
//!   `[0]`・`[1]` の語で 1 フレーム進む（`applied` に積み、乱数を混ぜる）。
//!
//! ホストは 1 tick に 1 つ `Frame`（約 1 秒ごとにその前に `Check`）を積む。ホストのポーズの間も積む（本物と同じ）。合格は、観戦者の試合が進んだ列
//! （`applied`）がホストと全行一致すること。
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use higanr_core::check::is_watch_check_frame;
use higanr_core::input_model::InputState;
use higanr_core::input_model::keys::{DOWN, ESC, LEFT, UP, X, Z};
use higanr_core::lockstep::Role;
use higanr_core::return_flow::{CLOSE_FRAMES, MENU_READ_FRAMES, MenuState, PlayView, pause};
use higanr_core::selection::Agreed;
use higanr_core::wire::{ByeReason, Check, CheckKind, Record, Seat, Select};

use super::*;
use crate::netplay::fake::{FRAME_US, FakeNet, local_word};
use crate::netplay::{Action, BadValue, Game, Phase, SCENE_MATCH};
use crate::scripted::SCENE_MENU;

const Q: u16 = pause::KEY_TO_TITLE;

fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^ (z >> 31)
}

/// ゲームの模型（1 本のマッチの中だけ）。
#[derive(Clone)]
struct Model {
    ims: [InputState; 3],
    paused: bool,
    end_menu: bool,
    state: u32,
    frames: u32,
    applied: Vec<(u16, u16)>,
    rng: u64,
    cur: u32,
    next: u32,
    /// 崩れの検査: GameManager がポーズを開かない / 閉じる数えをずらす。
    refuse_open: bool,
    close_at: u32,
}

impl Model {
    fn new() -> Self {
        Model {
            ims: [InputState::new(); 3],
            paused: false,
            end_menu: false,
            state: pause::INIT,
            frames: 0,
            applied: Vec::with_capacity(60_000),
            rng: 0x1234_5678,
            cur: SCENE_MATCH,
            next: 0,
            refuse_open: false,
            close_at: CLOSE_FRAMES,
        }
    }

    /// feed された InputManager だけを更新する（入力の地点が poll を呼ばずに戻る tick は、feed しない本はそのまま）。
    fn feed(&mut self, w: [Option<u16>; 3]) {
        for (im, w) in self.ims.iter_mut().zip(w) {
            if let Some(w) = w {
                im.feed(w);
            }
        }
    }

    fn update(&mut self) {
        if self.next == 1 {
            self.cur = SCENE_MENU;
            return;
        }
        if self.paused {
            self.menu();
        }
        let p2 = self.ims[2].pressed;
        if !self.paused && !self.end_menu && p2 & ESC != 0 && !self.refuse_open {
            self.paused = true;
            return;
        }
        if self.paused || self.end_menu {
            return;
        }
        let (a, b) = (self.ims[0].cur, self.ims[1].cur);
        self.applied.push((a, b));
        self.rng = mix(self.rng ^ (u64::from(a) << 16) ^ u64::from(b));
    }

    fn menu(&mut self) {
        let p = self.ims[2].pressed;
        if p & ESC != 0 && self.state != pause::RESUMING {
            self.state = pause::RESUMING;
            self.frames = 0;
        }
        if p & Q != 0 && self.state != pause::LEAVING {
            self.state = pause::LEAVING;
            self.frames = 0;
        }
        let read = self.frames >= MENU_READ_FRAMES;
        let to = |s: &mut Self, st: u32| {
            s.state = st;
            s.frames = 0;
        };
        match self.state {
            pause::INIT => to(self, pause::RESUME),
            pause::RESUME if read && p & Z != 0 => to(self, pause::RESUMING),
            pause::RESUME if read && p & (UP | DOWN) != 0 => self.state = pause::TITLE,
            pause::TITLE if read && p & Z != 0 => to(self, pause::NO),
            pause::TITLE if read && p & (UP | DOWN) != 0 => self.state = pause::RESUME,
            pause::NO if read && p & Z != 0 => to(self, pause::TITLE),
            pause::NO if read && p & (UP | DOWN) != 0 => self.state = pause::YES,
            pause::YES if read && p & Z != 0 => to(self, pause::LEAVING),
            pause::YES if read && p & (UP | DOWN) != 0 => self.state = pause::NO,
            pause::RESUMING if self.frames >= self.close_at => {
                self.state = pause::INIT;
                self.paused = false;
            }
            pause::LEAVING if self.frames >= CLOSE_FRAMES => {
                self.state = pause::INIT;
                self.paused = false;
                self.next = 1;
            }
            _ => {}
        }
        self.frames += 1;
    }

    fn check(&self, frame: u32) -> Check {
        Check {
            seg: 0,
            frame,
            rng_state: self.rng as u16,
            rng_count: (self.rng >> 16) as u32,
            life_1p: 10,
            life_2p: 10,
        }
    }
}

/// 観戦者のゲーム（模型 ＋ 手元の入力）。
struct WGame {
    m: Model,
    local: u16,
    fed: [Option<u16>; 3],
    readable: bool,
}

impl Game for WGame {
    fn scenes(&mut self) -> (u32, u32) {
        (self.m.cur, self.m.next)
    }
    fn local_input(&mut self, _s: Seat) -> u16 {
        self.local
    }
    fn peek_local_input(&mut self) -> u16 {
        self.local
    }
    fn feed(&mut self, i: u32, w: u16) {
        let f = &mut self.fed[i as usize];
        assert!(f.is_none(), "同じ tick に index {i} を 2 回 feed した");
        *f = Some(w);
    }
    fn own_select(&mut self, _s: Seat, _r: Role) -> Select {
        unreachable!()
    }
    fn apply_agreed(&mut self, _p: Seat, _a: &Agreed) -> Result<(), BadValue> {
        unreachable!()
    }
    fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
        true
    }
    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        let c = self.m.check(0);
        (c.rng_state, c.rng_count, c.life_1p, c.life_2p)
    }
    fn play_view(&mut self) -> Option<PlayView> {
        self.readable.then_some(PlayView {
            loading: false,
            paused: self.m.paused,
            end_menu: self.m.end_menu,
            pause: self.m.paused.then_some(MenuState {
                state: self.m.state,
                frames: self.m.frames,
            }),
            end: None,
        })
    }
}

/// ホストの n tick 目の語（Esc を含まない。`esc_at` の tick だけ 1P に Esc、`z_at` の tick だけ 1P に Z を足す）。
struct HostPlan {
    esc_at: Vec<u32>,
    z_at: Vec<u32>,
}

impl HostPlan {
    fn none() -> Self {
        HostPlan {
            esc_at: Vec::new(),
            z_at: Vec::new(),
        }
    }

    fn words(&self, n: u32) -> (u16, u16) {
        let p1 = local_word(0xA1, u64::from(n) + 1) & !ESC;
        let p2 = local_word(0xB2, u64::from(n) + 1) & !ESC;
        let extra = if self.esc_at.contains(&n) {
            ESC
        } else if self.z_at.contains(&n) {
            Z
        } else {
            0
        };
        (p1 | extra, p2)
    }
}

struct Rig {
    wp: WatchPlay,
    g: WGame,
    host: Model,
    plan: HostPlan,
    net: FakeNet,
    _peer: FakeNet,
    clock: Rc<Cell<u64>>,
    /// ホストが積んだフレームの数（次の frame の番号）。
    n: u32,
    streaming: bool,
    /// 照合を積むか（偽なら `Frame` だけ）。
    checks: bool,
    ticks: u32,
    /// 積んだ照合の記録の（frame, リングの位置）。
    check_at: Vec<(u32, u32)>,
}

impl Rig {
    fn new(plan: HostPlan) -> Rig {
        let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_501));
        let h = std::net::SocketAddr::from(([127, 0, 0, 1], 47_501));
        let (net, peer, clock) = FakeNet::pair(a, h, 0);
        let mut wp = WatchPlay::with_feed_len(
            Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 0,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: true,
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
            },
            1 << 17,
        );
        wp.set_phase_for_test(Phase::Lockstep);
        assert!(wp.feed.push(&Record::Seg { seg: 0 }, false, 0));
        Rig {
            wp,
            g: WGame {
                m: Model::new(),
                local: 0,
                fed: [None; 3],
                readable: true,
            },
            host: Model::new(),
            plan,
            net,
            _peer: peer,
            clock,
            n: 0,
            streaming: true,
            checks: true,
            ticks: 0,
            check_at: Vec::with_capacity(4_096),
        }
    }

    fn now_ms(&self) -> u32 {
        (self.clock.get() / 1_000) as u32
    }

    /// ホストの 1 tick: 照合（約 1 秒ごと。feed の前の値）→ `Frame` → ホストのゲームの更新。
    fn host_tick(&mut self) {
        let n = self.n;
        let (p1, p2) = self.plan.words(n);
        if self.checks && is_watch_check_frame(n) {
            let c = self.host.check(n);
            self.check_at.push((n, self.wp.feed.written()));
            assert!(
                self.wp
                    .feed
                    .push(&Record::check(CheckKind::Game, &c), false, 0)
            );
        }
        let t = self.clock.get() as u32;
        assert!(
            self.wp
                .feed
                .push(&Record::Frame { frame: n, p1, p2 }, false, t)
        );
        self.wp.feed.set_heard(Some(self.now_ms()));
        self.host.feed([Some(p1), Some(p2), Some(p1 | p2)]);
        self.host.update();
        self.n += 1;
    }

    /// 観戦者の 1 tick（手元の入力 `local`）。
    fn tick(&mut self, local: u16) -> Action {
        self.clock.set(self.clock.get() + FRAME_US);
        if self.streaming {
            self.host_tick();
        }
        self.scan(local)
    }

    /// 入力の地点 1 回と、その後の更新 1 回（時計とホストは進めない。早送りの余分な更新もこの形）。
    fn scan(&mut self, local: u16) -> Action {
        self.ticks += 1;
        self.g.local = local;
        self.g.fed = [None; 3];
        let a = self.wp.on_input(&mut self.g, &mut self.net);
        match a {
            Action::RenderOnly => {
                assert_eq!(self.g.fed, [None; 3], "描画だけのフレームで feed した");
                return a;
            }
            Action::Local => {
                assert_eq!(self.g.fed, [None; 3], "Local で feed した");
                // poll（`0x0042E9C0`）がキーボードを 1P と合成へ入れる。
                self.g.m.feed([Some(local), Some(0), Some(local)]);
            }
            Action::Played(_) => {
                let f = self.g.fed;
                assert!(
                    f.iter().all(Option::is_some),
                    "使ったフレームで 3 本とも feed していない: {f:?}"
                );
                self.g.m.feed(f);
            }
            _ => {
                assert!(self.g.fed[2].is_some(), "[2] を feed していない");
                self.g.m.feed(self.g.fed);
            }
        }
        self.g.m.update();
        a
    }

    fn run(&mut self, ticks: u32) {
        for _ in 0..ticks {
            self.tick(0);
        }
    }

    /// ホストを止め、観戦者が溜まりを使い切るまで回す（上限つき）。
    fn drain(&mut self) {
        self.streaming = false;
        for _ in 0..200_000 {
            if self.wp.stats().played == u64::from(self.n) || self.wp.is_ended() {
                break;
            }
            self.tick(0);
        }
    }

    fn fed(&self) -> [Option<u16>; 3] {
        self.g.fed
    }

    /// 手元で押して 1 tick 離す（押した瞬間を 1 回作る）。
    fn tap(&mut self, key: u16) -> Action {
        let a = self.tick(key);
        self.tick(0);
        a
    }

    fn assert_same_as_host(&self, tag: &str) {
        assert_eq!(
            self.wp.stats().played,
            u64::from(self.n),
            "{tag}: 使い切っていない"
        );
        assert_eq!(
            self.g.m.applied.len(),
            self.host.applied.len(),
            "{tag}: 進んだ数"
        );
        assert_eq!(self.g.m.applied, self.host.applied, "{tag}: 試合の列");
        assert_eq!(self.g.m.rng, self.host.rng, "{tag}: 乱数");
        let s = self.wp.stats();
        assert_eq!(
            s.checks_mismatched, 0,
            "{tag}: 照合がずれた {:?}",
            s.first_mismatch
        );
        assert!(
            s.checks_matched >= 2,
            "{tag}: 照合が少ない {}",
            s.checks_matched
        );
    }
}

/// 早送り（`run_display`）の走査の口。最初の更新と余分な更新で、手元の入力を分けて渡す。
struct FfRig<'a> {
    r: &'a mut Rig,
    first: u16,
    extra: u16,
    extras: u32,
    opened_in_extra: bool,
}

impl FfHost for FfRig<'_> {
    fn scan(&mut self, extra: bool) -> i32 {
        let k = if extra { self.extra } else { self.first };
        let a = self.r.scan(k);
        if extra {
            self.extras += 1;
            if self.r.wp.pause != LocalPause::Off {
                self.opened_in_extra = true;
            }
        }
        match a {
            Action::RenderOnly => 1,
            _ => 2,
        }
    }
    fn with_wp<R>(&mut self, f: impl FnOnce(&mut WatchPlay, u64) -> R) -> Option<R> {
        let now = self.r.clock.get();
        Some(f(&mut self.r.wp, now))
    }
    fn begin(&mut self) {}
    fn drop_outputs(&mut self) -> bool {
        true
    }
    fn boundary(&mut self) -> bool {
        self.r.g.m.cur != SCENE_MATCH || self.r.g.m.next != 0
    }
}

impl Rig {
    /// 1 表示（時計とホストを 1 tick 進め、走査を早送りの包みで回す）。戻りは（余分な更新の数, 余分な更新の中でポーズに入ったか）。
    fn display(&mut self, first: u16, extra: u16) -> (u32, bool) {
        self.clock.set(self.clock.get() + FRAME_US);
        if self.streaming {
            self.host_tick();
        }
        let mut h = FfRig {
            r: self,
            first,
            extra,
            extras: 0,
            opened_in_extra: false,
        };
        run_display(&mut h);
        (h.extras, h.opened_in_extra)
    }

    /// `[0]`・`[1]` の InputManager の今の状態（ポーズの前後で変わらないことを見る）。
    fn seats(&self) -> [InputState; 2] {
        [self.g.m.ims[0], self.g.m.ims[1]]
    }

    /// 終えるまで Esc を押し続けた tick の数（上限つき）。
    fn hold_until_end(&mut self, limit: u32) -> Option<u32> {
        for i in 1..=limit {
            self.tick(ESC);
            if self.wp.end().is_some() {
                return Some(i);
            }
        }
        None
    }
}

/// 3 秒（`ESC_HOLD_MS`）押し続けて終える tick: 押し始めの tick から 180 tick 後（16.667 ms × 180 ≥ 3,000 ms）。
const HOLD_END_TICK: u32 = 181;
/// 2.9 秒（押し始めの tick を含めて 175 tick）。
const SHORT_HOLD_TICKS: u32 = 175;

/// ゲームのポーズを開いて「再開」で閉じる: Esc の tick は記録を使わず `[2]` だけに最後の語と Esc を入れる。ポーズの間の `[2]` は手元の入力を
/// `MATCH_SEND_MASK` で切ったもの（Q・P/Home・Enter は落ちる）、再開を決めた後は最後の `p1|p2`。`[0]`・`[1]` は feed せず、InputManager は
/// ポーズの前と同じ。決定の tick から 20 tick 目に次の `Frame` を使い、最後までホストと同じ列になる。
#[test]
fn the_games_pause_opens_without_a_frame_and_resumes_on_the_predicted_tick() {
    let mut r = Rig::new(HostPlan::none());
    r.run(120);
    let played = r.wp.stats().played;
    assert!(played > 100, "{played}");
    let last = r.wp.last_words;
    let seats = r.seats();
    let a = r.tick(ESC);
    assert_eq!(a, Action::Zero, "Esc の tick");
    assert_eq!(r.fed(), [None, None, Some(last.0 | last.1 | ESC)]);
    assert_eq!(r.wp.stats().played, played, "Esc の tick に Frame を使った");
    assert!(r.g.m.paused, "ポーズが開いていない");
    assert_eq!(
        r.g.m.applied.len() as u64,
        played,
        "Esc の tick に試合が進んだ"
    );
    // ポーズの間: [2] は手元の生の語から、送る語の外（Q 0x200・P/Home 0x800・Enter 0x1000）を落としたもの。
    for (k, want) in [
        (X | LEFT | Q, X | LEFT),
        (0, 0),
        (Q, 0),
        (0, 0),
        (0x0800 | 0x1000 | X, X),
        (0, 0),
    ] {
        assert_eq!(r.tick(k), Action::Zero);
        assert_eq!(r.fed(), [None, None, Some(want)], "手元 {k:#x}");
        assert_eq!(r.g.m.state, pause::RESUME, "Q で状態 8 へ行った");
    }
    assert_eq!(r.wp.stats().played, played, "ポーズの間に Frame を使った");
    // 再開を決める（D）。
    let d = r.ticks + 1;
    r.tick(Z);
    assert_eq!(r.g.m.state, pause::RESUMING);
    let mut resumed = None;
    for _ in 0..40 {
        assert_eq!(r.seats(), seats, "ポーズの間に [0]・[1] が変わった");
        let a = r.tick(X | LEFT);
        if let Action::Played(rec) = a {
            resumed = Some((r.ticks, rec.frame));
            break;
        }
        assert_eq!(
            r.fed(),
            [None, None, Some(last.0 | last.1)],
            "状態 3 の間の [2]"
        );
    }
    let (at, frame) = resumed.expect("閉じる tick に Frame を使っていない");
    assert_eq!(at - d, CLOSE_FRAMES, "決定から閉じる tick まで");
    assert_eq!(u64::from(frame), played, "続きのフレーム");
    assert!(!r.g.m.paused, "閉じる tick に閉じていない");
    r.run(60);
    r.drain();
    r.assert_same_as_host("ポーズ 1 回");
    let p = r.wp.stats().pause;
    assert_eq!(
        (
            p.opened,
            p.resumed,
            p.reopened,
            p.open_slips,
            p.close_slips,
            p.unreadable
        ),
        (1, 1, 0, 0, 0, 0),
        "{p:?}"
    );
    assert!(p.ticks >= 26, "{p:?}");
    assert_eq!(r.wp.end(), None, "ポーズで観戦が終わった");
}

/// ポーズの間に `[0]`・`[1]` を毎 tick 入れないので、長押ししていたキーのリピートの数え（`+0x0A`〜）がホストより進まない。
#[test]
fn a_held_key_in_the_last_frame_does_not_repeat_on_during_the_pause() {
    let mut r = Rig::new(HostPlan::none());
    r.run(120);
    // 最後の語に押しっぱなしのキーがある形（どの語も 0 でない）。
    assert_ne!(r.wp.last_words.0, 0);
    let seats = r.seats();
    r.tap(ESC);
    r.run(300);
    assert_eq!(r.seats(), seats, "ポーズの 300 tick で [0]・[1] が変わった");
    r.tap(Z);
    r.run(40);
    r.drain();
    r.assert_same_as_host("長いポーズの後");
}

/// 再開の Esc（状態 1 からの近道）でも同じ: 決定の tick から 20 tick 目に使う。押しっぱなしの Esc は状態 3 を作り直さない。
#[test]
fn resuming_by_esc_and_holding_it_closes_on_the_same_tick() {
    let mut r = Rig::new(HostPlan::none());
    r.run(90);
    r.tap(ESC);
    r.run(8);
    let d = r.ticks + 1;
    let mut at = None;
    for i in 0..40 {
        // 再開の Esc を押したまま（状態 3 の間は [2] に入らない）。
        if let Action::Played(_) = r.tick(ESC) {
            at = Some(r.ticks);
            break;
        }
        if i == 0 {
            assert_eq!(r.g.m.state, pause::RESUMING);
        }
    }
    assert_eq!(at.map(|t| t - d), Some(CLOSE_FRAMES));
    // 閉じた後も押したままなので、押した瞬間が無くポーズは開かない。
    r.run(1);
    assert!(!r.g.m.paused);
    r.drain();
    r.assert_same_as_host("Esc で再開");
}

/// 閉じる tick に記録が無い: 描画だけのフレームで待ち（ポーズのメニューは数え 20 のまま）、記録が届いた tick に使う。
#[test]
fn the_closing_tick_waits_for_a_record_without_moving_the_menu() {
    let mut r = Rig::new(HostPlan::none());
    r.run(120);
    r.streaming = false;
    r.run(10);
    assert_eq!(r.wp.available(), 0, "溜まりが残っている");
    r.tap(ESC);
    r.run(6);
    r.tap(Z);
    while r.g.m.frames < CLOSE_FRAMES {
        assert_eq!(r.tick(0), Action::Zero);
    }
    for _ in 0..10 {
        assert_eq!(r.tick(0), Action::RenderOnly, "記録が無いのに進めた");
        assert_eq!(
            (r.g.m.paused, r.g.m.state, r.g.m.frames),
            (true, pause::RESUMING, CLOSE_FRAMES),
            "待つ間にポーズのメニューが進んだ"
        );
    }
    r.streaming = true;
    let mut played = false;
    for _ in 0..20 {
        if let Action::Played(_) = r.tick(0) {
            played = true;
            break;
        }
    }
    assert!(played, "記録が届いても使わない");
    assert!(!r.g.m.paused);
    r.run(30);
    r.drain();
    r.assert_same_as_host("閉じる tick に記録が無い");
    assert_eq!(r.wp.stats().pause.close_slips, 0);
}

/// ホストのポーズの再現中（記録で開いたポーズ）の Esc は、ゲームのポーズを開かず描画だけのフレームで止め、もう 1 回で戻る。止めた間は記録を使わない。
#[test]
fn esc_during_the_hosts_pause_holds_by_the_overlay_and_a_second_esc_resumes() {
    let mut r = Rig::new(HostPlan {
        esc_at: vec![150],
        z_at: vec![200],
    });
    while !(r.g.m.paused && r.g.m.state == pause::RESUME) {
        r.tick(0);
        assert!(r.ticks < 400, "ホストのポーズが観戦者に来ない");
    }
    let played = r.wp.stats().played;
    let (st, fr) = (r.g.m.state, r.g.m.frames);
    assert_eq!(r.tick(ESC), Action::RenderOnly);
    assert_eq!(r.wp.notice(), Some(WatchNotice::Paused));
    for _ in 0..30 {
        assert_eq!(r.tick(0), Action::RenderOnly);
    }
    assert_eq!(r.wp.stats().played, played, "止めた間に記録を使った");
    assert_eq!(
        (r.g.m.state, r.g.m.frames),
        (st, fr),
        "止めた間にポーズのメニューが進んだ"
    );
    assert_ne!(r.tick(ESC), Action::RenderOnly, "2 回目の Esc で戻らない");
    assert_eq!(r.wp.notice(), None);
    r.run(100);
    r.drain();
    r.assert_same_as_host("ホストのポーズの再現中");
    let p = r.wp.stats().pause;
    assert_eq!((p.opened, p.overlays, p.overlay_ticks), (0, 1, 31), "{p:?}");
    assert_eq!(r.wp.end(), None);
}

/// 終わりのメニューが出ている・前の語に Esc が入っている（押した瞬間を作れない）・ポーズを読めないときも、ゲームのポーズを開かない。
#[test]
fn the_overlay_is_used_when_the_games_pause_cannot_be_opened() {
    for case in ["終わりのメニュー", "前の語に Esc", "読めない"] {
        let mut r = Rig::new(HostPlan::none());
        r.run(60);
        match case {
            "終わりのメニュー" => r.g.m.end_menu = true,
            "前の語に Esc" => r.wp.last_words = (ESC, 0),
            _ => r.g.readable = false,
        }
        let played = r.wp.stats().played;
        assert_eq!(r.tick(ESC), Action::RenderOnly, "{case}");
        assert!(!r.g.m.paused, "{case}: ゲームのポーズを開いた");
        assert_eq!(r.wp.stats().played, played, "{case}");
        assert_eq!(r.tick(0), Action::RenderOnly, "{case}");
        assert_ne!(r.tick(ESC), Action::RenderOnly, "{case}: 戻らない");
        assert_eq!(r.wp.stats().pause.overlays, 1, "{case}");
    }
}

/// ポーズの「タイトルへ戻る」→「はい」（状態 8）で BYE を頼み、以後の手元の Esc・Q は `[2]` に入れない（観戦を終えた後も、場面がマッチを出るまで）。
/// 状態 8 は 20 tick で閉じて 1 フレーム進み、場面が移る。移った後は `Local`。
#[test]
fn title_from_the_pause_ends_the_watch_and_later_escs_never_reopen_the_match() {
    let mut r = Rig::new(HostPlan::none());
    r.run(90);
    r.tap(ESC);
    r.run(6);
    r.tap(DOWN);
    r.run(4);
    r.tap(Z);
    assert_eq!(r.g.m.state, pause::NO);
    r.run(4);
    r.tap(UP);
    r.tap(Z);
    assert_eq!(r.g.m.state, pause::LEAVING, "「はい」に入っていない");
    assert_eq!(r.wp.end(), Some(WatchEnd::Quit));
    assert_eq!(
        r.wp.feed().end_request(),
        Some(Some(ByeReason::Cancelled)),
        "BYE を頼んでいない"
    );
    let applied = r.g.m.applied.len();
    let locals = leave_with_noisy_keys(&mut r);
    assert_eq!(
        r.g.m.applied.len(),
        applied + 1,
        "状態 8 が閉じて 1 フレームだけ進む"
    );
    assert!(locals > 10, "マッチを出た後も Local でない");
    assert_eq!(r.wp.stats().pause.quits, 1);
    let st = WatchStatus::new();
    st.publish(&r.wp);
    assert_eq!(st.get(S_END_DETAIL), QUIT_BY_PAUSE);
}

/// 観戦を終えた後に Esc・Q・Z を叩き続けても、場面がマッチを出るまで手元の語は `[2]` に入らず（入るのはこちらが作る 0・Q・Esc だけ）、
/// `[0]`・`[1]` も feed しない。マッチを出た後の `Local` の tick の数を返す。
fn leave_with_noisy_keys(r: &mut Rig) -> u32 {
    let mut locals = 0;
    for i in 0..120 {
        let k = if i % 2 == 0 { ESC | Q | Z } else { 0 };
        let in_match = r.g.m.cur == SCENE_MATCH;
        let a = r.tick(k);
        if in_match {
            assert_eq!(a, Action::Zero, "マッチの中の tick {i}");
            let f = r.fed();
            assert_eq!(
                (f[0], f[1]),
                (None, None),
                "終えた後に [0]・[1] を feed した"
            );
            assert!(
                matches!(f[2], Some(0) | Some(Q) | Some(ESC)),
                "終えた後に手元の語が [2] に入った: {:?}",
                f[2]
            );
        }
        if a == Action::Local {
            locals += 1;
        }
    }
    assert_eq!(r.g.m.cur, SCENE_MENU, "場面がマッチを出ていない");
    locals
}

/// 閉じる tick に使う `Frame` でホストがポーズを開いた（`[2]` に Esc の押した瞬間）: 閉じた直後に開き直し、試合は進まない（ホストも進んでいない）。
#[test]
fn a_host_pause_on_the_closing_frame_reopens_right_after() {
    let k = 200u32;
    let mut r = Rig::new(HostPlan {
        esc_at: vec![k],
        z_at: vec![k + 10],
    });
    while r.wp.stats().played < u64::from(k) {
        r.tick(0);
    }
    assert_eq!(r.wp.expect_frame, k);
    r.tap(ESC);
    r.run(6);
    r.tap(Z);
    for _ in 0..30 {
        r.tick(0);
    }
    let p = r.wp.stats().pause;
    assert_eq!((p.resumed, p.reopened, p.close_slips), (1, 1, 0), "{p:?}");
    r.run(100);
    r.drain();
    r.assert_same_as_host("閉じる tick にホストのポーズ");
}

/// 崩れ（Esc の tick に開かなかった・閉じる tick に閉じなかった）を 1 回見たら、その seg の残りは重ね描きの一時停止だけを使う。次の seg で戻る。
/// 数えは本ログの行に出る（観戦は続ける）。
#[test]
fn a_slip_keeps_the_rest_of_the_seg_on_the_overlay() {
    let mut r = Rig::new(HostPlan::none());
    r.run(60);
    r.g.m.refuse_open = true;
    r.tap(ESC);
    assert_eq!(r.wp.stats().pause.open_slips, 1);
    assert!(!r.g.m.paused);
    r.g.m.refuse_open = false;
    r.run(10);
    assert_eq!(
        r.tick(ESC),
        Action::RenderOnly,
        "崩れた seg でゲームのポーズを開いた"
    );
    assert!(!r.g.m.paused);
    assert_eq!(r.wp.notice(), Some(WatchNotice::Paused));
    r.tick(0);
    assert_ne!(r.tick(ESC), Action::RenderOnly);
    // 次の seg では、またゲームのポーズを開く。閉じる tick が外れたら、また重ね描きだけになる。
    r.wp.begin_seg_for_test(1);
    r.run(10);
    r.g.m.close_at = CLOSE_FRAMES + 3;
    r.tap(ESC);
    assert!(r.g.m.paused, "次の seg でゲームのポーズを開かない");
    r.run(6);
    r.tap(Z);
    r.run(40);
    let p = r.wp.stats().pause;
    assert!(p.close_slips >= 1, "{p:?}");
    r.g.m.close_at = CLOSE_FRAMES;
    r.run(10);
    assert_eq!(
        r.tick(ESC),
        Action::RenderOnly,
        "閉じる tick が外れた seg でゲームのポーズを開いた"
    );
    let p = r.wp.stats().pause;
    assert_eq!((p.opened, p.fallbacks), (2, 2), "{p:?}");
    assert_eq!(r.wp.end(), None);
    let st = WatchStatus::new();
    st.publish(&r.wp);
    let t = st.text();
    assert!(t.contains("Esc の tick に開かなかった 1"), "{t}");
    assert!(t.contains("異常の後にオーバーレイにした 2"), "{t}");
}

/// 長いポーズ（30 秒・10 分）: ホストは最後の 25 秒だけ黙る。溜めた記録がある間は観戦を終えず、明けた後の列がホストと一致する（10 分は約 3.4 万フレーム溜まる）。
/// 重ね描きの一時停止も同じ。
#[test]
fn a_long_pause_outlasts_a_silent_host_and_the_rest_matches_afterwards() {
    for (ticks, overlay) in [(1_800u32, false), (36_000, false), (1_800, true)] {
        let tag = format!("{ticks} tick・重ね描き {overlay}");
        let mut r = Rig::new(HostPlan::none());
        if overlay {
            r.g.readable = false;
        }
        r.run(90);
        r.tap(ESC);
        let mut silent_from = None;
        for i in 0..ticks {
            if i == ticks - 1_500 {
                // ホストが黙る（以後は何も届かない）。
                r.streaming = false;
                silent_from = Some(r.now_ms());
            }
            r.tick(0);
            assert_eq!(r.wp.end(), None, "{tag}: ポーズの間に終えた（{i} tick）");
        }
        let silent = r.now_ms() - silent_from.unwrap();
        assert!(silent > HOST_SILENT_END_MS, "{tag}: 沈黙 {silent} ms");
        if overlay {
            r.tap(ESC);
        } else {
            r.run(6);
            r.tap(Z);
        }
        for _ in 0..60_000 {
            if r.wp.stats().played == u64::from(r.n) {
                break;
            }
            r.tick(0);
        }
        r.assert_same_as_host(&tag);
        // 使い切った後は、黙ったホストで終える。
        r.run(10);
        assert_eq!(r.wp.end(), Some(WatchEnd::HostSilent), "{tag}");
    }
}

/// マッチの外の Esc: 2.9 秒で離すと終えない（押している間と離して 3 秒は案内を出す）。3 秒押し続けると終える。
#[test]
fn holding_esc_for_three_seconds_outside_the_match_stops_watching() {
    let mut r = Rig::new(HostPlan::none());
    r.wp.set_phase_for_test(Phase::Loading);
    r.streaming = false;
    r.tick(0);
    assert_eq!(r.wp.notice(), None);
    for _ in 0..SHORT_HOLD_TICKS {
        assert_eq!(r.tick(ESC), Action::Zero);
        assert_eq!(r.wp.notice(), Some(WatchNotice::EscHold));
    }
    assert_eq!(r.wp.end(), None, "2.9 秒で終えた");
    r.tick(0);
    let released_at = r.now_ms();
    assert_eq!(
        r.wp.notice(),
        Some(WatchNotice::EscHold),
        "離した tick に消えた"
    );
    loop {
        r.tick(0);
        let t = r.now_ms();
        if t - released_at < ESC_HINT_KEEP_MS {
            assert_eq!(r.wp.notice(), Some(WatchNotice::EscHold), "{t}");
        } else {
            assert_eq!(r.wp.notice(), None, "離して 3 秒たっても残った");
            break;
        }
    }
    assert_eq!(r.hold_until_end(400), Some(HOLD_END_TICK), "3 秒で終えない");
    assert_eq!(r.wp.end(), Some(WatchEnd::Quit));
    assert_eq!(r.wp.notice(), None);
    assert_eq!(r.wp.stats().pause.esc_outside, 2);
    assert_eq!(r.wp.feed().end_request(), Some(Some(ByeReason::Cancelled)));
    assert_eq!(
        r.tick(ESC),
        Action::Local,
        "マッチの外は終えた後は自分の入力"
    );
    let st = WatchStatus::new();
    st.publish(&r.wp);
    assert_eq!(st.get(S_END_DETAIL), 0, "長押しの詳細");
}

/// マッチの中の長押し: 最初の押下でゲームのポーズが開き、押したままでは再開しない。2.9 秒で離すと続き、3 秒で終える。終えた後は Q で
/// 「タイトルへ戻る」へ送り、場面がマッチを出るまで手元の Esc・Q で試合を進めない。
#[test]
fn holding_esc_in_the_match_opens_the_pause_then_stops_watching_and_leaves_to_the_title() {
    let mut r = Rig::new(HostPlan::none());
    r.run(90);
    for i in 0..SHORT_HOLD_TICKS {
        r.tick(ESC);
        assert!(
            r.g.m.paused,
            "{i}: 押したままでポーズが開いていない・閉じた"
        );
    }
    assert_eq!(r.wp.end(), None, "2.9 秒で終えた");
    assert_eq!(r.g.m.state, pause::RESUME, "押したままで再開へ進んだ");
    r.run(2);
    r.tap(Z);
    r.run(40);
    assert!(!r.g.m.paused);
    let applied = r.g.m.applied.len();
    assert_eq!(r.hold_until_end(400), Some(HOLD_END_TICK));
    assert_eq!(r.wp.end(), Some(WatchEnd::Quit));
    assert!(r.g.m.paused, "最初の押下でポーズが開いていない");
    let locals = leave_with_noisy_keys(&mut r);
    assert!(locals > 10);
    assert_eq!(
        r.g.m.applied.len(),
        applied + 1,
        "終えた後に試合が進んだ（状態 8 が閉じる 1 フレームのほか）"
    );
    let st = WatchStatus::new();
    st.publish(&r.wp);
    assert_eq!(st.get(S_END_DETAIL), 0, "長押しの詳細");
}

/// 開いているポーズの中で押し始めた長押し: 押し始めの Esc はポーズのメニューが再開に使い（状態 3）、試合が続く。3 秒で終えた時には
/// ポーズは閉じているので、Esc で開き直してから「タイトルへ戻る」へ送る。
#[test]
fn a_hold_begun_inside_the_pause_resumes_then_stops_watching_and_leaves_to_the_title() {
    let mut r = Rig::new(HostPlan::none());
    r.run(90);
    r.tap(ESC);
    r.run(6);
    assert_eq!(r.g.m.state, pause::RESUME);
    r.tick(ESC);
    assert_eq!(r.g.m.state, pause::RESUMING, "押し始めの Esc で再開しない");
    let before = r.g.m.applied.len();
    assert_eq!(r.hold_until_end(400), Some(HOLD_END_TICK - 1));
    let applied = r.g.m.applied.len();
    assert!(applied > before + 100, "長押しの間に試合が続いていない");
    leave_with_noisy_keys(&mut r);
    assert!(
        r.g.m.applied.len() <= applied + 3,
        "終えた後に試合が進んだ: {} → {}",
        applied,
        r.g.m.applied.len()
    );
}

/// 重ね描きの一時停止の間の長押し（ホストのポーズの再現中）: 最初の押下で重ね描き、3 秒で終える。終えた後は開いているホストのポーズから
/// 「タイトルへ戻る」へ送る。
#[test]
fn holding_esc_on_the_overlay_stops_watching_and_leaves_to_the_title() {
    let mut r = Rig::new(HostPlan {
        esc_at: vec![150],
        z_at: vec![1_000],
    });
    while !(r.g.m.paused && r.g.m.state == pause::RESUME) {
        r.tick(0);
    }
    for _ in 0..SHORT_HOLD_TICKS {
        assert_eq!(r.tick(ESC), Action::RenderOnly);
    }
    assert_eq!(r.wp.end(), None, "2.9 秒で終えた");
    r.tick(0);
    assert_ne!(r.tick(ESC), Action::RenderOnly, "重ね描きが戻らない");
    r.tick(0);
    assert_eq!(r.hold_until_end(400), Some(HOLD_END_TICK));
    assert_eq!(r.wp.stats().pause.overlays, 2);
    leave_with_noisy_keys(&mut r);
}

/// ポーズの間に観戦が終わった（受信スレッドの観戦者の処理が止まった）: 本物のキーボードで試合を進めず、「タイトルへ戻る」へ送る。
#[test]
fn an_end_during_the_pause_leaves_to_the_title_without_the_keyboard() {
    let mut r = Rig::new(HostPlan::none());
    r.run(90);
    r.tap(ESC);
    r.run(10);
    r.wp.feed.mark_broken();
    let applied = r.g.m.applied.len();
    leave_with_noisy_keys(&mut r);
    assert_eq!(r.wp.end(), Some(WatchEnd::Broken));
    assert_eq!(r.g.m.applied.len(), applied + 1);
}

/// 早送りの余分な更新の中で Esc を押し始めても、その更新ではポーズを開かず、次の表示の最初の更新で開く。
#[test]
fn an_esc_pressed_during_fast_forward_opens_the_pause_on_the_next_display() {
    let mut r = Rig::new(HostPlan::none());
    for _ in 0..600 {
        r.host_tick();
    }
    let mut extras = 0;
    for _ in 0..20 {
        extras += r.display(0, 0).0;
    }
    assert!(extras > 0, "早送りしていない");
    let (n, opened) = r.display(0, ESC);
    assert!(n > 0, "余分な更新が無い表示で押した");
    assert!(!opened, "余分な更新の中でポーズを開いた");
    assert_eq!(r.wp.pause, LocalPause::Off);
    let (n, opened) = r.display(ESC, ESC);
    assert!(!opened);
    assert_eq!(n, 0, "Esc の tick の後に余分な更新を回した");
    assert_ne!(r.wp.pause, LocalPause::Off, "次の表示でポーズを開かない");
    assert!(r.g.m.paused);
    r.run(6);
    r.tap(Z);
    r.run(40);
    for _ in 0..2_000 {
        if r.wp.stats().played == u64::from(r.n) {
            break;
        }
        r.display(0, 0);
    }
    r.streaming = false;
    r.drain();
    r.assert_same_as_host("早送りの途中のポーズ");
}

/// 照合: ずれたら数えて最初のずれを持ち、観戦は続ける。本ログに 1 行出る。
#[test]
fn a_mismatching_check_is_counted_and_the_watch_goes_on() {
    let mut r = Rig::new(HostPlan::none());
    r.run(130);
    assert!(r.wp.stats().checks_matched >= 2);
    r.g.m.rng ^= 1;
    r.run(130);
    let s = r.wp.stats();
    assert!(s.checks_mismatched >= 1, "{s:?}");
    let (kind, host, own) = s.first_mismatch.expect("最初のずれ");
    assert_eq!(kind, CheckKind::Game);
    assert_ne!(
        (host.rng_state, host.rng_count),
        (own.rng_state, own.rng_count)
    );
    assert_eq!(r.wp.end(), None, "ずれで観戦を終えた");
    let st = WatchStatus::new();
    st.publish(&r.wp);
    let t = st.text();
    assert!(t.contains("観戦者の照合: 一致"), "{t}");
    assert!(t.contains(&format!("frame {}", host.frame)), "{t}");
}

/// 照合のずれは seg の最初の 1 回だけ、ずれた Check の記録の位置と、その seg のポーズの崩れを受信スレッドへ渡す。
#[test]
fn the_first_mismatch_of_a_seg_hands_its_check_records_position_to_the_receive_thread() {
    let mut r = Rig::new(HostPlan::none());
    r.run(130);
    assert_eq!(r.wp.feed.take_mismatch(), None, "一致の間に渡した");
    r.g.m.rng ^= 1;
    let before = crate::test_alloc::count();
    r.run(130);
    assert_eq!(
        crate::test_alloc::count() - before,
        0,
        "ずれを渡すときに確保した"
    );
    let s = r.wp.stats();
    assert!(s.checks_mismatched >= 2, "{s:?}");
    let frame = s.first_mismatch.expect("最初のずれ").1.frame;
    let pos = r
        .check_at
        .iter()
        .find(|c| c.0 == frame)
        .map(|c| c.1)
        .expect("ずれた frame の記録");
    assert_eq!(r.wp.feed.take_mismatch(), Some((pos, false)));
    r.run(130);
    assert!(r.wp.stats().checks_mismatched > s.checks_mismatched);
    assert_eq!(
        r.wp.feed.take_mismatch(),
        None,
        "同じ seg の 2 回目のずれを渡した"
    );
    // 次の seg では、また最初のずれを渡す。
    r.wp.begin_seg_for_test(1);
    let n = r.check_at.len();
    r.run(130);
    let next = r.check_at.get(n).expect("次の seg の記録").1;
    assert_eq!(r.wp.feed.take_mismatch(), Some((next, false)));
}

#[test]
fn a_mismatch_in_a_seg_whose_pause_slipped_is_handed_over_as_broken() {
    let mut r = Rig::new(HostPlan::none());
    r.run(60);
    r.g.m.refuse_open = true;
    r.tap(ESC);
    assert_eq!(r.wp.stats().pause.open_slips, 1);
    r.g.m.refuse_open = false;
    r.g.m.rng ^= 1;
    r.run(130);
    assert!(r.wp.stats().checks_mismatched >= 1);
    assert!(matches!(r.wp.feed.take_mismatch(), Some((_, true))));
}

/// 照合の記録は時間を使わない: 積んでも積まなくても、使ったフレーム・溜まり・描画だけのフレームの数が変わらない。
#[test]
fn checks_do_not_count_as_frames_or_backlog() {
    let run = |checks: bool| {
        let mut r = Rig::new(HostPlan::none());
        r.checks = checks;
        let mut backlog = Vec::new();
        for _ in 0..300 {
            r.tick(0);
            backlog.push(r.wp.available());
        }
        r.drain();
        (r.wp.stats(), r.g.m.applied.clone(), backlog)
    };
    let (with, wa, wb) = run(true);
    let (without, oa, ob) = run(false);
    assert!(with.checks_matched >= 4, "{with:?}");
    assert_eq!(without.checks_matched, 0);
    assert_eq!(wa, oa, "試合の列");
    assert_eq!(wb, ob, "tick ごとの溜まり");
    assert_eq!(
        (
            with.played,
            with.backlog_max,
            with.stalls,
            with.start_hold_ticks,
            with.stray_records
        ),
        (
            without.played,
            without.backlog_max,
            without.stalls,
            without.start_hold_ticks,
            without.stray_records
        )
    );
}

/// ゲームのスレッドの規則: ポーズの開閉・重ね描きの一時停止・照合・長押しで終えた後の tick に確保しない。
#[test]
fn the_pause_and_the_checks_never_allocate() {
    let mut r = Rig::new(HostPlan {
        esc_at: vec![400],
        z_at: vec![420],
    });
    r.run(100);
    let before = crate::test_alloc::count();
    r.tap(ESC);
    r.run(6);
    r.tap(Z);
    r.run(300);
    r.tap(ESC);
    r.run(10);
    r.tap(ESC);
    r.run(200);
    assert!(r.wp.stats().checks_matched >= 4);
    r.hold_until_end(400);
    r.run(60);
    assert_eq!(crate::test_alloc::count() - before, 0, "確保した");
}
