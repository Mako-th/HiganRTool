//! rollback のモードのマッチの経路（pure。unsafe 無し）。`NetPlay` の Lockstep の phase を、delay 方式の `lockstep_tick`
//! （取得 → 待ち → 使う）の代わりに、走査の包み（`0x0042DCA9`）のフックから rollback の状態機械（`Rollback`）で進める。
//!
//! ## 呼ばれ方（`script_win.rs` が繋ぐ）
//!
//! - 走査の包みのフック（[`NetPlay::rollback_scan`]）: do_frame の走査の call の直前。受けと生存の手入れ → 自分の入力ワードの取得と送信 →
//!   入力の地点（止まっていれば書き戻しと進め直しもここ）→ 更新 1 回か描画だけのフレーム（どちらもフックが走査を呼ぶ）。
//!   [`RbScan::Ran`] ならフックが走査を呼び終えたので `ret` を do_frame へ返し、[`RbScan::Pass`] なら走査を普段どおり 1 回呼ぶ。
//! - Present の後（[`NetPlay::rollback_post_present`]）: コピーする / 届いた語で F を進める。先に受けを 1 回回す（進め直しが 1 フレーム遅れないように）。
//! - 出るたびに、確定した (frame, p1, p2) だけを観戦・記録・CHECK へ流す（[`NetPlay::rb_drain`]）。予測の入力ワードはここを通らない。
//!
//! ## 場面の出入り
//!
//! 場面の境目が確定するとフックは `Pass` を返し、走査の中の `on_input` → `lockstep_tick` が再戦・退出の要求を見て `leave_seg`
//! （`Connected::begin_seg` と `rb_epoch` を進める）。次の入口が `rb_epoch` の差を見て `PlayState::begin_seg` を呼ぶ。
//! `lockstep_tick` が rollback の接続で下まで来たら、`rb_abort` でマッチをやめる。
//!
//! ## 遅延
//!
//! delay 方式の実行では [`NetPlay::rollback_ready`] が false で、フックは何も触らない。rollback のモードで入力 → Present の間に
//! 足すのは、走査の包み 1 段と受けの手入れだけ。止まっている間は delay 方式と同じ予算つきの待ちをフックが行う（`Machine::wait`）。
#![forbid(unsafe_code)]

use higanr_core::rollback::{After, GlideParams, SYNC_HEAD_FRAMES, Step, Wait, Window};

use super::*;
use crate::pacing::glide;
use crate::rollback_play::{self, Machine, PlayMem, PlayState, PostFrame, ScanFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RbScan {
    /// フックが走査を呼び終えた（更新 1 回か、描画だけ）。`ret` を do_frame へ返す。
    Ran { ret: i32 },
    /// フックは走査を呼んでいない。走査を普段どおり 1 回呼ぶ（delay 方式の実行・場面の境目・マッチをやめた後・ネットワークの外）。
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RbAbort {
    CoreFault,
    ScanBroken,
    PostBroken,
    /// フックが走査を呼ばなかった tick に、走査の中の入力の地点が今までの経路（`lockstep_tick`）まで来た。
    Stray,
}

impl RbAbort {
    pub fn text(self) -> &'static str {
        match self {
            RbAbort::CoreFault => "状態機械の内部の食い違い",
            RbAbort::ScanBroken => "走査の包みのフックで窓の食い違い",
            RbAbort::PostBroken => "Present の後のフックで窓の食い違い",
            RbAbort::Stray => "フックが走査を呼ばなかった tick に入力の地点が今までの経路へ来た",
        }
    }
}

struct NpMachine<'a, G: Game, N: Net> {
    np: &'a mut NetPlay,
    game: &'a mut G,
    net: &'a mut N,
    wait_start: Option<u64>,
    timed_out: bool,
}

