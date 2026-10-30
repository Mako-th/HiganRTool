//! 入力の地点の判断（`netplay.rs`）が見るゲームのウィンドウの本物（pure。unsafe無し）。
//! 場面・自分の入力ワード・feed・自分のSELECT・揃った値の書き込み・チャージ型・CHECKの値を、ゲームのメモリと関数（`NetIo`の向こう。本物は`script_win::RealTickIo`、検査では偽物）で扱う。
//! ゲームのスレッドで走るので、確保・ファイル・ロック待ち・`unwrap`を使わない。
//!
//! - **自分の入力ワード**: 台本があればlockstepのフレームごとに台本を1 tick進めて自分のサイドの入力ワードを使う（相手のサイドの入力ワードは捨てる）。台本が終わっていれば（か無ければ）ローカル入力（素のpoll `0x0042B850`のサイド0。照合が合わなかった実行は`0x0042BE40`）。
//!   台本の無い実行（人の入力）は、選択の画面でもマッチでもこの経路（`script`が`None`）を通る。人の押下は選択の画面では`NetPlay::menu_tick`が押した瞬間からSTEP_REQ / STEPにする（台本の押下と同じ経路）。
//! - **選択の画面**: MainMenuのフィールド（画面・substate・移る途中・カーソル・キャラのカーソルとサイドの旗）は例外を出さない読み方で読む。人間 対 人間のフラグ`0x4A7EA4`とマップの一覧`0x4AC8C4`は直に読む（`net_select::MenuView`）。押下を反映するtickには、カーソルをMainMenuへ例外を出さない書き方で書く。
//! - **書く**のは相手のサイドのキャラ・色・初期ライフ、合意したマップ・難易度、シード（`set_state`）、開始のフックの中のコピーのチャージ型、選択の画面のカーソルだけ。書く前に範囲を見る（`check_agreed`）。1つでも外れていれば1つも書かない。
//!   CPUレベルは書かない（読み込みスレッドが難易度から書き直す）。オプションブロック`0x004B3488`には書かない（終了時にth09.cfgへ書き出される）。
#![forbid(unsafe_code)]

use higanr_core::end_pick;
use higanr_core::input_model::{InputState, MATCH_SEND_MASK, keys};
use higanr_core::lockstep::Role;
use higanr_core::return_flow::{MenuState, PlayView};
use higanr_core::script::{Engine, Status};
use higanr_core::selection::Agreed;
use higanr_core::wire::{ByeReason, Seat, Select};

use crate::feeds::{IM_BYTES, im_from_bytes};
use crate::net_config::AfterScript;
use crate::net_portrait::{self, Followed, Locked, Plan, PortraitCall, PortraitEnv};
use crate::net_select::MenuView;
use crate::netplay::{Action, BadValue, Game, watched_agreed};
use crate::scripted::{self, GameAddrs, Stepped, TickIo, read_obs};
use crate::th09_addr::{
    APP_ACTIVE, CHARA_MAX, CHARA_UNLOCK, CHARGE_MAX, COLOR_MAX, DIFFICULTY, DIFFICULTY_MAX,
    END_MENU, GLOBAL_STATE, GLOBAL_STATE_REPLAY, INPUT_MANAGERS, LIFE_MAX, LIFE_MIN, LOADING,
    MAIN_MENU_PTR, MAP, MAP_LIST, MAP_LIST_LEN, MAP_MAX, MAP_RANDOM_ANY, MAP_RANDOM_OF_TWO,
    MATCH_END_MENU, MENU_CHARA_CURSOR, MENU_CHARA_DECIDED, MENU_CURSOR, MENU_OBJ_FRAMES,
    MENU_OBJ_STATE, MENU_PARTS_PTR, MENU_SCREEN_ID, MENU_SUBSTATE, MENU_TRANSITIONING, OPT_CHARGE,
    OPTION_DIGEST_RANGES, OPTIONS_BLOCK, OPTIONS_COPY_PTR, P1_CHAR, P1_COLOR, P1_LIFE_INIT,
    P1_PLAYER_PTR, P2_CHAR, P2_COLOR, P2_LIFE_INIT, P2_PLAYER_PTR, PAUSE, PAUSE_MENU, PLAYER_LIFE,
    RNG_ADDR, RNG_COUNT, SE_ID_MENU_CURSOR, SE_PAN_MENU_CURSOR, SELECT_KIND, SUP_SCENE_CUR,
    SUP_SCENE_REQ, SUPERVISOR,
};

/// ゲームのスレッドが外から得るもの（台本の`TickIo`に書き込みとシードを足したもの）。
/// 本物は`script_win::RealTickIo`。どれも確保しない・パニックしない。
pub trait NetIo: TickIo {
    /// 素のpoll（`0x0042B850`）をサイド0・1・2の順に1回ずつ呼んだ入力ワード（`poll_all`と同じ読み。InputManagerは汚さない —— 書き戻す）。
    /// サイド2の入力ワードはキーボード全配置 ＋ 両方のパッドで、画面6（Match）が読む`[2]`の入力ワードそのもの。
    /// pollの入口の照合が合わなかった実行と検査の偽物は`None`（呼び出し側は`local_input`の1入力ワードへ戻る）。
    fn poll_words(&mut self) -> Option<[u32; 3]> {
        None
    }
    /// ルーム名の入力フィールド（ウィンドウから打たれた文字を貯める）を開け閉めする。本物は`text_input_win::set_gate`。開けられなければfalse。
    /// 偽物は何もしない。
    fn text_gate(&mut self, _open: bool) -> bool {
        true
    }
    /// 貯まった文字を古い順に`out`の長さまで取る。本物は`text_input_win::drain`。偽物は0。
    fn typed_units(&mut self, _out: &mut [u16]) -> usize {
        0
    }
    /// ゲームの .dataの4バイトを直に書く（設置の前に読み取り可能と検証した`direct_ranges`のアドレスだけ）。
    fn write_data_u32(&mut self, addr: u32, v: u32);
    /// 例外を出さない書き方（自分のプロセスへの`WriteProcessMemory`）で1バイト書く。書き込めなければfalse。
    fn safe_write_u8(&mut self, addr: u32, v: u8) -> bool;
    /// 例外を出さない書き方で4バイト（リトルエンディアン）書く。書き込めなければfalse。
    fn safe_write_u32(&mut self, addr: u32, v: u32) -> bool;
    /// `set_state 0x00420160`（thiscall、ecx = `RNG_ADDR`、引数u16）を1回呼ぶ。
    fn set_rng_state(&mut self, seed: u16);
    /// .dataの`out.len()`バイトを直に読む（`direct_ranges`の中だけ）。
    fn read_data(&mut self, addr: u32, out: &mut [u8]);
    /// 立ち絵の3本の関数（`net_portrait`）を呼べるかどうか。
    /// 起動時に3本の入口のバイト列を全部照合して合ったときだけtrue。
    /// 合わなければ立ち絵の追随だけを有効にしない。選択そのものは動く。
    fn portrait_armed(&mut self) -> bool {
        false
    }
    /// 立ち絵の関数を1回呼ぶ（thiscall・ecx = MainMenu `m`・引数4つ・`ret 0x10`）。
    /// `portrait_armed`がfalseなら何もしない。
    fn call_portrait(&mut self, _m: u32, _call: PortraitCall) {}
    /// SEを1つ積む（`0x43E2F0`。呼ぶ前に関数の頭のバイト列を照合する）。積めなければfalse。偽物は積まない。
    fn push_se(&mut self, _id: u32, _pan: i32) -> bool {
        false
    }
    /// マッチの終わりの選択の縁取りと色の指示を画面へ（本物は`end_outline_win::show`）。偽物は何もしない。
    fn end_show(&mut self, _show: &end_pick::Show) {}
    /// マッチの終わりの選択のイベントを本ログへ（本物は`end_outline_win::note`。確保しない）。
    fn end_note(&mut self, _seg: u16, _note: end_pick::Note, _frame: u32) {}
    /// `[debug] end_pick = shadow`かどうか（本物は`end_outline_win::shadow`）。
    fn end_shadow(&mut self) -> bool {
        false
    }
}

struct PortraitGate<'a> {
    io: &'a mut dyn NetIo,
}

impl PortraitEnv for PortraitGate<'_> {
    fn unlocked(&mut self, chara: u8) -> bool {
        // 4バイト読みの下位1バイトが解放のフラグ（フラグは16バイトの表`CHARA_UNLOCK`。表の後ろも同じ .dataで読み取り可能）。
        CHARA_UNLOCK
            .checked_add(u32::from(chara))
            .and_then(|a| self.io.safe_u32(a))
            .is_some_and(|x| x & 0xFF != 0)
    }

    fn ready(&mut self) -> Option<u32> {
        if !self.io.portrait_armed() {
            return None;
        }
        let m = self.io.safe_u32(MAIN_MENU_PTR).filter(|&p| p != 0)?;
        // 部品の配列が0ならANMが読まれていない（立ち絵の関数は何もしない。呼ぶ意味も無い）。
        m.checked_add(MENU_PARTS_PTR)
            .and_then(|a| self.io.safe_u32(a))
            .filter(|&p| p != 0)?;
        Some(m)
    }
}

