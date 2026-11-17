use super::*;
use crate::room::{JsonValue as V, parse_flat};
use crate::watch_list::{HostAction, HostClient, HostEvent, HostState, WatchName, write_post};

const RULES: &str = include_str!("../../../../tests/fixtures/room/rules.json");
const ROOM: &str = "r.0123456789abcdef0123456789abcd";
const TICKET: &str = "1790000000.1790086400.Zq3-_xZq3-_xZq3-_xZq3-_xZq3-_xZq3-_xZq3-_x0";

fn t(ms: u64) -> Now {
    Now {
        ms,
        unix_secs: 1_790_000_000 + ms / 1000,
    }
}

fn room() -> LobbyRoom {
    LobbyRoom::parse(ROOM).unwrap()
}

fn name(s: &str) -> WatchName {
    WatchName::parse(s).unwrap()
}

fn lobby_posting(watch: bool, cap: u8) -> LobbyPosting {
    LobbyPosting {
        room: room(),
        p1: name("Alice"),
        p2: name("Bob"),
        mode: Mode::Rollback,
        delay: 1,
        watch,
        cap,
        ticket: SeatTicket::parse(TICKET).unwrap(),
    }
}

fn wait_json(n: &str, room: &str) -> String {
    format!(r#"{{"kind":"wait","name":"{n}","room":"{room}"}}"#)
}

fn play_json(watch: &str, watchers: i64, cap: i64) -> String {
    format!(
        r#"{{"kind":"play","p1":"Alice","p2":"Bob","room":"w.0123456789abcdef","watch":{watch},"watchers":{watchers},"cap":{cap},"mode":"rollback","delay":1}}"#
    )
}

fn list_json(rows: &[String]) -> String {
    format!(r#"{{"t":"list","items":[{}]}}"#, rows.join(","))
}

fn sends<'a, T>(v: &'a [T], f: impl Fn(&'a T) -> Option<&'a str>) -> Vec<&'a str> {
    v.iter().filter_map(f).collect()
}

// ---------------- 見本との突き合わせ ----------------

#[test]
fn constants_match_the_lobby_keys_of_the_rules_fixture() {
    let list = parse_flat(RULES).expect("rules.json は平らな JSON");
    let mut seen = 0;
    for (k, v) in &list {
        if !k.starts_with("lobby_") {
            continue;
        }
        seen += 1;
        match (k.as_ref(), v) {
            ("lobby_path_prefix", V::Str(s)) => assert_eq!(s, LOBBY_PATH_PREFIX),
            ("lobby_room_prefix", V::Str(s)) => assert_eq!(s, LOBBY_ROOM_PREFIX),
            ("lobby_room_hex_len", V::Int(n)) => assert_eq!(*n as usize, LOBBY_ROOM_HEX_LEN),
            ("lobby_max_total", V::Int(n)) => assert_eq!(*n as usize, LOBBY_ROWS_MAX),
            ("lobby_max_per_key", V::Int(n)) => assert_eq!(
                *n, 2,
                "クライアントは同じ鍵の 2 件の上限を使わない（待機中は 1 件・載せるのはマッチ 1 つに 1 件）"
            ),
            ("lobby_view_max_total", V::Int(n)) => assert_eq!(*n, 50, "クライアントは使わない"),
            ("lobby_view_max_per_key", V::Int(n)) => assert_eq!(*n, 3, "クライアントは使わない"),
            ("lobby_view_ttl_ms", V::Int(n)) => assert_eq!(
                *n, 600_000,
                "見る接続は 10 分で expired になる（クライアントは入り直す）"
            ),
            ("lobby_list_max_bytes", V::Int(n)) => assert_eq!(*n as usize, LOBBY_LIST_MAX_BYTES),
            other => panic!("見本の鍵を知らない、または型が違う: {other:?}"),
        }
    }
    assert_eq!(seen, 9, "見本の lobby_ の鍵の数");
}

#[test]
fn the_reserved_prefix_is_the_one_the_room_name_check_refuses() {
    // 見本の接頭辞の名はクライアントが打てない（ルーム名の予約）。サーバーの決まりだけなら通る。
    let list = parse_flat(RULES).expect("rules.json は平らな JSON");
    let prefix = list
        .iter()
        .find(|(k, _)| k == "lobby_room_prefix")
        .map(|(_, v)| v.clone());
    assert_eq!(prefix, Some(V::Str(LOBBY_ROOM_PREFIX.into())));
    for typed in [
        ROOM.to_owned(),
        format!("{LOBBY_ROOM_PREFIX}x"),
        LOBBY_ROOM_PREFIX.to_uppercase(),
    ] {
        assert_eq!(
            RoomName::parse(&typed),
            Err(crate::room::RoomNameError::ReservedPrefix),
            "{typed}"
        );
        assert!(RoomName::parse_server_rules(&typed).is_ok());
    }
}

// ---------------- パス・ルーム名 ----------------

#[test]
fn the_path_holds_the_build_and_the_encoded_room_name() {
    let p = |raw: &str| lobby_path(403_920, &RoomName::parse(raw).unwrap());
    assert_eq!(p("abc"), "/v0/lobby/403920/abc");
    assert_eq!(p("a b"), "/v0/lobby/403920/a%20b");
    assert_eq!(p("A.b_c-d~e"), "/v0/lobby/403920/A.b_c-d~e");
    assert_eq!(p("a%+&"), "/v0/lobby/403920/a%25%2B%26");
    assert_eq!(
        lobby_path(0, &RoomName::parse("x").unwrap()),
        "/v0/lobby/0/x"
    );
    assert_eq!(
        lobby_path(u32::MAX, &RoomName::parse("x").unwrap()),
        "/v0/lobby/4294967295/x"
    );
    // サーバーの検査と同じ符号化（日本語はUTF-8のバイトごとの%XX）。
    let jp = RoomName::parse_server_rules("あ").unwrap();
    assert_eq!(lobby_path(1, &jp), "/v0/lobby/1/%E3%81%82");
    // ルームのパスの名の部分と同じ式。
    let n = RoomName::parse("Room 1").unwrap();
    assert_eq!(
        lobby_path(7, &n).strip_prefix("/v0/lobby/7/"),
        n.path().strip_prefix(crate::room::PATH_PREFIX)
    );
}

#[test]
fn the_lobby_room_accepts_only_the_assigned_shape() {
    assert_eq!(room().name().as_str(), ROOM);
    let hex = &ROOM[2..];
    for bad in [
        String::new(),
        "r.".to_owned(),
        format!("q.{hex}"),
        format!("w.{hex}"),
        format!("R.{hex}"),
        format!("r.{}", &hex[1..]),
        format!("r.{hex}0"),
        format!("r.{}", hex.to_uppercase()),
        format!("r.{}", "g".repeat(30)),
        format!(" r.{hex}"),
        format!("r.{hex}\n"),
        "room-1".to_owned(),
    ] {
        assert_eq!(LobbyRoom::parse(&bad), None, "{bad:?}");
    }
    assert_eq!(format!("{:?}", room()), "LobbyRoom(..)");
    assert!(!format!("{:?}", lobby_posting(true, 10)).contains("Alice"));
    assert_eq!(format!("{:?}", lobby_posting(true, 10)), "LobbyPosting(..)");
}

// ---------------- 届く文 ----------------

#[test]
fn a_list_holds_waiting_and_playing_rows_in_order() {
    let text = list_json(&[
        wait_json("Alice", ROOM),
        wait_json("Bob", "r.fedcba9876543210fedcba98765432"),
        play_json("true", 3, 10),
        play_json("false", 10, 10),
    ]);
    let LobbyMsg::List { rows, skipped } = parse_lobby_message(&text).unwrap() else {
        panic!("list");
    };
    assert_eq!(skipped, 0);
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0],
        LobbyRow::Wait(WaitRow {
            name: name("Alice"),
            room: room()
        })
    );
    let LobbyRow::Wait(w) = rows[1] else { panic!() };
    assert_eq!(w.name.as_str(), "Bob");
    let LobbyRow::Play(a) = rows[2] else { panic!() };
    assert!(a.watch && !a.row.is_full() && a.can_watch());
    assert_eq!(a.row.p1.as_str(), "Alice");
    assert_eq!(
        (a.row.watchers, a.row.cap, a.row.mode, a.row.delay),
        (3, 10, Mode::Rollback, 1)
    );
    assert_eq!(a.row.room_name().unwrap().as_str(), "w.0123456789abcdef");
    let LobbyRow::Play(b) = rows[3] else { panic!() };
    assert!(!b.watch && b.row.is_full() && !b.can_watch());
    // 許可していても満員なら入れない。許可していなければ空きがあっても入れない。
    let open_but_off = PlayRow {
        row: a.row,
        watch: false,
    };
    assert!(!open_but_off.can_watch());
}

