//! ルーム名ごとの一覧の画面の接続（`lobby_feed.rs`）の検査。偽のルームサーバー（loopback）と本物のWinHTTPで実時間に回すものは`#[ignore]`。
//! 外へは接続しない（`127.0.0.1`と`Proxy::Direct`）。名前・ルーム名は合成の値だけ。
use super::*;

use std::time::Instant;

use higanr_core::lobby::LobbySnapshot;
use higanr_core::watch_list::WatchName;

use crate::net_room::Base;
use crate::net_room::fake::FakeServer;
use crate::room_win::{Proxy, Timeouts};

const BUILD: u32 = 12;

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

fn room(name: &str) -> RoomName {
    RoomName::parse(name).unwrap()
}

/// 一覧のバッファを読む側（ゲームのスレッドの代わり）。作るときに`NetUi`と同じく世代を進める。
struct Reader {
    link: Arc<UiLink>,
    generation: u32,
    seen: u32,
    snap: Box<LobbySnapshot>,
    scratch: Box<LobbySnapshot>,
}

impl Reader {
    fn new(link: &Arc<UiLink>) -> Reader {
        Reader {
            link: Arc::clone(link),
            generation: link.next_lobby_generation(),
            seen: link.lobby_list_version(),
            snap: Box::new(LobbySnapshot::EMPTY),
            scratch: Box::new(LobbySnapshot::EMPTY),
        }
    }

    fn poll(&mut self) {
        if self
            .link
            .read_lobby_list(&mut self.scratch, &mut self.seen, self.generation)
        {
            std::mem::swap(&mut self.snap, &mut self.scratch);
        }
    }

