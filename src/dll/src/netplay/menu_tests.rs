//! ネットの選択の組み上がりの検査（pure）: 選択の画面を持つ偽のゲーム 2 つを、本物の入力の地点の判断（`NetPlay`）と仮のネットワーク（`FakeNet`）で繋ぎ、
//! 両者が同じ画面を同じ順に通って、同じ値でマッチに入るかを見る。
//!
//! 偽のゲーム `MenuGame` は、ゲームの決まり（選択の 3 画面の表）を `netplay.rs` / `net_select.rs` /
//! `menu_model.rs` とは別に転記したもの: feed された 3 本の押した瞬間を読み、substate 0 の間（入る演出）は押下を読まず、難易度 7・キャラ 8・
//! マップ 16 の画面を移る。読まれなかった画面を移る押下は数える（押下を入れる tick を dll が誤ると、ここが 0 でなくなる）。
//!
//! 台本の無いネットワーク実行: `MenuGame` の台本（`Act` の列）は dll の台本ではなく人の手の語の列の真似で、`Game::local_input`（本物ではローカル入力。
//! 素の poll `0x0042B850` の席 0、照合が合わなければ `0x0042BE40`）から入る。押し続け（`hold` > 1）は画面が移っても押したまま、
//! マッチの中の語は `match_words`、再戦は `rematches`、CHECK のゲームの値は `load_draws`（frame 0 の抽選回数だけが両者で違う形）で作る。
#![forbid(unsafe_code)]

use std::net::SocketAddr;

use higanr_core::handshake::{self, Name};
use higanr_core::input_model::keys;
use higanr_core::link;
use higanr_core::lockstep::Role;
use higanr_core::selection::Agreed;
use higanr_core::wire::{Seat, Select, StepKind};

use super::fake::{FRAME_US, FakeNet, local_word};
use super::{Action, BadValue, CheckSource, Config, Game, Net, NetPlay, Phase, WAIT_BUDGET_US};
use crate::net_portrait::{Followed, Plan, PortraitEnv, plan};
use crate::net_select::{MenuView, seat_index};
use crate::th09_addr::{MENU_CHARA_CURSOR, MENU_CURSOR};

const DECIDE: u16 = 0x1001;
const CANCEL: u16 = 0x000A;
/// キャラのカーソル → キャラ（表 `0x4A1E2C`）。
pub(super) const CHARA_TABLE: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 14, 15, 7, 8, 10, 11, 9, 12, 13];

/// 入力の地点の判断を偽のゲームから同じ入口で呼ぶ（対戦の `NetPlay` と、観戦者の `spectate_watch::WatchPlay`）。
pub(crate) trait Driver {
    fn on_input<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action;
    fn on_match_start<G: Game>(&mut self, game: &mut G);
    fn phase(&self) -> Phase;
    fn last_timing(&self) -> super::FrameTiming;
    /// 本物の呼び手（`script_win::ui_hud_tick`）が入力の地点の後に毎 tick 呼ぶ `NetPlay::hud_facts`。観戦者のセッションには無いので `None`。
    fn hud_probe(&mut self) -> Option<crate::net_ui::HudFacts> {
        None
    }
}

impl Driver for NetPlay {
    fn on_input<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        NetPlay::on_input(self, game, net)
    }
    fn on_match_start<G: Game>(&mut self, game: &mut G) {
        NetPlay::on_match_start(self, game)
    }
    fn phase(&self) -> Phase {
        NetPlay::phase(self)
    }
    fn last_timing(&self) -> super::FrameTiming {
        NetPlay::last_timing(self)
    }
    fn hud_probe(&mut self) -> Option<crate::net_ui::HudFacts> {
        NetPlay::hud_facts(self)
    }
}

/// 台本の 1 項目: `round` マッチを終えた後で、画面が `screen` になってから `wait` tick 待ち、`word` を `hold` tick 押す。
#[derive(Debug, Clone, Copy)]
pub struct Act {
    pub round: u32,
    pub screen: u32,
    pub wait: u32,
    pub word: u16,
    pub hold: u32,
}

pub(super) const fn act(round: u32, screen: u32, wait: u32, word: u16, hold: u32) -> Act {
    Act {
        round,
        screen,
        wait,
        word,
        hold,
    }
}

/// 台本とは別に、人が押す語（Esc など。観戦者の検査用）。`when` が初めて真になった tick から `ticks` tick のあいだ `word` を押す
/// （その tick の自分の語に OR する）。
pub struct Press {
    pub when: Box<dyn Fn(&MenuGame) -> bool>,
    pub word: u16,
    pub ticks: u32,
    left: u32,
    started: bool,
}

impl Press {
    pub fn new(when: impl Fn(&MenuGame) -> bool + 'static, word: u16, ticks: u32) -> Press {
        Press {
            when: Box::new(when),
            word,
            ticks,
            left: 0,
            started: false,
        }
    }
}

pub struct MenuGame {
    seat: usize,
    pub cur: u32,
    pub next: u32,
    pub screen: u32,
    pub sub: u32,
    anim: u32,
    /// 入る演出の長さ（花映塚は 8）。検査が片方だけ長くして、相手の押下が演出の間に届く形を作る。
    pub anim_len: u32,
    pub cursor: u32,
    pub chara_cursor: [u32; 2],
    pub decided: [u32; 2],
    pub map_list: [u8; 16],
    /// `[0x4B3538]`（難易度のカーソルのバックアップ）。
    pub saved_diff: u32,
    pub difficulty: u32,
    pub map: i32,
    pub chara: [u8; 2],
    pub color: [u8; 2],
    prev: [u16; 3],
    fed: [Option<u16>; 3],
    plan: Vec<Act>,
    plan_i: usize,
    plan_t: u32,
    /// 通った画面の列（画面が変わるたび）。
    pub screens: Vec<u32>,
    /// 読まれなかった画面を移る押下（入る演出の間・移らない画面で）。
    pub ignored_presses: u32,
    pub agreed: Vec<Agreed>,
    load_left: u32,
    pub load_ticks: u32,
    in_match: bool,
    pub match_frames: u32,
    frames: u32,
    pub matches: u32,
    pub max_matches: u32,
    pub played: Vec<(u16, u32, u16, u16)>,
    /// マッチのフレームを使った tick に feed された 3 本（`played` と同じ並び。合成の語の入れ違いを見る）。
    pub fed_played: Vec<[u16; 3]>,
    /// 選択の画面で feed された 3 本（`Action::Menu` の tick。席の入れ違いを見る）。
    pub fed_menu: Vec<[u16; 3]>,
    /// マッチから戻った: 次のメニューの tick で押下なしに 7 → 8。
    pub pending_auto8: bool,
    /// ゲームが決めた値（止める地点で SELECT が書き直す前）: 両席が揃ったときの両席のキャラ、決めたマップ。
    pub decided_chars: Vec<[u8; 2]>,
    pub decided_maps: Vec<i32>,
    /// `on_input` の中の割り当て・解放の回数（ゲームのスレッドの規則: 0 であること）。
    pub on_input_allocs: u64,
    /// `Some` なら、毎 tick 入力の地点の後に `hud_facts` を呼び、その戻りを順に溜める。
    pub hud_log: Option<Vec<Option<crate::net_ui::HudFacts>>>,
    /// この tick は dll がネットの選択で feed した。
    menu_fed: bool,
    // ---- 人の手の真似 ----
    /// 押し続けている語と、残りの tick（画面が移っても押したまま）。
    hold_word: u16,
    hold_left: u32,
    /// マッチの中の語の種（`Some` なら `local_word(種, n)`。None ならマッチの中は 0）。
    pub match_words: Option<u64>,
    match_word_n: u64,
    /// 1 回の選択の後に続ける再戦の数（マッチの最後のフレームの更新が次の要求 10 を書く）。
    pub rematches: u32,
    rematch_left: u32,
    /// マッチの「乱数」（使った語で進む。マッチの始めに同じ値へ戻す）と、frame 0 の抽選回数（読み込みの引き。両者で違ってよい値）。
    rng: u64,
    pub load_draws: u32,
    // ---- タイトルのデモ ----
    /// 画面 6 で押さずに読む状態の tick がこの数に達したら、選択を通らずに次の要求 2（デモ）を書く（1 回だけ）。花映塚は放っておくと約 27 秒でデモに入る。
    pub demo_after: Option<u32>,
    demo_idle: u32,
    demo_req: bool,
    /// デモの中なら、残りのフレーム（読み込みの後に数える）。
    demo_left: Option<u32>,
    demo_load: u32,
    /// デモの長さ（読み込みの後の更新の数）。
    pub demo_len: u32,
    /// デモの更新が走った数（描画だけのフレームなら走らない）と、終えたデモの数。
    pub demo_frames: u32,
    pub demos: u32,
    /// デモの要求からデモの終わりまでで、判断が `Local` 以外を返した tick（ネットワークが触った）。
    pub demo_not_local: u32,
    /// デモの要求を書いた tick のネットワークの段階（`Selecting` なら繋がった後にデモが来た —— 直す前に止まった期間）。
    pub demo_req_phase: Option<super::Phase>,
    // ---- 繋がる前の決定 ----
    /// `Some((画面, 語, 周期))`: その画面にいる間、台本とは別に `周期` tick ごとに語を押す（人が決定を叩き続ける形。押した
    /// 画面が移れば止まり、台本の続きが動く）。
    pub spam: Option<(u32, u16, u32)>,
    spam_t: u32,
    // ---- 立ち絵の追随 ----
    /// dll がキャラのカーソルを違う値へ書いた回（席, 前, 後）と、そのとき立ち絵の 3 本を呼ばせた回。ゲームの移動の経路は
    /// 「カーソルが動く → 立ち絵と席の目印」なので、両方の列は同じでなければならない。
    pub cursor_jumps: Vec<(usize, u32, u32)>,
    pub portraits: Vec<(usize, u32, u32)>,
    // ---- 合わせの観測 ----
    /// 何 tick 目か（`tick` の呼び出しの数）と、今の画面に入ってからの tick。
    pub tick_no: u32,
    screen_age: u32,
    /// 画面に入ってから `SNAP_AGE` tick 目の (画面, 難易度・マップの値, キャラのカーソル)。決定より前の静かな間に取る
    /// ので、決定の tick のカーソル合わせでは作れない（合わせの検査用）。
    pub snaps: Vec<(u32, u32, [u32; 2])>,
    /// 同じ画面の中でカーソルが変わった (tick, 画面, 難易度・マップの値, キャラのカーソル)。
    pub changes: Vec<(u32, u32, u32, [u32; 2])>,
    last_view: (u32, u32, [u32; 2]),
    // ---- 観戦者の観測 ----
    /// `Game::local_input` が呼ばれた回数（観戦者は繋がった後に読まない —— 自分の入力が届かない証拠）。
    pub local_input_calls: u32,
    /// `Game::peek_local_input`（観戦者の Esc の読み）が呼ばれた回数。
    pub peek_calls: u32,
    /// 人が押す語（`Press`）。
    pub presses: Vec<Press>,
    /// dll の台本の語（`Press` の形で書く。空なら台本は無い）。台本は繋がっている間も 1 tick に 1 回進む（`peek_local_input` が進める）。
    /// 読みに足されるのは台本の語の ESC だけで、`Action::Local` を返した tick の feed は台本の語を全部入れる。
    pub script_presses: Vec<Press>,
    /// 台本を進めた回数（`peek_local_input` は呼ぶたびに進める。`local_input` と `Local` の feed は、その tick にもう進めていれば進めない）。
    pub script_steps: u32,
    script_word: Option<u16>,
    /// 台本の語が 0 でなかった tick の数と、そのうち `Action::Local` でなかった tick の数（後者の tick でも台本の語は feed されない）。
    pub script_nonzero_ticks: u32,
    pub script_nonzero_non_local: u32,
    /// この tick の自分の語（台本と `presses`）。同じ tick に何度読んでも同じ値を返す（本物のデバイスは 1 tick の間同じ状態）。
    tick_word: Option<u16>,
    /// `Action::Local` の feed に Esc が含まれていた tick の番号（`tick_no`。自分の Esc がゲームへ届いた証拠）。
    pub esc_local: Vec<u32>,
    /// `Game::apply_watched` で書かれた両席の SELECT の組（止める地点ごと）。`on_input` の中から push するので先に広げておく。
    pub watched: Vec<[Select; 2]>,
    /// `Game::write_charge` に渡された（1P, 2P）。
    pub charges: Vec<(u8, u8)>,
    /// 台本の段階（`digit=x` / `tab=1`）の代わり: 次の tick の `Game::digit_script` が 1 回だけ返す「押した瞬間」の集合。
    pub digit_script: Option<u16>,
    /// 検査の Tab の予定（画面, その画面に入ってからの tick）。`Game::digit_script` がその tick に 1 回だけ Tab を返す（返したら予定から外す）。
    pub tab_at: Vec<(u32, u32)>,
    /// 本物のキーの読み（`Game::digit_keys`）の代わり: 数字キー 0〜9・Tab・Ctrl・Alt/Win の今の押下の集合（`digit_keys` の bit の並び）。`None` は読んではいけない間。
    pub digit_held: Option<u16>,
    /// キーボードの Ctrl が押されている（`Game::keyboard_ctrl_down`）。
    pub ctrl_down: bool,
    /// 台本の付いている実行（`Game::scripted_run`）。
    pub scripted: bool,
}

/// 合わせの検査で、画面に入ってから何 tick 目にカーソルを控えるか（入る演出 9 tick と、片道 40 ms の合わせ + 押し直しより後）。
const SNAP_AGE: u32 = 120;

