//! フックを書く Win32 の層（unsafe はここと `stub.rs`・`win.rs` に閉じ込める。1 つずつ `// SAFETY:`）。
//!
//! `hook::HookEnv` の本物: 自分のプロセスのスレッドの一覧（ToolHelp）、停止と再開、EIP の読み取り（`GetThreadContext`）、
//! コードの読み直し（ヒープを使わない）、保護の一時的な変更と rel32 の書き込みと命令キャッシュの流し、時計。手順と判定
//! （どの順に呼び、何があれば書かないか）は `hook.rs` にだけ書く。
//!
//! **書く先は自分のプロセスのコードだけ**（他のプロセスのメモリに書く API は使わない）。書くのは `hook.rs` が期待バイト列の
//! 照合を通した 4 バイトだけ。ゲームのファイル・設定には触らない。

#[cfg(not(target_arch = "x86"))]
compile_error!("hook_win.rs は 32 ビット（i686）専用（EIP・32 ビットのアドレスを前提にしている）");

use core::ffi::c_void;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_CONTROL_X86, FlushInstructionCache, GetThreadContext,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread,
    SuspendThread, THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME,
};

use crate::detour::DetourEnv;
use crate::draw;
use crate::hook::{HookEnv, ThreadRef, WriteDone};
use crate::stub;
use crate::ticks::CopyRow;
use crate::win::read_own_into;

/// `THREADENTRY32` のうち、所有プロセスの ID まで（`th32OwnerProcessID` の終わり）が埋まっているかの境目。ToolHelp は `dwSize` に
/// 書いた大きさを返すので、足りなければその行を読まない。
const THREADENTRY_OWNER_END: u32 = 16;

pub struct RealHook {
    pid: u32,
    me: u32,
}

impl RealHook {
    /// 呼んだスレッド（読み取りのスレッド）を「自分」とする。止めるのは自分以外の全部。
    pub fn new() -> Self {
        // SAFETY: どちらも引数の無い問い合わせ。
        let (pid, me) = unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
        RealHook { pid, me }
    }
}

fn last_error() -> u32 {
    // SAFETY: 引数の無い呼び出し。直前の Win32 の呼び出しの結果を指す。
    unsafe { GetLastError() }
}

fn handle(t: ThreadRef) -> HANDLE {
    t.0 as HANDLE
}

