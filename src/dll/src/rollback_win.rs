//! rollback の Win32 とゲームのメモリと入口のアセンブリの層。判断は `rollback_probe.rs`（pure）。
//!
//! - 確保・解放の入口 3 つ（`rb_malloc_entry` / `rb_new_entry` / `rb_free_entry`）。`ALLOC_SITES` の 2・3・4 行（CRT の malloc `0x47B151`・
//!   operator new `0x47B24E`・free `0x47B163`）に `alloc_count::install_all` で detour を書く。同じ地点を使うので、数える入口とは ini で排他。
//! - `RealProbeIo`: `ProbeIo` の本物。設置（`arm`）と終わり（`finish`）は `alloc_win::on_step` から、入力の地点からは `on_tick` / `after_feed`。
//!
//! 確保を DLL の領域へ向ける条件（全部満たすときだけ。外れたら元の CRT へ素通し）: `REDIRECT` が 1・ゲームのスレッド（`ALLOC_MAIN_TID`）・
//! 呼び出し元がゲームのコード `GAME_CODE_START..GAME_CODE_END` の中で `ROLLBACK_EXCLUDED_CALLERS` の外（確保の包みの中からなら、包みを呼んだ所で見る）。
//! 解放は、ポインタが領域の中なら領域へ返す（別のスレッドからなら返さずに数えるだけ）。領域の外は CRT へ素通し（見張りの間は数える）。
//! 塊の先頭には呼び出し元を札として置き、比べるときに記録の処理の fps の塊を見分ける。
//!
//! フレームの形（ini の `resim = frame`）: do_frame の走査の call（`0x0042DCA9`）も `rb_scan_entry` へ向ける（配るバージョンでは書かない）。
//! 書き戻しのあったフレームだけ走査を k 回呼び（`rollback_resim::run_scan`）、途中の回が積んだ SE・BGM・文字の数は進め直しの前にバックアップ、最後の回の前に戻す（`RawOutputs`）。
//!
//! 書くのはプロセスのメモリ（detour の 5 地点と、コピーした範囲の書き戻し）だけ。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "rollback_win.rs は 32 ビット（i686）専用（入口のアセンブリが fs:[0x24] と 32 ビットのアドレスを前提にしている）"
);

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};

use higanr_core::arena::{ArenaMem, ArenaState, STATE_WORDS};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualAlloc, VirtualQuery,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::alloc_count::{
    self, ALLOC_MAIN_TID, ALLOC_TRAMPS, AllocHost, AllocSettings, SITE_COUNT, SiteDetourEnv,
};
use crate::detour::DetourEnv;
use crate::hook::{self, HookEnv};
use crate::menu::MemRead;
use crate::menu_win::OwnProcess;
use crate::rollback_draw::{
    self, DrawDiag, SkipDraw, arena_row, compare_arena, compare_region_slots,
};
use crate::rollback_play::{OUTSIDE_BODIES_CAP, OUTSIDE_NODES_CAP, STRAY_REC_CAP, StrayRec};
use crate::rollback_probe::{
    BornSet, Buf, Counters, Layout, MAX_RECORDS, MAX_REGIONS, Marks, Probe, ProbeIo, RegionKind,
    RegionResult, STATE_BYTES, Settings, Tables, TrialRecord, region_name, region_skips,
};
use crate::rollback_resim::{
    self, MAX_SAMPLES, OUTPUT_RANGES, OutputMem, ResimMode, SavedOutputs, ScanAbort, ScanIo,
};
use crate::sites::check_table;
use crate::stub::TEB_THREAD_ID;
use crate::th09_addr::{
    ALLOC_SITES, ALLOC_WRAP_OUTER, ALLOC_WRAP_RET, ASCII_COUNT, CALLOC_SITES, GAME_CODE_END,
    GAME_CODE_START, LOADING, MATCH_END_MENU, PAUSE, RESIM_SCAN_SITE, RESIM_SITES,
    ROLLBACK_ARENA_VM_TAGS, ROLLBACK_EXCLUDED_CALLERS, ROLLBACK_SLOT_ARRAYS, SUP_SCENE_CUR,
    SUP_SCENE_REQ, SUPERVISOR,
};
use higanr_core::patch::AddrRange;

pub static RB_REDIRECT: AtomicU32 = AtomicU32::new(0);
pub static RB_WATCH: AtomicU32 = AtomicU32::new(0);
pub static RB_BASE: AtomicU32 = AtomicU32::new(0);
pub static RB_END: AtomicU32 = AtomicU32::new(0);
pub static RB_OUT_ALLOCS: AtomicU32 = AtomicU32::new(0);
pub static RB_OUT_FREES: AtomicU32 = AtomicU32::new(0);
/// 領域の外の解放のうち、同じ見張りの間に CRT へ回した確保で生まれた塊（`OUT_BORN` で引けた）。
pub static RB_OUT_FREES_BORN: AtomicU32 = AtomicU32::new(0);
/// 見張りの間に CRT へ回した確保のうち、`OUT_BORN` が満ちてアドレスをバックアップできなかったもの。
pub static RB_OUT_UNTRACKED: AtomicU32 = AtomicU32::new(0);
/// 見張りの間に CRT へ回した確保のアドレス（`set_watch(true)` のたびに空にする）。ゲームのスレッドだけが触る。
static OUT_BORN: BornSet = BornSet::new();
/// `OUT_BORN` が同時にバックアップした数の最大（枠 `BORN_CAP` の余裕を数字で見る）。
pub static RB_OUT_BORN_MAX: AtomicU32 = AtomicU32::new(0);
/// 1 なら、見張りの間の前からある領域の外の塊の解放を CRT へ渡さない（製品の経路の予測の tick だけ）。予測の tick は書き戻されて確定の語で進め直され、同じ解放がそこで本当に行われる。
pub static RB_HOLD_FREES: AtomicU32 = AtomicU32::new(0);
/// CRT へ渡さなかった解放の数・そのアドレスをバックアップできなかった数（`HELD` が満ちた）。
pub static RB_OUT_FREES_HELD: AtomicU32 = AtomicU32::new(0);
pub static RB_HELD_UNTRACKED: AtomicU32 = AtomicU32::new(0);
/// コピーの後に CRT へ渡さなかった解放のアドレス（コピー・書き戻しのたびに空にする）。ゲームのスレッドだけが触る。
static HELD: BornSet = BornSet::new();
/// 1 なら、コピーの後に確定していない tick を走らせ、まだ書き戻していない。この間の見張りの外の領域の外の解放を `RB_STRAY_FREES` に数える（書き戻す塊・節が本当に解放されていないかの実測）。
pub static RB_PENDING: AtomicU32 = AtomicU32::new(0);
pub static RB_STRAY_FREES: AtomicU32 = AtomicU32::new(0);
pub static RB_FOREIGN_FREES: AtomicU32 = AtomicU32::new(0);
pub static RB_BAD_FREES: AtomicU32 = AtomicU32::new(0);
/// 解放で中身を 0 で埋められなかった回数（`RawMem::zero` の範囲が領域を越えた）。届くと「印は空き・中身は古い」塊ができるので数える。
pub static RB_UNZEROED: AtomicU32 = AtomicU32::new(0);
pub static RB_ARENA_FULL: AtomicU32 = AtomicU32::new(0);
static ARMED: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicU32 = AtomicU32::new(0);
static RESULT_COUNT: AtomicU32 = AtomicU32::new(0);

// SAFETY（`#[unsafe(naked)]`。3 つの入口に共通）: 本体は `naked_asm!` だけ。関数の先頭（`[esp]` が戻りアドレス、`[esp + 4]` が cdecl の引数 1 つ）に書いた `E9` から飛んでくる。
// - 素通しの経路: eax・ecx・edx を push / pop で戻し、esp を入った時と同じにしてトランポリンへ `jmp`。ebx・esi・edi・ebp には書かない。
// - 配った経路（malloc / new）と領域へ返した経路（free）: eax に戻り値を置き、ecx・edx を戻して `ret`（cdecl なので引数は呼んだ側が片付ける）。元の関数も eax・ecx・edx を壊してよい。
//   フラグは戻さない（呼び出し規約で当てにされない。DF は変えない）。
// - バックアップする経路（ゲームのスレッド・`rb_alloc` が 0・見張りが 1）: 大きさを積み直してトランポリンを `call` し、戻り値を `rb_out_alloc` に渡してから eax に置いて `ret`。
//   operator new が例外を投げたら、この入口の枠は SEH の鎖に載っていないので飛び越され、捕まえた側がスタックを戻す。
// - 呼ぶ Rust の関数は cdecl で、ebx・esi・edi・ebp を保つ。読むのは自分のスタック・TEB（`fs:[0x24]`）・DLL の静的変数だけ。
//   `[esp + … + ALLOC_WRAP_OUTER]` は確保の包みの中から呼ばれたときの包みの呼び出し元で、自分のスレッドのスタックの中を読むだけ。
// - ラベルは 2〜6（0 と 1 だけの名前は 2 進の数と紛れる）。
macro_rules! rb_alloc_entry {
    ($name:ident, $site:expr) => {
        /// 確保の入口（`ALLOC_SITES` の `$site` 行）。Rust からは呼ばない（アドレスを取るだけ）。
        #[unsafe(naked)]
        pub extern "C" fn $name() {
            core::arch::naked_asm!(
                "cmp dword ptr [{redirect}], 0",
                "je 4f",
                "push eax",
                "push ecx",
                "push edx",
                // ここの esp を E とする: [E+12] 戻りアドレス、[E+16] 大きさ、包みの呼び出し元は [E+12+OUTER]。
                "mov eax, dword ptr fs:[{teb_tid}]",
                "cmp eax, dword ptr [{main_tid}]",
                "jne 3f",
                "push dword ptr [esp + 16]",
                "push dword ptr [esp + 16 + {outer}]",
                "push dword ptr [esp + 20]",
                "call {rb_alloc}",
                "add esp, 12",
                "test eax, eax",
                "jz 7f",
                "pop edx",
                "pop ecx",
                "add esp, 4",
                "ret",
                "7:",
                // CRT へ回す確保（ゲームのスレッド）。見張りの間だけ、元の関数をトランポリン越しに呼んで戻り値のアドレスをバックアップしてから返す（見張りの外は下の素通しのまま）。
                "cmp dword ptr [{watch}], 0",
                "je 3f",
                "pop edx",
                "pop ecx",
                "pop eax",
                // ここの esp は入った時と同じ。大きさを積み直して呼ぶ（元の関数から見た [esp] と [esp+4] は、呼ばれた時と同じ形）。
                "push dword ptr [esp + 4]",
                "call dword ptr [{tramps} + {toff}]",
                "add esp, 4",
                "push eax",
                "push eax",
                "call {rb_out_alloc}",
                "add esp, 4",
                "pop eax",
                "ret",
                "3:",
                "pop edx",
                "pop ecx",
                "pop eax",
                "4:",
                "jmp dword ptr [{tramps} + {toff}]",
                redirect = sym RB_REDIRECT,
                teb_tid = const TEB_THREAD_ID,
                main_tid = sym ALLOC_MAIN_TID,
                outer = const ALLOC_WRAP_OUTER,
                rb_alloc = sym rb_alloc,
                watch = sym RB_WATCH,
                rb_out_alloc = sym rb_out_alloc,
                tramps = sym ALLOC_TRAMPS,
                toff = const $site * 4,
            )
        }
    };
}

rb_alloc_entry!(rb_malloc_entry, SITE_MALLOC);
rb_alloc_entry!(rb_new_entry, SITE_NEW);

