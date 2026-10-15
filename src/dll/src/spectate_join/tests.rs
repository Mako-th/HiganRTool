//! 観戦者のルームの見張り（`spectate_join.rs`）の検査。ルーム名・名前・アドレス・秘密は合成の値だけ。
//!
//! 毎回の回: `Runner::step` を仮の時刻で直に回して、状態の書き方・取りやめ・つながらない・失敗の理由ごとの出来事を見る（ルームサーバーへ
//! 繋がない）。繋ぐ検査は loopback の偽サーバー（`net_room::fake`）と本物の WinHTTP で実時間に回すので `#[ignore]`（全件の回が
//! `--ignored` で走らせる）。外へは繋がない（`127.0.0.1` と `Proxy::Direct`）。
use super::*;

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::code::{Code, CodeError, Side};
use higanr_core::net_menu::Cmd;
use higanr_core::overlay::ui_glyphs;
use higanr_core::room::{Event as RoomEv, Role};
use higanr_core::spectate_room::JOINER_GIVE_UP_MS;

use crate::net_config::NetSettings;
use crate::net_room::fake::FakeServer;
use crate::net_room::{Base, SubmitRefused};
use crate::room_win::{Proxy, Timeouts};

/// 観戦用のルームの見張りが使う静的な値（`RUN`・`ACTIVE`）に触る検査は、1 本ずつ走らせる。
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn reset_globals() {
    RUN.store(0, Ordering::SeqCst);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 検査だけ: スレッドを起こさずに、「動いているルームのスレッド」を置く。数は `entered` を 1 にして返す。`TEST_LOCK` の下で、後で
/// `reset_globals`。
pub(crate) fn plant_active_for_test() -> Arc<JoinShared> {
    let shared = Arc::new(JoinShared::default());
    shared.bump(Stat::Entered);
    RUN.store(1, Ordering::SeqCst);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&shared));
    shared
}

