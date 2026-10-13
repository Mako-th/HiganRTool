//! 進行中のランダム対戦の一覧（`src/server/src/watch_list.ts`）のクライアントの純粋な部品。
//!
//! - **載せる口**（[`HostClient`]）: ランダム対戦のホストが持ち続ける。繋いだら割り当てのルーム名（`q.…`）・1P と 2P の名前・同期方式・
//!   観戦者の数と上限を `post` で送り、`listed` を待つ。以後は観戦者の数が変わるたびに `count` を送る。切れたら一覧から消える。
//! - **見る口**（[`ViewClient`]）: 観戦者が一覧を開いている間だけ持つ。繋ぐと一覧の全体が届き、変わるたびに全体が届く。
//!
//! サーバーは観戦用のルーム名（`w.…`）を割り当ての名から自分で導き、一覧には `w.…` だけを載せる。割り当ての名は画面にもログにも出さない
//! （[`crate::room_queue::AssignedRoom`] の `Debug` は名を伏せる）。待ち列・ルームの手順と同じく、ソケットも時計も持たない。
//!
//! 見張りのスレッドで回す。ゲームのスレッドでは呼ばない（一覧をゲームのスレッドへ渡す入れ物は [`WatchSnapshot`]・[`WatchRow::to_words`]）。
#![forbid(unsafe_code)]

use crate::room::{
    CloseCode, Failure, Fields, JsonAt, JsonValue, MESSAGE_MAX_BYTES, MsgError, Now,
    PING_INTERVAL_MS, PING_TEXT, PONG_TIMEOUT_MS, ROOM_GREETING_TIMEOUT_MS, RoomName, Why,
    classify_http, hex_lower, parse_flat, parse_flat_with_items,
};
use crate::room_queue::AssignedRoom;
use crate::spectate::MAX_SPECTATORS;
use crate::wire::Mode;

// ---------------- サーバーの決まり（見本 tests/fixtures/room/rules.json の watch_*） ----------------

/// 一覧のパスの先頭。後ろにビルド番号（10 進）と `post` か `view` が付く（`rules.ts` の `WATCH_PATH_PREFIX`）。
pub const WATCH_PATH_PREFIX: &str = "/v0/watch/";
pub const WATCH_POST_SEGMENT: &str = "post";
pub const WATCH_VIEW_SEGMENT: &str = "view";
/// 一覧の件数の上限（サーバーが全体で受ける数）。
pub const WATCH_ROWS_MAX: usize = 100;
/// 観戦者の上限として受ける値（サーバーの `WATCH_CAP_MAX`。観戦のホストの上限と同じ）。
pub const WATCH_CAP_MAX: usize = MAX_SPECTATORS;
/// 受ける遅延フレーム数の最大（サーバーの `WATCH_DELAY_MAX`）。
pub const WATCH_DELAY_MAX: u8 = 15;
/// プレイヤー名の文字数の上限（サーバーの `WATCH_NAME_MAX`。HELLO の name の上限と同じ）。
pub const WATCH_NAME_MAX: usize = crate::wire::NAME_MAX;
/// 一覧の 1 通の上限（バイト。サーバーの `WATCH_LIST_MAX_BYTES`）。ふつうの 1 通は 2 KB に収まらないので、ルームの上限とは別に持つ。
pub const WATCH_LIST_MAX_BYTES: usize = 32 * 1024;
/// 観戦用のルーム名の先頭と、続く 16 進の文字数（[`RoomName::spectate_of`] と同じ）。
const SPECTATE_PREFIX: &str = crate::room::SPECTATE_ROOM_PREFIX;
const HASH_BYTES: usize = 8;

const _: () = assert!(WATCH_CAP_MAX == 10);

/// ランダムマッチのホストがこのマッチを一覧に載せてよいか。`own` は自分の `[spectate] enable = 1`、`peer` は相手が HELLO の
/// +3 の bit2 で名乗った同じ設定。片方でも断れば載せない（一覧に載らなければ観戦用のルームの名を知る人もいないので、配る側も起こさない）。
/// ルームマッチの観戦はこれを見ない（ホストの設定だけで決まる）。
pub fn may_list(own: bool, peer: bool) -> bool {
    own && peer
}

