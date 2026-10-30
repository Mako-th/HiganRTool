//! ルームサーバーのクライアントの通信部分。受信だけの専用スレッド`higanr-room`と、監視のスレッドから呼ぶ入口を持つ。
//! 入口はルーム用の[`RoomConn`]とランダム対戦の待ち列用の[`QueueConn`]。どちらも[`Conn`]で、動かすステートマシンだけが違う。
//!
//! - **受信スレッド**（`higanr-room`）: 接続・Upgrade・受信を繰り返し、届いたものをchannelに積むだけ。ブロックしてよいのはこのスレッドだけ（名前解決・TCP・TLS・HTTPの往復・受信待ち）。
//! - **監視のスレッド**（呼び出し側。`net_code_game`の`GameWatcher`）: [`RoomConn::poll`]でchannelから取り出し、coreの`room::RoomClient`を動かす。
//!   送る文（offer / answer / 30秒ごとのping）を送り、イベントを返す。
//!   ブロックしない（送信は小さな文をサーバーへのTCPの送信側へ渡すだけ。閉じる途中なら送らずに戻る）。
//! - **閉じるスレッド**（`higanr-room-close`。終了時に1本だけ短く動く）: `WinHttpWebSocketClose`を呼ぶ。
//!   ブロック中の受信はそれでその場で戻る（12017）。
//!   `close`自身は相手が閉じ返さないと待ち時間（[`CLOSE_WAIT_MS`]）までブロックするので監視のスレッドから外した。
//!   別のスレッドからハンドルを閉じて受信を解く形は採らない（`room_win.rs`）。
//! - ゲームのスレッドからは何も呼ばない（メモリ確保とブロッキングI/Oがある）。
//!   マッチ中の遅延の増分は0。対戦のルームの接続は成立後も試合の間保つが、監視のスレッドが30秒ごとにpingを送るだけで、対戦のパケットとは別の接続。
//! - 手順の判断は全部coreの`room.rs`（待ち列は`room_queue.rs`）にある。
//!   判断とは何をいつ送るか・届いた文の意味・期限・ping・pongが来ないこと。
//!   ここが持つのは「届いたものを順に渡す」「返ってきた送る文を送る」「終わったら閉じる」だけ。
//!   coreの`room::Role`は[`code_side`]で`net_config::CodeSide`へコピーする（host / guestの語は使わない）。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use higanr_core::room::{
    Action, CLIENT_ID_BYTES, Event, MESSAGE_MAX_BYTES, Now, Role, RoomClient, RoomName, SeatTicket,
    State, SubmitError,
};
use higanr_core::room_queue::{QueueAction, QueueClient, QueueEvent, random_path};
use higanr_core::watch_list::{
    HostAction, HostClient, HostEvent, Posting, ViewAction, ViewClient, ViewEvent,
    WATCH_LIST_MAX_BYTES, watch_post_path, watch_view_path,
};

use crate::net_config::CodeSide;
use crate::net_win::os_random;
use crate::room_win::{Endpoint, OpenError, Proxy, Received, Step, Timeouts, WebSocket};

/// Upgradeまでの各段階（名前解決・TCPの接続・送信・101の先頭）の待ち時間の上限。
/// coreの`ROOM_GREETING_TIMEOUT_MS`と同じ10秒（人がメニューの前で待てる長さ）。
pub const OPEN_TIMEOUT_MS: u32 = 10_000;

// Upgradeの後の受信には上限を設けない（設けられない）。
// `WINHTTP_OPTION_RECEIVE_TIMEOUT`はWebSocketの受信に効かない（loopbackで測った。検査`winhttp_facts_receive_timeout_*`）。
// 生きているかどうかはcoreのping / pong（`room::PONG_TIMEOUT_MS`）が見る。
// 終了時は閉じるスレッドの`close`が受信を解く。

/// 閉じるとき、相手の閉じ返しを待つ上限（`WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT`。単位はmsと測った）。
/// 閉じるスレッドだけがこの間ブロックする。閉じ返しはサーバー内の往復だけなので2秒で足りる。
pub const CLOSE_WAIT_MS: u32 = 2_000;

/// こちらから閉じるときの番号（RFC 6455の 1000 = 正常に終えた。取りやめ・失敗でも同じ）。
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

/// この起動の乱数（入室の要求のクエリ`cid`）。起動ごとに1回だけ生成する。
/// 入り直しが同じ値を送ることで、サーバーは前の入室（こちらには届かなかったもの）を3人目扱いにせず置き換える。
/// 生成できなければ付けない（サーバーは古いクライアントと同じに扱う）。
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

// ---------------- サーバーのホスト名（設定の値） ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub secure: bool,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseError {
    /// `ws://`か`wss://`で始まらない。
    Scheme,
    /// ホスト名が空または使えない文字を含む（英数字・`-`・`.`以外。IPv6の`[...]`も読まない）。
    Host,
    /// ポート番号が数でない・0・65535を越える。
    Port,
    /// パス・クエリ・フラグメントが付いている（パスはルーム名から作る）。末尾の`/`だけは許す。
    Path,
}

impl Base {
    /// `ws://宿[:番号][/]`か`wss://宿[:番号][/]`。番号が無ければ80 / 443。スキームの大文字小文字は問わない。
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

// ---------------- 受信スレッドと監視のスレッドの受け渡し ----------------

#[derive(Debug)]
enum Incoming {
    /// 101を受けた（WebSocketは`Shared::ws`に格納した）。
    Upgraded,
    /// 101でない応答。
    Refused {
        status: u16,
        body: String,
    },
    /// 接続できなかった（WinHTTPの段階と番号）。
    OpenFailed {
        step: Step,
        code: u32,
    },
    Text(String),
    /// バイナリの文（サーバーは送らない）。
    Binary,
    /// 相手のclose（番号。無ければ1005）。
    Closed(u16),
    /// 受信の誤り（WinHTTPの番号）。
    RecvError(u32),
}

struct Shared {
    ws: Mutex<Option<Arc<WebSocket>>>,
    /// 送信側の呼び出し（`send_text` / `close`）を1本に絞るロック。
    /// WinHTTPのWebSocketは送信1本と受信1本までなので、監視のスレッドの送信と閉じるスレッド・受信スレッドの`close`を重ねない。
    /// 監視のスレッドは`try_lock`で取り、取れなければ（閉じる途中なら）送らない。
    send_lock: Mutex<()>,
    /// `close`を1回だけ呼ぶためのフラグ。受信スレッドが相手のcloseに答える処理と閉じるスレッドが閉じる処理とが重なるため。
    closing: AtomicBool,
    /// 監視のスレッドが終えた（取りやめ・手順の終わり）。Upgradeの前なら受信スレッドは接続した直後に自分で閉じる。
    ended: AtomicBool,
}

impl Shared {
    /// `close`を1回だけ呼ぶ。[`CLOSE_WAIT_MS`]までブロックすることがあるので受信スレッドか閉じるスレッドからだけ呼ぶ。
    fn close_once(&self, ws: &WebSocket) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _g = self.send_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ = ws.close(CLOSE_NORMAL);
    }
}

