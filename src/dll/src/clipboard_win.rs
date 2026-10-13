//! 本物のクリップボード（Win32）。ゲームの中のメニューが接続コードをコピーする・貼るときだけ、見張りのスレッドから呼ぶ。
//! unsafe はこのファイルだけ。形式は `CF_UNICODETEXT`（UTF-16。値 13）。ウィンドウの持ち主は付けない（`OpenClipboard(NULL)`）ので、
//! 他のプロセスのウィンドウ・入力・前面には触らない。
//!
//! 別のアプリが開いている間は `OpenClipboard` が失敗するので、数回だけ短く待ち直す。
//! 変換は pure な関数に分けて検査する。本物のクリップボードを呼ぶ検査は書かない（利用者のクリップボードを書き換えるため）。

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};

use higanr_core::code_files::CF_UNICODETEXT;

use crate::clipboard::{Clipboard, MAX_READ_CHARS};

/// `OpenClipboard` を試す回数と間隔。
const OPEN_TRIES: u32 = 10;
const OPEN_GAP: std::time::Duration = std::time::Duration::from_millis(10);

pub(crate) fn to_wide_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(core::iter::once(0)).collect()
}

/// 最初の 0 まで、`MAX_READ_CHARS` 単位まで。壊れた対は `U+FFFD`。
pub(crate) fn from_wide(units: &[u16]) -> String {
    let end = units
        .iter()
        .take(MAX_READ_CHARS)
        .position(|&u| u == 0)
        .unwrap_or(units.len().min(MAX_READ_CHARS));
    String::from_utf16_lossy(units.get(..end).unwrap_or(&[]))
}

struct Open;

impl Open {
    fn new() -> Result<Open, String> {
        for i in 0..OPEN_TRIES {
            // SAFETY: ウィンドウの持ち主なし（NULL）の呼び出し。成功したら `Drop` が閉じる。
            if unsafe { OpenClipboard(core::ptr::null_mut()) } != 0 {
                return Ok(Open);
            }
            if i + 1 < OPEN_TRIES {
                std::thread::sleep(OPEN_GAP);
            }
        }
        Err("クリップボードを開けなかった（別のアプリが使っている）".to_string())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: `Open::new` の成功後にだけ作られ、1 つにつき 1 回だけ閉じる。
        unsafe { CloseClipboard() };
    }
}

pub struct SystemClipboard;

impl Clipboard for SystemClipboard {
    fn read(&mut self) -> Result<Option<String>, String> {
        let _open = Open::new()?;
        // SAFETY: 形式の有無を読むだけ。クリップボードは開いている。
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return Ok(None);
        }
        // SAFETY: 開いている間に取ったハンドルで、持ち主はクリップボードのまま（解放も保存もしない）。
        let h = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if h.is_null() {
            return Ok(None);
        }
        // SAFETY: `h` は `CF_UNICODETEXT` の全域メモリ。`GlobalLock` は失敗で NULL、`GlobalSize` はバイト数。
        let p = unsafe { GlobalLock(h) }.cast::<u16>();
        if p.is_null() {
            return Err(
                "クリップボードの文字列を固定できなかった（GlobalLock が NULL）".to_string(),
            );
        }
        // SAFETY: 同上。
        let bytes = unsafe { GlobalSize(h) };
        let units = (bytes / 2).min(MAX_READ_CHARS + 1);
        // SAFETY: `GlobalSize` の範囲内を固定している間だけ読み、`from_wide` が自分のヒープへコピーする。
        // UTF-16 の並びは 2 バイト境界（全域メモリは 8 バイト境界）。
        let text = from_wide(unsafe { core::slice::from_raw_parts(p, units) });
        // SAFETY: 上で固定した取っ手を 1 回だけ解く。
        unsafe { GlobalUnlock(h) };
        Ok((!text.is_empty()).then_some(text))
    }

    fn write(&mut self, text: &str) -> Result<(), String> {
        let wide = to_wide_nul(text);
        let bytes = wide.len() * 2;
        let _open = Open::new()?;
        // SAFETY: 移動可能な全域メモリを取る。成功したら `SetClipboardData` へ渡し、渡せないときだけ `GlobalFree` する。
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if h.is_null() {
            return Err(
                "クリップボードに置く入れ物を取れなかった（GlobalAlloc が NULL）".to_string(),
            );
        }
        // SAFETY: 今取った `h` を固定する。
        let p = unsafe { GlobalLock(h) }.cast::<u16>();
        if p.is_null() {
            // SAFETY: 誰にも渡していない `h` を返す。
            unsafe { GlobalFree(h) };
            return Err(
                "クリップボードに置く入れ物を固定できなかった（GlobalLock が NULL）".to_string(),
            );
        }
        // SAFETY: `p` は `bytes` バイト書ける。`wide` と重ならない。
        unsafe { core::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len()) };
        // SAFETY: 固定した `h` を解く。
        unsafe { GlobalUnlock(h) };
        // SAFETY: クリップボードは開いている。持ち主なしなので今の中身を空にするだけ。
        if unsafe { EmptyClipboard() } == 0 {
            // SAFETY: 渡していない `h` を返す。
            unsafe { GlobalFree(h) };
            return Err("クリップボードを空にできなかった（EmptyClipboard が 0）".to_string());
        }
        // SAFETY: `h` は `CF_UNICODETEXT` の形（0 で終わる UTF-16）を入れた全域メモリ。成功したら持ち主はクリップボードになり、こちらは触らない。
        // 失敗したらこちらのものなので返す。
        let set = unsafe { SetClipboardData(CF_UNICODETEXT, h) };
        if set.is_null() {
            // SAFETY: 渡せなかった `h` を返す。
            unsafe { GlobalFree(h) };
            return Err(
                "クリップボードへコピーできなかった（SetClipboardData が NULL）".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_text_round_trips_and_stops_at_the_nul() {
        for t in ["", "abc-DEF_123", "相手の接続コード", "a😀b"] {
            let w = to_wide_nul(t);
            assert_eq!(*w.last().unwrap(), 0, "末尾に 0");
            assert_eq!(from_wide(&w), t);
        }
        let mut w = to_wide_nul("ab");
        w.extend(to_wide_nul("後ろ"));
        assert_eq!(from_wide(&w), "ab", "最初の 0 まで");
        let long: Vec<u16> = vec![u16::from(b'x'); MAX_READ_CHARS * 2];
        assert_eq!(
            from_wide(&long).len(),
            MAX_READ_CHARS,
            "0 が無くても上限で切る"
        );
        assert_eq!(from_wide(&[0xD800, u16::from(b'a'), 0]), "\u{FFFD}a");
        assert_eq!(from_wide(&[]), "");
    }
}