/// 載せる口のパス（`/v0/watch/<ビルド番号>/post`。ビルド番号は 10 進で先頭に 0 を付けない）。
pub fn watch_post_path(build: u32) -> String {
    format!("{WATCH_PATH_PREFIX}{build}/{WATCH_POST_SEGMENT}")
}

/// 見る口のパス（`/v0/watch/<ビルド番号>/view`）。
pub fn watch_view_path(build: u32) -> String {
    format!("{WATCH_PATH_PREFIX}{build}/{WATCH_VIEW_SEGMENT}")
}

// ---------------- 名前 ----------------

/// 一覧に載せるプレイヤー名。サーバーが受ける文字（ASCII の英数字・`-`・`_`・`@` と、受けない文字の置き換えの `?`）の 1〜32 文字だけ。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WatchName {
    bytes: [u8; WATCH_NAME_MAX],
    len: u8,
}

impl core::fmt::Debug for WatchName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "WatchName({:?})", self.as_str())
    }
}

fn name_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'@' | b'?')
}

impl WatchName {
    pub const EMPTY: WatchName = WatchName {
        bytes: [0; WATCH_NAME_MAX],
        len: 0,
    };

    /// サーバーの決まりどおりの名前だけを受ける（読み手用。ずれた名は行ごと捨てる）。
    pub fn parse(s: &str) -> Option<WatchName> {
        let b = s.as_bytes();
        if b.is_empty() || b.len() > WATCH_NAME_MAX || !b.iter().all(|&c| name_byte_ok(c)) {
            return None;
        }
        let mut bytes = [0u8; WATCH_NAME_MAX];
        bytes.get_mut(..b.len())?.copy_from_slice(b);
        Some(WatchName {
            bytes,
            len: b.len() as u8,
        })
    }

    /// 任意の名前（プレイヤー名の文字の決まりの外を含む）を載せられる形にする（書き手用）: 前後の空白を除き、使えない文字は `?` 1 つに
    /// して 32 文字で切る。空になれば `?`。
    pub fn lossy(s: &str) -> WatchName {
        let mut bytes = [0u8; WATCH_NAME_MAX];
        let mut n = 0usize;
        for c in s.trim_matches(' ').chars().take(WATCH_NAME_MAX) {
            let b = if c.is_ascii() && name_byte_ok(c as u8) {
                c as u8
            } else {
                b'?'
            };
            if let Some(slot) = bytes.get_mut(n) {
                *slot = b;
                n += 1;
            }
        }
        if n == 0 {
            bytes[0] = b'?';
            n = 1;
        }
        WatchName {
            bytes,
            len: n as u8,
        }
    }

    pub fn as_str(&self) -> &str {
        // 入るのは ASCII だけ。
        core::str::from_utf8(self.bytes.get(..usize::from(self.len)).unwrap_or(&[])).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// ---------------- 一覧の 1 行 ----------------

/// 一覧の 1 行（進行中のランダム対戦 1 つ）。観戦用のルーム名はハッシュの 8 B で持つ（`w.` ＋ 16 進 16 文字）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchRow {
    pub hash: [u8; HASH_BYTES],
    pub p1: WatchName,
    pub p2: WatchName,
    pub mode: Mode,
    pub delay: u8,
    pub watchers: u8,
    pub cap: u8,
}

/// `WatchRow::to_words` の語の数（ハッシュ 2・名前 8 + 8・名前の長さ ＋ 方式 ＋ 遅延 1・観戦者の数と上限 1）。
pub const WATCH_ROW_WORDS: usize = 2 + WATCH_NAME_MAX / 4 + WATCH_NAME_MAX / 4 + 1 + 1;
const _: () = assert!(WATCH_NAME_MAX.is_multiple_of(4));

impl WatchRow {
    pub const EMPTY: WatchRow = WatchRow {
        hash: [0; HASH_BYTES],
        p1: WatchName::EMPTY,
        p2: WatchName::EMPTY,
        mode: Mode::Delay,
        delay: 0,
        watchers: 0,
        cap: 0,
    };

    /// 満員（観戦者が上限に達した）。メニューは選べなくする。
    pub fn is_full(&self) -> bool {
        self.watchers >= self.cap
    }

