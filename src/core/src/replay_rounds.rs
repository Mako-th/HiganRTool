//! マッチの最中に積む、マッチの記録（pure。確保しない・パニックしない。ゲームのスレッドで使う）。
//!
//! - `RoundLog`: ラウンドの境目（`completed_rounds` が増えた tick）の値を、境目の番号の枠に積む。カウンタは境目どうしの差を取る前の
//!   累計のまま持ち、差と勝者は `round_records` が保存の時に出す（保管庫のスレッド）。
//! - `CommTally`: ping の標本から中央値と最大を出す（固定の度数表）。
//! - `MatchLog`: 1 マッチ（SaveGate の 1 seg）の分をまとめる構造体。`MatchSummary` が保存の時に保管庫のスレッドへ渡す固定長のコピー。
//!
//! 境目の値をどの tick で読むかは呼び手が決める（`RoundLog::observe` は呼ばれた時点の値を読むだけ）。rollback では書き戻しで予測の
//! tick の境目が消えるので、呼び手は確定した tick の後だけ呼ぶ。それでも増えた枠は番号を鍵に上書きし、数が戻れば後ろの枠を捨てる。
#![forbid(unsafe_code)]

use crate::replay_detail::{CommSummary, MatchDetail, RoundRec, SyncKind, Winner};

/// 記録するラウンドの上限。ゲームは Match の勝ちに要る数を 2 に決める（`0x0041AFBA`。隣リポ TR）ので 3 ラウンドが最大で、
/// 他の道具が要る数を変えても 7（4 本先取）まで収まる。越えた分は捨てて数える。
pub const MAX_ROUNDS: usize = 7;
/// ping の度数表の目盛り（1 ms ごと。最後の目盛りは 255 ms 以上）。
pub const PING_BUCKETS: usize = 256;

/// 席ごとの、境目の tick の値。ライフとスコアはそのまま、カウンタはマッチ通しの累計。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SideSnap {
    pub life: u32,
    /// C2 + C3（Player の `spell_attacks`）。
    pub c23: u32,
    /// ボスアタック（`boss_attacks`）。
    pub boss: u32,
    /// リバーサル（`boss_reversals`）。
    pub rev: u32,
    /// 内部のスコア（画面に出る値の 1/10）。
    pub score: u32,
}

/// ラウンドの境目の tick の値。`side` は読めなかった席が `None`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Boundary {
    /// `round_frames`。
    pub frames: u32,
    /// 1P・2P の勝ち数（境目の tick で勝者の分が増えた後）。
    pub wins: [u32; 2],
    pub side: [Option<SideSnap>; 2],
}

#[derive(Debug, Clone, Copy)]
pub struct RoundLog {
    slots: [Option<Boundary>; MAX_ROUNDS],
    /// 最後に見た `completed_rounds`。
    seen: u32,
}

impl Default for RoundLog {
    fn default() -> Self {
        Self::new()
    }
}

impl RoundLog {
    pub const fn new() -> Self {
        RoundLog {
            slots: [None; MAX_ROUNDS],
            seen: 0,
        }
    }

    /// マッチの始まり: 空にして、いまの `completed_rounds`（ふつう 0）から数える。
    pub fn start_at(&mut self, completed: u32) {
        *self = RoundLog::new();
        self.seen = completed;
    }

    /// `completed` は読んだ `completed_rounds`。増えたときだけ `read` を呼び（毎 tick の費用は 4 バイトの比べ 1 回）、その値を番号の
    /// 枠へ入れる（同じ番号の枠は上書き）。減ったときは書き戻しで境目が消えたか次のマッチなので、その数より後ろの枠を捨てる。
    pub fn observe(&mut self, completed: u32, read: impl FnOnce() -> Boundary) {
        use core::cmp::Ordering;
        match completed.cmp(&self.seen) {
            Ordering::Equal => {}
            Ordering::Greater => {
                let b = read();
                // completed > seen >= 0 なので 1 以上。
                let idx = usize::try_from(completed - 1).unwrap_or(usize::MAX);
                if let Some(slot) = self.slots.get_mut(idx) {
                    *slot = Some(b);
                }
                self.seen = completed;
            }
            Ordering::Less => {
                for (i, slot) in self.slots.iter_mut().enumerate() {
                    if u32::try_from(i).map_or(true, |i| i >= completed) {
                        *slot = None;
                    }
                }
                self.seen = completed;
            }
        }
    }

