//! 観戦者の早送り（遅れて入った観戦者を、小さな遅れまで追いつかせる）。判断は pure（unsafe 無し）。ゲームの走査を呼ぶ本物は
//! `script_win.rs`（`RealFfHost`）、検査は偽のゲーム。
//!
//! 観戦者はマッチの途中で入ると、その seg の始まりから同じ速さで再生するので、入るまでの時間の分だけずっと遅れる（実機で記録が届いてから
//! 使うまで 平均 16.2 秒・最大 17.9 秒）。使っていないマッチのフレームが閾値より溜まっている間、1 回の表示（do_frame）の中で
//! ゲームの更新（走査 `0x42C700`）を余分に回して進める。溜まりが戻る閾値まで減ったら普段の速さに戻す（入る閾値と戻る閾値を分ける）。
//! 1 表示に余分に回す数は core の `spectate_pace::Pace` が決める（届く平均の速さに合わせる。往復が長い回線で往復ごとにまとまって
//! 届く間も、最大の速さで溜まりを使い切って止まらない）。
//!
//! ## 更新の経路
//!
//! do_frame の走査の call（`0x0042DCA9`）の包み（`rollback_win::rb_scan_entry`）で、走査を続けて何度も呼ぶ。rollback の進め直しが
//! 「1 回の描画の中で更新だけを k 回回す」形（`rollback_resim::run_scan`。k = 1・8・60 で元の実行とバイト一致）で確かめてある経路を、
//! 前へ進めるだけに使う。違いは 3 つ:
//! - コピーも書き戻しも要らない（観戦者は巻き戻さない。記録の語は確定した語）。
//! - 余分な回の入力の地点（`0x0043112D`）は普段の tick と同じ経路（`WatchPlay::on_input`）で次の記録を使う。`RESIM_TICK` だけを立てて、
//!   `_ticks.csv`・ペースの時刻のフックに余分な回を積まない（積むと「入力 → Present」の計測が崩れる）。
//! - 出力は文字の数だけ整える: 余分な回の前に、文字の数を最初の走査の前の値へ戻す（戻さないと古い数字が重なる）。SE と BGM の
//!   待ち行列は触らない（ゲームが積んだとおりに鳴らす。BGM は、飛ばした tick の命令が本物なので消すと曲が替わらない）。
//!
//! 余分な回はマッチのフレーム（`Frame`）の間だけ（選択の画面の押下・止める地点・読み込みは普段のまま）。次の記録が `Frame` でないとき・
//! 直前の回がマッチのフレームを使わなかったとき・走査が打ち切りの値（0・1・負）を返したとき・場面の要求が立ったときはそこで止める。
//! 1 表示あたりの余分な回の数と時間に上限を置く（ゲームの画面が止まって見えないように）。
//!
//! 通るのは観戦者のセッションがある実行（`Slot::Watch`）だけ。対戦者の経路（`rb_scan_handler` の `Slot::Play`）と delay 方式の実行は通らず、
//! 対戦者の遅延の増分は 0。
#![forbid(unsafe_code)]

use higanr_core::wire::Record;

use super::WatchPlay;
use crate::netplay::Phase;
use crate::rollback_resim::abort_of;

/// 入る閾値・戻る閾値・1 表示の余分の上限（観戦者の模型と同じ値を使うため core に置く）。
pub use higanr_core::spectate_pace::{FF_ENTER_FRAMES, FF_EXIT_FRAMES, FF_MAX_EXTRA};
/// 1 回の表示の中で余分な更新に使う時間の上限（µs。8 ms）。越えたらその表示では打ち切る（60 Hz の 1 フレーム 16.7 ms の半分を
/// 越えて描画とメッセージ処理を遅らせない）。遅い PC では上の回数に届く前にここで止まり、PC が回せる速さまで自然に落ちる。
pub const FF_BUDGET_US: u64 = 8_000;

