//! パッドの十字キー（POV）を足す入口の Win32 の層（unsafe はここに閉じ込める。1 つずつ `// SAFETY:`）。
//!
//! (1) ゲームが呼ぶ入口 `pad_entry`（元の `0x0042B410` を呼んで、戻り値に十字キーの方向を足す）、(2) デバイスの本物の読み
//! （ゲームのメモリの 2 語・DirectInput の `GetDeviceState`・winmm の `joyGetPosEx`）、(3) `DllMain` からの設置と、ハットの有無の
//! 引き直しと数えの行を頼む別スレッド。行は `main_log::push` で本ログの書き手に頼む。判断は `pad_extra.rs`、POV の値の読み方は
//! `higanr_core::pad_dirs`。**OS へ入力を注ぎ込む API は使わない**（読むだけ）。
//!
//! 設置を `DllMain` の中で行うのは、ローダーがゲームを `CREATE_SUSPENDED` で作り `LoadLibraryW` が戻るまで主スレッドを走らせない
//! （`src/loader/src/launch.rs`）ので、この時点ならゲームのコードをまだ誰も実行していないため。別のスレッドから後で書くと、
//! 読み取りのスレッド（`win.rs`）の設置と互いのスレッドを止め合う恐れがある（`hook::run_attempts` はどちらも自分以外の全スレッドを止める）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "pad_extra_win.rs は 32 ビット（i686）専用（ゲームの呼び出しの約束 fastcall と 32 ビットのアドレスを前提にしている）"
);

use core::ffi::c_void;
use std::io::ErrorKind;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use higanr_core::ini;
use higanr_core::pad_dirs::{self, PadSettings, PovMode};
use windows_sys::Win32::Foundation::{CloseHandle, HMODULE};
use windows_sys::Win32::Media::Multimedia::{JOYCAPSW, JOYINFOEX, joyGetDevCapsW, joyGetPosEx};
use windows_sys::Win32::System::Threading::CreateThread;

use crate::hook_win::RealHook;
use crate::main_log;
use crate::net_config::{INI_FILE, locate};
use crate::pad_extra::{
    InstallReport, LogPlan, PadIo, PadState, attach_lines, counts_line, finish, install,
    settings_line,
};
use crate::th09_addr::{
    DI_GET_DEVICE_STATE_OFFSET, PAD_DEVICE_SLOT_1, PAD_DEVICE_SLOTS, PAD_READ_FN, PAD_SITES,
};
use crate::win::module_path;

/// 元の `0x0042B410` と入口の型。MSVC の `__fastcall`（ecx = 席、edx = デバイス番号、スタックの 1 語、呼ばれた側が `ret 4`）は
/// Rust の `extern "fastcall"` と同じ約束。呼び手 4 か所は `push %esi` の後でスタックを片付けず、`0x0042B497` ほか 4 つの出口が `ret 4`。
pub type PadFn = unsafe extern "fastcall" fn(u32, u32, u32) -> u32;

/// `joyGetPosEx` の `dwFlags`: 全部の欄を返させる。ゲームと同じ値（`0x0042B60F movl $0xff,-0x34(%ebp)`）。windows-sys には合成の定数が無いので書く。
const JOY_RETURNALL: u32 = 0xFF;

/// 数えと旗（ゲームのスレッドとハットの引き直しのスレッドが共有する）。
static STATE: PadState = PadState::new();

/// 書けた呼び出しの数（0 = 入口は働かない。別スレッドが引き直すかの判断に使う）。
static INSTALLED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// `PovMode` を別スレッドへ渡す数（0 = 切る、1 = 既定、2 = always）。
static MODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// ゲームから呼ばれる入口。4 つの呼び出しの rel32 がここへ向く。引数は元の `0x0042B410` と同じ。
pub extern "fastcall" fn pad_entry(seat: u32, dev: u32, acc: u32) -> u32 {
    // SAFETY: `PAD_READ_FN` は表 `PAD_SITES` の `pad_fn_head` / `pad_fn_ret` の行が th09.exe の実バイトと一致したときだけ
    // 入口を呼び出しに繋いだ（`on_attach`）ので、ここに元の関数がある。
    let orig: PadFn = unsafe { core::mem::transmute::<usize, PadFn>(PAD_READ_FN as usize) };
    entry_with(&STATE, &RealPadIo, orig, seat, dev, acc)
}

