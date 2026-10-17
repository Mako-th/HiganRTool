//! マッチの始まりと終わりを、ネットワークの数の変わり目から本ログの 1 行にする（pure。unsafe 無し）。
//!
//! 読み取りのスレッドが 1 秒ごとにネットワークの数（`NetSnapshot`）を渡す。簡潔なログでもマッチごとの節目が残るように、
//! 始まったマッチの数が増えたとき（始まり）と、マッチの最中の段階（`PHASE_LOCKSTEP`）を出たとき（終わり）に 1 行ずつ返す。
//! 終わりの番号は始まりの番号と同じ（「N 試合目が始まった」に必ず「N 試合目が終わった」が対応する）。1 秒の間に段階を見逃した終わり
//! （次の試合がすぐ始まった）・接続が終わった・接続が替わった終わりは、理由つきで同じ番号の終わりを書く。
#![forbid(unsafe_code)]

use crate::net_watch::{NetSnapshot, phase_code};
use crate::netplay::Phase;
use crate::trial_summary::{N_END, end_item};

#[derive(Debug, Default)]
pub struct MatchWatch {
    /// 前の秒までに始まりを書いたマッチの数（この接続の中の番号。接続が替わったら数え直す）。
    segs: u32,
    /// 始まりを書いたが、終わりをまだ書いていないマッチがある（その番号は `segs`）。
    open: bool,
    /// 開いているマッチで、マッチの最中の段階を見たか。見る前の段階の変わりを終わりと読まない。
    seen_lockstep: bool,
    /// 前の秒のセッションを作った時刻（`NetSnapshot::link_started_ms`）。変わったら新しい接続。
    link: u32,
    /// 観戦者の今のマッチで、対戦者の値の行を出したか。
    peer_shown: bool,
    /// 前の秒の「この接続の合計」（使ったフレーム・stall・CHECK の一致）。見逃した終わりは、次の試合や新しい接続の数でなくこちらで書く。
    last: (u32, u32, u32),
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
    /// 開いているマッチの終わりの行を書いて閉じる。`why` は通常の終わり（段階を出た）以外の理由。`totals` はこの接続の合計。
    fn close(&mut self, why: Option<&str>, totals: (u32, u32, u32), k: u32) -> String {
        self.open = false;
        let why = why.map_or(String::new(), |w| format!("{w}。"));
        format!(
            "試合: {} 試合目が終わった（{why}この接続の合計: 使ったフレーム {}・stall {}・CHECK の一致 {}、計測 {k} 秒の時点）",
            self.segs, totals.0, totals.1, totals.2
        )
    }