    /// 観戦用のルーム名（`w.` ＋ ハッシュの 16 進）。観戦者はこの名で今の観戦の入り方（観戦用のルームの guest）へ入る。
    pub fn room_name(&self) -> Option<RoomName> {
        let mut s = String::with_capacity(SPECTATE_PREFIX.len() + 2 * HASH_BYTES);
        s.push_str(SPECTATE_PREFIX);
        for b in self.hash {
            s.push(char::from(hex_lower(b >> 4)));
            s.push(char::from(hex_lower(b & 0x0F)));
        }
        RoomName::parse_server_rules(&s).ok()
    }

    /// ゲームのスレッドへ渡す入れ物（原子変数の語）に詰める。
    pub fn to_words(&self) -> [u32; WATCH_ROW_WORDS] {
        let mut w = [0u32; WATCH_ROW_WORDS];
        let le = |c: &[u8]| u32::from_le_bytes(c.try_into().unwrap_or([0; 4]));
        let mut i = 0;
        let mut bytes = [0u8; HASH_BYTES + 2 * WATCH_NAME_MAX];
        for (d, s) in bytes
            .iter_mut()
            .zip(self.hash.iter().chain(&self.p1.bytes).chain(&self.p2.bytes))
        {
            *d = *s;
        }
        for chunk in bytes.chunks(4) {
            w[i] = le(chunk);
            i += 1;
        }
        let mode = u32::from(self.mode == Mode::Rollback);
        // 語 18: 1P の長さ（0〜7 ビット）・2P の長さ（8〜15）・方式（16）・遅延（24〜31）。
        w[i] = u32::from(self.p1.len)
            | (u32::from(self.p2.len) << 8)
            | (mode << 16)
            | (u32::from(self.delay) << 24);
        w[i + 1] = u32::from(self.watchers) | (u32::from(self.cap) << 8);
        w
    }

    /// `to_words` の逆。長さ・文字・数が決まりの外なら `None`（読み出しの途中で別の書き手が書いた語を取り違えない保険）。
    pub fn from_words(w: &[u32; WATCH_ROW_WORDS]) -> Option<WatchRow> {
        let mut hash = [0u8; HASH_BYTES];
        for (c, v) in hash.chunks_mut(4).zip(&w[0..2]) {
            c.copy_from_slice(&v.to_le_bytes());
        }
        let name = |words: &[u32], len: u32| -> Option<WatchName> {
            let mut bytes = [0u8; WATCH_NAME_MAX];
            for (c, v) in bytes.chunks_mut(4).zip(words) {
                c.copy_from_slice(&v.to_le_bytes());
            }
            let len = usize::try_from(len).ok()?;
            let n = WatchName::parse(core::str::from_utf8(bytes.get(..len)?).ok()?)?;
            Some(n)
        };
        let meta = w[18];
        let p1 = name(&w[2..10], meta & 0xFF)?;
        let p2 = name(&w[10..18], (meta >> 8) & 0xFF)?;
        let mode = if (meta >> 16) & 1 == 1 {
            Mode::Rollback
        } else {
            Mode::Delay
        };
        let delay = u8::try_from(meta >> 24)
            .ok()
            .filter(|d| *d <= WATCH_DELAY_MAX)?;
        let watchers = (w[19] & 0xFF) as u8;
        let cap = ((w[19] >> 8) & 0xFF) as u8;
        (cap >= 1 && usize::from(cap) <= WATCH_CAP_MAX && watchers <= cap).then_some(WatchRow {
            hash,
            p1,
            p2,
            mode,
            delay,
            watchers,
            cap,
        })
    }
}

fn mode_wire(m: Mode) -> &'static str {
    match m {
        Mode::Rollback => "rollback",
        Mode::Delay => "delay",
    }
}

fn parse_hash(room: &str) -> Option<[u8; HASH_BYTES]> {
    let hex = room.strip_prefix(SPECTATE_PREFIX)?;
    if hex.len() != 2 * HASH_BYTES {
        return None;
    }
    let mut out = [0u8; HASH_BYTES];
    for (o, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        *o = (digit(*pair.first()?)? << 4) | digit(*pair.get(1)?)?;
    }
    Some(out)
}

