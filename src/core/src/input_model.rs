//! feed `0x0042ACE0` の模型。入力語を InputManager の 3 本 `[0]`/`[1]`/`[2]` に展開する規則
//! （prev・リピート発火・押した瞬間・離した瞬間）。逆アセンブル `42ace0`〜`42ad62` を読んで確かめた。
//! feed 1 個ぶんの状態機械だけを持ち、1P・2P・合成の 3 本の呼び分けは呼び出し側が行う。

/// 台本の語のうち、ゲームへ送ってよいビット。Z・X・Shift・Esc・方向の 8 ビットだけ。
/// 相手の語のビットは自分の側でも効くので、Enter や P/Home（スクリーンショット `0x0042DBAA`）などは送らない。
pub const SEND_MASK: u16 = 0x00FF;

/// 台本の `p1=`/`p2=` に名前で書けるキーの値（InputManager の `+0x00` の語のビット）。
/// 方向は分岐表 `0x0041C89C` を exe から読んだ値（実測の向きの一致 97.80%、母数 1,152,165 tick）。
/// `ESC` は `0x0042BABC` を根拠に実測済み。`Z`・`X`・`SHIFT` は逆アセンブルの読みで、実機とは未照合。
pub mod keys {
    pub const Z: u16 = 0x0001;
    pub const X: u16 = 0x0002;
    pub const SHIFT: u16 = 0x0004;
    pub const ESC: u16 = 0x0008;
    pub const UP: u16 = 0x0010;
    pub const DOWN: u16 = 0x0020;
    pub const LEFT: u16 = 0x0040;
    pub const RIGHT: u16 = 0x0080;
}

/// feed が作る 1 本ぶんの状態（InputManager の `+0x00`〜`+0x29` のコピー）。
/// 欄はゲームのオフセットに合わせる: `cur`=`+0x00`、`prev`=`+0x02`、`repeat`=`+0x04`、`pressed`=`+0x06`、
/// `released`=`+0x08`、`cnt[16]`=`+0x0A`〜（各キーのリピートカウンタ u16）。`+0x2A` 以降は feed が書かないので持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputState {
    pub cur: u16,
    pub prev: u16,
    pub repeat: u16,
    pub pressed: u16,
    pub released: u16,
    pub cnt: [u16; 16],
}

/// リピートの閾値。`42ad1d: cmpw $0x1a,(%esi)`。0 → 26 まで数えて発火する（初回 26 フレーム）。
const REPEAT_THRESHOLD: u16 = 0x1A;

/// 発火のたびに引く量。`42ad27: addw $0xfff8,(%esi)`。以後は 8 フレームごとに発火する。
const REPEAT_DECREMENT: u16 = 8;

impl InputState {
    pub fn new() -> Self {
        Self::default()
    }

    /// feed 1 回ぶん。`word` は新しい入力語の下位 16 ビット（fastcall `edx`）。
    ///
    /// - `42acf2`/`42acf7`: `prev ← 旧 cur`（`cur` を書く前の値）
    /// - `42acfb`: `cur ← word`
    /// - `42ad06`: `repeat ← 0`（発火のビットは毎回作り直す）
    /// - `42ad0c`〜`42ad3d`: b = 0..15 を新しい `cur` のビットで判定。立っていれば `cnt[b] += 1` し、
    ///   `cnt[b] >= 0x1A` なら `repeat` の b ビットを立てて `cnt[b] -= 8`。立っていなければ `cnt[b] = 0`
    /// - `42ad3f`〜`42ad53`: `changed = cur ^ prev`
    /// - `42ad56`: `released = changed & !cur`、`42ad5b`: `pressed = changed & cur`
    pub fn feed(&mut self, word: u16) {
        self.prev = self.cur;
        self.cur = word;
        self.repeat = 0;
        for b in 0..16u32 {
            let bit = 1u16 << b;
            if self.cur & bit != 0 {
                self.cnt[b as usize] = self.cnt[b as usize].wrapping_add(1);
                if self.cnt[b as usize] >= REPEAT_THRESHOLD {
                    self.repeat |= bit;
                    self.cnt[b as usize] -= REPEAT_DECREMENT;
                }
            } else {
                self.cnt[b as usize] = 0;
            }
        }
        let changed = self.cur ^ self.prev;
        self.released = changed & !self.cur;
        self.pressed = changed & self.cur;
    }
}

#[cfg(test)]
mod tests {
    //! 期待値は手で数えた表で書く（`feed` の式やループから作らない）。
    use super::*;

