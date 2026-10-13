//! ウィンドウモードの起動時のウィンドウの大きさ・位置・タイトルバー・最前面（ini の `[window]`）を決める（unsafe 無し）。
//!
//! 「ウィンドウだけ大きくする」作り: D3D8 の裏の画面は 640×480 のまま、Present の引き伸ばしに任せる。ここは ini の読みと
//! `CreateWindowExA` に渡す値の計算だけで、Win32 は呼ばない（呼ぶ側は `src/dll/src/window_win.rs`）。
//!
//! ini（adonis rev4 / rev5 と vpatch の `[Window]` に寄せた名前。キーの大文字・小文字は区別されない）:
//! - `[window]` が無ければ何もしない（`from_ini` が `None`）。今のゲームのウィンドウのまま。
//! - `width` / `height`: client 領域（枠を除いた中身）の画素数。既定 640 / 480。整数倍でなくてよい。
//! - `x` / `y`: ウィンドウの左上（枠を含む外側の位置）。両方書いたときだけ使う。書かなければ `CW_USEDEFAULT`。
//! - `titlebar`: 1（既定）= 今の枠のまま、0 = 枠なし。
//! - `alwaysontop`: 1 = 最前面、0（既定）= 通常。
//!
//! 知らないキー・読めない値・範囲外の値があれば、その節を丸ごと使わない（`Err`。`[netplay]` / `[pacing]` と同じ）。
#![forbid(unsafe_code)]

use crate::ini::Ini;
use crate::patch;

pub const SECTION: &str = "window";
pub const KEYS: [&str; 6] = ["width", "height", "x", "y", "titlebar", "alwaysontop"];

/// 既定の client の大きさ。ゲームのウィンドウの大きさ（`0x42D584` の `lea 0x280(...)` と `0x42D59E` の `lea 0x1e0(...)`）と同じ。
pub const DEFAULT_WIDTH: i32 = 640;
pub const DEFAULT_HEIGHT: i32 = 480;
/// width / height の範囲（本実装の決め）: 下は既定の 4 分の 1（これより小さいと枠の最小幅に負ける）、上は 8K。
pub const MIN_WIDTH: i32 = 160;
pub const MIN_HEIGHT: i32 = 120;
pub const MAX_WIDTH: i32 = 7680;
pub const MAX_HEIGHT: i32 = 4320;
/// x / y の範囲。複数のモニターで左や上に置く負の値を通す。
pub const MIN_POS: i32 = -32768;
pub const MAX_POS: i32 = 32767;

/// `CreateWindowExA` の x / y に置くと「システムが決める」になる値（`0x80000000`）。ゲームも `0x42D5A7` / `0x42D5AC` で置く。
pub const CW_USEDEFAULT: i32 = i32::MIN;
/// ウィンドウスタイルの `WS_POPUP`（枠も title も無い）と `WS_VISIBLE`。
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowConfig {
    pub client_w: i32,
    pub client_h: i32,
    /// ウィンドウの左上。`None` なら `CW_USEDEFAULT`。
    pub pos: Option<(i32, i32)>,
    pub title_bar: bool,
    pub always_on_top: bool,
}

impl WindowConfig {
    /// `[window]` の節だけがあって何も書かなかったときの形（今のゲームの 640×480 の client・枠あり・位置は今のまま）。
    pub const DEFAULT: WindowConfig = WindowConfig {
        client_w: DEFAULT_WIDTH,
        client_h: DEFAULT_HEIGHT,
        pos: None,
        title_bar: true,
        always_on_top: false,
    };

    pub fn describe(&self) -> String {
        let pos = match self.pos {
            Some((x, y)) => format!("{x},{y}"),
            None => "今のまま".to_string(),
        };
        format!(
            "client {}×{}、位置 {pos}、タイトルバー {}、最前面 {}",
            self.client_w,
            self.client_h,
            if self.title_bar { "あり" } else { "なし" },
            if self.always_on_top {
                "あり"
            } else {
                "なし"
            },
        )
    }
}

/// 整数（`-` は先頭に 1 つだけ。`+` や小数点や単位は断る）を読み、`[min, max]` を確かめる。
fn parse_int(key: &str, v: &str, min: i32, max: i32) -> Result<i32, String> {
    let t = v.trim();
    let digits = t.strip_prefix('-').unwrap_or(t);
    let n = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| t.parse::<i32>().ok())
        .flatten();
    match n {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(format!(
            "[{SECTION}] の {key} の値 {v:?} は {min}〜{max} の整数"
        )),
    }
}