fn int_in(f: &Fields<'_, '_>, key: &'static str, lo: i64, hi: i64) -> Option<i64> {
    match f.get(key) {
        Some(JsonValue::Int(n)) if (lo..=hi).contains(n) => Some(*n),
        _ => None,
    }
}

/// サーバーの一覧の 1 行を読む。決まりの外なら `None`。
fn read_row(pairs: &[(std::borrow::Cow<'_, str>, JsonValue<'_>)]) -> Option<WatchRow> {
    let f = Fields(pairs);
    let hash = parse_hash(f.str("room").ok()?)?;
    let p1 = WatchName::parse(f.str("p1").ok()?)?;
    let p2 = WatchName::parse(f.str("p2").ok()?)?;
    let mode = match f.str("mode").ok()? {
        "rollback" => Mode::Rollback,
        "delay" => Mode::Delay,
        _ => return None,
    };
    let delay = int_in(&f, "delay", 0, i64::from(WATCH_DELAY_MAX))? as u8;
    let cap = int_in(&f, "cap", 1, WATCH_CAP_MAX as i64)? as u8;
    let watchers = int_in(&f, "watchers", 0, i64::from(cap))? as u8;
    Some(WatchRow {
        hash,
        p1,
        p2,
        mode,
        delay,
        watchers,
        cap,
    })
}

// ---------------- ゲームのスレッドが読む一覧 ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStatus {
    /// まだ一覧が届いていない（繋いでいる）。
    #[default]
    Loading,
    /// 一覧が届いている（0 件でもここ）。
    Ready,
    /// 繋げなかった・切れた。
    Failed,
}

/// メニューが読む一覧（ゲームのスレッドで確保しない固定の配列）。書き手は見張りの側。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchSnapshot {
    pub status: ListStatus,
    len: usize,
    rows: [WatchRow; WATCH_ROWS_MAX],
}

impl WatchSnapshot {
    pub const EMPTY: WatchSnapshot = WatchSnapshot {
        status: ListStatus::Loading,
        len: 0,
        rows: [WatchRow::EMPTY; WATCH_ROWS_MAX],
    };

    pub fn rows(&self) -> &[WatchRow] {
        self.rows.get(..self.len).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 行を置き替える（`WATCH_ROWS_MAX` を越える分は捨てる）。
    pub fn set(&mut self, status: ListStatus, rows: &[WatchRow]) {
        self.status = status;
        self.len = rows.len().min(WATCH_ROWS_MAX);
        for (d, s) in self.rows.iter_mut().zip(rows) {
            *d = *s;
        }
    }

    /// 検査と読み手が 1 行ずつ足すとき用。満杯なら足さずに false。
    pub fn push(&mut self, row: WatchRow) -> bool {
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

impl Default for WatchSnapshot {
    fn default() -> Self {
        WatchSnapshot::EMPTY
    }
}

// ---------------- サーバーから届く文 ----------------

/// 載せる口に届く文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMsg {
    /// `post` が通って一覧に載った。
    Listed,
    Pong,
    /// 閉じる直前の 1 通。知らない名は `None`。
    Error {
        code: Option<CloseCode>,
    },
}

pub fn parse_host_message(text: &str) -> Result<HostMsg, MsgError> {
    if text.len() > MESSAGE_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: text.len() });
    }
    let list = parse_flat(text).map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    let f = Fields(&list);
    match f.str("t")? {
        "listed" => Ok(HostMsg::Listed),
        "pong" => Ok(HostMsg::Pong),
        "error" => Ok(HostMsg::Error {
            code: CloseCode::from_name(f.str("code")?),
        }),
        _ => Err(MsgError::UnknownType),
    }
}

/// 見る口に届く文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewMsg {
    /// 一覧の全体。決まりの外の行は捨てて、その数を `skipped` に（サーバーは出さないので、増えたらサーバーか途中が壊れている）。
    List {
        rows: Vec<WatchRow>,
        skipped: usize,
    },
    Pong,
    Error {
        code: Option<CloseCode>,
    },
}

