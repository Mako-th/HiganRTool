//! 組み上がりの検査（花映塚は使わない）。`0x4316B0`の枠と同じ形の偽の関数に差し替えを書き、機械語を実際に呼ぶ。
use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
};

/// 静的変数を使う検査を1本ずつにする。
static GUARD: Mutex<()> = Mutex::new(());
const QPF_TEST: u64 = 10_000_000;
const T0: u64 = 50_000_000;

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

fn reset() {
    BASE.store(0, Ordering::SeqCst);
    COUNT.store(0, Ordering::SeqCst);
    REBASE.store(0, Ordering::SeqCst);
    QPF.store(QPF_TEST, Ordering::SeqCst);
    DISABLED.store(0, Ordering::SeqCst);
    PANIC_LOGGED.store(0, Ordering::SeqCst);
    TEST_PANIC.store(0, Ordering::SeqCst);
    TEST_QPC_FLAG.store(0, Ordering::SeqCst);
    TEST_START.store(0, Ordering::SeqCst);
    TEST_TID.store(current_tid(), Ordering::SeqCst);
    TRAMPOLINE.store(0, Ordering::SeqCst);
    SHOW_TO.store(0, Ordering::SeqCst);
    NO_SHOW_TO.store(0, Ordering::SeqCst);
    TEST_LINES.with(|l| l.borrow_mut().clear());
}

fn lines() -> Vec<String> {
    TEST_LINES.with(|l| l.borrow().clone())
}

// ---- 判断（`decide`） ----

#[test]
fn decide_falls_back_without_touching_the_meter() {
    let _g = lock();
    reset();
    TEST_START.store(T0, Ordering::SeqCst);
    // 先頭がまだ呼ばれていない（どのスレッドからも）。
    TEST_TID.store(0, Ordering::SeqCst);
    assert_eq!(decide(0), FALLBACK);
    assert_eq!(REBASE.load(Ordering::SeqCst), 0);
    // 始めた時刻が無い（ゲーム自身の処理フローの区間）。
    TEST_TID.store(current_tid(), Ordering::SeqCst);
    TEST_START.store(0, Ordering::SeqCst);
    assert_eq!(decide(0), FALLBACK);
    // ゲームがもう1つの時計で測る。
    TEST_START.store(T0, Ordering::SeqCst);
    TEST_QPC_FLAG.store(1, Ordering::SeqCst);
    assert_eq!(decide(0), FALLBACK);
    TEST_QPC_FLAG.store(0, Ordering::SeqCst);
    // パニックを受けた後。
    DISABLED.store(1, Ordering::SeqCst);
    assert_eq!(decide(0), FALLBACK);
    DISABLED.store(0, Ordering::SeqCst);
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (0, 0),
        "ゲーム自身の測りへ任せた呼びで測りの状態を動かした"
    );
    // 条件が揃えば基点を置く（上の呼びが素通りでなかった検証）。
    assert_eq!(decide(0), NO_SHOW);
    assert_eq!(BASE.load(Ordering::SeqCst), T0);
}

/// ゲームのスレッドから数回呼んで基点と数を作る。
fn prime_meter() {
    TEST_START.store(T0, Ordering::SeqCst);
    assert_eq!(decide(0), NO_SHOW);
    TEST_START.store(T0 + 1_000, Ordering::SeqCst);
    assert_eq!(decide(0), NO_SHOW);
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (T0, 1)
    );
}

