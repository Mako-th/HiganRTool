//! 台本を 1 tick 進める判断（pure）: 画面の組（`Obs`）を読み、`Engine::step` を呼び、feed する tick では InputManager 3 本の
//! 前後をコピーして feed を 3 回呼ぶ。
//!
//! ゲームのメモリの読み・ゲームの関数の呼び出しは `TickIo` の向こうに置く（本物は `script_win::RealTickIo`、検査では偽物）。
//! ここはゲームのスレッドで走るので、確保・ファイル・ロック待ち・`unwrap` を置かない（`FeedRecord` はスタックに置く）。
#![forbid(unsafe_code)]

use higanr_core::input_model::SEND_MASK;
use higanr_core::script::{Engine, Obs, Out, Status};

use crate::feeds::{FeedRecord, IM_BYTES};
use crate::netplay::SCENE_MATCH;
use crate::th09_addr::{
    GLOBAL_STATE, GLOBAL_STATE_DEMO, GUI_PTR, GUI_TALK, INPUT_MANAGERS, LOADING, MAIN_MENU_PTR,
    MATCH_END_MENU, MENU_CURSOR, MENU_SCREEN_ID, MENU_SUBSTATE, MENU_TRANSITIONING, PAUSE,
    SUP_SCENE_CUR, SUPERVISOR,
};

/// MainMenu の欄を使う場面（場面 1 = タイトルとメニュー）。
pub const SCENE_MENU: u32 = 1;

/// ゲームのスレッドが読むアドレス（花映塚では `th09_addr` の値。検査では検査の静的変数のアドレス）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GameAddrs {
    pub supervisor: u32,
    pub pause: u32,
    pub loading: u32,
    pub global_state: u32,
    /// MainMenu へのポインタの置き場（落ちない読み方でだけたどる）。
    pub main_menu_ptr: u32,
    pub input_managers: [u32; 3],
    /// マッチの終わりのメニューの印（1 バイトの欄。4 バイトで読んで下位 1 バイトだけ見る）。
    pub match_end_menu: u32,
    /// Gui へのポインタの置き場（会話の状態を落ちない読み方でたどる）。
    pub gui_ptr: u32,
}

impl GameAddrs {
    pub fn th09() -> Self {
        GameAddrs {
            supervisor: SUPERVISOR,
            pause: PAUSE,
            loading: LOADING,
            global_state: GLOBAL_STATE,
            main_menu_ptr: MAIN_MENU_PTR,
            input_managers: INPUT_MANAGERS,
            match_end_menu: MATCH_END_MENU,
            gui_ptr: GUI_PTR,
        }
    }

    pub fn direct_ranges(&self) -> [(u32, u32); 10] {
        let im = IM_BYTES as u32;
        [
            (self.supervisor.wrapping_add(SUP_SCENE_CUR), 4),
            (self.pause, 4),
            (self.loading, 4),
            (self.global_state, 4),
            (self.main_menu_ptr, 4),
            (self.input_managers[0], im),
            (self.input_managers[1], im),
            (self.input_managers[2], im),
            (self.match_end_menu, 4),
            (self.gui_ptr, 4),
        ]
    }
}

/// Gui の会話の状態の値が「会話が走っている」か。ゲーム自身の判定 `0x00404980` と同じ: 0 以上と −2 が会話中、−1 が会話なし。
/// 実測で見えた値は −1（会話なし）と 0・200・201（会話中）だけで、−2 の意味は未確認だが、ゲームの定義に合わせて「会話中」に数える。
pub fn talk_state_running(v: u32) -> bool {
    let s = v as i32;
    s >= 0 || s == -2
}

