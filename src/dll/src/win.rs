//! Win32 の層（unsafe はこのファイルに閉じ込める。1 つずつ `// SAFETY:`）。
//! 自分のモジュールのパス・exe の位置・自分のプロセスのメモリの読み取り（`VirtualQuery` で頁を確かめてから読む）・時刻・
//! 二重注入のミューテックス・スレッドを 1 本起こすこと・デバッガへの一言。ゲームのメモリに書く API はここに無い（書く層は `hook_win.rs`）。
//! ウィンドウも作らず、ネットワークにも出ない。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::windows::ffi::OsStringExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use core::ffi::c_void;

use higanr_core::log_scrub::{Scrub, ScrubWrite};
use higanr_core::pe::TH09_V150A;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HMODULE, SYSTEMTIME,
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

/// `GetModuleFileNameW` が受け付ける最長（拡張パスの上限）。
const MAX_WIDE_PATH: usize = 32_768;

/// モジュールのパス。`module` が NULL なら自分のプロセスの exe。日本語を含みうるので W の API で取る。
pub fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut buf: Vec<u16> = vec![0; 260];
    loop {
        // SAFETY: `buf` は書ける領域で、長さをそのまま渡している。`module` は NULL か自分の DLL のハンドル
        // （この DLL は自分を解放しない）。
        let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            buf.truncate(n);
            return Some(PathBuf::from(OsString::from_wide(&buf)));
        }
        // n == buf.len() は切り詰められた印。広げて取り直す。
        if buf.len() >= MAX_WIDE_PATH {
            return None;
        }
        buf.resize((buf.len() * 2).min(MAX_WIDE_PATH), 0);
    }
}

/// アドレス `addr` が入っているモジュールのパス（D3D8 の EndScene / Present が d3d8.dll かラッパーかを見るため。参照カウントは増やさない）。
/// どのモジュールにも入っていなければ `None`。
pub fn module_of_addr(addr: u32) -> Option<PathBuf> {
    let mut module: HMODULE = core::ptr::null_mut();
    // SAFETY: 旗は「アドレスから引く」＋「参照カウントを増やさない」（閉じなくてよい）。第 2 引数はアドレスを指す疑似ポインタで、中身は読まれない。
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

/// Wine の版（`ntdll.dll` が `wine_get_version` を持つときだけ）。Windows では `None`。ログの 1 行のためで、ゲームのスレッドでは呼ばない。
pub fn wine_version() -> Option<String> {
    let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
    // SAFETY: 0 終わりの UTF-16 の名前。ntdll は全プロセスに読み込み済みで、参照カウントを増やさない。
    let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: 読み込み済みのモジュールのハンドルと 0 終わりの ASCII の名前。無ければ None が返るだけ。
    let proc = unsafe { GetProcAddress(module, c"wine_get_version".as_ptr().cast()) }?;
    // SAFETY: Wine の ntdll がこの名前で出す関数は引数なしで、静的な C 文字列へのポインタを返す（cdecl。i686 の extern "C" と同じ）。
    let get: extern "C" fn() -> *const core::ffi::c_char = unsafe { core::mem::transmute(proc) };
    let p = get();
    if p.is_null() {
        return None;
    }
    // SAFETY: 上の関数が返す 0 終わりの静的な文字列。
    let s = unsafe { core::ffi::CStr::from_ptr(p) }.to_string_lossy();
    let s: String = s
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(32)
        .collect();
    (!s.is_empty()).then_some(s)
}

/// OS のバージョン（`ntdll!RtlGetVersion`。`GetVersionEx` と違い、exe の互換モードの宣言で値が変わらない）。ゲームのスレッドでは呼ばない。
pub fn os_version() -> Option<probe::OsVersion> {
    let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
    // SAFETY: 0 終わりの UTF-16 の名前。ntdll は全プロセスに読み込み済みで、参照カウントを増やさない。
    let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: 読み込み済みのモジュールのハンドルと 0 終わりの ASCII の名前。無ければ None が返るだけ。
    let proc = unsafe { GetProcAddress(module, c"RtlGetVersion".as_ptr().cast()) }?;
    // SAFETY: RtlGetVersion は `NTSTATUS NTAPI (PRTL_OSVERSIONINFOW)`（stdcall。i686 の extern "system"）。
    let get: unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32 =
        unsafe { core::mem::transmute(proc) };
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: core::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: 大きさの欄を埋めた OSVERSIONINFOW を渡す。関数はそれを書くだけ。
    let status = unsafe { get(&mut info) };
    (status >= 0).then_some(probe::OsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
    })
}

