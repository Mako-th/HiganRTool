//! Win32の層（unsafeはこのファイルに閉じ込める。1つずつ`// SAFETY:`）。
//! 自分のモジュールのパス・exeの位置・時刻・二重注入のミューテックス・スレッドを1本作ること・デバッガへのメッセージ出力を扱う。
//! 自分のプロセスのメモリの読み取りも扱う（`VirtualQuery`でページをチェックしてから読む）。
//! ゲームのメモリに書くAPIはここに無い（書く層は`hook_win.rs`）。
//! ウィンドウを作らずネットワークにも出ない。

// stdのProcessPrngの遅延読み込みが失敗したときのフック（build.rsの /DELAYLOADと対）。
higanr_prng_shim::install_delay_load_hook!();

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::windows::ffi::OsStringExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use core::ffi::c_void;

use higanr_core::ansi::Ansi;
use higanr_core::log_scrub::{Scrub, ScrubWrite};
use higanr_core::pe::TH09_V150A;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HMODULE, SYSTEMTIME,
};
use windows_sys::Win32::Globalization::{
    CP_UTF8, GetACP, MB_ERR_INVALID_CHARS, MultiByteToWideChar, WC_NO_BEST_FIT_CHARS,
    WideCharToMultiByte,
};
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, OSVERSIONINFOW};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, CreateThread, IsProcessorFeaturePresent, PF_XMMI64_INSTRUCTIONS_AVAILABLE,
};
use windows_sys::core::BOOL;

use crate::hook_win::RealHook;
use crate::log_mode::detail_out;
use crate::names::{
    KEEP_LOG_DAYS, LOG_NAME_ATTEMPTS, LocalTime, daily_log_file_name, is_expired_log,
    is_same_or_inside, is_scripted_run, log_file_name, mutex_name,
};
use crate::probe::{self, Env, IdentOutcome};
use crate::record_win::RealRecord;
use crate::script_win::RealScriptHost;
use crate::session::{self, End, ScriptHost, Sites};
use crate::th09_addr::{FORBIDDEN, HOOK_SITE, IMAGE_BASE, SITES, WINDOW_ADDR};
use crate::ticks::COPY_TABLE;

/// `GetModuleFileNameW`が受け付ける最長（拡張パスの上限）。
const MAX_WIDE_PATH: usize = 32_768;

/// モジュールのパス。`module`がNULLなら自分のプロセスのexe。日本語を含みうるのでWのAPIで取る。
pub fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut buf: Vec<u16> = vec![0; 260];
    loop {
        // SAFETY: `buf`は書き込み可能な領域で、長さをそのまま渡している。
        // `module`はNULLか自分のDLLのハンドル（このDLLは自分を解放しない）。
        let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            buf.truncate(n);
            return Some(PathBuf::from(OsString::from_wide(&buf)));
        }
        // n == buf.len()は切り詰められたことを示す。広げて取り直す。
        if buf.len() >= MAX_WIDE_PATH {
            return None;
        }
        buf.resize((buf.len() * 2).min(MAX_WIDE_PATH), 0);
    }
}

/// アドレス`addr`が含まれるモジュールのパス（参照カウントは増やさない）。
/// どのモジュールにも含まれなければ`None`。
#[cfg_attr(test, allow(dead_code))] // 使うのは`window_win::original`（検査の構成には無い）だけ
pub fn module_of_addr(addr: u32) -> Option<PathBuf> {
    let mut module: HMODULE = core::ptr::null_mut();
    // SAFETY: フラグは「アドレスから取得する」＋「参照カウントを増やさない」（閉じなくてよい）。第2引数はアドレスを指す疑似ポインタで中身は読まれない。
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as usize as *const u16,
            &mut module,
        )
    };
    if ok == 0 || module.is_null() {
        return None;
    }
    module_path(module)
}

/// Wineのバージョン（`ntdll.dll`が`wine_get_version`を持つときだけ）。Windowsでは`None`。ログの1行に使う。ゲームのスレッドでは呼ばない。
pub fn wine_version() -> Option<String> {
    let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
    // SAFETY: 0終わりのUTF-16の名前。ntdllは全プロセスに読み込み済み。参照カウントは増やさない。
    let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: 読み込み済みのモジュールのハンドルと0終わりのASCIIの名前。無ければNoneが返るだけ。
    let proc = unsafe { GetProcAddress(module, c"wine_get_version".as_ptr().cast()) }?;
    // SAFETY: Wineのntdllがこの名前でエクスポートする関数は引数なしで静的なC文字列へのポインタを返す（cdecl。i686のextern "C" と同じ）。
    let get: extern "C" fn() -> *const core::ffi::c_char = unsafe { core::mem::transmute(proc) };
    let p = get();
    if p.is_null() {
        return None;
    }
    // SAFETY: 上の関数が返す0終わりの静的な文字列。
    let s = unsafe { core::ffi::CStr::from_ptr(p) }.to_string_lossy();
    let s: String = s
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(32)
        .collect();
    (!s.is_empty()).then_some(s)
}

