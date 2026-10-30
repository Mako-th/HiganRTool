//! メインループのTestCooperativeLevel（`0x42E692`）の差し替え。
//! フルスクリーンで前面を失ってデバイスが失われている間、ゲームはメッセージが無ければTestCooperativeLevelを呼んですぐループの頭へ戻り、論理コア1つを回し続ける。
//! 差し替えた入口は同じ呼び出しをし、失敗なら10 ms寝てから同じ値を返す（vpatchの同じ地点と同じ仕組み。コードは自作）。
//! アドレスと期待バイト列は`th09_addr.rs`の「フルスクリーンの垂直同期」の節。
//! **時期**: 書くのはDllMainの中（`win::on_process_attach`）。ゲームの主スレッドはまだメインループへ来ていない。iniは読まない。
//! 11バイトを実バイトで照合し、1バイトでも違えば何も書かない。デバイスが正常なときの仕事は元と同じ（vtableの読みとTestCooperativeLevelの1回）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "lost_sleep_win.rs は 32 ビット（i686）専用（ゲームのアドレスと cdecl / stdcall の呼びを前提にしている）"
);

use core::sync::atomic::{AtomicBool, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use higanr_core::patch;

use crate::hook::HookEnv;
use crate::hook_win::RealHook;
use crate::sites::SiteDef;
use crate::th09_addr::{D3D8_TESTCOOP_SLOT, LOST_SLEEP_LEN, LOST_SLEEP_SITES};

/// デバイスが失われている間に1回寝る長さ（ms）。
pub const LOST_SLEEP_MS: u32 = 10;
/// D3DERR_DEVICELOST。デバイスのポインタが0のときに返す（ゲームはループの頭へ戻る）。
const DEVICELOST: i32 = 0x8876_0868_u32 as i32;

type DeviceFn = unsafe extern "system" fn(u32) -> i32;

/// 失われたときの行を出したかどうか（行は1起動に1回）。
static LOGGED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static TEST_LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
static TEST_CELL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(test)]
static TEST_SLEPT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn log(line: String) {
    let line = format!("デバイスの喪失: {line}");
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
    #[cfg(not(test))]
    crate::main_log::push(line);
}

fn log_detail(line: String) {
    let line = format!("デバイスの喪失: {line}");
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
    #[cfg(not(test))]
    crate::main_log::push_detail(line);
}