    /// 上限を越えて記録できなかったラウンドの数（いまの `completed_rounds` から導く。数が戻れば減る）。
    pub fn dropped(&self) -> u32 {
        self.seen.saturating_sub(MAX_ROUNDS as u32)
    }

    /// 1 ラウンド目から途切れずに並んだ分。
    pub fn boundaries(&self) -> impl Iterator<Item = &Boundary> {
        self.slots.iter().map_while(Option::as_ref)
    }

    pub fn seen(&self) -> u32 {
        self.seen
    }
}

/// ping の標本の度数表。中央値は小さい方から数えて ceil(n/2) 番目（偶数個なら下の側）。
#[derive(Debug, Clone, Copy)]
pub struct CommTally {
    hist: [u32; PING_BUCKETS],
    samples: u32,
    max: u32,
}

impl Default for CommTally {
    fn default() -> Self {
        Self::new()
    }
}

impl CommTally {
    pub const fn new() -> Self {
        CommTally {
            hist: [0; PING_BUCKETS],
            samples: 0,
            max: 0,
        }
    }

    pub fn sample(&mut self, ms: u32) {
        let b = usize::try_from(ms).map_or(PING_BUCKETS - 1, |m| m.min(PING_BUCKETS - 1));
        if let Some(c) = self.hist.get_mut(b) {
            *c = c.saturating_add(1);
        }
        self.samples = self.samples.saturating_add(1);
        self.max = self.max.max(ms);
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    pub fn max(&self) -> Option<u32> {
        (self.samples > 0).then_some(self.max)
    }

    /// 中央値（ms）。255 ms 以上の標本は 255 にまとめて数える。標本が無ければ `None`。
    pub fn median(&self) -> Option<u32> {
        if self.samples == 0 {
            return None;
        }
        let want = self.samples.div_ceil(2);
        let mut acc = 0u32;
        for (ms, c) in self.hist.iter().enumerate() {
            acc = acc.saturating_add(*c);
            if acc >= want {
                return u32::try_from(ms).ok();
            }
        }
        // 度数の足し込みが飽和したときだけ。最大を返す。
        Some(self.max)
    }
}

/// ネットワークのセッションの累計の数え（接続の始まりからの合計。マッチの分は差で出す）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetTotals {
    /// 相手の入力ワードを待って止まった数（delay 方式は描画だけにしたフレーム、rollback は入力の地点が待った数）。
    pub stalls: u64,
    /// 進め直しの回数と、そのために余分に進めた tick の数。
    pub rewinds: u64,
    pub resim_ticks: u64,
}

/// 毎 tick にネットワークのセッションから取る値。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetSample {
    /// 今の形式が rollback か。
    pub rollback: bool,
    /// 接続の中で何マッチ目か（1 から）。マッチの最初の入力の地点は開始のフックの前なので、前のマッチの番号が見える。毎 tick 取り直す。
    pub match_no: u32,
    pub totals: NetTotals,
}

#[derive(Debug, Clone, Copy)]
struct NetPart {
    rollback: bool,
    match_no: u32,
    base: NetTotals,
    last: NetTotals,
}

impl NetPart {
    fn from_first(s: NetSample) -> NetPart {
        NetPart {
            rollback: s.rollback,
            match_no: s.match_no,
            base: s.totals,
            last: s.totals,
        }
    }
}

/// 1 マッチ（`SaveGate` の 1 seg）の分の記録。
#[derive(Debug, Clone, Copy)]
pub struct MatchLog {
    started: bool,
    start_utc: u64,
    net: Option<NetPart>,
    rounds: RoundLog,
    ping: CommTally,
}

impl Default for MatchLog {
    fn default() -> Self {
        Self::new()
    }
}

impl MatchLog {
    pub const fn new() -> Self {
        MatchLog {
            started: false,
            start_utc: 0,
            net: None,
            rounds: RoundLog::new(),
            ping: CommTally::new(),
        }
    }

    /// 新しい seg の始まり: 前のマッチの分を捨てる。
    pub fn begin_seg(&mut self) {
        *self = MatchLog::new();
    }

