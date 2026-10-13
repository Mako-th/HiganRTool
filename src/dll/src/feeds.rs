//! feed の記録（リングバッファ）と、読み取りのスレッドでの確かめ（pure）。
//!
//! ゲームのスレッド（`script_win::script_on_tick`）は、台本で feed した tick ごとに、feed の前と後の InputManager 3 本の
//! `+0x00`〜`+0x29`（0x2A バイト）を、tick の番号・段階・p1・p2・ローカル入力と一緒にこのリングバッファへ 1 件書く（書き手はゲームのスレッド
//! だけ）。読み取りのスレッドは、「前」を模型（`higanr_core::input_model`）で feed した結果と「後」を 3 本とも欄ごとに比べ、
//! 続けて feed した 2 件では「前の件の後」と「次の件の前」も比べる（間にゲームが書いていないか）。
//!
//! リングバッファの書き方と読み方は tick のリングバッファ（`stub.rs`）と同じ seqlock: 札を 0 → 中身 → 札 = 番号 + 1 の順に書き、読み手は札・中身・札の
//! 順に読み、2 回の札がどちらも番号 + 1 のときだけ採る。書き手（`FeedRing::push`）は確保もパニックもしない。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering, fence};
use std::io::Write;

use higanr_core::input_model::InputState;

use crate::th09_addr::{
    IM_CUR, IM_DOWN, IM_FEED_LEN, IM_PREV, IM_REPEAT, IM_REPEAT_COUNTERS, IM_UP,
};
use crate::ticks::{SlotState, classify};

/// InputManager 1 本からコピーする長さ（feed が書く範囲 `+0x00`〜`+0x29`）。
pub const IM_BYTES: usize = IM_FEED_LEN as usize;

/// リングバッファの件数（2 の冪。60 fps で約 17 秒ぶん）。
pub const FEED_RING_LEN: u32 = 1024;
const FEED_RING_MASK: u32 = FEED_RING_LEN - 1;
const _: () = assert!(FEED_RING_LEN.is_power_of_two());

/// 1 件の中身の語の数: k / 段階とローカル入力 / p1 と p2 / 前 3 本 ＋ 後 3 本（252 バイト = 63 語）。
const IM_ALL_BYTES: usize = IM_BYTES * 6;
const _: () = assert!(IM_ALL_BYTES.is_multiple_of(4));
pub const FEED_BODY_WORDS: usize = 3 + IM_ALL_BYTES / 4;
/// 1 件の語の数（中身 ＋ 予備 1 ＋ 札 1。272 バイト）。
pub const FEED_SLOT_WORDS: usize = FEED_BODY_WORDS + 2;
const W_TICKET: usize = FEED_SLOT_WORDS - 1;
const W_K: usize = 0;
/// 下位 16 ビットが段階、上位 16 ビットがローカル入力。
const W_STEP_LOCAL: usize = 1;
/// 下位 16 ビットが p1、上位 16 ビットが p2。
const W_KEYS: usize = 2;
const W_BYTES: usize = 3;

/// 台本で feed した 1 tick の記録。`before[i]` / `after[i]` は InputManager `[i]`（0 = 1P、1 = 2P、2 = 合成）の
/// feed の前と後の `+0x00`〜`+0x29`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedRecord {
    pub k: u32,
    pub step: u16,
    pub p1: u16,
    pub p2: u16,
    /// ローカル入力の下位 16 ビット（記録だけ。feed しない）。台本だけの実行では `0x0042BE40` の読み。
    pub local: u16,
    pub before: [[u8; IM_BYTES]; 3],
    pub after: [[u8; IM_BYTES]; 3],
}

impl FeedRecord {
    pub fn ring_words(&self) -> [u32; FEED_BODY_WORDS] {
        let mut w = [0u32; FEED_BODY_WORDS];
        w[W_K] = self.k;
        w[W_STEP_LOCAL] = self.step as u32 | (self.local as u32) << 16;
        w[W_KEYS] = self.p1 as u32 | (self.p2 as u32) << 16;
        let mut bytes = [0u8; IM_ALL_BYTES];
        for (i, im) in self.before.iter().chain(self.after.iter()).enumerate() {
            bytes[i * IM_BYTES..(i + 1) * IM_BYTES].copy_from_slice(im);
        }
        for (d, c) in w[W_BYTES..].iter_mut().zip(bytes.as_chunks::<4>().0) {
            *d = u32::from_le_bytes(*c);
        }
        w
    }