#[test]
fn a_list_may_be_empty_and_spaced() {
    for text in [
        r#"{"t":"list","items":[]}"#,
        r#"{ "t" : "list" , "items" : [ ] }"#,
    ] {
        assert_eq!(
            parse_lobby_message(text),
            Ok(LobbyMsg::List {
                rows: vec![],
                skipped: 0
            }),
            "{text}"
        );
    }
}

#[test]
fn rows_outside_the_rules_are_skipped_and_counted() {
    let good_wait = wait_json("Alice", ROOM);
    let bad = [
        // 種類が無い・知らない。
        r#"{"name":"Alice","room":"r.0123456789abcdef0123456789abcd"}"#.to_owned(),
        r#"{"kind":"other","name":"Alice","room":"r.0123456789abcdef0123456789abcd"}"#.to_owned(),
        // 待機中: 名前の決まりの外・割り当て以外の名（q.・w.）・名の形違い。
        wait_json(" Alice", ROOM),
        wait_json("", ROOM),
        wait_json("アリス", ROOM),
        wait_json("Alice", "q.0123456789abcdef0123456789abcd"),
        wait_json("Alice", "w.0123456789abcdef"),
        wait_json("Alice", "r.0123456789ABCDEF0123456789ABCD"),
        // 試合中: watchが真偽でない・無い・観戦用でない名・数の外。
        play_json("\"true\"", 3, 10),
        play_json("1", 3, 10),
        play_json("null", 3, 10),
        r#"{"kind":"play","p1":"A","p2":"B","room":"w.0123456789abcdef","watchers":0,"cap":10,"mode":"rollback","delay":1}"#.to_owned(),
        play_json("true", 11, 10),
        play_json("true", 0, 11),
        play_json("true", 0, 0),
        play_json("true", -1, 10),
        play_json("true", 0, 10).replace("w.0123456789abcdef", ROOM),
    ];
    let mut rows = vec![good_wait.clone()];
    rows.extend(bad.iter().cloned());
    rows.push(play_json("false", 0, 10));
    let LobbyMsg::List { rows: got, skipped } = parse_lobby_message(&list_json(&rows)).unwrap()
    else {
        panic!("list");
    };
    assert_eq!(got.len(), 2, "良い行だけ残る");
    assert_eq!(skipped, bad.len());
}

