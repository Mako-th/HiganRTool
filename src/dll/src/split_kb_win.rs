//! キーボード2台を分ける入力の受け取り（Win32の層。判断はcoreの`split_kb`）。
//!
//! DirectInput経路（`[0x4B3110]`が0でない）: DirectInputが自分のウィンドウへRaw Inputを登録し、前面の出入りで付け外しする（推定）。
//! ここで登録を置き換えるとゲームのキーボードが止まるので、登録は変えない。
//! ポーリングの処理が500 msごとに登録の一覧を読む。同じプロセスのウィンドウに登録があればそのウィンドウの手続きを差し替えて`WM_INPUT`を読む（必ず元の手続きへ渡す。送り先が替われば差し替え直す）。
//! GetKeyboardState経路（`[0x4B3110]`が0）: ゲームのウィンドウへ自分で登録する（`RIDEV_INPUTSINK`・`RIDEV_NOLEGACY`は付けない）。読むのは`text_input_win`の差し替えた手続き。
//! 読んだキーは台ごとの押下の表（`TABLE`）へ書く。送り先がNULL・別のプロセス・差し替え不能ならこの起動では無効にする。
//! ゲームのスレッドは最初の1回だけ判断する。一覧は読まない。台本の実行は登録せず、台本の命令（`kbd=`）が表を書く。

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::panic::catch_unwind;

use higanr_core::script::RawKeys;
use higanr_core::split_kb::{DeviceTable, is_fake_shift};
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RID_INPUT, RIM_TYPEKEYBOARD, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetClassNameW, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, RI_KEY_BREAK, RI_KEY_E0,
    SetWindowLongPtrW, WM_ACTIVATEAPP, WM_INPUT, WNDPROC,
};

/// 台ごとの押下の表。書くのは`WM_INPUT`の受け取りと台本の命令。読むのは`NetUi::split_step`。
pub static TABLE: DeviceTable = DeviceTable::new();

/// 受け方の状態（`REG`）。値の意味は次のとおり。
/// - 0 = 未試行（ウィンドウが無い間も0で、次に探し直す）
/// - 1 = ゲームのウィンドウへ自分で登録済み
/// - 2 = 登録を断られた
/// - 3 = 無効（送り先がNULL・別のプロセス・差し替え不能・一覧を読めない。この起動では分割は働かない）
/// - 4 = 別のウィンドウ宛ての登録を、そのウィンドウの手続きを差し替えて読んでいる
/// - 5 = DirectInput経路で登録がまだ出ていない（ポーリングの処理が待っている）
/// - 6 = ゲームのウィンドウ宛ての登録を読んでいる（登録したのは他者。登録は変えず、ゲームのウィンドウ側で読む）
const REG_NOT_TRIED: u32 = 0;
const REG_OK: u32 = 1;
const REG_FAILED: u32 = 2;
const REG_FOREIGN: u32 = 3;
const REG_HOOKED: u32 = 4;
const REG_WAITING: u32 = 5;
const REG_ON_GAME: u32 = 6;
/// ポーリングの処理が一覧の変化を見た回数（本ログに書いた数ではなく要約に出す数）。
static CHANGES: AtomicU32 = AtomicU32::new(0);
/// ポーリングの処理（`refresh`が待ちを解く相手）。
static WATCHER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();
/// ポーリングの間隔（ミリ秒）。前面の出入りで登録が付いたり外れたりするのを人の操作の速さ（0.5秒）で追える長さ。
const WATCH_MS: u64 = 500;
/// 本ログに書く一覧の変化の上限。前面の出入り1回で2件（付く・外れる）の変化が出るので20回ぶん。1行は約150バイト（推定）なので40行で6 KB以内。
const MAX_CHANGE_LOGS: u32 = 40;
static REG: AtomicU32 = AtomicU32::new(REG_NOT_TRIED);
/// 差し替えて読んでいる送り先のウィンドウ（`REG_HOOKED`のとき）。
static HOOK_CUR: AtomicUsize = AtomicUsize::new(0);
/// 受け取って表へ書き込んだイベントの数（要約に出す。受け取れているかを見るため）。
static EVENTS: AtomicU32 = AtomicU32::new(0);
/// 登録を試すtickの間引き（ウィンドウが見つからない間はウィンドウの探索を毎tick走らせない）。
static TRIES: AtomicU32 = AtomicU32::new(0);
const RETRY_EVERY: u32 = 64;

/// HIDの使用ページ・使用: キーボードはusage page 1（汎用デスクトップ）・usage 6。
const PAGE_GENERIC: u16 = 1;
const USAGE_KEYBOARD: u16 = 6;

/// プロセスのRaw Inputの登録の一覧の1件（`RAWINPUTDEVICE`の要点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegEntry {
    pub(crate) page: u16,
    pub(crate) usage: u16,
    pub(crate) flags: u32,
    pub(crate) target: usize,
}

/// 一覧を読むバッファの大きさ（件）。足りなければ「取得不能」として扱う。
pub(crate) const LIST_MAX: usize = 16;

const EMPTY_ENTRY: RegEntry = RegEntry {
    page: 0,
    usage: 0,
    flags: 0,
    target: 0,
};

/// このプロセスのRaw Inputの登録の一覧（`GetRegisteredRawInputDevices`）。読めなければ`None`。
pub(crate) fn registered(list: &mut [RegEntry; LIST_MAX]) -> Option<usize> {
    let mut raw = [RAWINPUTDEVICE::default(); LIST_MAX];
    let mut n = LIST_MAX as u32;
    // SAFETY: `raw`は`n`件ぶんの書き込み可能な配列。`n`にその件数を渡す（足りなければ -1が返り、`n`に要る件数が入るだけ）。サイズは構造体の大きさ。
    let got = unsafe {
        GetRegisteredRawInputDevices(raw.as_mut_ptr(), &mut n, size_of::<RAWINPUTDEVICE>() as u32)
    };
    if got == u32::MAX {
        return None;
    }
    let count = (got as usize).min(LIST_MAX);
    for (dst, src) in list.iter_mut().zip(&raw).take(count) {
        *dst = RegEntry {
            page: src.usUsagePage,
            usage: src.usUsage,
            flags: src.dwFlags,
            target: src.hwndTarget as usize,
        };
    }
    Some(count)
}

fn target_text(target: usize, game: usize) -> &'static str {
    if target == game {
        "ゲーム"
    } else if target == 0 {
        "なし"
    } else {
        "別"
    }
}

/// 本ログの1行ぶん: 件数と、各件の使用ページ/使用・フラグ・送り先のウィンドウ（ゲームのウィンドウか別か）。
fn describe(list: &[RegEntry], game: usize) -> String {
    let items: Vec<String> = list
        .iter()
        .map(|e| {
            format!(
                "{}/{} フラグ0x{:X} 送り先{}",
                e.page,
                e.usage,
                e.flags,
                target_text(e.target, game)
            )
        })
        .collect();
    format!("{}件 [{}]", list.len(), items.join(", "))
}

