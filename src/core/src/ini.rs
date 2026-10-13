//! 設定ファイル `HiganRTool.ini` の読み方。節（`[名前]`）と `キー = 値` の行だけを読み、意味は読む側が決める。
//!
//! - 行ごとに前後の空白を落とす。空の行と、`;` か `#` で始まる行、先頭の BOM は読まない。
//! - `[名前]` で節を始める。最初の節より前の `キー = 値` は名前の無い節（`""`）に入る。
//! - 節の名前とキーは ASCII の大文字・小文字を区別しない（小文字にして持つ）。値の後ろの `;` は注にしない（値の一部）。
//! - 同じ節に同じキーが 2 回、`=` の無い行、閉じない `[` は誤り。
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

    pub fn has_section(&self, section: &str) -> bool {
        let s = section.to_ascii_lowercase();
        self.sections.contains(&s)
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

pub fn parse(text: &str) -> Result<Ini, IniError> {
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut ini = Ini::default();
    let mut section = String::new();
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
                .filter(|n| !n.is_empty())
                .ok_or(IniError::BadSection { line })?;
            section = name.to_ascii_lowercase();
            if !ini.sections.contains(&section) {
                ini.sections.push(section.clone());
            }
            continue;
        }
        let (k, v) = t.split_once('=').ok_or(IniError::NoEquals { line })?;
        let key = k.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(IniError::EmptyKey { line });
        }
        if let Some(e) = ini
            .entries
            .iter()
            .find(|e| e.section == section && e.key == key)
        {
            return Err(IniError::Duplicate {
                section,
                key,
                first: e.line,
                line,
            });
        }
        ini.entries.push(Entry {
            section: section.clone(),
            key,
            value: v.trim().to_string(),
            line,
        });
    }
    Ok(ini)
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
        assert_eq!(parse("k = 2 ; x\n").unwrap().get("", "k"), Some("2 ; x"));
    }
}