#[test]
fn a_list_over_the_limits_is_refused() {
    let one = wait_json("Alice", ROOM);
    let full: Vec<String> = (0..LOBBY_ROWS_MAX).map(|_| one.clone()).collect();
    assert!(matches!(
        parse_lobby_message(&list_json(&full)),
        Ok(LobbyMsg::List { skipped: 0, .. })
    ));
    let over: Vec<String> = (0..=LOBBY_ROWS_MAX).map(|_| one.clone()).collect();
    assert!(matches!(
        parse_lobby_message(&list_json(&over)),
        Err(MsgError::NotJson { .. })
    ));
    let big = format!("{}{}", list_json(&[]), " ".repeat(LOBBY_LIST_MAX_BYTES));
    assert!(matches!(
        parse_lobby_message(&big),
        Err(MsgError::TooLarge { .. })
    ));
    assert_eq!(
        parse_lobby_message(r#"{"t":"list"}"#),
        Err(MsgError::Missing("items"))
    );
    for bad in [
        r#"{"t":"list","items":"x"}"#,
        r#"{"t":"list","items":[1,2]}"#,
        r#"{"t":"list","items":[[]]}"#,
    ] {
        assert!(parse_lobby_message(bad).is_err(), "{bad}");
    }
}

#[test]
fn created_carries_only_an_assigned_room() {
    assert_eq!(
        parse_lobby_message(&format!(r#"{{"t":"created","room":"{ROOM}"}}"#)),
        Ok(LobbyMsg::Created { room: room() })
    );
    for bad in [
        r#"{"t":"created","room":"q.0123456789abcdef0123456789abcd"}"#,
        r#"{"t":"created","room":"w.0123456789abcdef"}"#,
        r#"{"t":"created","room":"room"}"#,
    ] {
        assert_eq!(
            parse_lobby_message(bad),
            Err(MsgError::BadValue("room")),
            "{bad}"
        );
    }
    assert_eq!(
        parse_lobby_message(r#"{"t":"created"}"#),
        Err(MsgError::Missing("room"))
    );
    assert_eq!(
        parse_lobby_message(r#"{"t":"created","room":3}"#),
        Err(MsgError::WrongType("room"))
    );
}

#[test]
fn pong_error_and_unknown_messages() {
    assert_eq!(parse_lobby_message(r#"{"t":"pong"}"#), Ok(LobbyMsg::Pong));
    assert_eq!(
        parse_lobby_message(r#"{"t":"error","code":"list_full"}"#),
        Ok(LobbyMsg::Error {
            code: Some(CloseCode::ListFull)
        })
    );
    assert_eq!(
        parse_lobby_message(r#"{"t":"error","code":"zzz"}"#),
        Ok(LobbyMsg::Error { code: None })
    );
    for bad in [
        r#"{"t":"listed"}"#,
        r#"{"t":"room","role":"host","expires_at":1}"#,
        r#"{"t":"queue","expires_at":1}"#,
        r#"{"t":"Pong"}"#,
    ] {
        assert_eq!(
            parse_lobby_message(bad),
            Err(MsgError::UnknownType),
            "{bad}"
        );
    }
    assert!(matches!(
        parse_lobby_message("not json"),
        Err(MsgError::NotJson { .. })
    ));
    // 一覧でない文は短い（長いものは中を読まずに断る）。
    let long = format!(r#"{{"t":"pong","x":"{}"}}"#, "a".repeat(MESSAGE_MAX_BYTES));
    assert!(matches!(
        parse_lobby_message(&long),
        Err(MsgError::TooLarge { .. })
    ));
}

// ---------------- 書く文 ----------------

#[test]
fn the_posts_are_the_servers_exact_shape() {
    assert_eq!(
        write_lobby_post(&lobby_posting(true, 10), 2),
        format!(
            r#"{{"t":"post","room":"{ROOM}","p1":"Alice","p2":"Bob","mode":"rollback","delay":1,"watch":true,"watchers":2,"cap":10,"ticket":"{TICKET}"}}"#
        )
    );
    let off = write_lobby_post(&lobby_posting(false, 4), 9);
    assert!(
        off.contains(r#""watch":false,"watchers":4,"cap":4,"#),
        "{off}"
    );
    // 名前の`"`と`\`は文を壊さない。
    let mut p = lobby_posting(true, 10);
    p.p1 = name(r#"a"b\c"#);
    let s = write_lobby_post(&p, 0);
    assert!(s.contains(r#""p1":"a\"b\\c""#), "{s}");
    // 文はサーバーの読み手（平らなJSON）で読み戻せる。`watch`は真偽で届く。
    let list = parse_flat(&s).unwrap();
    let f = Fields(&list);
    assert_eq!(f.str("t"), Ok("post"));
    assert_eq!(f.str("p1"), Ok(r#"a"b\c"#));
    assert_eq!(f.get("watch"), Some(&V::Bool(true)));
    assert_eq!(
        write_create(&name("Alice")),
        r#"{"t":"create","name":"Alice"}"#
    );
    assert_eq!(
        write_create(&name(r#"a"b\c"#)),
        r#"{"t":"create","name":"a\"b\\c"}"#
    );
}

#[test]
fn the_random_post_has_no_watch_field() {
    use crate::room_queue::AssignedRoom;
    use crate::watch_list::Posting;
    let p = Posting {
        room: AssignedRoom::parse("q.0123456789abcdef0123456789abcd").unwrap(),
        p1: name("Alice"),
        p2: name("Bob"),
        mode: Mode::Delay,
        delay: 2,
        cap: 10,
        ticket: SeatTicket::parse(TICKET).unwrap(),
    };
    assert_eq!(
        write_post(&p, 1),
        format!(
            r#"{{"t":"post","room":"q.0123456789abcdef0123456789abcd","p1":"Alice","p2":"Bob","mode":"delay","delay":2,"watchers":1,"cap":10,"ticket":"{TICKET}"}}"#
        )
    );
}

// ---------------- 載せる接続（試合中） ----------------

fn upgraded_host(watch: bool) -> HostClient {
    let mut c = HostClient::new_lobby(lobby_posting(watch, 10), 0);
    let a = c.on_upgraded(t(0));
    assert_eq!(
        a,
        vec![HostAction::Send(write_lobby_post(
            &lobby_posting(watch, 10),
            0
        ))]
    );
    c
}

#[test]
fn the_lobby_host_posts_and_counts_like_the_random_host() {
    let mut c = upgraded_host(true);
    assert_eq!(c.state(), HostState::AwaitListed);
    // 接続の直後に届く一覧の全体は読み捨てる（載せる前も後も）。
    let list = list_json(&[wait_json("Alice", ROOM), play_json("true", 0, 10)]);
    assert!(c.on_text(t(5), &list).is_empty());
    assert_eq!(c.state(), HostState::AwaitListed);
    assert_eq!(
        c.on_text(t(10), r#"{"t":"listed"}"#),
        vec![HostAction::Notify(HostEvent::Listed)]
    );
    assert_eq!(
        c.set_watchers(3),
        vec![HostAction::Send(r#"{"t":"count","watchers":3}"#.to_owned())]
    );
    // 上限を越える数は上限に収める。
    assert_eq!(
        c.set_watchers(99),
        vec![HostAction::Send(
            r#"{"t":"count","watchers":10}"#.to_owned()
        )]
    );
    assert!(c.on_text(t(20), &list).is_empty());
    assert!(!c.is_finished());
}

#[test]
fn the_lobby_host_fails_with_the_servers_reason() {
    for (code, want) in [
        (4429u16, Failure::ListFull),
        (4408, Failure::Expired),
        (1000, Failure::Unresponsive(Why::Closed(1000))),
    ] {
        let mut c = upgraded_host(false);
        assert_eq!(
            c.on_close(code),
            vec![HostAction::Notify(HostEvent::Failed(want))],
            "{code}"
        );
    }
    let mut c = upgraded_host(true);
    assert_eq!(
        c.on_text(t(1), r#"{"t":"error","code":"list_full"}"#),
        vec![HostAction::Notify(HostEvent::Failed(Failure::ListFull))]
    );
    // 見る接続の上限の409は件数の上限と別のもの。
    let mut c = HostClient::new_lobby(lobby_posting(true, 10), 0);
    assert_eq!(
        c.on_http_refused(409, r#"{"error":"room_full"}"#),
        vec![HostAction::Notify(HostEvent::Failed(Failure::RoomFull))]
    );
    // 一覧でもlistedでもない文は誤り。
    let mut c = upgraded_host(true);
    assert!(matches!(
        c.on_text(
            t(1),
            r#"{"t":"created","room":"r.0123456789abcdef0123456789abcd"}"#
        )
        .as_slice(),
        [HostAction::Notify(HostEvent::Failed(
            Failure::Unresponsive(Why::BadMessage(MsgError::UnknownType))
        ))]
    ));
    // 一覧の1通は2 KBを越えてよい。一覧でない文は越えられない。
    let mut c = upgraded_host(true);
    let long = format!(r#"{{"t":"pong","x":"{}"}}"#, "a".repeat(MESSAGE_MAX_BYTES));
    assert!(matches!(
        c.on_text(t(1), &long).as_slice(),
        [HostAction::Notify(HostEvent::Failed(_))]
    ));
}

#[test]
fn the_random_host_still_refuses_a_list() {
    // ランダム対戦の一覧の載せる接続は一覧を受け取らない（サーバーが送らない）。読み捨てるのはルーム名ごとの一覧の接続だけ。
    let p = crate::watch_list::Posting {
        room: crate::room_queue::AssignedRoom::parse("q.0123456789abcdef0123456789abcd").unwrap(),
        p1: name("Alice"),
        p2: name("Bob"),
        mode: Mode::Rollback,
        delay: 1,
        cap: 10,
        ticket: SeatTicket::parse(TICKET).unwrap(),
    };
    let mut c = HostClient::new(p, 0);
    c.on_upgraded(t(0));
    assert!(matches!(
        c.on_text(t(1), &list_json(&[])).as_slice(),
        [HostAction::Notify(HostEvent::Failed(_))]
    ));
}

// ---------------- 一覧の接続 ----------------

fn text_sends(v: &[LobbyAction]) -> Vec<&str> {
    sends(v, |a| match a {
        LobbyAction::Send(s) => Some(s.as_str()),
        LobbyAction::Notify(_) => None,
    })
}

fn viewing() -> LobbyClient {
    let mut c = LobbyClient::new();
    assert!(c.poll(t(0)).is_empty(), "接続の前は何もしない");
    assert!(c.on_upgraded(t(0)).is_empty());
    assert_eq!(c.state(), LobbyState::AwaitList);
    let a = c.on_text(t(30), &list_json(&[]));
    assert_eq!(
        a,
        vec![LobbyAction::Notify(LobbyEvent::List {
            rows: vec![],
            skipped: 0
        })]
    );
    assert_eq!(c.state(), LobbyState::Viewing);
    c
}

#[test]
fn the_client_receives_the_list_and_every_change() {
    let mut c = viewing();
    let text = list_json(&[wait_json("Alice", ROOM), play_json("true", 1, 10)]);
    let a = c.on_text(t(2000), &text);
    let [LobbyAction::Notify(LobbyEvent::List { rows, skipped })] = a.as_slice() else {
        panic!("{a:?}");
    };
    assert_eq!((rows.len(), *skipped), (2, 0));
    assert_eq!(c.state(), LobbyState::Viewing);
    assert!(!c.is_finished());
    // 見る間は一覧が届く以外の期限を持たない（10分で閉じるのはサーバー。error expiredで知る）。
    assert!(c.poll(t(10_000)).is_empty());
    assert_eq!(
        c.on_text(t(600_000), r#"{"t":"error","code":"expired"}"#),
        vec![LobbyAction::Notify(LobbyEvent::Failed(Failure::Expired))]
    );
    assert!(c.is_finished());
}

#[test]
fn create_is_sent_once_after_the_first_list_and_the_room_is_reported() {
    let mut c = LobbyClient::new();
    // 最初の一覧の前はcreateを送らない。
    assert!(c.create(t(0), &name("Alice")).is_empty());
    c.on_upgraded(t(0));
    assert!(c.create(t(1), &name("Alice")).is_empty());
    c.on_text(t(30), &list_json(&[]));
    let a = c.create(t(40), &name("Alice"));
    assert_eq!(text_sends(&a), [r#"{"t":"create","name":"Alice"}"#]);
    assert_eq!(c.state(), LobbyState::AwaitCreated);
    // 2通目のcreateは送らない。
    assert!(c.create(t(41), &name("Bob")).is_empty());
    // createの前にサーバーが送っていた一覧が届いても状態は変わらず通知もしない。
    assert!(
        c.on_text(t(50), &list_json(&[wait_json("Zed", ROOM)]))
            .is_empty()
    );
    assert_eq!(c.state(), LobbyState::AwaitCreated);
    let a = c.on_text(t(60), &format!(r#"{{"t":"created","room":"{ROOM}"}}"#));
    assert_eq!(
        a,
        vec![LobbyAction::Notify(LobbyEvent::Created { room: room() })]
    );
    assert_eq!(c.state(), LobbyState::Waiting);
    // 待機中の間は一覧も期限も要らない。pingだけを送る。
    assert!(c.on_text(t(70), &list_json(&[])).is_empty());
    assert!(c.create(t(80), &name("Alice")).is_empty());
    assert!(!c.is_finished());
}

#[test]
fn created_without_create_and_other_wrong_order_fail() {
    let order = Failure::Unresponsive(Why::BadMessage(MsgError::BadValue("t")));
    let mut c = viewing();
    assert_eq!(
        c.on_text(t(40), &format!(r#"{{"t":"created","room":"{ROOM}"}}"#)),
        vec![LobbyAction::Notify(LobbyEvent::Failed(order))]
    );
    let mut c = LobbyClient::new();
    assert_eq!(
        c.on_text(t(0), &list_json(&[])),
        vec![LobbyAction::Notify(LobbyEvent::Failed(order))]
    );
    let mut c = viewing();
    c.create(t(40), &name("A"));
    c.on_text(t(60), &format!(r#"{{"t":"created","room":"{ROOM}"}}"#));
    assert_eq!(
        c.on_text(t(61), &format!(r#"{{"t":"created","room":"{ROOM}"}}"#)),
        vec![LobbyAction::Notify(LobbyEvent::Failed(order))]
    );
    let mut c = viewing();
    assert!(matches!(
        c.on_text(t(40), r#"{"t":"listed"}"#).as_slice(),
        [LobbyAction::Notify(LobbyEvent::Failed(
            Failure::Unresponsive(Why::BadMessage(MsgError::UnknownType))
        ))]
    ));
}

#[test]
fn the_client_fails_with_the_servers_reason() {
    for (text, want) in [
        (r#"{"t":"error","code":"list_full"}"#, Failure::ListFull),
        (r#"{"t":"error","code":"expired"}"#, Failure::Expired),
        (
            r#"{"t":"error","code":"bad_message"}"#,
            Failure::Unresponsive(Why::ErrorMessage(Some(CloseCode::BadMessage))),
        ),
    ] {
        let mut c = viewing();
        assert_eq!(
            c.on_text(t(5000), text),
            vec![LobbyAction::Notify(LobbyEvent::Failed(want))],
            "{text}"
        );
        assert!(c.on_text(t(5001), &list_json(&[])).is_empty(), "終わった後");
        assert_eq!(c.next_wake(), None);
    }
    for (code, want) in [
        (4429u16, Failure::ListFull),
        (4408, Failure::Expired),
        (1000, Failure::Unresponsive(Why::Closed(1000))),
        (1005, Failure::Unresponsive(Why::Closed(1005))),
    ] {
        let mut c = viewing();
        assert_eq!(
            c.on_close(code),
            vec![LobbyAction::Notify(LobbyEvent::Failed(want))],
            "{code}"
        );
        assert!(c.on_close(code).is_empty());
    }
}

#[test]
fn the_http_refusals_are_classified() {
    let mut c = LobbyClient::new();
    assert_eq!(
        c.on_http_refused(400, r#"{"error":"bad_room_name"}"#),
        vec![LobbyAction::Notify(LobbyEvent::Failed(
            Failure::BadRoomName
        ))]
    );
    let mut c = LobbyClient::new();
    assert_eq!(
        c.on_http_refused(409, r#"{"error":"room_full"}"#),
        vec![LobbyAction::Notify(LobbyEvent::Failed(Failure::RoomFull))]
    );
    let mut c = LobbyClient::new();
    assert_eq!(
        c.on_http_refused(429, r#"{"error":"rate_limited","retry_after":60}"#),
        vec![LobbyAction::Notify(LobbyEvent::Failed(
            Failure::RateLimited {
                retry_after_secs: 60
            }
        ))]
    );
    let mut c = LobbyClient::new();
    assert_eq!(
        c.on_transport_failed(),
        vec![LobbyAction::Notify(LobbyEvent::Failed(
            Failure::Unresponsive(Why::NoAnswer)
        ))]
    );
    let mut c = viewing();
    assert_eq!(
        c.on_transport_failed(),
        vec![LobbyAction::Notify(LobbyEvent::Failed(
            Failure::Unresponsive(Why::Lost)
        ))]
    );
}

#[test]
fn the_first_list_and_created_have_deadlines_and_pings_run_throughout() {
    // 最初の一覧が来ない。
    let mut c = LobbyClient::new();
    c.on_upgraded(t(0));
    assert_eq!(c.next_wake(), Some(ROOM_GREETING_TIMEOUT_MS));
    assert!(c.poll(t(ROOM_GREETING_TIMEOUT_MS - 1)).is_empty());
    assert_eq!(
        c.poll(t(ROOM_GREETING_TIMEOUT_MS)),
        vec![LobbyAction::Notify(LobbyEvent::Failed(
            Failure::Unresponsive(Why::NoRoom)
        ))]
    );
    // createdが来ない。
    let mut c = viewing();
    c.create(t(100), &name("Alice"));
    assert!(c.poll(t(100 + ROOM_GREETING_TIMEOUT_MS - 1)).is_empty());
    assert_eq!(
        c.poll(t(100 + ROOM_GREETING_TIMEOUT_MS)),
        vec![LobbyAction::Notify(LobbyEvent::Failed(
            Failure::Unresponsive(Why::NoRoom)
        ))]
    );
    // pingを送り、pongが来なければ切れた扱い。見る間も待機中も同じ。
    for waiting in [false, true] {
        let mut c = viewing();
        if waiting {
            c.create(t(40), &name("Alice"));
            c.on_text(t(60), &format!(r#"{{"t":"created","room":"{ROOM}"}}"#));
        }
        let at = crate::room::PING_INTERVAL_MS;
        let a = c.poll(t(at));
        assert_eq!(
            text_sends(&a),
            [crate::room::PING_TEXT],
            "waiting={waiting}"
        );
        assert!(c.poll(t(at + 1)).is_empty());
        let silent = at + crate::room::PONG_TIMEOUT_MS;
        assert_eq!(
            c.poll(t(silent)),
            vec![LobbyAction::Notify(LobbyEvent::Failed(
                Failure::Unresponsive(Why::NoPong)
            ))],
            "waiting={waiting}"
        );
    }
    // pongが来れば続く。
    let mut c = viewing();
    let at = crate::room::PING_INTERVAL_MS;
    c.poll(t(at));
    assert!(c.on_text(t(at + 10), r#"{"t":"pong"}"#).is_empty());
    assert!(c.poll(t(at + crate::room::PONG_TIMEOUT_MS)).len() <= 1);
    assert!(!c.is_finished());
}

fn snapshot_row(i: usize) -> LobbyRow {
    LobbyRow::Wait(WaitRow {
        name: name(&format!("P{i}")),
        room: room(),
    })
}

#[test]
fn the_snapshot_keeps_the_server_order_and_truncates_at_the_row_limit() {
    let mut s = LobbySnapshot::EMPTY;
    assert!(s.is_empty() && s.status == ListStatus::Loading && s.rows().is_empty());
    let rows: Vec<LobbyRow> = (0..LOBBY_ROWS_MAX + 5).map(snapshot_row).collect();
    s.set(ListStatus::Ready, &rows);
    assert_eq!(s.status, ListStatus::Ready);
    assert_eq!(s.len(), LOBBY_ROWS_MAX);
    assert_eq!(s.rows(), &rows[..LOBBY_ROWS_MAX], "並びも中身もそのまま");
    // 入れ替えると前の行は残らない。
    s.set(ListStatus::Ready, &rows[..2]);
    assert_eq!(s.rows(), &rows[..2]);
    s.clear();
    assert!(s.is_empty());
}

#[test]
fn the_snapshot_push_stops_at_the_row_limit() {
    let mut s = LobbySnapshot::EMPTY;
    for i in 0..LOBBY_ROWS_MAX {
        assert!(s.push(snapshot_row(i)), "{i}");
    }
    assert!(!s.push(snapshot_row(99)), "満杯でも足せた");
    assert_eq!(s.len(), LOBBY_ROWS_MAX);
    assert_eq!(s.rows().last(), Some(&snapshot_row(LOBBY_ROWS_MAX - 1)));
}

// ---------------- スレッドの間のワード ----------------

fn play_row(watch: bool, watchers: u8, cap: u8) -> LobbyRow {
    LobbyRow::Play(PlayRow {
        row: WatchRow {
            hash: [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
            p1: name("A \"q\" \\b"),
            p2: name("Bob"),
            mode: Mode::Rollback,
            delay: 15,
            watchers,
            cap,
        },
        watch,
    })
}

#[test]
fn rows_and_rooms_survive_the_words_in_both_kinds() {
    let long = "x".repeat(crate::watch_list::WATCH_NAME_MAX);
    let other = LobbyRoom::parse("r.ffffffffffffffffffffffffffff00").unwrap();
    for r in [
        LobbyRow::Wait(WaitRow {
            name: name("Alice"),
            room: room(),
        }),
        LobbyRow::Wait(WaitRow {
            name: name(&long),
            room: other,
        }),
        play_row(true, 0, 1),
        play_row(false, 3, 10),
    ] {
        assert_eq!(LobbyRow::from_words(&r.to_words()), Some(r), "{r:?}");
    }
    assert_eq!(LobbyRoom::from_words(&room().to_words()), Some(room()));
    assert_eq!(LobbyRoom::from_words(&other.to_words()), Some(other));
    assert_ne!(room().to_words(), other.to_words());
}

#[test]
fn words_outside_the_rules_are_not_rows() {
    assert_eq!(LobbyRow::from_words(&[0; LOBBY_ROW_WORDS]), None, "全部0");
    assert_eq!(LobbyRoom::from_words(&[0; LOBBY_ROOM_WORDS]), None, "全部0");
    let wait = LobbyRow::Wait(WaitRow {
        name: name("Alice"),
        room: room(),
    })
    .to_words();
    let mut bad = wait;
    bad[0] = 3;
    assert_eq!(LobbyRow::from_words(&bad), None, "知らない種類");
    let mut bad = wait;
    bad[1 + crate::watch_list::WATCH_NAME_MAX / 4] = 0;
    assert_eq!(LobbyRow::from_words(&bad), None, "空の名前");
    let mut bad = wait;
    // 割り当てのルーム名の16進を大文字にする。
    let at = 2 + crate::watch_list::WATCH_NAME_MAX / 4;
    bad[at] = u32::from_le_bytes(*b"ABCD");
    assert_eq!(LobbyRow::from_words(&bad), None, "16進の小文字でない");
    let mut bad = play_row(true, 0, 1).to_words();
    bad[0] = ROW_TAG_WAIT;
    assert_eq!(
        LobbyRow::from_words(&bad),
        None,
        "試合中のワードを待機中と読んだ"
    );
    let mut bad = play_row(true, 0, 1).to_words();
    bad[LOBBY_ROW_WORDS - 1] = 0;
    assert_eq!(LobbyRow::from_words(&bad), None, "上限0の試合中の行");
}
