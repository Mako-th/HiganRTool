//! 起動直後のリフレッシュレートの計測（`0x42F1A0`）がBeginSceneを呼ぶ箇所の差し替え。
//! 計測はPresentが失敗するとResetの戻り値を見ずに次のBeginSceneへ進む。そのためフルスクリーンでデバイスが失われるとsystemのd3d8.dllの中でNULLを読んで落ちる。
//! 差し替えた入口はTestCooperativeLevelがD3D_OKでないときBeginSceneを呼ばずに計測の終わりの判定（`0x42F2A8`）へ抜ける。
//! サンプル不足の扱いはゲーム自身に任せる。
//! アドレスと期待バイト列は`th09_addr.rs`の「起動の計測の守り」の節。
//!
//! **時期**: 書くのはDllMainの中（`win::on_process_attach`）。
//! ローダーは`CREATE_SUSPENDED`で作ったゲームに`LoadLibraryW`を呼ばせてから主スレッドを走らせる。そのため計測まで誰もこの14バイトを実行していない。
//! iniは読まない（ローダーロック）。
//! 書く前に14バイトを実バイトで照合し、1バイトでも違えば何も書かない。
//! デバイスが正常なときの入口の仕事はvtable 2回の読みとTestCooperativeLevelの1回だけ。計測の間（起動の約1秒）にしか走らない。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "calib_guard_win.rs は 32 ビット（i686）専用（ゲームのアドレスと cdecl / stdcall の呼びを前提にしている）"
);

