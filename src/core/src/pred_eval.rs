//! rollback の「相手の入力ワードの予測」を、ゲームを動かさずに人の入力の記録で採点する部品（道具 `pred_eval` が使う）。
//!
//! 採点のしかた（`rollback.rs` の `predicted_word` との対応）:
//! - 片道 k tick・入力の遅れ D のとき、相手のフレーム f の入力ワードは tick f − D に取られて tick f − D + k に届く。
//!   フレーム f を進める tick f に確定しているのは f − m まで（m = max(k − D, 0)）で、`predicted_word` は
//!   `remote_next - 1`（= f − m）の語を繰り返すので、幅 m の予測は「m フレーム前の語の繰り返し」と同じ。
//! - 見える状態は、確定した最新の語（c = t − m）の後の m フレームを予測の語で進めたもの。tick ごとに進め直すので、
//!   窓（c+1..=t）の予測はその tick の確定の最新から引き直す（`post_present` の進め直しと同じ）。
//! - 外れ: tick t で使った予測（窓の最後の語）が本物の語と違う。見た目のずれ: 窓の予測の語と本物の語で位置を積分した差。
//!
//! 予測の方法 `Predictor` はゲームスレッドに置ける形（確保なし・固定の大きさの配列と整数だけ。`tests/alloc_check.rs` で確かめる）。
//! 採点側の `eval_stream` / `Pool` は道具の側で、ヒープと浮動小数を使う。
//! 語は `WORD_MASK`（送る語。`input_model::SEND_MASK`）を掛けて渡す。

use crate::input_model::SEND_MASK;
use crate::pad_dirs::{DIR_DOWN, DIR_LEFT, DIR_MASK, DIR_RIGHT, DIR_UP};

/// 採点する語のマスク。ネットワークで送る語と同じ。
pub const WORD_MASK: u16 = SEND_MASK;
/// 予測の幅の上限（rollback の `MAX_AHEAD`）。
pub const MAX_M: usize = 16;

/// 低速のビット（`0x0004`。入力語のビット）。
const BIT_SLOW: u16 = 0x0004;
const UP: u16 = DIR_UP as u16;
const DOWN: u16 = DIR_DOWN as u16;
const LEFT: u16 = DIR_LEFT as u16;
const RIGHT: u16 = DIR_RIGHT as u16;
const DIRS: u16 = DIR_MASK as u16;

/// 位置を積分するときの 1 フレームあたりの移動量（px）。16 キャラの高速 3.0〜5.5・低速 1.8〜3.5 の中ほどの代表値
/// （霊夢・咲夜は高速 4.0、霊夢・メルランは低速 2.0）。出典はゲームの `plNN.sht` の実バイト。
pub const SPEED_FAST: f64 = 4.0;
pub const SPEED_SLOW: f64 = 2.0;
/// 斜めは 1 軸ぶんが 縦横 × sin(π/4)（合成の移動量が縦横と同じ。`0x0041BBF7`〜 が読み込み直後に上書きする）。
/// 画面の端での止まりは考えない（ずれが大きめに出る）。
const DIAGONAL: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// 語 1 つで 1 フレームに動く量 (dx, dy)。左右・上下の同時押しは打ち消す。
pub fn velocity(w: u16) -> (f64, f64) {
    let dx = i32::from(w & RIGHT != 0) - i32::from(w & LEFT != 0);
    let dy = i32::from(w & DOWN != 0) - i32::from(w & UP != 0);
    let speed = if w & BIT_SLOW != 0 {
        SPEED_SLOW
    } else {
        SPEED_FAST
    };
    let k = if dx != 0 && dy != 0 { DIAGONAL } else { 1.0 };
    (f64::from(dx) * speed * k, f64::from(dy) * speed * k)
}

/// 予測の方法。確定した語を 1 つずつ `observe` で受け、直近の確定の後 m フレーム先までの語を `predict` で出す。
pub trait Predictor {
    /// 履歴（直近の語）だけ捨てる。学習した表は残す（別のマッチへ移るとき）。
    fn reset_stream(&mut self);
    fn observe(&mut self, w: u16);
    /// `out[i]` = 直近の確定の i+1 フレーム先の語。`out[..m]` だけ書く（m は `MAX_M` まで）。
    fn predict(&self, m: usize, out: &mut [u16; MAX_M]);
}

/// P0: 直前の確定の語の繰り返し（いまの `predicted_word`）。何も無ければ 0。
#[derive(Clone, Default)]
pub struct Repeat {
    last: u16,
}

