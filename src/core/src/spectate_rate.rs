//! 観戦の送信量の上限と、対戦の往復の延びで下げる決まり（pure。確保しない）。
//! 観戦のホスト（`spectate::SpectateHost`。受信スレッド）が使う。
//! - 上限: 観戦者1人への追いつきの送信は、その観戦者の早送りの速さ（`ff_max`倍）× ふだんの記録の量（`NORMAL_BPS`）まで。
//!   全体は上限の大きい2人ぶんの和まで（`total_cap_bps`）。追いついている観戦者への送信は上限に数えない。
//! - 適応（`Throttle`）: 対戦の往復（`rtt_probe::DelaySnapshot`）の延びが`TARGET_MS`を越えたら全体の上限を半分にする。`CALM_MS`以下なら少しずつ戻す（すぐ下げてゆっくり戻す。LEDBATと同じ考え）。
//!   下げきって（`FLOOR_PPM`）なお延びていれば、追いついている観戦者への送信も止める（生存確認の空のSTREAMだけ残す）。
//!   サンプルが古い間（メニュー・ポーズ）は下げも戻しもしない。
//!   追いつきの送信を始めるたびにサンプルがあれば、割合をホストの上り（接続時に相手が測った値）の`UPLINK_SHARE_DEN`分の1までに下げてから戻し始める（上りが分からなければ下限から）。
//!   延びは2秒のウィンドウで見て揺れを無視する。直近の数個の延びでは上げるのをやめる。急な伸び（`SURGE_MS`）ならウィンドウを待たずに下げる。

use crate::rtt_probe::{DelaySnapshot, elapsed};
use crate::spectate_pace::{FF_UPDATES_DEFAULT, FF_UPDATES_MAX, FF_UPDATES_MIN};
use crate::wire::{MAX_STREAM_RECORDS, sealed_frame_stream_len};

/// 1秒に積まれる記録の数（花映塚の60 fps）。
pub const RECORDS_PER_SEC: u32 = 60;
/// ふだんの記録の量（B/s）: 1秒ぶんの記録（`Frame` 60個）を追いつきの送信の形（1通に`MAX_STREAM_RECORDS`個）で封じた長さ（697 B/s）。
pub const NORMAL_BPS: u32 = RECORDS_PER_SEC * sealed_frame_stream_len(MAX_STREAM_RECORDS) as u32
    / MAX_STREAM_RECORDS as u32;
/// 観戦者1人の上限に掛ける余白（÷ 4 で1.25倍）: 再送の分と、観戦者が往復の間を埋める溜まりを作る分。
/// マッチの間に約1秒ごとに積む対戦の状態（`Record::Stats` 5 B）と照合（`Record::Check` 14 B）の約19 B/s（`NORMAL_BPS`の約3%）も、`NORMAL_BPS`には数えずこの余白に含める。
const CAP_MARGIN_NUM: u32 = 5;
const CAP_MARGIN_DEN: u32 = 4;
/// 上限の貯められる分（msぶん）。受信スレッドの周回が受け待ちの上限（50 ms）まで空いても上限の速さを落とさない長さ。
pub const BURST_MS: u32 = 64;
/// 貯められる分の下限（B）: いちばん長いSTREAM（名前の記録16個）1通。
pub const BURST_MIN_BYTES: u32 = 600;

/// 早送りの速さ`ff_max`（1〜16に収める）の観戦者への追いつきの送信の上限（B/s）。
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

/// 中継役にしてよい上りの太さの下限（B/s）: 子4人へ既定の早送りの上限で配信できる太さ。
pub const RELAY_MIN_BPS: u32 = 4 * watcher_cap_bps(FF_UPDATES_DEFAULT as u8);

/// 全体の上限（B/s）: いる観戦者のうち上限の大きい2人ぶんの和（1人なら1人ぶん。0人なら0）。
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

/// 上限`bps`の貯められる分（B）。
pub fn burst_bytes(bps: u32) -> u32 {
    (bps.saturating_mul(BURST_MS) / 1_000).max(BURST_MIN_BYTES)
}

/// **検査と比較の計測のためだけ**: 固定の全体の上限（B/s。`spectate::SpectateHost::use_fixed_rate`に渡す値）。
/// 512記録のウィンドウを0.1秒で出し切る量。
#[doc(hidden)]
pub const FIXED_COMPARE_BPS: u32 = 64_000;

