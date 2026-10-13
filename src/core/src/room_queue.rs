//! ランダム対戦の待ち列（`src/server/src/random_queue.ts`）のクライアントの純粋な部品。
//!
//! 待ち列に入ると、サーバーが来た順に 2 人ずつ組にし、組ごとに推測できない新しいルーム名を割り当てて 2 人へ返す。
//! 組になった後は今のルームの手順（`room.rs`）へ、その名で入り直す。ここは待ち列の手順だけを持つ（ソケットも時計も持たない）。
//!
//! ## 順番
//!
//! `queue`（待ち列に入った。期限つき）→ `paired`（割り当てたルーム名と、待ち列に先に入った側か）→ サーバーが 1000 で閉じる。
//! 待つ間の ping / pong、期限の切れ方、閉じる番号と直前の error の文はルームと同じ（`room.rs`）。
//!
//! ## 秘密
//!
//! 割り当てられたルーム名は画面にもログにも出さない（人がそのルームへ割り込めないための秘密）。[`AssignedRoom`] の `Debug` は名を伏せる。
//!
//! 見張りのスレッドで回す。ゲームのスレッドでは呼ばない。

use crate::room::{CLIENT_ID_BYTES, CLIENT_ID_PARAM};
use crate::room::{
    CloseCode, DEADLINE_GRACE_MS, Failure, Fields, JsonAt, MESSAGE_MAX_BYTES, MsgError, Now,
    PING_INTERVAL_MS, PING_TEXT, PONG_TIMEOUT_MS, ROOM_GREETING_TIMEOUT_MS, ROOM_NAME_MAX,
    ROOM_TTL_MS, RoomName, Why, backstop, classify_http, hex_lower, parse_flat,
};

// ---------------- サーバーの決まり（見本 tests/fixtures/room/rules.json の random_*） ----------------

/// 待ち列のパスの先頭。後ろにビルド番号（10 進）が付く（`rules.ts` の `RANDOM_PATH_PREFIX`）。
pub const RANDOM_PATH_PREFIX: &str = "/v0/random/";
/// 待ち列の期限（`rules.ts` の `RANDOM_QUEUE_TTL_MS`。ルームの期限と同じ）。
pub const RANDOM_QUEUE_TTL_MS: u64 = ROOM_TTL_MS;
/// 割り当てられたルームの期限（`rules.ts` の `RANDOM_ROOM_TTL_MS`）。
pub const RANDOM_ROOM_TTL_MS: u64 = 60 * 1000;
/// 割り当てられるルーム名の先頭と、続く乱数の 16 進の文字数（`rules.ts` の `RANDOM_ROOM_PREFIX` / `RANDOM_ROOM_HEX_LEN`）。
/// 利用者は `.` を打てない（[`RoomName::parse`]）ので、打ったルーム名とぶつからない。
pub const RANDOM_ROOM_PREFIX: &str = "q.";
pub const RANDOM_ROOM_HEX_LEN: usize = 30;

const _: () = assert!(RANDOM_ROOM_PREFIX.len() + RANDOM_ROOM_HEX_LEN == ROOM_NAME_MAX);

/// 待ち列へ繋ぐパス（`/v0/random/<ビルド番号>`、`id` があれば問い `?cid=…` を付ける。問いの書き方はルームのパスと同じ）。
/// ビルド番号は 10 進で先頭に 0 を付けない（サーバーが 1 通りの書き方だけを受ける）。
pub fn random_path(build: u32, id: Option<&[u8; CLIENT_ID_BYTES]>) -> String {
    let mut out = format!("{RANDOM_PATH_PREFIX}{build}");
    if let Some(id) = id {
        out.push('?');
        out.push_str(CLIENT_ID_PARAM);
        out.push('=');
        for b in id {
            out.push(char::from(hex_lower(b >> 4)));
            out.push(char::from(hex_lower(b & 0x0F)));
        }
    }
    out
}

/// サーバーが待ち列で割り当てたルーム名。形は `q.` ＋ 小文字の 16 進 30 文字だけ（それ以外は受けない）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AssignedRoom(RoomName);

