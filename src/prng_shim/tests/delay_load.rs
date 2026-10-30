//! 本物の遅延読み込み（link.exeの /DELAYLOADとdelayimp.libのhelper）を通して、2つのhookが代わりの関数に着くことを見る。
//! bcryptprimitives.dllはこのPCでは読めて関数もあるので、同じ形の失敗を偽のimportで作る。
//! - `higanr_absent_prng.dll`（存在しないDLL）: helperはLoadLibraryの失敗でdliFailLoadLibを呼ぶ。返ったハンドルからGetProcAddressを試して失敗し、dliFailGetProcを呼ぶ。
//! - `version.dll!HigMissingPrng`（DLLはあるがexportが無い）: dliFailGetProcだけが来る。
//! - `shlwapi.dll!StrChrA` / `StrChrW`（exportがあり、`ProcessPrng`と同じ (ポインタ, 数)の2引数。バッファが空なら何も起きず0を返す）: exportはあるが穴埋めの関数（Wineの`@ stub`）の形。dliNotePreGetProcAddressのhookがWineの上と判定したときだけ代わりに差し替わる。そうでなければ本物が使われる。
//!
//! hookはこのファイルのラッパー。DLL名と関数名をbcryptprimitives.dll!ProcessPrngに読み替え、Wineの判定を差し替えて、製品のhook（`delay_load_failure_hook` / `notify_hook_with`）へ渡す。名前の判定そのものはlib.rsの単体の検査が持つ。

#![cfg(all(windows, target_env = "msvc"))]

use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use higanr_prng_shim::{
    DLI_NOTE_PRE_GET_PROC, DelayLoadHook, DelayLoadInfo, DelayLoadProc, delay_load_failure_hook,
    fallback_calls, notify_hook_with,
};

#[link(name = "higanr_absent_prng", kind = "raw-dylib")]
unsafe extern "system" {
    fn HigAbsentPrng(buf: *mut u8, len: usize) -> i32;
}

#[link(name = "version", kind = "raw-dylib")]
unsafe extern "system" {
    fn HigMissingPrng(buf: *mut u8, len: usize) -> i32;
}

// i686のstdcallは既定で`_StrChrA@8`の名前でimportするので、素の名前にする。
#[link(name = "shlwapi", kind = "raw-dylib", import_name_type = "undecorated")]
unsafe extern "system" {
    fn StrChrA(buf: *mut u8, ch: usize) -> *mut u8;
    fn StrChrW(buf: *mut u8, ch: usize) -> *mut u8;
}

static ABSENT_LOAD: AtomicU32 = AtomicU32::new(0);
static ABSENT_GETPROC: AtomicU32 = AtomicU32::new(0);
static MISSING_LOAD: AtomicU32 = AtomicU32::new(0);
static MISSING_GETPROC: AtomicU32 = AtomicU32::new(0);
/// 差し替えたWineの判定の答え（`STRCHR_LOCK`を持つ間だけ書く）。
static WINE: AtomicBool = AtomicBool::new(false);
static NOTE_SEEN: AtomicU32 = AtomicU32::new(0);
/// Wineの判定と代わりの呼び出し回数を共有するので、4本とも1本ずつ走らせる。
static STRCHR_LOCK: Mutex<()> = Mutex::new(());

#[allow(non_upper_case_globals)]
#[used]
#[unsafe(no_mangle)]
static __pfnDliFailureHook2: Option<DelayLoadHook> = Some(failure_wrapper);

#[allow(non_upper_case_globals)]
#[used]
#[unsafe(no_mangle)]
static __pfnDliNotifyHook2: Option<DelayLoadHook> = Some(notify_wrapper);

fn names(info: *const DelayLoadInfo) -> (Vec<u8>, Vec<u8>) {
    // SAFETY: helperが渡す有効な構造体で、DLL名と関数名は終端0つき。
    unsafe {
        let dll = CStr::from_ptr((*info).sz_dll.cast()).to_bytes().to_vec();
        let p = (*info).dlp.name_or_ordinal;
        let name = if p.is_null() {
            Vec::new()
        } else {
            CStr::from_ptr(p.cast()).to_bytes().to_vec()
        };
        (dll, name)
    }
}

/// `bcryptprimitives.dll!ProcessPrng`を求められた形の構造体に読み替えて返す。
fn as_prng(orig: &DelayLoadInfo) -> DelayLoadInfo {
    DelayLoadInfo {
        cb: orig.cb,
        pidd: orig.pidd,
        ppfn: orig.ppfn,
        sz_dll: c"bcryptprimitives.dll".as_ptr().cast(),
        dlp: DelayLoadProc {
            import_by_name: 1,
            name_or_ordinal: c"ProcessPrng".as_ptr().cast(),
        },
        hmod_cur: orig.hmod_cur,
        pfn_cur: orig.pfn_cur,
        last_error: orig.last_error,
    }
}

