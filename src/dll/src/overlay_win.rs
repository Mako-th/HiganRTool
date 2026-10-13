//! D3D8 と GDI の層（unsafe はここにもある。1 つずつ `// SAFETY:`）: 描画の入口（`draw.rs` の `draw_entry`）が `OVERLAY_ON` を見て呼ぶ
//! `overlay_on_draw`、文字の表の置き場、読み取りのスレッドで GDI が文字の表を描く `gdi_atlas`、本物の実装 `RealOverlayHost`。
//!
//! - 文字の表は読み取りのスレッドが GDI で 1 回だけメモリの面に描く。ゲームのスレッドは最初の 1 回だけ D3D8 のテクスチャ
//!   （`D3DPOOL_MANAGED`。デバイスの Reset を越えて残る）を作ってコピー、以後は毎フレーム頂点をスタックの固定の配列に作って
//!   `DrawPrimitiveUP` で出す。
//! - **ゲームの描画の状態は、描く前に自分で読み（Get*）、描いた後に読んだ値へ戻す。** state block は使わない: (1) 取り込んだテクスチャ・
//!   頂点バッファへの参照を次に取り込むまで持ち続けるので、ゲームが Reset する（`0x0042DB88`。戻り値を見ずに毎フレームやり直す）ときに
//!   D3DPOOL_DEFAULT の物を生かしたままにして Reset を失敗させうる。(2) Reset の前に消す流儀があるが、ゲームの Reset はこちらの入口の後
//!   （Present のラッパの中）で起きるので消す機会が無い。(3) ゲームは自分の描画の状態を覚えていて、同じなら Set を飛ばす（頂点の形の覚え
//!   `[AnmManager+0x12886]`、霧の覚え `[Supervisor+0x794]`）ので、戻し漏れはゲームの絵を壊す。戻す一覧は目で数えられる形にした。
//! - デバイスは `D3DCREATE_PUREDEVICE` 無しで作られている（behavior flags は `0x40` か `0x20`。`0x0042DF04` / `0x0042DF41` / `0x0042DF7E`）
//!   ので Get* は使える。`D3DCREATE_MULTITHREADED` も無いので、ゲームのスレッド以外から呼んではならない。
//! - `TestCooperativeLevel` が D3D_OK でない間は描かない。描けない・作れないときは描くのをやめ、理由と HRESULT を 1 つだけ残す。
//!   ゲームのスレッドでは確保・ファイル・ロック待ちをしない。中は全部 `catch_unwind`。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "overlay_win.rs は 32 ビット（i686）専用（D3D8 の vtable と 32 ビットのアドレスを前提にしている）"
);

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use higanr_core::overlay::{
    self as ov, AtlasSpec, FVF, GAME_ADVANCE, GAME_GLYPHS, GAME_SLOT_H, GAME_SLOT_W, GameGlyphSrc,
    MAX_VERTS_ALL, PLACE_BOTTOM_RIGHT, STYLE_DEFAULT, Status, VERTEX_SIZE, Vertex,
};
use higanr_core::shot::D3DFMT_A8R8G8B8;

use crate::net_ui::{BOARD, draw_board};
use crate::overlay::{
    Fail, GameAddrs, GameFontWhy, GfGate, OverlayCounters, OverlayHost, TEXT, TEXT_UNITS, TIMINGS,
    TimingSnapshot, Updater, gf_gate, gf_lock_gives_up, probe_game_font, swap_should_try,
};
use crate::pacing_win::{qpc, qpf};
use crate::th09_addr::{
    ANM_MANAGER_PTR, ASCII_ANM_LOADED_PTR, D3D8_SURF_RELEASE_SLOT, PP_HEIGHT, PP_WIDTH,
    SURFACE_DESC_FORMAT, SURFACE_DESC_HEIGHT, SURFACE_DESC_WIDTH, TEX_GETLEVELDESC_SLOT,
};
use crate::win::{read_own_into, read_own_memory};

// ---- D3D8 の決まり（SDK の d3d8.h / d3d8types.h の値）----
//
// vtable の番号（バイト）は IDirect3DDevice8 の並び（SDK）。花映塚の自分の呼び出しで要所を確かめた: Reset `0x38`（`0x0042DB88`）/
// Present `0x3C` / GetBackBuffer `0x40` / CreateTexture `0x50`（`0x00452D52` の 7 引数）/ BeginScene `0x88` / EndScene `0x8C` /
// SetRenderState `0xC8`（`0x0042FF2C` で FOGENABLE = 0x1C を 0 に）/ SetTexture `0xF4`（`0x0042DD4B`）/ SetTextureStageState `0xFC` /
// DrawPrimitiveUP `0x120`（`0x00417447`: 種類 5・数 2・stride 0x14）/ SetVertexShader `0x130`（`0x004396E2` で `0x144`）/
// SetStreamSource `0x14C`（`0x0043B478`: 0 番・stride 0x14）。Get の側（`0xCC` `0xF0` `0xF8` `0x134` `0x150` `0xA4`）と
// TestCooperativeLevel `0x0C`・SetViewport `0xA0`、テクスチャの LockRect `0x40` / UnlockRect `0x44` は、ゲームが呼ばないので
// SDK の並びだけが根拠（実機で見る）。Set と Get は SDK で隣り合い、GetTexture / GetTextureStageState は Set の 1 つ前。
const DEV_TESTCOOPERATIVELEVEL: u32 = 0x0C;
const DEV_CREATETEXTURE: u32 = 0x50;
const DEV_SETVIEWPORT: u32 = 0xA0;
const DEV_GETVIEWPORT: u32 = 0xA4;
const DEV_SETRENDERSTATE: u32 = 0xC8;
const DEV_GETRENDERSTATE: u32 = 0xCC;
const DEV_GETTEXTURE: u32 = 0xF0;
const DEV_SETTEXTURE: u32 = 0xF4;
const DEV_GETTEXTURESTAGESTATE: u32 = 0xF8;
const DEV_SETTEXTURESTAGESTATE: u32 = 0xFC;
const DEV_DRAWPRIMITIVEUP: u32 = 0x120;
const DEV_SETVERTEXSHADER: u32 = 0x130;
const DEV_GETVERTEXSHADER: u32 = 0x134;
const DEV_SETSTREAMSOURCE: u32 = 0x14C;
const DEV_GETSTREAMSOURCE: u32 = 0x150;
/// IDirect3DTexture8 の LockRect / UnlockRect（IUnknown 3・IDirect3DResource8 8・IDirect3DBaseTexture8 3・GetLevelDesc・
/// GetSurfaceLevel の後の 16 番目・17 番目）。
const TEX_LOCKRECT: u32 = 0x40;
const TEX_UNLOCKRECT: u32 = 0x44;
/// GetLevelDesc は同じ並びの 12 番目（`th09_addr::TEX_GETLEVELDESC_SLOT` = 0x38。ゲーム自身が呼んでいる）。
const TEX_GETLEVELDESC: u32 = TEX_GETLEVELDESC_SLOT;
/// `D3DLOCK_READONLY`（`0x10`）。ゲームのテクスチャを開くのはこの旗だけ（読むだけ）。
const D3DLOCK_READONLY: u32 = 0x10;
/// IUnknown の Release（どの COM の物も同じ番号。撮影の面の Release と同じ値なので、その定数を使う）。
const COM_RELEASE: u32 = D3D8_SURF_RELEASE_SLOT;

/// `D3DPOOL_MANAGED`（Reset を越えて残る。ゲームが Reset の前に何も片付けなくてよい）。
const D3DPOOL_MANAGED: u32 = 1;
const D3DPT_TRIANGLELIST: u32 = 4;

// レンダーステートの番号（`D3DRENDERSTATETYPE`）と値。FOGENABLE = 28（0x1C）はゲームの `0x0042FF29` と合う。
const D3DRS_ZENABLE: u32 = 7;
const D3DRS_FILLMODE: u32 = 8;
const D3DRS_ZWRITEENABLE: u32 = 14;
const D3DRS_ALPHATESTENABLE: u32 = 15;
const D3DRS_SRCBLEND: u32 = 19;
const D3DRS_DESTBLEND: u32 = 20;
const D3DRS_CULLMODE: u32 = 22;
const D3DRS_ALPHABLENDENABLE: u32 = 27;
const D3DRS_FOGENABLE: u32 = 28;
const D3DRS_STENCILENABLE: u32 = 52;
const D3DRS_COLORWRITEENABLE: u32 = 168;
const D3DRS_BLENDOP: u32 = 171;
const D3DFILL_SOLID: u32 = 3;
const D3DBLEND_SRCALPHA: u32 = 5;
const D3DBLEND_INVSRCALPHA: u32 = 6;
const D3DCULL_NONE: u32 = 1;
const D3DBLENDOP_ADD: u32 = 1;

// テクスチャステージの番号（`D3DTEXTURESTAGESTATETYPE`。D3D8 ではサンプラの状態もここ）と値。
const D3DTSS_COLOROP: u32 = 1;
const D3DTSS_COLORARG1: u32 = 2;
const D3DTSS_COLORARG2: u32 = 3;
const D3DTSS_ALPHAOP: u32 = 4;
const D3DTSS_ALPHAARG1: u32 = 5;
const D3DTSS_ALPHAARG2: u32 = 6;
const D3DTSS_TEXCOORDINDEX: u32 = 11;
const D3DTSS_ADDRESSU: u32 = 13;
const D3DTSS_ADDRESSV: u32 = 14;
const D3DTSS_MAGFILTER: u32 = 16;
const D3DTSS_MINFILTER: u32 = 17;
const D3DTSS_MIPFILTER: u32 = 18;
const D3DTSS_TEXTURETRANSFORMFLAGS: u32 = 24;
const D3DTOP_DISABLE: u32 = 1;
const D3DTOP_MODULATE: u32 = 4;
const D3DTA_DIFFUSE: u32 = 0;
const D3DTA_TEXTURE: u32 = 2;
const D3DTADDRESS_CLAMP: u32 = 3;
const D3DTEXF_NONE: u32 = 0;
const D3DTEXF_POINT: u32 = 1;

/// 描く間だけ置くレンダーステート（番号, 値）。深さ・ステンシル・霧・カリングを切り、α で重ねる。ゲームの値と
/// 同じものは Set しない（戻しもしない）。
const RS: [(u32, u32); 12] = [
    (D3DRS_ZENABLE, 0),
    (D3DRS_FILLMODE, D3DFILL_SOLID),
    (D3DRS_ZWRITEENABLE, 0),
    (D3DRS_ALPHATESTENABLE, 0),
    (D3DRS_SRCBLEND, D3DBLEND_SRCALPHA),
    (D3DRS_DESTBLEND, D3DBLEND_INVSRCALPHA),
    (D3DRS_CULLMODE, D3DCULL_NONE),
    (D3DRS_ALPHABLENDENABLE, 1),
    (D3DRS_FOGENABLE, 0),
    (D3DRS_STENCILENABLE, 0),
    (D3DRS_COLORWRITEENABLE, 0xF),
    (D3DRS_BLENDOP, D3DBLENDOP_ADD),
];

/// 描く間だけ置くテクスチャステージの状態（段, 番号, 値）。段 0 はテクスチャの α × 頂点の色、点で拾う（文字の表を
/// 1 対 1 で貼るので補間しない）。段 1 は切る（ゲームが段 1 を使っていても、この描画には混ぜない）。
const TSS: [(u32, u32, u32); 15] = [
    (0, D3DTSS_COLOROP, D3DTOP_MODULATE),
    (0, D3DTSS_COLORARG1, D3DTA_TEXTURE),
    (0, D3DTSS_COLORARG2, D3DTA_DIFFUSE),
    (0, D3DTSS_ALPHAOP, D3DTOP_MODULATE),
    (0, D3DTSS_ALPHAARG1, D3DTA_TEXTURE),
    (0, D3DTSS_ALPHAARG2, D3DTA_DIFFUSE),
    (0, D3DTSS_TEXCOORDINDEX, 0),
    (0, D3DTSS_ADDRESSU, D3DTADDRESS_CLAMP),
    (0, D3DTSS_ADDRESSV, D3DTADDRESS_CLAMP),
    (0, D3DTSS_MAGFILTER, D3DTEXF_POINT),
    (0, D3DTSS_MINFILTER, D3DTEXF_POINT),
    (0, D3DTSS_MIPFILTER, D3DTEXF_NONE),
    (0, D3DTSS_TEXTURETRANSFORMFLAGS, 0),
    (1, D3DTSS_COLOROP, D3DTOP_DISABLE),
    (1, D3DTSS_ALPHAOP, D3DTOP_DISABLE),
];

/// 文字の表の字体の候補（先に合ったものを使う。`gdi_atlas`）。ゲーム自身は `CreateFontA(2h, …, SHIFTJIS_CHARSET, ANTIALIASED_QUALITY,
/// "ＭＳ ゴシック")`（`0x004364C0`、字体名は `0x00490F40`）。GDI が返す名前は環境で「MS Gothic」のこともあるので両方書く。
/// Yu Gothic UI と Meiryo UI は MS Gothic が無い環境の代わり。
pub const FONT_FACES: [&str; 4] = ["ＭＳ ゴシック", "MS Gothic", "Yu Gothic UI", "Meiryo UI"];
/// 半角の枠の高さ（画素。GDI の `CreateFontW` の正の高さ = 枠の高さ。全角は幅がその 2 枠ぶん = 高さと同じ）。
pub const FONT_HEIGHT: i32 = 14;
/// 文字の表を描く倍率（この倍の大きさで描いて縮める。ゲームも 2 倍で描く）。
pub const FONT_SCALE: u32 = 2;

// ---- 静的変数（ゲームのスレッドと読み取りのスレッドの受け渡し）----

/// 文字の描画が有効か。描画の入口のアセンブリが `cmp dword ptr [OVERLAY_ON], 0` で見て、0 でなければ `overlay_on_draw` を呼ぶ。
/// 読み取りのスレッドが文字の表とアドレスを置いた後で 1 にし、ゲームのスレッドが描けない理由を見つけたら 0 にする。
pub static OVERLAY_ON: AtomicU32 = AtomicU32::new(0);
/// デバイスへのポインタの置き場（花映塚では `D3D8_DEVICE_PTR`）と present parameters のコピー（`D3D8_PRESENT_PARAMS`）。
/// 検査では検査の静的変数のアドレス。`prepare` が読めると確かめてから置く。
static DEVICE_PTR_ADDR: AtomicU32 = AtomicU32::new(0);
static PP_ADDR: AtomicU32 = AtomicU32::new(0);

/// 文字の表（割り付けとテクスチャの画素）。`place_atlas` が確保して、プロセスが終わるまで返さない。
struct Atlas {
    spec: AtlasSpec,
    texels: Vec<u32>,
}
static ATLAS: AtomicUsize = AtomicUsize::new(0);

/// 自分のテクスチャと、それを作ったデバイス（ゲームのスレッドだけが書く）。
static TEX: AtomicU32 = AtomicU32::new(0);
static TEX_DEVICE: AtomicU32 = AtomicU32::new(0);

static DRAWN: AtomicU32 = AtomicU32::new(0);
static LOST: AtomicU32 = AtomicU32::new(0);
static NO_DEVICE: AtomicU32 = AtomicU32::new(0);
static TORN: AtomicU32 = AtomicU32::new(0);
static DRAW_FAILED: AtomicU32 = AtomicU32::new(0);
static SET_FAILED: AtomicU32 = AtomicU32::new(0);
static PANICS: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);
static FAIL_HR: AtomicU32 = AtomicU32::new(0);
static SETUP_TICKS: AtomicU32 = AtomicU32::new(0);
static UPDATER_STARTED: AtomicU32 = AtomicU32::new(0);

// ---- ゲームの文字（ascii.anm の絵）を 1 回だけコピーする ----
//
// 1. ゲームのスレッド（描画の入口）が、自分のテクスチャを作った後、30 フレームに 1 回 `probe_game_font` で ascii.anm の読み込みを
//    確かめる。通ったら、ゲームのテクスチャを `LockRect(READONLY)` で開き、英数字の絵を作業場 `SCRATCH`（読み取りのスレッドが先に
//    確保した物）へ ARGB でコピーして閉じる（`GF_STATE` = `GF_CAPTURED`）。ゲームのメモリには 1 バイトも書かない。
// 2. 文の更新スレッド（読み取りのスレッド）が、作業場から「ゲームの文字をコピーした文字の表」を作り、`ATLAS` を差し替える
//    （`GF_STATE` = `GF_APPLIED`）。ヒープを使う仕事は全部ここ。
// 3. ゲームのスレッドが、`ATLAS` が変わったのを見て自分のテクスチャを作り直す（`TEX_ATLAS`）。以後の毎フレームの描画は今までと同じ。
//    コピーした後はゲームのテクスチャに触らない。
/// いまの `TEX` を作った文字の表（`Atlas` のアドレス）。`ATLAS` が違う値になったら作り直す。
static TEX_ATLAS: AtomicUsize = AtomicUsize::new(0);
/// 状態: 試し中 / コピーした（更新スレッドが文字の表を作る前）/ 文字の表に反映した / 断った（もう試さない）/ 更新スレッドが文字の表を作れなかった。
const GF_TRYING: u32 = 0;
const GF_CAPTURED: u32 = 1;
const GF_APPLIED: u32 = 2;
const GF_REFUSED: u32 = 3;
const GF_APPLY_FAILED: u32 = 4;
static GF_STATE: AtomicU32 = AtomicU32::new(GF_TRYING);
/// 最後の理由（`GameFontWhy` の番号）と、試した回数（LockRect の失敗を数える）。
static GF_WHY: AtomicU32 = AtomicU32::new(0);
static GF_LOCK_FAILS: AtomicU32 = AtomicU32::new(0);
/// 描画の入口を通った回数（試すのは `GF_EVERY` 回に 1 回、`GF_MAX_ATTEMPTS` 回まで —— `gf_gate`）と、試した回数。
static GF_FRAMES: AtomicU32 = AtomicU32::new(0);
static GF_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
/// 試しの上限であきらめたとき（`GameFontWhy::GaveUp`）の、その直前の待つ理由（本ログ用）。
static GF_LAST_WAIT: AtomicU32 = AtomicU32::new(0);

// ---- 文字の表の差し替えでテクスチャを作り直すのに失敗したときの数え（ゲームのスレッドだけが書く）----
/// ここまでに作り直しを試した回数（成功したら 0 に戻す）と、最後の失敗からの描画の入口の回数（`swap_should_try` が見る）。
static SWAP_TRIES: AtomicU32 = AtomicU32::new(0);
static SWAP_SINCE: AtomicU32 = AtomicU32::new(0);
/// 作り直しの失敗の合計と、最後の失敗の理由（`Fail` の番号）と HRESULT（本ログ用。描くのは止めない）。
static SWAP_FAILS: AtomicU32 = AtomicU32::new(0);
static SWAP_FAIL_WHY: AtomicU32 = AtomicU32::new(0);
static SWAP_FAIL_HR: AtomicU32 = AtomicU32::new(0);
/// コピーした結果の数字（本ログ用）: 実際の形式・テクスチャの幅・高さ・コピーした文字の数・sprite[0x20]（`@`）の絵の幅・高さ・画面の幅・高さ。
static GF_INFO: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
/// コピーにかかった QPC のカウント（LockRect の前から UnlockRect の後まで）。
static GF_TICKS: AtomicU32 = AtomicU32::new(0);
static SWAP_TICKS: AtomicU32 = AtomicU32::new(0);
/// 更新スレッドが文字の表を作れなかったときの理由の文（更新スレッドだけが書き、本ログが読む）。
static GF_NOTE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static GF_LOADED_PTR: AtomicU32 = AtomicU32::new(ASCII_ANM_LOADED_PTR);
static GF_MGR_PTR: AtomicU32 = AtomicU32::new(ANM_MANAGER_PTR);

/// 作業場: 文字 1 つあたり `SLOT_PX` 画素の ARGB（幅 `GAME_SLOT_W` の行）＋ 大きさの 4 語（絵の幅・高さ・画面の幅・高さ。幅 0 は
/// 「コピーしていない」）。`GAME_GLYPHS` 文字ぶん。読み取りのスレッドが `ensure_scratch` で確保して返さない。書くのはゲームのスレッドだけ
/// （`GF_STATE` が `GF_TRYING` の間の 1 回）、読むのは更新スレッドだけ（`GF_CAPTURED` を Acquire で見た後）。
const SLOT_PX: usize = (GAME_SLOT_W * GAME_SLOT_H) as usize;
const SLOT_META: usize = 4;
const SLOT_WORDS: usize = SLOT_PX + SLOT_META;
const SCRATCH_WORDS: usize = GAME_GLYPHS * SLOT_WORDS;
static SCRATCH: AtomicUsize = AtomicUsize::new(0);

fn ensure_scratch() {
    if SCRATCH.load(Ordering::Acquire) == 0 {
        let b: &'static mut [u32] = Box::leak(vec![0u32; SCRATCH_WORDS].into_boxed_slice());
        SCRATCH.store(b.as_mut_ptr() as usize, Ordering::Release);
    }
}

#[derive(Clone, Default)]
struct UpdaterCfg {
    preview: bool,
    /// 右下の fps の行を出さない（ゲーム内のメニューのためだけに開いたとき）。
    quiet: bool,
    /// 回ごとの名前のパスと exe のフォルダ（PNG を本ログの隣へ書く。ゲームのフォルダの中には書かない —— `RealRecord`）。
    run_path: Option<std::path::PathBuf>,
    exe_dir: Option<std::path::PathBuf>,
}

#[cfg(test)]
pub(crate) static PANIC_NEXT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 読み取りのスレッドが、文字の表を置く（`prepare` の前でも後でもよい。1 回だけ。2 回目は断る）。
pub fn place_atlas(spec: AtlasSpec, texels: Vec<u32>) -> Result<(), String> {
    if texels.len() != spec.texel_count() {
        return Err(format!(
            "文字の表の画素の数 {} がテクスチャ {}×{} と違う",
            texels.len(),
            spec.tex_w,
            spec.tex_h
        ));
    }
    if ATLAS.load(Ordering::SeqCst) != 0 {
        return Err("文字の表はもう置いてある".to_string());
    }
    let a: &'static Atlas = Box::leak(Box::new(Atlas { spec, texels }));
    ATLAS.store(a as *const Atlas as usize, Ordering::Release);
    // ゲームの文字をコピーする作業場も、ゲームのスレッドが要る前にここで用意する（ゲームのスレッドでは確保しない）。
    ensure_scratch();
    Ok(())
}

/// 読み取りのスレッドが、デバイスと present parameters のアドレスを置く（読めなければ断る。ゲームのスレッドが読めない
/// アドレスを読まないため）。描くのはまだ有効にしない（`set_on`）。
pub fn prepare(device_ptr_addr: u32, pp_addr: u32) -> Result<(), String> {
    if OVERLAY_ON.load(Ordering::SeqCst) != 0 {
        return Err("文字の描画がもう有効になっている".to_string());
    }
    if device_ptr_addr == 0 || read_own_memory(device_ptr_addr, 4).is_none() {
        return Err(format!(
            "デバイスの置き場 0x{device_ptr_addr:08X} が読めない"
        ));
    }
    if pp_addr == 0 || read_own_memory(pp_addr, (PP_HEIGHT + 4) as usize).is_none() {
        return Err(format!("present parameters 0x{pp_addr:08X} が読めない"));
    }
    if ATLAS.load(Ordering::Acquire) == 0 {
        return Err("文字の表がまだ置かれていない".to_string());
    }
    DEVICE_PTR_ADDR.store(device_ptr_addr, Ordering::SeqCst);
    PP_ADDR.store(pp_addr, Ordering::SeqCst);
    Ok(())
}

pub fn set_on() {
    OVERLAY_ON.store(1, Ordering::Release);
}

