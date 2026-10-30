//! Win32とゲームのメモリの層（unsafeはここと`rollback_win.rs`）。判断は`rollback_play.rs`（pure）。
//! 確保の入口のアセンブリ・DLLの領域・コピーの方法・走査のラッパーは`rollback_win.rs`と共用する。
//! 入口の関数は`arm`以外どれもゲームのスレッドから呼ぶ:
//! - `arm`: 読み取りのスレッドから、入力のフックを書いた後に1回呼ぶ。rollbackを望んだ実行でだけ書き込む（`rollback_play::install_wanted`）。
//! - [`with_play`]: コンテナ（`PlayState`と本物の`PlayMem`）を貸す。
//!   走査のラッパー（`0x0042DCA9`）のフックとPresentの後のフックは、この中で`NetPlay`のrollbackの経路（`netplay/rollback.rs`）を呼ぶ。
//!   ステートマシンは`Connected`の中にあり、外から`&mut`で借りられない。そのためpureな経路がコンテナを借りて進める。
//! - [`input_hook`]（入力の地点`0x0043112D`の入口から）: `Some(r)`なら入口へ`r`を返す（1 = feedした / 2 = 描画だけ）。`None`なら従来の経路。
//!   feedのフックはローカル入力を読まず、InputManagerの3本へ書き込む。自分のワードは走査のラッパーのフックが1フレームに1回だけ読む。
//! - [`resim_active`]: 進め直しの走査の中かどうか（`_ticks.csv`の記録・ペースの時刻の取得・台本の時計が見るフラグ）。
//!   `stub.rs`の入口のアセンブリも`RESIM_TICK`を直接見る。
//! - [`with_game_fpu`]: 進め直しの間のx87の制御ワード。
//! - カウントと食い違い: [`publish_summary`]（ゲームのスレッド）/ [`summary_text`]（読み取りのスレッド）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "rollback_play_win.rs は 32 ビット（i686）専用（32 ビットのアドレスを前提にしている）"
);

use core::sync::atomic::{AtomicU32, Ordering};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use higanr_core::patch::AddrRange;
use higanr_core::render_reset::{self, Field as ResetField, Refused};
use higanr_core::rollback::{Mode, Stats as CoreStats};
use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;

use crate::alloc_count::ALLOC_MAIN_TID;
use crate::detour::DetourEnv;
use crate::log_mode::detail_out;
use crate::menu::MemRead;
use crate::menu_win::OwnProcess;
use crate::rollback_play::{
    self, BODY_WORDS, Fault, NODE_WORDS, Order, OutCounts, PlayHooks, PlayMem, PlayState,
    PlayStats, SUP_FIELDS, ScanRun,
};
use crate::rollback_probe::{Layout, Region, Tables};
use crate::rollback_resim::{code_head_matches, output_addr_allowed};
use crate::rollback_win::{
    self, GameCell, RB_HELD_UNTRACKED, RB_HOLD_FREES, RB_OUT_ALLOCS, RB_OUT_FREES,
    RB_OUT_FREES_BORN, RB_OUT_FREES_HELD, RB_OUT_UNTRACKED, RB_PENDING, RB_REDIRECT,
    RB_STRAY_FREES, RB_WATCH,
};
use crate::th09_addr::{
    GAME_FPU_CW, IMAGE_DATA_END, IMAGE_DATA_START, RENDER_RESETS, SE_PUSH_FN, SE_PUSH_HEAD,
    SOUND_MANAGER, UPDATE_REGISTRY,
};

/// SEを積む`0x43E2F0`（thiscall: ecx = SoundManager・引数 (id, パン)・`ret $0x8`）。
type SeFn = unsafe extern "thiscall" fn(u32, u32, i32);

/// アドレス`f`のthiscallの関数を`(this, id, pan)`で呼ぶ（検査が偽の関数で引数の並びをチェックするためにアドレスを引数にした）。
///
/// # Safety
/// `f`はthiscallで ecx = this・スタックの引数2つ（`id`, `pan`）・`ret $0x8`の関数の入口であること。
/// 呼ぶ時点で`this`が指す物とゲームの状態は、その関数の想定どおりであること。
unsafe fn call_se_fn(f: u32, this: u32, id: u32, pan: i32) {
    // SAFETY: 呼び手が上の約束を守る。関数のアドレスと整数は32ビットで同じ大きさ。
    unsafe {
        let f = core::mem::transmute::<usize, SeFn>(f as usize);
        f(this, id, pan)
    }
}

