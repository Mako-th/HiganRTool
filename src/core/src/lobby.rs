//! ルーム名ごとの一覧（`src/server/src/lobby.ts`）のクライアントの純粋な部品。
//! - 一覧の接続（[`LobbyClient`]）: 接続すると一覧の全体が届く。変わるたびに全体が届く。
//!   `create`を送ると待機中の1件として載る。割り当てのルーム名（`r.…`）が返る。載せている間はこの接続を持ち続ける。
//! - 試合中の1件を載せる接続: マッチのホストが成立の後に別の接続で`post`を送る。手順は[`crate::watch_list::HostClient::new_lobby`]（ランダム対戦の一覧と同じ手順）。
//!
//! 割り当てのルーム名・チケットは画面にもログにも出さない（`Debug`は伏せる）。
//! 待ち列・ルームの手順と同じくソケットも時計も持たない。監視のスレッドで回す。
#![forbid(unsafe_code)]

use crate::room::{
    CloseCode, Failure, Fields, JsonAt, JsonValue, MESSAGE_MAX_BYTES, MsgError, Now,
    ROOM_GREETING_TIMEOUT_MS, RoomName, SeatTicket, Why, classify_http, parse_flat_with_items,
    push_percent_encoded,
};
use crate::watch_list::{
    HostMsg, ListStatus, Pings, PostFields, WATCH_NAME_MAX, WATCH_ROW_WORDS, WatchName, WatchRow,
    close_failure, error_failure, json_escaped, parse_host_message, post_text, read_row, wake_min,
};
use crate::wire::Mode;

// ---------------- サーバーの決まり（見本tests/fixtures/room/rules.jsonのlobby_*） ----------------

/// 一覧のパスの先頭。後ろにビルド番号（10進）とルーム名（パーセント符号化）が付く（`rules.ts`の`LOBBY_PATH_PREFIX`）。
pub const LOBBY_PATH_PREFIX: &str = "/v0/lobby/";
/// 待機中の1件へ割り当てる2席のルームの名の先頭と続く乱数の16進の文字数（`rules.ts`の`LOBBY_ROOM_PREFIX` / `LOBBY_ROOM_HEX_LEN`）。
/// 利用者は`r.`で始まる名を打てない（[`RoomName::parse`]が断る）ので打ったルーム名とぶつからない。
pub const LOBBY_ROOM_PREFIX: &str = "r.";
pub const LOBBY_ROOM_HEX_LEN: usize = 30;
/// 1つの一覧の件数の上限（待機中と試合中の合計。サーバーの`LOBBY_MAX_TOTAL`）。
pub const LOBBY_ROWS_MAX: usize = 20;
/// 一覧の1通の上限（バイト。サーバーの`LOBBY_LIST_MAX_BYTES`）。
pub const LOBBY_LIST_MAX_BYTES: usize = 8 * 1024;

const _: () = assert!(LOBBY_ROOM_PREFIX.len() + LOBBY_ROOM_HEX_LEN == crate::room::ROOM_NAME_MAX);

/// 一覧へ接続するパス（`/v0/lobby/<ビルド番号>/<ルーム名>`）。
/// ビルド番号は10進で先頭に0を付けない。ルーム名はルームのパスと同じ符号化（サーバーが揃えるので打った文字のまま渡す）。
pub fn lobby_path(build: u32, name: &RoomName) -> String {
    let mut out = format!("{LOBBY_PATH_PREFIX}{build}/");
    push_percent_encoded(&mut out, name.as_str());
    out
}

// ---------------- 割り当てのルーム名 ----------------

/// 待機中の1件へサーバーが割り当てた2席のルームの名。形は`r.` ＋ 小文字の16進30文字だけ（それ以外は受けない）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LobbyRoom(RoomName);

impl core::fmt::Debug for LobbyRoom {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("LobbyRoom(..)")
    }
}