/// 対戦の往復の延びの目安（ms）。これを越えたら下げる。
pub const TARGET_MS: u32 = 20;
/// 戻すのは延びがこれ以下のときだけ（目安との間は保つ。上げ下げを繰り返さない）。
pub const CALM_MS: u32 = 10;
/// 直近の数個の延び（`DelaySnapshot::fresh_ms`）がこれを越えたらウィンドウを待たずに目安を越えたのと同じに扱う（ms）。
/// 1通ごとに0〜80 ms揺れる回線でも越えず、上りの列が一気に伸びたときに越える大きさ。
pub const SURGE_MS: u32 = 60;
/// 下げる下限（ppm。1/8）: `ff_max` 8の観戦者1人で1秒に約75記録（積まれる60記録をわずかに越える）。これより下げても追いつきは進まない。
pub const FLOOR_PPM: u32 = 125_000;
pub const FULL_PPM: u32 = 1_000_000;
/// 戻す速さ（ppm/秒）: 下限から7秒で全部に戻る（ふだんの往復を見る10秒より短く、下げる間隔よりずっと長い）。
pub const RAISE_PPM_PER_SEC: u32 = 125_000;
/// 止めた送信を戻すのは延びが`CALM_MS`以下のままこの間続いてから（ms）。
/// 止めている間に追いついていた観戦者は`spectate::BEHIND_RECORDS`（64記録 ≈ 1.07秒）より遅れる。
/// そのため戻した後は追いつきの上限の下で送られる（溜まった分を一度に送らない）。
pub const RESUME_HOLD_MS: u32 = 1_500;
/// サンプルがこれより古ければ下げも戻しもしない（ms）。対戦のPINGの間隔に余裕を足した値。
pub const STALE_MS: u32 = 1_500;
const _: () = assert!(STALE_MS > crate::liveness::PING_INTERVAL_MS);
/// 続けて下げる間隔の下限と上限（ms）。間隔はふだんの往復の2倍（下げた効きがサンプルに出るまで待つ）。
pub const CUT_GAP_MIN_MS: u32 = 100;
pub const CUT_GAP_MAX_MS: u32 = 1_000;
/// 追いつきの送信がこの間に1通も無ければ延びたときに半分にせずすぐ止める（ms）。下げる間隔の上限と同じ長さ: 半分にしても減る送信が無い。
pub const CATCHUP_RECENT_MS: u32 = CUT_GAP_MAX_MS;
/// 追いつきの送信の始めに使ってよいホストの上りの割合（分の1）。
/// 往復の延びがウィンドウ（`rtt_probe::RECENT_MS`）を待って下げ始めるまでに、対戦の便を押しのけない量。
pub const UPLINK_SHARE_DEN: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThrottleCounts {
    /// 半分にした回数。
    pub cuts: u64,
    /// 追いついている観戦者への送信も止めた回数（下げきった後か追いつきの送信が無いとき）。
    pub pauses: u64,
    /// 延びが収まって止めていた送信を戻した回数。
    pub resumes: u64,
    /// 止めていた時間の合計（ms）。
    pub paused_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Throttle {
    level: u32,
    paused: bool,
    /// 止めている間に延びが`CALM_MS`以下になった時刻（越えたら・サンプルが古くなったら消す）。
    calm_since: Option<u32>,
    last_cut: Option<u32>,
    last_update: Option<u32>,
    /// 前の`update`で追いつきの送信があったかどうか。
    catching: bool,
    /// ホストの上り（B/s。0は分からない）。
    uplink_bps: u32,
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
            catching: false,
            uplink_bps: 0,
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

    /// 追いついている観戦者への送信も止めているかどうか。
    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn counts(&self) -> ThrottleCounts {
        self.counts
    }

    /// ホストの上り（B/s。接続のときに相手が測った値。0は分からない）。
    pub fn set_uplink(&mut self, bps: u32) {
        self.uplink_bps = bps;
    }

    pub fn uplink_bps(&self) -> u32 {
        self.uplink_bps
    }

    /// 全体の上限`cap_bps`で追いつきの送信を始めるときの割合の上限。
    pub fn start_ppm(&self, cap_bps: u32) -> u32 {
        if self.uplink_bps == 0 || cap_bps == 0 {
            return FLOOR_PPM;
        }
        let share = u64::from(self.uplink_bps / UPLINK_SHARE_DEN);
        (share * u64::from(FULL_PPM) / u64::from(cap_bps))
            .clamp(u64::from(FLOOR_PPM), u64::from(FULL_PPM)) as u32
    }

    fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            self.calm_since = None;
            self.counts.pauses += 1;
        }
    }

    /// `poll`ごとにそのときの往復の要約で決め直す。
    /// `catching_up`は直近`CATCHUP_RECENT_MS`に追いつきの送信があったかどうか。`cap_bps`は今の全体の上限。
    pub fn update(
        &mut self,
        now: u32,
        snap: Option<DelaySnapshot>,
        catching_up: bool,
        cap_bps: u32,
    ) {
        let dt = self.last_update.map_or(0, |t| elapsed(now, t));
        self.last_update = Some(now);
        let snap = snap.filter(|s| elapsed(now, s.at) <= STALE_MS);
        // 始めの割合へ下げるのはサンプルがあって後で戻せるときだけ（対戦の往復が無い中継役の観戦者は全部のまま）。
        if catching_up && !self.catching && snap.is_some() {
            self.level = self.level.min(self.start_ppm(cap_bps));
        }
        self.catching = catching_up;
        if self.paused {
            self.counts.paused_ms = self.counts.paused_ms.saturating_add(u64::from(dt));
        }
        let Some(s) = snap else {
            // 古い間の静かな続きを戻ってきた最初のサンプルの静かさに足さない。
            self.calm_since = None;
            return;
        };
        let q = s.queuing_ms();
        let surge = u32::from(s.fresh_ms) > SURGE_MS;
        if q > CALM_MS || surge {
            self.calm_since = None;
        }
        if q > TARGET_MS || surge {
            let gap = (2 * u32::from(s.base_ms)).clamp(CUT_GAP_MIN_MS, CUT_GAP_MAX_MS);
            if self.last_cut.is_some_and(|t| elapsed(now, t) < gap) {
                return;
            }
            self.last_cut = Some(now);
            // 追いつきの送信が無ければ半分にしても減る流れが無い（追いついている観戦者への送信は上限に数えない）。下げずにすぐ止める。
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
            } else if u32::from(s.fresh_ms) <= CALM_MS {
                // 上げるのは直近の数個も静かなときだけ（列のでき始めはウィンドウの延びに出る前に上げるのをやめる。揺れの間は保つだけ）。
                let raise = u64::from(RAISE_PPM_PER_SEC) * u64::from(dt) / 1_000;
                self.level = (u64::from(self.level) + raise).min(u64::from(FULL_PPM)) as u32;
            }
        }
    }

    /// 上限`bps`に今の割合を掛けた値。
    pub fn scale(&self, bps: u32) -> u32 {
        (u64::from(bps) * u64::from(self.level) / u64::from(FULL_PPM)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 延び`recent − base`の要約。
    fn snap(at: u32, recent: u16, base: u16) -> Option<DelaySnapshot> {
        Some(DelaySnapshot {
            at,
            queue_ms: recent - base,
            fresh_ms: recent - base,
            base_ms: base,
        })
    }

    /// 検査の全体の上限（B/s）。
    const CAP: u32 = 10_000;

    /// 追いつきの送信がある間の決め直し。上りは十分に太い（始めの割合は全部）。
    fn up(t: &mut Throttle, now: u32, s: Option<DelaySnapshot>) {
        t.set_uplink(u32::MAX);
        t.update(now, s, true, CAP);
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
        // 延び21 ms（目安を越える）: 半分。ふだんの往復30 ms → 間隔は下限の100 ms。
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
    fn a_catchup_starts_at_a_quarter_of_the_hosts_uplink_or_at_the_floor_when_unknown() {
        let calm = |at| snap(at, 30, 30);
        // 上りが分からない: 下限から始めて静かなら戻す速さで上げる。
        let mut t = Throttle::new();
        t.update(0, calm(0), true, CAP);
        assert_eq!(t.level_ppm(), FLOOR_PPM);
        t.update(1_000, calm(1_000), true, CAP);
        assert_eq!(t.level_ppm(), FLOOR_PPM + RAISE_PPM_PER_SEC);
        // 上りの1/4（8,000 B/s ÷ 4 = 上限10,000の20%）から始める。太い上りなら全部。細い上りでも下限より下げない。
        for (uplink, want) in [(8_000, 200_000), (u32::MAX, FULL_PPM), (1_000, FLOOR_PPM)] {
            let mut u = Throttle::new();
            u.set_uplink(uplink);
            u.update(0, calm(0), true, CAP);
            assert_eq!(u.level_ppm(), want, "上り {uplink} B/s");
        }
        // 追いつきの送信が途切れてから始め直すとまた始めの割合まで下げる。下げた割合は上げない。
        let mut v = Throttle::new();
        v.set_uplink(8_000);
        for ms in (0..=10_000).step_by(100) {
            v.update(ms, calm(ms), true, CAP);
        }
        assert_eq!(v.level_ppm(), FULL_PPM);
        v.update(10_100, calm(10_100), false, CAP);
        v.update(10_200, calm(10_200), true, CAP);
        assert_eq!(
            v.level_ppm(),
            200_000 + RAISE_PPM_PER_SEC / 10,
            "始めの割合へ下げ、その 100 ms ぶん戻す"
        );
        let mut w = Throttle::new();
        w.update(0, calm(0), true, CAP);
        w.update(100, calm(100), false, CAP);
        w.set_uplink(u32::MAX);
        w.update(200, calm(200), true, CAP);
        assert!(
            w.level_ppm() < FULL_PPM / 2,
            "下げた割合を始めの割合まで上げた"
        );
        // サンプルが無い・古い（対戦の往復が無い中継役など）なら戻せないので下げない。
        let mut x = Throttle::new();
        x.update(0, None, true, CAP);
        x.update(5_000, snap(0, 30, 30), false, CAP);
        x.update(5_100, snap(0, 30, 30), true, CAP);
        assert_eq!(x.level_ppm(), FULL_PPM, "戻す標本が無いのに下げた");
        assert_eq!(Throttle::new().start_ppm(0), FLOOR_PPM, "上限 0 で割らない");
    }

    #[test]
    fn a_rise_only_in_the_last_few_samples_holds_the_level_without_cutting() {
        let mut t = Throttle::new();
        up(&mut t, 0, snap(0, 60, 30));
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        let fresh_only = |at| {
            Some(DelaySnapshot {
                at,
                queue_ms: 0,
                fresh_ms: 30,
                base_ms: 30,
            })
        };
        for ms in (100..=3_000).step_by(100) {
            up(&mut t, ms, fresh_only(ms));
        }
        assert_eq!(t.level_ppm(), FULL_PPM / 2, "直近が延びている間に上げた");
        assert_eq!(
            (t.counts().cuts, t.counts().pauses),
            (1, 0),
            "直近の延びで下げた"
        );
        up(&mut t, 3_100, snap(3_100, 30, 30));
        assert_eq!(t.level_ppm(), FULL_PPM / 2 + RAISE_PPM_PER_SEC / 10);
    }

    #[test]
    fn without_catchup_sends_a_delay_pauses_at_once_without_halving() {
        let mut t = Throttle::new();
        t.update(0, snap(0, 51, 30), false, CAP);
        assert!(t.paused(), "追いつきの送りが無いのに半分にした");
        assert_eq!(t.level_ppm(), FULL_PPM, "下げずに止める");
        assert_eq!(t.counts().cuts, 0);
        // 追いつきの送信があれば今までどおり半分から。
        let mut u = Throttle::new();
        up(&mut u, 0, snap(0, 51, 30));
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
        // 静かになってから`RESUME_HOLD_MS`続いたら止めた送信を戻す。途中で1度でも静かでなくなったら数え直す。
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
        // 1秒で`RAISE_PPM_PER_SEC`だけ戻る。
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
        t.update(0, snap(0, 60, 30), false, CAP);
        assert!(t.paused());
        // 静かになり始めた後にサンプルが古くなった（メニューなど）。
        up(&mut t, 100, snap(100, 30, 30));
        up(&mut t, 100 + STALE_MS + 1, snap(100, 30, 30));
        // 戻ってきた最初の静かなサンプルですぐ戻さない。そこから`RESUME_HOLD_MS`続いてから。
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
        // 延びを示す古いサンプルで下げ続けない。
        for ms in (STALE_MS + 1..10_000).step_by(50) {
            up(&mut t, ms, snap(0, 60, 30));
        }
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        // 古い静かなサンプルで戻し続けない。サンプルが無いときも同じ。
        for ms in (10_000..20_000).step_by(50) {
            up(&mut t, ms, snap(8_000, 30, 30));
            up(&mut t, ms + 1, None);
        }
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        // 新しいサンプルが来れば戻り始める。
        up(&mut t, 20_000, snap(20_000, 30, 30));
        up(&mut t, 21_000, snap(21_000, 30, 30));
        assert!(t.level_ppm() > FULL_PPM / 2);
    }

    #[test]
    fn a_delay_flickering_around_the_thresholds_does_not_drift_the_level() {
        // 19 / 21 msを1 msごとに行き来し、同じmsに何度も呼ばれる。下げるのは間隔ごとに1回だけ（呼んだ数に比例しない）。
        let mut t = Throttle::new();
        for ms in 0..250u32 {
            for k in 0..3 {
                let q = if (ms + k) % 2 == 0 { 49 } else { 51 };
                up(&mut t, ms, snap(ms, q, 30));
            }
        }
        assert_eq!(t.counts().cuts, 3, "0・100・200 ms の 3 回");
        // 9 / 11 msを行き来: 戻すのは静かだったmsの分だけ（静かでないmsの分を後でまとめて足さない）。
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
        // ふだんの往復300 ms: 次に下げるのは600 ms後。
        up(&mut t, 0, snap(0, 330, 300));
        up(&mut t, 599, snap(599, 330, 300));
        assert_eq!(t.level_ppm(), FULL_PPM / 2);
        up(&mut t, 600, snap(600, 330, 300));
        assert_eq!(t.level_ppm(), FULL_PPM / 4);
        assert_eq!(t.scale(10_000), 2_500);
    }
}
