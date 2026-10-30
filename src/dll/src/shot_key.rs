//! ネットワーク対戦の間も自分のPCにだけゲーム自身のスクリーンショットを撮らせるキー（P・Home）の取り方（pure。unsafe無し）。
//!
//! ツールが入力ワードを占有しているtickでは、自分の分のワードにも相手と同じマスクをかける。
//! `MATCH_SEND_MASK`にP/Homeの`0x0800`は無い（相手のPCにスクリーンショットを書かせないため）。
//! そのためゲーム自身の判定（Presentのラッパの中で`[2]+0x06 & 0x0800`を見る）に押下が届かない。
//! そこでDLLが自分のキーボードの押した瞬間だけを覚え、Presentのラッパを呼ぶ間だけ`0x0800`を立てて戻す（書き込みは`shot_key_win.rs`）。
//! 入力ワード・リプレイのワード・CHECK・観戦には触れない。
//!
//! - 押した瞬間はPとHomeの「どちらか」で取る（ゲームは同じビットに割り当てているので片方を押したまま他方を押しても1枚）。
//! - 取るのはツールが入力を占有しているtickだけ。占有していないtick（`Action::Local`）はゲーム自身のpollがPを読むので二重に撮らない。
//! - 押下は次のPresentのラッパまで持ち越す（フレームスキップで失われない。1回の押下で1枚）。
//!   読めないtick（別のアプリが前面・文字の入力中）の後は押されたままのキーを押下と取らない。

#![forbid(unsafe_code)]

use crate::th09_addr::SHOT_BIT;

const PREV_BIT: u32 = 1;
const PENDING_BIT: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotKeys {
    /// 前のtickにPかHomeが押されていた（読めなかったtickは押されていたことにする）。
    prev: bool,
    /// 取った押下がまだPresentのラッパに届いていない。
    pending: bool,
}

impl Default for ShotKeys {
    fn default() -> Self {
        Self::new()
    }
}

impl ShotKeys {
    /// 最初の読みの前から押されていたキーは押下にしない。
    pub const fn new() -> Self {
        ShotKeys {
            prev: true,
            pending: false,
        }
    }

    /// アトミック変数に保存するための2ビットの形。
    pub const fn from_bits(b: u32) -> Self {
        ShotKeys {
            prev: b & PREV_BIT != 0,
            pending: b & PENDING_BIT != 0,
        }
    }

    pub const fn to_bits(self) -> u32 {
        (self.prev as u32 * PREV_BIT) | (self.pending as u32 * PENDING_BIT)
    }

    pub const fn pending(self) -> bool {
        self.pending
    }

    /// 1 tickに1回呼ぶ。
    /// `down`はPかHomeが今押されているか（読んではいけない間は`None`）。
    /// `held`はこのtickでツールが入力を占有しているか。
    /// 占有していないtickは押下を作らず、持ち越しも捨てる（そのtickからゲーム自身のpollが読むので遅れて撮らない）。
    pub fn tick(&mut self, down: Option<bool>, held: bool) {
        let edge = match down {
            Some(d) => {
                let e = d && !self.prev;
                self.prev = d;
                e
            }
            None => {
                self.prev = true;
                false
            }
        };
        if !held {
            self.pending = false;
        } else if edge {
            self.pending = true;
        }
    }

    /// Presentのラッパを呼ぶ直前に1回。持ち越した押下があれば真を返して消す（1回の押下で1枚）。
    pub fn take(&mut self) -> bool {
        core::mem::take(&mut self.pending)
    }
}

/// ラッパを呼ぶ間だけビットを立てるワード。すでに立っていれば`None`（ゲームはそのまま撮るので触らない。戻すときも落とさない）。
pub fn raise(word: u16) -> Option<u16> {
    (word & SHOT_BIT == 0).then_some(word | SHOT_BIT)
}