impl Predictor for Repeat {
    fn reset_stream(&mut self) {
        self.last = 0;
    }
    fn observe(&mut self, w: u16) {
        self.last = w;
    }
    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        out[..m.min(MAX_M)].fill(self.last);
    }
}

/// P1: ビットごとに直近 3 語の多数決（1 フレームだけの揺れを無視する）。2 語しか無いときは同数なら直前の語に従う。
#[derive(Clone, Default)]
pub struct Majority3 {
    hist: [u16; 3],
    n: usize,
}

impl Predictor for Majority3 {
    fn reset_stream(&mut self) {
        *self = Self::default();
    }
    fn observe(&mut self, w: u16) {
        self.hist = [w, self.hist[0], self.hist[1]];
        self.n = (self.n + 1).min(3);
    }
    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        let mut v = 0u16;
        for b in 0..16 {
            let ones = self.hist[..self.n]
                .iter()
                .filter(|w| (**w >> b) & 1 != 0)
                .count();
            let on = match (ones * 2).cmp(&self.n) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => (self.hist[0] >> b) & 1 != 0,
            };
            v |= u16::from(on) << b;
        }
        out[..m.min(MAX_M)].fill(v);
    }
}

/// P4: 直前の語の繰り返し。ただし `keep` フレームより先の予測では方向のビットを離す（押し続けは長く続かないという賭け）。
#[derive(Clone)]
pub struct FarRelease {
    last: u16,
    keep: usize,
}

impl FarRelease {
    pub fn new(keep: usize) -> Self {
        Self { last: 0, keep }
    }
}

impl Predictor for FarRelease {
    fn reset_stream(&mut self) {
        self.last = 0;
    }
    fn observe(&mut self, w: u16) {
        self.last = w;
    }
    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        for (i, o) in out.iter_mut().enumerate().take(m.min(MAX_M)) {
            *o = if i < self.keep {
                self.last
            } else {
                self.last & !DIRS
            };
        }
    }
}

/// `RunHazard` の連続の長さの上限（これ以上は同じ扱い）。
const RUN_CAP: usize = 31;
/// 表の 1 マスの数がこれを超えたら両方を半分にする（古い癖を忘れる）。
const COUNT_LIMIT: u16 = 8192;
/// この回数に満たない状態では「変わらない」と見る（学習の前は繰り返しと同じ）。
const MIN_OBS: u32 = 2;

/// P2: 押した長さの癖。語の下位 8 ビットを別々に見て、「値 v のまま r フレーム続いたあと、次も続くか変わるか」の度数を学び、
/// 予測は「続く確率が 5 割を切る所で変わる」とした道筋を引く。表は 8 ビット × 値 2 × 長さ 32 × 2 数 × u16 = 2 KB。
#[derive(Clone)]
pub struct RunHazard {
    cur: [u8; 8],
    run: [u8; 8],
    seen: bool,
    stay: [[[u16; RUN_CAP + 1]; 2]; 8],
    leave: [[[u16; RUN_CAP + 1]; 2]; 8],
}

impl Default for RunHazard {
    fn default() -> Self {
        Self {
            cur: [0; 8],
            run: [0; 8],
            seen: false,
            stay: [[[0; RUN_CAP + 1]; 2]; 8],
            leave: [[[0; RUN_CAP + 1]; 2]; 8],
        }
    }
}

impl RunHazard {
    /// 変わる確率（0〜65536）。
    fn hazard(&self, b: usize, v: u8, r: usize) -> u32 {
        let s = u32::from(self.stay[b][usize::from(v)][r]);
        let l = u32::from(self.leave[b][usize::from(v)][r]);
        if s + l < MIN_OBS {
            0
        } else {
            l * 65536 / (s + l)
        }
    }
}

impl Predictor for RunHazard {
    fn reset_stream(&mut self) {
        self.cur = [0; 8];
        self.run = [0; 8];
        self.seen = false;
    }

