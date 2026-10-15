//! 対戦が終わった・切れた後に、Match の画面（6）でカーソルを「妖怪 対 妖怪」に合わせるまでの道筋（pure）。ゲームのメモリには
//! 書かず、入力の語だけを作る。観戦者の経路も同じ部品で戻せる形にしてある（場面の判断は呼び手が渡す）。
//!
//! 流れ（`Flow::step` を入力の地点で 1 tick に 1 回）:
//! 1. 入力止め（`HOLD_TICKS`）: 押しっぱなしのキーで何かを選ばないよう、人の入力を捨てる。試合の場面は描画だけのフレームで止める。
//! 2. 試合の場面: 終わりのメニューは「リプレイを保存して終了」だけ、一時停止は「タイトルへ戻る」と確かめの枠だけを選べるようにする。
//!    人の語からは、その状態のメニューが読むキーの押した瞬間だけを通す（Esc = 再開・Q = 確かめなしでタイトルへ、は通さない）。
//!    カーソルが選べない項目にあれば自分で動かす。どちらのメニューも出ていなければ Esc を差し込んで一時停止を開く。
//! 3. メニューの場面へ移ったら MainMenu が作り直されるのを待ち（`SETTLE_TICKS`）、画面を毎 tick 読んで、効く tick に 1 つずつ押す:
//!    16・8・7 は取消、タイトルは Match へ、6 はカーソルを 4 へ（決定は押さない —— 押すと入口のメニューが開く）。14・15 は人に任せる。
//!
//! 読んだ値が想定の外・押しても状態が動かないときは、その場でやめる（`Finish::GaveUp`。以後はゲーム自身の入力で続く）。
#![forbid(unsafe_code)]

use crate::input_model::InputState;
use crate::input_model::keys::{DOWN, ESC, UP, X, Z};
use crate::menu_model::{DECIDE_BITS, SCREEN_CHARA, SCREEN_DIFFICULTY, SCREEN_MAP};
use crate::save_skip::{SCREEN_AFTER_MATCH, SCREEN_SAVE, SCREEN_TITLE};
use crate::ui_entry::{EntryView, MODE_ITEMS, SCREEN_MATCH_MODE, YOKAI_CURSOR, leaks};

/// 戻り始めてから人の入力を受け付けない tick（1.5 秒。60 fps）。
pub const HOLD_TICKS: u32 = 90;
/// メニューの場面へ移ってから画面を読み始めるまでの tick。場面 1 に入った最初の tick の MainMenu のポインタは前の（解放された）
/// MainMenu を指しうる（`save_skip::Watch::step`）ほか、試合から戻る MainMenu はタイトル → 6 → 7 → 8 を押下なしで 1〜2 フレームで
/// 進む（`0x00429FE1` の `[M+0x1B23C]`）ので、その間は押さない。
pub const SETTLE_TICKS: u32 = 10;
/// 一時停止を開こうとして開かないまま続けてよい tick（5 秒）。
pub const OPEN_TICKS: u32 = 300;
/// 一時停止・終わりのメニューで、こちらが押す状態のまま動かない tick の上限（2 秒）。
pub const STUCK_TICKS: u32 = 120;
/// メニューの場面で同じ画面・同じカーソルのまま押し続けてよい tick、メニューの場面を待つ tick の上限（10 秒）。
pub const MENU_STUCK_TICKS: u32 = 600;