pub fn parse_view_message(text: &str) -> Result<ViewMsg, MsgError> {
    if text.len() > WATCH_LIST_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: text.len() });
    }
    let (head, items) = parse_flat_with_items(text, "items", WATCH_ROWS_MAX)
        .map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    let f = Fields(&head);
    match f.str("t")? {
        "list" => {
            // `items` が配列でなければ（鍵が無い・別の型）一覧の文ではない。
            if !matches!(f.get("items"), Some(JsonValue::Other)) {
                return Err(MsgError::Missing("items"));
            }
            let rows: Vec<WatchRow> = items.iter().filter_map(|p| read_row(p)).collect();
            let skipped = items.len() - rows.len();
            Ok(ViewMsg::List { rows, skipped })
        }
        "pong" => Ok(ViewMsg::Pong),
        "error" => Ok(ViewMsg::Error {
            code: CloseCode::from_name(f.str("code")?),
        }),
        _ => Err(MsgError::UnknownType),
    }
}

/// 載せる口の最初の 1 通の中身。割り当てのルーム名は、サーバーが `w.…` を導く元（画面にもログにも出さない）。
#[derive(Clone, Copy)]
pub struct Posting {
    pub room: AssignedRoom,
    pub p1: WatchName,
    pub p2: WatchName,
    pub mode: Mode,
    pub delay: u8,
    pub cap: u8,
}

impl core::fmt::Debug for Posting {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Posting(..)")
    }
}

/// `{"t":"post",…}` の文を作る。名前・ルーム名・方式は逃がしの要らない文字だけ（作る前に確かめてある）ので、そのまま書く。
pub fn write_post(p: &Posting, watchers: u8) -> String {
    format!(
        r#"{{"t":"post","room":"{}","p1":"{}","p2":"{}","mode":"{}","delay":{},"watchers":{},"cap":{}}}"#,
        p.room.name().as_str(),
        p.p1.as_str(),
        p.p2.as_str(),
        mode_wire(p.mode),
        p.delay.min(WATCH_DELAY_MAX),
        watchers.min(p.cap),
        p.cap.clamp(1, WATCH_CAP_MAX as u8),
    )
}

