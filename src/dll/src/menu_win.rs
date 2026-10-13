//! メニューとライフの約 1 ms ごとの読み取りのスレッド（Win32 の層。unsafe はここにもある。1 つずつ `// SAFETY:`）。
//!
//! - 読み方は自分のプロセスへの `ReadProcessMemory`（`OwnProcess`）。MainMenu は解放された後もポインタが残るので、解放された所を
//!   読んでも落ちない読み方でたどる（頁が無ければ失敗が返るだけ。値は当てにしない）。
//! - 間隔は高分解能の待機タイマー（Windows 10 1803 から）で 1 ms。作れなければ `Sleep(1)`（既定の OS の間隔では約 15.6 ms まで
//!   延びる。実際の間隔は集計に出る）。
//! - 変わったときだけ `_menu.csv` に 1 行（判定と行は `menu.rs`）。1 秒ごとにファイルを流し、集計を共有の所へコピーする。
//! - スレッドの入口はパニックを受けて外へ出さない。ゲームのスレッドには触らない（読むのは `stub::SEQ` の原子変数だけ）。
//!   ゲームのメモリは読むだけ。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "menu_win.rs は 32 ビット（i686）専用（ゲームの 32 ビットのアドレスを前提にしている）"
);

use core::ffi::c_void;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentProcess,
    SetWaitableTimer, Sleep, TIMER_ALL_ACCESS, WaitForSingleObject,
};

use crate::menu::{MENU_CSV_HEADER, MemRead, MenuWatch, TickStats, TimerMode, read_sample};
use crate::stub;

/// 待機タイマーの 1 回の待ち（100 ns 単位。負は「今から」の相対）。1 ms。
const WAIT_1MS_100NS: i64 = -10_000;
/// タイマーが鳴らないときの歯止め（ms）。止まらずに `stop` を見に戻る。
const WAIT_GUARD_MS: u32 = 100;

pub struct OwnProcess;

impl MemRead for OwnProcess {
    fn u32_at(&mut self, addr: u32) -> Option<u32> {
        let mut v: u32 = 0;
        let mut n: usize = 0;
        // SAFETY: 自分のプロセスの擬似ハンドルと、読むアドレス・書く先（スタックの `v` の 4 バイト）・長さ・読めたバイト数の書く先を渡す
        // だけ。読めなければ OS が失敗を返す。
        let ok = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                addr as usize as *const c_void,
                &mut v as *mut u32 as *mut c_void,
                4,
                &mut n,
            )
        };
        (ok != 0 && n == 4).then_some(v)
    }
}

fn qpc() -> i64 {
    let mut q: i64 = 0;
    // SAFETY: `q` は書ける i64（LARGE_INTEGER と同じ大きさ）。XP 以降は失敗しない。
    unsafe { QueryPerformanceCounter(&mut q) };
    q
}

fn qpc_frequency() -> i64 {
    let mut f: i64 = 0;
    // SAFETY: 同上。
    unsafe { QueryPerformanceFrequency(&mut f) };
    f
}

/// 高分解能の待機タイマー（作れなければ `None`）。スレッドの中で作り、そのスレッドで閉じる。
struct Timer(HANDLE);

impl Timer {
    fn new() -> Option<Timer> {
        // SAFETY: 属性 NULL・名前 NULL（名前なし）・旗と権利を渡すだけ。古い Windows で旗が分からなければ NULL が返る。
        let h = unsafe {
            CreateWaitableTimerExW(
                core::ptr::null(),
                core::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        (!h.is_null()).then_some(Timer(h))
    }

    fn wait_1ms(&self) {
        let due = WAIT_1MS_100NS;
        // SAFETY: `self.0` はいま作ってまだ閉じていないタイマー。`due` はこの呼び出しの間生きている。周期 0（1 回だけ）、完了ルーチンなし、電源の復帰なし。
        let ok = unsafe { SetWaitableTimer(self.0, &due, 0, None, core::ptr::null(), 0) };
        if ok == 0 {
            sleep_1ms();
            return;
        }
        // SAFETY: 同じタイマーを歯止めつきで待つだけ。
        let r = unsafe { WaitForSingleObject(self.0, WAIT_GUARD_MS) };
        if r != WAIT_OBJECT_0 {
            sleep_1ms();
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: `new` で作ったハンドルを 1 回だけ閉じる。
        unsafe { CloseHandle(self.0) };
    }
}

fn sleep_1ms() {
    // SAFETY: 引数はミリ秒だけ。
    unsafe { Sleep(1) };
}

struct Shared {
    stop: AtomicBool,
    stats: Mutex<TickStats>,
}

fn lock(m: &Mutex<TickStats>) -> std::sync::MutexGuard<'_, TickStats> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct MenuThread {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl MenuThread {
    pub fn start(mut file: Box<dyn Write + Send>) -> Result<MenuThread, String> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            stats: Mutex::new(TickStats::new(TimerMode::Sleep1)),
        });
        let s = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name("higanrtool-menu".to_string())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| run_loop(&s, &mut *file)));
                if r.is_err() {
                    lock(&s.stats).write_error =
                        Some("パニックを受けてスレッドを止めた".to_string());
                }
            })
            .map_err(|e| format!("スレッドを起こせない: {e}"))?;
        Ok(MenuThread {
            shared,
            join: Some(join),
        })
    }

    pub fn stats(&self) -> TickStats {
        lock(&self.shared.stats).clone()
    }

    pub fn stop(mut self) -> TickStats {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.stats()
    }
}

