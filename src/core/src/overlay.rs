//! 画面の右下に文字を描くための pure な部分: 文字の表（アトラス）の割り付け、GDI が描いた面から
//! テクスチャの画素を作る変換、1 行の頂点（TL の 2D）の組み立て、表示する 1 行の文、ini の `[overlay]` の読み方。
//! D3D8 を呼ぶ層は `src/dll/src/overlay_win.rs`、受け渡しのバッファと読み取りのスレッドは `src/dll/src/overlay.rs`。
//!
//! `layout_line` / `layout_lines` はゲームのスレッドで毎フレーム呼ぶので、確保もパニックもしない
//! （出力は呼び出し側の固定の配列）。ほかはヒープを使ってよい。
//!
//! 文字の表は半角 1 枠・全角 2 枠（`glyph_cells`）で、文字は `UI_LINES` の文言から作る（`ui_glyphs`）。
//! 英数字（`0x21`〜`0x7E`、`` ` `` を除く）はゲーム自身の ascii.anm の文字の絵を 1 回だけ読み取って
//! 「ゲームの文字の枠」（`GameBlock`）へコピーして使う。コピーする前・コピーできない文字は GDI の枠のまま。
//! ゲームのメモリには触らない（読む側は `overlay_win.rs`）。
#![forbid(unsafe_code)]

use crate::ini::Ini;
use crate::shot::{
    D3DFMT_A1R5G5B5, D3DFMT_A8R8G8B8, D3DFMT_R5G6B5, D3DFMT_X1R5G5B5, D3DFMT_X8R8G8B8,
};

// ---- 頂点 ----

/// 1 頂点。D3D8 の `D3DFVF_XYZRHW | D3DFVF_DIFFUSE | D3DFVF_TEX1`（= `FVF`）の並びそのもの
/// （x・y・z・rhw の f32 × 4 → 色 ARGB の u32 → u・v の f32 × 2。28 バイト）。
/// 花映塚の描画も同じ形（`0x004396DC` / `0x0043B556` の `push $0x144` → `SetVertexShader`）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub rhw: f32,
    pub color: u32,
    pub u: f32,
    pub v: f32,
}

/// `D3DFVF_XYZRHW`（0x004）| `D3DFVF_DIFFUSE`（0x040）| `D3DFVF_TEX1`（0x100）。
pub const FVF: u32 = 0x004 | 0x040 | 0x100;
/// `DrawPrimitiveUP` の stride。
pub const VERTEX_SIZE: u32 = 28;
const _: () = assert!(core::mem::size_of::<Vertex>() == VERTEX_SIZE as usize);
const _: () = assert!(FVF == 0x144);

/// 受け渡しのバッファに入る文の長さの上限と、1 行に描く文字の数の上限（UTF-16 の単位の数。基本面の文字は 1 文字 1 単位、
/// 改行も 1 単位）。128 の根拠: 画面の 1 行に全角で約 45 文字入るので約 3 行ぶん。バッファは二重で 512 バイトで済み、
/// 毎フレーム 0 で初期化する頂点の配列（`MAX_VERTS_ALL` × 28 バイト）も約 22 KB に収まる。
pub const MAX_TEXT: usize = 128;
/// バッファの文を UTF-8 に直したときのバイト数の上限（1 単位は最大 3 バイト）。ゲームのスレッドがスタックに置く書き先の大きさ。
pub const MAX_TEXT_UTF8: usize = MAX_TEXT * 3;
/// 1 行に要る頂点の数の上限（文字ごとに 2 つの三角形 = 6 頂点、下地の四角形も 6 頂点）。
pub const MAX_VERTS: usize = (MAX_TEXT + 1) * 6;
/// 一度に出す行数の上限（多い分は後ろの行を落とす）。
pub const MAX_LINES: usize = 4;
/// 複数行で要る頂点の数の上限。文字の四角形は全部で `MAX_TEXT` 個まで、下地は行ごとに 1 個。
pub const MAX_VERTS_ALL: usize = (MAX_TEXT + MAX_LINES) * 6;

/// `text` を UTF-16 にして `out` へ書き、書いた単位の数を返す（書き手のスレッドで呼ぶ）。
/// 入りきらない分は落とす。**文字の途中では切らない**（基本面の外の文字は 2 単位で 1 文字）ので、いつも正しい UTF-16 になる。
/// 確保もパニックもしない。
pub fn encode_utf16_capped(text: &str, out: &mut [u16]) -> usize {
    let mut n = 0;
    for c in text.chars() {
        let Some(dst) = out.get_mut(n..n + c.len_utf16()) else {
            break;
        };
        c.encode_utf16(dst);
        n += dst.len();
    }
    n
}

/// UTF-16 の単位 `units` を UTF-8 にして `out` へ書き、書いたバイト数を返す（ゲームのスレッドがバッファの文を `&str` にする）。
/// 対になっていない代理の単位は `U+FFFD`（文字の表に無いので `?` で描かれる）。入りきらない分は文字の途中では切らず落とす。
/// 確保もパニックもしない。
pub fn utf16_to_utf8(units: &[u16], out: &mut [u8]) -> usize {
    let mut n = 0;
    for c in char::decode_utf16(units.iter().copied()) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        let Some(dst) = out.get_mut(n..n + c.len_utf8()) else {
            break;
        };
        c.encode_utf8(dst);
        n += dst.len();
    }
    n
}

// ---- 文字の表（アトラス）----

/// ASCII の表示できる文字（0x20〜0x7E の 95 文字）。小さい順（`slot_of` の二分探索の前提）。
pub const ASCII: [char; 95] = {
    let mut a = [' '; 95];
    let mut i = 0;
    while i < 95 {
        a[i] = (0x20 + i as u8) as char;
        i += 1;
    }
    a
};

/// テクスチャの一辺の上限（D3D8 のどの世代の板でも作れる大きさ）。
pub const MAX_TEX_SIDE: u32 = 1024;
/// 文字の表の 1 行に並べる半角の枠の数（`AtlasSpec::new` の既定。日本語の表は `fit` が選ぶ）。
pub const ATLAS_COLS: u32 = 16;
const FIT_COLS: [u32; 3] = [16, 32, 64];

/// 文字の幅（半角の枠の数）: ASCII の表示できる文字と半角カナ（U+FF61〜U+FF9F。East Asian Width が Halfwidth）は 1、
/// それ以外は 2。ゲームの ＭＳ ゴシック も半角 : 全角 = 1 : 2 の等幅。
pub fn glyph_cells(c: char) -> u32 {
    if matches!(c, ' '..='~' | '\u{FF61}'..='\u{FF9F}') || small_ascii(c).is_some() {
        1
    } else {
        2
    }
}

// ---- 小さい文字（観戦者の左下の行だけ）----

/// 小さい文字の枠の高さ・幅（画素。半角 = `ceil(高さ / 2)`）。観戦者の左下の行（最大 50 文字）が
/// `ui_layout::HUD_BOTTOM_TEXT_MAX_W`（273 画素）に収まる最大の送りが 5 画素（50 × 5 = 250）なので高さ 10・半角 5。
/// 数は `net_menu` の検査が文字の表から数える。
pub const SMALL_CELL_H: u32 = 10;
pub const SMALL_CELL_W: u32 = SMALL_CELL_H.div_ceil(2);
/// 小さい文字の鍵: ASCII の文字 `c`（`!`〜`~`）の小さい文字は、文字の表の私用領域の文字 `U+E000 + c` の枠に描く。
/// 枠（半角 7 × 14）の左上の `SMALL_CELL_W` × `SMALL_CELL_H` だけに墨があり、残りは透明。
const SMALL_KEY_BASE: u32 = 0xE000;

/// 空白は枠を持たない。
pub fn small_key(c: char) -> Option<char> {
    matches!(c, '!'..='~')
        .then(|| char::from_u32(SMALL_KEY_BASE + c as u32))
        .flatten()
}

pub fn small_ascii(key: char) -> Option<char> {
    let u = (key as u32).checked_sub(SMALL_KEY_BASE)?;
    let c = char::from_u32(u)?;
    matches!(c, '!'..='~').then_some(c)
}

/// 画面の文言。ここにある文字が文字の表に入る。**ここに無い文字は `?` で描かれる**ので、画面に新しい語を出すときは足す
/// （食い違いは検査 `ui_lines_are_all_in_the_glyph_table` が見る）。
/// コピー元: `src/dll/src/net_code.rs` のウィンドウの文と `src/loader/src/code_relay.rs` の `REFUSE_OWN`、画面の状態・メニューの語。
pub const UI_LINES: &[&str] = &[
    // 状態の語。
    "HiganRTool",
    "接続しました。",
    "接続しました。あなたは 1P です。",
    "接続しました。あなたは 2P です。",
    "待っています",
    "接続コード",
    "接続コードを貼ってください",
    "自分の接続コード",
    "相手の接続コード",
    "始める側",
    "返す側",
    "見るだけの側",
    "ホスト",
    "ゲスト",
    "1P",
    "2P",
    "遅延",
    "フレーム",
    "観戦",
    "再戦",
    "はい",
    "いいえ",
    // ローダーのウィンドウ・本ログの「窓の文」。
    "これは自分の接続コードです。相手から届いたコードを貼ってください。",
    "相手から届くコードを待っています。",
    "相手から返ってくるコードを待っています（自分のコードは 30 分で使えなくなります）。",
    "相手のコードは期限（30 分）が切れています。相手に新しいコードを頼んでください。",
    "相手のコードの文字が欠けているか、違っています。もう一度全部をコピーして、このウィンドウに貼ってください。",
    "相手のコードが空でした。相手から届いたコードを貼ってください。",
    "貼られたのは自分の接続コードです。相手から届いたコードを貼ってください。",
    "2 人とも「始める側」の設定です。どちらか一方が HiganRTool.ini の code_side を answer にして、起動し直してください。",
    "2 人とも「返す側」の設定です。どちらか一方が HiganRTool.ini の code_side を offer にして、起動し直してください。",
    "これは前回使用した返答コードです。上に出ている今回の自分のコードを相手に送り直してください。",
    "相手のコードが今回のものと合いません。相手に今のコードを送り直してもらってください。",
    "接続の準備に失敗しました（詳細はログを確認してください）。ゲームを閉じて起動し直してください。",
    "待っても使えるコードが届かなかったため、待機をやめました。もう一度試すときは、ゲームを閉じて起動し直してください。",
];

/// 見本の絵に描く数行（`\n` 区切り。日本語・ASCII・混ざった行）。受け渡しのバッファの `MAX_TEXT` 単位に入る。
pub const PREVIEW_TEXT: &str = "接続コード\n接続しました。あなたは 1P です。\n遅延 2 フレーム / 観戦 0 人\nHiganRTool 59.94fps ホスト";

/// ini の `[overlay] preview = 1` のとき右下に出す見本（3 行）。実機の撮影で「ゲームの文字」と日本語の並びを見るためのもの。
pub const PREVIEW_SCREEN_TEXT: &str = "接続コード\n遅延 2 フレーム\nHiganRTool 59.94fps";

// ---- ゲームの文字（ascii.anm の絵。英数字）----

/// ゲームの文字にする範囲の最初と最後（`!` と `~`）。空白 `0x20` は絵を持たない（`0x00434612 cmp $0x20,%al; je`）。
/// `` ` ``（`0x60`）は範囲から外し、GDI の文字のまま。
pub const GAME_FIRST: u32 = 0x21;
pub const GAME_LAST: u32 = 0x7E;
/// 範囲の文字の数（`0x21`〜`0x7E`）。`` ` `` も数に入れて欠番にする（枠の番号 = 文字 − `0x21`）。
pub const GAME_GLYPHS: usize = 94;
/// ゲームの文字の枠 1 つの最大の大きさ（画素）と、枠を並べる 1 行の数。文字の大きさは実行時（ascii.anm を読んだとき）に
/// 分かるので、表を作る時点では最大の枠を予約し、越えたらコピーしない。仮の値（実測前）。
pub const GAME_SLOT_W: u32 = 16;
pub const GAME_SLOT_H: u32 = 16;
pub const GAME_BLOCK_COLS: u32 = 16;
/// ゲームの文字の送り（画素）。AsciiManager の `+0x8280`（`0x004344BF push $9` → `0x004343C0` が書く）。
/// 文字の列を描く関数は文字ごとに `+0x8280 × 拡大` 進む（`0x00434585 fildl 0x8280(%esi)`）。
pub const GAME_ADVANCE: u32 = 9;
/// ゲームの文字の縦の位置: 行の枠（`cell_h`）の下端に文字の絵の下端を揃え、そこからこの値（画素、正で下）だけずらす。仮の値。
pub const GAME_BASELINE_SHIFT: i32 = 0;

/// `c` がゲームの文字の枠を持てるなら、その枠の番号（`c − 0x21`）。
pub fn game_index(c: char) -> Option<usize> {
    let u = c as u32;
    ((GAME_FIRST..=GAME_LAST).contains(&u) && c != '`').then(|| (u - GAME_FIRST) as usize)
}

/// 文字の表に入れる文字（小さい順・重複なし）: ASCII の 95 文字と `UI_LINES` の文字。改行は入れない。
pub fn ui_glyphs() -> Vec<char> {
    let mut v: Vec<char> = ASCII.to_vec();
    // ゲーム内のメニューの固定の文（`net_menu::TEXTS`。日本語と英語の両方）も同じ表に入れる。
    v.extend(
        UI_LINES
            .iter()
            .copied()
            .chain(crate::net_menu::TEXTS.iter().flat_map(|t| t.both()))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_control()),
    );
    // 小さい文字の鍵（`SMALL_CELL_H`）。
    v.extend(ASCII.iter().filter_map(|&c| small_key(c)));
    v.sort_unstable();
    v.dedup();
    v
}

/// 文字の表の 1 つの枠の置き場所（画素）と幅（半角の枠の数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placed {
    x: u32,
    y: u32,
    cells: u32,
}

/// 文字の表の割り付け。枠の高さは全部同じで、幅は半角 1 枠（`cell_w`）か全角 2 枠。左上から横に `cols` 枠ぶんずつ文字の順に
/// 詰める（全角が行の終わりに入らなければ次の行へ）。文字の枠の後ろに全面不透明の「塗り」の枠（半角 1 枠）を 1 つ置く
/// （下地の四角形が貼る。テクスチャを替えずに 1 回の `DrawPrimitiveUP` で下地と文字を描くため）。
/// ASCII だけの表は、文字 `i` が `i` 番目の枠に入る。
///
/// `fit_game` で作った表は、文字の枠の下に「ゲームの文字の枠」の塊（`GameBlock`。`GAME_SLOT_W × GAME_SLOT_H` の枠を
/// `GAME_BLOCK_COLS` 列）を予約する。`apply_game_font` で絵をコピーする（`game` が `Some`）まで使わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasSpec {
    pub cell_w: u32,
    pub cell_h: u32,
    pub cols: u32,
    pub rows: u32,
    /// テクスチャの幅・高さ（2 の冪。古い板は 2 の冪以外を作れない）。
    pub tex_w: u32,
    pub tex_h: u32,
    pub glyphs: Vec<char>,
    /// 文字ごとの置き場所（`glyphs` と同じ順）。最後に「塗り」の枠。
    placed: Vec<Placed>,
    pub game_block: Option<GameBlock>,
    /// 絵をコピーした後のゲームの文字。
    pub game: Option<GameFont>,
}