/// 解放の入口（`ALLOC_SITES` の `SITE_FREE` 行）。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn rb_free_entry() {
    core::arch::naked_asm!(
        "push eax",
        "push ecx",
        "push edx",
        "mov eax, dword ptr [esp + 16]",
        "cmp eax, dword ptr [{base}]",
        "jb 5f",
        "cmp eax, dword ptr [{end}]",
        "jae 5f",
        "xor ecx, ecx",
        "mov edx, dword ptr fs:[{teb_tid}]",
        "cmp edx, dword ptr [{main_tid}]",
        "jne 2f",
        "mov ecx, 1",
        "2:",
        "push ecx",
        "push eax",
        "call {rb_free}",
        "add esp, 8",
        "pop edx",
        "pop ecx",
        "pop eax",
        "ret",
        "5:",
        "cmp dword ptr [{watch}], 0",
        "jne 7f",
        // 見張りの外でも、予測の tick の後・書き戻しの前（`RB_PENDING`）なら、ゲームのスレッドの NULL でない解放を数えて素通し。
        "cmp dword ptr [{pending}], 0",
        "jne 8f",
        // コピーの後・最初の予測の tick の前でも、バックアップした ScreenEffect の本体があれば、その解放に印だけ立てる（数えない）。
        "cmp dword ptr [{body_n}], 0",
        "je 6f",
        "test eax, eax",
        "jz 6f",
        "mov edx, dword ptr fs:[{teb_tid}]",
        "cmp edx, dword ptr [{main_tid}]",
        "jne 6f",
        "push eax",
        "call {body_free}",
        "add esp, 4",
        "jmp 6f",
        "8:",
        "test eax, eax",
        "jz 6f",
        "mov edx, dword ptr fs:[{teb_tid}]",
        "cmp edx, dword ptr [{main_tid}]",
        "jne 6f",
        // 誰が何を解放したかをバックアップする（診断）。rb_stray_free(ptr, 呼び手の戻りアドレス)。cdecl。eax・ecx・edx は壊れるが 6 で戻す。
        "push dword ptr [esp + 12]",
        "push eax",
        "call {stray}",
        "add esp, 8",
        "jmp 6f",
        "7:",
        "test eax, eax",
        "jz 6f",
        "mov edx, dword ptr fs:[{teb_tid}]",
        "cmp edx, dword ptr [{main_tid}]",
        "jne 6f",
        // 期間の中で生まれた塊か、前からある塊かをアドレスで分けて数える（eax・ecx・edx は下で戻す）。
        "push eax",
        "call {rb_out_free}",
        "add esp, 4",
        // 1 が返ったら（製品の経路の予測の tick の前からある塊）CRT へ渡さずに呼び手へ戻る。解放は確定の語の進め直しがもう一度行う。
        "test eax, eax",
        "jz 6f",
        "pop edx",
        "pop ecx",
        "pop eax",
        "ret",
        "6:",
        "pop edx",
        "pop ecx",
        "pop eax",
        "jmp dword ptr [{tramps} + {toff}]",
        base = sym RB_BASE,
        end = sym RB_END,
        teb_tid = const TEB_THREAD_ID,
        main_tid = sym ALLOC_MAIN_TID,
        rb_free = sym rb_free,
        watch = sym RB_WATCH,
        pending = sym RB_PENDING,
        stray = sym rb_stray_free,
        body_n = sym STRAY_BODY_N,
        body_free = sym rb_body_free,
        rb_out_free = sym rb_out_free,
        tramps = sym ALLOC_TRAMPS,
        toff = const SITE_FREE * 4,
    )
}

/// `ALLOC_SITES` の行の番号（0 包み・1 包み・2 malloc・3 operator new・4 free）。`rollback_play::SITE_*` と同じ番号。
const SITE_MALLOC: usize = crate::rollback_play::SITE_MALLOC;
const SITE_NEW: usize = crate::rollback_play::SITE_NEW;
const SITE_FREE: usize = crate::rollback_play::SITE_FREE;
const _: () = assert!(SITE_COUNT == 5 && SITE_MALLOC == 2 && SITE_NEW == 3 && SITE_FREE == 4);

// ---- calloc の見張り（`th09_addr::CALLOC_SITES`）----
/// calloc の入口のトランポリン（`install_detour` が JMP を書く前に置く。0 の間はこの入口へ向く JMP は無い）。
pub static RB_CALLOC_TRAMP: AtomicU32 = AtomicU32::new(0);
/// 見張りの間にゲームのスレッドで calloc を通った確保の数（`RB_OUT_ALLOCS` にも数える。内訳を見るため別にも持つ）。
pub static RB_OUT_CALLOCS: AtomicU32 = AtomicU32::new(0);

// SAFETY（`#[unsafe(naked)]`）: calloc `0x0047DE66` の先頭（`[esp]` が戻りアドレス、`[esp + 4]` が個数、`[esp + 8]` が大きさ。cdecl）に書いた `E9` から飛んでくる。
// - 素通しの経路（見張りの外・別のスレッド）: esp と eax・ecx・edx を入った時と同じにしてトランポリンへ `jmp`（盗んだ 2 命令は即値の push）。
// - 見張りの経路（`RB_WATCH` が 1 でゲームのスレッド）: 引数を積み直してトランポリンを `call` し、戻り値を `rb_out_calloc` に渡してから eax に置いて `ret`。ecx・edx は元の関数と `rb_out_calloc` が壊したまま（どちらも cdecl で ebx・esi・edi・ebp を保つ）。
// - 読むのは自分のスタック・TEB・DLL の静的変数だけ。DLL の領域へは向けない（呼ぶ所 8 か所はどれも `GAME_CODE_END` の外）。ラベルは 4 だけ。
/// calloc の入口（`CALLOC_SITES` の行）。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn rb_calloc_entry() {
    core::arch::naked_asm!(
        "cmp dword ptr [{watch}], 0",
        "je 4f",
        "push eax",
        "mov eax, dword ptr fs:[{teb_tid}]",
        "cmp eax, dword ptr [{main_tid}]",
        "pop eax",
        "jne 4f",
        // ここの esp は入った時と同じ。大きさ（[esp+8]）を先に、個数（積んだ後の [esp+8] = 元の [esp+4]）を後に積む。
        "push dword ptr [esp + 8]",
        "push dword ptr [esp + 8]",
        "call dword ptr [{tramp}]",
        "add esp, 8",
        "push eax",
        "push eax",
        "call {rb_out_calloc}",
        "add esp, 4",
        "pop eax",
        "ret",
        "4:",
        "jmp dword ptr [{tramp}]",
        watch = sym RB_WATCH,
        teb_tid = const TEB_THREAD_ID,
        main_tid = sym ALLOC_MAIN_TID,
        tramp = sym RB_CALLOC_TRAMP,
        rb_out_calloc = sym rb_out_calloc,
    )
}

extern "C" fn rb_out_calloc(ptr: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        RB_OUT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        RB_OUT_CALLOCS.fetch_add(1, Ordering::Relaxed);
        if !OUT_BORN.add(ptr) {
            RB_OUT_UNTRACKED.fetch_add(1, Ordering::Relaxed);
        }
        RB_OUT_BORN_MAX.fetch_max(OUT_BORN.len() as u32, Ordering::Relaxed);
    }));
}

pub(crate) fn out_born_len() -> u32 {
    OUT_BORN.len() as u32
}

pub(crate) fn clear_out_born() {
    OUT_BORN.clear();
}

pub(crate) fn arena_top() -> u32 {
    // SAFETY: ゲームのスレッドだけが触る入れ物（`GameCell` の Sync の注）。読むだけで、参照はこの式の中で終わる。
    unsafe { (*ARENA.0.get()).as_ref() }.map_or(0, |a| a.state.top)
}

pub(crate) fn in_arena(addr: u32) -> bool {
    (RB_BASE.load(Ordering::Relaxed)..RB_END.load(Ordering::Relaxed)).contains(&addr)
}

/// 領域を確保して置く（読み取りのスレッドで 1 回。E1 がもう置いていたら置かない）。置けたら領域の先頭。
pub(crate) fn place_play_arena(bytes: u32) -> Option<u32> {
    if armed() || RB_BASE.load(Ordering::SeqCst) != 0 {
        return None;
    }
    let base = virtual_alloc(bytes)?;
    // SAFETY: 置くのは読み取りのスレッドの 1 回だけ。まだ `RB_BASE` を立てておらず入口の detour も書いていないので、ゲームのスレッドは入れ物に触らない。
    unsafe {
        *ARENA.0.get() = Some(Arena {
            state: ArenaState::new(),
            mem: RawMem { base, len: bytes },
        });
    }
    RB_BASE.store(base, Ordering::SeqCst);
    RB_END.store(base + bytes, Ordering::SeqCst);
    Some(base)
}

/// 新しい領域を予約してコミットする（`VirtualAlloc`。0 で埋まっている）。
pub(crate) fn alloc_pages(bytes: u32) -> Option<u32> {
    virtual_alloc(bytes)
}

struct RawMem {
    base: u32,
    len: u32,
}

impl ArenaMem for RawMem {
    fn len(&self) -> u32 {
        self.len
    }
    fn read(&self, off: u32) -> u32 {
        // SAFETY: `off` は割り当て器が渡す `len` 未満の 4 の倍数。`[base, base + len)` は `arm` が VirtualAlloc で確保した領域で、プロセスが終わるまで返さない。
        unsafe { ((self.base + off) as usize as *const u32).read() }
    }
    fn write(&mut self, off: u32, v: u32) {
        // SAFETY: `read` と同じ。
        unsafe { ((self.base + off) as usize as *mut u32).write(v) }
    }
    fn zero(&mut self, off: u32, len: u32) {
        // 解放のたびに塊の中身（676 B〜8 KiB）を埋めるので、語ごとの `write` でなく 1 回の memset で埋める。範囲が領域を越えるなら何も書かず、数える。
        if off.checked_add(len).is_none_or(|e| e > self.len) {
            RB_UNZEROED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: `[base + off, base + off + len)` は上で確かめたとおり領域の中。u8 の書きなので境界の制約は無い。領域は `arm` が VirtualAlloc で確保し、プロセスが終わるまで返さない。
        unsafe { core::ptr::write_bytes((self.base + off) as usize as *mut u8, 0, len as usize) }
    }
}

struct Arena {
    state: ArenaState,
    mem: RawMem,
}

/// ゲームのスレッドだけが触る入れ物。`arm` が読み取りのスレッドで 1 回だけ置き、以後は入口と入力の地点だけが触る。
pub(crate) struct GameCell<T>(pub(crate) UnsafeCell<Option<T>>);
// SAFETY: 中身に触るのは、置く前の 1 回と、その後のゲームのスレッドだけ。ゲームのスレッドは 1 本（実測）。別のスレッドの解放は中身に触らず数える。
unsafe impl<T> Sync for GameCell<T> {}

static ARENA: GameCell<Arena> = GameCell(UnsafeCell::new(None));
static PROBE: GameCell<ProbeBox> = GameCell(UnsafeCell::new(None));
/// 回ごとの記録。`TrialRecord` が診断の表（約 2 KB）を持ち 120 件で 0.6 MB を越えるので、`arm` のスタックに一時の配列を作らないようヒープの塊にした。
static RESULTS: GameCell<Box<[TrialRecord]>> = GameCell(UnsafeCell::new(None));

struct ProbeBox {
    probe: Probe,
    snap: u32,
    d1: u32,
    qpf: u64,
}

fn caller_of(ret: u32, outer: u32) -> u32 {
    if ret == ALLOC_WRAP_RET { outer } else { ret }
}

pub fn redirectable(ret: u32, outer: u32) -> bool {
    let caller = caller_of(ret, outer);
    (GAME_CODE_START..GAME_CODE_END).contains(&caller)
        && !ROLLBACK_EXCLUDED_CALLERS
            .iter()
            .any(|c| (c.start..c.end).contains(&caller))
}

/// 確保の入口から（ゲームのスレッド・`RB_REDIRECT` が 1 のときだけ）。配れたらアドレス、配らないなら 0（入口は CRT へ素通し）。
extern "C" fn rb_alloc(ret: u32, outer: u32, size: u32) -> u32 {
    catch_unwind(AssertUnwindSafe(|| alloc_inner(ret, outer, size))).unwrap_or(0)
}

fn alloc_inner(ret: u32, outer: u32, size: u32) -> u32 {
    if !redirectable(ret, outer) {
        if RB_WATCH.load(Ordering::Relaxed) != 0 {
            RB_OUT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        return 0;
    }
    // SAFETY: ゲームのスレッド（入口が確かめた）。`ARENA` を置いた後にしか `RB_REDIRECT` は 1 にならない。
    let Some(a) = (unsafe { (*ARENA.0.get()).as_mut() }) else {
        return 0;
    };
    // 塊の先頭に呼び出し元を札として置く（比べるときに記録の処理の fps の塊を見分ける）。
    match a
        .state
        .alloc_tagged(&mut a.mem, size, caller_of(ret, outer))
    {
        Some(off) => a.mem.base + off,
        None => {
            RB_ARENA_FULL.fetch_add(1, Ordering::Relaxed);
            0
        }
    }
}

extern "C" fn rb_free(ptr: u32, main: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| free_inner(ptr, main)));
}

