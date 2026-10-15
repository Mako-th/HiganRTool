//! マッチの始まりと終わりを、ネットワークの数の変わり目から本ログの 1 行にする（pure。unsafe 無し）。
//!
//! 読み取りのスレッドが 1 秒ごとにネットワークの数（`NetSnapshot`）を渡す。簡潔なログでもマッチごとの節目が残るように、
//! 始まったマッチの数が増えたとき（始まり）と、マッチの最中の段階（`PHASE_LOCKSTEP`）を出たとき（終わり）に 1 行ずつ返す。
#![forbid(unsafe_code)]

use crate::net_watch::{NetSnapshot, phase_code};
use crate::netplay::Phase;

#[derive(Debug, Default)]
pub struct MatchWatch {
    /// 前の秒の「始まったマッチの数」と「マッチの最中だったか」。
    segs: u32,
    in_match: bool,
    /// この接続の中で終わりを書いたマッチの数（接続が替わったら数え直す）。
    ended: u32,
    /// 前の秒のセッションを作った時刻（`NetSnapshot::link_started_ms`）。変わったら新しい接続。
    link: u32,
    /// 観戦者の今のマッチで、対戦者の値の行を出したか。
    peer_shown: bool,
}

/// 観戦者の実行で、流れから対戦者の値を受けていれば「対戦者: rollback・delay N」/「対戦者: delay 方式 N」。
fn peer_text(snap: &NetSnapshot) -> Option<String> {
    let p = snap.peer.filter(|_| snap.watcher)?;
    Some(if p.rollback.is_some() {
        format!("対戦者: rollback・delay {}", p.latency)
    } else {
        format!("対戦者: delay 方式 {}", p.latency)
    })
}

