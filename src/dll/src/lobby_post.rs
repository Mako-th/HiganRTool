//! ルーム名ごとの一覧へ試合中の1件を載せるスレッド`higanr-lobbypost`（ルーム名ごとの一覧から会ったマッチのホストだけ）。
//!
//! 観戦を許可していないペアも`watch:false`で載せるので観戦の受付（`spectate_offer`）とは別に動く。観戦者の数は受付があればその数を送る。
//! 切れたら間隔を空けて載せ直す（`spectate_offer::relist_delay`）。件数の上限（`list_full`）で断られたら載せ直さない。
//! 監視の処理が接続の後の定期処理を終えるまで（試合の後のメニューと再戦の間も）持ち続ける。
//! ゲームのスレッドは何も呼ばない。割り当てのルーム名・チケットは本ログに書かない。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::lobby::LobbyPosting;
use higanr_core::room::{Failure, RoomName};
use higanr_core::watch_list::HostEvent;

use crate::net_room::{HostListConn, RoomOptions};
use crate::spectate_host;
use crate::spectate_offer::relist_delay;

/// ポーリングの間隔（観戦の受付のスレッドと同じ50 ms）。
const POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    listed: AtomicU32,
    failed: AtomicU32,
    connect_errors: AtomicU32,
    /// 件数の上限（`list_full`）で断られた（以後は載せ直さない）。
    gave_up: AtomicBool,
    panicked: AtomicBool,
}

/// 本ログの1行に出す数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PostCounts {
    pub listed: u32,
    pub failed: u32,
    pub connect_errors: u32,
    pub gave_up: bool,
    pub panicked: bool,
}

impl PostCounts {
    pub fn text(&self) -> String {
        format!(
            "ルーム名ごとの一覧: 試合中として載った {}・載せ不能 {}・接続できなかった {}{}{}",
            self.listed,
            self.failed,
            self.connect_errors,
            if self.gave_up {
                "（件数の上限で断られたので載せ直さない）"
            } else {
                ""
            },
            if self.panicked {
                "・スレッドが異常終了"
            } else {
                ""
            }
        )
    }
}

impl Shared {
    fn counts(&self) -> PostCounts {
        PostCounts {
            listed: self.listed.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
            connect_errors: self.connect_errors.load(Ordering::Acquire),
            gave_up: self.gave_up.load(Ordering::Acquire),
            panicked: self.panicked.load(Ordering::Acquire),
        }
    }
}

fn bump(a: &AtomicU32) {
    a.fetch_add(1, Ordering::AcqRel);
}

pub(crate) struct Runner {
    opts: RoomOptions,
    build: u32,
    /// 打ったルーム名（一覧のパスの元）。
    name: RoomName,
    posting: LobbyPosting,
    /// 観戦の受付の集計（観戦者の数を読む）。観戦を許可していないペアには受付が無い。
    watchers: Option<Arc<spectate_host::Shared>>,
    conn: Option<HostListConn>,
    retry_at_ms: u64,
    fails: u32,
    sent: u8,
    shared: Arc<Shared>,
}

impl Runner {
    fn new(
        opts: RoomOptions,
        build: u32,
        name: RoomName,
        posting: LobbyPosting,
        watchers: Option<Arc<spectate_host::Shared>>,
        shared: Arc<Shared>,
    ) -> Runner {
        Runner {
            opts,
            build,
            name,
            posting,
            watchers,
            conn: None,
            retry_at_ms: 0,
            fails: 0,
            sent: 0,
            shared,
        }
    }

    fn watchers_now(&self) -> u8 {
        self.watchers.as_ref().map_or(0, |w| {
            u8::try_from(w.get().spectators)
                .unwrap_or(u8::MAX)
                .min(self.posting.cap)
        })
    }

    /// 1回のポーリング: 切れていれば間隔を空けて接続し直し、観戦者の数が変わったら送信し、届いたイベントを数える。
    pub(crate) fn step(&mut self, now_ms: u64) {
        if self.shared.gave_up.load(Ordering::Acquire) {
            return;
        }
        let watchers = self.watchers_now();
        if self.conn.is_none() && now_ms >= self.retry_at_ms {
            match HostListConn::start_lobby_post(
                &self.opts,
                self.build,
                &self.name,
                self.posting,
                watchers,
            ) {
                Ok(c) => {
                    self.sent = watchers;
                    self.conn = Some(c);
                }
                Err(_) => {
                    bump(&self.shared.connect_errors);
                    self.fails = self.fails.saturating_add(1);
                    self.retry_at_ms = now_ms + relist_delay(self.fails);
                }
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        if watchers != self.sent {
            self.sent = watchers;
            conn.set_watchers(watchers);
        }
        let evs = conn.poll();
        for e in evs {
            match e {
                HostEvent::Listed => {
                    bump(&self.shared.listed);
                    self.fails = 0;
                }
                HostEvent::Failed(f) => {
                    bump(&self.shared.failed);
                    self.fails = self.fails.saturating_add(1);
                    // 見る接続の上限（409 `room_full`）は一時的な断りなので間隔を空けて載せ直す。
                    match f {
                        Failure::ListFull => self.shared.gave_up.store(true, Ordering::Release),
                        Failure::RateLimited { retry_after_secs } => {
                            self.retry_at_ms = now_ms + u64::from(retry_after_secs) * 1000;
                        }
                        _ => self.retry_at_ms = now_ms + relist_delay(self.fails),
                    }
                    if let Some(mut c) = self.conn.take() {
                        c.cancel();
                    }
                    return;
                }
            }
        }
        if self.conn.as_ref().is_some_and(HostListConn::is_finished) {
            self.conn = None;
        }
    }

    fn close(&mut self) {
        if let Some(mut c) = self.conn.take() {
            c.cancel();
        }
    }
}

/// 載せるスレッド。手放す（`finish`か`Drop`）と止めて待つ（最大でポーリング1回分）。
pub struct LobbyPoster {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl LobbyPoster {
    /// 止めて待ち、最後の数を返す。
    pub fn finish(mut self) -> PostCounts {
        self.halt();
        self.shared.counts()
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> PostCounts {
        self.shared.counts()
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for LobbyPoster {
    fn drop(&mut self) {
        self.halt();
    }
}

/// 載せるスレッドを作る（監視のスレッドが呼ぶ）。`name`は打ったルーム名。`watchers`は観戦の受付の集計（受付が無いペアは`None`）。
pub fn spawn(
    opts: RoomOptions,
    build: u32,
    name: RoomName,
    posting: LobbyPosting,
    watchers: Option<Arc<spectate_host::Shared>>,
) -> Result<LobbyPoster, String> {
    let shared = Arc::new(Shared::default());
    let mut runner = Runner::new(opts, build, name, posting, watchers, Arc::clone(&shared));
    let sh = Arc::clone(&shared);
    let h = std::thread::Builder::new()
        .name("higanr-lobbypost".into())
        .spawn(move || {
            let t0 = Instant::now();
            // パニックは捕捉して外へ出さない（スレッドの入口）。捕捉したら載せるのだけをやめる。
            let r = catch_unwind(AssertUnwindSafe(|| {
                while !sh.stop.load(Ordering::Acquire) {
                    let now = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
                    runner.step(now);
                    std::thread::sleep(POLL);
                }
            }));
            if r.is_err() {
                sh.panicked.store(true, Ordering::Release);
            }
            runner.close();
        })
        .map_err(|e| format!("ルーム名ごとの一覧へ載せるスレッドの作成不能: {e}"))?;
    Ok(LobbyPoster {
        shared,
        thread: Some(h),
    })
}

#[cfg(test)]
mod tests;
