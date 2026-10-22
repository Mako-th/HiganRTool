//! 観戦の送りの量の上限と、対戦の往復の延びで下げる決まり（pure。確保しない）。観戦のホスト（`spectate::SpectateHost`。受信スレッド）が使う。
//! - 上限: 観戦者 1 人への追いつきの送りは、その観戦者の早送りの速さ（`ff_max` 倍）× ふだんの記録の量（`NORMAL_BPS`）まで。全体は、
//!   上限の大きい 2 人ぶんの和まで（`total_cap_bps`）。追いついている観戦者への送りは上限に数えない。
//! - 適応（`Throttle`）: 対戦の往復（`rtt_probe::DelaySnapshot`）の延びが `TARGET_MS` を越えたら全体の上限を半分にし、`CALM_MS` 以下なら
//!   少しずつ戻す（すぐ下げ、ゆっくり戻す。LEDBAT と同じ考え）。下げきって（`FLOOR_PPM`）なお延びていれば、追いついている観戦者への送りも
//!   止める（生きている合図だけ残す）。標本が古い間（メニュー・ポーズ）は下げも戻しもしない。

use crate::rtt_probe::{DelaySnapshot, elapsed};
use crate::spectate_pace::{FF_UPDATES_DEFAULT, FF_UPDATES_MAX, FF_UPDATES_MIN};
use crate::wire::{MAX_STREAM_RECORDS, sealed_frame_stream_len};

/// 1 秒に積まれる記録の数（花映塚の 60 fps）。
pub const RECORDS_PER_SEC: u32 = 60;
/// ふだんの記録の量（B/s）: 1 秒ぶんの記録（`Frame` 60 個）を、追いつきの送りの形（1 通に `MAX_STREAM_RECORDS` 個）で封じた長さ（697 B/s）。
pub const NORMAL_BPS: u32 = RECORDS_PER_SEC * sealed_frame_stream_len(MAX_STREAM_RECORDS) as u32
    / MAX_STREAM_RECORDS as u32;
/// 観戦者 1 人の上限に掛ける余白（÷ 4 で 1.25 倍）: 送り直しの分と、観戦者が往復の間を埋める溜まりを作る分。マッチの間に約 1 秒ごとに積む
/// 対戦の状態（`Record::Stats` 5 B）と照合（`Record::Check` 14 B）の約 19 B/s（`NORMAL_BPS` の約 3%）も、`NORMAL_BPS` には数えずこの余白に入れる。
const CAP_MARGIN_NUM: u32 = 5;
const CAP_MARGIN_DEN: u32 = 4;
/// 上限の、貯められる分（ms ぶん）。受信スレッドの周回が受け待ちの上限（50 ms）まで空いても、上限の速さを落とさない長さ。
pub const BURST_MS: u32 = 64;
/// 貯められる分の下限（B）: いちばん長い STREAM（名前の記録 16 個）1 通。
pub const BURST_MIN_BYTES: u32 = 600;

/// 早送りの速さ `ff_max`（1〜16 に収める）の観戦者への、追いつきの送りの上限（B/s）。
pub const fn watcher_cap_bps(ff_max: u8) -> u32 {
    let ff = ff_max as u32;
    let ff = if ff < FF_UPDATES_MIN {
        FF_UPDATES_MIN
    } else if ff > FF_UPDATES_MAX {
        FF_UPDATES_MAX
    } else {
        ff
    };
    ff * NORMAL_BPS * CAP_MARGIN_NUM / CAP_MARGIN_DEN
}

/// 中継役にしてよい上りの太さの下限（B/s）: 子 4 人へ既定の早送りの上限で配れる太さ。
pub const RELAY_MIN_BPS: u32 = 4 * watcher_cap_bps(FF_UPDATES_DEFAULT as u8);

/// 全体の上限（B/s）: いる観戦者のうち上限の大きい 2 人ぶんの和（1 人なら 1 人ぶん。0 人なら 0）。
pub fn total_cap_bps(ff_maxes: impl Iterator<Item = u8>) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for f in ff_maxes {
        let c = watcher_cap_bps(f);
        if c > a {
            b = a;
            a = c;
        } else if c > b {
            b = c;
        }
    }
    a + b
}

