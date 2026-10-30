//! 観戦を断る配布側: 2人のどちらかが観戦を許可していないルームマッチのホストが、観戦用のルーム（`RoomName::spectate_of`）に役hostで入る。
//! `offer`の代わりに`refuse`を送り、ルームの期限まで残る。入ってくる観戦者はサーバーが4403で閉じるので、観戦者にはすぐ断りの文が出る。
//! 入り直しの判断は配布側と同じ`Caster`（4408の後すぐ入り直して、また断る）。
//! 監視のスレッド（`net_code_game::GameWatcher`）が接続したときに`spawn`し、接続の後のポーリングを終えるときに手放す（`Drop`で止めて待つ）。
//! ゲームのスレッドは何も呼ばない。ルーム名は本ログに書かない。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::room::{Event, Failure, RoomName, SeatTicket, Why};
use higanr_core::spectate_room::{Caster, CasterAction};

use crate::net_room::{RoomConn, RoomOptions, SubmitRefused};

/// ポーリングの間隔（ms）。配布側（`spectate_offer`）と同じ50 ms。
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefuseCounts {
    /// 役hostで入った回数。
    pub entered: u32,
    /// `refuse`を送れた回数。
    pub refused: u32,
    /// 役guestを受けた（観戦者が先にいた）ので閉じた回数。
    pub wrong_role: u32,
    /// ルームの手順の失敗（ルームの期限4408を含む）。
    pub failed: u32,
    /// より新しいチケットの配布側に席を渡した回数。
    pub evicted: u32,
    /// サーバーがチケットを受けなかった（以後は入らない）。
    pub bad_ticket: u32,
    pub connect_errors: u32,
    pub panicked: u32,
}

impl RefuseCounts {
    pub fn text(&self) -> String {
        format!(
            "観戦拒否（観戦用のルームで観戦者を拒否）: 入った {}（役が逆で閉じた {}）・拒否を送った {}・ルームの失敗 {}（入室許可の拒否 {}）・後から成立した対戦に席を譲った {}・接続不能 {}{}",
            self.entered,
            self.wrong_role,
            self.refused,
            self.failed,
            self.bad_ticket,
            self.evicted,
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
    evicted: AtomicU32,
    bad_ticket: AtomicU32,
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
            evicted: g(&self.evicted),
            bad_ticket: g(&self.bad_ticket),
            connect_errors: g(&self.connect_errors),
            panicked: g(&self.panicked),
        }
    }
}

/// 観戦用のルーム1つ分の処理。`step`は時刻を引数で渡す形（検査は時計を進めて直接呼ぶ）。
pub(crate) struct Runner {
    room: RoomOptions,
    /// 観戦用のルームの名（元のルーム名のハッシュ。`Debug`に出さない）。
    name: RoomName,
    /// hostの席を取るチケット（ログに出さない）。
    ticket: SeatTicket,
    caster: Caster,
    conn: Option<RoomConn>,
    shared: Arc<Shared>,
    /// 席を譲った・チケットを拒否されたことを本ログに書いた（1回だけ）。
    told: bool,
}

impl Runner {
    fn new(room: RoomOptions, origin: RoomName, ticket: SeatTicket, shared: Arc<Shared>) -> Runner {
        Runner {
            room,
            name: origin.spectate_of(),
            ticket,
            caster: Caster::new(0),
            conn: None,
            shared,
            told: false,
        }
    }

    pub(crate) fn step(&mut self, now_ms: u64) {
        if let Some(CasterAction::Connect) = self.caster.poll(now_ms) {
            match RoomConn::start_as_host(&self.room, &self.name, &self.ticket) {
                Ok(c) => self.conn = Some(c),
                Err(_) => {
                    bump(&self.shared.connect_errors);
                    self.caster
                        .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
                }
            }
        }
        self.drain(now_ms);
        // `refuse`の送信が失敗すると、その失敗のイベントは次の`poll`で返る。
        // 捨てる前に1回読まないと`Caster`が入室済みのままになり、二度と入り直さない。
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
            // `Caster`の「offerせよ」をここでは「断れ」と読む。
            Event::Joined { role, .. } => match self.caster.on_joined(now_ms, role) {
                Some(CasterAction::MakeOffer) => {
                    bump(&self.shared.entered);
                    let Some(conn) = self.conn.as_mut() else {
                        return;
                    };
                    match conn.refuse() {
                        Ok(()) => bump(&self.shared.refused),
                        // 送れなかった失敗は`step`の末尾の`drain`で届き、そこで入り直しの待ちに入る。
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
            Event::Matched { .. } | Event::PeerCode(_) | Event::Done | Event::Held => {
                self.caster.on_done(now_ms);
                self.conn = None;
            }
            Event::Failed(f) => {
                bump(&self.shared.failed);
                let was_evicted = self.caster.evicted();
                self.caster.on_failed(now_ms, f);
                self.conn = None;
                let line = if f == Failure::BadTicket {
                    bump(&self.shared.bad_ticket);
                    "観戦拒否: 停止（ルームサーバーが観戦用のルームの入室許可を拒否）。対戦は続きます。"
                } else if self.caster.evicted() && !was_evicted {
                    bump(&self.shared.evicted);
                    "観戦拒否: 同じルーム名で後から成立した対戦に観戦用のルームを譲りました（入り直しは 5 分ごと）。"
                } else {
                    return;
                };
                if !self.told {
                    self.told = true;
                    crate::main_log::push(line);
                }
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

/// 動いている断りのスレッド。手放すと止めて待つ（最大でポーリング1回分。ルームの接続は閉じ始めるだけで待たない）。
pub struct Refuser {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Refuser {
    /// 止めて待ち、最後のカウントを返す。
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

/// 断りのスレッドを作る（監視のスレッドが呼ぶ）。`room`は元のルーム名（観戦用のルームの名はここで導く。サーバーへは送らない）。
/// `ticket`はその対戦の成立で受け取った席のチケット。
pub fn spawn(opts: RoomOptions, room: &RoomName, ticket: SeatTicket) -> Result<Refuser, String> {
    let shared = Arc::new(Shared::default());
    let mut runner = Runner::new(opts, *room, ticket, Arc::clone(&shared));
    let sh = Arc::clone(&shared);
    let h = std::thread::Builder::new()
        .name("higanr-refuse".into())
        .spawn(move || {
            let t0 = Instant::now();
            // パニックは捕捉して外へ出さない（スレッドの入口）。捕捉したら断りだけを止める。
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
