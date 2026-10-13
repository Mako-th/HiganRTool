//! 繋がった知らせ（音とタスクバー点滅。前面は奪わない）の Win32 の層。
//!
//! 接続コードの見張りのスレッド（`net_code` の `higanr-code`）が、繋がったのを見た 1 回だけ `connected` を呼ぶ。ゲームのスレッドでは呼ばない。
//! - 自分のプロセスの見えている一番上のウィンドウ（ゲームのウィンドウ）を探し、前面に無ければ `FlashWindowEx` で点滅させる（前面に来たら止まる）。
//!   前面は奪わない。
//! - `higanr_core::chime` で合成した音を `PlaySoundW` で鳴らす（メモリから、鳴り終わるまで待つ。約 0.9 秒。待つのは見張りのスレッドだけ）。
//!
//! 他のプロセスのウィンドウには触らない。

use core::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, FlashWindowEx, GW_OWNER,
    GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsWindowVisible,
};
use windows_sys::core::BOOL;

struct Search {
    pid: u32,
    found: HWND,
}

unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` は `game_window` が渡した `&mut Search` で、`EnumWindows` の中で同期に呼ばれる間は生きている。
    let s = unsafe { &mut *(lparam as *mut Search) };
    let mut pid = 0u32;
    // SAFETY: `hwnd` は `EnumWindows` が渡したウィンドウ。`pid` は書ける u32。ウィンドウが消えていても 0 が返るだけ。
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != s.pid {
        return 1;
    }
    // SAFETY: 読むだけの問い合わせ。ウィンドウが消えていれば 0 / NULL が返るだけ。
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    // SAFETY: 同上。
    let owned = !unsafe { GetWindow(hwnd, GW_OWNER) }.is_null();
    if visible && !owned {
        s.found = hwnd;
        return 0;
    }
    1
}

/// 自分のプロセスの、見えていて持ち主の無い一番上のウィンドウ（花映塚ではゲームのウィンドウ）。無ければ None。`text_input_win` も同じウィンドウを探す。
pub(crate) fn game_window() -> Option<HWND> {
    let mut s = Search {
        // SAFETY: 引数の無い問い合わせ。
        pid: unsafe { GetCurrentProcessId() },
        found: core::ptr::null_mut(),
    };
    // SAFETY: `visit` は署名どおりの呼び戻しで、`s` はこの呼び出しの間だけ生きている。途中で止めても戻り値は見ない（`found` で決める）。
    unsafe { EnumWindows(Some(visit), &mut s as *mut Search as LPARAM) };
    (!s.found.is_null()).then_some(s.found)
}

/// ウィンドウを点滅させる（前面に来るまで）。前面にあれば点滅しない。本ログへの 1 句を返す。
fn flash() -> &'static str {
    let Some(hwnd) = game_window() else {
        return "ゲームのウィンドウが見つからないので点滅しなかった";
    };
    // SAFETY: 読むだけの問い合わせ。
    if unsafe { GetForegroundWindow() } == hwnd {
        return "ゲームのウィンドウは前面にあるので点滅しなかった";
    }
    let info = FLASHWINFO {
        cbSize: size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: `info` は大きさを入れた FLASHWINFO。戻り値は「前に点滅していたか」で失敗の印ではない。前面は変えない。
    unsafe { FlashWindowEx(&info) };
    "ゲームのウィンドウを点滅させた（前面に来たら止まる）"
}

fn chime() -> &'static str {
    let wav = higanr_core::chime::connected_wav();
    // SAFETY: `SND_MEMORY` なので 1 つ目の引数は WAV の先頭（`SND_SYNC` で戻るのは鳴り終わった後なので `wav` は生きている）。
    // `SND_NODEFAULT` なので読めなくても既定の音は鳴らさない。
    let ok = unsafe {
        PlaySoundW(
            wav.as_ptr().cast::<u16>(),
            core::ptr::null_mut::<c_void>(),
            SND_MEMORY | SND_SYNC | SND_NODEFAULT,
        )
    } != 0;
    if ok {
        "音を鳴らした"
    } else {
        "音を鳴らせなかった（PlaySoundW が 0 を返した）"
    }
}

/// 繋がった知らせ（点滅 → 音）。本ログの「ネットワークの外」の行へ足す文を返す。見張りのスレッドから 1 回だけ呼ぶ。
pub fn connected() -> String {
    let f = flash();
    let c = chime();
    format!("知らせ: {f} / {c}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_without_windows_finds_none() {
        assert_eq!(game_window(), None);
    }
}
