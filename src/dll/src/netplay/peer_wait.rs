//! 相手を待ち続けている間の知らせと切断（pure）。
//!
//! 入力の地点がtickごとに「このtickは相手を待っていたか」を`PeerWait::step`へ渡す。
//! `NOTICE_TICKS`続いたら画面に1行出し（`shown`）、その間にEscが`ESC_HOLD_MS`押され続けたら切る（一時停止のつもりの1回の押下では切らない）。
//! 待ちが`LIMIT_MS`続いても切る。
//! 何を「待っていた」とするかは呼び手が決める（`NetPlay::peer_wait_tick`）。
//! ゲームスレッドで毎tick走るので確保しない・パニックしない。
#![forbid(unsafe_code)]

use higanr_core::liveness::{PING_INTERVAL_MS, SILENCE_LIMIT_MS};

/// 待ちが何tick続いたら知らせを出すか（約2秒）。
pub const NOTICE_TICKS: u32 = 120;
/// 待ちが続いたら切るまで（ms）。自分のゲームスレッドが同じ長さ止まると相手の側でも切れる（`liveness::HEARTBEAT_LIMIT_MS`）ので、そろえた。
/// 相手が最小化してコードを渡す間も待つ長さ。
pub const LIMIT_MS: u32 = SILENCE_LIMIT_MS;
/// 選択の画面で相手のゲームスレッドが黙ったとみなす長さ（ms）。
/// 相手は1秒ごとにPINGを送るので、その間隔より長くした。
/// PINGが1通失われても黙ったとみなすのは0.5秒ほど。知らせ（`NOTICE_TICKS`）までは続かない。
pub const QUIET_MS: u32 = 1_500;
const _: () = assert!(QUIET_MS > PING_INTERVAL_MS);
/// 知らせを出している間にEscをこの長さ（ms）押し続けると切る。画面の文（`net_menu::T_PEER_WAIT`）の「3秒」と同じ。
pub const ESC_HOLD_MS: u32 = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// 知らせを出している間にEscが`ESC_HOLD_MS`押され続けた。
    Esc,
    /// 待ちが`LIMIT_MS`続いた。
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerWait {
    run: u32,
    since_ms: u32,
    /// 知らせを出してからEscが離れているのを1度見たかどうか（押したまま待ちに入った瞬間に切らないため）。
    esc_armed: bool,
    /// 離れているのを見た後にEscが押され始めた時刻。離れたらNone。
    esc_since: Option<u32>,
}

impl PeerWait {
    /// 1 tick。`blocked`はこのtickに相手を待っていたなら`Some(待ちの始まりの時刻)`（始まりは待ちの最初のtickのものだけを使う）。
    /// `esc`はEscが押されているかどうかの読み。このtickに知らせを出しているときだけ呼ぶ。
    pub fn step(
        &mut self,
        blocked: Option<u32>,
        now_ms: u32,
        esc: impl FnOnce() -> bool,
    ) -> Verdict {
        let Some(start) = blocked else {
            *self = PeerWait::default();
            return Verdict::Keep;
        };
        if self.run == 0 {
            self.since_ms = start;
        }
        self.run = self.run.saturating_add(1);
        if now_ms.wrapping_sub(self.since_ms) >= LIMIT_MS {
            return Verdict::TimedOut;
        }
        if !self.shown() {
            return Verdict::Keep;
        }
        if !esc() {
            self.esc_armed = true;
            self.esc_since = None;
        } else if self.esc_armed {
            let since = *self.esc_since.get_or_insert(now_ms);
            if now_ms.wrapping_sub(since) >= ESC_HOLD_MS {
                return Verdict::Esc;
            }
        }
        Verdict::Keep
    }