#[test]
fn a_call_from_another_thread_gets_no_show_and_leaves_the_meter_to_the_game_thread() {
    let _g = lock();
    reset();
    prime_meter();
    TEST_TID.store(current_tid().wrapping_add(1), Ordering::SeqCst);
    // 読み込みのスレッド。ゲームの測りへ任せない（表示を作らない側へ通す）。
    assert_eq!(decide(0), NO_SHOW);
    assert_eq!(decide(3), NO_SHOW);
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (T0, 1),
        "ゲームのスレッド以外が測りの状態を書いた"
    );
    assert_eq!(REBASE.load(Ordering::SeqCst), 1);
    // もう1つの時計の処理フローはどのスレッドでもゲーム自身の測りのまま。
    TEST_QPC_FLAG.store(1, Ordering::SeqCst);
    assert_eq!(decide(0), FALLBACK);
    TEST_QPC_FLAG.store(0, Ordering::SeqCst);
    // 次のゲームのスレッドの描画は基点を置き直す。500 ms過ぎていても1回ぶんを表示しない。
    TEST_TID.store(current_tid(), Ordering::SeqCst);
    TEST_START.store(T0 + QPF_TEST, Ordering::SeqCst);
    assert_eq!(decide(0), NO_SHOW);
    assert_eq!(REBASE.load(Ordering::SeqCst), 0);
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (T0 + QPF_TEST, 0)
    );
}

#[test]
fn coming_back_after_a_start_less_stretch_does_not_show_one_frame_over_the_gap() {
    let _g = lock();
    reset();
    prime_meter();
    // 始めた時刻が無い間（ゲーム自身の測りの区間）はゲーム自身の測りへ。基点は捨てる。
    TEST_START.store(0, Ordering::SeqCst);
    assert_eq!(decide(0), FALLBACK);
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (0, 0)
    );
    // 戻った最初の描画は500 msを大きく越えていても表示を作らず、基点を置く。
    TEST_START.store(T0 + 10 * QPF_TEST, Ordering::SeqCst);
    assert_eq!(decide(0), NO_SHOW);
    assert_eq!(BASE.load(Ordering::SeqCst), T0 + 10 * QPF_TEST);
}

#[test]
fn decide_shows_every_500_ms_of_start_time() {
    let _g = lock();
    reset();
    let mut shown = Vec::new();
    for i in 0..=300u64 {
        TEST_START.store(T0 + i * QPF_TEST / 60, Ordering::SeqCst);
        match decide(0) {
            SHOW => shown.push((i, f32::from_bits(OUT.load(Ordering::SeqCst)))),
            NO_SHOW => {}
            other => panic!("フレーム{i}: 答え {other}"),
        }
    }
    let at: Vec<u64> = shown.iter().map(|s| s.0).collect();
    assert_eq!(at, (1..=10).map(|k| k * 30).collect::<Vec<_>>());
    for (i, f) in shown {
        assert_eq!(format!("{f:.2}"), "60.00", "フレーム{i}");
    }
}

#[test]
fn decide_passes_the_skip_setting_to_the_count() {
    let _g = lock();
    reset();
    TEST_START.store(T0, Ordering::SeqCst);
    assert_eq!(decide(7), NO_SHOW);
    TEST_START.store(T0 + QPF_TEST / 2, Ordering::SeqCst);
    assert_eq!(decide(1), SHOW);
    // 1 + 1の2回 ÷ 0.5 s。最初の呼び（基点を置くだけ）の7は足さない。
    assert_eq!(f32::from_bits(OUT.load(Ordering::SeqCst)), 4.0);
}

#[test]
fn a_panic_disables_the_measure_and_logs_once() {
    let _g = lock();
    reset();
    TEST_START.store(T0, Ordering::SeqCst);
    TEST_PANIC.store(1, Ordering::SeqCst);
    assert_eq!(fps_on_render(0), FALLBACK);
    assert_eq!(DISABLED.load(Ordering::SeqCst), 1);
    TEST_PANIC.store(0, Ordering::SeqCst);
    assert_eq!(fps_on_render(0), FALLBACK, "受けた後は条件が揃っても任せる");
    let l = lines();
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(l[0].starts_with("fps の表示: パニック"), "{l:?}");
    assert_eq!(BASE.load(Ordering::SeqCst), 0);
}

#[test]
fn the_frequency_is_read_once_and_kept() {
    let _g = lock();
    reset();
    QPF.store(0, Ordering::SeqCst);
    let q = qpf();
    assert_eq!(q, crate::pacing_win::qpf());
    assert_ne!(q, 0);
    assert_eq!(QPF.load(Ordering::SeqCst), q);
}

// ---- 入口のアセンブリ ----

