//! ゲーム内 UI の頂点の置き方（pure。D3D8 は触らない）。
//!
//! 右下に 1 行を置く `overlay::layout_line` では、画面の中央のメニュー・左上の行・上の枠の左の行は置けない。
//! ここは「左上の座標（画素）を渡して置く」下の層（四角形と 1 行の文字）と、中央の枠の大きさの決め方を持つ。
//! 文字の幅・フォールバック（表に無い文字は `?`）・ゲームの文字の置き方は `overlay::resolve` を使う（`layout_line` と同じ幅になる）。
//!
//! ゲームのスレッドで毎フレーム呼ぶので、確保もパニックもしない（出力は呼び手の配列。添字は範囲を確かめてから使う）。
//! メニューは行ごとに `DrawPrimitiveUP` を呼ぶ（1 行は最大 `MAX_TEXT` 文字 = `MAX_VERTS` 頂点で、スタックの配列が小さく済む。
//! 状態の保存と戻しは 1 回にまとめる。`overlay_win::draw_with_saved_state`）。
#![forbid(unsafe_code)]

use crate::overlay::{
    AtlasSpec, Face, GAME_BASELINE_SHIFT, MAX_TEXT, SMALL_CELL_H, Vertex, game_glyph_left, quad,
    rect_uv, resolve, resolve_plain, resolve_small, slot_uv,
};

/// 文字の幅と絵の決め方（`overlay::resolve` か、ゲームの文字を使わない `overlay::resolve_plain`）。
type Resolver = fn(&AtlasSpec, char, Option<u32>) -> (u32, Face);

/// 行の下地の余白（画素。`overlay::STYLE_DEFAULT.pad` と同じ）。
pub const PAD: u32 = 2;
/// 中央の枠の内側の余白（画素）。
pub const BOX_MARGIN: u32 = 10;
/// 枠線の太さ（画素）。
pub const BORDER: u32 = 2;
/// 画面から枠までの最小の余白。
pub const SCREEN_MARGIN: u32 = 4;

/// 画面を暗くする色（ARGB。黒の α 0xA0）。
pub const COLOR_DIM: u32 = 0xA000_0000;
/// 枠の下地・枠線。
pub const COLOR_BOX: u32 = 0xF010_1830;
pub const COLOR_BORDER: u32 = 0xFFB0_C0E0;
/// 文字の色: ふつう / 選んでいる項目 / 見出し / 補足（キーの行） / 断り・失敗。
pub const COLOR_TEXT: u32 = 0xFFFF_FFFF;
pub const COLOR_SELECTED: u32 = 0xFFFF_E060;
pub const COLOR_TITLE: u32 = 0xFF90_D0FF;
pub const COLOR_HINT: u32 = 0xFFA0_A8B8;
pub const COLOR_WARN: u32 = 0xFFFF_A070;
/// 左上・上の枠の左の行の下地（半透明の黒）。
pub const COLOR_LINE_BACK: u32 = 0x9000_0000;

/// マッチ中の行（`net_menu::hud_line`）の置き場（画素）: 上の枠（高さ `HUD_BAND_H`）の左。文字の枠の左上が (`HUD_X`, `HUD_Y`)、
/// 下地は文字の左右に `PAD` ずつ広げて y = 0 から `HUD_BAND_H` の高さ。
pub const HUD_X: u32 = 6;
pub const HUD_Y: u32 = 1;
pub const HUD_BAND_H: u32 = 15;
/// ゲームの真ん中のタイマー（「00:03」）の文字の左の縁（画素の x）。実機の撮影 3 枚で測った（ゲームの定義からは引いていない）。
pub const TIMER_LEFT: u32 = 299;
/// マッチ中の行の下地の右端（この x は含まない）の上限。文字の終わり（`HUD_X + HUD_TEXT_MAX_W` = 288）からタイマーの縁まで
/// 11 画素（英数字 1 文字の送り `overlay::GAME_ADVANCE` = 9 より広い）あけ、「推奨 1」の 1 がタイマーに続いて読めないようにする。
pub const HUD_RIGHT: u32 = 290;
const _: () = assert!(
    HUD_RIGHT - PAD + crate::overlay::GAME_ADVANCE < TIMER_LEFT,
    "行の文字の終わりとタイマーの間に、英数字 1 文字ぶんより広い隙間を置く"
);
/// マッチ中の行の文字を置ける幅（画素）。下地の右端 `HUD_X + 幅 + PAD` が `HUD_RIGHT` を越えない幅。越える文字は描く側
/// （`push_text_plain` の `max_w`）が落とす。この行は GDI の文字（半角 7 画素）で描く（`text_width_plain`）。
pub const HUD_TEXT_MAX_W: u32 = HUD_RIGHT - HUD_X - PAD;

