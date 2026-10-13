//! 差し替え先の入口（素のアセンブリ）と、入口が触る静的変数。アセンブリの unsafe はここだけ。
//!
//! `0x0043112D` の `call 0x0042E9C0` の rel32 をこの入口へ向ける。入口は回数を 1 つ増やし、tick ごとの記録を 1 件（1024 バイト）リングバッファに書いて、
//! 元の飛び先へ `jmp` する。**ゲームのスレッドで Rust のコードは走らない**（例外は `script_win::SCRIPT_ON` が 0 でないときの
//! `script_on_tick` の呼び出しだけ）。使うレジスタは push / pop で戻し、`jmp` の時点のスタックは入った時と同じなので、元の飛び先
//! （poll_all）の `ret` は呼び出し元（on_tick の call の次）へ直接戻る。
//!
//! 書く順は「`lock xadd` で番号を取る → 札を 0 → 中身 → 札 = 番号 + 1」。読み取りのスレッドは札・中身・札の順に読み、2 回とも
//! 番号 + 1 のときだけ採る（`read_slot`・`ticks::classify`）。x86 は書きの順を入れ替えない。先頭の 64 バイトの後ろへ**コピーの表**（最大 8 行）の
//! 範囲を `rep movsb` で札の前にコピーする（`rep movsb` の中の書きは互いに順が入れ替わりうるが、後の書き（札）とは入れ替わらない ——
//! Intel SDM Vol.3A の「Fast-String Operation and Out-of-Order Stores」。検査 `concurrent_reader_never_takes_a_torn_slot` が破れた枠 0 件を見る）。
//!
//! **描画だけのフレーム**: 出口 `RENDER_ONLY_EXIT` が 0 でなく `RENDER_ONLY_LEFT` が 0 より大きければ、枠の `+52` に印 1 を書き、残りを 1 減らし、
//! 札を書き、スタックの頂上の戻りアドレスを出口へ替えて `ret` する（poll_all を呼ばない）。call の時点のスタックは [戻りアドレス `0x00431132`][esi][ebx]
//! で、出口 `0x00431259` は `pop esi; mov eax,3; pop ebx; ret` ⇒ on_tick が 3 を返し、そのフレームは更新を走らせずに描画へ進む。
//! 普通の件は `+52` に 0 を書く。
//!
//! **台本・ネットワークの分かれ道**: 札を書いた後、`SCRIPT_ON` が 0 なら元の飛び先へ `jmp`。0 でなければ `script_on_tick(k)` を呼び、戻り値が 0 なら
//! 元の飛び先へ `jmp`、1 なら `ret`、2 なら戻りアドレスを `RENDER_ONLY_EXIT` へ替えて `ret`（ネットワークの lockstep で相手の入力ワードが揃わなかったフレーム）。
//! **開始のフック**の入口 `match_start_entry`: `0x0041AB2B` の `call 0x0043BF30` の rel32 をここへ向け、回数を数え、`NET_ON` が 0 でなければ
//! `match_start_on` を呼んでから元の飛び先へ `jmp` する。

use core::sync::atomic::{AtomicU32, Ordering, fence};

use crate::copies::check_layout;
use crate::th09_addr::{SUP_FRAME, SUP_SCENE_CUR, SUP_SCENE_REQ, WINDOW_DEADLINE, WINDOW_NOW};
use crate::ticks::{
    COPY_MAX_ROWS, CopyRow, OFF_DEADLINE_HI, OFF_DEADLINE_LO, OFF_ESI, OFF_FPUCW, OFF_NOW_HI,
    OFF_NOW_LO, OFF_RENDER_ONLY, OFF_SCENE_CUR, OFF_SCENE_REQ, OFF_SUP_FRAME, OFF_TICKET, OFF_TID,
    OFF_TSC_HI, OFF_TSC_LO, RING_LEN, RING_MASK, SLOT_BYTES, SLOT_SHIFT, SLOT_WORDS, SlotRead,
    SlotState, Snap, classify,
};

/// 32 ビットの TEB の中のスレッド ID の位置（`ClientId` の `UniqueThread` = 0x24）。x86 の `GetCurrentThreadId` と同じ所を読む。
/// 検査 `entry_writes_one_slot_per_call` が `GetCurrentThreadId` と比べる。
pub(crate) const TEB_THREAD_ID: u32 = 0x24;

/// 入口を通った回数。ゲームのスレッドが `lock inc` で増やし、読み取りのスレッドが読む（1 秒ごとの行）。`AtomicU32` は `u32` と
/// 同じ大きさ・並びなので、アセンブリから素の 4 バイトとして書いてよい（下の静的変数も同じ）。
pub static CALLS: AtomicU32 = AtomicU32::new(0);

/// 元の飛び先。設置の前に、rel32 から計算して表の値と一致したものだけを置く（`hook::install_call_hook`）。置く前（0 の間）は、
/// この入口へ向いた call はまだ 1 つも無い。
pub static ORIGINAL_TARGET: AtomicU32 = AtomicU32::new(0);

/// ウィンドウの構造体のアドレス（花映塚では `th09_addr::WINDOW_ADDR`）。元の飛び先と同じく rel32 を書く前に置く。
/// **0 なら入口はウィンドウを読まず、ウィンドウの 2 欄に 0 を書く**（検査のプロセスや、ウィンドウが読めなかったとき）。
pub static WINDOW: AtomicU32 = AtomicU32::new(0);

/// 次に書く番号。入口が `lock xadd` で 1 つずつ取る（呼ぶスレッドが 1 本でなくても枠が重ならない）。
pub static SEQ: AtomicU32 = AtomicU32::new(0);

/// 描画だけのフレームの出口（花映塚では `0x00431259`）。ウィンドウのアドレスと同じく rel32 を書く前に置く。**0 なら入口は分岐を通らない**
/// （検査のプロセスと、`render_only_exit` 行が照合で一致しなかったとき）。
pub static RENDER_ONLY_EXIT: AtomicU32 = AtomicU32::new(0);

/// 描画だけのフレームをあと何回作るか。0 から N にするのは読み取りのスレッドだけ（`try_start_render_only`。compare-exchange で
/// 0 のときだけ）、1 減らすのは入口だけ（`lock dec`）。入口は「読んで 0 より大きい → 減らす」の 2 手で、その間に読み取りの
/// スレッドが書かないのは「0 のときだけ N にする」決まりによる（入口を呼ぶゲームのスレッドは 1 本の見込み）。
pub static RENDER_ONLY_LEFT: AtomicU32 = AtomicU32::new(0);

/// コピーの表の 1 行の大きさ（元・長さ・置き場の 4 バイトずつ。入口のアセンブリが `add eax, 12` で進む）。
const COPY_ROW_BYTES: u32 = 12;
const COPY_ROW_WORDS: usize = (COPY_ROW_BYTES / 4) as usize;

/// コピーの表の行の数。**0 なら入口は何もコピーしない**（検査のプロセスと、表が置けなかったとき）。元の飛び先・ウィンドウのアドレスと同じく、
/// rel32 を書く前に置く（`set_copy_table`）。
pub static COPY_COUNT: AtomicU32 = AtomicU32::new(0);
/// コピーの表（行 i は語 3i・3i+1・3i+2 に元・長さ・置き場）。`COPY_COUNT` より後ろの行は読まれない。
pub static COPY_ROWS: [AtomicU32; COPY_MAX_ROWS * COPY_ROW_WORDS] =
    [const { AtomicU32::new(0) }; COPY_MAX_ROWS * COPY_ROW_WORDS];

/// リングバッファの 1 件（1024 バイト。中の配置は `ticks` の `OFF_*` と `OFF_COPY_*`）。64 バイト境界に置き、先頭の 64 バイトがキャッシュの行
/// 1 つに収まるようにする（読み取りのスレッドとゲームのスレッドが別の件を触るときに同じ行を取り合わない）。
#[repr(C, align(64))]
pub struct Slot {
    pub words: [AtomicU32; SLOT_WORDS],
}

/// リングバッファ（4096 件 = 4 MiB）。静的変数の初めの値は全部 0 で、DLL の .bss に置かれる。
#[repr(C, align(64))]
pub struct Ring {
    pub slots: [Slot; RING_LEN as usize],
}

