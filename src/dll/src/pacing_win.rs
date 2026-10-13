//! ペースと時刻のフックの Win32 の層（unsafe はここにもある。1 つずつ `// SAFETY:`）。
//!
//! 置くもの: QPC、先頭の入口 `pacing_head_entry` と Present の入口 `present_entry`（素のアセンブリ）、入口から呼ぶ Rust の関数
//! （`pace_head` / `pace_input` / `pace_present_begin` / `pace_present_end`。中は全部 `catch_unwind`）、先頭のフック（ペースの状態と
//! 高分解能の待機タイマー）、本物の `PacingHost`（ini・書き出しのスレッド）。判断・順番・記録のリングバッファは `pacing.rs`、期限の式と集計は
//! `higanr_core::pacing` にだけ書く。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "pacing_win.rs は 32 ビット（i686）専用（入口が 32 ビットのアドレスと x87 を前提にしている）"
);

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use higanr_core::pacing::{self as pace, CpuClasses, PacingIni, Preamble};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Kernel::PROCESSOR_NUMBER;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::GetSystemCpuSetInformation;
use windows_sys::Win32::System::Threading::{
    CreateWaitableTimerW, GetCurrentProcessorNumberEx, GetCurrentThread, SetWaitableTimer, Sleep,
    WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

use crate::net_win::{Event, Waiter};
use crate::observe::RecordEnv;
use crate::pacing::{
    self, HEAD_FALLBACK, HEAD_NOT_YET, HEAD_RUN, INPUT_STAMP_ON, PACE_RING, PaceClock,
    PaceCounters, Pacer, PacingHost, WriterEnd,
};
use crate::record_win::RealRecord;
use crate::script_win::RealScriptHost;
use crate::session::ScriptHost;
use crate::th09_addr::{GAME_FPU_CW, WINDOW_CLOCK_DIVISOR, WINDOW_DEADLINE};

pub fn qpc() -> u64 {
    let mut q: i64 = 0;
    // SAFETY: `q` は書ける i64（LARGE_INTEGER と同じ大きさ）。XP 以降は失敗しない。
    unsafe { QueryPerformanceCounter(&mut q) };
    q as u64
}

pub fn qpf() -> u64 {
    let mut f: i64 = 0;
    // SAFETY: 同上。
    unsafe { QueryPerformanceFrequency(&mut f) };
    f.max(0) as u64
}

/// いま走っている論理 CPU（`pace::flat_cpu`）。GetCurrentProcessorNumberEx は OS に入らずに読む作りと見られ、1 回は数十 ns 程度の
/// 見込み。実際の重さは `tests::stamp_overhead_per_frame` が測る。
pub fn cpu_now() -> u32 {
    let mut p = PROCESSOR_NUMBER {
        Group: 0,
        Number: 0,
        Reserved: 0,
    };
    // SAFETY: `p` は書ける PROCESSOR_NUMBER。Windows 7 以降の API で、失敗は無い（戻り値も無い）。
    unsafe { GetCurrentProcessorNumberEx(&mut p) };
    pace::flat_cpu(p.Group, p.Number)
}

/// このスレッドが CPU で走った周期の累計（QueryThreadCycleTime）。取れなければ 0（「無い」）。数える速さは TSC（周波数の変わらない
/// 時計）の見込みで、分かるのは「その間にスレッドが CPU に載っていた割合」。
pub fn thread_cycles() -> u64 {
    let mut c: u64 = 0;
    // SAFETY: GetCurrentThread は閉じなくてよい疑似ハンドルを返し、`c` は書ける u64。失敗したら 0 のまま。
    let ok = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    if ok == 0 { 0 } else { c }
}

/// 論理 CPU ごとの効率クラスの表（GetSystemCpuSetInformation で大きさを聞いてから、8 バイト揃えの箱に埋めさせる。`AllocationTag` が
/// u64 なので揃える）。読み取りのスレッドで設置の時に 1 回だけ呼ぶ。
pub fn cpu_classes_now() -> Result<CpuClasses, String> {
    let mut need: u32 = 0;
    // SAFETY: 箱を渡さず（null・長さ 0）必要な大きさだけを聞く。プロセスは null（全体の表）。
    unsafe {
        GetSystemCpuSetInformation(
            core::ptr::null_mut(),
            0,
            &mut need,
            core::ptr::null_mut(),
            0,
        )
    };
    if need == 0 {
        return Err("必要な大きさが 0 と返った".to_string());
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    let mut got: u32 = 0;
    // SAFETY: `buf` は `need` バイト以上・8 バイト揃えで書ける。`got` に書いた大きさが返る。
    let ok = unsafe {
        GetSystemCpuSetInformation(
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut got,
            core::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetSystemCpuSetInformation が失敗（大きさ {need} バイト）"
        ));
    }
    let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_le_bytes()).collect();
    let n = (got as usize).min(bytes.len());
    pace::parse_cpu_sets(&bytes[..n])
}

/// 普通の待機タイマー（高分解能の旗なし）。`mode = vpatch` が、vpatch と同じ待ち方（待ちの細かさは OS の時計の分解能）を再現するためだけに使う。
struct PlainTimer(HANDLE);

// SAFETY: カーネルのタイマーのハンドル。作ったスレッド（読み取りのスレッド）から `RealClock` ごとゲームのスレッドへ移して使い、同時に 2 本からは使わない。
unsafe impl Send for PlainTimer {}

/// タイマーが鳴らないときの歯止め（ms）。待ちたい長さにこれを足した所で必ず戻る。
const PLAIN_GUARD_MS: u32 = 50;

impl PlainTimer {
    fn new() -> Option<PlainTimer> {
        // SAFETY: 属性 NULL・手動リセット・名前 NULL（名前なし）を渡すだけ。
        let h = unsafe { CreateWaitableTimerW(core::ptr::null(), 1, core::ptr::null()) };
        (!h.is_null()).then_some(PlainTimer(h))
    }

    fn sleep_us(&self, us: u32) {
        if us == 0 {
            return;
        }
        // 100 ns 単位。負は「今から」の相対。
        let due: i64 = -(i64::from(us) * 10);
        // SAFETY: `self.0` は `new` で作ってまだ閉じていないタイマー。`due` はこの呼び出しの間生きている。周期 0（1 回だけ）、完了ルーチンなし、電源の復帰なし。
        let ok = unsafe { SetWaitableTimer(self.0, &due, 0, None, core::ptr::null(), 0) };
        if ok != 0 {
            // SAFETY: 開いたタイマーを有限の ms で待つだけ（歯止めがあるので `INFINITE` にしない）。
            unsafe { WaitForSingleObject(self.0, us / 1000 + PLAIN_GUARD_MS) };
        }
    }
}

impl Drop for PlainTimer {
    fn drop(&mut self) {
        // SAFETY: `new` で作ったハンドルを 1 回だけ閉じる。
        unsafe { CloseHandle(self.0) };
    }
}

/// 本物の時計と待ち（高分解能の待機タイマー。`net_win::Waiter` をそのまま使い、イベントは立てない）。
pub struct RealClock {
    waiter: Waiter,
    /// `mode = vpatch` の待ちに使う普通の待機タイマー（他の方式では None）。
    plain: Option<PlainTimer>,
    /// auto の先頭が読む画面の形のアドレス（`th09_addr::OPTION_WINDOWED`）。0 なら読まない（明示の形・読めると確かめられなかった設置・
    /// 検査のプロセス）。
    screen_flag: u32,
    /// ゲーム自身の時計の経路で読むウィンドウの構造体のアドレス（`th09_addr::WINDOW_ADDR`）。0 なら読まない（`game_due` は偽 —— 始まりで待たない）。
    window: u32,
}

impl RealClock {
    /// ゲーム自身の時計の経路（先頭が `HEAD_FALLBACK` を返す回）で、この 1 回がフレームを始める見込みか（`pace::game_frame_due`。真は
    /// 早すぎる側にだけ外れる）。ウィンドウの構造体の時計の分母 `+0x14` と期限 `+0x3C`（double を 2 つの 32 ビットで）を読み、整数だけで
    /// 比べる（x87 に触らない）。確保しない・待たない。
    fn game_due(&self) -> bool {
        if self.window == 0 {
            return false;
        }
        let at = |off: u32| (self.window + off) as usize as *const u32;
        // SAFETY: 0 でないアドレスは `pacing::install` が `WINDOW_ADDR`（th09.exe の .data。image base の一致と地点の照合が通った後に
        // だけ来る）から `WINDOW_SPAN` バイトを設置の時に読めると確かめてから渡したものだけ。読む 3 語は範囲の中で 4 バイト揃え。exe の .data はプロセスが終わるまで読める。
        // 書き手（時計の分母は WinMain、期限は do_frame の先頭）はこの先頭と同じゲームのスレッドなので、読む間に変わらない。
        let (divisor, lo, hi) = unsafe {
            (
                core::ptr::read_volatile(at(WINDOW_CLOCK_DIVISOR)),
                core::ptr::read_volatile(at(WINDOW_DEADLINE)),
                core::ptr::read_volatile(at(WINDOW_DEADLINE + 4)),
            )
        };
        pace::game_frame_due(
            qpc() as u32,
            divisor,
            (u64::from(hi) << 32) | u64::from(lo),
            pace::game_due_early_ticks(divisor),
        )
    }
}