    const BIT0: u16 = 0x0001;
    const BIT1: u16 = 0x0002;

    #[test]
    fn repeat_fires_at_26_then_every_8_frames() {
        let mut st = InputState::new();
        let expect_fire: [bool; 51] = {
            let mut a = [false; 51];
            a[26] = true;
            a[34] = true;
            a[42] = true;
            a[50] = true;
            a
        };
        for frame in 1..=50u32 {
            st.feed(BIT0);
            let fired = st.repeat & BIT0 != 0;
            assert_eq!(
                fired, expect_fire[frame as usize],
                "frame {frame}: repeat={:#06x}",
                st.repeat
            );
        }
    }

    #[test]
    fn pressed_and_released_are_exactly_one_frame() {
        let mut st = InputState::new();

        st.feed(BIT0);
        assert_eq!(st.pressed, BIT0, "frame1: pressed");
        assert_eq!(st.released, 0, "frame1: released");

        st.feed(BIT0);
        assert_eq!(st.pressed, 0, "frame2: pressed should not repeat");
        assert_eq!(st.released, 0, "frame2: released");
        st.feed(BIT0);
        assert_eq!(st.pressed, 0, "frame3: pressed should not repeat");
        assert_eq!(st.released, 0, "frame3: released");

        st.feed(0);
        assert_eq!(st.pressed, 0, "frame4: pressed");
        assert_eq!(st.released, BIT0, "frame4: released");

        st.feed(0);
        assert_eq!(st.released, 0, "frame5: released should not repeat");
        assert_eq!(st.pressed, 0, "frame5: pressed");
    }

    #[test]
    fn releasing_resets_the_counter_to_zero() {
        let mut st = InputState::new();
        for frame in 1..=10u32 {
            st.feed(BIT0);
            assert_eq!(st.repeat & BIT0, 0, "frame {frame}: must not fire yet");
        }
        st.feed(0);
        assert_eq!(st.cnt[0], 0, "counter must reset to 0 on release");

        for frame in 1..=25u32 {
            st.feed(BIT0);
            assert_eq!(
                st.repeat & BIT0,
                0,
                "frame {frame} after re-press: must not fire before 26"
            );
        }
        st.feed(BIT0);
        assert_ne!(st.repeat & BIT0, 0, "frame 26 after re-press: must fire");
    }

    #[test]
    fn two_bits_offset_do_not_interfere() {
        let mut st = InputState::new();
        for frame in 1..=60u32 {
            let word = if frame < 5 { BIT0 } else { BIT0 | BIT1 };
            st.feed(word);

            if frame == 5 {
                assert_eq!(st.pressed & BIT1, BIT1, "frame5: BIT1 pressed");
            } else {
                assert_eq!(st.pressed & BIT1, 0, "frame{frame}: BIT1 pressed only once");
            }

            let bit0_should_fire = matches!(frame, 26 | 34 | 42 | 50 | 58);
            assert_eq!(
                st.repeat & BIT0 != 0,
                bit0_should_fire,
                "frame{frame}: BIT0 repeat"
            );

            let bit1_should_fire = matches!(frame, 30 | 38 | 46 | 54);
            assert_eq!(
                st.repeat & BIT1 != 0,
                bit1_should_fire,
                "frame{frame}: BIT1 repeat"
            );
        }
    }

    #[test]
    fn all_bits_pressed_together_share_the_single_bit_schedule() {
        let mut st = InputState::new();
        let expect_fire: [bool; 51] = {
            let mut a = [false; 51];
            a[26] = true;
            a[34] = true;
            a[42] = true;
            a[50] = true;
            a
        };
        for frame in 1..=50u32 {
            st.feed(0xFFFF);
            let fired_all = st.repeat == 0xFFFF;
            let fired_none = st.repeat == 0;
            if expect_fire[frame as usize] {
                assert!(
                    fired_all,
                    "frame {frame}: all bits should fire together, repeat={:#06x}",
                    st.repeat
                );
            } else {
                assert!(
                    fired_none,
                    "frame {frame}: no bit should fire, repeat={:#06x}",
                    st.repeat
                );
            }
        }
        let mut st2 = InputState::new();
        st2.feed(0xFFFF);
        assert_eq!(st2.pressed, 0xFFFF, "frame1: all bits pressed");
        assert_eq!(st2.released, 0, "frame1: released");
        st2.feed(0xFFFF);
        assert_eq!(st2.pressed, 0, "frame2: pressed should not repeat");
    }
}