impl<G: Game, N: Net> Machine for NpMachine<'_, G, N> {
    fn mode(&self) -> Mode {
        self.np.link.connected().map_or(Mode::Delay, |c| c.mode())
    }

    /// 穴埋めの判定の D（seg の始まりの D。`Rollback::seg_delay`）。今の D で読むと seg の始まりの語を 0 と読む
    /// （`rollback.rs` のモジュール doc の「唯一の本物の危険」）。
    fn delay(&self) -> u8 {
        self.np
            .link
            .connected()
            .and_then(|c| c.rollback())
            .map_or(0, |r| r.seg_delay())
    }

    fn remote_next(&self) -> u32 {
        self.np
            .link
            .connected()
            .and_then(|c| c.rollback())
            .map_or(0, |r| r.remote_next())
    }

    fn scene_ended(&self) -> bool {
        self.np
            .link
            .connected()
            .and_then(|c| c.rollback())
            .is_some_and(|r| r.scene_ended())
    }

    fn captured(&self) -> bool {
        self.np.link.connected().is_none_or(|c| c.captured())
    }

    fn capture(&mut self) -> Result<(), CaptureError> {
        let now = ms(self.net.now_us());
        // 数字キー（0〜8 がその値・9 が auto）: 取得の前に 1 回読み、押した瞬間をネットワークの側へ渡す。
        self.np.digit_tick(&mut *self.game);
        let np = &mut *self.np;
        let Some(c) = np.link.connected_mut() else {
            return Err(CaptureError::Full);
        };
        if c.captured() {
            return Err(CaptureError::AlreadyCaptured);
        }
        // ゲストは、この seg の D をホストの INPUT で知るまで取らない。自分の入力も読まない（読んで持ち越すと数フレーム前の入力を
        // 使うことになる。台本も進めない）。語 0 個の INPUT を送り直して待つ。
        if !c.delay_known() {
            c.wait_delay(now, &mut *self.net);
            return Err(CaptureError::NoDelay);
        }
        // latency の切替の予定は、セッションが断る（`LatError::RollbackMode`）ので `lat_refused` に数えて飛ばす。
        NetPlay::lat_plan_step(c, &np.cfg, &mut np.lat_next, &mut np.stats);
        // 溢れて入らなかった前の語は読み直さない（台本を二重に進めない）。
        let local = match np.held_local {
            Some(w) => w,
            None => {
                np.stats.local_reads = np.stats.local_reads.saturating_add(1);
                self.game.local_input(c.seat()) & SEND_MASK
            }
        };
        // 初めの値をネットワークの側へ渡す（この取得の前に）。ホストは seg の途中の D の予定（`SWITCH_LEAD` フレーム先から）、
        // ゲストはホストへの要求（`Rollback::press_delay`）。
        NetPlay::deliver_pick(&mut np.pick, c, &mut np.stats);
        match c.capture(local, now, &mut *self.net) {
            Ok(()) => {
                np.held_local = None;
                Ok(())
            }
            Err(CaptureError::AlreadyCaptured) => {
                np.held_local = None;
                Err(CaptureError::AlreadyCaptured)
            }
            Err(e @ (CaptureError::Full | CaptureError::NoDelay)) => {
                np.held_local = Some(local);
                Err(e)
            }
        }
    }

    fn input_point<W: Window>(&mut self, w: &mut W) -> Step {
        let now = ms(self.net.now_us());
        match self
            .np
            .link
            .connected_mut()
            .and_then(|c| c.rollback_input_point(w, now, &mut *self.net))
        {
            Some(step) => step,
            None => Step::Wait(Wait::Fault),
        }
    }

    fn wait(&mut self, _why: Wait) -> bool {
        // 待つ理由かどうかは `rollback_play::retry_after_wait` が決めて、呼ぶ前に見ている。ここはネットワークの側の条件だけ。
        if self.np.phase != Phase::Lockstep {
            return false;
        }
        let now = self.net.now_us();
        let start = *self.wait_start.get_or_insert(now);
        let deadline = start.saturating_add(u64::from(self.np.cfg.wait_budget_us));
        if now >= deadline {
            self.timed_out = true;
            return false;
        }
        let left = (deadline - now).min(u64::from(lockstep::RESEND_MS) * 1_000);
        if self.net.wait(left as u32) {
            let over = self.net.now_us().saturating_sub(now).saturating_sub(left);
            self.np.count_wait_over(over);
        }
        self.np.stats.wakes = self.np.stats.wakes.saturating_add(1);
        self.np.pump(&mut *self.net);
        self.np.watch_end();
        self.np.phase == Phase::Lockstep
    }
}

impl NetPlay {
    /// マッチの tick を rollback の状態機械で動かす phase か（Lockstep で、決まったモードが rollback）。false ならフックは何も触らず `Pass` を返す。
    pub fn rollback_ready(&self) -> bool {
        self.phase == Phase::Lockstep
            && self
                .link
                .connected()
                .is_some_and(|c| c.rollback().is_some())
    }

    /// rollback の状態機械の数え。rollback の接続でなければ `None`。数えは接続の合計。
    pub fn rollback_core_stats(&self) -> Option<higanr_core::rollback::Stats> {
        self.link
            .connected()
            .and_then(|c| c.rollback())
            .map(|r| r.stats())
    }