/// 早送りの数（本ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FfStats {
    /// 走査の包みを通った表示の数（0 のまま観戦者の tick が進むなら、早送りの経路が無い）。
    pub begun: u64,
    pub displays: u64,
    pub extra_ticks: u64,
    /// 余分な更新に使った時間の 1 表示ぶんの最大（µs。最初の走査の後から表示の終わりまで）。
    pub batch_max_us: u32,
    /// 早送りに入った回数と、戻った回数。
    pub episodes: u32,
    pub finished: u32,
    pub first_enter_backlog: Option<u32>,
    /// 追いつくまでの時間（ms）: 最初の 1 回と、最長と、合計。入ってから戻るまで（実時間）。
    pub first_ms: Option<u32>,
    pub max_ms: u32,
    pub total_ms: u64,
    /// 時間の上限で余分な更新を打ち切った回数。
    pub cutoffs: u32,
    /// 余分な更新の前に SE・文字の数を整えられなかった回数（その表示では早送りをやめる）。
    pub drop_fail: u32,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct FfState {
    active: bool,
    since_us: u64,
    /// 前に見たときの `stats.played`（直前の走査がマッチのフレームを使ったかを見る）。
    seen_played: u64,
    extra: u32,
    /// この表示で余分に回してよい数（表示の最初の問いで `Pace::extras` が決める）。
    allow: u32,
    /// この表示の余分な更新の時間の起点（µs）。
    t0_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    Idle,
    /// 早送り中で、溜まりが戻る閾値以下になった（追いついた）。
    Leave,
    Max,
    /// この表示の余分な更新の時間の上限に達した。
    Cutoff,
    Go,
}

/// `active`: いま早送り中か / `backlog`: 使っていない記録の数 / `extra`: この表示で認めた余分な更新の数 / `allow`: この表示で余分に
/// 回してよい数（`Pace::extras`。入る閾値・戻る閾値もそこで見る）/ `elapsed_us`: この表示の余分な更新に使った時間。
pub(super) fn decide(
    active: bool,
    backlog: u32,
    extra: u32,
    allow: u32,
    elapsed_us: u64,
) -> Verdict {
    if active && backlog <= FF_EXIT_FRAMES {
        return Verdict::Leave;
    }
    if extra >= allow.min(FF_MAX_EXTRA) {
        return if active || extra > 0 {
            Verdict::Max
        } else {
            Verdict::Idle
        };
    }
    if elapsed_us >= FF_BUDGET_US {
        return Verdict::Cutoff;
    }
    Verdict::Go
}

impl WatchPlay {
    /// 表示の始まり。前の表示の間に使えるようになった記録の数（組み立ての `ack` の進み）を `Pace` へ入れる。
    pub fn ff_begin(&mut self, _now_us: u64) {
        self.ff.seen_played = self.stats.played;
        self.ff.extra = 0;
        self.ff.allow = 0;
        self.stats.ff.begun = self.stats.ff.begun.saturating_add(1);
        self.pace.on_display(std::mem::take(&mut self.pace_arrived));
    }

    /// 走査を 1 回呼んだ後に聞く: もう 1 回、余分に更新を回してよいか。真を返したら呼び手は必ず回す（数えている）。
    pub fn ff_more(&mut self, now_us: u64) -> bool {
        let advanced = self.stats.played != self.ff.seen_played;
        self.ff.seen_played = self.stats.played;
        if self.phase != Phase::Lockstep || !self.started {
            self.ff_leave(now_us);
            return false;
        }
        if !advanced {
            // 直前の走査がマッチのフレームを使わなかった（語を待った・場面の切替）。
            return false;
        }
        self.refill();
        if !matches!(self.head(), Some(Record::Frame { .. })) {
            // 次がマッチのフレームでない（溜まりが無い・`Seg` / `End` の境目）。
            self.ff_leave(now_us);
            return false;
        }
        let backlog = self.available();
        if self.ff.extra == 0 {
            self.ff.t0_us = now_us;
            self.ff.allow = self.pace.extras(backlog, self.cfg.margin_frames);
            if self.ff.active && !self.pace.is_active() {
                // 溜まりが戻る閾値まで減った（`Pace` が早送りをやめた）。
                self.ff_leave(now_us);
                return false;
            }
        }
        let elapsed = now_us.saturating_sub(self.ff.t0_us);
        match decide(
            self.ff.active,
            backlog,
            self.ff.extra,
            self.ff.allow,
            elapsed,
        ) {
            Verdict::Idle | Verdict::Max => false,
            Verdict::Leave => {
                self.ff_leave(now_us);
                false
            }
            Verdict::Cutoff => {
                self.stats.ff.cutoffs = self.stats.ff.cutoffs.saturating_add(1);
                false
            }
            Verdict::Go => {
                if !self.ff.active {
                    self.ff.active = true;
                    self.ff.since_us = now_us;
                    self.stats.ff.episodes = self.stats.ff.episodes.saturating_add(1);
                    self.stats.ff.first_enter_backlog.get_or_insert(backlog);
                }
                self.ff.extra += 1;
                true
            }
        }
    }

