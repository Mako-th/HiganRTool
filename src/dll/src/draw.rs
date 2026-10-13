//! 描画の地点（`0x0042DD51`）の素のアセンブリの入口と、描画のリングバッファ（unsafe はアセンブリだけ）。
//!
//! EndScene の直前の `mov eax,[0x4B3108]` を inline detour（`detour.rs`）で差し替え、この入口へ飛ばす。入口は回数を `lock inc` し、描画のリングバッファ
//! （1 件 16 バイト: TSC 8 / その時点の tick の番号 `SEQ` の値 4 / 札 4）に 1 件書いて、トランポリンへ `jmp` する。トランポリンは盗んだ
//! `mov eax,[0x4B3108]` を実行して `0x0042DD56` へ戻る。**ゲームのスレッドで Rust のコードは走らない**: ただし `overlay_win::OVERLAY_ON` が
//! 0 でないときは `overlay_on_draw`（右下に 1 行を描く）、`shot_win::SHOT_REQ` が 0 でないときは `shot_on_draw`（バックバッファの撮影）を、
//! 全部のレジスタとフラグを保って呼ぶ（文字の描画が先。撮った絵に文字が写る）。描画のリングバッファの読み方は tick のリングバッファと同じ seqlock
//! （`ticks::classify`）。

use core::sync::atomic::{AtomicU32, Ordering, fence};

use crate::ticks::{RING_MASK, SlotState, classify};

/// 描画のリングバッファの件数（2 の冪）。
pub const DRAW_RING_LEN: u32 = 4096;
pub const DRAW_SLOT_WORDS: usize = 4;
pub const DRAW_SLOT_SHIFT: u32 = 4;

// 枠の中のバイトの位置（入口のアセンブリが `const` で使う。値はここだけ）。
/// TSC（`rdtsc`）の下位 / 上位。
pub const DOFF_TSC_LO: u32 = 0;
pub const DOFF_TSC_HI: u32 = 4;
/// その時点の tick の番号（`stub::SEQ`。描画がどの更新の後に走ったかを結びつける）。
pub const DOFF_TICK_SEQ: u32 = 8;
/// 番号札（seq + 1）。0 は「書いている途中」。中身の後ろに置く（tick のリングバッファと同じ seqlock）。
pub const DOFF_TICKET: u32 = 12;

/// 描画の地点を通った回数。入口が `lock inc`、読み取りのスレッドが読む。
pub static DRAW_CALLS: AtomicU32 = AtomicU32::new(0);
/// 描画のリングバッファの次の番号。入口が `lock xadd` で 1 つずつ取る。
pub static DRAW_SEQ: AtomicU32 = AtomicU32::new(0);
/// 入口がトランポリンへ飛ぶためのアドレス。`detour::install_detour` が JMP を書く前に置く（0 の間はこの入口へ向く
/// JMP はまだ無い）。**0 なら入口は 0 アドレスへ飛ぶので、置く前に JMP を書いてはならない。**
pub static DRAW_TRAMPOLINE: AtomicU32 = AtomicU32::new(0);

/// 描画のリングバッファの 1 件（16 バイト）。
#[repr(C)]
pub struct DrawSlot {
    pub words: [AtomicU32; DRAW_SLOT_WORDS],
}

#[repr(C)]
pub struct DrawRing {
    pub slots: [DrawSlot; DRAW_RING_LEN as usize],
}

pub static DRAW_RING: DrawRing = DrawRing {
    slots: [const {
        DrawSlot {
            words: [const { AtomicU32::new(0) }; DRAW_SLOT_WORDS],
        }
    }; DRAW_RING_LEN as usize],
};

pub const DRAW_RING_MASK: u32 = DRAW_RING_LEN - 1;
// 描画のリングバッファの番号も tick のリングバッファと同じ大きさ（4096）なので `RING_MASK` と同じ（読み手の分岐を減らす）。
const _: () = assert!(DRAW_RING_MASK == RING_MASK);