/// 上限 `bps` の貯められる分（B）。
pub fn burst_bytes(bps: u32) -> u32 {
    (bps.saturating_mul(BURST_MS) / 1_000).max(BURST_MIN_BYTES)
}

/// **検査と比べの測りのためだけ**: 固定の全体の上限（B/s。`spectate::SpectateHost::use_fixed_rate` に渡す値）。512 記録のウィンドウを
/// 0.1 秒で出し切る量。
#[doc(hidden)]
pub const FIXED_COMPARE_BPS: u32 = 64_000;

/// 対戦の往復の延びの目安（ms）。これを越えたら下げる。
pub const TARGET_MS: u32 = 20;
/// 戻すのは延びがこれ以下のときだけ（目安との間は保つ。上げ下げを繰り返さない）。
pub const CALM_MS: u32 = 10;
/// 下げる下限（ppm。1/8）: `ff_max` 8 の観戦者 1 人で 1 秒に約 75 記録（積まれる 60 記録をわずかに越える）。これより下げても追いつきは進まない。
pub const FLOOR_PPM: u32 = 125_000;
pub const FULL_PPM: u32 = 1_000_000;
/// 戻す速さ（ppm/秒）: 下限から 7 秒で全部に戻る（ふだんの往復を見る 10 秒より短く、下げる間隔よりずっと長い）。
pub const RAISE_PPM_PER_SEC: u32 = 125_000;
/// 止めた送りを戻すのは、延びが `CALM_MS` 以下のまま、この間続いてから（ms）。止めている間に追いついていた観戦者は
/// `spectate::BEHIND_RECORDS`（64 記録 ≈ 1.07 秒）より遅れるので、戻した後は追いつきの上限の下で送られる（溜まった分を一度に送らない）。
pub const RESUME_HOLD_MS: u32 = 1_500;
/// 標本がこれより古ければ下げも戻しもしない（ms）。対戦の PING の間隔に余裕を足した値。
pub const STALE_MS: u32 = 1_500;
const _: () = assert!(STALE_MS > crate::liveness::PING_INTERVAL_MS);
/// 続けて下げる間隔の下限と上限（ms）。間隔はふだんの往復の 2 倍（下げた効きが標本に出るまで待つ）。
pub const CUT_GAP_MIN_MS: u32 = 100;
pub const CUT_GAP_MAX_MS: u32 = 1_000;
/// 追いつきの送りがこの間に 1 通も無ければ、延びたときに半分にせずすぐ止める（ms）。下げる間隔の上限と同じ長さ: 半分にして減る送りが無い。
pub const CATCHUP_RECENT_MS: u32 = CUT_GAP_MAX_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThrottleCounts {
    /// 半分にした回数。
    pub cuts: u64,
    /// 追いついている観戦者への送りも止めた回数（下げきった後か、追いつきの送りが無いとき）。
    pub pauses: u64,
    /// 延びが収まって、止めていた送りを戻した回数。
    pub resumes: u64,
    /// 止めていた時間の合計（ms）。
    pub paused_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Throttle {
    level: u32,
    paused: bool,
    /// 止めている間に、延びが `CALM_MS` 以下になった時刻（越えたら・標本が古くなったら消す）。
    calm_since: Option<u32>,
    last_cut: Option<u32>,
    last_update: Option<u32>,
    counts: ThrottleCounts,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

impl Throttle {
    pub const fn new() -> Self {
        Self {
            level: FULL_PPM,
            paused: false,
            calm_since: None,
            last_cut: None,
            last_update: None,
            counts: ThrottleCounts {
                cuts: 0,
                pauses: 0,
                resumes: 0,
                paused_ms: 0,
            },
        }
    }

    /// 全体の上限に掛ける割合（ppm。`FLOOR_PPM`〜`FULL_PPM`）。
    pub fn level_ppm(&self) -> u32 {
        self.level
    }

    /// 追いついている観戦者への送りも止めているか。
    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn counts(&self) -> ThrottleCounts {
        self.counts
    }

    fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            self.calm_since = None;
            self.counts.pauses += 1;
        }
    }

    /// `poll` ごとに、そのときの往復の要約で決め直す。`catching_up` は、直近 `CATCHUP_RECENT_MS` に追いつきの送りがあったか。
    pub fn update(&mut self, now: u32, snap: Option<DelaySnapshot>, catching_up: bool) {
        let dt = self.last_update.map_or(0, |t| elapsed(now, t));
        self.last_update = Some(now);
        if self.paused {
            self.counts.paused_ms = self.counts.paused_ms.saturating_add(u64::from(dt));
        }
        let Some(s) = snap.filter(|s| elapsed(now, s.at) <= STALE_MS) else {
            // 古い間の静かな続きを、戻ってきた最初の標本の静かさに足さない。
            self.calm_since = None;
            return;
        };
        let q = s.queuing_ms();
        if q > CALM_MS {
            self.calm_since = None;
        }
        if q > TARGET_MS {
            let gap = (2 * u32::from(s.base_ms)).clamp(CUT_GAP_MIN_MS, CUT_GAP_MAX_MS);
            if self.last_cut.is_some_and(|t| elapsed(now, t) < gap) {
                return;
            }
            self.last_cut = Some(now);
            // 追いつきの送りが無ければ、半分にしても減る流れが無い（追いついている観戦者への送りは上限に数えない）。下げずにすぐ止める。
            if self.level > FLOOR_PPM && catching_up {
                self.level = (self.level / 2).max(FLOOR_PPM);
                self.counts.cuts += 1;
            } else {
                self.pause();
            }
        } else if q <= CALM_MS {
            if self.paused {
                let since = *self.calm_since.get_or_insert(now);
                if elapsed(now, since) >= RESUME_HOLD_MS {
                    self.paused = false;
                    self.calm_since = None;
                    self.counts.resumes += 1;
                }
            } else {
                let raise = u64::from(RAISE_PPM_PER_SEC) * u64::from(dt) / 1_000;
                self.level = (u64::from(self.level) + raise).min(u64::from(FULL_PPM)) as u32;
            }
        }
    }

    /// 上限 `bps` に今の割合を掛けた値。
    pub fn scale(&self, bps: u32) -> u32 {
        (u64::from(bps) * u64::from(self.level) / u64::from(FULL_PPM)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 延び `recent − base` の要約。
    fn snap(at: u32, recent: u16, base: u16) -> Option<DelaySnapshot> {
        Some(DelaySnapshot {
            at,
            queue_ms: recent - base,
            base_ms: base,
        })
    }

    /// 追いつきの送りがある間の決め直し。
    fn up(t: &mut Throttle, now: u32, s: Option<DelaySnapshot>) {
        t.update(now, s, true);
    }

    #[test]
    fn the_caps_follow_ff_max_and_the_two_largest_watchers() {
        assert_eq!(NORMAL_BPS, 697, "60 × 186 B ÷ 16");
        assert_eq!(watcher_cap_bps(8), 8 * 697 * 5 / 4);
        assert_eq!(watcher_cap_bps(0), watcher_cap_bps(1), "範囲の下に丸める");
        assert_eq!(watcher_cap_bps(99), watcher_cap_bps(16), "範囲の上に丸める");
        assert_eq!(total_cap_bps([].into_iter()), 0);
        assert_eq!(total_cap_bps([8].into_iter()), watcher_cap_bps(8));
        assert_eq!(
            total_cap_bps([4, 16, 8, 16, 2].into_iter()),
            2 * watcher_cap_bps(16),
            "大きい 2 人ぶん"
        );
        assert_eq!(
            total_cap_bps([2, 8, 4].into_iter()),
            watcher_cap_bps(8) + watcher_cap_bps(4)
        );
        assert_eq!(burst_bytes(FIXED_COMPARE_BPS), 4_096);
        assert_eq!(burst_bytes(1_000), BURST_MIN_BYTES);
        assert_eq!(burst_bytes(u32::MAX), u32::MAX / 1_000, "桁あふれしない");
    }

    #[test]
    fn a_delay_over_the_target_halves_the_rate_down_to_the_floor_then_pauses() {
        let mut t = Throttle::new();
        // 延び 21 ms（目安を越える）: 半分。ふだんの往復 30 ms → 間隔は下限の 100 ms。
        up(&mut t, 0, snap(0, 51, 30));
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        // 間隔の内は下げない。
        up(&mut t, 99, snap(99, 51, 30));
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        up(&mut t, 100, snap(100, 51, 30));
        assert_eq!(t.level_ppm(), FULL_PPM / 4);
        up(&mut t, 200, snap(200, 51, 30));
        assert_eq!(t.level_ppm(), FLOOR_PPM);
        assert!(!t.paused(), "下限に着いただけでは止めない");
        up(&mut t, 300, snap(300, 51, 30));
        assert!(t.paused(), "下限でなお延びている");
        assert_eq!(t.level_ppm(), FLOOR_PPM);
        up(&mut t, 400, snap(400, 51, 30));
        assert_eq!(
            t.counts(),
            ThrottleCounts {
                cuts: 3,
                pauses: 1,
                resumes: 0,
                paused_ms: 100,
            }
        );
        // 目安ちょうどは下げない。
        let mut u = Throttle::new();
        up(&mut u, 0, snap(0, 50, 30));
        assert_eq!(u.level_ppm(), FULL_PPM);
    }

    #[test]
    fn without_catchup_sends_a_delay_pauses_at_once_without_halving() {
        let mut t = Throttle::new();
        t.update(0, snap(0, 51, 30), false);
        assert!(t.paused(), "追いつきの送りが無いのに半分にした");
        assert_eq!(t.level_ppm(), FULL_PPM, "下げずに止める");
        assert_eq!(t.counts().cuts, 0);
        // 追いつきの送りがあれば今までどおり半分から。
        let mut u = Throttle::new();
        u.update(0, snap(0, 51, 30), true);
        assert!(!u.paused());
        assert_eq!(u.level_ppm(), FULL_PPM / 2);
    }

    #[test]
    fn a_calm_line_resumes_first_then_raises_slowly() {
        let mut t = Throttle::new();
        for k in 0..4 {
            up(&mut t, k * 100, snap(k * 100, 60, 30));
        }
        assert!(t.paused());
        // 目安と静かの間（15 ms）は保つ。
        up(&mut t, 500, snap(500, 45, 30));
        assert!(t.paused());
        // 静かになってから `RESUME_HOLD_MS` 続いたら、止めた送りから戻す。途中で 1 度でも静かでなくなったら数え直す。
        up(&mut t, 600, snap(600, 40, 30));
        up(&mut t, 1_000, snap(1_000, 41, 30));
        up(&mut t, 1_100, snap(1_100, 40, 30));
        let almost = 1_100 + RESUME_HOLD_MS - 1;
        up(&mut t, almost, snap(almost, 40, 30));
        assert!(t.paused(), "静かな続きが足りない");
        let back = 1_100 + RESUME_HOLD_MS;
        up(&mut t, back, snap(back, 40, 30));
        assert!(!t.paused(), "静かが続いたら止めた送りから戻す");
        assert_eq!(t.level_ppm(), FLOOR_PPM);
        // 1 秒で `RAISE_PPM_PER_SEC` だけ戻る。
        for ms in back + 1..=back + 1_000 {
            up(&mut t, ms, snap(ms, 40, 30));
        }
        assert_eq!(t.level_ppm(), FLOOR_PPM + RAISE_PPM_PER_SEC);
        for ms in back + 1_001..back + 20_000 {
            up(&mut t, ms, snap(ms, 40, 30));
        }
        assert_eq!(t.level_ppm(), FULL_PPM, "全部より上には戻らない");
    }

    #[test]
    fn a_stale_gap_while_paused_restarts_the_calm_hold() {
        let mut t = Throttle::new();
        t.update(0, snap(0, 60, 30), false);
        assert!(t.paused());
        // 静かになり始めた後、標本が古くなった（メニューなど）。
        up(&mut t, 100, snap(100, 30, 30));
        up(&mut t, 100 + STALE_MS + 1, snap(100, 30, 30));
        // 戻ってきた最初の静かな標本ですぐ戻さない。そこから `RESUME_HOLD_MS` 続いてから。
        let back = 100 + RESUME_HOLD_MS + 500;
        up(&mut t, back, snap(back, 30, 30));
        assert!(t.paused(), "古い間の静かさを足して戻した");
        up(
            &mut t,
            back + RESUME_HOLD_MS,
            snap(back + RESUME_HOLD_MS, 30, 30),
        );
        assert!(!t.paused());
    }

    #[test]
    fn a_stale_or_missing_sample_neither_cuts_nor_raises() {
        let mut t = Throttle::new();
        up(&mut t, 0, snap(0, 60, 30));
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        // 古い延びた標本で下げ続けない。
        for ms in (STALE_MS + 1..10_000).step_by(50) {
            up(&mut t, ms, snap(0, 60, 30));
        }
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        // 古い静かな標本で戻し続けない。標本が無いときも同じ。
        for ms in (10_000..20_000).step_by(50) {
            up(&mut t, ms, snap(8_000, 30, 30));
            up(&mut t, ms + 1, None);
        }
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        // 新しい標本が来れば戻り始める。
        up(&mut t, 20_000, snap(20_000, 30, 30));
        up(&mut t, 21_000, snap(21_000, 30, 30));
        assert!(t.level_ppm() > FULL_PPM / 2);
    }

    #[test]
    fn a_delay_flickering_around_the_thresholds_does_not_drift_the_level() {
        // 19 / 21 ms を 1 ms ごとに行き来し、同じ ms に何度も呼ばれる: 下げるのは間隔ごとに 1 回だけ（呼んだ数に比例しない）。
        let mut t = Throttle::new();
        for ms in 0..250u32 {
            for k in 0..3 {
                let q = if (ms + k) % 2 == 0 { 49 } else { 51 };
                up(&mut t, ms, snap(ms, q, 30));
            }
        }
        assert_eq!(t.counts().cuts, 3, "0・100・200 ms の 3 回");
        // 9 / 11 ms を行き来: 戻すのは静かだった ms の分だけ（静かでない ms の分を後でまとめて足さない）。
        let mut t = Throttle::new();
        up(&mut t, 0, snap(0, 60, 30));
        let start = t.level_ppm();
        for ms in 1..=1_000u32 {
            let q = if ms % 2 == 0 { 39 } else { 41 };
            up(&mut t, ms, snap(ms, q, 30));
            up(&mut t, ms, snap(ms, q, 30));
        }
        assert_eq!(t.level_ppm(), start + RAISE_PPM_PER_SEC / 2);
    }

    #[test]
    fn a_huge_gap_between_updates_does_not_overflow_the_raise() {
        let mut t = Throttle::new();
        up(&mut t, 0, snap(0, 60, 30));
        up(&mut t, u32::MAX / 2 - 1, snap(u32::MAX / 2 - 1, 30, 30));
        assert_eq!(t.level_ppm(), FULL_PPM);
    }

    #[test]
    fn the_cut_gap_follows_the_base_round_trip() {
        let mut t = Throttle::new();
        // ふだんの往復 300 ms: 次に下げるのは 600 ms 後。
        up(&mut t, 0, snap(0, 330, 300));
        up(&mut t, 599, snap(599, 330, 300));
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        up(&mut t, 600, snap(600, 330, 300));
        assert_eq!(t.level_ppm(), FULL_PPM / 4);
        assert_eq!(t.scale(10_000), 2_500);
    }
}
