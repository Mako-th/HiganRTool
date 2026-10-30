//! ローカルのMatch（人間 対 人間）でキーボード2台を1P・2Pに分ける判断（pure。unsafe無し）。
//!
//! - `DeviceTable`: Raw Inputが知らせるキーボードごとの押下の表（VKの256ビット）。書くのはWM_INPUTを受けるウィンドウのスレッド（ゲームのものとは限らない）。読むのはゲームのスレッド。
//! - `vk_word`: VKの集合 → ゲームの入力語（`0x0042B88F`のGetKeyboardStateを使う経路での添字）。
//! - `Machine`: 人間 対 人間を選ぶたびに1P・2Pそれぞれに7つのキーを順に押してもらい、届いた台を全部その人の台にするステートマシン。ゲームのスレッドで1 tickに1回`step`を呼ぶ。確保しない・パニックしない。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering};

use crate::ini::Ini;
use crate::input_model::keys;
use crate::menu_model::{SCREEN_DIFFICULTY, is_human_selection};
use crate::ui_entry::SCREEN_MATCH_MODE;

pub const DEVICES: usize = 8;
const VK_WORDS: usize = 8;
/// スキャンコード（MakeCodeの下位7ビット ＋ E0のフラグを最上位ビット）の数。
const SCAN_CODES: usize = 256;

/// `hDevice`が0の入力を表に記録するときの代わりの値（0は表の空きを表すので別の値にする）。実在のハンドルがこの値になることは無い。
const NULL_DEVICE: usize = usize::MAX;

/// VKの256ビット（添字`vk >> 5`のワードのbit `vk & 31`）。
pub type Vks = [u32; VK_WORDS];

/// 仮想キーコード（Win32の公開の定義）。
pub mod vk {
    pub const RETURN: u8 = 0x0D;
    pub const SHIFT: u8 = 0x10;
    pub const CONTROL: u8 = 0x11;
    pub const ESCAPE: u8 = 0x1B;
    pub const LEFT: u8 = 0x25;
    pub const UP: u8 = 0x26;
    pub const RIGHT: u8 = 0x27;
    pub const DOWN: u8 = 0x28;
    pub const D: u8 = 0x44;
    pub const Q: u8 = 0x51;
    pub const R: u8 = 0x52;
    pub const S: u8 = 0x53;
    pub const X: u8 = 0x58;
    pub const Z: u8 = 0x5A;
    pub const NUMPAD2: u8 = 0x62;
    pub const NUMPAD4: u8 = 0x64;
    pub const NUMPAD6: u8 = 0x66;
    pub const NUMPAD8: u8 = 0x68;
    pub const LSHIFT: u8 = 0xA0;
    pub const RSHIFT: u8 = 0xA1;
    pub const LCONTROL: u8 = 0xA2;
    pub const RCONTROL: u8 = 0xA3;
}

/// キー → 入力語のビット。P / Home（`keys::SHOT`）は`shot_key_win`が別に扱うので含めない。斜めのテンキー（7・9・1・3）は逆アセンブルで照合していないので含めない。
const KEY_WORDS: [(u8, u16); 22] = [
    (vk::Z, keys::Z),
    (vk::X, keys::X),
    (vk::SHIFT, keys::SHIFT),
    (vk::LSHIFT, keys::SHIFT),
    (vk::RSHIFT, keys::SHIFT),
    (vk::ESCAPE, keys::ESC),
    (vk::UP, keys::UP),
    (vk::NUMPAD8, keys::UP),
    (vk::DOWN, keys::DOWN),
    (vk::NUMPAD2, keys::DOWN),
    (vk::LEFT, keys::LEFT),
    (vk::NUMPAD4, keys::LEFT),
    (vk::RIGHT, keys::RIGHT),
    (vk::NUMPAD6, keys::RIGHT),
    (vk::CONTROL, keys::CTRL),
    (vk::LCONTROL, keys::CTRL),
    (vk::RCONTROL, keys::CTRL),
    (vk::Q, keys::Q),
    (vk::S, keys::S),
    (vk::RETURN, keys::ENTER),
    (vk::D, keys::D),
    (vk::R, keys::R),
];

fn bit_of(vks: &Vks, vk: u8) -> bool {
    vks.get(usize::from(vk >> 5))
        .is_some_and(|w| w >> (vk & 31) & 1 != 0)
}

/// VKの集合 → 入力語。
pub fn vk_word(down: &Vks) -> u16 {
    KEY_WORDS
        .iter()
        .filter(|&&(vk, _)| bit_of(down, vk))
        .fold(0, |w, &(_, bit)| w | bit)
}

/// 1つのVKが立てる入力語のビット（無ければ0）。
fn word_of_vk(vk: u8) -> u16 {
    KEY_WORDS
        .iter()
        .filter(|&&(k, _)| k == vk)
        .fold(0, |w, &(_, bit)| w | bit)
}

/// ゲームのスレッドが1 tickに1回取る台ごとの表のコピー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snap {
    /// 台ごとの今押されているキーの入力語。
    pub words: [u16; DEVICES],
    /// 前のコピーから後に押されたキー（押した瞬間。押し続けのリピートは数えない）の入力語。短く押して離したキーも残る。
    pub pressed: [u16; DEVICES],
    /// 何かのキー（入力語に無いキーも含む）が押されている台の集合（bit `i` = 台`i`）。
    pub down: u8,
    /// 起動してからキーが押された台の集合。
    pub seen: u8,
}

/// Raw Inputのキーボードごとの押下の表。静的な1つの表をWM_INPUTの受け取りと台本の命令が書き込み、tickが読む。
pub struct DeviceTable {
    /// 台の`hDevice`（0 = 未使用）。8台まで覚え、9台目以降は数えない。
    handles: [AtomicUsize; DEVICES],
    down: [[AtomicU32; VK_WORDS]; DEVICES],
    /// 前のコピーから後に押された入力語。
    latched: [AtomicU32; DEVICES],
    /// 押したときのVKをスキャンコードごとに覚える（0 = 押されていない）。
    /// 離したときの通知のVKが押したときと違っても、押したときのVKを消すために使う。
    /// NumLockがONのテンキーはShiftが押されている間だけVKが方向キーになる。
    /// Shiftを先に離すと離したときの通知のVKが変わる。
    made: [[AtomicU8; SCAN_CODES]; DEVICES],
    seen: AtomicU32,
}

impl DeviceTable {
    pub const fn new() -> DeviceTable {
        DeviceTable {
            handles: [const { AtomicUsize::new(0) }; DEVICES],
            down: [const { [const { AtomicU32::new(0) }; VK_WORDS] }; DEVICES],
            latched: [const { AtomicU32::new(0) }; DEVICES],
            made: [const { [const { AtomicU8::new(0) }; SCAN_CODES] }; DEVICES],
            seen: AtomicU32::new(0),
        }
    }

