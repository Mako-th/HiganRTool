//! ネットワーク対戦の間も、自分の PC にだけゲーム自身のスクリーンショットを撮らせるキー（P・Home）の取り方（pure。unsafe 無し）。
//!
//! ツールが入力ワードを握っている tick の語は、自分の分も相手と同じマスクをかける（`MATCH_SEND_MASK` に P/Home の `0x0800` は無い。
//! 相手の PC にスクリーンショットを書かせないため）ので、ゲーム自身の判定（Present のラッパの中で `[2]+0x06 & 0x0800` を見る）に届かない。
//! そこで DLL が自分のキーボードの押した瞬間だけを覚え、Present のラッパを呼ぶ間だけ `0x0800` を立てて戻す
//! （立てて戻す書き込みは `shot_key_win.rs`）。入力ワード・リプレイの語・CHECK・観戦には触れない。
//!
//! - 押した瞬間は P と Home の「どちらか」で取る（ゲームは同じビットに割り当てているので、片方を押したまま他方を押しても 1 枚）。
//! - 取るのはツールが握っている tick だけ。握っていない tick（`Action::Local`）はゲーム自身の poll が P を読むので、二重に撮らない。
//! - 押下は次の Present のラッパまで持ち越す（フレームスキップで Present が飛んでも失われない）。1 回の押下で 1 枚。
//! - 読めない tick（別のアプリが前面・文字の入力中）の後は、押されたままのキーを押下と取らない。

#![forbid(unsafe_code)]

use crate::th09_addr::SHOT_BIT;

const PREV_BIT: u32 = 1;
const PENDING_BIT: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotKeys {
    /// 前の tick に P か Home が押されていた（読めなかった tick は押されていたことにする）。
    prev: bool,
    /// 取った押下がまだ Present のラッパに届いていない。
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

    /// 原子変数に預けるための 2 ビットの形。
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

    /// 1 tick に 1 回。`down` は P か Home が今押されているか（読んではいけない間は `None`）、`held` はこの tick でツールが入力を握っているか。
    /// 握っていない tick は押下を作らず、持ち越しも捨てる（その tick からゲーム自身の poll が読むので、遅れて撮らない）。
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

    /// Present のラッパを呼ぶ直前に 1 回。持ち越した押下があれば真を返して消す（1 回の押下で 1 枚）。
    pub fn take(&mut self) -> bool {
        core::mem::take(&mut self.pending)
    }
}

/// ラッパを呼ぶ間だけ立てる語。すでに立っていれば `None`（ゲームはそのまま撮るので、触らず、戻すときも落とさない）。
pub fn raise(word: u16) -> Option<u16> {
    (word & SHOT_BIT == 0).then_some(word | SHOT_BIT)
}

/// ラッパから戻ったら、立てたビットだけを落とす。ほかのビットは今の値のまま（立てている間に書き換わったものを巻き戻さない）。
pub fn lower(word: u16) -> u16 {
    word & !SHOT_BIT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 読める tick（握っている）で押下の列を流し、各 tick の後に押下が持ち越されているかを返す。
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
        // 最初の読みで押されていたキーは押下でない。離して押し直した tick で 1 回。押し続けは新しい押下を作らない。
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
        // 握っていない tick（ゲーム自身の poll が読む）の押下は作らない。握っていない間に押して、握る tick になっても押したままなら押下でない。
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
        // 持ち越した押下は、握っていない tick が来たら捨てる（遅れて 1 枚撮らない）。
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
        // 読めない tick は持ち越しを消さない（前面を離れても、押した分の 1 枚は残る）。
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
