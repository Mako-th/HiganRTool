//! 文字の描画の DLL の側のうち、unsafe の要らない部分（pure）: 表示する文の二重のバッファ、1 フレームの上乗せの記録、文の更新スレッドの計算、
//! 設置の順（`session::run` から呼ぶ）と本ログへの集計。D3D8 と GDI を呼ぶ層は `overlay_win.rs`、頂点と文字の表の計算は `higanr_core::overlay`。
//!
//! - 文: 読み取りの側の更新スレッド（1 本）が `TEXT.publish` で書き、ゲームのスレッド（描画の入口）が `TEXT.read` で読む。
//!   読み手は確保もロック待ちもしない（書きかけなら読み直すか、そのフレームは描かない）。文字の単位は UTF-16（`TEXT_UNITS` = `MAX_TEXT` = 128 単位）。
//! - 上乗せの時間: ゲームのスレッドが `TIMINGS.record` で書き、読み取りのスレッドが終わりに `snapshot` で読む。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering, fence};
use std::io::Write;

use higanr_core::overlay::{self as ov, GAME_GLYPHS, MAX_TEXT, Rect, Status, game_index};
use higanr_core::pacing::Dist;

use crate::log_mode::detail_out;
use crate::th09_addr::{
    ANM_LOADED_START, ANM_LOADED_STRIDE, ASCII_ANM_INDEX, ASCII_ANM_NAME, LOADED_FILE, LOADED_NAME,
    LOADED_PROGRESS, LOADED_SPRITES, SPRITE_CHAR_BASE, SPRITE_H, SPRITE_STRIDE, SPRITE_TEX_H,
    SPRITE_TEX_W, SPRITE_TEXTURE, SPRITE_W, SPRITE_X0, SPRITE_X1, SPRITE_Y0, SPRITE_Y1,
};

// ---- 文の二重のバッファ ----

/// バッファ 1 つに入る UTF-16 の単位の数（= `MAX_TEXT`）。日本語（基本面）は 1 文字 1 単位なので 128 文字まで入る。長さ `len` は別の語に持つので、
/// 単位を丸ごと使い切れる。読み手は語を全部コピーした後に札を読み直し、違えば読んだ中身を捨てる（`read_with`）。
pub const TEXT_UNITS: usize = MAX_TEXT;
const BOX_WORDS: usize = TEXT_UNITS / 2;
const _: () = assert!(TEXT_UNITS.is_multiple_of(2) && BOX_WORDS * 2 == TEXT_UNITS);

struct TextSlot {
    /// 札（奇数 = 書いている途中）。seqlock の書き方（tick のリングバッファと同じ考え方）。
    seq: AtomicU32,
    len: AtomicU32,
    /// 単位を 2 つずつ（下位 16 ビットが先の単位）。
    words: [AtomicU32; BOX_WORDS],
}

/// 表示する 1 行の二重のバッファ。書き手は 1 人（更新スレッド）で、最後に書いたバッファの番号を `generation` で知らせる。書き手は「知らせていない方」のバッファに
/// 書くので、読み手が読んでいるバッファとはふつう重ならない。重なったときは札で分かる。
pub struct TextBox {
    generation: AtomicU32,
    slots: [TextSlot; 2],
}

impl TextBox {
    pub const fn new() -> TextBox {
        TextBox {
            generation: AtomicU32::new(0),
            slots: [const {
                TextSlot {
                    seq: AtomicU32::new(0),
                    len: AtomicU32::new(0),
                    words: [const { AtomicU32::new(0) }; BOX_WORDS],
                }
            }; 2],
        }
    }

    /// 書く（書き手は 1 人だけ）。`MAX_TEXT` 単位を越える分は文字の境で落とす（基本面の外の文字を 2 単位の途中で切らない）。
    pub fn publish(&self, text: &str) {
        let mut units = [0u16; TEXT_UNITS];
        let len = ov::encode_utf16_capped(text, &mut units);
        let g = self.generation.load(Ordering::Relaxed).wrapping_add(1);
        let slot = &self.slots[(g & 1) as usize];
        let s = slot.seq.load(Ordering::Relaxed);
        slot.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (w, pair) in slot.words.iter().zip(units.as_chunks::<2>().0) {
            w.store(
                u32::from(pair[0]) | u32::from(pair[1]) << 16,
                Ordering::Relaxed,
            );
        }
        slot.len.store(len as u32, Ordering::Relaxed);
        slot.seq.store(s.wrapping_add(2), Ordering::Release);
        self.generation.store(g, Ordering::Release);
    }

    /// 最後に知らされたバッファを読み、`buf` にコピーして単位の数を返す。書きかけ・壊れた長さなら `None`。確保しない。
    pub fn read(&self, buf: &mut [u16; TEXT_UNITS]) -> Option<usize> {
        self.read_with(buf, || {}, || {})
    }

    /// `read` の中身。`mid` は語の前半をコピーした後・後半をコピーする前に、`between` は全部をコピーした後・札を読み直す前に呼ぶ（検査が「読んでいる間に
    /// 書かれた」を作るため）。本物は何もしない閉包。
    fn read_with(
        &self,
        buf: &mut [u16; TEXT_UNITS],
        mid: impl FnOnce(),
        between: impl FnOnce(),
    ) -> Option<usize> {
        let g = self.generation.load(Ordering::Acquire);
        let slot = &self.slots[(g & 1) as usize];
        let s1 = slot.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            return None;
        }
        let len = slot.len.load(Ordering::Relaxed) as usize;
        let copy = |words: &[AtomicU32], out: &mut [[u16; 2]]| {
            for (w, pair) in words.iter().zip(out) {
                let v = w.load(Ordering::Relaxed);
                *pair = [v as u16, (v >> 16) as u16];
            }
        };
        let (lo, hi) = slot.words.split_at(BOX_WORDS / 2);
        let (out_lo, out_hi) = buf.as_chunks_mut::<2>().0.split_at_mut(BOX_WORDS / 2);
        copy(lo, out_lo);
        mid();
        copy(hi, out_hi);
        between();
        fence(Ordering::Acquire);
        let s2 = slot.seq.load(Ordering::Relaxed);
        if s1 != s2 || len > MAX_TEXT {
            return None;
        }
        Some(len)
    }

    /// 検査用: いま読まれる方のバッファを「書いている途中」にする／戻す（描画の側の検査が、書きかけの文を描かないことを見るため）。
    #[cfg(test)]
    pub(crate) fn mark_writing(&self, writing: bool) {
        let g = self.generation.load(Ordering::SeqCst);
        let seq = &self.slots[(g & 1) as usize].seq;
        let s = seq.load(Ordering::SeqCst);
        if (s & 1 != 0) != writing {
            seq.store(s.wrapping_add(1), Ordering::SeqCst);
        }
    }
}

impl Default for TextBox {
    fn default() -> Self {
        TextBox::new()
    }
}

/// 表示する文（描画の入口が読む。`\n` 区切りで最大 `MAX_LINES` 行）。
pub static TEXT: TextBox = TextBox::new();

/// 接続で決まった Latency（`NO_LATENCY` なら出さない）。ネットワークのスレッドが書き、更新スレッドが 250 ms ごとに読んで文にする。
pub static LATENCY: AtomicU32 = AtomicU32::new(NO_LATENCY);
pub const NO_LATENCY: u32 = u32::MAX;

// ---- 1 フレームの上乗せ ----

/// 上乗せの記録の件数（60 fps で約 68 秒ぶん）。百分位はこの最後の件数から、最大は全件から出す。
pub const TIMING_LEN: usize = 4096;

