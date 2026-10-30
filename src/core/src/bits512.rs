//! 512ビットの固定長ビット集合（観戦者へ送るホストの送信ウィンドウ`spectate::SEND_WINDOW`とACKの受け取れた範囲`sack`の表）。
//! `Copy`で確保せず、添字は`get`で範囲を見る（ゲームスレッドから呼ぶ部品がパニックしない）。
//! ビットiはワード`i / 64`の下位から`i % 64`番目。

/// ワードの数。
pub const WORDS: usize = 8;
/// ビットの数。
pub const BITS: u32 = 512;
/// ワイヤに載せるバイト列の最大の長さ（`BITS / 8`）。
pub const BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bits512(pub [u64; WORDS]);

impl Bits512 {
    pub const EMPTY: Bits512 = Bits512([0; WORDS]);

    /// 下位`n`ビットが立った集合（`n`が512以上なら全部）。
    pub const fn low(n: u32) -> Bits512 {
        let mut w = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            let base = (i as u32) * 64;
            w[i] = if n >= base + 64 {
                u64::MAX
            } else if n > base {
                (1u64 << (n - base)) - 1
            } else {
                0
            };
            i += 1;
        }
        Bits512(w)
    }

    pub fn get(&self, i: u32) -> bool {
        self.0
            .get((i / 64) as usize)
            .is_some_and(|w| (w >> (i % 64)) & 1 == 1)
    }

    /// 範囲の外は無視する。
    pub fn set(&mut self, i: u32) {
        if let Some(w) = self.0.get_mut((i / 64) as usize) {
            *w |= 1 << (i % 64);
        }
    }

    /// `first`から`n`ビットを立てる。
    pub fn set_range(&mut self, first: u32, n: u32) {
        for i in first..first.saturating_add(n).min(BITS) {
            self.set(i);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    pub fn count_ones(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }

    /// 立っている最も上のビットの番号。
    pub fn highest(&self) -> Option<u32> {
        self.0
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &w)| w != 0)
            .map(|(i, w)| i as u32 * 64 + 63 - w.leading_zeros())
    }

    /// `from`以降で最初に立っているビットの番号。
    pub fn next_set(&self, from: u32) -> Option<u32> {
        let start = (from / 64) as usize;
        for (i, &w) in self.0.iter().enumerate().skip(start) {
            let w = if i == start {
                w & (u64::MAX << (from % 64))
            } else {
                w
            };
            if w != 0 {
                return Some(i as u32 * 64 + w.trailing_zeros());
            }
        }
        None
    }

    /// `from`から続けて立っているビットの数（`max`まで）。
    pub fn run_len(&self, from: u32, max: u32) -> u32 {
        let mut n = 0;
        while n < max && self.get(from.saturating_add(n)) {
            n += 1;
        }
        n
    }

    /// 右へ`n`ビット詰める（番号が`n`小さくなる。512以上なら空）。
    pub fn shr(&mut self, n: u32) {
        let (ws, bs) = ((n / 64) as usize, n % 64);
        let src = self.0;
        for (i, out) in self.0.iter_mut().enumerate() {
            let lo = src.get(i + ws).copied().unwrap_or(0);
            let hi = src.get(i + ws + 1).copied().unwrap_or(0);
            *out = if bs == 0 {
                lo
            } else {
                (lo >> bs) | (hi << (64 - bs))
            };
        }
    }

    /// 左へ`n`ビット送る（番号が`n`大きくなる。512を越えた分は捨てる）。
    pub fn shl(&mut self, n: u32) {
        let (ws, bs) = ((n / 64) as usize, n % 64);
        let src = self.0;
        for (i, out) in self.0.iter_mut().enumerate() {
            let cur = i
                .checked_sub(ws)
                .and_then(|j| src.get(j))
                .copied()
                .unwrap_or(0);
            let below = i
                .checked_sub(ws + 1)
                .and_then(|j| src.get(j))
                .copied()
                .unwrap_or(0);
            *out = if bs == 0 {
                cur
            } else {
                (cur << bs) | (below >> (64 - bs))
            };
        }
    }

    pub fn and(&self, o: &Bits512) -> Bits512 {
        self.zip(o, |a, b| a & b)
    }

    pub fn or(&self, o: &Bits512) -> Bits512 {
        self.zip(o, |a, b| a | b)
    }

    /// `self`のうち`o`に無いもの。
    pub fn andn(&self, o: &Bits512) -> Bits512 {
        self.zip(o, |a, b| a & !b)
    }

    fn zip(&self, o: &Bits512, f: impl Fn(u64, u64) -> u64) -> Bits512 {
        let mut r = *self;
        for (x, &b) in r.0.iter_mut().zip(o.0.iter()) {
            *x = f(*x, b);
        }
        r
    }

    /// ワイヤの形: バイトjがビット8j〜8j+7（下位から）。後ろの0のバイトは載せない。`out`に書いて載せた長さを返す。
    pub fn to_wire(&self, out: &mut [u8; BYTES]) -> usize {
        for (w, dst) in self.0.iter().zip(out.as_chunks_mut::<8>().0.iter_mut()) {
            *dst = w.to_le_bytes();
        }
        out.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1)
    }

    /// `to_wire`の逆。`BYTES`を越える長さはNone。後ろの0のバイトを許すかどうかは呼び出し側が決める。
    pub fn from_wire(bytes: &[u8]) -> Option<Bits512> {
        if bytes.len() > BYTES {
            return None;
        }
        let mut raw = [0u8; BYTES];
        raw.get_mut(..bytes.len())?.copy_from_slice(bytes);
        let mut w = [0u64; WORDS];
        for (x, chunk) in w.iter_mut().zip(raw.as_chunks::<8>().0.iter()) {
            *x = u64::from_le_bytes(*chunk);
        }
        Some(Bits512(w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(b: &Bits512) -> Vec<bool> {
        (0..BITS).map(|i| b.get(i)).collect()
    }

    fn sample(seed: u64) -> Bits512 {
        let mut x = seed;
        let mut w = [0u64; WORDS];
        for v in &mut w {
            x = x
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0xBF58_476D_1CE4_E5B9);
            *v = x ^ (x >> 29);
        }
        Bits512(w)
    }

    #[test]
    fn low_sets_exactly_the_lowest_n_bits_including_word_edges() {
        for n in [0, 1, 63, 64, 65, 127, 128, 255, 256, 448, 511, 512, 600] {
            let b = Bits512::low(n);
            let want = n.min(BITS);
            assert_eq!(b.count_ones(), want, "n={n}");
            assert!((0..want).all(|i| b.get(i)), "n={n}");
            assert!((want..BITS).all(|i| !b.get(i)), "n={n}");
        }
    }

    #[test]
    fn shifts_match_a_bool_model_across_word_boundaries() {
        for seed in 0..8 {
            let b = sample(seed);
            let m = model(&b);
            for n in [
                0u32, 1, 7, 63, 64, 65, 100, 128, 200, 447, 448, 500, 511, 512, 700,
            ] {
                let mut r = b;
                r.shr(n);
                for i in 0..BITS {
                    let want = m.get((i + n) as usize).copied().unwrap_or(false);
                    assert_eq!(r.get(i), want, "shr seed={seed} n={n} i={i}");
                }
                let mut l = b;
                l.shl(n);
                for i in 0..BITS {
                    let want = i >= n && m[(i - n) as usize];
                    assert_eq!(l.get(i), want, "shl seed={seed} n={n} i={i}");
                }
            }
        }
    }

    #[test]
    fn next_set_highest_and_run_len_find_bits_in_every_word() {
        for i in [0u32, 1, 63, 64, 130, 255, 256, 400, 511] {
            let mut b = Bits512::EMPTY;
            b.set(i);
            assert_eq!(b.highest(), Some(i));
            assert_eq!(b.next_set(0), Some(i));
            assert_eq!(b.next_set(i), Some(i));
            assert_eq!(b.next_set(i + 1), None, "i={i}");
            assert_eq!(b.run_len(i, 16), 1);
        }
        assert_eq!(Bits512::EMPTY.highest(), None);
        assert_eq!(Bits512::EMPTY.next_set(0), None);
        let mut b = Bits512::EMPTY;
        b.set_range(60, 8);
        assert_eq!(b.run_len(60, 16), 8, "語をまたぐ連なり");
        assert_eq!(b.run_len(60, 5), 5, "上限で切る");
        assert_eq!(b.next_set(61), Some(61));
        assert_eq!(b.next_set(68), None);
    }

    #[test]
    fn set_ops_and_out_of_range_are_safe() {
        let (a, b) = (sample(1), sample(2));
        for i in 0..BITS {
            assert_eq!(a.and(&b).get(i), a.get(i) && b.get(i));
            assert_eq!(a.or(&b).get(i), a.get(i) || b.get(i));
            assert_eq!(a.andn(&b).get(i), a.get(i) && !b.get(i));
        }
        let mut c = Bits512::EMPTY;
        c.set(512);
        c.set(u32::MAX);
        c.set_range(500, 1_000);
        assert!(!c.get(512) && !c.get(u32::MAX));
        assert_eq!(c.count_ones(), 12, "500..512 だけ");
    }

    #[test]
    fn the_wire_form_drops_trailing_zero_bytes_and_round_trips() {
        let mut out = [0u8; BYTES];
        assert_eq!(Bits512::EMPTY.to_wire(&mut out), 0);
        for (bit, len) in [
            (0u32, 1usize),
            (7, 1),
            (8, 2),
            (63, 8),
            (64, 9),
            (510, 64),
            (511, 64),
        ] {
            let mut b = Bits512::EMPTY;
            b.set(bit);
            assert_eq!(b.to_wire(&mut out), len, "bit {bit}");
            assert_eq!(out[len - 1], 1 << (bit % 8), "最後のバイトが最上位のビット");
            assert_eq!(Bits512::from_wire(&out[..len]), Some(b));
        }
        assert_eq!(Bits512::from_wire(&[0u8; BYTES + 1]), None);
    }
}
