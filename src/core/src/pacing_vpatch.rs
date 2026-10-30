//! `mode = vpatch`: vpatchの通常のペース（Vsync=0・SleepType=1・AutoBltPrepareTime=1・AllowShortDelay=0）の形を再現した測定専用の方式（pure）。
//!
//! 遅延の表の「vpatch 相当」の列を本ツールの中で測るためのもの。既定ではない。`wait`や`cycle_phases`にも含めない。
//! 形は2010-02-06バージョンのvpatch_th09.dllの逆アセンブルで読んだもの（コードはコピーしていない）。
//! - 期限`T_n = 基点 + floor(n × QPF ÷ fps)`（`Grid::at`）。フレームの始まりでnを1進めて`T_n`を作り、`T_n − 準備時間 `に起きる。Presentの直前で`T_n`まで待つ。
//! - 待ちは「(目標 − 回し読みの幅) まで普通の待機タイマーで寝る → 目標までQPCを読み続ける」（`sleep_ticks`）。
//! - 始まりで`T_n`を既に過ぎていたら待たずに基点を今に取り直し、nを0に戻す（本ツールのB / b-spinは位相を保つ。ここは意図して違う）。Presentでnが0なら基点をその時刻に取り直して待たない。
//! - 準備時間（`Prep`）= 「起きてからPresentの直前まで」の直近64本のサンプルから決める。
//!   - 64本そろうまでは上位5%（添字`(95n − 1) ÷ 100`）。
//!   - そろってからは上位2位と4位の差が2 ms以下なら2位、超えたら4位（外れ値を避ける）。これに1 msを足した値にする。
//!   - どちらも3 ms〜30 msに収める。
//!
//! 本ツールが違うところ: FPUの制御ワードはBの始まりの入口と同じ（`fldcw 0x007F`）。SwapEffectは替えない。
#![forbid(unsafe_code)]

use crate::pacing::Grid;

/// 準備時間の標本の本数。
pub const PREP_WINDOW: usize = 64;
/// 準備時間の下限・上限・足す余裕・外れ値とみなす2位と4位の差（ms）。vpatchはQPFから`i × QPF ÷ 1000`の表を作る。その3・30・1・2番目を使う。
pub const PREP_MIN_MS: u64 = 3;
pub const PREP_MAX_MS: u64 = 30;
pub const PREP_MARGIN_MS: u64 = 1;
pub const PREP_SPREAD_MS: u64 = 2;
/// 64本そろう前に選ぶ順位（百分率）。
const PREP_PERCENTILE: u64 = 95;
/// そろった後に見る昇順の添字（63が最大）。上位2位 = 62、上位4位 = 60。
const TOP2: usize = PREP_WINDOW - 2;
const TOP4: usize = PREP_WINDOW - 4;

/// `ms`ミリ秒をQPCのカウントにする（切り捨て）。
pub fn ms_to_ticks(ms: u64, qpf: u64) -> u64 {
    (u128::from(ms) * u128::from(qpf) / 1_000).min(u128::from(u64::MAX)) as u64
}

/// 準備時間の見積もり。直近`PREP_WINDOW`本のサンプルを入れた順のリングバッファと昇順の並びの両方で持つ（確保しない。サンプルの入れ替えは並びの中の差し替え）。
#[derive(Debug, Clone)]
pub struct Prep {
    min: u64,
    max: u64,
    margin: u64,
    spread: u64,
    ring: [u64; PREP_WINDOW],
    sorted: [u64; PREP_WINDOW],
    count: u64,
}

