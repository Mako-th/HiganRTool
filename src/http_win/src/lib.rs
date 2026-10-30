//! WinHTTPでGETを1回行う（更新のマニフェストとファイルを取得する）。ローダー（起動前の更新）とDLL（遊んでいる間の新バージョンの通知）が使う。
//! 判断は呼び出し側が行う（`higanr_core::update`とローダーの`update.rs`・DLLの`update_check.rs`）。OSのTLS・プロキシ設定をそのまま使う。
//! 302はWinHTTPがリダイレクトを追う（httpsからhttpへの転送は断る）。本文は`max`バイトまで（ネットから来る長さを信じない）。
//! `Content-Length`と受けた長さが違えば`Truncated`（WinHTTPは閉じられた接続を「読み終わり」と返すことがある）。
//! 待ち時間の上限は`Timeouts`の4つ（操作ごと）。全体の上限は呼び出し側が決める。

use core::ffi::c_void;

use higanr_core::lang::Lang;
use higanr_core::update::Endpoint;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};

/// 2つの書式のうち言語に合う方で`format!`する。
macro_rules! by_lang {
    ($lang:expr, ja: $ja:literal, en: $en:literal $(, $($arg:tt)*)?) => {
        match $lang {
            Lang::Ja => format!($ja $(, $($arg)*)?),
            Lang::En => format!($en $(, $($arg)*)?),
        }
    };
}

const USER_AGENT: &str = "HiganRTool-updater";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proxy {
    Automatic,
    /// 検査（loopback）用。自動の探索で外へ出ないように（製品は常にOSの設定に従う）。
    Direct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub resolve_ms: u32,
    pub connect_ms: u32,
    pub send_ms: u32,
    pub receive_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Transport { step: &'static str, code: u32 },
    Status(u16),
    TooLarge { max: usize },
    Truncated { got: usize, want: u64 },
}

impl HttpError {
    /// 更新のウィンドウや黒いウィンドウに出す文。
    pub fn describe(&self, lang: Lang) -> String {
        match self {
            HttpError::Transport { step, code } => by_lang!(
                lang,
                ja: "{step} が失敗（GetLastError {code}）",
                en: "{step} failed (GetLastError {code})"
            ),
            HttpError::Status(s) => {
                by_lang!(lang, ja: "サーバーの答えが {s}", en: "The server answered {s}")
            }
            HttpError::TooLarge { max } => by_lang!(
                lang,
                ja: "{max} バイトを越える応答",
                en: "A response larger than {max} bytes"
            ),
            HttpError::Truncated { got, want } => by_lang!(
                lang,
                ja: "途中で切れた（{want} バイトのはずが {got} バイト）",
                en: "The response was cut off ({got} bytes received, {want} expected)"
            ),
        }
    }
}

struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void, step: &'static str) -> Result<Handle, HttpError> {
        if h.is_null() {
            Err(HttpError::Transport {
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

/// ヘッダが無いか読み取れなければNone。
fn query_number(request: &Handle, which: u32) -> Option<u32> {
    let mut value: u32 = 0;
    let mut len: u32 = core::mem::size_of::<u32>() as u32;
    // SAFETY: 応答を受けた要求のハンドル。書き先は4バイトの`value`（長さ`len`）。
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.0,
            which | WINHTTP_QUERY_FLAG_NUMBER,
            core::ptr::null(),
            (&raw mut value).cast::<c_void>(),
            &mut len,
            core::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(value)
}

/// `progress`には本文を読み進めるたびにそこまでに受け取ったバイト数を渡す（呼ぶのはこのスレッド）。
pub fn get(
    ep: &Endpoint,
    proxy: Proxy,
    t: Timeouts,
    max: usize,
    progress: &mut dyn FnMut(usize),
) -> Result<Vec<u8>, HttpError> {
    let agent = wide(USER_AGENT);
    let access = match proxy {
        Proxy::Automatic => WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
        Proxy::Direct => WINHTTP_ACCESS_TYPE_NO_PROXY,
    };
    // SAFETY: `agent`は終端0のUTF-16で生きている。プロキシの名と除外はnull。
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
        "WinHttpOpen",
    )?;
    let ms = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
    // SAFETY: 開いたセッションのハンドルに4つのmsを設定するだけ。
    let ok = unsafe {
        WinHttpSetTimeouts(
            session.0,
            ms(t.resolve_ms),
            ms(t.connect_ms),
            ms(t.send_ms),
            ms(t.receive_ms),
        )
    };
    if ok == 0 {
        return Err(HttpError::Transport {
            step: "WinHttpSetTimeouts",
            code: last_error(),
        });
    }
    let host = wide(&ep.host);
    // SAFETY: 開いたセッション・終端0のホスト名・ポート番号。
    let connect = Handle::new(
        unsafe { WinHttpConnect(session.0, host.as_ptr(), ep.port, 0) },
        "WinHttpConnect",
    )?;
    let verb = wide("GET");
    let path = wide(&ep.path);
    let flags = if ep.secure { WINHTTP_FLAG_SECURE } else { 0 };
    // SAFETY: 開いた接続・終端0のHTTPメソッドとパス。バージョン・参照元・受ける型はnull。
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
        "WinHttpOpenRequest",
    )?;
    // SAFETY: 追加するヘッダーも本文も無い要求を送る（null・長さ0）。
    let ok =
        unsafe { WinHttpSendRequest(request.0, core::ptr::null(), 0, core::ptr::null(), 0, 0, 0) };
    if ok == 0 {
        return Err(HttpError::Transport {
            step: "WinHttpSendRequest",
            code: last_error(),
        });
    }
    // SAFETY: 送った要求の応答を待つ。予約の引数はnull。
    let ok = unsafe { WinHttpReceiveResponse(request.0, core::ptr::null_mut()) };
    if ok == 0 {
        return Err(HttpError::Transport {
            step: "WinHttpReceiveResponse",
            code: last_error(),
        });
    }
    let status =
        query_number(&request, WINHTTP_QUERY_STATUS_CODE).ok_or_else(|| HttpError::Transport {
            step: "WinHttpQueryHeaders(status)",
            code: last_error(),
        })?;
    if status != 200 {
        return Err(HttpError::Status(u16::try_from(status).unwrap_or(u16::MAX)));
    }
    let declared = query_number(&request, WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);
    if declared.is_some_and(|d| d > max as u64) {
        return Err(HttpError::TooLarge { max });
    }
    let mut body: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let mut read: u32 = 0;
        // SAFETY: `buf`は`buf.len()`バイト書き込み可能な領域。`read`は書き込み可能なu32。
        let ok = unsafe {
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u32,
                &mut read,
            )
        };
        if ok == 0 {
            // 途中で切られた接続はここで失敗として返ることがある（読み終わりと区別して「切れた」として扱う）。
            return match declared {
                Some(want) => Err(HttpError::Truncated {
                    got: body.len(),
                    want,
                }),
                None => Err(HttpError::Transport {
                    step: "WinHttpReadData",
                    code: last_error(),
                }),
            };
        }
        if read == 0 {
            break;
        }
        body.extend_from_slice(buf.get(..read as usize).unwrap_or(&[]));
        if body.len() > max {
            return Err(HttpError::TooLarge { max });
        }
        progress(body.len());
    }
    if let Some(want) = declared
        && body.len() as u64 != want
    {
        return Err(HttpError::Truncated {
            got: body.len(),
            want,
        });
    }
    Ok(body)
}