fn free_inner(ptr: u32, main: u32) {
    if main == 0 {
        RB_FOREIGN_FREES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: ゲームのスレッド（入口が確かめた）。ポインタが領域の中なら `ARENA` は置かれている。
    let Some(a) = (unsafe { (*ARENA.0.get()).as_mut() }) else {
        RB_BAD_FREES.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let off = ptr - a.mem.base;
    if a.state.free(&mut a.mem, off).is_err() {
        RB_BAD_FREES.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn rb_out_alloc(ptr: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !OUT_BORN.add(ptr) {
            RB_OUT_UNTRACKED.fetch_add(1, Ordering::Relaxed);
        }
        RB_OUT_BORN_MAX.fetch_max(OUT_BORN.len() as u32, Ordering::Relaxed);
    }));
}

/// 解放の入口から（ゲームのスレッド・見張りの間・領域の外の NULL でない塊）。生まれた塊かで分けて数え、製品の経路の予測の tick の前からある塊なら 1 を返す（入口は CRT へ渡さずに戻る）。
extern "C" fn rb_out_free(ptr: u32) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if OUT_BORN.take(ptr) {
            RB_OUT_FREES_BORN.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        RB_OUT_FREES.fetch_add(1, Ordering::Relaxed);
        if RB_HOLD_FREES.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        RB_OUT_FREES_HELD.fetch_add(1, Ordering::Relaxed);
        if !HELD.add(ptr) {
            RB_HELD_UNTRACKED.fetch_add(1, Ordering::Relaxed);
        }
        1
    }))
    .unwrap_or(0)
}

/// 見張りの外の解放のバックアップ（ゲームのスレッドだけが書き、読み取りのスレッドが読む）。`RB_STRAY_FREES` が上限を越えたら数だけ進める。
static STRAY_REC: [[AtomicU32; 4]; STRAY_REC_CAP] =
    [const { [const { AtomicU32::new(0) }; 4] }; STRAY_REC_CAP];
/// コピーのときのバックアップ: 確定したフレーム・範囲の外の節のアドレス・コピーの表の塊のアドレス（`set_stray_refs`）。
static STRAY_FRAME: AtomicU32 = AtomicU32::new(0);
static STRAY_NODE_N: AtomicU32 = AtomicU32::new(0);
static STRAY_NODES: [AtomicU32; OUTSIDE_NODES_CAP] =
    [const { AtomicU32::new(0) }; OUTSIDE_NODES_CAP];
static STRAY_BLOCK_N: AtomicU32 = AtomicU32::new(0);
static STRAY_BLOCKS: [AtomicU32; MAX_REGIONS] = [const { AtomicU32::new(0) }; MAX_REGIONS];
/// コピーのときにバックアップした ScreenEffect の本体のアドレスと、コピーの後に見張りの外で CRT へ解放が渡った本体の印（第 i ビット = `STRAY_BODIES[i]`）。印はコピーのたびに消す。
static STRAY_BODY_N: AtomicU32 = AtomicU32::new(0);
static STRAY_BODIES: [AtomicU32; OUTSIDE_BODIES_CAP] =
    [const { AtomicU32::new(0) }; OUTSIDE_BODIES_CAP];
static STRAY_BODY_HIT: AtomicU32 = AtomicU32::new(0);
// 印は 1 つの AtomicU32 のビットで持つ（本体の枠が 32 を越えたらビットが足りない）。
const _: () = assert!(OUTSIDE_BODIES_CAP <= 32);

/// コピーのたびに、見張りの外の解放がだれのものかを見分ける表を置き直す。表より長い分は切る。`bodies` はバックアップした ScreenEffect の本体のアドレス（印を消してから置く）。
pub(crate) fn set_stray_refs(frame: u32, nodes: &[u32], blocks: &[u32], bodies: &[u32]) {
    STRAY_BODY_HIT.store(0, Ordering::Relaxed);
    let nb = bodies.len().min(STRAY_BODIES.len());
    for (slot, v) in STRAY_BODIES.iter().zip(bodies) {
        slot.store(*v, Ordering::Relaxed);
    }
    STRAY_BODY_N.store(nb as u32, Ordering::Relaxed);
    STRAY_FRAME.store(frame, Ordering::Relaxed);
    let n = nodes.len().min(STRAY_NODES.len());
    for (slot, v) in STRAY_NODES.iter().zip(nodes) {
        slot.store(*v, Ordering::Relaxed);
    }
    STRAY_NODE_N.store(n as u32, Ordering::Relaxed);
    let b = blocks.len().min(STRAY_BLOCKS.len());
    for (slot, v) in STRAY_BLOCKS.iter().zip(blocks) {
        slot.store(*v, Ordering::Relaxed);
    }
    STRAY_BLOCK_N.store(b as u32, Ordering::Relaxed);
}

fn in_table(table: &[AtomicU32], n: &AtomicU32, ptr: u32) -> bool {
    let n = (n.load(Ordering::Relaxed) as usize).min(table.len());
    table[..n].iter().any(|a| a.load(Ordering::Relaxed) == ptr)
}

fn mark_body_freed(ptr: u32) -> bool {
    let n = (STRAY_BODY_N.load(Ordering::Relaxed) as usize).min(STRAY_BODIES.len());
    let Some(i) = STRAY_BODIES
        .iter()
        .take(n)
        .position(|a| a.load(Ordering::Relaxed) == ptr)
    else {
        return false;
    };
    STRAY_BODY_HIT.fetch_or(1 << i, Ordering::Relaxed);
    true
}

/// コピーのときにバックアップした ScreenEffect の本体 `ptr` が、コピーの後に見張りの外で CRT へ解放を渡されたか。表に無いアドレスは真。
pub(crate) fn body_freed(ptr: u32) -> bool {
    let n = (STRAY_BODY_N.load(Ordering::Relaxed) as usize).min(STRAY_BODIES.len());
    match STRAY_BODIES
        .iter()
        .take(n)
        .position(|a| a.load(Ordering::Relaxed) == ptr)
    {
        Some(i) => STRAY_BODY_HIT.load(Ordering::Relaxed) & (1 << i) != 0,
        None => true,
    }
}

/// 解放の入口から（見張りの外・書き戻し待ちでない・バックアップした本体の表が空でない）。バックアップした本体が CRT へ渡ったら印を立てる（数えない）。
extern "C" fn rb_body_free(ptr: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        mark_body_freed(ptr);
    }));
}

/// マッチを動かさなくなったときに、バックアップした本体の表を空にする（マッチの外の解放が `rb_body_free` を通らないように）。
pub(crate) fn clear_stray_bodies() {
    STRAY_BODY_N.store(0, Ordering::Relaxed);
    STRAY_BODY_HIT.store(0, Ordering::Relaxed);
}

/// 解放の入口から（見張りの外・書き戻し待ち）。数えて、最初の `STRAY_REC_CAP` 件まで呼び手・ポインタ・どの表に載っているか・コピーのフレームをバックアップする。確保も待ちもしない。
extern "C" fn rb_stray_free(ptr: u32, ret: u32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let is_body = mark_body_freed(ptr);
        let i = RB_STRAY_FREES.load(Ordering::Relaxed) as usize;
        if i < STRAY_REC_CAP {
            let mut flags = 0;
            if is_body {
                flags |= StrayRec::BODY;
            }
            if in_table(&STRAY_NODES, &STRAY_NODE_N, ptr) {
                flags |= StrayRec::NODE;
            }
            if freed_held(ptr) {
                flags |= StrayRec::HELD;
            }
            if in_table(&STRAY_BLOCKS, &STRAY_BLOCK_N, ptr) {
                flags |= StrayRec::BLOCK;
            }
            let r = &STRAY_REC[i];
            r[0].store(ret, Ordering::Relaxed);
            r[1].store(ptr, Ordering::Relaxed);
            r[2].store(flags, Ordering::Relaxed);
            r[3].store(STRAY_FRAME.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        RB_STRAY_FREES.fetch_add(1, Ordering::Release);
    }));
}

pub(crate) fn stray_recs() -> ([StrayRec; STRAY_REC_CAP], usize) {
    let n = (RB_STRAY_FREES.load(Ordering::Acquire) as usize).min(STRAY_REC_CAP);
    let mut out = [StrayRec::default(); STRAY_REC_CAP];
    for (o, r) in out.iter_mut().zip(STRAY_REC.iter()).take(n) {
        *o = StrayRec {
            ret: r[0].load(Ordering::Relaxed),
            ptr: r[1].load(Ordering::Relaxed),
            flags: r[2].load(Ordering::Relaxed),
            frame: r[3].load(Ordering::Relaxed),
        };
    }
    (out, n)
}

pub(crate) fn freed_held(ptr: u32) -> bool {
    HELD.contains(ptr)
}

pub(crate) fn clear_held() {
    HELD.clear();
}

pub(crate) fn qpc() -> u64 {
    let mut v: i64 = 0;
    // SAFETY: 書く先は自分の変数。
    unsafe { QueryPerformanceCounter(&mut v) };
    v as u64
}

pub(crate) fn is_rw(addr: u32, len: u32) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    let mut p = addr;
    while p < end {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: 自分のプロセスのアドレスの問い合わせだけ。書く先は自分の構造体で、大きさを渡す。
        let n = unsafe {
            VirtualQuery(
                p as usize as *const core::ffi::c_void,
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 || mbi.State != MEM_COMMIT {
            return false;
        }
        let prot = mbi.Protect & 0xFF;
        if prot != PAGE_READWRITE
            && prot != PAGE_EXECUTE_READWRITE
            && prot != PAGE_WRITECOPY
            && prot != PAGE_EXECUTE_WRITECOPY
        {
            return false;
        }
        let next = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize) as u32;
        if next <= p {
            return false;
        }
        p = next;
    }
    true
}

fn current_len(r_kind: RegionKind, r_len: u32, arena: &Arena) -> u32 {
    match r_kind {
        RegionKind::Arena => arena.state.top,
        RegionKind::ArenaState => STATE_BYTES,
        _ => r_len,
    }
}

struct RealProbeIo<'a> {
    feed: &'a mut dyn FnMut(u16, u16),
    snap: u32,
    d1: u32,
    qpf: u64,
}

impl RealProbeIo<'_> {
    fn arena(&mut self) -> Option<&'static mut Arena> {
        // SAFETY: 入力の地点（ゲームのスレッド）から。確保の入口も同じスレッドなので、この参照と重ならない。
        unsafe { (*ARENA.0.get()).as_mut() }
    }
    fn buf(&self, b: Buf) -> u32 {
        match b {
            Buf::Snapshot => self.snap,
            Buf::D1 => self.d1,
        }
    }
}