/// ゲームの文字の枠の塊の置き場所（画素）。枠 `i` は左上から `cols` 個ずつ横に並べた `i` 番目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameBlock {
    pub x: u32,
    pub y: u32,
    pub cols: u32,
    pub slot_w: u32,
    pub slot_h: u32,
}

/// コピーしたゲームの文字の大きさと送り。`sizes[i]` は枠 `i` の絵の大きさ（幅 0 は「コピーしていない = GDI の文字」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameFont {
    pub advance: u32,
    pub sizes: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameGlyph {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl AtlasSpec {
    pub fn new(cell_w: u32, cell_h: u32, glyphs: &[char]) -> Result<AtlasSpec, String> {
        AtlasSpec::with_cols(cell_w, cell_h, glyphs, ATLAS_COLS)
    }

    /// 枠が 0・大きすぎる・`cols` が 2 未満・テクスチャが上限を越える・表が小さい順でない（同じ文字が 2 つある）ときは断る。
    pub fn with_cols(
        cell_w: u32,
        cell_h: u32,
        glyphs: &[char],
        cols: u32,
    ) -> Result<AtlasSpec, String> {
        AtlasSpec::build(cell_w, cell_h, glyphs, cols, false)
    }

    fn build(
        cell_w: u32,
        cell_h: u32,
        glyphs: &[char],
        cols: u32,
        reserve_game: bool,
    ) -> Result<AtlasSpec, String> {
        if cell_w == 0 || cell_h == 0 || cell_w > 64 || cell_h > 64 {
            return Err(format!("文字の枠 {cell_w}×{cell_h} は 1〜64 の外"));
        }
        if cols < 2 {
            return Err(format!(
                "1 行の枠の数 {cols} が 2 未満（全角の文字が入らない）"
            ));
        }
        if glyphs.is_empty() || glyphs.windows(2).any(|w| w[0] >= w[1]) {
            return Err("文字の表が空か、小さい順に並んでいない".to_string());
        }
        let mut placed = Vec::with_capacity(glyphs.len() + 1);
        let (mut col, mut row) = (0u32, 0u32);
        for cells in glyphs
            .iter()
            .map(|&c| glyph_cells(c))
            .chain(core::iter::once(1))
        {
            if col + cells > cols {
                col = 0;
                row += 1;
            }
            placed.push(Placed {
                x: col * cell_w,
                y: row * cell_h,
                cells,
            });
            col += cells;
        }
        let rows = row + 1;
        // ゲームの文字の塊は文字の枠の行の真下に置く。塊の幅が広ければテクスチャの幅を広げる。
        let block = reserve_game.then_some(GameBlock {
            x: 0,
            y: rows * cell_h,
            cols: GAME_BLOCK_COLS,
            slot_w: GAME_SLOT_W,
            slot_h: GAME_SLOT_H,
        });
        let (block_w, block_bottom) = block.map_or((0, 0), |b| {
            (
                b.cols * b.slot_w,
                b.y + (GAME_GLYPHS as u32).div_ceil(b.cols) * b.slot_h,
            )
        });
        let tex_w = (cols * cell_w).max(block_w).next_power_of_two();
        let tex_h = (rows * cell_h).max(block_bottom).next_power_of_two();
        if tex_w > MAX_TEX_SIDE || tex_h > MAX_TEX_SIDE {
            return Err(format!(
                "文字の表のテクスチャ {tex_w}×{tex_h} が上限 {MAX_TEX_SIDE} を越える"
            ));
        }
        Ok(AtlasSpec {
            cell_w,
            cell_h,
            cols,
            rows,
            tex_w,
            tex_h,
            glyphs: glyphs.to_vec(),
            placed,
            game_block: block,
            game: None,
        })
    }

    /// 1 行の枠の数を `FIT_COLS` から選ぶ: テクスチャの面積が最小のもの（同じなら辺の長い方が短いもの）。
    /// 全部断られたら最初の理由を返す。
    pub fn fit(cell_w: u32, cell_h: u32, glyphs: &[char]) -> Result<AtlasSpec, String> {
        AtlasSpec::fit_with(cell_w, cell_h, glyphs, false)
    }

    /// `fit` に、ゲームの文字の枠の塊を予約した表。絵はコピーしていない（`game` は `None`）。
    pub fn fit_game(cell_w: u32, cell_h: u32, glyphs: &[char]) -> Result<AtlasSpec, String> {
        AtlasSpec::fit_with(cell_w, cell_h, glyphs, true)
    }

    fn fit_with(
        cell_w: u32,
        cell_h: u32,
        glyphs: &[char],
        reserve_game: bool,
    ) -> Result<AtlasSpec, String> {
        let mut best: Option<AtlasSpec> = None;
        let mut first_err = None;
        for cols in FIT_COLS {
            match AtlasSpec::build(cell_w, cell_h, glyphs, cols, reserve_game) {
                Ok(s) => {
                    let key = |s: &AtlasSpec| (s.tex_w * s.tex_h, s.tex_w.max(s.tex_h));
                    if best.as_ref().is_none_or(|b| key(&s) < key(b)) {
                        best = Some(s);
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        best.ok_or_else(|| first_err.unwrap_or_else(|| "文字の表を割り付けられない".to_string()))
    }

    pub fn solid_slot(&self) -> u32 {
        self.glyphs.len() as u32
    }

    /// 表に無ければ `None`。確保しない。
    pub fn slot_of(&self, c: char) -> Option<u32> {
        self.glyphs.binary_search(&c).ok().map(|i| i as u32)
    }

    /// 枠が無ければ (0, 0)。
    pub fn cell_origin(&self, slot: u32) -> (u32, u32) {
        self.placed
            .get(slot as usize)
            .map_or((0, 0), |p| (p.x, p.y))
    }

    /// 半角の枠の数（1 か 2）。枠が無ければ 1。
    pub fn slot_cells(&self, slot: u32) -> u32 {
        self.placed.get(slot as usize).map_or(1, |p| p.cells)
    }

    pub fn texel_count(&self) -> usize {
        self.tex_w as usize * self.tex_h as usize
    }

    /// 枠 `index` の左上の画素の位置。塊を予約していない表・範囲の外なら `None`。
    pub fn game_slot_origin(&self, index: usize) -> Option<(u32, u32)> {
        let b = self.game_block?;
        (index < GAME_GLYPHS).then(|| {
            let i = index as u32;
            (b.x + (i % b.cols) * b.slot_w, b.y + (i / b.cols) * b.slot_h)
        })
    }

    /// 絵をコピーしていない表・範囲の外の文字（`` ` `` を含む）・コピーしていない文字なら `None`（GDI の文字で描く）。確保しない。
    pub fn game_glyph(&self, c: char) -> Option<GameGlyph> {
        let font = self.game.as_ref()?;
        let i = game_index(c)?;
        let &(w, h) = font.sizes.get(i)?;
        if w == 0 || h == 0 {
            return None;
        }
        let (x, y) = self.game_slot_origin(i)?;
        Some(GameGlyph { x, y, w, h })
    }

    /// 大きさの表が 94 文字ぶんでない・枠より大きい・1 つもコピーしていない・送りが 0 や大きすぎるときは断る。
    pub fn with_game_font(&self, font: GameFont) -> Result<AtlasSpec, String> {
        let b = self
            .game_block
            .ok_or("ゲームの文字の枠を予約していない表に、絵をコピーできない")?;
        if font.sizes.len() != GAME_GLYPHS {
            return Err(format!(
                "ゲームの文字の大きさの表が {} 文字ぶん（{GAME_GLYPHS} 文字ぶんのはず）",
                font.sizes.len()
            ));
        }
        if !(1..=64).contains(&font.advance) {
            return Err(format!("ゲームの文字の送り {} が 1〜64 の外", font.advance));
        }
        if let Some((i, &(w, h))) = font
            .sizes
            .iter()
            .enumerate()
            .find(|&(_, &(w, h))| w > b.slot_w || h > b.slot_h)
        {
            return Err(format!(
                "ゲームの文字（枠 {i}）の絵 {w}×{h} が枠 {}×{} より大きい",
                b.slot_w, b.slot_h
            ));
        }
        if font.sizes.iter().all(|&(w, h)| w == 0 || h == 0) {
            return Err("ゲームの文字が 1 つもコピーされていない".to_string());
        }
        let mut s = self.clone();
        s.game = Some(font);
        Ok(s)
    }
}

/// GDI が描いた 32 ビットの面（1 画素 B,G,R,X。上の行から、行の送りは幅 × 4）から文字の濃さ（0〜255）を取る。
/// ClearType の色つきの縁取りでも濃さが欠けないよう、3 成分の最大を濃さにする。
pub fn coverage_from_bgrx(bits: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let n = (w as usize)
        .checked_mul(h as usize)
        .ok_or("面の大きさが大きすぎる")?;
    let need = n.checked_mul(4).ok_or("面の大きさが大きすぎる")?;
    if bits.len() < need {
        return Err(format!(
            "面のバイト数 {} が {w}×{h}×4 = {need} に足りない",
            bits.len()
        ));
    }
    Ok(bits[..need]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[0].max(p[1]).max(p[2]))
        .collect())
}

/// 濃さの面を縦横 1 / `factor` に縮める（平均、四捨五入）。ゲームは文字を 2 倍で描いて縮めて使う
/// （`CreateFontA(2h, …)` —— `0x004364C0`）ので、それに合わせる。割り切れない・長さが合わないときは断る。
pub fn downscale_coverage(src: &[u8], w: u32, h: u32, factor: u32) -> Result<Vec<u8>, String> {
    if factor == 0 || !w.is_multiple_of(factor) || !h.is_multiple_of(factor) {
        return Err(format!("面 {w}×{h} を 1/{factor} に縮められない"));
    }
    let need = (w as usize)
        .checked_mul(h as usize)
        .ok_or("面の大きさが大きすぎる")?;
    if src.len() != need {
        return Err(format!("濃さの画素の数 {} が {w}×{h} と違う", src.len()));
    }
    let (ow, oh, f) = (
        (w / factor) as usize,
        (h / factor) as usize,
        factor as usize,
    );
    let area = (f * f) as u32;
    let mut out = Vec::with_capacity(ow * oh);
    for oy in 0..oh {
        for ox in 0..ow {
            let mut sum = 0u32;
            for dy in 0..f {
                let row = (oy * f + dy) * w as usize + ox * f;
                sum += src[row..row + f].iter().map(|&v| v as u32).sum::<u32>();
            }
            out.push(((sum + area / 2) / area) as u8);
        }
    }
    Ok(out)
}

/// 濃さからテクスチャの画素（`D3DFMT_A8R8G8B8`。色は白、濃さを α）を作り、「塗り」の枠を α = 255 で埋める。
/// 文字の色と下地の色は頂点の色で決める。
pub fn texels(spec: &AtlasSpec, coverage: &[u8]) -> Result<Vec<u32>, String> {
    let n = spec.texel_count();
    if coverage.len() != n {
        return Err(format!(
            "濃さの画素の数 {} がテクスチャ {}×{} と違う",
            coverage.len(),
            spec.tex_w,
            spec.tex_h
        ));
    }
    let mut t: Vec<u32> = coverage
        .iter()
        .map(|&a| (a as u32) << 24 | 0x00FF_FFFF)
        .collect();
    let (ox, oy) = spec.cell_origin(spec.solid_slot());
    for y in oy..oy + spec.cell_h {
        for x in ox..ox + spec.cell_w {
            if let Some(p) = t.get_mut(y as usize * spec.tex_w as usize + x as usize) {
                *p = 0xFFFF_FFFF;
            }
        }
    }
    Ok(t)
}

/// 2 枠の図形の記号（U+25A0〜U+25FF。◀ ▶ など）のうち墨の幅が枠の半分までのものを、墨ごと枠の横の真ん中へ動かす。
/// GDI はどの文字も枠の左端から描くので、細い記号は右に空きが残り「◀ 値 ▶」の値が右へ寄って見える。
/// 「。」「、」「」」のように左へ寄っているのが正しい形の文字があるので、図形の記号だけを動かす。
pub fn center_narrow_wide_glyphs(spec: &AtlasSpec, coverage: &mut [u8]) -> Result<(), String> {
    if coverage.len() != spec.texel_count() {
        return Err(format!(
            "濃さの画素の数 {} がテクスチャ {}×{} と違う",
            coverage.len(),
            spec.tex_w,
            spec.tex_h
        ));
    }
    let tw = spec.tex_w as usize;
    let (cw, ch) = (spec.cell_w as usize * 2, spec.cell_h as usize);
    for (slot, &c) in spec.glyphs.iter().enumerate() {
        let slot = slot as u32;
        if spec.slot_cells(slot) != 2 || !('\u{25A0}'..='\u{25FF}').contains(&c) {
            continue;
        }
        let (ox, oy) = spec.cell_origin(slot);
        let (ox, oy) = (ox as usize, oy as usize);
        if ox + cw > tw || (oy + ch) * tw > coverage.len() {
            continue;
        }
        let inked = |x: usize| (oy..oy + ch).any(|y| coverage[y * tw + ox + x] != 0);
        let Some(left) = (0..cw).find(|&x| inked(x)) else {
            continue;
        };
        let right = (0..cw).rev().find(|&x| inked(x)).unwrap_or(left);
        let ink = right - left + 1;
        let to = (cw - ink) / 2;
        if ink * 2 > cw || to <= left {
            continue;
        }
        let shift = to - left;
        for y in oy..oy + ch {
            let row = &mut coverage[y * tw + ox..y * tw + ox + cw];
            row.copy_within(left..=right, left + shift);
            row[left..left + shift].fill(0);
        }
    }
    Ok(())
}

// ---- ゲームの文字の絵（ascii.anm）を読み取って文字の表へコピーする ----

/// ゲームが ascii.anm のテクスチャに使いうる形式の番号（`D3DFORMAT`）。ゲームの形式の表 `0x00490F58` は
/// `[0, 21, 25, 23, 20, 26]`（th09.exe の実バイト）。番号 0 は D3DX に任せる（実際の形式は `GetLevelDesc`）。
/// ほかの形式は `shot` の定数。
pub const D3DFMT_R8G8B8: u32 = 20;
pub const D3DFMT_A4R4G4B4: u32 = 26;

/// テクスチャの形式ごとの 1 画素のバイト数。コピーできない形式は `None`。
pub fn texture_pixel_bytes(fmt: u32) -> Option<usize> {
    match fmt {
        D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8 => Some(4),
        D3DFMT_R8G8B8 => Some(3),
        D3DFMT_R5G6B5 | D3DFMT_X1R5G5B5 | D3DFMT_A1R5G5B5 | D3DFMT_A4R4G4B4 => Some(2),
        _ => None,
    }
}

pub fn texture_format_name(fmt: u32) -> String {
    match fmt {
        D3DFMT_R8G8B8 => "R8G8B8".to_string(),
        D3DFMT_A8R8G8B8 => "A8R8G8B8".to_string(),
        D3DFMT_X8R8G8B8 => "X8R8G8B8".to_string(),
        D3DFMT_R5G6B5 => "R5G6B5".to_string(),
        D3DFMT_X1R5G5B5 => "X1R5G5B5".to_string(),
        D3DFMT_A1R5G5B5 => "A1R5G5B5".to_string(),
        D3DFMT_A4R4G4B4 => "A4R4G4B4".to_string(),
        n => format!("D3DFMT {n}"),
    }
}

/// 1 画素（形式ごとの生バイト。小さい端の並び）を ARGB（`0xAARRGGBB`）にする。α を持たない形式は 255。
/// 5・6・4 ビットは上位ビットを下へ繰り返して 8 ビットへ広げる（4 ビットは `v × 17`）。
/// `px` が足りない・知らない形式なら `None`。
pub fn argb_of_pixel(fmt: u32, px: &[u8]) -> Option<u32> {
    let e5 = |v: u32| (v << 3) | (v >> 2);
    let e6 = |v: u32| (v << 2) | (v >> 4);
    let pack = |a: u32, r: u32, g: u32, b: u32| a << 24 | r << 16 | g << 8 | b;
    let word = || Some(u32::from(*px.first()?) | u32::from(*px.get(1)?) << 8);
    match fmt {
        D3DFMT_A8R8G8B8 => Some(pack(
            u32::from(*px.get(3)?),
            u32::from(*px.get(2)?),
            u32::from(*px.get(1)?),
            u32::from(*px.first()?),
        )),
        D3DFMT_X8R8G8B8 => Some(pack(
            255,
            u32::from(*px.get(2)?),
            u32::from(*px.get(1)?),
            u32::from(*px.first()?),
        )),
        D3DFMT_R8G8B8 => Some(pack(
            255,
            u32::from(*px.get(2)?),
            u32::from(*px.get(1)?),
            u32::from(*px.first()?),
        )),
        D3DFMT_R5G6B5 => {
            let v = word()?;
            Some(pack(255, e5(v >> 11 & 31), e6(v >> 5 & 63), e5(v & 31)))
        }
        D3DFMT_X1R5G5B5 | D3DFMT_A1R5G5B5 => {
            let v = word()?;
            let a = if fmt == D3DFMT_A1R5G5B5 && v & 0x8000 == 0 {
                0
            } else {
                255
            };
            Some(pack(a, e5(v >> 10 & 31), e5(v >> 5 & 31), e5(v & 31)))
        }
        D3DFMT_A4R4G4B4 => {
            let v = word()?;
            Some(pack(
                (v >> 12 & 15) * 17,
                (v >> 8 & 15) * 17,
                (v >> 4 & 15) * 17,
                (v & 15) * 17,
            ))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// ロックしたテクスチャの生バイト `data`（行 `i` は `data[i × pitch ..]`）から、矩形 `r` の画素だけを ARGB にして
/// `out`（行の送り `out_stride` 画素）へ書く。**矩形の外は読まない**。矩形がテクスチャの外・知らない形式・
/// `pitch` が 1 行に満たない・`data` が最後の画素まで届かない・`out` が入らないときは何も書かずに断る。確保しない。
#[allow(clippy::too_many_arguments)] // 面の形と矩形と書き出し先を別々に渡すだけ
pub fn extract_rect_argb(
    fmt: u32,
    tex_w: u32,
    tex_h: u32,
    pitch: usize,
    data: &[u8],
    r: Rect,
    out: &mut [u32],
    out_stride: usize,
) -> Result<(), String> {
    let bpp = texture_pixel_bytes(fmt).ok_or_else(|| format!("知らない画素の形式 {fmt}"))?;
    let end_x = r.x.checked_add(r.w).filter(|&e| e <= tex_w);
    let end_y = r.y.checked_add(r.h).filter(|&e| e <= tex_h);
    if r.w == 0 || r.h == 0 || end_x.is_none() || end_y.is_none() {
        return Err(format!(
            "矩形 {}×{}（左上 {},{}）がテクスチャ {tex_w}×{tex_h} の外か、空",
            r.w, r.h, r.x, r.y
        ));
    }
    if (pitch as u64) < tex_w as u64 * bpp as u64 {
        return Err(format!(
            "pitch {pitch} が 1 行ぶん（{tex_w}×{bpp}）に満たない"
        ));
    }
    // 最後の画素の次のバイト（u64 で計算。32 ビットの usize を越えうる壊れた値を弾く）。
    let need =
        (r.y as u64 + r.h as u64 - 1) * pitch as u64 + (r.x as u64 + r.w as u64) * bpp as u64;
    if (data.len() as u64) < need {
        return Err(format!(
            "画素のバイト数 {} が矩形の最後の画素まで届かない（必要 {need}）",
            data.len()
        ));
    }
    let out_need = (r.h as u64 - 1) * out_stride as u64 + r.w as u64;
    if (out_stride as u64) < r.w as u64 || (out.len() as u64) < out_need {
        return Err(format!(
            "書き出し先（行の送り {out_stride}・{} 画素）に矩形 {}×{} が入らない",
            out.len(),
            r.w,
            r.h
        ));
    }
    for row in 0..r.h as usize {
        let src_row = (r.y as usize + row) * pitch + r.x as usize * bpp;
        for col in 0..r.w as usize {
            let at = src_row + col * bpp;
            let px = data
                .get(at..at + bpp)
                .and_then(|p| argb_of_pixel(fmt, p))
                .ok_or("画素を読めない")?;
            if let Some(o) = out.get_mut(row * out_stride + col) {
                *o = px;
            }
        }
    }
    Ok(())
}

/// ARGB の絵を `dw` × `dh` にする。縮めるときは元の画素のボックスの平均（色は α で重み付け。縁が黒ずまない）、
/// 広げるときは最近傍。大きさが 0・画素の数が合わないときは断る。
pub fn resample_argb(src: &[u32], sw: u32, sh: u32, dw: u32, dh: u32) -> Result<Vec<u32>, String> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() != sw as usize * sh as usize {
        return Err(format!(
            "縮尺の元 {sw}×{sh}（画素 {}）か先 {dw}×{dh} が変",
            src.len()
        ));
    }
    let mut out = Vec::with_capacity(dw as usize * dh as usize);
    for dy in 0..dh as u64 {
        let y0 = (dy * sh as u64 / dh as u64) as usize;
        let y1 = ((((dy + 1) * sh as u64).div_ceil(dh as u64)) as usize).max(y0 + 1);
        for dx in 0..dw as u64 {
            let x0 = (dx * sw as u64 / dw as u64) as usize;
            let x1 = ((((dx + 1) * sw as u64).div_ceil(dw as u64)) as usize).max(x0 + 1);
            let (mut n, mut sa, mut sr, mut sg, mut sb) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in y0..y1.min(sh as usize) {
                for x in x0..x1.min(sw as usize) {
                    let p = src[y * sw as usize + x] as u64;
                    let a = p >> 24 & 255;
                    n += 1;
                    sa += a;
                    sr += (p >> 16 & 255) * a;
                    sg += (p >> 8 & 255) * a;
                    sb += (p & 255) * a;
                }
            }
            out.push(if n == 0 || sa == 0 {
                0
            } else {
                let c = |s: u64| ((s + sa / 2) / sa) as u32;
                (((sa + n / 2) / n) as u32) << 24 | c(sr) << 16 | c(sg) << 8 | c(sb)
            });
        }
    }
    Ok(out)
}

/// 読み取った文字の絵 1 つ（ARGB の `w` × `h`、隙間なし）と、画面に描く大きさ `quad_w` × `quad_h`。
#[derive(Debug, Clone, Copy)]
pub struct GameGlyphSrc<'a> {
    pub c: char,
    pub w: u32,
    pub h: u32,
    pub pixels: &'a [u32],
    pub quad_w: u32,
    pub quad_h: u32,
}

/// 画面に描く大きさが読めない（0 か枠より大きい）ときは絵の大きさのまま。
fn game_draw_size(g: &GameGlyphSrc, b: &GameBlock) -> (u32, u32) {
    if (1..=b.slot_w).contains(&g.quad_w) && (1..=b.slot_h).contains(&g.quad_h) {
        (g.quad_w, g.quad_h)
    } else {
        (g.w, g.h)
    }
}

/// 予約した表 `spec`（と画素 `texels`）に、読み取った文字の絵をコピーした表と画素を返す（元は変えない）。絵が描く大きさ
/// と違えば `resample_argb` で合わせて枠へ置く。塊の全面は先に透明にする。範囲の外の文字・同じ文字が 2 つ・
/// 画素の数が合わない・枠より大きい・1 文字も無いときは断る。
pub fn apply_game_font(
    spec: &AtlasSpec,
    texels: &[u32],
    glyphs: &[GameGlyphSrc],
    advance: u32,
) -> Result<(AtlasSpec, Vec<u32>), String> {
    let b = spec
        .game_block
        .ok_or("ゲームの文字の枠を予約していない表に、絵をコピーできない")?;
    if texels.len() != spec.texel_count() {
        return Err(format!(
            "画素の数 {} が文字の表 {}×{} と違う",
            texels.len(),
            spec.tex_w,
            spec.tex_h
        ));
    }
    let mut out = texels.to_vec();
    let tw = spec.tex_w as usize;
    for y in b.y..(b.y + (GAME_GLYPHS as u32).div_ceil(b.cols) * b.slot_h).min(spec.tex_h) {
        let row = y as usize * tw;
        if let Some(r) = out.get_mut(row + b.x as usize..row + (b.x + b.cols * b.slot_w) as usize) {
            r.fill(0);
        }
    }
    let mut sizes = vec![(0u32, 0u32); GAME_GLYPHS];
    for g in glyphs {
        let i =
            game_index(g.c).ok_or_else(|| format!("文字 {:?} はゲームの文字の範囲の外", g.c))?;
        if sizes[i].0 != 0 {
            return Err(format!("文字 {:?} が 2 つある", g.c));
        }
        if g.w == 0 || g.h == 0 || g.pixels.len() != g.w as usize * g.h as usize {
            return Err(format!(
                "文字 {:?} の絵 {}×{}（画素 {}）が変",
                g.c,
                g.w,
                g.h,
                g.pixels.len()
            ));
        }
        let (dw, dh) = game_draw_size(g, &b);
        if dw > b.slot_w || dh > b.slot_h {
            return Err(format!(
                "文字 {:?} の絵 {dw}×{dh} が枠 {}×{} より大きい",
                g.c, b.slot_w, b.slot_h
            ));
        }
        let scaled;
        let px: &[u32] = if (dw, dh) == (g.w, g.h) {
            g.pixels
        } else {
            scaled = resample_argb(g.pixels, g.w, g.h, dw, dh)?;
            &scaled
        };
        let (ox, oy) = spec
            .game_slot_origin(i)
            .ok_or("ゲームの文字の枠の位置を出せない")?;
        for row in 0..dh as usize {
            let at = (oy as usize + row) * tw + ox as usize;
            let dst = out
                .get_mut(at..at + dw as usize)
                .ok_or("ゲームの文字の枠が文字の表の外")?;
            dst.copy_from_slice(&px[row * dw as usize..][..dw as usize]);
        }
        sizes[i] = (dw, dh);
    }
    let s = spec.with_game_font(GameFont { advance, sizes })?;
    Ok((s, out))
}

// ---- 1 行の頂点 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub text: u32,
    /// 下地の四角形の色（ARGB。α で透ける）。
    pub back: u32,
    pub pad: u32,
}

/// 右下の置き場の既定。下の余白 18 画素は、ゲーム自身の fps の表示と重ならないよう 1 行ぶん上げた値（表示の位置は測っていない）。
pub const PLACE_BOTTOM_RIGHT: Place = Place {
    right: 2,
    bottom: 18,
};
pub const STYLE_DEFAULT: Style = Style {
    text: 0xFFFF_FFFF,
    back: 0x9000_0000,
    pad: 2,
};

/// 四角形 1 つ（6 頂点）を `out[at..at + 6]` に書く。`(x0, y0)`〜`(x1, y1)` は画素の端の座標。
/// D3D8 の画素の中心は整数の座標なので、テクセルと画素を 1 対 1 に合わせるために 0.5 引く。
pub(crate) fn quad(out: &mut [Vertex], at: usize, px: [f32; 4], uv: [f32; 4], color: u32) -> bool {
    let Some(q) = out.get_mut(at..at + 6) else {
        return false;
    };
    let [x0, y0, x1, y1] = px.map(|p| p - 0.5);
    let [u0, v0, u1, v1] = uv;
    let v = |x: f32, y: f32, u: f32, v: f32| Vertex {
        x,
        y,
        z: 0.0,
        rhw: 1.0,
        color,
        u,
        v,
    };
    let (tl, tr, bl, br) = (
        v(x0, y0, u0, v0),
        v(x1, y0, u1, v0),
        v(x0, y1, u0, v1),
        v(x1, y1, u1, v1),
    );
    q.copy_from_slice(&[tl, tr, bl, bl, tr, br]);
    true
}

/// 枠 `slot` のテクスチャ座標（左・上・右・下）。全角の枠は幅が半角の 2 枠ぶん。
pub(crate) fn slot_uv(spec: &AtlasSpec, slot: u32) -> [f32; 4] {
    let (ox, oy) = spec.cell_origin(slot);
    let (tw, th) = (spec.tex_w.max(1) as f32, spec.tex_h.max(1) as f32);
    [
        ox as f32 / tw,
        oy as f32 / th,
        (ox + spec.slot_cells(slot) * spec.cell_w) as f32 / tw,
        (oy + spec.cell_h) as f32 / th,
    ]
}

pub(crate) fn rect_uv(spec: &AtlasSpec, x: u32, y: u32, w: u32, h: u32) -> [f32; 4] {
    let (tw, th) = (spec.tex_w.max(1) as f32, spec.tex_h.max(1) as f32);
    [
        x as f32 / tw,
        y as f32 / th,
        (x + w) as f32 / tw,
        (y + h) as f32 / th,
    ]
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Face {
    /// 何も描かない（空白の枠、表に無くて代わりの文字も無い）。
    None,
    Cell(u32),
    Game(GameGlyph),
}

/// 文字 `c` が行の中で進む幅（画素）と描く絵。表に無ければ `fallback`（`?`）、それも無ければ絵は無しで文字の種類の幅。
/// 描く文字の幅と進む幅は必ず一致する。ゲームの文字をコピーした表では、範囲の文字はゲームの文字で描いて `GAME_ADVANCE` 進み、
/// 空白も同じ送りで進む。
pub(crate) fn resolve(spec: &AtlasSpec, c: char, fallback: Option<u32>) -> (u32, Face) {
    let slot = spec.slot_of(c).or(fallback);
    let drawn = if spec.slot_of(c).is_some() { c } else { '?' };
    if slot.is_some()
        && let (Some(g), Some(font)) = (spec.game_glyph(drawn), spec.game.as_ref())
    {
        return (font.advance, Face::Game(g));
    }
    match (slot, spec.game.as_ref()) {
        (Some(_), Some(font)) if c == ' ' => (font.advance, Face::None),
        _ => resolve_plain(spec, c, fallback),
    }
}

/// ゲームの文字の絵を置く左端。絵は送り（`GameFont::advance`）より広く、左端を送りの枠に合わせると墨が右へ寄る
/// （GDI の文字と並べると「◀ 値 ▶」の値が右へ寄って見える）ので、絵の横の真ん中を送りの枠の真ん中に揃える。
/// ずらす量は整数の画素に切り捨てる（半端な位置に置くと文字の絵が画素の境目で崩れる）。
pub(crate) fn game_glyph_left(spec: &AtlasSpec, gx: f32, g: &GameGlyph) -> f32 {
    let advance = spec.game.as_ref().map_or(g.w, |f| f.advance);
    gx - (g.w.saturating_sub(advance) / 2) as f32
}

/// `resolve` の、ゲームの文字を使わないバージョン: GDI の文字の枠で描き（半角 `cell_w`・全角 2 × `cell_w`）、空白も枠の幅で進む。
/// ゲームの文字をコピーした表にも GDI の枠は残る（`apply_game_font` は枠の外の塊だけ書く）。ゲームの文字（9 画素送り）では
/// 上の枠の左に収まらないマッチ中の行（`net_menu::hud_line`）が使う。拡大縮小はしない。
pub(crate) fn resolve_plain(spec: &AtlasSpec, c: char, fallback: Option<u32>) -> (u32, Face) {
    match spec.slot_of(c).or(fallback) {
        Some(s) => (
            spec.slot_cells(s) * spec.cell_w,
            if c == ' ' { Face::None } else { Face::Cell(s) },
        ),
        None => (glyph_cells(c) * spec.cell_w, Face::None),
    }
}

/// `resolve_plain` の小さい文字のバージョン（`SMALL_CELL_W` 画素送り。文字の絵は鍵の枠の左上 `SMALL_CELL_W` × `SMALL_CELL_H`）。
/// 空白は枠を持たず同じ送りで進む。ASCII の外の文字・鍵が無い文字は `?` の小さい文字（それも無ければ送りだけ進む）。
pub(crate) fn resolve_small(spec: &AtlasSpec, c: char, _fallback: Option<u32>) -> (u32, Face) {
    if c == ' ' {
        return (SMALL_CELL_W, Face::None);
    }
    let slot = small_key(c)
        .and_then(|k| spec.slot_of(k))
        .or_else(|| small_key('?').and_then(|k| spec.slot_of(k)));
    match slot {
        Some(s) => {
            let (x, y) = spec.cell_origin(s);
            (
                SMALL_CELL_W,
                Face::Game(GameGlyph {
                    x,
                    y,
                    w: SMALL_CELL_W,
                    h: SMALL_CELL_H,
                }),
            )
        }
        None => (SMALL_CELL_W, Face::None),
    }
}

/// 1 行 `text` を画面（`screen_w` × `screen_h`）の右下に置き、下地と文字の四角形の頂点を `out` に書いて、
/// 書いた頂点の数（6 の倍数）を返す。`D3DPT_TRIANGLELIST` で描く。改行は扱わない（複数行は `layout_lines`）。
///
/// - 文字の数は `MAX_TEXT`・`out` に入る数・画面の幅に入る数のうち小さい方で切る（後ろを落とす）。
/// - 空白は四角形を置かずに 1 枠ぶん進む。表に無い文字は `?` で描く（進む幅は 1 枠）。
/// - 画面が狭くて下地が収まらない・`out` に下地も入らない・文字が 0 文字なら 0 を返す。
///
/// 確保もパニックもしない（毎フレーム呼ぶ）。
pub fn layout_line(
    text: &str,
    spec: &AtlasSpec,
    screen_w: u32,
    screen_h: u32,
    place: Place,
    style: &Style,
    out: &mut [Vertex],
) -> usize {
    let cap_by_out = (out.len() / 6).saturating_sub(1);
    let avail_w = screen_w
        .saturating_sub(place.right)
        .saturating_sub(2 * style.pad);
    let fallback = spec.slot_of('?');
    let limit = MAX_TEXT.min(cap_by_out);
    let (mut n, mut total) = (0usize, 0u32);
    for c in text.chars() {
        let (w, _) = resolve(spec, c, fallback);
        if n >= limit || total.saturating_add(w) > avail_w {
            break;
        }
        n += 1;
        total += w;
    }
    if n == 0 {
        return 0;
    }
    let box_w = total + 2 * style.pad;
    let box_h = spec.cell_h + 2 * style.pad;
    let (Some(x0), Some(y0)) = (
        screen_w.checked_sub(place.right + box_w),
        screen_h.checked_sub(place.bottom + box_h),
    ) else {
        return 0;
    };
    let (x1, y1) = (x0 + box_w, y0 + box_h);
    if !quad(
        out,
        0,
        [x0 as f32, y0 as f32, x1 as f32, y1 as f32],
        slot_uv(spec, spec.solid_slot()),
        style.back,
    ) {
        return 0;
    }
    let mut at = 6;
    let mut used = 0u32;
    for c in text.chars().take(n) {
        let (w, face) = resolve(spec, c, fallback);
        let gx = (x0 + style.pad + used) as f32;
        let row_top = (y0 + style.pad) as f32;
        let placed = match face {
            Face::None => None,
            Face::Cell(slot) => Some((
                [gx, row_top, gx + w as f32, row_top + spec.cell_h as f32],
                slot_uv(spec, slot),
            )),
            Face::Game(g) => {
                // 文字の絵の下端を行の枠の下端に揃える（`GAME_BASELINE_SHIFT`）。
                let bottom = (y0 + style.pad + spec.cell_h) as f32 + GAME_BASELINE_SHIFT as f32;
                let left = game_glyph_left(spec, gx, &g);
                Some((
                    [left, bottom - g.h as f32, left + g.w as f32, bottom],
                    rect_uv(spec, g.x, g.y, g.w, g.h),
                ))
            }
        };
        if let Some((px, uv)) = placed {
            if !quad(out, at, px, uv, style.text) {
                break;
            }
            at += 6;
        }
        used += w;
    }
    at
}

/// 複数行 `text`（`\n` 区切り。行末の `\r` は落とす）を右下から上へ積む: 最後の行が `place` の位置、前の行が
/// 行の高さ（文字の高さ + 下地の余白 × 2）ずつ上がる。行ごとに下地を持つ（右に揃える）。行は `MAX_LINES` まで。
/// 頂点は 1 つの配列 `out` に続けて書く。空の行は行の高さだけ空ける。確保もパニックもしない（毎フレーム呼ぶ）。
pub fn layout_lines(
    text: &str,
    spec: &AtlasSpec,
    screen_w: u32,
    screen_h: u32,
    place: Place,
    style: &Style,
    out: &mut [Vertex],
) -> usize {
    let shown = text.split('\n').count().min(MAX_LINES);
    let step = spec.cell_h + 2 * style.pad;
    let mut at = 0;
    for (i, line) in text.split('\n').take(shown).enumerate() {
        let up = (shown - 1 - i) as u32;
        let p = Place {
            right: place.right,
            bottom: place.bottom.saturating_add(up.saturating_mul(step)),
        };
        let Some(rest) = out.get_mut(at..) else {
            break;
        };
        let line = line.strip_suffix('\r').unwrap_or(line);
        at += layout_line(line, spec, screen_w, screen_h, p, style, rest);
    }
    at
}

// ---- 見本の絵（検査用。ゲームには使わない）----

/// 頂点を文字の表の画素 `texels`（`tex_w` × `tex_h`）を貼って RGB8 の面 `canvas`（`cw` × `ch`）に重ねる
/// （D3D8 の描画の CPU バージョン: 点で拾う・頂点の色 × テクスチャ・α で重ねる）。`(dx, dy)` は面の上の原点。
/// はみ出す分は描かない。面に描いた四角形の数を返す。
#[allow(clippy::too_many_arguments)] // 面・貼る元・置き場所を別々に渡すだけ
pub fn blit_quads(
    verts: &[Vertex],
    texels: &[u32],
    tex_w: u32,
    tex_h: u32,
    canvas: &mut [u8],
    cw: u32,
    ch: u32,
    dx: i32,
    dy: i32,
) -> usize {
    if texels.len() != tex_w as usize * tex_h as usize
        || canvas.len() != cw as usize * ch as usize * 3
    {
        return 0;
    }
    let mut drawn = 0;
    for q in verts.as_chunks::<6>().0 {
        // 四角形は [tl, tr, bl, bl, tr, br]。
        let (tl, br) = (q[0], q[5]);
        let px = |v: f32| (v + 0.5).round() as i32;
        let (x0, y0, x1, y1) = (px(tl.x), px(tl.y), px(br.x), px(br.y));
        let (u0, v0) = (
            (tl.u * tex_w as f32).round() as i64,
            (tl.v * tex_h as f32).round() as i64,
        );
        let (u1, v1) = (
            (br.u * tex_w as f32).round() as i64,
            (br.v * tex_h as f32).round() as i64,
        );
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let (qw, qh) = ((x1 - x0) as i64, (y1 - y0) as i64);
        let color = tl.color;
        let ca = (color >> 24) & 255;
        let mut any = false;
        // 面に入る範囲だけを走る。
        for y in y0.max(-dy)..y1.min(ch as i32 - dy) {
            for x in x0.max(-dx)..x1.min(cw as i32 - dx) {
                let (cx, cy) = (x + dx, y + dy);
                // 四角形の画素の中心を、テクスチャの画素の位置に対応させる（1 対 1 なら同じ位置）。
                let tx = u0 + ((x - x0) as i64 * 2 + 1) * (u1 - u0) / (qw * 2);
                let ty = v0 + ((y - y0) as i64 * 2 + 1) * (v1 - v0) / (qh * 2);
                let Some(&t) = texels
                    .get(usize::try_from(ty * tex_w as i64 + tx).unwrap_or(usize::MAX))
                    .filter(|_| tx >= 0 && ty >= 0 && tx < tex_w as i64 && ty < tex_h as i64)
                else {
                    continue;
                };
                let a = ((t >> 24) * ca + 127) / 255;
                let at = (cy as usize * cw as usize + cx as usize) * 3;
                for (k, shift) in [16u32, 8, 0].into_iter().enumerate() {
                    let s = ((t >> shift & 255) * (color >> shift & 255) + 127) / 255;
                    let d = canvas[at + k] as u32;
                    canvas[at + k] = ((d * (255 - a) + s * a + 127) / 255) as u8;
                }
                any = true;
            }
        }
        if any {
            drawn += 1;
        }
    }
    drawn
}

/// 見本の絵の幅と、文字の表 1 つぶんの区画の高さ。
pub const PREVIEW_SHEET_W: u32 = 640;
pub const PREVIEW_BLOCK_H: u32 = 160;
const PREVIEW_ASCII_ROWS: [&str; 2] = [
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz",
    "0123456789 !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
];

/// 文字の表を並べた見本の絵（RGB8）を作る。表ごとに 1 区画で、右下に `PREVIEW_TEXT`、その上に ASCII の 2 行、
/// いちばん上に見出し `label` を、実機と同じ頂点の組み立てと CPU の描画で描く。
pub fn preview_sheet_rgb(variants: &[(&str, &AtlasSpec, &[u32])]) -> (u32, u32, Vec<u8>) {
    let (w, bh) = (PREVIEW_SHEET_W, PREVIEW_BLOCK_H);
    let h = bh * variants.len() as u32;
    let mut img: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [
                (40 + x * 180 / w) as u8,
                (60 + (y % bh) * 120 / bh) as u8,
                110,
            ]
        })
        .collect();
    let mut v = vec![Vertex::default(); MAX_VERTS_ALL];
    for (k, (label, spec, texels)) in variants.iter().enumerate() {
        let dy = (k as u32 * bh) as i32;
        let rows: [(&str, u32); 4] = [
            (PREVIEW_TEXT, 2),
            (PREVIEW_ASCII_ROWS[1], 2 + 4 * (spec.cell_h + 4)),
            (PREVIEW_ASCII_ROWS[0], 2 + 5 * (spec.cell_h + 4)),
            (label, 2 + 6 * (spec.cell_h + 4)),
        ];
        for (text, bottom) in rows {
            let place = Place { right: 2, bottom };
            let n = layout_lines(text, spec, w, bh, place, &STYLE_DEFAULT, &mut v);
            blit_quads(
                &v[..n],
                texels,
                spec.tex_w,
                spec.tex_h,
                &mut img,
                w,
                h,
                0,
                dy,
            );
        }
    }
    (w, h, img)
}

/// RGB8 の面を縦横 `factor` 倍にする（最近傍）。大きさが合わないときは空。
pub fn upscale_rgb(rgb: &[u8], w: u32, h: u32, factor: u32) -> Vec<u8> {
    if factor == 0 || rgb.len() != w as usize * h as usize * 3 {
        return Vec::new();
    }
    let f = factor as usize;
    let mut out = Vec::with_capacity(rgb.len() * f * f);
    for row in rgb.chunks_exact(w as usize * 3) {
        let mut line = Vec::with_capacity(row.len() * f);
        for px in row.as_chunks::<3>().0 {
            for _ in 0..f {
                line.extend_from_slice(px);
            }
        }
        for _ in 0..f {
            out.extend_from_slice(&line);
        }
    }
    out
}

/// 文字の表そのものの絵（RGB8）: 暗い地に文字の濃さを白で重ね、枠の境を薄い線で引く。
pub fn atlas_preview_rgb(spec: &AtlasSpec, texels: &[u32]) -> Vec<u8> {
    let (w, h) = (spec.tex_w as usize, spec.tex_h as usize);
    if texels.len() != w * h {
        return Vec::new();
    }
    const BG: [u32; 3] = [24, 24, 40];
    const EDGE: [u32; 3] = [70, 70, 110];
    let mut img = vec![0u8; w * h * 3];
    for (i, &t) in texels.iter().enumerate() {
        let a = t >> 24;
        for (k, bg) in BG.iter().enumerate() {
            img[i * 3 + k] = ((bg * (255 - a) + 255 * a + 127) / 255) as u8;
        }
    }
    for slot in 0..=spec.solid_slot() {
        let (ox, oy) = spec.cell_origin(slot);
        let (cw, chh) = (spec.slot_cells(slot) * spec.cell_w, spec.cell_h);
        for (x, y) in (0..cw)
            .map(|d| (ox + d, oy))
            .chain((0..chh).map(|d| (ox, oy + d)))
        {
            let i = (y as usize * w + x as usize) * 3;
            // 文字の濃い所は消さない（線は地の色のときだけ）。
            if let Some(px) = img.get_mut(i..i + 3)
                && *px == BG.map(|v| v as u8)
            {
                px.copy_from_slice(&EDGE.map(|v| v as u8));
            }
        }
    }
    img
}

// ---- 表示する 1 行 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    /// 描画の入口を通った回数の 1 秒あたり（× 100。`fps_centi`）。
    pub fps_centi: Option<u32>,
    /// 接続で決まった Latency。
    pub latency: Option<u8>,
}

/// 表示する 1 行（仮の文言）。`MAX_TEXT` 単位を越えない（文言は ASCII だけで 1 文字 1 単位）。
pub fn status_line(s: &Status) -> String {
    let mut t = String::from("HiganRTool");
    if let Some(f) = s.fps_centi {
        t.push_str(&format!(" {}.{:02}fps", f / 100, f % 100));
    }
    if let Some(l) = s.latency {
        t.push_str(&format!(" Latency {l}"));
    }
    t.truncate(MAX_TEXT);
    t
}

/// `frames` 回を `ticks`（QPC のカウント、周波数 `qpf`）で割った 1 秒あたりの回数 × 100（四捨五入）。時間か周波数が 0 なら `None`。
pub fn fps_centi(frames: u32, ticks: u64, qpf: u64) -> Option<u32> {
    if ticks == 0 || qpf == 0 {
        return None;
    }
    let num = frames as u128 * qpf as u128 * 100;
    let v = (num + ticks as u128 / 2) / ticks as u128;
    u32::try_from(v).ok()
}

// ---- ini ----

pub const SECTION: &str = "overlay";
/// 読めるキー。これ以外は断る（綴りの誤りで既定のまま走らないように）。
const KEYS: [&str; 2] = ["enable", "preview"];

/// `[overlay]` を読む。節か `enable` が無いか 0 なら `Ok(false)`（描かない。既定）、`enable = 1` なら `Ok(true)`。
/// 知らないキー・0 と 1 以外の値は断る。
pub fn enabled_in_ini(ini: &Ini) -> Result<bool, String> {
    if !ini.has_section(SECTION) {
        return Ok(false);
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} を知らない（読めるのは {}）",
                KEYS.join(" / ")
            ));
        }
    }
    match ini.get(SECTION, "enable") {
        Some("1") => Ok(true),
        Some("0") | None => Ok(false),
        Some(v) => Err(format!("enable の値 {v:?} は 0 か 1")),
    }
}

