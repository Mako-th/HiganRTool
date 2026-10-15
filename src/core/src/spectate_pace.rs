//! 観戦者の早送りの速さと、止まった後に余裕を作り直す決まり（pure。確保しない）。dll の観戦者（`spectate_watch/ff.rs`・`lockstep_tick`）と
//! core の模型（`tests/spectate_model.rs`）が同じものを使う。
//!
//! 遅れて入った観戦者には、ホストの送りのウィンドウ（`spectate::SEND_WINDOW` = 64 記録）が往復ごとにまとまって届く。溜まりだけを見て
//! 「越えたら最大の速さ・減ったら止める」と決めると、往復が長い回線では「最大の速さ → 溜まりが尽きて止まる」を往復ごとに繰り返す。
//! ここでは次の 2 つで、届く平均の速さに合わせて途切れず進める:
//!
//! - **取っておく数**（`reserve`）= 余裕 ＋ 届く平均の速さ × 直近 `PACE_WINDOW` 表示で何も届かなかった表示の最長の続き。往復ごとに
//!   まとめて届く間は、次のまとまりまで平均の速さで進んでも尽きない分を残す。1 表示ごとに届く（追いついた）間は余裕のまま。
//! - **速さ** = 届く平均の速さ（直近 `PACE_WINDOW` 表示。1 が普段の速さ）＋（溜まり − 取っておく数）÷ `PACE_GAIN`（取っておく数より
//!   少なければその `PACE_SHORT_DAMP` 倍で割る）。端数は次の表示へ持ち越す（2.5 倍なら余分 1 回と 2 回を交互に）。取っておく数を
//!   1 ウィンドウぶん越えた溜まりと、まとまりで届いていない（追いついた）ときの越えた分は、遅れそのものなので最大の速さで減らす。
//!
//! 止まった（使う記録が無かった）後は、溜まりが戻るまで（`REFILL_HOLD_DISPLAYS` まで）待ってから使い始める。待たずに 1 記録ずつ
//! 使うと、すぐまた止まる。戻す数は、まとまりで届いている間は `reserve`（往復が長いほど何も届かない続きが長く、多く溜める）、1 表示に
//! 1 つずつ届いている間は余裕だけ（ホストの一時停止や落ちた便の待ちは往復ごとにくり返さないので、止まった長さぶん待ち足さない）。

/// 早送りに入る溜まり: 使っていない記録がこの数を越えたら入る。ふだんの溜まりは余裕（3）前後で、ホストの送りが 1 フレーム強ぶれても
/// 5 を越えない。6 は、100 ms 以上遅れたときだけ入る値。
pub const FF_ENTER_FRAMES: u32 = 6;
/// 早送りから戻る溜まり: この数以下になったら普段の速さに戻す。余裕と同じ 3 にして、戻った後も記録を待たずに続けられるようにする。
pub const FF_EXIT_FRAMES: u32 = 3;
/// 1 回の表示の中で余分に回す更新の数の上限（最大 17 倍速 = 普段の 1 回 ＋ 余分 16 回 ≒ 毎秒 1,020 更新）。ペースの格子の上限
/// `MAX_FPS`（1000）に寄せた。遅い PC では dll の時間の上限（`FF_BUDGET_US`）が先に効き、回せる速さまで自然に落ちる。更新 1 回は
/// p50 0.04 / p99 0.07 ms（n = 660。弾の多い場面は重い見込み）なので、16 回で 1 ms 前後の見込み（外挿）。
pub const FF_MAX_EXTRA: u32 = 16;
/// 届いた速さと何も届かない続きを見る表示の数（60 Hz で約 1.07 秒）。往復 1 秒までの回線で、往復ごとのまとまりを 1 つ以上含む。
pub const PACE_WINDOW: usize = 64;
/// 取っておく数を越えた溜まりを、何表示かけて減らすか（約 0.13 秒）。小さいと速さが表示ごとに大きく揺れ、大きいと溜まりを減らすのが
/// 遅い（core の模型の追いつきの表で、止まりが最も少なかった値）。
pub const PACE_GAIN: u32 = 8;
/// 溜まりが取っておく数より少ないときに、`PACE_GAIN` を何倍に弱めて速さを落とすか（届く速さが最大の速さに近い短い往復で、まとまりの
/// 間の谷のたびに速さを落とさない）。
pub const PACE_SHORT_DAMP: u32 = 4;
/// 止まった後、溜まりが戻るのを待ってよい表示の数（1 秒）。往復 600 ms の回線でも、まとまりが 1 つ届く長さ。
pub const REFILL_HOLD_DISPLAYS: u32 = 60;

use crate::spectate::SEND_WINDOW;