/// マッチ中の帯の文字（GDI の文字）の枠の幅（画素）。`overlay_win` が文字の高さ 14 から `cell_w = 高さ / 2 切り上げ` = 7 で作る。
pub const HUD_CELL_W: u32 = 7;

/// 対戦中の名前の帯と下の通信の状態の行の文字の左端（画素の x）。帯は画面の端に接し、文字は端から `PAD` ずつ内側。
/// 下地は文字の左右に `PAD` ずつ広げる（左の帯は x = 0 から、右の帯は画面の右端まで）。
pub const HUD_EDGE_X: u32 = PAD;

/// 対戦中の名前の帯: 上の枠の左に「1P 名前」、右に「名前 2P」（右寄せ）。幅の上限は左右とも、
/// 中央のタイマーに届かない `HUD_RIGHT` までの幅。
pub const HUD_NAME_MAX_W: u32 = HUD_RIGHT - HUD_EDGE_X - PAD;
/// 名前の帯の文字の枠の数の上限（半角 1 枠。全角は 2 枠）。ラベル「1P 」「 2P」の 3 枠を引いた残り。
/// 名前は HELLO で 32 バイトまでなので、ふつうは切れない。
pub const HUD_NAME_CELLS: u32 = HUD_NAME_MAX_W / HUD_CELL_W - 3;
const _: () = assert!(HUD_NAME_CELLS >= 32, "半角 32 文字の名前は切らずに収まる");

/// 通信の状態の行を下の枠の左へ置く。帯は y = `screen_h - HUD_BAND_H` から、文字の枠の上端はその `HUD_Y` 下。
pub fn hud_bottom_y(screen_h: u32) -> u32 {
    screen_h.saturating_sub(HUD_BAND_H)
}
/// 下の枠の中央のゲームの fps 表示（「60.00fps」）の文字の左の縁（画素の x）。実機の撮影 1 枚から測った。
pub const FPS_LEFT: u32 = 284;
/// 通信の状態の行の下地の右端（この x は含まない）の上限。fps の文字の縁まで半角 1 文字ぶんあける。
/// 実際に出るいちばん広い形（39 文字 = 273 画素）がちょうど収まる。
pub const HUD_BOTTOM_RIGHT: u32 = FPS_LEFT - HUD_CELL_W;
/// 通信の状態の行の文字を置ける幅（画素）。越える文字は描く側（`push_text_plain` の `max_w`）が落とす。
pub const HUD_BOTTOM_TEXT_MAX_W: u32 = HUD_BOTTOM_RIGHT - HUD_EDGE_X - PAD;