/// CPU の（SSE2, AVX2）。SSE2 は OS の見方（i686 のビルドは SSE2 を前提にしているので、無ければここまで来ない）、
/// AVX2 は CPUID と OS の保存の対応（XGETBV）の両方。
pub fn cpu_features() -> (bool, bool) {
    // SAFETY: 引数は機能の番号だけで、失敗しない。
    let sse2 = unsafe { IsProcessorFeaturePresent(PF_XMMI64_INSTRUCTIONS_AVAILABLE) } != 0;
    (sse2, std::arch::is_x86_feature_detected!("avx2"))
}

pub fn exe_image_base() -> usize {
    // SAFETY: 引数 NULL は自分の exe の意味で、返るハンドルは参照カウントを増やさない。
    unsafe { GetModuleHandleW(core::ptr::null()) as usize }
}

/// 頁の保護が「読める」か。`PAGE_GUARD` は読むと例外が一度起きて保護が外れる（ゲームの状態を変えうる）ので読めない扱い。
/// 修飾は下位 8 ビットで見る。
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

/// 自分のプロセスのメモリ `[addr, addr + len)` を読む。頁がすべてコミット済みで読める保護でなければ `None`。読むだけで保護は変えない。
pub fn read_own_memory(addr: u32, len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; len];
    read_own_into(addr, &mut out).then_some(out)
}

/// `[addr, addr + buf.len())` を `buf` に読む。**ヒープを使わない**（スレッドを止めている間の読み直し用）。
/// 頁がすべてコミット済みで読める保護でなければ false（確かめ終わるまで 1 バイトも書かない）。
pub fn read_own_into(addr: u32, buf: &mut [u8]) -> bool {
    readable_range(addr as usize, buf.len()) && {
        for (i, b) in buf.iter_mut().enumerate() {
            // SAFETY: 範囲の頁を直前に VirtualQuery でコミット済み・読める保護と確かめた。th09.exe のイメージの頁は解放されず、
            // この DLL が保護を変えるのは `hook_win` だけで、それも読める保護の間を行き来するだけ。確かめてから読むまでに別の DLL が保護を外すことは起きない
            // 前提（ローダーがサスペンドで起動して読ませるので、他の注入物は並んでいない）。
            // volatile なのは、ゲームのスレッドが書き換えうるアドレスの読みを最適化で畳ませないため。
            *b = unsafe { core::ptr::read_volatile((addr as usize + i) as *const u8) };
        }
        true
    }
}

