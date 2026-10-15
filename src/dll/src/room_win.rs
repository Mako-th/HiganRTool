//! ルームサーバーへの WebSocket の運び（WinHTTP の同期の形）。unsafe はここだけ（1 つずつ `// SAFETY:`）。手順の判断は core の
//! `room::RoomClient`、スレッドの分け方は `net_room.rs`。ここは WinHTTP の関数を Rust の型で包むだけ。
//!
//! WinHTTP を使うのは、OS の TLS（Schannel）とプロキシの設定（`WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`）をそのまま使え、クレートを足さずに
//! 済むため（windows-sys の機能 `Win32_Networking_WinHttp` だけ。WebSocket の関数は Windows 8 から）。手順は `WinHttpOpen` →
//! `WinHttpConnect` → `WinHttpOpenRequest`（`wss` は `WINHTTP_FLAG_SECURE`）→ `WinHttpSetOption(WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET)` →
//! `WinHttpSendRequest` → `WinHttpReceiveResponse` → 状態の番号が 101 なら `WinHttpWebSocketCompleteUpgrade`、101 でなければ本文を上限まで読んで返す。
//!
//! loopback で測ったこと（Windows 11 Pro 10.0.26200、i686。`net_room::tests::winhttp_facts_*` が毎回の全件の回で確かめ直す）:
//! - 受信の上限（`WINHTTP_OPTION_RECEIVE_TIMEOUT`）は 101 の後の WebSocket の受信に効かない（セッション・要求のどちらに置いても戻らず、
//!   WebSocket のハンドルへ置くと 12018 で断られる）。Upgrade の前には効く（応答の先頭を待つ `WinHttpReceiveResponse` と、101 でない応答の
//!   本文の受信）。なので WebSocket の受信は文が来るまで止まったままで、待ちを解くのは `close` だけ。
//! - `WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT` は応答の先頭の待ちに効かなかったので置かない。
//! - 時間切れは約 4 秒おきに見回られる（6 回の測りからの推定）ため、置いた値より最大 4 秒ほど遅れて切れる（製品の 10 秒は 10〜14 秒）。
//! - 受信で止まっている間に、別のスレッドから `send_text`・`shutdown`・`close` を呼べる。`shutdown` の後に相手が閉じ返せば、
//!   止まっている受信が close を受けて戻る。
//! - `close` を呼ぶと止まっている受信はその場で 12017 で戻る。`close` 自身は、相手が閉じ返せば約 0.2 ms、答えなければ
//!   `WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT`（単位は ms）の後に 12002 で戻る。
//! - 番号の無い close を受けると `WinHttpWebSocketQueryCloseStatus` は 1005 を返す。
//! - 別のスレッドからの `WinHttpCloseHandle` でも受信は戻るが採らない。閉じたハンドルの値が使い回されない保証が WinHTTP に無く、
//!   受け側が閉じたかを見てから呼ぶまでの隙も消せない。受信を解くのは `close`、ハンドルを閉じるのは両方のスレッドが手放した後の `Drop` の 1 回だけ。

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

/// 閉じる番号が無いときに返す番号（RFC 6455 の「番号の無い close」。`WINHTTP_WEB_SOCKET_EMPTY_CLOSE_STATUS` と同じ）。
pub const NO_STATUS: u16 = 1005;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proxy {
    /// OS の設定（WPAD・PAC を含む）に従う。製品はこちら。
    Automatic,
    /// プロキシを使わない。検査（loopback）はこちら（自動の探索で名前を引きに外へ出ないように）。
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `wss`（TLS）なら true。
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// `/v0/rooms/<部屋名>`。
    pub path: String,
}

