//! `bcryptprimitives.dll!ProcessPrng` の遅延読み込みの失敗・欠落への備え。
//! Rust の標準ライブラリは `HashMap` の乱数の種を `ProcessPrng` で引き、通常の import で結ぶ。この関数が無い・実装されていない Wine では exe や DLL が動き出す前後に落ちる。そこで loader と dll の `build.rs` が `/DELAYLOAD:` で最初の呼びまで遅らせ、MSVC の遅延読み込みの hook（`delayimp.h`）で次の 3 通りをふさぐ。
//! DLL が読めない: 失敗の hook（`__pfnDliFailureHook2`、`dliFailLoadLib`）が自プロセスの exe のハンドルを返し、helper の `GetProcAddress` の失敗から次へ進む。DLL にその名前の export が無い: 失敗の hook（`dliFailGetProc`）が [`process_prng_fallback`] を返す。
//! export はあるが `@ stub`（呼ぶと中断する穴埋め）: `GetProcAddress` が成功するので失敗の hook は呼ばれない。通知の hook（`__pfnDliNotifyHook2`、`dliNotePreGetProcAddress`）が、Wine の上でだけ先に代わりを返す。
//! Windows では DLL も関数もあるので、どちらの hook も本物を使わせる。hook の変数は [`install_delay_load_hook!`] で使う側のクレートに定義する（rlib の中の定義は、リンカーが delayimp.lib の既定の定義より先に選ぶ保証が無い）。

use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};

/// 遅延読み込みにする DLL の名前。`build.rs` の `/DELAYLOAD:` はこの定数から作る。
pub const PRNG_DLL: &str = "bcryptprimitives.dll";
/// 標準ライブラリが引く関数の名前。
pub const PRNG_PROC: &str = "ProcessPrng";
/// Wine の ntdll だけが出す関数。あれば Wine の上。
const WINE_MODULE: &str = "ntdll.dll";
const WINE_EXPORT: &CStr = c"wine_get_version";

/// `delayimp.h` の通知の番号（0 始まり）。
pub const DLI_NOTE_PRE_GET_PROC: u32 = 2;
pub const DLI_FAIL_LOAD_LIB: u32 = 3;
pub const DLI_FAIL_GET_PROC: u32 = 4;

/// `delayimp.h` の `DelayLoadProc`（`szProcName` と `dwOrdinal` の共用体は、i686 ではどちらも 4 バイト）。
#[repr(C)]
pub struct DelayLoadProc {
    pub import_by_name: i32,
    pub name_or_ordinal: *const u8,
}

/// `delayimp.h` の `DelayLoadInfo`。hook が読むだけ。
#[repr(C)]
pub struct DelayLoadInfo {
    pub cb: u32,
    pub pidd: *const c_void,
    pub ppfn: *mut *const c_void,
    pub sz_dll: *const u8,
    pub dlp: DelayLoadProc,
    pub hmod_cur: HMODULE,
    pub pfn_cur: *const c_void,
    pub last_error: u32,
}

/// hook の型（`PfnDliHook`）。
pub type DelayLoadHook = unsafe extern "system" fn(u32, *const DelayLoadInfo) -> *const c_void;

/// `ProcessPrng` と同じ型（`BOOL ProcessPrng(PBYTE, SIZE_T)`）。
pub type ProcessPrngFn = unsafe extern "system" fn(*mut u8, usize) -> i32;

/// 戻り値は BOOLEAN（AL だけが有効）。上位ビットは不定なので、u32 で受けて下位 8 ビットだけ見る。
type RtlGenRandomFn = unsafe extern "system" fn(*mut u8, u32) -> u32;