impl MenuGame {
    pub fn new(seat: usize, plan: Vec<Act>, unlocked_maps: &[u8]) -> Self {
        // 一覧: 表 `0x4A1D9C` の並びのうち、解放済み（と 14・15）だけを前から詰める。
        let order = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 9, 12, 13, 14, 15];
        let mut map_list = [0xFFu8; 16];
        let mut n = 0;
        for v in order {
            if v >= 14 || unlocked_maps.contains(&v) {
                map_list[n] = v;
                n += 1;
            }
        }
        let mut g = MenuGame {
            seat,
            cur: 1,
            next: 0,
            screen: 6,
            sub: 1,
            anim: 0,
            anim_len: 8,
            cursor: 0,
            chara_cursor: [3, 8],
            decided: [0, 0],
            map_list,
            saved_diff: 1,
            difficulty: 0,
            map: 0,
            chara: [0, 0],
            color: [0, 0],
            prev: [0; 3],
            fed: [None; 3],
            plan,
            plan_i: 0,
            plan_t: 0,
            screens: Vec::new(),
            ignored_presses: 0,
            // `on_input` の中（止める地点の `apply_agreed`）から push するので、先に広げておく。
            agreed: Vec::with_capacity(16),
            load_left: 0,
            load_ticks: 4,
            in_match: false,
            match_frames: 20,
            frames: 0,
            matches: 0,
            max_matches: 1,
            played: Vec::new(),
            fed_played: Vec::new(),
            fed_menu: Vec::new(),
            pending_auto8: false,
            decided_chars: Vec::new(),
            decided_maps: Vec::new(),
            on_input_allocs: 0,
            hud_log: None,
            menu_fed: false,
            hold_word: 0,
            hold_left: 0,
            match_words: None,
            match_word_n: 0,
            rematches: 0,
            rematch_left: 0,
            rng: 0,
            load_draws: 0,
            demo_after: None,
            demo_idle: 0,
            demo_req: false,
            demo_left: None,
            demo_load: 0,
            demo_len: 30,
            demo_frames: 0,
            demos: 0,
            demo_not_local: 0,
            demo_req_phase: None,
            spam: None,
            spam_t: 0,
            cursor_jumps: Vec::with_capacity(64),
            portraits: Vec::with_capacity(64),
            tick_no: 0,
            screen_age: 0,
            snaps: Vec::new(),
            changes: Vec::new(),
            last_view: (0, 0, [0; 2]),
            local_input_calls: 0,
            peek_calls: 0,
            presses: Vec::new(),
            script_presses: Vec::new(),
            script_steps: 0,
            script_word: None,
            script_nonzero_ticks: 0,
            script_nonzero_non_local: 0,
            tick_word: None,
            esc_local: Vec::new(),
            watched: Vec::with_capacity(16),
            charges: Vec::with_capacity(16),
            digit_script: None,
            tab_at: Vec::new(),
            digit_held: None,
            ctrl_down: false,
            scripted: false,
        };
        // 画面 6（人対人を選ぶ画面）から始める。台本が 6 で決めて 7 へ入る（繋がった後に選択の画面へ入る形）。
        g.set_screen(6, true);
        g
    }

    /// 合わせの検査が見る値: 難易度（7）はカーソル、マップ（16）はカーソルが指す一覧の値（添字は解放状況で両者ごとに違う）、
    /// キャラ（8）は 0。
    fn cursor_item(&self) -> u32 {
        match self.screen {
            7 => self.cursor,
            16 => self
                .map_list
                .get(self.cursor as usize)
                .map_or(0xFFFF, |&x| u32::from(x)),
            _ => 0,
        }
    }

    fn observe(&mut self) {
        self.tick_no += 1;
        // キャラのカーソルはキャラ選択（8）の間だけ見る（ほかの画面では、まだ合わせ前の最初のカーソルのままでよい）。
        let chara = if self.screen == 8 {
            self.chara_cursor
        } else {
            [0; 2]
        };
        let view = (self.screen, self.cursor_item(), chara);
        if view.0 == self.last_view.0 {
            self.screen_age += 1;
            if (view.1, view.2) != (self.last_view.1, self.last_view.2) {
                self.changes.push((self.tick_no, view.0, view.1, view.2));
            }
        } else {
            self.screen_age = 0;
        }
        self.last_view = view;
        if self.screen_age == SNAP_AGE && matches!(self.screen, 7 | 8 | 16) {
            self.snaps.push(view);
        }
    }

    fn set_screen(&mut self, s: u32, direct: bool) {
        self.screen = s;
        self.sub = 0;
        self.anim = 0;
        self.screens.push(s);
        match s {
            7 => {
                self.cursor = if self.saved_diff >= 4 {
                    1
                } else {
                    self.saved_diff
                };
            }
            8 => self.decided = [0, 0],
            16 => {
                let n = self.map_list.iter().filter(|&&x| x != 0xFF).count() as u32;
                self.cursor = n - 2;
            }
            _ => {}
        }
        if direct {
            self.anim_step();
        }
    }

    fn anim_step(&mut self) {
        if self.anim == self.anim_len {
            self.sub = 1;
        }
        self.anim += 1;
    }

    fn plan_word(&mut self) -> u16 {
        if let Some((screen, word, period)) = self.spam
            && self.cur == 1
            && self.screen == screen
        {
            self.spam_t += 1;
            return if self.spam_t.is_multiple_of(period) {
                word
            } else {
                0
            };
        }
        if self.hold_left > 0 {
            self.hold_left -= 1;
            return self.hold_word;
        }
        let Some(a) = self.plan.get(self.plan_i).copied() else {
            return 0;
        };
        if self.cur != 1 || self.screen != a.screen || self.matches != a.round {
            self.plan_t = 0;
            return 0;
        }
        self.plan_t += 1;
        if self.plan_t <= a.wait {
            return 0;
        }
        // 押し始めの tick で次の項目へ（押下で画面が移ると、次の tick はもう違う画面）。残りは押し続ける。
        self.plan_i += 1;
        self.plan_t = 0;
        self.hold_word = a.word;
        self.hold_left = a.hold.saturating_sub(1);
        a.word
    }

    /// この tick の自分の語（台本の語に `presses` を OR したもの）。1 tick に 1 回だけ台本を進める。
    fn word_once(&mut self) -> u16 {
        if let Some(w) = self.tick_word {
            return w;
        }
        let plan = self.plan_word();
        let w = plan | self.eval_presses(false);
        self.tick_word = Some(w);
        w
    }

    fn eval_presses(&mut self, script: bool) -> u16 {
        let mut ps = std::mem::take(if script {
            &mut self.script_presses
        } else {
            &mut self.presses
        });
        let mut w = 0;
        for p in &mut ps {
            if p.left > 0 {
                p.left -= 1;
                w |= p.word;
            } else if !p.started && (p.when)(self) {
                p.started = true;
                p.left = p.ticks.saturating_sub(1);
                w |= p.word;
            }
        }
        *(if script {
            &mut self.script_presses
        } else {
            &mut self.presses
        }) = ps;
        w
    }

    fn script_on(&self) -> bool {
        !self.script_presses.is_empty()
    }

    fn script_advance(&mut self) -> u16 {
        self.script_steps += 1;
        let w = self.eval_presses(true);
        self.script_word = Some(w);
        w
    }

    fn script_word_once(&mut self) -> u16 {
        match self.script_word {
            Some(w) => w,
            None => self.script_advance(),
        }
    }

    pub fn plan_done(&self) -> bool {
        self.plan_i >= self.plan.len()
    }

    pub fn tick<P: Driver, N: Net>(&mut self, np: &mut P, net: &mut N) -> Action {
        self.observe();
        self.fed = [None; 3];
        self.tick_word = None;
        self.script_word = None;
        let before = crate::test_alloc::count();
        let a = np.on_input(self, net);
        self.on_input_allocs += crate::test_alloc::count() - before;
        if self.hud_log.is_some() {
            let h = np.hud_probe();
            if let Some(log) = self.hud_log.as_mut() {
                log.push(h);
            }
        }
        if self.script_word.is_some_and(|w| w != 0) {
            self.script_nonzero_ticks += 1;
            if a != Action::Local {
                self.script_nonzero_non_local += 1;
            }
        }
        if (self.demo_req || self.demo_left.is_some()) && a != Action::Local {
            self.demo_not_local += 1;
        }
        match a {
            Action::RenderOnly => return a,
            Action::Local => {
                let mut w = self.word_once();
                if self.script_on() {
                    // 台本の語（読みで進めていればその結果。進めていなければここで 1 回進める）は全部入る。
                    w |= self.script_word_once();
                }
                let w = w & 0x00FF;
                if w & higanr_core::input_model::keys::ESC != 0 {
                    self.esc_local.push(self.tick_no);
                }
                let mut f = [0u16; 3];
                f[self.seat] = w;
                f[2] = w;
                self.fed = f.map(Some);
            }
            _ => {}
        }
        let fed = self.fed.map(|x| x.unwrap_or(0));
        let pressed = [0, 1, 2].map(|i| fed[i] & !self.prev[i]);
        self.prev = fed;
        let played = matches!(a, Action::Played(_));
        self.menu_fed = matches!(a, Action::Menu);
        if self.menu_fed {
            self.fed_menu.push(fed);
        }
        if let Action::Played(r) = a {
            self.played.push((r.seg, r.frame, r.p1, r.p2));
            self.fed_played.push(fed);
        }
        self.update(np, pressed, fed, played);
        a
    }

    fn update<P: Driver>(&mut self, np: &mut P, pressed: [u16; 3], held: [u16; 3], played: bool) {
        // タイトルのデモ: 選択を通らずに場面 2 へ（リプレイの再生のマッチ）。デモの更新はネットワークの判断が `Local` の tick にだけ走る。
        if self.demo_req && self.cur == 1 && self.next == 2 {
            self.demo_req = false;
            self.cur = 2;
            self.next = 0;
            self.demo_left = Some(self.demo_len);
            self.demo_load = self.load_ticks;
            return;
        }
        if let Some(left) = self.demo_left {
            if self.cur == 2 && self.next == 1 {
                self.cur = 1;
                self.next = 0;
                self.demo_left = None;
                self.demos += 1;
                self.screen = 6;
                self.sub = 1;
                self.anim = 0;
                return;
            }
            if self.demo_load > 0 {
                self.demo_load -= 1;
                if self.demo_load == 0 {
                    np.on_match_start(self);
                }
                return;
            }
            self.demo_frames += 1;
            let left = left.saturating_sub(1);
            self.demo_left = Some(left);
            if left == 0 {
                self.next = 1;
            }
            return;
        }
        if self.cur == 1 && self.next == 2 {
            self.cur = 2;
            self.next = 0;
            self.load_left = self.load_ticks;
            self.in_match = false;
            self.frames = 0;
            self.rematch_left = self.rematches;
            return;
        }
        if self.cur == 2 && self.next == 10 {
            self.next = 0;
            self.load_left = self.load_ticks;
            self.in_match = false;
            self.frames = 0;
            return;
        }
        if self.cur == 2 && self.next == 1 {
            self.cur = 1;
            self.next = 0;
            self.in_match = false;
            self.screens.push(6);
            self.set_screen(7, false);
            self.pending_auto8 = true;
            return;
        }
        if self.cur == 2 {
            if self.load_left > 0 {
                self.load_left -= 1;
                if self.load_left == 0 {
                    np.on_match_start(self);
                    self.in_match = true;
                    self.rng = 0x1234_5678;
                }
                return;
            }
            if self.in_match && played {
                self.frames += 1;
                self.rng =
                    self.rng.rotate_left(5) ^ (u64::from(held[0]) << 16) ^ u64::from(held[1]);
                if self.frames >= self.match_frames {
                    self.matches += 1;
                    if self.rematch_left > 0 {
                        self.rematch_left -= 1;
                        self.next = 10;
                    } else {
                        self.next = 1;
                    }
                }
            }
            return;
        }
        self.menu(pressed, held);
        if self.demo_req && self.demo_req_phase.is_none() {
            self.demo_req_phase = Some(np.phase());
        }
    }

    fn count_ignored(&mut self, pressed: &[u16]) {
        for p in pressed {
            if p & (DECIDE | CANCEL) != 0 {
                self.ignore();
            }
        }
    }

    /// 読まれなかった画面を移る押下を数える（dll がネットの選択で feed した tick だけ。いつもの入力の押下は数えない）。
    fn ignore(&mut self) {
        if self.menu_fed {
            self.ignored_presses += 1;
        }
    }

    fn menu(&mut self, pressed: [u16; 3], held: [u16; 3]) {
        if self.pending_auto8 {
            self.pending_auto8 = false;
            self.count_ignored(&pressed);
            self.set_screen(8, false);
            return;
        }
        if self.sub == 0 {
            self.count_ignored(&pressed);
            self.anim_step();
            return;
        }
        let p2 = pressed[2];
        match self.screen {
            6 => {
                if pressed == [0; 3]
                    && let Some(n) = self.demo_after
                {
                    self.demo_idle += 1;
                    if self.demo_idle >= n {
                        self.demo_after = None;
                        self.demo_req = true;
                        self.next = 2;
                        return;
                    }
                }
                self.count_ignored(&pressed[..2]);
                if p2 & DECIDE != 0 {
                    self.set_screen(7, true);
                } else if p2 & CANCEL != 0 {
                    self.ignore();
                }
            }
            7 => {
                self.count_ignored(&pressed[..2]);
                if p2 & keys::UP != 0 {
                    self.cursor = (self.cursor + 3) % 4;
                }
                if p2 & keys::DOWN != 0 {
                    self.cursor = (self.cursor + 1) % 4;
                }
                if p2 & DECIDE != 0 {
                    self.saved_diff = self.cursor;
                    self.set_screen(8, true);
                } else if p2 & CANCEL != 0 {
                    self.saved_diff = self.cursor;
                    self.set_screen(6, false);
                }
            }
            8 => {
                for ((cur, &dec), &p) in self
                    .chara_cursor
                    .iter_mut()
                    .zip(&self.decided)
                    .zip(&pressed)
                {
                    if dec == 0 {
                        if p & keys::LEFT != 0 {
                            *cur = (*cur + 15) % 16;
                        }
                        if p & keys::RIGHT != 0 {
                            *cur = (*cur + 1) % 16;
                        }
                    }
                }
                let ch = [0, 1].map(|s| CHARA_TABLE[self.chara_cursor[s] as usize]);
                for s in 0..2 {
                    if pressed[s] & DECIDE != 0 {
                        if self.decided[s] == 0 {
                            self.decided[s] = 1;
                            let o = 1 - s;
                            self.color[s] = ((held[s] >> 2) & 1) as u8;
                            if self.decided[o] != 0 && ch[o] == ch[s] {
                                self.color[s] = 1 - self.color[o];
                            }
                        } else {
                            self.ignore();
                        }
                    }
                }
                if self.decided == [1, 1] {
                    self.chara = ch;
                    self.decided_chars.push(ch);
                    self.set_screen(16, true);
                    return;
                }
                let c = [0, 1].map(|s| pressed[s] & CANCEL != 0);
                if (c[0] && self.decided[0] == 0) || (c[1] && self.decided[1] == 0) {
                    self.difficulty = self.cursor;
                    self.set_screen(7, false);
                    return;
                }
                for (d, &cancel) in self.decided.iter_mut().zip(&c) {
                    if cancel && *d == 1 {
                        *d = 0;
                    }
                }
            }
            16 => {
                self.count_ignored(&pressed[..2]);
                let n = self.map_list.iter().filter(|&&x| x != 0xFF).count() as u32;
                if p2 & keys::DOWN != 0 {
                    self.cursor = (self.cursor + 1) % n;
                }
                if p2 & keys::UP != 0 {
                    self.cursor = (self.cursor + n - 1) % n;
                }
                if p2 & DECIDE != 0 {
                    let v = self.map_list[self.cursor as usize];
                    self.map = match v {
                        14 => -1,
                        15 => -2,
                        0xFF => -1,
                        v => i32::from(v),
                    };
                    self.decided_maps.push(self.map);
                    self.difficulty = self.saved_diff;
                    self.next = 2;
                    // MainMenu のノードの登録が外れる（画面は 16 のまま、以後は走らない）。
                    self.sub = 99;
                } else if p2 & CANCEL != 0 {
                    self.set_screen(8, true);
                }
            }
            _ => {}
        }
    }
}

