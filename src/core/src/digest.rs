//! 状態のダイジェストと、2 つのコピーの違いの数え（pure）。巻き戻しの実験で、進めた後の状態と、
//! 書き戻して進め直した後の状態を範囲ごとに比べる。違った範囲だけバイトで比べる。
//!
//! FNV-1a（64 ビット。`check.rs` の入力のダイジェストと同じ定数）を 8 バイトの語ごとに回し、最後に長さを混ぜる。
//! 1 バイトずつより 8 倍速い（1 回の実験で約 12 MB を 2 回通し、ゲームのスレッドで走る）ので値は違う。
//! 確保しない・パニックしない。

/// 初期値（FNV-1a 64 ビットの offset basis。`check::INPUT_DIGEST_INIT` と同じ値）。
pub const DIGEST_INIT: u64 = 0xCBF2_9CE4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01B3;

/// 頁の大きさ（Windows と同じ 4 KiB）。
pub const PAGE: usize = 4096;

pub fn digest(bytes: &[u8]) -> u64 {
    digest_continue(DIGEST_INIT, bytes)
}

pub fn digest_continue(h: u64, bytes: &[u8]) -> u64 {
    let mut h = h;
    let (words, rest) = bytes.as_chunks::<8>();
    for w in words {
        h ^= u64::from_le_bytes(*w);
        h = h.wrapping_mul(PRIME);
    }
    for &b in rest {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    // 長さを混ぜる（0 を足しただけの範囲と、短い範囲を取り違えないため）。
    h ^= bytes.len() as u64;
    h.wrapping_mul(PRIME)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Diff {
    /// 違うバイトの数（長さが違えば、長い方のはみ出した分も違うと数える）。
    pub bytes: u32,
    /// 最初と最後の違うバイトの位置。
    pub first: Option<u32>,
    pub last: Option<u32>,
    pub pages: u32,
}

/// `a` と `b` をバイトで比べる。8 バイトずつ見て同じ語は飛ばす。違う位置を `offsets` の長さまで書き、書いた数を返す。
pub fn diff(a: &[u8], b: &[u8], offsets: &mut [u32]) -> (Diff, usize) {
    let n = a.len().min(b.len());
    let mut d = Diff::default();
    let mut written = 0usize;
    let mut last_page: Option<usize> = None;
    let mut note = |i: usize, d: &mut Diff| {
        d.bytes = d.bytes.saturating_add(1);
        if d.first.is_none() {
            d.first = Some(i as u32);
        }
        d.last = Some(i as u32);
        let p = i / PAGE;
        if last_page != Some(p) {
            d.pages = d.pages.saturating_add(1);
            last_page = Some(p);
        }
        if let Some(slot) = offsets.get_mut(written) {
            *slot = i as u32;
            written += 1;
        }
    };
    let mut i = 0usize;
    while i < n {
        if i + 8 <= n && a[i..i + 8] == b[i..i + 8] {
            i += 8;
            continue;
        }
        if a[i] != b[i] {
            note(i, &mut d);
        }
        i += 1;
    }
    for j in n..a.len().max(b.len()) {
        note(j, &mut d);
    }
    (d, written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_sees_every_byte_and_the_length() {
        let base: Vec<u8> = (0..37u32).map(|i| (i * 7) as u8).collect();
        let d0 = digest(&base);
        assert_eq!(digest(&base.clone()), d0);
        for i in 0..base.len() {
            for bit in [0x01u8, 0x80] {
                let mut x = base.clone();
                x[i] ^= bit;
                assert_ne!(digest(&x), d0, "位置 {i} のビット {bit:#x}");
            }
        }
        let mut longer = base.clone();
        longer.push(0);
        assert_ne!(digest(&longer), d0);
        assert_ne!(digest(&[]), digest(&[0]));
        assert_ne!(digest(&[0u8; 8]), digest(&[0u8; 16]));
    }

    #[test]
    fn continue_depends_on_order() {
        let a = [1u8, 2, 3];
        let b = [4u8, 5];
        let ab = digest_continue(digest(&a), &b);
        let ba = digest_continue(digest(&b), &a);
        assert_ne!(ab, ba);
        assert_ne!(ab, digest(&b));
    }

    #[test]
    fn diff_counts_bytes_pages_and_positions() {
        let a = vec![0u8; PAGE * 3 + 5];
        let mut b = a.clone();
        let hits = [3usize, 9, 10, PAGE * 2 + 1, PAGE * 3 + 4];
        for &h in &hits {
            b[h] = 1;
        }
        let mut offs = [0u32; 3];
        let (d, n) = diff(&a, &b, &mut offs);
        assert_eq!(d.bytes, 5);
        assert_eq!(d.first, Some(3));
        assert_eq!(d.last, Some((PAGE * 3 + 4) as u32));
        assert_eq!(d.pages, 3, "頁 0・2・3");
        assert_eq!(n, 3);
        assert_eq!(offs, [3, 9, 10]);
        let (same, n) = diff(&a, &a, &mut offs);
        assert_eq!((same, n), (Diff::default(), 0));
        let (d, _) = diff(&a[..10], &a[..12], &mut []);
        assert_eq!((d.bytes, d.first, d.last), (2, Some(10), Some(11)));
    }
}
