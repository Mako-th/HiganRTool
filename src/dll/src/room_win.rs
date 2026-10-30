//! ルームサーバーへのWebSocket通信（WinHTTPの同期モード）。unsafeはここだけで、1つずつ`// SAFETY:`を付ける。
//! 手順の判断はcoreの`room::RoomClient`が持つ。スレッドの分け方は`net_room.rs`にある。ここはWinHTTPの関数をRustの型で包むだけ。
//!
//! WinHTTPを使うのは、OSのTLS（Schannel）とプロキシの設定（`WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`）をそのまま使えて、クレートを足さずに済むため。
//! 必要なのはwindows-sysの機能`Win32_Networking_WinHttp`だけ。WebSocketの関数はWindows 8から使える。
//! 手順は`WinHttpOpen` → `WinHttpConnect` → `WinHttpOpenRequest`（`wss`は`WINHTTP_FLAG_SECURE`）→ `WinHttpSetOption(WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET)` → `WinHttpSendRequest` → `WinHttpReceiveResponse`。
//! ステータスコードが101なら`WinHttpWebSocketCompleteUpgrade`を呼ぶ。101でなければ本文を上限まで読んで返す。
//!
//! 受信の上限（`WINHTTP_OPTION_RECEIVE_TIMEOUT`）は101の後のWebSocketの受信には効かない（Upgradeの前には効く）。
//! そのためWebSocketの受信はメッセージが来るまで止まったままになる。待ちを解くのは`close`だけで、止まっている受信は12017で戻る。
//! 時間切れは約4秒おきのポーリングで検出される。そのため設定した値より最大4秒ほど遅れて切れる（製品の10秒は10〜14秒。6回の測定からの推定）。
//! 別のスレッドからの`WinHttpCloseHandle`でも受信は戻るが採用しない。閉じたハンドルの値が使い回されない保証がWinHTTPに無いため。
//! ハンドルを閉じるのは両方のスレッドが手放した後の`Drop`の1回だけ。
//! ほかの実測（Windows 11 Pro 10.0.26200、i686、loopback）は全件の検査を走らせるたびに`net_room::tests::winhttp_facts_*`が検証し直す。

use core::ffi::c_void;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE, WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE, WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
    WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts, WinHttpWebSocketClose,
    WinHttpWebSocketCompleteUpgrade, WinHttpWebSocketQueryCloseStatus, WinHttpWebSocketReceive,
    WinHttpWebSocketSend, WinHttpWebSocketShutdown,
};

/// 要求に名乗る名（User-Agent）。サーバーは見ない。
const USER_AGENT: &str = "HiganRTool";

/// 閉じる番号が無いときに返す番号（RFC 6455の「番号の無いclose」。`WINHTTP_WEB_SOCKET_EMPTY_CLOSE_STATUS`と同じ）。
pub const NO_STATUS: u16 = 1005;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proxy {
    /// OSの設定（WPAD・PACを含む）に従う。製品はこちら。
    Automatic,
    /// プロキシを使わない。検査（loopback）はこちら（自動探索が名前解決のために外へ出ないようにする）。
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `wss`（TLS）ならtrue。
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// `/v0/rooms/<部屋名>`。
    pub path: String,
}