/// hook の変数 `__pfnDliFailureHook2` と `__pfnDliNotifyHook2`（i686 の C の名前は先頭に `_` が付く）を定義する。
/// 使う側のクレートで 1 回だけ呼ぶ（loader の `real.rs`・dll の `win.rs`）。
/// `#[no_mangle]` の `static` にすると rustc が cdylib の export に出してしまうので、アセンブラで 4 バイトの定数として定義する。
#[macro_export]
macro_rules! install_delay_load_hook {
    () => {
        #[cfg(all(target_arch = "x86", target_os = "windows"))]
        core::arch::global_asm!(
            ".globl ___pfnDliFailureHook2",
            ".globl ___pfnDliNotifyHook2",
            ".section .rdata,\"dr\"",
            ".p2align 2",
            "___pfnDliFailureHook2:",
            ".long {failure}",
            "___pfnDliNotifyHook2:",
            ".long {notify}",
            ".text",
            failure = sym $crate::delay_load_failure_hook,
            notify = sym $crate::delay_load_notify_hook,
        );
    };
}

/// [`install_delay_load_hook!`] で定義した 2 つの変数が、リンカーに自前の hook として選ばれていることを検査する
/// （変数の綴りを誤ると delayimp.lib の既定の定義が選ばれ、リンクは通る）。検査の中で 1 回呼ぶ。
#[macro_export]
macro_rules! assert_delay_load_hooks_installed {
    () => {{
        #[allow(non_upper_case_globals)]
        unsafe extern "C" {
            static __pfnDliFailureHook2: usize;
            static __pfnDliNotifyHook2: usize;
        }
        // SAFETY: どちらも `install_delay_load_hook!` が `.rdata` に置いた 4 バイトの定数。
        let (failure, notify) = unsafe { (__pfnDliFailureHook2, __pfnDliNotifyHook2) };
        assert_eq!(failure, $crate::delay_load_failure_hook as *const () as usize);
        assert_eq!(notify, $crate::delay_load_notify_hook as *const () as usize);
    }};
}

/// 代わりの関数が呼ばれた回数（0 なら本物の `ProcessPrng` を使えている）。
static FALLBACK_CALLS: AtomicU32 = AtomicU32::new(0);
/// そのうち、OS の乱数が引けず非暗号の種で埋めた回数。
static WEAK_CALLS: AtomicU32 = AtomicU32::new(0);
/// 非暗号の種を呼びごとに変えるための通し番号（時計の刻みが同じ瞬間の 2 回の呼びでも別の列になる）。
static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);
/// `advapi32!SystemFunction036` のアドレス。0 は未解決、1 は引けなかった。
static RTL_GEN_RANDOM: AtomicUsize = AtomicUsize::new(0);

pub fn fallback_calls() -> u32 {
    FALLBACK_CALLS.load(Ordering::Relaxed)
}

pub fn weak_calls() -> u32 {
    WEAK_CALLS.load(Ordering::Relaxed)
}

/// 代わりが使われたか。数を読むだけで、乱数は引かない（引くと、まだ結ばれていない `ProcessPrng` をここで結んでしまい、
/// 「使わなかった」と誤って記録する）。
pub fn fallback_in_use() -> bool {
    fallback_calls() > 0
}

/// `fallback_in_use` が真のときにログへ書く一行。
pub fn fallback_log_line() -> String {
    let weak = weak_calls();
    format!(
        "乱数: {PRNG_DLL} の {PRNG_PROC} を使えないので代わりを使った（{} 回。OS の乱数を引けず非暗号の種で埋めた {weak} 回）",
        fallback_calls()
    )
}

fn eq_ignore_ascii_case(a: &[u8], b: &str) -> bool {
    a.eq_ignore_ascii_case(b.as_bytes())
}

/// 代わりを出す相手か。DLL 名は大文字小文字を区別せず、関数名は区別する（`GetProcAddress` と同じ）。
/// 順序番号の import（`import_by_name` が 0）は断る。
pub fn is_prng_import(dll: &[u8], proc_name: Option<&[u8]>) -> bool {
    eq_ignore_ascii_case(dll, PRNG_DLL) && proc_name == Some(PRNG_PROC.as_bytes())
}

