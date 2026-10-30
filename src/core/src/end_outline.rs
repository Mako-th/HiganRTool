//! マッチ終了時のメニューの項目に付ける1P・2Pの縁取りの描き方（pure）。
//! ゲームが項目の絵を描く前に同じ絵を色だけにして描く。
//! 少しずつずらして何回か描くと文字の外側に縁取りが残る。
//! ここでは、ずらす量と色（選択中はゆっくり明滅・確定したら速く点滅してから止まる）を決める。描くのはdllの側。
#![forbid(unsafe_code)]

/// 項目の数（再戦・キャラを変更・リプレイを保存して終了）。
pub const ITEMS: usize = 3;

/// 1P・2Pの色（ARGBのRGB。抑えめの赤と青）。
pub const RGB_1P: u32 = 0x00D6_6060;
pub const RGB_2P: u32 = 0x0060_8CDE;
/// 確定してから点滅する間のフレーム数（0.5秒）。
/// 点滅の明るい側・薄い側の1区間のフレーム数（確定の瞬間は明るい側から）。
pub const FLASH_FRAMES: u32 = 30;
pub const FLASH_BLINK_FRAMES: u32 = 5;
/// 明滅の1往復のフレーム数（1.2秒）と明滅の α の下限・上限。
pub const PULSE_FRAMES: u32 = 72;
pub const PULSE_ALPHA_MIN: u32 = 64;
pub const PULSE_ALPHA_MAX: u32 = 230;
// 確定の点滅（1往復 `FLASH_BLINK_FRAMES` × 2）は選択中の明滅より速い。
const _: () = assert!(FLASH_BLINK_FRAMES * 2 < PULSE_FRAMES / 2);
/// 縁取りの太さ（画素）の上限。2人が同じ項目にいると外側の人は太さの2倍で描く。
pub const MAX_WIDTH: u8 = 2;
/// 1項目で描く回数の上限（太さ2の内側 ＋ 太さ4の外側）。
pub const MAX_PASSES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pick {
    #[default]
    None,
    /// この項目が選択中。
    Picking,
    /// この項目で確定済み。`at`は確定したフレーム（`frame`と同じ数え方）。
    Confirmed { at: u32 },
}

/// 1項目の2人の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ItemMarks {
    pub p1: Pick,
    pub p2: Pick,
}

impl ItemMarks {
    pub fn is_empty(&self) -> bool {
        self.p1 == Pick::None && self.p2 == Pick::None
    }
}

/// 1回ぶんの描き方: 絵を (`dx`, `dy`)画素ずらして色`argb`で塗る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pass {
    pub dx: i8,
    pub dy: i8,
    pub argb: u32,
}

/// 半径`r`の円に入るずらし（中心は除く）。太さ1は周りの8画素。太さ2以上は角を落とした円。
fn offsets(r: i32, mut f: impl FnMut(i8, i8)) {
    let limit = if r <= 1 { 2 } else { r * r + r };
    for dy in -r..=r {
        for dx in -r..=r {
            if (dx, dy) != (0, 0) && dx * dx + dy * dy <= limit {
                f(dx as i8, dy as i8);
            }
        }
    }
}

/// 状態`pick`のフレーム`frame`での色（ARGB）。印が無ければ`None`。
pub fn color(pick: Pick, rgb: u32, frame: u32) -> Option<u32> {
    match pick {
        Pick::None => None,
        Pick::Picking => {
            // 三角波で下限と上限の間を往復する。
            let half = PULSE_FRAMES / 2;
            let t = frame % PULSE_FRAMES;
            let up = if t < half { t } else { PULSE_FRAMES - t };
            let a = PULSE_ALPHA_MIN + (PULSE_ALPHA_MAX - PULSE_ALPHA_MIN) * up / half;
            Some(a << 24 | rgb)
        }
        Pick::Confirmed { at } => {
            let t = frame.wrapping_sub(at);
            let dim = t < FLASH_FRAMES && !(t / FLASH_BLINK_FRAMES).is_multiple_of(2);
            let a = if dim { PULSE_ALPHA_MIN } else { 0xFF };
            Some(a << 24 | rgb)
        }
    }
}