impl HookEnv for RealHook {
    fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
        // SAFETY: 引数は旗とプロセス ID（スレッドの一覧ではプロセス ID は見られず、全部のスレッドが返る）。
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return Err(last_error());
        }
        let mut out: Vec<ThreadRef> = Vec::new();
        let mut failure: Option<u32> = None;
        let mut te = THREADENTRY32 {
            dwSize: core::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: `snap` はいま作った一覧のハンドル。`te` は `dwSize` を埋めた THREADENTRY32 で、書ける。
        let mut more = unsafe { Thread32First(snap, &mut te) } != 0;
        while more {
            if te.dwSize >= THREADENTRY_OWNER_END
                && te.th32OwnerProcessID == self.pid
                && te.th32ThreadID != self.me
            {
                // SAFETY: 権利の旗とスレッド ID を渡すだけ（継承しない）。返ったハンドルは `close` で閉じる。
                let h = unsafe {
                    OpenThread(
                        THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT,
                        0,
                        te.th32ThreadID,
                    )
                };
                if h.is_null() {
                    let code = last_error();
                    // 一覧を取った後に終わったスレッドは ERROR_INVALID_PARAMETER で開けない。もう居ないので
                    // 止めなくてよい。それ以外（権限など）で開けないスレッドは止められないので、書かない。
                    if code != ERROR_INVALID_PARAMETER {
                        failure = Some(code);
                        break;
                    }
                } else {
                    out.push(ThreadRef(h as usize));
                }
            }
            te.dwSize = core::mem::size_of::<THREADENTRY32>() as u32;
            // SAFETY: 上と同じ。
            more = unsafe { Thread32Next(snap, &mut te) } != 0;
        }
        // SAFETY: `snap` はいま作って、まだ閉じていないハンドル。
        unsafe { CloseHandle(snap) };
        if let Some(code) = failure {
            for t in out {
                self.close(t);
            }
            return Err(code);
        }
        Ok(out)
    }

    fn suspend(&mut self, t: ThreadRef) -> bool {
        // SAFETY: `t` は `open_other_threads` が THREAD_SUSPEND_RESUME で開き、まだ閉じていない自分以外のハンドル。
        unsafe { SuspendThread(handle(t)) != u32::MAX }
    }

    fn resume(&mut self, t: ThreadRef) -> bool {
        // SAFETY: 同上。
        unsafe { ResumeThread(handle(t)) != u32::MAX }
    }

    fn close(&mut self, t: ThreadRef) {
        // SAFETY: `open_other_threads` が開いたハンドルを 1 回だけ閉じる。
        unsafe { CloseHandle(handle(t)) };
    }

    fn eip(&mut self, t: ThreadRef) -> Option<u32> {
        // 制御レジスタ（EIP・ESP など）だけを求める。x86 の CONTEXT は固定長でスタックに置く。
        let mut ctx = CONTEXT {
            ContextFlags: CONTEXT_CONTROL_X86,
            ..Default::default()
        };
        // SAFETY: `t` は THREAD_GET_CONTEXT で開いた、止めたスレッドのハンドル。`ctx` は書ける CONTEXT で、`ContextFlags` を
        // 埋めてある。32 ビットのプロセスから読むので WOW64 でも 32 ビットの文脈が返る。
        let ok = unsafe { GetThreadContext(handle(t), &mut ctx) } != 0;
        ok.then_some(ctx.Eip)
    }

    fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        read_own_into(addr, buf)
    }

    fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
        let at = addr as usize as *mut u8;
        let len = bytes.len();
        let mut old: u32 = 0;
        // SAFETY: アドレスと長さと新しい保護を渡し、元の保護を `old` に受け取るだけ。アドレスが確保されていなければ失敗して 0 が返る
        // （そのときは何も書かない）。範囲が頁の境をまたいでも全部の頁に効き、`old` は先頭の頁のもの（0x0043112E〜0x00431131 と
        // 検査の領域は、どちらも 1 頁の中）。
        let ok =
            unsafe { VirtualProtect(at as *const c_void, len, PAGE_EXECUTE_READWRITE, &mut old) };
        if ok == 0 {
            return Err(last_error());
        }
        for (i, &b) in bytes.iter().enumerate() {
            // SAFETY: 直前の VirtualProtect で `[addr, addr + len)` を書ける保護にした。呼び出し元（`hook::attempt_while_stopped`）は、
            // 自分以外のスレッドを全部止め、どの EIP もこの命令の 5 バイトに無いことと、止めたまま読み直した 5 バイトが期待どおり
            // であることを確かめてから呼ぶ。書くのは call の rel32 の 4 バイトだけで、オペコードは変えない。
            unsafe { core::ptr::write_volatile(at.add(i), b) };
        }
        let mut dummy: u32 = 0;
        // SAFETY: 上と同じ範囲の保護を、受け取っておいた元の値に戻す。
        let restored = unsafe { VirtualProtect(at as *const c_void, len, old, &mut dummy) } != 0;
        // SAFETY: 自分のプロセスの擬似ハンドルと、いま書いた範囲を渡すだけ。
        let flushed =
            unsafe { FlushInstructionCache(GetCurrentProcess(), at as *const c_void, len) } != 0;
        Ok(WriteDone {
            protect_restored: restored,
            flushed,
        })
    }

    fn set_original_target(&mut self, target: u32) {
        stub::set_original_target(target);
    }

    fn set_window(&mut self, addr: u32) {
        stub::set_window(addr);
    }

    fn set_copy_table(&mut self, rows: &[CopyRow]) {
        // 枠の側の確かめに通らなければ stub が件数 0 のまま置く。どちらになったかは計測の始まりで読み返してログに書く
        // （`RecordEnv::copy_table`）。
        let _ = stub::set_copy_table(rows);
    }

    fn set_render_only_exit(&mut self, addr: u32) {
        stub::set_render_only_exit(addr);
    }

    fn entry_address(&self) -> u32 {
        stub::entry_address()
    }

    fn match_entry_address(&self) -> u32 {
        stub::match_entry_address()
    }

    fn set_match_original(&mut self, target: u32) {
        stub::set_match_original(target);
    }

    fn sleep_ms(&mut self, ms: u32) {
        std::thread::sleep(Duration::from_millis(ms as u64));
    }

    fn now_ms(&self) -> u64 {
        // SAFETY: 引数の無い問い合わせ。
        unsafe { GetTickCount64() }
    }

    fn counter(&self) -> u32 {
        stub::calls()
    }
}