impl Game for MenuGame {
    fn scenes(&mut self) -> (u32, u32) {
        (self.cur, self.next)
    }
    fn local_input(&mut self, _seat: Seat) -> u16 {
        self.local_input_calls += 1;
        let mut w = self.word_once();
        if self.script_on() {
            w |= self.script_word_once();
        }
        match (self.cur, self.match_words) {
            (2, Some(seed)) => {
                self.match_word_n += 1;
                local_word(seed, self.match_word_n)
            }
            _ => w,
        }
    }
    fn digit_keys(&mut self) -> Option<u16> {
        self.digit_held
    }
    fn keyboard_ctrl_down(&mut self) -> bool {
        self.ctrl_down
    }
    fn scripted_run(&self) -> bool {
        self.scripted
    }
    fn digit_script(&mut self) -> Option<u16> {
        if let Some(i) = self
            .tab_at
            .iter()
            .position(|&(s, age)| s == self.screen && age == self.screen_age)
        {
            self.tab_at.remove(i);
            return Some(crate::digit_keys::TAB_BIT);
        }
        self.digit_script.take()
    }
    fn peek_local_input(&mut self) -> u16 {
        // 本物の `NetGame` と同じ約束: デバイスの語に、台本の語の ESC だけを足す。台本はここで 1 回進める（呼ぶたびに数える）。
        // `local_input_calls` は増やさない。
        self.peek_calls += 1;
        let device = self.word_once();
        if self.script_on() {
            device | (self.script_advance() & keys::ESC)
        } else {
            device
        }
    }
    fn feed(&mut self, index: u32, word: u16) {
        let f = &mut self.fed[index as usize];
        assert!(f.is_none(), "同じ tick に index {index} を 2 回 feed した");
        *f = Some(word);
    }
    fn own_select(&mut self, seat: Seat, role: Role) -> Select {
        let s = seat_index(seat);
        Select {
            match_no: 0,
            chara: self.chara[s],
            color: self.color[s],
            life: 10,
            charge: s as u8,
            map: self.map as i8 as u8,
            difficulty: self.difficulty as u8,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: if role == Role::Host { 0x4243 } else { 0 },
            option_digest: 0,
        }
    }
    fn apply_agreed(&mut self, peer_seat: Seat, a: &Agreed) -> Result<(), BadValue> {
        let s = seat_index(peer_seat);
        self.chara[s] = a.peer.chara;
        self.color[s] = a.peer.color;
        self.map = i32::from(a.map as i8);
        self.difficulty = u32::from(a.difficulty);
        self.agreed.push(*a);
        Ok(())
    }
    fn apply_watched(&mut self, p1: &Select, p2: &Select) -> Result<(), BadValue> {
        for (s, sel) in [p1, p2].into_iter().enumerate() {
            self.chara[s] = sel.chara;
            self.color[s] = sel.color;
        }
        self.map = i32::from(p1.map as i8);
        self.difficulty = u32::from(p1.difficulty);
        self.watched.push([*p1, *p2]);
        Ok(())
    }
    fn write_charge(&mut self, p1: u8, p2: u8) -> bool {
        self.charges.push((p1, p2));
        true
    }
    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        // frame 0（まだ 1 フレームも使っていない）の抽選回数は読み込みの引き（両者で違ってよい）、以後は同じ値。
        let count = if self.frames == 0 { self.load_draws } else { 5 };
        (self.rng as u16, count, 10, 10)
    }
    fn menu_view(&mut self) -> Option<MenuView> {
        assert!(
            self.cur == 1 && self.next != 2,
            "止める地点で MainMenu を読んだ"
        );
        Some(MenuView {
            screen: self.screen,
            sub: self.sub,
            trans: 0,
            cursor: self.cursor,
            chara_cursor: self.chara_cursor,
            decided: self.decided,
            select_kind: 0,
            map_list: self.map_list,
        })
    }
    fn write_menu_u32(&mut self, offset: u32, value: u32) -> bool {
        match offset {
            MENU_CURSOR => self.cursor = value,
            o if o == MENU_CHARA_CURSOR || o == MENU_CHARA_CURSOR + 4 => {
                let seat = ((o - MENU_CHARA_CURSOR) / 4) as usize;
                if self.chara_cursor[seat] != value {
                    self.cursor_jumps
                        .push((seat, self.chara_cursor[seat], value));
                }
                self.chara_cursor[seat] = value;
            }
            _ => return false,
        }
        true
    }
    fn follow_portrait(&mut self, v: &MenuView, seat: usize, cursor: u32) -> Followed {
        /// ゲートを全部開けた偽物（署名・MainMenu・部品・解放は本物の窓の検査 `net_game.rs` が見る）。
        struct Open;
        impl PortraitEnv for Open {
            fn unlocked(&mut self, _chara: u8) -> bool {
                true
            }
            fn ready(&mut self) -> Option<u32> {
                Some(1)
            }
        }
        match plan(v, seat, cursor, &mut Open) {
            Plan::Unchanged => Followed::Unchanged,
            Plan::Refused => Followed::Refused,
            Plan::Calls { .. } => {
                self.portraits
                    .push((seat, v.chara_cursor[seat & 1], cursor));
                Followed::Called
            }
        }
    }
    fn map_list(&mut self) -> [u8; 16] {
        self.map_list
    }
}

pub(super) fn addr(p: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], p))
}

pub(super) fn link_cfg(peer: u16, nonce: u64) -> link::Config {
    link_cfg_named(peer, nonce, "m")
}

/// `link_cfg` に名前を指定する形（観戦の名前の検査は、2 人の名前が違わないと席の取り違えに気づけない）。
pub(super) fn link_cfg_named(peer: u16, nonce: u64, name: &str) -> link::Config {
    link::Config {
        handshake: handshake::Config {
            session_id: 9,
            role_nonce: nonce,
            latency: 2,
            name: Name::new(name),
            host_seat: Seat::P1,
        },
        secret: peer_sim::cli::DEFAULT_SECRET,
        peers: higanr_core::candidate::Candidates::from_addrs(&[addr(peer)]).unwrap(),
        key_seed: higanr_core::seal::KeySeed([nonce as u8; 32]),
    }
}

/// 1 回走らせる。A はホスト 1P、B はゲスト 2P。どちらも `max_matches` マッチを終えてメニューへ戻り、台本を使い切るか、
/// `ticks` を越えたら止める。
pub(super) struct Run {
    pub(super) a: MenuGame,
    pub(super) b: MenuGame,
    pub(super) npa: NetPlay,
    pub(super) npb: NetPlay,
}

fn run(a: MenuGame, b: MenuGame, delay_us: u64, drop_a: u32, drop_b: u32, ticks: u32) -> Run {
    run_with_ring(a, b, delay_us, drop_a, drop_b, ticks, None)
}

/// `run` に、ホスト（A）へ観戦のリングバッファを付ける入口（`spectate_tests.rs`）。リングバッファを読むのは呼び手（走らせた後）。
pub(super) fn run_with_ring(
    a: MenuGame,
    b: MenuGame,
    delay_us: u64,
    drop_a: u32,
    drop_b: u32,
    ticks: u32,
    ring: Option<std::sync::Arc<crate::spectate_ring::SpectateRing>>,
) -> Run {
    run_with_rings(a, b, delay_us, drop_a, drop_b, ticks, (ring, None), false)
}

/// `run_with_ring` の、両者にリングバッファを付けられる形。`host_only` が真なら、ゲーム内のメニューの経路と同じ `NetPlay::set_spectate_for_host` で置く。
// 検査の道具。引数を束ねる型を足しても読み手は増えない。
#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_rings(
    mut a: MenuGame,
    mut b: MenuGame,
    delay_us: u64,
    drop_a: u32,
    drop_b: u32,
    ticks: u32,
    rings: (
        Option<std::sync::Arc<crate::spectate_ring::SpectateRing>>,
        Option<std::sync::Arc<crate::spectate_ring::SpectateRing>>,
    ),
    host_only: bool,
) -> Run {
    let ring = rings.0;
    let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), delay_us);
    na.drop_every = drop_a;
    nb.drop_every = drop_b;
    let cfg = Config {
        check: CheckSource::InputDigest,
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        ..Config::default()
    };
    let mut npa = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
    if let Some(r) = ring {
        if host_only {
            npa.set_spectate_for_host(r);
        } else {
            npa.set_spectate(r);
        }
    }
    let mut npb = NetPlay::new(link_cfg(1, 1), cfg, &mut nb);
    if let Some(r) = rings.1 {
        if host_only {
            npb.set_spectate_for_host(r);
        } else {
            npb.set_spectate(r);
        }
    }
    let mut slot = clock.get();
    for _ in 0..ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        a.tick(&mut npa, &mut na);
        clock.set(clock.get().max(slot + FRAME_US / 2));
        b.tick(&mut npb, &mut nb);
        slot += FRAME_US;
        let back = |g: &MenuGame| {
            g.matches >= g.max_matches
                && g.cur == 1
                && g.screen == 8
                && !g.pending_auto8
                && g.plan_done()
        };
        if back(&a) && back(&b) {
            break;
        }
    }
    Run { a, b, npa, npb }
}

pub(super) fn plans() -> (Vec<Act>, Vec<Act>) {
    use keys::{DOWN, LEFT, RIGHT, SHIFT, UP, X, Z};
    // 両者とも画面 6 で 30 tick 待って 7 へ入る。入る演出は 9 tick なので、wait 8 は「読む状態になった最初の tick に押す」
    // （ゲストでは、ホストの押下の STEP が演出の明ける前に届き、持ち越す形になる）。両者の最初のカーソルは違えてあるので、
    // 押下を入れる tick にカーソルを合わせなければ、ゲームが決める値が両者で分かれる。
    let a = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 8, X, 1),
        act(0, 6, 10, Z, 1),
        act(0, 7, 20, DOWN, 1),
        act(0, 8, 12, RIGHT, 1),
        act(0, 8, 3, RIGHT, 1),
        act(0, 8, 3, Z, 1),
        act(0, 8, 20, X, 1),
        act(0, 8, 6, Z, 1),
        act(0, 16, 8, X, 1),
        act(0, 8, 30, Z, 1),
        // マッチから戻ったキャラ選択は直に呼ばれない経路なので、読む状態になるのは入ってから 10 tick 目（wait 9）。
        act(1, 8, 9, Z, 1),
        act(1, 16, 12, DOWN, 1),
        act(1, 16, 3, Z, 1),
    ];
    let b = vec![
        act(0, 6, 30, Z, 1),
        act(0, 6, 15, Z, 1),
        act(0, 7, 40, Z, 1),
        act(0, 8, 15, LEFT, 1),
        act(0, 8, 60, Z | SHIFT, 1),
        act(0, 8, 50, Z | SHIFT, 1),
        act(0, 16, 30, UP, 1),
        act(0, 16, 6, Z, 1),
        act(1, 8, 20, Z, 1),
    ];
    (a, b)
}