/// キーボードを受ける方法を使えない理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// 送り先がNULL（キーボードフォーカスに従う登録）。
    NullTarget,
    /// 送り先が自分のプロセスのウィンドウでない。
    OtherProcess,
    /// 登録の一覧を読めない。
    Unreadable,
    /// 送り先のウィンドウの手続きを差し替えられなかった（エラー番号）。
    HookRefused(u32),
    /// ポーリングの処理を開始できなかった。
    NoWatcher,
}

fn disabled_text(why: Why) -> String {
    let reason = match why {
        Why::NullTarget => "Raw Input の送り先が未指定（NULL）".to_string(),
        Why::OtherProcess => "Raw Input の送り先が別のプロセス".to_string(),
        Why::Unreadable => "Raw Input の登録の一覧の取得不能".to_string(),
        Why::HookRefused(e) => {
            format!("送り先のウィンドウの手続きの差し替え不能（エラー {e}）")
        }
        Why::NoWatcher => "チェックの処理の開始不能".to_string(),
    };
    format!("キーボードの分割: この起動では無効です（{reason}）。")
}

/// 登録の一覧から決める受け方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// キーボードの登録が無い。
    Absent,
    /// ゲームのウィンドウ宛てに登録済み。
    OnGame,
    /// 同じプロセスの別のウィンドウに登録済み。登録は変えず、そのウィンドウの手続きを差し替えて読む。
    Hook(usize),
    Disabled(Why),
}

/// 一覧・ゲームのウィンドウ・`own_window`（自分のプロセスの生きているウィンドウかどうか）から受け方を決める。
fn plan(list: &[RegEntry], game: usize, own_window: impl Fn(usize) -> bool) -> Plan {
    let Some(e) = list
        .iter()
        .find(|e| e.page == PAGE_GENERIC && e.usage == USAGE_KEYBOARD)
    else {
        return Plan::Absent;
    };
    if e.target == game {
        Plan::OnGame
    } else if e.target == 0 {
        Plan::Disabled(Why::NullTarget)
    } else if !own_window(e.target) {
        Plan::Disabled(Why::OtherProcess)
    } else {
        Plan::Hook(e.target)
    }
}

/// 自分のプロセスの生きているウィンドウかどうか。
fn is_own_window(h: usize) -> bool {
    let hwnd = h as HWND;
    let mut pid = 0u32;
    // SAFETY: 読むだけの問い合わせ。`hwnd`が消えていれば`IsWindow`が0を返し、`GetWindowThreadProcessId`も0を返すだけ。`pid`は書き込み可能なu32。
    unsafe {
        IsWindow(hwnd) != 0
            && GetWindowThreadProcessId(hwnd, &mut pid) != 0
            && pid == GetCurrentProcessId()
    }
}

