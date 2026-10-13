//! 撮影の D3D8 の層（unsafe はここにもある。1 つずつ `// SAFETY:`）: 描画の入口（`draw.rs` の `draw_entry`）が撮影の要求を見て呼ぶ
//! `shot_on_draw`、要求の置き場、コピーの置き場 4 枚、読み取りのスレッドの受け取りの入口。
//!
//! 読み方: デバイス `[0x4B3108]` の GetBackBuffer（vtable `+0x40`。引数 0, 0）→ 面の GetDesc（`+0x20`）→ LockRect（`+0x24`。全面、フラグ 0）→
//! 行ごとにコピーする → UnlockRect（`+0x28`）→ Release（`+0x08`）。ゲーム自身の撮影 `0x0042F540` と同じ関数。コピーしてよいかの判断
//! （幅・高さ・形式・置き場に収まるか）は `shots::plan_copy`（pure）にだけ書き、ここは呼ぶ順とコピーだけを持つ。
//!
//! ゲームのスレッドの経路（`shot_on_draw` と `place_request`）では、確保・ファイル・ロック待ちをしない。中は全部 `catch_unwind` で、
//! パニックを受けたら撮影を止める。置き場の受け渡しは状態の原子変数（空き → 書いている → できた → 空き）で、各遷移の書き手は 1 人だけ
//! （空き → 書いている → できた はゲームのスレッド、できた → 空き は読み取りのスレッド）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "shot_win.rs は 32 ビット（i686）専用（D3D8 の vtable と 32 ビットのアドレスを前提にしている）"
);

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::shots::{
    C_GETBACKBUFFER, C_GETDESC, C_LOCKRECT, C_RELEASE, C_UNLOCKRECT, Fail, H_BYTES, H_CALLS,
    H_DESC, H_DEVICE, H_DRAW_SEQ, H_FAIL, H_FORMAT, H_HEIGHT, H_HR_GETBACKBUFFER, H_HR_GETDESC,
    H_HR_LOCKRECT, H_HR_UNLOCKRECT, H_K, H_MERGED, H_PITCH, H_PP, H_REASON, H_RELEASE, H_SERIAL,
    H_STEP, H_TSC_COPY, H_TSC_TOTAL, H_WIDTH, HEAD_WORDS, SHOT_CAP, SHOT_SLOTS, ShotCounters,
    ShotHead, plan_copy,
};
use crate::th09_addr::{
    D3D8_GETBACKBUFFER_SLOT, D3D8_SURF_GETDESC_SLOT, D3D8_SURF_LOCKRECT_SLOT,
    D3D8_SURF_RELEASE_SLOT, D3D8_SURF_UNLOCKRECT_SLOT, PP_FORMAT, PP_HEIGHT, PP_WIDTH,
};
use crate::win::read_own_memory;

/// 撮影の要求（理由のビット。0 なら要求なし）。描画の入口のアセンブリが `cmp dword ptr [SHOT_REQ], 0` で見て、0 でなければ
/// `shot_on_draw` を呼ぶ。書くのも取り出すのもゲームのスレッドだけ。
pub static SHOT_REQ: AtomicU32 = AtomicU32::new(0);
/// 要求を置いた tick の番号・段階・まとめた回数（ゲームのスレッドだけが触る）。
static REQ_K: AtomicU32 = AtomicU32::new(0);
static REQ_STEP: AtomicU32 = AtomicU32::new(0);
static REQ_MERGED: AtomicU32 = AtomicU32::new(0);

/// 撮影が有効か。読み取りのスレッドが置き場を確保し、描画の detour が書けた後で 1 にする。ゲームのスレッドがパニックを受けたら 0 にする。
pub static SHOT_ON: AtomicU32 = AtomicU32::new(0);
/// デバイスへのポインタの置き場（花映塚では `D3D8_DEVICE_PTR`）と present parameters のコピー（`D3D8_PRESENT_PARAMS`）。`prepare` が読めると確かめてから置く。
static DEVICE_PTR_ADDR: AtomicU32 = AtomicU32::new(0);
static PP_ADDR: AtomicU32 = AtomicU32::new(0);

const FREE: u32 = 0;
const BUSY: u32 = 1;
const DONE: u32 = 2;

/// 置き場の画素（`SHOT_CAP` バイトずつ。`prepare` が確保し、プロセスが終わるまで返さない）。
static BUFS: [AtomicUsize; SHOT_SLOTS] = [const { AtomicUsize::new(0) }; SHOT_SLOTS];
static STATES: [AtomicU32; SHOT_SLOTS] = [const { AtomicU32::new(FREE) }; SHOT_SLOTS];
struct Head([AtomicU32; HEAD_WORDS]);
static HEADS: [Head; SHOT_SLOTS] =
    [const { Head([const { AtomicU32::new(0) }; HEAD_WORDS]) }; SHOT_SLOTS];