/// `module` が読み込み済みで `name` を export しているか（読み込みはしない）。
fn exports(module: &str, name: &CStr) -> bool {
    let mut w = [0u16; 32];
    for (d, s) in w.iter_mut().zip(module.encode_utf16().take(31)) {
        *d = s;
    }
    // SAFETY: 終端 0 つきの UTF-16 の名前。参照カウントは増えない。
    let h = unsafe { GetModuleHandleW(w.as_ptr()) };
    // SAFETY: 有効なモジュールのハンドル（NULL なら GetProcAddress が失敗を返すだけ）と終端 0 つきの名前。
    !h.is_null() && unsafe { GetProcAddress(h, name.as_ptr().cast()) }.is_some()
}

/// Wine の上か（`ntdll.dll` が `wine_get_version` を出している）。
pub fn is_wine() -> bool {
    is_wine_with(&exports)
}

fn is_wine_with(lookup: &dyn Fn(&str, &CStr) -> bool) -> bool {
    lookup(WINE_MODULE, WINE_EXPORT)
}

/// hook に渡された import の (DLL 名, 関数名)。
///
/// # Safety
/// `info` の文字列は終端 0 つき（helper が渡す形）。
unsafe fn import_names(info: &DelayLoadInfo) -> Option<(&[u8], Option<&[u8]>)> {
    if info.sz_dll.is_null() {
        return None;
    }
    // SAFETY: 呼び出し元の約束どおり、DLL の名前は import 表の終端 0 つきの文字列。
    let dll = unsafe { CStr::from_ptr(info.sz_dll.cast()) }.to_bytes();
    let proc_name = if info.dlp.import_by_name != 0 && !info.dlp.name_or_ordinal.is_null() {
        // SAFETY: 名前での import のとき、同じ共用体は終端 0 つきの関数名を指す。
        Some(unsafe { CStr::from_ptr(info.dlp.name_or_ordinal.cast()) }.to_bytes())
    } else {
        None
    };
    Some((dll, proc_name))
}

fn fallback_ptr() -> *const c_void {
    process_prng_fallback as ProcessPrngFn as usize as *const c_void
}

/// 遅延読み込みの失敗の通知を受ける。`ProcessPrng` を求められたときだけ答え、ほかは 0（既定の動き = 例外）を返す。
/// `dliFailLoadLib`（DLL が読めなかった）では、export の無い自プロセスの exe のハンドルを返す（有効な `HMODULE` を返すと helper がそこから `GetProcAddress` を試みて失敗し、次の `dliFailGetProc` に来る）。`dliFailGetProc`（DLL に export が無い）では代わりの関数を返す。
///
/// # Safety
/// `info` は helper が渡す有効な `DelayLoadInfo` を指す（文字列は終端 0 つき）。null なら何もしない。
pub unsafe extern "system" fn delay_load_failure_hook(
    notify: u32,
    info: *const DelayLoadInfo,
) -> *const c_void {
    if !matches!(notify, DLI_FAIL_LOAD_LIB | DLI_FAIL_GET_PROC) || info.is_null() {
        return core::ptr::null();
    }
    // SAFETY: 呼び出し元の約束どおり `info` は有効な構造体。
    let info = unsafe { &*info };
    // SAFETY: 同上（文字列は終端 0 つき）。
    let Some((dll, proc_name)) = (unsafe { import_names(info) }) else {
        return core::ptr::null();
    };
    match notify {
        DLI_FAIL_LOAD_LIB if eq_ignore_ascii_case(dll, PRNG_DLL) => {
            // SAFETY: 引数 NULL は「この exe 自身」。参照カウントは増えない。
            let exe = unsafe { GetModuleHandleW(core::ptr::null()) };
            exe.cast_const()
        }
        DLI_FAIL_GET_PROC if is_prng_import(dll, proc_name) => fallback_ptr(),
        _ => core::ptr::null(),
    }
}