    fn observe(&mut self, w: u16) {
        for b in 0..8 {
            let nb = ((w >> b) & 1) as u8;
            if !self.seen {
                self.cur[b] = nb;
                self.run[b] = 1;
                continue;
            }
            let v = usize::from(self.cur[b]);
            let r = usize::from(self.run[b]);
            if nb == self.cur[b] {
                self.stay[b][v][r] += 1;
                self.run[b] = (r + 1).min(RUN_CAP) as u8;
            } else {
                self.leave[b][v][r] += 1;
                self.cur[b] = nb;
                self.run[b] = 1;
            }
            if self.stay[b][v][r] >= COUNT_LIMIT || self.leave[b][v][r] >= COUNT_LIMIT {
                self.stay[b][v][r] /= 2;
                self.leave[b][v][r] /= 2;
            }
        }
        self.seen = true;
    }

    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        let m = m.min(MAX_M);
        out[..m].fill(0);
        if !self.seen {
            return;
        }
        for b in 0..8 {
            let mut v = self.cur[b];
            let mut r = usize::from(self.run[b]);
            let mut surv = 65536u64;
            for o in out.iter_mut().take(m) {
                surv = surv * u64::from(65536 - self.hazard(b, v, r)) / 65536;
                if surv < 32768 {
                    v ^= 1;
                    r = 1;
                    surv = 65536;
                } else {
                    r = (r + 1).min(RUN_CAP);
                }
                *o |= u16::from(v) << b;
            }
        }
    }
}

/// P5: P2 の道筋に、P4 と同じ「`keep` フレームより先は方向を離す」を重ねる。
#[derive(Clone, Default)]
pub struct HazardRelease {
    inner: RunHazard,
    keep: usize,
}

impl HazardRelease {
    pub fn new(keep: usize) -> Self {
        Self {
            inner: RunHazard::default(),
            keep,
        }
    }
}

impl Predictor for HazardRelease {
    fn reset_stream(&mut self) {
        self.inner.reset_stream();
    }
    fn observe(&mut self, w: u16) {
        self.inner.observe(w);
    }
    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        self.inner.predict(m, out);
        for o in out.iter_mut().take(m.min(MAX_M)).skip(self.keep) {
            *o &= !DIRS;
        }
    }
}

/// 表の 1 マス: 文脈の確かめ（ハッシュの別の部分）・いちばん多い次の語の候補・その勢い（多数派の候補を 1 つだけ持つ数え方）。
#[derive(Clone, Copy, Default)]
struct Slot {
    check: u16,
    word: u8,
    cnt: u8,
}

impl Slot {
    fn update(&mut self, check: u16, next: u8) {
        if self.cnt == 0 || self.check != check {
            *self = Slot {
                check,
                word: next,
                cnt: 1,
            };
        } else if self.word == next {
            self.cnt = self.cnt.saturating_add(1);
        } else {
            self.cnt -= 1;
            if self.cnt == 0 {
                self.word = next;
                self.cnt = 1;
            }
        }
    }

    fn hit(&self, check: u16) -> Option<u8> {
        (self.cnt >= CONTEXT_CONF && self.check == check).then_some(self.word)
    }
}

/// 候補として使う最低の勢い。
const CONTEXT_CONF: u8 = 2;
const HASH_BITS: u32 = 12;
const HASH_LEN: usize = 1 << HASH_BITS;

fn hash(ctx: u32) -> (usize, u16) {
    let idx = (ctx.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize;
    let check = (ctx.wrapping_mul(0x85EB_CA6B) >> 16) as u16;
    (idx, check)
}

/// P3: 直前の数語を文脈にした度数表。1 語前・2 語前まで・3 語前までの 3 つの表（1 語前は直に引き、あとの 2 つは 4096 マスのハッシュ）を持ち、
/// 勢いのある長い文脈の候補を採る。どれにも無ければ直前の語の繰り返し。先の語は、予測した語を文脈へ足して引き続ける。
/// 表の大きさは固定（`Slot` 4 B × (256 + 4096 × 2) ≒ 33 KB）。
#[derive(Clone)]
pub struct Context {
    hist: [u8; 3],
    n: usize,
    t1: [Slot; 256],
    t2: [Slot; HASH_LEN],
    t3: [Slot; HASH_LEN],
}

impl Default for Context {
    fn default() -> Self {
        Self {
            hist: [0; 3],
            n: 0,
            t1: [Slot::default(); 256],
            t2: [Slot::default(); HASH_LEN],
            t3: [Slot::default(); HASH_LEN],
        }
    }
}

fn ctx2(h: &[u8; 3]) -> u32 {
    u32::from(h[0]) | (u32::from(h[1]) << 8)
}

fn ctx3(h: &[u8; 3]) -> u32 {
    ctx2(h) | (u32::from(h[2]) << 16)
}

impl Context {
    fn next_word(&self, h: &[u8; 3], n: usize) -> u8 {
        if n >= 3 {
            let (i, c) = hash(ctx3(h));
            if let Some(w) = self.t3[i].hit(c) {
                return w;
            }
        }
        if n >= 2 {
            let (i, c) = hash(ctx2(h));
            if let Some(w) = self.t2[i].hit(c) {
                return w;
            }
        }
        if n >= 1 {
            if let Some(w) = self.t1[usize::from(h[0])].hit(0) {
                return w;
            }
            return h[0];
        }
        0
    }
}

impl Predictor for Context {
    fn reset_stream(&mut self) {
        self.hist = [0; 3];
        self.n = 0;
    }

    fn observe(&mut self, w: u16) {
        let next = (w & 0xFF) as u8;
        let h = self.hist;
        if self.n >= 1 {
            self.t1[usize::from(h[0])].update(0, next);
        }
        if self.n >= 2 {
            let (i, c) = hash(ctx2(&h));
            self.t2[i].update(c, next);
        }
        if self.n >= 3 {
            let (i, c) = hash(ctx3(&h));
            self.t3[i].update(c, next);
        }
        self.hist = [next, h[0], h[1]];
        self.n = (self.n + 1).min(3);
    }

    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        let mut h = self.hist;
        let mut n = self.n;
        for o in out.iter_mut().take(m.min(MAX_M)) {
            let w = self.next_word(&h, n);
            *o = u16::from(w);
            h = [w, h[0], h[1]];
            n = (n + 1).min(3);
        }
    }
}

