//! 致命的な例外を本ログへ残す（ベクタ化した例外ハンドラ）。th09.exeはトップレベルのフィルタを自分で差し替えうる。そのためVEHで見る。
//! 1次例外を見る。そのためゲームやOSが自分で受けて続ける例外も行になる（行の文は「見た」とだけ書く。落ちたとは限らない）。
//! 同じ（コード, 場所）の例外は最初の1回だけ行にし、繰り返しは数えるだけ。別の（コード, 場所）は32件まで行にする。
//! ハンドラはヒープを確保せずロックも取らない。いつも`EXCEPTION_CONTINUE_SEARCH`を返す（例外の処理を変えない）。
//! 致命的でないコードは比べるだけで戻るので普段の経路には何も足さない。
//!
//! ハンドラは外さない。登録する前にDLLを固定（PIN）するのでプロセスの終わり（DLL_PROCESS_DETACHの後も）までハンドラのコードは写像されたままになる。
//! 使うのはアトミック変数と、複製してどこでも閉じないログのハンドルだけ。

mod line;

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::fs::File;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::windows::io::{FromRawHandle, IntoRawHandle};

use windows_sys::Wdk::Storage::FileSystem::NtQueryVirtualMemory;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW,
};
use windows_sys::Win32::System::Memory::{MEM_IMAGE, MEMORY_BASIC_INFORMATION, VirtualQuery};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThreadId};

use line::{
    ACCESS_VIOLATION, IN_PAGE_ERROR, Kinds, MAX_LINES, Note, Place, STACK_OVERFLOW, Seen,
    fatal_name, file_name_utf8, format_seen,
};

/// ハンドラが書くログのハンドル（本ログの複製）。NULLの間は書かない。
static LOG: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
/// 致命的なコードの例外を見た回数（繰り返しも含む）。
static SEEN: AtomicU32 = AtomicU32::new(0);
/// 行にした（コード, 場所）のペア。
static KINDS: Kinds = Kinds::new();
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// 本ログ`log`の複製へ書くハンドラを登録する（プロセスで1回）。ゲームのスレッドでは呼ばない。
pub fn install(log: &File) -> Result<(), String> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err("既に登録済み".to_string());
    }
    // 本ログの書き手は自分のスレッドが終わると閉じる。
    // そのためハンドラには閉じない複製を持たせる。
    // 同じファイルオブジェクトなので書く位置は通常の書き手と共有され、上書きし合わない。
    let dup = log
        .try_clone()
        .map_err(|e| format!("ログのハンドルが複製不可: {e}"))?;
    let mut pinned: HMODULE = core::ptr::null_mut();
    // SAFETY: フラグは「アドレスから取得する」＋「固定する」。第2引数はこのDLLの中の関数のアドレスを指す疑似ポインタ。中身は読まれない。
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            on_exception as *const () as *const u16,
            &mut pinned,
        )
    };
    if ok == 0 {
        return Err("DLL 固定不能（外さないハンドラを登録できません）".to_string());
    }
    LOG.store(dup.into_raw_handle(), Ordering::Release);
    // SAFETY: 渡す関数はPVECTORED_EXCEPTION_HANDLERの形。上で固定したDLLの中にあり、プロセスの終わりまで呼べる。
    let handle = unsafe { AddVectoredExceptionHandler(0, Some(on_exception)) };
    if handle.is_null() {
        return Err("AddVectoredExceptionHandler が失敗しました".to_string());
    }
    Ok(())
}

/// 致命的なコードの例外を見た回数（行に書かなかった分も含む）。
pub fn seen_count() -> u32 {
    SEEN.load(Ordering::Relaxed)
}

/// 終了時に本ログへ足す1行。致命的なコードの例外を1回も見ていなければNone。
pub fn summary_line() -> Option<String> {
    let seen = seen_count();
    if seen == 0 {
        return None;
    }
    let (lines, repeats, dropped) = (KINDS.lines(), KINDS.repeats(), KINDS.dropped());
    let mut text = format!(
        "例外: ここまでに致命的なコードの例外を {seen} 回見ました（行にしたのは別々の {lines} 件。同じ例外（コードと場所が同じ）の繰り返し {repeats} 回は行にしていません"
    );
    if dropped > 0 {
        text.push_str(&format!(
            "。{MAX_LINES} 件の上限を超えて行にしなかった別の例外 {dropped} 回"
        ));
    }
    text.push_str("）。");
    Some(text)
}