fn write_count(watchers: u8) -> String {
    format!(r#"{{"t":"count","watchers":{watchers}}}"#)
}

// ---------------- 共通: ping と pong の見張り ----------------

#[derive(Debug, Clone, Copy, Default)]
struct Pings {
    next_ping_ms: Option<u64>,
    unanswered_ms: Option<u64>,
}

impl Pings {
    fn start(&mut self, now: Now) {
        self.next_ping_ms = Some(now.ms.saturating_add(PING_INTERVAL_MS));
    }

    fn stop(&mut self) {
        *self = Pings::default();
    }

    fn silence_deadline(&self) -> Option<u64> {
        self.unanswered_ms
            .map(|p| p.saturating_add(PONG_TIMEOUT_MS))
    }

    /// pong が来ないまま `PONG_TIMEOUT_MS` 経ったか。
    fn silent(&self, now: Now) -> bool {
        self.silence_deadline().is_some_and(|d| now.ms >= d)
    }

    /// ping の時刻なら次の時刻を置いて ping の文を返す。
    fn due(&mut self, now: Now) -> Option<String> {
        let p = self.next_ping_ms?;
        if now.ms < p {
            return None;
        }
        self.next_ping_ms = Some(now.ms.saturating_add(PING_INTERVAL_MS));
        self.unanswered_ms.get_or_insert(now.ms);
        Some(PING_TEXT.to_owned())
    }
}

fn wake_min(a: &[Option<u64>]) -> Option<u64> {
    a.iter().copied().flatten().min()
}

fn close_failure(code: u16) -> Failure {
    match CloseCode::from_number(code) {
        Some(CloseCode::Expired) => Failure::Expired,
        Some(CloseCode::PeerLeft) => Failure::PeerLeft,
        Some(CloseCode::ListFull) => Failure::RoomFull,
        _ => Failure::Unresponsive(Why::Closed(code)),
    }
}

fn error_failure(code: Option<CloseCode>) -> Failure {
    match code {
        Some(CloseCode::Expired) => Failure::Expired,
        Some(CloseCode::PeerLeft) => Failure::PeerLeft,
        Some(CloseCode::ListFull) => Failure::RoomFull,
        other => Failure::Unresponsive(Why::ErrorMessage(other)),
    }
}

// ---------------- 載せる口の状態機械 ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    /// 一覧に載った。
    Listed,
    /// 手順が終わった（一覧から消えた）。`RoomFull` は件数の上限（全体か 1 IP あたり）。
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    /// この文字をそのままテキストの文で送る（post・count・ping）。
    Send(String),
    Notify(HostEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    Connecting,
    /// `post` を送った後、`listed` を待っている。
    AwaitListed,
    Listed,
    Failed(Failure),
}

/// 載せる口の手順。繋ぐ前に `new` → 101 なら `on_upgraded`（この中で `post` を送る）、101 でなければ `on_http_refused`、繋がらなければ
/// `on_transport_failed`。以後、受けた文を `on_text`、閉じる番号を `on_close`。観戦者の数が変わったら `set_watchers`。
/// `next_wake` の時刻か、それより前に `poll` を呼ぶ。終わりは失敗だけ（載せている間は閉じない）。
#[derive(Debug, Clone)]
pub struct HostClient {
    state: HostState,
    posting: Posting,
    /// いまの観戦者の数（`set_watchers` が置く）と、サーバーへ最後に送った（`post` か `count` の）数。
    watchers: u8,
    sent: u8,
    deadline_ms: Option<u64>,
    pings: Pings,
}

impl HostClient {
    pub fn new(posting: Posting, watchers: u8) -> HostClient {
        let watchers = watchers.min(posting.cap);
        HostClient {
            state: HostState::Connecting,
            posting,
            watchers,
            sent: watchers,
            deadline_ms: None,
            pings: Pings::default(),
        }
    }

    pub fn state(&self) -> HostState {
        self.state
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, HostState::Failed(_))
    }

    pub fn next_wake(&self) -> Option<u64> {
        wake_min(&[
            self.pings.next_ping_ms,
            self.deadline_ms,
            self.pings.silence_deadline(),
        ])
    }

    fn fail(&mut self, f: Failure) -> Vec<HostAction> {
        self.state = HostState::Failed(f);
        self.deadline_ms = None;
        self.pings.stop();
        vec![HostAction::Notify(HostEvent::Failed(f))]
    }

    pub fn on_upgraded(&mut self, now: Now) -> Vec<HostAction> {
        if self.state != HostState::Connecting {
            return Vec::new();
        }
        self.state = HostState::AwaitListed;
        self.deadline_ms = Some(now.ms.saturating_add(ROOM_GREETING_TIMEOUT_MS));
        self.pings.start(now);
        self.sent = self.watchers;
        vec![HostAction::Send(write_post(&self.posting, self.watchers))]
    }

    pub fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<HostAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(classify_http(status, body))
    }

    pub fn on_transport_failed(&mut self) -> Vec<HostAction> {
        match self.state {
            HostState::Failed(_) => Vec::new(),
            HostState::Connecting => self.fail(Failure::Unresponsive(Why::NoAnswer)),
            _ => self.fail(Failure::Unresponsive(Why::Lost)),
        }
    }

    pub fn on_close(&mut self, code: u16) -> Vec<HostAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(close_failure(code))
    }

    /// 観戦者の数が変わった。載っていれば `count` を送る（載る前なら、載った直後に送る）。
    pub fn set_watchers(&mut self, n: u8) -> Vec<HostAction> {
        self.watchers = n.min(self.posting.cap);
        self.flush_count()
    }

    fn flush_count(&mut self) -> Vec<HostAction> {
        if self.state == HostState::Listed && self.watchers != self.sent {
            self.sent = self.watchers;
            return vec![HostAction::Send(write_count(self.watchers))];
        }
        Vec::new()
    }

    pub fn on_text(&mut self, _now: Now, text: &str) -> Vec<HostAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.pings.unanswered_ms = None;
        let msg = match parse_host_message(text) {
            Ok(m) => m,
            Err(e) => return self.fail(Failure::Unresponsive(Why::BadMessage(e))),
        };
        match (self.state, msg) {
            (HostState::Connecting, _) => self.out_of_order(),
            (_, HostMsg::Pong) => Vec::new(),
            (_, HostMsg::Error { code }) => self.fail(error_failure(code)),
            (HostState::AwaitListed, HostMsg::Listed) => {
                self.state = HostState::Listed;
                self.deadline_ms = None;
                let mut out = vec![HostAction::Notify(HostEvent::Listed)];
                out.extend(self.flush_count());
                out
            }
            _ => self.out_of_order(),
        }
    }

    fn out_of_order(&mut self) -> Vec<HostAction> {
        self.fail(Failure::Unresponsive(Why::BadMessage(MsgError::BadValue(
            "t",
        ))))
    }

    pub fn poll(&mut self, now: Now) -> Vec<HostAction> {
        if self.is_finished() || self.state == HostState::Connecting {
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
            Some(p) => vec![HostAction::Send(p)],
            None => Vec::new(),
        }
    }
}