/// SEを1つ積む（ゲームの`0x43E2F0`。進め直しの積み直しと選択画面での相手のカーソルの音）。
/// ゲームのスレッドのうちゲームの更新の外（入力の地点・Presentの後）からだけ呼ぶ。
/// 呼ぶ先の先頭25バイトを実バイトと照合し、合わなければ何も呼ばずにfalseを返す。
pub(crate) fn push_se_checked(id: u32, pan: i32) -> bool {
    // `OwnProcess`の例外を出さない読み取り。
    if !code_head_matches(SE_PUSH_FN, &SE_PUSH_HEAD, &mut |a| OwnProcess.u32_at(a)) {
        return false;
    }
    // SAFETY: `SE_PUSH_FN`はSEを積む`0x43E2F0`。先頭25バイトを上で実バイトと照合した。
    // thiscallで ecx = SoundManager（`SOUND_MANAGER`。th09.exeの .data）。
    // スタックの引数は (id, パン)の2つで、`ret $0x8`により関数自身が取り除く。
    // これはゲーム自身の呼び出し`0x00404081 mov $0x4dc698,%ecx` → `0x00404086 call 0x43e2f0`・`0x004247DA` → `0x004247DF`と同じ。
    // ebx・esi・edi・ebpはpush / popで戻す。eax・ecx・edxは壊してよい（thiscall）。
    // 触るのはSoundManagerの`+0x620`〜`+0x1E80`と`+0x408 + id × 4`だけ（枠が12を越えるなら何も書かずに戻る）。
    // 呼び手はゲームのスレッド。ゲームの更新は走っていない。
    unsafe { call_se_fn(SE_PUSH_FN, SOUND_MANAGER, id, pan) };
    true
}

/// 1なら設置が全部済んでいる。このとき入口の関数がコンテナに触ってよい。
static PLAY_ARMED: AtomicU32 = AtomicU32::new(0);

/// コンテナ（ゲームのスレッドだけが触る。`arm`が`PLAY_ARMED`を立てる前に1回だけ設定する）。
struct PlayBox {
    st: PlayState,
    snap_buf: u32,
    qpf: u64,
}
static PLAY: GameCell<PlayBox> = GameCell(core::cell::UnsafeCell::new(None));

// 走査の中の入力の地点への指示（`RealPlayMem::run_scan`が書き込み、`input_hook`が1回だけ取る）。
// どちらもゲームのスレッドで動く。アトミック変数にするのは`static`にするため。
/// 0 = 指示なし / 1 = feed / 2 = 描画だけ。
static ORDER: AtomicU32 = AtomicU32::new(0);
/// feedのワード（下位16ビットがp1・上位がp2）と`[2]`のワード（マッチの終わりの選択の出力）。
static ORDER_WORDS: AtomicU32 = AtomicU32::new(0);
static ORDER_WORD2: AtomicU32 = AtomicU32::new(0);
/// 指示を取ったかどうか。
static ORDER_TAKEN: AtomicU32 = AtomicU32::new(0);
/// 1なら進め直しの走査の中（`resim_active`）。`stub::input_wait_entry`のアセンブリが`cmp dword ptr [..], 0`で直接見る。
pub(crate) static RESIM_TICK: AtomicU32 = AtomicU32::new(0);

const ORDER_FEED: u32 = 1;
const ORDER_RENDER_ONLY: u32 = 2;

pub fn armed() -> bool {
    PLAY_ARMED.load(Ordering::Acquire) != 0
}

/// 進め直しの走査の中かどうか（`_ticks.csv`の記録・ペースの時刻の取得・台本の時計はこの間は積まない）。
pub fn resim_active() -> bool {
    RESIM_TICK.load(Ordering::Relaxed) != 0
}

fn play_box() -> Option<&'static mut PlayBox> {
    if !armed() || PANICKED.load(Ordering::Acquire) != 0 {
        return None;
    }
    // SAFETY: `PLAY_ARMED`をAcquireで1と読んだので`arm`が設定したコンテナが見える。
    // 触るのはゲームのスレッドだけ。入口の関数は入れ子にならない（`input_hook`はコンテナに触らない）。
    unsafe { (*PLAY.0.get()).as_mut() }
}

// ---- ゲームの側の本物 ----

/// .data（`IMAGE_DATA_START`〜`IMAGE_DATA_END`）かDLLの領域の中の4バイトなら直に読む。ほかはシステムコールで読む。
pub(crate) fn read_fast(addr: u32) -> Option<u32> {
    let in_data =
        addr >= IMAGE_DATA_START && addr.checked_add(4).is_some_and(|e| e <= IMAGE_DATA_END);
    let in_arena =
        rollback_win::in_arena(addr) && addr.checked_add(3).is_some_and(rollback_win::in_arena);
    if in_data || in_arena {
        // SAFETY: .dataはth09.exeのセクションで、起動から終了までコミット済みの読み書き可能なページ。
        // DLLの領域は`rollback_win::place_play_arena`がVirtualAllocで確保し、プロセスが終わるまで返さないページ。
        // どちらも4バイトが丸ごと中にあることを上でチェックした。
        // 読むのはゲームのスレッド。書き手も同じスレッド。
        return Some(unsafe { (addr as usize as *const u32).read_unaligned() });
    }
    OwnProcess.u32_at(addr)
}

struct RealPlayMem {
    snap_buf: u32,
    qpf: u64,
}