/// 設置の前に「直に読み取り可能・書き込み可能」と検証する .dataの範囲（アドレス, 長さ）。花映塚ではどれもth09.exeの .data。
/// 一時停止のバイトのワードはマッチ終了時のメニューのフラグ（`PAUSE + 2`）を含み、試合の毎tickに1回読む（`read_end_view`）。
pub fn direct_ranges() -> [(u32, u32); 21] {
    [
        (PAUSE, 4),
        (
            END_MENU + MENU_OBJ_STATE,
            MENU_OBJ_FRAMES + 4 - MENU_OBJ_STATE,
        ),
        (SUPERVISOR + SUP_SCENE_CUR, 4),
        (SUPERVISOR + SUP_SCENE_REQ, 4),
        (P1_CHAR, 4),
        (P2_CHAR, 4),
        (P1_COLOR, 4),
        (P2_COLOR, 4),
        (P1_LIFE_INIT, 4),
        (P2_LIFE_INIT, 4),
        (MAP, 4),
        (DIFFICULTY, 4),
        (RNG_ADDR, 4),
        (RNG_COUNT, 4),
        (OPTIONS_BLOCK, OPT_CHARGE + 4),
        (
            OPTIONS_BLOCK + OPTION_DIGEST_RANGES[0].0,
            OPTION_DIGEST_RANGES[3].1 - OPTION_DIGEST_RANGES[0].0,
        ),
        (OPTIONS_COPY_PTR, 4),
        (P1_PLAYER_PTR, 4),
        (P2_PLAYER_PTR, 4),
        (SELECT_KIND, 4),
        (MAP_LIST, MAP_LIST_LEN),
    ]
}

// 一時停止のバイトの4バイト読みにマッチ終了時のメニューのフラグのバイトが入る（`read_end_view`の前提）。
const _: () = assert!(MATCH_END_MENU == PAUSE + 2);

/// マッチの終わりの選択が読む終わりのメニュー。ワード`PAUSE`の下位1バイトが一時停止で、3バイト目が`MATCH_END_MENU`（どちらも`movb`の1バイトのフィールド）。
/// メニューの状態とカウントは出ているときだけ読む（出ていないメニューのフィールドは前の値のまま）。
/// 読み取れなければ「出ていない」とする（`[2]`は2人のワードのORのまま）。
/// delay方式・観戦者（`NetGame::end_view`）・rollbackのモード（`rollback_play::GameWindow::tick`）が同じ関数を使う。
pub fn read_end_view(read: &mut dyn FnMut(u32) -> Option<u32>) -> end_pick::View {
    let Some(w) = read(PAUSE) else {
        return end_pick::View::default();
    };
    let shown = (w >> ((MATCH_END_MENU - PAUSE) * 8)) & 0xFF != 0;
    let paused = w & 0xFF != 0;
    if !shown {
        return end_pick::View {
            paused,
            ..end_pick::View::default()
        };
    }
    match (
        read(END_MENU + MENU_OBJ_STATE),
        read(END_MENU + MENU_OBJ_FRAMES),
    ) {
        (Some(state), Some(frames)) => end_pick::View {
            shown,
            paused,
            state,
            frames,
        },
        _ => end_pick::View::default(),
    }
}

fn seat_addrs(seat: Seat) -> (u32, u32, u32) {
    match seat {
        Seat::P1 => (P1_CHAR, P1_COLOR, P1_LIFE_INIT),
        Seat::P2 => (P2_CHAR, P2_COLOR, P2_LIFE_INIT),
    }
}

pub fn check_agreed(a: &Agreed) -> Result<(), BadValue> {
    let p = &a.peer;
    let bad = |field: &'static str, value: i32| Err(BadValue { field, value });
    if p.chara > CHARA_MAX {
        return bad("キャラ", p.chara.into());
    }
    if p.color > COLOR_MAX {
        return bad("色", p.color.into());
    }
    if !(LIFE_MIN..=LIFE_MAX).contains(&p.life) {
        return bad("初期ライフ", p.life.into());
    }
    if p.charge > CHARGE_MAX {
        return bad("チャージ型", p.charge.into());
    }
    let map = a.map as i8;
    if !((0..=MAP_MAX).contains(&map) || map == MAP_RANDOM_OF_TWO || map == MAP_RANDOM_ANY) {
        return bad("マップ", map.into());
    }
    if a.difficulty > DIFFICULTY_MAX {
        return bad("難易度", a.difficulty.into());
    }
    Ok(())
}