#[repr(C)]
struct Cells {
    fps_double: AtomicU64,
    skip: AtomicU32,
    draws: AtomicU32,
    eax_seen: AtomicU32,
    ecx_seen: AtomicU32,
    edi_seen: AtomicU32,
    esi_seen: AtomicU32,
    ebx_seen: AtomicU32,
    esp_show: AtomicU32,
    ebp_show: AtomicU32,
    local: AtomicU32,
    fpu: AtomicU32,
    path: AtomicU32,
}

static CELLS: Cells = Cells {
    fps_double: AtomicU64::new(0),
    skip: AtomicU32::new(0),
    draws: AtomicU32::new(0),
    eax_seen: AtomicU32::new(0),
    ecx_seen: AtomicU32::new(0),
    edi_seen: AtomicU32::new(0),
    esi_seen: AtomicU32::new(0),
    ebx_seen: AtomicU32::new(0),
    esp_show: AtomicU32::new(0),
    ebp_show: AtomicU32::new(0),
    local: AtomicU32::new(0),
    fpu: AtomicU32::new(0),
    path: AtomicU32::new(0),
};

const EDI: u32 = 0xD1D1_D1D1;
const ESI: u32 = 0xE5E5_E5E5;
const EBX: u32 = 0xB3B3_B3B3;
const P_RESUME: u32 = 1;
const P_SHOW: u32 = 2;
const P_NO_SHOW: u32 = 3;
const DRAWS_VALUE: u32 = 0x7777_0001;

fn cell<T>(x: &T) -> u32 {
    x as *const T as usize as u32
}

/// 偽の関数の機械語を作る小さなアセンブラ。
struct Asm {
    base: u32,
    b: Vec<u8>,
}

impl Asm {
    fn at(&self) -> u32 {
        self.base + self.b.len() as u32
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.b.extend_from_slice(bytes);
    }
    /// 前置きのバイトと32ビットの値（絶対アドレスか即値）。
    fn with32(&mut self, prefix: &[u8], v: u32) {
        self.raw(prefix);
        self.raw(&v.to_le_bytes());
    }
    /// `mov dword ptr [abs], imm32`。
    fn store_const(&mut self, addr: u32, v: u32) {
        self.with32(&[0xC7, 0x05], addr);
        self.raw(&v.to_le_bytes());
    }
    /// `jmp rel32`。飛び先は後で`fix`で書く。
    fn jmp(&mut self) -> usize {
        self.raw(&[0xE9, 0, 0, 0, 0]);
        self.b.len() - 4
    }
    fn fix(&mut self, at: usize, to: u32) {
        let next = self.base + at as u32 + 4;
        self.b[at..at + 4].copy_from_slice(&to.wrapping_sub(next).to_le_bytes());
    }
}

struct Fake {
    page: *mut u8,
    func: u32,
    site: u32,
    show: u32,
    no_show: u32,
    resume: u32,
}

impl Drop for Fake {
    fn drop(&mut self) {
        // SAFETY: `new`が確保したページを1回だけ返す。
        unsafe { VirtualFree(self.page.cast(), 0, MEM_RELEASE) };
    }
}

