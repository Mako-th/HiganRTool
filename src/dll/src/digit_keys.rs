//! Ctrl+上段の数字（0〜9）でディレイを替える入口の「押した瞬間」の取り方（pure。unsafe 無し）。
//! 0〜8 がその値、9 が auto（`higanr_core::delay_pick::AUTO`）。Ctrl+Tab は同期方式の切替（キャラセレクトの間）。
//! Ctrl+C は観戦コードのコピーの頼み（つながっている間。`net_ui::NetUi::copy_tick` が同じ取り方で読む）。
//! キーボードの読み（`GetAsyncKeyState`）は `digit_keys_win.rs`、「読んでよい間」の判断は `net_game::NetGame::digit_keys`（ゲートの式は `may_read`）、
//! 押下の行き先は `NetPlay::digit_tick`。
//!
//! 数字と Tab を使うのは、ゲームの入力の関数 `0x0042B850` が `GetKeyboardState` の表から読むキーに上段の数字 0〜9 と Tab が無いため
//! （表の添字 `-0xd4`〜`-0xcb` と `-0xfb` の参照が 0 件）。テンキーは方向と衝突するので読まない。
//! ゲームは Ctrl（VK_CONTROL。`0x0042BAAB`）を入力ワードの `0x0100` として読む（キャラ・マップ選択の「ランダム」）ので、
//! 選択の画面で Ctrl+数字・Ctrl+Tab を押すと「ランダム」も回る（`0x0100` は落とさず、そのままゲームと相手へ届く）。
//!
//! 1 tick に 1 回、押下の集合（bit `d` = 数字 `d`・Tab・C・Ctrl・Alt/Win）を渡され、前の tick に離れていて今押されている数字・Tab・C だけを取る
//! （押し続けは 1 回）。取るのは、その押した瞬間に Ctrl が押されていて Alt・Win が押されていないときだけ: Ctrl を後から押しても押したままの
//! 数字は取らず、Alt+Tab・Win+Tab でウィンドウを切り替えて戻った後に押しっぱなしの Tab も取らない。読んでよくない tick（`None`）が挟まると、
//! 次に読めた tick で押されたままのキーを押下と取らない（前面に戻ったとき・文字の入力を閉じたときの誤爆を防ぐ）。
#![forbid(unsafe_code)]

use higanr_core::delay_pick::{AUTO, bounds};
use higanr_core::wire::Mode;

/// 読んだ集合の bit10: Tab（仮想キー `VK_TAB`）。ゲームは Tab を読まない（表の添字 `-0xfb` の参照が 0 件）ので衝突しない。
pub const TAB_BIT: u16 = 1 << 10;

/// 読んだ集合の bit11: Alt か Win が押されている。押した瞬間の集合には出ない。
pub const MOD_BIT: u16 = 1 << 11;

/// 読んだ集合の bit12: Ctrl が押されている。押した瞬間の集合には出ない。
pub const CTRL_BIT: u16 = 1 << 12;

/// 読んだ集合の bit13: C（Ctrl+C で観戦コードをコピーする）。ゲームの入力の関数 `0x0042B850` は C を読まない
/// （Z・X・Shift・Esc・矢印・Ctrl・Q・S・P・Home・Enter・D・R だけ）ので、Ctrl の
/// 扱いは Ctrl+数字と同じで足りる。
pub const C_BIT: u16 = 1 << 13;

/// 押した瞬間を取る対象のキー（数字 0〜9・Tab・C）の bit。
pub const KEYS_MASK: u16 = 0x07FF | C_BIT;

/// 読んだ集合の全部のビット。
pub const ALL_HELD: u16 = KEYS_MASK | MOD_BIT | CTRL_BIT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigitEdge {
    prev: u16,
}

impl Default for DigitEdge {
    fn default() -> Self {
        Self::new()
    }
}

impl DigitEdge {
    /// 最初の読みの前から押されていたキーは押下にしない（`prev` は押されているとみなす）。
    pub const fn new() -> Self {
        DigitEdge { prev: KEYS_MASK }
    }

    /// この tick に読んだ集合（読んでよくなければ `None`）を渡し、押した瞬間の集合を返す。`None` の後の最初の読みは押下を作らない。
    /// 押した瞬間の判断は修飾キーを含まない生のキーで行う（`prev` は生の集合）ので、Ctrl が無い間に押したキーは、後で Ctrl を押しても
    /// 押下にならず、Alt・Win を先に離しても押したままの Tab は押下にならない。
    pub fn feed(&mut self, held: Option<u16>) -> u16 {
        match held {
            Some(h) => {
                let h = h & ALL_HELD;
                let keys = h & KEYS_MASK;
                let pressed = keys & !self.prev;
                self.prev = keys;
                if h & CTRL_BIT != 0 && h & MOD_BIT == 0 {
                    pressed
                } else {
                    0
                }
            }
            None => {
                self.prev = KEYS_MASK;
                0
            }
        }
    }
}