    /// 読み込みが終わって最初の tick で呼ぶ（`start` を呼ぶまで境目も ping も積まない）。
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// `completed` はそのときの `completed_rounds`（ふつう 0）、`net` はネットワークのセッションがあれば最初の値（累計の数えの基準になる）。
    pub fn start(&mut self, utc: u64, completed: u32, net: Option<NetSample>) {
        *self = MatchLog::new();
        self.started = true;
        self.start_utc = utc;
        self.rounds.start_at(completed);
        self.net = net.map(NetPart::from_first);
    }

    /// 毎 tick（ネットワークのセッションがあるときだけ `net` を渡す）。`ping_ms` は最後に測れた往復の時間。
    pub fn sample(&mut self, ping_ms: Option<u32>, net: Option<NetSample>) {
        if !self.started {
            return;
        }
        if let Some(p) = ping_ms {
            self.ping.sample(p);
        }
        if let Some(s) = net {
            match self.net.as_mut() {
                Some(n) => {
                    n.rollback = s.rollback;
                    n.match_no = s.match_no;
                    n.last = s.totals;
                }
                None => self.net = Some(NetPart::from_first(s)),
            }
        }
    }

    /// 境目の見張り（`RoundLog::observe`）。始まっていなければ何もしない。
    pub fn observe_rounds(&mut self, completed: u32, read: impl FnOnce() -> Boundary) {
        if self.started {
            self.rounds.observe(completed, read);
        }
    }

    pub fn rounds(&self) -> &RoundLog {
        &self.rounds
    }