    pub fn shown(&self) -> bool {
        self.run >= NOTICE_TICKS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u32 = 50_000;
    const FRAME_MS: u32 = 16;

    fn waited(w: &mut PeerWait, ticks: u32, esc: bool) -> Vec<Verdict> {
        (0..ticks)
            .map(|i| w.step(Some(T0), T0 + i * FRAME_MS, || esc))
            .collect()
    }

    #[test]
    fn the_notice_shows_after_the_notice_ticks_and_clears_when_the_wait_ends() {
        let mut w = PeerWait::default();
        waited(&mut w, NOTICE_TICKS - 1, false);
        assert!(!w.shown(), "早く出た");
        w.step(Some(T0), T0 + NOTICE_TICKS * FRAME_MS, || false);
        assert!(w.shown(), "{NOTICE_TICKS} tick で出ない");
        assert_eq!(w.step(None, T0 + 3_000, || false), Verdict::Keep);
        assert!(!w.shown(), "待ちが解けても残った");
        waited(&mut w, NOTICE_TICKS - 1, false);
        assert!(!w.shown(), "前の待ちの数えが残った");
    }

    #[test]
    fn esc_is_read_only_while_shown_and_cuts_only_after_it_was_seen_up() {
        let mut w = PeerWait::default();
        let mut reads = 0;
        for i in 0..NOTICE_TICKS + 30 {
            let v = w.step(Some(T0), T0 + i * FRAME_MS, || {
                reads += 1;
                true
            });
            assert_eq!(
                v,
                Verdict::Keep,
                "押したまま待ちに入っただけで切った（{i}）"
            );
        }
        assert_eq!(reads, 31, "知らせを出していない tick に Esc を読んだ");
        let at = T0 + (NOTICE_TICKS + 30) * FRAME_MS;
        assert_eq!(w.step(Some(T0), at, || false), Verdict::Keep);
        let pressed = at + FRAME_MS;
        assert_eq!(w.step(Some(T0), pressed, || true), Verdict::Keep);
        assert_eq!(
            w.step(Some(T0), pressed + ESC_HOLD_MS, || true),
            Verdict::Esc
        );
        // 知らせが出たtickに離れていればそこから押し続けて切る。
        let mut w = PeerWait::default();
        waited(&mut w, NOTICE_TICKS - 1, true);
        let at = T0 + NOTICE_TICKS * FRAME_MS;
        assert_eq!(w.step(Some(T0), at, || false), Verdict::Keep);
        assert_eq!(w.step(Some(T0), at + FRAME_MS, || true), Verdict::Keep);
        assert_eq!(
            w.step(Some(T0), at + FRAME_MS + ESC_HOLD_MS, || true),
            Verdict::Esc
        );
    }

    #[test]
    fn a_short_esc_press_does_not_cut_and_releasing_starts_the_hold_over() {
        let mut w = PeerWait::default();
        waited(&mut w, NOTICE_TICKS, false);
        let start = T0 + NOTICE_TICKS * FRAME_MS;
        // 押し続けて、3秒の1 tick手前までは切らない。
        let mut t = start;
        while t < start + ESC_HOLD_MS {
            assert_eq!(
                w.step(Some(T0), t, || true),
                Verdict::Keep,
                "{} ms で切った",
                t - start
            );
            t += FRAME_MS;
        }
        // 1 tick離すと数え直し。続けて押してもそこから3秒たつまで切らない。
        assert_eq!(w.step(Some(T0), t, || false), Verdict::Keep);
        let again = t + FRAME_MS;
        assert_eq!(w.step(Some(T0), again, || true), Verdict::Keep);
        assert_eq!(
            w.step(Some(T0), again + ESC_HOLD_MS - 1, || true),
            Verdict::Keep,
            "離した前の押下の長さが残った"
        );
        assert_eq!(w.step(Some(T0), again + ESC_HOLD_MS, || true), Verdict::Esc);
    }

    #[test]
    fn the_wait_times_out_at_the_limit_from_its_first_start() {
        assert_eq!(LIMIT_MS, 60_000);
        let mut w = PeerWait::default();
        assert_eq!(w.step(Some(T0), T0, || false), Verdict::Keep);
        assert_eq!(w.step(Some(T0), T0 + 59_999, || false), Verdict::Keep);
        assert_eq!(w.step(Some(T0), T0 + 60_000, || false), Verdict::TimedOut);
        let mut w = PeerWait::default();
        // 始まりは最初のtickのもの（後のtickの値は使わない）。
        assert_eq!(w.step(Some(T0 - 1_000), T0, || false), Verdict::Keep);
        assert_eq!(
            w.step(Some(T0), T0 + LIMIT_MS - 1_001, || false),
            Verdict::Keep
        );
        assert_eq!(
            w.step(Some(T0 + 5_000), T0 + LIMIT_MS - 1_000, || false),
            Verdict::TimedOut
        );
        // 1 tickでも解ければ始まりから数え直す。
        let mut w = PeerWait::default();
        w.step(Some(T0), T0, || false);
        w.step(None, T0 + LIMIT_MS - 1, || false);
        assert_eq!(
            w.step(Some(T0 + LIMIT_MS), T0 + LIMIT_MS, || false),
            Verdict::Keep
        );
    }
}