/// 一時停止のメニュー（`0x00434740`、状態は this の `+0x00`、フレームの数え `+0x04`。振り分けの表 `0x00434E08`、10 件）。
pub mod pause {
    /// 項目を作って 1 へ（`0x00434878 incl (%esi)`）。
    pub const INIT: u32 = 0;
    /// カーソルが「再開」（`0x00434881`）。決定で `RESUMING`（`0x0043497F`）、上下で `TITLE`（`0x00434911` / `0x00434936`）。
    pub const RESUME: u32 = 1;
    /// カーソルが「タイトルへ戻る」（`0x0043499A`）。決定で `NO`（`0x00434ABD`）、上下で `RESUME`。
    pub const TITLE: u32 = 2;
    /// 再開の確定。20 フレーム後に一時停止を閉じる（`0x00434AD8`）。
    pub const RESUMING: u32 = 3;
    /// 確かめの枠の「はい」（`0x00434B54`）。決定で `LEAVING`（`0x00434C4E`）、上下で `NO`。
    pub const YES: u32 = 4;
    /// 確かめの枠の「いいえ」（最初のカーソル。`0x00434C58`）。決定で `TITLE` へ戻る（`0x00434D62`）、上下で `YES`。
    pub const NO: u32 = 5;
    /// タイトルへ。20 フレーム後に次の要求 1 を書く（`0x00434D84`）。
    pub const LEAVING: u32 = 8;
    /// 状態の数（`0x0043480B cmp $0x9` → `ja`）。
    pub const STATES: u32 = 10;
    /// どの状態でも先頭で読む近道: Esc は `RESUMING` へ（`0x0043474B push $0x8`）、Q は `LEAVING` へ（`0x004347AD push $0x200`）。
    pub const KEY_TO_TITLE: u16 = 0x0200;
}

/// マッチの終わりのメニュー（`0x004355F0`、状態 `+0x00`・フレームの数え `+0x04`。振り分けの表 `0x00435A68`、7 件）。上下は循環。
pub mod end_menu {
    pub const INIT: u32 = 0;
    /// カーソルが「再戦」（`0x00435670`）。上で `SAVE`（`0x00435715`）、下で `CHARA`（`0x0043573A`）。
    pub const REMATCH: u32 = 1;
    /// カーソルが「キャラを変更」（`0x004357B9`）。上で `REMATCH`、下で `SAVE`（`0x00435883`）。
    pub const CHARA: u32 = 2;
    /// カーソルが「リプレイを保存して終了」（`0x004358D8`）。決定で 6 → 20 フレーム後に次の要求 6（`0x00435A40`）。
    pub const SAVE: u32 = 3;
    /// 状態の数（`0x0043561F cmp $0x6` → `ja`。4・5・6 は決定の後の 20 フレーム）。
    pub const STATES: u32 = 7;
}

/// 一時停止・終わりのメニューのカーソルの状態が、入力を読み始めるフレーム（`0x004348E3` / `0x004356EE cmpl $0x4,0x4(%esi)`）。
pub const MENU_READ_FRAMES: u32 = 4;
/// タイトルの項目の数（`0x0042A0C4 push $8`）と Match の項目（決定の表 `0x0042A43B` の 3 件目が `0x0042A23C` の set_screen(6)）。
/// Extra が未解放だと項目 1 を飛ばす（`0x0042A0D1`）が、カーソルを毎 tick 読んで押すので数を決め打たない。
pub const TITLE_ITEMS: i32 = 8;
pub const TITLE_MATCH_ITEM: i32 = 2;

/// 戻り始めた理由（左上の 1 行の選び）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// 相手が抜けた（相手の BYE）。
    PeerLeft,
    /// 相手の応答が途絶えた・手順が食い違って閉じた。
    Lost,
    /// 自分が相手を待つ間に切った。
    SelfCut,
}

/// メニュー（一時停止・終わりのメニュー）の状態とフレームの数え。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuState {
    pub state: u32,
    pub frames: u32,
}

/// 試合の場面で読むもの。メニューの状態は読めなければ `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayView {
    /// 読み込み中（`0x4A7EB4`）。
    pub loading: bool,
    /// 一時停止のバイト `0x4A7ECC` の下位 1 バイトが 0 でない。
    pub paused: bool,
    /// 終わりのメニューの印 `0x4A7ECE` が 0 でない。
    pub end_menu: bool,
    pub pause: Option<MenuState>,
    pub end: Option<MenuState>,
}