impl Fake {
    /// `0x4316B0`と同じ枠（`push ebp; mov ebp,esp; sub esp,0xc; push ebx; push esi; push edi`）を持つ偽の関数。
    /// 差し替えは`install_at`と同じ`plan_detour`の`E9`とトランポリンで書く。
    fn new() -> Fake {
        // SAFETY: 新しく1ページを確保する（アドレスはOSが選ぶ）。
        let page = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as *mut u8;
        assert!(!page.is_null());
        let base = page as usize as u32;
        let mut a = Asm {
            base,
            b: Vec::new(),
        };
        a.raw(&[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x0C, 0x53, 0x56, 0x57]);
        a.with32(&[0xBF], EDI);
        a.with32(&[0xBE], ESI);
        a.with32(&[0xBB], EBX);
        a.with32(&[0xA1], cell(&CELLS.skip));
        let site = a.at();
        let stolen: [u8; 6] = {
            let m = cell(&CELLS.draws).to_le_bytes();
            [0x8B, 0x0D, m[0], m[1], m[2], m[3]]
        };
        a.raw(&stolen);
        let resume = a.at();
        a.with32(&[0xA3], cell(&CELLS.eax_seen));
        a.with32(&[0x89, 0x0D], cell(&CELLS.ecx_seen));
        a.store_const(cell(&CELLS.path), P_RESUME);
        let j1 = a.jmp();
        let show = a.at();
        a.with32(&[0xDD, 0x1D], cell(&CELLS.fps_double));
        a.with32(&[0x89, 0x25], cell(&CELLS.esp_show));
        a.with32(&[0x89, 0x2D], cell(&CELLS.ebp_show));
        a.raw(&[0x8B, 0x45, 0xFC]);
        a.with32(&[0xA3], cell(&CELLS.local));
        a.store_const(cell(&CELLS.path), P_SHOW);
        let j2 = a.jmp();
        let no_show = a.at();
        a.store_const(cell(&CELLS.path), P_NO_SHOW);
        let done = a.at();
        a.fix(j1, done);
        a.fix(j2, done);
        a.raw(&[0xDF, 0xE0]);
        a.with32(&[0x66, 0xA3], cell(&CELLS.fpu));
        a.with32(&[0x89, 0x3D], cell(&CELLS.edi_seen));
        a.with32(&[0x89, 0x35], cell(&CELLS.esi_seen));
        a.with32(&[0x89, 0x1D], cell(&CELLS.ebx_seen));
        a.raw(&[0x8D, 0x65, 0xE8, 0x5F, 0x5E, 0x5B, 0x8B, 0xE5, 0x5D, 0xC3]);
        assert!(a.b.len() < 0x300);
        // 差し替え: 盗む6バイトの行を作り、`plan_detour`の`E9 + NOP`とトランポリンを書く。
        let def = SiteDef {
            name: "fake_fps",
            addr: site,
            expected: Box::leak(Box::new(stolen)),
            kind: crate::sites::Use::Detour { instr_lens: &[6] },
            source: "合成",
        };
        let plan = detour::plan_detour(&def, entry_address(), &[]).expect("計画");
        let tramp = base + 0x400;
        let tb = plan.trampoline(tramp).expect("トランポリン");
        let site_off = (site - base) as usize;
        a.b[site_off..site_off + plan.patch.len]
            .copy_from_slice(&plan.patch.bytes[..plan.patch.len]);
        // SAFETY: 確保したページの先頭から、ページより短い機械語を書く（`a.b`は0x300未満。トランポリンは0x400から16バイト未満）。
        unsafe {
            core::ptr::copy_nonoverlapping(a.b.as_ptr(), page, a.b.len());
            core::ptr::copy_nonoverlapping(tb.bytes.as_ptr(), page.add(0x400), tb.len);
        }
        TRAMPOLINE.store(tramp, Ordering::SeqCst);
        SHOW_TO.store(show, Ordering::SeqCst);
        NO_SHOW_TO.store(no_show, Ordering::SeqCst);
        Fake {
            page,
            func: base,
            site,
            show,
            no_show,
            resume,
        }
    }

    fn call(&self) {
        // SAFETY: `new`が書いた機械語（cdeclの引数なし関数の形。ebx・esi・edi・ebpはpush / popで戻す）。
        let f: extern "C" fn() -> u32 = unsafe { core::mem::transmute(self.func as usize) };
        f();
    }
}

/// 偽の関数を1回呼び、通った処理フローを返す。
fn run(fake: &Fake, skip: u32) -> u32 {
    CELLS.skip.store(skip, Ordering::SeqCst);
    CELLS.draws.store(DRAWS_VALUE, Ordering::SeqCst);
    for c in [&CELLS.eax_seen, &CELLS.ecx_seen, &CELLS.fpu, &CELLS.path] {
        c.store(0, Ordering::SeqCst);
    }
    fake.call();
    // どの処理フローでもcallee-savedは元のまま、x87のスタックは空（TOP = 0）。
    assert_eq!(CELLS.edi_seen.load(Ordering::SeqCst), EDI, "edi");
    assert_eq!(CELLS.esi_seen.load(Ordering::SeqCst), ESI, "esi");
    assert_eq!(CELLS.ebx_seen.load(Ordering::SeqCst), EBX, "ebx");
    assert_eq!(
        CELLS.fpu.load(Ordering::SeqCst) & 0x3800,
        0,
        "x87のスタックが空に戻っていない"
    );
    CELLS.path.load(Ordering::SeqCst)
}

