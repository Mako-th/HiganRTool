//! fpsの表示（`0x4316B0`）の測りの差し替え。
//! ゲームの測りはtimeGetTimeのmsで描画した時刻を読む。描画までの長さの揺れが表示に出る。
//! 描画の回数を足す`0x4316C2`をinline detourで入口へ向け、フレームを始めた時刻（ペースの記録。QPC）で測ったfpsを`0x431805`へ渡す。
//! ペースが格子を刻んでいない区間は盗んだ命令を実行し、ゲーム自身の測りへ戻す。
//! 読み込みのスレッド（`0x41B566`。edi = 0）の呼びは表示を作らない側（`0x4318E5`）へ通す。何も測らない。
//! **時期**: 書くのはDllMainの中（`win::on_process_attach`）。主スレッドはまだ`0x4316B0`を実行していない。iniは読まない。
//! 計算は`higanr_core::fps_meter`。アドレスと期待バイト列は`th09_addr.rs`の「fpsの表示の測り」の節。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "fps_win.rs は 32 ビット（i686）専用（ゲームのアドレスと x87 のスタックを前提にしている）"
);

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use higanr_core::fps_meter::FpsMeter;
use higanr_core::patch;

use crate::detour;
use crate::hook::{HookEnv, WriteDone};
use crate::hook_win::RealHook;
use crate::sites::{SiteDef, hex_bytes};
use crate::th09_addr::{D3D8_DEVICE_PTR, FORBIDDEN, FPS_NO_SHOW_AT, FPS_SHOW_AT, FPS_SITES};

/// 入口がアセンブリへ返す答え。
const FALLBACK: u32 = 0;
const NO_SHOW: u32 = 1;
const SHOW: u32 = 2;

/// 盗んだ`mov 0x4b42cc,%ecx`と戻りの`jmp 0x4316C8`。設置が差し替えを書く前に設定する。
static TRAMPOLINE: AtomicU32 = AtomicU32::new(0);
/// 表示を作る側・作らない側の入口（`FPS_SHOW_AT`・`FPS_NO_SHOW_AT`）。設置が差し替えを書く前に設定する。
static SHOW_TO: AtomicU32 = AtomicU32::new(0);
static NO_SHOW_TO: AtomicU32 = AtomicU32::new(0);
/// 表示を作るときのfps（f32のビット）。
static OUT: AtomicU32 = AtomicU32::new(0);
/// `FpsMeter`の状態。書くのはゲームのスレッドだけ。
static BASE: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU32 = AtomicU32::new(0);
/// 基点を置き直すフラグ。1のとき、次のゲームのスレッドの描画で基点を置き直す。読み込みのスレッドが呼んだときに立て、ゲームのスレッドが下ろす。
static REBASE: AtomicU32 = AtomicU32::new(0);
/// 差し替えを書けたら1（`install`だけが立てる）。
static INSTALLED: AtomicU32 = AtomicU32::new(0);
static QPF: AtomicU64 = AtomicU64::new(0);
/// 1にすると以後はゲーム自身の測りへ戻す（パニックを受けた後）。
static DISABLED: AtomicU32 = AtomicU32::new(0);
static PANIC_LOGGED: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
thread_local! {
    static TEST_LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// 検査だけ: ゲームのスレッドのID・フレームを始めた時刻・`[0x4B3844]`の代わりの置き場・パニックの要求。
#[cfg(test)]
static TEST_TID: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static TEST_START: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_QPC_FLAG: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static TEST_PANIC: AtomicU32 = AtomicU32::new(0);

fn log(line: String) {
    let line = format!("fps の表示: {line}");
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
    #[cfg(not(test))]
    crate::main_log::push(line);
}

fn log_detail(line: String) {
    let line = format!("fps の表示: {line}");
    #[cfg(test)]
    TEST_LINES.with(|l| l.borrow_mut().push(line.clone()));
    #[cfg(not(test))]
    crate::main_log::push_detail(line);
}

/// 差し替えが外から得るもの（本物は`RealHook`）。`install_at`の検査が偽のメモリで手順を見る。
trait Host {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool;
    fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32>;
    fn alloc(&mut self, len: usize) -> Result<u32, u32>;
    fn seal(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32>;
    fn free(&mut self, addr: u32);
}

impl Host for RealHook {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        HookEnv::read_code(self, addr, buf)
    }
    fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
        HookEnv::write_code(self, addr, bytes)
    }
    fn alloc(&mut self, len: usize) -> Result<u32, u32> {
        detour::DetourEnv::alloc_trampoline(self, len)
    }
    fn seal(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32> {
        detour::DetourEnv::write_and_seal_trampoline(self, addr, bytes)
    }
    fn free(&mut self, addr: u32) {
        detour::DetourEnv::free_trampoline(self, addr)
    }
}

/// 描画の回数の加算を自前の入口へ向ける。**DllMainから呼ぶ**（ローダーロックの下）。失敗してもロードは止めない。
pub fn install() {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut env = RealHook::new();
        install_at(
            &mut env,
            FPS_SITES,
            entry_address(),
            D3D8_DEVICE_PTR,
            FORBIDDEN,
        )
    }));
    if matches!(r, Ok(Ok(_))) {
        INSTALLED.store(1, Ordering::Relaxed);
    }
    install_log(r);
}

