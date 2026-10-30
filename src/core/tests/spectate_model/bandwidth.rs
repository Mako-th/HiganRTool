//! 上りの帯域が限られた1本の出口（列があり溢れたら落ちる）を対戦の便（ホスト → 相手のINPUT。1フレームに1通）と観戦の便が共有する模型。
//! ホストと相手はINPUTの時刻の返答（`rtt_probe::EchoClock`）で往復を測る。ホストはその要約を観戦者への送信（`SpectateHost::note_delay`）へ渡す。
//! 遅れて入った観戦者が追いつく間に対戦の便が出口の列で待った長さ（観戦の便のせいで増えた遅れ）を数える。
//!
//! 出口は送った順に並べて1通ずつ帯域の速さで出す（IP・UDPのヘッダ28 Bを足す）。
//! 列の長さは帯域の`buffer_ms`ぶん。越えた便は落とす。
//! 相手 → ホストの向きは絞らない（ホストの上りだけの模型）。

use super::catchup::{Display, Policy};
use super::*;
use higanr_core::rtt_probe::{DelayProbe, DelaySnapshot, EchoClock};
use higanr_core::spectate_pace::FF_UPDATES_DEFAULT;
use higanr_core::wire::Stamp;
use std::collections::VecDeque;

/// 対戦の相手のアドレス。
pub(super) const OPP: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    46_999,
);
/// IP（20 B）とUDP（8 B）のヘッダ。
const IP_UDP: usize = 28;
/// 対戦のINPUTの封じた長さ（入力ワード4個: ヘッダ12 ＋ nonceのフィールド8 ＋ 本文21 ＋ ワード8 ＋ タグ16）。
const INPUT_BYTES: usize = 65;

pub(super) struct Uplink {
    bps: u64,
    limit: usize,
    /// 列にいる便（出口を出終える時刻µs, 長さ）。
    inq: VecDeque<(u64, usize)>,
    backlog: usize,
    free_at_us: u64,
    pub dropped: u64,
    pub duel_dropped: u64,
    /// 対戦の便ごとの（列に入ったms, 列で待ったµs）。自分を出す時間は含まない。
    pub duel_waits: Vec<(u32, u32)>,
}

impl Uplink {
    pub(super) fn new(bps: u64, buffer_ms: u64) -> Uplink {
        Uplink {
            bps,
            limit: (bps / 8 * buffer_ms / 1_000) as usize,
            inq: VecDeque::new(),
            backlog: 0,
            free_at_us: 0,
            dropped: 0,
            duel_dropped: 0,
            duel_waits: Vec::new(),
        }
    }

    /// 時刻`now`に`len` Bの便を並べて出口を出終えるms（切り上げ）を返す。列が溢れたらNone。
    pub(super) fn enqueue(&mut self, now: u32, to: SocketAddr, len: usize) -> Option<u32> {
        let now_us = u64::from(now) * 1_000;
        while let Some(&(t, n)) = self.inq.front() {
            if t > now_us {
                break;
            }
            self.inq.pop_front();
            self.backlog -= n;
        }
        let size = len + IP_UDP;
        if self.backlog + size > self.limit {
            self.dropped += 1;
            if to == OPP {
                self.duel_dropped += 1;
            }
            return None;
        }
        let start = now_us.max(self.free_at_us);
        let done = start + size as u64 * 8 * 1_000_000 / self.bps;
        self.free_at_us = done;
        self.inq.push_back((done, size));
        self.backlog += size;
        if to == OPP {
            self.duel_waits.push((now, (start - now_us) as u32));
        }
        Some(done.div_ceil(1_000) as u32)
    }
}

fn encode(st: Stamp) -> Vec<u8> {
    let mut b = vec![0u8; INPUT_BYTES];
    b[0] = b'D';
    b[1..3].copy_from_slice(&st.sent.to_le_bytes());
    if let Some((e, h)) = st.echo {
        b[3..5].copy_from_slice(&e.to_le_bytes());
        b[5] = h;
        b[6] = 1;
    }
    b
}

fn decode(b: &[u8]) -> Stamp {
    Stamp {
        sent: u16::from_le_bytes([b[1], b[2]]),
        echo: (b[6] == 1).then(|| (u16::from_le_bytes([b[3], b[4]]), b[5])),
    }
}