#[test]
fn the_patched_function_takes_the_three_ways() {
    let _g = lock();
    reset();
    let fake = Fake::new();
    assert_ne!(fake.show, fake.no_show);
    assert_eq!(fake.resume, fake.site + 6, "戻り先は盗んだ命令の次");
    // 基点を置く最初のフレーム: 表示を作らない処理フロー。
    TEST_START.store(T0, Ordering::SeqCst);
    assert_eq!(run(&fake, 0), P_NO_SHOW);
    // 500 ms未満: 表示を作らない処理フロー。ゲームの数の置き場は触らない。
    TEST_START.store(T0 + 100_000, Ordering::SeqCst);
    assert_eq!(run(&fake, 0), P_NO_SHOW);
    assert_eq!(CELLS.draws.load(Ordering::SeqCst), DRAWS_VALUE);
    // 500 ms: 表示を作る処理フロー。数は1（前の呼び）+ 1 + 2（描画を飛ばす設定）= 4回 ÷ 0.5 s = 8.0。
    TEST_START.store(T0 + 5_000_000, Ordering::SeqCst);
    assert_eq!(run(&fake, 2), P_SHOW);
    let fps = 8.0f32;
    assert_eq!(
        f64::from_bits(CELLS.fps_double.load(Ordering::SeqCst)),
        f64::from(fps),
        "st(0)のfps"
    );
    assert_eq!(
        CELLS.local.load(Ordering::SeqCst),
        fps.to_bits(),
        "-0x4(%ebp)のfloat"
    );
    assert_eq!(
        CELLS.esp_show.load(Ordering::SeqCst),
        CELLS.ebp_show.load(Ordering::SeqCst) - 0x20,
        "espは枠（ebp - 0x18）から8下がっている"
    );
    assert_eq!(CELLS.draws.load(Ordering::SeqCst), DRAWS_VALUE);
}

#[test]
fn a_call_from_another_thread_takes_the_no_show_way_and_never_runs_the_stolen_instruction() {
    let _g = lock();
    reset();
    let fake = Fake::new();
    TEST_START.store(T0, Ordering::SeqCst);
    TEST_TID.store(current_tid().wrapping_add(1), Ordering::SeqCst);
    assert_eq!(run(&fake, 5), P_NO_SHOW);
    assert_eq!(
        CELLS.ecx_seen.load(Ordering::SeqCst),
        0,
        "盗んだ命令を実行した"
    );
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (0, 0)
    );
    // HEAD_TIDが0のうちはどのスレッドでもゲーム自身の測りのまま。
    TEST_TID.store(0, Ordering::SeqCst);
    assert_eq!(run(&fake, 5), P_RESUME);
}

#[test]
fn every_fallback_runs_the_stolen_instruction_and_keeps_eax() {
    let _g = lock();
    reset();
    let fake = Fake::new();
    let check = |label: &str| {
        assert_eq!(run(&fake, 5), P_RESUME, "{label}");
        assert_eq!(CELLS.eax_seen.load(Ordering::SeqCst), 5, "{label}: eax");
        assert_eq!(
            CELLS.ecx_seen.load(Ordering::SeqCst),
            DRAWS_VALUE,
            "{label}: 盗んだ命令（ecxに数を読む）を実行していない"
        );
    };
    TEST_START.store(0, Ordering::SeqCst);
    check("始めた時刻が無い");
    TEST_START.store(T0, Ordering::SeqCst);
    TEST_QPC_FLAG.store(1, Ordering::SeqCst);
    check("ゲームがもう1つの時計で測る");
    TEST_QPC_FLAG.store(0, Ordering::SeqCst);
    TEST_PANIC.store(1, Ordering::SeqCst);
    check("パニック");
    TEST_PANIC.store(0, Ordering::SeqCst);
    assert_eq!(DISABLED.load(Ordering::SeqCst), 1);
    check("パニックの後");
    assert_eq!(
        (BASE.load(Ordering::SeqCst), COUNT.load(Ordering::SeqCst)),
        (0, 0)
    );
}