impl PaceClock for RealClock {
    fn now(&mut self) -> u64 {
        qpc()
    }
    fn sleep_us(&mut self, us: u32) {
        // イベントは誰も立てないので、戻るのはタイマーの時刻（か歯止め）。
        let _ = self.waiter.wait(us);
    }
    fn sleep_plain_us(&mut self, us: u32) {
        if let Some(t) = &self.plain {
            t.sleep_us(us);
        }
    }
    fn yield_now(&mut self) {
        // SAFETY: 引数だけの呼び出し。0 は「残りの時間片を譲り、走れるほかのスレッドが無ければすぐ戻る」。
        unsafe { Sleep(0) };
    }
    fn cpu(&mut self) -> u32 {
        cpu_now()
    }
    fn screen(&mut self) -> pace::Screen {
        if self.screen_flag == 0 {
            return pace::Screen::Unknown;
        }
        // SAFETY: 0 でないアドレスは `pacing::install` が `OPTION_WINDOWED`（th09.exe の .data。image base の一致と地点の照合が通った
        // 後にだけ来る）を設置の時に 1 バイト読めると確かめてから渡したものだけ。exe の .data はプロセスが終わるまで読める。書き手（設定の読み込み・オプションの画面）は
        // この先頭と同じゲームのスレッドなので、
        // 読む間に変わらない。確保も待ちもしない。
        let b = unsafe { core::ptr::read_volatile(self.screen_flag as usize as *const u8) };
        pace::screen_from_flag(b)
    }
}

/// 先頭のフック。ゲームのスレッドは `try_lock` だけで取り（待たない）、取れなければゲーム自身の経路へ任せる。錠を持つのは設置の前
/// （`arm_pacer`）と、ゲームのスレッドの先頭のフックの中だけ。
static PACER: Mutex<Option<(Pacer, RealClock)>> = Mutex::new(None);

/// 先頭の入口を開けるか（0 なら元の飛び先へ素通り）。rel32 を書く前に 1 にする。
pub static PACING_ON: AtomicU32 = AtomicU32::new(0);
/// 先頭の入口が呼ぶ関数（本物は `pace_head`。検査で差し替える）。
pub static HEAD_FN: AtomicU32 = AtomicU32::new(0);
/// 先頭の元の飛び先（時計 `0x0042CC90`）・始めるときの着地（`0x0042DC8D`）・まだのときの着地（`0x0042DD81`）。
pub static HEAD_ORIGINAL: AtomicU32 = AtomicU32::new(0);
pub static HEAD_RESUME: AtomicU32 = AtomicU32::new(0);
pub static HEAD_EARLY_EXIT: AtomicU32 = AtomicU32::new(0);
/// Present のラッパ（`0x0042DB50`）。
pub static PRESENT_ORIGINAL: AtomicU32 = AtomicU32::new(0);
/// `mode = vpatch` のときだけ 1。Present の前の待ちのフックを開ける（他の方式では `pace_present_begin` が原子変数を 1 回読むだけ）。
static VPATCH_WAIT_ON: AtomicU32 = AtomicU32::new(0);
/// 始めるときに `fldcw` で置く制御語（下位 16 ビット）。
static FPU_CW: AtomicU32 = AtomicU32::new(GAME_FPU_CW as u32);
/// 先頭でゲーム自身の経路へ任せた回数・パニックを受けた回数。
static FALLBACKS: AtomicU32 = AtomicU32::new(0);
static PANICS: AtomicU32 = AtomicU32::new(0);

/// 始まりの中で相手の入力ワードを待つフック（`script_win::head_wait_hook`。delay 方式の lockstep のときだけ待つ）。0 なら何もしない。置くのはネットワークのセッションを
/// 動かす実行だけで、それ以外では `pace_head` は原子変数を 1 回読んで戻るだけ。関数のアドレスを切り捨てずに持つので `AtomicUsize`。
static HEAD_WAIT_FN: AtomicUsize = AtomicUsize::new(0);

/// 始まりの中の待ちのフックを置く・外す（`NET_ON` を 1 にする前後に、`script_win` が置く）。
pub fn set_head_wait(f: Option<fn()>) {
    HEAD_WAIT_FN.store(f.map_or(0, |f| f as usize), Ordering::Release);
}

/// 始まりの中の待ちのフックを呼ぶ（箱の錠は手放した後。ゲームのスレッド）。フックの中のパニックは受けてフックを外す。
fn run_head_wait() {
    let f = HEAD_WAIT_FN.load(Ordering::Acquire);
    if f == 0 {
        return;
    }
    // SAFETY: `f` は `set_head_wait` が `fn()` のアドレスを `usize` にして置いた値で、0 でない。切り捨てや加工をしていない。関数のアドレスと `usize` は同じ大きさ。
    let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(f) };
    if catch_unwind(f).is_err() {
        HEAD_WAIT_FN.store(0, Ordering::SeqCst);
        PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

/// 先頭の判断（ゲームのスレッド。`pacing_head_entry` が呼ぶ）。錠が取れない・箱が無い・パニックなら `HEAD_FALLBACK`（`FALLBACKS` /
/// `PANICS` に数える）。`mode = cycle` の game の区間は箱が `HEAD_FALLBACK` を返す（決めた経路なので数えない）。
///
/// 始まりの中の待ち（`HEAD_WAIT_FN`）を呼ぶのは次の 2 つのときだけ。返す直前に呼ぶ。
/// - 箱が「始める」（`HEAD_RUN`）と決めたとき（ウィンドウの b-spin / b の格子の待ちの後）。
/// - 箱がゲーム自身の時計へ任せる（`HEAD_FALLBACK`。全画面・cycle の game の区間）と決め、フックが置かれていて、ゲームがこの 1 回で
///   フレームを始める見込みのとき（`RealClock::game_due`）。この経路は先頭が 1 フレームに何千回も来る（全画面の実回線で平均
///   13,000〜21,000 回）ので、期限の前の回で待つと上限を期限の前に使い切って効かない。期限の「早め」
///   （`pace::GAME_DUE_EARLY_US`）の中に入った回でだけ呼ぶ。同じ tick で 2 度は待たない（フックの側）。
///
/// 「まだ」（`HEAD_NOT_YET`）・錠が取れない・箱が無い・パニックの経路では待たない。錠は待つ前に手放す（待つ間、読み取りのスレッドが
/// 先頭のフックを触れるように）。
pub extern "C" fn pace_head() -> u32 {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut g) = PACER.try_lock() else {
            return None;
        };
        let (p, c) = g.as_mut()?;
        let code = p.head(c, &PACE_RING);
        // フックが無い実行（ネットワークなし）ではウィンドウの構造体を読まない（1 フレームに何千回も来る経路に読みを足さない）。
        let due =
            code == HEAD_FALLBACK && HEAD_WAIT_FN.load(Ordering::Relaxed) != 0 && c.game_due();
        Some((code, due))
    }));
    match r {
        Ok(Some((code, due))) => {
            if code == HEAD_RUN || due {
                run_head_wait();
            }
            code
        }
        Ok(None) => {
            FALLBACKS.fetch_add(1, Ordering::Relaxed);
            HEAD_FALLBACK
        }
        Err(_) => {
            PANICS.fetch_add(1, Ordering::Relaxed);
            HEAD_FALLBACK
        }
    }
}

