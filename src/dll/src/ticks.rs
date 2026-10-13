//! tick ごとの記録の形・札の判定・集計・CSV の行（pure。unsafe 無し）。
//!
//! 入口（`stub.rs` の素のアセンブリ）が 1 件 1024 バイトの枠をリングバッファに書き（先頭の 64 バイトは前のバージョンと同じ配置、その後ろにコピーの表の範囲）、
//! 読み取りのスレッドが札・中身・札の順に読む。ここは「枠のどこに何があるか」（入口のアセンブリもこの定数を使う）と、
//! 読んだ札の判定、読んだ件の集計・出口の判定・CSV の行を持つ。Win32 にもゲームのメモリにも触らない。
//! 読み取りのスレッドで走るので、ヒープも I/O も構わない。
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Write;

use crate::th09_addr::{
    INPUT_COPY_LEN, INPUT_MANAGERS, MATCH_LEN, MATCH_RANGE, RNG_ADDR, RNG_LEN, SEATS_LEN,
    SEATS_RANGE,
};

/// リングバッファの件数（60 fps で約 68 秒分）。2 の冪（番号から枠を `and` で引くため）。
pub const RING_LEN: u32 = 4096;
pub const RING_MASK: u32 = RING_LEN - 1;
/// 1 件の大きさ。リングバッファは 4096 件 = 4 MiB で DLL の .bss。
pub const SLOT_BYTES: u32 = 1024;
/// 枠の番号 → リングバッファの先頭からのバイト数の `shl` の値（1024 = 1 << 10）。
pub const SLOT_SHIFT: u32 = 10;
pub const SLOT_WORDS: usize = (SLOT_BYTES / 4) as usize;
/// 先頭の大きさ。前のバージョンの 64 バイトの枠と同じ配置で、札は `+48`、`+52` は描画だけのフレームの印、`+56`〜`+63` は予備。
/// コピーはこの後ろにだけ置く。
pub const HEAD_BYTES: u32 = 64;
// 札（と先頭の欄）は先頭の中にある（コピーがどれも先頭の後ろなら、コピーが札を潰さない）。
const _: () = assert!(OFF_TICKET + 4 <= HEAD_BYTES);

/// コピーの表の 1 行: ゲームの `src` から `len` バイトを、枠の `dst` バイト目へコピーする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyRow {
    pub src: u32,
    pub len: u32,
    pub dst: u32,
}

/// コピーの表の行の上限。このバージョンは 6 行を使い、2 行は予備。
pub const COPY_MAX_ROWS: usize = 8;

// コピーの置き場（枠の中の位置）。先頭の直後から、4 バイト境界にそろえて順に詰める（合計 548 バイトで 612 まで。
// 612〜1023 は予備の 2 行のための空き）。
/// 席のブロックからマップ・一時停止までの範囲（320 バイト）。
pub const OFF_COPY_SEATS: u32 = HEAD_BYTES;
/// 初期ライフ・読み込みの終わり・対戦の登録の範囲（48 バイト）。
pub const OFF_COPY_MATCH: u32 = OFF_COPY_SEATS + SEATS_LEN;
/// 乱数の 12 バイト。
pub const OFF_COPY_RNG: u32 = OFF_COPY_MATCH + MATCH_LEN;
/// InputManager 3 本の `+0x00`〜`+0x37`（56 バイトずつ）。
pub const OFF_COPY_INPUT: [u32; 3] = [
    OFF_COPY_RNG + RNG_LEN,
    OFF_COPY_RNG + RNG_LEN + INPUT_COPY_LEN,
    OFF_COPY_RNG + RNG_LEN + 2 * INPUT_COPY_LEN,
];

/// このバージョンのコピーの表（6 行）。アドレスは `th09_addr`、置き場は上の `OFF_COPY_*` から作る。
pub const COPY_TABLE: [CopyRow; 6] = [
    CopyRow {
        src: SEATS_RANGE,
        len: SEATS_LEN,
        dst: OFF_COPY_SEATS,
    },
    CopyRow {
        src: MATCH_RANGE,
        len: MATCH_LEN,
        dst: OFF_COPY_MATCH,
    },
    CopyRow {
        src: RNG_ADDR,
        len: RNG_LEN,
        dst: OFF_COPY_RNG,
    },
    CopyRow {
        src: INPUT_MANAGERS[0],
        len: INPUT_COPY_LEN,
        dst: OFF_COPY_INPUT[0],
    },
    CopyRow {
        src: INPUT_MANAGERS[1],
        len: INPUT_COPY_LEN,
        dst: OFF_COPY_INPUT[1],
    },
    CopyRow {
        src: INPUT_MANAGERS[2],
        len: INPUT_COPY_LEN,
        dst: OFF_COPY_INPUT[2],
    },
];

/// 枠 1 件ぶんの中身（札・中身・札の読みを通ったもの）と、その先頭を読んだもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    pub rec: Record,
    pub words: Box<[u32; SLOT_WORDS]>,
}

impl Snap {
    pub fn from_words(seq: u32, words: Box<[u32; SLOT_WORDS]>) -> Self {
        let mut head = [0u32; BODY_WORDS];
        head.copy_from_slice(&words[..BODY_WORDS]);
        Snap {
            rec: Record::from_words(seq, &head),
            words,
        }
    }

