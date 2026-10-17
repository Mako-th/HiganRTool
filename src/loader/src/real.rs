//! 本物の Win32（`windows-sys`）。ローダーの unsafe は `folder_dialog.rs`・`http_win.rs` とここにだけ置く。
//! 判断はしない（手順は `launch.rs`。ここに置くと偽物の検査が本物の経路を見なくなる）。

use core::ffi::c_void;
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows_sys::Wdk::Storage::FileSystem::NtQueryVirtualMemory;
use windows_sys::Win32::Foundation::{
    CloseHandle, FreeLibrary, GetLastError, HANDLE, HMODULE, SYSTEMTIME, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, FlushInstructionCache, ReadProcessMemory,
    SetUnhandledExceptionFilter, WriteProcessMemory,
};
// 自己検査だけが使う。
#[cfg(not(feature = "dist"))]
use windows_sys::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_IMAGE, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
    VirtualProtectEx, VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
#[cfg(not(feature = "dist"))]
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, CreateRemoteThread,
    GetCurrentProcess, GetExitCodeThread, PROCESS_INFORMATION, ResumeThread, STARTF_USESHOWWINDOW,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

use crate::crash_log::{ExceptionFacts, LocalStamp, StackBuf, format_exception};
use crate::imports::{ImportName, ImportProbe, Presence};
use crate::win::{ErrorCode, Handle, ProcessInfo, Remote, Spawn, WaitResult, Win32};
use higanr_core::code_files::CF_UNICODETEXT;
use higanr_core::exception_names::ACCESS_VIOLATION;
use higanr_core::lang::Lang;

use crate::ui_lang::by_lang;

const ERROR_INVALID_PARAMETER: ErrorCode = 87;

fn last_error() -> ErrorCode {
    // SAFETY: 引数の無い読み取りだけの関数で、どのスレッドから呼んでもよい。
    unsafe { GetLastError() }
}

fn h(handle: Handle) -> HANDLE {
    handle.0 as HANDLE
}

/// 終端 0 つきの UTF-16。途中に 0 があれば None。
fn wide_nul(s: &OsStr) -> Option<Vec<u16>> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    if v.contains(&0) {
        return None;
    }
    v.push(0);
    Some(v)
}

/// 引数を C ランタイムが読み戻せる形に包む（両端を `"` で囲み、閉じの `"` 直前の `\` を倍にする）。`"` を含む引数は断る。
fn push_quoted(out: &mut Vec<u16>, arg: &OsStr) -> Option<()> {
    let w: Vec<u16> = arg.encode_wide().collect();
    if w.contains(&0) || w.contains(&(b'"' as u16)) {
        return None;
    }
    out.push(b'"' as u16);
    out.extend_from_slice(&w);
    let trailing = w.iter().rev().take_while(|&&c| c == b'\\' as u16).count();
    out.extend(std::iter::repeat_n(b'\\' as u16, trailing));
    out.push(b'"' as u16);
    Some(())
}

pub fn command_line(spawn: &Spawn) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    push_quoted(&mut out, spawn.exe.as_os_str())?;
    for a in &spawn.args {
        out.push(b' ' as u16);
        push_quoted(&mut out, a)?;
    }
    out.push(0);
    Some(out)
}

/// `no_activate` なら、子の最初の `ShowWindow` の `nCmdShow` を `SW_SHOWNOACTIVATE` にする。
/// 前面のアプリの子孫として起こしたプロセスの最初のウィンドウには Windows が前面を渡し、起こした花映塚が約 1.4 秒後に
/// 前面を取ってキー入力を奪った（実測）ため。
pub fn startup_info(no_activate: bool) -> STARTUPINFOW {
    let mut si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    if no_activate {
        si.dwFlags |= STARTF_USESHOWWINDOW;
        si.wShowWindow = SW_SHOWNOACTIVATE as u16;
    }
    si
}

pub struct RealWin32;