impl PlayMem for RealPlayMem {
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        read_fast(addr)
    }

    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        if !SUP_FIELDS.contains(&addr) && !output_addr_allowed(addr) {
            return false;
        }
        // SAFETY: 書き込むのはSupervisorの2つのフィールド（`0x4B368C` / `0x4B3690`）か`OUTPUT_RANGES`の中のワードだけ。
        // どれもth09.exeの .dataにあり、`arm`が`output_ranges_rw`で検証した。
        // ゲームのスレッドから呼ぶ。ゲームは動いていない（走査の外）。
        unsafe { (addr as usize as *mut u32).write_unaligned(v) };
        true
    }

    fn check_rw(&mut self, addr: u32, len: u32) -> bool {
        rollback_win::is_rw(addr, len)
    }

    fn arena_top(&mut self) -> u32 {
        rollback_win::arena_top()
    }

    fn copy_out(&mut self, l: &Layout) {
        rollback_win::save_regions(l, self.snap_buf);
    }

    fn copy_in(&mut self, l: &Layout) {
        rollback_win::restore_regions(l, self.snap_buf);
    }

    fn reset_in_snap(&mut self, r: &Region, fields: &[ResetField<'_>]) -> Result<u32, Refused> {
        let len = r.len.min(r.cap) as usize;
        // SAFETY: `[snap_buf + buf_off, + len)`はコピーのバッファの中にある。
        // バッファは`arm`が`buffer_bytes()`バイト以上をVirtualAllocで確保し、プロセスが終わるまで返さない。
        // 範囲の表では`buf_off + cap ≤ buffer_bytes`と`len ≤ cap`が成り立つ。
        // 触るのはゲームのスレッドだけで、このバッファへの参照はほかに無い。ゲームのメモリではなくコピーの中。
        let block = unsafe {
            core::slice::from_raw_parts_mut((self.snap_buf + r.buf_off) as usize as *mut u8, len)
        };
        render_reset::apply(block, fields)
    }

    fn list_signature(&mut self) -> u64 {
        rollback_play::list_signature_with(&mut read_fast)
    }

    fn list_chain(&mut self, out: &mut rollback_play::ListChain) -> u64 {
        rollback_play::list_chain_with(&mut read_fast, out)
    }

    fn run_scan(&mut self, order: Order, resim: bool) -> ScanRun {
        if !rollback_win::scan_hooked() {
            // 走査のcallを書き込んでいない。元の飛び先が無いので呼ばない。
            return ScanRun {
                ret: 1,
                consumed: false,
            };
        }
        let (code, words, w2) = match order {
            Order::Feed { p1, p2, w2 } => (ORDER_FEED, u32::from(p1) | u32::from(p2) << 16, w2),
            Order::RenderOnly => (ORDER_RENDER_ONLY, 0, 0),
        };
        ORDER_WORDS.store(words, Ordering::Relaxed);
        ORDER_WORD2.store(u32::from(w2), Ordering::Relaxed);
        ORDER_TAKEN.store(0, Ordering::Relaxed);
        ORDER.store(code, Ordering::Relaxed);
        RESIM_TICK.store(u32::from(resim), Ordering::Relaxed);
        let ret = rollback_win::call_scan(UPDATE_REGISTRY);
        ORDER.store(0, Ordering::Relaxed);
        RESIM_TICK.store(0, Ordering::Relaxed);
        ScanRun {
            ret,
            consumed: ORDER_TAKEN.load(Ordering::Relaxed) != 0,
        }
    }

    fn now_us(&mut self) -> u64 {
        if self.qpf == 0 {
            return 0;
        }
        (u128::from(rollback_win::qpc()) * 1_000_000 / u128::from(self.qpf)) as u64
    }

    fn set_redirect(&mut self, on: bool) {
        RB_REDIRECT.store(u32::from(on), Ordering::SeqCst);
        if !on {
            // マッチを動かさなくなった（場面の終わり・食い違い・ネットワークの終わり）。もう書き戻しは来ない。
            RB_PENDING.store(0, Ordering::SeqCst);
            rollback_win::clear_stray_bodies();
        }
    }

    fn set_watch(&mut self, on: bool) {
        RB_WATCH.store(u32::from(on), Ordering::SeqCst);
    }

    fn out_counts(&mut self) -> OutCounts {
        OutCounts {
            allocs: RB_OUT_ALLOCS.load(Ordering::Relaxed),
            frees_old: RB_OUT_FREES.load(Ordering::Relaxed),
            frees_born: RB_OUT_FREES_BORN.load(Ordering::Relaxed),
            frees_held: RB_OUT_FREES_HELD.load(Ordering::Relaxed),
        }
    }

    fn born_len(&mut self) -> u32 {
        rollback_win::out_born_len()
    }

    fn clear_born(&mut self) {
        rollback_win::clear_out_born();
        rollback_win::clear_held();
        RB_PENDING.store(0, Ordering::SeqCst);
    }

    fn set_hold_frees(&mut self, on: bool) {
        RB_HOLD_FREES.store(u32::from(on), Ordering::SeqCst);
    }

    fn set_pending(&mut self) {
        RB_PENDING.store(1, Ordering::SeqCst);
    }

    fn set_stray_refs(&mut self, frame: u32, nodes: &[u32], blocks: &[u32], bodies: &[u32]) {
        rollback_win::set_stray_refs(frame, nodes, blocks, bodies);
    }

    fn body_freed(&mut self, addr: u32) -> bool {
        rollback_win::body_freed(addr)
    }

    fn freed_held(&mut self, addr: u32) -> bool {
        rollback_win::freed_held(addr)
    }

    fn read_node(&mut self, addr: u32) -> Option<[u32; NODE_WORDS]> {
        let len = (NODE_WORDS * 4) as u32;
        if !rollback_win::is_rw(addr, len) {
            return None;
        }
        let mut w = [0u32; NODE_WORDS];
        // SAFETY: `[addr, addr + 0x20)`が読み書き可能なページの中にあることを上の`is_rw`（VirtualQuery）でチェックした。
        // ノードに書くのはゲームのスレッドだけ。ここもゲームのスレッド。書き込む先は自分の配列。
        unsafe {
            core::ptr::copy_nonoverlapping(addr as usize as *const u32, w.as_mut_ptr(), NODE_WORDS)
        };
        Some(w)
    }

    fn write_node(&mut self, addr: u32, w: &[u32; NODE_WORDS]) -> bool {
        let len = (NODE_WORDS * 4) as u32;
        if !rollback_win::is_rw(addr, len) {
            return false;
        }
        // SAFETY: 書き込むのは、コピーのときに2本の列をたどって記録したノード（コピーの範囲の外）の0x20 Bだけ。中身もコピーのときの値。
        // コピーから書き戻しまでに走る更新は確定していないtickだけ。
        // その間の前からあるブロックの解放はCRTへ渡していない（`RB_HOLD_FREES`）ので、ノードはまだ確保されたまま。
        // 監視の外の解放は、実機ではすべてBGMの通知`0x43F890`（呼び手`0x43F8D7` / `0x43F99C`）だった。
        // そのアドレスは記録したノードとは別だった（その経路を全部洗い出したわけではない）。
        // ページは上の`is_rw`でチェックした。ゲームのスレッドから呼ぶ。ゲームは動いていない。
        unsafe {
            core::ptr::copy_nonoverlapping(w.as_ptr(), addr as usize as *mut u32, NODE_WORDS)
        };
        true
    }

    fn push_se(&mut self, id: u32, pan: i32) -> bool {
        push_se_checked(id, pan)
    }

    fn end_show(&mut self, show: &higanr_core::end_pick::Show) {
        crate::end_outline_win::show(show);
    }

    fn end_note(&mut self, seg: u16, note: higanr_core::end_pick::Note, frame: u32) {
        crate::end_outline_win::note(seg, note, frame);
    }

    fn end_shadow(&mut self) -> bool {
        crate::end_outline_win::shadow()
    }

    fn read_strided(&mut self, base: u32, stride: u32, out: &mut [u32]) -> bool {
        let Some(n) = u32::try_from(out.len()).ok().filter(|&n| n != 0) else {
            return true;
        };
        // 読むワードがすべて`[base, base + span)`の中にある（最後のワードの終わりまで）。
        let Some(span) = (n - 1).checked_mul(stride).and_then(|d| d.checked_add(4)) else {
            return false;
        };
        if !rollback_win::is_rw(base, span) {
            return false;
        }
        for (i, o) in out.iter_mut().enumerate() {
            let a = base + i as u32 * stride;
            // SAFETY: `[base, base + span)`が読み書き可能なページの中にあることを上の`is_rw`でチェックした。`a + 4 ≤ base + span`（i < n）。
            // 読むだけ。書き手も同じスレッド（ゲームは動いていない）。
            *o = unsafe { (a as usize as *const u32).read_unaligned() };
        }
        true
    }

    fn read_body(&mut self, addr: u32) -> Option<[u32; BODY_WORDS]> {
        let len = (BODY_WORDS * 4) as u32;
        if !rollback_win::is_rw(addr, len) {
            return None;
        }
        let mut w = [0u32; BODY_WORDS];
        // SAFETY: `[addr, addr + 0x38)`が読み書き可能なページの中にあることを上の`is_rw`でチェックした。
        // 本体に書くのはゲームのスレッドだけ。ここもゲームのスレッド。書き込む先は自分の配列。
        unsafe {
            core::ptr::copy_nonoverlapping(addr as usize as *const u32, w.as_mut_ptr(), BODY_WORDS)
        };
        Some(w)
    }

    fn write_body(&mut self, addr: u32, w: &[u32; BODY_WORDS]) -> bool {
        let len = (BODY_WORDS * 4) as u32;
        if !rollback_win::is_rw(addr, len) {
            return false;
        }
        // SAFETY: 書き込むのは、コピーのときにScreenEffectのノードのthisとして記録した本体の0x38 Bだけ。
        // 記録したのは`0x422D20`（`push 0x38` → operator new。他に作る所は無い）が作った本体。
        // 戻りのフィールド（+0x08 / +0x0Cがノードを指す）でその本体だと検証した。大きさは決め打ち。
        // 呼び手（`GameWindow::restore`）は書く前に`body_freed`が偽であることをチェックしている。
        // これは、コピーの後に監視の外でCRTへ解放を渡していないという意味（書き戻し待ちの前の期間も`rb_body_free`がフラグを立てる）。
        // 予測のtickの解放はCRTへ渡さず保留する（`RB_HOLD_FREES`）ので、ブロックはまだ確保されたまま。
        // ページは上の`is_rw`でチェックした。ゲームのスレッドで呼ぶ。ゲームは動いていない。
        unsafe {
            core::ptr::copy_nonoverlapping(w.as_ptr(), addr as usize as *mut u32, BODY_WORDS)
        };
        true
    }
}