// SAFETY（`#[unsafe(naked)]`）: 本体は `naked_asm!` だけ。差し替えた `mov eax,[0x4B3108]`（`A1 08 31 4B 00`）の代わりに呼ばれ、
// 最後にトランポリンへ飛ぶ。フラグと eax・ecx・edx・ebx を push して逆順に pop し、esi・edi・ebp・esp は変えない。
// 書くのは DLL の静的変数だけで、ゲームのメモリは読まない。DRAW_TRAMPOLINE は JMP を書く前に置かれる（`detour::install_detour`）。
// 枠の番号は `and DRAW_RING_MASK` でリングバッファの中に収まる。
// 文字の描画・撮影の経路は `pushad` → `cld`（Rust の関数が前提にする DF = 0。フラグは先頭の `pushfd` で取ってあり最後の `popfd` で
// 戻る）→ `extern "C"` の呼び出し（中は全部 `catch_unwind`）→ `popad`（スタックは `pushad` の 32 バイトだけ使い、`popad` の後は入った時と同じ位置）。`0x0042DD51` は `SetTexture` の call の直後で x87 の
// スタックは空の見込み。ゲームのコードは XMM を使っていない見込みなので、Rust が XMM を使っても壊さない。x87 の制御語は変えない。
/// 描画の地点（`0x0042DD51`）の差し替え先。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn draw_entry() {
    core::arch::naked_asm!(
        "pushfd",
        "push eax",
        "push ecx",
        "push edx",
        "push ebx",
        "lock inc dword ptr [{calls}]",
        // 番号を取る（ecx = seq）。枠の位置 ebx = (seq & MASK) << SHIFT。
        "mov ecx, 1",
        "lock xadd dword ptr [{seq}], ecx",
        "mov ebx, ecx",
        "and ebx, {mask}",
        "shl ebx, {shift}",
        "mov dword ptr [{ring} + ebx + {o_ticket}], 0",
        "rdtsc",
        "mov dword ptr [{ring} + ebx + {o_tsc_lo}], eax",
        "mov dword ptr [{ring} + ebx + {o_tsc_hi}], edx",
        "mov eax, dword ptr [{tick_seq}]",
        "mov dword ptr [{ring} + ebx + {o_tick_seq}], eax",
        // 札 = seq + 1（中身の書きより後に見える。x86 は書きの順を入れ替えない）。
        "inc ecx",
        "mov dword ptr [{ring} + ebx + {o_ticket}], ecx",
        // 文字の描画（有効なら）。撮影より先に描く（撮った絵に文字が写る）。0 なら素通り（cmp と je の 2 つだけ）。
        "cmp dword ptr [{ov_on}], 0",
        "je 3f",
        "pushad",
        "cld",
        "call {ov_draw}",
        "popad",
        "3:",
        // 撮影（要求が 0 でなければ）。0 なら素通り。
        "cmp dword ptr [{shot_req}], 0",
        "je 2f",
        "pushad",
        "cld",
        "call {shot_on_draw}",
        "popad",
        "2:",
        "pop ebx",
        "pop edx",
        "pop ecx",
        "pop eax",
        "popfd",
        "jmp dword ptr [{tramp}]",
        calls = sym DRAW_CALLS,
        seq = sym DRAW_SEQ,
        ring = sym DRAW_RING,
        tramp = sym DRAW_TRAMPOLINE,
        tick_seq = sym crate::stub::SEQ,
        shot_req = sym crate::shot_win::SHOT_REQ,
        shot_on_draw = sym crate::shot_win::shot_on_draw,
        ov_on = sym crate::overlay_win::OVERLAY_ON,
        ov_draw = sym crate::overlay_win::overlay_on_draw,
        mask = const DRAW_RING_MASK,
        shift = const DRAW_SLOT_SHIFT,
        o_ticket = const DOFF_TICKET,
        o_tsc_lo = const DOFF_TSC_LO,
        o_tsc_hi = const DOFF_TSC_HI,
        o_tick_seq = const DOFF_TICK_SEQ,
    )
}

pub fn draw_entry_address() -> u32 {
    draw_entry as *const () as usize as u32
}

pub fn draw_calls() -> u32 {
    DRAW_CALLS.load(Ordering::Acquire)
}

pub fn draw_next_seq() -> u32 {
    DRAW_SEQ.load(Ordering::Acquire)
}