fn install_log(r: std::thread::Result<Result<String, String>>) {
    match r {
        Ok(Ok(s)) => log_detail(format!("設置: {s}")),
        Ok(Err(why)) => log(format!("設置: 非差し替え（ゲーム自身の測りのまま）: {why}")),
        Err(_) => log(
            "設置: パニックを受けました（ゲーム自身の測りのまま。書いたかどうかは分からない）。"
                .to_string(),
        ),
    }
}

fn entry_address() -> u32 {
    fps_entry as *const () as usize as u32
}

/// `rows`（先頭が差し替える行）を全部読んで照合し、全部合ったときだけトランポリンを用意して先頭の行へ`E9`を書く。
/// 他のスレッドは止めない。主スレッドが`0x4316B0`へ来る前（デバイスがまだ無い）でなければ書かない（動いているゲームへの注入）。
/// 書かなかったときは理由を`Err`で返す。
fn install_at(
    env: &mut dyn Host,
    rows: &[SiteDef],
    entry: u32,
    device_cell: u32,
    forbidden: &[patch::AddrRange],
) -> Result<String, String> {
    let mut dev = [0u8; 4];
    if !env.read(device_cell, &mut dev) {
        return Err(format!("{device_cell:#010X} が読み取り不能"));
    }
    if u32::from_le_bytes(dev) != 0 {
        return Err("デバイスが既に存在（ゲームの実行後の注入）".to_string());
    }
    for def in rows {
        let mut now = vec![0u8; def.expected.len()];
        if !env.read(def.addr, &mut now) {
            return Err(format!(
                "{:#010X} から {} バイトの読み取り不能（th09.exe のないプロセスの可能性）",
                def.addr,
                def.expected.len()
            ));
        }
        if now != def.expected {
            return Err(format!(
                "{} {:#010X} の命令が期待と不一致（いま {} / 期待 {}）。別のバージョンの th09.exe か、このフック地点を書き換えるツール（vpatch など）があります",
                def.name,
                def.addr,
                hex_bytes(&now),
                hex_bytes(def.expected)
            ));
        }
    }
    let hook = rows.first().ok_or("表が空")?;
    let plan = detour::plan_detour(hook, entry, forbidden)
        .map_err(|e| format!("表の検査失敗（{e:?}）"))?;
    let tramp = env
        .alloc(hook.expected.len() + 5)
        .map_err(|code| format!("トランポリンのページの確保不能（GetLastError = {code}）"))?;
    let Some(bytes) = plan.trampoline(tramp) else {
        env.free(tramp);
        return Err("トランポリンの長さ超過".to_string());
    };
    if let Err(code) = env.seal(tramp, &bytes.bytes[..bytes.len]) {
        env.free(tramp);
        return Err(format!(
            "トランポリンの書き込みか保護の変更が不能（GetLastError = {code}）"
        ));
    }
    // 入口が読む静的変数は入口へ向く`E9`を書く前に設定する。
    TRAMPOLINE.store(tramp, Ordering::SeqCst);
    SHOW_TO.store(FPS_SHOW_AT, Ordering::SeqCst);
    NO_SHOW_TO.store(FPS_NO_SHOW_AT, Ordering::SeqCst);
    let new = &plan.patch.bytes[..plan.patch.len];
    let done = match env.write(hook.addr, new) {
        Ok(d) => d,
        Err(code) => {
            // 書き込めなかったので誰もこのページへ飛ばない。
            env.free(tramp);
            return Err(format!(
                "{:#010X} へ書き込み不能（GetLastError = {code}）",
                hook.addr
            ));
        }
    };
    let mut back = vec![0u8; new.len()];
    if !env.read(hook.addr, &mut back) || back != new {
        return Err(format!(
            "{:#010X} へ書いた後の読み直しが不一致: {}",
            hook.addr,
            hex_bytes(&back)
        ));
    }
    Ok(format!(
        "{:#010X} の描画の回数の加算を入口 {entry:#010X} への inline detour に差し替えました（トランポリン {tramp:#010X}。保護を戻せた: {}、命令キャッシュを流せた: {}）",
        hook.addr, done.protect_restored, done.flushed
    ))
}