    /// 枠の `off` バイト目の 1 バイト（範囲の外は `None`）。語はリトルエンディアンで並ぶ（x86 の書きと同じ）。
    pub fn byte(&self, off: u32) -> Option<u8> {
        let w = self.words.get((off / 4) as usize)?;
        Some(w.to_le_bytes()[(off % 4) as usize])
    }

    /// 枠の `off` バイト目から `width`（1〜4）バイトを、リトルエンディアンの数として読む（4 バイト境界に無くてよい）。
    pub fn read(&self, off: u32, width: u32) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..width {
            v |= (self.byte(off.checked_add(i)?)? as u32) << (8 * i);
        }
        Some(v)
    }

    /// 枠の 1024 バイト（`_snap.bin` に書く形）。
    pub fn bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

/// TSC（`rdtsc`）の下位 / 上位。
pub const OFF_TSC_LO: u32 = 0;
pub const OFF_TSC_HI: u32 = 4;
/// ウィンドウ `+0x2C`（今の時刻、double）の 8 バイト。ウィンドウのアドレスが 0 なら 0。
pub const OFF_NOW_LO: u32 = 8;
pub const OFF_NOW_HI: u32 = 12;
/// ウィンドウ `+0x3C`（期限、double）の 8 バイト。ウィンドウのアドレスが 0 なら 0。
pub const OFF_DEADLINE_LO: u32 = 16;
pub const OFF_DEADLINE_HI: u32 = 20;
/// on_tick の this の `+0x454`（tick ごとに 1 増える欄）。
pub const OFF_SUP_FRAME: u32 = 24;
/// on_tick の this の `+0x58C`（今の場面）/ `+0x590`（次の場面の要求）。
pub const OFF_SCENE_CUR: u32 = 28;
pub const OFF_SCENE_REQ: u32 = 32;
/// esi そのもの（on_tick の this）。
pub const OFF_ESI: u32 = 36;
/// スレッド ID（`fs:[0x24]`）。
pub const OFF_TID: u32 = 40;
/// FPU の制御語（`fnstcw` の 2 バイト。上位 2 バイトは 0）。
pub const OFF_FPUCW: u32 = 44;
/// 番号札（seq + 1）。0 は「書いている途中」。
pub const OFF_TICKET: u32 = 48;
/// 中身の語の数（札の手前まで）。`+52` の印は `render_only_mark` で読む。`+56`〜`+63` は予備で、入口は書かない。
pub const BODY_WORDS: usize = (OFF_TICKET / 4) as usize;
/// 描画だけのフレームの印（4 バイト）。入口は毎回書く（普通の件は 0、描画だけのフレームは 1）: リングバッファは一周して枠を使い回すので、
/// 普通の件で 0 を書かないと前の周の 1 が残る。札より前に書く（札・中身・札の読みで中身と一緒に採れる）。
pub const OFF_RENDER_ONLY: u32 = 52;
const _: () = assert!(OFF_RENDER_ONLY == OFF_TICKET + 4 && OFF_RENDER_ONLY + 4 <= HEAD_BYTES);

impl Snap {
    pub fn render_only_mark(&self) -> bool {
        self.read(OFF_RENDER_ONLY, 4) == Some(1)
    }
}

/// 読み取りのスレッドが 1 回の実行で持つ件数の上限（先頭だけ。1 件 56 バイト）。15 分を 60 fps で 54,000 件、1 フレームに 3 回
/// 呼ばれ続けても 162,000 件なので、その上の 2 の冪（約 14 MiB）。越えた分は数だけ数える（CSV とスナップには書く）。
pub const MAX_RECORDS: usize = 1 << 18;

/// 出口の判定の長さ。
pub const EXIT_RUN: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub seq: u32,
    pub tsc: u64,
    pub now_bits: u64,
    pub deadline_bits: u64,
    pub sup_frame: u32,
    pub scene_cur: u32,
    pub scene_req: u32,
    pub esi: u32,
    pub tid: u32,
    /// `+44` の 4 バイト（下位 2 バイトが制御語、上位 2 バイトは 0 のはず）。
    pub fpucw: u32,
}

fn word(w: &[u32; BODY_WORDS], off: u32) -> u32 {
    w[(off / 4) as usize]
}

impl Record {
    pub fn from_words(seq: u32, w: &[u32; BODY_WORDS]) -> Self {
        let pair = |lo, hi| (word(w, hi) as u64) << 32 | word(w, lo) as u64;
        Record {
            seq,
            tsc: pair(OFF_TSC_LO, OFF_TSC_HI),
            now_bits: pair(OFF_NOW_LO, OFF_NOW_HI),
            deadline_bits: pair(OFF_DEADLINE_LO, OFF_DEADLINE_HI),
            sup_frame: word(w, OFF_SUP_FRAME),
            scene_cur: word(w, OFF_SCENE_CUR),
            scene_req: word(w, OFF_SCENE_REQ),
            esi: word(w, OFF_ESI),
            tid: word(w, OFF_TID),
            fpucw: word(w, OFF_FPUCW),
        }
    }

    #[cfg(test)]
    pub fn to_words(self) -> [u32; BODY_WORDS] {
        let mut w = [0u32; BODY_WORDS];
        let mut put = |off: u32, v: u32| w[(off / 4) as usize] = v;
        put(OFF_TSC_LO, self.tsc as u32);
        put(OFF_TSC_HI, (self.tsc >> 32) as u32);
        put(OFF_NOW_LO, self.now_bits as u32);
        put(OFF_NOW_HI, (self.now_bits >> 32) as u32);
        put(OFF_DEADLINE_LO, self.deadline_bits as u32);
        put(OFF_DEADLINE_HI, (self.deadline_bits >> 32) as u32);
        put(OFF_SUP_FRAME, self.sup_frame);
        put(OFF_SCENE_CUR, self.scene_cur);
        put(OFF_SCENE_REQ, self.scene_req);
        put(OFF_ESI, self.esi);
        put(OFF_TID, self.tid);
        put(OFF_FPUCW, self.fpucw);
        w
    }