/// Gui の会話が走っているか（`talk=` 条件の値）。Gui のポインタが 0・読めない、会話の欄が読めないときは `None`（読めないまま
/// 押すより、台本が止まって理由が出る方が安全）。場面 2 でないときも `None`（場面 1 で古い Gui を読んで真にしないため）。
fn read_talk(io: &mut dyn TickIo, a: &GameAddrs, scene: u32) -> Option<bool> {
    if scene != SCENE_MATCH {
        return None;
    }
    io.safe_u32(a.gui_ptr)
        .filter(|&p| p != 0)
        .and_then(|p| p.checked_add(GUI_TALK))
        .and_then(|addr| io.safe_u32(addr))
        .map(talk_state_running)
}

/// マッチの終わりのメニューが押せるか。印 `[0x4A7ECE]`（1 バイト）が 0 でなく、しかも会話が走っていない。会話が読めないときは
/// 押せない側に倒す（読めない間に押して会話を送らないため）。
///
/// 印を 1 にするのは `0x4170A8` と `0x4178BE` の 2 か所だけ。後者は直前の `0x417854`（`mov 0xe94c(%esi),%eax; test; jge 0x417bf1`）で、
/// 会話が走っている（0 以上）間は通らない。前者の経路（`0x41705B`〜`0x4170A8`）には会話の確認を読み取れなかった（見つからなかった
/// だけで、無いとは確かめていない）。印は決定の 20 フレーム後に `0x435A4A` / `0x43560C` が 0 に戻す。会話の状態を重ねて見るのは、
/// 順序を逆アセンブルだけに頼らないため（撮影ではメニューと会話のウィンドウが同時に写っていたが、会話のウィンドウは会話が終わった後も描かれたままかもしれない。推定）。メニューは出てから 4 フレーム入力を見ないので、真になってから 10 tick 以上待って押すこと。
fn end_menu_pressable(menu_flag: bool, talk: Option<bool>) -> bool {
    menu_flag && talk == Some(false)
}

/// ゲームのスレッドが外から得るもの。本物は `script_win::RealTickIo`。どれも確保しない・パニックしない。
pub trait TickIo {
    /// ゲームの .data の 4 バイトを直に読む（設置の前に読めると確かめたアドレスだけ）。
    fn data_u32(&mut self, addr: u32) -> u32;
    /// 落ちない読み方（自分のプロセスへの `ReadProcessMemory`）で 4 バイト。読めなければ `None`。
    fn safe_u32(&mut self, addr: u32) -> Option<u32>;
    fn read_im(&mut self, addr: u32, out: &mut [u8; IM_BYTES]);
    /// ローカル入力を 1 回読む（戻り値の下位 16 ビットが語）。台本だけの実行（ネットワークなし）は `0x0042BE40`（InputManager に書かない）。
    /// ネットワークのセッションを置けた実行で poll の入口の照合が合っていれば、素の poll `0x0042B850` を席 0 で呼ぶ（InputManager[0] は呼ぶ前後で
    /// コピーして戻す）。
    fn local_input(&mut self) -> u32;
    /// feed `0x0042ACE0`（ecx = 番号、edx = 語）を 1 回呼ぶ。
    fn feed(&mut self, index: u32, word: u32);
}

