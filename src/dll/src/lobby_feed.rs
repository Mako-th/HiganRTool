//! ルーム名ごとの一覧の画面の接続: メニューが一覧の画面を開いている間だけ動くスレッド`higanr-lobbylist`。
//!
//! 持ち方は観戦の「ランダムマッチ」の一覧（`watch_feed`）と同じ。届いた一覧の全体を`UiLink`のバッファへ書き、ゲームのスレッドは読むだけ。
//! 接続できなかった・切れたときは「取得できなかった」を書いて終わる。サーバーの期限切れ（4408）は黙って入り直す。
//! 本ログには一覧の中身（名前・割り当てのルーム名）とルーム名を書かない。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use higanr_core::lobby::{LobbyEvent, LobbyRow};
use higanr_core::room::{Failure, RoomName};
use higanr_core::watch_list::ListStatus;

use crate::net_room::{LobbyConn, RoomOptions};
use crate::net_ui::UiLink;

const POLL: Duration = Duration::from_millis(50);

/// 世代は`UiLink`が持つカウンタ。一覧の画面を開け閉めするたびに`NetUi`が進め、このスレッドは開けたときの世代でだけ書く。
pub struct LobbyFeed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LobbyFeed {
    /// 一覧の接続のスレッドを作る。`generation`は`NetUi`が開けたときに進めた世代。作れなければエラーメッセージを返す。
    /// 呼ぶのはゲームのスレッドなのでバッファには書かない（「取得中」は`NetUi`が自分のコピーに設定する）。
    pub fn start(
        link: Arc<UiLink>,
        opts: RoomOptions,
        build: u32,
        name: RoomName,
        generation: u32,
    ) -> Result<LobbyFeed, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let st = Arc::clone(&stop);
        let owner = link;
        let thread = std::thread::Builder::new()
            .name("higanr-lobbylist".into())
            .spawn(move || {
                // パニックは受けて外へ出さない。受けたら「取得できなかった」を書く。
                let r = catch_unwind(AssertUnwindSafe(|| {
                    run(&owner, &opts, build, &name, &st, generation)
                }));
                if r.is_err() {
                    publish(&owner, generation, ListStatus::Failed, &[]);
                }
            })
            .map_err(|e| format!("ルーム名ごとの一覧のスレッドの作成不能: {e}"))?;
        Ok(LobbyFeed {
            stop,
            thread: Some(thread),
        })
    }

    /// 止める（待たない。スレッドは次のポーリングで接続を閉じて終わる）。世代は`NetUi`が先に進めているので以後は書けない。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
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

impl Drop for LobbyFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn run(
    link: &UiLink,
    opts: &RoomOptions,
    build: u32,
    name: &RoomName,
    stop: &AtomicBool,
    generation: u32,
) {
    let mut conn = match LobbyConn::start_lobby(opts, build, name) {
        Ok(c) => c,
        Err(e) => {
            crate::main_log::push(format!(
                "ルーム名ごとの一覧: 接続を始められませんでした（{e}）。"
            ));
            publish(link, generation, ListStatus::Failed, &[]);
            return;
        }
    };
    let mut skipped_logged = false;
    // 期限切れで入り直した後でまだ一覧を受けていない間だけ真（続けて期限切れなら「取得できなかった」）。
    let mut rejoined = false;
    'poll: while !stop.load(Ordering::Acquire) {
        let events = conn.poll();
        let notes = conn.take_notes();
        for ev in events {
            match ev {
                LobbyEvent::Failed(Failure::Expired) if !rejoined => {
                    conn.cancel();
                    match LobbyConn::start_lobby(opts, build, name) {
                        Ok(c) => {
                            conn = c;
                            rejoined = true;
                            continue 'poll;
                        }
                        Err(e) => {
                            crate::main_log::push(format!(
                                "ルーム名ごとの一覧: 入り直しを始められませんでした（{e}）。"
                            ));
                            publish(link, generation, ListStatus::Failed, &[]);
                            return;
                        }
                    }
                }
                LobbyEvent::List { rows, skipped } => {
                    rejoined = false;
                    if skipped > 0 && !skipped_logged {
                        skipped_logged = true;
                        crate::main_log::push(format!(
                            "ルーム名ごとの一覧: サーバーの決まりに合わない行を {skipped} 件捨てました。"
                        ));
                    }
                    publish(link, generation, ListStatus::Ready, &rows);
                }
                // 見るだけの接続は`create`を送らないので届かない筋。
                LobbyEvent::Created { .. } => {}
                LobbyEvent::Failed(f) => {
                    if notes.is_empty() {
                        crate::main_log::push(format!("ルーム名ごとの一覧: 取得不能（{f:?}）"));
                    } else {
                        crate::main_log::push(format!(
                            "ルーム名ごとの一覧: 取得不能（{f:?}）運び {notes:?}"
                        ));
                    }
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

fn publish(link: &UiLink, generation: u32, status: ListStatus, rows: &[LobbyRow]) {
    link.publish_lobby_list(generation, status, rows);
}

#[cfg(test)]
mod tests;
