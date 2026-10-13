//! 対戦の成立（READY。type 20）の状態機械（pure。確保しない）。
//!
//! 成立は「両者が『妖怪 対 妖怪』を選んだ」とき。成立の前は lockstep・選択の同期（MENU / STEP_REQ / STEP / SELECT）・CHECK に
//! 入らない（待っている間に別のモードで遊べる）。繋がってから `MATCH_WAIT_MS` の間に成立しなければ閉じる。
//! `code_side = game` の実行だけが使う（`link::Link::set_match_gate`）。それ以外（`offer` / `answer`・観戦・peer_sim の既定）は
//! 繋がったときに成立したことになり、READY を 1 通も送らない。
//!
//! ## 決まり
//!
//! - 単調: 自分の「選んだ」も相手の「選んだ」も一度立ったら戻らない。取り消したいときは「抜ける」（BYE）だけ。
//!   戻せると「片方が取り消した READY が遅れて届き、もう片方だけが成立する」筋を閉じる決まりが要る。
//! - 送り直し: 自分が選んでいる間、`READY_RESEND_MS` ごとに READY を送る（選んだ瞬間に 1 通目）。成立した後も、相手から
//!   成立後の通信（MENU・STEP_REQ・STEP・SELECT・INPUT・CHECK）を 1 通受けるまでは送り続ける（相手が自分の READY を落としたとき、
//!   相手だけが成立しないまま残らないため）。受け側は重複の READY を数えるだけで何も変えない。
//! - 時間切れ: 繋がった時刻から `MATCH_WAIT_MS` 経っても成立していなければ一度だけ「切れた」と言う（呼び手が BYE(取消) で閉じる。
//!   `link::Connected::poll`）。成立した後は切れない。
//! - ゲートの無い相手: 相手から成立後の通信を受けたのに相手の READY を 1 通も受けていない状態が `NO_GATE_MS` 続いたら、一度だけ
//!   「相手にゲートが無い」と言う（閉じ方は時間切れと同じ）。ゲートの無い相手 = 古いバージョンの `code_side = game` か、同じバージョンの `offer` / `answer`。
//!   READY を送らずこちらの READY は捨てるので、待ち続けると相手だけが難易度の画面で止まる。ゲートのある相手は READY の 1 通目を
//!   成立後の通信より先に出すので、1 通目が落ちても `NO_GATE_MS` の間に送り直しが 2 回ある。
//!
//! 時刻は u32 の ms で、差は `wrapping_sub` で取る（`liveness.rs` と同じ）。

use crate::liveness::PING_INTERVAL_MS;

/// 繋がってから成立を待つ長さ（ms）。10 分。画面の文（`net_menu`）もここから作る。
pub const MATCH_WAIT_MS: u32 = 10 * 60 * 1000;

/// READY を送り直す間隔（ms）。生存の PING と同じ間隔（`liveness::PING_INTERVAL_MS`）。
pub const READY_RESEND_MS: u32 = PING_INTERVAL_MS;

/// 相手から成立後の通信が届いてから、相手の READY を待つ長さ（ms）。過ぎても READY が 1 通も届かなければ、相手にゲートが無いと見て閉じる。
/// 送り直しの間隔の 3 倍: ゲートのある相手なら、1 通目とその後の 2 回の送り直しが続けて落ちない限り届く。
pub const NO_GATE_MS: u32 = 3 * READY_RESEND_MS;

/// 成立しないまま閉じた理由。`Gate::check_close` が返すのは `TimedOut` と `PeerHasNoGate` だけ。`PeerLeft` は呼び手（dll）が
/// 成立の前に相手の BYE(取消) を受けたときに使う（同じ画面の行と終わりの種類に載せるため、ここに置く）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoMatch {
    TimedOut,
    PeerHasNoGate,
    PeerLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateCounts {
    pub ready_sent: u32,
    pub ready_received: u32,
    pub ready_dup: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    /// 繋がった時刻（時間切れの起点）。
    since: u32,
    local: bool,
    peer: bool,
    peer_post: bool,
    /// 相手の成立後の通信を最初に受けた時刻（ゲートの無い相手を見分ける起点）。
    post_since: Option<u32>,
    last_sent: Option<u32>,
    closed: Option<NoMatch>,
    counts: GateCounts,
}

impl Gate {
    pub fn new(now: u32) -> Gate {
        Gate {
            since: now,
            local: false,
            peer: false,
            peer_post: false,
            post_since: None,
            last_sent: None,
            closed: None,
            counts: GateCounts::default(),
        }
    }

    /// 自分が選んだ。初めてなら true（呼び手はすぐ `poll_send` を呼んで 1 通目を出す）。閉じた後は立てない。
    pub fn set_local(&mut self) -> bool {
        if self.local || self.closed.is_some() {
            return false;
        }
        self.local = true;
        true
    }

