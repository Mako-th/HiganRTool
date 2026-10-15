//! rollback のモードの製品の経路の部品（pure。unsafe 無し。Win32・ゲームのメモリ・入口のアセンブリは `rollback_play_win.rs` と `rollback_win.rs`）。
//!
//! 中身は 3 つ:
//! - [`GameWindow`]: 本物の花映塚のための `higanr_core::rollback::Window`。コピー（決まった塊・`.data` の範囲・DLL の領域の使った所・
//!   更新の列のつなぎ（レジストリの先頭の 2 つの節 0x40 B））と、コピーのときにバックアップする Supervisor の `+0x58C` / `+0x590`、書き戻し、
//!   更新だけを 1 回回す tick（走査を 1 回呼ぶ形）、進め直しの前後の SE・BGM・文字の数のバックアップと戻し。
//! - 入口の手順 [`scan_point_with`] / [`post_present_with`]: 状態機械を 1 フレームぶん進める。状態機械は [`Machine`] のインターフェースの向こう
//!   （本物は `netplay/rollback.rs` の `NpMachine`、検査は `Rollback` そのもの）。
//! - 設置の手順 [`install_play`]: 確保・解放の 3 入口 ＋ calloc ＋ 走査の call を、期待バイト列が全部合ったときだけ書く。
//!
//! ## 入力の地点をどこに置くか
//!
//! 状態機械の `input_point` は、中で `Window::tick` を呼んでその場で結果（場面の要求が立ったか）を受け取り、止まっている間は書き戻しと
//! 進め直し（`tick` を何回も）も同じ呼び出しの中で行う。花映塚の入力の地点 `0x0043112D` は更新の走査の中（優先度 0 の Supervisor::on_tick の先頭）
//! なので、そこから走査を呼び直すと走査に再入するうえ、その tick の残り（優先度 1〜26）は入口から戻った後に走るので結果をその場で返せない。
//! そこで rollback のモードでは、状態機械の入力の地点を do_frame の走査の call `0x0042DCA9` の包み（走査の直前・走査の外）に置く。
//! 1 回の tick = 「feed する語を置いて走査を 1 回呼ぶ」で、走査の中の入力の地点（`0x0043112D`）は置かれた語を feed するだけ（[`Order`]）。
//! 待つフレームは、置く語を「描画だけ」にして走査を 1 回呼ぶ（入力の地点が描画だけの出口 `0x431259` へ抜ける）。走査の call と入力の地点の間に
//! あるのは走査の臨界区間と Supervisor の先頭（`0x431116` の `+0x478 = 0` と `0x43111C` の NetworkManager の確かめ）だけで、同じフレームの数 µs 前（推定）。
//!
//! ## 境目（`TickReport::boundary`）
//!
//! - その tick の後に次の場面の要求が立っている（Supervisor `+0x590` ≠ `+0x58C`。読めなければ立っている側）。
//! - 走査が 0 か負の値を返した（異常 —— `rollback_resim::scan_ret_class`）。1（一時停止の tick を含む）は普通。
//! - 入力の地点が置いた入力ワードを受け取らなかった（読み込みの枝など。その tick は自分の入力ワードで進んでいない）。
//! - 確定していない tick で更新の列・描画の列の並びが変わった（ラウンドの終わりの ScreenEffect の登録など）。確定した tick の並びの変化は
//!   境目にしない（ラウンドの終わりは場面の終わりではない）。書き戻しの後は並びがコピーのときと同じかを確かめ、違えば食い違い
//!   （[`Fault::ListNotRestored`]。コピーの外の節のつなぎが戻らなかった形）で止める。
//!
//! ## 遅延
//!
//! delay 方式で走る間、この層は何もしない（確保の向け先と 0 埋めは [`redirect_wanted`] が rollback のモードのときだけ開く）。入口は対戦者なら
//! 望みによらず書く（[`install_wanted`]）ので、delay 方式で走る間も確保 1 回あたり `cmp` / `je` / `jmp` の 3 命令と走査の包み 1 段が残る。
//! rollback のモードでは、入力 → Present の間に足すのは走査の包み 1 段と、tick ごとの列の並びの読み（2 回）と場面の要求の読み。
//! コピー・書き戻し・進め直しは Present の後。
//!
//! ゲームのスレッドで走るので、確保・ロック待ち・パニックしうる添字を置かない（バックアップの列は固定長、表は固定長の `Layout`）。
#![forbid(unsafe_code)]

use std::io::Write;

use higanr_core::frame::is_prefill;
use higanr_core::input_model::MATCH_SEND_MASK;
use higanr_core::lockstep::CaptureError;
use higanr_core::patch::AddrRange;
use higanr_core::render_reset::{self, Field as ResetField, Refused};
use higanr_core::rollback::{
    After, CheckValues, Mode, Step, Tick, TickKind, TickReport, Wait, Window,
};
use higanr_core::wire::Check;
// 検査のインターフェース（`RbMachine`・`scan_point`・`post_present`）だけが使う。
#[cfg(test)]
use higanr_core::rollback::Rollback;
#[cfg(test)]
use higanr_core::wire::Input;

use crate::alloc_count::{AllocHost, AllocSettings, SITE_COUNT, SiteDetourEnv};
use crate::detour::{self, DetourEnv};
use crate::hook;
use crate::log_mode::detail_out;
use crate::net_game::{CheckRead, read_check_values};
use crate::rollback_probe::{
    Layout, LayoutError, LayoutIo, MAX_REGIONS, Region, RegionKind, Tables, fill_layout,
    vm_array_shape,
};
use crate::rollback_resim::{
    FullOutputs, HitSe, OutputMem, ScanRet, SePending, SePlayed, SeQueue, SeStats, capture_full,
    clear_se_queue, drop_pending_se, flush_se, hold_hit_se, note_live_hit, note_live_se,
    output_addr_allowed, output_addr_readable, read_se_queue, restore_full, scan_ret_class,
    se_summary_line, sort_resim_se,
};
use crate::sites::{SiteDef, check_table, hex_bytes};
use crate::th09_addr::{
    ALLOC_SITES, BG_VM_MAX, CALLOC_SITES, FPS_DRAWS, FPS_SHOWN, RENDER_RESETS, RESIM_SITES,
    ROLLBACK_VM_ARRAYS, SCREEN_EFFECT_BYTES, SCREEN_EFFECT_DRAW_NODE, SCREEN_EFFECT_UPDATE_NODE,
    STAGE_BLOCKS, STAGE_TINT, STAGE_TINT_MODE, SUP_SCENE_CUR, SUP_SCENE_REQ, SUPERVISOR,
    UPDATE_ARG, UPDATE_DRAW_HEAD, UPDATE_FN, UPDATE_NEXT, UPDATE_REGISTRY, VM_CLOCK, VM_CUR_INSTR,
    is_screen_effect_fn,
};

/// マッチの中のゲームのスレッドの確保を DLL の領域へ向けるか。rollback のモードで、状態機械がマッチを動かしている間だけ。0 埋めは領域の中の解放に
/// だけ起き、領域へ配るのはこれが開いている間だけなので、このガードが閉じていれば 0 埋めも起きない。
pub fn redirect_wanted(mode: Mode, controlled: bool) -> bool {
    mode == Mode::Rollback && controlled
}

/// 確保・解放・calloc・走査の call の入口を書くか。呼び手が `Mode::Rollback` を渡した実行だけ。対戦者は望みによらず `Rollback` を渡す
/// （キャラセレクトの Tab で rollback へ替えられるように。入れなければ後から rollback へは行けない）。観戦者の delay 方式の実行だけ `Delay` を
/// 渡して 1 バイトも書かない。入口は書いてあるが [`redirect_wanted`] が閉じたままの接続では、確保 1 回あたり `cmp` / `je` / `jmp` の 3 命令と
/// 走査の包み 1 段だけが残る（遅延の増分）。
pub fn install_wanted(wish: Mode) -> bool {
    wish == Mode::Rollback
}

/// 走査の中の入力の地点に置く指図（[`PlayMem::run_scan`] が置き、入力の地点の入口が 1 回だけ取る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// この (p1, p2) を 3 本へ feed する（0 に p1・1 に p2・2 に p1|p2。台本の経路と同じ順）。ローカル入力は読まない。
    Feed { p1: u16, p2: u16 },
    /// 描画だけのフレーム（入力の地点が描画だけの出口へ抜ける）。
    RenderOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanRun {
    pub ret: i32,
    /// 入力の地点が指図を取ったか（取らなければ、その tick は置いた語で進んでいない）。
    pub consumed: bool,
}

/// 見張りの数（`rollback_win` の `RB_OUT_*`。どれも増えるだけで、差で使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutCounts {
    /// DLL の領域へ向けなかったゲームのスレッドの確保（malloc・operator new・calloc）。
    pub allocs: u32,
    /// 領域の外の塊の解放のうち、見張りの間に生まれた塊でないもの。
    pub frees_old: u32,
    /// 領域の外の塊の解放のうち、見張りの間に生まれた塊。
    pub frees_born: u32,
    /// `frees_old` のうち、CRT へ渡さなかった解放（`set_hold_frees` が開いている間）。
    pub frees_held: u32,
}

/// ゲームの側（本物は `rollback_play_win::RealPlayMem`、検査は偽物）。どれも確保しない・パニックしない。
pub trait PlayMem {
    fn read_u32(&mut self, addr: u32) -> Option<u32>;
    /// 4 バイト書く。本物は Supervisor の `+0x58C` / `+0x590` と `rollback_resim::OUTPUT_RANGES` の中だけを書き、ほかは断る。
    fn write_u32(&mut self, addr: u32, v: u32) -> bool;
    fn check_rw(&mut self, addr: u32, len: u32) -> bool;
    fn arena_top(&mut self) -> u32;
    /// 範囲をコピーの置き場へコピーする（割り当て器の状態を含む。`Layout` は `fill_layout` がアドレスを埋めたもの）。
    fn copy_out(&mut self, l: &Layout);
    /// コピーの置き場から書き戻す（領域は `[0, コピーしたときの top)` を戻し、その先を今の top まで 0 に。割り当て器の状態も）。
    fn copy_in(&mut self, l: &Layout);
    /// コピーの置き場の中の範囲 `r` の先頭から、`fields` の欄を描画の後の値に置き換える（コピーの中だけ。ゲームのメモリには書かない）。
    /// 戻り値は置き換える前の値が違っていた欄の数（`higanr_core::render_reset::apply`）。
    fn reset_in_snap(&mut self, r: &Region, fields: &[ResetField<'_>]) -> Result<u32, Refused>;
    fn list_signature(&mut self) -> u64;
    /// `list_signature` と同じ値を返し、節のアドレスの並びを `out` にバックアップする（コピーのときと、書き戻しの後の並びの確かめ）。並びを持たない偽物はバックアップを空にする。
    fn list_chain(&mut self, out: &mut ListChain) -> u64 {
        *out = ListChain::EMPTY;
        self.list_signature()
    }
    /// 指図を置いて走査を 1 回呼ぶ。`resim` は進め直しの回か（ペースの入口・tick の記録を分ける旗）。
    fn run_scan(&mut self, order: Order, resim: bool) -> ScanRun;
    fn now_us(&mut self) -> u64;
    fn set_redirect(&mut self, on: bool);
    fn set_watch(&mut self, on: bool);
    fn out_counts(&mut self) -> OutCounts;
    fn born_len(&mut self) -> u32;
    fn clear_born(&mut self);
    /// 予測の tick（見張りの間）の、前からある領域の外の塊の解放を CRT へ渡さないか（`RB_HOLD_FREES`）。見張りと同時に開け閉めする。
    fn set_hold_frees(&mut self, _on: bool) {}
    /// コピーの後に確定していない tick を走らせた（書き戻しが来る）印を立てる。`clear_born`（コピー・書き戻し）と向け先を閉じるときに
    /// 倒れる。立っている間の見張りの外の領域の外の解放を数える（診断）。
    fn set_pending(&mut self) {}
    /// コピーのときのバックアップ: 確定したフレーム・範囲の外の節のアドレス・コピーの表の塊のアドレス・バックアップした ScreenEffect の本体のアドレス。見張りの外の解放が誰のものかを
    /// 見分ける（診断）のに加え、本体が見張りの外で本当に解放されたかを [`PlayMem::body_freed`] が答えるための表。
    fn set_stray_refs(&mut self, _frame: u32, _nodes: &[u32], _blocks: &[u32], _bodies: &[u32]) {}
    /// コピーの後に解放を CRT へ渡さなかった塊か（その塊はまだ確保されたままで、書き戻してよい）。
    fn freed_held(&mut self, _addr: u32) -> bool {
        false
    }
    fn read_node(&mut self, addr: u32) -> Option<[u32; NODE_WORDS]> {
        let mut w = [0u32; NODE_WORDS];
        for (i, v) in w.iter_mut().enumerate() {
            *v = self.read_u32(addr.checked_add(4 * i as u32)?)?;
        }
        Some(w)
    }
    /// コピーの範囲の外の列の節 1 つ（0x20 B）を書き戻す（コピーのときにバックアップした節だけ）。書けなければ false。
    fn write_node(&mut self, _addr: u32, _w: &[u32; NODE_WORDS]) -> bool {
        false
    }
    fn read_body(&mut self, addr: u32) -> Option<[u32; BODY_WORDS]> {
        let mut w = [0u32; BODY_WORDS];
        for (i, v) in w.iter_mut().enumerate() {
            *v = self.read_u32(addr.checked_add(4 * i as u32)?)?;
        }
        Some(w)
    }
    /// `out[i] = [base + i × stride]`（i < `out.len()`）を読む（陣の背景の VM の時計の測る入口）。1 つでも読めなければ false。本物は範囲の頁を
    /// 1 回だけ確かめて直に読む（CRT の塊の語をシステムの呼び出しで 1 語ずつ読むと、Present の後に百回を越える）。
    fn read_strided(&mut self, base: u32, stride: u32, out: &mut [u32]) -> bool {
        for (i, o) in out.iter_mut().enumerate() {
            let Some(a) = (i as u32)
                .checked_mul(stride)
                .and_then(|d| base.checked_add(d))
            else {
                return false;
            };
            match self.read_u32(a) {
                Some(v) => *o = v,
                None => return false,
            }
        }
        true
    }
    /// SE を 1 つ積む（ゲームの `0x43E2F0` を thiscall で呼ぶ。進め直しで初めて起きた SE を遅れて鳴らすため）。呼ぶ前に先の先頭のバイト列を照合し、
    /// 合わなければ呼ばずに false。既定は積まない。
    fn push_se(&mut self, _id: u32, _pan: i32) -> bool {
        false
    }
    fn write_body(&mut self, addr: u32, w: &[u32; BODY_WORDS]) -> bool;
    /// コピーのときにバックアップした ScreenEffect の本体 `addr` が、コピーの後に見張りの外で本当に解放された（CRT へ渡った）か。**分からなければ
    /// 真**（書き戻すと解放済みの所へ書くので、証明できないときは書かない側）。
    fn body_freed(&mut self, addr: u32) -> bool;
}

/// 描画が消す欄の 1 行（表は `th09_addr::RENDER_RESETS`）。コピーを取るたびに、コピーの中のこの欄を `after` に置き換える（`higanr_core::render_reset`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbRenderReset {
    pub block: &'static str,
    pub offset: u32,
    pub after: &'static [u8],
    /// `after` を作る描画の命令のアドレスとバイト列（照合だけ。書かない）。設置のときに本物のバイト列と比べ、違えば表を使わない（[`render_reset_code_matches`]）。
    pub code_at: u32,
    pub code: &'static [u8],
    pub source: &'static str,
}

/// 表の行の上限（`PlayState` の固定長の置き場。越えると `PlayState::new` が断る）。
pub const MAX_RENDER_RESETS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetRow {
    block: u8,
    off: u32,
    after: &'static [u8],
}

/// 表の各行の `code` が、本物のゲームの `code_at` のバイト列と同じか（設置のとき、読み取りのスレッドから 1 回。読むだけ）。
/// 違う・読めない行があれば、その行の名前を返す。
pub fn render_reset_code_matches(
    rows: &[RbRenderReset],
    read: &mut dyn FnMut(u32) -> Option<u32>,
) -> Result<(), String> {
    for r in rows {
        for (i, &want) in r.code.iter().enumerate() {
            let got = r
                .code_at
                .checked_add(i as u32)
                .and_then(&mut *read)
                .map(|w| w as u8);
            if got != Some(want) {
                return Err(format!(
                    "{} の {:#010X} の命令が違う（{:#010X} が {:?}、想定 {want:#04X}）",
                    r.block,
                    r.code_at,
                    r.code_at.wrapping_add(i as u32),
                    got
                ));
            }
        }
    }
    Ok(())
}

/// 列の節の語の数（0x20 B）。
pub const NODE_WORDS: usize = 8;
pub const BODY_WORDS: usize = (SCREEN_EFFECT_BYTES / 4) as usize;
/// コピーのときにバックアップする、コピーの範囲の外の ScreenEffect の本体の数の上限（同じ本体は 1 回だけ数える）。
pub const OUTSIDE_BODIES_CAP: usize = 16;
/// コピーのときにバックアップする、コピーの範囲の外の列の節の数の上限（2 本の列の合計）。越えた分はバックアップせず、数えて本ログへ（書き戻しの後の
/// 並びの確かめが拾う）。
pub const OUTSIDE_NODES_CAP: usize = 64;

/// コピーの範囲の外にある列の節のバックアップ（コピーのとき。書き戻しの後にそのまま書き戻す —— 予測の tick で外された節は、欄が 0 に
/// され解放される。解放は見張りの間は CRT へ渡さないので、書き戻す先はまだ確保されている）。
#[derive(Debug, Clone, Copy)]
struct OutsideNodes {
    n: usize,
    addr: [u32; OUTSIDE_NODES_CAP],
    words: [[u32; NODE_WORDS]; OUTSIDE_NODES_CAP],
}

impl OutsideNodes {
    const EMPTY: OutsideNodes = OutsideNodes {
        n: 0,
        addr: [0; OUTSIDE_NODES_CAP],
        words: [[0; NODE_WORDS]; OUTSIDE_NODES_CAP],
    };
}

/// コピーの範囲の外にある ScreenEffect の本体のバックアップ（コピーのとき。書き戻しの後にそのまま書き戻す）。後片付け `0x422900` が本体の
/// `+0x0C` を 0 にして本体を解放するが、予測の tick では解放は CRT へ渡さない（バックアップする）ので書き戻す先はまだ確保されている。
/// バックアップできなかった本体（上限・読めない・本体と思えない）は `missed` に数え、書き戻しのとき止める（静かに続けない）。
#[derive(Debug, Clone, Copy)]
struct OutsideBodies {
    n: usize,
    addr: [u32; OUTSIDE_BODIES_CAP],
    words: [[u32; BODY_WORDS]; OUTSIDE_BODIES_CAP],
    /// バックアップできなかった本体の数と、その最初のアドレス（アドレスが分からない形は 0）。
    missed: u32,
    missed_first: u32,
}

impl OutsideBodies {
    const EMPTY: OutsideBodies = OutsideBodies {
        n: 0,
        addr: [0; OUTSIDE_BODIES_CAP],
        words: [[0; BODY_WORDS]; OUTSIDE_BODIES_CAP],
        missed: 0,
        missed_first: 0,
    };

    fn miss(&mut self, addr: u32) {
        if self.missed == 0 {
            self.missed_first = addr;
        }
        self.missed = self.missed.saturating_add(1);
    }
}

/// 書き戻しのとき中身がコピーのときと違っていた、コピーの範囲の外の列の節の種類（コピーのときの `+0x00` 優先度と旗・`+0x04` 毎フレーム・
/// `+0x0C` 外すとき・`+0x1C` this）と回数（診断。予測の tick で何が外れているかを見る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChangedNode {
    pub head: u32,
    pub on_tick: u32,
    pub on_remove: u32,
    pub this: u32,
    pub n: u32,
}

/// `ChangedNode` をバックアップする種類の数（毎フレームの関数で分ける。越えた分は `changed_nodes_other` に数える）。
pub const CHANGED_KINDS: usize = 8;

/// `addr` がコピーの範囲（`layout` のどれか）の中か。VM の配列は今の長さだけ（置き場の大きさ `cap` は上限の本数ぶんで、配列の
/// 後ろの CRT の塊 —— 範囲の外の節や ScreenEffect の本体でありうる —— を「中」と見るとバックアップし損ねる）。
fn in_layout(layout: &Layout, addr: u32) -> bool {
    layout.regions().iter().any(|r| {
        let span = match r.kind {
            RegionKind::VmArray(_) => r.len,
            _ => r.len.max(r.cap),
        };
        addr >= r.addr && addr - r.addr < span
    })
}

/// `PlayMem` を `LayoutIo` として渡す包み（範囲の表を埋める手順は `fill_layout`）。
struct AsLayout<'a>(&'a mut dyn PlayMem);

impl LayoutIo for AsLayout<'_> {
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        self.0.read_u32(addr)
    }
    fn check_rw(&mut self, addr: u32, len: u32) -> bool {
        self.0.check_rw(addr, len)
    }
}

/// `PlayMem` を `OutputMem` として渡す包み（読むのは `OUTPUT_RANGES` と SE のパンの欄、書くのは `OUTPUT_RANGES` だけ）。
struct AsOutputs<'a>(&'a mut dyn PlayMem);

impl OutputMem for AsOutputs<'_> {
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        if !output_addr_readable(addr) {
            return None;
        }
        self.0.read_u32(addr)
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> bool {
        output_addr_allowed(addr) && self.0.write_u32(addr, v)
    }
}

/// 更新の列と描画の列の並び（節のアドレス・関数・this）のダイジェスト。`read` は 4 バイトの読み。列はレジストリ `0x4ACFC8` の先頭の節から
/// `+0x14`（次）でたどり、描画の列は `+0x20` の先頭の節から。
pub fn list_signature_with(read: &mut dyn FnMut(u32) -> Option<u32>) -> u64 {
    list_walk_with(read, &mut |_, _, _| {})
}

/// 列の並びの節のアドレスのバックアップの枠（1 本の列あたり。越えた分はバックアップしない —— 書き戻しの後の並びが違ったときの診断だけに使う）。
pub const CHAIN_CAP: usize = 128;

/// 更新の列・描画の列の節のアドレスの並び（コピーのときのバックアップ。書き戻しの後の並びが違ったとき、どの節から分かれたかを言う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListChain {
    pub nodes: [[u32; CHAIN_CAP]; 2],
    pub lens: [u16; 2],
}

impl ListChain {
    pub const EMPTY: ListChain = ListChain {
        nodes: [[0; CHAIN_CAP]; 2],
        lens: [0; 2],
    };

    pub fn list(&self, list: usize) -> &[u32] {
        let n = self.lens.get(list).map_or(0, |&n| usize::from(n));
        self.nodes.get(list).map_or(&[], |a| &a[..n.min(CHAIN_CAP)])
    }
}

/// [`list_signature_with`] と同じダイジェストを作りながら、節のアドレスを `chain` にバックアップする。
pub fn list_chain_with(read: &mut dyn FnMut(u32) -> Option<u32>, chain: &mut ListChain) -> u64 {
    *chain = ListChain::EMPTY;
    list_walk_with(read, &mut |list, i, node| {
        if let Some(slot) = chain.nodes.get_mut(list).and_then(|a| a.get_mut(i)) {
            *slot = node;
            chain.lens[list & 1] = (i + 1) as u16;
        }
    })
}

/// 2 本の列を先頭から `+0x14`（次）でたどってダイジェストを作る。`on_node(列の番号, 列の中の位置, 節のアドレス)` を節ごとに呼ぶ。
fn list_walk_with(
    read: &mut dyn FnMut(u32) -> Option<u32>,
    on_node: &mut dyn FnMut(usize, usize, u32),
) -> u64 {
    let mut h = higanr_core::digest::DIGEST_INIT;
    for (list, head) in [UPDATE_REGISTRY, UPDATE_REGISTRY + UPDATE_DRAW_HEAD]
        .into_iter()
        .enumerate()
    {
        let mut node = head;
        for i in 0..4096 {
            on_node(list, i, node);
            let f = read(node.wrapping_add(UPDATE_FN)).unwrap_or(0);
            let arg = read(node.wrapping_add(UPDATE_ARG)).unwrap_or(0);
            let mut w = [0u8; 12];
            w[..4].copy_from_slice(&node.to_le_bytes());
            w[4..8].copy_from_slice(&f.to_le_bytes());
            w[8..].copy_from_slice(&arg.to_le_bytes());
            h = higanr_core::digest::digest_continue(h, &w);
            match read(node.wrapping_add(UPDATE_NEXT)) {
                Some(n) if n != 0 && n != head => node = n,
                _ => break,
            }
        }
        h = higanr_core::digest::digest_continue(h, &[0xFF]);
    }
    h
}

/// Supervisor の 2 欄（今の場面・次の場面の要求）。コピーの範囲の外（`0x4B30B0`〜`0x4B38B0` はコピーしない）なので、コピーのときにバックアップして書き戻しで戻す。
pub const SUP_FIELDS: [u32; 2] = [SUPERVISOR + SUP_SCENE_CUR, SUPERVISOR + SUP_SCENE_REQ];

fn scene_pending(mem: &mut dyn PlayMem) -> bool {
    match (mem.read_u32(SUP_FIELDS[1]), mem.read_u32(SUP_FIELDS[0])) {
        (Some(req), Some(cur)) => req != cur,
        _ => true,
    }
}

/// 以後この seg で rollback を続けられない食い違い（入口は [`ScanFrame::Broken`] を返し、配線はマッチをやめる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 範囲の表を埋められない（ポインタが 0・読めない・EffectSys の形が違う・書けない頁）。
    Layout(LayoutError),
    /// コピーの後に塊・要素の配列のアドレスが変わった（書き戻すと解放済みの所へ書く）。
    PlacesMoved,
    /// 書き戻すコピーが無い・フレームが違う（状態機械との食い違い）。
    NoSnapshot { want: u32 },
    /// Supervisor の 2 欄が読めない・書けない。
    Supervisor,
    /// 書き戻した後の列の並びが、コピーしたときと違う（コピーの外の節のつなぎが戻らなかった）。
    ListNotRestored,
    /// コピーの範囲の外の列の節を書き戻せなかった（頁が書けない）。後片付けの欄などが静かに失われるので止める。
    NodeNotWritten { addr: u32 },
    /// コピーのとき、範囲の外の ScreenEffect の本体をバックアップできなかった（上限・読めない・本体と思えない形。`addr` は分かれば本体、
    /// 無ければ 0）。書き戻しのとき、その本体の `+0x0C` などが戻らず、解放済みの本体を描画の節が指す形になりうるので止める。
    BodyNotCaptured { addr: u32 },
    /// バックアップした ScreenEffect の本体が、コピーの後に見張りの外で本当に解放された（CRT へ渡った）か、解放されていないと言い切れない。
    /// 書き戻すと解放済みの所へ書くので書かずに止める。
    BodyFreed { addr: u32 },
    /// バックアップした ScreenEffect の本体を書き戻せなかった（頁が書けない）。
    BodyNotWritten { addr: u32 },
    /// 状態機械が更新したと言うのに、走査を呼んだ記録が無い。
    NoLiveRun,
    /// 入口の中でパニックを受けた（本物の層が置く）。
    Panicked,
}

