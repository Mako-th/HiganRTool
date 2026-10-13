//! ルーム名の入力がウィンドウから文字を受ける仕組み（Win32 の層。判断は core の `room_input` と `net_menu`）。
//!
//! ゲームのウィンドウの手続きを、文字（`WM_CHAR`）だけを横取りする手続きに差し替える（サブクラス化。自分のプロセスのウィンドウだけ）:
//! - 入力の受信ソケット（`TextRing`）が閉じている間は何も貯めず、全部のメッセージを元の手続きへ素通しする。
//! - 開いている間は `WM_CHAR` を貯め（ゲームへは渡さない）、入力の地点が 1 tick に 1 回 `drain` で取る。
//!   開いている間だけ、ウィンドウから IME を切り離す（IME が開いたままだと、打った文字が変換の途中の文字になって `WM_CHAR` に来ない）。
//!
//! ウィンドウの手続きも入力の地点もゲームのスレッドで走るので、リングバッファの書き手も読み手も同じスレッド。それでも `head` / `tail` は Release / Acquire で進める。
//! 確保しない・待たない・パニックしない（ウィンドウの手続きは `catch_unwind` で包む）。
//!
//! 花映塚のメインループは `PeekMessageA`（`0x0042E672`）で取ったメッセージを必ず `TranslateMessage`（`0x0042E680`）→
//! `DispatchMessageA`（`0x0042E68A`）へ流すので `WM_CHAR` は生成され、打った文字は同じ frame の入力の地点より前に届く。
//! ウィンドウの手続き `0x0042D3D0` は `WM_CLOSE`・`WM_ACTIVATEAPP`・`WM_ERASEBKGND`・`WM_SETCURSOR`・`0x3C9` を扱い、ほかは
//! `DefWindowProcA` へ流す（`WM_CHAR` は何もされない）。ウィンドウは ANSI のウィンドウなので、`SetWindowLongPtrW` で Unicode の手続きに替えると
//! `WM_CHAR` は UTF-16 の 1 単位で来る（元の手続きへは `CallWindowProcW` が ANSI に直して渡す）。

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::panic::catch_unwind;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::Ime::ImmAssociateContext;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_CHAR, WNDPROC,
};

/// リングバッファの長さ（2 の冪）。台本が一度に入れる最大（Backspace 32 ＋ 文字 32 ＋ Enter = 65 単位）と人が打つ文字を足しても余る。溢れたら新しい文字を捨てる。
const RING_LEN: usize = 128;
const _: () = assert!(RING_LEN.is_power_of_two());

pub struct TextRing {
    open: AtomicBool,
    slots: [AtomicU32; RING_LEN],
    head: AtomicU32,
    tail: AtomicU32,
}

impl TextRing {
    pub const fn new() -> TextRing {
        TextRing {
            open: AtomicBool::new(false),
            slots: [const { AtomicU32::new(0) }; RING_LEN],
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }
    }