impl LobbyRoom {
    /// サーバーから届いた名を検証する。形が違えば`None`。
    pub fn parse(raw: &str) -> Option<LobbyRoom> {
        let hex = raw.strip_prefix(LOBBY_ROOM_PREFIX)?;
        let shape_ok = hex.len() == LOBBY_ROOM_HEX_LEN
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !shape_ok {
            return None;
        }
        RoomName::parse_server_rules(raw).ok().map(LobbyRoom)
    }

    /// 2席のルームへ接続するためのルーム名。
    pub fn name(&self) -> RoomName {
        self.0
    }

    /// スレッドの間のバッファ（アトミック変数のワード）に詰める。書き込むのは`r.`の後ろの16進だけ。
    pub fn to_words(&self) -> [u32; LOBBY_ROOM_WORDS] {
        let mut bytes = [0u8; LOBBY_ROOM_WORDS * 4];
        let hex = self
            .0
            .as_str()
            .as_bytes()
            .get(LOBBY_ROOM_PREFIX.len()..)
            .unwrap_or(&[]);
        for (d, s) in bytes.iter_mut().zip(hex) {
            *d = *s;
        }
        let mut w = [0u32; LOBBY_ROOM_WORDS];
        for (d, c) in w.iter_mut().zip(bytes.chunks(4)) {
            *d = u32::from_le_bytes(c.try_into().unwrap_or([0; 4]));
        }
        w
    }

    /// `to_words`の逆。形が違えば`None`（全部0のワードも読めない）。
    /// ゲームのスレッドが一覧を読むときに通るので確保しない（`r.`と16進をスタックの配列に並べる）。
    pub fn from_words(w: &[u32; LOBBY_ROOM_WORDS]) -> Option<LobbyRoom> {
        const P: usize = LOBBY_ROOM_PREFIX.len();
        let mut bytes = [0u8; P + LOBBY_ROOM_WORDS * 4];
        bytes[..P].copy_from_slice(LOBBY_ROOM_PREFIX.as_bytes());
        for (c, v) in bytes[P..].chunks_mut(4).zip(w) {
            c.copy_from_slice(&v.to_le_bytes());
        }
        let s = core::str::from_utf8(bytes.get(..P + LOBBY_ROOM_HEX_LEN)?).ok()?;
        LobbyRoom::parse(s)
    }
}

/// [`LobbyRoom::to_words`]のワードの数。
pub const LOBBY_ROOM_WORDS: usize = LOBBY_ROOM_HEX_LEN.div_ceil(4);

// ---------------- 一覧の行 ----------------

/// 待機中の1件。選ぶと[`LobbyRoom`]の2席のルームへguestとして入る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitRow {
    pub name: WatchName,
    pub room: LobbyRoom,
}

/// 試合中の1件。`row`の観戦用のルーム名（`w.…`）へ観戦者として入る。観戦を許可していないペア（`watch`が偽）も載る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayRow {
    pub row: WatchRow,
    pub watch: bool,
}