/// 速さの単位（1 表示に普段の 1 回 = 1,000）。
const MILLI: u32 = 1_000;
/// 届く平均がこれ以下（1 表示に 1.5 記録）なら、まとまりで届いていない（追いついて 1 表示に 1 つずつ届いている）と見る。
const LIVE_RATE_MILLI: u32 = 1_500;
/// 1 つのまとまりとして足す表示の数。ウィンドウの 4 通は、観戦者が 1 通ごとに返す ACK に合わせてホストが送るので、往復の長い回線では
/// 数表示に分かれて届く（core の模型の往復 600 ms で 16・16・32 記録が 3 表示に）。
const BURST_SPAN: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// 直近の表示ごとに使えるようになった記録の数（輪）。
    arrived: [u16; PACE_WINDOW],
    pos: usize,
    sum: u32,
    /// 最初に記録が届いてからの表示の数（`PACE_WINDOW` まで）。平均の分母（入った直後の 1 秒を、届く前の 0 で薄めない）。
    seen: u32,
    /// 持ち越した端数（`MILLI` 単位）。
    credit: u32,
    active: bool,
    /// 止まった後、溜まりが戻るのを待った表示の数（待っていなければ `None`）。
    refill: Option<u32>,
}

impl Default for Pace {
    fn default() -> Self {
        Self::new()
    }
}

impl Pace {
    pub const fn new() -> Self {
        Self {
            arrived: [0; PACE_WINDOW],
            pos: 0,
            sum: 0,
            seen: 0,
            credit: 0,
            active: false,
            refill: None,
        }
    }

    /// 表示の始まりに、前の表示の間に使えるようになった記録の数（組み立ての連続の終わりが進んだ数）を入れる。
    pub fn on_display(&mut self, arrived: u32) {
        let a = arrived.min(u32::from(u16::MAX)) as u16;
        if self.seen > 0 || a > 0 {
            self.seen = (self.seen + 1).min(PACE_WINDOW as u32);
        }
        if let Some(slot) = self.arrived.get_mut(self.pos) {
            self.sum = self.sum - u32::from(*slot) + u32::from(a);
            *slot = a;
        }
        self.pos = (self.pos + 1) % PACE_WINDOW;
    }

    /// 届く平均の速さ（1 表示あたりの記録の数 × `MILLI`）。
    fn rate(&self) -> u32 {
        self.sum.saturating_mul(MILLI) / self.seen.max(1)
    }