/// 本物のキーを読んでよい間か: ゲームの前面の旗（`th09_addr::APP_ACTIVE`）が立っていて、かつ Windows の前面のウィンドウがこのゲームのウィンドウのときだけ。
/// 旗は意味が未照合で、ほかのアプリのウィンドウが前面なのに打った数字を拾った疑いがあったので OS の前面のウィンドウでも縛る。旗が 0 なら前面の問い合わせは呼ばない。
pub fn may_read(app_active: bool, foreground_is_game: impl FnOnce() -> bool) -> bool {
    app_active && foreground_is_game()
}

pub fn tab_pressed(pressed: u16) -> bool {
    pressed & TAB_BIT != 0
}

/// 押した瞬間の集合に Ctrl+C（観戦コードのコピーの頼み）があるか。
pub fn copy_pressed(pressed: u16) -> bool {
    pressed & C_BIT != 0
}

/// 押した瞬間の集合から、遅れの量の押下（`Connected::press_delay` に渡す値）を 1 つ選ぶ。0〜`bounds` の上限はその値、9 は `AUTO`。
/// 同じ tick に複数押したら番号の大きい方が後勝ち。何も押していなければ `None`。
pub fn pick(pressed: u16) -> Option<u8> {
    // 数字 d の値は d そのもの（0〜8）で、9 は `AUTO`（= 9）。`bounds` の上限と `AUTO` が隣り合わせであることを縛る。
    debug_assert_eq!(bounds(Mode::Rollback).1 + 1, AUTO);
    (0..=AUTO).rev().find(|&d| pressed & (1 << d) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K0: u16 = 1 << 0;
    const K5: u16 = 1 << 5;
    const K8: u16 = 1 << 8;
    const K9: u16 = 1 << 9;
    const C: u16 = CTRL_BIT;

    #[test]
    fn only_the_press_edge_counts_and_the_first_read_makes_none() {
        let mut e = DigitEdge::new();
        assert_eq!(e.feed(Some(K5 | C)), 0, "最初の読みの前から押していた");
        assert_eq!(e.feed(Some(K5 | C)), 0, "押し続け");
        assert_eq!(e.feed(Some(C)), 0);
        assert_eq!(e.feed(Some(K5 | C)), K5, "離して押し直した");
        assert_eq!(e.feed(Some(K5 | K0 | C)), K0, "別のキーを足した");
        assert_eq!(e.feed(Some(K9 | C)), K9, "端のキー");
        assert_eq!(
            e.feed(Some(0xC000 | K8 | C)),
            K8,
            "範囲の外のビットは押下でない"
        );
        for d in 0..10u16 {
            let mut f = DigitEdge::new();
            f.feed(Some(C));
            assert_eq!(f.feed(Some((1 << d) | C)), 1 << d, "数字 {d}");
        }
    }

    #[test]
    fn a_digit_or_tab_counts_only_when_ctrl_is_down_at_the_moment_it_is_pressed() {
        let mut e = DigitEdge::new();
        e.feed(Some(0));
        assert_eq!(e.feed(Some(K5)), 0, "Ctrl 無しの数字");
        assert_eq!(
            e.feed(Some(K5 | C)),
            0,
            "Ctrl を後から押しても、押したままの数字は取らない"
        );
        assert_eq!(e.feed(Some(C)), 0);
        assert_eq!(e.feed(Some(K5 | C)), K5, "Ctrl を押したまま押し直した");
        assert_eq!(e.feed(Some(0)), 0);
        assert_eq!(e.feed(Some(TAB_BIT)), 0, "Ctrl 無しの Tab");
        assert_eq!(e.feed(Some(TAB_BIT | C)), 0, "Ctrl を後から押した Tab");
        assert_eq!(e.feed(Some(0)), 0);
        assert_eq!(e.feed(Some(TAB_BIT | C)), TAB_BIT, "Ctrl+Tab");
        assert_eq!(e.feed(Some(TAB_BIT | C)), 0, "押し続け");
        assert_eq!(e.feed(Some(C)), 0, "Ctrl の印そのものは押下でない");
        assert_eq!(e.feed(Some(C | K9 | TAB_BIT)), K9 | TAB_BIT);
        assert!(tab_pressed(TAB_BIT) && !tab_pressed(K5 | K9));
    }

    #[test]
    fn alt_and_win_stop_both_digits_and_tab() {
        let mut e = DigitEdge::new();
        e.feed(Some(0));
        assert_eq!(e.feed(Some(TAB_BIT | C | MOD_BIT)), 0, "Ctrl+Alt+Tab");
        assert_eq!(e.feed(Some(TAB_BIT | C)), 0, "Alt を離した瞬間の誤爆");
        assert_eq!(e.feed(Some(0)), 0);
        assert_eq!(e.feed(Some(K5 | C | MOD_BIT)), 0, "Ctrl+Alt+数字");
        assert_eq!(e.feed(Some(K5 | C)), 0, "Alt を離した瞬間の誤爆");
        assert_eq!(e.feed(Some(0)), 0);
        assert_eq!(e.feed(Some(TAB_BIT | C)), TAB_BIT, "離して押し直せば効く");
        assert_eq!(e.feed(Some(0)), 0);
        assert_eq!(
            e.feed(Some(TAB_BIT | K5 | C | MOD_BIT)),
            0,
            "Alt か Win があれば数字も Tab も取らない"
        );
        assert_eq!(e.feed(Some(MOD_BIT)), 0, "修飾キーの印そのものは押下でない");
    }

    #[test]
    fn a_gap_in_reading_suppresses_the_held_keys_until_they_are_released() {
        let mut e = DigitEdge::new();
        e.feed(Some(0));
        assert_eq!(e.feed(None), 0);
        assert_eq!(e.feed(Some(K5 | C)), 0, "読めない間から押していたキー");
        assert_eq!(e.feed(Some(K5 | C)), 0);
        assert_eq!(e.feed(Some(C)), 0);
        assert_eq!(e.feed(Some(K5 | C)), K5, "離して押せば効く");
        assert_eq!(e.feed(None), 0);
        assert_eq!(e.feed(Some(C)), 0);
        assert_eq!(e.feed(Some(K0 | C)), K0);
        assert_eq!(e.feed(None), 0);
        assert_eq!(e.feed(Some(TAB_BIT | C)), 0);
        assert_eq!(e.feed(Some(C)), 0);
        assert_eq!(e.feed(Some(TAB_BIT | C)), TAB_BIT);
    }

    #[test]
    fn ctrl_c_counts_once_per_press_and_only_with_ctrl_down_at_that_moment() {
        let mut e = DigitEdge::new();
        e.feed(Some(0));
        assert_eq!(e.feed(Some(C_BIT)), 0, "Ctrl 無しの C");
        assert_eq!(
            e.feed(Some(C_BIT | C)),
            0,
            "Ctrl を後から押しても、押したままの C は取らない"
        );
        assert_eq!(e.feed(Some(0)), 0);
        let p = e.feed(Some(C_BIT | C));
        assert_eq!(p, C_BIT);
        assert!(copy_pressed(p));
        for _ in 0..10 {
            assert_eq!(e.feed(Some(C_BIT | C)), 0, "押しっぱなしで繰り返さない");
        }
        assert_eq!(e.feed(Some(C)), 0, "C だけを離した");
        assert_eq!(
            e.feed(Some(C_BIT | C | MOD_BIT)),
            0,
            "Ctrl+Alt+C は取らない"
        );
        assert_eq!(e.feed(Some(C_BIT | C)), 0, "Alt を離した瞬間の誤爆");
        assert_eq!(e.feed(None), 0);
        assert_eq!(e.feed(Some(C_BIT | C)), 0, "読めない間から押していた");
        assert_eq!(e.feed(Some(C)), 0);
        assert_eq!(e.feed(Some(C_BIT | C)), C_BIT, "離して押せば効く");
        // 数字と一緒でも、遅れの量の選びは C に影響されず、C は遅れの量にも Tab にもならない。
        let mut e = DigitEdge::new();
        e.feed(Some(C));
        let p = e.feed(Some(C | C_BIT | K5));
        assert_eq!((pick(p), copy_pressed(p)), (Some(5), true));
        assert_eq!(pick(C_BIT), None);
        assert!(!tab_pressed(C_BIT) && !copy_pressed(K5 | TAB_BIT));
    }

    #[test]
    fn reading_needs_both_the_flag_and_the_game_window_in_front() {
        use std::cell::Cell;
        let asked = Cell::new(0);
        let ask = |v: bool| {
            asked.set(asked.get() + 1);
            v
        };
        assert!(
            !may_read(true, || ask(false)),
            "前面が別のアプリなのに読んだ"
        );
        assert!(may_read(true, || ask(true)), "旗も前面も満たすのに読まない");
        assert_eq!(asked.get(), 2);
        assert!(!may_read(false, || ask(true)), "旗が 0 なのに読んだ");
        assert_eq!(asked.get(), 2, "旗が 0 なのに前面を問い合わせた");
    }

    #[test]
    fn digits_zero_to_eight_are_values_and_nine_is_auto() {
        for d in 0..=8u8 {
            assert_eq!(pick(1 << d), Some(d));
        }
        assert_eq!(pick(K9), Some(AUTO));
        assert_eq!(AUTO, 9);
        assert_eq!(pick(K0 | K5), Some(5), "大きい方が後勝ち");
        assert_eq!(pick(K5 | K9), Some(AUTO));
        assert_eq!(pick(0), None);
        assert_eq!(pick(0xFC00), None, "範囲の外のビット");
    }
}
