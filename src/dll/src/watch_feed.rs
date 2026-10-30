//! 観戦の「ランダムマッチ」の一覧の接続: メニューが一覧の画面を開いている間だけ動くスレッド`higanr-watchlist`。
//!
//! 一覧の接続先は見る口（`net_room::ViewListConn`）。届いた一覧の全体を`UiLink`のバッファへ書く。
//! ゲームのスレッドはバッファを読むだけ。
//! メモリの確保も待ちもこのスレッドと`ViewListConn`の受信スレッドの側で行う（マッチ中は動かないので相手への遅延の増分は0）。
//! 一覧の画面を出たら止める（`stop`）。接続できなかった・切れたときは「取得できなかった」を書いて終わる（再接続は人が画面を開き直したときに行う）。
//! ただしサーバーの期限切れ（4408）は黙って入り直す。入り直した接続で一覧を受ける前に続けて期限切れになったら「取得できなかった」とする。
//! 本ログには一覧の中身（名前・観戦用のルーム名）を書かない。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use higanr_core::room::Failure;
use higanr_core::watch_list::{ListStatus, ViewEvent};

use crate::net_room::{RoomOptions, TransportNote, ViewListConn};
use crate::net_ui::UiLink;

/// 一覧の接続のスレッドが`poll`する間隔（監視の処理`net_code_game::GAME_POLL`と同じ50 ms）。
const POLL: Duration = Duration::from_millis(50);

/// 世代は`UiLink`が持つカウンタ。止めた古いスレッドが後から開いた一覧のバッファへ書き込まないために、書く直前に自分の世代と比べる。
pub struct WatchFeed {
    link: Arc<UiLink>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchFeed {
    /// 一覧の接続のスレッドを作る。バッファへまず「取得中」を書く。作れなければエラーメッセージを返す。
    pub fn start(link: Arc<UiLink>, opts: RoomOptions, build: u32) -> Result<WatchFeed, String> {
        let generation = link.next_watch_generation();
        let stop = Arc::new(AtomicBool::new(false));
        let st = Arc::clone(&stop);
        link.publish_watch_list(ListStatus::Loading, &[]);
        let owner = Arc::clone(&link);
        let thread = std::thread::Builder::new()
            .name("higanr-watchlist".into())
            .spawn(move || {
                // パニックは受けて外へ出さない。受けたら「取得できなかった」を書く。
                let r = catch_unwind(AssertUnwindSafe(|| {
                    run(&owner, &opts, build, &st, generation)
                }));
                if r.is_err() {
                    publish(&owner, generation, ListStatus::Failed, &[]);
                }
            })
            .map_err(|e| format!("観戦の一覧のスレッドの作成不能: {e}"))?;
        Ok(WatchFeed {
            link,
            stop,
            thread: Some(thread),
        })
    }

    /// 止める（待たない。スレッドは次のポーリングで接続を閉じて終わる）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // 世代を進める。これ以後、止めたスレッドはバッファへ書き込まない。
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
            crate::main_log::push(format!("観戦の一覧: 接続を始められませんでした（{e}）。"));
            publish(link, generation, ListStatus::Failed, &[]);
            return;
        }
    };
    let mut skipped_logged = false;
    // 期限切れ（サーバーは見る口を作ってから10分で閉じる）で入り直した後でまだ一覧を受けていない間だけ真。
    let mut rejoined = false;
    'poll: while !stop.load(Ordering::Acquire) {
        let events = conn.poll();
        let notes = conn.take_notes();
        for ev in events {
            match ev {
                ViewEvent::Failed(Failure::Expired) if !rejoined => {
                    // 画面は一覧のまま黙って1回入り直す。新しい接続は最初に一覧の全体を送ってくる。
                    conn.cancel();
                    match ViewListConn::start_view(opts, build) {
                        Ok(c) => {
                            conn = c;
                            rejoined = true;
                            continue 'poll;
                        }
                        Err(e) => {
                            crate::main_log::push(format!(
                                "観戦の一覧: 入り直しを始められませんでした（{e}）。"
                            ));
                            publish(link, generation, ListStatus::Failed, &[]);
                            return;
                        }
                    }
                }
                ViewEvent::List { rows, skipped } => {
                    rejoined = false;
                    if skipped > 0 && !skipped_logged {
                        skipped_logged = true;
                        crate::main_log::push(format!(
                            "観戦の一覧: サーバーの決まりに合わない行を {skipped} 件捨てました。"
                        ));
                    }
                    publish(link, generation, ListStatus::Ready, &rows);
                }
                ViewEvent::Failed(f) => {
                    crate::main_log::push(failure_line(&f, &notes));
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

/// 取得できなかったときの本ログの1行。通信の記録はWinHTTPの段階名と番号だけ。ルーム名・アドレスは含まない。
fn failure_line(f: &impl std::fmt::Debug, notes: &[TransportNote]) -> String {
    if notes.is_empty() {
        format!("観戦の一覧: 取得不能（{f:?}）")
    } else {
        format!("観戦の一覧: 取得不能（{f:?}）運び {notes:?}")
    }
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
