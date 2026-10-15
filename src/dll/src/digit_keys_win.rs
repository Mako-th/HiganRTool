//! Ctrl+上段の数字（0〜9）・Ctrl+Tab・Ctrl+C のキーの読み（`GetAsyncKeyState` の層。unsafe はこの中だけ）。押した瞬間の取り方は `digit_keys.rs`、
//! 読んでよい間の判断は `net_game::NetGame::digit_keys`。
//!
//! **読むだけ**: OS への入力の注入も、前面を奪う呼び出しもしない。`GetAsyncKeyState` はキーが今押されているかの問い合わせで、
//! ゲームの入力の関数が読む表（`GetKeyboardState`）とは別の経路。IME が開いていてもキーそのものの状態を返す（公開資料の仕様から。実機では確かめていない）。
//! 1 tick に 1 回、12 回の問い合わせ（数字 10 個 ＋ Tab ＋ C。どれかが押されている tick だけ、Ctrl・Alt・左右の Win の 4 回が足される）。
//! ゲームのスレッドで、確保しない。

use crate::digit_keys::{ALL_HELD, C_BIT, CTRL_BIT, MOD_BIT, TAB_BIT};

/// 検査用の偽の読み: `-1` は「読んではいけない間」（`held` が `None`）、それ以外は押している集合。
#[cfg(test)]
static TEST_HELD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(test)]
pub(crate) fn test_set_held(h: Option<u16>) {
    TEST_HELD.store(h.map_or(-1, i32::from), std::sync::atomic::Ordering::SeqCst);
}

/// 検査用の偽の前面: 既定は「ゲームのウィンドウが前面」。
#[cfg(test)]
static TEST_FOREGROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[cfg(test)]
pub(crate) fn test_set_foreground(mine: bool) {
    TEST_FOREGROUND.store(mine, std::sync::atomic::Ordering::SeqCst);
}

/// Windows の前面のウィンドウがこのプロセス（ゲーム）のウィンドウか。ほかのアプリが前面なら false、前面のウィンドウが無いときも false。
/// HWND でなく持ち主のプロセス ID で比べる: `notify_win::game_window` は `EnumWindows` で全部のウィンドウを舐めるので 1 tick ごとには
/// 重く、`text_input_win` の差し替えたウィンドウは最初の文字の入力まで無い。ゲームの主ウィンドウは 1 つなので、同じプロセスのウィンドウが前面＝
/// ゲームのウィンドウが前面とみなす。読むだけで、前面は変えない。
#[cfg(not(test))]
pub fn foreground_is_game() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    // SAFETY: 引数の無い読み取りの問い合わせ。前面のウィンドウが消えていれば NULL が返るだけ。
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_null() {
        return false;
    }
    let mut pid = 0u32;
    // SAFETY: `fg` は今取ったウィンドウ（消えていても 0 が返るだけ）。`pid` は書ける u32。
    let tid = unsafe { GetWindowThreadProcessId(fg, &mut pid) };
    // SAFETY: 引数の無い問い合わせ。
    tid != 0 && pid == unsafe { GetCurrentProcessId() }
}

#[cfg(test)]
pub fn foreground_is_game() -> bool {
    TEST_FOREGROUND.load(std::sync::atomic::Ordering::SeqCst)
}

/// 仮想キー C（文字キーの仮想キーコードは大文字の ASCII）。
const VK_C: u16 = 0x43;

/// 仮想キーが今押されているか（`shot_key_win` の P・Home の読みも使う）。
#[cfg(not(test))]
pub(crate) fn key_down(vk: u16) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: 引数は仮想キーコードだけの読み取りの問い合わせ。ポインタも所有権も渡さず、状態を変えない。
    let s = unsafe { GetAsyncKeyState(i32::from(vk)) };
    // 最上位ビットが「今押されている」。
    s < 0
}

/// 数字キー 0〜9・Tab・C のうち今押されているものの集合（bit `d` = 数字 `d`、bit10 = Tab、bit13 = C）。どれかが押されているときだけ、
/// Ctrl（bit12）と Alt・Win のどれか（bit11）も読む（Ctrl+数字・Ctrl+Tab・Ctrl+C だけを取り、Alt+Tab などを取らない）。
/// `down` はキー（仮想キーコード）が今押されているかの問い合わせ（本物は `key_down`、検査は偽物）。
fn held_with(mut down: impl FnMut(u16) -> bool) -> u16 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_TAB,
    };
    let mut set = 0u16;
    for d in 0..10u16 {
        if down(0x30 + d) {
            set |= 1 << d;
        }
    }
    if down(VK_TAB) {
        set |= TAB_BIT;
    }
    if down(VK_C) {
        set |= C_BIT;
    }
    if set != 0 {
        if down(VK_CONTROL) {
            set |= CTRL_BIT;
        }
        if [VK_MENU, VK_LWIN, VK_RWIN].into_iter().any(down) {
            set |= MOD_BIT;
        }
    }
    set & ALL_HELD
}

/// `held_with` を本物のキーで読む。ルーム名などの文字の入力がウィンドウから文字を受けている間は読まず `None`。
#[cfg(not(test))]
pub fn held() -> Option<u16> {
    if crate::text_input_win::gate_open() {
        return None;
    }
    Some(held_with(key_down))
}

#[cfg(test)]
pub fn held() -> Option<u16> {
    let v = TEST_HELD.load(std::sync::atomic::Ordering::SeqCst);
    u16::try_from(v).ok().map(|h| h & ALL_HELD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const VK_CTRL: u16 = 0x11;
    const VK_ALT: u16 = 0x12;

    /// 押されている仮想キーが `pressed` の読み。戻りは（集合, 問い合わせの数）。
    fn read(pressed: &[u16]) -> (u16, usize) {
        let asked = Cell::new(0);
        let set = held_with(|vk| {
            asked.set(asked.get() + 1);
            pressed.contains(&vk)
        });
        (set, asked.get())
    }

    #[test]
    fn c_is_read_as_a_key_and_ctrl_alt_are_asked_only_when_a_key_is_down() {
        assert_eq!(VK_C, u16::from(b'C'));
        assert_eq!(read(&[]), (0, 12), "数字 10 ＋ Tab ＋ C だけ問い合わせる");
        assert_eq!(read(&[VK_CTRL]), (0, 12), "Ctrl だけでは何も押していない");
        assert_eq!(
            read(&[VK_C]),
            (C_BIT, 16),
            "Ctrl・Alt・左右の Win を足して聞く"
        );
        assert_eq!(read(&[VK_C, VK_CTRL]), (C_BIT | CTRL_BIT, 16));
        assert_eq!(
            read(&[VK_C, VK_CTRL, VK_ALT]),
            (C_BIT | CTRL_BIT | MOD_BIT, 14)
        );
        assert_eq!(read(&[0x35, VK_CTRL]), (1 << 5 | CTRL_BIT, 16));
        assert_eq!(read(&[0x09, VK_CTRL]), (TAB_BIT | CTRL_BIT, 16));
        assert_eq!(read(&[0x30, 0x39]).0, 1 | 1 << 9, "数字の両端");
    }
}
