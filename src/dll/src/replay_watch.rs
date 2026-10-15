//! 素の花映塚でのリプレイの再生を見張り、各ラウンドの決着の tick の値をログへ 1 行残す（pure。unsafe 無し）。リプレイの印の `rN` と
//! 突き合わせて、生きた試合どおりに再生できたかを機械で確かめるため（道具は `higanr_core::replay_playcheck`）。
//! - ゲームのスレッドの判断（`PlayWatch::tick`）: 入力の地点で毎 tick、Match の場面・再生の側（`+0x114` が 0 でない）かを見て、
//!   生きた試合と同じ `MatchLog` / `read_boundary` で境目の値を積む。再生を出たら（場面を出る・生きた側になる）1 回分を `PlayHandoff` に置く。
//!   確保・ファイル・ロック待ち・パニックしうる添字を置かない。生きた試合とネットワークの経路の状態には触らない。
//!   1 ラウンドも決着しなかった再生（タイトルのデモなど）と、直前と同じ中身の再生は置かない。
//! - 受け渡し（`PlayHandoff`。原子変数の小さなリング）: 書き手はゲームのスレッド 1 本、読み手は本ログへ行を頼むスレッド 1 本。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering, fence};

use higanr_core::replay_playcheck::{HEADER_FIELD_LEN, Ident, format_line};
use higanr_core::replay_rounds::{MatchLog, MatchSummary, SUMMARY_WORDS};

use crate::netplay::SCENE_MATCH;
use crate::replay_save::SaveIo;
use crate::th09_addr::GAME_MODE_MATCH;

/// 再生した見出しの日付と名前の生のバイト（`+0xC4` / `+0xCE` の各 9 バイト）。行に書ける字にするのは読み手（`Ident::from_header`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderIdent {
    pub date: [u8; HEADER_FIELD_LEN],
    pub name: [u8; HEADER_FIELD_LEN],
}

/// 1 回の再生の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayReport {
    pub ident: Option<HeaderIdent>,
    /// 勝ちに要る数に届いた最後のラウンドまで見た（false は途中で抜けた）。
    pub complete: bool,
    pub summary: MatchSummary,
}

impl PlayReport {
    /// 本ログへ書く 1 行（`replay_playcheck::format_line`）。確保する。ゲームのスレッドでは呼ばない。
    pub fn line(&self) -> String {
        let ident = self
            .ident
            .map_or_else(Ident::default, |i| Ident::from_header(&i.date, &i.name));
        format_line(
            self.complete,
            &ident,
            &self.summary.detail(0).rounds,
            self.summary.rounds_dropped,
        )
    }
}

/// リングの枠の数。読み手は 100 ms ごとに空にするので、その間に 3 回再生が終わることは無い。溢れたら読み手が数える。
pub const SLOTS: usize = 4;
const IDENT_BYTES: usize = 2 * HEADER_FIELD_LEN;
const IDENT_WORDS: usize = IDENT_BYTES.div_ceil(4);
const F_COMPLETE: u32 = 1;
const F_IDENT: u32 = 1 << 1;
const SLOT_WORDS: usize = 1 + IDENT_WORDS + SUMMARY_WORDS;

fn to_words(r: &PlayReport) -> [u32; SLOT_WORDS] {
    let mut w = [0u32; SLOT_WORDS];
    w[0] = if r.complete { F_COMPLETE } else { 0 } | if r.ident.is_some() { F_IDENT } else { 0 };
    if let Some(id) = r.ident {
        let mut b = [0u8; IDENT_WORDS * 4];
        b[..HEADER_FIELD_LEN].copy_from_slice(&id.date);
        b[HEADER_FIELD_LEN..IDENT_BYTES].copy_from_slice(&id.name);
        for (i, c) in b.as_chunks::<4>().0.iter().enumerate() {
            w[1 + i] = u32::from_le_bytes(*c);
        }
    }
    w[1 + IDENT_WORDS..].copy_from_slice(&r.summary.to_words());
    w
}