/// 今の画面の組を読む。MainMenu の欄は落ちない読み方で読み、今の場面が 1 のときだけ使う。MainMenu のポインタが 0・読めない欄は
/// `None`。
pub fn read_obs(io: &mut dyn TickIo, a: &GameAddrs) -> Obs {
    let scene = io.data_u32(a.supervisor.wrapping_add(SUP_SCENE_CUR));
    // 一時停止は 1 バイトの欄（`movb` で 1 → 2 に書かれる）。4 バイトで読んだ隣の 3 バイトは見ない。
    let pause = io.data_u32(a.pause) & 0xFF != 0;
    let loading = io.data_u32(a.loading) != 0;
    let demo = io.data_u32(a.global_state) & GLOBAL_STATE_DEMO != 0;
    let mut menu = [None; 4];
    if scene == SCENE_MENU
        && let Some(p) = io.safe_u32(a.main_menu_ptr).filter(|&p| p != 0)
    {
        for (m, off) in menu.iter_mut().zip([
            MENU_SCREEN_ID,
            MENU_CURSOR,
            MENU_SUBSTATE,
            MENU_TRANSITIONING,
        ]) {
            *m = p.checked_add(off).and_then(|addr| io.safe_u32(addr));
        }
    }
    let [screen, cursor, sub, trans] = menu;
    let menu_flag = io.data_u32(a.match_end_menu) & 0xFF != 0;
    let talk = read_talk(io, a, scene);
    Obs {
        scene,
        screen,
        cursor,
        sub,
        trans,
        pause,
        loading,
        demo,
        end_menu: end_menu_pressable(menu_flag, talk),
        menu_flag,
        talk,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickOut {
    /// 入口へ返す値: 0 = 元の飛び先（poll_all）へ、1 = poll_all を呼ばずに戻る。
    pub ret: u32,
    /// 撮影の理由のビット（`higanr_core::script` の `START`〜`WAIT`。0 なら撮らない）と段階。
    pub shot: u8,
    pub step: u16,
    /// 台本が終わった / 止まった（以後この tick から元の poll_all へ戻す。`Out::feed` が `None`）。
    pub stop: bool,
    /// feed した tick の記録（feed しなかった tick は `None`）。
    pub record: Option<FeedRecord>,
    pub status: Status,
}

/// 台本を 1 tick 進める。順: 前の 3 本をコピーする → ローカル入力 → feed 3 回（`0` に p1・`1` に p2・`2` に p1|p2）→ 後の 3 本をコピーする。
/// 元の poll_all は呼ばずに戻る。
#[cfg(test)] // 本物の経路は `script_win::script_tick` が `advance` と `commit` に割って呼ぶ（検査が「割っても 1 つの `tick` と同じ」を見る）
pub fn tick(engine: &mut Engine, io: &mut dyn TickIo, a: &GameAddrs, k: u32) -> TickOut {
    let stepped = advance(engine, io, a);
    commit(io, a, k, &stepped)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stepped {
    pub out: Out,
    pub status: Status,
}

impl Stepped {
    pub fn word(&self) -> Option<u16> {
        self.out.feed.map(|(p1, p2)| (p1 | p2) & SEND_MASK)
    }
}

/// 台本を 1 tick 進めるだけ（画面を読んで `Engine::step`。feed はしない）。観戦者の経路が、繋がっている間の台本の Esc を読むために
/// 使う: 観戦中の tick は台本の語をゲームへ入れないが、台本は 1 tick に 1 回ずつ進めたい。`Local` を返す tick は、同じ結果を
/// `commit` に渡して feed する（二重に進めない）。
pub fn advance(engine: &mut Engine, io: &mut dyn TickIo, a: &GameAddrs) -> Stepped {
    let obs = read_obs(io, a);
    let out = engine.step(&obs);
    Stepped {
        out,
        status: engine.status(),
    }
}

/// `advance` の結果を feed する（`tick` の後半。台本を進めない）。feed しない結果（終わった・止まった）は元の飛び先へ戻す。
pub fn commit(io: &mut dyn TickIo, a: &GameAddrs, k: u32, stepped: &Stepped) -> TickOut {
    let (out, status) = (stepped.out, stepped.status);
    let Some((p1, p2)) = out.feed else {
        return TickOut {
            ret: 0,
            shot: out.shot,
            step: out.step,
            stop: true,
            record: None,
            status,
        };
    };
    // 語は念のためここでも `SEND_MASK` の中へ（台本の読み込みで断ってあるが、feed する直前の守り）。
    let (p1, p2) = (p1 & SEND_MASK, p2 & SEND_MASK);
    let mut rec = FeedRecord {
        k,
        step: out.step,
        p1,
        p2,
        local: 0,
        before: [[0u8; IM_BYTES]; 3],
        after: [[0u8; IM_BYTES]; 3],
    };
    for (addr, b) in a.input_managers.iter().zip(rec.before.iter_mut()) {
        io.read_im(*addr, b);
    }
    rec.local = io.local_input() as u16;
    for (i, w) in rec.words().iter().enumerate() {
        io.feed(i as u32, *w as u32);
    }
    for (addr, b) in a.input_managers.iter().zip(rec.after.iter_mut()) {
        io.read_im(*addr, b);
    }
    TickOut {
        ret: 1,
        shot: out.shot,
        step: out.step,
        stop: false,
        record: Some(rec),
        status,
    }
}

/// 進み具合の状態（ゲームのスレッドが原子変数へ出し、読み取りのスレッドが読む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunState {
    #[default]
    Idle,
    Running,
    Done,
    Aborted,
    Panicked,
}

impl RunState {
    pub fn code(self) -> u32 {
        match self {
            RunState::Idle => 0,
            RunState::Running => 1,
            RunState::Done => 2,
            RunState::Aborted => 3,
            RunState::Panicked => 4,
        }
    }

    pub fn from_code(c: u32) -> Self {
        match c {
            1 => RunState::Running,
            2 => RunState::Done,
            3 => RunState::Aborted,
            4 => RunState::Panicked,
            _ => RunState::Idle,
        }
    }

    pub fn of(s: Status) -> Self {
        match s {
            Status::Running { .. } => RunState::Running,
            Status::Done => RunState::Done,
            Status::Aborted { .. } => RunState::Aborted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub state: RunState,
    /// 今の段階（0 始まり）。止まったときは止まった段階。
    pub step: u32,
    /// 止まった段階の行の番号（止まっていなければ 0）。
    pub line: u32,
    /// 台本の入口を通った tick の数と、そのうち feed した tick の数。
    pub ticks: u32,
    pub fed: u32,
    /// 入口の分かれ道（`SCRIPT_ON`）がいま開いているか。
    pub on: bool,
}

impl Progress {
    pub fn finished(&self) -> bool {
        !matches!(self.state, RunState::Running) || !self.on
    }

    pub fn text(&self, steps: Option<usize>) -> String {
        let n = steps.map_or(String::new(), |n| format!("（全 {n} 段）"));
        let body = match self.state {
            RunState::Idle => "台本は置いていない".to_string(),
            RunState::Running => format!("まだ走っている（段 {}{n}）", self.step),
            RunState::Done => format!("最後の段まで終わった{n}"),
            RunState::Aborted => format!(
                "行 {} の段 {} で条件が満たされずに止まった{n}",
                self.line, self.step
            ),
            RunState::Panicked => format!(
                "ゲームのスレッドでパニックを受けて止めた（段 {}{n}）",
                self.step
            ),
        };
        format!(
            "{body}。入口を通った tick {} / feed した tick {} / 分かれ道は{}",
            self.ticks,
            self.fed,
            if self.on {
                "開いている"
            } else {
                "閉じた"
            }
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::feeds::{FeedChecker, im_from_bytes, im_to_bytes};
    use higanr_core::input_model::{InputState, keys};
    use higanr_core::script::{END, POST, PRE, START, parse};
    use std::collections::HashMap;

    pub(crate) const ADDRS: GameAddrs = GameAddrs {
        supervisor: 0x0010_0000,
        pause: 0x0020_0000,
        loading: 0x0020_0004,
        global_state: 0x0020_0008,
        main_menu_ptr: 0x0020_000C,
        input_managers: [0x0030_0000, 0x0030_008E, 0x0030_011C],
        match_end_menu: 0x0020_0010,
        gui_ptr: 0x0020_0014,
    };
    const MENU: u32 = 0x0400_0000;
    const GUI: u32 = 0x0500_0000;

    /// 偽のゲーム: .data の値・落ちない読み（無いアドレスは読めない）・InputManager 3 本（feed は模型で動く）と、呼ばれた順の記録。
    pub(crate) struct FakeIo {
        pub data: HashMap<u32, u32>,
        pub safe: HashMap<u32, u32>,
        pub ims: [InputState; 3],
        pub local: u32,
        pub log: Vec<String>,
    }

    impl FakeIo {
        pub(crate) fn new(scene: u32) -> Self {
            let mut data = HashMap::new();
            data.insert(ADDRS.supervisor + SUP_SCENE_CUR, scene);
            data.insert(ADDRS.pause, 0);
            data.insert(ADDRS.loading, 0);
            data.insert(ADDRS.global_state, 0);
            data.insert(ADDRS.match_end_menu, 0);
            FakeIo {
                data,
                safe: HashMap::new(),
                ims: [InputState::new(); 3],
                local: 0xABCD_0024,
                log: Vec::new(),
            }
        }
    }

    impl TickIo for FakeIo {
        fn data_u32(&mut self, addr: u32) -> u32 {
            *self
                .data
                .get(&addr)
                .expect("偽物: 直に読むアドレスが置かれていない")
        }
        fn safe_u32(&mut self, addr: u32) -> Option<u32> {
            self.safe.get(&addr).copied()
        }
        fn read_im(&mut self, addr: u32, out: &mut [u8; IM_BYTES]) {
            let i = ADDRS
                .input_managers
                .iter()
                .position(|&a| a == addr)
                .expect("偽物: InputManager でないアドレス");
            self.log.push(format!("im{i}"));
            *out = im_to_bytes(&self.ims[i]);
        }
        fn local_input(&mut self) -> u32 {
            self.log.push("local".to_string());
            self.local
        }
        fn feed(&mut self, index: u32, word: u32) {
            self.log.push(format!("feed {index} {word:#x}"));
            self.ims[index as usize].feed(word as u16);
        }
    }

    #[test]
    fn read_obs_follows_the_design() {
        let mut io = FakeIo::new(1);
        io.safe.insert(ADDRS.main_menu_ptr, MENU);
        io.safe.insert(MENU + MENU_SCREEN_ID, 8);
        io.safe.insert(MENU + MENU_CURSOR, 3);
        io.safe.insert(MENU + MENU_SUBSTATE, 1);
        io.data.insert(ADDRS.pause, 7);
        io.data.insert(ADDRS.loading, 0x100);
        io.data.insert(ADDRS.global_state, 0x0C); // bit1 は 0（bit2・bit3）
        let o = read_obs(&mut io, &ADDRS);
        assert_eq!(
            (o.scene, o.screen, o.cursor, o.sub, o.trans),
            (1, Some(8), Some(3), Some(1), None)
        );
        assert_eq!((o.pause, o.loading, o.demo), (true, true, false));
        io.data.insert(ADDRS.global_state, 0x0E);
        assert!(read_obs(&mut io, &ADDRS).demo);
        io.data.insert(ADDRS.pause, 0xFFFF_FF00);
        assert!(!read_obs(&mut io, &ADDRS).pause);
        io.data.insert(ADDRS.pause, 0x0000_0002);
        assert!(read_obs(&mut io, &ADDRS).pause);
        io.safe.insert(ADDRS.main_menu_ptr, 0);
        io.safe.insert(MENU_SCREEN_ID, 99);
        assert_eq!(read_obs(&mut io, &ADDRS).screen, None);
        io.safe.insert(ADDRS.main_menu_ptr, MENU);
        io.data.insert(ADDRS.supervisor + SUP_SCENE_CUR, 2);
        let o = read_obs(&mut io, &ADDRS);
        assert_eq!((o.scene, o.screen, o.cursor), (2, None, None));
    }

    #[test]
    fn end_menu_is_pressable_only_when_flag_is_set_and_no_talk_runs() {
        let mut io = FakeIo::new(2);
        let talk = |io: &mut FakeIo, v: Option<u32>| {
            io.safe.remove(&(GUI + GUI_TALK));
            if let Some(v) = v {
                io.safe.insert(GUI + GUI_TALK, v);
            }
        };
        io.safe.insert(ADDRS.gui_ptr, GUI);
        talk(&mut io, Some(u32::MAX));
        assert!(!read_obs(&mut io, &ADDRS).end_menu);
        io.data.insert(ADDRS.match_end_menu, 1);
        assert!(read_obs(&mut io, &ADDRS).end_menu);
        for v in [0, 200, 201, u32::MAX - 1] {
            talk(&mut io, Some(v));
            assert!(!read_obs(&mut io, &ADDRS).end_menu, "会話の状態 {v}");
        }
        talk(&mut io, None);
        assert!(!read_obs(&mut io, &ADDRS).end_menu, "会話の欄が読めない");
        talk(&mut io, Some(u32::MAX));
        io.safe.insert(ADDRS.gui_ptr, 0);
        assert!(!read_obs(&mut io, &ADDRS).end_menu, "Gui が 0");
        io.safe.remove(&ADDRS.gui_ptr);
        assert!(!read_obs(&mut io, &ADDRS).end_menu, "Gui が読めない");
        io.safe.insert(ADDRS.gui_ptr, GUI);
        assert!(read_obs(&mut io, &ADDRS).end_menu, "戻したのに真にならない");
        io.data.insert(ADDRS.match_end_menu, 0xFFFF_FF00);
        assert!(!read_obs(&mut io, &ADDRS).end_menu);
        io.data.insert(ADDRS.match_end_menu, 0x0000_0001);
        assert!(read_obs(&mut io, &ADDRS).end_menu);
    }

    #[test]
    fn obs_talk_is_none_when_unreadable_and_menu_flag_ignores_talk() {
        let mut io = FakeIo::new(2);
        io.safe.insert(ADDRS.gui_ptr, GUI);
        for (v, want) in [
            (0, true),
            (200, true),
            (201, true),
            (u32::MAX - 1, true),
            (u32::MAX, false),
        ] {
            io.safe.insert(GUI + GUI_TALK, v);
            assert_eq!(read_obs(&mut io, &ADDRS).talk, Some(want), "会話の状態 {v}");
        }
        io.safe.insert(GUI + GUI_TALK, 200);
        io.data.insert(ADDRS.match_end_menu, 1);
        let o = read_obs(&mut io, &ADDRS);
        assert_eq!((o.menu_flag, o.end_menu), (true, false));
        io.data.insert(ADDRS.match_end_menu, 0xFFFF_FF00);
        assert!(!read_obs(&mut io, &ADDRS).menu_flag);
        io.safe.remove(&(GUI + GUI_TALK));
        assert_eq!(read_obs(&mut io, &ADDRS).talk, None, "会話の欄が読めない");
        io.safe.insert(GUI + GUI_TALK, u32::MAX);
        io.safe.insert(ADDRS.gui_ptr, 0);
        assert_eq!(read_obs(&mut io, &ADDRS).talk, None, "Gui が 0");
        io.safe.remove(&ADDRS.gui_ptr);
        assert_eq!(read_obs(&mut io, &ADDRS).talk, None, "Gui が読めない");
        io.safe.insert(ADDRS.gui_ptr, GUI);
        assert_eq!(read_obs(&mut io, &ADDRS).talk, Some(false));
        io.data.insert(ADDRS.supervisor + SUP_SCENE_CUR, 1);
        assert_eq!(read_obs(&mut io, &ADDRS).talk, None, "場面 1");
    }

    #[test]
    fn a_script_sends_talk_with_z_then_skips_and_presses_the_menu() {
        let script = parse(
            "scene=2 menuflag=1 wait=1 note=up\n\
             scene=2 talk=1 optional=1 limit=5 wait=2 p1=Z shot=none\n\
             scene=2 talk=1 optional=1 limit=5 wait=2 p1=Z shot=none\n\
             scene=2 talk=1 optional=1 limit=5 wait=2 p1=Z shot=none\n\
             scene=2 endmenu=1 wait=1 p1=DOWN\n",
        )
        .unwrap();
        let mut e = Engine::new(script);
        let mut io = FakeIo::new(2);
        io.safe.insert(ADDRS.gui_ptr, GUI);
        io.safe.insert(GUI + GUI_TALK, 200);
        let mut pressed: Vec<u16> = Vec::new();
        let run = |e: &mut Engine, io: &mut FakeIo, k: u32| -> u16 {
            let o = tick(e, io, &ADDRS, k);
            assert!(!o.stop, "k={k}: 台本が止まった {:?}", o.status);
            o.record.unwrap().p1
        };
        for k in 0..4 {
            pressed.push(run(&mut e, &mut io, k));
        }
        assert!(pressed.iter().all(|&p| p == 0));
        io.data.insert(ADDRS.match_end_menu, 1);
        for k in 4..9 {
            pressed.push(run(&mut e, &mut io, k));
        }
        pressed.retain(|&p| p != 0);
        assert_eq!(pressed, [keys::Z]);
        // 会話が終わる。次の Z の段階は押し始める tick の再確認で崩れているので押さない。その次の Z の段階は出ない会話を `limit` で飛ばす。
        // 会話が終わって初めて（`endmenu=1`）下を押す。
        io.safe.insert(GUI + GUI_TALK, u32::MAX);
        for k in 9..18 {
            pressed.push(run(&mut e, &mut io, k));
        }
        pressed.retain(|&p| p != 0);
        assert_eq!(
            pressed,
            [keys::Z, keys::DOWN],
            "会話の後は Z を押さず、下だけ"
        );
    }

    #[test]
    fn a_script_presses_down_only_when_the_end_menu_is_pressable() {
        let script = parse("scene=2 endmenu=1 wait=1 p1=DOWN\n").unwrap();
        let mut e = Engine::new(script);
        let mut io = FakeIo::new(2);
        io.safe.insert(ADDRS.gui_ptr, GUI);
        io.safe.insert(GUI + GUI_TALK, 0); // 会話が走っている
        io.data.insert(ADDRS.match_end_menu, 1);
        for k in 0..5 {
            let o = tick(&mut e, &mut io, &ADDRS, k);
            assert_eq!((o.record.unwrap().p1, o.stop), (0, false), "会話の間 k={k}");
        }
        io.safe.insert(GUI + GUI_TALK, u32::MAX); // 会話が終わった
        let o = tick(&mut e, &mut io, &ADDRS, 5); // wait=1: 満たした tick が PRE
        assert_eq!(o.record.unwrap().p1, 0);
        let o = tick(&mut e, &mut io, &ADDRS, 6);
        assert_eq!(o.record.unwrap().p1, keys::DOWN);
    }

    #[test]
    fn tick_feeds_in_order_and_records_before_and_after() {
        let script = parse("scene=2 wait=1 p1=Z+LEFT p2=RIGHT hold=2\n").unwrap();
        let mut e = Engine::new(script);
        let mut io = FakeIo::new(2);
        let o = tick(&mut e, &mut io, &ADDRS, 100);
        assert_eq!((o.ret, o.shot, o.step, o.stop), (1, START | PRE, 0, false));
        let r = o.record.unwrap();
        assert_eq!((r.k, r.p1, r.p2, r.local), (100, 0, 0, 0x0024));
        io.log.clear();
        let o = tick(&mut e, &mut io, &ADDRS, 101);
        assert_eq!((o.ret, o.shot), (1, 0));
        assert_eq!(
            io.log,
            vec![
                "im0",
                "im1",
                "im2",
                "local",
                "feed 0 0x41",
                "feed 1 0x80",
                "feed 2 0xc1",
                "im0",
                "im1",
                "im2"
            ]
        );
        let r = o.record.unwrap();
        assert_eq!((r.p1, r.p2), (keys::Z | keys::LEFT, keys::RIGHT));
        assert_eq!(im_from_bytes(&r.before[2]).cur, 0);
        assert_eq!(im_from_bytes(&r.after[2]).cur, 0xC1);
        assert_eq!(im_from_bytes(&r.after[0]).pressed, keys::Z | keys::LEFT);
        let o = tick(&mut e, &mut io, &ADDRS, 102);
        assert_eq!((o.ret, o.shot), (1, POST));
        io.log.clear();
        let o = tick(&mut e, &mut io, &ADDRS, 103);
        assert_eq!((o.ret, o.shot, o.stop), (0, END, true));
        assert!(o.record.is_none());
        assert!(
            !io.log.iter().any(|l| l.starts_with("feed")),
            "{:?}",
            io.log
        );
        assert_eq!(o.status, Status::Done);
    }

    #[test]
    fn advance_then_commit_equals_tick_and_advance_alone_feeds_nothing() {
        let text = "scene=2 wait=1 p1=Z+LEFT p2=RIGHT+ESC hold=2\n";
        let (mut e1, mut io1) = (Engine::new(parse(text).unwrap()), FakeIo::new(2));
        let (mut e2, mut io2) = (Engine::new(parse(text).unwrap()), FakeIo::new(2));
        for k in 0..6 {
            let a = tick(&mut e1, &mut io1, &ADDRS, k);
            let n = io2.log.len();
            let s = advance(&mut e2, &mut io2, &ADDRS);
            assert!(
                !io2.log[n..]
                    .iter()
                    .any(|l| l.starts_with("feed") || l == "local"),
                "k={k}: advance が feed かローカル入力を呼んだ {:?}",
                &io2.log[n..]
            );
            let b = commit(&mut io2, &ADDRS, k, &s);
            assert_eq!(a, b, "k={k}");
            assert_eq!(io1.log, io2.log, "k={k}: 偽のゲームへの呼び出し");
            assert_eq!(s.word(), a.record.map(|r| (r.p1 | r.p2) & SEND_MASK));
        }
        assert_eq!(e1.status(), e2.status());
        assert_eq!(e2.status(), Status::Done);
    }

    #[test]
    fn records_from_a_model_game_check_clean() {
        let script = parse("scene=2 wait=2 p1=RIGHT hold=30\nscene=2 wait=3 p2=ESC\n").unwrap();
        let mut e = Engine::new(script);
        let mut io = FakeIo::new(2);
        let mut c = FeedChecker::default();
        let mut k = 0;
        let mut repeat_seen = false;
        loop {
            let o = tick(&mut e, &mut io, &ADDRS, k);
            if o.stop {
                break;
            }
            let r = o.record.unwrap();
            repeat_seen |= im_from_bytes(&r.after[0]).repeat & keys::RIGHT != 0;
            assert_eq!(c.check(&r).im, [0; 3], "k={k}");
            k += 1;
        }
        assert!(repeat_seen, "標本にリピートの立つ件が無い");
        assert_eq!(c.records, k as u64);
        assert_eq!((c.cont_breaks, c.gaps), (0, 0));
        assert_eq!(c.cont_pairs, k as u64 - 1);
    }

    #[test]
    fn progress_states() {
        for s in [
            RunState::Idle,
            RunState::Running,
            RunState::Done,
            RunState::Aborted,
            RunState::Panicked,
        ] {
            assert_eq!(RunState::from_code(s.code()), s);
        }
        let running = Progress {
            state: RunState::Running,
            on: true,
            ..Default::default()
        };
        assert!(!running.finished());
        assert!(
            Progress {
                on: false,
                ..running
            }
            .finished()
        );
        assert!(
            Progress {
                state: RunState::Aborted,
                ..running
            }
            .finished()
        );
        assert_eq!(
            RunState::of(Status::Aborted { step: 1, line: 2 }),
            RunState::Aborted
        );
    }
}
