//! フレーム番号の式。
//!
//! - `capture_frame`: 自分の入力を取ったフレーム
//! - `delay`: いま有効な latency（0〜9）
//! - `apply_frame = capture_frame + delay`: その入力を両者が使うフレーム
//! - 開始直後の `apply_frame` 0 〜 `delay - 1` は、取った入力が無いので両者とも入力 0

/// latency の上限。
pub const MAX_DELAY: u8 = 9;

/// 取ったフレームから、その入力を使うフレームを返す。u32 は 60 fps で 2 年余りなので折り返しは考えない。
pub fn apply_frame(capture_frame: u32, delay: u8) -> u32 {
    capture_frame + u32::from(delay)
}

/// 開始直後の穴埋めのフレームか（取った入力が無く、両者とも入力 0 を使う）。
pub fn is_prefill(apply_frame: u32, delay: u8) -> bool {
    apply_frame < u32::from(delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_frame_adds_delay() {
        assert_eq!(apply_frame(0, 0), 0);
        assert_eq!(apply_frame(57, 0), 57);
        assert_eq!(apply_frame(0, 3), 3);
        assert_eq!(apply_frame(10, 3), 13);
        assert_eq!(apply_frame(10, MAX_DELAY), 19);
    }

    #[test]
    fn prefill_is_exactly_the_first_delay_frames() {
        // latency 3: apply_frame 0, 1, 2 は穴埋め、3 からは取った入力
        assert!(is_prefill(0, 3));
        assert!(is_prefill(2, 3));
        assert!(!is_prefill(3, 3));
        assert_eq!(apply_frame(0, 3), 3);
        assert!(!is_prefill(0, 0));
    }
}