fn from_words(w: &[u32; SLOT_WORDS]) -> PlayReport {
    let ident = (w[0] & F_IDENT != 0).then(|| {
        let mut b = [0u8; IDENT_WORDS * 4];
        for (i, c) in b.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *c = w[1 + i].to_le_bytes();
        }
        let mut date = [0u8; HEADER_FIELD_LEN];
        let mut name = [0u8; HEADER_FIELD_LEN];
        date.copy_from_slice(&b[..HEADER_FIELD_LEN]);
        name.copy_from_slice(&b[HEADER_FIELD_LEN..IDENT_BYTES]);
        HeaderIdent { date, name }
    });
    let mut s = [0u32; SUMMARY_WORDS];
    s.copy_from_slice(&w[1 + IDENT_WORDS..]);
    PlayReport {
        ident,
        complete: w[0] & F_COMPLETE != 0,
        summary: MatchSummary::from_words(&s),
    }
}

/// ゲームのスレッド（書き手 1 本）から本ログへ頼むスレッド（読み手 1 本）への受け渡し。
pub struct PlayHandoff {
    /// 置いた回数（枠は `written % SLOTS`）。書き手だけが進める。
    written: AtomicU32,
    slots: [[AtomicU32; SLOT_WORDS]; SLOTS],
}

impl PlayHandoff {
    pub const fn new() -> Self {
        PlayHandoff {
            written: AtomicU32::new(0),
            slots: [const { [const { AtomicU32::new(0) }; SLOT_WORDS] }; SLOTS],
        }
    }

    /// 1 回分を置く（書き手）。枠を埋めてから回数を `Release` で進める。
    pub fn publish(&self, r: &PlayReport) {
        let n = self.written.load(Ordering::Relaxed);
        let slot = &self.slots[n as usize % SLOTS];
        for (a, v) in slot.iter().zip(to_words(r)) {
            a.store(v, Ordering::Relaxed);
        }
        self.written.store(n.wrapping_add(1), Ordering::Release);
    }

    /// 置かれた分を古い順に `f` へ渡す（読み手）。`read` は読み終えた回数（呼び手が持つ）。書き手に追い越された分は数えて返す。
    pub fn drain(&self, read: &mut u32, mut f: impl FnMut(PlayReport)) -> u32 {
        let mut lost = 0u32;
        let w = self.written.load(Ordering::Acquire);
        let behind = w.wrapping_sub(*read);
        // 書き手が次に書く枠は読めないので、信じるのは新しい `SLOTS - 1` 回分まで。
        let keep = SLOTS as u32 - 1;
        if behind > keep {
            lost += behind - keep;
            *read = w.wrapping_sub(keep);
        }
        while *read != w {
            let slot = &self.slots[*read as usize % SLOTS];
            let words: [u32; SLOT_WORDS] =
                core::array::from_fn(|i| slot[i].load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            // 書き手が 1 周して同じ枠を書き始めていたら、いま読んだ中身は混ざっている。
            let overrun = self.written.load(Ordering::Relaxed).wrapping_sub(*read) >= SLOTS as u32;
            *read = read.wrapping_add(1);
            if overrun {
                lost += 1;
            } else {
                f(from_words(&words));
            }
        }
        lost
    }
}

impl Default for PlayHandoff {
    fn default() -> Self {
        Self::new()
    }
}

/// ゲームのスレッドの状態。
#[derive(Debug)]
pub struct PlayWatch {
    log: MatchLog,
    /// 再生の試合を見張っている最中。
    active: bool,
    /// 勝ちに要る数（境目を積むたびに読み直す。0 なら決着の判定をしない）。
    required: u32,
    ident: Option<HeaderIdent>,
    /// この Match の場面で調べた ReplayManager と、再生の側か。場面を出るまで引き直さない（`+0x114` の読みは 1 試合に 1 回）。
    mode: Option<(u32, bool)>,
    /// 最後に書いた再生（同じ中身を続けて書かないため）。
    last: Option<PlayReport>,
}

impl Default for PlayWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayWatch {
    pub const fn new() -> Self {
        PlayWatch {
            log: MatchLog::new(),
            active: false,
            required: 0,
            ident: None,
            mode: None,
            last: None,
        }
    }

