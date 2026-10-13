//! 描画だけのフレームの実験の予定と集計（pure）。読み取りのスレッドで走る（ゲームのスレッドではない）。
//! - 予定: 設置の後 20 秒は作らない（基準）。その後 3 秒ごとに N = 1, 2, 3, 10, 60 を順に繰り返し、設置から 180 秒で終える。
//!   0 から N にするのは読み取りのスレッドだけ（`stub::try_start_render_only`。compare-exchange で 0 のときだけ）、減らすのは入口だけ。
//!   判定は時刻から決まる純粋な計算で、`RENDER_ONLY_LEFT` を置くのは `observe.rs`。
//! - 集計: 描画だけのフレームの件 k と次の件 k+1 を比べ、`+0x454` が進んでいないか / 抽選回数 `0x4ACE10` が同じか /
//!   コピーの範囲（548 バイト）の中で変わったアドレスと回数 / 番号 k を持つ描画の件の数 / 前後の件との間隔（TSC）。
//!   デモの間（`0x4A7EC4` の bit1）とそれ以外に分けて数える。
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Write;

use crate::copies::{CHANGE_RANGES, change_words};
use crate::draw::DrawRecord;
use crate::th09_addr::{GLOBAL_STATE, GLOBAL_STATE_DEMO, RNG_COUNT};
use crate::ticks::{Snap, tsc_to_us};

// ---- 予定 ----

/// 設置から実験を終えるまで（秒）。
pub const TOTAL_SECONDS: u32 = 180;
/// 基準の長さ（この秒までは描画だけのフレームを作らない）。
pub const BASELINE_SECONDS: u32 = 20;
/// 塊と塊の間隔（秒）。
pub const STEP_SECONDS: u32 = 3;
/// N の並び（順に繰り返す）。
pub const N_SEQUENCE: [u32; 5] = [1, 2, 3, 10, 60];

/// 経過 `second` 秒の区切りで、描画だけのフレームを何回頼むか（頼まない秒は `None`）。基準の間と終わりは `None`。
pub fn burst_at(second: u32) -> Option<u32> {
    if !(BASELINE_SECONDS..TOTAL_SECONDS).contains(&second) {
        return None;
    }
    let since = second - BASELINE_SECONDS;
    if !since.is_multiple_of(STEP_SECONDS) {
        return None;
    }
    let step = since / STEP_SECONDS;
    Some(N_SEQUENCE[(step as usize) % N_SEQUENCE.len()])
}

// ---- 集計 ----

/// tick `k` の後に走った描画が、描画のリングバッファに書く番号（`DrawRecord::tick_seq`）。**`k + 1`。**
///
/// 入力の入口（`stub.rs` の `input_wait_entry`）は `lock xadd` で `SEQ` の値 k を自分の番号として取り `SEQ` を k+1 へ進め、
/// 描画の入口（`draw.rs` の `draw_entry`）は `SEQ` をそのまま書く。tick k と同じ do_frame の描画は k+1 を書くので、
/// k で引くと前のフレームの描画を数えて、いつも 1 回に見える。この関係はここ 1 か所に置く。
pub fn draw_tag_after_tick(k: u32) -> u32 {
    k.wrapping_add(1)
}