fn device_cell() -> u32 {
    #[cfg(test)]
    {
        TEST_CELL.load(Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        crate::th09_addr::D3D8_DEVICE_PTR
    }
}

fn sleep_lost() {
    #[cfg(test)]
    TEST_SLEPT.fetch_add(1, Ordering::SeqCst);
    #[cfg(not(test))]
    // SAFETY: 引数はmsの長さだけ。ゲームの主スレッドがデバイスの戻りを待つ間に寝る。
    unsafe {
        windows_sys::Win32::System::Threading::Sleep(LOST_SLEEP_MS)
    };
}

/// メインループのTestCooperativeLevelの呼び出しを自前の入口へ向ける。**DllMainから呼ぶ**（ローダーロックの下）。失敗してもロードは止めない。
pub fn install() {
    let site = LOST_SLEEP_SITES[0];
    let r = catch_unwind(AssertUnwindSafe(|| {
        // 他のスレッドを止めずに11バイトを書くので主スレッドがメインループへ来る前（デバイスがまだ無い）でなければ書かない。
        let mut cell = [0u8; 4];
        let mut env = RealHook::new();
        if !env.read_code(device_cell(), &mut cell) {
            return Err(format!("{:#010X} が読み取り不能", device_cell()));
        }
        if u32::from_le_bytes(cell) != 0 {
            return Err("デバイスが既に存在（ゲームの実行後の注入）".to_string());
        }
        install_at(&mut env, &site, entry_address())
    }));
    match r {
        Ok(Ok(s)) => log_detail(format!("設置: {s}")),
        Ok(Err(why)) => log(format!(
            "設置: 非差し替え（失われている間もゲームは寝ずに回る）: {why}"
        )),
        Err(_) => log("設置: パニックを受けました（書いたかどうかは分からない）。".to_string()),
    }
}

fn entry_address() -> u32 {
    lost_sleep_entry as *const () as usize as u32
}

/// 書く11バイト: `call entry` / `nop` × 6。`call`のrel32は`at + 5`からの相対。
fn patch_to(at: u32, entry: u32) -> [u8; LOST_SLEEP_LEN] {
    let c = patch::call_rel32(at, entry);
    [
        c[0], c[1], c[2], c[3], c[4], 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
    ]
}

fn install_at(env: &mut dyn HookEnv, site: &SiteDef, entry: u32) -> Result<String, String> {
    let mut seen = [0u8; LOST_SLEEP_LEN];
    if site.expected.len() != LOST_SLEEP_LEN || !env.read_code(site.addr, &mut seen) {
        return Err(format!(
            "{:#010X} から {} バイトの読み取り不能（th09.exe のないプロセスの可能性）",
            site.addr, LOST_SLEEP_LEN
        ));
    }
    patch::verify(&seen, site.expected).map_err(|e| {
        format!(
            "{:#010X} の命令が期待と不一致（{e:?}）。別のバージョンの th09.exe か、このフック地点を書き換えるツールがあります",
            site.addr
        )
    })?;
    let new = patch_to(site.addr, entry);
    let done = env.write_code(site.addr, &new).map_err(|code| {
        format!(
            "{:#010X} へ書き込み不能（GetLastError = {code}）",
            site.addr
        )
    })?;
    let mut back = [0u8; LOST_SLEEP_LEN];
    if !env.read_code(site.addr, &mut back) || back != new {
        return Err(format!(
            "{:#010X} へ書いた後の読み直しが不一致: {back:02X?}",
            site.addr
        ));
    }
    Ok(format!(
        "{:#010X} の TestCooperativeLevel の呼びを入口 {entry:#010X} への call に差し替えました（保護を戻せた: {}、命令キャッシュを流せた: {}）",
        site.addr, done.protect_restored, done.flushed
    ))
}

/// 失われたときの行を1回だけ頼む（ゲームの主スレッド。デバイスが失われている間はフレームが回らない）。パニックはここで受ける。
fn note_lost(hr: i32) {
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        log_detail(format!(
            "TestCooperativeLevel = {:#010X}。戻るまで {LOST_SLEEP_MS} ms ずつ寝ます（この行は 1 起動に 1 回）",
            hr as u32
        ))
    }));
}

/// # Safety
/// `addr`は読み取り可能な4バイト（デバイスのポインタの置き場・生きたデバイスとそのvtableの中だけを渡す）。
unsafe fn read_u32(addr: u32) -> u32 {
    // SAFETY: 呼び出し元の約束（読み取り可能な4バイト）。
    unsafe { core::ptr::read_volatile(addr as usize as *const u32) }
}

/// 差し替えた呼び出しの入口（cdecl・引数なし）。TestCooperativeLevelの戻り値をそのまま返す（ゲームは`0x42E69D test %eax,%eax`から続ける）。
/// ebx・esi・edi・ebpはcdeclの約束で保たれる。ecx・edxは`0x42E69D`以降で代入し直してから読まれる。
extern "C" fn lost_sleep_entry() -> i32 {
    // SAFETY: 置き場は`[0x4B3108]`（.data。読み取り可能）。メインループではゲームがデバイスを作った後。
    let dev = unsafe { read_u32(device_cell()) };
    let vtbl = if dev == 0 {
        0
    } else {
        // SAFETY: 0でない値はゲーム自身がTestCooperativeLevelの呼び出しに使うデバイス。先頭4バイトはvtableのアドレス。
        unsafe { read_u32(dev) }
    };
    let tcl = if vtbl == 0 {
        0
    } else {
        // SAFETY: 生きたデバイスのvtableの中のスロット（`+0x0C`）。
        unsafe { read_u32(vtbl.wrapping_add(D3D8_TESTCOOP_SLOT)) }
    };
    if tcl == 0 {
        sleep_lost();
        return DEVICELOST;
    }
    // SAFETY: `tcl`は0でないTestCooperativeLevel（元の呼び出し`0x0042E69A`と同じ）。stdcall・引数はthisだけ。
    let hr = unsafe { core::mem::transmute::<usize, DeviceFn>(tcl as usize)(dev) };
    if hr < 0 {
        note_lost(hr);
        sleep_lost();
    }
    hr
}