    /// 再生の側か。ReplayManager が替われば引き直す。読めなければ偽（次の tick にもう 1 回）。
    fn is_playback(&mut self, io: &mut dyn SaveIo, rm: u32) -> bool {
        if rm == 0 {
            self.mode = None;
            return false;
        }
        if let Some((m, p)) = self.mode
            && m == rm
        {
            return p;
        }
        match io.replay_mode(rm) {
            Some(v) => {
                self.mode = Some((rm, v != 0));
                v != 0
            }
            None => false,
        }
    }

    /// 入力の地点で毎 tick。Match の場面でなければ、場面の 2 語を読んで戻る。
    pub fn tick(&mut self, h: &PlayHandoff, io: &mut dyn SaveIo) {
        let (cur, _) = io.scenes();
        if cur != SCENE_MATCH || io.game_mode() != GAME_MODE_MATCH {
            self.mode = None;
            self.leave(h);
            return;
        }
        // 読み込みの間は始めも終わりもしない（読み込みの途中で ReplayManager の欄は揃っていない）。
        if io.loading() != 0 {
            return;
        }
        let rm = io.replay_manager();
        if !self.is_playback(io, rm) {
            self.leave(h);
            return;
        }
        let completed = io.completed_rounds();
        if !self.active {
            self.active = true;
            self.required = 0;
            self.ident = io.replay_ident(rm);
            self.log.start(0, completed, None);
        }
        let seen = self.log.rounds().seen();
        self.log.observe_rounds(completed, || io.read_boundary());
        if completed != seen {
            self.required = io.wins()[2];
        }
    }