/// ラッパから戻ったら立てたビットだけを落とす。ほかのビットは今の値のまま（立てている間に書き換わったものを巻き戻さない）。
pub fn lower(word: u16) -> u16 {
    word & !SHOT_BIT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 読めるtick（入力を占有している）で押下の列を渡して、各tickの後に押下が持ち越されているかを返す。
    fn run(seq: &[bool]) -> Vec<bool> {
        let mut s = ShotKeys::new();
        seq.iter()
            .map(|&d| {
                s.tick(Some(d), true);
                s.pending()
            })
            .collect()
    }

    #[test]
    fn only_the_press_edge_counts_and_a_key_held_since_the_start_does_not() {
        // 最初の読みで押されていたキーは押下でない。離して押し直したtickで1回。押し続けは新しい押下を作らない。
        assert_eq!(
            run(&[true, true, false, true, true]),
            [false, false, false, true, true],
            "持ち越しは Present まで残る（消すのは take）"
        );
        let mut s = ShotKeys::new();
        s.tick(Some(false), true);
        s.tick(Some(true), true);
        assert!(s.take(), "押した瞬間");
        s.tick(Some(true), true);
        assert!(!s.take(), "押し続けで毎フレーム撮った");
        s.tick(Some(false), true);
        s.tick(Some(true), true);
        assert!(s.take(), "離して押し直した");
    }

    #[test]
    fn one_press_makes_one_shot_even_if_present_is_skipped_for_several_ticks() {
        let mut s = ShotKeys::new();
        s.tick(Some(false), true);
        s.tick(Some(true), true);
        for _ in 0..5 {
            s.tick(Some(true), true);
            assert!(s.pending(), "Present が飛んだ間に押下が失われた");
        }
        assert!(s.take(), "最初の Present で 1 枚");
        assert!(!s.take(), "同じ押下で 2 枚目");
        s.tick(Some(true), true);
        assert!(!s.take(), "押し続けのまま次のフレームでも撮った");
    }

    #[test]
    fn nothing_is_made_or_kept_on_a_tick_the_tool_does_not_hold() {
        // 占有していないtick（ゲーム自身のpollが読む）の押下は作らない。占有していない間に押して占有するtickになっても押したままなら押下でない。
        let mut s = ShotKeys::new();
        s.tick(Some(false), false);
        s.tick(Some(true), false);
        assert!(
            !s.pending(),
            "握っていない tick で押下を取った（二重に撮る）"
        );
        s.tick(Some(true), true);
        assert!(
            !s.pending(),
            "握っていない間から押していたキーを押下と取った"
        );
        s.tick(Some(false), true);
        s.tick(Some(true), true);
        assert!(s.pending());
        // 持ち越した押下は占有していないtickが来たら捨てる（遅れて1枚撮らない）。
        s.tick(Some(true), false);
        assert!(!s.take(), "握っていない tick の後に持ち越しが残った");
    }

    #[test]
    fn a_gap_in_reading_suppresses_held_keys_until_they_are_released() {
        let mut s = ShotKeys::new();
        s.tick(Some(false), true);
        s.tick(None, true);
        s.tick(Some(true), true);
        assert!(!s.pending(), "読めない間から押していたキーを押下と取った");
        s.tick(Some(false), true);
        s.tick(Some(true), true);
        assert!(s.pending(), "離して押せば取る");
        // 読めないtickは持ち越しを消さない（前面を離れても押した分の1枚は残る）。
        s.tick(None, true);
        assert!(s.pending());
        assert!(s.take());
    }

    #[test]
    fn bits_round_trip() {
        for prev in [false, true] {
            for pending in [false, true] {
                let s = ShotKeys { prev, pending };
                assert_eq!(ShotKeys::from_bits(s.to_bits()), s);
            }
        }
        assert_eq!(
            ShotKeys::from_bits(ShotKeys::new().to_bits()),
            ShotKeys::new()
        );
        assert_eq!(ShotKeys::from_bits(0xFFFF_FFFC), ShotKeys::from_bits(0));
    }

    #[test]
    fn raise_sets_only_the_shot_bit_and_lower_drops_only_it() {
        assert_eq!(SHOT_BIT, 0x0800);
        assert_eq!(raise(0x1234), Some(0x1A34));
        assert_eq!(raise(0), Some(0x0800));
        assert_eq!(raise(0x1A34), None, "すでに立っているのに立て直した");
        assert_eq!(lower(0x1A34), 0x1234);
        assert_eq!(lower(0xFFFF), 0xF7FF, "ほかのビットを落とした");
        assert_eq!(lower(0x1234), 0x1234);
    }
}