/// 意味の分からないフィールドのダイジェスト（FNV-1a 32 bit。違えばログに残すだけ）。
pub fn option_digest(block_part: &[u8], base: u32) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for (s, e) in OPTION_DIGEST_RANGES {
        let (from, to) = ((s - base) as usize, (e - base) as usize);
        for b in block_part.get(from..to).unwrap_or(&[]) {
            h ^= u32::from(*b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

/// 入力の地点の判断を入口のアセンブリへの戻り値にする。
/// `stub.rs`の決まりは 0 = poll_all・1 = poll_all を呼ばずに戻る・2 = 描画だけのフレーム。
/// `None`は「lockstepの外」を表し、台本の経路（台本が終わっていれば0）へ回す。
pub fn entry_return(a: Action) -> Option<u32> {
    match a {
        Action::Local => None,
        Action::Zero | Action::Played(_) | Action::Menu => Some(1),
        Action::RenderOnly => Some(2),
    }
}

/// 台本が終わったらネットワークを閉じるときのBYEの理由。走っている間は`None`。
/// 最後の段階まで押した場合は終了。途中で止まった（条件を待ちきれなかった）場合は中止。
/// iniの`after_script = keep`なら最後の段階まで押しても閉じない（人の入力で続く）。
/// 途中で止まったときはkeepでも閉じる。
pub fn script_end_bye(s: Status, after: AfterScript) -> Option<ByeReason> {
    match (s, after) {
        (Status::Running { .. }, _) | (Status::Done, AfterScript::Keep) => None,
        (Status::Done, AfterScript::Bye) => Some(ByeReason::Finished),
        (Status::Aborted { .. }, _) => Some(ByeReason::Cancelled),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptStep {
    pub shot: u8,
    pub step: u16,
    pub status: Status,
    /// このtickで台本が終わった・止まった（以後はローカル入力）。
    pub ended: bool,
}

pub struct NetGame<'a> {
    pub io: &'a mut dyn NetIo,
    pub script: Option<&'a mut Engine>,
    pub script_addrs: GameAddrs,
    pub script_step: Option<ScriptStep>,
    /// 観戦者のEscの読み（`peek_local_input`）が台本を進めた結果。台本の入力ワードはfeedしていない。
    /// `Action::Local`を返したtickはこれを`scripted::commit`に渡してfeedする（`script_win::watch_tick`。台本を二重に進めない）。
    pub peeked: Option<Stepped>,
    /// ゲーム内のメニューが読んで保存しておいた入力ワード（`Game::stash_local`）。次の`local_input`が返す。
    pub stashed: Option<u16>,
    /// このtickに`digit_keys`が読んだ結果（`NetGame`はtickごとに作る）。
    /// Ctrl+数字の読み（`NetPlay::digit_tick`）とCtrl+Cの読み（`NetUi::copy_tick`）が同じtickに呼んでもキーの問い合わせは1 tickに1回で済ませる。
    keys: Option<Option<u16>>,
    /// このtickに台本から取り出した押下（数字・Tab・Ctrl+C）。`digit_script`と`copy_script`が同じtickに別々に呼ばれるので、台本からは1回だけ取る。
    script_keys: Option<Option<higanr_core::script::KeyPress>>,
    /// `script_keys`の数字・Tab / Ctrl+Cをもう渡した。
    digits_given: bool,
    copy_given: bool,
}

impl<'a> NetGame<'a> {
    pub fn new(
        io: &'a mut dyn NetIo,
        script: Option<&'a mut Engine>,
        script_addrs: GameAddrs,
    ) -> Self {
        NetGame {
            io,
            script,
            script_addrs,
            script_step: None,
            peeked: None,
            stashed: None,
            keys: None,
            script_keys: None,
            digits_given: false,
            copy_given: false,
        }
    }

    fn script_keys(&mut self) -> Option<higanr_core::script::KeyPress> {
        let script = &mut self.script;
        *self
            .script_keys
            .get_or_insert_with(|| script.as_deref_mut().and_then(Engine::take_keys))
    }

    fn read_keys(&mut self) -> Option<u16> {
        let active = self.io.safe_u32(APP_ACTIVE)? != 0;
        if !crate::digit_keys::may_read(active, crate::digit_keys_win::foreground_is_game) {
            return None;
        }
        crate::digit_keys_win::held()
    }

    /// 台本が動いていれば1 tick進めて結果を`script_step`に残す（`peek_local_input`用）。
    /// 1つの期間で進めるのは1回だけで、2回目からは前の結果を返す。
    /// 台本が無い・終わった・止まった後は`None`。
    fn step_script_once(&mut self) -> Option<Stepped> {
        if self.peeked.is_some() {
            return self.peeked;
        }
        let e = self.script.as_deref_mut()?;
        if !matches!(e.status(), Status::Running { .. }) {
            return None;
        }
        let s = scripted::advance(e, &mut *self.io, &self.script_addrs);
        self.script_step = Some(ScriptStep {
            shot: s.out.shot,
            step: s.out.step,
            status: s.status,
            ended: s.out.feed.is_none(),
        });
        self.peeked = Some(s);
        self.peeked
    }

    fn byte(&mut self, addr: u32) -> u8 {
        self.io.data_u32(addr) as u8
    }

    fn write_seat(&mut self, seat: Seat, s: &Select) {
        let (c, co, li) = seat_addrs(seat);
        self.io.write_data_u32(c, s.chara.into());
        self.io.write_data_u32(co, s.color.into());
        self.io.write_data_u32(li, s.life.into());
    }

    fn write_common(&mut self, a: &Agreed) {
        // マップは符号つき（`0x004264BF movsbl`）。
        self.io.write_data_u32(MAP, i32::from(a.map as i8) as u32);
        self.io.write_data_u32(DIFFICULTY, a.difficulty.into());
        self.io.set_rng_state(a.rng_seed);
    }
}

impl Game for NetGame<'_> {
    fn scenes(&mut self) -> (u32, u32) {
        (
            self.io.data_u32(SUPERVISOR + SUP_SCENE_CUR),
            self.io.data_u32(SUPERVISOR + SUP_SCENE_REQ),
        )
    }

    fn stash_local(&mut self, word: u16) {
        self.stashed = Some(word);
    }

    fn text_gate(&mut self, open: bool) -> bool {
        self.io.text_gate(open)
    }

    fn typed(&mut self, out: &mut [u16]) -> usize {
        self.io.typed_units(out)
    }

    /// 数字キーの読み（遅れの量を替える。`digit_keys`）: ゲームのウィンドウが前面のときだけ読む。
    /// 前面のフラグ`[0x004B30B8]`はゲーム自身がキーボードを読むかを決めるフラグと同じ（0ならゲームも読まない）。
    /// フラグの意味は未照合なので、フラグに加えてWindowsの前面のウィンドウもこのゲームのものであることを条件にする（`digit_keys_win::foreground_is_game`）。
    /// どちらかが満たされない・フラグが読めないときは`None`。文字の入力中かどうかは`digit_keys_win::held`が見る。
    ///
    /// 遅延の増分: フラグが立っているtickに前面のウィンドウの問い合わせ3回。同じ`NetGame`の2回目からは前の結果を返す。
    fn digit_keys(&mut self) -> Option<u16> {
        if let Some(k) = self.keys {
            return k;
        }
        let k = self.read_keys();
        self.keys = Some(k);
        k
    }

    /// 台本の段階（`digit=x` / `tab=1`）が効き始めた次のtickに1回だけ押した瞬間の集合（`digit_keys`のbitの並び）を返す。
    /// OSには何も送らない。台本が動いていない・段階が無い・`copy=1`だけの段階のときは`None`。
    fn digit_script(&mut self) -> Option<u16> {
        if self.digits_given {
            return None;
        }
        let k = self.script_keys()?;
        if k.digit.is_none() && !k.tab {
            return None;
        }
        self.digits_given = true;
        Some(k.digit.map_or(0, |d| 1u16 << d) | if k.tab { crate::digit_keys::TAB_BIT } else { 0 })
    }

    /// 台本の段階（`copy=1`）が効き始めた次のtickに1回だけ`true`（Ctrl+Cを押したことにする）。
    fn copy_script(&mut self) -> bool {
        if self.copy_given {
            return false;
        }
        self.copy_given = self.script_keys().is_some_and(|k| k.copy);
        self.copy_given
    }

    /// 台本のコンテナ（`script`）があれば台本の付いている実行。
    /// コンテナは`script_win`が武装している間ずっと残る（終わっても外さない）ので、台本が終わった後のtickも`true`。
    /// `digit_tick`はこの実行で本物のキーを読まない。
    fn scripted_run(&self) -> bool {
        self.script.is_some()
    }

    /// 台本の実行は偽の押下だけを使うので常に開ける。それ以外は数字キーと同じ門（ゲームの前面のフラグとOSの前面のウィンドウ）。
    fn keys_gate(&mut self) -> bool {
        if self.script.is_some() {
            return true;
        }
        let active = self.io.safe_u32(APP_ACTIVE).is_some_and(|v| v != 0);
        crate::digit_keys::may_read(active, crate::digit_keys_win::foreground_is_game)
    }

    fn replaying(&mut self) -> bool {
        self.io.data_u32(GLOBAL_STATE) & GLOBAL_STATE_REPLAY != 0
    }

    fn local_input(&mut self, seat: Seat) -> u16 {
        // ゲーム内のメニューが同じtickに読んだ入力ワード（台本・デバイスをもう読まない）。1回だけ返す。
        if let Some(w) = self.stashed.take() {
            return w;
        }
        if let Some(s) = self.peeked {
            // 同じtickにEscの読み（`peek_local_input`）が台本をもう進めた。二重に進めない。その結果の入力ワードを使う。
            if let Some((p1, p2)) = s.out.feed {
                let w = match seat {
                    Seat::P1 => p1,
                    Seat::P2 => p2,
                };
                return w & MATCH_SEND_MASK;
            }
            return self.io.local_input() as u16;
        }
        if let Some(e) = self.script.as_deref_mut()
            && matches!(e.status(), Status::Running { .. })
        {
            let obs = read_obs(&mut *self.io, &self.script_addrs);
            let out = e.step(&obs);
            self.script_step = Some(ScriptStep {
                shot: out.shot,
                step: out.step,
                status: e.status(),
                ended: out.feed.is_none(),
            });
            if let Some((p1, p2)) = out.feed {
                let w = match seat {
                    Seat::P1 => p1,
                    Seat::P2 => p2,
                };
                return w & MATCH_SEND_MASK;
            }
        }
        self.io.local_input() as u16
    }

    fn menu_words(&mut self, seat: Seat) -> [u16; 3] {
        // 台本が動いている・同じtickにもう読んだ（保存済み・Escの読み）ときは台本の入力ワード（`local_input`）をそのまま1Pのサイドへ渡す。
        // 台本の入力ワードをpollの入力ワードに差し替えると台本の検査（`scripts/runs/m1c3_*.run`）が人の機器に左右される。
        let scripted = self.stashed.is_some()
            || self.peeked.is_some()
            || self
                .script
                .as_deref()
                .is_some_and(|e| matches!(e.status(), Status::Running { .. }));
        if !scripted && let Some(w) = self.io.poll_words() {
            return w.map(|x| x as u16);
        }
        let w = self.local_input(seat);
        [w, 0, w]
    }

    fn peek_local_input(&mut self) -> u16 {
        // 台本が動いていればここで1 tick進める（この期間で1回だけ）。結果は`peeked`に残り`Local`を返したtickのfeedが使う。
        // 返すのは本物のデバイスの入力ワードに台本の入力ワードのESCビットだけを足したもの。
        // 観戦中のtickは台本の入力ワードをゲームへ渡さず記録の列だけを渡すので、台本のEsc以外のビットは読みに要らない。
        // 台本のEscを読みに加えるのは、実機で台本から観戦者のEscの経路を検証できるようにするため。
        let device = self.io.local_input() as u16;
        let script = self.step_script_once().and_then(|s| s.word()).unwrap_or(0);
        device | (script & keys::ESC)
    }

    fn feed(&mut self, index: u32, word: u16) {
        self.io.feed(index, u32::from(word));
    }

    fn input_state(&mut self, index: usize) -> InputState {
        let Some(&addr) = INPUT_MANAGERS.get(index) else {
            return InputState::new();
        };
        let mut b = [0u8; IM_BYTES];
        self.io.read_im(addr, &mut b);
        im_from_bytes(&b)
    }

    fn own_select(&mut self, seat: Seat, role: Role) -> Select {
        let (c, co, li) = seat_addrs(seat);
        let mut part = [0u8; (OPTION_DIGEST_RANGES[3].1 - OPTION_DIGEST_RANGES[0].0) as usize];
        self.io
            .read_data(OPTIONS_BLOCK + OPTION_DIGEST_RANGES[0].0, &mut part);
        let host = role == Role::Host;
        Select {
            match_no: 0,
            chara: self.byte(c),
            color: self.byte(co),
            life: self.byte(li),
            charge: self.byte(OPTIONS_BLOCK + OPT_CHARGE),
            // −1 / −2は下位1バイト（0xFF / 0xFE）で送り、書くときにi8として戻す。
            map: self.byte(MAP),
            difficulty: self.byte(DIFFICULTY),
            // CPUレベルは読み込みスレッドが難易度から書き直すので送らない。背景は読み込みスレッドが書く。
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: if host {
                self.io.data_u32(RNG_ADDR) as u16
            } else {
                0
            },
            option_digest: option_digest(&part, OPTION_DIGEST_RANGES[0].0),
        }
    }

    fn apply_agreed(&mut self, peer_seat: Seat, agreed: &Agreed) -> Result<(), BadValue> {
        check_agreed(agreed)?;
        self.write_seat(peer_seat, &agreed.peer);
        self.write_common(agreed);
        Ok(())
    }

    fn apply_watched(&mut self, p1: &Select, p2: &Select) -> Result<(), BadValue> {
        // 先に2サイドとも範囲を見る（1つでも外れていれば1つも書かない）。
        // 共通のフィールド（マップ・難易度・シード）は両サイドで同じ（対戦のホストの値が両方に入って積まれる）ので1つ目のサイドの値を1回だけ書く。
        let (a1, a2) = (watched_agreed(p1), watched_agreed(p2));
        check_agreed(&a1)?;
        check_agreed(&a2)?;
        self.write_seat(Seat::P1, p1);
        self.write_seat(Seat::P2, p2);
        self.write_common(&a1);
        Ok(())
    }

    fn write_charge(&mut self, p1: u8, p2: u8) -> bool {
        let Some(copy) = self.io.safe_u32(OPTIONS_COPY_PTR).filter(|&p| p != 0) else {
            return false;
        };
        let at = copy.wrapping_add(OPT_CHARGE);
        self.io.safe_write_u8(at, p1) && self.io.safe_write_u8(at.wrapping_add(1), p2)
    }

    fn menu_view(&mut self) -> Option<MenuView> {
        let p = self.io.safe_u32(MAIN_MENU_PTR).filter(|&p| p != 0)?;
        let mut field = |off: u32| p.checked_add(off).and_then(|a| self.io.safe_u32(a));
        let screen = field(MENU_SCREEN_ID)?;
        let sub = field(MENU_SUBSTATE)?;
        let trans = field(MENU_TRANSITIONING)?;
        let cursor = field(MENU_CURSOR)?;
        let chara_cursor = [field(MENU_CHARA_CURSOR)?, field(MENU_CHARA_CURSOR + 4)?];
        let decided = [field(MENU_CHARA_DECIDED)?, field(MENU_CHARA_DECIDED + 4)?];
        let select_kind = self.io.data_u32(SELECT_KIND);
        let mut map_list = [0u8; 16];
        self.io.read_data(MAP_LIST, &mut map_list);
        Some(MenuView {
            screen,
            sub,
            trans,
            cursor,
            chara_cursor,
            decided,
            select_kind,
            map_list,
        })
    }

    fn write_menu_u32(&mut self, offset: u32, value: u32) -> bool {
        let Some(p) = self.io.safe_u32(MAIN_MENU_PTR).filter(|&p| p != 0) else {
            return false;
        };
        p.checked_add(offset)
            .is_some_and(|a| self.io.safe_write_u32(a, value))
    }

    fn map_list(&mut self) -> [u8; 16] {
        let mut list = [0u8; 16];
        self.io.read_data(MAP_LIST, &mut list);
        list
    }

    fn follow_portrait(
        &mut self,
        v: &MenuView,
        seat: usize,
        cursor: u32,
        locked: Locked,
    ) -> Followed {
        let plan = net_portrait::plan(
            v,
            seat,
            cursor,
            locked,
            &mut PortraitGate { io: &mut *self.io },
        );
        match plan {
            Plan::Unchanged => Followed::Unchanged,
            Plan::Refused => Followed::Refused,
            Plan::Calls { m, calls } => {
                for c in calls {
                    self.io.call_portrait(m, c);
                }
                Followed::Called
            }
        }
    }

    fn cursor_se(&mut self) -> bool {
        self.io.push_se(SE_ID_MENU_CURSOR, SE_PAN_MENU_CURSOR)
    }

    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        read_check_values(&mut IoRead(&mut *self.io))
    }

    fn play_view(&mut self) -> Option<PlayView> {
        // 読むのは戻り道の間と観戦者のEscのtick・ゲームのポーズの間だけ（ふだんの試合のtickには呼ばれない）なので、どれも例外を出さない読み方でよい。
        let loading = self.io.safe_u32(LOADING)? != 0;
        // どちらも1バイトのフィールド（`movb`）。隣の3バイトは見ない。
        let paused = self.io.safe_u32(PAUSE)? & 0xFF != 0;
        let end_menu = self.io.safe_u32(MATCH_END_MENU)? & 0xFF != 0;
        let mut menu = |base: u32| {
            Some(MenuState {
                state: self.io.safe_u32(base + MENU_OBJ_STATE)?,
                frames: self.io.safe_u32(base + MENU_OBJ_FRAMES)?,
            })
        };
        // 出ていないメニューの状態は前の値のままなので読まない。
        let pause = if paused { menu(PAUSE_MENU) } else { None };
        let end = if end_menu { menu(END_MENU) } else { None };
        Some(PlayView {
            loading,
            paused,
            end_menu,
            pause,
            end,
        })
    }

    fn end_view(&mut self) -> end_pick::View {
        // 試合の毎tickに読むので設置の前に読み取り可能と検証したフィールド（`direct_ranges`）を直に読む。
        read_end_view(&mut |a| Some(self.io.data_u32(a)))
    }

    fn end_se(&mut self, se: end_pick::Se) -> bool {
        let (id, pan) = crate::th09_addr::end_menu_se(se);
        self.io.push_se(id, pan)
    }

    fn end_show(&mut self, show: &end_pick::Show) {
        self.io.end_show(show);
    }

    fn end_note(&mut self, seg: u16, note: end_pick::Note, frame: u32) {
        self.io.end_note(seg, note, frame);
    }

    fn end_shadow(&mut self) -> bool {
        self.io.end_shadow()
    }
}

/// CHECKの値を読む手段（`read_check_values`が使う）。
/// `data`はth09.exeの .dataを直に読む（例外を出さない）。`safe`はヒープ（Player）を例外を出さない読み方で読む。
pub trait CheckRead {
    fn data(&mut self, addr: u32) -> u32;
    fn safe(&mut self, addr: u32) -> Option<u32>;
}

struct IoRead<'a>(&'a mut dyn NetIo);

impl CheckRead for IoRead<'_> {
    fn data(&mut self, addr: u32) -> u32 {
        self.0.data_u32(addr)
    }
    fn safe(&mut self, addr: u32) -> Option<u32> {
        self.0.safe_u32(addr)
    }
}