/// ホストと相手のINPUTの送り合い（60 Hz。相手はフレームの位相をずらす）。
pub(super) struct Duel {
    host: EchoClock,
    opp: EchoClock,
    probe: DelayProbe,
    host_frame: u64,
    opp_frame: u64,
    opp_phase: u32,
    /// falseなら観戦者への送信へ往復を渡さない（往復の延びで下げない形の比べ）。
    feed: bool,
}

impl Duel {
    pub(super) fn new(feed: bool) -> Duel {
        Duel {
            host: EchoClock::new(),
            opp: EchoClock::new(),
            probe: DelayProbe::new(),
            host_frame: 0,
            opp_frame: 0,
            opp_phase: 7,
            feed,
        }
    }

    pub(super) fn owns(&self, p: &InFlight) -> bool {
        p.to == OPP || p.from == OPP
    }

    pub(super) fn on_packet(&mut self, p: &InFlight, now: u32) {
        let st = decode(&p.bytes);
        if p.to == OPP {
            self.opp.on_receive(&st, now);
        } else {
            let e = self.host.on_receive(&st, now);
            self.probe.sample_input(now, e);
        }
    }

    pub(super) fn send_due(&mut self, now: u32, net: &mut Net) {
        while u64::from(now) >= self.host_frame * 50 / 3 {
            net.send(now, HOST, OPP, &encode(self.host.stamp(now)));
            self.host_frame += 1;
        }
        while u64::from(now) >= self.opp_frame * 50 / 3 + u64::from(self.opp_phase) {
            net.send(now, OPP, HOST, &encode(self.opp.stamp(now)));
            self.opp_frame += 1;
        }
    }