    /// 1 秒ごとに呼ぶ。`k` は計測の経過秒。書く行を返す（無ければ空）。
    pub fn step(&mut self, snap: &NetSnapshot, k: u32) -> Vec<String> {
        let mut lines = Vec::new();
        // ネットワークの数は接続ごとに 0 から数え直される（つなぎ直し）。セッションを作り直した（時刻が変わった）か、数が減ったら新しい接続。
        // 前の接続のマッチの数と同じ数のまま次の接続の最初のマッチが始まっても、数え直しを逃さない。
        if snap.link_started_ms != self.link || snap.segs_started < self.segs {
            if self.open {
                lines.push(self.close(Some("接続が替わるまで終わりを見なかった"), self.last, k));
            }
            self.link = snap.link_started_ms;
            self.segs = 0;
            self.seen_lockstep = false;
            self.peer_shown = false;
        }
        while self.segs < snap.segs_started {
            if self.open {
                lines.push(self.close(
                    Some("次の試合が始まるまで終わりを見なかった"),
                    self.last,
                    k,
                ));
            }
            self.segs += 1;
            self.open = true;
            self.seen_lockstep = false;
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
        let totals = (snap.played, snap.stalls, snap.check_matches);
        // 接続が終わった秒は、終わりの理由を添える（中断・相手が切った・自分から終えた など）。マッチの最中の段階を一度も見ないまま終わった試合も閉じる。
        if self.open && snap.link_ended() {
            let reason = end_item(N_END, snap.end, snap.end_detail).text;
            lines.push(self.close(Some(&format!("接続が終わったため（{reason}）")), totals, k));
        } else if self.open && self.seen_lockstep && !now_in_match {
            lines.push(self.close(None, totals, k));
        }
        self.seen_lockstep = self.open && (self.seen_lockstep || now_in_match);
        self.last = totals;
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
        // 前の試合は終わりの段階を見ないまま次が始まったので、その終わりの行が先に出る。
        let again = w.step(&watching(2, 4, Some(rb)), 5);
        assert!(again[0].contains("1 試合目が終わった"), "{again:?}");
        assert!(again[1].contains("対戦者: rollback・delay 2"), "{again:?}");
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

    /// 「N 試合目が始まった」「N 試合目が終わった」の番号を、行から拾う。
    fn numbers(lines: &[String], word: &str) -> Vec<u32> {
        lines
            .iter()
            .filter_map(|l| {
                let rest = l.strip_prefix("試合: ")?;
                let (n, tail) = rest.split_once(" 試合目が")?;
                if tail.starts_with(word) {
                    n.parse().ok()
                } else {
                    None
                }
            })
            .collect()
    }

    /// 壊れたら落ちる: 段階が試合の最中のまま次の試合が始まった（再戦が 1 秒の間に済んだ）とき、前の試合の終わりの行が出ず、
    /// 始まりの数が終わりの数より多くなる。接続が試合の最中に終わったとき、終わりに理由が付かない。
    #[test]
    fn every_started_match_gets_an_end_line_with_the_same_number() {
        let mut w = MatchWatch::default();
        let mut all = Vec::new();
        // 1 試合目: 段階を出て終わる。2 試合目: 段階が試合の最中のまま 3 試合目が始まる。3 試合目: 試合の最中に接続が終わる。
        let seconds = [
            snap(1, 4),
            snap(1, 1),
            snap(2, 4),
            snap(3, 4),
            NetSnapshot {
                end: 5,
                end_detail: 1,
                ..snap(3, 5)
            },
        ];
        for (k, s) in seconds.iter().enumerate() {
            all.extend(w.step(s, k as u32 + 1));
        }
        assert_eq!(numbers(&all, "始まった"), [1, 2, 3], "{all:#?}");
        assert_eq!(numbers(&all, "終わった"), [1, 2, 3], "{all:#?}");
        let by_next = all
            .iter()
            .find(|l| l.contains("2 試合目が終わった"))
            .unwrap();
        assert!(
            by_next.contains("次の試合が始まるまで終わりを見なかった"),
            "{by_next}"
        );
        let by_link = all
            .iter()
            .find(|l| l.contains("3 試合目が終わった"))
            .unwrap();
        assert!(
            by_link.contains("接続が終わったため（自分から終えた）"),
            "{by_link}"
        );
        // 終わりの行は 2 度出ない。
        assert!(w.step(&snap(3, 5), 9).is_empty());
    }

    /// 壊れたら落ちる: 試合の最中に接続が替わった（数を見逃した）とき、前の接続の試合の終わりが行にならない。
    #[test]
    fn a_match_open_when_the_link_changes_is_closed_with_its_reason() {
        let mut w = MatchWatch::default();
        let on = |link: u32, segs: u32, phase: u32| NetSnapshot {
            link_started_ms: link,
            ..snap(segs, phase)
        };
        w.step(&on(1000, 1, 4), 1);
        let lines = w.step(&on(7000, 1, 1), 2);
        assert_eq!(numbers(&lines, "終わった"), [1], "{lines:?}");
        assert!(
            lines[0].contains("接続が替わるまで終わりを見なかった"),
            "{lines:?}"
        );
        assert_eq!(
            numbers(&lines, "始まった"),
            [1],
            "新しい接続の最初の試合: {lines:?}"
        );
    }

    /// 壊れたら落ちる: まだ試合の最中の段階を見ていない試合（読み込み中）を、段階が「試合の最中でない」ことだけで終わりにする。
    #[test]
    fn a_match_still_loading_is_not_ended() {
        let mut w = MatchWatch::default();
        let start = w.step(&snap(1, 3), 1);
        assert_eq!(numbers(&start, "始まった"), [1]);
        assert!(w.step(&snap(1, 3), 2).is_empty());
        assert!(w.step(&snap(1, 4), 3).is_empty());
        assert_eq!(numbers(&w.step(&snap(1, 1), 4), "終わった"), [1]);
    }
}