    /// `hDevice`の台の番号。初めて見る`hDevice`は空いている番号を取る。8台が埋まっていれば`None`。
    /// 0はSendInputやリモート操作ツール（Parsecなど）のキーに付く値（推定）。1つの台として数える（表の空きは`NULL_DEVICE`で別に持つ）。
    pub fn slot_of(&self, handle: usize) -> Option<usize> {
        let key = if handle == 0 { NULL_DEVICE } else { handle };
        for (i, h) in self.handles.iter().enumerate() {
            match h.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(i),
                Err(cur) if cur == key => return Some(i),
                Err(_) => {}
            }
        }
        None
    }

    /// 台`dev`の`hDevice`が0かどうか（詳細のログに印を付けるため）。
    pub fn is_null_device(&self, dev: usize) -> bool {
        self.handles
            .get(dev)
            .is_some_and(|h| h.load(Ordering::Acquire) == NULL_DEVICE)
    }

    /// 台`dev`のキー`vk`を押した・離した。押した瞬間（離れていたキーが立つ）だけ`pressed`に残す。
    pub fn set_key(&self, dev: usize, vk: u8, down: bool) {
        let Some(word) = self.down.get(dev).and_then(|d| d.get(usize::from(vk >> 5))) else {
            return;
        };
        let mask = 1u32 << (vk & 31);
        if down {
            self.seen.fetch_or(1 << dev, Ordering::AcqRel);
            if word.fetch_or(mask, Ordering::AcqRel) & mask == 0
                && let Some(l) = self.latched.get(dev)
            {
                l.fetch_or(u32::from(word_of_vk(vk)), Ordering::AcqRel);
            }
        } else {
            word.fetch_and(!mask, Ordering::AcqRel);
        }
    }

    /// Raw Inputのキー1つのイベント。`scan`はスキャンコードの添字（`scan_index`）。押したVKを覚え、離すときは覚えたVKを消す。
    pub fn key_event(&self, dev: usize, vk: u8, scan: Option<u8>, down: bool) {
        let slot = scan.and_then(|s| self.made.get(dev).and_then(|d| d.get(usize::from(s))));
        if down {
            if let Some(slot) = slot {
                let old = slot.swap(vk, Ordering::AcqRel);
                if old != 0 && old != vk {
                    self.set_key(dev, old, false);
                }
            }
            self.set_key(dev, vk, true);
        } else {
            let made = slot.map_or(0, |s| s.swap(0, Ordering::AcqRel));
            self.set_key(dev, if made != 0 { made } else { vk }, false);
        }
    }

    /// 台`dev`の押下を`vks`だけにする（台本の命令。新しく立ったキーは押した瞬間になる）。
    pub fn set_keys(&self, dev: usize, vks: &[u8]) {
        if dev >= DEVICES {
            return;
        }
        for vk in 0..=u8::MAX {
            let want = vks.contains(&vk);
            let now = self
                .down
                .get(dev)
                .and_then(|d| d.get(usize::from(vk >> 5)))
                .is_some_and(|w| w.load(Ordering::Acquire) >> (vk & 31) & 1 != 0);
            if want != now {
                self.set_key(dev, vk, want);
            }
        }
    }

    /// 全部の台の押下を消す（前面を失ったとき。離したときの通知が来ないため）。押された台の記録は残す。
    pub fn clear_keys(&self) {
        for d in &self.down {
            for w in d {
                w.store(0, Ordering::Release);
            }
        }
        for d in &self.made {
            for s in d {
                s.store(0, Ordering::Release);
            }
        }
    }

    /// 表をコピーして押した瞬間の記録を空にする。`full`がfalseなら押下のワードは読まず（0）、記録の掃除と台の集合だけを行う。
    pub fn snap(&self, full: bool) -> Snap {
        let mut s = Snap::default();
        for dev in 0..DEVICES {
            let latched = self
                .latched
                .get(dev)
                .map_or(0, |l| l.swap(0, Ordering::AcqRel));
            if !full {
                continue;
            }
            let mut down: Vks = [0; VK_WORDS];
            let mut any = false;
            if let Some(d) = self.down.get(dev) {
                for (dst, w) in down.iter_mut().zip(d) {
                    *dst = w.load(Ordering::Acquire);
                    any |= *dst != 0;
                }
            }
            if let Some(w) = s.words.get_mut(dev) {
                *w = vk_word(&down);
            }
            if let Some(p) = s.pressed.get_mut(dev) {
                *p = latched as u16;
            }
            if any {
                s.down |= 1 << dev;
            }
        }
        s.seen = self.seen.load(Ordering::Acquire) as u8;
        s
    }

    /// 検査用: 表を空にする。
    pub fn reset(&self) {
        for h in &self.handles {
            h.store(0, Ordering::Release);
        }
        self.clear_keys();
        for l in &self.latched {
            l.store(0, Ordering::Release);
        }
        self.seen.store(0, Ordering::Release);
    }
}

/// Raw Inputの`MakeCode`とE0のフラグから押したときのVKを覚える添字。`MakeCode`が0のもの（OSが作ったイベント）は覚えない。
pub fn scan_index(make_code: u16, e0: bool) -> Option<u8> {
    let code = (make_code & 0x7F) as u8;
    (code != 0).then_some(code | if e0 { 0x80 } else { 0 })
}

/// NumLockがONのとき、テンキーやInsert・Delete・矢印はOSが偽のShift（E0付きの0x2A・0x36）を前後に挟む。本物のShiftの押下を消すので捨てる。
pub fn is_fake_shift(make_code: u16, e0: bool) -> bool {
    e0 && matches!(make_code, 0x2A | 0x36)
}

impl Default for DeviceTable {
    fn default() -> Self {
        DeviceTable::new()
    }
}

// ---- ステートマシン ----

/// ゲームの場面（呼び手が見たもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scene {
    /// 読み込みなど。状態を動かさない。
    #[default]
    Other,
    /// メニューの場面。`select_kind`は`[0x4A7EA4]`（0 = 人間 対 人間のキャラ選択）。`accepting`は画面が押下を読む状態かどうか（`menu_model::reads_presses`）。
    Menu {
        screen: u32,
        select_kind: u32,
        accepting: bool,
    },
    /// 試合の場面（試合の始まりの切り替えのtickを含む）。
    Match,
}

/// 1P・2Pそれぞれに押してもらうキー（入力語のビット）の順。
pub const ASK_KEYS: [u16; 7] = [
    keys::Z,
    keys::X,
    keys::SHIFT,
    keys::UP,
    keys::DOWN,
    keys::LEFT,
    keys::RIGHT,
];
/// 1人ぶんのステップ数。
pub const ASK_STEPS: u8 = ASK_KEYS.len() as u8;

/// 聞いている間に画面へ出す案内。数は`ASK_KEYS`の添字（0始まり）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    P1(u8),
    P2(u8),
    /// 割り当てに使ったキーを離すのを待っている。
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// `stage` 0..7が1Pのステップ、7..14が2Pのステップ。`p1`・`p2`は今までに受けた台の集合（bit `i` = 台`i`）。
    Ask {
        stage: u8,
        p1: u8,
        p2: u8,
    },
    /// 割り当てに使ったキーが離れるまでゲームには0を渡す。`p1`・`p2`は台の集合。
    Release {
        p1: u8,
        p2: u8,
    },
    Active {
        p1: u8,
        p2: u8,
    },
    /// 今回は使わないことが決定済み（Escかパッド）。押しているEscや決定がゲームへ渡らないよう、ゲームの入力と表が空になるまで0を渡す。
    Drain {
        ticks: u8,
    },
    /// 今回は使わない。人間 対 人間の画面を出るまでゲームのいつもの入力。
    Skipped,
}

/// 人間 対 人間の分け方で詳細のログに書くイベント。`Machine::take_event`で1回ずつ取る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// 1P・2Pを聞き始めた（キーが押された台が`devices`台）。
    Asking { devices: u32 },
    /// 1P・2Pを割り当てた（台の集合。bit `i` = 表の番号`i`）。
    Assigned { p1: u8, p2: u8 },
    /// Escで今回は分けないことになった。
    SkippedByEsc,
    /// パッドの決定で今回は分けないことになった（表にキーが無いのにゲームの入力が続いた）。
    SkippedByPad,
    /// 割り当てた後に人間 対 人間を抜けた（受け付ける画面が人間 対 人間の選択の外にある）。割り当てを解いた。
    Released,
}

/// イベントの詳細のログの1行。台の番号は表の番号（台本の`kbd=`と同じ）。hDevice 0の台には印を付ける。
pub fn event_line(ev: Event, table: &DeviceTable) -> String {
    let dev = |d: u8| {
        if table.is_null_device(usize::from(d)) {
            format!("台 {d}（hDevice 0）")
        } else {
            format!("台 {d}")
        }
    };
    let set = |m: u8| {
        (0..DEVICES as u8)
            .filter(|&d| m >> d & 1 != 0)
            .map(dev)
            .collect::<Vec<_>>()
            .join("・")
    };
    let head = "キーボードの分割:";
    match ev {
        Event::Asking { devices } => format!(
            "{head} 人間 対 人間なので 1P・2P のキーボードを聞きます（キーが押されたキーボード {devices} 台）。"
        ),
        Event::Assigned { p1, p2 } => {
            // 印の「）」の後は空けずに「に」を続ける（「台 1（hDevice 0）に」）。
            let ni = |m: u8| {
                let s = set(m);
                if s.ends_with('）') {
                    format!("{s}に")
                } else {
                    format!("{s} に")
                }
            };
            format!("{head} 1P は{}、2P は{}割り当てました。", set(p1), ni(p2))
        }
        Event::SkippedByEsc => format!("{head} Esc が押されたので、今回は分けません。"),
        Event::SkippedByPad => format!("{head} パッドが押されたので、今回は分けません。"),
        Event::Released => format!("{head} 人間 対 人間を抜けたので、割り当てを解きました。"),
    }
}

/// 割り当てを持つ状態から離れるときのイベント（割り当てを持たない状態からは出ない）。
fn released(phase: Phase) -> Option<Event> {
    matches!(phase, Phase::Active { .. } | Phase::Release { .. }).then_some(Event::Released)
}

/// 1 tickに呼び手が渡す事実。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Facts {
    /// iniの`[input] split_keyboards`。
    pub enabled: bool,
    /// ネットのセッションがある・開こうとしている。
    pub net: bool,
    pub scene: Scene,
    pub snap: Snap,
    /// キーを読んでよい間（ゲームが前面。台本の実行は常にtrue）。`Active`のときだけ呼び手が作る。
    pub gate: bool,
    /// ゲーム自身の入力関数を読んだ入力語（3サイドのOR）。パッドの判定に使う。`Machine::polls`がtrueの間だけ呼び手が作る（ほかは0）。
    pub game_word: u16,
    /// リプレイの再生中（`[0x4A7EC4]`のbit3）。分けている間の試合の場面でだけ呼び手が作る。
    pub replay: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// 何もしない。元の入力（poll_all）へ。
    Pass,
    /// 3本とも0をfeedする。案内があれば画面へ出す。
    Hold(Option<Prompt>),
    /// `[0]`=1P・`[1]`=2P・`[2]`=1P|2Pをfeedする。
    Feed { w1: u16, w2: u16 },
}

