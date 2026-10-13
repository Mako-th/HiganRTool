//! ウィンドウモードのペースの第 3 の待ち方 b-hybrid の判断（pure）。期限の手前までは寝て、最後の短い間だけ b-spin と同じく回る。
//!
//! b-spin は期限の前ずっと `Sleep(0)` で回って論理コア 1 つを使い続ける。寝る方式 b は入力 → Present の前が b-spin より
//! p99 で約 +500 µs 悪かった（実測。原因は寝ることまでで、電源まわりの副作用と推定）。b-hybrid は寝るのを「期限 −（回し読みの幅 + 余裕）」までにとどめ、その後は b-spin と
//! 同じに回る。余裕の間に CPU が起き直す見込み（実機で測っていない）。既定は b-spin のまま。数値は ini の `[pacing]` で変えられる（`HybridParams`。名前・範囲・既定は `HYBRID_KEYS`）。
//!
//! フレームの始まりの 3 つの区間（期限 D、いま t、残り r = D − t。`spin` は `hybrid_spin_us`）:
//!
//! | 残り r | 始まりでのこと | 戻り |
//! |---|---|---|
//! | r > spin + 余裕 | 高分解能の待機タイマーで寝る。長さ = min(r − spin − 余裕, `hybrid_chunk_us`) を µs に切り捨て | まだ（メッセージループへ） |
//! | spin < r ≤ spin + 余裕 | b-spin と同じく `Sleep(0)` で譲る | まだ |
//! | r ≤ spin | QPC を読み続けて期限ちょうどに始める（`Pace::decide` の `Head::Spin`） | 始める |
//!
//! **余裕**（`Doze::margin`）= max(`hybrid_margin_us`, 寝過ごしの山 + `hybrid_guard_us`) を `hybrid_margin_max_us` で抑えたもの。
//! 寝過ごし = 実際に寝た長さ − 頼んだ長さ。山は寝るたびに減り、新しい寝過ごしがそれより大きければその値になる。
//!
//! 寝る長さは `us ≤ r − spin − 余裕` で決めるので、寝過ごしが「余裕 + spin」以下なら期限を越えない。越えたフレームは
//! `late = 1` になり、越え続ける PC では `hybrid_fallback_*` の数に達した時点でその実行の残りを b-spin に戻す（`Doze::fell_back`）。
//! 遅延: 入力 → Present の間は変えない。増えうるのは、寝過ごしが余裕 + spin を越えたフレームの開始の遅れだけ。
#![forbid(unsafe_code)]

use crate::pacing::{ticks_to_us_floor, us_to_ticks_ceil};

/// `hybrid_margin_us` の既定（µs）。余裕の下限で、回し読みの 1 ms と合わせて期限の 3 ms 前から回る。
/// b の実測では寝過ごしが最大でおよそ 1.9 ms、期限を越えた量は最大 882 µs だったので、それを越える 2 ms にした。
pub const DEFAULT_MARGIN_US: u32 = 2_000;
/// `hybrid_margin_max_us` の既定（µs）。余裕の上限。60 fps の 1 周期の約半分で、これより大きいと寝る区間がほとんど残らない。
pub const MAX_MARGIN_US: u32 = 8_000;
/// 余裕の下限・上限の ini の範囲の上端（µs）。60 fps の 1 周期に近く、これ以上は寝る区間が残らない。
pub const MARGIN_LIMIT_US: u32 = 16_000;
/// 寝過ごしの山に足す上乗せの既定（µs）。
pub const GUARD_US: u32 = 250;
/// 山の減り方の既定: 寝るたびに `山 >> 10`（1/1024）だけ減らす。60 fps で 1 フレームに 10 回前後寝るなら半分になるのは約 1 秒強
/// （寝る回数は余裕で変わる。推定）。単発の大きな寝過ごしは数秒で既定へ戻る（推定）が、
/// 数秒ごとに引っかかる PC では余裕が上限の近くに居続け、CPU は下がりきらない。
pub const PEAK_DECAY_SHIFT: u32 = 10;
/// 1 回に寝る長さの上限の既定（µs）。寝続けるとメッセージを汲まないため、b と同じ 1 ms。
pub const CHUNK_US: u32 = 1_000;
/// 最後に QPC を回し読みする幅の既定（µs）。b / b-spin の `pacing::SPIN_US` と同じ 1 ms。
pub const SPIN_US: u32 = 1_000;
/// b-spin へ戻す条件の既定: 「寝た後に期限を越えた枠」が `FALLBACK_FRAMES` 枠の期間の中で `FALLBACK_COUNT` 回。
/// 単発の引っかかりでは戻らず、寝過ごしが余裕の上限を越え続ける PC でだけ戻る。
pub const FALLBACK_COUNT: u32 = 10;
pub const FALLBACK_FRAMES: u32 = 60;