impl PlayRow {
    /// 観戦に入れるかどうか（許可していて満員でない）。
    pub fn can_watch(&self) -> bool {
        self.watch && !self.row.is_full()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyRow {
    Wait(WaitRow),
    Play(PlayRow),
}

impl LobbyRow {
    pub const EMPTY: LobbyRow = LobbyRow::Play(PlayRow {
        row: WatchRow::EMPTY,
        watch: false,
    });

    /// 一覧の接続のスレッドがゲームのスレッドへ渡すバッファに詰める。
    /// ワード0は種類（1 = 待機中・2 = 試合中）と観戦の許可（ビット8）。
    /// 待機中は名前8・名前の長さ1・割り当てのルーム名8を続ける。試合中は[`WatchRow::to_words`]の20を続ける。
    pub fn to_words(&self) -> [u32; LOBBY_ROW_WORDS] {
        let mut w = [0u32; LOBBY_ROW_WORDS];
        match self {
            LobbyRow::Wait(r) => {
                w[0] = ROW_TAG_WAIT;
                let mut bytes = [0u8; WATCH_NAME_MAX];
                for (d, s) in bytes.iter_mut().zip(r.name.as_str().as_bytes()) {
                    *d = *s;
                }
                for (d, c) in w[1..=NAME_WORDS].iter_mut().zip(bytes.chunks(4)) {
                    *d = u32::from_le_bytes(c.try_into().unwrap_or([0; 4]));
                }
                w[1 + NAME_WORDS] = r.name.len() as u32;
                let at = 2 + NAME_WORDS;
                w[at..at + LOBBY_ROOM_WORDS].copy_from_slice(&r.room.to_words());
            }
            LobbyRow::Play(p) => {
                w[0] = ROW_TAG_PLAY | (u32::from(p.watch) << 8);
                w[1..].copy_from_slice(&p.row.to_words());
            }
        }
        w
    }

    /// `to_words`の逆。種類・名前・ルーム名・数が決まりの外なら`None`（書きかけのワードを取り違えない保険）。
    pub fn from_words(w: &[u32; LOBBY_ROW_WORDS]) -> Option<LobbyRow> {
        match w[0] & 0xFF {
            ROW_TAG_WAIT => {
                let mut bytes = [0u8; WATCH_NAME_MAX];
                for (c, v) in bytes.chunks_mut(4).zip(&w[1..=NAME_WORDS]) {
                    c.copy_from_slice(&v.to_le_bytes());
                }
                let len = usize::try_from(w[1 + NAME_WORDS]).ok()?;
                let name = WatchName::parse(core::str::from_utf8(bytes.get(..len)?).ok()?)?;
                let at = 2 + NAME_WORDS;
                let room = LobbyRoom::from_words(w[at..at + LOBBY_ROOM_WORDS].try_into().ok()?)?;
                Some(LobbyRow::Wait(WaitRow { name, room }))
            }
            ROW_TAG_PLAY => Some(LobbyRow::Play(PlayRow {
                row: WatchRow::from_words(w[1..].try_into().ok()?)?,
                watch: (w[0] >> 8) & 1 == 1,
            })),
            _ => None,
        }
    }
}

const ROW_TAG_WAIT: u32 = 1;
const ROW_TAG_PLAY: u32 = 2;
const NAME_WORDS: usize = WATCH_NAME_MAX / 4;

/// [`LobbyRow::to_words`]のワードの数（種類1 ＋ 試合中の行の20。待機中の行は18で収まる）。
pub const LOBBY_ROW_WORDS: usize = 1 + WATCH_ROW_WORDS;
const _: () = assert!(2 + NAME_WORDS + LOBBY_ROOM_WORDS <= LOBBY_ROW_WORDS);

// ---------------- ゲームのスレッドが読む一覧 ----------------

/// メニューが読む一覧のコピー（ゲームのスレッドで確保しない固定の配列）。書き手は一覧の接続の監視の処理。
/// 持ち方は[`WatchSnapshot`]と同じ。行は並びのまま（サーバーの順: 待機中 → 試合中）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LobbySnapshot {
    pub status: ListStatus,
    len: usize,
    rows: [LobbyRow; LOBBY_ROWS_MAX],
}

impl LobbySnapshot {
    pub const EMPTY: LobbySnapshot = LobbySnapshot {
        status: ListStatus::Loading,
        len: 0,
        rows: [LobbyRow::EMPTY; LOBBY_ROWS_MAX],
    };