impl core::fmt::Debug for AssignedRoom {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AssignedRoom(..)")
    }
}

impl AssignedRoom {
    /// サーバーから届いた名を確かめる。形が違えば `None`。
    pub fn parse(raw: &str) -> Option<AssignedRoom> {
        let hex = raw.strip_prefix(RANDOM_ROOM_PREFIX)?;
        let shape_ok = hex.len() == RANDOM_ROOM_HEX_LEN
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !shape_ok {
            return None;
        }
        RoomName::parse_server_rules(raw).ok().map(AssignedRoom)
    }

    /// 繋ぐためのルーム名。
    pub fn name(&self) -> RoomName {
        self.0
    }
}

// ---------------- サーバーから届く文 ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMsg {
    /// 繋いだ直後に 1 回。`expires_at` は待ち列の期限（UNIX 秒）。
    Queue { expires_at: u64 },
    /// 組になった。割り当てたルーム名と、待ち列に先に入った側か。
    Paired { room: AssignedRoom, first: bool },
    /// ping への自動応答。
    Pong,
    /// 閉じる直前の 1 通。知らない名は `None`。
    Error { code: Option<CloseCode> },
}

/// サーバーから届いた 1 通を読む。知らない `t`（`room` など、ルームの文を含む）は誤り。順番は見ない（[`QueueClient`] が見る）。
pub fn parse_queue_message(text: &str) -> Result<QueueMsg, MsgError> {
    if text.len() > MESSAGE_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: text.len() });
    }
    let list = parse_flat(text).map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    let f = Fields(&list);
    match f.str("t")? {
        "queue" => Ok(QueueMsg::Queue {
            expires_at: f.unix_secs("expires_at")?,
        }),
        "paired" => {
            let room = AssignedRoom::parse(f.str("room")?).ok_or(MsgError::BadValue("room"))?;
            let first = match f.str("order")? {
                "first" => true,
                "second" => false,
                _ => return Err(MsgError::BadValue("order")),
            };
            Ok(QueueMsg::Paired { room, first })
        }
        "pong" => Ok(QueueMsg::Pong),
        "error" => Ok(QueueMsg::Error {
            code: CloseCode::from_name(f.str("code")?),
        }),
        _ => Err(MsgError::UnknownType),
    }
}

// ---------------- クライアントの状態機械 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEvent {
    /// 待ち列に入った。`expires_at` は待ち列の期限（UNIX 秒。表示の目安）。
    Queued { expires_at: u64 },
    /// 組になった（この直後に手順は終わる）。割り当てられたルームへ入り直す。`first` は待ち列に先に入った側。
    Paired { room: AssignedRoom, first: bool },
    /// 手順が終わらなかった。
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    /// この文字をそのままテキストの文で送る（ping だけ）。
    Send(String),
    Notify(QueueEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    /// 101 を待っている（運びは dll）。
    Connecting,
    /// 101 の後、`queue` を待っている。
    AwaitQueue,
    /// `queue` の後、組になる（`paired`）のを待っている。
    AwaitPaired,
    Done,
    Failed(Failure),
}

/// 待ち列のクライアントの手順。I/O をせず、送る文と出来事の列を返す。使い方は [`crate::room::RoomClient`] と同じ
/// （繋ぐ前に `new` → 101 なら `on_upgraded`、101 でなければ `on_http_refused`、繋がらなければ `on_transport_failed`。
/// 以後、受けた文を `on_text`、閉じる番号を `on_close` へ。`next_wake` の時刻か、それより前に `poll` を呼ぶ）。
#[derive(Debug, Clone)]
pub struct QueueClient {
    state: QueueState,
    deadline_ms: Option<u64>,
    next_ping_ms: Option<u64>,
    unanswered_ping_ms: Option<u64>,
}

impl Default for QueueClient {
    fn default() -> Self {
        QueueClient::new()
    }
}