/// CHECKに載せるゲームの値（乱数の状態の下16 bit・抽選回数・1Pのライフ・2Pのライフ）の読み方。
/// delay方式（`NetGame::check_values`）とrollbackのモード（`rollback_play::GameWindow::check_values`）が同じ関数を使う。
/// どれも描画だけのフィールドには当たらない。ライフはPlayerへのポインタが0なら0。読み取れなければ0。
pub fn read_check_values(r: &mut dyn CheckRead) -> (u16, u32, u8, u8) {
    let rng = r.data(RNG_ADDR) as u16;
    let count = r.data(RNG_COUNT);
    let mut life = |ptr: u32| {
        let p = r.data(ptr);
        if p == 0 {
            return 0;
        }
        r.safe(p.wrapping_add(PLAYER_LIFE)).map_or(0, |v| v as u8)
    };
    let (l1, l2) = (life(P1_PLAYER_PTR), life(P2_PLAYER_PTR));
    (rng, count, l1, l2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::IM_BYTES;
    use higanr_core::input_model::keys;
    use higanr_core::script::parse;
    use higanr_core::selection::Mismatch;
    use std::collections::HashMap;

    /// 偽のゲームは .dataのワード（4バイトずつ。アドレスをキーにする）と例外を出さない読み書きの先を持つ。InputManagerは持たない。
    /// 呼ばれた書き込みとシードを順に残す。
    #[derive(Default)]
    struct Fake {
        data: HashMap<u32, u32>,
        safe: HashMap<u32, u32>,
        safe_bytes: HashMap<u32, u8>,
        unwritable: Vec<u32>,
        options: Vec<u8>,
        map_list: [u8; 16],
        local: u32,
        local_calls: u32,
        feeds: Vec<(u32, u32)>,
        writes: Vec<(u32, u32)>,
        seeds: Vec<u16>,
        /// 立ち絵の3本の関数が照合済みかどうか（`NetIo::portrait_armed`）と、呼ばれた（ecx, 呼び出し）の列。
        portrait_armed: bool,
        portrait_calls: Vec<(u32, PortraitCall)>,
        /// 積んだSE（id, パン）。
        se: Vec<(u32, i32)>,
    }

    impl TickIo for Fake {
        fn data_u32(&mut self, addr: u32) -> u32 {
            *self.data.get(&addr).unwrap_or(&0)
        }
        fn safe_u32(&mut self, addr: u32) -> Option<u32> {
            self.safe.get(&addr).copied()
        }
        fn read_im(&mut self, _addr: u32, out: &mut [u8; IM_BYTES]) {
            *out = [0; IM_BYTES];
        }
        fn local_input(&mut self) -> u32 {
            self.local_calls += 1;
            self.local
        }
        fn feed(&mut self, index: u32, word: u32) {
            self.feeds.push((index, word));
        }
    }

    impl NetIo for Fake {
        fn write_data_u32(&mut self, addr: u32, v: u32) {
            self.writes.push((addr, v));
            self.data.insert(addr, v);
        }
        fn safe_write_u8(&mut self, addr: u32, v: u8) -> bool {
            if self.unwritable.contains(&addr) {
                return false;
            }
            self.safe_bytes.insert(addr, v);
            true
        }
        fn safe_write_u32(&mut self, addr: u32, v: u32) -> bool {
            if self.unwritable.contains(&addr) {
                return false;
            }
            self.safe.insert(addr, v);
            true
        }
        fn set_rng_state(&mut self, seed: u16) {
            self.seeds.push(seed);
        }
        fn portrait_armed(&mut self) -> bool {
            self.portrait_armed
        }
        fn call_portrait(&mut self, m: u32, call: PortraitCall) {
            assert!(self.portrait_armed, "照合していない関数を呼んだ");
            self.portrait_calls.push((m, call));
        }
        fn push_se(&mut self, id: u32, pan: i32) -> bool {
            self.se.push((id, pan));
            true
        }
        fn read_data(&mut self, addr: u32, out: &mut [u8]) {
            if addr == MAP_LIST {
                out.copy_from_slice(&self.map_list[..out.len()]);
                return;
            }
            let off = (addr - OPTIONS_BLOCK) as usize;
            out.copy_from_slice(&self.options[off..off + out.len()]);
        }
    }

    fn fake() -> Fake {
        let mut f = Fake {
            options: (0..0xCCu32).map(|i| (i * 3 + 1) as u8).collect(),
            local: 0xABCD_0042,
            ..Fake::default()
        };
        f.data.insert(SUPERVISOR + SUP_SCENE_CUR, 1);
        f.data.insert(SUPERVISOR + SUP_SCENE_REQ, 2);
        f.data.insert(P1_CHAR, 3);
        f.data.insert(P2_CHAR, 12);
        f.data.insert(P1_COLOR, 1);
        f.data.insert(P2_COLOR, 0);
        f.data.insert(P1_LIFE_INIT, 7);
        f.data.insert(P2_LIFE_INIT, 10);
        f.data.insert(MAP, 0xFFFF_FFFE); // −2
        f.data.insert(DIFFICULTY, 2);
        f.data.insert(RNG_ADDR, 0x0005_BEEF); // 上の2バイトは抽選回数の隣のフィールドではない（状態はu16）
        f.data.insert(RNG_COUNT, 1234);
        f.data.insert(OPTIONS_BLOCK + OPT_CHARGE, 0x0000_0100); // +0xB4 = 0（1Pの枠）、+0xB5 = 1
        f
    }

    fn agreed(peer: Select) -> Agreed {
        Agreed {
            peer,
            map: 5,
            difficulty: 3,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: 0x1357,
            mismatch: Mismatch::default(),
        }
    }

    fn peer_select() -> Select {
        Select {
            match_no: 0,
            chara: 15,
            color: 1,
            life: 1,
            charge: 1,
            map: 5,
            difficulty: 3,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: 0,
            option_digest: 0,
        }
    }

    #[test]
    fn digit_keys_are_read_only_while_the_game_window_is_in_front() {
        const K5: u16 = 1 << 5;
        let mut f = fake();
        crate::digit_keys_win::test_set_held(Some(K5));
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), None, "旗を読めない間は読まない");
        }
        f.safe.insert(APP_ACTIVE, 0);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), None, "前面でないのに読んだ");
        }
        f.safe.insert(APP_ACTIVE, 1);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), Some(K5), "前面なのに読まない");
        }
        // フラグは立っているが前面のウィンドウがほかのアプリ: 読まない。戻すと読む（隣の肯定）。
        crate::digit_keys_win::test_set_foreground(false);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(
                g.digit_keys(),
                None,
                "前面のウィンドウがゲームでないのに読んだ"
            );
        }
        f.safe.insert(APP_ACTIVE, 0);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), None, "旗 0 ・前面もゲームでない");
        }
        f.safe.insert(APP_ACTIVE, 1);
        crate::digit_keys_win::test_set_foreground(true);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), Some(K5), "前面がゲームに戻ったのに読まない");
        }
        crate::digit_keys_win::test_set_held(None);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), None);
        }
        crate::digit_keys_win::test_set_foreground(true);
        // 同じtickの2回目の呼び出し（Ctrl+数字の読みとCtrl+Cの読み）は問い合わせ直さない。次のtick（新しい`NetGame`）は読み直す。
        use crate::digit_keys::{C_BIT, CTRL_BIT};
        f.safe.insert(APP_ACTIVE, 1);
        crate::digit_keys_win::test_set_held(Some(C_BIT | CTRL_BIT));
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), Some(C_BIT | CTRL_BIT));
            crate::digit_keys_win::test_set_held(Some(0));
            assert_eq!(
                g.digit_keys(),
                Some(C_BIT | CTRL_BIT),
                "同じ tick に読み直した"
            );
        }
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.digit_keys(), Some(0), "次の tick に読み直さない");
        }
        crate::digit_keys_win::test_set_held(None);
        assert_eq!(APP_ACTIVE, 0x004B_30B8);
    }

    #[test]
    fn a_script_step_with_digit_and_tab_gives_the_pressed_set_once() {
        use higanr_core::script::{Engine, Obs, parse};
        let obs = Obs {
            scene: 1,
            screen: None,
            cursor: None,
            sub: None,
            trans: None,
            pause: false,
            loading: false,
            demo: false,
            end_menu: false,
            menu_flag: false,
            talk: None,
        };
        let mut f = fake();
        let mut e = Engine::new(
            parse("scene=1 wait=1 digit=5 tab=1\nscene=1 wait=1 digit=0\nscene=1 wait=1 tab=1\nscene=1 wait=1\n").unwrap(),
        );
        let mut seen = Vec::new();
        for _ in 0..6 {
            let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
            let got = g.digit_script();
            assert_eq!(g.digit_script(), None, "2 回取り出せた");
            seen.push(got);
            e.step(&obs);
        }
        assert_eq!(
            seen,
            vec![
                None,
                Some((1 << 5) | crate::digit_keys::TAB_BIT),
                Some(1 << 0),
                Some(crate::digit_keys::TAB_BIT),
                None,
                None,
            ],
            "段ごとに 1 回ずつ（効き始めた次の取り出しで）"
        );
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        assert_eq!(g.digit_script(), None);
    }

    /// `copy=1`の段階で数字・Tabの押下が出ないこと、`digit=`の段階でCtrl+Cが出ないことも見る。
    #[test]
    fn a_script_step_with_copy_gives_it_once_and_the_call_order_does_not_matter() {
        use higanr_core::script::{Engine, Obs, parse};
        let obs = Obs {
            scene: 1,
            screen: None,
            cursor: None,
            sub: None,
            trans: None,
            pause: false,
            loading: false,
            demo: false,
            end_menu: false,
            menu_flag: false,
            talk: None,
        };
        for copy_first in [true, false] {
            let mut f = fake();
            let mut e = Engine::new(
                parse("scene=1 wait=1 copy=1\nscene=1 wait=1 digit=4\nscene=1 wait=1\n").unwrap(),
            );
            let mut seen = Vec::new();
            for _ in 0..5 {
                let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
                let (c, d) = if copy_first {
                    let c = g.copy_script();
                    (c, g.digit_script())
                } else {
                    let d = g.digit_script();
                    (g.copy_script(), d)
                };
                assert!(!g.copy_script(), "コピーを 2 回取り出せた");
                assert_eq!(g.digit_script(), None, "数字を 2 回取り出せた");
                seen.push((c, d));
                e.step(&obs);
            }
            assert_eq!(
                seen,
                vec![
                    (false, None),
                    (true, None),
                    (false, Some(1 << 4)),
                    (false, None),
                    (false, None)
                ],
                "copy_first = {copy_first}"
            );
        }
        let mut f = fake();
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        assert!(!g.copy_script(), "台本が無いのにコピーが出た");
    }

    #[test]
    fn scripted_run_is_true_exactly_while_a_script_box_is_armed() {
        use higanr_core::script::{Engine, Obs, parse};
        let obs = Obs {
            scene: 1,
            screen: None,
            cursor: None,
            sub: None,
            trans: None,
            pause: false,
            loading: false,
            demo: false,
            end_menu: false,
            menu_flag: false,
            talk: None,
        };
        let mut f = fake();
        {
            let g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert!(!g.scripted_run(), "台本の無い実行を台本の実行と見た");
        }
        let mut e = Engine::new(parse("scene=1 wait=1\n").unwrap());
        {
            let g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
            assert!(g.scripted_run(), "台本の入れ物があるのに台本の実行と見ない");
        }
        // 台本を最後まで進めてもコンテナは残る（終わった後も本物のキーを読まない）。
        for _ in 0..10 {
            if !matches!(e.status(), Status::Running { .. }) {
                break;
            }
            e.step(&obs);
        }
        assert!(
            !matches!(e.status(), Status::Running { .. }),
            "検査の前提: 台本が終わっている"
        );
        let g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
        assert!(
            g.scripted_run(),
            "台本が終わった後の実行を台本の無い実行と見た"
        );
    }

    #[test]
    fn reads_scenes_and_own_select() {
        let mut f = fake();
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        assert_eq!(g.scenes(), (1, 2));
        let s = g.own_select(Seat::P2, Role::Host);
        assert_eq!(
            (s.chara, s.color, s.life, s.charge, s.map, s.difficulty),
            (12, 0, 10, 0, 0xFE, 2)
        );
        assert_eq!(s.rng_seed, 0xBEEF);
        assert_eq!((s.cpu_1p, s.cpu_2p, s.background), (0, 0, 0));
        let s1 = g.own_select(Seat::P1, Role::Guest);
        assert_eq!((s1.chara, s1.color, s1.life, s1.rng_seed), (3, 1, 7, 0));
        let d0 = s.option_digest;
        f.options[0xA4] ^= 0xFF;
        f.options[0xB4] ^= 0xFF;
        f.options[0x6B] ^= 0xFF;
        let d1 = NetGame::new(&mut f, None, GameAddrs::th09())
            .own_select(Seat::P2, Role::Host)
            .option_digest;
        assert_eq!(d0, d1, "欄の外で変わった");
        for at in [0x6C, 0xA3, 0xB3, 0xB6, 0xB9] {
            f.options[at] ^= 0x01;
            let d = NetGame::new(&mut f, None, GameAddrs::th09())
                .own_select(Seat::P2, Role::Host)
                .option_digest;
            assert_ne!(d, d0, "+0x{at:X} を変えても変わらない");
            f.options[at] ^= 0x01;
        }
    }

    #[test]
    fn applies_only_in_range_values_to_the_peer_seat() {
        let mut f = fake();
        let mut a = agreed(peer_select());
        a.map = (-1i8) as u8;
        NetGame::new(&mut f, None, GameAddrs::th09())
            .apply_agreed(Seat::P1, &a)
            .unwrap();
        assert_eq!(
            f.writes,
            vec![
                (P1_CHAR, 15),
                (P1_COLOR, 1),
                (P1_LIFE_INIT, 1),
                (MAP, 0xFFFF_FFFF),
                (DIFFICULTY, 3),
            ]
        );
        assert_eq!(f.seeds, vec![0x1357]);
        let mut f = fake();
        NetGame::new(&mut f, None, GameAddrs::th09())
            .apply_agreed(Seat::P2, &agreed(peer_select()))
            .unwrap();
        assert_eq!(
            f.writes[..3],
            [(P2_CHAR, 15), (P2_COLOR, 1), (P2_LIFE_INIT, 1)]
        );
        type Bend = fn(&mut Agreed);
        let cases: [(&str, i32, Bend); 8] = [
            ("キャラ", 16, |a| a.peer.chara = 16),
            ("色", 2, |a| a.peer.color = 2),
            ("初期ライフ", 0, |a| a.peer.life = 0),
            ("初期ライフ", 11, |a| a.peer.life = 11),
            ("チャージ型", 2, |a| a.peer.charge = 2),
            ("マップ", 14, |a| a.map = 14),
            ("マップ", -3, |a| a.map = (-3i8) as u8),
            ("難易度", 4, |a| a.difficulty = 4),
        ];
        for (field, value, bend) in cases {
            let mut a = agreed(peer_select());
            bend(&mut a);
            let mut f = fake();
            let r = NetGame::new(&mut f, None, GameAddrs::th09()).apply_agreed(Seat::P1, &a);
            assert_eq!(r, Err(BadValue { field, value }));
            assert!(
                f.writes.is_empty() && f.seeds.is_empty(),
                "{field} {value}: 書いた"
            );
        }
        for m in [0i8, 13, -2] {
            let mut a = agreed(peer_select());
            a.map = m as u8;
            a.peer.life = 10;
            assert!(check_agreed(&a).is_ok(), "マップ {m}");
        }
    }

    #[test]
    fn applies_both_seats_from_the_records_and_checks_both_first() {
        let mut f = fake();
        let mut p1 = peer_select();
        p1.chara = 9;
        p1.color = 0;
        p1.life = 7;
        let mut p2 = peer_select();
        p2.chara = 4;
        p2.color = 1;
        p2.life = 2;
        for s in [&mut p1, &mut p2] {
            s.map = (-1i8) as u8;
            s.difficulty = 3;
            s.rng_seed = 0x2468;
        }
        NetGame::new(&mut f, None, GameAddrs::th09())
            .apply_watched(&p1, &p2)
            .unwrap();
        assert_eq!(
            f.writes,
            vec![
                (P1_CHAR, 9),
                (P1_COLOR, 0),
                (P1_LIFE_INIT, 7),
                (P2_CHAR, 4),
                (P2_COLOR, 1),
                (P2_LIFE_INIT, 2),
                (MAP, 0xFFFF_FFFF),
                (DIFFICULTY, 3),
            ]
        );
        assert_eq!(f.seeds, vec![0x2468], "シードは 1 回だけ");
        // 2つ目のサイドだけ範囲の外: 1つも書かない（1つ目のサイドも書かない）。
        let mut bad = p2;
        bad.chara = 16;
        let mut f = fake();
        let r = NetGame::new(&mut f, None, GameAddrs::th09()).apply_watched(&p1, &bad);
        assert_eq!(
            r,
            Err(BadValue {
                field: "キャラ",
                value: 16
            })
        );
        assert!(f.writes.is_empty() && f.seeds.is_empty());
    }

    #[test]
    fn writes_charge_into_the_copy() {
        let mut f = fake();
        assert!(!NetGame::new(&mut f, None, GameAddrs::th09()).write_charge(1, 0));
        f.safe.insert(OPTIONS_COPY_PTR, 0);
        assert!(!NetGame::new(&mut f, None, GameAddrs::th09()).write_charge(1, 0));
        assert!(f.safe_bytes.is_empty());
        f.safe.insert(OPTIONS_COPY_PTR, 0x0200_0000);
        assert!(NetGame::new(&mut f, None, GameAddrs::th09()).write_charge(1, 0));
        assert_eq!(f.safe_bytes.get(&0x0200_00B4), Some(&1));
        assert_eq!(f.safe_bytes.get(&0x0200_00B5), Some(&0));
        f.unwritable.push(0x0200_00B5);
        assert!(!NetGame::new(&mut f, None, GameAddrs::th09()).write_charge(0, 1));
    }

    #[test]
    fn reads_check_values() {
        let mut f = fake();
        f.data.insert(P1_PLAYER_PTR, 0x0300_0000);
        f.data.insert(P2_PLAYER_PTR, 0x0400_0000);
        f.safe.insert(0x0300_0000 + PLAYER_LIFE, 9);
        f.safe.insert(0x0400_0000 + PLAYER_LIFE, 4);
        let v = NetGame::new(&mut f, None, GameAddrs::th09()).check_values();
        assert_eq!(v, (0xBEEF, 1234, 9, 4));
        f.data.insert(P2_PLAYER_PTR, 0);
        f.safe.remove(&(0x0300_0000 + PLAYER_LIFE));
        let v = NetGame::new(&mut f, None, GameAddrs::th09()).check_values();
        assert_eq!((v.2, v.3), (0, 0));
    }

    #[test]
    fn follows_the_portrait_only_through_open_gates() {
        use crate::net_portrait::PortraitFn;
        const M: u32 = 0x0500_0000;
        let view = MenuView {
            screen: 8,
            sub: 1,
            trans: 0,
            cursor: 0,
            chara_cursor: [5, 9],
            decided: [0, 0],
            select_kind: 0,
            map_list: [0xFF; 16],
        };
        let open = || {
            let mut f = fake();
            f.portrait_armed = true;
            f.safe.insert(MAIN_MENU_PTR, M);
            f.safe.insert(M + MENU_PARTS_PTR, 0x0600_0000);
            f.safe.insert(CHARA_UNLOCK + 6, 1);
            f.safe.insert(CHARA_UNLOCK + 0x0E, 0x0101);
            f
        };
        let follow = |f: &mut Fake, seat: usize, cursor: u32| {
            NetGame::new(f, None, GameAddrs::th09()).follow_portrait(
                &view,
                seat,
                cursor,
                Locked::Refuse,
            )
        };
        let mut f = open();
        assert_eq!(follow(&mut f, 0, 6), Followed::Called);
        let call = |f: PortraitFn, args: [u32; 4]| (M, PortraitCall { f, args });
        assert_eq!(
            f.portrait_calls,
            vec![
                call(PortraitFn::Slide, [6, 0x5C, 16, 2]),
                call(PortraitFn::Slide, [6, 0x3C, 16, 2]),
                call(PortraitFn::Marker, [0, 6, 7, crate::th09_addr::CHARA_TABLE]),
            ]
        );
        let mut f = open();
        assert_eq!(follow(&mut f, 1, 7), Followed::Called, "2P、カーソル 9 → 7");
        assert_eq!(f.portrait_calls[0].1.args, [0x0E, 0x5D, 16, 2]);
        let mut f = open();
        assert_eq!(follow(&mut f, 0, 5), Followed::Unchanged);
        assert!(f.portrait_calls.is_empty());
        type Close = fn(&mut Fake);
        let closers: [(&str, Close); 5] = [
            ("署名が合わない", |f| f.portrait_armed = false),
            ("MainMenu が無い", |f| {
                f.safe.insert(MAIN_MENU_PTR, 0);
            }),
            ("部品の配列が無い", |f| {
                f.safe.insert(M + MENU_PARTS_PTR, 0);
            }),
            ("未解放", |f| {
                f.safe.insert(CHARA_UNLOCK + 6, 0);
            }),
            ("MainMenu が読めない", |f| {
                f.safe.remove(&MAIN_MENU_PTR);
            }),
        ];
        for (why, close) in closers {
            let mut f = open();
            close(&mut f);
            assert_eq!(follow(&mut f, 0, 6), Followed::Refused, "{why}");
            assert!(f.portrait_calls.is_empty(), "{why}: 呼んだ");
        }
        let mut f = open();
        f.safe.insert(CHARA_UNLOCK + 7, 1);
        f.safe.insert(CHARA_UNLOCK + 0x0E, 0x0100);
        assert_eq!(follow(&mut f, 0, 7), Followed::Refused);
        assert!(f.portrait_calls.is_empty());
        // 相手のサイドの位置の経路は未解放でも呼ぶ（ほかの門は同じ）。
        let mut f = open();
        f.safe.insert(CHARA_UNLOCK + 6, 0);
        let got = NetGame::new(&mut f, None, GameAddrs::th09()).follow_portrait(
            &view,
            0,
            6,
            Locked::Allow,
        );
        assert_eq!(got, Followed::Called);
        assert_eq!(f.portrait_calls.len(), 3);
        let mut f = open();
        f.portrait_armed = false;
        let got = NetGame::new(&mut f, None, GameAddrs::th09()).follow_portrait(
            &view,
            0,
            6,
            Locked::Allow,
        );
        assert_eq!(got, Followed::Refused, "署名が合わなければ呼ばない");
    }

    /// ゲームのカーソルの移動`0x0042474A`（`push $0x0`・`push $0xc`）と同じid・パン。
    #[test]
    fn the_cursor_sound_is_the_games_cursor_move_sound() {
        let mut f = fake();
        assert!(NetGame::new(&mut f, None, GameAddrs::th09()).cursor_se());
        assert_eq!(f.se, vec![(0x0C, 0)]);
    }

    #[test]
    fn entry_return_follows_the_stub_contract() {
        use crate::netplay::FrameRec;
        let r = FrameRec {
            seg: 0,
            frame: 0,
            p1: 0,
            p2: 0,
        };
        assert_eq!(entry_return(Action::Local), None);
        assert_eq!(entry_return(Action::Zero), Some(1));
        assert_eq!(entry_return(Action::Played(r)), Some(1));
        assert_eq!(entry_return(Action::RenderOnly), Some(2));
    }

    #[test]
    fn the_net_closes_only_after_the_script_ends() {
        let (bye, keep) = (AfterScript::Bye, AfterScript::Keep);
        assert_eq!(script_end_bye(Status::Running { step: 3 }, bye), None);
        assert_eq!(script_end_bye(Status::Done, bye), Some(ByeReason::Finished));
        assert_eq!(
            script_end_bye(Status::Aborted { step: 2, line: 9 }, bye),
            Some(ByeReason::Cancelled)
        );
        assert_eq!(script_end_bye(Status::Running { step: 3 }, keep), None);
        assert_eq!(script_end_bye(Status::Done, keep), None);
        assert_eq!(
            script_end_bye(Status::Aborted { step: 2, line: 9 }, keep),
            Some(ByeReason::Cancelled)
        );
    }

    #[test]
    fn works_inside_netplay_against_a_fake_peer() {
        use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
        use crate::netplay::{
            Action, CheckSource, Config, FrameRec, NetPlay, Phase, WAIT_BUDGET_US,
        };
        use higanr_core::handshake::{self, Name};
        use higanr_core::link;
        use std::net::SocketAddr;

        const F: u32 = 30;
        let addr = |p: u16| SocketAddr::from(([127, 0, 0, 1], p));
        let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), 2_000);
        let lc = |peer: u16, nonce: u64| link::Config {
            handshake: handshake::Config {
                session_id: 7,
                role_nonce: nonce,
                latency: 2,
                name: Name::new("x"),
                host_seat: Seat::P2,
            },
            secret: peer_sim::cli::DEFAULT_SECRET,
            peers: higanr_core::candidate::Candidates::from_addrs(&[addr(peer)]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        };
        let cfg = Config {
            check: CheckSource::InputDigest,
            wait_budget_us: WAIT_BUDGET_US,
            ..Config::default()
        };
        let mut npa = NetPlay::new(lc(2, 0xF000), cfg, &mut na);
        let mut npb = NetPlay::new(lc(1, 1), cfg, &mut nb);
        let mut f = fake();
        f.data.insert(SUPERVISOR + SUP_SCENE_REQ, 0);
        f.safe.insert(OPTIONS_COPY_PTR, 0x0200_0000);
        let mut gb = FakeGame::new(0xB, F, 1, 1);
        gb.select_chara = 13; // チャージ型は 13 % 3 = 1（範囲の中）
        gb.load_first = 3;
        let (mut loading, mut starts) = (None::<u32>, 0);
        let mut recs: Vec<FrameRec> = Vec::new();
        let mut slot = clock.get();
        for _ in 0..3_000 {
            if clock.get() < slot {
                clock.set(slot);
            }
            let scene = |f: &Fake| {
                (
                    f.data[&(SUPERVISOR + SUP_SCENE_CUR)],
                    f.data[&(SUPERVISOR + SUP_SCENE_REQ)],
                )
            };
            f.feeds.clear();
            let act = {
                let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
                npa.on_input(&mut g, &mut na)
            };
            match (act, scene(&f)) {
                (Action::Local, (1, 0)) if npa.phase() == Phase::Selecting && starts == 0 => {
                    f.data.insert(SUPERVISOR + SUP_SCENE_REQ, 2);
                }
                (Action::Zero, (1, 2)) => {
                    assert_eq!(npa.phase(), Phase::Loading);
                    f.data.insert(SUPERVISOR + SUP_SCENE_CUR, 2);
                    f.data.insert(SUPERVISOR + SUP_SCENE_REQ, 0);
                    loading = Some(3);
                }
                (Action::Zero, (2, 0)) => {
                    if let Some(n) = loading.as_mut() {
                        *n -= 1;
                        if *n == 0 {
                            loading = None;
                            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
                            npa.on_match_start(&mut g);
                            starts += 1;
                        }
                    }
                }
                (Action::Played(r), (2, 0)) => {
                    assert_eq!(
                        f.feeds,
                        vec![
                            (0, r.p1.into()),
                            (1, r.p2.into()),
                            (2, (r.p1 | r.p2).into())
                        ]
                    );
                    recs.push(r);
                    if recs.len() == F as usize {
                        f.data.insert(SUPERVISOR + SUP_SCENE_REQ, 1);
                    }
                }
                (Action::Zero, (2, 1)) => {
                    f.data.insert(SUPERVISOR + SUP_SCENE_CUR, 1);
                    f.data.insert(SUPERVISOR + SUP_SCENE_REQ, 0);
                }
                _ => {}
            }
            gb.do_frame(&mut npb, &mut nb);
            slot += FRAME_US;
            if gb.done && recs.len() == F as usize && scene(&f) == (1, 0) {
                break;
            }
        }
        assert_eq!(starts, 1, "開始のフック");
        assert_eq!(recs, gb.recs, "両者の列");
        assert_eq!(recs.len(), F as usize);
        assert!(
            recs.iter().skip(2).all(|r| r.p2 == 0x42 && r.p1 != 0),
            "{recs:?}"
        );
        assert_eq!(
            f.writes,
            vec![
                (P1_CHAR, 13),
                (P1_COLOR, 0),
                (P1_LIFE_INIT, 10),
                (MAP, 0xFFFF_FFFE),
                (DIFFICULTY, 2),
            ]
        );
        assert_eq!(f.seeds, vec![0xBEEF]);
        let ag = gb.agreed[0].1;
        assert_eq!(
            (ag.peer.chara, ag.peer.life, ag.rng_seed, ag.map),
            (12, 10, 0xBEEF, 0xFE)
        );
        assert_eq!(f.safe_bytes.get(&0x0200_00B4), Some(&1));
        assert_eq!(f.safe_bytes.get(&0x0200_00B5), Some(&0));
        assert_eq!(gb.charges, vec![(1, 0)]);
    }

    #[test]
    fn own_word_comes_from_the_script_then_local() {
        let mut f = fake();
        f.data.insert(SUPERVISOR + SUP_SCENE_CUR, 2);
        let mut e = Engine::new(parse("scene=2 wait=1 p1=Z p2=X+ESC hold=2\n").unwrap());
        {
            let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
            assert_eq!(g.local_input(Seat::P2), 0);
            let w = g.local_input(Seat::P2);
            assert_eq!(w, keys::X | keys::ESC);
            assert!(!g.script_step.unwrap().ended);
            g.local_input(Seat::P2);
            assert_eq!(g.local_input(Seat::P2), 0x0042);
            assert!(g.script_step.unwrap().ended);
            assert_eq!(g.script_step.unwrap().status, Status::Done);
            g.script_step = None;
            assert_eq!(g.local_input(Seat::P1), 0x0042);
            assert_eq!(g.script_step, None);
            g.feed(2, 0x0013);
        }
        assert_eq!(f.local_calls, 2);
        assert_eq!(f.feeds, vec![(2, 0x13)]);
    }

    #[test]
    fn peek_local_input_adds_only_the_scripts_esc_and_steps_the_script_once_per_window() {
        const DEVICE: u16 = 0x0040; // LEFTだけ（X・Z・ESCを含まない）
        let mut f = fake();
        f.local = u32::from(DEVICE);
        f.data.insert(SUPERVISOR + SUP_SCENE_CUR, 2);
        let mut e = Engine::new(parse("scene=2 wait=1 p1=Z p2=X+ESC hold=2\n").unwrap());
        // tickごとの（最初の読み, 期間の終わりに台本が終わっていたか（None = 進めなかった）, local_inputの1P, 2P）
        let mut got = Vec::new();
        for tick in 1..=5 {
            let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
            let first = g.peek_local_input();
            for _ in 0..2 {
                assert_eq!(
                    g.peek_local_input(),
                    first,
                    "tick {tick}: 同じ期間の 2・3 回目"
                );
            }
            let ended = g.peeked.map(|s| s.word().is_none());
            let w2 = g.local_input(Seat::P2);
            let w1 = g.local_input(Seat::P1);
            got.push((first, ended, w1, w2));
        }
        assert_eq!(
            got,
            vec![
                (DEVICE, Some(false), 0, 0), // 条件を満たしたtick（押す前のwait）
                (
                    DEVICE | keys::ESC,
                    Some(false),
                    keys::Z,
                    keys::X | keys::ESC
                ),
                (
                    DEVICE | keys::ESC,
                    Some(false),
                    keys::Z,
                    keys::X | keys::ESC
                ),
                (DEVICE, Some(true), DEVICE, DEVICE), // 台本が終わったtickはローカル入力に戻る
                (DEVICE, None, DEVICE, DEVICE),       // 終わった後は進めない
            ],
            "tick ごとの（最初の読み, 台本が終わったか, local_input の 1P, 2P）"
        );
        // デバイスの読み: peekは期間ごとに3回（15）＋ `local_input`が台本の入力ワードを使わない期間（tick 4・5）の2回ずつ（4）。
        assert_eq!(f.local_calls, 3 * 5 + 2 * 2, "デバイスの読みの数");
    }

    #[test]
    fn peek_local_input_without_a_running_script_is_just_the_device() {
        let mut f = fake();
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.peek_local_input(), 0x0042);
            assert!(g.peeked.is_none() && g.script_step.is_none());
        }
        let mut e = Engine::new(parse("scene=9 limit=1 p1=Z\n").unwrap());
        {
            let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
            let _ = g.peek_local_input(); // 条件が成り立たず、limit=1で止まる
            assert!(matches!(
                g.peeked.map(|s| s.status),
                Some(Status::Aborted { .. })
            ));
        }
        let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
        assert_eq!(g.peek_local_input(), 0x0042);
        assert!(g.peeked.is_none(), "止まった台本を進めた");
    }

    #[test]
    fn peek_local_input_does_not_allocate() {
        let mut f = fake();
        f.data.insert(SUPERVISOR + SUP_SCENE_CUR, 2);
        let mut e = Engine::new(parse("scene=2 wait=1 p1=Z p2=X+ESC hold=200\n").unwrap());
        let a0 = crate::test_alloc::count();
        for _ in 0..50 {
            let mut g = NetGame::new(&mut f, Some(&mut e), GameAddrs::th09());
            g.peek_local_input();
            g.local_input(Seat::P1);
        }
        assert_eq!(crate::test_alloc::count(), a0, "読みが確保した");
    }

    #[test]
    fn play_view_reads_the_one_byte_flags_and_only_the_menus_that_are_shown() {
        let mut f = fake();
        // 隣の3バイトが立っていても1バイトのフィールドだけを見る（一時停止の`0x4A7ECC`と`0x4A7ECE`は同じ4バイトに並ぶ）。
        f.safe.insert(LOADING, 0);
        f.safe.insert(PAUSE, 0x0001_0000);
        f.safe.insert(MATCH_END_MENU, 0x0000_0100);
        f.safe.insert(PAUSE_MENU + MENU_OBJ_STATE, 2);
        f.safe.insert(PAUSE_MENU + MENU_OBJ_FRAMES, 9);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            let v = g.play_view().unwrap();
            assert!(!v.loading && !v.paused && !v.end_menu);
            assert_eq!((v.pause, v.end), (None, None), "出ていないメニューを読んだ");
        }
        f.safe.insert(PAUSE, 0x0001_0002);
        f.safe.insert(MATCH_END_MENU, 0x0000_0001);
        f.safe.insert(END_MENU + MENU_OBJ_STATE, 3);
        f.safe.insert(END_MENU + MENU_OBJ_FRAMES, 40);
        f.safe.insert(LOADING, 1);
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        let v = g.play_view().unwrap();
        assert!(v.loading && v.paused && v.end_menu);
        assert_eq!(
            v.pause,
            Some(MenuState {
                state: 2,
                frames: 9
            })
        );
        assert_eq!(
            v.end,
            Some(MenuState {
                state: 3,
                frames: 40
            })
        );
        // 一時停止のメニューの状態が読み取れなければ試合の場面ごと読み取れない扱いにせず、そのフィールドだけNone。
        f.safe.remove(&(PAUSE_MENU + MENU_OBJ_FRAMES));
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        assert_eq!(g.play_view().unwrap().pause, None);
        f.safe.remove(&PAUSE);
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        assert_eq!(g.play_view(), None);
    }

    #[test]
    fn the_end_view_reads_the_flag_byte_of_the_pause_word_and_the_menu_only_when_shown() {
        let mut f = fake();
        // 一時停止のワードの3バイト目だけがフラグ。2バイト目（Story / Extraのフラグ`0x4A7ECD`）は見ない。
        f.data.insert(PAUSE, 0x0000_0100);
        f.data.insert(END_MENU + MENU_OBJ_STATE, 2);
        f.data.insert(END_MENU + MENU_OBJ_FRAMES, 7);
        {
            let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
            assert_eq!(g.end_view(), end_pick::View::default());
        }
        f.data.insert(PAUSE, 0x0001_0001);
        let mut g = NetGame::new(&mut f, None, GameAddrs::th09());
        assert_eq!(
            g.end_view(),
            end_pick::View {
                shown: true,
                paused: true,
                state: 2,
                frames: 7,
            }
        );
        // 出ていなければメニューのフィールドは読まない。読み取れなければ「出ていない」。
        let mut reads = Vec::new();
        let v = read_end_view(&mut |a| {
            reads.push(a);
            Some(0x0000_0001)
        });
        assert_eq!((v.shown, v.paused, reads), (false, true, vec![PAUSE]));
        assert_eq!(read_end_view(&mut |_| None), end_pick::View::default());
        let v = read_end_view(&mut |a| (a == PAUSE).then_some(0x0001_0000));
        assert_eq!(
            v,
            end_pick::View::default(),
            "メニューの欄が読めないのに出ていると返した"
        );
        // 試合の毎tickに直に読むフィールドは設置の前に読み取り可能と検証する範囲に入っている。
        let ranges = direct_ranges();
        for a in [PAUSE, END_MENU + MENU_OBJ_STATE, END_MENU + MENU_OBJ_FRAMES] {
            assert!(
                ranges.iter().any(|&(s, n)| s <= a && a + 4 <= s + n),
                "{a:#X} が直に読む範囲の外"
            );
        }
    }
}