/// パッドの判定に使うビット（P / Homeはキーボードの表に出るので数えない）。
const PAD_MASK: u16 = 0x7FFF & !keys::SHOT;
/// 聞いている間、表にキーが無いのにゲームの入力がこのtick数続いたらパッドの押下とみなす。
/// キーを離した直後に表とゲームの読みが1 tickずれるのを許す。
const PAD_TICKS: u8 = 3;
/// 使わないことが決定済みの間、押しているキーとボタンが離れるのを待つ上限のtick数。押しっぱなしのボタンや傾いたスティックで画面を止め続けないようにする（2秒）。
const DRAIN_MAX_TICKS: u8 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Machine {
    phase: Phase,
    /// メニューの場面で最後に見た画面とその前に見た別の画面。画面6の間は`step`が呼ばれない（メニューが入力を占有する）ので毎tick呼ぶ`observe`が記録する。
    screen: Option<u32>,
    from: Option<u32>,
    pad_ticks: u8,
    /// 直前の`step`で生じたイベント（まだ取られていない分。1回の`step`で高々1つ）。
    event: Option<Event>,
}

impl Machine {
    pub const fn new() -> Machine {
        Machine {
            phase: Phase::Idle,
            screen: None,
            from: None,
            pad_ticks: 0,
            event: None,
        }
    }