/// デモの間か（`0x4A7EC4` の bit1 = タイトルデモ再生中）。読めなければ `None`。
pub fn is_demo(rows: &[crate::ticks::CopyRow], snap: &Snap) -> Option<bool> {
    crate::copies::value_at(rows, snap, GLOBAL_STATE, 4).map(|v| v & GLOBAL_STATE_DEMO != 0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderOnlyFrame {
    pub seq: u32,
    pub demo: Option<bool>,
    pub has_next: bool,
    /// `+0x454`（`sup_frame`）が k から k+1 で進んだか。**描画だけのフレームでは進まないはず。**
    pub frame_advanced: Option<bool>,
    /// 抽選回数 `0x4ACE10` の k のときの値（読めなければ `None`）。
    pub rng_count: Option<u32>,
    /// 抽選回数が k と k+1 で同じか。**同じはず。**
    pub rng_count_same: Option<bool>,
    /// コピーの範囲（548 バイト）の中で k から k+1 で変わったアドレス（アドレス・前・後）。
    pub changed: Vec<(u32, u32, u32)>,
    /// tick k と k+1 の間に走った描画の数（描画のリングバッファで `tick_seq == draw_tag_after_tick(k)` の件）。1 なら更新なしで描画だけが 1 回走った。
    pub draws_for_k: u32,
    pub dt_tsc: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderOnlySummary {
    pub frames: usize,
    pub demo_frames: usize,
    pub nondemo_frames: usize,
    /// `+0x454` が進んでしまった件（0 のはず。進むと「更新が走った」＝出口を満たさない）。
    pub frame_advanced_count: usize,
    /// 抽選回数が変わってしまった件（0 のはず）。
    pub rng_changed_count: usize,
    /// tick k と tick k+1 の間に描画がちょうど 1 回走った件（1 でない件は下の `draws_not_one`）。
    pub draws_one_count: usize,
    pub draws_not_one: usize,
    /// 変わったアドレスごとの件数（何件の描画だけのフレームでそのアドレスが変わったか。アドレスの順）。
    pub changed_addr_hist: Vec<(u32, usize)>,
}

/// 描画だけのフレーム（k）と次の件（k+1。無ければ `None`）の組から集計する。`draws_for(n)` は描画のリングバッファで `tick_seq == n` の件の数
/// （`draw_tag_after_tick(k)` で引く）。
pub fn analyze(
    frames: &[(Snap, Option<Snap>)],
    rows: &[crate::ticks::CopyRow],
    draws_for: impl Fn(u32) -> u32,
) -> (Vec<RenderOnlyFrame>, RenderOnlySummary) {
    let mut out = Vec::new();
    let mut hist: BTreeMap<u32, usize> = BTreeMap::new();
    let mut s = RenderOnlySummary {
        frames: 0,
        demo_frames: 0,
        nondemo_frames: 0,
        frame_advanced_count: 0,
        rng_changed_count: 0,
        draws_one_count: 0,
        draws_not_one: 0,
        changed_addr_hist: Vec::new(),
    };
    for (k, next) in frames {
        let demo = is_demo(rows, k);
        let rng_count = crate::copies::value_at(rows, k, RNG_COUNT, 4);
        let draws_for_k = draws_for(draw_tag_after_tick(k.rec.seq));
        let (mut frame_advanced, mut rng_count_same, mut changed, mut dt_tsc) =
            (None, None, Vec::new(), None);
        if let Some(nx) = next {
            frame_advanced = Some(nx.rec.sup_frame != k.rec.sup_frame);
            rng_count_same = match (rng_count, crate::copies::value_at(rows, nx, RNG_COUNT, 4)) {
                (Some(a), Some(b)) => Some(a == b),
                _ => None,
            };
            changed = changed_addresses(rows, k, nx);
            dt_tsc = Some(nx.rec.tsc.wrapping_sub(k.rec.tsc));
        }
        s.frames += 1;
        match demo {
            Some(true) => s.demo_frames += 1,
            Some(false) => s.nondemo_frames += 1,
            None => {}
        }
        if frame_advanced == Some(true) {
            s.frame_advanced_count += 1;
        }
        if rng_count_same == Some(false) {
            s.rng_changed_count += 1;
        }
        if draws_for_k == 1 {
            s.draws_one_count += 1;
        } else {
            s.draws_not_one += 1;
        }
        for &(addr, _, _) in &changed {
            *hist.entry(addr).or_default() += 1;
        }
        out.push(RenderOnlyFrame {
            seq: k.rec.seq,
            demo,
            has_next: next.is_some(),
            frame_advanced,
            rng_count,
            rng_count_same,
            changed,
            draws_for_k,
            dt_tsc,
        });
    }
    s.changed_addr_hist = hist.into_iter().collect();
    (out, s)
}

/// コピーの範囲（`CHANGE_RANGES` の 548 バイト）を 4 バイトずつ、k と k+1 で比べて変わったアドレス（アドレス・前・後）。
/// 表に無くて読めない語は数えない。
fn changed_addresses(
    rows: &[crate::ticks::CopyRow],
    k: &Snap,
    next: &Snap,
) -> Vec<(u32, u32, u32)> {
    let a = change_words(rows, k);
    let b = change_words(rows, next);
    let mut out = Vec::new();
    for (&(addr, va), &(addr2, vb)) in a.iter().zip(b.iter()) {
        debug_assert_eq!(addr, addr2);
        if let (Some(before), Some(after)) = (va, vb)
            && before != after
        {
            out.push((addr, before, after));
        }
    }
    out
}

pub fn changed_words_total() -> u32 {
    CHANGE_RANGES.iter().map(|(_, len)| len / 4).sum()
}

/// 描画だけのフレームと次の件（k+1）を、tick の流れから拾い集める（100 ms ごとに読む件を順に食わせる）。描画の記録は番号ごとに数える。
#[derive(Debug, Default)]
pub struct Collector {
    /// 次の件（k+1）を待っている描画だけのフレーム。
    pending: Option<Snap>,
    /// 拾った (k, k+1)。次の件が採れなければ k+1 は `None`（番号が飛んだ・計測が終わった）。
    pub frames: Vec<(Snap, Option<Snap>)>,
    draws_by_seq: BTreeMap<u32, u32>,
    pub seen: usize,
}

impl Collector {
    /// tick の件を 1 つ食わせる（番号の順に呼ぶこと）。
    pub fn feed_tick(&mut self, s: &Snap) {
        if let Some(p) = self.pending.take() {
            if s.rec.seq == p.rec.seq.wrapping_add(1) {
                self.frames.push((p, Some(s.clone())));
            } else {
                // 次の件が飛んだ（読み落とし・番号の乱れ）。k+1 は採れない。
                self.frames.push((p, None));
            }
        }
        if s.render_only_mark() {
            self.seen += 1;
            self.pending = Some(s.clone());
        }
    }

    pub fn feed_draw(&mut self, r: &DrawRecord) {
        *self.draws_by_seq.entry(r.tick_seq).or_default() += 1;
    }

    /// 描画のリングバッファで `tick_seq == seq` の件の数（生の番号。tick k の後の描画は `draws_for(draw_tag_after_tick(k))`）。
    pub fn draws_for(&self, seq: u32) -> u32 {
        self.draws_by_seq.get(&seq).copied().unwrap_or(0)
    }

    pub fn finish(&mut self) {
        if let Some(p) = self.pending.take() {
            self.frames.push((p, None));
        }
    }
}

// ---- CSV と集計のログ ----

pub const RENDERONLY_HEADER: &str =
    "seq,demo,has_next,frame_advanced,rng_count,rng_same,draws_for_k,dt_us,changed_count";

/// 描画だけのフレーム 1 件の行（`freq_hz` があれば間隔を µs で）。
pub fn renderonly_row(f: &RenderOnlyFrame, freq_hz: Option<f64>) -> String {
    let b = |o: Option<bool>| match o {
        Some(true) => "1",
        Some(false) => "0",
        None => "",
    };
    let dt = match (f.dt_tsc, freq_hz) {
        (Some(d), Some(fq)) => format!("{:.3}", tsc_to_us(d, fq)),
        _ => String::new(),
    };
    let rng = f.rng_count.map(|v| v.to_string()).unwrap_or_default();
    format!(
        "{},{},{},{},{rng},{},{},{dt},{}",
        f.seq,
        b(f.demo),
        if f.has_next { 1 } else { 0 },
        b(f.frame_advanced),
        b(f.rng_count_same),
        f.draws_for_k,
        f.changed.len()
    )
}

pub const DRAWS_HEADER: &str = "seq,tick_seq,t_us,dt_us";

/// 描画の記録の行を流して作る（`t_us` は最初の件からの µs、`dt_us` は前の件との差。周波数が無ければ空）。
#[derive(Debug, Clone, Default)]
pub struct DrawCsv {
    pub freq_hz: Option<f64>,
    first_tsc: Option<u64>,
    prev_tsc: Option<u64>,
}

impl DrawCsv {
    pub fn new(freq_hz: Option<f64>) -> Self {
        DrawCsv {
            freq_hz,
            first_tsc: None,
            prev_tsc: None,
        }
    }

    pub fn row(&mut self, r: &DrawRecord) -> String {
        let first = *self.first_tsc.get_or_insert(r.tsc);
        let t = self
            .freq_hz
            .map(|f| format!("{:.3}", tsc_to_us(r.tsc.wrapping_sub(first), f)))
            .unwrap_or_default();
        let dt = match (self.prev_tsc, self.freq_hz) {
            (Some(p), Some(f)) => format!("{:.3}", tsc_to_us(r.tsc.wrapping_sub(p), f)),
            _ => String::new(),
        };
        self.prev_tsc = Some(r.tsc);
        format!("{},{},{t},{dt}", r.seq, r.tick_seq)
    }
}

pub fn write_summary(out: &mut dyn Write, s: &RenderOnlySummary, draw_calls: u32, draw_seq: u32) {
    let _ = writeln!(
        out,
        "描画だけのフレームの実験: 描画の地点を通った回数 {draw_calls}、描画のリングバッファに書いた件 {draw_seq}"
    );
    let _ = writeln!(
        out,
        "描画だけのフレーム: {} 件（デモの間 {} / それ以外 {}）",
        s.frames, s.demo_frames, s.nondemo_frames
    );
    let _ = writeln!(
        out,
        "描画だけのフレーム: +0x454 が進んだ {} 件（0 のはず）/ 抽選回数が変わった {} 件（0 のはず）/ tick k と k+1 の間に描画がちょうど 1 回走った {} 件・そうでない {} 件",
        s.frame_advanced_count, s.rng_changed_count, s.draws_one_count, s.draws_not_one
    );
    if s.changed_addr_hist.is_empty() {
        let _ = writeln!(
            out,
            "描画だけのフレーム: コピーの範囲（{} 語）で変わったアドレスは無い（決定的）",
            changed_words_total()
        );
    } else {
        let list: Vec<String> = s
            .changed_addr_hist
            .iter()
            .map(|(a, n)| format!("0x{a:08X}: {n} 件"))
            .collect();
        let _ = writeln!(
            out,
            "描画だけのフレーム: コピーの範囲（{} 語）で変わったアドレス {} 種類（アドレス: 変わった描画だけのフレームの数）{}",
            changed_words_total(),
            s.changed_addr_hist.len(),
            list.join(" / ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copies::tests::snap_with;
    use crate::th09_addr::SEATS_RANGE;
    use crate::ticks::{COPY_TABLE, OFF_RENDER_ONLY};

    #[test]
    fn schedule_is_baseline_then_n_sequence_every_three_seconds() {
        for s in [0, 1, 10, 19] {
            assert_eq!(burst_at(s), None, "{s}");
        }
        assert_eq!(burst_at(20), Some(1));
        assert_eq!(burst_at(23), Some(2));
        assert_eq!(burst_at(26), Some(3));
        assert_eq!(burst_at(29), Some(10));
        assert_eq!(burst_at(32), Some(60));
        assert_eq!(burst_at(35), Some(1), "並びが繰り返さない");
        assert_eq!(burst_at(38), Some(2));
        for s in [21, 22, 24, 25, 30, 31] {
            assert_eq!(burst_at(s), None, "{s}");
        }
        assert_eq!(burst_at(179), Some(10), "179 = 20 + 3×53、53 % 5 = 3");
        assert_eq!(burst_at(TOTAL_SECONDS), None);
        assert_eq!(burst_at(TOTAL_SECONDS + 3), None);
        let n = (BASELINE_SECONDS..TOTAL_SECONDS)
            .filter(|&s| burst_at(s).is_some())
            .count();
        assert_eq!(
            n,
            (TOTAL_SECONDS - BASELINE_SECONDS).div_ceil(STEP_SECONDS) as usize
        );
        assert_eq!(n, 54);
    }

    fn ro_snap(seq: u32, sup: u32, demo: bool, rng: u32, mark: bool) -> Snap {
        let mut s = snap_with(seq, sup, |_| 0);
        let mut words = *s.words;
        let put = |words: &mut [u32], addr: u32, v: u32| {
            let r = COPY_TABLE
                .iter()
                .find(|r| addr >= r.src && addr + 4 <= r.src + r.len)
                .unwrap();
            let off = r.dst + addr - r.src;
            words[(off / 4) as usize] = v;
        };
        put(&mut words, GLOBAL_STATE, if demo { 0x0E } else { 0x04 });
        put(&mut words, RNG_COUNT, rng);
        words[(OFF_RENDER_ONLY / 4) as usize] = if mark { 1 } else { 0 };
        s = Snap::from_words(seq, Box::new(words));
        s
    }

    #[test]
    fn analyze_render_only_frames() {
        let rows = COPY_TABLE.to_vec();
        let k0 = ro_snap(0, 100, true, 7, true);
        let mut n0 = ro_snap(1, 100, true, 7, false);
        {
            let mut w = *n0.words;
            let r = COPY_TABLE
                .iter()
                .find(|r| SEATS_RANGE + 0xF4 >= r.src)
                .unwrap();
            let off = r.dst + (crate::th09_addr::BACKGROUND - r.src);
            w[(off / 4) as usize] = 0x1234;
            n0 = Snap::from_words(1, Box::new(w));
        }
        let k1 = ro_snap(2, 200, true, 10, true);
        let n1 = ro_snap(3, 201, true, 11, false);
        let k2 = ro_snap(4, 300, false, 20, true);

        let frames = vec![
            (k0.clone(), Some(n0.clone())),
            (k1.clone(), Some(n1.clone())),
            (k2.clone(), None),
        ];
        // 描画のリングバッファは本物の関係で作る: tick k と同じ do_frame の描画は tick_seq = k+1 を書く。前の do_frame の描画は tick_seq = k で、
        // どの k にも 1 件ずつある（数えてはいけない）。番号は `draw_tag_after_tick` を通さずに文字で書く（関数を k に戻す変異で落ちるように）。
        let draws_for = |tick_seq: u32| match tick_seq {
            0 | 2 | 4 => 1, // 1 つ前の do_frame の描画
            1 => 1,
            3 => 2,
            _ => 0,
        };
        let (rows_out, s) = analyze(&frames, &rows, draws_for);
        assert_eq!(s.frames, 3);
        assert_eq!((s.demo_frames, s.nondemo_frames), (2, 1));
        assert_eq!(s.frame_advanced_count, 1, "sup が進んだのは件 2 だけ");
        assert_eq!(s.rng_changed_count, 1, "抽選回数が変わったのは件 2 だけ");
        assert_eq!((s.draws_one_count, s.draws_not_one), (1, 2));
        assert_eq!(
            rows_out[0].changed,
            vec![(crate::th09_addr::BACKGROUND, 0, 0x1234)]
        );
        assert_eq!(rows_out[0].frame_advanced, Some(false));
        assert_eq!(rows_out[0].rng_count_same, Some(true));
        assert_eq!(rows_out[0].draws_for_k, 1);
        assert_eq!(rows_out[0].demo, Some(true));
        assert!(!rows_out[2].has_next);
        assert_eq!(rows_out[2].frame_advanced, None);
        assert_eq!(rows_out[2].demo, Some(false));
        assert_eq!(
            rows_out[2].draws_for_k, 0,
            "tick 4 の後の描画（tick_seq 5）は無い。前の do_frame の tick_seq 4 を数えた"
        );
        assert_eq!(
            rows_out[1].draws_for_k, 2,
            "tick 2 の後の描画（tick_seq 3）"
        );
        assert!(
            s.changed_addr_hist
                .iter()
                .any(|&(a, n)| a == crate::th09_addr::BACKGROUND && n == 1)
        );
    }

    #[test]
    fn previous_frame_draw_is_not_counted() {
        let rows = COPY_TABLE.to_vec();
        let mut c = Collector::default();
        c.feed_tick(&ro_snap(9, 100, true, 7, false));
        c.feed_draw(&DrawRecord {
            seq: 0,
            tsc: 1,
            tick_seq: 10,
        });
        c.feed_tick(&ro_snap(10, 100, true, 7, true));
        c.feed_tick(&ro_snap(11, 100, true, 7, false));
        c.finish();
        let (f, s) = analyze(&c.frames, &rows, |n| c.draws_for(n));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].seq, 10);
        assert_eq!(
            f[0].draws_for_k, 0,
            "前の do_frame の描画（tick_seq 10）を数えた"
        );
        assert_eq!((s.draws_one_count, s.draws_not_one), (0, 1));
        c.feed_draw(&DrawRecord {
            seq: 1,
            tsc: 2,
            tick_seq: 11,
        });
        let (f, _) = analyze(&c.frames, &rows, |n| c.draws_for(n));
        assert_eq!(f[0].draws_for_k, 1);
        assert_eq!(draw_tag_after_tick(u32::MAX), 0, "番号の回り込み");
    }

    #[test]
    fn collector_pairs_render_only_frames_with_the_next() {
        let mut c = Collector::default();
        c.feed_tick(&ro_snap(0, 100, true, 7, false));
        c.feed_tick(&ro_snap(1, 100, true, 7, true));
        c.feed_tick(&ro_snap(2, 100, true, 7, false));
        c.feed_tick(&ro_snap(3, 200, true, 8, true));
        c.feed_tick(&ro_snap(5, 300, false, 9, true));
        c.feed_draw(&DrawRecord {
            seq: 0,
            tsc: 1,
            tick_seq: 1,
        });
        c.feed_draw(&DrawRecord {
            seq: 1,
            tsc: 2,
            tick_seq: 1,
        });
        c.feed_draw(&DrawRecord {
            seq: 2,
            tsc: 3,
            tick_seq: 3,
        });
        c.finish();
        assert_eq!(c.seen, 3);
        assert_eq!(c.frames.len(), 3);
        assert_eq!(c.frames[0].0.rec.seq, 1);
        assert_eq!(c.frames[0].1.as_ref().unwrap().rec.seq, 2, "1 の次は 2");
        assert_eq!(c.frames[1].0.rec.seq, 3);
        assert!(c.frames[1].1.is_none(), "3 の次（4）は読み落とし");
        assert_eq!(c.frames[2].0.rec.seq, 5);
        assert!(c.frames[2].1.is_none(), "5 の次は終わりまで来ない");
        assert_eq!(c.draws_for(1), 2);
        assert_eq!(c.draws_for(3), 1);
        assert_eq!(c.draws_for(2), 0);
    }

    #[test]
    fn draw_csv_rows() {
        let mut csv = DrawCsv::new(Some(3.0e9));
        let r0 = DrawRecord {
            seq: 0,
            tsc: 1000,
            tick_seq: 100,
        };
        let r1 = DrawRecord {
            seq: 1,
            tsc: 1000 + 3_000_000,
            tick_seq: 101,
        };
        assert_eq!(csv.row(&r0), "0,100,0.000,");
        assert_eq!(csv.row(&r1), "1,101,1000.000,1000.000");
        let mut csv = DrawCsv::new(None);
        assert_eq!(csv.row(&r1), "1,101,,");
        assert_eq!(DRAWS_HEADER, "seq,tick_seq,t_us,dt_us");
        assert!(RENDERONLY_HEADER.starts_with("seq,demo,"));
    }
}