fn room_opts(base: &str) -> RoomOptions {
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

fn settings() -> NetSettings {
    crate::net_config::from_text(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = viewer\ndelay = 3\n",
    )
    .unwrap()
    .unwrap()
}

const NAME: &str = "synth-join";
const SESSION: u32 = 0x5EC7_0C0D;
const SECRET: Secret = Secret(*b"join-tests-00001");

fn viewer_addr() -> SocketAddr {
    "127.0.0.1:40123".parse().unwrap()
}

fn caster_addr() -> SocketAddr {
    "127.0.0.1:40124".parse().unwrap()
}

struct Kit {
    runner: Runner,
    link: Arc<UiLink>,
    slot: Arc<WatchSlot>,
    shared: Arc<JoinShared>,
}

fn kit_parts(base: &str) -> (JoinKit, Arc<UiLink>, Arc<WatchSlot>) {
    let link = Arc::new(UiLink::new());
    let slot = Arc::new(WatchSlot::new());
    let glyphs = ui_glyphs();
    let publisher = Publisher::new(
        Arc::clone(&link),
        higanr_core::net_menu::sanitize_room(NAME, |c| glyphs.binary_search(&c).is_ok()),
        1_000,
    );
    let kit = JoinKit {
        source: JoinSource::Room {
            room: room_opts(base),
            watch_room: RoomName::parse(NAME).unwrap().spectate_of(),
        },
        reader: Reader::new(&settings(), 0x77),
        reply_name: Name::new("viewer"),
        own: Candidates::from_addrs(&[viewer_addr()]).unwrap(),
        slot: Arc::clone(&slot),
        publisher,
    };
    (kit, link, slot)
}

fn kit(base: &str) -> Kit {
    let (jk, link, slot) = kit_parts(base);
    let shared = Arc::new(JoinShared::default());
    Kit {
        runner: Runner::new(jk, Arc::clone(&shared)),
        link,
        slot,
        shared,
    }
}

/// 観戦コードの経路の材料（クリップボードを読んだ結果 `read`）。
fn code_kit(read: Result<Option<String>, String>) -> Kit {
    let (mut jk, link, slot) = kit_parts("ws://127.0.0.1:9");
    jk.source = JoinSource::Code(read);
    let shared = Arc::new(JoinShared::default());
    Kit {
        runner: Runner::new(jk, Arc::clone(&shared)),
        link,
        slot,
        shared,
    }
}

/// ホストが書く観戦コード（種別 3。候補は loopback の 1 つ）。`build` を替えると別のビルドのコード。
fn watch_code(expires: u64, build: u32) -> String {
    code::write_with_build(
        &Code {
            side: Side::Spectate,
            session_id: SESSION,
            secret: Some(SECRET),
            name: Name::new("caster"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: Candidates::from_addrs(&[caster_addr()]).unwrap(),
            expires,
        },
        build,
        None,
        Some(&[0x41; 32]),
    )
    .unwrap()
}

fn state_of(link: &UiLink) -> (CodeState, Event) {
    let n = link.note().expect("状態が書かれていない");
    (n.state, n.event)
}

fn units_text(units: &[u16]) -> String {
    units
        .iter()
        .take_while(|&&u| u != 0)
        .map(|&u| char::from_u32(u32::from(u)).unwrap_or('?'))
        .collect()
}

#[test]
fn the_publisher_writes_only_changes_and_continues_the_sequence() {
    let link = Arc::new(UiLink::new());
    link.publish(&Note {
        seq: 7,
        ..Note::default()
    });
    let mut p = Publisher::new(Arc::clone(&link), [b'a' as u16; ROOM_UNITS], 1_234);
    assert_eq!(link.note().unwrap().seq, 7, "書く前に札が動いた");
    p.publish(CodeState::SpectateJoining, Event::RoomJoining);
    let n = link.note().unwrap();
    assert_eq!(
        (n.seq, n.state, n.event, n.since, n.room[0]),
        (
            8,
            CodeState::SpectateJoining,
            Event::RoomJoining,
            1_234,
            b'a' as u16
        )
    );
    p.publish(CodeState::SpectateJoining, Event::RoomJoining);
    assert_eq!(link.note().unwrap().seq, 8, "同じ状態を書き直した");
    p.publish(CodeState::SpectateWaiting, Event::RoomWaiting);
    assert_eq!(link.note().unwrap().seq, 9);
    p.publish(CodeState::SpectateWaiting, Event::RoomJoining);
    p.publish(CodeState::Idle, Event::RoomJoining);
    assert_eq!(link.note().unwrap().seq, 11);
}

#[test]
fn a_cancel_ends_the_wait_before_any_connection_and_tells_the_game_to_give_up() {
    let mut k = kit("ws://127.0.0.1:9");
    k.link.send(Cmd::SpectateCancel);
    k.runner.step(0, 0);
    assert!(k.runner.is_done());
    assert!(k.runner.conn.is_none(), "取りやめより先に接続を開いた");
    assert_eq!(
        state_of(&k.link),
        (CodeState::Idle, Event::SpectateCancelled)
    );
    assert!(k.slot.gave_up(), "WatchPlay を終わらせていない");
    assert_eq!(k.shared.counts().cancelled, 1);
    k.link.send(Cmd::SpectateCancel);
    k.runner.step(10, 0);
    assert_eq!(
        state_of(&k.link),
        (CodeState::Idle, Event::SpectateCancelled)
    );
}

#[test]
fn once_watching_a_cancel_is_dropped_and_the_end_is_told() {
    let mut k = kit("ws://127.0.0.1:9");
    k.slot.mark_joined();
    k.runner.step(0, 0);
    assert_eq!(state_of(&k.link), (CodeState::SpectateIdle, Event::None));
    assert!(k.runner.conn.is_none());
    assert_eq!(k.shared.counts().linked, 1);
    k.link.send(Cmd::SpectateCancel);
    k.runner.step(10, 0);
    assert!(!k.runner.is_done(), "観戦中の取りやめで切った");
    assert_eq!(k.shared.counts().ignored_orders, 1);
    assert!(!k.slot.gave_up());
    k.slot.mark_done();
    k.runner.step(20, 0);
    assert!(k.runner.is_done());
    assert_eq!(state_of(&k.link), (CodeState::Idle, Event::SpectateEnded));
    assert_eq!(k.shared.counts().ended, 1);
    assert!(!k.slot.gave_up(), "終わりを取りやめと取り違えた");
}

/// 観戦コードの経路: 読めたら JOIN を始めさせ（ルームへは繋がない）、ホストの名前を左上の材料に載せる。答えが無ければ
/// `CODE_LINK_TIMEOUT_MS` で「ホストに接続できませんでした」で終える。
#[test]
fn a_watch_code_starts_the_join_without_a_room_and_gives_up_after_ten_seconds() {
    let now = 5_000_000u64;
    let mut k = code_kit(Ok(Some(format!(
        "\u{FEFF}{}\r\n",
        watch_code(now + 1_800, higanr_core::update::BUILD_NUMBER)
    ))));
    k.runner.step(1_000, now);
    assert_ne!(k.slot.generation(), 0, "観戦者の設定を置いていない");
    assert!(k.runner.conn.is_none(), "観戦コードの経路がルームへ繋いだ");
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateLinking, Event::SpectatePeer)
    );
    assert_eq!(units_text(&k.link.note().unwrap().name), "caster");
    let c = k.shared.counts();
    assert_eq!(
        (c.by_code, c.codes_received, c.replies_sent, c.entered),
        (1, 1, 0, 0),
        "{c:?}"
    );
    k.runner.step(1_000 + CODE_LINK_TIMEOUT_MS - 1, now);
    assert!(!k.runner.is_done(), "上限の手前で諦めた");
    k.runner.step(1_000 + CODE_LINK_TIMEOUT_MS, now);
    assert!(k.runner.is_done());
    assert_eq!(
        state_of(&k.link),
        (CodeState::Idle, Event::SpectateCodeNoLink)
    );
    assert!(k.slot.gave_up());
    assert_eq!(k.shared.counts().link_timeout, 1);
    assert!(k.shared.line().contains("観戦コードでホストへ直に繋ぐ"));
}