/// closeの待ち時間の上限を設定できなかったこと（Wineなど）を接続ごとでなく起動ごとに1回だけ本ログへ出す。
fn log_close_timeout_once(code: u32) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::AcqRel) {
        crate::main_log::push(format!(
            "ルームサーバーとの通信: WebSocket の close の待ちの上限が設定できませんでした（{code}）。既定の長さで続けます。"
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
    // 101でない本文は1通の上限 + 1バイトまで読む（越えたことをcoreが「サーバーのJSONでない」と読めるように）。
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
    // 監視のスレッドが先に終えていた（接続の途中で取りやめた）か、もう監視のスレッドがいない場合は閉じて終わる。
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
                // 相手のcloseに閉じ返す（こちらが先に閉じていれば何もしない）。
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

// ---------------- 監視のスレッドの入口 ----------------

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
    /// coreが断った（いまはコードを受け付けられない状態・文字が悪い）。状態は変わらない。
    Core(SubmitError),
    /// 送れなかった（接続が切れた・閉じる途中）。失敗のイベントは次の`poll`で返る。
    NotSent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportNote {
    OpenFailed { step: Step, code: u32 },
    RecvError(u32),
    SendError(u32),
    Binary,
}

/// 通信部分が手順のステートマシン（coreの`RoomClient`・`QueueClient`）を動かすための入口。ステートマシンが返す送る文とイベントを[`Act`]に揃える。
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
    /// 受けられる1通の大きさの上限（バイト）。一覧の全体の文はルームの文より大きい。
    fn message_max(&self) -> usize {
        MESSAGE_MAX_BYTES
    }
}

/// ステートマシンが返す1つの仕事。文を送る、またはイベントを上（呼び出し側の監視のスレッド）へ渡す。
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

/// ルームサーバーとの1回の手順（接続する → ルームに入る → コードを渡し合う → 閉じる）。
/// 監視のスレッドが持ち、[`Conn::poll`]を[`Conn::next_poll_in`]より短い間隔で呼ぶ。
/// 手放す（`Drop`）と閉じ始める（待たない）。
/// 手順のステートマシン`M`がルーム（[`RoomConn`]）とランダム対戦の待ち列（[`QueueConn`]）で違うだけで、通信部分は同じ。
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
    /// `submit_code`の中で起きたイベント（送れなかった失敗）。次の`poll`の始まりで返す。
    pending: Vec<M::Event>,
    /// 検査だけ: 以後の送信をWinHTTPの送信の失敗と同じに扱う。
    #[cfg(test)]
    fail_sends: bool,
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
    /// 受信スレッドを作って接続し始める（待たない）。スレッドを作れなければエラーの文を返す。
    /// 対戦のルームは成立後も接続を保つ（[`State::Holding`]。手放すまでサーバーがルーム名を使用中にする）。
    pub fn start(opts: &RoomOptions, room: &RoomName) -> Result<RoomConn, String> {
        let path = match client_id() {
            Some(id) => room.path_with_client(&id),
            None => room.path(),
        };
        Conn::start_at(opts, path, RoomClient::for_room(room))
    }

    /// 観戦用のルーム`room`のhostの席を取りに入る（配信する側・観戦の断り）。
    /// サーバーは`room`について正しく期限内のチケットを持つ接続にだけhostの席を渡す（チケットは対戦の成立で受けたもの。元のルーム名は送らない）。
    pub fn start_as_host(
        opts: &RoomOptions,
        room: &RoomName,
        ticket: &SeatTicket,
    ) -> Result<RoomConn, String> {
        let id = client_id();
        Conn::start_at(
            opts,
            room.path_with_ticket(id.as_ref(), ticket),
            RoomClient::new(),
        )
    }

    /// この入室で受け取った席のチケット（対戦のルームで成立したとき）。古いサーバーや鍵の無いサーバーでは無い。
    pub fn ticket(&self) -> Option<SeatTicket> {
        self.client.ticket()
    }

    /// 自分のコードを渡す（始める側は`Joined`の後。返す側は`PeerCode`の後）。`Ok`は「文をWebSocketへ渡し終えた」こと。
    /// 返す側はこの`Ok`の時点でP2Pを始めてよい（サーバーの1000を待たない）。
    pub fn submit_code(&mut self, code: &str) -> Result<(), SubmitRefused> {
        if self.cancelled {
            return Err(SubmitRefused::NotSent);
        }
        let acts = self.client.submit_code(code).map_err(SubmitRefused::Core)?;
        self.send_acts(acts)
    }

    /// 観戦を断る（観戦用のルームの始める側が`Joined`の後に`offer`の代わりに送る）。以後ルームの期限まで残る。
    pub fn refuse(&mut self) -> Result<(), SubmitRefused> {
        if self.cancelled {
            return Err(SubmitRefused::NotSent);
        }
        let acts = self.client.refuse().map_err(SubmitRefused::Core)?;
        self.send_acts(acts)
    }

    fn send_acts(&mut self, acts: Vec<Action>) -> Result<(), SubmitRefused> {
        let acts = acts_of_room(acts);
        let mut out = Vec::new();
        let sent = self.run(acts, &mut out);
        // 送れなかったときに`run`が積んだ失敗のイベントは、coreが終わった後の`poll`からは出てこない。
        // そのため保持しておき、次の`poll`の始まりで返す（閉じる途中なら、閉じた番号の方が次の`poll`で失敗を返す）。
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
    /// ランダム対戦の待ち列へ接続し始める（待たない）。`build`は`update::BUILD_NUMBER`（同じビルドの人どうしだけがペアになる）。
    /// ペアになると[`QueueEvent::Paired`]で割り当てられたルーム名が届き、手順はそこで終わる。ルームへは[`RoomConn::start`]で入り直す。
    pub fn start_queue(opts: &RoomOptions, build: u32) -> Result<QueueConn, String> {
        let id = client_id();
        let path = random_path(build, id.as_ref());
        Conn::start_at(opts, path, QueueClient::new())
    }
}

impl Conn<HostClient> {
    /// 一覧へ掲載するエンドポイントへ接続し始める（待たない）。接続したら`posting`を送る。一覧に載ったことは`HostEvent::Listed`で分かる。
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

    /// 観戦者の数が変わった。一覧に載っていれば`count`を送る（載る前なら載った直後に送る）。送れなかった失敗は次の`poll`で返る。
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
    /// 一覧を閲覧するエンドポイントへ接続し始める（待たない）。一覧の全体は`ViewEvent::List`で届く。
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
                // パニックはここで受けて外へ出さない。受けるとchannelが切れ、監視のスレッドは「切れた」と読む。
                let _ = catch_unwind(AssertUnwindSafe(|| recv_main(ep, proxy, t, sh, tx, max)));
            })
            .map_err(|e| format!("ルームの受信スレッドの作成不能: {e}"))?;
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
            #[cfg(test)]
            fail_sends: false,
        })
    }

    /// 検査だけ: 以後の送信を失敗させる（upgradeの後にWinHTTPの送信がエラーを返した状態）。
    #[cfg(test)]
    pub fn fail_sends_for_test(&mut self) {
        self.fail_sends = true;
    }

    pub fn now(&self) -> Now {
        Now {
            ms: u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX),
            unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    /// 届いたものを渡して時計を進め、送る文を送る。起きたイベントを順に返す。ブロックしない。
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
                    // 受信スレッドが終わった（閉じる番号・受信のエラーは渡し済み）か、パニックで終了した。
                    // 手順の途中なら「切れた」として扱う。
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
                    // サーバーはバイナリを送らない。来たら途中か相手が壊れているので切れたと同じ扱いにする。
                    self.notes.push(TransportNote::Binary);
                    self.client.on_transport_failed()
                }
                // 番号が無ければ1005のまま渡す（coreは知らない番号として「応答しません」に）。
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

    /// 取りやめる（メニューから操作したときやルーム名を変えたとき）。この後`poll`は何も返さない。閉じ始めて戻る（待たない）。
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.begin_close();
    }

    pub fn is_finished(&self) -> bool {
        self.cancelled || self.client.is_finished()
    }

    /// 次に`poll`を呼ぶべきときまでの長さ（pingと期限）。
    /// 届いた文は時刻に関わらずすぐ渡したいので、監視のスレッドはこれより短い間隔で呼んでもよい。
    pub fn next_poll_in(&self) -> Option<Duration> {
        let now = self.now().ms;
        self.client
            .next_wake()
            .map(|w| Duration::from_millis(w.saturating_sub(now)))
    }

    pub fn take_notes(&mut self) -> Vec<TransportNote> {
        std::mem::take(&mut self.notes)
    }

    /// 受信スレッド（と閉じるスレッド）が終わるのを`limit`まで待つ。終わったらtrue。
    /// 監視のスレッドでは呼ばない（検査と、DLLを閉じるときの後始末用）。
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
                    // 相手が先に閉じた。閉じる番号は受信スレッドが先にchannelへ積んである（`recv_main`は積んでから閉じ返す）。
                    // そのためここでは「切れた」にせず、次の`poll`で番号の方（期限・相手が抜けた など）で終える。
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
        #[cfg(test)]
        if self.fail_sends {
            return Sent::Failed;
        }
        let Some(ws) = self.ws.as_ref() else {
            return Sent::Failed;
        };
        // 閉じる途中（受信スレッドが相手のcloseに閉じ返している）なら送らない。待たない。
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

    /// 閉じ始める（1回だけ）。
    /// Upgradeの前なら受信スレッドが接続した直後に閉じる。
    /// 後なら閉じるスレッドを1本作る。
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
            // スレッドを作れない場合は待たない`shutdown`だけ送る（サーバーが閉じ返せば受信スレッドはそれで終わる）。
            let _ = ws.shutdown(CLOSE_NORMAL);
        }
    }
}

enum Sent {
    Yes,
    /// 閉じる途中で送らなかった（相手のcloseの番号がchannelにある）。
    Closing,
    /// 送れなかった（接続していない・WinHTTPの誤り）。
    Failed,
}

