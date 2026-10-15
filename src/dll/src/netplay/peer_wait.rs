//! 相手を待ち続けている間の知らせと切断（pure）。
//!
//! 入力の地点が tick ごとに「この tick は相手を待っていたか」を渡す。`NOTICE_TICKS` 続いたら画面に 1 行出し（`shown`）、その間に Esc の
//! 押した瞬間を見たら切る。待ちが `LIMIT_MS` 続いたら切る。待ちが解けたら数え直す。
//!
//! 何を「待っていた」とするかは呼び手が決める（`NetPlay::peer_wait_tick`）: マッチの描画だけのフレーム、止める地点で相手の SELECT を待つ
//! フレーム、選択の画面で相手のゲームスレッドが `QUIET_MS` 黙っている間。
//!
//! ゲームスレッドで毎 tick 走るので、確保しない・パニックしない。
#![forbid(unsafe_code)]

use higanr_core::liveness::{PING_INTERVAL_MS, SILENCE_LIMIT_MS};

/// 知らせを出すまでの、待ちが続いた tick の数（約 2 秒）。
pub const NOTICE_TICKS: u32 = 120;
/// 待ちが続いたら切るまで（ms）。自分のゲームスレッドが同じ長さ止まると相手の側でも切れる（`liveness::HEARTBEAT_LIMIT_MS`）ので、そろえた。
/// 相手が最小化してコードを渡す間も待つ長さ。
pub const LIMIT_MS: u32 = SILENCE_LIMIT_MS;
/// 選択の画面で、相手のゲームスレッドが黙ったとみなす長さ（ms）。相手は 1 秒ごとに PING を送るので、その間隔より長くした。PING が 1 通
/// 落ちても黙ったとみなすのは 0.5 秒ほどで、知らせ（`NOTICE_TICKS`）までは続かない。
pub const QUIET_MS: u32 = 1_500;
const _: () = assert!(QUIET_MS > PING_INTERVAL_MS);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// 知らせを出している間に Esc が押された。
    Esc,
    /// 待ちが `LIMIT_MS` 続いた。
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerWait {
    run: u32,
    since_ms: u32,
    /// 知らせを出してから Esc が離れているのを 1 度見たか（押したまま待ちに入った瞬間に切らないため）。
    esc_armed: bool,
}

impl PeerWait {
    /// 1 tick。`blocked` はこの tick に相手を待っていたなら `Some(待ちの始まりの時刻)`（始まりは待ちの最初の tick のものだけを使う）。
    /// `esc` は Esc が押されているかの読み。この tick に知らせを出しているときだけ呼ぶ。
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
        } else if self.esc_armed {
            return Verdict::Esc;
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
        assert_eq!(w.step(Some(T0), at + FRAME_MS, || true), Verdict::Esc);
        // 知らせが出た tick に離れていれば、次の押下で切る。
        let mut w = PeerWait::default();
        waited(&mut w, NOTICE_TICKS - 1, true);
        let at = T0 + NOTICE_TICKS * FRAME_MS;
        assert_eq!(w.step(Some(T0), at, || false), Verdict::Keep);
        assert_eq!(w.step(Some(T0), at + FRAME_MS, || true), Verdict::Esc);
    }

    #[test]
    fn the_wait_times_out_at_the_limit_from_its_first_start() {
        assert_eq!(LIMIT_MS, 60_000);
        let mut w = PeerWait::default();
        assert_eq!(w.step(Some(T0), T0, || false), Verdict::Keep);
        assert_eq!(w.step(Some(T0), T0 + 59_999, || false), Verdict::Keep);
        assert_eq!(w.step(Some(T0), T0 + 60_000, || false), Verdict::TimedOut);
        let mut w = PeerWait::default();
        // 始まりは最初の tick のもの（後の tick の値は使わない）。
        assert_eq!(w.step(Some(T0 - 1_000), T0, || false), Verdict::Keep);
        assert_eq!(
            w.step(Some(T0), T0 + LIMIT_MS - 1_001, || false),
            Verdict::Keep
        );
        assert_eq!(
            w.step(Some(T0 + 5_000), T0 + LIMIT_MS - 1_000, || false),
            Verdict::TimedOut
        );
        // 1 tick でも解ければ始まりから数え直す。
        let mut w = PeerWait::default();
        w.step(Some(T0), T0, || false);
        w.step(None, T0 + LIMIT_MS - 1, || false);
        assert_eq!(
            w.step(Some(T0 + LIMIT_MS), T0 + LIMIT_MS, || false),
            Verdict::Keep
        );
    }
}