#[test]
fn both_sides_walk_the_same_screens() {
    let mut total_held = 0u64;
    let mut total_jumps = 0usize;
    let mut total_applied = 0u32;
    for (i, (delay_ms, drop_a, drop_b, b_anim)) in [
        (1u64, 0u32, 0u32, 8u32),
        (5, 0, 0, 8),
        (16, 0, 0, 8),
        (40, 0, 0, 8),
        (8, 3, 0, 8),
        (8, 0, 3, 8),
        (12, 5, 7, 8),
        (25, 7, 5, 8),
        // B の入る演出を長くする（A の押下が B の演出の間に届き、B が持ち越す形を必ず作る）。
        (1, 0, 0, 14),
        (16, 0, 0, 14),
        (12, 5, 7, 14),
    ]
    .into_iter()
    .enumerate()
    {
        let (pa, pb) = plans();
        let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
        b.saved_diff = 3;
        b.chara_cursor = [5, 10];
        a.max_matches = 2;
        b.max_matches = 2;
        b.anim_len = b_anim;
        let r = run(a, b, delay_ms * 1_000, drop_a, drop_b, 20_000);
        let ctx =
            format!("走行 {i}（片道 {delay_ms} ms、落とす {drop_a}/{drop_b}、B の演出 {b_anim}）");
        assert!(
            r.a.plan_done() && r.b.plan_done(),
            "{ctx}: 台本を使い切っていない（A の項目 {} 画面 {:?} / B の項目 {} 画面 {:?} / 終わり {:?}）",
            r.a.plan_i,
            r.a.screens,
            r.b.plan_i,
            r.b.screens,
            r.npa.end()
        );
        assert_eq!(r.a.matches, 2, "{ctx}: A の試合");
        assert_eq!(r.b.matches, 2, "{ctx}: B の試合");
        assert_eq!(r.a.screens, r.b.screens, "{ctx}: 通った画面の列");
        assert_eq!(r.a.ignored_presses, 0, "{ctx}: A が読まなかった押下");
        assert_eq!(r.b.ignored_presses, 0, "{ctx}: B が読まなかった押下");
        let steps = |np: &NetPlay| {
            np.step_log()
                .iter()
                .map(|s| (s.step.step, s.step.kind, s.step.value, s.step.screen))
                .collect::<Vec<_>>()
        };
        assert_eq!(steps(&r.npa), steps(&r.npb), "{ctx}: 入った押下の列");
        let own = |np: &NetPlay| np.step_log().iter().map(|s| s.own).collect::<Vec<_>>();
        let (oa, ob) = (own(&r.npa), own(&r.npb));
        assert!(
            oa.iter().zip(&ob).all(|(x, y)| x != y),
            "{ctx}: 誰の押下か {oa:?} {ob:?}"
        );
        assert_eq!(r.a.agreed.len(), 2, "{ctx}");
        assert_eq!(r.b.agreed.len(), 2, "{ctx}");
        assert_eq!(
            (r.a.chara, r.a.color, r.a.difficulty, r.a.map),
            (r.b.chara, r.b.color, r.b.difficulty, r.b.map),
            "{ctx}: 試合に入った値"
        );
        // ゲームが決めた値そのもの（SELECT で書き直す前）も両者で同じ（押下の tick にカーソルを合わせた）。
        assert_eq!(
            r.a.decided_chars, r.b.decided_chars,
            "{ctx}: ゲームが決めたキャラ"
        );
        assert_eq!(
            r.a.decided_maps, r.b.decided_maps,
            "{ctx}: ゲームが決めたマップ"
        );
        assert_eq!(
            r.a.saved_diff, r.b.saved_diff,
            "{ctx}: 難易度のカーソルの控え"
        );
        // 1 マッチ目は合わせの後の値（両者の最初のカーソルが違っても、入ったときに B が A の値へ寄る）。
        let first = r.a.agreed[0];
        assert_eq!(first.difficulty, 2, "{ctx}: 難易度");
        assert_eq!(first.map, 9, "{ctx}: マップ");
        assert_eq!(r.b.agreed[0].map, first.map, "{ctx}");
        assert_eq!(
            (first.peer.chara, first.peer.color),
            (CHARA_TABLE[7], 1),
            "{ctx}: A が受けた 2P"
        );
        assert_eq!(
            r.a.decided_chars.first(),
            Some(&[CHARA_TABLE[5], CHARA_TABLE[7]]),
            "{ctx}"
        );
        let second = r.a.agreed[1];
        let m = second.map as i8;
        assert!((0..=13).contains(&m), "{ctx}: マップ −2 の置き換え {m}");
        assert_eq!(r.b.agreed[1].map, second.map, "{ctx}");
        assert_eq!(r.a.decided_maps.last(), Some(&-2), "{ctx}");
        assert_eq!(second.difficulty, 2, "{ctx}: 難易度は前のまま");
        assert_eq!(
            (r.a.on_input_allocs, r.b.on_input_allocs),
            (0, 0),
            "{ctx}: on_input が確保した"
        );
        assert_eq!(r.a.played, r.b.played, "{ctx}: 試合の列");
        assert!(!r.a.played.is_empty());
        for (who, g, np) in [("A", &r.a, &r.npa), ("B", &r.b, &r.npb)] {
            assert_eq!(g.portraits, g.cursor_jumps, "{ctx}: {who} の立ち絵");
            assert_eq!(
                (
                    np.stats().menu_portrait_called as usize,
                    np.stats().menu_portrait_refused
                ),
                (g.portraits.len(), 0),
                "{ctx}: {who} の数"
            );
            total_jumps += g.cursor_jumps.len();
        }
        for np in [&r.npa, &r.npb] {
            assert_eq!(
                np.stats().menu_align_miss,
                0,
                "{ctx}: カーソルを合わせられなかった"
            );
            assert_eq!(np.stats().menu_lost, 0, "{ctx}");
            total_held += np.stats().menu_held;
            total_applied += np.stats().menu_applied;
        }
    }
    // 持ち越しが本当に起きた（0 なら入る演出の期間を踏めていない）。
    eprintln!(
        "ネットの選択の組み上がり（11 走行 × 2 台）: 入れた押下 {total_applied}、持ち越したフレーム {total_held}"
    );
    assert!(total_held > 10, "持ち越し {total_held}");
    // カーソルを違う値へ書いた回が本当にあった（0 なら立ち絵の検査は空振り）。
    assert!(total_jumps > 10, "カーソルを書いた回 {total_jumps}");
    assert!(total_applied > 100, "{total_applied}");
}

#[test]
fn a_human_walks_selection_match_rematch_and_bye() {
    use higanr_core::wire::ByeReason;
    use keys::{DOWN, LEFT, RIGHT, SHIFT, UP, Z};
    for (delay_ms, drop_a, drop_b) in [(1u64, 0u32, 0u32), (16, 0, 0), (40, 0, 0), (12, 5, 7)] {
        let pa = vec![
            act(0, 6, 30, Z, 6),
            act(0, 7, 14, Z, 14),
            act(0, 8, 12, RIGHT, 4),
            act(0, 8, 6, Z, 9),
            act(2, 8, 12, Z, 5),
            act(2, 16, 14, DOWN, 3),
            act(2, 16, 5, Z, 6),
        ];
        let pb = vec![
            act(0, 6, 30, Z, 6),
            act(0, 8, 20, LEFT, 3),
            act(0, 8, 30, Z | SHIFT, 8),
            act(0, 16, 25, UP, 2),
            act(0, 16, 8, Z, 6),
            act(2, 8, 25, Z, 5),
        ];
        let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
        b.saved_diff = 3;
        b.chara_cursor = [5, 10];
        for (g, seed, draws) in [(&mut a, 0xA11u64, 2u32), (&mut b, 0xB22, 1030)] {
            g.match_words = Some(seed);
            g.rematches = 1;
            g.match_frames = 75;
            g.max_matches = 4;
            g.load_draws = draws;
        }
        let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), delay_ms * 1_000);
        na.drop_every = drop_a;
        nb.drop_every = drop_b;
        let cfg = Config {
            check: CheckSource::Game,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            ..Config::default()
        };
        let mut npa = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
        let mut npb = NetPlay::new(link_cfg(1, 1), cfg, &mut nb);
        let ctx = format!("片道 {delay_ms} ms、落とす {drop_a}/{drop_b}");
        let mut slot = clock.get();
        let mut closed_at: Option<u32> = None;
        for t in 0..30_000u32 {
            if clock.get() < slot {
                clock.set(slot);
            }
            a.tick(&mut npa, &mut na);
            clock.set(clock.get().max(slot + FRAME_US / 2));
            b.tick(&mut npb, &mut nb);
            slot += FRAME_US;
            let back = |g: &MenuGame| {
                g.matches >= g.max_matches
                    && g.cur == 1
                    && g.screen == 8
                    && !g.pending_auto8
                    && g.plan_done()
            };
            if closed_at.is_none() && back(&a) && back(&b) {
                npb.close(ByeReason::Finished);
                closed_at = Some(t);
            }
            if closed_at.is_some_and(|c| t >= c + 120) {
                break;
            }
        }
        assert!(
            closed_at.is_some(),
            "{ctx}: 終わりまで進まない（A 項目 {} 画面 {:?} 試合 {} / B 項目 {} 画面 {:?} 試合 {} / {:?} {:?}）",
            a.plan_i,
            a.screens,
            a.matches,
            b.plan_i,
            b.screens,
            b.matches,
            npa.end(),
            npb.end()
        );
        assert_eq!(
            npa.end(),
            Some(super::End::PeerBye(ByeReason::Finished)),
            "{ctx}"
        );
        assert_eq!(
            npb.end(),
            Some(super::End::Closed(ByeReason::Finished)),
            "{ctx}"
        );
        assert_eq!(a.screens, b.screens, "{ctx}: 通った画面の列");
        let steps = |np: &NetPlay| {
            np.step_log()
                .iter()
                .map(|s| (s.step.step, s.step.kind, s.step.value, s.step.screen))
                .collect::<Vec<_>>()
        };
        assert_eq!(steps(&npa), steps(&npb), "{ctx}: 入った押下の列");
        // 決定・取消（合わせを除く）の数。合わせは画面ごとに 1 回ずつ（5 回）。
        let real = |np: &NetPlay| {
            np.step_log()
                .iter()
                .filter(|s| s.step.kind != StepKind::Align)
                .count()
        };
        assert_eq!(
            real(&npa),
            7,
            "{ctx}: 押し続けを 2 回目の押下にした・押下を取りこぼした: {:?}",
            steps(&npa)
        );
        assert_eq!(
            steps(&npa).len() - real(&npa),
            5,
            "{ctx}: 画面ごとの合わせの数: {:?}",
            steps(&npa)
        );
        assert_eq!((a.ignored_presses, b.ignored_presses), (0, 0), "{ctx}");
        assert_eq!((a.agreed.len(), b.agreed.len()), (2, 2), "{ctx}");
        assert_eq!(
            (a.chara, a.color, a.difficulty, a.map),
            (b.chara, b.color, b.difficulty, b.map),
            "{ctx}: 試合に入った値"
        );
        assert_eq!(
            a.decided_chars.first(),
            Some(&[CHARA_TABLE[4], CHARA_TABLE[7]]),
            "{ctx}: 押し続けが決定になった"
        );
        assert_eq!(a.decided_chars, b.decided_chars, "{ctx}");
        assert_eq!(a.agreed[0].difficulty, 1, "{ctx}");
        assert_eq!(a.agreed[0].peer.color, 1, "{ctx}: B の Shift");
        assert_eq!(a.played, b.played, "{ctx}: 試合の列");
        assert_eq!(a.played.len(), 4 * 75, "{ctx}");
        let segs: std::collections::BTreeSet<u16> = a.played.iter().map(|r| r.0).collect();
        assert_eq!(
            segs.into_iter().collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "{ctx}"
        );
        assert!(
            a.played
                .iter()
                .filter(|r| r.1 >= 2)
                .all(|r| r.2 != 0 && r.3 != 0),
            "{ctx}: 試合の中の人の語が入っていない"
        );
        // CHECK: frame 0 と 30・60 を 4 マッチ。ロスの実行は落ちた分だけ減る（送り直さない）。
        for (who, np) in [("A", &npa), ("B", &npb)] {
            let c = np.connected().expect("繋がった");
            let m = c.counts().check_matches;
            let want = if drop_a == 0 && drop_b == 0 { 12 } else { 6 };
            assert!(m >= want, "{ctx}: {who} の CHECK の一致 {m}");
            assert_eq!(np.stats().menu_align_miss, 0, "{ctx}: {who}");
            assert_eq!(np.stats().menu_lost, 0, "{ctx}: {who}");
        }
        assert_eq!((a.on_input_allocs, b.on_input_allocs), (0, 0), "{ctx}");
    }
}

fn link_cfg_seat(peer: u16, nonce: u64, host_seat: Seat) -> link::Config {
    let mut c = link_cfg(peer, nonce);
    c.handshake.host_seat = host_seat;
    c
}

#[test]
fn simultaneous_chara_decides_are_not_lost() {
    use keys::Z;
    let (mut guest_retried, mut host_retried) = (0u32, 0u32);
    for host_seat in [Seat::P1, Seat::P2] {
        for delay_ms in [1u64, 16] {
            for b_wait in 8..=30u32 {
                let (sa, sb) = match host_seat {
                    Seat::P1 => (0usize, 1usize),
                    Seat::P2 => (1, 0),
                };
                let pa = vec![
                    act(0, 6, 30, Z, 1),
                    act(0, 7, 12, Z, 1),
                    act(0, 8, 20, Z, 1),
                    act(0, 16, 12, Z, 1),
                ];
                let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, b_wait, Z, 1)];
                let mut a = MenuGame::new(sa, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
                let mut b = MenuGame::new(sb, pb, &[0, 2, 5, 9]);
                a.max_matches = 1;
                b.max_matches = 1;
                let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), delay_ms * 1_000);
                let cfg = Config {
                    check: CheckSource::InputDigest,
                    wait_budget_us: WAIT_BUDGET_US,
                    require_net_selection: true,
                    ..Config::default()
                };
                let mut npa = NetPlay::new(link_cfg_seat(2, 0xF000, host_seat), cfg, &mut na);
                let mut npb = NetPlay::new(link_cfg_seat(1, 1, host_seat), cfg, &mut nb);
                let mut slot = clock.get();
                for _ in 0..6_000 {
                    if clock.get() < slot {
                        clock.set(slot);
                    }
                    a.tick(&mut npa, &mut na);
                    clock.set(clock.get().max(slot + FRAME_US / 2));
                    b.tick(&mut npb, &mut nb);
                    slot += FRAME_US;
                    if a.matches >= 1 && b.matches >= 1 && a.cur == 1 && b.cur == 1 {
                        break;
                    }
                }
                let ctx =
                    format!("ホストの席 {host_seat:?}・片道 {delay_ms} ms・B の wait {b_wait}");
                assert_eq!(
                    (a.matches, b.matches),
                    (1, 1),
                    "{ctx}: 試合まで進まない（A 画面 {:?} 旗 {:?} / B 画面 {:?} 旗 {:?} / 押下 {:?}）",
                    a.screens,
                    a.decided,
                    b.screens,
                    b.decided,
                    npa.step_log().iter().map(|s| s.step).collect::<Vec<_>>()
                );
                assert_eq!(a.decided_chars, b.decided_chars, "{ctx}");
                let mut want = [0u8; 2];
                want[sa] = CHARA_TABLE[a.chara_cursor[sa] as usize];
                want[sb] = CHARA_TABLE[b.chara_cursor[sb] as usize];
                assert_eq!(a.decided_chars.first(), Some(&want), "{ctx}");
                assert_eq!(a.played, b.played, "{ctx}");
                // 押し直した実行を役ごとに数える。どちらかが 0 なら、その期間を踏めていない。
                guest_retried += u32::from(npb.stats().menu_retries > 0);
                host_retried += u32::from(npa.stats().menu_retries > 0);
            }
        }
    }
    eprintln!(
        "キャラの画面の同時の決定（2 席 × 2 遅れ × 23 の時刻 = 92 走行）: ゲストが押し直した実行 {guest_retried}、ホストが押し直した実行 {host_retried}"
    );
    assert!(guest_retried > 0, "ゲストの要求が負ける期間を踏めていない");
    assert!(
        host_retried > 0,
        "ホストの押下が捨てられる期間を踏めていない"
    );
}

