//! 試合中の1件を載せるスレッド（`lobby_post.rs`）の検査。偽のルームサーバー（loopback）と本物のWinHTTPで実時間に回すものは`#[ignore]`。
//! 外へは接続しない（`127.0.0.1`と`Proxy::Direct`）。名前・ルーム名は合成の値だけ。
use super::*;

use higanr_core::lobby::{LobbyEvent, LobbyRoom};
use higanr_core::room::RoomName;
use higanr_core::watch_list::WatchName;
use higanr_core::wire::Mode;

use crate::net_room::fake::FakeServer;
use crate::net_room::{Base, LobbyConn};
use crate::room_win::{Proxy, Timeouts};

const BUILD: u32 = 13;

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

/// 一覧へ`create`して待機中の1件として載り、割り当てのルーム名を返す（作った人の代わり。接続は持ち続ける）。
fn create(srv: &FakeServer, name: &str) -> (LobbyConn, LobbyRoom) {
    let mut c =
        LobbyConn::start_lobby(&opts(&srv.base()), BUILD, &RoomName::parse(name).unwrap()).unwrap();
    let t0 = Instant::now();
    loop {
        for e in c.poll() {
            match e {
                LobbyEvent::List { .. } => {
                    c.create(&WatchName::parse("alice").unwrap());
                }
                LobbyEvent::Created { room } => return (c, room),
                LobbyEvent::Failed(f) => panic!("作れない: {f:?}"),
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "載らない");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn posting(srv: &FakeServer, room: LobbyRoom, watch: bool) -> LobbyPosting {
    LobbyPosting {
        room,
        p1: WatchName::parse("alice").unwrap(),
        p2: WatchName::parse("bob").unwrap(),
        mode: Mode::Rollback,
        delay: 1,
        watch,
        cap: 10,
        ticket: srv.mint_ticket(&room.name().spectate_of()),
    }
}

fn wait_rows(srv: &FakeServer, name: &str, what: &str, want: &[&str]) {
    let t0 = Instant::now();
    while srv.lobby_rows(BUILD, name) != want {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "5 秒の内に {what} にならない: {:?}",
            srv.lobby_rows(BUILD, name)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_pair_without_watching_is_listed_with_watch_false_and_replaces_the_waiting_row() {
    let srv = FakeServer::start();
    let (mut waiting, room) = create(&srv, "synth-p");
    wait_rows(&srv, "synth-p", "待機中の 1 件", &["wait:alice"]);
    let p = spawn(
        opts(&srv.base()),
        BUILD,
        RoomName::parse("Synth-P").unwrap(),
        posting(&srv, room, false),
        None,
    )
    .unwrap();
    wait_rows(
        &srv,
        "synth-p",
        "試合中（観戦不可）の 1 件",
        &["play:alice:bob:false:0"],
    );
    // 作った人の待機中の接続はサーバーが閉じる。
    let t0 = Instant::now();
    while !waiting.is_finished() {
        waiting.poll();
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "待機中の接続が閉じない"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let posts = srv.seen.lobby_posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].contains(r#""watch":false"#), "{}", posts[0]);
    let t0 = Instant::now();
    while p.counts().listed == 0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "listed を受けない");
        std::thread::sleep(Duration::from_millis(10));
    }
    let c = p.finish();
    assert_eq!((c.listed, c.failed, c.gave_up), (1, 0, false), "{c:?}");
    // 止めたら一覧から消える。
    wait_rows(&srv, "synth-p", "0 件", &[]);
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_watchable_pair_is_listed_with_watch_true_and_a_full_list_is_not_retried() {
    let srv = FakeServer::start();
    let (_waiting, room) = create(&srv, "synth-q");
    let p = spawn(
        opts(&srv.base()),
        BUILD,
        RoomName::parse("synth-q").unwrap(),
        posting(&srv, room, true),
        None,
    )
    .unwrap();
    wait_rows(&srv, "synth-q", "試合中の 1 件", &["play:alice:bob:true:0"]);
    drop(p);
    wait_rows(&srv, "synth-q", "0 件", &[]);
    // 件数の上限で断られたら載せ直さない（1回だけ接続する）。
    srv.knobs.lobby_full.store(true, Ordering::Release);
    let ups = srv.seen.lobby_upgrades.load(Ordering::Acquire);
    let p = spawn(
        opts(&srv.base()),
        BUILD,
        RoomName::parse("synth-q").unwrap(),
        posting(&srv, room, true),
        None,
    )
    .unwrap();
    let t0 = Instant::now();
    while !p.counts().gave_up {
        assert!(t0.elapsed() < Duration::from_secs(5), "上限で諦めない");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        srv.seen.lobby_upgrades.load(Ordering::Acquire),
        ups + 1,
        "上限の後に載せ直した"
    );
    let c = p.finish();
    assert_eq!((c.listed, c.failed, c.gave_up), (0, 1, true));
    assert!(c.text().contains("載せ直さない"), "{}", c.text());
}

#[test]
#[ignore = "偽サーバーと本物のWinHTTP（loopback）で実時間に回す（1 秒ほど）。全件の回（build.bat）で --ignored で走る"]
fn a_busy_list_is_relisted_after_the_delay_instead_of_given_up() {
    let srv = FakeServer::start();
    let (_waiting, room) = create(&srv, "synth-busy");
    // 見る接続の上限（409 `room_full`）。件数の上限（closeの`list_full`）とは別の一時的な断り。
    *srv.knobs.forced.lock().unwrap() = Some((409, r#"{"error":"room_full"}"#.to_owned()));
    let shared = Arc::new(Shared::default());
    let mut r = Runner::new(
        opts(&srv.base()),
        BUILD,
        RoomName::parse("synth-busy").unwrap(),
        posting(&srv, room, true),
        None,
        Arc::clone(&shared),
    );
    let t0 = Instant::now();
    while shared.counts().failed == 0 {
        r.step(0);
        assert!(t0.elapsed() < Duration::from_secs(5), "断りが届かない");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!shared.counts().gave_up, "見る接続の上限で載せ直しをやめた");
    assert!(r.conn.is_none());
    let retry = r.retry_at_ms;
    assert_eq!(retry, relist_delay(1));
    *srv.knobs.forced.lock().unwrap() = None;
    r.step(retry - 1);
    assert!(r.conn.is_none(), "間隔を空けずに載せ直した");
    let t0 = Instant::now();
    while shared.counts().listed == 0 {
        r.step(retry);
        assert!(t0.elapsed() < Duration::from_secs(5), "載せ直さない");
        std::thread::sleep(Duration::from_millis(10));
    }
    wait_rows(
        &srv,
        "synth-busy",
        "試合中の 1 件",
        &["play:alice:bob:true:0"],
    );
    r.close();
}

#[test]
#[ignore = "偽サーバーと本物のスレッド・WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_watcher_count_follows_the_spectators_up_to_the_cap() {
    let srv = FakeServer::start();
    let (_waiting, room) = create(&srv, "synth-w");
    let host = Arc::new(spectate_host::Shared::default());
    host.set_spectators(2);
    let p = spawn(
        opts(&srv.base()),
        BUILD,
        RoomName::parse("synth-w").unwrap(),
        posting(&srv, room, true),
        Some(Arc::clone(&host)),
    )
    .unwrap();
    wait_rows(&srv, "synth-w", "観戦者 2", &["play:alice:bob:true:2"]);
    host.set_spectators(5);
    wait_rows(&srv, "synth-w", "観戦者 5", &["play:alice:bob:true:5"]);
    // 上限（cap 10）を越える数は上限で送る。
    host.set_spectators(50);
    wait_rows(&srv, "synth-w", "観戦者 10", &["play:alice:bob:true:10"]);
    assert_eq!(
        srv.seen.lobby_posts.lock().unwrap().len(),
        1,
        "数の変化で載せ直した"
    );
    let c = p.finish();
    assert_eq!((c.listed, c.failed), (1, 0), "{c:?}");
}

#[test]
fn the_counts_line_says_when_relisting_stopped() {
    let mut c = PostCounts {
        listed: 2,
        failed: 1,
        connect_errors: 3,
        gave_up: false,
        panicked: false,
    };
    let t = c.text();
    assert!(
        t.contains("載った 2") && t.contains("載せ不能 1") && t.contains("接続できなかった 3"),
        "{t}"
    );
    assert!(
        !t.contains("載せ直さない") && !t.contains("異常終了"),
        "{t}"
    );
    c.gave_up = true;
    c.panicked = true;
    let t = c.text();
    assert!(t.contains("載せ直さない") && t.contains("異常終了"), "{t}");
}