use core::sync::atomic::{AtomicBool, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use higanr_core::patch;

use crate::hook::HookEnv;
use crate::hook_win::RealHook;
use crate::sites::SiteDef;
use crate::th09_addr::{
    CALIB_ABORT_TARGET, CALIB_GUARD_LEN, CALIB_GUARD_SITES, D3D8_BEGINSCENE_SLOT,
    D3D8_TESTCOOP_SLOT,
};

/// デバイス（IDirect3DDevice8）のCOMメソッド（TestCooperativeLevel / BeginScene）はstdcallで引数はthisだけ。
type DeviceFn = unsafe extern "system" fn(u32) -> i32;

/// 打ち切りの行を出したかどうか。打ち切りは計測の終わりなので何度も起きないが、行は1回だけにする。
static LOGGED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// 検査だけ: この検査のスレッドが頼んだ行（本ログの待ち行列は全検査で1つなので、行の中身はこちらで見る）。
    static TEST_LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// 検査だけ: `[0x4B3108]`の代わりの置き場のアドレス。
#[cfg(test)]
static TEST_CELL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn log(line: String) {
    let line = format!("起動の計測: {line}");
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
    #[cfg(not(test))]
    crate::main_log::push(line);
}

fn log_detail(line: String) {
    let line = format!("起動の計測: {line}");
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
    #[cfg(not(test))]
    crate::main_log::push_detail(line);
}

/// デバイスへのポインタの置き場のアドレス（製品では`[0x4B3108]`）。
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

/// 計測のBeginSceneの呼びを自前の入口へ向ける。**DllMainから呼ぶ**（ローダーロックの下）。
/// 失敗してもロードは止めない（計測が今のままなだけ）。
pub fn install() {
    let site = CALIB_GUARD_SITES[0];
    let r = catch_unwind(AssertUnwindSafe(|| {
        // 他のスレッドを止めずに書く。そのため主スレッドが計測へ入る前（デバイスがまだ無い）でなければ書かない（動いているゲームへの注入）。
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
        Ok(Err(why)) => log(format!("設置: 非差し替え（計測は今のまま）: {why}")),
        Err(_) => log(
            "設置: パニックを受けました（計測は今のまま。書いたかどうかは分からない）。"
                .to_string(),
        ),
    }
}

fn entry_address() -> u32 {
    calib_guard_entry as *const () as usize as u32
}

/// 書く14バイト: `call entry` / `test eax,eax` / `jnz abort` / `nop`。`call`のrel32は`at + 5`から、`jnz`のrel32は`at + 13`からの相対。
fn guard_patch_to(at: u32, entry: u32, abort: u32) -> [u8; CALIB_GUARD_LEN] {
    let c = patch::call_rel32(at, entry);
    let rel = abort.wrapping_sub(at.wrapping_add(13)).to_le_bytes();
    [
        c[0], c[1], c[2], c[3], c[4], 0x85, 0xC0, 0x0F, 0x85, rel[0], rel[1], rel[2], rel[3], 0x90,
    ]
}

/// `site`の14バイトを読んで期待バイト列と照合し、全部合ったときだけ書く。書いた後に読み直して検証する。
/// 書かなかったときは理由を`Err`で返す。
fn install_at(env: &mut dyn HookEnv, site: &SiteDef, entry: u32) -> Result<String, String> {
    let mut seen = [0u8; CALIB_GUARD_LEN];
    if site.expected.len() != CALIB_GUARD_LEN || !env.read_code(site.addr, &mut seen) {
        return Err(format!(
            "{:#010X} から {} バイトの読み取り不能（th09.exe のないプロセスの可能性）",
            site.addr, CALIB_GUARD_LEN
        ));
    }
    patch::verify(&seen, site.expected).map_err(|e| {
        format!(
            "{:#010X} の命令が期待と不一致（{e:?}）。別のバージョンの th09.exe か、このフック地点を書き換えるツールがあります",
            site.addr
        )
    })?;
    let new = guard_patch_to(site.addr, entry, CALIB_ABORT_TARGET);
    let done = env.write_code(site.addr, &new).map_err(|code| {
        format!(
            "{:#010X} へ書き込み不能（GetLastError = {code}）",
            site.addr
        )
    })?;
    let mut back = [0u8; CALIB_GUARD_LEN];
    if !env.read_code(site.addr, &mut back) || back != new {
        return Err(format!(
            "{:#010X} へ書いた後の読み直しが不一致: {back:02X?}",
            site.addr
        ));
    }
    Ok(format!(
        "{:#010X} の BeginScene の呼びを入口 {entry:#010X} への call に差し替えました（保護を戻せた: {}、命令キャッシュを流せた: {}）",
        site.addr, done.protect_restored, done.flushed
    ))
}

/// 計測を打ち切る理由。
enum Lost {
    NoDevice,
    /// TestCooperativeLevelがD3D_OK以外を返した（`0x88760868` DEVICELOST・`0x88760869` DEVICENOTRESETなど）。
    Hr(i32),
}

/// 打ち切りの行を1回だけ頼む。
/// ゲームの主スレッドから呼ばれるが計測の終わりの1回だけ。ヒープの確保とキューのロックは他のフック（ウィンドウの作成の失敗の行）と同じ。
/// パニックはここで受け、入口の戻り値を変えない。
fn note_lost(why: &Lost) {
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        match why {
        Lost::NoDevice => log(
            "打ち切り: デバイスのポインタ（またはその vtable・メソッド）が 0。BeginScene を呼ばずに計測の終わりの判定へ進みます。".to_string(),
        ),
        Lost::Hr(hr) => log(format!(
            "打ち切り: デバイスが失われています（TestCooperativeLevel = {:#010X}）。BeginScene を呼ばずに計測の終わりの判定へ進みます。",
            *hr as u32
        )),
    }
    }));
}

/// # Safety
/// `addr`は読み取り可能な4バイト（呼び出し元はデバイスのポインタの置き場・生きたデバイスとそのvtableの中だけを渡す）。
unsafe fn read_u32(addr: u32) -> u32 {
    // SAFETY: 呼び出し元の約束（読み取り可能な4バイト）。
    unsafe { core::ptr::read_volatile(addr as usize as *const u32) }
}