    fn wait(&mut self, what: &str, mut cond: impl FnMut(&LobbySnapshot) -> bool) {
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

/// 一覧へ`create`して待機中の1件として載る（作った人の代わり）。
fn creator(srv: &FakeServer, name: &str, who: &str) -> LobbyConn {
    let mut c = LobbyConn::start_lobby(&opts(&srv.base()), BUILD, &room(name)).unwrap();
    let t0 = Instant::now();
    let mut sent = false;
    loop {
        for e in c.poll() {
            match e {
                LobbyEvent::List { .. } if !sent => {
                    sent = c.create(&WatchName::parse(who).unwrap());
                }
                LobbyEvent::Created { .. } => return c,
                LobbyEvent::Failed(f) => panic!("作れない: {f:?}"),
                _ => {}
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "載らない");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_row(i: u8, who: &str) -> LobbyRow {
    LobbyRow::Wait(higanr_core::lobby::WaitRow {
        name: WatchName::parse(who).unwrap(),
        room: higanr_core::lobby::LobbyRoom::parse(&format!("r.{:030x}", u128::from(i) << 8))
            .unwrap(),
    })
}

#[test]
fn a_stopped_lobby_feed_can_no_longer_write_the_box() {
    let link = Arc::new(UiLink::new());
    let mut r = Reader::new(&link);
    let old = r.generation;
    assert!(link.publish_lobby_list(old, ListStatus::Ready, &[wait_row(1, "a")]));
    r.poll();
    assert_eq!(r.snap.len(), 1, "いまの世代は書ける");
    let v = link.lobby_list_version();
    link.next_lobby_generation();
    assert!(
        !link.publish_lobby_list(old, ListStatus::Ready, &[]),
        "止めた後の古いスレッドがバッファを書いた"
    );
    assert_eq!(
        link.lobby_list_version(),
        v,
        "書かなかったのにバージョンが進んだ"
    );
}

#[test]
fn a_list_written_under_the_previous_generation_is_not_taken() {
    let link = Arc::new(UiLink::new());
    let old = link.next_lobby_generation();
    // 古いスレッドが世代の確認を通った直後に画面が開き直された（書き終えるのは世代が進んだ後）。
    assert!(link.publish_lobby_list(old, ListStatus::Ready, &[wait_row(1, "old")]));
    let new = link.next_lobby_generation();
    let mut snap = Box::new(LobbySnapshot::EMPTY);
    // 読み始めのバージョンはその書き込みより前だった形（0）。
    let mut seen = 0;
    assert!(
        !link.read_lobby_list(&mut snap, &mut seen, new),
        "前の世代の一覧を受け取った"
    );
    assert!(snap.is_empty(), "前の世代の一覧で写しを書き換えた");
    assert_eq!(
        seen,
        link.lobby_list_version(),
        "前の世代の一覧を読んだことにしていない（毎回読み直す）"
    );
    assert!(link.publish_lobby_list(new, ListStatus::Ready, &[wait_row(2, "new")]));
    assert!(link.read_lobby_list(&mut snap, &mut seen, new));
    let LobbyRow::Wait(w) = snap.rows()[0] else {
        panic!("{:?}", snap.rows());
    };
    assert_eq!((snap.len(), w.name.as_str()), (1, "new"));
}

#[test]
fn an_old_and_a_new_writer_never_leave_an_old_list() {
    let link = Arc::new(UiLink::new());
    let old = link.next_lobby_generation();
    let mut r = Reader::new(&link);
    let new = r.generation;
    let stop = Arc::new(AtomicBool::new(false));
    let w_old = {
        let (link, stop) = (Arc::clone(&link), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut wrote = 0u32;
            while !stop.load(Ordering::Acquire) {
                wrote += u32::from(link.publish_lobby_list(
                    old,
                    ListStatus::Ready,
                    &[wait_row(9, "old")],
                ));
            }
            wrote
        })
    };
    let w_new = {
        let link = Arc::clone(&link);
        std::thread::spawn(move || {
            (0..2_000u32)
                .filter(|i| {
                    let rows: Vec<_> = (0..(i % 5) as u8).map(|k| wait_row(k, "new")).collect();
                    link.publish_lobby_list(new, ListStatus::Ready, &rows)
                })
                .count()
        })
    };
    let mut taken = 0u32;
    while !w_new.is_finished() {
        let before = r.seen;
        r.poll();
        if r.seen != before {
            taken += 1;
        }
        for row in r.snap.rows() {
            let LobbyRow::Wait(w) = row else {
                panic!("試合中の行: {row:?}");
            };
            assert_eq!(w.name.as_str(), "new", "古い世代の行を受け取った");
        }
    }
    stop.store(true, Ordering::Release);
    assert_eq!(
        w_new.join().unwrap(),
        2_000,
        "いまの世代の書き込みが断られた"
    );
    assert_eq!(w_old.join().unwrap(), 0, "古い世代の書き手が書いた");
    assert!(taken > 0, "一度も読めていない");
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_lobby_feed_shows_the_rooms_list_follows_it_and_closes_when_stopped() {
    let srv = FakeServer::start();
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = LobbyFeed::start(
        Arc::clone(&link),
        opts(&srv.base()),
        BUILD,
        room("Synth-L"),
        rd.generation,
    )
    .unwrap();
    rd.wait("空の一覧が届く", |s| s.status == ListStatus::Ready);
    assert!(rd.snap.is_empty());
    assert_eq!(
        srv.lobby_views(BUILD, "synth-l"),
        1,
        "揃えた名の一覧を 1 本見ている"
    );
    assert_eq!(srv.lobby_views(BUILD, "other"), 0);
    let a = creator(&srv, "synth-l", "alice");
    rd.wait("待機中の 1 件が届く", |s| s.len() == 1);
    let LobbyRow::Wait(w) = rd.snap.rows()[0] else {
        panic!("待機中の行でない: {:?}", rd.snap.rows());
    };
    assert_eq!(w.name.as_str(), "alice");
    assert_eq!(
        srv.lobby_wait_rooms(BUILD, "synth-l"),
        [w.room.name().as_str().to_string()],
        "割り当てのルーム名がそのまま届く"
    );
    // 別の名の一覧には出ない。
    let mut b = creator(&srv, "other", "bob");
    std::thread::sleep(Duration::from_millis(100));
    rd.poll();
    assert_eq!(rd.snap.len(), 1, "別の名の一覧の行が届いた");
    // 作った人が切れると消える。
    drop(a);
    rd.wait("0 件に戻る", |s| {
        s.status == ListStatus::Ready && s.is_empty()
    });
    b.cancel();
    let t = Instant::now();
    feed.join();
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "止めるのに {:?}",
        t.elapsed()
    );
    let t0 = Instant::now();
    while srv.lobby_views(BUILD, "synth-l") != 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "止めたのに見る接続が残る"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_lobby_feed_silently_rejoins_when_the_view_expires() {
    let srv = FakeServer::start();
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = LobbyFeed::start(
        Arc::clone(&link),
        opts(&srv.base()),
        BUILD,
        room("exp"),
        rd.generation,
    )
    .unwrap();
    rd.wait("空の一覧が届く", |s| s.status == ListStatus::Ready);
    let ups = || srv.seen.lobby_upgrades.load(Ordering::Acquire);
    assert_eq!(ups(), 1);
    for want in [2, 3] {
        srv.expire_lobby(BUILD, "exp", false);
        let t0 = Instant::now();
        while ups() < want {
            rd.poll();
            assert_ne!(
                rd.snap.status,
                ListStatus::Failed,
                "期限切れで失敗にした（{want}）"
            );
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "入り直さない（{want}）"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // 入り直した接続の一覧を受けてから次の期限切れを起こす（続けて期限切れなら失敗にする作り）。
        let t0 = Instant::now();
        while srv.lobby_views(BUILD, "exp") != 1 {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _a = creator(&srv, "exp", "alice");
    rd.wait("入り直した接続で行が届く", |s| s.len() == 1);
    assert_eq!(rd.snap.status, ListStatus::Ready);
    feed.join();
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_lobby_feed_says_failed_when_refused_or_unreachable() {
    let srv = FakeServer::start();
    *srv.knobs.forced.lock().unwrap() = Some((
        429,
        r#"{"error":"rate_limited","retry_after":60}"#.to_owned(),
    ));
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = LobbyFeed::start(
        Arc::clone(&link),
        opts(&srv.base()),
        BUILD,
        room("abc"),
        rd.generation,
    )
    .unwrap();
    rd.wait("失敗が届く", |s| s.status == ListStatus::Failed);
    assert!(rd.snap.is_empty());
    feed.join();
    let link = Arc::new(UiLink::new());
    let mut rd = Reader::new(&link);
    let feed = LobbyFeed::start(
        Arc::clone(&link),
        opts("ws://127.0.0.1:9"),
        BUILD,
        room("abc"),
        rd.generation,
    )
    .unwrap();
    rd.wait("失敗が届く", |s| s.status == ListStatus::Failed);
    feed.join();
}

#[test]
#[ignore = "偽サーバーと本物のWinHTTP（loopback）で実時間に回す（1 秒ほど）。全件の回（build.bat）で --ignored で走る"]
fn the_fake_lobby_refuses_reserved_names_and_a_full_list() {
    let srv = FakeServer::start();
    for reserved in ["w.abc", "q.abc", "r.abc"] {
        let mut c = LobbyConn::start_lobby(
            &opts(&srv.base()),
            BUILD,
            &RoomName::parse_server_rules(reserved).unwrap(),
        )
        .unwrap();
        let t0 = Instant::now();
        let f = loop {
            if let Some(LobbyEvent::Failed(f)) = c.poll().into_iter().next() {
                break f;
            }
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(f, Failure::BadRoomName, "{reserved}");
    }
    srv.knobs.lobby_full.store(true, Ordering::Release);
    let mut c = LobbyConn::start_lobby(&opts(&srv.base()), BUILD, &room("full")).unwrap();
    let t0 = Instant::now();
    let f = loop {
        let mut out = None;
        for e in c.poll() {
            match e {
                LobbyEvent::List { .. } => assert!(c.create(&WatchName::parse("a").unwrap())),
                LobbyEvent::Failed(f) => out = Some(f),
                LobbyEvent::Created { .. } => panic!("上限なのに作れた"),
            }
        }
        if let Some(f) = out {
            break f;
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(f, Failure::ListFull, "list_full は件数の上限");
}