/// 道具の側で方法を並べて扱うための入れ物（表の大きいものは `Box`。ゲームスレッドの側は上の構造体を直に使う）。
#[derive(Clone)]
pub enum Method {
    Repeat(Repeat),
    Majority3(Majority3),
    RunHazard(Box<RunHazard>),
    Context(Box<Context>),
    FarRelease(FarRelease),
    HazardRelease(Box<HazardRelease>),
}

/// 採点する方法の並び。名前は出力の列の見出し。
pub const METHODS: [&str; 7] = ["P0", "P1", "P2", "P3", "P4a", "P4b", "P5"];
/// P4a / P4b が方向を残す先の長さ（フレーム）。
pub const FAR_KEEP: [usize; 2] = [3, 6];
/// P5 が方向を残す先の長さ（フレーム）。結果を見る前に決めた値（P4a と P4b の間）。
pub const P5_KEEP: usize = 4;

impl Method {
    /// `METHODS` の添字から作る。
    pub fn new(index: usize) -> Option<Self> {
        Some(match index {
            0 => Method::Repeat(Repeat::default()),
            1 => Method::Majority3(Majority3::default()),
            2 => Method::RunHazard(Box::default()),
            3 => Method::Context(Box::default()),
            4 => Method::FarRelease(FarRelease::new(FAR_KEEP[0])),
            5 => Method::FarRelease(FarRelease::new(FAR_KEEP[1])),
            6 => Method::HazardRelease(Box::new(HazardRelease::new(P5_KEEP))),
            _ => return None,
        })
    }
}

impl Predictor for Method {
    fn reset_stream(&mut self) {
        match self {
            Method::Repeat(p) => p.reset_stream(),
            Method::Majority3(p) => p.reset_stream(),
            Method::RunHazard(p) => p.reset_stream(),
            Method::Context(p) => p.reset_stream(),
            Method::FarRelease(p) => p.reset_stream(),
            Method::HazardRelease(p) => p.reset_stream(),
        }
    }
    fn observe(&mut self, w: u16) {
        match self {
            Method::Repeat(p) => p.observe(w),
            Method::Majority3(p) => p.observe(w),
            Method::RunHazard(p) => p.observe(w),
            Method::Context(p) => p.observe(w),
            Method::FarRelease(p) => p.observe(w),
            Method::HazardRelease(p) => p.observe(w),
        }
    }
    fn predict(&self, m: usize, out: &mut [u16; MAX_M]) {
        match self {
            Method::Repeat(p) => p.predict(m, out),
            Method::Majority3(p) => p.predict(m, out),
            Method::RunHazard(p) => p.predict(m, out),
            Method::Context(p) => p.predict(m, out),
            Method::FarRelease(p) => p.predict(m, out),
            Method::HazardRelease(p) => p.predict(m, out),
        }
    }
}

/// 1 本の列を幅 m で採点した結果。tick t = m..n の分（m = 0 は予測が無いので全 tick で外れ 0・ずれ 0）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameEval {
    /// 使った予測の語が本物と違う。
    pub miss: Vec<bool>,
    /// 窓の予測と本物で積分した位置の差の大きさ（px）。
    pub err: Vec<f32>,
    /// 前の tick からのずれの変化の大きさ（px。見た目と本物の動きの食い違い）。先頭の tick は 0。
    pub snap: Vec<f32>,
}