    pub fn summary(&self) -> MatchSummary {
        let mut rounds = [None; MAX_ROUNDS];
        for (dst, src) in rounds.iter_mut().zip(self.rounds.boundaries()) {
            *dst = Some(*src);
        }
        let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        MatchSummary {
            start_utc: self.started.then_some(self.start_utc),
            net: self.net.map(|n| NetSummary {
                rollback: n.rollback,
                match_no: n.match_no,
                ping: self.ping.median().zip(self.ping.max()),
                stalls: clamp(n.last.stalls.saturating_sub(n.base.stalls)),
                rewinds: clamp(n.last.rewinds.saturating_sub(n.base.rewinds)),
                resim_ticks: clamp(n.last.resim_ticks.saturating_sub(n.base.resim_ticks)),
            }),
            rounds,
            rounds_dropped: self.rounds.dropped(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetSummary {
    pub rollback: bool,
    pub match_no: u32,
    /// （中央値, 最大）ms。標本が無ければ `None`。
    pub ping: Option<(u32, u32)>,
    pub stalls: u32,
    pub rewinds: u32,
    pub resim_ticks: u32,
}

/// 保存の時にゲームのスレッドから保管庫のスレッドへ渡す、マッチの記録のコピー（固定長）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchSummary {
    pub start_utc: Option<u64>,
    /// ネットワーク対戦のときだけ。ひとりと観戦者は `None`。
    pub net: Option<NetSummary>,
    /// 1 ラウンド目から途切れずに並んだ分。
    pub rounds: [Option<Boundary>; MAX_ROUNDS],
    pub rounds_dropped: u32,
}

impl MatchSummary {
    pub const EMPTY: MatchSummary = MatchSummary {
        start_utc: None,
        net: None,
        rounds: [None; MAX_ROUNDS],
        rounds_dropped: 0,
    };

    /// 印に足す形。`build` は `update::BUILD_NUMBER`。
    pub fn detail(&self, build: u32) -> MatchDetail {
        let bs: Vec<Boundary> = self.rounds.iter().map_while(|b| *b).collect();
        MatchDetail {
            sync: self.net.map(|n| {
                if n.rollback {
                    SyncKind::Rollback
                } else {
                    SyncKind::Delay
                }
            }),
            build: Some(build),
            start_utc: self.start_utc,
            match_no: self.net.map(|n| n.match_no),
            comm: self.net.map(|n| CommSummary {
                ping_median_ms: n.ping.map(|p| p.0),
                ping_max_ms: n.ping.map(|p| p.1),
                stalls: n.stalls,
                rewinds: n.rewinds,
                resim_ticks: n.resim_ticks,
            }),
            rounds: round_records(&bs),
            rounds_dropped: self.rounds_dropped,
        }
    }
}

/// 境目の列から、ラウンドごとの記録を出す。勝者は勝ち数の増えた席、カウンタは前の境目との差（前が読めていなければ不明）、
/// ライフとスコアは境目の値のまま。
pub fn round_records(bs: &[Boundary]) -> Vec<RoundRec> {
    let mut out = Vec::with_capacity(bs.len());
    for (k, b) in bs.iter().enumerate() {
        let prev = k.checked_sub(1).and_then(|p| bs.get(p));
        let prev_wins = prev.map_or([0, 0], |p| p.wins);
        let winner = match (b.wins[0] > prev_wins[0], b.wins[1] > prev_wins[1]) {
            (true, false) => Winner::P1,
            (false, true) => Winner::P2,
            _ => Winner::Unknown,
        };
        // 1 ラウンド目の前の累計は 0（マッチの始まりで Player が作られる）。2 ラウンド目以降は前の境目が読めていなければ差が取れない。
        let before = |i: usize| -> Option<SideSnap> {
            match prev {
                None => Some(SideSnap::default()),
                Some(p) => p.side[i],
            }
        };
        let diff = |i: usize, pick: fn(&SideSnap) -> u32| -> Option<u32> {
            let cur = pick(&b.side[i]?);
            let was = pick(&before(i)?);
            // カウンタが減っていたら Player が作り直されている。境目の値がそのラウンドの数。
            Some(if cur >= was { cur - was } else { cur })
        };
        let at = |i: usize, pick: fn(&SideSnap) -> u32| b.side[i].as_ref().map(pick);
        out.push(RoundRec {
            frames: b.frames,
            winner,
            life: [at(0, |s| s.life), at(1, |s| s.life)],
            c23: [diff(0, |s| s.c23), diff(1, |s| s.c23)],
            boss: [diff(0, |s| s.boss), diff(1, |s| s.boss)],
            rev: [diff(0, |s| s.rev), diff(1, |s| s.rev)],
            score: [
                at(0, |s| s.score).map(|v| u64::from(v) * SCORE_FACTOR),
                at(1, |s| s.score).map(|v| u64::from(v) * SCORE_FACTOR),
            ],
        });
    }
    out
}

/// 画面に出るスコアは内部の値の 10 倍（隣リポ Memory_Map「表示スコア = 内部値×10」、`th09_monitor.py` の `s1a*10`）。
pub const SCORE_FACTOR: u64 = 10;

// ---- 固定長の語へのコピー（`Handoff` の原子変数の列に載せる） ----

const SIDE_WORDS: usize = 6;
const BOUNDARY_WORDS: usize = 4 + 2 * SIDE_WORDS;
const SUMMARY_HEAD_WORDS: usize = 10;
pub const SUMMARY_WORDS: usize = SUMMARY_HEAD_WORDS + MAX_ROUNDS * BOUNDARY_WORDS;

const F_START: u32 = 1;
const F_NET: u32 = 1 << 1;
const F_ROLLBACK: u32 = 1 << 2;
const F_PING: u32 = 1 << 3;

impl MatchSummary {
    pub fn to_words(&self) -> [u32; SUMMARY_WORDS] {
        let mut w = [0u32; SUMMARY_WORDS];
        let n = self.net;
        let flag = |on: bool, bit: u32| if on { bit } else { 0 };
        w[0] = flag(self.start_utc.is_some(), F_START)
            | flag(n.is_some(), F_NET)
            | flag(n.is_some_and(|n| n.rollback), F_ROLLBACK)
            | flag(n.is_some_and(|n| n.ping.is_some()), F_PING);
        let t = self.start_utc.unwrap_or(0);
        w[1] = t as u32;
        w[2] = (t >> 32) as u32;
        if let Some(n) = n {
            w[3] = n.match_no;
            let (med, max) = n.ping.unwrap_or((0, 0));
            w[4] = med;
            w[5] = max;
            w[6] = n.stalls;
            w[7] = n.rewinds;
            w[8] = n.resim_ticks;
        }
        w[9] = self.rounds_dropped;
        for (i, b) in self.rounds.iter().enumerate() {
            let Some(b) = b else { continue };
            let o = SUMMARY_HEAD_WORDS + i * BOUNDARY_WORDS;
            w[o] = 1;
            w[o + 1] = b.frames;
            w[o + 2] = b.wins[0];
            w[o + 3] = b.wins[1];
            for (s, snap) in b.side.iter().enumerate() {
                let Some(snap) = snap else { continue };
                let p = o + 4 + s * SIDE_WORDS;
                w[p] = 1;
                w[p + 1] = snap.life;
                w[p + 2] = snap.c23;
                w[p + 3] = snap.boss;
                w[p + 4] = snap.rev;
                w[p + 5] = snap.score;
            }
        }
        w
    }

    pub fn from_words(w: &[u32; SUMMARY_WORDS]) -> MatchSummary {
        let f = w[0];
        let net = (f & F_NET != 0).then(|| NetSummary {
            rollback: f & F_ROLLBACK != 0,
            match_no: w[3],
            ping: (f & F_PING != 0).then_some((w[4], w[5])),
            stalls: w[6],
            rewinds: w[7],
            resim_ticks: w[8],
        });
        let mut rounds = [None; MAX_ROUNDS];
        for (i, slot) in rounds.iter_mut().enumerate() {
            let o = SUMMARY_HEAD_WORDS + i * BOUNDARY_WORDS;
            if w[o] == 0 {
                continue;
            }
            let side = |s: usize| {
                let p = o + 4 + s * SIDE_WORDS;
                (w[p] != 0).then(|| SideSnap {
                    life: w[p + 1],
                    c23: w[p + 2],
                    boss: w[p + 3],
                    rev: w[p + 4],
                    score: w[p + 5],
                })
            };
            *slot = Some(Boundary {
                frames: w[o + 1],
                wins: [w[o + 2], w[o + 3]],
                side: [side(0), side(1)],
            });
        }
        MatchSummary {
            start_utc: (f & F_START != 0).then(|| u64::from(w[1]) | u64::from(w[2]) << 32),
            net,
            rounds,
            rounds_dropped: w[9],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(life: u32, c23: u32, boss: u32, rev: u32, score: u32) -> Option<SideSnap> {
        Some(SideSnap {
            life,
            c23,
            boss,
            rev,
            score,
        })
    }

    /// 偽のゲーム: 1 tick ごとに round_frames が進み、`round_end_at` の tick の中で completed_rounds と勝ち数が増える。
    /// 境目の後も round_frames・カウンタは動く（約 189 フレームの結果の演出の間）。
    struct FakeGame {
        tick: u32,
        frames: u32,
        completed: u32,
        wins: [u32; 2],
        c23: [u32; 2],
        life: [u32; 2],
    }

    impl FakeGame {
        fn new() -> Self {
            FakeGame {
                tick: 0,
                frames: 0,
                completed: 0,
                wins: [0; 2],
                c23: [0; 2],
                life: [10; 2],
            }
        }

        fn boundary(&self) -> Boundary {
            Boundary {
                frames: self.frames,
                wins: self.wins,
                side: [
                    snap(self.life[0], self.c23[0], 0, 0, 100),
                    snap(self.life[1], self.c23[1], 0, 0, 50),
                ],
            }
        }

        /// 1 tick 進める。`winner` があればこの tick でラウンドが終わる。
        fn step(&mut self, winner: Option<usize>) {
            self.tick += 1;
            self.frames += 1;
            if let Some(w) = winner {
                self.completed += 1;
                self.wins[w] += 1;
                self.life[1 - w] = 0;
            } else if self.completed > 0 {
                // 境目の後に動くカウンタ。
                self.c23[0] += 1;
            }
        }
    }

    #[test]
    fn reads_the_value_of_the_tick_where_the_count_rose_and_not_a_later_one() {
        let mut g = FakeGame::new();
        let mut log = RoundLog::new();
        let mut reads = 0;
        for t in 0..30 {
            g.step((t == 9).then_some(0));
            let b = g.boundary();
            let c = g.completed;
            log.observe(c, || {
                reads += 1;
                b
            });
        }
        assert_eq!(reads, 1, "増えたときの 1 回だけ読む");
        let got: Vec<_> = log.boundaries().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].frames, 10, "境目の tick（10 tick 目）の値");
        assert_eq!(
            got[0].side[0].unwrap().c23,
            0,
            "後の tick のカウンタではない"
        );
        assert_eq!(got[0].wins, [1, 0]);
    }

    #[test]
    fn equal_and_flat_counts_cost_no_read() {
        let mut log = RoundLog::new();
        for _ in 0..1000 {
            log.observe(0, || panic!("読まない"));
        }
        log.observe(1, Boundary::default);
        for _ in 0..1000 {
            log.observe(1, || panic!("読まない"));
        }
        assert_eq!(log.boundaries().count(), 1);
    }

    #[test]
    fn counters_are_differences_of_cumulative_values_and_winners_follow_the_wins() {
        let b =
            |frames, wins, s0: (u32, u32, u32, u32, u32), s1: (u32, u32, u32, u32, u32)| Boundary {
                frames,
                wins,
                side: [
                    snap(s0.0, s0.1, s0.2, s0.3, s0.4),
                    snap(s1.0, s1.1, s1.2, s1.3, s1.4),
                ],
            };
        let bs = [
            b(3000, [1, 0], (4, 2, 1, 0, 1000), (0, 1, 0, 0, 800)),
            b(2400, [1, 1], (0, 5, 3, 1, 2500), (6, 1, 2, 1, 1900)),
            b(1800, [2, 1], (2, 6, 3, 1, 3100), (0, 4, 2, 1, 2000)),
        ];
        let r = round_records(&bs);
        assert_eq!(r.len(), 3);
        assert_eq!(
            [r[0].winner, r[1].winner, r[2].winner],
            [Winner::P1, Winner::P2, Winner::P1]
        );
        // 1 ラウンド目は累計そのまま、2 ラウンド目以降は差。
        assert_eq!(r[0].c23, [Some(2), Some(1)]);
        assert_eq!(r[1].c23, [Some(3), Some(0)]);
        assert_eq!(r[2].c23, [Some(1), Some(3)]);
        assert_eq!(r[1].boss, [Some(2), Some(2)]);
        assert_eq!(r[2].boss, [Some(0), Some(0)]);
        assert_eq!(r[1].rev, [Some(1), Some(1)]);
        assert_eq!(r[2].rev, [Some(0), Some(0)]);
        // ライフとスコアは境目の値のまま（スコアは画面の値 = 内部 × 10）。
        assert_eq!(r[1].life, [Some(0), Some(6)]);
        assert_eq!(r[2].score, [Some(31_000), Some(20_000)]);
        assert_eq!([r[0].frames, r[2].frames], [3000, 1800]);
    }

    #[test]
    fn a_missing_previous_side_makes_the_difference_unknown_but_not_the_rest() {
        let full = Boundary {
            frames: 100,
            wins: [1, 0],
            side: [snap(1, 2, 0, 0, 5), snap(2, 3, 0, 0, 6)],
        };
        let cur = Boundary {
            frames: 200,
            wins: [2, 0],
            side: [snap(3, 4, 0, 0, 7), snap(4, 5, 0, 0, 8)],
        };
        let prev_gap = Boundary {
            side: [None, snap(2, 3, 0, 0, 6)],
            ..full
        };
        let r = round_records(&[prev_gap, cur]);
        assert_eq!(
            r[0].c23,
            [None, Some(3)],
            "1 ラウンド目の 1P は席が読めていない"
        );
        assert_eq!(r[1].c23, [None, Some(2)], "前が読めない席の差は不明");
        assert_eq!(r[1].life, [Some(3), Some(4)], "ライフは境目の値で出る");
    }

    #[test]
    fn a_counter_that_went_down_is_taken_as_this_rounds_value() {
        let a = Boundary {
            wins: [1, 0],
            side: [snap(1, 5, 4, 2, 0), snap(1, 0, 0, 0, 0)],
            ..Boundary::default()
        };
        let b = Boundary {
            wins: [1, 1],
            side: [snap(1, 2, 1, 0, 0), snap(1, 0, 0, 0, 0)],
            ..Boundary::default()
        };
        let r = round_records(&[a, b]);
        assert_eq!(r[1].c23, [Some(2), Some(0)]);
        assert_eq!(r[1].boss, [Some(1), Some(0)]);
        assert_eq!(r[1].rev, [Some(0), Some(0)]);
    }

    #[test]
    fn the_winner_is_unknown_unless_exactly_one_win_count_rose() {
        let mk = |wins| Boundary {
            wins,
            ..Boundary::default()
        };
        assert_eq!(round_records(&[mk([0, 0])])[0].winner, Winner::Unknown);
        assert_eq!(round_records(&[mk([1, 1])])[0].winner, Winner::Unknown);
        assert_eq!(round_records(&[mk([0, 1])])[0].winner, Winner::P2);
        assert_eq!(round_records(&[mk([1, 0])])[0].winner, Winner::P1);
    }

    #[test]
    fn a_rolled_back_boundary_is_dropped_and_a_replayed_one_overwrites() {
        let mut log = RoundLog::new();
        let at = |f| Boundary {
            frames: f,
            ..Boundary::default()
        };
        log.observe(1, || at(100));
        log.observe(2, || at(200));
        assert_eq!(log.boundaries().count(), 2);
        // 予測の tick の 2 ラウンド目が書き戻しで消えた。
        log.observe(1, || panic!("減ったときは読まない"));
        assert_eq!(
            log.boundaries().map(|b| b.frames).collect::<Vec<_>>(),
            [100]
        );
        // 確定の入力ワードでは 2 ラウンド目が別の tick で終わった。
        log.observe(2, || at(205));
        assert_eq!(
            log.boundaries().map(|b| b.frames).collect::<Vec<_>>(),
            [100, 205]
        );
        // 1 ラウンド目まで消えた後、終わらないままなら空。
        log.observe(0, || panic!("読まない"));
        assert_eq!(log.boundaries().count(), 0);
        assert_eq!(log.seen(), 0);
    }

    #[test]
    fn rounds_beyond_the_cap_are_counted_and_the_count_follows_a_rollback() {
        let mut log = RoundLog::new();
        for c in 1..=(MAX_ROUNDS as u32 + 2) {
            log.observe(c, || Boundary {
                frames: c,
                ..Boundary::default()
            });
        }
        assert_eq!(log.boundaries().count(), MAX_ROUNDS);
        assert_eq!(log.dropped(), 2);
        log.observe(MAX_ROUNDS as u32 + 1, || panic!("読まない"));
        assert_eq!(log.dropped(), 1);
        log.observe(MAX_ROUNDS as u32 - 1, || panic!("読まない"));
        assert_eq!(log.dropped(), 0);
        assert_eq!(log.boundaries().count(), MAX_ROUNDS - 1);
    }

    #[test]
    fn a_jump_over_a_round_leaves_a_gap_and_only_the_prefix_is_used() {
        let mut log = RoundLog::new();
        log.observe(3, Boundary::default);
        assert_eq!(log.boundaries().count(), 0, "1・2 ラウンド目が無い");
        // 数が戻ってから 1 ラウンド目が終われば、普通に積まれる。
        log.observe(0, Boundary::default);
        log.observe(1, Boundary::default);
        assert_eq!(log.boundaries().count(), 1);
    }

    #[test]
    fn start_at_forgets_the_previous_match() {
        let mut log = RoundLog::new();
        log.observe(2, Boundary::default);
        log.start_at(0);
        assert_eq!((log.boundaries().count(), log.seen()), (0, 0));
        // 前のマッチの値が残っていても、始まりの値から数える。
        log.start_at(3);
        log.observe(3, || panic!("読まない"));
        log.observe(4, Boundary::default);
        assert_eq!(log.boundaries().count(), 0, "4 ラウンド目の枠だけ");
    }

    #[test]
    fn ping_median_and_max() {
        let mut t = CommTally::new();
        assert_eq!((t.median(), t.max()), (None, None));
        for ms in [30, 20, 100, 25, 40] {
            t.sample(ms);
        }
        assert_eq!(t.median(), Some(30));
        assert_eq!(t.max(), Some(100));
        // 偶数個は下の側。
        t.sample(10);
        assert_eq!(t.median(), Some(25));
        // 大きい値は目盛りの端にまとめるが、最大はそのまま。
        let mut big = CommTally::new();
        big.sample(5);
        big.sample(900);
        big.sample(1200);
        assert_eq!(big.median(), Some(255));
        assert_eq!(big.max(), Some(1200));
        let mut one = CommTally::new();
        one.sample(0);
        assert_eq!((one.median(), one.max()), (Some(0), Some(0)));
    }

    #[test]
    fn a_match_log_only_counts_after_start_and_uses_differences_of_the_totals() {
        let mut m = MatchLog::new();
        m.sample(Some(500), None);
        m.observe_rounds(1, || panic!("始まる前は読まない"));
        assert_eq!(m.summary().net, None);
        assert_eq!(m.summary().start_utc, None);
        m.start(
            1_790_000_000,
            0,
            Some(NetSample {
                rollback: false,
                match_no: 1,
                totals: NetTotals {
                    stalls: 10,
                    rewinds: 100,
                    resim_ticks: 400,
                },
            }),
        );
        // 最初の tick は開始のフックの前なので、番号と形式は後の tick の値に替わる。
        for p in [20, 40, 30] {
            m.sample(
                Some(p),
                Some(NetSample {
                    rollback: true,
                    match_no: 2,
                    totals: NetTotals {
                        stalls: 13,
                        rewinds: 112,
                        resim_ticks: 450,
                    },
                }),
            );
        }
        m.observe_rounds(1, || Boundary {
            frames: 77,
            ..Boundary::default()
        });
        let s = m.summary();
        assert_eq!(s.start_utc, Some(1_790_000_000));
        assert_eq!(
            s.net,
            Some(NetSummary {
                rollback: true,
                match_no: 2,
                ping: Some((30, 40)),
                stalls: 3,
                rewinds: 12,
                resim_ticks: 50
            })
        );
        assert_eq!(s.rounds[0].unwrap().frames, 77);
        // 接続が作り直されて累計が減っても、負の値にはしない。
        m.sample(
            None,
            Some(NetSample {
                rollback: true,
                match_no: 2,
                totals: NetTotals {
                    stalls: 1,
                    rewinds: 0,
                    resim_ticks: 0,
                },
            }),
        );
        let n = m.summary().net.unwrap();
        assert_eq!((n.stalls, n.rewinds, n.resim_ticks), (0, 0, 0));
        m.begin_seg();
        assert!(!m.is_started());
        assert_eq!(m.summary(), MatchSummary::EMPTY);
    }

    #[test]
    fn a_match_without_a_net_box_has_no_net_part() {
        let mut m = MatchLog::new();
        m.start(5, 0, None);
        m.sample(Some(40), None);
        let s = m.summary();
        assert_eq!(s.net, None);
        let d = s.detail(9);
        assert_eq!((d.sync, d.match_no, d.comm), (None, None, None));
        assert_eq!((d.build, d.start_utc), (Some(9), Some(5)));
    }

    #[test]
    fn words_round_trip_every_shape() {
        let mut full = MatchSummary::EMPTY;
        full.start_utc = Some(0x1_2345_6789);
        full.net = Some(NetSummary {
            rollback: false,
            match_no: 7,
            ping: Some((31, 90)),
            stalls: 1,
            rewinds: 2,
            resim_ticks: 3,
        });
        full.rounds_dropped = 4;
        for i in 0..MAX_ROUNDS {
            full.rounds[i] = Some(Boundary {
                frames: 100 + i as u32,
                wins: [i as u32, 1],
                side: [
                    snap(i as u32, 2, 3, 4, 5),
                    if i % 2 == 0 {
                        None
                    } else {
                        snap(9, 8, 7, 6, 5)
                    },
                ],
            });
        }
        for s in [
            MatchSummary::EMPTY,
            full,
            MatchSummary {
                net: Some(NetSummary {
                    rollback: true,
                    ping: None,
                    ..full.net.unwrap()
                }),
                rounds: {
                    let mut r = [None; MAX_ROUNDS];
                    r[0] = full.rounds[0];
                    r
                },
                ..full
            },
        ] {
            assert_eq!(MatchSummary::from_words(&s.to_words()), s);
        }
    }

    #[test]
    fn the_detail_of_a_three_round_match_is_small() {
        let mut m = MatchLog::new();
        m.start(
            1_790_000_000,
            0,
            Some(NetSample {
                rollback: true,
                match_no: 12,
                totals: NetTotals::default(),
            }),
        );
        let mut g = FakeGame::new();
        for (i, w) in [0usize, 1, 0].into_iter().enumerate() {
            for _ in 0..1800 {
                g.step(None);
                m.sample(Some(30), None);
            }
            g.step(Some(w));
            let b = g.boundary();
            let c = g.completed;
            m.observe_rounds(c, || b);
            assert_eq!(m.rounds().boundaries().count(), i + 1);
        }
        let d = m.summary().detail(403_920);
        assert_eq!(d.rounds.len(), 3);
        assert_eq!(
            d.rounds.iter().map(|r| r.winner).collect::<Vec<_>>(),
            [Winner::P1, Winner::P2, Winner::P1]
        );
        let bytes: usize = d
            .fields()
            .iter()
            .map(|(k, v)| 1 + k.len() + 1 + v.len())
            .sum();
        assert!(bytes < 260, "3 ラウンドで {bytes} バイト");
    }
}