/// 期限の切れた観戦コード: 時計のずれを見込んで繋ぎに行き、答えが無ければ期限切れの文で終える。答えが来れば観戦になる。
#[test]
fn an_expired_watch_code_still_tries_and_names_the_expiry_only_when_nobody_answers() {
    let now = 5_000_000u64;
    let text = watch_code(now - 1, higanr_core::update::BUILD_NUMBER);
    let mut k = code_kit(Ok(Some(text.clone())));
    k.runner.step(0, now);
    assert_ne!(k.slot.generation(), 0, "期限切れで繋ぎに行かなかった");
    assert_eq!(k.shared.counts().expired_but_tried, 1);
    k.runner.step(CODE_LINK_TIMEOUT_MS, now);
    assert_eq!(
        state_of(&k.link),
        (CodeState::Idle, Event::SpectateCodeExpired)
    );
    let mut k = code_kit(Ok(Some(text)));
    k.runner.step(0, now);
    k.slot.mark_joined();
    k.runner.step(50, now);
    assert_eq!(state_of(&k.link), (CodeState::SpectateIdle, Event::None));
    assert!(!k.runner.is_done());
}

/// 観戦コードとして使えないものは、JOIN を始めずに理由ごとの出来事で終える（クリップボードの空・読めない・対戦のコード・壊れた・別のビルド・範囲の外）。
#[test]
fn an_unusable_watch_code_ends_with_its_own_event_and_never_joins() {
    let now = 5_000_000u64;
    let initiator = code::write_initiator(&Code {
        side: Side::Initiator,
        session_id: SESSION,
        secret: Some(SECRET),
        name: Name::new("player"),
        nat: higanr_core::nat::NatMark::UNKNOWN,
        candidates: Candidates::from_addrs(&[caster_addr()]).unwrap(),
        expires: now + 1_800,
    })
    .unwrap();
    let good = watch_code(now + 1_800, higanr_core::update::BUILD_NUMBER);
    let mut broken: Vec<char> = good.chars().collect();
    let mid = broken.len() / 2;
    broken[mid] = if broken[mid] == 'A' { 'B' } else { 'A' };
    let broken: String = broken.into_iter().collect();
    let far = {
        let c = Code {
            side: Side::Spectate,
            session_id: SESSION,
            secret: Some(SECRET),
            name: Name::new("caster"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: Candidates::from_addrs(&["203.0.113.5:2000".parse().unwrap()]).unwrap(),
            expires: now + 1_800,
        };
        code::write_spectate(&c, &[0x41; 32]).unwrap()
    };
    let other_build = higanr_core::update::BUILD_NUMBER.wrapping_add(1);
    type ClipRead = Result<Option<String>, String>;
    let cases: [(&str, ClipRead, Event); 7] = [
        ("空", Ok(None), Event::SpectateCodeEmpty),
        (
            "空白だけ",
            Ok(Some(" \r\n".to_string())),
            Event::SpectateCodeEmpty,
        ),
        (
            "読めない",
            Err("偽のクリップボードを読めない".to_string()),
            Event::Unreadable,
        ),
        (
            "対戦のコード",
            Ok(Some(initiator)),
            Event::SpectateCodeWrongKind,
        ),
        ("壊れた", Ok(Some(broken)), Event::SpectateCodeBroken),
        (
            "別のビルド",
            Ok(Some(watch_code(now + 1_800, other_build))),
            Event::SpectateOtherBuild,
        ),
        // 検査の ini の範囲（既定）の外の候補だけ。
        ("範囲の外", Ok(Some(far)), Event::NoRoute),
    ];
    for (name, read, want) in cases {
        let mut k = code_kit(read);
        k.runner.step(0, now);
        assert!(k.runner.is_done(), "{name}: 終わっていない");
        assert_eq!(state_of(&k.link), (CodeState::Idle, want), "{name}");
        assert_eq!(k.slot.generation(), 0, "{name}: JOIN を始めた");
        assert!(k.slot.gave_up(), "{name}: WatchPlay を終わらせていない");
        if want == Event::SpectateOtherBuild {
            assert_eq!(k.link.note().unwrap().peer_build, other_build);
        }
    }
}

/// 観戦コードの経路でも、つなぎに行く間は「観戦をやめる」で取りやめられる。
#[test]
fn a_watch_code_link_can_be_cancelled() {
    let now = 5_000_000u64;
    let mut k = code_kit(Ok(Some(watch_code(
        now + 1_800,
        higanr_core::update::BUILD_NUMBER,
    ))));
    k.link.send(Cmd::SpectateCode);
    k.runner.step(0, now);
    assert!(!k.runner.is_done());
    k.link.send(Cmd::SpectateCancel);
    k.runner.step(100, now);
    assert!(k.runner.is_done());
    assert_eq!(
        state_of(&k.link),
        (CodeState::Idle, Event::SpectateCancelled)
    );
    assert_eq!(k.shared.counts().ignored_orders, 0, "起こした指示を数えた");
}

#[test]
fn a_link_that_never_answers_ends_as_no_link_at_the_timeout() {
    let mut k = kit("ws://127.0.0.1:9");
    let _ = k.runner.joiner.stop();
    k.runner.replied_at = Some(5_000);
    k.runner.step(5_000 + LINK_TIMEOUT_MS - 1, 0);
    assert!(!k.runner.is_done(), "上限の手前で諦めた");
    assert!(!k.slot.gave_up());
    k.runner.step(5_000 + LINK_TIMEOUT_MS, 0);
    assert!(k.runner.is_done());
    assert_eq!(state_of(&k.link), (CodeState::Idle, Event::SpectateNoLink));
    assert!(k.slot.gave_up());
    assert_eq!(k.shared.counts().link_timeout, 1);
}

#[test]
fn the_screen_state_follows_the_joiner() {
    let mut k = kit("ws://127.0.0.1:9");
    let r = &mut k.runner;
    assert_eq!(r.joiner.poll(0), Some(JoinerAction::Connect));
    r.refresh(0);
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateJoining, Event::RoomJoining)
    );
    assert_eq!(
        r.joiner.on_joined(0, Role::Offer),
        Some(JoinerAction::Close)
    );
    r.refresh(0);
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateWaiting, Event::RoomWaiting)
    );
    assert_eq!(r.joiner.poll(12_000), Some(JoinerAction::Connect));
    r.refresh(12_000);
    assert_eq!(state_of(&k.link).0, CodeState::SpectateWaiting);
    assert_eq!(r.joiner.on_joined(12_100, Role::Answer), None);
    r.refresh(12_100);
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateJoining, Event::RoomJoining)
    );
    assert_eq!(
        r.joiner.on_peer_code("x".to_string()),
        Some(JoinerAction::SpectateCode("x".to_string()))
    );
    r.joiner.on_answer_sent();
    r.refresh(12_200);
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateLinking, Event::SpectatePeer)
    );
    r.joiner.on_done();
    r.refresh(12_300);
    assert_eq!(r.phase, Phase::Linking);
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateLinking, Event::SpectatePeer)
    );
    assert!(!k.slot.gave_up(), "つなぎに行く段階で終わらせた");
}