/// 描画の入口で文字を描くのにかかった時間（QPC のカウント）。書き手はゲームのスレッドだけ。
pub struct Timings {
    n: AtomicU32,
    max: AtomicU32,
    ring: [AtomicU32; TIMING_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingSnapshot {
    pub total: u32,
    pub max: u32,
    /// 最後の `min(total, TIMING_LEN)` 件（順は問わない）。
    pub recent: Vec<i64>,
}

impl Timings {
    pub const fn new() -> Timings {
        Timings {
            n: AtomicU32::new(0),
            max: AtomicU32::new(0),
            ring: [const { AtomicU32::new(0) }; TIMING_LEN],
        }
    }

    pub fn record(&self, ticks: u32) {
        let i = self.n.load(Ordering::Relaxed);
        self.ring[i as usize % TIMING_LEN].store(ticks, Ordering::Relaxed);
        if ticks > self.max.load(Ordering::Relaxed) {
            self.max.store(ticks, Ordering::Relaxed);
        }
        self.n.store(i.wrapping_add(1), Ordering::Release);
    }

    /// コピーする（読み取りのスレッド）。ゲームのスレッドが書いている最中でも、1 件ずつは書き終えた値か前の周の値。
    pub fn snapshot(&self) -> TimingSnapshot {
        let total = self.n.load(Ordering::Acquire);
        let k = (total as usize).min(TIMING_LEN);
        TimingSnapshot {
            total,
            max: self.max.load(Ordering::Relaxed),
            recent: self.ring[..k]
                .iter()
                .map(|a| a.load(Ordering::Relaxed) as i64)
                .collect(),
        }
    }

    #[cfg(test)]
    pub fn reset(&self) {
        self.n.store(0, Ordering::SeqCst);
        self.max.store(0, Ordering::SeqCst);
    }
}

impl Default for Timings {
    fn default() -> Self {
        Timings::new()
    }
}

pub static TIMINGS: Timings = Timings::new();

// ---- 描けなかった理由と数え ----

/// 描くのを止めた理由（最初の 1 つだけ残す。`overlay_win::FAIL`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    None = 0,
    /// 文字の表が置かれていない（設置の順の誤り）。
    NoAtlas = 1,
    /// CreateTexture が失敗した。
    CreateTexture = 2,
    /// テクスチャの LockRect が失敗した・画素の置き場が返らなかった。
    LockTexture = 3,
    /// 状態を読む Get* が失敗した（戻せないので描かない）。
    GetState = 4,
    /// デバイスのアドレスが、テクスチャを作ったときと違う（作り直された。古いテクスチャは使えない）。
    DeviceChanged = 5,
    /// ゲームのスレッドでパニックを受けた。
    Panic = 6,
}

impl Fail {
    pub fn from_code(c: u32) -> Option<Fail> {
        Some(match c {
            0 => Fail::None,
            1 => Fail::NoAtlas,
            2 => Fail::CreateTexture,
            3 => Fail::LockTexture,
            4 => Fail::GetState,
            5 => Fail::DeviceChanged,
            6 => Fail::Panic,
            _ => return None,
        })
    }

    pub fn text(self) -> &'static str {
        match self {
            Fail::None => "なし",
            Fail::NoAtlas => "文字の表が置かれていない",
            Fail::CreateTexture => "CreateTexture が失敗した",
            Fail::LockTexture => "テクスチャの LockRect が失敗した",
            Fail::GetState => "状態を読む Get* が失敗した（戻せないので描かない）",
            Fail::DeviceChanged => "デバイスが作り直された（前のデバイスのテクスチャは使わない）",
            Fail::Panic => "ゲームのスレッドでパニックを受けた",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayCounters {
    /// 描いたフレーム。
    pub drawn: u32,
    /// `TestCooperativeLevel` が D3D_OK でなく描かなかったフレーム（デバイスが失われている間）。
    pub lost: u32,
    /// デバイスのアドレスが 0 で描かなかったフレーム。
    pub no_device: u32,
    /// 文が書きかけで読めず描かなかったフレーム。
    pub torn: u32,
    /// `DrawPrimitiveUP` が失敗したフレーム。
    pub draw_failed: u32,
    /// 自分の状態を置く・戻す `Set*` のどれかが失敗した回数。
    pub set_failed: u32,
    pub panics: u32,
    /// 止めた理由（`Fail` の番号）と、そのときの HRESULT。
    pub fail: u32,
    pub fail_hr: u32,
    /// 最初の 1 回（テクスチャを作ってコピーする）にかかった QPC のカウント。
    pub setup_ticks: u32,
}

// ---- ゲームの文字（ascii.anm）を読み取る前の確かめ ----

/// ゲームの文字をコピーできていない理由（最後の 1 つを `overlay_win` が覚え、本ログが書く）。`Wait` の組は「まだ読み込み前かもしれない」ので
/// 次の試しで直る。`Refuse` の組は直らないので試さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameFontWhy {
    None = 0,
    // ---- 試し直す ----
    /// AsciiManager の読み込み口のポインタが 0 か読めない（ゲームの初期化の前）。
    NoLoadedPtr = 1,
    /// AnmManager のポインタが 0 か読めない。
    NoAnmManager = 2,
    /// 読み込み口が `[AnmManager] + 0x144` と違う（別の物を掴んでいる）。
    PtrMismatch = 3,
    /// ファイルの中身が無い・読み込みの進みが 0 でない（読み込み中）。
    NotLoaded = 4,
    /// 読み込み口の名前が `ascii.anm` でない。
    NameMismatch = 5,
    /// スプライトの配列のポインタが 0 か、スプライトが読めない。
    SpriteUnreadable = 6,
    /// スプライトの欄が変（テクスチャのポインタが 0・矩形が空か画素の整数でない・テクスチャの大きさと合わない）。
    SpriteBad = 7,
    /// 文字によってテクスチャのポインタや大きさが違う。
    TextureDiffers = 8,
    /// 自分のテクスチャがまだ無い（最初の 1 回の後にだけ試す）。
    NoOwnTexture = 9,
    /// ゲームのテクスチャの vtable が自分のテクスチャと違う（IDirect3DTexture8 でなさそう）。
    VtableMismatch = 10,
    // ---- 試さない ----
    /// GetLevelDesc が失敗した。
    LevelDescFailed = 20,
    /// コピーできない画素の形式。
    FormatUnsupported = 21,
    /// GetLevelDesc の大きさがスプライトの欄の大きさと違う。
    DimsMismatch = 22,
    /// 文字の絵が予約した枠（16×16）より大きい。
    GlyphTooBig = 23,
    /// LockRect が失敗した（何度か試した後）。
    LockFailed = 24,
    /// 画素の切り出しが断られた（呼ぶ前の確かめをすり抜けた —— あり得ない）。
    ExtractFailed = 25,
    /// 試しの回数が上限（`GF_MAX_ATTEMPTS`）を越えた。待つ理由（読み込み前）が直らないまま長く続いたので、もう試さない。
    GaveUp = 26,
    // ---- 更新スレッド ----
    /// 更新スレッドがコピーした後の文字の表を作れなかった。
    ApplyFailed = 30,
}

impl GameFontWhy {
    pub fn from_code(c: u32) -> Option<GameFontWhy> {
        use GameFontWhy::*;
        [
            None,
            NoLoadedPtr,
            NoAnmManager,
            PtrMismatch,
            NotLoaded,
            NameMismatch,
            SpriteUnreadable,
            SpriteBad,
            TextureDiffers,
            NoOwnTexture,
            VtableMismatch,
            LevelDescFailed,
            FormatUnsupported,
            DimsMismatch,
            GlyphTooBig,
            LockFailed,
            ExtractFailed,
            GaveUp,
            ApplyFailed,
        ]
        .into_iter()
        .find(|w| *w as u32 == c)
    }