fn parse_flag(key: &str, v: &str) -> Result<bool, String> {
    match v.trim() {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("[{SECTION}] の {key} の値 {v:?} は 0 か 1")),
    }
}

/// ini の `[window]` を読む。節が無ければ `Ok(None)`（今のウィンドウのまま）。
pub fn from_ini(ini: &Ini) -> Result<Option<WindowConfig>, String> {
    if !ini.has_section(SECTION) {
        return Ok(None);
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} を知らない（読めるのは {}）",
                KEYS.join(" / ")
            ));
        }
    }
    let get = |key: &str| ini.get(SECTION, key);
    let client_w = match get("width") {
        None => DEFAULT_WIDTH,
        Some(v) => parse_int("width", v, MIN_WIDTH, MAX_WIDTH)?,
    };
    let client_h = match get("height") {
        None => DEFAULT_HEIGHT,
        Some(v) => parse_int("height", v, MIN_HEIGHT, MAX_HEIGHT)?,
    };
    let pos = match (get("x"), get("y")) {
        (None, None) => None,
        (Some(x), Some(y)) => Some((
            parse_int("x", x, MIN_POS, MAX_POS)?,
            parse_int("y", y, MIN_POS, MAX_POS)?,
        )),
        // 片方だけでは、書かなかった側を「今のまま」にできない（x が既定値だと y は無視される）。
        _ => {
            return Err(format!(
                "[{SECTION}] の x と y は両方書いたときだけ使う（片方だけでは位置を決めない）"
            ));
        }
    };
    let title_bar = match get("titlebar") {
        None => true,
        Some(v) => parse_flag("titlebar", v)?,
    };
    let always_on_top = match get("alwaysontop") {
        None => false,
        Some(v) => parse_flag("alwaysontop", v)?,
    };
    Ok(Some(WindowConfig {
        client_w,
        client_h,
        pos,
        title_bar,
        always_on_top,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMetrics {
    /// `SM_CXFIXEDFRAME`（7）。
    pub fixed_frame_x: i32,
    /// `SM_CYFIXEDFRAME`（8）。
    pub fixed_frame_y: i32,
    /// `SM_CYCAPTION`（4）。
    pub caption_y: i32,
}

/// ゲームがウィンドウの外寸を作る式（`0x42D584`: `0x280 + 2 × GetSystemMetrics(7)`、`0x42D59E`:
/// `0x1E0 + GetSystemMetrics(4) + 2 × GetSystemMetrics(8)`）に client の大きさを入れたもの。
pub fn game_outer_size(client_w: i32, client_h: i32, m: &FrameMetrics) -> (i32, i32) {
    (
        client_w.saturating_add(m.fixed_frame_x.saturating_mul(2)),
        client_h
            .saturating_add(m.caption_y)
            .saturating_add(m.fixed_frame_y.saturating_mul(2)),
    )
}

/// `CreateWindowExA` の引数のうち、こちらが替える 5 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateArgs {
    pub style: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// `orig_style` はゲームが渡した style（ウィンドウモードでは `0x100A0000`）。タイトルバーありはその style のままにして外寸だけ
/// client から作り直し、なしは `WS_POPUP` にして外寸 = client。`WS_VISIBLE` はゲームの style のものを引き継ぐ。
pub fn plan(cfg: &WindowConfig, orig_style: u32, m: &FrameMetrics) -> CreateArgs {
    let (x, y) = cfg.pos.unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
    if cfg.title_bar {
        let (width, height) = game_outer_size(cfg.client_w, cfg.client_h, m);
        CreateArgs {
            style: orig_style,
            x,
            y,
            width,
            height,
        }
    } else {
        CreateArgs {
            style: WS_POPUP | (orig_style & WS_VISIBLE),
            x,
            y,
            width: cfg.client_w,
            height: cfg.client_h,
        }
    }
}

/// 作ったウィンドウの client が指定と違ったときの、直した外寸。`game_outer_size` の枠の分が OS の実際の枠と食い違うため要る
/// （検査用のウィンドウで、式は +6 × +29、実際は +16 × +39（実測: Windows 11 の 1 台））。作った後の測定から直して client を守る。
/// `outer` は作ったウィンドウの外寸、`client` は測った client。合っていれば `None`。
pub fn corrected_outer(
    want: (i32, i32),
    client: (i32, i32),
    outer: (i32, i32),
) -> Option<(i32, i32)> {
    (client != want).then(|| {
        (
            outer.0.saturating_add(want.0).saturating_sub(client.0),
            outer.1.saturating_add(want.1).saturating_sub(client.1),
        )
    })
}

/// `CreateWindowExA` を呼ぶ `FF 15 abs32`（6 バイト）の置き換え: `E8 rel32` ＋ `90`。元の命令が 6 バイトで、
/// `call` の戻りアドレスが 5 バイト目の次になるので nop を通って続く。
pub fn call_patch(site: u32, target: u32) -> [u8; 6] {
    let c = patch::call_rel32(site, target);
    [c[0], c[1], c[2], c[3], c[4], 0x90]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ini;

    fn read(text: &str) -> Result<Option<WindowConfig>, String> {
        from_ini(&ini::parse(text).expect("ini が読める"))
    }

    /// 今のゲームのウィンドウの枠（96 dpi の標準）。
    const M: FrameMetrics = FrameMetrics {
        fixed_frame_x: 3,
        fixed_frame_y: 3,
        caption_y: 23,
    };
    const GAME_STYLE: u32 = 0x100A_0000;

    #[test]
    fn no_section_is_none_and_a_bare_header_is_the_default() {
        assert_eq!(read(""), Ok(None));
        assert_eq!(
            read("[netplay]\nwidth = 1000\n[pacing]\nmode = auto\n"),
            Ok(None)
        );
        assert_eq!(read("[window]\n"), Ok(Some(WindowConfig::DEFAULT)));
    }

    #[test]
    fn all_keys_are_read_and_non_integer_scales_pass() {
        let c = read(
            "[window]\nWidth = 1000\nHeight = 750\nX = -10\nY = 20\nTitleBar = 0\nAlwaysOnTop = 1\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            c,
            WindowConfig {
                client_w: 1000,
                client_h: 750,
                pos: Some((-10, 20)),
                title_bar: false,
                always_on_top: true,
            }
        );
        let odd = read("[window]\nwidth = 1001\nheight = 563\n")
            .unwrap()
            .unwrap();
        assert_eq!((odd.client_w, odd.client_h), (1001, 563));
        assert!(odd.title_bar && !odd.always_on_top && odd.pos.is_none());
    }

    #[test]
    fn out_of_range_or_malformed_values_refuse_the_whole_section() {
        let ok = |t: &str| read(t).is_ok();
        assert!(ok(&format!("[window]\nwidth = {MIN_WIDTH}\n")));
        assert!(ok(&format!("[window]\nwidth = {MAX_WIDTH}\n")));
        assert!(ok(&format!("[window]\nheight = {MIN_HEIGHT}\n")));
        assert!(ok(&format!("[window]\nheight = {MAX_HEIGHT}\n")));
        for bad in [
            format!("[window]\nwidth = {}\n", MIN_WIDTH - 1),
            format!("[window]\nwidth = {}\n", MAX_WIDTH + 1),
            format!("[window]\nheight = {}\n", MIN_HEIGHT - 1),
            format!("[window]\nheight = {}\n", MAX_HEIGHT + 1),
            "[window]\nwidth = 0\n".into(),
            "[window]\nwidth = -640\n".into(),
            "[window]\nwidth = 640px\n".into(),
            "[window]\nwidth = 640.5\n".into(),
            "[window]\nwidth = 0x280\n".into(),
            "[window]\nwidth = +640\n".into(),
            "[window]\nwidth =\n".into(),
            "[window]\nwidth = 99999999999\n".into(),
            "[window]\nx = 1\ny = -2147483648\n".into(),
            "[window]\nx = 32768\ny = 0\n".into(),
            "[window]\nx = 0\ny = -32769\n".into(),
            "[window]\nx = -\ny = 0\n".into(),
            "[window]\nx = 10\n".into(),
            "[window]\ny = 10\n".into(),
            "[window]\ntitlebar = 2\n".into(),
            "[window]\ntitlebar = yes\n".into(),
            "[window]\nalwaysontop = true\n".into(),
            "[window]\nscale = 2\n".into(),
        ] {
            let e = read(&bad).expect_err(&bad);
            assert!(e.contains("[window]"), "{bad:?}: {e}");
        }
        assert!(ok("[window]\nx = -32768\ny = 32767\n"));
    }

    #[test]
    fn default_plan_equals_what_the_game_builds_today() {
        let p = plan(&WindowConfig::DEFAULT, GAME_STYLE, &M);
        // ゲーム: 幅 = 0x280 + 3 × 2、高さ = 0x1E0 + 23 + 3 × 2。
        assert_eq!(p.width, 0x280 + 6);
        assert_eq!(p.height, 0x1E0 + 23 + 6);
        assert_eq!(
            p,
            CreateArgs {
                style: GAME_STYLE,
                x: CW_USEDEFAULT,
                y: CW_USEDEFAULT,
                width: 646,
                height: 509,
            }
        );
        let m2 = FrameMetrics {
            fixed_frame_x: 4,
            fixed_frame_y: 5,
            caption_y: 31,
        };
        let p2 = plan(&WindowConfig::DEFAULT, GAME_STYLE, &m2);
        assert_eq!((p2.width, p2.height), (640 + 8, 480 + 31 + 10));
    }

    #[test]
    fn title_bar_changes_the_outer_size_and_style() {
        let on = WindowConfig {
            client_w: 1280,
            client_h: 960,
            pos: Some((100, 50)),
            title_bar: true,
            always_on_top: false,
        };
        let off = WindowConfig {
            title_bar: false,
            ..on
        };
        let pon = plan(&on, GAME_STYLE, &M);
        let poff = plan(&off, GAME_STYLE, &M);
        assert_eq!((pon.width, pon.height), (1286, 960 + 23 + 6));
        assert_eq!((poff.width, poff.height), (1280, 960));
        assert_ne!((pon.width, pon.height), (poff.width, poff.height));
        assert_eq!(pon.style, GAME_STYLE);
        assert_eq!(poff.style, WS_POPUP | WS_VISIBLE);
        assert_eq!((pon.x, pon.y, poff.x, poff.y), (100, 50, 100, 50));
        // 見えないウィンドウ（検査）なら、枠なしも見えないまま。
        assert_eq!(plan(&off, GAME_STYLE & !WS_VISIBLE, &M).style, WS_POPUP);
    }

    #[test]
    fn correction_moves_the_outer_size_by_the_client_error() {
        assert_eq!(corrected_outer((640, 480), (640, 480), (646, 509)), None);
        // 枠が無いウィンドウに枠の分を足して作った（client 646×509）→ 640×480 に縮める。
        assert_eq!(
            corrected_outer((640, 480), (646, 509), (646, 509)),
            Some((640, 480))
        );
        // 枠の分を足し足りなかった（client が小さい）→ 広げる。
        assert_eq!(
            corrected_outer((1280, 960), (1274, 937), (1280, 960)),
            Some((1286, 983))
        );
    }

    #[test]
    fn call_patch_is_call_rel32_plus_nop() {
        let p = call_patch(0x0042_D5C2, 0x1000_0000);
        assert_eq!(p[0], 0xE8);
        assert_eq!(p[5], 0x90);
        let rel = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
        assert_eq!(
            0x0042_D5C2u32.wrapping_add(5).wrapping_add(rel),
            0x1000_0000
        );
        let q = call_patch(0x1000_0000, 0x0042_D5C2);
        let rel = u32::from_le_bytes([q[1], q[2], q[3], q[4]]);
        assert_eq!(
            0x1000_0000u32.wrapping_add(5).wrapping_add(rel),
            0x0042_D5C2
        );
    }

    #[test]
    fn the_dist_ini_example_is_readable_and_off_by_default() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini").replace("\r\n", "\n");
        assert_eq!(read(&dist), Ok(None), "配る ini のままでは [window] が無効");
        // `;[window]` から次の節（または末尾）までの、先頭が `; ` の行を外す。
        let start = dist
            .find("\n; [window]\n")
            .expect("配る ini に注の [window] が無い")
            + 1;
        let tail = &dist[start..];
        let body: String = tail
            .lines()
            .map(|l| l.strip_prefix("; ").unwrap_or(l))
            .filter(|l| *l == "[window]" || KEYS.iter().any(|k| l.starts_with(&format!("{k} ="))))
            .collect::<Vec<_>>()
            .join("\n");
        let c = read(&body).expect(&body).expect("注を外したら節がある");
        assert!(c.client_w >= MIN_WIDTH && c.client_h >= MIN_HEIGHT, "{c:?}");
        assert!(c.pos.is_some(), "見本は位置も書く: {c:?}");
    }
}