impl Win32 for RealWin32 {
    fn read_file(&mut self, path: &Path) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| e.to_string())
    }

    fn load_library_w_address(&mut self) -> Result<usize, ErrorCode> {
        let name = wide_nul(OsStr::new("kernel32.dll")).ok_or(ERROR_INVALID_PARAMETER)?;
        // SAFETY: `name` は終端 0 つきで生きている。kernel32.dll は常に載っているので参照を増やさない GetModuleHandleW で足りる。
        let module = unsafe { GetModuleHandleW(name.as_ptr()) };
        if module.is_null() {
            return Err(last_error());
        }
        // SAFETY: `module` は有効な HMODULE、名前は終端 0 つきの ASCII。
        let proc = unsafe { GetProcAddress(module, c"LoadLibraryW".as_ptr().cast()) };
        match proc {
            Some(f) => Ok(f as usize),
            None => Err(last_error()),
        }
    }

    fn create_process_suspended(&mut self, spawn: &Spawn) -> Result<ProcessInfo, ErrorCode> {
        let app = wide_nul(spawn.exe.as_os_str()).ok_or(ERROR_INVALID_PARAMETER)?;
        let mut cmd = command_line(spawn).ok_or(ERROR_INVALID_PARAMETER)?;
        let cwd = wide_nul(spawn.cwd.as_os_str()).ok_or(ERROR_INVALID_PARAMETER)?;
        // 足す変数が無ければ親の環境を継がせる（NULL）。有れば親の環境に重ねたブロックを渡す。
        let env_block = if spawn.env.is_empty() {
            None
        } else {
            Some(
                crate::env_block::build(std::env::vars_os(), &spawn.env)
                    .ok_or(ERROR_INVALID_PARAMETER)?,
            )
        };
        let flags = CREATE_SUSPENDED
            | if env_block.is_some() {
                CREATE_UNICODE_ENVIRONMENT
            } else {
                0
            };
        let env_ptr: *const c_void = env_block
            .as_ref()
            .map_or(ptr::null(), |b| b.as_ptr().cast());
        let si = startup_info(spawn.no_activate);
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: 文字列は終端 0 つきで生きている。lpCommandLine は書き換えられうるので自分の `Vec` を渡す。
        // ハンドルは継承させない。環境は null か、生きている `env_block`（CREATE_UNICODE_ENVIRONMENT を立てた）。
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmd.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                flags,
                env_ptr,
                cwd.as_ptr(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(ProcessInfo {
            process: Handle(pi.hProcess as usize),
            thread: Handle(pi.hThread as usize),
            pid: pi.dwProcessId,
        })
    }

    fn virtual_alloc(&mut self, process: Handle, size: usize) -> Result<Remote, ErrorCode> {
        // SAFETY: 相手のプロセスに領域を作るだけ。`process` は閉じていない全権のハンドル。
        let p = unsafe {
            VirtualAllocEx(
                h(process),
                ptr::null(),
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            return Err(last_error());
        }
        Ok(Remote(p as usize))
    }

    fn write_memory(&mut self, process: Handle, at: Remote, data: &[u8]) -> Result<(), ErrorCode> {
        let mut written = 0usize;
        // SAFETY: 読むのは `data`、書くのは相手の `at`（`data.len()` 以上を確保した領域）。
        let ok = unsafe {
            WriteProcessMemory(
                h(process),
                at.0 as *const c_void,
                data.as_ptr().cast(),
                data.len(),
                &mut written,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        if written != data.len() {
            // 一部しか書けなかった。途中で切れたパスの DLL を読ませないよう失敗にする。
            return Err(ERROR_INVALID_PARAMETER);
        }
        Ok(())
    }

    fn read_memory(
        &mut self,
        process: Handle,
        at: usize,
        len: usize,
    ) -> Result<Vec<u8>, ErrorCode> {
        let mut buf = vec![0u8; len];
        let mut read = 0usize;
        // SAFETY: 書くのは `buf`（`len` バイト）、読むのは相手の `at`（読めなければ失敗を返すだけ）。
        let ok = unsafe {
            ReadProcessMemory(
                h(process),
                at as *const c_void,
                buf.as_mut_ptr().cast(),
                len,
                &mut read,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        if read != len {
            return Err(ERROR_INVALID_PARAMETER);
        }
        Ok(buf)
    }

    fn patch_code(&mut self, process: Handle, at: usize, data: &[u8]) -> Result<(), ErrorCode> {
        let mut old: PAGE_PROTECTION_FLAGS = 0;
        // SAFETY: 相手の `at` から `data.len()` バイトの保護を変えるだけ。
        let ok = unsafe {
            VirtualProtectEx(
                h(process),
                at as *const c_void,
                data.len(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        let mut written = 0usize;
        // SAFETY: 読むのは `data`、書くのは相手の `at`（上で書けるようにした）。
        let wrote = unsafe {
            WriteProcessMemory(
                h(process),
                at as *const c_void,
                data.as_ptr().cast(),
                data.len(),
                &mut written,
            )
        };
        let write_err = if wrote == 0 {
            Some(last_error())
        } else if written != data.len() {
            Some(ERROR_INVALID_PARAMETER)
        } else {
            None
        };
        let mut back: PAGE_PROTECTION_FLAGS = 0;
        // SAFETY: 上と同じ範囲の保護を戻すだけ。
        let restored = unsafe {
            VirtualProtectEx(h(process), at as *const c_void, data.len(), old, &mut back)
        };
        if let Some(e) = write_err {
            return Err(e);
        }
        if restored == 0 {
            return Err(last_error());
        }
        // SAFETY: 相手のプロセスの命令キャッシュを流すだけ（書いた範囲）。
        let flushed = unsafe { FlushInstructionCache(h(process), at as *const c_void, data.len()) };
        if flushed == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn create_remote_thread(
        &mut self,
        process: Handle,
        start: usize,
        param: Remote,
    ) -> Result<Handle, ErrorCode> {
        if start == 0 {
            return Err(ERROR_INVALID_PARAMETER);
        }
        // SAFETY: `start` は 0 でないアドレス（kernel32 の LoadLibraryW）で、呼ぶのは相手のプロセスの中だけ。
        // 32bit では usize と関数ポインタは同じ大きさ。
        let routine: unsafe extern "system" fn(*mut c_void) -> u32 =
            unsafe { core::mem::transmute::<usize, _>(start) };
        // SAFETY: `process` は閉じていない全権のハンドル。引数は相手の中のアドレスで、こちらでは参照しない。
        let t = unsafe {
            CreateRemoteThread(
                h(process),
                ptr::null(),
                0,
                Some(routine),
                param.0 as *const c_void,
                0,
                ptr::null_mut(),
            )
        };
        if t.is_null() {
            return Err(last_error());
        }
        Ok(Handle(t as usize))
    }

    fn wait(&mut self, handle: Handle, timeout_ms: u32) -> WaitResult {
        // SAFETY: `handle` は閉じていないハンドル（スレッドかプロセス）。待つだけで何も書かない。
        let r = unsafe { WaitForSingleObject(h(handle), timeout_ms) };
        match r {
            WAIT_OBJECT_0 => WaitResult::Signaled,
            WAIT_TIMEOUT => WaitResult::TimedOut,
            WAIT_FAILED => WaitResult::Failed(last_error()),
            // WAIT_ABANDONED はミューテックスにしか返らない。
            other => WaitResult::Failed(other),
        }
    }

    fn thread_exit_code(&mut self, thread: Handle) -> Result<u32, ErrorCode> {
        let mut code = 0u32;
        // SAFETY: `thread` は閉じていないスレッドのハンドル。`code` は呼び出しの間生きている。
        let ok = unsafe { GetExitCodeThread(h(thread), &mut code) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(code)
    }

    fn virtual_free(&mut self, process: Handle, at: Remote) -> Result<(), ErrorCode> {
        // SAFETY: `at` は virtual_alloc が返した領域の先頭（MEM_RELEASE では大きさに 0）。相手のメモリだけに触る。
        let ok = unsafe { VirtualFreeEx(h(process), at.0 as *mut c_void, 0, MEM_RELEASE) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn resume_thread(&mut self, thread: Handle) -> Result<u32, ErrorCode> {
        // SAFETY: `thread` は CreateProcessW が返した主スレッドのハンドル（閉じていない）。
        let r = unsafe { ResumeThread(h(thread)) };
        if r == u32::MAX {
            return Err(last_error());
        }
        Ok(r)
    }

    fn terminate_process(&mut self, process: Handle, exit_code: u32) -> Result<(), ErrorCode> {
        // SAFETY: `process` はこのローダーが作ったプロセスのハンドル（他人のプロセスは殺さない）。
        let ok = unsafe { TerminateProcess(h(process), exit_code) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn close_handle(&mut self, handle: Handle) -> Result<(), ErrorCode> {
        // SAFETY: `handle` はこのローダーが開いたハンドルで、閉じるのはこの 1 回だけ。
        let ok = unsafe { CloseHandle(h(handle)) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

#[cfg(not(feature = "dist"))]
impl RealWin32 {
    /// 自己検査が的の終了コードを読むためだけに使う（`Win32` トレイトには入れない）。
    pub fn process_exit_code(&mut self, process: Handle) -> Result<u32, ErrorCode> {
        let mut code = 0u32;
        // SAFETY: `process` は閉じていないプロセスのハンドル。`code` は呼び出しの間生きている。
        let ok = unsafe { GetExitCodeProcess(h(process), &mut code) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(code)
    }
}

/// 自己検査だけ: 読めない DLL を読ませたとき OS のエラーのウィンドウで止まらないようにする。子プロセスにも引き継がれる。本番では呼ばない。
#[cfg(not(feature = "dist"))]
pub fn quiet_error_mode() {
    // SAFETY: 自分のプロセスの設定を 1 つ変えるだけ。戻り値（前の設定）は使わない。
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX);
    }
}

/// 黒いウィンドウに付いているプロセスの数。ショートカット起動なら 1、コマンドプロンプトや検査の道具からなら 2 以上。取れなければ None。
pub fn console_process_count() -> Option<u32> {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut ids = [0u32; 4];
    // SAFETY: `ids` は長さ 4 の書ける配列で、その長さを渡す（戻り値は配列より多くても数は正しい）。
    let n = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
    (n != 0).then_some(n)
}

/// 多重起動の確認（`single`）の本物。プロセスの一覧と他のプロセスのパスを読むだけで、何も書かない。
pub struct RealSingle;

impl crate::single::Host for RealSingle {
    fn lock(&mut self, name: &str) -> crate::single::Lock {
        use crate::single::Lock;
        use windows_sys::Win32::Foundation::WAIT_ABANDONED;
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let Some(wide) = wide_nul(OsStr::new(name)) else {
            return Lock::Failed(ERROR_INVALID_PARAMETER);
        };
        // SAFETY: `wide` は終端 0 つきで生きている。属性は既定（NULL）、最初は誰も持たない。
        let m = unsafe { CreateMutexW(ptr::null(), 0, wide.as_ptr()) };
        if m.is_null() {
            return Lock::Failed(last_error());
        }
        // SAFETY: `m` は上で作った有効なミューテックスのハンドル。待たずに取るだけ。
        match unsafe { WaitForSingleObject(m, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Lock::Held(m as usize),
            other => {
                let e = if other == WAIT_FAILED {
                    last_error()
                } else {
                    0
                };
                // SAFETY: 取れなかったので `m` は閉じて捨てる（1 回だけ）。
                unsafe { CloseHandle(m) };
                if other == WAIT_TIMEOUT {
                    Lock::Busy
                } else {
                    Lock::Failed(e)
                }
            }
        }
    }

    fn unlock(&mut self, token: usize) {
        use windows_sys::Win32::System::Threading::ReleaseMutex;
        let m = token as HANDLE;
        // SAFETY: `token` は `lock` が返した、同じスレッドで取ったミューテックス。放して閉じるのは 1 回だけ。
        unsafe {
            ReleaseMutex(m);
            CloseHandle(m);
        }
    }

    fn processes(&mut self) -> Result<Vec<(u32, String)>, ErrorCode> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };
        // SAFETY: 引数は定数だけ。失敗は INVALID_HANDLE_VALUE。
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        // SAFETY: 0 で埋めた構造体で足りる（dwSize は下で設定する）。
        let mut entry: PROCESSENTRY32W = unsafe { core::mem::zeroed() };
        entry.dwSize = core::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut out = Vec::new();
        // SAFETY: `snap` は有効なスナップショット、`entry` は dwSize を設定した書ける構造体。
        let mut more = unsafe { Process32FirstW(snap, &mut entry) } != 0;
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push((
                entry.th32ProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            // SAFETY: 同上。
            more = unsafe { Process32NextW(snap, &mut entry) } != 0;
        }
        // SAFETY: 上で作った `snap` を閉じる（1 回だけ）。
        unsafe { CloseHandle(snap) };
        Ok(out)
    }

    fn image_path(&mut self, pid: u32) -> Option<String> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        };
        // SAFETY: 制限つきの情報だけを求めて開く。開けなければ NULL。
        let p = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if p.is_null() {
            return None;
        }
        // パスの長さの上限（32767 文字）に終端の 0 を足した分。
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: `p` は有効なプロセスのハンドル。`buf` は `len` 文字を書ける。0 は Win32 形式のパス。
        let ok = unsafe { QueryFullProcessImageNameW(p, 0, buf.as_mut_ptr(), &mut len) };
        // SAFETY: 上で開いた `p` を閉じる（1 回だけ）。
        unsafe { CloseHandle(p) };
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// OS の UI の言語（`GetUserDefaultUILanguage`。LANGID）。画面の言語の既定に使う。
pub fn user_ui_langid() -> u16 {
    // SAFETY: 引数の無い読み取りだけの関数で、どのスレッドから呼んでもよい。
    unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() }
}

fn api_error(name: &str, code: ErrorCode, lang: Lang) -> String {
    by_lang!(lang, ja: "{name}、GetLastError {code}", en: "{name}, GetLastError {code}")
}

/// `OpenClipboard` を試す回数。他のアプリが一瞬開いていると断られるため。
const CLIPBOARD_TRIES: u32 = 5;
const CLIPBOARD_GAP_MS: u64 = 20;

/// `text` をクリップボードへコピーする。検査からは呼ばない（使っている人のクリップボードを書き換えるため。`code_relay::Host` の偽物で差し替える）。
///
/// 開き手のウィンドウは NULL。文書は「EmptyClipboard すると持ち主が NULL になり SetClipboardData が失敗する」と書くが、実物では確かめていない。
/// 失敗したら理由を返す。
pub fn set_clipboard_text(text: &str, lang: Lang) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };

    if text.contains('\0') {
        return Err(match lang {
            Lang::Ja => "文字の途中に0があります",
            Lang::En => "the text contains a NUL character",
        }
        .to_string());
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut opened = false;
    for i in 0..CLIPBOARD_TRIES {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(CLIPBOARD_GAP_MS));
        }
        // SAFETY: NULL のウィンドウ（呼び出しスレッドが開き手になる）。何も読み書きしない。
        if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
    }
    if !opened {
        return Err(api_error("OpenClipboard", last_error(), lang));
    }
    let result = (|| {
        // SAFETY: 上で開いたクリップボードを、このスレッドから空にするだけ。
        if unsafe { EmptyClipboard() } == 0 {
            return Err(api_error("EmptyClipboard", last_error(), lang));
        }
        let bytes = wide.len() * std::mem::size_of::<u16>();
        // SAFETY: 動かせるグローバルメモリを `bytes` だけ確保する。失敗なら NULL が返り、下で見る。
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if mem.is_null() {
            return Err(api_error("GlobalAlloc", last_error(), lang));
        }
        // SAFETY: `mem` は上で確保した閉じていないハンドル。錠を掛けてアドレスを得る。
        let p = unsafe { GlobalLock(mem) }.cast::<u16>();
        if p.is_null() {
            let e = last_error();
            // SAFETY: `mem` はまだクリップボードに渡していないので、こちらが返す。
            unsafe { GlobalFree(mem) };
            return Err(api_error("GlobalLock", e, lang));
        }
        // SAFETY: `p` は `wide.len()` 個の u16 を確保した領域の先頭で、`wide` と重ならない。書き終えたら錠を外す。
        unsafe {
            ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            GlobalUnlock(mem);
        }
        // SAFETY: 空にしたクリップボードへ、錠を外したメモリを渡す。成功すれば持ち主はシステムに移る。
        if unsafe { SetClipboardData(CF_UNICODETEXT, mem) }.is_null() {
            let e = last_error();
            // SAFETY: 渡せなかったので持ち主はこちらのまま。返す。
            unsafe { GlobalFree(mem) };
            return Err(api_error("SetClipboardData", e, lang));
        }
        Ok(())
    })();
    // SAFETY: 上で開いたクリップボードを閉じる（成功でも失敗でも 1 回だけ）。
    unsafe { CloseClipboard() };
    result
}

#[cfg(not(feature = "dist"))]
pub fn module_loaded(path: &Path) -> bool {
    let Some(name) = wide_nul(path.as_os_str()) else {
        return false;
    };
    // SAFETY: `name` は終端 0 つきで生きている。参照を増やさずに引くだけ。
    let m = unsafe { GetModuleHandleW(name.as_ptr()) };
    !m.is_null()
}

/// ローカルの時刻（段階ログの行頭とファイル名）。
pub fn local_stamp() -> LocalStamp {
    let mut st = SYSTEMTIME::default();
    // SAFETY: `st` は GetLocalTime が書く SYSTEMTIME そのもの。
    unsafe { GetLocalTime(&mut st) };
    LocalStamp {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
        millis: st.wMilliseconds,
    }
}

/// `RtlGetVersion` の OSVERSIONINFOW。`GetVersionExW` はマニフェストが無いと 6.2 以降を偽るので使わない。
#[repr(C)]
struct OsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    csd: [u16; 128],
}

/// `10.0.19045` の形の Windows のバージョン。ntdll の `RtlGetVersion` が引けなければ `不明`。
pub fn os_version() -> String {
    let Some(name) = wide_nul(OsStr::new("ntdll.dll")) else {
        return "不明".to_string();
    };
    // SAFETY: `name` は終端 0 つきで生きている。ntdll.dll は全プロセスに載っているので参照を増やさない関数で足りる。
    let ntdll = unsafe { GetModuleHandleW(name.as_ptr()) };
    if ntdll.is_null() {
        return "不明".to_string();
    }
    // SAFETY: `ntdll` は有効な HMODULE、名前は終端 0 つきの ASCII。
    let Some(f) = (unsafe { GetProcAddress(ntdll, c"RtlGetVersion".as_ptr().cast()) }) else {
        return "不明".to_string();
    };
    type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfo) -> i32;
    // SAFETY: RtlGetVersion は OSVERSIONINFOW を 1 つ受ける関数で、32bit では関数のアドレスと同じ大きさの型に直せる。
    let f: RtlGetVersion = unsafe { core::mem::transmute(f) };
    let mut info = OsVersionInfo {
        size: size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd: [0; 128],
    };
    // SAFETY: `info` は `size` を正しく書いた OSVERSIONINFOW と同じ並びで、呼び出しの間生きている。
    let status = unsafe { f(&mut info) };
    if status != 0 {
        return "不明".to_string();
    }
    format!("{}.{}.{}", info.major, info.minor, info.build)
}

/// Wine / Proton の上か（DLL の `win::wine_version` と同じ判定: `ntdll` が `wine_get_version` を出している）。名前を引くだけで呼ばない。
pub fn is_wine() -> bool {
    let Some(name) = wide_nul(OsStr::new("ntdll.dll")) else {
        return false;
    };
    // SAFETY: `name` は終端 0 つきで生きている。ntdll.dll は全プロセスに載っているので参照を増やさない関数で足りる。
    let ntdll = unsafe { GetModuleHandleW(name.as_ptr()) };
    if ntdll.is_null() {
        return false;
    }
    // SAFETY: `ntdll` は有効な HMODULE、名前は終端 0 つきの ASCII。無ければ None が返るだけ。
    unsafe { GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()) }.is_some()
}

const ERROR_MOD_NOT_FOUND: ErrorCode = 126;

/// import の引き当てを、このプロセスに実際に DLL を読んで確かめる本物。読んだ DLL は覚えておき、最後に手放す。
#[derive(Default)]
pub struct RealProbe {
    loaded: HashMap<String, Result<usize, ErrorCode>>,
}

impl RealProbe {
    fn load(dll: &str) -> Result<usize, ErrorCode> {
        let name = wide_nul(OsStr::new(dll)).ok_or(ERROR_INVALID_PARAMETER)?;
        // SAFETY: `name` は終端 0 つきで生きている。探す場所を System32 だけにして、同じ名前の別の DLL を拾わない。
        let m =
            unsafe { LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if m.is_null() {
            return Err(last_error());
        }
        Ok(m as usize)
    }
}

impl ImportProbe for RealProbe {
    fn probe(&mut self, dll: &str, name: &ImportName) -> Presence {
        let lib = *self
            .loaded
            .entry(dll.to_ascii_lowercase())
            .or_insert_with(|| Self::load(dll));
        let module = match lib {
            Ok(m) => m as HMODULE,
            Err(ERROR_MOD_NOT_FOUND) => return Presence::NoDll,
            Err(e) => return Presence::LoadFailed(e),
        };
        let proc = match name {
            ImportName::Name(n) => {
                let Ok(c) = CString::new(n.as_bytes()) else {
                    return Presence::NoFunction;
                };
                // SAFETY: `module` は `load` が返した読み込み済みの DLL、`c` は終端 0 つきで生きている。
                unsafe { GetProcAddress(module, c.as_ptr().cast()) }
            }
            // SAFETY: 序数での引き当て（下位 16 ビットだけを持つ値をポインタの形で渡す決まり）。
            ImportName::Ordinal(o) => unsafe {
                GetProcAddress(module, usize::from(*o) as *const u8)
            },
        };
        if proc.is_some() {
            Presence::Found
        } else {
            Presence::NoFunction
        }
    }
}

impl Drop for RealProbe {
    fn drop(&mut self) {
        for m in self.loaded.values().filter_map(|r| r.as_ref().ok()) {
            // SAFETY: `load` が 1 回だけ読んだ DLL を、DLL ごとに 1 回だけ手放す。
            unsafe { FreeLibrary(*m as HMODULE) };
        }
    }
}

// ---- 未処理の例外 ----

/// 書いた後は OS の普通の異常終了の扱い（報告の窓・イベントログ）に任せる。
pub const EXCEPTION_PASS_ON: i32 = EXCEPTION_CONTINUE_SEARCH;

static EXCEPTION_FILE: OnceLock<File> = OnceLock::new();
static EXCEPTION_VERDICT: AtomicI32 = AtomicI32::new(EXCEPTION_CONTINUE_SEARCH);
static FILTER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// 未処理の例外（アクセス違反など）の場所を 1 行書くフィルターを置く。書いた後は `verdict` を返す
/// （本番は `EXCEPTION_CONTINUE_SEARCH`: 普通の異常終了の扱いを変えない）。書き先は `attach_exception_file` で渡されるまで標準エラー
/// （ログのファイルを、ゲームのフォルダの外と確かめる前に作らないため）。置くのは最初の 1 回だけ。
pub fn install_exception_filter(verdict: i32) {
    EXCEPTION_VERDICT.store(verdict, Ordering::Relaxed);
    if FILTER_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: `exception_filter` は `LPTOP_LEVEL_EXCEPTION_FILTER` の形で、プロセスが終わるまで有効な関数。戻り値（前のフィルター）は使わない。
    unsafe { SetUnhandledExceptionFilter(Some(exception_filter)) };
}

/// 以降の未処理の例外の行を `file` へ書く。最初に渡した 1 つだけが有効。
pub fn attach_exception_file(file: File) {
    let _ = EXCEPTION_FILE.set(file);
}

unsafe extern "system" fn exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: OS が渡す例外の情報で、この呼び出しの間は有効。
    unsafe {
        match EXCEPTION_FILE.get() {
            Some(file) => write_exception(file, info),
            None => write_exception(std::io::stderr(), info),
        }
    }
    EXCEPTION_VERDICT.load(Ordering::Relaxed)
}

/// 例外のコード・アドレス・モジュール+RVA を 1 行書く。壊れたプロセスの中で走るので、確保せず、スタックの固定長の
/// 領域だけを使う。
///
/// # Safety
/// `info` は null か、`ExceptionRecord` が null か有効な `EXCEPTION_RECORD` を指す `EXCEPTION_POINTERS`。
unsafe fn write_exception(mut file: impl std::io::Write, info: *const EXCEPTION_POINTERS) {
    // SAFETY: 呼び出し側の約束（null か有効）。
    let Some(info) = (unsafe { info.as_ref() }) else {
        return;
    };
    // SAFETY: 同上。
    let Some(rec) = (unsafe { info.ExceptionRecord.as_ref() }) else {
        return;
    };
    let address = rec.ExceptionAddress as usize;
    let code = rec.ExceptionCode as u32;
    let mut name = [0u8; MODULE_NAME_MAX];
    let (len, rva) = module_of(address, &mut name);
    let access = (code == ACCESS_VIOLATION && rec.NumberParameters >= 2)
        .then(|| (rec.ExceptionInformation[0], rec.ExceptionInformation[1]));
    let mut line = StackBuf::<512>::new();
    let _ = format_exception(
        &mut line,
        &ExceptionFacts {
            code,
            address,
            module: &name[..len],
            rva,
            access,
        },
    );
    let _ = file.write_all(line.as_bytes());
}

const MODULE_NAME_MAX: usize = 260;

/// `NtQueryVirtualMemory` の `MemoryMappedFilenameInformation` の出力。先頭が UNICODE_STRING で、文字列はその直後に続く。
#[repr(C)]
struct SectionName {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
    path: [u16; MODULE_NAME_MAX + 40],
}

/// `MEMORY_INFORMATION_CLASS::MemoryMappedFilenameInformation`（winternl.h / ntifs.h の列挙の 3 番目）。
const MEMORY_MAPPED_FILENAME_INFORMATION: i32 = 2;

/// `addr` を含むイメージのファイル名（フォルダを除く。ASCII 以外は `?`）を `name` に入れて（長さ, アドレスの RVA）を返す。
/// イメージの外なら（0, 0）。名前だけ引けなければ `?` 1 文字。確保せず、`GetModuleFileNameW`（ローダーロックを取りうる）も使わない。
/// 先頭は `VirtualQuery` の `AllocationBase`、名前は `NtQueryVirtualMemory` が返す写像のパス（DLL の `crash_win` と同じ引き方）。
fn module_of(addr: usize, name: &mut [u8; MODULE_NAME_MAX]) -> (usize, u32) {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: アドレスの領域を調べるだけ（`addr` を読み書きしない）。`info` は書ける MEMORY_BASIC_INFORMATION で、その大きさを渡す。
    let got = unsafe {
        VirtualQuery(
            addr as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if got == 0 || info.Type != MEM_IMAGE || info.AllocationBase.is_null() {
        return (0, 0);
    }
    let rva = addr.wrapping_sub(info.AllocationBase as usize) as u32;
    let mut section = SectionName {
        length: 0,
        maximum_length: 0,
        buffer: ptr::null_mut(),
        path: [0; MODULE_NAME_MAX + 40],
    };
    // SAFETY: 自分のプロセス（疑似ハンドル）の `addr` の写像のパスを `section` に書かせるだけ。渡す大きさは `section` の大きさそのもの。
    let status = unsafe {
        NtQueryVirtualMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            MEMORY_MAPPED_FILENAME_INFORMATION,
            (&mut section as *mut SectionName).cast(),
            size_of::<SectionName>(),
            ptr::null_mut(),
        )
    };
    let chars = usize::from(section.length) / 2;
    let len = if status >= 0 {
        ascii_file_name(section.path.get(..chars).unwrap_or(&[]), name)
    } else {
        0
    };
    (unknown_if_empty(len, name), rva)
}

/// 名前が引けなかった（長さ 0）ときは `?` 1 文字にする。モジュールの場所は分かっているので RVA は残す。
fn unknown_if_empty(len: usize, name: &mut [u8]) -> usize {
    if len > 0 {
        return len;
    }
    name[0] = b'?';
    1
}

/// パスの最後の部分を `out` に入れた長さ（ASCII 以外は `?`。入りきらない分は切る）。
fn ascii_file_name(wide: &[u16], out: &mut [u8]) -> usize {
    let start = wide
        .iter()
        .rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
        .map_or(0, |i| i + 1);
    let mut len = 0;
    for (dst, &c) in out.iter_mut().zip(&wide[start..]) {
        *dst = if c < 0x80 { c as u8 } else { b'?' };
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    fn spawn(exe: &str, args: &[&str]) -> Spawn {
        Spawn {
            exe: PathBuf::from(exe),
            args: args.iter().map(OsString::from).collect(),
            cwd: PathBuf::from("C:\\"),
            env: Vec::new(),
            no_activate: false,
        }
    }

    fn s(v: &[u16]) -> String {
        String::from_utf16(&v[..v.len() - 1]).unwrap()
    }

    #[test]
    fn command_line_quotes_each_argument() {
        let c = command_line(&spawn(
            "C:\\a b\\x.exe",
            &["--selftest-target", "C:\\d e\\"],
        ))
        .unwrap();
        assert_eq!(*c.last().unwrap(), 0);
        assert_eq!(
            s(&c),
            "\"C:\\a b\\x.exe\" \"--selftest-target\" \"C:\\d e\\\\\""
        );
        let c = command_line(&spawn("C:\\x.exe", &[])).unwrap();
        assert_eq!(s(&c), "\"C:\\x.exe\"");
    }

    #[test]
    fn startup_info_hides_activation_only_when_asked() {
        let quiet = startup_info(true);
        assert_eq!(quiet.dwFlags & 0x0000_0001, 0x0000_0001);
        assert_eq!(quiet.wShowWindow, 4);
        let normal = startup_info(false);
        assert_eq!(normal.dwFlags, 0);
        assert_eq!(normal.wShowWindow, 0);
        for si in [&quiet, &normal] {
            assert_eq!(si.cb as usize, size_of::<STARTUPINFOW>());
        }
    }

    #[test]
    fn command_line_rejects_quote_and_nul() {
        assert!(command_line(&spawn("C:\\x.exe", &["a\"b"])).is_none());
        assert!(command_line(&spawn("C:\\x.exe", &["a\0b"])).is_none());
    }

    #[test]
    fn os_version_and_local_time_are_read() {
        let v = os_version();
        let parts: Vec<u32> = v.split('.').filter_map(|p| p.parse().ok()).collect();
        assert!(parts.len() == 3 && parts[0] >= 6, "{v}");
        let t = local_stamp();
        assert!(t.year >= 2024 && (1..=12).contains(&t.month) && (1..=31).contains(&t.day));
        assert!(t.hour < 24 && t.minute < 60 && t.second < 61 && t.millis < 1000);
    }

    #[test]
    fn the_probe_tells_a_present_function_from_a_missing_one() {
        let mut p = RealProbe::default();
        let name = |s: &str| ImportName::Name(s.to_string());
        assert_eq!(
            p.probe("KERNEL32.dll", &name("CreateFileW")),
            Presence::Found
        );
        assert_eq!(
            p.probe("kernel32.dll", &name("HigaNoSuchFunctionXyz")),
            Presence::NoFunction
        );
        assert_eq!(
            p.probe("kernel32.dll", &ImportName::Ordinal(0xFFFF)),
            Presence::NoFunction
        );
        assert_eq!(
            p.probe("higan_no_such_dll_xyz.dll", &name("F")),
            Presence::NoDll
        );
    }

    // 何が壊れたら落ちるか: import 表の読み取りか引き当てが誤ると、実際に動いているこの exe の import が「無い」と出る。
    #[test]
    fn every_import_of_this_running_exe_is_found() {
        let me = std::env::current_exe().unwrap();
        let bytes = std::fs::read(me).unwrap();
        let imports = crate::imports::read_imports(&bytes).expect("この exe の import 表");
        assert!(imports.len() > 20, "{} 件しか読めていない", imports.len());
        let report = crate::imports::check(&bytes, &mut RealProbe::default()).unwrap();
        assert_eq!(report.checked, imports.len());
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert!(report.unchecked.is_empty(), "{:?}", report.unchecked);
    }

    // 組み上がった DLL の import が、この機械で全部引けること。DLL の場所は環境変数 HIGANR_IMPORT_CHECK_DLL
    // （無ければ build\bin の HiganRTool.dll）。build.bat の後に `--ignored` で走らせる。
    #[test]
    #[ignore = "組み上がった DLL を読むので、build.bat の後に --ignored で走らせる"]
    fn every_import_of_the_built_dll_is_found_here() {
        let path = std::env::var_os("HIGANR_IMPORT_CHECK_DLL")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/bin/HiganRTool.dll")
            });
        let bytes = std::fs::read(&path).expect("DLL が読める");
        let report = crate::imports::check(&bytes, &mut RealProbe::default()).unwrap();
        println!("{} 件を引いた: {report:?}", report.checked);
        assert!(
            report.checked > 100,
            "{} 件しか読めていない",
            report.checked
        );
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert!(report.unchecked.is_empty(), "{:?}", report.unchecked);
    }

    // 何が壊れたら落ちるか: イメージの先頭（RVA の基準）か名前の引き方が誤ると、実行ファイル・システムの DLL の名前か RVA が合わない。
    // イメージの外（スタック・小さい値）を引けたことにすると落ちる。
    #[test]
    fn module_of_names_the_exe_and_the_offset_inside_it() {
        let mut name = [0u8; MODULE_NAME_MAX];
        let at = local_stamp as *const () as usize;
        let (len, rva) = module_of(at, &mut name);
        let me = std::env::current_exe().unwrap();
        let want = me.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(String::from_utf8_lossy(&name[..len]), want);
        // SAFETY: 引数 NULL は自分の exe の読み込み先頭を返すだけ。
        let base = unsafe { GetModuleHandleW(ptr::null()) } as usize;
        assert_eq!(rva as usize, at - base);

        let sys = GetCurrentThreadId as *const () as usize;
        let mut name = [0u8; MODULE_NAME_MAX];
        let (len, rva) = module_of(sys, &mut name);
        let shown = String::from_utf8_lossy(&name[..len]).to_ascii_lowercase();
        assert!(shown.ends_with(".dll"), "{shown}");
        assert!(rva > 0, "RVA が 0");

        let on_stack = 0u32;
        let mut name = [0u8; MODULE_NAME_MAX];
        assert_eq!(
            module_of(&on_stack as *const u32 as usize, &mut name),
            (0, 0)
        );
        assert_eq!(module_of(0x10, &mut name), (0, 0));
    }

    #[test]
    fn ascii_file_name_keeps_the_last_part_and_masks_the_rest() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let mut out = [0u8; 8];
        let n = ascii_file_name(&w("\\Device\\HarddiskVolume3\\x\\a.dll"), &mut out);
        assert_eq!(&out[..n], b"a.dll");
        let n = ascii_file_name(&w("C:/dir/b.exe"), &mut out);
        assert_eq!(&out[..n], b"b.exe");
        let n = ascii_file_name(&w("d\\あ1.dll"), &mut out);
        assert_eq!(&out[..n], b"?1.dll");
        let n = ascii_file_name(&w("d\\0123456789.dll"), &mut out);
        assert_eq!(&out[..n], b"01234567", "入りきらない分を切る");
        let n = ascii_file_name(&w("dir\\"), &mut out);
        assert_eq!(n, 0);
        let n = unknown_if_empty(n, &mut out);
        assert_eq!(&out[..n], b"?");
        out[..3].copy_from_slice(b"a.b");
        assert_eq!(unknown_if_empty(3, &mut out), 3);
        assert_eq!(&out[..3], b"a.b");
    }

    #[test]
    fn write_exception_writes_one_line_with_module_and_rva() {
        use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD;
        let dir =
            std::env::temp_dir().join(format!("HiganRTool_loader_exc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.log");
        let file = File::create(&path).unwrap();
        let mut rec = EXCEPTION_RECORD {
            ExceptionCode: 0xC000_0005u32 as i32,
            ExceptionAddress: local_stamp as *const () as *mut c_void,
            NumberParameters: 2,
            ..Default::default()
        };
        rec.ExceptionInformation[0] = 1;
        rec.ExceptionInformation[1] = 0x10;
        let info = EXCEPTION_POINTERS {
            ExceptionRecord: &mut rec,
            ContextRecord: ptr::null_mut(),
        };
        // SAFETY: `info` は有効な EXCEPTION_RECORD を指す。
        unsafe { write_exception(&file, &info) };
        // null の情報は何も書かず落ちない。
        // SAFETY: null は許される入力。
        unsafe { write_exception(&file, ptr::null()) };
        let text = std::fs::read_to_string(&path).unwrap();
        let me = std::env::current_exe().unwrap();
        let want = me.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            text.starts_with("未処理の例外: コード 0xC0000005（アクセス違反）アドレス 0x")
                && text.contains(&format!(" = {want}+0x"))
                && text.ends_with(" / 書き 0x00000010\n")
                && text.lines().count() == 1,
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    const CHILD_ENV: &str = "HIGANR_TEST_EXCEPTION_LOG";

    // 親の検査（下）が別プロセスとして起こす。単独で走らせても何もしない。
    #[test]
    #[ignore = "親の検査が別プロセスで起こす（落ちるため）"]
    fn exception_filter_child() {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            EXCEPTION_EXECUTE_HANDLER, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
        };
        let Some(path) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        // SAFETY: 自分のプロセスのエラー表示の設定を変えるだけ（検査が落ちたときの報告の窓を出さない）。
        unsafe { SetErrorMode(SEM_NOGPFAULTERRORBOX | SEM_FAILCRITICALERRORS) };
        install_exception_filter(EXCEPTION_EXECUTE_HANDLER);
        // "-" はファイルを渡さない回（標準エラーに書かれるはず）。
        if path != "-" {
            let file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .unwrap();
            attach_exception_file(file);
        }
        // SAFETY: わざと、先頭の 64 KB の中（必ず未割り当て）へ書いて未処理のアクセス違反を起こす。
        unsafe { (0x10 as *mut u32).write_volatile(1) };
    }

    // 何が壊れたら落ちるか: フィルターが置かれない・例外の情報を読み違えると、実際に落ちたプロセスのログにこの 1 行が残らない。
    #[test]
    fn the_exception_filter_logs_a_real_access_violation() {
        let dir = std::env::temp_dir().join(format!("HiganRTool_loader_av_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("crash.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "real::tests::exception_filter_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, &log)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0xC000_0005u32 as i32),
            "子はアクセス違反で終わるはず: {:?} {}",
            out.status,
            String::from_utf8_lossy(&out.stdout)
        );
        let text = std::fs::read_to_string(&log).expect("フィルターがログを書いたはず");
        let me = std::env::current_exe().unwrap();
        let want = me.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            text.contains("0xC0000005（アクセス違反）")
                && text.contains(&format!(" = {want}+0x"))
                && text.contains(" / 書き 0x00000010"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 何が壊れたら落ちるか: ログのファイルを渡す前の未処理の例外が画面（標準エラー）に出ない、または渡していないのにファイルを作る。
    #[test]
    fn without_a_log_file_the_filter_writes_to_the_screen_only() {
        let dir = std::env::temp_dir().join(format!(
            "HiganRTool_loader_av_screen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .current_dir(&dir)
            .args([
                "--exact",
                "real::tests::exception_filter_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "-")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0xC000_0005u32 as i32));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("未処理の例外: コード 0xC0000005（アクセス違反）")
                && err.contains(" / 書き 0x00000010"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "ファイルを作った"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