    /// 前の入口の後に seg を出ていたら、コピーとアドレスの表を捨てる。
    fn rb_sync_seg(&mut self, st: &mut PlayState) {
        if !self.rb_bound {
            // この接続の最初の入口。`PlayState` は静的で前の接続の食い違いの印が残りうるので消す
            // （`rb_epoch` はこの `NetPlay` で 0 から数えるので差では見分けられない）。
            st.begin_conn();
            self.rb_bound = true;
            self.rb_seen = self.rb_epoch;
        } else if self.rb_seen != self.rb_epoch {
            st.begin_seg();
            self.rb_seen = self.rb_epoch;
        }
    }

    /// rollback を続けられないのでマッチをやめる（BYE(中止) を出し、以後は自分の入力で続く）。最初の 1 回の経路と、
    /// そのときまでに確定したフレームの数を控える。
    pub(super) fn rb_abort(&mut self, why: RbAbort) {
        self.stats.rb_aborts = self.stats.rb_aborts.saturating_add(1);
        if self.rb_abort_first.is_none() {
            self.rb_abort_first = Some((why, self.stats.played));
        }
        self.close(ByeReason::Cancelled);
    }

    pub fn rb_aborts(&self) -> u32 {
        self.stats.rb_aborts
    }

    #[cfg(test)]
    pub fn rb_glide_last(&self) -> i32 {
        self.rb_glide_last
    }

    /// 最初にマッチをやめた経路と、そのときまでに確定したフレームの数。
    pub fn rb_abort_first(&self) -> Option<(RbAbort, u64)> {
        self.rb_abort_first
    }

    /// 走査の包みのフック（`0x0042DCA9` の直前）。`game` はゲームのウィンドウ（自分の入力ワードの取得）、`sink` は確定した 1 フレームごとの記録の受け手。
    pub fn rollback_scan<G: Game, N: Net>(
        &mut self,
        game: &mut G,
        net: &mut N,
        st: &mut PlayState,
        mem: &mut dyn PlayMem,
        sink: &mut dyn FnMut(&FrameRec, &FrameTiming),
    ) -> RbScan {
        if !self.rollback_ready() {
            // delay 方式の実行・マッチの外・マッチをやめた後。確保の向け先が開いたままなら閉じる。
            st.set_redirect(mem, false);
            return RbScan::Pass;
        }
        self.rb_sync_seg(st);
        // フックが走査を呼ぶ tick は、走査の中の入力の地点が `on_input` を通らないので、その手入れをここでする。
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        self.pump(net);
        self.watch_end();
        self.sync_pong(net);
        if !self.rollback_ready() {
            st.set_redirect(mem, false);
            return RbScan::Pass;
        }
        let (out, timed_out, waited_us) = {
            let mut m = NpMachine {
                np: &mut *self,
                game,
                net: &mut *net,
                wait_start: None,
                timed_out: false,
            };
            let out = rollback_play::scan_point_with(&mut m, st, mem);
            let waited = m.wait_start.map_or(0, |s| m.net.now_us().saturating_sub(s));
            (out, m.timed_out, waited.min(u64::from(u32::MAX)) as u32)
        };
        self.rb_drain(net, st, waited_us, sink);
        // 時刻合わせの寄せ幅を、次の枠の始まり（ウィンドウの b-spin の格子）へ渡す。1 フレームに 1 回。全画面（ゲーム自身の時計）の区間では頭が捨てる。
        if let Some(c) = self.link.connected_mut() {
            let ppm = c.rollback_glide_ppm(&GlideParams::DEFAULT, glide().frame_us());
            let open = c.rollback().is_some_and(|r| r.glide_on());
            glide().request(ppm);
            glide().note_request(ppm, c.frame() >= SYNC_HEAD_FRAMES, open);
            self.rb_glide_last = ppm;
        }
        match out {
            ScanFrame::Ran { ret, step } => {
                match step {
                    Step::Wait(Wait::Fault) => {
                        st.set_redirect(mem, false);
                        self.rb_abort(RbAbort::CoreFault);
                    }
                    Step::Wait(_) => {
                        self.stats.stalls = self.stats.stalls.saturating_add(1);
                        self.stall_run = self.stall_run.saturating_add(1);
                        self.stats.max_stall_run = self.stats.max_stall_run.max(self.stall_run);
                        if timed_out && self.peer_finished {
                            self.finish(End::PeerBye(ByeReason::Finished));
                        }
                    }
                    _ => self.stall_run = 0,
                }
                RbScan::Ran { ret }
            }
            // 境目が確定した。走査を普段どおり呼ぶと `on_input` が再戦か退出を見て seg を出る。
            ScanFrame::SceneEnd => RbScan::Pass,
            ScanFrame::Broken(_) => {
                st.set_redirect(mem, false);
                self.rb_abort(RbAbort::ScanBroken);
                RbScan::Pass
            }
        }
    }