/// メニューの場面で読む MainMenu の欄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub id: u32,
    /// `[M+0x28]`（1 のときだけ押下を読む）。
    pub sub: u32,
    /// `[M+0x1B238]`（0 でなければハンドラが走らない）。
    pub trans: u32,
    pub cursor: i32,
    /// キャラ選択の席の旗（押す数は旗で変わるが、毎 tick 読み直すので押し方は変えない。止まったかの目印に入れる）。
    pub decided: [u32; 2],
}

/// 1 tick の材料。場面の番号は呼び手が持つ（ここは「どの段にいるか」だけを見る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// 今の場面がマッチで、次の要求もマッチ（切替の途中でない）。
    pub in_match: bool,
    /// 今の場面がメニューで、次の要求もメニュー。
    pub in_menu: bool,
    /// 今がマッチで、次の要求が「リプレイを保存して終了」のもの（終わりのメニューが置く）。
    pub save_quit: bool,
    pub play: Option<PlayView>,
    pub menu: Option<Screen>,
}

/// 1 tick に必要になったときだけ読むもの（人の語は台本・機器を 1 tick に 1 回しか読めないので遅らせる）。
pub trait Reads {
    /// 人の語（InputManager[2] に入る合成の語）。1 tick に 1 回まで。
    fn raw(&mut self) -> u16;
    /// InputManager `index` の今の状態。
    fn im(&mut self, index: usize) -> InputState;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    /// 描画だけのフレーム（ゲームの更新を走らせない）。
    Freeze,
    /// この 3 本を feed する（index 0・1・2）。
    Feed([u16; 3]),
    /// 手を出さない（ゲーム自身の入力で進む）。
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// 試合の場面のメニューの状態・MainMenu が読めない、カーソルが範囲の外。
    Unreadable,
    PauseState(u32),
    EndState(u32),
    /// 押しても状態が動かない。
    Stuck,
    /// 一時停止が開かない。
    NoPause,
    /// メニューの場面に着かない。
    NoMenu,
    /// 戻り道に無い画面（Story のメニューなど）。
    Elsewhere(u32),
}

/// 理由の通しの印の、種類を置く桁（上位 8 bit）。下位 24 bit は状態か画面の番号。
const WHY_SHIFT: u32 = 24;
const WHY_ARG_MASK: u32 = (1 << WHY_SHIFT) - 1;

impl GiveUp {
    /// 原子変数に載せる形（0 は「まだ無い」）。ゲームのスレッドから確保なしで残し、本ログの側が `describe` で文にする。
    pub fn encode(self) -> u32 {
        let (kind, arg) = match self {
            GiveUp::Unreadable => (1, 0),
            GiveUp::PauseState(s) => (2, s),
            GiveUp::EndState(s) => (3, s),
            GiveUp::Stuck => (4, 0),
            GiveUp::NoPause => (5, 0),
            GiveUp::NoMenu => (6, 0),
            GiveUp::Elsewhere(id) => (7, id),
        };
        (kind << WHY_SHIFT) | (arg & WHY_ARG_MASK)
    }

