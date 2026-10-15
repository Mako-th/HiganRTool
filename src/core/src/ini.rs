//! 設定ファイル `HiganRTool.ini` の読み方。節（`[名前]`）と `キー = 値` の行だけを読み、意味は読む側が決める。
//!
//! - 行ごとに前後の空白を落とす。空の行と、`;` か `#` で始まる行、先頭の BOM は読まない。
//! - `[名前]` で節を始める。最初の節より前の `キー = 値` は名前の無い節（`""`）に入る。
//! - 節の名前とキーは ASCII の大文字・小文字を区別しない（小文字にして持つ）。値の後ろの「空白 + `;`」から行末までは注（`a;b` の `;` は値の一部）。
//! - 同じ節に同じキーが 2 回、`=` の無い行、閉じない `[` は誤り。`parse` は最初の誤りで断り、`parse_lenient` は誤りの行だけ読み飛ばす。
#![forbid(unsafe_code)]

use std::fmt;

/// ローダー（`--ini`）が子の環境ブロックにだけ置き、DLL は在ればそのパスを、無ければ DLL のフォルダの ini を読む。
/// 同じ PC で 2 つのゲームを起こすとき ini を別々に持たせるため。
pub const PATH_ENV: &str = "HIGANRTOOL_INI";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ini {
    entries: Vec<Entry>,
    sections: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    section: String,
    key: String,
    value: String,
    line: usize,
}

/// 読めなかった理由（行の番号は 1 から）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IniError {
    /// `=` の無い行。
    NoEquals { line: usize },
    /// `[` で始まり `]` で終わらない行、か名前が空。
    BadSection { line: usize },
    /// キーが空。
    EmptyKey { line: usize },
    /// 同じキーが 2 回。
    Duplicate {
        section: String,
        key: String,
        first: usize,
        line: usize,
    },
}

impl IniError {
    /// 誤りの行の番号（重複は 2 回目の行）。
    pub fn line(&self) -> usize {
        match self {
            IniError::NoEquals { line }
            | IniError::BadSection { line }
            | IniError::EmptyKey { line }
            | IniError::Duplicate { line, .. } => *line,
        }
    }
}

impl fmt::Display for IniError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IniError::NoEquals { line } => write!(f, "{line} 行目に = が無い"),
            IniError::BadSection { line } => {
                write!(f, "{line} 行目の節の書き方が違う（[名前] の形でない）")
            }
            IniError::EmptyKey { line } => write!(f, "{line} 行目のキーが空"),
            IniError::Duplicate {
                section,
                key,
                first,
                line,
            } => write!(
                f,
                "[{section}] の {key} が {first} 行目と {line} 行目の 2 回ある"
            ),
        }
    }
}

impl Ini {
    /// 名前は大文字・小文字を区別しない。
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let (s, k) = (section.to_ascii_lowercase(), key.to_ascii_lowercase());
        self.entries
            .iter()
            .find(|e| e.section == s && e.key == k)
            .map(|e| e.value.as_str())
    }

    /// キーを書いた行の番号。
    pub fn line_of(&self, section: &str, key: &str) -> Option<usize> {
        let (s, k) = (section.to_ascii_lowercase(), key.to_ascii_lowercase());
        self.entries
            .iter()
            .find(|e| e.section == s && e.key == k)
            .map(|e| e.line)
    }

    pub fn has_section(&self, section: &str) -> bool {
        let s = section.to_ascii_lowercase();
        self.sections.contains(&s)
    }

    /// 見出しのある節の名前（小文字。初めて現れた順）。見出しより前の名前の無い節（`""`）は含まない。
    pub fn sections(&self) -> &[String] {
        &self.sections
    }

    /// キーと、それを書いた行の番号（書いた順）。知らないキーを読む側が断るのに使う。
    pub fn keys(&self, section: &str) -> Vec<(&str, usize)> {
        let s = section.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|e| e.section == s)
            .map(|e| (e.key.as_str(), e.line))
            .collect()
    }
}

/// 値の後ろの注（空白 + `;` から行末まで）を落とす。
pub fn strip_comment(v: &str) -> &str {
    let mut prev_space = true;
    for (i, c) in v.char_indices() {
        if c == ';' && prev_space {
            return v[..i].trim_end();
        }
        prev_space = c.is_whitespace();
    }
    v
}

/// 読み飛ばした行と、その時いた節。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub error: IniError,
    pub section: String,
}

/// `parse_lenient` の結果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lenient {
    pub ini: Ini,
    pub skipped: Vec<Skipped>,
    /// 読まなかった行の番号（昇順）。`skipped` の行と、読めない `[` の見出しの下で次の見出しまでの行。
    pub dropped: Vec<usize>,
}