    /// 再生の側でなくなった（場面を出た・生きた試合になった）。見張っていたなら 1 回分を置く。
    fn leave(&mut self, h: &PlayHandoff) {
        if !self.active {
            return;
        }
        self.active = false;
        let summary = self.log.summary();
        let complete = self.required > 0
            && summary
                .rounds
                .iter()
                .map_while(|b| *b)
                .last()
                .is_some_and(|b| b.wins.iter().any(|&w| w >= self.required));
        let report = PlayReport {
            ident: self.ident.take(),
            complete,
            summary,
        };
        self.log.begin_seg();
        // 1 ラウンドも決着しなかった再生と、直前の行と中身が同じ再生は書かない。タイトルで放っておくと流れるデモは Match の再生なので
        // （game_mode 2・`+0x114` が 1）、書くと何もしていないログが毎回増える。
        if report.summary.rounds[0].is_none() || self.last == Some(report) {
            return;
        }
        self.last = Some(report);
        h.publish(&report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_save::tests::FakeGame;
    use higanr_core::replay_playcheck::{PLAY_MARK, parse_log};
    use higanr_core::replay_rounds::SideSnap;

    fn snap(life: u32, score: u32) -> Option<SideSnap> {
        Some(SideSnap {
            life,
            c23: 3,
            boss: 2,
            rev: 1,
            score,
        })
    }

    fn playback() -> FakeGame {
        FakeGame {
            replay_mode: Some(1),
            wins: [0, 0, 2],
            ident: Some(HeaderIdent {
                date: *b"26/10/14\0",
                name: *b"12:34:56\0",
            }),
            ..FakeGame::default()
        }
    }

    /// ラウンドを 1 つ決着させる（`completed_rounds` と勝ち数が増え、その tick の値が読める）。
    fn decide(g: &mut FakeGame, winner: usize, frames: u32, score1: u32) {
        g.completed += 1;
        g.wins[winner] += 1;
        g.round_frames = frames;
        g.sides = [snap(2, score1), snap(0, 50)];
    }

    fn lines(h: &PlayHandoff, read: &mut u32) -> Vec<String> {
        let mut out = Vec::new();
        h.drain(read, |r| out.push(r.line()));
        out
    }

    #[test]
    fn a_full_playback_publishes_every_round_and_says_complete() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = playback();
        let mut read = 0;
        for _ in 0..5 {
            w.tick(&h, &mut g);
        }
        assert_eq!(g.boundary_reads, 0, "決着の無い間は席の塊を読まない");
        decide(&mut g, 0, 9781, 1000);
        w.tick(&h, &mut g);
        w.tick(&h, &mut g);
        decide(&mut g, 0, 6582, 2000);
        w.tick(&h, &mut g);
        assert_eq!(g.boundary_reads, 2, "決着の tick に 1 回ずつ");
        assert!(lines(&h, &mut read).is_empty(), "場面を出るまで書かない");
        g.cur = 10;
        w.tick(&h, &mut g);
        let l = lines(&h, &mut read);
        assert_eq!(l.len(), 1);
        assert!(l[0].starts_with(PLAY_MARK));
        assert!(l[0].contains("end=complete date=26/10/14 name=12:34:56 rounds=2"));
        assert!(
            l[0].contains(" r1=9781.1.2.0.3.3.2.2.1.1.10000.500"),
            "{}",
            l[0]
        );
        assert!(
            l[0].contains(" r2=6582.1.2.0.0.0.0.0.0.0.20000.500"),
            "{}",
            l[0]
        );
        // 出た後の tick は何も足さない。
        w.tick(&h, &mut g);
        assert!(lines(&h, &mut read).is_empty());
        // 行は道具が読める形。
        let p = parse_log(&l[0]);
        assert!(p.bad.is_empty());
        assert_eq!(p.blocks[0].rounds.len(), 2);
    }

    #[test]
    fn a_live_match_is_never_watched() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = FakeGame {
            replay_mode: Some(0),
            wins: [0, 0, 2],
            ..FakeGame::default()
        };
        let mut read = 0;
        for k in 0..3 {
            w.tick(&h, &mut g);
            decide(&mut g, 0, 100 + k, 1);
        }
        g.wins = [2, 0, 2];
        w.tick(&h, &mut g);
        g.cur = 10;
        w.tick(&h, &mut g);
        assert_eq!(g.boundary_reads, 0, "記録の側では積まない");
        assert!(lines(&h, &mut read).is_empty());
        assert_eq!(h.written.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn other_game_modes_and_unreadable_modes_are_not_watched() {
        for (mode, rm, replay_mode) in [
            (0, 0x1000, Some(1)),
            (1, 0x1000, Some(1)),
            (2, 0, Some(1)),
            (2, 0x1000, None),
        ] {
            let h = PlayHandoff::new();
            let mut w = PlayWatch::new();
            let mut g = FakeGame {
                mode,
                rm,
                replay_mode,
                ..playback()
            };
            w.tick(&h, &mut g);
            decide(&mut g, 0, 100, 1);
            w.tick(&h, &mut g);
            g.cur = 10;
            w.tick(&h, &mut g);
            assert_eq!(
                h.written.load(Ordering::SeqCst),
                0,
                "{mode} {rm} {replay_mode:?}"
            );
            assert_eq!(g.boundary_reads, 0);
        }
    }

    #[test]
    fn leaving_early_says_partial_and_keeps_the_rounds_seen() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = playback();
        let mut read = 0;
        w.tick(&h, &mut g);
        decide(&mut g, 1, 5000, 7);
        w.tick(&h, &mut g);
        w.tick(&h, &mut g);
        g.cur = 10;
        w.tick(&h, &mut g);
        let l = lines(&h, &mut read);
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("途中で終わった end=partial"), "{}", l[0]);
        assert!(l[0].contains("rounds=1 r1=5000.2."), "{}", l[0]);
    }

    /// タイトルのデモのように 1 ラウンドも決着せずに出る再生と、同じ中身の再生の繰り返しは書かない。
    #[test]
    fn a_playback_without_a_decided_round_and_an_exact_repeat_are_not_written() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut read = 0;
        let once = |w: &mut PlayWatch, rounds: u32| {
            let mut g = playback();
            w.tick(&h, &mut g);
            for k in 0..rounds {
                decide(&mut g, 0, 100 + k, 5);
                w.tick(&h, &mut g);
            }
            g.cur = 10;
            w.tick(&h, &mut g);
        };
        once(&mut w, 0);
        assert!(lines(&h, &mut read).is_empty(), "決着の無い再生");
        once(&mut w, 1);
        assert_eq!(lines(&h, &mut read).len(), 1);
        once(&mut w, 1);
        assert!(lines(&h, &mut read).is_empty(), "続けて同じ中身");
        once(&mut w, 2);
        assert_eq!(lines(&h, &mut read).len(), 1, "中身が違えば書く");
        once(&mut w, 1);
        assert_eq!(lines(&h, &mut read).len(), 1, "間に別の再生があれば書く");
    }