#[cfg(test)]
mod tests {
    //! 組み上がりの検査（花映塚は使わない）。偽のCOMに入口を向け、書き換えた機械語を実際に呼ぶ。
    use super::*;
    use crate::hook::tests::FakeHook;
    use crate::stub::testkit::{
        AFTER, PRE_ESP, REAL_HOOK_TESTS, REG_EBP, REG_EBX, REG_EDI, call_with_regs, fake_this_addr,
        load,
    };
    use core::sync::atomic::AtomicU32;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const DEVICENOTRESET: i32 = 0x8876_0869_u32 as i32;

    #[repr(C)]
    struct Obj(AtomicU32);
    static FAKE_DEVICE: Obj = Obj(AtomicU32::new(0));
    static DEV_VTBL: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
    static FAKE_DEVICE_PTR: AtomicU32 = AtomicU32::new(0);
    static TCL_HR: AtomicU32 = AtomicU32::new(0);
    static N_TCL: AtomicU32 = AtomicU32::new(0);
    static BAD_ARGS: AtomicU32 = AtomicU32::new(0);

    fn addr_of<T>(x: &'static T) -> u32 {
        x as *const T as usize as u32
    }

    extern "system" fn fake_tcl(this: u32) -> i32 {
        N_TCL.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&FAKE_DEVICE) {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        TCL_HR.load(Ordering::SeqCst) as i32
    }

    fn setup(hr: i32, device: bool) {
        for w in DEV_VTBL.iter() {
            w.store(0, Ordering::SeqCst);
        }
        DEV_VTBL[(D3D8_TESTCOOP_SLOT / 4) as usize]
            .store(fake_tcl as *const () as usize as u32, Ordering::SeqCst);
        FAKE_DEVICE.0.store(addr_of(&DEV_VTBL), Ordering::SeqCst);
        FAKE_DEVICE_PTR.store(
            if device { addr_of(&FAKE_DEVICE) } else { 0 },
            Ordering::SeqCst,
        );
        TEST_CELL.store(addr_of(&FAKE_DEVICE_PTR), Ordering::SeqCst);
        TCL_HR.store(hr as u32, Ordering::SeqCst);
        for n in [&N_TCL, &BAD_ARGS, &TEST_SLEPT] {
            n.store(0, Ordering::SeqCst);
        }
        LOGGED.store(false, Ordering::SeqCst);
        TEST_LINES.with(|l| l.borrow_mut().clear());
    }

    fn lines() -> Vec<String> {
        TEST_LINES.with(|l| l.borrow().clone())
    }