impl<M: Machine> Drop for Conn<M> {
    fn drop(&mut self) {
        self.begin_close();
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! loopbackの偽のルームサーバー。`src/server/src/index.ts`（入口での断り）と`room.ts`の振る舞いのコピー。
    //! コピー範囲は次の通り。2人の役割・offer / peer / matched / answer・観戦の断りrefuseと4403・閉じる番号と直前のerrorの文・期限・片方が抜けたとき・席のチケット・成立後に2人を残して3人目をroom_in_useで断ること・生存確認（ping）が古くなった接続を閉じること。
    //! 値は全部`tests/fixtures/room/rules.json`から読み、自前の定数を持たない。`127.0.0.1:0`で受け、外へは接続しない。
    //! 席のチケット（`ticket.ts`）は、形と期限と「チケットの名のw.でしか席が取れない」ことをコピーする。
    //! 形の桁・文字数と用途の識別子はcoreの`room`の定数で、coreの検査がrules.jsonと突き合わせる。
    //! MACは本物のHMAC-SHA256でなく、偽サーバーの中だけの鍵とSHA-1 2本で作る（本物のチケットとは互換でない）。
    //! コピーしていないもの: 同じcidの入り直しの置き換え・Hibernation・Durable Objectの記憶・IPごとの数え・成立後のアラームの間隔。
    //! - 同じcidの入り直しの置き換え: 検査の一部は1つのプロセスから2人分を接続する。クエリは受けて`seen`に残すだけ。
    //! - IPごとの数え: 1本にまとめ、通す数は検査が`limit`で設定する。
    //! - 成立後のアラームの間隔: 生存確認（ping）の古さはポーリングの5 msごとに見る。
    //! クライアントの文はJSON.parseでなく、こちらのクライアントが送る4つの形（offer・answer・refuse・pingの12文字）との文字列の一致で読む。
    //! ほかはroom.tsと同じく4400にする。
    //! WebSocketの握手のSHA-1はOSのBCrypt（`room_win::sha1`）。フレームの組み立てとマスク解きはRFC 6455 §5を読んで手で書いた。
    //! 役割の名はroom.tsの文字（`"host"` / `"guest"`）を文にだけ書く。Rustの名前は`Side::Offer` / `Side::Answer`。

    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const RULES_JSON: &str = include_str!("../../../tests/fixtures/room/rules.json");

    /// WebSocketの握手の決まった文字（RFC 6455 §1.3）。
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
        pub held_stale_ms: u64,
        pub http_room_in_use: String,
        pub http_room_full: String,
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
        pub watch_room_prefix: String,
        pub watch_answer_ttl_ms: u64,
        pub ticket_param: String,
        pub ticket_ttl_secs: u64,
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

    /// `%XX`をデコードする（`decodeURIComponent`のコピー）。
    /// `%`の後が16進2桁でない場合と、デコードした結果がUTF-8でない場合（サロゲート・オーバーロング符号を含む）はNone。
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

    /// 平らなJSON（文字列と整数だけ）の小さなパーサー。見本の形だけを読む（入れ子・小数・真偽が来たらパニックする）。
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
            // 正規化の方法: コピーはNFKCを持たない（クレートを足さない）ので、前後の空白と小文字化だけをコピーする。
            // 検査の名はNFKCで変わらない文字で選ぶ。見本の手順が変わったら、コピーも直すよう検査を失敗させる。
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
                held_stale_ms: n("held_stale_ms") as u64,
                http_room_in_use: s("http_room_in_use"),
                http_room_full: s("http_room_full"),
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
                watch_room_prefix: s("watch_room_prefix"),
                watch_answer_ttl_ms: n("watch_answer_ttl_ms") as u64,
                ticket_param: s("ticket_param"),
                ticket_ttl_secs: n("ticket_ttl_secs") as u64,
            }
        }

        /// rules.tsの`parseBuildSegment`: 先頭に0を付けない10進でu32の範囲。
        fn build_from_segment(seg: &str) -> Option<u32> {
            let canonical = seg == "0"
                || (!seg.starts_with('0')
                    && !seg.is_empty()
                    && seg.len() <= 10
                    && seg.bytes().all(|b| b.is_ascii_digit()));
            canonical.then(|| seg.parse().ok()).flatten()
        }

        /// 正規化した後の名が、ランダム対戦に割り当てられる形（先頭 ＋ 小文字の16進）かどうか。
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

        /// rules.tsの`roomNameFromPath`: パスの先頭の後ろが空でなく`/`を含まない。
        fn room_from_path<'a>(&self, path: &'a str) -> Option<&'a str> {
            let rest = path.strip_prefix(self.path_prefix.as_str())?;
            (!rest.is_empty() && !rest.contains('/')).then_some(rest)
        }

        /// rules.tsの`decodeRoomName`と`normalizeRoomName`。パスの区切りを受け取り、デコードして検証し、正規化した文字列を返す。
        /// 正規化はNFKCを除いたコピー（[`Rules::load`]の注釈）。
        fn normalize(&self, segment: &str) -> Option<String> {
            // 先に長さのガード（1 Bは3文字まで）をかけ、デコードできないエンコード（`%`の後が16進2桁でない・UTF-8として壊れている）も断る。
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

        /// クエリの`ticket`（`URLSearchParams`と同じく`+`は空白、`%XX`はデコードする）。無ければNone。
        fn ticket_of_query(&self, query: &str) -> Option<String> {
            let raw = query.split('&').find_map(|kv| {
                kv.strip_prefix(self.ticket_param.as_str())?
                    .strip_prefix('=')
            })?;
            // デコードできないエンコードは、形の合わないチケットとして断る（サーバーはURLのパースで捨てるが、本番のクライアントは出さない）。
            Some(percent_decode(&raw.replace('+', " ")).unwrap_or_else(|| "%".into()))
        }

        /// rules.tsの`checkCode`（長さ → 文字の順）。良ければNone。悪ければ閉じる名を返す。
        fn check_code(&self, code: &str) -> Option<&'static str> {
            let n: usize = code.chars().map(char::len_utf16).sum();
            if n > self.code_max_chars {
                return Some("too_large");
            }
            let ok = n >= self.code_min_chars && code.chars().all(|c| self.code_chars.contains(c));
            (!ok).then_some("bad_code")
        }
    }

    // ---- 席のチケット（ticket.tsのコピー。MACだけ偽物）----

    use higanr_core::room::{TICKET_MAC_CHARS, TICKET_NUM_MAX_DIGITS, TICKET_PURPOSE};

    /// 偽サーバーのチケットのMACの鍵（本物のSEAT_KEYの代わり）。
    const FAKE_SEAT_KEY: &[u8] = b"higanr-fake-seat-key-loopback-only";

    fn base64url(data: &[u8]) -> String {
        base64(data)
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_")
    }

    /// チケットのMACの文字列（32 Bをbase64urlにした`TICKET_MAC_CHARS`文字）。署名する文は本物と同じ並び（用途の識別子・w. の名・発行・期限）。
    fn ticket_mac(watch: &str, issued: u64, expires: u64) -> String {
        let text = format!("{TICKET_PURPOSE}\n{watch}\n{issued}\n{expires}");
        let mut mac = Vec::new();
        for tag in *b"AB" {
            let mut m = vec![tag];
            m.extend_from_slice(FAKE_SEAT_KEY);
            m.extend_from_slice(text.as_bytes());
            mac.extend(crate::room_win::sha1(&m).expect("SHA-1"));
        }
        mac.truncate(32);
        base64url(&mac)
    }

    fn mint(watch: &str, issued: u64, ttl: u64) -> String {
        let expires = issued + ttl;
        format!("{issued}.{expires}.{}", ticket_mac(watch, issued, expires))
    }

    /// ticket.tsの`TicketVerdict`。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TicketVerdict {
        None,
        Bad,
        Expired,
        Valid(u64),
    }

    /// ticket.tsの`TICKET_RE`の数のフィールド。`0`か、先頭が0でない`TICKET_NUM_MAX_DIGITS`桁までの10進。
    fn ticket_number(s: &str) -> Option<u64> {
        let ok = s == "0"
            || (!s.is_empty()
                && s.len() <= TICKET_NUM_MAX_DIGITS
                && !s.starts_with('0')
                && s.bytes().all(|b| b.is_ascii_digit()));
        ok.then(|| s.parse().ok()).flatten()
    }

    /// ticket.tsの`judgeTicket`。`keyed`が偽ならSEAT_KEYの無いサーバー（形の合わないものだけBad、ほかはNone）。
    fn judge_ticket(
        ticket: Option<&str>,
        watch: &str,
        now_secs: u64,
        keyed: bool,
    ) -> TicketVerdict {
        let Some(t) = ticket else {
            return TicketVerdict::None;
        };
        let parts: Vec<&str> = t.split('.').collect();
        let shaped = parts.len() == 3
            && parts[2].len() == TICKET_MAC_CHARS
            && parts[2]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let (Some(issued), Some(expires), true) = (
            parts.first().and_then(|s| ticket_number(s)),
            parts.get(1).and_then(|s| ticket_number(s)),
            shaped,
        ) else {
            return TicketVerdict::Bad;
        };
        if !keyed {
            return TicketVerdict::None;
        }
        // 1通りの書き方のMACだけが一致する（最後の文字の使わないビットを変えたものもBad）。
        if parts[2] != ticket_mac(watch, issued, expires) {
            return TicketVerdict::Bad;
        }
        if now_secs >= expires {
            return TicketVerdict::Expired;
        }
        TicketVerdict::Valid(issued)
    }

    /// 打った名からroom.tsと同じ方法で導いた観戦用の名（dllの`spectate_of`）。パスの区切りを受け取る。
    fn watch_name_of_segment(segment: &str) -> Option<String> {
        let raw = percent_decode(segment)?;
        higanr_core::room::RoomName::parse_server_rules(&raw)
            .ok()
            .map(|r| r.spectate_of().as_str().to_owned())
    }

    #[derive(Default)]
    pub struct Knobs {
        /// pingにpongを返さない（自動応答を止める。片側だけ切れたTCPのコピー）。
        pub no_pong: AtomicBool,
        /// 何を受けても読まない・答えない（closeにも閉じ返さない。止まったサーバーのコピー）。
        pub deaf: AtomicBool,
        /// 101の後にroomを送らない。
        pub no_room: AtomicBool,
        /// roomの後に番号の無いcloseを送る（WinHTTPがそれを1005と返すかを見る）。
        pub close_without_status: AtomicBool,
        /// 閉じる前のerrorの文を送らない（閉じる番号だけで正しく読めるかを見る）。
        pub no_error_text: AtomicBool,
        /// 0なら見本の値（rules.json）。期限の検査が短く差し替える。
        pub room_ttl_ms: AtomicU64,
        pub answer_ttl_ms: AtomicU64,
        /// 0なら見本の値。ランダム対戦の待ち列の期限と割り当てたルームの期限。
        pub random_queue_ttl_ms: AtomicU64,
        pub random_room_ttl_ms: AtomicU64,
        /// 待ち列でペアにしない（待ち列の「待つ」を続けさせる）。
        pub no_pairing: AtomicBool,
        /// 一覧へ掲載するエンドポイントがpostをlist_fullで断る（件数の上限のコピー）。
        pub list_full: AtomicBool,
        /// 一覧へ掲載するエンドポイントがpostをbad_messageで断る（入り直しの検査用）。
        pub list_bad: AtomicBool,
        /// 回数制限までに通す要求の数（0なら数えない）。
        pub limit: AtomicUsize,
        /// SEAT_KEYの無いサーバーの動き。チケットを載せず、観戦用のルームのhostの席と一覧への投稿を断る。
        pub no_seat_key: AtomicBool,
        /// 0なら見本の値。観戦用のルームの観戦者のanswerの期限。
        pub watch_answer_ttl_ms: AtomicU64,
        /// 0なら見本の値。成立後の接続の生存確認（ping）の古さの上限。
        pub held_stale_ms: AtomicU64,
        /// 入口で必ずこの応答を返す（ステータスコード・本文）。Cloudflare自身のエラーページなどのコピー。
        pub forced: Mutex<Option<(u16, String)>>,
    }

    #[derive(Default)]
    pub struct Seen {
        pub texts: Mutex<Vec<String>>,
        pub client_closes: Mutex<Vec<Option<u16>>>,
        /// 届いた要求のパス。WinHTTPがルーム名のエンコードを変えずに渡すかを検証するためのもので、届いた文字のまま持つ（クエリは含めない）。
        pub paths: Mutex<Vec<String>>,
        /// 届いた要求のクエリ（`?`の後ろ。無ければ空）。`paths`と同じ順。
        pub queries: Mutex<Vec<String>>,
        pub upgrades: AtomicUsize,
        /// ランダム対戦の待ち列に入った数（Upgradeした数）。
        pub queue_upgrades: AtomicUsize,
        /// 待ち列が割り当てたルーム名（検査が「ログや画面に出ていない」ことを検証するときの元データ）。
        pub assigned: Mutex<Vec<String>>,
        /// 一覧へ掲載するエンドポイントと閲覧するエンドポイントに入った数（Upgradeした数）。
        pub watch_post_upgrades: AtomicUsize,
        pub watch_view_upgrades: AtomicUsize,
        /// 一覧へ掲載するエンドポイントへ届いたpostの文（検査が1P・2Pの名前・方式を検証する）。
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
        /// hostだけ: 観戦を断った（room.tsの`refused`）。
        refused: bool,
        /// 観戦用のルームかどうか。
        watch: bool,
        /// hostだけ: matchedを送った（guestが入れ替わっても2度は送らない）。
        matched: bool,
        /// 対戦のルームだけ: この接続が打った名から導いた観戦用の名（チケットに署名する名）。鍵が無ければNone。
        wname: Option<String>,
        /// 対戦のルームのhostだけ: 最初に揃ったときのチケットの発行時刻（UNIX秒）。
        issued: Option<u64>,
        /// 観戦用のルームのhostだけ: 席を取ったチケットの発行時刻（UNIX秒）。
        seat: Option<u64>,
        /// 対戦のルームだけ: 成立した時刻（room.tsの`held`）。以後は期限で閉じず、生存確認（ping）で見る。
        held: Option<Instant>,
        /// 最後にpingを受けた時刻（room.tsの自動応答の時刻）。
        last_ping: Option<Instant>,
        w: Arc<Mutex<TcpStream>>,
    }

    /// ランダム対戦の待ち列の1人（random_queue.tsの`QueueAtt`）。
    struct QMember {
        id: usize,
        deadline: Instant,
        done: bool,
        w: Arc<Mutex<TcpStream>>,
    }

    /// 一覧に載った1件（watch_list.tsの`WatchItem`）。
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
        /// ビルド番号ごとの一覧（載せた順）とそれを見ている人。
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
                            close_stale_held(&inner, room, now);
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

        /// 観戦用のルーム`watch`について、いま発行したチケット（検査が対戦を成立させずに配信する側を動かすため）。
        pub fn mint_ticket(
            &self,
            watch: &higanr_core::room::RoomName,
        ) -> higanr_core::room::SeatTicket {
            self.mint_ticket_at(watch, unix_ms() / 1000)
        }

        /// 発行時刻（UNIX秒）を指定したチケット（より新しいチケットが席を取る検査のため）。
        pub fn mint_ticket_at(
            &self,
            watch: &higanr_core::room::RoomName,
            issued: u64,
        ) -> higanr_core::room::SeatTicket {
            let t = mint(watch.as_str(), issued, self.inner.rules.ticket_ttl_secs);
            higanr_core::room::SeatTicket::parse(&t).expect("偽サーバーの札が札の形でない")
        }

        pub fn texts_matching(&self, pred: impl Fn(&str) -> bool) -> usize {
            lock(&self.seen.texts).iter().filter(|t| pred(t)).count()
        }

        /// 一覧（ビルド番号ごと）に載っている件を載せた順に返す。観戦用のルーム名は割り当てられた名から導いた`w.…`。
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

        /// 一覧を見ている接続を今すぐ全部4408 expiredで閉じる。サーバーは作ってから10分で閉じる。待たずに検査が起こすための関数。
        pub fn expire_views(&self, build: u32) {
            let name = "expired";
            let mut watch = lock(&self.inner.watch);
            let Some(book) = watch.get_mut(&build) else {
                return;
            };
            for v in book.views.drain(..) {
                write_frame(
                    &v.w,
                    0x1,
                    format!(r#"{{"t":"error","code":"{name}"}}"#).as_bytes(),
                );
                let mut p = self.inner.rules.close(name).to_be_bytes().to_vec();
                p.extend_from_slice(name.as_bytes());
                write_frame(&v.w, 0x8, &p);
            }
        }
    }

    /// 偽サーバーの一覧に載っている1件。
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

    // ---- フレーム（RFC 6455 §5.2）----

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

    /// room.tsの`closeWith`。終了済みにし、withErrorならerrorの文を1通送ってから閉じる（1000では送らない）。
    /// つまみ`no_error_text`を立てるとerrorの文を省く。
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

    // ---- HTTPの入口（index.ts）----

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
        // 相手が読み終えて閉じるまで待つ（先に閉じるとRSTで本文が失われることがある）。
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let mut sink = [0u8; 256];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    }

    fn serve(inner: &Inner, mut s: TcpStream) {
        let Some(head) = read_head(&mut s) else {
            return;
        };
        let rules = &inner.rules;
        // クエリはindex.tsが見る`url.pathname`に入らない。ここでもパスから外す。
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
        // ルーム名はエンコードされたまま来る。デコードできないエンコードも文字の決まりの外と同じ400にする。
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

        // 観戦用のルームのhostの席は、そのルームの名について正しく期限内のチケットを持つ接続だけが取れる（room.ts）。
        let is_watch = name.starts_with(rules.watch_room_prefix.as_str());
        let keyed = !inner.knobs.no_seat_key.load(Ordering::Acquire);
        let fresh = if is_watch {
            let t = rules.ticket_of_query(query);
            match judge_ticket(t.as_deref(), &name, unix_ms() / 1000, keyed) {
                TicketVerdict::Bad => return respond(&mut s, 400, "", r#"{"error":"bad_ticket"}"#),
                TicketVerdict::Valid(issued) => Some(issued),
                TicketVerdict::None | TicketVerdict::Expired => None,
            }
        } else {
            None
        };
        let wname = (!is_watch && keyed)
            .then(|| watch_name_of_segment(raw))
            .flatten();
        let id = inner.next_id.fetch_add(1, Ordering::AcqRel);
        {
            let mut rooms = lock(&inner.rooms);
            let now = Instant::now();
            let room = rooms.entry(name.clone()).or_default();
            expire_if_due(inner, room, now);
            close_stale_held(inner, room, now);
            let live: Vec<usize> = (0..room.len()).filter(|&i| !room[i].done).collect();
            let seated = live.iter().copied().find(|&i| room[i].side == Side::Offer);
            let new_room = || {
                let t = if rules.is_random_room(&name) {
                    ttl(&inner.knobs.random_room_ttl_ms, rules.random_room_ttl_ms)
                } else {
                    ttl(&inner.knobs.room_ttl_ms, rules.room_ttl_ms)
                };
                (Side::Offer, now + Duration::from_millis(t), unix_ms() + t)
            };
            let (side, deadline, deadline_unix_ms) = if let (true, Some(h), Some(f)) =
                (is_watch, seated, fresh)
                && room[h].seat.is_none_or(|s| f > s)
            {
                // より新しい対戦のチケットの場合は前のhost（とそのコードを受けた観戦者）を閉じて席を渡す。
                for &i in &live {
                    close_with(inner, &mut room[i], "peer_left", true);
                }
                new_room()
            } else if live.is_empty() {
                if is_watch && fresh.is_none() {
                    drop(rooms);
                    let body = format!(r#"{{"error":"{}"}}"#, rules.http_room_full);
                    return respond(&mut s, 409, "", &body);
                }
                new_room()
            } else if live.len() == 1
                && room[live[0]].side == Side::Offer
                && room[live[0]].held.is_none()
            {
                (
                    Side::Answer,
                    room[live[0]].deadline,
                    room[live[0]].deadline_unix_ms,
                )
            } else {
                drop(rooms);
                let error = if is_watch {
                    &rules.http_room_full
                } else {
                    &rules.http_room_in_use
                };
                return respond(&mut s, 409, "", &format!(r#"{{"error":"{error}"}}"#));
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
                refused: false,
                watch: is_watch,
                matched: false,
                wname,
                issued: None,
                seat: fresh.filter(|_| is_watch && side == Side::Offer),
                held: None,
                last_ping: None,
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
            // 観戦を断っているhostのルームへ入ったguestは、コードを渡さず断りの印で閉じる（hostは残す）。
            let host_refused = room
                .iter()
                .any(|h| !h.done && h.side == Side::Offer && h.refused);
            if side == Side::Answer && host_refused {
                close_with(inner, &mut m, "refused", true);
            }
            room.push(m);
            if side == Side::Answer && !host_refused {
                try_match(inner, room, now);
            }
        }
        read_loop(inner, &name, id, &mut s);
        let _ = s.shutdown(Shutdown::Both);
    }

    /// index.tsの`overLimit`。通す数（`knobs.limit`）を越えていれば429を返してtrueを返す。
    /// 数えるカウンタはルームと待ち列で共通の1本。
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

    // ---- ランダム対戦の待ち列（random_queue.tsのコピー）----
    //
    // コピーしていないもの: 同じcidの入り直しの置き換え（ルームのコピーと同じ理由。検査は1つのプロセスから2人分を接続するのでcidが同じ）。

    /// index.tsの待ち列のエンドポイントと`RandomQueue.fetch`。ビルド番号ごとの待ち列に入れ、2人そろえば割り当てたルーム名を返して閉じる。
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

    /// `RandomQueue.tryPair`。入った順に先の2人からペアにする。割り当てた名は`seen.assigned`にも残す（検査用）。
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

    // ---- 進行中のランダム対戦の一覧（watch_list.tsのコピー）----
    //
    // コピーしていないもの: 1 IPあたり2件の上限・掲載した人が生きているかの期限・閲覧するエンドポイントへの変化のまとめ送信・同じルームの掲載し直しの置き換え。
    // まとめ送信の代わりに、変わるたびにすぐ全体を送る。
    // クライアントの文はJSON.parseでなく`read_flat`で読む。

    fn is_name(s: &str) -> bool {
        (1..=32).contains(&s.chars().count())
            && s.chars().all(higanr_core::room::RoomName::accepts_char)
            && !s.starts_with(' ')
            && !s.ends_with(' ')
    }

    /// `JSON.stringify`が文字列に書き込む形（名前は0x20〜0x7Eだけなので`"`と`\`だけエスケープする）。
    fn json_str(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// `parsePost`のコピー。割り当てられた名の形・名前の文字・方式・数の範囲を検証する。
    /// 通れば一覧に載せる1件を返す（観戦用のルーム名は割り当てられた名から導く）。
    fn parse_post(rules: &Rules, text: &str, keyed: bool) -> Option<WItem> {
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
        let ticket = st("ticket")?;
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
        // その割り当ての名で本当に対戦が成立した証拠（席のチケット）が無ければ載せない。鍵が無ければ全部断る。
        if !matches!(
            judge_ticket(Some(&ticket), &room, unix_ms() / 1000, keyed),
            TicketVerdict::Valid(_)
        ) {
            return None;
        }
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

    /// `WatchList.listMessage`のコピー。載せた順の全体を返す（キーの順はJSON.stringifyの出力と同じ）。
    fn watch_list_text(book: &WatchBook) -> String {
        let items: Vec<String> = book
            .posts
            .iter()
            .filter_map(|p| p.item.as_ref())
            .map(|i| {
                format!(
                    r#"{{"room":"{}","p1":"{}","p2":"{}","mode":"{}","delay":{},"watchers":{},"cap":{}}}"#,
                    i.room,
                    json_str(&i.p1),
                    json_str(&i.p2),
                    i.mode,
                    i.delay,
                    i.watchers,
                    i.cap
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

    /// 掲載するエンドポイントと閲覧するエンドポイントの接続を1本読む。
    /// 切れたら一覧から外し、載っていたなら閲覧している人へ全体を送り直す。
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

    /// 掲載するエンドポイントに届いた1通の処理。
    /// 最初のpostなら一覧へ載せ、掲載してきた接続へlistedを送る。
    /// countなら掲載した項目の観戦者数を替える。
    /// 断るならerrorの名を返す（呼び出し側が掲載してきた接続へ送る）。
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
        let keyed = !inner.knobs.no_seat_key.load(Ordering::Acquire);
        let Some(item) = parse_post(rules, text, keyed) else {
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
        // `setWebSocketAutoResponse`: 文字の完全一致だけ。Durable Objectを起動しない（doneでも返す）。
        if text == rules.ping_text {
            if !inner.knobs.no_pong.load(Ordering::Acquire) {
                room[i].last_ping = Some(Instant::now());
                send_text(&room[i], &rules.pong_text);
            }
            return;
        }
        if room[i].done {
            return;
        }
        // 成立の後はpingだけを送る約束（room.ts）。ほかの文は誤りとして閉じ、相手も閉じる。
        if room[i].held.is_some() {
            return fail(inner, room, id, "bad_message");
        }
        if text.len() > rules.message_max_bytes {
            return fail(inner, room, id, "too_large");
        }
        // room.tsのrefuse。観戦用のルームのhostがofferの前に1回だけ送る。
        // 待っているguestは断りの印で閉じ、hostは残す。
        if text == higanr_core::room::REFUSE_TEXT {
            let ok = room[i].side == Side::Offer
                && room[i].code.is_none()
                && !room[i].refused
                && name.starts_with(rules.watch_room_prefix.as_str());
            if !ok {
                return fail(inner, room, id, "bad_message");
            }
            room[i].refused = true;
            for g in room
                .iter_mut()
                .filter(|m| !m.done && m.side == Side::Answer)
            {
                close_with(inner, g, "refused", true);
            }
            return;
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
                if room[i].side != Side::Offer || room[i].code.is_some() || room[i].refused {
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
                if room[i].watch {
                    close_with(inner, &mut room[h], "done", false);
                    close_with(inner, &mut room[i], "done", false);
                } else {
                    // 対戦のルーム: 2人を残してルーム名を使用中にする。
                    room[h].held = Some(now);
                    room[i].held = Some(now);
                }
            }
        }
    }

    /// room.tsの`tryMatch`。対戦のルームの最初の成立で、hostへはmatched、guestへはpeerにチケットを載せる（鍵があるときだけ）。
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
        let first = !room[h].matched;
        if first {
            let a = ttl(&inner.knobs.answer_ttl_ms, inner.rules.answer_ttl_ms);
            let by_answer = now + Duration::from_millis(a);
            if by_answer < room[h].deadline {
                room[h].deadline = by_answer;
                room[h].deadline_unix_ms = unix_ms() + a;
            }
            room[h].matched = true;
            if !room[h].watch {
                room[h].issued = Some(unix_ms() / 1000);
            }
        }
        // 観戦用のルームは観戦者ごとに短い期限で縛る（過ぎた観戦者だけを閉じ、hostは次の観戦者を待つ）。
        let (deadline, unix) = if room[h].watch {
            let w = ttl(
                &inner.knobs.watch_answer_ttl_ms,
                inner.rules.watch_answer_ttl_ms,
            );
            let by = now + Duration::from_millis(w);
            if by < room[h].deadline {
                (by, unix_ms() + w)
            } else {
                (room[h].deadline, room[h].deadline_unix_ms)
            }
        } else {
            (room[h].deadline, room[h].deadline_unix_ms)
        };
        room[g].deadline = deadline;
        room[g].deadline_unix_ms = unix;
        room[g].got_peer = true;
        let ttl_secs = inner.rules.ticket_ttl_secs;
        let ticket_of = |m: &Member| -> String {
            match (&m.wname, room[h].issued) {
                (Some(w), Some(i)) => format!(r#","ticket":"{}""#, mint(w, i, ttl_secs)),
                _ => String::new(),
            }
        };
        let (gt, ht) = (ticket_of(&room[g]), ticket_of(&room[h]));
        send_text(&room[g], &format!(r#"{{"t":"peer","code":"{code}"{gt}}}"#));
        if first {
            send_text(
                &room[h],
                &format!(
                    r#"{{"t":"matched","expires_at":{}{ht}}}"#,
                    room[h].deadline_unix_ms / 1000
                ),
            );
        }
    }

    /// room.tsの`closeStaleHeld`。成立後の接続のうち、最後のping（無ければ成立の時刻）が古いものをexpiredで閉じる。
    /// 成立後のもう1人はpeer_leftで閉じる。
    fn close_stale_held(inner: &Inner, room: &mut [Member], now: Instant) {
        let limit =
            Duration::from_millis(ttl(&inner.knobs.held_stale_ms, inner.rules.held_stale_ms));
        let stale = |m: &Member| {
            m.held.is_some_and(|h| {
                let last = m.last_ping.map_or(h, |p| p.max(h));
                now.saturating_duration_since(last) > limit
            })
        };
        let marks: Vec<bool> = room.iter().map(|m| !m.done && stale(m)).collect();
        if !marks.contains(&true) {
            return;
        }
        for (m, was_stale) in room.iter_mut().zip(marks) {
            if m.done || m.held.is_none() {
                continue;
            }
            close_with(
                inner,
                m,
                if was_stale { "expired" } else { "peer_left" },
                true,
            );
        }
    }

    /// room.tsの`expireIfDue`。成立の後の接続は期限で閉じない。
    fn expire_if_due(inner: &Inner, room: &mut [Member], now: Instant) -> bool {
        // 観戦用のルーム: answerの期限を過ぎた観戦者だけを閉じ、hostは残す。
        if let Some(h) = room
            .iter()
            .position(|m| !m.done && m.side == Side::Offer && m.watch)
            && now < room[h].deadline
        {
            let mut any = false;
            for m in room
                .iter_mut()
                .filter(|m| !m.done && m.side == Side::Answer && now >= m.deadline)
            {
                close_with(inner, m, "expired", true);
                any = true;
            }
            return any;
        }
        let waiting = |m: &Member| !m.done && m.held.is_none();
        let Some(d) = room.iter().filter(|m| waiting(m)).map(|m| m.deadline).min() else {
            return false;
        };
        if now < d {
            return false;
        }
        for m in room.iter_mut().filter(|m| waiting(m)) {
            close_with(inner, m, "expired", true);
        }
        true
    }

    /// room.tsの`fail`。
    fn fail(inner: &Inner, room: &mut [Member], id: usize, name: &str) {
        let Some(i) = idx(room, id) else { return };
        close_with(inner, &mut room[i], name, true);
        after_leave(inner, room, i);
    }

    /// room.tsの`gone`。
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

    /// room.tsの`afterLeave`。観戦用のルームではguestが抜けてもhostを閉じない（次の観戦者へ同じコードを渡す）。
    /// 成立の後にどちらかが抜けたら、もう1人も閉じる。
    fn after_leave(inner: &Inner, room: &mut [Member], i: usize) {
        let (side, got_peer, watch, held) = (
            room[i].side,
            room[i].got_peer,
            room[i].watch,
            room[i].held.is_some(),
        );
        for (j, other) in room.iter_mut().enumerate() {
            if j == i || other.done {
                continue;
            }
            if side == Side::Offer || (got_peer && !watch) || held {
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
            assert_eq!(r.close("refused"), 4403);
            assert_eq!(r.closes.len(), 8);
            assert_eq!(r.room_ttl_ms, 600_000);
            assert_eq!(r.answer_ttl_ms, 60_000);
            assert_eq!(r.held_stale_ms, 90_000);
            assert_eq!(
                (r.http_room_in_use.as_str(), r.http_room_full.as_str()),
                ("room_in_use", "room_full")
            );
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
    //! 部品の検査（毎回の検査で走る）は`Base`と役割のコピーだけ。
    //! 組み上がりの検査（loopbackの偽サーバー`fake`と本物のWinHTTP・本物のスレッドを使う）は実時間で走るので`#[ignore]`を付け、全件の検査で`--ignored`を付けて走らせる。
    //! 外へは接続しない（`127.0.0.1`と`Proxy::Direct`）。
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

    // ---- 組み上がり: ツール ----

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

    /// 検査のルーム名はサーバーの決まり（`parse_server_rules`）で検証する。
    /// dllの本番（`parse`）が通すのはASCIIの英数字と`-` `_`だけ。
    /// サーバーは日本語も受け付けたままなので、その受け入れ（エンコード・400）はこの入口で検証する。
    fn start(f: &FakeServer, room: &str) -> RoomConn {
        RoomConn::start(
            &opts_for(&f.base()),
            &RoomName::parse_server_rules(room).unwrap(),
        )
        .unwrap()
    }

    /// `poll`を繰り返し呼んでイベントを集める（時計は`offset_ms`だけ進めて渡す）。`stop`が真になったら返す。
    /// `limit`を過ぎたら集めたイベントを出力して検査を失敗させる。
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

    // 始める側・返す側のコード（base64urlの文字。実在のものではない合成の文字）。
    const OFFER_CODE: &str = "HRo1-OfferSide_Code";
    const ANSWER_CODE: &str = "HRa1-AnswerSide_Code";

    // ---- 組み上がり: 手順が通る ----

    /// 2人がコードを渡し合って両方が成立後の保持状態（Held）になり、片方が手放すともう片方も失敗にせず終わるまでを検証する。
    /// `offer_first`は始める側がofferを出してから返す側が来るか、返す側が先に来て待つかの切り替え。
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
            e.contains(&Event::Held)
        });
        assert_eq!(peer_code(&evs_a).as_deref(), Some(ANSWER_CODE));
        assert!(
            evs_a.iter().any(|e| matches!(e, Event::Matched { .. })),
            "始める側は matched を受ける: {evs_a:?}"
        );
        let evs_b = pump(&mut b, 0, Duration::from_secs(5), |e| {
            e.contains(&Event::Held)
        });
        assert!(!has_failed(&evs_a) && !has_failed(&evs_b));
        // 成立の後も2人は残り、同じ名の3人目は使用中で断られる。
        std::thread::sleep(Duration::from_millis(50));
        let evs: Vec<Event> = [a.poll(), b.poll()].concat();
        assert!(evs.is_empty(), "{evs:?}");
        assert_eq!((a.state(), b.state()), (State::Holding, State::Holding));
        let mut third = start(&f, "TRADE-ROOM");
        assert_eq!(wait_failure(&mut third), Failure::RoomInUse);
        // 始める側が手放すとサーバーが返す側も閉じ、返す側は失敗にせず終える。
        a.cancel();
        let evs_b = pump(&mut b, 0, Duration::from_secs(5), |e| {
            e.contains(&Event::Done)
        });
        assert!(!has_failed(&evs_b), "{evs_b:?}");
        assert!(a.is_finished() && b.is_finished());
        assert_joins(&mut a, Duration::from_secs(3));
        assert_joins(&mut b, Duration::from_secs(3));
        assert_joins(&mut third, Duration::from_secs(3));
        // 部屋は空いたので次の人は始める側になる。
        let mut next = start(&f, "trade-room");
        let evs = wait_state(&mut next, State::NeedOwnCode);
        assert!(joined_as(&evs, Role::Offer), "{evs:?}");
        next.cancel();
        assert_joins(&mut next, Duration::from_secs(3));
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
        // 届いたパスは、こちらが作ったパスと1文字も違わない。
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

        // こちらは通すが（ASCIIでないので長さを数えない）サーバーが400で断る名。
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
        // ルームを替えても入り直し（同じルームへ2度目）でも同じ値。
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
            (400, r#"{"error":"bad_ticket"}"#.into(), Failure::BadTicket),
            (409, r#"{"error":"room_full"}"#.into(), Failure::RoomFull),
            (409, r#"{"error":"room_in_use"}"#.into(), Failure::RoomInUse),
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
        assert_eq!(
            wait_failure(&mut c),
            Failure::RoomInUse,
            "成立の前でも、対戦のルームの 3 人目は使用中"
        );
        assert!(!a.is_finished() && !b.is_finished());

        // 回数制限の検査。1回だけ通し、次は429と、クライアントが待つ秒数の見本（index.tsと同じ本文とRetry-Afterヘッダー）を返す。
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

        // 誰も受けていないポートへ接続できない（WinHTTPの誤り）→ 応答しません。
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

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_watch_room_hands_its_host_seat_only_to_a_connection_with_a_ticket_for_that_room() {
        let f = FakeServer::start();
        let opts = opts_for(&f.base());
        let origin = RoomName::parse("Seat-Room").unwrap();
        let w = origin.spectate_of();

        // 空の観戦用のルームへチケットなし（観戦者の入り方）→ 409。
        let mut c = RoomConn::start(&opts, &w).unwrap();
        assert_eq!(wait_failure(&mut c), Failure::RoomFull);
        // 別のルームの名のチケット → 400 bad_ticket。
        let other = f.mint_ticket(&RoomName::parse("Seat-Room2").unwrap().spectate_of());
        let mut c = RoomConn::start_as_host(&opts, &w, &other).unwrap();
        assert_eq!(wait_failure(&mut c), Failure::BadTicket);
        // 期限の切れたチケット → 409（観戦者と同じ断り）。
        let ttl = f.rules().ticket_ttl_secs;
        let old = f.mint_ticket_at(&w, unix_now() - ttl - 5);
        let mut c = RoomConn::start_as_host(&opts, &w, &old).unwrap();
        assert_eq!(wait_failure(&mut c), Failure::RoomFull);
        // そのルームのチケット → host。クエリにはチケットだけが載り、元のルーム名は載らない。
        let ticket = f.mint_ticket(&w);
        let mut host = RoomConn::start_as_host(&opts, &w, &ticket).unwrap();
        pump(&mut host, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Offer)
        });
        let queries = f.seen.queries.lock().unwrap().clone();
        let last = queries.last().unwrap();
        assert!(
            last.ends_with(&format!("ticket={}", ticket.as_str())),
            "{queries:?}"
        );
        assert!(
            queries
                .iter()
                .all(|q| !q.to_ascii_lowercase().contains("seat-room") && !q.contains("origin")),
            "{queries:?}"
        );
        // hostがいればチケットなしの観戦者はguestで入れる。3人目は409。
        let mut guest = RoomConn::start(&opts, &w).unwrap();
        pump(&mut guest, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Answer)
        });
        let mut third = RoomConn::start(&opts, &w).unwrap();
        assert_eq!(wait_failure(&mut third), Failure::RoomFull);
        // チケットは観戦用でないルームでは見ない（別の名のチケットでも普通に入る）。
        let mut plain =
            RoomConn::start_as_host(&opts, &RoomName::parse("plain").unwrap(), &other).unwrap();
        pump(&mut plain, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Offer)
        });
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_newer_ticket_takes_the_seat_and_an_equal_one_joins_as_a_guest() {
        let f = FakeServer::start();
        let opts = opts_for(&f.base());
        let w = RoomName::parse("Evict-Room").unwrap().spectate_of();
        let t = unix_now();
        let mut a = RoomConn::start_as_host(&opts, &w, &f.mint_ticket_at(&w, t)).unwrap();
        pump(&mut a, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Offer)
        });
        // 同じ発行のチケット（成立した相手のチケット）は席を取らない。
        let mut same = RoomConn::start_as_host(&opts, &w, &f.mint_ticket_at(&w, t)).unwrap();
        pump(&mut same, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Answer)
        });
        same.cancel();
        assert_joins(&mut same, Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(100));
        // より新しいチケットは前のhostを4410で閉じて席を取る。
        let mut b = RoomConn::start_as_host(&opts, &w, &f.mint_ticket_at(&w, t + 5)).unwrap();
        pump(&mut b, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Offer)
        });
        assert_eq!(wait_failure(&mut a), Failure::PeerLeft);
        // 古いチケットで入り直すとguest。
        let mut a2 = RoomConn::start_as_host(&opts, &w, &f.mint_ticket_at(&w, t)).unwrap();
        pump(&mut a2, 0, Duration::from_secs(5), |e| {
            joined_as(e, Role::Answer)
        });
        assert!(!b.is_finished());
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_room_match_hands_both_sides_a_ticket_that_takes_the_seat_of_the_watch_room_they_derive() {
        for keyed in [true, false] {
            let f = FakeServer::start();
            f.knobs.no_seat_key.store(!keyed, Ordering::Release);
            let mut a = start(&f, "Ticket-Room");
            wait_state(&mut a, State::NeedOwnCode);
            a.submit_code(OFFER_CODE).unwrap();
            let mut b = start(&f, "ticket-room");
            wait_state(&mut b, State::NeedOwnCode);
            b.submit_code(ANSWER_CODE).unwrap();
            pump(&mut a, 0, Duration::from_secs(5), |e| {
                e.contains(&Event::Held)
            });
            pump(&mut b, 0, Duration::from_secs(5), |e| {
                e.contains(&Event::Held)
            });
            if !keyed {
                assert_eq!((a.ticket(), b.ticket()), (None, None), "鍵の無いサーバー");
                // 鍵の無いサーバーは偽サーバーが作ったチケットでも席を渡さない。
                let w = RoomName::parse("Ticket-Room").unwrap().spectate_of();
                let mut c =
                    RoomConn::start_as_host(&opts_for(&f.base()), &w, &f.mint_ticket(&w)).unwrap();
                assert_eq!(wait_failure(&mut c), Failure::RoomFull);
                continue;
            }
            let (ta, tb) = (
                a.ticket().expect("始める側の札"),
                b.ticket().expect("返す側の札"),
            );
            // 大文字小文字だけが違う名は同じ観戦用のルーム。どちらのチケットでも席が取れる。
            let w = RoomName::parse("Ticket-Room").unwrap().spectate_of();
            for t in [ta, tb] {
                let mut h = RoomConn::start_as_host(&opts_for(&f.base()), &w, &t).unwrap();
                pump(&mut h, 0, Duration::from_secs(5), |e| {
                    joined_as(e, Role::Offer)
                });
                h.cancel();
                assert_joins(&mut h, Duration::from_secs(3));
                std::thread::sleep(Duration::from_millis(100));
            }
            // そのチケットは別のルームの観戦用のルームでは使えない。
            let elsewhere = RoomName::parse("Other-Room").unwrap().spectate_of();
            let mut c = RoomConn::start_as_host(&opts_for(&f.base()), &elsewhere, &ta).unwrap();
            assert_eq!(wait_failure(&mut c), Failure::BadTicket);
            // クライアントはチケットを文では送らない（クエリに付けるだけ）。
            assert_eq!(f.texts_matching(|t| t.contains("ticket")), 0);
        }
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_watch_host_outlives_viewers_that_leave_or_time_out_without_answering() {
        let f = FakeServer::start();
        f.knobs.watch_answer_ttl_ms.store(400, Ordering::Release);
        let opts = opts_for(&f.base());
        let w = RoomName::parse("Outlive-Room").unwrap().spectate_of();
        let mut host = RoomConn::start_as_host(&opts, &w, &f.mint_ticket(&w)).unwrap();
        wait_state(&mut host, State::NeedOwnCode);
        host.submit_code(OFFER_CODE).unwrap();
        // 1人目はコードを受けて答えずに抜ける。
        let mut v1 = RoomConn::start(&opts, &w).unwrap();
        let evs = pump(&mut v1, 0, Duration::from_secs(5), |e| {
            peer_code(e).is_some()
        });
        assert_eq!(peer_code(&evs).as_deref(), Some(OFFER_CODE));
        v1.cancel();
        let evs = pump(&mut host, 0, Duration::from_secs(5), |e| {
            e.iter().any(|x| matches!(x, Event::Matched { .. }))
        });
        assert!(!has_failed(&evs), "{evs:?}");
        std::thread::sleep(Duration::from_millis(200));
        // 2人目は同じコードを受け、答えの期限で閉じられる。hostにはmatchedがもう来ない。
        let mut v2 = RoomConn::start(&opts, &w).unwrap();
        let evs = pump(&mut v2, 0, Duration::from_secs(5), |e| {
            peer_code(e).is_some()
        });
        assert_eq!(peer_code(&evs).as_deref(), Some(OFFER_CODE));
        assert_eq!(wait_failure(&mut v2), Failure::Expired);
        let evs = run_for(&mut host, 0, Duration::from_millis(300));
        assert!(evs.is_empty(), "host に届いた: {evs:?}");
        // 3人目が答えればhostはその返答を受けて終わる。
        let mut v3 = RoomConn::start(&opts, &w).unwrap();
        wait_state(&mut v3, State::NeedOwnCode);
        v3.submit_code(ANSWER_CODE).unwrap();
        let evs = pump(&mut host, 0, Duration::from_secs(5), |e| {
            e.contains(&Event::Done)
        });
        assert_eq!(peer_code(&evs).as_deref(), Some(ANSWER_CODE));
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
        // サーバーは閉じる前にerrorの文を送る。
        // 文を省いて閉じる番号だけでも同じ結果になること（番号の受け渡しを文が隠さないこと）を両方の場合で検証する。
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

    /// 対戦のルームで成立させ、2人とも保持状態にする。
    fn held_pair(f: &FakeServer, room: &str) -> (RoomConn, RoomConn) {
        let mut a = start(f, room);
        wait_state(&mut a, State::NeedOwnCode);
        a.submit_code(OFFER_CODE).unwrap();
        let mut b = start(f, room);
        wait_state(&mut b, State::NeedOwnCode);
        b.submit_code(ANSWER_CODE).unwrap();
        wait_state(&mut a, State::Holding);
        wait_state(&mut b, State::Holding);
        (a, b)
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_held_room_whose_sign_goes_stale_is_emptied_without_failing_either_side() {
        let f = FakeServer::start();
        f.knobs.held_stale_ms.store(400, Ordering::Release);
        let (mut a, mut b) = held_pair(&f, "stale-room");
        // aだけがpingを送り続ける（時計を30秒ずつ進めて渡す）。bは送らない。
        let t0 = Instant::now();
        let (mut evs_a, mut evs_b) = (Vec::new(), Vec::new());
        let mut k = 0;
        while !(a.is_finished() && b.is_finished()) {
            k += 1;
            let mut now = a.now();
            now.ms += k * PING_INTERVAL_MS;
            evs_a.extend(a.poll_at(now));
            evs_b.extend(b.poll());
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "閉じない: {evs_a:?} {evs_b:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !has_failed(&evs_a) && !has_failed(&evs_b),
            "{evs_a:?} {evs_b:?}"
        );
        assert!(evs_a.contains(&Event::Done) && evs_b.contains(&Event::Done));
        let pings = f.texts_matching(|t| t == higanr_core::room::PING_TEXT);
        assert!(pings >= 2, "a が ping を送っていない（{pings} 通）");
        // 部屋は空いた。
        let mut next = start(&f, "stale-room");
        let evs = wait_state(&mut next, State::NeedOwnCode);
        assert!(joined_as(&evs, Role::Offer), "{evs:?}");
        for c in [&mut a, &mut b, &mut next] {
            c.cancel();
            assert_joins(c, Duration::from_secs(3));
        }
    }

    #[test]
    #[ignore = "loopback の実時間の検査。全件の回（build.bat）で --ignored で走る"]
    fn a_held_room_outlives_the_room_and_answer_deadlines() {
        let f = FakeServer::start();
        f.knobs.room_ttl_ms.store(300, Ordering::Release);
        f.knobs.answer_ttl_ms.store(200, Ordering::Release);
        let (mut a, mut b) = held_pair(&f, "long-room");
        std::thread::sleep(Duration::from_millis(600));
        let evs: Vec<Event> = [a.poll(), b.poll()].concat();
        assert!(evs.is_empty(), "{evs:?}");
        assert_eq!((a.state(), b.state()), (State::Holding, State::Holding));
        // 返す側が手放しても始める側は失敗にせず終える。
        b.cancel();
        let evs = pump(&mut a, 0, Duration::from_secs(5), |e| {
            e.contains(&Event::Done)
        });
        assert!(!has_failed(&evs), "{evs:?}");
        assert_joins(&mut a, Duration::from_secs(3));
        assert_joins(&mut b, Duration::from_secs(3));
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

        // サーバーが閉じた直後（監視のスレッドがまだ読んでいない間）にコードを出す。
        // 送らずにNotSentを返し、失敗は「切れた」でなく期限（届いていた番号の方）で返す。
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

        // 何も答えないサーバーの場合。受信はその場で解け、閉じるスレッドは閉じる待ち時間（検査では500 ms）で終わる。
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

        // 接続の途中（TCPは受けるがHTTPに答えない）の場合。応答の先頭の待ち時間の上限で終わる（取りやめでは解けない。監視のスレッドは待たない）。
        // 上限は300 msに設定し、WinHTTPの定期処理の間隔（約4秒）の分を見込む。相手が切る9秒より前になる。
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

    /// 1本だけ受ける生のTCPの相手。要求の先頭を読み`reply`があれば書き、`hold`の間保持してから切る。
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

    // ---- WinHTTPの振る舞い ----

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
        // WinHTTPのタイムアウトは粗い間隔（約4秒）の定期処理で調べるので、300 msを設定しても切れるのは最大4秒余り後になる。
        // 検証するのは、相手が切る（8秒）より前に切れたことと、WebSocketの受信がその間隔を越えても戻らないこと。
        // 3つは同時に走らせる（待ちを重ねない）。
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
        // 効かない側の検証。同じ設定でUpgradeしたWebSocketの受信は文が来なければ間隔を越えても戻らない。
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

    /// WineのwinhttpはWebSocketのハンドルのcloseの待ち時間の設定を12009で断る。それでも開けて送受信も閉じる処理もできる。
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

        // 通常の経路は設定できる。印は付かない。
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
        // 終わった後に閉じる番号が来てもイベントは増えない。
        assert!(a.poll().is_empty() && b.poll().is_empty());
        assert_joins(&mut a, Duration::from_secs(3));
        assert_joins(&mut b, Duration::from_secs(3));
        // 割り当てられたルームには今のルームの手順で入り、先に着いた方が始める側になる。
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
        // 同じ番号の人が来ればその人とペアになる。
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
        // 取りやめた人は待ち列に残らない。次に来た人はペアにならない。
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
        // ルームへの接続も同じカウントに入る（待ち列の1回で上限）。
        let mut r = start(&f, "limit-shared");
        assert_eq!(
            wait_failure(&mut r),
            Failure::RateLimited {
                retry_after_secs: f.rules().retry_after_secs as u32
            }
        );
        // queueが来ない・pongが来ない。
        let f = FakeServer::start();
        f.knobs.no_room.store(true, Ordering::Release);
        let mut c = start_queue(&f, 1);
        // 101を受けてから時計を進める（受ける前に進めると期限が進めた時計から数え始まる）。
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