    /// `encode` の値を本ログの文にする。知らない印は印そのものを書く。
    pub fn describe(code: u32) -> String {
        let arg = code & WHY_ARG_MASK;
        match code >> WHY_SHIFT {
            0 => "なし".to_string(),
            1 => "試合の場面のメニューの状態か MainMenu が読めない、カーソルが範囲の外".to_string(),
            2 => format!("一時停止のメニューの状態 {arg} が想定の外"),
            3 => format!("終わりのメニューの状態 {arg} が想定の外"),
            4 => "押しても状態が動かない".to_string(),
            5 => "一時停止が開かない".to_string(),
            6 => "メニューの場面に着かない".to_string(),
            7 => format!("戻り道に無い画面 {arg}"),
            _ => format!("不明な理由の印 0x{code:08X}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// 画面 6 でカーソルが 4 に着いた。
    Arrived,
    GaveUp(GiveUp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub out: Out,
    /// この tick に「リプレイを保存して終了」で試合の場面を出るのを見た（1 回だけ）。呼び手は保存の画面を飛ばす印を立てる。
    pub save_quit: bool,
    /// 終わった（以後は呼ばない）。
    pub finish: Option<Finish>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Hold,
    Play,
    Settle,
    Menu,
    Done,
}

const ZERO: [u16; 3] = [0; 3];
/// 押し続けを数えない状態（人が選ぶ間）の目印。
const MARK_NONE: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow {
    stage: Stage,
    /// 今の段にいる tick（入力止め・落ち着くのを待つ間）。
    ticks: u32,
    /// 同じ目印のまま続いた tick と、その目印。
    stuck: u32,
    mark: u64,
    prev_raw: u16,
    save_quit_seen: bool,
}

impl Default for Flow {
    fn default() -> Self {
        Flow::new()
    }
}

/// 試合の場面のメニューは `[2]` だけを読むので、選手の席には何も入れない。
fn only2(w: u16) -> Out {
    Out::Feed([0, 0, w])
}

/// 選択の画面のキャラ選択は 1P の取消を `[0]` から読み、他の画面は `[2]` から読む。
fn both(w: u16) -> Out {
    Out::Feed([w, 0, w])
}

impl Flow {
    pub fn new() -> Flow {
        Flow {
            stage: Stage::Hold,
            ticks: 0,
            stuck: 0,
            mark: MARK_NONE,
            prev_raw: 0,
            save_quit_seen: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    pub fn step(&mut self, f: &Facts, r: &mut dyn Reads) -> Step {
        match self.stage {
            Stage::Hold => self.hold(f, r),
            Stage::Play => self.play(f, r),
            Stage::Settle => self.settle(f),
            Stage::Menu => self.menu(f, r),
            Stage::Done => Self::out(Out::Pass),
        }
    }

    fn out(out: Out) -> Step {
        Step {
            out,
            save_quit: false,
            finish: None,
        }
    }

    fn enter(&mut self, s: Stage) {
        self.stage = s;
        self.ticks = 0;
        self.unmark();
    }

    fn unmark(&mut self) {
        self.mark = MARK_NONE;
        self.stuck = 0;
    }

    /// 目印 `mark` のまま `limit` tick を超えて続いたら真。
    fn stuck_on(&mut self, mark: u64, limit: u32) -> bool {
        if mark != self.mark {
            self.mark = mark;
            self.stuck = 0;
        } else {
            self.stuck = self.stuck.saturating_add(1);
        }
        self.stuck > limit
    }

    fn give_up(&mut self, why: GiveUp) -> Step {
        self.stage = Stage::Done;
        Step {
            out: Out::Pass,
            save_quit: false,
            finish: Some(Finish::GaveUp(why)),
        }
    }

    /// 押した瞬間を作れる tick だけ `want` を返す（前の tick の語に立っていれば離す）。
    fn press(want: u16, r: &mut dyn Reads, ims: &[usize]) -> u16 {
        if ims.iter().any(|&i| r.im(i).cur & want != 0) {
            0
        } else {
            want
        }
    }

    /// 人の語の押した瞬間（前の tick の人の語に無かったビット）。押しっぱなしは押下にしない。
    fn fresh(&mut self, r: &mut dyn Reads) -> u16 {
        let raw = r.raw();
        let e = raw & !self.prev_raw;
        self.prev_raw = raw;
        e
    }

    fn hold(&mut self, f: &Facts, r: &mut dyn Reads) -> Step {
        let _ = self.fresh(r);
        self.ticks = self.ticks.saturating_add(1);
        let freeze = f.in_match && f.play.is_some_and(|p| !p.loading);
        if self.ticks >= HOLD_TICKS {
            self.enter(if f.in_match {
                Stage::Play
            } else {
                Stage::Settle
            });
        }
        Self::out(if freeze { Out::Freeze } else { Out::Feed(ZERO) })
    }

    fn play(&mut self, f: &Facts, r: &mut dyn Reads) -> Step {
        if !f.in_match {
            let mut s = Self::out(Out::Feed(ZERO));
            if f.save_quit && !self.save_quit_seen {
                self.save_quit_seen = true;
                s.save_quit = true;
            }
            if f.in_menu {
                self.enter(Stage::Settle);
            }
            return s;
        }
        let fresh = self.fresh(r);
        let Some(p) = f.play else {
            return self.give_up(GiveUp::Unreadable);
        };
        if p.loading {
            self.unmark();
            return Self::out(Out::Feed(ZERO));
        }
        if p.end_menu {
            return self.end_lock(p.end, fresh, r);
        }
        if p.paused {
            return self.pause_lock(p.pause, fresh, r);
        }
        // 一時停止が開かない間（試合の頭と終わりの会話の間は開かないかもしれない —— 確かめていない）は、人の決定の押した瞬間だけを
        // 通して会話を進められるようにし、押している間は諦めない。
        let decide = fresh & DECIDE_BITS;
        if decide != 0 {
            self.unmark();
        } else if self.stuck_on(1 << 40, OPEN_TICKS) {
            return self.give_up(GiveUp::NoPause);
        }
        Self::out(only2(Self::press(ESC, r, &[2]) | decide))
    }

    fn pause_lock(&mut self, m: Option<MenuState>, fresh: u16, r: &mut dyn Reads) -> Step {
        use pause::*;
        let Some(m) = m else {
            return self.give_up(GiveUp::Unreadable);
        };
        let word = match m.state {
            TITLE => {
                self.unmark();
                fresh & DECIDE_BITS
            }
            YES | NO => {
                self.unmark();
                // 決定は上下の後の状態を読む（`0x00434C3D mov (%esi),%ecx`）。「はい」で上下と決定が同じ tick に来ると、
                // 「いいえ」へ動いた後の決定がやり直し（状態 9 → 次の要求 10・11）になるので、決定の tick は上下を落とす。
                if fresh & DECIDE_BITS != 0 {
                    fresh & DECIDE_BITS
                } else {
                    fresh & (UP | DOWN)
                }
            }
            // 作る途中・カーソルが「再開」・閉じる途中（再開・タイトルへ）。人の語は通さない。
            s if s < STATES => {
                if self.stuck_on((2 << 40) | u64::from(s), STUCK_TICKS) {
                    return self.give_up(GiveUp::Stuck);
                }
                if s == RESUME && m.frames >= MENU_READ_FRAMES {
                    Self::press(DOWN, r, &[2])
                } else {
                    0
                }
            }
            s => return self.give_up(GiveUp::PauseState(s)),
        };
        Self::out(only2(word))
    }

    fn end_lock(&mut self, m: Option<MenuState>, fresh: u16, r: &mut dyn Reads) -> Step {
        use end_menu::*;
        let Some(m) = m else {
            return self.give_up(GiveUp::Unreadable);
        };
        let word = match m.state {
            SAVE => {
                self.unmark();
                fresh & DECIDE_BITS
            }
            s if s < STATES => {
                if self.stuck_on((3 << 40) | u64::from(s), STUCK_TICKS) {
                    return self.give_up(GiveUp::Stuck);
                }
                match s {
                    REMATCH if m.frames >= MENU_READ_FRAMES => Self::press(UP, r, &[2]),
                    CHARA if m.frames >= MENU_READ_FRAMES => Self::press(DOWN, r, &[2]),
                    _ => 0,
                }
            }
            s => return self.give_up(GiveUp::EndState(s)),
        };
        Self::out(only2(word))
    }

    fn settle(&mut self, f: &Facts) -> Step {
        if f.in_match {
            self.enter(Stage::Play);
            return Self::out(Out::Feed(ZERO));
        }
        if self.stuck_on(4 << 40, MENU_STUCK_TICKS) {
            return self.give_up(GiveUp::NoMenu);
        }
        self.ticks = if f.in_menu {
            self.ticks.saturating_add(1)
        } else {
            0
        };
        if self.ticks >= SETTLE_TICKS {
            self.enter(Stage::Menu);
        }
        Self::out(Out::Feed(ZERO))
    }

    fn menu(&mut self, f: &Facts, r: &mut dyn Reads) -> Step {
        if !f.in_menu {
            self.enter(if f.in_match {
                Stage::Play
            } else {
                Stage::Settle
            });
            return Self::out(Out::Feed(ZERO));
        }
        let Some(s) = f.menu else {
            if self.stuck_on(5 << 40, MENU_STUCK_TICKS) {
                return self.give_up(GiveUp::Unreadable);
            }
            return Self::out(Out::Feed(ZERO));
        };
        if s.id == SCREEN_AFTER_MATCH || s.id == SCREEN_SAVE {
            // 保存の画面は人が操作する（印を立てられなかったときに出る）。待つ長さは決めない。
            self.unmark();
            return Self::out(Out::Pass);
        }
        let mark = (6 << 40)
            | (u64::from(s.id & 0xFF) << 32)
            | (u64::from(s.cursor as u32 & 0xFFFF) << 16)
            | u64::from(s.decided[0] & 0xFF) << 8
            | u64::from(s.decided[1] & 0xFF);
        if self.stuck_on(mark, MENU_STUCK_TICKS) {
            return self.give_up(GiveUp::Stuck);
        }
        let accepting = s.sub == 1 && s.trans == 0;
        match s.id {
            SCREEN_TITLE => {
                if !(0..TITLE_ITEMS).contains(&s.cursor) {
                    return self.give_up(GiveUp::Unreadable);
                }
                if !accepting {
                    return Self::out(Out::Feed(ZERO));
                }
                let want = match s.cursor.cmp(&TITLE_MATCH_ITEM) {
                    core::cmp::Ordering::Equal => Z,
                    core::cmp::Ordering::Less => DOWN,
                    core::cmp::Ordering::Greater => UP,
                };
                Self::out(both(Self::press(want, r, &[2])))
            }
            SCREEN_MATCH_MODE => self.mode(s, accepting, r),
            id if id == u32::from(SCREEN_DIFFICULTY)
                || id == u32::from(SCREEN_CHARA)
                || id == u32::from(SCREEN_MAP) =>
            {
                if !accepting {
                    return Self::out(Out::Feed(ZERO));
                }
                Self::out(both(Self::press(X, r, &[0, 2])))
            }
            id => self.give_up(GiveUp::Elsewhere(id)),
        }
    }

    /// 画面 6: カーソルを近い向きに 1 つずつ 4 へ。決定は押さない。
    fn mode(&mut self, s: Screen, accepting: bool, r: &mut dyn Reads) -> Step {
        if !(0..MODE_ITEMS).contains(&s.cursor) {
            return self.give_up(GiveUp::Unreadable);
        }
        if !accepting {
            return Self::out(Out::Feed(ZERO));
        }
        if s.cursor == YOKAI_CURSOR {
            // 決定を押したまま手を返すと、次の tick に入口のメニューが開く。離すまで待つ。
            if r.raw() & DECIDE_BITS != 0 {
                self.unmark();
                return Self::out(Out::Feed(ZERO));
            }
            self.stage = Stage::Done;
            return Step {
                out: Out::Feed(ZERO),
                save_quit: false,
                finish: Some(Finish::Arrived),
            };
        }
        let up = (s.cursor - YOKAI_CURSOR).rem_euclid(MODE_ITEMS);
        let down = (YOKAI_CURSOR - s.cursor).rem_euclid(MODE_ITEMS);
        let want = if up <= down { UP } else { DOWN };
        let w = Self::press(want, r, &[2]);
        let ev = EntryView {
            screen: SCREEN_MATCH_MODE,
            sub: s.sub,
            trans: s.trans,
            cursor: s.cursor as u32,
        };
        // 語に決定は無いので漏れない見込みだが、1 回の漏れは取り返せないので模型でも確かめる。
        if leaks(&ev, &r.im(2), w) {
            return Self::out(Out::Feed(ZERO));
        }
        Self::out(both(w))
    }
}

#[cfg(test)]
mod tests;