/// ini の `[netplay]` に `enable = 1` と `code_side = game` があるか（ゲーム内のメニュー）。あれば `[overlay] enable` が
/// 無くても文字の描画を開く（メニューの文字が出ないと使えない）。その場合 fps の行は出さない。
/// `[netplay]` の読み誤りは `net_config` が断る（ここは形だけ見る）。
pub fn game_ui_in_ini(ini: &Ini) -> bool {
    ini.get("netplay", "enable") == Some("1") && ini.get("netplay", "code_side") == Some("game")
}

/// `[overlay] preview = 1`（実機で見本を撮る旗）。読み方は `enabled_in_ini` と同じ。立っていると、ゲームの文字をコピーした後の
/// 文字の表と見本の行を PNG にして本ログの隣へ書き、右下に `PREVIEW_SCREEN_TEXT` を出す。
pub fn preview_in_ini(ini: &Ini) -> Result<bool, String> {
    enabled_in_ini(ini)?;
    match ini.get(SECTION, "preview") {
        Some("1") => Ok(true),
        Some("0") | None => Ok(false),
        Some(v) => Err(format!("preview の値 {v:?} は 0 か 1")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AtlasSpec {
        AtlasSpec::new(7, 14, &ASCII).unwrap()
    }

    #[test]
    fn ascii_atlas_layout() {
        assert_eq!(ASCII[0], ' ');
        assert_eq!(ASCII[94], '~');
        let s = spec();
        assert_eq!((s.cols, s.rows, s.tex_w, s.tex_h), (16, 6, 128, 128));
        assert_eq!(s.slot_of('A'), Some(0x41 - 0x20));
        assert_eq!(s.slot_of(' '), Some(0));
        assert_eq!(s.slot_of('\u{3042}'), None);
        assert_eq!(s.solid_slot(), 95);
        assert_eq!(s.cell_origin(0x41 - 0x20), (7, 2 * 14));
        assert_eq!(s.cell_origin(95), (15 * 7, 5 * 14));
    }

    #[test]
    fn atlas_rejects_bad_specs() {
        assert!(AtlasSpec::new(0, 14, &ASCII).is_err());
        assert!(AtlasSpec::new(7, 65, &ASCII).is_err());
        static UNSORTED: [char; 2] = ['b', 'a'];
        assert!(AtlasSpec::new(7, 14, &UNSORTED).is_err());
        static EMPTY: [char; 0] = [];
        assert!(AtlasSpec::new(7, 14, &EMPTY).is_err());
        assert!(AtlasSpec::new(64, 64, &ASCII).is_ok());
        static BIG: [char; 16 * 16] = {
            let mut a = ['\0'; 16 * 16];
            let mut i = 0;
            while i < 16 * 16 {
                a[i] = i as u8 as char;
                i += 1;
            }
            a
        };
        let e = AtlasSpec::new(64, 64, &BIG).unwrap_err();
        assert!(e.contains("上限"), "{e}");
    }

    #[test]
    fn coverage_takes_max_of_bgr() {
        let bits = [10, 20, 30, 99, 200, 0, 0, 255, 0, 0, 7, 0];
        assert_eq!(coverage_from_bgrx(&bits, 3, 1).unwrap(), vec![30, 200, 7]);
        assert!(coverage_from_bgrx(&bits, 4, 1).is_err());
    }

    #[test]
    fn texels_are_white_with_coverage_alpha_and_a_solid_cell() {
        let s = spec();
        let mut cov = vec![0u8; s.texel_count()];
        cov[0] = 0x80;
        let t = texels(&s, &cov).unwrap();
        assert_eq!(t[0], 0x80FF_FFFF);
        assert_eq!(t[1], 0x00FF_FFFF);
        let (ox, oy) = s.cell_origin(s.solid_slot());
        let at = |x: u32, y: u32| t[(y * s.tex_w + x) as usize];
        assert_eq!(at(ox, oy), 0xFFFF_FFFF);
        assert_eq!(at(ox + s.cell_w - 1, oy + s.cell_h - 1), 0xFFFF_FFFF);
        assert_eq!(at(ox - 1, oy), 0x00FF_FFFF);
        assert_eq!(at(ox, oy + s.cell_h), 0x00FF_FFFF);
        assert!(texels(&s, &cov[1..]).is_err());
    }

    #[test]
    fn layout_places_the_line_at_the_bottom_right() {
        let s = spec();
        let mut out = [Vertex::default(); MAX_VERTS];
        let n = layout_line(
            "A B",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 18);
        let (x0, y0, x1, y1) = (613.0 - 0.5, 444.0 - 0.5, 638.0 - 0.5, 462.0 - 0.5);
        let b = &out[..6];
        assert_eq!((b[0].x, b[0].y), (x0, y0));
        assert_eq!((b[1].x, b[1].y), (x1, y0));
        assert_eq!((b[2].x, b[2].y), (x0, y1));
        assert_eq!((b[5].x, b[5].y), (x1, y1));
        assert!(b.iter().all(|v| v.color == STYLE_DEFAULT.back));
        assert!(b.iter().all(|v| v.z == 0.0 && v.rhw == 1.0));
        assert_eq!((b[0].u, b[0].v), (105.0 / 128.0, 70.0 / 128.0));
        assert_eq!((b[5].u, b[5].v), (112.0 / 128.0, 84.0 / 128.0));
        let a = &out[6..12];
        assert_eq!((a[0].x, a[0].y), (615.0 - 0.5, 446.0 - 0.5));
        assert_eq!((a[5].x, a[5].y), (622.0 - 0.5, 460.0 - 0.5));
        assert_eq!((a[0].u, a[0].v), (7.0 / 128.0, 28.0 / 128.0));
        assert!(a.iter().all(|v| v.color == STYLE_DEFAULT.text));
        assert_eq!(out[12].x, 615.0 + 14.0 - 0.5);
        assert_eq!((out[12].u, out[12].v), (14.0 / 128.0, 28.0 / 128.0));
    }

    #[test]
    fn layout_clips_and_never_overruns() {
        let s = spec();
        let mut small = [Vertex::default(); 6 * 3];
        let n = layout_line(
            "ABCDEF",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut small,
        );
        assert_eq!(n, 18);
        let mut out = [Vertex::default(); MAX_VERTS];
        let long = "X".repeat(300);
        assert_eq!(
            layout_line(
                &long,
                &s,
                2000,
                480,
                PLACE_BOTTOM_RIGHT,
                &STYLE_DEFAULT,
                &mut out
            ),
            (MAX_TEXT + 1) * 6
        );
        assert_eq!(
            layout_line(
                &long,
                &s,
                640,
                480,
                PLACE_BOTTOM_RIGHT,
                &STYLE_DEFAULT,
                &mut out
            ),
            (90 + 1) * 6
        );
        assert_eq!(
            layout_line(
                &long,
                &s,
                60,
                480,
                PLACE_BOTTOM_RIGHT,
                &STYLE_DEFAULT,
                &mut out
            ),
            8 * 6
        );
        assert!(out[..48].iter().all(|v| v.x >= -0.5 && v.x <= 58.0));
        assert_eq!(
            layout_line(
                "A",
                &s,
                640,
                20,
                PLACE_BOTTOM_RIGHT,
                &STYLE_DEFAULT,
                &mut out
            ),
            0
        );
        assert_eq!(
            layout_line(
                "",
                &s,
                640,
                480,
                PLACE_BOTTOM_RIGHT,
                &STYLE_DEFAULT,
                &mut out
            ),
            0
        );
        let mut tiny = [Vertex::default(); 5];
        assert_eq!(
            layout_line(
                "A",
                &s,
                640,
                480,
                PLACE_BOTTOM_RIGHT,
                &STYLE_DEFAULT,
                &mut tiny
            ),
            0
        );
        let n = layout_line(
            "\u{3042}",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 12);
        let q = s.cell_origin(s.slot_of('?').unwrap());
        assert_eq!(out[6].u, q.0 as f32 / 128.0);
    }

    #[test]
    fn status_line_and_fps() {
        assert_eq!(fps_centi(60, 10_000_000, 10_000_000), Some(6000));
        assert_eq!(fps_centi(59, 9_900_000, 10_000_000), Some(5960));
        assert_eq!(fps_centi(60, 0, 10_000_000), None);
        assert_eq!(fps_centi(60, 1, 0), None);
        assert_eq!(status_line(&Status::default()), "HiganRTool");
        assert_eq!(
            status_line(&Status {
                fps_centi: Some(5994),
                latency: None
            }),
            "HiganRTool 59.94fps"
        );
        assert_eq!(
            status_line(&Status {
                fps_centi: Some(6005),
                latency: Some(2)
            }),
            "HiganRTool 60.05fps Latency 2"
        );
        let long = status_line(&Status {
            fps_centi: Some(u32::MAX),
            latency: Some(255),
        });
        assert!(long.len() <= MAX_TEXT);
    }

    #[test]
    fn utf16_box_conversion_keeps_characters_whole() {
        let mut u = [0u16; MAX_TEXT];
        let ja: String = "あ".repeat(MAX_TEXT + 5);
        assert_eq!(encode_utf16_capped(&ja, &mut u), MAX_TEXT);
        assert!(u.iter().all(|&x| x == 0x3042));
        let tail = format!("{}😀", "a".repeat(MAX_TEXT - 1));
        let n = encode_utf16_capped(&tail, &mut u);
        assert_eq!(n, MAX_TEXT - 1, "文字の途中で切った");
        assert!(u[..n].iter().all(|&x| x == 'a' as u16));
        let fit = format!("{}😀", "a".repeat(MAX_TEXT - 2));
        assert_eq!(encode_utf16_capped(&fit, &mut u), MAX_TEXT);
        let mut back = [0u8; MAX_TEXT_UTF8];
        let m = utf16_to_utf8(&u, &mut back);
        assert_eq!(core::str::from_utf8(&back[..m]).unwrap(), fit);
        let mixed = "接続コード\nHiganRTool 59.94fps ホスト\n遅延 2";
        let n = encode_utf16_capped(mixed, &mut u);
        assert_eq!(n, mixed.encode_utf16().count());
        let m = utf16_to_utf8(&u[..n], &mut back);
        assert_eq!(core::str::from_utf8(&back[..m]).unwrap(), mixed);
        assert_eq!(encode_utf16_capped("", &mut u), 0);
        assert_eq!(encode_utf16_capped("abc", &mut []), 0);
        assert_eq!(utf16_to_utf8(&u[..0], &mut back), 0);
        assert_eq!(utf16_to_utf8(&u[..3], &mut []), 0);
        let bad = [0x41, 0xD83D, 0x42, 0xDE00, 0x43];
        let m = utf16_to_utf8(&bad, &mut back);
        assert_eq!(
            core::str::from_utf8(&back[..m]).unwrap(),
            "A\u{FFFD}B\u{FFFD}C"
        );
        let mut four = [0u8; 4];
        let jj = [0x3042u16, 0x3044];
        let m = utf16_to_utf8(&jj, &mut four);
        assert_eq!(&four[..m], "あ".as_bytes());
        let all_ja = [0x3042u16; MAX_TEXT];
        assert_eq!(utf16_to_utf8(&all_ja, &mut back), MAX_TEXT_UTF8);
    }

    #[test]
    fn ui_lines_and_previews_fit_the_box() {
        let units = |t: &str| t.encode_utf16().count();
        let longest = UI_LINES.iter().map(|l| units(l)).max().unwrap();
        assert!(longest <= MAX_TEXT, "{longest} 単位");
        assert!(UI_LINES.iter().any(|l| l.len() > 63));
        assert!(units(PREVIEW_TEXT) <= MAX_TEXT, "{}", units(PREVIEW_TEXT));
        assert!(units(PREVIEW_SCREEN_TEXT) <= MAX_TEXT);
    }

    #[test]
    fn a_full_box_of_text_fits_the_vertex_array() {
        let s = ui_spec();
        let mut all = [Vertex::default(); MAX_VERTS_ALL];
        let text = format!(
            "{}\n{}\n{}\n{}",
            "あ".repeat(31),
            "い".repeat(31),
            "う".repeat(31),
            "え".repeat(32)
        );
        assert_eq!(text.encode_utf16().count(), MAX_TEXT);
        let n = layout_lines(
            &text,
            &s,
            4000,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut all,
        );
        assert_eq!(n, 6 * (4 + 125));
        assert!(n <= MAX_VERTS_ALL);
    }

    #[test]
    fn ini_enable() {
        let r = |t: &str| enabled_in_ini(&crate::ini::parse(t).unwrap());
        assert_eq!(r(""), Ok(false));
        assert_eq!(r("[netplay]\nenable = 1\n"), Ok(false));
        assert_eq!(r("[overlay]\n"), Ok(false));
        assert_eq!(r("[overlay]\nenable = 0\n"), Ok(false));
        assert_eq!(r("[Overlay]\nENABLE = 1\n"), Ok(true));
        assert!(r("[overlay]\nenable = yes\n").is_err());
        assert!(r("[overlay]\nenable = 1\ncorner = tl\n").is_err());
    }

    #[test]
    fn game_ui_is_on_only_for_an_enabled_game_side() {
        let g = |t: &str| game_ui_in_ini(&crate::ini::parse(t).unwrap());
        assert!(g("[netplay]\nenable = 1\ncode_side = game\n"));
        assert!(g("[NetPlay]\nenable = 1\nCODE_SIDE = game\n"));
        assert!(!g(""));
        assert!(!g("[netplay]\nenable = 0\ncode_side = game\n"));
        assert!(!g("[netplay]\nenable = 1\ncode_side = offer\n"));
        assert!(!g("[netplay]\nenable = 1\n"));
        assert!(!g("[overlay]\nenable = 1\ncode_side = game\n"));
    }

    fn ui_spec() -> AtlasSpec {
        AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap()
    }

    /// 枠 `slot` の行 `y` に、枠の左から `x0..x1` の列だけ濃さ 200 を置く。
    fn ink(s: &AtlasSpec, cov: &mut [u8], slot: u32, y: u32, x0: u32, x1: u32) {
        let (ox, oy) = s.cell_origin(slot);
        for x in x0..x1 {
            cov[((oy + y) * s.tex_w + ox + x) as usize] = 200;
        }
    }

    /// 枠 `slot` の墨の列の範囲（枠の左から）。
    fn ink_cols(s: &AtlasSpec, cov: &[u8], slot: u32) -> Option<(u32, u32)> {
        let (ox, oy) = s.cell_origin(slot);
        let w = s.slot_cells(slot) * s.cell_w;
        let on = |x: u32| (0..s.cell_h).any(|y| cov[((oy + y) * s.tex_w + ox + x) as usize] != 0);
        let l = (0..w).find(|&x| on(x))?;
        Some((l, (0..w).rev().find(|&x| on(x))?))
    }

    #[test]
    fn a_narrow_wide_glyph_is_moved_to_the_middle_of_its_slot_and_others_stay() {
        let s = ui_spec();
        let mut cov = vec![0u8; s.texel_count()];
        let (arrow, wide_arrow, stop, a) = (
            s.slot_of('◀').unwrap(),
            s.slot_of('▶').unwrap(),
            s.slot_of('。').unwrap(),
            s.slot_of('A').unwrap(),
        );
        assert_eq!(
            (s.slot_cells(arrow), s.slot_cells(stop), s.slot_cells(a)),
            (2, 2, 1)
        );
        // 枠は 14 画素。◀ の墨は左の 0..7 の 7 列（ちょうど半分。2 行）、▶ は 0..8（半分を超える）、「。」は 0..4、A は 0..3。
        ink(&s, &mut cov, arrow, 3, 0, 7);
        ink(&s, &mut cov, arrow, 7, 1, 3);
        ink(&s, &mut cov, wide_arrow, 5, 0, 8);
        ink(&s, &mut cov, stop, 9, 0, 4);
        ink(&s, &mut cov, a, 5, 0, 3);
        let before = cov.clone();
        center_narrow_wide_glyphs(&s, &mut cov).unwrap();
        assert_eq!(
            ink_cols(&s, &cov, arrow),
            Some((3, 9)),
            "(14 - 7) / 2 = 3 から"
        );
        let (ox, oy) = s.cell_origin(arrow);
        assert_eq!(
            cov[((oy + 7) * s.tex_w + ox + 4) as usize],
            200,
            "行ごとの形を保つ"
        );
        assert_eq!(cov[((oy + 7) * s.tex_w + ox + 3) as usize], 0);
        assert_eq!(
            ink_cols(&s, &cov, wide_arrow),
            Some((0, 7)),
            "墨が枠の半分を超える文字は動かさない"
        );
        assert_eq!(
            ink_cols(&s, &cov, stop),
            Some((0, 3)),
            "図形の記号でない文字は左に寄ったまま"
        );
        assert_eq!(
            ink_cols(&s, &cov, a),
            Some((0, 2)),
            "1 枠の文字は動かさない"
        );
        let moved = cov.iter().zip(&before).filter(|(x, y)| x != y).count();
        assert_eq!(moved, (3 + 3) + (2 + 2), "◀ の 2 行の外は 1 画素も変えない");
        let mut short = vec![0u8; 3];
        assert!(center_narrow_wide_glyphs(&s, &mut short).is_err());
    }

    #[test]
    fn ui_lines_are_all_in_the_glyph_table() {
        let g = ui_glyphs();
        assert!(g.windows(2).all(|w| w[0] < w[1]), "小さい順・重複なし");
        assert_eq!(g[..95], ASCII[..], "ASCII は先頭の 95 枠");
        let s = ui_spec();
        for line in UI_LINES.iter().chain([&PREVIEW_TEXT]) {
            for c in line.chars().filter(|c| *c != '\n') {
                assert!(
                    s.slot_of(c).is_some(),
                    "文字 {c:?} が文字の表に無い（{line}）"
                );
            }
        }
        assert_eq!(s.slot_of('鬱'), None, "使っていない文字が入っている");
        assert!(s.tex_w.is_power_of_two() && s.tex_h.is_power_of_two());
        assert!(s.tex_w <= MAX_TEX_SIDE && s.tex_h <= MAX_TEX_SIDE);
        assert!(s.texel_count() <= 512 * 512, "{}×{}", s.tex_w, s.tex_h);
    }

    #[test]
    fn wide_glyphs_take_two_cells_and_never_overlap() {
        let s = ui_spec();
        assert_eq!(glyph_cells('A'), 1);
        assert_eq!(glyph_cells(' '), 1);
        assert_eq!(glyph_cells('~'), 1);
        assert_eq!(glyph_cells('ｱ'), 1, "半角カナ");
        assert_eq!(glyph_cells('あ'), 2);
        assert_eq!(glyph_cells('接'), 2);
        assert_eq!(glyph_cells('。'), 2);
        let a = s.slot_of('A').unwrap();
        let ja = s.slot_of('あ').unwrap();
        assert_eq!((s.slot_cells(a), s.slot_cells(ja)), (1, 2));
        assert_eq!(s.slot_cells(s.solid_slot()), 1);
        let mut used = vec![false; (s.cols * s.rows) as usize];
        for slot in 0..=s.solid_slot() {
            let (ox, oy) = s.cell_origin(slot);
            assert_eq!((ox % s.cell_w, oy % s.cell_h), (0, 0));
            let (col, row) = (ox / s.cell_w, oy / s.cell_h);
            for c in col..col + s.slot_cells(slot) {
                assert!(c < s.cols, "枠 {slot} が行の端をはみ出す");
                let i = (row * s.cols + c) as usize;
                assert!(!used[i], "枠 {slot} が重なっている");
                used[i] = true;
            }
        }
        let uv = slot_uv(&s, ja);
        assert_eq!((uv[2] - uv[0]) * s.tex_w as f32, 14.0);
        assert_eq!((uv[3] - uv[1]) * s.tex_h as f32, 14.0);
        let g = ['a', 'b', 'あ'];
        let t = AtlasSpec::with_cols(7, 14, &g, 2).unwrap();
        assert_eq!(t.cell_origin(0), (0, 0));
        assert_eq!(t.cell_origin(1), (7, 0));
        assert_eq!(t.cell_origin(2), (0, 14), "全角は次の行へ");
        assert_eq!(t.cell_origin(t.solid_slot()), (0, 28), "塗りは全角の次の行");
        assert!(AtlasSpec::with_cols(7, 14, &g, 1).is_err());
    }

    #[test]
    fn fit_picks_the_smallest_texture() {
        let s = ui_spec();
        let area = |c: u32| {
            let t = AtlasSpec::with_cols(7, 14, &ui_glyphs(), c).unwrap();
            t.tex_w * t.tex_h
        };
        assert!(s.tex_w * s.tex_h <= area(16).min(area(32)).min(area(64)));
        let a = AtlasSpec::fit(7, 14, &ASCII).unwrap();
        assert_eq!((a.tex_w, a.tex_h), (128, 128));
        let e = AtlasSpec::fit(64, 64, &ui_glyphs()).unwrap_err();
        assert!(e.contains("上限"), "{e}");
    }

    #[test]
    fn layout_advances_two_cells_for_wide_glyphs() {
        let s = ui_spec();
        let mut out = [Vertex::default(); MAX_VERTS];
        let n = layout_line(
            "Aあ B",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 4, "下地 + A・あ・B");
        let x0 = 638.0 - 39.0;
        assert_eq!(out[0].x, x0 - 0.5);
        assert_eq!(out[1].x, 638.0 - 0.5);
        let gx = |i: usize| out[i * 6].x + 0.5;
        assert_eq!(gx(1), x0 + 2.0);
        assert_eq!(gx(2), x0 + 2.0 + 7.0, "あ は A の 1 枠後ろ");
        assert_eq!(
            gx(3),
            x0 + 2.0 + 7.0 + 14.0 + 7.0,
            "B は あ（2 枠）と空白（1 枠）の後ろ"
        );
        assert_eq!(
            out[2 * 6 + 5].x - out[2 * 6].x,
            14.0,
            "あ の四角形は 2 枠幅"
        );
        let ja = slot_uv(&s, s.slot_of('あ').unwrap());
        assert_eq!((out[12].u, out[12].v), (ja[0], ja[1]));
        assert_eq!((out[17].u, out[17].v), (ja[2], ja[3]));
        let n = layout_line(
            "あああああ",
            &s,
            60,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 4, "下地 + 3 文字");
        assert!(out[..n].iter().all(|v| v.x >= -0.5 && v.x <= 58.0));
        let n = layout_line(
            "鬱B",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 3);
        assert_eq!(out[12].x - out[6].x, 7.0);
        let q = slot_uv(&s, s.slot_of('?').unwrap());
        assert_eq!((out[6].u, out[6].v), (q[0], q[1]));
    }

    #[test]
    fn ascii_only_lines_keep_their_vertices_in_the_ui_atlas() {
        let (old, new) = (spec(), ui_spec());
        let (mut a, mut b) = (
            [Vertex::default(); MAX_VERTS],
            [Vertex::default(); MAX_VERTS],
        );
        for text in ["HiganRTool 59.94fps Latency 2", "A B", "~"] {
            let (na, nb) = (
                layout_line(
                    text,
                    &old,
                    640,
                    480,
                    PLACE_BOTTOM_RIGHT,
                    &STYLE_DEFAULT,
                    &mut a,
                ),
                layout_line(
                    text,
                    &new,
                    640,
                    480,
                    PLACE_BOTTOM_RIGHT,
                    &STYLE_DEFAULT,
                    &mut b,
                ),
            );
            assert_eq!(na, nb, "{text}");
            for (i, (p, q)) in a[..na].iter().zip(&b[..nb]).enumerate() {
                assert_eq!(
                    (p.x, p.y, p.z, p.rhw, p.color),
                    (q.x, q.y, q.z, q.rhw, q.color),
                    "{text} の頂点 {i}"
                );
            }
        }
    }

    #[test]
    fn layout_lines_stack_upward_from_the_bottom_right() {
        let s = ui_spec();
        let mut all = [Vertex::default(); MAX_VERTS_ALL];
        let n = layout_lines(
            "接続\nAB\r\n\nC",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut all,
        );
        assert_eq!(n, 6 * (3 + 3 + 2));
        let step = 14.0 + 4.0;
        let c_box = &all[n - 12..n - 6];
        assert_eq!(c_box[5].y + 0.5, 462.0);
        let ab = &all[6 * 3..6 * 3 + 6];
        assert_eq!(ab[5].y + 0.5, 462.0 - 2.0 * step);
        assert_eq!(ab[5].y - ab[0].y, step);
        let top = &all[..6];
        assert_eq!(top[5].y + 0.5, 462.0 - 3.0 * step);
        assert!([top, ab, c_box].iter().all(|b| b[5].x + 0.5 == 638.0));
        let (mut one, mut ref_) = (
            [Vertex::default(); MAX_VERTS],
            [Vertex::default(); MAX_VERTS],
        );
        let a = layout_lines(
            "接続コード",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut one,
        );
        let b = layout_line(
            "接続コード",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut ref_,
        );
        assert_eq!((a, one[..a].to_vec()), (b, ref_[..b].to_vec()));
        let n5 = layout_lines(
            "a\nb\nc\nd\ne",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut all,
        );
        assert_eq!(n5, 4 * 12);
        let mut small = [Vertex::default(); 12];
        let n = layout_lines(
            "a\nb",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut small,
        );
        assert_eq!(n, 12);
        let n = layout_lines(
            "a\nb\nc",
            &s,
            640,
            40,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut all,
        );
        assert_eq!(
            n, 12,
            "下の 1 行だけ（2 行目は下端 18 + 18 + 18 が高さ 40 を越える）"
        );
    }

    #[test]
    fn downscale_averages_blocks() {
        let src = [0, 255, 10, 10, 255, 0, 10, 11];
        assert_eq!(downscale_coverage(&src, 4, 2, 2).unwrap(), vec![128, 10]);
        assert_eq!(downscale_coverage(&src, 4, 2, 1).unwrap(), src.to_vec());
        assert!(downscale_coverage(&src, 4, 2, 3).is_err());
        assert!(downscale_coverage(&src, 4, 2, 0).is_err());
        assert!(downscale_coverage(&src[1..], 4, 2, 2).is_err());
        assert_eq!(downscale_coverage(&[255; 4], 2, 2, 2).unwrap(), vec![255]);
    }

    #[test]
    fn blit_quads_draws_what_d3d8_would() {
        let s = spec();
        let mut cov = vec![0u8; s.texel_count()];
        let (ox, oy) = s.cell_origin(s.slot_of('A').unwrap());
        cov[(oy * s.tex_w + ox) as usize] = 255;
        let t = texels(&s, &cov).unwrap();
        let mut v = [Vertex::default(); MAX_VERTS];
        let style = Style {
            text: 0xFFFF_0000,
            back: 0x8000_0000,
            pad: 1,
        };
        let n = layout_line(
            "A",
            &s,
            32,
            20,
            Place {
                right: 1,
                bottom: 1,
            },
            &style,
            &mut v,
        );
        assert_eq!(n, 12);
        let (cw, ch) = (32u32, 20u32);
        let mut canvas = vec![200u8; (cw * ch * 3) as usize];
        assert_eq!(
            blit_quads(&v[..n], &t, s.tex_w, s.tex_h, &mut canvas, cw, ch, 0, 0),
            2
        );
        let px = |x: u32, y: u32| {
            let i = ((y * cw + x) * 3) as usize;
            [canvas[i], canvas[i + 1], canvas[i + 2]]
        };
        assert_eq!(px(22, 3), [100, 100, 100], "下地の左上");
        assert_eq!(px(23, 4), [255, 0, 0]);
        assert_eq!(
            px(24, 4),
            [100, 100, 100],
            "文字の枠のほかの画素は透明（下地のまま）"
        );
        assert_eq!(px(21, 3), [200, 200, 200], "下地の外は触らない");
        assert_eq!(px(31, 3), [200, 200, 200]);
        assert_eq!(px(30, 18), [100, 100, 100], "下地の右下の内側");
        let mut c2 = vec![200u8; (cw * ch * 3) as usize];
        assert_eq!(
            blit_quads(&v[..n], &t, s.tex_w, s.tex_h, &mut c2, cw, ch, 8, 0),
            2
        );
        assert_eq!(&c2[(3 * cw + 30) as usize * 3..][..3], &[100, 100, 100]);
        assert_eq!(&c2[(3 * cw + 29) as usize * 3..][..3], &[200, 200, 200]);
        assert_eq!(
            blit_quads(&v[..n], &t, s.tex_w, s.tex_h, &mut c2, cw, ch, 100, 0),
            0
        );
        assert_eq!(
            blit_quads(
                &v[..n],
                &t[1..],
                s.tex_w,
                s.tex_h,
                &mut canvas,
                cw,
                ch,
                0,
                0
            ),
            0
        );
        let mut big = v;
        let base = big[6].x;
        for p in &mut big[6..12] {
            p.x += (p.x - base).max(0.0);
        }
        let mut c3 = vec![200u8; (cw * ch * 3) as usize];
        assert_eq!(
            blit_quads(&big[6..12], &t, s.tex_w, s.tex_h, &mut c3, cw, ch, 0, 0),
            1
        );
        let at = |x: u32, y: u32| ((y * cw + x) * 3) as usize;
        assert_eq!(&c3[at(23, 4)..][..3], &[255, 0, 0]);
        assert_eq!(&c3[at(24, 4)..][..3], &[255, 0, 0], "横に引き伸ばされた");
        assert_eq!(&c3[at(25, 4)..][..3], &[200, 200, 200]);
        assert_eq!(&c3[at(23, 5)..][..3], &[200, 200, 200]);
    }

    #[test]
    fn upscale_rgb_repeats_pixels() {
        let img = [1, 2, 3, 4, 5, 6];
        let up = upscale_rgb(&img, 2, 1, 2);
        assert_eq!(
            up,
            vec![
                1, 2, 3, 1, 2, 3, 4, 5, 6, 4, 5, 6, 1, 2, 3, 1, 2, 3, 4, 5, 6, 4, 5, 6
            ]
        );
        assert!(upscale_rgb(&img, 3, 1, 2).is_empty());
        assert!(upscale_rgb(&img, 2, 1, 0).is_empty());
    }

    #[test]
    fn argb_of_pixel_converts_every_format() {
        assert_eq!(
            argb_of_pixel(D3DFMT_A8R8G8B8, &[0x11, 0x22, 0x33, 0x44]),
            Some(0x4433_2211)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_X8R8G8B8, &[0x11, 0x22, 0x33, 0x00]),
            Some(0xFF33_2211)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_R8G8B8, &[0x11, 0x22, 0x33]),
            Some(0xFF33_2211)
        );
        let w = |v: u16| v.to_le_bytes();
        for (v, want) in [
            (0xF800u16, 0xFFFF_0000u32),
            (0x07E0, 0xFF00_FF00),
            (0x001F, 0xFF00_00FF),
            (0x0000, 0xFF00_0000),
            (0x8410, 0xFF84_8284),
        ] {
            assert_eq!(argb_of_pixel(D3DFMT_R5G6B5, &w(v)), Some(want), "{v:04X}");
        }
        assert_eq!(
            argb_of_pixel(D3DFMT_X1R5G5B5, &w(0x7C00)),
            Some(0xFFFF_0000)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_X1R5G5B5, &w(0x0000)),
            Some(0xFF00_0000),
            "X は α 255"
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_A1R5G5B5, &w(0x8000)),
            Some(0xFF00_0000)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_A1R5G5B5, &w(0x7FFF)),
            Some(0x00FF_FFFF)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_A4R4G4B4, &w(0x1234)),
            Some(0x1122_3344)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_A4R4G4B4, &w(0xF000)),
            Some(0xFF00_0000)
        );
        assert_eq!(
            argb_of_pixel(D3DFMT_A4R4G4B4, &w(0x0F00)),
            Some(0x00FF_0000)
        );
        assert_eq!(argb_of_pixel(D3DFMT_A8R8G8B8, &[1, 2, 3]), None);
        assert_eq!(argb_of_pixel(D3DFMT_R5G6B5, &[1]), None);
        assert_eq!(argb_of_pixel(D3DFMT_R8G8B8, &[1, 2]), None);
        assert_eq!(argb_of_pixel(0, &[0; 4]), None);
        assert_eq!(argb_of_pixel(99, &[0; 4]), None);
        for (f, n) in [
            (D3DFMT_A8R8G8B8, 4),
            (D3DFMT_X8R8G8B8, 4),
            (D3DFMT_R8G8B8, 3),
            (D3DFMT_R5G6B5, 2),
            (D3DFMT_X1R5G5B5, 2),
            (D3DFMT_A1R5G5B5, 2),
            (D3DFMT_A4R4G4B4, 2),
        ] {
            assert_eq!(
                texture_pixel_bytes(f),
                Some(n),
                "{}",
                texture_format_name(f)
            );
        }
        assert_eq!(texture_pixel_bytes(0), None);
    }

    /// 4×3 の A8R8G8B8（行の送り 24 = 16 + 詰め物 8）。画素 (x, y) は B = x・G = y・R = 0xAA・A = 0xFF。詰め物は 0xEE。
    fn sample_surface() -> Vec<u8> {
        let mut d = vec![0xEEu8; 2 * 24 + 16];
        for y in 0..3usize {
            for x in 0..4usize {
                d[y * 24 + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, 0xAA, 0xFF]);
            }
        }
        d
    }

    fn sample_px(x: u32, y: u32) -> u32 {
        0xFFAA_0000 | y << 8 | x
    }

    #[test]
    fn extract_rect_reads_only_the_rect() {
        let d = sample_surface();
        let r = Rect {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        };
        let mut out = [0xDEADu32; 5];
        extract_rect_argb(D3DFMT_A8R8G8B8, 4, 3, 24, &d, r, &mut out, 3).unwrap();
        assert_eq!(
            out,
            [
                sample_px(1, 1),
                sample_px(2, 1),
                0xDEAD,
                sample_px(1, 2),
                sample_px(2, 2)
            ]
        );
        let mut out = [0u32; 4];
        extract_rect_argb(D3DFMT_A8R8G8B8, 4, 3, 24, &d[..60], r, &mut out, 2).unwrap();
        assert_eq!(out[3], sample_px(2, 2));
        let before = out;
        let e = extract_rect_argb(D3DFMT_A8R8G8B8, 4, 3, 24, &d[..59], r, &mut out, 2).unwrap_err();
        assert!(e.contains("届かない"), "{e}");
        assert_eq!(out, before, "断ったのに書いた");
        let r_left = Rect {
            x: 0,
            y: 2,
            w: 1,
            h: 1,
        };
        let mut one = [0u32; 1];
        extract_rect_argb(D3DFMT_A8R8G8B8, 4, 3, 24, &d[..52], r_left, &mut one, 1).unwrap();
        assert_eq!(one[0], sample_px(0, 2));
        let mut h = vec![0u8; 8 * 2];
        h[8 + 2..][..2].copy_from_slice(&0xF800u16.to_le_bytes());
        let mut px = [0u32; 1];
        let r16 = Rect {
            x: 1,
            y: 1,
            w: 1,
            h: 1,
        };
        extract_rect_argb(D3DFMT_R5G6B5, 4, 2, 8, &h, r16, &mut px, 1).unwrap();
        assert_eq!(px[0], 0xFFFF_0000);
    }

    #[test]
    fn extract_rect_refuses_bad_inputs() {
        let d = sample_surface();
        let ok = Rect {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
        };
        let f = D3DFMT_A8R8G8B8;
        let mut out = [7u32; 4];
        for (label, fmt, r, pitch, stride, out_len) in [
            ("右にはみ出す", f, Rect { x: 3, w: 2, ..ok }, 24, 2, 4),
            ("下にはみ出す", f, Rect { y: 2, ..ok }, 24, 2, 4),
            ("幅 0", f, Rect { w: 0, ..ok }, 24, 2, 4),
            ("高さ 0", f, Rect { h: 0, ..ok }, 24, 2, 4),
            ("桁あふれ", f, Rect { x: u32::MAX, ..ok }, 24, 2, 4),
            ("pitch が短い", f, ok, 15, 2, 4),
            ("行の送りが幅より小さい", f, ok, 24, 1, 4),
            ("書き出し先が短い", f, ok, 24, 2, 3),
            ("知らない形式", 0, ok, 24, 2, 4),
        ] {
            let mut o = out;
            let res = extract_rect_argb(fmt, 4, 3, pitch, &d, r, &mut o[..out_len], stride);
            assert!(res.is_err(), "{label} を通した");
            assert_eq!(o, out, "{label}: 断ったのに書いた");
        }
        // 対照: 同じ引数で正しければ通る（上の断りが「他の理由」でないこと）。
        extract_rect_argb(f, 4, 3, 24, &d, ok, &mut out, 2).unwrap();
        assert_eq!(out[3], sample_px(1, 1));
    }

    #[test]
    fn resample_argb_averages_and_repeats() {
        let img = [0xFF11_2233, 0x8044_5566];
        assert_eq!(resample_argb(&img, 2, 1, 2, 1).unwrap(), img.to_vec());
        let px = [0xFFFF_0000, 0x0000_00FF, 0x0000_00FF, 0x0000_00FF];
        assert_eq!(resample_argb(&px, 2, 2, 1, 1).unwrap(), vec![0x40FF_0000]);
        let up = resample_argb(&img, 2, 1, 4, 2).unwrap();
        assert_eq!(
            up,
            vec![
                img[0], img[0], img[1], img[1], img[0], img[0], img[1], img[1]
            ]
        );
        let three = [0xFF00_0000, 0xFF00_00FF, 0xFF00_0000];
        let two = resample_argb(&three, 3, 1, 2, 1).unwrap();
        assert_eq!(two.len(), 2);
        assert!(two.iter().all(|p| p >> 24 == 0xFF));
        assert!(resample_argb(&img, 2, 1, 0, 1).is_err());
        assert!(resample_argb(&img, 0, 1, 1, 1).is_err());
        assert!(resample_argb(&img[..1], 2, 1, 1, 1).is_err());
    }

    #[test]
    fn game_index_covers_the_printable_range_except_backtick() {
        assert_eq!(game_index(' '), None);
        assert_eq!(game_index('!'), Some(0));
        assert_eq!(game_index('A'), Some(0x41 - 0x21));
        assert_eq!(game_index('_'), Some(0x5F - 0x21));
        assert_eq!(game_index('`'), None, "0x60 はゲームの文字にしない");
        assert_eq!(game_index('a'), Some(0x61 - 0x21));
        assert_eq!(game_index('~'), Some(GAME_GLYPHS - 1));
        assert_eq!(game_index('\u{7F}'), None);
        assert_eq!(game_index('あ'), None);
        assert_eq!((GAME_LAST - GAME_FIRST + 1) as usize, GAME_GLYPHS);
    }

    fn game_base() -> (AtlasSpec, Vec<u32>) {
        let s = AtlasSpec::fit_game(7, 14, &ui_glyphs()).unwrap();
        let cov: Vec<u8> = (0..s.texel_count()).map(|i| (i % 251) as u8).collect();
        let t = texels(&s, &cov).unwrap();
        (s, t)
    }

    /// 全部の文字を `w` × `h` の一色（`0xFF000000 | 字`）で写した表。
    fn game_atlas(w: u32, h: u32) -> (AtlasSpec, Vec<u32>) {
        let (s, t) = game_base();
        let px: Vec<(char, Vec<u32>)> = (0x21u8..=0x7E)
            .filter(|&b| b != b'`')
            .map(|b| (b as char, vec![0xFF00_0000 | b as u32; (w * h) as usize]))
            .collect();
        let gs: Vec<GameGlyphSrc> = px
            .iter()
            .map(|(c, p)| GameGlyphSrc {
                c: *c,
                w,
                h,
                pixels: p,
                quad_w: w,
                quad_h: h,
            })
            .collect();
        apply_game_font(&s, &t, &gs, GAME_ADVANCE).unwrap()
    }

    #[test]
    fn game_block_never_overlaps_the_gdi_cells() {
        let s = AtlasSpec::fit_game(7, 14, &ui_glyphs()).unwrap();
        let b = s.game_block.expect("塊を予約している");
        println!("文字の表（ゲームの文字の塊つき）: {}×{}", s.tex_w, s.tex_h);
        assert!(s.tex_w.is_power_of_two() && s.tex_h.is_power_of_two());
        assert!(s.tex_w <= MAX_TEX_SIDE && s.tex_h <= MAX_TEX_SIDE);
        let block_bottom = b.y + (GAME_GLYPHS as u32).div_ceil(b.cols) * b.slot_h;
        assert!(b.x + b.cols * b.slot_w <= s.tex_w && block_bottom <= s.tex_h);
        for slot in 0..=s.solid_slot() {
            let (_, oy) = s.cell_origin(slot);
            assert!(oy + s.cell_h <= b.y, "枠 {slot} が塊に食い込む");
        }
        assert_eq!(s.game_slot_origin(0), Some((b.x, b.y)));
        assert_eq!(s.game_slot_origin(15), Some((b.x + 15 * b.slot_w, b.y)));
        assert_eq!(s.game_slot_origin(16), Some((b.x, b.y + b.slot_h)));
        assert_eq!(
            s.game_slot_origin(GAME_GLYPHS - 1),
            Some((b.x + 13 * b.slot_w, b.y + 5 * b.slot_h))
        );
        assert_eq!(s.game_slot_origin(GAME_GLYPHS), None);
        let plain = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        assert!(plain.game_block.is_none() && plain.game.is_none());
        assert!(plain.tex_w * plain.tex_h <= s.tex_w * s.tex_h);
        let a = AtlasSpec::new(7, 14, &ASCII).unwrap();
        assert_eq!((a.tex_w, a.tex_h, a.game_block), (128, 128, None));
        assert_eq!(a.game_slot_origin(0), None);
        assert!(s.game.is_none() && s.game_glyph('A').is_none());
    }

    #[test]
    fn apply_game_font_places_pixels_in_the_slot_only() {
        let (s, mut t) = game_base();
        let b = s.game_block.unwrap();
        let block_at = |x: u32, y: u32| (y * s.tex_w + x) as usize;
        t[block_at(b.x + 40, b.y + 40)] = 0xDEAD_BEEF;
        let a_px: Vec<u32> = (0..9 * 12).map(|i| 0xFF00_0000 | i).collect();
        let gs = [GameGlyphSrc {
            c: 'A',
            w: 9,
            h: 12,
            pixels: &a_px,
            quad_w: 9,
            quad_h: 12,
        }];
        let (s2, t2) = apply_game_font(&s, &t, &gs, GAME_ADVANCE).unwrap();
        let (ox, oy) = s.game_slot_origin(game_index('A').unwrap()).unwrap();
        for row in 0..12u32 {
            for col in 0..9u32 {
                assert_eq!(
                    t2[block_at(ox + col, oy + row)],
                    0xFF00_0000 | (row * 9 + col),
                    "({col}, {row})"
                );
            }
        }
        assert_eq!(t2[block_at(ox + 9, oy)], 0);
        assert_eq!(t2[block_at(ox, oy + 12)], 0);
        assert_eq!(t2[block_at(b.x + 40, b.y + 40)], 0, "前の絵が消えていない");
        assert_eq!(t2[..block_at(0, b.y)], t[..block_at(0, b.y)]);
        assert_eq!(
            s2.game_glyph('A'),
            Some(GameGlyph {
                x: ox,
                y: oy,
                w: 9,
                h: 12
            })
        );
        assert_eq!(s2.game_glyph('B'), None, "コピーしていない文字は GDI");
        assert_eq!(s2.game.as_ref().unwrap().advance, GAME_ADVANCE);
        assert!(s.game.is_none());
        let four: Vec<u32> = [0xFF00_0000u32, 0xFF00_00FF, 0xFF00_00FF, 0xFF00_0000].repeat(4);
        let small = [GameGlyphSrc {
            c: 'B',
            w: 4,
            h: 4,
            pixels: &four,
            quad_w: 2,
            quad_h: 2,
        }];
        let (s3, t3) = apply_game_font(&s, &t, &small, GAME_ADVANCE).unwrap();
        let g = s3.game_glyph('B').unwrap();
        assert_eq!((g.w, g.h), (2, 2));
        assert_eq!(
            t3[block_at(g.x, g.y)],
            0xFF00_0080,
            "平均 (0 + 255 + 0 + 255) / 4 の青 ≒ 128"
        );
        let zero = [GameGlyphSrc {
            quad_w: 0,
            quad_h: 0,
            ..small[0]
        }];
        let (s4, _) = apply_game_font(&s, &t, &zero, GAME_ADVANCE).unwrap();
        assert_eq!(s4.game_glyph('B').map(|g| (g.w, g.h)), Some((4, 4)));
        let bad = |g: GameGlyphSrc| apply_game_font(&s, &t, &[g], GAME_ADVANCE).is_err();
        assert!(bad(GameGlyphSrc { c: '`', ..gs[0] }), "`");
        assert!(bad(GameGlyphSrc { c: ' ', ..gs[0] }), "空白");
        assert!(bad(GameGlyphSrc { c: 'あ', ..gs[0] }), "範囲の外");
        assert!(bad(GameGlyphSrc { w: 8, ..gs[0] }), "画素の数が合わない");
        assert!(bad(GameGlyphSrc { w: 0, ..gs[0] }), "幅 0");
        let huge = vec![0xFFFF_FFFFu32; 17 * 4];
        assert!(
            bad(GameGlyphSrc {
                w: 17,
                h: 4,
                pixels: &huge,
                quad_w: 0,
                quad_h: 0,
                ..gs[0]
            }),
            "枠より大きい"
        );
        assert!(
            apply_game_font(&s, &t, &[gs[0], gs[0]], GAME_ADVANCE).is_err(),
            "重複"
        );
        assert!(
            apply_game_font(&s, &t, &[], GAME_ADVANCE).is_err(),
            "1 文字も無い"
        );
        assert!(apply_game_font(&s, &t, &gs, 0).is_err(), "送り 0");
        assert!(
            apply_game_font(&s, &t[1..], &gs, GAME_ADVANCE).is_err(),
            "画素の数"
        );
        let plain = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let pt = vec![0u32; plain.texel_count()];
        assert!(apply_game_font(&plain, &pt, &gs, GAME_ADVANCE).is_err());
    }

    #[test]
    fn a_game_glyph_wider_than_the_advance_is_centered_on_its_advance_cell() {
        let (s, _) = game_atlas(13, 12);
        let mut out = [Vertex::default(); MAX_VERTS];
        let n = layout_line(
            "AB",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 3);
        let glyph = |q: usize| (out[q * 6].x + 0.5, out[q * 6 + 5].x + 0.5);
        // 下地の右端は 638、枠は A が 618..627・B が 627..636。絵 13 画素の真ん中を枠の真ん中に揃えるので (13 - 9) / 2 = 2 画素左から。
        assert_eq!(glyph(1), (616.0, 629.0));
        assert_eq!(glyph(2), (625.0, 638.0));
        let (narrow, _) = game_atlas(8, 12);
        let n = layout_line(
            "A",
            &narrow,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 2);
        assert_eq!(out[6].x + 0.5, 627.0, "送りより狭い絵は枠の左端のまま");
        let (odd, _) = game_atlas(14, 12);
        let n = layout_line(
            "A",
            &odd,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 2);
        assert_eq!(
            out[6].x + 0.5,
            625.0,
            "(14 - 9) / 2 は 2 画素に切り捨て（半端な位置に置かない）"
        );
    }

    #[test]
    fn ascii_lines_use_the_game_advance_and_glyph_slots() {
        let (s, _) = game_atlas(8, 12);
        let mut out = [Vertex::default(); MAX_VERTS];
        let n = layout_line(
            "AB C",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 4, "下地 + A・B・C（空白は置かない）");
        assert_eq!((out[0].x + 0.5, out[5].x + 0.5), (598.0, 638.0));
        let glyph_x = |q: usize| out[q * 6].x + 0.5;
        assert_eq!(glyph_x(1), 600.0);
        assert_eq!(glyph_x(2), 600.0 + 9.0, "B は A の 9 画素後ろ");
        assert_eq!(glyph_x(3), 600.0 + 27.0, "C は空白（9 画素）の後ろ");
        let a = &out[6..12];
        assert_eq!(
            (a[5].x - a[0].x, a[5].y - a[0].y),
            (8.0, 12.0),
            "絵の大きさのまま"
        );
        assert_eq!(
            a[5].y + 0.5,
            460.0 + GAME_BASELINE_SHIFT as f32,
            "下端を行の枠の下端に揃える"
        );
        let g = s.game_glyph('A').unwrap();
        let (tw, th) = (s.tex_w as f32, s.tex_h as f32);
        assert_eq!((a[0].u, a[0].v), (g.x as f32 / tw, g.y as f32 / th));
        assert_eq!(
            (a[5].u, a[5].v),
            ((g.x + 8) as f32 / tw, (g.y + 12) as f32 / th)
        );
        assert!(a.iter().all(|v| v.color == STYLE_DEFAULT.text));
        let mut n2 = [Vertex::default(); MAX_VERTS];
        let m = layout_line(
            "!AZ~",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut n2,
        );
        assert_eq!(m, 6 * 5);
        for (q, c) in "!AZ~".chars().enumerate() {
            let g = s.game_glyph(c).unwrap();
            assert_eq!(
                (n2[(q + 1) * 6].u, n2[(q + 1) * 6].v),
                (g.x as f32 / tw, g.y as f32 / th),
                "{c}"
            );
        }
    }

    #[test]
    fn mixed_lines_keep_gdi_glyphs_for_the_rest() {
        let (s, _) = game_atlas(8, 12);
        let mut out = [Vertex::default(); MAX_VERTS];
        let (tw, th) = (s.tex_w as f32, s.tex_h as f32);
        let n = layout_line(
            "A`あ",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 4);
        assert_eq!(out[5].x + 0.5 - (out[0].x + 0.5), 30.0 + 4.0);
        let x = |q: usize| out[q * 6].x + 0.5;
        assert_eq!(x(2) - x(1), 9.0);
        assert_eq!(x(3) - x(2), 7.0, "` の送りは GDI の半角");
        let bt = slot_uv(&s, s.slot_of('`').unwrap());
        assert_eq!((out[12].u, out[12].v), (bt[0], bt[1]), "` は GDI の枠");
        assert_eq!(out[12 + 5].x - out[12].x, 7.0);
        assert_eq!(out[12 + 5].y - out[12].y, 14.0, "GDI の文字は枠の高さ");
        let ja = slot_uv(&s, s.slot_of('あ').unwrap());
        assert_eq!((out[18].u, out[18].v), (ja[0], ja[1]));
        assert_eq!(out[18 + 5].x - out[18].x, 14.0);
        let n = layout_line(
            "鬱B",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 3);
        let q = s.game_glyph('?').unwrap();
        assert_eq!((out[6].u, out[6].v), (q.x as f32 / tw, q.y as f32 / th));
        assert_eq!(out[12].x - out[6].x, 9.0);
        let n = layout_line(
            "AAAAAAAAAA",
            &s,
            60,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 7, "下地 + 6 文字");
        assert!(out[..n].iter().all(|v| v.x >= -0.5 && v.x <= 58.0));
        let (plain, _) = game_base();
        let n = layout_line(
            "AB",
            &plain,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut out,
        );
        assert_eq!(n, 6 * 3);
        assert_eq!(out[12].x - out[6].x, 7.0);
    }

    #[test]
    fn game_font_lines_stack_like_gdi_lines() {
        let (s, _) = game_atlas(8, 12);
        let mut all = [Vertex::default(); MAX_VERTS_ALL];
        let n = layout_lines(
            "接続\nAB\nHi",
            &s,
            640,
            480,
            PLACE_BOTTOM_RIGHT,
            &STYLE_DEFAULT,
            &mut all,
        );
        assert_eq!(n, 6 * (3 + 3 + 3));
        let bottoms: Vec<f32> = [0, 3, 6].iter().map(|&q| all[q * 6 + 5].y + 0.5).collect();
        assert_eq!(bottoms, vec![462.0 - 36.0, 462.0 - 18.0, 462.0]);
        assert!([0, 3, 6].iter().all(|&q| all[q * 6 + 5].x + 0.5 == 638.0));
    }

    #[test]
    fn preview_sheet_draws_each_atlas_in_its_own_block() {
        let (plain, plain_t) = game_base();
        let (game, game_t) = game_atlas(8, 12);
        let (w, h, img) = preview_sheet_rgb(&[
            ("GDI only", &plain, &plain_t),
            ("Game font", &game, &game_t),
        ]);
        assert_eq!((w, h), (PREVIEW_SHEET_W, PREVIEW_BLOCK_H * 2));
        assert_eq!(img.len(), (w * h * 3) as usize);
        let bg = |x: u32, y: u32| {
            [
                (40 + x * 180 / w) as u8,
                (60 + (y % PREVIEW_BLOCK_H) * 120 / PREVIEW_BLOCK_H) as u8,
                110,
            ]
        };
        let changed = |block: u32| {
            (0..PREVIEW_BLOCK_H)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let at = (((block * PREVIEW_BLOCK_H + y) * w + x) * 3) as usize;
                    img[at..at + 3] != bg(x, y)
                })
                .count()
        };
        assert!(
            changed(0) > 3000 && changed(1) > 3000,
            "{} {}",
            changed(0),
            changed(1)
        );
        let corner = |block: u32| {
            (0..40u32)
                .flat_map(|y| (0..40u32).map(move |x| (x, y)))
                .all(|(x, y)| {
                    let at = (((block * PREVIEW_BLOCK_H + y) * w + x) * 3) as usize;
                    img[at..at + 3] == bg(x, y)
                })
        };
        assert!(corner(0) && corner(1));
        let half = (PREVIEW_BLOCK_H * w * 3) as usize;
        assert_ne!(img[..half], img[half..], "2 つの表の違いが絵に出ていない");
        assert!(preview_sheet_rgb(&[]).2.is_empty());
    }

    #[test]
    fn ini_preview() {
        let r = |t: &str| preview_in_ini(&crate::ini::parse(t).unwrap());
        assert_eq!(r(""), Ok(false));
        assert_eq!(r("[overlay]\nenable = 1\n"), Ok(false));
        assert_eq!(r("[overlay]\nenable = 1\npreview = 0\n"), Ok(false));
        assert_eq!(r("[overlay]\nenable = 1\npreview = 1\n"), Ok(true));
        assert!(r("[overlay]\nenable = 1\npreview = on\n").is_err());
        assert!(r("[overlay]\nenable = 1\npreviw = 1\n").is_err());
        assert!(r("[overlay]\nenable = 2\npreview = 1\n").is_err());
        let en = |t: &str| enabled_in_ini(&crate::ini::parse(t).unwrap());
        assert_eq!(en("[overlay]\nenable = 1\npreview = 1\n"), Ok(true));
        assert_eq!(en("[overlay]\npreview = 1\n"), Ok(false));
    }

    #[test]
    fn preview_screen_text_fits_the_box_and_the_table() {
        let units = PREVIEW_SCREEN_TEXT.encode_utf16().count();
        assert!(units <= MAX_TEXT, "{units}");
        assert!(PREVIEW_SCREEN_TEXT.split('\n').count() <= MAX_LINES);
        let s = ui_spec();
        for c in PREVIEW_SCREEN_TEXT.chars().filter(|c| *c != '\n') {
            assert!(s.slot_of(c).is_some(), "文字 {c:?} が文字の表に無い");
        }
        assert!(PREVIEW_SCREEN_TEXT.chars().any(|c| glyph_cells(c) == 2));
        assert!(PREVIEW_SCREEN_TEXT.chars().any(|c| game_index(c).is_some()));
    }
}