// ---- 設置 ----

/// アドレス → バイトの偽のメモリ。
struct Mem {
    bytes: BTreeMap<u32, u8>,
    writes: Vec<(u32, Vec<u8>)>,
    sealed: Option<(u32, Vec<u8>)>,
    freed: Vec<u32>,
    allocs: u32,
    alloc_fail: Option<u32>,
    seal_fail: Option<u32>,
    write_fail: Option<u32>,
}

const TRAMP_AT: u32 = 0x0900_0000;
const DEV: u32 = D3D8_DEVICE_PTR;
const ENTRY: u32 = 0x1000_2000;

impl Mem {
    fn new() -> Mem {
        let mut bytes = BTreeMap::new();
        for def in FPS_SITES {
            for (i, b) in def.expected.iter().enumerate() {
                bytes.insert(def.addr + i as u32, *b);
            }
        }
        for i in 0..4 {
            bytes.insert(DEV + i, 0);
        }
        Mem {
            bytes,
            writes: Vec::new(),
            sealed: None,
            freed: Vec::new(),
            allocs: 0,
            alloc_fail: None,
            seal_fail: None,
            write_fail: None,
        }
    }
}

impl Host for Mem {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        for (i, o) in buf.iter_mut().enumerate() {
            match self.bytes.get(&(addr + i as u32)) {
                Some(b) => *o = *b,
                None => return false,
            }
        }
        true
    }
    fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
        if let Some(c) = self.write_fail {
            return Err(c);
        }
        for (i, b) in bytes.iter().enumerate() {
            self.bytes.insert(addr + i as u32, *b);
        }
        self.writes.push((addr, bytes.to_vec()));
        Ok(WriteDone {
            protect_restored: true,
            flushed: true,
        })
    }
    fn alloc(&mut self, _len: usize) -> Result<u32, u32> {
        self.allocs += 1;
        self.alloc_fail.map_or(Ok(TRAMP_AT), Err)
    }
    fn seal(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32> {
        if let Some(c) = self.seal_fail {
            return Err(c);
        }
        self.sealed = Some((addr, bytes.to_vec()));
        Ok(())
    }
    fn free(&mut self, addr: u32) {
        self.freed.push(addr);
    }
}

fn install(m: &mut Mem) -> Result<String, String> {
    install_at(m, FPS_SITES, ENTRY, DEV, FORBIDDEN)
}

#[test]
fn install_writes_one_jump_after_preparing_the_trampoline_and_the_targets() {
    let _g = lock();
    reset();
    let mut m = Mem::new();
    let msg = install(&mut m).expect("全部合えば書く");
    assert!(msg.contains("inline detour に差し替えました"), "{msg}");
    // 書くのは先頭の行の6バイト（`E9 rel32` + `90`）だけ。rel32は0x10002000 - (0x004316C2 + 5)。
    let site = FPS_SITES[0].addr;
    let rel = ENTRY.wrapping_sub(site + 5).to_le_bytes();
    assert_eq!(
        m.writes,
        vec![(site, vec![0xE9, rel[0], rel[1], rel[2], rel[3], 0x90])]
    );
    // トランポリン: 盗んだ`mov 0x4b42cc,%ecx`と、`0x004316C8`への`E9`。
    let (at, t) = m.sealed.clone().expect("トランポリンを書いていない");
    assert_eq!(at, TRAMP_AT);
    let back = (site + 6).wrapping_sub(TRAMP_AT + 6 + 5).to_le_bytes();
    let mut want = FPS_SITES[0].expected.to_vec();
    want.extend_from_slice(&[0xE9, back[0], back[1], back[2], back[3]]);
    assert_eq!(t, want);
    assert_eq!(TRAMPOLINE.load(Ordering::SeqCst), TRAMP_AT);
    assert_eq!(SHOW_TO.load(Ordering::SeqCst), FPS_SHOW_AT);
    assert_eq!(NO_SHOW_TO.load(Ordering::SeqCst), FPS_NO_SHOW_AT);
    assert!(m.freed.is_empty());
}