impl Prep {
    pub fn new(qpf: u64) -> Prep {
        Prep {
            min: ms_to_ticks(PREP_MIN_MS, qpf),
            max: ms_to_ticks(PREP_MAX_MS, qpf),
            margin: ms_to_ticks(PREP_MARGIN_MS, qpf),
            spread: ms_to_ticks(PREP_SPREAD_MS, qpf),
            ring: [0; PREP_WINDOW],
            sorted: [0; PREP_WINDOW],
            count: 0,
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// サンプル（起きてからPresentの直前までのカウント）を1本足す。64本を越えたら一番古いサンプルを外す。
    pub fn record(&mut self, x: u64) {
        let slot = (self.count % PREP_WINDOW as u64) as usize;
        let len = if self.count >= PREP_WINDOW as u64 {
            let old = self.ring[slot];
            let at = self.sorted.partition_point(|&v| v < old);
            self.sorted.copy_within(at + 1.., at);
            PREP_WINDOW - 1
        } else {
            self.count as usize
        };
        let at = self.sorted[..len].partition_point(|&v| v < x);
        self.sorted.copy_within(at..len, at + 1);
        self.sorted[at] = x;
        self.ring[slot] = x;
        self.count += 1;
    }

    /// いまの準備時間（カウント）。標本が無ければ下限。
    pub fn value(&self) -> u64 {
        let v = if self.count == 0 {
            return self.min;
        } else if self.count < PREP_WINDOW as u64 {
            self.sorted[((PREP_PERCENTILE * self.count - 1) / 100) as usize]
        } else {
            let (top2, top4) = (self.sorted[TOP2], self.sorted[TOP4]);
            let base = if top2.saturating_sub(top4) > self.spread {
                top4
            } else {
                top2
            };
            base.saturating_add(self.margin)
        };
        v.clamp(self.min, self.max)
    }
}

/// フレームの始まりの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    /// 期限`deadline`にPresentする。`wake_at`（= `deadline − 準備時間`）に起きて始める。
    At { deadline: u64, wake_at: u64 },
    /// 始まりで期限を過ぎていた（か最初の1回）。待たずに始めて基点を今に取り直した。
    Rebased,
}

/// 期限の作り方・基点の取り直し・準備時間のサンプルをまとめたもの。ゲームのスレッドだけが触る。
#[derive(Debug, Clone)]
pub struct Vpatch {
    qpf: u64,
    fps: u64,
    grid: Option<Grid>,
    n: u64,
    deadline: u64,
    wake: Option<u64>,
    prep: Prep,
    rebased: u64,
}

impl Vpatch {
    pub fn new(qpf: u64, fps: u64) -> Option<Vpatch> {
        crate::pacing::can_tick(qpf, fps).then(|| Vpatch {
            qpf,
            fps,
            grid: None,
            n: 0,
            deadline: 0,
            wake: None,
            prep: Prep::new(qpf),
            rebased: 0,
        })
    }

    pub fn rebased(&self) -> u64 {
        self.rebased
    }

    pub fn prep(&self) -> &Prep {
        &self.prep
    }

    /// 前のフレームでPresentの直前のサンプルを取りそこねていれば（起きた時刻が残っていれば）ここで取る。
    pub fn sample(&mut self, now: u64) {
        if let Some(w) = self.wake.take() {
            self.prep.record(now.saturating_sub(w));
        }
    }

    /// フレームの始まり。nを1進めて期限を作る。過ぎていなければ起きる時刻を返す。過ぎていたら基点を今に取り直す。
    pub fn begin(&mut self, now: u64) -> Begin {
        self.n += 1;
        if let Some(g) = self.grid {
            let deadline = g.at(self.n);
            if now <= deadline {
                self.deadline = deadline;
                return Begin::At {
                    deadline,
                    wake_at: deadline.saturating_sub(self.prep.value()),
                };
            }
        }
        self.grid = Grid::new(now, self.qpf, self.fps);
        self.n = 0;
        self.deadline = now;
        self.rebased += 1;
        Begin::Rebased
    }

    /// 起きた（フレームを始める）時刻。Presentの直前のサンプルはここからの長さ。
    pub fn woke(&mut self, t: u64) {
        self.wake = Some(t);
    }

