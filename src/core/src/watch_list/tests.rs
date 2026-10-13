use super::*;
use crate::room::{JsonValue as V, parse_flat};

const RULES: &str = include_str!("../../../../tests/fixtures/room/rules.json");
const SPECTATE_NAMES: &str = include_str!("../../../../tests/fixtures/room/spectate_names.json");
const ASSIGNED: &str = "q.0123456789abcdef0123456789abcd";
const W_OF_ASSIGNED: &str = "w.604d502a3981065f";

fn t(ms: u64) -> Now {
    // 壁の時計は単調な時計と別の値にずらす（取り違えても通る形にしない）。
    Now {
        ms,
        unix_secs: 1_790_000_000 + ms / 1000,
    }
}

fn posting(cap: u8) -> Posting {
    Posting {
        room: AssignedRoom::parse(ASSIGNED).unwrap(),
        p1: WatchName::parse("Alice").unwrap(),
        p2: WatchName::parse("Bob").unwrap(),
        mode: Mode::Rollback,
        delay: 1,
        cap,
    }
}

fn row_json(
    room: &str,
    p1: &str,
    p2: &str,
    mode: &str,
    delay: i64,
    watchers: i64,
    cap: i64,
) -> String {
    format!(
        r#"{{"room":"{room}","p1":"{p1}","p2":"{p2}","mode":"{mode}","delay":{delay},"watchers":{watchers},"cap":{cap}}}"#
    )
}

fn list_json(rows: &[String]) -> String {
    format!(r#"{{"t":"list","items":[{}]}}"#, rows.join(","))
}

fn good_row() -> String {
    row_json("w.0123456789abcdef", "Alice", "Bob", "rollback", 1, 3, 10)
}

/// ホストと相手の `[spectate] enable` の 4 通りのうち、載せるのは両者が 1 のときだけ。
#[test]
fn listing_needs_both_players_to_allow_it() {
    for (own, peer, want) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, false),
    ] {
        assert_eq!(may_list(own, peer), want, "自分 {own}・相手 {peer}");
    }
}

fn failed(f: Failure) -> Vec<HostAction> {
    vec![HostAction::Notify(HostEvent::Failed(f))]
}

fn sends(v: &[HostAction]) -> Vec<&str> {
    v.iter()
        .filter_map(|a| match a {
            HostAction::Send(s) => Some(s.as_str()),
            HostAction::Notify(_) => None,
        })
        .collect()
}

// ---------------- 見本との突き合わせ ----------------

#[test]
fn constants_match_the_watch_keys_of_the_rules_fixture() {
    let list = parse_flat(RULES).expect("rules.json は平らな JSON");
    let mut seen = 0;
    for (k, v) in &list {
        if !k.starts_with("watch_") {
            continue;
        }
        seen += 1;
        match (k.as_ref(), v) {
            ("watch_path_prefix", V::Str(s)) => assert_eq!(s, WATCH_PATH_PREFIX),
            ("watch_post_segment", V::Str(s)) => assert_eq!(s, WATCH_POST_SEGMENT),
            ("watch_view_segment", V::Str(s)) => assert_eq!(s, WATCH_VIEW_SEGMENT),
            ("watch_room_prefix", V::Str(s)) => assert_eq!(s, SPECTATE_PREFIX),
            ("watch_hash_bytes", V::Int(n)) => assert_eq!(*n as usize, HASH_BYTES),
            ("watch_max_total", V::Int(n)) => assert_eq!(*n as usize, WATCH_ROWS_MAX),
            ("watch_max_per_ip", V::Int(n)) => assert_eq!(
                *n, 2,
                "クライアントは 1 IP 2 件の上限を使わない（載せるのはマッチ 1 つに 1 件）"
            ),
            ("watch_cap_max", V::Int(n)) => assert_eq!(*n as usize, WATCH_CAP_MAX),
            ("watch_delay_max", V::Int(n)) => assert_eq!(*n as u8, WATCH_DELAY_MAX),
            ("watch_name_max", V::Int(n)) => assert_eq!(*n as usize, WATCH_NAME_MAX),
            ("watch_modes", V::Str(s)) => assert_eq!(s, "rollback,delay"),
            ("watch_stale_ms", V::Int(n)) => assert!(
                (*n as u64) > PONG_TIMEOUT_MS,
                "サーバーの切れた扱いは、こちらが pong を待つ上限より後"
            ),
            ("watch_list_max_bytes", V::Int(n)) => assert_eq!(*n as usize, WATCH_LIST_MAX_BYTES),
            other => panic!("見本の鍵を知らない、または型が違う: {other:?}"),
        }
    }
    assert_eq!(seen, 13, "見本の watch_ の鍵の数");
    // 新しい閉じる番号も見本にあり、部品の番号・名と合う。
    let key = list.iter().find(|(k, _)| k == "close_list_full");
    assert_eq!(
        key.map(|(_, v)| v.clone()),
        Some(V::Int(i64::from(CloseCode::ListFull.number())))
    );
    assert_eq!(CloseCode::ListFull.name(), "list_full");
}