/// 元の関数を呼んでから十字キーを足す。足す側（`finish`）のパニックは受けて、元の戻り値のまま返す（FFI の境界を越えさせない）。
pub(crate) fn entry_with(
    state: &PadState,
    io: &dyn PadIo,
    orig: PadFn,
    seat: u32,
    dev: u32,
    acc: u32,
) -> u32 {
    // SAFETY: `orig` は元の `0x0042B410`（検査では同じ約束の偽物）。引数は呼び手から受けた値をそのまま渡す。
    let ret = unsafe { orig(seat, dev, acc) };
    catch_unwind(AssertUnwindSafe(|| finish(state, io, ret, dev))).unwrap_or(ret)
}

pub struct RealPadIo;

impl PadIo for RealPadIo {
    fn slots(&self) -> [u32; 2] {
        let read = |addr: u32| -> u32 {
            // SAFETY: `PAD_DEVICE_SLOTS` / `PAD_DEVICE_SLOT_1` は th09.exe の .data（コミット済みで読める頁）の 4 バイトの語。
            // 表の照合が合った呼び出しからだけ来る。ゲームのスレッドが書く語なので volatile。
            unsafe { core::ptr::read_volatile(addr as usize as *const u32) }
        };
        [read(PAD_DEVICE_SLOTS), read(PAD_DEVICE_SLOT_1)]
    }

    fn dinput_pov0(&self, device: u32) -> Option<u32> {
        if device == 0 {
            return None;
        }
        // SAFETY: `device` は `slots` が返した IDirectInputDevice8 へのポインタで 0 でない。ゲームが同じ呼び出しの直前に同じポインタで
        // Poll と GetDeviceState を呼んでおり（`0x0042B453`・`0x0042B4BF`）、ゲームのスレッドは 1 本なので今は生きている。
        // `GetDeviceState` の位置 `DI_GET_DEVICE_STATE_OFFSET`（`0x24`）はゲームが呼ぶ位置と同じ。
        unsafe { dinput_pov0_of(device as usize as *mut c_void) }
    }

    fn winmm_pov(&self, joy_id: u32) -> Option<u32> {
        winmm_pov_of(joy_id)
    }
}

/// IDirectInputDevice8::GetDeviceState の型（COM は x86 では stdcall）。
type GetDeviceStateFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;

/// 4 バイトに揃えた `DIJOYSTATE2` の置き場（ゲームのスタックの置き場も 4 バイト境界）。
#[repr(C, align(4))]
struct Dij2([u8; pad_dirs::DIJOYSTATE2_LEN]);

/// # Safety
/// `device` は生きた IDirectInputDevice8 へのポインタ（先頭のワードが、`GetDeviceState` を `DI_GET_DEVICE_STATE_OFFSET` に持つ
/// vtable へのポインタ）であること。
unsafe fn dinput_pov0_of(device: *mut c_void) -> Option<u32> {
    // SAFETY: 呼び手の約束どおり、`device` の先頭のワードが vtable へのポインタ。
    let vtbl = unsafe { *(device as *const *const u8) };
    if vtbl.is_null() {
        return None;
    }
    // SAFETY: vtable は `GetDeviceState` の位置までの関数ポインタの表。
    let f = unsafe { *(vtbl.add(DI_GET_DEVICE_STATE_OFFSET as usize) as *const usize) };
    if f == 0 {
        return None;
    }
    // SAFETY: その位置の語は `GetDeviceState`（COM の呼び出しの約束は stdcall。引数は this・大きさ・書き先）。
    let get_state = unsafe { core::mem::transmute::<usize, GetDeviceStateFn>(f) };
    let mut st = Dij2([0; pad_dirs::DIJOYSTATE2_LEN]);
    // SAFETY: `st` は `DIJOYSTATE2` の大きさ（0x110）の書ける領域で、その大きさを渡している（ゲームも `0x0042B4B9 push $0x110`）。
    let hr = unsafe {
        get_state(
            device,
            pad_dirs::DIJOYSTATE2_LEN as u32,
            st.0.as_mut_ptr().cast(),
        )
    };
    if hr < 0 {
        return None;
    }
    pad_dirs::dinput_pov0(&st.0)
}

/// `joyGetPosEx` に渡す構造体: `dwSize` は 0x34、`dwFlags` は全部の欄（ゲームと同じ。`0x0042B608 movl $0x34,-0x38(%ebp)`・
/// `0x0042B60F movl $0xff,-0x34(%ebp)`）。
fn new_joyinfo() -> JOYINFOEX {
    JOYINFOEX {
        dwSize: core::mem::size_of::<JOYINFOEX>() as u32,
        dwFlags: JOY_RETURNALL,
        ..Default::default()
    }
}