#[test]
fn every_failure_ends_with_its_own_event_and_gives_the_game_up() {
    use higanr_core::room::{Failure, Why};
    let in_room = |k: &mut Kit| {
        assert_eq!(k.runner.joiner.poll(0), Some(JoinerAction::Connect));
        assert_eq!(k.runner.joiner.on_joined(0, Role::Answer), None);
    };
    type Setup = fn(&mut Runner);
    let cases: [(&str, Setup, Event, u8); 5] = [
        (
            "応答しない",
            |r| r.joiner.on_failed(5, Failure::Unresponsive(Why::NoAnswer)),
            Event::SpectateUnresponsive,
            0,
        ),
        (
            "400",
            |r| r.joiner.on_failed(5, Failure::BadRoomName),
            Event::SpectateRefused,
            0,
        ),
        (
            "バージョン違い",
            |r| {
                r.abort(
                    JoinerFailure::CodeUnreadable(CodeError::BadVersion(9)),
                    Stat::CodeUnreadable,
                )
            },
            Event::SpectateOtherVersion,
            9,
        ),
        (
            "コードを読めない",
            |r| {
                r.abort(
                    JoinerFailure::CodeUnreadable(CodeError::BadCheck),
                    Stat::CodeUnreadable,
                )
            },
            Event::SpectateBadCode,
            0,
        ),
        (
            "返せない",
            |r| r.abort(JoinerFailure::ReplyNotSent, Stat::ReplyFailed),
            Event::SpectateNoLink,
            0,
        ),
    ];
    for (name, setup, want, ver) in cases {
        let mut k = kit("ws://127.0.0.1:9");
        in_room(&mut k);
        setup(&mut k.runner);
        k.runner.refresh(10);
        assert!(k.runner.is_done(), "{name}: 終わっていない");
        assert_eq!(state_of(&k.link), (CodeState::Idle, want), "{name}");
        assert_eq!(
            k.link.note().unwrap().peer_ver,
            ver,
            "{name}: 相手のバージョン"
        );
        assert!(k.slot.gave_up(), "{name}: WatchPlay を終わらせていない");
    }
    let mut k = kit("ws://127.0.0.1:9");
    k.runner.step(JOINER_GIVE_UP_MS, 0);
    assert!(k.runner.is_done());
    assert_eq!(state_of(&k.link), (CodeState::Idle, Event::SpectateGaveUp));
    assert!(k.slot.gave_up());
    assert_eq!(k.shared.counts().gave_up, 1);
}

