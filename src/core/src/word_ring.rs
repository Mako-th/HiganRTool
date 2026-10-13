//! マッチのワードの置き場（apply_frame を印にした固定長のリング）と、INPUT の受理のウィンドウ。`lockstep.rs` と `rollback.rs` が使う。
//!
//! ゲームスレッドで呼ぶので、ヒープを使わず、パニックしうる添字を置かない。印が合わない枠は「無い」と読むので、
//! 古いワードが残っていても取り違えない。

/// ワードのリングの枠の数。未確認のワードは delay 方式で ≤ 30 ワード・rollback で D＋R（≤ 9＋16）程度なので、
/// 128 あれば溢れない（受理のウィンドウ ±64 ＋ 32 ワードより広い）。
pub(crate) const RING: usize = 128;

/// 受理のウィンドウの半幅（`e − 64 ≤ base ≤ e + 64`）。
pub(crate) const WINDOW: u32 = 64;

/// リングの空き枠の印。apply_frame は u32 で折り返しを考えないので、この値には届かない。
const EMPTY: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Ring {
    tag: [u32; RING],
    word: [u16; RING],
}

impl Ring {
    pub(crate) const fn new() -> Self {
        Ring {
            tag: [EMPTY; RING],
            word: [0; RING],
        }
    }

    fn slot(x: u32) -> usize {
        // RING は定数なので、添字は必ず 0..RING に収まる（パニックしない）。
        (x as usize) % RING
    }

    pub(crate) fn get(&self, x: u32) -> Option<u16> {
        let s = Self::slot(x);
        if self.tag[s] == x && x != EMPTY {
            Some(self.word[s])
        } else {
            None
        }
    }

    pub(crate) fn put(&mut self, x: u32, w: u16) {
        if x == EMPTY {
            return;
        }
        let s = Self::slot(x);
        self.tag[s] = x;
        self.word[s] = w;
    }

    pub(crate) fn clear(&mut self) {
        self.tag = [EMPTY; RING];
    }
}

/// `base` が受理のウィンドウ（次に要る apply_frame `e` の ±`WINDOW`）の中か。
pub(crate) fn in_window(base: u32, e: u32) -> bool {
    base >= e.saturating_sub(WINDOW) && base <= e.saturating_add(WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_answers_only_for_its_own_apply_frame() {
        let mut r = Ring::new();
        assert_eq!(r.get(0), None, "作った直後は空");
        r.put(5, 0x55);
        assert_eq!(r.get(5), Some(0x55));
        assert_eq!(r.get(5 + RING as u32), None);
        r.put(5 + RING as u32, 0x66);
        assert_eq!(r.get(5), None, "上書きされた古いワードを返さない");
        assert_eq!(r.get(5 + RING as u32), Some(0x66));
        r.put(u32::MAX, 0x77);
        assert_eq!(r.get(u32::MAX), None);
        r.clear();
        assert_eq!(r.get(5 + RING as u32), None);
    }

    #[test]
    fn window_edges() {
        assert!(in_window(100 - 64, 100));
        assert!(in_window(100 + 64, 100));
        assert!(!in_window(100 - 65, 100));
        assert!(!in_window(100 + 65, 100));
        assert!(in_window(0, 10));
    }
}