/// winmm の `joyGetPosEx(joy_id)` の `dwPOV`。エラー（つながっていない）なら `None`。
fn winmm_pov_of(joy_id: u32) -> Option<u32> {
    let mut info = new_joyinfo();
    // SAFETY: `info` は `dwSize` を埋めた JOYINFOEX で、書ける。`joyGetPosEx` は winmm の関数で、引数は番号とこの構造体。
    let r = unsafe { joyGetPosEx(joy_id, &mut info) };
    (r == 0).then_some(info.dwPOV)
}

/// `joyGetDevCapsW` の `wCaps`（つながっていない・引けないなら `None`）。ハットの有無の確かめ用（ゲームのスレッドでは呼ばない）。
fn winmm_caps(joy_id: u32) -> Option<u32> {
    let mut caps = JOYCAPSW::default();
    // SAFETY: `caps` は書ける JOYCAPSW で、その大きさを渡している。
    let r = unsafe {
        joyGetDevCapsW(
            joy_id as usize,
            &mut caps,
            core::mem::size_of::<JOYCAPSW>() as u32,
        )
    };
    (r == 0).then_some(caps.wCaps)
}

// ---- DllMain からの設置 ----

/// `DllMain`（`DLL_PROCESS_ATTACH`）から呼ぶ。ini を読み、切ってなければ 6 行を照合して 4 つの呼び出しを入口へ向け、ログとハットの
/// 引き直しの別スレッドを起こす。パニックは受けて何も書かない扱いにする。
pub fn on_attach(module: HMODULE) {
    let r = catch_unwind(AssertUnwindSafe(|| attach_inner(module)));
    if r.is_err() {
        push_summary("pad: 設置の途中でパニックを受けた（書いた分は残る。入口は書いた分だけ働く）");
    }
}

/// 本ログへ 1 行頼む（ファイルを開かない・書かない）。失敗と異常の行。
fn push_summary(line: &str) {
    main_log::push(line);
}

/// 詳細のときだけ書く行。
fn push_detail_summary(line: &str) {
    main_log::push_detail(line);
}

/// 設置の行。`problem`（ini の誤りなど）のときは簡潔にも残す。
fn push_setting_line(problem: bool, line: &str) {
    main_log::push_when(problem, line);
}

fn attach_inner(module: HMODULE) {
    let dll = module_path(module);
    let dll_dir = dll.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    let (settings, source, ini_problem) = load_settings(dll_dir.as_deref());
    let settings_text = settings_line(settings.pov, &source);
    MODE.store(
        match settings.pov {
            PovMode::Off => 0,
            PovMode::Auto => 1,
            PovMode::Always => 2,
        },
        core::sync::atomic::Ordering::Release,
    );
    if settings.pov == PovMode::Off {
        push_setting_line(
            ini_problem,
            &format!("{settings_text}。何も書かない（ゲームの読みのまま）"),
        );
        start_thread(module);
        return;
    }
    if settings.pov == PovMode::Always {
        STATE.set_winmm_gate(0, true);
        STATE.set_winmm_gate(1, true);
    }
    // 入口が働いてよいのは書く前から（書いた瞬間から呼ばれうる）。書けなかったら下で戻す。
    STATE.enable();
    let mut env = RealHook::new();
    let entry_fn: extern "fastcall" fn(u32, u32, u32) -> u32 = pad_entry;
    let entry = entry_fn as usize as u32;
    let report: InstallReport = install(&mut env, PAD_SITES, entry);
    INSTALLED.store(report.written as u32, core::sync::atomic::Ordering::Release);
    if report.written == 0 {
        STATE.disable();
    }
    // 全部書けて ini も読めたときは詳細だけ。書けなかった・ini が誤りのときは簡潔にも残す。
    for l in &attach_lines(&settings_text, &report) {
        push_setting_line(ini_problem || !report.complete(), l);
    }
    if report.written > 0 && !report.complete() {
        push_summary(
            "pad: 一部の呼び出しだけ書けた（書けなかった呼び出しは今までどおり元の関数を呼ぶだけ）",
        );
    }
    start_thread(module);
}

/// ini の `[pad]` を読む。無い・読めない・誤りは既定（入り）で、理由を返す文に入れる。3 つ目は「ini が読めない・誤り」（ini が無いだけは false）。
fn load_settings(dll_dir: Option<&Path>) -> (PadSettings, String, bool) {
    let env_value = std::env::var_os(ini::PATH_ENV);
    load_settings_with(env_value.as_deref(), dll_dir)
}