#[test]
fn the_spectate_room_derived_from_an_assigned_room_matches_the_hashlib_fixture() {
    let list = parse_flat(SPECTATE_NAMES).expect("spectate_names.json は平らな JSON");
    let get = |k: &str| match list.iter().find(|(n, _)| n == k) {
        Some((_, V::Str(s))) => Some(s.to_string()),
        _ => None,
    };
    let mut n = 0;
    while let Some(q) = get(&format!("q{n}")) {
        let w = get(&format!("w{n}")).expect("w の見本");
        let derived = AssignedRoom::parse(&q).unwrap().name().spectate_of();
        assert_eq!(derived.as_str(), w, "{q}");
        // 一覧の行（w. のハッシュ）から戻した観戦用のルーム名も同じ。
        let row = WatchRow {
            hash: parse_hash(&w).unwrap(),
            ..good_row_value()
        };
        assert_eq!(row.room_name(), Some(derived), "{w}");
        n += 1;
    }
    assert!(n >= 4, "見本の組の数 {n}");
    assert_eq!(
        AssignedRoom::parse(ASSIGNED)
            .unwrap()
            .name()
            .spectate_of()
            .as_str(),
        W_OF_ASSIGNED
    );
}

fn good_row_value() -> WatchRow {
    WatchRow {
        hash: [1, 2, 3, 4, 5, 6, 7, 8],
        p1: WatchName::parse("Alice").unwrap(),
        p2: WatchName::parse("Bob").unwrap(),
        mode: Mode::Rollback,
        delay: 2,
        watchers: 3,
        cap: 10,
    }
}

// ---------------- パスと文 ----------------

#[test]
fn the_paths_carry_a_plain_decimal_build() {
    assert_eq!(watch_post_path(0), "/v0/watch/0/post");
    assert_eq!(watch_post_path(403_920), "/v0/watch/403920/post");
    assert_eq!(watch_view_path(u32::MAX), "/v0/watch/4294967295/view");
}