    pub fn from_words(w: &[u32; FEED_BODY_WORDS]) -> Self {
        let mut bytes = [0u8; IM_ALL_BYTES];
        for (c, v) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&w[W_BYTES..]) {
            *c = v.to_le_bytes();
        }
        let im = |i: usize| -> [u8; IM_BYTES] {
            let mut a = [0u8; IM_BYTES];
            a.copy_from_slice(&bytes[i * IM_BYTES..(i + 1) * IM_BYTES]);
            a
        };
        FeedRecord {
            k: w[W_K],
            step: w[W_STEP_LOCAL] as u16,
            local: (w[W_STEP_LOCAL] >> 16) as u16,
            p1: w[W_KEYS] as u16,
            p2: (w[W_KEYS] >> 16) as u16,
            before: [im(0), im(1), im(2)],
            after: [im(3), im(4), im(5)],
        }
    }

    /// InputManager `[i]` へ feed した語（`0` に p1・`1` に p2・`2` に p1|p2）。
    pub fn words(&self) -> [u16; 3] {
        [self.p1, self.p2, self.p1 | self.p2]
    }
}

#[repr(C)]
pub struct FeedSlot {
    words: [AtomicU32; FEED_SLOT_WORDS],
}

/// feed のリングバッファ。書き手はゲームのスレッドだけ（`push`）、読み手は読み取りのスレッドだけ（`read`）。
pub struct FeedRing {
    slots: [FeedSlot; FEED_RING_LEN as usize],
    next: AtomicU32,
}

impl FeedRing {
    pub const fn new() -> Self {
        FeedRing {
            slots: [const {
                FeedSlot {
                    words: [const { AtomicU32::new(0) }; FEED_SLOT_WORDS],
                }
            }; FEED_RING_LEN as usize],
            next: AtomicU32::new(0),
        }
    }

    /// 1 件書く（ゲームのスレッドだけが呼ぶ）。seqlock の書き手: 札 0 の後の Release の fence で、中身の書きが札 0 より前に見えない。
    pub fn push(&self, r: &FeedRecord) {
        let seq = self.next.load(Ordering::Relaxed);
        let slot = &self.slots[(seq & FEED_RING_MASK) as usize];
        let ticket = &slot.words[W_TICKET];
        ticket.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        for (a, v) in slot.words.iter().zip(r.ring_words()) {
            a.store(v, Ordering::Relaxed);
        }
        ticket.store(seq.wrapping_add(1), Ordering::Release);
        self.next.store(seq.wrapping_add(1), Ordering::Release);
    }

    pub fn next_seq(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }

    pub fn read(&self, seq: u32) -> FeedRead {
        let slot = &self.slots[(seq & FEED_RING_MASK) as usize];
        let ticket = &slot.words[W_TICKET];
        let t1 = ticket.load(Ordering::Acquire);
        let w: [u32; FEED_BODY_WORDS] =
            core::array::from_fn(|i| slot.words[i].load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        let t2 = ticket.load(Ordering::Relaxed);
        match classify(seq, t1, t2) {
            SlotState::Complete => FeedRead::Record(Box::new(FeedRecord::from_words(&w))),
            SlotState::NotYet => FeedRead::NotYet,
            SlotState::Overwritten => FeedRead::Overwritten,
        }
    }
}

impl Default for FeedRing {
    fn default() -> Self {
        Self::new()
    }
}

pub static FEED_RING: FeedRing = FeedRing::new();

/// 1 件を読んだ結果（`FeedRecord` は 270 バイトほどあるので`Box` に入れる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedRead {
    Record(Box<FeedRecord>),
    NotYet,
    Overwritten,
}