/// トランポリンのアドレスを置く（JMP を書く前に呼ぶこと。0 なら入口は 0 アドレスへ飛ぶ）。
pub fn set_draw_trampoline(addr: u32) {
    DRAW_TRAMPOLINE.store(addr, Ordering::SeqCst);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawRecord {
    pub seq: u32,
    pub tsc: u64,
    pub tick_seq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawRead {
    Record(DrawRecord),
    NotYet,
    Overwritten,
}

pub fn read_draw_slot(seq: u32) -> DrawRead {
    let slot = &DRAW_RING.slots[(seq & DRAW_RING_MASK) as usize];
    let ticket = &slot.words[(DOFF_TICKET / 4) as usize];
    let t1 = ticket.load(Ordering::Acquire);
    let w: [u32; DRAW_SLOT_WORDS] = core::array::from_fn(|i| slot.words[i].load(Ordering::Relaxed));
    fence(Ordering::Acquire);
    let t2 = ticket.load(Ordering::Relaxed);
    match classify(seq, t1, t2) {
        SlotState::Complete => DrawRead::Record(DrawRecord {
            seq,
            tsc: (w[(DOFF_TSC_HI / 4) as usize] as u64) << 32
                | w[(DOFF_TSC_LO / 4) as usize] as u64,
            tick_seq: w[(DOFF_TICK_SEQ / 4) as usize],
        }),
        SlotState::NotYet => DrawRead::NotYet,
        SlotState::Overwritten => DrawRead::Overwritten,
    }
}

#[cfg(test)]
mod tests {
    //! 本物の描画の入口を検査のプロセスで呼ぶ（花映塚は使わない）。
    use super::*;
    use crate::stub::testkit::{
        AFTER, EFLAGS_ARITH, PRE_ESP, PRE_FLAGS, REAL_HOOK_TESTS, SEEN, TARGET_HITS,
        call_with_regs, load, seen_target_addr,
    };

    #[test]
    fn draw_slot_layout() {
        assert_eq!(
            [DOFF_TSC_LO, DOFF_TSC_HI, DOFF_TICK_SEQ, DOFF_TICKET],
            [0, 4, 8, 12]
        );
        assert_eq!(1u32 << DRAW_SLOT_SHIFT, DRAW_SLOT_WORDS as u32 * 4);
        assert!(DRAW_RING_LEN.is_power_of_two());
        assert_eq!(core::mem::size_of::<DrawSlot>(), 16);
    }

    fn draw_ticket(seq: u32) -> u32 {
        DRAW_RING.slots[(seq & DRAW_RING_MASK) as usize].words[(DOFF_TICKET / 4) as usize]
            .load(Ordering::SeqCst)
    }

    fn poison_draw(seq: u32) {
        for w in DRAW_RING.slots[(seq & DRAW_RING_MASK) as usize]
            .words
            .iter()
        {
            w.store(0xA5A5_A5A5, Ordering::SeqCst);
        }
    }

    #[test]
    fn entry_draw_preserves_registers_flags_and_esp() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        crate::shot_win::set_off();
        crate::overlay_win::set_off();
        set_draw_trampoline(seen_target_addr());
        crate::stub::SEQ.store(0x0BADF00D, Ordering::SeqCst);

        let s0 = draw_next_seq();
        poison_draw(s0);
        let calls0 = draw_calls();
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        call_with_regs(draw_entry_address());

        assert_eq!(draw_calls(), calls0 + 1, "回数を数えていない");
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0 + 1,
            "トランポリンへ飛んでいない"
        );
        assert_eq!(draw_next_seq(), s0.wrapping_add(1), "番号を進めていない");
        let seen = load(&SEEN);
        let after = load(&AFTER);
        for i in [3usize, 4, 5, 6] {
            assert_eq!(
                seen[i], after[i],
                "レジスタ {i} を保っていない（トランポリンが見た値）"
            );
        }
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

        let rec = match read_draw_slot(s0) {
            DrawRead::Record(r) => r,
            other => panic!("描画の記録が採れない: {other:?}"),
        };
        assert_eq!(rec.tick_seq, 0x0BADF00D, "tick の番号");
        assert_ne!(rec.tsc, 0, "TSC を取っていない");
        assert_eq!(draw_ticket(s0), s0.wrapping_add(1), "札");
        set_draw_trampoline(0);
    }

    #[test]
    fn read_draw_slot_follows_the_ticket() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let seq = draw_next_seq().wrapping_add(100);
        let slot = &DRAW_RING.slots[(seq & DRAW_RING_MASK) as usize];
        let t = &slot.words[(DOFF_TICKET / 4) as usize];
        let before = t.load(Ordering::SeqCst);
        slot.words[(DOFF_TICK_SEQ / 4) as usize].store(4242, Ordering::SeqCst);
        t.store(seq.wrapping_add(1), Ordering::SeqCst);
        assert!(matches!(read_draw_slot(seq), DrawRead::Record(r) if r.tick_seq == 4242));
        t.store(0, Ordering::SeqCst);
        assert_eq!(read_draw_slot(seq), DrawRead::NotYet);
        t.store(
            seq.wrapping_add(1).wrapping_add(DRAW_RING_LEN),
            Ordering::SeqCst,
        );
        assert_eq!(read_draw_slot(seq), DrawRead::Overwritten);
        t.store(before, Ordering::SeqCst);
    }
}