    /// ebx / esi / edi / ebp / espが変わらないことも見る。
    #[test]
    fn entry_sleeps_only_when_the_device_is_lost_and_passes_the_hresult() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entry_address();
        // (名前, TestCooperativeLevelの戻り, デバイスはあるか, 戻り値, TCLの回数, 寝た回数)
        let cases = [
            ("正常", 0, true, 0i32, 1u32, 0u32),
            ("成功の正の値", 1, true, 1, 1, 0),
            ("DEVICELOST", DEVICELOST, true, DEVICELOST, 1, 1),
            ("DEVICENOTRESET", DEVICENOTRESET, true, DEVICENOTRESET, 1, 1),
            ("デバイスが 0", 0, false, DEVICELOST, 0, 1),
        ];
        for (label, hr, device, ret, tcl, slept) in cases {
            setup(hr, device);
            call_with_regs(entry);
            let a = load(&AFTER);
            assert_eq!(a[0] as i32, ret, "{label}: 戻り値");
            assert_eq!(N_TCL.load(Ordering::SeqCst), tcl, "{label}: TCL の回数");
            assert_eq!(
                TEST_SLEPT.load(Ordering::SeqCst),
                slept,
                "{label}: 寝た回数"
            );
            assert_eq!(BAD_ARGS.load(Ordering::SeqCst), 0, "{label}: this が違う");
            assert_eq!(a[3], REG_EBX, "{label}: ebx");
            assert_eq!(a[4], fake_this_addr(), "{label}: esi");
            assert_eq!(a[5], REG_EDI, "{label}: edi");
            assert_eq!(a[6], REG_EBP, "{label}: ebp");
            assert_eq!(a[7], PRE_ESP.load(Ordering::SeqCst), "{label}: esp");
        }
    }

    #[test]
    fn lost_device_logs_one_line() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entry_address();
        setup(0, true);
        call_with_regs(entry);
        assert!(lines().is_empty(), "{:?}", lines());
        setup(DEVICELOST, true);
        call_with_regs(entry);
        call_with_regs(entry);
        let l = lines();
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("0x88760868"), "{l:?}");
        assert_eq!(TEST_SLEPT.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn patch_bytes_are_one_call_and_six_nops() {
        let at = LOST_SLEEP_SITES[0].addr;
        let entry = 0x1234_5678;
        let p = patch_to(at, entry);
        let call: [u8; 5] = p[..5].try_into().unwrap();
        assert_eq!(patch::call_target(at, &call), Some(entry));
        assert_eq!(&p[5..], &[0x90; 6]);
        assert_eq!(
            at + LOST_SLEEP_LEN as u32,
            0x0042_E69D,
            "次の命令は test eax,eax"
        );
    }

    #[test]
    fn install_writes_only_when_all_11_bytes_match() {
        let site = LOST_SLEEP_SITES[0];
        let entry = 0x1000_2000;
        for i in 0..LOST_SLEEP_LEN {
            let mut f = FakeHook::new(0);
            f.mem_base = site.addr;
            f.mem[..LOST_SLEEP_LEN].copy_from_slice(site.expected);
            f.mem[i] ^= 0xFF;
            let before = f.mem;
            assert!(install_at(&mut f, &site, entry).is_err(), "バイト {i}");
            assert_eq!(f.writes, 0, "バイト {i}");
            assert_eq!(f.mem, before, "バイト {i}");
        }
        let mut f = FakeHook::new(0);
        f.mem_base = site.addr;
        f.mem[..LOST_SLEEP_LEN].copy_from_slice(site.expected);
        let msg = install_at(&mut f, &site, entry).expect("全部合えば書く");
        assert!(msg.contains("差し替えました"), "{msg}");
        assert_eq!(f.writes, 1);
        assert_eq!(f.mem[..LOST_SLEEP_LEN], patch_to(site.addr, entry));
        let mut failing = FakeHook::new(0);
        failing.mem_base = site.addr;
        failing.mem[..LOST_SLEEP_LEN].copy_from_slice(site.expected);
        failing.write_fail = Some(5);
        assert!(install_at(&mut failing, &site, entry).is_err());
    }

    #[test]
    fn patched_code_hands_the_hresult_to_the_next_test() {
        const ESI: u32 = 0xE5E5_E5E5;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: 新しいページを1つ確保するだけ（アドレスはOSが選ぶ）。
        let p = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as *mut u8;
        assert!(!p.is_null());
        let base = p as usize as u32;
        let at = base + 6;
        let mut code: Vec<u8> = vec![0x56, 0xBE];
        code.extend_from_slice(&ESI.to_le_bytes());
        code.extend_from_slice(&patch_to(at, entry_address()));
        code.extend_from_slice(&[0x81, 0xFE]); // cmp esi, ESI
        code.extend_from_slice(&ESI.to_le_bytes());
        code.extend_from_slice(&[0x74, 0x05]); // je ok
        code.push(0xB8); // mov eax, 0xBAD
        code.extend_from_slice(&0xBADu32.to_le_bytes());
        code.extend_from_slice(&[0x5E, 0xC3]); // ok: pop esi / ret
        // SAFETY: 確保したページの先頭から、ページより短い機械語を書き込む。
        unsafe { core::ptr::copy_nonoverlapping(code.as_ptr(), p, code.len()) };
        let run = || {
            // SAFETY: 上で書いた機械語（cdeclの引数なし関数の形。戻り値はeax）。
            let f: extern "C" fn() -> i32 = unsafe { core::mem::transmute(p) };
            f()
        };
        setup(0, true);
        assert_eq!(run(), 0);
        setup(DEVICELOST, true);
        assert_eq!(run(), DEVICELOST);
        assert_eq!(TEST_SLEPT.load(Ordering::SeqCst), 1);
        // SAFETY: 上で確保したページを解放する。
        unsafe { VirtualFree(p.cast(), 0, MEM_RELEASE) };
    }
}