#[test]
fn a_title_demo_is_left_alone_and_the_net_match_still_works() {
    use keys::{LEFT, RIGHT, SHIFT, UP, Z};
    const DEMO_LEN: u32 = 30;
    const MATCH_FRAMES: u32 = 40;
    // 画面 6 の最初の押下までの tick。デモに入った側は戻ってから数え直す。入らない側は相手のデモの分を足し、両者がほぼ同じ頃に
    // 選択の画面へ入るようにする（先に入ったホストの押下は相手が来るまで入らず、この台本は 1 回しか押さない）。
    const W: u32 = 40;
    const W_WAIT_PEER_DEMO: u32 = W + 57;
    let mut runs = 0u32;
    for (demo_a, demo_b) in [
        (Some(12u32), None),
        (None, Some(12u32)),
        (Some(12), Some(16)),
    ] {
        for delay_ms in [1u64, 16, 40] {
            let wait_of = |mine: Option<u32>, other: Option<u32>| match (mine, other) {
                (None, Some(_)) => W_WAIT_PEER_DEMO,
                _ => W,
            };
            let pa = vec![
                act(0, 6, wait_of(demo_a, demo_b), Z, 6),
                act(0, 7, 14, Z, 14),
                act(0, 8, 12, RIGHT, 4),
                act(0, 8, 6, Z, 9),
            ];
            let pb = vec![
                act(0, 6, wait_of(demo_b, demo_a), Z, 6),
                act(0, 8, 20, LEFT, 3),
                act(0, 8, 30, Z | SHIFT, 8),
                act(0, 16, 25, UP, 2),
                act(0, 16, 8, Z, 6),
            ];
            let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
            let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
            b.saved_diff = 3;
            b.chara_cursor = [5, 10];
            for (g, seed, demo) in [(&mut a, 0xA11u64, demo_a), (&mut b, 0xB22, demo_b)] {
                g.match_words = Some(seed);
                g.rematches = 1;
                g.match_frames = MATCH_FRAMES;
                g.max_matches = 2;
                g.demo_after = demo;
                g.demo_len = DEMO_LEN;
            }
            let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), delay_ms * 1_000);
            let cfg = Config {
                check: CheckSource::InputDigest,
                wait_budget_us: WAIT_BUDGET_US,
                require_net_selection: true,
                ..Config::default()
            };
            let mut npa = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
            let mut npb = NetPlay::new(link_cfg(1, 1), cfg, &mut nb);
            let ctx = format!("デモ A {demo_a:?} / B {demo_b:?}、片道 {delay_ms} ms");
            let mut slot = clock.get();
            let mut done = false;
            for _ in 0..6_000u32 {
                if clock.get() < slot {
                    clock.set(slot);
                }
                a.tick(&mut npa, &mut na);
                clock.set(clock.get().max(slot + FRAME_US / 2));
                b.tick(&mut npb, &mut nb);
                slot += FRAME_US;
                let back = |g: &MenuGame| {
                    g.matches >= g.max_matches
                        && g.cur == 1
                        && g.screen == 8
                        && !g.pending_auto8
                        && g.plan_done()
                };
                if back(&a) && back(&b) {
                    done = true;
                    break;
                }
            }
            for (who, g, np, demo) in [("A", &a, &npa, demo_a), ("B", &b, &npb, demo_b)] {
                if demo.is_some() {
                    // 効く期間: 繋がった後（選択の段階）にデモが来た。直す前はここで止まった。
                    assert_eq!(
                        g.demo_req_phase,
                        Some(Phase::Selecting),
                        "{ctx}: {who} のデモが繋がる前に来た（期間を踏めていない）"
                    );
                    assert_eq!(
                        (g.demos, g.demo_frames),
                        (1, DEMO_LEN),
                        "{ctx}: {who} のデモが止まった・終わらない（段階 {:?}、止める地点で待った {}）",
                        np.phase(),
                        np.stats().hold_frames
                    );
                    assert_eq!(
                        g.demo_not_local,
                        0,
                        "{ctx}: {who} のデモにネットワークが触った（止める地点で待った {}、終わり {:?}）",
                        np.stats().hold_frames,
                        np.end()
                    );
                    assert_eq!(np.stats().local_scene2, 1, "{ctx}: {who}");
                } else {
                    assert_eq!(g.demo_req_phase, None, "{ctx}: {who}");
                    assert_eq!(np.stats().local_scene2, 0, "{ctx}: {who}");
                }
            }
            assert!(
                done,
                "{ctx}: 試合・再戦まで進まない（A 項目 {} 画面 {:?} 試合 {} 段階 {:?} / B 項目 {} 画面 {:?} 試合 {} 段階 {:?} / {:?} {:?}）",
                a.plan_i,
                a.screens,
                a.matches,
                npa.phase(),
                b.plan_i,
                b.screens,
                b.matches,
                npb.phase(),
                npa.end(),
                npb.end()
            );
            assert_eq!((npa.end(), npb.end()), (None, None), "{ctx}");
            assert_eq!(a.screens, b.screens, "{ctx}: 通った画面の列");
            let steps = |np: &NetPlay| {
                np.step_log()
                    .iter()
                    .map(|s| (s.step.step, s.step.kind, s.step.value, s.step.screen))
                    .collect::<Vec<_>>()
            };
            assert_eq!(steps(&npa), steps(&npb), "{ctx}: 入った押下の列");
            assert_eq!((a.agreed.len(), b.agreed.len()), (1, 1), "{ctx}");
            assert_eq!(
                (a.chara, a.color, a.difficulty, a.map),
                (b.chara, b.color, b.difficulty, b.map),
                "{ctx}: 試合に入った値"
            );
            assert_eq!(a.played, b.played, "{ctx}: 試合の列");
            assert_eq!(a.played.len(), (2 * MATCH_FRAMES) as usize, "{ctx}");
            let segs: std::collections::BTreeSet<u16> = a.played.iter().map(|r| r.0).collect();
            assert_eq!(segs.into_iter().collect::<Vec<_>>(), vec![0, 1], "{ctx}");
            for np in [&npa, &npb] {
                assert_eq!(np.stats().segs_started, 2, "{ctx}: 開始のフック");
                assert_eq!(np.stats().menu_lost, 0, "{ctx}");
            }
            assert_eq!((a.ignored_presses, b.ignored_presses), (0, 0), "{ctx}");
            assert_eq!((a.on_input_allocs, b.on_input_allocs), (0, 0), "{ctx}");
            runs += 1;
        }
    }
    assert_eq!(runs, 9);
}

// ---- 繋がる前の選択の画面 ----

struct Alone {
    g: MenuGame,
    np: NetPlay,
    net: FakeNet,
    clock: std::rc::Rc<std::cell::Cell<u64>>,
    slot: u64,
    seen: Vec<Action>,
}

impl Alone {
    fn new(screen: u32, plan: Vec<Act>) -> Self {
        Self::with_selection(screen, plan, true)
    }

    fn with_selection(screen: u32, plan: Vec<Act>, require_net_selection: bool) -> Self {
        let (mut na, _nb, clock) = FakeNet::pair(addr(1), addr(2), 1_000);
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection,
            ..Config::default()
        };
        let np = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
        let mut g = MenuGame::new(0, plan, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        g.set_screen(screen, false);
        g.sub = 1;
        let slot = clock.get();
        Alone {
            g,
            np,
            net: na,
            clock,
            slot,
            seen: Vec::new(),
        }
    }

    fn tick(&mut self, n: u32) {
        for _ in 0..n {
            self.clock.set(self.clock.get().max(self.slot));
            let a = self.g.tick(&mut self.np, &mut self.net);
            self.seen.push(a);
            self.slot += FRAME_US;
        }
    }
}

#[test]
fn decide_is_dropped_before_the_link_on_every_selection_screen() {
    use keys::{DOWN, RIGHT, Z};
    let mut t = Alone::new(7, vec![act(0, 7, 2, DOWN, 1), act(0, 7, 2, Z, 1)]);
    t.tick(30);
    assert_eq!(t.g.screen, 7, "難易度の決定が通って画面が移った");
    assert_eq!(t.g.cursor, 2, "カーソルの移動まで落とした");
    assert_eq!(t.g.saved_diff, 1);
    assert_eq!(t.np.stats().menu_pre_link_decides, 1);
    assert_eq!(t.np.stats().menu_applied, 0);
    assert!(t.seen.iter().all(|a| *a == Action::Menu), "{:?}", t.seen);
    assert_eq!(t.np.phase(), Phase::Unconnected);
    let mut t = Alone::new(8, vec![act(0, 8, 2, RIGHT, 1), act(0, 8, 2, Z, 1)]);
    let before = t.g.chara_cursor[0];
    t.tick(30);
    assert_eq!(t.g.decided, [0, 0], "キャラの決定が通った");
    assert_eq!(t.g.chara_cursor[0], (before + 1) % 16);
    assert_eq!(t.g.screen, 8);
    assert_eq!(t.np.stats().menu_pre_link_decides, 1);
    let mut t = Alone::new(16, vec![act(0, 16, 2, DOWN, 1), act(0, 16, 2, Z, 1)]);
    let before = t.g.cursor;
    t.tick(30);
    assert!(t.g.decided_maps.is_empty(), "マップの決定が通った");
    assert_eq!(t.g.next, 0);
    assert_ne!(t.g.cursor, before, "カーソルの移動まで落とした");
    assert_eq!(t.np.stats().menu_pre_link_decides, 1);
}

#[test]
fn dropped_decides_reach_the_log_line() {
    use crate::net_watch::NetStatus;
    let mut t = Alone::new(7, vec![act(0, 7, 2, keys::Z, 1)]);
    t.tick(10);
    let st = NetStatus::new();
    st.publish(&t.np);
    let line = st.menu_text();
    assert!(line.contains("繋がる前に落とした決定 1 回"), "{line}");
    let fresh = Alone::new(7, Vec::new());
    let st = NetStatus::new();
    st.publish(&fresh.np);
    assert!(st.menu_text().contains("繋がる前に落とした決定 0 回"));
}

#[test]
fn cancel_passes_and_the_dropped_decides_count_presses_before_the_link() {
    use keys::{X, Z};
    let mut t = Alone::new(
        7,
        vec![act(0, 7, 2, Z, 10), act(0, 7, 5, Z, 1), act(0, 7, 5, X, 1)],
    );
    t.tick(60);
    assert_eq!(t.np.stats().menu_pre_link_decides, 2);
    assert_eq!(
        t.g.screens.last(),
        Some(&6),
        "取消で 6 へ戻れない: {:?}",
        t.g.screens
    );
    assert_eq!(t.g.screen, 6);
}

#[test]
fn decide_outside_the_selection_screens_is_not_dropped_before_the_link() {
    use keys::Z;
    let mut t = Alone::new(6, vec![act(0, 6, 2, Z, 1)]);
    t.tick(10);
    assert_eq!(t.g.screen, 7, "画面 6 の決定が落ちた: {:?}", t.g.screens);
    assert_eq!(t.np.stats().menu_pre_link_decides, 0);
    assert_eq!(t.seen.first(), Some(&Action::Local));
    assert!(t.seen.contains(&Action::Menu));
}

#[test]
fn nothing_is_touched_before_the_link_without_selection_screens() {
    let mut t = Alone::with_selection(7, vec![act(0, 7, 2, keys::Z, 1)], false);
    t.tick(10);
    assert_eq!(t.g.screen, 8, "選択の画面を持たない窓で決定が落ちた");
    assert_eq!(t.np.stats().menu_pre_link_decides, 0);
    assert!(t.seen.iter().all(|a| *a == Action::Local), "{:?}", t.seen);
}

#[test]
fn decide_passes_after_the_net_has_ended() {
    use higanr_core::wire::ByeReason;
    use keys::Z;
    let mut t = Alone::new(7, vec![act(0, 7, 20, Z, 1)]);
    t.tick(10);
    assert_eq!(t.g.screen, 7);
    t.np.close(ByeReason::Cancelled);
    assert_eq!(t.np.phase(), Phase::Ended);
    t.tick(30);
    assert_eq!(t.g.screen, 8, "終えた後の決定が通らない: {:?}", t.g.screens);
    assert_eq!(t.np.stats().menu_pre_link_decides, 0);
    assert_eq!(t.seen.last(), Some(&Action::Local));
}

#[test]
fn decide_is_dropped_while_waiting_for_a_code_and_passes_after_giving_up() {
    use std::sync::Arc;
    let (mut na, _nb, clock) = FakeNet::pair(addr(1), addr(2), 1_000);
    let cfg = Config {
        check: CheckSource::InputDigest,
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        ..Config::default()
    };
    let slot = Arc::new(super::LinkSlot::new());
    let mut np = NetPlay::with_code_slot(slot.clone(), cfg, &mut na);
    let mut g = MenuGame::new(0, Vec::new(), &[0, 1, 2, 3]);
    g.set_screen(7, false);
    g.sub = 1;
    g.spam = Some((7, keys::Z, 5));
    let mut t = clock.get();
    let mut tick = |g: &mut MenuGame, np: &mut NetPlay, n: u32| {
        for _ in 0..n {
            clock.set(clock.get().max(t));
            g.tick(np, &mut na);
            t += FRAME_US;
        }
    };
    tick(&mut g, &mut np, 60);
    assert_eq!(g.screen, 7, "コードを待っている間に決定が通った");
    assert!(np.stats().menu_pre_link_decides >= 10);
    assert_eq!(np.phase(), Phase::Unconnected);
    slot.give_up();
    tick(&mut g, &mut np, 10);
    assert_eq!(np.phase(), Phase::Ended);
    assert_eq!(g.screen, 8, "諦めた後も決定が落ちる: {:?}", g.screens);
}