    /// `ff_more` が真を返したのに、余分な更新を回せなかった（SE・文字の数を整えられなかった）。
    pub fn ff_drop_failed(&mut self) {
        self.ff.extra = self.ff.extra.saturating_sub(1);
        self.stats.ff.drop_fail = self.stats.ff.drop_fail.saturating_add(1);
    }

    pub fn ff_end(&mut self, now_us: u64) {
        if self.ff.extra > 0 {
            let s = &mut self.stats.ff;
            s.displays = s.displays.saturating_add(1);
            s.extra_ticks = s.extra_ticks.saturating_add(u64::from(self.ff.extra));
            let used = now_us
                .saturating_sub(self.ff.t0_us)
                .min(u64::from(u32::MAX)) as u32;
            s.batch_max_us = s.batch_max_us.max(used);
        }
        self.ff.extra = 0;
        self.stats.ff.active = self.ff.active;
    }

    fn ff_leave(&mut self, now_us: u64) {
        self.pace.leave();
        if !self.ff.active {
            return;
        }
        self.ff.active = false;
        let ms = (now_us.saturating_sub(self.ff.since_us) / 1_000).min(u64::from(u32::MAX)) as u32;
        let s = &mut self.stats.ff;
        s.finished = s.finished.saturating_add(1);
        s.first_ms.get_or_insert(ms);
        s.max_ms = s.max_ms.max(ms);
        s.total_ms = s.total_ms.saturating_add(u64::from(ms));
    }
}

/// 走査を呼ぶ側（本物は `script_win::RealFfHost`、検査は偽のゲーム）。観戦者のセッション `WatchPlay` は走査の中（入力の地点）で別の借用に
/// なるので、セッションへの触り方は `with_wp` で毎回借りて返す。
pub trait FfHost {
    /// 走査（更新 1 tick）を 1 回呼んで、do_frame へ返す値を返す。`extra` は余分な回（`_ticks.csv`・時刻のフックに積まない）。
    fn scan(&mut self, extra: bool) -> i32;
    /// 観戦者のセッションを借りて `f(セッション, いまの時刻 µs)` を呼ぶ。セッションが無ければ `None`（早送りをやめる）。
    fn with_wp<R>(&mut self, f: impl FnOnce(&mut WatchPlay, u64) -> R) -> Option<R>;
    fn begin(&mut self);
    /// 余分な更新の前: 文字の数をバックアップの値へ戻す（SE は触らない —— 本物は `rollback_win::ff_drop_outputs`）。できたら真。
    fn drop_outputs(&mut self) -> bool;
    /// 次の場面の要求が立っているか（立っていれば、そこで余分な更新をやめる）。
    fn boundary(&mut self) -> bool;
}