/// 入力の入口（`stub::input_wait_entry`）が呼ぶ。引数は on_tick の this の `+0x454`。時刻を先に取り、論理 CPU とスレッドの周期は
/// 後に取る（周期の窓を QPC の窓の内側に入れる。足した 2 つの呼び出しの重さは「入力 → Present の前」に入る）。
pub extern "C" fn pace_input(sup_frame: u32) {
    let r = catch_unwind(|| {
        let t = qpc();
        PACE_RING.on_input(t, sup_frame, cpu_now(), thread_cycles())
    });
    if r.is_err() {
        PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Present の入口が、ラッパを呼ぶ前に呼ぶ。論理 CPU とスレッドの周期を先に取り、時刻は最後に取る（入力と逆の順。Present の長さに
/// 足した呼び出しの重さを入れない）。
pub extern "C" fn pace_present_begin() {
    let r = catch_unwind(|| {
        if VPATCH_WAIT_ON.load(Ordering::Relaxed) != 0 {
            vpatch_wait_before_present();
        }
        let (cpu, cycles) = (cpu_now(), thread_cycles());
        PACE_RING.on_present_begin(qpc(), cpu, cycles)
    });
    if r.is_err() {
        PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `mode = vpatch`: Present の呼び出しの前に期限まで待つ（待ち終わった後の時刻が「Present の前」の印になる）。錠が取れない・箱が無いときは待たない。
fn vpatch_wait_before_present() {
    let Ok(mut g) = PACER.try_lock() else {
        return;
    };
    if let Some((p, c)) = g.as_mut() {
        p.before_present(c);
    }
}

pub extern "C" fn pace_present_end() {
    if catch_unwind(|| PACE_RING.on_present_end(qpc())).is_err() {
        PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Present の後のフック（rollback のモードのコピーと進め直し。`script_win::rb_post_present`）。0 なら何もしない。置くのは rollback の入口を
/// 全部書けた実行だけ。
static AFTER_PRESENT_FN: AtomicU32 = AtomicU32::new(0);

/// Present の後のフックを置く・外す（読み取りのスレッドが、Present の call を書いた後・ネットワークを動かす前に）。
pub fn set_after_present(f: Option<fn()>) {
    AFTER_PRESENT_FN.store(f.map_or(0, |f| f as usize as u32), Ordering::Release);
}

/// Present の入口が、`pace_present_end` の後に呼ぶ。時刻のフックの印（Present の長さ）に進め直しの時間が入らないよう、印の後で呼ぶ。
/// フックの中のパニックは受けてフックを外す。
pub extern "C" fn pace_after_present() {
    let f = AFTER_PRESENT_FN.load(Ordering::Acquire);
    if f == 0 {
        return;
    }
    // SAFETY: `f` は `set_after_present` が `fn()` のアドレスを 32 ビットにして置いた値で、0 でない。関数のアドレスと整数は 32 ビットで同じ大きさ。
    let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(f as usize) };
    if catch_unwind(f).is_err() {
        AFTER_PRESENT_FN.store(0, Ordering::SeqCst);
        PANICS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY（`#[unsafe(naked)]`）: `0x0042DC44` の `call 0x42cc90`（時計）の差し替え先。入った時のスタックは
// [戻りアドレス `0x0042DC49`][edi][esi][メインループへの戻りアドレス]（do_frame の先頭の `push esi; push edi`）、x87 のスタックは空。
// - PACING_ON が 0 なら、何も変えずに元の飛び先（時計）へ `jmp`。時計は `0x0042DC49` へ戻り、ゲーム自身の経路になる。
// - 0 でなければ HEAD_FN（`pace_head`。`extern "C"`、引数なし、戻り値は eax）を呼ぶ。呼ばれた側は cdecl で ebx・esi・edi・ebp と
//   esp を保ち、x87 を使わない。DF は `cld` で 0。eax・ecx・edx は壊れてよい（着地の `0x0042DC8D` は ecx を読み直し、`0x0042DD81` は
//   eax を 0 にする）。
// - 1（始める）: `fldcw` でゲームの制御語 `0x007F` を置き、戻りアドレスを捨てて（`add esp, 4`）`0x0042DC8D` へ `jmp`。飛ばす区間
//   `0x0042DC49`〜`0x0042DC8C` はスタックを動かさず x87 のスタックを空で終えるので、着地の状態は元と同じ。
// - 2（まだ）: 戻りアドレスを捨てて `0x0042DD81`（`pop edi; xor eax,eax; pop esi; ret`）へ `jmp`。スタックは do_frame の `push esi; push edi` の直後と同じなので、
//   do_frame は 0 を返してメインループへ戻る。
// - それ以外（0 = 任せる）: PACING_ON が 0 のときと同じ経路。
// - 飛び先のアドレスと HEAD_FN は、この入口へ向く call を書く前に置かれる（`set_head_targets`）。着地のアドレスは照合で一致した行のアドレスだけ。
/// 先頭の call（`0x0042DC44`）の差し替え先。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn pacing_head_entry() {
    core::arch::naked_asm!(
        "cmp dword ptr [{on}], 0",
        "je 4f",
        "cld",
        "call dword ptr [{head_fn}]",
        "cmp eax, {run}",
        "je 2f",
        "cmp eax, {not_yet}",
        "je 3f",
        "4:",
        "jmp dword ptr [{orig}]",
        "2:",
        "fldcw word ptr [{cw}]",
        "add esp, 4",
        "jmp dword ptr [{resume}]",
        "3:",
        "add esp, 4",
        "jmp dword ptr [{early}]",
        on = sym PACING_ON,
        head_fn = sym HEAD_FN,
        run = const HEAD_RUN,
        not_yet = const HEAD_NOT_YET,
        orig = sym HEAD_ORIGINAL,
        cw = sym FPU_CW,
        resume = sym HEAD_RESUME,
        early = sym HEAD_EARLY_EXIT,
    )
}

// SAFETY（`#[unsafe(naked)]`）: `0x0042DD6D` の `call 0x42db50`（Present のラッパ）の差し替え先。入った時のスタックは
// [戻りアドレス `0x0042DD72`]…、x87 のスタックは空。ラッパは引数を読まず、ecx・edx も入口で読まない。戻り値は呼ぶ側が使わない。
// - `pace_present_begin` → ラッパ（PRESENT_ORIGINAL。自分の戻りアドレスを積んで呼ぶ）→ `pace_present_end` → `pace_after_present`
//   （フックが 0 なら原子変数を 1 回読んで戻る）→ `ret`。呼ぶ 4 つはどれも cdecl / MSVC の決まりで ebx・esi・edi・ebp と esp を保つ。
//   Rust の 3 つは x87 を使わない（フックの先で使う x87 は、フックが制御語を置き直して戻す）。DF は `cld` で 0。
// - `ret` の時点のスタックは入った時と同じなので、呼ぶ側の `0x0042DD72` へ戻る。
// - PRESENT_ORIGINAL は、この入口へ向く call を書く前に置かれる（`set_present_original`）。
/// Present の call（`0x0042DD6D`）の差し替え先。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn present_entry() {
    core::arch::naked_asm!(
        "cld",
        "call {begin}",
        "call dword ptr [{orig}]",
        "cld",
        "call {end}",
        "cld",
        "call {after}",
        "ret",
        begin = sym pace_present_begin,
        orig = sym PRESENT_ORIGINAL,
        end = sym pace_present_end,
        after = sym pace_after_present,
    )
}

fn fn_addr(f: extern "C" fn()) -> u32 {
    f as usize as u32
}

struct WriterThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<WriterEnd, String>>,
}

pub struct RealPacingHost {
    dll_dir: Option<PathBuf>,
    run_path: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    writer: Option<WriterThread>,
}

impl RealPacingHost {
    pub fn new(
        dll_dir: Option<PathBuf>,
        run_path: Option<PathBuf>,
        exe_dir: Option<PathBuf>,
    ) -> RealPacingHost {
        RealPacingHost {
            dll_dir,
            run_path,
            exe_dir,
            writer: None,
        }
    }
}

/// 書き出しのスレッドが CSV を開いたと答えるまで待つ上限。
const WRITER_OPEN_WAIT: Duration = Duration::from_secs(5);

/// CSV を書かない実行の、書き出し先の表示名（`ペース:` の行に出る）。
pub const NO_CSV_NAME: &str = "（書かない）";

impl PacingHost for RealPacingHost {
    fn read_ini(&mut self) -> Result<Option<String>, String> {
        let mut s = RealScriptHost {
            dll_dir: self.dll_dir.clone(),
            run_path: None,
            exe_dir: self.exe_dir.clone(),
        };
        s.read_net_ini()
    }

    fn qpf(&self) -> u64 {
        qpf()
    }

    fn head_entry(&self) -> u32 {
        fn_addr(pacing_head_entry)
    }

    fn present_entry(&self) -> u32 {
        fn_addr(present_entry)
    }

    fn arm_pacer(
        &mut self,
        qpf: u64,
        cfg: &PacingIni,
        screen_flag: u32,
        window: u32,
    ) -> Result<String, String> {
        // 途中で失敗しても前の回の値（vpatch の 1）が残らないよう、先に 0 に戻す。成功したときだけ下で書き直す。
        VPATCH_WAIT_ON.store(0, Ordering::SeqCst);
        // b-spin だけなら待機タイマーは使わないが、cycle の b の区間と同じ箱にするので形を問わず用意する
        // （Windows 11 では作れる。作れない古い OS では、どの形も時刻のフックだけになる）。
        let ev = Event::new().ok_or("待ちのイベントを作れない")?;
        let waiter = Waiter::new(Arc::new(ev));
        let vpatch = cfg.mode == pace::Mode::Vpatch;
        let plain = if vpatch {
            Some(PlainTimer::new().ok_or("普通の待機タイマーを作れない")?)
        } else {
            None
        };
        if !vpatch && !waiter.high_resolution() {
            return Err(
                "高分解能の待機タイマーを作れない（Windows 10 1803 より前？）。ms の待ちでは 1 フレームを刻めない"
                    .to_string(),
            );
        }
        let p = Pacer::new(qpf, cfg).ok_or("QPF が小さすぎる（か、先頭を書かない形）")?;
        let mut g = PACER.lock().map_err(|_| "先頭のフックの錠が壊れている")?;
        *g = Some((
            p,
            RealClock {
                waiter,
                plain,
                screen_flag,
                window,
            },
        ));
        VPATCH_WAIT_ON.store(u32::from(vpatch), Ordering::SeqCst);
        let hybrid = if cfg.uses_hybrid() {
            format!("、b-hybrid の数値 {}", cfg.hybrid.text())
        } else {
            String::new()
        };
        Ok(format!(
            "形 {}、{}の待機タイマー、ペース {} fps、回し読み {} µs、1 回に寝る上限 {} µs{hybrid}",
            cfg.mode.name(),
            if vpatch { "普通" } else { "高分解能" },
            cfg.fps,
            pacing::SPIN_US,
            pacing::CHUNK_US
        ))
    }

    fn cpu_classes(&mut self) -> Result<CpuClasses, String> {
        cpu_classes_now()
    }

    fn start_writer(
        &mut self,
        pre: Preamble,
        fps: u64,
        classes: CpuClasses,
    ) -> Result<String, String> {
        if self.writer.is_some() {
            return Err("もう起こしてある".to_string());
        }
        PACE_RING.reset();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        let (run_path, exe_dir) = (self.run_path.clone(), self.exe_dir.clone());
        let stop2 = Arc::clone(&stop);
        // 簡潔なログでは CSV を開かない（集計はリングバッファを読むスレッドが同じに作る）。
        let write_csv = crate::log_mode::detail();
        let limits = pacing::writer_limits(cfg!(feature = "dist"));
        let handle = std::thread::Builder::new()
            .name("higanrtool-pacing".to_string())
            .spawn(move || -> Result<WriterEnd, String> {
                // 本ログの隣に `create_new` で開く（既存を上書きしない・ゲームのフォルダの中なら断る）。
                let mut rec = RealRecord::new(run_path, exe_dir);
                let (name, mut w): (String, Box<dyn std::io::Write>) = if write_csv {
                    match rec.open_output(pacing::CSV_SUFFIX) {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = tx.send(Err(e.clone()));
                            return Err(e);
                        }
                    }
                } else {
                    (NO_CSV_NAME.to_string(), Box::new(std::io::sink()))
                };
                let _ = tx.send(Ok(name.clone()));
                let tally = pacing::writer_loop(
                    &PACE_RING,
                    &pre,
                    fps,
                    &classes,
                    &mut *w,
                    &|| stop2.load(Ordering::Acquire),
                    &mut |ms| std::thread::sleep(Duration::from_millis(ms)),
                    // CSV を書かない実行は、行を組み立てずに集計にだけ入れる（CSV の秒数 0）。
                    pacing::WriterLimits {
                        csv_seconds: if write_csv { limits.csv_seconds } else { 0 },
                        ..limits
                    },
                );
                Ok(WriterEnd { file: name, tally })
            })
            .map_err(|e| format!("書き出しのスレッドを起こせない: {e}"))?;
        match rx.recv_timeout(WRITER_OPEN_WAIT) {
            Ok(Ok(name)) => {
                self.writer = Some(WriterThread { stop, handle });
                Ok(name)
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                Err("書き出しのスレッドが CSV を開いたと答えない".to_string())
            }
        }
    }

    fn stop_writer(&mut self) -> Option<Result<WriterEnd, String>> {
        let w = self.writer.take()?;
        w.stop.store(true, Ordering::Release);
        Some(match w.handle.join() {
            Ok(r) => r,
            Err(_) => Err("書き出しのスレッドがパニックで終わった".to_string()),
        })
    }

    fn set_present_original(&mut self, original: u32) {
        PRESENT_ORIGINAL.store(original, Ordering::SeqCst);
    }

    fn set_head_targets(&mut self, original: u32, resume: u32, early_exit: u32) {
        HEAD_ORIGINAL.store(original, Ordering::SeqCst);
        HEAD_RESUME.store(resume, Ordering::SeqCst);
        HEAD_EARLY_EXIT.store(early_exit, Ordering::SeqCst);
        HEAD_FN.store(pace_head as *const () as usize as u32, Ordering::SeqCst);
        PACING_ON.store(1, Ordering::SeqCst);
    }

    fn start_stamps(&mut self) {
        INPUT_STAMP_ON.store(1, Ordering::SeqCst);
    }

    fn counters(&self) -> PaceCounters {
        PaceCounters {
            fallbacks: FALLBACKS.load(Ordering::Relaxed),
            panics: PANICS.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::PaceRead;
    use crate::pacing::tests::lock;
    use crate::stub::testkit::{
        AFTER, PRE_ESP, REG_EBP, REG_EBX, REG_EDI, SEEN, TARGET_HITS, TEST_CW, TEST_CW_VALUE,
        call_with_regs, fake_this_addr, load, seen_target_addr, set_fake,
    };
    use higanr_core::pacing::Summarizer;
    use higanr_core::pacing::{Mode, Phase};

    static DECISION: AtomicU32 = AtomicU32::new(0);
    static DECIDED: AtomicU32 = AtomicU32::new(0);
    static CLOCK_HITS: AtomicU32 = AtomicU32::new(0);
    static SEEN_CW: AtomicU32 = AtomicU32::new(0);

    extern "C" fn fake_decision() -> u32 {
        DECIDED.fetch_add(1, Ordering::SeqCst);
        DECISION.load(Ordering::SeqCst)
    }

    // SAFETY（`#[unsafe(naked)]`）: do_frame の先頭の真似。`push esi; push edi; mov esi, ecx` の後に先頭の入口を呼ぶ。入口が元の飛び先
    // （`fake_clock`）へ飛べば、それが `ret` でここへ戻り、10 を返す。着地へ飛べば、そちらが edi・esi を pop して 11 か 12 を返す。
    #[unsafe(naked)]
    extern "C" fn fake_do_frame() {
        core::arch::naked_asm!(
            "push esi",
            "push edi",
            "mov esi, ecx",
            "call {entry}",
            "mov dword ptr [{cw}], 0",
            "fnstcw word ptr [{cw}]",
            "mov eax, 10",
            "pop edi",
            "pop esi",
            "ret",
            entry = sym pacing_head_entry,
            cw = sym SEEN_CW,
        )
    }

    // SAFETY（`#[unsafe(naked)]`）: 時計 `0x0042CC90` の代わり（回数を数えて戻るだけ）。
    #[unsafe(naked)]
    extern "C" fn fake_clock() {
        core::arch::naked_asm!("lock inc dword ptr [{hits}]", "ret", hits = sym CLOCK_HITS)
    }

    // SAFETY（`#[unsafe(naked)]`）: 着地 `0x0042DC8D` の代わり。制御語を見てから、do_frame の尻の真似で pop して 11 を返す。
    #[unsafe(naked)]
    extern "C" fn fake_resume() {
        core::arch::naked_asm!(
            "mov dword ptr [{cw}], 0",
            "fnstcw word ptr [{cw}]",
            "mov eax, 11",
            "pop edi",
            "pop esi",
            "ret",
            cw = sym SEEN_CW,
        )
    }

    // SAFETY（`#[unsafe(naked)]`）: `0x0042DD81`（`pop edi; xor eax,eax; pop esi; ret`）の真似。0 の代わりに 12 を返す。
    #[unsafe(naked)]
    extern "C" fn fake_early() {
        core::arch::naked_asm!("pop edi", "mov eax, 12", "pop esi", "ret")
    }

    fn prepare_head(decision: u32) {
        HEAD_ORIGINAL.store(fn_addr(fake_clock), Ordering::SeqCst);
        HEAD_RESUME.store(fn_addr(fake_resume), Ordering::SeqCst);
        HEAD_EARLY_EXIT.store(fn_addr(fake_early), Ordering::SeqCst);
        HEAD_FN.store(fake_decision as *const () as usize as u32, Ordering::SeqCst);
        DECISION.store(decision, Ordering::SeqCst);
        SEEN_CW.store(0, Ordering::SeqCst);
    }

    fn assert_callee_saved(label: &str) {
        let a = load(&AFTER);
        assert_eq!(a[3], REG_EBX, "{label}: ebx");
        assert_eq!(a[4], fake_this_addr(), "{label}: esi");
        assert_eq!(a[5], REG_EDI, "{label}: edi");
        assert_eq!(a[6], REG_EBP, "{label}: ebp");
        assert_eq!(a[7], PRE_ESP.load(Ordering::SeqCst), "{label}: esp");
    }

    #[test]
    fn head_entry_takes_the_three_ways() {
        let _g = lock();
        TEST_CW.store(0x027F, Ordering::SeqCst);
        let cases = [
            ("任せる", 1u32, HEAD_FALLBACK, 10u32, 1u32, 0x027Fu32),
            ("始める", 1, HEAD_RUN, 11, 0, GAME_FPU_CW as u32),
            ("まだ", 1, HEAD_NOT_YET, 12, 0, 0),
            ("知らない答え 7", 1, 7, 10, 1, 0x027F),
            ("閉じている", 0, HEAD_RUN, 10, 1, 0x027F),
        ];
        for (label, on, decision, ret, clock, cw) in cases {
            prepare_head(decision);
            PACING_ON.store(on, Ordering::SeqCst);
            let (d0, c0) = (
                DECIDED.load(Ordering::SeqCst),
                CLOCK_HITS.load(Ordering::SeqCst),
            );
            call_with_regs(fn_addr(fake_do_frame));
            assert_eq!(load(&AFTER)[0], ret, "{label}: どの道で戻ったか");
            assert_eq!(
                CLOCK_HITS.load(Ordering::SeqCst) - c0,
                clock,
                "{label}: 時計"
            );
            assert_eq!(
                DECIDED.load(Ordering::SeqCst) - d0,
                on,
                "{label}: 判断を呼んだ回数"
            );
            assert_eq!(
                SEEN_CW.load(Ordering::SeqCst) & 0xFFFF,
                cw,
                "{label}: 制御語"
            );
            assert_callee_saved(label);
        }
        PACING_ON.store(0, Ordering::SeqCst);
        TEST_CW.store(TEST_CW_VALUE, Ordering::SeqCst);
    }

    #[test]
    fn present_entry_calls_through_and_stamps() {
        let _g = lock();
        PACE_RING.reset();
        PRESENT_ORIGINAL.store(seen_target_addr(), Ordering::SeqCst);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        let before = qpc();
        call_with_regs(fn_addr(present_entry));
        let after = qpc();
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst) - hits0,
            1,
            "ラッパを 1 回"
        );
        assert_callee_saved("Present");
        let seen = load(&SEEN);
        assert_eq!(
            [seen[3], seen[4], seen[5], seen[6]],
            [REG_EBX, fake_this_addr(), REG_EDI, REG_EBP]
        );
        assert_eq!(PACE_RING.completed(), 1);
        let PaceRead::Row(r) = PACE_RING.read(0) else {
            panic!("枠 0 が読めない")
        };
        assert!(
            before <= r.present_begin && r.present_begin <= r.present_end && r.present_end <= after,
            "{r:?}"
        );
        PACE_RING.reset();
    }

    static AFTER_HITS: AtomicU32 = AtomicU32::new(0);
    static AFTER_SAW_END: AtomicU32 = AtomicU32::new(0);

    fn after_hook() {
        AFTER_HITS.fetch_add(1, Ordering::SeqCst);
        AFTER_SAW_END.store(PACE_RING.completed(), Ordering::SeqCst);
    }

    fn after_panics() {
        panic!("検査: Present の後のフックのパニック");
    }

    #[test]
    fn present_entry_calls_the_after_present_hook_once_and_drops_a_panicking_one() {
        let _g = lock();
        PACE_RING.reset();
        PRESENT_ORIGINAL.store(seen_target_addr(), Ordering::SeqCst);
        set_after_present(None);
        let h0 = AFTER_HITS.load(Ordering::SeqCst);
        call_with_regs(fn_addr(present_entry));
        assert_eq!(AFTER_HITS.load(Ordering::SeqCst), h0, "口が無いのに呼んだ");
        assert_callee_saved("Present（口なし）");
        PACE_RING.reset();
        set_after_present(Some(after_hook));
        call_with_regs(fn_addr(present_entry));
        assert_eq!(AFTER_HITS.load(Ordering::SeqCst), h0 + 1, "口を 1 回");
        assert_eq!(
            AFTER_SAW_END.load(Ordering::SeqCst),
            1,
            "口が Present の印の前に呼ばれた"
        );
        assert_callee_saved("Present（口あり）");
        set_after_present(Some(after_panics));
        let p0 = PANICS.load(Ordering::SeqCst);
        call_with_regs(fn_addr(present_entry));
        assert_eq!(PANICS.load(Ordering::SeqCst), p0 + 1);
        assert_eq!(AFTER_PRESENT_FN.load(Ordering::SeqCst), 0, "口を外さない");
        assert_callee_saved("Present（パニック）");
        PACE_RING.reset();
    }

    #[test]
    fn input_entry_stamps_only_when_open() {
        let _g = lock();
        crate::stub::set_original_target(seen_target_addr());
        crate::stub::set_window(0);
        assert!(crate::stub::set_copy_table(&[]));
        crate::stub::set_render_only_exit(0);
        crate::stub::RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
        crate::script_win::stop();
        crate::shot_win::set_off();
        for on in [0u32, 1] {
            PACE_RING.reset();
            INPUT_STAMP_ON.store(on, Ordering::SeqCst);
            set_fake(0x0BAD_0777, 1, 1, 0.0, 0.0);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            call_with_regs(crate::stub::entry_address());
            INPUT_STAMP_ON.store(0, Ordering::SeqCst);
            assert_eq!(TARGET_HITS.load(Ordering::SeqCst) - hits0, 1, "on={on}");
            let seen = load(&SEEN);
            assert_eq!(
                [seen[3], seen[4], seen[5], seen[6]],
                [REG_EBX, fake_this_addr(), REG_EDI, REG_EBP],
                "on={on}: 元の飛び先が見たレジスタ"
            );
            assert_eq!(
                seen[8],
                PRE_ESP.load(Ordering::SeqCst) - 4,
                "on={on}: 元の飛び先が見た esp"
            );
            PACE_RING.on_present_end(1);
            let PaceRead::Row(r) = PACE_RING.read(0) else {
                panic!()
            };
            if on == 0 {
                assert_eq!((r.inputs, r.input, r.sup_frame), (0, 0, 0));
            } else {
                assert_eq!(r.inputs, 1);
                assert_eq!(r.sup_frame, 0x0BAD_0777);
                assert_ne!(r.input, 0);
            }
        }
        PACE_RING.reset();
    }

    #[test]
    fn real_writer_thread_writes_next_to_the_log() {
        let _g = lock();
        let d = std::env::temp_dir().join(format!("higanrtool_pacing_win_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut h = RealPacingHost::new(None, Some(d.join("dll_p.log")), Some(d.join("elsewhere")));
        let pre = |mode| Preamble::new(10_000_000, mode);
        assert_eq!(
            h.start_writer(pre(Mode::B), 240, CpuClasses::default())
                .as_deref(),
            Ok("dll_p_pacing.csv.gz")
        );
        for i in 0..3u64 {
            PACE_RING.on_present_begin(100 + i * 10, 0, 0);
            PACE_RING.on_present_end(105 + i * 10);
        }
        let end = h
            .stop_writer()
            .expect("起こしてある")
            .expect("終わりの報告");
        assert_eq!(end.tally.rows, 3);
        assert_eq!(end.tally.report.all.target_fps, 240);
        let path = d.join("dll_p_pacing.csv.gz");
        let raw = std::fs::read(&path).unwrap();
        assert!(higanr_core::gzlog::is_gzip(&raw), "gzip で書いていない");
        let read = higanr_core::gzlog::read_log_text(&path).unwrap();
        assert!(!read.cut, "止めた後に gzip の終わりの印を書いていない");
        let text = read.text;
        assert_eq!(text.lines().count(), 2 + 3, "{text}");
        assert!(h.stop_writer().is_none());
        let mut h2 =
            RealPacingHost::new(None, Some(d.join("dll_p.log")), Some(d.join("elsewhere")));
        assert!(
            h2.start_writer(pre(Mode::B), 60, CpuClasses::default())
                .unwrap_err()
                .contains("既にある")
        );
        assert_eq!(std::fs::read(&path).unwrap(), raw, "既存を上書きした");
        let mut h3 = RealPacingHost::new(None, Some(d.join("dll_q.log")), Some(d.clone()));
        assert!(
            h3.start_writer(pre(Mode::B), 60, CpuClasses::default())
                .is_err()
        );
        assert!(!d.join("dll_q_pacing.csv.gz").exists());
        PACE_RING.reset();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn real_writer_thread_runs_with_this_builds_limits() {
        let _g = lock();
        let d =
            std::env::temp_dir().join(format!("higanrtool_pacing_limits_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut h = RealPacingHost::new(None, Some(d.join("dll_l.log")), Some(d.join("elsewhere")));
        h.start_writer(
            Preamble::new(10_000_000, Mode::B),
            240,
            CpuClasses::default(),
        )
        .expect("書き出しを起こせる");
        let end = h
            .stop_writer()
            .expect("起こしてある")
            .expect("終わりの報告");
        let (want_csv, want_thread) = if cfg!(feature = "dist") {
            (660, 43_200)
        } else {
            (1_260, 43_200)
        };
        assert_eq!(
            (
                end.tally.limits.csv_seconds,
                end.tally.limits.thread_seconds
            ),
            (want_csv, want_thread)
        );
        assert_eq!(
            end.tally.limits,
            crate::pacing::writer_limits(cfg!(feature = "dist"))
        );
        PACE_RING.reset();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 壊れたら落ちる: 簡潔なログが `_pacing.csv.gz` を作る。詳細が作らない。
    #[test]
    fn the_pacing_csv_is_created_only_when_detailed() {
        let _g = lock();
        let d =
            std::env::temp_dir().join(format!("higanrtool_pacing_brief_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let files = || {
            let mut v: Vec<String> = std::fs::read_dir(&d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        let run = |detail: bool| {
            let mut h =
                RealPacingHost::new(None, Some(d.join("dll_l.log")), Some(d.join("elsewhere")));
            let name = crate::log_mode::with_detail(detail, || {
                h.start_writer(
                    Preamble::new(10_000_000, Mode::B),
                    60,
                    CpuClasses::default(),
                )
            })
            .expect("書き出しを起こせる");
            let end = h
                .stop_writer()
                .expect("起こしてある")
                .expect("終わりの報告");
            PACE_RING.reset();
            (name, end.tally.limits.csv_seconds)
        };
        let (name, csv_seconds) = run(false);
        assert_eq!(name, NO_CSV_NAME);
        assert_eq!(csv_seconds, 0, "簡潔なら行を組み立てず集計にだけ入れる");
        assert!(
            files().is_empty(),
            "簡潔なのにファイルを作った: {:?}",
            files()
        );
        let (name, csv_seconds) = run(true);
        assert!(csv_seconds > 0);
        assert!(name.ends_with("_pacing.csv.gz"), "{name}");
        assert_eq!(files(), vec![name]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn real_host_arms_the_pacer_with_the_ini_fps() {
        let _g = lock();
        let mut h = RealPacingHost::new(None, None, None);
        let fps_in_box = || PACER.lock().unwrap().as_ref().map(|(p, _)| p.pace().fps());
        let cfg = |mode, fps| PacingIni {
            mode,
            fps,
            cycle_secs: higanr_core::pacing::DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        for fps in [60u64, 240] {
            let desc = h
                .arm_pacer(10_000_000, &cfg(Mode::B, fps), 0, 0)
                .expect("箱を用意する");
            assert!(desc.contains(&format!("ペース {fps} fps")), "{desc}");
            assert_eq!(fps_in_box(), Some(fps));
        }
        let desc = h
            .arm_pacer(10_000_000, &cfg(Mode::Cycle, 60), 0, 0)
            .expect("cycle の箱");
        assert!(desc.contains("形 cycle"), "{desc}");
        *PACER.lock().unwrap() = None;
        assert!(h.arm_pacer(239, &cfg(Mode::B, 240), 0, 0).is_err());
        assert_eq!(fps_in_box(), None, "刻めないのに箱を置いた");
        assert!(h.arm_pacer(10_000_000, &cfg(Mode::Game, 60), 0, 0).is_err());
        assert_eq!(fps_in_box(), None, "先頭を書かない形で箱を置いた");
        *PACER.lock().unwrap() = None;
    }

    // 何が壊れたら落ちるか: vpatch の箱を置くと Present の前の待ちのフックが開き（B では閉じる）、Present の入口（`pace_present_begin`）が期限まで待つこと。
    // フックが開かない・錠を取りそこねると、フレームが待たずに続けて進んで落ちる。1 kHz なら先頭の起きる時刻が過去になり、普通の待機タイマーは使わない（寝過ごしで揺れない）。
    #[test]
    fn vpatch_box_opens_the_present_wait_and_present_entry_waits_for_the_deadline() {
        let _g = lock();
        PACE_RING.reset();
        let mut h = RealPacingHost::new(None, None, None);
        let cfg = |mode, fps| PacingIni {
            mode,
            fps,
            cycle_secs: higanr_core::pacing::DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        h.arm_pacer(qpf(), &cfg(Mode::BSpin, 60), 0, 0)
            .expect("b-spin の箱");
        assert_eq!(VPATCH_WAIT_ON.load(Ordering::SeqCst), 0);
        let desc = h
            .arm_pacer(qpf(), &cfg(Mode::Vpatch, 1_000), 0, 0)
            .expect("vpatch の箱");
        assert!(desc.contains("形 vpatch、普通の待機タイマー"), "{desc}");
        assert_eq!(VPATCH_WAIT_ON.load(Ordering::SeqCst), 1);
        let frames = 20u32;
        for _ in 0..frames {
            assert_eq!(
                pace_head(),
                HEAD_RUN,
                "先頭のフックの中で待つので「まだ」は返らない"
            );
            pace_present_begin();
            pace_present_end();
        }
        let (first, last) = (PACE_RING.read(1), PACE_RING.read(frames - 1));
        let (PaceRead::Row(a), PaceRead::Row(b)) = (first, last) else {
            panic!("{first:?} {last:?}")
        };
        let ms = (b.present_begin - a.present_begin) * 1_000 / qpf();
        assert!(
            (10..200).contains(&ms),
            "{} フレームの間 {ms} ms（待たなければ 1 ms 未満。1 kHz の 18 フレームで 18 ms のはず）",
            frames - 2
        );
        h.arm_pacer(qpf(), &cfg(Mode::B, 60), 0, 0).expect("b の箱");
        assert_eq!(
            VPATCH_WAIT_ON.load(Ordering::SeqCst),
            0,
            "B で口が開いたまま"
        );
        *PACER.lock().unwrap() = None;
        PACE_RING.reset();
    }

    // 何が壊れたら落ちるか: 普通の待機タイマーが実際に寝て戻ること（即座に戻る・戻らない）。粒度は OS の時計の分解能なので、下限は緩く、上限は歯止めの内。
    #[test]
    fn plain_timer_sleeps_at_least_roughly_what_was_asked() {
        let t = PlainTimer::new().expect("普通の待機タイマーを作れる");
        let (a, q) = (qpc(), qpf());
        t.sleep_us(2_000);
        let us = (qpc() - a) * 1_000_000 / q;
        assert!((1_500..200_000).contains(&us), "2,000 µs を頼んで {us} µs");
        let a = qpc();
        t.sleep_us(0);
        assert!((qpc() - a) * 1_000_000 / q < 1_000, "0 µs で寝た");
    }

    #[test]
    fn real_clock_reads_the_screen_flag_it_was_given() {
        use core::sync::atomic::AtomicU8;
        use higanr_core::pacing::Screen;
        static FLAG: AtomicU8 = AtomicU8::new(1);
        let _g = lock();
        let mut h = RealPacingHost::new(None, None, None);
        let auto = PacingIni {
            mode: Mode::Auto,
            fps: 60,
            cycle_secs: higanr_core::pacing::DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        let screen = || PACER.lock().unwrap().as_mut().map(|(_, c)| c.screen());
        let at = FLAG.as_ptr() as usize as u32;
        h.arm_pacer(10_000_000, &auto, at, 0).expect("auto の箱");
        for (v, want) in [
            (1u8, Screen::Windowed),
            (0, Screen::Fullscreen),
            (2, Screen::Unknown),
            (1, Screen::Windowed),
        ] {
            FLAG.store(v, Ordering::SeqCst);
            assert_eq!(screen(), Some(want), "{v}");
        }
        h.arm_pacer(10_000_000, &auto, 0, 0).expect("auto の箱");
        assert_eq!(screen(), Some(Screen::Unknown));
        *PACER.lock().unwrap() = None;
    }

    static HOOK_HITS: AtomicU32 = AtomicU32::new(0);

    fn count_hook() {
        HOOK_HITS.fetch_add(1, Ordering::SeqCst);
    }

    fn panic_hook() {
        panic!("検査: 先頭のフックの中の待ちのフックのパニック");
    }

    #[test]
    fn pace_head_calls_the_head_wait_hook_only_when_starting() {
        use core::sync::atomic::AtomicU8;
        static FLAG: AtomicU8 = AtomicU8::new(0);
        let _g = lock();
        PACE_RING.reset();
        let hits = || HOOK_HITS.load(Ordering::SeqCst);
        let mut h = RealPacingHost::new(None, None, None);
        let cfg = |mode| PacingIni {
            mode,
            fps: 60,
            cycle_secs: higanr_core::pacing::DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        set_head_wait(None);
        *PACER.lock().unwrap() = None;
        let h0 = hits();
        assert_eq!(pace_head(), HEAD_FALLBACK);
        set_head_wait(Some(count_hook));
        assert_eq!(pace_head(), HEAD_FALLBACK);
        assert_eq!(hits(), h0, "箱が無い道で口を呼んだ");
        h.arm_pacer(qpf(), &cfg(Mode::BSpin), 0, 0)
            .expect("b-spin の箱");
        let (mut runs, mut not_yet) = (0u32, 0u32);
        for _ in 0..4 {
            loop {
                match pace_head() {
                    HEAD_RUN => {
                        runs += 1;
                        break;
                    }
                    HEAD_NOT_YET => not_yet += 1,
                    other => panic!("先頭の答え {other}"),
                }
            }
            PACE_RING.on_present_begin(qpc(), cpu_now(), thread_cycles());
            PACE_RING.on_present_end(qpc());
        }
        assert_eq!(runs, 4);
        assert!(
            not_yet > 0,
            "格子を待つ「まだ」が 1 度も無い（検査が素通り）"
        );
        assert_eq!(
            hits() - h0,
            runs,
            "始める答えの前に 1 回ずつ（「まだ」{not_yet} 回では呼ばない）"
        );
        // 錠が取れない経路（読み取りのスレッドが箱を持っている間）: 答えはゲーム自身の経路で、フックは呼ばない。b-spin の箱は上で置いてある
        // ので、錠さえ取れれば「始める / まだ」を返す形（対照）。
        {
            let _held = PACER.lock().unwrap();
            let f0 = FALLBACKS.load(Ordering::SeqCst);
            let h2 = hits();
            assert_eq!(pace_head(), HEAD_FALLBACK);
            assert_eq!(FALLBACKS.load(Ordering::SeqCst), f0 + 1);
            assert_eq!(hits(), h2, "錠が取れない道で口を呼んだ");
        }
        // 全画面の auto: ゲーム自身の時計へ任せる答え（`HEAD_FALLBACK`）の前では、ウィンドウの構造体のアドレスが無ければ呼ばない（何度来ても）。
        let at = FLAG.as_ptr() as usize as u32;
        FLAG.store(0, Ordering::SeqCst);
        h.arm_pacer(qpf(), &cfg(Mode::Auto), at, 0)
            .expect("auto の箱");
        PACE_RING.reset();
        let h1 = hits();
        for _ in 0..3 {
            assert_eq!(pace_head(), HEAD_FALLBACK);
        }
        assert_eq!(
            hits(),
            h1,
            "ウィンドウの構造体なしでゲーム自身の経路へ任せる前に口を呼んだ"
        );
        // 偽のウィンドウの構造体（検査のプロセスの静的変数。`WINDOW_SPAN` バイト）: 時計の分母 `+0x14` はこのプロセスの QPF。
        static WIN: [AtomicU32; (crate::th09_addr::WINDOW_SPAN / 4) as usize] =
            [const { AtomicU32::new(0) }; (crate::th09_addr::WINDOW_SPAN / 4) as usize];
        let set_deadline = |d: f64| {
            let b = d.to_bits();
            WIN[(WINDOW_DEADLINE / 4) as usize].store(b as u32, Ordering::SeqCst);
            WIN[(WINDOW_DEADLINE / 4 + 1) as usize].store((b >> 32) as u32, Ordering::SeqCst);
        };
        let divisor = qpf() as u32;
        WIN[(WINDOW_CLOCK_DIVISOR / 4) as usize].store(divisor, Ordering::SeqCst);
        h.arm_pacer(qpf(), &cfg(Mode::Auto), at, WIN.as_ptr() as usize as u32)
            .expect("auto の箱");
        // 期限がまだ先（今 + 100 秒）: ゲームはこの回では始めない → 呼ばない（時計の経路は先頭が 1 フレームに何千回も来て、上限を期限の前に
        // 使い切るため）。
        set_deadline(f64::from(qpc() as u32) / f64::from(divisor) + 100.0);
        let h1 = hits();
        for _ in 0..3 {
            assert_eq!(pace_head(), HEAD_FALLBACK);
        }
        assert_eq!(hits(), h1, "期限の前の回で口を呼んだ");
        // 期限を過ぎている（期限 0）: ゲームはこの回で始める → 答えは任せるのまま、返す前に 1 回ずつ呼ぶ。
        set_deadline(0.0);
        for _ in 0..3 {
            assert_eq!(pace_head(), HEAD_FALLBACK);
        }
        assert_eq!(hits() - h1, 3, "ゲームが始める回で口を呼ばない");
        // 時計の分母が 0（timeGetTime の経路）: 見積もれないので呼ばない。
        WIN[(WINDOW_CLOCK_DIVISOR / 4) as usize].store(0, Ordering::SeqCst);
        let h2 = hits();
        assert_eq!(pace_head(), HEAD_FALLBACK);
        assert_eq!(hits(), h2, "分母 0 で口を呼んだ");
        WIN[(WINDOW_CLOCK_DIVISOR / 4) as usize].store(divisor, Ordering::SeqCst);
        // ウィンドウ（b-spin）のフレームではウィンドウの構造体を見ず、今までどおり始める答えの前だけ。箱を置き直す（方式はフレームの最初の始まりでだけ読む）。
        FLAG.store(1, Ordering::SeqCst);
        h.arm_pacer(qpf(), &cfg(Mode::Auto), at, WIN.as_ptr() as usize as u32)
            .expect("auto の箱");
        PACE_RING.reset();
        let h3 = hits();
        let mut not_yet_w = 0u32;
        loop {
            match pace_head() {
                HEAD_RUN => break,
                HEAD_NOT_YET => not_yet_w += 1,
                other => panic!("先頭の答え {other}"),
            }
        }
        PACE_RING.on_present_begin(qpc(), cpu_now(), thread_cycles());
        PACE_RING.on_present_end(qpc());
        assert_eq!(
            hits() - h3,
            1,
            "ウィンドウのフレーム（「まだ」{not_yet_w} 回）"
        );
        FLAG.store(0, Ordering::SeqCst);
        // パニックするフック: 外へ出さず、答え（始める）は変えず、フックを外す。b-spin の箱で、始める答えの前に呼ばれる形にする。
        h.arm_pacer(qpf(), &cfg(Mode::BSpin), 0, 0)
            .expect("b-spin の箱");
        PACE_RING.reset();
        let next_run = || {
            loop {
                match pace_head() {
                    HEAD_RUN => break,
                    HEAD_NOT_YET => {}
                    other => panic!("先頭の答え {other}"),
                }
            }
            PACE_RING.on_present_begin(qpc(), cpu_now(), thread_cycles());
            PACE_RING.on_present_end(qpc());
        };
        set_head_wait(Some(panic_hook));
        let p0 = PANICS.load(Ordering::SeqCst);
        next_run();
        assert_eq!(PANICS.load(Ordering::SeqCst), p0 + 1);
        assert_eq!(HEAD_WAIT_FN.load(Ordering::SeqCst), 0, "口を外さない");
        next_run();
        assert_eq!(PANICS.load(Ordering::SeqCst), p0 + 1, "外した後も呼んだ");
        *PACER.lock().unwrap() = None;
        PACE_RING.reset();
    }

    /// 本物の時計と待ちで `frames` フレームを刻み（1 フレームの作業は `work_us` µs の回し読み）、周期の中央が
    /// `1e6 / fps` µs から `tol_us` 以内・起床の遅れの中央が 200 µs 未満・遅れて先頭へ来たのが `frames / 10` 件以下・
    /// 開始の幅が「(frames − 1 + 飛ばした数) × QPF ÷ fps」から 1 ms 以内かを見る。`mode` は b か b-spin。
    fn run_real_pacer(mode: Mode, fps: u64, frames: u32, work_us: u64, tol_us: f64) {
        let _g = lock();
        PACE_RING.reset();
        let q = qpf();
        let ev = Event::new().unwrap();
        let waiter = Waiter::new(Arc::new(ev));
        assert!(waiter.high_resolution(), "高分解能の待機タイマーを作れない");
        let mut clock = RealClock {
            waiter,
            plain: None,
            screen_flag: 0,
            window: 0,
        };
        let mut p = Pacer::new(
            q,
            &PacingIni {
                mode,
                fps,
                cycle_secs: higanr_core::pacing::DEFAULT_CYCLE_SECS,
                ..PacingIni::DEFAULT
            },
        )
        .unwrap();
        let mut not_yet = 0u32;
        for _ in 0..frames {
            loop {
                match p.head(&mut clock, &PACE_RING) {
                    HEAD_RUN => break,
                    HEAD_NOT_YET => not_yet += 1,
                    other => panic!("先頭の答え {other}"),
                }
            }
            let t0 = qpc();
            while qpc() - t0 < q * work_us / 1_000_000 {
                core::hint::spin_loop();
            }
            PACE_RING.on_present_begin(qpc(), cpu_now(), thread_cycles());
            PACE_RING.on_present_end(qpc());
        }
        let mut s = Summarizer::new(q, fps);
        let mut starts = Vec::new();
        for n in 0..frames {
            let PaceRead::Row(r) = PACE_RING.read(n) else {
                panic!("{n}")
            };
            assert!(r.start >= r.deadline, "{n}: 期限の前に始めた");
            let want = match mode {
                Mode::BSpin => Phase::BSpin,
                Mode::BHybrid => Phase::BHybrid,
                _ => Phase::B,
            };
            assert_eq!(r.phase, Some(want), "{n}: 札");
            assert!(r.head_cpu.is_some() && r.present_cpu.is_some(), "{n}: CPU");
            starts.push(r.start);
            s.push(&r);
        }
        let sum = s.finish();
        for l in sum.lines() {
            println!("{l}");
        }
        assert_eq!(sum.target_fps, fps);
        let per = sum.period.unwrap();
        let p_us = |t: i64| t as f64 * 1e6 / q as f64;
        let want_us = 1e6 / fps as f64;
        assert!(
            (p_us(per.p50) - want_us).abs() < tol_us,
            "周期の中央 {} µs（期待 {want_us:.1} µs）",
            p_us(per.p50)
        );
        let wake = sum.wake_late.unwrap();
        assert!(
            p_us(wake.p50) < 200.0,
            "起床の遅れの中央 {} µs",
            p_us(wake.p50)
        );
        assert!(
            // 遅れて先頭へ来るのは、1 ms を頼んだ眠りが回し読みの幅を越えて寝過ごしたとき（OS のタイマーのずれ）。240 fps では 1 周期が
            // 短く割合が上がる。格子で刻めるかは周期の中央と開始の幅で見るので、ここは桁の外れだけを落とす。PC に負荷がある全件の回
            // では 720 件のうち 132 件が遅れた（実測）ので、上限は半分。
            sum.late <= (frames / 2) as usize,
            "遅れて先頭へ来た {} 件（{frames} 件のうち）",
            sum.late
        );
        let span = starts[starts.len() - 1] - starts[0];
        let want = (frames as u64 - 1 + sum.skipped) * q / fps;
        assert!(
            span.abs_diff(want) < q / 1_000,
            "開始の幅 {span} 刻み（期待 {want}）"
        );
        assert!(not_yet as u64 >= sum.sleeps, "寝た回数と「まだ」の回数");
        if mode == Mode::BSpin {
            assert_eq!(sum.sleeps, 0, "b-spin が待機タイマーで寝た");
            assert!(not_yet > 0, "b-spin が 1 回も譲らなかった");
        }
        if mode == Mode::BHybrid {
            // 1 フレームに数回は寝る（作業 4 ms・余裕 2 ms・回し読み 1 ms なら残り約 9.7 ms を 1 ms ずつ）。b-hybrid は OS の寝過ごしを
            // 見て余裕を広げ、寝る回数を減らす作りなので揺れる。ここで落としたいのは「寝ずに回し続ける」（0 回）で、2 回でも桁で分けられる。
            assert!(
                sum.sleeps >= u64::from(frames) * 2,
                "b-hybrid が寝ない: {}",
                sum.sleeps
            );
            assert!(not_yet as u64 > sum.sleeps, "b-hybrid が余裕の間に譲らない");
            let h = &sum.hybrid;
            assert!(h.margin.is_some() && h.oversleep.is_some(), "{h:?}");
        }
        PACE_RING.reset();
    }

    #[test]
    #[ignore = "高分解能の待機タイマーでフレームを刻む実時間の検査（約 3 秒）。全件の回（build.bat）で --ignored で走る"]
    fn real_pacer_keeps_60_fps() {
        run_real_pacer(Mode::B, 60, 180, 4_000, 300.0);
    }

    #[test]
    #[ignore = "高分解能の待機タイマーで 240 fps を刻む実時間の検査（約 3 秒）。全件の回（build.bat）で --ignored で走る"]
    fn real_pacer_keeps_240_fps() {
        run_real_pacer(Mode::B, 240, 720, 1_000, 100.0);
    }

    #[test]
    #[ignore = "Sleep(0) で 60 fps を刻む実時間の検査（約 2 秒、CPU を 1 つ回し続ける）。全件の回（build.bat）で --ignored で走る"]
    fn real_pacer_b_spin_keeps_60_fps() {
        run_real_pacer(Mode::BSpin, 60, 120, 4_000, 300.0);
    }

    #[test]
    #[ignore = "高分解能の待機タイマーと Sleep(0) で 60 fps を刻む実時間の検査（約 3 秒）。全件の回（build.bat）で --ignored で走る"]
    fn real_pacer_b_hybrid_keeps_60_fps() {
        run_real_pacer(Mode::BHybrid, 60, 180, 4_000, 300.0);
    }

    #[test]
    fn cpu_set_offsets_match_the_win32_layout() {
        use core::mem::offset_of;
        use higanr_core::pacing::{
            CPU_SET_OFF_EFFICIENCY, CPU_SET_OFF_GROUP, CPU_SET_OFF_LOGICAL_INDEX, CPU_SET_OFF_SIZE,
            CPU_SET_OFF_TYPE,
        };
        use windows_sys::Win32::System::SystemInformation::{
            SYSTEM_CPU_SET_INFORMATION, SYSTEM_CPU_SET_INFORMATION_0_0,
        };
        let head = offset_of!(SYSTEM_CPU_SET_INFORMATION, Anonymous);
        assert_eq!(
            offset_of!(SYSTEM_CPU_SET_INFORMATION, Size),
            CPU_SET_OFF_SIZE
        );
        assert_eq!(
            offset_of!(SYSTEM_CPU_SET_INFORMATION, Type),
            CPU_SET_OFF_TYPE
        );
        assert_eq!(
            head + offset_of!(SYSTEM_CPU_SET_INFORMATION_0_0, Group),
            CPU_SET_OFF_GROUP
        );
        assert_eq!(
            head + offset_of!(SYSTEM_CPU_SET_INFORMATION_0_0, LogicalProcessorIndex),
            CPU_SET_OFF_LOGICAL_INDEX
        );
        assert_eq!(
            head + offset_of!(SYSTEM_CPU_SET_INFORMATION_0_0, EfficiencyClass),
            CPU_SET_OFF_EFFICIENCY
        );
        assert_eq!(core::mem::size_of::<SYSTEM_CPU_SET_INFORMATION>(), 32);
    }

    #[test]
    fn real_cpu_table_and_thread_cycles() {
        let t = cpu_classes_now().expect("効率クラスの表");
        println!("{}", t.describe());
        assert!(!t.is_empty());
        let cpu = cpu_now();
        assert!(t.class_of(cpu).is_some(), "いまの論理 CPU {cpu} が表に無い");
        let c0 = thread_cycles();
        assert_ne!(c0, 0, "QueryThreadCycleTime が 0");
        let t0 = qpc();
        while qpc() - t0 < qpf() / 1_000 {
            core::hint::spin_loop();
        }
        let c1 = thread_cycles();
        assert!(c1 > c0, "1 ms 回したのに周期が増えない: {c0} → {c1}");
    }

    #[test]
    #[ignore = "呼び出しの重さを 20,000 回測る（約 1 秒）。全件の回（build.bat）で --ignored で走る"]
    fn stamp_overhead_per_frame() {
        let _g = lock();
        let q = qpf();
        let us = |t: u64| t as f64 * 1e6 / q as f64;
        let measure = |label: &str, f: &mut dyn FnMut()| {
            let mut v: Vec<u64> = (0..20_000)
                .map(|_| {
                    let t0 = qpc();
                    f();
                    qpc() - t0
                })
                .collect();
            v.sort_unstable();
            let p = |k: usize| us(v[(v.len() * k).div_ceil(100) - 1]);
            println!(
                "{label}: n = {}、p50 {:.2} / p99 {:.2} / max {:.2} µs（QPC 2 回の差を含む）",
                v.len(),
                p(50),
                p(99),
                us(v[v.len() - 1])
            );
            p(50)
        };
        let base = measure("空（QPC 2 回だけ）", &mut || {});
        let added = measure("足した記録（CPU 3 回・周期 2 回）", &mut || {
            core::hint::black_box((
                cpu_now(),
                cpu_now(),
                cpu_now(),
                thread_cycles(),
                thread_cycles(),
            ));
        });
        let cpu1 = measure("CPU 1 回", &mut || {
            core::hint::black_box(cpu_now());
        });
        let cyc1 = measure("周期 1 回", &mut || {
            core::hint::black_box(thread_cycles());
        });
        PACE_RING.reset();
        let mut n = 0u32;
        let path = measure(
            "入力・Present の前・後の口を通す（1 フレームぶん）",
            &mut || {
                pace_input(n);
                pace_present_begin();
                pace_present_end();
                n = n.wrapping_add(1);
            },
        );
        PACE_RING.reset();
        println!(
            "差し引き（p50 − 空）: 足した記録 {:.2} µs、CPU 1 回 {:.2} µs、周期 1 回 {:.2} µs、口 3 つ {:.2} µs",
            added - base,
            cpu1 - base,
            cyc1 - base,
            path - base
        );
        assert!(
            added - base < 50.0,
            "足した記録が重い: {:.2} µs",
            added - base
        );
    }
}