impl Drop for MenuThread {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

fn run_loop(shared: &Shared, file: &mut dyn Write) {
    let timer = Timer::new();
    let mode = if timer.is_some() {
        TimerMode::HighResolution
    } else {
        TimerMode::Sleep1
    };
    let mut local = TickStats::new(mode);
    let qpf = qpc_frequency().max(1);
    let to_us = |d: i64| (d.max(0) as u128 * 1_000_000 / qpf as u128) as u64;
    write(file, format!("{MENU_CSV_HEADER}\n").as_bytes(), &mut local);
    let mut mem = OwnProcess;
    let mut watch = MenuWatch::default();
    let t0 = qpc();
    let mut last = t0;
    let mut last_flush = t0;
    while !shared.stop.load(Ordering::Relaxed) {
        match &timer {
            Some(t) => t.wait_1ms(),
            None => sleep_1ms(),
        }
        let now = qpc();
        local.add(to_us(now - last));
        last = now;
        let before = stub::next_seq();
        let sample = read_sample(&mut mem);
        let after = stub::next_seq();
        if let Some(line) = watch.observe(&sample, before, after, to_us(now - t0)) {
            write(file, format!("{line}\n").as_bytes(), &mut local);
        }
        if now - last_flush >= qpf {
            last_flush = now;
            publish(shared, file, &mut local, &watch);
        }
    }
    publish(shared, file, &mut local, &watch);
}

fn write(file: &mut dyn Write, bytes: &[u8], local: &mut TickStats) {
    if local.write_error.is_none()
        && let Err(e) = file.write_all(bytes)
    {
        local.write_error = Some(e.to_string());
    }
}

fn publish(shared: &Shared, file: &mut dyn Write, local: &mut TickStats, watch: &MenuWatch) {
    if local.write_error.is_none()
        && let Err(e) = file.flush()
    {
        local.write_error = Some(e.to_string());
    }
    local.samples = watch.samples;
    local.lines = watch.lines;
    *lock(&shared.stats) = local.clone();
}

#[cfg(test)]
mod tests {
    //! 本物の Win32 を、検査のプロセス自身に当てる（花映塚は使わない）。MainMenu などの値は意味の無いものになる（落ちないことを見る）。
    use super::*;

    static SAMPLE: u32 = 0x1234_5678;

    #[test]
    fn own_process_reads_and_refuses() {
        let mut m = OwnProcess;
        assert_eq!(
            m.u32_at(&SAMPLE as *const u32 as usize as u32),
            Some(0x1234_5678)
        );
        assert_eq!(m.u32_at(0), None);
        assert_eq!(m.u32_at(0x10), None);
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn thread_runs_writes_and_stops() {
        let buf = Buf::default();
        let t = MenuThread::start(Box::new(buf.clone())).expect("起こせる");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let s = t.stop();
        assert!(s.samples >= 5, "読み {} 回", s.samples);
        assert_eq!(s.count, s.samples, "間隔の数と読みの数");
        assert!(s.lines >= 1, "最初の 1 行が無い");
        assert_eq!(s.write_error, None);
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(MENU_CSV_HEADER));
        assert_eq!(text.lines().count() as u64, 1 + s.lines);
        eprintln!(
            "メニューの間隔（検査のプロセス）: {:?}、{} 回、最小 {} µs / 最大 {} µs",
            s.mode, s.count, s.min_us, s.max_us
        );
    }
}
