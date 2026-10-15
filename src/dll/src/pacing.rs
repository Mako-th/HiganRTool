//! ペース B（先頭で待つだけ）と「入力 → Present」の時刻のフック（pure。unsafe 無し）。
//!
//! 順番と判定と記録のリングバッファだけを持ち、Win32（QPC・待機タイマー・入口のアセンブリ・書き出しのスレッド）は `PacingHost` と `PaceClock` の向こう
//! （本物は `pacing_win.rs`）に置く。
//!
//! - 既定の形: ini が無い・`[pacing]` が無い・`mode` を書かないときは auto（60 fps）。先頭は書き、フレームの最初の先頭のフックで画面の形
//!   （`th09_addr::OPTION_WINDOWED`）を読んで、ウィンドウモードなら b-spin、全画面・読めないならゲーム自身の時計の経路へ任せる。先頭を書かずゲーム自身の待ちのままにするのは `mode = game` と書いたときだけ。
//! - 設置（`install`。読み取りのスレッド）: ini の `[pacing]` を読み、使う行（game なら `present_call`、b なら `PACING_SITES` の 6 行）を全部読んで照合し、
//!   1 行でも違えば何も書かない（vpatch が先頭を書き換えていればここで断る）。書き出しを起こしてから Present の call の rel32 を書き、入力の入口の時刻を開け、
//!   b なら先頭の call の rel32 を書く。どの call も、入口が読む静的変数（元の飛び先・着地）を書く前に置く（`hook::install_call_hook_at` の `prepare`）。
//! - 先頭（`Pacer::head`。ゲームのスレッド）: 期限までの残りが 1 ms より多ければ最大 1 ms 寝て「まだ」を返し（do_frame が 0 を返し、ゲームのメッセージループへ戻る）、
//!   1 ms 以下なら期限まで QPC を読み続けて「始める」を返す。遅れていればすぐ「始める」。
//! - 記録のリングバッファ（`PACE_RING`）: 1 フレーム（Present 1 回）1 枠。書くのはゲームのスレッドだけで、先頭・入力・Present の前後が「いまの番号」の枠に書き、
//!   Present の後に札を立てて番号を進める。読むのは書き出しのスレッド（100 ms ごと）。
//! - 終わり（`finish`）: 書き出しを止め、集計を本ログへ書く。
//! - 切り分けの形: `mode = b-spin`（B の格子のまま、期限の前は寝ずに `Sleep(0)` でメッセージループへ戻す。auto のウィンドウのときの方式）と `mode = cycle`
//!   （game → b → b-spin を `cycle_secs` ごとに入れ替える。game の区間は先頭の入口が `HEAD_FALLBACK` を返してゲーム自身の時計の経路へ任せる）。
//!   どの形でも、先頭・入力・Present の前の論理 CPU と、入力・Present の前のスレッドの周期（QueryThreadCycleTime）を枠に残す。
//! - vpatch（`higanr_core::pacing_vpatch`。測定専用の `mode = vpatch`）: 先頭で「期限 − 準備時間」まで普通の待機タイマーで寝てから始め、Present の直前（`Pacer::before_present`）で期限まで待つ。
//!   遅延の表の「vpatch 相当」の列を測るためのもので、既定の経路は変えない（他の方式では Present の前のフックは何もしない）。
//! - b-hybrid（`higanr_core::pacing_hybrid`）: B の格子のまま、残りが「回し読み 1 ms + 余裕」より多い間は待機タイマーで寝て（1 回最大 1 ms）、余裕の間は
//!   b-spin と同じく `Sleep(0)` で譲り、最後の 1 ms は同じ回し読み。処理の重さを下げるための形で、既定ではない（`mode = b-hybrid`、auto のウィンドウなら `auto_window = b-hybrid`、
//!   cycle なら `cycle_phases` に入れる）。寝る長さと余裕の決め方は core にだけ書く。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering, fence};
use std::io::Write;

use higanr_core::pacing::{
    self as pace, CpuClasses, Cycle, FrameRow, Head, Mode, Pace, PacingIni, Phase, Preamble,
    Report, Screen, Splitter,
};
use higanr_core::pacing_hybrid::{Doze, HybridParams, Wait};
use higanr_core::pacing_vpatch::{self as vpatch, Begin, Vpatch};

use crate::hook::{self, HookEnv};
use crate::log_mode::detail_out;
use crate::sites::{SiteDef, hex_bytes};
use crate::th09_addr::{
    OPTION_WINDOWED, PACING_EARLY_EXIT_SITE, PACING_HEAD_SITE, PACING_RESUME_SITE, PACING_SITES,
    PRESENT_SITE, WINDOW_ADDR, WINDOW_SPAN,
};
use crate::trial_summary::PaceEnd;

/// 最後に QPC を読み続ける幅（µs）。vpatch の SleepType≠0 と同じ 1 ms。高分解能の待機タイマーの起床の遅れがこれを越えるとフレームが遅れて始まる（`_pacing.csv` の `wake_late_us`）。
pub const SPIN_US: u64 = 1_000;
/// 1 回に寝る長さの上限（µs）。寝たら「まだ」でメッセージループへ戻す（寝続けるとメッセージを汲まない）。
pub const CHUNK_US: u32 = 1_000;
/// 本ログの隣の CSV の名前の尻。`.gz` で終わる名前は gzip で書く（`higanr_core::gzlog` が読む）。
pub const CSV_SUFFIX: &str = "_pacing.csv.gz";
pub const WRITER_POLL_MS: u64 = 100;
/// 書き出しのスレッドを止める上限（秒。開発用と配るバージョンの両方）= 12 時間。`finish` が呼ばれなかった実行でもスレッドが止まる安全網で、ここで止まると集計も止まる。
/// 台本の実行（20 分）より長いのは、人が遊ぶネットワークの実行に上限が無いため。長くしても集計のメモリは伸びない（列の標本は `higanr_core::pacing::SERIES_CAP` で頭打ち）。
pub const WRITER_THREAD_MAX_SECONDS: u64 = 12 * 60 * 60;
/// 開発用で CSV に行を書く上限（秒）。台本の実行（`observe::SCRIPT_MAX_SECONDS`）に 60 秒足した値。
pub const WRITER_CSV_MAX_SECONDS_DEV: u64 = crate::observe::SCRIPT_MAX_SECONDS as u64 + 60;
/// 配るバージョンで本ログの隣の CSV に行を書く上限（秒）。起動から 10 分（600 + 60 秒）。1 起動の `_pacing.csv` が 12〜15 MB になり大きすぎたため。
/// この上限を越えてもリングバッファは読み続けて集計に入れる（書かなかった行は `WriterTally::csv_capped`）。
pub const WRITER_CSV_MAX_SECONDS_DIST: u64 = 600 + 60;

/// 書き出しの上限（秒）。`csv_seconds` は CSV に行を書く上限、`thread_seconds` はスレッドを止める上限（集計もここで止まる）。
/// 分けるのは、配るバージョンでファイルを軽くしつつ、起動から 11 分より後のマッチの分布を本ログに残すため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterLimits {
    pub csv_seconds: u64,
    pub thread_seconds: u64,
}

/// このバージョンの書き出しの上限。`dist` を引数で受けるのは、配るバージョンの値を開発用のビルドの検査でも見るため。
pub const fn writer_limits(dist: bool) -> WriterLimits {
    WriterLimits {
        csv_seconds: if dist {
            WRITER_CSV_MAX_SECONDS_DIST
        } else {
            WRITER_CSV_MAX_SECONDS_DEV
        },
        thread_seconds: WRITER_THREAD_MAX_SECONDS,
    }
}

/// 先頭の入口への答え（`pacing_win::pacing_head_entry` が分かれる値）。0 = ゲーム自身の経路（元の飛び先の時計へ）。
pub const HEAD_FALLBACK: u32 = 0;
pub const HEAD_RUN: u32 = 1;
pub const HEAD_NOT_YET: u32 = 2;

/// 入力の入口（`stub::input_wait_entry`）が時刻を打つか。0 の間、入口のこの経路の命令は `cmp` と `je` の 2 つだけ。1 にするのは `install` が Present の call を書いた後だけ。
pub static INPUT_STAMP_ON: AtomicU32 = AtomicU32::new(0);

// ---- 格子の寄せ ----
/// 寄せた枠として数える最小の寄せ幅（ppm）。`pacing_sum` も同じ値で数える。
pub const GLIDE_COUNT_PPM: u32 = pace::GLIDE_COUNT_PPM;
const GLIDE_DEFAULT_FRAME_US: u32 = 1_000_000 / pace::DEFAULT_FPS as u32;

/// rollback の時刻合わせが決めた「次の枠の長さの変化」を、先頭（ペース B の格子）へ渡す変数と、寄せの数え。書くのも読むのもゲームのスレッドだけなので順序は `Relaxed` で足りる。確保しない・待たない。
pub struct GlideCell {
    req: AtomicI32,
    frame_us: AtomicU32,
    req_frames: AtomicU32,
    gate_open: AtomicU32,
    sum_ppm: AtomicI64,
    max_abs_steady: AtomicU32,
    nonzero: AtomicU32,
    counted: AtomicU32,
    max_abs_ppm: AtomicU32,
    abs_ticks: AtomicU64,
    net_ticks: AtomicI64,
}

impl GlideCell {
    const fn new() -> GlideCell {
        GlideCell {
            req: AtomicI32::new(0),
            frame_us: AtomicU32::new(0),
            req_frames: AtomicU32::new(0),
            gate_open: AtomicU32::new(0),
            sum_ppm: AtomicI64::new(0),
            max_abs_steady: AtomicU32::new(0),
            nonzero: AtomicU32::new(0),
            counted: AtomicU32::new(0),
            max_abs_ppm: AtomicU32::new(0),
            abs_ticks: AtomicU64::new(0),
            net_ticks: AtomicI64::new(0),
        }
    }

    pub fn request(&self, ppm: i32) {
        self.req.store(ppm, Ordering::Relaxed);
    }

    pub fn note_request(&self, ppm: i32, steady: bool, open: bool) {
        self.req_frames.fetch_add(1, Ordering::Relaxed);
        if open {
            self.gate_open.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_ppm.fetch_add(i64::from(ppm), Ordering::Relaxed);
        if steady {
            self.max_abs_steady
                .fetch_max(ppm.unsigned_abs(), Ordering::Relaxed);
        }
    }

    fn take(&self) -> i32 {
        self.req.swap(0, Ordering::Relaxed)
    }

    pub fn frame_us(&self) -> u32 {
        match self.frame_us.load(Ordering::Relaxed) {
            0 => GLIDE_DEFAULT_FRAME_US,
            us => us,
        }
    }

    fn set_frame_us(&self, us: u32) {
        self.frame_us.store(us, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        self.req.store(0, Ordering::Relaxed);
        self.req_frames.store(0, Ordering::Relaxed);
        self.gate_open.store(0, Ordering::Relaxed);
        self.sum_ppm.store(0, Ordering::Relaxed);
        self.max_abs_steady.store(0, Ordering::Relaxed);
        self.nonzero.store(0, Ordering::Relaxed);
        self.counted.store(0, Ordering::Relaxed);
        self.max_abs_ppm.store(0, Ordering::Relaxed);
        self.abs_ticks.store(0, Ordering::Relaxed);
        self.net_ticks.store(0, Ordering::Relaxed);
    }

    fn note_applied(&self, ppm: i32, ticks: i64) {
        if ppm == 0 {
            return;
        }
        self.nonzero.fetch_add(1, Ordering::Relaxed);
        let a = ppm.unsigned_abs();
        if a >= GLIDE_COUNT_PPM {
            self.counted.fetch_add(1, Ordering::Relaxed);
        }
        self.max_abs_ppm.fetch_max(a, Ordering::Relaxed);
        self.abs_ticks
            .fetch_add(ticks.unsigned_abs(), Ordering::Relaxed);
        self.net_ticks.fetch_add(ticks, Ordering::Relaxed);
    }

    pub fn line(&self, qpf: u64) -> String {
        let max = self.max_abs_ppm.load(Ordering::Relaxed);
        let abs_us = pace::ticks_to_us_floor(self.abs_ticks.load(Ordering::Relaxed), qpf);
        let net = self.net_ticks.load(Ordering::Relaxed);
        let net_us = pace::ticks_to_us_floor(net.unsigned_abs(), qpf) as i64 * net.signum();
        let req = self.req_frames.load(Ordering::Relaxed);
        let open = self.gate_open.load(Ordering::Relaxed);
        let mean = self.sum_ppm.load(Ordering::Relaxed) as f64 / f64::from(req.max(1));
        format!(
            "ペース: 格子の寄せ（rollback の時刻合わせ。正 = 伸ばす）: 要求 {req} 枠のうち往復の判定が開いていた {open} 枠（{:.1}%）・要求の平均 {mean:+.0} ppm・seg の先頭 {} フレーム以降の要求の最大 {} ppm。先頭が適用した枠のうち 寄せた枠 {} 回（寄せ幅 {} ppm 以上。寄せ幅が 0 でなかった枠は {} 回）・寄せ幅の最大 {} ppm（1 枠 {} µs）・寄せた量の絶対値の合計 {:.1} ms・正味 {:+.1} ms",
            100.0 * f64::from(open) / f64::from(req.max(1)),
            higanr_core::rollback::SYNC_HEAD_FRAMES,
            self.max_abs_steady.load(Ordering::Relaxed),
            self.counted.load(Ordering::Relaxed),
            GLIDE_COUNT_PPM,
            self.nonzero.load(Ordering::Relaxed),
            max,
            u64::from(max) * u64::from(self.frame_us()) / 1_000_000,
            abs_us as f64 / 1_000.0,
            net_us as f64 / 1_000.0
        )
    }
}

/// 格子を寄せる要求の置き場（ひとつだけ。ゲームの 1 つのマッチの経路が使う）。じかに使わず `glide()` を通す。
#[cfg(not(test))]
static GLIDE: GlideCell = GlideCell::new();

/// 格子を寄せる要求の置き場。製品では 1 つの静的変数（書き手 `rollback_scan` と読み手 `Pacer::head` は同じゲームのスレッド）。
/// 検査ではスレッドごとの変数にする: cargo test は検査を並列のスレッドで走らせ、rollback の検査が書いた要求を別の検査の先頭が読んで期限を動かしうるため。
#[cfg(not(test))]
pub fn glide() -> &'static GlideCell {
    &GLIDE
}

#[cfg(test)]
pub fn glide() -> &'static GlideCell {
    thread_local! {
        static T_GLIDE: &'static GlideCell = Box::leak(Box::new(GlideCell::new()));
    }
    T_GLIDE.with(|g| *g)
}

/// リングバッファの件数（60 fps で約 68 秒ぶん。書き出しは 100 ms ごとに読むので、周回で失うのは書き出しが 1 分止まったときだけ）。
pub const PACE_RING_LEN: u32 = 4_096;
const PACE_RING_MASK: u32 = PACE_RING_LEN - 1;
const _: () = assert!(PACE_RING_LEN.is_power_of_two());
/// 1 枠の語の数（128 バイト。32 語を全部使う）。
const SLOT_WORDS: usize = 32;

// 枠の中の語の位置。64 ビットの時刻は下位・上位の 2 語。
const W_TICKET: usize = 0;
const W_FRAME: usize = 1;
const W_FLAGS: usize = 2;
const W_HEAD_CALLS: usize = 3;
const W_SLEEPS: usize = 4;
const W_SKIPPED: usize = 5;
const W_INPUTS: usize = 6;
const W_SUP_FRAME: usize = 7;
const W_DEADLINE: usize = 8;
const W_FIRST_HEAD: usize = 10;
const W_START: usize = 12;
const W_INPUT: usize = 14;
const W_PRESENT_BEGIN: usize = 16;
const W_PRESENT_END: usize = 18;
// 以下、CPU の番号は「平らな番号 + 1」（0 は「無い」）。
const W_PHASE: usize = 20;
const W_HEAD_CPU: usize = 21;
const W_INPUT_CPU: usize = 22;
const W_PRESENT_CPU: usize = 23;
const W_INPUT_CYCLES: usize = 24;
const W_PRESENT_CYCLES: usize = 26;
// 格子の寄せの語。どちらも符号つき 32 ビット（要求は ppm、動かした量は QPC のカウント）。
const W_GLIDE_REQ: usize = 28;
const W_GLIDE_MOVED: usize = 29;
// b-hybrid の語。余裕は µs（0 は判断が無い）、寝過ごしは「µs + 1」（0 は寝ていない）。どちらも枠の中の最大。
const W_HYBRID_MARGIN: usize = 30;
const W_OVERSLEEP: usize = 31;
const W_USED: usize = 32;
const _: () = assert!(W_USED <= SLOT_WORDS);

const F_PACED: u32 = 1;
const F_LATE: u32 = 2;
const F_REBASED: u32 = 4;

#[repr(C, align(64))]
pub struct PaceSlot {
    w: [AtomicU32; SLOT_WORDS],
}

/// 記録のリングバッファ。書くのはゲームのスレッドだけ（入力の入口・先頭・Present の入口はどれもゲームのスレッドで走る）。
pub struct PaceRing {
    slots: [PaceSlot; PACE_RING_LEN as usize],
    cur: AtomicU32,
}

