//! 読み込まれている`d3d8.dll`を本ログの1行にする（純粋な文字の組み立て）。
//!
//! 落ちたときに「どのd3d8.dllの上か」（OSのものかゲームのフォルダの差し替えか）をVEHの`d3d8.dll+RVA`と突き合わせて読むため。
//! フォルダ名にはユーザー名が入りうるのでパスは書かず「場所の種類」だけにする。

#![forbid(unsafe_code)]

/// 欄が取れなかったときに書く語。
pub const NONE_TEXT: &str = "無し";

/// 1つのフィールドの最大の文字数（バージョンの文字列に長い説明が入っていても行が伸びないように）。
const FIELD_MAX_CHARS: usize = 64;

/// 場所の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// ゲーム（exe）のフォルダ。差し替えのd3d8.dllはここに置かれる。
    Game,
    /// システムのフォルダ（`GetSystemDirectoryW`など）。
    System,
    /// どちらでもない（ゲームのフォルダの下の別のフォルダも含む）。
    Other,
}

impl Place {
    fn text(self) -> &'static str {
        match self {
            Place::Game => "ゲームフォルダ",
            Place::System => "システムフォルダ",
            Place::Other => "その他のフォルダ",
        }
    }
}

/// ファイルのバージョン情報の4つのフィールド。取れなかったフィールドは`None`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionFields {
    pub description: Option<String>,
    pub product: Option<String>,
    pub version: Option<String>,
    pub company: Option<String>,
}

/// 読み込まれているd3d8.dllの材料。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct D3d8Module {
    /// モジュールのパス。取れなければ`None`。
    pub path: Option<String>,
    /// ゲーム（exe）のフォルダ。
    pub game_dir: Option<String>,
    /// システムのフォルダ（32ビットのプロセスから見えるもの。複数あってよい）。
    pub system_dirs: Vec<String>,
    /// ファイルの大きさ（バイト）。
    pub size: Option<u64>,
    pub version: VersionFields,
}

/// パスの比べ方をそろえる。
/// `/`を`\`に変え、`\\?\`の接頭と末尾の`\`を外し、小文字にする（Windowsのパスは大文字小文字を区別しない）。
fn normalize(p: &str) -> String {
    let p = p.replace('/', "\\");
    let p = p.strip_prefix("\\\\?\\").unwrap_or(&p);
    p.trim_end_matches('\\').to_lowercase()
}

/// パスの最後の部分（ファイル名）。
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// パスのフォルダの部分（最後の区切りより前）。区切りが無ければ`None`。
fn parent(path: &str) -> Option<&str> {
    path.rfind(['\\', '/']).map(|i| &path[..i])
}

/// `path`の置かれているフォルダの種類。ゲームのフォルダとシステムのフォルダはフォルダが完全に一致するものだけ（その下は`Other`）。
pub fn place_of(path: &str, game_dir: Option<&str>, system_dirs: &[String]) -> Place {
    let Some(dir) = parent(path).map(normalize) else {
        return Place::Other;
    };
    if game_dir.is_some_and(|g| !g.is_empty() && normalize(g) == dir) {
        return Place::Game;
    }
    if system_dirs
        .iter()
        .any(|s| !s.is_empty() && normalize(s) == dir)
    {
        return Place::System;
    }
    Place::Other
}

/// フィールドを1行に収める。制御文字は空白にして前後の空白を落とす。長ければ切って「…」を付ける。空なら`無し`。
fn field(v: Option<&str>) -> String {
    let cleaned: String = v
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return NONE_TEXT.to_string();
    }
    if cleaned.chars().count() > FIELD_MAX_CHARS {
        let cut: String = cleaned.chars().take(FIELD_MAX_CHARS).collect();
        return format!("{cut}…");
    }
    cleaned.to_string()
}