impl ProbeIo for RealProbeIo<'_> {
    fn in_match(&mut self) -> bool {
        let mut m = OwnProcess;
        let scene = m.u32_at(SUPERVISOR + SUP_SCENE_CUR);
        let loading = m.u32_at(LOADING);
        let menu = m.u32_at(MATCH_END_MENU).map(|v| v & 0xFF);
        let pause = m.u32_at(PAUSE).map(|v| v & 0xFF);
        scene == Some(2) && loading == Some(0) && menu == Some(0) && pause == Some(0)
    }
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        OwnProcess.u32_at(addr)
    }
    fn check_rw(&mut self, addr: u32, len: u32) -> bool {
        is_rw(addr, len)
    }
    fn list_signature(&mut self) -> u64 {
        let mut m = OwnProcess;
        crate::rollback_play::list_signature_with(&mut |a| m.u32_at(a))
    }
    fn arena_top(&mut self) -> u32 {
        self.arena().map_or(0, |a| a.state.top)
    }
    fn set_redirect(&mut self, on: bool) {
        RB_REDIRECT.store(u32::from(on), Ordering::SeqCst);
    }
    fn set_watch(&mut self, on: bool) {
        if on {
            OUT_BORN.clear();
        }
        RB_WATCH.store(u32::from(on), Ordering::SeqCst);
    }
    fn counters(&mut self) -> Counters {
        let l = |a: &AtomicU32| a.load(Ordering::Relaxed);
        let (allocs, frees) = self
            .arena()
            .map_or((0, 0), |a| (a.state.allocs, a.state.frees));
        Counters {
            out_allocs: l(&RB_OUT_ALLOCS),
            out_frees: l(&RB_OUT_FREES),
            out_frees_born: l(&RB_OUT_FREES_BORN),
            out_untracked: l(&RB_OUT_UNTRACKED),
            foreign_frees: l(&RB_FOREIGN_FREES),
            bad_frees: l(&RB_BAD_FREES),
            arena_full: l(&RB_ARENA_FULL),
            arena_allocs: allocs,
            arena_frees: frees,
        }
    }
    fn save(&mut self, l: &Layout, to: Buf) {
        save_regions(l, self.buf(to));
    }
    fn restore(&mut self, l: &Layout) {
        restore_regions(l, self.snap);
    }
    fn compare(&mut self, t: &Tables, l: &Layout, marks: &Marks, out: &mut [RegionResult]) {
        self.compare_impl(t, l, marks, out, SkipDraw::None, None);
    }
    fn compare_draw(
        &mut self,
        t: &Tables,
        l: &Layout,
        marks: &Marks,
        out: &mut [RegionResult],
        skip: SkipDraw,
        diag: &mut DrawDiag,
    ) {
        self.compare_impl(t, l, marks, out, skip, Some(diag));
    }
    fn feed(&mut self, p1: u16, p2: u16) {
        (self.feed)(p1, p2);
    }
    fn now_us(&mut self) -> u64 {
        if self.qpf == 0 {
            return 0;
        }
        (qpc() as u128 * 1_000_000 / self.qpf as u128) as u64
    }
    fn stop_requested(&mut self) -> bool {
        STOP.load(Ordering::Acquire) != 0
    }
    fn publish(&mut self, rec: &TrialRecord) {
        let n = RESULT_COUNT.load(Ordering::Relaxed) as usize;
        // SAFETY: 書くのはゲームのスレッドだけ。`[0, n)` は読み取りのスレッドが読むので触らず、`n` 番目に書いてから Release で数を進める。
        if let Some(rs) = unsafe { (*RESULTS.0.get()).as_mut() }
            && n < rs.len()
        {
            rs[n] = *rec;
            RESULT_COUNT.store(n as u32 + 1, Ordering::Release);
        }
    }
    fn scene_request_pending(&mut self) -> bool {
        scene_request_pending_now()
    }
    fn capture_outputs(&mut self) {
        // SAFETY: ゲームのスレッドだけが触る（`SavedCell` の Sync の注）。バックアップできなければ `valid = false` のままで、最後の回の前の `apply_outputs` は何も書かない。
        let saved = unsafe { &mut *SAVED.0.get() };
        let _ = rollback_resim::capture_outputs(&mut RawOutputs, saved);
    }
}

/// 範囲を置き場 `buf` へコピーする（割り当て器の状態を含む）。`l` は書ける頁と確かめた表、`buf` は `l.buffer_bytes()` バイト以上のコピーの置き場のときだけ呼ぶ。
pub(crate) fn save_regions(l: &Layout, buf: u32) {
    // SAFETY: ゲームのスレッドだけが触る入れ物（`GameCell` の Sync の注）。確保の入口も同じスレッドなので、この参照と重ならない（コピーしている間はゲームは動かない）。
    let Some(a) = (unsafe { (*ARENA.0.get()).as_mut() }) else {
        return;
    };
    for r in l.regions() {
        let dst = (buf + r.buf_off) as usize as *mut u8;
        match r.kind {
            RegionKind::ArenaState => {
                let w = a.state.to_words();
                // SAFETY: 置き場の `[buf_off, buf_off + cap)` は呼び手が確保したコピーの置き場の中（cap = STATE_BYTES）。元はスタックで、重ならない。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        w.as_ptr().cast::<u8>(),
                        dst,
                        STATE_BYTES as usize,
                    )
                };
            }
            _ => {
                let n = r.len.min(r.cap) as usize;
                // 長さ 0 の範囲（コピーしない VM の配列。アドレスも 0）は触らない（0 バイトでも空のポインタを渡さない）。
                if n == 0 {
                    continue;
                }
                // SAFETY: 元の `[addr, addr + len)` は `fill` が書ける頁と確かめた範囲か DLL の領域の `[base, base + top)`。先は置き場の中（len ≤ cap）。ゲームは動かない。
                unsafe { core::ptr::copy_nonoverlapping(r.addr as usize as *const u8, dst, n) };
            }
        }
    }
}

/// 置き場 `buf` から書き戻す（領域は `[0, コピーしたときの top)` を戻し、`[コピーしたときの top, 今の top)` を 0 にし、割り当て器の状態を戻す）。`l` はコピーしたときの表（アドレスが変わっていないことを確かめてから呼ぶ）。
pub(crate) fn restore_regions(l: &Layout, buf: u32) {
    // SAFETY: `save_regions` と同じ。
    let Some(a) = (unsafe { (*ARENA.0.get()).as_mut() }) else {
        return;
    };
    let cur_top = a.state.top;
    let mut state_words: Option<[u32; STATE_WORDS]> = None;
    for r in l.regions() {
        let src = (buf + r.buf_off) as usize as *const u8;
        match r.kind {
            RegionKind::ArenaState => {
                let mut w = [0u32; STATE_WORDS];
                // SAFETY: 置き場の中の STATE_BYTES バイトを、スタックの語の配列へコピーするだけ。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src,
                        w.as_mut_ptr().cast::<u8>(),
                        STATE_BYTES as usize,
                    )
                };
                state_words = Some(w);
            }
            RegionKind::Arena => {
                let n = r.len.min(r.cap);
                // SAFETY: 先は DLL の領域の `[base, base + n)`（n はコピーしたときの top ≤ 領域の長さ）。元は置き場の中。
                unsafe {
                    core::ptr::copy_nonoverlapping(src, r.addr as usize as *mut u8, n as usize)
                };
                if cur_top > n {
                    // SAFETY: `[base + n, base + cur_top)` は領域の中（cur_top ≤ 領域の長さ）。配っていない所を 0 に戻す。
                    unsafe {
                        core::ptr::write_bytes(
                            (r.addr + n) as usize as *mut u8,
                            0,
                            (cur_top - n) as usize,
                        )
                    };
                }
            }
            _ => {
                let n = r.len.min(r.cap) as usize;
                if n == 0 {
                    continue;
                }
                // SAFETY: 先は `fill` が書ける頁と確かめた範囲（コピーしたときと同じアドレスと長さ。呼び手がアドレスが変わっていないことを確かめてから書き戻す）。元は置き場の中。ゲームは動いていない。
                unsafe { core::ptr::copy_nonoverlapping(src, r.addr as usize as *mut u8, n) };
            }
        }
    }
    if let Some(w) = state_words {
        a.state = ArenaState::from_words(&w);
    }
}

impl RealProbeIo<'_> {
    /// D1 の置き場と今の状態を範囲ごとに比べる。`diag` が `Some` なら診断を数え、`skip` が `Dead` なら描画用の欄を外す（純粋な判断は `rollback_draw`）。
    fn compare_impl(
        &mut self,
        t: &Tables,
        l: &Layout,
        marks: &Marks,
        out: &mut [RegionResult],
        skip: SkipDraw,
        mut diag: Option<&mut DrawDiag>,
    ) {
        let buf = self.d1;
        let Some(a) = self.arena() else { return };
        let cur_words = a.state.to_words();
        if let Some(d) = diag.as_deref_mut() {
            d.begin(ROLLBACK_SLOT_ARRAYS, skip);
        }
        let d1_state = l
            .regions()
            .iter()
            .find(|r| r.kind == RegionKind::ArenaState)
            .map_or_else(ArenaState::new, |r| {
                let mut w = [0u32; STATE_WORDS];
                // SAFETY: 置き場の `[buf_off, buf_off + STATE_BYTES)` は `arm` が確保したコピーの置き場の中。先はスタックの語の配列。重ならない。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (buf + r.buf_off) as usize as *const u8,
                        w.as_mut_ptr().cast::<u8>(),
                        STATE_BYTES as usize,
                    )
                };
                ArenaState::from_words(&w)
            });
        for (r, o) in l.regions().iter().zip(out.iter_mut()) {
            let len1 = r.len.min(r.cap);
            let len2 = current_len(r.kind, r.len, a);
            // SAFETY: `a1` は置き場の中の `[buf_off, buf_off + len1)`。`a2` は今の状態（割り当て器の状態はスタック、領域は `[base, base + top)`、ほかは D1 と同じ範囲）。ゲームは動いていない（別のスレッドが .data を書く所はコピーする範囲に入れていない）。
            let a1 = unsafe {
                core::slice::from_raw_parts((buf + r.buf_off) as usize as *const u8, len1 as usize)
            };
            let a2: &[u8] = match r.kind {
                RegionKind::ArenaState => unsafe {
                    core::slice::from_raw_parts(
                        cur_words.as_ptr().cast::<u8>(),
                        STATE_BYTES as usize,
                    )
                },
                _ => unsafe {
                    core::slice::from_raw_parts(r.addr as usize as *const u8, len2 as usize)
                },
            };
            // 比べない所（`th09_addr::ROLLBACK_COMPARE_SKIPS`）: fps の計測の語、fps の位置にある文字の枠の文字の欄、領域の中は記録の処理の塊のうち書き位置が進んだバイト。領域は今（D2）の割り当て器を歩く。
            let skips = region_skips(t, r, a1, a2, (&a.state, &a.mem), marks);
            // 診断のフックが無いときは今までの比べ。有るときは、範囲が枠の配列か arena なら診断を数え、`skip` が `Dead` なら死んでいる枠・塊の描画用の欄を外す。
            *o = match diag.as_deref_mut() {
                Some(d) if r.kind == RegionKind::Arena => match arena_row(d) {
                    Some((row, walk)) => compare_arena(
                        r,
                        ROLLBACK_ARENA_VM_TAGS,
                        (a1, a2),
                        &skips,
                        skip,
                        (&d1_state, &a.state, &a.mem),
                        (row, walk),
                    ),
                    None => RegionResult::of(r, a1, a2, &skips),
                },
                Some(d) => {
                    let name = region_name(t, r.kind);
                    compare_region_slots(r, (name, ROLLBACK_SLOT_ARRAYS), (a1, a2), &skips, skip, d)
                        .unwrap_or_else(|| RegionResult::of(r, a1, a2, &skips))
                }
                None => RegionResult::of(r, a1, a2, &skips),
            };
        }
    }
}

pub fn armed() -> bool {
    ARMED.load(Ordering::Acquire) != 0
}

/// 入力の地点（台本の経路の先頭。ゲームのスレッド）。`Some(r)` なら入口へ `r` を返す（記録した語を feed した）。`feed` は「ローカル入力を 1 回読み、3 本へ feed」。
pub fn on_tick(feed: &mut dyn FnMut(u16, u16)) -> Option<u32> {
    if !armed() {
        return None;
    }
    // SAFETY: `ARMED` を Acquire で 1 と読んだので、`arm` が置いた入れ物が見える。触るのはゲームのスレッドだけ。
    let pb = unsafe { (*PROBE.0.get()).as_mut() }?;
    let mut io = RealProbeIo {
        feed,
        snap: pb.snap,
        d1: pb.d1,
        qpf: pb.qpf,
    };
    match catch_unwind(AssertUnwindSafe(|| pb.probe.on_tick(&mut io))) {
        Ok(r) => r,
        Err(_) => {
            // パニック: 確保の向け先を戻して、以後は何もしない（ゲームは元のまま続く）。
            RB_REDIRECT.store(0, Ordering::SeqCst);
            RB_WATCH.store(0, Ordering::SeqCst);
            ARMED.store(0, Ordering::SeqCst);
            None
        }
    }
}

pub fn after_feed(p1: u16, p2: u16) {
    if !armed() {
        return;
    }
    // SAFETY: `on_tick` と同じ。
    if let Some(pb) = unsafe { (*PROBE.0.get()).as_mut() } {
        pb.probe.after_feed(p1, p2);
    }
}