/// 差し替えた呼びの入口（cdeclで引数なし）。
/// 0を返したときはBeginSceneを呼んだ後で、ゲームはそのままEndSceneへ進む。
/// 1を返したときは呼んでいない。
/// ebx・esi・edi・ebpはcdeclの約束で保たれるので、抜け先のesi（サンプルの数）が変わらない。
/// 判断の部分（ポインタの読みとvtable越しの2回の呼び）にパニックしうる処理は無い。パニックしうるのはログだけ（`note_lost`が受ける）。
extern "C" fn calib_guard_entry() -> u32 {
    // SAFETY: 置き場は`[0x4B3108]`（.data。読み取り可能）。値はゲームがデバイスを作った後はデバイスのアドレス。作る前と失敗の後は0。
    let dev = unsafe { read_u32(device_cell()) };
    if dev == 0 {
        note_lost(&Lost::NoDevice);
        return 1;
    }
    // SAFETY: 0でない値はゲーム自身がBeginSceneの呼びに使うデバイス。COMオブジェクトの先頭4バイトはvtableのアドレス。
    let vtbl = unsafe { read_u32(dev) };
    if vtbl == 0 {
        note_lost(&Lost::NoDevice);
        return 1;
    }
    // SAFETY: 生きたデバイスのvtableの中のスロット（TestCooperativeLevel `0x0C`・BeginScene `0x88`）。
    let (tcl, begin) = unsafe {
        (
            read_u32(vtbl.wrapping_add(D3D8_TESTCOOP_SLOT)),
            read_u32(vtbl.wrapping_add(D3D8_BEGINSCENE_SLOT)),
        )
    };
    if tcl == 0 || begin == 0 {
        note_lost(&Lost::NoDevice);
        return 1;
    }
    // SAFETY: `tcl`は0でないTestCooperativeLevel（ゲームのメインループ`0x0042E69A`が同じ呼びをする）。stdcall・引数はthisだけ。
    let hr = unsafe { core::mem::transmute::<usize, DeviceFn>(tcl as usize)(dev) };
    // 成功の正の値では打ち切らない（計測のPresentの判定`0x42F21F jge`と同じ向き）。
    if hr < 0 {
        note_lost(&Lost::Hr(hr));
        return 1;
    }
    // SAFETY: `begin`は0でないBeginScene（元の呼び`0x0042F1E1`と同じ）。戻り値はゲームも読まない。
    unsafe { core::mem::transmute::<usize, DeviceFn>(begin as usize)(dev) };
    0
}

#[cfg(test)]
mod tests {
    //! 組み上がりの検査（花映塚は使わない）。
    //! 偽のCOM（vtableを持つ静的な偽のデバイス）に入口を向け、書き換えた機械語を実際に呼んで抜け先・esi・スタックまで見る。
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

    const DEVICELOST: i32 = 0x8876_0868_u32 as i32;
    const DEVICENOTRESET: i32 = 0x8876_0869_u32 as i32;

    #[repr(C)]
    struct Obj(AtomicU32);
    static FAKE_DEVICE: Obj = Obj(AtomicU32::new(0));
    static DEV_VTBL: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
    /// `[0x4B3108]`の代わり（中身は偽のデバイスのアドレスか0）。
    static FAKE_DEVICE_PTR: AtomicU32 = AtomicU32::new(0);
    static TCL_HR: AtomicU32 = AtomicU32::new(0);
    static N_TCL: AtomicU32 = AtomicU32::new(0);
    static N_BEGIN: AtomicU32 = AtomicU32::new(0);
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
    extern "system" fn fake_begin(this: u32) -> i32 {
        N_BEGIN.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&FAKE_DEVICE) {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        0
    }

    /// 偽のデバイスを用意する（呼び出しの数・直前の行・一度だけのフラグも戻す）。
    fn setup(hr: i32, device: bool) {
        for w in DEV_VTBL.iter() {
            w.store(0, Ordering::SeqCst);
        }
        for (slot, f) in [
            (D3D8_TESTCOOP_SLOT, fake_tcl as *const () as usize),
            (D3D8_BEGINSCENE_SLOT, fake_begin as *const () as usize),
        ] {
            DEV_VTBL[(slot / 4) as usize].store(f as u32, Ordering::SeqCst);
        }
        FAKE_DEVICE.0.store(addr_of(&DEV_VTBL), Ordering::SeqCst);
        FAKE_DEVICE_PTR.store(
            if device { addr_of(&FAKE_DEVICE) } else { 0 },
            Ordering::SeqCst,
        );
        TEST_CELL.store(addr_of(&FAKE_DEVICE_PTR), Ordering::SeqCst);
        TCL_HR.store(hr as u32, Ordering::SeqCst);
        for n in [&N_TCL, &N_BEGIN, &BAD_ARGS] {
            n.store(0, Ordering::SeqCst);
        }
        LOGGED.store(false, Ordering::SeqCst);
        TEST_LINES.with(|l| l.borrow_mut().clear());
    }

    fn lines() -> Vec<String> {
        TEST_LINES.with(|l| l.borrow().clone())
    }