/// 1 回の表示の走査（do_frame の走査の call の代わり）。普段は走査 1 回で戻り値をそのまま返す。使っていないマッチのフレームが
/// 溜まっている間は、余分に走査を回す。戻り値は最後の走査の値。
pub fn run_display<H: FfHost>(h: &mut H) -> i32 {
    if h.with_wp(|wp, now| wp.ff_begin(now)).is_none() {
        return h.scan(false);
    }
    h.begin();
    let mut ret = h.scan(false);
    loop {
        if abort_of(ret).is_some() || h.boundary() {
            break;
        }
        if !h.with_wp(|wp, now| wp.ff_more(now)).unwrap_or(false) {
            break;
        }
        if !h.drop_outputs() {
            let _ = h.with_wp(|wp, _| wp.ff_drop_failed());
            break;
        }
        ret = h.scan(true);
    }
    let _ = h.with_wp(|wp, now| wp.ff_end(now));
    ret
}

#[cfg(test)]
mod tests {
    //! 早送りの判断（数の表）と、`run_display` の外側（セッションが無い・打ち切りの値・マッチの外・溜まりが無い）。溜まりが多いときの
    //! 組み上がりは `netplay/watch_tests.rs`。
    use std::sync::Arc;

    use super::*;
    use crate::spectate_watch::{WatchConfig, WatchSlot};