/// 右寄せの帯の文字の左端（画素の x）。画面の右端から `HUD_EDGE_X` あけ、文字の幅 `w` ぶん左へ。
pub fn hud_right_text_x(screen_w: u32, w: u32) -> u32 {
    screen_w.saturating_sub(HUD_EDGE_X.saturating_add(w))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowStyle {
    #[default]
    Normal,
    Selected,
    Title,
    Hint,
    Warn,
    /// ふつうの色で、小さい文字（`overlay::SMALL_CELL_H`）で描く行。観戦者の左下の通信の状態の行だけ。
    Small,
    /// 文字の無い隙間の行。高さは行の 1/3・1/2・1 行分（`RowStyle::height`）。
    GapSmall,
    GapMedium,
    GapLarge,
}

impl RowStyle {
    pub fn code(self) -> u32 {
        match self {
            RowStyle::Normal => 0,
            RowStyle::Selected => 1,
            RowStyle::Title => 2,
            RowStyle::Hint => 3,
            RowStyle::Warn => 4,
            RowStyle::Small => 5,
            RowStyle::GapSmall => 6,
            RowStyle::GapMedium => 7,
            RowStyle::GapLarge => 8,
        }
    }

    pub fn from_code(c: u32) -> RowStyle {
        match c {
            1 => RowStyle::Selected,
            2 => RowStyle::Title,
            3 => RowStyle::Hint,
            4 => RowStyle::Warn,
            5 => RowStyle::Small,
            6 => RowStyle::GapSmall,
            7 => RowStyle::GapMedium,
            8 => RowStyle::GapLarge,
            _ => RowStyle::Normal,
        }
    }

    pub fn color(self) -> u32 {
        match self {
            RowStyle::Normal
            | RowStyle::Small
            | RowStyle::GapSmall
            | RowStyle::GapMedium
            | RowStyle::GapLarge => COLOR_TEXT,
            RowStyle::Selected => COLOR_SELECTED,
            RowStyle::Title => COLOR_TITLE,
            RowStyle::Hint => COLOR_HINT,
            RowStyle::Warn => COLOR_WARN,
        }
    }

    /// 行の高さ（画素）。`row_h` は文字の行の高さ（`row_height`）。
    pub fn height(self, row_h: u32) -> u32 {
        match self {
            RowStyle::GapSmall => row_h / 3,
            RowStyle::GapMedium => row_h / 2,
            _ => row_h,
        }
    }
}

pub fn row_height(spec: &AtlasSpec) -> u32 {
    spec.cell_h + 2 * PAD
}

/// 並んだ行の高さの合計（画素）。メニューの枠の高さと、行の上端の位置（先頭から前の行までの合計）の元。
pub fn rows_height(spec: &AtlasSpec, styles: &[RowStyle]) -> u32 {
    let row_h = row_height(spec);
    styles.iter().map(|s| s.height(row_h)).sum()
}

/// 1 行 `text` の幅（画素）。表に無い文字は `?` の幅（描く文字と同じ幅）。`MAX_TEXT` 文字で切る（`push_text` と同じ）。
pub fn text_width(spec: &AtlasSpec, text: &str) -> u32 {
    width_by(spec, text, resolve)
}

/// `text_width_plain` の、小さい文字のバージョン（`overlay::resolve_small`。観戦者の左下の行）。
pub fn text_width_small(spec: &AtlasSpec, text: &str) -> u32 {
    width_by(spec, text, resolve_small)
}

/// 帯（上端 `band_y`・高さ `HUD_BAND_H`）の真ん中に小さい文字を置くときの、文字の枠の上端の y（`push_text_small` に渡す）。
/// 小さい文字の絵は枠の下端に揃う（`Face::Game`）ので、枠の上端は「絵の下端 − 枠の高さ」。
pub fn small_text_y(spec: &AtlasSpec, band_y: u32) -> u32 {
    (band_y + HUD_BAND_H.div_ceil(2) + SMALL_CELL_H.div_ceil(2)).saturating_sub(spec.cell_h)
}

/// `text_width` の、ゲームの文字を使わないバージョン（`push_text_plain` と同じ幅。英数字も GDI の枠の幅）。
pub fn text_width_plain(spec: &AtlasSpec, text: &str) -> u32 {
    width_by(spec, text, resolve_plain)
}

fn width_by(spec: &AtlasSpec, text: &str, by: Resolver) -> u32 {
    let fallback = spec.slot_of('?');
    text.chars()
        .take(MAX_TEXT)
        .map(|c| by(spec, c, fallback).0)
        .fold(0u32, u32::saturating_add)
}

/// 四角形 1 つ（塗りの枠を貼る）を `out[at..]` へ書く。左上 `(x, y)`・幅 `w`・高さ `h`（画素）。書けたら次の位置、入らなければ `at`。
pub fn push_rect(
    out: &mut [Vertex],
    at: usize,
    spec: &AtlasSpec,
    (x, y, w, h): (u32, u32, u32, u32),
    color: u32,
) -> usize {
    if w == 0 || h == 0 {
        return at;
    }
    let px = [
        x as f32,
        y as f32,
        x.saturating_add(w) as f32,
        y.saturating_add(h) as f32,
    ];
    if quad(out, at, px, slot_uv(spec, spec.solid_slot()), color) {
        at + 6
    } else {
        at
    }
}

/// 1 行 `text` の文字を、左上 `(x, y)`（文字の枠の左上）から `out[at..]` へ書く。幅が `max_w` を越える文字・`MAX_TEXT` 文字を越える文字・
/// `out` に入らない文字は落とす。戻りは（次の位置, 描いた幅）。下地は置かない（呼び手が `push_rect` で）。
pub fn push_text(
    out: &mut [Vertex],
    at: usize,
    spec: &AtlasSpec,
    pos: (u32, u32),
    text: &str,
    color: u32,
    max_w: u32,
) -> (usize, u32) {
    push_by(out, at, spec, pos, text, color, max_w, resolve)
}

/// `push_text` の、ゲームの文字を使わないバージョン: 英数字も GDI の文字の枠（高さ `cell_h`・半角 `cell_w`）で描き、空白も枠の幅で進む
/// （`overlay::resolve_plain`）。拡大縮小はしない（点の標本化なので縮めると崩れる）。
pub fn push_text_plain(
    out: &mut [Vertex],
    at: usize,
    spec: &AtlasSpec,
    pos: (u32, u32),
    text: &str,
    color: u32,
    max_w: u32,
) -> (usize, u32) {
    push_by(out, at, spec, pos, text, color, max_w, resolve_plain)
}

/// `push_text_plain` の、小さい文字のバージョン（観戦者の左下の行。文字の枠の上端は `small_text_y`）。
pub fn push_text_small(
    out: &mut [Vertex],
    at: usize,
    spec: &AtlasSpec,
    pos: (u32, u32),
    text: &str,
    color: u32,
    max_w: u32,
) -> (usize, u32) {
    push_by(out, at, spec, pos, text, color, max_w, resolve_small)
}

// 引数が 8 つになるのは `push_text` の 7 つに文字の決め方を足したため。
#[allow(clippy::too_many_arguments)]
fn push_by(
    out: &mut [Vertex],
    at: usize,
    spec: &AtlasSpec,
    (x, y): (u32, u32),
    text: &str,
    color: u32,
    max_w: u32,
    by: Resolver,
) -> (usize, u32) {
    let fallback = spec.slot_of('?');
    let mut at = at;
    let mut used = 0u32;
    for c in text.chars().take(MAX_TEXT) {
        let (w, face) = by(spec, c, fallback);
        if used.saturating_add(w) > max_w {
            break;
        }
        let gx = x.saturating_add(used) as f32;
        let top = y as f32;
        let placed = match face {
            Face::None => None,
            Face::Cell(slot) => Some((
                [gx, top, gx + w as f32, top + spec.cell_h as f32],
                slot_uv(spec, slot),
            )),
            Face::Game(g) => {
                // `overlay::layout_line` と同じ: 文字の絵の下端を枠の下端に揃える。
                let bottom = (y + spec.cell_h) as f32 + GAME_BASELINE_SHIFT as f32;
                let left = game_glyph_left(spec, gx, &g);
                Some((
                    [left, bottom - g.h as f32, left + g.w as f32, bottom],
                    rect_uv(spec, g.x, g.y, g.w, g.h),
                ))
            }
        };
        if let Some((px, uv)) = placed {
            if !quad(out, at, px, uv, color) {
                break;
            }
            at += 6;
        }
        used += w;
    }
    (at, used)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// 文字を置く左端と、1 行目の文字の枠の上端。行 `i` の文字の枠の上端は `text_y + i * row_h`（`row_h` = `row_height`）。
    pub text_x: u32,
    pub text_y: u32,
    /// 文字を置ける幅（これを越える文字は `push_text` が落とす）。
    pub text_w: u32,
}

/// 高さ `rows_h` 画素（`rows_height`）の行・いちばん広い行の幅 `max_text_w` の枠を、画面（`screen_w` × `screen_h`）の中央に置く。画面に入りきらなければ
/// 画面の余白（`SCREEN_MARGIN`）まで縮める（文字は `text_w` で切れる）。
pub fn menu_box(screen_w: u32, screen_h: u32, rows_h: u32, max_text_w: u32) -> MenuBox {
    let want_w = max_text_w.saturating_add(2 * (BOX_MARGIN + BORDER));
    let want_h = rows_h.saturating_add(2 * (BOX_MARGIN + BORDER));
    let w = want_w.min(screen_w.saturating_sub(2 * SCREEN_MARGIN));
    let h = want_h.min(screen_h.saturating_sub(2 * SCREEN_MARGIN));
    let x = screen_w.saturating_sub(w) / 2;
    let y = screen_h.saturating_sub(h) / 2;
    MenuBox {
        x,
        y,
        w,
        h,
        text_x: x + BOX_MARGIN + BORDER,
        text_y: y + BOX_MARGIN + BORDER,
        text_w: w.saturating_sub(2 * (BOX_MARGIN + BORDER)),
    }
}

/// 画面を暗くする四角と、枠の枠線・下地を `out[0..]` へ書く（18 頂点。`MENU_BACK_VERTS`）。戻りは書いた頂点の数。
pub fn push_menu_back(
    out: &mut [Vertex],
    spec: &AtlasSpec,
    screen_w: u32,
    screen_h: u32,
    b: &MenuBox,
) -> usize {
    let mut at = push_rect(out, 0, spec, (0, 0, screen_w, screen_h), COLOR_DIM);
    at = push_rect(out, at, spec, (b.x, b.y, b.w, b.h), COLOR_BORDER);
    push_rect(
        out,
        at,
        spec,
        (
            b.x + BORDER,
            b.y + BORDER,
            b.w.saturating_sub(2 * BORDER),
            b.h.saturating_sub(2 * BORDER),
        ),
        COLOR_BOX,
    )
}

/// `push_menu_back` が書く頂点の数（四角 3 つ）。
pub const MENU_BACK_VERTS: usize = 18;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{blit_quads, ui_glyphs};

    fn spec() -> AtlasSpec {
        AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap()
    }

    /// 全面が不透明な白のテクスチャ（頂点の色がそのまま画素の色になる）。
    fn white(spec: &AtlasSpec) -> Vec<u32> {
        vec![0xFFFF_FFFF; spec.texel_count()]
    }

    fn pixel(canvas: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * w as usize + x as usize) * 3;
        [canvas[i], canvas[i + 1], canvas[i + 2]]
    }

    #[test]
    fn menu_text_centers_a_game_glyph_wider_than_the_advance_on_its_cell() {
        use crate::overlay::{GAME_ADVANCE, GameGlyphSrc, apply_game_font, texels};
        let base = AtlasSpec::fit_game(7, 14, &ui_glyphs()).unwrap();
        let t = texels(&base, &vec![0; base.texel_count()]).unwrap();
        let px = vec![0xFFFF_FFFFu32; 13 * 12];
        let gs: Vec<GameGlyphSrc> = ['A', 'B']
            .iter()
            .map(|&c| GameGlyphSrc {
                c,
                w: 13,
                h: 12,
                pixels: &px,
                quad_w: 13,
                quad_h: 12,
            })
            .collect();
        let (s, _) = apply_game_font(&base, &t, &gs, GAME_ADVANCE).unwrap();
        let mut out = [Vertex::default(); 64];
        let (at, used) = push_text(&mut out, 0, &s, (100, 50), "AB◀", 0xFFFF_FFFF, 400);
        assert_eq!((at, used), (18, 9 + 9 + 14));
        let left = |q: usize| out[q * 6].x + 0.5;
        assert_eq!(
            left(0),
            98.0,
            "絵 13 画素の真ん中を枠 100..109 の真ん中に（2 画素左から）"
        );
        assert_eq!(left(1), 107.0);
        assert_eq!(left(2), 118.0, "GDI の文字は枠の左端のまま");
    }

    #[test]
    fn widths_follow_the_glyph_cells_and_the_question_mark_fallback() {
        let s = spec();
        assert_eq!(text_width(&s, ""), 0);
        assert_eq!(text_width(&s, "ab"), 14, "半角 2 文字 = 7 × 2");
        assert_eq!(text_width(&s, "接続"), 28, "全角 2 文字 = 14 × 2");
        assert_eq!(text_width(&s, "接続 ab"), 28 + 7 + 14);
        // 表に無い文字（`鬱`）は `?`（半角 1 枠）で数える。描く文字と同じ。
        assert!(s.slot_of('鬱').is_none() && s.slot_of('?').is_some());
        assert_eq!(text_width(&s, "鬱"), 7, "描く文字（`?`）の 1 枠で進む");
        let long: String = "a".repeat(MAX_TEXT + 50);
        assert_eq!(text_width(&s, &long), 7 * MAX_TEXT as u32);
    }

    #[test]
    fn a_rect_lands_exactly_and_never_overflows() {
        let s = spec();
        let mut out = [Vertex::default(); 12];
        assert_eq!(push_rect(&mut out, 0, &s, (10, 20, 30, 40), 0xFF11_2233), 6);
        // 画素の端の座標（0.5 引く）。
        assert_eq!((out[0].x, out[0].y), (9.5, 19.5));
        assert_eq!((out[5].x, out[5].y), (39.5, 59.5));
        assert_eq!(out[0].color, 0xFF11_2233);
        assert_eq!(
            push_rect(&mut out, 6, &s, (0, 0, 0, 5), 1),
            6,
            "幅 0 は書かない"
        );
        assert_eq!(
            push_rect(&mut out, 6, &s, (0, 0, 5, 0), 1),
            6,
            "高さ 0 は書かない"
        );
        assert_eq!(push_rect(&mut out, 6, &s, (0, 0, 5, 5), 1), 12);
        assert_eq!(push_rect(&mut out, 12, &s, (0, 0, 5, 5), 1), 12, "入らない");
        let mut tiny = [Vertex::default(); 5];
        assert_eq!(push_rect(&mut tiny, 0, &s, (0, 0, 5, 5), 1), 0);
    }

    #[test]
    fn text_is_placed_from_the_top_left_and_cut_by_width_and_capacity() {
        let s = spec();
        let mut out = [Vertex::default(); 64];
        let (at, w) = push_text(&mut out, 0, &s, (100, 50), "a b", 0xFFFF_FFFF, 1000);
        assert_eq!((at, w), (12, 21), "空白は四角を書かずに 1 枠進む");
        assert_eq!(out[0].x, 99.5);
        assert_eq!(out[0].y, 49.5);
        assert_eq!(out[6].x, 113.5);
        assert_eq!(w, text_width(&s, "a b"));
        let (at, w) = push_text(&mut out, 0, &s, (0, 0), "接続接", 1, 30);
        assert_eq!((at, w), (12, 28));
        let mut tiny = [Vertex::default(); 6];
        let (at, w) = push_text(&mut tiny, 0, &s, (0, 0), "abc", 1, 1000);
        assert_eq!((at, w), (6, 7));
    }

    #[test]
    fn the_menu_box_is_centered_and_clamped_to_the_screen() {
        let b = menu_box(640, 480, 10 * 18, 300);
        assert_eq!(b.w, 300 + 2 * 12);
        assert_eq!(b.h, 10 * 18 + 2 * 12);
        assert_eq!((b.x, b.y), ((640 - b.w) / 2, (480 - b.h) / 2));
        assert_eq!((b.text_x, b.text_y), (b.x + 12, b.y + 12));
        assert_eq!(b.text_w, 300);
        let wide = menu_box(640, 480, 40 * 18, 5000);
        assert_eq!(wide.w, 640 - 8);
        assert_eq!(wide.h, 480 - 8);
        assert_eq!((wide.x, wide.y), (4, 4));
        assert!(wide.text_w <= wide.w);
        let small = menu_box(10, 10, 3 * 18, 100);
        assert!(small.x + small.w <= 10 && small.y + small.h <= 10);
    }

    #[test]
    fn the_menu_back_dims_the_screen_and_draws_a_bordered_box() {
        let s = spec();
        let texels = white(&s);
        let (sw, sh) = (160u32, 120u32);
        let b = menu_box(sw, sh, 3 * 18, 60);
        let mut out = [Vertex::default(); MENU_BACK_VERTS];
        assert_eq!(push_menu_back(&mut out, &s, sw, sh, &b), MENU_BACK_VERTS);
        let mut canvas = vec![200u8; (sw * sh * 3) as usize];
        let drawn = blit_quads(&out, &texels, s.tex_w, s.tex_h, &mut canvas, sw, sh, 0, 0);
        assert_eq!(drawn, 3, "四角形 3 つ");
        let corner = pixel(&canvas, sw, 0, 0);
        assert!(corner.iter().all(|&c| c < 100), "{corner:?}");
        let edge = pixel(&canvas, sw, b.x, b.y + b.h / 2);
        assert_eq!(edge, [0xB0, 0xC0, 0xE0]);
        let inner = pixel(&canvas, sw, b.x + b.w / 2, b.y + b.h / 2);
        assert!(inner[0] < 0x30 && inner[2] > 0x20, "{inner:?}");
    }

    #[test]
    fn gaps_are_a_third_a_half_and_a_whole_row_and_add_up() {
        let s = spec();
        let row_h = row_height(&s);
        assert_eq!(row_h, 18);
        assert_eq!(RowStyle::GapSmall.height(row_h), 6);
        assert_eq!(RowStyle::GapMedium.height(row_h), 9);
        assert_eq!(RowStyle::GapLarge.height(row_h), 18);
        assert_eq!(RowStyle::Title.height(row_h), 18);
        let styles = [
            RowStyle::Title,
            RowStyle::GapSmall,
            RowStyle::GapMedium,
            RowStyle::GapLarge,
            RowStyle::Normal,
        ];
        assert_eq!(rows_height(&s, &styles), 18 + 6 + 9 + 18 + 18);
        assert_eq!(rows_height(&s, &[]), 0);
    }

    #[test]
    fn row_styles_round_trip_and_have_distinct_colors() {
        let all = [
            RowStyle::Normal,
            RowStyle::Selected,
            RowStyle::Title,
            RowStyle::Hint,
            RowStyle::Warn,
        ];
        for st in all {
            assert_eq!(RowStyle::from_code(st.code()), st);
        }
        assert_eq!(RowStyle::from_code(99), RowStyle::Normal);
        for st in [RowStyle::GapSmall, RowStyle::GapMedium, RowStyle::GapLarge] {
            assert_eq!(RowStyle::from_code(st.code()), st);
        }
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.color(), b.color());
            }
        }
    }
}