#[test]
fn restarts_of_a_box_without_candidates_are_not_a_stalled_handshake() {
    use std::sync::Arc;
    let mk = |peers_empty: bool| {
        let (mut na, _nb, clock) = FakeNet::pair(addr(1), addr(2), 1_000);
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            ..Config::default()
        };
        let slot = Arc::new(super::LinkSlot::new());
        let mut np = NetPlay::with_code_slot(slot.clone(), cfg, &mut na);
        let mut c = link::Config::single(
            handshake::Config {
                session_id: 9,
                role_nonce: 0xF000,
                latency: 2,
                name: Name::new("t"),
                host_seat: Seat::P1,
            },
            higanr_core::auth::Secret([5; 16]),
            addr(2),
            higanr_core::seal::KeySeed([0x4B; 32]),
        );
        if peers_empty {
            c.peers = higanr_core::candidate::Candidates::EMPTY;
        }
        slot.offer(c);
        for _ in 0..150 {
            clock.set(clock.get() + 100_000);
            np.tend(&mut na);
        }
        np.restarts()
    };
    assert!(
        mk(false).count >= 1,
        "対照: 候補のあるセッションは 10 秒で作り直される"
    );
    assert_eq!(
        mk(true),
        higanr_core::link::Restarts::default(),
        "候補の無いセッションの作り直しを数えた"
    );
}

#[test]
fn a_host_that_mashes_decide_before_the_link_does_not_run_ahead() {
    use keys::Z;
    for delay_ms in [1u64, 16, 40] {
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 8, 12, Z, 1),
            act(0, 16, 10, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 20, Z, 1)];
        let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
        a.spam = Some((7, Z, 4));
        for g in [&mut a, &mut b] {
            g.max_matches = 1;
        }
        let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), delay_ms * 1_000);
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            ..Config::default()
        };
        let mut npa = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
        let mut npb = NetPlay::new(link_cfg(1, 1), cfg, &mut nb);
        let ctx = format!("片道 {delay_ms} ms");
        // B が動き出す tick（それまで A は繋がらない。A は tick 31 で 7 へ入る）。
        const B_STARTS: u32 = 120;
        let mut slot = clock.get();
        let mut done = false;
        for t in 0..6_000u32 {
            clock.set(clock.get().max(slot));
            a.tick(&mut npa, &mut na);
            if t < B_STARTS {
                if t == B_STARTS - 1 {
                    assert_eq!(a.screens, vec![6, 7], "{ctx}: 繋がる前に A の画面が進んだ");
                    assert_eq!(npa.phase(), Phase::Unconnected, "{ctx}");
                    assert!(npa.stats().menu_pre_link_decides >= 10, "{ctx}");
                }
                slot += FRAME_US;
                continue;
            }
            clock.set(clock.get().max(slot + FRAME_US / 2));
            b.tick(&mut npb, &mut nb);
            slot += FRAME_US;
            let back =
                |g: &MenuGame| g.matches >= 1 && g.cur == 1 && g.screen == 8 && !g.pending_auto8;
            if back(&a) && back(&b) && a.plan_done() && b.plan_done() {
                done = true;
                break;
            }
        }
        assert!(
            done,
            "{ctx}: 試合まで行かない（A の画面 {:?} / B の画面 {:?} / A の終わり {:?} B の終わり {:?}）",
            a.screens,
            b.screens,
            npa.end(),
            npb.end()
        );
        assert_eq!(a.screens, b.screens, "{ctx}: 通った画面の列");
        assert_eq!((a.matches, b.matches), (1, 1), "{ctx}");
        let steps = |np: &NetPlay| {
            np.step_log()
                .iter()
                .map(|s| (s.step.step, s.step.kind, s.step.value, s.step.screen))
                .collect::<Vec<_>>()
        };
        assert_eq!(steps(&npa), steps(&npb), "{ctx}: 入った押下の列");
        assert_eq!((a.agreed.len(), b.agreed.len()), (1, 1), "{ctx}");
        assert_eq!(
            (a.chara, a.color, a.difficulty, a.map),
            (b.chara, b.color, b.difficulty, b.map),
            "{ctx}: 試合に入った値"
        );
        assert_eq!(a.played, b.played, "{ctx}: 試合の列");
        assert!(!a.played.is_empty(), "{ctx}");
        assert_eq!((a.ignored_presses, b.ignored_presses), (0, 0), "{ctx}");
        assert_eq!((a.on_input_allocs, b.on_input_allocs), (0, 0), "{ctx}");
        for np in [&npa, &npb] {
            assert_eq!(np.stats().menu_lost, 0, "{ctx}");
        }
        assert_eq!(
            npb.stats().menu_pre_link_decides,
            0,
            "{ctx}: B は決定を叩いていない"
        );
    }
}

// ---- 合わせ ----
// 2 台の偽のゲームの最初のカーソルは違える。「決定の前の静かな間」に取ったカーソル（`MenuGame::snaps`）を比べるので、
// 決定の tick のカーソル合わせ（`apply_plan`）では作れない値になる。

/// 2 台を同じ時計で 1 tick ずつ進める（A、その半フレーム後に B）。
struct Duo {
    a: MenuGame,
    b: MenuGame,
    npa: NetPlay,
    npb: NetPlay,
    na: FakeNet,
    nb: FakeNet,
    clock: std::rc::Rc<std::cell::Cell<u64>>,
    slot: u64,
}

impl Duo {
    fn new(a: MenuGame, b: MenuGame, delay_ms: u64, drop_a: u32, drop_b: u32) -> Self {
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            ..Config::default()
        };
        Self::with_config(a, b, delay_ms, drop_a, drop_b, cfg)
    }

    fn with_config(
        a: MenuGame,
        b: MenuGame,
        delay_ms: u64,
        drop_a: u32,
        drop_b: u32,
        cfg: Config,
    ) -> Self {
        let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), delay_ms * 1_000);
        na.drop_every = drop_a;
        nb.drop_every = drop_b;
        let npa = NetPlay::new(link_cfg(2, 0xF000), cfg, &mut na);
        let npb = NetPlay::new(link_cfg(1, 1), cfg, &mut nb);
        let slot = clock.get();
        Duo {
            a,
            b,
            npa,
            npb,
            na,
            nb,
            clock,
            slot,
        }
    }

    fn tick(&mut self) {
        if self.clock.get() < self.slot {
            self.clock.set(self.slot);
        }
        self.a.tick(&mut self.npa, &mut self.na);
        self.clock
            .set(self.clock.get().max(self.slot + FRAME_US / 2));
        self.b.tick(&mut self.npb, &mut self.nb);
        self.slot += FRAME_US;
    }

    fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.tick();
        }
    }

    fn until_done(&mut self, limit: u32) -> bool {
        let back = |g: &MenuGame| {
            g.matches >= g.max_matches
                && g.cur == 1
                && g.screen == 8
                && !g.pending_auto8
                && g.plan_done()
        };
        for _ in 0..limit {
            self.tick();
            if back(&self.a) && back(&self.b) {
                return true;
            }
        }
        false
    }
}

/// 最初のカーソルを違えた 2 台（A: 難易度 1・キャラ [3, 8]・一覧は全部、B: 難易度 3・キャラ [5, 10]・一覧は 4 つだけ）。
fn differing_games(pa: Vec<Act>, pb: Vec<Act>) -> (MenuGame, MenuGame) {
    let a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    (a, b)
}

#[test]
fn cursors_are_aligned_on_every_selection_screen_before_any_decision() {
    use keys::Z;
    let mut total_aligns = 0usize;
    for (delay_ms, drop_a, drop_b, b_anim) in [
        (1u64, 0u32, 0u32, 8u32),
        (16, 0, 0, 8),
        (40, 0, 0, 8),
        (12, 5, 7, 8),
        (25, 7, 5, 8),
        (16, 0, 0, 14),
        (40, 0, 0, 14),
    ] {
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 200, Z, 1),
            act(0, 8, 200, Z, 1),
            act(0, 16, 200, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 260, Z, 1)];
        let (a, mut b) = differing_games(pa, pb);
        b.anim_len = b_anim;
        let mut d = Duo::new(a, b, delay_ms, drop_a, drop_b);
        let ctx = format!("片道 {delay_ms} ms・落とす {drop_a}/{drop_b}・B の演出 {b_anim}");
        assert!(d.until_done(20_000), "{ctx}: 台本を使い切っていない");
        assert_eq!(
            d.a.snaps,
            vec![(7, 1, [0; 2]), (8, 0, [3, 8]), (16, 14, [0; 2])],
            "{ctx}: A（ホスト）の値は変わらない"
        );
        assert_eq!(d.b.snaps, d.a.snaps, "{ctx}: B が A の値へ寄っていない");
        for np in [&d.npa, &d.npb] {
            assert_eq!(np.stats().menu_align_miss, 0, "{ctx}");
        }
        total_aligns += d
            .npb
            .step_log()
            .iter()
            .filter(|s| s.step.kind == StepKind::Align)
            .count();
    }
    assert_eq!(total_aligns, 21);
}

#[test]
fn a_guest_entering_late_takes_the_hosts_current_cursor() {
    use keys::{DOWN, Z};
    for delay_ms in [1u64, 16, 40] {
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 12, DOWN, 1),
            act(0, 7, 8, DOWN, 1),
        ];
        let pb = vec![act(0, 6, 230, Z, 1)];
        let (a, mut b) = differing_games(pa, pb);
        b.saved_diff = 0;
        let mut d = Duo::new(a, b, delay_ms, 0, 0);
        d.ticks(700);
        assert_eq!(d.a.snaps, vec![(7, 3, [0; 2])], "片道 {delay_ms}: A");
        assert_eq!(
            d.b.snaps, d.a.snaps,
            "片道 {delay_ms}: B が A の今の値になる"
        );
    }
}

#[test]
fn a_guest_entering_first_does_not_move_until_aligned() {
    use keys::{DOWN, Z};
    for delay_ms in [1u64, 16, 40] {
        let pa = vec![act(0, 6, 300, Z, 1)];
        let pb = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 12, DOWN, 1),
            act(0, 7, 8, DOWN, 1),
        ];
        let (a, mut b) = differing_games(pa, pb);
        b.saved_diff = 0;
        let mut d = Duo::new(a, b, delay_ms, 0, 0);
        d.ticks(700);
        let on7: Vec<u32> =
            d.b.changes
                .iter()
                .filter(|c| c.1 == 7)
                .map(|c| c.2)
                .collect();
        assert_eq!(on7, vec![1], "片道 {delay_ms}: B のカーソルの変化");
        assert_eq!((d.a.cursor, d.b.cursor), (1, 1), "片道 {delay_ms}");
    }
}

#[test]
fn a_late_guest_sees_the_hosts_last_move_within_a_few_ticks() {
    use keys::{DOWN, Z};
    for delay_ms in [1u64, 16, 40] {
        let mut pa = vec![act(0, 6, 30, Z, 1)];
        pa.extend((0..40).map(|_| act(0, 7, 3, DOWN, 1)));
        pa.push(act(0, 7, 500, DOWN, 1));
        let pb = vec![act(0, 6, 330, Z, 1)];
        let (a, b) = differing_games(pa, pb);
        let mut d = Duo::new(a, b, delay_ms, 0, 0);
        d.ticks(900);
        assert!(d.a.plan_done());
        let (ta, tb) = (
            d.a.changes.last().expect("A が動いた").0,
            d.b.changes.last().expect("B が動いた").0,
        );
        let bound = (delay_ms * 1_000).div_ceil(FRAME_US) as u32 + 3;
        assert!(
            tb >= ta && tb - ta <= bound,
            "片道 {delay_ms} ms: A の最後の動き tick {ta}、B が映したのは tick {tb}（上限 +{bound}）"
        );
        assert_eq!(
            d.a.cursor, d.b.cursor,
            "片道 {delay_ms}: 最後にカーソルが揃う"
        );
    }
}

#[test]
fn cancel_does_not_align_the_cursor() {
    use keys::{X, Z};
    let pa = vec![act(0, 6, 30, Z, 1), act(0, 7, 150, X, 1)];
    let pb = vec![act(0, 6, 30, Z, 1)];
    let (a, b) = differing_games(pa, pb);
    let mut d = Duo::new(a, b, 8, 0, 0);
    let mut drifted = false;
    for _ in 0..400 {
        d.tick();
        if !drifted && d.a.screen == 7 && d.a.screen_age >= 100 {
            assert_eq!((d.a.cursor, d.b.cursor), (1, 1), "合わせの後は同じ");
            d.b.cursor = 0;
            drifted = true;
        }
    }
    assert!(drifted);
    assert_eq!((d.a.screen, d.b.screen), (6, 6), "取消で 6 へ戻った");
    assert_eq!(d.a.saved_diff, 1);
    assert_eq!(
        d.b.saved_diff, 0,
        "B の設定の難易度は自分のカーソルのまま（取消で相手の値を書かない）"
    );
}

#[test]
fn a_guest_decide_before_the_alignment_waits_and_takes_the_aligned_value() {
    use keys::Z;
    for delay_ms in [1u64, 16, 40] {
        let pa = vec![act(0, 6, 60, Z, 1)];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 7, 12, Z, 1)];
        let (a, b) = differing_games(pa, pb);
        let mut d = Duo::new(a, b, delay_ms, 0, 0);
        d.ticks(400);
        let steps: Vec<_> = d
            .npb
            .step_log()
            .iter()
            .map(|s| (s.step.step, s.step.kind, s.step.value, s.step.screen))
            .collect();
        assert_eq!(
            steps.get(..2),
            Some(&[(0, StepKind::Align, 1, 7), (1, StepKind::Decide, 1, 7)][..]),
            "片道 {delay_ms}: {steps:?}"
        );
        assert_eq!(d.a.screens, d.b.screens, "片道 {delay_ms}");
        assert_eq!(d.npb.stats().menu_lost, 0);
    }
}