impl MatchWatch {
    /// 1 秒ごとに呼ぶ。`k` は計測の経過秒。書く行を返す（無ければ空）。
    pub fn step(&mut self, snap: &NetSnapshot, k: u32) -> Vec<String> {
        let mut lines = Vec::new();
        // ネットワークの数は接続ごとに 0 から数え直される（つなぎ直し）。セッションを作り直した（時刻が変わった）か、数が減ったら新しい接続。
        // 前の接続のマッチの数と同じ数のまま次の接続の最初のマッチが始まっても、数え直しを逃さない。
        if snap.link_started_ms != self.link || snap.segs_started < self.segs {
            self.link = snap.link_started_ms;
            self.segs = 0;
            self.ended = 0;
            self.in_match = false;
            self.peer_shown = false;
        }
        while self.segs < snap.segs_started {
            self.segs += 1;
            self.peer_shown = false;
            let what = if snap.watcher {
                // 観戦者は遅延と rollback の欄を書かないので、対戦者の値は流れで受けた分だけを出す。
                match peer_text(snap) {
                    Some(t) => {
                        self.peer_shown = true;
                        format!("観戦・{t}")
                    }
                    None => "観戦".to_string(),
                }
            } else {
                format!(
                    "{}・遅延 {}",
                    if snap.rollback {
                        "rollback"
                    } else {
                        "delay 方式"
                    },
                    snap.delay
                )
            };
            lines.push(format!(
                "試合: {} 試合目が始まった（{what}、計測 {k} 秒の時点）",
                self.segs
            ));
        }
        let now_in_match = snap.phase == phase_code(Phase::Lockstep);
        if now_in_match
            && !self.peer_shown
            && let Some(t) = peer_text(snap)
        {
            self.peer_shown = true;
            lines.push(format!("試合: {t}（計測 {k} 秒の時点）"));
        }
        if self.in_match && !now_in_match {
            self.ended += 1;
            lines.push(format!(
                "試合: {} 試合目が終わった（この接続の合計: 使ったフレーム {}・stall {}・CHECK の一致 {}、計測 {k} 秒の時点）",
                self.ended, snap.played, snap.stalls, snap.check_matches
            ));
        }
        self.in_match = now_in_match;
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::net_menu::PeerStats;

    fn snap(segs: u32, phase: u32) -> NetSnapshot {
        NetSnapshot {
            segs_started: segs,
            phase,
            played: 100 * segs,
            ..NetSnapshot::default()
        }
    }

    /// 壊れたら落ちる: 前の接続と同じマッチの数のまま新しい接続へ移ると「始まった」が出ない。
    #[test]
    fn a_new_link_with_the_same_match_count_still_reports_its_first_match() {
        let mut w = MatchWatch::default();
        let on = |link: u32, segs: u32, phase: u32| NetSnapshot {
            link_started_ms: link,
            ..snap(segs, phase)
        };
        w.step(&on(1000, 1, 4), 1);
        let end = w.step(&on(1000, 1, 5), 2);
        assert!(end[0].contains("1 試合目が終わった"), "{end:?}");
        // 数が 0 に戻る秒を見逃し、新しい接続の最初のマッチ（数は 1）が次の秒に見える。
        let start = w.step(&on(5000, 1, 4), 3);
        assert!(start[0].contains("1 試合目が始まった"), "{start:?}");
        let end = w.step(&on(5000, 1, 1), 4);
        assert!(end[0].contains("1 試合目が終わった"), "{end:?}");
    }

    #[test]
    fn one_line_at_the_start_and_one_at_the_end_of_each_match() {
        let mut w = MatchWatch::default();
        assert!(w.step(&snap(0, 1), 1).is_empty(), "選択の最中は書かない");
        let start = w.step(&snap(1, 4), 2);
        assert_eq!(start.len(), 1);
        assert!(start[0].contains("1 試合目が始まった"), "{start:?}");
        assert!(w.step(&snap(1, 4), 3).is_empty(), "試合の最中は書かない");
        let end = w.step(&snap(1, 1), 4);
        assert_eq!(end.len(), 1);
        assert!(end[0].contains("1 試合目が終わった"), "{end:?}");
        assert!(w.step(&snap(1, 1), 5).is_empty());
        let again = w.step(&snap(2, 4), 6);
        assert!(again[0].contains("2 試合目が始まった"), "{again:?}");
    }

    fn watching(segs: u32, phase: u32, peer: Option<PeerStats>) -> NetSnapshot {
        NetSnapshot {
            watcher: true,
            peer,
            ..snap(segs, phase)
        }
    }

    /// 壊れたら落ちる: 観戦者の行に、書いていない遅延 0・delay 方式が出る。
    #[test]
    fn a_watchers_line_has_no_format_or_delay_of_its_own() {
        let mut w = MatchWatch::default();
        let start = w.step(&watching(1, 4, None), 1);
        assert_eq!(start.len(), 1, "{start:?}");
        assert!(start[0].contains("1 試合目が始まった（観戦、"), "{start:?}");
        assert!(
            !start[0].contains("delay")
                && !start[0].contains("rollback")
                && !start[0].contains("遅延"),
            "{start:?}"
        );
        assert!(w.step(&watching(1, 4, None), 2).is_empty());
    }

    /// 壊れたら落ちる: 流れで受けた対戦者の値が出ない・同じ値を毎秒出す・delay 方式を rollback と書く。
    #[test]
    fn a_watcher_shows_the_players_values_once_they_arrive() {
        let rb = PeerStats::from_record(14, 2, 3);
        let dl = PeerStats::from_record(14, 1, higanr_core::wire::STATS_ROLLBACK_NONE);
        let mut w = MatchWatch::default();
        let start = w.step(&watching(1, 4, Some(rb)), 1);
        assert!(
            start[0].contains("観戦・対戦者: rollback・delay 2"),
            "{start:?}"
        );
        assert!(w.step(&watching(1, 4, Some(rb)), 2).is_empty());
        // 値が試合の始まりの後に届く場合は、届いた秒に 1 行。
        let mut w = MatchWatch::default();
        assert!(w.step(&watching(1, 4, None), 1)[0].ends_with("時点）"));
        assert!(w.step(&watching(1, 4, None), 2).is_empty());
        let late = w.step(&watching(1, 4, Some(dl)), 3);
        assert_eq!(late.len(), 1, "{late:?}");
        assert!(late[0].contains("対戦者: delay 方式 1"), "{late:?}");
        assert!(!late[0].contains("rollback"), "{late:?}");
        assert!(w.step(&watching(1, 4, Some(dl)), 4).is_empty());
        // 次の試合ではまた出す。
        let again = w.step(&watching(2, 4, Some(rb)), 5);
        assert!(again[0].contains("対戦者: rollback・delay 2"), "{again:?}");
    }

    /// 壊れたら落ちる: 対戦者の側の行に、観戦用の値が混ざる。
    #[test]
    fn a_player_keeps_its_own_format_and_delay() {
        let mut w = MatchWatch::default();
        let s = NetSnapshot {
            rollback: true,
            delay: 3,
            peer: Some(PeerStats::from_record(1, 9, 9)),
            ..snap(1, 4)
        };
        let start = w.step(&s, 1);
        assert_eq!(start.len(), 1, "{start:?}");
        assert!(start[0].contains("rollback・遅延 3"), "{start:?}");
        assert!(!start[0].contains("対戦者"), "{start:?}");
    }

    #[test]
    fn leaving_the_link_during_a_match_counts_as_the_end() {
        let mut w = MatchWatch::default();
        w.step(&snap(1, 4), 1);
        let end = w.step(&snap(1, 5), 2);
        assert!(end[0].contains("終わった"), "{end:?}");
    }

    #[test]
    fn numbers_start_again_when_a_new_link_resets_the_counts() {
        let mut w = MatchWatch::default();
        w.step(&snap(2, 4), 1);
        w.step(&snap(2, 5), 2);
        let first = w.step(&snap(1, 4), 3);
        assert!(first[0].contains("1 試合目が始まった"), "{first:?}");
        let end = w.step(&snap(1, 1), 4);
        assert!(end[0].contains("1 試合目が終わった"), "{end:?}");
    }
}