    #[test]
    fn a_decided_last_round_with_the_wins_target_not_reached_is_still_partial() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = playback();
        let mut read = 0;
        w.tick(&h, &mut g);
        decide(&mut g, 0, 5000, 7);
        w.tick(&h, &mut g);
        g.cur = 10;
        w.tick(&h, &mut g);
        assert!(lines(&h, &mut read)[0].contains("end=partial"));
        // 勝ちに要る数が読めない（0）ときは、届いたとは言わない。
        let mut g = playback();
        g.wins = [0, 0, 0];
        w.tick(&h, &mut g);
        decide(&mut g, 0, 5000, 8);
        w.tick(&h, &mut g);
        g.cur = 10;
        w.tick(&h, &mut g);
        assert!(lines(&h, &mut read)[0].contains("end=partial"));
    }

    #[test]
    fn loading_ticks_neither_start_nor_end_a_playback() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = playback();
        g.loading = 1;
        w.tick(&h, &mut g);
        assert!(!w.active, "読み込み中は始めない");
        g.loading = 0;
        w.tick(&h, &mut g);
        assert!(w.active);
        g.loading = 1;
        w.tick(&h, &mut g);
        assert!(w.active, "読み込み中の tick で終わらせない");
        assert_eq!(h.written.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_mode_is_read_once_per_match_and_asked_again_in_the_next_one() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = FakeGame {
            replay_mode: Some(0),
            ..playback()
        };
        for _ in 0..10 {
            w.tick(&h, &mut g);
        }
        assert_eq!(g.mode_reads, 1, "生きた試合でも 1 試合に 1 回");
        g.cur = 10;
        w.tick(&h, &mut g);
        g.cur = SCENE_MATCH;
        g.replay_mode = Some(1);
        w.tick(&h, &mut g);
        assert_eq!(g.mode_reads, 2, "場面を出たら引き直す");
        assert!(w.active, "同じアドレスでも再生として始まる");
    }

    #[test]
    fn a_live_match_after_a_playback_ends_the_playback_block() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut g = playback();
        let mut read = 0;
        w.tick(&h, &mut g);
        decide(&mut g, 0, 100, 1);
        w.tick(&h, &mut g);
        // 場面を出ずに ReplayManager が別のもの（記録の側）に替わった。
        g.rm += 0x100;
        g.replay_mode = Some(0);
        w.tick(&h, &mut g);
        assert_eq!(lines(&h, &mut read).len(), 1);
        assert!(!w.active);
    }

    #[test]
    fn two_playbacks_in_a_row_do_not_share_rounds() {
        let h = PlayHandoff::new();
        let mut w = PlayWatch::new();
        let mut read = 0;
        for score in [111, 222] {
            let mut g = playback();
            w.tick(&h, &mut g);
            decide(&mut g, 0, 100, score);
            w.tick(&h, &mut g);
            g.cur = 10;
            w.tick(&h, &mut g);
        }
        let l = lines(&h, &mut read);
        assert_eq!(l.len(), 2);
        assert!(
            l[0].contains("rounds=1") && l[0].contains(&format!(".{}0.", 111)),
            "{}",
            l[0]
        );
        assert!(
            l[1].contains("rounds=1") && l[1].contains(&format!(".{}0.", 222)),
            "{}",
            l[1]
        );
    }

    #[test]
    fn the_ring_hands_over_in_order_and_counts_what_the_writer_overran() {
        let h = PlayHandoff::new();
        let mk = |n: u32| PlayReport {
            ident: None,
            complete: n.is_multiple_of(2),
            summary: {
                let mut m = MatchLog::new();
                m.start(u64::from(n), 0, None);
                m.summary()
            },
        };
        let mut read = 0;
        for n in 0..3 {
            h.publish(&mk(n));
        }
        let mut got = Vec::new();
        assert_eq!(h.drain(&mut read, |r| got.push(r.summary.start_utc)), 0);
        assert_eq!(got, vec![Some(0), Some(1), Some(2)]);
        // 読み手が 7 回分遅れた: 新しい SLOTS - 1 回分だけ残り、追い越された分を数える。
        for n in 10..17 {
            h.publish(&mk(n));
        }
        got.clear();
        let lost = h.drain(&mut read, |r| got.push(r.summary.start_utc));
        assert_eq!(lost, 4);
        assert_eq!(got, vec![Some(14), Some(15), Some(16)]);
        assert_eq!(h.drain(&mut read, |_| panic!("もう無い")), 0);
    }

    /// 書き手が読み手より速く回っても、渡された 1 件は 1 回分の中身のまま（別の回の語が混ざらない）で、渡した数 + 落とした数 = 置いた数。
    #[test]
    fn a_fast_writer_never_hands_over_a_torn_report() {
        use std::sync::atomic::AtomicBool;
        const TOTAL: u32 = 60_000;
        // n 回目の中身: 見出しの日付 = n の 10 進、開始時刻 = n、落としたラウンド数 = n。どれかが食い違えば混ざっている。
        let mk = |n: u32| {
            let mut date = [0u8; HEADER_FIELD_LEN];
            let digits = format!("{n:08}");
            date[..8].copy_from_slice(digits.as_bytes());
            let mut m = MatchLog::new();
            m.start(u64::from(n), 0, None);
            let mut s = m.summary();
            s.rounds_dropped = n;
            PlayReport {
                ident: Some(HeaderIdent {
                    date,
                    name: [b'x'; HEADER_FIELD_LEN],
                }),
                complete: n.is_multiple_of(2),
                summary: s,
            }
        };
        let reports: Vec<PlayReport> = (0..TOTAL).map(mk).collect();
        let h = PlayHandoff::new();
        let done = AtomicBool::new(false);
        let (mut delivered, mut lost, mut torn, mut last) = (0u32, 0u32, 0u32, None::<u32>);
        std::thread::scope(|s| {
            s.spawn(|| {
                for r in &reports {
                    h.publish(r);
                }
                done.store(true, Ordering::SeqCst);
            });
            let mut read = 0u32;
            loop {
                let finished = done.load(Ordering::SeqCst);
                lost += h.drain(&mut read, |r| {
                    delivered += 1;
                    let n = r.summary.start_utc.unwrap_or(u64::MAX) as u32;
                    let digits = r.ident.map(|i| i.date[..8].to_vec());
                    let ok = digits == Some(format!("{n:08}").into_bytes())
                        && r.summary.rounds_dropped == n
                        && r.complete == n.is_multiple_of(2)
                        && last.is_none_or(|l| n > l);
                    if !ok {
                        torn += 1;
                    }
                    last = Some(n);
                });
                if finished {
                    break;
                }
            }
        });
        assert_eq!(torn, 0, "混ざった・順が戻った report の数");
        assert_eq!(delivered + lost, TOTAL);
        assert!(delivered > 0);
    }

    #[test]
    fn a_report_survives_the_word_round_trip() {
        let mut m = MatchLog::new();
        m.start(0, 0, None);
        m.observe_rounds(1, || higanr_core::replay_rounds::Boundary {
            frames: 77,
            wins: [1, 0],
            side: [snap(2, 9), None],
        });
        let r = PlayReport {
            ident: Some(HeaderIdent {
                date: *b"26/10/14\0",
                name: *b"ab cd\0\0\0\0",
            }),
            complete: true,
            summary: m.summary(),
        };
        assert_eq!(from_words(&to_words(&r)), r);
        let r2 = PlayReport {
            ident: None,
            complete: false,
            ..r
        };
        assert_eq!(from_words(&to_words(&r2)), r2);
        assert!(r.line().contains("name=ab_cd"));
        assert!(r2.line().contains("end=partial") && !r2.line().contains("name="));
    }
}
