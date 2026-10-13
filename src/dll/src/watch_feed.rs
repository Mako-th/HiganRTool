//! 観戦の「ランダムマッチ」の一覧の接続: メニューが一覧の画面を開いている間だけ動くスレッド `higanr-watchlist`。
//!
//! 一覧の見る口へ繋ぎ（`net_room::ViewListConn`）、届いた一覧の全体を `UiLink` の入れ物へ置く。ゲームのスレッドは入れ物を読むだけで、
//! 確保も待ちもこのスレッドと `ViewListConn` の受信スレッドの側（マッチ中は動かないので、対戦者への遅延の増分は 0）。
//! 一覧の画面を出たら止める（`stop`）。繋げなかった・切れたときは「取得できなかった」を置いて終わる（入り直すのは人が画面を開き直す）。
//! 本ログには一覧の中身（名前・観戦用のルーム名）を書かない。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use higanr_core::watch_list::{ListStatus, ViewEvent};

use crate::net_room::{RoomOptions, ViewListConn};
use crate::net_ui::UiLink;

/// 一覧の接続のスレッドが `poll` する間隔（見張り `net_code_game::GAME_POLL` と同じ 50 ms）。
const POLL: Duration = Duration::from_millis(50);

/// 世代は `UiLink` が持つ（止めた古いスレッドが、後から開いた一覧の入れ物へ書かないための印。置く直前に自分の世代と比べる）。
pub struct WatchFeed {
    link: Arc<UiLink>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchFeed {
    /// 一覧の接続のスレッドを起こす。入れ物へまず「取得中」を置く。起こせなければ誤りの文。
    pub fn start(link: Arc<UiLink>, opts: RoomOptions, build: u32) -> Result<WatchFeed, String> {
        let generation = link.next_watch_generation();
        let stop = Arc::new(AtomicBool::new(false));
        let st = Arc::clone(&stop);
        link.publish_watch_list(ListStatus::Loading, &[]);
        let owner = Arc::clone(&link);
        let thread = std::thread::Builder::new()
            .name("higanr-watchlist".into())
            .spawn(move || {
                // パニックは受けて外へ出さない。受けたら「取得できなかった」を置く。
                let r = catch_unwind(AssertUnwindSafe(|| {
                    run(&owner, &opts, build, &st, generation)
                }));
                if r.is_err() {
                    publish(&owner, generation, ListStatus::Failed, &[]);
                }
            })
            .map_err(|e| format!("観戦の一覧のスレッドを起こせない: {e}"))?;
        Ok(WatchFeed {
            link,
            stop,
            thread: Some(thread),
        })
    }

    /// 止める（待たない。スレッドは次の見回りで接続を閉じて終わる）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // 世代を進める: これ以後、止めたスレッドは入れ物へ書かない。
        self.link.next_watch_generation();
        self.thread = None;
    }

    #[cfg(test)]
    pub fn join(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WatchFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn run(link: &UiLink, opts: &RoomOptions, build: u32, stop: &AtomicBool, generation: u32) {
    let mut conn: ViewListConn = match ViewListConn::start_view(opts, build) {
        Ok(c) => c,
        Err(e) => {
            crate::main_log::push(format!("観戦の一覧: 接続を始められなかった（{e}）"));
            publish(link, generation, ListStatus::Failed, &[]);
            return;
        }
    };
    let mut skipped_logged = false;
    while !stop.load(Ordering::Acquire) {
        for ev in conn.poll() {
            match ev {
                ViewEvent::List { rows, skipped } => {
                    if skipped > 0 && !skipped_logged {
                        skipped_logged = true;
                        crate::main_log::push(format!(
                            "観戦の一覧: サーバーの決まりに合わない行を {skipped} 件捨てた"
                        ));
                    }
                    publish(link, generation, ListStatus::Ready, &rows);
                }
                ViewEvent::Failed(f) => {
                    crate::main_log::push(format!("観戦の一覧: 取得できなかった（{f:?}）"));
                    publish(link, generation, ListStatus::Failed, &[]);
                    conn.cancel();
                    return;
                }
            }
        }
        std::thread::sleep(POLL);
    }
    conn.cancel();
}

fn publish(
    link: &UiLink,
    generation: u32,
    status: ListStatus,
    rows: &[higanr_core::watch_list::WatchRow],
) {
    if link.watch_generation() == generation {
        link.publish_watch_list(status, rows);
    }
}

#[cfg(test)]
mod tests;