    pub fn text(self) -> &'static str {
        match self {
            GameFontWhy::None => "なし",
            GameFontWhy::NoLoadedPtr => {
                "AsciiManager の読み込み口のポインタが 0 か読めない（ゲームの初期化の前）"
            }
            GameFontWhy::NoAnmManager => "AnmManager のポインタが 0 か読めない",
            GameFontWhy::PtrMismatch => "読み込み口が AnmManager の窓 1 の位置（+0x144）と違う",
            GameFontWhy::NotLoaded => {
                "ascii.anm を読み込み中（ファイルの中身が無いか、読み込みの進みが 0 でない）"
            }
            GameFontWhy::NameMismatch => "読み込み口の名前が ascii.anm でない",
            GameFontWhy::SpriteUnreadable => "スプライトの配列が読めない",
            GameFontWhy::SpriteBad => {
                "スプライトの欄が変（テクスチャ 0・矩形が空か整数でない・テクスチャの大きさと合わない）"
            }
            GameFontWhy::TextureDiffers => "文字によってテクスチャが違う",
            GameFontWhy::NoOwnTexture => "自分のテクスチャがまだ無い",
            GameFontWhy::VtableMismatch => "ゲームのテクスチャの vtable が自分のテクスチャと違う",
            GameFontWhy::LevelDescFailed => "GetLevelDesc が失敗した",
            GameFontWhy::FormatUnsupported => "コピーできない画素の形式",
            GameFontWhy::DimsMismatch => "GetLevelDesc の大きさがスプライトの欄と違う",
            GameFontWhy::GlyphTooBig => "文字の絵が予約した枠より大きい",
            GameFontWhy::LockFailed => "LockRect が失敗した",
            GameFontWhy::ExtractFailed => "画素の切り出しが断られた",
            GameFontWhy::GaveUp => "試す回数の上限を越えたので、もう試さない",
            GameFontWhy::ApplyFailed => "更新スレッドがコピーした後の文字の表を作れなかった",
        }
    }
}

// ---- ゲームの文字をコピーする試しと、文字の表の差し替えの試し直しの決まり（pure。`overlay_win` が使う）----

/// ゲームの文字を試す間隔（描画の入口を通った回数）。60 fps で約 0.5 秒。試しは safe な読み（VirtualQuery）が数回だが、
/// 読み込み前の長い間、毎フレーム払う理由が無い。
pub const GF_EVERY: u32 = 30;
/// 試しの回数の上限。越えたら以降は試さず、GDI の文字のまま描く。600 回 × 30 フレーム = 5 分。ゲームは ascii.anm を起動時に読み込む形
/// （`0x004344ED`〜`0x00434503` の AsciiManager の初期化）なので、読み込みは描画が始まってから長くかからないはず。
/// 5 分は、それを大きく越えて待つ余裕として置いた値（読み込みにかかる時間そのものは測っていない）。5 分を越えても読み込み前なら、
/// ゲームの作りが想定と違う（別のバージョンなど）。
pub const GF_MAX_ATTEMPTS: u32 = 600;
/// LockRect の失敗を待つ回数（この回数目の失敗であきらめる）。
pub const GF_LOCK_FAIL_LIMIT: u32 = 5;

/// 描画の入口を通った通し番号 `frame`（0 から）と、それまでの試しの回数 `attempts` から、今回することを決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfGate {
    /// 試す間隔でない（何もしない）。
    Skip,
    /// 試す。
    Try,
    /// 試す間隔だが、回数の上限を越えているので、もう試さない（あきらめる）。
    GiveUp,
}

pub fn gf_gate(frame: u32, attempts: u32) -> GfGate {
    if !frame.is_multiple_of(GF_EVERY) {
        GfGate::Skip
    } else if attempts >= GF_MAX_ATTEMPTS {
        GfGate::GiveUp
    } else {
        GfGate::Try
    }
}

pub fn gf_lock_gives_up(fails: u32) -> bool {
    fails >= GF_LOCK_FAIL_LIMIT
}

/// 文字の表の差し替えでテクスチャを作り直すのに失敗したとき、差し替え前のテクスチャで描き続けながら試し直す回数の上限
/// （最初の 1 回を含めて `SWAP_MAX_TRIES` 回試して、だめならあきらめる）。
pub const SWAP_MAX_TRIES: u32 = 3;
/// 失敗の後、次の試しまで待つ描画の入口の回数（60 fps で約 1 秒。失敗した直後の毎フレームに CreateTexture を呼ばない）。
pub const SWAP_RETRY_EVERY: u32 = 60;

/// 差し替えのテクスチャを作りに行くか。`tries` はここまでに試した回数（0 = まだ 1 回も試していない）、`frames_since` は
/// 最後の失敗からの描画の入口の回数。最初の 1 回はすぐ、失敗の後は `SWAP_RETRY_EVERY` 回待って、`SWAP_MAX_TRIES` 回まで。
pub fn swap_should_try(tries: u32, frames_since: u32) -> bool {
    tries == 0 || (tries < SWAP_MAX_TRIES && frames_since >= SWAP_RETRY_EVERY)
}

/// 読むアドレス（本物は `th09_addr` の定数。検査は偽のメモリのアドレス）。
#[derive(Debug, Clone, Copy)]
pub struct GameAddrs {
    /// AsciiManager の読み込み口のポインタの置き場（`ASCII_ANM_LOADED_PTR`）。
    pub loaded_ptr: u32,
    /// AnmManager のポインタの置き場（`ANM_MANAGER_PTR`）。
    pub anm_manager_ptr: u32,
}

/// スプライト 1 つの矩形と、画面に描く大きさ（`+0x34` × `+0x30` を四捨五入。読めなければ 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpriteInfo {
    pub rect: Rect,
    pub quad_w: u32,
    pub quad_h: u32,
}

/// 確かめが通った結果: 文字の絵の載っているテクスチャ（1 つ）とその大きさ、文字ごとのスプライト
/// （枠の番号 = `game_index`。`` ` `` の枠は `None`）。
#[derive(Debug, Clone, Copy)]
pub struct Probed {
    pub texture: u32,
    pub tex_w: u32,
    pub tex_h: u32,
    pub sprites: [Option<SpriteInfo>; GAME_GLYPHS],
}

/// 読む関数: `addr` から `buf.len()` バイトを読む。読めなければ false（ゲームのメモリを読むだけ。ヒープを使わない）。
pub type ReadMem<'a> = &'a mut dyn FnMut(u32, &mut [u8]) -> bool;

fn rd32(read: ReadMem, addr: u32) -> Option<u32> {
    let mut b = [0u8; 4];
    read(addr, &mut b).then(|| u32::from_le_bytes(b))
}

fn u32_at(b: &[u8], off: u32) -> Option<u32> {
    let o = off as usize;
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn f32_at(b: &[u8], off: u32) -> Option<f32> {
    let o = off as usize;
    Some(f32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn pixel_of(f: f32) -> Option<u32> {
    (f.is_finite() && (0.0..=65536.0).contains(&f) && (f - f.round()).abs() <= 0.01)
        .then(|| f.round() as u32)
}

/// ゲームが ascii.anm を読み終えているかを確かめ、英数字のスプライトを読む（読むだけ。確保しない）。確かめる順と根拠:
/// 1. `[loaded_ptr]` = L（0 でない）。2. `[anm_manager_ptr]` = M（0 でない）で、`L == M + 0x120 × 1 + 0x24`（`0x0043C640`〜`0x0043C682` が
///    返す読み込み口の式）。3. `[L+4]`（ファイルの中身）が 0 でなく、`[L+0x18]`（読み込みの進み）が 0（`0x0043C7D0`〜 が 0 になるまで待つ）。
/// 4. `L+0x20` が `"ascii.anm\0"`。5. `[L+0x0C]`（スプライトの配列）が 0 でなく、文字 `0x21`〜`0x7E`（`` ` `` を除く）の 0x44 バイトが
///    読めて、`+4` のテクスチャが 0 でなく全部同じ、矩形（`+8`〜`+0x14`）が整数の画素で空でなくテクスチャの大きさ（`+0x1C` 幅・`+0x18` 高さ）の中に収まる。
pub fn probe_game_font(read: ReadMem, a: GameAddrs) -> Result<Probed, GameFontWhy> {
    let l = rd32(read, a.loaded_ptr)
        .filter(|&v| v != 0)
        .ok_or(GameFontWhy::NoLoadedPtr)?;
    let m = rd32(read, a.anm_manager_ptr)
        .filter(|&v| v != 0)
        .ok_or(GameFontWhy::NoAnmManager)?;
    let want = m
        .checked_add(ANM_LOADED_STRIDE * ASCII_ANM_INDEX + ANM_LOADED_START)
        .ok_or(GameFontWhy::PtrMismatch)?;
    if l != want {
        return Err(GameFontWhy::PtrMismatch);
    }
    let field = |off: u32| l.checked_add(off);
    let file = field(LOADED_FILE)
        .and_then(|at| rd32(read, at))
        .ok_or(GameFontWhy::NotLoaded)?;
    let progress = field(LOADED_PROGRESS)
        .and_then(|at| rd32(read, at))
        .ok_or(GameFontWhy::NotLoaded)?;
    if file == 0 || progress != 0 {
        return Err(GameFontWhy::NotLoaded);
    }
    let mut name = [0u8; ASCII_ANM_NAME.len()];
    if !field(LOADED_NAME).is_some_and(|at| read(at, &mut name)) || name != *ASCII_ANM_NAME {
        return Err(GameFontWhy::NameMismatch);
    }
    let sprites = field(LOADED_SPRITES)
        .and_then(|at| rd32(read, at))
        .filter(|&v| v != 0)
        .ok_or(GameFontWhy::SpriteUnreadable)?;
    let mut out = Probed {
        texture: 0,
        tex_w: 0,
        tex_h: 0,
        sprites: [None; GAME_GLYPHS],
    };
    for code in ov_range() {
        let Some(index) = char::from_u32(code).and_then(game_index) else {
            continue;
        };
        let at = (code - SPRITE_CHAR_BASE)
            .checked_mul(SPRITE_STRIDE)
            .and_then(|o| sprites.checked_add(o))
            .ok_or(GameFontWhy::SpriteUnreadable)?;
        let mut b = [0u8; SPRITE_STRIDE as usize];
        if !read(at, &mut b) {
            return Err(GameFontWhy::SpriteUnreadable);
        }
        let tex = u32_at(&b, SPRITE_TEXTURE)
            .filter(|&t| t != 0)
            .ok_or(GameFontWhy::SpriteBad)?;
        let px = |off: u32| f32_at(&b, off).and_then(pixel_of);
        let (Some(x0), Some(y0), Some(x1), Some(y1), Some(tw), Some(th)) = (
            px(SPRITE_X0),
            px(SPRITE_Y0),
            px(SPRITE_X1),
            px(SPRITE_Y1),
            px(SPRITE_TEX_W),
            px(SPRITE_TEX_H),
        ) else {
            return Err(GameFontWhy::SpriteBad);
        };
        if x1 <= x0 || y1 <= y0 || x1 > tw || y1 > th || tw == 0 || th == 0 {
            return Err(GameFontWhy::SpriteBad);
        }
        if out.texture == 0 {
            (out.texture, out.tex_w, out.tex_h) = (tex, tw, th);
        } else if (out.texture, out.tex_w, out.tex_h) != (tex, tw, th) {
            return Err(GameFontWhy::TextureDiffers);
        }
        // 画面に描く大きさは、読めなければ 0（`apply_game_font` は 0 なら絵の大きさで描く）。
        let q = |off: u32| {
            f32_at(&b, off)
                .filter(|f| f.is_finite() && (0.0..=65536.0).contains(f))
                .map_or(0, |f| f.round() as u32)
        };
        out.sprites[index] = Some(SpriteInfo {
            rect: Rect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            },
            quad_w: q(SPRITE_W),
            quad_h: q(SPRITE_H),
        });
    }
    Ok(out)
}

fn ov_range() -> core::ops::RangeInclusive<u32> {
    ov::GAME_FIRST..=ov::GAME_LAST
}

// ---- 文の更新スレッド ----

/// 何回ぶんの見本で fps を出すか（250 ms ごとに 1 つ → 約 1 秒）。
const FPS_SPAN: usize = 5;

/// 更新スレッドの計算（読み取りの側で 250 ms ごと）: 描画の入口を通った回数と QPC の組を覚えておき、約 1 秒前との差から
/// fps を出して文を作る。
#[derive(Debug, Clone, Default)]
pub struct Updater {
    samples: std::collections::VecDeque<(u64, u32)>,
}

impl Updater {
    /// `now` は QPC、`draws` は描画の入口を通った回数（`draw::draw_calls`）、`qpf` は QPC の周波数。
    pub fn step(&mut self, now: u64, draws: u32, qpf: u64, latency: u32) -> String {
        self.samples.push_back((now, draws));
        while self.samples.len() > FPS_SPAN {
            self.samples.pop_front();
        }
        let fps_centi = match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, d0)), Some(&(t1, d1))) if self.samples.len() > 1 => {
                ov::fps_centi(d1.wrapping_sub(d0), t1.saturating_sub(t0), qpf)
            }
            _ => None,
        };
        ov::status_line(&Status {
            fps_centi,
            latency: u8::try_from(latency).ok(),
        })
    }
}