/// 描くのを止める（検査の後始末。本物ではゲームのスレッドが `stop` で止める）。
#[cfg(test)]
pub fn set_off() {
    OVERLAY_ON.store(0, Ordering::SeqCst);
}

pub fn counters() -> OverlayCounters {
    let l = |a: &AtomicU32| a.load(Ordering::Acquire);
    OverlayCounters {
        drawn: l(&DRAWN),
        lost: l(&LOST),
        no_device: l(&NO_DEVICE),
        torn: l(&TORN),
        draw_failed: l(&DRAW_FAILED),
        set_failed: l(&SET_FAILED),
        panics: l(&PANICS),
        fail: l(&FAIL),
        fail_hr: l(&FAIL_HR),
        setup_ticks: l(&SETUP_TICKS),
    }
}

fn stop(f: Fail, hr: u32) {
    OVERLAY_ON.store(0, Ordering::Release);
    if FAIL
        .compare_exchange(0, f as u32, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        FAIL_HR.store(hr, Ordering::Release);
    }
}

// ---- ゲームのスレッドの入口 ----

/// 描画の入口（`draw.rs`）が、`OVERLAY_ON` が 0 でないときに呼ぶ（全部のレジスタとフラグは入口が保つ）。中は全部
/// `catch_unwind` で、パニックを受けたら描くのを止めて数える。
pub extern "C" fn overlay_on_draw() {
    if catch_unwind(AssertUnwindSafe(overlay_inner)).is_err() {
        stop(Fail::Panic, 0);
        PANICS.fetch_add(1, Ordering::SeqCst);
    }
}

/// `addr` の 4 バイト。
///
/// # Safety
/// `addr` から 4 バイトが読めること（呼び出し側が理由を書く）。
unsafe fn read_u32(addr: u32) -> u32 {
    // SAFETY: 呼び出し側の約束。境界を問わない読み（`read_unaligned`）。
    unsafe { (addr as usize as *const u32).read_unaligned() }
}

fn overlay_inner() {
    if OVERLAY_ON.load(Ordering::Acquire) == 0 {
        return;
    }
    #[cfg(test)]
    if PANIC_NEXT.swap(false, Ordering::SeqCst) {
        panic!("検査: 文字の描画の中のパニック");
    }
    let t0 = qpc();
    let (dev_at, pp_at, atlas_at) = (
        DEVICE_PTR_ADDR.load(Ordering::Relaxed),
        PP_ADDR.load(Ordering::Relaxed),
        ATLAS.load(Ordering::Acquire),
    );
    if dev_at == 0 || pp_at == 0 || atlas_at == 0 {
        stop(Fail::NoAtlas, 0);
        return;
    }
    // SAFETY: どちらも `prepare` が読めると確かめてから置いたアドレス（花映塚では th09.exe の .data の `0x4B3108` と `0x4B3450`。プロセスが終わるまで読める）。
    let (device, screen_w, screen_h) = unsafe {
        (
            read_u32(dev_at),
            read_u32(pp_at + PP_WIDTH),
            read_u32(pp_at + PP_HEIGHT),
        )
    };
    if device == 0 {
        NO_DEVICE.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let d = Dev(device);
    // SAFETY: `device` はゲームが作った IDirect3DDevice8（`[0x4B3108]` が 0 でない。ゲームはこの入口の直後 `0x0042DD59` で同じアドレスの
    // EndScene を呼ぶので生きている）。描画のスレッド = ゲームのスレッド。
    let hr = unsafe { d.test_cooperative_level() };
    if hr != 0 {
        LOST.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: `atlas_at` は `place_atlas` が `Box::leak` で作って返さない `Atlas` のアドレス（Release で置き、上の Acquire で読んだので
    // 中身の書きは見えている）。以後だれも書かない。
    let atlas: &'static Atlas = unsafe { &*(atlas_at as *const Atlas) };
    let mut setup = false;
    let mut tex = match TEX.load(Ordering::Relaxed) {
        0 => match create_texture(d, atlas) {
            Ok(t) => {
                TEX.store(t, Ordering::Relaxed);
                TEX_DEVICE.store(device, Ordering::Relaxed);
                TEX_ATLAS.store(atlas_at, Ordering::Relaxed);
                setup = true;
                t
            }
            Err((f, hr)) => {
                stop(f, hr);
                return;
            }
        },
        t => {
            if TEX_DEVICE.load(Ordering::Relaxed) != device {
                stop(Fail::DeviceChanged, 0);
                return;
            }
            t
        }
    };
    // 更新スレッドがゲームの文字をコピーした文字の表に差し替えたら、テクスチャを作り直す（1 回だけ）。作り直せなければ、**描くのは止めずに**
    // 差し替え前のテクスチャと、それを作った文字の表（`TEX_ATLAS`）で描き続ける（組を崩さない。GDI の文字のまま見えるだけ）。
    // 失敗は数えて本ログに残し、間を置いて `SWAP_MAX_TRIES` 回まで試し直す（`swap_should_try`）。最初の 1 回（`TEX` が無い）だけは、
    // 描ける絵がまだ無いので、理由を残して描くのを止める。
    let mut draw_atlas = atlas;
    if TEX_ATLAS.load(Ordering::Relaxed) != atlas_at {
        let tries = SWAP_TRIES.load(Ordering::Relaxed);
        let since = if tries == 0 {
            0
        } else {
            SWAP_SINCE.load(Ordering::Relaxed).saturating_add(1)
        };
        SWAP_SINCE.store(since, Ordering::Relaxed);
        if swap_should_try(tries, since) {
            let t1 = qpc();
            match create_texture(d, atlas) {
                Ok(new_tex) => {
                    // SAFETY: 古い `TEX` は自分が作った参照を持つテクスチャで、描く間だけデバイスの段 0 に置き、描いた後にゲームの物へ
                    // 戻している（`draw_with_saved_state`）ので、いまデバイスは掴んでいない。
                    unsafe { Dev(tex).release() };
                    tex = new_tex;
                    TEX.store(new_tex, Ordering::Relaxed);
                    TEX_ATLAS.store(atlas_at, Ordering::Relaxed);
                    SWAP_TRIES.store(0, Ordering::Relaxed);
                    SWAP_TICKS.store(
                        u32::try_from(qpc().wrapping_sub(t1)).unwrap_or(u32::MAX),
                        Ordering::Relaxed,
                    );
                    setup = true;
                }
                Err((f, hr)) => {
                    SWAP_TRIES.store(tries.saturating_add(1), Ordering::Relaxed);
                    SWAP_SINCE.store(0, Ordering::Relaxed);
                    SWAP_FAILS.fetch_add(1, Ordering::Relaxed);
                    SWAP_FAIL_WHY.store(f as u32, Ordering::Relaxed);
                    SWAP_FAIL_HR.store(hr, Ordering::Relaxed);
                }
            }
        }
        if TEX_ATLAS.load(Ordering::Relaxed) != atlas_at {
            // 差し替えられていない: 今の `TEX` を作ったときの文字の表で頂点を組む。
            let old_at = TEX_ATLAS.load(Ordering::Relaxed);
            if old_at == 0 {
                stop(Fail::NoAtlas, 0);
                return;
            }
            // SAFETY: `old_at` は `TEX` を作ったときに `TEX_ATLAS` へ置いた `Atlas` のアドレス（返さない物。以後だれも書かない）。
            draw_atlas = unsafe { &*(old_at as *const Atlas) };
        }
    }
    // ゲームの文字のコピー: 自分のテクスチャができた後、まだコピーできていない間だけ、30 フレームに 1 回試す（`game_font_step`）。
    if GF_STATE.load(Ordering::Relaxed) == GF_TRYING && game_font_step(tex) {
        setup = true;
    }
    let mut units = [0u16; TEXT_UNITS];
    // 書きかけに当たったら、数回だけ読み直す（書き手は 250 ms に 1 回、数百 ns しか書かない）。読み直しても書きかけなら、
    // そのフレームは右下の行を描かない。ゲーム内のメニューの盤は別の描画なので描く。
    let len = (0..4).find_map(|_| TEXT.read(&mut units));
    if len.is_none() {
        TORN.fetch_add(1, Ordering::Relaxed);
    }
    let len = len.unwrap_or(0);
    // バッファの UTF-16 を、頂点の組み立てが取る `&str`（UTF-8）にする。スタックの配列に書くだけで確保しない。
    let mut utf8 = [0u8; ov::MAX_TEXT_UTF8];
    let n8 = ov::utf16_to_utf8(units.get(..len).unwrap_or(&[]), &mut utf8);
    let text = core::str::from_utf8(utf8.get(..n8).unwrap_or(&[])).unwrap_or("");
    let mut verts = [Vertex::default(); MAX_VERTS_ALL];
    let n = ov::layout_lines(
        text,
        &draw_atlas.spec,
        screen_w,
        screen_h,
        PLACE_BOTTOM_RIGHT,
        &STYLE_DEFAULT,
        &mut verts,
    );
    // ゲーム内のメニューの盤。見せるものが無ければ原子変数を 1 つ読むだけ。
    let board_on = BOARD.show() != 0;
    if n == 0 && !board_on {
        return;
    }
    // 状態の保存と戻しは 1 回にまとめ、右下の行（あれば）と盤の行を続けて描く。
    if let Err((f, hr)) = draw_with_saved_state(d, tex, screen_w, screen_h, &mut |emit| {
        if n > 0 {
            emit(&verts[..n]);
        }
        draw_board(&BOARD, &draw_atlas.spec, screen_w, screen_h, emit);
    }) {
        stop(f, hr);
        return;
    }
    let dt = u32::try_from(qpc().wrapping_sub(t0)).unwrap_or(u32::MAX);
    if setup {
        SETUP_TICKS.store(dt, Ordering::Relaxed);
    } else {
        TIMINGS.record(dt);
    }
    DRAWN.fetch_add(1, Ordering::Relaxed);
}

struct Saved {
    rs: [u32; RS.len()],
    tss: [u32; TSS.len()],
    vs: u32,
    vp: [u32; 6],
    /// 段 0 のテクスチャ（GetTexture が参照を 1 つ足して返す。0 なら無し）。
    tex: u32,
    /// 頂点の流れ 0 の頂点バッファと stride（GetStreamSource が参照を 1 つ足して返す。0 なら無し）。
    vb: u32,
    stride: u32,
}

/// 描く側の入口。状態を置いた後に 1 回だけ呼ばれ、渡された「頂点を出す関数」を何度でも呼んでよい（盤の行ごとの塊）。
type DrawFn<'a> = &'a mut dyn FnMut(&mut dyn FnMut(&[Vertex]));

/// 状態を読み、自分の状態を置き、描き、読んだ値へ戻す。Get* が 1 つでも失敗したら、何も置かずに（取った参照は
/// 返して）`Err`。Set* と DrawPrimitiveUP の失敗は数えるだけで、戻しは全部やる。
fn draw_with_saved_state(
    d: Dev,
    tex: u32,
    screen_w: u32,
    screen_h: u32,
    draw: DrawFn<'_>,
) -> Result<(), (Fail, u32)> {
    let mut s = Saved {
        rs: [0; RS.len()],
        tss: [0; TSS.len()],
        vs: 0,
        vp: [0; 6],
        tex: 0,
        vb: 0,
        stride: 0,
    };
    // SAFETY: `d` は `overlay_inner` が確かめた生きているデバイスで、ゲームのスレッドから呼んでいる（以下の `d.*` も同じ）。
    if let Err(hr) = unsafe { save(d, &mut s) } {
        // SAFETY: 同上。`s` の参照は `save` が取れたものだけ（取れなかった欄は 0）。
        unsafe { release_saved(&s) };
        return Err((Fail::GetState, hr as u32));
    }
    let vp = [0, 0, screen_w, screen_h, 0.0f32.to_bits(), 1.0f32.to_bits()];
    let mut set_fail = 0u32;
    let mut chk = |hr: i32| {
        if hr < 0 {
            set_fail += 1;
        }
    };
    // SAFETY: 同上。値は上の表の D3D8 の定数、テクスチャは自分が作った生きている物、viewport はスタックの 6 語。
    unsafe {
        for (i, &(st, v)) in RS.iter().enumerate() {
            if s.rs[i] != v {
                chk(d.set_render_state(st, v));
            }
        }
        for (i, &(stage, ty, v)) in TSS.iter().enumerate() {
            if s.tss[i] != v {
                chk(d.set_texture_stage_state(stage, ty, v));
            }
        }
        if s.tex != tex {
            chk(d.set_texture(0, tex));
        }
        if s.vs != FVF {
            chk(d.set_vertex_shader(FVF));
        }
        if s.vp != vp {
            chk(d.set_viewport(&vp));
        }
    }
    // 1 まとまりずつ描く（右下の行・メニューの背景・メニューの 1 行ずつ・左上の行）。
    let mut emit = |verts: &[Vertex]| {
        if verts.is_empty() {
            return;
        }
        // SAFETY: 同上。`verts` はスタックの `Vertex`（28 バイトの repr(C)）の並びで、数は 6 の倍数（`layout_lines` / `net_ui::draw_board` の約束）。D3D8 は呼び出しの中で読み終える。
        let hr = unsafe {
            d.draw_primitive_up(
                D3DPT_TRIANGLELIST,
                (verts.len() / 3) as u32,
                verts.as_ptr() as *const c_void,
                VERTEX_SIZE,
            )
        };
        if hr < 0 {
            DRAW_FAILED.fetch_add(1, Ordering::Relaxed);
        }
    };
    draw(&mut emit);
    // 戻す（置いたのと逆の順）。DrawPrimitiveUP は頂点の流れ 0 を NULL にするので、ゲームが頂点バッファを置いていたら置き直す
    // （ゲームは頂点の形の覚え `[AnmManager+0x12886]` が同じなら SetStreamSource を飛ばす —— `0x0043B449`〜`0x0043B478`）。
    // SAFETY: 同上。戻す値は `save` が読んだ値そのもの。テクスチャと頂点バッファは `save` が参照を持っているので生きている。
    unsafe {
        if s.vp != vp {
            chk(d.set_viewport(&s.vp));
        }
        if s.vs != FVF {
            chk(d.set_vertex_shader(s.vs));
        }
        if s.tex != tex {
            chk(d.set_texture(0, s.tex));
        }
        for (i, &(stage, ty, v)) in TSS.iter().enumerate().rev() {
            if s.tss[i] != v {
                chk(d.set_texture_stage_state(stage, ty, s.tss[i]));
            }
        }
        for (i, &(st, v)) in RS.iter().enumerate().rev() {
            if s.rs[i] != v {
                chk(d.set_render_state(st, s.rs[i]));
            }
        }
        if s.vb != 0 {
            chk(d.set_stream_source(0, s.vb, s.stride));
        }
        release_saved(&s);
    }
    if set_fail != 0 {
        SET_FAILED.fetch_add(set_fail, Ordering::Relaxed);
    }
    Ok(())
}

/// 状態を読む。参照を返すもの（テクスチャ・頂点バッファ）は最後に読む（その前で失敗しても返す物が無い）。
///
/// # Safety
/// `d` は生きているデバイスで、ゲームのスレッドから呼ぶこと。
unsafe fn save(d: Dev, s: &mut Saved) -> Result<(), i32> {
    let ok = |hr: i32| if hr < 0 { Err(hr) } else { Ok(()) };
    // SAFETY: 呼び出し側の約束。書く先はどれも `s` の欄（書ける u32）。
    unsafe {
        for (i, &(st, _)) in RS.iter().enumerate() {
            ok(d.get_render_state(st, &mut s.rs[i]))?;
        }
        for (i, &(stage, ty, _)) in TSS.iter().enumerate() {
            ok(d.get_texture_stage_state(stage, ty, &mut s.tss[i]))?;
        }
        ok(d.get_vertex_shader(&mut s.vs))?;
        ok(d.get_viewport(&mut s.vp))?;
        ok(d.get_texture(0, &mut s.tex))?;
        ok(d.get_stream_source(0, &mut s.vb, &mut s.stride))?;
    }
    Ok(())
}

/// `save` が取った参照を返す。
///
/// # Safety
/// `s.tex` と `s.vb` は 0 か、`save` が参照を 1 つ足して受け取った COM の物であること。
unsafe fn release_saved(s: &Saved) {
    for obj in [s.tex, s.vb] {
        if obj != 0 {
            // SAFETY: 呼び出し側の約束（GetTexture / GetStreamSource が足した参照を 1 回だけ返す）。
            unsafe { Dev(obj).release() };
        }
    }
}

/// 自分のテクスチャを作り、文字の表の画素をコピーする（最初の 1 回だけ）。
fn create_texture(d: Dev, atlas: &Atlas) -> Result<u32, (Fail, u32)> {
    let (w, h) = (atlas.spec.tex_w, atlas.spec.tex_h);
    let mut tex = 0u32;
    // SAFETY: `d` は生きているデバイス。引数は D3D8 の CreateTexture の 7 つ（幅・高さ・段 1・使い方 0・A8R8G8B8・MANAGED・書ける `tex`）。
    let hr = unsafe { d.create_texture(w, h, 1, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED, &mut tex) };
    if hr < 0 || tex == 0 {
        return Err((Fail::CreateTexture, hr as u32));
    }
    let t = Dev(tex);
    let mut locked = [0u32; 2];
    // SAFETY: `tex` は今作った IDirect3DTexture8。段 0 を全面・フラグ 0 で開く。書く先はスタックの 2 語（D3DLOCKED_RECT の Pitch・pBits）。
    let hr = unsafe { t.tex_lock_rect(0, 0, &mut locked) };
    if hr < 0 {
        // SAFETY: 作ったときの参照を 1 回だけ返す。
        unsafe { t.release() };
        return Err((Fail::LockTexture, hr as u32));
    }
    let (pitch, bits) = (locked[0] as usize, locked[1] as usize);
    let row = w as usize * 4;
    let ok = bits != 0 && pitch >= row;
    if ok {
        for y in 0..h as usize {
            // SAFETY: 元は `place_atlas` が長さを `tex_w × tex_h` と確かめた画素の行 y（`row` バイト）。先は LockRect が返した段 0 の
            // 行 y（`bits + y * pitch` から `row` バイト。pitch >= row を上で確かめた。高さは作った h）。2 つは別の領域。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (atlas.texels.as_ptr() as *const u8).add(y * row),
                    bits.wrapping_add(y * pitch) as *mut u8,
                    row,
                );
            }
        }
    }
    // SAFETY: 上の LockRect が通ったので対にして 1 回だけ閉じる。
    unsafe { t.tex_unlock_rect(0) };
    if !ok {
        // SAFETY: 作ったときの参照を 1 回だけ返す。
        unsafe { t.release() };
        return Err((Fail::LockTexture, 0));
    }
    Ok(tex)
}

fn gf_why(w: GameFontWhy) {
    GF_WHY.store(w as u32, Ordering::Relaxed);
}

fn gf_refuse(w: GameFontWhy) {
    gf_why(w);
    GF_STATE.store(GF_REFUSED, Ordering::Release);
}

/// ゲームの文字をコピーする試し（ゲームのスレッド。描画の入口の中）。30 フレームに 1 回だけ、`probe_game_font` で ascii.anm が読み込み済みと
/// 確かめ、通ったらゲームのテクスチャを READONLY で開いて英数字の絵を作業場へ ARGB でコピーする。**確保しない・待たない・ゲームのメモリへ
/// 書かない**。コピーする仕事（LockRect を含む）をしたら true（その回は描画の上乗せの記録から外す）。
fn game_font_step(own_tex: u32) -> bool {
    match gf_gate(
        GF_FRAMES.fetch_add(1, Ordering::Relaxed),
        GF_ATTEMPTS.load(Ordering::Relaxed),
    ) {
        GfGate::Skip => return false,
        GfGate::GiveUp => {
            // 待つ理由が直らないまま試しの回数の上限を越えた: 最後の理由を残して、もう試さない（GDI の文字のまま描く）。
            GF_LAST_WAIT.store(GF_WHY.load(Ordering::Relaxed), Ordering::Relaxed);
            gf_refuse(GameFontWhy::GaveUp);
            return false;
        }
        GfGate::Try => {}
    }
    GF_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    let scratch = SCRATCH.load(Ordering::Acquire);
    if scratch == 0 || own_tex == 0 {
        gf_why(GameFontWhy::NoOwnTexture);
        return false;
    }
    let mut read = |a: u32, b: &mut [u8]| read_own_into(a, b);
    let probed = match probe_game_font(
        &mut read,
        GameAddrs {
            loaded_ptr: GF_LOADED_PTR.load(Ordering::Relaxed),
            anm_manager_ptr: GF_MGR_PTR.load(Ordering::Relaxed),
        },
    ) {
        Ok(p) => p,
        Err(w) => {
            gf_why(w);
            return false;
        }
    };
    // ゲームのテクスチャの先頭（vtable へのアドレス）が自分のテクスチャと同じであること（ポインタが本当にテクスチャかを、COM の関数を呼ぶ前に確かめる）。
    let (mut theirs, mut ours) = ([0u8; 4], [0u8; 4]);
    if !(read_own_into(probed.texture, &mut theirs)
        && read_own_into(own_tex, &mut ours)
        && theirs == ours
        && ours != [0; 4])
    {
        gf_why(GameFontWhy::VtableMismatch);
        return false;
    }
    let t0 = qpc();
    let t = Dev(probed.texture);
    let mut desc = [0u32; 8];
    // SAFETY: `t` は今確かめた IDirect3DTexture8（vtable が自分のテクスチャと同じで、ゲームが作って持っている）。描画のスレッド = ゲームのスレッド。
    // `desc` は書ける 8 語。
    let hr = unsafe { t.tex_get_level_desc(0, &mut desc) };
    if hr < 0 {
        gf_refuse(GameFontWhy::LevelDescFailed);
        return true;
    }
    let (fmt, w, h) = (
        desc[SURFACE_DESC_FORMAT],
        desc[SURFACE_DESC_WIDTH],
        desc[SURFACE_DESC_HEIGHT],
    );
    let Some(bpp) = ov::texture_pixel_bytes(fmt) else {
        GF_INFO[0].store(fmt, Ordering::Relaxed);
        gf_refuse(GameFontWhy::FormatUnsupported);
        return true;
    };
    if (w, h) != (probed.tex_w, probed.tex_h) {
        GF_INFO[1].store(w, Ordering::Relaxed);
        GF_INFO[2].store(h, Ordering::Relaxed);
        gf_refuse(GameFontWhy::DimsMismatch);
        return true;
    }
    if probed
        .sprites
        .iter()
        .flatten()
        .any(|s| s.rect.w > GAME_SLOT_W || s.rect.h > GAME_SLOT_H)
    {
        gf_refuse(GameFontWhy::GlyphTooBig);
        return true;
    }
    let mut locked = [0u32; 2];
    // SAFETY: 同上。段 0 を全面（矩形 NULL）・READONLY で開く（ゲームのテクスチャは読むだけ）。書く先はスタックの 2 語。
    let hr = unsafe { t.tex_lock_rect(0, D3DLOCK_READONLY, &mut locked) };
    if hr < 0 || locked[1] == 0 {
        if gf_lock_gives_up(GF_LOCK_FAILS.fetch_add(1, Ordering::Relaxed) + 1) {
            gf_refuse(GameFontWhy::LockFailed);
        } else {
            gf_why(GameFontWhy::LockFailed);
        }
        if hr >= 0 {
            // SAFETY: 上の LockRect が通ったので、対にして 1 回だけ閉じる。
            unsafe { t.tex_unlock_rect(0) };
        }
        return true;
    }
    let (pitch, bits) = (locked[0] as usize, locked[1] as usize);
    // 最後の行は幅ぶんだけ（次の行の分は D3D の面の外かもしれない）。
    let row = w as usize * bpp;
    let len = (h as usize)
        .saturating_sub(1)
        .checked_mul(pitch)
        .and_then(|v| v.checked_add(row))
        .filter(|_| pitch >= row && h > 0);
    let mut ok = false;
    let mut count = 0u32;
    if let Some(len) = len {
        // SAFETY: LockRect が返した段 0 の面（pitch × 高さ h。最後の行は幅ぶん）。閉じるまで動かない・読むだけ。
        let data = unsafe { core::slice::from_raw_parts(bits as *const u8, len) };
        ok = true;
        for (i, sp) in probed.sprites.iter().enumerate() {
            let Some(sp) = sp else { continue };
            // SAFETY: 作業場は `ensure_scratch` の `SCRATCH_WORDS` 語（i < GAME_GLYPHS なので文字 i の `SLOT_WORDS` 語は範囲内）。
            // 書くのはこの関数（ゲームのスレッド）だけで、更新スレッドは `GF_CAPTURED` を Acquire で見るまで読まない。
            let slot = unsafe {
                core::slice::from_raw_parts_mut(
                    (scratch as *mut u32).add(i * SLOT_WORDS),
                    SLOT_WORDS,
                )
            };
            let (pix, meta) = slot.split_at_mut(SLOT_PX);
            if ov::extract_rect_argb(fmt, w, h, pitch, data, sp.rect, pix, GAME_SLOT_W as usize)
                .is_err()
            {
                ok = false;
                break;
            }
            meta.copy_from_slice(&[sp.rect.w, sp.rect.h, sp.quad_w, sp.quad_h]);
            count += 1;
        }
    }
    // SAFETY: 上の LockRect が通ったので、対にして 1 回だけ閉じる（`data` はここまで）。
    unsafe { t.tex_unlock_rect(0) };
    if !ok {
        gf_refuse(GameFontWhy::ExtractFailed);
        return true;
    }
    let at = ov::game_index('@').unwrap_or(0);
    let (a_w, a_h, a_qw, a_qh) = probed
        .sprites
        .get(at)
        .copied()
        .flatten()
        .map_or((0, 0, 0, 0), |s| (s.rect.w, s.rect.h, s.quad_w, s.quad_h));
    for (slot, v) in GF_INFO.iter().zip([fmt, w, h, count, a_w, a_h, a_qw, a_qh]) {
        slot.store(v, Ordering::Relaxed);
    }
    GF_TICKS.store(
        u32::try_from(qpc().wrapping_sub(t0)).unwrap_or(u32::MAX),
        Ordering::Relaxed,
    );
    gf_why(GameFontWhy::None);
    GF_STATE.store(GF_CAPTURED, Ordering::Release);
    true
}