/// b-hybrid の数値（ini の `[pacing]` の `hybrid_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HybridParams {
    pub margin_us: u32,
    pub margin_max_us: u32,
    pub guard_us: u32,
    pub decay_shift: u32,
    pub chunk_us: u32,
    pub spin_us: u32,
    /// 0 なら b-spin へ戻さない。
    pub fallback_count: u32,
    pub fallback_frames: u32,
}

impl HybridParams {
    pub const DEFAULT: HybridParams = HybridParams {
        margin_us: DEFAULT_MARGIN_US,
        margin_max_us: MAX_MARGIN_US,
        guard_us: GUARD_US,
        decay_shift: PEAK_DECAY_SHIFT,
        chunk_us: CHUNK_US,
        spin_us: SPIN_US,
        fallback_count: FALLBACK_COUNT,
        fallback_frames: FALLBACK_FRAMES,
    };

    /// ini のキーに値を置く（`HYBRID_KEYS` の範囲で確かめる）。知らないキーは `Ok(false)`。
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool, String> {
        let Some(k) = HYBRID_KEYS.iter().find(|k| k.name == key) else {
            return Ok(false);
        };
        let t = value.trim();
        // `u32::from_str` は先頭の `+` を通すので、数字だけかを先に見る。
        let n = (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
            .then(|| t.parse::<u32>().ok())
            .flatten()
            .filter(|n| (k.min..=k.max).contains(n))
            .ok_or(format!(
                "{} の値 {value:?} は {}〜{} の整数（{}。既定 {}）",
                k.name, k.min, k.max, k.unit, k.default
            ))?;
        *(k.field)(self) = n;
        Ok(true)
    }

    /// 値どうしの決まり（下限 ≤ 上限）。`set` を全部終えてから呼ぶ。
    pub fn check(&self) -> Result<(), String> {
        if self.margin_us > self.margin_max_us {
            return Err(format!(
                "hybrid_margin_us = {} が hybrid_margin_max_us = {} より大きい（下限 ≤ 上限）",
                self.margin_us, self.margin_max_us
            ));
        }
        Ok(())
    }

    /// 本ログの 1 行の中身（既定と違う値には ★ を付ける）。
    pub fn text(&self) -> String {
        HYBRID_KEYS
            .iter()
            .map(|k| {
                let v = *(k.field)(&mut self.clone());
                let mark = if v == k.default { "" } else { "★" };
                format!("{mark}{} = {v}", k.name)
            })
            .collect::<Vec<_>>()
            .join("、")
    }
}

pub struct HybridKey {
    pub name: &'static str,
    pub min: u32,
    pub max: u32,
    pub default: u32,
    pub unit: &'static str,
    field: fn(&mut HybridParams) -> &mut u32,
}

/// b-hybrid の ini のキー（名前・範囲・既定）。
pub const HYBRID_KEYS: [HybridKey; 8] = [
    HybridKey {
        name: "hybrid_margin_us",
        min: 0,
        max: MARGIN_LIMIT_US,
        default: DEFAULT_MARGIN_US,
        unit: "µs",
        field: |p| &mut p.margin_us,
    },
    HybridKey {
        name: "hybrid_margin_max_us",
        min: 0,
        max: MARGIN_LIMIT_US,
        default: MAX_MARGIN_US,
        unit: "µs",
        field: |p| &mut p.margin_max_us,
    },
    HybridKey {
        name: "hybrid_guard_us",
        min: 0,
        max: 5_000,
        default: GUARD_US,
        unit: "µs",
        field: |p| &mut p.guard_us,
    },
    HybridKey {
        name: "hybrid_decay_shift",
        min: 1,
        max: 20,
        default: PEAK_DECAY_SHIFT,
        unit: "寝るたびに 1/2^N",
        field: |p| &mut p.decay_shift,
    },
    HybridKey {
        name: "hybrid_chunk_us",
        min: 100,
        max: 4_000,
        default: CHUNK_US,
        unit: "µs",
        field: |p| &mut p.chunk_us,
    },
    HybridKey {
        name: "hybrid_spin_us",
        min: 100,
        max: 4_000,
        default: SPIN_US,
        unit: "µs",
        field: |p| &mut p.spin_us,
    },
    HybridKey {
        name: "hybrid_fallback_count",
        min: 0,
        max: 10_000,
        default: FALLBACK_COUNT,
        unit: "回。0 = 戻さない",
        field: |p| &mut p.fallback_count,
    },
    HybridKey {
        name: "hybrid_fallback_frames",
        min: 1,
        max: 1_000_000,
        default: FALLBACK_FRAMES,
        unit: "枠",
        field: |p| &mut p.fallback_frames,
    },
];

/// フレームの始まりの処理が寝るか譲るか（`Pace::decide` が `Head::Sleep` を返したときに聞く）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Sleep { us: u32 },
    Yield,
}