    /// 直前の`step`で生じたイベントを取る（取ったら消える）。
    pub fn take_event(&mut self) -> Option<Event> {
        self.event.take()
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// ゲーム自身の入力関数を読む必要があるかどうか（呼び手が`Facts::game_word`を作る）。
    /// 1P・2Pを聞いている間と、使わないことが決定済みで離すのを待つ間のパッドの判定だけに使う。
    /// 分けている間は割り当てた2台の入力語だけを使うので読まない。
    pub fn polls(&self) -> bool {
        matches!(self.phase, Phase::Ask { .. } | Phase::Drain { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Active { .. })
    }

    /// 分けている最中（案内・離すのを待つ・分けた入力）。試合の場面でリプレイかを見る必要がある。
    pub fn is_engaged(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Skipped)
    }

    /// 表の押下のワードまで読む必要があるかどうか。メニューの間は人間 対 人間の始まりを監視するために読む。
    pub fn wants_full(&self, scene: Scene) -> bool {
        self.is_engaged() || matches!(scene, Scene::Menu { .. })
    }

    /// 人間 対 人間を選んで画面が6から7へ移ったtick（聞き始めるtick）かどうか（`observe`の後`step`の前に呼ぶ）。呼び手は聞き始める前にRaw Inputの登録の送信先を調べ直す。
    pub fn arming(&self, scene: Scene) -> bool {
        matches!(
            scene,
            Scene::Menu { screen, select_kind, .. }
                if screen == u32::from(SCREEN_DIFFICULTY) && is_human_selection(screen, select_kind)
        ) && self.from == Some(SCREEN_MATCH_MODE)
            && self.phase == Phase::Idle
    }

    /// 場面を記録する（ゲームのスレッドで毎tick。`step`を呼ばないtickも）。
    /// メニューの場面で人間 対 人間の選択の画面の外が押下を受け付けたら、今回の分け方は終わる。
    /// 難易度でキャンセルして人間 対 人間を選び直したらまた聞く。
    /// 受け付けない間の通り抜けの画面（試合の後）では割り当てを残す。
    pub fn observe(&mut self, scene: Scene) {
        if let Scene::Menu {
            screen,
            select_kind,
            accepting,
        } = scene
        {
            if accepting && !is_human_selection(screen, select_kind) {
                self.leave();
            }
            if self.screen != Some(screen) {
                self.from = self.screen;
                self.screen = Some(screen);
            }
        }
    }

    fn reset(&mut self) {
        *self = Machine::new();
    }

    /// 分けるのを終えてIdleへ戻す。割り当てを解く（Active・Release）ときだけイベントを残す。
    fn leave(&mut self) {
        if let Some(ev) = released(self.phase) {
            self.event = Some(ev);
        }
        self.phase = Phase::Idle;
    }

    /// 表にキーが無いのにゲームの入力があるのが`limit` tick続いたかどうか。
    fn stalled(&mut self, f: &Facts, limit: u8) -> bool {
        if f.game_word & PAD_MASK != 0 && f.snap.down == 0 {
            self.pad_ticks = self.pad_ticks.saturating_add(1);
        } else {
            self.pad_ticks = 0;
        }
        self.pad_ticks >= limit
    }

    /// 押した瞬間に`bit`を持つ台の集合（`not`の台を除く）。
    fn pressed_set(f: &Facts, bit: u16, not: u8) -> u8 {
        f.snap
            .pressed
            .iter()
            .enumerate()
            .filter(|&(i, &p)| p & bit != 0 && not >> i & 1 == 0)
            .fold(0, |m, (i, _)| m | 1 << i)
    }

    /// 台の集合の今押されているキーの入力語のOR。
    fn word_of(f: &Facts, set: u8) -> u16 {
        f.snap
            .words
            .iter()
            .enumerate()
            .filter(|&(i, _)| set >> i & 1 != 0)
            .fold(0, |w, (_, &x)| w | x)
    }

    pub fn step(&mut self, f: &Facts) -> Act {
        // イベントは今回の`step`のぶんだけ。前のtickの取り忘れを残さない。
        self.event = None;
        if !f.enabled || f.net {
            let was = self.phase;
            self.reset();
            self.event = released(was);
            return Act::Pass;
        }
        self.observe(f.scene);
        match f.scene {
            Scene::Menu {
                screen,
                select_kind,
                ..
            } => {
                if is_human_selection(screen, select_kind)
                    && screen == u32::from(SCREEN_DIFFICULTY)
                    && self.from.take() == Some(SCREEN_MATCH_MODE)
                    && self.phase == Phase::Idle
                {
                    if f.snap.seen.count_ones() < 2 {
                        return Act::Pass;
                    }
                    // 人間 対 人間を選んだキーの押下が表に残っているのでこのtickでは聞かない。
                    self.phase = Phase::Ask {
                        stage: 0,
                        p1: 0,
                        p2: 0,
                    };
                    self.pad_ticks = 0;
                    self.event = Some(Event::Asking {
                        devices: f.snap.seen.count_ones(),
                    });
                    return Act::Hold(Some(Prompt::P1(0)));
                }
            }
            Scene::Match if f.replay => self.leave(),
            Scene::Match | Scene::Other => {}
        }
        match self.phase {
            Phase::Idle | Phase::Skipped => Act::Pass,
            Phase::Ask { .. } => self.ask(f),
            Phase::Drain { ticks } => {
                if f.game_word & PAD_MASK == 0 && f.snap.down == 0 || ticks >= DRAIN_MAX_TICKS {
                    self.phase = Phase::Skipped;
                    return Act::Pass;
                }
                self.phase = Phase::Drain { ticks: ticks + 1 };
                Act::Hold(None)
            }
            Phase::Release { p1, p2 } => {
                if Self::word_of(f, p1) | Self::word_of(f, p2) == 0 {
                    self.phase = Phase::Active { p1, p2 };
                    return Act::Hold(None);
                }
                Act::Hold(Some(Prompt::Release))
            }
            // 割り当てた2台の入力語だけを使う。ほかの台とパッドは読まないので割り当てが外れても戻さない（Idleへ戻るのは場面の終わりだけ）。
            Phase::Active { p1, p2 } => {
                if f.gate {
                    Act::Feed {
                        w1: Self::word_of(f, p1),
                        w2: Self::word_of(f, p2),
                    }
                } else {
                    Act::Feed { w1: 0, w2: 0 }
                }
            }
        }
    }

    /// 今回は分けないことにする。押しているEsc・決定がゲームへ渡らないよう空の入力を渡す（`Drain`）。
    fn skip(&mut self, why: Event) -> Act {
        self.phase = Phase::Drain { ticks: 0 };
        self.event = Some(why);
        Act::Hold(None)
    }

    fn ask(&mut self, f: &Facts) -> Act {
        // Escは表で押した瞬間に見る。パッドは表にキーが無いのにゲームの入力が続いたことで見る。
        if f.snap.pressed.iter().any(|&p| p & keys::ESC != 0) {
            return self.skip(Event::SkippedByEsc);
        }
        if self.stalled(f, PAD_TICKS) {
            return self.skip(Event::SkippedByPad);
        }
        let Phase::Ask { stage, p1, p2 } = self.phase else {
            return Act::Pass;
        };
        let second = stage >= ASK_STEPS;
        let bit = ASK_KEYS
            .get(usize::from(stage % ASK_STEPS))
            .copied()
            .unwrap_or(0);
        // 2Pのステップは1Pの台の押下を受けない。
        let got = Self::pressed_set(f, bit, if second { p1 } else { 0 });
        let (p1, p2) = if second {
            (p1, p2 | got)
        } else {
            (p1 | got, p2)
        };
        let stage = if got != 0 { stage + 1 } else { stage };
        if stage >= 2 * ASK_STEPS {
            self.phase = Phase::Release { p1, p2 };
            self.event = Some(Event::Assigned { p1, p2 });
            return Act::Hold(Some(Prompt::Release));
        }
        self.phase = Phase::Ask { stage, p1, p2 };
        let next = stage % ASK_STEPS;
        Act::Hold(Some(if stage >= ASK_STEPS {
            Prompt::P2(next)
        } else {
            Prompt::P1(next)
        }))
    }
}

// ---- ini ----

pub const SECTION: &str = "input";
pub const KEY: &str = "split_keyboards";

/// `[input] split_keyboards`を読む。カテゴリかキーが無ければ有効。知らないキー・`0`と`1`以外の値は断る（綴りの誤りで切ったつもりが有効のままにならないように）。
pub fn from_ini(ini: &Ini) -> Result<bool, String> {
    for (k, line) in ini.keys(SECTION) {
        if k != KEY {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} の記法が不適切です（使用可能なキー: {KEY}）"
            ));
        }
    }
    match ini.get(SECTION, KEY) {
        None | Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(v) => Err(format!(
            "[{SECTION}] {KEY} の値 {v:?} は 0 か 1 にしてください"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ini;

    fn set(vks: &[u8]) -> Vks {
        let mut v = [0u32; VK_WORDS];
        for &k in vks {
            v[usize::from(k >> 5)] |= 1 << (k & 31);
        }
        v
    }

    #[test]
    fn every_key_maps_to_its_word_and_overlapping_keys_share_a_bit() {
        for (vk, word) in [
            (vk::Z, 0x0001),
            (vk::X, 0x0002),
            (vk::SHIFT, 0x0004),
            (vk::LSHIFT, 0x0004),
            (vk::RSHIFT, 0x0004),
            (vk::ESCAPE, 0x0008),
            (vk::UP, 0x0010),
            (vk::NUMPAD8, 0x0010),
            (vk::DOWN, 0x0020),
            (vk::NUMPAD2, 0x0020),
            (vk::LEFT, 0x0040),
            (vk::NUMPAD4, 0x0040),
            (vk::RIGHT, 0x0080),
            (vk::NUMPAD6, 0x0080),
            (vk::CONTROL, 0x0100),
            (vk::LCONTROL, 0x0100),
            (vk::RCONTROL, 0x0100),
            (vk::Q, 0x0200),
            (vk::S, 0x0400),
            (vk::RETURN, 0x1000),
            (vk::D, 0x2000),
            (vk::R, 0x4000),
        ] {
            assert_eq!(vk_word(&set(&[vk])), word, "vk {vk:#04x}");
        }
        assert_eq!(vk_word(&set(&[vk::UP, vk::NUMPAD8])), 0x0010);
        assert_eq!(vk_word(&set(&[vk::Z, vk::X, vk::LEFT])), 0x0043);
        assert_eq!(vk_word(&set(&[])), 0);
    }

    #[test]
    fn keys_the_game_does_not_read_or_handled_elsewhere_make_no_word() {
        // P（0x50）・Home（0x24）はスクリーンショット、A（0x41）・数字・Tab・Fキー・Alt・斜めのテンキー（7・9・1・3）。
        for vk in [
            0x50u8, 0x24, 0x41, 0x31, 0x09, 0x70, 0x12, 0x67, 0x69, 0x61, 0x63,
        ] {
            assert_eq!(vk_word(&set(&[vk])), 0, "vk {vk:#04x}");
        }
        assert_eq!(
            vk_word(&[u32::MAX; 8]) & 0x0800,
            0,
            "P / Home のビットは立たない"
        );
    }

    #[test]
    fn the_table_counts_devices_and_marks_only_new_presses() {
        let t = DeviceTable::new();
        let a = t.slot_of(0xA0).unwrap();
        let b = t.slot_of(0xB0).unwrap();
        assert_ne!(a, b);
        assert_eq!(t.slot_of(0xA0), Some(a), "同じ hDevice は同じ台");
        assert_eq!(t.snap(true).seen, 0, "押されるまで数えない");
        t.set_key(a, vk::Z, true);
        t.set_key(a, vk::Z, true);
        let s = t.snap(true);
        assert_eq!(
            (s.words[a], s.pressed[a], s.down, s.seen),
            (1, 1, 1 << a, 1 << a)
        );
        assert_eq!(
            t.snap(true).pressed[a],
            0,
            "リピートとコピーの後は押した瞬間ではない"
        );
        t.set_key(a, vk::Z, true);
        assert_eq!(
            t.snap(true).pressed[a],
            0,
            "押し続けのリピートは押した瞬間にならない"
        );
        t.set_key(a, vk::Z, false);
        t.set_key(a, vk::Z, true);
        t.set_key(a, vk::Z, false);
        let s = t.snap(true);
        assert_eq!(
            (s.words[a], s.pressed[a]),
            (0, 1),
            "コピーの間に押して離したキーも残る"
        );
        t.set_key(b, vk::LEFT, true);
        assert_eq!(t.snap(true).seen.count_ones(), 2);
        t.clear_keys();
        let s = t.snap(true);
        assert_eq!((s.words[b], s.down, s.seen.count_ones()), (0, 0, 2));
    }

    #[test]
    fn a_null_hdevice_and_a_real_one_make_two_devices() {
        let t = DeviceTable::new();
        let real = t.slot_of(0xA0).unwrap();
        let null = t.slot_of(0).unwrap();
        assert_ne!(real, null, "hDevice 0 と本物の台が同じ台になった");
        assert_eq!(t.slot_of(0), Some(null), "0 が毎回新しい台になった");
        t.set_key(real, vk::Z, true);
        t.set_key(null, vk::Z, true);
        let s = t.snap(true);
        assert_eq!(s.seen.count_ones(), 2);
        assert_eq!((s.pressed[real], s.pressed[null]), (1, 1));
        let mut m = Machine::new();
        let f = |screen| Facts {
            enabled: true,
            scene: Scene::Menu {
                screen,
                select_kind: 0,
                accepting: true,
            },
            snap: Snap {
                seen: s.seen,
                ..Snap::default()
            },
            gate: true,
            ..Facts::default()
        };
        m.step(&f(6));
        m.step(&f(7));
        assert_eq!(m.phase(), FIRST_ASK, "2 台と数えて聞き始める");
    }

    #[test]
    fn a_ninth_device_is_not_counted_and_a_light_snap_still_clears_the_latch() {
        let t = DeviceTable::new();
        for i in 1..=8usize {
            assert_eq!(t.slot_of(i * 0x10), Some(i - 1));
        }
        assert_eq!(t.slot_of(0x99), None);
        t.set_key(0, vk::Z, true);
        let light = t.snap(false);
        assert_eq!(
            (light.words, light.pressed, light.down),
            ([0; 8], [0; 8], 0)
        );
        assert_eq!(light.seen, 1);
        assert_eq!(
            t.snap(true).pressed[0],
            0,
            "軽いコピーが記録を空にしなかった"
        );
        t.set_keys(1, &[vk::X, vk::UP]);
        let s = t.snap(true);
        assert_eq!((s.words[1], s.pressed[1]), (0x0012, 0x0012));
        t.set_keys(1, &[vk::X]);
        assert_eq!(t.snap(true).words[1], 0x0002);
        t.set_keys(1, &[]);
        assert_eq!(t.snap(true).words[1], 0);
        t.set_keys(9, &[vk::X]);
        t.set_key(9, vk::X, true);
        assert_eq!(t.snap(true).seen, 0b11);
    }

    #[test]
    fn a_made_vk_is_remembered_per_scan_code_so_a_changed_break_vk_still_releases_it() {
        let t = DeviceTable::new();
        let d = t.slot_of(0xA0).unwrap();
        let sc = scan_index(0x4B, false);
        // NumLockがONのテンキー4（スキャンコード0x4B）はShiftが押されている間だけVKが左になる。
        t.key_event(d, vk::LEFT, sc, true);
        assert_eq!(t.snap(true).words[d], keys::LEFT);
        t.key_event(d, vk::NUMPAD4, sc, false);
        let s = t.snap(true);
        assert_eq!((s.words[d], s.down), (0, 0), "離した VK が違うと残る");
        // 同じスキャンコードでVKが替わって押された（Shiftを離してからテンキーを押し直したのでなく、押したまま替わった）ときは前のVKを離す。
        t.key_event(d, vk::LEFT, sc, true);
        t.key_event(d, vk::NUMPAD4, sc, true);
        assert_eq!(t.snap(true).words[d], keys::LEFT);
        t.key_event(d, vk::NUMPAD4, sc, false);
        assert_eq!(t.snap(true).down, 0);
        // スキャンコードを覚えないイベントは離したVKをそのまま消す。
        t.key_event(d, vk::Z, None, true);
        t.key_event(d, vk::Z, None, false);
        assert_eq!(t.snap(true).down, 0);
        // E0の有無で別のキー（矢印の左E0 4Bとテンキー4）。
        assert_ne!(scan_index(0x4B, true), scan_index(0x4B, false));
        assert_eq!(scan_index(0, false), None);
        assert!(is_fake_shift(0x2A, true) && is_fake_shift(0x36, true));
        assert!(!is_fake_shift(0x2A, false), "本物の Shift を偽と見た");
        assert!(!is_fake_shift(0x1D, true));
        // 前面を失って消した後に、覚えたスキャンコードで余計なキーを消さない。
        t.key_event(d, vk::LEFT, sc, true);
        t.clear_keys();
        t.key_event(d, vk::NUMPAD4, sc, false);
        assert_eq!(t.snap(true).down, 0);
    }

    fn facts(scene: Scene, snap: Snap) -> Facts {
        Facts {
            enabled: true,
            scene,
            snap,
            gate: true,
            ..Facts::default()
        }
    }

    const MATCH_MODE: Scene = Scene::Menu {
        screen: 6,
        select_kind: 0,
        accepting: true,
    };
    const DIFFICULTY: Scene = Scene::Menu {
        screen: 7,
        select_kind: 0,
        accepting: true,
    };
    const CHARA: Scene = Scene::Menu {
        screen: 8,
        select_kind: 0,
        accepting: true,
    };

    fn two_devices() -> Snap {
        Snap {
            seen: 0b11,
            ..Snap::default()
        }
    }

    fn pressed(dev: usize, word: u16) -> Snap {
        let mut s = two_devices();
        s.pressed[dev] = word;
        s.words[dev] = word;
        s.down |= 1 << dev;
        s
    }

    /// 台のキーが何も押されていなくてゲームの入力だけ`word`がある（パッドの押下）。
    fn game_only(scene: Scene, word: u16) -> Facts {
        let mut f = facts(scene, two_devices());
        f.game_word = word;
        f
    }

    /// 本番と同じく画面6では`observe`だけを呼び、7に移ったtickから`step`を呼んで聞き始めたステートマシン。
    fn asking() -> Machine {
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(
            m.step(&facts(DIFFICULTY, two_devices())),
            Act::Hold(Some(Prompt::P1(0)))
        );
        m
    }

    const FIRST_ASK: Phase = Phase::Ask {
        stage: 0,
        p1: 0,
        p2: 0,
    };

    /// 台`devs`が同じtickに`word`のキーを押した。
    fn pressed_by(devs: &[usize], word: u16) -> Snap {
        let mut s = two_devices();
        for &d in devs {
            s.pressed[d] = word;
            s.words[d] = word;
            s.down |= 1 << d;
        }
        s
    }

    /// いま聞いているステップのキーを台`devs`が押したtickを進める。
    fn pass_stage(m: &mut Machine, devs: &[usize]) -> Act {
        let Phase::Ask { stage, .. } = m.phase() else {
            panic!("聞いていない: {:?}", m.phase());
        };
        let word = ASK_KEYS[usize::from(stage % ASK_STEPS)];
        m.step(&facts(DIFFICULTY, pressed_by(devs, word)))
    }

    /// 7キーを台`p1`が、続く7キーを台`p2`が押して離すのを待つステップまで進めたステートマシン。
    fn answered(p1: usize, p2: usize) -> Machine {
        let mut m = asking();
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[p1]);
        }
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[p2]);
        }
        assert_eq!(
            m.phase(),
            Phase::Release {
                p1: 1 << p1,
                p2: 1 << p2
            }
        );
        m
    }

    fn active(p1: u8, p2: u8) -> Machine {
        let mut m = answered(usize::from(p1), usize::from(p2));
        m.step(&facts(DIFFICULTY, two_devices()));
        assert_eq!(
            m.phase(),
            Phase::Active {
                p1: 1 << p1,
                p2: 1 << p2
            }
        );
        m
    }

    #[test]
    fn asking_starts_only_when_the_screen_goes_from_6_to_7_with_two_keyboards() {
        let one = Snap {
            seen: 0b1,
            ..Snap::default()
        };
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(m.step(&facts(DIFFICULTY, one)), Act::Pass, "台が 1 台");
        assert_eq!(m.phase(), Phase::Idle);
        let none = Snap::default();
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(
            m.step(&facts(DIFFICULTY, none)),
            Act::Pass,
            "押された台が無い"
        );
        // 6を経ずに7を見た（起動の途中・別の経路）。
        let mut m = Machine::new();
        assert_eq!(m.step(&facts(DIFFICULTY, two_devices())), Act::Pass);
        // 画面6以外（キャラ選択の8など）から7へ戻っても聞かない。
        let mut m = Machine::new();
        m.observe(CHARA);
        assert_eq!(m.step(&facts(DIFFICULTY, two_devices())), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        // 人間 対 人間でない選択（CPU戦: select_kindが0でない）。
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        let cpu = Scene::Menu {
            screen: 7,
            select_kind: 1,
            accepting: true,
        };
        assert_eq!(m.step(&facts(cpu, two_devices())), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        // iniが無効・ネットのセッションがあるときは聞かない。
        let mut off = facts(DIFFICULTY, two_devices());
        off.enabled = false;
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(m.step(&off), Act::Pass);
        let mut net = facts(DIFFICULTY, two_devices());
        net.net = true;
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(m.step(&net), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        // 台が2台あれば聞く。
        assert_eq!(asking().phase(), FIRST_ASK);
    }

    #[test]
    fn screen_six_is_only_observed_and_the_deciding_press_is_not_taken_as_1p() {
        let mut m = Machine::new();
        // 画面6の間はstepが呼ばれず、observeだけが毎tick呼ばれる。選択に使ったZは押した瞬間の記録に残っている。
        m.observe(MATCH_MODE);
        let mut snap = pressed(0, keys::Z);
        snap.pressed[1] = keys::Z;
        m.observe(DIFFICULTY);
        assert_eq!(
            m.step(&facts(DIFFICULTY, snap)),
            Act::Hold(Some(Prompt::P1(0)))
        );
        assert_eq!(m.phase(), FIRST_ASK, "決めた Z が 1P の 1 段目を満たした");
        // 次のtickから押した瞬間を数える。
        m.step(&facts(DIFFICULTY, pressed(1, keys::Z)));
        assert_eq!(
            m.phase(),
            Phase::Ask {
                stage: 1,
                p1: 0b10,
                p2: 0
            }
        );
    }

    #[test]
    fn arming_is_true_only_on_the_tick_the_screen_goes_from_6_to_7() {
        let mut m = Machine::new();
        assert!(!m.arming(DIFFICULTY), "画面 6 を見る前");
        m.observe(MATCH_MODE);
        assert!(!m.arming(MATCH_MODE));
        m.observe(DIFFICULTY);
        assert!(m.arming(DIFFICULTY));
        assert!(!m.arming(CHARA), "画面 7 以外");
        let cpu = Scene::Menu {
            screen: 7,
            select_kind: 1,
            accepting: true,
        };
        assert!(!m.arming(cpu), "人間 対 人間でない選択");
        m.step(&facts(DIFFICULTY, two_devices()));
        assert!(!m.arming(DIFFICULTY), "聞き始めた後");
        let mut m = Machine::new();
        m.observe(CHARA);
        m.observe(DIFFICULTY);
        assert!(!m.arming(DIFFICULTY), "8 から 7 へ戻った");
    }

    #[test]
    fn a_second_keyboard_pressed_later_on_screen_7_does_not_start_asking() {
        let one = Snap {
            seen: 0b1,
            ..Snap::default()
        };
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(m.step(&facts(DIFFICULTY, one)), Act::Pass);
        assert_eq!(m.step(&facts(DIFFICULTY, two_devices())), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
    }

    #[test]
    fn seven_keys_are_asked_for_1p_then_2p_in_order_and_the_prompt_follows() {
        assert_eq!(
            ASK_KEYS,
            [
                keys::Z,
                keys::X,
                keys::SHIFT,
                keys::UP,
                keys::DOWN,
                keys::LEFT,
                keys::RIGHT
            ]
        );
        let mut m = asking();
        for (second, dev) in [(false, 0usize), (true, 1)] {
            for (i, &word) in ASK_KEYS.iter().enumerate() {
                let i = i as u8;
                let here = Prompt::P1(i);
                let here = if second { Prompt::P2(i) } else { here };
                // 違うキーでは進まない。
                for &wrong in ASK_KEYS.iter().filter(|&&k| k != word) {
                    assert_eq!(
                        m.step(&facts(DIFFICULTY, pressed(dev, wrong))),
                        Act::Hold(Some(here)),
                        "{i} 段目に {wrong:#06x} で進んだ"
                    );
                }
                let next = match (second, i) {
                    (false, 6) => Prompt::P2(0),
                    (true, 6) => Prompt::Release,
                    (false, _) => Prompt::P1(i + 1),
                    (true, _) => Prompt::P2(i + 1),
                };
                assert_eq!(
                    m.step(&facts(DIFFICULTY, pressed(dev, word))),
                    Act::Hold(Some(next)),
                    "{i} 段目"
                );
            }
        }
        assert_eq!(m.phase(), Phase::Release { p1: 0b01, p2: 0b10 });
    }

    #[test]
    fn only_a_new_press_counts_in_a_stage() {
        let mut m = asking();
        // 押したままのキー（押した瞬間の記録が無い）ではどのステップも進まない。
        for _ in 0..2 {
            for &word in &ASK_KEYS {
                let held = Snap {
                    words: [word, 0, 0, 0, 0, 0, 0, 0],
                    down: 1,
                    ..two_devices()
                };
                m.step(&facts(DIFFICULTY, held));
                assert_eq!(m.phase(), FIRST_ASK);
            }
        }
        // 1Pを台0、2Pの1〜3ステップ目を台1で済ませる。
        let mut m = asking();
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[0]);
        }
        for _ in 0..3 {
            pass_stage(&mut m, &[1]);
        }
        assert_eq!(
            m.phase(),
            Phase::Ask {
                stage: 10,
                p1: 0b01,
                p2: 0b10
            }
        );
        // 2Pの4ステップ目（↑）。台1が押しっぱなし。
        let mut held = pressed_by(&[1], keys::UP);
        held.pressed = [0; 8];
        m.step(&facts(DIFFICULTY, held));
        assert!(matches!(m.phase(), Phase::Ask { stage: 10, .. }));
        // 次のステップ（↓）のキーが前のステップの途中から押されたままでも新しく押されるまで進まない。
        pass_stage(&mut m, &[1]);
        assert!(matches!(m.phase(), Phase::Ask { stage: 11, .. }));
        let mut held = pressed_by(&[1], keys::DOWN);
        held.pressed = [0; 8];
        m.step(&facts(DIFFICULTY, held));
        assert!(matches!(m.phase(), Phase::Ask { stage: 11, .. }));
    }

    #[test]
    fn the_2p_stages_ignore_1p_keyboards_and_a_press_in_the_same_tick_goes_to_both_sets() {
        let mut m = asking();
        // 同じtickに台0・2がZを押したらどちらも1P。
        pass_stage(&mut m, &[0, 2]);
        for _ in 1..ASK_STEPS {
            pass_stage(&mut m, &[0]);
        }
        assert!(matches!(
            m.phase(),
            Phase::Ask {
                stage: 7,
                p1: 0b101,
                p2: 0
            }
        ));
        // 2Pのステップで1Pの台（0・2）が押しても待ち続ける。
        for _ in 0..3 {
            for &word in &ASK_KEYS {
                m.step(&facts(DIFFICULTY, pressed_by(&[0, 2], word)));
            }
        }
        assert!(matches!(m.phase(), Phase::Ask { stage: 7, .. }));
        // 1Pの台と別の台が同じtickに押したら別の台だけが2Pに入る。
        pass_stage(&mut m, &[0, 1]);
        assert!(matches!(
            m.phase(),
            Phase::Ask {
                stage: 8,
                p1: 0b101,
                p2: 0b010
            }
        ));
    }

    #[test]
    fn one_person_can_use_two_keyboards_and_the_feed_is_the_or_of_their_words() {
        let mut m = asking();
        // 1P: Z・X・Shiftは台0、↑↓←→ は台2。2P: 台1。
        for i in 0..ASK_STEPS {
            pass_stage(&mut m, &[if i < 3 { 0 } else { 2 }]);
        }
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[1]);
        }
        assert_eq!(
            m.phase(),
            Phase::Release {
                p1: 0b101,
                p2: 0b010
            }
        );
        m.step(&facts(DIFFICULTY, two_devices()));
        assert_eq!(
            m.phase(),
            Phase::Active {
                p1: 0b101,
                p2: 0b010
            }
        );
        let mut snap = two_devices();
        snap.words = [
            keys::Z | keys::SHIFT,
            keys::X | keys::RIGHT,
            keys::LEFT,
            keys::UP,
            0,
            0,
            0,
            0,
        ];
        snap.down = 0b1111;
        assert_eq!(
            m.step(&facts(DIFFICULTY, snap)),
            Act::Feed {
                w1: keys::Z | keys::SHIFT | keys::LEFT,
                w2: keys::X | keys::RIGHT
            },
            "台 3 は集合の外なので読まない"
        );
        // 割り当てた台のどれかが押されている間は離すのを待つ。
        let mut m = asking();
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[0, 2]);
        }
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[1]);
        }
        let mut still = two_devices();
        still.words[2] = keys::LEFT;
        still.down = 0b100;
        assert_eq!(
            m.step(&facts(DIFFICULTY, still)),
            Act::Hold(Some(Prompt::Release))
        );
    }

    #[test]
    fn after_the_last_stage_zero_is_held_until_the_assigned_words_are_released() {
        let mut m = answered(1, 0);
        // 2台のどちらかの入力語のキーが押されている間は0のまま、離すように案内する。
        for dev in [0, 1] {
            let mut still = two_devices();
            still.words[dev] = keys::LEFT;
            still.down = 1 << dev;
            assert_eq!(
                m.step(&facts(DIFFICULTY, still)),
                Act::Hold(Some(Prompt::Release)),
                "台 {dev}"
            );
            assert!(matches!(m.phase(), Phase::Release { .. }));
        }
        // 入力語に無いキー（P / Homeなど）や関係の無い3台目の押下は待たない。
        let other = Snap {
            down: 0b111,
            ..two_devices()
        };
        assert_eq!(m.step(&facts(DIFFICULTY, other)), Act::Hold(None));
        assert_eq!(m.phase(), Phase::Active { p1: 0b10, p2: 0b01 });
    }

    #[test]
    fn active_feeds_each_keyboards_word_and_zero_when_the_gate_is_closed() {
        let mut m = active(0, 1);
        let mut snap = two_devices();
        snap.words = [
            keys::Z | keys::LEFT,
            keys::X | keys::RIGHT,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        snap.down = 0b11;
        assert_eq!(
            m.step(&facts(DIFFICULTY, snap)),
            Act::Feed {
                w1: keys::Z | keys::LEFT,
                w2: keys::X | keys::RIGHT
            }
        );
        let mut closed = facts(DIFFICULTY, snap);
        closed.gate = false;
        assert_eq!(m.step(&closed), Act::Feed { w1: 0, w2: 0 });
        // 試合の場面・読み込みでも続く。再戦で画面8に戻っても聞き直さない。
        assert!(matches!(
            m.step(&facts(Scene::Match, snap)),
            Act::Feed { .. }
        ));
        assert!(matches!(
            m.step(&facts(Scene::Other, snap)),
            Act::Feed { .. }
        ));
        assert!(matches!(m.step(&facts(CHARA, snap)), Act::Feed { .. }));
        assert!(m.is_active());
    }

    #[test]
    fn leaving_the_human_selection_screens_ends_the_split_even_when_only_observed() {
        // 本番は画面6ではobserveしか呼ばれない。難易度でキャンセルして人間 対 人間を選び直したらまた聞く。
        for scene in [
            MATCH_MODE,
            Scene::Menu {
                screen: 1,
                select_kind: 0,
                accepting: true,
            },
            Scene::Menu {
                screen: 8,
                select_kind: 1,
                accepting: true,
            },
        ] {
            let mut m = active(0, 1);
            m.observe(scene);
            assert_eq!(m.phase(), Phase::Idle, "{scene:?}");
        }
        let mut m = active(0, 1);
        m.observe(MATCH_MODE);
        assert_eq!(
            m.step(&facts(DIFFICULTY, two_devices())),
            Act::Hold(Some(Prompt::P1(0))),
            "選び直したらまた聞く"
        );
        // 使わないことが決定済みでも選び直したらまた聞く。
        let mut m = asking();
        m.step(&facts(DIFFICULTY, pressed(0, keys::ESC)));
        assert!(matches!(m.phase(), Phase::Drain { .. }));
        m.observe(MATCH_MODE);
        assert_eq!(m.phase(), Phase::Idle);
        let mut m = asking();
        m.step(&facts(DIFFICULTY, pressed(0, keys::ESC)));
        m.step(&game_only(DIFFICULTY, 0));
        assert_eq!(m.phase(), Phase::Skipped);
        m.observe(MATCH_MODE);
        assert_eq!(m.phase(), Phase::Idle);
        assert_eq!(
            m.step(&facts(DIFFICULTY, two_devices())),
            Act::Hold(Some(Prompt::P1(0)))
        );
    }

    #[test]
    fn the_pass_through_after_a_match_keeps_the_split_and_reports_nothing() {
        // 試合の後の通り抜け（menu.csvの1793〜1847: 画面1 → 6 → 7 → 8。1・6は受け付けない状態。7・8ではstepが呼ばれる）。
        let pass = |screen| Scene::Menu {
            screen,
            select_kind: 0,
            accepting: false,
        };
        let mut m = active(0, 1);
        m.observe(pass(1));
        m.observe(pass(1));
        m.observe(pass(6));
        assert_eq!(m.take_event(), None);
        for screen in [7, 8] {
            let scene = pass(screen);
            m.observe(scene);
            assert_eq!(
                step_event(&mut m, &facts(scene, two_devices())),
                None,
                "画面 {screen}"
            );
        }
        assert!(m.is_active(), "通り抜けで割り当てが解けた");
    }

    #[test]
    fn a_press_accepted_outside_the_selection_ends_the_split_and_reports_it_once() {
        // タイトル（1）・モードの選択（6）・保存の画面（15）で受け付けたら割り当てを解いてIdleになる。イベントは1回だけ。
        for screen in [1, 6, 15] {
            let scene = Scene::Menu {
                screen,
                select_kind: 0,
                accepting: true,
            };
            let mut m = active(0, 1);
            m.observe(scene);
            assert_eq!(m.phase(), Phase::Idle, "画面 {screen}");
            assert_eq!(m.take_event(), Some(Event::Released), "画面 {screen}");
            assert_eq!(m.take_event(), None, "画面 {screen}");
        }
        // 割り当てが決定する前（聞いている間）は割り当てを解いたイベントを出さない。
        let mut m = asking();
        assert_eq!(m.take_event(), Some(Event::Asking { devices: 2 }));
        m.observe(MATCH_MODE);
        assert_eq!(m.phase(), Phase::Idle);
        assert_eq!(m.take_event(), None);
        // 離すのを待つ間は割り当ての途中なのでイベントを出す。
        let mut m = answered(0, 1);
        m.observe(MATCH_MODE);
        assert_eq!(m.take_event(), Some(Event::Released));
    }

    #[test]
    fn a_replay_or_a_net_session_ends_the_split_and_reports_the_release() {
        let mut m = active(0, 1);
        let mut replay = facts(Scene::Match, two_devices());
        replay.replay = true;
        assert_eq!(step_event(&mut m, &replay), Some(Event::Released));
        let mut m = active(0, 1);
        let mut net = facts(Scene::Match, two_devices());
        net.net = true;
        assert_eq!(step_event(&mut m, &net), Some(Event::Released));
        // 割り当てが無い（Idle）のまま抜けても出さない。
        let mut m = Machine::new();
        assert_eq!(step_event(&mut m, &net), None);
    }

    #[test]
    fn replay_net_and_a_disabled_setting_end_the_split() {
        let mut m = active(0, 1);
        let mut replay = facts(Scene::Match, two_devices());
        replay.replay = true;
        assert_eq!(m.step(&replay), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        let mut m = active(0, 1);
        let mut net = facts(Scene::Match, two_devices());
        net.net = true;
        assert_eq!(m.step(&net), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
        let mut m = active(0, 1);
        let mut off = facts(Scene::Match, two_devices());
        off.enabled = false;
        assert_eq!(m.step(&off), Act::Pass);
        assert_eq!(m.phase(), Phase::Idle);
    }

    #[test]
    fn esc_on_any_keyboard_holds_zero_until_everything_is_released_then_hands_the_input_back() {
        for stage in [0usize, 1, 9] {
            let mut m = asking();
            for i in 0..stage {
                pass_stage(&mut m, &[usize::from(i >= usize::from(ASK_STEPS))]);
            }
            assert!(matches!(m.phase(), Phase::Ask { .. }));
            // Escを押したtickから、押している間はゲームには0を渡す（難易度の画面を戻させない）。
            assert_eq!(
                m.step(&facts(DIFFICULTY, pressed(1, keys::ESC))),
                Act::Hold(None),
                "{stage} 段を済ませた後"
            );
            assert!(matches!(m.phase(), Phase::Drain { .. }));
            let mut held = facts(DIFFICULTY, pressed(1, keys::ESC));
            held.snap.pressed = [0; 8];
            held.game_word = keys::ESC;
            for _ in 0..5 {
                assert_eq!(m.step(&held), Act::Hold(None));
            }
            // 表だけ空でもゲームの入力（Esc）が残っている間は空の入力を渡す。
            assert_eq!(m.step(&game_only(DIFFICULTY, keys::ESC)), Act::Hold(None));
            // ゲームの入力だけ空でも表にキーが残っている間は空の入力を渡す。
            let mut table_only = facts(DIFFICULTY, pressed(1, keys::ESC));
            table_only.snap.pressed = [0; 8];
            assert_eq!(m.step(&table_only), Act::Hold(None));
            // 両方が空になったtickから元の入力。
            assert_eq!(m.step(&game_only(DIFFICULTY, 0)), Act::Pass);
            assert_eq!(m.phase(), Phase::Skipped);
            // 使わない間は何もせず、Zを押しても聞き直さない。
            assert_eq!(m.step(&facts(DIFFICULTY, pressed(0, keys::Z))), Act::Pass);
            assert_eq!(m.phase(), Phase::Skipped);
        }
    }

    #[test]
    fn a_stuck_key_cannot_hold_the_screen_forever() {
        let mut m = asking();
        m.step(&facts(DIFFICULTY, pressed(0, keys::ESC)));
        let mut n = 0;
        while matches!(m.phase(), Phase::Drain { .. }) {
            assert!(n <= DRAIN_MAX_TICKS as usize + 1, "離すのを待ち続けた");
            m.step(&game_only(DIFFICULTY, keys::Z));
            n += 1;
        }
        assert_eq!(m.phase(), Phase::Skipped);
        assert_eq!(n, DRAIN_MAX_TICKS as usize + 1);
    }

    #[test]
    fn a_pad_press_is_not_leaked_and_skips_only_when_no_keyboard_key_is_down_for_three_ticks() {
        let mut m = asking();
        // キーボードのキーが押されている間はゲームの入力があってもパッドではない。
        for _ in 0..10 {
            let mut f = facts(DIFFICULTY, pressed(0, keys::X));
            f.game_word = keys::Z;
            assert_eq!(m.step(&f), Act::Hold(Some(Prompt::P1(0))));
        }
        assert_eq!(m.phase(), FIRST_ASK);
        // キーを離した直後の2 tickのずれは許す。
        m.step(&game_only(DIFFICULTY, keys::Z));
        m.step(&game_only(DIFFICULTY, keys::Z));
        assert_eq!(m.phase(), FIRST_ASK);
        m.step(&game_only(DIFFICULTY, 0));
        m.step(&game_only(DIFFICULTY, keys::Z));
        m.step(&game_only(DIFFICULTY, keys::Z));
        assert_eq!(m.phase(), FIRST_ASK, "途中で切れた数え直し");
        // 3 tick目からパッドの決定がゲームの難易度を決めないよう0を渡す。
        assert_eq!(
            m.step(&game_only(DIFFICULTY, keys::Z)),
            Act::Hold(None),
            "パッドの決定が漏れた"
        );
        assert!(matches!(m.phase(), Phase::Drain { .. }));
        assert_eq!(
            m.step(&game_only(DIFFICULTY, keys::Z)),
            Act::Hold(None),
            "押している間は流す"
        );
        assert_eq!(m.step(&game_only(DIFFICULTY, 0)), Act::Pass);
        assert_eq!(m.phase(), Phase::Skipped);
        // 2Pを聞いている間も同じ。P / Homeだけのゲームの入力はパッドではない。
        let mut m = asking();
        for _ in 0..ASK_STEPS {
            pass_stage(&mut m, &[0]);
        }
        for _ in 0..5 {
            m.step(&game_only(DIFFICULTY, keys::SHOT));
        }
        assert!(matches!(m.phase(), Phase::Ask { stage: 7, .. }));
        for _ in 0..3 {
            m.step(&game_only(DIFFICULTY, keys::Z));
        }
        assert!(matches!(m.phase(), Phase::Drain { .. }));
    }

    #[test]
    fn while_split_only_the_two_assigned_keyboards_are_read() {
        // 5秒（60 fpsで300 tick）ずつ。割り当てた2台は何も押していない。
        const LONG: usize = 300;
        let mut m = active(0, 1);
        // パッドの入力語（ゲームの入力）。表にキーが無い。
        for _ in 0..LONG {
            assert_eq!(
                m.step(&game_only(DIFFICULTY, keys::Z | keys::LEFT)),
                Act::Feed { w1: 0, w2: 0 },
                "パッドの語を読んだ"
            );
        }
        assert!(m.is_active());
        // 割り当てていない3台目（抜き差しで番号が替わった台など）が押され続けても戻らない。
        let mut third = facts(DIFFICULTY, two_devices());
        third.snap.seen = 0b111;
        third.snap.words[2] = keys::X;
        third.snap.pressed[2] = keys::X;
        third.snap.down = 0b100;
        for _ in 0..LONG {
            assert_eq!(m.step(&third), Act::Feed { w1: 0, w2: 0 }, "3 台目で戻った");
        }
        assert!(m.is_active());
        // 割り当てた2台の入力語だけをfeedする（3台目の入力語は混ざらない）。
        let mut snap = two_devices();
        snap.words = [keys::Z, keys::X, keys::UP, 0, 0, 0, 0, 0];
        snap.down = 0b111;
        assert_eq!(
            m.step(&facts(DIFFICULTY, snap)),
            Act::Feed {
                w1: keys::Z,
                w2: keys::X
            }
        );
        assert!(m.is_active());
    }

    #[test]
    fn while_split_a_silent_assigned_pair_reports_no_event() {
        let mut m = active(0, 1);
        for _ in 0..300 {
            assert_eq!(step_event(&mut m, &game_only(DIFFICULTY, keys::Z)), None);
        }
        assert!(m.is_active());
    }

    #[test]
    fn polling_is_asked_only_while_asking_or_draining() {
        let mut m = asking();
        assert!(m.polls() && m.is_engaged());
        m.step(&facts(DIFFICULTY, pressed(0, keys::ESC)));
        assert!(m.polls(), "離すのを待つ間");
        m.step(&game_only(DIFFICULTY, 0));
        assert!(!m.polls() && !m.is_engaged(), "使わない間");
        assert!(!active(0, 1).polls(), "分けている間は読まない");
        assert!(!Machine::new().polls());
        assert!(!Machine::new().wants_full(Scene::Match));
        assert!(Machine::new().wants_full(DIFFICULTY));
        assert!(asking().wants_full(Scene::Match));
        assert_eq!(
            asking().step(&facts(Scene::Other, two_devices())),
            Act::Hold(Some(Prompt::P1(0))),
            "読み込みの tick は状態を動かさない"
        );
    }

    /// イベントを1つずつ取る（`take_event`は取ったら消える。同じtickに2つ起きないこと）。
    fn step_event(m: &mut Machine, f: &Facts) -> Option<Event> {
        m.step(f);
        let ev = m.take_event();
        assert_eq!(m.take_event(), None, "同じ tick に 2 つ目の出来事");
        ev
    }

    #[test]
    fn each_transition_reports_one_event_and_a_single_keyboard_reports_none() {
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(
            step_event(&mut m, &facts(DIFFICULTY, two_devices())),
            Some(Event::Asking { devices: 2 })
        );
        // 聞いている間の押下（最後のステップの前）はイベントではない。
        assert_eq!(
            step_event(&mut m, &facts(DIFFICULTY, pressed(0, keys::X))),
            None
        );
        for i in 0..2 * ASK_STEPS - 1 {
            let dev = usize::from(i >= ASK_STEPS);
            let word = ASK_KEYS[usize::from(i % ASK_STEPS)];
            assert_eq!(
                step_event(&mut m, &facts(DIFFICULTY, pressed(dev, word))),
                None,
                "{i} 段目"
            );
        }
        assert_eq!(
            step_event(&mut m, &facts(DIFFICULTY, pressed(1, keys::RIGHT))),
            Some(Event::Assigned { p1: 0b01, p2: 0b10 })
        );
        // 割り当てに使ったキーが離れるまでの間は出ない。
        assert_eq!(step_event(&mut m, &facts(DIFFICULTY, two_devices())), None);
        assert!(m.is_active());
        // 台が1台以下なら聞かないのでイベントも無い。
        let one = Snap {
            seen: 0b1,
            ..Snap::default()
        };
        let mut m = Machine::new();
        m.observe(MATCH_MODE);
        assert_eq!(step_event(&mut m, &facts(DIFFICULTY, one)), None);
    }

    #[test]
    fn an_untaken_event_does_not_survive_into_the_next_step() {
        let mut m = asking();
        m.step(&facts(DIFFICULTY, pressed(0, keys::ESC)));
        // 取らずに次のtickへ。Escのイベントは残らない（次のtickのぶんだけ出す）。
        m.step(&game_only(DIFFICULTY, 0));
        assert_eq!(m.take_event(), None);
    }

    #[test]
    fn esc_and_a_pad_press_report_different_skips() {
        let mut m = asking();
        assert_eq!(
            step_event(&mut m, &facts(DIFFICULTY, pressed(0, keys::ESC))),
            Some(Event::SkippedByEsc)
        );
        // Escを押している間のtickはイベントを重ねない。
        assert_eq!(
            step_event(&mut m, &facts(DIFFICULTY, pressed(0, keys::ESC))),
            None
        );
        let mut m = asking();
        assert_eq!(step_event(&mut m, &game_only(DIFFICULTY, keys::Z)), None);
        assert_eq!(step_event(&mut m, &game_only(DIFFICULTY, keys::Z)), None);
        assert_eq!(
            step_event(&mut m, &game_only(DIFFICULTY, keys::Z)),
            Some(Event::SkippedByPad),
            "パッドの決定"
        );
    }

    #[test]
    fn the_lines_name_the_table_numbers_and_mark_a_null_device() {
        let t = DeviceTable::new();
        assert_eq!(t.slot_of(0xA0), Some(0));
        assert_eq!(t.slot_of(0xB0), Some(1));
        assert!(!t.is_null_device(0) && !t.is_null_device(1));
        assert!(!t.is_null_device(9), "表の外の番号");
        assert_eq!(
            event_line(Event::Asking { devices: 2 }, &t),
            "キーボードの分割: 人間 対 人間なので 1P・2P のキーボードを聞きます（キーが押されたキーボード 2 台）。"
        );
        assert_eq!(
            event_line(Event::Assigned { p1: 0b01, p2: 0b10 }, &t),
            "キーボードの分割: 1P は台 0、2P は台 1 に割り当てました。"
        );
        assert_eq!(
            event_line(
                Event::Assigned {
                    p1: 0b101,
                    p2: 0b010
                },
                &t
            ),
            "キーボードの分割: 1P は台 0・台 2、2P は台 1 に割り当てました。"
        );
        // hDevice 0の台（2番目）に印を付ける。
        let n = DeviceTable::new();
        assert_eq!(n.slot_of(0xA0), Some(0));
        assert_eq!(n.slot_of(0), Some(1));
        assert!(n.is_null_device(1) && !n.is_null_device(0));
        assert_eq!(
            event_line(Event::Assigned { p1: 0b01, p2: 0b10 }, &n),
            "キーボードの分割: 1P は台 0、2P は台 1（hDevice 0）に割り当てました。"
        );
        assert_eq!(
            event_line(Event::Released, &t),
            "キーボードの分割: 人間 対 人間を抜けたので、割り当てを解きました。"
        );
        assert_eq!(
            event_line(Event::SkippedByEsc, &t),
            "キーボードの分割: Esc が押されたので、今回は分けません。"
        );
        assert_eq!(
            event_line(Event::SkippedByPad, &t),
            "キーボードの分割: パッドが押されたので、今回は分けません。"
        );
    }

    #[test]
    fn the_ini_defaults_to_on_and_reads_zero_and_one_only() {
        let read = |t: &str| from_ini(&ini::parse(t).unwrap());
        assert_eq!(read(""), Ok(true));
        assert_eq!(read("[input]\n"), Ok(true));
        assert_eq!(read("[input]\nsplit_keyboards = 1\n"), Ok(true));
        assert_eq!(read("[input]\nsplit_keyboards = 0\n"), Ok(false));
        for bad in ["2", "on", "", "-1"] {
            let e = read(&format!("[input]\nsplit_keyboards = {bad}\n")).unwrap_err();
            assert!(e.contains("split_keyboards"), "{bad}: {e}");
        }
        let e = read("[input]\nsplit_keyboard = 0\n").unwrap_err();
        assert!(e.contains("split_keyboard"), "{e}");
    }
}