// ---- 入口の関数 ----

/// コンテナを貸して`f`を呼ぶ。設置していないときとパニックの後は`None`。`f`の中でパニックしたら、コンテナを閉じてから呼び手へ投げ直す。
/// コンテナを閉じるとは、確保の向け先・監視・指示を戻して以後の入口を開かないこと。
/// 投げ直したパニックは走査のラッパーとPresentのフックが受ける。
pub fn with_play<R>(f: impl FnOnce(&mut PlayState, &mut dyn PlayMem) -> R) -> Option<R> {
    let pb = play_box()?;
    let mut mem = RealPlayMem {
        snap_buf: pb.snap_buf,
        qpf: pb.qpf,
    };
    match catch_unwind(AssertUnwindSafe(|| f(&mut pb.st, &mut mem))) {
        Ok(r) => Some(r),
        Err(payload) => {
            panicked();
            std::panic::resume_unwind(payload)
        }
    }
}

/// 入力の地点の入口から呼ぶ（走査の中）。
/// 指示があれば取って、feedしたら1・描画だけなら2を返す。無ければ`None`。
/// `feed`はローカル入力を読まず、InputManagerの3本へ (p1, p2, `[2]`のワード)を書き込む関数。
/// 自分の入力ワードは走査のラッパーのフックが1フレームに1回だけ読む。
pub fn input_hook(feed: &mut dyn FnMut(u16, u16, u16)) -> Option<u32> {
    match ORDER.swap(0, Ordering::Relaxed) {
        ORDER_FEED => {
            let w = ORDER_WORDS.load(Ordering::Relaxed);
            let w2 = ORDER_WORD2.load(Ordering::Relaxed);
            ORDER_TAKEN.store(1, Ordering::Relaxed);
            feed(w as u16, (w >> 16) as u16, w2 as u16);
            Some(1)
        }
        ORDER_RENDER_ONLY => {
            ORDER_TAKEN.store(1, Ordering::Relaxed);
            Some(2)
        }
        _ => None,
    }
}