impl QueueClient {
    pub fn new() -> QueueClient {
        QueueClient {
            state: QueueState::Connecting,
            deadline_ms: None,
            next_ping_ms: None,
            unanswered_ping_ms: None,
        }
    }

    pub fn state(&self) -> QueueState {
        self.state
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, QueueState::Done | QueueState::Failed(_))
    }

    /// いまの状態の期限（ms。こちらの予備）。
    pub fn deadline_at(&self) -> Option<u64> {
        self.deadline_ms
    }

    fn silence_deadline_at(&self) -> Option<u64> {
        self.unanswered_ping_ms
            .map(|p| p.saturating_add(PONG_TIMEOUT_MS))
    }

    /// 次に `poll` を呼ぶべき時刻（ms）。何も待っていなければ None。
    pub fn next_wake(&self) -> Option<u64> {
        [
            self.next_ping_ms,
            self.deadline_ms,
            self.silence_deadline_at(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn stop_clocks(&mut self) {
        self.deadline_ms = None;
        self.next_ping_ms = None;
        self.unanswered_ping_ms = None;
    }

    fn fail(&mut self, f: Failure) -> Vec<QueueAction> {
        self.state = QueueState::Failed(f);
        self.stop_clocks();
        vec![QueueAction::Notify(QueueEvent::Failed(f))]
    }

    /// 101 を受けた。`queue` を待ち始め、ping の時計を回す。
    pub fn on_upgraded(&mut self, now: Now) -> Vec<QueueAction> {
        if self.state != QueueState::Connecting {
            return Vec::new();
        }
        self.state = QueueState::AwaitQueue;
        self.deadline_ms = Some(now.ms.saturating_add(ROOM_GREETING_TIMEOUT_MS));
        self.next_ping_ms = Some(now.ms.saturating_add(PING_INTERVAL_MS));
        Vec::new()
    }

    pub fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<QueueAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(classify_http(status, body))
    }

    /// 繋がらない・繋がった後に閉じる番号なしに切れた。
    pub fn on_transport_failed(&mut self) -> Vec<QueueAction> {
        match self.state {
            QueueState::Done | QueueState::Failed(_) => Vec::new(),
            QueueState::Connecting => self.fail(Failure::Unresponsive(Why::NoAnswer)),
            _ => self.fail(Failure::Unresponsive(Why::Lost)),
        }
    }

    /// サーバーが閉じた。組になる前の 1000 は早すぎる終わり。
    pub fn on_close(&mut self, code: u16) -> Vec<QueueAction> {
        if self.is_finished() {
            return Vec::new();
        }
        match CloseCode::from_number(code) {
            Some(CloseCode::Expired) => self.fail(Failure::Expired),
            Some(CloseCode::PeerLeft) => self.fail(Failure::PeerLeft),
            _ => self.fail(Failure::Unresponsive(Why::Closed(code))),
        }
    }

    pub fn on_text(&mut self, now: Now, text: &str) -> Vec<QueueAction> {
        if self.is_finished() {
            return Vec::new();
        }
        // 文が届いた = サーバーまでの経路は生きている（pong に限らない）。
        self.unanswered_ping_ms = None;
        let msg = match parse_queue_message(text) {
            Ok(m) => m,
            Err(e) => return self.fail(Failure::Unresponsive(Why::BadMessage(e))),
        };
        match (self.state, msg) {
            (QueueState::Connecting, _) => self.out_of_order(),
            (_, QueueMsg::Pong) => Vec::new(),
            (_, QueueMsg::Error { code }) => match code {
                Some(CloseCode::Expired) => self.fail(Failure::Expired),
                Some(CloseCode::PeerLeft) => self.fail(Failure::PeerLeft),
                other => self.fail(Failure::Unresponsive(Why::ErrorMessage(other))),
            },
            (QueueState::AwaitQueue, QueueMsg::Queue { expires_at }) => {
                self.deadline_ms = Some(backstop(now, expires_at, RANDOM_QUEUE_TTL_MS));
                self.state = QueueState::AwaitPaired;
                vec![QueueAction::Notify(QueueEvent::Queued { expires_at })]
            }
            (QueueState::AwaitPaired, QueueMsg::Paired { room, first }) => {
                self.state = QueueState::Done;
                self.stop_clocks();
                vec![QueueAction::Notify(QueueEvent::Paired { room, first })]
            }
            _ => self.out_of_order(),
        }
    }

    /// 順番違いの文（`queue` の前の `paired`・2 回目の `queue` など）。
    fn out_of_order(&mut self) -> Vec<QueueAction> {
        self.fail(Failure::Unresponsive(Why::BadMessage(MsgError::BadValue(
            "t",
        ))))
    }

    /// 時計を進める。期限を過ぎていれば失敗、ping の時刻なら ping。
    pub fn poll(&mut self, now: Now) -> Vec<QueueAction> {
        if self.is_finished() || self.state == QueueState::Connecting {
            return Vec::new();
        }
        if let Some(d) = self.deadline_ms
            && now.ms >= d
        {
            let f = match self.state {
                QueueState::AwaitQueue => Failure::Unresponsive(Why::NoRoom),
                _ => Failure::Expired,
            };
            return self.fail(f);
        }
        // ping より先に見る（3 周目の後の 4 通目を送る前に切る）。
        if let Some(s) = self.silence_deadline_at()
            && now.ms >= s
        {
            return self.fail(Failure::Unresponsive(Why::NoPong));
        }
        if let Some(p) = self.next_ping_ms
            && now.ms >= p
        {
            self.next_ping_ms = Some(now.ms.saturating_add(PING_INTERVAL_MS));
            self.unanswered_ping_ms.get_or_insert(now.ms);
            return vec![QueueAction::Send(PING_TEXT.to_owned())];
        }
        Vec::new()
    }
}

// 予備の期限の長さは、サーバーの期限の後ろに `DEADLINE_GRACE_MS` を足す（`backstop`）。ここで使う定数の取り込みを検査が確かめる。
const _: () = assert!(DEADLINE_GRACE_MS > 0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::{ROOM_NAME_BAD_NAMES, RoomNameError};

    const RULES: &str = include_str!("../../../tests/fixtures/room/rules.json");
    const ASSIGNED: &str = "q.0123456789abcdef0123456789abcd";

    fn t(ms: u64) -> Now {
        // 壁の時計は単調な時計と別の値にずらしておく（同じ値だと取り違えても通る）。
        Now {
            ms,
            unix_secs: 1_790_000_000 + ms / 1000,
        }
    }

    fn paired_text(room: &str, order: &str) -> String {
        format!(r#"{{"t":"paired","room":"{room}","order":"{order}"}}"#)
    }

    fn failed(why: Why) -> Vec<QueueAction> {
        vec![QueueAction::Notify(QueueEvent::Failed(
            Failure::Unresponsive(why),
        ))]
    }

    /// 見本の `random_` で始まる鍵を全部読み、この部品の定数と突き合わせる（読まなかった鍵が残れば落ちる）。
    #[test]
    fn constants_match_the_random_keys_of_the_rules_fixture() {
        let list = parse_flat(RULES).expect("rules.json は平らな JSON");
        let mut seen = Vec::new();
        for (k, v) in &list {
            if !k.starts_with("random_") {
                continue;
            }
            seen.push(k.to_string());
            use crate::room::JsonValue as V;
            match (k.as_ref(), v) {
                ("random_path_prefix", V::Str(s)) => assert_eq!(s, RANDOM_PATH_PREFIX),
                ("random_room_prefix", V::Str(s)) => assert_eq!(s, RANDOM_ROOM_PREFIX),
                ("random_room_hex_len", V::Int(n)) => {
                    assert_eq!(*n as usize, RANDOM_ROOM_HEX_LEN)
                }
                ("random_queue_ttl_ms", V::Int(n)) => {
                    assert_eq!(*n as u64, RANDOM_QUEUE_TTL_MS)
                }
                ("random_room_ttl_ms", V::Int(n)) => assert_eq!(*n as u64, RANDOM_ROOM_TTL_MS),
                other => panic!("見本の鍵を知らない、または型が違う: {other:?}"),
            }
        }
        assert_eq!(seen.len(), 5, "見本の random_ の鍵: {seen:?}");
    }

    #[test]
    fn the_random_path_carries_a_plain_decimal_build_and_the_client_query() {
        assert_eq!(random_path(0, None), "/v0/random/0");
        assert_eq!(random_path(403_920, None), "/v0/random/403920");
        assert_eq!(random_path(u32::MAX, None), "/v0/random/4294967295");
        let id = [0xAB; CLIENT_ID_BYTES];
        assert_eq!(
            random_path(7, Some(&id)),
            format!("/v0/random/7?cid={}", "ab".repeat(CLIENT_ID_BYTES))
        );
        // ルームのパスの問いと同じ書き方（サーバーの parseClientId が読む形）。
        let room = RoomName::parse("r").unwrap();
        assert_eq!(
            room.path_with_client(&id).split_once('?').map(|x| x.1),
            random_path(7, Some(&id)).split_once('?').map(|x| x.1)
        );
    }

    #[test]
    fn an_assigned_room_has_exactly_one_shape_and_cannot_be_typed() {
        let ok = AssignedRoom::parse(ASSIGNED).expect("割り当ての形");
        assert_eq!(ok.name().as_str(), ASSIGNED);
        assert_eq!(ok.name().path(), format!("/v0/rooms/{ASSIGNED}"));
        for bad in [
            "",
            "q.",
            "q.0123456789abcdef0123456789abc",
            "q.0123456789abcdef0123456789abcde",
            "q.0123456789ABCDEF0123456789abcd",
            "q.0123456789abcdeg0123456789abcd",
            "Q.0123456789abcdef0123456789abcd",
            "w.0123456789abcdef0123456789abcd",
            "q-0123456789abcdef0123456789abcd",
            " q.0123456789abcdef0123456789abc",
            "q.0123456789abcdef0123456789abc\n",
            "q.0123456789abcdef0123456789abcd/",
            "q.0123456789abcdef0123456789abc／",
        ] {
            assert_eq!(AssignedRoom::parse(bad), None, "{bad:?}");
        }
        // 利用者が打てる名ではない（`.` が NotAscii）。サーバーの決まりは通る。
        assert_eq!(
            RoomName::parse(ASSIGNED),
            Err(RoomNameError::NotAscii { pos: 1 })
        );
        assert!(RoomName::parse_server_rules(ASSIGNED).is_ok());
        assert!(!RoomName::accepts_char('.'));
        assert!(!ROOM_NAME_BAD_NAMES.contains(&ASSIGNED));
        // 打てる名どうしは割り当ての形にならない。
        for typed in [
            "q",
            "q0123456789abcdef0123456789abcd",
            "0123456789abcdef0123456789abcd",
            "room-1",
            "Q_0",
        ] {
            assert_eq!(AssignedRoom::parse(typed), None, "{typed}");
        }
        // 画面・ログに出さない名は Debug にも出ない。
        assert!(!format!("{ok:?}").contains("0123456789"));
        let ev = QueueEvent::Paired {
            room: ok,
            first: true,
        };
        assert!(!format!("{ev:?}").contains("0123456789"));
        let msg = QueueMsg::Paired {
            room: ok,
            first: false,
        };
        assert!(!format!("{msg:?}").contains("0123456789"));
    }

    #[test]
    fn queue_messages_read_and_malformed_ones_are_refused() {
        assert_eq!(
            parse_queue_message(r#"{"t":"queue","expires_at":1790000600}"#),
            Ok(QueueMsg::Queue {
                expires_at: 1_790_000_600
            })
        );
        let room = AssignedRoom::parse(ASSIGNED).unwrap();
        assert_eq!(
            parse_queue_message(&paired_text(ASSIGNED, "first")),
            Ok(QueueMsg::Paired { room, first: true })
        );
        assert_eq!(
            parse_queue_message(&paired_text(ASSIGNED, "second")),
            Ok(QueueMsg::Paired { room, first: false })
        );
        assert_eq!(parse_queue_message(r#"{"t":"pong"}"#), Ok(QueueMsg::Pong));
        assert_eq!(
            parse_queue_message(r#"{"t":"error","code":"expired"}"#),
            Ok(QueueMsg::Error {
                code: Some(CloseCode::Expired)
            })
        );
        for (text, want) in [
            (paired_text(ASSIGNED, "third"), MsgError::BadValue("order")),
            (paired_text("room-1", "first"), MsgError::BadValue("room")),
            (
                paired_text(&ASSIGNED.to_uppercase(), "first"),
                MsgError::BadValue("room"),
            ),
            (
                r#"{"t":"paired","order":"first"}"#.to_owned(),
                MsgError::Missing("room"),
            ),
            (
                format!(r#"{{"t":"paired","room":"{ASSIGNED}"}}"#),
                MsgError::Missing("order"),
            ),
            (
                r#"{"t":"queue"}"#.to_owned(),
                MsgError::Missing("expires_at"),
            ),
            (
                r#"{"t":"queue","expires_at":-1}"#.to_owned(),
                MsgError::BadValue("expires_at"),
            ),
            (
                r#"{"t":"room","role":"host","expires_at":1790000600}"#.to_owned(),
                MsgError::UnknownType,
            ),
            (
                r#"{"t":"peer","code":"Abc"}"#.to_owned(),
                MsgError::UnknownType,
            ),
        ] {
            assert_eq!(parse_queue_message(&text), Err(want), "{text}");
        }
        assert!(matches!(
            parse_queue_message(&"x".repeat(MESSAGE_MAX_BYTES + 1)),
            Err(MsgError::TooLarge { .. })
        ));
    }

    fn joined(c: &mut QueueClient, at: u64) -> Vec<QueueAction> {
        assert_eq!(c.on_upgraded(t(at)), vec![]);
        c.on_text(
            t(at + 20),
            &format!(
                r#"{{"t":"queue","expires_at":{}}}"#,
                t(at + 20).unix_secs + RANDOM_QUEUE_TTL_MS / 1000
            ),
        )
    }

    #[test]
    fn the_client_waits_then_gets_the_assigned_room_and_finishes() {
        let mut c = QueueClient::new();
        assert_eq!(c.state(), QueueState::Connecting);
        let want_expires = t(20).unix_secs + RANDOM_QUEUE_TTL_MS / 1000;
        assert_eq!(
            joined(&mut c, 0),
            vec![QueueAction::Notify(QueueEvent::Queued {
                expires_at: want_expires
            })]
        );
        assert_eq!(c.state(), QueueState::AwaitPaired);
        // 待つ間も ping を送り、pong を受ける。
        assert_eq!(
            c.poll(t(PING_INTERVAL_MS + 1)),
            vec![QueueAction::Send(PING_TEXT.to_owned())]
        );
        assert_eq!(
            c.on_text(t(PING_INTERVAL_MS + 40), r#"{"t":"pong"}"#),
            vec![]
        );
        let room = AssignedRoom::parse(ASSIGNED).unwrap();
        assert_eq!(
            c.on_text(t(60_000), &paired_text(ASSIGNED, "second")),
            vec![QueueAction::Notify(QueueEvent::Paired {
                room,
                first: false
            })]
        );
        assert!(c.is_finished());
        assert_eq!(c.state(), QueueState::Done);
        assert_eq!(c.next_wake(), None, "終わったら時計を止める");
        // サーバーの 1000 は終わった後なので何も起こさない。
        assert_eq!(c.on_close(1000), vec![]);
        assert_eq!(c.state(), QueueState::Done);
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(
            c.on_text(t(1_000), &paired_text(ASSIGNED, "first")),
            vec![QueueAction::Notify(QueueEvent::Paired {
                room,
                first: true
            })]
        );
    }

    #[test]
    fn the_client_refuses_room_messages_and_a_pairing_out_of_order() {
        let order = |m| failed(Why::BadMessage(m));
        let mut c = QueueClient::new();
        c.on_upgraded(t(0));
        assert_eq!(
            c.on_text(
                t(10),
                r#"{"t":"room","role":"host","expires_at":1790000600}"#
            ),
            order(MsgError::UnknownType)
        );
        let mut c = QueueClient::new();
        c.on_upgraded(t(0));
        assert_eq!(
            c.on_text(t(10), &paired_text(ASSIGNED, "first")),
            order(MsgError::BadValue("t")),
            "queue の前の paired"
        );
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(
            c.on_text(t(10), r#"{"t":"queue","expires_at":1790000600}"#),
            order(MsgError::BadValue("t")),
            "2 回目の queue"
        );
        let mut c = QueueClient::new();
        assert_eq!(
            c.on_text(t(10), r#"{"t":"queue","expires_at":1790000600}"#),
            order(MsgError::BadValue("t")),
            "101 の前の文"
        );
    }

    #[test]
    fn the_client_fails_on_early_closes_expiry_and_silence() {
        // 組になる前の 1000 は早すぎる終わり。
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(c.on_close(1000), failed(Why::Closed(1000)));
        // 待ち列の期限（サーバーの 4408）。
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(
            c.on_close(4408),
            vec![QueueAction::Notify(QueueEvent::Failed(Failure::Expired))]
        );
        // サーバーが error を送ってから閉じる形。
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(
            c.on_text(t(100), r#"{"t":"error","code":"expired"}"#),
            vec![QueueAction::Notify(QueueEvent::Failed(Failure::Expired))]
        );
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(
            c.on_text(t(100), r#"{"t":"error","code":"bad_message"}"#),
            failed(Why::ErrorMessage(Some(CloseCode::BadMessage)))
        );
        // こちらの予備の期限（サーバーの期限の後ろ）。手前では失敗にしない。
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        let deadline = c.deadline_at().expect("待ち列の期限");
        assert_eq!(
            deadline,
            20 + RANDOM_QUEUE_TTL_MS + DEADLINE_GRACE_MS,
            "待ち列の期限 + 予備"
        );
        assert!(
            !c.poll(t(deadline - 1))
                .iter()
                .any(|a| matches!(a, QueueAction::Notify(QueueEvent::Failed(_))))
        );
        assert_eq!(
            c.poll(t(deadline)),
            vec![QueueAction::Notify(QueueEvent::Failed(Failure::Expired))]
        );
        // 101 の後に queue が来ない。
        let mut c = QueueClient::new();
        c.on_upgraded(t(0));
        assert_eq!(c.poll(t(ROOM_GREETING_TIMEOUT_MS)), failed(Why::NoRoom));
        // ping に pong が来ない。
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(c.poll(t(PING_INTERVAL_MS)).len(), 1);
        assert_eq!(
            c.poll(t(PING_INTERVAL_MS + PONG_TIMEOUT_MS)),
            failed(Why::NoPong)
        );
        // 繋がらない・429・受信の誤り。
        let mut c = QueueClient::new();
        assert_eq!(c.on_transport_failed(), failed(Why::NoAnswer));
        let mut c = QueueClient::new();
        assert_eq!(
            c.on_http_refused(429, r#"{"error":"rate_limited","retry_after":60}"#),
            vec![QueueAction::Notify(QueueEvent::Failed(
                Failure::RateLimited {
                    retry_after_secs: 60
                }
            ))]
        );
        let mut c = QueueClient::new();
        joined(&mut c, 0);
        assert_eq!(c.on_transport_failed(), failed(Why::Lost));
        // 終わった後に何が来ても、何も起こさない。
        assert_eq!(c.on_text(t(1), &paired_text(ASSIGNED, "first")), vec![]);
        assert_eq!(c.on_close(4408), vec![]);
        assert_eq!(c.poll(t(10_000_000)), vec![]);
    }
}