    /// Present の後。コピーする / 届いた語で F を進める。マッチを動かしていなければ `None`。
    pub fn rollback_post_present<N: Net>(
        &mut self,
        net: &mut N,
        st: &mut PlayState,
        mem: &mut dyn PlayMem,
        sink: &mut dyn FnMut(&FrameRec, &FrameTiming),
    ) -> Option<PostFrame> {
        if !self.rollback_ready() {
            return None;
        }
        // 描画の間に届いた語を先に受ける（受けないと進め直しが次のフレームの入口まで 1 フレーム遅れる）。
        self.pump(net);
        self.watch_end();
        if !self.rollback_ready() {
            st.set_redirect(mem, false);
            return None;
        }
        self.rb_sync_seg(st);
        let hint = self
            .link
            .connected()
            .and_then(|c| c.rollback())
            .map(|r| (r.remote_next(), r.seg_delay()))?;
        let c = self.link.connected_mut()?;
        let r = rollback_play::post_present_with(hint, st, mem, &mut |w| {
            c.rollback_post_present(w).unwrap_or(After::Nothing)
        });
        self.rb_drain(net, st, 0, sink);
        if let PostFrame::Broken(_) = r {
            st.set_redirect(mem, false);
            self.rb_abort(RbAbort::PostBroken);
        }
        Some(r)
    }

    /// 確定した (frame, p1, p2) を古い方から流す: 観戦のリングバッファ・CHECK・`sink`・数え。確定した行だけ（予測の入力ワードは `PlayState` の控えに入らない）。
    /// CHECK は delay 方式と同じ作り方: 確定した行を使った後のダイジェスト（`CheckSource::InputDigest`）か、
    /// 確定の tick の始まりで読んだゲームの値（`CheckSource::Game`）。`waited_us` は、この入口で相手の語を待った時間。
    fn rb_drain<N: Net>(
        &mut self,
        net: &mut N,
        st: &mut PlayState,
        waited_us: u32,
        sink: &mut dyn FnMut(&FrameRec, &FrameTiming),
    ) {
        let now_us = net.now_us();
        let NetPlay {
            link,
            cfg,
            digest,
            stats,
            spec,
            arrivals,
            timing,
            #[cfg(test)]
            sent_checks,
            ..
        } = &mut *self;
        let Some(c) = link.connected_mut() else {
            st.drain_confirmed(&mut |_, _, _| {});
            st.drain_checks(&mut |_| {});
            return;
        };
        let seg = c.seg();
        // 確定した行に添える D は流す時点の今の D（seg の途中で替えた直後の数行は 1 つ前の D の行にも新しい D が付きうる）。
        let delay = c.cur_delay();
        let source = cfg.check;
        let mut mismatch = None;
        st.drain_confirmed(&mut |frame, p1, p2| {
            if source == CheckSource::InputDigest {
                *digest = input_digest_push(*digest, p1, p2);
            }
            NetPlay::spec_to(spec, Record::Frame { frame, p1, p2 });
            stats.played = stats.played.saturating_add(1);
            let slot = arrivals
                .get(frame as usize % ARRIVAL_SLOTS)
                .copied()
                .unwrap_or(NO_ARRIVAL);
            let margin_us = (slot.seg == seg && slot.frame == frame)
                .then(|| now_us.saturating_sub(slot.t_us).min(u64::from(u32::MAX)) as u32);
            if margin_us.is_none() {
                stats.margin_unknown = stats.margin_unknown.saturating_add(1);
            }
            let t = FrameTiming {
                delay,
                margin_us,
                wait_us: waited_us,
                // rollback は待たない作りで、始まりの待ちもしない。
                head_wait_us: 0,
            };
            *timing = t;
            sink(&FrameRec { seg, frame, p1, p2 }, &t);
            if source == CheckSource::InputDigest && is_check_frame(frame) {
                let chk = check_from_input_digest(seg, frame, *digest);
                #[cfg(test)]
                sent_checks.push(chk);
                if let Some(CheckResult::Mismatch { local, remote }) = c.submit_check(chk, net) {
                    mismatch.get_or_insert((local, remote));
                }
            }
        });
        st.drain_checks(&mut |chk| {
            // 確定した行を使った後のダイジェストで作るときは、確定の tick の始まりの値は使わない。
            if source == CheckSource::Game {
                #[cfg(test)]
                sent_checks.push(*chk);
                if let Some(CheckResult::Mismatch { local, remote }) = c.submit_check(*chk, net) {
                    mismatch.get_or_insert((local, remote));
                }
            }
        });
        if let Some((local, remote)) = mismatch {
            self.desync(local, remote);
        }
    }
}