    pub fn game_now(&self) -> f64 {
        f64::from_bits(self.now_bits)
    }

    pub fn game_deadline(&self) -> f64 {
        f64::from_bits(self.deadline_bits)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Complete,
    /// まだ書かれていない・書いている途中（札が 0 か、前の周の値）。次に読みに来る。
    NotYet,
    /// 札が seq + 1 より大きい: 読む前に一周して上書きされた（読み落とし = 欠番）。
    Overwritten,
}

/// `t` が `want` より後の周の札か（32 ビットで回り込む番号を、差の符号で比べる。0 は「書いている途中」）。
fn is_later(t: u32, want: u32) -> bool {
    t != 0 && (t.wrapping_sub(want) as i32) > 0
}

/// 番号 `seq` の枠を読んだときの札 2 つ（中身の前 `t1` と後 `t2`）を判定する。どちらも seq + 1 のときだけ採り、札が大きければ読み落とし。
/// seq が 32 ビットを一周する（60 fps で約 2.2 年）と札が 0 になり「途中」と区別できないが、読み取りのスレッドは 60 秒で終わるので扱わない。
pub fn classify(seq: u32, t1: u32, t2: u32) -> SlotState {
    let want = seq.wrapping_add(1);
    if t1 == want && t2 == want {
        SlotState::Complete
    } else if is_later(t1, want) || is_later(t2, want) {
        SlotState::Overwritten
    } else {
        SlotState::NotYet
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotRead {
    Record(Snap),
    NotYet,
    Overwritten,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Collected {
    pub records: Vec<Record>,
    /// 読み落とし（札が大きかった件と、リングバッファを一周以上遅れて読まずに飛ばした件）。
    pub lost: u64,
    /// 採ったが `MAX_RECORDS` を越えて持たなかった件。
    pub overflow: u64,
}

impl Collected {
    pub fn push(&mut self, r: Record) {
        if self.records.len() < MAX_RECORDS {
            self.records.push(r);
        } else {
            self.overflow += 1;
        }
    }
}

/// 間隔の区分の境目（ミリ秒）。60 fps の 1 周期 16.67 ms の前後を細かく、1 フレームに 2 回の近い間隔を 1 ms 未満で分ける。
/// 区分は「境目より小さい」の順で、最後は「最後の境目以上」。
pub const INTERVAL_EDGES_MS: [f64; 8] = [1.0, 8.0, 15.0, 16.0, 17.0, 18.0, 25.0, 40.0];

/// 間隔の統計（マイクロ秒）。番号が隣り合う 2 件の TSC の差だけから取る（読み落としをまたぐ間隔は 2 tick 分になるので入れない）。
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalStats {
    pub count: usize,
    pub min_us: f64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub buckets: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneChange {
    pub seq: u32,
    pub cur: u32,
    pub req: u32,
}

/// 出口の判定の連なり（「番号が 1 ずつ・`+0x454` が 1 ずつ・ウィンドウ `+0x2C` が前の件より進む」が続いた件の列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub len: usize,
    pub first_seq: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TickSummary {
    pub records: usize,
    pub lost: u64,
    pub overflow: u64,
    pub seq_breaks: usize,
    /// スレッド ID・esi・FPU の制御語の種類と件数（値の順）。
    pub tids: Vec<(u32, usize)>,
    pub esis: Vec<(u32, usize)>,
    pub fpucws: Vec<(u32, usize)>,
    pub interval: Option<IntervalStats>,
    /// 番号が隣り合う 2 件のうち、ウィンドウ `+0x2C` のビットが前の件と同じ（= 同じ do_frame の中の 2 回目以降）数。
    pub same_frame: usize,
    /// 番号が隣り合う 2 件の `+0x454` の増え方（符号つき）ごとの件数。
    pub d_frame: Vec<(i64, usize)>,
    pub scenes: Vec<SceneChange>,
    /// TSC で区切った 1 秒ごとの件数（最初の件からの秒。最後の 1 秒は途中まで）。周波数が分からなければ空。
    pub per_second: Vec<u32>,
    pub longest: Option<Run>,
}

impl TickSummary {
    pub fn exit_met(&self) -> bool {
        self.longest.is_some_and(|r| r.len >= EXIT_RUN)
    }
}

/// `+0x454` の増え方（32 ビットで回り込む欄なので、差を符号つきで取る）。
pub fn frame_delta(prev: u32, cur: u32) -> i64 {
    cur.wrapping_sub(prev) as i32 as i64
}

pub fn same_frame(prev: &Record, cur: &Record) -> bool {
    prev.now_bits == cur.now_bits
}

pub fn step_ok(prev: &Record, cur: &Record) -> bool {
    cur.seq.wrapping_sub(prev.seq) == 1
        && frame_delta(prev.sup_frame, cur.sup_frame) == 1
        && cur.game_now() > prev.game_now()
}

/// 最長の連なり（件がなければ `None`）。同じ長さなら先に来た方。
pub fn longest_run(records: &[Record]) -> Option<Run> {
    let first = records.first()?;
    let mut best = Run {
        len: 1,
        first_seq: first.seq,
    };
    let mut cur = best;
    for w in records.windows(2) {
        if step_ok(&w[0], &w[1]) {
            cur.len += 1;
        } else {
            cur = Run {
                len: 1,
                first_seq: w[1].seq,
            };
        }
        if cur.len > best.len {
            best = cur;
        }
    }
    Some(best)
}

/// 近い順位の百分位（小さい順に並べた `sorted` の、ceil(p * n / 100) 番目。1 始まり）。空なら `None`。
pub fn percentile(sorted: &[f64], p: usize) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let rank = (p * n).div_ceil(100).clamp(1, n);
    Some(sorted[rank - 1])
}

pub fn tsc_to_us(ticks: u64, freq_hz: f64) -> f64 {
    ticks as f64 * 1_000_000.0 / freq_hz
}

/// TSC の周波数（Hz）を、始めと終わりの (TSC, QPC) の対と QPC の周波数から求める。
/// どちらかの差が 0 以下・QPC の周波数が 0 以下なら `None`。
pub fn tsc_frequency(start: (u64, i64), end: (u64, i64), qpc_freq: i64) -> Option<f64> {
    let dtsc = end.0.checked_sub(start.0)?;
    let dqpc = end.1.checked_sub(start.1)?;
    if dtsc == 0 || dqpc <= 0 || qpc_freq <= 0 {
        return None;
    }
    Some(dtsc as f64 * qpc_freq as f64 / dqpc as f64)
}

fn bucket_of(us: f64) -> usize {
    let ms = us / 1000.0;
    INTERVAL_EDGES_MS
        .iter()
        .position(|&e| ms < e)
        .unwrap_or(INTERVAL_EDGES_MS.len())
}

fn kinds(values: impl Iterator<Item = u32>) -> Vec<(u32, usize)> {
    let mut m: BTreeMap<u32, usize> = BTreeMap::new();
    for v in values {
        *m.entry(v).or_default() += 1;
    }
    m.into_iter().collect()
}

pub fn summarize(c: &Collected, freq_hz: Option<f64>) -> TickSummary {
    let rs = &c.records;
    let mut seq_breaks = 0;
    let mut same = 0;
    let mut d_frame: BTreeMap<i64, usize> = BTreeMap::new();
    let mut intervals: Vec<f64> = Vec::new();
    for w in rs.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if b.seq.wrapping_sub(a.seq) != 1 {
            seq_breaks += 1;
            continue;
        }
        if same_frame(a, b) {
            same += 1;
        }
        *d_frame
            .entry(frame_delta(a.sup_frame, b.sup_frame))
            .or_default() += 1;
        if let Some(f) = freq_hz {
            intervals.push(tsc_to_us(b.tsc.wrapping_sub(a.tsc), f));
        }
    }

    let interval = freq_hz.and_then(|_| {
        let mut sorted = intervals.clone();
        sorted.sort_by(f64::total_cmp);
        let mut buckets = vec![0usize; INTERVAL_EDGES_MS.len() + 1];
        for &us in &sorted {
            buckets[bucket_of(us)] += 1;
        }
        Some(IntervalStats {
            count: sorted.len(),
            min_us: *sorted.first()?,
            p50_us: percentile(&sorted, 50)?,
            p99_us: percentile(&sorted, 99)?,
            max_us: *sorted.last()?,
            buckets,
        })
    });

    let mut scenes: Vec<SceneChange> = Vec::new();
    for r in rs {
        let changed = scenes
            .last()
            .is_none_or(|s| s.cur != r.scene_cur || s.req != r.scene_req);
        if changed {
            scenes.push(SceneChange {
                seq: r.seq,
                cur: r.scene_cur,
                req: r.scene_req,
            });
        }
    }

    let mut per_second: Vec<u32> = Vec::new();
    if let (Some(f), Some(first)) = (freq_hz, rs.first()) {
        for r in rs {
            let sec = (tsc_to_us(r.tsc.wrapping_sub(first.tsc), f) / 1_000_000.0) as usize;
            if per_second.len() <= sec {
                per_second.resize(sec + 1, 0);
            }
            per_second[sec] += 1;
        }
    }

    TickSummary {
        records: rs.len(),
        lost: c.lost,
        overflow: c.overflow,
        seq_breaks,
        tids: kinds(rs.iter().map(|r| r.tid)),
        esis: kinds(rs.iter().map(|r| r.esi)),
        fpucws: kinds(rs.iter().map(|r| r.fpucw)),
        interval,
        same_frame: same,
        d_frame: d_frame.into_iter().collect(),
        scenes,
        per_second,
        longest: longest_run(rs),
    }
}

pub const SCENE_LINES: usize = 64;

fn join<T>(items: &[T], f: impl Fn(&T) -> String) -> String {
    items.iter().map(f).collect::<Vec<_>>().join(" / ")
}

pub fn write_summary(out: &mut dyn Write, s: &TickSummary) {
    let _ = writeln!(
        out,
        "tick の集計: 取れた {} 件、読み落とし {} 件、番号の飛び {} か所、持ちきれなかった {} 件",
        s.records, s.lost, s.seq_breaks, s.overflow
    );
    let _ = writeln!(
        out,
        "tick の集計: スレッド ID {} 種類（{}）",
        s.tids.len(),
        join(&s.tids, |(v, n)| format!("{v}: {n} 件"))
    );
    let _ = writeln!(
        out,
        "tick の集計: esi {} 種類（{}）",
        s.esis.len(),
        join(&s.esis, |(v, n)| format!("0x{v:08X}: {n} 件"))
    );
    let _ = writeln!(
        out,
        "tick の集計: FPU の制御語 {} 種類（{}）",
        s.fpucws.len(),
        join(&s.fpucws, |(v, n)| format!("0x{v:04X}: {n} 件"))
    );
    match &s.interval {
        None => {
            let _ = writeln!(
                out,
                "tick の集計: 間隔は出せない（TSC の周波数が求まらない、または番号が隣り合う 2 件が無い）"
            );
        }
        Some(i) => {
            let _ = writeln!(
                out,
                "tick の集計: 間隔（番号が隣り合う {} 組。TSC から）最小 {:.1} µs / 中央 {:.1} µs / p99 {:.1} µs / 最大 {:.1} µs",
                i.count, i.min_us, i.p50_us, i.p99_us, i.max_us
            );
            let mut parts = Vec::new();
            for (k, n) in i.buckets.iter().enumerate() {
                let label = match k {
                    0 => format!("{} ms 未満", INTERVAL_EDGES_MS[0]),
                    k if k == INTERVAL_EDGES_MS.len() => {
                        format!("{} ms 以上", INTERVAL_EDGES_MS[k - 1])
                    }
                    k => format!("{}〜{} ms", INTERVAL_EDGES_MS[k - 1], INTERVAL_EDGES_MS[k]),
                };
                parts.push(format!("{label}: {n}"));
            }
            let _ = writeln!(out, "tick の集計: 間隔の区分 {}", parts.join(" / "));
        }
    }
    let _ = writeln!(
        out,
        "tick の集計: 前の件とウィンドウ +0x2C が同じ（同じ do_frame の中の 2 回目以降）{} 件",
        s.same_frame
    );
    let _ = writeln!(
        out,
        "tick の集計: +0x454 の増え方 {}",
        join(&s.d_frame, |(d, n)| format!("{d:+}: {n} 件"))
    );
    let shown = &s.scenes[..s.scenes.len().min(SCENE_LINES)];
    let _ = writeln!(
        out,
        "tick の集計: 場面の移り変わり {} 回（seq: 今の場面 → 次の要求）{}{}",
        s.scenes.len(),
        join(shown, |c| format!(
            "{}: {} → {}",
            c.seq, c.cur as i32, c.req as i32
        )),
        if s.scenes.len() > shown.len() {
            format!(" …ほか {} 件", s.scenes.len() - shown.len())
        } else {
            String::new()
        }
    );
    if s.per_second.is_empty() {
        let _ = writeln!(
            out,
            "tick の集計: TSC で区切った 1 秒ごとの件数は出せない（周波数が求まらないか、件が無い）"
        );
    } else {
        let list: Vec<String> = s.per_second.iter().map(u32::to_string).collect();
        let _ = writeln!(
            out,
            "tick の集計: TSC で区切った 1 秒ごとの件数（最後の 1 秒は途中まで）{}",
            list.join(", ")
        );
    }
    let _ = match s.longest {
        None => writeln!(out, "出口の判定: 件が無い（満たさない）"),
        Some(r) => writeln!(
            out,
            "出口の判定: 番号が 1 ずつ・読み落とし 0・+0x454 が 1 ずつ・ウィンドウ +0x2C が前より進む、が続いた最長 {} 件（seq {} から）。{} 件以上で出口 → {}",
            r.len,
            r.first_seq,
            EXIT_RUN,
            if s.exit_met() {
                "満たす"
            } else {
                "満たさない"
            }
        ),
    };
}

pub const CSV_HEADER: &str = "seq,t_us,dt_us,game_now,game_deadline,sup454,d454,scene_cur,scene_req,esi,tid,fpucw,same_frame";

/// tick ごとの行（先頭の 13 列）を流して作る。1 秒ごとの追記で呼ばれるので、最初の件の TSC と前の件を呼び出しをまたいで持つ。
/// `t_us` は最初の件の TSC からの µs、`dt_us` と `d454` と `same_frame` は前の行（番号が飛んでいても前に採った件）との差。
/// 最初の行は空。周波数が分からなければ時刻の 2 列は空。
#[derive(Debug, Clone, Default)]
pub struct TickCsv {
    pub freq_hz: Option<f64>,
    first_tsc: Option<u64>,
    prev: Option<Record>,
}

impl TickCsv {
    pub fn new(freq_hz: Option<f64>) -> Self {
        TickCsv {
            freq_hz,
            first_tsc: None,
            prev: None,
        }
    }

    pub fn row(&mut self, r: &Record) -> String {
        let first_tsc = *self.first_tsc.get_or_insert(r.tsc);
        let prev = self.prev.as_ref();
        let t = self
            .freq_hz
            .map(|f| format!("{:.3}", tsc_to_us(r.tsc.wrapping_sub(first_tsc), f)))
            .unwrap_or_default();
        let dt = match (prev, self.freq_hz) {
            (Some(p), Some(f)) => format!("{:.3}", tsc_to_us(r.tsc.wrapping_sub(p.tsc), f)),
            _ => String::new(),
        };
        let d454 = prev
            .map(|p| frame_delta(p.sup_frame, r.sup_frame).to_string())
            .unwrap_or_default();
        let same = prev
            .map(|p| if same_frame(p, r) { "1" } else { "0" })
            .unwrap_or_default();
        let line = format!(
            "{},{t},{dt},{:?},{:?},{},{d454},{},{},0x{:08X},{},0x{:04X},{same}",
            r.seq,
            r.game_now(),
            r.game_deadline(),
            r.sup_frame,
            r.scene_cur as i32,
            r.scene_req as i32,
            r.esi,
            r.tid,
            r.fpucw
        );
        self.prev = Some(*r);
        line
    }
}

#[cfg(test)]
pub fn write_csv(
    out: &mut dyn Write,
    records: &[Record],
    freq_hz: Option<f64>,
) -> std::io::Result<()> {
    writeln!(out, "{CSV_HEADER}")?;
    let mut csv = TickCsv::new(freq_hz);
    for r in records {
        writeln!(out, "{}", csv.row(r))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 検査の TSC の周波数（3 GHz）。
    pub(crate) const F: f64 = 3.0e9;
    /// 60 fps の 1 周期の TSC。
    pub(crate) const FRAME_TSC: u64 = 50_000_000;

    pub(crate) fn rec(seq: u32, tsc: u64, now: f64, sup: u32, cur: u32, req: u32) -> Record {
        Record {
            seq,
            tsc,
            now_bits: now.to_bits(),
            deadline_bits: (now + 1.0 / 60.0).to_bits(),
            sup_frame: sup,
            scene_cur: cur,
            scene_req: req,
            esi: 0x0100_0000,
            tid: 7,
            fpucw: 0x007F,
        }
    }

    /// 60 fps の合成: n 件、番号・TSC・今の時刻・+0x454 が 1 周期ずつ進む。場面は 2 のまま。
    pub(crate) fn sixty_fps(n: u32) -> Vec<Record> {
        (0..n)
            .map(|i| {
                rec(
                    i,
                    1_000_000 + i as u64 * FRAME_TSC,
                    10.0 + i as f64 / 60.0,
                    100 + i,
                    2,
                    2,
                )
            })
            .collect()
    }

    fn collected(records: Vec<Record>) -> Collected {
        Collected {
            records,
            lost: 0,
            overflow: 0,
        }
    }

    #[test]
    fn classify_tickets() {
        assert_eq!(classify(10, 11, 11), SlotState::Complete);
        // 中身を読む間に書き始められた（2 回目の札が 0）・書き終わった（1 回目が 0）。
        assert_eq!(classify(10, 11, 0), SlotState::NotYet);
        assert_eq!(classify(10, 0, 11), SlotState::NotYet);
        assert_eq!(
            classify(5000, 5001 - RING_LEN, 5001 - RING_LEN),
            SlotState::NotYet
        );
        // 次の周の札（上書きされた）。1 回目だけ・2 回目だけでも読み落とし。
        assert_eq!(
            classify(10, 11 + RING_LEN, 11 + RING_LEN),
            SlotState::Overwritten
        );
        assert_eq!(classify(10, 11, 11 + RING_LEN), SlotState::Overwritten);
        assert_eq!(classify(10, 11 + RING_LEN, 0), SlotState::Overwritten);
        // 回り込み: seq = u32::MAX - 1 の札は u32::MAX。
        assert_eq!(
            classify(u32::MAX - 1, u32::MAX, u32::MAX),
            SlotState::Complete
        );
        assert_eq!(
            classify(u32::MAX - 1, u32::MAX - 1, u32::MAX - 1),
            SlotState::NotYet
        );
    }

    #[test]
    fn record_from_words_uses_the_fixed_layout() {
        let mut w = [0u32; BODY_WORDS];
        for (i, x) in w.iter_mut().enumerate() {
            *x = 0x1000_0000 * (i as u32 + 1) + i as u32;
        }
        let r = Record::from_words(9, &w);
        assert_eq!(r.tsc, (w[1] as u64) << 32 | w[0] as u64);
        assert_eq!(r.now_bits, (w[3] as u64) << 32 | w[2] as u64);
        assert_eq!(r.deadline_bits, (w[5] as u64) << 32 | w[4] as u64);
        assert_eq!(
            (r.sup_frame, r.scene_cur, r.scene_req, r.esi, r.tid, r.fpucw),
            (w[6], w[7], w[8], w[9], w[10], w[11])
        );
        assert_eq!(r.seq, 9);
        assert_eq!(
            [
                OFF_TSC_LO,
                OFF_NOW_LO,
                OFF_DEADLINE_LO,
                OFF_SUP_FRAME,
                OFF_SCENE_CUR,
                OFF_SCENE_REQ,
                OFF_ESI,
                OFF_TID,
                OFF_FPUCW,
                OFF_TICKET
            ],
            [0, 8, 16, 24, 28, 32, 36, 40, 44, 48]
        );
        assert_eq!(1u32 << SLOT_SHIFT, SLOT_BYTES);
        assert!(RING_LEN.is_power_of_two());
        assert_eq!(SLOT_BYTES, 1024);
        assert_eq!(r.to_words(), w);
    }

    #[test]
    fn snap_reads_bytes_little_endian_at_any_offset() {
        let mut words = Box::new([0u32; SLOT_WORDS]);
        words[16] = 0x4433_2211;
        words[17] = 0x8877_6655;
        words[SLOT_WORDS - 1] = 0xDDCC_BBAA;
        let s = Snap::from_words(1, words);
        assert_eq!(s.byte(64), Some(0x11));
        assert_eq!(s.byte(67), Some(0x44));
        assert_eq!(s.read(66, 2), Some(0x4433));
        assert_eq!(s.read(66, 4), Some(0x6655_4433));
        assert_eq!(s.read(1020, 4), Some(0xDDCC_BBAA));
        assert_eq!(s.read(1021, 4), None);
        assert_eq!(s.byte(1024), None);
        let b = s.bytes();
        assert_eq!(b.len(), 1024);
        assert_eq!(
            &b[64..72],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }

    #[test]
    fn sixty_fps_is_clean_and_meets_the_exit() {
        let s = summarize(&collected(sixty_fps(600)), Some(F));
        assert_eq!(
            (s.records, s.lost, s.seq_breaks, s.same_frame),
            (600, 0, 0, 0)
        );
        assert_eq!(s.d_frame, vec![(1, 599)]);
        assert_eq!(s.tids, vec![(7, 600)]);
        assert_eq!(s.esis, vec![(0x0100_0000, 600)]);
        let i = s.interval.as_ref().unwrap();
        assert_eq!(i.count, 599);
        let frame_us = 1_000_000.0 / 60.0;
        for v in [i.min_us, i.p50_us, i.p99_us, i.max_us] {
            assert!((v - frame_us).abs() < 0.01, "{v}");
        }
        assert_eq!(i.buckets[4], 599);
        assert_eq!(s.per_second, vec![60; 10]);
        assert_eq!(
            s.scenes,
            vec![SceneChange {
                seq: 0,
                cur: 2,
                req: 2
            }]
        );
        assert_eq!(
            s.longest,
            Some(Run {
                len: 600,
                first_seq: 0
            })
        );
        assert!(s.exit_met());
        assert!(!summarize(&collected(sixty_fps(599)), Some(F)).exit_met());
    }

    #[test]
    fn two_calls_in_one_frame() {
        let mut rs = sixty_fps(10);
        let extra = rec(
            3,
            rs[2].tsc + 60_000,
            rs[2].game_now(),
            rs[2].sup_frame,
            2,
            2,
        );
        rs.insert(3, extra);
        // 挟んだ後ろの番号を 1 つずつ送る。+0x454 は元の列のまま（2 回目では増えていないので、次の件で +1）。
        for (i, r) in rs.iter_mut().enumerate().skip(4) {
            r.seq = i as u32;
        }
        let s = summarize(&collected(rs.clone()), Some(F));
        assert_eq!(s.same_frame, 1);
        assert_eq!(s.d_frame, vec![(0, 1), (1, 9)]);
        assert_eq!(s.per_second, vec![11]);
        let i = s.interval.as_ref().unwrap();
        assert!((i.min_us - 20.0).abs() < 0.001, "{}", i.min_us);
        assert_eq!(i.buckets[0], 1);
        // 連なり: seq 0〜2（3 件）で切れ、seq 3〜10（8 件）が最長。
        assert_eq!(
            s.longest,
            Some(Run {
                len: 8,
                first_seq: 3
            })
        );
        let mut csv = Vec::new();
        write_csv(&mut csv, &rs, Some(F)).unwrap();
        let text = String::from_utf8(csv).unwrap();
        let row3 = text.lines().nth(4).unwrap();
        assert!(row3.starts_with("3,"), "{row3}");
        assert!(row3.ends_with(",1"), "同じフレームの印: {row3}");
        assert!(row3.contains(",20.000,"), "dt_us: {row3}");
    }

    #[test]
    fn a_gap_breaks_the_run_and_is_not_an_interval() {
        let mut rs = sixty_fps(20);
        rs.remove(10); // seq 10 を読み落とした
        let c = Collected {
            records: rs,
            lost: 1,
            overflow: 0,
        };
        let s = summarize(&c, Some(F));
        assert_eq!((s.lost, s.seq_breaks), (1, 1));
        assert_eq!(s.interval.as_ref().unwrap().count, 17);
        assert_eq!(s.d_frame, vec![(1, 17)]);
        assert_eq!(
            s.longest,
            Some(Run {
                len: 10,
                first_seq: 0
            })
        );
    }

    #[test]
    fn frame_deltas_and_time_going_backwards() {
        let mut rs = sixty_fps(8);
        // seq 3 で 0 増え。seq 4 は元の値のままなので、seq 3 から 2 増え。以降は 1 ずつ。
        rs[3].sup_frame = rs[2].sup_frame;
        rs[6].now_bits = rs[5].now_bits; // 今の時刻が止まった（+0x454 は 1 増え）
        let s = summarize(&collected(rs), Some(F));
        assert_eq!(s.d_frame, vec![(0, 1), (1, 5), (2, 1)]);
        assert_eq!(s.same_frame, 1);
        // 連なり: 0〜2 / 3 / 4〜5 / 6〜7 → 最長 3（seq 0 から）。
        assert_eq!(
            s.longest,
            Some(Run {
                len: 3,
                first_seq: 0
            })
        );
        assert_eq!(frame_delta(u32::MAX, 0), 1);
        assert_eq!(frame_delta(5, 3), -2);
    }

    #[test]
    fn scene_transitions_are_listed() {
        let mut rs = sixty_fps(6);
        rs[2].scene_req = 7; // 要求が出た
        rs[3].scene_req = 7;
        rs[3].scene_cur = 7; // 次の tick で移った
        rs[4].scene_cur = 7;
        rs[4].scene_req = 7;
        rs[5].scene_cur = 7;
        rs[5].scene_req = 7;
        let s = summarize(&collected(rs), Some(F));
        assert_eq!(
            s.scenes,
            vec![
                SceneChange {
                    seq: 0,
                    cur: 2,
                    req: 2
                },
                SceneChange {
                    seq: 2,
                    cur: 2,
                    req: 7
                },
                SceneChange {
                    seq: 3,
                    cur: 7,
                    req: 7
                },
            ]
        );
        let mut out = Vec::new();
        write_summary(&mut out, &s);
        let log = String::from_utf8(out).unwrap();
        assert!(
            log.contains(
                "場面の移り変わり 3 回（seq: 今の場面 → 次の要求）0: 2 → 2 / 2: 2 → 7 / 3: 7 → 7"
            ),
            "{log}"
        );
    }

    #[test]
    fn percentile_boundaries() {
        let a: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&a, 50), Some(50.0));
        assert_eq!(percentile(&a, 99), Some(99.0));
        assert_eq!(percentile(&a, 100), Some(100.0));
        let b: Vec<f64> = (1..=101).map(f64::from).collect();
        assert_eq!(percentile(&b, 50), Some(51.0));
        assert_eq!(percentile(&b, 99), Some(100.0));
        assert_eq!(percentile(&[3.0], 99), Some(3.0));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn interval_bucket_edges() {
        assert_eq!(bucket_of(999.9), 0);
        assert_eq!(bucket_of(1000.0), 1);
        assert_eq!(bucket_of(15_999.0), 3);
        assert_eq!(bucket_of(16_000.0), 4);
        assert_eq!(bucket_of(16_666.7), 4);
        assert_eq!(bucket_of(40_000.0), 8);
        assert_eq!(bucket_of(1e9), 8);
    }

    #[test]
    fn tsc_frequency_from_two_pairs() {
        // 10 MHz の QPC で 2 秒（20,000,000）、TSC は 6e9 進んだ → 3 GHz。
        let f = tsc_frequency((1_000, 5), (6_000_001_000, 20_000_005), 10_000_000).unwrap();
        assert!((f - 3.0e9).abs() < 1.0, "{f}");
        assert_eq!(tsc_frequency((10, 5), (10, 9), 10), None);
        assert_eq!(tsc_frequency((10, 5), (20, 5), 10), None);
        assert_eq!(tsc_frequency((10, 5), (5, 9), 10), None);
        assert_eq!(tsc_frequency((10, 5), (20, 9), 0), None);
    }

    #[test]
    fn unknown_frequency_and_no_records() {
        let s = summarize(&collected(sixty_fps(5)), None);
        assert_eq!(s.interval, None);
        assert!(s.per_second.is_empty());
        let e = summarize(&Collected::default(), Some(F));
        assert_eq!((e.records, e.longest), (0, None));
        assert!(!e.exit_met());
        assert_eq!(e.interval, None);
        let mut out = Vec::new();
        write_summary(&mut out, &e);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("出口の判定: 件が無い（満たさない）"), "{log}");
        assert!(log.contains("間隔は出せない"), "{log}");
    }

    #[test]
    fn collected_caps_the_records() {
        let mut c = Collected::default();
        let r = rec(0, 0, 0.0, 0, 0, 0);
        for _ in 0..MAX_RECORDS + 3 {
            c.push(r);
        }
        assert_eq!((c.records.len(), c.overflow), (MAX_RECORDS, 3));
    }

    #[test]
    fn csv_rows() {
        let mut rs = sixty_fps(2);
        rs[1].scene_req = u32::MAX; // -1
        let mut out = Vec::new();
        write_csv(&mut out, &rs, Some(F)).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(
            lines[1],
            format!(
                "0,0.000,,10.0,{:?},100,,2,2,0x01000000,7,0x007F,",
                10.0 + 1.0 / 60.0
            )
        );
        assert_eq!(
            lines[2],
            format!(
                "1,16666.667,16666.667,{:?},{:?},101,1,2,-1,0x01000000,7,0x007F,0",
                10.0 + 1.0 / 60.0,
                10.0 + 1.0 / 60.0 + 1.0 / 60.0
            )
        );
        assert_eq!(lines.len(), 3);
        // 周波数が分からなければ時刻の 2 列は空。
        let mut out = Vec::new();
        write_csv(&mut out, &rs, None).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.lines().nth(2).unwrap().starts_with("1,,,"));
    }

    #[test]
    fn summary_log_lines() {
        let s = summarize(&collected(sixty_fps(600)), Some(F));
        let mut out = Vec::new();
        write_summary(&mut out, &s);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("tick の集計: 取れた 600 件、読み落とし 0 件、番号の飛び 0 か所、持ちきれなかった 0 件"), "{log}");
        assert!(
            log.contains("最長 600 件（seq 0 から）。600 件以上で出口 → 満たす"),
            "{log}"
        );
        assert!(log.contains("+0x454 の増え方 +1: 599 件"), "{log}");
        assert!(log.contains("16〜17 ms: 599"), "{log}");
        assert!(log.contains("スレッド ID 1 種類（7: 600 件）"), "{log}");
        assert!(
            log.contains("FPU の制御語 1 種類（0x007F: 600 件）"),
            "{log}"
        );
        assert!(log.contains("TSC で区切った 1 秒ごとの件数（最後の 1 秒は途中まで）60, 60, 60, 60, 60, 60, 60, 60, 60, 60"), "{log}");
    }
}