/// 本ログの1行。`module`が`None`ならd3d8.dllが読み込まれていない。
pub fn line(module: Option<&D3d8Module>) -> String {
    let Some(m) = module else {
        return "d3d8.dll: このプロセスには読み込まれていません".to_string();
    };
    let (place, name) = match m.path.as_deref() {
        Some(p) => (
            place_of(p, m.game_dir.as_deref(), &m.system_dirs).text(),
            field(Some(file_name(p))),
        ),
        None => ("場所の取得不能", NONE_TEXT.to_string()),
    };
    let size = m
        .size
        .map_or(NONE_TEXT.to_string(), |s| format!("{s} バイト"));
    format!(
        "d3d8.dllの場所: {place}（ファイル名: {name} / 大きさ: {size} / FileDescription: {} / ProductName: {} / FileVersion: {} / CompanyName: {}）",
        field(m.version.description.as_deref()),
        field(m.version.product.as_deref()),
        field(m.version.version.as_deref()),
        field(m.version.company.as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> Vec<String> {
        vec![
            "C:\\Windows\\SysWOW64".to_string(),
            "C:\\Windows\\System32".to_string(),
        ]
    }

    fn full(path: &str) -> D3d8Module {
        D3d8Module {
            path: Some(path.to_string()),
            game_dir: Some("D:\\Games\\th09".to_string()),
            system_dirs: sys(),
            size: Some(1_234_567),
            version: VersionFields {
                description: Some("Direct3D 8 Runtime".to_string()),
                product: Some("Example OS".to_string()),
                version: Some("6.1.7601.17514".to_string()),
                company: Some("Example Corp.".to_string()),
            },
        }
    }

    #[test]
    fn place_distinguishes_game_system_and_other() {
        let g = Some("D:\\Games\\th09");
        assert_eq!(
            place_of("D:\\Games\\th09\\d3d8.dll", g, &sys()),
            Place::Game
        );
        assert_eq!(
            place_of("C:\\Windows\\SysWOW64\\d3d8.dll", g, &sys()),
            Place::System
        );
        assert_eq!(
            place_of("E:\\tools\\d3d8.dll", g, &sys()),
            Place::Other,
            "どちらでもないフォルダ"
        );
        assert_eq!(
            place_of("D:\\Games\\th09\\sub\\d3d8.dll", g, &sys()),
            Place::Other,
            "ゲームのフォルダの下は Game ではない"
        );
        assert_eq!(place_of("d3d8.dll", g, &sys()), Place::Other);
    }

    #[test]
    fn place_ignores_case_separators_prefix_and_trailing_slash() {
        let s = vec!["c:\\windows\\syswow64\\".to_string()];
        assert_eq!(
            place_of("C:\\WINDOWS\\SysWow64\\D3D8.DLL", None, &s),
            Place::System
        );
        assert_eq!(
            place_of("\\\\?\\C:\\Windows\\SysWOW64\\d3d8.dll", None, &s),
            Place::System
        );
        assert_eq!(
            place_of("C:/Windows/SysWOW64/d3d8.dll", None, &s),
            Place::System
        );
        assert_eq!(
            place_of("D:/Games/th09/d3d8.dll", Some("d:\\games\\TH09\\"), &s),
            Place::Game
        );
    }

    #[test]
    fn empty_directories_match_nothing() {
        let s = vec![String::new()];
        assert_eq!(
            place_of("\\d3d8.dll", Some(""), &s),
            Place::Other,
            "ルート直下のファイルが空のフォルダと一致してはいけない"
        );
    }

    #[test]
    fn system_line_has_every_field_in_order() {
        assert_eq!(
            line(Some(&full("C:\\Windows\\SysWOW64\\d3d8.dll"))),
            "d3d8.dllの場所: システムフォルダ（ファイル名: d3d8.dll / 大きさ: 1234567 バイト / FileDescription: Direct3D 8 Runtime / ProductName: Example OS / FileVersion: 6.1.7601.17514 / CompanyName: Example Corp.）"
        );
    }

    #[test]
    fn game_folder_line_says_game_folder() {
        let mut m = full("D:\\Games\\th09\\d3d8.dll");
        m.size = Some(98_304);
        m.version = VersionFields::default();
        assert_eq!(
            line(Some(&m)),
            "d3d8.dllの場所: ゲームフォルダ（ファイル名: d3d8.dll / 大きさ: 98304 バイト / FileDescription: 無し / ProductName: 無し / FileVersion: 無し / CompanyName: 無し）"
        );
    }

    #[test]
    fn line_never_contains_the_folder_names() {
        let mut m = full("D:\\Home\\alice\\Games\\th09\\d3d8.dll");
        m.game_dir = Some("D:\\Home\\alice\\Games\\th09".to_string());
        let l = line(Some(&m));
        assert!(
            !l.contains("alice") && !l.contains("Home") && !l.contains("Games"),
            "{l}"
        );
        assert!(l.contains("ゲームフォルダ"), "{l}");
        let mut other = full("D:\\Home\\alice\\Desktop\\d3d8.dll");
        other.game_dir = None;
        let l = line(Some(&other));
        assert!(!l.contains("alice") && !l.contains("Desktop"), "{l}");
        assert!(l.contains("その他のフォルダ"), "{l}");
    }

    /// 取れないフィールドが空のままだと読む側が「0件」と取り違える。
    #[test]
    fn missing_pieces_are_written_as_none() {
        let m = D3d8Module::default();
        assert_eq!(
            line(Some(&m)),
            "d3d8.dllの場所: 場所の取得不能（ファイル名: 無し / 大きさ: 無し / FileDescription: 無し / ProductName: 無し / FileVersion: 無し / CompanyName: 無し）"
        );
        let mut blank = full("C:\\Windows\\SysWOW64\\d3d8.dll");
        blank.version.description = Some("   ".to_string());
        blank.version.company = Some(String::new());
        let l = line(Some(&blank));
        assert!(l.contains("FileDescription: 無し /"), "{l}");
        assert!(l.ends_with("CompanyName: 無し）"), "{l}");
    }

    #[test]
    fn not_loaded_is_its_own_line() {
        assert_eq!(line(None), "d3d8.dll: このプロセスには読み込まれていません");
    }

    #[test]
    fn fields_stay_on_one_line_and_are_cut() {
        let mut m = full("C:\\Windows\\SysWOW64\\d3d8.dll");
        m.version.description = Some("a\r\nb\u{0}c".to_string());
        m.version.product = Some("x".repeat(200));
        let l = line(Some(&m));
        assert_eq!(l.lines().count(), 1, "{l}");
        assert!(l.contains("FileDescription: a  b c /"), "{l}");
        assert!(
            l.contains(&format!("ProductName: {}… /", "x".repeat(FIELD_MAX_CHARS))),
            "{l}"
        );
    }
}