/// 待ちの上限（ms）。Upgrade の後の受信には上限が無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// `WinHttpSetTimeouts` の 3 つ（名前の解決・TCP の接続・送り）。
    pub resolve_ms: u32,
    pub connect_ms: u32,
    pub send_ms: u32,
    /// `WinHttpSetTimeouts` の 4 つ目（受信）。応答の先頭（101 か断り）を待つ上限と、101 でないときの本文の受信 1 回の上限になる。
    pub response_ms: u32,
    /// `close` が相手の閉じ返しを待つ上限（単位は ms）。
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
    /// WinHTTP の関数が失敗した（名前が引けない・繋がらない・時間切れ・TLS の誤り）。`code` は `GetLastError`。
    Transport { step: Step, code: u32 },
    /// 101 でない応答。`body` は上限まで読んだ本文（UTF-8 として読めない文字は置き換える）。
    Refused { status: u16, body: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// テキストの文。上限を越えた分は捨て、上限 + 1 バイトまでを返す（越えたことは長さで分かる。中を読むかは core が決める）。
    Text(String),
    /// バイナリの文（中身は捨てた。サーバーは送らない）。
    Binary,
    /// 相手の close を受けた（番号は `close_status`）。
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
        // SAFETY: `self.0` は WinHTTP が返した null でないハンドルで、ここで 1 回だけ閉じる（`Handle` はコピーできない）。
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn last_error() -> u32 {
    // SAFETY: 引数の無い呼び出し。直前の Win32 の呼び出しと同じスレッドで読む。
    unsafe { GetLastError() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn set_u32_option(h: *mut c_void, option: u32, value: u32) -> bool {
    // SAFETY: `h` は生きている WinHTTP のハンドル。値は 4 バイトの u32 で、呼び出しの間だけ読まれる。
    unsafe {
        WinHttpSetOption(
            h,
            option,
            (&raw const value).cast::<c_void>(),
            core::mem::size_of::<u32>() as u32,
        ) != 0
    }
}

/// 開いた WebSocket。送り（`send_text` / `shutdown` / `close`）と受け（`receive`）は別のスレッドから同時に呼んでよい。
/// `Arc` で両方のスレッドが持ち、最後に手放した側の `Drop` でハンドルを閉じる。
pub struct WebSocket {
    // 閉じる順は宣言の順（子から親へ）。
    ws: Handle,
    _connect: Handle,
    _session: Handle,
    /// close の待ちの上限を置けなかったときの `GetLastError`。Wine の winhttp はこの設定を断る（12009）が通信はできるので、
    /// 接続は捨てず、close の待ちが既定の長さになるだけにする。
    close_timeout_error: Option<u32>,
}

/// WebSocket のハンドルへ close の待ちの上限（ms）を置く。
type CloseTimeoutSetter = fn(*mut c_void, u32) -> Result<(), u32>;

fn set_close_timeout(ws: *mut c_void, ms: u32) -> Result<(), u32> {
    if set_u32_option(ws, WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT, ms) {
        Ok(())
    } else {
        Err(last_error())
    }
}

// SAFETY: WinHTTP のハンドルはどのスレッドから使ってもよい。WebSocket のハンドルは送り 1 本と受け 1 本を同時に持てる
// （loopback で確認）。閉じるのは `Drop` の 1 回だけ。
unsafe impl Send for WebSocket {}
// SAFETY: 同上。`&self` のメソッドはどれもハンドルを閉じない。同じ向きの呼び出しを同時にしないのは使う側が守る
// （`net_room.rs`: 受けは受信スレッドだけ、送りの向きは錠 `send_lock` で 1 本に絞る）。
unsafe impl Sync for WebSocket {}

impl WebSocket {
    /// 繋いで Upgrade する。`body_max` は 101 でないときに本文を読む上限（バイト）。ブロッキングなので受信スレッドからだけ呼ぶ。
    pub fn open(
        ep: &Endpoint,
        proxy: Proxy,
        t: Timeouts,
        body_max: usize,
    ) -> Result<WebSocket, OpenError> {
        Self::open_with(ep, proxy, t, body_max, set_close_timeout)
    }

    /// 検査だけ: close の待ちの設定を差し替えて開く（Wine のように断られる場合を再現する）。
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
        // SAFETY: `agent` は呼び出しの間生きている 0 終わりの UTF-16。プロキシの名と除外は null、旗 0 は同期の形。
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
        // SAFETY: 開いたセッションのハンドルに 4 つの ms を置くだけ。
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
        // SAFETY: 開いたセッション・0 で終わる宿の名（呼び出しの間生きている）・番号。予約の引数は 0。
        let connect = Handle::new(
            unsafe { WinHttpConnect(session.0, host.as_ptr(), ep.port, 0) },
            Step::Connect,
        )?;
        let verb = wide("GET");
        let path = wide(&ep.path);
        let flags = if ep.secure { WINHTTP_FLAG_SECURE } else { 0 };
        // SAFETY: 開いた繋ぎ・0 終わりの動詞とパス。バージョン・参照元・受ける型は null（既定の HTTP/1.1）。
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
        // SAFETY: 開いた要求のハンドル。この旗は値を持たない（null・長さ 0。WinHTTP の決まり）。
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
        // SAFETY: 足すヘッダーも本文も無い要求を送る（null・長さ 0）。文脈の値 0 は同期の形では使われない。
        let ok = unsafe {
            WinHttpSendRequest(request.0, core::ptr::null(), 0, core::ptr::null(), 0, 0, 0)
        };
        if ok == 0 {
            return Err(OpenError::Transport {
                step: Step::SendRequest,
                code: last_error(),
            });
        }
        // SAFETY: 送った要求の応答を待つ。予約の引数は null。
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
        // SAFETY: 101 を受けた要求のハンドル。返るのは新しい WebSocket のハンドル（要求のハンドルとは別。要求はこの後で閉じてよい）。
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

    /// close の待ちの上限を置けなかったときの番号（置けていれば `None`）。
    pub fn close_timeout_error(&self) -> Option<u32> {
        self.close_timeout_error
    }

    pub fn send_text(&self, text: &str) -> Result<(), u32> {
        let len = u32::try_from(text.len()).map_err(|_| ERROR_TOO_LONG)?;
        // SAFETY: 開いた WebSocket のハンドル。`text` は呼び出しの間生きている `len` バイト（同期の形なので戻るまでにコピーし終える）。
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

    /// 1 通受ける（届くまで、または受信の上限まで止まる）。`max` を越えたテキストは `max + 1` バイトまでを返して残りを読み捨てる。
    /// 受信スレッドからだけ呼ぶ。
    pub fn receive(&self, max: usize) -> Result<Received, u32> {
        let mut msg: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let mut read: u32 = 0;
            let mut ty: i32 = 0;
            // SAFETY: 開いた WebSocket のハンドル。`buf` は `buf.len()` バイト書ける領域で、`read` と `ty` は書ける u32 / i32。
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
        // SAFETY: 開いた WebSocket のハンドル。理由の文は付けない（null・長さ 0）。
        let r = unsafe { WinHttpWebSocketShutdown(self.ws.0, status, core::ptr::null(), 0) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    /// 閉じる（close の文を送り、相手の close を待つ。待ちは `Timeouts::close_wait_ms` まで）。別のスレッドで止まっている `receive` もこれで戻る。
    /// 待ちうるので見張りのスレッドからは呼ばない。
    pub fn close(&self, status: u16) -> Result<(), u32> {
        // SAFETY: 開いた WebSocket のハンドル。理由の文は付けない（null・長さ 0）。
        let r = unsafe { WinHttpWebSocketClose(self.ws.0, status, core::ptr::null(), 0) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    pub fn close_status(&self) -> u16 {
        let mut status: u16 = 0;
        let mut reason = [0u8; 123];
        let mut used: u32 = 0;
        // SAFETY: 開いた WebSocket のハンドル。`status`・`reason`（123 バイト = 理由の上限
        // `WINHTTP_WEB_SOCKET_MAX_CLOSE_REASON_LENGTH`）・`used` は呼び出しの間生きている書ける領域。
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

    /// 検査だけ: WebSocket のハンドルへ受信の上限を置こうとする（WinHTTP が断ることを確かめる）。断られたらその番号。
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

/// 文が長すぎて u32 に収まらない（WinHTTP の番号でない、こちらの印。`ERROR_INSUFFICIENT_BUFFER` の値を借りる）。
const ERROR_TOO_LONG: u32 = 122;

fn query_status(request: &Handle) -> Result<u16, OpenError> {
    let mut status: u32 = 0;
    let mut len: u32 = core::mem::size_of::<u32>() as u32;
    // SAFETY: 応答を受けた要求のハンドル。数として読む旗なので、書き先は 4 バイトの `status`（長さ `len`）。名前と添字は
    // null（決まった頭を読む）。
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

/// 本文を `max` バイトまで読む（越えた分は読まない。読めなければそこまで）。
fn read_body(request: &Handle, max: usize) -> String {
    let mut body: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    while body.len() < max {
        let want = (max - body.len()).min(buf.len()) as u32;
        let mut read: u32 = 0;
        // SAFETY: 応答を受けた要求のハンドル。`buf` は `want` バイト以上書ける領域、`read` は書ける u32。
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

/// 検査だけ: SHA-1（偽サーバーが `Sec-WebSocket-Accept` を作るため）。暗号は自作せず OS の BCrypt を呼ぶ。
#[cfg(test)]
pub fn sha1(data: &[u8]) -> Option<[u8; 20]> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA1_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 20];
    let len = u32::try_from(data.len()).ok()?;
    // SAFETY: 擬似ハンドル・鍵なし（null・0）・入力 `data`（`len` バイト）・出力 20 バイト。どれも呼び出しの間生きている。
    // NTSTATUS は 0 以上が成功。
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