/// リングバッファから新しい件を読む（`observe::drain` の feed バージョン）。`end` は書き手の次の番号。
pub fn drain_feeds(
    end: u32,
    read: &dyn Fn(u32) -> FeedRead,
    next: &mut u32,
    lost: &mut u64,
    out: &mut Vec<FeedRecord>,
) {
    let behind = end.wrapping_sub(*next);
    if behind > FEED_RING_LEN {
        let skip = behind - FEED_RING_LEN;
        *lost += skip as u64;
        *next = next.wrapping_add(skip);
    }
    while *next != end {
        match read(*next) {
            FeedRead::Record(r) => out.push(*r),
            FeedRead::Overwritten => *lost += 1,
            FeedRead::NotYet => break,
        }
        *next = next.wrapping_add(1);
    }
}

// ---- 確かめ ----

/// 比べる欄の数（cur・prev・リピート・押した瞬間・離した瞬間・カウンタ 16 個。0x2A バイトを全部覆う）。
pub const FIELD_COUNT: usize = 5 + 16;
const _: () = assert!(FIELD_COUNT * 2 == IM_BYTES);

pub fn field_name(i: usize) -> String {
    match i {
        0 => "cur".to_string(),
        1 => "prev".to_string(),
        2 => "repeat".to_string(),
        3 => "pressed".to_string(),
        4 => "released".to_string(),
        b => format!("cnt{}", b - 5),
    }
}

