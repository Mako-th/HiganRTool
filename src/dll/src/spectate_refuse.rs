//! 観戦を断る配る側: 2 人のどちらかが観戦を許可していないルームマッチのホストが、観戦用のルーム（`RoomName::spectate_of`）に役 host で入り、
//! `offer` の代わりに `refuse` を送ってルームの期限まで残る。入ってくる観戦者はサーバーが 4403 で閉じるので、観戦者はすぐ断りの文になる。
//! 入り直しの判断は配る側と同じ `Caster`（4408 の後すぐ入り直して、また断る）。見張り（`net_code_game::GameWatcher`）が接続したときに
//! `spawn` し、接続の後の見回りを終えるときに手放す（`Drop` で止めて待つ）。ゲームのスレッドは何も呼ばない。ルーム名は本ログに書かない。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::room::{Event, Failure, RoomName, Why};
use higanr_core::spectate_room::{Caster, CasterAction};

use crate::net_room::{RoomConn, RoomOptions, SubmitRefused};

/// 見回りの間隔（ms）。配る側（`spectate_offer`）と同じ 50 ms。
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefuseCounts {
    /// 役 host で入った回数。
    pub entered: u32,
    /// `refuse` を送れた回数。
    pub refused: u32,
    /// 役 guest を受けた（観戦者が先にいた）ので閉じた回数。
    pub wrong_role: u32,
    /// ルームの手順の失敗（ルームの期限 4408 を含む）。
    pub failed: u32,
    pub connect_errors: u32,
    pub panicked: u32,
}

impl RefuseCounts {
    pub fn text(&self) -> String {
        format!(
            "観戦拒否（観戦用のルームで観戦者を拒否）: 入った {}（役が逆で閉じた {}）・拒否を送った {}・ルームの失敗 {}・接続不能 {}{}",
            self.entered,
            self.wrong_role,
            self.refused,
            self.failed,
            self.connect_errors,
            if self.panicked > 0 {
                "・パニックで停止（対戦は続く）"
            } else {
                ""
            },
        )
    }
}

#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    entered: AtomicU32,
    refused: AtomicU32,
    wrong_role: AtomicU32,
    failed: AtomicU32,
    connect_errors: AtomicU32,
    panicked: AtomicU32,
}

fn bump(a: &AtomicU32) {
    a.fetch_add(1, Ordering::Relaxed);
}

impl Shared {
    fn counts(&self) -> RefuseCounts {
        let g = |a: &AtomicU32| a.load(Ordering::Relaxed);
        RefuseCounts {
            entered: g(&self.entered),
            refused: g(&self.refused),
            wrong_role: g(&self.wrong_role),
            failed: g(&self.failed),
            connect_errors: g(&self.connect_errors),
            panicked: g(&self.panicked),
        }
    }
}

/// 観戦用のルームの 1 本ぶん。`step` は時刻を渡す形（検査は時計を進めて直に呼ぶ）。
pub(crate) struct Runner {
    room: RoomOptions,
    /// 観戦用のルームの名（`Debug` に出さない）。
    name: RoomName,
    caster: Caster,
    conn: Option<RoomConn>,
    shared: Arc<Shared>,
}

impl Runner {
    fn new(room: RoomOptions, name: RoomName, shared: Arc<Shared>) -> Runner {
        Runner {
            room,
            name,
            caster: Caster::new(0),
            conn: None,
            shared,
        }
    }

    pub(crate) fn step(&mut self, now_ms: u64) {
        if let Some(CasterAction::Connect) = self.caster.poll(now_ms) {
            match RoomConn::start(&self.room, &self.name) {
                Ok(c) => self.conn = Some(c),
                Err(_) => {
                    bump(&self.shared.connect_errors);
                    self.caster
                        .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
                }
            }
        }
        self.drain(now_ms);
        // `refuse` の送りが失敗すると、その失敗の出来事は次の `poll` で返る。捨てる前に 1 回読まないと `Caster` が
        // 入ったままになり、二度と入り直さない。
        if self.conn.as_ref().is_some_and(RoomConn::is_finished) {
            self.drain(now_ms);
            self.conn = None;
        }
    }

    fn drain(&mut self, now_ms: u64) {
        let evs = match self.conn.as_mut() {
            Some(c) => c.poll(),
            None => return,
        };
        for e in evs {
            if self.conn.is_none() {
                break;
            }
            self.on_event(e, now_ms);
        }
    }

    fn on_event(&mut self, e: Event, now_ms: u64) {
        match e {
            // `Caster` の「offer せよ」を、ここでは「断れ」と読む。
            Event::Joined { role, .. } => match self.caster.on_joined(now_ms, role) {
                Some(CasterAction::MakeOffer) => {
                    bump(&self.shared.entered);
                    let Some(conn) = self.conn.as_mut() else {
                        return;
                    };
                    match conn.refuse() {
                        Ok(()) => bump(&self.shared.refused),
                        // 送れなかった失敗は `step` の末尾の `drain` で届き、そこで入り直しの待ちに入る。
                        Err(SubmitRefused::NotSent) => {}
                        Err(SubmitRefused::Core(_)) => {
                            self.caster
                                .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
                            self.conn = None;
                        }
                    }
                }
                Some(CasterAction::Close) => {
                    bump(&self.shared.wrong_role);
                    self.conn = None;
                }
                _ => {}
            },
            // 断った後は来ない筋（サーバーは観戦を断ったルームのホストにコードを渡さない）。来たら閉じて入り直す。
            Event::Matched { .. } | Event::PeerCode(_) | Event::Done => {
                self.caster.on_done(now_ms);
                self.conn = None;
            }
            Event::Failed(f) => {
                bump(&self.shared.failed);
                self.caster.on_failed(now_ms, f);
                self.conn = None;
            }
        }
    }

    fn close(&mut self) {
        self.caster.stop();
        if let Some(mut c) = self.conn.take() {
            c.cancel();
        }
    }
}

/// 動いている断りのスレッド。手放すと止めて待つ（最大で 1 回の見回り。ルームの接続は閉じ始めるだけで待たない）。
pub struct Refuser {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Refuser {
    /// 止めて待ち、最後の数を返す。
    pub fn finish(mut self) -> RefuseCounts {
        self.halt();
        self.shared.counts()
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Refuser {
    fn drop(&mut self) {
        self.halt();
    }
}

/// 断りのスレッドを作る（見張りのスレッド）。`room` は元のルーム名（観戦用のルームの名はここで導く）。
pub fn spawn(opts: RoomOptions, room: &RoomName) -> Result<Refuser, String> {
    let shared = Arc::new(Shared::default());
    let mut runner = Runner::new(opts, room.spectate_of(), Arc::clone(&shared));
    let sh = Arc::clone(&shared);
    let h = std::thread::Builder::new()
        .name("higanr-refuse".into())
        .spawn(move || {
            let t0 = Instant::now();
            // パニックは受けて外へ出さない（スレッドの入口）。受けたら断りだけを止める。
            let r = catch_unwind(AssertUnwindSafe(|| {
                while !sh.stop.load(Ordering::Acquire) {
                    let now = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
                    runner.step(now);
                    std::thread::sleep(POLL);
                }
            }));
            if r.is_err() {
                bump(&sh.panicked);
            }
            runner.close();
        })
        .map_err(|e| format!("観戦拒否の開始不能: {e}"))?;
    Ok(Refuser {
        shared,
        thread: Some(h),
    })
}

#[cfg(test)]
mod tests;