// ---- 走査の入口（判断は `rollback_resim.rs`）----
/// 元の走査（`RESIM_SITES` の行の元の飛び先 `0x0042C700`）。この入口へ向く call を書く前に `install_call_hook_at` の `prepare` が置く。
static SCAN_ORIGINAL: AtomicU32 = AtomicU32::new(0);
/// 追加の走査 1 回ごとの時間（マイクロ秒）の標本。書き手はゲームのスレッドだけで、標本を書いてから Release で数を進める。
static SAMPLES: [AtomicU32; MAX_SAMPLES] = [const { AtomicU32::new(0) }; MAX_SAMPLES];
static SAMPLE_COUNT: AtomicU32 = AtomicU32::new(0);
static SAMPLE_DROPPED: AtomicU32 = AtomicU32::new(0);

struct SavedCell(UnsafeCell<SavedOutputs>);
// SAFETY: 触るのはゲームのスレッドだけ（入力の地点の `capture_outputs` と、走査の入口の `before_last_call`）。読み取りのスレッドは触らない。
unsafe impl Sync for SavedCell {}
static SAVED: SavedCell = SavedCell(UnsafeCell::new(SavedOutputs::EMPTY));

// SAFETY（`#[unsafe(naked)]`）: `0x0042DCA9` の `call 0x42c700` の差し替え先。入った時は ecx = 更新の列のレジストリ `0x4ACFC8`（thiscall の this）、引数は無く、元の走査は `ret`。
// ecx を積んで Rust の `rb_scan_wrapper`（cdecl、引数 1 つ、戻り値 eax）を呼び、`add esp, 4` で片付けて `ret`。ebx・esi・edi・ebp と esp は cdecl の決まりで保たれる
// （do_frame は esi = ウィンドウ・edi を走査をまたいで使う）。DF は `cld` で 0。x87 のスタックは call の境目で空で、制御語は変えない。読むのは自分のスタックだけ。
/// 走査の call（`0x0042DCA9`）の差し替え先。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn rb_scan_entry() {
    core::arch::naked_asm!(
        "push ecx",
        "cld",
        "call {wrapper}",
        "add esp, 4",
        "ret",
        wrapper = sym rb_scan_wrapper,
    )
}

fn call_original(this: u32) -> i32 {
    let f = SCAN_ORIGINAL.load(Ordering::Relaxed);
    // SAFETY: `f` は `RESIM_SITES` の行の元の飛び先 `0x0042C700`（thiscall）で、この入口へ向く call を書く前に置かれるので、呼ばれるときは 0 でない。
    let f: unsafe extern "thiscall" fn(u32) -> i32 = unsafe { core::mem::transmute(f as usize) };
    // SAFETY: 上と同じ。走査の中のパニックはゲームのコード（Rust ではない）の外へは出ない。
    unsafe { f(this) }
}

/// 走査の包みで先に聞くフック（rollback のモードの配線が置く）。`Some(ret)` なら、そのフックが走査を呼び終えて `ret` を do_frame へ返す。0 は置いていない。
static SCAN_HANDLER: AtomicU32 = AtomicU32::new(0);

/// 走査の包みで先に聞くフックの型（ゲームのスレッドで呼ばれる。`this` は走査の this = 更新の列のレジストリ）。
pub type ScanHandler = fn(this: u32) -> Option<i32>;

/// 走査の包みで先に聞くフックを置く・外す（`script_win::install_rollback_wiring` が呼ぶ。`None` で外す）。
pub fn set_scan_handler(h: Option<ScanHandler>) {
    SCAN_HANDLER.store(h.map_or(0, |f| f as usize as u32), Ordering::Release);
}

/// 走査の包みのフックが置かれているか（検査が、観戦者の設置がフックまで置いたことを見る）。
#[cfg(test)]
pub(crate) fn scan_handler_is_set() -> bool {
    SCAN_HANDLER.load(Ordering::Acquire) != 0
}

/// 元の走査を 1 回呼ぶ（走査の call を書く前は呼ばない —— `scan_hooked` を先に見る）。
pub(crate) fn call_scan(this: u32) -> i32 {
    call_original(this)
}

pub(crate) fn scan_hooked() -> bool {
    SCAN_ORIGINAL.load(Ordering::Acquire) != 0
}

/// 走査の元の飛び先を置く（設置が、走査の call を書く前に呼ぶ —— `install_call_hook_at` の `prepare`）。
pub(crate) fn set_scan_original(addr: u32) {
    SCAN_ORIGINAL.store(addr, Ordering::SeqCst);
}

pub(crate) fn scan_entry_address() -> u32 {
    rb_scan_entry as *const () as usize as u32
}

extern "C" fn rb_scan_wrapper(this: u32) -> i32 {
    let h = SCAN_HANDLER.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: `h` は `set_scan_handler` が `ScanHandler` のアドレスを 32 ビットにして置いた値で、0 でない。関数のアドレスと整数は 32 ビットで同じ大きさ。
        let f: ScanHandler = unsafe { core::mem::transmute::<usize, ScanHandler>(h as usize) };
        match catch_unwind(AssertUnwindSafe(|| f(this))) {
            Ok(Some(r)) => return r,
            Ok(None) => {}
            Err(_) => {
                // パニック: フックを外す。フックの中で走査を呼び終えていたかは分からないので、走査を呼び直さず 1（描画へ進む値）を返す（同じフレームで 2 回更新しない）。
                SCAN_HANDLER.store(0, Ordering::SeqCst);
                RB_REDIRECT.store(0, Ordering::SeqCst);
                return 1;
            }
        }
    }
    if !armed() {
        return call_original(this);
    }
    // SAFETY: `ARMED` を Acquire で 1 と読んだので、`arm` が置いた入れ物が見える。触るのはゲームのスレッド（ここ）だけ。
    let Some(qpf) = (unsafe { (*PROBE.0.get()).as_ref() }).map(|pb| pb.qpf) else {
        return call_original(this);
    };
    let mut io = RealScan {
        this,
        qpf,
        last_us: 0,
        // 走査を 1 回も呼ぶ前にパニックしたときの戻り値（2 = ノードを通った。do_frame は描画へ進む）。
        last_ret: 2,
    };
    match catch_unwind(AssertUnwindSafe(|| rollback_resim::run_scan(&mut io))) {
        Ok(r) => r,
        Err(_) => {
            // パニック: 確保の向け先を戻して、以後は何もしない（ゲームは元のまま続く）。最後に受けた戻り値を返す。
            RB_REDIRECT.store(0, Ordering::SeqCst);
            RB_WATCH.store(0, Ordering::SeqCst);
            ARMED.store(0, Ordering::SeqCst);
            io.last_ret
        }
    }
}

pub(crate) fn scene_request_pending_now() -> bool {
    let mut m = OwnProcess;
    match (
        m.u32_at(SUPERVISOR + SUP_SCENE_REQ),
        m.u32_at(SUPERVISOR + SUP_SCENE_CUR),
    ) {
        (Some(req), Some(cur)) => req != cur,
        _ => true,
    }
}

/// `scene_request_pending_now` の直読みバージョン（観戦者の早送り用）。余分な走査ごとに見るので `ReadProcessMemory` の syscall を避け、`rollback_play_win::read_fast` で直に読む。読めなければ true。
pub(crate) fn scene_request_pending_fast() -> bool {
    use crate::rollback_play_win::read_fast;
    match (
        read_fast(SUPERVISOR + SUP_SCENE_REQ),
        read_fast(SUPERVISOR + SUP_SCENE_CUR),
    ) {
        (Some(req), Some(cur)) => req != cur,
        _ => true,
    }
}