/// `words`（語のマスク済みの列）を `p` で採点する。`p` は `reset_stream` 済みで、学習済みの表があってもよい。
pub fn eval_stream<P: Predictor>(p: &mut P, words: &[u16], m: usize) -> FrameEval {
    let n = words.len();
    let m = m.min(MAX_M);
    let mut ev = FrameEval::default();
    if m == 0 {
        ev.miss = vec![false; n];
        ev.err = vec![0.0; n];
        ev.snap = vec![0.0; n];
        return ev;
    }
    let mut out = [0u16; MAX_M];
    let mut prev = (0.0f64, 0.0f64);
    for t in m..n {
        p.observe(words[t - m]);
        p.predict(m, &mut out);
        ev.miss.push(out[m - 1] & WORD_MASK != words[t] & WORD_MASK);
        let (mut ex, mut ey) = (0.0, 0.0);
        for (i, o) in out.iter().enumerate().take(m) {
            let (px, py) = velocity(*o & WORD_MASK);
            let (tx, ty) = velocity(words[t - m + 1 + i] & WORD_MASK);
            ex += px - tx;
            ey += py - ty;
        }
        ev.err.push(ex.hypot(ey) as f32);
        ev.snap.push(if t == m {
            0.0
        } else {
            (ex - prev.0).hypot(ey - prev.1) as f32
        });
        prev = (ex, ey);
    }
    ev
}

/// どれかの方法が当たれば当たり・ずれは方法のうち最小、とした上限の目安（その場で最良を選べる神様の値）。
pub fn oracle(evals: &[&FrameEval]) -> FrameEval {
    let n = evals.iter().map(|e| e.miss.len()).min().unwrap_or(0);
    let mut o = FrameEval::default();
    for i in 0..n {
        o.miss.push(evals.iter().all(|e| e.miss[i]));
        o.err
            .push(evals.iter().map(|e| e.err[i]).fold(f32::INFINITY, f32::min));
        o.snap.push(
            evals
                .iter()
                .map(|e| e.snap[i])
                .fold(f32::INFINITY, f32::min),
        );
    }
    o
}

/// ずれの大きさがこれ以上の tick の割合も数える（px。花映塚の自機の当たり判定の半径 2.0〜2.3 の約 4 倍）。
pub const BIG_ERR: f32 = 8.0;

/// 複数の列を合わせた集計の入れ物。外れの続く長さは列の境目をまたがない。
#[derive(Debug, Clone, Default)]
pub struct Pool {
    pub streams: usize,
    pub n: u64,
    pub miss: u64,
    pub runs: Vec<u32>,
    pub err: Vec<f32>,
    pub snap: Vec<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub streams: usize,
    pub n: u64,
    pub miss: u64,
    pub miss_pct: f64,
    pub run_n: usize,
    pub run_p50: u32,
    pub run_p90: u32,
    pub run_max: u32,
    pub err_mean: f64,
    pub err_p90: f64,
    pub err_max: f64,
    pub big_pct: f64,
    pub snap_mean: f64,
    pub snap_p90: f64,
    pub snap_max: f64,
}

impl Pool {
    pub fn add(&mut self, e: &FrameEval) {
        self.streams += 1;
        self.n += e.miss.len() as u64;
        self.miss += e.miss.iter().filter(|x| **x).count() as u64;
        let mut run = 0u32;
        for &x in &e.miss {
            if x {
                run += 1;
            } else if run > 0 {
                self.runs.push(run);
                run = 0;
            }
        }
        if run > 0 {
            self.runs.push(run);
        }
        self.err.extend_from_slice(&e.err);
        self.snap.extend_from_slice(&e.snap);
    }

    pub fn summary(&self) -> Summary {
        let mut runs = self.runs.clone();
        runs.sort_unstable();
        let mut err = self.err.clone();
        err.sort_by(f32::total_cmp);
        let mut snap = self.snap.clone();
        snap.sort_by(f32::total_cmp);
        let nf = self.n.max(1) as f64;
        let big = err.iter().filter(|x| **x >= BIG_ERR).count() as f64;
        Summary {
            streams: self.streams,
            n: self.n,
            miss: self.miss,
            miss_pct: if self.n == 0 {
                0.0
            } else {
                100.0 * self.miss as f64 / nf
            },
            run_n: runs.len(),
            run_p50: nearest(&runs, 0.5),
            run_p90: nearest(&runs, 0.9),
            run_max: runs.last().copied().unwrap_or(0),
            err_mean: mean(&err),
            err_p90: f64::from(nearest(&err, 0.9)),
            err_max: f64::from(err.last().copied().unwrap_or(0.0)),
            big_pct: if self.n == 0 { 0.0 } else { 100.0 * big / nf },
            snap_mean: mean(&snap),
            snap_p90: f64::from(nearest(&snap, 0.9)),
            snap_max: f64::from(snap.last().copied().unwrap_or(0.0)),
        }
    }
}