#[test]
fn the_counts_text_counts_each_kind_once_and_holds_no_name() {
    use higanr_core::room::{Failure, Why};
    let mut k = kit("ws://127.0.0.1:9");
    let r = &mut k.runner;
    r.on_event(
        RoomEv::Joined {
            role: Role::Offer,
            expires_at: 0,
        },
        0,
        0,
    );
    assert_eq!(k.shared.counts().entered, 1);
    assert_eq!(r.joiner.poll(0), Some(JoinerAction::Connect));
    r.on_event(
        RoomEv::Joined {
            role: Role::Offer,
            expires_at: 0,
        },
        1,
        0,
    );
    assert_eq!(
        (k.shared.counts().entered, k.shared.counts().wrong_role),
        (2, 1)
    );
    assert_eq!(r.joiner.poll(13_000), Some(JoinerAction::Connect));
    r.on_event(
        RoomEv::Failed(Failure::Unresponsive(Why::NoAnswer)),
        13_001,
        0,
    );
    r.refresh(13_001);
    let c = k.shared.counts();
    assert_eq!(c.fail_unresponsive, 1, "二重に数えた: {c:?}");
    let t = c.text();
    assert!(t.contains("入った 2（役が逆で閉じて入り直した 1）"), "{t}");
    assert!(!t.to_lowercase().contains(NAME), "{t}");
    assert!(!t.contains("127.0.0.1"), "{t}");
}