unsafe extern "system" fn failure_wrapper(
    notify: u32,
    info: *const DelayLoadInfo,
) -> *const c_void {
    let (dll, _) = names(info);
    let counter = match (dll.as_slice(), notify) {
        (b"higanr_absent_prng.dll", higanr_prng_shim::DLI_FAIL_LOAD_LIB) => &ABSENT_LOAD,
        (b"higanr_absent_prng.dll", higanr_prng_shim::DLI_FAIL_GET_PROC) => &ABSENT_GETPROC,
        (b"version.dll", higanr_prng_shim::DLI_FAIL_LOAD_LIB) => &MISSING_LOAD,
        (b"version.dll", higanr_prng_shim::DLI_FAIL_GET_PROC) => &MISSING_GETPROC,
        _ => return core::ptr::null(),
    };
    counter.fetch_add(1, Ordering::SeqCst);
    // SAFETY: 上と同じ有効な構造体。
    let p = as_prng(unsafe { &*info });
    // SAFETY: 上で作った有効な構造体（文字列は 'staticの終端0つき）。
    unsafe { delay_load_failure_hook(notify, &p) }
}

unsafe extern "system" fn notify_wrapper(notify: u32, info: *const DelayLoadInfo) -> *const c_void {
    if notify != DLI_NOTE_PRE_GET_PROC {
        return core::ptr::null();
    }
    let (dll, name) = names(info);
    if dll != b"shlwapi.dll" || !(name == b"StrChrA" || name == b"StrChrW") {
        return core::ptr::null();
    }
    NOTE_SEEN.fetch_add(1, Ordering::SeqCst);
    // SAFETY: 同上。
    let p = as_prng(unsafe { &*info });
    // SAFETY: 同上。
    unsafe { notify_hook_with(&|| WINE.load(Ordering::SeqCst), notify, &p) }
}

fn assert_filled(r: i32, buf: &[u8]) {
    assert_eq!(r, 1, "代わりの関数は TRUE を返す");
    assert!(
        buf.iter().any(|&b| b != 0),
        "バッファが 0 のまま: {buf:02X?}"
    );
}

#[test]
fn missing_dll_goes_through_load_failure_then_get_proc_failure() {
    let _g = STRCHR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut buf = [0u8; 64];
    // SAFETY: 遅延読み込みのhelperがフックの返した代わりの関数（`ProcessPrng`と同じ型）に結ぶ。
    let r = unsafe { HigAbsentPrng(buf.as_mut_ptr(), buf.len()) };
    assert_filled(r, &buf);
    assert_eq!(ABSENT_LOAD.load(Ordering::SeqCst), 1);
    assert_eq!(ABSENT_GETPROC.load(Ordering::SeqCst), 1);
    // 2回目はIATに結ばれているので、フックは来ない。
    let mut buf2 = [0u8; 64];
    // SAFETY: 同上。
    let r = unsafe { HigAbsentPrng(buf2.as_mut_ptr(), buf2.len()) };
    assert_filled(r, &buf2);
    assert_ne!(buf, buf2);
    assert_eq!(ABSENT_LOAD.load(Ordering::SeqCst), 1);
    assert_eq!(ABSENT_GETPROC.load(Ordering::SeqCst), 1);
}

#[test]
fn missing_export_goes_through_get_proc_failure_only() {
    let _g = STRCHR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut buf = [0u8; 64];
    // SAFETY: 同上。
    let r = unsafe { HigMissingPrng(buf.as_mut_ptr(), buf.len()) };
    assert_filled(r, &buf);
    assert_eq!(MISSING_LOAD.load(Ordering::SeqCst), 0);
    assert_eq!(MISSING_GETPROC.load(Ordering::SeqCst), 1);
}

#[test]
fn existing_export_is_replaced_when_the_notify_hook_says_wine() {
    let _g = STRCHR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    WINE.store(true, Ordering::SeqCst);
    let (seen, calls) = (NOTE_SEEN.load(Ordering::SeqCst), fallback_calls());
    let mut buf = [0u8; 64];
    // SAFETY: helperが代わりの関数（`ProcessPrng`と同じ型）に結ぶ。本物の`StrChrA`ならバッファ（空の文字列）に0x40を探して0を返すだけ。
    let r = unsafe { StrChrA(buf.as_mut_ptr(), 64) };
    assert_eq!(NOTE_SEEN.load(Ordering::SeqCst), seen + 1);
    assert_eq!(fallback_calls(), calls + 1, "代わりが呼ばれていない");
    assert_eq!(r as usize, 1, "本物の StrChrA なら 0（見つからない）");
    assert!(
        buf.iter().any(|&b| b != 0),
        "代わりがバッファを埋めていない"
    );
}

#[test]
fn existing_export_is_used_as_is_when_not_wine() {
    let _g = STRCHR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    WINE.store(false, Ordering::SeqCst);
    let (seen, calls) = (NOTE_SEEN.load(Ordering::SeqCst), fallback_calls());
    let mut buf = [0u8; 64];
    // SAFETY: 本物の`StrChrW`（空の文字列から0x40を探す）。
    let r = unsafe { StrChrW(buf.as_mut_ptr(), 64) };
    assert_eq!(
        NOTE_SEEN.load(Ordering::SeqCst),
        seen + 1,
        "通知の hook は呼ばれる"
    );
    assert!(r.is_null(), "本物の StrChrW なら見つからず 0");
    assert_eq!(fallback_calls(), calls, "代わりが呼ばれた");
    assert_eq!(buf, [0u8; 64]);
}