const _: () = assert!(core::mem::size_of::<Slot>() == SLOT_BYTES as usize);
const _: () = assert!(core::mem::size_of::<Ring>() == (RING_LEN * SLOT_BYTES) as usize);

pub static RING: Ring = Ring {
    slots: [const {
        Slot {
            words: [const { AtomicU32::new(0) }; SLOT_WORDS],
        }
    }; RING_LEN as usize],
};

// SAFETY（`#[unsafe(naked)]`）: 本体は `naked_asm!` だけで、入る時の状態（戻りアドレスがスタックの頂上、引数なし）をそのまま元の飛び先へ渡す:
// - eax・ecx・edx・ebx を push し、最後に逆の順で pop してから `jmp`。esi・edi はコピーの間だけ使い push / pop で戻す。ebp・esp には書かない（esp は push / pop で動くだけで、`jmp` の時点では入った時と同じ）。
// - 書くのは DLL の静的変数と、描画だけのフレームの経路での自分の戻りアドレスだけ。ゲームのメモリは読むだけ: `[esi + 0x454/0x58C/0x590]`
//   （on_tick の this）と、WINDOW が 0 でなければ `[WINDOW + 0x2C/0x3C]` の 8 バイトずつ。枠の番号は `and RING_MASK` でリングバッファの中に収め、
//   `shl SLOT_SHIFT` で 1024 バイトの位置にする。
// - コピーの表の各行は `set_copy_table` が `copies::check_layout` を通したものだけ（枠の先頭 64 バイトの後ろ・1024 バイトの中・互いに重ならない）
//   なので、`rep movsb` は自分の枠の中だけに書く。元の範囲は設置の前に読み取りのスレッドが「コミット済みで読める保護」と確かめたもの（`session`。花映塚では th09.exe の .data）で、
//   コピーする間は ecx（番号）・esi・edi を push / pop で戻す。`cld` は、呼ぶ側が決まりを
//   破っても `rep movsb` が後ろ向きに枠の外へ書かないための守り。
// - 静的変数は絶対アドレスで指す（32 ビットでは `sym` が絶対アドレスになり、リンカの再配置で直る。objdump で確認）。ORIGINAL_TARGET・WINDOW・
//   コピーの表・RENDER_ONLY_EXIT は、この入口へ向く call を書く前に置かれる（`hook::install_call_hook`）。
// - 描画だけのフレームの経路は、`[esp + 16]` の戻りアドレスを RENDER_ONLY_EXIT（照合で `5E B8 03 00 00 00 5B C3` と一致したアドレス。0 ならこの経路を
//   通らない）に書き換えて pop で全部戻して `ret` する（書き換えるのは自分が呼ばれた call の戻りアドレスだけ）。出口は esi と ebx を pop するので、呼ぶ側（on_tick）が call の前に
//   push ebx; push esi を積んでいることが前提（`0x00431110`〜`0x00431111`）。
// - 台本の経路: tick の番号 k を積んで `script_on_tick`（`extern "C"`。中は全部 `catch_unwind`）を呼ぶ。cdecl の決まりで ebx・esi・edi・ebp と
//   esp は保たれ、DF は `cld` で 0、x87 は call の境目で空（制御語は変えない）。戻った後は 4 つの pop で入った時のレジスタへ戻し、戻り値が 0 なら元の飛び先へ `jmp`、0 でなければ `ret` で呼び出し元（`0x00431132`）へ
//   戻る。そこは `mov al, [esi+0x5D4]` で eax を読み直すので eax は使われない。戻り値が 2 なら `[esp + 16]` を RENDER_ONLY_EXIT に書き換える
//   （出口が 0 なら書き換えない）。
// - 進め直しの回（`rollback_play_win::RESIM_TICK` が 0 でない）: 記録・時刻の記録・コピー・描画だけの分岐を全部飛ばし、`ecx = 1`（k = 0）で
//   台本の分かれ道（`7:`）へ進む。`SCRIPT_ON` が 0 なら元の飛び先へ `jmp`（進め直しは `SCRIPT_ON` が開いたネットワークの経路の中でしか起きないので、実際には通らない守り）。
// - 時刻の記録（`pacing::INPUT_STAMP_ON` が 0 でない）: `[esi + 0x454]` を積んで `pacing_win::pace_input`（`extern "C"`。中は `catch_unwind`）を
//   呼び、`add esp, 4` で片付ける。壊れてよい eax・ecx・edx は、この後の命令が読む前に置き直し、出口で 4 つの pop が入った時の値へ戻す。DF は `cld` で 0、x87 は使わない。
/// 入力と待ちの call（`0x0043112D`）の差し替え先。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn input_wait_entry() {
    core::arch::naked_asm!(
        "push eax",
        "push ecx",
        "push edx",
        "push ebx",
        // 進め直しの回（rollback の Present の後の進め直し。`RESIM_TICK` が 0 でない）は、回数も tick の記録も時刻の記録も積まず、
        // 台本の分かれ道へ直に進む（k は使わない）。0 の間は cmp と je の 2 命令だけ。
        "cmp dword ptr [{resim}], 0",
        "je 13f",
        "cmp dword ptr [{script_on}], 0",
        "je 14f",
        "mov ecx, 1",
        "jmp 7f",
        "14:",
        "pop ebx",
        "pop edx",
        "pop ecx",
        "pop eax",
        "jmp dword ptr [{orig}]",
        "13:",
        "lock inc dword ptr [{calls}]",
        // 時刻の記録（`pacing::INPUT_STAMP_ON` が 0 なら cmp と je の 2 命令で素通り）。tick の欄を積んで、入った瞬間の QPC を記録のリングバッファへ。
        "cmp dword ptr [{stamp_on}], 0",
        "je 12f",
        "cld",
        "push dword ptr [esi + {t_frame}]",
        "call {pace_input}",
        "add esp, 4",
        "12:",
        // 番号を取る（ecx = seq）。枠の位置 ebx = (seq & MASK) << SHIFT。
        "mov ecx, 1",
        "lock xadd dword ptr [{seq}], ecx",
        "mov ebx, ecx",
        "and ebx, {mask}",
        "shl ebx, {shift}",
        // 札を 0（書いている途中）。
        "mov dword ptr [{ring} + ebx + {o_ticket}], 0",
        "rdtsc",
        "mov dword ptr [{ring} + ebx + {o_tsc_lo}], eax",
        "mov dword ptr [{ring} + ebx + {o_tsc_hi}], edx",
        "mov edx, dword ptr [{window}]",
        "test edx, edx",
        "jz 2f",
        "mov eax, dword ptr [edx + {w_now}]",
        "mov dword ptr [{ring} + ebx + {o_now_lo}], eax",
        "mov eax, dword ptr [edx + {w_now} + 4]",
        "mov dword ptr [{ring} + ebx + {o_now_hi}], eax",
        "mov eax, dword ptr [edx + {w_deadline}]",
        "mov dword ptr [{ring} + ebx + {o_dl_lo}], eax",
        "mov eax, dword ptr [edx + {w_deadline} + 4]",
        "mov dword ptr [{ring} + ebx + {o_dl_hi}], eax",
        "jmp 3f",
        "2:",
        "mov dword ptr [{ring} + ebx + {o_now_lo}], 0",
        "mov dword ptr [{ring} + ebx + {o_now_hi}], 0",
        "mov dword ptr [{ring} + ebx + {o_dl_lo}], 0",
        "mov dword ptr [{ring} + ebx + {o_dl_hi}], 0",
        "3:",
        "mov eax, dword ptr [esi + {t_frame}]",
        "mov dword ptr [{ring} + ebx + {o_frame}], eax",
        "mov eax, dword ptr [esi + {t_cur}]",
        "mov dword ptr [{ring} + ebx + {o_cur}], eax",
        "mov eax, dword ptr [esi + {t_req}]",
        "mov dword ptr [{ring} + ebx + {o_req}], eax",
        "mov dword ptr [{ring} + ebx + {o_esi}], esi",
        "mov eax, dword ptr fs:[{teb_tid}]",
        "mov dword ptr [{ring} + ebx + {o_tid}], eax",
        "mov dword ptr [{ring} + ebx + {o_fpucw}], 0",
        "fnstcw word ptr [{ring} + ebx + {o_fpucw}]",
        // コピーの表（件数 edx。0 なら飛ばす）。eax = 行、ecx = 長さ、esi = 元、edi = 枠の中の置き場。
        "mov edx, dword ptr [{copy_n}]",
        "test edx, edx",
        "jz 5f",
        "push ecx",
        "push esi",
        "push edi",
        "cld",
        "lea eax, [{copy_rows}]",
        "4:",
        "mov esi, dword ptr [eax]",
        "mov ecx, dword ptr [eax + 4]",
        "lea edi, [{ring} + ebx]",
        "add edi, dword ptr [eax + 8]",
        "rep movsb",
        "add eax, {row_bytes}",
        "dec edx",
        "jnz 4b",
        "pop edi",
        "pop esi",
        "pop ecx",
        "5:",
        // 描画だけのフレームの分かれ道（edx = 出口、eax = 残り）。出口が 0 か残りが 0 なら普通の経路へ。
        "mov edx, dword ptr [{exit}]",
        "test edx, edx",
        "jz 6f",
        "mov eax, dword ptr [{left}]",
        "test eax, eax",
        "jz 6f",
        // 印 1 → 残りを 1 減らす → 札（印は札より前に見える）。
        "mov dword ptr [{ring} + ebx + {o_mark}], 1",
        "lock dec dword ptr [{left}]",
        "inc ecx",
        "mov dword ptr [{ring} + ebx + {o_ticket}], ecx",
        // 戻りアドレス（4 つの push の下。on_tick の call の次 0x00431132）を出口へ替えて戻る。poll_all は呼ばない。
        "mov dword ptr [esp + 16], edx",
        "pop ebx",
        "pop edx",
        "pop ecx",
        "pop eax",
        "ret",
        "6:",
        // 普通の件の印は 0（前の周の 1 を消す）。
        "mov dword ptr [{ring} + ebx + {o_mark}], 0",
        // 札 = seq + 1（ここまでの中身の書きより後に見える。x86 は書きの順を入れ替えない）。
        "inc ecx",
        "mov dword ptr [{ring} + ebx + {o_ticket}], ecx",
        // M1d の台本の分かれ道。SCRIPT_ON が 0 なら今までどおり（下の 4 つの pop と元の飛び先への jmp）。
        "cmp dword ptr [{script_on}], 0",
        "jne 7f",
        "pop ebx",
        "pop edx",
        "pop ecx",
        "pop eax",
        "jmp dword ptr [{orig}]",
        "7:",
        // script_on_tick(k)。k = 札 - 1 = lock xadd で取った番号。cdecl（引数 1 つ、呼ぶ側が片付ける）。
        "dec ecx",
        "cld",
        "push ecx",
        "call {script_on_tick}",
        "add esp, 4",
        // 戻り値 2 = 描画だけのフレーム。戻りアドレス（4 つの push の下）を出口へ替えてから、下の 1 と同じ経路で戻る。出口が 0 なら替えない。
        "cmp eax, 2",
        "jne 9f",
        "mov edx, dword ptr [{exit}]",
        "test edx, edx",
        "jz 9f",
        "mov dword ptr [esp + 16], edx",
        "9:",
        // 戻り値 0 = 元の飛び先へ、それ以外 = poll_all を呼ばずに戻る（内蔵ネット対戦の枝と同じ）。
        // pop はフラグを変えないので、test の結果を pop の後で使える。
        "test eax, eax",
        "pop ebx",
        "pop edx",
        "pop ecx",
        "pop eax",
        "jnz 8f",
        "jmp dword ptr [{orig}]",
        "8:",
        "ret",
        resim = sym crate::rollback_play_win::RESIM_TICK,
        stamp_on = sym crate::pacing::INPUT_STAMP_ON,
        pace_input = sym crate::pacing_win::pace_input,
        script_on = sym crate::script_win::SCRIPT_ON,
        script_on_tick = sym crate::script_win::script_on_tick,
        exit = sym RENDER_ONLY_EXIT,
        left = sym RENDER_ONLY_LEFT,
        o_mark = const OFF_RENDER_ONLY,
        calls = sym CALLS,
        seq = sym SEQ,
        ring = sym RING,
        window = sym WINDOW,
        orig = sym ORIGINAL_TARGET,
        copy_n = sym COPY_COUNT,
        copy_rows = sym COPY_ROWS,
        row_bytes = const COPY_ROW_BYTES,
        mask = const RING_MASK,
        shift = const SLOT_SHIFT,
        o_ticket = const OFF_TICKET,
        o_tsc_lo = const OFF_TSC_LO,
        o_tsc_hi = const OFF_TSC_HI,
        o_now_lo = const OFF_NOW_LO,
        o_now_hi = const OFF_NOW_HI,
        o_dl_lo = const OFF_DEADLINE_LO,
        o_dl_hi = const OFF_DEADLINE_HI,
        o_frame = const OFF_SUP_FRAME,
        o_cur = const OFF_SCENE_CUR,
        o_req = const OFF_SCENE_REQ,
        o_esi = const OFF_ESI,
        o_tid = const OFF_TID,
        o_fpucw = const OFF_FPUCW,
        w_now = const WINDOW_NOW,
        w_deadline = const WINDOW_DEADLINE,
        t_frame = const SUP_FRAME,
        t_cur = const SUP_SCENE_CUR,
        t_req = const SUP_SCENE_REQ,
        teb_tid = const TEB_THREAD_ID,
    )
}