#[test]
fn a_transport_failure_reaches_the_log_line_as_text_without_the_room_name() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut k = kit(&format!("ws://127.0.0.1:{port}"));
    let t0 = Instant::now();
    while k.shared.counts().transport == 0 {
        assert!(t0.elapsed() < Duration::from_secs(10), "運びの記録が出ない");
        k.runner.step(t0.elapsed().as_millis() as u64, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    let line = k.shared.line();
    assert!(line.contains("運びの記録の文 [OpenFailed"), "{line}");
    assert!(!line.to_lowercase().contains(NAME), "{line}");
    assert!(!line.contains(&port.to_string()), "{line}");
    k.shared.note_transport(
        (0..NOTES_MAX as u32 + 5)
            .map(TransportNote::RecvError)
            .collect(),
    );
    let kept = k.shared.notes.lock().unwrap().len();
    assert_eq!(kept, NOTES_MAX);
    assert!(k.shared.counts().transport > NOTES_MAX as u32);
}

#[test]
fn the_thread_ends_on_a_cancel_and_the_counts_survive_the_cleanup() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_globals();
    assert!(!finished(), "起こす前から終わっている");
    assert_eq!(stats_text(), "");
    assert_eq!(active_line(), None, "起こす前から行がある");
    assert_eq!(stop_and_join(), None, "起こしていないのに行を返した");
    let (jk, link, slot) = kit_parts("ws://127.0.0.1:9");
    link.send(Cmd::SpectateCancel);
    let shared = spawn(jk).expect("起こせない");
    let t0 = Instant::now();
    while !finished() {
        assert!(t0.elapsed() < Duration::from_secs(5), "終わらない");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(shared.is_finished());
    assert!(slot.gave_up());
    assert_eq!(state_of(&link), (CodeState::Idle, Event::SpectateCancelled));
    assert!(stats_text().contains("取りやめた 1"), "{}", stats_text());
    assert!(active_line().is_some_and(|l| l.contains("取りやめた 1")));
    let line = stop_and_join().expect("起こしたのに行を返さない");
    assert!(!finished(), "片づけた後も終わった印が残る");
    // 最後の数は返り値で呼び手へ渡り（次の観戦で上書きされない）、ここには残らない（残すと節と全文で重なる）。
    assert!(line.contains("取りやめた 1"), "{line}");
    assert_eq!(stats_text(), "", "片づけた後も数の行が残る");
    assert_eq!(active_line(), None);
    reset_globals();
}

// ---- ルームサーバー（偽）越し ----

struct FakeCaster {
    conn: RoomConn,
    offered: bool,
    replies: Vec<String>,
    done: bool,
}

impl FakeCaster {
    /// 観戦用のルームへ入り、役 host を取れるまで入り直す（観戦者が閉じた直後は、サーバーが前の接続を片づける前に役 guest になりうる）。
    fn enter(base: &str, spec_room: &RoomName, secret: &Secret, unix: u64) -> FakeCaster {
        let t0 = Instant::now();
        loop {
            let mut conn = RoomConn::start(&room_opts(base), spec_room).unwrap();
            let t1 = Instant::now();
            let role = loop {
                if let Some(e) = conn.poll().into_iter().find_map(|e| match e {
                    RoomEv::Joined { role, .. } => Some(role),
                    _ => None,
                }) {
                    break Some(e);
                }
                if t1.elapsed() > Duration::from_secs(3) {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            if role == Some(Role::Offer) {
                let text = code::write_spectate(
                    &Code {
                        side: Side::Spectate,
                        session_id: SESSION,
                        secret: Some(*secret),
                        name: Name::new("caster"),
                        nat: higanr_core::nat::NatMark::UNKNOWN,
                        candidates: Candidates::from_addrs(&[caster_addr()]).unwrap(),
                        expires: unix + 1_800,
                    },
                    // 配る側の公開鍵（バージョン 5。この検査は鍵の交換まで進まないので、どの値でもよい）。
                    &[0x41; 32],
                )
                .unwrap();
                conn.submit_code(&text).unwrap();
                return FakeCaster {
                    conn,
                    offered: true,
                    replies: Vec::new(),
                    done: false,
                };
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "役 host を取れない");
            drop(conn);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn poll(&mut self) {
        for e in self.conn.poll() {
            match e {
                RoomEv::PeerCode(t) => self.replies.push(t),
                RoomEv::Done => self.done = true,
                _ => {}
            }
        }
    }
}

fn unix() -> u64 {
    crate::net_udp::now_unix()
}

#[test]
#[ignore = "偽サーバーと本物の WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_spectator_that_arrives_first_closes_retries_then_meets_the_caster_and_replies() {
    let srv = FakeServer::start();
    let base = srv.base();
    let mut k = kit(&base);
    let spec_room = RoomName::parse(NAME).unwrap().spectate_of();
    let mut now = 0u64;
    let t0 = Instant::now();
    let tick = |k: &mut Kit, caster: Option<&mut FakeCaster>, now: &mut u64| {
        k.runner.step(*now, unix());
        if let Some(c) = caster {
            c.poll();
        }
        *now += 10;
        std::thread::sleep(Duration::from_millis(10));
        assert!(t0.elapsed() < Duration::from_secs(30), "終わらない");
    };
    while k.shared.counts().wrong_role == 0 {
        tick(&mut k, None, &mut now);
    }
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateWaiting, Event::RoomWaiting)
    );
    assert_eq!(k.slot.generation(), 0);
    let secret = SECRET;
    let mut caster = FakeCaster::enter(&base, &spec_room, &secret, unix());
    assert!(caster.offered);
    now += 12_000;
    while k.shared.counts().replies_sent == 0 {
        tick(&mut k, Some(&mut caster), &mut now);
    }
    assert_eq!(k.slot.generation(), 1, "WatchSlot へ設定を置いていない");
    let c = k.shared.counts();
    assert_eq!((c.codes_received, c.wrong_role), (1, 1), "{c:?}");
    while k.runner.phase != Phase::Linking || caster.replies.is_empty() {
        tick(&mut k, Some(&mut caster), &mut now);
    }
    assert_eq!(
        state_of(&k.link),
        (CodeState::SpectateLinking, Event::SpectatePeer)
    );
    assert_eq!(
        units_text(&k.link.note().unwrap().name),
        "caster",
        "対戦者の名前（観戦用のコードの名）が状態に載っていない"
    );
    let reply = code::read_spectate_reply(&caster.replies[0], &secret, SESSION, unix())
        .expect("配る側が返しを読めない（タグ・session・期限）");
    assert_eq!(reply.candidates.as_slice().len(), 1);
    assert_eq!(
        reply.candidates.as_slice()[0].addr,
        viewer_addr(),
        "返しの候補が観戦者の受信ソケットでない"
    );
    assert_eq!(reply.name, Name::new("viewer"));
    k.slot.mark_joined();
    tick(&mut k, Some(&mut caster), &mut now);
    assert_eq!(state_of(&k.link), (CodeState::SpectateIdle, Event::None));
    assert!(k.runner.conn.is_none());
    k.slot.mark_done();
    tick(&mut k, None, &mut now);
    assert!(k.runner.is_done());
    assert_eq!(state_of(&k.link), (CodeState::Idle, Event::SpectateEnded));
    let t = k.shared.counts().text();
    assert!(!t.to_lowercase().contains(NAME), "{t}");
    let want = format!("/v0/rooms/{}", spec_room.as_str());
    let paths = srv.seen.paths.lock().unwrap().clone();
    assert!(
        !paths.is_empty() && paths.iter().all(|p| p.starts_with(&want)),
        "{paths:?}"
    );
}

#[test]
#[ignore = "偽サーバーと本物の WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_code_that_is_not_a_spectate_code_ends_as_a_bad_code_and_sends_no_reply() {
    let srv = FakeServer::start();
    let base = srv.base();
    let mut k = kit(&base);
    let spec_room = RoomName::parse(NAME).unwrap().spectate_of();
    let t = Instant::now();
    let mut caster = loop {
        let mut conn = RoomConn::start(&room_opts(&base), &spec_room).unwrap();
        let t1 = Instant::now();
        let mut role = None;
        while role.is_none() && t1.elapsed() < Duration::from_secs(3) {
            role = conn.poll().into_iter().find_map(|e| match e {
                RoomEv::Joined { role, .. } => Some(role),
                _ => None,
            });
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(role, Some(Role::Offer), "役 host を取れない");
        let wrong = code::write_initiator(&Code {
            side: Side::Initiator,
            session_id: SESSION,
            secret: Some(SECRET),
            name: Name::new("caster"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: Candidates::from_addrs(&[caster_addr()]).unwrap(),
            expires: unix() + 1_800,
        })
        .unwrap();
        match conn.submit_code(&wrong) {
            Ok(()) => break conn,
            Err(SubmitRefused::NotSent) | Err(SubmitRefused::Core(_)) => {
                assert!(t.elapsed() < Duration::from_secs(5));
            }
        }
    };
    let mut now = 0u64;
    while !k.runner.is_done() {
        k.runner.step(now, unix());
        let _ = caster.poll();
        now += 10;
        std::thread::sleep(Duration::from_millis(10));
        assert!(t.elapsed() < Duration::from_secs(20), "終わらない");
    }
    assert_eq!(state_of(&k.link), (CodeState::Idle, Event::SpectateBadCode));
    assert_eq!(k.slot.generation(), 0, "読めないコードで設定を置いた");
    assert!(k.slot.gave_up());
    let c = k.shared.counts();
    assert_eq!(
        (c.codes_received, c.code_unreadable, c.replies_sent),
        (1, 1, 0),
        "{c:?}"
    );
}