    pub fn rows(&self) -> &[LobbyRow] {
        self.rows.get(..self.len).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 行を入れ替える（`LOBBY_ROWS_MAX`を越える分は捨てる）。
    pub fn set(&mut self, status: ListStatus, rows: &[LobbyRow]) {
        self.status = status;
        self.len = rows.len().min(LOBBY_ROWS_MAX);
        for (d, s) in self.rows.iter_mut().zip(rows) {
            *d = *s;
        }
    }

    /// 検査と読み手が1行ずつ足すとき用。満杯なら足さずにfalse。
    pub fn push(&mut self, row: LobbyRow) -> bool {
        match self.rows.get_mut(self.len) {
            Some(slot) => {
                *slot = row;
                self.len += 1;
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for LobbySnapshot {
    fn default() -> Self {
        LobbySnapshot::EMPTY
    }
}

fn read_lobby_row(pairs: &[(std::borrow::Cow<'_, str>, JsonValue<'_>)]) -> Option<LobbyRow> {
    let f = Fields(pairs);
    match f.str("kind").ok()? {
        "wait" => Some(LobbyRow::Wait(WaitRow {
            name: WatchName::parse(f.str("name").ok()?)?,
            room: LobbyRoom::parse(f.str("room").ok()?)?,
        })),
        "play" => {
            let watch = match f.get("watch") {
                Some(JsonValue::Bool(b)) => *b,
                _ => return None,
            };
            Some(LobbyRow::Play(PlayRow {
                row: read_row(pairs)?,
                watch,
            }))
        }
        _ => None,
    }
}

// ---------------- サーバーから届く文 ----------------

/// 一覧の接続に届く文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyMsg {
    /// 一覧の全体。決まりの外の行は捨て、その数を`skipped`に数える。サーバーは出さないので増えたらサーバーか途中が壊れている。
    List {
        rows: Vec<LobbyRow>,
        skipped: usize,
    },
    /// `create`が通った。割り当てのルーム名。
    Created {
        room: LobbyRoom,
    },
    Pong,
    /// 閉じる直前の1通。知らない名は`None`。
    Error {
        code: Option<CloseCode>,
    },
}

pub fn parse_lobby_message(text: &str) -> Result<LobbyMsg, MsgError> {
    if text.len() > LOBBY_LIST_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: text.len() });
    }
    let (head, items) = parse_flat_with_items(text, "items", LOBBY_ROWS_MAX)
        .map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    let f = Fields(&head);
    match f.str("t")? {
        "list" => {
            // `items`が配列でなければ（キーが無い・別の型）一覧の文ではない。
            if !matches!(f.get("items"), Some(JsonValue::Other)) {
                return Err(MsgError::Missing("items"));
            }
            let rows: Vec<LobbyRow> = items.iter().filter_map(|p| read_lobby_row(p)).collect();
            let skipped = items.len() - rows.len();
            Ok(LobbyMsg::List { rows, skipped })
        }
        // 一覧以外の文は短い。長いものは中を読まずに断る。
        _ if text.len() > MESSAGE_MAX_BYTES => Err(MsgError::TooLarge { bytes: text.len() }),
        "created" => {
            let room = LobbyRoom::parse(f.str("room")?).ok_or(MsgError::BadValue("room"))?;
            Ok(LobbyMsg::Created { room })
        }
        "pong" => Ok(LobbyMsg::Pong),
        "error" => Ok(LobbyMsg::Error {
            code: CloseCode::from_name(f.str("code")?),
        }),
        _ => Err(MsgError::UnknownType),
    }
}

/// 試合中の1件を載せる接続に届く文を読む。
/// ルーム名ごとの一覧は接続の直後に一覧の全体を送る（載せる接続も最初は見る接続と区別がつかない）ので`list`は読み捨てる。
/// ほかは[`parse_host_message`]と同じ（`listed`・`pong`・`error`）。
pub(crate) fn parse_lobby_host_message(text: &str) -> Result<HostMsg, MsgError> {
    if text.len() > LOBBY_LIST_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: text.len() });
    }
    let (head, _) = parse_flat_with_items(text, "items", LOBBY_ROWS_MAX)
        .map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    if Fields(&head).str("t")? == "list" {
        return Ok(HostMsg::Ignored);
    }
    parse_host_message(text)
}

// ---------------- 試合中を載せる文 ----------------