static SERIAL: AtomicU32 = AtomicU32::new(0);

static PLACED: AtomicU32 = AtomicU32::new(0);
static MERGED: AtomicU32 = AtomicU32::new(0);
static DROPPED_OFF: AtomicU32 = AtomicU32::new(0);
static TAKEN: AtomicU32 = AtomicU32::new(0);
static NO_FREE: AtomicU32 = AtomicU32::new(0);
static PANICKED: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
pub(crate) static PANIC_NEXT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 読み取りのスレッドが、設置の前に置き場 4 枚を確保しアドレスを置く。撮影はまだ有効にしない（`set_on`）。アドレスが読めなければ断る。
pub fn prepare(device_ptr_addr: u32, pp_addr: u32) -> Result<(), String> {
    if SHOT_ON.load(Ordering::SeqCst) != 0 {
        return Err("撮影がもう有効になっている".to_string());
    }
    if device_ptr_addr == 0 || read_own_memory(device_ptr_addr, 4).is_none() {
        return Err(format!(
            "デバイスの置き場 0x{device_ptr_addr:08X} が読めない"
        ));
    }
    if pp_addr == 0 || read_own_memory(pp_addr, (PP_FORMAT + 4) as usize).is_none() {
        return Err(format!("present parameters 0x{pp_addr:08X} が読めない"));
    }
    DEVICE_PTR_ADDR.store(device_ptr_addr, Ordering::SeqCst);
    PP_ADDR.store(pp_addr, Ordering::SeqCst);
    for (b, s) in BUFS.iter().zip(STATES.iter()) {
        if b.load(Ordering::SeqCst) == 0 {
            // 1 枚 1.2 MB。ゲームのスレッドがいつ書くか分からないので、プロセスが終わるまで持つ。
            let buf: &'static mut [u8] = Box::leak(vec![0u8; SHOT_CAP].into_boxed_slice());
            b.store(buf.as_mut_ptr() as usize, Ordering::SeqCst);
        }
        s.store(FREE, Ordering::SeqCst);
    }
    SHOT_REQ.store(0, Ordering::SeqCst);
    REQ_MERGED.store(0, Ordering::SeqCst);
    Ok(())
}

pub fn set_on() {
    SHOT_ON.store(1, Ordering::Release);
}

pub fn set_off() {
    SHOT_ON.store(0, Ordering::SeqCst);
    SHOT_REQ.store(0, Ordering::SeqCst);
}