/// 送り先のウィンドウの説明（クラス名・表示中かどうか・作成元のスレッドがゲームと同じかどうか）。
fn window_text(target: HWND, game: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: `buf`は書き込み可能な64単位の配列。長さを渡す。消えたウィンドウは0が返るだけ。
    let n = unsafe { GetClassNameW(target, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    let class = String::from_utf16_lossy(buf.get(..n).unwrap_or(&[]));
    // SAFETY: 読むだけの問い合わせ。
    let visible = unsafe { IsWindowVisible(target) } != 0;
    // SAFETY: 同上。出力のプロセスIDは要らないのでNULL。
    let (t, g) = unsafe {
        (
            GetWindowThreadProcessId(target, core::ptr::null_mut()),
            GetWindowThreadProcessId(game, core::ptr::null_mut()),
        )
    };
    format!(
        "{class}・{}・作成元 {}",
        if visible { "表示中" } else { "非表示" },
        if t == g {
            "ゲームと同じ処理"
        } else {
            "別の処理"
        }
    )
}

// 検査では本ログの待ち行列を汚さずこのスレッドに記録する（検査が読む。ペアは（詳細だけか, 行））。
#[cfg(test)]
thread_local! {
    static TEST_LINES: std::cell::RefCell<Vec<(bool, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn log(line: impl Into<String>) {
    let line = line.into();
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push((false, line.clone())));
    #[cfg(not(test))]
    crate::main_log::push(line);
}

fn log_detail(line: impl Into<String>) {
    let line = line.into();
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push((true, line.clone())));
    #[cfg(not(test))]
    crate::main_log::push_detail(line);
}

/// 人間 対 人間の分け方のイベントを詳細のログに1行書く。
/// イベントは1回の選び方で数回しか起きない（tickごとではない）。そのためゲームのスレッドから呼んでよい。
pub fn log_event(ev: higanr_core::split_kb::Event) {
    log_detail(higanr_core::split_kb::event_line(ev, &TABLE));
}

/// 検査用: このスレッドに書かれたログの行を取る（取ったら消える）。ペアは（詳細だけか, 行）。
#[cfg(test)]
pub(crate) fn test_take_lines() -> Vec<(bool, String)> {
    TEST_LINES.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// 受け方を決める最初の1回（ゲームのスレッドから、台を分ける設定が有効なtickに呼ぶ。済んでいればアトミック変数1つを読むだけ）。
pub fn ensure_registered() {
    if REG.load(Ordering::Acquire) != REG_NOT_TRIED {
        return;
    }
    if !TRIES
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(RETRY_EVERY)
    {
        return;
    }
    // 検査のプロセスにあるほかの検査のウィンドウを、ゲームのウィンドウと取り違えて差し替えない。
    if cfg!(test) {
        return;
    }
    let Some(hwnd) = crate::text_input_win::hooked_window() else {
        return;
    };
    let device = keyboard_device();
    log(keyboard_device_text(device));
    begin(hwnd, device, true);
}

/// ゲームのキーボードのDirectInputデバイスの値。読めなければ`None`。
fn keyboard_device() -> Option<u32> {
    use crate::menu::MemRead;
    crate::menu_win::OwnProcess.u32_at(crate::th09_addr::KEYBOARD_DEVICE)
}

/// デバイスの値で経路を分ける。自分で登録するのは0（GetKeyboardState経路）のときだけ。ほかに登録する者がいないので、登録を調べてから登録する。
/// 0でない・読めない（DirectInput経路）ときは自分では一切登録しない。登録が出るのを待つのはポーリングの処理（`spawn`時に開始）。
fn begin(game: HWND, device: Option<u32>, spawn: bool) {
    if device == Some(0) {
        establish(game);
        return;
    }
    REG.store(REG_WAITING, Ordering::Release);
    HOOK_CUR.store(0, Ordering::Release);
    log(format!(
        "キーボードの分割: DirectInput の Raw Input の登録を待っています（{WATCH_MS} ms ごとにチェック）。"
    ));
    if !spawn {
        return;
    }
    let g = game as usize;
    match std::thread::Builder::new()
        .name("HiganRTool kbd-watch".to_string())
        .spawn(move || watcher(g))
    {
        Ok(j) => {
            let _ = WATCHER.set(j.thread().clone());
        }
        Err(_) => disable(Why::NoWatcher),
    }
}

/// ポーリングの処理を続けるべきかどうか（待っている・差し替えて読んでいる・ゲームのウィンドウ宛ての登録を読んでいる間）。
fn watching() -> bool {
    matches!(
        REG.load(Ordering::Acquire),
        REG_WAITING | REG_HOOKED | REG_ON_GAME
    )
}

/// ポーリングの処理（`WATCH_MS`ごとまたは`refresh`に待ちを解かれたときに一覧を1回読む）。無効になったら終わる。
fn watcher(game: usize) {
    /// 動いているポーリングの処理の数（検査が終わりを待つ）。
    struct Running;
    impl Running {
        fn new() -> Running {
            WATCHERS.fetch_add(1, Ordering::AcqRel);
            Running
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            WATCHERS.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let _running = Running::new();
    let mut w = Watch::new();
    loop {
        if !watching() {
            return;
        }
        if let Ok(Some(line)) =
            std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| w.step(game as HWND)))
        {
            log(line);
        }
        if !watching() {
            return;
        }
        std::thread::park_timeout(std::time::Duration::from_millis(WATCH_MS));
    }
}

/// 動いているポーリングの処理の数。
static WATCHERS: AtomicU32 = AtomicU32::new(0);

/// 聞き始める前に呼ぶ（人間 対 人間が決定済みになったtickの1回）。ポーリングの処理の待ちを解くだけ。ゲームのスレッドは登録の一覧を読まない。
pub fn refresh() {
    if !watching() {
        return;
    }
    if let Some(t) = WATCHER.get() {
        t.unpark();
    }
}

/// ポーリング1回ごとの記憶（前回の一覧と本ログに書いた変化の数）。
struct Watch {
    last: Option<Vec<RegEntry>>,
    logged: u32,
}

impl Watch {
    fn new() -> Watch {
        Watch {
            last: None,
            logged: 0,
        }
    }

    /// 一覧を読んで前回と変わっていれば本ログの文を返す（`MAX_CHANGE_LOGS`回まで）。
    /// 受け方は変わっていなくても毎回決め直す。一覧を読めなければ何もしない。
    fn step(&mut self, game: HWND) -> Option<String> {
        let g = game as usize;
        let mut list = [EMPTY_ENTRY; LIST_MAX];
        let n = registered(&mut list)?;
        let cur = &list[..n];
        let mut line = None;
        if self.last.as_deref() != Some(cur) {
            CHANGES.fetch_add(1, Ordering::Relaxed);
            if self.logged < MAX_CHANGE_LOGS {
                self.logged += 1;
                let tail = if self.logged == MAX_CHANGE_LOGS {
                    "。以後の変化は記録しません（回数は終了時の要約に出ます）"
                } else {
                    ""
                };
                line = Some(format!(
                    "キーボードの分割: Raw Input の登録の変化 {}{tail}",
                    describe(cur, g)
                ));
            }
            self.last = Some(cur.to_vec());
        }
        match plan(cur, g, is_own_window) {
            // 登録が無い間は待つか前に差し替えたウィンドウを読み続ける。
            // 前面の出入りで外れてもまた付くので手続きはそのままにしておく。
            Plan::Absent => {}
            Plan::OnGame => {
                if REG.load(Ordering::Acquire) != REG_ON_GAME {
                    HOOK_CUR.store(0, Ordering::Release);
                    REG.store(REG_ON_GAME, Ordering::Release);
                    log(
                        "キーボードの分割: Raw Input の登録の送り先がゲームのウィンドウなので、ゲームのウィンドウで受け取ります。",
                    );
                }
            }
            Plan::Hook(t) => {
                if !(REG.load(Ordering::Acquire) == REG_HOOKED
                    && HOOK_CUR.load(Ordering::Acquire) == t)
                {
                    hook_outcome(game, t, install_hook(t as HWND));
                }
            }
            Plan::Disabled(why) => disable(why),
        }
        line
    }
}

/// GetKeyboardState経路で登録を調べて受け方を決め、実行する（最初の1回）。
pub(crate) fn establish(game: HWND) {
    let g = game as usize;
    let mut list = [EMPTY_ENTRY; LIST_MAX];
    let before = registered(&mut list);
    let p = match before {
        Some(n) => plan(&list[..n], g, is_own_window),
        None => Plan::Disabled(Why::Unreadable),
    };
    match before {
        Some(n) => log(format!(
            "キーボードの分割: Raw Input の登録（登録の前） {}",
            describe(&list[..n], g)
        )),
        None => log("キーボードの分割: Raw Input の登録（登録の前） 取得不能"),
    }
    let after_note = match p {
        Plan::Absent | Plan::OnGame => {
            let ok = register_on(game);
            HOOK_CUR.store(0, Ordering::Release);
            if ok {
                log("キーボードの分割: ゲームのウィンドウへ Raw Input を登録しました。");
                "登録の後"
            } else {
                log("キーボードの分割: ゲームのウィンドウへの Raw Input の登録に失敗しました。");
                "登録の後・失敗"
            }
        }
        Plan::Hook(target) => hook_outcome(game, target, install_hook(target as HWND)),
        Plan::Disabled(why) => {
            disable(why);
            "無効"
        }
    };
    match registered(&mut list) {
        Some(n) => log(format!(
            "キーボードの分割: Raw Input の登録（{after_note}） {}",
            describe(&list[..n], g)
        )),
        None => log(format!(
            "キーボードの分割: Raw Input の登録（{after_note}） 取得不能"
        )),
    }
}

/// 送り先の手続きの差し替えの結果を受け方に反映する。本ログの「登録の後」の注を返す。
fn hook_outcome(game: HWND, target: usize, result: Result<(), u32>) -> &'static str {
    match result {
        Ok(()) => {
            HOOK_CUR.store(target, Ordering::Release);
            REG.store(REG_HOOKED, Ordering::Release);
            log(format!(
                "キーボードの分割: Raw Input の登録済みの送り先のウィンドウで受け取ります（登録は変更なし。送り先: {}）。",
                window_text(target as HWND, game)
            ));
            "差し替えのみ・登録は変更なし"
        }
        Err(e) => {
            disable(Why::HookRefused(e));
            "無効"
        }
    }
}

fn disable(why: Why) {
    REG.store(REG_FOREIGN, Ordering::Release);
    HOOK_CUR.store(0, Ordering::Release);
    log(disabled_text(why));
}

/// ゲームのキーボードのDirectInputデバイスの値の行（0なら入力の関数はGetKeyboardState経路で読む）。
fn keyboard_device_text(device: Option<u32>) -> String {
    use crate::th09_addr::KEYBOARD_DEVICE;
    match device {
        Some(0) => format!(
            "キーボードの分割: ゲームのキーボードの読み方 GetKeyboardState（[0x{KEYBOARD_DEVICE:08X}]=0）"
        ),
        Some(v) => format!(
            "キーボードの分割: ゲームのキーボードの読み方 DirectInput（[0x{KEYBOARD_DEVICE:08X}]=0x{v:08X}）"
        ),
        None => format!("キーボードの分割: [0x{KEYBOARD_DEVICE:08X}] 読み取り不能"),
    }
}

/// `hwnd`（ゲームのウィンドウ）へキーボードのRaw Inputを登録し、結果を覚える。登録できたらtrue。
pub(crate) fn register_on(hwnd: HWND) -> bool {
    let dev = RAWINPUTDEVICE {
        usUsagePage: PAGE_GENERIC,
        usUsage: USAGE_KEYBOARD,
        dwFlags: 0,
        hwndTarget: hwnd,
    };
    // SAFETY: `dev`は1件の有効なRAWINPUTDEVICE。`hwndTarget`は自分のプロセスのウィンドウ（ウィンドウを作ったスレッドから呼ぶ）。サイズは構造体の大きさ。
    let ok = unsafe { RegisterRawInputDevices(&dev, 1, size_of::<RAWINPUTDEVICE>() as u32) };
    REG.store(if ok != 0 { REG_OK } else { REG_FAILED }, Ordering::Release);
    ok != 0
}

// ---- 別のウィンドウの手続きの差し替え ----

/// 差し替えたウィンドウとそのウィンドウの元の手続き（ウィンドウごとに持つ。壊れたウィンドウの手続きを別のウィンドウに呼ばない）。
const HOOK_SLOTS: usize = 4;
static HOOK_WND: [AtomicUsize; HOOK_SLOTS] = [const { AtomicUsize::new(0) }; HOOK_SLOTS];
static HOOK_PREV: [AtomicUsize; HOOK_SLOTS] = [const { AtomicUsize::new(0) }; HOOK_SLOTS];

fn prev_of(hwnd: usize) -> usize {
    for (w, p) in HOOK_WND.iter().zip(&HOOK_PREV) {
        if w.load(Ordering::Acquire) == hwnd {
            return p.load(Ordering::Acquire);
        }
    }
    0
}

/// `hwnd`の表の枠: すでに入っている枠・空き・壊れたウィンドウの枠の順。
fn claim_slot(hwnd: usize) -> Option<usize> {
    let find =
        |ok: &dyn Fn(usize) -> bool| HOOK_WND.iter().position(|w| ok(w.load(Ordering::Acquire)));
    find(&|w| w == hwnd)
        .or_else(|| find(&|w| w == 0))
        // SAFETY: 読むだけの問い合わせ。消えたウィンドウは0が返るだけ。
        .or_else(|| find(&|w| unsafe { IsWindow(w as HWND) } == 0))
}

/// 別のウィンドウ（同じプロセス。別のスレッドのものでもよい）の手続きを差し替える。
/// 元の手続きは先に表へ書き込んでから差し替える（差し替えた直後にそのウィンドウのスレッドが手続きを呼んでも渡し先が決まっている）。
fn install_hook(hwnd: HWND) -> Result<(), u32> {
    let h = hwnd as usize;
    let me = foreign_proc as *const () as usize;
    // SAFETY: 読むだけの問い合わせ（自分のプロセスのウィンドウ。消えていれば0が返る）。
    let cur = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } as usize;
    if cur == 0 {
        // SAFETY: 直前のAPIの最後のエラー番号。
        return Err(unsafe { GetLastError() });
    }
    if cur == me && prev_of(h) != 0 {
        return Ok(());
    }
    let Some(slot) = claim_slot(h) else {
        return Err(0);
    };
    if let (Some(prev), Some(wnd)) = (HOOK_PREV.get(slot), HOOK_WND.get(slot)) {
        prev.store(cur, Ordering::Release);
        wnd.store(h, Ordering::Release);
        // SAFETY: `hwnd`は自分のプロセスのウィンドウ。`me`は下の署名どおりの手続きで、このDLLが生きている間（注入されたDLLは外れない）有効。
        // ウィンドウの手続きの差し替えは、同じプロセスの中ならウィンドウを作ったスレッドに依らず通る。戻り値は元の手続き（失敗は0）。
        let old = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, me as _) } as usize;
        if old == 0 {
            // SAFETY: 直前のAPIの最後のエラー番号。
            let e = unsafe { GetLastError() };
            wnd.store(0, Ordering::Release);
            return Err(e);
        }
        if old != cur {
            prev.store(old, Ordering::Release);
        }
    }
    Ok(())
}