/// # Safety
/// `addr`は読み取り可能な4バイト。
unsafe fn read_u32(addr: u32) -> u32 {
    // SAFETY: 呼び出し元の約束（読み取り可能な4バイト）。
    unsafe { core::ptr::read_volatile(addr as usize as *const u32) }
}

fn qpc_flag_cell() -> u32 {
    #[cfg(test)]
    {
        &TEST_QPC_FLAG as *const AtomicU32 as usize as u32
    }
    #[cfg(not(test))]
    {
        crate::th09_addr::FPS_QPC_FLAG
    }
}

/// ゲームのスレッドのIDと、いまのフレームを始めた時刻（QPC。始めていなければ0）。
fn frame_source() -> (u32, u64) {
    #[cfg(test)]
    {
        (
            TEST_TID.load(Ordering::SeqCst),
            TEST_START.load(Ordering::SeqCst),
        )
    }
    #[cfg(not(test))]
    {
        (
            crate::pacing_win::HEAD_TID.load(Ordering::Relaxed),
            crate::pacing::PACE_RING.current_start(),
        )
    }
}

fn current_tid() -> u32 {
    // SAFETY: 引数の無い呼び出し。
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

fn qpf() -> u64 {
    let q = QPF.load(Ordering::Relaxed);
    if q != 0 {
        return q;
    }
    let q = crate::pacing_win::qpf();
    QPF.store(q, Ordering::Relaxed);
    q
}

/// 基点と数を捨てる（ゲームのスレッドだけ）。次の描画が基点を置き直す。
fn reset_meter() {
    if BASE.load(Ordering::Relaxed) != 0 || COUNT.load(Ordering::Relaxed) != 0 {
        BASE.store(0, Ordering::Relaxed);
        COUNT.store(0, Ordering::Relaxed);
    }
}

/// 差し替えを書けたかどうか。
pub fn replaced() -> bool {
    INSTALLED.load(Ordering::Relaxed) != 0
}

/// 描画1回ぶんの判断。`fps_on_render`が呼ぶ。ゲームのスレッドも読み込みのスレッドも来る。確保しない・待たない。
fn decide(skip: u32) -> u32 {
    #[cfg(test)]
    if TEST_PANIC.load(Ordering::SeqCst) != 0 {
        panic!("検査: fps の測りのパニック");
    }
    if DISABLED.load(Ordering::Relaxed) != 0 {
        return FALLBACK;
    }
    let (game_tid, start) = frame_source();
    // 先頭が呼ばれていない（ペースを組み込まない実行）間はゲーム自身の測りのまま。
    if game_tid == 0 {
        return FALLBACK;
    }
    // SAFETY: 置き場は`[0x4B3844]`（th09.exeの.data。設置が`0x4316CC`の`mov 0x4b3844,%eax`を照合した後にだけここへ来る）。読み取り可能。
    if unsafe { read_u32(qpc_flag_cell()) } != 0 {
        return FALLBACK;
    }
    // 読み込みのスレッドの呼び（edi = 0）は表示を作らない側へ通す。
    // ゲームの測りの基準は差し替え中に止まっている。任せると古い基準との「数 ÷ 数分」を文字列に書いてしまう。
    // 測りの状態はゲームのスレッドだけが書くので基点の置き直しはフラグで頼む。
    if game_tid != current_tid() {
        REBASE.store(1, Ordering::Relaxed);
        return NO_SHOW;
    }
    if REBASE.load(Ordering::Relaxed) != 0 && REBASE.swap(0, Ordering::Relaxed) != 0 {
        reset_meter();
    }
    // 始めた時刻が無い区間（ゲーム自身の処理フロー）はゲーム自身の測りへ任せる。戻ったときに区間の長さで割った1回ぶんを出さないよう、基点を捨てる。
    // ゲーム側の基準`[0x4B42C4]`は逆に古いままになるが、直すにはゲームのメモリへの書き込みが要るので直さない。
    if start == 0 {
        reset_meter();
        return FALLBACK;
    }
    let qpf = qpf();
    if qpf == 0 {
        return FALLBACK;
    }
    let mut meter = FpsMeter::restore(BASE.load(Ordering::Relaxed), COUNT.load(Ordering::Relaxed));
    let shown = meter.on_render(start, skip, qpf);
    let (base, count) = meter.state();
    BASE.store(base, Ordering::Relaxed);
    COUNT.store(count, Ordering::Relaxed);
    match shown {
        Some(fps) => {
            OUT.store(fps.to_bits(), Ordering::Relaxed);
            SHOW
        }
        None => NO_SHOW,
    }
}

/// パニックの記録を1回だけ書く。ゲームのスレッドで確保とロックをするが、パニックを受けた後の1回だけ。
fn note_panic() {
    if PANIC_LOGGED.swap(1, Ordering::Relaxed) != 0 {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        log("パニックを受けました。以後はゲーム自身の測りへ戻します。".to_string())
    }));
}