/// OSのバージョン（`ntdll!RtlGetVersion`。`GetVersionEx`と違いexeの互換モードの宣言で値が変わらない）。ゲームのスレッドでは呼ばない。
pub fn os_version() -> Option<probe::OsVersion> {
    let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
    // SAFETY: 0終わりのUTF-16の名前。ntdllは全プロセスに読み込み済み。参照カウントは増やさない。
    let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: 読み込み済みのモジュールのハンドルと0終わりのASCIIの名前。無ければNoneが返るだけ。
    let proc = unsafe { GetProcAddress(module, c"RtlGetVersion".as_ptr().cast()) }?;
    // SAFETY: RtlGetVersionは`NTSTATUS NTAPI (PRTL_OSVERSIONINFOW)`（stdcall。i686のextern "system"）。
    let get: unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32 =
        unsafe { core::mem::transmute(proc) };
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: core::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: 大きさのフィールドを埋めたOSVERSIONINFOWを渡す。関数はそれを書くだけ。
    let status = unsafe { get(&mut info) };
    (status >= 0).then_some(probe::OsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
    })
}

/// CPUの対応機能（SSE2, AVX2）を返す。
/// SSE2はOSが報告する値を使う（i686のビルドはSSE2を前提にしているので、無ければここまで来ない）。
/// AVX2はCPUIDとOSの保存の対応（XGETBV）の両方で判定する。
pub fn cpu_features() -> (bool, bool) {
    // SAFETY: 引数は機能の番号だけ。失敗しない。
    let sse2 = unsafe { IsProcessorFeaturePresent(PF_XMMI64_INSTRUCTIONS_AVAILABLE) } != 0;
    (sse2, std::arch::is_x86_feature_detected!("avx2"))
}

pub fn exe_image_base() -> usize {
    // SAFETY: 引数NULLは自分のexeを意味する。返るハンドルは参照カウントを増やさない。
    unsafe { GetModuleHandleW(core::ptr::null()) as usize }
}

/// ページの保護が読み取り可能かどうか。`PAGE_GUARD`は読むと例外が一度起きて保護が外れる（ゲームの状態を変えうる）ので読み取り不可として扱う。
/// 修飾は下位8ビットで見る。
pub fn is_readable_protect(protect: u32) -> bool {
    if protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
        return false;
    }
    matches!(
        protect & 0xFF,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    )
}

/// 自分のプロセスのメモリ`[addr, addr + len)`を読む。ページがすべてコミット済みで読み取り可能な保護でなければ`None`。読むだけで保護は変えない。
pub fn read_own_memory(addr: u32, len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; len];
    read_own_into(addr, &mut out).then_some(out)
}

/// `[addr, addr + buf.len())`を`buf`に読み込む。**ヒープを使わない**（フックの手順がスレッドを止めている間に読み直すため）。
/// ページがすべてコミット済みで読み取り可能な保護でなければfalse（検証し終わるまで1バイトも書かない）。
pub fn read_own_into(addr: u32, buf: &mut [u8]) -> bool {
    readable_range(addr as usize, buf.len()) && {
        for (i, b) in buf.iter_mut().enumerate() {
            // SAFETY: 範囲のページを直前にVirtualQueryでコミット済み・読み取り可能な保護と検証した。
            // th09.exeのイメージのページは解放されない。
            // このDLLが保護を変えるのは`hook_win`だけ。それも読み取り可能な保護の間を行き来するだけ。
            // 検証してから読むまでに別のDLLが保護を外すことは起きない前提（ローダーがサスペンドで起動して読ませるので、他の注入物は並んでいない）。
            // volatileで読むのは、ゲームのスレッドが書き換えうるアドレスの読み取りが最適化で省かれないようにするため。
            *b = unsafe { core::ptr::read_volatile((addr as usize + i) as *const u8) };
        }
        true
    }
}