    /// 受信ソケットを開ける・閉じる。**どちらも貯まっている文字を捨てる**（次の入力へ持ち越さない）。読み手（ゲームのスレッド）から呼ぶ。
    pub fn set_open(&self, on: bool) {
        self.tail
            .store(self.head.load(Ordering::Acquire), Ordering::Release);
        self.open.store(on, Ordering::Release);
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// 文字を 1 つ貯める（書き手）。受信ソケットが閉じている・満杯なら貯めずに false。
    pub fn push(&self, unit: u16) -> bool {
        if !self.is_open() {
            return false;
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= RING_LEN {
            return false;
        }
        if let Some(slot) = self.slots.get(head as usize % RING_LEN) {
            slot.store(u32::from(unit), Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    pub fn drain(&self, out: &mut [u16]) -> usize {
        let mut tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let mut n = 0;
        for d in out.iter_mut() {
            if tail == head {
                break;
            }
            *d = self
                .slots
                .get(tail as usize % RING_LEN)
                .map_or(0, |s| s.load(Ordering::Relaxed)) as u16;
            tail = tail.wrapping_add(1);
            n += 1;
        }
        self.tail.store(tail, Ordering::Release);
        n
    }
}

impl Default for TextRing {
    fn default() -> Self {
        TextRing::new()
    }
}

static RING: TextRing = TextRing::new();

/// メッセージ 1 つが、受信ソケットの開いている入力の文字として横取りされるか。横取りしたら文字を貯める（`true` = 元の手続きへ渡さない）。
fn consume(ring: &TextRing, msg: u32, wparam: usize) -> bool {
    if msg != WM_CHAR || !ring.is_open() {
        return false;
    }
    // 貯められなかった（満杯）文字も、入力の間はゲームへ渡さない。
    ring.push(wparam as u16);
    true
}

// ---- ウィンドウの手続きの差し替え ----

/// 差し替える前のウィンドウの手続き（`SetWindowLongPtrW` が返したもの。0 = 差し替えていない）。
static PREV_PROC: AtomicUsize = AtomicUsize::new(0);
static HOOKED: AtomicUsize = AtomicUsize::new(0);
/// 差し替えの結果: 0 = まだ試していない（ウィンドウが見つからなかったときも 0。次に開くときに探し直す）、1 = 置いた、
/// 2 = 置けなかった（ウィンドウは見つかったが断られた。同じウィンドウへ何度やっても同じなので二度は試さない）。
static INSTALL: AtomicU32 = AtomicU32::new(INSTALL_NOT_TRIED);
/// 直近の探索でゲームのウィンドウが見つからなかった（`install_text` が本ログの 1 行に書き分ける）。
static NO_WINDOW: AtomicBool = AtomicBool::new(false);
const INSTALL_NOT_TRIED: u32 = 0;
const INSTALL_OK: u32 = 1;
const INSTALL_FAILED: u32 = 2;
static IME_OFF: AtomicBool = AtomicBool::new(false);
/// 切り離す前の IME の文脈（`HIMC` の値。0 = 元から無かった）。
static SAVED_IMC: AtomicUsize = AtomicUsize::new(0);

/// 差し替えたウィンドウの手続き。文字を横取りする以外は全部元の手続きへ渡す。
///
/// # Safety
/// Windows がウィンドウの手続きとして呼ぶ（引数は Windows が渡した値）。
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // パニックはウィンドウの手続きの外へ出さない。起きたら横取りしなかったことにして元へ渡す。
    let taken = catch_unwind(|| consume(&RING, msg, wparam)).unwrap_or(false);
    if taken {
        return 0;
    }
    let prev = PREV_PROC.load(Ordering::Acquire);
    if prev == 0 {
        // 差し替えの記録が無いのに呼ばれた（来ない見込み）。ウィンドウの既定の扱いへ。
        // SAFETY: 引数は Windows が渡した、この手続きの引数そのもの。
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `prev` は `install_on` が置いた元のウィンドウの手続き（0 でない）。`CallWindowProcW` は ANSI の元の手続きへ必要な変換をして渡す。
    unsafe {
        CallWindowProcW(
            core::mem::transmute::<usize, WNDPROC>(prev),
            hwnd,
            msg,
            wparam,
            lparam,
        )
    }
}

fn install_on(hwnd: HWND) -> bool {
    let me = subclass_proc as *const () as usize;
    // SAFETY: `hwnd` は自分のプロセスのウィンドウ、`me` は上の署名どおりの手続きで、この DLL が生きている間（注入された DLL は外れない）有効。戻り値は元の手続き（失敗は 0）。ウィンドウの手続きの差し替えは、ウィンドウを作ったスレッドに依らず同じプロセスの中なら通る。
    let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, me as _) } as usize;
    if prev == 0 {
        INSTALL.store(INSTALL_FAILED, Ordering::Release);
        return false;
    }
    PREV_PROC.store(prev, Ordering::Release);
    HOOKED.store(hwnd as usize, Ordering::Release);
    INSTALL.store(INSTALL_OK, Ordering::Release);
    true
}

/// 差し替えを済ませる（初めて受信ソケットを開けるときに 1 回）。置けたら true。ウィンドウが見つからないときは何も覚えず（起動の直後など、次に探し直す）、
/// 見つかったのに断られたときだけ覚えて二度は試さない。`find` はウィンドウの探し方。
fn ensure_installed_with(find: impl FnOnce() -> Option<HWND>) -> bool {
    match INSTALL.load(Ordering::Acquire) {
        INSTALL_OK => true,
        INSTALL_NOT_TRIED => match find() {
            Some(h) => {
                NO_WINDOW.store(false, Ordering::Release);
                install_on(h)
            }
            None => {
                NO_WINDOW.store(true, Ordering::Release);
                false
            }
        },
        _ => false,
    }
}

fn ensure_installed() -> bool {
    ensure_installed_with(crate::notify_win::game_window)
}

pub fn install_text() -> &'static str {
    match INSTALL.load(Ordering::Acquire) {
        INSTALL_OK => "ウィンドウの文字の受信ソケット: 置いた",
        INSTALL_FAILED => {
            "ウィンドウの文字の受信ソケット: 置けなかった（ゲームのウィンドウの手続きの差し替えを断られた）"
        }
        _ if NO_WINDOW.load(Ordering::Acquire) => {
            "ウィンドウの文字の受信ソケット: ゲームのウィンドウが見つからなかった（次にルーム名の入力を開くときに探し直す）"
        }
        _ => "ウィンドウの文字の受信ソケット: まだ置いていない（ルーム名の入力を開くまで置かない）",
    }
}

// ---- IME ----

fn ime_off() {
    let hwnd = HOOKED.load(Ordering::Acquire) as HWND;
    if hwnd.is_null() || IME_OFF.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: `hwnd` は差し替えた自分のウィンドウ。ウィンドウを作ったスレッド（ゲームのスレッド）から呼ぶ（別のスレッドからは断られるだけで、戻り値が空になる）。第 2 引数の空は IME の文脈を切り離す約束。
    let prev = unsafe { ImmAssociateContext(hwnd, core::ptr::null_mut()) };
    SAVED_IMC.store(prev as usize, Ordering::Release);
}

fn ime_restore() {
    if !IME_OFF.swap(false, Ordering::AcqRel) {
        return;
    }
    let hwnd = HOOKED.load(Ordering::Acquire) as HWND;
    let saved = SAVED_IMC.swap(0, Ordering::AcqRel);
    if hwnd.is_null() || saved == 0 {
        return;
    }
    // SAFETY: `hwnd` は差し替えた自分のウィンドウで、`saved` は切り離したときに返った元の文脈。同じスレッドで戻す。
    unsafe { ImmAssociateContext(hwnd, saved as _) };
}

// ---- ゲームのスレッドから使う関数 ----

/// 入力の受信ソケットを開ける・閉じる（ルーム名の入力に入った・出た tick に 1 回）。開けるときウィンドウの手続きをまだ差し替えていなければ差し替える
/// （人の操作で 1 回だけの重い所）。置けなければ受信ソケットは開けず false。
pub fn set_gate(open: bool) -> bool {
    if open {
        if !ensure_installed() {
            return false;
        }
        RING.set_open(true);
        ime_off();
    } else {
        RING.set_open(false);
        ime_restore();
    }
    true
}

/// 文字の入力の受信ソケットが開いているか。数字キーで遅れの量を替える読みが、打った数字を文字として取っている間は読まないために見る。
#[cfg_attr(test, allow(dead_code))] // 検査では `digit_keys_win::held` が偽の読みを返し、この関数は呼ばない
pub fn gate_open() -> bool {
    RING.is_open()
}

pub fn drain(out: &mut [u16]) -> usize {
    RING.drain(out)
}

/// 台本の文字を受信ソケットへ入れる（受信ソケットが閉じていれば入らない。本物のキーボードと同じ決まり）。入った数を返す。
pub fn push_script(units: impl Iterator<Item = u16>) -> usize {
    units.filter(|&u| RING.push(u)).count()
}

/// 検査用: 本物の静的な状態を使う検査は、このモジュールのものも `script_win` のものも 1 本ずつ走らせる。
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 検査用: ウィンドウの手続きを差し替えずに、本物のリングバッファの受信ソケットだけを開け閉めする（`script_win` の検査が、台本の文字がリングバッファへ入ることを見る）。
#[cfg(test)]
pub(crate) fn test_set_ring_open(on: bool) {
    RING.set_open(on);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Input::Ime::{ImmGetContext, ImmReleaseContext};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowLongPtrW, GetWindowTextW, SetWindowTextW,
        WM_KEYDOWN,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn the_ring_keeps_only_what_is_typed_while_open_and_drops_the_rest() {
        let r = TextRing::new();
        let mut out = [0u16; 8];
        assert!(!r.push(u16::from(b'a')), "閉じているのに貯めた");
        assert_eq!(r.drain(&mut out), 0);
        r.set_open(true);
        assert!(r.push(u16::from(b'a')) && r.push(u16::from(b'b')) && r.push(0x0D));
        assert_eq!(r.drain(&mut out[..2]), 2, "取る数が out の長さを越えた");
        assert_eq!(out[..2], [u16::from(b'a'), u16::from(b'b')], "古い順でない");
        assert_eq!(r.drain(&mut out), 1);
        assert_eq!(out[0], 0x0D);
        assert_eq!(r.drain(&mut out), 0, "もう一度出た");
        let mut accepted = 0;
        for i in 0..RING_LEN + 10 {
            accepted += usize::from(r.push(0x100 + i as u16));
        }
        assert_eq!(accepted, RING_LEN);
        let mut all = [0u16; RING_LEN + 10];
        assert_eq!(r.drain(&mut all), RING_LEN);
        assert_eq!(all[0], 0x100);
        assert_eq!(all[RING_LEN - 1], 0x100 + RING_LEN as u16 - 1);
        r.push(u16::from(b'x'));
        r.set_open(false);
        assert_eq!(r.drain(&mut out), 0, "閉じても文字が残った");
        r.set_open(true);
        assert_eq!(r.drain(&mut out), 0);
        // tail を 2^32 の手前に置いて 1 周させても、番号の桁あふれで壊れない。
        r.head.store(u32::MAX - 3, Ordering::Release);
        r.tail.store(u32::MAX - 3, Ordering::Release);
        for u in 1..=10u16 {
            assert!(r.push(u));
        }
        assert_eq!(r.drain(&mut out), 8);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.drain(&mut out), 2);
        assert_eq!(out[..2], [9, 10]);
    }

    #[test]
    fn only_wm_char_with_an_open_gate_is_taken() {
        let r = TextRing::new();
        let mut out = [0u16; 4];
        assert!(!consume(&r, WM_CHAR, 0x61), "閉じているのに横取りした");
        r.set_open(true);
        assert!(!consume(&r, WM_KEYDOWN, 0x41), "WM_CHAR 以外を横取りした");
        assert_eq!(r.drain(&mut out), 0);
        assert!(consume(&r, WM_CHAR, 0x61));
        assert_eq!(r.drain(&mut out), 1);
        assert_eq!(out[0], 0x61);
        assert!(consume(&r, WM_CHAR, 0x1_0000 | 0x62));
        assert_eq!(r.drain(&mut out), 1);
        assert_eq!(out[0], 0x62);
    }

    fn make_window() -> HWND {
        let class = wide("STATIC");
        let title = wide("");
        // SAFETY: 引数は 0 終わりの UTF-16 と空の値。STATIC は OS の組み込みのクラスで、ウィンドウは検査のスレッドの持ち物。
        unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                10,
                10,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null(),
            )
        }
    }

    fn restore_window(hwnd: HWND) {
        let prev = PREV_PROC.swap(0, Ordering::AcqRel);
        if prev != 0 {
            // SAFETY: `prev` は `install_on` が受け取った元の手続き。同じウィンドウへ戻す。
            unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, prev as _) };
        }
        HOOKED.store(0, Ordering::Release);
        INSTALL.store(INSTALL_NOT_TRIED, Ordering::Release);
        RING.set_open(false);
        ime_restore();
        // SAFETY: 検査が作った自分のウィンドウ。
        unsafe { DestroyWindow(hwnd) };
    }

    struct WindowGuard(HWND);

    impl Drop for WindowGuard {
        fn drop(&mut self) {
            restore_window(self.0);
        }
    }

    fn deliver(hwnd: HWND, msg: u32, wparam: usize) -> LRESULT {
        // SAFETY: `GetWindowLongPtrW` が返すのはウィンドウの今の手続きで、`CallWindowProcW` はそれを呼ぶ。
        unsafe {
            let cur = GetWindowLongPtrW(hwnd, GWLP_WNDPROC) as usize;
            CallWindowProcW(
                core::mem::transmute::<usize, WNDPROC>(cur),
                hwnd,
                msg,
                wparam,
                0,
            )
        }
    }

    #[test]
    fn a_real_window_is_subclassed_and_only_chars_are_taken_while_the_gate_is_open() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let hwnd = make_window();
        // SAFETY: 直前の API の最後の誤りの番号（失敗したときだけ使う）。
        assert!(
            !hwnd.is_null(),
            "検査のウィンドウを作れない（{}）",
            unsafe { GetLastError() }
        );
        let _guard = WindowGuard(hwnd);
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_NOT_TRIED);
        assert!(install_on(hwnd), "差し替えが置けない");
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_OK);
        let mut out = [0u16; 8];
        deliver(hwnd, WM_CHAR, 0x61);
        assert_eq!(drain(&mut out), 0, "閉じているのに取り込んだ");
        let text = wide("room-title");
        // SAFETY: 検査のウィンドウと 0 終わりの UTF-16。
        unsafe { SetWindowTextW(hwnd, text.as_ptr()) };
        let mut got = [0u16; 32];
        // SAFETY: 検査のウィンドウと書ける 32 単位の配列。
        let n = unsafe { GetWindowTextW(hwnd, got.as_mut_ptr(), got.len() as i32) } as usize;
        assert_eq!(
            String::from_utf16_lossy(&got[..n]),
            "room-title",
            "WM_CHAR 以外が元の手続きへ届かない"
        );
        // 開ける: IME を切り離し、文字を取り込む（戻りは 0）。
        // SAFETY: 検査のウィンドウから今の IME の文脈を読むだけ（取ったら返す）。
        let before = unsafe { ImmGetContext(hwnd) };
        if !before.is_null() {
            // SAFETY: 取った文脈を返す。
            unsafe { ImmReleaseContext(hwnd, before) };
        }
        assert!(set_gate(true));
        // SAFETY: 同上。
        let during = unsafe { ImmGetContext(hwnd) };
        assert!(
            during.is_null(),
            "受信ソケットを開けても IME が切り離されない"
        );
        assert_eq!(deliver(hwnd, WM_CHAR, 0x41), 0);
        assert_eq!(deliver(hwnd, WM_CHAR, 0x0D), 0);
        assert_eq!(drain(&mut out), 2);
        assert_eq!(out[..2], [0x41, 0x0D]);
        assert_eq!(push_script([0x62u16, 0x63].into_iter()), 2);
        assert_eq!(drain(&mut out), 2);
        assert!(set_gate(false));
        // SAFETY: 同上。
        let after = unsafe { ImmGetContext(hwnd) };
        assert_eq!(
            after.is_null(),
            before.is_null(),
            "受信ソケットを閉じても IME が戻らない"
        );
        if !after.is_null() {
            // SAFETY: 取った文脈を返す。
            unsafe { ImmReleaseContext(hwnd, after) };
        }
        deliver(hwnd, WM_CHAR, 0x64);
        assert_eq!(drain(&mut out), 0, "閉じた後に取り込んだ");
        assert_eq!(
            push_script([0x65u16].into_iter()),
            0,
            "閉じているのに台本の文字が入った"
        );
    }

    #[test]
    fn without_a_game_window_the_gate_stays_shut_and_the_search_is_repeated_next_time() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_NOT_TRIED);
        assert!(!set_gate(true), "ウィンドウが無いのに受信ソケットを開けた");
        assert!(!RING.is_open());
        assert!(
            !IME_OFF.load(Ordering::Acquire),
            "置けないのに IME を切り離した"
        );
        assert_eq!(
            INSTALL.load(Ordering::Acquire),
            INSTALL_NOT_TRIED,
            "ウィンドウが見つからなかったことを覚えた（次に探し直さない）"
        );
        assert!(install_text().contains("見つからなかった"));
        assert!(set_gate(false));
        let mut asked = 0;
        for _ in 0..3 {
            assert!(!ensure_installed_with(|| {
                asked += 1;
                None
            }));
        }
        assert_eq!(asked, 3, "見つからなかった後に探さなくなった");
        let hwnd = make_window();
        assert!(!hwnd.is_null());
        let _guard = WindowGuard(hwnd);
        assert!(
            ensure_installed_with(|| Some(hwnd)),
            "ウィンドウが現れても置けない"
        );
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_OK);
        assert!(!NO_WINDOW.load(Ordering::Acquire));
        assert!(install_text().contains("置いた"));
    }

    #[test]
    fn a_refused_install_is_remembered_and_not_retried() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_NOT_TRIED);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                INSTALL.store(INSTALL_NOT_TRIED, Ordering::Release);
                NO_WINDOW.store(false, Ordering::Release);
            }
        }
        let _reset = Reset;
        let bogus = 0x10usize as HWND;
        assert!(!ensure_installed_with(|| Some(bogus)));
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_FAILED);
        assert_eq!(PREV_PROC.load(Ordering::Acquire), 0, "断られたのに記録した");
        assert!(install_text().contains("断られた"));
        let mut asked = 0;
        assert!(!ensure_installed_with(|| {
            asked += 1;
            None
        }));
        assert_eq!(asked, 0, "断られた後にまたウィンドウを探した");
        assert_eq!(INSTALL.load(Ordering::Acquire), INSTALL_FAILED);
    }
}
