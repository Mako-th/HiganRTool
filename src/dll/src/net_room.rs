//! ルームサーバーのクライアントの運び: 受信だけの専用スレッド `higanr-room` と、見張りのスレッドから呼ぶ入口（ルームの [`RoomConn`]・
//! ランダム対戦の待ち列の [`QueueConn`]。どちらも [`Conn`] で、回す手順の機械だけが違う）。
//!
//! - **受信スレッド**（`higanr-room`）: 繋ぐ → Upgrade → 受信を繰り返し、届いたものを channel で積むだけ。止まってよいのは
//!   このスレッドだけ（名前の解決・TCP・TLS・HTTP の往復・届くまでの受信）。
//! - **見張りのスレッド**（呼び手。`net_code_game` の `GameWatcher`）: [`RoomConn::poll`] で channel を取り出し、core の
//!   `room::RoomClient` を回し、送る文（offer / answer / 30 秒ごとの ping）を送り、出来事を返す。止まらない（送りは
//!   小さな文を TCP の送信側へ渡すだけ。閉じる途中なら送らずに戻る）。
//! - **閉じるスレッド**（`higanr-room-close`。終わりのときに 1 本だけ、短く）: `WinHttpWebSocketClose` を呼ぶ。止まっている
//!   受信はそれでその場で戻る（12017）。`close` 自身は相手が閉じ返さなければ閉じる待ち（[`CLOSE_WAIT_MS`]）まで止まるので
//!   見張りから外した。別のスレッドからハンドルを閉じて受信を解く形は採らない（`room_win.rs`）。
//! - ゲームのスレッドからは何も呼ばない（確保・ブロッキング I/O がある）。遅延の増分はマッチの中で 0（P2P が繋がる前に
//!   WebSocket はサーバーが閉じている）。
//!
//! 手順の判断（何をいつ送るか・届いた文の意味・期限・ping・pong が来ないこと）は全部 core の `room.rs`（待ち列は `room_queue.rs`）。ここは
//! 「届いたものを順に渡す」「返ってきた送る文を送る」「終わったら閉じる」だけを持つ。core の `room::Role` は [`code_side`] で
//! `net_config::CodeSide` へコピーする（host / guest の語は使わない）。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use higanr_core::room::{
    Action, CLIENT_ID_BYTES, Event, MESSAGE_MAX_BYTES, Now, Role, RoomClient, RoomName, State,
    SubmitError,
};
use higanr_core::room_queue::{QueueAction, QueueClient, QueueEvent, random_path};
use higanr_core::watch_list::{
    HostAction, HostClient, HostEvent, Posting, ViewAction, ViewClient, ViewEvent,
    WATCH_LIST_MAX_BYTES, watch_post_path, watch_view_path,
};

use crate::net_config::CodeSide;
use crate::net_win::os_random;
use crate::room_win::{Endpoint, OpenError, Proxy, Received, Step, Timeouts, WebSocket};

/// Upgrade までの各段階（名前の解決・TCP の接続・送り・101 の先頭）の待ちの上限。core の `ROOM_GREETING_TIMEOUT_MS` と同じ
/// 10 秒（人がメニューの前で待てる長さ）。
pub const OPEN_TIMEOUT_MS: u32 = 10_000;

// Upgrade の後の受信には上限を置かない（置けない）: `WINHTTP_OPTION_RECEIVE_TIMEOUT` は WebSocket の受信に効かない
// （loopback で測った。検査 `winhttp_facts_receive_timeout_*`）。生きているかは core の ping / pong（`room::PONG_TIMEOUT_MS`）が見て、
// 終わりは閉じるスレッドの `close` が受信を解く。

/// 閉じるとき、相手の閉じ返しを待つ上限（`WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT`。単位は ms と測った）。
/// 閉じるスレッドだけがこの間止まる。閉じ返しはサーバーの中の往復だけなので 2 秒で足りる。
pub const CLOSE_WAIT_MS: u32 = 2_000;

/// こちらから閉じるときの番号（RFC 6455 の 1000 = 正常に終えた。取りやめ・失敗でも同じ）。
const CLOSE_NORMAL: u16 = 1000;

pub fn product_timeouts() -> Timeouts {
    Timeouts {
        resolve_ms: OPEN_TIMEOUT_MS,
        connect_ms: OPEN_TIMEOUT_MS,
        send_ms: OPEN_TIMEOUT_MS,
        response_ms: OPEN_TIMEOUT_MS,
        close_wait_ms: CLOSE_WAIT_MS,
    }
}

/// この起動の乱数（入室の要求の問い `cid`）。起動ごとに 1 回だけ引く: 入り直しが同じ値を送ることで、サーバーが前の入室
/// （こちらには届かなかったもの）を 3 人目扱いにせず置き換える。引けなければ付けない（サーバーは古いクライアントと同じに扱う）。
fn client_id() -> Option<[u8; CLIENT_ID_BYTES]> {
    static ID: OnceLock<Option<[u8; CLIENT_ID_BYTES]>> = OnceLock::new();
    *ID.get_or_init(|| {
        let mut b = [0u8; CLIENT_ID_BYTES];
        os_random(&mut b).then_some(b)
    })
}

pub fn code_side(role: Role) -> CodeSide {
    match role {
        Role::Offer => CodeSide::Offer,
        Role::Answer => CodeSide::Answer,
    }
}

// ---------------- サーバーの宿（設定の値） ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub secure: bool,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseError {
    /// `ws://` か `wss://` で始まらない。
    Scheme,
    /// 宿の名が空・使えない文字（英数字・`-`・`.` の外。IPv6 の `[...]` も読まない）。
    Host,
    /// 番号が数でない・0・65535 を越える。
    Port,
    /// パス・問い・断片が付いている（パスはルーム名から作る）。末尾の `/` だけは許す。
    Path,
}

impl Base {
    /// `ws://宿[:番号][/]` か `wss://宿[:番号][/]`。番号が無ければ 80 / 443。スキームの大文字小文字は問わない。
    pub fn parse(s: &str) -> Result<Base, BaseError> {
        let lower = s.get(..6).map(str::to_ascii_lowercase);
        let (secure, rest) = if lower.as_deref() == Some("wss://") {
            (true, s.get(6..).unwrap_or(""))
        } else if lower.as_deref().and_then(|l| l.get(..5)) == Some("ws://") {
            (false, s.get(5..).unwrap_or(""))
        } else {
            return Err(BaseError::Scheme);
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.contains(['/', '?', '#']) {
            return Err(BaseError::Path);
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(BaseError::Port);
                }
                let n: u16 = p.parse().map_err(|_| BaseError::Port)?;
                if n == 0 {
                    return Err(BaseError::Port);
                }
                (h, n)
            }
            None => (rest, if secure { 443 } else { 80 }),
        };
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            return Err(BaseError::Host);
        }
        Ok(Base {
            secure,
            host: host.to_owned(),
            port,
        })
    }

    fn endpoint(&self, path: String) -> Endpoint {
        Endpoint {
            secure: self.secure,
            host: self.host.clone(),
            port: self.port,
            path,
        }
    }
}

// ---------------- 受信スレッドと見張りの受け渡し ----------------

#[derive(Debug)]
enum Incoming {
    /// 101 を受けた（WebSocket は `Shared::ws` に置いた）。
    Upgraded,
    /// 101 でない応答。
    Refused {
        status: u16,
        body: String,
    },
    /// 繋げなかった（WinHTTP の段階と番号）。
    OpenFailed {
        step: Step,
        code: u32,
    },
    Text(String),
    /// バイナリの文（サーバーは送らない）。
    Binary,
    /// 相手の close（番号。無ければ 1005）。
    Closed(u16),
    /// 受信の誤り（WinHTTP の番号）。
    RecvError(u32),
}

struct Shared {
    ws: Mutex<Option<Arc<WebSocket>>>,
    /// 送りの向き（`send_text` / `close`）を 1 本に絞る錠。WinHTTP の WebSocket は送り 1 本と受け 1 本までなので、見張りの送りと
    /// 閉じるスレッド・受信スレッドの `close` を重ねない。見張りは `try_lock` で取り、取れなければ（閉じる途中）送らない。
    send_lock: Mutex<()>,
    /// `close` を 1 回だけ呼ぶための印（受信スレッドが相手の close に答えるのと、閉じるスレッドが閉じるのとが重なるため）。
    closing: AtomicBool,
    /// 見張りが終えた（取りやめ・手順の終わり）。Upgrade の前なら、受信スレッドは繋がった直後に自分で閉じる。
    ended: AtomicBool,
}

impl Shared {
    /// `close` を 1 回だけ呼ぶ。待ちうる（[`CLOSE_WAIT_MS`]）ので受信スレッドか閉じるスレッドからだけ呼ぶ。
    fn close_once(&self, ws: &WebSocket) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _g = self.send_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ = ws.close(CLOSE_NORMAL);
    }
}

/// close の待ちの上限を置けなかったこと（Wine など）を、接続ごとでなく起動ごとに 1 回だけ本ログへ出す。
fn log_close_timeout_once(code: u32) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::AcqRel) {
        crate::main_log::push(format!(
            "ルームの運び: WebSocket の close の待ちの上限を置けなかった（{code}）。既定の長さで続ける"
        ));
    }
}

fn recv_main(
    ep: Endpoint,
    proxy: Proxy,
    t: Timeouts,
    shared: Arc<Shared>,
    tx: Sender<Incoming>,
    max: usize,
) {
    // 101 でない本文は 1 通の上限 + 1 バイトまで読む（越えたことを core が「サーバーの JSON でない」と読めるように）。
    let ws = match WebSocket::open(&ep, proxy, t, MESSAGE_MAX_BYTES + 1) {
        Ok(ws) => Arc::new(ws),
        Err(OpenError::Refused { status, body }) => {
            let _ = tx.send(Incoming::Refused { status, body });
            return;
        }
        Err(OpenError::Transport { step, code }) => {
            let _ = tx.send(Incoming::OpenFailed { step, code });
            return;
        }
    };
    if let Some(code) = ws.close_timeout_error() {
        log_close_timeout_once(code);
    }
    *shared.ws.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&ws));
    // 見張りが先に終えていた（繋ぐ途中の取りやめ）か、もう見張りがいない: 閉じて終わる。
    if shared.ended.load(Ordering::Acquire) || tx.send(Incoming::Upgraded).is_err() {
        shared.close_once(&ws);
        return;
    }
    loop {
        match ws.receive(max) {
            Ok(Received::Text(text)) => {
                if tx.send(Incoming::Text(text)).is_err() {
                    shared.close_once(&ws);
                    return;
                }
            }
            Ok(Received::Binary) => {
                let _ = tx.send(Incoming::Binary);
            }
            Ok(Received::Close) => {
                let _ = tx.send(Incoming::Closed(ws.close_status()));
                // 相手の close に閉じ返す（こちらが先に閉じていれば何もしない）。
                shared.close_once(&ws);
                return;
            }
            Err(code) => {
                let _ = tx.send(Incoming::RecvError(code));
                return;
            }
        }
    }
}

// ---------------- 見張りの入口 ----------------

#[derive(Debug, Clone)]
pub struct RoomOptions {
    pub base: Base,
    pub proxy: Proxy,
    pub timeouts: Timeouts,
}