/// 待ちの上限（ms）。Upgradeの後の受信には上限が無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// `WinHttpSetTimeouts`の3つ（名前解決・TCPの接続・送信）。
    pub resolve_ms: u32,
    pub connect_ms: u32,
    pub send_ms: u32,
    /// `WinHttpSetTimeouts`の4つ目（受信）。応答の先頭（101か拒否）を待つ上限と、101でないときの本文の受信1回の上限になる。
    pub response_ms: u32,
    /// `close`が相手の閉じ返しを待つ上限（単位はms）。
    pub close_wait_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Open,
    Timeouts,
    Connect,
    OpenRequest,
    UpgradeOption,
    SendRequest,
    ReceiveResponse,
    StatusCode,
    CompleteUpgrade,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// WinHTTPの関数が失敗した（名前解決できない・接続できない・時間切れ・TLSの誤り）。`code`は`GetLastError`の値。
    Transport { step: Step, code: u32 },
    /// 101でない応答。`body`は上限まで読んだ本文（UTF-8として読めない文字は置き換える）。
    Refused { status: u16, body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// テキストのメッセージ。上限を越えた分は捨てる。返すのは上限 + 1バイトまで（越えたことは長さで分かる。中を読むかどうかはcoreが決める）。
    Text(String),
    /// バイナリのメッセージ（中身は捨てる。サーバーは送らない）。
    Binary,
    /// 相手のcloseを受けた（番号は`close_status`）。
    Close,
}

struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void, step: Step) -> Result<Handle, OpenError> {
        if h.is_null() {
            Err(OpenError::Transport {
                step,
                code: last_error(),
            })
        } else {
            Ok(Handle(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0`はWinHTTPが返したnullでないハンドル。ここで1回だけ閉じる（`Handle`はコピーできない）。
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn last_error() -> u32 {
    // SAFETY: 引数の無い呼び出し。直前のWin32の呼び出しと同じスレッドで読む。
    unsafe { GetLastError() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn set_u32_option(h: *mut c_void, option: u32, value: u32) -> bool {
    // SAFETY: `h`は生きているWinHTTPのハンドル。値は4バイトのu32で呼び出しの間だけ読まれる。
    unsafe {
        WinHttpSetOption(
            h,
            option,
            (&raw const value).cast::<c_void>(),
            core::mem::size_of::<u32>() as u32,
        ) != 0
    }
}

/// 開いたWebSocket。送信（`send_text` / `shutdown` / `close`）と受信（`receive`）は別のスレッドから同時に呼んでよい。
/// `Arc`で両方のスレッドが持ち、最後に手放した側の`Drop`でハンドルを閉じる。
pub struct WebSocket {
    // 閉じる順は宣言の順（子から親へ）。
    ws: Handle,
    _connect: Handle,
    _session: Handle,
    /// closeの待ちの上限を設定できなかったときの`GetLastError`。
    /// Wineのwinhttpはこの設定を断る（12009）が通信はできる。
    /// そのため接続は捨てず、closeの待ちが既定の長さになるだけにする。
    close_timeout_error: Option<u32>,
}

/// WebSocketのハンドルへcloseの待ちの上限（ms）を設定する。
type CloseTimeoutSetter = fn(*mut c_void, u32) -> Result<(), u32>;

fn set_close_timeout(ws: *mut c_void, ms: u32) -> Result<(), u32> {
    if set_u32_option(ws, WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT, ms) {
        Ok(())
    } else {
        Err(last_error())
    }
}

// SAFETY: WinHTTPのハンドルはどのスレッドから使ってもよい。
// WebSocketのハンドルは送信の呼び出し1本と受信の呼び出し1本を同時に持てる（loopbackで確認）。
// 閉じるのは`Drop`の1回だけ。
unsafe impl Send for WebSocket {}
// SAFETY: 同上。`&self`のメソッドはどれもハンドルを閉じない。
// 同じ向きの呼び出しを同時にしないのは使う側が守る。
// 具体的には`net_room.rs`で、受信の呼び出しは受信スレッドだけ。送信の呼び出しは`send_lock`のロックで1本に絞る。
unsafe impl Sync for WebSocket {}

impl WebSocket {
    /// 接続してUpgradeする。`body_max`は101でないときに本文を読む上限（バイト）。ブロッキングなので受信スレッドからだけ呼ぶ。
    pub fn open(
        ep: &Endpoint,
        proxy: Proxy,
        t: Timeouts,
        body_max: usize,
    ) -> Result<WebSocket, OpenError> {
        Self::open_with(ep, proxy, t, body_max, set_close_timeout)
    }

    /// 検査だけ: closeの待ちの設定を差し替えて開く（Wineのように断られる場合を再現する）。
    #[cfg(test)]
    pub fn open_with_close_timeout(
        ep: &Endpoint,
        proxy: Proxy,
        t: Timeouts,
        body_max: usize,
        setter: fn(*mut c_void, u32) -> Result<(), u32>,
    ) -> Result<WebSocket, OpenError> {
        Self::open_with(ep, proxy, t, body_max, setter)
    }

    fn open_with(
        ep: &Endpoint,
        proxy: Proxy,
        t: Timeouts,
        body_max: usize,
        set_close_timeout: CloseTimeoutSetter,
    ) -> Result<WebSocket, OpenError> {
        let agent = wide(USER_AGENT);
        let access = match proxy {
            Proxy::Automatic => WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            Proxy::Direct => WINHTTP_ACCESS_TYPE_NO_PROXY,
        };
        // SAFETY: `agent`は呼び出しの間生きている0終わりのUTF-16。プロキシの名前と除外リストはnull。フラグ0は同期モード。
        let session = Handle::new(
            unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    access,
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                )
            },
            Step::Open,
        )?;
        let ms = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: 開いたセッションのハンドルに4つのmsを設定するだけ。
        let ok = unsafe {
            WinHttpSetTimeouts(
                session.0,
                ms(t.resolve_ms),
                ms(t.connect_ms),
                ms(t.send_ms),
                ms(t.response_ms),
            )
        };
        if ok == 0 {
            return Err(OpenError::Transport {
                step: Step::Timeouts,
                code: last_error(),
            });
        }
        let host = wide(&ep.host);
        // SAFETY: 開いたセッション・0で終わるホスト名（呼び出しの間生きている）・ポート番号。予約の引数は0。
        let connect = Handle::new(
            unsafe { WinHttpConnect(session.0, host.as_ptr(), ep.port, 0) },
            Step::Connect,
        )?;
        let verb = wide("GET");
        let path = wide(&ep.path);
        let flags = if ep.secure { WINHTTP_FLAG_SECURE } else { 0 };
        // SAFETY: 開いた接続・0終わりのHTTPメソッドとパス。バージョン・参照元・受け付けるタイプはnull（既定のHTTP/1.1）。
        let request = Handle::new(
            unsafe {
                WinHttpOpenRequest(
                    connect.0,
                    verb.as_ptr(),
                    path.as_ptr(),
                    core::ptr::null(),
                    core::ptr::null(),
                    core::ptr::null(),
                    flags,
                )
            },
            Step::OpenRequest,
        )?;
        // SAFETY: 開いた要求のハンドル。このフラグは値を持たない（null・長さ0。WinHTTPの決まり）。
        let ok = unsafe {
            WinHttpSetOption(
                request.0,
                WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET,
                core::ptr::null(),
                0,
            )
        };
        if ok == 0 {
            return Err(OpenError::Transport {
                step: Step::UpgradeOption,
                code: last_error(),
            });
        }
        // SAFETY: 追加するヘッダーも本文も無い要求を送る（null・長さ0）。コンテキストの値0は同期モードでは使われない。
        let ok = unsafe {
            WinHttpSendRequest(request.0, core::ptr::null(), 0, core::ptr::null(), 0, 0, 0)
        };
        if ok == 0 {
            return Err(OpenError::Transport {
                step: Step::SendRequest,
                code: last_error(),
            });
        }
        // SAFETY: 送った要求の応答を待つ。予約の引数はnull。
        let ok = unsafe { WinHttpReceiveResponse(request.0, core::ptr::null_mut()) };
        if ok == 0 {
            return Err(OpenError::Transport {
                step: Step::ReceiveResponse,
                code: last_error(),
            });
        }
        let status = query_status(&request)?;
        if status != 101 {
            return Err(OpenError::Refused {
                status,
                body: read_body(&request, body_max),
            });
        }
        // SAFETY: 101を受けた要求のハンドル。返るのは新しいWebSocketのハンドル（要求のハンドルとは別。要求はこの後で閉じてよい）。
        let ws = Handle::new(
            unsafe { WinHttpWebSocketCompleteUpgrade(request.0, 0) },
            Step::CompleteUpgrade,
        )?;
        drop(request);
        let close_timeout_error = set_close_timeout(ws.0, t.close_wait_ms).err();
        Ok(WebSocket {
            ws,
            _connect: connect,
            _session: session,
            close_timeout_error,
        })
    }

    /// closeの待ちの上限を設定できなかったときの番号（設定できていれば`None`）。
    pub fn close_timeout_error(&self) -> Option<u32> {
        self.close_timeout_error
    }

    pub fn send_text(&self, text: &str) -> Result<(), u32> {
        let len = u32::try_from(text.len()).map_err(|_| ERROR_TOO_LONG)?;
        // SAFETY: 開いたWebSocketのハンドル。`text`は呼び出しの間生きている`len`バイト（同期モードなので戻るまでにコピーし終える）。
        let r = unsafe {
            WinHttpWebSocketSend(
                self.ws.0,
                WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE,
                text.as_ptr().cast::<c_void>(),
                len,
            )
        };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    /// 1通受ける（届くまで、または受信の上限まで止まる）。`max`を越えたテキストは`max + 1`バイトまでを返して残りを読み捨てる。
    /// 受信スレッドからだけ呼ぶ。
    pub fn receive(&self, max: usize) -> Result<Received, u32> {
        let mut msg: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let mut read: u32 = 0;
            let mut ty: i32 = 0;
            // SAFETY: 開いたWebSocketのハンドル。`buf`は`buf.len()`バイトの書き込み可能な領域。`read`と`ty`は書き込み可能なu32 / i32。
            let r = unsafe {
                WinHttpWebSocketReceive(
                    self.ws.0,
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len() as u32,
                    &mut read,
                    &mut ty,
                )
            };
            if r != 0 {
                return Err(r);
            }
            let got = buf.get(..read as usize).unwrap_or(&[]);
            match ty {
                WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE
                | WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE => {
                    let room = (max + 1).saturating_sub(msg.len());
                    msg.extend_from_slice(got.get(..got.len().min(room)).unwrap_or(&[]));
                    if ty == WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE {
                        return Ok(Received::Text(String::from_utf8_lossy(&msg).into_owned()));
                    }
                }
                WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE => {}
                WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE => return Ok(Received::Binary),
                WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE => return Ok(Received::Close),
                // 知らない種類はバイナリと同じに扱う（中は読まない）。
                _ => return Ok(Received::Binary),
            }
        }
    }

    pub fn shutdown(&self, status: u16) -> Result<(), u32> {
        // SAFETY: 開いたWebSocketのハンドル。理由の文字列は付けない（null・長さ0）。
        let r = unsafe { WinHttpWebSocketShutdown(self.ws.0, status, core::ptr::null(), 0) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    /// 閉じる（closeメッセージを送って相手のcloseを待つ。待ちは`Timeouts::close_wait_ms`まで）。
    /// 別のスレッドで止まっている`receive`もこれで戻る。
    /// 待つことがあるため、監視のスレッドからは呼ばない。
    pub fn close(&self, status: u16) -> Result<(), u32> {
        // SAFETY: 開いたWebSocketのハンドル。理由の文字列は付けない（null・長さ0）。
        let r = unsafe { WinHttpWebSocketClose(self.ws.0, status, core::ptr::null(), 0) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    pub fn close_status(&self) -> u16 {
        let mut status: u16 = 0;
        let mut reason = [0u8; 123];
        let mut used: u32 = 0;
        // SAFETY: 開いたWebSocketのハンドル。`status`・`reason`・`used`は呼び出しの間生きている書き込み可能な領域。
        // 理由の領域は123バイト（理由の上限`WINHTTP_WEB_SOCKET_MAX_CLOSE_REASON_LENGTH`）。
        let r = unsafe {
            WinHttpWebSocketQueryCloseStatus(
                self.ws.0,
                &mut status,
                reason.as_mut_ptr().cast::<c_void>(),
                reason.len() as u32,
                &mut used,
            )
        };
        if r == 0 { status } else { NO_STATUS }
    }

    /// 検査だけ: WebSocketのハンドルへ受信の上限を設定しようとする（WinHTTPが断ることを検証する）。断られたらその番号。
    #[cfg(test)]
    pub fn try_set_receive_timeout(&self, ms: u32) -> Result<(), u32> {
        use windows_sys::Win32::Networking::WinHttp::WINHTTP_OPTION_RECEIVE_TIMEOUT;
        if set_u32_option(self.ws.0, WINHTTP_OPTION_RECEIVE_TIMEOUT, ms) {
            Ok(())
        } else {
            Err(last_error())
        }
    }
}

/// メッセージが長すぎてu32に収まらない（WinHTTPの番号でなく、こちらが決めた値。`ERROR_INSUFFICIENT_BUFFER`の値を借りる）。
const ERROR_TOO_LONG: u32 = 122;

fn query_status(request: &Handle) -> Result<u16, OpenError> {
    let mut status: u32 = 0;
    let mut len: u32 = core::mem::size_of::<u32>() as u32;
    // SAFETY: 応答を受けた要求のハンドル。数値として読むフラグなので書き込み先は4バイトの`status`（長さ`len`）。
    // 名前と添字はnull（決まったヘッダを読む）。
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            core::ptr::null(),
            (&raw mut status).cast::<c_void>(),
            &mut len,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(OpenError::Transport {
            step: Step::StatusCode,
            code: last_error(),
        });
    }
    u16::try_from(status).map_err(|_| OpenError::Transport {
        step: Step::StatusCode,
        code: 0,
    })
}

/// 本文を`max`バイトまで読む（越えた分は読まない。読めなければそこまで）。
fn read_body(request: &Handle, max: usize) -> String {
    let mut body: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    while body.len() < max {
        let want = (max - body.len()).min(buf.len()) as u32;
        let mut read: u32 = 0;
        // SAFETY: 応答を受けた要求のハンドル。`buf`は`want`バイト以上の書き込み可能な領域。`read`は書き込み可能なu32。
        let ok = unsafe {
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast::<c_void>(),
                want,
                &mut read,
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
        body.extend_from_slice(buf.get(..read as usize).unwrap_or(&[]));
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// 検査だけ: SHA-1（偽サーバーが`Sec-WebSocket-Accept`を作るため）。暗号は自作せずOSのBCryptを呼ぶ。
#[cfg(test)]
pub fn sha1(data: &[u8]) -> Option<[u8; 20]> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA1_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 20];
    let len = u32::try_from(data.len()).ok()?;
    // SAFETY: 擬似ハンドル・鍵なし（null・0）・入力`data`（`len`バイト）・出力20バイト。どれも呼び出しの間生きている。
    // NTSTATUSは0以上が成功。
    let st = unsafe {
        BCryptHash(
            BCRYPT_SHA1_ALG_HANDLE,
            core::ptr::null(),
            0,
            data.as_ptr(),
            len,
            out.as_mut_ptr(),
            out.len() as u32,
        )
    };
    (st >= 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_rfc6455_example() {
        let got = sha1(b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11").unwrap();
        let want: [u8; 20] = [
            0xb3, 0x7a, 0x4f, 0x2c, 0xc0, 0x62, 0x4f, 0x16, 0x90, 0xf6, 0x46, 0x06, 0xcf, 0x38,
            0x59, 0x45, 0xb2, 0xbe, 0xc4, 0xea,
        ];
        assert_eq!(got, want);
    }
}