/// 1項目ぶんの描き方を`out`に書く。書いた数を返す。後に書いたものほど上に描かれる。
/// 2人が同じ項目にいるときは2Pを太さの2倍で先に、1Pを太さどおりに後で描く（1Pが内側・2Pが外側）。
/// 確保もパニックもしない（ゲームのスレッドで毎フレーム呼ぶ）。
pub fn passes(m: ItemMarks, frame: u32, width: u8, out: &mut [Pass]) -> usize {
    let w = width.clamp(1, MAX_WIDTH) as i32;
    let both = m.p1 != Pick::None && m.p2 != Pick::None;
    let mut n = 0;
    let mut push = |pick: Pick, rgb: u32, r: i32| {
        let Some(argb) = color(pick, rgb, frame) else {
            return;
        };
        offsets(r, |dx, dy| {
            if let Some(slot) = out.get_mut(n) {
                *slot = Pass { dx, dy, argb };
                n += 1;
            }
        });
    };
    push(m.p2, RGB_2P, if both { 2 * w } else { w });
    push(m.p1, RGB_1P, w);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(m: ItemMarks, frame: u32, width: u8) -> Vec<Pass> {
        let mut out = [Pass::default(); MAX_PASSES];
        let n = passes(m, frame, width, &mut out);
        out[..n].to_vec()
    }

    #[test]
    fn no_marks_draw_nothing() {
        assert!(run(ItemMarks::default(), 0, 1).is_empty());
        assert!(ItemMarks::default().is_empty());
    }

    #[test]
    fn width_one_is_the_eight_neighbours_and_width_two_is_a_round_ring() {
        let m = ItemMarks {
            p1: Pick::Confirmed { at: 0 },
            p2: Pick::None,
        };
        let one = run(m, 100, 1);
        assert_eq!(one.len(), 8);
        assert!(one.iter().all(|p| p.dx.abs() <= 1 && p.dy.abs() <= 1));
        let two = run(m, 100, 2);
        assert_eq!(two.len(), 20, "5×5 から中心と角 4 つを落とした数");
        assert!(!two.iter().any(|p| p.dx.abs() == 2 && p.dy.abs() == 2));
        // 太さの上限を越える値は上限で描く。0は1で描く。
        assert_eq!(run(m, 100, 9).len(), 20);
        assert_eq!(run(m, 100, 0).len(), 8);
    }

    #[test]
    fn picking_pulses_slowly_and_confirmed_blinks_fast_then_stays() {
        let a = |argb: u32| argb >> 24;
        let lo = color(Pick::Picking, RGB_1P, 0).unwrap();
        let hi = color(Pick::Picking, RGB_1P, PULSE_FRAMES / 2).unwrap();
        assert_eq!((a(lo), a(hi)), (PULSE_ALPHA_MIN, PULSE_ALPHA_MAX));
        assert_eq!(lo & 0xFF_FFFF, RGB_1P);
        assert_eq!(color(Pick::Picking, RGB_1P, PULSE_FRAMES), Some(lo));
        let c = Pick::Confirmed { at: 50 };
        let full = 0xFF00_0000 | RGB_2P;
        let dim = PULSE_ALPHA_MIN << 24 | RGB_2P;
        assert_eq!(color(c, RGB_2P, 50), Some(full), "確定の瞬間は明るい側");
        assert_eq!(color(c, RGB_2P, 50 + FLASH_BLINK_FRAMES - 1), Some(full));
        assert_eq!(color(c, RGB_2P, 50 + FLASH_BLINK_FRAMES), Some(dim));
        let toggles = (51..50 + FLASH_FRAMES)
            .filter(|&f| color(c, RGB_2P, f) != color(c, RGB_2P, f - 1))
            .count();
        assert!(toggles >= 4, "点滅の切り替えが少ない: {toggles}");
        // 点滅の後は明るい側のまま止まる。
        for f in 50 + FLASH_FRAMES..50 + FLASH_FRAMES + 3 * PULSE_FRAMES {
            assert_eq!(color(c, RGB_2P, f), Some(full), "{f}");
        }
        // 数えが一周しても確定の直後として扱う（点滅の最初の区間）。
        assert_eq!(
            color(Pick::Confirmed { at: u32::MAX }, RGB_2P, 1),
            Some(full)
        );
        assert_eq!(color(Pick::None, RGB_2P, 0), None);
    }

    #[test]
    fn two_on_one_item_draw_2p_outside_first_and_1p_inside_last() {
        let m = ItemMarks {
            p1: Pick::Confirmed { at: 0 },
            p2: Pick::Confirmed { at: 0 },
        };
        let v = run(m, 100, 1);
        let (p2, p1): (Vec<&Pass>, Vec<&Pass>) =
            v.iter().partition(|p| p.argb & 0xFF_FFFF == RGB_2P);
        assert_eq!(p1.len(), 8);
        assert!(p2.iter().any(|p| p.dx.abs() == 2), "2P は外側（太さ 2）");
        let last_2p = v
            .iter()
            .rposition(|p| p.argb & 0xFF_FFFF == RGB_2P)
            .unwrap();
        let first_1p = v.iter().position(|p| p.argb & 0xFF_FFFF == RGB_1P).unwrap();
        assert!(last_2p < first_1p, "1P を後に描く（上に重なる）");
        // 太さ2で2人なら外側は太さ4で上限の数に収まる。
        assert!(run(m, 100, 2).len() <= MAX_PASSES);
    }

    #[test]
    fn a_short_buffer_is_filled_without_overrun() {
        let m = ItemMarks {
            p1: Pick::Picking,
            p2: Pick::Picking,
        };
        let mut out = [Pass::default(); 5];
        assert_eq!(passes(m, 0, 2, &mut out), 5);
    }
}