/// x87の制御ワードをゲームが前提にする値（`GAME_FPU_CW`）にして`f`を呼び、終わったら元へ戻す。
/// Presentの後はD3D8の呼び出しの後なので、制御ワードが更新の中と同じとは限らない。
/// 進め直しは更新を何度も再現するので、制御ワードが違うと両者の状態が食い違う。
/// 保留中の例外は無い。
pub fn with_game_fpu<R>(f: impl FnOnce() -> R) -> R {
    let mut old: u16 = 0;
    // SAFETY: `fnstcw`は制御ワードを2バイトの変数に書き込むだけ（待機も例外のチェックもしない）。書き込む先は自分のスタックの`old`。
    unsafe {
        core::arch::asm!("fnstcw word ptr [{}]", in(reg) &mut old as *mut u16, options(nostack))
    };
    let want: u16 = GAME_FPU_CW;
    // SAFETY: `fldcw`は2バイトの変数から制御ワードを読むだけ。
    // 値はゲームが元々使っている`0x007F`（24ビット精度・最近接・例外は全部マスク）。
    // x87のスタックは呼ぶ側で空になっている。
    unsafe {
        core::arch::asm!("fldcw word ptr [{}]", in(reg) &want as *const u16, options(nostack, readonly))
    };
    let r = f();
    // SAFETY: 上と同じ。読む元は`fnstcw`で取った元の値。
    unsafe {
        core::arch::asm!("fldcw word ptr [{}]", in(reg) &old as *const u16, options(nostack, readonly))
    };
    r
}

/// パニックを受けたときの後始末: 確保の向け先と監視と指示を戻し、以後は入口を開かない（`play_box`が`None`を返す）。
fn panicked() {
    RB_REDIRECT.store(0, Ordering::SeqCst);
    RB_WATCH.store(0, Ordering::SeqCst);
    RB_HOLD_FREES.store(0, Ordering::SeqCst);
    RB_PENDING.store(0, Ordering::SeqCst);
    ORDER.store(0, Ordering::SeqCst);
    RESIM_TICK.store(0, Ordering::SeqCst);
    PANICKED.store(1, Ordering::SeqCst);
}