/// 試合中の1件を載せる接続の最初の1通の中身。
/// `room`はその2席のルームの名（`create`で割り当てられたもの。サーバーが`w.…`を導く元。画面にもログにも出さない）。
/// `ticket`はその2席のルームで対戦が成立したときに受けたチケット（サーバーは導いた`w.…`について正しいチケットの投稿だけを載せる）。
#[derive(Clone, Copy)]
pub struct LobbyPosting {
    pub room: LobbyRoom,
    pub p1: WatchName,
    pub p2: WatchName,
    pub mode: Mode,
    pub delay: u8,
    /// 観戦を許可しているペアかどうか（許可していないペアも載せて、観戦の行を選べなくする）。
    pub watch: bool,
    pub cap: u8,
    pub ticket: SeatTicket,
}

impl core::fmt::Debug for LobbyPosting {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("LobbyPosting(..)")
    }
}

/// `{"t":"post",…,"watch":…}`の文を作る（エスケープの決まりは`watch_list::write_post`と同じ）。
pub fn write_lobby_post(p: &LobbyPosting, watchers: u8) -> String {
    post_text(&PostFields {
        room: p.room.name().as_str(),
        p1: p.p1,
        p2: p.p2,
        mode: p.mode,
        delay: p.delay,
        watch: Some(p.watch),
        watchers,
        cap: p.cap,
        ticket: p.ticket.as_str(),
    })
}

/// `{"t":"create","name":"…"}`の文を作る（名前は`"` `\`を含みうるのでエスケープする）。
pub fn write_create(name: &WatchName) -> String {
    format!(
        r#"{{"t":"create","name":"{}"}}"#,
        json_escaped(name.as_str())
    )
}