    pub fn on_ready(&mut self) {
        self.counts.ready_received = self.counts.ready_received.saturating_add(1);
        if self.peer {
            self.counts.ready_dup = self.counts.ready_dup.saturating_add(1);
        }
        self.peer = true;
    }

    /// 相手から成立後の通信が時刻 `now` に届いた。相手はもう成立しているので送り直しをやめる。相手の「選んだ」は READY でだけ立てる
    /// （この印で立てるとゲートの無い相手と成立してしまう）。最初の 1 通の時刻を、ゲートの無い相手を見分ける起点に覚える。
    pub fn on_post(&mut self, now: u32) {
        self.peer_post = true;
        if self.post_since.is_none() {
            self.post_since = Some(now);
        }
    }

    pub fn local(&self) -> bool {
        self.local
    }

    pub fn peer(&self) -> bool {
        self.peer
    }

    pub fn matched(&self) -> bool {
        self.local && self.peer
    }

    pub fn expired(&self) -> bool {
        self.closed == Some(NoMatch::TimedOut)
    }

    pub fn closed(&self) -> Option<NoMatch> {
        self.closed
    }

    /// 今 READY を送るか（送るなら送ったことにして true）。自分が選んでいて、相手の成立後の通信をまだ受けておらず、前に送ってから
    /// `READY_RESEND_MS` 経った（1 度も送っていなければすぐ）とき。閉じた後は送らない。
    pub fn poll_send(&mut self, now: u32) -> bool {
        if !self.local || self.peer_post || self.closed.is_some() {
            return false;
        }
        let due = self
            .last_sent
            .is_none_or(|t| now.wrapping_sub(t) >= READY_RESEND_MS);
        if due {
            self.last_sent = Some(now);
            self.counts.ready_sent = self.counts.ready_sent.saturating_add(1);
        }
        due
    }

    /// 閉じるかを見る。相手にゲートが無いと分かった（相手の成立後の通信から `NO_GATE_MS` の間 READY が来ない）か `MATCH_WAIT_MS` 経った、
    /// 最初の 1 回だけ理由を返す。両方が同じ時刻に当たったら、具体的なゲートの無い方を採る。
    pub fn check_close(&mut self, now: u32) -> Option<NoMatch> {
        if self.closed.is_some() || self.matched() {
            return None;
        }
        let no_gate = !self.peer
            && self
                .post_since
                .is_some_and(|t| now.wrapping_sub(t) >= NO_GATE_MS);
        let why = if no_gate {
            NoMatch::PeerHasNoGate
        } else if now.wrapping_sub(self.since) >= MATCH_WAIT_MS {
            NoMatch::TimedOut
        } else {
            return None;
        };
        self.closed = Some(why);
        Some(why)
    }

    pub fn left_ms(&self, now: u32) -> u32 {
        if self.matched() || self.closed.is_some() {
            return 0;
        }
        MATCH_WAIT_MS.saturating_sub(now.wrapping_sub(self.since))
    }

    pub fn counts(&self) -> GateCounts {
        self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_both_choosing_matches() {
        for (l, p, want) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let mut g = Gate::new(100);
            if p {
                g.on_ready();
            }
            if l {
                g.set_local();
            }
            assert_eq!(g.matched(), want, "local {l} peer {p}");
            let mut g = Gate::new(100);
            if l {
                g.set_local();
            }
            if p {
                g.on_ready();
            }
            assert_eq!(g.matched(), want, "（逆の順）local {l} peer {p}");
        }
    }

    #[test]
    fn the_flags_are_monotonic_and_duplicates_change_nothing() {
        let mut g = Gate::new(0);
        assert!(g.set_local());
        assert!(!g.set_local(), "2 回目は初めてでない");
        assert!(g.local());
        g.on_ready();
        g.on_ready();
        g.on_ready();
        assert!(g.peer() && g.matched());
        assert_eq!(g.counts().ready_received, 3);
        assert_eq!(g.counts().ready_dup, 2);
    }

    #[test]
    fn ready_is_sent_at_once_then_resent_until_the_peer_moves_on() {
        let mut g = Gate::new(0);
        for t in [0, 500, 5_000] {
            assert!(!g.poll_send(t), "選ぶ前は送らない（{t} ms）");
        }
        g.set_local();
        assert!(g.poll_send(7_000), "選んだ瞬間に 1 通目");
        assert!(!g.poll_send(7_000));
        assert!(!g.poll_send(7_000 + READY_RESEND_MS - 1), "間隔の手前");
        assert!(
            g.poll_send(7_000 + READY_RESEND_MS),
            "間隔ちょうどで送り直す"
        );
        g.on_ready();
        assert!(g.matched());
        assert!(g.poll_send(7_000 + 2 * READY_RESEND_MS));
        g.on_post(7_000 + 2 * READY_RESEND_MS);
        assert!(!g.poll_send(7_000 + 3 * READY_RESEND_MS));
        assert!(!g.poll_send(7_000 + 30 * READY_RESEND_MS));
        assert_eq!(g.counts().ready_sent, 3);
    }