#[test]
fn install_writes_nothing_when_any_byte_of_any_row_differs() {
    let _g = lock();
    for (r, def) in FPS_SITES.iter().enumerate() {
        for i in 0..def.expected.len() {
            reset();
            let mut m = Mem::new();
            *m.bytes.get_mut(&(def.addr + i as u32)).unwrap() ^= 0xFF;
            let e = install(&mut m).expect_err(&format!("行{r}のバイト{i}"));
            assert!(e.contains("不一致"), "{e}");
            assert!(
                m.writes.is_empty() && m.sealed.is_none() && m.allocs == 0,
                "行{r}のバイト{i}"
            );
            assert_eq!(TRAMPOLINE.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn install_refuses_a_site_that_vpatch_already_rewrote() {
    let _g = lock();
    reset();
    let mut m = Mem::new();
    let site = FPS_SITES[0].addr;
    for (i, b) in [0xE9u8, 0x10, 0x20, 0x30, 0x40, 0x90].iter().enumerate() {
        m.bytes.insert(site + i as u32, *b);
    }
    let e = install(&mut m).expect_err("別のフックがあれば断る");
    assert!(e.contains("vpatch"), "{e}");
    assert!(m.writes.is_empty());
}

#[test]
fn install_refuses_when_the_game_is_already_running_or_unreadable() {
    let _g = lock();
    reset();
    let mut m = Mem::new();
    m.bytes.insert(DEV, 1);
    let e = install(&mut m).expect_err("デバイスが既にある");
    assert!(e.contains("デバイスが既に存在"), "{e}");
    assert!(m.writes.is_empty() && m.allocs == 0);
    let mut m = Mem::new();
    m.bytes.remove(&DEV);
    assert!(
        install(&mut m)
            .expect_err("読めない")
            .contains("読み取り不能")
    );
    let mut m = Mem::new();
    m.bytes.remove(&FPS_SITES[3].addr);
    assert!(
        install(&mut m)
            .expect_err("行が読めない")
            .contains("読み取り不能")
    );
    assert!(m.writes.is_empty() && m.allocs == 0);
}

#[test]
fn install_frees_the_page_and_writes_nothing_when_the_trampoline_or_the_write_fails() {
    let _g = lock();
    reset();
    let mut m = Mem::new();
    m.alloc_fail = Some(8);
    assert!(
        install(&mut m)
            .expect_err("確保")
            .contains("GetLastError = 8")
    );
    assert!(m.writes.is_empty() && m.freed.is_empty());
    let mut m = Mem::new();
    m.seal_fail = Some(9);
    assert!(
        install(&mut m)
            .expect_err("保護")
            .contains("GetLastError = 9")
    );
    assert_eq!(m.freed, vec![TRAMP_AT]);
    assert!(m.writes.is_empty());
    let mut m = Mem::new();
    m.write_fail = Some(5);
    assert!(
        install(&mut m)
            .expect_err("書き込み")
            .contains("GetLastError = 5")
    );
    assert_eq!(m.freed, vec![TRAMP_AT]);
}

#[test]
fn the_install_result_is_logged_with_the_wording() {
    let _g = lock();
    reset();
    assert!(
        !replaced(),
        "設置していない検査のプロセスで差し替え済みと答えた"
    );
    install_log(Ok(Err("理由".to_string())));
    install_log(Ok(Ok("成功".to_string())));
    install_log(Err(Box::new("panic")));
    let l = lines();
    assert_eq!(
        l[0],
        "fps の表示: 設置: 非差し替え（ゲーム自身の測りのまま）: 理由"
    );
    assert_eq!(l[1], "fps の表示: 設置: 成功");
    assert!(
        l[2].starts_with("fps の表示: 設置: パニックを受けました"),
        "{l:?}"
    );
}