/// 差し替えた別のウィンドウの手続き。`WM_INPUT`を読むだけ。全部のメッセージは元の手続きへ渡す（消費しない: DirectInputが同じ通知を読めるように）。
///
/// # Safety
/// Windowsがウィンドウの手続きとして呼ぶ（引数はWindowsが渡した値）。
unsafe extern "system" fn foreign_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT && REG.load(Ordering::Acquire) == REG_HOOKED {
        // パニックはウィンドウの手続きの外へ出さない。
        let _ = catch_unwind(|| on_wm_input(lparam));
    }
    let prev = prev_of(hwnd as usize);
    if prev == 0 {
        // 差し替えの記録が無いのに呼ばれた（来ない見込み）。
        // SAFETY: 引数はこの手続きにWindowsが渡した引数そのもの。
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `prev`は`install_hook`が表へ書き込んだこのウィンドウの元の手続き（0でない）。`CallWindowProcW`は必要なANSI / Unicodeの変換をして渡す。
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

/// 本ログの要約の1行（受け方と受け取ったイベントの数）。
pub fn status_text() -> String {
    let how = match REG.load(Ordering::Acquire) {
        REG_OK => "ゲームのウィンドウへ登録済み",
        REG_HOOKED => "登録済みの送り先で受け取り中",
        REG_WAITING => "DirectInput の登録待ち",
        REG_ON_GAME => "ゲームのウィンドウで受け取り中",
        REG_FAILED => "登録不能",
        REG_FOREIGN => "無効",
        _ => "未開始",
    };
    format!(
        "キーボードの分割: {how}（Raw Input の登録の変化 {} 回・受け取ったキーの知らせ {} 件）",
        CHANGES.load(Ordering::Relaxed),
        EVENTS.load(Ordering::Relaxed)
    )
}

/// ゲームのウィンドウの差し替えた手続きから、全部のメッセージが渡される。
/// 自分で登録しているときだけ`WM_INPUT`を読む。
/// 受け方が有効なら前面を失ったときに押下を全部消す（離した通知が来ないため）。
pub fn on_message(msg: u32, wparam: WPARAM, lparam: LPARAM) {
    let reg = REG.load(Ordering::Acquire);
    if matches!(reg, REG_NOT_TRIED | REG_FAILED | REG_FOREIGN) {
        return;
    }
    match msg {
        WM_INPUT if matches!(reg, REG_OK | REG_ON_GAME) => on_wm_input(lparam),
        WM_ACTIVATEAPP if wparam == 0 => TABLE.clear_keys(),
        _ => {}
    }
}

/// `WM_INPUT`で読んだキーボードのイベント。
#[derive(Debug, Clone, Copy)]
struct RawKey {
    handle: usize,
    vk: u16,
    make_code: u16,
    flags: u16,
}

fn on_wm_input(lparam: LPARAM) {
    if let Some(k) = read_raw(lparam) {
        EVENTS.fetch_add(1, Ordering::Relaxed);
        apply_key(k.handle, k.vk, k.make_code, k.flags);
    }
}

/// 検査用: `WM_INPUT`で読めたことにするキー（本物の入力ハンドルは検査では作れない）。
#[cfg(test)]
static TEST_KEY: std::sync::Mutex<Option<RawKey>> = std::sync::Mutex::new(None);

/// ハンドルの入力を読む。読むだけ。入力は消費しない（`RID_INPUT`）。キーボード以外のとき、または読めなければ`None`。
fn read_raw(lparam: LPARAM) -> Option<RawKey> {
    #[cfg(test)]
    if let Some(k) = *TEST_KEY.lock().unwrap_or_else(|e| e.into_inner()) {
        return Some(k);
    }
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: `lparam`はWM_INPUTが渡したHRAWINPUT。出力先はRAWINPUT 1つぶんの書き込み可能な領域。`size`にその大きさを渡す（足りなければ -1が返るだけ）。
    let n = unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            (&mut raw as *mut RAWINPUT).cast(),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if n == u32::MAX || raw.header.dwType != RIM_TYPEKEYBOARD {
        return None;
    }
    // SAFETY: 種類がキーボードの入力なので共用体の中身はRAWKEYBOARD（全部のフィールドが整数）。
    let k = unsafe { raw.data.keyboard };
    Some(RawKey {
        handle: raw.header.hDevice as usize,
        vk: k.VKey,
        make_code: k.MakeCode,
        flags: k.Flags,
    })
}

/// キー1つのイベントを台の表へ書き込む。VKが0と0xFFのものはOSが作るマーカー（エスケープシーケンスなど）でキーではない。
/// 押したときのVKはスキャンコードごとに覚え、離すときはそれを消す。
pub fn apply_key(handle: usize, vk: u16, make_code: u16, flags: u16) {
    apply_key_to(&TABLE, handle, vk, make_code, flags);
}

fn apply_key_to(table: &DeviceTable, handle: usize, vk: u16, make_code: u16, flags: u16) {
    let e0 = u32::from(flags) & RI_KEY_E0 != 0;
    if vk == 0 || vk >= 0xFF || is_fake_shift(make_code, e0) {
        return;
    }
    if let Some(dev) = table.slot_of(handle) {
        table.key_event(
            dev,
            vk as u8,
            higanr_core::split_kb::scan_index(make_code, e0),
            u32::from(flags) & RI_KEY_BREAK == 0,
        );
    }
}

/// 台本の命令（`kbd=`）を台の表へ書き込む。
pub fn inject(raw: &RawKeys) {
    for b in raw.boards() {
        TABLE.set_keys(usize::from(b.dev), b.vks());
    }
}

/// 検査用: 登録したことにする・しないことにする。
#[cfg(test)]
pub(crate) fn test_set_registered(on: bool) {
    REG.store(if on { REG_OK } else { REG_NOT_TRIED }, Ordering::Release);
    HOOK_CUR.store(0, Ordering::Release);
    TRIES.store(0, Ordering::Release);
}

/// 検査用: プロセスのキーボードのRaw Inputの登録を消す。
#[cfg(test)]
pub(crate) fn test_unregister() {
    use windows_sys::Win32::UI::Input::RIDEV_REMOVE;
    let dev = RAWINPUTDEVICE {
        usUsagePage: PAGE_GENERIC,
        usUsage: USAGE_KEYBOARD,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: core::ptr::null_mut(),
    };
    // SAFETY: 1件の有効なRAWINPUTDEVICE（削除の指定は送り先がNULL）。サイズは構造体の大きさ。
    unsafe { RegisterRawInputDevices(&dev, 1, size_of::<RAWINPUTDEVICE>() as u32) };
}

/// 検査用: 受け方の状態と差し替えて読んでいる送り先。
#[cfg(test)]
pub(crate) fn test_mode() -> (u32, usize) {
    (
        REG.load(Ordering::Acquire),
        HOOK_CUR.load(Ordering::Acquire),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::split_kb::vk;

    const BREAK: u16 = RI_KEY_BREAK as u16;
    const E0: u16 = RI_KEY_E0 as u16;

    #[test]
    fn make_and_break_flags_set_and_clear_the_key_and_non_keys_are_ignored() {
        let t = DeviceTable::new();
        apply_key_to(&t, 0xA0, u16::from(vk::Z), 0x2C, 0);
        let s = t.snap(true);
        assert_eq!((s.words[0], s.seen), (1, 1));
        apply_key_to(&t, 0xA0, u16::from(vk::Z), 0x2C, BREAK);
        assert_eq!(t.snap(true).words[0], 0);
        // E0フラグが付いても離したときの扱いは同じ。
        apply_key_to(&t, 0xA0, u16::from(vk::RIGHT), 0x4D, E0);
        assert_eq!(t.snap(true).words[0], 0x0080);
        apply_key_to(&t, 0xA0, u16::from(vk::RIGHT), 0x4D, E0 | BREAK);
        assert_eq!(t.snap(true).words[0], 0);
        for ignored in [0u16, 0xFF, 0x100] {
            apply_key_to(&t, 0xB0, ignored, 0x10, 0);
        }
        assert_eq!(t.snap(true).seen, 1, "キーでない出来事が台を作った");
        // hDevice 0は1つの台。
        apply_key_to(&t, 0, u16::from(vk::X), 0x2D, 0);
        assert_eq!(t.snap(true).seen.count_ones(), 2);
    }

    #[test]
    fn numlock_on_numpad_with_shift_leaves_no_stuck_key_and_keeps_the_real_shift() {
        let t = DeviceTable::new();
        let d = 0xA0;
        // Shift（0x2A）を押したまま、NumLockがONのテンキー4（スキャンコード0x4B・E0無し）を押す。
        // OSは偽のShift解放（E0付き0x2A）を挟む。VKは方向キーの左になる。
        apply_key_to(&t, d, u16::from(vk::SHIFT), 0x2A, 0);
        apply_key_to(&t, d, u16::from(vk::SHIFT), 0x2A, E0 | BREAK);
        apply_key_to(&t, d, u16::from(vk::LEFT), 0x4B, 0);
        assert_eq!(
            t.snap(true).words[0],
            0x0004 | 0x0040,
            "偽の Shift が本物の Shift を消した"
        );
        // 先にShiftを離す。後でテンキーを離すときのVKはNUMPAD4になる。
        apply_key_to(&t, d, u16::from(vk::SHIFT), 0x2A, BREAK);
        apply_key_to(&t, d, u16::from(vk::NUMPAD4), 0x4B, BREAK);
        apply_key_to(&t, d, u16::from(vk::SHIFT), 0x2A, E0);
        let s = t.snap(true);
        assert_eq!((s.words[0], s.down), (0, 0), "押したままのキーが残った");
    }

    #[test]
    fn registered_keyboards_are_described_and_the_way_to_receive_is_chosen_from_them() {
        let game = 0x1234usize;
        let other = 0x9999usize;
        let own = RegEntry {
            page: 1,
            usage: 6,
            flags: 0,
            target: game,
        };
        let mouse = RegEntry {
            page: 1,
            usage: 2,
            flags: 0x100,
            target: other,
        };
        let kb_other = RegEntry {
            target: other,
            flags: 0x200,
            ..own
        };
        let focus = RegEntry { target: 0, ..own };
        assert_eq!(describe(&[], game), "0件 []");
        assert_eq!(
            describe(&[own, mouse], game),
            "2件 [1/6 フラグ0x0 送り先ゲーム, 1/2 フラグ0x100 送り先別]"
        );
        assert!(describe(&[focus], game).contains("送り先なし"));
        let mine = |h: usize| h == other;
        assert_eq!(plan(&[], game, mine), Plan::Absent, "登録が無い");
        assert_eq!(plan(&[mouse], game, mine), Plan::Absent, "マウスだけ");
        assert_eq!(plan(&[own], game, mine), Plan::OnGame);
        assert_eq!(plan(&[mouse, kb_other], game, mine), Plan::Hook(other));
        assert_eq!(
            plan(&[focus], game, mine),
            Plan::Disabled(Why::NullTarget),
            "NULL の送り先"
        );
        assert_eq!(
            plan(&[kb_other], game, |_| false),
            Plan::Disabled(Why::OtherProcess),
            "自分のプロセスでない・壊れたウィンドウ"
        );
        for why in [
            Why::NullTarget,
            Why::OtherProcess,
            Why::Unreadable,
            Why::HookRefused(5),
        ] {
            assert!(disabled_text(why).contains("無効"), "{why:?}");
        }
        assert!(disabled_text(Why::HookRefused(5)).contains("エラー 5"));
    }

    #[test]
    fn a_registered_state_of_a_foreign_window_leaves_the_table_dead() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        TABLE.reset();
        REG.store(REG_FOREIGN, Ordering::Release);
        TABLE.set_key(0, vk::Z, true);
        on_message(WM_ACTIVATEAPP, 0, 0);
        assert_eq!(TABLE.snap(true).words[0], 1, "無効なのに消す処理が動いた");
        assert!(status_text().contains("無効"));
        test_set_registered(false);
        TABLE.reset();
    }

    #[test]
    fn messages_are_ignored_until_registered_and_deactivation_clears_every_key() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        TABLE.reset();
        TABLE.set_key(0, vk::Z, true);
        test_set_registered(false);
        on_message(WM_ACTIVATEAPP, 0, 0);
        assert_eq!(TABLE.snap(true).words[0], 1, "登録前に消した");
        test_set_registered(true);
        on_message(WM_ACTIVATEAPP, 1, 0);
        assert_eq!(TABLE.snap(true).words[0], 1, "前面になったのに消した");
        // 偽のHRAWINPUTは読めずに何も起きない。
        on_message(WM_INPUT, 0, 0x10);
        assert_eq!(TABLE.snap(true).seen, 1);
        on_message(WM_ACTIVATEAPP, 0, 0);
        assert_eq!(TABLE.snap(true).words[0], 0, "前面を失ったのに押下が残った");
        test_set_registered(false);
        TABLE.reset();
    }

    #[test]
    fn script_events_replace_a_devices_keys() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        TABLE.reset();
        let s = higanr_core::script::parse("scene=1 kbd=0:Z+LEFT,1:X\n").unwrap();
        let raw = s.steps[0].raw.unwrap();
        inject(&raw);
        let snap = TABLE.snap(true);
        assert_eq!((snap.words[0], snap.words[1], snap.seen), (0x41, 2, 0b11));
        let s = higanr_core::script::parse("scene=1 kbd=0:none\n").unwrap();
        inject(&s.steps[0].raw.unwrap());
        assert_eq!(TABLE.snap(true).words[0], 0);
        TABLE.reset();
    }

    // ---- 本物のウィンドウでの受け方 ----

    use std::sync::mpsc;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, RegisterClassExW, WNDCLASSEXW,
    };

    /// 検査のウィンドウの元の手続きが受け取った`WM_INPUT`の数（差し替えたウィンドウが元の手続きへ渡したかどうか）。
    /// 種類A（ゲームのウィンドウ側と登録の作り直し後）と種類B（別のスレッドのウィンドウ）で元の手続きを分けて、渡し先がウィンドウごとに正しいかを見分ける。
    static PASSED_INPUT: AtomicU32 = AtomicU32::new(0);
    static PASSED_INPUT_B: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn counting_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            PASSED_INPUT.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: 引数はこの手続きにWindowsが渡した引数そのもの。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    unsafe extern "system" fn counting_proc_b(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            PASSED_INPUT_B.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: 引数はこの手続きにWindowsが渡した引数そのもの。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    type Proc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    fn make_window(name: &str, proc: Proc) -> HWND {
        let class = wide(name);
        // SAFETY: 引数の無い問い合わせ（自分のモジュールのハンドル）。
        let inst = unsafe { GetModuleHandleW(core::ptr::null()) };
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: inst,
            hIcon: core::ptr::null_mut(),
            hCursor: core::ptr::null_mut(),
            hbrBackground: core::ptr::null_mut(),
            lpszMenuName: core::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: core::ptr::null_mut(),
        };
        // SAFETY: `wc`は有効なWNDCLASSEXW（クラス名は0終わりのUTF-16でこの呼び出しの間生きている）。登録済みのクラスなら0が返るだけ。
        unsafe { RegisterClassExW(&wc) };
        // SAFETY: クラスは上で登録した自分のもの。ウィンドウは表示しない。検査が所有する。
        unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                10,
                10,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                inst,
                core::ptr::null(),
            )
        }
    }

    fn test_window() -> HWND {
        make_window("HigRawTest", counting_proc)
    }

    /// 別のスレッドが作るウィンドウ（DirectInputのウィンドウがゲームのスレッドとは別の場合）。`stop`に送るとウィンドウが破棄されてスレッドが終わる。
    struct OtherThreadWindow {
        hwnd: usize,
        stop: mpsc::Sender<()>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl OtherThreadWindow {
        fn new() -> Self {
            let (tx, rx) = mpsc::channel();
            let (stop, wait) = mpsc::channel::<()>();
            let join = std::thread::spawn(move || {
                let h = make_window("HigRawTestB", counting_proc_b);
                tx.send(h as usize).unwrap();
                let _ = wait.recv();
                // SAFETY: このスレッドが作った検査のウィンドウ。
                unsafe { DestroyWindow(h) };
            });
            OtherThreadWindow {
                hwnd: rx.recv().unwrap(),
                stop,
                join: Some(join),
            }
        }
    }

    impl Drop for OtherThreadWindow {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    fn deliver(hwnd: usize, msg: u32, wparam: usize, lparam: isize) -> LRESULT {
        // SAFETY: `GetWindowLongPtrW`が返すのはウィンドウの今の手続き。`CallWindowProcW`はそれを直に呼ぶ（検査のスレッドで）。
        unsafe {
            let cur = GetWindowLongPtrW(hwnd as HWND, GWLP_WNDPROC) as usize;
            CallWindowProcW(
                core::mem::transmute::<usize, WNDPROC>(cur),
                hwnd as HWND,
                msg,
                wparam,
                lparam,
            )
        }
    }

    fn list_now() -> Vec<RegEntry> {
        let mut list = [EMPTY_ENTRY; LIST_MAX];
        let n = registered(&mut list).expect("登録の一覧を読めない");
        list[..n].to_vec()
    }

    fn set_test_key(k: Option<RawKey>) {
        *TEST_KEY.lock().unwrap_or_else(|e| e.into_inner()) = k;
    }

    fn z_on(handle: usize) -> RawKey {
        RawKey {
            handle,
            vk: u16::from(vk::Z),
            make_code: 0x2C,
            flags: 0,
        }
    }

    fn clean() {
        set_test_key(None);
        test_unregister();
        test_set_registered(false);
        TABLE.reset();
        PASSED_INPUT.store(0, Ordering::SeqCst);
        PASSED_INPUT_B.store(0, Ordering::SeqCst);
    }

    struct Clean;
    impl Drop for Clean {
        fn drop(&mut self) {
            clean();
        }
    }

    fn register_kb(h: usize, flags: u32) -> i32 {
        let dev = RAWINPUTDEVICE {
            usUsagePage: PAGE_GENERIC,
            usUsage: USAGE_KEYBOARD,
            dwFlags: flags,
            hwndTarget: h as HWND,
        };
        // SAFETY: 1件の有効なRAWINPUTDEVICE（自分のプロセスのウィンドウ、またはNULL）。
        unsafe { RegisterRawInputDevices(&dev, 1, size_of::<RAWINPUTDEVICE>() as u32) }
    }

    /// 検査のウィンドウ（`game`）を破棄する。
    fn destroy(h: HWND) {
        // SAFETY: 検査が作った自分のウィンドウ。
        unsafe { DestroyWindow(h) };
    }

    #[test]
    fn on_the_getkeystate_path_a_registration_on_another_threads_window_is_read_by_replacing_its_procedure()
     {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        let di = OtherThreadWindow::new();
        // DirectInputと同じ形: 別のスレッドのウィンドウへ（フラグ0x200）登録する。
        assert_ne!(register_kb(di.hwnd, 0x200), 0);
        let before = list_now();
        assert_eq!(before.len(), 1);
        establish(game);
        assert_eq!(test_mode(), (REG_HOOKED, di.hwnd), "差し替えられなかった");
        assert_eq!(list_now(), before, "登録の一覧が変わった（登録を奪った）");
        assert!(status_text().contains("登録済みの送り先で受け取り中"));
        // 差し替えたウィンドウに届いたWM_INPUTは読んだ後で元の手続きへ渡る（偽のハンドルは読めず、表は変わらない）。
        assert_eq!(PASSED_INPUT_B.load(Ordering::SeqCst), 0);
        deliver(di.hwnd, WM_INPUT, 0, 0x10);
        assert_eq!(
            PASSED_INPUT_B.load(Ordering::SeqCst),
            1,
            "元の手続きへ渡さなかった"
        );
        assert_eq!(TABLE.snap(true).seen, 0);
        // 読めたイベントは表に書き込まれ、元の手続きへも渡る。差し替えで読んでいる間は、ゲームのウィンドウ側の経路はWM_INPUTを読まない。
        set_test_key(Some(z_on(0xAAA)));
        let events = EVENTS.load(Ordering::Relaxed);
        deliver(di.hwnd, WM_INPUT, 0, 0x10);
        let s = TABLE.snap(true);
        assert_eq!((s.seen.count_ones(), s.words[0]), (1, 1), "表に入らない");
        assert_eq!(EVENTS.load(Ordering::Relaxed), events + 1);
        assert_eq!(PASSED_INPUT_B.load(Ordering::SeqCst), 2);
        on_message(WM_INPUT, 0, 0x10);
        assert_eq!(EVENTS.load(Ordering::Relaxed), events + 1, "二重に読んだ");
        set_test_key(None);
        TABLE.reset();
        // 差し替えたのは送り先のウィンドウだけ。
        assert_eq!(prev_of(game as usize), 0);
        assert_ne!(prev_of(di.hwnd), 0);
        // 前面を失う通知はゲームのウィンドウ側が受けて押下を消す。
        TABLE.set_key(0, vk::Z, true);
        on_message(WM_ACTIVATEAPP, 0, 0);
        assert_eq!(TABLE.snap(true).words[0], 0);
        destroy(game);
    }

    #[test]
    fn on_the_getkeystate_path_it_registers_on_the_game_window_itself() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        assert!(list_now().iter().all(|e| e.usage != USAGE_KEYBOARD));
        begin(game, Some(0), false);
        assert_eq!(test_mode(), (REG_OK, 0));
        assert!(
            list_now()
                .iter()
                .any(|e| (e.page, e.usage, e.target) == (1, 6, game as usize)),
            "自分で登録していない"
        );
        // 自分で登録している間はWM_INPUTをゲームのウィンドウ側で読む（偽のハンドルは読めない）。
        on_message(WM_INPUT, 0, 0x10);
        assert_eq!(TABLE.snap(true).seen, 0);
        set_test_key(Some(z_on(0xBBB)));
        on_message(WM_INPUT, 0, 0x10);
        assert_eq!(
            TABLE.snap(true).words[0],
            1,
            "自分で登録した WM_INPUT を読まない"
        );
        destroy(game);
    }

    #[test]
    fn a_null_target_or_a_refused_hook_disables_it_without_touching_the_registration() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        // SAFETY相当: 送り先がNULLの登録（有効なRAWINPUTDEVICE）。
        assert_ne!(register_kb(0, 0), 0);
        let before = list_now();
        begin(game, Some(0), false);
        assert_eq!(test_mode().0, REG_FOREIGN, "NULL の送り先で無効にならない");
        assert_eq!(list_now(), before, "登録を変えた");
        assert!(status_text().contains("無効"));
        // 壊れたウィンドウの手続きは差し替えられない。差し替えが断られたら無効にする。
        assert!(install_hook(0x10 as HWND).is_err());
        test_set_registered(false);
        assert_eq!(hook_outcome(game, 0x10, Err(5)), "無効");
        assert_eq!(test_mode(), (REG_FOREIGN, 0));
        // 無効のときは聞き始める前にポーリングの待ちを解いても何もしない。
        refresh();
        assert_eq!(test_mode().0, REG_FOREIGN);
        assert!(!watching());
        destroy(game);
    }

    #[test]
    fn on_the_directinput_path_it_never_registers_itself_and_waits_without_disabling() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        assert!(list_now().iter().all(|e| e.usage != USAGE_KEYBOARD));
        for device in [Some(0x0090_32A4), Some(1), None] {
            test_set_registered(false);
            begin(game, device, false);
            assert_eq!(test_mode(), (REG_WAITING, 0), "{device:?}");
            assert!(
                list_now().iter().all(|e| e.usage != USAGE_KEYBOARD),
                "{device:?}: DirectInput の道で自分で登録した"
            );
            assert!(watching());
        }
        assert!(status_text().contains("登録待ち"));
        // 登録がまだ出ていない間はポーリングを重ねても待ちのまま（無効にしない・登録しない）。
        let mut w = Watch::new();
        let first = w.step(game).expect("最初の読みは変化として書く");
        assert!(first.contains("0件"), "{first}");
        for _ in 0..3 {
            assert_eq!(w.step(game), None, "変わっていないのに行を出した");
        }
        assert_eq!(test_mode(), (REG_WAITING, 0));
        assert!(list_now().iter().all(|e| e.usage != USAGE_KEYBOARD));
        destroy(game);
    }

    #[test]
    fn on_the_directinput_path_a_registration_that_appears_later_is_hooked_and_only_changes_are_logged()
     {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        let first = OtherThreadWindow::new();
        let second = test_window();
        begin(game, Some(0x0090_32A4), false);
        let mut w = Watch::new();
        assert!(w.step(game).is_some());
        // DirectInputが前面になってAcquireし、別のスレッドのウィンドウへ登録した。
        assert_ne!(register_kb(first.hwnd, 0x200), 0);
        let after_register = list_now();
        let line = w.step(game).expect("登録が出たのに行を出さない");
        assert!(line.contains("1件") && line.contains("送り先別"), "{line}");
        assert_eq!(test_mode(), (REG_HOOKED, first.hwnd), "差し替えない");
        assert_eq!(list_now(), after_register, "登録を変えた");
        assert_eq!(w.step(game), None, "変わっていないのに行を出した");
        // 前面を外れてUnacquireし、登録が外れた。差し替えた手続きはそのまま読み続ける。
        test_unregister();
        let line = w.step(game).expect("登録が外れたのに行を出さない");
        assert!(line.contains("0件"), "{line}");
        assert_eq!(test_mode(), (REG_HOOKED, first.hwnd));
        assert_eq!(w.step(game), None);
        // 次に前面になったとき登録のウィンドウが作り直されていた。
        assert_ne!(register_kb(second as usize, 0x200), 0);
        assert!(w.step(game).is_some());
        assert_eq!(
            test_mode(),
            (REG_HOOKED, second as usize),
            "差し替え直さない"
        );
        assert_ne!(prev_of(second as usize), 0);
        assert_ne!(
            prev_of(first.hwnd),
            0,
            "前のウィンドウの元の手続きを忘れた（前のウィンドウは生きている）"
        );
        deliver(second as usize, WM_INPUT, 0, 0x10);
        assert_eq!(PASSED_INPUT.load(Ordering::SeqCst), 1);
        deliver(first.hwnd, WM_INPUT, 0, 0x10);
        assert_eq!(
            (
                PASSED_INPUT.load(Ordering::SeqCst),
                PASSED_INPUT_B.load(Ordering::SeqCst)
            ),
            (1, 1),
            "ウィンドウごとの元の手続きへ渡していない"
        );
        // 送り先がNULLの登録は無効（ポーリングも終わる）。
        assert_ne!(register_kb(0, 0), 0);
        assert!(w.step(game).is_some());
        assert_eq!(test_mode().0, REG_FOREIGN);
        assert!(!watching());
        destroy(second);
        destroy(game);
    }

    #[test]
    fn on_the_directinput_path_a_registration_on_the_game_window_is_read_without_registering() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        begin(game, Some(1), false);
        let mut w = Watch::new();
        w.step(game);
        assert_ne!(register_kb(game as usize, 0x200), 0);
        let before = list_now();
        assert!(w.step(game).is_some());
        assert_eq!(test_mode(), (REG_ON_GAME, 0));
        assert_eq!(list_now(), before);
        set_test_key(Some(z_on(0xCCC)));
        on_message(WM_INPUT, 0, 0x10);
        assert_eq!(
            TABLE.snap(true).words[0],
            1,
            "ゲームのウィンドウ側で読まない"
        );
        destroy(game);
    }

    #[test]
    fn the_real_watcher_thread_hooks_a_late_registration_and_refresh_wakes_it_early_and_it_ends_when_disabled()
     {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        let win = OtherThreadWindow::new();
        begin(game, Some(1), true);
        assert_eq!(test_mode().0, REG_WAITING);
        // ポーリングの処理が最初の読みを済ませて待ちに入るのを待つ。
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(WATCHERS.load(Ordering::Acquire), 1);
        // 間隔（500 ms）より早く差し替わるのは`refresh`が待ちを解いたとき。
        assert_ne!(register_kb(win.hwnd, 0x200), 0);
        refresh();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while test_mode().0 != REG_HOOKED && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            test_mode(),
            (REG_HOOKED, win.hwnd),
            "待ちを解いても差し替わらない"
        );
        // NULLの送り先の登録で無効になり、ポーリングの処理は終わる。
        assert_ne!(register_kb(0, 0), 0);
        refresh();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while WATCHERS.load(Ordering::Acquire) != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(test_mode().0, REG_FOREIGN);
        assert_eq!(
            WATCHERS.load(Ordering::Acquire),
            0,
            "無効になっても動き続けた"
        );
        destroy(game);
    }

    #[test]
    fn list_changes_are_logged_up_to_the_limit_then_only_counted() {
        let _g = crate::text_input_win::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _c = Clean;
        clean();
        let game = test_window();
        let win = OtherThreadWindow::new();
        begin(game, Some(1), false);
        let mut w = Watch::new();
        let changes = CHANGES.load(Ordering::Relaxed);
        let total = MAX_CHANGE_LOGS as usize + 10;
        let mut lines = Vec::new();
        for i in 0..total {
            // 前面の出入りで登録が付いたり外れたりする。
            if i % 2 == 0 {
                register_kb(win.hwnd, 0x200);
            } else {
                test_unregister();
            }
            if let Some(l) = w.step(game) {
                lines.push(l);
            }
        }
        assert_eq!(
            lines.len(),
            MAX_CHANGE_LOGS as usize,
            "上限まで書いていない"
        );
        assert!(lines.last().unwrap().contains("以後の変化は記録しません"));
        assert!(lines[..lines.len() - 1].iter().all(|l| !l.contains("以後")));
        assert_eq!(
            CHANGES.load(Ordering::Relaxed) - changes,
            total as u32,
            "上限を越えた変化を数えていない"
        );
        assert!(status_text().contains("Raw Input の登録の変化"));
        destroy(game);
    }
}