pub fn entry_address() -> u32 {
    input_wait_entry as *const () as usize as u32
}

/// 開始のフックの元の飛び先（花映塚では `0x0043BF30`）。rel32 を書く前に置く。
pub static MATCH_ORIGINAL: AtomicU32 = AtomicU32::new(0);
pub static MATCH_CALLS: AtomicU32 = AtomicU32::new(0);

// SAFETY（`#[unsafe(naked)]`）: `0x0041AB2B` の `call 0x0043BF30` の差し替え先。呼ぶ側は thiscall（`0x0041AB23 mov 0x4dc550,%ecx`、
// `0x0041AB29 push $0x8`）で、入った時のスタックは [戻りアドレス `0x0041AB30`][引数 8]。
// - `lock inc` と `cmp` はフラグだけを変える（call の境目なので呼ぶ側はフラグを当てにしない）。
// - `NET_ON` が 0 でなければ eax・ecx（this）・edx を push し、`script_win::match_start_on`（`extern "C"`、引数なし。中は全部
//   `catch_unwind`）を呼び、逆の順で pop する。cdecl の決まりで ebx・esi・edi・ebp と esp は保たれる。DF は `cld` で 0。x87 の
//   スタックは call の境目なので空。
// - 最後の `jmp` の時点のスタックは入った時と同じなので、元の飛び先の `ret 4` は呼ぶ側へ直に戻る。MATCH_ORIGINAL は、この入口へ
//   向く call を書く前に置かれる（`hook::install_call_hook_at`）。
/// 開始の call（`0x0041AB2B`）の差し替え先。Rust からは呼ばない（アドレスを取るだけ）。
#[unsafe(naked)]
pub extern "C" fn match_start_entry() {
    core::arch::naked_asm!(
        "lock inc dword ptr [{calls}]",
        "cmp dword ptr [{net_on}], 0",
        "je 2f",
        "push eax",
        "push ecx",
        "push edx",
        "cld",
        "call {on_start}",
        "pop edx",
        "pop ecx",
        "pop eax",
        "2:",
        "jmp dword ptr [{orig}]",
        calls = sym MATCH_CALLS,
        net_on = sym crate::script_win::NET_ON,
        on_start = sym crate::script_win::match_start_on,
        orig = sym MATCH_ORIGINAL,
    )
}

pub fn match_entry_address() -> u32 {
    match_start_entry as *const () as usize as u32
}

pub fn set_match_original(target: u32) {
    MATCH_ORIGINAL.store(target, Ordering::SeqCst);
}

pub fn calls() -> u32 {
    CALLS.load(Ordering::Acquire)
}

/// 元の飛び先を置く。rel32 を書くより前に呼ぶこと（書いた後に呼ぶと、入口が 0 アドレスへ飛びうる）。
pub fn set_original_target(target: u32) {
    ORIGINAL_TARGET.store(target, Ordering::SeqCst);
}