/// 遅延読み込みの通知の hook。`dliNotePreGetProcAddress` で、`ProcessPrng` を求められ、かつ Wine の上のときだけ代わりを返す
/// （helper は `GetProcAddress` を呼ばずにそれを使う）。Wine の判定は import が `ProcessPrng` のときにしか走らない。
///
/// # Safety
/// [`delay_load_failure_hook`] と同じ。
pub unsafe extern "system" fn delay_load_notify_hook(
    notify: u32,
    info: *const DelayLoadInfo,
) -> *const c_void {
    // SAFETY: 呼び出し元の約束をそのまま渡す。
    unsafe { notify_hook_with(&is_wine, notify, info) }
}

/// [`delay_load_notify_hook`] の本体。Wine の判定を差し替えられる（本物の helper を通す検査が使う）。
///
/// # Safety
/// [`delay_load_failure_hook`] と同じ。
pub unsafe fn notify_hook_with(
    wine: &dyn Fn() -> bool,
    notify: u32,
    info: *const DelayLoadInfo,
) -> *const c_void {
    if notify != DLI_NOTE_PRE_GET_PROC || info.is_null() {
        return core::ptr::null();
    }
    // SAFETY: 呼び出し元の約束どおり `info` は有効な構造体。
    let info = unsafe { &*info };
    // SAFETY: 同上（文字列は終端 0 つき）。
    let Some((dll, proc_name)) = (unsafe { import_names(info) }) else {
        return core::ptr::null();
    };
    if is_prng_import(dll, proc_name) && wine() {
        fallback_ptr()
    } else {
        core::ptr::null()
    }
}

/// `ProcessPrng` の代わり。常に TRUE（1）を返す（標準ライブラリは失敗を受けない作り）。
/// `advapi32!SystemFunction036`（RtlGenRandom）で埋め、引けなければ [`weak_fill`] で埋める。
///
/// # Safety
/// `buf` は `len` バイト書ける（`ProcessPrng` と同じ約束。NULL または `len` が 0 なら何も書かない）。
pub unsafe extern "system" fn process_prng_fallback(buf: *mut u8, len: usize) -> i32 {
    FALLBACK_CALLS.fetch_add(1, Ordering::Relaxed);
    if buf.is_null() || len == 0 {
        return 1;
    }
    // SAFETY: 呼び出し元（標準ライブラリ）が `len` バイト書ける領域を渡す約束（`ProcessPrng` と同じ）。
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if !os_random_fill(out) {
        WEAK_CALLS.fetch_add(1, Ordering::Relaxed);
        weak_fill(out, weak_seed());
    }
    1
}

fn os_random_fill(out: &mut [u8]) -> bool {
    let Some(f) = rtl_gen_random() else {
        return false;
    };
    for chunk in out.chunks_mut(1 << 20) {
        // SAFETY: `chunk` は書ける領域で、長さは u32 に収まる。`f` は advapi32 の SystemFunction036（BOOLEAN WINAPI RtlGenRandom(PVOID, ULONG)）。
        if unsafe { f(chunk.as_mut_ptr(), chunk.len() as u32) } & 0xFF == 0 {
            return false;
        }
    }
    true
}