static PANICKED: AtomicU32 = AtomicU32::new(0);

/// カウントと食い違いのコピー（ゲームのスレッドが`publish_summary`で書き込み、読み取りのスレッドが`summary_text`で読む）。
/// ゲームのスレッドは`try_lock`だけでロックを取る（待たない）。
static SUMMARY: Mutex<Option<Summary>> = Mutex::new(None);

/// `SUMMARY`の中身。`core`はrollbackのステートマシンのカウント。
#[derive(Clone, Copy)]
struct Summary {
    play: PlayStats,
    fault: Option<Fault>,
    list_diag: Option<rollback_play::ListDiag>,
    place_diag: Option<rollback_play::PlaceDiag>,
    changed_nodes: (
        [rollback_play::ChangedNode; rollback_play::CHANGED_KINDS],
        u32,
    ),
    core: Option<CoreStats>,
}

/// カウントと食い違いとrollbackのステートマシンのカウント`core`を、読み取りのスレッドが読み取り可能な場所へコピーする。
/// ゲームのスレッドから、数フレームに1回と場面の終わりと食い違いのときに呼ぶ。ロックが取れなければ何もしない。
/// `core`が`None`のときは前にコピーした値を残す。ここで確保しない。
pub fn publish_summary(core: Option<CoreStats>) {
    let Some(pb) = play_box() else {
        return;
    };
    if let Ok(mut g) = SUMMARY.try_lock() {
        let prev = g.and_then(|s| s.core);
        *g = Some(Summary {
            play: pb.st.stats(),
            fault: pb.st.fault(),
            list_diag: pb.st.list_diag(),
            place_diag: pb.st.place_diag(),
            changed_nodes: pb.st.changed_nodes(),
            core: core.or(prev),
        });
    }
}

/// 本ログの文（読み取りのスレッドから）。一度もコピーしていなければ`None`。パニックを受けた後は食い違いを`Panicked`にする。
pub fn summary_text() -> Option<String> {
    let s = SUMMARY.lock().ok().and_then(|g| *g)?;
    let fault = if PANICKED.load(Ordering::Acquire) != 0 {
        Some(Fault::Panicked)
    } else {
        s.fault
    };
    let mut lines = rollback_play::summary_lines(&s.play, fault);
    if let Some(d) = &s.list_diag {
        lines.push(format!(
            "巻き戻し（製品の処理フロー）: 並びの食い違いの所 {}",
            d.text()
        ));
    }
    if let Some(d) = &s.place_diag {
        // 範囲の名前は製品の経路の表（`PlayState::new`に渡したものと同じ`Tables::th09_play`）から取得する。
        lines.push(format!(
            "巻き戻し（製品の処理フロー）: アドレスが変わった範囲 {}",
            d.text(&Tables::th09_play())
        ));
    }
    let stray = rollback_win::stray_recs();
    lines.extend(rollback_play::hold_lines(
        &s.changed_nodes.0,
        s.changed_nodes.1,
        RB_STRAY_FREES.load(Ordering::Relaxed),
        RB_HELD_UNTRACKED.load(Ordering::Relaxed),
        RB_OUT_UNTRACKED.load(Ordering::Relaxed),
        &stray.0[..stray.1],
    ));
    if let Some(c) = &s.core {
        lines.extend(rollback_play::core_stats_lines(c));
    }
    Some(lines.join("\n"))
}

// ---- 設置 ----

struct RealHooks;

impl PlayHooks for RealHooks {
    fn entry(&self, site: usize) -> u32 {
        rollback_win::rb_entry_address(site).unwrap_or(0)
    }
    fn set_trampoline(&mut self, site: usize, addr: u32) {
        rollback_win::set_rb_trampoline(site, addr);
    }
    fn scan_entry(&self) -> u32 {
        rollback_win::scan_entry_address()
    }
    fn set_scan_original(&mut self, addr: u32) {
        rollback_win::set_scan_original(addr);
    }
}

/// 観戦者の実行で、早送りのために走査のcallだけを書き込む。
/// 確保の入口と領域は要らない（観戦者は巻き戻さない）。判断と書き方は`rollback_play::install_watch_scan`。
/// 呼ぶのは観戦者のネットワークを開くときの2か所（起動時の`RealScriptHost::arm_rollback`の観戦者の実行と、ゲーム内のメニューの観戦で「ルームマッチ」を開くスレッド）。**どちらも対戦の実行では呼ばない**。
/// 走査のcallが既に書かれていれば何もしない。書き込んだか既に書かれていればtrueを返す。
/// 早送りは文字数のフィールド（`ASCII_COUNT`）を直接読み書きする（`ff_read_ascii`）。
/// そのため、このフィールドが読み書き可能なページかどうかを検証する。
pub fn arm_watch_scan(
    hook_env: &mut dyn crate::hook::HookEnv,
    forbidden: &[AddrRange],
    out: &mut dyn Write,
) -> bool {
    if rollback_win::scan_hooked() {
        let _ = writeln!(
            detail_out(out),
            "観戦者の早送り: 走査の call は書き込み済み（rollback の設置などが先に書きました）なので何もしません"
        );
        return true;
    }
    if !rollback_win::output_ranges_rw() {
        let _ = writeln!(
            out,
            "観戦者の早送り: SE・BGM・文字の数の欄が読み書きできるページでないので走査の call を書きません（早送りは効かない）。"
        );
        return false;
    }
    let wrote =
        rollback_play::install_watch_scan(true, false, hook_env, &mut RealHooks, forbidden, out);
    if wrote {
        let _ = writeln!(
            detail_out(out),
            "観戦者の早送り: 走査の call だけを書きました（確保・解放の入口は書きません。マッチの方式に関わらず早送りが効きます）"
        );
    }
    wrote
}