/// 撮影の要求を置く（ゲームのスレッドの台本の入口から。確保しない）。同じ描画の前に 2 回置かれたら理由を OR し、tick の番号と段階は後のものにし、
/// まとめた回数を数える。撮影が無効なら捨てて数える。
pub fn place_request(reason: u8, step: u16, k: u32) {
    if reason == 0 {
        return;
    }
    if SHOT_ON.load(Ordering::Acquire) == 0 {
        DROPPED_OFF.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let prev = SHOT_REQ.load(Ordering::Relaxed);
    if prev != 0 {
        MERGED.fetch_add(1, Ordering::Relaxed);
        REQ_MERGED.fetch_add(1, Ordering::Relaxed);
    }
    REQ_K.store(k, Ordering::Relaxed);
    REQ_STEP.store(step as u32, Ordering::Relaxed);
    SHOT_REQ.store(prev | reason as u32, Ordering::Relaxed);
    PLACED.fetch_add(1, Ordering::Relaxed);
}

pub fn counters() -> ShotCounters {
    let l = |a: &AtomicU32| a.load(Ordering::Acquire);
    ShotCounters {
        placed: l(&PLACED),
        merged: l(&MERGED),
        dropped_off: l(&DROPPED_OFF),
        taken: l(&TAKEN),
        no_free: l(&NO_FREE),
        panicked: l(&PANICKED),
    }
}

fn rdtsc() -> u64 {
    // SAFETY: `rdtsc` は読むだけの命令で、ユーザーモードで許されている。
    unsafe { core::arch::x86::_rdtsc() }
}

/// 描画の入口（`draw.rs`）が、撮影の要求が 0 でないときに呼ぶ（全部のレジスタとフラグは入口が保つ）。パニックは止めて数える。
pub extern "C" fn shot_on_draw() {
    if catch_unwind(AssertUnwindSafe(shot_inner)).is_err() {
        set_off();
        PANICKED.fetch_add(1, Ordering::SeqCst);
    }
}

fn claim_free() -> Option<usize> {
    STATES.iter().position(|s| {
        s.compare_exchange(FREE, BUSY, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    })
}

fn shot_inner() {
    let reason = SHOT_REQ.swap(0, Ordering::Relaxed);
    if reason == 0 {
        return;
    }
    if SHOT_ON.load(Ordering::Acquire) == 0 {
        return;
    }
    #[cfg(test)]
    if PANIC_NEXT.swap(false, Ordering::SeqCst) {
        panic!("検査: 撮影の中のパニック");
    }
    TAKEN.fetch_add(1, Ordering::Relaxed);
    let merged = REQ_MERGED.swap(0, Ordering::Relaxed);
    let Some(i) = claim_free() else {
        NO_FREE.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let head = &HEADS[i].0;
    for w in head.iter() {
        w.store(0, Ordering::Relaxed);
    }
    let set = |idx: usize, v: u32| head[idx].store(v, Ordering::Relaxed);
    set(
        H_SERIAL,
        SERIAL.fetch_add(1, Ordering::Relaxed).wrapping_add(1),
    );
    set(H_REASON, reason);
    set(H_STEP, REQ_STEP.load(Ordering::Relaxed));
    set(H_K, REQ_K.load(Ordering::Relaxed));
    set(H_DRAW_SEQ, crate::stub::SEQ.load(Ordering::Relaxed));
    set(H_MERGED, merged);
    let t0 = rdtsc();
    let mut calls = 0u32;
    let fail = capture(i, head, &mut calls);
    set(H_CALLS, calls);
    set(
        H_TSC_TOTAL,
        u32::try_from(rdtsc().wrapping_sub(t0)).unwrap_or(u32::MAX),
    );
    set(H_FAIL, fail.code());
    STATES[i].store(DONE, Ordering::Release);
}

// D3D8 の関数の形（stdcall = `extern "system"`、第 1 引数が this。32 ビットなのでアドレスは u32）。
type GetBackBufferFn = unsafe extern "system" fn(u32, u32, u32, *mut u32) -> i32;
type GetDescFn = unsafe extern "system" fn(u32, *mut u32) -> i32;
type LockRectFn = unsafe extern "system" fn(u32, *mut u32, *const c_void, u32) -> i32;
type UnlockRectFn = unsafe extern "system" fn(u32) -> i32;
type ReleaseFn = unsafe extern "system" fn(u32) -> u32;

/// `addr` の 4 バイト。
///
/// # Safety
/// `addr` から 4 バイトが読めること。
unsafe fn read_u32(addr: u32) -> u32 {
    // SAFETY: 呼び出し側の約束。境界を問わない読み（`read_unaligned`）。
    unsafe { (addr as usize as *const u32).read_unaligned() }
}

/// COM の物 `obj` の vtable の `slot` バイト目の関数のアドレス。
///
/// # Safety
/// `obj` が生きている COM の物（先頭の 4 バイトが vtable へのアドレス）であること。
unsafe fn vtable_fn(obj: u32, slot: u32) -> usize {
    // SAFETY: 呼び出し側の約束（先頭が vtable）。`slot` は D3D8 の決まった並びの中。
    unsafe { read_u32(read_u32(obj).wrapping_add(slot)) as usize }
}

/// デバイスから面を取り、コピー、面を返す。GetBackBuffer が通ったら、どの経路でも Release は 1 回だけ。
fn capture(i: usize, head: &[AtomicU32; HEAD_WORDS], calls: &mut u32) -> Fail {
    let set = |idx: usize, v: u32| head[idx].store(v, Ordering::Relaxed);
    let (dev_at, pp_at) = (
        DEVICE_PTR_ADDR.load(Ordering::Relaxed),
        PP_ADDR.load(Ordering::Relaxed),
    );
    if dev_at == 0 || pp_at == 0 {
        return Fail::NoDevice;
    }
    // SAFETY: どちらも `prepare` が読めると確かめてから置いたアドレス（th09.exe の .data の `0x4B3108` と `0x4B3450`。プロセスが終わるまで読める）。
    let (device, pp) = unsafe {
        (
            read_u32(dev_at),
            [
                read_u32(pp_at + PP_WIDTH),
                read_u32(pp_at + PP_HEIGHT),
                read_u32(pp_at + PP_FORMAT),
            ],
        )
    };
    set(H_DEVICE, device);
    for (j, v) in pp.iter().enumerate() {
        set(H_PP + j, *v);
    }
    if device == 0 {
        return Fail::NoDevice;
    }
    let mut surf: u32 = 0;
    // SAFETY: `device` はゲームが作った IDirect3DDevice8（`[0x4B3108]` が 0 でない。ゲームも描画の直前 `0x0042DD51` で同じアドレスを読む）。
    // vtable の `+0x40` は GetBackBuffer。引数はゲームの撮影と同じ 0, 0 と書ける `surf`。ゲームのスレッドから呼ぶ。
    let hr = unsafe {
        let f: GetBackBufferFn = core::mem::transmute::<usize, GetBackBufferFn>(vtable_fn(
            device,
            D3D8_GETBACKBUFFER_SLOT,
        ));
        f(device, 0, 0, &mut surf)
    };
    *calls |= C_GETBACKBUFFER;
    set(H_HR_GETBACKBUFFER, hr as u32);
    if hr < 0 {
        return Fail::GetBackBuffer;
    }
    if surf == 0 {
        return Fail::NullSurface;
    }
    let fail = with_surface(i, surf, &pp, head, calls);
    // SAFETY: `surf` は GetBackBuffer が参照を 1 つ足して返した面。ここで 1 回だけ返す（`+0x08` は IUnknown の Release）。
    let refs = unsafe {
        let f: ReleaseFn =
            core::mem::transmute::<usize, ReleaseFn>(vtable_fn(surf, D3D8_SURF_RELEASE_SLOT));
        f(surf)
    };
    *calls |= C_RELEASE;
    set(H_RELEASE, refs);
    fail
}

/// 面の GetDesc → 判断 → LockRect → コピー → UnlockRect。UnlockRect は LockRect が通ったときだけ。
fn with_surface(
    i: usize,
    surf: u32,
    pp: &[u32; 3],
    head: &[AtomicU32; HEAD_WORDS],
    calls: &mut u32,
) -> Fail {
    let set = |idx: usize, v: u32| head[idx].store(v, Ordering::Relaxed);
    let mut desc = [0u32; 8];
    // SAFETY: `surf` は GetBackBuffer が返した生きている面。`+0x20` は GetDesc で、書く先は D3DSURFACE_DESC と同じ 8 語のスタックの配列。
    let hr = unsafe {
        let f: GetDescFn =
            core::mem::transmute::<usize, GetDescFn>(vtable_fn(surf, D3D8_SURF_GETDESC_SLOT));
        f(surf, desc.as_mut_ptr())
    };
    *calls |= C_GETDESC;
    set(H_HR_GETDESC, hr as u32);
    for (j, v) in desc.iter().enumerate() {
        set(H_DESC + j, *v);
    }
    if hr < 0 {
        return Fail::GetDesc;
    }
    let plan = match plan_copy(&desc, pp, SHOT_CAP) {
        Ok(p) => p,
        Err(f) => return f,
    };
    let buf = BUFS[i].load(Ordering::Acquire);
    if buf == 0 {
        return Fail::NoBuffer;
    }
    let mut locked = [0u32; 2];
    // SAFETY: 同じ面。`+0x24` は LockRect。書く先は D3DLOCKED_RECT（Pitch・pBits の 2 語）で、矩形は NULL（全面）、フラグ 0。
    let hr = unsafe {
        let f: LockRectFn =
            core::mem::transmute::<usize, LockRectFn>(vtable_fn(surf, D3D8_SURF_LOCKRECT_SLOT));
        f(surf, locked.as_mut_ptr(), core::ptr::null(), 0)
    };
    *calls |= C_LOCKRECT;
    set(H_HR_LOCKRECT, hr as u32);
    if hr < 0 {
        return Fail::LockRect;
    }
    let (pitch, bits) = (locked[0] as i32, locked[1]);
    set(H_PITCH, pitch as u32);
    let fail = if bits == 0 || pitch < plan.row as i32 {
        Fail::BadLock
    } else {
        let t = rdtsc();
        for y in 0..plan.height as usize {
            // SAFETY: 元は LockRect が返した画素で、行 y は `bits + y * pitch` から `row` バイト（pitch >= row を確かめ済み。y < Height）。
            // 先は `prepare` が確保した置き場で、`y * row + row <= total <= SHOT_CAP`（`plan_copy` が確かめた）。「書いている」の間は読み取りのスレッドが触らない。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (bits as usize).wrapping_add(y * pitch as usize) as *const u8,
                    buf.wrapping_add(y * plan.row as usize) as *mut u8,
                    plan.row as usize,
                );
            }
        }
        set(
            H_TSC_COPY,
            u32::try_from(rdtsc().wrapping_sub(t)).unwrap_or(u32::MAX),
        );
        set(H_WIDTH, plan.width);
        set(H_HEIGHT, plan.height);
        set(H_FORMAT, plan.format);
        set(H_BYTES, plan.total);
        Fail::Ok
    };
    // SAFETY: 同じ面。`+0x28` は UnlockRect。LockRect が通ったので対にして 1 回だけ呼ぶ。
    let hr = unsafe {
        let f: UnlockRectFn =
            core::mem::transmute::<usize, UnlockRectFn>(vtable_fn(surf, D3D8_SURF_UNLOCKRECT_SLOT));
        f(surf)
    };
    *calls |= C_UNLOCKRECT;
    set(H_HR_UNLOCKRECT, hr as u32);
    fail
}

/// 「できた」置き場のうち、撮った順の番号がいちばん小さいものの番号と先頭（無ければ `None`）。
pub fn take_done() -> Option<(usize, ShotHead)> {
    (0..SHOT_SLOTS)
        .filter(|&i| STATES[i].load(Ordering::Acquire) == DONE)
        .map(|i| {
            let h: [u32; HEAD_WORDS] =
                core::array::from_fn(|j| HEADS[i].0[j].load(Ordering::Relaxed));
            (i, ShotHead(h))
        })
        .min_by_key(|(_, h)| h.0[H_SERIAL])
}

/// 「できた」置き場 `slot` の画素の先頭 `len` バイトをコピーして返す（読み取りのスレッド）。置き場が無い・範囲の外なら空。
pub fn copy_pixels(slot: usize, len: usize) -> Vec<u8> {
    let Some(b) = BUFS.get(slot) else {
        return Vec::new();
    };
    let p = b.load(Ordering::Acquire);
    if p == 0 || STATES[slot].load(Ordering::Acquire) != DONE {
        return Vec::new();
    }
    let len = len.min(SHOT_CAP);
    // SAFETY: `p` は `prepare` が確保して返さない `SHOT_CAP` バイトの置き場で `len <= SHOT_CAP`。状態が「できた」の間はゲームのスレッドは書かず、
    // Acquire の読みで、Release で書かれた画素が見える。
    unsafe { core::slice::from_raw_parts(p as *const u8, len) }.to_vec()
}

pub fn release(slot: usize) {
    if let Some(s) = STATES.get(slot) {
        let _ = s.compare_exchange(DONE, FREE, Ordering::Release, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! 偽の COM（vtable を持つ静的な偽のデバイスと面）で、本物の描画の入口から撮影の経路を通す。
    use super::*;
    use crate::draw::{draw_entry_address, draw_next_seq, set_draw_trampoline};
    use crate::shots::{H_TSC_COPY, H_TSC_TOTAL};
    use crate::stub::testkit::{
        AFTER, EFLAGS_ARITH, PRE_ESP, PRE_FLAGS, REAL_HOOK_TESTS, REG_EAX, REG_EBP, REG_EBX,
        REG_ECX, REG_EDI, REG_EDX, SEEN, TARGET_HITS, call_with_regs, fake_this_addr, load,
        seen_target_addr,
    };
    use core::sync::atomic::AtomicBool;
    use higanr_core::shot::D3DFMT_X8R8G8B8;

    #[repr(C)]
    pub(crate) struct Obj(AtomicU32);
    pub(crate) static FAKE_DEVICE: Obj = Obj(AtomicU32::new(0));
    static FAKE_SURF: Obj = Obj(AtomicU32::new(0));
    static DEV_VTBL: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
    static SURF_VTBL: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
    /// `[0x4B3108]` の代わり（中身は偽のデバイスのアドレスか 0）。
    pub(crate) static FAKE_DEVICE_PTR: AtomicU32 = AtomicU32::new(0);
    pub(crate) static FAKE_PP: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];

    /// 合成の絵: 4×2 の X8R8G8B8、pitch 24 バイト（1 行 16 バイト ＋ 詰め物 8 バイト）。詰め物は 0xEE。
    const W: u32 = 4;
    const H: u32 = 2;
    const PITCH: u32 = 24;
    static FAKE_BITS: [AtomicU32; (PITCH * H / 4) as usize] =
        [const { AtomicU32::new(0) }; (PITCH * H / 4) as usize];

    static GBB_HR: AtomicU32 = AtomicU32::new(0);
    static GBB_NULL: AtomicBool = AtomicBool::new(false);
    static DESC_W: AtomicU32 = AtomicU32::new(W);
    static DESC_FMT: AtomicU32 = AtomicU32::new(D3DFMT_X8R8G8B8);
    static LOCK_HR: AtomicU32 = AtomicU32::new(0);
    static N_GBB: AtomicU32 = AtomicU32::new(0);
    static N_GBB_OK: AtomicU32 = AtomicU32::new(0);
    static N_DESC: AtomicU32 = AtomicU32::new(0);
    static N_LOCK: AtomicU32 = AtomicU32::new(0);
    static N_UNLOCK: AtomicU32 = AtomicU32::new(0);
    static N_RELEASE: AtomicU32 = AtomicU32::new(0);
    static BAD_ARGS: AtomicU32 = AtomicU32::new(0);

    fn addr_of<T>(x: &'static T) -> u32 {
        x as *const T as usize as u32
    }

    const E_FAIL: u32 = 0x8000_4005;

    extern "system" fn fake_gbb(this: u32, index: u32, ty: u32, out: *mut u32) -> i32 {
        N_GBB.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&FAKE_DEVICE) || index != 0 || ty != 0 || out.is_null() {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        let hr = GBB_HR.load(Ordering::SeqCst) as i32;
        if hr < 0 {
            return hr;
        }
        N_GBB_OK.fetch_add(1, Ordering::SeqCst);
        let s = if GBB_NULL.load(Ordering::SeqCst) {
            0
        } else {
            addr_of(&FAKE_SURF)
        };
        // SAFETY: `out` は `capture` のスタックの `surf`（書ける 4 バイト）。
        unsafe { out.write(s) };
        0
    }
    extern "system" fn fake_desc(this: u32, desc: *mut u32) -> i32 {
        N_DESC.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&FAKE_SURF) {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        let d = [
            DESC_FMT.load(Ordering::SeqCst),
            1,         // D3DRTYPE_SURFACE
            1,         // D3DUSAGE_RENDERTARGET
            0,         // D3DPOOL_DEFAULT
            W * H * 4, // Size
            0,         // D3DMULTISAMPLE_NONE
            DESC_W.load(Ordering::SeqCst),
            H,
        ];
        for (j, v) in d.iter().enumerate() {
            // SAFETY: `desc` は `with_surface` のスタックの 8 語の配列。
            unsafe { desc.add(j).write(*v) };
        }
        0
    }
    extern "system" fn fake_lock(
        this: u32,
        locked: *mut u32,
        rect: *const c_void,
        flags: u32,
    ) -> i32 {
        N_LOCK.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&FAKE_SURF) || !rect.is_null() || flags != 0 {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        let hr = LOCK_HR.load(Ordering::SeqCst) as i32;
        if hr < 0 {
            return hr;
        }
        // SAFETY: `locked` は `with_surface` のスタックの 2 語の配列。
        unsafe {
            locked.write(PITCH);
            locked.add(1).write(addr_of(&FAKE_BITS));
        }
        0
    }
    extern "system" fn fake_unlock(this: u32) -> i32 {
        N_UNLOCK.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&FAKE_SURF) {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        0
    }
    extern "system" fn fake_release(this: u32) -> u32 {
        if this != addr_of(&FAKE_SURF) {
            BAD_ARGS.fetch_add(1, Ordering::SeqCst);
        }
        N_RELEASE.fetch_add(1, Ordering::SeqCst)
    }

    fn px(x: u32, y: u32) -> [u8; 4] {
        [
            (10 + x + 40 * y) as u8,
            (100 + x) as u8,
            (200 + y) as u8,
            0x5A,
        ]
    }

    pub(crate) fn setup() {
        let dev_vt = [(D3D8_GETBACKBUFFER_SLOT, fake_gbb as *const () as usize)];
        for w in DEV_VTBL.iter() {
            w.store(0, Ordering::SeqCst);
        }
        for (slot, f) in dev_vt {
            DEV_VTBL[(slot / 4) as usize].store(f as u32, Ordering::SeqCst);
        }
        for (slot, f) in [
            (D3D8_SURF_RELEASE_SLOT, fake_release as *const () as usize),
            (D3D8_SURF_GETDESC_SLOT, fake_desc as *const () as usize),
            (D3D8_SURF_LOCKRECT_SLOT, fake_lock as *const () as usize),
            (D3D8_SURF_UNLOCKRECT_SLOT, fake_unlock as *const () as usize),
        ] {
            SURF_VTBL[(slot / 4) as usize].store(f as u32, Ordering::SeqCst);
        }
        FAKE_DEVICE.0.store(addr_of(&DEV_VTBL), Ordering::SeqCst);
        FAKE_SURF.0.store(addr_of(&SURF_VTBL), Ordering::SeqCst);
        FAKE_DEVICE_PTR.store(addr_of(&FAKE_DEVICE), Ordering::SeqCst);
        for (a, v) in FAKE_PP.iter().zip([W, H, D3DFMT_X8R8G8B8]) {
            a.store(v, Ordering::SeqCst);
        }
        for (i, w) in FAKE_BITS.iter().enumerate() {
            let off = i as u32 * 4;
            let (y, x) = (off / PITCH, (off % PITCH) / 4);
            let v = if x < W {
                u32::from_le_bytes(px(x, y))
            } else {
                0xEEEE_EEEE
            };
            w.store(v, Ordering::SeqCst);
        }
        GBB_HR.store(0, Ordering::SeqCst);
        GBB_NULL.store(false, Ordering::SeqCst);
        DESC_W.store(W, Ordering::SeqCst);
        DESC_FMT.store(D3DFMT_X8R8G8B8, Ordering::SeqCst);
        LOCK_HR.store(0, Ordering::SeqCst);
        set_off();
        prepare(addr_of(&FAKE_DEVICE_PTR), addr_of(&FAKE_PP)).expect("prepare");
        set_on();
    }

    pub(crate) fn teardown() {
        set_off();
        while let Some((i, _)) = take_done() {
            release(i);
        }
        set_draw_trampoline(0);
    }

    fn calls_now() -> [u32; 6] {
        [&N_GBB, &N_GBB_OK, &N_DESC, &N_LOCK, &N_UNLOCK, &N_RELEASE]
            .map(|a| a.load(Ordering::SeqCst))
    }

    fn draw_once() {
        set_draw_trampoline(seen_target_addr());
        call_with_regs(draw_entry_address());
    }

    #[test]
    fn draw_entry_shoots_the_fake_backbuffer_and_preserves_state() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        setup();
        crate::stub::SEQ.store(0x0000_7777, Ordering::SeqCst);
        place_request(
            higanr_core::script::PRE | higanr_core::script::START,
            5,
            0x0000_7776,
        );
        let c0 = calls_now();
        let bad0 = BAD_ARGS.load(Ordering::SeqCst);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        let d0 = draw_next_seq();
        draw_once();
        assert_eq!(
            SHOT_REQ.load(Ordering::SeqCst),
            0,
            "要求を 0 に戻していない"
        );
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0 + 1,
            "トランポリンへ飛んでいない"
        );
        assert_eq!(
            draw_next_seq(),
            d0.wrapping_add(1),
            "描画の記録を書いていない"
        );
        let seen = load(&SEEN);
        let want = [
            REG_EAX,
            REG_ECX,
            REG_EDX,
            REG_EBX,
            fake_this_addr(),
            REG_EDI,
            REG_EBP,
        ];
        assert_eq!(seen[..7], want[..], "トランポリンが見たレジスタ");
        assert_eq!(
            seen[8],
            PRE_ESP.load(Ordering::SeqCst) - 4,
            "esp を保っていない"
        );
        let pre = PRE_FLAGS.load(Ordering::SeqCst);
        assert_eq!(
            seen[9] & EFLAGS_ARITH,
            pre & EFLAGS_ARITH,
            "フラグを保っていない"
        );
        assert_eq!(load(&AFTER)[..7], want[..], "戻った後のレジスタ");

        let c1 = calls_now();
        let d: Vec<u32> = c0.iter().zip(c1.iter()).map(|(a, b)| b - a).collect();
        assert_eq!(
            d,
            vec![1, 1, 1, 1, 1, 1],
            "GetBackBuffer・GetDesc・LockRect・UnlockRect・Release が 1 回ずつ"
        );
        assert_eq!(
            BAD_ARGS.load(Ordering::SeqCst),
            bad0,
            "偽の関数に渡した this か引数が違う"
        );

        let (slot, head) = take_done().expect("置き場が「できた」になっていない");
        assert_eq!(head.fail(), Some(Fail::Ok));
        assert_eq!(
            head.get(H_REASON),
            (higanr_core::script::PRE | higanr_core::script::START) as u32
        );
        assert_eq!((head.get(H_STEP), head.get(H_K)), (5, 0x7776));
        assert_eq!(head.get(H_DRAW_SEQ), 0x7777);
        assert_eq!(head.get(H_HR_GETBACKBUFFER), 0);
        assert_eq!(head.desc(), [D3DFMT_X8R8G8B8, 1, 1, 0, W * H * 4, 0, W, H]);
        assert_eq!(head.pp(), [W, H, D3DFMT_X8R8G8B8]);
        assert_eq!(head.get(H_PITCH), PITCH);
        assert_eq!(
            (head.get(H_WIDTH), head.get(H_HEIGHT), head.get(H_BYTES)),
            (W, H, W * H * 4)
        );
        assert_eq!(head.get(H_DEVICE), addr_of(&FAKE_DEVICE));
        assert!(head.release_balanced());
        assert_ne!(head.get(H_TSC_TOTAL), 0);
        let _ = head.get(H_TSC_COPY);
        let pixels = copy_pixels(slot, head.get(H_BYTES) as usize);
        let mut want_px = Vec::new();
        for y in 0..H {
            for x in 0..W {
                want_px.extend_from_slice(&px(x, y));
            }
        }
        assert_eq!(pixels, want_px, "コピーした画素が違う");
        let png_bytes = head.to_png(&pixels).expect("PNG");
        let mut r = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
        r.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..3], &[px(0, 0)[2], px(0, 0)[1], px(0, 0)[0]]);
        release(slot);
        assert!(take_done().is_none());
        teardown();
    }

    #[test]
    fn no_request_or_off_means_no_d3d_calls() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        setup();
        let c0 = calls_now();
        draw_once();
        assert_eq!(calls_now(), c0, "要求が無いのに D3D8 を呼んだ");
        set_off();
        let dropped = counters().dropped_off;
        place_request(higanr_core::script::POST, 1, 2);
        assert_eq!(
            SHOT_REQ.load(Ordering::SeqCst),
            0,
            "撮影が無効なのに要求を置いた"
        );
        assert_eq!(counters().dropped_off, dropped + 1);
        draw_once();
        assert_eq!(calls_now(), c0);
        teardown();
    }

    fn shoot_with(prep: impl FnOnce()) -> (ShotHead, [u32; 6]) {
        setup();
        prep();
        place_request(higanr_core::script::POST, 2, 9);
        let c0 = calls_now();
        draw_once();
        let c1 = calls_now();
        let (slot, head) = take_done().expect("失敗でも「できた」にする");
        release(slot);
        teardown();
        (head, core::array::from_fn(|i| c1[i] - c0[i]))
    }

    /// 並び: [GetBackBuffer, その成功, GetDesc, LockRect, UnlockRect, Release]。
    #[test]
    fn failure_paths_keep_release_balanced() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        type Prep = fn();
        let cases: [(&str, Prep, Fail, [u32; 6]); 6] = [
            (
                "デバイスが 0",
                || FAKE_DEVICE_PTR.store(0, Ordering::SeqCst),
                Fail::NoDevice,
                [0, 0, 0, 0, 0, 0],
            ),
            (
                "GetBackBuffer の失敗",
                || GBB_HR.store(E_FAIL, Ordering::SeqCst),
                Fail::GetBackBuffer,
                [1, 0, 0, 0, 0, 0],
            ),
            (
                "面が NULL",
                || GBB_NULL.store(true, Ordering::SeqCst),
                Fail::NullSurface,
                [1, 1, 0, 0, 0, 0],
            ),
            (
                "LockRect の失敗",
                || LOCK_HR.store(E_FAIL, Ordering::SeqCst),
                Fail::LockRect,
                [1, 1, 1, 1, 0, 1],
            ),
            (
                "幅が present parameters と違う",
                || DESC_W.store(W + 1, Ordering::SeqCst),
                Fail::SizeMismatch,
                [1, 1, 1, 0, 0, 1],
            ),
            (
                "知らない形式",
                || DESC_FMT.store(50, Ordering::SeqCst),
                Fail::UnknownFormat,
                [1, 1, 1, 0, 0, 1],
            ),
        ];
        for (label, prep, fail, want) in cases {
            let (head, d) = shoot_with(prep);
            assert_eq!(head.fail(), Some(fail), "{label}");
            assert_eq!(d, want, "{label}: 呼ばれた回数");
            assert_eq!(
                d[5],
                if fail == Fail::NullSurface { 0 } else { d[1] },
                "{label}: Release の回数が GetBackBuffer の成功と違う"
            );
            assert!(head.release_balanced(), "{label}: 頭の呼んだ関数のビット");
        }
    }

    #[test]
    fn no_free_slot_is_counted_and_nothing_is_overwritten() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        setup();
        for k in 0..SHOT_SLOTS as u32 {
            place_request(higanr_core::script::WAIT, 0, 100 + k);
            draw_once();
        }
        let before: Vec<u32> = (0..SHOT_SLOTS)
            .map(|i| HEADS[i].0[H_K].load(Ordering::SeqCst))
            .collect();
        let nf = counters().no_free;
        let c0 = calls_now();
        place_request(higanr_core::script::WAIT, 0, 999);
        draw_once();
        assert_eq!(counters().no_free, nf + 1);
        assert_eq!(calls_now(), c0, "空きが無いのに D3D8 を呼んだ");
        let after: Vec<u32> = (0..SHOT_SLOTS)
            .map(|i| HEADS[i].0[H_K].load(Ordering::SeqCst))
            .collect();
        assert_eq!(before, after, "できた置き場を書き換えた");
        let mut ks = Vec::new();
        while let Some((i, h)) = take_done() {
            ks.push(h.get(H_K));
            release(i);
        }
        assert_eq!(ks, vec![100, 101, 102, 103]);
        teardown();
    }

    #[test]
    fn merged_requests_and_panic_stop() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        setup();
        place_request(higanr_core::script::TRANSITION, 1, 10);
        place_request(higanr_core::script::SETTLE, 2, 11);
        draw_once();
        let (i, h) = take_done().unwrap();
        release(i);
        assert_eq!(
            h.get(H_REASON),
            (higanr_core::script::TRANSITION | higanr_core::script::SETTLE) as u32
        );
        assert_eq!((h.get(H_K), h.get(H_STEP), h.get(H_MERGED)), (11, 2, 1));
        let p0 = counters().panicked;
        PANIC_NEXT.store(true, Ordering::SeqCst);
        place_request(higanr_core::script::WAIT, 0, 12);
        draw_once();
        assert_eq!(counters().panicked, p0 + 1);
        assert_eq!(
            SHOT_ON.load(Ordering::SeqCst),
            0,
            "パニックの後も撮影が有効"
        );
        assert_eq!(SHOT_REQ.load(Ordering::SeqCst), 0);
        teardown();
    }

    #[test]
    fn shot_on_draw_does_not_allocate() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        setup();
        place_request(higanr_core::script::PRE, 0, 1);
        let a0 = crate::test_alloc::count();
        shot_on_draw();
        let a1 = crate::test_alloc::count();
        assert_eq!(a1, a0, "撮影の経路で確保した");
        assert!(take_done().is_some_and(|(_, h)| h.fail() == Some(Fail::Ok)));
        teardown();
    }
}