    fn assert_callee_saved(label: &str) {
        let a = load(&AFTER);
        assert_eq!(a[3], REG_EBX, "{label}: ebx");
        assert_eq!(a[4], fake_this_addr(), "{label}: esi");
        assert_eq!(a[5], REG_EDI, "{label}: edi");
        assert_eq!(a[6], REG_EBP, "{label}: ebp");
        assert_eq!(a[7], PRE_ESP.load(Ordering::SeqCst), "{label}: esp");
    }

    /// ebx / esi / edi / ebp / espが変わらないことも見る。
    #[test]
    fn entry_calls_begin_scene_only_when_the_device_is_ok() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entry_address();
        // (名前, TestCooperativeLevelの戻り, デバイスはあるか, 戻り値, TCLの回数, BeginSceneの回数)
        let cases = [
            ("正常", 0, true, 0u32, 1u32, 1u32),
            ("成功の正の値", 1, true, 0, 1, 1),
            ("DEVICELOST", DEVICELOST, true, 1, 1, 0),
            ("DEVICENOTRESET", DEVICENOTRESET, true, 1, 1, 0),
            ("デバイスが 0", 0, false, 1, 0, 0),
        ];
        for (label, hr, device, ret, tcl, begin) in cases {
            setup(hr, device);
            call_with_regs(entry);
            assert_eq!(load(&AFTER)[0], ret, "{label}: 戻り値");
            assert_eq!(N_TCL.load(Ordering::SeqCst), tcl, "{label}: TCL の回数");
            assert_eq!(
                N_BEGIN.load(Ordering::SeqCst),
                begin,
                "{label}: BeginScene の回数"
            );
            assert_eq!(BAD_ARGS.load(Ordering::SeqCst), 0, "{label}: this が違う");
            assert_callee_saved(label);
        }
    }

    #[test]
    fn lost_device_logs_one_line_with_the_hresult() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entry_address();
        setup(0, true);
        call_with_regs(entry);
        assert!(lines().is_empty(), "正常なのに行が出た: {:?}", lines());
        setup(DEVICELOST, true);
        call_with_regs(entry);
        call_with_regs(entry);
        let l = lines();
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("0x88760868"), "{l:?}");
        setup(0, false);
        call_with_regs(entry);
        let l = lines();
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("が 0"), "{l:?}");
    }

    #[test]
    fn patch_bytes_encode_the_call_and_the_jump() {
        let at = CALIB_GUARD_SITES[0].addr;
        let entry = 0x1234_5678;
        let p = guard_patch_to(at, entry, CALIB_ABORT_TARGET);
        assert_eq!(p.len(), 14);
        let call: [u8; 5] = p[..5].try_into().unwrap();
        assert_eq!(patch::call_target(at, &call), Some(entry), "call の飛び先");
        assert_eq!(&p[5..9], &[0x85, 0xC0, 0x0F, 0x85], "test eax,eax / jnz");
        let rel = i32::from_le_bytes(p[9..13].try_into().unwrap());
        let jnz_end = at + 13;
        assert_eq!(
            jnz_end.wrapping_add(rel as u32),
            CALIB_ABORT_TARGET,
            "jnz の飛び先"
        );
        assert_eq!(rel, 0xC2, "ノートの計算 0x42F2A8 - 0x42F1E6");
        assert_eq!(p[13], 0x90);
        assert_eq!(at + 14, 0x0042_F1E7, "次の命令");
    }

    #[test]
    fn install_writes_only_when_all_14_bytes_match() {
        let site = CALIB_GUARD_SITES[0];
        let entry = 0x1000_2000;
        for i in 0..CALIB_GUARD_LEN {
            let mut f = FakeHook::new(0);
            f.mem_base = site.addr;
            f.mem[..CALIB_GUARD_LEN].copy_from_slice(site.expected);
            f.mem[i] ^= 0xFF;
            let before = f.mem;
            let r = install_at(&mut f, &site, entry);
            assert!(r.is_err(), "バイト {i} が違うのに書いた");
            assert_eq!(f.writes, 0, "バイト {i}");
            assert_eq!(f.mem, before, "バイト {i} が違うのに中身が変わった");
        }
        let mut f = FakeHook::new(0);
        f.mem_base = site.addr;
        f.mem[..CALIB_GUARD_LEN].copy_from_slice(site.expected);
        let msg = install_at(&mut f, &site, entry).expect("全部合えば書く");
        assert!(msg.contains("差し替えました"), "{msg}");
        assert_eq!(f.writes, 1);
        assert_eq!(f.written_at, Some(site.addr));
        assert_eq!(
            f.mem[..CALIB_GUARD_LEN],
            guard_patch_to(site.addr, entry, CALIB_ABORT_TARGET)
        );
        let mut nowhere = FakeHook::new(0);
        nowhere.mem_base = 0x10;
        assert!(install_at(&mut nowhere, &site, entry).is_err());
        assert_eq!(nowhere.writes, 0);
        let mut failing = FakeHook::new(0);
        failing.mem_base = site.addr;
        failing.mem[..CALIB_GUARD_LEN].copy_from_slice(site.expected);
        failing.write_fail = Some(5);
        assert!(install_at(&mut failing, &site, entry).is_err());
    }

    struct Page(*mut u8);
    impl Page {
        fn new() -> Page {
            // SAFETY: 新しいページを1つ確保するだけ（アドレスはOSが選ぶ）。
            let p = unsafe {
                VirtualAlloc(
                    core::ptr::null(),
                    4096,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                )
            };
            assert!(!p.is_null(), "VirtualAlloc");
            Page(p.cast())
        }
        fn addr(&self) -> u32 {
            self.0 as usize as u32
        }
        fn put(&self, at: usize, b: &[u8]) {
            // SAFETY: 確保した4096バイトのページの中（呼び出し元が at + len <= 4096 を守る）。
            unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), self.0.add(at), b.len()) };
        }
    }
    impl Drop for Page {
        fn drop(&mut self) {
            // SAFETY: `new`で確保したページを、確保した側が解放する。
            unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
        }
    }

    /// ゲームの`0x42F1D9`の前後の形（esi = サンプルの数）を真似る。
    #[test]
    fn patched_code_falls_through_or_jumps_and_keeps_esi() {
        const ESI: u32 = 0xE5E5_E5E5;
        const FALL: u32 = 100;
        const ABORT: u32 = 200;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let page = Page::new();
        let at_off = 6usize; // push esi (1) + mov esi, imm32 (5)
        let at = page.addr() + at_off as u32;
        let abort_off = at_off + CALIB_GUARD_LEN + 5 + 2;
        let mut code: Vec<u8> = vec![0x56, 0xBE];
        code.extend_from_slice(&ESI.to_le_bytes());
        code.extend_from_slice(&guard_patch_to(
            at,
            entry_address(),
            page.addr() + abort_off as u32,
        ));
        code.push(0xB8); // mov eax, FALL
        code.extend_from_slice(&FALL.to_le_bytes());
        code.extend_from_slice(&[0xEB, 0x05]); // jmp done（抜け先のmov eaxを飛ばす）
        assert_eq!(code.len(), abort_off);
        code.push(0xB8); // abort: mov eax, ABORT
        code.extend_from_slice(&ABORT.to_le_bytes());
        code.extend_from_slice(&[0x81, 0xF6]); // done: xor esi, ESI（壊れていなければ0）
        code.extend_from_slice(&ESI.to_le_bytes());
        code.extend_from_slice(&[0x09, 0xF0, 0x5E, 0xC3]); // or eax, esi / pop esi / ret
        page.put(0, &code);
        let run = || {
            // SAFETY: 上で書いた機械語（cdeclの引数なし関数と同じ形で、戻り値はeax）。esiは入口で保存して出口で戻す。
            let f: extern "C" fn() -> u32 = unsafe { core::mem::transmute(page.0) };
            f()
        };
        setup(0, true);
        assert_eq!(run(), FALL, "正常: 次の命令へ落ちて esi が保たれる");
        assert_eq!(N_BEGIN.load(Ordering::SeqCst), 1);
        setup(DEVICELOST, true);
        assert_eq!(run(), ABORT, "失われている: 抜け先へ飛んで esi が保たれる");
        assert_eq!(N_BEGIN.load(Ordering::SeqCst), 0);
        setup(0, false);
        assert_eq!(run(), ABORT, "デバイスが 0: 抜け先へ");
    }
}