    /// 使わずに取っておく記録の数（`margin` は余裕の記録の数）: 余裕 ＋ 平均の速さ × 直近の窓で何も届かなかった表示の最長の続き。
    /// 往復ごとにまとめて届く間は、何も届かない間を平均の速さで進んでも尽きない分を残す。1 表示ごとに届く（追いついた）間は余裕のまま。
    /// ただし、続けて `BURST_SPAN` 表示に届いた最大の数（1 つのまとまり）を越えては取っておかない（長く止まった後の 1 回きりの
    /// まとまりで、まとまりより多くを待たない）。
    pub fn reserve(&self, margin: u32) -> u32 {
        let (mut run, mut gap) = (0u32, 0u32);
        let (mut span, mut burst) = (0u32, 0u32);
        let n = (self.seen as usize).min(PACE_WINDOW);
        for age in 0..n {
            let at = |age: usize| {
                let i = (self.pos + PACE_WINDOW - 1 - age) % PACE_WINDOW;
                self.arrived.get(i).copied().map_or(0, u32::from)
            };
            let a = at(age);
            if a == 0 {
                run += 1;
                gap = gap.max(run);
            } else {
                run = 0;
            }
            span += a;
            if age >= BURST_SPAN {
                span -= at(age - BURST_SPAN);
            }
            burst = burst.max(span);
        }
        let need = self.rate().saturating_mul(gap) / MILLI;
        margin.saturating_add(need.min(burst))
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 早送りをやめる（追いついた・マッチの外へ出た・止まった）。
    pub fn leave(&mut self) {
        self.active = false;
        self.credit = 0;
    }

    /// この表示で余分に回す更新の数（`backlog` はこの表示の普段の 1 回を使った後の、使っていない記録の数）。早送りでなければ溜まりが
    /// `FF_ENTER_FRAMES` を越えたら入り、`FF_EXIT_FRAMES` 以下で戻る。溜まりを `FF_EXIT_FRAMES` より下へは減らさない。
    pub fn extras(&mut self, backlog: u32, margin: u32) -> u32 {
        if self.active {
            if backlog <= FF_EXIT_FRAMES {
                self.leave();
                return 0;
            }
        } else if backlog > FF_ENTER_FRAMES {
            self.active = true;
        } else {
            return 0;
        }
        let rate = self.rate();
        let excess = i64::from(backlog) - i64::from(self.reserve(margin));
        let gain = if excess < 0 {
            PACE_GAIN * PACE_SHORT_DAMP
        } else {
            PACE_GAIN
        };
        let top = i64::from(MILLI * (1 + FF_MAX_EXTRA));
        // 取っておく数を 1 ウィンドウぶん越えた溜まりは、まとまりの揺れ（最大 1 ウィンドウ）ではなく遅れそのもの。届くのが普段の速さ
        // （まとまりで届いていない）なら、越えた分はどれも遅れ。どちらも最大の速さで減らす（追いつきの終わり・ホストが止まった後）。
        let live = rate <= LIVE_RATE_MILLI;
        let speed = if excess > i64::from(SEND_WINDOW) || (live && excess > 0) {
            top
        } else {
            i64::from(rate) + excess * i64::from(MILLI) / i64::from(gain)
        };
        let speed = speed.clamp(i64::from(MILLI), top) as u32;
        self.credit = self.credit.saturating_add(speed - MILLI);
        let n = (self.credit / MILLI)
            .min(FF_MAX_EXTRA)
            .min(backlog - FF_EXIT_FRAMES);
        self.credit -= n * MILLI;
        // 上限で切った分は持ち越さない（後でまとめて回さない）。
        self.credit = self.credit.min(MILLI - 1);
        n
    }

    /// 使う記録が無くて止まった（マッチの途中）。次に使う前に溜まりが戻るのを待つ。
    pub fn on_stall(&mut self) {
        self.leave();
        self.refill.get_or_insert(0);
    }

    /// この表示で記録を使ってよいか。止まった後は、溜まり `backlog` が戻るまで（`REFILL_HOLD_DISPLAYS` 表示まで）false（描画だけの
    /// フレーム）。戻す数は、まとまりで届いている（追いついている途中の）間は `reserve(margin)`、1 表示に 1 つずつ届いている間は余裕だけ
    /// （ホストが一時停止した・落ちた便を待った後に、止まった長さぶん待ち足さない）。
    pub fn may_play(&mut self, backlog: u32, margin: u32) -> bool {
        let Some(n) = self.refill else {
            return true;
        };
        let target = if self.rate() > LIVE_RATE_MILLI {
            self.reserve(margin)
        } else {
            margin
        };
        if backlog < target && n < REFILL_HOLD_DISPLAYS {
            self.refill = Some(n + 1);
            return false;
        }
        self.refill = None;
        true
    }

    /// 止まった後の待ちの中か。
    pub fn refilling(&self) -> bool {
        self.refill.is_some()
    }

    /// 新しい seg の始まり（読み込みの間に届いたまとまりや、前のマッチの止まりを持ち越さない）。
    pub fn reset_seg(&mut self) {
        self.leave();
        self.refill = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(per_display: &[u32]) -> Pace {
        let mut p = Pace::new();
        for &a in per_display {
            p.on_display(a);
        }
        p
    }

    #[test]
    fn the_reserve_covers_the_longest_silence_at_the_mean_rate() {
        // 19 表示ごとに 64（表示 0・19・…・133）。表示 151 の時点の窓（表示 88〜151）には 95・114・133 の 3 つで平均 192/64 = 3 / 表示、
        // 何も届かない最長の続きは 18 表示（96〜113・115〜132・134〜151）。取っておく数 = 余裕 3 ＋ 3 × 18 = 57。
        let mut p = Pace::new();
        for d in 0..(19 * 8) {
            p.on_display(if d % 19 == 0 { 64 } else { 0 });
        }
        assert_eq!(p.rate(), 3 * MILLI);
        assert_eq!(p.reserve(3), 57);
        // 普段の観戦（1 表示に 1 つ）: 何も届かない表示が無いので余裕のまま。
        assert_eq!(fed(&[1; PACE_WINDOW]).reserve(3), 3);
        // まとまりが来なくなって 1 表示に 1 つずつになったら、まとまりの間の空きが窓を出るまで残す（16 表示ごとに 64 → 表示 0〜63 の
        // 後に 1 ずつ 40 表示: 窓は 40〜63 と 1 × 40、空きの最長 15、平均 (64 + 40) / 64）。
        let mut p = Pace::new();
        for d in 0..64 {
            p.on_display(if d % 16 == 0 { 64 } else { 0 });
        }
        for _ in 0..40 {
            p.on_display(1);
        }
        assert_eq!(p.reserve(3), 3 + (64 + 40) * 15 / 64);
        for _ in 0..24 {
            p.on_display(1);
        }
        assert_eq!(p.reserve(3), 3, "空きが窓を出たら余裕だけ");
        // 40 表示止まった後に 30 がまとめて届いた（1 回きり）: 窓は 1 × 23・0 × 40・30 で、平均 53/64 × 空き 40 = 33 だが、
        // 4 表示のまとまりの最大 30 までしか取っておかない。
        let mut p = fed(&[1; PACE_WINDOW]);
        for _ in 0..40 {
            p.on_display(0);
        }
        p.on_display(30);
        assert_eq!(p.reserve(3), 3 + 30);
        // 最初に届く前の表示は数えない（入った直後の平均を 0 で薄めない）。
        let mut p = Pace::new();
        for _ in 0..30 {
            p.on_display(0);
        }
        p.on_display(64);
        assert_eq!(p.rate(), 64 * MILLI);
    }

    #[test]
    fn a_small_backlog_does_not_enter_and_the_exit_threshold_holds() {
        let mut p = Pace::new();
        assert_eq!(p.extras(FF_ENTER_FRAMES, 3), 0);
        assert!(!p.is_active());
        // 入ったら、戻る閾値より下へは減らさない。
        let mut p = fed(&[1; PACE_WINDOW]);
        let n = p.extras(FF_ENTER_FRAMES + 1, 3);
        assert!(p.is_active());
        assert!(n <= FF_ENTER_FRAMES + 1 - FF_EXIT_FRAMES);
        assert_eq!(p.extras(FF_EXIT_FRAMES, 3), 0);
        assert!(!p.is_active());
    }

    #[test]
    fn bursts_of_a_long_round_trip_are_played_at_their_mean_speed_without_running_dry() {
        // 往復 300 ms の追いつき: 64 記録のまとまりが 19 表示ごとに届く（平均 3.4 倍）。
        let mut p = Pace::new();
        let mut backlog = 0u32;
        let (mut stalls, mut extras_max, mut played) = (0, 0, 0u64);
        for d in 0..1_900u32 {
            let arrived = if d % 19 == 0 { 64 } else { 0 };
            backlog += arrived;
            p.on_display(arrived);
            if !p.may_play(backlog, 3) {
                continue;
            }
            if backlog == 0 {
                stalls += 1;
                p.on_stall();
                continue;
            }
            backlog -= 1;
            let n = p.extras(backlog, 3);
            backlog -= n;
            // 入った直後の 400 表示（平均がまだ定まらない）の後を見る。
            if d >= 400 {
                played += 1 + u64::from(n);
                extras_max = extras_max.max(n);
                assert!(backlog > 0, "表示 {d} で溜まりが尽きた");
            }
        }
        assert!(stalls <= 2, "止まった回数 {stalls}");
        assert!(extras_max <= 8, "1 表示の余分の最大 {extras_max}");
        let mean = played as f64 / 1_500.0;
        assert!((3.0..3.8).contains(&mean), "平均の速さ {mean:.2}");
    }

    #[test]
    fn a_fast_line_runs_at_the_cap() {
        let mut p = fed(&[64; PACE_WINDOW]);
        assert_eq!(p.extras(1_000, 3), FF_MAX_EXTRA);
        assert_eq!(p.extras(1_000, 3), FF_MAX_EXTRA, "上限の端数を貯めない");
    }

    #[test]
    fn after_a_stall_it_waits_for_the_reserve_or_the_hold_limit() {
        let mut p = fed(&[1; PACE_WINDOW]);
        assert!(p.may_play(0, 3), "止まる前は待たない");
        p.on_stall();
        assert!(!p.may_play(2, 3));
        assert!(p.may_play(3, 3), "余裕まで戻った");
        assert!(!p.refilling());
        p.on_stall();
        for _ in 0..REFILL_HOLD_DISPLAYS {
            assert!(!p.may_play(1, 3));
        }
        assert!(p.may_play(1, 3), "待ちの上限で使い始める");
    }

    #[test]
    fn after_a_stall_a_catching_up_spectator_refills_the_reserve_and_a_live_one_only_the_margin() {
        // まとまりで届く（19 表示ごとに 64。取っておく数 57 —— 上の検査）: 止まった後は 57 まで待つ。
        let mut p = Pace::new();
        for d in 0..(19 * 8) {
            p.on_display(if d % 19 == 0 { 64 } else { 0 });
        }
        p.on_stall();
        assert!(!p.may_play(56, 3));
        assert!(p.may_play(57, 3));
        // 1 表示に 1 つずつ届いていて、ホストが 40 表示止まった（平均は 1 を下回る）: 止まった長さぶん待ち足さず、余裕だけ。
        let mut p = fed(&[1; PACE_WINDOW]);
        for _ in 0..40 {
            p.on_display(0);
        }
        p.on_display(1);
        p.on_stall();
        assert!(p.reserve(3) > 3, "止まった長さは取っておく数には入る");
        assert!(!p.may_play(2, 3));
        assert!(p.may_play(3, 3));
    }
}