/// DLLの領域の既定の大きさ（16 MiB。マッチの112秒まで足りた）。
pub const DEFAULT_ARENA_BYTES: u32 = 16 << 20;

/// 設置（読み取りのスレッドで1回。入力のフックを書いた後）。
/// `wish`はiniの望むモード（rollbackを望まなければ1バイトも書かない）。
/// 巻き戻しの実験の仕組み（`[rollback_probe]`）・`[alloc_count]`とは同じ入口を使う。そのため並べて有効にせず、呼び手がどちらか1つだけを有効にする。
/// 書き込めたらtrueを返す。
pub fn arm(
    wish: Mode,
    arena_bytes: u32,
    hook_env: &mut dyn DetourEnv,
    forbidden: &[AddrRange],
    main_tid: Option<u32>,
    out: &mut dyn Write,
) -> bool {
    if !rollback_play::install_wanted(wish) {
        let _ = writeln!(
            detail_out(out),
            "巻き戻し（製品の処理フロー）: rollback を望んでいない実行（delay 方式）なので何も書きません"
        );
        return false;
    }
    if armed() || rollback_win::armed() {
        let _ = writeln!(
            out,
            "巻き戻し（製品の処理フロー）: もう入っているか、E1 の実験の入口が入っているので入れません。"
        );
        return false;
    }
    let Some(tid) = main_tid else {
        let _ = writeln!(
            out,
            "巻き戻し（製品の処理フロー）: ゲームのスレッドの ID が取れないので入れません。"
        );
        return false;
    };
    if !rollback_win::output_ranges_rw() {
        let _ = writeln!(
            out,
            "巻き戻し（製品の処理フロー）: SE・BGM・文字の数の欄が読み書きできるページでないので入れません。"
        );
        return false;
    }
    let Some(base) = rollback_win::place_play_arena(arena_bytes) else {
        let _ = writeln!(
            out,
            "巻き戻し（製品の処理フロー）: 領域 {arena_bytes} バイトが確保できないので入れません。"
        );
        return false;
    };
    // 描画が消すフィールド: 描画の後の値を作る命令が想定のバイト列かどうかを、本物を読むだけで検証する。
    // 違えば表を使わない。巻き戻しは有効にする。色が残る不具合が戻るだけで、コピーと書き戻しの正しさには関わらない。
    let made = match rollback_play::render_reset_code_matches(RENDER_RESETS, &mut |a| {
        OwnProcess.u32_at(a)
    }) {
        Ok(()) => {
            let _ = writeln!(
                detail_out(out),
                "巻き戻し（製品の処理フロー）: 描画が消す欄 {} 行をコピーの中で描画後の値に置き換えます（描画の命令の照合が一致）",
                RENDER_RESETS.len()
            );
            PlayState::new(Tables::th09_play(), base, arena_bytes)
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "巻き戻し（製品の処理フロー）: 描画が消す欄を置き換えません（{e}）。カットインの色が残る不具合が出るおそれがあります。"
            );
            PlayState::new_with_resets(Tables::th09_play(), &[], base, arena_bytes)
        }
    };
    let st = match made {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(
                out,
                "巻き戻し（製品の処理フロー）: 範囲の表の作成不能（{e}）。入れません。"
            );
            return false;
        }
    };
    let buf_bytes = st.buffer_bytes();
    let Some(snap_buf) = rollback_win::alloc_pages(buf_bytes) else {
        let _ = writeln!(
            out,
            "巻き戻し（製品の処理フロー）: コピー用の領域 {buf_bytes} バイトが確保できないので入れません。"
        );
        return false;
    };
    let mut qpf: i64 = 0;
    // SAFETY: 書く先は自分の変数。
    unsafe { QueryPerformanceFrequency(&mut qpf) };
    let regions = st.plan().n;
    // SAFETY: 設定するのはここの1回だけ。まだ`PLAY_ARMED`を立てていないのでゲームのスレッドはコンテナに触らない。
    unsafe {
        *PLAY.0.get() = Some(PlayBox {
            st,
            snap_buf,
            qpf: qpf as u64,
        });
    }
    ALLOC_MAIN_TID.store(tid, Ordering::SeqCst);
    let _ = writeln!(
        detail_out(out),
        "巻き戻し（製品の処理フロー）: 領域 0x{base:08X}（{} MiB）、コピー用の領域 {buf_bytes} バイト × 1、範囲 {regions} 個（更新の列のつなぎ 0x40 B と陣の背景の VM の配列 2 本を含み、fps の計測の 6 語は書き戻しません）",
        arena_bytes >> 20
    );
    let r = rollback_play::install_play(wish, hook_env, &mut RealHooks, forbidden, out);
    if r.all_written() {
        PLAY_ARMED.store(1, Ordering::Release);
        let _ = writeln!(
            detail_out(out),
            "巻き戻し（製品の処理フロー）: 設置しました（確保・解放・calloc・走査の call）。確保の向け先は rollback のモードのマッチの間だけ開きます"
        );
        true
    } else {
        let _ = writeln!(
            out,
            "巻き戻し（製品の処理フロー）: 5 フック地点が全部は書けなかったので入れません（書けた入口は素通しのまま残る）。"
        );
        false
    }
}