    #[test]
    fn the_wait_expires_once_at_ten_minutes_unless_matched() {
        assert_eq!(MATCH_WAIT_MS, 600_000, "10 分（メインが決めた値）");
        for since in [5u32, u32::MAX - 100_000] {
            let mut g = Gate::new(since);
            g.set_local();
            assert_eq!(g.check_close(since.wrapping_add(MATCH_WAIT_MS - 1)), None);
            assert_eq!(g.left_ms(since.wrapping_add(MATCH_WAIT_MS - 1)), 1);
            assert_eq!(
                g.check_close(since.wrapping_add(MATCH_WAIT_MS)),
                Some(NoMatch::TimedOut),
                "since {since}"
            );
            assert_eq!(
                g.check_close(since.wrapping_add(MATCH_WAIT_MS + 1)),
                None,
                "2 回目は言わない"
            );
            assert!(g.expired());
            assert_eq!(g.closed(), Some(NoMatch::TimedOut));
            assert!(!g.poll_send(since.wrapping_add(MATCH_WAIT_MS + 5_000)));
            g.on_ready();
            assert!(!g.set_local());
            assert_eq!(g.left_ms(since.wrapping_add(MATCH_WAIT_MS)), 0);
        }
        let mut g = Gate::new(0);
        g.set_local();
        g.on_ready();
        assert_eq!(g.check_close(MATCH_WAIT_MS * 3), None);
        assert!(!g.expired());
        let mut g = Gate::new(0);
        assert_eq!(g.left_ms(60_000), MATCH_WAIT_MS - 60_000);
        assert_eq!(g.check_close(MATCH_WAIT_MS), Some(NoMatch::TimedOut));
    }

    #[test]
    fn a_peer_sending_post_match_traffic_without_ready_is_closed_after_three_seconds() {
        assert_eq!(NO_GATE_MS, 3_000, "送り直しの 3 倍（メインが決めた目安）");
        for (since, local) in [(5u32, true), (u32::MAX - 1_000, false)] {
            let mut g = Gate::new(since);
            if local {
                g.set_local();
                assert!(g.poll_send(since));
            }
            let first = since.wrapping_add(40_000);
            g.on_post(first);
            for k in 1..NO_GATE_MS / 16 {
                g.on_post(first.wrapping_add(k * 16));
            }
            assert_eq!(g.check_close(first.wrapping_add(NO_GATE_MS - 1)), None);
            assert!(g.left_ms(first.wrapping_add(NO_GATE_MS - 1)) > 0);
            assert_eq!(
                g.check_close(first.wrapping_add(NO_GATE_MS)),
                Some(NoMatch::PeerHasNoGate),
                "since {since} local {local}"
            );
            assert_eq!(g.check_close(first.wrapping_add(NO_GATE_MS + 1)), None);
            assert_eq!(g.closed(), Some(NoMatch::PeerHasNoGate));
            assert!(!g.expired(), "時間切れと取り違えた");
            assert!(!g.matched());
            assert!(!g.poll_send(first.wrapping_add(NO_GATE_MS + 5_000)));
            assert_eq!(g.left_ms(first.wrapping_add(NO_GATE_MS)), 0);
            if !local {
                assert!(!g.set_local(), "閉じた後は選べない");
            }
        }
    }

    #[test]
    fn a_peer_whose_ready_arrives_late_or_was_seen_is_not_closed_as_gateless() {
        let mut g = Gate::new(0);
        g.set_local();
        g.on_post(10_000);
        assert_eq!(g.check_close(10_000 + 2 * READY_RESEND_MS - 1), None);
        g.on_ready();
        assert!(g.matched());
        assert_eq!(g.check_close(10_000 + 10 * NO_GATE_MS), None);
        assert_eq!(g.closed(), None);
        // READY を 1 通でも受けていれば、成立後の通信が来てもゲートの無い相手とは見ない。
        let mut g = Gate::new(0);
        g.on_ready();
        g.on_post(1_000);
        assert_eq!(g.check_close(1_000 + 10 * NO_GATE_MS), None);
        assert!(!g.matched());
        let mut g = Gate::new(0);
        g.set_local();
        assert_eq!(g.check_close(NO_GATE_MS * 2), None);
    }
}