fn u16_at(b: &[u8; IM_BYTES], off: u32) -> u16 {
    let o = off as usize;
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub fn im_from_bytes(b: &[u8; IM_BYTES]) -> InputState {
    InputState {
        cur: u16_at(b, IM_CUR),
        prev: u16_at(b, IM_PREV),
        repeat: u16_at(b, IM_REPEAT),
        pressed: u16_at(b, IM_DOWN),
        released: u16_at(b, IM_UP),
        cnt: core::array::from_fn(|i| u16_at(b, IM_REPEAT_COUNTERS + 2 * i as u32)),
    }
}

/// 模型の形を 0x2A バイトへ（`im_from_bytes` の逆。検査の偽の feed と、合成の記録に使う）。
#[cfg(test)]
pub fn im_to_bytes(s: &InputState) -> [u8; IM_BYTES] {
    let mut b = [0u8; IM_BYTES];
    let mut put = |off: u32, v: u16| {
        let o = off as usize;
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    };
    put(IM_CUR, s.cur);
    put(IM_PREV, s.prev);
    put(IM_REPEAT, s.repeat);
    put(IM_DOWN, s.pressed);
    put(IM_UP, s.released);
    for (i, c) in s.cnt.iter().enumerate() {
        put(IM_REPEAT_COUNTERS + 2 * i as u32, *c);
    }
    b
}

fn fields(s: &InputState) -> [u16; FIELD_COUNT] {
    let mut f = [0u16; FIELD_COUNT];
    f[0] = s.cur;
    f[1] = s.prev;
    f[2] = s.repeat;
    f[3] = s.pressed;
    f[4] = s.released;
    f[5..].copy_from_slice(&s.cnt);
    f
}

pub fn diff_fields(want: &InputState, got: &InputState) -> u32 {
    let (a, b) = (fields(want), fields(got));
    let mut m = 0u32;
    for i in 0..FIELD_COUNT {
        if a[i] != b[i] {
            m |= 1 << i;
        }
    }
    m
}

fn mask_text(m: u32) -> String {
    if m == 0 {
        return "ok".to_string();
    }
    (0..FIELD_COUNT)
        .filter(|i| m & (1 << i) != 0)
        .map(field_name)
        .collect::<Vec<_>>()
        .join("+")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cont {
    First,
    /// 前の件の k + 1 でない（間に台本で feed しなかった tick がある・読み落とし）。
    Gap,
    /// 前の件の「後」と、この件の「前」の、3 本ごとの違う欄。全部 0 なら続いている。
    Pair([u32; 3]),
}

/// 1 件の確かめの結果。`im[i]` は、InputManager `[i]` の「前を模型で feed した結果」と「後」で違う欄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckRow {
    pub im: [u32; 3],
    pub cont: Cont,
}

/// `_feed.csv` の見出し。im0〜im2 は 3 本の一致（`ok` か違う欄の名前）、cont は続きの一致
/// （`-` 最初の件 / `gap` k が飛んだ / `ok` / `im0:cur+prev` の形）。
pub const FEED_CSV_HEADER: &str = "k,step,p1,p2,local,im0,im1,im2,cont";

#[derive(Debug, Clone, Default)]
pub struct FeedChecker {
    prev: Option<FeedRecord>,
    pub records: u64,
    pub mismatched_records: u64,
    pub mismatch: [[u64; FIELD_COUNT]; 3],
    /// 続けて feed した組（k と k+1）の数と、そのうち前の後と次の前が違った組の数。
    pub cont_pairs: u64,
    pub cont_breaks: u64,
    pub cont_fields: [[u64; FIELD_COUNT]; 3],
    pub gaps: u64,
}

impl FeedChecker {
    pub fn check(&mut self, r: &FeedRecord) -> CheckRow {
        let words = r.words();
        let mut im = [0u32; 3];
        for i in 0..3 {
            let mut want = im_from_bytes(&r.before[i]);
            want.feed(words[i]);
            let got = im_from_bytes(&r.after[i]);
            im[i] = diff_fields(&want, &got);
            for (f, n) in self.mismatch[i].iter_mut().enumerate() {
                if im[i] & (1 << f) != 0 {
                    *n += 1;
                }
            }
        }
        self.records += 1;
        if im.iter().any(|&m| m != 0) {
            self.mismatched_records += 1;
        }
        let cont = match &self.prev {
            None => Cont::First,
            Some(p) if p.k.wrapping_add(1) != r.k => {
                self.gaps += 1;
                Cont::Gap
            }
            Some(p) => {
                let mut d = [0u32; 3];
                for (i, di) in d.iter_mut().enumerate() {
                    *di = diff_fields(&im_from_bytes(&p.after[i]), &im_from_bytes(&r.before[i]));
                    for (f, n) in self.cont_fields[i].iter_mut().enumerate() {
                        if *di & (1 << f) != 0 {
                            *n += 1;
                        }
                    }
                }
                self.cont_pairs += 1;
                if d.iter().any(|&m| m != 0) {
                    self.cont_breaks += 1;
                }
                Cont::Pair(d)
            }
        };
        self.prev = Some(*r);
        CheckRow { im, cont }
    }

    pub fn row(r: &FeedRecord, c: &CheckRow) -> String {
        let cont = match c.cont {
            Cont::First => "-".to_string(),
            Cont::Gap => "gap".to_string(),
            Cont::Pair(d) if d.iter().all(|&m| m == 0) => "ok".to_string(),
            Cont::Pair(d) => d
                .iter()
                .enumerate()
                .filter(|(_, m)| **m != 0)
                .map(|(i, m)| format!("im{i}:{}", mask_text(*m)))
                .collect::<Vec<_>>()
                .join("/"),
        };
        format!(
            "{},{},0x{:04X},0x{:04X},0x{:04X},{},{},{},{}",
            r.k,
            r.step,
            r.p1,
            r.p2,
            r.local,
            mask_text(c.im[0]),
            mask_text(c.im[1]),
            mask_text(c.im[2]),
            cont
        )
    }

    pub fn write_summary(&self, out: &mut dyn Write, lost: u64) {
        let _ = writeln!(
            out,
            "feed の確かめ: リングバッファから読んだ件 {}（台本で feed した tick）、3 本とも一致 {} 件 / 食い違いのあった件 {} 件、リングバッファの読み落とし {} 件",
            self.records,
            self.records - self.mismatched_records,
            self.mismatched_records,
            lost
        );
        for (i, name) in ["im0（1P）", "im1（2P）", "im2（合成）"].iter().enumerate() {
            let _ = writeln!(
                out,
                "feed の確かめ: {name} の食い違い 合計 {}{}",
                self.mismatch[i].iter().sum::<u64>(),
                counts_text(&self.mismatch[i])
            );
        }
        let _ = writeln!(
            out,
            "feed の続き: 続けて feed した組 {}、前の後と次の前が違った組（間にゲームが書いた）{}、k が飛んだ所 {}",
            self.cont_pairs, self.cont_breaks, self.gaps
        );
        if self.cont_breaks > 0 {
            for (i, name) in ["im0", "im1", "im2"].iter().enumerate() {
                let _ = writeln!(
                    out,
                    "feed の続き: {name} の違った欄{}",
                    counts_text(&self.cont_fields[i])
                );
            }
        }
    }
}

fn counts_text(c: &[u64; FIELD_COUNT]) -> String {
    let parts: Vec<String> = c
        .iter()
        .enumerate()
        .filter(|(_, n)| **n != 0)
        .map(|(f, n)| format!("{} {n}", field_name(f)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("（{}）", parts.join(" / "))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use higanr_core::input_model::keys;

    pub(crate) fn fed(s: &InputState, word: u16) -> InputState {
        let mut t = *s;
        t.feed(word);
        t
    }

    pub(crate) fn good_record(k: u32, p1: u16, p2: u16, before: [InputState; 3]) -> FeedRecord {
        let words = [p1, p2, p1 | p2];
        FeedRecord {
            k,
            step: 3,
            p1,
            p2,
            local: 0x0010,
            before: core::array::from_fn(|i| im_to_bytes(&before[i])),
            after: core::array::from_fn(|i| im_to_bytes(&fed(&before[i], words[i]))),
        }
    }

    #[test]
    fn im_bytes_layout_by_hand() {
        let mut b = [0u8; IM_BYTES];
        b[0x00] = 0x11;
        b[0x02] = 0x22;
        b[0x04] = 0x33;
        b[0x05] = 0x01;
        b[0x06] = 0x44;
        b[0x08] = 0x55;
        b[0x0A] = 0x07; // cnt0
        b[0x28] = 0x09; // cnt15
        b[0x29] = 0x01;
        let s = im_from_bytes(&b);
        assert_eq!(
            (s.cur, s.prev, s.repeat, s.pressed, s.released),
            (0x11, 0x22, 0x0133, 0x44, 0x55)
        );
        assert_eq!(s.cnt[0], 7);
        assert_eq!(s.cnt[15], 0x0109);
        assert_eq!(s.cnt[1..15], [0u16; 14]);
        assert_eq!(im_to_bytes(&s), b);
    }

    #[test]
    fn record_roundtrips_through_words() {
        let r = FeedRecord {
            k: 0xDEAD_BEEF,
            step: 0x1234,
            p1: 0x0081,
            p2: 0x0042,
            local: 0xFFFE,
            before: core::array::from_fn(|i| core::array::from_fn(|j| (i * 50 + j) as u8)),
            after: core::array::from_fn(|i| core::array::from_fn(|j| (200 - i * 30 - j) as u8)),
        };
        assert_eq!(FeedRecord::from_words(&r.ring_words()), r);
        assert_eq!(r.words(), [0x0081, 0x0042, 0x00C3]);
    }

    #[test]
    fn all_matching_records_count_zero() {
        let mut c = FeedChecker::default();
        let mut st = [InputState::new(); 3];
        for _ in 0..25 {
            for (i, s) in st.iter_mut().enumerate() {
                s.feed([keys::RIGHT, 0, keys::RIGHT][i]);
            }
        }
        for (k, (p1, p2)) in (100u32..).zip([
            (keys::RIGHT, 0),
            (keys::RIGHT, keys::Z),
            (0, keys::Z),
            (0, 0),
        ]) {
            let r = good_record(k, p1, p2, st);
            let row = c.check(&r);
            assert_eq!(row.im, [0; 3], "k={k}");
            for (i, s) in st.iter_mut().enumerate() {
                *s = im_from_bytes(&r.after[i]);
            }
        }
        assert_eq!(
            good_record(1, keys::RIGHT, 0, {
                let mut s = [InputState::new(); 3];
                for _ in 0..25 {
                    s[0].feed(keys::RIGHT);
                }
                s
            })
            .after[0][4],
            keys::RIGHT as u8
        );
        assert_eq!(c.records, 4);
        assert_eq!(c.mismatched_records, 0);
        assert_eq!(c.mismatch, [[0; FIELD_COUNT]; 3]);
        assert_eq!((c.cont_pairs, c.cont_breaks, c.gaps), (3, 0, 0));
    }

    #[test]
    fn single_field_mismatches_land_in_the_right_column() {
        let base = [InputState::new(); 3];
        let mut c = FeedChecker::default();

        let mut r = good_record(10, keys::UP, 0, base);
        r.after[0][IM_REPEAT as usize] ^= 0x10;
        let row = c.check(&r);
        assert_eq!(row.im, [1 << 2, 0, 0]);
        assert_eq!(c.mismatch[0][2], 1);

        let mut r = good_record(20, 0, keys::DOWN, base);
        r.after[1][IM_PREV as usize] = 0x77;
        let row = c.check(&r);
        assert_eq!(row.im, [0, 1 << 1, 0]);
        assert_eq!(row.cont, Cont::Gap);
        assert_eq!(c.mismatch[1][1], 1);

        let mut r = good_record(30, keys::Z, keys::X, base);
        r.after[2][(IM_REPEAT_COUNTERS + 6) as usize] = 9;
        let row = c.check(&r);
        assert_eq!(row.im, [0, 0, 1 << (5 + 3)]);
        assert_eq!(c.mismatch[2][5 + 3], 1);

        assert_eq!(c.records, 3);
        assert_eq!(c.mismatched_records, 3);
        let total: u64 = c.mismatch.iter().flatten().sum();
        assert_eq!(total, 3, "ほかの欄に数えた");
        assert_eq!(
            FeedChecker::row(&r, &row),
            "30,3,0x0001,0x0002,0x0010,ok,ok,cnt3,gap"
        );
    }

    #[test]
    fn continuity_breaks_and_gaps_are_counted() {
        let mut c = FeedChecker::default();
        let a = good_record(5, keys::LEFT, 0, [InputState::new(); 3]);
        assert_eq!(c.check(&a).cont, Cont::First);
        let mut next_before: [InputState; 3] = core::array::from_fn(|i| im_from_bytes(&a.after[i]));
        next_before[1].cur = keys::SHIFT;
        let b = good_record(6, keys::LEFT, 0, next_before);
        let row = c.check(&b);
        assert_eq!(row.cont, Cont::Pair([0, 1, 0]));
        assert_eq!(row.im, [0; 3], "この件そのものは正しく feed されている");
        assert_eq!((c.cont_pairs, c.cont_breaks), (1, 1));
        assert_eq!(c.cont_fields[1][0], 1);
        assert_eq!(
            FeedChecker::row(&b, &row),
            "6,3,0x0040,0x0000,0x0010,ok,ok,ok,im1:cur"
        );
        let before3: [InputState; 3] = core::array::from_fn(|i| im_from_bytes(&b.after[i]));
        let d = good_record(7, 0, 0, before3);
        assert_eq!(c.check(&d).cont, Cont::Pair([0; 3]));
        let e = good_record(9, 0, 0, [InputState::new(); 3]);
        assert_eq!(c.check(&e).cont, Cont::Gap);
        assert_eq!((c.cont_pairs, c.cont_breaks, c.gaps), (2, 1, 1));
    }

    #[test]
    fn ring_push_read_and_drain() {
        let ring = Box::new(FeedRing::new());
        let r0 = good_record(40, keys::Z, 0, [InputState::new(); 3]);
        assert_eq!(ring.read(0), FeedRead::NotYet);
        ring.push(&r0);
        assert_eq!(ring.next_seq(), 1);
        assert_eq!(ring.read(0), FeedRead::Record(Box::new(r0)));
        assert_eq!(ring.read(1), FeedRead::NotYet);
        for i in 1..FEED_RING_LEN + 6 {
            let mut r = r0;
            r.k = 40 + i;
            ring.push(&r);
        }
        assert_eq!(ring.read(0), FeedRead::Overwritten);
        let mut next = 0;
        let mut lost = 0;
        let mut out = Vec::new();
        drain_feeds(
            ring.next_seq(),
            &|s| ring.read(s),
            &mut next,
            &mut lost,
            &mut out,
        );
        assert_eq!(lost, 6);
        assert_eq!(out.len(), FEED_RING_LEN as usize);
        assert_eq!(out[0].k, 46);
        assert_eq!(next, ring.next_seq());
    }
}