impl DetourEnv for RealHook {
    fn alloc_trampoline(&mut self, len: usize) -> Result<u32, u32> {
        // SAFETY: アドレスは OS に任せ（NULL）、長さぶんを予約 ＋ コミットして読み書きできる保護にする。失敗すれば NULL が返る。
        // トランポリンはこの DLL が生きている間 解放しないので、ゲームのスレッドがいつ飛んできても頁はある。
        let p = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            Err(last_error())
        } else {
            Ok(p as usize as u32)
        }
    }

    fn write_and_seal_trampoline(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32> {
        let at = addr as usize as *mut u8;
        for (i, &b) in bytes.iter().enumerate() {
            // SAFETY: `addr` はいま `alloc_trampoline` が確保した読み書きできる頁の先頭で、`bytes` はその長さに収まる
            // （確保した len は `盗んだ長さ + 5`）。
            unsafe { core::ptr::write_volatile(at.add(i), b) };
        }
        let mut old: u32 = 0;
        // SAFETY: いま確保した頁を、盗んだ命令を実行できる保護（実行 ＋ 読み）にする。
        let ok = unsafe {
            VirtualProtect(
                at as *const c_void,
                bytes.len(),
                PAGE_EXECUTE_READ,
                &mut old,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        // SAFETY: いま書いたトランポリンの範囲の命令キャッシュを流す（書いた後に実行するため）。
        unsafe { FlushInstructionCache(GetCurrentProcess(), at as *const c_void, bytes.len()) };
        Ok(())
    }

    fn free_trampoline(&mut self, addr: u32) {
        // SAFETY: `alloc_trampoline` が確保した頁を 1 回だけ返す。この頁へ飛ぶ JMP はまだ書いていない。
        unsafe { VirtualFree(addr as usize as *mut c_void, 0, MEM_RELEASE) };
    }

    fn set_trampoline(&mut self, addr: u32) {
        draw::set_draw_trampoline(addr);
    }

    fn detour_entry_address(&self) -> u32 {
        draw::draw_entry_address()
    }
}

#[cfg(test)]
mod tests {
    //! 本物の Win32 を、検査のプロセス自身に当てる（花映塚は使わない）。自分で確保した実行できる領域に `E8 rel32; C3` を置き、
    //! 本物の入口と本物のスレッドの停止で差し替える。入口は esi（on_tick の this）を読むので、差し替えた後の領域は
    //! `stub::testkit::call_with_regs` で呼ぶ。
    use super::*;
    use crate::hook::{Outcome, Refusal, install_call_hook};
    use crate::sites::{SiteDef, Use};
    use crate::stub::testkit::{
        COPY_SRC, REAL_HOOK_TESTS, TARGET_HITS, call_with_regs, copy_src_addr, fake_this_addr,
        fake_window_addr, seen_target_addr, set_fake,
    };
    use crate::ticks::SlotRead;
    use core::sync::atomic::{AtomicU32, Ordering};
    use higanr_core::patch;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ,
        PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualQuery,
    };

    static OTHER_HITS: AtomicU32 = AtomicU32::new(0);

    extern "C" fn other_target() {
        OTHER_HITS.fetch_add(1, Ordering::SeqCst);
    }

    fn fn_addr(f: extern "C" fn()) -> u32 {
        f as *const () as usize as u32
    }

    /// 実行できる 1 頁。`at` に `E8 rel32(→ to); C3` を置き、保護を PAGE_EXECUTE_READ にする。
    struct Region {
        base: *mut c_void,
        at: u32,
    }

    impl Region {
        fn with_call_to(to: u32) -> Self {
            // SAFETY: 新しく 1 頁を確保する（アドレスは OS に任せる）。失敗は下で見る。
            let base = unsafe {
                VirtualAlloc(
                    core::ptr::null(),
                    4096,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            assert!(!base.is_null(), "VirtualAlloc");
            let at = base as usize as u32 + 0x40;
            let mut code = [0u8; 6];
            code[..5].copy_from_slice(&patch::call_rel32(at, to));
            code[5] = 0xC3;
            for (i, &b) in code.iter().enumerate() {
                // SAFETY: いま確保した読み書きのできる頁の中（0x40〜0x45）。
                unsafe { core::ptr::write_volatile((at as usize + i) as *mut u8, b) };
            }
            let mut old = 0;
            // SAFETY: いま確保した頁の保護を変えるだけ。
            let ok = unsafe { VirtualProtect(base, 4096, PAGE_EXECUTE_READ, &mut old) };
            assert_ne!(ok, 0, "VirtualProtect");
            Region { base, at }
        }

        fn call(&self) {
            // SAFETY: `at` は PAGE_EXECUTE_READ の頁に置いた `call rel32; ret` で、飛び先は引数も戻り値も無い `extern "C" fn()`。
            // スタックは call / ret で釣り合う。入口へ差し替えた後の領域はここで呼ばない（入口は esi を on_tick の this として読むので、`call_with_regs` で呼ぶ）。
            let f: extern "C" fn() = unsafe { core::mem::transmute(self.at as usize) };
            f();
        }

        fn bytes(&self) -> [u8; 5] {
            let mut b = [0u8; 5];
            assert!(read_own_into(self.at, &mut b));
            b
        }

        fn protect(&self) -> u32 {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: 情報を `mbi` に書くだけ。
            let n = unsafe {
                VirtualQuery(
                    self.at as usize as *const c_void,
                    &mut mbi,
                    core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            assert_ne!(n, 0);
            mbi.Protect
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: `with_call_to` で確保した頁を 1 回だけ返す。この頁へ飛ぶ call は検査の中にしか無い。
            unsafe { VirtualFree(self.base, 0, MEM_RELEASE) };
        }
    }

    fn row(r: &Region, expected: [u8; 5], original_target: u32) -> SiteDef {
        SiteDef {
            name: "selftest_call",
            addr: r.at,
            expected: Box::leak(Box::new(expected)),
            kind: Use::CallRel32 { original_target },
            source: "検査の領域",
        }
    }

    #[test]
    fn real_install_redirects_the_call_through_the_entry() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let target = seen_target_addr();
        let region = Region::with_call_to(target);
        let before = region.bytes();

        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        call_with_regs(region.at);
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0 + 1,
            "差し替える前"
        );

        let def = row(&region, before, target);
        let mut env = RealHook::new();
        let calls0 = stub::calls();
        let seq0 = stub::next_seq();
        let copies = [CopyRow {
            src: copy_src_addr() + 6,
            len: 8,
            dst: 64,
        }];
        COPY_SRC.0[1].store(0x4433_2211, Ordering::SeqCst);
        COPY_SRC.0[2].store(0x8877_6655, Ordering::SeqCst);
        COPY_SRC.0[3].store(0xCCBB_AA99, Ordering::SeqCst);
        let r = install_call_hook(&mut env, &def, fake_window_addr(), &copies, 0);
        assert_eq!(
            stub::WINDOW.load(Ordering::SeqCst),
            fake_window_addr(),
            "窓のアドレス"
        );
        assert_eq!(stub::copy_table(), copies, "コピーの表");
        assert!(r.written(), "{r:?}");
        assert_eq!(
            r.outcome,
            Outcome::Written(WriteDone {
                protect_restored: true,
                flushed: true
            })
        );
        assert_eq!(r.tries.suspended_total, r.tries.resumed_total);
        assert_eq!(r.tries.resume_failures, 0);
        assert!(r.tries.threads >= 1, "止めたスレッドが 0 本（一覧が空）");
        assert_eq!(stub::calls(), calls0, "書いただけで入口を通った");

        let after = region.bytes();
        assert_eq!(after[0], 0xE8, "オペコードを変えた");
        assert_eq!(
            patch::call_target(region.at, &after),
            Some(stub::entry_address())
        );
        assert_eq!(r.readback, Some(crate::hook::CodeBytes::from(after)));
        assert_eq!(region.protect(), PAGE_EXECUTE_READ, "保護が戻っていない");

        set_fake(321, 4, 5, 7.25, 7.5);
        call_with_regs(region.at);
        assert_eq!(stub::calls(), calls0 + 1, "入口を通っていない");
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0 + 2,
            "元の飛び先へ飛んでいない"
        );
        call_with_regs(region.at);
        assert_eq!(stub::calls(), calls0 + 2);
        assert_eq!(TARGET_HITS.load(Ordering::SeqCst), hits0 + 3);
        assert_eq!(stub::next_seq(), seq0.wrapping_add(2), "記録を書いていない");
        match stub::read_slot(seq0) {
            SlotRead::Record(s) => {
                let rec = s.rec;
                assert_eq!((rec.sup_frame, rec.scene_cur, rec.scene_req), (321, 4, 5));
                assert_eq!((rec.game_now(), rec.game_deadline()), (7.25, 7.5));
                assert_eq!(rec.esi, fake_this_addr());
                assert_eq!(s.read(64, 4), Some(0x6655_4433), "写し");
                assert_eq!(s.read(68, 4), Some(0xAA99_8877), "写し");
            }
            other => panic!("記録が採れない: {other:?}"),
        }
        assert!(stub::set_copy_table(&[]));
    }

    #[test]
    fn real_install_writes_nothing_when_bytes_differ() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let region = Region::with_call_to(fn_addr(other_target));
        let before = region.bytes();
        let expected = patch::call_rel32(region.at, seen_target_addr());
        assert_ne!(before, expected);
        let def = row(&region, expected, seen_target_addr());

        let orig_before = stub::ORIGINAL_TARGET.load(Ordering::SeqCst);
        let window_before = stub::WINDOW.load(Ordering::SeqCst);
        let copies_before = stub::copy_table();
        let mut env = RealHook::new();
        let other_copies = [CopyRow {
            src: copy_src_addr(),
            len: 4,
            dst: 200,
        }];
        assert_ne!(copies_before, other_copies);
        let r = install_call_hook(&mut env, &def, window_before ^ 0x10, &other_copies, 0);
        assert_eq!(
            stub::copy_table(),
            copies_before,
            "書かないのにコピーの表を置き換えた"
        );
        assert_eq!(
            r.outcome,
            Outcome::Refused(Refusal::Mismatch(crate::hook::CodeBytes::from(before)))
        );
        assert_eq!(r.tries.attempts, 0);
        assert_eq!(region.bytes(), before);
        assert_eq!(region.protect(), PAGE_EXECUTE_READ, "保護を変えた");
        assert_eq!(stub::ORIGINAL_TARGET.load(Ordering::SeqCst), orig_before);
        assert_eq!(
            stub::WINDOW.load(Ordering::SeqCst),
            window_before,
            "書かないのに窓のアドレスを置き換えた"
        );

        let hits0 = OTHER_HITS.load(Ordering::SeqCst);
        region.call();
        assert_eq!(OTHER_HITS.load(Ordering::SeqCst), hits0 + 1);
    }

    #[test]
    fn real_thread_list_excludes_self_and_can_suspend_others() {
        use core::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use windows_sys::Win32::Foundation::CompareObjectHandles;
        use windows_sys::Win32::System::Threading::THREAD_QUERY_LIMITED_INFORMATION;

        /// 止めたハンドルを、抜け方に関わらず再開する。
        struct ResumeGuard(ThreadRef, bool);
        impl Drop for ResumeGuard {
            fn drop(&mut self) {
                if self.1 {
                    // SAFETY: 検査が開いて止めたハンドル。まだ閉じていない。
                    unsafe { ResumeThread(handle(self.0)) };
                }
            }
        }
        /// worker の spin を止める（assert が落ちても worker を残さない）。
        struct StopGuard(Arc<AtomicBool>);
        impl Drop for StopGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_guard = StopGuard(stop.clone());
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let stop_w = stop.clone();
        let worker = std::thread::spawn(move || {
            // SAFETY: 引数の無い問い合わせ。
            tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            while !stop_w.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        });
        let worker_id = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("worker の id");

        let mut env = RealHook::new();
        let threads = env.open_other_threads().expect("一覧");
        // 一覧のハンドルは ID を聞く権利（QUERY）を持たないので、検査が自分で worker と自分を開き、`CompareObjectHandles` で
        // 突き合わせる。
        // SAFETY: スレッド ID を渡すだけ。開いたハンドルはすぐ下で閉じる。
        let (worker_h, me_h) = unsafe {
            (
                OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, worker_id),
                OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()),
            )
        };
        assert!(!worker_h.is_null() && !me_h.is_null(), "検査用のハンドル");
        // SAFETY: どれも開いたままのハンドルで、比べるだけ。
        let same = |a: HANDLE, b: HANDLE| unsafe { CompareObjectHandles(a, b) } != 0;
        let pos = threads.iter().position(|&t| same(handle(t), worker_h));
        let me_listed = threads.iter().any(|&t| same(handle(t), me_h));
        let listed = threads.len();
        // SAFETY: 上で開いた、まだ閉じていないハンドルを 1 回ずつ閉じる。
        unsafe {
            CloseHandle(worker_h);
            CloseHandle(me_h);
        }

        let mut suspended = false;
        let mut eip = None;
        let mut resumed = false;
        if let Some(i) = pos {
            let mut guard = ResumeGuard(threads[i], false);
            suspended = env.suspend(threads[i]);
            guard.1 = suspended;
            if suspended {
                eip = env.eip(threads[i]);
                guard.1 = false;
                resumed = env.resume(threads[i]);
            }
        }
        for t in threads {
            env.close(t);
        }
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap();

        assert!(listed > 0, "一覧が空");
        assert!(!me_listed, "一覧に自分が入っている");
        assert!(pos.is_some(), "一覧に worker が入っていない（{listed} 本）");
        assert!(suspended, "worker を止められない");
        assert!(eip.is_some(), "止めた worker の EIP");
        assert!(resumed, "worker を再開できない");
    }

    static ABS_VALUE: AtomicU32 = AtomicU32::new(0xDEAD_BEEF);

    #[test]
    fn real_install_detour_redirects_through_draw_entry() {
        use crate::detour::{DetourOutcome, install_detour};
        use crate::draw;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());

        let base = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        assert!(!base.is_null());
        let at = base as usize as u32 + 0x40;
        let abs = &ABS_VALUE as *const AtomicU32 as usize as u32;
        let mut code = [0u8; 6];
        code[0] = 0xA1;
        code[1..5].copy_from_slice(&abs.to_le_bytes());
        code[5] = 0xC3;
        for (i, &b) in code.iter().enumerate() {
            unsafe { core::ptr::write_volatile((at as usize + i) as *mut u8, b) };
        }
        let mut old = 0;
        assert_ne!(
            unsafe { VirtualProtect(base, 4096, PAGE_EXECUTE_READ, &mut old) },
            0
        );

        let f: extern "C" fn() -> u32 = unsafe { core::mem::transmute(at as usize) };
        assert_eq!(f(), 0xDEAD_BEEF, "detour の前");

        let expected: &'static [u8] =
            Box::leak(Box::new([code[0], code[1], code[2], code[3], code[4]]));
        let def = SiteDef {
            name: "selftest_detour",
            addr: at,
            expected,
            kind: Use::Detour { instr_lens: &[5] },
            source: "検査の領域",
        };
        let mut env = RealHook::new();
        let calls0 = draw::draw_calls();
        let seq0 = draw::draw_next_seq();
        let r = install_detour(&mut env, &def, &[]);
        assert_eq!(r.outcome, DetourOutcome::Written, "{r:?}");
        assert_eq!(
            draw::DRAW_TRAMPOLINE.load(Ordering::SeqCst),
            r.trampoline.unwrap(),
            "入口のトランポリンのアドレス"
        );
        let mut back = [0u8; 5];
        assert!(read_own_into(at, &mut back));
        assert_eq!(back[0], 0xE9);
        assert_eq!(
            patch::jmp_target(at, &back),
            Some(draw::draw_entry_address())
        );

        assert_eq!(f(), 0xDEAD_BEEF, "detour の後（戻り値が変わった）");
        assert_eq!(draw::draw_calls(), calls0 + 1, "描画の回数を数えていない");
        assert_eq!(
            draw::draw_next_seq(),
            seq0.wrapping_add(1),
            "描画のリングバッファに書いていない"
        );
        match draw::read_draw_slot(seq0) {
            crate::draw::DrawRead::Record(rec) => assert_ne!(rec.tsc, 0, "TSC を取っていない"),
            other => panic!("描画の記録が採れない: {other:?}"),
        }
        assert_eq!(f(), 0xDEAD_BEEF);
        assert_eq!(draw::draw_calls(), calls0 + 2);

        draw::set_draw_trampoline(0);
        unsafe {
            VirtualFree(
                r.trampoline.unwrap() as usize as *mut c_void,
                0,
                MEM_RELEASE,
            )
        };
        unsafe { VirtualFree(base, 0, MEM_RELEASE) };
    }
}