// ---- 設置と集計（`session::run` から）----

/// 文字の描画を受け持つもの。本物は `overlay_win::RealOverlayHost`。
pub trait OverlayHost {
    /// DLL のフォルダの `HiganRTool.ini` を読む（移さない）。無ければ `Ok(None)`。
    fn read_ini(&mut self) -> Result<Option<String>, String>;
    /// GDI で文字の表を描き、ゲームのスレッドが読む置き場へ置く（1 回だけ）。文字の枠の大きさなどの文を返す。
    fn build_atlas(&mut self) -> Result<String, String>;
    /// デバイスと present parameters のアドレスを確かめて置き、文の更新スレッドを起こす（描画の入口はまだ描かない）。
    fn arm(&mut self) -> Result<(), String>;
    /// 描画の入口の分岐を開く（`OVERLAY_ON` = 1）。
    fn on(&mut self);
    /// ini の `[overlay] preview = 1`（旗が立っているときだけ、`build_atlas` の前に呼ぶ）。
    fn set_preview(&mut self, _on: bool) {}
    /// 右下の fps の行を出さない（ゲーム内のメニューのためだけに文字の描画を開いたとき。`[overlay] enable = 1` が無い）。`arm` の前に呼ぶ。
    fn set_quiet(&mut self, _on: bool) {}
    /// ゲームの文字（ascii.anm の絵）の状態の説明（本ログ用。ゲームの文字を使わない実装は空）。
    fn game_font_report(&self) -> String {
        String::new()
    }
    fn counters(&self) -> OverlayCounters;
    fn timings(&self) -> TimingSnapshot;
    /// QPC の周波数。
    fn qpf(&self) -> u64;
}

/// 文字の描画を入れる（`session::run` が描画の detour の後に 1 回呼ぶ）。ini の `[overlay]` に `enable = 1` が
/// あり、描画の detour が書け、文字の表とアドレスが置けたときだけ開いて true を返す。ログは「文字の描画: 」で始まる行。
pub fn install(host: &mut dyn OverlayHost, draw_written: bool, out: &mut dyn Write) -> bool {
    let text = match host.read_ini() {
        Ok(Some(t)) => t,
        Ok(None) => {
            let _ = writeln!(out, "文字の描画: HiganRTool.ini が無い（描かない）");
            return false;
        }
        Err(why) => {
            let _ = writeln!(
                out,
                "文字の描画: HiganRTool.ini を読めない（{why}）。描かない"
            );
            return false;
        }
    };
    let (preview, quiet) = match higanr_core::ini::parse(&text)
        .map_err(|e| e.to_string())
        .and_then(|ini| {
            Ok((
                ov::enabled_in_ini(&ini)?,
                ov::preview_in_ini(&ini)?,
                ov::game_ui_in_ini(&ini),
            ))
        }) {
        Ok((true, p, _)) => (p, false),
        // ゲーム内のメニュー（`code_side = game`）は、文字が出ないと使えないので `[overlay] enable` が無くても開く。右下の行は出さない。
        Ok((false, p, true)) => (p, true),
        Ok((false, _, false)) => {
            let _ = writeln!(
                detail_out(out),
                "文字の描画: ini の [{}] に enable = 1 が無い（描かない）",
                ov::SECTION
            );
            return false;
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "文字の描画: ini の [{}] を読めない（{e}）。描かない",
                ov::SECTION
            );
            return false;
        }
    };
    if !draw_written {
        let _ = writeln!(
            out,
            "文字の描画: 描画の地点の detour が書けていないので描かない"
        );
        return false;
    }
    if preview {
        host.set_preview(true);
        let _ = writeln!(
            detail_out(out),
            "文字の描画: 見本の旗（preview = 1）が立っている。ゲームの文字をコピーした後、文字の表と見本の行を PNG にして本ログの隣へ書き、画面の右下に見本の行を出す"
        );
    }
    if quiet {
        host.set_quiet(true);
        let _ = writeln!(
            detail_out(out),
            "文字の描画: [{}] に enable = 1 が無いが、ゲーム内のメニュー（code_side = game）のために開く（右下の行は出さない）",
            ov::SECTION
        );
    }
    match host.build_atlas() {
        Ok(desc) => {
            let _ = writeln!(detail_out(out), "文字の描画: 文字の表を作った（{desc}）");
        }
        Err(why) => {
            let _ = writeln!(out, "文字の描画: 文字の表を作れない（{why}）。描かない");
            return false;
        }
    }
    if let Err(why) = host.arm() {
        let _ = writeln!(out, "文字の描画: 置けない（{why}）。描かない");
        return false;
    }
    host.on();
    let _ = writeln!(
        detail_out(out),
        "文字の描画: 有効にした（描画の入口で、画面の右下に 1 行。状態は描くたびに読んで戻す）"
    );
    true
}

