//! 遅れて入った観戦者の追いつきの模型: ホスト（`SpectateHost`）と、dll の観戦者の形（60 Hz の表示ごとに届いた便を受けて ACK を返し、
//! 1 記録を使い、溜まりに応じて早送りする）を、往復・落としのあるネットワークで繋ぐ。重複の割合・追いつくまでの時間・ホストの送り・
//! 観戦者の止まりを数える。
//!
//! ホストの `poll` は dll の受信スレッドと同じく、便が届いた ms と 16 ms ごと（対戦の相手の INPUT の間隔）にだけ回す。

use super::*;
use higanr_core::spectate_pace::{FF_ENTER_FRAMES, FF_EXIT_FRAMES, FF_MAX_EXTRA, Pace};

/// 観戦者の余裕（dll の `WATCH_MARGIN_FRAMES`）と、マッチの 1 フレーム目の前に余裕を待つ上限（`START_HOLD_TICKS`）。
const MARGIN: u32 = 3;
const START_HOLD: u32 = 30;
/// 追いついたと見る遅れ: 観戦者が使う記録が、積まれてから 片道 ＋ これ 以内。
const CAUGHT_UP_SLACK_MS: u32 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Policy {
    /// 前の dll の早送り（溜まりが 6 を越えたら 1 表示に余分 16 回まで・3 以下で戻る。止まった後は待たない）。比べるための写し。
    Old,
    /// `spectate_pace::Pace`（届く速さに合わせる・止まった後に余裕を作り直す）。
    Pace,
}

pub(super) struct Display {
    policy: Policy,
    one_way: u32,
    t0: Option<u32>,
    n: u64,
    pace: Pace,
    old_active: bool,
    last_ack: Option<u32>,
    started: bool,
    start_hold: u32,
    frozen: bool,
    /// 使っていた表示から、止まった・待った表示へ変わった回数（見て分かる止まり）。
    pub freezes: u32,
    pub stall_displays: u64,
    pub refill_displays: u64,
    pub caught_up_at: Option<u32>,
    /// 追いつくまでの表示ごとの、使った記録の数（1 = 普段の速さ）の度数。
    pub speed_hist: [u64; FF_MAX_EXTRA as usize + 2],
}

impl Display {
    pub(super) fn new(policy: Policy, one_way: u32) -> Display {
        Display {
            policy,
            one_way,
            t0: None,
            n: 0,
            pace: Pace::new(),
            old_active: false,
            last_ack: None,
            started: false,
            start_hold: 0,
            frozen: false,
            freezes: 0,
            stall_displays: 0,
            refill_displays: 0,
            caught_up_at: None,
            speed_hist: [0; FF_MAX_EXTRA as usize + 2],
        }
    }

    fn freeze(&mut self) {
        if !self.frozen {
            self.freezes += 1;
        }
        self.frozen = true;
    }

    fn take(&mut self, w: &mut Watcher, got: &mut Vec<(u32, Record)>, k: u32) -> u32 {
        let mut used = 0;
        for _ in 0..k {
            match w.pop_indexed() {
                Some(r) => {
                    got.push(r);
                    used += 1;
                }
                None => break,
            }
        }
        used
    }

    /// 旧の早送り: 1 表示の余分は、溜まりが戻る閾値に着くか上限まで。
    fn old_extras(&mut self, backlog: u32) -> u32 {
        if !self.old_active && backlog <= FF_ENTER_FRAMES {
            return 0;
        }
        if backlog <= FF_EXIT_FRAMES {
            self.old_active = false;
            return 0;
        }
        let n = FF_MAX_EXTRA.min(backlog - FF_EXIT_FRAMES);
        self.old_active = backlog - n > FF_EXIT_FRAMES;
        n
    }