fn mean(v: &[f32]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().map(|x| f64::from(*x)).sum::<f64>() / v.len() as f64
    }
}

/// 昇順の列の q 分位（最近接順位）。空は既定値。
fn nearest<T: Copy + Default>(sorted: &[T], q: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// 予測の幅 m（待ちの上限 R で打ち切った後）と、R を超えて待つ tick の割合（%）。
///
/// 片道 k tick・遅れ D の対称な 2 台で、tick ごとの先頭 N が `N ≤ F + R` に縛られるとき、F は相手の取得（N_peer + D）に k tick 遅れて追うので、
/// 長く見た進み方は `(D + R) / k` tick あたり 1（k > D + R のとき）。待つ割合 = `1 − (D + R) / k`（揺れの無い一定の片道での定常の見積もり）。
pub fn width_and_wait(k: usize, d: usize, r: usize) -> (usize, f64) {
    let m = k.saturating_sub(d);
    let m_eff = m.min(r);
    let wait = if k > d + r {
        100.0 * (1.0 - (d + r) as f64 / k as f64)
    } else {
        0.0
    };
    (m_eff, wait)
}

/// 外れた tick の割合（%）を、「w フレーム前の語の繰り返し」で直に数える（採点の検算用。`eval_stream` の P0 と一致するはず）。
pub fn repeat_miss_count(words: &[u16], w: usize) -> (u64, u64) {
    let mut miss = 0;
    let mut n = 0;
    for t in w..words.len() {
        n += 1;
        if words[t] & WORD_MASK != words[t - w] & WORD_MASK {
            miss += 1;
        }
    }
    (miss, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<P: Predictor>(mut p: P, words: &[u16], m: usize) -> FrameEval {
        p.reset_stream();
        eval_stream(&mut p, words, m)
    }

    fn miss_count(e: &FrameEval) -> usize {
        e.miss.iter().filter(|x| **x).count()
    }

    #[test]
    fn velocity_table() {
        assert_eq!(velocity(0), (0.0, 0.0));
        assert_eq!(velocity(RIGHT), (4.0, 0.0));
        assert_eq!(velocity(LEFT | BIT_SLOW), (-2.0, 0.0));
        assert_eq!(velocity(UP), (0.0, -4.0));
        assert_eq!(
            velocity(LEFT | RIGHT),
            (0.0, 0.0),
            "左右の同時押しは打ち消す"
        );
        let (x, y) = velocity(RIGHT | DOWN);
        assert!((x - 2.0 * std::f64::consts::SQRT_2).abs() < 1e-12 && (y - x).abs() < 1e-12);
        let (x, y) = velocity(LEFT | UP | BIT_SLOW);
        assert!((x + std::f64::consts::SQRT_2).abs() < 1e-12 && (y - x).abs() < 1e-12);
        assert_eq!(velocity(0x0001), (0.0, 0.0), "射撃は動かさない");
    }

    #[test]
    fn constant_stream_never_misses() {
        let words = vec![RIGHT | 0x0001; 200];
        for m in 0..=MAX_M {
            for (i, name) in METHODS.iter().enumerate().take(4) {
                let e = run(Method::new(i).unwrap(), &words, m);
                assert_eq!(miss_count(&e), 0, "{name} m={m}");
                assert!(e.err.iter().all(|x| *x == 0.0), "{name} m={m}");
            }
            // 方向を離す P4 は、押し続けの列では keep より先の分だけ必ず外れ、ずれは先の分 × 4 px になる。
            for (i, keep) in FAR_KEEP.iter().enumerate() {
                let e = run(Method::new(4 + i).unwrap(), &words, m);
                let (want_miss, want_err) = if m > *keep && m > 0 {
                    (200 - m, 4.0 * (m - keep) as f32)
                } else {
                    (0, 0.0)
                };
                assert_eq!(miss_count(&e), want_miss, "P4 keep={keep} m={m}");
                assert!(e.err.iter().all(|x| *x == want_err), "P4 keep={keep} m={m}");
            }
            // P5 も同じ（押し続けの列では P2 の道筋は続くので、離すのは keep より先だけ）。
            let e = run(Method::new(6).unwrap(), &words, m);
            let (want_miss, want_err) = if m > P5_KEEP {
                (200 - m, 4.0 * (m - P5_KEEP) as f32)
            } else {
                (0, 0.0)
            };
            assert_eq!(miss_count(&e), want_miss, "P5 m={m}");
            assert!(e.err.iter().all(|x| *x == want_err), "P5 m={m}");
        }
    }

    /// 交互に A・B と変わる列: 幅 m の繰り返しは m が奇数なら毎 tick 外れ、偶数なら一度も外れない。
    #[test]
    fn alternating_stream_p0_follows_the_formula() {
        let words: Vec<u16> = (0..100)
            .map(|i| if i % 2 == 0 { LEFT } else { RIGHT })
            .collect();
        for m in 1..=10usize {
            let e = run(Repeat::default(), &words, m);
            assert_eq!(e.miss.len(), 100 - m);
            let want = if m % 2 == 1 { 100 - m } else { 0 };
            assert_eq!(miss_count(&e), want, "m={m}");
            let (miss, n) = repeat_miss_count(&words, m);
            assert_eq!((miss as usize, n as usize), (want, 100 - m));
        }
    }

    /// 前半 R、後半 L の列。m = 3 の繰り返しは、変わった tick の 10・11・12 で外れ、ずれは 8・16・24 px。
    #[test]
    fn step_stream_gives_exact_miss_and_error() {
        let words: Vec<u16> = (0..20).map(|i| if i < 10 { RIGHT } else { LEFT }).collect();
        let e = run(Repeat::default(), &words, 3);
        assert_eq!(e.miss.len(), 17);
        let misses: Vec<usize> = e
            .miss
            .iter()
            .enumerate()
            .filter(|(_, x)| **x)
            .map(|(i, _)| i + 3)
            .collect();
        assert_eq!(misses, vec![10, 11, 12]);
        let errs: Vec<f32> = e.err.iter().copied().filter(|x| *x > 0.0).collect();
        assert_eq!(errs, vec![8.0, 16.0, 24.0]);
        let snaps: Vec<f32> = e.snap.iter().copied().filter(|x| *x > 0.0).collect();
        assert_eq!(
            snaps,
            vec![8.0, 8.0, 8.0, 24.0],
            "ずれの変化: 増える 3 回と、予測が追いついて戻る 1 回"
        );
        // 列の最後で外れが続いていても、その連を数える。
        let tail: Vec<u16> = (0..11).map(|i| if i < 10 { RIGHT } else { LEFT }).collect();
        let mut tail_pool = Pool::default();
        tail_pool.add(&run(Repeat::default(), &tail, 1));
        assert_eq!(tail_pool.summary().run_n, 1);
        let mut pool = Pool::default();
        pool.add(&e);
        let s = pool.summary();
        assert_eq!((s.n, s.miss, s.run_n, s.run_max), (17, 3, 1, 3));
        assert!((s.err_mean - 48.0 / 17.0).abs() < 1e-6);
        assert_eq!(s.err_max, 24.0);
        assert_eq!(
            s.big_pct,
            100.0 * 3.0 / 17.0,
            "8 px 以上は 8・16・24 の 3 tick"
        );
    }

    /// 7 フレーム押して 3 フレーム離す周期: 幅 1 の繰り返しは周期ごとに 2 回外れる。押した長さを学ぶ P2 は慣れた後は外れない。
    #[test]
    fn run_hazard_learns_a_fixed_press_length() {
        let words: Vec<u16> = (0..1000)
            .map(|i| if i % 10 < 7 { LEFT } else { 0 })
            .collect();
        let p0 = run(Repeat::default(), &words, 1);
        assert_eq!(miss_count(&p0), 200 - 1);
        let p2 = run(RunHazard::default(), &words, 1);
        assert!(
            miss_count(&p2) <= 12,
            "慣れる前の数周期だけ外れる: {}",
            miss_count(&p2)
        );
        let p2w = run(RunHazard::default(), &words, 3);
        assert!(
            miss_count(&p2w) <= 30,
            "幅 3 でも周期を当てる: {}",
            miss_count(&p2w)
        );
    }

    /// 3 語の周期（A B C）: 直前だけでは当たらず、文脈の表は慣れた後は外れない。
    #[test]
    fn context_learns_a_cycle() {
        let cyc = [LEFT, RIGHT | 0x0001, UP];
        let words: Vec<u16> = (0..600).map(|i| cyc[i % 3]).collect();
        let p0 = run(Repeat::default(), &words, 1);
        assert_eq!(miss_count(&p0), 599);
        for m in [1usize, 2, 5] {
            let p3 = run(Context::default(), &words, m);
            assert!(miss_count(&p3) <= 12, "m={m}: {}", miss_count(&p3));
        }
        // 学習済みの表を持ち越せば、別の列の最初から当たる。
        let mut warm = Context::default();
        warm.reset_stream();
        for w in &words {
            warm.observe(*w);
        }
        warm.reset_stream();
        let e = eval_stream(&mut warm, &words, 1);
        assert!(miss_count(&e) <= 3, "{}", miss_count(&e));
    }

    /// 直前 1 語では決まらない列を、2 語・3 語の文脈が決める（長い文脈の表を引かなければ落ちる）。
    #[test]
    fn context_longer_orders_resolve_what_shorter_cannot() {
        let (a, b, c) = (LEFT, RIGHT, UP);
        // A B A C: A の後は B と C が交互。
        let w2: Vec<u16> = (0..800).map(|i| [a, b, a, c][i % 4]).collect();
        assert!(miss_count(&run(Context::default(), &w2, 1)) <= 12);
        // A A B A A C: (A,A) の後が B と C に分かれ、3 語前まで見ないと決まらない。
        let w3: Vec<u16> = (0..1200).map(|i| [a, a, b, a, a, c][i % 6]).collect();
        let p3 = miss_count(&run(Context::default(), &w3, 1));
        assert!(p3 <= 15, "{p3}");
        assert!(miss_count(&run(Repeat::default(), &w3, 1)) > 700);
    }

    #[test]
    fn majority_ignores_one_frame_glitch() {
        let mut p = Majority3::default();
        for w in [RIGHT, LEFT, RIGHT] {
            p.observe(w);
        }
        let mut out = [0u16; MAX_M];
        p.predict(2, &mut out);
        assert_eq!(&out[..2], &[RIGHT, RIGHT]);
        let mut q = Majority3::default();
        q.observe(RIGHT);
        q.observe(LEFT);
        q.predict(1, &mut out);
        assert_eq!(out[0], LEFT, "2 語で同数なら直前に従う");
    }

    #[test]
    fn far_release_drops_directions_after_keep() {
        let mut p = FarRelease::new(2);
        p.observe(RIGHT | 0x0001 | BIT_SLOW);
        let mut out = [0u16; MAX_M];
        p.predict(4, &mut out);
        assert_eq!(out[0], RIGHT | 0x0001 | BIT_SLOW);
        assert_eq!(out[1], RIGHT | 0x0001 | BIT_SLOW);
        assert_eq!(out[2], 0x0001 | BIT_SLOW, "方向だけ離し、ボタンは残す");
        assert_eq!(out[3], 0x0001 | BIT_SLOW);
    }

    #[test]
    fn oracle_is_hit_if_any_hits() {
        let a = FrameEval {
            miss: vec![true, true, false],
            err: vec![4.0, 2.0, 0.0],
            snap: vec![0.0; 3],
        };
        let b = FrameEval {
            miss: vec![true, false, false],
            err: vec![1.0, 8.0, 0.0],
            snap: vec![0.0; 3],
        };
        let o = oracle(&[&a, &b]);
        assert_eq!(o.miss, vec![true, false, false]);
        assert_eq!(o.err, vec![1.0, 2.0, 0.0]);
    }

    #[test]
    fn width_and_wait_formula() {
        assert_eq!(width_and_wait(9, 1, 8), (8, 0.0));
        assert_eq!(width_and_wait(9, 0, 8), (8, 100.0 * (1.0 - 8.0 / 9.0)));
        assert_eq!(width_and_wait(3, 5, 8), (0, 0.0));
        assert_eq!(width_and_wait(12, 0, 8), (8, 100.0 * (1.0 - 8.0 / 12.0)));
        assert_eq!(width_and_wait(12, 4, 8), (8, 0.0));
        assert_eq!(width_and_wait(12, 2, 8), (8, 100.0 * (1.0 - 10.0 / 12.0)));
    }

    #[test]
    fn tables_have_fixed_size() {
        assert!(std::mem::size_of::<Context>() < 40 * 1024);
        assert!(std::mem::size_of::<RunHazard>() < 4 * 1024);
    }
}