/// `load_settings` の本体（環境の変数の値を引数に取る。検査が環境を触らずに済む）。
fn load_settings_with(
    env_value: Option<&std::ffi::OsStr>,
    dll_dir: Option<&Path>,
) -> (PadSettings, String, bool) {
    let loc = match locate(ini::PATH_ENV, env_value, dll_dir, INI_FILE) {
        Ok(l) => l,
        Err(e) => {
            return (
                PadSettings::default(),
                format!("ini の置き場が決まらない（{e}）ので既定"),
                true,
            );
        }
    };
    let text = match std::fs::read_to_string(&loc.path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return (
                PadSettings::default(),
                format!("{} が無いので既定", loc.text()),
                false,
            );
        }
        Err(e) => {
            return (
                PadSettings::default(),
                format!("{} を読めない（{e}）ので既定", loc.text()),
                true,
            );
        }
    };
    match pad_dirs::settings_from_text(&text) {
        Ok(s) => (s, loc.text(), false),
        Err(e) => (
            PadSettings::default(),
            format!("{} の [pad] を読めない（{e}）ので既定", loc.text()),
            true,
        ),
    }
}

// ---- 別スレッド（ハットの有無の引き直しと、数えの行の依頼）----

fn start_thread(module: HMODULE) {
    // SAFETY: `pad_thread` は LPTHREAD_START_ROUTINE の形。引数は自分の DLL のハンドルで、この DLL は自分を解放しない。
    // スレッドの属性は既定。ローダーロックが外れてから走る。
    let h = unsafe {
        CreateThread(
            core::ptr::null(),
            0,
            Some(pad_thread),
            module as *const c_void,
            0,
            core::ptr::null_mut(),
        )
    };
    if h.is_null() {
        push_summary("pad: ハットの引き直しのスレッドを起こせない（winmm の経路は読まない）");
        return;
    }
    // SAFETY: いま作ったスレッドのハンドル。閉じてもスレッドは止まらない。
    unsafe { CloseHandle(h) };
}

extern "system" fn pad_thread(_param: *mut c_void) -> u32 {
    let r = catch_unwind(AssertUnwindSafe(run_thread));
    if r.is_err() {
        crate::win::debug_out("HiganRTool: パッドのスレッドでパニックを受けた。スレッドを終える");
    }
    0
}

/// ハットの引き直しの間隔（ミリ秒）。つなぎ直したパッドを拾うためで、速くする理由が無い。
const REFRESH_MS: u64 = 1000;