#[test]
fn the_post_text_is_exact_and_clamps_what_the_server_would_refuse() {
    let p = posting(10);
    assert_eq!(
        write_post(&p, 3),
        format!(
            r#"{{"t":"post","room":"{ASSIGNED}","p1":"Alice","p2":"Bob","mode":"rollback","delay":1,"watchers":3,"cap":10}}"#
        )
    );
    let mut d = posting(10);
    d.mode = Mode::Delay;
    d.delay = 200;
    // 観戦者の数は上限まで・遅延は最大まで・上限は 1〜WATCH_CAP_MAX に収める。
    let text = write_post(&d, 99);
    assert!(
        text.contains(r#""mode":"delay","delay":15,"watchers":10,"cap":10"#),
        "{text}"
    );
    let z = write_post(&posting(0), 0);
    assert!(z.contains(r#""watchers":0,"cap":1"#), "{z}");
    assert!(write_post(&posting(10), 0).len() < MESSAGE_MAX_BYTES);
    assert_eq!(write_count(7), r#"{"t":"count","watchers":7}"#);
}

#[test]
fn a_posting_never_shows_the_assigned_room_in_debug() {
    let p = posting(10);
    assert!(!format!("{p:?}").contains("0123"));
    let c = HostClient::new(p, 0);
    assert!(!format!("{c:?}").contains("0123456789abcdef"), "{c:?}");
}

// ---------------- 名前 ----------------

#[test]
fn watch_names_accept_only_the_server_characters() {
    for ok in ["a", "Alice-1_@", "?", "?abc?", &"A".repeat(32)] {
        assert_eq!(WatchName::parse(ok).unwrap().as_str(), ok);
    }
    for bad in [
        "",
        &"A".repeat(33),
        "a b",
        " a",
        "アリス",
        "a/b",
        "a.b",
        "<b>",
        "a\n",
        "a\u{0}",
    ] {
        assert!(WatchName::parse(bad).is_none(), "{bad:?}");
    }
}

#[test]
fn lossy_names_replace_what_the_server_would_refuse() {
    assert_eq!(WatchName::lossy("Alice").as_str(), "Alice");
    assert_eq!(WatchName::lossy("  Bob  ").as_str(), "Bob");
    assert_eq!(WatchName::lossy("アリス").as_str(), "???");
    assert_eq!(WatchName::lossy("a b.c").as_str(), "a?b?c");
    assert_eq!(WatchName::lossy("").as_str(), "?");
    assert_eq!(WatchName::lossy("   ").as_str(), "?");
    assert_eq!(WatchName::lossy(&"x".repeat(40)).len(), WATCH_NAME_MAX);
    // 作ったものはサーバーの決まり（parse）を必ず通る。
    for s in ["", "あ", "a b", "A".repeat(40).as_str(), "Alice", "🙂x"] {
        let n = WatchName::lossy(s);
        assert_eq!(WatchName::parse(n.as_str()), Some(n), "{s:?}");
    }
}

// ---------------- 一覧の文を読む ----------------

#[test]
fn a_list_is_read_in_order_with_every_field() {
    let a = row_json("w.0123456789abcdef", "Alice", "Bob", "rollback", 1, 3, 10);
    let b = row_json("w.fedcba9876543210", "Zed", "Amy", "delay", 15, 0, 1);
    let ViewMsg::List { rows, skipped } = parse_view_message(&list_json(&[a, b])).unwrap() else {
        panic!("一覧でない");
    };
    assert_eq!(skipped, 0);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].hash,
        [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
    );
    assert_eq!(
        (
            rows[0].p1.as_str(),
            rows[0].p2.as_str(),
            rows[0].mode,
            rows[0].delay,
            rows[0].watchers,
            rows[0].cap
        ),
        ("Alice", "Bob", Mode::Rollback, 1, 3, 10)
    );
    assert_eq!(rows[1].hash[0], 0xfe);
    assert_eq!(
        (
            rows[1].p1.as_str(),
            rows[1].mode,
            rows[1].delay,
            rows[1].watchers,
            rows[1].cap
        ),
        ("Zed", Mode::Delay, 15, 0, 1)
    );
    assert_eq!(rows[0].room_name().unwrap().as_str(), "w.0123456789abcdef");
    assert!(!rows[0].is_full());
    assert!(!rows[1].is_full());
}

#[test]
fn an_empty_list_is_a_list() {
    assert_eq!(
        parse_view_message(r#"{"t":"list","items":[]}"#),
        Ok(ViewMsg::List {
            rows: Vec::new(),
            skipped: 0
        })
    );
    assert_eq!(
        parse_view_message(r#"{ "t" : "list" , "items" : [ ] }"#),
        Ok(ViewMsg::List {
            rows: Vec::new(),
            skipped: 0
        })
    );
}

#[test]
fn a_full_row_is_full_only_at_the_cap() {
    let mut r = good_row_value();
    r.watchers = 9;
    assert!(!r.is_full());
    r.watchers = 10;
    assert!(r.is_full());
    r.cap = 1;
    r.watchers = 1;
    assert!(r.is_full());
}

#[test]
fn rows_outside_the_rules_are_skipped_and_counted_and_the_good_ones_stay() {
    let bad = [
        // 観戦用でない名（q.… を載せない約束の鏡。サーバーが出したら捨てる）。
        row_json("q.0123456789abcdef0123456789abcd", "A", "B", "rollback", 1, 0, 10),
        row_json("w.0123456789abcde", "A", "B", "rollback", 1, 0, 10),
        row_json("w.0123456789abcdeff", "A", "B", "rollback", 1, 0, 10),
        row_json("w.0123456789ABCDEF", "A", "B", "rollback", 1, 0, 10),
        row_json("w.0123456789abcdeg", "A", "B", "rollback", 1, 0, 10),
        row_json("x.0123456789abcdef", "A", "B", "rollback", 1, 0, 10),
        row_json("w.0123456789abcdef", "", "B", "rollback", 1, 0, 10),
        row_json("w.0123456789abcdef", "A", "a b", "rollback", 1, 0, 10),
        row_json("w.0123456789abcdef", "A", "B", "auto", 1, 0, 10),
        row_json("w.0123456789abcdef", "A", "B", "rollback", 16, 0, 10),
        row_json("w.0123456789abcdef", "A", "B", "rollback", -1, 0, 10),
        row_json("w.0123456789abcdef", "A", "B", "rollback", 1, 0, 11),
        row_json("w.0123456789abcdef", "A", "B", "rollback", 1, 0, 0),
        row_json("w.0123456789abcdef", "A", "B", "rollback", 1, 5, 4),
        row_json("w.0123456789abcdef", "A", "B", "rollback", 1, -1, 10),
        r#"{"room":"w.0123456789abcdef","p1":"A","p2":"B","mode":"rollback","delay":1,"watchers":0}"#.to_owned(),
        r#"{"room":"w.0123456789abcdef","p1":"A","p2":"B","mode":"rollback","delay":1.5,"watchers":0,"cap":10}"#.to_owned(),
        r#"{"room":"w.0123456789abcdef","p1":"A","p2":"B","mode":"rollback","delay":"1","watchers":0,"cap":10}"#.to_owned(),
        r#"{}"#.to_owned(),
    ];
    let mut all: Vec<String> = bad.to_vec();
    all.insert(3, good_row());
    all.push(row_json(
        "w.fedcba9876543210",
        "Zed",
        "Amy",
        "delay",
        0,
        1,
        2,
    ));
    let ViewMsg::List { rows, skipped } = parse_view_message(&list_json(&all)).unwrap() else {
        panic!();
    };
    assert_eq!(skipped, bad.len());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].p1.as_str(), "Alice");
    assert_eq!(rows[1].p1.as_str(), "Zed");
}

#[test]
fn a_list_message_that_is_not_a_list_is_an_error() {
    // items が無い・配列でない・配列の中が物でない・件数が多すぎる・同じ鍵が 2 回。
    assert_eq!(
        parse_view_message(r#"{"t":"list"}"#),
        Err(MsgError::Missing("items"))
    );
    assert_eq!(
        parse_view_message(r#"{"t":"list","items":"x"}"#),
        Err(MsgError::Missing("items"))
    );
    assert!(matches!(
        parse_view_message(r#"{"t":"list","items":[1,2]}"#),
        Err(MsgError::NotJson { .. })
    ));
    assert!(matches!(
        parse_view_message(r#"{"t":"list","items":[[]]}"#),
        Err(MsgError::NotJson { .. })
    ));
    let many: Vec<String> = (0..=WATCH_ROWS_MAX).map(|_| good_row()).collect();
    assert!(matches!(
        parse_view_message(&list_json(&many)),
        Err(MsgError::NotJson { .. })
    ));
    let max: Vec<String> = (0..WATCH_ROWS_MAX).map(|_| good_row()).collect();
    let ViewMsg::List { rows, .. } = parse_view_message(&list_json(&max)).unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), WATCH_ROWS_MAX);
    assert!(matches!(
        parse_view_message(r#"{"t":"list","items":[],"items":[]}"#),
        Err(MsgError::NotJson { .. })
    ));
    assert!(matches!(
        parse_view_message(r#"{"t":"list","items":[] x"#),
        Err(MsgError::NotJson { .. })
    ));
    assert!(matches!(
        parse_view_message(r#"{"t":"list","items":[{"room":"w.0"},]}"#),
        Err(MsgError::NotJson { .. })
    ));
}

#[test]
fn a_hundred_rows_fit_the_list_limit_and_the_room_limit_is_not_used() {
    let max: Vec<String> = (0..WATCH_ROWS_MAX).map(|_| good_row()).collect();
    let text = list_json(&max);
    assert!(
        text.len() > MESSAGE_MAX_BYTES,
        "ルームの 1 通の上限を越える大きさ（別の上限が要る）"
    );
    assert!(text.len() < WATCH_LIST_MAX_BYTES, "{}", text.len());
    // 載せる口の文はルームの上限のまま。
    let pad = format!(
        r#"{{"t":"listed","pad":"{}"}}"#,
        "x".repeat(MESSAGE_MAX_BYTES)
    );
    assert!(matches!(
        parse_host_message(&pad),
        Err(MsgError::TooLarge { .. })
    ));
    let big = format!(
        r#"{{"t":"list","items":[],"pad":"{}"}}"#,
        "x".repeat(WATCH_LIST_MAX_BYTES)
    );
    assert!(matches!(
        parse_view_message(&big),
        Err(MsgError::TooLarge { .. })
    ));
}

#[test]
fn view_messages_other_than_lists() {
    assert_eq!(parse_view_message(r#"{"t":"pong"}"#), Ok(ViewMsg::Pong));
    assert_eq!(
        parse_view_message(r#"{"t":"error","code":"expired"}"#),
        Ok(ViewMsg::Error {
            code: Some(CloseCode::Expired)
        })
    );
    assert_eq!(
        parse_view_message(r#"{"t":"error","code":"nope"}"#),
        Ok(ViewMsg::Error { code: None })
    );
    for bad in [
        r#"{"t":"room","role":"host","expires_at":1}"#,
        r#"{"t":"listed"}"#,
        r#"{"x":1}"#,
    ] {
        assert!(parse_view_message(bad).is_err(), "{bad}");
    }
    assert!(parse_view_message("not json").is_err());
}

#[test]
fn host_messages() {
    assert_eq!(parse_host_message(r#"{"t":"listed"}"#), Ok(HostMsg::Listed));
    assert_eq!(parse_host_message(r#"{"t":"pong"}"#), Ok(HostMsg::Pong));
    assert_eq!(
        parse_host_message(r#"{"t":"error","code":"list_full"}"#),
        Ok(HostMsg::Error {
            code: Some(CloseCode::ListFull)
        })
    );
    for bad in [
        r#"{"t":"list","items":[]}"#,
        r#"{"t":"queue","expires_at":1}"#,
        r#"{"t":1}"#,
        "[]",
    ] {
        assert!(parse_host_message(bad).is_err(), "{bad}");
    }
}

// ---------------- ゲームのスレッドへ渡す語 ----------------

#[test]
fn a_row_survives_the_words_and_garbage_words_are_refused() {
    let r = good_row_value();
    assert_eq!(WatchRow::from_words(&r.to_words()), Some(r));
    let mut long = r;
    long.p1 = WatchName::parse(&"Z".repeat(32)).unwrap();
    long.p2 = WatchName::parse("?").unwrap();
    long.mode = Mode::Delay;
    long.delay = WATCH_DELAY_MAX;
    long.watchers = 10;
    assert_eq!(WatchRow::from_words(&long.to_words()), Some(long));
    // 語が壊れている: 名前の長さ・文字・遅延・上限・観戦者の数。
    let base = r.to_words();
    let mut w = base;
    w[18] = (w[18] & !0xFF) | 33;
    assert_eq!(WatchRow::from_words(&w), None, "長さ 33");
    w = base;
    w[18] &= !0xFF;
    assert_eq!(WatchRow::from_words(&w), None, "長さ 0");
    w = base;
    w[2] = u32::from_le_bytes(*b" lic");
    assert_eq!(WatchRow::from_words(&w), None, "空白");
    w = base;
    w[18] = (w[18] & 0x00FF_FFFF) | (16 << 24);
    assert_eq!(WatchRow::from_words(&w), None, "遅延 16");
    w = base;
    w[19] = 3;
    assert_eq!(WatchRow::from_words(&w), None, "上限 0");
    w[19] = 3 | (11 << 8);
    assert_eq!(WatchRow::from_words(&w), None, "上限 11");
    w[19] = 11 | (10 << 8);
    assert_eq!(WatchRow::from_words(&w), None, "観戦者が上限より多い");
    assert_eq!(WatchRow::from_words(&[0; WATCH_ROW_WORDS]), None, "全部 0");
}

#[test]
fn the_snapshot_holds_at_most_the_list_limit_and_replaces_rows() {
    let mut s = WatchSnapshot::EMPTY;
    assert_eq!(
        (s.status, s.len(), s.is_empty()),
        (ListStatus::Loading, 0, true)
    );
    let rows: Vec<WatchRow> = (0..WATCH_ROWS_MAX + 5)
        .map(|i| WatchRow {
            hash: [i as u8; 8],
            ..good_row_value()
        })
        .collect();
    s.set(ListStatus::Ready, &rows);
    assert_eq!((s.status, s.len()), (ListStatus::Ready, WATCH_ROWS_MAX));
    assert_eq!(
        s.rows()[WATCH_ROWS_MAX - 1].hash[0],
        (WATCH_ROWS_MAX - 1) as u8
    );
    assert!(!s.push(good_row_value()), "満杯");
    s.set(ListStatus::Ready, &rows[..2]);
    assert_eq!(s.len(), 2);
    assert!(s.push(good_row_value()));
    assert_eq!(s.len(), 3);
    s.clear();
    assert!(s.is_empty());
}

// ---------------- 載せる口 ----------------

fn upgraded_host(watchers: u8) -> HostClient {
    let mut c = HostClient::new(posting(10), watchers);
    let a = c.on_upgraded(t(0));
    assert_eq!(
        a,
        vec![HostAction::Send(write_post(&posting(10), watchers))]
    );
    c
}

#[test]
fn the_host_posts_when_upgraded_and_is_listed_on_the_answer() {
    let mut c = HostClient::new(posting(10), 2);
    assert_eq!(c.state(), HostState::Connecting);
    assert!(c.poll(t(0)).is_empty());
    let a = c.on_upgraded(t(0));
    assert_eq!(sends(&a).len(), 1);
    assert!(sends(&a)[0].contains(r#""watchers":2"#));
    assert_eq!(c.state(), HostState::AwaitListed);
    // 2 回目の 101 は何も起こさない。
    assert!(c.on_upgraded(t(1)).is_empty());
    let a = c.on_text(t(50), r#"{"t":"listed"}"#);
    assert_eq!(a, vec![HostAction::Notify(HostEvent::Listed)]);
    assert_eq!(c.state(), HostState::Listed);
    assert!(!c.is_finished());
}

#[test]
fn watchers_that_change_before_listing_are_sent_right_after_listed() {
    let mut c = upgraded_host(0);
    assert!(c.set_watchers(4).is_empty(), "載る前は送らない");
    let a = c.on_text(t(10), r#"{"t":"listed"}"#);
    assert_eq!(
        a,
        vec![
            HostAction::Notify(HostEvent::Listed),
            HostAction::Send(r#"{"t":"count","watchers":4}"#.to_owned())
        ]
    );
    // 変わらない数・元に戻した数: 戻したときだけ送る。
    assert!(c.set_watchers(4).is_empty());
    assert_eq!(
        c.set_watchers(0),
        vec![HostAction::Send(r#"{"t":"count","watchers":0}"#.to_owned())]
    );
    assert!(c.set_watchers(0).is_empty());
    // 上限を越える数は上限に収める。
    assert_eq!(
        c.set_watchers(99),
        vec![HostAction::Send(
            r#"{"t":"count","watchers":10}"#.to_owned()
        )]
    );
}

#[test]
fn the_host_that_changes_back_before_listing_sends_nothing_extra() {
    let mut c = upgraded_host(2);
    c.set_watchers(5);
    c.set_watchers(2);
    assert_eq!(
        c.on_text(t(10), r#"{"t":"listed"}"#),
        vec![HostAction::Notify(HostEvent::Listed)]
    );
}

#[test]
fn the_host_fails_with_the_servers_reason() {
    for (text, want) in [
        (r#"{"t":"error","code":"list_full"}"#, Failure::RoomFull),
        (r#"{"t":"error","code":"expired"}"#, Failure::Expired),
        (
            r#"{"t":"error","code":"bad_message"}"#,
            Failure::Unresponsive(Why::ErrorMessage(Some(CloseCode::BadMessage))),
        ),
        (
            r#"{"t":"error","code":"zzz"}"#,
            Failure::Unresponsive(Why::ErrorMessage(None)),
        ),
    ] {
        let mut c = upgraded_host(0);
        assert_eq!(c.on_text(t(5), text), failed(want), "{text}");
        assert!(c.is_finished());
        assert!(
            c.on_text(t(6), r#"{"t":"listed"}"#).is_empty(),
            "終わった後は何もしない"
        );
        assert!(c.set_watchers(3).is_empty());
        assert_eq!(c.next_wake(), None);
    }
    for (code, want) in [
        (4429u16, Failure::RoomFull),
        (4408, Failure::Expired),
        (4410, Failure::PeerLeft),
        (1000, Failure::Unresponsive(Why::Closed(1000))),
        (1005, Failure::Unresponsive(Why::Closed(1005))),
        (4400, Failure::Unresponsive(Why::Closed(4400))),
    ] {
        let mut c = upgraded_host(0);
        assert_eq!(c.on_close(code), failed(want), "{code}");
    }
}

#[test]
fn the_host_refuses_the_wrong_order_and_bad_text() {
    let mut c = HostClient::new(posting(10), 0);
    assert_eq!(
        c.on_text(t(0), r#"{"t":"listed"}"#),
        failed(Failure::Unresponsive(Why::BadMessage(MsgError::BadValue(
            "t"
        ))))
    );
    let mut c = upgraded_host(0);
    c.on_text(t(1), r#"{"t":"listed"}"#);
    assert_eq!(
        c.on_text(t(2), r#"{"t":"listed"}"#),
        failed(Failure::Unresponsive(Why::BadMessage(MsgError::BadValue(
            "t"
        ))))
    );
    let mut c = upgraded_host(0);
    assert!(matches!(
        c.on_text(t(1), "garbage").as_slice(),
        [HostAction::Notify(HostEvent::Failed(
            Failure::Unresponsive(Why::BadMessage(MsgError::NotJson { .. }))
        ))]
    ));
    // 一覧の文（見る口の文）は載せる口では知らない文。
    let mut c = upgraded_host(0);
    assert!(
        c.on_text(t(1), r#"{"t":"list","items":[]}"#)
            .first()
            .is_some_and(|a| matches!(a, HostAction::Notify(HostEvent::Failed(_))))
    );
}

#[test]
fn the_host_http_and_transport_failures_are_classified_like_the_room() {
    let mut c = HostClient::new(posting(10), 0);
    assert_eq!(
        c.on_http_refused(429, r#"{"error":"rate_limited","retry_after":60}"#),
        failed(Failure::RateLimited {
            retry_after_secs: 60
        })
    );
    let mut c = HostClient::new(posting(10), 0);
    assert_eq!(
        c.on_http_refused(404, ""),
        failed(Failure::Unresponsive(Why::HttpStatus(404)))
    );
    let mut c = HostClient::new(posting(10), 0);
    assert_eq!(
        c.on_transport_failed(),
        failed(Failure::Unresponsive(Why::NoAnswer))
    );
    let mut c = upgraded_host(0);
    assert_eq!(
        c.on_transport_failed(),
        failed(Failure::Unresponsive(Why::Lost))
    );
    assert!(c.on_transport_failed().is_empty());
    assert!(c.on_http_refused(429, "").is_empty());
}

#[test]
fn the_host_gives_up_when_listed_never_comes_but_never_after() {
    let mut c = upgraded_host(0);
    assert_eq!(c.next_wake(), Some(ROOM_GREETING_TIMEOUT_MS));
    assert!(c.poll(t(ROOM_GREETING_TIMEOUT_MS - 1)).is_empty());
    assert_eq!(
        c.poll(t(ROOM_GREETING_TIMEOUT_MS)),
        failed(Failure::Unresponsive(Why::NoRoom))
    );
    // 載った後は、時間がいくら経っても自分からは切れない（pong が来る限り）。
    let mut c = upgraded_host(0);
    c.on_text(t(1), r#"{"t":"listed"}"#);
    let mut now = 1;
    for _ in 0..20 {
        now += PING_INTERVAL_MS;
        let a = c.poll(t(now));
        assert_eq!(a, vec![HostAction::Send(PING_TEXT.to_owned())], "{now}");
        assert!(c.on_text(t(now + 10), r#"{"t":"pong"}"#).is_empty());
    }
    assert!(!c.is_finished());
}

#[test]
fn the_host_is_cut_when_pongs_stop() {
    let mut c = upgraded_host(0);
    c.on_text(t(1), r#"{"t":"listed"}"#);
    // 30 秒ごとに ping、最初の ping から PONG_TIMEOUT_MS で切る（途中で他の文が来なければ）。
    let first = PING_INTERVAL_MS;
    assert_eq!(
        c.poll(t(first)),
        vec![HostAction::Send(PING_TEXT.to_owned())]
    );
    assert_eq!(
        c.poll(t(2 * first)),
        vec![HostAction::Send(PING_TEXT.to_owned())]
    );
    assert!(
        c.poll(t(first + PONG_TIMEOUT_MS - 1))
            .iter()
            .all(|a| !matches!(a, HostAction::Notify(_)))
    );
    assert_eq!(
        c.poll(t(first + PONG_TIMEOUT_MS)),
        failed(Failure::Unresponsive(Why::NoPong))
    );
    // 否定: pong が 1 通でも届けば数え直す。
    let mut c = upgraded_host(0);
    c.on_text(t(1), r#"{"t":"listed"}"#);
    c.poll(t(first));
    c.on_text(t(first + 5), r#"{"t":"pong"}"#);
    assert!(
        c.poll(t(first + PONG_TIMEOUT_MS))
            .iter()
            .all(|a| !matches!(a, HostAction::Notify(_)))
    );
}

// ---------------- 見る口 ----------------

fn viewing() -> ViewClient {
    let mut c = ViewClient::new();
    assert!(c.on_upgraded(t(0)).is_empty());
    c
}

#[test]
fn the_viewer_gets_every_list_in_full_and_stays_open() {
    let mut c = viewing();
    assert_eq!(c.state(), ViewState::AwaitList);
    let a = c.on_text(t(5), &list_json(&[good_row()]));
    let [ViewAction::Notify(ViewEvent::List { rows, skipped })] = a.as_slice() else {
        panic!("{a:?}");
    };
    assert_eq!((rows.len(), *skipped), (1, 0));
    assert_eq!(c.state(), ViewState::Viewing);
    let a = c.on_text(t(6), &list_json(&[]));
    assert_eq!(
        a,
        vec![ViewAction::Notify(ViewEvent::List {
            rows: Vec::new(),
            skipped: 0
        })]
    );
    assert!(!c.is_finished());
    // 一覧が届いた後は期限が無い（ping だけ）。
    assert_eq!(c.next_wake(), Some(PING_INTERVAL_MS));
}

#[test]
fn the_viewer_fails_without_a_first_list_and_on_the_servers_close() {
    let mut c = viewing();
    assert_eq!(
        c.poll(t(ROOM_GREETING_TIMEOUT_MS)),
        vec![ViewAction::Notify(ViewEvent::Failed(
            Failure::Unresponsive(Why::NoRoom)
        ))]
    );
    let mut c = viewing();
    assert_eq!(
        c.on_close(4408),
        vec![ViewAction::Notify(ViewEvent::Failed(Failure::Expired))]
    );
    assert!(c.on_close(4408).is_empty());
    let mut c = ViewClient::new();
    assert_eq!(
        c.on_transport_failed(),
        vec![ViewAction::Notify(ViewEvent::Failed(
            Failure::Unresponsive(Why::NoAnswer)
        ))]
    );
    let mut c = ViewClient::new();
    assert_eq!(
        c.on_http_refused(429, r#"{"error":"rate_limited","retry_after":60}"#),
        vec![ViewAction::Notify(ViewEvent::Failed(
            Failure::RateLimited {
                retry_after_secs: 60
            }
        ))]
    );
    // 101 の前に届いた一覧・載せる口の文・壊れた文は誤り。
    let mut c = ViewClient::new();
    assert!(matches!(
        c.on_text(t(0), &list_json(&[])).as_slice(),
        [ViewAction::Notify(ViewEvent::Failed(_))]
    ));
    for bad in [r#"{"t":"listed"}"#, "garbage", r#"{"t":"list","items":3}"#] {
        let mut c = viewing();
        assert!(
            matches!(
                c.on_text(t(1), bad).as_slice(),
                [ViewAction::Notify(ViewEvent::Failed(_))]
            ),
            "{bad}"
        );
    }
}

#[test]
fn the_viewer_pings_and_is_cut_when_pongs_stop() {
    let mut c = viewing();
    c.on_text(t(1), &list_json(&[]));
    assert_eq!(
        c.poll(t(PING_INTERVAL_MS)),
        vec![ViewAction::Send(PING_TEXT.to_owned())]
    );
    assert!(
        c.on_text(t(PING_INTERVAL_MS + 1), r#"{"t":"pong"}"#)
            .is_empty()
    );
    assert_eq!(
        c.poll(t(2 * PING_INTERVAL_MS)),
        vec![ViewAction::Send(PING_TEXT.to_owned())]
    );
    assert_eq!(
        c.poll(t(PING_INTERVAL_MS + 2 + PONG_TIMEOUT_MS + PING_INTERVAL_MS)),
        vec![ViewAction::Notify(ViewEvent::Failed(
            Failure::Unresponsive(Why::NoPong)
        ))]
    );
}