fn rtl_gen_random() -> Option<RtlGenRandomFn> {
    match RTL_GEN_RANDOM.load(Ordering::Relaxed) {
        0 => {}
        1 => return None,
        // SAFETY: 下で SystemFunction036 のアドレスとして置いた値。
        p => return Some(unsafe { core::mem::transmute::<usize, RtlGenRandomFn>(p) }),
    }
    let advapi32: [u16; 13] = {
        let mut w = [0u16; 13];
        for (d, s) in w.iter_mut().zip("advapi32.dll".encode_utf16()) {
            *d = s;
        }
        w
    };
    // SAFETY: 終端 0 つきの UTF-16 の名前。読み込み済みならそれを使い、参照カウントは増やさない。
    let mut module = unsafe { GetModuleHandleW(advapi32.as_ptr()) };
    if module.is_null() {
        // SAFETY: 同上。ファイルハンドルは NULL（予約）、探索は System32 だけ。
        module = unsafe {
            LoadLibraryExW(
                advapi32.as_ptr(),
                core::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
    }
    let addr = if module.is_null() {
        None
    } else {
        // SAFETY: 有効なモジュールのハンドルと終端 0 つきの ASCII の名前。無ければ None。
        unsafe { GetProcAddress(module, c"SystemFunction036".as_ptr().cast()) }
    };
    match addr {
        Some(f) => {
            RTL_GEN_RANDOM.store(f as usize, Ordering::Relaxed);
            // SAFETY: Windows の advapi32 がこの名前で出す関数は `BOOLEAN RtlGenRandom(PVOID, ULONG)`（stdcall）。
            Some(unsafe {
                core::mem::transmute::<unsafe extern "system" fn() -> isize, RtlGenRandomFn>(f)
            })
        }
        None => {
            RTL_GEN_RANDOM.store(1, Ordering::Relaxed);
            None
        }
    }
}

/// OS の乱数が引けないときの最後の手の種。暗号の強さは無い（標準ライブラリの `HashMap` の種を 0 のままにしないためだけ。
/// 秘密の鍵はこの経路を通らず、dll が `BCryptGenRandom` で直接引く）。
fn weak_seed() -> u64 {
    let mut qpc = 0i64;
    // SAFETY: 書ける i64 を渡す。失敗しても 0 のまま。
    unsafe { QueryPerformanceCounter(&mut qpc) };
    // SAFETY: 引数なしの呼び。
    let (tick, pid, tid) = unsafe {
        (
            GetTickCount64(),
            GetCurrentProcessId(),
            GetCurrentThreadId(),
        )
    };
    let local = 0u8;
    let mut s = qpc as u64;
    for v in [
        tick,
        (u64::from(pid) << 32) | u64::from(tid),
        core::ptr::addr_of!(local) as usize as u64,
        SEED_COUNTER.fetch_add(1, Ordering::Relaxed),
    ] {
        s = splitmix64(s ^ v);
    }
    s
}

/// `seed` から決まる列で `out` を埋める（splitmix64）。
pub fn weak_fill(out: &mut [u8], seed: u64) {
    let mut s = seed;
    for chunk in out.chunks_mut(8) {
        s = splitmix64(s);
        chunk.copy_from_slice(&s.to_le_bytes()[..chunk.len()]);
    }
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(dll: &'static CStr, proc_name: Option<&'static CStr>) -> DelayLoadInfo {
        DelayLoadInfo {
            cb: core::mem::size_of::<DelayLoadInfo>() as u32,
            pidd: core::ptr::null(),
            ppfn: core::ptr::null_mut(),
            sz_dll: dll.as_ptr().cast(),
            dlp: DelayLoadProc {
                import_by_name: i32::from(proc_name.is_some()),
                name_or_ordinal: proc_name.map_or(core::ptr::null(), |p| p.as_ptr().cast()),
            },
            hmod_cur: core::ptr::null_mut(),
            pfn_cur: core::ptr::null(),
            last_error: 0,
        }
    }

    fn call(notify: u32, i: &DelayLoadInfo) -> *const c_void {
        // SAFETY: `i` は有効な構造体で、文字列は 'static の終端 0 つき。
        unsafe { delay_load_failure_hook(notify, i) }
    }

    fn notify(wine: bool, n: u32, i: &DelayLoadInfo) -> *const c_void {
        // SAFETY: 同上。
        unsafe { notify_hook_with(&|| wine, n, i) }
    }

    fn fb(buf: *mut u8, len: usize) -> i32 {
        // SAFETY: 呼び出し側が渡すのは書ける配列か NULL（長さ 0 を含む）。
        unsafe { process_prng_fallback(buf, len) }
    }

    fn fallback_addr() -> *const c_void {
        process_prng_fallback as ProcessPrngFn as usize as *const c_void
    }

    // 通知の番号は本物の helper を通す検査（tests/delay_load.rs）が見る。
    #[test]
    fn layout_matches_delayimp_h() {
        #[cfg(target_arch = "x86")]
        assert_eq!(core::mem::size_of::<DelayLoadInfo>(), 36);
        assert_eq!(
            core::mem::offset_of!(DelayLoadInfo, dlp),
            4 * core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn get_proc_failure_for_process_prng_returns_the_fallback() {
        let i = info(c"bcryptprimitives.dll", Some(c"ProcessPrng"));
        assert_eq!(call(DLI_FAIL_GET_PROC, &i), fallback_addr());
        let i = info(c"bcryptPrimitives.DLL", Some(c"ProcessPrng"));
        assert_eq!(call(DLI_FAIL_GET_PROC, &i), fallback_addr());
    }

    #[test]
    fn load_failure_for_the_prng_dll_returns_a_module_without_the_export() {
        let i = info(c"bcryptprimitives.dll", Some(c"ProcessPrng"));
        let h = call(DLI_FAIL_LOAD_LIB, &i);
        assert!(!h.is_null());
        // SAFETY: 返ったハンドルは自分の exe。名前は終端 0 つきの ASCII。
        let p = unsafe { GetProcAddress(h.cast_mut(), c"ProcessPrng".as_ptr().cast()) };
        assert!(p.is_none());
    }

    #[test]
    fn other_dlls_functions_notifications_and_ordinals_get_zero() {
        let p = Some(c"ProcessPrng");
        for (dll, name) in [
            (c"kernel32.dll", p),
            (c"bcryptprimitives.dll.x", p),
            (c"bcryptprimitive.dll", p),
            (c"bcryptprimitives.dll", Some(c"ProcessPrn")),
            (c"bcryptprimitives.dll", Some(c"processprng")),
            (c"bcryptprimitives.dll", Some(c"GetRandom")),
            (c"bcryptprimitives.dll", None),
        ] {
            let i = info(dll, name);
            assert!(call(DLI_FAIL_GET_PROC, &i).is_null(), "{dll:?} {name:?}");
            assert!(
                notify(true, DLI_NOTE_PRE_GET_PROC, &i).is_null(),
                "{dll:?} {name:?}"
            );
        }
        let i = info(c"kernel32.dll", p);
        assert!(call(DLI_FAIL_LOAD_LIB, &i).is_null());
        let i = info(c"bcryptprimitives.dll", p);
        for n in [0, 1, 2, 5, 99] {
            assert!(call(n, &i).is_null(), "{n}");
        }
        // SAFETY: NULL を渡す約束の検査。
        assert!(unsafe { delay_load_failure_hook(DLI_FAIL_GET_PROC, core::ptr::null()) }.is_null());
        let mut i = info(c"bcryptprimitives.dll", None);
        i.sz_dll = core::ptr::null();
        assert!(call(DLI_FAIL_GET_PROC, &i).is_null());
    }

    #[test]
    fn notify_hook_returns_the_fallback_only_for_process_prng_on_wine() {
        let i = info(c"bcryptprimitives.dll", Some(c"ProcessPrng"));
        assert_eq!(notify(true, DLI_NOTE_PRE_GET_PROC, &i), fallback_addr());
        assert!(notify(false, DLI_NOTE_PRE_GET_PROC, &i).is_null());
        for n in [0, 1, 3, 4, 5] {
            assert!(notify(true, n, &i).is_null(), "{n}");
        }
        let other = info(c"kernel32.dll", Some(c"ProcessPrng"));
        let ran = core::cell::Cell::new(false);
        // SAFETY: 有効な構造体。
        let r = unsafe {
            notify_hook_with(
                &|| {
                    ran.set(true);
                    true
                },
                DLI_NOTE_PRE_GET_PROC,
                &other,
            )
        };
        assert!(
            r.is_null() && !ran.get(),
            "ほかの import では Wine の判定を走らせない"
        );
    }

    #[test]
    fn wine_is_judged_by_ntdll_wine_get_version() {
        let only = |m: &'static str, f: &'static CStr| move |a: &str, b: &CStr| a == m && b == f;
        assert!(is_wine_with(&only("ntdll.dll", c"wine_get_version")));
        assert!(!is_wine_with(&only("kernel32.dll", c"wine_get_version")));
        assert!(!is_wine_with(&only("ntdll.dll", c"NtClose")));
        assert!(!is_wine_with(&|_, _| false));
        // 本物の問い合わせが読めている証拠（陽性対照）。この PC は Windows なので `is_wine` は偽。
        assert!(exports("ntdll.dll", c"NtClose") && exports("kernel32.dll", c"GetProcAddress"));
        assert!(!exports("ntdll.dll", c"HigNoSuchExport") && !exports("higanr_no_such.dll", c"x"));
        assert!(!is_wine());
    }

    #[test]
    fn fallback_fills_the_buffer_with_fresh_bytes_and_returns_true() {
        let before = fallback_calls();
        let mut a = [0u8; 4096];
        let mut b = [0u8; 4096];
        assert_eq!(fb(a.as_mut_ptr(), a.len()), 1);
        assert_eq!(fb(b.as_mut_ptr(), b.len()), 1);
        assert!(fallback_calls() >= before + 2);
        // この PC では advapi32 から引けるので、非暗号の種の道は通らない。
        assert_eq!(weak_calls(), 0);
        assert_ne!(a, b);
        for x in [&a, &b] {
            assert!(x.iter().filter(|&&v| v == 0).count() < 100, "ほぼ 0 のまま");
            assert!(x[..8].iter().any(|&v| v != 0) && x[4088..].iter().any(|&v| v != 0));
        }
        // 長さ 0・NULL は何も書かず TRUE。
        assert_eq!(fb(core::ptr::null_mut(), 16), 1);
        let mut c = [0xAAu8; 3];
        assert_eq!(fb(c.as_mut_ptr(), 0), 1);
        assert_eq!(c, [0xAA; 3]);
        // 長さが 8 の倍数でなくても、指定の長さの先は書かない。
        let mut d = [0u8; 12];
        assert_eq!(fb(d.as_mut_ptr(), 5), 1);
        assert_eq!(&d[5..], &[0u8; 7]);
    }

    #[test]
    fn weak_fill_depends_on_the_seed_and_respects_the_length() {
        let (mut a, mut b, mut c) = ([0u8; 13], [0u8; 13], [0u8; 13]);
        weak_fill(&mut a, 1);
        weak_fill(&mut b, 1);
        weak_fill(&mut c, 2);
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut e = [0u8; 0];
        weak_fill(&mut e, 1);
    }

    #[test]
    fn weak_seed_changes_between_calls() {
        assert_ne!(weak_seed(), weak_seed());
    }

    // この検査の exe は遅延読み込みではないので、標準ライブラリの種の取得では代わりが呼ばれない。
    #[test]
    fn log_line_carries_the_counts_and_in_use_follows_the_counter() {
        let before = fallback_calls();
        fb(core::ptr::null_mut(), 0);
        assert!(fallback_in_use());
        // 他の検査も並行して呼ぶので、回数は前の値より大きいことだけを見る。
        let line = fallback_log_line();
        let n: u32 = line
            .split_once('（')
            .and_then(|(_, r)| r.split_once(' '))
            .and_then(|(n, _)| n.parse().ok())
            .unwrap_or_else(|| panic!("{line}"));
        assert!(line.starts_with("乱数:") && n > before, "{line}");
    }
}