/// `[start, start + len)` の頁がすべてコミット済みで読める保護か。ヒープを使わない。
fn readable_range(start: usize, len: usize) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let mut cur = start;
    while cur < end {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: VirtualQuery はアドレスの中身を読まず、領域の情報を書ける大きさの `mbi` に書くだけ。
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
    // SAFETY: `st` は GetLocalTime が書く SYSTEMTIME そのもの。
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

/// デバッガ（DebugView など）への一言。ログを開けないときの最後の知らせ。ウィンドウは出さない。
pub fn debug_out(s: &str) {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` は 0 で終わる UTF-16 の列で、呼び出しの間生きている。
    unsafe { OutputDebugStringW(wide.as_ptr()) };
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexRefusal {
    /// 同じ名前が既にある（同じ PID に 2 本目が読まれた）。
    AlreadyExists,
    /// 作れなかった（`GetLastError` の値）。
    Failed(u32),
}

/// 名前つきのミューテックスを作る。既にあれば、開いてしまったハンドルを閉じて断る。
/// 返したハンドルは閉じない（プロセスが終わるまで名前を残し、2 本目を断り続けるため）。
pub fn acquire_named_mutex(name: &str) -> Result<HANDLE, MutexRefusal> {
    let wide = to_wide_nul(name);
    // SAFETY: 属性は NULL、初めから持たない（FALSE）、名前は呼び出しの間生きている 0 終わりの UTF-16。
    let h = unsafe { CreateMutexW(core::ptr::null(), 0, wide.as_ptr()) };
    // SAFETY: 引数の無い呼び出し。CreateMutexW の直後に読む。
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
    // SAFETY: 呼び出し元は、自分が開いてまだ閉じていないハンドルだけを渡す。
    unsafe { CloseHandle(h) };
}

/// DLL_PROCESS_ATTACH でやること（ローダーロックの下）: ミューテックスを取り、読み取りのスレッドを起こすだけ。待たない。
/// false を返すと DllMain が FALSE を返し、DLL は読まれない。
pub fn on_process_attach(module: HMODULE) -> bool {
    let name = mutex_name(std::process::id());
    let mutex = match acquire_named_mutex(&name) {
        Ok(h) => h,
        Err(MutexRefusal::AlreadyExists) => {
            debug_out("HiganRTool: 同じプロセスに既に読まれている（2 本目は読まない）");
            return false;
        }
        Err(MutexRefusal::Failed(code)) => {
            // 1 本目と確かめられないので読まない（2 本が同じ地点を見張るより、始めない方が安全）。
            debug_out(&format!(
                "HiganRTool: 二重注入のミューテックスを作れない（GetLastError = {code}）"
            ));
            return false;
        }
    };
    // ウィンドウモードのウィンドウを作る呼び出しの差し替え（ini の `[window]`）。ゲームのメインスレッドが走り出す前のここでしか間に合わない。
    // ini はここで読まない（ローダーロック）。失敗しても DLL の読み込みは止めない。
    crate::window_win::install(module);
    // SAFETY: `probe_thread` は LPTHREAD_START_ROUTINE の形。引数は自分の DLL のハンドルで、この DLL は自分を解放しない。
    // 属性 NULL・スタックの大きさ既定・すぐ走らせる・スレッド ID は不要。
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

/// 読み取りのスレッドの入口。パニックはここで受け、OS へ出さない。
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
    // 例外の記録は、この後のどこで落ちても残るように、ログを開いた直後に入れる。
    let veh = crate::crash_win::install(&file);
    // 区切りの行はウィンドウとパッドの頼まれた行（`DllMain` から積まれる）より先に、包みの外から 1 回で書く。
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
    // ウィンドウとパッドの行（別のスレッドが頼む）は、この包みが本ログの行の先頭ごとに先に書く。
    // 書く行は 1 行ずつ、ユーザー名・フォルダ・IP のアドレスを伏せてからファイルへ渡す。
    let mut file = crate::main_log::wrap(ScrubWrite::new(
        file,
        log_scrub(exe.as_deref(), dll.as_deref()),
    ));
    // tick ごとの行などは本ログの隣に、回ごとの名前の先頭で。
    let exe_dir = exe.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    // 台本の読み込み（DLL のフォルダの script.txt を読み、本ログの隣へ移す。ゲームのフォルダの中には触らない）。
    let mut script = RealScriptHost {
        dll_dir: dll.as_deref().and_then(Path::parent).map(Path::to_path_buf),
        run_path: Some(run_path.clone()),
        exe_dir: exe_dir.clone(),
    };
    // 配るバージョンで ini が無ければ、最初に読む前に既定を書き出す（開発用のビルドは書かない）。
    let seed_line = script
        .seed_default_ini(crate::ini_seed::default_for_build(cfg!(feature = "dist")).as_deref())
        .log_line();
    // 簡潔か詳細かは、ログに書き始める前に決める（ini の読めない理由は後の `ini:` の行が書く）。
    let choice = higanr_core::log_cfg::choose(
        script.read_net_ini().ok().flatten().as_deref(),
        crate::log_mode::default_detail(cfg!(feature = "dist")),
    );
    crate::log_mode::set_detail(choice.detail);
    let _ = writeln!(file, "{}", mode_line(choice.detail));
    let (sse2, avx2) = cpu_features();
    let _ = writeln!(file, "{}", probe::system_line(os_version(), sse2, avx2));
    let _ = match &veh {
        Ok(()) => writeln!(detail_out(&mut file), "例外の記録: 入れた"),
        Err(why) => writeln!(file, "例外の記録: 入れられなかった（{why}）"),
    };
    if let Some(v) = wine_version() {
        let _ = writeln!(file, "環境: Wine {v}");
    }
    // 落ちたときの `d3d8.dll+RVA` が OS のものか差し替えかを読めるように。
    let _ = writeln!(file, "{}", crate::d3d8_info_win::d3d8_line());
    if let Some(line) = &seed_line {
        let _ = writeln!(file, "{line}");
    }
    if let Some(note) = &choice.note {
        let _ = writeln!(file, "{note}");
    }
    // 配るバージョンだけ、`logs\` の古いログを消す（このスレッドはゲームのスレッドではない。フックを置く前の数 ms）。
    if cfg!(feature = "dist")
        && let Some(dir) = run_path.parent()
    {
        let (removed, failed) = prune_old_logs(dir, &local_time(), KEEP_LOG_DAYS);
        let line = format!(
            "古いログ: 名前の日付が {KEEP_LOG_DAYS} 日より前のものを {removed} 個消した（消せなかった {failed} 個）"
        );
        if failed > 0 {
            let _ = writeln!(file, "{line}");
        } else {
            let _ = writeln!(detail_out(&mut file), "{line}");
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
            // 描画だけのフレームの実験は 180 秒。
            crate::render_only::TOTAL_SECONDS,
            &mut file,
        )
    }));
    // パッドの終わりの数えの 1 行を頼む。次の本ログへの書き込みの始まりで本ログへ入る。
    crate::pad_extra_win::push_final_counts();
    let _ = match r {
        Ok(End::NotInstalled) => writeln!(file, "終了: スレッドを終える（フックは設置していない）"),
        Ok(End::Installed) => writeln!(
            detail_out(&mut file),
            "終了: スレッドを終える（フックは残る。外す手順は無い）"
        ),
        // パニックがどこで起きたかで、書いたかどうかは変わる。止めたスレッドは `hook::Stopped` の Drop が戻している。
        Err(p) => writeln!(
            file,
            "終了: パニックを受けて止めた（{}）。スレッドを終える（書いたかどうかは上の行を見る）",
            panic_text(p.as_ref())
        ),
    };
    if let Some(line) = crate::crash_win::summary_line() {
        let _ = writeln!(file, "{line}");
    }
    // 「終了」の行の後に頼まれた分も本ログの最後へ流す。
    file.drain_now();
}

/// ログに書く行から伏せるフォルダ（ゲームと DLL）。名前が出ない形に替えて書く。
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
        format!("{BRIEF_MARK}（本ログだけ。詳しく調べるときは ini の [log] の detail を 1 に）")
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

/// 台本・検査の実行か（`names::is_scripted_run`）。本ログを回ごとの名前にする。
fn scripted_run(dll: &Path) -> bool {
    is_scripted_run(
        std::env::var_os(higanr_core::ini::PATH_ENV).as_deref(),
        std::env::var_os(higanr_core::script::PATH_ENV).as_deref(),
        dll.parent()
            .is_some_and(|d| d.join(crate::script_win::SCRIPT_FILE).exists()),
    )
}

/// DLL と同じフォルダの `logs\` に本ログを開く。`logs\` が exe のフォルダと同じかその下なら、書かずに断る。
/// 開いたファイルと、隣のファイルの名前の元になるパス（回ごとの名前。台本の実行では本ログそのもの）を返す。
fn open_log(dll: Option<&Path>, exe: Option<&Path>) -> Result<MainLog, String> {
    let dll = dll.ok_or("DLL のパスが取れない")?;
    let dir = dll
        .parent()
        .ok_or("DLL のフォルダが分からない")?
        .join("logs");
    let exe_dir = exe
        .and_then(Path::parent)
        .ok_or("exe のフォルダが分からないので、ゲームのフォルダの外か確かめられない")?;
    if is_same_or_inside(&dir.to_string_lossy(), &exe_dir.to_string_lossy()) {
        return Err(format!(
            "ログの置き場 {} が exe のフォルダの中なので書かない",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作れない: {e}", dir.display()))?;
    open_main_log(&dir, &local_time(), std::process::id(), scripted_run(dll))
}

/// 開いた本ログ。`run_path` は隣のファイルの名前の元（回ごとの名前。台本の実行では本ログそのもの）。
struct MainLog {
    file: File,
    run_path: PathBuf,
    /// 日ごとの本ログを使えず起動ごとのファイルにしたときの、本ログの先頭に足す 1 行。
    fallback_note: Option<&'static str>,
}

const FALLBACK_NOTE: &str =
    "日ごとの本ログを別の起動が開いていて使えなかったので、この起動は起動ごとのファイルに書く";

/// 日ごとの本ログを開くときの共有の指定（`FILE_SHARE_READ | FILE_SHARE_DELETE`）。読みと削除だけ許し、書き込みは 1 つの起動に限る
/// （同じ日の 2 つの起動が同じファイルへ書くと、区切りの行の範囲が崩れる）。
const DAILY_SHARE: u32 = 1 | 4;

/// `ERROR_SHARING_VIOLATION`。
const SHARING_VIOLATION: i32 = 32;

/// 回ごと（`per_run`）なら、回ごとの名前を `create_new` で開く。そうでなければ日ごとの名前を追記で開く。
/// 日ごとのファイルを別の起動が書き込みで開いていれば（共有違反）、回ごとの名前へ逃がす。
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

/// `dir` の直下で、名前の日付が古いログを消す（判定は `names::is_expired_log`。フォルダとリンクには触らない）。（消した数, 消せなかった数）。
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
    fn now(&self) -> LocalTime {
        local_time()
    }
    fn pid(&self) -> u32 {
        std::process::id()
    }
    fn exe_path(&self) -> Option<String> {
        self.exe.as_ref().map(|p| p.display().to_string())
    }
    fn ident(&self) -> IdentOutcome {
        match &self.exe {
            None => IdentOutcome::Unreadable("exe のパスが取れない".to_string()),
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
    //! 本物の Win32 を、検査のプロセス自身に当てる（花映塚は使わない）。
    use super::*;

    static SAMPLE: [u8; 8] = [0x10, 0x32, 0x54, 0x76, 0x98, 0xBA, 0xDC, 0xFE];

    /// Windows では `wine_get_version` が無く `None`。Wine で走らせたときは版の文字列（空でない ASCII）になる。
    #[test]
    fn wine_version_is_none_or_a_plain_string() {
        if let Some(v) = wine_version() {
            assert!(
                !v.is_empty() && v.chars().all(|c| c.is_ascii_graphic()),
                "{v}"
            );
        }
    }

    /// 壊れたら落ちる: RtlGetVersion の呼び方（構造体の大きさ・呼び出し規約）を誤って値が取れない・0 になる。
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

    /// 壊れたら落ちる: 日ごとの本ログが起動ごとに別ファイルになる・上書き（前の起動の行が消える）・
    /// 隣のファイルの名前の元が日ごとの名前になる（別の起動の CSV と同じ名前になる）。
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

    /// 壊れたら落ちる: DLL のフォルダの `script.txt` を見ずに、台本の実行を日ごとの 1 ファイルへ混ぜる。
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

    /// 壊れたら落ちる: 台本の実行が日ごとの 1 ファイルに混ざる、または既存の回ごとのログを上書きする。
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

    /// 壊れたら落ちる: 同じ日に 2 つの起動が同じ日ごとの本ログへ書き込みで開く（区切りの行の範囲が崩れる）、
    /// または共有違反のときに起動ごとのファイルへ逃げず失敗する・理由の 1 行が付かない、
    /// 書き込み中の日ごとの本ログを既定の共有で読めない。
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
