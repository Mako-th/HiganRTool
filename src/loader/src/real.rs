//! 本物の Win32（`windows-sys`）。ローダーの unsafe は `folder_dialog.rs`・`http_win.rs` とここにだけ置く。
//! 判断はしない（手順は `launch.rs`。ここに置くと偽物の検査が本物の経路を見なくなる）。

use core::ffi::c_void;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX,
    SetErrorMode, WriteProcessMemory,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
    PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx, VirtualProtectEx,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, CreateRemoteThread,
    GetExitCodeProcess, GetExitCodeThread, PROCESS_INFORMATION, ResumeThread, STARTF_USESHOWWINDOW,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

use crate::win::{ErrorCode, Handle, ProcessInfo, Remote, Spawn, WaitResult, Win32};
use higanr_core::code_files::CF_UNICODETEXT;
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
            Lang::Ja => "字の途中に 0 がある",
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

pub fn module_loaded(path: &Path) -> bool {
    let Some(name) = wide_nul(path.as_os_str()) else {
        return false;
    };
    // SAFETY: `name` は終端 0 つきで生きている。参照を増やさずに引くだけ。
    let m = unsafe { GetModuleHandleW(name.as_ptr()) };
    !m.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

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
}