pub static PACE_RING: PaceRing = PaceRing {
    slots: [const {
        PaceSlot {
            w: [const { AtomicU32::new(0) }; SLOT_WORDS],
        }
    }; PACE_RING_LEN as usize],
    cur: AtomicU32::new(0),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceRead {
    Row(FrameRow),
    NotYet,
    /// 周回で上書きされた（か、上書きの途中）。
    Lapped,
}

impl PaceRing {
    fn slot(&self, n: u32) -> &PaceSlot {
        &self.slots[(n & PACE_RING_MASK) as usize]
    }

    fn cur_slot(&self) -> &PaceSlot {
        self.slot(self.cur.load(Ordering::Relaxed))
    }

    fn put64(s: &PaceSlot, i: usize, v: u64) {
        s.w[i].store(v as u32, Ordering::Relaxed);
        s.w[i + 1].store((v >> 32) as u32, Ordering::Relaxed);
    }

    fn get64(w: &[u32; SLOT_WORDS], i: usize) -> u64 {
        w[i] as u64 | (w[i + 1] as u64) << 32
    }

    fn bump(s: &PaceSlot, i: usize) {
        let v = s.w[i].load(Ordering::Relaxed);
        s.w[i].store(v.wrapping_add(1), Ordering::Relaxed);
    }

    pub fn completed(&self) -> u32 {
        self.cur.load(Ordering::Acquire)
    }

    pub fn on_phase(&self, phase: Phase) {
        self.cur_slot().w[W_PHASE].store(phase.code(), Ordering::Relaxed);
    }

    /// 先頭が読んだ格子の寄せの要求（ppm）と、実際に動かしたカウント。要求が 0 でないときだけ呼ぶ（ゲーム自身の区間は寝て戻る先頭ごとに読むので、0 で上書きして前に読んだ要求を消さない）。
    pub fn on_glide(&self, req_ppm: i32, moved_ticks: i64) {
        let s = self.cur_slot();
        s.w[W_GLIDE_REQ].store(req_ppm as u32, Ordering::Relaxed);
        let m = moved_ticks.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        s.w[W_GLIDE_MOVED].store(m as u32, Ordering::Relaxed);
    }

    pub fn on_head_cpu(&self, cpu: u32) {
        self.cur_slot().w[W_HEAD_CPU].store(cpu.wrapping_add(1), Ordering::Relaxed);
    }

    pub fn on_head_pass(&self, now: u64) {
        let s = self.cur_slot();
        if s.w[W_HEAD_CALLS].load(Ordering::Relaxed) == 0 {
            Self::put64(s, W_FIRST_HEAD, now);
        }
        Self::bump(s, W_HEAD_CALLS);
    }

    pub fn on_head(&self, now: u64, deadline: u64) {
        let s = self.cur_slot();
        if s.w[W_HEAD_CALLS].load(Ordering::Relaxed) == 0 {
            Self::put64(s, W_FIRST_HEAD, now);
        }
        Self::bump(s, W_HEAD_CALLS);
        Self::put64(s, W_DEADLINE, deadline);
        let f = s.w[W_FLAGS].load(Ordering::Relaxed);
        s.w[W_FLAGS].store(f | F_PACED, Ordering::Relaxed);
    }

    pub fn on_sleep(&self) {
        Self::bump(self.cur_slot(), W_SLEEPS);
    }

    /// b-hybrid の先頭が寝るか譲るかを決めたときの余裕（µs）。枠の中の最大を残す。
    pub fn on_hybrid_margin(&self, us: u32) {
        let w = &self.cur_slot().w[W_HYBRID_MARGIN];
        if us > w.load(Ordering::Relaxed) {
            w.store(us, Ordering::Relaxed);
        }
    }

    pub fn on_oversleep(&self, us: u32) {
        let w = &self.cur_slot().w[W_OVERSLEEP];
        let v = us.saturating_add(1);
        if v > w.load(Ordering::Relaxed) {
            w.store(v, Ordering::Relaxed);
        }
    }

    pub fn on_start(&self, start: u64, late: bool, rebased: bool, skipped: u64) {
        let s = self.cur_slot();
        Self::put64(s, W_START, start);
        let mut f = s.w[W_FLAGS].load(Ordering::Relaxed);
        if late {
            f |= F_LATE;
        }
        if rebased {
            f |= F_REBASED;
        }
        s.w[W_FLAGS].store(f, Ordering::Relaxed);
        s.w[W_SKIPPED].store(skipped.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    }

    pub fn on_input(&self, now: u64, sup_frame: u32, cpu: u32, cycles: u64) {
        let s = self.cur_slot();
        if s.w[W_INPUTS].load(Ordering::Relaxed) == 0 {
            Self::put64(s, W_INPUT, now);
            s.w[W_SUP_FRAME].store(sup_frame, Ordering::Relaxed);
            s.w[W_INPUT_CPU].store(cpu.wrapping_add(1), Ordering::Relaxed);
            Self::put64(s, W_INPUT_CYCLES, cycles);
        }
        Self::bump(s, W_INPUTS);
    }

    pub fn on_present_begin(&self, now: u64, cpu: u32, cycles: u64) {
        let s = self.cur_slot();
        Self::put64(s, W_PRESENT_BEGIN, now);
        s.w[W_PRESENT_CPU].store(cpu.wrapping_add(1), Ordering::Relaxed);
        Self::put64(s, W_PRESENT_CYCLES, cycles);
    }

    /// Present から戻った。札を立て（それまでの書きより後に見える）、番号を進め、次の枠を空にする（札を 0 にしてから中身を消す）。
    pub fn on_present_end(&self, now: u64) {
        let n = self.cur.load(Ordering::Relaxed);
        let s = self.slot(n);
        Self::put64(s, W_PRESENT_END, now);
        s.w[W_TICKET].store(n.wrapping_add(1), Ordering::Release);
        let next = n.wrapping_add(1);
        let t = self.slot(next);
        t.w[W_TICKET].store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        for (i, w) in t.w.iter().enumerate().take(W_USED).skip(1) {
            w.store(if i == W_FRAME { next } else { 0 }, Ordering::Relaxed);
        }
        self.cur.store(next, Ordering::Release);
    }

    pub fn read(&self, n: u32) -> PaceRead {
        let s = self.slot(n);
        let want = n.wrapping_add(1);
        let t1 = s.w[W_TICKET].load(Ordering::Acquire);
        if t1 != want {
            return if n >= self.completed() {
                PaceRead::NotYet
            } else {
                PaceRead::Lapped
            };
        }
        let mut w = [0u32; SLOT_WORDS];
        for (x, a) in w.iter_mut().zip(s.w.iter()) {
            *x = a.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        if s.w[W_TICKET].load(Ordering::Relaxed) != want {
            return PaceRead::Lapped;
        }
        let f = w[W_FLAGS];
        let cpu = |i: usize| w[i].checked_sub(1);
        PaceRead::Row(FrameRow {
            frame: w[W_FRAME],
            paced: f & F_PACED != 0,
            deadline: Self::get64(&w, W_DEADLINE),
            first_head: Self::get64(&w, W_FIRST_HEAD),
            head_calls: w[W_HEAD_CALLS],
            sleeps: w[W_SLEEPS],
            start: Self::get64(&w, W_START),
            late: f & F_LATE != 0,
            rebased: f & F_REBASED != 0,
            skipped: w[W_SKIPPED],
            inputs: w[W_INPUTS],
            sup_frame: w[W_SUP_FRAME],
            input: Self::get64(&w, W_INPUT),
            present_begin: Self::get64(&w, W_PRESENT_BEGIN),
            present_end: Self::get64(&w, W_PRESENT_END),
            phase: Phase::from_code(w[W_PHASE]),
            head_cpu: cpu(W_HEAD_CPU),
            input_cpu: cpu(W_INPUT_CPU),
            present_cpu: cpu(W_PRESENT_CPU),
            head_eff: None,
            input_eff: None,
            present_eff: None,
            input_cycles: Self::get64(&w, W_INPUT_CYCLES),
            present_cycles: Self::get64(&w, W_PRESENT_CYCLES),
            glide_req_ppm: w[W_GLIDE_REQ] as i32,
            glide_moved: i64::from(w[W_GLIDE_MOVED] as i32),
            hybrid_margin_us: w[W_HYBRID_MARGIN],
            oversleep_us: w[W_OVERSLEEP].checked_sub(1),
        })
    }

    pub fn reset(&self) {
        for s in &self.slots {
            for w in &s.w {
                w.store(0, Ordering::Relaxed);
            }
        }
        self.cur.store(0, Ordering::Release);
    }
}

pub trait PaceClock {
    fn now(&mut self) -> u64;
    fn sleep_us(&mut self, us: u32);
    /// 普通の（高分解能でない）待機タイマーで最大 `us` µs 寝る。`mode = vpatch` だけが使う。
    fn sleep_plain_us(&mut self, us: u32);
    fn yield_now(&mut self);
    fn cpu(&mut self) -> u32;
    /// 花映塚の画面の形（`mode = auto` がフレームの最初の先頭のフックで 1 回だけ読む）。本物は `OPTION_WINDOWED` の 1 バイトを読むだけ。設置で読めると確かめられなかったときは読まずに `Screen::Unknown`。
    fn screen(&mut self) -> Screen;
}

const SPIN_GUARD: u32 = 20_000_000;

pub struct Pacer {
    pace: Pace,
    fresh: Pace,
    spin: u64,
    cycle: Option<Cycle>,
    auto: bool,
    auto_window: Phase,
    phase: Phase,
    frame: Option<u32>,
    /// b-hybrid の余裕の見積もりと b-spin へ戻す応答（寝る長さを決める。数値は ini の `hybrid_*`）。
    doze: Doze,
    /// この枠で b-hybrid の先頭が 1 回以上寝たか（遅れて始めた枠が「寝た後の遅れ」かを決める）。
    slept: bool,
    /// `mode = vpatch` の期限と準備時間（他の方式では None）。
    vp: Option<Vpatch>,
}

impl Pacer {
    pub fn new(qpf: u64, cfg: &PacingIni) -> Option<Pacer> {
        let pace = Pace::new(qpf, cfg.fps)?;
        let (phase, cycle, auto) = match cfg.mode {
            Mode::Game => return None,
            Mode::B => (Phase::B, None, false),
            Mode::BSpin => (Phase::BSpin, None, false),
            Mode::BHybrid => (Phase::BHybrid, None, false),
            Mode::Vpatch => (Phase::Vpatch, None, false),
            Mode::Cycle => {
                let c = Cycle::with_phases(qpf, cfg.cycle_secs, cfg.cycle_phases)?;
                (c.phase(), Some(c), false)
            }
            Mode::Auto => (Phase::Game, None, true),
        };
        glide().reset();
        glide().set_frame_us((1_000_000 / cfg.fps.max(1)) as u32);
        Some(Pacer {
            fresh: pace.clone(),
            pace,
            spin: pace::us_to_ticks_ceil(SPIN_US, qpf),
            cycle,
            auto,
            auto_window: cfg.auto_window,
            phase,
            frame: None,
            doze: Doze::with_params(qpf, &cfg.hybrid),
            slept: false,
            vp: if cfg.mode == Mode::Vpatch {
                Vpatch::new(qpf, cfg.fps)
            } else {
                None
            },
        })
    }

    #[cfg(test)]
    pub fn pace(&self) -> &Pace {
        &self.pace
    }

    /// フレームの最初の先頭のフックで方式を決め、枠に札を書く。cycle・auto で game から b / b-spin へ移るときは格子を置き直す（game の区間の間に B の格子点は何百も過ぎているので、
    /// 置き直さないと最初のフレームが「遅れて始めた」になり飛ばした格子点を数え続ける）。b → b-spin は同じ格子のまま。auto の画面の形もここ（フレームに 1 回）でだけ読み、1 フレームの途中で方式を変えない。
    fn phase_for_frame(&mut self, now: u64, clock: &mut dyn PaceClock, ring: &PaceRing) -> bool {
        let n = ring.completed();
        if self.frame == Some(n) {
            return false;
        }
        self.frame = Some(n);
        let prev = self.phase;
        if let Some(c) = self.cycle.as_mut() {
            self.phase = c.at_frame(now).0;
        } else if self.auto {
            self.phase = pace::auto_phase_with(clock.screen(), self.auto_window);
        }
        // b-hybrid で寝た後に期限を越える枠が続いて b-spin へ戻した後は、b-hybrid の枠を b-spin として走らせ、札も b-spin にする。
        if self.phase == Phase::BHybrid && self.doze.fell_back() {
            self.phase = Phase::BSpin;
        }
        if prev == Phase::Game && self.phase != Phase::Game {
            self.pace = self.fresh.clone();
        }
        self.doze.on_frame();
        self.slept = false;
        ring.on_phase(self.phase);
        true
    }

    /// 先頭へ来たときの判断。`HEAD_RUN`・`HEAD_NOT_YET`（寝た・譲った後）・`HEAD_FALLBACK`（cycle の game の区間・auto の全画面のフレーム）を返し、枠に書く。
    pub fn head(&mut self, clock: &mut dyn PaceClock, ring: &PaceRing) -> u32 {
        let now = clock.now();
        let first = self.phase_for_frame(now, clock, ring);
        if self.phase == Phase::Game {
            // ゲーム自身の経路は期限の前なら `Sleep(0)` の後に do_frame から 0 を返してまた先頭へ来る。どの 1 回がフレームを始めるかは分からないので、論理 CPU は来るたびに置き直す。
            ring.on_head_pass(now);
            ring.on_head_cpu(clock.cpu());
            // 格子を持たない区間（全画面）は寄せられないので、溜めずに捨てる。
            let dropped = glide().take();
            if dropped != 0 {
                ring.on_glide(dropped, 0);
            }
            return HEAD_FALLBACK;
        }
        if self.phase == Phase::Vpatch {
            return self.head_vpatch(now, clock, ring);
        }
        // 格子を寄せる（rollback の時刻合わせ）: これから始める枠の期限を、前の枠の入力の地点が決めた分だけ前後させる。期限を決める（`decide`）より前、枠の最初の先頭のフックに 1 回だけ
        // （寝て戻る先頭で動かすと同じ枠の期限が先頭の数だけずれる）。変わるのは期限だけで、入力を読む順は変わらない。
        if first {
            let ppm = glide().take();
            // 格子がまだ無い枠（全画面からウィンドウモードへ移った最初の枠など）は動かせないので、寄せた枠に数えない。
            let mut moved = 0;
            if ppm != 0 && self.pace.grid().is_some() {
                moved = self.pace.nudge(ppm);
                glide().note_applied(ppm, moved);
            }
            if ppm != 0 {
                ring.on_glide(ppm, moved);
            }
        }
        let rebased0 = self.pace.rebased();
        // 最後の回し読みの幅: b-hybrid の枠は ini の `hybrid_spin_us`、b / b-spin は `SPIN_US`。
        let spin = if self.phase == Phase::BHybrid {
            self.doze.spin()
        } else {
            self.spin
        };
        let decision = self.pace.decide(now, spin, CHUNK_US);
        let rebased = self.pace.rebased() != rebased0;
        match decision {
            Head::Sleep { deadline, us } => {
                ring.on_head(now, deadline);
                match self.phase {
                    Phase::BSpin => clock.yield_now(),
                    Phase::BHybrid => {
                        // 寝るか譲るかと寝る長さは core（`Doze::choose`）。`Head::Sleep` は残りが回し読みの幅より大きいときだけ来る。
                        ring.on_hybrid_margin(self.doze.margin_us());
                        let rest = deadline.saturating_sub(now).saturating_sub(spin);
                        match self.doze.choose(rest) {
                            Wait::Sleep { us } => {
                                ring.on_sleep();
                                self.slept = true;
                                clock.sleep_us(us);
                                let over = self.doze.woke(us, clock.now().saturating_sub(now));
                                ring.on_oversleep(self.over_us(over));
                            }
                            Wait::Yield => clock.yield_now(),
                        }
                    }
                    // b（と、ここへは来ない game と vpatch）。寝過ごしは測らない（測るための QPC の読みを足さない）。
                    Phase::B | Phase::Game | Phase::Vpatch => {
                        ring.on_sleep();
                        clock.sleep_us(us);
                    }
                }
                HEAD_NOT_YET
            }
            Head::Spin { deadline } => {
                ring.on_head(now, deadline);
                let mut t = clock.now();
                let mut guard = 0u32;
                while t < deadline && guard < SPIN_GUARD {
                    core::hint::spin_loop();
                    t = clock.now();
                    guard += 1;
                }
                self.start(t, false, rebased, clock, ring)
            }
            Head::Run { deadline, late } => {
                ring.on_head(now, deadline);
                // 寝た後に期限を越えた枠を数える（数が ini の `hybrid_fallback_*` に達したら、次の枠から b-spin へ戻す）。
                if late && self.slept && self.phase == Phase::BHybrid {
                    self.doze.late_after_sleep();
                }
                self.start(now, late, rebased, clock, ring)
            }
        }
    }

    /// vpatch 相当の先頭。起きる時刻（期限 − 準備時間）まで、普通の待機タイマーで目標の `SPIN_US` 手前まで寝て、残りは QPC を読み続け、1 回で「始める」を返す
    /// （メッセージループへは戻さない。vpatch も先頭のフックの中で待つ）。期限を過ぎていれば待たずに始める（基点の置き直しは `Vpatch::begin`）。格子の寄せは受けない。
    fn head_vpatch(&mut self, now: u64, clock: &mut dyn PaceClock, ring: &PaceRing) -> u32 {
        let dropped = glide().take();
        if dropped != 0 {
            ring.on_glide(dropped, 0);
        }
        let (spin, qpf) = (self.spin, self.pace.qpf());
        let Some(vp) = self.vp.as_mut() else {
            return HEAD_FALLBACK;
        };
        vp.sample(now);
        let seen = vp.rebased();
        match vp.begin(now) {
            Begin::Rebased => {
                vp.woke(now);
                ring.on_head(now, now);
                ring.on_start(now, seen > 0, true, 0);
            }
            Begin::At { wake_at, .. } => {
                ring.on_head(now, wake_at);
                if vp_wait(clock, now, wake_at, spin, qpf) {
                    ring.on_sleep();
                }
                let t = clock.now();
                vp.woke(t);
                ring.on_start(t, now > wake_at, false, 0);
            }
        }
        ring.on_head_cpu(clock.cpu());
        HEAD_RUN
    }

    /// Present の直前（`mode = vpatch` だけ）。標本を足し、期限までの待ちを同じ手順で行う。他の方式では何もしない。
    pub fn before_present(&mut self, clock: &mut dyn PaceClock) {
        let (spin, qpf) = (self.spin, self.pace.qpf());
        let Some(vp) = self.vp.as_mut() else {
            return;
        };
        let now = clock.now();
        if let Some(target) = vp.before_present(now) {
            vp_wait(clock, now, target, spin, qpf);
        }
    }

    fn over_us(&self, over: u64) -> u32 {
        pace::ticks_to_us_floor(over, self.pace.qpf()).min(u64::from(u32::MAX)) as u32
    }

    #[cfg(test)]
    pub fn doze(&self) -> &Doze {
        &self.doze
    }

    fn start(
        &mut self,
        start: u64,
        late: bool,
        rebased: bool,
        clock: &mut dyn PaceClock,
        ring: &PaceRing,
    ) -> u32 {
        let skipped = self.pace.started(start);
        ring.on_start(start, late, rebased, skipped);
        ring.on_head_cpu(clock.cpu());
        HEAD_RUN
    }
}

/// `target` まで待つ（vpatch 相当）。`target − spin` までは普通の待機タイマーで寝て、その後 `target` まで QPC を読み続ける。寝たかを返す。
fn vp_wait(clock: &mut dyn PaceClock, now: u64, target: u64, spin: u64, qpf: u64) -> bool {
    let slept = vpatch::sleep_ticks(now, target, spin);
    if let Some(r) = slept {
        let us = pace::ticks_to_us_floor(r, qpf).clamp(1, u64::from(u32::MAX)) as u32;
        clock.sleep_plain_us(us);
    }
    let mut t = clock.now();
    let mut guard = 0u32;
    while t < target && guard < SPIN_GUARD {
        core::hint::spin_loop();
        t = clock.now();
        guard += 1;
    }
    slept.is_some()
}

#[derive(Debug, Clone)]
pub struct WriterTally {
    pub rows: u64,
    pub lost: u64,
    pub write_failed: bool,
    pub limits: WriterLimits,
    pub csv_capped: u64,
    pub report: Report,
}

/// リングバッファを `stop()` が true になるまで（か `limits.thread_seconds` まで）`WRITER_POLL_MS` ごとに読み、全部の行を集計に入れる。1 フレーム 1 行は `out` に書くが、`limits.csv_seconds` を越えた後は書かず `csv_capped` に数える。
/// 止めた後にもう 1 回読み切ってから返す。`fps` は集計のペース。論理 CPU の効率クラスは `classes`（設置の時に 1 回読んだ表）から、このスレッドで引く。
#[allow(clippy::too_many_arguments)] // 書き出しのスレッドの入口 1 か所から呼ぶだけで、束ねる型を作るほどの口ではない
pub fn writer_loop(
    ring: &PaceRing,
    pre: &Preamble,
    fps: u64,
    classes: &CpuClasses,
    out: &mut dyn Write,
    stop: &dyn Fn() -> bool,
    sleep_ms: &mut dyn FnMut(u64),
    limits: WriterLimits,
) -> WriterTally {
    let qpf = pre.qpf;
    let csv_polls = limits.csv_seconds * 1_000 / WRITER_POLL_MS;
    let max_polls = limits.thread_seconds * 1_000 / WRITER_POLL_MS;
    let mut sum = Splitter::new(qpf, fps, &pre.eff_classes, pre.mode.fixed_phase());
    // 最初の行は `# qpf=… mode=… eff_classes=… cycle_secs=…`（あとから集計する `pacing_sum` が読む）、次が列の名前。
    let mut ok = writeln!(out, "{}", pace::csv_preamble(pre))
        .and_then(|_| writeln!(out, "{}", pace::CSV_HEADER))
        .is_ok();
    let (mut rows, mut lost, mut csv_capped) = (0u64, 0u64, 0u64);
    let mut next = 0u32;
    let mut prev: Option<FrameRow> = None;
    // game の実行は先頭を書かないので札が付かない。行に `game` を入れる。
    let fill_phase = (pre.mode == Mode::Game).then_some(Phase::Game);
    // `write_csv` が false（CSV の上限の後）なら行は書かず集計にだけ入れる。リングバッファは止めるまで読み続ける。
    let mut drain = |ok: &mut bool, sum: &mut Splitter, write_csv: bool| {
        let upto = ring.completed();
        while next != upto {
            match ring.read(next) {
                PaceRead::Row(mut r) => {
                    r.head_eff = r.head_cpu.and_then(|c| classes.class_of(c));
                    r.input_eff = r.input_cpu.and_then(|c| classes.class_of(c));
                    r.present_eff = r.present_cpu.and_then(|c| classes.class_of(c));
                    r.phase = r.phase.or(fill_phase);
                    if !write_csv {
                        csv_capped += 1;
                    } else if *ok {
                        *ok = writeln!(out, "{}", r.csv_line(prev.as_ref(), qpf)).is_ok();
                    }
                    sum.push(&r);
                    prev = Some(r);
                    rows += 1;
                }
                PaceRead::Lapped => lost += 1,
                PaceRead::NotYet => break,
            }
            next = next.wrapping_add(1);
        }
        if write_csv && *ok {
            *ok = out.flush().is_ok();
        }
    };
    let mut polls = 0u64;
    while !stop() && polls < max_polls {
        drain(&mut ok, &mut sum, polls < csv_polls);
        sleep_ms(WRITER_POLL_MS);
        polls += 1;
    }
    drain(&mut ok, &mut sum, polls < csv_polls);
    WriterTally {
        rows,
        lost,
        write_failed: !ok,
        limits,
        csv_capped,
        report: sum.finish(),
    }
}

#[derive(Debug, Clone)]
pub struct WriterEnd {
    pub file: String,
    pub tally: WriterTally,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaceCounters {
    pub fallbacks: u32,
    pub panics: u32,
}

pub trait PacingHost {
    fn read_ini(&mut self) -> Result<Option<String>, String>;
    fn qpf(&self) -> u64;
    fn head_entry(&self) -> u32;
    fn present_entry(&self) -> u32;
    /// B の待ち（高分解能の待機タイマー）と先頭のフック（形・ペース・cycle の秒は `cfg`）を用意する。作れなければ Err（先頭は書かない）。`screen_flag` は auto の先頭が読む画面の形のアドレス
    /// （設置で読めると確かめたときだけ `OPTION_WINDOWED`。それ以外は 0 で、先頭は読まずに `Screen::Unknown`）。`window` はゲーム自身の時計の経路で「この先頭の 1 回がフレームを始めるか」を見るウィンドウの構造体のアドレス
    /// （読めると確かめたときだけ `WINDOW_ADDR`。それ以外は 0 で、その経路では先頭のフックの中の待ちを呼ばない）。
    fn arm_pacer(
        &mut self,
        qpf: u64,
        cfg: &PacingIni,
        screen_flag: u32,
        window: u32,
    ) -> Result<String, String>;
    fn cpu_classes(&mut self) -> Result<CpuClasses, String>;
    /// 記録のリングバッファを空にし、書き出しのスレッドを起こす（CSV を開く）。開いたファイルの名前。`fps` は集計のペース。
    fn start_writer(
        &mut self,
        pre: Preamble,
        fps: u64,
        classes: CpuClasses,
    ) -> Result<String, String>;
    fn stop_writer(&mut self) -> Option<Result<WriterEnd, String>>;
    fn set_present_original(&mut self, original: u32);
    fn set_head_targets(&mut self, original: u32, resume: u32, early_exit: u32);
    fn start_stamps(&mut self);
    fn counters(&self) -> PaceCounters;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    Nothing,
    Stamps,
    Paced,
}

fn row(name: &str) -> Option<&'static SiteDef> {
    PACING_SITES.iter().find(|d| d.name == name)
}

fn verify_row(env: &mut dyn HookEnv, d: &SiteDef) -> Result<(), String> {
    let mut buf = vec![0u8; d.expected.len()];
    if !env.read_code(d.addr, &mut buf) {
        return Err(format!("地点 {} 0x{:08X} が読めない", d.name, d.addr));
    }
    if buf != d.expected {
        return Err(format!(
            "地点 {} 0x{:08X} が {} で、期待 {} と違う",
            d.name,
            d.addr,
            hex_bytes(&buf),
            hex_bytes(d.expected)
        ));
    }
    Ok(())
}

/// auto の先頭が読む画面の形のアドレスを、設置の時に 1 バイト読めるかで確かめる（読めなければ 0 を返し、先頭は読まない）。ここで読んだ値は本ログに残すだけで形は決めない:
/// 設置は花映塚の設定の読み込みより前に走りうるので、この時点の 0 は「全画面」とは限らない。
fn checked_screen_flag(env: &mut dyn HookEnv, out: &mut dyn Write) -> u32 {
    let mut b = [0u8; 1];
    if !env.read_code(OPTION_WINDOWED, &mut b) {
        let _ = writeln!(
            out,
            "ペース: 画面の形 0x{OPTION_WINDOWED:08X} が読めない。auto は全フレームをゲーム自身の待ちの経路へ任せる（game と同じ）"
        );
        return 0;
    }
    let _ = writeln!(
        detail_out(out),
        "ペース: 画面の形 0x{OPTION_WINDOWED:08X}（設定の +0xB1）は読める。設置の時の値 {}（起動の途中なら設定を読む前の 0 のことがある。形はフレームごとに読み直す）",
        b[0]
    );
    OPTION_WINDOWED
}

/// ゲーム自身の時計の経路（auto の全画面・cycle の game の区間）で先頭が読むウィンドウの構造体（時計の分母 `+0x14`・期限 `+0x3C`）を、設置の時に読めるかで確かめる
/// （読めなければ 0 を返し、その経路では先頭のフックの中の待ちを呼ばない）。中の値は設置の時には決まっていないことがあるので、読めるかだけを見る。
fn checked_window(env: &mut dyn HookEnv, out: &mut dyn Write) -> u32 {
    let mut b = [0u8; WINDOW_SPAN as usize];
    if !env.read_code(WINDOW_ADDR, &mut b) {
        let _ = writeln!(
            out,
            "ペース: ウィンドウの構造体 0x{WINDOW_ADDR:08X} が読めない。ゲーム自身の時計の経路（全画面）では先頭で相手の入力ワードを待たない"
        );
        return 0;
    }
    WINDOW_ADDR
}

/// ini から形を決める。`Ok((形, 既定になった理由))`、`Err` は本ログへ書く理由（何も触らない）。ini が無い・節が無い・`mode` を書かないときは既定の auto（`pace::DEFAULT_MODE`）。
/// 読めない ini・節の中の誤りは触らない（誤った設定のまま黙って既定を走らせない）。`mode = auto` と明示したときは既定の行を出さない。
fn cfg_from_ini(
    read: Result<Option<String>, String>,
) -> Result<(PacingIni, Option<String>), String> {
    let text = match read {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Ok((
                PacingIni::DEFAULT,
                Some("HiganRTool.ini が無い".to_string()),
            ));
        }
        Err(why) => return Err(format!("HiganRTool.ini を読めない（{why}）")),
    };
    let bad = |e: String| format!("ini の [{}] を読めない（{e}）", pace::SECTION);
    let ini = higanr_core::ini::parse(&text).map_err(|e| bad(e.to_string()))?;
    let cfg = pace::mode_from_ini(&ini).map_err(bad)?;
    let why = if !ini.has_section(pace::SECTION) {
        Some(format!("ini に [{}] が無い", pace::SECTION))
    } else if ini.get(pace::SECTION, "mode").is_none() {
        Some(format!("[{}] に mode が無い", pace::SECTION))
    } else {
        None
    };
    Ok((cfg, why))
}

/// 設置の結果と、求められた形・実際に使う形（試験のまとめの「フレームのペース」の行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaceInstall {
    pub installed: Installed,
    pub requested: Option<Mode>,
    pub used: Option<Mode>,
}

pub fn install(
    env: &mut dyn HookEnv,
    host: &mut dyn PacingHost,
    out: &mut dyn Write,
) -> PaceInstall {
    let mut modes = (None, None);
    let installed = install_modes(env, host, out, &mut modes);
    let used = match installed {
        Installed::Nothing => None,
        Installed::Stamps => Some(Mode::Game),
        Installed::Paced => modes.1,
    };
    PaceInstall {
        installed,
        requested: modes.0,
        used,
    }
}

/// `install` の中身。`modes` には、形が決まったときに 求められた形 と 先頭を用意できた後の形 を置く。
fn install_modes(
    env: &mut dyn HookEnv,
    host: &mut dyn PacingHost,
    out: &mut dyn Write,
    modes: &mut (Option<Mode>, Option<Mode>),
) -> Installed {
    let cfg = match cfg_from_ini(host.read_ini()) {
        Ok((c, None)) => c,
        Ok((c, Some(why))) => {
            // 既定で入ったことを本ログで見分けられるよう、どの経路で既定になったかを 1 行書く。
            let _ = writeln!(
                detail_out(out),
                "ペース: {why}ので既定の mode = {}（{} fps）で置く。ゲーム自身の待ちのままにするなら [{}] に mode = game",
                c.mode.name(),
                c.fps,
                pace::SECTION
            );
            c
        }
        Err(e) => {
            let _ = writeln!(out, "ペース: {e}。触らない");
            return Installed::Nothing;
        }
    };
    let PacingIni {
        mode,
        fps,
        cycle_secs,
        cycle_phases,
        auto_window,
        hybrid,
    } = cfg;
    modes.0 = Some(mode);
    let qpf = host.qpf();
    if !pace::can_tick(qpf, fps) {
        let _ = writeln!(
            out,
            "ペース: QPF {qpf} Hz では {fps} fps を刻めない。触らない"
        );
        return Installed::Nothing;
    }
    let names: &[&str] = match mode {
        Mode::Game => &[PRESENT_SITE],
        Mode::B | Mode::BSpin | Mode::BHybrid | Mode::Vpatch | Mode::Cycle | Mode::Auto => &[
            crate::th09_addr::PACING_ENTRY_SITE,
            PACING_HEAD_SITE,
            crate::th09_addr::PACING_SKIPPED_SITE,
            PACING_RESUME_SITE,
            PACING_EARLY_EXIT_SITE,
            PRESENT_SITE,
        ],
    };
    let mut defs = Vec::new();
    for n in names {
        match row(n) {
            Some(d) => defs.push(d),
            None => {
                let _ = writeln!(out, "ペース: 表に {n} の行が無い。触らない");
                return Installed::Nothing;
            }
        }
    }
    let mut bad = Vec::new();
    for d in &defs {
        if let Err(why) = verify_row(env, d) {
            bad.push(why);
        }
    }
    if !bad.is_empty() {
        let _ = writeln!(
            out,
            "ペース: mode = {} の地点 {} 行のうち {} 行が合わないので、1 バイトも書かない。ゲーム自身の待ちのまま動く（vpatch などが先に書き換えている見込み）: {}",
            mode.name(),
            defs.len(),
            bad.len(),
            bad.join(" / ")
        );
        return Installed::Nothing;
    }
    let _ = writeln!(
        detail_out(out),
        "ペース: mode = {}、地点 {} 行を照合して全部一致（{}）、QPF {qpf} Hz",
        mode.name(),
        defs.len(),
        defs.iter().map(|d| d.name).collect::<Vec<_>>().join(" / ")
    );
    let classes = match host.cpu_classes() {
        Ok(c) if !c.is_empty() => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 効率クラス（GetSystemCpuSetInformation）: {}。大きい方を性能コアと数える",
                c.describe()
            );
            c
        }
        Ok(_) => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 効率クラスの表が 0 件。コアの種類の列は空にする"
            );
            CpuClasses::default()
        }
        Err(why) => {
            let _ = writeln!(
                out,
                "ペース: 効率クラスを読めない（{why}）。コアの種類の列は空にする"
            );
            CpuClasses::default()
        }
    };
    let mut paced = mode.uses_head();
    if paced {
        let screen_flag = if mode == Mode::Auto {
            checked_screen_flag(env, out)
        } else {
            0
        };
        let window = if matches!(mode, Mode::Auto | Mode::Cycle) {
            checked_window(env, out)
        } else {
            0
        };
        match host.arm_pacer(qpf, &cfg, screen_flag, window) {
            Ok(desc) => {
                let _ = writeln!(detail_out(out), "ペース: 先頭の待ちを用意した（{desc}）");
            }
            Err(why) => {
                let _ = writeln!(
                    out,
                    "ペース: 先頭の待ちを用意できない（{why}）。{} は書かず、時刻のフックだけ付ける",
                    mode.name()
                );
                paced = false;
            }
        }
    }
    let used_mode = if paced { mode } else { Mode::Game };
    modes.1 = Some(used_mode);
    // 先頭に入れなかったらゲーム自身のペース（60）で集計する。ini の fps は b / b-spin にしか効かない。
    let used_fps = if paced { fps } else { pace::DEFAULT_FPS };
    let pre = Preamble {
        qpf,
        mode: used_mode,
        eff_classes: classes.distinct(),
        cycle_secs: (used_mode == Mode::Cycle).then_some(cycle_secs),
    };
    match host.start_writer(pre, used_fps, classes) {
        Ok(name) => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 1 フレーム 1 行を {name} へ書く（{} ms ごと）",
                WRITER_POLL_MS
            );
        }
        Err(why) => {
            let _ = writeln!(out, "ペース: 書き出しを起こせない（{why}）。何も書かない");
            return Installed::Nothing;
        }
    }
    let Some(pdef) = row(PRESENT_SITE) else {
        return Installed::Nothing;
    };
    let entry = host.present_entry();
    let r = hook::install_call_hook_at(env, pdef, entry, &mut |_, original| {
        host.set_present_original(original);
    });
    hook::write_report(out, pdef, &r);
    if !r.written() {
        let _ = writeln!(
            out,
            "ペース: Present の call を書けなかった。時刻のフックもペースも付けない"
        );
        return Installed::Nothing;
    }
    host.start_stamps();
    let _ = writeln!(
        detail_out(out),
        "ペース: 時刻のフックを開けた（入力の入口と Present の前後の QPC）"
    );
    if !paced {
        return Installed::Stamps;
    }
    let (Some(hdef), Some(resume), Some(early)) = (
        row(PACING_HEAD_SITE),
        row(PACING_RESUME_SITE),
        row(PACING_EARLY_EXIT_SITE),
    ) else {
        return Installed::Stamps;
    };
    let (resume, early) = (resume.addr, early.addr);
    let entry = host.head_entry();
    let r = hook::install_call_hook_at(env, hdef, entry, &mut |_, original| {
        host.set_head_targets(original, resume, early);
    });
    hook::write_report(out, hdef, &r);
    if !r.written() {
        let _ = writeln!(
            out,
            "ペース: 先頭の call を書けなかった。ゲーム自身のペースのまま（時刻のフックは付いている）"
        );
        return Installed::Stamps;
    }
    // vpatch の先頭のフックは寝てもメッセージループへ戻らない（先頭のフックの中で待つ）ので、B の文を当てない。
    if mode == Mode::Vpatch {
        let _ = writeln!(
            detail_out(out),
            "ペース: vpatch の先頭のフックを入れた（期限は QPC の整数で {fps}.000 fps、メッセージループへは戻さず先頭のフックの中で待つ。始めるときは fldcw 0x{:04X}。着地 0x{resume:08X} / まだ 0x{early:08X}）",
            crate::th09_addr::GAME_FPU_CW
        );
    } else {
        let _ = writeln!(
            detail_out(out),
            "ペース: B を入れた（期限は QPC の整数で {fps}.000 fps、1 回に最大 {CHUNK_US} µs 寝てメッセージループへ戻し、残り {SPIN_US} µs からは QPC を読み続ける。始めるときは fldcw 0x{:04X}。着地 0x{resume:08X} / まだ 0x{early:08X}）",
            crate::th09_addr::GAME_FPU_CW
        );
    }
    match mode {
        Mode::BSpin => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 形は b-spin（B と同じ格子で、残り {SPIN_US} µs より前は寝ずに Sleep(0) で譲ってメッセージループへ戻す。論理コア 1 つを回し続ける）"
            );
        }
        Mode::BHybrid => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 形は b-hybrid（{}）",
                hybrid_text(&hybrid)
            );
        }
        Mode::Auto if auto_window == Phase::BHybrid => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 形は auto（フレームの最初の先頭のフックで画面の形を読み、ウィンドウなら b-hybrid（ini の auto_window）—— {}。全画面・読めないときは先頭の入口が元の call 先（ゲーム自身の時計と垂直同期）の道へ任せる。全画面から窓へ移るときは格子を置き直す）",
                hybrid_text(&hybrid)
            );
        }
        Mode::Auto => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 形は auto（既定。フレームの最初の先頭のフックで画面の形を読み、ウィンドウなら b-spin —— 残り {SPIN_US} µs より前は寝ずに Sleep(0) で譲る。全画面・読めないときは先頭の入口が元の call 先（ゲーム自身の時計と垂直同期）の道へ任せる。全画面から窓へ移るときは格子を置き直す）"
            );
        }
        Mode::Cycle => {
            let first = cycle_phases.as_slice()[0].name();
            let _ = writeln!(
                detail_out(out),
                "ペース: 形は cycle（{cycle_secs} 秒ごとに {} → {first} … と入れ替える。game の区間は先頭の入口が元の call 先（ゲーム自身の時計）の道へ任せる。game から b へ移るときは格子を置き直す。切り分け用）",
                cycle_phases.text()
            );
            if cycle_phases.as_slice().contains(&Phase::BHybrid) {
                let _ = writeln!(
                    detail_out(out),
                    "ペース: cycle の b-hybrid の区間は {}",
                    hybrid_text(&hybrid)
                );
            }
        }
        Mode::Vpatch => {
            let _ = writeln!(
                detail_out(out),
                "ペース: 形は vpatch（測定専用。vpatch の通常のペースの再現: 期限 = 基点 + n × QPF ÷ fps、先頭で「期限 − 準備時間」まで普通の待機タイマーで寝て（目標の {SPIN_US} µs 手前まで）QPC を読み続け、Present の直前で期限まで同じ手順で待つ。準備時間は起きてから Present の直前までの直近 {} 本から {}〜{} ms。期限を過ぎて先頭へ来たら基点を今に置き直す。先頭のフックの中で待つのでメッセージループへは戻らない）",
                vpatch::PREP_WINDOW,
                vpatch::PREP_MIN_MS,
                vpatch::PREP_MAX_MS
            );
        }
        Mode::B | Mode::Game => {}
    }
    if fps != pace::DEFAULT_FPS {
        // 製品の実行で誤って使ったときに本ログで見分けられるよう、既定と違えば必ず 1 行足す。
        let _ = writeln!(
            out,
            "ペース: ★検査用: fps = {fps}（ゲームが {fps}/{} 倍で進む。普段の対戦では使わない）",
            pace::DEFAULT_FPS
        );
    }
    Installed::Paced
}

