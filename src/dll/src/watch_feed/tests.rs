//! 一覧の接続のスレッド（`watch_feed.rs`）の検査。偽のルームサーバー（loopback）と本物の WinHTTP で実時間に回すものは `#[ignore]`。
//! 外へは繋がない（`127.0.0.1` と `Proxy::Direct`）。名前・ルーム名は合成の値だけ。
use super::*;

use std::time::Instant;

use higanr_core::room_queue::AssignedRoom;
use higanr_core::watch_list::{Posting, WatchName, WatchRow, WatchSnapshot};
use higanr_core::wire::Mode;

use crate::net_room::fake::FakeServer;
use crate::net_room::{Base, HostListConn};
use crate::room_win::{Proxy, Timeouts};

const BUILD: u32 = 11;
const Q: &str = "q.0123456789abcdef0123456789abcd";

fn opts(base: &str) -> RoomOptions {
    RoomOptions {
        base: Base::parse(base).unwrap(),
        proxy: Proxy::Direct,
        timeouts: Timeouts {
            resolve_ms: 2_000,
            connect_ms: 2_000,
            send_ms: 2_000,
            response_ms: 2_000,
            close_wait_ms: 500,
        },
    }
}

fn posting(p1: &str, q: &str) -> Posting {
    Posting {
        room: AssignedRoom::parse(q).unwrap(),
        p1: WatchName::parse(p1).unwrap(),
        p2: WatchName::parse("bob").unwrap(),
        mode: Mode::Delay,
        delay: 2,
        cap: 10,
    }
}

/// 一覧の入れ物を見る側（ゲームのスレッドの代わり）。
struct Reader {
    link: Arc<UiLink>,
    seen: u32,
    snap: Box<WatchSnapshot>,
    scratch: Box<WatchSnapshot>,
}

impl Reader {
    fn new(link: &Arc<UiLink>) -> Reader {
        Reader {
            link: Arc::clone(link),
            seen: link.watch_list_version(),
            snap: Box::new(WatchSnapshot::EMPTY),
            scratch: Box::new(WatchSnapshot::EMPTY),
        }
    }

    fn poll(&mut self) {
        if self.link.read_watch_list(&mut self.scratch, &mut self.seen) {
            std::mem::swap(&mut self.snap, &mut self.scratch);
        }
    }

    fn wait(&mut self, what: &str, mut cond: impl FnMut(&WatchSnapshot) -> bool) {
        let t0 = Instant::now();
        loop {
            self.poll();
            if cond(&self.snap) {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "5 秒の内に {what} にならない: {:?} {} 行",
                self.snap.status,
                self.snap.len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// 載せる口に 1 件載せて、載るまで回す（対戦のホストの代わり）。
fn host(srv: &FakeServer, p1: &str, q: &str) -> HostListConn {
    let mut c = HostListConn::start_post(&opts(&srv.base()), BUILD, posting(p1, q), 0).unwrap();
    let t0 = Instant::now();
    let want = srv.watch_items(BUILD).len() + 1;
    while srv.watch_items(BUILD).len() < want {
        assert!(t0.elapsed() < Duration::from_secs(5), "載らない");
        c.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    // 載った知らせ（listed）を受けて、観戦者の数を送れる状態にする。
    std::thread::sleep(Duration::from_millis(100));
    c.poll();
    c
}

#[test]
fn a_stopped_feed_can_no_longer_write_the_box_and_a_new_one_can() {
    let link = Arc::new(UiLink::new());
    let row = WatchRow {
        hash: [9; 8],
        p1: WatchName::parse("a").unwrap(),
        p2: WatchName::parse("b").unwrap(),
        mode: Mode::Delay,
        delay: 0,
        watchers: 0,
        cap: 10,
    };
    let old = link.next_watch_generation();
    let mut r = Reader::new(&link);
    publish(&link, old, ListStatus::Ready, &[row]);
    r.poll();
    assert_eq!(r.snap.len(), 1, "いまの世代は書ける");
    // 世代が進んだ（止めた・次を起こした）ら、古い世代は書けない。
    link.next_watch_generation();
    publish(&link, old, ListStatus::Ready, &[]);
    r.poll();
    assert_eq!(r.snap.len(), 1, "止めた後の古いスレッドが入れ物を書いた");
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_feed_shows_the_servers_list_follows_its_changes_and_closes_when_stopped() {
    let srv = FakeServer::start();
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = WatchFeed::start(Arc::clone(&link), opts(&srv.base()), BUILD).unwrap();
    // 空の一覧が届く（取得中 → 0 件）。
    rd.wait("空の一覧が届く", |s| s.status == ListStatus::Ready);
    assert_eq!(rd.snap.len(), 0);
    assert_eq!(
        srv.watch_views(BUILD),
        1,
        "一覧の画面の間は 1 本だけ見ている"
    );
    // 載ると全体が届く（観戦用のルーム名は割り当ての名から導いたもの）。
    let mut a = host(&srv, "alice", Q);
    rd.wait("1 件載る", |s| s.len() == 1);
    let row = rd.snap.rows()[0];
    assert_eq!(
        (
            row.p1.as_str(),
            row.p2.as_str(),
            row.mode,
            row.delay,
            row.watchers,
            row.cap
        ),
        ("alice", "bob", Mode::Delay, 2, 0, 10)
    );
    assert_eq!(
        row.room_name(),
        Some(AssignedRoom::parse(Q).unwrap().name().spectate_of()),
        "一覧の行から戻した観戦用のルーム名が、ホストの導く名と同じ"
    );
    // 2 件目と、観戦者の数の変化。
    let mut b = host(&srv, "carol", "q.a1b2c3d4e5f60718293a4b5c6d7e8f");
    rd.wait("2 件載る", |s| s.len() == 2);
    assert_eq!(
        rd.snap
            .rows()
            .iter()
            .map(|r| r.p1.as_str())
            .collect::<Vec<_>>(),
        ["alice", "carol"],
        "載せた順"
    );
    a.set_watchers(4);
    let t0 = Instant::now();
    rd.wait("観戦者の数が届く", |s| {
        s.rows().first().is_some_and(|r| r.watchers == 4)
    });
    let _ = t0;
    // 載せた人が切れると消える。
    a.cancel();
    rd.wait("1 件消える", |s| s.len() == 1);
    assert_eq!(rd.snap.rows()[0].p1.as_str(), "carol");
    b.cancel();
    rd.wait("0 件になる", |s| s.is_empty());
    // 止めると見る口の接続が閉じる。
    let t = Instant::now();
    feed.join();
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "止めるのに {:?}",
        t.elapsed()
    );
    let t0 = Instant::now();
    while srv.watch_views(BUILD) != 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "止めたのに見る口が残る"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_feed_says_failed_when_the_server_refuses_or_is_unreachable_and_never_rows() {
    // 429（回数超え）。
    let srv = FakeServer::start();
    *srv.knobs.forced.lock().unwrap() = Some((
        429,
        r#"{"error":"rate_limited","retry_after":60}"#.to_owned(),
    ));
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = WatchFeed::start(Arc::clone(&link), opts(&srv.base()), BUILD).unwrap();
    rd.wait("失敗が届く", |s| s.status == ListStatus::Failed);
    assert!(rd.snap.is_empty());
    feed.join();
    // 繋がらない（閉じた loopback の番号）。
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = WatchFeed::start(Arc::clone(&link), opts("ws://127.0.0.1:9"), BUILD).unwrap();
    rd.wait("失敗が届く", |s| s.status == ListStatus::Failed);
    assert!(rd.snap.is_empty());
    feed.join();
}