pub fn finish(host: &mut dyn OverlayHost, out: &mut dyn Write) {
    let c = host.counters();
    let qpf = host.qpf().max(1);
    let us = |t: u32| t as f64 * 1_000_000.0 / qpf as f64;
    // 描画の失敗・パニック・止まった理由があるときだけ簡潔にも残す。
    let abnormal = c.draw_failed > 0
        || c.set_failed > 0
        || c.panics > 0
        || !matches!(Fail::from_code(c.fail), Some(Fail::None));
    let out: &mut dyn Write = if abnormal { out } else { &mut detail_out(out) };
    let _ = writeln!(
        out,
        "==== 文字の描画（M1c-2） ====\n文字の描画: 描いた {} フレーム / デバイスが失われていて描かなかった {} / デバイスが 0 で描かなかった {} / 文が書きかけで描かなかった {} / DrawPrimitiveUP の失敗 {} / Set* の失敗 {} / パニック {}",
        c.drawn, c.lost, c.no_device, c.torn, c.draw_failed, c.set_failed, c.panics
    );
    let _ = writeln!(
        out,
        "文字の描画: 最初の 1 回（テクスチャを作ってコピーする）{:.1} µs",
        us(c.setup_ticks)
    );
    let gf = host.game_font_report();
    if !gf.is_empty() {
        let _ = writeln!(out, "文字の描画: ゲームの字: {gf}");
    }
    match Fail::from_code(c.fail) {
        Some(Fail::None) => {}
        Some(f) => {
            let _ = writeln!(
                out,
                "文字の描画: 止めた（{}。HRESULT 0x{:08X}）",
                f.text(),
                c.fail_hr
            );
        }
        None => {
            let _ = writeln!(out, "文字の描画: 止めた（知らない理由の番号 {}）", c.fail);
        }
    }
    let t = host.timings();
    match Dist::of(t.recent) {
        Some(d) => {
            let _ = writeln!(
                out,
                "文字の描画: 1 フレームの上乗せ（描画の入口の中。最初の 1 回を除く。最後の {} 件、QPC）{}。全 {} 件の最大 {:.1} µs",
                d.n,
                d.text(qpf),
                t.total,
                us(t.max)
            );
        }
        None => {
            let _ = writeln!(out, "文字の描画: 1 フレームの上乗せ: 描いた件が 0 件");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn read_string(b: &TextBox) -> Option<String> {
        let mut units = [0u16; TEXT_UNITS];
        let n = b.read(&mut units)?;
        let mut utf8 = [0u8; ov::MAX_TEXT_UTF8];
        let m = ov::utf16_to_utf8(&units[..n], &mut utf8);
        Some(String::from_utf8(utf8[..m].to_vec()).unwrap())
    }

    #[test]
    fn text_box_round_trip_and_truncation() {
        let b = TextBox::new();
        let mut buf = [0u16; TEXT_UNITS];
        assert_eq!(b.read(&mut buf), Some(0), "まだ書いていないバッファは空");
        b.publish("HiganRTool 60.00fps");
        assert_eq!(read_string(&b).as_deref(), Some("HiganRTool 60.00fps"));
        b.publish("second");
        assert_eq!(read_string(&b).as_deref(), Some("second"));
        let mixed = "接続コード\n接続しました。あなたは 1P です。\n遅延 2 😀";
        b.publish(mixed);
        let n = b.read(&mut buf).unwrap();
        assert_eq!(n, mixed.encode_utf16().count());
        assert_eq!(read_string(&b).as_deref(), Some(mixed));
        let ja = "あ".repeat(MAX_TEXT);
        assert!(ja.len() > 63 * 5, "この文は旧いバッファに入らない大きさ");
        b.publish(&ja);
        assert_eq!(b.read(&mut buf), Some(MAX_TEXT));
        assert_eq!(read_string(&b).as_deref(), Some(ja.as_str()));
        // 上限を越える分は落とす。最後が基本面の外の文字（2 単位）にまたがるときは、その文字ごと落とす。
        b.publish(&format!("{ja}あ"));
        assert_eq!(b.read(&mut buf), Some(MAX_TEXT));
        let tail = format!("{}😀", "x".repeat(MAX_TEXT - 1));
        b.publish(&tail);
        assert_eq!(b.read(&mut buf), Some(MAX_TEXT - 1));
        assert_eq!(read_string(&b), Some("x".repeat(MAX_TEXT - 1)));
        b.publish("ab");
        assert_eq!(read_string(&b).as_deref(), Some("ab"));
    }

    #[test]
    fn text_box_rejects_torn_reads() {
        let b = TextBox::new();
        b.publish("abc");
        let g = b.generation.load(Ordering::SeqCst);
        let slot = &b.slots[(g & 1) as usize];
        let s = slot.seq.load(Ordering::SeqCst);
        slot.seq.store(s + 1, Ordering::SeqCst);
        let mut buf = [0u16; TEXT_UNITS];
        assert_eq!(b.read(&mut buf), None, "書いている途中を読んだ");
        slot.seq.store(s, Ordering::SeqCst);
        assert_eq!(b.read(&mut buf), Some(3));
        let got = b.read_with(
            &mut buf,
            || {},
            || {
                b.publish("xyz1");
                b.publish("xyz2");
            },
        );
        assert_eq!(
            got, None,
            "読んでいる間に書き換わったバッファを読めたことにした"
        );
        assert_eq!(read_string(&b).as_deref(), Some("xyz2"));
        let g = b.generation.load(Ordering::SeqCst);
        let len = &b.slots[(g & 1) as usize].len;
        len.store(MAX_TEXT as u32 + 1, Ordering::SeqCst);
        assert_eq!(b.read(&mut buf), None);
        len.store(MAX_TEXT as u32, Ordering::SeqCst);
        assert_eq!(b.read(&mut buf), Some(MAX_TEXT));
    }

    #[test]
    fn text_box_never_returns_a_mix_of_two_texts() {
        let b = TextBox::new();
        let (a, c) = ("あ".repeat(MAX_TEXT), "い".repeat(MAX_TEXT));
        b.publish(&a);
        let mut buf = [0u16; TEXT_UNITS];
        let got = b.read_with(
            &mut buf,
            || {
                b.publish(&c);
                b.publish(&c);
            },
            || {},
        );
        assert_eq!(buf[0], 0x3042);
        assert_eq!(
            buf[TEXT_UNITS - 1],
            0x3044,
            "検査が半々の中身を作れていない"
        );
        assert_eq!(got, None, "2 つの文が混ざった中身を読めたことにした");
        assert_eq!(b.read(&mut buf), Some(MAX_TEXT));
        assert!(buf.iter().all(|&u| u == 0x3044));
    }

    #[test]
    fn game_font_gate_skips_tries_and_gives_up() {
        assert_eq!(gf_gate(0, 0), GfGate::Try);
        assert_eq!(gf_gate(1, 0), GfGate::Skip);
        assert_eq!(gf_gate(29, 0), GfGate::Skip);
        assert_eq!(gf_gate(30, 1), GfGate::Try);
        assert_eq!(gf_gate(60, GF_MAX_ATTEMPTS - 1), GfGate::Try, "最後の 1 回");
        assert_eq!(gf_gate(90, GF_MAX_ATTEMPTS), GfGate::GiveUp);
        assert_eq!(gf_gate(90, GF_MAX_ATTEMPTS + 7), GfGate::GiveUp);
        // 間隔でないフレームは、上限を越えていても何もしない。
        assert_eq!(gf_gate(91, GF_MAX_ATTEMPTS), GfGate::Skip);
        assert!(!gf_lock_gives_up(GF_LOCK_FAIL_LIMIT - 1));
        assert!(gf_lock_gives_up(GF_LOCK_FAIL_LIMIT));
    }

    #[test]
    fn swap_retry_is_spaced_and_bounded() {
        assert!(swap_should_try(0, 0), "最初の 1 回はすぐ");
        assert!(swap_should_try(0, 12345));
        assert!(!swap_should_try(1, 0), "失敗の直後は待つ");
        assert!(!swap_should_try(1, SWAP_RETRY_EVERY - 1));
        assert!(swap_should_try(1, SWAP_RETRY_EVERY));
        assert!(swap_should_try(SWAP_MAX_TRIES - 1, SWAP_RETRY_EVERY));
        assert!(
            !swap_should_try(SWAP_MAX_TRIES, u32::MAX),
            "上限の回数を越えて試す"
        );
    }

    #[test]
    fn gave_up_reason_is_a_refusal() {
        assert_eq!(GameFontWhy::from_code(26), Some(GameFontWhy::GaveUp));
        assert!(GameFontWhy::GaveUp as u32 >= GameFontWhy::LevelDescFailed as u32);
        assert!(GameFontWhy::GaveUp.text().contains("上限"));
    }

    #[test]
    fn timings_keep_recent_and_overall_max() {
        let t = Timings::new();
        t.record(500);
        for i in 0..(TIMING_LEN as u32 + 10) {
            t.record(i % 7);
        }
        let s = t.snapshot();
        assert_eq!(s.total, TIMING_LEN as u32 + 11);
        assert_eq!(s.max, 500);
        assert_eq!(s.recent.len(), TIMING_LEN);
        assert!(
            s.recent.iter().all(|&v| v < 7),
            "リングバッファの外の古い 500 が残っている"
        );
    }

    #[test]
    fn updater_makes_the_line() {
        let mut u = Updater::default();
        let qpf = 10_000_000;
        assert_eq!(u.step(0, 0, qpf, NO_LATENCY), "HiganRTool");
        let mut line = String::new();
        for i in 1..=8u64 {
            line = u.step(i * 2_500_000, (i * 15) as u32, qpf, 2);
        }
        assert_eq!(line, "HiganRTool 60.00fps Latency 2");
        assert_eq!(u.samples.len(), FPS_SPAN);
    }

    #[derive(Default)]
    pub(crate) struct FakeOverlay {
        pub ini: Option<String>,
        pub atlas_fail: bool,
        pub arm_fail: bool,
        pub events: Vec<&'static str>,
        pub counters: OverlayCounters,
        pub recent: Vec<i64>,
        pub report: String,
    }

    impl OverlayHost for FakeOverlay {
        fn read_ini(&mut self) -> Result<Option<String>, String> {
            self.events.push("ini");
            Ok(self.ini.clone())
        }
        fn set_preview(&mut self, on: bool) {
            assert!(on, "旗が立っていないときは呼ばない");
            self.events.push("preview");
        }
        fn set_quiet(&mut self, on: bool) {
            assert!(on, "旗が立っていないときは呼ばない");
            self.events.push("quiet");
        }
        fn game_font_report(&self) -> String {
            self.report.clone()
        }
        fn build_atlas(&mut self) -> Result<String, String> {
            self.events.push("atlas");
            if self.atlas_fail {
                return Err("合成の失敗".to_string());
            }
            Ok("合成".to_string())
        }
        fn arm(&mut self) -> Result<(), String> {
            self.events.push("arm");
            if self.arm_fail {
                return Err("合成の失敗".to_string());
            }
            Ok(())
        }
        fn on(&mut self) {
            self.events.push("on");
        }
        fn counters(&self) -> OverlayCounters {
            self.counters
        }
        fn timings(&self) -> TimingSnapshot {
            TimingSnapshot {
                total: self.recent.len() as u32,
                max: self.recent.iter().copied().max().unwrap_or(0) as u32,
                recent: self.recent.clone(),
            }
        }
        fn qpf(&self) -> u64 {
            10_000_000
        }
    }

    fn run(f: &mut FakeOverlay, draw_written: bool) -> (bool, String) {
        let mut out = Vec::new();
        let r = install(f, draw_written, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_game_menu_opens_the_overlay_quietly_without_an_overlay_section() {
        let mk = |ini: &str| FakeOverlay {
            ini: Some(ini.to_string()),
            ..Default::default()
        };
        let game = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\n";
        let mut f = mk(game);
        let (r, log) = run(&mut f, true);
        assert!(r, "{log}");
        assert_eq!(f.events, vec!["ini", "quiet", "atlas", "arm", "on"]);
        assert!(log.contains("ゲーム内のメニュー"), "{log}");
        let mut f = mk(&format!("{game}[overlay]\nenable = 1\n"));
        let (r, _) = run(&mut f, true);
        assert!(r);
        assert_eq!(f.events, vec!["ini", "atlas", "arm", "on"]);
        for ini in [
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n",
            "[netplay]\nenable = 0\ncode_side = game\n",
            "[netplay]\ncode_side = game\n",
            "[pacing]\nenable = 1\n",
        ] {
            let mut f = mk(ini);
            let (r, log) = run(&mut f, true);
            assert!(!r, "{ini:?}: {log}");
            assert_eq!(f.events, vec!["ini"], "{ini:?}");
        }
        let mut f = mk(game);
        let (r, _) = run(&mut f, false);
        assert!(!r);
        assert!(!f.events.contains(&"atlas"));
    }

    #[test]
    fn install_passes_the_preview_flag_before_the_atlas() {
        let mk = |ini: &str| FakeOverlay {
            ini: Some(ini.to_string()),
            ..Default::default()
        };
        let mut f = mk("[overlay]\nenable = 1\npreview = 1\n");
        let (r, log) = run(&mut f, true);
        assert!(r, "{log}");
        assert_eq!(f.events, ["ini", "preview", "atlas", "arm", "on"]);
        assert!(log.contains("見本の旗（preview = 1）"), "{log}");
        for (label, ini, draw, events) in [
            (
                "旗なし",
                "[overlay]\nenable = 1\n",
                true,
                &["ini", "atlas", "arm", "on"][..],
            ),
            (
                "preview = 0",
                "[overlay]\nenable = 1\npreview = 0\n",
                true,
                &["ini", "atlas", "arm", "on"],
            ),
            (
                "enable = 0",
                "[overlay]\nenable = 0\npreview = 1\n",
                true,
                &["ini"],
            ),
            (
                "detour なし",
                "[overlay]\nenable = 1\npreview = 1\n",
                false,
                &["ini"],
            ),
            (
                "読めない値",
                "[overlay]\nenable = 1\npreview = yes\n",
                true,
                &["ini"],
            ),
        ] {
            let mut f = mk(ini);
            let (_, log) = run(&mut f, draw);
            assert_eq!(f.events, events, "{label}: {log}");
            assert!(!log.contains("見本の旗"), "{label}: {log}");
        }
    }

    #[test]
    fn finish_writes_the_game_font_report_only_when_there_is_one() {
        let mut f = FakeOverlay {
            report: "写した（テスト）".to_string(),
            recent: vec![10, 20],
            ..Default::default()
        };
        let mut out = Vec::new();
        finish(&mut f, &mut out);
        let log = String::from_utf8(out).unwrap();
        assert!(
            log.contains("文字の描画: ゲームの字: 写した（テスト）"),
            "{log}"
        );
        f.report.clear();
        let mut out = Vec::new();
        finish(&mut f, &mut out);
        assert!(!String::from_utf8(out).unwrap().contains("ゲームの字"));
    }

    // ---- ゲームの文字を読む前の確かめ（偽のメモリ）----

    struct Mem {
        regions: Vec<(u32, Vec<u8>)>,
        reads: Vec<(u32, usize)>,
    }

    impl Mem {
        fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            self.reads.push((addr, buf.len()));
            for (base, bytes) in &self.regions {
                if addr >= *base && (addr - base) as usize + buf.len() <= bytes.len() {
                    let at = (addr - base) as usize;
                    buf.copy_from_slice(&bytes[at..at + buf.len()]);
                    return true;
                }
            }
            false
        }
        fn put(&mut self, addr: u32, bytes: &[u8]) {
            for (base, data) in &mut self.regions {
                if addr >= *base && (addr - *base) as usize + bytes.len() <= data.len() {
                    let at = (addr - *base) as usize;
                    data[at..at + bytes.len()].copy_from_slice(bytes);
                    return;
                }
            }
            panic!("区画の外 {addr:#X}");
        }
    }

    const P_LOADED: u32 = 0x1000;
    const P_MGR: u32 = 0x1004;
    const L: u32 = 0x2000;
    const SPRITES: u32 = 0x3000;
    const TEX: u32 = 0xAAAA_0000;
    const ADDRS: GameAddrs = GameAddrs {
        loaded_ptr: P_LOADED,
        anm_manager_ptr: P_MGR,
    };

    fn sprite_at(c: u32) -> u32 {
        SPRITES + (c - 0x20) * 0x44
    }

    fn put_sprite(
        m: &mut Mem,
        c: u32,
        tex: u32,
        r: [f32; 4],
        tex_wh: (f32, f32),
        quad_wh: (f32, f32),
    ) {
        let mut b = [0u8; 0x44];
        b[4..8].copy_from_slice(&tex.to_le_bytes());
        for (i, v) in [r[0], r[1], r[2], r[3], tex_wh.1, tex_wh.0]
            .iter()
            .enumerate()
        {
            b[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b[0x30..0x34].copy_from_slice(&quad_wh.1.to_le_bytes());
        b[0x34..0x38].copy_from_slice(&quad_wh.0.to_le_bytes());
        m.put(sprite_at(c), &b);
    }

    /// 読み込み済みの ascii.anm の偽のメモリ。文字 c の絵は 7 × 9（`(k % 16) × 8`, `(k / 16) × 10`）、テクスチャは 128 × 128。
    fn loaded_world() -> Mem {
        let mut m = Mem {
            regions: vec![
                (P_LOADED, vec![0; 8]),
                (L, vec![0; 0x40]),
                (SPRITES, vec![0; 0x60 * 0x44]),
            ],
            reads: Vec::new(),
        };
        m.put(P_LOADED, &L.to_le_bytes());
        m.put(P_MGR, &(L - 0x144).to_le_bytes());
        m.put(L + 4, &0x1234u32.to_le_bytes());
        m.put(L + 0x0C, &SPRITES.to_le_bytes());
        m.put(L + 0x20, b"ascii.anm\0");
        for c in 0x21u32..=0x7E {
            let k = (c - 0x21) as f32;
            let (x0, y0) = ((k % 16.0) * 8.0, (k / 16.0).floor() * 10.0);
            put_sprite(
                &mut m,
                c,
                TEX,
                [x0, y0, x0 + 7.0, y0 + 9.0],
                (128.0, 128.0),
                (7.0, 9.0),
            );
        }
        m
    }

    fn probe(m: &mut Mem) -> Result<Probed, GameFontWhy> {
        probe_game_font(&mut |a, b| m.read(a, b), ADDRS)
    }

    #[test]
    fn probe_reads_the_loaded_sprites() {
        let mut m = loaded_world();
        let p = probe(&mut m).expect("読み込み済み");
        assert_eq!((p.texture, p.tex_w, p.tex_h), (TEX, 128, 128));
        assert_eq!(p.sprites.iter().flatten().count(), 93);
        let a = p.sprites[ov::game_index('A').unwrap()].unwrap();
        let k = 'A' as u32 - 0x21;
        assert_eq!(
            a,
            SpriteInfo {
                rect: Rect {
                    x: (k % 16) * 8,
                    y: (k / 16) * 10,
                    w: 7,
                    h: 9
                },
                quad_w: 7,
                quad_h: 9
            }
        );
        assert!(p.sprites[0x60 - 0x21].is_none(), "` の枠は欠番");
        let bt = sprite_at(0x60);
        assert!(
            m.reads
                .iter()
                .all(|&(a, n)| !(a < bt + 0x44 && a + n as u32 > bt)),
            "` のスプライトを読んだ"
        );
        assert_eq!(m.reads.iter().filter(|r| r.1 == 0x44).count(), 93);
        assert!(m.reads.iter().all(|&(a, n)| {
            [(P_LOADED, 8), (L, 0x40), (SPRITES, 0x60 * 0x44)]
                .iter()
                .any(|&(base, len)| a >= base && a + n as u32 <= base + len)
        }));
        put_sprite(
            &mut m,
            'A' as u32,
            TEX,
            [0.0, 0.0, 7.0, 9.0],
            (128.0, 128.0),
            (9.6, 13.4),
        );
        let a = probe(&mut m).unwrap().sprites[ov::game_index('A').unwrap()].unwrap();
        assert_eq!((a.quad_w, a.quad_h), (10, 13), "四捨五入");
        put_sprite(
            &mut m,
            'A' as u32,
            TEX,
            [0.0, 0.0, 7.0, 9.0],
            (128.0, 128.0),
            (f32::NAN, -3.0),
        );
        let a = probe(&mut m).unwrap().sprites[ov::game_index('A').unwrap()].unwrap();
        assert_eq!((a.quad_w, a.quad_h), (0, 0));
    }

    #[test]
    fn probe_refuses_each_unready_state() {
        let a_at = |off: u32| sprite_at('M' as u32) + off;
        type Edit = Box<dyn Fn(&mut Mem)>;
        let cases: Vec<(&str, Edit, GameFontWhy)> = vec![
            (
                "L が 0",
                Box::new(|m| m.put(P_LOADED, &0u32.to_le_bytes())),
                GameFontWhy::NoLoadedPtr,
            ),
            (
                "置き場が読めない",
                Box::new(|m| m.regions.retain(|r| r.0 != P_LOADED)),
                GameFontWhy::NoLoadedPtr,
            ),
            (
                "M が 0",
                Box::new(|m| m.put(P_MGR, &0u32.to_le_bytes())),
                GameFontWhy::NoAnmManager,
            ),
            (
                "M が窓 1 の式と違う",
                Box::new(|m| m.put(P_MGR, &(L - 0x148).to_le_bytes())),
                GameFontWhy::PtrMismatch,
            ),
            (
                "M が窓 0 の式（L − 0x24）",
                Box::new(|m| m.put(P_MGR, &(L - 0x24).to_le_bytes())),
                GameFontWhy::PtrMismatch,
            ),
            (
                "ファイル 0",
                Box::new(|m| m.put(L + 4, &0u32.to_le_bytes())),
                GameFontWhy::NotLoaded,
            ),
            (
                "進み 1",
                Box::new(|m| m.put(L + 0x18, &1u32.to_le_bytes())),
                GameFontWhy::NotLoaded,
            ),
            (
                "名前",
                Box::new(|m| m.put(L + 0x20, b"ascii.ang\0")),
                GameFontWhy::NameMismatch,
            ),
            (
                "名前の終わりの 0 が無い",
                Box::new(|m| m.put(L + 0x20, b"ascii.anmx")),
                GameFontWhy::NameMismatch,
            ),
            (
                "スプライトの配列 0",
                Box::new(|m| m.put(L + 0x0C, &0u32.to_le_bytes())),
                GameFontWhy::SpriteUnreadable,
            ),
            (
                "スプライトの配列が範囲の外",
                Box::new(|m| m.put(L + 0x0C, &0x9000u32.to_le_bytes())),
                GameFontWhy::SpriteUnreadable,
            ),
            (
                "テクスチャ 0",
                Box::new(move |m| m.put(a_at(4), &0u32.to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "x0 が整数でない",
                Box::new(move |m| m.put(a_at(8), &0.5f32.to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "x1 が NaN",
                Box::new(move |m| m.put(a_at(0x10), &f32::NAN.to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "y1 が負",
                Box::new(move |m| m.put(a_at(0x14), &(-1.0f32).to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "矩形が空",
                Box::new(move |m| m.put(a_at(0x10), &0.0f32.to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "矩形がテクスチャの外",
                Box::new(move |m| m.put(a_at(0x14), &129.0f32.to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "テクスチャの幅 0",
                Box::new(move |m| m.put(a_at(0x1C), &0.0f32.to_le_bytes())),
                GameFontWhy::SpriteBad,
            ),
            (
                "別のテクスチャ",
                Box::new(move |m| m.put(a_at(4), &0xBBBB_0000u32.to_le_bytes())),
                GameFontWhy::TextureDiffers,
            ),
            (
                "別のテクスチャの大きさ",
                Box::new(move |m| m.put(a_at(0x18), &256.0f32.to_le_bytes())),
                GameFontWhy::TextureDiffers,
            ),
        ];
        for (label, edit, want) in cases {
            let mut m = loaded_world();
            edit(&mut m);
            assert_eq!(probe(&mut m).err(), Some(want), "{label}");
        }
        let mut m = loaded_world();
        m.regions
            .iter_mut()
            .find(|r| r.0 == SPRITES)
            .unwrap()
            .1
            .truncate(0x5E * 0x44);
        assert_eq!(probe(&mut m).err(), Some(GameFontWhy::SpriteUnreadable));
        assert!(probe(&mut loaded_world()).is_ok());
    }

    #[test]
    fn game_font_why_round_trips() {
        for code in 0..40u32 {
            if let Some(w) = GameFontWhy::from_code(code) {
                assert_eq!(w as u32, code);
                assert!(!w.text().is_empty());
            }
        }
        assert_eq!(GameFontWhy::from_code(0), Some(GameFontWhy::None));
        assert_eq!(GameFontWhy::from_code(30), Some(GameFontWhy::ApplyFailed));
        assert_eq!(GameFontWhy::from_code(11), None);
        assert!(
            GameFontWhy::NoOwnTexture as u32 <= 10 && GameFontWhy::LevelDescFailed as u32 >= 20
        );
    }

    #[test]
    fn install_opens_only_when_everything_is_ready() {
        let on = "[overlay]\nenable = 1\n".to_string();
        let cases: [(&str, FakeOverlay, bool, bool, &[&str]); 7] = [
            ("ini が無い", FakeOverlay::default(), true, false, &["ini"]),
            (
                "enable = 0",
                FakeOverlay {
                    ini: Some("[overlay]\nenable = 0\n".into()),
                    ..Default::default()
                },
                true,
                false,
                &["ini"],
            ),
            (
                "読めない値",
                FakeOverlay {
                    ini: Some("[overlay]\nenable = 2\n".into()),
                    ..Default::default()
                },
                true,
                false,
                &["ini"],
            ),
            (
                "detour が無い",
                FakeOverlay {
                    ini: Some(on.clone()),
                    ..Default::default()
                },
                false,
                false,
                &["ini"],
            ),
            (
                "文字の表が作れない",
                FakeOverlay {
                    ini: Some(on.clone()),
                    atlas_fail: true,
                    ..Default::default()
                },
                true,
                false,
                &["ini", "atlas"],
            ),
            (
                "置けない",
                FakeOverlay {
                    ini: Some(on.clone()),
                    arm_fail: true,
                    ..Default::default()
                },
                true,
                false,
                &["ini", "atlas", "arm"],
            ),
            (
                "そろった",
                FakeOverlay {
                    ini: Some(on.clone()),
                    ..Default::default()
                },
                true,
                true,
                &["ini", "atlas", "arm", "on"],
            ),
        ];
        for (label, mut f, draw, want, events) in cases {
            let (r, log) = run(&mut f, draw);
            assert_eq!(r, want, "{label}: {log}");
            assert_eq!(f.events, events, "{label}");
            assert!(log.starts_with("文字の描画: "), "{label}: {log}");
        }
    }

    #[test]
    fn the_m1c2_sample_ini_reads_as_documented() {
        let text = include_str!("../../../scripts/m1c2_overlay.ini");
        let ini = higanr_core::ini::parse(text).expect("見本の ini");
        assert_eq!(ov::enabled_in_ini(&ini), Ok(true));
        assert_eq!(crate::net_config::from_text(text), Ok(None));
        assert!(!ini.has_section("pacing"));
    }

    #[test]
    fn the_preview_sample_ini_reads_as_documented() {
        let text = include_str!("../../../scripts/m1c2_overlay_preview.ini");
        let ini = higanr_core::ini::parse(text).expect("見本の ini");
        assert_eq!(ov::enabled_in_ini(&ini), Ok(true));
        assert_eq!(ov::preview_in_ini(&ini), Ok(true));
        assert_eq!(crate::net_config::from_text(text), Ok(None));
    }

    // ---- 読み取りのスレッドの実行（`session::run`）への繋ぎ ----

    type Events = std::rc::Rc<core::cell::RefCell<Vec<&'static str>>>;

    /// 呼ばれた順だけを外に残す偽の実装（`session::run` が中身を持つので、中を後から覗けない）。
    struct SpyOverlay(Events);

    impl OverlayHost for SpyOverlay {
        fn read_ini(&mut self) -> Result<Option<String>, String> {
            self.0.borrow_mut().push("read_ini");
            Ok(Some("[overlay]\nenable = 1\n".to_string()))
        }
        fn build_atlas(&mut self) -> Result<String, String> {
            self.0.borrow_mut().push("atlas");
            Ok("合成".to_string())
        }
        fn arm(&mut self) -> Result<(), String> {
            self.0.borrow_mut().push("arm");
            Ok(())
        }
        fn on(&mut self) {
            self.0.borrow_mut().push("on");
        }
        fn counters(&self) -> OverlayCounters {
            self.0.borrow_mut().push("counters");
            OverlayCounters::default()
        }
        fn timings(&self) -> TimingSnapshot {
            TimingSnapshot {
                total: 0,
                max: 0,
                recent: Vec::new(),
            }
        }
        fn qpf(&self) -> u64 {
            10_000_000
        }
    }

    struct OverlayOnlyScript(Events);

    impl crate::session::ScriptHost for OverlayOnlyScript {
        fn read_script(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn move_script(&mut self) -> Result<String, String> {
            unreachable!()
        }
        fn prepare_shots(&mut self) -> Result<(), String> {
            unreachable!()
        }
        fn arm(&mut self, _s: higanr_core::script::Script, _f: u32, _l: u32) -> Result<(), String> {
            unreachable!()
        }
        fn shots_on(&mut self) {}
        fn start(&mut self) -> Result<(), String> {
            unreachable!()
        }
        fn overlay_host(&mut self) -> Option<Box<dyn OverlayHost>> {
            self.0.borrow_mut().push("overlay_host");
            Some(Box::new(SpyOverlay(std::rc::Rc::clone(&self.0))))
        }
    }

    #[test]
    fn session_opens_the_overlay_after_the_draw_detour_and_finishes_it() {
        use crate::hook::tests::FakeHook;
        use crate::observe::tests::FakeRecord;
        use crate::probe::tests::Fake;
        use crate::th09_addr::{FORBIDDEN, HOOK_SITE, IMAGE_BASE, SITES, WINDOW_ADDR};
        for (installs, draw_ok) in [(true, true), (true, false), (false, true)] {
            let events: Events = Default::default();
            let mut env = Fake::all_match();
            if !installs {
                env.ident = crate::probe::IdentOutcome::Unreadable("合成".to_string());
            }
            let def = SITES.iter().find(|d| d.name == HOOK_SITE).unwrap();
            let mut h = FakeHook::new(2);
            h.mem_base = def.addr;
            h.mem[..5].copy_from_slice(def.expected);
            if draw_ok {
                let d = SITES
                    .iter()
                    .find(|d| d.name == "draw_before_endscene")
                    .unwrap();
                h.mem2_base = d.addr;
                h.mem2[..d.expected.len()].copy_from_slice(d.expected);
            }
            let mut rec = FakeRecord::new(Vec::new(), 0);
            let mut script = OverlayOnlyScript(std::rc::Rc::clone(&events));
            let mut out = Vec::new();
            let end = crate::session::run(
                &mut env,
                &mut h,
                &mut rec,
                &mut script,
                SITES,
                FORBIDDEN,
                IMAGE_BASE,
                crate::session::Sites {
                    hook_site: HOOK_SITE,
                    draw_site: "draw_before_endscene",
                    exit_site: "render_only_exit",
                },
                WINDOW_ADDR,
                &crate::ticks::COPY_TABLE,
                2,
                &mut out,
            );
            let log = String::from_utf8(out).unwrap();
            let label = format!("installs={installs} draw_ok={draw_ok}");
            match (installs, draw_ok) {
                (true, true) => {
                    assert_eq!(end, crate::session::End::Installed, "{log}");
                    assert_eq!(
                        *events.borrow(),
                        vec!["overlay_host", "read_ini", "atlas", "arm", "on", "counters"],
                        "{label}"
                    );
                    let on_at = log.find("文字の描画: 有効にした").expect("有効にした行");
                    let summary_at = log.find("計測の集計").expect("計測の行");
                    let finish_at = log.find("==== 文字の描画").expect("集計の行");
                    assert!(on_at < summary_at && summary_at < finish_at, "{log}");
                }
                (true, false) => {
                    assert_eq!(
                        *events.borrow(),
                        vec!["overlay_host", "read_ini"],
                        "{label}"
                    );
                    assert!(log.contains("detour が書けていないので描かない"), "{log}");
                    assert!(!log.contains("==== 文字の描画"), "{log}");
                }
                _ => {
                    assert_eq!(end, crate::session::End::NotInstalled);
                    assert!(events.borrow().is_empty(), "{:?}", events.borrow());
                }
            }
        }
    }

    #[test]
    fn finish_writes_counts_reason_and_distribution() {
        let mut f = FakeOverlay {
            counters: OverlayCounters {
                drawn: 120,
                lost: 3,
                fail: Fail::DeviceChanged as u32,
                fail_hr: 0x8876_086C,
                setup_ticks: 12_345,
                ..Default::default()
            },
            recent: (1..=100).map(|i| i * 10).collect(),
            ..Default::default()
        };
        let mut out = Vec::new();
        finish(&mut f, &mut out);
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("描いた 120 フレーム"), "{log}");
        assert!(log.contains("失われていて描かなかった 3"), "{log}");
        assert!(log.contains("1234.5 µs"), "{log}");
        assert!(log.contains("デバイスが作り直された"), "{log}");
        assert!(log.contains("0x8876086C"), "{log}");
        assert!(log.contains("p50 50.0"), "{log}");
        assert!(log.contains("p99 99.0"), "{log}");
        assert!(log.contains("最大 100.0 µs"), "{log}");
        let mut empty = FakeOverlay::default();
        let mut out = Vec::new();
        finish(&mut empty, &mut out);
        assert!(String::from_utf8(out).unwrap().contains("描いた件が 0 件"));
    }
}