// ---------------- 見る口の状態機械 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    /// 一覧の全体（繋いだ直後と、変わるたびに届く）。
    List {
        rows: Vec<WatchRow>,
        skipped: usize,
    },
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewAction {
    Send(String),
    Notify(ViewEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Connecting,
    /// 101 の後、最初の一覧を待っている。
    AwaitList,
    Viewing,
    Failed(Failure),
}

/// 見る口の手順。使い方は [`HostClient`] と同じ（`post` も `count` も送らない。終わるのは閉じる・失敗のときだけ）。
#[derive(Debug, Clone)]
pub struct ViewClient {
    state: ViewState,
    deadline_ms: Option<u64>,
    pings: Pings,
}

impl Default for ViewClient {
    fn default() -> Self {
        ViewClient::new()
    }
}

impl ViewClient {
    pub fn new() -> ViewClient {
        ViewClient {
            state: ViewState::Connecting,
            deadline_ms: None,
            pings: Pings::default(),
        }
    }

    pub fn state(&self) -> ViewState {
        self.state
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, ViewState::Failed(_))
    }

    pub fn next_wake(&self) -> Option<u64> {
        wake_min(&[
            self.pings.next_ping_ms,
            self.deadline_ms,
            self.pings.silence_deadline(),
        ])
    }

    fn fail(&mut self, f: Failure) -> Vec<ViewAction> {
        self.state = ViewState::Failed(f);
        self.deadline_ms = None;
        self.pings.stop();
        vec![ViewAction::Notify(ViewEvent::Failed(f))]
    }

    pub fn on_upgraded(&mut self, now: Now) -> Vec<ViewAction> {
        if self.state != ViewState::Connecting {
            return Vec::new();
        }
        self.state = ViewState::AwaitList;
        self.deadline_ms = Some(now.ms.saturating_add(ROOM_GREETING_TIMEOUT_MS));
        self.pings.start(now);
        Vec::new()
    }

    pub fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<ViewAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(classify_http(status, body))
    }

    pub fn on_transport_failed(&mut self) -> Vec<ViewAction> {
        match self.state {
            ViewState::Failed(_) => Vec::new(),
            ViewState::Connecting => self.fail(Failure::Unresponsive(Why::NoAnswer)),
            _ => self.fail(Failure::Unresponsive(Why::Lost)),
        }
    }

    pub fn on_close(&mut self, code: u16) -> Vec<ViewAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(close_failure(code))
    }

    pub fn on_text(&mut self, _now: Now, text: &str) -> Vec<ViewAction> {
        if self.is_finished() {
            return Vec::new();
        }
        self.pings.unanswered_ms = None;
        let msg = match parse_view_message(text) {
            Ok(m) => m,
            Err(e) => return self.fail(Failure::Unresponsive(Why::BadMessage(e))),
        };
        match (self.state, msg) {
            (ViewState::Connecting, _) => self.fail(Failure::Unresponsive(Why::BadMessage(
                MsgError::BadValue("t"),
            ))),
            (_, ViewMsg::Pong) => Vec::new(),
            (_, ViewMsg::Error { code }) => self.fail(error_failure(code)),
            (_, ViewMsg::List { rows, skipped }) => {
                self.state = ViewState::Viewing;
                self.deadline_ms = None;
                vec![ViewAction::Notify(ViewEvent::List { rows, skipped })]
            }
        }
    }

    pub fn poll(&mut self, now: Now) -> Vec<ViewAction> {
        if self.is_finished() || self.state == ViewState::Connecting {
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
            Some(p) => vec![ViewAction::Send(p)],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