#[test]
fn the_select_row_shows_from_the_chara_screen_on_and_the_guest_follows_the_host_flag() {
    use higanr_core::delay_pick::AUTO;
    use higanr_core::wire::Mode;
    use keys::Z;
    let pa = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 100, Z, 1),
        act(0, 8, 500, Z, 1),
        act(0, 16, 60, Z, 1),
    ];
    let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 560, Z, 1)];
    let (a, b) = differing_games(pa, pb);
    let mut d = Duo::new(a, b, 5, 0, 0);
    // 画面ごとの「出た・出なかった」の数（空振りでないこと）と、画面が同じまま続いた tick の数。
    let (mut on_select, mut on16, mut off_other, mut in_match, mut stable) =
        (0u32, 0u32, 0u32, 0u32, [0u32; 2]);
    let mut last_screen = [0u32; 2];
    let mut stage = 0;
    for tick in 0..20_000u32 {
        d.tick();
        let hud = [d.npa.hud_facts(), d.npb.hud_facts()];
        let games = [&d.a, &d.b];
        let phases = [d.npa.phase(), d.npb.phase()];
        for i in 0..2 {
            let g = games[i];
            stable[i] = if g.screen == last_screen[i] {
                stable[i] + 1
            } else {
                0
            };
            last_screen[i] = g.screen;
            let who = ["A（ホスト）", "B（ゲスト）"][i];
            match phases[i] {
                Phase::Selecting if g.cur == 1 && stable[i] >= 3 && matches!(g.screen, 8 | 16) => {
                    let h = hud[i].unwrap_or_else(|| {
                        panic!("{who} tick {tick}: 画面 {} で行が出ない", g.screen)
                    });
                    assert_eq!(h.format, Some(Mode::Delay), "{who}: 形式の語");
                    assert!(h.rewound.is_none(), "{who}: 選択の行に戻しは無い");
                    assert!(h.names.is_some(), "{who}: 名前の帯の材料");
                    on_select += 1;
                    on16 += u32::from(g.screen == 16);
                }
                // 画面 6・7 は、場面に入ったばかりの tick（マッチから戻った tick。`stable` が溜まらない）も出さない（前の選択の番号が残っていれば出る）。
                Phase::Selecting if g.cur == 1 && matches!(g.screen, 6 | 7) => {
                    assert!(
                        hud[i].is_none(),
                        "{who} tick {tick}: 画面 {} に行が出た",
                        g.screen
                    );
                    off_other += 1;
                }
                Phase::Lockstep => {
                    let h = hud[i].unwrap_or_else(|| panic!("{who}: 試合中に行が出ない"));
                    assert_eq!(h.format, None, "{who}: 試合中の行に形式の語は付かない");
                    in_match += 1;
                }
                _ => {}
            }
        }
        if d.a.screen == 8 && d.b.screen == 8 && stable[0] >= 60 && stable[1] >= 60 {
            let hud_b = |d: &mut Duo| d.npb.hud_facts().expect("ゲストのキャラセレクトの行");
            match stage {
                0 => {
                    let h = hud_b(&mut d);
                    assert!(!h.auto, "最初は旗が降りている");
                    d.npa.link.connected_mut().unwrap().press_delay(AUTO);
                    stage = 1;
                    stable = [0; 2];
                }
                1 => {
                    assert!(hud_b(&mut d).auto, "ホストの 9 がゲストの (A) にならない");
                    assert!(d.npa.hud_facts().unwrap().auto);
                    d.npa.link.connected_mut().unwrap().press_delay(5);
                    stage = 2;
                    stable = [0; 2];
                }
                2 => {
                    let h = hud_b(&mut d);
                    assert!(!h.auto, "ホストが数字を押しても旗が降りない");
                    assert_eq!(h.latency, Some(5), "選択の間にホストが押した D が出ない");
                    d.npb.link.connected_mut().unwrap().press_delay(3);
                    let h = hud_b(&mut d);
                    assert_eq!(h.latency, Some(3), "要求がホストの値に負けた");
                    assert!(!h.auto);
                    d.npb.link.connected_mut().unwrap().press_delay(AUTO);
                    assert!(hud_b(&mut d).auto, "auto の要求で (A) が出ない");
                    stage = 3;
                }
                _ => {}
            }
        }
        if stage == 3 && d.a.matches >= d.a.max_matches && d.b.matches >= d.b.max_matches {
            break;
        }
    }
    assert_eq!(stage, 3, "キャラセレクトに落ち着かなかった");
    assert!(
        on_select > 100 && on16 > 0 && off_other > 100,
        "空振り: 選択 {on_select} / マップ {on16} / 画面 6・7 {off_other}"
    );
    assert!(in_match > 10, "試合を見ていない: {in_match}");
}

#[test]
fn a_start_choice_on_the_slot_becomes_the_hosts_auto_flag_in_the_first_match() {
    use std::sync::Arc;

    use higanr_core::delay_pick::DelayChoice;
    use keys::Z;
    let run_with = |host_start: Option<DelayChoice>, guest_start: Option<DelayChoice>| {
        let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), 5_000);
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            delay_pick: true,
            ..Config::default()
        };
        let (sa, sb) = (
            Arc::new(super::LinkSlot::new()),
            Arc::new(super::LinkSlot::new()),
        );
        let mut npa = NetPlay::with_code_slot(sa.clone(), cfg, &mut na);
        let mut npb = NetPlay::with_code_slot(sb.clone(), cfg, &mut nb);
        sa.set_start(host_start);
        sb.set_start(guest_start);
        sa.offer(link_cfg(2, 0xF000));
        sb.offer(link_cfg(1, 1));
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 100, Z, 1),
            act(0, 8, 100, Z, 1),
            act(0, 16, 60, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 160, Z, 1)];
        let (mut a, mut b) = differing_games(pa, pb);
        let mut slot_t = clock.get();
        let mut in_match = 0u32;
        for _ in 0..6_000 {
            if clock.get() < slot_t {
                clock.set(slot_t);
            }
            a.tick(&mut npa, &mut na);
            clock.set(clock.get().max(slot_t + FRAME_US / 2));
            b.tick(&mut npb, &mut nb);
            slot_t += FRAME_US;
            if npa.phase() == Phase::Lockstep && npb.phase() == Phase::Lockstep {
                for np in [&mut npa, &mut npb] {
                    let h = np.hud_facts().expect("試合中の行");
                    assert!(
                        h.latency.is_some_and(|d| d <= 8),
                        "遅れに auto の 9 が出た: {h:?}"
                    );
                }
                in_match += 1;
                if in_match >= 8 {
                    break;
                }
            }
        }
        assert!(
            in_match >= 8,
            "最初の試合に入らない: A {:?} / B {:?} / 画面 {:?} {:?} / {:?}",
            npa.phase(),
            npb.phase(),
            a.screens,
            b.screens,
            npa.end()
        );
        let (ca, cb) = (npa.connected().unwrap(), npb.connected().unwrap());
        assert_eq!(ca.role(), Role::Host, "A がホストのはず");
        (ca.auto_on(), cb.auto_on(), cb.shown_auto())
    };
    assert_eq!(
        run_with(Some(DelayChoice::Auto), None),
        (true, false, true),
        "ホストの auto が初めの値にならない"
    );
    for host in [Some(DelayChoice::Fixed(4)), None] {
        assert_eq!(run_with(host, None), (false, false, false), "{host:?}");
    }
    assert_eq!(
        run_with(None, Some(DelayChoice::Auto)),
        (false, false, false),
        "ゲストの start がホストと競った"
    );
}

// ---- 対戦の形式の切替（キャラセレクトの Tab）----

fn press_tab(g: &mut MenuGame) {
    g.digit_script = Some(crate::digit_keys::TAB_BIT);
}

fn tick_until(d: &mut Duo, limit: u32, what: &str, cond: impl Fn(&Duo) -> bool) {
    for _ in 0..limit {
        if cond(d) {
            return;
        }
        d.tick();
    }
    assert!(
        cond(d),
        "{what}: {limit} tick 経っても成り立たない（画面 {} {} / A {:?} B {:?}）",
        d.a.screen,
        d.b.screen,
        d.npa.connected().map(|c| c.mode()),
        d.npb.connected().map(|c| c.mode())
    );
}

fn modes(d: &Duo) -> (higanr_core::wire::Mode, higanr_core::wire::Mode) {
    (
        d.npa.connected().unwrap().mode(),
        d.npb.connected().unwrap().mode(),
    )
}

#[test]
fn tab_in_the_chara_select_flips_the_format_for_both_sides_in_the_same_step() {
    use crate::digit_keys::TAB_BIT;
    use higanr_core::wire::Mode;
    use keys::Z;
    for (delay_ms, drop_a, drop_b) in [(5u64, 0u32, 0u32), (25, 7, 5)] {
        let tag = format!("片道 {delay_ms} ms ロス {drop_a}/{drop_b}");
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 400, Z, 1),
            act(0, 8, 4000, Z, 1),
            act(0, 16, 60, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 4060, Z, 1)];
        let (a, b) = differing_games(pa, pb);
        let mut d = Duo::new(a, b, delay_ms, drop_a, drop_b);
        tick_until(&mut d, 3_000, "難易度の画面", |d| {
            d.a.screen == 7 && d.b.screen == 7 && d.a.screen_age >= 20 && d.b.screen_age >= 20
        });
        press_tab(&mut d.a);
        press_tab(&mut d.b);
        d.ticks(30);
        assert_eq!(
            modes(&d),
            (Mode::Delay, Mode::Delay),
            "{tag}: 画面 7 の Tab"
        );
        assert!(
            d.npa.stats().tab_ignored >= 1 && d.npb.stats().tab_ignored >= 1,
            "{tag}: 効かなかった Tab を数えない"
        );
        assert_eq!(
            d.npa.connected().unwrap().format_stats().applied,
            0,
            "{tag}"
        );
        tick_until(&mut d, 6_000, "キャラセレクトに落ち着く", |d| {
            d.a.screen == 8 && d.b.screen == 8 && d.a.screen_age >= 200 && d.b.screen_age >= 200
        });
        d.npa.link.connected_mut().unwrap().press_delay(5);
        d.ticks(60);
        let flip = |d: &mut Duo, who: &str, pressers: [bool; 2], want: Mode| {
            if pressers[0] {
                press_tab(&mut d.a);
            }
            if pressers[1] {
                press_tab(&mut d.b);
            }
            tick_until(d, 1_500, who, |d| modes(d) == (want, want));
            d.ticks(120);
            assert_eq!(modes(d), (want, want), "{who}: 落ち着いた後に動いた");
            let (fa, fb) = (
                d.npa.connected().unwrap().format_stats(),
                d.npb.connected().unwrap().format_stats(),
            );
            assert_eq!(fa.applied, fb.applied, "{who}: 入れた数が両者で違う");
            assert_eq!(fa.changed, fb.changed, "{who}");
            let (ra, rb) = (
                fa.log[(fa.applied as usize - 1) % higanr_core::link::FORMAT_LOG],
                fb.log[(fb.applied as usize - 1) % higanr_core::link::FORMAT_LOG],
            );
            assert_eq!(
                (ra.seg, ra.value, ra.from),
                (rb.seg, rb.value, rb.from),
                "{who}: 切替の記録が両者で違う"
            );
            assert_eq!(ra.value.mode, want, "{who}");
            assert_eq!(
                ra.value.delay, 5,
                "{who}: ホストの D が切替の STEP に載らない"
            );
            (ra.by_peer, rb.by_peer, fa.applied)
        };
        let (host_view, guest_view, n) =
            flip(&mut d, "ゲストの Tab", [false, true], Mode::Rollback);
        assert_eq!((host_view, guest_view, n), (true, false, 1), "{tag}");
        let (host_view, guest_view, n) = flip(&mut d, "ホストの Tab", [true, false], Mode::Delay);
        assert_eq!((host_view, guest_view, n), (false, true, 2), "{tag}");
        let (_, _, n) = flip(&mut d, "同時押し", [true, true], Mode::Rollback);
        assert_eq!(
            n, 3,
            "{tag}: 同時押しで 2 回替わった（または替わらなかった）"
        );
        let (_, _, n) = flip(&mut d, "ゲストの Tab（戻す）", [false, true], Mode::Delay);
        assert_eq!(n, 4, "{tag}");
        let (sa, sb) = (d.npa.stats(), d.npb.stats());
        assert_eq!((sa.format_steps, sb.format_steps), (4, 4), "{tag}");
        assert_eq!((sa.format_refused, sb.format_refused), (0, 0), "{tag}");
        assert!(
            d.npa
                .step_log()
                .iter()
                .filter(|r| r.step.kind == StepKind::Format)
                .count()
                == d.npb
                    .step_log()
                    .iter()
                    .filter(|r| r.step.kind == StepKind::Format)
                    .count(),
            "{tag}"
        );
        let _ = TAB_BIT;
        let (ta, tb) = (
            crate::net_watch::NetStatus::new(),
            crate::net_watch::NetStatus::new(),
        );
        ta.publish(&d.npa);
        tb.publish(&d.npb);
        let (xa, xb) = (ta.fmt_text(), tb.fmt_text());
        for (who, t) in [("A", &xa), ("B", &xb)] {
            assert!(t.contains("入れた切替の STEP 4"), "{tag} {who}: {t}");
            assert!(t.contains("rollback ができる組 はい"), "{tag} {who}: {t}");
            assert!(
                t.contains("seg 0 で delay → rollback（D 5・auto なし。"),
                "{tag} {who}: {t}"
            );
            assert!(
                t.contains("seg 0 で rollback → delay（D 5・auto なし。"),
                "{tag} {who}: {t}"
            );
        }
        assert!(
            xa.contains("HOST_STATE）: 送った ") && !xa.contains("送った 0 "),
            "{tag}: {xa}"
        );
        assert!(
            xb.contains("受けた ") && !xb.contains("受けた 0 "),
            "{tag}: {xb}"
        );
        assert!(d.until_done(30_000), "{tag}: 試合に入って戻らない");
        assert_eq!(d.a.played, d.b.played, "{tag}: 両者の試合の列が違う");
        assert!(!d.a.played.is_empty(), "{tag}: 試合を動かしていない");
    }
}