/// 誤りの行だけ読み飛ばして読む。読めない見出しの下の行は、前の節に入れず次の見出しまで捨てる。
pub fn parse_lenient(text: &str) -> Lenient {
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut out = Lenient::default();
    let mut section = String::new();
    let mut lost = false;
    for (i, raw) in body.lines().enumerate() {
        let line = i + 1;
        let t = raw.trim();
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix('[') {
            let name = rest
                .strip_suffix(']')
                .map(str::trim)
                .filter(|n| !n.is_empty());
            if let Some(n) = name {
                lost = false;
                section = n.to_ascii_lowercase();
                if !out.ini.sections.contains(&section) {
                    out.ini.sections.push(section.clone());
                }
            } else {
                lost = true;
                out.skip(&section, IniError::BadSection { line });
            }
            continue;
        }
        if lost {
            out.dropped.push(line);
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            out.skip(&section, IniError::NoEquals { line });
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        if key.is_empty() {
            out.skip(&section, IniError::EmptyKey { line });
            continue;
        }
        let first = out
            .ini
            .entries
            .iter()
            .find(|e| e.section == section && e.key == key)
            .map(|e| e.line);
        if let Some(first) = first {
            let error = IniError::Duplicate {
                section: section.clone(),
                key,
                first,
                line,
            };
            out.skip(&section, error);
            continue;
        }
        out.ini.entries.push(Entry {
            section: section.clone(),
            key,
            value: strip_comment(v.trim()).to_string(),
            line,
        });
    }
    out
}

impl Lenient {
    fn skip(&mut self, section: &str, error: IniError) {
        self.dropped.push(error.line());
        self.skipped.push(Skipped {
            error,
            section: section.to_string(),
        });
    }
}

pub fn parse(text: &str) -> Result<Ini, IniError> {
    let r = parse_lenient(text);
    match r.skipped.into_iter().next() {
        Some(s) => Err(s.error),
        None => Ok(r.ini),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sections_keys_and_values() {
        let text = "\u{FEFF}top = 1\n; 注\n# 注\n\n[NetPlay]\n  Peer = 127.0.0.1:7001  \nname= a=b \n[other]\npeer = x\n";
        let ini = parse(text).unwrap();
        assert_eq!(ini.get("", "top"), Some("1"));
        assert_eq!(ini.get("netplay", "peer"), Some("127.0.0.1:7001"));
        assert_eq!(ini.get("NETPLAY", "PEER"), Some("127.0.0.1:7001"));
        assert_eq!(ini.get("netplay", "name"), Some("a=b"));
        assert_eq!(ini.get("other", "peer"), Some("x"));
        assert_eq!(ini.get("other", "name"), None);
        assert!(ini.has_section("netplay") && ini.has_section("Other"));
        assert!(!ini.has_section("missing"));
        assert_eq!(ini.keys("netplay"), vec![("peer", 6), ("name", 7)]);
    }

    #[test]
    fn refuses_bad_lines_with_line_numbers() {
        assert_eq!(
            parse("[a]\nfoo\n").unwrap_err(),
            IniError::NoEquals { line: 2 }
        );
        assert_eq!(
            parse("\n[a\n").unwrap_err(),
            IniError::BadSection { line: 2 }
        );
        assert_eq!(
            parse("[ ]\n").unwrap_err(),
            IniError::BadSection { line: 1 }
        );
        assert_eq!(parse(" = 1\n").unwrap_err(), IniError::EmptyKey { line: 1 });
        assert_eq!(
            parse("[a]\nk=1\n[b]\nk=2\n[A]\nK=3\n").unwrap_err(),
            IniError::Duplicate {
                section: "a".into(),
                key: "k".into(),
                first: 2,
                line: 6
            }
        );
    }

    #[test]
    fn a_comment_after_a_value_needs_a_space_before_the_semicolon() {
        let ini = parse("k = 2 ; x\nj = a;b\nm = ; none\nn = 1\t; tab\nname = a;b ; c\n").unwrap();
        assert_eq!(ini.get("", "k"), Some("2"));
        assert_eq!(ini.get("", "j"), Some("a;b"));
        assert_eq!(ini.get("", "m"), Some(""));
        assert_eq!(ini.get("", "n"), Some("1"));
        assert_eq!(ini.get("", "name"), Some("a;b"));
    }

    #[test]
    fn lenient_parse_skips_only_the_bad_lines() {
        let text = "[a]\nk = 1\nfoo\n = 2\nk = 3\nj = 4\n[b\nx = 5\n[c]\ny = 6\n";
        let r = parse_lenient(text);
        assert_eq!(
            r.skipped.iter().map(|s| s.error.line()).collect::<Vec<_>>(),
            vec![3, 4, 5, 7]
        );
        assert_eq!(r.skipped[0].section, "a");
        assert_eq!(r.dropped, vec![3, 4, 5, 7, 8]);
        assert_eq!(r.ini.get("a", "k"), Some("1"), "重複は最初の行を採る");
        assert_eq!(r.ini.get("a", "j"), Some("4"));
        assert_eq!(
            r.ini.get("a", "x"),
            None,
            "読めない見出しの下は前の節に入れない"
        );
        assert_eq!(r.ini.get("b", "x"), None);
        assert_eq!(r.ini.get("c", "y"), Some("6"));
        assert_eq!(r.ini.line_of("c", "Y"), Some(10));
        assert_eq!(parse(text).unwrap_err(), IniError::NoEquals { line: 3 });
    }
}