    pub(super) fn step(
        &mut self,
        now: u32,
        w: &mut Watcher,
        inbox: &mut Vec<(SocketAddr, Vec<u8>)>,
        got: &mut Vec<(u32, Record)>,
        push_at: &[u32],
        tx: &mut Tx<'_>,
    ) {
        let t0 = *self.t0.get_or_insert(now);
        if now < t0 + (self.n * 50 / 3) as u32 {
            return;
        }
        self.n += 1;
        for (from, bytes) in std::mem::take(inbox) {
            w.on_datagram(from, &bytes, now, tx);
        }
        w.poll(now, tx);
        let ack = w.ack();
        let arrived = self.last_ack.map_or(0, |a| ack.saturating_sub(a));
        self.last_ack = Some(ack);
        self.pace.on_display(arrived);
        let avail = w.pending() as u32;
        if !self.started {
            if avail == 0 {
                return;
            }
            if avail < MARGIN && self.start_hold < START_HOLD {
                self.start_hold += 1;
                return;
            }
            self.started = true;
        }
        let mut used = 0;
        let play = match self.policy {
            Policy::Pace => self.pace.may_play(avail, MARGIN),
            Policy::Old => true,
        };
        if !play {
            self.refill_displays += 1;
            self.freeze();
        } else if avail == 0 {
            self.stall_displays += 1;
            self.freeze();
            self.old_active = false;
            if self.policy == Policy::Pace {
                self.pace.on_stall();
            }
        } else {
            used = self.take(w, got, 1);
            self.frozen = false;
            let backlog = w.pending() as u32;
            let extra = match self.policy {
                Policy::Pace => self.pace.extras(backlog, MARGIN),
                Policy::Old => self.old_extras(backlog),
            };
            used += self.take(w, got, extra);
        }
        if self.caught_up_at.is_none() {
            if let Some(h) = self.speed_hist.get_mut(used as usize) {
                *h += 1;
            }
            if let Some(&(idx, _)) = got.last()
                && now.saturating_sub(push_at[idx as usize]) <= self.one_way + CAUGHT_UP_SLACK_MS
            {
                self.caught_up_at = Some(now);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Case {
    pub rtt: u32,
    pub loss: u64,
    /// 観戦者が入る時刻（試合の頭から、秒）。
    pub late_s: u32,
    pub watchers: u16,
    pub policy: Policy,
    pub seed: u64,
    /// 追いつかないときに打ち切る、入ってからの秒。
    pub limit_s: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Outcome {
    /// 観戦者が受けた記録のうち重複（観戦者の数の合計）。
    pub dup: u64,
    pub received: u64,
    /// 追いつくまでの ms（入ってから。観戦者の最も遅い人。追いつかなければ None）。
    pub catchup_ms: Option<u32>,
    pub host_packets: u64,
    pub host_resent_records: u64,
    /// ホストが送ったバイト数の、入ってから後の 1 秒ごとの最大。
    pub peak_bps: u64,
    /// ホストの `poll` 1 回で送った通数の最大（受信スレッドの 1 周の仕事の目安）。
    pub max_poll_pkts: u64,
    pub freezes: u32,
    pub stall_displays: u64,
    pub refill_displays: u64,
    /// 追いつくまでの表示の、使った記録の数（添字）ごとの度数。
    pub hist: [u64; FF_MAX_EXTRA as usize + 2],
}

pub(super) fn run(c: Case) -> Outcome {
    let one_way = c.rtt / 2;
    let mut rng = Rng(c.seed ^ 0xCA7C);
    let frames = u64::from(c.late_s + c.limit_s + 5) * 63;
    let script = make_script(&mut rng, 1, frames);
    let mut sim = Sim::new(
        Net::new(c.seed, one_way.max(1), 4, c.loss, 0),
        SpectateHost::with_capacity(host_cfg(), 65_536),
        script,
    );
    sim.host_poll_every = 16;
    let start = c.late_s * 1_000;
    for n in 1..=c.watchers {
        sim.add_watcher(n, start, 0, ASM_CAP);
        let w = sim.watchers.last_mut().expect("足した");
        w.display = Some(Box::new(Display::new(c.policy, one_way)));
    }
    let end = start + c.limit_s * 1_000;
    let mut done_at: Option<u32> = None;
    while sim.now < end {
        sim.tick();
        if done_at.is_none()
            && sim
                .watchers
                .iter()
                .all(|w| w.display.as_ref().is_some_and(|d| d.caught_up_at.is_some()))
        {
            done_at = Some(sim.now);
        }
        // 追いついた後も 3 秒は回す（追いついた直後の止まりも数える）。
        if done_at.is_some_and(|t| sim.now >= t + 3_000) {
            break;
        }
    }
    let mut o = Outcome::default();
    let mut hist = [0u64; FF_MAX_EXTRA as usize + 2];
    o.catchup_ms = Some(0);
    for w in &sim.watchers {
        // 使った列が、ホストが積んだ列の頭からの続きと一致する（追いつきの途中で打ち切っても、使った所まで）。
        assert!(!w.got.is_empty(), "{c:?}: 1 つも使っていない");
        for (k, (idx, rec)) in w.got.iter().enumerate() {
            assert_eq!(*idx as usize, k, "{c:?}: 番号が連続でない");
            assert_eq!(*rec, sim.log[k], "{c:?}: 番号 {k} の記録が違う");
        }
        let a = w.w.assembler_counts();
        o.dup += a.dup;
        o.received += a.dup + a.stored;
        let d = w.display.as_ref().expect("表示の観戦者");
        o.freezes += d.freezes;
        o.stall_displays += d.stall_displays;
        o.refill_displays += d.refill_displays;
        for (h, x) in hist.iter_mut().zip(d.speed_hist) {
            *h += x;
        }
        o.catchup_ms = o
            .catchup_ms
            .zip(d.caught_up_at)
            .map(|(m, t)| m.max(t - start));
    }
    let c_host = sim.host.counts();
    o.host_packets = c_host.packets_sent;
    o.host_resent_records = c_host.records_resent;
    o.max_poll_pkts = sim.max_poll_pkts;
    o.peak_bps = sim
        .net
        .host_sec
        .iter()
        .skip(c.late_s as usize)
        .copied()
        .max()
        .unwrap_or(0);
    o.hist = hist;
    o
}

/// 種ごとの結果を 1 行にまとめる（数は合計、追いつくまでは平均と最長、B/s は最大）。
struct Row {
    runs: u32,
    dup: u64,
    received: u64,
    catchup: Vec<Option<u32>>,
    packets: u64,
    resent: u64,
    peak_bps: u64,
    max_poll_pkts: u64,
    freezes: u32,
    stall_displays: u64,
    refill_displays: u64,
    hist: [u64; FF_MAX_EXTRA as usize + 2],
}

pub(super) const MEASURE_SEEDS: [u64; 3] = [0x5EED_C47C, 0x0BAD_F00D, 0x1357_9BDF];

fn measure(c: Case) -> Row {
    let mut r = Row {
        runs: 0,
        dup: 0,
        received: 0,
        catchup: Vec::new(),
        packets: 0,
        resent: 0,
        peak_bps: 0,
        max_poll_pkts: 0,
        freezes: 0,
        stall_displays: 0,
        refill_displays: 0,
        hist: [0; FF_MAX_EXTRA as usize + 2],
    };
    for seed in MEASURE_SEEDS {
        let o = run(Case { seed, ..c });
        r.runs += 1;
        r.dup += o.dup;
        r.received += o.received;
        r.catchup.push(o.catchup_ms);
        r.packets += o.host_packets;
        r.resent += o.host_resent_records;
        r.peak_bps = r.peak_bps.max(o.peak_bps);
        r.max_poll_pkts = r.max_poll_pkts.max(o.max_poll_pkts);
        r.freezes += o.freezes;
        r.stall_displays += o.stall_displays;
        r.refill_displays += o.refill_displays;
        for (h, x) in r.hist.iter_mut().zip(o.hist) {
            *h += x;
        }
    }
    r
}

fn row(c: Case, r: &Row) -> String {
    let done: Vec<u32> = r.catchup.iter().flatten().copied().collect();
    let catchup = if done.len() == r.catchup.len() {
        let mean = done.iter().map(|&m| f64::from(m)).sum::<f64>() / done.len() as f64;
        let worst = done.iter().copied().max().unwrap_or(0);
        format!(
            "{:.1} s（最長 {:.1} s）",
            mean / 1000.0,
            f64::from(worst) / 1000.0
        )
    } else {
        format!(
            "{}/{} 回が {} s で追いつかない",
            r.catchup.len() - done.len(),
            r.catchup.len(),
            c.limit_s
        )
    };
    let displays: u64 = r.hist.iter().sum();
    let used: u64 = r.hist.iter().enumerate().map(|(k, n)| k as u64 * n).sum();
    let fast: u64 = r.hist.iter().skip(9).sum();
    format!(
        "| {} | {}% | {} × {} | {:.2}% ({}/{}) | {} | {} | {} | {} | {} | {} | {}+{} | {:.2} | {:.1}% ({}/{}) |",
        c.rtt,
        c.loss,
        c.watchers,
        r.runs,
        r.dup as f64 * 100.0 / r.received.max(1) as f64,
        r.dup,
        r.received,
        catchup,
        r.packets / u64::from(r.runs),
        r.resent / u64::from(r.runs),
        r.peak_bps,
        r.max_poll_pkts,
        r.freezes,
        r.stall_displays,
        r.refill_displays,
        used as f64 / displays.max(1) as f64,
        fast as f64 * 100.0 / displays.max(1) as f64,
        fast,
        displays,
    )
}

/// 往復 30 / 300 / 600 ms・落とす 0 / 2 %・試合の頭から 135 秒遅れて入る観戦者の、追いつきの表（標準エラー）。種 3 つの合計
/// （追いつくまでは平均と最長、通数と送り直した記録は 1 回あたり）。前後の比べは、この検査を前のホストのソース（`spectate.rs`）で
/// 回した表と並べる。
#[test]
#[ignore = "手で回す測り（release で数秒）。表を標準エラーに出す"]
fn measure_a_late_spectators_catchup_by_round_trip_and_loss() {
    eprintln!(
        "| 往復 ms | 落とす | 観戦者 × 種 | 重複の割合（重複/受けた記録） | 追いつくまで | ホストの通数（1 回あたり） | 送り直した記録（1 回あたり） | ホストの B/s の最大 | poll 1 回の通数の最大 | 止まり（回） | 止まった表示＋待った表示 | 追いつくまでの平均の速さ | 余分 8 回以上の表示 |"
    );
    for policy in [Policy::Old, Policy::Pace] {
        eprintln!("観戦者の早送り: {policy:?}");
        for (rtt, loss, watchers) in [
            (30, 0, 1),
            (30, 2, 1),
            (300, 0, 1),
            (300, 2, 1),
            (600, 0, 1),
            (600, 2, 1),
            (30, 0, 10),
            (300, 0, 10),
        ] {
            let c = Case {
                rtt,
                loss,
                late_s: 135,
                watchers,
                policy,
                seed: 0,
                limit_s: 400,
            };
            let r = measure(c);
            eprintln!("{}", row(c, &r));
        }
    }
}

/// 往復 300 ms で遅れて入った観戦者: 届く途中の記録を送り直さず（重複がほぼ 0）、往復ごとのまとまりで止まらない（`Pace`）。比べに旧の
/// 早送り（溜まりが閾値を越えたら最大の速さ）を同じ回線で回し、止まりが 1 桁以上少ないことを見る。
#[test]
fn a_late_spectator_on_a_300ms_line_gets_few_duplicates_and_does_not_freeze() {
    let case = |policy| Case {
        rtt: 300,
        loss: 0,
        late_s: 60,
        watchers: 1,
        policy,
        seed: 0,
        limit_s: 120,
    };
    let pace = measure(case(Policy::Pace));
    let old = measure(case(Policy::Old));
    eprintln!(
        "{}\n{}",
        row(case(Policy::Pace), &pace),
        row(case(Policy::Old), &old)
    );
    assert!(pace.catchup.iter().all(Option::is_some), "追いつかない");
    assert!(
        pace.dup * 100 <= pace.received * 3,
        "受けた {} 記録のうち重複 {}",
        pace.received,
        pace.dup
    );
    assert!(
        old.freezes >= 30,
        "旧の早送りが止まらない（この検査の狙いが外れた）: {}",
        old.freezes
    );
    assert!(
        pace.freezes * 10 <= old.freezes,
        "止まり {}（旧の早送り {}）",
        pace.freezes,
        old.freezes
    );
}

/// 往復 30 ms で遅れて入った観戦者は、早送りの最大の速さ（1 表示に `FF_MAX_EXTRA` ＋ 1 記録）で追いつく。遅れ L 秒を、ホストが積む
/// 速さ（1 表示に約 1 記録）を引いた 16 記録 / 表示で詰める時間に、2 割と 0.5 秒の余白を足した値を上限にする。
#[test]
fn a_late_spectator_on_a_30ms_line_catches_up_at_the_top_speed() {
    let late_s = 30;
    let c = Case {
        rtt: 30,
        loss: 0,
        late_s,
        watchers: 1,
        policy: Policy::Pace,
        seed: 0,
        limit_s: 60,
    };
    let r = measure(c);
    eprintln!("{}", row(c, &r));
    let lag_records = f64::from(late_s) * 1000.0 / 16.0;
    let ideal_ms = lag_records / f64::from(FF_MAX_EXTRA) * 1000.0 / 60.0;
    for m in &r.catchup {
        let m = f64::from(m.expect("追いつかない"));
        assert!(
            m <= ideal_ms * 1.2 + 500.0,
            "追いつくまで {m} ms（最大の速さで {ideal_ms:.0} ms）"
        );
    }
}