#[test]
fn tab_does_nothing_when_one_side_cannot_roll_back_and_the_row_says_delay_only() {
    use higanr_core::wire::Mode;
    use keys::Z;
    for (a_can, b_can) in [(false, true), (true, false), (true, true)] {
        let tag = format!("A {a_can} × B {b_can}");
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 100, Z, 1),
            act(0, 8, 1500, Z, 1),
            act(0, 16, 60, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 1560, Z, 1)];
        let (a, b) = differing_games(pa, pb);
        let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), 5_000);
        let cfg = |can| Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            rollback_able: can,
            ..Config::default()
        };
        let npa = NetPlay::new(link_cfg(2, 0xF000), cfg(a_can), &mut na);
        let npb = NetPlay::new(link_cfg(1, 1), cfg(b_can), &mut nb);
        let slot = clock.get();
        let mut d = Duo {
            a,
            b,
            npa,
            npb,
            na,
            nb,
            clock,
            slot,
        };
        tick_until(&mut d, 6_000, "キャラセレクトに落ち着く", |d| {
            d.a.screen == 8 && d.b.screen == 8 && d.a.screen_age >= 200 && d.b.screen_age >= 200
        });
        let able = a_can && b_can;
        for np in [&d.npa, &d.npb] {
            assert_eq!(np.connected().unwrap().rollback_able(), able, "{tag}");
        }
        let h = [d.npa.hud_facts(), d.npb.hud_facts()];
        for (who, h) in ["A", "B"].into_iter().zip(h) {
            let h = h.unwrap_or_else(|| panic!("{tag} {who}: 行が出ない"));
            assert_eq!(h.format, Some(Mode::Delay), "{tag} {who}");
            assert_eq!(h.delay_only, !able, "{tag} {who}: 「delay only」の印");
        }
        press_tab(&mut d.a);
        press_tab(&mut d.b);
        d.ticks(200);
        if able {
            assert_eq!(modes(&d), (Mode::Rollback, Mode::Rollback), "{tag}");
            let h = d.npa.hud_facts().unwrap();
            assert_eq!(
                (h.format, h.delay_only),
                (Some(Mode::Rollback), false),
                "{tag}"
            );
        } else {
            assert_eq!(
                modes(&d),
                (Mode::Delay, Mode::Delay),
                "{tag}: できない組で替わった"
            );
            for np in [&d.npa, &d.npb] {
                assert_eq!(np.connected().unwrap().format_stats().applied, 0, "{tag}");
                assert_eq!(np.stats().format_asked, 0, "{tag}: 要求を作った");
                assert!(
                    np.stats().tab_ignored >= 1,
                    "{tag}: 効かなかった Tab を数えない"
                );
            }
        }
    }
}

#[test]
fn only_ctrl_tab_flips_the_format_and_alt_or_win_stops_it() {
    use crate::digit_keys::{CTRL_BIT, MOD_BIT, TAB_BIT};
    use higanr_core::wire::Mode;
    use keys::Z;
    let pa = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 100, Z, 1),
        act(0, 8, 3000, Z, 1),
    ];
    let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 3060, Z, 1)];
    let (a, b) = differing_games(pa, pb);
    let cfg = Config {
        check: CheckSource::InputDigest,
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        delay_pick: true,
        ..Config::default()
    };
    let mut d = Duo::with_config(a, b, 5, 0, 0, cfg);
    tick_until(&mut d, 6_000, "キャラセレクトに落ち着く", |d| {
        d.a.screen == 8 && d.b.screen == 8 && d.a.screen_age >= 200 && d.b.screen_age >= 200
    });
    assert!(
        d.npa.delay_pick().is_some(),
        "本物のキーの経路（pick）が無い"
    );
    d.a.digit_held = Some(0);
    d.ticks(2);
    for held in [
        TAB_BIT,
        TAB_BIT | MOD_BIT,
        TAB_BIT | CTRL_BIT | MOD_BIT,
        TAB_BIT | CTRL_BIT | MOD_BIT,
        TAB_BIT | CTRL_BIT,
        TAB_BIT | CTRL_BIT,
        0,
    ] {
        d.a.digit_held = Some(held);
        d.ticks(40);
        assert_eq!(modes(&d), (Mode::Delay, Mode::Delay), "読み {held:#x}");
    }
    assert_eq!(
        d.npa.stats().tab_presses,
        0,
        "Ctrl の無い Tab・Alt か Win つきの Tab を押下に数えた"
    );
    assert_eq!(d.npa.connected().unwrap().format_stats().applied, 0);
    d.a.digit_held = Some(TAB_BIT | CTRL_BIT);
    d.ticks(40);
    d.a.digit_held = Some(0);
    tick_until(&mut d, 1_500, "Ctrl+Tab", |d| {
        modes(d) == (Mode::Rollback, Mode::Rollback)
    });
    assert_eq!(d.npa.stats().tab_presses, 1);
}

/// キャラセレクトに落ち着いた 2 台。`scripted` は A の実行が台本の付いたものか、`pick` は A が Ctrl+数字を読む実行か。
fn chara_select_duo(scripted: bool, pick: bool) -> Duo {
    use keys::Z;
    let pa = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 100, Z, 1),
        act(0, 8, 3000, Z, 1),
    ];
    let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 3060, Z, 1)];
    let (mut a, b) = differing_games(pa, pb);
    a.scripted = scripted;
    let cfg = Config {
        check: CheckSource::InputDigest,
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        delay_pick: pick,
        ..Config::default()
    };
    let mut d = Duo::with_config(a, b, 5, 0, 0, cfg);
    tick_until(&mut d, 6_000, "キャラセレクトに落ち着く", |d| {
        d.a.screen == 8 && d.b.screen == 8 && d.a.screen_age >= 200 && d.b.screen_age >= 200
    });
    d
}

/// A が `ticks` tick のあいだ語の Ctrl（`0x0100`）を押し、その間に自分のゲームへ入った語・相手へ届いた語に `0x0100` が残ったか。
fn skip_bit_leaks(d: &mut Duo, ticks: u32) -> (bool, bool) {
    const SKIP: u16 = 0x0100;
    let (na, nb) = (d.a.fed_menu.len(), d.b.fed_menu.len());
    d.a.presses.push(Press::new(|_| true, SKIP, ticks));
    d.ticks(ticks + 20);
    let has = |v: &[[u16; 3]]| v.iter().any(|w| w.iter().any(|x| x & SKIP != 0));
    (has(&d.a.fed_menu[na..]), has(&d.b.fed_menu[nb..]))
}

/// 選択の画面（キャラセレクト）では、キーボードの Ctrl が押されている間は語の Ctrl（`0x0100`。ゲームの「ランダム」）を落とす。
/// 自分のゲームへ入れる語にも相手へ送る語にも残らない。Ctrl が離れているときの `0x0100`（パッドのスキップ）は両方に残る。
#[test]
fn the_keyboard_ctrl_never_reaches_the_chara_select_but_a_pad_skip_does() {
    for ctrl_down in [true, false] {
        let mut d = chara_select_duo(false, true);
        d.a.ctrl_down = ctrl_down;
        let (mine, theirs) = skip_bit_leaks(&mut d, 20);
        assert_eq!(
            (mine, theirs),
            (!ctrl_down, !ctrl_down),
            "ctrl_down {ctrl_down}: 自分のゲームへ入った {mine} / 相手へ届いた {theirs}"
        );
    }
}

/// ゲームの Ctrl の読みはこちらのキーの読みより遅れうる。Ctrl を離しても、語の `0x0100` が 0 に戻るまでは落とし続ける。
/// 戻った後のパッドのスキップは通る。
#[test]
fn the_ctrl_drop_outlives_the_key_release_until_the_word_clears() {
    let mut d = chara_select_duo(false, true);
    let (na, nb) = (d.a.fed_menu.len(), d.b.fed_menu.len());
    d.a.ctrl_down = true;
    d.a.presses.push(Press::new(|_| true, 0x0100, 30));
    d.ticks(5);
    d.a.ctrl_down = false;
    d.ticks(40);
    let has = |v: &[[u16; 3]]| v.iter().any(|w| w.iter().any(|x| x & 0x0100 != 0));
    assert!(
        !has(&d.a.fed_menu[na..]) && !has(&d.b.fed_menu[nb..]),
        "Ctrl を離した後も語に残る間に 0x0100 が漏れた"
    );
    assert_eq!(
        skip_bit_leaks(&mut d, 20),
        (true, true),
        "語が 0 に戻った後のパッドのスキップまで落とした"
    );
}

/// 台本の実行と、Ctrl+数字を読まない実行（`pick` が無い）は、本物の Ctrl を読まない。
#[test]
fn the_real_ctrl_is_not_read_in_a_scripted_run_or_without_a_pick() {
    for (scripted, pick) in [(true, true), (false, false)] {
        let mut d = chara_select_duo(scripted, pick);
        d.a.ctrl_down = true;
        assert_eq!(
            skip_bit_leaks(&mut d, 20),
            (true, true),
            "scripted {scripted} pick {pick}: 本物の Ctrl を読んで落とした"
        );
    }
}

#[test]
fn script_digits_work_in_a_run_with_a_lat_plan_but_real_keys_are_not_read() {
    use crate::netplay::LatPlan;
    use keys::Z;
    let pa = vec![
        act(0, 6, 30, Z, 1),
        act(0, 7, 100, Z, 1),
        act(0, 8, 3000, Z, 1),
    ];
    let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 3060, Z, 1)];
    let (a, b) = differing_games(pa, pb);
    let cfg = Config {
        check: CheckSource::InputDigest,
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        // `delay_pick` を真にしても、`lat_plan` が空でないと `pick` は作られない（`ensure_pick`）。
        delay_pick: true,
        lat_plan: LatPlan::new(&[(1_000_000, 3)]).unwrap(),
        ..Config::default()
    };
    let mut d = Duo::with_config(a, b, 5, 0, 0, cfg);
    tick_until(&mut d, 6_000, "キャラセレクトに落ち着く", |d| {
        d.a.screen == 8 && d.b.screen == 8 && d.a.screen_age >= 200 && d.b.screen_age >= 200
    });
    assert!(
        d.npa.delay_pick().is_none(),
        "lat_plan があるのに pick がある"
    );
    d.a.digit_held = Some((1 << 4) | crate::digit_keys::CTRL_BIT);
    d.ticks(5);
    d.a.digit_held = Some(0);
    d.ticks(5);
    assert_eq!(
        d.npa.stats().digit_presses,
        0,
        "pick が無いのに本物のキーを読んだ"
    );
    d.a.digit_script = Some(1 << 5);
    d.ticks(5);
    assert_eq!(
        d.npa.stats().digit_presses,
        1,
        "lat_plan の実行で台本の数字が効かない"
    );
    assert_eq!(d.npa.connected().unwrap().shown_delay(), 5);
}

#[test]
fn after_a_format_switch_the_pick_does_not_push_the_start_recommendation() {
    use keys::Z;
    let run = |tabs: Vec<(u32, u32)>| {
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 100, Z, 1),
            act(0, 8, 400, Z, 1),
            act(0, 16, 60, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, 460, Z, 1)];
        let (mut a, b) = differing_games(pa, pb);
        a.tab_at = tabs;
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            require_net_selection: true,
            delay_pick: true,
            ..Config::default()
        };
        let mut d = Duo::with_config(a, b, 5, 0, 0, cfg);
        assert!(d.until_done(30_000), "試合に入って戻らない");
        assert_eq!(d.a.played, d.b.played);
        (d.npa.stats(), d.npa.connected().unwrap().format_stats())
    };
    let (control, cf) = run(Vec::new());
    assert_eq!(cf.applied, 0);
    assert!(
        control.delay_presses >= 1,
        "対照: 切り替えない delay 方式の接続は試合の始まりで推奨値を押すはず（押していなければこの検査は何も見ていない）: {control:?}"
    );
    let (flipped, ff) = run(vec![(8, 40), (8, 100)]);
    assert_eq!(ff.applied, 2, "Tab が 2 回通っていない");
    assert_eq!(
        flipped.delay_presses, 0,
        "切替の後に試合の始まりの推奨値を押した（切替の STEP の D を上書きする）: {flipped:?}"
    );
}

#[test]
fn a_format_step_does_not_swallow_the_guests_pending_decide_retry() {
    use keys::Z;
    const TAB_AGE: u32 = 60;
    for b_decide_age in [TAB_AGE - 1, TAB_AGE, TAB_AGE + 1, TAB_AGE + 2] {
        let tag = format!("ゲストの決定が画面 8 に入って {b_decide_age} tick 目");
        let pa = vec![
            act(0, 6, 30, Z, 1),
            act(0, 7, 100, Z, 1),
            act(0, 8, 1500, Z, 1),
            act(0, 16, 60, Z, 1),
        ];
        let pb = vec![act(0, 6, 30, Z, 1), act(0, 8, b_decide_age, Z, 1)];
        let (mut a, b) = differing_games(pa, pb);
        a.tab_at = vec![(8, TAB_AGE), (8, TAB_AGE + 300)];
        let mut d = Duo::new(a, b, 5, 0, 0);
        assert!(d.until_done(40_000), "{tag}: 試合に入って戻らない");
        assert_eq!(
            d.npa.connected().unwrap().format_stats().applied,
            2,
            "{tag}: 形式の STEP が 2 回入らない"
        );
        assert_eq!(d.a.played, d.b.played, "{tag}");
        assert!(!d.a.played.is_empty(), "{tag}");
    }
}