/// `[start, start + len)`のページがすべてコミット済みで読み取り可能な保護かどうか。ヒープを使わない。
fn readable_range(start: usize, len: usize) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let mut cur = start;
    while cur < end {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: VirtualQueryはアドレスの中身を読まない。領域の情報を十分な大きさの`mbi`に書くだけ。
        let n = unsafe {
            VirtualQuery(
                cur as *const c_void,
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 || mbi.State != MEM_COMMIT || !is_readable_protect(mbi.Protect) {
            return false;
        }
        let Some(region_end) = (mbi.BaseAddress as usize).checked_add(mbi.RegionSize) else {
            return false;
        };
        if region_end <= cur {
            return false; // 進まない答えは信じない（無限に回らない）
        }
        cur = region_end;
    }
    true
}

pub fn local_time() -> LocalTime {
    let mut st = SYSTEMTIME::default();
    // SAFETY: `st`はGetLocalTimeが書くSYSTEMTIMEそのもの。
    unsafe { GetLocalTime(&mut st) };
    LocalTime {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
        millis: st.wMilliseconds,
    }
}

/// コードページ番号で決まるANSIの変換（`MultiByteToWideChar` / `WideCharToMultiByte`）。
pub struct CodePage(pub u32);

/// このプロセスのANSIコードページ（`GetACP`）。ゲームの`CreateFileA`が読むコードページと同じ。
pub struct AcpAnsi;

impl Ansi for AcpAnsi {
    fn encode(&self, s: &str) -> Option<Vec<u8>> {
        // SAFETY: 引数の無い呼び出し。
        CodePage(unsafe { GetACP() }).encode(s)
    }

    fn decode(&self, b: &[u8]) -> Option<String> {
        // SAFETY: 引数の無い呼び出し。
        CodePage(unsafe { GetACP() }).decode(b)
    }
}

impl Ansi for CodePage {
    fn encode(&self, s: &str) -> Option<Vec<u8>> {
        if s.is_empty() {
            return Some(Vec::new());
        }
        let wide: Vec<u16> = s.encode_utf16().collect();
        let wlen = i32::try_from(wide.len()).ok()?;
        // UTF-8ではフラグと既定の文字の指定を渡せない（渡すと失敗する）。
        let utf8 = self.0 == CP_UTF8;
        let flags = if utf8 { 0 } else { WC_NO_BEST_FIT_CHARS };
        let mut used_default: BOOL = 0;
        let used_ptr: *mut BOOL = if utf8 {
            core::ptr::null_mut()
        } else {
            &mut used_default
        };
        // SAFETY: `wide`は`wlen`個読み取り可能。出力先はNULL・0（必要な長さだけを返す呼び方）。`used_ptr`はNULLか書き込み可能なBOOL。
        let n = unsafe {
            WideCharToMultiByte(
                self.0,
                flags,
                wide.as_ptr(),
                wlen,
                core::ptr::null_mut(),
                0,
                core::ptr::null(),
                used_ptr,
            )
        };
        if n <= 0 {
            return None;
        }
        let mut out = vec![0u8; usize::try_from(n).ok()?];
        // SAFETY: 入力は上と同じ。出力先は`n`バイト書き込み可能。
        let wrote = unsafe {
            WideCharToMultiByte(
                self.0,
                flags,
                wide.as_ptr(),
                wlen,
                out.as_mut_ptr(),
                n,
                core::ptr::null(),
                used_ptr,
            )
        };
        // 代替の文字や別の文字に置き換わったもの（CP932の`¥` → `\`など）は逆変換して元と比べて断る。
        if wrote != n || used_default != 0 || self.decode(&out).as_deref() != Some(s) {
            return None;
        }
        Some(out)
    }

    fn decode(&self, b: &[u8]) -> Option<String> {
        if b.is_empty() {
            return Some(String::new());
        }
        let len = i32::try_from(b.len()).ok()?;
        // SAFETY: `b`は`len`バイト読み取り可能。出力先はNULL・0（必要な長さだけを返す呼び方）。
        let n = unsafe {
            MultiByteToWideChar(
                self.0,
                MB_ERR_INVALID_CHARS,
                b.as_ptr(),
                len,
                core::ptr::null_mut(),
                0,
            )
        };
        if n <= 0 {
            return None;
        }
        let mut wide = vec![0u16; usize::try_from(n).ok()?];
        // SAFETY: 入力は上と同じ。出力先は`n`個のu16を書き込み可能。
        let wrote = unsafe {
            MultiByteToWideChar(
                self.0,
                MB_ERR_INVALID_CHARS,
                b.as_ptr(),
                len,
                wide.as_mut_ptr(),
                n,
            )
        };
        if wrote != n {
            return None;
        }
        String::from_utf16(&wide).ok()
    }
}

/// デバッガ（DebugViewなど）へのメッセージ出力。ログを開けないときの最後の通知手段。ウィンドウは出さない。
pub fn debug_out(s: &str) {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide`は0で終わるUTF-16の列で呼び出しの間生きている。
    unsafe { OutputDebugStringW(wide.as_ptr()) };
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexRefusal {
    /// 同じ名前が既にある（同じPIDに2本目が読まれた）。
    AlreadyExists,
    /// 作れなかった（`GetLastError`の値）。
    Failed(u32),
}

/// 名前つきのミューテックスを作る。既にあれば開いてしまったハンドルを閉じて断る。
/// 返したハンドルは閉じない（プロセスが終わるまで名前を残して2本目を断り続けるため）。
pub fn acquire_named_mutex(name: &str) -> Result<HANDLE, MutexRefusal> {
    let wide = to_wide_nul(name);
    // SAFETY: 属性はNULL。初めから持たない（FALSE）。名前は呼び出しの間生きている0終わりのUTF-16。
    let h = unsafe { CreateMutexW(core::ptr::null(), 0, wide.as_ptr()) };
    // SAFETY: 引数の無い呼び出し。CreateMutexWの直後に読む。
    let last = unsafe { GetLastError() };
    if h.is_null() {
        return Err(MutexRefusal::Failed(last));
    }
    if last == ERROR_ALREADY_EXISTS {
        close_handle(h);
        return Err(MutexRefusal::AlreadyExists);
    }
    Ok(h)
}

pub fn close_handle(h: HANDLE) {
    // SAFETY: 呼び出し元は自分が開いてまだ閉じていないハンドルだけを渡す。
    unsafe { CloseHandle(h) };
}

/// DLL_PROCESS_ATTACHでの処理（ローダーロックの下）: ミューテックスを取って読み取りのスレッドを作るだけ。待たない。
/// falseを返すとDllMainがFALSEを返し、DLLは読み込まれない。
pub fn on_process_attach(module: HMODULE) -> bool {
    let name = mutex_name(std::process::id());
    let mutex = match acquire_named_mutex(&name) {
        Ok(h) => h,
        Err(MutexRefusal::AlreadyExists) => {
            debug_out("HiganRTool: 同じプロセスに既に読まれている（2 本目は読まない）");
            return false;
        }
        Err(MutexRefusal::Failed(code)) => {
            // 1本目と判定できないので読み込まない（2本が同じ地点を監視するより、始めない方が安全）。
            debug_out(&format!(
                "HiganRTool: 二重注入のミューテックスを作れない（GetLastError = {code}）"
            ));
            return false;
        }
    };
    // ウィンドウモードのウィンドウを作る呼び出しの差し替え（iniの`[window]`）。ゲームのメインスレッドが走り出す前のここでしか間に合わない。
    // iniはここで読まない（ローダーロック）。失敗してもDLLの読み込みは止めない。
    crate::window_win::install(module);
    // 起動時の計測のBeginSceneのガード。同じくDllMainの中でしか間に合わない。失敗しても読み込みは止めない。
    crate::calib_guard_win::install();
    // フルスクリーンの垂直同期の答え（iniの`[pacing] vsync`。iniは最初に答えるときに読む）と、デバイスが失われている間のメインループのスリープ。
    crate::vsync_answer_win::install(module);
    crate::lost_sleep_win::install();
    // SAFETY: `probe_thread`はLPTHREAD_START_ROUTINEの形。引数は自分のDLLのハンドル。このDLLは自分を解放しない。
    // 属性はNULL・スタックの大きさは既定・すぐ走らせる・スレッドIDは不要。
    let thread = unsafe {
        CreateThread(
            core::ptr::null(),
            0,
            Some(probe_thread),
            module as *const c_void,
            0,
            core::ptr::null_mut(),
        )
    };
    if thread.is_null() {
        debug_out("HiganRTool: 読み取りのスレッドを起こせない");
        close_handle(mutex);
        return false;
    }
    // スレッドのハンドルは要らない（閉じてもスレッドは止まらない）。
    close_handle(thread);
    true
}

/// 読み取りのスレッドの入口。パニックはここで受けてOSへは出さない。
extern "system" fn probe_thread(param: *mut c_void) -> u32 {
    let r = catch_unwind(AssertUnwindSafe(|| run_probe_thread(param as HMODULE)));
    if r.is_err() {
        debug_out("HiganRTool: 読み取りのスレッドでパニックを受けた。スレッドを終える");
    }
    0
}

fn run_probe_thread(module: HMODULE) {
    let exe = module_path(core::ptr::null_mut());
    let dll = module_path(module);
    let MainLog {
        mut file,
        run_path,
        fallback_note,
    } = match open_log(dll.as_deref(), exe.as_deref()) {
        Ok(f) => f,
        Err(why) => {
            debug_out(&format!(
                "HiganRTool: ログを開けない（{why}）。何もせずにスレッドを終える"
            ));
            return;
        }
    };
    // 例外の記録はこの後のどこで落ちても残るように、ログを開いた直後に有効にする。
    let veh = crate::crash_win::install(&file);
    // 区切りの行はウィンドウとパッドの書き込み依頼の行（`DllMain`から積まれる）より先にラッパーの外から1回で書く。
    let stem = run_path.file_stem().map(|s| s.to_string_lossy());
    let header = format!(
        "{}\n",
        higanr_core::log_names::run_header(
            std::process::id(),
            &local_time().display(),
            stem.as_deref().unwrap_or("")
        )
    );
    let _ = file.write_all(header.as_bytes());
    if let Some(note) = fallback_note {
        let _ = writeln!(file, "{note}");
    }
    // ウィンドウとパッドの行（別のスレッドが書き込みを依頼する）は、このラッパーが本ログの行の先頭ごとに先に書く。
    // 書く行は1行ずつユーザー名・フォルダ・IPのアドレスを伏せてからファイルへ渡す。
    let mut file = crate::main_log::wrap(ScrubWrite::new(
        file,
        log_scrub(exe.as_deref(), dll.as_deref()),
    ));
    // tickごとの行などは本ログの隣のファイルに書く。ファイル名は回ごとの名前で始める。
    let exe_dir = exe.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    // 台本の読み込み（DLLのフォルダのscript.txtを読み、そのファイルを本ログの隣へ移す。ゲームのフォルダの中には触らない）。
    let mut script = RealScriptHost {
        dll_dir: dll.as_deref().and_then(Path::parent).map(Path::to_path_buf),
        run_path: Some(run_path.clone()),
        exe_dir: exe_dir.clone(),
    };
    // 配布バージョンでiniが無ければ最初に読む前に既定を書き出す（開発用のビルドは書かない）。
    let seed_line = script
        .seed_default_ini(crate::ini_seed::default_for_build(cfg!(feature = "dist")).as_deref())
        .log_line();
    // 簡潔か詳細かをログに書き始める前に決める（iniが読めない理由は後の`ini:`の行に書く）。
    let choice = higanr_core::log_cfg::choose(
        script.read_net_ini().ok().flatten().as_deref(),
        crate::log_mode::default_detail(cfg!(feature = "dist")),
    );
    crate::log_mode::set_detail(choice.detail);
    let _ = writeln!(file, "{}", mode_line(choice.detail));
    let (sse2, avx2) = cpu_features();
    let _ = writeln!(file, "{}", probe::system_line(os_version(), sse2, avx2));
    let _ = match &veh {
        Ok(()) => writeln!(detail_out(&mut file), "例外: ハンドラ登録済み"),
        Err(why) => writeln!(file, "例外: ハンドラ登録不能（{why}）"),
    };
    if let Some(v) = wine_version() {
        let _ = writeln!(file, "環境: Wine {v}");
    }
    log_prng_fallback_once(&mut file);
    // クラッシュしたときの`d3d8.dll+RVA`がOSのものか差し替えかを判別できるように。
    let _ = writeln!(file, "{}", crate::d3d8_info_win::d3d8_line());
    if let Some(line) = &seed_line {
        let _ = writeln!(file, "{line}");
    }
    if let Some(note) = &choice.note {
        let _ = writeln!(file, "{note}");
    }
    // 配布バージョンだけ`logs\`の古いログを消す（このスレッドはゲームのスレッドではない。フックを設置する前の数ms）。
    if cfg!(feature = "dist")
        && let Some(dir) = run_path.parent()
    {
        let (removed, failed) = prune_old_logs(dir, &local_time(), KEEP_LOG_DAYS);
        if failed > 0 {
            let _ = writeln!(
                file,
                "過去のログ: 名前の日付が {KEEP_LOG_DAYS} 日より前のものを {removed} 個消しました（ {failed} 個が消せませんでした）。"
            );
        } else {
            let _ = writeln!(
                detail_out(&mut file),
                "過去のログ: 名前の日付が {KEEP_LOG_DAYS} 日より前のものを {removed} 個消しました（消せなかったもの {failed} 個）"
            );
        }
    }
    let mut rec = RealRecord::new(Some(run_path), exe_dir);
    let mut env = RealEnv { exe };
    let mut hook_env = RealHook::new();
    let r = catch_unwind(AssertUnwindSafe(|| {
        session::run(
            &mut env,
            &mut hook_env,
            &mut rec,
            &mut script,
            SITES,
            FORBIDDEN,
            IMAGE_BASE,
            Sites {
                hook_site: HOOK_SITE,
                draw_site: "draw_before_endscene",
                exit_site: "render_only_exit",
            },
            WINDOW_ADDR,
            &COPY_TABLE,
            // 描画だけのフレームの実験は180秒。
            crate::render_only::TOTAL_SECONDS,
            &mut file,
        )
    }));
    // パッドの終了時のカウントの1行の書き込みを依頼する。次の本ログへの書き込みの開始時に本ログへ入る。
    crate::pad_extra_win::push_final_counts();
    log_prng_fallback_once(&mut file);
    let _ = match r {
        Ok(End::NotInstalled) => {
            writeln!(file, "終了: スレッドを終えます（フックは設置していない）。")
        }
        Ok(End::Installed) => writeln!(
            detail_out(&mut file),
            "終了: スレッドを終了します（フックは残ります。外す手順はありません）"
        ),
        // パニックがどこで起きたかによって書いたかどうかは変わる。止めたスレッドは`hook::Stopped`のDropが戻している。
        Err(p) => writeln!(
            file,
            "終了: パニックを受けて止めました（{}）。スレッドを終えます（書いたかどうかは上の行を見てください）。",
            panic_text(p.as_ref())
        ),
    };
    if let Some(line) = crate::crash_win::summary_line() {
        let _ = writeln!(file, "{line}");
    }
    // 「終了」の行の後に書き込みを依頼された分も本ログの最後へ書き出す。
    file.drain_now();
}

/// `ProcessPrng`の代わりが使われていれば1回だけ書く。
/// 乱数は生成せず数を読むだけなので、ログの先頭の時点で標準ライブラリのシードがまだ生成されていなければ書かれない。
/// 終わりの前にもう一度呼ぶ。
fn log_prng_fallback_once(file: &mut impl Write) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if higanr_prng_shim::fallback_in_use()
        && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        let _ = writeln!(file, "{}", higanr_prng_shim::fallback_log_line());
    }
}

/// ログに書く行から伏せるフォルダ（ゲームとDLL）。名前が出ない形に替えて書く。
fn log_scrub(exe: Option<&Path>, dll: Option<&Path>) -> Scrub {
    let mut s = Scrub::new();
    if let Some(d) = dll.and_then(Path::parent) {
        s = s.dir(&d.to_string_lossy(), "<ツール>");
    }
    if let Some(d) = exe.and_then(Path::parent) {
        s = s.dir(&d.to_string_lossy(), "<ゲーム>");
    }
    s
}

fn mode_line(detail: bool) -> String {
    use higanr_core::log_cfg::{BRIEF_MARK, DETAIL_MARK};
    if detail {
        format!("{DETAIL_MARK}（ini の [log] detail = 1 か開発用の既定。CSV も書く）")
    } else {
        format!("{BRIEF_MARK}（ini で [log] detail = 1にすると詳細）")
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "理由の文が無い".to_string()
    }
}

/// 台本・検査の実行かどうか（`names::is_scripted_run`）。本ログを回ごとの名前にする。
fn scripted_run(dll: &Path) -> bool {
    is_scripted_run(
        std::env::var_os(higanr_core::ini::PATH_ENV).as_deref(),
        std::env::var_os(higanr_core::script::PATH_ENV).as_deref(),
        dll.parent()
            .is_some_and(|d| d.join(crate::script_win::SCRIPT_FILE).exists()),
    )
}

/// DLLと同じフォルダの`logs\`に本ログを開く。`logs\`がexeのフォルダと同じかその下なら書かずに断る。
/// 開いたファイルと隣のファイルの名前の元になるパス（回ごとの名前。台本の実行では本ログそのもの）を返す。
fn open_log(dll: Option<&Path>, exe: Option<&Path>) -> Result<MainLog, String> {
    let dll = dll.ok_or("DLL のパスが取れない")?;
    let dir = dll
        .parent()
        .ok_or("DLL のフォルダが分からない")?
        .join("logs");
    let exe_dir = exe
        .and_then(Path::parent)
        .ok_or("exe のフォルダが不明なのでゲームのフォルダの外かチェックできません")?;
    if is_same_or_inside(&dir.to_string_lossy(), &exe_dir.to_string_lossy()) {
        return Err(format!(
            "ログの置き場 {} が exe のフォルダの中なので書かない",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作れない: {e}", dir.display()))?;
    open_main_log(&dir, &local_time(), std::process::id(), scripted_run(dll))
}

/// 開いた本ログ。`run_path`は隣のファイルの名前の元（回ごとの名前。台本の実行では本ログそのもの）。
struct MainLog {
    file: File,
    run_path: PathBuf,
    /// 日ごとの本ログを使えず起動ごとのファイルにしたときの、本ログの先頭に足す1行。
    fallback_note: Option<&'static str>,
}

const FALLBACK_NOTE: &str = "同日のログが他のプログラムに使われていて開けませんでした。\n起動ごとに別のファイルへ書きます。";

/// 日ごとの本ログを開くときの共有の指定（`FILE_SHARE_READ | FILE_SHARE_DELETE`）。
/// 読み取りと削除だけ許す。書き込みは1つの起動に限る。
/// 同じ日の2つの起動が同じファイルへ書くと、区切りの行の範囲が崩れるため。
const DAILY_SHARE: u32 = 1 | 4;

/// `ERROR_SHARING_VIOLATION`。
const SHARING_VIOLATION: i32 = 32;

/// 回ごと（`per_run`）なら回ごとの名前を`create_new`で開く。そうでなければ日ごとの名前を追記で開く。
/// 日ごとのファイルを別の起動が書き込みで開いていれば（共有違反）回ごとの名前へ切り替える。
fn open_main_log(dir: &Path, t: &LocalTime, pid: u32, per_run: bool) -> Result<MainLog, String> {
    use std::os::windows::fs::OpenOptionsExt;
    if !per_run {
        let path = dir.join(daily_log_file_name(t));
        match OpenOptions::new()
            .append(true)
            .create(true)
            .share_mode(DAILY_SHARE)
            .open(&path)
        {
            Ok(file) => {
                return Ok(MainLog {
                    file,
                    run_path: dir.join(log_file_name(t, pid, 0)),
                    fallback_note: None,
                });
            }
            Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) => {
                let (file, run_path) = open_per_run(dir, t, pid)?;
                return Ok(MainLog {
                    file,
                    run_path,
                    fallback_note: Some(FALLBACK_NOTE),
                });
            }
            Err(e) => return Err(format!("{} を開けない: {e}", path.display())),
        }
    }
    let (file, run_path) = open_per_run(dir, t, pid)?;
    Ok(MainLog {
        file,
        run_path,
        fallback_note: None,
    })
}

fn open_per_run(dir: &Path, t: &LocalTime, pid: u32) -> Result<(File, PathBuf), String> {
    for attempt in 0..LOG_NAME_ATTEMPTS {
        let path = dir.join(log_file_name(t, pid, attempt));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{} を開けない: {e}", path.display())),
        }
    }
    Err("ログの名前がすべて使われていた".to_string())
}

/// `dir`の直下にあるログのうち名前の日付が古いものを消す（判定は`names::is_expired_log`。フォルダとリンクには触らない）。返り値は（消した数, 消せなかった数）。
fn prune_old_logs(dir: &Path, today: &LocalTime, keep_days: i64) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let (mut removed, mut failed) = (0, 0);
    for e in entries.flatten() {
        let is_file = e.file_type().is_ok_and(|t| t.is_file());
        let expired = e
            .file_name()
            .to_str()
            .is_some_and(|n| is_expired_log(n, today, keep_days));
        if !(is_file && expired) {
            continue;
        }
        match std::fs::remove_file(e.path()) {
            Ok(()) => removed += 1,
            Err(_) => failed += 1,
        }
    }
    (removed, failed)
}

struct RealEnv {
    exe: Option<PathBuf>,
}

impl Env for RealEnv {
    fn exe_path(&self) -> Option<String> {
        self.exe.as_ref().map(|p| p.display().to_string())
    }
    fn ident(&self) -> IdentOutcome {
        match &self.exe {
            None => IdentOutcome::Unreadable("exe のパス取得不能".to_string()),
            Some(p) => probe::ident_of_file(std::fs::read(p), &TH09_V150A),
        }
    }
    fn image_base(&self) -> usize {
        exe_image_base()
    }
    fn read(&mut self, addr: u32, len: usize) -> Option<Vec<u8>> {
        read_own_memory(addr, len)
    }
}

#[cfg(test)]
mod tests {
    //! 本物のWin32を、検査のプロセス自身に当てる（花映塚は使わない）。
    use super::*;

    static SAMPLE: [u8; 8] = [0x10, 0x32, 0x54, 0x76, 0x98, 0xBA, 0xDC, 0xFE];

    // システムのコードページに依らず、番号を指定して本物の変換を実行する（936と932の表は日本語のWindowsにも入っている）。
    #[test]
    fn code_page_936_reads_and_writes_chinese_and_932_does_not() {
        let gb = CodePage(936);
        assert_eq!(gb.decode(&[0xB6, 0xAB]).as_deref(), Some("东"));
        assert_eq!(
            gb.encode("D:\\东方").unwrap(),
            b"D:\\\xB6\xAB\xB7\xBD".to_vec()
        );
        let sj = CodePage(932);
        assert_eq!(sj.decode(&[0xB6, 0xAB]).as_deref(), Some("ｶｫ"));
        assert_eq!(sj.encode("东"), None);
        assert_eq!(sj.encode("ア"), Some(vec![0x83, 0x41]));
        assert_eq!(gb.encode("ｱ"), None);
    }

    #[test]
    fn code_page_conversion_refuses_what_does_not_come_back() {
        for cp in [CodePage(932), CodePage(936)] {
            assert_eq!(cp.encode("a😀"), None);
            assert_eq!(cp.encode("¥"), None);
            assert_eq!(cp.encode(""), Some(vec![]));
            assert_eq!(cp.decode(&[0x81]), None);
            assert_eq!(cp.decode(b""), Some(String::new()));
            assert_eq!(cp.encode("abc"), Some(b"abc".to_vec()));
        }
    }

    #[test]
    fn utf8_code_page_round_trips_without_flags() {
        let u = CodePage(CP_UTF8);
        assert_eq!(u.encode("东"), Some("东".as_bytes().to_vec()));
        assert_eq!(u.decode("东".as_bytes()).as_deref(), Some("东"));
        assert_eq!(u.decode(&[0xFF]), None);
    }

    #[test]
    fn the_process_code_page_matches_its_number() {
        // SAFETY: 引数の無い呼び出し。
        let acp = unsafe { GetACP() };
        let s = "abc\\def";
        assert_eq!(AcpAnsi.encode(s), CodePage(acp).encode(s));
        let all = (0x20u8..0x7F).collect::<Vec<_>>();
        assert_eq!(AcpAnsi.decode(&all), CodePage(acp).decode(&all));
    }

    // 変数の綴りが違っても、変数を設定する呼び出しが消えてもリンクは通る。その場合`delayimp.lib`の既定の何もしないhookが選ばれてしまう。
    #[test]
    fn delay_load_hook_variables_are_ours() {
        higanr_prng_shim::assert_delay_load_hooks_installed!();
    }

    // 文面を読む検査なのでリンクされたDLLは見ていない（配布のビルドの`check_pe`が通常のimportを見る）。
    #[test]
    fn build_rs_passes_the_delay_load_flags() {
        let build_rs = include_str!("../build.rs");
        for flag in [
            "cargo:rustc-link-arg-cdylib=/DELAYLOAD:{}",
            "higanr_prng_shim::PRNG_DLL",
            "cargo:rustc-link-arg-cdylib=delayimp.lib",
        ] {
            assert!(build_rs.contains(flag), "{flag}");
        }
    }

    /// Windowsでは`wine_get_version`が無く`None`。Wineで走らせたときはバージョンの文字列（空でないASCII）になる。
    #[test]
    fn wine_version_is_none_or_a_plain_string() {
        if let Some(v) = wine_version() {
            assert!(
                !v.is_empty() && v.chars().all(|c| c.is_ascii_graphic()),
                "{v}"
            );
        }
    }

    #[test]
    fn os_version_and_cpu_features_are_read() {
        let v = os_version().expect("RtlGetVersion が値を返す");
        assert!(v.major >= 6 && v.build > 0, "{v:?}");
        let (sse2, _avx2) = cpu_features();
        assert!(sse2, "i686 のビルドが走っているなら SSE2 はある");
    }

    #[test]
    fn read_own_memory_reads_a_static_array() {
        let addr = SAMPLE.as_ptr() as usize as u32;
        assert_eq!(read_own_memory(addr, 8), Some(SAMPLE.to_vec()));
        assert_eq!(read_own_memory(addr + 2, 3), Some(vec![0x54, 0x76, 0x98]));
    }

    #[test]
    fn read_own_memory_refuses_the_null_page() {
        assert_eq!(read_own_memory(0, 4), None);
        assert_eq!(read_own_memory(0x10, 1), None);
    }

    #[test]
    fn read_own_memory_refuses_wraparound() {
        if usize::BITS == 32 {
            assert_eq!(read_own_memory(0xFFFF_FFF0, 0x20), None);
        }
    }

    #[test]
    fn readable_protect_flags() {
        assert!(is_readable_protect(PAGE_EXECUTE_READ));
        assert!(is_readable_protect(PAGE_READONLY | 0x200));
        assert!(is_readable_protect(PAGE_EXECUTE_WRITECOPY));
        assert!(!is_readable_protect(PAGE_EXECUTE_READ | PAGE_GUARD));
        assert!(!is_readable_protect(PAGE_NOACCESS));
        assert!(!is_readable_protect(0x10)); // PAGE_EXECUTE（実行だけ）
        assert!(!is_readable_protect(0));
    }

    #[test]
    fn named_mutex_refuses_the_second_holder() {
        let name = format!("Local\\HiganRTool.selftest.{}.mutex", std::process::id());
        let first = acquire_named_mutex(&name).expect("1 本目は取れる");
        assert_eq!(acquire_named_mutex(&name), Err(MutexRefusal::AlreadyExists));
        close_handle(first);
        let again = acquire_named_mutex(&name).expect("閉じた後は取れる");
        close_handle(again);
    }

    #[test]
    fn module_path_of_the_test_exe() {
        let p = module_path(core::ptr::null_mut()).expect("exe のパス");
        assert!(p.is_absolute());
        assert_eq!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(str::to_lowercase),
            Some("exe".to_string())
        );
        assert!(p.exists());
    }

    #[test]
    fn exe_image_base_is_nonzero_and_readable() {
        let base = exe_image_base();
        assert_ne!(base, 0);
        assert_eq!(read_own_memory(base as u32, 2), Some(b"MZ".to_vec()));
    }

    fn temp_logs(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("higanr_win_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn time(day: u16, second: u16) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 10,
            day,
            hour: 12,
            minute: 0,
            second,
            millis: 0,
        }
    }

    fn names_in(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn the_daily_main_log_is_appended_across_launches_and_side_names_stay_per_run() {
        let d = temp_logs("daily");
        let mut a = open_main_log(&d, &time(13, 1), 100, false).unwrap();
        a.file.write_all(b"first launch\n").unwrap();
        let a_run = a.run_path.clone();
        drop(a);
        let mut b = open_main_log(&d, &time(13, 2), 200, false).unwrap();
        assert_eq!(b.fallback_note, None);
        b.file.write_all(b"second launch\n").unwrap();
        let b_run = b.run_path.clone();
        drop(b);
        let mut next_day = open_main_log(&d, &time(14, 3), 300, false).unwrap();
        next_day.file.write_all(b"next day\n").unwrap();
        drop(next_day);
        assert_eq!(names_in(&d), ["dll_20261013.log", "dll_20261014.log"]);
        assert_eq!(
            std::fs::read_to_string(d.join("dll_20261013.log")).unwrap(),
            "first launch\nsecond launch\n"
        );
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name(&a_run), "dll_20261013-120001-000_pid100.log");
        assert_eq!(name(&b_run), "dll_20261013-120002-000_pid200.log");
        assert!(
            !a_run.exists(),
            "回ごとの名前は隣のファイルの元で、本ログではない"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_script_file_next_to_the_dll_makes_the_run_scripted() {
        let d = temp_logs("scripted");
        let dll = d.join("higanrtool.dll");
        let env_set = std::env::var_os(higanr_core::ini::PATH_ENV).is_some()
            || std::env::var_os(higanr_core::script::PATH_ENV).is_some();
        if !env_set {
            assert!(!scripted_run(&dll));
        }
        std::fs::write(d.join(crate::script_win::SCRIPT_FILE), "x").unwrap();
        assert!(scripted_run(&dll));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_per_run_main_log_is_a_new_file_each_launch() {
        let d = temp_logs("per_run");
        let mut a = open_main_log(&d, &time(13, 1), 100, true).unwrap();
        a.file.write_all(b"one\n").unwrap();
        let b = open_main_log(&d, &time(13, 1), 100, true).unwrap();
        assert_ne!(a.run_path, b.run_path, "同じ秒・同じ PID でも別名にする");
        assert_eq!(std::fs::read_to_string(&a.run_path).unwrap(), "one\n");
        assert_eq!((a.fallback_note, b.fallback_note), (None, None));
        assert_eq!(
            names_in(&d),
            [
                "dll_20261013-120001-000_pid100.log",
                "dll_20261013-120001-000_pid100_2.log"
            ]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 共有違反のときの理由の1行を検証する。書き込み中の日ごとの本ログを既定の共有で読み取れないことも検証する。
    #[test]
    fn a_second_launch_on_the_same_day_falls_back_to_its_own_file() {
        let d = temp_logs("share");
        let mut first = open_main_log(&d, &time(13, 1), 100, false).unwrap();
        first.file.write_all(b"first\n").unwrap();
        let mut second = open_main_log(&d, &time(13, 2), 200, false).unwrap();
        assert_eq!(second.fallback_note, Some(FALLBACK_NOTE));
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name(&second.run_path), "dll_20261013-120002-000_pid200.log");
        second.file.write_all(b"second\n").unwrap();
        first.file.write_all(b"first again\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("dll_20261013.log")).unwrap(),
            "first\nfirst again\n"
        );
        assert_eq!(
            std::fs::read_to_string(&second.run_path).unwrap(),
            "second\n"
        );
        drop(first);
        let third = open_main_log(&d, &time(13, 3), 300, false).unwrap();
        assert_eq!(third.fallback_note, None, "持ち主が閉じた後は日ごとに戻る");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn prune_old_logs_removes_only_old_log_files() {
        let d = std::env::temp_dir().join(format!("higanr_prune_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let old = "dll_20260901-120000-000_pid1";
        let edge_gone = "dll_20261004-120000-000_pid4";
        let edge_kept = "dll_20261005-120000-000_pid5";
        let new = "dll_20261008-120000-000_pid2";
        for n in [
            format!("{old}.log"),
            format!("{old}_net_frames.csv"),
            format!("{old}_net_frames.csv.gz"),
            format!("{edge_gone}.log"),
            format!("{edge_gone}_pacing.csv.gz"),
            format!("{edge_kept}.log"),
            format!("{edge_kept}_pacing.csv.gz"),
            format!("{new}.log"),
            format!("{new}_pacing.csv.gz"),
            "dll_20260901.log".to_string(),
            "dll_20261004.log".to_string(),
            "dll_20261005.log".to_string(),
            "dll_20261008.log".to_string(),
            "note.txt".to_string(),
        ] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let sub = d.join(format!("{old}_dir"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("{old}.log")), b"x").unwrap();
        let today = LocalTime {
            year: 2026,
            month: 10,
            day: 8,
            hour: 12,
            minute: 0,
            second: 0,
            millis: 0,
        };
        assert_eq!(prune_old_logs(&d, &today, KEEP_LOG_DAYS), (7, 0));
        let mut left: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want = vec![
            format!("{old}_dir"),
            format!("{edge_kept}.log"),
            format!("{edge_kept}_pacing.csv.gz"),
            format!("{new}.log"),
            format!("{new}_pacing.csv.gz"),
            "dll_20261005.log".to_string(),
            "dll_20261008.log".to_string(),
            "note.txt".to_string(),
        ];
        want.sort();
        assert_eq!(left, want);
        assert!(
            sub.join(format!("{old}.log")).exists(),
            "下のフォルダへ降りた"
        );
        assert_eq!(
            prune_old_logs(&d.join("無い"), &today, KEEP_LOG_DAYS),
            (0, 0)
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