    /// Presentの直前に呼ぶ。サンプルを足して待つべき時刻（`Some(期限)`。過ぎていれば`None`）を返す。nが0なら基点をいまに取り直して待たない。
    pub fn before_present(&mut self, now: u64) -> Option<u64> {
        self.sample(now);
        if self.n == 0 {
            self.grid = Grid::new(now, self.qpf, self.fps);
            self.deadline = now;
            return None;
        }
        (now < self.deadline).then_some(self.deadline)
    }
}

/// 目標`target`まで待つとき普通の待機タイマーで寝る長さ（カウント）。目標の`spin`手前まで。残りが無ければ寝ない。寝た後は目標までQPCを読み続ける。
pub fn sleep_ticks(now: u64, target: u64, spin: u64) -> Option<u64> {
    target
        .saturating_sub(spin)
        .checked_sub(now)
        .filter(|&r| r > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QPF: u64 = 10_000_000;

    fn ms(x: u64) -> u64 {
        ms_to_ticks(x, QPF)
    }

    fn prep_with(samples: &[u64]) -> Prep {
        let mut p = Prep::new(QPF);
        for &s in samples {
            p.record(s);
        }
        p
    }

    #[test]
    fn prep_floor_is_three_ms() {
        assert_eq!(Prep::new(QPF).value(), ms(3));
        let p = prep_with(&[ms(1); 10]);
        assert_eq!(p.value(), ms(3));
        let p = prep_with(&[ms(1); 100]);
        assert_eq!(
            p.value(),
            ms(3),
            "そろった後も 1 ms + 1 ms = 2 ms は 3 ms に上げる"
        );
    }

    #[test]
    fn prep_ceiling_is_thirty_ms() {
        assert_eq!(prep_with(&[ms(500); 10]).value(), ms(30));
        assert_eq!(prep_with(&[ms(500); 64]).value(), ms(30));
        assert_eq!(
            prep_with(&[ms(29); 64]).value(),
            ms(30),
            "29 + 余裕 1 ms = 30 ms ちょうど"
        );
    }

    // n = 1 は添字0、n = 20 は添字18、n = 63 は添字59。
    #[test]
    fn prep_before_full_uses_the_95_percentile_without_margin() {
        assert_eq!(prep_with(&[ms(7)]).value(), ms(7));
        let mut s: Vec<u64> = (1..=20).map(|i| ms(3) + i * 1_000).collect();
        s.reverse();
        let p = prep_with(&s);
        assert_eq!(
            p.value(),
            ms(3) + 19 * 1_000,
            "20 本の昇順の添字 18 = 19 番目に小さい値"
        );
        let s63: Vec<u64> = (1..=63).map(|i| ms(4) + i * 1_000).collect();
        assert_eq!(
            prep_with(&s63).value(),
            ms(4) + 60 * 1_000,
            "63 本の添字 59"
        );
    }

    #[test]
    fn prep_full_picks_top2_or_top4_by_spread() {
        // 昇順の60本は4 ms、上位4本を指定する。
        let build = |top: [u64; 4]| {
            let mut v = vec![ms(4); 60];
            v.extend(top);
            prep_with(&v)
        };
        // 上位は昇順で[top4, top3, top2, top1]。2位 = 5 ms・4位 = 4 ms（差1 ms以下）なので2位 + 1 ms。
        assert_eq!(build([ms(4), ms(4), ms(5), ms(9)]).value(), ms(6));
        // 差がちょうど2 msは2位のまま（超えたときだけ4位）。
        assert_eq!(build([ms(4), ms(4), ms(6), ms(9)]).value(), ms(7));
        // 2位が4位より2 msを超えて大きい → 外れ値として4位 + 1 ms。
        assert_eq!(build([ms(4), ms(5), ms(7), ms(9)]).value(), ms(5));
        // 最大（1位）だけが飛び抜けていても見ない。
        assert_eq!(build([ms(4), ms(4), ms(4), ms(25)]).value(), ms(5));
    }

    #[test]
    fn prep_forgets_samples_older_than_64() {
        let mut p = Prep::new(QPF);
        for _ in 0..64 {
            p.record(ms(20));
        }
        assert_eq!(p.value(), ms(21));
        for _ in 0..64 {
            p.record(ms(5));
        }
        assert_eq!(p.value(), ms(6));
        assert_eq!(p.count(), 128);
    }

    #[test]
    fn prep_sorted_window_matches_a_plain_sort() {
        let mut p = Prep::new(QPF);
        let mut all = Vec::new();
        let mut x = 12345u64;
        for _ in 0..500 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = ms(1) + (x >> 40) % 40_000;
            let v = if x & 7 == 0 { ms(3) } else { v };
            p.record(v);
            all.push(v);
            let tail: Vec<u64> = all.iter().rev().take(PREP_WINDOW).copied().collect();
            let mut s = tail.clone();
            s.sort_unstable();
            let n = s.len();
            let want = if n < PREP_WINDOW {
                s[(95 * n - 1) / 100]
            } else if s[TOP2] - s[TOP4] > ms(2) {
                s[TOP4] + ms(1)
            } else {
                s[TOP2] + ms(1)
            }
            .clamp(ms(3), ms(30));
            assert_eq!(p.value(), want, "{} 本目", all.len());
        }
    }

    // 足し算で作るとQPFが割り切れないPCでずれる。
    #[test]
    fn deadline_is_base_plus_n_periods_from_the_grid() {
        let qpf = 3_579_545;
        let mut v = Vpatch::new(qpf, 60).unwrap();
        assert_eq!(v.begin(1_000), Begin::Rebased);
        assert_eq!(v.before_present(1_000), None);
        let g = Grid::new(1_000, qpf, 60).unwrap();
        for n in 1..=200u64 {
            let Begin::At { deadline, wake_at } = v.begin(g.at(n - 1)) else {
                panic!("{n}")
            };
            assert_eq!(deadline, g.at(n), "n = {n}");
            assert_eq!(
                wake_at,
                deadline - ms_to_ticks(3, qpf),
                "標本が無い間は下限 3 ms"
            );
            // 期限どおりにPresentして次の始まりへ。
            assert_eq!(v.before_present(g.at(n) - 10), Some(g.at(n)));
        }
    }

    #[test]
    fn late_head_moves_the_base_to_now_not_the_grid() {
        let mut v = Vpatch::new(QPF, 60).unwrap();
        assert_eq!(v.begin(5_000_000), Begin::Rebased);
        assert_eq!(
            v.before_present(5_000_000),
            None,
            "n = 0 の Present は待たない"
        );
        let period = QPF / 60;
        // 期限 5_000_000 + period を1カウント過ぎて始まりへ来る。
        let late = 5_000_000 + period + 1;
        assert_eq!(v.begin(late), Begin::Rebased);
        assert_eq!(v.rebased(), 2);
        // Presentでも基点を取り直す（始まりの時刻でなくPresentの時刻）。
        let p = late + 30_000;
        assert_eq!(v.before_present(p), None);
        let Begin::At { deadline, .. } = v.begin(p + 100) else {
            panic!()
        };
        assert_eq!(deadline, p + period, "次の期限は Present の時刻から 1 周期");
        // 元の格子に載っていない（位相を保つBなら 5_000_000 + 2 × period になる）。
        assert_ne!(deadline, 5_000_000 + 2 * period);
    }

    // Presentを待たずに次の始まりが来ても期限は取り直した時刻から1周期。
    #[test]
    fn head_rebase_alone_sets_the_base_to_now() {
        let mut v = Vpatch::new(QPF, 60).unwrap();
        v.begin(1_000);
        v.before_present(1_000);
        let late = 1_000 + QPF / 60 + 5_000;
        assert_eq!(v.begin(late), Begin::Rebased);
        let Begin::At { deadline, .. } = v.begin(late + 10) else {
            panic!("2 回目の先頭も置き直しになった")
        };
        assert_eq!(deadline, late + QPF / 60);
        assert_eq!(v.rebased(), 2, "最初の 1 回と遅れた 1 回だけ");
    }

    #[test]
    fn head_exactly_on_deadline_is_not_late() {
        let mut v = Vpatch::new(QPF, 60).unwrap();
        v.begin(1_000);
        v.before_present(1_000);
        let d = Grid::new(1_000, QPF, 60).unwrap().at(1);
        assert!(matches!(v.begin(d), Begin::At { .. }));
        let mut w = Vpatch::new(QPF, 60).unwrap();
        w.begin(1_000);
        w.before_present(1_000);
        assert_eq!(w.begin(d + 1), Begin::Rebased);
    }

    #[test]
    fn prep_sample_is_wake_to_before_present() {
        let mut v = Vpatch::new(QPF, 60).unwrap();
        v.begin(0);
        v.before_present(0);
        let Begin::At { wake_at, .. } = v.begin(100) else {
            panic!()
        };
        v.woke(wake_at);
        // 起きてから4.5 msでPresentの直前へ（期限は起きた3 ms後なのでもう過ぎている）。
        let t = wake_at + ms(4) + ms(1) / 2;
        assert_eq!(v.before_present(t), None);
        assert_eq!(v.prep().count(), 1);
        assert_eq!(v.prep().value(), ms(4) + ms(1) / 2);
        // 同じ起床のサンプルは1回だけ（Presentの直前の後に次の始まりが来ても二重に追加しない）。
        v.sample(t + 1_000_000);
        assert_eq!(v.prep().count(), 1);
        // 期限より前のPresentの直前は期限を返す。
        let Begin::At { deadline, wake_at } = v.begin(t) else {
            panic!()
        };
        v.woke(wake_at);
        assert_eq!(v.before_present(wake_at + 1_000), Some(deadline));
    }

    #[test]
    fn sleep_plan_stops_one_spin_before_the_target() {
        let spin = ms(1);
        assert_eq!(sleep_ticks(0, ms(10), spin), Some(ms(9)));
        assert_eq!(sleep_ticks(0, spin, spin), None);
        assert_eq!(sleep_ticks(0, spin + 1, spin), Some(1));
        assert_eq!(sleep_ticks(ms(10), ms(10), spin), None);
        assert_eq!(
            sleep_ticks(ms(11), ms(10), spin),
            None,
            "過ぎていても溢れない"
        );
    }

    #[test]
    fn needs_a_ticking_qpf() {
        assert!(Vpatch::new(30, 60).is_none());
        assert!(Vpatch::new(60, 60).is_some());
    }
}