    fn wp() -> WatchPlay {
        WatchPlay::new(
            Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: 3,
                require_net_selection: false,
            },
        )
    }

    #[test]
    fn the_verdict_follows_the_allowance_the_exit_threshold_and_two_caps() {
        // 早送りでなく、この表示の許しも 0（入る閾値は `Pace` が見る）。
        assert_eq!(decide(false, 50, 0, 0, 0), Verdict::Idle);
        assert_eq!(decide(false, 50, 0, 3, 0), Verdict::Go);
        // 許した数まで回したら、その表示はおしまい。早送り中で許しが 0（端数を貯めている表示）も同じ。
        assert_eq!(decide(true, 50, 2, 3, 0), Verdict::Go);
        assert_eq!(decide(true, 50, 3, 3, 0), Verdict::Max);
        assert_eq!(decide(true, 50, 0, 0, 0), Verdict::Max);
        // 上限の数（許しが上限を越えても）。
        assert_eq!(decide(true, 50, FF_MAX_EXTRA - 1, 40, 0), Verdict::Go);
        assert_eq!(decide(true, 50, FF_MAX_EXTRA, 40, 0), Verdict::Max);
        // 戻る閾値が先。
        for b in 0..=FF_EXIT_FRAMES {
            assert_eq!(decide(true, b, 0, 5, 0), Verdict::Leave, "溜まり {b}");
        }
        assert_eq!(
            decide(true, FF_EXIT_FRAMES, FF_MAX_EXTRA, 40, 0),
            Verdict::Leave
        );
        assert_eq!(decide(true, FF_EXIT_FRAMES + 1, 0, 5, 0), Verdict::Go);
        // 時間の上限。
        assert_eq!(decide(true, 50, 1, 5, FF_BUDGET_US - 1), Verdict::Go);
        assert_eq!(decide(true, 50, 1, 5, FF_BUDGET_US), Verdict::Cutoff);
        const { assert!(FF_EXIT_FRAMES < FF_ENTER_FRAMES) };
    }

    struct Stub {
        wp: WatchPlay,
        has_box: bool,
        scans: Vec<bool>,
        ret: i32,
        boundary: bool,
        /// 走査のたびに「マッチのフレームを 1 つ使った」ことにする。
        bump: bool,
        begun: u32,
        drops: u32,
        now: u64,
        /// 走査 1 回ごとに進む時計（µs）。
        cost_us: u64,
        drop_ok: bool,
    }

    impl Stub {
        fn new() -> Stub {
            Stub {
                wp: wp(),
                has_box: true,
                scans: Vec::new(),
                ret: 2,
                boundary: false,
                bump: false,
                begun: 0,
                drops: 0,
                now: 0,
                cost_us: 0,
                drop_ok: true,
            }
        }

        fn in_match(backlog: u32) -> Stub {
            let mut s = Stub::new();
            s.wp.set_phase_for_test(Phase::Lockstep);
            s.wp.started = true;
            s.wp.look[0] = Some((
                0,
                Record::Frame {
                    frame: 0,
                    p1: 0,
                    p2: 0,
                },
            ));
            s.wp.test_backlog = Some(backlog);
            s.bump = true;
            s
        }
    }

    impl FfHost for Stub {
        fn scan(&mut self, extra: bool) -> i32 {
            self.scans.push(extra);
            if self.bump {
                self.wp.stats.played += 1;
            }
            self.now += self.cost_us;
            self.ret
        }
        fn with_wp<R>(&mut self, f: impl FnOnce(&mut WatchPlay, u64) -> R) -> Option<R> {
            self.has_box.then(|| f(&mut self.wp, self.now))
        }
        fn begin(&mut self) {
            self.begun += 1;
        }
        fn drop_outputs(&mut self) -> bool {
            self.drops += 1;
            self.drop_ok
        }
        fn boundary(&mut self) -> bool {
            self.boundary
        }
    }

    #[test]
    fn a_display_without_a_backlog_runs_exactly_one_scan() {
        let mut s = Stub::new();
        s.has_box = false;
        assert_eq!(run_display(&mut s), 2);
        assert_eq!(s.scans, vec![false]);
        assert_eq!(s.begun, 0, "セッションが無いのに手続きを始めた");
        for phase in [
            Phase::Unconnected,
            Phase::Selecting,
            Phase::Holding,
            Phase::Loading,
            Phase::Ended,
        ] {
            let mut s = Stub::new();
            s.wp.set_phase_for_test(phase);
            assert_eq!(run_display(&mut s), 2);
            assert_eq!(s.scans, vec![false], "{phase:?}");
            assert_eq!((s.drops, s.begun), (0, 1));
            assert_eq!(s.wp.stats().ff.begun, 1, "{phase:?}");
            assert_eq!(s.wp.stats().ff.displays, 0, "{phase:?}");
        }
        let mut s = Stub::new();
        s.wp.set_phase_for_test(Phase::Lockstep);
        s.wp.started = true;
        s.wp.stats.played = 5;
        s.bump = true;
        assert_eq!(run_display(&mut s), 2);
        assert_eq!(s.scans, vec![false], "溜まりが無いのに余分に回した");
        assert_eq!(s.wp.stats().ff.displays, 0);
        // 打ち切りの値（0・1・負）・場面の要求が立っているときは、`ff_more` にも聞かずに止める。1 は描画だけのフレーム・場面の切替で、
        // 一時停止の間も 1。
        for ret in [0, 1, -1] {
            let mut s = Stub::new();
            s.ret = ret;
            assert_eq!(run_display(&mut s), ret);
            assert_eq!(s.scans, vec![false]);
        }
        let mut s = Stub::new();
        s.boundary = true;
        assert_eq!(run_display(&mut s), 2);
        assert_eq!(s.scans, vec![false]);
        assert_eq!(s.wp.stats().ff.begun, 1);
    }

    #[test]
    fn a_large_backlog_runs_extra_scans_up_to_the_caps_and_stops_at_every_guard() {
        // 溜まりが多い: 1 表示 = 最初の走査 ＋ 余分 FF_MAX_EXTRA 回。余分の前に必ず SE・文字の数を整える。
        let mut s = Stub::in_match(50);
        assert_eq!(run_display(&mut s), 2);
        let mut want = vec![false];
        want.extend(std::iter::repeat_n(true, FF_MAX_EXTRA as usize));
        assert_eq!(s.scans, want);
        assert_eq!(s.drops, FF_MAX_EXTRA);
        let ff = s.wp.stats().ff;
        assert_eq!(
            (ff.begun, ff.displays, ff.extra_ticks, ff.episodes),
            (1, 1, u64::from(FF_MAX_EXTRA), 1)
        );
        assert_eq!((ff.first_enter_backlog, ff.active), (Some(50), true));
        for (name, mutate) in [
            (
                "直前の走査が試合のフレームを使わない",
                (|s: &mut Stub| s.bump = false) as fn(&mut Stub),
            ),
            ("先頭が Seg", |s: &mut Stub| {
                s.wp.look[0] = Some((0, Record::Seg { seg: 1 }));
            }),
            ("先頭が無い", |s: &mut Stub| s.wp.look[0] = None),
            (
                "走査が 1 を返す（描画だけのフレーム・場面の切替。一時停止の間も 1）",
                |s| s.ret = 1,
            ),
            ("走査が 0 を返す", |s| s.ret = 0),
            ("場面の要求が立っている", |s| s.boundary = true),
            ("試合の 1 フレーム目の前", |s| {
                s.wp.started = false
            }),
            ("試合の外", |s| {
                s.wp.set_phase_for_test(Phase::Selecting)
            }),
        ] {
            let mut s = Stub::in_match(50);
            mutate(&mut s);
            run_display(&mut s);
            assert_eq!(s.scans, vec![false], "{name}");
            assert_eq!(s.wp.stats().ff.extra_ticks, 0, "{name}");
        }
        // SE・文字の数を整えられなかったら、その余分は回さず、数える（その表示の早送りはやめる）。
        let mut s = Stub::in_match(50);
        s.drop_ok = false;
        run_display(&mut s);
        let ff = s.wp.stats().ff;
        assert_eq!(s.scans, vec![false]);
        assert_eq!((ff.drop_fail, ff.extra_ticks, ff.displays), (1, 0, 0));
        // 時間の上限: 走査 1 回が 5 ms なら、余分 2 回（10 ms）で上限の 8 ms を越えて打ち切る。
        let mut s = Stub::in_match(50);
        s.cost_us = 5_000;
        run_display(&mut s);
        let ff = s.wp.stats().ff;
        assert_eq!(s.scans, vec![false, true, true], "{ff:?}");
        assert_eq!((ff.cutoffs, ff.extra_ticks), (1, 2));
        assert_eq!(ff.batch_max_us, 10_000);
    }

    #[test]
    fn the_enter_and_exit_thresholds_work_across_displays() {
        let mut s = Stub::in_match(FF_ENTER_FRAMES + 1);
        s.now = 1_000_000;
        run_display(&mut s);
        assert_eq!(s.wp.stats().ff.episodes, 1);
        assert!(s.wp.stats().ff.active);
        s.now = 1_100_000;
        s.scans.clear();
        s.wp.test_backlog = Some(FF_ENTER_FRAMES - 1);
        run_display(&mut s);
        assert!(s.scans.len() > 1, "早送り中なのに入る閾値以下で止めた");
        assert_eq!(s.wp.stats().ff.episodes, 1, "入り直した");
        s.now = 1_250_000;
        s.scans.clear();
        s.wp.test_backlog = Some(FF_EXIT_FRAMES);
        run_display(&mut s);
        let ff = s.wp.stats().ff;
        assert_eq!(s.scans, vec![false]);
        assert!(!ff.active);
        assert_eq!((ff.finished, ff.first_ms, ff.max_ms), (1, Some(250), 250));
        assert_eq!(ff.total_ms, 250);
        // 戻った後: 入る閾値以下では入り直さない。越えたら入り直す（2 回目は最初の 1 回の値を変えない）。
        s.now = 2_000_000;
        s.scans.clear();
        s.wp.test_backlog = Some(FF_ENTER_FRAMES);
        run_display(&mut s);
        assert_eq!(s.scans, vec![false]);
        assert_eq!(s.wp.stats().ff.episodes, 1);
        s.now = 2_100_000;
        s.wp.test_backlog = Some(FF_ENTER_FRAMES + 1);
        run_display(&mut s);
        assert_eq!(s.wp.stats().ff.episodes, 2);
        // マッチの外へ出たら戻る（2 回目 400 ms）。最初の 1 回は 250 ms のまま。
        s.now = 2_500_000;
        s.wp.set_phase_for_test(Phase::Selecting);
        run_display(&mut s);
        let ff = s.wp.stats().ff;
        assert!(!ff.active);
        assert_eq!(
            (ff.finished, ff.first_ms, ff.max_ms, ff.total_ms),
            (2, Some(250), 400, 650)
        );
    }
}
