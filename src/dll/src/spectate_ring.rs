//! 観戦のリングバッファ: ゲームのスレッド（書き手 1 本）が、使うと決めた語・止める地点の SELECT の結果・選択の画面で入った押下
//! （`wire::Record`）を書き、受信スレッド（読み手 1 本。`spectate_host.rs`）が取り出して `SpectateHost::push` する。
//!
//! - 書き手は確保・ロック待ち・ブロッキング I/O・パニックしうる添字を持たない（枠は固定長の原子変数の配列）。枠の中身を
//!   `Relaxed` で書いてから書いた数を `Release` で進める。読み手は書いた数を `Acquire` で読んでから枠を読み、読み終えてから
//!   読んだ数を `Release` で進める。
//! - リングバッファが溢れても対戦は止めない。書けなかった記録は捨てて数え、次の `Seg` まで捨て続ける（途中が欠けた列を観戦者へ流さない）。
//!   次に書けた `Seg` には `after_gap` の印が付き、読み手は `Record::End(Cancelled)` を挟んで「ここで切れた」を観戦者に知らせる。
//! - 1 枠 9 語（36 B）。`Record::Name` が最長で、ほかの記録は先頭の 5 語（`BASE_WORDS`）だけを書く。語の割り付けはリングバッファの中だけの形で、
//!   ワイヤの形とは無関係。
//!
//! `RING_LEN` = 4096 記録は、受信スレッドが 1 秒止まっても溢れない見積もり: 書くのは 1 tick に 1〜2 記録（60 tick/秒で
//! 1〜2 秒ぶんが 120 記録ほど）。受信スレッドは PING・INPUT のたび（約 4 ms〜16 ms）と、観戦者がいる間は 1 ms の待ちの上限ごとに
//! リングバッファを空にする（見積もりで、測っていない）。
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use higanr_core::wire::{ByeReason, Record, Seat, Select, Step, StepKind};

pub const RING_LEN: usize = 4096;
/// 1 枠の語の数（名前の記録が使う 1 + 8 語）。
const WORDS: usize = 9;
/// 名前の記録以外が使う語の数（`Record::Select` の 5 語）。書き手はこの数だけ書く。
const BASE_WORDS: usize = 5;
/// 語 0 の bit 8: 溢れの後の最初の記録（`Seg`）。
const FLAG_AFTER_GAP: u32 = 1 << 8;

const K_SEG: u32 = 1;
const K_SELECT: u32 = 2;
const K_STEP: u32 = 3;
const K_MENU: u32 = 4;
const K_FRAME: u32 = 5;
const K_END: u32 = 6;
const K_NAME: u32 = 7;
const K_STATS: u32 = 8;

/// 記録を語に詰める（`unpack` の逆）。戻りの 2 つ目は書く語の数。
fn pack(rec: &Record) -> ([u32; WORDS], usize) {
    let mut w = [0u32; WORDS];
    match rec {
        Record::Seg { seg } => {
            w[0] = K_SEG;
            w[1] = u32::from(*seg);
        }
        Record::Select { seat, sel } => {
            w[0] = K_SELECT | (*seat as u32) << 16 | u32::from(sel.chara) << 24;
            w[1] = u32::from(sel.color)
                | u32::from(sel.life) << 8
                | u32::from(sel.charge) << 16
                | u32::from(sel.map) << 24;
            w[2] = u32::from(sel.difficulty)
                | u32::from(sel.cpu_1p) << 8
                | u32::from(sel.cpu_2p) << 16
                | u32::from(sel.background) << 24;
            w[3] = u32::from(sel.match_no) | u32::from(sel.rng_seed) << 16;
            w[4] = sel.option_digest;
        }
        Record::Step(s) => {
            w[0] = K_STEP | (s.kind as u32) << 16 | u32::from(s.value) << 24;
            w[1] = u32::from(s.step) | u32::from(s.screen) << 16;
        }
        Record::Menu { p1, p2 } => {
            w[0] = K_MENU;
            w[1] = u32::from(*p1) | u32::from(*p2) << 16;
        }
        Record::Frame { frame, p1, p2 } => {
            w[0] = K_FRAME;
            w[1] = *frame;
            w[2] = u32::from(*p1) | u32::from(*p2) << 16;
        }
        Record::End(r) => {
            w[0] = K_END;
            w[1] = *r as u32;
        }
        Record::Stats {
            ping_ms,
            latency,
            rollback,
        } => {
            w[0] = K_STATS | u32::from(*latency) << 16 | u32::from(*rollback) << 24;
            w[1] = u32::from(*ping_ms);
        }
        Record::Name { seat, len, bytes } => {
            w[0] = K_NAME | (*seat as u32) << 16 | u32::from(*len) << 24;
            for (dst, c) in w[1..].iter_mut().zip(bytes.as_chunks::<4>().0) {
                *dst = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            }
            return (w, WORDS);
        }
    }
    (w, BASE_WORDS)
}