// ---------------- 一覧の接続のステートマシン ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyEvent {
    /// 一覧の全体（接続した直後と変わるたびに届く）。
    List { rows: Vec<LobbyRow>, skipped: usize },
    /// `create`が通って待機中の1件として載った。割り当てのルーム名の2席のルームへ入って待つ。
    Created { room: LobbyRoom },
    /// 手順が終わった（一覧から消えた）。`ListFull`は件数の上限（全体か1 IPあたり）。`RoomFull`は見る接続の上限。
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyAction {
    /// この文字をそのままテキストの文で送る（create・ping）。
    Send(String),
    Notify(LobbyEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyState {
    Connecting,
    /// 101の後に最初の一覧を待っている。
    AwaitList,
    /// 一覧を受け取っている（見るだけ）。
    Viewing,
    /// `create`を送った後に`created`を待っている。
    AwaitCreated,
    /// `created`を受けた。切れるまで待機中の1件として載っている。
    Waiting,
    Failed(Failure),
}

/// 一覧の接続の手順。
/// 呼ぶ順: 接続する前に`new`。101なら`on_upgraded`。101でなければ`on_http_refused`。接続できなければ`on_transport_failed`。
/// 以後は受けた文を`on_text`に渡し、閉じる番号を`on_close`に渡す。最初の一覧が届いた後（`Viewing`）に`create`を呼ぶと待機中の1件として載る。
/// `next_wake`の時刻かそれより前に`poll`を呼ぶ。終わりは失敗だけ（載せている間も見ている間も閉じない）。
/// 一覧の接続を閉じるのはこちらの手順の外（入る・観戦に入る・やめるときに接続を捨てる）。
#[derive(Debug, Clone)]
pub struct LobbyClient {
    state: LobbyState,
    deadline_ms: Option<u64>,
    pings: Pings,
}

impl Default for LobbyClient {
    fn default() -> Self {
        LobbyClient::new()
    }
}

impl LobbyClient {
    pub fn new() -> LobbyClient {
        LobbyClient {
            state: LobbyState::Connecting,
            deadline_ms: None,
            pings: Pings::default(),
        }
    }

    pub fn state(&self) -> LobbyState {
        self.state
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, LobbyState::Failed(_))
    }

    pub fn next_wake(&self) -> Option<u64> {
        wake_min(&[
            self.pings.next_ping_ms,
            self.deadline_ms,
            self.pings.silence_deadline(),
        ])
    }

    fn fail(&mut self, f: Failure) -> Vec<LobbyAction> {
        self.state = LobbyState::Failed(f);
        self.deadline_ms = None;
        self.pings.stop();
        vec![LobbyAction::Notify(LobbyEvent::Failed(f))]
    }

    fn out_of_order(&mut self) -> Vec<LobbyAction> {
        self.fail(Failure::Unresponsive(Why::BadMessage(MsgError::BadValue(
            "t",
        ))))
    }

    pub fn on_upgraded(&mut self, now: Now) -> Vec<LobbyAction> {
        if self.state != LobbyState::Connecting {
            return Vec::new();
        }
        self.state = LobbyState::AwaitList;
        self.deadline_ms = Some(now.ms.saturating_add(ROOM_GREETING_TIMEOUT_MS));
        self.pings.start(now);
        Vec::new()
    }

    pub fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<LobbyAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(classify_http(status, body))
    }

    pub fn on_transport_failed(&mut self) -> Vec<LobbyAction> {
        match self.state {
            LobbyState::Failed(_) => Vec::new(),
            LobbyState::Connecting => self.fail(Failure::Unresponsive(Why::NoAnswer)),
            _ => self.fail(Failure::Unresponsive(Why::Lost)),
        }
    }

    pub fn on_close(&mut self, code: u16) -> Vec<LobbyAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(close_failure(code))
    }

    /// 待機中の1件として載る。最初の一覧が届いて見ている間（`Viewing`）だけ送る（それ以外は何もしない）。
    pub fn create(&mut self, now: Now, name: &WatchName) -> Vec<LobbyAction> {
        if self.state != LobbyState::Viewing {
            return Vec::new();
        }
        self.state = LobbyState::AwaitCreated;
        self.deadline_ms = Some(now.ms.saturating_add(ROOM_GREETING_TIMEOUT_MS));
        vec![LobbyAction::Send(write_create(name))]
    }

    pub fn on_text(&mut self, _now: Now, text: &str) -> Vec<LobbyAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.pings.unanswered_ms = None;
        let msg = match parse_lobby_message(text) {
            Ok(m) => m,
            Err(e) => return self.fail(Failure::Unresponsive(Why::BadMessage(e))),
        };
        match (self.state, msg) {
            (LobbyState::Connecting, _) => self.out_of_order(),
            (_, LobbyMsg::Pong) => Vec::new(),
            (_, LobbyMsg::Error { code }) => self.fail(error_failure(code)),
            (LobbyState::AwaitList | LobbyState::Viewing, LobbyMsg::List { rows, skipped }) => {
                self.state = LobbyState::Viewing;
                self.deadline_ms = None;
                vec![LobbyAction::Notify(LobbyEvent::List { rows, skipped })]
            }
            // `create`を送った後でもサーバーが先に送っていた一覧が届くことがある。
            (LobbyState::AwaitCreated | LobbyState::Waiting, LobbyMsg::List { .. }) => Vec::new(),
            (LobbyState::AwaitCreated, LobbyMsg::Created { room }) => {
                self.state = LobbyState::Waiting;
                self.deadline_ms = None;
                vec![LobbyAction::Notify(LobbyEvent::Created { room })]
            }
            _ => self.out_of_order(),
        }
    }

    pub fn poll(&mut self, now: Now) -> Vec<LobbyAction> {
        if self.is_finished() || self.state == LobbyState::Connecting {
            return Vec::new();
        }
        if let Some(d) = self.deadline_ms
            && now.ms >= d
        {
            return self.fail(Failure::Unresponsive(Why::NoRoom));
        }
        if self.pings.silent(now) {
            return self.fail(Failure::Unresponsive(Why::NoPong));
        }
        match self.pings.due(now) {
            Some(p) => vec![LobbyAction::Send(p)],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