// ---- D3D8 の呼び出し（COM の vtable）----

#[derive(Debug, Clone, Copy)]
struct Dev(u32);

impl Dev {
    /// vtable の `slot` バイト目の関数のアドレス。
    ///
    /// # Safety
    /// `self` が生きている COM の物（先頭の 4 バイトが vtable へのアドレス）で、`slot` がその vtable の中であること。
    unsafe fn f(self, slot: u32) -> usize {
        // SAFETY: 呼び出し側の約束。
        unsafe { read_u32(read_u32(self.0).wrapping_add(slot)) as usize }
    }

    // 以下、どれも `# Safety`: `self` は生きているデバイス（テクスチャの関数はテクスチャ）で、ゲームのスレッドから呼ぶこと。ポインタの
    // 引数は書ける / 読める所を指すこと。関数の形は D3D8 の stdcall（`extern "system"`、第 1 引数が this。32 ビットなのでアドレスは u32）。

    unsafe fn test_cooperative_level(self) -> i32 {
        type F = unsafe extern "system" fn(u32) -> i32;
        // SAFETY: 上の注。`0x0C` は TestCooperativeLevel（引数なし）。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_TESTCOOPERATIVELEVEL))(self.0) }
    }

    #[allow(clippy::too_many_arguments)] // D3D8 の CreateTexture の引数そのまま
    unsafe fn create_texture(
        self,
        w: u32,
        h: u32,
        levels: u32,
        usage: u32,
        format: u32,
        pool: u32,
        out: &mut u32,
    ) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32, u32, u32, u32, u32, *mut u32) -> i32;
        // SAFETY: 上の注。`out` は書ける 4 バイト。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_CREATETEXTURE))(
                self.0, w, h, levels, usage, format, pool, out,
            )
        }
    }

    unsafe fn get_render_state(self, state: u32, out: &mut u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, *mut u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_GETRENDERSTATE))(self.0, state, out) }
    }

    unsafe fn set_render_state(self, state: u32, v: u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_SETRENDERSTATE))(self.0, state, v) }
    }

    unsafe fn get_texture_stage_state(self, stage: u32, ty: u32, out: &mut u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32, *mut u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_GETTEXTURESTAGESTATE))(
                self.0, stage, ty, out,
            )
        }
    }

    unsafe fn set_texture_stage_state(self, stage: u32, ty: u32, v: u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32, u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_SETTEXTURESTAGESTATE))(self.0, stage, ty, v)
        }
    }

    /// `out` には参照が 1 つ足された物（か 0）が入る。
    unsafe fn get_texture(self, stage: u32, out: &mut u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, *mut u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_GETTEXTURE))(self.0, stage, out) }
    }

    /// `tex` は 0 か生きているテクスチャ。
    unsafe fn set_texture(self, stage: u32, tex: u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_SETTEXTURE))(self.0, stage, tex) }
    }

    unsafe fn get_vertex_shader(self, out: &mut u32) -> i32 {
        type F = unsafe extern "system" fn(u32, *mut u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_GETVERTEXSHADER))(self.0, out) }
    }

    unsafe fn set_vertex_shader(self, handle: u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_SETVERTEXSHADER))(self.0, handle) }
    }

    /// `vp` は D3DVIEWPORT8（X・Y・幅・高さ・MinZ・MaxZ の 6 語）。
    unsafe fn get_viewport(self, vp: &mut [u32; 6]) -> i32 {
        type F = unsafe extern "system" fn(u32, *mut u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_GETVIEWPORT))(self.0, vp.as_mut_ptr())
        }
    }

    unsafe fn set_viewport(self, vp: &[u32; 6]) -> i32 {
        type F = unsafe extern "system" fn(u32, *const u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(DEV_SETVIEWPORT))(self.0, vp.as_ptr()) }
    }

    /// `vb` には参照が 1 つ足された物（か 0）が入る。
    unsafe fn get_stream_source(self, n: u32, vb: &mut u32, stride: &mut u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, *mut u32, *mut u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_GETSTREAMSOURCE))(self.0, n, vb, stride)
        }
    }

    /// `vb` は 0 か生きている頂点バッファ。
    unsafe fn set_stream_source(self, n: u32, vb: u32, stride: u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32, u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_SETSTREAMSOURCE))(self.0, n, vb, stride)
        }
    }

    /// `data` から `count` 個の三角形ぶんの頂点（stride バイトずつ）が読めること。
    unsafe fn draw_primitive_up(
        self,
        ty: u32,
        count: u32,
        data: *const c_void,
        stride: u32,
    ) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, u32, *const c_void, u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(DEV_DRAWPRIMITIVEUP))(
                self.0, ty, count, data, stride,
            )
        }
    }

    /// テクスチャの LockRect（全面。`flags` は 自分のテクスチャは 0、ゲームのテクスチャは `D3DLOCK_READONLY`）。
    ///
    /// # Safety
    /// `self` は生きているテクスチャ。`locked` は D3DLOCKED_RECT（Pitch・pBits の 2 語）。
    unsafe fn tex_lock_rect(self, level: u32, flags: u32, locked: &mut [u32; 2]) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, *mut u32, *const c_void, u32) -> i32;
        // SAFETY: 上の注。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(TEX_LOCKRECT))(
                self.0,
                level,
                locked.as_mut_ptr(),
                core::ptr::null(),
                flags,
            )
        }
    }

    /// テクスチャの GetLevelDesc（`desc` は D3DSURFACE_DESC の 8 語）。
    ///
    /// # Safety
    /// `self` は生きているテクスチャ。`desc` は書ける 8 語（32 バイト）。
    unsafe fn tex_get_level_desc(self, level: u32, desc: &mut [u32; 8]) -> i32 {
        type F = unsafe extern "system" fn(u32, u32, *mut u32) -> i32;
        // SAFETY: 上の注。`0x38` は GetLevelDesc（段 · 書き先の 2 引数。ゲーム自身が `0x0043C8A6` で同じ形で呼ぶ）。
        unsafe {
            core::mem::transmute::<usize, F>(self.f(TEX_GETLEVELDESC))(
                self.0,
                level,
                desc.as_mut_ptr(),
            )
        }
    }

    /// # Safety
    /// `self` は生きているテクスチャで、`level` を LockRect で開いていること。
    unsafe fn tex_unlock_rect(self, level: u32) -> i32 {
        type F = unsafe extern "system" fn(u32, u32) -> i32;
        // SAFETY: 上の注。
        unsafe { core::mem::transmute::<usize, F>(self.f(TEX_UNLOCKRECT))(self.0, level) }
    }

    /// # Safety
    /// `self` は生きている COM の物で、呼ぶ側がその参照を 1 つ持っていること（1 回だけ返す）。
    unsafe fn release(self) -> u32 {
        type F = unsafe extern "system" fn(u32) -> u32;
        // SAFETY: 上の注。`0x08` は IUnknown の Release。
        unsafe { core::mem::transmute::<usize, F>(self.f(COM_RELEASE))(self.0) }
    }
}

// ---- 読み取りのスレッド: GDI で文字の表を描く ----

struct Gdi {
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
    old_font: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
    dib: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    old_dib: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
}

impl Drop for Gdi {
    fn drop(&mut self) {
        use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, SelectObject};
        // SAFETY: どれも、この構造体が作った（か選び出した）GDI の物で、0 でなければ 1 回だけ戻し・消す。選んでいた物を戻してから消す。
        unsafe {
            if !self.old_dib.is_null() {
                SelectObject(self.dc, self.old_dib);
            }
            if !self.old_font.is_null() {
                SelectObject(self.dc, self.old_font);
            }
            if !self.dib.is_null() {
                DeleteObject(self.dib);
            }
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

/// 字体の別名（GDI が返す名前は環境で違うことがある）。同じ組の名前は同じ字体として扱う。
const FACE_ALIASES: [&[&str]; 1] = [&["ＭＳ ゴシック", "MS Gothic"]];

/// 頼んだ字体 `wanted` と、GDI が実際に選んだ字体 `actual` が同じか（大文字小文字は問わない。`FACE_ALIASES` の同じ組の名前も同じ）。
/// 頼んだ字体が無いと GDI は黙って別の字体を選ぶので、`GetTextFaceW` の名前で確かめる。
fn same_face(wanted: &str, actual: &str) -> bool {
    let eq = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    eq(wanted, actual)
        || FACE_ALIASES
            .iter()
            .any(|g| g.iter().any(|n| eq(n, wanted)) && g.iter().any(|n| eq(n, actual)))
}

fn selected_face(dc: windows_sys::Win32::Graphics::Gdi::HDC) -> String {
    use windows_sys::Win32::Graphics::Gdi::GetTextFaceW;
    let mut buf = [0u16; 64];
    // SAFETY: `dc` は呼び出し側が持つ生きている DC、`buf` は書ける 64 語（LF_FACESIZE は 32。長い名前は切られる）。
    let n = unsafe { GetTextFaceW(dc, buf.len() as i32, buf.as_mut_ptr()) };
    // 戻りは終わりの 0 を含む文字数（失敗は 0）。
    let n = (n.max(1) as usize - 1).min(buf.len() - 1);
    String::from_utf16_lossy(&buf[..n])
}

pub struct GdiAtlas {
    pub spec: AtlasSpec,
    pub texels: Vec<u32>,
    pub face: String,
    /// 頼んだ字体のどれかと同じか（false なら、無かったので GDI が代わりの字体を選んだ）。
    pub matched: bool,
    /// 枠・倍率・テクスチャの大きさの説明（字体は含まない）。
    pub desc: String,
}

impl GdiAtlas {
    /// 本ログに出す 1 文: 実際に選ばれた字体（頼んだ字体か、無くて代わりに選ばれたか）と、`desc`。
    pub fn describe(&self) -> String {
        let face = if self.face.is_empty() {
            "（名前を読めず）"
        } else {
            &self.face
        };
        let how = if self.matched {
            "（頼んだ字体）"
        } else {
            "（頼んだ字体が無く、GDI が代わりに選んだ）"
        };
        format!("字体 {face}{how}、{}", self.desc)
    }
}

/// 字体を `faces` の順に試し（`CreateFontW`・`SHIFTJIS_CHARSET`。`GetTextFaceW` で実際に選ばれた名前が頼んだ名前と同じものを採る）、
/// 文字の表（`ov::ui_glyphs`）を GDI でメモリの面に描いて、割り付けとテクスチャの画素を返す（読み取りのスレッド）。半角 1 枠は
/// `ceil(height / 2)` × `height` 画素、全角はその 2 枠ぶん。`scale` が 2 以上なら、その倍の大きさで描いて縮める（ゲームは文字を
/// 2 倍で描いて縮めている —— `CreateFontA(2h, …)`、`0x004364C0`。縮めるのは `ov::downscale_coverage`）。文字は 1 つずつ自分の枠に
/// 切り取って（`ETO_CLIPPED`）描くので、枠より広い文字を持つ字体でも隣の枠を汚さない。頼んだ字体がどれも無いときは、GDI が選んだ
/// 代わりの字体で作る（`matched` = false）。字体の順は呼び出し側が決める（thprac の日本語の字体の順を参考にした）。
#[cfg(test)] // 本物は `gdi_atlas_with` を呼ぶ。ゴシックの字体の検査は予約なしの表で見る
pub fn gdi_atlas(faces: &[&str], height: i32, scale: u32) -> Result<GdiAtlas, String> {
    gdi_atlas_with(faces, height, scale, false)
}

/// 実物の実装が使う文字の表: 既定の字体・高さ・倍率（ＭＳ ゴシック、高さ 14 を 2 倍で描いて縮める）で、ゲームの文字の枠の塊を予約したもの。
/// 既定の値を 1 か所から引くための入口（検査が既定を縛る）。
pub fn default_atlas() -> Result<GdiAtlas, String> {
    gdi_atlas_with(&FONT_FACES, FONT_HEIGHT, FONT_SCALE, true)
}

/// `gdi_atlas` に、ゲームの文字の枠の塊を予約するかどうかを選べる形（`reserve_game` = true なら `AtlasSpec::fit_game`。
/// 塊の中は透明のまま —— GDI は文字の枠にしか描かない。絵をコピーする前・コピーできない文字は、文字の枠の GDI の文字で描く）。
pub fn gdi_atlas_with(
    faces: &[&str],
    height: i32,
    scale: u32,
    reserve_game: bool,
) -> Result<GdiAtlas, String> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_PITCH, DIB_RGB_COLORS,
        DeleteObject, ETO_CLIPPED, ExtTextOutW, FF_DONTCARE, FW_NORMAL, GdiFlush, GetTextMetricsW,
        OUT_DEFAULT_PRECIS, SHIFTJIS_CHARSET, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW,
        TRANSPARENT,
    };
    if height <= 0 || !(1..=4).contains(&scale) || faces.is_empty() {
        return Err(format!(
            "文字の表の指定が違う（高さ {height}・倍率 {scale}・字体 {} 個）",
            faces.len()
        ));
    }
    let cell_h = height as u32;
    let cell_w = cell_h.div_ceil(2);
    let glyphs = ov::ui_glyphs();
    let spec = if reserve_game {
        AtlasSpec::fit_game(cell_w, cell_h, &glyphs)?
    } else {
        AtlasSpec::fit(cell_w, cell_h, &glyphs)?
    };
    let (w, h) = (spec.tex_w * scale, spec.tex_h * scale);
    let mut g = Gdi {
        dc: core::ptr::null_mut(),
        font: core::ptr::null_mut(),
        old_font: core::ptr::null_mut(),
        dib: core::ptr::null_mut(),
        old_dib: core::ptr::null_mut(),
    };
    // SAFETY: NULL は「画面と同じ形のメモリの DC」を作る決まり。
    g.dc = unsafe { CreateCompatibleDC(core::ptr::null_mut()) };
    if g.dc.is_null() {
        return Err("CreateCompatibleDC が失敗した".to_string());
    }
    let (mut face, mut matched) = (String::new(), false);
    for want in faces {
        let want_w: Vec<u16> = want.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `want_w` は 0 で終わる UTF-16。ほかは値だけの引数。ゲームの `CreateFontA` と同じく SHIFTJIS_CHARSET・
        // ANTIALIASED_QUALITY。高さは負（= 文字の高さ em で指定）: 正（行の高さ）だと、上下の余白が大きい Yu Gothic UI・Meiryo UI は em が
        // 10 画素ほどに縮む。ＭＳ ゴシックは余白が 0 なので、どちらの指定でも同じ文字になる（実測: 高さ 14 と -14 で tmHeight 14）。
        let font = unsafe {
            CreateFontW(
                -(height * scale as i32),
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                SHIFTJIS_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                want_w.as_ptr(),
            )
        };
        if font.is_null() {
            continue;
        }
        // SAFETY: `g.dc` と `font` は上で作った物。前の候補は選び直した後で消す（選ばれたままの物は消せない）。最初に選び出された物は
        // `Gdi` の Drop が戻す。
        unsafe {
            let prev = SelectObject(g.dc, font);
            if g.old_font.is_null() {
                g.old_font = prev;
            } else if !g.font.is_null() {
                DeleteObject(g.font);
            }
        }
        g.font = font;
        face = selected_face(g.dc);
        if same_face(want, &face) {
            matched = true;
            break;
        }
    }
    if g.font.is_null() {
        return Err("CreateFontW が全部の候補で失敗した".to_string());
    }
    // 基準線をＭＳ ゴシックに揃える: ＭＳ ゴシックは文字の高さ（em）の 6/7 のところが基準線（高さ 14 で ascent 12）。全角の文字の墨は
    // 基準線の上に約 0.88 em 乗るので、字体の ascent がそれより大きい字体（Yu Gothic UI・Meiryo UI は em 14 で ascent 15）は、その差だけ
    // 上へ描き出して墨を枠の中に収める（実測: 差 3 画素で墨の y が 3..15 → 0..12 になり、ＭＳ ゴシックと同じ位置に揃う）。
    let em_px = (cell_h * scale) as i32;
    // SAFETY: `TEXTMETRICW` は数だけの構造体で、0 で埋めた値は正しい値の 1 つ。
    let mut tm: TEXTMETRICW = unsafe { core::mem::zeroed() };
    // SAFETY: `g.dc` は生きている DC、`tm` は書ける構造体。
    let raise = if unsafe { GetTextMetricsW(g.dc, &mut tm) } != 0 {
        (tm.tmAscent - (em_px * 6 + 3) / 7).max(0)
    } else {
        0
    };
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            // 負の高さ = 上の行から並ぶ面。
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = core::ptr::null_mut();
    // SAFETY: `bmi` は読める構造体、`bits` は書けるアドレス。区画は NULL（GDI が確保する）。
    g.dib = unsafe {
        CreateDIBSection(
            g.dc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            core::ptr::null_mut(),
            0,
        )
    };
    if g.dib.is_null() || bits.is_null() {
        return Err(format!("CreateDIBSection（{w}×{h}）が失敗した"));
    }
    let len = w as usize * h as usize * 4;
    // SAFETY: `bits` は CreateDIBSection が返した `w × h × 4` バイトの面（32 ビット・詰め物なし）。黒で埋める。
    unsafe { core::ptr::write_bytes(bits as *mut u8, 0, len) };
    // SAFETY: `g.dc` と `g.dib` は上で作った物。戻す物は `Gdi` の Drop が戻す。以下の GDI の呼び出しは値か、スタックの UTF-16 の 1〜2 語と
    // 切り取りの矩形を渡すだけ。小さい字体の段は、0 で終わる UTF-16 の字体名（`face_w`）で作った字体を選び、描いたあと元の字体を選び直してから消す（`g.font` は Drop が消す。`TEXTMETRICW` は 0 で埋めた値が正しい値の 1 つ）。
    unsafe {
        g.old_dib = SelectObject(g.dc, g.dib);
        SetTextColor(g.dc, 0x00FF_FFFF);
        SetBkMode(g.dc, TRANSPARENT as i32);
        for (slot, &c) in spec.glyphs.iter().enumerate() {
            // 小さい文字の鍵（`ov::small_key`）の枠は次の段（下）で小さい字体で描く。
            if c == ' ' || ov::small_ascii(c).is_some() {
                continue;
            }
            let slot = slot as u32;
            let (x, y) = spec.cell_origin(slot);
            let (x, y) = ((x * scale) as i32, (y * scale) as i32);
            let rect = RECT {
                left: x,
                top: y,
                right: x + (spec.slot_cells(slot) * spec.cell_w * scale) as i32,
                bottom: y + (spec.cell_h * scale) as i32,
            };
            let mut u = [0u16; 2];
            let s = c.encode_utf16(&mut u);
            ExtTextOutW(
                g.dc,
                x,
                y - raise,
                ETO_CLIPPED,
                &rect,
                s.as_ptr(),
                s.len() as u32,
                core::ptr::null(),
            );
        }
        // 小さい文字（観戦者の左下の行。`ov::SMALL_CELL_H`）: 同じ字体を小さい高さで作り直し、鍵の枠の左上の `SMALL_CELL_W` × `SMALL_CELL_H` だけに
        // 描く（残りは透明のまま。描く側がこの大きさだけを貼る —— `ov::resolve_small`）。作れなければ小さい文字の枠は透明のままで、
        // 観戦者の行の文字が出ない（ほかは変わらない）。
        let face_w: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
        let small_em = (ov::SMALL_CELL_H * scale) as i32;
        let small = CreateFontW(
            -small_em,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            SHIFTJIS_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            face_w.as_ptr(),
        );
        if !small.is_null() {
            let main_font = SelectObject(g.dc, small);
            let mut tm_small: TEXTMETRICW = core::mem::zeroed();
            // 基準線の揃え方は上の大きい文字と同じ（ＭＳ ゴシックの 6/7）。
            let raise_small = if GetTextMetricsW(g.dc, &mut tm_small) != 0 {
                (tm_small.tmAscent - (small_em * 6 + 3) / 7).max(0)
            } else {
                0
            };
            for (slot, &key) in spec.glyphs.iter().enumerate() {
                let Some(c) = ov::small_ascii(key) else {
                    continue;
                };
                let (x, y) = spec.cell_origin(slot as u32);
                let (x, y) = ((x * scale) as i32, (y * scale) as i32);
                let rect = RECT {
                    left: x,
                    top: y,
                    right: x + (ov::SMALL_CELL_W * scale) as i32,
                    bottom: y + small_em,
                };
                let mut u = [0u16; 2];
                let s = c.encode_utf16(&mut u);
                ExtTextOutW(
                    g.dc,
                    x,
                    y - raise_small,
                    ETO_CLIPPED,
                    &rect,
                    s.as_ptr(),
                    s.len() as u32,
                    core::ptr::null(),
                );
            }
            // 選び直してから小さい字体を消す（選ばれたままの物は消せない）。
            SelectObject(g.dc, main_font);
            DeleteObject(small);
        }
        GdiFlush();
    }
    // SAFETY: 同じ面の `len` バイト。GdiFlush で描き終えている。この後 `g` の Drop で消すので、先にコピーする。
    let bytes = unsafe { core::slice::from_raw_parts(bits as *const u8, len) }.to_vec();
    drop(g);
    let mut cov = ov::coverage_from_bgrx(&bytes, w, h)?;
    if scale > 1 {
        cov = ov::downscale_coverage(&cov, w, h, scale)?;
    }
    ov::center_narrow_wide_glyphs(&spec, &mut cov)?;
    let texels = ov::texels(&spec, &cov)?;
    let desc = format!(
        "文字の高さ {height} を {scale} 倍で描いて縮める（基準線を {raise} 画素上げた）→ 半角の枠 {}×{}（全角 {}×{}）、文字 {} 個、テクスチャ {}×{}",
        spec.cell_w,
        spec.cell_h,
        spec.cell_w * 2,
        spec.cell_h,
        spec.glyphs.len(),
        spec.tex_w,
        spec.tex_h,
    );
    Ok(GdiAtlas {
        spec,
        texels,
        face,
        matched,
        desc,
    })
}