impl RoomOptions {
    pub fn product(base: Base) -> RoomOptions {
        RoomOptions {
            base,
            proxy: Proxy::Automatic,
            timeouts: product_timeouts(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitRefused {
    /// core が断った（番でない・文字が悪い）。状態は変わらない。
    Core(SubmitError),
    /// 送れなかった（繋がりが切れた・閉じる途中）。失敗の出来事は次の `poll` で返る。
    NotSent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportNote {
    OpenFailed { step: Step, code: u32 },
    RecvError(u32),
    SendError(u32),
    Binary,
}

/// 運びが手順の機械（core の `RoomClient`・`QueueClient`）を回すための入口。機械が返す送る文と出来事を [`Act`] に揃える。
pub trait Machine {
    type Event;
    fn on_upgraded(&mut self, now: Now) -> Vec<Act<Self::Event>>;
    fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<Act<Self::Event>>;
    fn on_transport_failed(&mut self) -> Vec<Act<Self::Event>>;
    fn on_close(&mut self, code: u16) -> Vec<Act<Self::Event>>;
    fn on_text(&mut self, now: Now, text: &str) -> Vec<Act<Self::Event>>;
    fn poll(&mut self, now: Now) -> Vec<Act<Self::Event>>;
    fn is_finished(&self) -> bool;
    fn next_wake(&self) -> Option<u64>;
    /// 受けられる 1 通の大きさの上限（バイト）。一覧の全体の文は、ルームの文より大きい。
    fn message_max(&self) -> usize {
        MESSAGE_MAX_BYTES
    }
}

/// 機械が返す 1 つの仕事: 文を送る・出来事を上へ渡す。
pub enum Act<E> {
    Send(String),
    Notify(E),
}

fn acts_of_room(v: Vec<Action>) -> Vec<Act<Event>> {
    v.into_iter()
        .map(|a| match a {
            Action::Send(t) => Act::Send(t),
            Action::Notify(e) => Act::Notify(e),
        })
        .collect()
}

fn acts_of_queue(v: Vec<QueueAction>) -> Vec<Act<QueueEvent>> {
    v.into_iter()
        .map(|a| match a {
            QueueAction::Send(t) => Act::Send(t),
            QueueAction::Notify(e) => Act::Notify(e),
        })
        .collect()
}

impl Machine for RoomClient {
    type Event = Event;
    fn on_upgraded(&mut self, now: Now) -> Vec<Act<Event>> {
        acts_of_room(RoomClient::on_upgraded(self, now))
    }
    fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<Act<Event>> {
        acts_of_room(RoomClient::on_http_refused(self, status, body))
    }
    fn on_transport_failed(&mut self) -> Vec<Act<Event>> {
        acts_of_room(RoomClient::on_transport_failed(self))
    }
    fn on_close(&mut self, code: u16) -> Vec<Act<Event>> {
        acts_of_room(RoomClient::on_close(self, code))
    }
    fn on_text(&mut self, now: Now, text: &str) -> Vec<Act<Event>> {
        acts_of_room(RoomClient::on_text(self, now, text))
    }
    fn poll(&mut self, now: Now) -> Vec<Act<Event>> {
        acts_of_room(RoomClient::poll(self, now))
    }
    fn is_finished(&self) -> bool {
        RoomClient::is_finished(self)
    }
    fn next_wake(&self) -> Option<u64> {
        RoomClient::next_wake(self)
    }
}

impl Machine for QueueClient {
    type Event = QueueEvent;
    fn on_upgraded(&mut self, now: Now) -> Vec<Act<QueueEvent>> {
        acts_of_queue(QueueClient::on_upgraded(self, now))
    }
    fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<Act<QueueEvent>> {
        acts_of_queue(QueueClient::on_http_refused(self, status, body))
    }
    fn on_transport_failed(&mut self) -> Vec<Act<QueueEvent>> {
        acts_of_queue(QueueClient::on_transport_failed(self))
    }
    fn on_close(&mut self, code: u16) -> Vec<Act<QueueEvent>> {
        acts_of_queue(QueueClient::on_close(self, code))
    }
    fn on_text(&mut self, now: Now, text: &str) -> Vec<Act<QueueEvent>> {
        acts_of_queue(QueueClient::on_text(self, now, text))
    }
    fn poll(&mut self, now: Now) -> Vec<Act<QueueEvent>> {
        acts_of_queue(QueueClient::poll(self, now))
    }
    fn is_finished(&self) -> bool {
        QueueClient::is_finished(self)
    }
    fn next_wake(&self) -> Option<u64> {
        QueueClient::next_wake(self)
    }
}

fn acts_of_host(v: Vec<HostAction>) -> Vec<Act<HostEvent>> {
    v.into_iter()
        .map(|a| match a {
            HostAction::Send(t) => Act::Send(t),
            HostAction::Notify(e) => Act::Notify(e),
        })
        .collect()
}

fn acts_of_view(v: Vec<ViewAction>) -> Vec<Act<ViewEvent>> {
    v.into_iter()
        .map(|a| match a {
            ViewAction::Send(t) => Act::Send(t),
            ViewAction::Notify(e) => Act::Notify(e),
        })
        .collect()
}

impl Machine for HostClient {
    type Event = HostEvent;
    fn on_upgraded(&mut self, now: Now) -> Vec<Act<HostEvent>> {
        acts_of_host(HostClient::on_upgraded(self, now))
    }
    fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<Act<HostEvent>> {
        acts_of_host(HostClient::on_http_refused(self, status, body))
    }
    fn on_transport_failed(&mut self) -> Vec<Act<HostEvent>> {
        acts_of_host(HostClient::on_transport_failed(self))
    }
    fn on_close(&mut self, code: u16) -> Vec<Act<HostEvent>> {
        acts_of_host(HostClient::on_close(self, code))
    }
    fn on_text(&mut self, now: Now, text: &str) -> Vec<Act<HostEvent>> {
        acts_of_host(HostClient::on_text(self, now, text))
    }
    fn poll(&mut self, now: Now) -> Vec<Act<HostEvent>> {
        acts_of_host(HostClient::poll(self, now))
    }
    fn is_finished(&self) -> bool {
        HostClient::is_finished(self)
    }
    fn next_wake(&self) -> Option<u64> {
        HostClient::next_wake(self)
    }
}

impl Machine for ViewClient {
    type Event = ViewEvent;
    fn on_upgraded(&mut self, now: Now) -> Vec<Act<ViewEvent>> {
        acts_of_view(ViewClient::on_upgraded(self, now))
    }
    fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<Act<ViewEvent>> {
        acts_of_view(ViewClient::on_http_refused(self, status, body))
    }
    fn on_transport_failed(&mut self) -> Vec<Act<ViewEvent>> {
        acts_of_view(ViewClient::on_transport_failed(self))
    }
    fn on_close(&mut self, code: u16) -> Vec<Act<ViewEvent>> {
        acts_of_view(ViewClient::on_close(self, code))
    }
    fn on_text(&mut self, now: Now, text: &str) -> Vec<Act<ViewEvent>> {
        acts_of_view(ViewClient::on_text(self, now, text))
    }
    fn poll(&mut self, now: Now) -> Vec<Act<ViewEvent>> {
        acts_of_view(ViewClient::poll(self, now))
    }
    fn is_finished(&self) -> bool {
        ViewClient::is_finished(self)
    }
    fn next_wake(&self) -> Option<u64> {
        ViewClient::next_wake(self)
    }
    fn message_max(&self) -> usize {
        WATCH_LIST_MAX_BYTES
    }
}

/// ルームサーバーとの 1 回の手順（繋ぐ → ルームに入る → コードを渡し合う → 閉じる）。見張りのスレッドが持ち、
/// [`Conn::poll`] を [`Conn::next_poll_in`] より短い間隔で呼ぶ。手放す（`Drop`）と閉じ始める（待たない）。
/// 手順の機械 `M` がルーム（[`RoomConn`]）とランダム対戦の待ち列（[`QueueConn`]）で違うだけで、運びは同じ。
pub struct Conn<M: Machine> {
    client: M,
    rx: Receiver<Incoming>,
    shared: Arc<Shared>,
    ws: Option<Arc<WebSocket>>,
    recv: Option<JoinHandle<()>>,
    closer: Option<JoinHandle<()>>,
    t0: Instant,
    cancelled: bool,
    notes: Vec<TransportNote>,
    /// `submit_code` の中で起きた出来事（送れなかった失敗）。次の `poll` の始まりで返す。
    pending: Vec<M::Event>,
}

/// ルームの手順。
pub type RoomConn = Conn<RoomClient>;
/// ランダム対戦の待ち列の手順。
pub type QueueConn = Conn<QueueClient>;
/// 進行中のランダム対戦の一覧に載せる手順（対戦のホストが持ち続ける）。
pub type HostListConn = Conn<HostClient>;
/// 進行中のランダム対戦の一覧を見る手順（観戦者が一覧の画面を開いている間だけ持つ）。
pub type ViewListConn = Conn<ViewClient>;

impl Conn<RoomClient> {
    /// 受信スレッドを起こして繋ぎ始める（待たない）。スレッドを起こせなければ誤りの文。
    pub fn start(opts: &RoomOptions, room: &RoomName) -> Result<RoomConn, String> {
        let path = match client_id() {
            Some(id) => room.path_with_client(&id),
            None => room.path(),
        };
        Conn::start_at(opts, path, RoomClient::new())
    }

    /// 自分のコードを渡す（始める側は `Joined` の後、返す側は `PeerCode` の後）。`Ok` は「文を WebSocket へ渡し終えた」。
    /// 返す側はこの `Ok` の時点で P2P を始めてよい（サーバーの 1000 を待たない）。
    pub fn submit_code(&mut self, code: &str) -> Result<(), SubmitRefused> {
        if self.cancelled {
            return Err(SubmitRefused::NotSent);
        }
        let acts = acts_of_room(self.client.submit_code(code).map_err(SubmitRefused::Core)?);
        let mut out = Vec::new();
        let sent = self.run(acts, &mut out);
        // 送れなかったときに `run` が積んだ失敗の出来事は、core が終わった後の `poll` からは出てこないので、持っておいて
        // 次の `poll` の始まりで返す（閉じる途中なら、番号の方が次の `poll` で失敗を返す）。
        self.pending.extend(out);
        if self.client.is_finished() {
            self.begin_close();
        }
        if sent {
            Ok(())
        } else {
            Err(SubmitRefused::NotSent)
        }
    }

    pub fn state(&self) -> State {
        self.client.state()
    }
}

impl Conn<QueueClient> {
    /// ランダム対戦の待ち列へ繋ぎ始める（待たない）。`build` は `update::BUILD_NUMBER`（同じビルドの人どうしだけが組になる）。
    /// 組になると [`QueueEvent::Paired`] で割り当てのルーム名が届き、手順はそこで終わる（ルームへは [`RoomConn::start`] で入り直す）。
    pub fn start_queue(opts: &RoomOptions, build: u32) -> Result<QueueConn, String> {
        let id = client_id();
        let path = random_path(build, id.as_ref());
        Conn::start_at(opts, path, QueueClient::new())
    }
}

impl Conn<HostClient> {
    /// 一覧の載せるエンドポイントへ繋ぎ始める（待たない）。繋がったら `posting` を送り、`HostEvent::Listed` で載ったと分かる。
    pub fn start_post(
        opts: &RoomOptions,
        build: u32,
        posting: Posting,
        watchers: u8,
    ) -> Result<HostListConn, String> {
        Conn::start_at(
            opts,
            watch_post_path(build),
            HostClient::new(posting, watchers),
        )
    }

    /// 観戦者の数が変わった。載っていれば `count` を送る（載る前なら、載った直後に送る）。送れなかった失敗は次の `poll` で返る。
    pub fn set_watchers(&mut self, n: u8) {
        if self.cancelled {
            return;
        }
        let acts = acts_of_host(self.client.set_watchers(n));
        let mut out = Vec::new();
        self.run(acts, &mut out);
        self.pending.extend(out);
    }
}

impl Conn<ViewClient> {
    /// 一覧の見るエンドポイントへ繋ぎ始める（待たない）。一覧は `ViewEvent::List` で全体が届く。
    pub fn start_view(opts: &RoomOptions, build: u32) -> Result<ViewListConn, String> {
        Conn::start_at(opts, watch_view_path(build), ViewClient::new())
    }
}

impl<M: Machine> Conn<M> {
    fn start_at(opts: &RoomOptions, path: String, client: M) -> Result<Conn<M>, String> {
        let max = client.message_max();
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            ws: Mutex::new(None),
            send_lock: Mutex::new(()),
            closing: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        let ep = opts.base.endpoint(path);
        let (proxy, t) = (opts.proxy, opts.timeouts);
        let sh = Arc::clone(&shared);
        let recv = std::thread::Builder::new()
            .name("higanr-room".into())
            .spawn(move || {
                // パニックは受けて外へ出さない。受けたら channel が切れ、見張りは「切れた」と読む。
                let _ = catch_unwind(AssertUnwindSafe(|| recv_main(ep, proxy, t, sh, tx, max)));
            })
            .map_err(|e| format!("ルームの受信スレッドを起こせない: {e}"))?;
        Ok(Conn {
            client,
            rx,
            shared,
            ws: None,
            recv: Some(recv),
            closer: None,
            t0: Instant::now(),
            cancelled: false,
            notes: Vec::new(),
            pending: Vec::new(),
        })
    }

    pub fn now(&self) -> Now {
        Now {
            ms: u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX),
            unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    /// 届いたものを渡し、時計を進め、送る文を送る。起きた出来事を順に返す。止まらない。
    pub fn poll(&mut self) -> Vec<M::Event> {
        let now = self.now();
        self.poll_at(now)
    }

    pub fn poll_at(&mut self, now: Now) -> Vec<M::Event> {
        if self.cancelled {
            return Vec::new();
        }
        let mut out = std::mem::take(&mut self.pending);
        loop {
            let m = match self.rx.try_recv() {
                Ok(m) => m,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // 受信スレッドが終わった（閉じる番号・受信の誤りはもう渡した）か、パニックで落ちた。手順の途中なら
                    // 「切れた」。
                    let acts = self.client.on_transport_failed();
                    self.run(acts, &mut out);
                    break;
                }
            };
            let acts = match m {
                Incoming::Upgraded => {
                    self.ws = self
                        .shared
                        .ws
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone();
                    self.client.on_upgraded(now)
                }
                Incoming::Refused { status, body } => self.client.on_http_refused(status, &body),
                Incoming::OpenFailed { step, code } => {
                    self.notes.push(TransportNote::OpenFailed { step, code });
                    self.client.on_transport_failed()
                }
                Incoming::Text(text) => self.client.on_text(now, &text),
                Incoming::Binary => {
                    // サーバーはバイナリを送らない。来たら途中か相手が壊れているので、切れたと同じ扱い。
                    self.notes.push(TransportNote::Binary);
                    self.client.on_transport_failed()
                }
                // 番号が無ければ 1005 のまま渡す（core は知らない番号として「応答しません」に）。
                Incoming::Closed(code) => self.client.on_close(code),
                Incoming::RecvError(code) => {
                    self.notes.push(TransportNote::RecvError(code));
                    self.client.on_transport_failed()
                }
            };
            self.run(acts, &mut out);
        }
        let acts = self.client.poll(now);
        self.run(acts, &mut out);
        if self.client.is_finished() {
            self.begin_close();
        }
        out
    }

    /// 取りやめる（メニューから・ルーム名を変える）。この後 `poll` は何も返さない。閉じ始めて戻る（待たない）。
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.begin_close();
    }

    pub fn is_finished(&self) -> bool {
        self.cancelled || self.client.is_finished()
    }

    /// 次に `poll` を呼ぶべきまでの長さ（ping と期限。届いた文は時刻に関わらずすぐ渡したいので、見張りはこれより
    /// 短い間隔でも呼んでよい）。
    pub fn next_poll_in(&self) -> Option<Duration> {
        let now = self.now().ms;
        self.client
            .next_wake()
            .map(|w| Duration::from_millis(w.saturating_sub(now)))
    }

    pub fn take_notes(&mut self) -> Vec<TransportNote> {
        std::mem::take(&mut self.notes)
    }

    /// 受信スレッド（と閉じるスレッド）が終わるのを `limit` まで待つ。終わったら true。見張りのスレッドでは呼ばない
    /// （検査と、DLL を閉じるときの片付け用）。
    pub fn join_within(&mut self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        for slot in [&mut self.recv, &mut self.closer] {
            if let Some(h) = slot.take() {
                while !h.is_finished() {
                    if Instant::now() >= until {
                        *slot = Some(h);
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = h.join();
            }
        }
        true
    }

    fn run(&mut self, acts: Vec<Act<M::Event>>, out: &mut Vec<M::Event>) -> bool {
        let mut all_sent = true;
        for a in acts {
            match a {
                Act::Notify(e) => out.push(e),
                Act::Send(text) => match self.send(&text) {
                    Sent::Yes => {}
                    // 相手が先に閉じた。閉じる番号は受信スレッドが channel へ先に積んである（`recv_main` は積んでから閉じ返す）
                    // ので、ここで「切れた」にせず、次の `poll` で番号の方（期限・相手が抜けた など）で終える。
                    Sent::Closing => all_sent = false,
                    Sent::Failed => {
                        all_sent = false;
                        let more = self.client.on_transport_failed();
                        for m in more {
                            if let Act::Notify(e) = m {
                                out.push(e);
                            }
                        }
                    }
                },
            }
        }
        all_sent
    }

    fn send(&mut self, text: &str) -> Sent {
        let Some(ws) = self.ws.as_ref() else {
            return Sent::Failed;
        };
        // 閉じる途中（受信スレッドが相手の close に閉じ返している）なら送らない。待たない。
        let _g = match self.shared.send_lock.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Sent::Closing,
        };
        if self.shared.closing.load(Ordering::Acquire) {
            return Sent::Closing;
        }
        match ws.send_text(text) {
            Ok(()) => Sent::Yes,
            Err(code) => {
                self.notes.push(TransportNote::SendError(code));
                Sent::Failed
            }
        }
    }

    /// 閉じ始める（1 回だけ）。Upgrade の前なら受信スレッドが繋がった直後に閉じる。後なら閉じるスレッドを 1 本起こす。
    fn begin_close(&mut self) {
        if self.shared.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let ws = self.ws.clone().or_else(|| {
            self.shared
                .ws
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        });
        let Some(ws) = ws else {
            return;
        };
        if self.shared.closing.load(Ordering::Acquire) {
            return;
        }
        let sh = Arc::clone(&self.shared);
        let w = Arc::clone(&ws);
        self.closer = std::thread::Builder::new()
            .name("higanr-room-close".into())
            .spawn(move || {
                // パニックは受けて外へ出さない。
                let _ = catch_unwind(AssertUnwindSafe(|| sh.close_once(&w)));
            })
            .ok();
        if self.closer.is_none()
            && let Ok(_g) = self.shared.send_lock.try_lock()
            && !self.shared.closing.swap(true, Ordering::AcqRel)
        {
            // スレッドを起こせない: 待たない `shutdown` だけ送る（サーバーが閉じ返せば受信スレッドはそれで終わる）。
            let _ = ws.shutdown(CLOSE_NORMAL);
        }
    }
}

enum Sent {
    Yes,
    /// 閉じる途中で送らなかった（相手の close の番号が channel にある）。
    Closing,
    /// 送れなかった（繋がっていない・WinHTTP の誤り）。
    Failed,
}

impl<M: Machine> Drop for Conn<M> {
    fn drop(&mut self) {
        self.begin_close();
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! loopback の偽のルームサーバー。`src/server/src/index.ts`（入口の断り）と `room.ts`（2 人の役割・offer / peer / matched /
    //! answer・閉じる番号と直前の error の文・期限・片方が抜けたとき）の振る舞いのコピー。値は全部 `tests/fixtures/room/rules.json`
    //! から読み、自前の定数を持たない。`127.0.0.1:0` で受け、外へは繋がない。
    //!
    //! コピーしていないもの: 同じ cid の入り直しの置き換え（検査の一部は 1 つのプロセスから 2 人分を繋ぐ。問いは受けて `seen` に残すだけ）・
    //! Hibernation・Durable Object の記憶・IP ごとの数え（1 本にまとめ、通す数は検査が `limit` で置く）。
    //! クライアントの文は JSON.parse でなく、こちらのクライアントが送る 3 つの形（offer・answer・ping の 12 文字）の文字の一致で
    //! 読む（ほかは room.ts と同じく 4400）。WebSocket の握手の SHA-1 は OS の BCrypt（`room_win::sha1`）。枠組みとマスク解きは
    //! RFC 6455 §5 を読んで手で書いた。
    //!
    //! 役割の名は room.ts の文字（`"host"` / `"guest"`）を文にだけ書き、Rust の名前は `Side::Offer` / `Side::Answer`。

    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const RULES_JSON: &str = include_str!("../../../tests/fixtures/room/rules.json");

    /// WebSocket の握手の決まった文字（RFC 6455 §1.3）。
    const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub struct Rules {
        pub path_prefix: String,
        pub room_name_min: usize,
        pub room_name_max: usize,
        pub room_name_max_bytes: usize,
        pub room_name_controls: Vec<(u32, u32)>,
        pub room_name_bad_chars: String,
        pub room_name_bad_names: Vec<String>,
        pub code_min_chars: usize,
        pub code_max_chars: usize,
        pub code_chars: String,
        pub message_max_bytes: usize,
        pub room_ttl_ms: u64,
        pub answer_ttl_ms: u64,
        pub ping_text: String,
        pub pong_text: String,
        pub retry_after_secs: u64,
        pub closes: Vec<(String, u16)>,
        pub random_path_prefix: String,
        pub random_room_prefix: String,
        pub random_room_hex_len: usize,
        pub random_queue_ttl_ms: u64,
        pub random_room_ttl_ms: u64,
        pub watch_path_prefix: String,
        pub watch_post_segment: String,
        pub watch_view_segment: String,
        pub watch_max_total: usize,
        pub watch_cap_max: i64,
        pub watch_delay_max: i64,
        pub watch_modes: Vec<String>,
    }

    enum Val {
        S(String),
        I(i64),
    }

    fn skip_ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    }

    fn read_str(b: &[u8], i: &mut usize) -> String {
        assert_eq!(b[*i], b'"', "見本の {i} に文字列が無い");
        *i += 1;
        let mut v = Vec::new();
        loop {
            match b[*i] {
                b'"' => {
                    *i += 1;
                    break;
                }
                b'\\' => {
                    v.push(match b[*i + 1] {
                        b'"' => b'"',
                        b'\\' => b'\\',
                        b'/' => b'/',
                        b'n' => b'\n',
                        other => panic!("見本に読まない逃がし {:?}", other as char),
                    });
                    *i += 2;
                }
                c => {
                    v.push(c);
                    *i += 1;
                }
            }
        }
        String::from_utf8(v).expect("見本は UTF-8")
    }

    /// `%XX` を戻す（`decodeURIComponent` のコピー）。`%` の後が 16 進 2 桁でない・戻したものが UTF-8 でない（代理・長すぎる
    /// 符号を含む）なら None。
    fn percent_decode(s: &str) -> Option<String> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                let h = b.get(i + 1..i + 3)?;
                let h = std::str::from_utf8(h).ok()?;
                if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()
    }

    /// 平らな JSON（文字列と整数だけ）の小さな読み手。見本の形だけを読む（入れ子・小数・真偽が来たら落ちる）。
    fn read_flat(s: &str) -> Vec<(String, Val)> {
        let b = s.as_bytes();
        let mut i = 0;
        let mut out = Vec::new();
        skip_ws(b, &mut i);
        assert_eq!(b[i], b'{');
        i += 1;
        loop {
            skip_ws(b, &mut i);
            let k = read_str(b, &mut i);
            skip_ws(b, &mut i);
            assert_eq!(b[i], b':');
            i += 1;
            skip_ws(b, &mut i);
            let v = if b[i] == b'"' {
                Val::S(read_str(b, &mut i))
            } else {
                let st = i;
                while i < b.len() && (b[i] == b'-' || b[i].is_ascii_digit()) {
                    i += 1;
                }
                Val::I(s[st..i].parse().expect("見本の数は整数"))
            };
            out.push((k, v));
            skip_ws(b, &mut i);
            match b[i] {
                b',' => i += 1,
                b'}' => break,
                c => panic!("見本の {i} に {:?}", c as char),
            }
        }
        out
    }

    impl Rules {
        pub fn load() -> Rules {
            let list = read_flat(RULES_JSON);
            let s = |k: &str| match list.iter().find(|(n, _)| n == k) {
                Some((_, Val::S(v))) => v.clone(),
                _ => panic!("見本に文字列の {k} が無い"),
            };
            let n = |k: &str| match list.iter().find(|(n, _)| n == k) {
                Some((_, Val::I(v))) => *v,
                _ => panic!("見本に整数の {k} が無い"),
            };
            let closes = list
                .iter()
                .filter_map(|(k, v)| match (k.strip_prefix("close_"), v) {
                    (Some(name), Val::I(num)) => Some((name.to_owned(), *num as u16)),
                    _ => None,
                })
                .collect();
            // 揃え方: コピーは NFKC を持たない（クレートを足さない）ので、前後の空白と小文字だけをコピーする。検査の名は NFKC で
            // 変わらない文字で選ぶ。見本の手順が変わったら、コピーも直すよう落とす。
            assert_eq!(
                s("room_name_fold"),
                "nfkc,trim,lower",
                "見本の揃え方が変わった。偽サーバーの normalize を直す"
            );
            let room_name_controls = s("room_name_controls")
                .split(',')
                .map(|part| {
                    let (lo, hi) = part.split_once('-').expect("制御文字の範囲は lo-hi");
                    let hex = |x: &str| {
                        u32::from_str_radix(x.strip_prefix("U+").expect("U+ で始まる"), 16)
                            .expect("16 進")
                    };
                    (hex(lo), hex(hi))
                })
                .collect();
            Rules {
                path_prefix: s("path_prefix"),
                room_name_min: n("room_name_min") as usize,
                room_name_max: n("room_name_max") as usize,
                room_name_max_bytes: n("room_name_max_bytes") as usize,
                room_name_controls,
                room_name_bad_chars: s("room_name_bad_chars"),
                room_name_bad_names: s("room_name_bad_names")
                    .split(',')
                    .map(str::to_owned)
                    .collect(),
                code_min_chars: n("code_min_chars") as usize,
                code_max_chars: n("code_max_chars") as usize,
                code_chars: s("code_chars"),
                message_max_bytes: n("message_max_bytes") as usize,
                room_ttl_ms: n("room_ttl_ms") as u64,
                answer_ttl_ms: n("answer_ttl_ms") as u64,
                ping_text: s("ping_text"),
                pong_text: s("pong_text"),
                retry_after_secs: n("retry_after_secs") as u64,
                closes,
                random_path_prefix: s("random_path_prefix"),
                random_room_prefix: s("random_room_prefix"),
                random_room_hex_len: n("random_room_hex_len") as usize,
                random_queue_ttl_ms: n("random_queue_ttl_ms") as u64,
                random_room_ttl_ms: n("random_room_ttl_ms") as u64,
                watch_path_prefix: s("watch_path_prefix"),
                watch_post_segment: s("watch_post_segment"),
                watch_view_segment: s("watch_view_segment"),
                watch_max_total: n("watch_max_total") as usize,
                watch_cap_max: n("watch_cap_max"),
                watch_delay_max: n("watch_delay_max"),
                watch_modes: s("watch_modes").split(',').map(str::to_owned).collect(),
            }
        }

        /// rules.ts の `parseBuildSegment`: 先頭に 0 を付けない 10 進で u32 の範囲。
        fn build_from_segment(seg: &str) -> Option<u32> {
            let canonical = seg == "0"
                || (!seg.starts_with('0')
                    && !seg.is_empty()
                    && seg.len() <= 10
                    && seg.bytes().all(|b| b.is_ascii_digit()));
            canonical.then(|| seg.parse().ok()).flatten()
        }

        /// 揃えた後の名が、ランダム対戦の割り当てた形（先頭 ＋ 小文字の 16 進）か。
        fn is_random_room(&self, name: &str) -> bool {
            name.strip_prefix(self.random_room_prefix.as_str())
                .is_some_and(|hex| {
                    hex.len() == self.random_room_hex_len
                        && hex
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
        }

        pub fn close(&self, name: &str) -> u16 {
            self.closes
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, c)| *c)
                .unwrap_or_else(|| panic!("見本に close_{name} が無い"))
        }

        /// rules.ts の `roomNameFromPath`: パスの先頭の後ろが空でなく `/` を含まない。
        fn room_from_path<'a>(&self, path: &'a str) -> Option<&'a str> {
            let rest = path.strip_prefix(self.path_prefix.as_str())?;
            (!rest.is_empty() && !rest.contains('/')).then_some(rest)
        }

        /// rules.ts の `decodeRoomName` と `normalizeRoomName`（パスの区切りを受け、符号化を戻して確かめ、揃えた文字を返す）。
        /// 揃え方は NFKC を除いたコピー（[`Rules::load`] の注釈）。
        fn normalize(&self, segment: &str) -> Option<String> {
            // 区切りの前置きのガード（1 B は 3 文字まで）と、戻せない符号（`%` の後が 16 進 2 桁でない・UTF-8 として壊れている）。
            if segment.len() > self.room_name_max_bytes * 3 {
                return None;
            }
            let raw = percent_decode(segment)?;
            if raw.len() > self.room_name_max_bytes {
                return None;
            }
            let bad = |c: char| {
                let cp = u32::from(c);
                self.room_name_controls
                    .iter()
                    .any(|&(lo, hi)| (lo..=hi).contains(&cp))
                    || self.room_name_bad_chars.contains(c)
            };
            if raw.chars().any(bad) || self.room_name_bad_names.contains(&raw) {
                return None;
            }
            let key = raw
                .trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
                .to_lowercase();
            let n = key.chars().count();
            (n >= self.room_name_min && n <= self.room_name_max).then_some(key)
        }

        /// rules.ts の `checkCode`（長さ → 文字の順）。良ければ None、悪ければ閉じる名。
        fn check_code(&self, code: &str) -> Option<&'static str> {
            let n: usize = code.chars().map(char::len_utf16).sum();
            if n > self.code_max_chars {
                return Some("too_large");
            }
            let ok = n >= self.code_min_chars && code.chars().all(|c| self.code_chars.contains(c));
            (!ok).then_some("bad_code")
        }
    }

    #[derive(Default)]
    pub struct Knobs {
        /// ping に pong を返さない（自動応答を止める。片側だけ切れた TCP のコピー）。
        pub no_pong: AtomicBool,
        /// 何を受けても読まない・答えない（close にも閉じ返さない。止まったサーバーのコピー）。
        pub deaf: AtomicBool,
        /// 101 の後に room を送らない。
        pub no_room: AtomicBool,
        /// room の後に、番号の無い close を送る（WinHTTP がそれを 1005 と返すかを見る）。
        pub close_without_status: AtomicBool,
        /// 閉じる前の error の文を送らない（閉じる番号だけで正しく読めるかを見る）。
        pub no_error_text: AtomicBool,
        /// 0 なら見本の値（rules.json）。期限の検査が短く差し替える。
        pub room_ttl_ms: AtomicU64,
        pub answer_ttl_ms: AtomicU64,
        /// 0 なら見本の値。ランダム対戦の待ち列の期限と、割り当てたルームの期限。
        pub random_queue_ttl_ms: AtomicU64,
        pub random_room_ttl_ms: AtomicU64,
        /// 待ち列で組にしない（待ち列の「待つ」を続けさせる）。
        pub no_pairing: AtomicBool,
        /// 一覧の載せるエンドポイントが、post を list_full で断る（件数の上限のコピー）。
        pub list_full: AtomicBool,
        /// 一覧の載せるエンドポイントが、post を bad_message で断る（入り直しの検査用）。
        pub list_bad: AtomicBool,
        /// 回数の歯止めまでに通す要求の数（0 なら数えない）。
        pub limit: AtomicUsize,
        /// 入口で必ずこの応答を返す（状態の番号・本文）。Cloudflare 自身の誤りの頁などのコピー。
        pub forced: Mutex<Option<(u16, String)>>,
    }

    #[derive(Default)]
    pub struct Seen {
        pub texts: Mutex<Vec<String>>,
        pub client_closes: Mutex<Vec<Option<u16>>>,
        /// 届いた要求のパス（WinHTTP がルーム名の符号化を変えずに渡すかを見るため。届いた文字のまま。問いは含めない）。
        pub paths: Mutex<Vec<String>>,
        /// 届いた要求の問い（`?` の後ろ。無ければ空）。`paths` と同じ順。
        pub queries: Mutex<Vec<String>>,
        pub upgrades: AtomicUsize,
        /// ランダム対戦の待ち列に入った数（Upgrade した数）。
        pub queue_upgrades: AtomicUsize,
        /// 待ち列が割り当てたルーム名（検査が「ログや画面に出ていない」を確かめる元）。
        pub assigned: Mutex<Vec<String>>,
        /// 一覧の載せるエンドポイント・見るエンドポイントに入った数（Upgrade した数）。
        pub watch_post_upgrades: AtomicUsize,
        pub watch_view_upgrades: AtomicUsize,
        /// 一覧の載せるエンドポイントへ届いた post の文（検査が 1P・2P の名前・方式を確かめる）。
        pub posts: Mutex<Vec<String>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Side {
        Offer,
        Answer,
    }

    impl Side {
        fn wire(self) -> &'static str {
            match self {
                Side::Offer => "host",
                Side::Answer => "guest",
            }
        }
    }

    struct Member {
        id: usize,
        side: Side,
        deadline: Instant,
        deadline_unix_ms: u64,
        code: Option<String>,
        got_peer: bool,
        done: bool,
        w: Arc<Mutex<TcpStream>>,
    }

    /// ランダム対戦の待ち列の 1 人（random_queue.ts の `QueueAtt`）。
    struct QMember {
        id: usize,
        deadline: Instant,
        done: bool,
        w: Arc<Mutex<TcpStream>>,
    }

    /// 一覧に載った 1 件（watch_list.ts の `WatchItem`）。
    #[derive(Clone)]
    struct WItem {
        room: String,
        p1: String,
        p2: String,
        mode: String,
        delay: i64,
        watchers: i64,
        cap: i64,
    }

    struct WPost {
        id: usize,
        item: Option<WItem>,
        w: Arc<Mutex<TcpStream>>,
    }

    struct WView {
        id: usize,
        w: Arc<Mutex<TcpStream>>,
    }

    #[derive(Default)]
    struct WatchBook {
        posts: Vec<WPost>,
        views: Vec<WView>,
    }

    struct Inner {
        rules: Rules,
        knobs: Arc<Knobs>,
        seen: Arc<Seen>,
        rooms: Mutex<HashMap<String, Vec<Member>>>,
        /// ビルド番号ごとの待ち列（入った順）。
        queues: Mutex<HashMap<u32, Vec<QMember>>>,
        /// ビルド番号ごとの一覧（載せた順）と、見ている人。
        watch: Mutex<HashMap<u32, WatchBook>>,
        next_id: AtomicUsize,
        passed: AtomicUsize,
        streams: Mutex<Vec<TcpStream>>,
        stop: AtomicBool,
    }

    pub struct FakeServer {
        pub addr: SocketAddr,
        pub knobs: Arc<Knobs>,
        pub seen: Arc<Seen>,
        inner: Arc<Inner>,
        threads: Vec<JoinHandle<()>>,
    }

    fn unix_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }

    impl FakeServer {
        pub fn start() -> FakeServer {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback で受けられない");
            let addr = listener.local_addr().unwrap();
            let knobs = Arc::new(Knobs::default());
            let seen = Arc::new(Seen::default());
            let inner = Arc::new(Inner {
                rules: Rules::load(),
                knobs: Arc::clone(&knobs),
                seen: Arc::clone(&seen),
                rooms: Mutex::new(HashMap::new()),
                queues: Mutex::new(HashMap::new()),
                watch: Mutex::new(HashMap::new()),
                next_id: AtomicUsize::new(1),
                passed: AtomicUsize::new(0),
                streams: Mutex::new(Vec::new()),
                stop: AtomicBool::new(false),
            });
            let acc = {
                let inner = Arc::clone(&inner);
                std::thread::spawn(move || {
                    for conn in listener.incoming() {
                        if inner.stop.load(Ordering::Acquire) {
                            break;
                        }
                        let Ok(s) = conn else { continue };
                        if let Ok(c) = s.try_clone() {
                            lock(&inner.streams).push(c);
                        }
                        let inner = Arc::clone(&inner);
                        std::thread::spawn(move || serve(&inner, s));
                    }
                })
            };
            let alarm = {
                let inner = Arc::clone(&inner);
                std::thread::spawn(move || {
                    while !inner.stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(5));
                        let now = Instant::now();
                        let mut rooms = lock(&inner.rooms);
                        for room in rooms.values_mut() {
                            expire_if_due(&inner, room, now);
                        }
                        drop(rooms);
                        let mut queues = lock(&inner.queues);
                        for queue in queues.values_mut() {
                            expire_queue_if_due(&inner, queue, now);
                        }
                    }
                })
            };
            FakeServer {
                addr,
                knobs,
                seen,
                inner,
                threads: vec![acc, alarm],
            }
        }

        pub fn base(&self) -> String {
            format!("ws://{}", self.addr)
        }

        pub fn rules(&self) -> &Rules {
            &self.inner.rules
        }

        pub fn texts_matching(&self, pred: impl Fn(&str) -> bool) -> usize {
            lock(&self.seen.texts).iter().filter(|t| pred(t)).count()
        }

        /// 一覧（ビルド番号ごと）に載っている件を載せた順に。観戦用のルーム名は割り当ての名から導いた `w.…`。
        pub fn watch_items(&self, build: u32) -> Vec<ListedItem> {
            lock(&self.inner.watch)
                .get(&build)
                .map(|b| {
                    b.posts
                        .iter()
                        .filter_map(|p| p.item.clone())
                        .map(|i| ListedItem {
                            room: i.room,
                            p1: i.p1,
                            p2: i.p2,
                            mode: i.mode,
                            delay: i.delay,
                            watchers: i.watchers,
                            cap: i.cap,
                        })
                        .collect()
                })
                .unwrap_or_default()
        }

        /// 一覧を見ている接続の数（ビルド番号ごと）。
        pub fn watch_views(&self, build: u32) -> usize {
            lock(&self.inner.watch)
                .get(&build)
                .map_or(0, |b| b.views.len())
        }
    }

    /// 偽サーバーの一覧に載っている 1 件。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ListedItem {
        pub room: String,
        pub p1: String,
        pub p2: String,
        pub mode: String,
        pub delay: i64,
        pub watchers: i64,
        pub cap: i64,
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.inner.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.addr);
            for s in lock(&self.inner.streams).iter() {
                let _ = s.shutdown(Shutdown::Both);
            }
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }

    fn ttl(knob: &AtomicU64, default: u64) -> u64 {
        match knob.load(Ordering::Acquire) {
            0 => default,
            v => v,
        }
    }

    // ---- 枠（RFC 6455 §5.2）----

    fn write_frame(w: &Mutex<TcpStream>, op: u8, payload: &[u8]) {
        let mut f = vec![0x80 | op];
        let n = payload.len();
        if n < 126 {
            f.push(n as u8);
        } else if n < 65536 {
            f.push(126);
            f.extend((n as u16).to_be_bytes());
        } else {
            f.push(127);
            f.extend((n as u64).to_be_bytes());
        }
        f.extend_from_slice(payload);
        let _ = lock(w).write_all(&f);
    }

    fn send_text(m: &Member, text: &str) {
        write_frame(&m.w, 0x1, text.as_bytes());
    }

    /// room.ts の `closeWith`: 印を付け、withError なら error の文を 1 通送ってから閉じる（1000 では送らない）。
    /// つまみ `no_error_text` は error の文を省く。
    fn close_with(inner: &Inner, m: &mut Member, name: &str, with_error: bool) {
        m.done = true;
        if with_error && name != "done" && !inner.knobs.no_error_text.load(Ordering::Acquire) {
            send_text(m, &format!(r#"{{"t":"error","code":"{name}"}}"#));
        }
        let mut p = inner.rules.close(name).to_be_bytes().to_vec();
        p.extend_from_slice(name.as_bytes());
        write_frame(&m.w, 0x8, &p);
    }

    fn read_frame(r: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
        let mut msg = Vec::new();
        let mut first = 0u8;
        loop {
            let mut h = [0u8; 2];
            r.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let op = h[0] & 0x0f;
            let masked = h[1] & 0x80 != 0;
            let mut len = u64::from(h[1] & 0x7f);
            if len == 126 {
                let mut e = [0u8; 2];
                r.read_exact(&mut e)?;
                len = u64::from(u16::from_be_bytes(e));
            } else if len == 127 {
                let mut e = [0u8; 8];
                r.read_exact(&mut e)?;
                len = u64::from_be_bytes(e);
            }
            if len > 1 << 20 {
                return Err(io::Error::other("大きすぎる枠"));
            }
            let mut key = [0u8; 4];
            if masked {
                r.read_exact(&mut key)?;
            }
            let mut p = vec![0u8; len as usize];
            r.read_exact(&mut p)?;
            if masked {
                for (i, b) in p.iter_mut().enumerate() {
                    *b ^= key[i % 4];
                }
            }
            if op >= 0x8 {
                return Ok((op, p));
            }
            if op != 0 {
                first = op;
            }
            msg.extend_from_slice(&p);
            if fin {
                return Ok((first, msg));
            }
        }
    }

    fn base64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in data.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for k in 0..4 {
                if k <= c.len() {
                    out.push(T[((n >> (18 - 6 * k)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    // ---- HTTP の入口（index.ts）----

    struct Head {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
    }

    impl Head {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    fn read_head(s: &mut TcpStream) -> Option<Head> {
        s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut buf = Vec::new();
        let mut b = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if buf.len() > 8192 || s.read(&mut b).ok()? == 0 {
                return None;
            }
            buf.push(b[0]);
        }
        s.set_read_timeout(None).ok()?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_owned();
        let path = first.next()?.to_owned();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        Some(Head {
            method,
            path,
            headers,
        })
    }

    fn respond(s: &mut TcpStream, status: u16, extra: &str, body: &str) {
        let r = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        );
        let _ = s.write_all(r.as_bytes());
        let _ = s.flush();
        let _ = s.shutdown(Shutdown::Write);
        // 相手が読み終えて閉じるまで待つ（先に閉じると RST で本文が落ちることがある）。
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let mut sink = [0u8; 256];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    }

    fn serve(inner: &Inner, mut s: TcpStream) {
        let Some(head) = read_head(&mut s) else {
            return;
        };
        let rules = &inner.rules;
        // 問いは index.ts が見る `url.pathname` に入らない。ここでもパスから外す。
        let (path, query) = head.path.split_once('?').unwrap_or((&head.path, ""));
        lock(&inner.seen.paths).push(path.to_owned());
        lock(&inner.seen.queries).push(query.to_owned());
        let forced = lock(&inner.knobs.forced).clone();
        if let Some((status, body)) = forced {
            return respond(&mut s, status, "", &body);
        }
        if path.starts_with(rules.random_path_prefix.as_str()) {
            return serve_queue(inner, s, &head, path);
        }
        if path.starts_with(rules.watch_path_prefix.as_str()) {
            return serve_watch(inner, s, &head, path);
        }
        let Some(raw) = rules.room_from_path(path) else {
            return respond(&mut s, 404, "", r#"{"error":"not_found"}"#);
        };
        if head.method != "GET" {
            return respond(&mut s, 405, "", r#"{"error":"method_not_allowed"}"#);
        }
        // ルーム名は符号化されたまま来る。戻せない符号も文字の決まりの外と同じ 400。
        let Some(name) = rules.normalize(raw) else {
            return respond(&mut s, 400, "", r#"{"error":"bad_room_name"}"#);
        };
        if !head
            .header("Upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            return respond(&mut s, 426, "", r#"{"error":"upgrade_required"}"#);
        }
        if refuse_if_over_limit(inner, &mut s) {
            return;
        }
        let Some(key) = head.header("Sec-WebSocket-Key") else {
            return respond(&mut s, 400, "", "");
        };
        let accept =
            base64(&crate::room_win::sha1(format!("{key}{WS_GUID}").as_bytes()).expect("SHA-1"));

        let id = inner.next_id.fetch_add(1, Ordering::AcqRel);
        {
            let mut rooms = lock(&inner.rooms);
            let now = Instant::now();
            let room = rooms.entry(name.clone()).or_default();
            expire_if_due(inner, room, now);
            let live: Vec<&Member> = room.iter().filter(|m| !m.done).collect();
            let (side, deadline, deadline_unix_ms) = if live.is_empty() {
                let t = if rules.is_random_room(&name) {
                    ttl(&inner.knobs.random_room_ttl_ms, rules.random_room_ttl_ms)
                } else {
                    ttl(&inner.knobs.room_ttl_ms, rules.room_ttl_ms)
                };
                (Side::Offer, now + Duration::from_millis(t), unix_ms() + t)
            } else if live.len() == 1 && live[0].side == Side::Offer {
                (Side::Answer, live[0].deadline, live[0].deadline_unix_ms)
            } else {
                drop(rooms);
                return respond(&mut s, 409, "", r#"{"error":"room_full"}"#);
            };
            let hs = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            if s.write_all(hs.as_bytes()).is_err() {
                return;
            }
            let Ok(w) = s.try_clone() else { return };
            let mut m = Member {
                id,
                side,
                deadline,
                deadline_unix_ms,
                code: None,
                got_peer: false,
                done: false,
                w: Arc::new(Mutex::new(w)),
            };
            inner.seen.upgrades.fetch_add(1, Ordering::AcqRel);
            if !inner.knobs.no_room.load(Ordering::Acquire) {
                send_text(
                    &m,
                    &format!(
                        r#"{{"t":"room","role":"{}","expires_at":{}}}"#,
                        side.wire(),
                        deadline_unix_ms / 1000
                    ),
                );
            }
            if inner.knobs.close_without_status.load(Ordering::Acquire) {
                m.done = true;
                write_frame(&m.w, 0x8, &[]);
            }
            room.push(m);
            if side == Side::Answer {
                try_match(inner, room, now);
            }
        }
        read_loop(inner, &name, id, &mut s);
        let _ = s.shutdown(Shutdown::Both);
    }

    /// index.ts の `overLimit`: 通す数（`knobs.limit`）を越えていれば 429 を返して true。ルームと待ち列で 1 本の数え。
    fn refuse_if_over_limit(inner: &Inner, s: &mut TcpStream) -> bool {
        let limit = inner.knobs.limit.load(Ordering::Acquire);
        if limit > 0 && inner.passed.fetch_add(1, Ordering::AcqRel) >= limit {
            let secs = inner.rules.retry_after_secs;
            respond(
                s,
                429,
                &format!("Retry-After: {secs}\r\n"),
                &format!(r#"{{"error":"rate_limited","retry_after":{secs}}}"#),
            );
            return true;
        }
        false
    }

    // ---- ランダム対戦の待ち列（random_queue.ts のコピー）----
    //
    // コピーしていないもの: 同じ cid の入り直しの置き換え（ルームのコピーと同じ理由。検査は 1 つのプロセスから 2 人分を繋ぐので cid が同じ）。

    /// index.ts の待ち列のエンドポイントと `RandomQueue.fetch`: ビルド番号ごとの待ち列に入れ、2 人そろえば割り当てたルーム名を返して閉じる。
    fn serve_queue(inner: &Inner, mut s: TcpStream, head: &Head, path: &str) {
        let rules = &inner.rules;
        if head.method != "GET" {
            return respond(&mut s, 405, "", r#"{"error":"method_not_allowed"}"#);
        }
        let seg = path
            .strip_prefix(rules.random_path_prefix.as_str())
            .unwrap_or("");
        let Some(build) = Rules::build_from_segment(seg) else {
            return respond(&mut s, 400, "", r#"{"error":"bad_build"}"#);
        };
        if !head
            .header("Upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            return respond(&mut s, 426, "", r#"{"error":"upgrade_required"}"#);
        }
        if refuse_if_over_limit(inner, &mut s) {
            return;
        }
        let Some(key) = head.header("Sec-WebSocket-Key") else {
            return respond(&mut s, 400, "", "");
        };
        let accept =
            base64(&crate::room_win::sha1(format!("{key}{WS_GUID}").as_bytes()).expect("SHA-1"));
        let id = inner.next_id.fetch_add(1, Ordering::AcqRel);
        {
            let mut queues = lock(&inner.queues);
            let now = Instant::now();
            let q = queues.entry(build).or_default();
            q.retain(|m| !m.done);
            expire_queue_if_due(inner, q, now);
            let hs = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            if s.write_all(hs.as_bytes()).is_err() {
                return;
            }
            let Ok(w) = s.try_clone() else { return };
            let t = ttl(&inner.knobs.random_queue_ttl_ms, rules.random_queue_ttl_ms);
            let deadline_unix_ms = unix_ms() + t;
            let m = QMember {
                id,
                deadline: now + Duration::from_millis(t),
                done: false,
                w: Arc::new(Mutex::new(w)),
            };
            inner.seen.queue_upgrades.fetch_add(1, Ordering::AcqRel);
            if !inner.knobs.no_room.load(Ordering::Acquire) {
                send_queue_text(
                    &m,
                    &format!(
                        r#"{{"t":"queue","expires_at":{}}}"#,
                        deadline_unix_ms / 1000
                    ),
                );
            }
            q.push(m);
            try_pair_queue(inner, q);
        }
        queue_loop(inner, build, id, &mut s);
        let _ = s.shutdown(Shutdown::Both);
    }

    fn send_queue_text(m: &QMember, text: &str) {
        write_frame(&m.w, 0x1, text.as_bytes());
    }

    fn qclose(inner: &Inner, m: &mut QMember, name: &str, with_error: bool) {
        m.done = true;
        if with_error && name != "done" && !inner.knobs.no_error_text.load(Ordering::Acquire) {
            send_queue_text(m, &format!(r#"{{"t":"error","code":"{name}"}}"#));
        }
        let mut p = inner.rules.close(name).to_be_bytes().to_vec();
        p.extend_from_slice(name.as_bytes());
        write_frame(&m.w, 0x8, &p);
    }

    /// `RandomQueue.tryPair`: 入った順に先の 2 人から組にする。割り当てた名は `seen.assigned` にも残す（検査用）。
    fn try_pair_queue(inner: &Inner, q: &mut [QMember]) {
        if inner.knobs.no_pairing.load(Ordering::Acquire) {
            return;
        }
        loop {
            let live: Vec<usize> = (0..q.len()).filter(|&i| !q[i].done).collect();
            let (Some(&a), Some(&b)) = (live.first(), live.get(1)) else {
                return;
            };
            let mut raw = vec![0u8; inner.rules.random_room_hex_len / 2];
            assert!(crate::net_win::os_random(&mut raw), "OS の乱数");
            let hex: String = raw.iter().map(|x| format!("{x:02x}")).collect();
            let room = format!("{}{hex}", inner.rules.random_room_prefix);
            lock(&inner.seen.assigned).push(room.clone());
            send_queue_text(
                &q[a],
                &format!(r#"{{"t":"paired","room":"{room}","order":"first"}}"#),
            );
            send_queue_text(
                &q[b],
                &format!(r#"{{"t":"paired","room":"{room}","order":"second"}}"#),
            );
            qclose(inner, &mut q[a], "done", false);
            qclose(inner, &mut q[b], "done", false);
        }
    }

    fn expire_queue_if_due(inner: &Inner, q: &mut [QMember], now: Instant) {
        for m in q.iter_mut().filter(|m| !m.done && now >= m.deadline) {
            qclose(inner, m, "expired", true);
        }
    }

    fn queue_loop(inner: &Inner, build: u32, id: usize, s: &mut TcpStream) {
        let leave = |inner: &Inner| {
            if let Some(m) = lock(&inner.queues)
                .get_mut(&build)
                .and_then(|q| q.iter_mut().find(|m| m.id == id))
            {
                m.done = true;
            }
        };
        loop {
            let Ok((op, payload)) = read_frame(s) else {
                return leave(inner);
            };
            if inner.knobs.deaf.load(Ordering::Acquire) {
                continue;
            }
            let mut queues = lock(&inner.queues);
            let Some(m) = queues
                .get_mut(&build)
                .and_then(|q| q.iter_mut().find(|m| m.id == id))
            else {
                return;
            };
            if op == 0x1 {
                lock(&inner.seen.texts).push(String::from_utf8_lossy(&payload).into_owned());
            }
            match op {
                0x1 if String::from_utf8_lossy(&payload) == inner.rules.ping_text => {
                    if !inner.knobs.no_pong.load(Ordering::Acquire) {
                        send_queue_text(m, &inner.rules.pong_text);
                    }
                }
                0x8 => {
                    lock(&inner.seen.client_closes).push(
                        (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]])),
                    );
                    if !m.done {
                        let mut p = 1000u16.to_be_bytes().to_vec();
                        p.extend_from_slice(b"bye");
                        write_frame(&m.w, 0x8, &p);
                    }
                    m.done = true;
                    return;
                }
                0x9 => write_frame(&m.w, 0xA, &payload),
                0xA => {}
                _ if !m.done => qclose(inner, m, "bad_message", true),
                _ => {}
            }
        }
    }

    // ---- 進行中のランダム対戦の一覧（watch_list.ts のコピー）----
    //
    // コピーしていないもの: 1 IP あたり 2 件の上限・載せた人が生きているかの期限・見るエンドポイントへの変化のまとめ送り（変わるたびにすぐ全体を送る）・
    // 同じルームの載せ直しの置き換え。クライアントの文は JSON.parse でなく `read_flat` で読む。

    fn is_name(s: &str) -> bool {
        (1..=32).contains(&s.chars().count())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'@' | b'?'))
    }

    /// `parsePost` のコピー: 割り当ての名の形・名前の文字・方式・数の範囲。通れば一覧に載せる 1 件（観戦用のルーム名は割り当ての名から導く）。
    fn parse_post(rules: &Rules, text: &str) -> Option<WItem> {
        let list = std::panic::catch_unwind(|| read_flat(text)).ok()?;
        let st = |k: &str| match list.iter().find(|(n, _)| n == k) {
            Some((_, Val::S(v))) => Some(v.clone()),
            _ => None,
        };
        let num = |k: &str| match list.iter().find(|(n, _)| n == k) {
            Some((_, Val::I(v))) => Some(*v),
            _ => None,
        };
        if st("t")? != "post" {
            return None;
        }
        let q = st("room")?;
        if !rules.is_random_room(&q) {
            return None;
        }
        let (p1, p2, mode) = (st("p1")?, st("p2")?, st("mode")?);
        let (delay, watchers, cap) = (num("delay")?, num("watchers")?, num("cap")?);
        let ok = is_name(&p1)
            && is_name(&p2)
            && rules.watch_modes.contains(&mode)
            && (0..=rules.watch_delay_max).contains(&delay)
            && (1..=rules.watch_cap_max).contains(&cap)
            && (0..=cap).contains(&watchers);
        if !ok {
            return None;
        }
        let room = higanr_core::room::RoomName::parse_server_rules(&q)
            .ok()?
            .spectate_of()
            .as_str()
            .to_owned();
        Some(WItem {
            room,
            p1,
            p2,
            mode,
            delay,
            watchers,
            cap,
        })
    }

    /// `WatchList.listMessage` のコピー: 載せた順の全体（鍵の順は JSON.stringify の出力と同じ）。
    fn watch_list_text(book: &WatchBook) -> String {
        let items: Vec<String> = book
            .posts
            .iter()
            .filter_map(|p| p.item.as_ref())
            .map(|i| {
                format!(
                    r#"{{"room":"{}","p1":"{}","p2":"{}","mode":"{}","delay":{},"watchers":{},"cap":{}}}"#,
                    i.room, i.p1, i.p2, i.mode, i.delay, i.watchers, i.cap
                )
            })
            .collect();
        format!(r#"{{"t":"list","items":[{}]}}"#, items.join(","))
    }

    fn broadcast_watch(book: &WatchBook) {
        let text = watch_list_text(book);
        for v in &book.views {
            write_frame(&v.w, 0x1, text.as_bytes());
        }
    }

    fn serve_watch(inner: &Inner, mut s: TcpStream, head: &Head, path: &str) {
        let rules = &inner.rules;
        if head.method != "GET" {
            return respond(&mut s, 405, "", r#"{"error":"method_not_allowed"}"#);
        }
        let rest = path
            .strip_prefix(rules.watch_path_prefix.as_str())
            .unwrap_or("");
        let mut parts = rest.split('/');
        let (Some(seg), Some(kind), None) = (parts.next(), parts.next(), parts.next()) else {
            return respond(&mut s, 404, "", r#"{"error":"not_found"}"#);
        };
        let is_post = kind == rules.watch_post_segment;
        if !is_post && kind != rules.watch_view_segment {
            return respond(&mut s, 404, "", r#"{"error":"not_found"}"#);
        }
        let Some(build) = Rules::build_from_segment(seg) else {
            return respond(&mut s, 400, "", r#"{"error":"bad_build"}"#);
        };
        if !head
            .header("Upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            return respond(&mut s, 426, "", r#"{"error":"upgrade_required"}"#);
        }
        if refuse_if_over_limit(inner, &mut s) {
            return;
        }
        let Some(key) = head.header("Sec-WebSocket-Key") else {
            return respond(&mut s, 400, "", "");
        };
        let accept =
            base64(&crate::room_win::sha1(format!("{key}{WS_GUID}").as_bytes()).expect("SHA-1"));
        let id = inner.next_id.fetch_add(1, Ordering::AcqRel);
        {
            let mut watch = lock(&inner.watch);
            let book = watch.entry(build).or_default();
            let hs = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            if s.write_all(hs.as_bytes()).is_err() {
                return;
            }
            let Ok(w) = s.try_clone() else { return };
            let w = Arc::new(Mutex::new(w));
            if is_post {
                inner
                    .seen
                    .watch_post_upgrades
                    .fetch_add(1, Ordering::AcqRel);
                book.posts.push(WPost { id, item: None, w });
            } else {
                inner
                    .seen
                    .watch_view_upgrades
                    .fetch_add(1, Ordering::AcqRel);
                write_frame(&w, 0x1, watch_list_text(book).as_bytes());
                book.views.push(WView { id, w });
            }
        }
        watch_loop(inner, build, id, is_post, &mut s);
        let _ = s.shutdown(Shutdown::Both);
    }

    /// 載せるエンドポイント・見るエンドポイントの接続を 1 本読む。切れたら一覧から外し、載っていたなら見る人へ全体を送り直す。
    fn watch_loop(inner: &Inner, build: u32, id: usize, is_post: bool, s: &mut TcpStream) {
        let leave = |inner: &Inner| {
            let mut watch = lock(&inner.watch);
            let Some(book) = watch.get_mut(&build) else {
                return;
            };
            if is_post {
                let listed = book.posts.iter().any(|p| p.id == id && p.item.is_some());
                book.posts.retain(|p| p.id != id);
                if listed {
                    broadcast_watch(book);
                }
            } else {
                book.views.retain(|v| v.id != id);
            }
        };
        loop {
            let Ok((op, payload)) = read_frame(s) else {
                return leave(inner);
            };
            if inner.knobs.deaf.load(Ordering::Acquire) {
                continue;
            }
            let own = |book: &WatchBook| -> Option<Arc<Mutex<TcpStream>>> {
                if is_post {
                    book.posts
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| Arc::clone(&p.w))
                } else {
                    book.views
                        .iter()
                        .find(|v| v.id == id)
                        .map(|v| Arc::clone(&v.w))
                }
            };
            match op {
                0x1 => {
                    let text = String::from_utf8_lossy(&payload).into_owned();
                    lock(&inner.seen.texts).push(text.clone());
                    let mut watch = lock(&inner.watch);
                    let Some(book) = watch.get_mut(&build) else {
                        return;
                    };
                    let Some(w) = own(book) else { return };
                    if text == inner.rules.ping_text {
                        if !inner.knobs.no_pong.load(Ordering::Acquire) {
                            write_frame(&w, 0x1, inner.rules.pong_text.as_bytes());
                        }
                        continue;
                    }
                    let name = if !is_post {
                        Some("bad_message")
                    } else {
                        on_post_text(inner, book, id, &text)
                    };
                    if let Some(name) = name {
                        write_frame(
                            &w,
                            0x1,
                            format!(r#"{{"t":"error","code":"{name}"}}"#).as_bytes(),
                        );
                        let mut p = inner.rules.close(name).to_be_bytes().to_vec();
                        p.extend_from_slice(name.as_bytes());
                        write_frame(&w, 0x8, &p);
                        drop(watch);
                        return leave(inner);
                    }
                }
                0x8 => {
                    let mut watch = lock(&inner.watch);
                    if let Some(book) = watch.get_mut(&build)
                        && let Some(w) = own(book)
                    {
                        let mut p = 1000u16.to_be_bytes().to_vec();
                        p.extend_from_slice(b"bye");
                        write_frame(&w, 0x8, &p);
                    }
                    drop(watch);
                    lock(&inner.seen.client_closes).push(
                        (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]])),
                    );
                    return leave(inner);
                }
                0x9 => {
                    let mut watch = lock(&inner.watch);
                    if let Some(w) = watch.get_mut(&build).and_then(|b| own(b)) {
                        write_frame(&w, 0xA, &payload);
                    }
                }
                _ => {}
            }
        }
    }

    /// 載せるエンドポイントの 1 通: 最初の post なら一覧へ載せて listed を返す。count なら数を替える。断るなら閉じる名を返す。
    fn on_post_text(
        inner: &Inner,
        book: &mut WatchBook,
        id: usize,
        text: &str,
    ) -> Option<&'static str> {
        let rules = &inner.rules;
        let listed_before = book.posts.iter().any(|p| p.id == id && p.item.is_some());
        if text.starts_with(r#"{"t":"count","watchers":"#) {
            let n: i64 = text
                .trim_start_matches(r#"{"t":"count","watchers":"#)
                .trim_end_matches('}')
                .parse()
                .ok()?;
            let post = book.posts.iter_mut().find(|p| p.id == id)?;
            let item = post.item.as_mut()?;
            if !(0..=item.cap).contains(&n) {
                return Some("bad_message");
            }
            item.watchers = n;
            broadcast_watch(book);
            return None;
        }
        if listed_before {
            return Some("bad_message");
        }
        lock(&inner.seen.posts).push(text.to_owned());
        let Some(item) = parse_post(rules, text) else {
            return Some("bad_message");
        };
        if inner.knobs.list_bad.load(Ordering::Acquire) {
            return Some("bad_message");
        }
        let total = book.posts.iter().filter(|p| p.item.is_some()).count();
        if inner.knobs.list_full.load(Ordering::Acquire) || total >= rules.watch_max_total {
            return Some("list_full");
        }
        let post = book.posts.iter().find(|p| p.id == id)?;
        write_frame(&post.w, 0x1, br#"{"t":"listed"}"#);
        if let Some(p) = book.posts.iter_mut().find(|p| p.id == id) {
            p.item = Some(item);
        }
        broadcast_watch(book);
        None
    }

    fn read_loop(inner: &Inner, name: &str, id: usize, s: &mut TcpStream) {
        loop {
            let Ok((op, payload)) = read_frame(s) else {
                return gone(inner, name, id);
            };
            if inner.knobs.deaf.load(Ordering::Acquire) {
                continue;
            }
            match op {
                0x1 => on_text(inner, name, id, &String::from_utf8_lossy(&payload)),
                0x8 => {
                    let code =
                        (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]));
                    lock(&inner.seen.client_closes).push(code);
                    let mut rooms = lock(&inner.rooms);
                    if let Some(m) = rooms
                        .get_mut(name)
                        .and_then(|r| r.iter_mut().find(|m| m.id == id))
                        && !m.done
                    {
                        let mut p = 1000u16.to_be_bytes().to_vec();
                        p.extend_from_slice(b"bye");
                        write_frame(&m.w, 0x8, &p);
                    }
                    drop(rooms);
                    return gone(inner, name, id);
                }
                0x9 => {
                    let rooms = lock(&inner.rooms);
                    if let Some(m) = rooms.get(name).and_then(|r| r.iter().find(|m| m.id == id)) {
                        write_frame(&m.w, 0xA, &payload);
                    }
                }
                0xA => {}
                _ => {
                    let mut rooms = lock(&inner.rooms);
                    if let Some(room) = rooms.get_mut(name) {
                        fail(inner, room, id, "bad_message");
                    }
                }
            }
        }
    }

    fn idx(room: &[Member], id: usize) -> Option<usize> {
        room.iter().position(|m| m.id == id)
    }

    fn on_text(inner: &Inner, name: &str, id: usize, text: &str) {
        let rules = &inner.rules;
        lock(&inner.seen.texts).push(text.to_owned());
        let mut rooms = lock(&inner.rooms);
        let Some(room) = rooms.get_mut(name) else {
            return;
        };
        let Some(i) = idx(room, id) else { return };
        // `setWebSocketAutoResponse`: 文字の完全一致だけ。Durable Object を起こさない（done でも返す）。
        if text == rules.ping_text {
            if !inner.knobs.no_pong.load(Ordering::Acquire) {
                send_text(&room[i], &rules.pong_text);
            }
            return;
        }
        if room[i].done {
            return;
        }
        if text.len() > rules.message_max_bytes {
            return fail(inner, room, id, "too_large");
        }
        let parsed = [("offer", Side::Offer), ("answer", Side::Answer)]
            .into_iter()
            .find_map(|(t, side)| {
                text.strip_prefix(&format!(r#"{{"t":"{t}","code":""#))
                    .and_then(|r| r.strip_suffix(r#""}"#))
                    .filter(|c| !c.contains(['"', '\\']))
                    .map(|c| (side, c.to_owned()))
            });
        let Some((kind, code)) = parsed else {
            return fail(inner, room, id, "bad_message");
        };
        if let Some(bad) = rules.check_code(&code) {
            return fail(inner, room, id, bad);
        }
        let now = Instant::now();
        match kind {
            Side::Offer => {
                if room[i].side != Side::Offer || room[i].code.is_some() {
                    return fail(inner, room, id, "bad_message");
                }
                room[i].code = Some(code);
                try_match(inner, room, now);
            }
            Side::Answer => {
                if room[i].side != Side::Answer || !room[i].got_peer {
                    return fail(inner, room, id, "bad_message");
                }
                let Some(h) = room.iter().position(|m| !m.done && m.side == Side::Offer) else {
                    return fail(inner, room, id, "peer_left");
                };
                send_text(&room[h], &format!(r#"{{"t":"peer","code":"{code}"}}"#));
                close_with(inner, &mut room[h], "done", false);
                close_with(inner, &mut room[i], "done", false);
            }
        }
    }

    /// room.ts の `tryMatch`。
    fn try_match(inner: &Inner, room: &mut [Member], now: Instant) {
        let h = room.iter().position(|m| !m.done && m.side == Side::Offer);
        let g = room.iter().position(|m| !m.done && m.side == Side::Answer);
        let (Some(h), Some(g)) = (h, g) else { return };
        let Some(code) = room[h].code.clone() else {
            return;
        };
        if room[g].got_peer {
            return;
        }
        let a = ttl(&inner.knobs.answer_ttl_ms, inner.rules.answer_ttl_ms);
        let by_answer = now + Duration::from_millis(a);
        let (deadline, unix) = if by_answer < room[h].deadline {
            (by_answer, unix_ms() + a)
        } else {
            (room[h].deadline, room[h].deadline_unix_ms)
        };
        for k in [h, g] {
            room[k].deadline = deadline;
            room[k].deadline_unix_ms = unix;
        }
        room[g].got_peer = true;
        send_text(&room[g], &format!(r#"{{"t":"peer","code":"{code}"}}"#));
        send_text(
            &room[h],
            &format!(r#"{{"t":"matched","expires_at":{}}}"#, unix / 1000),
        );
    }

    /// room.ts の `expireIfDue`。
    fn expire_if_due(inner: &Inner, room: &mut [Member], now: Instant) -> bool {
        let Some(d) = room.iter().filter(|m| !m.done).map(|m| m.deadline).min() else {
            return false;
        };
        if now < d {
            return false;
        }
        for m in room.iter_mut().filter(|m| !m.done) {
            close_with(inner, m, "expired", true);
        }
        true
    }

    /// room.ts の `fail`。
    fn fail(inner: &Inner, room: &mut [Member], id: usize, name: &str) {
        let Some(i) = idx(room, id) else { return };
        close_with(inner, &mut room[i], name, true);
        after_leave(inner, room, i);
    }

    /// room.ts の `gone`。
    fn gone(inner: &Inner, name: &str, id: usize) {
        let mut rooms = lock(&inner.rooms);
        let Some(room) = rooms.get_mut(name) else {
            return;
        };
        let Some(i) = idx(room, id) else { return };
        if room[i].done {
            return;
        }
        room[i].done = true;
        after_leave(inner, room, i);
    }

    /// room.ts の `afterLeave`。
    fn after_leave(inner: &Inner, room: &mut [Member], i: usize) {
        let (side, got_peer) = (room[i].side, room[i].got_peer);
        for (j, other) in room.iter_mut().enumerate() {
            if j == i || other.done {
                continue;
            }
            if side == Side::Offer || got_peer {
                close_with(inner, other, "peer_left", true);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn accept_matches_the_rfc6455_example() {
            let got = base64(
                &crate::room_win::sha1(format!("dGhlIHNhbXBsZSBub25jZQ=={WS_GUID}").as_bytes())
                    .unwrap(),
            );
            assert_eq!(got, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
            assert_eq!(base64(b"f"), "Zg==");
            assert_eq!(base64(b"fo"), "Zm8=");
            assert_eq!(base64(b"foo"), "Zm9v");
        }

        #[test]
        fn rules_are_read_from_the_fixture() {
            let r = Rules::load();
            assert_eq!(r.path_prefix, "/v0/rooms/");
            assert_eq!(r.random_path_prefix, "/v0/random/");
            assert_eq!(r.random_room_prefix, "q.");
            assert_eq!(r.random_room_hex_len, 30);
            assert_eq!(r.random_queue_ttl_ms, 600_000);
            assert_eq!(r.random_room_ttl_ms, 60_000);
            for (seg, want) in [
                ("0", Some(0)),
                ("403920", Some(403_920)),
                ("4294967295", Some(u32::MAX)),
                ("4294967296", None),
                ("", None),
                ("00", None),
                ("01", None),
                ("-1", None),
                ("1.5", None),
                ("abc", None),
                ("12345678901", None),
            ] {
                assert_eq!(Rules::build_from_segment(seg), want, "{seg:?}");
            }
            assert!(r.is_random_room(&format!("q.{}", "ab01".repeat(7) + "cd")));
            for not in ["q.", "q.xyz", "room", &format!("w.{}", "a".repeat(30))] {
                assert!(!r.is_random_room(not), "{not}");
            }
            assert_eq!(r.close("done"), 1000);
            assert_eq!(r.close("bad_message"), 4400);
            assert_eq!(r.close("expired"), 4408);
            assert_eq!(r.close("peer_left"), 4410);
            assert_eq!(r.close("too_large"), 4413);
            assert_eq!(r.close("bad_code"), 4422);
            assert_eq!(r.close("list_full"), 4429);
            assert_eq!(r.closes.len(), 7);
            assert_eq!(r.room_ttl_ms, 600_000);
            assert_eq!(r.answer_ttl_ms, 60_000);
            assert_eq!(r.ping_text, r#"{"t":"ping"}"#);
            assert_eq!(r.normalize("Room-1_X").as_deref(), Some("room-1_x"));
            assert_eq!(r.normalize(&"a".repeat(33)), None);
            assert_eq!(r.normalize("%E8%8A%B1-A").as_deref(), Some("花-a"));
            assert_eq!(r.normalize("%e8%8a%b1-a").as_deref(), Some("花-a"));
            assert_eq!(r.normalize("%20%E3%80%80x%20").as_deref(), Some("x"));
            assert_eq!(
                r.normalize(&"%E9%83%A8".repeat(32)).as_deref(),
                Some(&*"部".repeat(32))
            );
            assert_eq!(r.normalize(&"%E9%83%A8".repeat(33)), None);
            assert_eq!(
                r.normalize(&"%F0%A0%AE%B7".repeat(32)).map(|k| k.len()),
                Some(128)
            );
            for bad in [
                "%",
                "%E",
                "%ZZ",
                "%E8%8A",
                "%ED%A0%80",
                "%C0%AF",
                "%20",
                "%E3%80%80",
                "a%2Fb",
                "a%00b",
                "a%7Fb",
                "a%C2%85b",
                "..",
            ] {
                assert_eq!(r.normalize(bad), None, "{bad}");
            }
            assert_eq!(
                r.normalize(&format!("{}a", "%F0%A0%AE%B7".repeat(32))),
                None,
                "129 B"
            );
            assert_eq!(r.check_code(""), Some("bad_code"));
            assert_eq!(r.check_code(&"A".repeat(1025)), Some("too_large"));
            assert_eq!(r.check_code("Ab-_9"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    //! 部品の検査（毎回の回）は `Base` と役割のコピーだけ。組み上がりの検査（loopback の偽サーバー `fake` と、本物の WinHTTP・
    //! 本物のスレッド）は実時間で走るので `#[ignore]` で、全件の回が `--ignored` で走らせる。外へは繋がない（`127.0.0.1` と `Proxy::Direct`）。
    use super::fake::FakeServer;
    use super::*;
    use higanr_core::room::{
        Failure, PING_INTERVAL_MS, PONG_TIMEOUT_MS, ROOM_GREETING_TIMEOUT_MS, Why,
    };
    use std::net::TcpListener;

    // ---- 部品 ----

    #[test]
    fn base_is_split_into_scheme_host_and_port() {
        let b = |secure, host: &str, port| Base {
            secure,
            host: host.into(),
            port,
        };
        assert_eq!(
            Base::parse("wss://room.example.workers.dev"),
            Ok(b(true, "room.example.workers.dev", 443))
        );
        assert_eq!(
            Base::parse("WSS://Room.example.dev/"),
            Ok(b(true, "Room.example.dev", 443))
        );
        assert_eq!(
            Base::parse("ws://127.0.0.1:8787"),
            Ok(b(false, "127.0.0.1", 8787))
        );
        assert_eq!(Base::parse("ws://localhost"), Ok(b(false, "localhost", 80)));
        assert_eq!(Base::parse("wss://a.dev:8443/"), Ok(b(true, "a.dev", 8443)));
        for (bad, why) in [
            ("https://a.dev", BaseError::Scheme),
            ("a.dev", BaseError::Scheme),
            ("ws:/a.dev", BaseError::Scheme),
            ("", BaseError::Scheme),
            ("wss://", BaseError::Host),
            ("wss://:443", BaseError::Host),
            ("wss://a b.dev", BaseError::Host),
            ("wss://[::1]:80", BaseError::Host),
            ("wss://a:b:80", BaseError::Host),
            ("wss://a.dev:", BaseError::Port),
            ("wss://a.dev:0", BaseError::Port),
            ("wss://a.dev:65536", BaseError::Port),
            ("wss://a.dev:+80", BaseError::Port),
            ("wss://a.dev/v0", BaseError::Path),
            ("wss://a.dev//", BaseError::Path),
            ("wss://a.dev?x=1", BaseError::Path),
        ] {
            assert_eq!(Base::parse(bad), Err(why), "{bad:?}");
        }
        let ep = b(false, "127.0.0.1", 1).endpoint(RoomName::parse("Room-A").unwrap().path());
        assert_eq!(ep.path, "/v0/rooms/Room-A");
        let ep =
            b(false, "127.0.0.1", 1).endpoint(RoomName::parse_server_rules("花 A").unwrap().path());
        assert_eq!(ep.path, "/v0/rooms/%E8%8A%B1%20A");
        let ep = b(false, "127.0.0.1", 1).endpoint(random_path(403_920, None));
        assert_eq!(ep.path, "/v0/random/403920");
    }

    #[test]
    fn roles_map_to_code_sides() {
        assert_eq!(code_side(Role::Offer), CodeSide::Offer);
        assert_eq!(code_side(Role::Answer), CodeSide::Answer);
    }

    #[test]
    fn product_timeouts_follow_the_constants() {
        let t = product_timeouts();
        assert_eq!(
            [t.resolve_ms, t.connect_ms, t.send_ms, t.response_ms],
            [OPEN_TIMEOUT_MS; 4]
        );
        assert_eq!(t.close_wait_ms, CLOSE_WAIT_MS);
        assert_eq!(
            RoomOptions::product(Base::parse("wss://a.dev").unwrap()).proxy,
            Proxy::Automatic
        );
    }

    // ---- 組み上がり: 道具 ----

    fn test_timeouts() -> Timeouts {
        Timeouts {
            resolve_ms: 2_000,
            connect_ms: 2_000,
            send_ms: 2_000,
            response_ms: 2_000,
            close_wait_ms: 500,
        }
    }

    fn opts_for(base: &str) -> RoomOptions {
        RoomOptions {
            base: Base::parse(base).unwrap(),
            proxy: Proxy::Direct,
            timeouts: test_timeouts(),
        }
    }

    /// 検査のルーム名はサーバーの決まり（`parse_server_rules`）で確かめる。dll の本番（`parse`）は ASCII の英数字と `-` `_` だけだが、
    /// サーバーは日本語も受け付けたままなので、その受け入れ（符号化・400）はこの入口で見る。
    fn start(f: &FakeServer, room: &str) -> RoomConn {
        RoomConn::start(
            &opts_for(&f.base()),
            &RoomName::parse_server_rules(room).unwrap(),
        )
        .unwrap()
    }

    /// `poll` を回して出来事を集める（時計は `offset_ms` だけ進めて渡す）。`stop` が真になったら返す。`limit` を過ぎたら
    /// 集めた出来事を出して落ちる。
    fn pump(
        c: &mut RoomConn,
        offset_ms: u64,
        limit: Duration,
        stop: impl Fn(&[Event]) -> bool,
    ) -> Vec<Event> {
        let t0 = Instant::now();
        let mut evs = Vec::new();
        loop {
            let mut now = c.now();
            now.ms += offset_ms;
            evs.extend(c.poll_at(now));
            if stop(&evs) {
                return evs;
            }
            assert!(
                t0.elapsed() < limit,
                "{limit:?} の内に終わらない。出来事 {evs:?}・状態 {:?}",
                c.state()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn failure_of(evs: &[Event]) -> Option<Failure> {
        evs.iter().find_map(|e| match e {
            Event::Failed(f) => Some(*f),
            _ => None,
        })
    }

    fn has_failed(evs: &[Event]) -> bool {
        failure_of(evs).is_some()
    }

    fn wait_failure(c: &mut RoomConn) -> Failure {
        let evs = pump(c, 0, Duration::from_secs(5), has_failed);
        failure_of(&evs).unwrap()
    }

    fn joined_as(evs: &[Event], role: Role) -> bool {
        evs.iter()
            .any(|e| matches!(e, Event::Joined { role: r, .. } if *r == role))
    }

    fn peer_code(evs: &[Event]) -> Option<String> {
        evs.iter().find_map(|e| match e {
            Event::PeerCode(c) => Some(c.clone()),
            _ => None,
        })
    }

    fn wait_state(c: &mut RoomConn, want: State) -> Vec<Event> {
        let t0 = Instant::now();
        let mut evs = Vec::new();
        while c.state() != want {
            evs.extend(c.poll());
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "{want:?} にならない。状態 {:?}・出来事 {evs:?}",
                c.state()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        evs
    }

    fn assert_joins<M: Machine>(c: &mut Conn<M>, limit: Duration) -> Duration {
        let t0 = Instant::now();
        assert!(c.join_within(limit), "{limit:?} の内にスレッドが終わらない");
        t0.elapsed()
    }

    // 始める側・返す側のコード（base64url の文字。実在のものではない合成の文字）。
    const OFFER_CODE: &str = "HRo1-OfferSide_Code";
    const ANSWER_CODE: &str = "HRa1-AnswerSide_Code";

    // ---- 組み上がり: 手順が通る ----

    /// 2 人がコードを渡し合って両方 Done になるまで。`offer_first` は「始める側が offer を出してから返す側が来る」か
    /// 「返す側が先に来て待つ」か。
    fn trade(offer_first: bool) {
        let f = FakeServer::start();
        let mut a = start(&f, "Trade-Room");
        let evs = wait_state(&mut a, State::NeedOwnCode);
        assert!(
            joined_as(&evs, Role::Offer),
            "先に来た方が始める側: {evs:?}"
        );
        if offer_first {
            a.submit_code(OFFER_CODE).unwrap();
            wait_state(&mut a, State::AwaitMatched);
        }
        let mut b = start(&f, "trade-room");
        let evs = pump(&mut b, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Answer)
        });
        assert!(peer_code(&evs).is_none() || offer_first);
        if !offer_first {
            assert_eq!(b.state(), State::AwaitPeerCode);
            a.submit_code(OFFER_CODE).unwrap();
        }
        let evs_b = wait_state(&mut b, State::NeedOwnCode);
        let all_b: Vec<Event> = evs.into_iter().chain(evs_b).collect();
        assert_eq!(peer_code(&all_b).as_deref(), Some(OFFER_CODE));
        assert_eq!(
            b.submit_code(ANSWER_CODE),
            Ok(()),
            "返す側の Ok = answer を渡し終えた（§5-2 の (2) で P2P を始める時点）"
        );
        let evs_a = pump(&mut a, 0, Duration::from_secs(5), |e| {
            e.contains(&Event::Done)
        });
        assert_eq!(peer_code(&evs_a).as_deref(), Some(ANSWER_CODE));
        assert!(
            evs_a.iter().any(|e| matches!(e, Event::Matched { .. })),
            "始める側は matched を受ける: {evs_a:?}"
        );
        let evs_b = pump(&mut b, 0, Duration::from_secs(5), |e| {
            e.contains(&Event::Done)
        });
        assert!(!has_failed(&evs_a) && !has_failed(&evs_b));
        assert!(a.is_finished() && b.is_finished());
        assert_joins(&mut a, Duration::from_secs(3));
        assert_joins(&mut b, Duration::from_secs(3));
        let offer = format!(r#"{{"t":"offer","code":"{OFFER_CODE}"}}"#);
        let answer = format!(r#"{{"t":"answer","code":"{ANSWER_CODE}"}}"#);
        assert_eq!(f.texts_matching(|t| t == offer), 1);
        assert_eq!(f.texts_matching(|t| t == answer), 1);
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn two_clients_trade_codes_and_finish() {
        trade(true);
        trade(false);
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn japanese_room_names_reach_the_server_as_encoded() {
        let f = FakeServer::start();
        let name_a = "花映塚 Room #1?";
        let name_b = "花映塚 room #1?";
        let mut a = start(&f, name_a);
        let evs = wait_state(&mut a, State::NeedOwnCode);
        assert!(joined_as(&evs, Role::Offer), "{evs:?}");
        let mut b = start(&f, name_b);
        let evs = pump(&mut b, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Answer)
        });
        assert!(
            joined_as(&evs, Role::Answer),
            "大小だけ違う日本語の名は同じルーム"
        );
        // 届いたパスは、こちらが作ったパスと 1 文字も違わない。
        let want: Vec<String> = [name_a, name_b]
            .iter()
            .map(|n| RoomName::parse_server_rules(n).unwrap().path())
            .collect();
        assert_eq!(
            want[0],
            "/v0/rooms/%E8%8A%B1%E6%98%A0%E5%A1%9A%20Room%20%231%3F"
        );
        assert_eq!(*f.seen.paths.lock().unwrap(), want);
        a.cancel();
        b.cancel();
        assert_joins(&mut a, Duration::from_secs(3));
        assert_joins(&mut b, Duration::from_secs(3));

        // こちらは通し（ASCII でないので長さを数えない）、サーバーが 400 で断る名。
        let long = "部".repeat(33);
        assert!(RoomName::parse_server_rules(&long).is_ok());
        let mut c = start(&f, &long);
        assert_eq!(wait_failure(&mut c), Failure::BadRoomName);
        assert_joins(&mut c, Duration::from_secs(3));
    }

    #[test]
    fn client_id_is_drawn_once_per_process() {
        let a = client_id().expect("OS の乱数が引ける");
        assert_ne!(a, [0u8; CLIENT_ID_BYTES]);
        assert_eq!(client_id(), Some(a));
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn every_join_carries_the_same_client_id() {
        let f = FakeServer::start();
        // ルームを替えても、入り直し（同じルームへ 2 度目）でも同じ値。
        let mut a = start(&f, "cid-a");
        wait_state(&mut a, State::NeedOwnCode);
        a.cancel();
        assert_joins(&mut a, Duration::from_secs(3));
        let mut b = start(&f, "cid-a");
        wait_state(&mut b, State::NeedOwnCode);
        let mut c = start(&f, "cid-b");
        wait_state(&mut c, State::NeedOwnCode);
        let queries = f.seen.queries.lock().unwrap().clone();
        let ids: Vec<&str> = queries
            .iter()
            .map(|q| {
                let h = q.strip_prefix("cid=").expect("問いは cid だけ");
                assert!(
                    h.len() == 2 * CLIENT_ID_BYTES
                        && h.bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                    "16 進小文字 32 文字: {h}"
                );
                h
            })
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
        b.cancel();
        c.cancel();
        assert_joins(&mut b, Duration::from_secs(3));
        assert_joins(&mut c, Duration::from_secs(3));
    }

    // ---- 組み上がり: 断り ----

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn refusals_become_failures() {
        let html = "<html><body>error code: 1027</body></html>";
        let cases: Vec<(u16, String, Failure)> = vec![
            (
                404,
                r#"{"error":"not_found"}"#.into(),
                Failure::Unresponsive(Why::HttpStatus(404)),
            ),
            (
                426,
                r#"{"error":"upgrade_required"}"#.into(),
                Failure::Unresponsive(Why::HttpStatus(426)),
            ),
            (
                500,
                String::new(),
                Failure::Unresponsive(Why::HttpStatus(500)),
            ),
            (
                503,
                html.into(),
                Failure::Unresponsive(Why::HttpStatus(503)),
            ),
            (
                409,
                html.into(),
                Failure::Unresponsive(Why::NotOurBody(409)),
            ),
            (
                429,
                html.into(),
                Failure::Unresponsive(Why::NotOurBody(429)),
            ),
            (
                400,
                r#"{"error":"bad_room_name"}"#.into(),
                Failure::BadRoomName,
            ),
            (
                429,
                r#"{"error":"rate_limited","retry_after":7}"#.into(),
                Failure::RateLimited {
                    retry_after_secs: 7,
                },
            ),
        ];
        let f = FakeServer::start();
        for (status, body, want) in cases {
            *f.knobs.forced.lock().unwrap() = Some((status, body.clone()));
            let mut c = start(&f, "refused");
            assert_eq!(wait_failure(&mut c), want, "{status} {body}");
            assert_joins(&mut c, Duration::from_secs(3));
        }
        *f.knobs.forced.lock().unwrap() = None;

        let mut a = start(&f, "full");
        wait_state(&mut a, State::NeedOwnCode);
        let mut b = start(&f, "full");
        wait_state(&mut b, State::AwaitPeerCode);
        let mut c = start(&f, "full");
        assert_eq!(wait_failure(&mut c), Failure::RoomFull);
        assert!(!a.is_finished() && !b.is_finished());

        // 回数の歯止め: 1 回だけ通し、次は 429 と見本の待ちの秒（index.ts と同じ本文と Retry-After ヘッダー）。
        let f2 = FakeServer::start();
        f2.knobs.limit.store(1, Ordering::Release);
        let mut d = start(&f2, "limited");
        wait_state(&mut d, State::NeedOwnCode);
        let mut e = start(&f2, "limited-2");
        assert_eq!(
            wait_failure(&mut e),
            Failure::RateLimited {
                retry_after_secs: f2.rules().retry_after_secs as u32
            }
        );

        // 誰も受けていない番号: 繋がらない（WinHTTP の誤り）→ 応答しません。
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut g = RoomConn::start(
            &opts_for(&format!("ws://127.0.0.1:{port}")),
            &RoomName::parse("nobody").unwrap(),
        )
        .unwrap();
        assert_eq!(wait_failure(&mut g), Failure::Unresponsive(Why::NoAnswer));
        assert!(
            g.take_notes()
                .iter()
                .any(|n| matches!(n, TransportNote::OpenFailed { .. })),
            "繋がらなかった段階と番号を記録に残す"
        );
    }

    // ---- 組み上がり: 手順の途中の失敗 ----

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn no_room_after_upgrade_is_unresponsive() {
        let f = FakeServer::start();
        f.knobs.no_room.store(true, Ordering::Release);
        let mut c = start(&f, "silent");
        wait_state(&mut c, State::AwaitRoom);
        let evs = run_for(
            &mut c,
            ROOM_GREETING_TIMEOUT_MS - 2_000,
            Duration::from_millis(200),
        );
        assert!(!has_failed(&evs), "10 秒の手前では切らない: {evs:?}");
        let evs = pump(
            &mut c,
            ROOM_GREETING_TIMEOUT_MS,
            Duration::from_secs(2),
            has_failed,
        );
        assert_eq!(failure_of(&evs), Some(Failure::Unresponsive(Why::NoRoom)));
        assert_joins(&mut c, Duration::from_secs(3));
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_leaving_peer_is_told() {
        // サーバーは閉じる前に error の文を送る。文を省いて閉じる番号だけでも同じ結果になること（番号の受け渡しを文が隠さないこと）を両方で見る。
        for no_error_text in [false, true] {
            let f = FakeServer::start();
            f.knobs
                .no_error_text
                .store(no_error_text, Ordering::Release);
            let mut a = start(&f, "leave-1");
            wait_state(&mut a, State::NeedOwnCode);
            a.submit_code(OFFER_CODE).unwrap();
            let mut b = start(&f, "leave-1");
            wait_state(&mut b, State::NeedOwnCode);
            b.cancel();
            assert!(b.poll().is_empty(), "取りやめた後は何も返さない");
            assert_eq!(wait_failure(&mut a), Failure::PeerLeft, "{no_error_text}");
            assert_joins(&mut b, Duration::from_secs(3));
            assert_joins(&mut a, Duration::from_secs(3));

            let mut a = start(&f, "leave-2");
            wait_state(&mut a, State::NeedOwnCode);
            let mut b = start(&f, "leave-2");
            wait_state(&mut b, State::AwaitPeerCode);
            drop(a);
            assert_eq!(wait_failure(&mut b), Failure::PeerLeft, "{no_error_text}");
        }
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn expiry_is_told() {
        for no_error_text in [false, true] {
            let f = FakeServer::start();
            f.knobs.room_ttl_ms.store(400, Ordering::Release);
            f.knobs
                .no_error_text
                .store(no_error_text, Ordering::Release);
            let mut a = start(&f, "expire-room");
            wait_state(&mut a, State::NeedOwnCode);
            assert_eq!(wait_failure(&mut a), Failure::Expired, "{no_error_text}");
            assert_joins(&mut a, Duration::from_secs(3));
        }

        let f = FakeServer::start();
        f.knobs.answer_ttl_ms.store(400, Ordering::Release);
        let mut a = start(&f, "expire-answer");
        wait_state(&mut a, State::NeedOwnCode);
        a.submit_code(OFFER_CODE).unwrap();
        let mut b = start(&f, "expire-answer");
        wait_state(&mut b, State::NeedOwnCode);
        assert_eq!(wait_failure(&mut b), Failure::Expired);
        assert_eq!(wait_failure(&mut a), Failure::Expired);

        // サーバーが閉じた直後（見張りがまだ読んでいない間）にコードを出す: 送らずに NotSent を返し、失敗は「切れた」でなく
        // 期限（届いていた番号の方）で返す。
        let f = FakeServer::start();
        f.knobs.room_ttl_ms.store(300, Ordering::Release);
        let mut a = start(&f, "expire-race");
        wait_state(&mut a, State::NeedOwnCode);
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(a.submit_code(OFFER_CODE), Err(SubmitRefused::NotSent));
        assert_eq!(wait_failure(&mut a), Failure::Expired);
        assert_eq!(
            f.texts_matching(|t| t.contains("offer")),
            0,
            "閉じた後には送らない"
        );
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn silence_after_ping_is_unresponsive() {
        let is_ping = |t: &str| t == higanr_core::room::PING_TEXT;
        let f = FakeServer::start();
        let mut c = start(&f, "alive");
        wait_state(&mut c, State::NeedOwnCode);
        let next = c.next_poll_in().expect("ping の時刻がある");
        assert!(
            next <= Duration::from_millis(PING_INTERVAL_MS) && next > Duration::from_secs(25),
            "次に呼ぶのは ping の時刻: {next:?}"
        );
        run_for(&mut c, PING_INTERVAL_MS, Duration::from_millis(300));
        assert_eq!(f.texts_matching(is_ping), 1, "30 秒で ping を 1 通");
        let evs = run_for(
            &mut c,
            PING_INTERVAL_MS + PONG_TIMEOUT_MS,
            Duration::from_millis(300),
        );
        assert!(!has_failed(&evs), "pong が来ていれば切らない: {evs:?}");
        assert_eq!(f.texts_matching(is_ping), 2);

        let f = FakeServer::start();
        f.knobs.no_pong.store(true, Ordering::Release);
        let mut c = start(&f, "no-pong");
        wait_state(&mut c, State::NeedOwnCode);
        run_for(&mut c, PING_INTERVAL_MS, Duration::from_millis(300));
        assert_eq!(
            f.texts_matching(is_ping),
            1,
            "ping は送っている（届いている）"
        );
        let evs = pump(
            &mut c,
            PING_INTERVAL_MS + PONG_TIMEOUT_MS,
            Duration::from_secs(2),
            has_failed,
        );
        assert_eq!(failure_of(&evs), Some(Failure::Unresponsive(Why::NoPong)));
        assert_joins(&mut c, Duration::from_secs(3));
    }

    fn run_for(c: &mut RoomConn, offset_ms: u64, dur: Duration) -> Vec<Event> {
        let t0 = Instant::now();
        let mut evs = Vec::new();
        while t0.elapsed() < dur {
            let mut now = c.now();
            now.ms += offset_ms;
            evs.extend(c.poll_at(now));
            std::thread::sleep(Duration::from_millis(2));
        }
        evs
    }

    // ---- 組み上がり: 取りやめ ----

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn cancel_stops_the_receive_thread() {
        let f = FakeServer::start();
        let mut c = start(&f, "cancel-1");
        wait_state(&mut c, State::NeedOwnCode);
        let t0 = Instant::now();
        c.cancel();
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "cancel は待たない"
        );
        assert!(c.is_finished());
        let took = assert_joins(&mut c, Duration::from_secs(2));
        assert!(
            took < Duration::from_millis(500),
            "閉じ返すなら待ちは短い: {took:?}"
        );
        assert_eq!(
            *f.seen.client_closes.lock().unwrap(),
            vec![Some(CLOSE_NORMAL)]
        );

        // 何も答えないサーバー: 受信はその場で解け、閉じるスレッドは閉じる待ち（検査は 500 ms）で終わる。
        let f = FakeServer::start();
        let mut c = start(&f, "cancel-2");
        wait_state(&mut c, State::NeedOwnCode);
        f.knobs.deaf.store(true, Ordering::Release);
        let t0 = Instant::now();
        c.cancel();
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "cancel は待たない"
        );
        let took = assert_joins(&mut c, Duration::from_secs(3));
        assert!(
            took >= Duration::from_millis(400),
            "閉じる待ちの分は止まる: {took:?}"
        );

        // 繋ぐ途中（TCP は受けるが HTTP に答えない）: 応答の先頭の待ちの上限で終わる（取りやめでは解けない。見張りは待たない）。
        // 上限は 300 ms に置き、WinHTTP の見回りの間隔（約 4 秒）の分を見込む。相手が切る 9 秒より前。
        let (port, hold) = raw_server(None, Duration::from_secs(9));
        let t0 = Instant::now();
        let mut o = opts_for(&format!("ws://127.0.0.1:{port}"));
        o.timeouts.response_ms = 300;
        let mut c = RoomConn::start(&o, &RoomName::parse("hang").unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        c.cancel();
        assert!(c.poll().is_empty());
        assert_joins(&mut c, Duration::from_secs(8));
        let took = t0.elapsed();
        assert!(
            took < Duration::from_millis(6_500),
            "応答のヘッダーの待ちで終わる: {took:?}"
        );
        hold.join().unwrap();
    }

    /// 1 本だけ受ける生の TCP の相手。要求の先頭を読み、`reply` があれば書き、`hold` の間持ってから切る。
    fn raw_server(reply: Option<&'static [u8]>, hold: Duration) -> (u16, JoinHandle<()>) {
        use std::io::{Read, Write};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && s.read(&mut b).is_ok_and(|n| n == 1) {
                head.push(b[0]);
            }
            if let Some(r) = reply {
                let _ = s.write_all(r);
            }
            std::thread::sleep(hold);
            drop(s);
        });
        (port, h)
    }

    // ---- WinHTTP の振る舞い ----

    fn ep(f: &FakeServer, room: &str) -> Endpoint {
        Base::parse(&f.base())
            .unwrap()
            .endpoint(RoomName::parse(room).unwrap().path())
    }

    fn open_ws(f: &FakeServer, room: &str, t: Timeouts) -> Arc<WebSocket> {
        let ws = WebSocket::open(&ep(f, room), Proxy::Direct, t, 4096).expect("open");
        assert!(matches!(ws.receive(4096), Ok(Received::Text(r)) if r.contains(r#""t":"room""#)));
        Arc::new(ws)
    }

    fn receive_in_thread(ws: &Arc<WebSocket>) -> JoinHandle<(Result<Received, u32>, Instant)> {
        let w = Arc::clone(ws);
        std::thread::spawn(move || {
            let r = w.receive(4096);
            (r, Instant::now())
        })
    }

    fn raw_endpoint(port: u16) -> Endpoint {
        Endpoint {
            secure: false,
            host: "127.0.0.1".into(),
            port,
            path: "/v0/rooms/x".into(),
        }
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn winhttp_facts_receive_timeout_does_not_bound_ws_receive() {
        // WinHTTP の時間切れは粗い間隔（約 4 秒）で見回るので、300 ms を置いても切れるのは最大 4 秒余り後。相手が切る（8 秒）より前に切れたこと・
        // WebSocket の受信がその間隔を越えても戻らないことを見る。3 つは同時に走らせる（待ちを重ねない）。
        let t = Timeouts {
            response_ms: 300,
            ..test_timeouts()
        };
        let hold = Duration::from_secs(8);
        let within = Duration::from_millis(6_500);

        let (port1, hold1) = raw_server(None, hold);
        let head = std::thread::spawn(move || {
            let t0 = Instant::now();
            let r = WebSocket::open(&raw_endpoint(port1), Proxy::Direct, t, 64);
            (r.err(), t0.elapsed())
        });
        let (port2, hold2) = raw_server(
            Some(b"HTTP/1.1 500 X\r\nContent-Length: 100\r\n\r\n0123456789"),
            hold,
        );
        let body = std::thread::spawn(move || {
            let t0 = Instant::now();
            let r = WebSocket::open(&raw_endpoint(port2), Proxy::Direct, t, 4096);
            (r.err(), t0.elapsed())
        });
        // 効かない側: 同じ設定で Upgrade した WebSocket の受信は、文が来なければ間隔を越えても戻らない。
        let f = FakeServer::start();
        let ws = open_ws(&f, "rt", t);
        assert_eq!(ws.try_set_receive_timeout(300), Err(12018));
        let h = receive_in_thread(&ws);
        let ws_started = Instant::now();

        let (r, took) = head.join().unwrap();
        assert_eq!(
            r,
            Some(OpenError::Transport {
                step: Step::ReceiveResponse,
                code: 12002
            }),
            "{took:?}"
        );
        assert!(took < within, "応答のヘッダーの待ちが効く: {took:?}");
        let (r, took) = body.join().unwrap();
        assert_eq!(
            r,
            Some(OpenError::Refused {
                status: 500,
                body: "0123456789".into()
            })
        );
        assert!(took < within, "本文の受信の上限が効く: {took:?}");
        let wait = Duration::from_millis(5_500);
        if let Some(rest) = wait.checked_sub(ws_started.elapsed()) {
            std::thread::sleep(rest);
        }
        assert!(
            !h.is_finished(),
            "WebSocket の受信が上限で戻った（WinHTTP の振る舞いが変わった）"
        );
        let _ = ws.close(CLOSE_NORMAL);
        let (r, _) = h.join().unwrap();
        assert_eq!(r, Err(12017));
        hold1.join().unwrap();
        hold2.join().unwrap();
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn winhttp_facts_send_and_shutdown_while_receiving() {
        let f = FakeServer::start();
        let ws = open_ws(&f, "sr", test_timeouts());
        let w = Arc::clone(&ws);
        let h = std::thread::spawn(move || (w.receive(4096), w.receive(4096), w.close_status()));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!h.is_finished(), "受信は止まっている");
        let t0 = Instant::now();
        assert_eq!(ws.send_text(higanr_core::room::PING_TEXT), Ok(()));
        assert!(t0.elapsed() < Duration::from_millis(500));
        std::thread::sleep(Duration::from_millis(100));
        let t0 = Instant::now();
        assert_eq!(ws.shutdown(CLOSE_NORMAL), Ok(()));
        assert!(t0.elapsed() < Duration::from_millis(500));
        let (r1, r2, status) = h.join().unwrap();
        assert_eq!(
            r1,
            Ok(Received::Text(f.rules().pong_text.clone())),
            "送った ping の答え"
        );
        assert_eq!(r2, Ok(Received::Close));
        assert_eq!(status, 1000);
        assert_eq!(f.texts_matching(|t| t == f.rules().ping_text), 1);
        assert_eq!(*f.seen.client_closes.lock().unwrap(), vec![Some(1000)]);
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn winhttp_facts_close_unblocks_receive() {
        for deaf in [false, true] {
            let f = FakeServer::start();
            let t = Timeouts {
                close_wait_ms: 700,
                ..test_timeouts()
            };
            let ws = open_ws(&f, "cl", t);
            if deaf {
                f.knobs.deaf.store(true, Ordering::Release);
            }
            let h = receive_in_thread(&ws);
            std::thread::sleep(Duration::from_millis(200));
            assert!(!h.is_finished());
            let t0 = Instant::now();
            let c = ws.close(CLOSE_NORMAL);
            let close_took = t0.elapsed();
            let (r, at) = h.join().unwrap();
            let recv_took = at.duration_since(t0);
            assert_eq!(r, Err(12017), "deaf={deaf}");
            assert!(
                recv_took < Duration::from_millis(300),
                "deaf={deaf}: {recv_took:?}"
            );
            if deaf {
                assert_eq!(c, Err(12002));
                assert!(
                    close_took >= Duration::from_millis(600)
                        && close_took < Duration::from_millis(2000),
                    "閉じる待ちは ms: {close_took:?}"
                );
            } else {
                assert_eq!(c, Ok(()));
                assert!(close_took < Duration::from_millis(300), "{close_took:?}");
                assert_eq!(*f.seen.client_closes.lock().unwrap(), vec![Some(1000)]);
            }
        }
    }

    /// Wine の winhttp は WebSocket のハンドルの close の待ちの設定を 12009 で断る。それでも開けて、送受信も閉じる処理もできる。
    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn open_survives_a_refused_close_timeout() {
        const ERROR_WINHTTP_INVALID_OPTION: u32 = 12009;
        let f = FakeServer::start();
        let ws = WebSocket::open_with_close_timeout(
            &ep(&f, "wine"),
            Proxy::Direct,
            test_timeouts(),
            4096,
            |_, _| Err(ERROR_WINHTTP_INVALID_OPTION),
        )
        .expect("close の待ちを断られても開ける");
        assert_eq!(ws.close_timeout_error(), Some(ERROR_WINHTTP_INVALID_OPTION));
        assert!(matches!(ws.receive(4096), Ok(Received::Text(r)) if r.contains(r#""t":"room""#)));
        assert_eq!(ws.send_text(higanr_core::room::PING_TEXT), Ok(()));
        assert_eq!(
            ws.receive(4096),
            Ok(Received::Text(f.rules().pong_text.clone()))
        );
        assert_eq!(ws.close(CLOSE_NORMAL), Ok(()));

        // 通常の経路は置けて、印が付かない。
        let ws = open_ws(&f, "normal", test_timeouts());
        assert_eq!(ws.close_timeout_error(), None);
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn winhttp_facts_close_without_status_is_1005() {
        let f = FakeServer::start();
        f.knobs.close_without_status.store(true, Ordering::Release);
        let ws = open_ws(&f, "nostatus", test_timeouts());
        assert_eq!(ws.receive(4096), Ok(Received::Close));
        assert_eq!(ws.close_status(), crate::room_win::NO_STATUS);
        assert_eq!(crate::room_win::NO_STATUS, 1005);
        drop(ws);
        let mut c = start(&f, "nostatus-2");
        assert_eq!(
            wait_failure(&mut c),
            Failure::Unresponsive(Why::Closed(1005))
        );
    }

    // ---- ランダム対戦の待ち列 ----

    fn start_queue(f: &FakeServer, build: u32) -> QueueConn {
        QueueConn::start_queue(&opts_for(&f.base()), build).unwrap()
    }

    fn pump_queue(
        c: &mut QueueConn,
        offset_ms: u64,
        limit: Duration,
        stop: impl Fn(&[QueueEvent]) -> bool,
    ) -> Vec<QueueEvent> {
        let t0 = Instant::now();
        let mut evs = Vec::new();
        loop {
            let mut now = c.now();
            now.ms += offset_ms;
            evs.extend(c.poll_at(now));
            if stop(&evs) {
                return evs;
            }
            assert!(t0.elapsed() < limit, "{limit:?} の内に終わらない: {evs:?}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn queued(evs: &[QueueEvent]) -> bool {
        evs.iter().any(|e| matches!(e, QueueEvent::Queued { .. }))
    }

    fn paired(evs: &[QueueEvent]) -> Option<(String, bool)> {
        evs.iter().find_map(|e| match e {
            QueueEvent::Paired { room, first } => Some((room.name().as_str().to_owned(), *first)),
            _ => None,
        })
    }

    fn queue_failure(evs: &[QueueEvent]) -> Option<Failure> {
        evs.iter().find_map(|e| match e {
            QueueEvent::Failed(f) => Some(*f),
            _ => None,
        })
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn two_queue_connections_are_paired_with_one_assigned_room_and_finish() {
        let f = FakeServer::start();
        let mut a = start_queue(&f, 403_920);
        let evs = pump_queue(&mut a, 0, Duration::from_secs(5), queued);
        assert!(paired(&evs).is_none());
        let mut b = start_queue(&f, 403_920);
        let eb = pump_queue(&mut b, 0, Duration::from_secs(5), |e| paired(e).is_some());
        let ea = pump_queue(&mut a, 0, Duration::from_secs(5), |e| paired(e).is_some());
        let (room_a, first_a) = paired(&ea).unwrap();
        let (room_b, first_b) = paired(&eb).unwrap();
        assert_eq!(room_a, room_b, "同じルーム名");
        assert!(first_a && !first_b, "先に待ち列に入った A が先");
        assert!(
            room_a.starts_with("q.") && room_a.len() == 32,
            "{}",
            room_a.len()
        );
        assert!(a.is_finished() && b.is_finished());
        assert_eq!(*f.seen.assigned.lock().unwrap(), vec![room_a.clone()]);
        let paths = f.seen.paths.lock().unwrap().clone();
        assert_eq!(paths, vec!["/v0/random/403920"; 2]);
        // 終わった後に閉じる番号が来ても出来事は増えない。
        assert!(a.poll().is_empty() && b.poll().is_empty());
        assert_joins(&mut a, Duration::from_secs(3));
        assert_joins(&mut b, Duration::from_secs(3));
        // 割り当てられたルームには、今のルームの手順で入って先に着いた方が始める側になる。
        let name = RoomName::parse_server_rules(&room_a).unwrap();
        let mut ra = RoomConn::start(&opts_for(&f.base()), &name).unwrap();
        let evs = wait_state(&mut ra, State::NeedOwnCode);
        assert!(joined_as(&evs, Role::Offer), "{evs:?}");
        assert_eq!(
            f.seen.paths.lock().unwrap().last().unwrap(),
            &format!("/v0/rooms/{room_a}")
        );
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn queue_connections_with_other_build_numbers_stay_apart() {
        let f = FakeServer::start();
        let mut a = start_queue(&f, 7);
        let mut b = start_queue(&f, 8);
        pump_queue(&mut a, 0, Duration::from_secs(5), queued);
        pump_queue(&mut b, 0, Duration::from_secs(5), queued);
        std::thread::sleep(Duration::from_millis(250));
        assert!(a.poll().is_empty() && b.poll().is_empty());
        assert!(f.seen.assigned.lock().unwrap().is_empty());
        // 同じ番号の人が来れば、その人と組になる。
        let mut c = start_queue(&f, 8);
        pump_queue(&mut c, 0, Duration::from_secs(5), |e| paired(e).is_some());
        pump_queue(&mut b, 0, Duration::from_secs(5), |e| paired(e).is_some());
        assert!(a.poll().is_empty(), "7 の A は待ち続ける");
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_queue_connection_expires_and_a_cancelled_one_leaves_the_queue() {
        let f = FakeServer::start();
        f.knobs.random_queue_ttl_ms.store(200, Ordering::Release);
        let mut a = start_queue(&f, 1);
        let evs = pump_queue(&mut a, 0, Duration::from_secs(5), |e| {
            queue_failure(e).is_some()
        });
        assert_eq!(queue_failure(&evs), Some(Failure::Expired));
        assert_joins(&mut a, Duration::from_secs(3));
        // 取りやめた人は待ち列に残らない: 次に来た人は組にならない。
        f.knobs.random_queue_ttl_ms.store(0, Ordering::Release);
        let mut b = start_queue(&f, 2);
        pump_queue(&mut b, 0, Duration::from_secs(5), queued);
        let closes_before = f.seen.client_closes.lock().unwrap().len();
        b.cancel();
        assert!(b.poll().is_empty());
        assert_joins(&mut b, Duration::from_secs(3));
        let t0 = Instant::now();
        while f.seen.client_closes.lock().unwrap().len() == closes_before {
            assert!(
                t0.elapsed() < Duration::from_secs(3),
                "取りやめがサーバーへ届かない"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            f.seen.client_closes.lock().unwrap().last(),
            Some(&Some(1000))
        );
        let mut c = start_queue(&f, 2);
        pump_queue(&mut c, 0, Duration::from_secs(5), queued);
        std::thread::sleep(Duration::from_millis(250));
        assert!(c.poll().is_empty(), "取りやめた人と組にされた");
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn the_queue_shares_the_rate_limit_and_the_greeting_and_pong_timeouts_with_rooms() {
        let f = FakeServer::start();
        f.knobs.limit.store(1, Ordering::Release);
        let mut a = start_queue(&f, 1);
        pump_queue(&mut a, 0, Duration::from_secs(5), queued);
        let mut b = start_queue(&f, 1);
        let evs = pump_queue(&mut b, 0, Duration::from_secs(5), |e| {
            queue_failure(e).is_some()
        });
        assert_eq!(
            queue_failure(&evs),
            Some(Failure::RateLimited {
                retry_after_secs: f.rules().retry_after_secs as u32
            }),
            "ルームと同じ 1 本の数え"
        );
        // ルームへの接続も同じ数えに入る（待ち列の 1 回で上限）。
        let mut r = start(&f, "limit-shared");
        assert_eq!(
            wait_failure(&mut r),
            Failure::RateLimited {
                retry_after_secs: f.rules().retry_after_secs as u32
            }
        );
        // queue が来ない・pong が来ない。
        let f = FakeServer::start();
        f.knobs.no_room.store(true, Ordering::Release);
        let mut c = start_queue(&f, 1);
        // 101 を受けてから時計を進める（受ける前に進めると、期限が進めた時計から数え始まる）。
        let t0 = Instant::now();
        while f.seen.queue_upgrades.load(Ordering::Acquire) == 0
            || t0.elapsed() < Duration::from_millis(300)
        {
            assert!(c.poll().is_empty());
            assert!(t0.elapsed() < Duration::from_secs(5), "101 にならない");
            std::thread::sleep(Duration::from_millis(5));
        }
        let evs = pump_queue(
            &mut c,
            ROOM_GREETING_TIMEOUT_MS + 100,
            Duration::from_secs(5),
            |e| queue_failure(e).is_some(),
        );
        assert_eq!(
            queue_failure(&evs),
            Some(Failure::Unresponsive(Why::NoRoom))
        );
        let f = FakeServer::start();
        f.knobs.no_pong.store(true, Ordering::Release);
        let mut d = start_queue(&f, 1);
        pump_queue(&mut d, 0, Duration::from_secs(5), queued);
        let evs = pump_queue(
            &mut d,
            PING_INTERVAL_MS + 100,
            Duration::from_secs(5),
            |_| f.texts_matching(|t| t == f.rules().ping_text) >= 1,
        );
        assert!(queue_failure(&evs).is_none());
        let evs = pump_queue(
            &mut d,
            PING_INTERVAL_MS + PONG_TIMEOUT_MS + 200,
            Duration::from_secs(5),
            |e| queue_failure(e).is_some(),
        );
        assert_eq!(
            queue_failure(&evs),
            Some(Failure::Unresponsive(Why::NoPong))
        );
    }
}
