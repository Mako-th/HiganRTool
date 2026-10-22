//! 観戦者へ断りを配る側（`spectate_refuse.rs`）の検査。ルーム名は合成の値だけ。接続する検査は loopback の偽サーバー（`net_room::fake`）と
//! 本物の WinHTTP で実時間に回すので `#[ignore]`（全件の回が `--ignored` で走らせる）。外へは接続しない（`127.0.0.1` と `Proxy::Direct`）。
use super::*;

use std::sync::atomic::Ordering;

use crate::net_room::Base;
use crate::net_room::fake::FakeServer;
use crate::room_win::{Proxy, Timeouts};

pub(crate) fn room_opts(base: &str) -> RoomOptions {
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

const NAME: &str = "synth-refuse";

#[test]
fn the_counts_text_names_each_count_and_no_room() {
    let c = RefuseCounts {
        entered: 3,
        refused: 2,
        wrong_role: 1,
        failed: 4,
        connect_errors: 5,
        panicked: 0,
    };
    let t = c.text();
    assert!(
        t.contains("入った 3（役が逆で閉じた 1）・拒否を送った 2・ルームの失敗 4・接続不能 5"),
        "{t}"
    );
    assert!(!t.contains("パニック"), "{t}");
    let p = RefuseCounts { panicked: 1, ..c }.text();
    assert!(p.contains("パニックで停止"), "{p}");
}

#[test]
fn dropping_the_refuser_stops_its_thread_even_when_the_server_is_unreachable() {
    let r = spawn(
        room_opts("ws://127.0.0.1:9"),
        &RoomName::parse(NAME).unwrap(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // 止まらないと join が返らないので、別のスレッドで手放して時間の上限で待つ。
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(r);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(2)).is_ok(),
        "2 秒で止まらない"
    );
}

#[test]
#[ignore = "偽サーバーと本物の WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_refuse_that_cannot_be_sent_ends_the_entry_and_the_caster_enters_again_later() {
    use higanr_core::spectate_room::CasterPhase;
    let srv = FakeServer::start();
    let shared = Arc::new(Shared::default());
    let spec_room = RoomName::parse(NAME).unwrap().spectate_of();
    let mut r = Runner::new(room_opts(&srv.base()), spec_room, Arc::clone(&shared));
    r.step(0);
    r.conn.as_mut().expect("入っていない").fail_sends_for_test();
    let t0 = Instant::now();
    let mut now = 0;
    while shared.counts().failed == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "送りの失敗が届かない: {:?}",
            shared.counts()
        );
        now = u64::try_from(t0.elapsed().as_millis()).unwrap();
        r.step(now);
        std::thread::sleep(Duration::from_millis(10));
    }
    let c = shared.counts();
    assert_eq!((c.entered, c.refused), (1, 0), "{c:?}");
    assert!(r.conn.is_none());
    let s = r.caster.status(now);
    assert_eq!(
        s.phase,
        CasterPhase::Waiting,
        "入り直しを待っていない: {s:?}"
    );
    assert!(s.next_entry_at.is_some(), "{s:?}");
    r.close();
}

#[test]
#[ignore = "偽サーバーと本物の WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_refusing_host_sends_refuse_and_enters_again_after_the_room_expires() {
    let srv = FakeServer::start();
    srv.knobs.room_ttl_ms.store(600, Ordering::Release);
    let shared = Arc::new(Shared::default());
    let spec_room = RoomName::parse(NAME).unwrap().spectate_of();
    let mut r = Runner::new(room_opts(&srv.base()), spec_room, Arc::clone(&shared));
    let t0 = Instant::now();
    while shared.counts().refused < 2 {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "入り直して断らない: {:?}",
            shared.counts()
        );
        r.step(u64::try_from(t0.elapsed().as_millis()).unwrap());
        std::thread::sleep(Duration::from_millis(10));
    }
    r.close();
    let c = shared.counts();
    assert!(c.entered >= 2 && c.failed >= 1, "{c:?}");
    assert_eq!(c.connect_errors, 0, "{c:?}");
    let refuses = srv.texts_matching(|t| t == higanr_core::room::REFUSE_TEXT);
    let offers = srv.texts_matching(|t| t.contains(r#""t":"offer""#));
    assert!(refuses >= 2, "{refuses}");
    assert_eq!(offers, 0, "断る側が offer を出した");
    let want = format!("/v0/rooms/{}", spec_room.as_str());
    let paths = srv.seen.paths.lock().unwrap().clone();
    assert!(
        !paths.is_empty() && paths.iter().all(|p| p.starts_with(&want)),
        "観戦用のルームでないところへ入った: {paths:?}"
    );
}