// ---- 読み取りのスレッド: 文の更新スレッド ----

/// 文の更新スレッドを起こす（1 本だけ）。250 ms ごとに描画の回数から fps を出し、文を入れ物に書く。パニックしたら止まる（描画は最後に書いた文の
/// まま続く）。ゲームの文字をコピーした後（`GF_CAPTURED`）は、ここで文字の表を作り直して差し替える（`apply_captured_game_font`。ヒープを使う仕事は
/// 全部このスレッド）。文の更新の失敗と文字の表の作り直しの失敗は別々に受けて、片方が止まってももう片方は続ける。
fn start_updater(cfg: UpdaterCfg) -> Result<(), String> {
    if UPDATER_STARTED.swap(1, Ordering::SeqCst) != 0 {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("HiganRTool overlay".to_string())
        .spawn(move || {
            let mut u = Updater::default();
            let f = qpf();
            let mut game_font_ok = true;
            loop {
                let r = catch_unwind(AssertUnwindSafe(|| {
                    let line = u.step(
                        qpc(),
                        crate::draw::draw_calls(),
                        f,
                        crate::overlay::LATENCY.load(Ordering::Relaxed),
                    );
                    // 見本の旗が立っているときは、fps の行の代わりに見本の行を出し続ける（撮影で画面の中の見え方を撮る）。
                    TEXT.publish(if cfg.quiet {
                        ""
                    } else {
                        shown_text(cfg.preview, &line)
                    });
                }));
                if r.is_err() {
                    return;
                }
                if game_font_ok
                    && catch_unwind(AssertUnwindSafe(|| apply_captured_game_font(&cfg))).is_err()
                {
                    game_font_ok = false;
                    set_apply_failed("文字の表を作る仕事でパニックした".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .map(|_| ())
        .map_err(|e| {
            UPDATER_STARTED.store(0, Ordering::SeqCst);
            format!("文の更新スレッドを起こせない: {e}")
        })
}

/// 画面に出す文: 見本の旗が立っていれば見本の行（`PREVIEW_SCREEN_TEXT`）、なければ更新スレッドの計算した行。
fn shown_text(preview: bool, line: &str) -> &str {
    if preview {
        ov::PREVIEW_SCREEN_TEXT
    } else {
        line
    }
}

fn set_apply_failed(note: String) {
    if let Ok(mut g) = GF_NOTE.lock() {
        *g = note;
    }
    GF_WHY.store(GameFontWhy::ApplyFailed as u32, Ordering::Relaxed);
    GF_STATE.store(GF_APPLY_FAILED, Ordering::Release);
}

/// ゲームのスレッドが作業場へコピーしたゲームの文字（`GF_CAPTURED`）から、ゲームの文字をコピーした文字の表を作って差し替える（読み取りのスレッド）。
/// 作れなければ理由を残して GDI の文字のままにする。見本の旗が立っていれば、見本の PNG を本ログの隣へ書く。
fn apply_captured_game_font(cfg: &UpdaterCfg) {
    if GF_STATE.load(Ordering::Acquire) != GF_CAPTURED {
        return;
    }
    let (scratch, atlas_at) = (
        SCRATCH.load(Ordering::Acquire),
        ATLAS.load(Ordering::Acquire),
    );
    if scratch == 0 || atlas_at == 0 {
        set_apply_failed("作業場か文字の表が無い".to_string());
        return;
    }
    // SAFETY: `atlas_at` は `place_atlas` / この関数が `Box::leak` で作って返さない `Atlas`（Release で置いた）。
    let before: &'static Atlas = unsafe { &*(atlas_at as *const Atlas) };
    // SAFETY: `scratch` は `ensure_scratch` の `SCRATCH_WORDS` 語。`GF_CAPTURED` を Acquire で見たので、ゲームのスレッドの書きは見えていて、
    // 以後だれも書かない。
    let words = unsafe { core::slice::from_raw_parts(scratch as *const u32, SCRATCH_WORDS) };
    let mut pixels: Vec<(char, u32, u32, u32, u32, Vec<u32>)> = Vec::new();
    for i in 0..GAME_GLYPHS {
        let slot = &words[i * SLOT_WORDS..(i + 1) * SLOT_WORDS];
        let (w, h) = (slot[SLOT_PX], slot[SLOT_PX + 1]);
        let Some(c) =
            char::from_u32(ov::GAME_FIRST + i as u32).filter(|c| ov::game_index(*c) == Some(i))
        else {
            continue;
        };
        if w == 0 || h == 0 || w > GAME_SLOT_W || h > GAME_SLOT_H {
            continue;
        }
        let tight: Vec<u32> = (0..h as usize)
            .flat_map(|y| {
                slot[y * GAME_SLOT_W as usize..][..w as usize]
                    .iter()
                    .copied()
            })
            .collect();
        pixels.push((c, w, h, slot[SLOT_PX + 2], slot[SLOT_PX + 3], tight));
    }
    let srcs: Vec<GameGlyphSrc> = pixels
        .iter()
        .map(|(c, w, h, qw, qh, px)| GameGlyphSrc {
            c: *c,
            w: *w,
            h: *h,
            pixels: px,
            quad_w: *qw,
            quad_h: *qh,
        })
        .collect();
    match ov::apply_game_font(&before.spec, &before.texels, &srcs, GAME_ADVANCE) {
        Ok((spec, texels)) => {
            if cfg.preview {
                write_previews(cfg, before, &spec, &texels);
            }
            let a: &'static Atlas = Box::leak(Box::new(Atlas { spec, texels }));
            ATLAS.store(a as *const Atlas as usize, Ordering::Release);
            GF_STATE.store(GF_APPLIED, Ordering::Release);
        }
        Err(e) => set_apply_failed(e),
    }
}

/// 見本の PNG を本ログの隣へ書く（`<本ログの名前の先頭>_glyphs_lines.png`（GDI だけの表とゲームの文字の表を並べた、日本語と英数字の混ざった行・
/// ASCII 全部。3 倍に拡大）と `_glyphs_atlas.png`（コピーした後の文字の表そのもの。3 倍）と `_glyphs.txt`（`game_font_text`）。本ログは回ごとに
/// 別名で、開くのは `create_new`（既存を上書きしない）、置き場がゲームのフォルダの中なら書かない —— `RealRecord`）。失敗は本ログには書けない
/// （更新スレッドは本ログを持たない）ので、`GF_NOTE` に残して本ログの集計が読む。
fn write_previews(cfg: &UpdaterCfg, before: &Atlas, spec: &AtlasSpec, texels: &[u32]) {
    use crate::observe::RecordEnv;
    let mut rec = crate::record_win::RealRecord::new(cfg.run_path.clone(), cfg.exe_dir.clone());
    let mut note = String::new();
    let (w, h, sheet) = ov::preview_sheet_rgb(&[
        ("GDI only", &before.spec, &before.texels),
        ("Game font", spec, texels),
    ]);
    let atlas_img = ov::atlas_preview_rgb(spec, texels);
    let files: [(&str, u32, u32, Vec<u8>); 2] = [
        (
            "_glyphs_lines.png",
            w * 3,
            h * 3,
            ov::upscale_rgb(&sheet, w, h, 3),
        ),
        (
            "_glyphs_atlas.png",
            spec.tex_w * 3,
            spec.tex_h * 3,
            ov::upscale_rgb(&atlas_img, spec.tex_w, spec.tex_h, 3),
        ),
    ];
    for (suffix, pw, ph, rgb) in files {
        let r = higanr_core::shot::encode_png(pw, ph, &rgb)
            .map_err(|e| e.to_string())
            .and_then(|png_bytes| {
                let (name, mut f) = rec.open_output(suffix)?;
                f.write_all(&png_bytes)
                    .and_then(|()| f.flush())
                    .map_err(|e| format!("{name} に書けない: {e}"))?;
                Ok(name)
            });
        if let Err(e) = r {
            note.push_str(&format!("{suffix}: {e}。"));
        }
    }
    let text = game_font_text_with(GF_APPLIED);
    match rec.open_output("_glyphs.txt") {
        Ok((name, mut f)) => {
            if let Err(e) = f.write_all(text.as_bytes()).and_then(|()| f.flush()) {
                note.push_str(&format!("{name} に書けない: {e}。"));
            }
        }
        Err(e) => note.push_str(&format!("_glyphs.txt: {e}。")),
    }
    if !note.is_empty()
        && let Ok(mut g) = GF_NOTE.lock()
    {
        *g = format!("見本のファイル: {note}");
    }
}

fn game_font_text() -> String {
    game_font_text_with(GF_STATE.load(Ordering::Acquire))
}

fn game_font_text_with(state: u32) -> String {
    let why = GameFontWhy::from_code(GF_WHY.load(Ordering::Relaxed)).unwrap_or(GameFontWhy::None);
    let info: Vec<u32> = GF_INFO.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    let us = |t: &AtomicU32| t.load(Ordering::Relaxed) as f64 * 1_000_000.0 / qpf().max(1) as f64;
    let note = GF_NOTE.lock().map(|g| g.clone()).unwrap_or_default();
    let note = if note.is_empty() {
        String::new()
    } else {
        format!("（{note}）")
    };
    let done = || {
        format!(
            "ゲームのテクスチャ {}×{}・形式 {}・{} 文字、sprite[0x20]（@）の絵 {}×{}（画面に描く大きさ {}×{}）、コピーにかかった {:.1} µs",
            info[1],
            info[2],
            ov::texture_format_name(info[0]),
            info[3],
            info[4],
            info[5],
            info[6],
            info[7],
            us(&GF_TICKS),
        )
    };
    // 文字の表の差し替えでテクスチャを作り直せなかった回数（0 なら書かない）。描くのは止めず、差し替え前の文字で描き続ける。
    let swap_fails = SWAP_FAILS.load(Ordering::Relaxed);
    let swap = if swap_fails == 0 {
        String::new()
    } else {
        let why = Fail::from_code(SWAP_FAIL_WHY.load(Ordering::Relaxed)).map_or("不明", Fail::text);
        format!(
            "。ただしテクスチャの作り直しに {swap_fails} 回失敗した（最後の理由: {why}。HRESULT 0x{:08X}）ので、{}",
            SWAP_FAIL_HR.load(Ordering::Relaxed),
            if TEX_ATLAS.load(Ordering::Relaxed) == ATLAS.load(Ordering::Acquire) {
                "その後の試し直しで作り直せた"
            } else {
                "差し替え前の文字（GDI の文字）で描き続けている"
            }
        )
    };
    match state {
        GF_APPLIED => format!(
            "コピーした（{}）。英数字（0x21〜0x7E、` を除く）はゲームの文字で描く（送り {GAME_ADVANCE} 画素、行の枠の下端に揃える）。文字の表の差し替え（テクスチャの作り直し）{:.1} µs{note}{swap}",
            done(),
            us(&SWAP_TICKS)
        ),
        GF_CAPTURED => format!("コピーした（{}）。文字の表への反映はこれから", done()),
        GF_REFUSED if why == GameFontWhy::GaveUp => format!(
            "コピーしなかった（{}。試し {} 回 / 描画の入口 {} 回。最後の理由: {}。GDI の文字のまま）",
            why.text(),
            GF_ATTEMPTS.load(Ordering::Relaxed),
            GF_FRAMES.load(Ordering::Relaxed),
            GameFontWhy::from_code(GF_LAST_WAIT.load(Ordering::Relaxed))
                .unwrap_or(GameFontWhy::None)
                .text()
        ),
        GF_REFUSED => format!(
            "コピーしなかった（{}。GDI の文字のまま。形式 {} / テクスチャ {}×{}）",
            why.text(),
            ov::texture_format_name(info[0]),
            info[1],
            info[2]
        ),
        GF_APPLY_FAILED => {
            format!("コピーしたが文字の表を作れなかった（{note}。GDI の文字のまま）")
        }
        _ => format!(
            "まだコピーしていない（最後の理由: {}。試し {} 回 / 描画の入口 {} 回。GDI の文字のまま）",
            why.text(),
            GF_ATTEMPTS.load(Ordering::Relaxed),
            GF_FRAMES.load(Ordering::Relaxed)
        ),
    }
}

// ---- 本物の実装 ----

pub struct RealOverlayHost {
    ini: Result<Option<String>, String>,
    /// 見本の PNG の置き場（本ログのパスと exe のフォルダ）と、見本の旗。
    cfg: UpdaterCfg,
}

impl RealOverlayHost {
    pub fn new(
        ini: Result<Option<String>, String>,
        run_path: Option<std::path::PathBuf>,
        exe_dir: Option<std::path::PathBuf>,
    ) -> RealOverlayHost {
        RealOverlayHost {
            ini,
            cfg: UpdaterCfg {
                preview: false,
                quiet: false,
                run_path,
                exe_dir,
            },
        }
    }
}

impl OverlayHost for RealOverlayHost {
    fn read_ini(&mut self) -> Result<Option<String>, String> {
        self.ini.clone()
    }

    fn build_atlas(&mut self) -> Result<String, String> {
        // ゲームの文字の枠の塊を予約した文字の表を作る（ゲームの文字をコピーできなければ、GDI の文字のまま描く）。
        let a = default_atlas()?;
        let desc = a.describe();
        place_atlas(a.spec, a.texels)?;
        Ok(desc)
    }

    fn arm(&mut self) -> Result<(), String> {
        prepare(
            crate::th09_addr::D3D8_DEVICE_PTR,
            crate::th09_addr::D3D8_PRESENT_PARAMS,
        )?;
        let first = ov::status_line(&Status::default());
        TEXT.publish(if self.cfg.quiet {
            ""
        } else {
            shown_text(self.cfg.preview, &first)
        });
        start_updater(self.cfg.clone())
    }

    fn set_preview(&mut self, on: bool) {
        self.cfg.preview = on;
    }

    fn set_quiet(&mut self, on: bool) {
        self.cfg.quiet = on;
    }

    fn game_font_report(&self) -> String {
        game_font_text()
    }

    fn on(&mut self) {
        set_on();
    }

    fn counters(&self) -> OverlayCounters {
        counters()
    }

    fn timings(&self) -> TimingSnapshot {
        TIMINGS.snapshot()
    }

    fn qpf(&self) -> u64 {
        qpf()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! 偽の COM（vtable を持つ静的な偽のデバイス・テクスチャ・頂点バッファ）で、本物の描画の入口から文字の描画の経路を通す。
    //! 偽のデバイスは状態を配列に持ち、DrawPrimitiveUP の時点の状態をコピーして残す。
    use super::*;
    use crate::draw::{draw_entry_address, set_draw_trampoline};
    use crate::overlay::{GF_EVERY, GF_MAX_ATTEMPTS, SWAP_MAX_TRIES, SWAP_RETRY_EVERY};
    use crate::stub::testkit::{
        AFTER, EFLAGS_ARITH, PRE_ESP, PRE_FLAGS, REAL_HOOK_TESTS, REG_EAX, REG_EBP, REG_EBX,
        REG_ECX, REG_EDI, REG_EDX, SEEN, TARGET_HITS, call_with_regs, fake_this_addr, load,
        seen_target_addr,
    };
    use higanr_core::overlay::ASCII;
    use std::sync::MutexGuard;

    #[repr(C)]
    pub(crate) struct Obj(AtomicU32);
    static FAKE_DEVICE: Obj = Obj(AtomicU32::new(0));
    static FAKE_DEVICE2: Obj = Obj(AtomicU32::new(0));
    static OUR_TEX: Obj = Obj(AtomicU32::new(0));
    static GAME_TEX: Obj = Obj(AtomicU32::new(0));
    static GAME_VB: Obj = Obj(AtomicU32::new(0));
    static DEV_VTBL: [AtomicU32; 96] = [const { AtomicU32::new(0) }; 96];
    static TEX_VTBL: [AtomicU32; 24] = [const { AtomicU32::new(0) }; 24];
    static REF_VTBL: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
    static FAKE_DEVICE_PTR: AtomicU32 = AtomicU32::new(0);
    static FAKE_PP: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];

    static RS_STATE: [AtomicU32; 256] = [const { AtomicU32::new(0) }; 256];
    static TSS_STATE: [AtomicU32; 8 * 32] = [const { AtomicU32::new(0) }; 8 * 32];
    static TEX0: AtomicU32 = AtomicU32::new(0);
    static VS: AtomicU32 = AtomicU32::new(0);
    static VB0: AtomicU32 = AtomicU32::new(0);
    static STRIDE0: AtomicU32 = AtomicU32::new(0);
    static VIEWPORT: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
    static GAME_TEX_REFS: AtomicU32 = AtomicU32::new(0);
    static GAME_VB_REFS: AtomicU32 = AtomicU32::new(0);

    static TCL_HR: AtomicU32 = AtomicU32::new(0);
    static CREATE_HR: AtomicU32 = AtomicU32::new(0);
    static LOCK_HR: AtomicU32 = AtomicU32::new(0);
    static GET_RS_HR: AtomicU32 = AtomicU32::new(0);
    static GET_SS_HR: AtomicU32 = AtomicU32::new(0);
    static DRAW_HR: AtomicU32 = AtomicU32::new(0);
    const E_FAIL: u32 = 0x8000_4005;
    const DEVICELOST: u32 = 0x8876_0868;

    static N_CREATE: AtomicU32 = AtomicU32::new(0);
    static N_LOCK: AtomicU32 = AtomicU32::new(0);
    static N_UNLOCK: AtomicU32 = AtomicU32::new(0);
    static N_OUR_RELEASE: AtomicU32 = AtomicU32::new(0);
    static N_DRAW: AtomicU32 = AtomicU32::new(0);
    static N_SET: AtomicU32 = AtomicU32::new(0);
    static N_GET: AtomicU32 = AtomicU32::new(0);
    static BAD_ARGS: AtomicU32 = AtomicU32::new(0);

    static AT_DRAW_RS: [AtomicU32; 12] = [const { AtomicU32::new(0) }; 12];
    static AT_DRAW_TSS: [AtomicU32; 15] = [const { AtomicU32::new(0) }; 15];
    static AT_DRAW_TEX: AtomicU32 = AtomicU32::new(0);
    static AT_DRAW_VS: AtomicU32 = AtomicU32::new(0);
    static AT_DRAW_VP: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
    static AT_DRAW: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
    static AT_DRAW_V0: [AtomicU32; 7] = [const { AtomicU32::new(0) }; 7];

    /// 自分のテクスチャの画素の置き場。pitch は 1 行（幅 × 4 バイト）＋ 詰め物 16 バイト（`pitch()`。128 幅なら 528）。
    /// 日本語の文字の表（幅・高さとも 512 まで）が入る大きさ。
    const BITS_WORDS: usize = (512 * 4 + 16) * 512 / 4;
    static TEX_BITS: [AtomicU32; BITS_WORDS] = [const { AtomicU32::new(0) }; BITS_WORDS];
    static CREATE_DIMS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

    static GAME_GLYPH_TEX: Obj = Obj(AtomicU32::new(0));
    static FAKE_L_PTR: AtomicU32 = AtomicU32::new(0);
    static FAKE_MGR_PTR: AtomicU32 = AtomicU32::new(0);
    static FAKE_L: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
    const SPRITE_WORDS: usize = 0x44 / 4;
    static FAKE_SPRITES: [AtomicU32; 0x60 * SPRITE_WORDS] =
        [const { AtomicU32::new(0) }; 0x60 * SPRITE_WORDS];
    const GLYPH_TEX_SIDE: u32 = 128;
    const GLYPH_PITCH: usize = GLYPH_TEX_SIDE as usize * 4 + 16;
    static GLYPH_BITS: [AtomicU32; GLYPH_PITCH / 4 * GLYPH_TEX_SIDE as usize] =
        [const { AtomicU32::new(0) }; GLYPH_PITCH / 4 * GLYPH_TEX_SIDE as usize];
    static GLYPH_FMT: AtomicU32 = AtomicU32::new(0);
    static GLYPH_W: AtomicU32 = AtomicU32::new(0);
    static GLYPH_H: AtomicU32 = AtomicU32::new(0);
    static GLD_HR: AtomicU32 = AtomicU32::new(0);
    static GAME_LOCK_HR: AtomicU32 = AtomicU32::new(0);
    static N_GLD: AtomicU32 = AtomicU32::new(0);
    static N_GAME_LOCK: AtomicU32 = AtomicU32::new(0);
    static N_GAME_UNLOCK: AtomicU32 = AtomicU32::new(0);
    fn pitch() -> usize {
        atlas().spec.tex_w as usize * 4 + 16
    }

    fn addr_of<T>(x: &'static T) -> u32 {
        x as *const T as usize as u32
    }
    fn dev_ok(this: u32) -> bool {
        this == addr_of(&FAKE_DEVICE) || this == addr_of(&FAKE_DEVICE2)
    }
    fn bad() {
        BAD_ARGS.fetch_add(1, Ordering::SeqCst);
    }

    extern "system" fn f_tcl(this: u32) -> i32 {
        if !dev_ok(this) {
            bad();
        }
        TCL_HR.load(Ordering::SeqCst) as i32
    }
    #[allow(clippy::too_many_arguments)] // D3D8 の CreateTexture の形（this と 7 つの引数）そのまま
    extern "system" fn f_create(
        this: u32,
        w: u32,
        h: u32,
        levels: u32,
        usage: u32,
        fmt: u32,
        pool: u32,
        out: *mut u32,
    ) -> i32 {
        N_CREATE.fetch_add(1, Ordering::SeqCst);
        CREATE_DIMS[0].store(w, Ordering::SeqCst);
        CREATE_DIMS[1].store(h, Ordering::SeqCst);
        let (aw, ah) = (atlas().spec.tex_w, atlas().spec.tex_h);
        if !dev_ok(this)
            || (w, h, levels, usage, fmt, pool) != (aw, ah, 1, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED)
        {
            bad();
        }
        let hr = CREATE_HR.load(Ordering::SeqCst);
        if hr != 0 {
            return hr as i32;
        }
        // SAFETY: `out` は `create_texture` のスタックの 4 バイト。
        unsafe { out.write(addr_of(&OUR_TEX)) };
        0
    }
    extern "system" fn f_get_rs(this: u32, st: u32, out: *mut u32) -> i32 {
        N_GET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        let hr = GET_RS_HR.load(Ordering::SeqCst);
        if hr != 0 {
            return hr as i32;
        }
        // SAFETY: `out` は `Saved` の欄。
        unsafe { out.write(RS_STATE[st as usize].load(Ordering::SeqCst)) };
        0
    }
    extern "system" fn f_set_rs(this: u32, st: u32, v: u32) -> i32 {
        N_SET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        RS_STATE[st as usize].store(v, Ordering::SeqCst);
        0
    }
    extern "system" fn f_get_tss(this: u32, stage: u32, ty: u32, out: *mut u32) -> i32 {
        N_GET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        // SAFETY: `out` は `Saved` の欄。
        unsafe { out.write(TSS_STATE[(stage * 32 + ty) as usize].load(Ordering::SeqCst)) };
        0
    }
    extern "system" fn f_set_tss(this: u32, stage: u32, ty: u32, v: u32) -> i32 {
        N_SET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        TSS_STATE[(stage * 32 + ty) as usize].store(v, Ordering::SeqCst);
        0
    }
    extern "system" fn f_get_tex(this: u32, stage: u32, out: *mut u32) -> i32 {
        N_GET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) || stage != 0 {
            bad();
        }
        let t = TEX0.load(Ordering::SeqCst);
        if t == addr_of(&GAME_TEX) {
            GAME_TEX_REFS.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: `out` は `Saved` の欄。
        unsafe { out.write(t) };
        0
    }
    extern "system" fn f_set_tex(this: u32, stage: u32, t: u32) -> i32 {
        N_SET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) || stage != 0 {
            bad();
        }
        TEX0.store(t, Ordering::SeqCst);
        0
    }
    extern "system" fn f_get_vs(this: u32, out: *mut u32) -> i32 {
        N_GET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        // SAFETY: `out` は `Saved` の欄。
        unsafe { out.write(VS.load(Ordering::SeqCst)) };
        0
    }
    extern "system" fn f_set_vs(this: u32, h: u32) -> i32 {
        N_SET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        VS.store(h, Ordering::SeqCst);
        0
    }
    extern "system" fn f_get_vp(this: u32, out: *mut u32) -> i32 {
        N_GET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        for (i, v) in VIEWPORT.iter().enumerate() {
            // SAFETY: `out` は `Saved` の 6 語。
            unsafe { out.add(i).write(v.load(Ordering::SeqCst)) };
        }
        0
    }
    extern "system" fn f_set_vp(this: u32, vp: *const u32) -> i32 {
        N_SET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        for (i, v) in VIEWPORT.iter().enumerate() {
            // SAFETY: `vp` は 6 語の配列。
            v.store(unsafe { vp.add(i).read() }, Ordering::SeqCst);
        }
        0
    }
    extern "system" fn f_get_ss(this: u32, n: u32, vb: *mut u32, stride: *mut u32) -> i32 {
        N_GET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) || n != 0 {
            bad();
        }
        let hr = GET_SS_HR.load(Ordering::SeqCst);
        if hr != 0 {
            return hr as i32;
        }
        let v = VB0.load(Ordering::SeqCst);
        if v == addr_of(&GAME_VB) {
            GAME_VB_REFS.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: どちらも `Saved` の欄。
        unsafe {
            vb.write(v);
            stride.write(STRIDE0.load(Ordering::SeqCst));
        }
        0
    }
    extern "system" fn f_set_ss(this: u32, n: u32, vb: u32, stride: u32) -> i32 {
        N_SET.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) || n != 0 {
            bad();
        }
        VB0.store(vb, Ordering::SeqCst);
        STRIDE0.store(stride, Ordering::SeqCst);
        0
    }
    extern "system" fn f_draw_up(
        this: u32,
        ty: u32,
        count: u32,
        data: *const c_void,
        stride: u32,
    ) -> i32 {
        N_DRAW.fetch_add(1, Ordering::SeqCst);
        if !dev_ok(this) {
            bad();
        }
        for (i, &(st, _)) in RS.iter().enumerate() {
            AT_DRAW_RS[i].store(
                RS_STATE[st as usize].load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
        }
        for (i, &(stage, t, _)) in TSS.iter().enumerate() {
            AT_DRAW_TSS[i].store(
                TSS_STATE[(stage * 32 + t) as usize].load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
        }
        AT_DRAW_TEX.store(TEX0.load(Ordering::SeqCst), Ordering::SeqCst);
        AT_DRAW_VS.store(VS.load(Ordering::SeqCst), Ordering::SeqCst);
        for (a, v) in AT_DRAW_VP.iter().zip(VIEWPORT.iter()) {
            a.store(v.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        for (a, v) in AT_DRAW
            .iter()
            .zip([ty, count, stride, data as usize as u32])
        {
            a.store(v, Ordering::SeqCst);
        }
        for (i, a) in AT_DRAW_V0.iter().enumerate() {
            // SAFETY: `data` は `overlay_inner` のスタックの頂点の並び（1 つ以上。28 バイト = 7 語）。
            a.store(
                unsafe { (data as *const u32).add(i).read() },
                Ordering::SeqCst,
            );
        }
        VB0.store(0, Ordering::SeqCst);
        STRIDE0.store(0, Ordering::SeqCst);
        DRAW_HR.load(Ordering::SeqCst) as i32
    }
    extern "system" fn f_lock(
        this: u32,
        level: u32,
        locked: *mut u32,
        rect: *const c_void,
        flags: u32,
    ) -> i32 {
        if this == addr_of(&GAME_GLYPH_TEX) {
            N_GAME_LOCK.fetch_add(1, Ordering::SeqCst);
            if level != 0 || !rect.is_null() || flags != D3DLOCK_READONLY {
                bad();
            }
            let hr = GAME_LOCK_HR.load(Ordering::SeqCst);
            if hr != 0 {
                return hr as i32;
            }
            // SAFETY: `locked` は `game_font_step` のスタックの 2 語。
            unsafe {
                locked.write(GLYPH_PITCH as u32);
                locked.add(1).write(addr_of(&GLYPH_BITS));
            }
            return 0;
        }
        N_LOCK.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&OUR_TEX) || level != 0 || !rect.is_null() || flags != 0 {
            bad();
        }
        let hr = LOCK_HR.load(Ordering::SeqCst);
        if hr != 0 {
            return hr as i32;
        }
        // SAFETY: `locked` は `create_texture` のスタックの 2 語。
        unsafe {
            locked.write(pitch() as u32);
            locked.add(1).write(addr_of(&TEX_BITS));
        }
        0
    }
    extern "system" fn f_unlock(this: u32, level: u32) -> i32 {
        if this == addr_of(&GAME_GLYPH_TEX) {
            N_GAME_UNLOCK.fetch_add(1, Ordering::SeqCst);
            if level != 0 {
                bad();
            }
            return 0;
        }
        N_UNLOCK.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&OUR_TEX) || level != 0 {
            bad();
        }
        0
    }
    extern "system" fn f_get_level_desc(this: u32, level: u32, desc: *mut u32) -> i32 {
        N_GLD.fetch_add(1, Ordering::SeqCst);
        if this != addr_of(&GAME_GLYPH_TEX) || level != 0 {
            bad();
        }
        let hr = GLD_HR.load(Ordering::SeqCst);
        if hr != 0 {
            return hr as i32;
        }
        let (fmt, w, h) = (
            GLYPH_FMT.load(Ordering::SeqCst),
            GLYPH_W.load(Ordering::SeqCst),
            GLYPH_H.load(Ordering::SeqCst),
        );
        for (i, v) in [fmt, 3, 0, 1, 0, 0, w, h].into_iter().enumerate() {
            // SAFETY: `desc` は `game_font_step` のスタックの 8 語。
            unsafe { desc.add(i).write(v) };
        }
        0
    }
    extern "system" fn f_our_release(this: u32) -> u32 {
        if this != addr_of(&OUR_TEX) {
            bad();
        }
        N_OUR_RELEASE.fetch_add(1, Ordering::SeqCst);
        0
    }
    extern "system" fn f_ref_release(this: u32) -> u32 {
        let c = if this == addr_of(&GAME_TEX) {
            &GAME_TEX_REFS
        } else if this == addr_of(&GAME_VB) {
            &GAME_VB_REFS
        } else {
            bad();
            return 0;
        };
        c.fetch_sub(1, Ordering::SeqCst).wrapping_sub(1)
    }

    fn synthetic_atlas() -> (AtlasSpec, Vec<u32>) {
        let spec = AtlasSpec::new(7, 14, &ASCII).unwrap();
        let texels = (0..spec.texel_count() as u32)
            .map(|i| i.wrapping_mul(2_654_435_761))
            .collect();
        (spec, texels)
    }

    static SYNTH_ATLAS: AtomicUsize = AtomicUsize::new(0);
    static BIG_ATLAS: AtomicUsize = AtomicUsize::new(0);

    fn big_synthetic_atlas() -> (AtlasSpec, Vec<u32>) {
        let spec = AtlasSpec::fit(7, 14, &ov::ui_glyphs()).unwrap();
        let cov: Vec<u8> = (0..spec.texel_count() as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let texels = ov::texels(&spec, &cov).unwrap();
        (spec, texels)
    }

    /// `slot` に覚えてある文字の表のアドレスを返す。無ければ `make` で作って置く（`place_atlas` は `ATLAS` が空でなければ断るので、いったん
    /// 空にしてから置く）。呼ぶのは `REAL_HOOK_TESTS` の錠を持つ間だけ。
    fn kept_atlas(slot: &AtomicUsize, make: fn() -> (AtlasSpec, Vec<u32>)) -> usize {
        match slot.load(Ordering::SeqCst) {
            0 => {
                ATLAS.store(0, Ordering::SeqCst);
                let (spec, texels) = make();
                place_atlas(spec, texels).expect("文字の表");
                let at = ATLAS.load(Ordering::SeqCst);
                slot.store(at, Ordering::SeqCst);
                at
            }
            at => at,
        }
    }

    fn use_big_atlas() {
        let at = kept_atlas(&BIG_ATLAS, big_synthetic_atlas);
        ATLAS.store(at, Ordering::SeqCst);
    }

    static GAME_ATLAS: AtomicUsize = AtomicUsize::new(0);

    fn game_synthetic_atlas() -> (AtlasSpec, Vec<u32>) {
        let spec = AtlasSpec::fit_game(7, 14, &ov::ui_glyphs()).unwrap();
        let cov: Vec<u8> = (0..spec.texel_count() as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let texels = ov::texels(&spec, &cov).unwrap();
        (spec, texels)
    }

    fn use_game_atlas() {
        let at = kept_atlas(&GAME_ATLAS, game_synthetic_atlas);
        ATLAS.store(at, Ordering::SeqCst);
    }

    fn reset_game_font() {
        for a in [
            &GF_STATE,
            &GF_WHY,
            &GF_LOCK_FAILS,
            &GF_FRAMES,
            &GF_ATTEMPTS,
            &GF_LAST_WAIT,
            &GF_TICKS,
            &SWAP_TICKS,
            &SWAP_TRIES,
            &SWAP_SINCE,
            &SWAP_FAILS,
            &SWAP_FAIL_WHY,
            &SWAP_FAIL_HR,
            &GLD_HR,
            &GAME_LOCK_HR,
            &N_GLD,
            &N_GAME_LOCK,
            &N_GAME_UNLOCK,
        ] {
            a.store(0, Ordering::SeqCst);
        }
        for a in &GF_INFO {
            a.store(0, Ordering::SeqCst);
        }
        TEX_ATLAS.store(0, Ordering::SeqCst);
        if let Ok(mut g) = GF_NOTE.lock() {
            g.clear();
        }
        GF_LOADED_PTR.store(0x10, Ordering::SeqCst);
        GF_MGR_PTR.store(0x10, Ordering::SeqCst);
        let scratch = SCRATCH.load(Ordering::SeqCst);
        if scratch != 0 {
            // SAFETY: `ensure_scratch` の `SCRATCH_WORDS` 語。検査は `REAL_HOOK_TESTS` の錠を持つ間だけ動くので、
            // ゲームのスレッドの代わりの書き手ともこの書き手しかいない。
            unsafe { core::ptr::write_bytes(scratch as *mut u32, 0, SCRATCH_WORDS) };
        }
    }

    /// 偽のゲームの物を「読み込み済み・文字の絵が 128×128 の一枚に並んでいる」形に置く。文字 c（`0x21`〜`0x7E`）の絵は幅 7 × 高さ 9、
    /// `(k % 16) × 8`, `(k / 16) × 10`（k = c − 0x21）。`` ` `` のスプライトも置く（使わないことを見る）。画素は A8R8G8B8
    /// （B = x, G = y, R = 0xAA, A = 0xFF）で、`fmt` を変えるなら `fill_glyph_pixels` で入れ直す。
    fn game_world() {
        let l = addr_of(&FAKE_L);
        FAKE_L_PTR.store(l, Ordering::SeqCst);
        FAKE_MGR_PTR.store(l - 0x144, Ordering::SeqCst);
        for w in FAKE_L.iter() {
            w.store(0, Ordering::SeqCst);
        }
        FAKE_L[1].store(0x1234, Ordering::SeqCst); // +4 ファイルの中身
        FAKE_L[3].store(addr_of(&FAKE_SPRITES), Ordering::SeqCst); // +0x0C
        FAKE_L[6].store(0, Ordering::SeqCst); // +0x18 進み
        for (i, ch) in b"ascii.anm\0".chunks(4).enumerate() {
            let mut b = [0u8; 4];
            b[..ch.len()].copy_from_slice(ch);
            FAKE_L[8 + i].store(u32::from_le_bytes(b), Ordering::SeqCst); // +0x20 名前
        }
        GF_LOADED_PTR.store(addr_of(&FAKE_L_PTR), Ordering::SeqCst);
        GF_MGR_PTR.store(addr_of(&FAKE_MGR_PTR), Ordering::SeqCst);
        for c in 0x21u32..=0x7E {
            let k = c - 0x21;
            let (x0, y0) = ((k % 16) * 8, (k / 16) * 10);
            set_sprite(
                c,
                [x0, y0, x0 + 7, y0 + 9],
                (GLYPH_TEX_SIDE, GLYPH_TEX_SIDE),
            );
        }
        GAME_GLYPH_TEX.0.store(addr_of(&TEX_VTBL), Ordering::SeqCst);
        GLD_HR.store(0, Ordering::SeqCst);
        GAME_LOCK_HR.store(0, Ordering::SeqCst);
        fill_glyph_pixels(D3DFMT_A8R8G8B8);
        GLYPH_W.store(GLYPH_TEX_SIDE, Ordering::SeqCst);
        GLYPH_H.store(GLYPH_TEX_SIDE, Ordering::SeqCst);
    }

    fn set_sprite(c: u32, r: [u32; 4], (tw, th): (u32, u32)) {
        let base = (c - 0x20) as usize * SPRITE_WORDS;
        let f = |v: u32| (v as f32).to_bits();
        let words = [
            (1, addr_of(&GAME_GLYPH_TEX)),
            (2, f(r[0])),
            (3, f(r[1])),
            (4, f(r[2])),
            (5, f(r[3])),
            (6, f(th)),
            (7, f(tw)),
            (12, f(r[3] - r[1])),
            (13, f(r[2] - r[0])),
        ];
        for w in &FAKE_SPRITES[base..base + SPRITE_WORDS] {
            w.store(0, Ordering::SeqCst);
        }
        for (i, v) in words {
            FAKE_SPRITES[base + i].store(v, Ordering::SeqCst);
        }
    }

    fn fill_glyph_pixels(fmt: u32) {
        GLYPH_FMT.store(fmt, Ordering::SeqCst);
        let bpp = ov::texture_pixel_bytes(fmt).unwrap_or(4);
        assert!(bpp * GLYPH_TEX_SIDE as usize <= GLYPH_PITCH - 16);
        let mut bytes = vec![0xEEu8; GLYPH_PITCH * GLYPH_TEX_SIDE as usize];
        for y in 0..GLYPH_TEX_SIDE as usize {
            for x in 0..GLYPH_TEX_SIDE as usize {
                let (a, r, g, b) = (0xFFu32, 0xAAu32, y as u32, x as u32);
                let px: Vec<u8> = match fmt {
                    D3DFMT_A8R8G8B8 => vec![b as u8, g as u8, r as u8, a as u8],
                    ov::D3DFMT_A4R4G4B4 => {
                        let v = (a >> 4) << 12 | (r >> 4) << 8 | (g >> 4 & 15) << 4 | (b >> 4 & 15);
                        (v as u16).to_le_bytes().to_vec()
                    }
                    higanr_core::shot::D3DFMT_R5G6B5 => {
                        let v = (r >> 3) << 11 | (g >> 2 & 63) << 5 | (b >> 3 & 31);
                        (v as u16).to_le_bytes().to_vec()
                    }
                    _ => panic!("検査が知らない形式 {fmt}"),
                };
                let at = y * GLYPH_PITCH + x * bpp;
                bytes[at..at + bpp].copy_from_slice(&px);
            }
        }
        for (i, c) in bytes.as_chunks::<4>().0.iter().enumerate() {
            GLYPH_BITS[i].store(
                u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                Ordering::SeqCst,
            );
        }
    }

    fn game_memory_snapshot() -> Vec<u32> {
        [
            &FAKE_L[..],
            &FAKE_SPRITES[..],
            &GLYPH_BITS[..],
            std::slice::from_ref(&FAKE_L_PTR),
            std::slice::from_ref(&FAKE_MGR_PTR),
            std::slice::from_ref(&GAME_GLYPH_TEX.0),
        ]
        .iter()
        .flat_map(|s| s.iter().map(|a| a.load(Ordering::SeqCst)))
        .collect()
    }

    const GAME_VP: [u32; 6] = [32, 16, 384, 448, 0, 0x3F80_0000];
    const GAME_VS: u32 = 0x102;

    fn game_state() {
        for (i, a) in RS_STATE.iter().enumerate() {
            a.store(0x1000 + i as u32, Ordering::SeqCst);
        }
        for (i, a) in TSS_STATE.iter().enumerate() {
            a.store(0x2000 + i as u32, Ordering::SeqCst);
        }
        TEX0.store(addr_of(&GAME_TEX), Ordering::SeqCst);
        VS.store(GAME_VS, Ordering::SeqCst);
        VB0.store(addr_of(&GAME_VB), Ordering::SeqCst);
        STRIDE0.store(0x14, Ordering::SeqCst);
        for (a, v) in VIEWPORT.iter().zip(GAME_VP) {
            a.store(v, Ordering::SeqCst);
        }
        GAME_TEX_REFS.store(1, Ordering::SeqCst);
        GAME_VB_REFS.store(1, Ordering::SeqCst);
    }

    fn device_state() -> Vec<u32> {
        let mut v: Vec<u32> = RS_STATE.iter().map(|a| a.load(Ordering::SeqCst)).collect();
        v.extend(TSS_STATE.iter().map(|a| a.load(Ordering::SeqCst)));
        v.extend(VIEWPORT.iter().map(|a| a.load(Ordering::SeqCst)));
        v.extend(
            [&TEX0, &VS, &VB0, &STRIDE0, &GAME_TEX_REFS, &GAME_VB_REFS]
                .map(|a| a.load(Ordering::SeqCst)),
        );
        v
    }

    pub(crate) fn setup() -> MutexGuard<'static, ()> {
        let g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for w in DEV_VTBL
            .iter()
            .chain(TEX_VTBL.iter())
            .chain(REF_VTBL.iter())
        {
            w.store(0, Ordering::SeqCst);
        }
        let dev_fns: [(u32, usize); 15] = [
            (DEV_TESTCOOPERATIVELEVEL, f_tcl as *const () as usize),
            (DEV_CREATETEXTURE, f_create as *const () as usize),
            (DEV_GETRENDERSTATE, f_get_rs as *const () as usize),
            (DEV_SETRENDERSTATE, f_set_rs as *const () as usize),
            (DEV_GETTEXTURESTAGESTATE, f_get_tss as *const () as usize),
            (DEV_SETTEXTURESTAGESTATE, f_set_tss as *const () as usize),
            (DEV_GETTEXTURE, f_get_tex as *const () as usize),
            (DEV_SETTEXTURE, f_set_tex as *const () as usize),
            (DEV_GETVERTEXSHADER, f_get_vs as *const () as usize),
            (DEV_SETVERTEXSHADER, f_set_vs as *const () as usize),
            (DEV_GETVIEWPORT, f_get_vp as *const () as usize),
            (DEV_SETVIEWPORT, f_set_vp as *const () as usize),
            (DEV_GETSTREAMSOURCE, f_get_ss as *const () as usize),
            (DEV_SETSTREAMSOURCE, f_set_ss as *const () as usize),
            (DEV_DRAWPRIMITIVEUP, f_draw_up as *const () as usize),
        ];
        for (slot, f) in dev_fns {
            DEV_VTBL[(slot / 4) as usize].store(f as u32, Ordering::SeqCst);
        }
        for (slot, f) in [
            (TEX_GETLEVELDESC, f_get_level_desc as *const () as usize),
            (TEX_LOCKRECT, f_lock as *const () as usize),
            (TEX_UNLOCKRECT, f_unlock as *const () as usize),
            (COM_RELEASE, f_our_release as *const () as usize),
        ] {
            TEX_VTBL[(slot / 4) as usize].store(f as u32, Ordering::SeqCst);
        }
        GAME_GLYPH_TEX.0.store(addr_of(&TEX_VTBL), Ordering::SeqCst);
        REF_VTBL[(COM_RELEASE / 4) as usize]
            .store(f_ref_release as *const () as usize as u32, Ordering::SeqCst);
        FAKE_DEVICE.0.store(addr_of(&DEV_VTBL), Ordering::SeqCst);
        FAKE_DEVICE2.0.store(addr_of(&DEV_VTBL), Ordering::SeqCst);
        OUR_TEX.0.store(addr_of(&TEX_VTBL), Ordering::SeqCst);
        GAME_TEX.0.store(addr_of(&REF_VTBL), Ordering::SeqCst);
        GAME_VB.0.store(addr_of(&REF_VTBL), Ordering::SeqCst);
        FAKE_DEVICE_PTR.store(addr_of(&FAKE_DEVICE), Ordering::SeqCst);
        for (a, v) in FAKE_PP.iter().zip([640, 480, 22]) {
            a.store(v, Ordering::SeqCst);
        }
        for a in [
            &TCL_HR, &CREATE_HR, &LOCK_HR, &GET_RS_HR, &GET_SS_HR, &DRAW_HR,
        ] {
            a.store(0, Ordering::SeqCst);
        }
        for w in TEX_BITS.iter() {
            w.store(0xEEEE_EEEE, Ordering::SeqCst);
        }
        game_state();
        set_off();
        for a in [
            &TEX,
            &TEX_DEVICE,
            &FAIL,
            &FAIL_HR,
            &DRAWN,
            &LOST,
            &NO_DEVICE,
            &TORN,
            &DRAW_FAILED,
            &SET_FAILED,
            &PANICS,
            &SETUP_TICKS,
        ] {
            a.store(0, Ordering::SeqCst);
        }
        TIMINGS.reset();
        reset_game_font();
        ATLAS.store(kept_atlas(&SYNTH_ATLAS, synthetic_atlas), Ordering::SeqCst);
        prepare(addr_of(&FAKE_DEVICE_PTR), addr_of(&FAKE_PP)).expect("prepare");
        TEXT.publish("HiganRTool 60.00fps");
        crate::shot_win::set_off();
        set_on();
        g
    }

    fn teardown() {
        set_off();
        set_draw_trampoline(0);
    }

    fn draw_once() {
        set_draw_trampoline(seen_target_addr());
        call_with_regs(draw_entry_address());
    }

    fn atlas() -> &'static Atlas {
        // SAFETY: `setup` が置いた、返さない文字の表。
        unsafe { &*(ATLAS.load(Ordering::SeqCst) as *const Atlas) }
    }

    fn assert_texture_copied(ys: &[usize], xs: &[usize]) {
        let a = atlas();
        let (w, p) = (a.spec.tex_w as usize, pitch());
        for &y in ys {
            for &x in xs {
                assert_eq!(
                    TEX_BITS[(y * p) / 4 + x].load(Ordering::SeqCst),
                    a.texels[y * w + x],
                    "テクスチャの画素 ({x}, {y})"
                );
            }
            assert_eq!(
                TEX_BITS[(y * p + w * 4) / 4].load(Ordering::SeqCst),
                0xEEEE_EEEE,
                "詰め物に書いた"
            );
        }
    }

    #[test]
    fn draw_entry_draws_with_our_state_and_restores_the_game_state() {
        let _g = setup();
        let before = device_state();
        let (bad0, hits0, draw0, create0) = (
            BAD_ARGS.load(Ordering::SeqCst),
            TARGET_HITS.load(Ordering::SeqCst),
            N_DRAW.load(Ordering::SeqCst),
            N_CREATE.load(Ordering::SeqCst),
        );
        draw_once();
        assert_eq!(
            TARGET_HITS.load(Ordering::SeqCst),
            hits0 + 1,
            "トランポリンへ飛んでいない"
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

        assert_eq!(
            BAD_ARGS.load(Ordering::SeqCst),
            bad0,
            "偽の関数に渡した this か引数が違う"
        );
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst),
            create0 + 1,
            "テクスチャを 1 回作る"
        );
        assert_eq!(
            N_DRAW.load(Ordering::SeqCst),
            draw0 + 1,
            "DrawPrimitiveUP を 1 回"
        );
        assert_eq!(
            [
                CREATE_DIMS[0].load(Ordering::SeqCst),
                CREATE_DIMS[1].load(Ordering::SeqCst)
            ],
            [128, 128],
            "合成の表は 128 × 128"
        );
        assert_texture_copied(&[0, 1, 64, 127], &[0, 1, 127]);
        for (i, &(st, v)) in RS.iter().enumerate() {
            assert_eq!(
                AT_DRAW_RS[i].load(Ordering::SeqCst),
                v,
                "描く時点のレンダーステート {st}"
            );
        }
        for (i, &(stage, t, v)) in TSS.iter().enumerate() {
            assert_eq!(
                AT_DRAW_TSS[i].load(Ordering::SeqCst),
                v,
                "描く時点のステージ {stage} の {t}"
            );
        }
        assert_eq!(AT_DRAW_TEX.load(Ordering::SeqCst), addr_of(&OUR_TEX));
        assert_eq!(AT_DRAW_VS.load(Ordering::SeqCst), 0x144);
        let vp: Vec<u32> = AT_DRAW_VP
            .iter()
            .map(|a| a.load(Ordering::SeqCst))
            .collect();
        assert_eq!(vp, vec![0, 0, 640, 480, 0, 0x3F80_0000]);
        let d: Vec<u32> = AT_DRAW.iter().map(|a| a.load(Ordering::SeqCst)).collect();
        assert_eq!(&d[..3], &[D3DPT_TRIANGLELIST, 38, 28]);
        let v0: Vec<u32> = AT_DRAW_V0
            .iter()
            .map(|a| a.load(Ordering::SeqCst))
            .collect();
        assert_eq!(f32::from_bits(v0[0]), 501.0 - 0.5);
        assert_eq!(f32::from_bits(v0[1]), 444.0 - 0.5);
        assert_eq!(v0[4], STYLE_DEFAULT.back);
        assert_eq!(device_state(), before, "ゲームの状態を戻していない");
        let c = counters();
        assert_eq!((c.drawn, c.fail, c.set_failed), (1, 0, 0));

        draw_once();
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst),
            create0 + 1,
            "テクスチャを 2 回作った"
        );
        assert_eq!(N_DRAW.load(Ordering::SeqCst), draw0 + 2);
        assert_eq!(device_state(), before);
        assert_eq!(TIMINGS.snapshot().total, 1);
        teardown();
    }

    #[test]
    fn draw_entry_draws_the_board_in_batches_with_one_state_save() {
        use crate::net_ui::{ROW_TL0, SHOW_MENU, SHOW_TL0};
        use higanr_core::ui_layout::RowStyle;
        let _g = setup();
        let before = device_state();
        let (d0, s0, g0) = (
            N_DRAW.load(Ordering::SeqCst),
            N_SET.load(Ordering::SeqCst),
            N_GET.load(Ordering::SeqCst),
        );
        draw_once();
        let (d_base, s_base, g_base) = (
            N_DRAW.load(Ordering::SeqCst) - d0,
            N_SET.load(Ordering::SeqCst) - s0,
            N_GET.load(Ordering::SeqCst) - g0,
        );
        assert_eq!(d_base, 1);
        BOARD.set_row(0, "接続コード", RowStyle::Title);
        BOARD.set_row(1, "", RowStyle::Normal);
        BOARD.set_row(2, "> 戻る", RowStyle::Selected);
        BOARD.set_menu_len(3);
        BOARD.set_row(ROW_TL0, "待っています", RowStyle::Normal);
        BOARD.set_show(SHOW_MENU | SHOW_TL0);
        let (d1, s1, g1) = (
            N_DRAW.load(Ordering::SeqCst),
            N_SET.load(Ordering::SeqCst),
            N_GET.load(Ordering::SeqCst),
        );
        draw_once();
        assert_eq!(
            N_DRAW.load(Ordering::SeqCst) - d1,
            d_base + 4,
            "まとまりの数"
        );
        assert_eq!(
            N_SET.load(Ordering::SeqCst) - s1,
            s_base,
            "状態を置く回数はまとまりの数によらない"
        );
        assert_eq!(
            N_GET.load(Ordering::SeqCst) - g1,
            g_base,
            "状態を読む回数はまとまりの数によらない"
        );
        assert_eq!(device_state(), before, "ゲームの状態を戻していない");
        BOARD.set_show(0);
        let d2 = N_DRAW.load(Ordering::SeqCst);
        draw_once();
        assert_eq!(N_DRAW.load(Ordering::SeqCst) - d2, d_base);
        assert_eq!(counters().draw_failed, 0);
        teardown();
    }

    #[test]
    fn states_already_ours_are_not_touched() {
        let _g = setup();
        for &(st, v) in RS.iter() {
            RS_STATE[st as usize].store(v, Ordering::SeqCst);
        }
        for &(stage, t, v) in TSS.iter() {
            TSS_STATE[(stage * 32 + t) as usize].store(v, Ordering::SeqCst);
        }
        VS.store(FVF, Ordering::SeqCst);
        for (a, v) in VIEWPORT.iter().zip([0, 0, 640, 480, 0, 0x3F80_0000]) {
            a.store(v, Ordering::SeqCst);
        }
        TEX0.store(0, Ordering::SeqCst);
        VB0.store(0, Ordering::SeqCst);
        STRIDE0.store(0, Ordering::SeqCst);
        let before = device_state();
        let s0 = N_SET.load(Ordering::SeqCst);
        draw_once();
        assert_eq!(N_SET.load(Ordering::SeqCst) - s0, 2);
        assert_eq!(device_state(), before);
        teardown();
    }

    /// 1 つの断る経路を通し、(描いた回数の差, Set の回数の差, Get の回数の差, 前の状態, 後の状態) を返す。
    fn refuse_with(prep: impl FnOnce()) -> (u32, u32, u32, Vec<u32>, Vec<u32>) {
        let before = device_state();
        prep();
        let (d0, s0, g0) = (
            N_DRAW.load(Ordering::SeqCst),
            N_SET.load(Ordering::SeqCst),
            N_GET.load(Ordering::SeqCst),
        );
        draw_once();
        (
            N_DRAW.load(Ordering::SeqCst) - d0,
            N_SET.load(Ordering::SeqCst) - s0,
            N_GET.load(Ordering::SeqCst) - g0,
            before,
            device_state(),
        )
    }

    #[test]
    fn lost_device_is_skipped_and_counted() {
        let _g = setup();
        let (d, s, g, before, after) = refuse_with(|| TCL_HR.store(DEVICELOST, Ordering::SeqCst));
        assert_eq!((d, s, g), (0, 0, 0));
        assert_eq!(after, before);
        assert_eq!(counters().lost, 1);
        assert_eq!(
            OVERLAY_ON.load(Ordering::SeqCst),
            1,
            "失われている間は止めずに待つ"
        );
        TCL_HR.store(0, Ordering::SeqCst);
        let d0 = N_DRAW.load(Ordering::SeqCst);
        draw_once();
        assert_eq!(N_DRAW.load(Ordering::SeqCst), d0 + 1, "戻ったのに描かない");
        teardown();
    }

    #[test]
    fn texture_failures_stop_without_drawing() {
        let _g = setup();
        let (d, s, _, before, after) = refuse_with(|| CREATE_HR.store(E_FAIL, Ordering::SeqCst));
        assert_eq!((d, s), (0, 0));
        assert_eq!(after, before);
        let c = counters();
        assert_eq!((c.fail, c.fail_hr), (Fail::CreateTexture as u32, E_FAIL));
        assert_eq!(OVERLAY_ON.load(Ordering::SeqCst), 0, "止めていない");
        let (d, s, g, _, _) = refuse_with(|| {});
        assert_eq!((d, s, g), (0, 0, 0));
        teardown();

        let _g2 = {
            drop(_g);
            setup()
        };
        let r0 = N_OUR_RELEASE.load(Ordering::SeqCst);
        let (d, s, _, before, after) = refuse_with(|| LOCK_HR.store(E_FAIL, Ordering::SeqCst));
        assert_eq!((d, s), (0, 0));
        assert_eq!(after, before);
        assert_eq!(counters().fail, Fail::LockTexture as u32);
        assert_eq!(
            N_OUR_RELEASE.load(Ordering::SeqCst),
            r0 + 1,
            "開けなかったテクスチャを返していない"
        );
        assert_eq!(TEX.load(Ordering::SeqCst), 0);
        teardown();
    }

    #[test]
    fn get_failures_touch_nothing() {
        let _g = setup();
        draw_once();
        let (d, s, _, before, after) = refuse_with(|| GET_RS_HR.store(E_FAIL, Ordering::SeqCst));
        assert_eq!((d, s), (0, 0));
        assert_eq!(after, before);
        assert_eq!(counters().fail, Fail::GetState as u32);
        teardown();

        let _g2 = {
            drop(_g);
            setup()
        };
        draw_once();
        let (d, s, _, before, after) = refuse_with(|| GET_SS_HR.store(E_FAIL, Ordering::SeqCst));
        assert_eq!((d, s), (0, 0));
        assert_eq!(
            after, before,
            "テクスチャの参照を返していない（参照の数が違う）"
        );
        teardown();
    }

    #[test]
    fn off_device_change_draw_failure_and_panic() {
        let _g = setup();
        set_off();
        let (d, s, g, _, _) = refuse_with(|| {});
        assert_eq!((d, s, g), (0, 0, 0), "無効なのに D3D8 を呼んだ");
        set_on();
        draw_once();
        let before = device_state();
        DRAW_HR.store(E_FAIL, Ordering::SeqCst);
        draw_once();
        DRAW_HR.store(0, Ordering::SeqCst);
        assert_eq!(device_state(), before);
        assert_eq!(counters().draw_failed, 1);
        FAKE_DEVICE_PTR.store(addr_of(&FAKE_DEVICE2), Ordering::SeqCst);
        let (d, s, _, _, _) = refuse_with(|| {});
        assert_eq!((d, s), (0, 0));
        assert_eq!(counters().fail, Fail::DeviceChanged as u32);
        teardown();

        let _g2 = {
            drop(_g);
            setup()
        };
        PANIC_NEXT.store(true, Ordering::SeqCst);
        draw_once();
        let c = counters();
        assert_eq!((c.panics, c.fail), (1, Fail::Panic as u32));
        assert_eq!(OVERLAY_ON.load(Ordering::SeqCst), 0);
        teardown();
    }

    #[test]
    fn d3d8_numbers_match_the_game_and_the_sdk_order() {
        assert_eq!(
            [
                DEV_CREATETEXTURE,
                DEV_SETRENDERSTATE,
                DEV_SETTEXTURE,
                DEV_SETTEXTURESTAGESTATE,
                DEV_DRAWPRIMITIVEUP,
                DEV_SETVERTEXSHADER,
                DEV_SETSTREAMSOURCE,
            ],
            [0x50, 0xC8, 0xF4, 0xFC, 0x120, 0x130, 0x14C]
        );
        assert_eq!(DEV_GETRENDERSTATE, DEV_SETRENDERSTATE + 4);
        assert_eq!(DEV_GETVERTEXSHADER, DEV_SETVERTEXSHADER + 4);
        assert_eq!(DEV_GETSTREAMSOURCE, DEV_SETSTREAMSOURCE + 4);
        assert_eq!(DEV_GETVIEWPORT, DEV_SETVIEWPORT + 4);
        assert_eq!(DEV_GETTEXTURE + 4, DEV_SETTEXTURE);
        assert_eq!(DEV_GETTEXTURESTAGESTATE + 4, DEV_SETTEXTURESTAGESTATE);
        assert_eq!(DEV_TESTCOOPERATIVELEVEL, 3 * 4);
        assert_eq!(
            (TEX_LOCKRECT, TEX_UNLOCKRECT, COM_RELEASE),
            (0x40, 0x44, 0x08)
        );
        assert_eq!(D3DRS_FOGENABLE, 0x1C);
        let rs = |st: u32| RS.iter().find(|r| r.0 == st).map(|r| r.1);
        assert_eq!(rs(D3DRS_ALPHABLENDENABLE), Some(1));
        assert_eq!(rs(D3DRS_SRCBLEND), Some(5));
        assert_eq!(rs(D3DRS_DESTBLEND), Some(6));
        assert_eq!(rs(D3DRS_ZENABLE), Some(0));
        assert_eq!(rs(D3DRS_FOGENABLE), Some(0));
        let tss = |stage: u32, t: u32| TSS.iter().find(|r| r.0 == stage && r.1 == t).map(|r| r.2);
        assert_eq!(tss(0, D3DTSS_ALPHAOP), Some(4));
        assert_eq!(tss(0, D3DTSS_ALPHAARG1), Some(2));
        assert_eq!(tss(0, D3DTSS_MINFILTER), Some(1));
        assert_eq!(tss(1, D3DTSS_COLOROP), Some(1));
        assert_eq!((D3DPOOL_MANAGED, D3DPT_TRIANGLELIST, FVF), (1, 4, 0x144));
    }

    #[test]
    fn overlay_on_draw_does_not_allocate() {
        let _g = setup();
        for _ in 0..2 {
            let a0 = crate::test_alloc::count();
            overlay_on_draw();
            assert_eq!(crate::test_alloc::count(), a0, "文字の描画の経路で確保した");
        }
        assert_eq!(counters().drawn, 2);
        teardown();
    }

    #[test]
    fn draw_entry_draws_multiple_japanese_lines_with_a_big_atlas() {
        let _g = setup();
        use_big_atlas();
        TEXT.publish("接続コード\n接続しました。\nHi 2P");
        let before = device_state();
        let (bad0, draw0) = (
            BAD_ARGS.load(Ordering::SeqCst),
            N_DRAW.load(Ordering::SeqCst),
        );
        draw_once();
        let (w, h) = (atlas().spec.tex_w, atlas().spec.tex_h);
        println!(
            "大きい文字の表（合成）: {w}×{h}、文字 {} 個",
            atlas().spec.glyphs.len()
        );
        assert!(w.is_power_of_two() && h.is_power_of_two());
        assert!(
            (128..=512).contains(&w) && (128..=512).contains(&h),
            "{w}×{h}"
        );
        assert_eq!(
            [
                CREATE_DIMS[0].load(Ordering::SeqCst),
                CREATE_DIMS[1].load(Ordering::SeqCst)
            ],
            [w, h],
            "CreateTexture の大きさが文字の表と違う"
        );
        assert_eq!(
            BAD_ARGS.load(Ordering::SeqCst),
            bad0,
            "偽の関数に渡した引数が違う"
        );
        assert_eq!(
            N_DRAW.load(Ordering::SeqCst),
            draw0 + 1,
            "DrawPrimitiveUP は 1 回"
        );
        let (w, h) = (w as usize, h as usize);
        assert_texture_copied(&[0, 1, h / 2, h - 1], &[0, 1, w / 2, w - 1]);
        let d: Vec<u32> = AT_DRAW.iter().map(|a| a.load(Ordering::SeqCst)).collect();
        assert_eq!(&d[..3], &[D3DPT_TRIANGLELIST, 38, 28]);
        let v0: Vec<u32> = AT_DRAW_V0
            .iter()
            .map(|a| a.load(Ordering::SeqCst))
            .collect();
        assert_eq!(f32::from_bits(v0[0]), 564.0 - 0.5);
        assert_eq!(f32::from_bits(v0[1]), 408.0 - 0.5);
        assert_eq!(v0[4], STYLE_DEFAULT.back);
        assert_eq!(device_state(), before, "ゲームの状態を戻していない");
        let c = counters();
        assert_eq!((c.drawn, c.fail, c.set_failed), (1, 0, 0));
        teardown();
    }

    #[test]
    fn big_multiline_path_does_not_allocate() {
        let _g = setup();
        use_big_atlas();
        TEXT.publish("接続コード\n接続しました。\nHi 2P");
        for _ in 0..2 {
            let a0 = crate::test_alloc::count();
            overlay_on_draw();
            assert_eq!(crate::test_alloc::count(), a0, "文字の描画の経路で確保した");
        }
        assert_eq!(counters().drawn, 2);
        teardown();
    }

    fn cell_alpha(a: &GdiAtlas, slot: u32) -> Vec<Vec<u32>> {
        let (ox, oy) = a.spec.cell_origin(slot);
        let w = a.spec.slot_cells(slot) * a.spec.cell_w;
        (oy..oy + a.spec.cell_h)
            .map(|y| {
                (ox..ox + w)
                    .map(|x| a.texels[(y * a.spec.tex_w + x) as usize] >> 24)
                    .collect()
            })
            .collect()
    }

    fn ink_of(a: &GdiAtlas, c: char) -> (usize, Option<usize>, Option<usize>) {
        let rows = cell_alpha(a, a.spec.slot_of(c).expect("文字の表にある文字"));
        let cols: Vec<usize> = rows
            .iter()
            .flat_map(|r| {
                r.iter()
                    .enumerate()
                    .filter(|(_, v)| **v >= 64)
                    .map(|(x, _)| x)
            })
            .collect();
        (
            cols.len(),
            cols.iter().min().copied(),
            cols.iter().max().copied(),
        )
    }

    fn assert_japanese_ink(a: &GdiAtlas) {
        let d = &a.describe();
        let cw = a.spec.cell_w as usize;
        for c in ['接', 'あ'] {
            let (n, _, _) = ink_of(a, c);
            assert!(n > 5, "{c} の濃い画素が {n} 個しかない: {d}");
        }
        for c in ['接', '続', 'あ', 'ホ'] {
            let (_, min_x, max_x) = ink_of(a, c);
            let (min_x, max_x) = (min_x.unwrap_or(0), max_x.unwrap_or(0));
            let need = if "接続".contains(c) {
                2 * cw - 3
            } else {
                cw + 2
            };
            assert!(
                max_x >= need,
                "全角の {c} が右の枠まで届かない（右端の列 {max_x}、必要 {need}、枠の幅 {}）: {d}",
                2 * cw
            );
            assert!(
                min_x <= 3,
                "全角の {c} が左の枠から始まらない（左端の列 {min_x}）: {d}"
            );
        }
        for c in ['A', 'g', '2', 'P'] {
            let (n, _, max_x) = ink_of(a, c);
            assert!(n > 3, "{c} の濃い画素が {n} 個しかない: {d}");
            assert!(max_x.unwrap_or(0) < cw, "{c} が半角の枠をはみ出す: {d}");
        }
        assert!(
            cell_alpha(a, a.spec.slot_of(' ').unwrap())
                .iter()
                .flatten()
                .all(|&v| v == 0)
        );
        assert!(
            cell_alpha(a, a.spec.solid_slot())
                .iter()
                .flatten()
                .all(|&v| v == 255)
        );
    }

    #[test]
    fn gdi_atlas_draws_japanese() {
        let a = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        println!("文字の表: {}", a.describe());
        assert!(
            !a.face.is_empty(),
            "字体の名前を読めていない: {}",
            a.describe()
        );
        assert_eq!((a.spec.cell_w, a.spec.cell_h), (7, 14), "{}", a.describe());
        let (w, h) = (a.spec.tex_w, a.spec.tex_h);
        assert!(w.is_power_of_two() && h.is_power_of_two() && w <= 1024 && h <= 1024);
        assert_eq!(a.texels.len(), a.spec.texel_count());
        assert_japanese_ink(&a);
    }

    #[test]
    fn gdi_atlas_puts_the_choice_arrows_in_the_middle_of_their_slots() {
        let a = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        for c in ['◀', '▶'] {
            let slot = a.spec.slot_of(c).expect("矢印が文字の表にある");
            let rows = cell_alpha(&a, slot);
            let w = rows[0].len();
            let on = |x: usize| rows.iter().any(|r| r[x] != 0);
            let l = (0..w).find(|&x| on(x)).expect("墨がある");
            let r = (0..w).rev().find(|&x| on(x)).unwrap();
            println!(
                "{c}: 枠 {} 枠ぶん・幅 {w}・墨 {l}..={r}",
                a.spec.slot_cells(slot)
            );
            assert!(
                l.abs_diff(w - 1 - r) <= 1,
                "{c} の左の空き {l} と右の空き {}",
                w - 1 - r
            );
        }
    }

    #[test]
    fn gdi_atlas_draws_the_small_glyphs_only_in_their_corner() {
        let a = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        let (sw, sh) = (ov::SMALL_CELL_W as usize, ov::SMALL_CELL_H as usize);
        let mut shapes = std::collections::HashSet::new();
        for c in ASCII.iter().copied().filter(|&c| c != ' ') {
            let key = ov::small_key(c).expect("小さい文字の鍵");
            let slot = a.spec.slot_of(key).expect("鍵が文字の表にある");
            let rows = cell_alpha(&a, slot);
            let dark = |x: usize, y: usize| rows[y][x] >= 64;
            let ink: usize = (0..rows.len())
                .flat_map(|y| (0..rows[y].len()).map(move |x| (x, y)))
                .filter(|&(x, y)| dark(x, y))
                .count();
            assert!(ink >= 2, "{c:?} の小さい文字に墨が無い（{}）", a.describe());
            let outside: u32 = (0..rows.len())
                .flat_map(|y| (0..rows[y].len()).map(move |x| (x, y)))
                .filter(|&(x, y)| x >= sw || y >= sh)
                .map(|(x, y)| rows[y][x])
                .sum();
            assert_eq!(outside, 0, "{c:?} の小さい文字が {sw}×{sh} の外へはみ出す");
            let shape: Vec<bool> = (0..sh)
                .flat_map(|y| (0..sw).map(move |x| (x, y)))
                .map(|(x, y)| dark(x, y))
                .collect();
            shapes.insert(shape);
        }
        assert!(shapes.len() >= 80, "形が {} 通りしかない", shapes.len());
        let (n, l, r) = ink_of(&a, 'A');
        assert!(
            n >= 10 && l.is_some() && r.is_some_and(|r| r + 1 >= sw),
            "{n} {l:?} {r:?}"
        );
        assert_japanese_ink(&a);
    }

    #[test]
    fn writes_the_watch_line_preview_png() {
        use crate::net_ui::{Board, ROW_HUD, SHOW_HUD, draw_board};
        use higanr_core::net_menu::{PeerStats, WatchHudView, watch_hud_line};
        use higanr_core::ui_layout::RowStyle;
        let a = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        let (sw, sh) = (640u32, 480u32);
        let dir = std::env::temp_dir().join("higanrtool_overlay_preview");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let stamp = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let cases = [
            (Some(PeerStats::from_record(14, 1, 2)), 3, false),
            (Some(PeerStats::from_record(999, 99, 99)), 99, true),
            (None, 3, false),
        ];
        let band = 15u32;
        let mut sheet = vec![40u8; (sw * band * cases.len() as u32 * 3) as usize];
        for (i, (stats, delay_frames, fast_forward)) in cases.iter().enumerate() {
            let board: &'static Board = Box::leak(Box::new(Board::new()));
            let mut line = higanr_core::net_menu::LineBuf::new();
            watch_hud_line(
                &mut line,
                higanr_core::lang::Lang::Ja,
                &WatchHudView {
                    stats: *stats,
                    delay_frames: *delay_frames,
                    fast_forward: *fast_forward,
                },
            );
            board.set_row(ROW_HUD, line.as_str(), RowStyle::Small);
            board.set_show(SHOW_HUD);
            let dy = -(sh as i32 - band as i32) + (i as u32 * band) as i32;
            let mut drawn_any = false;
            draw_board(board, &a.spec, sw, sh, &mut |v| {
                let n = ov::blit_quads(
                    v,
                    &a.texels,
                    a.spec.tex_w,
                    a.spec.tex_h,
                    &mut sheet,
                    sw,
                    band * cases.len() as u32,
                    0,
                    dy,
                );
                drawn_any |= n > 0;
            });
            assert!(drawn_any, "{} が描かれない", line.as_str());
        }
        for y in 0..band * cases.len() as u32 {
            let i = ((y * sw + higanr_core::ui_layout::FPS_LEFT) * 3) as usize;
            sheet[i..i + 3].copy_from_slice(&[255, 0, 0]);
        }
        let png_bytes =
            higanr_core::shot::encode_png(sw, band * cases.len() as u32, &sheet).expect("PNG");
        let x4 = ov::upscale_rgb(&sheet, sw, band * cases.len() as u32, 4);
        let big =
            higanr_core::shot::encode_png(sw * 4, band * cases.len() as u32 * 4, &x4).expect("PNG");
        let path = dir.join(format!("watch_line_{stamp}.png"));
        let path4 = dir.join(format!("watch_line_x4_{stamp}.png"));
        assert!(
            !path.exists() && !path4.exists(),
            "既存のファイルを上書きしない"
        );
        std::fs::write(&path, png_bytes).expect("PNG を書く");
        std::fs::write(&path4, big).expect("PNG を書く");
        println!("PNG: {}", path4.display());
    }

    #[test]
    fn same_face_knows_aliases() {
        assert!(same_face("ＭＳ ゴシック", "MS Gothic"));
        assert!(same_face("MS Gothic", "ＭＳ ゴシック"));
        assert!(same_face("Yu Gothic UI", "yu gothic ui"));
        assert!(!same_face("Meiryo UI", "MS Gothic"));
        assert!(!same_face("MS Gothic", "MS UI Gothic"));
        assert!(!same_face("Yu Gothic UI", ""));
    }

    #[test]
    fn gdi_atlas_checks_the_chosen_face_and_the_order() {
        let bogus = "HiganRTool No Such Font 0123";
        let only_bogus = gdi_atlas(&[bogus], FONT_HEIGHT, 1).expect("代わりの字体で作る");
        assert!(
            !only_bogus.matched,
            "無い字体を頼んだ字体としている: {}",
            only_bogus.describe()
        );
        assert!(!only_bogus.face.is_empty());
        assert!(
            only_bogus.describe().contains("代わりに"),
            "{}",
            only_bogus.describe()
        );
        let plain = gdi_atlas(&FONT_FACES, FONT_HEIGHT, 1).expect("文字の表");
        let mut faces = vec![bogus];
        faces.extend(FONT_FACES);
        let with_bogus = gdi_atlas(&faces, FONT_HEIGHT, 1).expect("文字の表");
        assert_eq!(
            (plain.matched, &plain.face),
            (with_bogus.matched, &with_bogus.face)
        );
        assert!(gdi_atlas(&FONT_FACES, 0, 1).is_err());
        assert!(gdi_atlas(&FONT_FACES, FONT_HEIGHT, 0).is_err());
        assert!(gdi_atlas(&FONT_FACES, FONT_HEIGHT, 5).is_err());
        assert!(gdi_atlas(&[], FONT_HEIGHT, 1).is_err());
    }

    #[test]
    fn writes_japanese_preview_pngs() {
        const W: u32 = 640;
        const BLOCK_H: u32 = 100;
        let variants: [(&str, &str, u32); 6] = [
            ("MS Gothic 1x", FONT_FACES[0], 1),
            ("MS Gothic 2x", FONT_FACES[0], 2),
            ("Yu Gothic UI 1x", "Yu Gothic UI", 1),
            ("Yu Gothic UI 2x", "Yu Gothic UI", 2),
            ("Meiryo UI 1x", "Meiryo UI", 1),
            ("Meiryo UI 2x", "Meiryo UI", 2),
        ];
        let h = BLOCK_H * variants.len() as u32;
        let mut sheet: Vec<u8> = (0..W * h)
            .flat_map(|i| {
                let (x, y) = (i % W, i / W);
                [
                    (40 + x * 180 / W) as u8,
                    (60 + (y % BLOCK_H) * 120 / BLOCK_H) as u8,
                    110,
                ]
            })
            .collect();
        let bg = sheet.clone();
        for (i, (label, face, scale)) in variants.iter().enumerate() {
            let a = gdi_atlas(&[face], FONT_HEIGHT, *scale).expect("GDI の文字の表");
            println!("見本 {label}: {}", a.describe());
            if a.matched {
                assert_japanese_ink(&a);
            } else {
                println!("  （この PC に {face} が無く、代わりの字体なので、濃さの条件は見ない）");
            }
            let dy = (i as u32 * BLOCK_H) as i32;
            let mut v = [Vertex::default(); MAX_VERTS_ALL];
            let tag = if a.matched { "" } else { " (fallback)" };
            let head = format!("{label}{tag}");
            let place = ov::Place {
                right: 2,
                bottom: BLOCK_H - 2 - 18,
            };
            let n = ov::layout_line(&head, &a.spec, W, BLOCK_H, place, &STYLE_DEFAULT, &mut v);
            let drawn = ov::blit_quads(
                &v[..n],
                &a.texels,
                a.spec.tex_w,
                a.spec.tex_h,
                &mut sheet,
                W,
                h,
                0,
                dy,
            );
            assert_eq!(drawn, n / 6, "{label} の見出しの四角形が絵に出ていない");
            let body = ov::Place {
                right: 2,
                bottom: 2,
            };
            let n = ov::layout_lines(
                ov::PREVIEW_TEXT,
                &a.spec,
                W,
                BLOCK_H,
                body,
                &STYLE_DEFAULT,
                &mut v,
            );
            assert_eq!(n / 6, 58, "{label}: 四角形の数");
            let drawn = ov::blit_quads(
                &v[..n],
                &a.texels,
                a.spec.tex_w,
                a.spec.tex_h,
                &mut sheet,
                W,
                h,
                0,
                dy,
            );
            assert_eq!(drawn, 58, "{label} の本文の四角形が絵に出ていない");
            let block =
                &sheet[(i as u32 * BLOCK_H * W * 3) as usize..][..(BLOCK_H * W * 3) as usize];
            let white = block
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| p.iter().all(|&c| c >= 200))
                .count();
            assert!(white >= 60, "{label}: 文字の白い画素が {white} 個しかない");
        }
        assert!(sheet != bg, "絵が背景のまま");

        let dir = std::env::temp_dir().join("higanrtool_overlay_preview");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let stamp = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let write = |kind: &str, w: u32, h: u32, rgb: &[u8]| {
            let png_bytes = higanr_core::shot::encode_png(w, h, rgb).expect("PNG に符号化");
            let path = dir.join(format!("jp_{kind}_{stamp}.png"));
            assert!(!path.exists(), "既存のファイルを上書きしない");
            std::fs::write(&path, &png_bytes).expect("PNG を書く");
            let dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
            let mut r = dec.read_info().expect("PNG を読み戻す");
            let mut buf = vec![0; r.output_buffer_size().unwrap_or(0)];
            let info = r.next_frame(&mut buf).expect("PNG の画素");
            assert_eq!((info.width, info.height), (w, h));
            println!("PNG: {}", path.display());
        };
        write("lines_x1", W, h, &sheet);
        write("lines_x3", W * 3, h * 3, &ov::upscale_rgb(&sheet, W, h, 3));
        let a = gdi_atlas(&FONT_FACES, FONT_HEIGHT, FONT_SCALE).expect("GDI の文字の表");
        println!("文字の表（既定）: {}", a.describe());
        let atlas_img = ov::atlas_preview_rgb(&a.spec, &a.texels);
        write(
            "atlas_x3",
            a.spec.tex_w * 3,
            a.spec.tex_h * 3,
            &ov::upscale_rgb(&atlas_img, a.spec.tex_w, a.spec.tex_h, 3),
        );
    }

    #[test]
    fn place_and_prepare_refuse_bad_inputs() {
        let _g = setup();
        let (spec, mut texels) = synthetic_atlas();
        texels.pop();
        assert!(place_atlas(spec.clone(), texels).is_err());
        let (spec, texels) = synthetic_atlas();
        assert!(place_atlas(spec, texels).is_err(), "2 回目を通した");
        set_off();
        assert!(prepare(0, addr_of(&FAKE_PP)).is_err());
        assert!(prepare(addr_of(&FAKE_DEVICE_PTR), 0x10).is_err());
        set_on();
        assert!(
            prepare(addr_of(&FAKE_DEVICE_PTR), addr_of(&FAKE_PP)).is_err(),
            "有効なのに置き直した"
        );
        teardown();
    }

    fn glyph_px(x: u32, y: u32) -> u32 {
        0xFFAA_0000 | y << 8 | x
    }

    fn scratch_slot(index: usize) -> Vec<u32> {
        let p = SCRATCH.load(Ordering::SeqCst);
        assert_ne!(p, 0, "作業場が確保されていない");
        // SAFETY: `ensure_scratch` の `SCRATCH_WORDS` 語（index < GAME_GLYPHS）。検査は錠を持つ間だけ動く。
        unsafe {
            core::slice::from_raw_parts((p as *const u32).add(index * SLOT_WORDS), SLOT_WORDS)
        }
        .to_vec()
    }

    fn index_of(c: char) -> usize {
        ov::game_index(c).unwrap()
    }

    fn assert_captured(c: char, meta: [u32; 4], want: impl Fn(u32, u32) -> u32) {
        let s = scratch_slot(index_of(c));
        assert_eq!(&s[SLOT_PX..], &meta, "{c} の大きさ");
        for row in 0..meta[1] {
            for col in 0..meta[0] {
                assert_eq!(
                    s[(row * GAME_SLOT_W + col) as usize],
                    want(col, row),
                    "{c} の画素 ({col}, {row})"
                );
            }
        }
        assert_eq!(s[meta[0] as usize], 0, "{c} の枠の右を触った");
        assert_eq!(
            s[(meta[1] * GAME_SLOT_W) as usize],
            0,
            "{c} の枠の下を触った"
        );
    }

    fn try_capture() -> (u32, GameFontWhy) {
        GF_FRAMES.store(0, Ordering::SeqCst);
        game_font_step(addr_of(&OUR_TEX));
        (
            GF_STATE.load(Ordering::SeqCst),
            GameFontWhy::from_code(GF_WHY.load(Ordering::SeqCst)).unwrap(),
        )
    }

    #[test]
    fn game_font_is_captured_once_applied_and_the_texture_is_rebuilt() {
        let _g = setup();
        use_game_atlas();
        game_world();
        let game0 = game_memory_snapshot();
        let dev0 = device_state();
        let (bad0, alloc0, create0, release0) = (
            BAD_ARGS.load(Ordering::SeqCst),
            crate::test_alloc::count(),
            N_CREATE.load(Ordering::SeqCst),
            N_OUR_RELEASE.load(Ordering::SeqCst),
        );
        overlay_on_draw();
        assert_eq!(
            crate::test_alloc::count(),
            alloc0,
            "ゲームの文字をコピーする経路で確保した"
        );
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_CAPTURED);
        assert_eq!(
            (
                N_GLD.load(Ordering::SeqCst),
                N_GAME_LOCK.load(Ordering::SeqCst),
                N_GAME_UNLOCK.load(Ordering::SeqCst)
            ),
            (1, 1, 1),
            "GetLevelDesc・LockRect・UnlockRect は 1 回ずつ"
        );
        assert_eq!(BAD_ARGS.load(Ordering::SeqCst), bad0, "偽の関数への引数");
        assert_eq!(game_memory_snapshot(), game0, "ゲームの物に書いた");
        assert_eq!(device_state(), dev0, "ゲームの状態を戻していない");
        let info: Vec<u32> = GF_INFO.iter().map(|a| a.load(Ordering::SeqCst)).collect();
        assert_eq!(info, [D3DFMT_A8R8G8B8, 128, 128, 93, 7, 9, 7, 9]);
        for c in ['!', 'A', 'a', '~', '_'] {
            let k = c as u32 - 0x21;
            let (x0, y0) = ((k % 16) * 8, (k / 16) * 10);
            assert_captured(c, [7, 9, 7, 9], |col, row| glyph_px(x0 + col, y0 + row));
        }
        assert!(
            scratch_slot(index_of('!'))[..SLOT_PX]
                .iter()
                .any(|&p| p != 0),
            "最初の文字が空"
        );
        assert!(scratch_slot(0x60 - 0x21).iter().all(|&p| p == 0));
        for _ in 0..70 {
            overlay_on_draw();
        }
        assert_eq!(
            (
                N_GLD.load(Ordering::SeqCst),
                N_GAME_LOCK.load(Ordering::SeqCst)
            ),
            (1, 1),
            "コピーした後もゲームのテクスチャを開いている"
        );

        let old_at = ATLAS.load(Ordering::SeqCst);
        // SAFETY: `use_game_atlas` が置いた、返さない文字の表。
        let old: &Atlas = unsafe { &*(old_at as *const Atlas) };
        apply_captured_game_font(&UpdaterCfg::default());
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_APPLIED);
        assert_ne!(
            ATLAS.load(Ordering::SeqCst),
            old_at,
            "文字の表を差し替えていない"
        );
        let new = atlas();
        assert!(new.spec.game.is_some() && old.spec.game.is_none());
        let g = new.spec.game_glyph('A').expect("A はゲームの文字");
        assert_eq!((g.w, g.h), (7, 9));
        let k = 'A' as u32 - 0x21;
        let (x0, y0) = ((k % 16) * 8, (k / 16) * 10);
        for (col, row) in [(0, 0), (6, 8), (3, 4)] {
            assert_eq!(
                new.texels[((g.y + row) * new.spec.tex_w + g.x + col) as usize],
                glyph_px(x0 + col, y0 + row),
                "文字の表の A の ({col}, {row})"
            );
        }
        assert!(new.spec.game_glyph('`').is_none() && new.spec.game_glyph(' ').is_none());
        let block_y = new.spec.game_block.unwrap().y;
        let grid = (block_y * new.spec.tex_w) as usize;
        assert_eq!(
            new.texels[..grid],
            old.texels[..grid],
            "GDI の文字の枠を潰した"
        );

        TEXT.publish("AB");
        let draw0 = N_DRAW.load(Ordering::SeqCst);
        overlay_on_draw();
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst),
            create0 + 2,
            "作り直しは 1 回"
        );
        assert_eq!(N_OUR_RELEASE.load(Ordering::SeqCst), release0 + 1);
        assert_eq!(
            TEX_ATLAS.load(Ordering::SeqCst),
            ATLAS.load(Ordering::SeqCst)
        );
        assert_eq!(
            N_DRAW.load(Ordering::SeqCst),
            draw0 + 1,
            "DrawPrimitiveUP は 1 回"
        );
        assert_eq!(AT_DRAW[1].load(Ordering::SeqCst), 6);
        let (ww, hh) = (new.spec.tex_w as usize, new.spec.tex_h as usize);
        assert_texture_copied(
            &[0, g.y as usize, g.y as usize + 8, hh - 1],
            &[0, g.x as usize, g.x as usize + 6, ww - 1],
        );
        for _ in 0..5 {
            overlay_on_draw();
        }
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst),
            create0 + 2,
            "何度も作り直した"
        );
        let c = counters();
        assert_eq!((c.fail, c.set_failed, c.panics), (0, 0, 0));
        assert_eq!(BAD_ARGS.load(Ordering::SeqCst), bad0);
        assert_eq!(game_memory_snapshot(), game0, "ゲームの物に書いた");
        assert_eq!(device_state(), dev0, "ゲームの状態を戻していない");
        teardown();
    }

    #[test]
    fn game_font_is_tried_every_30_frames_until_the_game_has_loaded_it() {
        let _g = setup();
        use_game_atlas();
        game_world();
        FAKE_L[1].store(0, Ordering::SeqCst); // ファイルの中身が無い = 読み込み前
        for _ in 0..31 {
            overlay_on_draw();
        }
        assert_eq!(
            GF_ATTEMPTS.load(Ordering::SeqCst),
            2,
            "0 番目と 30 番目のフレーム"
        );
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_TRYING);
        assert_eq!(GF_WHY.load(Ordering::SeqCst), GameFontWhy::NotLoaded as u32);
        assert_eq!(
            N_GLD.load(Ordering::SeqCst),
            0,
            "読み込み前にゲームのテクスチャを触った"
        );
        FAKE_L[1].store(0x1234, Ordering::SeqCst);
        for _ in 31..60 {
            overlay_on_draw();
            assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_TRYING);
        }
        overlay_on_draw();
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_CAPTURED);
        assert_eq!(GF_ATTEMPTS.load(Ordering::SeqCst), 3);
        teardown();
    }

    #[test]
    fn game_font_waits_when_the_game_is_not_ready() {
        let _g = setup();
        use_game_atlas();
        game_world();
        let scratch0 = (0..GAME_GLYPHS).flat_map(scratch_slot).collect::<Vec<_>>();
        let l = addr_of(&FAKE_L);
        let sprite = |c: char| (c as u32 - 0x20) as usize * SPRITE_WORDS;
        type Case = (&'static str, Box<dyn Fn()>, GameFontWhy);
        let cases: Vec<Case> = vec![
            (
                "読み込み口のポインタが 0",
                Box::new(|| FAKE_L_PTR.store(0, Ordering::SeqCst)),
                GameFontWhy::NoLoadedPtr,
            ),
            (
                "読み込み口のポインタの置き場が読めない",
                Box::new(|| GF_LOADED_PTR.store(0x10, Ordering::SeqCst)),
                GameFontWhy::NoLoadedPtr,
            ),
            (
                "AnmManager が 0",
                Box::new(|| FAKE_MGR_PTR.store(0, Ordering::SeqCst)),
                GameFontWhy::NoAnmManager,
            ),
            (
                "読み込み口が窓 1 の位置と 4 バイト違う",
                Box::new(move || FAKE_MGR_PTR.store(l - 0x140, Ordering::SeqCst)),
                GameFontWhy::PtrMismatch,
            ),
            (
                "ファイルの中身が無い",
                Box::new(|| FAKE_L[1].store(0, Ordering::SeqCst)),
                GameFontWhy::NotLoaded,
            ),
            (
                "読み込みの進みが 0 でない",
                Box::new(|| FAKE_L[6].store(1, Ordering::SeqCst)),
                GameFontWhy::NotLoaded,
            ),
            (
                "名前が違う",
                Box::new(|| FAKE_L[9].store(u32::from_le_bytes(*b"i.ag"), Ordering::SeqCst)),
                GameFontWhy::NameMismatch,
            ),
            (
                "スプライトの配列が 0",
                Box::new(|| FAKE_L[3].store(0, Ordering::SeqCst)),
                GameFontWhy::SpriteUnreadable,
            ),
            (
                "スプライトの配列が読めない",
                Box::new(|| FAKE_L[3].store(0x10, Ordering::SeqCst)),
                GameFontWhy::SpriteUnreadable,
            ),
            (
                "スプライトのテクスチャが 0",
                Box::new(move || FAKE_SPRITES[sprite('M') + 1].store(0, Ordering::SeqCst)),
                GameFontWhy::SpriteBad,
            ),
            (
                "矩形が整数でない",
                Box::new(move || {
                    FAKE_SPRITES[sprite('M') + 2].store(0.5f32.to_bits(), Ordering::SeqCst)
                }),
                GameFontWhy::SpriteBad,
            ),
            (
                "矩形が空",
                Box::new(move || {
                    let x0 = FAKE_SPRITES[sprite('M') + 2].load(Ordering::SeqCst);
                    FAKE_SPRITES[sprite('M') + 4].store(x0, Ordering::SeqCst)
                }),
                GameFontWhy::SpriteBad,
            ),
            (
                "矩形がテクスチャの外",
                Box::new(move || {
                    FAKE_SPRITES[sprite('M') + 4].store(200.0f32.to_bits(), Ordering::SeqCst)
                }),
                GameFontWhy::SpriteBad,
            ),
            (
                "文字によってテクスチャの大きさが違う",
                Box::new(move || {
                    FAKE_SPRITES[sprite('Z') + 7].store(256.0f32.to_bits(), Ordering::SeqCst)
                }),
                GameFontWhy::TextureDiffers,
            ),
            (
                "文字によってテクスチャが違う",
                Box::new(move || {
                    FAKE_SPRITES[sprite('Z') + 1].store(addr_of(&GAME_TEX), Ordering::SeqCst)
                }),
                GameFontWhy::TextureDiffers,
            ),
            (
                "テクスチャの vtable が自分のテクスチャと違う",
                Box::new(|| GAME_GLYPH_TEX.0.store(addr_of(&REF_VTBL), Ordering::SeqCst)),
                GameFontWhy::VtableMismatch,
            ),
            (
                "テクスチャのポインタが読めないアドレス",
                Box::new(|| {
                    for c in 0x21u32..=0x7E {
                        FAKE_SPRITES[(c - 0x20) as usize * SPRITE_WORDS + 1]
                            .store(0x10, Ordering::SeqCst);
                    }
                }),
                GameFontWhy::VtableMismatch,
            ),
        ];
        for (label, mutate, want) in cases {
            game_world();
            mutate();
            let bad0 = BAD_ARGS.load(Ordering::SeqCst);
            let game0 = game_memory_snapshot();
            let a0 = crate::test_alloc::count();
            let got = try_capture();
            assert_eq!(
                crate::test_alloc::count(),
                a0,
                "{label}: 確かめの経路で確保した"
            );
            assert_eq!(got, (GF_TRYING, want), "{label}");
            assert_eq!(
                (
                    N_GLD.load(Ordering::SeqCst),
                    N_GAME_LOCK.load(Ordering::SeqCst)
                ),
                (0, 0),
                "{label}: 通さないのにゲームのテクスチャの関数を呼んだ"
            );
            assert_eq!(BAD_ARGS.load(Ordering::SeqCst), bad0, "{label}");
            let now: Vec<u32> = (0..GAME_GLYPHS).flat_map(scratch_slot).collect();
            assert_eq!(now, scratch0, "{label}: 作業場を触った");
            assert_eq!(game_memory_snapshot(), game0, "{label}: ゲームの物に書いた");
        }
        // 対照: 何も変えなければコピーする。
        game_world();
        assert_eq!(try_capture(), (GF_CAPTURED, GameFontWhy::None));
        teardown();
    }

    #[test]
    fn game_font_refuses_what_it_cannot_copy() {
        let _g = setup();
        use_game_atlas();
        let counts = || {
            (
                N_GLD.load(Ordering::SeqCst),
                N_GAME_LOCK.load(Ordering::SeqCst),
                N_GAME_UNLOCK.load(Ordering::SeqCst),
            )
        };
        type Case = (&'static str, Box<dyn Fn()>, GameFontWhy, (u32, u32, u32));
        let cases: Vec<Case> = vec![
            (
                "GetLevelDesc の失敗",
                Box::new(|| GLD_HR.store(E_FAIL, Ordering::SeqCst)),
                GameFontWhy::LevelDescFailed,
                (1, 0, 0),
            ),
            (
                "形式が D3DFMT_UNKNOWN",
                Box::new(|| GLYPH_FMT.store(0, Ordering::SeqCst)),
                GameFontWhy::FormatUnsupported,
                (1, 0, 0),
            ),
            (
                "幅がスプライトの欄と違う",
                Box::new(|| GLYPH_W.store(256, Ordering::SeqCst)),
                GameFontWhy::DimsMismatch,
                (1, 0, 0),
            ),
            (
                "高さがスプライトの欄と違う",
                Box::new(|| GLYPH_H.store(64, Ordering::SeqCst)),
                GameFontWhy::DimsMismatch,
                (1, 0, 0),
            ),
            (
                "絵が枠より大きい（幅 17）",
                Box::new(|| set_sprite('A' as u32, [0, 0, 17, 9], (128, 128))),
                GameFontWhy::GlyphTooBig,
                (1, 0, 0),
            ),
            (
                "絵が枠より大きい（高さ 17）",
                Box::new(|| set_sprite('A' as u32, [0, 0, 7, 17], (128, 128))),
                GameFontWhy::GlyphTooBig,
                (1, 0, 0),
            ),
        ];
        for (label, mutate, want, want_counts) in cases {
            game_world();
            reset_game_font_counters();
            mutate();
            assert_eq!(try_capture(), (GF_REFUSED, want), "{label}");
            assert_eq!(counts(), want_counts, "{label}: 関数の呼び出し");
            let attempts = GF_ATTEMPTS.load(Ordering::SeqCst);
            for _ in 0..40 {
                overlay_on_draw();
            }
            assert_eq!(
                GF_ATTEMPTS.load(Ordering::SeqCst),
                attempts,
                "{label}: あきらめた後も試した"
            );
            reset_game_font_state();
        }
        game_world();
        reset_game_font_counters();
        GAME_LOCK_HR.store(E_FAIL, Ordering::SeqCst);
        for n in 1..=4u32 {
            assert_eq!(
                try_capture(),
                (GF_TRYING, GameFontWhy::LockFailed),
                "{n} 回目"
            );
            assert_eq!(counts(), (n, n, 0));
        }
        assert_eq!(
            try_capture(),
            (GF_REFUSED, GameFontWhy::LockFailed),
            "5 回目"
        );
        assert_eq!(counts(), (5, 5, 0), "開けなかったのに閉じた");
        game_world();
        reset_game_font_counters();
        reset_game_font_state();
        GLYPH_FMT.store(D3DFMT_A8R8G8B8, Ordering::SeqCst);
        GLYPH_W.store(256, Ordering::SeqCst);
        GLYPH_H.store(256, Ordering::SeqCst);
        for c in 0x21u32..=0x7E {
            let k = c - 0x21;
            let (x0, y0) = ((k % 16) * 8, (k / 16) * 10);
            set_sprite(c, [x0, y0, x0 + 7, y0 + 9], (256, 256));
        }
        assert_eq!(try_capture(), (GF_REFUSED, GameFontWhy::ExtractFailed));
        assert_eq!(counts(), (1, 1, 1), "開いたら必ず閉じる");
        teardown();
    }

    fn applied_but_texture_not_rebuilt() {
        use_game_atlas();
        game_world();
        TEXT.publish("AB");
        overlay_on_draw();
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_CAPTURED);
        apply_captured_game_font(&UpdaterCfg::default());
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_APPLIED);
        assert_ne!(
            TEX_ATLAS.load(Ordering::SeqCst),
            ATLAS.load(Ordering::SeqCst),
            "差し替え前のテクスチャのはず"
        );
    }

    /// 描いた最初の頂点（下地の左上）の x 座標 + 0.5。文 "AB" の下地は、旧い表（送り 7）なら 620、ゲームの文字をコピーした表（送り 9）なら 616（右端 638 から引く）。
    fn box_left() -> f32 {
        f32::from_bits(AT_DRAW_V0[0].load(Ordering::SeqCst)) + 0.5
    }

    /// 差し替えの後の CreateTexture / LockRect の失敗の 1 ケース。失敗で描画ごと止めない・差し替え前のテクスチャに差し替え後の文字の表の
    /// 頂点を組まない・失敗の直後の毎フレームに CreateTexture を呼ばない・直った後の試し直しで差し替える。
    fn swap_failure_case(fail_lock: bool) {
        let _g = setup();
        applied_but_texture_not_rebuilt();
        let knob = if fail_lock { &LOCK_HR } else { &CREATE_HR };
        let dev0 = device_state();
        let (create0, rel0, draw0, drawn0, bad0) = (
            N_CREATE.load(Ordering::SeqCst),
            N_OUR_RELEASE.load(Ordering::SeqCst),
            N_DRAW.load(Ordering::SeqCst),
            counters().drawn,
            BAD_ARGS.load(Ordering::SeqCst),
        );
        knob.store(E_FAIL, Ordering::SeqCst);
        let alloc0 = crate::test_alloc::count();
        overlay_on_draw();
        assert_eq!(
            crate::test_alloc::count(),
            alloc0,
            "失敗の経路で確保した（ゲームのスレッド）"
        );
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst),
            create0 + 1,
            "作り直しを試す"
        );
        assert_eq!(
            OVERLAY_ON.load(Ordering::SeqCst),
            1,
            "差し替えの失敗で描画ごと止めた"
        );
        let c = counters();
        assert_eq!((c.fail, c.drawn), (0, drawn0 + 1), "止めた／描かなかった");
        assert_eq!(
            N_DRAW.load(Ordering::SeqCst),
            draw0 + 1,
            "差し替え前の文字で描く"
        );
        assert_eq!(box_left(), 620.0, "旧い表の頂点（下地の幅 18）で描く");
        assert_eq!(
            AT_DRAW_TEX.load(Ordering::SeqCst),
            addr_of(&OUR_TEX),
            "差し替え前のテクスチャで描く"
        );
        assert_eq!(
            N_OUR_RELEASE.load(Ordering::SeqCst),
            rel0 + u32::from(fail_lock),
            "返したテクスチャの数"
        );
        assert_ne!(
            TEX_ATLAS.load(Ordering::SeqCst),
            ATLAS.load(Ordering::SeqCst),
            "作れていないのに差し替え済みにした"
        );
        assert_eq!(device_state(), dev0, "ゲームの状態を戻していない");
        assert_eq!(BAD_ARGS.load(Ordering::SeqCst), bad0);
        assert_eq!(SWAP_FAILS.load(Ordering::SeqCst), 1);
        assert_eq!(SWAP_FAIL_HR.load(Ordering::SeqCst), E_FAIL);
        let want_why = if fail_lock {
            Fail::LockTexture
        } else {
            Fail::CreateTexture
        };
        assert_eq!(SWAP_FAIL_WHY.load(Ordering::SeqCst), want_why as u32);

        knob.store(0, Ordering::SeqCst);
        for _ in 1..SWAP_RETRY_EVERY {
            overlay_on_draw();
            assert_eq!(box_left(), 620.0);
        }
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst),
            create0 + 1,
            "失敗の直後の毎フレームに CreateTexture を呼んだ"
        );
        let rel1 = N_OUR_RELEASE.load(Ordering::SeqCst);
        overlay_on_draw();
        assert_eq!(N_CREATE.load(Ordering::SeqCst), create0 + 2);
        assert_eq!(
            TEX_ATLAS.load(Ordering::SeqCst),
            ATLAS.load(Ordering::SeqCst)
        );
        assert_eq!(N_OUR_RELEASE.load(Ordering::SeqCst), rel1 + 1);
        assert_eq!(box_left(), 616.0, "差し替え後の表（送り 9）の頂点");
        assert_eq!(SWAP_TRIES.load(Ordering::SeqCst), 0, "成功で数えを戻す");
        for _ in 0..5 {
            overlay_on_draw();
        }
        assert_eq!(N_CREATE.load(Ordering::SeqCst), create0 + 2);
        let c = counters();
        assert_eq!((c.fail, c.panics, c.drawn), (0, 0, drawn0 + 1 + 59 + 1 + 5));
        let t = game_font_text();
        assert!(t.contains("作り直しに 1 回失敗"), "{t}");
        assert!(t.contains("作り直せた"), "{t}");
        teardown();
    }

    #[test]
    fn swap_create_texture_failure_keeps_drawing_with_the_old_texture() {
        swap_failure_case(false);
    }

    #[test]
    fn swap_lock_failure_keeps_drawing_with_the_old_texture() {
        swap_failure_case(true);
    }

    #[test]
    fn swap_failures_stop_retrying_after_the_limit_and_keep_drawing() {
        let _g = setup();
        applied_but_texture_not_rebuilt();
        let create0 = N_CREATE.load(Ordering::SeqCst);
        let drawn0 = counters().drawn;
        CREATE_HR.store(E_FAIL, Ordering::SeqCst);
        let frames = SWAP_MAX_TRIES * (SWAP_RETRY_EVERY + 1) + 300;
        for _ in 0..frames {
            overlay_on_draw();
            assert_eq!(box_left(), 620.0, "旧い表で描き続ける");
        }
        assert_eq!(
            N_CREATE.load(Ordering::SeqCst) - create0,
            SWAP_MAX_TRIES,
            "試し直しの回数の上限"
        );
        CREATE_HR.store(0, Ordering::SeqCst);
        for _ in 0..(SWAP_RETRY_EVERY * 3) {
            overlay_on_draw();
        }
        assert_eq!(N_CREATE.load(Ordering::SeqCst) - create0, SWAP_MAX_TRIES);
        let c = counters();
        assert_eq!(
            (c.fail, c.drawn, OVERLAY_ON.load(Ordering::SeqCst)),
            (0, drawn0 + frames + SWAP_RETRY_EVERY * 3, 1)
        );
        assert_eq!(SWAP_FAILS.load(Ordering::SeqCst), SWAP_MAX_TRIES);
        let t = game_font_text();
        assert!(t.contains("3 回失敗"), "{t}");
        assert!(
            t.contains("差し替え前の文字（GDI の文字）で描き続けている"),
            "{t}"
        );
        teardown();
    }

    #[test]
    fn first_texture_failure_still_stops() {
        let _g = setup();
        use_game_atlas();
        game_world();
        CREATE_HR.store(E_FAIL, Ordering::SeqCst);
        overlay_on_draw();
        let c = counters();
        assert_eq!(
            (c.fail, c.fail_hr, c.drawn),
            (Fail::CreateTexture as u32, E_FAIL, 0)
        );
        assert_eq!(OVERLAY_ON.load(Ordering::SeqCst), 0);
        teardown();
    }

    #[test]
    fn game_font_gives_up_after_the_attempt_limit() {
        let _g = setup();
        use_game_atlas();
        game_world();
        FAKE_L[1].store(0, Ordering::SeqCst); // 読み込み前
        let last_try_frame = (GF_MAX_ATTEMPTS - 1) * GF_EVERY;
        for _ in 0..last_try_frame {
            overlay_on_draw();
        }
        assert_eq!(GF_ATTEMPTS.load(Ordering::SeqCst), GF_MAX_ATTEMPTS - 1);
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_TRYING);
        FAKE_L[1].store(0x1234, Ordering::SeqCst);
        overlay_on_draw();
        assert_eq!(
            GF_STATE.load(Ordering::SeqCst),
            GF_CAPTURED,
            "上限の最後の 1 回でコピーできない"
        );

        // 対照: 600 回とも読み込み前なら、次の間隔でもう試さない。
        reset_game_font();
        game_world();
        FAKE_L[1].store(0, Ordering::SeqCst);
        for _ in 0..(GF_MAX_ATTEMPTS * GF_EVERY) {
            overlay_on_draw();
        }
        assert_eq!(GF_ATTEMPTS.load(Ordering::SeqCst), GF_MAX_ATTEMPTS);
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_TRYING, "まだ待っている");
        let drawn0 = counters().drawn;
        overlay_on_draw(); // 601 回目の間隔
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_REFUSED);
        assert_eq!(GF_WHY.load(Ordering::SeqCst), GameFontWhy::GaveUp as u32);
        assert_eq!(
            GF_ATTEMPTS.load(Ordering::SeqCst),
            GF_MAX_ATTEMPTS,
            "上限を越えて試した"
        );
        assert_eq!(counters().drawn, drawn0 + 1, "あきらめても文は描く");
        FAKE_L[1].store(0x1234, Ordering::SeqCst);
        let game0 = game_memory_snapshot();
        for _ in 0..(GF_EVERY * 4) {
            overlay_on_draw();
        }
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_REFUSED);
        assert_eq!(GF_ATTEMPTS.load(Ordering::SeqCst), GF_MAX_ATTEMPTS);
        assert_eq!(
            (
                N_GLD.load(Ordering::SeqCst),
                N_GAME_LOCK.load(Ordering::SeqCst)
            ),
            (0, 0)
        );
        assert_eq!(game_memory_snapshot(), game0);
        let c = counters();
        assert_eq!(
            (c.fail, c.panics, OVERLAY_ON.load(Ordering::SeqCst)),
            (0, 0, 1)
        );
        let t = game_font_text();
        assert!(t.contains("試す回数の上限"), "{t}");
        assert!(t.contains("GDI の文字のまま"), "{t}");
        assert!(t.contains(GameFontWhy::NotLoaded.text()), "最後の理由: {t}");
        teardown();
    }

    #[test]
    fn torn_text_is_not_drawn_and_recovers() {
        let _g = setup();
        TEXT.publish("HiganRTool 60.00fps");
        overlay_on_draw();
        let (draw0, torn0, drawn0) = (
            N_DRAW.load(Ordering::SeqCst),
            counters().torn,
            counters().drawn,
        );
        TEXT.mark_writing(true);
        for k in 1..=3 {
            overlay_on_draw();
            assert_eq!(counters().torn, torn0 + k, "書きかけを数えていない");
        }
        assert_eq!(N_DRAW.load(Ordering::SeqCst), draw0, "書きかけの文を描いた");
        assert_eq!(counters().drawn, drawn0);
        assert_eq!(OVERLAY_ON.load(Ordering::SeqCst), 1, "止めてはいけない");
        TEXT.mark_writing(false);
        overlay_on_draw();
        assert_eq!(N_DRAW.load(Ordering::SeqCst), draw0 + 1);
        teardown();
    }

    #[test]
    fn a_full_box_of_japanese_is_drawn_whole() {
        let _g = setup();
        use_big_atlas();
        let line = "接続コード".repeat(8) + "接続";
        assert_eq!(line.chars().count(), 42);
        let text = format!("{line}\n{line}\n{line}");
        assert_eq!(text.encode_utf16().count(), higanr_core::overlay::MAX_TEXT);
        assert!(text.len() > 63 * 5);
        TEXT.publish(&text);
        let (draw0, alloc0) = (N_DRAW.load(Ordering::SeqCst), crate::test_alloc::count());
        overlay_on_draw();
        assert_eq!(
            crate::test_alloc::count(),
            alloc0,
            "入れ物いっぱいの文の経路で確保した"
        );
        assert_eq!(N_DRAW.load(Ordering::SeqCst), draw0 + 1);
        assert_eq!(AT_DRAW[1].load(Ordering::SeqCst), 258);
        let c = counters();
        assert_eq!((c.fail, c.torn, c.drawn), (0, 0, 1));
        assert_eq!(box_left(), 46.0);
        teardown();
    }

    #[test]
    fn font_defaults_are_ms_gothic_at_double_scale() {
        assert_eq!(FONT_SCALE, 2);
        assert_eq!(FONT_FACES[0], "ＭＳ ゴシック");
        assert_eq!(FONT_HEIGHT, 14);
        let a = default_atlas().expect("既定の文字の表");
        assert!(
            a.desc.contains("文字の高さ 14 を 2 倍で描いて縮める"),
            "{}",
            a.desc
        );
        assert_eq!((a.spec.cell_w, a.spec.cell_h), (7, 14));
        assert!(
            a.spec.game_block.is_some(),
            "ゲームの文字の枠を予約していない"
        );
    }

    fn reset_game_font_counters() {
        for a in [
            &N_GLD,
            &N_GAME_LOCK,
            &N_GAME_UNLOCK,
            &GF_LOCK_FAILS,
            &GF_ATTEMPTS,
        ] {
            a.store(0, Ordering::SeqCst);
        }
    }

    fn reset_game_font_state() {
        GF_STATE.store(GF_TRYING, Ordering::SeqCst);
        GF_WHY.store(0, Ordering::SeqCst);
    }

    #[test]
    fn game_font_reads_sixteen_bit_formats() {
        let _g = setup();
        use_game_atlas();
        let e5 = |v: u32| (v << 3) | (v >> 2);
        let e6 = |v: u32| (v << 2) | (v >> 4);
        for fmt in [ov::D3DFMT_A4R4G4B4, higanr_core::shot::D3DFMT_R5G6B5] {
            game_world();
            fill_glyph_pixels(fmt);
            reset_game_font_state();
            assert_eq!(try_capture(), (GF_CAPTURED, GameFontWhy::None), "{fmt}");
            assert_eq!(GF_INFO[0].load(Ordering::SeqCst), fmt);
            let k = 'B' as u32 - 0x21;
            let (x0, y0) = ((k % 16) * 8, (k / 16) * 10);
            assert_captured('B', [7, 9, 7, 9], |col, row| {
                let (x, y) = (x0 + col, y0 + row);
                let (r, g, b) = if fmt == ov::D3DFMT_A4R4G4B4 {
                    (0xA * 17, ((y >> 4) & 15) * 17, ((x >> 4) & 15) * 17)
                } else {
                    (e5(0xAA >> 3), e6((y >> 2) & 63), e5((x >> 3) & 31))
                };
                0xFF00_0000 | (r << 16) | (g << 8) | b
            });
            reset_game_font();
        }
        teardown();
    }

    #[test]
    fn game_font_rescales_to_the_drawn_size() {
        let _g = setup();
        use_game_atlas();
        game_world();
        set_sprite('A' as u32, [0, 0, 8, 10], (128, 128));
        FAKE_SPRITES[sprite_at('A') + 12].store(5.0f32.to_bits(), Ordering::SeqCst);
        FAKE_SPRITES[sprite_at('A') + 13].store(4.0f32.to_bits(), Ordering::SeqCst);
        assert_eq!(try_capture(), (GF_CAPTURED, GameFontWhy::None));
        assert_captured('A', [8, 10, 4, 5], glyph_px);
        apply_captured_game_font(&UpdaterCfg::default());
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_APPLIED);
        let g = atlas().spec.game_glyph('A').unwrap();
        assert_eq!((g.w, g.h), (4, 5));
        teardown();
    }

    fn sprite_at(c: char) -> usize {
        (c as u32 - 0x20) as usize * SPRITE_WORDS
    }

    #[test]
    fn game_font_without_a_reserved_block_keeps_the_gdi_atlas() {
        let _g = setup();
        game_world();
        overlay_on_draw();
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_CAPTURED);
        let at = ATLAS.load(Ordering::SeqCst);
        apply_captured_game_font(&UpdaterCfg::default());
        assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_APPLY_FAILED);
        assert_eq!(ATLAS.load(Ordering::SeqCst), at, "文字の表を差し替えた");
        let text = game_font_text();
        assert!(text.contains("文字の表を作れなかった"), "{text}");
        assert!(text.contains("予約していない"), "{text}");
        let create0 = N_CREATE.load(Ordering::SeqCst);
        overlay_on_draw();
        assert_eq!(N_CREATE.load(Ordering::SeqCst), create0, "作り直した");
        assert_eq!(counters().drawn, 2);
        assert_eq!(counters().fail, 0);
        teardown();
    }

    #[test]
    fn game_font_report_says_what_happened() {
        let _g = setup();
        use_game_atlas();
        game_world();
        FAKE_L[1].store(0, Ordering::SeqCst);
        overlay_on_draw();
        let t = game_font_text();
        assert!(t.starts_with("まだコピーしていない"), "{t}");
        assert!(t.contains("読み込み中") && t.contains("試し 1 回"), "{t}");
        FAKE_L[1].store(0x1234, Ordering::SeqCst);
        GF_FRAMES.store(0, Ordering::SeqCst);
        overlay_on_draw();
        let t = game_font_text();
        assert!(t.starts_with("コピーした（"), "{t}");
        for want in [
            "128×128",
            "A8R8G8B8",
            "93 文字",
            "sprite[0x20]（@）の絵 7×9",
            "画面に描く大きさ 7×9",
        ] {
            assert!(t.contains(want), "{want} が無い: {t}");
        }
        assert!(t.contains("文字の表への反映はこれから"), "{t}");
        apply_captured_game_font(&UpdaterCfg::default());
        let t = game_font_text();
        assert!(t.contains("英数字") && t.contains("送り 9 画素"), "{t}");
        reset_game_font_state();
        GLYPH_FMT.store(0, Ordering::SeqCst);
        GF_STATE.store(GF_TRYING, Ordering::SeqCst);
        game_world();
        GLYPH_FMT.store(0, Ordering::SeqCst);
        assert_eq!(try_capture().0, GF_REFUSED);
        let t = game_font_text();
        assert!(
            t.starts_with("コピーしなかった（コピーできない画素の形式"),
            "{t}"
        );
        assert!(t.contains("GDI の文字のまま"), "{t}");
        teardown();
    }

    #[test]
    fn preview_flag_chooses_the_screen_text() {
        assert_eq!(
            shown_text(true, "HiganRTool 60.00fps"),
            ov::PREVIEW_SCREEN_TEXT
        );
        assert_eq!(
            shown_text(false, "HiganRTool 60.00fps"),
            "HiganRTool 60.00fps"
        );
    }

    fn temp_case(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "higanrtool_gf_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn previews_are_written_next_to_the_log_and_never_into_the_game_folder() {
        let dir = temp_case("prev");
        let game_dir = dir.join("game");
        std::fs::create_dir_all(&game_dir).unwrap();
        let cfg = |preview: bool, exe: &std::path::Path| UpdaterCfg {
            preview,
            quiet: false,
            run_path: Some(dir.join("dll_t.log")),
            exe_dir: Some(exe.to_path_buf()),
        };
        let run = |c: &UpdaterCfg| {
            game_world();
            reset_game_font_state();
            GF_STATE.store(GF_TRYING, Ordering::SeqCst);
            assert_eq!(try_capture(), (GF_CAPTURED, GameFontWhy::None));
            ATLAS.store(
                kept_atlas(&GAME_ATLAS, game_synthetic_atlas),
                Ordering::SeqCst,
            );
            apply_captured_game_font(c);
            assert_eq!(GF_STATE.load(Ordering::SeqCst), GF_APPLIED);
        };
        let _g = setup();
        run(&cfg(false, &game_dir));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "game/ だけ");
        run(&cfg(true, &game_dir));
        let names: Vec<String> = ["_glyphs_lines.png", "_glyphs_atlas.png", "_glyphs.txt"]
            .iter()
            .map(|s| format!("dll_t{s}"))
            .collect();
        for n in &names {
            assert!(dir.join(n).is_file(), "{n} が無い");
        }
        let a = atlas();
        for (n, w, h) in [
            (&names[0], 640 * 3, 320 * 3),
            (&names[1], a.spec.tex_w * 3, a.spec.tex_h * 3),
        ] {
            let dec = png::Decoder::new(std::io::Cursor::new(std::fs::read(dir.join(n)).unwrap()));
            let mut r = dec.read_info().expect("PNG を読み戻す");
            let mut buf = vec![0; r.output_buffer_size().unwrap_or(0)];
            let info = r.next_frame(&mut buf).expect("PNG の画素");
            assert_eq!((info.width, info.height), (w, h), "{n}");
            assert!(buf.iter().any(|&b| b != buf[0]), "{n} が一色");
        }
        let txt = std::fs::read_to_string(dir.join(&names[2])).unwrap();
        assert!(txt.starts_with("コピーした（"), "{txt}");
        let before = std::fs::read(dir.join(&names[0])).unwrap();
        run(&cfg(true, &game_dir));
        assert_eq!(std::fs::read(dir.join(&names[0])).unwrap(), before);
        let note = GF_NOTE.lock().unwrap().clone();
        assert!(
            note.contains("見本のファイル") && note.contains("既にある"),
            "{note}"
        );
        let inside = temp_case("prev_inside");
        let c = UpdaterCfg {
            preview: true,
            quiet: false,
            run_path: Some(inside.join("dll_u.log")),
            exe_dir: Some(inside.clone()),
        };
        run(&c);
        assert_eq!(std::fs::read_dir(&inside).unwrap().count(), 0);
        let note = GF_NOTE.lock().unwrap().clone();
        assert!(note.contains("exe のフォルダの中"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&inside);
        teardown();
    }
}