/// 余裕の見積もりと、b-spin へ戻す応答（ゲームのスレッドのフレームの始まりの処理だけが触る。確保しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doze {
    qpf: u64,
    params: HybridParams,
    /// ini の余裕の下限・上乗せ・上限（カウント）。
    base: u64,
    guard: u64,
    max: u64,
    /// 最後の回し読みの幅（カウント）。
    spin: u64,
    /// 寝過ごしの山（カウント。`max` 以下）。
    peak: u64,
    /// いまの余裕（カウントと µs）。山が変わるとき（`new` / `woke`）だけ出し直す。フレームの始まりが来るたびに割り算をしないため
    /// （i686 の u128 の割り算は関数呼び出しになる）。
    margin: u64,
    margin_us: u32,
    /// 寝た回数・寝過ごしがそのときの余裕を越えた回数・寝過ごしの最大（カウント。山と違って減らない）。
    sleeps: u64,
    over_margin: u64,
    over_max: u64,
    /// b-spin へ戻す応答: 見た枠の数・いまの期間の始まりの枠・期間の中の「寝た後に期限を越えた枠」の数・その全体の数・戻したか。
    frames: u64,
    window_start: u64,
    window_late: u32,
    late_after_sleep: u64,
    fell_back: bool,
}

impl Doze {
    /// 余裕の下限だけを変え、ほかは既定（余裕だけを見るコンストラクタ）。
    pub fn new(qpf: u64, margin_us: u32) -> Doze {
        Doze::with_params(
            qpf,
            &HybridParams {
                margin_us,
                margin_max_us: margin_us.max(MAX_MARGIN_US),
                ..HybridParams::DEFAULT
            },
        )
    }

    pub fn with_params(qpf: u64, p: &HybridParams) -> Doze {
        let base = us_to_ticks_ceil(u64::from(p.margin_us), qpf);
        let mut d = Doze {
            qpf,
            params: *p,
            base,
            guard: us_to_ticks_ceil(u64::from(p.guard_us), qpf),
            max: base.max(us_to_ticks_ceil(u64::from(p.margin_max_us), qpf)),
            spin: us_to_ticks_ceil(u64::from(p.spin_us), qpf),
            peak: 0,
            margin: 0,
            margin_us: 0,
            sleeps: 0,
            over_margin: 0,
            over_max: 0,
            frames: 0,
            window_start: 0,
            window_late: 0,
            late_after_sleep: 0,
            fell_back: false,
        };
        d.refresh();
        d
    }