/// ハットの有無を引き直して旗を置き、数えの行を本ログへ頼む（初めて経路が使われた・初めて方向を足した・初めて読めなかった。それぞれ高々 1 回）。
fn run_thread() {
    // 設置を断った・1 つも書けなかった実行は、引き直すものが無い（設置の結果は `attach_inner` が本ログへ頼んである）。
    if INSTALLED.load(core::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let mode = match MODE.load(core::sync::atomic::Ordering::Acquire) {
        0 => PovMode::Off,
        2 => PovMode::Always,
        _ => PovMode::Auto,
    };
    let mut plan = LogPlan::default();
    loop {
        let caps = [winmm_caps(0), winmm_caps(1)];
        for (dev, c) in caps.iter().enumerate() {
            STATE.set_winmm_gate(dev, pad_dirs::winmm_may_read(mode, *c));
        }
        let gate = [STATE.winmm_gate(0), STATE.winmm_gate(1)];
        let counts = STATE.counts();
        for l in plan.lines(caps, &counts, STATE.skipped(), gate) {
            // 読めなかった回数が出ているときだけ簡潔にも残す。
            if counts.failed > 0 {
                push_summary(&l);
            } else {
                push_detail_summary(&l);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS));
    }
}

/// 読み取りのスレッドが終わるとき（`win.rs`）に、終わりの数えの 1 行を頼む。設置を断った・1 つも書けなかった実行は頼まない。
pub fn push_final_counts() {
    if INSTALLED.load(core::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let gate = [STATE.winmm_gate(0), STATE.winmm_gate(1)];
    let counts = STATE.counts();
    let line = counts_line("終わり", &counts, STATE.skipped(), gate);
    if counts.failed > 0 {
        push_summary(&line);
    } else {
        push_detail_summary(&line);
    }
}

#[cfg(test)]
mod tests {
    //! 本物の Win32 を、検査のプロセス自身に当てる（花映塚は使わない）。
    use core::cell::Cell;
    use std::path::PathBuf;

    use super::*;
    use crate::pad_extra::PadState;
    use higanr_core::pad_dirs::{DIR_LEFT, DIR_UP};

    #[test]
    fn winmm_structs_match_the_games_layout() {
        assert_eq!(core::mem::size_of::<JOYINFOEX>(), 0x34);
        assert_eq!(core::mem::offset_of!(JOYINFOEX, dwPOV), 0x28);
        assert_eq!(core::mem::offset_of!(JOYINFOEX, dwButtons), 0x20);
        use windows_sys::Win32::Media::Multimedia as mm;
        let all = mm::JOY_RETURNX
            | mm::JOY_RETURNY
            | mm::JOY_RETURNZ
            | mm::JOY_RETURNR
            | mm::JOY_RETURNU
            | mm::JOY_RETURNV
            | mm::JOY_RETURNPOV
            | mm::JOY_RETURNBUTTONS;
        assert_eq!(JOY_RETURNALL, all as u32);
        let info = new_joyinfo();
        assert_eq!((info.dwSize, info.dwFlags), (0x34, 0xFF));
        assert_eq!(JOY_RETURNALL, 0xFF);
        assert_eq!(mm::JOYCAPS_HASPOV, pad_dirs::JOYCAPS_HASPOV);
    }

    #[test]
    fn real_winmm_calls_do_not_crash_and_agree_with_each_other() {
        for id in [0u32, 1, 7] {
            let pos = winmm_pov_of(id);
            let caps = winmm_caps(id);
            if pos.is_some() {
                assert!(caps.is_some(), "{id} 番: 位置は取れたのに caps が引けない");
            }
        }
    }

    // ---- 偽の COM のデバイス ----

    thread_local! {
        static LAST_SIZE: Cell<u32> = const { Cell::new(0) };
        static POV: Cell<u32> = const { Cell::new(0xFFFF_FFFF) };
        static HR: Cell<i32> = const { Cell::new(0) };
    }

    unsafe extern "system" fn fake_get_device_state(
        _this: *mut c_void,
        cb: u32,
        data: *mut c_void,
    ) -> i32 {
        LAST_SIZE.with(|c| c.set(cb));
        if HR.with(Cell::get) < 0 {
            return HR.with(Cell::get);
        }
        // SAFETY: 呼び手（`dinput_pov0_of`）が `cb` バイトの書ける領域を渡している。
        let buf = unsafe { core::slice::from_raw_parts_mut(data.cast::<u8>(), cb as usize) };
        buf.fill(0);
        buf[0x20..0x24].copy_from_slice(&POV.with(Cell::get).to_le_bytes());
        buf[0x1C..0x20].copy_from_slice(&9000u32.to_le_bytes());
        buf[0x24..0x28].copy_from_slice(&27_000u32.to_le_bytes());
        0
    }

    /// 偽の IDirectInputDevice8: 先頭のワードが vtable。vtable の `0x24` 番目（バイト）に GetDeviceState を置く。
    #[repr(C)]
    struct FakeDevice {
        vtbl: *const [usize; 10],
    }

    #[test]
    fn dinput_read_calls_get_device_state_with_the_games_size_and_reads_pov0() {
        assert_eq!(DI_GET_DEVICE_STATE_OFFSET, 0x24);
        let mut vt = [0usize; 10];
        vt[(DI_GET_DEVICE_STATE_OFFSET / 4) as usize] = fake_get_device_state as *const () as usize;
        for (i, v) in vt.iter_mut().enumerate() {
            if i != 9 {
                *v = 0xDEAD_0000 + i;
            }
        }
        let dev = FakeDevice { vtbl: &vt };
        let p = &dev as *const FakeDevice as u32;
        POV.with(|c| c.set(13_500));
        HR.with(|c| c.set(0));
        assert_eq!(RealPadIo.dinput_pov0(p), Some(13_500));
        assert_eq!(LAST_SIZE.with(Cell::get), 0x110);
        HR.with(|c| c.set(-2_147_024_883)); // DIERR_NOTACQUIRED 相当（負）
        assert_eq!(RealPadIo.dinput_pov0(p), None);
        HR.with(|c| c.set(0));
        assert_eq!(RealPadIo.dinput_pov0(0), None);
        let broken = FakeDevice {
            vtbl: core::ptr::null(),
        };
        assert_eq!(
            RealPadIo.dinput_pov0(&broken as *const FakeDevice as u32),
            None
        );
    }

    /// 偽の元の関数（`0x0042B410` の代わり）: 引数の順を戻り値の別々のビットにコピーする。ecx = 席、edx = デバイス、スタックの 1 語。
    unsafe extern "fastcall" fn fake_orig(seat: u32, dev: u32, acc: u32) -> u32 {
        acc | (seat << 8) | (dev << 12)
    }

    struct PovIo(Option<u32>);
    impl PadIo for PovIo {
        fn slots(&self) -> [u32; 2] {
            [1, 0] // winmm の経路
        }
        fn dinput_pov0(&self, _device: u32) -> Option<u32> {
            None
        }
        fn winmm_pov(&self, _joy_id: u32) -> Option<u32> {
            self.0
        }
    }

    struct PanicIo;
    impl PadIo for PanicIo {
        fn slots(&self) -> [u32; 2] {
            panic!("偽物: 読みの中のパニック")
        }
        fn dinput_pov0(&self, _device: u32) -> Option<u32> {
            None
        }
        fn winmm_pov(&self, _joy_id: u32) -> Option<u32> {
            None
        }
    }

    #[test]
    fn entry_calls_the_original_with_the_same_arguments_and_then_adds_the_hat() {
        let s = PadState::new();
        s.enable();
        s.set_winmm_gate(1, true);
        let r = entry_with(&s, &PovIo(Some(27_000)), fake_orig, 1, 1, 0x0003);
        assert_eq!(r, 0x0003 | (1 << 8) | (1 << 12) | DIR_LEFT);
        let r = entry_with(&s, &PovIo(Some(0xFFFF)), fake_orig, 1, 1, 0x0003);
        assert_eq!(r, 0x0003 | (1 << 8) | (1 << 12));
        let r = entry_with(&s, &PovIo(None), fake_orig, 0, 1, 0x0001);
        assert_eq!(r, 0x0001 | (1 << 12));
        s.disable();
        let r = entry_with(&s, &PovIo(Some(0)), fake_orig, 0, 1, 0x0001);
        assert_eq!(r, 0x0001 | (1 << 12));
        s.enable();
        let r = entry_with(&s, &PanicIo, fake_orig, 1, 0, 0x0010);
        assert_eq!(r, 0x0010 | (1 << 8));
        assert_ne!(r & DIR_UP, 0, "元のビット（0x10）は残る");
    }

    #[test]
    fn entry_with_has_the_games_fastcall_shape() {
        let f: PadFn = fake_orig;
        // SAFETY: 偽の元の関数（上）。約束は `PadFn`。
        let r = unsafe { f(2, 1, 0x00F0) };
        assert_eq!(r, 0x00F0 | (2 << 8) | (1 << 12));
    }

    // ---- 機械語の呼び出し元と呼び先の間に、本物の rel32 の書き換えで入口をはさむ ----

    use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use crate::hook::{HookEnv, ThreadRef, WriteDone};
    use crate::sites::{SiteDef, Use};
    use crate::ticks::CopyRow;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
    };

    static T_STATE: PadState = PadState::new();
    static T_ORIG: AtomicUsize = AtomicUsize::new(0);
    static T_POV: AtomicU32 = AtomicU32::new(0xFFFF);

    struct TestIo;
    impl PadIo for TestIo {
        fn slots(&self) -> [u32; 2] {
            [1, 0]
        }
        fn dinput_pov0(&self, _device: u32) -> Option<u32> {
            None
        }
        fn winmm_pov(&self, _joy_id: u32) -> Option<u32> {
            Some(T_POV.load(Ordering::SeqCst))
        }
    }

    /// 検査の入口（`pad_entry` と同じ形。呼び先は機械語で書いた偽の元の関数）。
    extern "fastcall" fn test_entry(seat: u32, dev: u32, acc: u32) -> u32 {
        // SAFETY: `T_ORIG` は検査が置いた、機械語の偽の元の関数（fastcall、スタック 1 個、`ret 4`）のアドレス。
        let orig = unsafe { core::mem::transmute::<usize, PadFn>(T_ORIG.load(Ordering::SeqCst)) };
        entry_with(&T_STATE, &TestIo, orig, seat, dev, acc)
    }

    /// 実行できるメモリに置いた偽のコード。`write_code` / `read_code` はそこへ直に書く（止めるスレッドは無い）。
    struct RawCode {
        base: u32,
    }

    impl HookEnv for RawCode {
        fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
            Ok(Vec::new())
        }
        fn suspend(&mut self, _t: ThreadRef) -> bool {
            true
        }
        fn resume(&mut self, _t: ThreadRef) -> bool {
            true
        }
        fn close(&mut self, _t: ThreadRef) {}
        fn eip(&mut self, _t: ThreadRef) -> Option<u32> {
            None
        }
        fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            assert!(addr >= self.base && addr + buf.len() as u32 <= self.base + 0x200);
            // SAFETY: 範囲は検査が確保した 0x1000 バイトの中（上の assert）。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    addr as usize as *const u8,
                    buf.as_mut_ptr(),
                    buf.len(),
                )
            };
            true
        }
        fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
            assert!(addr >= self.base && addr + bytes.len() as u32 <= self.base + 0x200);
            // SAFETY: 範囲は検査が確保した 0x1000 バイト（読み書き実行できる頁）の中（上の assert）。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    addr as usize as *mut u8,
                    bytes.len(),
                )
            };
            Ok(WriteDone {
                protect_restored: true,
                flushed: true,
            })
        }
        fn set_original_target(&mut self, _target: u32) {}
        fn set_window(&mut self, _addr: u32) {}
        fn set_copy_table(&mut self, _rows: &[CopyRow]) {}
        fn set_render_only_exit(&mut self, _addr: u32) {}
        fn entry_address(&self) -> u32 {
            0
        }
        fn match_entry_address(&self) -> u32 {
            0
        }
        fn set_match_original(&mut self, _target: u32) {}
        fn sleep_ms(&mut self, _ms: u32) {}
        fn now_ms(&self) -> u64 {
            0
        }
        fn counter(&self) -> u32 {
            0
        }
    }

    #[test]
    fn patched_call_site_reaches_the_entry_with_the_games_calling_convention() {
        // SAFETY: 読み書き実行できる 0x1000 バイトを確保する（解放は最後）。
        let mem = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as usize;
        assert_ne!(mem, 0);
        let base = mem as u32;
        let callee = base + 0x100;
        // 偽の `0x0042B410`: eax = [esp+4] | (ecx << 8) | (edx << 12); ret 4。
        let callee_code: [u8; 20] = [
            0x8B, 0x44, 0x24, 0x04, // mov eax,[esp+4]
            0xC1, 0xE1, 0x08, // shl ecx,8
            0x09, 0xC8, // or eax,ecx
            0xC1, 0xE2, 0x0C, // shl edx,12
            0x09, 0xD0, // or eax,edx
            0xC2, 0x04, 0x00, // ret 4
            0x90, 0x90, 0x90,
        ];
        // 呼び出し元: ゲームと同じ形。呼んだ後でスタックが元の高さか確かめ、ずれていれば eax に印を入れて戻す。
        let acc: u32 = 0x0000_1303;
        let seat: u32 = 1;
        let dev: u32 = 1;
        let call_at = base + 18;
        let rel = callee.wrapping_sub(call_at + 5);
        let mut caller: Vec<u8> = vec![0x53, 0x89, 0xE3, 0x68];
        caller.extend_from_slice(&acc.to_le_bytes());
        caller.push(0xB9);
        caller.extend_from_slice(&seat.to_le_bytes());
        caller.push(0xBA);
        caller.extend_from_slice(&dev.to_le_bytes());
        assert_eq!(caller.len() as u32, 18);
        caller.push(0xE8);
        caller.extend_from_slice(&rel.to_le_bytes());
        caller.extend_from_slice(&[
            0x39, 0xE3, // cmp ebx,esp
            0x74, 0x07, // je +7
            0x89, 0xDC, // mov esp,ebx
            0xB8, 0x00, 0x00, 0xAD, 0x0B, // mov eax,0x0BAD0000
            0x5B, // pop ebx
            0xC3, // ret
        ]);
        // SAFETY: どちらも確保した 0x1000 バイトの中（先頭と +0x100）。
        unsafe {
            core::ptr::copy_nonoverlapping(caller.as_ptr(), mem as *mut u8, caller.len());
            core::ptr::copy_nonoverlapping(
                callee_code.as_ptr(),
                (mem + 0x100) as *mut u8,
                callee_code.len(),
            );
        }
        let run = || -> u32 {
            // SAFETY: 先頭は上で書いた呼び出し元（cdecl で呼べる: ebx を保ち、eax に戻す）。
            let f = unsafe { core::mem::transmute::<usize, extern "C" fn() -> u32>(mem) };
            f()
        };
        let plain = acc | (seat << 8) | (dev << 12);
        assert_eq!(
            run(),
            plain,
            "書き換える前の呼び出し元と呼び先が合っていない"
        );

        // 本物の `install` の書き方で rel32 を入口へ向ける。行は検査用の合成（アドレスは確保した領域）。
        let want: &'static [u8] = Box::leak(
            {
                let mut v = vec![0xE8];
                v.extend_from_slice(&rel.to_le_bytes());
                v
            }
            .into_boxed_slice(),
        );
        let rows = [SiteDef {
            name: "test_call",
            addr: call_at,
            expected: want,
            kind: Use::CallRel32 {
                original_target: callee,
            },
            source: "合成",
        }];
        T_ORIG.store(callee as usize, Ordering::SeqCst);
        T_STATE.enable();
        T_STATE.set_winmm_gate(1, true);
        let entry_fn: extern "fastcall" fn(u32, u32, u32) -> u32 = test_entry;
        let entry = entry_fn as usize as u32;
        let report = crate::pad_extra::install(&mut RawCode { base }, &rows, entry);
        assert!(report.complete(), "{:?}", report.lines);

        T_POV.store(27_000, Ordering::SeqCst);
        assert_eq!(run(), plain | DIR_LEFT, "入口が方向を足していない");
        T_POV.store(0, Ordering::SeqCst);
        assert_eq!(run(), plain | DIR_UP);
        T_POV.store(0xFFFF, Ordering::SeqCst);
        assert_eq!(run(), plain);
        T_STATE.disable();
        T_POV.store(27_000, Ordering::SeqCst);
        assert_eq!(run(), plain);
        // SAFETY: 上で確保した領域を 1 回だけ解放する。
        unsafe { VirtualFree(mem as *mut c_void, 0, MEM_RELEASE) };
    }

    #[test]
    fn a_foreign_process_is_refused_with_the_real_reader_and_nothing_is_written() {
        let mut env = RealHook::new();
        let entry_fn: extern "fastcall" fn(u32, u32, u32) -> u32 = test_entry;
        let r = crate::pad_extra::install(&mut env, PAD_SITES, entry_fn as usize as u32);
        assert_eq!(r.written, 0, "{:?}", r.lines);
        assert!(!r.complete());
        assert!(
            r.lines.iter().any(|l| l.contains("1 バイトも書いていない")),
            "{:?}",
            r.lines
        );
        assert!(
            r.lines
                .iter()
                .any(|l| l.contains("不一致") || l.contains("読めない")),
            "{:?}",
            r.lines
        );
        assert!(r.verified < r.rows);
    }

    #[test]
    fn the_final_counts_line_reaches_the_main_log_queue_only_when_installed() {
        use core::sync::atomic::Ordering::SeqCst;
        let marker = |l: &String| l.starts_with("pad: 数え（終わり）");
        INSTALLED.store(0, SeqCst);
        push_final_counts();
        assert!(!crate::main_log::peek().iter().any(marker));
        INSTALLED.store(4, SeqCst);
        push_final_counts();
        INSTALLED.store(0, SeqCst);
        assert_eq!(
            crate::main_log::peek().iter().filter(|l| marker(l)).count(),
            1
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pad_ini_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn settings_come_from_the_ini_next_to_the_dll_or_the_env_path() {
        let d = temp_dir("a");
        let (s, why, problem) = load_settings_with(None, Some(&d));
        assert_eq!(s, PadSettings::default());
        assert!(
            why.contains("無い") && !problem,
            "ini が無いだけは誤りでない: {why}"
        );
        std::fs::write(d.join(INI_FILE), "[pad]\npov = 0\n").unwrap();
        assert_eq!(load_settings_with(None, Some(&d)).0.pov, PovMode::Off);
        std::fs::write(d.join(INI_FILE), "[pad]\npov = maybe\n").unwrap();
        let (s, why, problem) = load_settings_with(None, Some(&d));
        assert_eq!(s, PadSettings::default());
        assert!(why.contains("maybe") && problem, "{why}");
        let other = d.join("other.ini");
        std::fs::write(&other, "[pad]\npov = always\n").unwrap();
        assert_eq!(
            load_settings_with(Some(other.as_os_str()), Some(&d)).0.pov,
            PovMode::Always
        );
        let (s, why, problem) = load_settings_with(Some(std::ffi::OsStr::new("rel.ini")), Some(&d));
        assert_eq!(s, PadSettings::default());
        assert!(why.contains("絶対パス") && problem, "{why}");
        // 読めて入っているときも誤りでない。
        std::fs::write(d.join(INI_FILE), "[pad]\npov = 1\n").unwrap();
        assert!(!load_settings_with(None, Some(&d)).2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 壊れたら落ちる: `[pad]` の誤りの行が、設置が全部できた実行で詳細だけに回る（配線の検査。読みそのものは上の検査）。
    #[test]
    fn an_ini_problem_reaches_every_setup_line_as_a_brief_line() {
        let src = include_str!("pad_extra_win.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            body.contains("let (settings, source, ini_problem) = load_settings("),
            "ini の誤りを受けていない"
        );
        assert!(body.contains("push_setting_line(ini_problem || !report.complete(), l)"));
        assert!(body.contains("push_setting_line(\n            ini_problem,"));
    }

    #[test]
    fn the_distributed_ini_has_a_pad_section_that_parses_to_the_default() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini");
        assert!(dist.contains("[pad]"));
        assert_eq!(
            pad_dirs::settings_from_text(dist),
            Ok(PadSettings { pov: PovMode::Auto })
        );
        assert!(crate::net_config::from_text(dist).is_ok());
    }
}