fn seat_of(v: u32) -> Option<Seat> {
    match v {
        0 => Some(Seat::P1),
        1 => Some(Seat::P2),
        _ => None,
    }
}

/// リングバッファの語から STEP の種類を戻す。形式の切替（`StepKind::Format`）は観戦の流れに積まない（`NetPlay::menu_tick`・`wire::decode`）ので、
/// リングバッファの中にあっても読まない（`None` = 読み手が数える異常）。
fn kind_of(v: u32) -> Option<StepKind> {
    u8::try_from(v)
        .ok()
        .and_then(StepKind::from_u8)
        .filter(|k| *k != StepKind::Format)
}

fn bye_of(v: u32) -> Option<ByeReason> {
    match v {
        1 => Some(ByeReason::Finished),
        2 => Some(ByeReason::Disconnected),
        3 => Some(ByeReason::Desync),
        4 => Some(ByeReason::Cancelled),
        _ => None,
    }
}

fn byte(v: u32, shift: u32) -> u8 {
    (v >> shift) as u8
}

fn half(v: u32, shift: u32) -> u16 {
    (v >> shift) as u16
}

/// 語から記録を戻す。リングバッファの中の値は `pack` しか作らないので `None` は起きないはず（読み手が数える）。
fn unpack(w: &[u32; WORDS]) -> Option<Record> {
    let kind = w[0] & 0xFF;
    Some(match kind {
        K_SEG => Record::Seg { seg: w[1] as u16 },
        K_SELECT => Record::Select {
            seat: seat_of(u32::from(byte(w[0], 16)))?,
            sel: Select {
                match_no: half(w[3], 0),
                chara: byte(w[0], 24),
                color: byte(w[1], 0),
                life: byte(w[1], 8),
                charge: byte(w[1], 16),
                map: byte(w[1], 24),
                difficulty: byte(w[2], 0),
                cpu_1p: byte(w[2], 8),
                cpu_2p: byte(w[2], 16),
                background: byte(w[2], 24),
                rng_seed: half(w[3], 16),
                option_digest: w[4],
            },
        },
        K_STEP => Record::Step(Step {
            step: half(w[1], 0),
            kind: kind_of(u32::from(byte(w[0], 16)))?,
            value: byte(w[0], 24),
            screen: byte(w[1], 16),
        }),
        K_MENU => Record::Menu {
            p1: half(w[1], 0),
            p2: half(w[1], 16),
        },
        K_FRAME => Record::Frame {
            frame: w[1],
            p1: half(w[2], 0),
            p2: half(w[2], 16),
        },
        K_END => Record::End(bye_of(w[1])?),
        K_STATS => Record::Stats {
            ping_ms: half(w[1], 0),
            latency: byte(w[0], 16),
            rollback: byte(w[0], 24),
        },
        K_NAME => {
            let len = byte(w[0], 24);
            if usize::from(len) > higanr_core::wire::NAME_MAX {
                return None;
            }
            let mut bytes = [0u8; higanr_core::wire::NAME_MAX];
            for (dst, v) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&w[1..]) {
                *dst = v.to_le_bytes();
            }
            Record::Name {
                seat: seat_of(u32::from(byte(w[0], 16)))?,
                len,
                bytes,
            }
        }
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Popped {
    pub rec: Record,
    /// 直前に溢れて記録を捨てた（この記録は溢れの後の最初の `Seg`）。
    pub after_gap: bool,
}

/// 数え（ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RingCounts {
    pub pushed: u32,
    /// リングバッファが埋まっていて捨てた記録。
    pub dropped_full: u32,
    /// 溢れの後、次の `Seg` まで捨てた記録。
    pub dropped_gap: u32,
    /// 溢れた回数（欠けの数）。
    pub gaps: u32,
    /// 読み手が戻せなかった枠（起きないはず）。
    pub bad: u32,
}

pub struct SpectateRing {
    cells: Box<[[AtomicU32; WORDS]]>,
    written: AtomicU32,
    read: AtomicU32,
    /// 溢れて、次の `Seg` まで捨てている（書き手だけが書く）。
    gap: AtomicBool,
    pushed: AtomicU32,
    dropped_full: AtomicU32,
    dropped_gap: AtomicU32,
    gaps: AtomicU32,
    bad: AtomicU32,
}

impl SpectateRing {
    /// 確保は起動のときのこの 1 回だけ。
    pub fn new() -> Self {
        Self::with_len(RING_LEN)
    }

    /// 枠の数を指定して作る（検査の小さいリングバッファ。0 は 1 に上げる）。
    pub fn with_len(len: usize) -> Self {
        Self {
            cells: (0..len.max(1))
                .map(|_| [const { AtomicU32::new(0) }; WORDS])
                .collect(),
            written: AtomicU32::new(0),
            read: AtomicU32::new(0),
            gap: AtomicBool::new(false),
            pushed: AtomicU32::new(0),
            dropped_full: AtomicU32::new(0),
            dropped_gap: AtomicU32::new(0),
            gaps: AtomicU32::new(0),
            bad: AtomicU32::new(0),
        }
    }

    /// 書き手（ゲームのスレッド）: 記録を 1 つ書く。書けたら true。確保しない・待たない。溢れたら記録を捨てて `gap` を立て、次の `Seg` まで捨てる。
    pub fn push(&self, rec: Record) -> bool {
        let is_seg = matches!(rec, Record::Seg { .. });
        let gap = self.gap.load(Ordering::Relaxed);
        if gap && !is_seg {
            bump(&self.dropped_gap);
            return false;
        }
        let w = self.written.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        if w.wrapping_sub(r) as usize >= self.cells.len() {
            bump(&self.dropped_full);
            if !gap {
                bump(&self.gaps);
                self.gap.store(true, Ordering::Relaxed);
            }
            return false;
        }
        let Some(cell) = self.cells.get(w as usize % self.cells.len()) else {
            return false;
        };
        let (mut words, used) = pack(&rec);
        if gap {
            words[0] |= FLAG_AFTER_GAP;
            self.gap.store(false, Ordering::Relaxed);
        }
        for (dst, v) in cell.iter().zip(words).take(used) {
            dst.store(v, Ordering::Relaxed);
        }
        self.written.store(w.wrapping_add(1), Ordering::Release);
        bump(&self.pushed);
        true
    }

    /// 読み手（受信スレッド）: 1 つ取り出す。無ければ None。読み手は 1 本だけ。
    pub fn pop(&self) -> Option<Popped> {
        loop {
            let r = self.read.load(Ordering::Relaxed);
            let w = self.written.load(Ordering::Acquire);
            if r == w {
                return None;
            }
            let cell = self.cells.get(r as usize % self.cells.len())?;
            let mut words = [0u32; WORDS];
            for (dst, src) in words.iter_mut().zip(cell.iter()) {
                *dst = src.load(Ordering::Relaxed);
            }
            // 枠を読み終えてから読んだ数を進める（書き手はそれを見てから上書きする）。
            self.read.store(r.wrapping_add(1), Ordering::Release);
            match unpack(&words) {
                Some(rec) => {
                    return Some(Popped {
                        rec,
                        after_gap: words[0] & FLAG_AFTER_GAP != 0,
                    });
                }
                None => bump(&self.bad),
            }
        }
    }

    /// 数え。
    pub fn counts(&self) -> RingCounts {
        let l = |a: &AtomicU32| a.load(Ordering::Relaxed);
        RingCounts {
            pushed: l(&self.pushed),
            dropped_full: l(&self.dropped_full),
            dropped_gap: l(&self.dropped_gap),
            gaps: l(&self.gaps),
            bad: l(&self.bad),
        }
    }
}

impl Default for SpectateRing {
    fn default() -> Self {
        Self::new()
    }
}

/// 数えを 1 進める。同じ数を進めるのは 1 本のスレッドだけなので、読んでから書く形で足りる（`fetch_add` の排他命令を使わない）。
fn bump(a: &AtomicU32) {
    a.store(a.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Record> {
        let sel = |seed: u16| Select {
            match_no: 0xA1B2,
            chara: 0xC3,
            color: 0xD4,
            life: 0xE5,
            charge: 0xF6,
            map: 0x17,
            difficulty: 0x28,
            cpu_1p: 0x39,
            cpu_2p: 0x4A,
            background: 0x5B,
            rng_seed: seed,
            option_digest: 0xDEAD_BEEF,
        };
        let mut v = vec![
            Record::Seg { seg: 0 },
            Record::Seg { seg: u16::MAX },
            Record::Select {
                seat: Seat::P1,
                sel: sel(0xFFFF),
            },
            Record::Select {
                seat: Seat::P2,
                sel: sel(0),
            },
            Record::Menu {
                p1: 0xFFFF,
                p2: 0x0001,
            },
            Record::Frame {
                frame: u32::MAX,
                p1: 0x8001,
                p2: 0x7FFE,
            },
            Record::Frame {
                frame: 0,
                p1: 0,
                p2: 0,
            },
        ];
        v.push(Record::name(Seat::P1, b""));
        v.push(Record::name(Seat::P2, "テスト".as_bytes()));
        v.push(Record::name(Seat::P1, &[b'z'; 32]));
        v.push(Record::Stats {
            ping_ms: 0xA1B2,
            latency: 0xC3,
            rollback: 0xD4,
        });
        v.push(Record::stats(None, 0, None));
        v.push(Record::stats(Some(0), 0, Some(0)));
        v.push(Record::stats(Some(u32::MAX), u8::MAX, Some(u32::MAX)));
        for kind in [StepKind::Decide, StepKind::Cancel, StepKind::Align] {
            v.push(Record::Step(Step {
                step: 0xFFFE,
                kind,
                value: 0xFF,
                screen: 0x10,
            }));
        }
        for r in [
            ByeReason::Finished,
            ByeReason::Disconnected,
            ByeReason::Desync,
            ByeReason::Cancelled,
        ] {
            v.push(Record::End(r));
        }
        v
    }

    #[test]
    fn every_kind_of_record_survives_the_round_trip() {
        let ring = SpectateRing::with_len(64);
        for rec in samples() {
            assert!(ring.push(rec), "{rec:?}");
            assert_eq!(
                ring.pop(),
                Some(Popped {
                    rec,
                    after_gap: false
                })
            );
        }
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.counts().pushed as usize, samples().len());
    }

    #[test]
    fn a_format_switch_step_is_never_read_back_from_the_ring() {
        let ring = SpectateRing::with_len(8);
        let step = |kind| {
            Record::Step(Step {
                step: 3,
                kind,
                value: 0x0F,
                screen: 8,
            })
        };
        let frame = Record::Frame {
            frame: 1,
            p1: 2,
            p2: 3,
        };
        assert!(ring.push(step(StepKind::Decide)));
        assert!(ring.push(step(StepKind::Format)));
        assert!(ring.push(frame));
        assert_eq!(ring.pop().map(|p| p.rec), Some(step(StepKind::Decide)));
        assert_eq!(
            ring.pop().map(|p| p.rec),
            Some(frame),
            "形式の切替が読めた、または後ろの記録まで失った"
        );
        assert_eq!(ring.counts().bad, 1, "読めない記録を数えない");
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn a_short_record_reuses_a_cell_that_held_a_name() {
        let ring = SpectateRing::with_len(1);
        let name = Record::name(Seat::P2, &[b'q'; 32]);
        let frame = Record::Frame {
            frame: 7,
            p1: 0x8001,
            p2: 2,
        };
        for _ in 0..3 {
            assert!(ring.push(name));
            assert_eq!(ring.pop().map(|p| p.rec), Some(name));
            assert!(ring.push(frame));
            assert_eq!(ring.pop().map(|p| p.rec), Some(frame));
        }
        // 短い名前の枠に、長い名前の残りが混ざらない。
        let short = Record::name(Seat::P1, b"a");
        assert!(ring.push(short));
        assert_eq!(ring.pop().map(|p| p.rec), Some(short));
        assert_eq!(ring.counts().bad, 0);
    }

    #[test]
    fn a_full_ring_drops_until_the_next_seg_and_marks_it() {
        let ring = SpectateRing::with_len(5);
        let f = |n: u32| Record::Frame {
            frame: n,
            p1: 1,
            p2: 2,
        };
        assert!(ring.push(Record::Seg { seg: 0 }));
        for n in 0..4 {
            assert!(ring.push(f(n)));
        }
        // 満杯。捨てて、次の Seg まで捨て続ける（読み手が空けても、Seg でない記録は入れない）。
        assert!(!ring.push(f(4)));
        assert_eq!(ring.pop().map(|p| p.rec), Some(Record::Seg { seg: 0 }));
        assert!(!ring.push(f(5)), "溢れの後の Seg でない記録を入れた");
        assert!(!ring.push(Record::End(ByeReason::Finished)));
        assert!(ring.push(Record::Seg { seg: 1 }));
        assert_eq!(ring.pop().map(|p| p.rec), Some(f(0)));
        assert!(ring.push(f(9)));
        let mut got = Vec::new();
        while let Some(p) = ring.pop() {
            got.push((p.rec, p.after_gap));
        }
        assert_eq!(
            got,
            vec![
                (f(1), false),
                (f(2), false),
                (f(3), false),
                (Record::Seg { seg: 1 }, true),
                (f(9), false),
            ]
        );
        let c = ring.counts();
        assert_eq!(
            (c.gaps, c.dropped_full, c.dropped_gap, c.bad),
            (1, 1, 2, 0),
            "{c:?}"
        );
        assert!(ring.push(Record::Seg { seg: 2 }));
        assert_eq!(
            ring.pop(),
            Some(Popped {
                rec: Record::Seg { seg: 2 },
                after_gap: false
            })
        );
    }

    #[test]
    fn the_writer_never_allocates() {
        let ring = SpectateRing::with_len(64);
        assert!(ring.push(Record::Seg { seg: 0 }));
        let _ = ring.pop();
        let before = crate::test_alloc::count();
        for n in 0..200u32 {
            ring.push(Record::Frame {
                frame: n,
                p1: 1,
                p2: 2,
            });
            let _ = ring.pop();
        }
        assert_eq!(
            crate::test_alloc::count() - before,
            0,
            "リングバッファで確保した"
        );
    }
}