fn push_sample(us: u32) {
    let n = SAMPLE_COUNT.load(Ordering::Relaxed) as usize;
    match SAMPLES.get(n) {
        Some(slot) => {
            slot.store(us, Ordering::Relaxed);
            SAMPLE_COUNT.store(n as u32 + 1, Ordering::Release);
        }
        None => {
            SAMPLE_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 観戦者の早送り（`spectate_watch/ff.rs`）が、最初の走査の前に文字の数をバックアップする。読めなければ `None`（その表示では早送りしない）。
pub(crate) fn ff_read_ascii() -> Option<u32> {
    RawOutputs.read_u32(ASCII_COUNT)
}

/// 観戦者の早送りが、余分な走査の前に呼ぶ: 文字の数を `ascii0` へ戻す。書けたら真。SE と BGM の待ち行列は触らない（余分な回の SE はゲームが積んだとおり待ち行列に残る）。
pub(crate) fn ff_drop_outputs(ascii0: u32) -> bool {
    RawOutputs.write_u32(ASCII_COUNT, ascii0)
}

/// 出力（SE・BGM・文字の数）を語で読み書きする本物。`OUTPUT_RANGES` の外は断る（`arm` がその 3 つの範囲が書ける頁と確かめた）。
struct RawOutputs;

impl OutputMem for RawOutputs {
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        if !rollback_resim::output_addr_allowed(addr) {
            return None;
        }
        // SAFETY: `addr` は `OUTPUT_RANGES` の中の 4 バイト境界の語で、`arm` が読み書きできる頁と確かめた。ゲームのスレッドで、ゲームは動いていない（入力の地点か走査の間）。
        Some(unsafe { (addr as usize as *const u32).read() })
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        if !rollback_resim::output_addr_allowed(addr) {
            return false;
        }
        // SAFETY: `read_u32` と同じ。
        unsafe { (addr as usize as *mut u32).write(v) };
        true
    }
}

/// 検査の入口（`output_ranges_rw` を頁を見ずに真にする）。
#[cfg(test)]
pub(crate) static TEST_OUTPUT_RANGES_OK: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub(crate) fn output_ranges_rw() -> bool {
    // 検査のプロセスには th09.exe の .data の頁が無いので、検査が設置の配線を通すときだけ頁を見ずに真にする。
    #[cfg(test)]
    if TEST_OUTPUT_RANGES_OK.load(Ordering::SeqCst) {
        return true;
    }
    OUTPUT_RANGES.iter().all(|&(s, e)| is_rw(s, e - s))
}

struct RealScan {
    this: u32,
    qpf: u64,
    last_us: u32,
    last_ret: i32,
}

impl RealScan {
    fn probe() -> Option<&'static mut ProbeBox> {
        // SAFETY: `rb_scan_wrapper` が `ARMED` を Acquire で 1 と読んだ後。触るのはゲームのスレッドだけで、入力の地点の `on_tick` の借用は走査の中で終わっている。
        unsafe { (*PROBE.0.get()).as_mut() }
    }
}

impl ScanIo for RealScan {
    fn call_scan(&mut self) -> i32 {
        let t0 = qpc();
        let r = call_original(self.this);
        let t1 = qpc();
        self.last_us = if self.qpf == 0 {
            0
        } else {
            (u128::from(t1.saturating_sub(t0)) * 1_000_000 / u128::from(self.qpf))
                .min(u128::from(u32::MAX)) as u32
        };
        self.last_ret = r;
        r
    }

    fn take_owed(&mut self) -> Option<u32> {
        Self::probe().and_then(|pb| pb.probe.take_owed())
    }

    fn before_last_call(&mut self) {
        // SAFETY: ゲームのスレッドだけが触る（`SavedCell` の Sync の注）。
        let saved = unsafe { &*SAVED.0.get() };
        let ok = rollback_resim::apply_outputs(&mut RawOutputs, saved);
        if let Some(pb) = Self::probe() {
            pb.probe.note_outputs(ok);
        }
    }

    fn after_call(&mut self, index: u32, _ret: i32) {
        let req = scene_request_pending_now();
        if let Some(pb) = Self::probe() {
            pb.probe.note_scan(index, self.last_us, req);
        }
        if index >= 1 {
            push_sample(self.last_us);
        }
    }

    fn aborted(&mut self, _index: u32, why: ScanAbort) {
        if let Some(pb) = Self::probe() {
            pb.probe.note_abort(why);
        }
    }
}

/// 走査の call を DLL の入口へ向ける（フレームの形のときだけ。書けたら true）。ここで表の検査と書く前の照合を通す。
fn install_resim_hook(env: &mut dyn HookEnv, forbidden: &[AddrRange], out: &mut dyn Write) -> bool {
    if cfg!(feature = "dist") {
        let _ = writeln!(
            out,
            "巻き戻しの実験: resim = frame は配るバージョンでは書かない。realtime のまま動く"
        );
        return false;
    }
    if let Err(e) = check_table(RESIM_SITES, forbidden) {
        let _ = writeln!(
            out,
            "巻き戻しの実験: 走査の call の表が検査に落ちた（{e:?}）ので書かない。realtime のまま動く"
        );
        return false;
    }
    let Some(def) = RESIM_SITES.iter().find(|d| d.name == RESIM_SCAN_SITE) else {
        let _ = writeln!(
            out,
            "巻き戻しの実験: 表に {RESIM_SCAN_SITE} の行が無いので書かない。realtime のまま動く"
        );
        return false;
    };
    if !output_ranges_rw() {
        let _ = writeln!(
            out,
            "巻き戻しの実験: SE・BGM・文字の数の欄が読み書きできる頁でないので書かない。realtime のまま動く"
        );
        return false;
    }
    let entry = rb_scan_entry as *const () as usize as u32;
    let r = hook::install_call_hook_at(env, def, entry, &mut |_, original| {
        SCAN_ORIGINAL.store(original, Ordering::SeqCst);
    });
    hook::write_report(out, def, &r);
    if !r.written() {
        let _ = writeln!(
            out,
            "巻き戻しの実験: 走査の call を書けなかったので realtime のまま動く"
        );
    }
    r.written()
}

/// 確保・解放の入口（malloc・operator new・free・calloc）のアドレス。ほかの地点は `None`（数える入口）。
pub(crate) fn rb_entry_address(site: usize) -> Option<u32> {
    let f: extern "C" fn() = match site {
        SITE_MALLOC => rb_malloc_entry,
        SITE_NEW => rb_new_entry,
        SITE_FREE => rb_free_entry,
        crate::rollback_play::SITE_CALLOC => rb_calloc_entry,
        _ => return None,
    };
    Some(f as *const () as usize as u32)
}

pub(crate) fn set_rb_trampoline(site: usize, addr: u32) {
    if site == crate::rollback_play::SITE_CALLOC {
        RB_CALLOC_TRAMP.store(addr, Ordering::SeqCst);
    } else if let Some(t) = ALLOC_TRAMPS.get(site) {
        t.store(addr, Ordering::SeqCst);
    }
}

/// 設置の入口（`alloc_count::install_all` へ渡す AllocHost。入口のアドレスとトランポリンの置き場だけを使う）。
struct RbHost;

impl AllocHost for RbHost {
    fn read_ini(&mut self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn game_thread_id(&mut self) -> Option<u32> {
        None
    }
    fn open_outputs(&mut self) -> Result<String, String> {
        Ok(String::new())
    }
    fn prepare_counters(&mut self, main_tid: u32) {
        ALLOC_MAIN_TID.store(main_tid, Ordering::SeqCst);
    }
    fn entry_address(&self, site: usize) -> u32 {
        match rb_entry_address(site) {
            Some(a) => a,
            None => crate::alloc_win::entry_address(site),
        }
    }
    fn set_trampoline(&mut self, site: usize, addr: u32) {
        set_rb_trampoline(site, addr);
    }
    fn start_writer(&mut self, _s: &AllocSettings, _i: [bool; SITE_COUNT]) -> Result<(), String> {
        Ok(())
    }
    fn finish(&mut self) -> Option<Vec<String>> {
        None
    }
}

pub enum Armed {
    /// ini に `[rollback_probe]` が無い（数える入口の経路へ）。
    NotConfigured,
    /// 扱った（入れたか、理由を書いて入れなかった）。数える入口の経路へは進まない。
    Handled,
}

fn virtual_alloc(bytes: u32) -> Option<u32> {
    // SAFETY: 新しい領域を予約してコミットするだけ（読み書きできる頁。0 で埋まっている）。
    let p = unsafe {
        VirtualAlloc(
            core::ptr::null(),
            bytes as usize,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    (!p.is_null()).then_some(p as usize as u32)
}

/// ini の本文を読み、`[rollback_probe]` があれば設置する（入力のフックを書いた後・台本の分岐を開く前）。
pub fn arm(
    text: &str,
    hook_env: &mut dyn DetourEnv,
    forbidden: &[AddrRange],
    main_tid: Option<u32>,
    out: &mut dyn Write,
) -> Armed {
    let s: Settings = match crate::rollback_probe::settings_from_text(text) {
        Ok(None) => return Armed::NotConfigured,
        Ok(Some(s)) => s,
        Err(e) => {
            let _ = writeln!(out, "巻き戻しの実験: ini が読めない（{e}）ので入れない");
            return Armed::Handled;
        }
    };
    if matches!(alloc_count::settings_from_text(text), Ok(Some(_))) {
        let _ = writeln!(
            out,
            "巻き戻しの実験: [alloc_count] と同時には入れない（同じ 5 地点を使う）。どちらかを enable = 0 に"
        );
        return Armed::Handled;
    }
    if crate::rollback_play_win::armed() || RB_BASE.load(Ordering::SeqCst) != 0 {
        let _ = writeln!(
            out,
            "巻き戻しの実験: rollback のモードの製品の経路がもう入っている（同じ入口と領域を使う）ので入れない"
        );
        return Armed::Handled;
    }
    let Some(tid) = main_tid else {
        let _ = writeln!(
            out,
            "巻き戻しの実験: ゲームのスレッドの ID が取れないので入れない"
        );
        return Armed::Handled;
    };
    let Some(base) = virtual_alloc(s.arena_bytes) else {
        let _ = writeln!(
            out,
            "巻き戻しの実験: 領域 {} バイトを確保できないので入れない",
            s.arena_bytes
        );
        return Armed::Handled;
    };
    let mut probe = match Probe::new(s, Tables::th09(), base) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(out, "巻き戻しの実験: 範囲の表を作れない（{e}）ので入れない");
            return Armed::Handled;
        }
    };
    let skip_draw = crate::rollback_probe::skip_draw_from_text(text).unwrap_or_default();
    probe.set_skip_draw(skip_draw);
    let buf_bytes = probe.plan().buffer_bytes();
    let (Some(snap), Some(d1)) = (virtual_alloc(buf_bytes), virtual_alloc(buf_bytes)) else {
        let _ = writeln!(
            out,
            "巻き戻しの実験: コピーの置き場 {buf_bytes} バイト × 2 を確保できないので入れない"
        );
        return Armed::Handled;
    };
    let mut qpf: i64 = 0;
    // SAFETY: 書く先は自分の変数。
    unsafe { QueryPerformanceFrequency(&mut qpf) };
    // SAFETY: 置くのは読み取りのスレッドの 1 回だけ。まだ `ARMED` も `RB_BASE` も立てておらず、ゲームのスレッドは入れ物に触らない。
    unsafe {
        *ARENA.0.get() = Some(Arena {
            state: ArenaState::new(),
            mem: RawMem {
                base,
                len: s.arena_bytes,
            },
        });
        *PROBE.0.get() = Some(ProbeBox {
            probe,
            snap,
            d1,
            qpf: qpf as u64,
        });
        *RESULTS.0.get() = Some(vec![TrialRecord::EMPTY; MAX_RECORDS].into_boxed_slice());
    }
    RESULT_COUNT.store(0, Ordering::SeqCst);
    SAMPLE_COUNT.store(0, Ordering::SeqCst);
    SAMPLE_DROPPED.store(0, Ordering::SeqCst);
    STOP.store(0, Ordering::SeqCst);
    RB_BASE.store(base, Ordering::SeqCst);
    RB_END.store(base + s.arena_bytes, Ordering::SeqCst);
    alloc_count::reset_counters();
    let mut host = RbHost;
    host.prepare_counters(tid);
    let _ = writeln!(
        out,
        "巻き戻しの実験: 領域 0x{base:08X}（{} MiB）、コピーの置き場 {buf_bytes} バイト × 2、範囲 {} 個、k = {:?}、各 {} 回、最初は試合の {} tick 目、間 {} tick、進め直しの形 {}",
        s.arena_bytes >> 20,
        probe_regions(),
        s.ks(),
        s.trials_per_k,
        s.start_ticks,
        s.gap_ticks,
        s.resim.name()
    );
    let _ = writeln!(
        out,
        "巻き戻しの実験: 描画用の VM の欄の除外 {}（skip_draw_fields。none は数えるだけ、dead は D1 で死んでいる枠の欄を比べから外す、all は生死に関係なく枠の配列の VM と使用中の VM の配列の欄を比べから外す。診断は比べから外す前の違いを、どれでも数える）",
        skip_draw.name()
    );
    let written = alloc_count::install_all(hook_env, &mut host, ALLOC_SITES, forbidden, out);
    if written[SITE_MALLOC] && written[SITE_NEW] && written[SITE_FREE] {
        // フレームの形は走査の call も書く。書けなければ realtime へ戻す。
        if s.resim == ResimMode::Frame && !install_resim_hook(hook_env, forbidden, out) {
            // SAFETY: 置いた後・`ARMED` を立てる前なので、ゲームのスレッドはまだ入れ物に触らない。
            if let Some(pb) = unsafe { (*PROBE.0.get()).as_mut() } {
                pb.probe.set_resim(ResimMode::Realtime);
            }
        }
        let calloc = install_calloc(hook_env, forbidden, out);
        ARMED.store(1, Ordering::Release);
        let _ = writeln!(
            out,
            "巻き戻しの実験: 入れた（malloc・new・free の 3 地点を DLL の入口へ。包み 2 つは数えるだけ。calloc の見張り {}）",
            if calloc { "あり" } else { "なし" }
        );
    } else {
        let _ = writeln!(
            out,
            "巻き戻しの実験: 3 地点のどれかが書けなかったので、確保は向けない（{written:?}）"
        );
    }
    Armed::Handled
}

/// calloc の入口を 1 地点だけ書く（表の検査と照合は `install_detour` の中。合わなければ書かない）。
fn install_calloc(
    hook_env: &mut dyn DetourEnv,
    forbidden: &[AddrRange],
    out: &mut dyn Write,
) -> bool {
    let Some(def) = CALLOC_SITES.first() else {
        return false;
    };
    if let Err(e) = check_table(CALLOC_SITES, forbidden) {
        let _ = writeln!(
            out,
            "巻き戻しの実験: calloc の表が検査に落ちた（{e:?}）ので書かない"
        );
        return false;
    }
    let mut host = RbHost;
    let mut env = SiteDetourEnv {
        inner: hook_env,
        host: &mut host,
        site: crate::rollback_play::SITE_CALLOC,
    };
    let r = crate::detour::install_detour(&mut env, def, forbidden);
    crate::detour::write_report(out, def, &r);
    r.written()
}

fn probe_regions() -> usize {
    // SAFETY: `arm` の中（置いた直後、ゲームのスレッドがまだ触らない）からだけ呼ぶ。
    unsafe { (*PROBE.0.get()).as_ref() }.map_or(0, |p| p.probe.plan().n)
}

pub fn finish(out: &mut dyn Write) {
    // SAFETY: 読むだけ（`Probe` の設定と範囲の表は置いた後に変わらない）。
    let Some(pb) = (unsafe { (*PROBE.0.get()).as_ref() }) else {
        return;
    };
    STOP.store(1, Ordering::Release);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let n = RESULT_COUNT.load(Ordering::Acquire) as usize;
    let _ = writeln!(out, "==== 巻き戻しの実験（E1-2） ====");
    // SAFETY: `[0, n)` はゲームのスレッドが書き終えて数を進めた記録（Acquire で数を読んだ）。
    let Some(rs) = (unsafe { (*RESULTS.0.get()).as_ref() }) else {
        return;
    };
    let rs = &rs[..n.min(rs.len())];
    let t = Tables::th09();
    for r in rs {
        for l in crate::rollback_probe::trial_lines(&t, pb.probe.plan(), r) {
            let _ = writeln!(out, "{l}");
        }
        for l in rollback_draw::trial_lines(&r.draw) {
            let _ = writeln!(out, "{l}");
        }
    }
    for l in crate::rollback_probe::summary_lines(&t, pb.probe.settings(), rs) {
        let _ = writeln!(out, "{l}");
    }
    for l in rollback_draw::summary_lines(rs, pb.probe.skip_draw()) {
        let _ = writeln!(out, "{l}");
    }
    let ns = (SAMPLE_COUNT.load(Ordering::Acquire) as usize).min(MAX_SAMPLES);
    let samples: Vec<u32> = SAMPLES[..ns]
        .iter()
        .map(|a| a.load(Ordering::Relaxed))
        .collect();
    let _ = writeln!(
        out,
        "巻き戻しの実験: 進め直しの形 {}",
        pb.probe.settings().resim.name()
    );
    for l in rollback_resim::summary_lines(rs, &samples, SAMPLE_DROPPED.load(Ordering::Relaxed)) {
        let _ = writeln!(out, "{l}");
    }
    let _ = writeln!(
        out,
        "巻き戻しの実験: 記録 {n} 件（予定の回を{}）",
        if pb.probe.finished() {
            "全部終えた"
        } else {
            "終える前に止めた"
        }
    );
    let _ = writeln!(
        out,
        "巻き戻しの実験: 領域の外の解放（見張りの間）{} / 別のスレッドの領域の解放 {} / 断った解放 {} / 0 で埋められなかった解放 {} / 領域が足りない {} / 状態の外の確保（見張りの間）{}（うち calloc {}） / 窓の中で生まれた外の塊の解放 {} / アドレスを控えられなかった外の確保 {} / 同時に控えた外の塊の最大 {}（枠 {}）",
        RB_OUT_FREES.load(Ordering::Relaxed),
        RB_FOREIGN_FREES.load(Ordering::Relaxed),
        RB_BAD_FREES.load(Ordering::Relaxed),
        RB_UNZEROED.load(Ordering::Relaxed),
        RB_ARENA_FULL.load(Ordering::Relaxed),
        RB_OUT_ALLOCS.load(Ordering::Relaxed),
        RB_OUT_CALLOCS.load(Ordering::Relaxed),
        RB_OUT_FREES_BORN.load(Ordering::Relaxed),
        RB_OUT_UNTRACKED.load(Ordering::Relaxed),
        RB_OUT_BORN_MAX.load(Ordering::Relaxed),
        crate::rollback_probe::BORN_CAP
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_only_game_code_outside_the_excluded_ranges() {
        let inside = GAME_CODE_START + 0x100;
        assert!(redirectable(inside, 0));
        assert!(
            redirectable(ALLOC_WRAP_RET, inside),
            "包みの中からは包みの呼び出し元で見る"
        );
        assert!(!redirectable(ALLOC_WRAP_RET, GAME_CODE_END + 0x10));
        assert!(
            !redirectable(GAME_CODE_END, 0),
            "ゲームのコードの外（D3DX など）"
        );
        assert!(!redirectable(GAME_CODE_START - 1, 0));
        for c in ROLLBACK_EXCLUDED_CALLERS {
            assert!(!redirectable(c.start, 0), "{}", c.why);
            assert!(!redirectable(ALLOC_WRAP_RET, c.end - 1), "{}", c.why);
        }
    }

    static SEEN_ECX: AtomicU32 = AtomicU32::new(0);

    // SAFETY（`#[unsafe(naked)]`）: 元の走査の代わり（検査だけ）。入った時の ecx を書いて、eax に 0x1234 を置き、ecx・edx を壊して `ret` する。
    #[unsafe(naked)]
    extern "C" fn fake_scan() {
        core::arch::naked_asm!(
            "mov dword ptr [{seen}], ecx",
            "mov eax, 0x1234",
            "xor ecx, ecx",
            "xor edx, edx",
            "ret",
            seen = sym SEEN_ECX,
        )
    }

    #[test]
    fn scan_entry_is_transparent_when_not_armed() {
        use crate::stub::testkit::{
            AFTER, PRE_ESP, REAL_HOOK_TESTS, REG_EAX, REG_EBP, REG_EBX, REG_ECX, REG_EDI,
            call_with_regs, fake_this_addr,
        };
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!armed(), "ほかの検査が ARMED を立てたまま");
        SCAN_ORIGINAL.store(fake_scan as *const () as usize as u32, Ordering::SeqCst);
        SEEN_ECX.store(0, Ordering::SeqCst);
        call_with_regs(rb_scan_entry as *const () as usize as u32);
        let after = |i: usize| AFTER[i].load(Ordering::SeqCst);
        assert_eq!(after(0), 0x1234, "元の走査の戻り値");
        assert_eq!(SEEN_ECX.load(Ordering::SeqCst), REG_ECX, "this");
        assert_eq!(after(3), REG_EBX);
        assert_eq!(after(4), fake_this_addr());
        assert_eq!(after(5), REG_EDI);
        assert_eq!(after(6), REG_EBP);
        assert_eq!(after(7), PRE_ESP.load(Ordering::SeqCst), "esp");
        assert_ne!(after(0), REG_EAX);
        SCAN_ORIGINAL.store(0, Ordering::SeqCst);
    }

    static HANDLER_THIS: AtomicU32 = AtomicU32::new(0);
    static HANDLER_HITS: AtomicU32 = AtomicU32::new(0);

    fn handler_takes(this: u32) -> Option<i32> {
        HANDLER_THIS.store(this, Ordering::SeqCst);
        HANDLER_HITS.fetch_add(1, Ordering::SeqCst);
        Some(0x77)
    }

    fn handler_passes(this: u32) -> Option<i32> {
        HANDLER_THIS.store(this, Ordering::SeqCst);
        HANDLER_HITS.fetch_add(1, Ordering::SeqCst);
        None
    }

    #[test]
    fn scan_entry_asks_the_play_handler_first() {
        use crate::stub::testkit::{AFTER, REAL_HOOK_TESTS, REG_ECX, call_with_regs};
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!armed(), "ほかの検査が ARMED を立てたまま");
        SCAN_ORIGINAL.store(fake_scan as *const () as usize as u32, Ordering::SeqCst);
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let entry = rb_scan_entry as *const () as usize as u32;

        set_scan_handler(Some(handler_takes));
        SEEN_ECX.store(0, Ordering::SeqCst);
        call_with_regs(entry);
        assert_eq!(AFTER[0].load(Ordering::SeqCst), 0x77, "差し替え先の戻り値");
        assert_eq!(
            SEEN_ECX.load(Ordering::SeqCst),
            0,
            "差し替え先が引き受けたのに元の走査も呼んだ"
        );
        assert_eq!(
            HANDLER_THIS.load(Ordering::SeqCst),
            REG_ECX,
            "差し替え先に this を渡していない"
        );

        set_scan_handler(Some(handler_passes));
        call_with_regs(entry);
        assert_eq!(AFTER[0].load(Ordering::SeqCst), 0x1234, "元の走査の戻り値");
        assert_eq!(
            SEEN_ECX.load(Ordering::SeqCst),
            REG_ECX,
            "差し替え先が断ったのに元の走査を呼ばない"
        );

        set_scan_handler(None);
        call_with_regs(entry);
        assert_eq!(
            HANDLER_HITS.load(Ordering::SeqCst),
            2,
            "外した差し替え先に聞いた"
        );
        SCAN_ORIGINAL.store(0, Ordering::SeqCst);
    }

    static FAKE_FREE_LAST: AtomicU32 = AtomicU32::new(0);
    static FAKE_FREE_HITS: AtomicU32 = AtomicU32::new(0);

    extern "C" fn fake_free(ptr: u32) {
        FAKE_FREE_LAST.store(ptr, Ordering::SeqCst);
        FAKE_FREE_HITS.fetch_add(1, Ordering::SeqCst);
    }

    // SAFETY（`#[unsafe(naked)]`。検査だけ）: cdecl の関数（引数 1 つ）。受けた 1 つを積み直して解放の入口を呼び（ゲームの `call 0x47b163` と同じ形）、片付けて戻る。ebx・esi・edi・ebp は入口も `fake_free` も保つ（cdecl）。
    #[unsafe(naked)]
    extern "C" fn call_free_entry(_ptr: u32) {
        core::arch::naked_asm!(
            "push dword ptr [esp + 4]",
            "call {entry}",
            "add esp, 4",
            "ret",
            entry = sym rb_free_entry,
        )
    }

    #[test]
    fn free_entry_holds_only_old_outside_blocks_while_predicting() {
        use crate::stub::testkit::REAL_HOOK_TESTS;
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let saved = (
            RB_WATCH.load(Ordering::SeqCst),
            RB_HOLD_FREES.load(Ordering::SeqCst),
            ALLOC_MAIN_TID.load(Ordering::SeqCst),
            ALLOC_TRAMPS[SITE_FREE].load(Ordering::SeqCst),
            RB_BASE.load(Ordering::SeqCst),
            RB_END.load(Ordering::SeqCst),
        );
        ALLOC_TRAMPS[SITE_FREE].store(fake_free as *const () as usize as u32, Ordering::SeqCst);
        RB_BASE.store(0, Ordering::SeqCst);
        RB_END.store(0, Ordering::SeqCst);
        OUT_BORN.clear();
        HELD.clear();
        // SAFETY: 引数の無い呼び出し。
        let me = unsafe { GetCurrentThreadId() };
        ALLOC_MAIN_TID.store(me, Ordering::SeqCst);
        let hits = || FAKE_FREE_HITS.load(Ordering::SeqCst);

        RB_WATCH.store(0, Ordering::SeqCst);
        RB_HOLD_FREES.store(1, Ordering::SeqCst);
        let h = hits();
        call_free_entry(0x0BAB_0FD8);
        assert_eq!(hits(), h + 1, "見張りの外で止めた");
        assert_eq!(FAKE_FREE_LAST.load(Ordering::SeqCst), 0x0BAB_0FD8);

        RB_WATCH.store(1, Ordering::SeqCst);
        RB_HOLD_FREES.store(0, Ordering::SeqCst);
        let (h, old) = (hits(), RB_OUT_FREES.load(Ordering::SeqCst));
        call_free_entry(0x0BAB_1050);
        assert_eq!(hits(), h + 1, "旗 0 で止めた（E1 が変わる）");
        assert_eq!(RB_OUT_FREES.load(Ordering::SeqCst), old + 1);
        assert!(!freed_held(0x0BAB_1050));

        RB_HOLD_FREES.store(1, Ordering::SeqCst);
        let (h, held) = (hits(), RB_OUT_FREES_HELD.load(Ordering::SeqCst));
        call_free_entry(0x0BAB_2000);
        assert_eq!(hits(), h, "前からある塊を CRT へ渡した");
        assert_eq!(
            RB_OUT_FREES_HELD.load(Ordering::SeqCst),
            held + 1,
            "数えない"
        );
        assert!(freed_held(0x0BAB_2000), "記録しない");

        assert!(OUT_BORN.add(0x0BAB_3000));
        let h = hits();
        call_free_entry(0x0BAB_3000);
        assert_eq!(hits(), h + 1, "生まれた塊の解放を止めた");
        assert!(!freed_held(0x0BAB_3000));

        let h = hits();
        call_free_entry(0);
        assert_eq!(hits(), h + 1);

        clear_held();
        assert!(!freed_held(0x0BAB_2000), "記録が残った");

        let saved_pending = RB_PENDING.load(Ordering::SeqCst);
        RB_WATCH.store(0, Ordering::SeqCst);
        RB_PENDING.store(1, Ordering::SeqCst);
        let (h, stray) = (hits(), RB_STRAY_FREES.load(Ordering::SeqCst));
        call_free_entry(0x0BAB_4000);
        assert_eq!(hits(), h + 1, "書き戻し待ちの見張りの外で止めた");
        assert_eq!(RB_STRAY_FREES.load(Ordering::SeqCst), stray + 1, "数えない");
        assert!(!freed_held(0x0BAB_4000));
        RB_PENDING.store(0, Ordering::SeqCst);
        call_free_entry(0x0BAB_4000);
        assert_eq!(
            RB_STRAY_FREES.load(Ordering::SeqCst),
            stray + 1,
            "待ちでないのに数えた"
        );

        let saved_count = RB_STRAY_FREES.swap(0, Ordering::SeqCst);
        set_stray_refs(
            77,
            &[0x0BAB_4000],
            &[0x0BAB_5000],
            &[0x0BAB_9000, 0x0BAB_A000],
        );
        assert!(!body_freed(0x0BAB_9000), "解放の前に印が立っている");
        HELD.clear();
        assert!(HELD.add(0x0BAB_6000));
        RB_PENDING.store(1, Ordering::SeqCst);
        for p in [0x0BAB_4000, 0x0BAB_5000, 0x0BAB_6000, 0x0BAB_9000] {
            call_free_entry(p);
        }
        for _ in 0..12 {
            call_free_entry(0x0BAB_7000);
        }
        call_free_entry(0x0BAB_8000);
        RB_PENDING.store(0, Ordering::SeqCst);
        assert_eq!(RB_STRAY_FREES.load(Ordering::SeqCst), 17, "数えない");
        let (recs, n) = stray_recs();
        assert_eq!(n, STRAY_REC_CAP, "記録の数が上限でない");
        let entry = call_free_entry as *const () as usize as u32;
        for r in &recs[..n] {
            assert!(
                r.ret > entry && r.ret < entry + 32,
                "戻りアドレスが違う: {:#X}",
                r.ret
            );
            assert_eq!(r.frame, 77);
        }
        let by = |p: u32| recs[..n].iter().find(|r| r.ptr == p).copied();
        assert_eq!(by(0x0BAB_4000).map(|r| r.flags), Some(StrayRec::NODE));
        assert_eq!(by(0x0BAB_5000).map(|r| r.flags), Some(StrayRec::BLOCK));
        assert_eq!(by(0x0BAB_6000).map(|r| r.flags), Some(StrayRec::HELD));
        assert_eq!(by(0x0BAB_7000).map(|r| r.flags), Some(0));
        assert!(by(0x0BAB_8000).is_none(), "17 件目を記録した");
        assert_eq!(by(0x0BAB_9000).map(|r| r.flags), Some(StrayRec::BODY));
        assert!(body_freed(0x0BAB_9000), "解放が渡った本体に印が無い");
        assert!(!body_freed(0x0BAB_A000), "解放していない本体に印が立った");
        assert!(
            body_freed(0x0BAB_B000),
            "記録した覚えの無いアドレスを解放されていないと見なした"
        );
        RB_PENDING.store(1, Ordering::SeqCst);
        call_free_entry(0x0BAB_A000);
        RB_PENDING.store(0, Ordering::SeqCst);
        assert!(
            body_freed(0x0BAB_A000),
            "上限を越えた後の解放で印が立たない"
        );
        set_stray_refs(78, &[], &[], &[0x0BAB_9000, 0x0BAB_A000]);
        assert!(
            !body_freed(0x0BAB_9000) && !body_freed(0x0BAB_A000),
            "コピーし直しで印が消えない"
        );
        let n_before = RB_STRAY_FREES.load(Ordering::SeqCst);
        call_free_entry(0x0BAB_9000);
        call_free_entry(0x0BAB_C000);
        assert!(
            body_freed(0x0BAB_9000),
            "書き戻し待ちの前の解放で印が立たない"
        );
        assert!(!body_freed(0x0BAB_A000), "解放していない本体に印が立った");
        assert_eq!(
            RB_STRAY_FREES.load(Ordering::SeqCst),
            n_before,
            "書き戻し待ちの前の解放を数えた"
        );
        clear_stray_bodies();
        assert_eq!(STRAY_BODY_N.load(Ordering::SeqCst), 0, "表が空にならない");
        HELD.clear();
        set_stray_refs(0, &[], &[], &[]);
        RB_STRAY_FREES.store(saved_count, Ordering::SeqCst);
        RB_PENDING.store(saved_pending, Ordering::SeqCst);

        RB_WATCH.store(saved.0, Ordering::SeqCst);
        RB_HOLD_FREES.store(saved.1, Ordering::SeqCst);
        ALLOC_MAIN_TID.store(saved.2, Ordering::SeqCst);
        ALLOC_TRAMPS[SITE_FREE].store(saved.3, Ordering::SeqCst);
        RB_BASE.store(saved.4, Ordering::SeqCst);
        RB_END.store(saved.5, Ordering::SeqCst);
        OUT_BORN.clear();
    }

    static FAKE_CALLOC_ARGS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
    static FAKE_CALLOC_HITS: AtomicU32 = AtomicU32::new(0);

    extern "C" fn fake_calloc(num: u32, size: u32) -> u32 {
        FAKE_CALLOC_ARGS[0].store(num, Ordering::SeqCst);
        FAKE_CALLOC_ARGS[1].store(size, Ordering::SeqCst);
        FAKE_CALLOC_HITS.fetch_add(1, Ordering::SeqCst);
        0x0BAD_0000 + num * size
    }

    // SAFETY（`#[unsafe(naked)]`。検査だけ）: cdecl の関数（引数 2 つ）。受けた 2 つを積み直して calloc の入口を呼び、片付けて戻る。
    #[unsafe(naked)]
    extern "C" fn call_calloc_entry(_num: u32, _size: u32) -> u32 {
        core::arch::naked_asm!(
            "push dword ptr [esp + 8]",
            "push dword ptr [esp + 8]",
            "call {entry}",
            "add esp, 8",
            "ret",
            entry = sym rb_calloc_entry,
        )
    }

    #[test]
    fn calloc_entry_counts_only_on_the_game_thread_while_watching() {
        use crate::stub::testkit::REAL_HOOK_TESTS;
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let saved = (
            RB_WATCH.load(Ordering::SeqCst),
            ALLOC_MAIN_TID.load(Ordering::SeqCst),
            RB_CALLOC_TRAMP.load(Ordering::SeqCst),
        );
        RB_CALLOC_TRAMP.store(fake_calloc as *const () as usize as u32, Ordering::SeqCst);
        OUT_BORN.clear();
        // SAFETY: 引数の無い呼び出し。
        let me = unsafe { GetCurrentThreadId() };
        let counts = || {
            (
                RB_OUT_ALLOCS.load(Ordering::SeqCst),
                RB_OUT_CALLOCS.load(Ordering::SeqCst),
            )
        };

        RB_WATCH.store(0, Ordering::SeqCst);
        ALLOC_MAIN_TID.store(me, Ordering::SeqCst);
        let c0 = counts();
        assert_eq!(call_calloc_entry(3, 5), 0x0BAD_0000 + 15);
        assert_eq!(
            (
                FAKE_CALLOC_ARGS[0].load(Ordering::SeqCst),
                FAKE_CALLOC_ARGS[1].load(Ordering::SeqCst)
            ),
            (3, 5)
        );
        assert_eq!(counts(), c0, "見張りの外で数えた");
        assert_eq!(OUT_BORN.len(), 0);

        RB_WATCH.store(1, Ordering::SeqCst);
        let hits = FAKE_CALLOC_HITS.load(Ordering::SeqCst);
        let p = call_calloc_entry(4, 6);
        assert_eq!(p, 0x0BAD_0000 + 24, "戻り値を替えた");
        assert_eq!(
            (
                FAKE_CALLOC_ARGS[0].load(Ordering::SeqCst),
                FAKE_CALLOC_ARGS[1].load(Ordering::SeqCst)
            ),
            (4, 6),
            "個数と大きさの並び"
        );
        assert_eq!(
            FAKE_CALLOC_HITS.load(Ordering::SeqCst),
            hits + 1,
            "元の calloc を 1 回だけ呼ぶ"
        );
        assert_eq!(counts(), (c0.0 + 1, c0.1 + 1), "見張りの間に数えない");
        assert!(OUT_BORN.take(p), "アドレスを記録していない");

        ALLOC_MAIN_TID.store(me.wrapping_add(1), Ordering::SeqCst);
        let c1 = counts();
        assert_eq!(call_calloc_entry(2, 2), 0x0BAD_0000 + 4);
        assert_eq!(counts(), c1, "別のスレッドで数えた");
        assert_eq!(OUT_BORN.len(), 0);

        RB_WATCH.store(saved.0, Ordering::SeqCst);
        ALLOC_MAIN_TID.store(saved.1, Ordering::SeqCst);
        RB_CALLOC_TRAMP.store(saved.2, Ordering::SeqCst);
    }

    #[test]
    fn rw_check_accepts_heap_and_refuses_code_and_holes() {
        let v = vec![0u8; 3 * 4096];
        assert!(is_rw(v.as_ptr() as usize as u32, v.len() as u32));
        let f: fn() = redirect_only_game_code_outside_the_excluded_ranges;
        let code = f as *const () as usize as u32;
        assert!(!is_rw(code, 16), "コードの頁は書けない");
        assert!(!is_rw(0x0000_1000, 16), "確保していないアドレス");
        assert!(is_rw(0, 0));
    }

    #[test]
    fn raw_arena_free_zeroes_without_allocating() {
        const GUARD: u32 = 0xA5A5_A5A5;
        let mut backing = vec![GUARD; 16 + (1 << 14) + 16];
        let base = backing[16..].as_mut_ptr() as usize as u32;
        for w in &mut backing[16..16 + (1 << 14)] {
            *w = 0;
        }
        let mut mem = RawMem { base, len: 1 << 16 };
        let mut st = ArenaState::new();
        let sizes = [676u32, 40, 7224, 100, 676, 3000, 16, 1024];
        let mut live: Vec<(u32, u32)> = Vec::with_capacity(sizes.len());
        let a0 = crate::test_alloc::count();
        for round in 0..200u32 {
            for &n in &sizes {
                let p = st.alloc_tagged(&mut mem, n, 0x0040_CB17).expect("入る");
                for off in (p..p + higanr_core::arena::class_bytes(
                    higanr_core::arena::class_of(n).expect("級がある"),
                ))
                    .step_by(4)
                {
                    mem.write(off, 0xDEAD_0000 | round);
                }
                live.push((p, n));
            }
            for (p, _) in live.drain(..) {
                st.free(&mut mem, p).expect("解放できる");
            }
        }
        let a1 = crate::test_alloc::count();
        assert_eq!(
            a1, a0,
            "ゲームのスレッドの経路（確保・解放）でヒープを確保した"
        );
        assert_eq!(st.live_blocks, 0);
        assert!(
            st.for_each_used(&mem, |_, _, _| panic!("使っている塊が残っている")),
            "歩けない"
        );
        let mut at = 0u32;
        while at < st.top {
            let c = mem.read(at + 4) as usize;
            let payload = higanr_core::arena::class_bytes(c);
            for off in (at + higanr_core::arena::HEADER..at + higanr_core::arena::HEADER + payload)
                .step_by(4)
            {
                assert_eq!(
                    mem.read(off),
                    0,
                    "空きの塊 {at:#x} の +{} が残っている",
                    off - at
                );
            }
            at += higanr_core::arena::HEADER + payload;
        }
        assert!(
            backing[..16].iter().all(|&w| w == GUARD),
            "領域の前を書いた"
        );
        assert!(
            backing[16 + (1 << 14)..].iter().all(|&w| w == GUARD),
            "領域の後を書いた"
        );
        mem.write(mem.len - 4, 0x1111);
        let unz = RB_UNZEROED.load(Ordering::Relaxed);
        mem.zero(mem.len - 8, 16);
        assert_eq!(mem.read(mem.len - 4), 0x1111, "領域を越える範囲を書いた");
        assert!(
            RB_UNZEROED.load(Ordering::Relaxed) > unz,
            "範囲外の zero を数えていない"
        );
        let tight = st.top;
        for w in &mut backing[16..] {
            *w = GUARD;
        }
        for w in &mut backing[16..16 + (tight / 4) as usize] {
            *w = 0;
        }
        let mut mem = RawMem { base, len: tight };
        let mut st = ArenaState::new();
        let ps: Vec<u32> = sizes
            .iter()
            .map(|&n| st.alloc_tagged(&mut mem, n, 1).expect("入る"))
            .collect();
        assert_eq!(st.top, tight, "前提: 同じ確保の列は同じ大きさになる");
        for p in ps {
            st.free(&mut mem, p).expect("解放できる");
        }
        assert!(
            backing[16 + (tight / 4) as usize..]
                .iter()
                .all(|&w| w == GUARD),
            "領域の終わりの先を書いた"
        );
    }
}