/// ウィンドウのアドレスを置く（0 なら入口はウィンドウを読まない）。rel32 を書くより前に呼ぶこと。
pub fn set_window(addr: u32) {
    WINDOW.store(addr, Ordering::SeqCst);
}

/// 描画だけのフレームの出口を置く（0 なら入口は分岐を通らない）。rel32 を書くより前に呼ぶこと。
pub fn set_render_only_exit(addr: u32) {
    RENDER_ONLY_EXIT.store(addr, Ordering::SeqCst);
}

/// 描画だけのフレームを `n` 回作るよう入口に頼む。残りが 0 のときだけ置き（compare-exchange）、置けたら true。
/// 残りが 0 でない（前の頼みをまだ使い切っていない）なら何もせず false。
pub fn try_start_render_only(n: u32) -> bool {
    RENDER_ONLY_LEFT
        .compare_exchange(0, n, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

pub fn render_only_left() -> u32 {
    RENDER_ONLY_LEFT.load(Ordering::SeqCst)
}

/// コピーの表を置く。rel32 を書くより前に呼ぶこと。枠の側の確かめ（`copies::check_layout`）を通らなければ件数 0 のまま置き、
/// false を返す（入口が枠の外へ書かないための最後の守り。元が読めるかは呼び出し側が先に確かめる）。件数をいったん 0 にしてから
/// 行を書き、最後に件数を置く（書き換え途中の表を入口が使わない）。
pub fn set_copy_table(rows: &[CopyRow]) -> bool {
    COPY_COUNT.store(0, Ordering::SeqCst);
    if check_layout(rows).is_err() {
        return false;
    }
    for (i, r) in rows.iter().enumerate() {
        let base = i * COPY_ROW_WORDS;
        COPY_ROWS[base].store(r.src, Ordering::SeqCst);
        COPY_ROWS[base + 1].store(r.len, Ordering::SeqCst);
        COPY_ROWS[base + 2].store(r.dst, Ordering::SeqCst);
    }
    COPY_COUNT.store(rows.len() as u32, Ordering::SeqCst);
    true
}

pub fn copy_table() -> Vec<CopyRow> {
    let n = (COPY_COUNT.load(Ordering::SeqCst) as usize).min(COPY_MAX_ROWS);
    (0..n)
        .map(|i| {
            let base = i * COPY_ROW_WORDS;
            CopyRow {
                src: COPY_ROWS[base].load(Ordering::SeqCst),
                len: COPY_ROWS[base + 1].load(Ordering::SeqCst),
                dst: COPY_ROWS[base + 2].load(Ordering::SeqCst),
            }
        })
        .collect()
}

/// 次に書かれる番号（ここより前の番号は、入口が取った。書き終わったかは札で見る）。
pub fn next_seq() -> u32 {
    SEQ.load(Ordering::Acquire)
}

/// 番号 `seq` の枠を、札・中身（1024 バイト全部）・札の順に読む。書き手は素のアセンブリなので、ここは原子変数で読む: 1 回目の札は
/// Acquire、中身の後に Acquire の fence（2 回目の札の読みが中身の前へ動かない）。いわゆる seqlock の読み方。
pub fn read_slot(seq: u32) -> SlotRead {
    let slot = &RING.slots[(seq & RING_MASK) as usize];
    let ticket = &slot.words[(OFF_TICKET / 4) as usize];
    let t1 = ticket.load(Ordering::Acquire);
    let mut w = Box::new([0u32; SLOT_WORDS]);
    for (x, a) in w.iter_mut().zip(slot.words.iter()) {
        *x = a.load(Ordering::Relaxed);
    }
    fence(Ordering::Acquire);
    let t2 = ticket.load(Ordering::Relaxed);
    match classify(seq, t1, t2) {
        SlotState::Complete => SlotRead::Record(Snap::from_words(seq, w)),
        SlotState::NotYet => SlotRead::NotYet,
        SlotState::Overwritten => SlotRead::Overwritten,
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    //! 本物の入口を検査のプロセスで呼ぶための道具（素のアセンブリ）。`seen_target` は元の飛び先の代わり（入った時のレジスタ・
    //! `[esp]`・esp・EFLAGS を `SEEN` に書き、何も変えずに `ret`）。`call_with_regs(target)` は各レジスタに決まった値を入れ、
    //! FPU の制御語を `TEST_CW` にして `target` を呼び、戻った直後のレジスタを `AFTER` に書く。`COPY_SRC` はコピーの表の元の代わり。
    use super::*;
    use std::sync::Mutex;

    /// 本物の入口の静的変数は 1 つしか無いので、本物の検査は 1 本ずつ走らせる。
    pub(crate) static REAL_HOOK_TESTS: Mutex<()> = Mutex::new(());

    pub(crate) const EFLAGS_DF: u32 = 1 << 10;
    /// EFLAGS の算術系の旗（CF bit0・PF bit2・AF bit4・ZF bit6・SF bit7・OF bit11）。描画の入口が `pushfd`／`popfd` で全部戻すかを見る。
    pub(crate) const EFLAGS_ARITH: u32 = 0x8D5;

    #[repr(C, align(16))]
    pub(crate) struct CopySrc(pub(crate) [AtomicU32; 64]);
    pub(crate) static COPY_SRC: CopySrc = CopySrc([const { AtomicU32::new(0) }; 64]);

    pub(crate) fn copy_src_addr() -> u32 {
        &COPY_SRC as *const CopySrc as usize as u32
    }

    pub(crate) fn copy_src_byte(off: u32) -> u8 {
        COPY_SRC.0[(off / 4) as usize]
            .load(Ordering::SeqCst)
            .to_le_bytes()[(off % 4) as usize]
    }

    pub(crate) const REG_EAX: u32 = 0x1111_1111;
    pub(crate) const REG_ECX: u32 = 0x2222_2222;
    pub(crate) const REG_EDX: u32 = 0x3333_3333;
    pub(crate) const REG_EBX: u32 = 0x4444_4444;
    pub(crate) const REG_EDI: u32 = 0x6666_6666;
    pub(crate) const REG_EBP: u32 = 0x7777_7777;
    /// 検査で入れる FPU の制御語（Windows の既定 0x027F と違う値）。
    pub(crate) const TEST_CW_VALUE: u32 = 0x007F;

    /// `SEEN` の並び: eax, ecx, edx, ebx, esi, edi, ebp, [esp], esp, EFLAGS。
    pub(crate) static SEEN: [AtomicU32; 10] = [const { AtomicU32::new(0) }; 10];
    pub(crate) static PRE_FLAGS: AtomicU32 = AtomicU32::new(0);
    /// `AFTER` の並び: eax, ecx, edx, ebx, esi, edi, ebp, esp。
    pub(crate) static AFTER: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
    pub(crate) static TARGET_HITS: AtomicU32 = AtomicU32::new(0);
    /// 呼ぶ直前の esp（呼ばれた側の esp はこれ - 4 のはず）と、戻りアドレスのはずの値（`call` の次）。
    pub(crate) static PRE_ESP: AtomicU32 = AtomicU32::new(0);
    pub(crate) static EXP_RET: AtomicU32 = AtomicU32::new(0);
    static CALL_TARGET: AtomicU32 = AtomicU32::new(0);
    static SAVED_CW: AtomicU32 = AtomicU32::new(0);
    pub(crate) static TEST_CW: AtomicU32 = AtomicU32::new(TEST_CW_VALUE);

    #[repr(C, align(16))]
    pub(crate) struct FakeThis(pub(crate) [AtomicU32; 0x600 / 4]);
    pub(crate) static FAKE_THIS: FakeThis = FakeThis([const { AtomicU32::new(0) }; 0x600 / 4]);
    #[repr(C, align(16))]
    pub(crate) struct FakeWindow(pub(crate) [AtomicU32; 0x48 / 4]);
    pub(crate) static FAKE_WINDOW: FakeWindow = FakeWindow([const { AtomicU32::new(0) }; 0x48 / 4]);

    pub(crate) fn fake_this_addr() -> u32 {
        &FAKE_THIS as *const FakeThis as usize as u32
    }
    pub(crate) fn fake_window_addr() -> u32 {
        &FAKE_WINDOW as *const FakeWindow as usize as u32
    }

    pub(crate) fn set_fake(frame: u32, cur: u32, req: u32, now: f64, deadline: f64) {
        let t = &FAKE_THIS.0;
        t[(SUP_FRAME / 4) as usize].store(frame, Ordering::SeqCst);
        t[(SUP_SCENE_CUR / 4) as usize].store(cur, Ordering::SeqCst);
        t[(SUP_SCENE_REQ / 4) as usize].store(req, Ordering::SeqCst);
        let w = &FAKE_WINDOW.0;
        for (off, v) in [(WINDOW_NOW, now), (WINDOW_DEADLINE, deadline)] {
            let b = v.to_bits();
            w[(off / 4) as usize].store(b as u32, Ordering::SeqCst);
            w[(off / 4 + 1) as usize].store((b >> 32) as u32, Ordering::SeqCst);
        }
    }

    // SAFETY（`#[unsafe(naked)]`）: 書くのは検査の静的変数（SEEN・TARGET_HITS）だけで、eax は一時に使った後で元の値へ戻す。
    // フラグは `lock inc` より先に `pushfd` で取る（入った時のフラグを全部見る）。`pushfd` と `pop` でスタックは戻る。`ret` は入った時の `[esp]` へ戻る。
    #[unsafe(naked)]
    pub(crate) extern "C" fn seen_target() {
        core::arch::naked_asm!(
            "pushfd",
            "pop dword ptr [{seen} + 36]",
            "lock inc dword ptr [{hits}]",
            "mov dword ptr [{seen}], eax",
            "mov dword ptr [{seen} + 4], ecx",
            "mov dword ptr [{seen} + 8], edx",
            "mov dword ptr [{seen} + 12], ebx",
            "mov dword ptr [{seen} + 16], esi",
            "mov dword ptr [{seen} + 20], edi",
            "mov dword ptr [{seen} + 24], ebp",
            "mov dword ptr [{seen} + 32], esp",
            "mov eax, dword ptr [esp]",
            "mov dword ptr [{seen} + 28], eax",
            "mov eax, dword ptr [{seen}]",
            "ret",
            hits = sym TARGET_HITS,
            seen = sym SEEN,
        )
    }

    /// 呼ぶ直前に入れる EFLAGS（CF・PF・AF・ZF・SF・OF を 1、bit 1 は常に 1、IF は 1 のまま、DF と TF は 0）。
    pub(crate) const TEST_FLAGS: u32 = 0x0000_0AD7;

    // SAFETY（`#[unsafe(naked)]`）: cdecl の関数（引数 1 つ `target` を `[esp + 4]` に受け、戻り値なし）。ebx・esi・edi・ebp を
    // push して最後に pop で戻す。FPU の制御語は SAVED_CW に取って戻す。EFLAGS は `TEST_FLAGS` にしてから呼ぶ。`target` は
    // 「引数なし・戻り値なしで `ret` する関数」でなければならない。
    #[unsafe(naked)]
    unsafe extern "C" fn call_with_regs_asm(_target: u32) {
        core::arch::naked_asm!(
            "push ebp",
            "push ebx",
            "push esi",
            "push edi",
            "mov eax, dword ptr [esp + 20]",
            "mov dword ptr [{call_target}], eax",
            "fnstcw word ptr [{saved_cw}]",
            "fldcw word ptr [{test_cw}]",
            "push {flags}",
            "popfd",
            "pushfd",
            "pop dword ptr [{pre_flags}]",
            "lea eax, [2f]",
            "mov dword ptr [{exp_ret}], eax",
            "mov dword ptr [{pre_esp}], esp",
            "mov eax, {r_eax}",
            "mov ecx, {r_ecx}",
            "mov edx, {r_edx}",
            "mov ebx, {r_ebx}",
            "lea esi, [{fake_this}]",
            "mov edi, {r_edi}",
            "mov ebp, {r_ebp}",
            "call dword ptr [{call_target}]",
            "2:",
            "mov dword ptr [{after}], eax",
            "mov dword ptr [{after} + 4], ecx",
            "mov dword ptr [{after} + 8], edx",
            "mov dword ptr [{after} + 12], ebx",
            "mov dword ptr [{after} + 16], esi",
            "mov dword ptr [{after} + 20], edi",
            "mov dword ptr [{after} + 24], ebp",
            "mov dword ptr [{after} + 28], esp",
            "fldcw word ptr [{saved_cw}]",
            "pop edi",
            "pop esi",
            "pop ebx",
            "pop ebp",
            "ret",
            call_target = sym CALL_TARGET,
            saved_cw = sym SAVED_CW,
            test_cw = sym TEST_CW,
            exp_ret = sym EXP_RET,
            pre_esp = sym PRE_ESP,
            pre_flags = sym PRE_FLAGS,
            after = sym AFTER,
            fake_this = sym FAKE_THIS,
            flags = const TEST_FLAGS,
            r_eax = const REG_EAX,
            r_ecx = const REG_ECX,
            r_edx = const REG_EDX,
            r_ebx = const REG_EBX,
            r_edi = const REG_EDI,
            r_ebp = const REG_EBP,
        )
    }

    pub(crate) fn call_with_regs(target: u32) {
        // SAFETY: `target` は呼び出し元（検査）が上の約束どおりに選ぶ。入口を呼ぶときは、元の飛び先が `seen_target` に置かれていて、
        // ウィンドウは 0 か `FAKE_WINDOW`、esi は `FAKE_THIS`（入口が読む `+0x590` まで入る大きさ）、コピーの表は 0 行か `COPY_SRC` の中を指す行だけ。RENDER_ONLY_EXIT は、入口を
        // 直に呼ぶときは 0（戻りアドレスを替えられるとここへ戻れない）、`fake_on_tick` を呼ぶときは 0 か `fake_exit`。
        unsafe { call_with_regs_asm(target) }
    }

    pub(crate) fn seen_target_addr() -> u32 {
        seen_target as *const () as usize as u32
    }

    // SAFETY（`#[unsafe(naked)]`）: on_tick `0x00431110` の先頭と、入口の call の次の真似。`push ebx; push esi` で call の時点の
    // スタックを [戻りアドレス][esi][ebx] にして入口を呼ぶ。入口が元の飛び先（`seen_target`）へ飛べば、それが `ret` でここへ戻り、1 を
    // 返す。入口が戻りアドレスを出口（`fake_exit`）へ替えれば、そちらが esi・ebx を pop して 3 を返す。どちらでも、積んだ 2 つは pop で
    // 戻り、`ret` は呼んだ所へ戻る。
    #[unsafe(naked)]
    pub(crate) extern "C" fn fake_on_tick() {
        core::arch::naked_asm!(
            "push ebx",
            "push esi",
            "call {entry}",
            "mov eax, 1",
            "pop esi",
            "pop ebx",
            "ret",
            entry = sym super::input_wait_entry,
        )
    }

    // SAFETY（`#[unsafe(naked)]`）: 出口 `0x00431259`（`5E B8 03 00 00 00 5B C3`）の真似。入口が `fake_on_tick` の戻りアドレスを替えて
    // ここへ `ret` したとき、スタックの頂上は `fake_on_tick` が積んだ esi・ebx と、その呼び出し元への戻りアドレス。
    #[unsafe(naked)]
    pub(crate) extern "C" fn fake_exit() {
        core::arch::naked_asm!("pop esi", "mov eax, 3", "pop ebx", "ret")
    }

    pub(crate) fn fake_on_tick_addr() -> u32 {
        fake_on_tick as *const () as usize as u32
    }

    pub(crate) fn fake_exit_addr() -> u32 {
        fake_exit as *const () as usize as u32
    }

    pub(crate) fn load(a: &[AtomicU32]) -> Vec<u32> {
        a.iter().map(|x| x.load(Ordering::SeqCst)).collect()
    }
}

#[cfg(test)]
mod tests {
    //! 本物の入口を、検査のプロセス自身で呼ぶ（花映塚は使わない）。
    use super::testkit::*;
    use super::*;
    use core::sync::atomic::AtomicBool;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    const POISON: u32 = 0xA5A5_A5A5;
    const POISON_BYTE: u8 = 0xA5;

    fn poison_slot(seq: u32) {
        let slot = &RING.slots[(seq & RING_MASK) as usize];
        for w in slot.words.iter() {
            w.store(POISON, Ordering::SeqCst);
        }
    }

    fn ticket_of(seq: u32) -> u32 {
        RING.slots[(seq & RING_MASK) as usize].words[(OFF_TICKET / 4) as usize]
            .load(Ordering::SeqCst)
    }

    fn snap_of(seq: u32) -> Snap {
        match read_slot(seq) {
            SlotRead::Record(s) => s,
            other => panic!("seq {seq} が採れない: {other:?}"),
        }
    }

    fn prepare(window: u32) {
        set_original_target(seen_target_addr());
        set_window(window);
        assert!(set_copy_table(&[]));
        TEST_CW.store(TEST_CW_VALUE, Ordering::SeqCst);
        set_render_only_exit(0);
        RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
        crate::script_win::stop();
        crate::shot_win::set_off();
    }

    fn fill_copy_src(salt: u32) {
        for (i, w) in COPY_SRC.0.iter().enumerate() {
            let b = |k: u32| ((i as u32 * 4 + k) * 7 + salt) & 0x7F;
            w.store(b(0) | b(1) << 8 | b(2) << 16 | b(3) << 24, Ordering::SeqCst);
        }
    }

    /// 検査のコピーの表: 4 バイト境界の元、4 バイト境界に無い元（+0x46 = 2 余り。InputManager の 2 本目と同じずれ）、奇数のアドレスの元で枠の終わりちょうどまで。
    fn test_table() -> [CopyRow; 3] {
        let s = copy_src_addr();
        [
            CopyRow {
                src: s,
                len: 20,
                dst: 64,
            },
            CopyRow {
                src: s + 0x46,
                len: 0x38,
                dst: 100,
            },
            CopyRow {
                src: s + 0x81,
                len: 7,
                dst: SLOT_BYTES - 7,
            },
        ]
    }

    #[test]
    fn entry_passes_registers_and_return_address_through() {
        let _g = lock();
        for rows in [&[][..], &test_table()[..]] {
            prepare(fake_window_addr());
            assert!(set_copy_table(rows));
            assert_eq!(copy_table(), rows, "表を置けていない");
            fill_copy_src(3);
            set_fake(1, 2, 3, 1.0, 2.0);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            let calls0 = calls();
            call_with_regs(entry_address());
            let want = [
                REG_EAX,
                REG_ECX,
                REG_EDX,
                REG_EBX,
                fake_this_addr(),
                REG_EDI,
                REG_EBP,
            ];
            let n = rows.len();
            let seen = load(&SEEN);
            assert_eq!(seen[..7], want[..], "{n} 行: 元の飛び先が見たレジスタ");
            assert_eq!(
                seen[7],
                EXP_RET.load(Ordering::SeqCst),
                "{n} 行: 元の飛び先が見た戻りアドレス"
            );
            assert_eq!(
                seen[8],
                PRE_ESP.load(Ordering::SeqCst) - 4,
                "{n} 行: 元の飛び先が見た esp（呼ぶ前 - 戻りアドレスの 4）"
            );
            let pre = PRE_FLAGS.load(Ordering::SeqCst);
            assert_eq!(
                pre & EFLAGS_DF,
                0,
                "呼ぶ前の DF は 0 のはず（呼び出し規約）"
            );
            assert_eq!(
                seen[9] & EFLAGS_DF,
                pre & EFLAGS_DF,
                "{n} 行: 元の飛び先が見た DF"
            );
            assert_eq!(load(&AFTER)[..7], want[..], "{n} 行: 戻った後のレジスタ");
            assert_eq!(
                TARGET_HITS.load(Ordering::SeqCst),
                hits0 + 1,
                "{n} 行: 元の飛び先へ飛んでいない"
            );
            assert_eq!(calls(), calls0 + 1, "{n} 行: 回数を数えていない");
        }
        assert!(set_copy_table(&[]));
    }

    #[test]
    fn entry_copies_the_table_byte_for_byte() {
        let _g = lock();
        prepare(fake_window_addr());
        let rows = test_table();
        assert!(set_copy_table(&rows));
        fill_copy_src(11);
        let s0 = next_seq();
        poison_slot(s0);
        set_fake(5, 6, 7, 1.0, 2.0);
        call_with_regs(entry_address());
        let s = snap_of(s0);
        let base = copy_src_addr();
        let mut covered = vec![false; SLOT_BYTES as usize];
        let mut copied = 0;
        for r in &rows {
            for k in 0..r.len {
                let want = copy_src_byte(r.src - base + k);
                assert_ne!(want, POISON_BYTE);
                assert_eq!(
                    s.byte(r.dst + k),
                    Some(want),
                    "元 0x{:08X} の {k} バイト目 → 枠 +{}",
                    r.src,
                    r.dst + k
                );
                covered[(r.dst + k) as usize] = true;
                copied += 1;
            }
        }
        assert_eq!(copied, 20 + 0x38 + 7);
        assert!(!s.render_only_mark(), "普通の件で印 +52 が 1");
        assert_eq!(s.read(OFF_RENDER_ONLY, 4), Some(0), "印 +52 が 0 でない");
        let untouched = (OFF_RENDER_ONLY + 4..SLOT_BYTES).filter(|&p| !covered[p as usize]);
        let mut n = 0;
        for p in untouched {
            assert_eq!(s.byte(p), Some(POISON_BYTE), "枠 +{p} に書いた");
            n += 1;
        }
        assert_eq!(n, SLOT_BYTES - (OFF_RENDER_ONLY + 4) - copied);
        assert!(set_copy_table(&[]));
    }

    #[test]
    fn zero_rows_copy_nothing() {
        let _g = lock();
        prepare(fake_window_addr());
        assert!(set_copy_table(&test_table()));
        assert!(set_copy_table(&[]));
        assert_eq!(COPY_COUNT.load(Ordering::SeqCst), 0);
        fill_copy_src(1);
        let s0 = next_seq();
        poison_slot(s0);
        call_with_regs(entry_address());
        let s = snap_of(s0);
        assert_eq!(s.read(OFF_RENDER_ONLY, 4), Some(0), "印 +52 が 0 でない");
        for p in OFF_RENDER_ONLY + 4..SLOT_BYTES {
            assert_eq!(s.byte(p), Some(POISON_BYTE), "枠 +{p} に書いた");
        }
        assert_eq!(s.rec.esi, fake_this_addr(), "頭は書かれている");
    }

    #[test]
    fn set_copy_table_refuses_a_bad_layout() {
        let _g = lock();
        let good = test_table();
        assert!(set_copy_table(&good));
        let mut out = good;
        out[2].len = 8; // 枠の終わりを 1 バイト越える
        assert!(!set_copy_table(&out));
        assert_eq!(
            COPY_COUNT.load(Ordering::SeqCst),
            0,
            "断ったのに件数が残った"
        );
        let mut head = good;
        head[0].dst = 60;
        assert!(!set_copy_table(&head));
        let mut overlap = good;
        overlap[1].dst = 80;
        assert!(!set_copy_table(&overlap));
        assert!(copy_table().is_empty());
        assert!(set_copy_table(&good));
        assert_eq!(copy_table(), good);
        assert!(set_copy_table(&[]));
    }

    #[test]
    fn entry_writes_one_slot_per_call() {
        let _g = lock();
        prepare(fake_window_addr());
        let s0 = next_seq();
        poison_slot(s0);
        poison_slot(s0.wrapping_add(1));
        set_fake(1000, 5, 6, 12.5, 12.5 + 1.0 / 60.0);
        call_with_regs(entry_address());
        set_fake(1001, 6, 6, 12.75, 12.75 + 1.0 / 60.0);
        call_with_regs(entry_address());
        assert_eq!(next_seq(), s0.wrapping_add(2), "番号が 1 回に 1 つ進まない");
        assert_eq!(ticket_of(s0), s0.wrapping_add(1), "札");
        assert_eq!(ticket_of(s0.wrapping_add(1)), s0.wrapping_add(2), "札");
        let a = snap_of(s0).rec;
        let b = snap_of(s0.wrapping_add(1)).rec;
        assert_ne!(a.tsc, 0);
        assert!(b.tsc > a.tsc, "TSC が進まない: {} → {}", a.tsc, b.tsc);
        // SAFETY: 引数の無い問い合わせ。
        let tid = unsafe { GetCurrentThreadId() };
        for (r, frame, cur, req, now) in [(a, 1000, 5, 6, 12.5), (b, 1001, 6, 6, 12.75)] {
            assert_eq!(r.game_now(), now);
            assert_eq!(r.game_deadline(), now + 1.0 / 60.0);
            assert_eq!((r.sup_frame, r.scene_cur, r.scene_req), (frame, cur, req));
            assert_eq!(r.esi, fake_this_addr());
            assert_eq!(r.tid, tid, "スレッド ID");
            assert_eq!(r.fpucw, TEST_CW_VALUE, "FPU の制御語（上位 2 バイトは 0）");
        }
        let s = snap_of(s0);
        assert_eq!(s.read(OFF_RENDER_ONLY, 4), Some(0), "印 +52 が 0 でない");
        for p in OFF_RENDER_ONLY + 4..crate::ticks::HEAD_BYTES {
            assert_eq!(s.byte(p), Some(POISON_BYTE), "予備 +{p} に書いた");
        }
    }

    #[test]
    fn resim_ticks_write_no_slot_and_skip_the_count() {
        use crate::rollback_play_win::RESIM_TICK;
        let _g = lock();
        prepare(fake_window_addr());
        set_fake(500, 2, 2, 3.0, 3.0 + 1.0 / 60.0);
        let (s0, c0) = (next_seq(), calls());
        poison_slot(s0);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        RESIM_TICK.store(1, Ordering::SeqCst);
        call_with_regs(entry_address());
        RESIM_TICK.store(0, Ordering::SeqCst);
        assert_eq!(next_seq(), s0, "進め直しの回が番号を進めた");
        assert_eq!(calls(), c0, "進め直しの回を数えた");
        assert_eq!(ticket_of(s0), POISON, "進め直しの回が枠に書いた");
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0 + 1,
            "台本の分岐が閉じているのに元の飛び先へ渡さなかった"
        );
        // 対照: 進め直しでない回は今までどおり 1 件書く。
        call_with_regs(entry_address());
        assert_eq!(next_seq(), s0.wrapping_add(1), "普通の回が番号を進めない");
        assert_eq!(calls(), c0 + 1);
        assert_eq!(ticket_of(s0), s0.wrapping_add(1));
    }

    #[test]
    fn resim_ticks_with_script_on_feed_and_return_without_recording() {
        use crate::rollback_play_win::RESIM_TICK;
        use crate::script_win::{FORCE_RET, SCRIPT_ON, arm, start, stop, tests as sw};
        let _g = lock();
        prepare(fake_window_addr());
        arm(
            higanr_core::script::parse("scene=3 wait=100 p1=Z\n").unwrap(),
            sw::fake_addrs(),
            sw::feed_fn_addr(),
            sw::local_fn_addr(),
        )
        .unwrap();
        start().unwrap();
        assert_ne!(SCRIPT_ON.load(Ordering::SeqCst), 0, "分岐が開いていない");
        set_fake(500, 3, 3, 3.0, 3.0 + 1.0 / 60.0);
        let (s0, c0) = (next_seq(), calls());
        poison_slot(s0);
        let hits0 = TARGET_HITS.load(Ordering::SeqCst);
        RESIM_TICK.store(1, Ordering::SeqCst);
        FORCE_RET.store(2, Ordering::SeqCst); // 台本の戻り値 1（feed した）
        call_with_regs(entry_address());
        RESIM_TICK.store(0, Ordering::SeqCst);
        assert_eq!(
            FORCE_RET.load(Ordering::SeqCst),
            0,
            "台本の分岐を通っていない"
        );
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0,
            "feed したのに元の飛び先へ飛んだ"
        );
        assert_eq!(next_seq(), s0, "進め直しの回が番号を進めた");
        assert_eq!(calls(), c0, "進め直しの回を数えた");
        assert_eq!(ticket_of(s0), POISON, "進め直しの回が枠に書いた");
        // 対照: 進め直しでない回は 1 件書く（同じ戻り値でも元の飛び先へは飛ばない）。
        FORCE_RET.store(2, Ordering::SeqCst);
        call_with_regs(entry_address());
        assert_eq!(next_seq(), s0.wrapping_add(1), "普通の回が番号を進めない");
        assert_eq!(calls(), c0 + 1);
        assert_eq!(ticket_of(s0), s0.wrapping_add(1));
        assert_eq!(TARGET_HITS.load(Ordering::SeqCst), hits0);
        stop();
    }

    #[test]
    fn zero_window_is_not_read_and_writes_zero() {
        let _g = lock();
        prepare(0);
        let s0 = next_seq();
        poison_slot(s0);
        set_fake(77, 1, 1, 3.0, 4.0);
        call_with_regs(entry_address());
        let r = snap_of(s0).rec;
        assert_eq!((r.now_bits, r.deadline_bits), (0, 0));
        assert_eq!(r.sup_frame, 77);
        assert_eq!(r.esi, fake_this_addr());
    }

    #[test]
    fn render_only_branch_returns_three_and_marks() {
        let _g = lock();
        prepare(0);
        set_render_only_exit(fake_exit_addr());
        assert_eq!(
            RENDER_ONLY_EXIT.load(Ordering::SeqCst),
            fake_exit_addr(),
            "出口のアドレスを置けていない"
        );
        assert!(try_start_render_only(1), "0 から 1 に置ける");
        assert_eq!(render_only_left(), 1);
        set_fake(500, 1, 1, 3.0, 4.0);
        let s0 = next_seq();
        poison_slot(s0);
        let calls0 = calls();
        call_with_regs(fake_on_tick_addr());
        assert_eq!(load(&AFTER)[0], 3, "on_tick が 3 を返していない");
        assert_eq!(render_only_left(), 0, "RENDER_ONLY_LEFT を減らしていない");
        let s = snap_of(s0);
        assert!(s.render_only_mark(), "印 +52 が 1 でない");
        assert_eq!(s.rec.sup_frame, 500, "頭（this の欄）が書かれていない");
        assert_eq!(calls(), calls0 + 1, "回数を数えていない");
        let after = load(&AFTER);
        assert_eq!(after[3], REG_EBX, "ebx を保っていない");
        assert_eq!(after[4], fake_this_addr(), "esi を保っていない");
        assert_eq!(after[6], REG_EBP, "ebp を保っていない");
        assert_eq!(
            after[7],
            PRE_ESP.load(Ordering::SeqCst),
            "esp を保っていない"
        );
        set_render_only_exit(0);
    }

    #[test]
    fn match_entry_passes_everything_through() {
        let _g = lock();
        prepare(0);
        set_match_original(seen_target_addr());
        for net_on in [0u32, 1] {
            crate::script_win::NET_ON.store(net_on, Ordering::SeqCst);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            let calls0 = MATCH_CALLS.load(Ordering::SeqCst);
            let foreign0 = crate::script_win::MATCH_FOREIGN.load(Ordering::SeqCst);
            let on0 = crate::script_win::MATCH_ON_CALLS.load(Ordering::SeqCst);
            call_with_regs(match_entry_address());
            let want = [
                REG_EAX,
                REG_ECX,
                REG_EDX,
                REG_EBX,
                fake_this_addr(),
                REG_EDI,
                REG_EBP,
            ];
            let seen = load(&SEEN);
            assert_eq!(
                seen[..7],
                want[..],
                "NET_ON {net_on}: 元の飛び先が見たレジスタ"
            );
            assert_eq!(
                seen[7],
                EXP_RET.load(Ordering::SeqCst),
                "NET_ON {net_on}: 戻りアドレス"
            );
            assert_eq!(
                seen[8],
                PRE_ESP.load(Ordering::SeqCst) - 4,
                "NET_ON {net_on}: esp"
            );
            assert_eq!(
                load(&AFTER)[..7],
                want[..],
                "NET_ON {net_on}: 戻った後のレジスタ"
            );
            assert_eq!(TARGET_HITS.load(Ordering::SeqCst), hits0 + 1);
            assert_eq!(MATCH_CALLS.load(Ordering::SeqCst), calls0 + 1, "回数");
            assert_eq!(
                crate::script_win::MATCH_ON_CALLS.load(Ordering::SeqCst) - on0,
                net_on,
                "NET_ON {net_on}: Rust を呼んだ回数"
            );
            assert_eq!(
                crate::script_win::MATCH_FOREIGN.load(Ordering::SeqCst) - foreign0,
                net_on,
                "NET_ON {net_on}: ゲームのスレッドでないと数えた回数"
            );
        }
        crate::script_win::NET_ON.store(0, Ordering::SeqCst);
    }

    #[test]
    fn no_render_only_when_left_zero_or_exit_zero() {
        let _g = lock();
        prepare(0);
        set_render_only_exit(fake_exit_addr());
        assert_eq!(render_only_left(), 0);
        let s0 = next_seq();
        poison_slot(s0);
        set_fake(1, 1, 1, 1.0, 2.0);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(load(&AFTER)[0], 1, "残り 0 なのに 3 を返した");
        assert!(
            !snap_of(s0).render_only_mark(),
            "残り 0 なのに印を 1 にした"
        );
        set_render_only_exit(0);
        assert!(try_start_render_only(3));
        let s1 = next_seq();
        poison_slot(s1);
        call_with_regs(fake_on_tick_addr());
        assert_eq!(load(&AFTER)[0], 1, "出口 0 なのに 3 を返した");
        assert!(
            !snap_of(s1).render_only_mark(),
            "出口 0 なのに印を 1 にした"
        );
        assert_eq!(render_only_left(), 3, "出口 0 なのに残りを減らした");
        RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn draw_after_tick_carries_draw_tag_after_tick() {
        use crate::draw::{
            DrawRead, draw_entry_address, draw_next_seq, read_draw_slot, set_draw_trampoline,
        };
        use crate::render_only::draw_tag_after_tick;
        let _g = lock();
        set_draw_trampoline(seen_target_addr());
        for render_only in [false, true] {
            prepare(0);
            if render_only {
                set_render_only_exit(fake_exit_addr());
                assert!(try_start_render_only(1));
            }
            set_fake(7, 1, 1, 1.0, 2.0);
            let k = next_seq();
            call_with_regs(fake_on_tick_addr());
            let want = if render_only { 3 } else { 1 };
            assert_eq!(
                load(&AFTER)[0],
                want,
                "通った経路が違う（render_only={render_only}）"
            );
            assert_eq!(snap_of(k).render_only_mark(), render_only);

            let d = draw_next_seq();
            call_with_regs(draw_entry_address());
            let rec = match read_draw_slot(d) {
                DrawRead::Record(r) => r,
                other => panic!("描画の記録が採れない: {other:?}"),
            };
            assert_eq!(
                rec.tick_seq,
                draw_tag_after_tick(k),
                "描画が書いた番号が draw_tag_after_tick(k) と違う（render_only={render_only}）"
            );
            assert_ne!(
                rec.tick_seq, k,
                "tick の番号 k そのままで引ける形になっている"
            );
        }
        set_render_only_exit(0);
        RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
        set_draw_trampoline(0);
    }

    #[test]
    fn try_start_render_only_only_when_zero() {
        let _g = lock();
        RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
        assert!(try_start_render_only(5));
        assert!(!try_start_render_only(2), "残りがあるのに置けた");
        assert_eq!(render_only_left(), 5);
        RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
        assert!(try_start_render_only(2));
        assert_eq!(render_only_left(), 2);
        RENDER_ONLY_LEFT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn read_slot_follows_the_ticket() {
        let _g = lock();
        let seq = next_seq().wrapping_add(100);
        let slot = &RING.slots[(seq & RING_MASK) as usize];
        let t = &slot.words[(OFF_TICKET / 4) as usize];
        let before = t.load(Ordering::SeqCst);
        slot.words[(OFF_SUP_FRAME / 4) as usize].store(4242, Ordering::SeqCst);
        t.store(seq.wrapping_add(1), Ordering::SeqCst);
        assert!(
            matches!(read_slot(seq), SlotRead::Record(s) if s.rec.sup_frame == 4242 && s.rec.seq == seq)
        );
        t.store(0, Ordering::SeqCst);
        assert_eq!(read_slot(seq), SlotRead::NotYet);
        t.store(seq.wrapping_add(1).wrapping_add(RING_LEN), Ordering::SeqCst);
        assert_eq!(read_slot(seq), SlotRead::Overwritten);
        t.store(before, Ordering::SeqCst);
    }

    #[test]
    fn concurrent_reader_never_takes_a_torn_slot() {
        let _g = lock();
        prepare(fake_window_addr());
        let src = copy_src_addr();
        let rows = [
            CopyRow {
                src,
                len: 256,
                dst: 64,
            },
            CopyRow {
                src: src + 2,
                len: 0x38,
                dst: 700,
            },
        ];
        assert!(set_copy_table(&rows));
        let copy_expect =
            |seq: u32| -> [u32; 4] { [seq, seq.wrapping_mul(3), seq ^ 0xC3C3_C3C3, !seq] };
        let copy_seen = |s: &Snap| -> Option<[u32; 5]> {
            Some([
                s.read(64, 4)?,
                s.read(702, 4)?,
                s.read(64 + 128, 4)?,
                s.read(64 + 252, 4)?,
                s.read(700, 2)?,
            ])
        };
        const CALLS_N: u32 = 1_500_000;
        let base = next_seq();
        let stop = AtomicBool::new(false);
        let checked = AtomicU32::new(0);
        let torn = AtomicU32::new(0);
        let expect = |seq: u32| -> (u32, u32, u32, u64) {
            (
                seq,
                seq ^ 0x5A5A_5A5A,
                !seq,
                (seq as u64) << 32 | (seq ^ 0x1234_5678) as u64,
            )
        };
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let end = next_seq();
                    for seq in [
                        end.wrapping_sub(1),
                        end.wrapping_sub(RING_LEN),
                        end.wrapping_sub(RING_LEN + 1),
                    ] {
                        if seq.wrapping_sub(base) >= CALLS_N {
                            continue; // この検査の前の件
                        }
                        if let SlotRead::Record(s) = read_slot(seq) {
                            checked.fetch_add(1, Ordering::Relaxed);
                            let r = &s.rec;
                            let (f, c, q, now) = expect(seq);
                            let [w0, w1, w32, w63] = copy_expect(seq);
                            if (r.sup_frame, r.scene_cur, r.scene_req, r.now_bits) != (f, c, q, now)
                                || copy_seen(&s) != Some([w0, w1, w32, w63, w0 >> 16])
                            {
                                torn.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            });
            struct StopOnDrop<'a>(&'a AtomicBool);
            impl Drop for StopOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _stop_guard = StopOnDrop(&stop);
            for _ in 0..CALLS_N {
                let seq = next_seq();
                let (f, c, q, now) = expect(seq);
                FAKE_THIS.0[(SUP_FRAME / 4) as usize].store(f, Ordering::Relaxed);
                FAKE_THIS.0[(SUP_SCENE_CUR / 4) as usize].store(c, Ordering::Relaxed);
                FAKE_THIS.0[(SUP_SCENE_REQ / 4) as usize].store(q, Ordering::Relaxed);
                FAKE_WINDOW.0[(WINDOW_NOW / 4) as usize].store(now as u32, Ordering::Relaxed);
                FAKE_WINDOW.0[(WINDOW_NOW / 4 + 1) as usize]
                    .store((now >> 32) as u32, Ordering::Relaxed);
                let [w0, w1, w32, w63] = copy_expect(seq);
                COPY_SRC.0[0].store(w0, Ordering::Relaxed);
                COPY_SRC.0[1].store(w1, Ordering::Relaxed);
                COPY_SRC.0[32].store(w32, Ordering::Relaxed);
                COPY_SRC.0[63].store(w63, Ordering::Relaxed);
                call_with_regs(entry_address());
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert!(set_copy_table(&[]));
        let (n, bad) = (checked.load(Ordering::SeqCst), torn.load(Ordering::SeqCst));
        eprintln!("並列の読み（1024 バイトの枠・コピー 2 行）: 採った {n} 件、破れた枠 {bad} 件");
        assert!(
            n > 1000,
            "読み手が採った件が少なすぎる（{n} 件。測れていない）"
        );
        assert_eq!(bad, 0, "破れた枠を採った（{bad} 件 / {n} 件）");
    }
}