unsafe extern "system" fn on_exception(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: OSが渡すEXCEPTION_POINTERSとその指すEXCEPTION_RECORDはハンドラの間有効。NULLなら見ない。
    let rec = unsafe { info.as_ref().and_then(|p| p.ExceptionRecord.as_ref()) };
    if let Some(rec) = rec {
        let n = rec.NumberParameters as usize;
        let params = rec.ExceptionInformation.get(..n).unwrap_or(&[]);
        observe(
            rec.ExceptionCode as u32,
            rec.ExceptionAddress as usize as u32,
            params,
        );
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// 数える。初めての（コード, 場所）なら1行書く。大きなバッファは下の関数の中に確保し、ここのフレームは小さく保つ。
fn observe(code: u32, addr: u32, params: &[usize]) {
    if fatal_name(code).is_none() {
        return;
    }
    SEEN.fetch_add(1, Ordering::Relaxed);
    let Note::Write(nth) = KINDS.note(code, addr) else {
        return;
    };
    let log = LOG.load(Ordering::Acquire);
    if log.is_null() {
        return;
    }
    if code == STACK_OVERFLOW {
        write_small(log, nth, code, addr);
    } else {
        write_full(log, nth, code, addr, params);
    }
}

/// スタックオーバーフロー用。残りのスタックが少ないので小さなバッファだけを使い、モジュールも時刻も取得しない。
#[inline(never)]
fn write_small(log: *mut c_void, nth: u32, code: u32, addr: u32) {
    let mut buf = [0u8; 192];
    let n = format_seen(
        &mut buf,
        &Seen {
            nth,
            code,
            addr,
            thread: current_thread_id(),
            time: None,
            place: Place::NotLooked,
            access: None,
        },
    );
    write_raw(log, buf.get(..n).unwrap_or(&[]));
}

#[inline(never)]
fn write_full(log: *mut c_void, nth: u32, code: u32, addr: u32, params: &[usize]) {
    let mut name = [0u8; 96];
    let place = module_at(addr, &mut name);
    let access = match (code, params) {
        (ACCESS_VIOLATION | IN_PAGE_ERROR, [kind, target, ..]) => Some((*kind, *target as u32)),
        _ => None,
    };
    let mut buf = [0u8; 512];
    let n = format_seen(
        &mut buf,
        &Seen {
            nth,
            code,
            addr,
            thread: current_thread_id(),
            time: Some(crate::win::local_time()),
            place,
            access,
        },
    );
    write_raw(log, buf.get(..n).unwrap_or(&[]));
}

/// `NtQueryVirtualMemory`の`MemoryMappedFilenameInformation`の出力。先頭がUNICODE_STRING。文字列はその直後に続く。
#[repr(C)]
struct SectionName {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
    path: [u16; 300],
}

/// `MEMORY_INFORMATION_CLASS::MemoryMappedFilenameInformation`（winternl.h / ntifs.hの列挙の3番目）。
const MEMORY_MAPPED_FILENAME_INFORMATION: i32 = 2;

/// `addr`を含むモジュールのファイル名とRVA。ヒープもローダーロックも使わない（呼び出し側のバッファに書く）。
/// 先頭は`VirtualQuery`の`AllocationBase`（イメージの写像の先頭）。名前は`NtQueryVirtualMemory`が返す写像のパスの最後の部分。
/// `GetModuleFileNameW`はローダーロックを取りうるので使わない（ほかのスレッドがロックを持ったままこのスレッドを待っていると止まる）。
/// 名前が取得できなければ`?`。イメージの外なら取得できなかった扱い。
fn module_at<'a>(addr: u32, name: &'a mut [u8; 96]) -> Place<'a> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: アドレスの領域を調べるだけ（`addr`を読み書きしない）。`info`は書き込み可能なMEMORY_BASIC_INFORMATION。その大きさを渡す。
    let got = unsafe {
        VirtualQuery(
            addr as usize as *const c_void,
            &mut info,
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if got == 0 || info.Type != MEM_IMAGE || info.AllocationBase.is_null() {
        return Place::Unknown;
    }
    let rva = addr.wrapping_sub(info.AllocationBase as usize as u32);
    let mut section = SectionName {
        length: 0,
        maximum_length: 0,
        buffer: core::ptr::null_mut(),
        path: [0; 300],
    };
    // SAFETY: 自分のプロセス（疑似ハンドル）の`addr`の写像のパスを`section`に書かせるだけ。渡す大きさは`section`の大きさそのもの。
    let status = unsafe {
        NtQueryVirtualMemory(
            GetCurrentProcess(),
            addr as usize as *const c_void,
            MEMORY_MAPPED_FILENAME_INFORMATION,
            (&mut section as *mut SectionName).cast(),
            core::mem::size_of::<SectionName>(),
            core::ptr::null_mut(),
        )
    };
    let chars = usize::from(section.length) / 2;
    let len = if status >= 0 {
        file_name_utf8(section.path.get(..chars).unwrap_or(&[]), name)
    } else {
        0
    };
    if len == 0 {
        return match name.first_mut() {
            Some(c) => {
                *c = b'?';
                Place::Module(name.get(..1).unwrap_or(&[]), rva)
            }
            None => Place::Unknown,
        };
    }
    Place::Module(name.get(..len).unwrap_or(&[]), rva)
}

fn current_thread_id() -> u32 {
    // SAFETY: 引数の無い呼び出しで、失敗しない。
    unsafe { GetCurrentThreadId() }
}

fn write_raw(log: *mut c_void, bytes: &[u8]) {
    // SAFETY: `log`は`install`で複製した、どこでも閉じないハンドル。`ManuallyDrop`なのでここでも閉じない。
    // 書き込みは1回のWriteFile相当。ヒープもロックも使わない。
    let mut f = ManuallyDrop::new(unsafe { File::from_raw_handle(log) });
    let _ = f.write(bytes);
}

#[cfg(test)]
mod tests {
    //! 本物のVEHを検査のプロセス自身に登録する。致命的なコードは`RaiseException`で投げる。検査の後ろのハンドラが続行させる。
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        RaiseException, RemoveVectoredExceptionHandler,
    };

    /// 検査が投げた例外の印（最後の引数）。ほかの例外は続行させない。
    const MAGIC: usize = 0x4869_6741;
    const NOT_FATAL: u32 = 0xE048_4701;

    unsafe extern "system" fn continue_ours(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: OSが渡す、ハンドラの間有効な構造体。
        let Some(rec) = (unsafe { info.as_ref().and_then(|p| p.ExceptionRecord.as_ref()) }) else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        let n = rec.NumberParameters as usize;
        if n > 0 && rec.ExceptionInformation.get(n - 1) == Some(&MAGIC) {
            -1 // EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }

    fn raise(code: u32, args: &[usize]) {
        // SAFETY: 引数は呼び出しの間生きている配列。印つきの例外は`continue_ours`が続行させるのでここへ戻る。
        unsafe { RaiseException(code, 0, args.len() as u32, args.as_ptr()) };
    }

    fn lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_handler_writes_each_distinct_place_once_and_stops_after_thirty_two_lines() {
        let dir = std::env::temp_dir().join(format!("higanr_crash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("veh.log");
        let file = File::create(&path).unwrap();
        install(&file).unwrap();
        assert!(install(&file).is_err(), "2 回目は入れない");
        // SAFETY: 渡す関数はPVECTORED_EXCEPTION_HANDLERの形で検査のバイナリの中にある。後ろへ足すので上のハンドラが先に見る。
        let cont = unsafe { AddVectoredExceptionHandler(0, Some(continue_ours)) };
        assert!(!cont.is_null());

        raise(NOT_FATAL, &[MAGIC]);
        assert_eq!(seen_count(), 0);
        assert!(lines(&path).is_empty());
        assert_eq!(summary_line(), None);

        // 同じ場所から3回投げても行は1本で、残りは繰り返しとして数える。
        let site = |i: usize| raise(ACCESS_VIOLATION, &[1, 0x10 + i, MAGIC]);
        for i in 0..3 {
            site(i);
        }
        assert_eq!(seen_count(), 3);
        let got = lines(&path);
        assert_eq!(got.len(), 1, "{got:?}");
        let l = &got[0];
        assert!(
            l.starts_with("例外を見ました（落ちたとは限らない・1 件目）: "),
            "{l}"
        );
        assert!(
            l.contains("0xC0000005 アクセス違反（0x00000010 の書き込み）"),
            "{l}"
        );
        // RaiseExceptionの場所はKernelBase.dll（古いWindowsではkernel32.dll）の中。
        assert!(l.to_ascii_lowercase().contains(".dll+0x"), "{l}");
        // 場所のアドレスからRVAを引いた値はモジュールの先頭（64 KB境界）。
        let hex = |s: &str| u32::from_str_radix(&s[2..10], 16).unwrap();
        let at = l.find("場所 0x").unwrap() + "場所 ".len();
        let addr = hex(&l[at..]);
        let rva = hex(&l[l.find("+0x").unwrap() + 1..]);
        assert!(rva < addr && (addr - rva) % 0x1_0000 == 0, "{l}");
        assert!(
            l.ends_with(&format!("スレッド {}", current_thread_id())),
            "{l}"
        );

        // スタックオーバーフローの経路は本物を発生させずに直に通す。
        observe(STACK_OVERFLOW, 0x0040_1000, &[]);
        let l = &lines(&path)[1];
        assert!(
            l.starts_with("例外を見ました（落ちたとは限らない・2 件目）")
                && l.contains(
                    "0xC00000FD スタックオーバーフロー / 場所 0x00401000（モジュールは引かない）"
                ),
            "{l}"
        );

        // 別のコードは別のペア（場所は同じRaiseExceptionの中）。同じコードの繰り返しは行にならない。
        for _ in 0..8 {
            raise(0xC000_001D, &[MAGIC]);
        }
        assert_eq!(seen_count(), 3 + 1 + 8);
        let got = lines(&path);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[2].contains("不正な命令"), "{}", got[2]);

        // 場所が違えば別のペア。32件目で打ち止めになり、以降の別のペアは数えるだけ。
        for i in 0..40u32 {
            observe(0xC000_0094, 0x0050_0000 + i, &[]);
        }
        let got = lines(&path);
        assert_eq!(got.len(), MAX_LINES as usize, "{got:?}");
        assert!(
            got[31].contains("32 件目。以降の別の例外は数えるだけ"),
            "{}",
            got[31]
        );
        // 上限の後でも行にしたペアの繰り返しは数える。
        site(9);
        assert_eq!(seen_count(), 3 + 1 + 8 + 40 + 1);
        assert_eq!(lines(&path).len(), MAX_LINES as usize);
        let sum = summary_line().unwrap();
        assert!(
            sum.contains("別々の 32 件")
                && sum.contains("繰り返し 10 回")
                && sum.contains("32 件の上限を超えて行にしなかった別の例外 11 回"),
            "{sum}"
        );

        // SAFETY: 上で登録した検査のハンドラのハンドル。
        unsafe { RemoveVectoredExceptionHandler(cont) };
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_module_of_an_address_comes_from_the_mapping_without_the_loader_lock() {
        let mut name = [0u8; 96];
        let here = module_of_this_test as *const () as usize as u32;
        let Place::Module(n, rva) = module_at(here, &mut name) else {
            panic!("この検査の実行ファイルのイメージが引けない");
        };
        let exe = std::env::current_exe().unwrap();
        let want = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(String::from_utf8_lossy(n), want);
        // SAFETY: 引数NULLは自分の実行ファイルの読み込み先頭を返すだけ。
        let base = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(core::ptr::null())
        } as usize as u32;
        assert_eq!(rva, here - base);

        let mut name = [0u8; 96];
        let k32 =
            windows_sys::Win32::System::Threading::GetCurrentThreadId as *const () as usize as u32;
        let Place::Module(n, _) = module_at(k32, &mut name) else {
            panic!("システムの DLL が引けない");
        };
        assert!(
            String::from_utf8_lossy(n)
                .to_ascii_lowercase()
                .ends_with(".dll"),
            "{}",
            String::from_utf8_lossy(n)
        );

        let on_stack = 0u32;
        let mut name = [0u8; 96];
        assert_eq!(
            module_at(&on_stack as *const u32 as usize as u32, &mut name),
            Place::Unknown
        );
        assert_eq!(module_at(0x10, &mut name), Place::Unknown);
    }

    fn module_of_this_test() {}
}