    pub(super) fn snapshot(&self) -> Option<DelaySnapshot> {
        if self.feed {
            self.probe.snapshot()
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BwCase {
    /// 上りの帯域（bit/s）と列の長さ（その帯域のmsぶん）。
    pub bps: u64,
    pub buffer_ms: u64,
    pub rtt: u32,
    /// 試合の頭からいる（追いついている）観戦者と、`late_s`秒遅れて入る観戦者の数。
    pub live: u16,
    pub late: u16,
    pub late_s: u32,
    pub ff_max: u32,
    /// 有れば固定の全体の上限（B/s。観戦者ごとの上限と往復の延びによる下げなし）で比べる。
    pub fixed: Option<u32>,
    /// falseなら往復を観戦者への送信へ渡さない。
    pub feed: bool,
    pub seed: u64,
    pub limit_s: u32,
    /// ホストが測れた自分の上り（B/s。0は測れていない）。Noneなら出口の太さそのもの。
    pub measured: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct BwOutcome {
    /// 遅れて入った観戦者が入ってから追いつく（打ち切る）までの間に対戦の便が列で待ったµs（並べ替え済み）。
    pub waits: Vec<u32>,
    /// 入る前の5秒の同じ値の最大（観戦者が追いついている間の基準）。
    pub before_max: u32,
    pub duel_dropped: u64,
    pub dropped: u64,
    pub catchup_ms: Option<u32>,
    pub cuts: u64,
    pub pauses: u64,
    pub resumes: u64,
    /// 最初に止めた時刻（ms）とそれまでに半分にした回数。
    pub first_pause: Option<(u32, u64)>,
}

impl BwOutcome {
    pub(super) fn max(&self) -> u32 {
        self.waits.last().copied().unwrap_or(0)
    }

    pub(super) fn pct(&self, p: usize) -> u32 {
        if self.waits.is_empty() {
            return 0;
        }
        let i = (self.waits.len() * p / 100).min(self.waits.len() - 1);
        self.waits[i]
    }
}

pub(super) fn run_bw(c: BwCase) -> BwOutcome {
    let one_way = (c.rtt / 2).max(1);
    let mut rng = Rng(c.seed ^ 0xB4D0);
    let frames = u64::from(c.late_s + c.limit_s + 5) * 63;
    let script = make_script(&mut rng, 1, frames);
    let mut sim = Sim::new(
        Net::new(c.seed, one_way, 2, 0, 0),
        SpectateHost::with_capacity(host_cfg(), 65_536),
        script,
    );
    sim.host_poll_every = 16;
    sim.net.uplink = Some(Uplink::new(c.bps, c.buffer_ms));
    sim.host
        .note_host_uplink(c.measured.unwrap_or((c.bps / 8) as u32));
    if let Some(b) = c.fixed {
        sim.host.use_fixed_rate(b);
    }
    sim.duel = Some(Box::new(Duel::new(c.feed)));
    let start = c.late_s * 1_000;
    for n in 1..=c.live + c.late {
        let at = if n <= c.live { 0 } else { start };
        sim.add_watcher(n, at, 0, ASM_CAP);
        let w = sim.watchers.last_mut().expect("足した");
        w.w.set_ff_max(c.ff_max);
        w.display = Some(Box::new(Display::new(Policy::Pace, one_way, c.ff_max)));
    }
    let end = start + c.limit_s * 1_000;
    let mut done_at: Option<u32> = None;
    let mut first_pause = None;
    while sim.now < end {
        sim.tick();
        if first_pause.is_none() {
            let t = sim.host.throttle_counts();
            if t.pauses > 0 {
                first_pause = Some((sim.now, t.cuts));
            }
        }
        if done_at.is_none()
            && c.late > 0
            && sim.now > start
            && sim
                .watchers
                .iter()
                .skip(usize::from(c.live))
                .all(|w| w.display.as_ref().is_some_and(|d| d.caught_up_at.is_some()))
        {
            done_at = Some(sim.now);
        }
        if done_at.is_some_and(|t| sim.now >= t + 3_000) {
            break;
        }
    }
    // 使った列はホストが積んだ列の頭からの続きと一致する。
    for w in &sim.watchers {
        for (k, (idx, rec)) in w.got.iter().enumerate() {
            assert_eq!(*idx as usize, k, "{c:?}: 番号が連続でない");
            assert_eq!(*rec, sim.log[k], "{c:?}: 番号 {k} の記録が違う");
        }
    }
    let up = sim.net.uplink.as_ref().expect("出口");
    // 遅れて入る観戦者がいなければ試合の頭の2秒（往復の基準を覚える間）の後から終わりまで。
    let (from, stop) = if c.late > 0 {
        (start, done_at.unwrap_or(end))
    } else {
        (2_000, end)
    };
    let mut waits: Vec<u32> = up
        .duel_waits
        .iter()
        .filter(|(t, _)| (from..stop).contains(t))
        .map(|&(_, w)| w)
        .collect();
    waits.sort_unstable();
    let before_max = up
        .duel_waits
        .iter()
        .filter(|(t, _)| (start.saturating_sub(5_000)..start).contains(t))
        .map(|&(_, w)| w)
        .max()
        .unwrap_or(0);
    let tc = sim.host.throttle_counts();
    let late_caught: Option<u32> = sim
        .watchers
        .iter()
        .skip(usize::from(c.live))
        .map(|w| w.display.as_ref().and_then(|d| d.caught_up_at))
        .try_fold(0u32, |m, t| t.map(|t| m.max(t - start)));
    BwOutcome {
        waits,
        before_max,
        duel_dropped: up.duel_dropped,
        dropped: up.dropped,
        catchup_ms: late_caught,
        cuts: tc.cuts,
        pauses: tc.pauses,
        resumes: tc.resumes,
        first_pause,
    }
}

fn line(label: &str, c: &BwCase, o: &BwOutcome) -> String {
    format!(
        "| {label} | {} kbps | {} ms | 追いついている {} 人 + 遅れて入る {} 人（ff_max {}） | {} | {:.1} / {:.1} / {:.1} ms | {:.1} ms | {} / {} | {} | {}・{}・{} |",
        c.bps / 1_000,
        c.rtt,
        c.live,
        c.late,
        c.ff_max,
        o.waits.len(),
        f64::from(o.max()) / 1_000.0,
        f64::from(o.pct(99)) / 1_000.0,
        f64::from(o.pct(50)) / 1_000.0,
        f64::from(o.before_max) / 1_000.0,
        o.duel_dropped,
        o.dropped,
        o.catchup_ms.map_or_else(
            || format!("{} s で追いつかない", c.limit_s),
            |m| format!("{:.1} s", f64::from(m) / 1_000.0)
        ),
        o.cuts,
        o.pauses,
        o.resumes,
    )
}

fn case(bps: u64, live: u16, late: u16, ff_max: u32) -> BwCase {
    BwCase {
        bps,
        buffer_ms: 250,
        rtt: 30,
        live,
        late,
        late_s: 30,
        ff_max,
        fixed: None,
        feed: true,
        seed: 0x5EED_B4D0,
        limit_s: 120,
        measured: None,
    }
}

/// 比べる相手: 往復を観戦者への送信へ渡さず、上りも太いと測れた形（追いつきの送りは全部の上限のまま）。
fn blind_of(c: BwCase) -> BwCase {
    BwCase {
        feed: false,
        measured: Some(u32::MAX),
        ..c
    }
}

/// 遅れて入った観戦者が追いつく間に対戦の便が出口の列で待つ増分の上限（ms）。往復の延びの目安（`spectate_rate::TARGET_MS` 20 ms）と同じ値。
/// 観戦者への送信はこの延びを越えると下がる。
/// そのため観戦者への送信が延びを越えて対戦の便を待たせるのは、送信を下げる判断が効くまでの間だけのはず。
const DUEL_WAIT_MAX_MS: u32 = higanr_core::spectate_rate::TARGET_MS;

/// 上り1 Mbpsと0.5 Mbpsで30秒遅れて観戦者が入る（追いついている観戦者1人がいるところへ1人入るか、全体の上限いっぱいの最大の早送りの観戦者が2人入る）。
/// 追いつく間に対戦の便が列で待つ増分は最大でも`DUEL_WAIT_MAX_MS`以下。対戦の便は1通も落ちず、観戦者は追いつく。
#[test]
fn a_late_spectator_catching_up_on_a_thin_uplink_keeps_the_match_flow_wait_under_the_target() {
    let ff_top = higanr_core::spectate_pace::FF_UPDATES_MAX;
    for bps in [1_000_000, 500_000] {
        for (live, late, ff) in [(1, 1, FF_UPDATES_DEFAULT), (1, 1, ff_top), (0, 2, ff_top)] {
            let c = case(bps, live, late, ff);
            let o = run_bw(c);
            eprintln!("{}", line("新", &c, &o));
            assert!(
                o.waits.len() > 100,
                "{c:?}: 追いつく間の対戦の便が少ない（{}）",
                o.waits.len()
            );
            assert!(o.catchup_ms.is_some(), "{c:?}: 追いつかない");
            assert_eq!(o.duel_dropped, 0, "{c:?}: 対戦の便が落ちた");
            assert!(
                o.max() <= DUEL_WAIT_MAX_MS * 1_000,
                "{c:?}: 対戦の便が列で最大 {} µs 待った（p99 {} µs）",
                o.max(),
                o.pct(99)
            );
        }
    }
}

/// 観戦者への送信だけで上りが詰まる回線（上り0.25 Mbpsに追いついている観戦者10人。1人1秒に60通の流れでヘッダ込み約4.7 KB/s）:
/// 観戦者への追いつきの送信が無いので送る量を半分にせずすぐ止める。止めない形（観戦者への送信へ往復を渡さない）と比べて対戦の便の待ちと落ちが減る。
#[test]
fn when_live_spectators_alone_fill_the_uplink_the_host_pauses_them_and_the_match_flow_recovers() {
    let c = BwCase {
        late: 0,
        late_s: 10,
        limit_s: 20,
        ..case(250_000, 10, 0, FF_UPDATES_DEFAULT)
    };
    let adaptive = run_bw(c);
    let blind = run_bw(blind_of(c));
    eprintln!(
        "{}\n{}\n最初に止めた時刻と、それまでに半分にした回数: {:?}",
        line("新", &c, &adaptive),
        line("往復を渡さない", &c, &blind),
        adaptive.first_pause
    );
    let (paused_at, cuts_before) = adaptive.first_pause.expect("止めていない");
    assert_eq!(
        cuts_before, 0,
        "追いつきの送りが無いのに半分にしてから止めた"
    );
    // 窓（2秒）を待たず、直近の数個の延びが`SURGE_MS`を越えたところで止める。
    assert!(paused_at <= 1_000, "止めるのが遅い: {paused_at} ms");
    assert!(adaptive.resumes > 0, "止めた送りを戻していない");
    assert!(
        blind.pct(50) > 100_000,
        "往復を渡さない形で詰まらない（この検査の狙いが外れた）: p50 {} µs",
        blind.pct(50)
    );
    assert!(
        adaptive.pct(99) <= BW_LIVE_P99_MAX_MS * 1_000,
        "止めても待ちが下がらない: p99 {} µs（渡さない形 {} µs）",
        adaptive.pct(99),
        blind.pct(99)
    );
    assert!(
        adaptive.duel_dropped * 10 < blind.duel_dropped,
        "止めても対戦の便が落ちる: {}（渡さない形 {}）",
        adaptive.duel_dropped,
        blind.duel_dropped
    );
}

/// `when_live_spectators_alone_fill_…`の対戦の便の待ちのp99の上限（ms）。模型で測った189 msに余白（往復を渡さない形は246 ms）。
const BW_LIVE_P99_MAX_MS: u32 = 210;

/// 上限の和が上りを越える回線（上り0.2 Mbpsに最大の早送りの観戦者が2人遅れて入る。上限の和27,880 B/s ＞ 上り25,000 B/s）:
/// 上りの1/4から始め、直近の延びが出たら上げるのをやめる（窓の延びが目安を越えたら半分にする）。
/// 往復を渡さない形より対戦の便の待ちの最大とp99が小さい。観戦者は追いつく。
#[test]
fn when_the_caps_exceed_the_uplink_the_host_stops_raising_and_the_match_flow_waits_less_than_without_it()
 {
    let c = case(200_000, 0, 2, higanr_core::spectate_pace::FF_UPDATES_MAX);
    let adaptive = run_bw(c);
    let blind = run_bw(blind_of(c));
    eprintln!(
        "{}\n{}",
        line("新", &c, &adaptive),
        line("往復を渡さない", &c, &blind)
    );
    assert!(adaptive.catchup_ms.is_some(), "追いつかない");
    assert!(
        blind.pct(99) > adaptive.pct(99) && blind.max() > adaptive.max(),
        "往復を渡さない形より待ちが小さくない: 最大 {} / p99 {} µs（渡さない形 {} / {} µs）",
        adaptive.max(),
        adaptive.pct(99),
        blind.max(),
        blind.pct(99)
    );
    assert!(
        adaptive.max() <= BW_THIN_MAX_MS * 1_000,
        "対戦の便が列で最大 {} µs 待った",
        adaptive.max()
    );
}

/// `when_the_caps_exceed_the_uplink_…`の対戦の便の待ちの最大の上限（ms）。模型で測った65 msに2割ほどの余白（往復を渡さない形は243 ms）。
const BW_THIN_MAX_MS: u32 = 80;

/// 追いついている観戦者への送信は全体の上限に数えない。
/// 追いついている8人（1人1秒に約3 KB）がいても遅れて入った1人は自分の上限の速さで追いつく（上りは絞らない）。
#[test]
fn live_spectators_do_not_use_up_the_total_cap_of_a_late_one() {
    let c = case(100_000_000, 8, 1, FF_UPDATES_DEFAULT);
    let o = run_bw(c);
    eprintln!("{}", line("新", &c, &o));
    let ms = o.catchup_ms.expect("追いつかない");
    // ff_max 8の1人だけのときと同じ速さ（30秒の遅れを8倍で約4.5秒）に余白を足した値。
    assert!(ms <= 7_000, "追いつくまで {ms} ms");
}

/// 対戦の便が列で待つ増分の表（標準エラー）。「旧」の行は固定の全体の上限64,000 B/s（観戦者ごとの上限と往復の延びによる下げなし）で比べる。
#[test]
#[ignore = "手で回す測り。表を標準エラーに出す"]
fn measure_the_match_flow_wait_on_thin_uplinks_new_and_old() {
    eprintln!(
        "| 形 | 上り | 往復 | 観戦者 | 対戦の便の数 | 列で待った 最大 / p99 / p50 | 入る前の最大 | 落ちた対戦の便 / 落ちた便 | 追いつくまで | 下げ・止め・戻し |"
    );
    for bps in [1_000_000, 500_000, 200_000] {
        for (live, late, ff) in [(1, 1, FF_UPDATES_DEFAULT), (1, 1, 16), (0, 2, 16)] {
            let c = case(bps, live, late, ff);
            let new = run_bw(c);
            eprintln!("{}", line("新", &c, &new));
            let old = BwCase {
                fixed: Some(higanr_core::spectate_rate::FIXED_COMPARE_BPS),
                feed: false,
                ..c
            };
            let o = run_bw(old);
            eprintln!("{}", line("旧", &old, &o));
        }
    }
}