impl Fault {
    pub fn text(&self) -> String {
        match self {
            Fault::Layout(e) => format!("範囲の表を埋められない（{e:?}）"),
            Fault::PlacesMoved => "コピーの後に塊か要素の配列のアドレスが変わった".to_string(),
            Fault::NoSnapshot { want } => format!("フレーム {want} の写しが無い"),
            Fault::Supervisor => "Supervisor の場面の 2 欄を読めない・書けない".to_string(),
            Fault::ListNotRestored => {
                "書き戻した後の更新の列・描画の列の並びが写したときと違う".to_string()
            }
            Fault::NodeNotWritten { addr } => {
                format!("コピーの範囲の外の列の節 {addr:#010X} を書き戻せない")
            }
            Fault::BodyNotCaptured { addr } => format!(
                "コピーの範囲の外の ScreenEffect の本体 {addr:#010X} を控えられない（上限・読めない・本体と思えない形）"
            ),
            Fault::BodyFreed { addr } => format!(
                "控えた ScreenEffect の本体 {addr:#010X} が見張りの外で解放された（解放されていないと言い切れない）"
            ),
            Fault::BodyNotWritten { addr } => {
                format!("控えた ScreenEffect の本体 {addr:#010X} を書き戻せない")
            }
            Fault::NoLiveRun => "更新したと言われたのに走査を呼んでいない".to_string(),
            Fault::Panicked => "入口の中でパニックを受けた".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub n: u32,
    pub sum_us: u64,
    pub max_us: u32,
}

impl Time {
    fn add(&mut self, us: u64) {
        let us = us.min(u64::from(u32::MAX)) as u32;
        self.n = self.n.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(u64::from(us));
        self.max_us = self.max_us.max(us);
    }

    fn mean_ms(&self) -> String {
        if self.n == 0 {
            return "—".to_string();
        }
        format!("{:.2}", self.sum_us as f64 / f64::from(self.n) / 1000.0)
    }

    fn max_ms(&self) -> String {
        if self.n == 0 {
            return "—".to_string();
        }
        format!("{:.2}", f64::from(self.max_us) / 1000.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayStats {
    /// 走査の包みを通ったフレーム・描画だけにしたフレーム。
    pub frames: u32,
    pub render_only: u32,
    /// 本物の tick（入力の地点）・進め直しの確定の入力ワードの tick・予測の入力ワードの tick。
    pub live_ticks: u32,
    pub resim_confirmed_ticks: u32,
    pub resim_predicted_ticks: u32,
    /// 確定していない tick（本物の予測の tick ＋ 進め直しの予測の tick）。
    pub unconfirmed_ticks: u32,
    pub saves: u32,
    pub restores: u32,
    /// 範囲の表を埋め直した回数（seg の初めと、アドレスが変わったとき）。
    pub layout_fills: u32,
    /// コピー・書き戻し・進め直しの tick 1 回・Present の後の手順全体（進め直しがあったフレームだけ）の時間。
    pub save_time: Time,
    pub restore_time: Time,
    pub resim_tick_time: Time,
    pub resim_frame_time: Time,
    /// 走査が 1 を返した tick（一時停止の間を含む）・0 か負を返した tick・入力の地点が指図を取らなかった tick。
    pub stopped_ticks: u32,
    pub abnormal_ticks: u32,
    pub unfed_ticks: u32,
    /// 次の場面の要求が立った tick・確定していない tick で列の並びが変わった数（境目にした）・確定した tick で変わった数。
    pub scene_ticks: u32,
    pub list_holds: u32,
    pub list_changes_confirmed: u32,
    /// 予測の間（確定していない tick の中）の、領域の外の確保・解放。
    pub pred_out_allocs: u32,
    pub pred_out_frees_old: u32,
    pub pred_out_frees_born: u32,
    /// 書き戻しの時点で、予測の間に生まれて生きていた領域の外の塊（進め直しのたびに作り直す塊の漏れの目安）。
    pub pred_out_alive_at_restore: u32,
    /// 予測の間の、前からある領域の外の塊の解放のうち CRT へ渡さなかったもの（確定の進め直しがもう一度解放する）。
    pub pred_out_frees_held: u32,
    /// コピーの範囲の外にあった列の節の数の最大（1 回のコピーあたり）・バックアップの枠が足りなかった節・読めなかった節・書き戻せなかった節。
    pub outside_nodes_max: u32,
    pub nodes_over: u32,
    pub nodes_unreadable: u32,
    pub nodes_unwritten: u32,
    /// 範囲の外の ScreenEffect の本体: 1 回のコピーでバックアップした数の最大・バックアップの枠が足りなかった本体・読めなかった（本体と思えない形を含む）
    /// 本体・書き戻した本体（のべ）・うち中身がコピーのときと違っていた本体・書き戻せなかった本体・見張りの外で解放されていて書かず
    /// に止めた本体。
    pub bodies_max: u32,
    pub bodies_over: u32,
    pub bodies_unreadable: u32,
    pub bodies_restored: u32,
    pub bodies_changed: u32,
    pub bodies_unwritten: u32,
    pub bodies_freed: u32,
    /// アドレスが動いた塊・要素の配列を、元の解放をバックアップしていたので元のアドレスへ書き戻した書き戻し。
    pub places_moved_back: u32,
    /// SE・BGM・文字の数をバックアップできなかった・戻せなかった進め直し。
    pub outputs_uncaptured: u32,
    pub outputs_unrestored: u32,
    /// 自分のリングが溢れて取得できなかったフレーム（描画だけにした）。
    pub capture_full: u32,
    /// バックアップの列が溢れて落とした確定の入力ワード・CHECK。
    pub confirmed_dropped: u32,
    pub checks_dropped: u32,
    /// 描画が消す欄: 使っている表の行の数（設置で照合が合わなければ 0）・コピーの中で置き換える前の値が描画の後の値と違っていた欄
    /// （のべ。進め直しの途中のコピーに描画が消すはずの値が残っていた数）・置き換えを断った欄（塊がコピーに無い・はみ出す。0 の見込み）。
    pub reset_rows: u32,
    pub reset_changed: u32,
    pub reset_refused: u32,
    /// 書き戻しの直後に見た Stage の数（書き戻し × 2 陣。読めなかった陣は数えない）と、そのうち色の模様が 0 なのに色の α が 0 でなかった数
    /// （更新がもう書かないのに描画が色を掛け続ける形 —— カットインの赤い点滅・暗いまま。直した後は 0 の見込み）。
    pub tint_checked: u32,
    pub tint_stale: u32,
    /// コピーのときに色の α が 0 でなかった Stage の数（コピー × 2 陣のうち。進め直しの途中にカットインの色が立っていたコピー）。
    pub tint_at_save: u32,
    /// 測る入口。どれも Present の後の入口の始まり（コピー・書き戻しの前 = 描画した状態）で 1 回ずつ読む。fps の表示の丸めた値 `[0x4B36C8]`（i16）の分布
    /// （添字は [`fps_bucket`]、見出しは [`FPS_LABELS`]）・読んだ回数・値の和。
    pub fps_hist: [u32; FPS_BUCKETS],
    pub fps_n: u32,
    pub fps_sum: i64,
    /// 陣の背景（中の名前は Stage）の VM の時計が、前の Present の後の入口から進んだ tick の数の分布（Present × 陣ごとに 1 つ。
    /// 生きている VM のうち進みが 0〜0x7FFF のものの最大。添字は [`bg_step_bucket`]、見出しは [`BG_STEP_LABELS`]）。delay 方式と
    /// 同じく 1 に集まるのが正しい形（描画 1 回に更新 1 回）。2 以上は書き戻しの後の進め直しの tick が背景を先へ進めた署名。
    pub bg_step: [u32; BG_STEP_BUCKETS],
    pub bg_unreadable: u32,
    /// 範囲の表を埋めたときに見た陣の背景の VM の本数の最大・上限（`BG_VM_MAX`）を越えてコピーしなかった陣（埋めた回 × 陣のうち）。
    pub bg_count_max: u32,
    pub bg_over: u32,
    /// 書き戻しで陣の背景の VM の配列を書き戻した数（書き戻し × 陣のうち、長さが 0 でなかったもの）。
    pub bg_restored: u32,
    /// 書き戻しの前後で描画の回数 `[0x4B42CC]` が変わった書き戻し（fps の計測の欄を書き戻した署名。直した後は 0）と、見た書き戻し。
    pub fps_words_restored: u32,
    pub fps_words_checked: u32,
    /// 進め直しで初めて起きた SE を遅れて積み直す経路の数え（`rollback_resim::SeStats`）。
    pub se: SeStats,
}

/// fps の表示の分布の枠の数と見出し（`fps_bucket`）。60 の前後を 1 ずつ、離れた所は粗く分ける。
pub const FPS_BUCKETS: usize = 15;
pub const FPS_LABELS: [&str; FPS_BUCKETS] = [
    "19 以下",
    "20〜29",
    "30〜39",
    "40〜49",
    "50〜54",
    "55",
    "56",
    "57",
    "58",
    "59",
    "60",
    "61",
    "62",
    "63〜69",
    "70 以上",
];

pub fn fps_bucket(v: i16) -> usize {
    match v {
        i16::MIN..=19 => 0,
        20..=29 => 1,
        30..=39 => 2,
        40..=49 => 3,
        50..=54 => 4,
        55..=62 => (v - 50) as usize,
        63..=69 => 13,
        _ => 14,
    }
}

/// 陣の背景の VM の時計の進みの分布の枠の数と見出し（`bg_step_bucket`）。
pub const BG_STEP_BUCKETS: usize = 7;
pub const BG_STEP_LABELS: [&str; BG_STEP_BUCKETS] = [
    "0",
    "1",
    "2",
    "3",
    "4〜7",
    "8 以上",
    "進みを見られる VM が無い",
];

pub fn bg_step_bucket(step: Option<u32>) -> usize {
    match step {
        Some(s @ 0..=3) => s as usize,
        Some(4..=7) => 4,
        Some(_) => 5,
        None => 6,
    }
}

/// 陣の背景の VM の時計のバックアップ（陣ごと。前の Present の後の入口で読んだ値）。
#[derive(Debug, Clone, Copy)]
struct BgWatch {
    valid: bool,
    addr: u32,
    n: u32,
    clocks: [u32; BG_VM_MAX as usize],
}

impl BgWatch {
    const EMPTY: BgWatch = BgWatch {
        valid: false,
        addr: 0,
        n: 0,
        clocks: [0; BG_VM_MAX as usize],
    };
}

#[derive(Debug, Clone, Copy)]
pub struct Queue<T: Copy, const N: usize> {
    buf: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Queue<T, N> {
    pub const fn new() -> Self {
        Queue {
            buf: [None; N],
            head: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, v: T) -> bool {
        if self.len >= N || N == 0 {
            return false;
        }
        let i = (self.head + self.len) % N;
        if let Some(slot) = self.buf.get_mut(i) {
            *slot = Some(v);
            self.len += 1;
            true
        } else {
            false
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 || N == 0 {
            return None;
        }
        let v = self.buf.get_mut(self.head).and_then(Option::take);
        self.head = (self.head + 1) % N;
        self.len -= 1;
        v
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<T: Copy, const N: usize> Default for Queue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// 確定した (frame, p1, p2) のバックアップの枠の数（1 回の進め直しで確定するのは R ≤ 16 フレームまで。配線が入口のたびに汲む）。
pub const CONFIRMED_CAP: usize = 64;
/// CHECK のバックアップの枠の数（30 フレームに 1 つ）。
pub const CHECKS_CAP: usize = 8;

#[derive(Debug, Clone, Copy)]
struct Snap {
    frame: u32,
    /// コピーしたときの範囲の表（書き戻しはこのアドレスと長さで行う）。
    layout: Layout,
    sup: [u32; 2],
    list: u64,
    chain: ListChain,
    /// コピーの範囲の外にある列の節の中身（書き戻しの後に書き戻す）。
    outside: OutsideNodes,
    /// 上の節のうち ScreenEffect のものが指す、コピーの範囲の外の本体の中身（書き戻しの後に書き戻す）。
    bodies: OutsideBodies,
}

/// 書き戻した後の列の並びがコピーしたときと違った所（[`Fault::ListNotRestored`] の診断。本ログへ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListDiag {
    /// 0 = 更新の列・1 = 描画の列。
    pub list: u8,
    pub index: u16,
    /// 分かれる直前の節（この節の「次」が戻らなかった）。
    pub before: u32,
    /// コピーしたときの次の節・今の次の節（列が終わっていれば 0）。
    pub want: u32,
    pub got: u32,
    /// `before` がコピーの範囲の中か（中なら範囲の書き戻しで戻っているはず）。
    pub before_in_snap: bool,
    pub want_len: u16,
    pub got_len: u16,
}

impl ListDiag {
    pub fn text(&self) -> String {
        format!(
            "{}の列の {} 番目で分かれた: 直前の節 {:#010X}（コピーの範囲の{}）の次が 写したとき {:#010X} / 今 {:#010X}（節の数 写したとき {} / 今 {}）",
            if self.list == 0 { "更新" } else { "描画" },
            self.index,
            self.before,
            if self.before_in_snap { "中" } else { "外" },
            self.want,
            self.got,
            self.want_len,
            self.got_len
        )
    }
}

/// コピーしたときと今の節の並びを比べ、最初に分かれた所を返す（同じなら `None`。節のアドレスが同じで関数・this だけ違うときも `None`）。
fn list_diverge(layout: &Layout, want: &ListChain, got: &ListChain) -> Option<ListDiag> {
    for list in 0..2 {
        let (w, g) = (want.list(list), got.list(list));
        for i in 1..w.len().max(g.len()) {
            let (wn, gn) = (
                w.get(i).copied().unwrap_or(0),
                g.get(i).copied().unwrap_or(0),
            );
            if wn != gn {
                let before = w.get(i - 1).copied().unwrap_or(0);
                return Some(ListDiag {
                    list: list as u8,
                    index: i as u16,
                    before,
                    want: wn,
                    got: gn,
                    before_in_snap: in_layout(layout, before),
                    want_len: w.len() as u16,
                    got_len: g.len() as u16,
                });
            }
        }
    }
    None
}

/// 入力の地点の直前に状態機械からコピーする値（`tick` が「確定した状態の上の本物の入力ワードの tick か」を決めるのに使う。
/// 状態機械の `live_tick` の `confirmed = 本物の語 && N == F` と同じ決め方）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Hint {
    remote_next: u32,
    delay: u8,
}

pub struct PlayState {
    tables: Tables,
    plan: Layout,
    placed: Option<Layout>,
    snap: Option<Snap>,
    outputs: FullOutputs,
    /// フレームごとの、鳴らした（積まれた）SE の id の集合（seg の中のフレーム番号が印。`begin_seg` で空にする）。
    se_played: SePlayed,
    se_pending: SePending,
    /// この進め直しの SE の経路が生きているか（`resim_begin` でバックアップも空にもできたときだけ。`resim_end` で倒す）。
    se_armed: bool,
    /// 被弾音の鳴らし方（ini の `rollback_hit_se`。既定は確定まで待つ。`set_hit_se` で置く）。
    hit_se: HitSe,
    in_resim: bool,
    hint: Hint,
    front: u32,
    last_live: Option<ScanRun>,
    fault: Option<Fault>,
    list_diag: Option<ListDiag>,
    place_diag: Option<PlaceDiag>,
    changed_nodes: [ChangedNode; CHANGED_KINDS],
    changed_nodes_other: u32,
    /// 確保の向け先を今開けているか（`mem.set_redirect` に最後に渡した値。変わるときだけ渡す —— delay 方式で開いたままの入口を
    /// 毎フレーム叩かない）。
    redirect: bool,
    confirmed: Queue<(u32, u16, u16), CONFIRMED_CAP>,
    checks: Queue<Check, CHECKS_CAP>,
    stats: PlayStats,
    resets: [ResetRow; MAX_RENDER_RESETS],
    n_resets: usize,
    stage_blocks: [u8; 2],
    bg: [BgWatch; 2],
}

impl PlayState {
    /// `arena_base` / `arena_bytes` は DLL の領域（範囲の表の形を決める）。
    pub fn new(tables: Tables, arena_base: u32, arena_bytes: u32) -> Result<PlayState, String> {
        Self::new_with_resets(tables, RENDER_RESETS, arena_base, arena_bytes)
    }

    /// 描画が消す欄の表を渡して作る（設置で命令の照合が合わなかったときは空の表。検査は小さな表）。表の行の塊が `tables` に
    /// 無い・欄が塊からはみ出す・行が多すぎるときは断る（ゲームのスレッドで断らずに済むよう、ここで 1 回だけ見る）。
    pub fn new_with_resets(
        tables: Tables,
        resets: &'static [RbRenderReset],
        arena_base: u32,
        arena_bytes: u32,
    ) -> Result<PlayState, String> {
        let plan = Layout::plan(&tables, arena_base, arena_bytes)?;
        let block_of = |name: &str| -> Result<(u8, u32), String> {
            let i = tables
                .blocks
                .iter()
                .position(|b| b.name == name)
                .ok_or_else(|| format!("塊 {name} がコピーの表に無い"))?;
            Ok((i as u8, tables.blocks[i].size))
        };
        if resets.len() > MAX_RENDER_RESETS {
            return Err(format!(
                "描画が消す欄の表が {} 行（{MAX_RENDER_RESETS} 行まで）",
                resets.len()
            ));
        }
        let mut rows = [ResetRow {
            block: 0,
            off: 0,
            after: &[],
        }; MAX_RENDER_RESETS];
        for (slot, r) in rows.iter_mut().zip(resets) {
            let (block, size) = block_of(r.block)?;
            render_reset::check(
                size as usize,
                &[ResetField {
                    off: r.offset,
                    after: r.after,
                }],
            )
            .map_err(|e| {
                format!(
                    "描画が消す欄 {} +{:#X} を使えない（{e:?}）",
                    r.block, r.offset
                )
            })?;
            *slot = ResetRow {
                block,
                off: r.offset,
                after: r.after,
            };
        }
        let stage_blocks = [block_of(STAGE_BLOCKS[0])?.0, block_of(STAGE_BLOCKS[1])?.0];
        let stats = PlayStats {
            reset_rows: resets.len() as u32,
            ..PlayStats::default()
        };
        Ok(PlayState {
            tables,
            plan,
            placed: None,
            snap: None,
            outputs: FullOutputs::EMPTY,
            se_played: SePlayed::EMPTY,
            se_pending: SePending::EMPTY,
            se_armed: false,
            hit_se: HitSe::default(),
            in_resim: false,
            hint: Hint::default(),
            front: 0,
            last_live: None,
            fault: None,
            list_diag: None,
            place_diag: None,
            changed_nodes: [ChangedNode::default(); CHANGED_KINDS],
            changed_nodes_other: 0,
            redirect: false,
            confirmed: Queue::new(),
            checks: Queue::new(),
            stats,
            resets: rows,
            n_resets: resets.len(),
            stage_blocks,
            bg: [BgWatch::EMPTY; 2],
        })
    }

    /// 測る入口（Present の後の入口の始まり。コピー・書き戻しの前 = 描画した状態）: fps の表示の丸めた値と、陣の背景の VM の時計の前の回からの進みを数える。
    /// 読むだけで、ゲームのメモリには書かない。確保しない。
    fn sample_present(&mut self, mem: &mut dyn PlayMem) {
        let s = &mut self.stats;
        if let Some(w) = mem.read_u32(FPS_SHOWN) {
            let v = w as u16 as i16;
            s.fps_hist[fps_bucket(v)] = s.fps_hist[fps_bucket(v)].saturating_add(1);
            s.fps_n = s.fps_n.saturating_add(1);
            s.fps_sum = s.fps_sum.saturating_add(i64::from(v));
        }
        // 読む配列はコピーする表（`Tables::vm_arrays`）でなく本物の表から引く: 測るのは「コピーしたか」に関わらない背景の進みで、コピーしない
        // 表（直す前の形）の実行でも同じ入口で比べられるように。
        let t = self.tables;
        for (i, v) in ROLLBACK_VM_ARRAYS.iter().enumerate().take(self.bg.len()) {
            let shape = vm_array_shape(&t, v, &mut AsLayout(&mut *mem), i as u8).ok();
            let w = &mut self.bg[i];
            let Some(shape) = shape.filter(|x| x.addr != 0 && x.count > 0) else {
                w.valid = false;
                continue;
            };
            let n = (shape.count as u32).min(BG_VM_MAX);
            let mut clocks = [0u32; BG_VM_MAX as usize];
            let mut instrs = [0u32; BG_VM_MAX as usize];
            let ok = shape
                .addr
                .checked_add(VM_CLOCK)
                .is_some_and(|b| mem.read_strided(b, v.elem, &mut clocks[..n as usize]))
                && shape
                    .addr
                    .checked_add(VM_CUR_INSTR)
                    .is_some_and(|b| mem.read_strided(b, v.elem, &mut instrs[..n as usize]));
            let s = &mut self.stats;
            if !ok {
                s.bg_unreadable = s.bg_unreadable.saturating_add(1);
                w.valid = false;
                continue;
            }
            if w.valid && w.addr == shape.addr && w.n == n {
                // 終わった VM（今の命令が 0）は進まないので外す。台本の飛び（時刻を戻す）で負になった VM も外す。
                let step = (0..n as usize)
                    .filter(|&j| instrs[j] != 0)
                    .map(|j| clocks[j].wrapping_sub(w.clocks[j]))
                    .filter(|&d| d <= 0x7FFF)
                    .max();
                let b = bg_step_bucket(step);
                s.bg_step[b] = s.bg_step[b].saturating_add(1);
            }
            *w = BgWatch {
                valid: true,
                addr: shape.addr,
                n,
                clocks,
            };
        }
    }

    fn note_vm_arrays(&mut self, mem: &mut dyn PlayMem) {
        let t = self.tables;
        for (i, v) in t.vm_arrays.iter().enumerate() {
            if let Ok(shape) = vm_array_shape(&t, v, &mut AsLayout(&mut *mem), i as u8) {
                let s = &mut self.stats;
                s.bg_count_max = s.bg_count_max.max(shape.count.max(0) as u32);
                if shape.over(v) {
                    s.bg_over = s.bg_over.saturating_add(1);
                }
            }
        }
    }

    /// 確保の向け先を開け閉めする（今と同じ値なら何もしない）。閉じるのは、マッチを動かさなくなった tick（delay 方式・場面の終わり・
    /// 食い違い・ネットワークが終わった）。
    pub fn set_redirect(&mut self, mem: &mut dyn PlayMem, on: bool) {
        if self.redirect != on {
            mem.set_redirect(on);
            self.redirect = on;
        }
    }

    pub fn buffer_bytes(&self) -> u32 {
        self.plan.buffer_bytes()
    }

    pub fn plan(&self) -> &Layout {
        &self.plan
    }

    pub fn stats(&self) -> PlayStats {
        self.stats
    }

    pub fn set_hit_se(&mut self, hit: HitSe) {
        self.hit_se = hit;
        self.stats.se.hit_mode = hit;
    }

    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }

    pub fn list_diag(&self) -> Option<ListDiag> {
        self.list_diag
    }

    pub fn place_diag(&self) -> Option<PlaceDiag> {
        self.place_diag
    }

    /// 書き戻しのとき中身が変わっていた範囲の外の節の種類（数えた分だけ）と、種類の枠に入らなかった数。接続を跨いで数える。
    pub fn changed_nodes(&self) -> ([ChangedNode; CHANGED_KINDS], u32) {
        (self.changed_nodes, self.changed_nodes_other)
    }

    fn note_changed_node(&mut self, saved: &[u32; NODE_WORDS]) {
        let (head, on_tick, on_remove, this) = (saved[0], saved[1], saved[3], saved[7]);
        let slot = self
            .changed_nodes
            .iter_mut()
            .find(|c| c.n == 0 || (c.on_tick == on_tick && c.on_remove == on_remove));
        match slot {
            Some(c) => {
                if c.n == 0 {
                    *c = ChangedNode {
                        head,
                        on_tick,
                        on_remove,
                        this,
                        n: 0,
                    };
                }
                c.n = c.n.saturating_add(1);
            }
            None => self.changed_nodes_other = self.changed_nodes_other.saturating_add(1),
        }
    }

    #[cfg(test)]
    pub fn in_resim(&self) -> bool {
        self.in_resim
    }

    /// 入力の地点の直前に状態機械からコピーした、その seg の D（`is_confirmed_tick` が穴埋めの判定に使う。状態機械の `seg_delay` と
    /// 食い違うと「確定した状態の上の本物の入力ワード」の見分けが状態機械とずれる）。
    #[cfg(test)]
    pub fn hint_delay(&self) -> u8 {
        self.hint.delay
    }

    /// 新しい seg に入る（配線が `Rollback::begin_seg` と同時に呼ぶ）。コピーとアドレスの表を捨て、次のコピーで埋め直す。食い違いの印は残す（壊れた接続を続けない）。
    pub fn begin_seg(&mut self) {
        self.placed = None;
        self.snap = None;
        self.front = 0;
        self.last_live = None;
        self.in_resim = false;
        self.outputs.valid = false;
        self.se_played.clear();
        self.se_pending.clear();
        self.se_armed = false;
    }

    /// 新しい接続の最初の入口（配線が `rb_sync_seg` で 1 回だけ呼ぶ）。`PlayState` は静的で接続を跨いで生き残るので、前の接続の食い違いの印と、
    /// 汲み残した確定の入力ワード・CHECK を捨てる（捨てないと 2 本目の接続が frame 0 から `Broken` になる）。数え（`PlayStats`）は残す。
    pub fn begin_conn(&mut self) {
        self.begin_seg();
        self.fault = None;
        self.list_diag = None;
        self.place_diag = None;
        self.confirmed = Queue::new();
        self.checks = Queue::new();
    }

    pub fn drain_confirmed(&mut self, f: &mut dyn FnMut(u32, u16, u16)) {
        while let Some((fr, p1, p2)) = self.confirmed.pop() {
            f(fr, p1, p2);
        }
    }

    pub fn drain_checks(&mut self, f: &mut dyn FnMut(&Check)) {
        while let Some(c) = self.checks.pop() {
            f(&c);
        }
    }

    fn set_fault(&mut self, f: Fault) {
        if self.fault.is_none() {
            self.fault = Some(f);
        }
    }
}

/// コピーのアドレスが今も同じか（塊と要素の配列のポインタを読み直すだけ。頁の確かめはしない —— 1 フレームに 1 回回すので軽くする）。
fn places_unchanged(l: &Layout, t: &Tables, mem: &mut dyn PlayMem) -> bool {
    first_moved(l, t, mem).is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceDiag {
    pub kind: RegionKind,
    pub was: u32,
    pub now: Option<u32>,
}

impl PlaceDiag {
    pub fn text(&self, t: &Tables) -> String {
        let name = match self.kind {
            RegionKind::Block(b) => t
                .blocks
                .get(b as usize)
                .map_or_else(|| format!("塊 {b}"), |x| format!("塊 {}", x.name)),
            RegionKind::Effect(e) => format!("要素の配列 {e}"),
            RegionKind::VmArray(v) => t.vm_arrays.get(v as usize).map_or_else(
                || format!("VM の配列 {v}"),
                |x| format!("VM の配列 {}", x.name),
            ),
            k => format!("{k:?}"),
        };
        let now = self.now.map_or("読めない・形が違う".to_string(), |a| {
            format!("{a:#010X}")
        });
        format!("{name}: 写したとき {:#010X} / 今 {now}", self.was)
    }
}

fn first_moved(l: &Layout, t: &Tables, mem: &mut dyn PlayMem) -> Option<PlaceDiag> {
    l.regions().iter().find_map(|r| region_moved(r, t, mem))
}

/// 範囲 1 つのアドレスがコピーの後に変わったか（.data・領域・割り当て器の状態はアドレスが変わらないので `None`）。
fn region_moved(r: &Region, t: &Tables, mem: &mut dyn PlayMem) -> Option<PlaceDiag> {
    if let RegionKind::VmArray(v) = r.kind {
        let now = t
            .vm_arrays
            .get(v as usize)
            .and_then(|x| vm_array_shape(t, x, &mut AsLayout(mem), v).ok())
            .map(|s| s.place());
        return (now != Some((r.addr, r.len))).then_some(PlaceDiag {
            kind: r.kind,
            was: r.addr,
            now: now.filter(|&(_, len)| len == r.len).map(|(a, _)| a),
        });
    }
    {
        let now = match r.kind {
            RegionKind::Block(b) => t
                .blocks
                .get(b as usize)
                .and_then(|x| mem.read_u32(x.ptr_at)),
            RegionKind::Effect(e) => t.effects.get(e as usize).and_then(|x| {
                let sys = mem.read_u32(x.sys_ptr_at).filter(|&s| s != 0)?;
                let count = mem.read_u32(sys.checked_add(t.effect_count)?)?;
                let extra = mem.read_u32(sys.checked_add(t.effect_extra)?)?;
                if count != x.count || extra != x.extra {
                    return None;
                }
                mem.read_u32(sys.checked_add(t.effect_ptr)?)
            }),
            RegionKind::VmArray(_)
            | RegionKind::Data(_)
            | RegionKind::Arena
            | RegionKind::ArenaState => return None,
        };
        (now != Some(r.addr)).then_some(PlaceDiag {
            kind: r.kind,
            was: r.addr,
            now,
        })
    }
}

pub struct GameWindow<'a> {
    pub mem: &'a mut dyn PlayMem,
    pub st: &'a mut PlayState,
}

impl GameWindow<'_> {
    /// 2 本の列の節のうち、コピーの範囲の外にあるものの中身（0x20 B）をバックアップする（コピーのとき）。
    ///
    /// ScreenEffect の節（`+0x04` の関数で見分ける）の this（`+0x1C`）がコピーの範囲の外なら、this の本体（0x38 B）も同じ手順でバックアップする
    /// （同じ本体を指す更新の節と描画の節は 1 回だけ）。後片付け `0x422900` が範囲の外の本体の `+0x0C` を 0 にして解放するので、節だけ戻しても
    /// 本体の欄が戻らず、確定の進め直しの外しが空振りして描画の節が解放済みの本体を指したまま残る。
    fn capture_outside_nodes(
        &mut self,
        layout: &Layout,
        chain: &ListChain,
    ) -> (OutsideNodes, OutsideBodies) {
        let mut o = OutsideNodes::EMPTY;
        let mut b = OutsideBodies::EMPTY;
        let mut seen = 0u32;
        for list in 0..2 {
            for &node in chain.list(list) {
                // 範囲の中の節は範囲の書き戻しで戻る。節の先頭が範囲の外ならバックアップする（0x20 B が範囲の境を跨ぐ節は無い見込み）。
                if node == 0 || in_layout(layout, node) {
                    continue;
                }
                seen += 1;
                let Some(w) = self.mem.read_node(node) else {
                    self.st.stats.nodes_unreadable =
                        self.st.stats.nodes_unreadable.saturating_add(1);
                    continue;
                };
                if is_screen_effect_fn(w[1]) && w[7] != 0 && !in_layout(layout, w[7]) {
                    self.capture_body(&mut b, list, node, w[7]);
                }
                if o.n >= OUTSIDE_NODES_CAP {
                    self.st.stats.nodes_over = self.st.stats.nodes_over.saturating_add(1);
                    continue;
                }
                o.addr[o.n] = node;
                o.words[o.n] = w;
                o.n += 1;
            }
        }
        self.st.stats.outside_nodes_max = self.st.stats.outside_nodes_max.max(seen);
        self.st.stats.bodies_max = self.st.stats.bodies_max.max(b.n as u32);
        (o, b)
    }

    /// 範囲の外の ScreenEffect の節 `node`（`list` 0 = 更新の列・1 = 描画の列）の this `body` の本体 0x38 B をバックアップする。
    /// 本体の戻りの欄（更新の節は `+0x08`・描画の節は `+0x0C`）がこの節を指していなければ、`0x422D20` が作った本体ではない
    /// （大きさを推して書くと隣の塊の先頭を壊す）のでバックアップせず、`missed` に数える。
    fn capture_body(&mut self, b: &mut OutsideBodies, list: usize, node: u32, body: u32) {
        let back_off = if list == 0 {
            SCREEN_EFFECT_UPDATE_NODE
        } else {
            SCREEN_EFFECT_DRAW_NODE
        };
        let back = (back_off / 4) as usize;
        if let Some(i) = b.addr.iter().take(b.n).position(|&a| a == body) {
            if b.words.get(i).and_then(|w| w.get(back)) != Some(&node) {
                self.st.stats.bodies_unreadable = self.st.stats.bodies_unreadable.saturating_add(1);
                b.miss(body);
            }
            return;
        }
        if b.n >= OUTSIDE_BODIES_CAP {
            self.st.stats.bodies_over = self.st.stats.bodies_over.saturating_add(1);
            b.miss(body);
            return;
        }
        match self.mem.read_body(body) {
            Some(w) if w.get(back) == Some(&node) => {
                b.addr[b.n] = body;
                b.words[b.n] = w;
                b.n += 1;
            }
            _ => {
                self.st.stats.bodies_unreadable = self.st.stats.bodies_unreadable.saturating_add(1);
                b.miss(body);
            }
        }
    }

    /// コピーの後にアドレスが変わった範囲のうち、元のアドレスの解放を CRT へ渡した・分からないもの（書き戻せない）。無ければ `None`。
    fn moved_not_held(&mut self, layout: &Layout) -> Option<PlaceDiag> {
        let (t, mem) = (&self.st.tables, &mut *self.mem);
        for r in layout.regions() {
            if let Some(d) = region_moved(r, t, mem)
                && !mem.freed_held(d.was)
            {
                return Some(d);
            }
        }
        None
    }

    fn place(&mut self) -> Result<Layout, Fault> {
        let top = self.mem.arena_top();
        if let Some(mut l) = self.st.placed
            && places_unchanged(&l, &self.st.tables, self.mem)
        {
            for r in l.regions.iter_mut().take(l.n) {
                if r.kind == RegionKind::Arena {
                    r.len = top.min(r.cap);
                }
            }
            return Ok(l);
        }
        let mut l = self.st.plan;
        fill_layout(&mut l, &self.st.tables, &mut AsLayout(self.mem), top)
            .map_err(Fault::Layout)?;
        self.st.placed = Some(l);
        self.st.stats.layout_fills = self.st.stats.layout_fills.saturating_add(1);
        self.st.note_vm_arrays(self.mem);
        Ok(l)
    }

    fn elapsed(&mut self, t0: u64) -> u64 {
        self.mem.now_us().saturating_sub(t0)
    }

    /// コピーの中の描画が消す欄を描画の後の値に置き換える（`copy_out` の直後）。進め直しの途中のコピーは描画を通らないので、更新が立てた値
    /// （Stage の色）が残っている。置き換えないと、以後の書き戻しのたびにその値が戻る。コピーの範囲の塊ごとに 1 回呼ぶだけで、確保・ロックはしない。
    fn reset_snap_fields(&mut self, layout: &Layout) {
        for row in self.st.resets.iter().take(self.st.n_resets) {
            let region = layout
                .regions()
                .iter()
                .find(|r| r.kind == RegionKind::Block(row.block));
            let s = &mut self.st.stats;
            let Some(region) = region else {
                s.reset_refused = s.reset_refused.saturating_add(1);
                continue;
            };
            let field = ResetField {
                off: row.off,
                after: row.after,
            };
            match self.mem.reset_in_snap(region, &[field]) {
                Ok(n) => s.reset_changed = s.reset_changed.saturating_add(n),
                Err(_) => s.reset_refused = s.reset_refused.saturating_add(1),
            }
        }
    }

    /// 両陣の Stage の（色の模様, 色の 4 B）をゲームのメモリから読む（`layout` の塊のアドレス。読めない陣は `None`）。
    fn stage_tints(&mut self, layout: &Layout) -> [Option<(u32, u32)>; 2] {
        let mut out = [None; 2];
        for (slot, b) in out.iter_mut().zip(self.st.stage_blocks) {
            let Some(base) = layout
                .regions()
                .iter()
                .find(|r| r.kind == RegionKind::Block(b))
                .map(|r| r.addr)
            else {
                continue;
            };
            let mode = base
                .checked_add(STAGE_TINT_MODE)
                .and_then(|a| self.mem.read_u32(a));
            let tint = base
                .checked_add(STAGE_TINT)
                .and_then(|a| self.mem.read_u32(a));
            if let (Some(mode), Some(tint)) = (mode, tint) {
                *slot = Some((mode, tint));
            }
        }
        out
    }

    /// コピーのときの数え（`copy_out` の直後）: 色の α が 0 でない Stage の数。Present の後のコピーは描画の後なので必ず 0 で、数えるのは進め直しの途中に
    /// カットインの色が立っていたコピーだけ（0 ならこの実行は不具合の形を通っていない）。表を空にした実行でも数える。
    fn note_tint_at_save(&mut self, layout: &Layout) {
        for (_, tint) in self.stage_tints(layout).into_iter().flatten() {
            if tint >> 24 != 0 {
                let s = &mut self.st.stats;
                s.tint_at_save = s.tint_at_save.saturating_add(1);
            }
        }
    }

    /// 書き戻しの直後の確かめ: 両陣の Stage で、色の模様が 0（更新がもう色を書かない）なのに色の α が 0 でない（次の描画が色を掛ける）回数を数える。
    /// 読むのはゲームのメモリ（書き戻した後の本物の値）で、4 語だけ。
    fn check_stage_tint(&mut self, layout: &Layout) {
        for (mode, tint) in self.stage_tints(layout).into_iter().flatten() {
            let s = &mut self.st.stats;
            s.tint_checked = s.tint_checked.saturating_add(1);
            // α は 4 バイトの最上位（`+0x642F`。little endian）。
            if mode == 0 && tint >> 24 != 0 {
                s.tint_stale = s.tint_stale.saturating_add(1);
            }
        }
    }

    /// tick の後に、その tick が積んだ SE を読む（走査の直後。`0x43F1F0` はまだ鳴らしていない）。
    /// - 生きた tick: そのフレームの「鳴らした SE」のバックアップに足す。待ち行列には触らない（次の `0x43F1F0` が鳴らす）。
    /// - 進め直しの tick: 読んで空にし（次の tick が積んだ分だけを読めるように）、前後 2 フレームのどこにも無い id を積み直しの列へ。
    ///   積み直しは `resim_end`（書き戻した出力の後）。
    ///
    /// 被弾音（`rollback_resim::is_hit_se`）は `hit_se` が `Confirmed` のとき、未確定の生きた tick（`confirmed` が偽）が積んだ分を待ち行列から
    /// 抜いて鳴らさない（バックアップの `se_played` にも足さない）。確定の回で初めて出たものとして積み直されて鳴る。進め直しの予測の回で出た分も鳴らさない（`sort_resim_se`）。
    fn note_se(&mut self, t: &Tick, pre: Option<&SeQueue>, confirmed: bool) {
        match (t.kind, pre) {
            (TickKind::Live, Some(pre)) => {
                let mut post = read_se_queue(&mut AsOutputs(&mut *self.mem));
                if !confirmed && self.st.hit_se == HitSe::Confirmed {
                    let st = &mut *self.st;
                    if let Some(rest) =
                        hold_hit_se(&mut st.se_played, &mut st.stats.se, t.frame, pre, &post)
                    {
                        self.rewrite_se_queue(&rest);
                        post = rest;
                    }
                } else if !confirmed {
                    // `live`: 待ち行列には触らず鳴らすが、確定の回が同じ被弾音を出すかを数えるために覚える。
                    let st = &mut *self.st;
                    note_live_hit(&mut st.se_played, &mut st.stats.se, t.frame, pre, &post);
                }
                note_live_se(
                    &mut self.st.se_played,
                    &mut self.st.stats.se,
                    t.frame,
                    pre,
                    &post,
                );
            }
            (TickKind::Confirmed | TickKind::Predicted, _) if self.st.se_armed => {
                let q = read_se_queue(&mut AsOutputs(&mut *self.mem));
                // 読めたかに関わらず空にする（残すと次の tick の分に混ざる）。空にできなければ、この進め直しの SE の経路は止める
                // （`resim_end` の `restore_full` がバックアップの値へ戻す）。
                if !clear_se_queue(&mut AsOutputs(&mut *self.mem)) {
                    self.st.se_armed = false;
                }
                let st = &mut *self.st;
                sort_resim_se(
                    &mut st.se_played,
                    &mut st.se_pending,
                    &mut st.stats.se,
                    t.frame,
                    &q,
                    st.hit_se,
                    t.kind == TickKind::Confirmed,
                );
            }
            _ => {}
        }
    }

    /// 生きた tick の待ち行列を `rest` に書き直す（被弾音を抜いた残り）。パンの欄は書いてよい範囲の外（`output_addr_allowed`）なので、
    /// 枠を詰めるにはゲーム自身の積む関数（`push_se`）で積み直す: 空にして、残りを回数個ずつ（パンは平均。ゲームが鳴らすのも回数個の
    /// 平均なので、鳴り方は変わらない）。積む関数の先頭のバイト列が合わなければ積めず、その分は `refused` に数える。
    fn rewrite_se_queue(&mut self, rest: &SeQueue) {
        if !clear_se_queue(&mut AsOutputs(&mut *self.mem)) {
            let s = &mut self.st.stats.se;
            s.unreadable = s.unreadable.saturating_add(1);
        }
        for (e, count) in rest.events() {
            for _ in 0..count {
                if !self.mem.push_se(e.id, e.pan) {
                    let s = &mut self.st.stats.se;
                    s.refused = s.refused.saturating_add(1);
                    break;
                }
            }
        }
    }

    /// この tick が確定した状態の上の本物の入力ワードの tick か（状態機械の `live_tick` と同じ決め方。進め直しの種類はそのまま）。
    fn is_confirmed_tick(&self, t: &Tick) -> bool {
        match t.kind {
            TickKind::Confirmed => true,
            TickKind::Predicted => false,
            TickKind::Live => {
                let real =
                    t.frame < self.st.hint.remote_next || is_prefill(t.frame, self.st.hint.delay);
                real && t.frame == self.st.front
            }
        }
    }
}

impl Window for GameWindow<'_> {
    fn save(&mut self, frame: u32) {
        if self.st.fault.is_some() {
            return;
        }
        let t0 = self.mem.now_us();
        let layout = match self.place() {
            Ok(l) => l,
            Err(f) => {
                self.st.set_fault(f);
                return;
            }
        };
        let (Some(cur), Some(req)) = (
            self.mem.read_u32(SUP_FIELDS[0]),
            self.mem.read_u32(SUP_FIELDS[1]),
        ) else {
            self.st.set_fault(Fault::Supervisor);
            return;
        };
        self.mem.copy_out(&layout);
        // コピーした後に（コピーする前に置き換えても `copy_out` が上書きする）、コピーの中の描画が消す欄を描画の後の値へ。
        self.note_tint_at_save(&layout);
        self.reset_snap_fields(&layout);
        let mut chain = ListChain::EMPTY;
        let list = self.mem.list_chain(&mut chain);
        let (outside, bodies) = self.capture_outside_nodes(&layout, &chain);
        self.st.snap = Some(Snap {
            frame,
            layout,
            sup: [cur, req],
            list,
            chain,
            outside,
            bodies,
        });
        // コピーの後に生まれた外の塊だけを、次の書き戻しで「予測の間に生まれて生きていた」と数える。
        self.mem.clear_born();
        let mut blocks = [0u32; MAX_REGIONS];
        let mut nb = 0;
        for r in layout.regions() {
            // 領域（Arena）と割り当て器の状態は CRT の塊ではない。塊・要素の配列・VM の配列・データの範囲の先頭だけバックアップする
            // （コピーしない VM の配列はアドレス 0 なのでバックアップしない）。
            if !matches!(r.kind, RegionKind::Arena | RegionKind::ArenaState) && r.addr != 0 {
                blocks[nb] = r.addr;
                nb += 1;
            }
        }
        self.mem.set_stray_refs(
            frame,
            &outside.addr[..outside.n],
            &blocks[..nb],
            &bodies.addr[..bodies.n],
        );
        self.st.front = frame;
        self.st.stats.saves = self.st.stats.saves.saturating_add(1);
        let dt = self.elapsed(t0);
        self.st.stats.save_time.add(dt);
    }

    fn restore(&mut self, frame: u32) {
        if self.st.fault.is_some() {
            return;
        }
        let t0 = self.mem.now_us();
        let Some(snap) = self.st.snap.filter(|s| s.frame == frame) else {
            self.st.set_fault(Fault::NoSnapshot { want: frame });
            return;
        };
        // アドレスが動いた塊・要素の配列は、元のアドレスの解放を CRT へ渡していなければ（`freed_held`）元のアドレスへ書き戻してよい（予測の
        // 間に解放されて作り直された形。元のアドレスを指すポインタは範囲の中で戻る）。渡した・分からないものは書き戻さずに止める。
        let moved = self.moved_not_held(&snap.layout);
        if let Some(d) = moved {
            if self.st.fault.is_none() {
                self.st.place_diag = Some(d);
            }
            self.st.set_fault(Fault::PlacesMoved);
            return;
        }
        // 範囲の外の ScreenEffect の本体: 何かを書く前に確かめる。バックアップできなかった本体があれば、その本体の欄（`+0x0C` など）が戻らず
        // 描画の節が解放済みの本体を指す形が残りうるので止める。バックアップした本体が見張りの外で本当に解放されていれば、書き戻すと
        // 解放済みの所へ書くので書かずに止める（予測の間の解放は CRT へ渡さずバックアップしている。渡したのは見張りの外の経路だけ）。
        if snap.bodies.missed != 0 {
            self.st.set_fault(Fault::BodyNotCaptured {
                addr: snap.bodies.missed_first,
            });
            return;
        }
        for addr in snap.bodies.addr.iter().take(snap.bodies.n) {
            if self.mem.body_freed(*addr) {
                self.st.stats.bodies_freed = self.st.stats.bodies_freed.saturating_add(1);
                self.st.set_fault(Fault::BodyFreed { addr: *addr });
                return;
            }
        }
        let moved_back = first_moved(&snap.layout, &self.st.tables, self.mem).is_some();
        let alive = self.mem.born_len();
        self.st.stats.pred_out_alive_at_restore = self
            .st
            .stats
            .pred_out_alive_at_restore
            .saturating_add(alive);
        self.mem.clear_born();
        // fps の計測の欄を書き戻していないかの確かめ: 書き戻しの前後で描画の回数を読む（.data の 1 語ずつ）。
        let draws_before = self.mem.read_u32(FPS_DRAWS);
        self.mem.copy_in(&snap.layout);
        let s = &mut self.st.stats;
        if let (Some(a), Some(b)) = (draws_before, self.mem.read_u32(FPS_DRAWS)) {
            s.fps_words_checked = s.fps_words_checked.saturating_add(1);
            if a != b {
                s.fps_words_restored = s.fps_words_restored.saturating_add(1);
            }
        }
        let bg = snap
            .layout
            .regions()
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::VmArray(_)) && r.len != 0)
            .count() as u32;
        s.bg_restored = s.bg_restored.saturating_add(bg);
        // Supervisor の 2 欄とつなぎ（レジストリはもう範囲の中で戻った）を戻す。
        let ok = self.mem.write_u32(SUP_FIELDS[0], snap.sup[0])
            & self.mem.write_u32(SUP_FIELDS[1], snap.sup[1]);
        if !ok {
            self.st.set_fault(Fault::Supervisor);
            return;
        }
        // コピーの範囲の外の列の節を書き戻す（予測の tick で外された節は欄が 0 のまま）。
        let o = &snap.outside;
        for (addr, w) in o.addr.iter().zip(o.words.iter()).take(o.n) {
            // 中身がコピーのときと違う節（予測の tick で外された・隣が挿さった）を種類ごとに数える（診断）。
            if self.mem.read_node(*addr).is_some_and(|now| now != *w) {
                self.st.note_changed_node(w);
            }
            if !self.mem.write_node(*addr, w) {
                self.st.stats.nodes_unwritten = self.st.stats.nodes_unwritten.saturating_add(1);
                self.st.set_fault(Fault::NodeNotWritten { addr: *addr });
                return;
            }
        }
        let bo = &snap.bodies;
        for (addr, w) in bo.addr.iter().zip(bo.words.iter()).take(bo.n) {
            if self.mem.read_body(*addr).is_some_and(|now| now != *w) {
                self.st.stats.bodies_changed = self.st.stats.bodies_changed.saturating_add(1);
            }
            if !self.mem.write_body(*addr, w) {
                self.st.stats.bodies_unwritten = self.st.stats.bodies_unwritten.saturating_add(1);
                self.st.set_fault(Fault::BodyNotWritten { addr: *addr });
                return;
            }
            self.st.stats.bodies_restored = self.st.stats.bodies_restored.saturating_add(1);
        }
        if moved_back {
            if let Some(d) = first_moved(&snap.layout, &self.st.tables, self.mem) {
                if self.st.fault.is_none() {
                    self.st.place_diag = Some(d);
                }
                self.st.set_fault(Fault::PlacesMoved);
                return;
            }
            self.st.stats.places_moved_back = self.st.stats.places_moved_back.saturating_add(1);
        }
        let mut now = ListChain::EMPTY;
        if self.mem.list_chain(&mut now) != snap.list {
            if self.st.fault.is_none() {
                self.st.list_diag = list_diverge(&snap.layout, &snap.chain, &now);
            }
            self.st.set_fault(Fault::ListNotRestored);
            return;
        }
        self.check_stage_tint(&snap.layout);
        self.st.front = frame;
        self.st.stats.restores = self.st.stats.restores.saturating_add(1);
        let dt = self.elapsed(t0);
        self.st.stats.restore_time.add(dt);
    }

    fn tick(&mut self, t: Tick) -> TickReport {
        if self.st.fault.is_some() {
            // 壊れた後は走査を呼ばず、境目として状態機械を止める（確定の tick なら場面の終わり、予測なら待ち）。
            return TickReport { boundary: true };
        }
        let confirmed = self.is_confirmed_tick(&t);
        let c0 = if confirmed {
            None
        } else {
            let c = self.mem.out_counts();
            self.mem.set_watch(true);
            self.mem.set_hold_frees(true);
            self.mem.set_pending();
            Some(c)
        };
        let sig0 = self.mem.list_signature();
        // 生きた tick は走査の前の待ち行列も読む（直前の進め直しが遅れて積み直した SE をこの tick の分に数えないため。普段は空で、
        // 読むのは 1 語）。
        let se_pre =
            (t.kind == TickKind::Live).then(|| read_se_queue(&mut AsOutputs(&mut *self.mem)));
        let t0 = self.mem.now_us();
        // 相手の入力ワードも MATCH_SEND_MASK の外を落とす（`netplay::play` の delay 方式と同じ。外のビットは自分の側でも効く: P/Home は
        // ゲームのフォルダへスクリーンショットを書く）。自分の入力ワードは取るときに落としてある。
        let run = self.mem.run_scan(
            Order::Feed {
                p1: t.p1 & MATCH_SEND_MASK,
                p2: t.p2 & MATCH_SEND_MASK,
            },
            t.kind != TickKind::Live,
        );
        let dt = self.elapsed(t0);
        if confirmed {
            // 確定した状態の上の tick だけ、リプレイの印のためにラウンドの境目をその tick の値のまま読む。
            crate::replay_save_win::on_confirmed_tick();
        }
        self.note_se(&t, se_pre.as_ref(), confirmed);
        let s = &mut self.st.stats;
        if let Some(c0) = c0 {
            self.mem.set_hold_frees(false);
            self.mem.set_watch(false);
            let c1 = self.mem.out_counts();
            s.unconfirmed_ticks = s.unconfirmed_ticks.saturating_add(1);
            s.pred_out_allocs = s
                .pred_out_allocs
                .saturating_add(c1.allocs.wrapping_sub(c0.allocs));
            s.pred_out_frees_old = s
                .pred_out_frees_old
                .saturating_add(c1.frees_old.wrapping_sub(c0.frees_old));
            s.pred_out_frees_born = s
                .pred_out_frees_born
                .saturating_add(c1.frees_born.wrapping_sub(c0.frees_born));
            s.pred_out_frees_held = s
                .pred_out_frees_held
                .saturating_add(c1.frees_held.wrapping_sub(c0.frees_held));
        }
        let list_changed = self.mem.list_signature() != sig0;
        let scene = scene_pending(self.mem);
        let class = scan_ret_class(run.ret);
        let s = &mut self.st.stats;
        match t.kind {
            TickKind::Live => {
                s.live_ticks = s.live_ticks.saturating_add(1);
                self.st.last_live = Some(run);
            }
            TickKind::Confirmed => {
                s.resim_confirmed_ticks = s.resim_confirmed_ticks.saturating_add(1);
                s.resim_tick_time.add(dt);
            }
            TickKind::Predicted => {
                s.resim_predicted_ticks = s.resim_predicted_ticks.saturating_add(1);
                s.resim_tick_time.add(dt);
            }
        }
        let s = &mut self.st.stats;
        match class {
            ScanRet::Normal => {}
            ScanRet::Stopped => s.stopped_ticks = s.stopped_ticks.saturating_add(1),
            ScanRet::Abnormal => s.abnormal_ticks = s.abnormal_ticks.saturating_add(1),
        }
        if !run.consumed {
            s.unfed_ticks = s.unfed_ticks.saturating_add(1);
        }
        if scene {
            s.scene_ticks = s.scene_ticks.saturating_add(1);
        }
        let list_hold = list_changed && !confirmed;
        if list_hold {
            s.list_holds = s.list_holds.saturating_add(1);
        } else if list_changed {
            s.list_changes_confirmed = s.list_changes_confirmed.saturating_add(1);
        }
        TickReport {
            boundary: scene || class == ScanRet::Abnormal || !run.consumed || list_hold,
        }
    }

    fn check_values(&mut self) -> CheckValues {
        // delay 方式の CHECK と同じ関数（`net_game::read_check_values`。アドレスと読み方は 1 か所。どれも描画だけの欄に当たらない）。
        struct Read<'a>(&'a mut dyn PlayMem);
        impl CheckRead for Read<'_> {
            fn data(&mut self, addr: u32) -> u32 {
                self.0.read_u32(addr).unwrap_or(0)
            }
            fn safe(&mut self, addr: u32) -> Option<u32> {
                self.0.read_u32(addr)
            }
        }
        let (rng_state, rng_count, life_1p, life_2p) = read_check_values(&mut Read(&mut *self.mem));
        CheckValues {
            rng_state,
            rng_count,
            life_1p,
            life_2p,
        }
    }

    fn resim_begin(&mut self) {
        self.st.in_resim = true;
        self.st.se_pending.clear();
        self.st.se_armed = false;
        if capture_full(&mut AsOutputs(self.mem), &mut self.st.outputs) {
            // バックアップした後で SE の待ち行列を空にして始める（tick ごとに「その tick が積んだ SE」だけを読むため。バックアップの値は `resim_end` が戻す）。
            self.st.se_armed = clear_se_queue(&mut AsOutputs(self.mem));
        } else {
            self.st.stats.outputs_uncaptured = self.st.stats.outputs_uncaptured.saturating_add(1);
        }
    }

    fn resim_end(&mut self) {
        // 進め直しの回の SE・BGM・文字の数は全部捨て、進め直しの前の値へ戻す。バックアップが無ければ書かない。
        let restored = self.st.outputs.valid
            && if restore_full(&mut AsOutputs(self.mem), &self.st.outputs) {
                true
            } else {
                self.st.stats.outputs_unrestored =
                    self.st.stats.outputs_unrestored.saturating_add(1);
                false
            };
        // 戻した後に、進め直しで初めて起きた SE を積み直す（次の do_frame の `0x43F1F0` が鳴らす）。戻せていなければ
        // 待ち行列の中身が分からないので積まない。
        if restored {
            let mem = &mut *self.mem;
            let st = &mut *self.st;
            flush_se(&mut st.se_pending, &mut st.stats.se, &mut |e| {
                mem.push_se(e.id, e.pan)
            });
        } else {
            drop_pending_se(&mut self.st.se_pending, &mut self.st.stats.se);
        }
        self.st.se_armed = false;
        self.st.outputs.valid = false;
        self.st.in_resim = false;
    }

    fn confirmed(&mut self, frame: u32, p1: u16, p2: u16) {
        self.st.front = frame.saturating_add(1);
        // 観戦と記録へ流す行は、ゲームへ feed した語と同じ（マスク後。観戦者の側で外のビットが効かないように）。
        if !self
            .st
            .confirmed
            .push((frame, p1 & MATCH_SEND_MASK, p2 & MATCH_SEND_MASK))
        {
            self.st.stats.confirmed_dropped = self.st.stats.confirmed_dropped.saturating_add(1);
        }
    }

    fn check(&mut self, c: &Check) {
        if !self.st.checks.push(*c) {
            self.st.stats.checks_dropped = self.st.stats.checks_dropped.saturating_add(1);
        }
    }
}

/// 止まっている間に、入力の地点をもう一度試す前に相手の入力ワードを待ってよい理由か（待ちの予算の中で同じフレームの入力の地点を繰り返す形）。
/// この判断はここ 1 か所（`scan_point_with` が使う）。`Wait` は理由で分けず、語を待つ理由だけを true にする。
///
/// 時刻合わせ（`Wait::Sync`。自分が先にいるので描画だけの 1 フレームで待つ。相手の入力ワードを待つ待ちではない）は false: 同じフレームの中で入力の地点を
/// 繰り返してはいけない（待った直後の同じ N の呼び出しは更新まで進むので、予算つきの待ちの中で再試行すると時刻合わせの待ちが時間を取らない）。
pub fn retry_after_wait(why: Wait) -> bool {
    match why {
        Wait::Ahead | Wait::Boundary => true,
        // 内部の食い違い（`Fault`）・取得がまだ（`NotCaptured`）は待たずに描画だけのフレーム。時刻合わせ（`Sync`）は相手の入力ワードを待つ待ちではなく、
        // 同じフレームで試し直すと直後の呼び出しが更新まで進んで待ちが時間を取らない。理由が増えたらここで決める（ワイルドカードを置かない）。
        Wait::NotCaptured | Wait::Fault | Wait::Sync => false,
    }
}

/// 状態機械のインターフェース。本物は `netplay` の rollback の経路（`Connected` を包む —— 取得と同時に INPUT を送るのはセッションの仕事で、状態機械 `Rollback` だけを
/// 外から `&mut` で借りるインターフェースは無い）、検査は [`RbMachine`]。入口の手順はこのインターフェースだけを通して状態機械を進める。
pub trait Machine {
    fn mode(&self) -> Mode;
    fn delay(&self) -> u8;
    fn remote_next(&self) -> u32;
    fn scene_ended(&self) -> bool;
    fn captured(&self) -> bool;
    /// このフレームの自分の入力ワードを取り（1 フレームに 1 回）、すぐ送る。溢れたら `Full`（読んだ入力ワードはインターフェースが持ち越す）。
    fn capture(&mut self) -> Result<(), CaptureError>;
    /// `Rollback::input_point`。本物は待つときの再送もセッションがする。
    fn input_point<W: Window>(&mut self, w: &mut W) -> Step;
    /// 止まっている間に、相手の入力ワードを待つか。true なら入力の地点をもう一度呼ぶ（待ちの予算を使い切った・ネットワークが終わったときは false）。既定は待たない。
    /// 呼ばれるのは [`retry_after_wait`] が true の理由のときと、ゲストが seg の D を知るまでの取得の待ち（`CaptureError::NoDelay`。
    /// `Wait::NotCaptured` で呼ぶ。true なら取得をもう一度試す）。ほかの理由は待たずに描画だけのフレームにする。
    fn wait(&mut self, _why: Wait) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFrame {
    /// このフレームの走査を呼んだ（更新 1 回か、描画だけ）。`ret` を do_frame へ返す。
    Ran { ret: i32, step: Step },
    /// 場面の境目が確定した。走査はまだ呼んでいない（配線は `begin_seg` し、走査を普段どおり 1 回呼ぶ）。
    SceneEnd,
    /// 食い違いで rollback を続けられない。走査はまだ呼んでいない（配線はマッチをやめ、走査を普段どおり 1 回呼ぶ）。
    Broken(Fault),
}

/// 走査の call の包み（`0x0042DCA9`）から、rollback のモードのマッチの間だけ呼ぶ。自分の入力ワードの取得（1 フレームに 1 回）→ 送信 → 状態機械の
/// 入力の地点（止まっていれば書き戻しと進め直しもここ）→ 本物の tick か描画だけのフレーム。止まっている間は、語を待つ理由
/// （`Ahead` / `Boundary`）ならインターフェースが予算まで待ち（[`Machine::wait`]）、届けばもう一度入力の地点で書き戻して進む
/// （delay 方式の「予算つきの待ち ＋ 描画だけのフレーム」と同じ）。
pub fn scan_point_with<M: Machine>(
    m: &mut M,
    st: &mut PlayState,
    mem: &mut dyn PlayMem,
) -> ScanFrame {
    st.stats.frames = st.stats.frames.saturating_add(1);
    if let Some(f) = st.fault {
        st.set_redirect(mem, false);
        return ScanFrame::Broken(f);
    }
    if m.scene_ended() {
        st.set_redirect(mem, false);
        return ScanFrame::SceneEnd;
    }
    st.set_redirect(mem, redirect_wanted(m.mode(), true));
    if !m.captured() {
        let mut got = m.capture();
        // ゲストが seg の D をホストの INPUT で知るまで（`NoDelay`）は、インターフェースが予算まで待ってから同じフレームで取り直す
        // （読み込みの終わりがホストより早いときの、seg の始まりの 1 回だけの待ち）。予算を使い切ったら描画だけのフレーム。
        while got == Err(CaptureError::NoDelay) && m.wait(Wait::NotCaptured) {
            got = m.capture();
        }
        match got {
            Ok(()) | Err(CaptureError::AlreadyCaptured) => {}
            Err(CaptureError::Full | CaptureError::NoDelay) => {
                if got == Err(CaptureError::Full) {
                    st.stats.capture_full = st.stats.capture_full.saturating_add(1);
                }
                let run = mem.run_scan(Order::RenderOnly, false);
                st.stats.render_only = st.stats.render_only.saturating_add(1);
                return ScanFrame::Ran {
                    ret: run.ret,
                    step: Step::Wait(Wait::NotCaptured),
                };
            }
        }
    }
    let step = loop {
        st.hint = Hint {
            remote_next: m.remote_next(),
            delay: m.delay(),
        };
        st.last_live = None;
        let step = m.input_point(&mut GameWindow {
            mem: &mut *mem,
            st: &mut *st,
        });
        if st.fault.is_some() {
            break step;
        }
        match step {
            // 語を待つ理由のときだけ、インターフェースが待ってから同じフレームでもう一度試す（`retry_after_wait`）。
            Step::Wait(why) if retry_after_wait(why) && m.wait(why) => {}
            _ => break step,
        }
    };
    if let Some(f) = st.fault {
        st.set_redirect(mem, false);
        return ScanFrame::Broken(f);
    }
    match step {
        Step::Ran { .. } => match st.last_live {
            Some(run) => ScanFrame::Ran { ret: run.ret, step },
            None => {
                st.set_fault(Fault::NoLiveRun);
                st.set_redirect(mem, false);
                ScanFrame::Broken(Fault::NoLiveRun)
            }
        },
        // 待つ理由は分けず、どれも描画だけのフレーム（今後 `Wait` に理由が増えても同じ）。
        Step::Wait(_) => {
            let run = mem.run_scan(Order::RenderOnly, false);
            st.stats.render_only = st.stats.render_only.saturating_add(1);
            ScanFrame::Ran { ret: run.ret, step }
        }
        Step::SceneEnd => {
            st.set_redirect(mem, false);
            ScanFrame::SceneEnd
        }
    }
}

#[cfg(test)]
pub struct RbMachine<'a> {
    pub rb: &'a mut Rollback,
    pub local: &'a mut dyn FnMut() -> u16,
    pub send: &'a mut dyn FnMut(&Input),
}

#[cfg(test)]
impl Machine for RbMachine<'_> {
    fn mode(&self) -> Mode {
        self.rb.config().mode()
    }
    fn delay(&self) -> u8 {
        self.rb.seg_delay()
    }
    fn remote_next(&self) -> u32 {
        self.rb.remote_next()
    }
    fn scene_ended(&self) -> bool {
        self.rb.scene_ended()
    }
    fn captured(&self) -> bool {
        self.rb.captured()
    }
    fn capture(&mut self) -> Result<(), CaptureError> {
        if !self.rb.delay_known() {
            self.rb.note_delay_wait();
            return Err(CaptureError::NoDelay);
        }
        let r = self.rb.capture((self.local)());
        if r.is_ok() {
            (self.send)(&self.rb.build_input());
        }
        r
    }
    fn input_point<W: Window>(&mut self, w: &mut W) -> Step {
        self.rb.input_point(w)
    }
}