    fn refresh(&mut self) {
        self.margin = self
            .base
            .max(self.peak.saturating_add(self.guard))
            .min(self.max);
        self.margin_us = ticks_to_us_floor(self.margin, self.qpf).min(u64::from(u32::MAX)) as u32;
    }

    pub fn params(&self) -> &HybridParams {
        &self.params
    }

    pub fn spin(&self) -> u64 {
        self.spin
    }

    pub fn margin(&self) -> u64 {
        self.margin
    }

    /// いまの余裕（µs に切り捨て。CSV の `hybrid_margin_us`）。
    pub fn margin_us(&self) -> u32 {
        self.margin_us
    }

    /// 寝るか譲るか。`rest` は「期限 − いま − 回し読みの幅」（カウント）。寝る長さは `rest − 余裕` を µs に切り捨てて
    /// `hybrid_chunk_us` で抑えたもの（0 なら譲る）で、「頼んだ長さ + 余裕 ≤ rest」がいつも成り立つ。b-spin へ戻した後は、いつも譲る。
    pub fn choose(&self, rest: u64) -> Wait {
        let m = self.margin;
        if self.fell_back || rest <= m {
            return Wait::Yield;
        }
        match ticks_to_us_floor(rest - m, self.qpf).min(u64::from(self.params.chunk_us)) {
            0 => Wait::Yield,
            us => Wait::Sleep { us: us as u32 },
        }
    }

    /// `us` µs を頼んで寝て、実際には `slept` カウント経っていた。寝過ごし（カウント）を返し、山を更新する。入口から寝るまでの処理も
    /// 入るので 0 まで下がりきらない。寝過ごしが寝る前の余裕を越えたら数える。
    pub fn woke(&mut self, us: u32, slept: u64) -> u64 {
        let margin = self.margin;
        let over = slept.saturating_sub(us_to_ticks_ceil(u64::from(us), self.qpf));
        self.sleeps += 1;
        if over > margin {
            self.over_margin += 1;
        }
        self.over_max = self.over_max.max(over);
        // 小さい山でも 1 カウントずつは減らす（`>>` だけだと 2^N カウント未満で止まり、QPF の小さい PC で余裕が既定へ戻りきらない）。
        let decayed = self
            .peak
            .saturating_sub((self.peak >> self.params.decay_shift).max(1));
        self.peak = decayed.max(over.min(self.max));
        self.refresh();
        over
    }

    /// 新しい枠の始まりで呼ぶ。
    pub fn on_frame(&mut self) {
        self.frames += 1;
    }

    /// 寝た後に期限を越えて始めた枠。期間（最初に越えた枠から `hybrid_fallback_frames` 枠。過ぎてから越えたら数え直す）の中で
    /// `hybrid_fallback_count` 回に達したら b-spin へ戻す。
    pub fn late_after_sleep(&mut self) {
        self.late_after_sleep += 1;
        let p = self.params;
        if p.fallback_count == 0 || self.fell_back {
            return;
        }
        if self.window_late == 0 || self.frames - self.window_start >= u64::from(p.fallback_frames)
        {
            self.window_start = self.frames;
            self.window_late = 0;
        }
        self.window_late += 1;
        if self.window_late >= p.fallback_count {
            self.fell_back = true;
        }
    }

    /// b-spin へ戻したか（その実行の残りは戻さない）。
    pub fn fell_back(&self) -> bool {
        self.fell_back
    }

    /// 寝た後に期限を越えた枠の数（戻した後は数えない —— 寝ないので）。
    pub fn late_after_sleep_count(&self) -> u64 {
        self.late_after_sleep
    }