#[cfg(test)]
mod tests {
    //! 指示の受け渡し（`input_hook`）とx87の制御ワードの再設定（`with_game_fpu`）。静的変数に触るので1本ずつ実行する。
    use super::*;

    static LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read_cw() -> u16 {
        let mut cw: u16 = 0;
        // SAFETY: `fnstcw`は制御ワードを2バイトの変数に書き込むだけ。書き込む先は自分のスタックの`cw`。
        unsafe {
            core::arch::asm!("fnstcw word ptr [{}]", in(reg) &mut cw as *mut u16, options(nostack))
        };
        cw
    }

    #[test]
    fn input_hook_takes_each_order_once() {
        let _g = lock();
        let mut fed = Vec::new();
        ORDER.store(0, Ordering::SeqCst);
        assert_eq!(input_hook(&mut |a, b, c| fed.push((a, b, c))), None);
        assert!(fed.is_empty());
        ORDER_WORDS.store(0x0022_0011, Ordering::SeqCst);
        ORDER_WORD2.store(0x0013, Ordering::SeqCst);
        ORDER_TAKEN.store(0, Ordering::SeqCst);
        ORDER.store(ORDER_FEED, Ordering::SeqCst);
        assert_eq!(input_hook(&mut |a, b, c| fed.push((a, b, c))), Some(1));
        assert_eq!(fed, vec![(0x0011, 0x0022, 0x0013)], "[2] の語が指図と違う");
        assert_eq!(ORDER_TAKEN.load(Ordering::SeqCst), 1);
        assert_eq!(
            input_hook(&mut |a, b, c| fed.push((a, b, c))),
            None,
            "指図を 2 回取れた"
        );
        ORDER_TAKEN.store(0, Ordering::SeqCst);
        ORDER.store(ORDER_RENDER_ONLY, Ordering::SeqCst);
        assert_eq!(input_hook(&mut |a, b, c| fed.push((a, b, c))), Some(2));
        assert_eq!(fed.len(), 1, "描画だけの指図で feed した");
        assert_eq!(ORDER_TAKEN.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_game_fpu_word_is_set_inside_and_restored_after() {
        let _g = lock();
        let before = read_cw();
        let inside = with_game_fpu(read_cw);
        assert_eq!(inside, GAME_FPU_CW);
        assert_eq!(read_cw(), before, "制御語を戻していない");
    }

    static SE_CALL: Mutex<Option<(u32, u32, i32)>> = Mutex::new(None);

    /// 偽のSEを積む関数（thiscall。ゲームの`0x43E2F0`と同じ呼ばれ方: ecx = this・スタックに (id, パン)・`ret $0x8`）。
    extern "thiscall" fn fake_se_push(this: u32, id: u32, pan: i32) {
        *SE_CALL.lock().unwrap_or_else(|e| e.into_inner()) = Some((this, id, pan));
    }

    #[test]
    fn the_se_push_call_passes_this_then_id_then_pan() {
        let _g = lock();
        *SE_CALL.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // SAFETY: `fake_se_push`はthiscallで引数3つ（this・id・パン）を取るRustの関数。呼ぶ約束（上の`call_se_fn`の注）を満たす。
        unsafe {
            call_se_fn(
                fake_se_push as *const () as usize as u32,
                0x004D_C698,
                0x35,
                -1234,
            )
        };
        assert_eq!(
            *SE_CALL.lock().unwrap_or_else(|e| e.into_inner()),
            Some((0x004D_C698, 0x35, -1234))
        );
    }

    #[test]
    fn a_se_push_is_refused_when_the_code_is_not_the_games() {
        let _g = lock();
        let mut m = RealPlayMem {
            snap_buf: 0,
            qpf: 0,
        };
        assert!(!m.push_se(0x0E, 0), "ゲームでないプロセスで積んだ");
    }

    #[test]
    fn nothing_is_lent_before_arming() {
        let _g = lock();
        assert!(!armed());
        assert!(with_play(|_, _| ()).is_none());
    }
}