/// [`scan_point_with`] を `Rollback` に直に掛ける形（検査用）。`local` は自分の入力ワードを読む関数（取得が要るフレームだけ呼ぶ）、
/// `send` は INPUT を送る関数。
#[cfg(test)]
pub fn scan_point(
    rb: &mut Rollback,
    st: &mut PlayState,
    mem: &mut dyn PlayMem,
    local: &mut dyn FnMut() -> u16,
    send: &mut dyn FnMut(&Input),
) -> ScanFrame {
    scan_point_with(&mut RbMachine { rb, local, send }, st, mem)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostFrame {
    Done(After),
    Broken(Fault),
}

/// Present の後（Present のラッパの中、描画の後）から呼ぶ。コピーする / F を進める（書き戻し → 確定の入力ワードで更新 → コピー → 予測の入力ワードで
/// 更新）。delay 方式では状態機械が何もしない。`hint` は (相手から次に要る apply_frame, D)、`run` は状態機械の
/// `post_present`（ウィンドウを受け取る）。
pub fn post_present_with(
    hint: (u32, u8),
    st: &mut PlayState,
    mem: &mut dyn PlayMem,
    run: &mut dyn FnMut(&mut GameWindow<'_>) -> After,
) -> PostFrame {
    if let Some(f) = st.fault {
        return PostFrame::Broken(f);
    }
    st.hint = Hint {
        remote_next: hint.0,
        delay: hint.1,
    };
    st.sample_present(mem);
    let t0 = mem.now_us();
    let a = run(&mut GameWindow {
        mem: &mut *mem,
        st: &mut *st,
    });
    if matches!(a, After::Resim { .. }) {
        let dt = mem.now_us().saturating_sub(t0);
        st.stats.resim_frame_time.add(dt);
    }
    match st.fault {
        Some(f) => {
            st.set_redirect(mem, false);
            PostFrame::Broken(f)
        }
        None => PostFrame::Done(a),
    }
}

#[cfg(test)]
pub fn post_present(rb: &mut Rollback, st: &mut PlayState, mem: &mut dyn PlayMem) -> PostFrame {
    let hint = (rb.remote_next(), rb.seg_delay());
    post_present_with(hint, st, mem, &mut |w| rb.post_present(w))
}

pub fn summary_lines(s: &PlayStats, fault: Option<Fault>) -> Vec<String> {
    vec![
        format!(
            "巻き戻し（製品の経路）: フレーム {} のうち描画だけ {}。本物の tick {}・進め直しの tick 確定 {} / 予測 {}（確定していない tick {}）。コピー {}・書き戻し {}・範囲の表を埋めた回 {}",
            s.frames,
            s.render_only,
            s.live_ticks,
            s.resim_confirmed_ticks,
            s.resim_predicted_ticks,
            s.unconfirmed_ticks,
            s.saves,
            s.restores,
            s.layout_fills
        ),
        format!(
            "巻き戻し（製品の経路）: 時間 コピー 平均 {} / 最大 {} ms（n = {}）・書き戻し 平均 {} / 最大 {} ms（n = {}）・進め直しの tick 1 回 平均 {} / 最大 {} ms（n = {}）・進め直しのあった Present の後の全体 平均 {} / 最大 {} ms（n = {}）",
            s.save_time.mean_ms(),
            s.save_time.max_ms(),
            s.save_time.n,
            s.restore_time.mean_ms(),
            s.restore_time.max_ms(),
            s.restore_time.n,
            s.resim_tick_time.mean_ms(),
            s.resim_tick_time.max_ms(),
            s.resim_tick_time.n,
            s.resim_frame_time.mean_ms(),
            s.resim_frame_time.max_ms(),
            s.resim_frame_time.n
        ),
        format!(
            "巻き戻し（製品の経路）: 予測の間の領域の外の確保 {}・解放（前からある塊 {}（うち CRT へ渡さなかった {}） / 予測の間に生まれた塊 {}）・書き戻しの時点で予測の間に生まれて生きていた外の塊 {}",
            s.pred_out_allocs,
            s.pred_out_frees_old,
            s.pred_out_frees_held,
            s.pred_out_frees_born,
            s.pred_out_alive_at_restore
        ),
        format!(
            "巻き戻し（製品の経路）: コピーの範囲の外の列の節 1 回のコピーで最大 {}（控えの枠が足りなかった {}・読めなかった {}・書き戻せなかった {}）・アドレスが動いた塊を元のアドレスへ書き戻した書き戻し {}",
            s.outside_nodes_max,
            s.nodes_over,
            s.nodes_unreadable,
            s.nodes_unwritten,
            s.places_moved_back
        ),
        format!(
            "巻き戻し（製品の経路）: 控えた ScreenEffect の本体（コピーの範囲の外。0x38 B）1 回のコピーで最大 {}・書き戻し {}（うち中身が違っていた {}）・控えの枠が足りなかった {}・読めなかった {}・書き戻せなかった {}・見張りの外で解放されていて止めた {}",
            s.bodies_max,
            s.bodies_restored,
            s.bodies_changed,
            s.bodies_over,
            s.bodies_unreadable,
            s.bodies_unwritten,
            s.bodies_freed
        ),
        format!(
            "巻き戻し（製品の経路）: 境目 場面の要求 {}・確定していない tick の列の変化 {}（確定した tick の変化 {}）・走査が 1 {} / 0 か負 {}・語を取らなかった tick {}。SE ほかを控えられなかった {} / 戻せなかった {}。取得できなかったフレーム {}。落とした確定の語 {} / CHECK {}",
            s.scene_ticks,
            s.list_holds,
            s.list_changes_confirmed,
            s.stopped_ticks,
            s.abnormal_ticks,
            s.unfed_ticks,
            s.outputs_uncaptured,
            s.outputs_unrestored,
            s.capture_full,
            s.confirmed_dropped,
            s.checks_dropped
        ),
        format!(
            "巻き戻し（製品の経路）: 描画が消す欄 表 {} 行・コピーの中で描画の後の値に置き換えた欄 {}（コピー {} 回 × {} 行のうち。断った {}）・コピーのときに Stage の色の α が 0 でなかった {}（コピー × 2 陣のうち。カットインの間のコピー）・書き戻しの直後に Stage の色が残っていた（模様 0 なのに α が 0 でない）{}（見た Stage {} のうち）",
            s.reset_rows,
            s.reset_changed,
            s.saves,
            s.reset_rows,
            s.reset_refused,
            s.tint_at_save,
            s.tint_stale,
            s.tint_checked
        ),
        format!(
            "巻き戻し（製品の経路）: fps の表示（丸めた値 0x4B36C8。Present の後に読んだ {} 回の分布）: {}・平均 {}。書き戻しの前後で描画の回数 0x4B42CC が変わった書き戻し {}（見た {} のうち。fps の計測の欄を書き戻していなければ 0）",
            s.fps_n,
            hist_text(&FPS_LABELS, &s.fps_hist),
            if s.fps_n == 0 {
                "—".to_string()
            } else {
                format!("{:.2}", s.fps_sum as f64 / f64::from(s.fps_n))
            },
            s.fps_words_restored,
            s.fps_words_checked
        ),
        format!(
            "巻き戻し（製品の経路）: 陣の背景（中の名前は Stage）の VM の時計が描画 1 回あたりに進んだ tick（Present × 陣ごとの最大。delay 方式と同じく 1 に集まるのが正しい形）: {}。読めなかった {}。VM の本数 最大 {}（写す上限 {}。越えて写さなかった陣 {}）・書き戻しで背景の VM を書き戻した {}（書き戻し × 陣のうち）",
            hist_text(&BG_STEP_LABELS, &s.bg_step),
            s.bg_unreadable,
            s.bg_count_max,
            BG_VM_MAX,
            s.bg_over,
            s.bg_restored
        ),
        se_summary_line(&s.se),
        match fault {
            None => "巻き戻し（製品の経路）: 食い違い なし".to_string(),
            Some(f) => format!("巻き戻し（製品の経路）: 食い違いで止めた（{}）", f.text()),
        },
    ]
}

/// 分布の文（数が 0 の枠は出さない。全部 0 なら「なし」—— 測れていないことを 0 件の分布に見せない）。
fn hist_text(labels: &[&str], counts: &[u32]) -> String {
    let parts: Vec<String> = labels
        .iter()
        .zip(counts)
        .filter(|&(_, &n)| n != 0)
        .map(|(l, n)| format!("{l}: {n}"))
        .collect();
    if parts.is_empty() {
        "なし（読めた回が 0）".to_string()
    } else {
        parts.join(" / ")
    }
}

pub const STRAY_REC_CAP: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrayRec {
    /// 解放を呼んだ側の戻りアドレス（free `0x47B163` の呼び手。operator delete は `jmp` なので delete の呼び手と推定）。
    pub ret: u32,
    pub ptr: u32,
    pub flags: u32,
    pub frame: u32,
}

impl StrayRec {
    pub const NODE: u32 = 1;
    /// ポインタが、予測の間に CRT へ渡さずバックアップした解放のアドレス（`freed_held`）。
    pub const HELD: u32 = 2;
    /// ポインタが、コピーの表の塊・要素の配列・データの範囲の先頭のアドレス。
    pub const BLOCK: u32 = 4;
    /// ポインタが、コピーのときにバックアップした ScreenEffect の本体のアドレス（これを見張りの外で解放すると、書き戻しは [`Fault::BodyFreed`] で
    /// 止まる）。
    pub const BODY: u32 = 8;
}

/// 解放のバックアップと、範囲の外の節の本ログの文。`stray` は予測の tick の後・書き戻しの前に見張りの外で
/// 起きたゲームのスレッドの領域の外の解放（0 の見込み）、`held_untracked` / `out_untracked` はアドレスをバックアップできなかった解放・確保。
pub fn hold_lines(
    changed: &[ChangedNode],
    other: u32,
    stray: u32,
    held_untracked: u32,
    out_untracked: u32,
    recs: &[StrayRec],
) -> Vec<String> {
    let mut v = vec![format!(
        "巻き戻し（製品の経路）: 予測の後・書き戻しの前に見張りの外で起きた領域の外の解放 {stray}・アドレスを控えられなかった解放 {held_untracked} / 確保 {out_untracked}"
    )];
    let yn = |r: &StrayRec, bit: u32| {
        if r.flags & bit != 0 {
            "はい"
        } else {
            "いいえ"
        }
    };
    for (i, r) in recs.iter().enumerate() {
        v.push(format!(
            "巻き戻し（製品の経路）: 見張りの外の解放 {}件目: 呼び手 {:#010X}・ポインタ {:#010X}・範囲の外の節 {}・解放を控えたアドレス {}・コピーの表の塊 {}・ScreenEffect の本体 {}・コピーのフレーム {}",
            i + 1,
            r.ret,
            r.ptr,
            yn(r, StrayRec::NODE),
            yn(r, StrayRec::HELD),
            yn(r, StrayRec::BLOCK),
            yn(r, StrayRec::BODY),
            r.frame
        ));
    }
    let kinds: Vec<String> = changed
        .iter()
        .filter(|c| c.n != 0)
        .map(|c| {
            format!(
                "毎フレーム {:#010X}・外すとき {:#010X}・頭 {:#010X}・this {:#010X} が {} 回",
                c.on_tick, c.on_remove, c.head, c.this, c.n
            )
        })
        .collect();
    if !kinds.is_empty() || other != 0 {
        v.push(format!(
            "巻き戻し（製品の経路）: 書き戻しのとき中身が変わっていた範囲の外の節（コピーのときの値）: {}{}",
            kinds.join(" / "),
            if other != 0 {
                format!(" / ほか {other} 回")
            } else {
                String::new()
            }
        ));
    }
    v
}

/// rollback の状態機械の数え（`higanr_core::rollback::Stats` の全欄）の本ログの文。`PlayStats`（インターフェース側の数え）とは別の数えで、
/// 待ち・予測・進め直し・保留・食い違い（`faults` が 0 でなければバグ）を接続の合計で出す。
pub fn core_stats_lines(c: &higanr_core::rollback::Stats) -> Vec<String> {
    vec![
        format!(
            "巻き戻し（状態機械）: 入力の地点の更新 確定の状態の上の本物の語 {} / 相手の語は本物で状態は未確定 {} / 予測の語 {}。進め直し {}（うち入力の地点で行った分 {}）・その中の更新 確定 {} / 予測 {}。コピー {}・書き戻し {}",
            c.live_confirmed,
            c.live_unconfirmed_real,
            c.live_predicted,
            c.resims,
            c.stall_rewinds,
            c.resim_confirmed_ticks,
            c.resim_predicted_ticks,
            c.saves,
            c.restores
        ),
        format!(
            "巻き戻し（状態機械）: 待ち 先に行き過ぎ {} / 境目 {} / 時刻合わせ {}（うち seg の頭の 300 フレーム以降 {}）。保留 立てた {}（場面の終わり {} / 消えて続けた {} / 立て直した {}）。捨てた tick {}・取得のやり直し {}・N − F の最大 {}・場面の境目 {}・同じフレームに違う語 {}・内部の食い違い {}",
            c.waits_ahead,
            c.waits_boundary,
            c.waits_sync,
            c.waits_sync_late,
            c.holds_set,
            c.holds_ended,
            c.holds_released,
            c.holds_rearmed,
            c.discarded_ticks,
            c.recaptured,
            c.max_ahead_seen,
            c.scenes_ended,
            c.conflicts,
            c.faults
        ),
        sync_lead_line(c),
        seg_d_line(c),
    ]
}

/// 時刻合わせの見積もりの行（`Stats` の `sync_*`）。見積もり自体は seg の終わりで捨てられるので、標本ごとの平均の値を接続の合計で貯めたもの
/// （平均・最大・最小）を出す。単位はフレーム（正 = 自分が相手より先）。状態機械が貯めた値をそのまま割るだけ（式の原本は
/// `higanr_core::rollback::sync_sample`）。本ログの N − F の最大は上の行（`max_ahead_seen`）と並べて読む。
fn sync_lead_line(c: &higanr_core::rollback::Stats) -> String {
    use higanr_core::rollback::SYNC_UNIT;
    if c.sync_samples == 0 {
        return "巻き戻し（状態機械）: 時刻合わせの見積もり（先にいるフレーム数。正 = 自分が先）: 標本なし".to_string();
    }
    let unit = f64::from(SYNC_UNIT);
    let mean = c.sync_lead_sum as f64 / f64::from(c.sync_samples) / unit;
    format!(
        "巻き戻し（状態機械）: 時刻合わせの見積もり（先にいるフレーム数。正 = 自分が先）: 標本 {} 個・平均 {:.2} / 最大 {:.2} / 最小 {:.2}",
        c.sync_samples,
        mean,
        f64::from(c.sync_lead_max) / unit,
        f64::from(c.sync_lead_min) / unit
    )
}

/// 「seg ごとの D」の行: その seg の D が決まった順の (seg 番号 = D)、新しい方から `SEG_D_LOG` 個までを古い順に。
/// ゲストが D 未定で取得を待った数も添える（seg の始まりで、ホストのその seg の最初の INPUT を待った分）。
fn seg_d_line(c: &higanr_core::rollback::Stats) -> String {
    let n = (c.seg_d_count as usize).min(higanr_core::rollback::SEG_D_LOG);
    let start = if (c.seg_d_count as usize) > n {
        (c.seg_d_count as usize) % higanr_core::rollback::SEG_D_LOG
    } else {
        0
    };
    let mut ds = String::new();
    for k in 0..n {
        let i = (start + k) % higanr_core::rollback::SEG_D_LOG;
        if let Some(&(seg, d)) = c.seg_d_log.get(i) {
            if !ds.is_empty() {
                ds.push(' ');
            }
            ds.push_str(&format!("seg{seg}=D{d}"));
        }
    }
    format!(
        "巻き戻し（状態機械）: seg ごとの D（決まった順。{} 個のうち新しい {} 個）: {}。ゲストが D 未定で取得を待った回 {}",
        c.seg_d_count, n, ds, c.delay_waits
    )
}

pub const SITE_MALLOC: usize = 2;
pub const SITE_NEW: usize = 3;
pub const SITE_FREE: usize = 4;
pub const SITE_CALLOC: usize = SITE_COUNT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayInstall {
    /// rollback を望んでいない実行（何も書かない）。
    NotWanted,
    /// 表の検査か照合に落ちた（1 バイトも書いていない）。
    Refused,
    /// 書いた（malloc・new・free・calloc・走査の call の順。途中の 1 つが書けなくても書けた所は残る）。
    Written([bool; 5]),
}

impl PlayInstall {
    pub fn all_written(&self) -> bool {
        matches!(self, PlayInstall::Written(w) if w.iter().all(|&b| b))
    }
}

pub trait PlayHooks {
    fn entry(&self, site: usize) -> u32;
    fn set_trampoline(&mut self, site: usize, addr: u32);
    fn scan_entry(&self) -> u32;
    fn set_scan_original(&mut self, addr: u32);
}

struct HooksHost<'a>(&'a mut dyn PlayHooks);

impl AllocHost for HooksHost<'_> {
    fn read_ini(&mut self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn game_thread_id(&mut self) -> Option<u32> {
        None
    }
    fn open_outputs(&mut self) -> Result<String, String> {
        Ok(String::new())
    }
    fn prepare_counters(&mut self, _main_tid: u32) {}
    fn entry_address(&self, site: usize) -> u32 {
        self.0.entry(site)
    }
    fn set_trampoline(&mut self, site: usize, addr: u32) {
        self.0.set_trampoline(site, addr);
    }
    fn start_writer(&mut self, _s: &AllocSettings, _i: [bool; SITE_COUNT]) -> Result<(), String> {
        Ok(())
    }
    fn finish(&mut self) -> Option<Vec<String>> {
        None
    }
}

/// 書く地点の一覧（地点の番号・表の行）。並びは `PlayInstall::Written` の並び。
fn play_sites() -> [(usize, &'static SiteDef); 5] {
    [
        (SITE_MALLOC, &ALLOC_SITES[SITE_MALLOC]),
        (SITE_NEW, &ALLOC_SITES[SITE_NEW]),
        (SITE_FREE, &ALLOC_SITES[SITE_FREE]),
        (SITE_CALLOC, &CALLOC_SITES[0]),
        (usize::MAX, &RESIM_SITES[0]),
    ]
}

/// rollback を望んだ実行で、5 地点（確保・解放の 3 入口・calloc・走査の call）を書く。表の検査と 5 地点の今のバイト列の照合が
/// **全部**通ったときだけ書く（1 つでも合わなければ 1 バイトも書かない）。
pub fn install_play(
    wish: Mode,
    env: &mut dyn DetourEnv,
    hooks: &mut dyn PlayHooks,
    forbidden: &[AddrRange],
    out: &mut dyn Write,
) -> PlayInstall {
    if !install_wanted(wish) {
        let _ = writeln!(
            detail_out(out),
            "巻き戻し（製品の経路）: rollback を望んでいない走りなので、確保・解放・calloc・走査の入口は書かない"
        );
        return PlayInstall::NotWanted;
    }
    let sites = play_sites();
    for (site, def) in sites {
        let entry = if site == usize::MAX {
            hooks.scan_entry()
        } else {
            hooks.entry(site)
        };
        if entry == 0 {
            let _ = writeln!(
                out,
                "巻き戻し（製品の経路）: 地点 {} の入口のアドレスが無いので、どこにも書かない",
                def.name
            );
            return PlayInstall::Refused;
        }
        if let Err(e) = check_table(core::slice::from_ref(def), forbidden) {
            let _ = writeln!(
                out,
                "巻き戻し（製品の経路）: 地点 {} の表の検査に落ちた（{e:?}）ので、どこにも書かない",
                def.name
            );
            return PlayInstall::Refused;
        }
    }
    let mut all_match = true;
    let mut checks = Vec::new();
    for (_, def) in sites {
        let mut now = [0u8; higanr_core::patch::MAX_PATCH_LEN];
        let n = def.expected.len().min(now.len());
        let line = if !env.read_code(def.addr, &mut now[..n]) {
            all_match = false;
            "読めない".to_string()
        } else if &now[..n] != def.expected {
            all_match = false;
            format!(
                "不一致（いま {} / 期待 {}）",
                hex_bytes(&now[..n]),
                hex_bytes(def.expected)
            )
        } else {
            "一致".to_string()
        };
        checks.push(format!(
            "巻き戻し（製品の経路）: 照合 {} 0x{:08X}: {line}",
            def.name, def.addr
        ));
    }
    // 全部合ったときは詳細だけ。合わなかったときは、どの地点かを簡潔にも残す。
    {
        let o: &mut dyn Write = if all_match { &mut detail_out(out) } else { out };
        for l in &checks {
            let _ = writeln!(o, "{l}");
        }
    }
    if !all_match {
        let _ = writeln!(
            out,
            "巻き戻し（製品の経路）: 5 地点のうち一致しない地点があるので、どこにも書かない"
        );
        return PlayInstall::Refused;
    }
    let mut written = [false; 5];
    for (i, (site, def)) in sites.into_iter().enumerate() {
        if site == usize::MAX {
            let entry = hooks.scan_entry();
            let r = hook::install_call_hook_at(env, def, entry, &mut |_, original| {
                hooks.set_scan_original(original);
            });
            hook::write_report(out, def, &r);
            written[i] = r.written();
        } else {
            let mut host = HooksHost(&mut *hooks);
            let mut denv = SiteDetourEnv {
                inner: &mut *env,
                host: &mut host,
                site,
            };
            let r = detour::install_detour(&mut denv, def, forbidden);
            detour::write_report(out, def, &r);
            written[i] = r.written();
        }
    }
    let o: &mut dyn Write = if written.iter().all(|&w| w) {
        &mut detail_out(out)
    } else {
        out
    };
    let _ = writeln!(
        o,
        "巻き戻し（製品の経路）: 書いた地点 {}",
        sites
            .iter()
            .zip(written)
            .map(|((_, d), w)| format!("{}={}", d.name, u8::from(w)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    PlayInstall::Written(written)
}

/// 観戦者の早送りのために、走査の call（`RESIM_SITES[0]`）の 1 地点だけを書く。観戦者の早送り（`spectate_watch/ff.rs`）は、走査の包みの中で
/// 余分に更新を回すだけで、観戦するマッチの方式（rollback / delay）と関わらない。一方、走査の call は `install_play` でしか書かれず、ini が delay の
/// 実行には書かれていなかった。そこで観戦者の実行（`watching`）は、走査の call がまだ書かれていなければ（`scan_hooked` が偽）そこだけを書く。
/// 確保・解放・calloc の入口は書かない（観戦者は巻き戻さない）。観戦者でない実行（対戦者）は何も書かない —— delay 方式の対戦者の実行は 1 バイトも
/// 書かない約束（遅延の増分 0）を、この関数の入口で守る。書く前の照合は `install_call_hook_at` が止めた後の再読みまで含めて行い、合わなければ何も書かない。
/// 書いたら true。
pub fn install_watch_scan(
    watching: bool,
    scan_hooked: bool,
    env: &mut dyn hook::HookEnv,
    hooks: &mut dyn PlayHooks,
    forbidden: &[AddrRange],
    out: &mut dyn Write,
) -> bool {
    if !watching || scan_hooked {
        return false;
    }
    let def = &RESIM_SITES[0];
    let entry = hooks.scan_entry();
    if entry == 0 {
        let _ = writeln!(
            out,
            "観戦者の早送り: 走査の包みの入口のアドレスが無いので、どこにも書かない"
        );
        return false;
    }
    if let Err(e) = check_table(core::slice::from_ref(def), forbidden) {
        let _ = writeln!(
            out,
            "観戦者の早送り: 走査の call の表の検査に落ちた（{e:?}）ので、どこにも書かない"
        );
        return false;
    }
    let r = hook::install_call_hook_at(env, def, entry, &mut |_, original| {
        hooks.set_scan_original(original);
    });
    hook::write_report(out, def, &r);
    r.written()
}

#[cfg(test)]
pub(crate) mod tests {
    //! 偽のゲーム（`FakeGame`。語のアドレス → 値の表で、コピーは範囲の中の語だけをコピーする）と本物の状態機械 `Rollback` で、ウィンドウと入口の
    //! 手順を見る。設置は偽の命令の表（`FakeCode`）で見る。
    use super::*;
    use crate::hook::{HookEnv, ThreadRef, WriteDone};
    use crate::rollback_probe::{RbData, RbSkipKind, RegionKind};
    use crate::rollback_resim::SE_NEAR;
    use crate::rollback_testkit::{
        ARENA_BASE, ARENA_BYTES, FakeGame, LIST_OUTSIDE, STATE, TINT_TICKS, play_state,
    };
    use crate::th09_addr::{
        ASCII_COUNT, FORBIDDEN, PACING_SITES, REGISTRY_TAIL_OBJ, ROLLBACK_BLOCKS, ROLLBACK_DATA,
        ROLLBACK_EXCLUDED_CALLERS, ROLLBACK_PLAY_DATA, SE_ID_CUTIN, SE_ID_HIT, SE_ID_LAST_LIFE,
        SITES, SND_BGM_QUEUE, SND_SE_COUNTS, SND_SE_IDS, SOUND_MANAGER, STAGE_TINT_AFTER,
        STAGE_TINT_RESET_AT, STAGE_TINT_RESET_CODE, UPDATE_REGISTRY_BYTES,
    };
    use crate::ticks::CopyRow;
    use higanr_core::rollback::Config;
    use higanr_core::wire::{MAX_WORDS, Seat};
    use std::collections::BTreeMap;

    const NOTE: &str = include_str!("../../../analysis/notes/th09_rollback_E1.md");

    fn remote(rb: &Rollback, base: u32, words: &[u16]) -> Input {
        let mut w = [0u16; MAX_WORDS];
        w[..words.len()].copy_from_slice(words);
        Input {
            seg: rb.seg(),
            ack: rb.frame(),
            base,
            lat: None,
            count: words.len() as u8,
            words: w,
        }
    }

    fn frame(
        rb: &mut Rollback,
        st: &mut PlayState,
        g: &mut FakeGame,
        local: u16,
    ) -> (ScanFrame, PostFrame) {
        let mut sent = 0u32;
        let f = scan_point(rb, st, g, &mut || local, &mut |_| sent += 1);
        g.play_and_draw();
        let p = post_present(rb, st, g);
        (f, p)
    }

    #[test]
    fn a_guest_without_the_seg_d_renders_only_and_reads_nothing_until_the_hosts_input() {
        use higanr_core::lockstep::Role;
        use higanr_core::wire::LatChange;
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_role(Role::Guest));
        let mut st = play_state();
        let mut g = FakeGame::new();
        rb.begin_seg(1);
        let (mut reads, mut sent) = (0u32, 0u32);
        for k in 0..3 {
            let f = scan_point(
                &mut rb,
                &mut st,
                &mut g,
                &mut || {
                    reads += 1;
                    0x41
                },
                &mut |_| sent += 1,
            );
            assert!(
                matches!(
                    f,
                    ScanFrame::Ran {
                        step: Step::Wait(Wait::NotCaptured),
                        ..
                    }
                ),
                "{k}: {f:?}"
            );
            g.play_and_draw();
        }
        assert_eq!((reads, sent), (0, 0), "D 未定の間に読んだ・送った");
        assert!(
            g.scans.iter().all(|(order, _)| *order == Order::RenderOnly),
            "D 未定の間に更新を走らせた: {:?}",
            g.scans
        );
        assert_eq!(rb.stats().live_confirmed + rb.stats().live_predicted, 0);
        let mut words = [0u16; MAX_WORDS];
        words[0] = 0x77;
        let host = Input {
            seg: 1,
            ack: 0,
            base: 1,
            lat: Some(LatChange { frame: 0, delay: 1 }),
            count: 1,
            words,
        };
        assert!(matches!(
            rb.on_input(&host),
            higanr_core::lockstep::RecvOutcome::Accepted { .. }
        ));
        let f = scan_point(
            &mut rb,
            &mut st,
            &mut g,
            &mut || {
                reads += 1;
                0x41
            },
            &mut |_| sent += 1,
        );
        assert!(
            matches!(
                f,
                ScanFrame::Ran {
                    step: Step::Ran { frame: 0, .. },
                    ..
                }
            ),
            "{f:?}"
        );
        assert_eq!((reads, sent), (1, 1), "D を知った後に取らない");
    }

    #[test]
    fn a_guest_waiting_for_the_seg_d_retries_in_the_same_frame_after_the_budgeted_wait() {
        use higanr_core::lockstep::Role;
        use higanr_core::wire::LatChange;
        struct Waiting<'a> {
            inner: RbMachine<'a>,
            host: Option<Input>,
            waits: Vec<Wait>,
        }
        impl Machine for Waiting<'_> {
            fn mode(&self) -> Mode {
                self.inner.mode()
            }
            fn delay(&self) -> u8 {
                self.inner.delay()
            }
            fn remote_next(&self) -> u32 {
                self.inner.remote_next()
            }
            fn scene_ended(&self) -> bool {
                self.inner.scene_ended()
            }
            fn captured(&self) -> bool {
                self.inner.captured()
            }
            fn capture(&mut self) -> Result<(), CaptureError> {
                self.inner.capture()
            }
            fn input_point<W: Window>(&mut self, w: &mut W) -> Step {
                self.inner.input_point(w)
            }
            fn wait(&mut self, why: Wait) -> bool {
                self.waits.push(why);
                match self.host.take() {
                    Some(i) => {
                        self.inner.rb.on_input(&i);
                        true
                    }
                    None => false,
                }
            }
        }
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_role(Role::Guest));
        rb.begin_seg(1);
        let mut st = play_state();
        let mut g = FakeGame::new();
        let mut words = [0u16; MAX_WORDS];
        words[0] = 0x77;
        let host = Input {
            seg: 1,
            ack: 0,
            base: 1,
            lat: Some(LatChange { frame: 0, delay: 1 }),
            count: 1,
            words,
        };
        let (mut reads, mut sent) = (0u32, 0u32);
        let mut local = || {
            reads += 1;
            0x41
        };
        let mut send = |_: &Input| sent += 1;
        let mut m = Waiting {
            inner: RbMachine {
                rb: &mut rb,
                local: &mut local,
                send: &mut send,
            },
            host: Some(host),
            waits: Vec::new(),
        };
        let f = scan_point_with(&mut m, &mut st, &mut g);
        assert_eq!(m.waits, vec![Wait::NotCaptured], "待ちの理由と回数");
        assert!(
            matches!(
                f,
                ScanFrame::Ran {
                    step: Step::Ran { frame: 0, .. },
                    ..
                }
            ),
            "待った後、同じフレームで取って更新する: {f:?}"
        );
        let mut rb2 = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_role(Role::Guest));
        rb2.begin_seg(1);
        let mut local2 = || 0x41;
        let mut send2 = |_: &Input| {};
        let mut m2 = Waiting {
            inner: RbMachine {
                rb: &mut rb2,
                local: &mut local2,
                send: &mut send2,
            },
            host: None,
            waits: Vec::new(),
        };
        let f = scan_point_with(&mut m2, &mut st, &mut g);
        assert_eq!(m2.waits, vec![Wait::NotCaptured]);
        assert!(
            matches!(
                f,
                ScanFrame::Ran {
                    step: Step::Wait(Wait::NotCaptured),
                    ..
                }
            ),
            "{f:?}"
        );
        drop(m);
        assert_eq!((reads, sent), (1, 1));
    }

    #[test]
    fn gates_are_closed_in_delay_mode() {
        for controlled in [false, true] {
            assert!(!redirect_wanted(Mode::Delay, controlled), "{controlled}");
        }
        assert!(!redirect_wanted(Mode::Rollback, false));
        assert!(redirect_wanted(Mode::Rollback, true));
        assert!(!install_wanted(Mode::Delay));
        assert!(install_wanted(Mode::Rollback));
    }

    #[test]
    fn delay_mode_never_redirects_watches_or_snapshots() {
        for mode in [Mode::Delay, Mode::Rollback] {
            let cfg = match mode {
                Mode::Delay => Config::delay_mode(Seat::P1, 1),
                Mode::Rollback => Config::rollback(Seat::P1, 1, 8),
            };
            let mut rb = Rollback::new(cfg);
            let mut st = play_state();
            let mut g = FakeGame::new();
            let mut ran = 0;
            for k in 0..240u32 {
                if k >= 3 {
                    let base = k - 2;
                    let inp = remote(&rb, base, &[(base as u16) & 0x3F]);
                    rb.on_input(&inp);
                }
                let (f, _) = frame(&mut rb, &mut st, &mut g, (k as u16) & 0x7);
                if matches!(
                    f,
                    ScanFrame::Ran {
                        step: Step::Ran { .. },
                        ..
                    }
                ) {
                    ran += 1;
                }
            }
            assert!(ran > 100, "{mode:?}: 進んだ tick {ran}");
            assert_eq!(st.fault(), None, "{mode:?}");
            match mode {
                Mode::Delay => {
                    assert_eq!(g.redirect_on_calls, 0, "delay 方式で確保の向け先を開いた");
                    assert_eq!(g.watch_on_calls, 0, "delay 方式で見張りを立てた");
                    assert_eq!((g.copy_outs, g.copy_ins), (0, 0), "delay 方式でコピーした");
                    assert_eq!(st.stats().saves, 0);
                    assert!(
                        g.scans.iter().all(|(_, resim)| !resim),
                        "delay 方式で進め直した"
                    );
                }
                Mode::Rollback => {
                    assert!(
                        g.redirect_on_calls > 0,
                        "rollback のモードでゲートが開かない"
                    );
                    assert!(
                        g.copy_outs > 0 && g.copy_ins > 0,
                        "rollback のモードで写していない"
                    );
                    assert!(g.watch_on_calls > 0, "予測の間に見張りを立てていない");
                }
            }
        }
    }

    #[test]
    fn restore_brings_back_the_registry_and_the_supervisor_fields() {
        let mut g = FakeGame::new();
        let mut st = play_state();
        g.mem.insert(REGISTRY_TAIL_OBJ + 8, 0x5AB);
        let reg0 = g.registry();
        let state0 = g.get(STATE);
        {
            let mut w = GameWindow {
                mem: &mut g,
                st: &mut st,
            };
            w.save(10);
        }
        for off in (0..UPDATE_REGISTRY_BYTES).step_by(4) {
            g.mem.insert(UPDATE_REGISTRY + off, 0xB000 + off);
        }
        g.mem.insert(SUP_FIELDS[0], 3);
        g.mem.insert(SUP_FIELDS[1], 5);
        g.mem.insert(STATE, 999);
        g.mem.insert(REGISTRY_TAIL_OBJ + 8, 0x7AB);
        {
            let mut w = GameWindow {
                mem: &mut g,
                st: &mut st,
            };
            w.restore(10);
        }
        assert_eq!(st.fault(), None);
        assert_eq!(g.registry(), reg0, "つなぎの 0x40 B が戻らない");
        assert_eq!(
            (g.get(SUP_FIELDS[0]), g.get(SUP_FIELDS[1])),
            (2, 2),
            "Supervisor の 2 欄"
        );
        assert_eq!(g.get(STATE), state0);
        assert_eq!(st.front, 10);
        let placed = st.snap.expect("写しがある").layout;
        for a in SUP_FIELDS {
            assert!(
                !FakeGame::in_regions(&placed, a),
                "{a:08X} がコピーの範囲にある（検査の前提が崩れた）"
            );
        }
        assert!(FakeGame::in_regions(&placed, UPDATE_REGISTRY + 0x3C));
        // レジストリの後ろの 16 B（ファイルの読み込みの処理と見られる別の物）はコピーしない（持ち物のハンドルを書き戻さない）。
        assert!(!FakeGame::in_regions(&placed, REGISTRY_TAIL_OBJ));
        assert_eq!(
            g.get(REGISTRY_TAIL_OBJ + 8),
            0x7AB,
            "後ろの 16 B を書き戻した"
        );
    }

    #[test]
    fn without_the_registry_row_the_list_mismatch_is_caught() {
        let mut g = FakeGame::new();
        let mut st = PlayState::new(Tables::th09(), ARENA_BASE, ARENA_BYTES).unwrap();
        {
            let mut w = GameWindow {
                mem: &mut g,
                st: &mut st,
            };
            w.save(4);
        }
        g.mem.insert(UPDATE_REGISTRY + 0x14, 0xDEAD);
        {
            let mut w = GameWindow {
                mem: &mut g,
                st: &mut st,
            };
            w.restore(4);
        }
        assert_eq!(
            g.get(UPDATE_REGISTRY + 0x14),
            0xDEAD,
            "E1 の表でつなぎが戻った（対照が崩れた）"
        );
        assert_eq!(st.fault(), Some(Fault::ListNotRestored));
    }

    #[test]
    fn list_diverge_names_the_node_whose_next_was_not_put_back() {
        let reg = UPDATE_REGISTRY;
        let draw = UPDATE_REGISTRY + UPDATE_DRAW_HEAD;
        let (sup, a, b) = (SUPERVISOR + 0x40, 0x0071_0000u32, 0x0071_0100u32);
        let chain_of = |m: &BTreeMap<u32, u32>| {
            let mut c = ListChain::EMPTY;
            let h = list_chain_with(&mut |x| Some(m.get(&x).copied().unwrap_or(0)), &mut c);
            assert_eq!(
                h,
                list_signature_with(&mut |x| Some(m.get(&x).copied().unwrap_or(0))),
                "控えつきのダイジェストが違う"
            );
            c
        };
        let mut m = BTreeMap::from([(reg + UPDATE_NEXT, sup), (sup + UPDATE_NEXT, a)]);
        let layout = *play_state().plan();
        let want = chain_of(&m);
        assert_eq!(want.list(0), &[reg, sup, a]);
        assert_eq!(want.list(1), &[draw]);
        assert_eq!(list_diverge(&layout, &want, &want), None, "同じ並び");
        m.insert(sup + UPDATE_NEXT, b);
        m.insert(b + UPDATE_NEXT, a);
        let d = list_diverge(&layout, &want, &chain_of(&m)).expect("分かれた");
        assert_eq!(
            d,
            ListDiag {
                list: 0,
                index: 2,
                before: sup,
                want: a,
                got: b,
                before_in_snap: false,
                want_len: 3,
                got_len: 4,
            }
        );
        assert!(d.text().contains("コピーの範囲の外"), "{}", d.text());
        let mut m2 = BTreeMap::from([(reg + UPDATE_NEXT, a), (draw + UPDATE_NEXT, b)]);
        let d2 = list_diverge(&layout, &want, &chain_of(&m2)).expect("分かれた");
        assert_eq!((d2.list, d2.index, d2.before, d2.got), (0, 1, reg, a));
        assert!(d2.before_in_snap, "レジストリはコピーの範囲の中");
        m2.insert(reg + UPDATE_NEXT, sup);
        m2.insert(sup + UPDATE_NEXT, a);
        let d3 = list_diverge(&layout, &want, &chain_of(&m2)).expect("描画の列が分かれた");
        assert_eq!(
            (d3.list, d3.index, d3.before, d3.want, d3.got),
            (1, 1, draw, 0, b)
        );
    }

    #[test]
    fn restore_refuses_what_it_cannot_put_back() {
        let mut g = FakeGame::new();
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        g.mem.insert(LIST_OUTSIDE, 8);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::ListNotRestored));
        let outs = g.copy_outs;
        let scans = g.scans.len();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(2);
        let rep = GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .tick(Tick {
            frame: 2,
            p1: 0,
            p2: 0,
            kind: TickKind::Live,
        });
        assert_eq!(
            (g.copy_outs, g.scans.len(), rep.boundary),
            (outs, scans, true)
        );

        let mut g = FakeGame::new();
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        g.mem.insert(ROLLBACK_BLOCKS[0].ptr_at, 0x0F00_0000);
        let ins = g.copy_ins;
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::PlacesMoved));
        assert_eq!(g.copy_ins, ins, "アドレスが変わったのに書き戻した");
        let d = st.place_diag().expect("動いた範囲を記録していない");
        assert_eq!((d.kind, d.now), (RegionKind::Block(0), Some(0x0F00_0000)));
        assert_ne!(d.was, 0x0F00_0000);
        let t = d.text(&Tables::th09_play());
        assert!(
            t.contains(ROLLBACK_BLOCKS[0].name) && t.contains("0x0F000000"),
            "{t}"
        );

        let mut g = FakeGame::new();
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(2);
        assert_eq!(st.fault(), Some(Fault::NoSnapshot { want: 2 }));
    }

    #[test]
    fn resim_outputs_return_to_the_values_before_the_resim() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8));
        let mut st = play_state();
        let mut g = FakeGame::new();
        for k in 0..4u32 {
            let (f, _) = frame(&mut rb, &mut st, &mut g, 0x10 + k as u16);
            assert!(matches!(f, ScanFrame::Ran { .. }), "{k}: {f:?}");
        }
        assert_eq!((rb.frame(), rb.front()), (4, 1));
        g.mem.insert(SOUND_MANAGER + SND_SE_IDS + 5 * 4, 0x33);
        g.mem.insert(SOUND_MANAGER + SND_SE_COUNTS + 5 * 4, 2);
        g.mem.insert(ASCII_COUNT, 7);
        g.mem.insert(SOUND_MANAGER + SND_BGM_QUEUE, 0x55);
        let before = g.outputs();
        let writes_before = g.writes.len();
        let resim_before = g.se_in_resim;
        let inp = remote(&rb, 1, &[0x21, 0x22, 0x23]);
        rb.on_input(&inp);
        let p = post_present(&mut rb, &mut st, &mut g);
        assert_eq!(
            p,
            PostFrame::Done(After::Resim {
                from: 1,
                confirmed: 3,
                predicted: 0
            })
        );
        assert_eq!(
            g.se_in_resim - resim_before,
            3,
            "進め直しの回が SE を積んでいない（検査の前提）"
        );
        // 進め直しの回が積んだ SE は捨て、進め直しの前の値へ戻す。ただし進め直しで初めて起きた SE（前後 2 フレームのどこにも無い id）だけは
        // `resim_end` が戻した後に積み直す。それ以外 —— BGM の待ち行列・文字の数・進め直しの前に積まれていた SE（枠 5 の id 0x33・回数 2）—— は進め直しの前のまま。
        let pushed: Vec<u32> = g.se_pushed.iter().map(|&(id, _)| id).collect();
        assert!(
            !pushed.is_empty(),
            "進め直しで初めて出た SE が 1 つも無い（検査の前提）"
        );
        assert_eq!(st.stats().se.replayed as usize, pushed.len());
        let (se_before, rest_before) = before.split_at(24);
        let after = g.outputs();
        let (se_after, rest_after) = after.split_at(24);
        assert_eq!(rest_after, rest_before, "BGM・文字の数が戻らない");
        assert_eq!(
            &se_after[10..12],
            &se_before[10..12],
            "進め直しの前に積まれていた SE が戻らない"
        );
        let queued: Vec<u32> = g.se_queue().iter().map(|&(id, _)| id).collect();
        let mut want = pushed.clone();
        want.dedup();
        assert_eq!(
            queued, want,
            "待ち行列は、積み直した SE のほかに進め直しの回の SE が残っている"
        );
        let ascii_writes = g.writes[writes_before..]
            .iter()
            .filter(|(a, _)| *a == ASCII_COUNT)
            .count();
        assert_eq!(ascii_writes, 1, "戻しの回数");
        assert!(!st.in_resim());
        assert_eq!(st.stats().outputs_unrestored, 0);
        let mut conf = Vec::new();
        st.drain_confirmed(&mut |f, p1, p2| conf.push((f, p1, p2)));
        assert_eq!(
            conf,
            vec![(0, 0, 0), (1, 0x10, 0x21), (2, 0x11, 0x22), (3, 0x12, 0x23)]
        );
    }

    #[test]
    fn a_resim_tick_without_begin_and_end_changes_the_outputs() {
        let mut g = FakeGame::new();
        let mut st = play_state();
        let before = g.outputs();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .tick(Tick {
            frame: 1,
            p1: 1,
            p2: 2,
            kind: TickKind::Predicted,
        });
        assert_ne!(g.outputs(), before);
    }

    /// D = 1・R = 8 の rollback を `loops` フレーム回す（`frame` 1 回 = 走査の包み → 鳴らす → Present の後）。`feed(rb, k)` は k 回目の
    /// 走査の前に相手の入力ワードを届ける関数、`local(k)` は k 回目に取る自分の入力ワード。戻り値は実行し終わった後の (rb, st)。
    fn run_late(
        g: &mut FakeGame,
        loops: u32,
        feed: &dyn Fn(&mut Rollback, u32),
        local: &dyn Fn(u32) -> u16,
    ) -> (Rollback, PlayState) {
        // 偽の相手は ack を自分の N のまま返し、語だけ 3 フレーム遅れて届ける（往路だけ遅いネットワーク）。この検査は予測・書き戻し・進め直しが毎フレーム
        // 起きる実行を見るので、時刻合わせは切る（時刻合わせの検査は `higanr_core` の `tests/rollback_sim.rs`）。
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_time_sync(false));
        let mut st = play_state();
        rb.begin_seg(1);
        for k in 0..loops {
            feed(&mut rb, k);
            let (f, p) = frame(&mut rb, &mut st, g, local(k));
            assert!(matches!(f, ScanFrame::Ran { .. }), "{k}: {f:?}");
            assert!(matches!(p, PostFrame::Done(_)), "{k}: {p:?}");
        }
        assert_eq!(st.fault(), None);
        (rb, st)
    }

    /// 相手の入力ワードを 1 ワードずつ 3 フレーム遅れて届ける（k 回目に apply_frame k − 2 の入力ワード。予測は直前の確定の入力ワードの繰り返しなので、変わり目は
    /// 届いた次の tick で予測に入り、本当の変わり目より 2 フレーム遅れて生きた tick で起きる）。
    fn feed_one_by_one(words: fn(u32) -> u16) -> impl Fn(&mut Rollback, u32) {
        move |rb, k| {
            if k >= 3 {
                let base = k - 2;
                let inp = remote(rb, base, &[words(base)]);
                rb.on_input(&inp);
            }
        }
    }

    /// 相手の入力ワードを 4 ワードずつまとめて届ける（apply_frame 4j + 1〜4j + 4 を j × 4 + 6 回目に。回線の揺れで 1 つ遅れた形）。まとまりの中で
    /// 変わって戻る語は、予測（まとまりの最後の語の繰り返し）には現れず、進め直しの確定の回で初めて出る。
    fn feed_in_bursts(words: fn(u32) -> u16) -> impl Fn(&mut Rollback, u32) {
        move |rb, k| {
            if k >= 6 && (k - 6).is_multiple_of(4) {
                let base = (k - 6) / 4 * 4 + 1;
                let ws: Vec<u16> = (base..base + 4).map(words).collect();
                let inp = remote(rb, base, &ws);
                rb.on_input(&inp);
            }
        }
    }

    /// 相手の入力ワードは 10 フレームごとに 3 → 5 → 0 と変わる（apply_frame 10・20・30 が変わり目）。
    fn remote_steps(b: u32) -> u16 {
        match b {
            0..=9 => 0,
            10..=19 => 3,
            20..=29 => 5,
            _ => 0,
        }
    }

    /// 相手の入力ワードは 12 フレームごとに 1 フレームだけ 3（apply_frame 10・22・34）。4 ワードずつのまとまり（9〜12・21〜24・33〜36）の中で
    /// 上がって下がる。
    fn remote_pulses(b: u32) -> u16 {
        if matches!(b, 10 | 22 | 34) { 3 } else { 0 }
    }

    #[test]
    fn a_remote_event_first_seen_in_a_resim_is_played_once_soon_after() {
        const LOOPS: u32 = 50;
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_change = Some(0x20);
        let (rb, st) = run_late(&mut g, LOOPS, &feed_in_bursts(remote_pulses), &|_| 0);
        assert!(
            rb.stats().resims > 5,
            "進め直しが起きていない（検査の前提）"
        );
        assert_eq!(
            g.played_ids(),
            vec![0x23, 0x20, 0x23, 0x20, 0x23, 0x20],
            "出来事ごとにちょうど 1 回鳴らない"
        );
        for (&(draw, id, count), f) in g.se_log.iter().zip([10u32, 11, 22, 23, 34, 35]) {
            assert_eq!(count, 1, "id {id:#X} の回数");
            assert!(
                draw > f && draw - f <= 8 + 1 + 1,
                "id {id:#X}: 出来事のフレーム {f} の後 {} 回目の do_frame で鳴った",
                i64::from(draw) - i64::from(f)
            );
        }
        let se = st.stats().se;
        assert_eq!((se.replayed, se.refused, se.dropped), (6, 0, 0), "{se:?}");
        assert_eq!(se.new_ids, 6, "{se:?}");
        assert_eq!(
            se.live_ids, 0,
            "生きた tick では起きていない（予測は繰り返し）"
        );
        assert!(se.late_max <= 8 + 1, "{se:?}");

        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_change = Some(0x20);
        g.se_refuse = true;
        let (_, st) = run_late(&mut g, LOOPS, &feed_in_bursts(remote_pulses), &|_| 0);
        assert_eq!(g.played_ids(), Vec::<u32>::new(), "断ったのに鳴った");
        assert_eq!(st.stats().se.refused, 6);
        assert_eq!(st.stats().se.replayed, 0);
    }

    #[test]
    fn a_live_event_shifted_by_the_prediction_is_not_played_twice() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_change = Some(0x20);
        let (rb, st) = run_late(&mut g, 60, &feed_one_by_one(remote_steps), &|_| 0);
        assert!(
            rb.stats().resims > 10,
            "進め直しが起きていない（検査の前提）"
        );
        assert_eq!(
            g.played_ids(),
            vec![0x23, 0x25, 0x20],
            "出来事ごとにちょうど 1 回鳴らない"
        );
        let se = st.stats().se;
        assert_eq!((se.live_ids, se.new_ids, se.replayed), (3, 0, 0), "{se:?}");
        assert!(se.suppressed >= 3, "{se:?}");
    }

    #[test]
    fn a_local_event_is_played_once_live_and_not_again_by_the_resims() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p1_change = Some(0x40);
        let (rb, st) = run_late(&mut g, 60, &feed_one_by_one(|_| 0), &|k| {
            ((k / 6) as u16 + 1).min(15)
        });
        assert!(
            rb.stats().resims > 10,
            "進め直しが起きていない（検査の前提）"
        );
        let played = g.played_ids();
        assert!(played.len() >= 8, "自分の出来事が鳴っていない: {played:?}");
        let want: Vec<u32> = (0..played.len() as u32).map(|i| 0x41 + i).collect();
        assert_eq!(played, want, "二重に鳴った・抜けた・順が違う");
        let se = st.stats().se;
        assert_eq!(se.live_ids as usize, played.len(), "{se:?}");
        assert!(
            se.suppressed + 4 >= se.live_ids && se.suppressed >= 5,
            "進め直しがその tick を走らせ直して抑えた数が見えていない: {se:?}"
        );
        assert_eq!((se.new_ids, se.replayed, se.refused), (0, 0, 0), "{se:?}");
    }

    /// 進め直し 1 回（ウィンドウを直に使う）: `ticks` の (種類, フレーム, p2) を順に tick し、`resim_begin` と `resim_end` で挟む。
    fn resim_once(g: &mut FakeGame, st: &mut PlayState, ticks: &[(TickKind, u32, u16)]) {
        GameWindow { mem: g, st }.resim_begin();
        for &(kind, frame, p2) in ticks {
            GameWindow { mem: g, st }.tick(Tick {
                frame,
                p1: 0,
                p2,
                kind,
            });
        }
        GameWindow { mem: g, st }.resim_end();
    }

    /// 壊れたら落ちる: リプレイの印のラウンドの境目を読む関数が、確定した tick の後だけ呼ばれる（予測の tick の境目は書き戻しで消える）。
    #[test]
    fn the_round_log_hook_runs_after_confirmed_ticks_only() {
        use crate::replay_save_win::confirmed_hook_calls;
        let mut g = FakeGame::new();
        let mut st = play_state();
        let calls = || confirmed_hook_calls();
        let c0 = calls();
        resim_once(
            &mut g,
            &mut st,
            &[
                (TickKind::Confirmed, 20, 0),
                (TickKind::Predicted, 21, 0),
                (TickKind::Predicted, 22, 0),
                (TickKind::Confirmed, 23, 0),
            ],
        );
        assert_eq!(calls() - c0, 2, "確定の tick の数だけ呼ぶ");
        // 生きた tick: 相手の入力ワードが本物で、確定した状態の上（frame == front）のときだけ確定。
        st.hint.remote_next = 100;
        st.front = 30;
        let live = |g: &mut FakeGame, st: &mut PlayState, frame| {
            GameWindow { mem: g, st }.tick(Tick {
                frame,
                p1: 0,
                p2: 0,
                kind: TickKind::Live,
            });
        };
        let c1 = calls();
        live(&mut g, &mut st, 30);
        assert_eq!(calls() - c1, 1, "確定した状態の上の本物の語");
        live(&mut g, &mut st, 45);
        assert_eq!(calls() - c1, 1, "先の予測の状態の上の tick には呼ばない");
        st.hint.remote_next = 0;
        st.front = 50;
        live(&mut g, &mut st, 50);
        assert_eq!(
            calls() - c1,
            1,
            "相手の語が本物でない（予測の語）tick には呼ばない"
        );
    }

    #[test]
    fn a_replayed_se_is_remembered_and_the_window_is_two_frames() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, 0x35));
        let mut st = play_state();
        g.se_queue_push(7, 0);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 20, 0x44)]);
        assert_eq!(g.se_pushed, vec![(0x35, 0)], "初めての出来事を積み直さない");
        assert_eq!(g.se_queue(), vec![(7, 1), (0x35, 1)]);
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 20, 0x44)]);
        assert_eq!(
            g.se_pushed.len(),
            1,
            "バックアップに足していない（2 回目が積まれた）"
        );
        for (frame, pushed) in [
            (18, false),
            (19, false),
            (21, false),
            (22, false),
            (17, true),
            (23, true),
        ] {
            let mut g = FakeGame::new();
            g.se_noise = false;
            g.se_p2_eq = Some((0x44, 0x35));
            let mut st = play_state();
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 20, 0x44)]);
            assert_eq!(g.se_pushed.len(), 1);
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, frame, 0x44)]);
            assert_eq!(
                g.se_pushed.len(),
                1 + usize::from(pushed),
                "フレーム {frame}（20 の前後 2 フレームは同じ出来事）"
            );
        }
    }

    #[test]
    fn a_late_pushed_se_is_not_counted_as_the_next_live_ticks() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, 0x0E));
        let mut st = play_state();
        g.se_queue_push(0x0E, 0);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .tick(Tick {
            frame: 30,
            p1: 0,
            p2: 0,
            kind: TickKind::Live,
        });
        assert_eq!(
            st.stats().se.live_ids,
            0,
            "積み直した分を生きた tick の分に数えた"
        );
        g.se_queue_push(0x01, 0); // 後ろの枠に別の SE を足しても、読み方が詰まった枠の順に読むこと
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 31, 0x44)]);
        assert_eq!(st.stats().se.new_cutin, 1);
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, 0x0E));
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .tick(Tick {
            frame: 30,
            p1: 0,
            p2: 0x44,
            kind: TickKind::Live,
        });
        assert_eq!(st.stats().se.live_ids, 1);
        assert_eq!(
            g.se_queue(),
            vec![(0x0E, 1)],
            "生きた tick は待ち行列に触らない"
        );
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 31, 0x44)]);
        assert_eq!(g.se_pushed.len(), 0, "予測の SE と確定の SE が二重に鳴る");
        assert_eq!(st.stats().se.suppressed, 1);
    }

    #[test]
    fn replays_are_dropped_when_the_outputs_were_not_restored_and_a_new_seg_forgets() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, 0x35));
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .resim_begin();
        st.outputs.valid = false;
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .tick(Tick {
            frame: 5,
            p1: 0,
            p2: 0x44,
            kind: TickKind::Confirmed,
        });
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .resim_end();
        assert_eq!(g.se_pushed.len(), 0, "戻せていないのに積んだ");
        assert_eq!((st.stats().se.new_ids, st.stats().se.dropped), (1, 1));
        st.begin_seg();
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 5, 0x44)]);
        assert_eq!(g.se_pushed.len(), 1, "前の seg のバックアップが残っている");
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, 0x35));
        let mut st = play_state();
        let ticks: Vec<(TickKind, u32, u16)> = (0..40u32)
            .map(|i| (TickKind::Confirmed, 100 + i * 10, 0x44))
            .collect();
        resim_once(&mut g, &mut st, &ticks);
        assert_eq!(g.se_pushed.len(), 32);
        assert_eq!(st.stats().se.dropped, 8);
    }

    #[test]
    fn without_a_capture_the_se_path_leaves_the_queue_alone() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, 0x35));
        g.deny_read = Some(ASCII_COUNT);
        let mut st = play_state();
        g.se_queue_push(7, 0);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 20, 0x44)]);
        assert_eq!(st.stats().outputs_uncaptured, 1);
        let se = st.stats().se;
        assert_eq!(
            (se.resim_ids, se.new_ids, se.dropped, se.replayed),
            (0, 0, 0, 0)
        );
        assert_eq!(g.se_pushed.len(), 0);
        assert_eq!(g.se_queue(), vec![(7, 1), (0x35, 1)]);
    }

    fn hit_game(hit: HitSe, id: u32) -> (FakeGame, PlayState) {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, id));
        let mut st = play_state();
        st.set_hit_se(hit);
        (g, st)
    }

    #[test]
    fn an_unconfirmed_live_hit_waits_for_the_confirmed_tick_and_sounds_once() {
        let (mut g, mut st) = (FakeGame::new(), play_state());
        g.se_noise = false;
        g.se_p2_eq = Some((0x44, SE_ID_HIT));
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        assert_eq!(g.se_queue(), Vec::<(u32, u32)>::new(), "既定が即時");
        assert_eq!(st.stats().se.hit_mode, HitSe::Confirmed);
        for id in [SE_ID_HIT, SE_ID_LAST_LIFE] {
            let (mut g, mut st) = hit_game(HitSe::Confirmed, id);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            assert_eq!(
                g.se_queue(),
                Vec::<(u32, u32)>::new(),
                "id {id:#X}: 未確定の被弾音を待ち行列に残した"
            );
            g.play_and_draw();
            assert_eq!(
                g.played_ids(),
                Vec::<u32>::new(),
                "id {id:#X}: 確定の前に鳴った"
            );
            let se = st.stats().se;
            assert_eq!((se.hit_waited, se.live_ids), (1, 0), "{se:?}");
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0x44)]);
            assert_eq!(g.se_pushed, vec![(id, 0)], "id {id:#X}");
            g.play_and_draw();
            assert_eq!(g.played_ids(), vec![id], "id {id:#X}");
            let se = st.stats().se;
            assert_eq!(
                (se.hit_matched, se.replayed_hit, se.replayed),
                (1, 1, 1),
                "{se:?}"
            );
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0x44)]);
            assert_eq!(g.se_pushed.len(), 1, "id {id:#X}: 2 回目が積まれた");
            let (mut g, mut st) = hit_game(HitSe::Live, id);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            assert_eq!(g.se_queue(), vec![(id, 1)], "id {id:#X}: live で待たせた");
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0x44)]);
            assert_eq!(g.se_pushed.len(), 0, "id {id:#X}");
            let se = st.stats().se;
            assert_eq!(
                (se.hit_waited, se.live_ids, se.suppressed),
                (0, 1, 1),
                "{se:?}"
            );
        }
    }

    #[test]
    fn a_predicted_hit_that_the_confirmation_does_not_repeat_is_never_played() {
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        g.play_and_draw();
        let words = [(TickKind::Confirmed, 30, 0), (TickKind::Confirmed, 31, 0)];
        resim_once(&mut g, &mut st, &words);
        g.play_and_draw();
        assert_eq!(g.played_ids(), Vec::<u32>::new(), "幻の被弾音が鳴った");
        assert_eq!(g.se_pushed.len(), 0);
        let se = st.stats().se;
        assert_eq!(
            (se.hit_waited, se.hit_matched, se.replayed_hit),
            (1, 0, 0),
            "{se:?}"
        );
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        g.play_and_draw();
        resim_once(&mut g, &mut st, &words);
        assert_eq!(g.played_ids(), vec![SE_ID_HIT], "live は幻でも鳴る");
    }

    #[test]
    fn a_hit_confirmed_three_or_more_frames_away_still_sounds_once() {
        for d in -6i32..=6 {
            let frame = (30 + d) as u32;
            let near = d.unsigned_abs() <= SE_NEAR;
            let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            g.play_and_draw();
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, frame, 0x44)]);
            g.play_and_draw();
            assert_eq!(g.played_ids(), vec![SE_ID_HIT], "d = {d}");
            assert_eq!(st.stats().se.hit_matched, u32::from(near), "d = {d}");

            let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            g.play_and_draw();
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, frame, 0x44)]);
            g.play_and_draw();
            assert_eq!(
                g.played_ids().len(),
                1 + usize::from(!near),
                "d = {d}（live の対照）"
            );
        }
    }

    #[test]
    fn a_confirmed_live_hit_sounds_at_once() {
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
        st.hint.remote_next = 31;
        st.front = 30;
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        assert_eq!(g.se_queue(), vec![(SE_ID_HIT, 1)], "確定した被弾を待たせた");
        g.play_and_draw();
        assert_eq!(g.played_ids(), vec![SE_ID_HIT]);
        let se = st.stats().se;
        assert_eq!((se.hit_waited, se.live_ids), (0, 1), "{se:?}");
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0x44)]);
        assert_eq!(g.se_pushed.len(), 0, "即座に鳴った被弾がもう 1 回鳴る");
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
        st.hint.remote_next = 31;
        st.front = 28;
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        assert_eq!(
            g.se_queue(),
            Vec::<(u32, u32)>::new(),
            "前に未確定が残っている"
        );
    }

    #[test]
    fn a_predicted_resim_tick_does_not_play_or_remember_a_hit() {
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 40, 0x44)]);
        assert_eq!(g.se_pushed.len(), 0, "予測の回の被弾音を積んだ");
        let se = st.stats().se;
        assert_eq!(
            (se.resim_ids, se.new_ids, se.suppressed, se.hit_waited),
            (0, 0, 0, 0),
            "{se:?}"
        );
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 40, 0x44)]);
        assert_eq!(g.se_pushed, vec![(SE_ID_HIT, 0)]);
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 40, 0x44)]);
        assert_eq!(g.se_pushed, vec![(SE_ID_HIT, 0)]);
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_CUTIN);
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 40, 0x44)]);
        assert_eq!(g.se_pushed, vec![(SE_ID_CUTIN, 0)], "カットインを待たせた");
    }

    #[test]
    fn a_whole_late_run_sounds_each_hit_once_and_only_after_it_is_confirmed() {
        let run = |hit: HitSe| {
            let mut g = FakeGame::new();
            g.se_noise = false;
            g.se_p2_change = Some(1);
            let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_time_sync(false));
            let mut st = play_state();
            st.set_hit_se(hit);
            rb.begin_seg(1);
            let feed = feed_one_by_one(remote_steps);
            for k in 0..60 {
                feed(&mut rb, k);
                let (f, p) = frame(&mut rb, &mut st, &mut g, 0);
                assert!(matches!(f, ScanFrame::Ran { .. }), "{k}: {f:?}");
                assert!(matches!(p, PostFrame::Done(_)), "{k}: {p:?}");
            }
            assert_eq!(st.fault(), None);
            assert!(
                rb.stats().resims > 10,
                "進め直しが起きていない（検査の前提）"
            );
            (g, st)
        };
        let (g, st) = run(HitSe::Confirmed);
        assert_eq!(
            g.played_ids(),
            vec![4, 6, 1],
            "被弾音が 1 回ずつ・順に鳴らない"
        );
        let se = st.stats().se;
        assert_eq!(
            (se.hit_waited, se.hit_matched, se.replayed_hit),
            (1, 1, 1),
            "{se:?}"
        );
        assert_eq!(se.live_ids, 2, "{se:?}");
        let (draw, _, _) = g.se_log[0];
        assert!(
            (12..=12 + 8 + 2).contains(&draw),
            "被弾音が鳴った do_frame: {draw}"
        );
        let (g, st) = run(HitSe::Live);
        assert_eq!(g.played_ids(), vec![4, 6, 1]);
        assert_eq!(st.stats().se.hit_waited, 0);
        assert_eq!(st.stats().se.live_ids, 3);
        assert!(
            g.se_log[0].0 < draw,
            "live が確定の回より先に鳴らない（待たされたことが検査に見えていない）: live {} / confirmed {draw}",
            g.se_log[0].0
        );

        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p2_change = Some(1);
        let (_, st) = run_late(&mut g, 50, &feed_in_bursts(remote_pulses), &|_| 0);
        assert_eq!(g.played_ids(), vec![4, 1, 4, 1, 4, 1]);
        let se = st.stats().se;
        assert_eq!(
            (se.hit_waited, se.hit_matched, se.replayed_hit),
            (0, 0, 3),
            "{se:?}"
        );
    }

    #[test]
    fn live_counts_a_phantom_hit_that_the_confirmation_does_not_repeat() {
        let phantom =
            |se: &crate::rollback_resim::SeStats| se.hit_live_sounded - se.hit_live_matched;
        for id in [SE_ID_HIT, SE_ID_LAST_LIFE] {
            let (mut g, mut st) = hit_game(HitSe::Live, id);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            g.play_and_draw();
            assert_eq!(g.played_ids(), vec![id], "id {id:#X}: live で鳴らない");
            let words = [(TickKind::Confirmed, 30, 0), (TickKind::Confirmed, 31, 0)];
            resim_once(&mut g, &mut st, &words);
            let se = st.stats().se;
            assert_eq!(
                (se.hit_live_sounded, se.hit_live_matched, phantom(&se)),
                (1, 0, 1),
                "id {id:#X}: {se:?}"
            );
            assert_eq!((se.hit_waited, se.hit_matched), (0, 0), "{se:?}");
            let (mut g, mut st) = hit_game(HitSe::Live, id);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0x44)]);
            let se = st.stats().se;
            assert_eq!(
                (se.hit_live_sounded, se.hit_live_matched, phantom(&se)),
                (1, 1, 0),
                "id {id:#X}: {se:?}"
            );
            assert_eq!(se.hit_matched, 0, "confirmed 側の数に入った: {se:?}");
        }
        for d in -6i32..=6 {
            let frame = (30 + d) as u32;
            let near = d.unsigned_abs() <= SE_NEAR;
            let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, frame, 0x44)]);
            let se = st.stats().se;
            assert_eq!(
                (se.hit_live_sounded, se.hit_live_matched, phantom(&se)),
                (1, u32::from(near), u32::from(!near)),
                "d = {d}: {se:?}"
            );
            let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
            tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
            resim_once(&mut g, &mut st, &[(TickKind::Confirmed, frame, 0x44)]);
            assert_eq!(
                st.stats().se.hit_matched,
                se.hit_live_matched,
                "d = {d}: 判定が confirmed とずれた"
            );
        }
    }

    #[test]
    fn live_phantom_counts_stay_zero_for_confirmed_runs_and_other_cases() {
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0)]);
        let se = st.stats().se;
        assert_eq!(
            (
                se.hit_waited,
                se.hit_matched,
                se.hit_live_sounded,
                se.hit_live_matched
            ),
            (1, 0, 0, 0),
            "{se:?}"
        );
        let l = crate::rollback_resim::se_summary_line(&se);
        assert!(
            !l.contains("live の被弾音"),
            "confirmed の行が変わった: {l}"
        );
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
        st.hint.remote_next = 31;
        st.front = 30;
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0x44)]);
        let se = st.stats().se;
        assert_eq!((se.hit_live_sounded, se.hit_live_matched), (0, 0), "{se:?}");
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_CUTIN);
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0)]);
        let se = st.stats().se;
        assert_eq!((se.hit_live_sounded, se.hit_live_matched), (0, 0), "{se:?}");
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
        tick_with(&mut g, &mut st, TickKind::Live, 30, 0x44);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 30, 0)]);
        let l = crate::rollback_resim::se_summary_line(&st.stats().se);
        assert!(
            l.contains("鳴らした 1・うち確定の回で見つかった 0・見つからなかった幻の被弾音")
                && l.contains("走り終わりの未確定の分を含む） 1"),
            "{l}"
        );
    }

    #[test]
    fn live_counts_a_hit_first_played_in_a_predicted_resim_tick() {
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 40, 0x44)]);
        assert_eq!(g.se_pushed, vec![(SE_ID_HIT, 0)], "live は予測の回でも積む");
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 40, 0x44)]);
        let se = st.stats().se;
        assert_eq!((se.hit_live_sounded, se.hit_live_matched), (1, 0), "{se:?}");
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 40, 0x44)]);
        let se = st.stats().se;
        assert_eq!((se.hit_live_sounded, se.hit_live_matched), (1, 1), "{se:?}");
        let (mut g, mut st) = hit_game(HitSe::Live, SE_ID_HIT);
        resim_once(&mut g, &mut st, &[(TickKind::Predicted, 40, 0x44)]);
        resim_once(&mut g, &mut st, &[(TickKind::Confirmed, 40, 0)]);
        let se = st.stats().se;
        assert_eq!((se.hit_live_sounded, se.hit_live_matched), (1, 0), "{se:?}");
    }

    #[test]
    fn a_whole_late_run_counts_the_live_hit_as_found_in_the_confirmed_tick() {
        let run = |hit: HitSe| {
            let mut g = FakeGame::new();
            g.se_noise = false;
            g.se_p2_change = Some(1);
            let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_time_sync(false));
            let mut st = play_state();
            st.set_hit_se(hit);
            rb.begin_seg(1);
            let feed = feed_one_by_one(remote_steps);
            for k in 0..60 {
                feed(&mut rb, k);
                let (f, p) = frame(&mut rb, &mut st, &mut g, 0);
                assert!(matches!(f, ScanFrame::Ran { .. }), "{k}: {f:?}");
                assert!(matches!(p, PostFrame::Done(_)), "{k}: {p:?}");
            }
            st.stats().se
        };
        let se = run(HitSe::Live);
        assert_eq!(
            (se.hit_live_sounded, se.hit_live_matched),
            (1, 1),
            "live の被弾 1 回: {se:?}"
        );
        let se = run(HitSe::Confirmed);
        assert_eq!(
            (se.hit_live_sounded, se.hit_live_matched),
            (0, 0),
            "confirmed で数えた: {se:?}"
        );
    }

    #[test]
    fn holding_a_hit_leaves_the_other_se_in_the_queue_as_they_were() {
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p1_change = Some(0); // p1 = 4 で id 4 を積む（被弾音）
        g.se_p2_eq = Some((0x44, SE_ID_CUTIN)); // 被弾音の後ろに 0xE
        let mut st = play_state();
        g.se_queue_push(7, 10); // 走査の前からある SE（7 が 2 回。パンの平均 15）
        g.se_queue_push(7, 20);
        g.se_queue_push(SE_ID_HIT, 33); // 走査の前からある被弾音（遅れて積み直した確定の分。抜かない）
        let live = |g: &mut FakeGame, st: &mut PlayState| {
            GameWindow { mem: g, st }.tick(Tick {
                frame: 30,
                p1: 4,
                p2: 0x44,
                kind: TickKind::Live,
            });
        };
        live(&mut g, &mut st);
        assert_eq!(
            g.se_queue(),
            vec![(7, 2), (SE_ID_HIT, 1), (SE_ID_CUTIN, 1)],
            "順・回数"
        );
        let q = read_se_queue(&mut AsOutputs(&mut g));
        let got: Vec<(u32, i32, u32)> = q.events().map(|(e, c)| (e.id, e.pan, c)).collect();
        assert_eq!(
            got,
            vec![(7, 15, 2), (SE_ID_HIT, 33, 1), (SE_ID_CUTIN, 0, 1)],
            "パンの平均"
        );
        let se = st.stats().se;
        assert_eq!(
            (se.hit_waited, se.live_ids, se.live_cutin),
            (1, 1, 1),
            "{se:?}"
        );
        let (mut g, mut st) = hit_game(HitSe::Confirmed, SE_ID_HIT);
        g.se_queue_push(7, 0);
        tick_with(&mut g, &mut st, TickKind::Live, 31, 0);
        assert_eq!(g.se_pushed.len(), 0);
        assert_eq!(g.se_queue(), vec![(7, 1)]);
        let mut g = FakeGame::new();
        g.se_noise = false;
        g.se_p1_change = Some(0);
        g.se_p2_eq = Some((0x44, SE_ID_CUTIN));
        g.se_refuse = true;
        let mut st = play_state();
        live(&mut g, &mut st);
        assert_eq!(g.se_queue(), Vec::<(u32, u32)>::new());
        assert_eq!(st.stats().se.refused, 1);
    }

    fn tick_with(
        g: &mut FakeGame,
        st: &mut PlayState,
        kind: TickKind,
        frame: u32,
        p2: u16,
    ) -> bool {
        GameWindow { mem: g, st }
            .tick(Tick {
                frame,
                p1: 0,
                p2,
                kind,
            })
            .boundary
    }

    #[test]
    fn boundaries_follow_the_scene_the_scan_and_unconfirmed_list_changes() {
        for ret in [2, 30, 1] {
            let mut g = FakeGame::new();
            let mut st = play_state();
            g.ret = ret;
            assert!(
                !tick_with(&mut g, &mut st, TickKind::Predicted, 1, 0),
                "ret {ret}"
            );
        }
        for ret in [0, -1] {
            let mut g = FakeGame::new();
            let mut st = play_state();
            g.ret = ret;
            assert!(
                tick_with(&mut g, &mut st, TickKind::Confirmed, 1, 0),
                "ret {ret}"
            );
            assert_eq!(st.stats().abnormal_ticks, 1);
        }
        let mut g = FakeGame::new();
        let mut st = play_state();
        g.consume = false;
        assert!(tick_with(&mut g, &mut st, TickKind::Confirmed, 1, 0));
        for kind in [TickKind::Confirmed, TickKind::Predicted] {
            let mut g = FakeGame::new();
            let mut st = play_state();
            g.scene_on_p2 = Some(9);
            assert!(tick_with(&mut g, &mut st, kind, 1, 9), "{kind:?}");
        }
        let mut g = FakeGame::new();
        let mut st = play_state();
        g.list_on_p2 = Some(4);
        assert!(tick_with(&mut g, &mut st, TickKind::Predicted, 1, 4));
        assert!(!tick_with(&mut g, &mut st, TickKind::Confirmed, 1, 4));
        assert_eq!(
            (st.stats().list_holds, st.stats().list_changes_confirmed),
            (1, 1)
        );
    }

    #[test]
    fn a_live_tick_is_confirmed_only_on_the_front_with_a_real_word() {
        let mut g = FakeGame::new();
        let mut st = play_state();
        g.list_on_p2 = Some(4);
        st.front = 5;
        st.hint = Hint {
            remote_next: 8,
            delay: 1,
        };
        assert!(
            !tick_with(&mut g, &mut st, TickKind::Live, 5, 4),
            "前線の本物の語"
        );
        assert!(
            tick_with(&mut g, &mut st, TickKind::Live, 6, 4),
            "前線より先"
        );
        st.hint.remote_next = 5;
        assert!(
            tick_with(&mut g, &mut st, TickKind::Live, 5, 4),
            "相手の語がまだ"
        );
        st.front = 0;
        st.hint.remote_next = 0;
        assert!(
            !tick_with(&mut g, &mut st, TickKind::Live, 0, 4),
            "穴埋めのフレーム"
        );
    }

    #[test]
    fn the_scan_point_reads_the_local_word_once_and_orders_the_scan() {
        let mut rb = Rollback::new(Config::rollback(Seat::P2, 1, 2));
        let mut st = play_state();
        let mut g = FakeGame::new();
        let mut reads = 0u32;
        let mut sent = Vec::new();
        let f = scan_point(
            &mut rb,
            &mut st,
            &mut g,
            &mut || {
                reads += 1;
                0x41
            },
            &mut |i| sent.push(*i),
        );
        assert!(matches!(
            f,
            ScanFrame::Ran {
                step: Step::Ran { frame: 0, .. },
                ..
            }
        ));
        assert_eq!(g.scans.last(), Some(&(Order::Feed { p1: 0, p2: 0 }, false)));
        let _ = post_present(&mut rb, &mut st, &mut g);
        for k in 1..=2u16 {
            let _ = scan_point(
                &mut rb,
                &mut st,
                &mut g,
                &mut || {
                    reads += 1;
                    0x41 + k
                },
                &mut |i| sent.push(*i),
            );
            if k == 1 {
                assert_eq!(
                    g.scans.last(),
                    Some(&(Order::Feed { p1: 0, p2: 0x41 }, false))
                );
            }
            let _ = post_present(&mut rb, &mut st, &mut g);
        }
        let reads_before = reads;
        for _ in 0..3 {
            let f = scan_point(
                &mut rb,
                &mut st,
                &mut g,
                &mut || {
                    reads += 1;
                    0x43
                },
                &mut |i| sent.push(*i),
            );
            assert!(
                matches!(
                    f,
                    ScanFrame::Ran {
                        step: Step::Wait(Wait::Ahead),
                        ret: 1
                    }
                ),
                "{f:?}"
            );
            assert_eq!(g.scans.last(), Some(&(Order::RenderOnly, false)));
        }
        assert_eq!(
            reads - reads_before,
            1,
            "待ちのフレームで自分の語を取り直した"
        );
        assert_eq!(sent.len() as u32, reads, "取った語ごとに 1 回送る");
        assert!(g.redirect);
        g.scene_on_p2 = Some(0x41);
        let inp = remote(&rb, 1, &[0x77]);
        rb.on_input(&inp);
        let _ = post_present(&mut rb, &mut st, &mut g);
        let scans = g.scans.len();
        let f = scan_point(&mut rb, &mut st, &mut g, &mut || 0, &mut |_| {});
        assert_eq!(f, ScanFrame::SceneEnd);
        assert!(!g.redirect, "場面の境目の後も確保の向け先を開いたまま");
        assert_eq!(g.scans.len(), scans, "場面の境目の後に走査を呼んだ");
    }

    #[test]
    fn outside_allocations_are_counted_only_while_predicting() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 0, 8));
        let mut st = play_state();
        let mut g = FakeGame::new();
        g.alloc_on_p2 = Some(0);
        for _ in 0..3 {
            frame(&mut rb, &mut st, &mut g, 1);
        }
        assert_eq!(st.stats().pred_out_allocs, 3);
        let inp = remote(&rb, 0, &[5, 5, 5]);
        rb.on_input(&inp);
        let _ = post_present(&mut rb, &mut st, &mut g);
        let s = st.stats();
        assert_eq!((s.pred_out_allocs, s.pred_out_alive_at_restore), (3, 3));
        assert!(!g.watch, "見張りを立てたまま");
        assert_eq!(g.hold_in_scans.len(), 6, "予測 3 回 ＋ 確定の進め直し 3 回");
        assert_eq!(
            g.hold_in_scans,
            [true, true, true, false, false, false],
            "旗の立て方"
        );
        assert!(!g.hold_frees, "旗を立てたまま");
        assert_eq!(
            g.pending_sets, 3,
            "書き戻しが来る印を予測の tick だけで立てる"
        );
    }

    #[test]
    fn restore_puts_back_outside_nodes_unlinked_while_predicting() {
        let reg = UPDATE_REGISTRY;
        let draw = UPDATE_REGISTRY + UPDATE_DRAW_HEAD;
        let (x, y) = (0x0BAB_0FD8u32, 0x0BAB_1050u32);
        let setup = |g: &mut FakeGame| {
            g.real_lists = true;
            g.mem.insert(reg + UPDATE_NEXT, x);
            g.mem.insert(draw + UPDATE_NEXT, 0);
            for (node, prev, next, f) in [(x, reg, y, 0x0042_1111), (y, x, 0, 0x0042_2222)] {
                g.mem.insert(node + 0x02, 1); // ヒープの節
                g.mem.insert(node + UPDATE_FN, f);
                g.mem.insert(node + 0x0C, 0x0042_3333);
                g.mem.insert(node + 0x10, prev);
                g.mem.insert(node + UPDATE_NEXT, next);
                g.mem.insert(node + UPDATE_ARG, node + 0x100);
            }
        };
        let unlink_x = |g: &mut FakeGame| {
            g.mem.insert(reg + UPDATE_NEXT, y);
            g.mem.insert(y + 0x10, reg);
            for off in [0x04, 0x08, 0x0C, 0x10, 0x14] {
                g.mem.insert(x + off, 0);
            }
        };
        let mut g = FakeGame::new();
        setup(&mut g);
        let before: Vec<u32> = (0..16).map(|i| g.get(x + 4 * i)).collect();
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!(
            st.stats().outside_nodes_max,
            2,
            "範囲の外の節を数えていない"
        );
        unlink_x(&mut g);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), None, "{:?}", st.list_diag());
        assert_eq!(g.node_writes, 2);
        let after: Vec<u32> = (0..16).map(|i| g.get(x + 4 * i)).collect();
        assert_eq!(after, before, "外された節の欄が戻らない");
        assert_eq!(
            (g.get(reg + UPDATE_NEXT), g.get(y + 0x10)),
            (x, x),
            "つなぎが戻らない"
        );
        let (kinds, other) = st.changed_nodes();
        let seen: Vec<(u32, u32, u32, u32)> = kinds
            .iter()
            .filter(|c| c.n != 0)
            .map(|c| (c.on_tick, c.on_remove, c.this, c.n))
            .collect();
        assert_eq!(
            seen,
            [
                (0x0042_1111, 0x0042_3333, x + 0x100, 1),
                (0x0042_2222, 0x0042_3333, y + 0x100, 1)
            ]
        );
        assert_eq!(other, 0);

        let mut g = FakeGame::new();
        setup(&mut g);
        g.refuse_node_writes = true;
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        unlink_x(&mut g);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::NodeNotWritten { addr: x }));
        assert_eq!(st.stats().nodes_unwritten, 1);
        assert_eq!(g.get(x + UPDATE_NEXT), 0, "書けないのに書いた");
    }

    /// 更新の節 `x`（fn = 種類 5 の更新 `0x4224F0`）と描画の節 `d`（fn = `0x422C10`）が同じ本体 `body`（0x38 B。`+0x08` が `x`、
    /// `+0x0C` が `d`）を指す形を、偽のゲームへ置く。節も本体もコピーの範囲の外のヒープ。
    fn screen_effect_scene(g: &mut FakeGame, x: u32, d: u32, body: u32) {
        let reg = UPDATE_REGISTRY;
        let draw = UPDATE_REGISTRY + UPDATE_DRAW_HEAD;
        g.real_lists = true;
        g.mem.insert(draw + UPDATE_NEXT, d);
        g.mem.insert(reg + UPDATE_NEXT, x);
        for (node, prev, f) in [(x, reg, 0x0042_24F0), (d, draw, 0x0042_2C10)] {
            g.mem.insert(node + 0x02, 1);
            g.mem.insert(node + UPDATE_FN, f);
            g.mem.insert(node + 0x0C, 0x0042_2900);
            g.mem.insert(node + 0x10, prev);
            g.mem.insert(node + UPDATE_NEXT, 0);
            g.mem.insert(node + UPDATE_ARG, body);
        }
        for i in 0..BODY_WORDS as u32 {
            g.mem.insert(body + 4 * i, 0xB0D0_0000 + i);
        }
        g.mem.insert(body + SCREEN_EFFECT_UPDATE_NODE, x);
        g.mem.insert(body + SCREEN_EFFECT_DRAW_NODE, d);
    }

    fn words_at(g: &FakeGame, addr: u32, n: usize) -> Vec<u32> {
        (0..n as u32).map(|i| g.get(addr + 4 * i)).collect()
    }

    /// 予測の tick が後片付け `0x422900` を走らせた形（`0x42B350` が節を列から外して欄を 0 にし、`0x422900` が本体の `+0x0C` を 0
    /// にして、本体の解放は CRT へ渡さずバックアップする）。
    fn run_screen_effect_teardown(g: &mut FakeGame, x: u32, d: u32, body: u32) {
        let draw = UPDATE_REGISTRY + UPDATE_DRAW_HEAD;
        g.mem.insert(UPDATE_REGISTRY + UPDATE_NEXT, 0);
        g.mem.insert(draw + UPDATE_NEXT, 0);
        for node in [x, d] {
            for off in [0x04, 0x08, 0x0C, 0x10, 0x14] {
                g.mem.insert(node + off, 0);
            }
        }
        g.mem.insert(body + SCREEN_EFFECT_DRAW_NODE, 0);
        g.held.push(body);
    }

    #[test]
    fn restore_puts_back_a_screen_effect_body_whose_back_pointer_was_zeroed() {
        let (x, d, body) = (0x0BAB_0FD8u32, 0x0BAB_1050u32, 0x0BAC_0000u32);
        let mut g = FakeGame::new();
        screen_effect_scene(&mut g, x, d, body);
        let before = words_at(&g, body, BODY_WORDS);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!(
            st.stats().bodies_max,
            1,
            "同じ本体を 2 回数えた / バックアップしない"
        );
        assert_eq!(g.stray_bodies, [body], "解放の見分けの表へ本体を渡さない");
        run_screen_effect_teardown(&mut g, x, d, body);
        assert_eq!(g.get(body + SCREEN_EFFECT_DRAW_NODE), 0, "検査の前提");
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), None, "{:?}", st.list_diag());
        assert_eq!(
            words_at(&g, body, BODY_WORDS),
            before,
            "本体の 0x38 B が戻らない"
        );
        assert_eq!(g.get(d + UPDATE_ARG), body, "描画の節が戻らない");
        let s = st.stats();
        assert_eq!(
            (s.bodies_restored, s.bodies_changed, g.body_writes),
            (1, 1, 1)
        );
        assert_eq!(
            (s.bodies_over, s.bodies_unreadable, s.bodies_unwritten),
            (0, 0, 0)
        );
        let t = summary_lines(&s, None).join("\n");
        assert!(
            t.contains("控えた ScreenEffect の本体（コピーの範囲の外。0x38 B）1 回のコピーで最大 1・書き戻し 1（うち中身が違っていた 1）"),
            "{t}"
        );
    }

    #[test]
    fn a_body_inside_the_snapshot_or_behind_another_function_is_not_captured() {
        let (x, d) = (0x0BAB_0FD8u32, 0x0BAB_1050u32);
        let inside = ARENA_BASE + 0x100;
        let mut g = FakeGame::new();
        screen_effect_scene(&mut g, x, d, inside);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!(st.stats().bodies_max, 0, "領域の中の本体をバックアップした");
        assert_eq!(g.body_reads, 0, "領域の中の本体を読んだ");
        assert!(g.stray_bodies.is_empty());
        let heap = 0x0BAC_0000u32;
        let mut g = FakeGame::new();
        screen_effect_scene(&mut g, x, d, heap);
        g.mem.insert(x + UPDATE_FN, 0x0042_0190);
        g.mem.insert(d + UPDATE_FN, 0x0043_1540);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!(
            st.stats().bodies_max,
            0,
            "別の関数の this をバックアップした"
        );
        assert_eq!(g.body_reads, 0, "別の関数の this を読んだ");
        let mut g = FakeGame::new();
        screen_effect_scene(&mut g, x, d, heap);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!((st.stats().bodies_max, g.body_reads), (1, 1));
    }

    #[test]
    fn a_body_freed_outside_the_watch_is_not_written_back() {
        let (x, d, body) = (0x0BAB_0FD8u32, 0x0BAB_1050u32, 0x0BAC_0000u32);
        let mut g = FakeGame::new();
        screen_effect_scene(&mut g, x, d, body);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        run_screen_effect_teardown(&mut g, x, d, body);
        g.freed_bodies.push(body);
        let (ins, nodes) = (g.copy_ins, g.node_writes);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::BodyFreed { addr: body }));
        assert_eq!(st.stats().bodies_freed, 1);
        assert_eq!(
            (g.body_writes, g.node_writes, g.copy_ins),
            (0, nodes, ins),
            "止める前に書いた"
        );
        assert_eq!(g.get(body + SCREEN_EFFECT_DRAW_NODE), 0, "書き戻した");
        assert!(
            Fault::BodyFreed { addr: body }
                .text()
                .contains("0x0BAC0000")
        );
        let mut g = FakeGame::new();
        assert!(g.body_freed(body));
    }

    #[test]
    fn bodies_that_cannot_be_captured_or_written_stop_the_match() {
        let chain = |g: &mut FakeGame, n: u32| -> Vec<(u32, u32)> {
            g.real_lists = true;
            g.mem
                .insert(UPDATE_REGISTRY + UPDATE_DRAW_HEAD + UPDATE_NEXT, 0);
            let mut prev = UPDATE_REGISTRY;
            let mut v = Vec::new();
            for i in 0..n {
                let (node, body) = (0x0BAB_0000 + i * 0x100, 0x0BAC_0000 + i * 0x100);
                g.mem.insert(prev + UPDATE_NEXT, node);
                g.mem.insert(node + UPDATE_FN, 0x0042_24F0);
                g.mem.insert(node + UPDATE_ARG, body);
                g.mem.insert(node + UPDATE_NEXT, 0);
                g.mem.insert(body + SCREEN_EFFECT_UPDATE_NODE, node);
                prev = node;
                v.push((node, body));
            }
            v
        };
        let cap = OUTSIDE_BODIES_CAP as u32;
        let mut g = FakeGame::new();
        let v = chain(&mut g, cap);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!((st.stats().bodies_max, st.stats().bodies_over), (cap, 0));
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), None);
        assert_eq!(g.body_writes, cap);
        let mut g = FakeGame::new();
        let v2 = chain(&mut g, cap + 1);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!(
            (st.stats().bodies_max, st.stats().bodies_over),
            (cap, 1),
            "{v:?}"
        );
        assert_eq!(st.fault(), None, "バックアップできなかった時点では止めない");
        let last = v2.last().map(|&(_, b)| b).unwrap_or(0);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::BodyNotCaptured { addr: last }));
        assert_eq!(g.body_writes, 0, "止める前に書いた");

        let mut g = FakeGame::new();
        let v = chain(&mut g, 1);
        g.mem.insert(v[0].1 + SCREEN_EFFECT_UPDATE_NODE, 0x1234);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        assert_eq!(
            (st.stats().bodies_max, st.stats().bodies_unreadable),
            (0, 1)
        );
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(
            st.fault(),
            Some(Fault::BodyNotCaptured { addr: v[0].1 }),
            "本体と思えない物を静かに通した"
        );

        let mut g = FakeGame::new();
        let v = chain(&mut g, 1);
        g.refuse_body_writes = true;
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::BodyNotWritten { addr: v[0].1 }));
        assert_eq!(st.stats().bodies_unwritten, 1);
    }

    #[test]
    fn hold_lines_name_the_changed_nodes_and_stray_frees() {
        let quiet = hold_lines(&[ChangedNode::default(); CHANGED_KINDS], 0, 0, 0, 0, &[]);
        assert_eq!(quiet.len(), 1, "{quiet:?}");
        assert!(quiet[0].contains("見張りの外で起きた領域の外の解放 0"));
        let mut k = [ChangedNode::default(); CHANGED_KINDS];
        k[0] = ChangedNode {
            head: 0x0001_0005,
            on_tick: 0x0041_B9D0,
            on_remove: 0x0041_BA00,
            this: 0x0BAB_1000,
            n: 7,
        };
        let rec = StrayRec {
            ret: 0x0041_2345,
            ptr: 0x0BAB_4000,
            flags: StrayRec::NODE | StrayRec::BLOCK,
            frame: 77,
        };
        let body_rec = StrayRec {
            ret: 0x0042_2923,
            ptr: 0x0BAC_0000,
            flags: StrayRec::BODY,
            frame: 78,
        };
        let t = hold_lines(&k, 2, 3, 4, 5, &[rec, body_rec]).join("\n");
        assert!(
            t.contains("1件目: 呼び手 0x00412345・ポインタ 0x0BAB4000・範囲の外の節 はい・解放を控えたアドレス いいえ・コピーの表の塊 はい・ScreenEffect の本体 いいえ・コピーのフレーム 77"),
            "控えの行が無い: {t}"
        );
        assert!(
            t.contains("2件目: 呼び手 0x00422923・ポインタ 0x0BAC0000・範囲の外の節 いいえ・解放を控えたアドレス いいえ・コピーの表の塊 いいえ・ScreenEffect の本体 はい・コピーのフレーム 78"),
            "本体の印が無い: {t}"
        );
        for want in [
            "解放 3・アドレスを控えられなかった解放 4 / 確保 5",
            "毎フレーム 0x0041B9D0・外すとき 0x0041BA00・頭 0x00010005・this 0x0BAB1000 が 7 回",
            "ほか 2 回",
        ] {
            assert!(t.contains(want), "{want} が無い: {t}");
        }
    }

    #[test]
    fn restore_moves_a_block_back_when_its_free_was_held() {
        let mut g = FakeGame::new();
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        let b = &ROLLBACK_BLOCKS[0];
        let old = g.get(b.ptr_at);
        g.mem.insert(b.ptr_at, 0x0F00_0000);
        g.held.push(old);
        let ins = g.copy_ins;
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), None, "{:?}", st.place_diag());
        assert_eq!(g.copy_ins, ins + 1, "書き戻していない");
        assert_eq!(g.get(b.ptr_at), old, "ポインタが元のアドレスへ戻らない");
        assert_eq!(st.stats().places_moved_back, 1);
        assert!(g.held.is_empty(), "書き戻しの後もバックアップが残る");
    }

    #[test]
    fn play_data_is_the_e1_data_plus_the_registry() {
        for (p, e) in ROLLBACK_PLAY_DATA.iter().zip(ROLLBACK_DATA) {
            assert_eq!((p.name, p.end), (e.name, e.end));
            if p.name != "globals_c" {
                assert_eq!(p, e);
            }
        }
        let reg = ROLLBACK_PLAY_DATA[ROLLBACK_DATA.len()];
        assert_eq!((reg.start, reg.end), (0x004A_CFC8, 0x004A_D008));
        assert_eq!(ROLLBACK_PLAY_DATA.len(), ROLLBACK_DATA.len() + 1);
        assert!(!ROLLBACK_DATA.iter().any(|d| d.start == 0x004A_CFC8));
        let st = play_state();
        let n_data = st
            .plan()
            .regions()
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::Data(_)))
            .count();
        assert_eq!(n_data, ROLLBACK_DATA.len() + 1);
        assert!(
            st.plan()
                .regions()
                .iter()
                .any(|r| r.addr == 0x004A_CFC8 && r.len == 0x40)
        );
    }

    #[test]
    fn calloc_site_passes_the_table_checks_and_overlaps_nothing() {
        assert_eq!(check_table(CALLOC_SITES, FORBIDDEN), Ok(()));
        let mut all: Vec<SiteDef> = SITES.to_vec();
        all.extend_from_slice(PACING_SITES);
        all.extend_from_slice(ALLOC_SITES);
        all.extend_from_slice(RESIM_SITES);
        all.extend_from_slice(CALLOC_SITES);
        assert_eq!(check_table(&all, FORBIDDEN), Ok(()));
    }

    #[test]
    fn play_values_appear_in_the_note() {
        let start = NOTE.find("## 30. ").expect("ノートに §30 が無い");
        let sec = &NOTE[start..];
        let c = &CALLOC_SITES[0];
        assert!(
            sec.contains(&format!("`0x{:08X}`", c.addr)),
            "calloc のアドレス"
        );
        assert!(
            sec.contains(&hex_bytes(c.expected)),
            "calloc の期待バイト列 {}",
            hex_bytes(c.expected)
        );
        assert!(sec.contains("`0x004ACFC8`〜`0x004AD008`"), "つなぎ");
        assert!(
            sec.contains(&format!("`0x{REGISTRY_TAIL_OBJ:08X}`")),
            "レジストリの後ろの 16 B"
        );
        let shot = ROLLBACK_EXCLUDED_CALLERS
            .iter()
            .find(|r| r.start == 0x0042_F540)
            .expect("スクリーンショットの行");
        assert!(sec.contains(&format!("`0x{:08X}`〜`0x{:08X}`", shot.start, shot.end)));
    }

    /// 偽の命令の表（`HookEnv` と `DetourEnv`）。スレッドは 0 本。
    pub(crate) struct FakeCode {
        pub(crate) mem: BTreeMap<u32, u8>,
        pub(crate) writes: Vec<(u32, Vec<u8>)>,
        next_tramp: u32,
    }

    impl FakeCode {
        pub(crate) fn new() -> Self {
            let mut mem = BTreeMap::new();
            for (_, d) in play_sites() {
                for (i, &b) in d.expected.iter().enumerate() {
                    mem.insert(d.addr + i as u32, b);
                }
            }
            FakeCode {
                mem,
                writes: Vec::new(),
                next_tramp: 0x0090_0000,
            }
        }
    }

    impl HookEnv for FakeCode {
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
            for (i, b) in buf.iter_mut().enumerate() {
                match self.mem.get(&(addr + i as u32)) {
                    Some(&v) => *b = v,
                    None => return false,
                }
            }
            true
        }
        fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
            for (i, &b) in bytes.iter().enumerate() {
                self.mem.insert(addr + i as u32, b);
            }
            self.writes.push((addr, bytes.to_vec()));
            Ok(WriteDone {
                protect_restored: true,
                flushed: true,
            })
        }
        fn set_original_target(&mut self, _t: u32) {
            panic!("入力のフックの元の飛び先を触った");
        }
        fn set_window(&mut self, _a: u32) {
            panic!("ウィンドウを触った");
        }
        fn set_copy_table(&mut self, _r: &[CopyRow]) {
            panic!("コピーの表を触った");
        }
        fn set_render_only_exit(&mut self, _a: u32) {
            panic!("出口を触った");
        }
        fn entry_address(&self) -> u32 {
            0x1111_0000
        }
        fn match_entry_address(&self) -> u32 {
            0x1111_1000
        }
        fn set_match_original(&mut self, _t: u32) {
            panic!("開始のフックを触った");
        }
        fn sleep_ms(&mut self, _ms: u32) {}
        fn now_ms(&self) -> u64 {
            0
        }
        fn counter(&self) -> u32 {
            0
        }
    }

    impl DetourEnv for FakeCode {
        fn alloc_trampoline(&mut self, _len: usize) -> Result<u32, u32> {
            self.next_tramp += 0x1000;
            Ok(self.next_tramp)
        }
        fn write_and_seal_trampoline(&mut self, _addr: u32, _bytes: &[u8]) -> Result<(), u32> {
            Ok(())
        }
        fn free_trampoline(&mut self, _addr: u32) {}
        fn set_trampoline(&mut self, _addr: u32) {
            panic!("描画の入口のトランポリンを触った");
        }
        fn detour_entry_address(&self) -> u32 {
            0x2222_0000
        }
    }

    const FAKE_ENTRY: u32 = 0x3000_0000;
    pub(crate) const FAKE_SCAN_ENTRY: u32 = 0x3100_0000;

    #[derive(Default)]
    pub(crate) struct FakeHooks {
        tramps: BTreeMap<usize, u32>,
        pub(crate) scan_original: Option<u32>,
        zero_entry: Option<usize>,
    }

    impl PlayHooks for FakeHooks {
        fn entry(&self, site: usize) -> u32 {
            if self.zero_entry == Some(site) {
                0
            } else {
                FAKE_ENTRY + site as u32 * 0x100
            }
        }
        fn set_trampoline(&mut self, site: usize, addr: u32) {
            self.tramps.insert(site, addr);
        }
        fn scan_entry(&self) -> u32 {
            FAKE_SCAN_ENTRY
        }
        fn set_scan_original(&mut self, addr: u32) {
            self.scan_original = Some(addr);
        }
    }

    fn install(
        f: &mut FakeCode,
        h: &mut FakeHooks,
        wish: Mode,
        forbidden: &[AddrRange],
    ) -> (PlayInstall, String) {
        let mut out = Vec::new();
        let r = install_play(wish, f, h, forbidden, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn install_writes_five_sites_through_their_own_entries() {
        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        let (r, log) = install(&mut f, &mut h, Mode::Rollback, FORBIDDEN);
        assert!(r.all_written(), "{log}");
        assert_eq!(f.writes.len(), 5, "{log}");
        for (i, (site, def)) in play_sites().into_iter().enumerate() {
            let addr = f.writes[i].0;
            let addr = &addr;
            let mut head = [0u8; 5];
            assert!(f.read_code(def.addr, &mut head));
            if site == usize::MAX {
                assert_eq!(*addr, def.addr + 1, "{}", def.name);
                assert_eq!(head[0], 0xE8);
                let rel = i32::from_le_bytes(head[1..5].try_into().unwrap());
                assert_eq!((def.addr as i32 + 5 + rel) as u32, FAKE_SCAN_ENTRY);
            } else {
                assert_eq!(*addr, def.addr, "{}", def.name);
                assert_eq!(
                    higanr_core::patch::jmp_target(def.addr, &head),
                    Some(FAKE_ENTRY + site as u32 * 0x100),
                    "{}",
                    def.name
                );
                assert!(h.tramps.contains_key(&site), "{} のトランポリン", def.name);
            }
        }
        assert!(h.tramps.contains_key(&SITE_CALLOC));
        assert_eq!(h.scan_original, Some(0x0042_C700));
    }

    #[test]
    fn a_mismatch_anywhere_including_calloc_writes_nothing() {
        let sites = play_sites();
        for (i, (_, def)) in sites.iter().enumerate() {
            for pos in 0..def.expected.len() as u32 {
                for unreadable in [false, true] {
                    let mut f = FakeCode::new();
                    let a = def.addr + pos;
                    if unreadable {
                        f.mem.remove(&a);
                    } else {
                        *f.mem.get_mut(&a).unwrap() ^= 0x01;
                    }
                    let mut h = FakeHooks::default();
                    let (r, log) = install(&mut f, &mut h, Mode::Rollback, FORBIDDEN);
                    assert_eq!(
                        r,
                        PlayInstall::Refused,
                        "{} +{pos} {unreadable}: {log}",
                        def.name
                    );
                    assert!(
                        f.writes.is_empty(),
                        "{} が合わないのに書いた: {log}",
                        def.name
                    );
                    assert!(h.tramps.is_empty() && h.scan_original.is_none(), "{i}");
                    assert!(log.contains("どこにも書かない"), "{log}");
                }
            }
        }
    }

    #[test]
    fn install_refuses_delay_zero_entries_and_forbidden_ranges() {
        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        let (r, _) = install(&mut f, &mut h, Mode::Delay, FORBIDDEN);
        assert_eq!(r, PlayInstall::NotWanted);
        assert!(f.writes.is_empty() && h.tramps.is_empty());

        let mut f = FakeCode::new();
        let mut h = FakeHooks {
            zero_entry: Some(SITE_CALLOC),
            ..FakeHooks::default()
        };
        let (r, _) = install(&mut f, &mut h, Mode::Rollback, FORBIDDEN);
        assert_eq!(r, PlayInstall::Refused);
        assert!(f.writes.is_empty());

        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        let c = &CALLOC_SITES[0];
        let forbidden = [AddrRange {
            start: c.addr + 1,
            end: c.addr + 2,
        }];
        let (r, _) = install(&mut f, &mut h, Mode::Rollback, &forbidden);
        assert_eq!(r, PlayInstall::Refused);
        assert!(f.writes.is_empty());
    }

    pub(crate) fn scan_call_points_at_our_entry(f: &mut FakeCode) -> bool {
        let def = &RESIM_SITES[0];
        let mut head = [0u8; 5];
        assert!(f.read_code(def.addr, &mut head));
        let rel = i32::from_le_bytes(head[1..5].try_into().unwrap());
        head[0] == 0xE8 && (def.addr as i32 + 5 + rel) as u32 == FAKE_SCAN_ENTRY
    }

    pub(crate) fn scan_call_written_after_setup(wish: Mode, watching: bool) -> (bool, usize) {
        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        let mut out = Vec::new();
        install_play(wish, &mut f, &mut h, FORBIDDEN, &mut out);
        install_watch_scan(
            watching,
            h.scan_original.is_some(),
            &mut f,
            &mut h,
            FORBIDDEN,
            &mut out,
        );
        (scan_call_points_at_our_entry(&mut f), f.writes.len())
    }

    #[test]
    fn a_watcher_gets_only_the_scan_call_whatever_the_mode_and_a_player_in_delay_gets_nothing() {
        let (placed, writes) = scan_call_written_after_setup(Mode::Delay, true);
        assert!(placed, "観戦者の delay の実行で走査の call が書かれない");
        assert_eq!(writes, 1, "走査の call の 1 地点だけのはず");
        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        let mut out = Vec::new();
        assert!(install_watch_scan(
            true, false, &mut f, &mut h, FORBIDDEN, &mut out
        ));
        assert_eq!(f.writes.len(), 1);
        assert_eq!(
            f.writes[0].0,
            RESIM_SITES[0].addr + 1,
            "rel32 の 4 バイトだけ"
        );
        assert!(h.tramps.is_empty(), "確保の入口まで書いた");
        assert_eq!(h.scan_original, Some(0x0042_C700));
        assert_eq!(
            scan_call_written_after_setup(Mode::Delay, false),
            (false, 0)
        );
        assert_eq!(
            scan_call_written_after_setup(Mode::Rollback, false),
            (true, 5)
        );
        assert_eq!(
            scan_call_written_after_setup(Mode::Rollback, true),
            (true, 5)
        );
        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        assert!(!install_watch_scan(
            true, true, &mut f, &mut h, FORBIDDEN, &mut out
        ));
        assert!(f.writes.is_empty());
        let def = &RESIM_SITES[0];
        for pos in 0..def.expected.len() as u32 {
            for unreadable in [false, true] {
                let mut f = FakeCode::new();
                let a = def.addr + pos;
                if unreadable {
                    f.mem.remove(&a);
                } else {
                    *f.mem.get_mut(&a).unwrap() ^= 0x01;
                }
                let mut h = FakeHooks::default();
                let mut out = Vec::new();
                assert!(
                    !install_watch_scan(true, false, &mut f, &mut h, FORBIDDEN, &mut out),
                    "+{pos} {unreadable}"
                );
                assert!(
                    f.writes.is_empty(),
                    "+{pos} {unreadable}: 合わないのに書いた"
                );
            }
        }
        let forbidden = [AddrRange {
            start: def.addr + 1,
            end: def.addr + 2,
        }];
        let mut f = FakeCode::new();
        let mut h = FakeHooks::default();
        assert!(!install_watch_scan(
            true, false, &mut f, &mut h, &forbidden, &mut out
        ));
        assert!(f.writes.is_empty(), "禁止の範囲に重なるのに書いた");
    }

    #[test]
    fn the_queue_is_bounded_and_ordered() {
        let mut q: Queue<u32, 3> = Queue::new();
        assert!(q.push(1) && q.push(2) && q.push(3));
        assert!(!q.push(4));
        assert_eq!(q.pop(), Some(1));
        assert!(q.push(5));
        assert_eq!(
            (q.pop(), q.pop(), q.pop(), q.pop()),
            (Some(2), Some(3), Some(5), None)
        );
        assert!(q.is_empty());
    }

    /// Stage の色の 4 B（ゲームのメモリ）。
    fn tint_of(g: &FakeGame, side: usize) -> u32 {
        g.get(g.stage(side) + STAGE_TINT)
    }

    /// 製品の作り方（`PlayState::new` —— 表は `RENDER_RESETS`）か、表を空にした直す前の形。直した側を表の中身で決めない
    /// （本物の表が空になったら、直した側の検査が落ちるように）。
    fn state_for(fixed: bool) -> PlayState {
        if fixed {
            play_state()
        } else {
            PlayState::new_with_resets(Tables::th09_play(), &[], ARENA_BASE, ARENA_BYTES).unwrap()
        }
    }

    #[test]
    fn a_snapshot_taken_before_the_draw_holds_the_drawn_tint() {
        const RED: u32 = 0x80F0_0000;
        const DARK: u32 = 0x8030_3040;
        let after = u32::from_le_bytes(STAGE_TINT_AFTER);
        for fixed in [true, false] {
            let mut g = FakeGame::new();
            let mut st = state_for(fixed);
            let (s1, s2) = (g.stage(0), g.stage(1));
            for (s, tint) in [(s1, RED), (s2, DARK)] {
                g.mem.insert(s + STAGE_TINT, tint);
                g.mem.insert(s + STAGE_TINT_MODE, 0);
            }
            g.mem.insert(s1 + STAGE_TINT + 4, 0x1234_5678);
            g.mem.insert(s1 + STAGE_TINT - 4, 0x0BAD_F00D);
            {
                let mut w = GameWindow {
                    mem: &mut g,
                    st: &mut st,
                };
                w.save(7);
            }
            assert_eq!(
                (tint_of(&g, 0), tint_of(&g, 1)),
                (RED, DARK),
                "コピーを取っただけでゲームの色を書き換えた"
            );
            for s in [s1, s2] {
                g.mem.insert(s + STAGE_TINT, after);
            }
            g.mem.insert(s1 + STAGE_TINT + 4, 0);
            g.mem.insert(s1 + STAGE_TINT - 4, 0);
            {
                let mut w = GameWindow {
                    mem: &mut g,
                    st: &mut st,
                };
                w.restore(7);
            }
            assert_eq!(st.fault(), None);
            let s = st.stats();
            assert_eq!(g.get(s1 + STAGE_TINT + 4), 0x1234_5678);
            assert_eq!(g.get(s1 + STAGE_TINT - 4), 0x0BAD_F00D);
            assert_eq!(g.get(s1 + STAGE_TINT_MODE), 0);
            assert_eq!(s.tint_checked, 2, "{s:?}");
            assert_eq!(s.tint_at_save, 2, "{s:?}");
            if fixed {
                assert_eq!(
                    (tint_of(&g, 0), tint_of(&g, 1)),
                    (after, after),
                    "書き戻しで更新の色が戻った"
                );
                assert_eq!((s.reset_rows, s.reset_changed, s.reset_refused), (2, 2, 0));
                assert_eq!(s.tint_stale, 0);
            } else {
                assert_eq!((tint_of(&g, 0), tint_of(&g, 1)), (RED, DARK));
                assert_eq!((s.reset_rows, s.reset_changed), (0, 0));
                assert_eq!(s.tint_stale, 2, "直していない形の色の残りを数えない");
                g.mem.insert(s1 + STAGE_TINT, RED);
                g.mem.insert(s1 + STAGE_TINT_MODE, 1);
                g.mem.insert(s2 + STAGE_TINT, after);
                {
                    let mut w = GameWindow {
                        mem: &mut g,
                        st: &mut st,
                    };
                    w.save(8);
                    w.restore(8);
                }
                let s2nd = st.stats();
                assert_eq!(s2nd.tint_checked, 4);
                assert_eq!(s2nd.tint_stale, 2, "模様の間の色を残りと数えた");
            }
        }
    }

    #[test]
    fn the_cut_in_tint_does_not_come_back_after_it_ends() {
        const TRIGGER: u16 = 0x55;
        const AT: u32 = 40;
        for fixed in [true, false] {
            let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_time_sync(false));
            let mut st = state_for(fixed);
            let mut g = FakeGame::new();
            g.tint_on_p2 = Some(TRIGGER);
            let mut tinted_during = 0u32;
            let mut stale_draws = 0u32;
            let mut late_frames = 0u32;
            for k in 0..240u32 {
                if k >= 3 {
                    let base = k - 2;
                    let w = if base == AT {
                        TRIGGER
                    } else {
                        (base & 3) as u16
                    };
                    let inp = remote(&rb, base, &[w]);
                    rb.on_input(&inp);
                }
                let mut sent = 0u32;
                let _ = scan_point(
                    &mut rb,
                    &mut st,
                    &mut g,
                    &mut || (k as u16) & 7,
                    &mut |_| sent += 1,
                );
                let alphas = [tint_of(&g, 0) >> 24, tint_of(&g, 1) >> 24];
                if (AT + 2..AT + 20).contains(&k) && alphas.iter().any(|&a| a != 0) {
                    tinted_during += 1;
                }
                if k >= AT + TINT_TICKS + 30 {
                    late_frames += 1;
                    if alphas.iter().any(|&a| a != 0) {
                        stale_draws += 1;
                    }
                }
                g.play_and_draw();
                let _ = post_present(&mut rb, &mut st, &mut g);
            }
            let s = st.stats();
            assert_eq!(st.fault(), None);
            assert!(s.restores > 100, "書き戻しが起きていない: {s:?}");
            assert!(s.tint_at_save > 0, "{s:?}");
            assert!(
                tinted_during > 0,
                "模様が立っていない（検査の前提が崩れた）"
            );
            assert!(late_frames > 100);
            if fixed {
                assert_eq!(
                    stale_draws, 0,
                    "模様の後も描画が色を掛けた（{late_frames} フレームのうち）"
                );
                assert_eq!(s.tint_stale, 0, "{s:?}");
                assert!(
                    s.reset_changed > 0,
                    "コピーの中の色を 1 度も置き換えていない: {s:?}"
                );
                assert_eq!(s.reset_refused, 0);
            } else {
                assert!(
                    stale_draws > 0,
                    "表を空にしても色が残らない（不具合を再現できていない）"
                );
                assert!(
                    s.tint_stale > 0,
                    "直していない形の色の残りを数えない: {s:?}"
                );
            }
            assert_eq!(s.tint_checked, 2 * s.restores, "{s:?}");
        }
    }

    #[test]
    fn render_reset_rows_are_checked_when_the_state_is_made() {
        static UNKNOWN: [RbRenderReset; 1] = [RbRenderReset {
            block: "no_such_block",
            offset: 0,
            after: &[0],
            code_at: 0,
            code: &[],
            source: "",
        }];
        static CROSS: [RbRenderReset; 1] = [RbRenderReset {
            block: "p1_stage",
            offset: 0x65C4 - 2,
            after: &STAGE_TINT_AFTER,
            code_at: 0,
            code: &[],
            source: "",
        }];
        const VALID: RbRenderReset = RbRenderReset {
            block: "p1_stage",
            offset: 0x65C4 - 4,
            after: &STAGE_TINT_AFTER,
            code_at: 0,
            code: &[],
            source: "",
        };
        static FULL: [RbRenderReset; MAX_RENDER_RESETS] = [VALID; MAX_RENDER_RESETS];
        static MANY: [RbRenderReset; MAX_RENDER_RESETS + 1] = [VALID; MAX_RENDER_RESETS + 1];
        let mk = |t: &'static [RbRenderReset]| {
            PlayState::new_with_resets(Tables::th09_play(), t, ARENA_BASE, ARENA_BYTES)
        };
        assert!(mk(&UNKNOWN).is_err());
        let e = mk(&CROSS).err().expect("はみ出す欄を通した");
        assert!(e.contains("p1_stage"), "{e}");
        assert_eq!(
            mk(&FULL).map(|s| s.stats().reset_rows),
            Ok(MAX_RENDER_RESETS as u32)
        );
        assert!(mk(&MANY).is_err());
        assert_eq!(mk(RENDER_RESETS).unwrap().stats().reset_rows, 2);
        assert_eq!(play_state().stats().reset_rows, RENDER_RESETS.len() as u32);
    }

    #[test]
    fn the_draw_code_is_matched_before_the_table_is_used() {
        let code = |a: u32| -> Option<u32> {
            let i = a.checked_sub(STAGE_TINT_RESET_AT)? as usize;
            let b = |k: usize| u32::from(STAGE_TINT_RESET_CODE.get(i + k).copied().unwrap_or(0xCC));
            (i < STAGE_TINT_RESET_CODE.len()).then(|| b(0) | b(1) << 8 | b(2) << 16 | b(3) << 24)
        };
        assert_eq!(
            render_reset_code_matches(RENDER_RESETS, &mut |a| code(a)),
            Ok(())
        );
        let off_by_one = |a: u32| {
            code(a).map(|v| {
                if a == STAGE_TINT_RESET_AT + 1 {
                    v ^ 1
                } else {
                    v
                }
            })
        };
        assert!(render_reset_code_matches(RENDER_RESETS, &mut |a| off_by_one(a)).is_err());
        assert!(render_reset_code_matches(RENDER_RESETS, &mut |_| None).is_err());
        assert_eq!(render_reset_code_matches(&[], &mut |_| None), Ok(()));
    }

    #[test]
    fn summary_lines_state_the_counts_and_the_fault() {
        let s = PlayStats {
            pred_out_allocs: 12,
            pred_out_alive_at_restore: 3,
            saves: 41,
            reset_rows: 2,
            reset_changed: 37,
            reset_refused: 5,
            tint_checked: 66,
            tint_stale: 9,
            tint_at_save: 14,
            ..PlayStats::default()
        };
        let l = summary_lines(&s, Some(Fault::ListNotRestored)).join("\n");
        assert!(l.contains("予測の間の領域の外の確保 12"), "{l}");
        assert!(l.contains("生きていた外の塊 3"), "{l}");
        assert!(l.contains("並びが写したときと違う"), "{l}");
        assert!(
            l.contains("描画の後の値に置き換えた欄 37（コピー 41 回 × 2 行のうち。断った 5）"),
            "{l}"
        );
        assert!(
            l.contains("α が 0 でない）9（見た Stage 66 のうち）"),
            "{l}"
        );
        assert!(
            l.contains("コピーのときに Stage の色の α が 0 でなかった 14（"),
            "{l}"
        );
    }

    #[test]
    fn core_stats_lines_state_every_field() {
        let c = higanr_core::rollback::Stats {
            live_confirmed: 101,
            live_unconfirmed_real: 102,
            live_predicted: 103,
            resims: 104,
            stall_rewinds: 105,
            resim_confirmed_ticks: 106,
            resim_predicted_ticks: 107,
            saves: 108,
            restores: 109,
            waits_ahead: 110,
            waits_boundary: 111,
            waits_sync: 112,
            waits_sync_late: 124,
            holds_set: 113,
            holds_ended: 114,
            holds_released: 115,
            holds_rearmed: 116,
            discarded_ticks: 117,
            recaptured: 118,
            max_ahead_seen: 119,
            scenes_ended: 120,
            conflicts: 121,
            faults: 122,
            seg_d_log: std::array::from_fn(|i| (100 + i as u16, (i % 3) as u8)),
            seg_d_count: 18,
            delay_waits: 123,
            sync_samples: 4,
            sync_lead_sum: 4 * 5 * i64::from(higanr_core::rollback::SYNC_UNIT) / 2,
            sync_lead_max: 4 * higanr_core::rollback::SYNC_UNIT,
            sync_lead_min: -3 * higanr_core::rollback::SYNC_UNIT / 2,
            ..Default::default()
        };
        let l = core_stats_lines(&c).join("\n");
        for (label, v) in [
            ("本物の語 101", 101),
            ("未確定 102", 102),
            ("予測の語 103", 103),
            ("進め直し 104", 104),
            ("入力の地点で行った分 105", 105),
            ("確定 106", 106),
            ("予測 107", 107),
            ("コピー 108", 108),
            ("書き戻し 109", 109),
            ("先に行き過ぎ 110", 110),
            ("境目 111", 111),
            ("時刻合わせ 112", 112),
            ("300 フレーム以降 124", 124),
            ("立てた 113", 113),
            ("場面の終わり 114", 114),
            ("消えて続けた 115", 115),
            ("立て直した 116", 116),
            ("捨てた tick 117", 117),
            ("取得のやり直し 118", 118),
            ("最大 119", 119),
            ("場面の境目 120", 120),
            ("違う語 121", 121),
            ("内部の食い違い 122", 122),
            ("取得を待った回 123", 123),
            ("18 個のうち新しい 16 個", 18),
            ("標本 4 個", 4),
            ("平均 2.50", 250),
            ("最大 4.00", 400),
            ("最小 -1.50", -150),
        ] {
            assert!(l.contains(label), "{label}（欄の値 {v}）が文に無い: {l}");
        }
        // リングバッファの並び: 18 個決まっていれば、次に上書きされる枠（18 % 16 = 2）が一番古い。古い順は枠 2〜15、続けて枠 0・1。
        // 欄 `seg_d_log[i]` は seg = 100 + 枠の番号（上の組み立て）なので、並べ直しを間違えると seg の順が崩れる。
        let want = "seg102=D2 seg103=D0 seg104=D1 seg105=D2 seg106=D0 seg107=D1 seg108=D2 seg109=D0 seg110=D1 seg111=D2 seg112=D0 seg113=D1 seg114=D2 seg115=D0 seg100=D0 seg101=D1";
        assert!(l.contains(want), "{l}");
        let none = core_stats_lines(&higanr_core::rollback::Stats::default()).join("\n");
        assert!(none.contains("標本なし"), "{none}");
        assert!(!none.contains("NaN"), "{none}");
    }

    #[test]
    fn arming_for_delay_mode_writes_nothing_and_places_nothing() {
        let mut env = FakeCode::new();
        let before = env.mem.clone();
        let mut out = Vec::new();
        let ok = crate::rollback_play_win::arm(
            Mode::Delay,
            crate::rollback_play_win::DEFAULT_ARENA_BYTES,
            &mut env,
            FORBIDDEN,
            Some(1),
            &mut out,
        );
        assert!(!ok);
        assert!(
            env.writes.is_empty(),
            "delay 方式で書いた: {:?}",
            env.writes
        );
        assert_eq!(env.mem, before);
        assert!(!crate::rollback_play_win::armed());
        assert!(
            String::from_utf8_lossy(&out).contains("delay 方式"),
            "{}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn only_waits_for_words_are_retried() {
        assert!(retry_after_wait(Wait::Ahead));
        assert!(retry_after_wait(Wait::Boundary));
        assert!(!retry_after_wait(Wait::Fault));
        assert!(!retry_after_wait(Wait::NotCaptured));
    }

    struct Scripted {
        steps: std::collections::VecDeque<Step>,
        input_points: u32,
        waited: Vec<Wait>,
    }

    impl Machine for Scripted {
        fn mode(&self) -> Mode {
            Mode::Rollback
        }
        fn delay(&self) -> u8 {
            1
        }
        fn remote_next(&self) -> u32 {
            0
        }
        fn scene_ended(&self) -> bool {
            false
        }
        fn captured(&self) -> bool {
            true
        }
        fn capture(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
        fn input_point<W: Window>(&mut self, w: &mut W) -> Step {
            self.input_points += 1;
            let s = self.steps.pop_front().expect("台本の答えが尽きた");
            if let Step::Ran { frame, .. } = s {
                w.tick(Tick {
                    frame,
                    p1: 0,
                    p2: 0,
                    kind: TickKind::Live,
                });
            }
            s
        }
        fn wait(&mut self, why: Wait) -> bool {
            self.waited.push(why);
            true
        }
    }

    #[test]
    fn the_scan_point_retries_only_for_word_waits() {
        let ran = Step::Ran {
            frame: 0,
            real: true,
            confirmed: true,
        };
        let mut g = FakeGame::new();
        let mut st = play_state();
        let mut m = Scripted {
            steps: [Step::Wait(Wait::Ahead), Step::Wait(Wait::Boundary), ran].into(),
            input_points: 0,
            waited: Vec::new(),
        };
        let f = scan_point_with(&mut m, &mut st, &mut g);
        assert!(
            matches!(
                f,
                ScanFrame::Ran {
                    step: Step::Ran { .. },
                    ..
                }
            ),
            "{f:?}"
        );
        assert_eq!(m.input_points, 3);
        assert_eq!(m.waited, vec![Wait::Ahead, Wait::Boundary]);
        assert_eq!(g.scans.len(), 1);
        for why in [Wait::Fault, Wait::NotCaptured, Wait::Sync] {
            let mut g = FakeGame::new();
            let mut st = play_state();
            let mut m = Scripted {
                steps: [Step::Wait(why)].into(),
                input_points: 0,
                waited: Vec::new(),
            };
            let f = scan_point_with(&mut m, &mut st, &mut g);
            assert!(
                matches!(f, ScanFrame::Ran { step: Step::Wait(w), .. } if w == why),
                "{f:?}"
            );
            assert_eq!((m.input_points, m.waited.len()), (1, 0), "{why:?}");
            assert_eq!(g.scans, vec![(Order::RenderOnly, false)], "{why:?}");
        }
    }

    #[test]
    fn words_keep_ctrl_and_are_masked_before_they_reach_the_game_and_the_recorded_rows() {
        let mut g = FakeGame::new();
        let mut st = play_state();
        {
            let mut w = GameWindow {
                mem: &mut g,
                st: &mut st,
            };
            w.tick(Tick {
                frame: 3,
                p1: 0xFF35,
                p2: 0x1334,
                kind: TickKind::Predicted,
            });
            w.confirmed(3, 0xFF35, 0x1334);
        }
        assert_eq!(
            g.scans,
            vec![(
                Order::Feed {
                    p1: 0x0135,
                    p2: 0x0134
                },
                true
            )]
        );
        let mut rows = Vec::new();
        st.drain_confirmed(&mut |f, p1, p2| rows.push((f, p1, p2)));
        assert_eq!(rows, vec![(3, 0x0135, 0x0134)]);
    }

    /// 直す前の製品の経路の .data の表（7 行 ＋ つなぎ。`globals_c` が fps の計測の 6 語を含む）。対照の実行だけが使う。
    static OLD_PLAY_DATA: [RbData; 8] = [
        ROLLBACK_DATA[0],
        ROLLBACK_DATA[1],
        ROLLBACK_DATA[2],
        ROLLBACK_DATA[3],
        ROLLBACK_DATA[4],
        ROLLBACK_DATA[5],
        ROLLBACK_DATA[6],
        ROLLBACK_PLAY_DATA[7],
    ];

    fn run_late_remote(st: &mut PlayState, g: &mut FakeGame, frames: u32) {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8).with_time_sync(false));
        for k in 0..frames {
            if k >= 3 {
                let base = k - 2;
                let inp = remote(&rb, base, &[(base & 3) as u16]);
                rb.on_input(&inp);
            }
            let mut sent = 0u32;
            let _ = scan_point(&mut rb, st, g, &mut || (k as u16) & 7, &mut |_| sent += 1);
            g.play_and_draw();
            let _ = post_present(&mut rb, st, g);
        }
    }

    #[test]
    fn fps_words_are_left_out_of_the_play_copy() {
        for fixed in [true, false] {
            let mut st = if fixed {
                play_state()
            } else {
                PlayState::new(
                    Tables {
                        data: &OLD_PLAY_DATA,
                        ..Tables::th09_play()
                    },
                    ARENA_BASE,
                    ARENA_BYTES,
                )
                .unwrap()
            };
            let mut g = FakeGame::new();
            g.fps_model = true;
            run_late_remote(&mut st, &mut g, 300);
            let s = st.stats();
            assert_eq!(st.fault(), None);
            assert!(s.restores > 200, "書き戻しが起きていない: {s:?}");
            assert_eq!(s.fps_words_checked, s.restores, "{s:?}");
            assert_eq!(s.fps_n, 300, "Present の後に毎回読んでいない: {s:?}");
            assert_eq!(s.fps_hist.iter().sum::<u32>(), s.fps_n);
            let at_60 = s.fps_hist[fps_bucket(60)];
            if fixed {
                assert_eq!(s.fps_words_restored, 0, "fps の計測の欄を書き戻した: {s:?}");
                assert!(
                    at_60 >= s.fps_n - 31,
                    "直した表で fps の表示が 60 に集まらない: {:?}",
                    s.fps_hist
                );
            } else {
                assert!(
                    s.fps_words_restored > 100,
                    "直す前の表で書き戻しを数えない: {s:?}"
                );
                assert!(
                    at_60 < s.fps_n / 2,
                    "直す前の表でも fps の表示が 60 のまま（不具合を再現できていない）: {:?}",
                    s.fps_hist
                );
            }
        }
    }

    #[test]
    fn the_play_copy_drops_exactly_the_fps_timing_words() {
        use crate::th09_addr::{FPS_TIMING_END, FPS_TIMING_START, ROLLBACK_COMPARE_SKIPS};
        let covered = |rows: &[RbData], a: u32| rows.iter().any(|d| (d.start..d.end).contains(&a));
        let fps = ROLLBACK_COMPARE_SKIPS
            .iter()
            .find(|s| s.name == "fps_timing")
            .expect("比べの除外に fps_timing が無い");
        assert_eq!(
            fps.kind,
            RbSkipKind::Range {
                start: FPS_TIMING_START,
                end: FPS_TIMING_END
            }
        );
        assert_eq!(
            (FPS_TIMING_START, FPS_TIMING_END),
            (0x004B_42B8, 0x004B_42D0)
        );
        for a in (FPS_TIMING_START - 8..FPS_TIMING_END + 8).step_by(4) {
            let in_fps = (FPS_TIMING_START..FPS_TIMING_END).contains(&a);
            assert_eq!(
                covered(ROLLBACK_DATA, a),
                a >= FPS_TIMING_START,
                "E1 の表 {a:#X}"
            );
            assert_eq!(
                covered(ROLLBACK_PLAY_DATA, a),
                !in_fps && a >= FPS_TIMING_START,
                "製品の経路の表 {a:#X}"
            );
        }
        let gc = ROLLBACK_PLAY_DATA
            .iter()
            .find(|d| d.name == "globals_c")
            .expect("globals_c");
        let e1 = ROLLBACK_DATA
            .iter()
            .find(|d| d.name == "globals_c")
            .unwrap();
        assert_eq!((gc.start, gc.end), (FPS_TIMING_END, e1.end));
    }

    #[test]
    fn background_vms_are_put_back_and_advance_one_tick_per_draw() {
        for fixed in [true, false] {
            let t = if fixed {
                Tables::th09_play()
            } else {
                Tables {
                    vm_arrays: &[],
                    ..Tables::th09_play()
                }
            };
            let mut st = PlayState::new(t, ARENA_BASE, ARENA_BYTES).unwrap();
            let mut g = FakeGame::new();
            g.set_bg(54);
            run_late_remote(&mut st, &mut g, 240);
            let s = st.stats();
            assert_eq!(st.fault(), None, "{:?}", st.place_diag());
            assert!(s.restores > 150, "書き戻しが起きていない: {s:?}");
            let fast: u32 = s.bg_step[bg_step_bucket(Some(2))..=bg_step_bucket(Some(8))]
                .iter()
                .sum();
            assert_eq!(s.bg_step.iter().sum::<u32>(), 2 * 239, "{:?}", s.bg_step);
            assert_eq!(s.bg_step[bg_step_bucket(None)], 0, "{:?}", s.bg_step);
            if fixed {
                assert_eq!(
                    fast, 0,
                    "描画 1 回に背景が 2 tick 以上進んだ: {:?}",
                    s.bg_step
                );
                assert!(s.bg_step[1] > 2 * 200, "{:?}", s.bg_step);
                assert_eq!(s.bg_restored, 2 * s.restores, "{s:?}");
                assert_eq!(s.bg_count_max, 54);
                assert_eq!(g.bg_clock(0, 0), g.bg_clock(1, 53));
            } else {
                assert!(
                    fast > 100,
                    "配列をコピーしなくても背景が速くならない: {:?}",
                    s.bg_step
                );
                assert_eq!(s.bg_restored, 0);
            }
        }
    }

    #[test]
    fn background_vm_count_limits() {
        let t = Tables::th09_play();
        let v = &t.vm_arrays[0];
        assert_eq!((v.max, v.elem), (BG_VM_MAX, 0x2A4));
        let shape = |count: u32, arr: u32| {
            let mut g = FakeGame::new();
            let s = g.stage(0);
            g.mem.insert(s + crate::th09_addr::STAGE_BG_VMS, arr);
            g.mem.insert(s + crate::th09_addr::STAGE_BG_VM_COUNT, count);
            vm_array_shape(&t, v, &mut AsLayout(&mut g), 0)
        };
        let ok = |count: u32, arr: u32| shape(count, arr).map(|s| (s.place(), s.over(v)));
        assert_eq!(ok(54, 0x0E00_0000), Ok(((0x0E00_0000, 54 * 0x2A4), false)));
        assert_eq!(
            ok(BG_VM_MAX, 0x0E00_0000),
            Ok(((0x0E00_0000, BG_VM_MAX * 0x2A4), false))
        );
        assert_eq!(ok(BG_VM_MAX + 1, 0x0E00_0000), Ok(((0, 0), true)));
        assert_eq!(ok(0, 0x0E00_0000), Ok(((0, 0), false)));
        assert_eq!(ok(u32::MAX, 0x0E00_0000), Ok(((0, 0), false)), "負の本数");
        assert_eq!(ok(54, 0), Ok(((0, 0), false)));
        let mut g = FakeGame::new();
        g.mem.insert(ROLLBACK_BLOCKS[0].ptr_at, 0);
        assert_eq!(
            vm_array_shape(&t, v, &mut AsLayout(&mut g), 7),
            Err(LayoutError::Null(7))
        );
        let mut st = play_state();
        let mut g = FakeGame::new();
        g.set_bg(BG_VM_MAX + 1);
        run_late_remote(&mut st, &mut g, 120);
        let s = st.stats();
        assert_eq!(st.fault(), None);
        assert!(s.restores > 50);
        assert_eq!(s.bg_restored, 0, "上限を越えた陣を書き戻した");
        assert_eq!(s.bg_over, 2 * s.layout_fills, "{s:?}");
        assert_eq!(s.bg_count_max, BG_VM_MAX + 1);
        assert!(s.bg_step[bg_step_bucket(Some(2))..].iter().sum::<u32>() > 0);
        let text = summary_lines(&s, None).join("\n");
        assert!(text.contains("越えて写さなかった陣 2"), "{text}");
    }

    #[test]
    fn the_play_layout_holds_two_background_arrays() {
        let play = Layout::plan(&Tables::th09_play(), ARENA_BASE, ARENA_BYTES).unwrap();
        let e1 = Layout::plan(&Tables::th09(), ARENA_BASE, ARENA_BYTES).unwrap();
        let vms: Vec<&Region> = play
            .regions()
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::VmArray(_)))
            .collect();
        assert_eq!(vms.len(), 2);
        assert!(vms.iter().all(|r| r.cap == BG_VM_MAX * 0x2A4));
        assert_eq!(play.n, MAX_REGIONS);
        assert!(
            !e1.regions()
                .iter()
                .any(|r| matches!(r.kind, RegionKind::VmArray(_)))
        );
        assert_eq!(
            play.buffer_bytes(),
            e1.buffer_bytes() + 0x40 + 2 * BG_VM_MAX * 0x2A4 - 0x18
        );
        let mut g = FakeGame::new();
        g.set_bg(3);
        let mut l = play;
        fill_layout(&mut l, &Tables::th09_play(), &mut AsLayout(&mut g), 0).unwrap();
        let r = l
            .regions()
            .iter()
            .find(|r| r.kind == RegionKind::VmArray(0))
            .copied()
            .unwrap();
        assert_eq!((r.addr, r.len), (0x0E00_0000, 3 * 0x2A4));
        assert!(in_layout(&l, r.addr + r.len - 4));
        assert!(!in_layout(&l, r.addr + r.len), "配列の後ろを範囲の中と見た");
    }

    #[test]
    fn a_background_array_that_changed_shape_is_not_put_back() {
        let mut g = FakeGame::new();
        g.set_bg(10);
        let mut st = play_state();
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .save(1);
        let s = g.stage(1);
        g.mem.insert(s + crate::th09_addr::STAGE_BG_VM_COUNT, 11);
        GameWindow {
            mem: &mut g,
            st: &mut st,
        }
        .restore(1);
        assert_eq!(st.fault(), Some(Fault::PlacesMoved));
        let d = st.place_diag().expect("動いた範囲");
        assert_eq!(d.kind, RegionKind::VmArray(1));
        assert_eq!(d.now, None, "長さが違うのに今のアドレスを言った");
        assert!(d.text(&Tables::th09_play()).contains("p2_bgscroll"));
    }

    #[test]
    fn the_m8_7_lines_say_what_was_measured() {
        let mut s = PlayStats::default();
        let t = summary_lines(&s, None).join("\n");
        assert!(
            t.contains("fps の表示") && t.contains("なし（読めた回が 0）"),
            "{t}"
        );
        s.fps_n = 3;
        s.fps_sum = 179;
        s.fps_hist[fps_bucket(60)] = 2;
        s.fps_hist[fps_bucket(59)] = 1;
        s.bg_step[1] = 5;
        s.bg_step[bg_step_bucket(Some(9))] = 2;
        s.fps_words_restored = 4;
        s.fps_words_checked = 9;
        let t = summary_lines(&s, None).join("\n");
        for want in [
            "59: 1 / 60: 2",
            "平均 59.67",
            "変わった書き戻し 4（見た 9 のうち",
            "1: 5 / 8 以上: 2",
        ] {
            assert!(t.contains(want), "{want} が無い: {t}");
        }
        assert_eq!(fps_bucket(-3), 0);
        assert_eq!(fps_bucket(55), 5);
        assert_eq!(fps_bucket(62), 12);
        assert_eq!(fps_bucket(i16::MAX), FPS_BUCKETS - 1);
        assert_eq!(FPS_LABELS[fps_bucket(60)], "60");
    }

    #[test]
    fn m8_7_values_appear_in_the_note() {
        use crate::th09_addr::{
            FPS_DRAWS, FPS_TIMING_END, FPS_TIMING_START, STAGE_BG_VM_COUNT, STAGE_BG_VMS,
        };
        let start = NOTE.find("## 31. ").expect("ノートに §31 が無い");
        let sec = &NOTE[start..];
        for want in [
            format!("`0x{FPS_TIMING_START:08X}`〜`0x{FPS_TIMING_END:08X}`"),
            format!("`0x{FPS_DRAWS:08X}`"),
            format!("`0x{FPS_SHOWN:08X}`"),
            "`0x004316C2`〜`0x004317F3`".to_string(),
            format!("`+0x{STAGE_BG_VMS:X}`"),
            format!("`+0x{STAGE_BG_VM_COUNT:X}`"),
            format!("`+0x{VM_CLOCK:X}`"),
            format!("`+0x{VM_CUR_INSTR:X}`"),
            format!("{BG_VM_MAX} 本"),
            "`0x402628 call 0x401340`".to_string(),
            "`0x401241 call 0x436f30`".to_string(),
        ] {
            assert!(sec.contains(&want), "ノート §31 に {want} が無い");
        }
    }
}