    pub fn sleeps(&self) -> u64 {
        self.sleeps
    }

    pub fn over_margin(&self) -> u64 {
        self.over_margin
    }

    pub fn over_max_us(&self) -> u64 {
        ticks_to_us_floor(self.over_max, self.qpf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::{Head, Pace};

    const QPF: u64 = 10_000_000;
    const QPFS: [u64; 4] = [10_000_000, 3_579_545, 14_318_180, 2_148_437];
    const SPIN_US: u64 = 1_000;
    const CHUNK_US: u32 = 1_000;
    const MIN_MARGIN_US: u32 = 500;

    /// 合成の時計: `sleep(us)` は `us + over(n)` µs 進む（n は寝た回数）。
    struct Sim {
        qpf: u64,
        now: u64,
        yield_us: u64,
        head_us: u64,
        work_us: u64,
        over: Box<dyn Fn(u64) -> u64>,
        sleeps: u64,
    }

    #[derive(Debug, Default)]
    struct Frame {
        start: u64,
        deadline: u64,
        late: bool,
        sleeps: u32,
        yields: u32,
        /// 期限の何カウント前から寝ずにいたか（寝なかったら 0）。
        awake_before: u64,
        max_sleep_us: u32,
    }

    impl Sim {
        fn new(qpf: u64, over: impl Fn(u64) -> u64 + 'static) -> Sim {
            Sim {
                qpf,
                now: 1_000 * qpf,
                yield_us: 5,
                head_us: 2,
                work_us: 3_000,
                over: Box::new(over),
                sleeps: 0,
            }
        }

        fn adv_us(&mut self, us: u64) {
            self.now += us_to_ticks_ceil(us, self.qpf);
        }

        /// `dll::pacing::Pacer::head` の b-hybrid の経路を真似て、フレームを 1 つ始めるまで回す。
        fn frame(&mut self, pace: &mut Pace, doze: &mut Doze) -> Frame {
            let spin = doze.spin();
            doze.on_frame();
            let mut f = Frame::default();
            let mut last_wake = None;
            for _ in 0..1_000_000 {
                let now = self.now;
                match pace.decide(now, spin, CHUNK_US) {
                    Head::Sleep { deadline, .. } => {
                        f.deadline = deadline;
                        match doze.choose(deadline - now - spin) {
                            Wait::Sleep { us } => {
                                let over = (self.over)(self.sleeps);
                                self.sleeps += 1;
                                self.adv_us(u64::from(us) + over);
                                doze.woke(us, self.now - now);
                                f.sleeps += 1;
                                f.max_sleep_us = f.max_sleep_us.max(us);
                                last_wake = Some(self.now);
                            }
                            Wait::Yield => {
                                self.adv_us(self.yield_us);
                                f.yields += 1;
                            }
                        }
                        self.adv_us(self.head_us);
                    }
                    Head::Spin { deadline } => {
                        f.deadline = deadline;
                        self.now = deadline;
                        break;
                    }
                    Head::Run { deadline, late } => {
                        f.deadline = deadline;
                        f.late = late;
                        break;
                    }
                }
            }
            f.start = self.now;
            f.awake_before = last_wake.map_or(0, |w| f.deadline.saturating_sub(w));
            pace.started(f.start);
            if f.late && f.sleeps > 0 {
                doze.late_after_sleep();
            }
            self.adv_us(self.work_us);
            f
        }
    }

    #[test]
    fn sleep_never_plans_past_the_margin() {
        for qpf in QPFS {
            let d = Doze::new(qpf, DEFAULT_MARGIN_US);
            let m = d.margin();
            assert_eq!(m, us_to_ticks_ceil(u64::from(DEFAULT_MARGIN_US), qpf));
            let us_ticks = |us: u32| u64::from(us) * qpf / 1_000_000;
            for rest in (1..m + us_to_ticks_ceil(5_000, qpf)).step_by(7) {
                match d.choose(rest) {
                    Wait::Sleep { us } => {
                        assert!((1..=CHUNK_US).contains(&us), "qpf {qpf} rest {rest}: {us}");
                        assert!(us_ticks(us) + m <= rest, "qpf {qpf} rest {rest}: {us}");
                        assert!(rest > m);
                    }
                    Wait::Yield => assert!(
                        ticks_to_us_floor(rest.saturating_sub(m), qpf) == 0,
                        "qpf {qpf} rest {rest}: 寝られるのに譲った"
                    ),
                }
            }
        }
    }

    #[test]
    fn within_the_margin_every_frame_starts_on_the_deadline() {
        for qpf in QPFS {
            // 寝過ごしは 0〜1,900 µs を回す（b の実測の最大に近い値まで）。
            let mut sim = Sim::new(qpf, |n| (n * 397) % 1_900);
            let mut pace = Pace::new(qpf, 60).unwrap();
            let mut doze = Doze::new(qpf, DEFAULT_MARGIN_US);
            for i in 0..300 {
                let f = sim.frame(&mut pace, &mut doze);
                if i == 0 {
                    continue; // 最初のフレームの始まりは期限 = いま（格子の基点）。
                }
                assert!(!f.late, "qpf {qpf} frame {i}: {f:?}");
                assert_eq!(f.start, f.deadline, "qpf {qpf} frame {i}: {f:?}");
                assert!(f.sleeps >= 3, "qpf {qpf} frame {i}: 寝ていない {f:?}");
                assert!(
                    f.awake_before >= us_to_ticks_ceil(SPIN_US, qpf),
                    "qpf {qpf} frame {i}: {f:?}"
                );
            }
            // 寝過ごしの山（最大 1,899 µs）+ 上乗せで、余裕は 2,149 µs まで上がる。上限は越えない。
            assert!(doze.margin_us() <= MAX_MARGIN_US);
            assert_eq!(doze.over_margin(), 0, "qpf {qpf}");
        }
    }

    #[test]
    fn oversleep_past_the_deadline_is_late_then_the_margin_grows() {
        for qpf in QPFS {
            // 最初の 1 回の眠りだけ 14 ms 寝過ごす（期限を越える）。その後は 0。
            let mut sim = Sim::new(qpf, |n| if n == 0 { 14_000 } else { 0 });
            let mut pace = Pace::new(qpf, 60).unwrap();
            let mut doze = Doze::new(qpf, DEFAULT_MARGIN_US);
            let _ = sim.frame(&mut pace, &mut doze);
            let f1 = sim.frame(&mut pace, &mut doze);
            assert!(f1.late, "qpf {qpf}: {f1:?}");
            assert!(f1.start > f1.deadline, "qpf {qpf}: {f1:?}");
            assert_eq!(f1.sleeps, 1, "qpf {qpf}: {f1:?}");
            assert_eq!(doze.over_margin(), 1, "qpf {qpf}");
            assert!(
                (13_999..=14_000).contains(&doze.over_max_us()),
                "qpf {qpf}: {}",
                doze.over_max_us()
            );
            assert_eq!(doze.margin_us(), MAX_MARGIN_US, "qpf {qpf}");
            for i in 0..30 {
                let f = sim.frame(&mut pace, &mut doze);
                assert!(
                    !f.late && f.start == f.deadline,
                    "qpf {qpf} frame {i}: {f:?}"
                );
            }
            for _ in 0..3_000 {
                let _ = sim.frame(&mut pace, &mut doze);
            }
            assert_eq!(doze.margin_us(), DEFAULT_MARGIN_US, "qpf {qpf}");
            assert_eq!(doze.over_margin(), 1, "qpf {qpf}");
        }
    }

    #[test]
    fn oversleep_past_the_margin_but_within_the_spin_is_counted_not_late() {
        // 眠りは毎回 2.5 ms 寝過ごす。1 回目は余裕 2 ms を越え（数える）、以後は余裕 2.75 ms の中。
        let mut sim = Sim::new(QPF, |_| 2_500);
        let mut pace = Pace::new(QPF, 60).unwrap();
        let mut doze = Doze::new(QPF, DEFAULT_MARGIN_US);
        let _ = sim.frame(&mut pace, &mut doze);
        let f = sim.frame(&mut pace, &mut doze);
        assert_eq!(doze.over_margin(), 1);
        assert!(!f.late && f.start == f.deadline, "{f:?}");
        assert_eq!(doze.margin_us(), 2_500 + GUARD_US);
    }

    #[test]
    fn a_stall_raises_the_margin_only_up_to_the_cap() {
        let mut d = Doze::new(QPF, MIN_MARGIN_US);
        assert_eq!(d.margin_us(), MIN_MARGIN_US);
        // 47 ms の引っかかり（実測で見た大きさ）。
        let over = d.woke(1_000, us_to_ticks_ceil(48_000, QPF));
        assert_eq!(over, us_to_ticks_ceil(47_000, QPF));
        assert_eq!(d.margin_us(), MAX_MARGIN_US);
        assert_eq!(d.over_max_us(), 47_000);
        assert_eq!(
            d.choose(us_to_ticks_ceil(15_666, QPF)),
            Wait::Sleep { us: 1_000 }
        );
        assert_eq!(d.choose(us_to_ticks_ceil(8_000, QPF)), Wait::Yield);
        for _ in 0..20_000 {
            d.woke(1_000, us_to_ticks_ceil(1_000, QPF));
        }
        assert_eq!(d.margin_us(), MIN_MARGIN_US);
        // ini の余裕が上限より大きくても（コンストラクタは受ける）、余裕はその値で止まる。
        let big = Doze::new(QPF, 12_000);
        assert_eq!(big.margin_us(), 12_000);
        // QPF の小さい PC でも下限まで戻る（山が 1,024 カウント未満で減らなくならない）。
        for qpf in QPFS {
            let mut d = Doze::new(qpf, MIN_MARGIN_US);
            d.woke(1_000, us_to_ticks_ceil(48_000, qpf));
            assert_eq!(d.margin_us(), MAX_MARGIN_US, "qpf {qpf}");
            for _ in 0..20_000 {
                d.woke(1_000, us_to_ticks_ceil(1_000, qpf));
            }
            assert_eq!(d.margin_us(), MIN_MARGIN_US, "qpf {qpf}");
        }
    }

    #[test]
    fn waking_early_is_not_negative_oversleep() {
        let mut d = Doze::new(QPF, DEFAULT_MARGIN_US);
        d.woke(1_000, us_to_ticks_ceil(3_000, QPF));
        let m = d.margin();
        assert_eq!(d.woke(1_000, 0), 0);
        assert!(d.margin() <= m && d.margin() >= m - (m >> PEAK_DECAY_SHIFT) - 1);
        assert_eq!(d.sleeps(), 2);
    }

    #[test]
    fn repeated_late_frames_fall_back_to_b_spin() {
        for qpf in QPFS {
            let mut sim = Sim::new(qpf, |_| 14_000);
            let mut pace = Pace::new(qpf, 60).unwrap();
            let mut doze = Doze::with_params(qpf, &HybridParams::DEFAULT);
            let _ = sim.frame(&mut pace, &mut doze);
            // 遅れた枠の後はフレームの始まりへ来るのが遅くなり、余裕の中に入って寝ない枠も混ざる。
            let mut late = 0u32;
            for i in 0..60 {
                if doze.fell_back() {
                    break;
                }
                let f = sim.frame(&mut pace, &mut doze);
                if f.late && f.sleeps > 0 {
                    late += 1;
                } else {
                    assert!(!f.late, "qpf {qpf} {i}: 寝ずに遅れた {f:?}");
                }
                assert_eq!(
                    doze.fell_back(),
                    late >= FALLBACK_COUNT,
                    "qpf {qpf} {i}: 遅れ {late} 回"
                );
            }
            assert!(doze.fell_back(), "qpf {qpf}: 60 枠で戻らない");
            assert_eq!(late, FALLBACK_COUNT, "qpf {qpf}");
            assert!(doze.fell_back(), "qpf {qpf}");
            assert_eq!(
                doze.late_after_sleep_count(),
                u64::from(FALLBACK_COUNT),
                "qpf {qpf}"
            );
            for i in 0..30 {
                let f = sim.frame(&mut pace, &mut doze);
                assert!(f.sleeps == 0 && !f.late, "qpf {qpf} {i}: {f:?}");
                assert!(f.start == f.deadline, "qpf {qpf} {i}: {f:?}");
            }
        }
        let mut sim = Sim::new(QPF, |_| 14_000);
        let mut pace = Pace::new(QPF, 60).unwrap();
        let p = HybridParams {
            fallback_count: 0,
            ..HybridParams::DEFAULT
        };
        let mut doze = Doze::with_params(QPF, &p);
        for _ in 0..100 {
            let _ = sim.frame(&mut pace, &mut doze);
        }
        assert!(!doze.fell_back());
        assert!(
            doze.late_after_sleep_count() >= 50,
            "{}",
            doze.late_after_sleep_count()
        );
        let mut d = Doze::with_params(QPF, &HybridParams::DEFAULT);
        for f in 0..1_000u64 {
            d.on_frame();
            if f % 100 < 9 {
                d.late_after_sleep();
            }
        }
        assert!(!d.fell_back(), "期間ごとに 9 回では戻さない");
        let mut d = Doze::with_params(QPF, &HybridParams::DEFAULT);
        for f in 0..60u64 {
            d.on_frame();
            if f == 0 || f >= 51 {
                d.late_after_sleep();
            }
        }
        assert!(d.fell_back(), "60 枠の期間の中で 10 回");
    }

    #[test]
    fn ini_numbers_shape_the_wait() {
        for qpf in QPFS {
            let p = HybridParams {
                margin_us: 1_000,
                margin_max_us: 3_000,
                guard_us: 100,
                chunk_us: 400,
                spin_us: 2_500,
                ..HybridParams::DEFAULT
            };
            let mut sim = Sim::new(qpf, |n| (n * 131) % 600);
            let mut pace = Pace::new(qpf, 60).unwrap();
            let mut doze = Doze::with_params(qpf, &p);
            assert_eq!(doze.spin(), us_to_ticks_ceil(2_500, qpf));
            let _ = sim.frame(&mut pace, &mut doze);
            for i in 0..100 {
                let before = sim.sleeps;
                let f = sim.frame(&mut pace, &mut doze);
                assert!(!f.late && f.start == f.deadline, "qpf {qpf} {i}: {f:?}");
                assert!(
                    f.sleeps >= 10 && f.max_sleep_us <= 400,
                    "qpf {qpf} {i}: {f:?}"
                );
                assert!(sim.sleeps - before == u64::from(f.sleeps));
                assert!(
                    f.awake_before >= us_to_ticks_ceil(2_500 + 1_000 - 600, qpf),
                    "qpf {qpf} {i}: {f:?}"
                );
            }
            assert_eq!(doze.margin_us(), 1_000, "qpf {qpf}");
            doze.woke(400, us_to_ticks_ceil(20_000, qpf));
            assert_eq!(doze.margin_us(), 3_000, "qpf {qpf}");
            assert_eq!(
                doze.choose(us_to_ticks_ceil(10_000, qpf)),
                Wait::Sleep { us: 400 }
            );
        }
        let p = HybridParams {
            margin_us: 0,
            guard_us: 300,
            ..HybridParams::DEFAULT
        };
        let mut d = Doze::with_params(QPF, &p);
        assert_eq!(d.margin_us(), 300);
        d.woke(1_000, us_to_ticks_ceil(1_500, QPF));
        assert_eq!(d.margin_us(), 800);
    }
}