fn hybrid_text(p: &HybridParams) -> String {
    let back = if p.fallback_count == 0 {
        "b-spin へは戻さない".to_string()
    } else {
        format!(
            "寝た後に期限を越えた枠が {} 枠の中で {} 回に達したら、その走りの残りは b-spin に戻す",
            p.fallback_frames, p.fallback_count
        )
    };
    format!(
        "B と同じ格子で、残りが {} µs + 余裕より多い間は待機タイマーで 1 回最大 {} µs 寝てメッセージループへ戻し、余裕の間は寝ずに Sleep(0) で譲り、最後の {} µs は QPC を読み続ける。余裕は {} µs から、寝過ごしの山 + {} µs まで広げる（上限 {} µs。山は寝るたびに 1/2^{} 減る）。{back}。数値は ini の [pacing] の hybrid_*（★ は既定と違う値）: {}",
        p.spin_us,
        p.chunk_us,
        p.spin_us,
        p.margin_us,
        p.guard_us,
        p.margin_max_us,
        p.decay_shift,
        p.text()
    )
}

/// 書き出しを止め、集計を本ログへ書く（`session::run` が計測の後に呼ぶ）。戻りは試験のまとめの材料（書き出しを起こしていない・終わりを受け取れなければ None）。
pub fn finish(host: &mut dyn PacingHost, out: &mut dyn Write) -> Option<PaceEnd> {
    let end = host.stop_writer()?;
    let c = host.counters();
    match end {
        Err(why) => {
            let _ = writeln!(out, "ペース: 書き出しの終わりを受け取れない（{why}）");
            None
        }
        Ok(e) => {
            // 集計の節は詳細だけ。パニックと書き出しの失敗は試験のまとめ（ペースの項）が簡潔にも残す。
            let out: &mut dyn Write = if c.panics > 0 || e.tally.write_failed {
                out
            } else {
                &mut detail_out(out)
            };
            let _ = writeln!(
                out,
                "==== ペース（M7） ====\nペース: {} に {} 行（周回で読めなかった {} 件{}）。先頭でゲーム自身の経路へ任せた {} 回、パニック {} 回",
                e.file,
                e.tally.rows.saturating_sub(e.tally.csv_capped),
                e.tally.lost,
                if e.tally.write_failed {
                    "。途中から書けなくなった"
                } else {
                    ""
                },
                c.fallbacks,
                c.panics
            );
            // CSV の上限を越えた行は書かず、集計にだけ入れた。
            if e.tally.csv_capped > 0 {
                let _ = writeln!(
                    out,
                    "ペース: 書き出しを始めて {} 秒の後は CSV に書かず、その後の {} 行は集計にだけ入れた（上の集計は {} 行すべて）",
                    e.tally.limits.csv_seconds, e.tally.csv_capped, e.tally.rows
                );
            }
            for l in e.tally.report.lines() {
                let _ = writeln!(out, "{l}");
            }
            let _ = writeln!(out, "{}", glide().line(host.qpf()));
            Some(PaceEnd {
                rows: e.tally.rows,
                lost: e.tally.lost,
                fallbacks: c.fallbacks,
                panics: c.panics,
                write_failed: e.tally.write_failed,
            })
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hook::{ThreadRef, WriteDone};
    use crate::sites::Use;
    use crate::stub::testkit::REAL_HOOK_TESTS;
    use crate::th09_addr::{PACING_ENTRY_SITE, PACING_SKIPPED_SITE};
    use crate::ticks::CopyRow;
    use higanr_core::patch;
    use std::collections::BTreeMap;

    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) struct FakeCode {
        pub mem: BTreeMap<u32, u8>,
        pub writes: Vec<(u32, Vec<u8>)>,
        pub setup_at_write: Vec<u32>,
        pub setup: std::rc::Rc<core::cell::Cell<u32>>,
        pub fail_write_at: Option<u32>,
    }

    impl FakeCode {
        /// 表の全行の期待バイト列と、画面の形の 1 バイト（1 = ウィンドウ）と、ウィンドウの構造体の範囲（0）を置いた偽物。
        pub(crate) fn original() -> FakeCode {
            let mut mem = BTreeMap::new();
            for d in PACING_SITES {
                for (i, b) in d.expected.iter().enumerate() {
                    mem.insert(d.addr + i as u32, *b);
                }
            }
            mem.insert(OPTION_WINDOWED, 1);
            for i in 0..WINDOW_SPAN {
                mem.insert(WINDOW_ADDR + i, 0);
            }
            FakeCode {
                mem,
                writes: Vec::new(),
                setup_at_write: Vec::new(),
                setup: Default::default(),
                fail_write_at: None,
            }
        }

        pub(crate) fn put(&mut self, addr: u32, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                self.mem.insert(addr + i as u32, *b);
            }
        }
    }

    impl HookEnv for FakeCode {
        fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
            Ok(vec![ThreadRef(1)])
        }
        fn suspend(&mut self, _t: ThreadRef) -> bool {
            true
        }
        fn resume(&mut self, _t: ThreadRef) -> bool {
            true
        }
        fn close(&mut self, _t: ThreadRef) {}
        fn eip(&mut self, _t: ThreadRef) -> Option<u32> {
            Some(0x7000_0000)
        }
        fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            for (i, b) in buf.iter_mut().enumerate() {
                match self.mem.get(&(addr + i as u32)) {
                    Some(v) => *b = *v,
                    None => return false,
                }
            }
            true
        }
        fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
            if self
                .fail_write_at
                .is_some_and(|a| addr >= a && addr < a + 5)
            {
                return Err(5);
            }
            self.put(addr, bytes);
            self.writes.push((addr, bytes.to_vec()));
            self.setup_at_write.push(self.setup.get());
            Ok(WriteDone {
                protect_restored: true,
                flushed: true,
            })
        }
        fn set_original_target(&mut self, _target: u32) {
            panic!("ペースは入力の入口の静的変数に触らない");
        }
        fn set_window(&mut self, _addr: u32) {
            panic!("ペースは入力の入口の静的変数に触らない");
        }
        fn set_copy_table(&mut self, _rows: &[CopyRow]) {
            panic!("ペースは入力の入口の静的変数に触らない");
        }
        fn set_render_only_exit(&mut self, _addr: u32) {
            panic!("ペースは入力の入口の静的変数に触らない");
        }
        fn entry_address(&self) -> u32 {
            0x1000_0000
        }
        fn match_entry_address(&self) -> u32 {
            0x1000_1000
        }
        fn set_match_original(&mut self, _target: u32) {
            panic!("ペースは開始のフックに触らない");
        }
        fn sleep_ms(&mut self, _ms: u32) {}
        fn now_ms(&self) -> u64 {
            0
        }
        fn counter(&self) -> u32 {
            0
        }
    }

    pub(crate) const HEAD_ENTRY: u32 = 0x1000_5000;
    pub(crate) const PRESENT_ENTRY: u32 = 0x1000_6000;

    #[derive(Default)]
    pub(crate) struct FakeHost {
        pub ini: Option<String>,
        pub read_fail: bool,
        pub qpf: u64,
        pub arm_fail: bool,
        pub writer_fail: bool,
        pub writer_capped: u64,
        pub writer_extra_rows: u32,
        pub events: Vec<String>,
        pub present_original: Option<u32>,
        pub head_targets: Option<(u32, u32, u32)>,
        pub stamps: bool,
        pub writer_mode: Option<Mode>,
        pub armed_fps: Option<u64>,
        pub writer_fps: Option<u64>,
        pub armed: Option<PacingIni>,
        pub armed_screen_flag: Option<u32>,
        pub armed_window: Option<u32>,
        pub writer_pre: Option<Preamble>,
        pub writer_classes: Option<CpuClasses>,
        pub classes: Option<CpuClasses>,
        pub setup: std::rc::Rc<core::cell::Cell<u32>>,
    }

    pub(crate) fn hybrid() -> CpuClasses {
        CpuClasses::from_pairs(vec![(0, 1), (1, 1), (2, 1), (3, 1), (4, 0), (5, 0)])
    }

    impl FakeHost {
        pub(crate) fn with_ini(ini: &str) -> FakeHost {
            FakeHost {
                ini: Some(ini.to_string()),
                qpf: 10_000_000,
                classes: Some(hybrid()),
                ..Default::default()
            }
        }
    }

    impl PacingHost for FakeHost {
        fn read_ini(&mut self) -> Result<Option<String>, String> {
            self.events.push("read_ini".into());
            if self.read_fail {
                return Err("合成の失敗".into());
            }
            Ok(self.ini.clone())
        }
        fn qpf(&self) -> u64 {
            self.qpf
        }
        fn head_entry(&self) -> u32 {
            HEAD_ENTRY
        }
        fn present_entry(&self) -> u32 {
            PRESENT_ENTRY
        }
        fn arm_pacer(
            &mut self,
            _qpf: u64,
            cfg: &PacingIni,
            screen_flag: u32,
            window: u32,
        ) -> Result<String, String> {
            self.events.push("arm_pacer".into());
            if self.arm_fail {
                return Err("合成の失敗".into());
            }
            self.armed_fps = Some(cfg.fps);
            self.armed = Some(*cfg);
            self.armed_screen_flag = Some(screen_flag);
            self.armed_window = Some(window);
            Ok("合成のタイマー".into())
        }
        fn cpu_classes(&mut self) -> Result<CpuClasses, String> {
            self.events.push("cpu_classes".into());
            self.classes.clone().ok_or("合成: 表が無い".to_string())
        }
        fn start_writer(
            &mut self,
            pre: Preamble,
            fps: u64,
            classes: CpuClasses,
        ) -> Result<String, String> {
            self.events.push("start_writer".into());
            if self.writer_fail {
                return Err("合成の失敗".into());
            }
            self.writer_mode = Some(pre.mode);
            self.writer_fps = Some(fps);
            self.writer_pre = Some(pre);
            self.writer_classes = Some(classes);
            Ok("synthetic_pacing.csv".into())
        }
        fn stop_writer(&mut self) -> Option<Result<WriterEnd, String>> {
            self.events.push("stop_writer".into());
            let pre = self.writer_pre.clone()?;
            let mut s = Splitter::new(
                self.qpf,
                self.writer_fps.unwrap_or(pace::DEFAULT_FPS),
                &pre.eff_classes,
                pre.mode.fixed_phase(),
            );
            s.push(&FrameRow {
                frame: 0,
                present_begin: 5,
                present_end: 6,
                ..Default::default()
            });
            for f in 1..=self.writer_extra_rows {
                s.push(&FrameRow {
                    frame: f,
                    present_begin: 5 + u64::from(f) * 166_667,
                    present_end: 6 + u64::from(f) * 166_667,
                    ..Default::default()
                });
            }
            Some(Ok(WriterEnd {
                file: "synthetic_pacing.csv".into(),
                tally: WriterTally {
                    rows: 1 + u64::from(self.writer_extra_rows) + self.writer_capped,
                    lost: 0,
                    write_failed: false,
                    limits: writer_limits(false),
                    csv_capped: self.writer_capped,
                    report: s.finish(),
                },
            }))
        }
        fn set_present_original(&mut self, original: u32) {
            self.events.push("set_present_original".into());
            self.present_original = Some(original);
            self.setup.set(self.setup.get() + 1);
        }
        fn set_head_targets(&mut self, original: u32, resume: u32, early_exit: u32) {
            self.events.push("set_head_targets".into());
            self.head_targets = Some((original, resume, early_exit));
            self.setup.set(self.setup.get() + 1);
        }
        fn start_stamps(&mut self) {
            self.events.push("start_stamps".into());
            self.stamps = true;
        }
        fn counters(&self) -> PaceCounters {
            PaceCounters::default()
        }
    }

    fn site(name: &str) -> &'static SiteDef {
        row(name).unwrap()
    }

    fn run(code: &mut FakeCode, host: &mut FakeHost) -> (Installed, String) {
        code.setup = std::rc::Rc::clone(&host.setup);
        let mut out = Vec::new();
        let r = install(code, host, &mut out).installed;
        (r, String::from_utf8(out).unwrap())
    }

    const INI_GAME: &str = "[pacing]\nmode = game\n";
    const INI_B: &str = "[netplay]\nenable = 0\n[pacing]\nmode = b\n";

    #[test]
    fn game_mode_writes_only_the_present_call() {
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini(INI_GAME);
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Stamps, "{log}");
        let p = site(PRESENT_SITE);
        assert_eq!(code.writes.len(), 1, "{log}");
        assert_eq!(code.writes[0].0, p.addr + 1, "rel32 の位置");
        let back: [u8; 5] = (0..5)
            .map(|i| code.mem[&(p.addr + i)])
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert_eq!(patch::call_target(p.addr, &back), Some(PRESENT_ENTRY));
        assert_eq!(host.present_original, Some(0x0042_DB50));
        assert_eq!(code.setup_at_write, vec![1], "元の飛び先を置く前に書いた");
        assert_eq!(
            host.events,
            vec![
                "read_ini",
                "cpu_classes",
                "start_writer",
                "set_present_original",
                "start_stamps"
            ]
        );
        assert_eq!(host.writer_mode, Some(Mode::Game));
        assert_eq!(
            host.writer_pre,
            Some(Preamble {
                qpf: 10_000_000,
                mode: Mode::Game,
                eff_classes: vec![0, 1],
                cycle_secs: None,
            })
        );
        assert_eq!(host.writer_classes, Some(hybrid()));
        assert!(
            log.contains("ペース: 効率クラス（GetSystemCpuSetInformation）: 論理 CPU 6 個: class 0 が 2 個 / class 1 が 4 個"),
            "{log}"
        );
        assert!(host.head_targets.is_none());
        let h = site(PACING_HEAD_SITE);
        let now: Vec<u8> = (0..5).map(|i| code.mem[&(h.addr + i)]).collect();
        assert_eq!(now, h.expected);
    }

    #[test]
    fn b_mode_writes_present_then_head() {
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini(INI_B);
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        let (p, h) = (site(PRESENT_SITE), site(PACING_HEAD_SITE));
        assert_eq!(
            code.writes.iter().map(|w| w.0).collect::<Vec<_>>(),
            vec![p.addr + 1, h.addr + 1]
        );
        assert_eq!(code.setup_at_write, vec![1, 2]);
        let back: [u8; 5] = (0..5)
            .map(|i| code.mem[&(h.addr + i)])
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert_eq!(patch::call_target(h.addr, &back), Some(HEAD_ENTRY));
        assert_eq!(
            host.head_targets,
            Some((0x0042_CC90, 0x0042_DC8D, 0x0042_DD81))
        );
        assert_eq!(host.writer_mode, Some(Mode::B));
        assert_eq!(
            host.events,
            vec![
                "read_ini",
                "cpu_classes",
                "arm_pacer",
                "start_writer",
                "set_present_original",
                "start_stamps",
                "set_head_targets"
            ]
        );
        assert!(log.contains("地点 6 行を照合して全部一致"), "{log}");
        assert!(log.contains("ペース: B を入れた"), "{log}");
        assert_eq!((host.armed_fps, host.writer_fps), (Some(60), Some(60)));
        assert!(log.contains("QPC の整数で 60.000 fps"), "{log}");
        assert!(!log.contains("★検査用"), "{log}");
    }

    const INI_B_240: &str = "[pacing]\nmode = b\nfps = 240\n";

    #[test]
    fn b_mode_passes_the_ini_fps_and_marks_the_log() {
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini(INI_B_240);
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        assert_eq!((host.armed_fps, host.writer_fps), (Some(240), Some(240)));
        assert!(log.contains("QPC の整数で 240.000 fps"), "{log}");
        assert!(
            log.contains("ペース: ★検査用: fps = 240（ゲームが 240/60 倍で進む。"),
            "{log}"
        );
        let mut out = Vec::new();
        finish(&mut host, &mut out);
        let fin = String::from_utf8(out).unwrap();
        assert!(fin.contains("ペース 240 fps"), "{fin}");
        for (qpf, want) in [(239u64, Installed::Nothing), (240, Installed::Paced)] {
            let mut code = FakeCode::original();
            let mut host = FakeHost {
                qpf,
                ..FakeHost::with_ini(INI_B_240)
            };
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, want, "qpf {qpf}: {log}");
            if want == Installed::Nothing {
                assert!(code.writes.is_empty());
                assert!(log.contains("QPF 239 Hz では 240 fps を刻めない"), "{log}");
                assert_eq!(host.events, vec!["read_ini"]);
            }
        }
        let mut code = FakeCode::original();
        let mut host = FakeHost {
            arm_fail: true,
            ..FakeHost::with_ini(INI_B_240)
        };
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Stamps, "{log}");
        assert_eq!(
            (host.writer_mode, host.writer_fps),
            (Some(Mode::Game), Some(60))
        );
        assert!(!log.contains("★検査用"), "{log}");
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini("[pacing]\nmode = game\nfps = 240\n");
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Nothing, "{log}");
        assert!(code.writes.is_empty());
        assert!(
            log.contains("mode = b / b-spin / b-hybrid / vpatch のときだけ効く"),
            "{log}"
        );
    }

    /// vpatch（FpuPatch の 5 バイト・Vsync≥0 の 8 バイト）や rev5 #20 が先頭を書き換えた実行で、b が何も書かず書き出しも起こさない。標本は実際に出る形から取る。どの形でも game なら Present のフックだけは付く。
    #[test]
    fn vpatch_forms_at_the_head_write_nothing() {
        let h = site(PACING_HEAD_SITE).addr;
        let mut rev5 = patch::call_rel32(h, 0x6201_CC80).to_vec();
        rev5.extend_from_slice(&[0xEB, 0x42, 0x90]);
        let mut vsync = patch::call_rel32(h, 0x6000_40B0).to_vec();
        vsync.extend_from_slice(&[0xEB, 0x42, 0x90]);
        // FpuPatch の飛び先は読んでいない（vpatch の DLL の中のどこか）ので合成のアドレス。形（E8 rel32 の 5 バイト）だけ実物。
        let fpu = patch::call_rel32(h, 0x6000_3F00).to_vec();
        let mut n = 0;
        for (label, bytes) in [
            ("rev5 #20", rev5),
            ("vpatch Vsync", vsync),
            ("vpatch FpuPatch", fpu),
        ] {
            let mut code = FakeCode::original();
            code.put(h, &bytes);
            let mut host = FakeHost::with_ini(INI_B);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Nothing, "{label}: {log}");
            assert!(code.writes.is_empty(), "{label}: 書いた");
            assert!(
                !host.events.contains(&"start_writer".to_string()),
                "{label}"
            );
            assert!(log.contains("1 バイトも書かない"), "{label}: {log}");
            assert!(log.contains(PACING_HEAD_SITE), "{label}: {log}");
            let mut code = FakeCode::original();
            code.put(h, &bytes);
            let mut host = FakeHost::with_ini(INI_GAME);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Stamps, "{label}: {log}");
            assert_eq!(code.writes.len(), 1);
            n += 1;
        }
        assert_eq!(n, 3);
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini(INI_B);
        assert_eq!(run(&mut code, &mut host).0, Installed::Paced);
    }

    #[test]
    fn any_mismatching_row_refuses_the_whole_mode() {
        for name in [
            PACING_ENTRY_SITE,
            PACING_SKIPPED_SITE,
            PACING_RESUME_SITE,
            PACING_EARLY_EXIT_SITE,
            PRESENT_SITE,
        ] {
            let d = site(name);
            let last = d.addr + d.expected.len() as u32 - 1;
            let mut code = FakeCode::original();
            code.mem
                .insert(last, d.expected[d.expected.len() - 1] ^ 0x01);
            let mut host = FakeHost::with_ini(INI_B);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Nothing, "{name}: {log}");
            assert!(code.writes.is_empty(), "{name}");
            let mut code = FakeCode::original();
            code.mem.remove(&last);
            let mut host = FakeHost::with_ini(INI_B);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Nothing, "{name}: {log}");
            assert!(log.contains("が読めない"), "{name}: {log}");
        }
        let d = site(PRESENT_SITE);
        let mut code = FakeCode::original();
        code.mem.insert(d.addr + 2, 0x00);
        let mut host = FakeHost::with_ini(INI_GAME);
        assert_eq!(run(&mut code, &mut host).0, Installed::Nothing);
        assert!(code.writes.is_empty());
    }

    #[test]
    fn install_reports_requested_and_used_modes() {
        let go = |ini: &str, arm_fail: bool, bad_head: bool| {
            let mut code = FakeCode::original();
            if bad_head {
                let h = site(PACING_HEAD_SITE).addr;
                code.put(h, &patch::call_rel32(h, 0x6000_3F00));
            }
            let mut host = FakeHost {
                arm_fail,
                ..FakeHost::with_ini(ini)
            };
            code.setup = std::rc::Rc::clone(&host.setup);
            install(&mut code, &mut host, &mut Vec::new())
        };
        let r = go(INI_GAME, false, false);
        assert_eq!(
            (r.installed, r.requested, r.used),
            (Installed::Stamps, Some(Mode::Game), Some(Mode::Game))
        );
        let r = go(INI_B, false, false);
        assert_eq!(
            (r.installed, r.requested, r.used),
            (Installed::Paced, Some(Mode::B), Some(Mode::B))
        );
        let r = go(INI_B, true, false);
        assert_eq!(
            (r.installed, r.requested, r.used),
            (Installed::Stamps, Some(Mode::B), Some(Mode::Game))
        );
        let r = go(INI_B, false, true);
        assert_eq!(
            (r.installed, r.requested, r.used),
            (Installed::Nothing, Some(Mode::B), None)
        );
        let mut code = FakeCode::original();
        code.fail_write_at = Some(site(PACING_HEAD_SITE).addr);
        let mut host = FakeHost::with_ini(INI_B);
        code.setup = std::rc::Rc::clone(&host.setup);
        let r = install(&mut code, &mut host, &mut Vec::new());
        assert_eq!(
            (r.installed, r.requested, r.used),
            (Installed::Stamps, Some(Mode::B), Some(Mode::Game)),
            "先頭の書き込みを断られた"
        );
        let mut code = FakeCode::original();
        let mut host = FakeHost {
            read_fail: true,
            ..FakeHost::with_ini(INI_B)
        };
        let r = install(&mut code, &mut host, &mut Vec::new());
        assert_eq!(
            (r.installed, r.requested, r.used),
            (Installed::Nothing, None, None)
        );
    }

    #[test]
    fn missing_pieces_degrade_or_refuse() {
        let mut code = FakeCode::original();
        let mut host = FakeHost {
            arm_fail: true,
            ..FakeHost::with_ini(INI_B)
        };
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Stamps, "{log}");
        assert_eq!(code.writes.len(), 1);
        assert_eq!(host.writer_mode, Some(Mode::Game));
        assert!(log.contains("b は書かず、時刻のフックだけ付ける"), "{log}");
        let mut code = FakeCode::original();
        let mut host = FakeHost {
            writer_fail: true,
            ..FakeHost::with_ini(INI_B)
        };
        assert_eq!(run(&mut code, &mut host).0, Installed::Nothing);
        assert!(code.writes.is_empty());
        for (ini, read_fail, qpf, want) in [
            (
                None,
                true,
                10_000_000,
                "HiganRTool.ini を読めない（合成の失敗）",
            ),
            (
                Some("[pacing]\nmode = c\n"),
                false,
                10_000_000,
                "game / b / b-spin / b-hybrid / vpatch / cycle / auto のどれか",
            ),
            (
                Some("[pacing]\nmdoe = b-spin\n"),
                false,
                10_000_000,
                "知らない",
            ),
            (Some("[pacing]\nmode = b\n"), false, 59, "刻めない"),
            (Some("[netplay]\nenable = 0\n"), false, 59, "刻めない"),
        ] {
            let mut code = FakeCode::original();
            let mut host = FakeHost {
                ini: ini.map(str::to_string),
                read_fail,
                qpf,
                ..Default::default()
            };
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Nothing, "{want}: {log}");
            assert!(code.writes.is_empty());
            assert!(log.contains(want), "{want}: {log}");
            assert_eq!(host.events, vec!["read_ini"], "{want}");
        }
    }

    #[test]
    fn defaults_to_auto_without_the_section_or_the_mode() {
        let (p, h) = (site(PRESENT_SITE), site(PACING_HEAD_SITE));
        let default_line = "で既定の mode = auto（60 fps）で置く";
        for (ini, why) in [
            (None, "ペース: HiganRTool.ini が無いの"),
            (
                Some("[netplay]\nenable = 0\n"),
                "ペース: ini に [pacing] が無いの",
            ),
            (Some("[pacing]\n"), "ペース: [pacing] に mode が無いの"),
            (
                Some("[pacing]\nfps = 60\n"),
                "ペース: [pacing] に mode が無いの",
            ),
        ] {
            let mut code = FakeCode::original();
            let mut host = FakeHost {
                ini: ini.map(str::to_string),
                ..FakeHost::with_ini("")
            };
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Paced, "{ini:?}: {log}");
            assert!(
                log.contains(&format!("{why}{default_line}")),
                "{ini:?}: {log}"
            );
            assert_eq!(
                code.writes.iter().map(|w| w.0).collect::<Vec<_>>(),
                vec![p.addr + 1, h.addr + 1],
                "{ini:?}"
            );
            let back: [u8; 5] = (0..5)
                .map(|i| code.mem[&(h.addr + i)])
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            assert_eq!(patch::call_target(h.addr, &back), Some(HEAD_ENTRY));
            assert_eq!(host.armed, Some(PacingIni::DEFAULT), "{ini:?}");
            assert_eq!(host.armed.map(|c| (c.mode, c.fps)), Some((Mode::Auto, 60)));
            assert_eq!(host.armed_screen_flag, Some(OPTION_WINDOWED), "{ini:?}");
            assert_eq!(host.armed_window, Some(WINDOW_ADDR), "{ini:?}");
            assert_eq!(
                (host.writer_mode, host.writer_fps),
                (Some(Mode::Auto), Some(60))
            );
            assert!(log.contains("ペース: 形は auto（既定。"), "{log}");
            assert!(
                log.contains("ペース: 画面の形 0x004B3539（設定の +0xB1）は読める。設置の時の値 1"),
                "{log}"
            );
            assert!(!log.contains("★検査用"), "{log}");
        }
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini("[pacing]\nmode = auto\n");
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        assert_eq!(host.armed, Some(PacingIni::DEFAULT));
        assert_eq!(host.armed_screen_flag, Some(OPTION_WINDOWED));
        assert!(!log.contains(default_line), "{log}");
        let mut code = FakeCode::original();
        code.mem.remove(&OPTION_WINDOWED);
        let mut host = FakeHost::with_ini("");
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        assert_eq!(host.armed_screen_flag, Some(0));
        assert!(
            log.contains("ペース: 画面の形 0x004B3539 が読めない。auto は全フレームをゲーム自身の待ちの経路へ任せる"),
            "{log}"
        );
        let mut code = FakeCode::original();
        code.mem.remove(&(WINDOW_ADDR + WINDOW_SPAN - 1));
        let mut host = FakeHost::with_ini("");
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        assert_eq!(host.armed_window, Some(0));
        assert_eq!(host.armed_screen_flag, Some(OPTION_WINDOWED));
        assert!(
            log.contains("ペース: ウィンドウの構造体 0x004B30B0 が読めない。ゲーム自身の時計の経路（全画面）では先頭で相手の入力ワードを待たない"),
            "{log}"
        );
        for (ini, mode, window) in [
            ("[pacing]\nmode = b\n", Mode::B, 0),
            ("[pacing]\nmode = b-spin\n", Mode::BSpin, 0),
            ("[pacing]\nmode = cycle\n", Mode::Cycle, WINDOW_ADDR),
        ] {
            let mut code = FakeCode::original();
            let mut host = FakeHost::with_ini(ini);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Paced, "{ini:?}: {log}");
            assert_eq!(host.armed.map(|c| c.mode), Some(mode), "{ini:?}");
            assert_eq!(host.armed_screen_flag, Some(0), "{ini:?}");
            assert_eq!(host.armed_window, Some(window), "{ini:?}");
            assert_eq!(host.writer_mode, Some(mode), "{ini:?}");
            assert!(!log.contains("画面の形"), "{ini:?}: {log}");
            assert!(!log.contains(default_line), "{ini:?}: {log}");
        }
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini("[netplay]\nenable = 0\n[pacing]\nmode = game\n");
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Stamps, "{log}");
        assert_eq!(
            code.writes.iter().map(|w| w.0).collect::<Vec<_>>(),
            vec![p.addr + 1]
        );
        let now: Vec<u8> = (0..5).map(|i| code.mem[&(h.addr + i)]).collect();
        assert_eq!(now, h.expected);
        assert!(host.armed.is_none() && host.head_targets.is_none());
        assert!(!log.contains(default_line), "{log}");
        let fpu = patch::call_rel32(h.addr, 0x6000_3F00).to_vec();
        for (label, at, bytes) in [
            ("頭", h.addr, fpu),
            ("Present", p.addr + 4, vec![p.expected[4] ^ 0x01]),
        ] {
            for ini in [None, Some("[pacing]\n")] {
                let mut code = FakeCode::original();
                code.put(at, &bytes);
                let mut host = FakeHost {
                    ini: ini.map(str::to_string),
                    ..FakeHost::with_ini("")
                };
                let (r, log) = run(&mut code, &mut host);
                assert_eq!(r, Installed::Nothing, "{label} {ini:?}: {log}");
                assert!(code.writes.is_empty(), "{label} {ini:?}: 書いた");
                assert_eq!(host.events, vec!["read_ini"], "{label} {ini:?}");
                assert!(
                    log.contains("ペース: mode = auto の地点 6 行のうち 1 行が合わないので、1 バイトも書かない。ゲーム自身の待ちのまま動く"),
                    "{label} {ini:?}: {log}"
                );
            }
        }
    }

    #[test]
    fn finish_writes_the_summary_only_after_a_writer() {
        let mut host = FakeHost::with_ini(INI_GAME);
        let mut out = Vec::new();
        finish(&mut host, &mut out);
        assert!(out.is_empty());
        let mut code = FakeCode::original();
        let _ = run(&mut code, &mut host);
        let mut out = Vec::new();
        finish(&mut host, &mut out);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("==== ペース（M7） ===="), "{log}");
        assert!(log.contains("synthetic_pacing.csv に 1 行"), "{log}");
        assert!(log.contains("ペースの集計: フレーム 1 件"), "{log}");
        assert!(
            !log.contains("集計にだけ入れた"),
            "上限の後の行が無いのに注が出た:\n{log}"
        );
    }

    #[test]
    fn finish_says_the_rows_after_the_csv_limit_were_summarized_only() {
        let mut host = FakeHost {
            writer_capped: 5,
            ..FakeHost::with_ini(INI_GAME)
        };
        let mut code = FakeCode::original();
        let _ = run(&mut code, &mut host);
        let mut out = Vec::new();
        finish(&mut host, &mut out);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("synthetic_pacing.csv に 1 行"), "{log}");
        assert!(
            log.contains(
                "書き出しを始めて 1260 秒の後は CSV に書かず、その後の 5 行は集計にだけ入れた（上の集計は 6 行すべて）"
            ),
            "{log}"
        );
    }

    #[test]
    fn ring_keeps_one_frame_per_present() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        assert_eq!(ring.read(0), PaceRead::NotYet);
        ring.on_phase(Phase::B);
        ring.on_head(100, 500);
        ring.on_sleep();
        ring.on_head(300, 500);
        ring.on_start(501, false, false, 0);
        ring.on_head_cpu(0);
        ring.on_glide(-7_654, -1_275);
        ring.on_hybrid_margin(2_000);
        ring.on_hybrid_margin(1_800);
        ring.on_oversleep(0);
        ring.on_oversleep(350);
        ring.on_oversleep(120);
        ring.on_input(600, 42, 5, 7_000);
        ring.on_input(650, 43, 3, 7_500);
        ring.on_present_begin(900, 2, 9_000);
        assert_eq!(ring.read(0), PaceRead::NotYet, "札の前に読めた");
        ring.on_present_end(950);
        ring.on_present_begin(2_000, 1, 0);
        ring.on_present_end(2_100);
        assert_eq!(ring.completed(), 2);
        let PaceRead::Row(r0) = ring.read(0) else {
            panic!("0 が読めない")
        };
        assert_eq!(
            r0,
            FrameRow {
                frame: 0,
                paced: true,
                deadline: 500,
                first_head: 100,
                head_calls: 2,
                sleeps: 1,
                start: 501,
                late: false,
                rebased: false,
                skipped: 0,
                inputs: 2,
                sup_frame: 42,
                input: 600,
                present_begin: 900,
                present_end: 950,
                phase: Some(Phase::B),
                head_cpu: Some(0),
                input_cpu: Some(5),
                present_cpu: Some(2),
                head_eff: None,
                input_eff: None,
                present_eff: None,
                input_cycles: 7_000,
                present_cycles: 9_000,
                glide_req_ppm: -7_654,
                glide_moved: -1_275,
                hybrid_margin_us: 2_000,
                oversleep_us: Some(350),
            }
        );
        let PaceRead::Row(r1) = ring.read(1) else {
            panic!("1 が読めない")
        };
        assert_eq!(
            r1,
            FrameRow {
                frame: 1,
                present_begin: 2_000,
                present_end: 2_100,
                present_cpu: Some(1),
                ..Default::default()
            }
        );
        assert_eq!(ring.read(2), PaceRead::NotYet);
        ring.reset();
    }

    #[test]
    fn ring_reports_lapped_frames() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        for i in 0..(PACE_RING_LEN + 5) {
            if i < PACE_RING_LEN {
                ring.on_input(i as u64 * 10, i, 3, 77);
                ring.on_head(i as u64 * 10, i as u64 * 10);
                ring.on_phase(Phase::BSpin);
                ring.on_head_cpu(4);
                ring.on_glide(9_000, 1_500);
                ring.on_hybrid_margin(2_000);
                ring.on_oversleep(400);
                ring.on_present_begin(i as u64 * 10 + 1, 5, 88);
            } else {
                ring.on_present_begin(i as u64 * 10 + 1, 0, 0);
            }
            ring.on_present_end(i as u64 * 10 + 2);
        }
        assert_eq!(ring.read(0), PaceRead::Lapped);
        assert_eq!(ring.read(4), PaceRead::Lapped);
        assert_eq!(ring.read(5), PaceRead::Lapped);
        assert!(matches!(ring.read(6), PaceRead::Row(r) if r.frame == 6 && r.present_begin == 61));
        let last = PACE_RING_LEN + 4;
        let PaceRead::Row(r) = ring.read(last) else {
            panic!("最後のフレームが読めない")
        };
        assert_eq!(r.frame, last);
        assert_eq!(
            (
                r.inputs,
                r.input,
                r.sup_frame,
                r.paced,
                r.deadline,
                r.head_calls
            ),
            (0, 0, 0, false, 0, 0),
            "前の周の値が残った: {r:?}"
        );
        assert_eq!(
            (r.phase, r.head_cpu, r.input_cpu, r.input_cycles),
            (None, None, None, 0),
            "足した語に前の周の値が残った: {r:?}"
        );
        assert_eq!(
            (r.hybrid_margin_us, r.oversleep_us),
            (0, None),
            "b-hybrid の 2 語に前の周の値が残った: {r:?}"
        );
        assert_eq!(
            (r.glide_req_ppm, r.glide_moved),
            (0, 0),
            "寄せの語に前の周の値が残った: {r:?}"
        );
        ring.reset();
    }

    /// 壊れたら落ちる: CSV の秒数 0（簡潔なログ）で行を出力に書く。集計に入れなくなる。
    #[test]
    fn writer_loop_with_zero_csv_seconds_only_tallies() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        for i in 0..5u64 {
            ring.on_input(1_000_000 + i * 166_667, i as u32, 1, 500_000_000 + i);
            ring.on_present_begin(1_030_000 + i * 166_667, 4, 511_100_000 + i);
            ring.on_present_end(1_031_000 + i * 166_667);
        }
        let mut buf = Vec::new();
        let pre = Preamble {
            eff_classes: hybrid().distinct(),
            ..Preamble::new(10_000_000, Mode::Game)
        };
        let t = writer_loop(
            ring,
            &pre,
            pace::DEFAULT_FPS,
            &hybrid(),
            &mut buf,
            &|| true,
            &mut |_| {},
            WriterLimits {
                csv_seconds: 0,
                thread_seconds: 10,
            },
        );
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 2, "見出しの 2 行だけ: {text}");
        assert_eq!((t.rows, t.csv_capped), (5, 5));
        assert_eq!(t.report.all.rows, 5);
    }

    #[test]
    fn writer_loop_writes_every_completed_frame() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        for i in 0..5u64 {
            if i == 2 {
                ring.on_glide(-10_000, -1_667);
            }
            ring.on_input(1_000_000 + i * 166_667, i as u32, 1, 500_000_000 + i);
            ring.on_present_begin(1_030_000 + i * 166_667, 4, 511_100_000 + i);
            ring.on_present_end(1_031_000 + i * 166_667);
        }
        let mut buf = Vec::new();
        let polls = core::cell::Cell::new(0u32);
        let mut sleeps = Vec::new();
        let pre = Preamble {
            eff_classes: hybrid().distinct(),
            ..Preamble::new(qpf, Mode::Game)
        };
        let t = writer_loop(
            ring,
            &pre,
            pace::DEFAULT_FPS,
            &hybrid(),
            &mut buf,
            &|| polls.get() >= 1,
            &mut |ms| {
                sleeps.push(ms);
                polls.set(polls.get() + 1);
                for i in 5..7u64 {
                    ring.on_present_begin(1_030_000 + i * 166_667, 9, 0);
                    ring.on_present_end(1_031_000 + i * 166_667);
                }
            },
            WriterLimits {
                csv_seconds: 10,
                thread_seconds: 10,
            },
        );
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# qpf=10000000 mode=game eff_classes=0,1");
        assert_eq!(lines[1], pace::CSV_HEADER);
        assert_eq!(lines.len(), 2 + 7, "{text}");
        assert_eq!((t.rows, t.lost, t.write_failed), (7, 0, false));
        assert_eq!(sleeps, vec![WRITER_POLL_MS]);
        let all = &t.report.all;
        assert_eq!(all.rows, 7);
        assert_eq!(all.no_input, 2);
        assert_eq!(all.input_to_present.unwrap().p50, 30_000);
        assert!(
            lines[2].ends_with(",game,,,1,1,4,0,500000000,511100000,11100000,3700,0,0.0,0,,"),
            "{}",
            lines[2]
        );
        assert!(
            lines[4].ends_with(",3700,-10000,-166.7,0,,1"),
            "{}",
            lines[4]
        );
        assert!(lines[3].ends_with(",3700,0,0.0,0,,1"), "{}", lines[3]);
        assert!(lines[5].ends_with(",3700,0,0.0,0,,1"), "{}", lines[5]);
        assert!(lines[8].contains(",game,,,,,9,,"), "{}", lines[8]);
        assert_eq!(
            lines[8].split(',').count(),
            pace::CSV_HEADER.split(',').count()
        );
        assert_eq!(all.kinds, [0, 0, 5, 0, 2]);
        assert_eq!(t.report.phase(Phase::Game).unwrap().rows, 7);
        let back: Vec<FrameRow> = lines
            .iter()
            .filter_map(|l| FrameRow::parse_csv(l).ok().flatten())
            .collect();
        assert_eq!(back.len(), 7);
        assert_eq!(back[6].frame, 6);
        assert_eq!((back[2].glide_req_ppm, back[2].glide_moved), (-10_000, 0));
        let back_at: Vec<FrameRow> = lines
            .iter()
            .filter_map(|l| FrameRow::parse_csv_at(l, qpf).ok().flatten())
            .collect();
        assert_eq!(
            (back_at[2].glide_req_ppm, back_at[2].glide_moved),
            (-10_000, -1_667)
        );
        assert_eq!(all.glide.nonzero, 1, "集計に寄せの要求が届いていない");
        ring.reset();
        for i in 0..(PACE_RING_LEN + 3) as u64 {
            ring.on_present_begin(i * 10 + 1, 0, 0);
            ring.on_present_end(i * 10 + 2);
        }
        let t = writer_loop(
            ring,
            &Preamble::new(qpf, Mode::B),
            240,
            &CpuClasses::default(),
            &mut Vec::new(),
            &|| true,
            &mut |_| {},
            WriterLimits {
                csv_seconds: 1_260,
                thread_seconds: 0,
            },
        );
        assert_eq!((t.rows, t.lost), (PACE_RING_LEN as u64 - 1, 4));
        assert_eq!(t.rows + t.lost, (PACE_RING_LEN + 3) as u64);
        assert_eq!(t.report.all.target_fps, 240);
        ring.reset();
    }

    /// 偽の時計: `now` を返すたびに `step` 進み、寝ると寝た µs ぶん進む（10 MHz で 1 µs = 10 カウント）。譲ると 5 µs 進む。
    struct FakeClock {
        now: u64,
        step: u64,
        slept: Vec<u32>,
        /// 普通の待機タイマーで寝た長さ（µs）。
        plain: Vec<u32>,
        yields: u32,
        cpu: u32,
        overshoot: u64,
        screen: Screen,
        screen_reads: u32,
    }

    impl FakeClock {
        fn at(now: u64, step: u64) -> FakeClock {
            FakeClock {
                now,
                step,
                slept: Vec::new(),
                plain: Vec::new(),
                yields: 0,
                cpu: 3,
                overshoot: 700,
                screen: Screen::Unknown,
                screen_reads: 0,
            }
        }
    }

    impl PaceClock for FakeClock {
        fn now(&mut self) -> u64 {
            let t = self.now;
            self.now += self.step;
            t
        }
        fn sleep_us(&mut self, us: u32) {
            self.slept.push(us);
            self.now += us as u64 * 10 + self.overshoot; // 起床の遅れ（既定 70 µs）
        }
        fn sleep_plain_us(&mut self, us: u32) {
            self.plain.push(us);
            self.now += us as u64 * 10 + self.overshoot;
        }
        fn yield_now(&mut self) {
            self.yields += 1;
            self.now += 50;
        }
        fn cpu(&mut self) -> u32 {
            self.cpu
        }
        fn screen(&mut self) -> Screen {
            self.screen_reads += 1;
            self.screen
        }
    }

    fn cfg(mode: Mode) -> PacingIni {
        PacingIni {
            mode,
            fps: pace::DEFAULT_FPS,
            cycle_secs: pace::DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        }
    }

    #[test]
    fn head_sleeps_in_chunks_and_starts_at_the_deadline() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(qpf, &cfg(Mode::B)).unwrap();
        let mut c = FakeClock::at(1_000_000, 3);
        assert_eq!(p.head(&mut c, ring), HEAD_RUN);
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let d1 = p.pace().grid().unwrap().at(1);
        c.now += 50_000;
        let mut codes = Vec::new();
        loop {
            let r = p.head(&mut c, ring);
            codes.push(r);
            if r == HEAD_RUN {
                break;
            }
            assert!(codes.len() < 100, "始まらない");
        }
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        assert!(codes[..codes.len() - 1].iter().all(|&r| r == HEAD_NOT_YET));
        assert!(
            c.slept.iter().all(|us| (1..=CHUNK_US).contains(us)),
            "{:?}",
            c.slept
        );
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert_eq!(r.deadline, d1);
        assert!(r.start >= d1, "期限の前に始めた");
        assert!(
            r.start - d1 < 10,
            "回し読みの後の遅れ {} カウント",
            r.start - d1
        );
        assert_eq!(r.head_calls as usize, codes.len());
        assert_eq!(r.sleeps as usize, c.slept.len());
        assert_eq!(r.sleeps, 10, "{:?}", c.slept);
        assert_eq!(codes.len(), 11);
        assert!(!r.late && r.paced);
        c.now = p.pace().grid().unwrap().at(2) + 100_000;
        let slept = c.slept.len();
        assert_eq!(p.head(&mut c, ring), HEAD_RUN);
        assert_eq!(c.slept.len(), slept, "遅れているのに寝た");
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(2) else {
            panic!()
        };
        assert!(r.late && r.head_calls == 1 && r.sleeps == 0);
        ring.reset();
    }

    /// 1 フレーム進める: 先頭 → 作業 `work` カウント → Present の直前（待ちがあれば待つ）→ Present の前後の印。先頭の答えと、Present の直前のフックを呼ぶ前の時刻を返す。
    fn vpatch_frame(p: &mut Pacer, c: &mut FakeClock, ring: &PaceRing, work: u64) -> (u32, u64) {
        let code = p.head(c, ring);
        c.now += work;
        let entry = c.now;
        p.before_present(c);
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        (code, entry)
    }

    // 何が壊れたら落ちるか: vpatch の先頭のフックが「普通の待機タイマーで目標の 1 ms 手前まで 1 回だけ寝て、メッセージループへ戻さず（NOT_YET を返さず）、期限 − 準備時間（下限 3 ms）ちょうどに始める」こと。
    // 高分解能の寝かた（sleep_us）に替える・NOT_YET で戻す形に替える・準備時間を引かない、のどれでも落ちる。
    #[test]
    fn vpatch_head_sleeps_once_with_the_plain_timer_and_starts_at_deadline_minus_prep() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(qpf, &cfg(Mode::Vpatch)).unwrap();
        let mut c = FakeClock::at(1_000_000, 3);
        let (code, base) = vpatch_frame(&mut p, &mut c, ring, 0);
        assert_eq!(code, HEAD_RUN);
        assert!(
            c.plain.is_empty() && c.slept.is_empty(),
            "最初の先頭のフックは基点を置くだけ"
        );
        let PaceRead::Row(r0) = ring.read(0) else {
            panic!()
        };
        assert!(
            r0.rebased && !r0.late,
            "最初の 1 回は置き直しだが遅れではない"
        );
        let t1 = base + qpf / 60;
        let wake = t1 - 30_000;
        c.now += 50_000;
        let entry = c.now;
        assert_eq!(
            p.head(&mut c, ring),
            HEAD_RUN,
            "先頭のフックの中で待ち、メッセージループへ戻さない"
        );
        let spin = pace::us_to_ticks_ceil(SPIN_US, qpf);
        assert_eq!(c.plain, vec![((wake - spin - entry) / 10) as u32]);
        assert!(c.slept.is_empty(), "高分解能の寝かたを使った");
        // 作業 1 ms の後、Present の直前で期限まで待つ（目標の 1 ms 手前まで普通のタイマーで寝る）。
        c.now += 10_000;
        let before = c.now;
        p.before_present(&mut c);
        assert_eq!(c.plain.len(), 2);
        assert_eq!(c.plain[1], ((t1 - spin - before) / 10) as u32);
        assert!(
            c.now >= t1 && c.now - t1 < 10,
            "期限ちょうどに出る: {}",
            c.now - t1
        );
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert_eq!(r.deadline, wake, "枠の期限の欄は起きる時刻");
        assert!(
            r.start >= wake && r.start - wake < 10,
            "起きる時刻ちょうどに始める"
        );
        assert_eq!((r.head_calls, r.sleeps), (1, 1));
        assert!(!r.late && !r.rebased && r.paced);
        ring.reset();
    }

    // 何が壊れたら落ちるか: 準備時間が「起きてから Present の直前まで」の標本で伸びること。作業が 8 ms かかった次のフレームは、期限の 8 ms 前に起きる（下限 3 ms のまま固定すると落ちる）。
    #[test]
    fn vpatch_prep_follows_the_measured_work() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let period = qpf / 60;
        let mut p = Pacer::new(qpf, &cfg(Mode::Vpatch)).unwrap();
        let mut c = FakeClock::at(1_000_000, 3);
        let (_, base) = vpatch_frame(&mut p, &mut c, ring, 0);
        c.now += 10_000;
        // 作業 8 ms（起きてから 8 ms 後に Present の直前。期限は起きた 3 ms 後なので過ぎている）。
        let (code, at) = vpatch_frame(&mut p, &mut c, ring, 80_000);
        assert_eq!(code, HEAD_RUN);
        assert!(at > base + period, "作業が期限を越えた");
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert_eq!(
            r.deadline,
            base + period - 30_000,
            "標本が無い間は下限 3 ms"
        );
        let (code, _) = vpatch_frame(&mut p, &mut c, ring, 0);
        assert_eq!(code, HEAD_RUN);
        let PaceRead::Row(r) = ring.read(2) else {
            panic!()
        };
        // 標本 = 起きた時刻（偽の時計は読むたびに 3 カウント進む）から Present の直前のフックまで = 作業 80,000 + 3。
        assert_eq!(
            r.deadline,
            base + 2 * qpf / 60 - 80_003,
            "期限 − 8 ms で起きる"
        );
        ring.reset();
    }

    // 何が壊れたら落ちるか: 先頭で期限を過ぎていたら待たずに始め、基点を今（Present の時刻）に置き直すこと。位相を保つ B と同じ「元の格子の次の点」に合わせると落ちる。
    #[test]
    fn vpatch_late_head_runs_at_once_and_moves_the_base() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let period = qpf / 60;
        let mut p = Pacer::new(qpf, &cfg(Mode::Vpatch)).unwrap();
        let mut c = FakeClock::at(1_000_000, 3);
        let (_, base) = vpatch_frame(&mut p, &mut c, ring, 0);
        // 次の期限（base + period）を 5 ms 過ぎて先頭へ来る。
        c.now = base + period + 50_000;
        let plain = c.plain.len();
        let (code, at) = vpatch_frame(&mut p, &mut c, ring, 1_000);
        assert_eq!(code, HEAD_RUN);
        assert_eq!(c.plain.len(), plain, "遅れているのに寝た");
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert!(r.late && r.rebased, "{r:?}");
        assert!(r.sleeps == 0 && r.head_calls == 1);
        // 次のフレームの期限は、置き直した Present の時刻から 1 周期。元の格子（base + 2 × period）ではない。
        c.now += 10_000;
        p.head(&mut c, ring);
        c.now += 1_000;
        p.before_present(&mut c);
        assert!(
            c.now >= at + period && c.now < at + period + 10,
            "待ちの終わり {} は {at} + 1 周期",
            c.now
        );
        assert_ne!(at + period, base + 2 * period);
        ring.reset();
    }

    // 何が壊れたら落ちるか: vpatch 以外の方式では Present の直前のフックが何もしない（時計を読まない・寝ない）こと。既定の経路に待ちが紛れ込むと落ちる。
    #[test]
    fn before_present_does_nothing_outside_vpatch() {
        for mode in [Mode::B, Mode::BSpin, Mode::BHybrid, Mode::Cycle, Mode::Auto] {
            let mut p = Pacer::new(10_000_000, &cfg(mode)).unwrap();
            let mut c = FakeClock::at(5_000, 3);
            p.before_present(&mut c);
            assert_eq!(c.now, 5_000, "{mode:?}: 時計を読んだ");
            assert!(c.plain.is_empty() && c.slept.is_empty() && c.yields == 0);
        }
    }

    #[test]
    fn glide_request_moves_the_deadline_once_per_frame_before_the_decision() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(qpf, &cfg(Mode::B)).unwrap();
        assert_eq!(glide().frame_us(), 16_666, "ペースから決まる 1 周期（µs）");
        let mut c = FakeClock::at(1_000_000, 3);
        assert_eq!(p.head(&mut c, ring), HEAD_RUN);
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let before = p.pace().grid().unwrap().at(1);
        glide().request(10_000);
        c.now += 50_000;
        let mut heads = 0;
        loop {
            heads += 1;
            if p.head(&mut c, ring) == HEAD_RUN {
                break;
            }
        }
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        assert!(heads >= 5, "寝て戻る先頭が複数ある実行でない: {heads}");
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert_eq!(
            r.deadline,
            before + 1_666,
            "期限が 1 回だけ 1,666 カウント遅れる"
        );
        assert!(!r.late, "寄せで遅れて始めた扱いになった");
        assert_eq!(
            (r.glide_req_ppm, r.glide_moved),
            (10_000, 1_666),
            "枠 1 の寄せの記録"
        );
        let PaceRead::Row(r0) = ring.read(0) else {
            panic!()
        };
        assert_eq!(
            (r0.glide_req_ppm, r0.glide_moved),
            (0, 0),
            "要求の無い枠 0 に値が載った"
        );
        assert_eq!(glide().take(), 0, "読んだ要求が残っている");
        let d2 = p.pace().grid().unwrap().at(2);
        c.now += 50_000;
        while p.head(&mut c, ring) != HEAD_RUN {}
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(2) else {
            panic!()
        };
        assert_eq!(r.deadline, d2, "要求が無いのに期限が動いた");
        assert_eq!(
            (r.glide_req_ppm, r.glide_moved),
            (0, 0),
            "要求が無い枠に前の枠の値が持ち越された"
        );
        glide().request(-4_000);
        let d3 = p.pace().grid().unwrap().at(3);
        c.now += 50_000;
        while p.head(&mut c, ring) != HEAD_RUN {}
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(3) else {
            panic!()
        };
        assert_eq!(
            r.deadline,
            d3 - 665,
            "-4,000 ppm = 666.67 カウントに、前の端数 +0.67 を足して 0 へ丸めた 665 カウント早める"
        );
        assert_eq!(
            (r.glide_req_ppm, r.glide_moved),
            (-4_000, -665),
            "負の要求と動かした量が符号つきで載る"
        );
        let l = glide().line(qpf);
        assert!(l.contains("寄せた枠 2 回"), "{l}");
        assert!(l.contains("寄せ幅の最大 10000 ppm"), "{l}");
        assert!(
            l.contains("絶対値の合計 0.2 ms") && l.contains("正味 +0.1 ms"),
            "{l}"
        );
        ring.reset();
    }

    #[test]
    fn glide_request_is_dropped_in_the_game_phase_not_kept_for_later() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(qpf, &cfg(Mode::Auto)).unwrap();
        let mut c = FakeClock::at(1_000_000, 3);
        c.screen = Screen::Fullscreen;
        glide().request(8_000);
        assert_eq!(p.head(&mut c, ring), HEAD_FALLBACK);
        assert_eq!(glide().take(), 0, "ゲーム自身の区間で要求を溜めた");
        assert!(
            p.pace().grid().is_none(),
            "格子を持たない区間で格子ができた"
        );
        let l = glide().line(qpf);
        assert!(l.contains("寄せた枠 0 回"), "{l}");
        ring.on_present_begin(c.now() + 10, 0, 0);
        ring.on_present_end(c.now() + 20);
        let PaceRead::Row(r) = ring.read(0) else {
            panic!()
        };
        assert_eq!(
            (r.glide_req_ppm, r.glide_moved),
            (8_000, 0),
            "ゲーム自身の区間で捨てた要求の記録"
        );
        c.screen = Screen::Windowed;
        glide().request(8_000);
        assert_ne!(
            p.head(&mut c, ring),
            HEAD_FALLBACK,
            "ウィンドウモードの区間へ移っていない"
        );
        let l = glide().line(qpf);
        assert!(l.contains("寄せ幅が 0 でなかった枠は 0 回"), "{l}");
        ring.on_present_begin(c.now() + 30, 0, 0);
        ring.on_present_end(c.now() + 40);
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert_eq!(
            (r.glide_req_ppm, r.glide_moved),
            (8_000, 0),
            "格子が無い枠の記録"
        );
        ring.reset();
    }

    /// 偽の時計で 1 フレームを回す。game の区間（先頭が `HEAD_FALLBACK`）はゲーム自身の経路が 1/60 秒ごとに進めることにして時計を自分で進め、b / b-spin の区間は先頭が待つ。どのフレームも作業 2 ms の後に入力と Present。
    fn run_one(p: &mut Pacer, c: &mut FakeClock, ring: &PaceRing, n: u32) {
        let mut calls = 0;
        let code = loop {
            let r = p.head(c, ring);
            calls += 1;
            assert!(calls < 100_000, "{n}: 始まらない");
            if r != HEAD_NOT_YET {
                break r;
            }
        };
        c.now += 20_000;
        ring.on_input(c.now, n, c.cpu, 1_000 + n as u64);
        ring.on_present_begin(c.now + 3_000, c.cpu, 2_000 + n as u64);
        ring.on_present_end(c.now + 3_500);
        if code == HEAD_FALLBACK {
            c.now += 166_667 - 20_000;
        }
    }

    fn run_frames(p: &mut Pacer, c: &mut FakeClock, ring: &PaceRing, frames: u32) -> Vec<FrameRow> {
        for n in 0..frames {
            run_one(p, c, ring, n);
        }
        (0..frames)
            .map(|n| match ring.read(n) {
                PaceRead::Row(r) => r,
                other => panic!("{n}: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn cycle_pacer_switches_phases_at_frame_boundaries() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(
            qpf,
            &PacingIni {
                cycle_secs: 1,
                ..cfg(Mode::Cycle)
            },
        )
        .unwrap();
        let mut c = FakeClock::at(1_000_000_000, 3);
        c.cpu = 7;
        let rows = run_frames(&mut p, &mut c, ring, 300);
        let mut blocks: Vec<(Phase, usize, usize)> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let ph = r.phase.expect("札が無い");
            match blocks.last_mut() {
                Some(b) if b.0 == ph => b.2 = i,
                _ => blocks.push((ph, i, i)),
            }
        }
        let order: Vec<Phase> = blocks.iter().map(|b| b.0).collect();
        assert_eq!(
            &order[..5],
            &[Phase::Game, Phase::B, Phase::BSpin, Phase::Game, Phase::B],
            "{blocks:?}"
        );
        for b in &blocks[..4] {
            let len = b.2 - b.1 + 1;
            assert!((58..=62).contains(&len), "区間の長さ {len}: {blocks:?}");
        }
        let (b0, bs0) = (blocks[1].1, blocks[2].1);
        for r in &rows[..b0] {
            assert!(
                !r.paced && r.sleeps == 0 && r.head_calls == 1,
                "game: {r:?}"
            );
            assert_eq!(r.head_cpu, Some(7));
        }
        for i in [b0, blocks[4].1] {
            let first_b = &rows[i];
            assert!(first_b.paced && !first_b.late, "{i}: {first_b:?}");
            assert_eq!(first_b.skipped, 0, "{i}: 格子を置き直していない");
            assert_eq!(first_b.start, first_b.deadline, "{i}");
        }
        for r in &rows[b0 + 1..bs0] {
            assert!(r.paced && r.sleeps > 0 && !r.late, "b: {r:?}");
            assert_eq!(r.head_cpu, Some(7));
        }
        let (last_b, first_bs) = (&rows[bs0 - 1], &rows[bs0]);
        let step = first_bs.deadline - last_b.deadline;
        assert!(
            step == 166_666 || step == 166_667,
            "b → b-spin で格子が変わった: {step}"
        );
        for r in &rows[bs0..blocks[2].2 + 1] {
            assert!(
                r.paced && r.sleeps == 0 && r.head_calls > 1,
                "b-spin: {r:?}"
            );
        }
        assert!(c.yields > 0);
        for r in &rows[blocks[3].1..blocks[3].2 + 1] {
            assert!(!r.paced, "{r:?}");
        }
        ring.reset();
    }

    #[test]
    fn cycle_phase_does_not_change_inside_a_frame() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(
            qpf,
            &PacingIni {
                cycle_secs: 1,
                ..cfg(Mode::Cycle)
            },
        )
        .unwrap();
        let mut c = FakeClock::at(0, 3);
        let rows = run_frames(&mut p, &mut c, ring, 62);
        let b0 = rows.iter().position(|r| r.phase == Some(Phase::B)).unwrap();
        let boundary = rows[b0].first_head + qpf;
        let mut n = rows.len() as u32;
        while p.pace().clone().deadline(c.now) < boundary {
            run_one(&mut p, &mut c, ring, n);
            n += 1;
            assert!(n < 200, "期限が境目に届かない");
        }
        assert_eq!(p.pace().clone().deadline(c.now), boundary);
        c.now = boundary - 15_000;
        c.overshoot = 30_000;
        let mut codes = Vec::new();
        loop {
            let r = p.head(&mut c, ring);
            codes.push(r);
            if r == HEAD_RUN {
                break;
            }
            assert!(codes.len() < 1_000);
        }
        c.overshoot = 700;
        assert_eq!(codes, vec![HEAD_NOT_YET, HEAD_RUN]);
        assert!(c.now > boundary, "区間の境目をまたいでいない");
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(n) else {
            panic!()
        };
        assert_eq!(r.phase, Some(Phase::B), "{r:?}");
        assert!(r.sleeps == 1 && r.late, "{r:?}");
        assert_eq!(c.yields, 0);
        let _ = p.head(&mut c, ring);
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(n + 1) else {
            panic!()
        };
        assert_eq!(r.phase, Some(Phase::BSpin));
        ring.reset();
    }

    #[test]
    fn auto_pacer_follows_the_screen_each_frame() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let mut p = Pacer::new(10_000_000, &cfg(Mode::Auto)).unwrap();
        let mut c = FakeClock::at(1_000_000_000, 3);
        let read = |n: u32| match ring.read(n) {
            PaceRead::Row(r) => r,
            other => panic!("{n}: {other:?}"),
        };
        let plan = |n: u32| match n {
            0..30 => Screen::Fullscreen,
            30..40 => Screen::Unknown,
            40..100 => Screen::Windowed,
            _ => Screen::Fullscreen,
        };
        let mut heads_in_windowed = 0u32;
        for n in 0..110 {
            c.screen = plan(n);
            let before = c.screen_reads;
            run_one(&mut p, &mut c, ring, n);
            assert_eq!(c.screen_reads - before, 1, "{n}: 1 フレームに 1 回だけ読む");
            let r = read(n);
            heads_in_windowed += if plan(n) == Screen::Windowed {
                r.head_calls
            } else {
                0
            };
        }
        for n in (0..40).chain(100..110) {
            let r = read(n);
            assert_eq!(r.phase, Some(Phase::Game), "{n}: {r:?}");
            assert!(!r.paced && r.sleeps == 0 && r.head_calls == 1, "{n}: {r:?}");
        }
        let first = read(40);
        assert_eq!(first.phase, Some(Phase::BSpin), "{first:?}");
        assert!(
            first.paced && !first.late && first.skipped == 0,
            "{first:?}"
        );
        assert_eq!(first.start, first.deadline);
        for n in 41..100 {
            let r = read(n);
            assert_eq!(r.phase, Some(Phase::BSpin), "{n}: {r:?}");
            assert!(
                r.paced && r.sleeps == 0 && r.head_calls > 1 && !r.late,
                "{n}: {r:?}"
            );
        }
        assert!(
            c.slept.is_empty(),
            "ウィンドウモードで待機タイマーを使った: {:?}",
            c.slept
        );
        assert!(c.yields > 0 && heads_in_windowed > 60);
        ring.reset();
    }

    #[test]
    fn b_spin_and_cycle_write_the_head_and_tell_the_writer() {
        for (ini, mode, secs, line) in [
            (
                "[pacing]\nmode = b-spin\n",
                Mode::BSpin,
                None,
                "ペース: 形は b-spin（B と同じ格子で",
            ),
            (
                "[pacing]\nmode = cycle\ncycle_secs = 45\n",
                Mode::Cycle,
                Some(45u64),
                "ペース: 形は cycle（45 秒ごとに game → b → b-spin → game",
            ),
            (
                "[pacing]\nmode = vpatch\n",
                Mode::Vpatch,
                None,
                "ペース: 形は vpatch（測定専用",
            ),
        ] {
            let mut code = FakeCode::original();
            let mut host = FakeHost::with_ini(ini);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Paced, "{log}");
            let (p, h) = (site(PRESENT_SITE), site(PACING_HEAD_SITE));
            assert_eq!(
                code.writes.iter().map(|w| w.0).collect::<Vec<_>>(),
                vec![p.addr + 1, h.addr + 1]
            );
            assert!(log.contains("地点 6 行を照合して全部一致"), "{log}");
            assert_eq!(host.armed.map(|c| c.mode), Some(mode));
            assert_eq!(
                host.armed.map(|c| c.cycle_secs),
                Some(secs.unwrap_or(pace::DEFAULT_CYCLE_SECS))
            );
            let pre = host.writer_pre.clone().unwrap();
            assert_eq!((pre.mode, pre.cycle_secs), (mode, secs));
            assert_eq!(pre.eff_classes, vec![0, 1]);
            assert!(log.contains(line), "{log}");
            let mut code = FakeCode::original();
            let mut host = FakeHost {
                arm_fail: true,
                ..FakeHost::with_ini(ini)
            };
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Stamps, "{log}");
            let pre = host.writer_pre.clone().unwrap();
            assert_eq!((pre.mode, pre.cycle_secs), (Mode::Game, None));
            assert!(!log.contains("ペース: 形は"), "{log}");
        }
    }

    /// b-hybrid の先頭（偽の時計。寝過ごしは既定の 70 µs）。
    #[test]
    fn b_hybrid_head_sleeps_then_yields_then_spins() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let qpf = 10_000_000;
        let mut p = Pacer::new(qpf, &cfg(Mode::BHybrid)).unwrap();
        let margin = p.doze().margin();
        assert_eq!(
            p.doze().margin_us(),
            higanr_core::pacing_hybrid::DEFAULT_MARGIN_US
        );
        let mut c = FakeClock::at(1_000_000, 3);
        assert_eq!(p.head(&mut c, ring), HEAD_RUN);
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let d1 = p.pace().grid().unwrap().at(1);
        c.now += 50_000;
        let mut wakes = Vec::new();
        loop {
            let slept = c.slept.len();
            let r = p.head(&mut c, ring);
            if c.slept.len() > slept {
                wakes.push(c.now);
            }
            if r == HEAD_RUN {
                break;
            }
            assert_eq!(r, HEAD_NOT_YET);
            assert!(wakes.len() < 100 && c.yields < 10_000, "始まらない");
        }
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(1) else {
            panic!()
        };
        assert_eq!(r.phase, Some(Phase::BHybrid), "{r:?}");
        assert_eq!(r.deadline, d1);
        assert!(r.start >= d1 && r.start - d1 < 10, "{r:?}");
        assert!(!r.late && r.paced, "{r:?}");
        assert!((7..=9).contains(&c.slept.len()), "{:?}", c.slept);
        assert!(
            c.slept.iter().all(|us| (1..=CHUNK_US).contains(us)),
            "{:?}",
            c.slept
        );
        assert_eq!(r.sleeps as usize, c.slept.len());
        let last = *wakes.last().unwrap();
        assert!(
            last + margin <= d1 - p.spin + 1_000,
            "期限の {} カウント前まで寝た",
            d1 - last
        );
        assert!(c.yields > 100, "余裕の間に譲っていない: {}", c.yields);
        assert_eq!(r.hybrid_margin_us, 2_000);
        assert!(
            matches!(r.oversleep_us, Some(70..=71)),
            "{:?}",
            r.oversleep_us
        );
        assert!(p.doze().over_margin() == 0);
        c.now = p.pace().grid().unwrap().at(2) - 116_000;
        c.overshoot = 150_000;
        while p.head(&mut c, ring) != HEAD_RUN {}
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(2) else {
            panic!()
        };
        assert!(r.late && r.sleeps == 1, "{r:?}");
        assert!(matches!(r.oversleep_us, Some(15_000..=15_001)), "{r:?}");
        assert_eq!(p.doze().over_margin(), 1);
        assert_eq!(
            p.doze().margin_us(),
            higanr_core::pacing_hybrid::MAX_MARGIN_US
        );
        c.overshoot = 700;
        let g = p.pace().grid().unwrap();
        c.now = g.at(g.first_after(r.start)) - 116_000;
        while p.head(&mut c, ring) != HEAD_RUN {}
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(3) else {
            panic!()
        };
        assert!(!r.late && r.sleeps > 0, "{r:?}");
        assert_eq!(r.hybrid_margin_us, 8_000, "{r:?}");
        c.now += 400_000;
        assert_eq!(p.head(&mut c, ring), HEAD_RUN);
        ring.on_present_begin(c.now, 0, 0);
        ring.on_present_end(c.now + 1);
        let PaceRead::Row(r) = ring.read(4) else {
            panic!()
        };
        assert!(r.late && r.sleeps == 0, "{r:?}");
        assert_eq!((r.hybrid_margin_us, r.oversleep_us), (0, None), "{r:?}");
        ring.reset();
    }

    #[test]
    fn b_hybrid_falls_back_to_b_spin_after_late_frames() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let cfg = PacingIni {
            hybrid: HybridParams {
                fallback_count: 2,
                ..HybridParams::DEFAULT
            },
            ..cfg(Mode::BHybrid)
        };
        let mut p = Pacer::new(10_000_000, &cfg).unwrap();
        let mut c = FakeClock::at(1_000_000_000, 3);
        c.overshoot = 150_000;
        let rows = run_frames(&mut p, &mut c, ring, 12);
        for (n, r) in rows.iter().enumerate().take(3).skip(1) {
            assert_eq!(r.phase, Some(Phase::BHybrid), "{n}: {r:?}");
            assert!(r.late && r.sleeps > 0, "{n}: {r:?}");
        }
        assert!(p.doze().fell_back());
        for (n, r) in rows.iter().enumerate().skip(3) {
            assert_eq!(r.phase, Some(Phase::BSpin), "{n}: {r:?}");
            assert!(r.sleeps == 0 && !r.late, "{n}: {r:?}");
            assert_eq!(
                (r.hybrid_margin_us, r.oversleep_us),
                (0, None),
                "{n}: {r:?}"
            );
        }
        ring.reset();
    }

    #[test]
    fn auto_window_and_cycle_phases_select_b_hybrid() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let auto = PacingIni {
            auto_window: Phase::BHybrid,
            ..cfg(Mode::Auto)
        };
        let mut p = Pacer::new(10_000_000, &auto).unwrap();
        let mut c = FakeClock::at(1_000_000_000, 3);
        for n in 0..40 {
            c.screen = if n < 20 {
                Screen::Windowed
            } else {
                Screen::Fullscreen
            };
            run_one(&mut p, &mut c, ring, n);
        }
        for n in 0..40 {
            let PaceRead::Row(r) = ring.read(n) else {
                panic!("{n}")
            };
            let want = if n < 20 { Phase::BHybrid } else { Phase::Game };
            assert_eq!(r.phase, Some(want), "{n}: {r:?}");
            if (1..20).contains(&n) {
                assert!(r.sleeps > 0 && r.hybrid_margin_us > 0, "{n}: {r:?}");
            }
        }
        ring.reset();
        let cyc = PacingIni {
            cycle_secs: 1,
            cycle_phases: pace::CyclePhases::new(&[Phase::BSpin, Phase::BHybrid]).unwrap(),
            ..cfg(Mode::Cycle)
        };
        let mut p = Pacer::new(10_000_000, &cyc).unwrap();
        let mut c = FakeClock::at(1_000_000_000, 3);
        let rows = run_frames(&mut p, &mut c, ring, 150);
        let phases: Vec<Phase> = rows.iter().map(|r| r.phase.unwrap()).collect();
        assert_eq!(phases[0], Phase::BSpin);
        assert!(phases.contains(&Phase::BHybrid), "{phases:?}");
        assert!(!phases.contains(&Phase::Game) && !phases.contains(&Phase::B));
        for (n, r) in rows.iter().enumerate().skip(1) {
            match r.phase.unwrap() {
                Phase::BSpin => assert!(r.sleeps == 0 && r.hybrid_margin_us == 0, "{n}: {r:?}"),
                _ => assert!(r.sleeps > 0, "{n}: {r:?}"),
            }
        }
        ring.reset();
    }

    #[test]
    fn b_hybrid_forms_write_the_head_and_tell_the_margin() {
        for (ini, mode, line) in [
            (
                "[pacing]\nmode = b-hybrid\nhybrid_margin_us = 3000\n",
                Mode::BHybrid,
                "ペース: 形は b-hybrid（B と同じ格子で、残りが 1000 µs + 余裕より多い間は待機タイマーで 1 回最大 1000 µs 寝て",
            ),
            (
                "[pacing]\nauto_window = b-hybrid\nhybrid_margin_us = 3000\n",
                Mode::Auto,
                "ペース: 形は auto（フレームの最初の先頭のフックで画面の形を読み、ウィンドウなら b-hybrid（ini の auto_window）",
            ),
            (
                "[pacing]\nmode = cycle\ncycle_phases = b-spin, b-hybrid\nhybrid_margin_us = 3000\n",
                Mode::Cycle,
                "ペース: 形は cycle（30 秒ごとに b-spin → b-hybrid → b-spin … と入れ替える",
            ),
        ] {
            let mut code = FakeCode::original();
            let mut host = FakeHost::with_ini(ini);
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Paced, "{log}");
            assert!(log.contains("地点 6 行を照合して全部一致"), "{log}");
            let armed = host.armed.unwrap();
            assert_eq!(
                (armed.mode, armed.hybrid.margin_us),
                (mode, 3_000),
                "{ini:?}"
            );
            assert!(armed.uses_hybrid(), "{ini:?}");
            assert_eq!(host.writer_pre.clone().unwrap().mode, mode);
            assert!(log.contains(line), "{log}");
            assert!(
                log.contains(
                    "余裕は 3000 µs から、寝過ごしの山 + 250 µs まで広げる（上限 8000 µs。山は寝るたびに 1/2^10 減る）。寝た後に期限を越えた枠が 60 枠の中で 10 回に達したら、その走りの残りは b-spin に戻す"
                ),
                "{log}"
            );
        }
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini(
            "[pacing]\nmode = b-hybrid\nhybrid_spin_us = 1500\nhybrid_chunk_us = 500\nhybrid_fallback_count = 0\n",
        );
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        let h = host.armed.unwrap().hybrid;
        assert_eq!(
            (h.spin_us, h.chunk_us, h.fallback_count, h.margin_us),
            (1_500, 500, 0, 2_000)
        );
        for want in [
            "残りが 1500 µs + 余裕より多い間は待機タイマーで 1 回最大 500 µs 寝て",
            "最後の 1500 µs は QPC を読み続ける",
            "b-spin へは戻さない",
            "★hybrid_chunk_us = 500、★hybrid_spin_us = 1500、★hybrid_fallback_count = 0",
            "hybrid_margin_us = 2000、",
        ] {
            assert!(log.contains(want), "{want}: {log}");
        }
        let mut code = FakeCode::original();
        let mut host = FakeHost::with_ini("");
        let (r, log) = run(&mut code, &mut host);
        assert_eq!(r, Installed::Paced, "{log}");
        assert_eq!(host.armed.unwrap().auto_window, Phase::BSpin);
        assert!(
            log.contains(
                "ペース: 形は auto（既定。フレームの最初の先頭のフックで画面の形を読み、ウィンドウなら b-spin"
            ),
            "{log}"
        );
        assert!(!log.contains("b-hybrid"), "{log}");
    }

    #[test]
    fn missing_cpu_classes_only_empty_the_core_columns() {
        for (classes, want) in [
            (None, "ペース: 効率クラスを読めない（合成: 表が無い）"),
            (Some(CpuClasses::default()), "ペース: 効率クラスの表が 0 件"),
        ] {
            let mut code = FakeCode::original();
            let mut host = FakeHost {
                classes,
                ..FakeHost::with_ini(INI_B)
            };
            let (r, log) = run(&mut code, &mut host);
            assert_eq!(r, Installed::Paced, "{log}");
            assert!(log.contains(want), "{log}");
            assert_eq!(
                host.writer_pre.clone().unwrap().eff_classes,
                Vec::<u8>::new()
            );
            assert_eq!(host.writer_classes, Some(CpuClasses::default()));
        }
    }

    #[test]
    fn call_rows_are_head_and_present_only() {
        let calls: Vec<&str> = PACING_SITES
            .iter()
            .filter(|d| matches!(d.kind, Use::CallRel32 { .. }))
            .map(|d| d.name)
            .collect();
        assert_eq!(calls, vec![PACING_HEAD_SITE, PRESENT_SITE]);
    }

    #[test]
    fn ini_samples_select_game_and_b_without_the_net() {
        for (text, mode, fps) in [
            (
                include_str!("../../../scripts/m7_pacing_game.ini"),
                Mode::Game,
                60,
            ),
            (
                include_str!("../../../scripts/m7_pacing_b.ini"),
                Mode::B,
                60,
            ),
            (
                include_str!("../../../scripts/m7_pacing_b_240.ini"),
                Mode::B,
                240,
            ),
            (
                include_str!("../../../scripts/m7_pacing_cycle.ini"),
                Mode::Cycle,
                60,
            ),
            (
                include_str!("../../../scripts/m7_pacing_vpatch.ini"),
                Mode::Vpatch,
                60,
            ),
        ] {
            let ini = higanr_core::ini::parse(text).unwrap();
            assert_eq!(
                pace::mode_from_ini(&ini),
                Ok(pace::PacingIni {
                    mode,
                    fps,
                    cycle_secs: pace::DEFAULT_CYCLE_SECS,
                    ..pace::PacingIni::DEFAULT
                })
            );
            assert_eq!(crate::net_config::from_text(text), Ok(None));
        }
    }

    struct SpyHost(std::rc::Rc<core::cell::RefCell<Vec<&'static str>>>);

    impl PacingHost for SpyHost {
        fn read_ini(&mut self) -> Result<Option<String>, String> {
            self.0.borrow_mut().push("read_ini");
            Ok(None)
        }
        fn qpf(&self) -> u64 {
            10_000_000
        }
        fn head_entry(&self) -> u32 {
            HEAD_ENTRY
        }
        fn present_entry(&self) -> u32 {
            PRESENT_ENTRY
        }
        fn arm_pacer(
            &mut self,
            _qpf: u64,
            _cfg: &PacingIni,
            _flag: u32,
            _w: u32,
        ) -> Result<String, String> {
            unreachable!()
        }
        fn cpu_classes(&mut self) -> Result<CpuClasses, String> {
            unreachable!()
        }
        fn start_writer(
            &mut self,
            _pre: Preamble,
            _fps: u64,
            _classes: CpuClasses,
        ) -> Result<String, String> {
            unreachable!()
        }
        fn stop_writer(&mut self) -> Option<Result<WriterEnd, String>> {
            self.0.borrow_mut().push("stop_writer");
            None
        }
        fn set_present_original(&mut self, _original: u32) {}
        fn set_head_targets(&mut self, _o: u32, _r: u32, _e: u32) {}
        fn start_stamps(&mut self) {}
        fn counters(&self) -> PaceCounters {
            PaceCounters::default()
        }
    }

    struct PacingOnlyScript(std::rc::Rc<core::cell::RefCell<Vec<&'static str>>>);

    impl crate::session::ScriptHost for PacingOnlyScript {
        fn read_script(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn move_script(&mut self) -> Result<String, String> {
            unreachable!()
        }
        fn prepare_shots(&mut self) -> Result<(), String> {
            unreachable!()
        }
        fn arm(&mut self, _s: higanr_core::script::Script, _f: u32, _l: u32) -> Result<(), String> {
            unreachable!()
        }
        fn shots_on(&mut self) {}
        fn start(&mut self) -> Result<(), String> {
            unreachable!()
        }
        fn pacing_host(&mut self) -> Option<Box<dyn PacingHost>> {
            self.0.borrow_mut().push("pacing_host");
            Some(Box::new(SpyHost(std::rc::Rc::clone(&self.0))))
        }
    }

    #[test]
    fn session_opens_the_pacing_host_after_the_hooks_and_finishes_it() {
        use crate::hook::tests::FakeHook;
        use crate::observe::tests::FakeRecord;
        use crate::probe::tests::Fake;
        use crate::th09_addr::{FORBIDDEN, HOOK_SITE, IMAGE_BASE, SITES, WINDOW_ADDR};
        for installs in [true, false] {
            let events = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
            let mut env = Fake::all_match();
            if !installs {
                env.ident = crate::probe::IdentOutcome::Unreadable("合成".to_string());
            }
            let def = SITES.iter().find(|d| d.name == HOOK_SITE).unwrap();
            let mut h = FakeHook::new(2);
            h.mem_base = def.addr;
            h.mem[..5].copy_from_slice(def.expected);
            let mut rec = FakeRecord::new(Vec::new(), 0);
            let mut script = PacingOnlyScript(std::rc::Rc::clone(&events));
            let mut out = Vec::new();
            let end = crate::session::run(
                &mut env,
                &mut h,
                &mut rec,
                &mut script,
                SITES,
                FORBIDDEN,
                IMAGE_BASE,
                crate::session::Sites {
                    hook_site: HOOK_SITE,
                    draw_site: "draw_before_endscene",
                    exit_site: "render_only_exit",
                },
                WINDOW_ADDR,
                &crate::ticks::COPY_TABLE,
                2,
                &mut out,
            );
            let log = String::from_utf8(out).unwrap();
            if installs {
                assert_eq!(end, crate::session::End::Installed, "{log}");
                assert_eq!(
                    *events.borrow(),
                    vec!["pacing_host", "read_ini", "stop_writer"],
                    "{log}"
                );
                let hook_at = log.find("フックの設置: 書いた").expect("フックを書いた行");
                let pace_at = log
                    .find("ペース: HiganRTool.ini が無い")
                    .expect("ペースの行");
                let summary_at = log.find("計測の集計").expect("計測の行");
                assert!(hook_at < pace_at && pace_at < summary_at, "{log}");
                let block = &log[log.rfind("==== 試験のまとめ ====\n").expect("まとめ")..];
                assert!(
                    block.contains("× フレームのペース: ペースのフックを入れなかった"),
                    "{block}"
                );
            } else {
                assert_eq!(end, crate::session::End::NotInstalled);
                assert!(events.borrow().is_empty(), "{:?}", events.borrow());
            }
        }
    }

    #[test]
    fn writer_limits_csv_is_660_seconds_in_the_dist_build_and_the_thread_outlives_it() {
        assert_eq!(
            writer_limits(true),
            WriterLimits {
                csv_seconds: 660,
                thread_seconds: 43_200
            }
        );
        assert_eq!(
            writer_limits(false),
            WriterLimits {
                csv_seconds: 1_260,
                thread_seconds: 43_200
            }
        );
        assert_eq!(
            WRITER_CSV_MAX_SECONDS_DEV,
            u64::from(crate::observe::SCRIPT_MAX_SECONDS) + 60
        );
        assert_eq!(WRITER_THREAD_MAX_SECONDS, 12 * 60 * 60);
        for dist in [false, true] {
            let l = writer_limits(dist);
            assert!(l.thread_seconds > l.csv_seconds, "{l:?}");
        }
    }

    #[test]
    fn writer_loop_keeps_summarizing_after_the_csv_limit() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let mut buf = Vec::new();
        let made = core::cell::Cell::new(0u64);
        let t = writer_loop(
            ring,
            &Preamble::new(10_000_000, Mode::B),
            240,
            &CpuClasses::default(),
            &mut buf,
            &|| made.get() >= 1_000,
            &mut |_| {
                let i = made.get();
                ring.on_present_begin(1_000 + i * 1_000, 0, 0);
                ring.on_present_end(1_500 + i * 1_000);
                made.set(i + 1);
            },
            WriterLimits {
                csv_seconds: 1,
                thread_seconds: 3,
            },
        );
        assert_eq!(
            made.get(),
            30,
            "ループの上限（3 秒 = 30 回）で止まらなかった"
        );
        assert_eq!((t.rows, t.csv_capped, t.lost), (30, 21, 0));
        assert_eq!(t.report.all.rows, 30, "集計が CSV の上限の後で止まった");
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(
            text.lines().count(),
            2 + 9,
            "CSV の上限の後も行を書いた:\n{text}"
        );
        assert!(!t.write_failed);
        ring.reset();
    }

    #[test]
    fn writer_loop_stops_at_the_thread_limit_without_a_stop() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let reads = core::cell::Cell::new(0u32);
        let _t = writer_loop(
            ring,
            &Preamble::new(10_000_000, Mode::B),
            240,
            &CpuClasses::default(),
            &mut Vec::new(),
            &|| reads.get() >= 1_000,
            &mut |_| reads.set(reads.get() + 1),
            WriterLimits {
                csv_seconds: 1,
                thread_seconds: 1,
            },
        );
        assert_eq!(reads.get(), 10, "スレッドの上限で止まらなかった");
        ring.reset();
    }

    #[test]
    fn writer_loop_keeps_summarizing_past_the_script_length_until_stopped() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let made = core::cell::Cell::new(0u64);
        let t = writer_loop(
            ring,
            &Preamble::new(10_000_000, Mode::B),
            240,
            &CpuClasses::default(),
            &mut std::io::sink(),
            &|| made.get() >= 13_000,
            &mut |_| {
                let i = made.get();
                ring.on_present_begin(1_000 + i * 1_000, 0, 0);
                ring.on_present_end(1_500 + i * 1_000);
                made.set(i + 1);
            },
            writer_limits(false),
        );
        assert_eq!(made.get(), 13_000, "止める旗の前に読むのをやめた");
        assert_eq!((t.rows, t.lost), (13_000, 0));
        assert_eq!(t.report.all.rows, 13_000, "1260 秒で集計が止まった");
        assert_eq!(t.csv_capped, 401);
        ring.reset();
    }

    #[test]
    fn writer_loop_safety_net_is_twelve_hours_without_a_stop() {
        let _g = lock();
        let ring = &PACE_RING;
        ring.reset();
        let reads = core::cell::Cell::new(0u64);
        let _t = writer_loop(
            ring,
            &Preamble::new(10_000_000, Mode::B),
            240,
            &CpuClasses::default(),
            &mut std::io::sink(),
            &|| reads.get() >= 10_000_000,
            &mut |_| reads.set(reads.get() + 1),
            writer_limits(cfg!(feature = "dist")),
        );
        assert_eq!(
            reads.get(),
            12 * 60 * 60 * 10,
            "12 時間の安全網で止まらなかった"
        );
        ring.reset();
    }

    #[test]
    fn writer_loop_report_says_when_the_quantiles_are_approximate() {
        use higanr_core::pacing::SERIES_CAP;
        let _g = lock();
        let ring = &PACE_RING;
        for (frames, thinned) in [
            (SERIES_CAP as u64 / 2, false),
            (SERIES_CAP as u64 + 5_000, true),
        ] {
            ring.reset();
            let made = core::cell::Cell::new(0u64);
            let t = writer_loop(
                ring,
                &Preamble::new(10_000_000, Mode::B),
                60,
                &CpuClasses::default(),
                &mut std::io::sink(),
                &|| made.get() >= frames,
                &mut |_| {
                    for _ in 0..1_000 {
                        let i = made.get();
                        ring.on_present_begin(1_000 + i * 166_667, 0, 0);
                        ring.on_present_end(1_500 + i * 166_667);
                        made.set(i + 1);
                    }
                },
                writer_limits(false),
            );
            assert_eq!((t.rows, t.lost), (made.get(), 0));
            assert_eq!(t.report.all.rows as u64, made.get());
            let text = t.report.lines().join("\n");
            assert_eq!(text.contains("分位数は近似"), thinned, "{text}");
            assert_eq!(!t.report.all.thinned.is_empty(), thinned);
        }
        ring.reset();
    }

    #[test]
    fn finish_writes_the_approximation_line_only_when_a_column_was_thinned() {
        let go = |extra: u32| {
            let mut host = FakeHost {
                writer_extra_rows: extra,
                ..FakeHost::with_ini(INI_GAME)
            };
            let mut code = FakeCode::original();
            let _ = run(&mut code, &mut host);
            let mut out = Vec::new();
            finish(&mut host, &mut out);
            String::from_utf8(out).unwrap()
        };
        let log = go(higanr_core::pacing::SERIES_CAP as u32 + 10);
        assert!(log.contains("  分位数は近似: 列が上限 "), "{log}");
        assert!(
            log.contains("件数・最小・最大・平均・長い周期の数は全件で正確"),
            "{log}"
        );
        let log = go(100);
        assert!(!log.contains("分位数は近似"), "{log}");
    }

    #[test]
    fn pacing_gz_round_trips_and_shrinks_for_a_realistic_shape() {
        use higanr_core::gzlog::{self, GzLogWriter};
        let qpf = 10_000_000u64;
        let mut x: u32 = 0x0BAD_5EED;
        let mut rnd = move || {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            x >> 8
        };
        let pre = Preamble::new(qpf, Mode::BHybrid);
        let mut plain = format!("{}\n{}\n", pace::csv_preamble(&pre), pace::CSV_HEADER);
        let mut gz = GzLogWriter::new(Vec::new());
        gz.write_all(plain.as_bytes()).unwrap();
        let (mut prev, mut cycles) = (None::<FrameRow>, 40_000_000_000u64);
        let frames = 10 * 60 * 60u32;
        for f in 0..frames {
            let deadline = 5_000_000_000 + u64::from(f) * 166_667;
            let first_head = deadline - 100_000 + u64::from(rnd() % 20_000);
            let start = deadline + u64::from(rnd() % 400);
            let input = start + 20_000 + u64::from(rnd() % 3_000);
            let begin = input + 60_000 + u64::from(rnd() % 8_000);
            let present_cycles = cycles + 30_000_000 + u64::from(rnd() % 4_000_000);
            let r = FrameRow {
                frame: f,
                paced: true,
                deadline,
                first_head,
                head_calls: 1 + rnd() % 3,
                sleeps: rnd() % 2,
                start,
                late: rnd() % 200 == 0,
                inputs: 1,
                sup_frame: 1_000 + f,
                input,
                present_begin: begin,
                present_end: begin + 1_500 + u64::from(rnd() % 600),
                phase: Some(Phase::BHybrid),
                head_cpu: Some(rnd() % 4),
                input_cpu: Some(rnd() % 4),
                present_cpu: Some(rnd() % 4),
                head_eff: Some(1),
                input_eff: Some(1),
                present_eff: Some(1),
                input_cycles: cycles + 12_000_000 + u64::from(rnd() % 1_000_000),
                present_cycles,
                hybrid_margin_us: 500 + rnd() % 900,
                oversleep_us: (rnd() % 2 == 0).then(|| rnd() % 300),
                ..Default::default()
            };
            cycles = present_cycles;
            let line = r.csv_line(prev.as_ref(), qpf);
            plain.push_str(&line);
            plain.push('\n');
            gz.write_all(line.as_bytes()).unwrap();
            gz.write_all(b"\n").unwrap();
            prev = Some(r);
        }
        let bytes = gz.finish().unwrap();
        let back = gzlog::text_from_bytes(bytes.clone()).unwrap();
        assert!(!back.cut);
        assert!(back.text == plain, "gzip を伸ばした行が平文と違う");
        let r = bytes.len() as f64 / plain.len() as f64;
        eprintln!(
            "合成 _pacing.csv（10 分 {frames} 行）: 平文 {} B → gzip {} B（{:.1}%）",
            plain.len(),
            bytes.len(),
            r * 100.0
        );
        assert!(r < 0.5, "{r}");
    }
}
