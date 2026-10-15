//! 読み込まれている `d3d8.dll` を本ログの 1 行にする（純粋な文字の組み立て）。
//!
//! 落ちたときに「どの d3d8.dll の上か」（OS のものか、ゲームのフォルダの差し替えか）を、VEH の `d3d8.dll+RVA` と突き合わせて読むため。
//! フォルダ名にはユーザー名が入りうるので、パスは書かず「場所の種類」だけにする。

#![forbid(unsafe_code)]

/// 欄が取れなかったときに書く語。
pub const NONE_TEXT: &str = "無し";

/// 1 つの欄の最大の文字数（版の文字列に長い説明が入っていても行が伸びないように）。
const FIELD_MAX_CHARS: usize = 64;

/// 場所の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// ゲーム（exe）のフォルダ。差し替えの d3d8.dll はここに置かれる。
    Game,
    /// システムのフォルダ（`GetSystemDirectoryW` など）。
    System,
    /// どちらでもない（ゲームのフォルダの下の別のフォルダも含む）。
    Other,
}

impl Place {
    fn text(self) -> &'static str {
        match self {
            Place::Game => "ゲームのフォルダにある",
            Place::System => "システムのフォルダにある",
            Place::Other => "その他のフォルダにある",
        }
    }
}

/// ファイルのバージョン情報の 4 欄。取れなかった欄は `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionFields {
    pub description: Option<String>,
    pub product: Option<String>,
    pub version: Option<String>,
    pub company: Option<String>,
}

/// 読み込まれている d3d8.dll の材料。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct D3d8Module {
    /// モジュールのパス。取れなければ `None`。
    pub path: Option<String>,
    /// ゲーム（exe）のフォルダ。
    pub game_dir: Option<String>,
    /// システムのフォルダ（32 ビットのプロセスから見えるもの。複数あってよい）。
    pub system_dirs: Vec<String>,
    /// ファイルの大きさ（バイト）。
    pub size: Option<u64>,
    pub version: VersionFields,
}

/// パスの比べ方をそろえる: `/` を `\` に、`\\?\` の接頭を外し、末尾の `\` を外し、小文字にする（Windows のパスは大文字小文字を区別しない）。
fn normalize(p: &str) -> String {
    let p = p.replace('/', "\\");
    let p = p.strip_prefix("\\\\?\\").unwrap_or(&p);
    p.trim_end_matches('\\').to_lowercase()
}

/// パスの最後の部分（ファイル名）。
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// パスのフォルダの部分（最後の区切りより前）。区切りが無ければ `None`。
fn parent(path: &str) -> Option<&str> {
    path.rfind(['\\', '/']).map(|i| &path[..i])
}

/// `path` の置かれているフォルダの種類。ゲームのフォルダとシステムのフォルダは、フォルダが完全に一致するものだけ（その下は `Other`）。
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

/// 欄を 1 行に収める: 制御文字は空白にし、前後の空白を落とし、長ければ切って「…」を付ける。空なら `無し`。
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

/// 本ログの 1 行。`module` が `None` なら d3d8.dll が読み込まれていない。
pub fn line(module: Option<&D3d8Module>) -> String {
    let Some(m) = module else {
        return "D3D8: d3d8.dll はこのプロセスに読み込まれていない".to_string();
    };
    let (place, name) = match m.path.as_deref() {
        Some(p) => (
            place_of(p, m.game_dir.as_deref(), &m.system_dirs).text(),
            field(Some(file_name(p))),
        ),
        None => ("場所が取れなかった", NONE_TEXT.to_string()),
    };
    let size = m
        .size
        .map_or(NONE_TEXT.to_string(), |s| format!("{s} バイト"));
    format!(
        "D3D8: 読み込まれている d3d8.dll は{place}（ファイル名: {name} / 大きさ: {size} / FileDescription: {} / ProductName: {} / FileVersion: {} / CompanyName: {}）",
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

    /// 壊れたら落ちる: 場所の種類の取り違え（差し替えをシステムのものと書く、または逆）。
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

    /// 壊れたら落ちる: 大文字小文字・区切り・`\\?\` の接頭・末尾の `\` の違いで場所が外れる。
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

    /// 壊れたら落ちる: 空のフォルダ名が「どのパスにも一致する」扱いになる。
    #[test]
    fn empty_directories_match_nothing() {
        let s = vec![String::new()];
        assert_eq!(
            place_of("\\d3d8.dll", Some(""), &s),
            Place::Other,
            "ルート直下のファイルが空のフォルダと一致してはいけない"
        );
    }

    /// 壊れたら落ちる: 欄の並びと語（FileDescription / ProductName / FileVersion / CompanyName）の入れ替わり。
    #[test]
    fn system_line_has_every_field_in_order() {
        assert_eq!(
            line(Some(&full("C:\\Windows\\SysWOW64\\d3d8.dll"))),
            "D3D8: 読み込まれている d3d8.dll はシステムのフォルダにある（ファイル名: d3d8.dll / 大きさ: 1234567 バイト / FileDescription: Direct3D 8 Runtime / ProductName: Example OS / FileVersion: 6.1.7601.17514 / CompanyName: Example Corp.）"
        );
    }

    #[test]
    fn game_folder_line_says_game_folder() {
        let mut m = full("D:\\Games\\th09\\d3d8.dll");
        m.size = Some(98_304);
        m.version = VersionFields::default();
        assert_eq!(
            line(Some(&m)),
            "D3D8: 読み込まれている d3d8.dll はゲームのフォルダにある（ファイル名: d3d8.dll / 大きさ: 98304 バイト / FileDescription: 無し / ProductName: 無し / FileVersion: 無し / CompanyName: 無し）"
        );
    }

    /// 壊れたら落ちる: ユーザー名を含むフォルダ名がそのまま行に出る。
    #[test]
    fn line_never_contains_the_folder_names() {
        let mut m = full("D:\\Home\\alice\\Games\\th09\\d3d8.dll");
        m.game_dir = Some("D:\\Home\\alice\\Games\\th09".to_string());
        let l = line(Some(&m));
        assert!(
            !l.contains("alice") && !l.contains("Home") && !l.contains("Games"),
            "{l}"
        );
        assert!(l.contains("ゲームのフォルダ"), "{l}");
        let mut other = full("D:\\Home\\alice\\Desktop\\d3d8.dll");
        other.game_dir = None;
        let l = line(Some(&other));
        assert!(!l.contains("alice") && !l.contains("Desktop"), "{l}");
        assert!(l.contains("その他のフォルダ"), "{l}");
    }

    /// 壊れたら落ちる: 取れない欄が空のまま・欠けたまま行に出る（読む側が「0 件」と取り違える）。
    #[test]
    fn missing_pieces_are_written_as_none() {
        let m = D3d8Module::default();
        assert_eq!(
            line(Some(&m)),
            "D3D8: 読み込まれている d3d8.dll は場所が取れなかった（ファイル名: 無し / 大きさ: 無し / FileDescription: 無し / ProductName: 無し / FileVersion: 無し / CompanyName: 無し）"
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
        assert_eq!(
            line(None),
            "D3D8: d3d8.dll はこのプロセスに読み込まれていない"
        );
    }

    /// 壊れたら落ちる: 版の文字列の改行・制御文字・長い文字列で行が崩れる（ログの 1 行が複数行になる）。
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