/// 入口が呼ぶ。引数は描画を飛ばす設定（`[0x4B353A]`のゼロ拡張）。戻り値は`FALLBACK` / `NO_SHOW` / `SHOW`。
/// ebx・esi・edi・ebpはcdeclの約束で保たれる。x87は使わない。
extern "C" fn fps_on_render(skip: u32) -> u32 {
    match catch_unwind(AssertUnwindSafe(|| decide(skip))) {
        Ok(v) => v,
        Err(_) => {
            DISABLED.store(1, Ordering::SeqCst);
            note_panic();
            FALLBACK
        }
    }
}

// SAFETY（`#[unsafe(naked)]`）: `0x004316C2`の`mov 0x4b42cc,%ecx`（6バイト）の代わりに`E9`で入る。`call`ではないので戻りアドレスは積まれていない。
// 入った時のスタックは`0x4316B0`の枠の続き（`push ebp; mov ebp,esp; sub esp,0xc; push ebx; push esi; push edi`の後）、x87のスタックは空、eaxは描画を飛ばす設定（`movzbl`）、ediはthis。
// - eaxを積んで（保存と引数）`fps_on_render`（cdecl）を呼ぶ。呼ばれた側はebx・esi・edi・ebpとespを保ち、x87を使わない。DFは`cld`で0。
// - `NO_SHOW`: 積んだeaxを捨て`0x4318E5`へ`jmp`。スタックもx87もゲームが表示を作らない処理フロー（`0x431723 jb 0x4318e5`）と同じ。ecx・edx・ebxはそこで読まれない。
//   読み込みのスレッドの呼び（edi = 0）はここで`0x4318EE test %edi,%edi`・`0x4318F0 je 0x431922`により何も描かず戻る。
// - `SHOW`: 積んだeaxを捨て、`-0x4(%ebp)`にfloatのfpsを書き、`fld`でst(0)に積み、espを8下げて`0x431805`（`fstpl (%esp)`）へ`jmp`。
//   これはゲームが`0x4317F0 sub $0x8,%esp`・`0x4317FD fstps -0x4(%ebp)`・`0x431802 flds -0x4(%ebp)`で作る状態と同じ。以後の`-0x4(%ebp)`の読み（`0x431841`〜）もそのまま動く。
//   `[0x4B42CC]`（描画の回数）は`0x431805`以降のどこでも読まれない（即値で参照する22行は全部`0x4316C2`〜`0x4317F3`の中）ので書かない。
// - それ以外（`FALLBACK`）: 積んだeaxを戻し、トランポリン（盗んだ`mov 0x4b42cc,%ecx`と`jmp 0x4316C8`）へ。ゲーム自身の測りのまま続く。
// - トランポリンと2つの入口はこの入口へ向く`E9`を書く前に設定される（`install_at`）。
/// 描画の回数の加算（`0x004316C2`）の差し替え先。Rustからは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
extern "C" fn fps_entry() {
    core::arch::naked_asm!(
        "push eax",
        "push eax",
        "cld",
        "call {on_render}",
        "add esp, 4",
        "cmp eax, {show}",
        "je 2f",
        "cmp eax, {no_show}",
        "je 3f",
        "pop eax",
        "jmp dword ptr [{tramp}]",
        "2:",
        "add esp, 4",
        "mov eax, dword ptr [{out}]",
        "mov dword ptr [ebp - 4], eax",
        "fld dword ptr [ebp - 4]",
        "sub esp, 8",
        "jmp dword ptr [{show_to}]",
        "3:",
        "add esp, 4",
        "jmp dword ptr [{no_show_to}]",
        on_render = sym fps_on_render,
        show = const SHOW,
        no_show = const NO_SHOW,
        tramp = sym TRAMPOLINE,
        out = sym OUT,
        show_to = sym SHOW_TO,
        no_show_to = sym NO_SHOW_TO,
    )
}

#[cfg(test)]
mod tests;
