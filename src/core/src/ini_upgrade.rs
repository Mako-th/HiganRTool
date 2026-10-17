//! 配る ini の雛形を土台に、今の ini の値を移して新しい形の ini を作る（pure。ファイルは触らない）。
//!
//! 更新で増えたキーや説明を、利用者が変えた値を残したまま古い ini に入れるためのもの。
//! - 今の ini で行として書かれているキー（節とキーは `ini` と同じ扱い。大文字・小文字を区別しない）は、雛形の同じキーの行に
//!   今の値で置く。雛形で注になっている行（`; key = v`）は、注を外してから置く。
//! - 雛形に無いキーは、その節の最後のキーの行の後ろへ残す（節も無ければ末尾に節ごと）。値は消さない。
//! - 雛形の `; [window]` のように見出しごと注の節は、今の ini にその節のキーがあれば見出しの注を外す。
//! - 注は雛形のものになる（今の ini にあった注は残らない）。改行は雛形に合わせる。値の中身は見ない。
//! - 値が、知っている古い配る ini（`OLD_TEMPLATES`）の同じ節・キーの値と同じなら、利用者が変えていない既定値とみなして移さず、
//!   新しい雛形の値にする（新しい版で既定値を変えても、既存の利用者が古い既定値のままにならないように）。値は `ini` の読み手が返す形
//!   （前後の空白と行末の注を除いたもの。大文字・小文字は区別する）で比べる。古い雛形で注だった行は比べない。
//! - 読めない行（`ini::parse_lenient` が捨てる行）があるときは、捨てずに済ませられないので `Upgraded::is_safe` を偽にする。
//!   作った結果を読み直して値が合わないキーがあるときも同じ。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashSet};

use crate::ini::{self, Ini, strip_comment};

/// これまでに配った版の配る ini（値の比べ先）。配る版を上げるときは、直前に配った版の `scripts/dist/HiganRTool.ini` を
/// `ini_templates/` へ足して、ここへ 1 行足す（足し忘れは `every_old_template_file_is_listed` が落とす）。
pub const OLD_TEMPLATES: [&str; 2] = [
    include_str!("ini_templates/v1.0.0.ini"),
    include_str!("ini_templates/v1.1.0.ini"),
];

/// 節とキー（どちらも小文字）。値は持たない（名前・ゲームのパスを文に出さないため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRef {
    pub section: String,
    pub key: String,
}

impl KeyRef {
    /// `[section] key`（名前の無い節は `key` だけ）。
    pub fn shown(&self) -> String {
        if self.section.is_empty() {
            self.key.clone()
        } else {
            format!("[{}] {}", self.section, self.key)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgraded {
    /// 新しい ini の全文（BOM なし）。
    pub text: String,
    /// 雛形のキーの行に値を移したもの（今の ini に書かれていた順）。
    pub kept: Vec<KeyRef>,
    /// 雛形に無く、節の末尾などへ残したもの。
    pub carried: Vec<KeyRef>,
    /// 値が古い配る ini の既定値のままだったので、移さず新しい雛形の値にしたもの。
    pub defaulted: Vec<KeyRef>,
    /// 今の ini の読めない行の番号（1 から）。
    pub unreadable: Vec<usize>,
    /// 作った結果を読み直すと値が合わないキー。
    pub lost: Vec<KeyRef>,
}

impl Upgraded {
    /// 今の ini を `text` に置き換えてよいか。
    pub fn is_safe(&self) -> bool {
        self.unreadable.is_empty() && self.lost.is_empty()
    }
}

enum Kind {
    /// 読める見出し。
    Header,
    /// 注になっている見出し（`; [window]`）。雛形に同じ名前の見出しが無いものだけ。
    CommentedHeader,
    Key(String),
    /// 注になっているキーの行（`; key = v`）。
    CommentedKey(String),
    Other,
}

struct Line {
    text: String,
    kind: Kind,
    /// この行が属する節（見出しの行はその節）。注の見出しの下は、その名前の節に数える。
    section: String,
}

fn header_name(t: &str) -> Option<String> {
    t.strip_prefix('[')?
        .strip_suffix(']')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_ascii_lowercase)
}

fn is_key_name(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn classify(template: &str) -> Vec<Line> {
    let texts: Vec<&str> = template.lines().collect();
    let real: HashSet<String> = texts.iter().filter_map(|l| header_name(l.trim())).collect();
    let mut section = String::new();
    let mut out = Vec::new();
    for text in texts {
        let t = text.trim();
        let kind = if let Some(name) = header_name(t) {
            section = name;
            Kind::Header
        } else if let Some(name) = t
            .strip_prefix(';')
            .and_then(|r| header_name(r.trim()))
            .filter(|n| !real.contains(n))
        {
            section = name;
            Kind::CommentedHeader
        } else if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
            match t
                .strip_prefix(';')
                .and_then(|r| r.trim_start().split_once('='))
                .map(|(k, _)| k.trim())
                .filter(|k| is_key_name(k))
            {
                Some(k) => Kind::CommentedKey(k.to_ascii_lowercase()),
                None => Kind::Other,
            }
        } else {
            match t.split_once('=') {
                Some((k, _)) if !k.trim().is_empty() => Kind::Key(k.trim().to_ascii_lowercase()),
                _ => Kind::Other,
            }
        };
        out.push(Line {
            text: text.to_string(),
            kind,
            section: section.clone(),
        });
    }
    out
}

/// `key = 値 ; 注` の値を `value` にする。キーの書き方と後ろの注は残す。
fn with_value(line: &str, value: &str) -> String {
    let Some(eq) = line.find('=') else {
        return line.to_string();
    };
    let rest = line[eq + 1..].trim();
    let now = strip_comment(rest);
    if now == value {
        return line.to_string();
    }
    let comment = rest[now.len()..].trim();
    let mut out = line[..=eq].to_string();
    for part in [value, comment] {
        if !part.is_empty() {
            out.push(' ');
            out.push_str(part);
        }
    }
    out
}

fn key_line(key: &str, value: &str) -> String {
    with_value(&format!("{key} ="), value)
}

fn eol_of(template: &str) -> &'static str {
    match template.find('\n') {
        Some(i) if i > 0 && template.as_bytes()[i - 1] == b'\r' => "\r\n",
        Some(_) => "\n",
        None => "\r\n",
    }
}

fn find_line(lines: &[Line], section: &str, key: &str, commented: bool) -> Option<usize> {
    lines.iter().position(|l| {
        l.section == section
            && match &l.kind {
                Kind::Key(k) => !commented && k == key,
                Kind::CommentedKey(k) => commented && k == key,
                _ => false,
            }
    })
}

/// 節の最後のキーの行（なければ見出しの行）。名前の無い節は、最初の見出しより前の最後の空でない行。
fn anchor(lines: &[Line], section: &str) -> Option<usize> {
    lines.iter().rposition(|l| {
        l.section == section
            && if section.is_empty() {
                !l.text.trim().is_empty()
            } else {
                !matches!(l.kind, Kind::Other | Kind::CommentedHeader)
            }
    })
}

/// (節, キー, 値) を書かれた順に。名前の無い節が先頭。
fn entries_of(ini: &Ini) -> Vec<(String, String, String)> {
    let mut entries = Vec::new();
    for section in std::iter::once("").chain(ini.sections().iter().map(String::as_str)) {
        for (key, _) in ini.keys(section) {
            let value = ini.get(section, key).unwrap_or_default();
            entries.push((section.to_string(), key.to_string(), value.to_string()));
        }
    }
    entries
}

/// 雛形の行 `key = 値 ; 注` の値（`ini` の読み手と同じ形）。
fn line_value(line: &str) -> &str {
    line.split_once('=')
        .map_or("", |(_, rest)| strip_comment(rest.trim()))
}

/// `template` の形に `user`（今の ini の全文）の値を移す。古い雛形との比べはしない。
pub fn upgrade(template: &str, user: &str) -> Upgraded {
    upgrade_over(template, user, &[])
}

/// `upgrade` に、これまでに配った版の配る ini（`OLD_TEMPLATES`）との比べを足したもの。更新の本番はこちら。
pub fn upgrade_known(template: &str, user: &str) -> Upgraded {
    upgrade_over(template, user, &OLD_TEMPLATES)
}

/// `old_templates` のどれかの同じ節・キーの値と同じ値は、利用者が変えていない既定値として移さない。
/// ただし新しい雛形の行の値も同じなら、移しても同じなので普通に移す（`kept`）。
pub fn upgrade_over(template: &str, user: &str, old_templates: &[&str]) -> Upgraded {
    let lenient = ini::parse_lenient(user);
    let old_defaults: HashSet<(String, String, String)> = old_templates
        .iter()
        .flat_map(|t| entries_of(&ini::parse_lenient(t).ini))
        .collect();

    let mut lines = classify(template);
    let mut defaulted = Vec::new();
    let mut entries = Vec::new();
    for entry in entries_of(&lenient.ini) {
        let same_in_new = find_line(&lines, &entry.0, &entry.1, false)
            .is_some_and(|i| line_value(&lines[i].text) == entry.2);
        if old_defaults.contains(&entry) && !same_in_new {
            defaulted.push(KeyRef {
                section: entry.0,
                key: entry.1,
            });
        } else {
            entries.push(entry);
        }
    }

    // 注の見出しの節に今の ini のキーがあれば、見出しの注を外す。
    for line in &mut lines {
        if matches!(line.kind, Kind::CommentedHeader) && entries.iter().any(|e| e.0 == line.section)
        {
            line.text = line.text.trim().trim_start_matches(';').trim().to_string();
            line.kind = Kind::Header;
        }
    }

    let (mut kept, mut carried) = (Vec::new(), Vec::new());
    let mut after: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut top: Vec<String> = Vec::new();
    let mut new_sections: Vec<(String, Vec<String>)> = Vec::new();
    for (section, key, value) in &entries {
        let who = KeyRef {
            section: section.clone(),
            key: key.clone(),
        };
        if let Some(i) = find_line(&lines, section, key, false) {
            lines[i].text = with_value(&lines[i].text, value);
            kept.push(who);
        } else if let Some(i) = find_line(&lines, section, key, true) {
            let uncommented = lines[i]
                .text
                .trim_start()
                .trim_start_matches(';')
                .trim_start();
            lines[i].text = with_value(uncommented, value);
            lines[i].kind = Kind::Key(key.clone());
            kept.push(who);
        } else {
            let text = key_line(key, value);
            match anchor(&lines, section) {
                Some(i) => after.entry(i).or_default().push(text),
                None if section.is_empty() => top.push(text),
                None => match new_sections.iter_mut().find(|(s, _)| s == section) {
                    Some((_, v)) => v.push(text),
                    None => new_sections.push((section.clone(), vec![text])),
                },
            }
            carried.push(who);
        }
    }

    let mut out: Vec<String> = Vec::new();
    if !top.is_empty() {
        out.append(&mut top);
        out.push(String::new());
    }
    for (i, line) in lines.iter().enumerate() {
        out.push(line.text.clone());
        if let Some(extra) = after.get(&i) {
            out.extend(extra.iter().cloned());
        }
    }
    let appended = !new_sections.is_empty();
    for (name, texts) in new_sections {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(format!("[{name}]"));
        out.extend(texts);
    }
    let eol = eol_of(template);
    let mut text = out.join(eol);
    if !out.is_empty() && (appended || template.ends_with('\n')) {
        text.push_str(eol);
    }

    let back = ini::parse_lenient(&text).ini;
    let lost = entries
        .iter()
        .filter(|(s, k, v)| back.get(s, k) != Some(v.as_str()))
        .map(|(s, k, _)| KeyRef {
            section: s.clone(),
            key: k.clone(),
        })
        .collect();
    Upgraded {
        text,
        kept,
        carried,
        defaulted,
        unreadable: lenient.dropped,
        lost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "\
; 説明
; 2 行目

[netplay]
; --- 通信 ---
enable = 1
upnp = on
; room =
; mode = rollback
delay = auto

[pacing]
mode = auto

; ウィンドウ。使うときは [window] と使う行の ; を外す
; [window]
; 大きさ
; width = 1280
; height = 960

[ui]
lang = auto
";

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    fn who(section: &str, key: &str) -> KeyRef {
        KeyRef {
            section: section.into(),
            key: key.into(),
        }
    }

    // 壊れたら落ちる: 利用者が変えた値が新しい ini に残らない（更新のたびに設定が消える）。
    #[test]
    fn the_users_values_replace_the_template_values_in_place() {
        let user = "[netplay]\nenable = 0\ndelay = 4\n[ui]\nlang = en\n";
        let r = upgrade(T, user);
        assert!(r.is_safe(), "{r:?}");
        let got = ini::parse(&r.text).unwrap();
        assert_eq!(got.get("netplay", "enable"), Some("0"));
        assert_eq!(got.get("netplay", "delay"), Some("4"));
        assert_eq!(got.get("ui", "lang"), Some("en"));
        // 触っていないキーは雛形のまま、順序も説明も雛形のまま。
        assert_eq!(got.get("netplay", "upnp"), Some("on"));
        assert_eq!(got.get("pacing", "mode"), Some("auto"));
        assert_eq!(
            lines(&r.text),
            lines(
                &T.replace("enable = 1", "enable = 0")
                    .replace("delay = auto", "delay = 4")
                    .replace("lang = auto", "lang = en")
            )
        );
        assert_eq!(
            r.kept,
            vec![
                who("netplay", "enable"),
                who("netplay", "delay"),
                who("ui", "lang")
            ]
        );
        assert!(r.carried.is_empty());
    }

    // 壊れたら落ちる: 雛形が注にしている行に値を書いていた利用者の値が、注のまま（効かない）になる。
    #[test]
    fn a_commented_template_line_becomes_a_real_line_with_the_users_value() {
        let r = upgrade(T, "[netplay]\nroom = my-room\nmode = delay\n");
        assert!(r.is_safe(), "{r:?}");
        assert!(r.text.contains("\nroom = my-room\n"), "{}", r.text);
        assert!(r.text.contains("\nmode = delay\n"), "{}", r.text);
        assert!(!r.text.contains("; room ="), "{}", r.text);
        assert!(!r.text.contains("; mode = rollback"), "{}", r.text);
        // [pacing] の mode とは別。
        assert!(r.text.contains("[pacing]\nmode = auto\n"), "{}", r.text);
        assert_eq!(r.kept, vec![who("netplay", "room"), who("netplay", "mode")]);
    }

    // 壊れたら落ちる: 雛形に無い（古い・利用者が足した）キーが消える、または別の節に入る。
    #[test]
    fn keys_the_template_does_not_have_are_kept_at_the_end_of_their_section() {
        let user =
            "[netplay]\nold_key = x\nenable = 1\n[mine]\nfoo = bar\nbaz = 1\n[ui]\nextra = 2\n";
        let r = upgrade(T, user);
        assert!(r.is_safe(), "{r:?}");
        let got = ini::parse(&r.text).unwrap();
        assert_eq!(got.get("netplay", "old_key"), Some("x"));
        assert_eq!(got.get("ui", "extra"), Some("2"));
        assert_eq!(got.get("mine", "foo"), Some("bar"));
        assert_eq!(got.get("mine", "baz"), Some("1"));
        // 節の最後のキーの行（注の行を除く）の後ろ。
        assert!(r.text.contains("delay = auto\nold_key = x\n"), "{}", r.text);
        assert!(r.text.contains("lang = auto\nextra = 2\n"), "{}", r.text);
        // 雛形に無い節は末尾に節ごと。
        assert!(
            r.text.ends_with("\n\n[mine]\nfoo = bar\nbaz = 1\n"),
            "{}",
            r.text
        );
        assert_eq!(
            r.carried,
            vec![
                who("netplay", "old_key"),
                who("mine", "foo"),
                who("mine", "baz"),
                who("ui", "extra")
            ]
        );
    }

    // 壊れたら落ちる: 見出しごと注の節に書いていた値（ウィンドウの大きさ）が効かないまま残る。
    #[test]
    fn a_section_commented_out_in_the_template_is_opened_when_the_user_has_it() {
        let r = upgrade(T, "[window]\nwidth = 1500\n");
        assert!(r.is_safe(), "{r:?}");
        assert!(
            r.text
                .contains("\n[window]\n; 大きさ\nwidth = 1500\n; height = 960\n"),
            "{}",
            r.text
        );
        assert!(!r.text.contains("; [window]"), "{}", r.text);
        assert_eq!(
            ini::parse(&r.text).unwrap().get("window", "width"),
            Some("1500")
        );
        assert_eq!(r.kept, vec![who("window", "width")]);

        // 利用者が持っていなければ注のまま。
        let r = upgrade(T, "[ui]\nlang = ja\n");
        assert!(r.text.contains("\n; [window]\n"), "{}", r.text);
        assert!(!ini::parse(&r.text).unwrap().has_section("window"));

        // 雛形に無いキーだけでも節を開き、その節の最後の行の後ろへ置く。
        let r = upgrade(T, "[window]\nfoo = 1\n");
        assert!(r.is_safe(), "{r:?}");
        assert!(
            r.text
                .contains("\n[window]\n; 大きさ\n; width = 1280\n; height = 960\nfoo = 1\n"),
            "{}",
            r.text
        );
        assert_eq!(r.carried, vec![who("window", "foo")]);
        // ほかの節のキーが、注の節の下へ入らない。
        let r = upgrade(T, "[window]\nwidth = 1\n[ui]\nextra = 2\n");
        let got = ini::parse(&r.text).unwrap();
        assert_eq!(got.get("ui", "extra"), Some("2"));
        assert_eq!(got.get("window", "extra"), None);
    }

    // 壊れたら落ちる: 雛形と今の ini が同じなのに書き換わる（更新のたびに ini が揺れる）。
    #[test]
    fn upgrading_a_file_made_from_the_template_gives_the_template_back() {
        let r = upgrade(T, T);
        assert_eq!(r.text, T);
        assert!(r.carried.is_empty() && r.is_safe(), "{r:?}");
        assert_eq!(r.kept.len(), 5);
        let r = upgrade(&T.replace('\n', "\r\n"), &T.replace('\n', "\r\n"));
        assert_eq!(r.text, T.replace('\n', "\r\n"));
    }

    // 壊れたら落ちる: ini が空でも新しい形が出ない。
    #[test]
    fn an_empty_or_comment_only_ini_becomes_the_template() {
        for user in ["", "\n\n", "; 注だけ\n; もう 1 行\n", "\u{FEFF}"] {
            let r = upgrade(T, user);
            assert_eq!(r.text, T, "{user:?}");
            assert!(r.kept.is_empty() && r.carried.is_empty() && r.is_safe());
        }
    }

    // 壊れたら落ちる: 改行が雛形とそろわない（古いメモ帳で 1 行に潰れる）。
    #[test]
    fn line_endings_follow_the_template() {
        let crlf = T.replace('\n', "\r\n");
        let user = "[netplay]\nenable = 0\nold = 1\n[mine]\nx = 1\n";
        for (tpl, eol) in [(T.to_string(), "\n"), (crlf, "\r\n")] {
            let r = upgrade(&tpl, user);
            let bare_lf = r.text.matches('\n').count();
            assert_eq!(r.text.matches(eol).count(), bare_lf, "{eol:?}");
            assert!(r.text.ends_with(eol));
        }
        // 利用者の ini が CRLF でも LF でも、結果は雛形の改行。
        let r = upgrade(T, "[netplay]\r\nenable = 0\r\n");
        assert!(!r.text.contains('\r'), "{:?}", r.text);
    }

    // 壊れたら落ちる: 名前・節の大文字小文字の違いで値が移らない。
    #[test]
    fn names_are_matched_without_case_and_values_keep_theirs() {
        let r = upgrade(T, "[NetPlay]\nENABLE = 0\nName = Aa Bb\n[UI]\nLang = En\n");
        assert!(r.is_safe(), "{r:?}");
        let got = ini::parse(&r.text).unwrap();
        assert_eq!(got.get("netplay", "enable"), Some("0"));
        assert_eq!(got.get("ui", "lang"), Some("En"));
        assert_eq!(got.get("netplay", "name"), Some("Aa Bb"));
        assert!(r.text.contains("\nenable = 0\n"), "{}", r.text);
    }

    // 壊れたら落ちる: 値の中身（空・`;`・`=`・全角）が、移すときに変わる。
    #[test]
    fn odd_values_survive() {
        let user = "[netplay]\nenable =\ndelay = a;b=c\nupnp = 花映塚 ; 注\n[mine]\ng = C:\\x y\\th09.exe\n";
        let r = upgrade(T, user);
        assert!(r.is_safe(), "{r:?}");
        let got = ini::parse(&r.text).unwrap();
        assert_eq!(got.get("netplay", "enable"), Some(""));
        assert_eq!(got.get("netplay", "delay"), Some("a;b=c"));
        assert_eq!(got.get("netplay", "upnp"), Some("花映塚"));
        assert_eq!(got.get("mine", "g"), Some("C:\\x y\\th09.exe"));
        // 利用者の注は残らない。
        assert!(!r.text.contains("花映塚 ;"), "{}", r.text);
    }

    // 壊れたら落ちる: 雛形の行の後ろの注が、値を変えると消える。
    #[test]
    fn a_comment_after_a_template_value_stays() {
        let tpl = "[a]\nk = 1 ; 既定\nj=2\n; z = 3 ; 説明\n";
        let r = upgrade(tpl, "[a]\nk = 9\nj = 2\nz = 4\n");
        assert_eq!(r.text, "[a]\nk = 9 ; 既定\nj=2\nz = 4 ; 説明\n");
        assert!(r.is_safe());
    }

    // 壊れたら落ちる: 読めない行のある ini を黙って書き換えて、その行を失う。
    #[test]
    fn an_ini_with_unreadable_lines_is_not_safe_to_replace() {
        let r = upgrade(T, "[netplay]\nenable = 0\nthis line has no equals\n");
        assert_eq!(r.unreadable, vec![3]);
        assert!(!r.is_safe());
        let r = upgrade(T, "[netplay]\nenable = 0\nenable = 1\n");
        assert_eq!(r.unreadable, vec![3], "同じキーが 2 回");
        assert!(!r.is_safe());
        let r = upgrade(T, "[netplay\nenable = 0\n[ui]\nlang = ja\n");
        assert_eq!(r.unreadable, vec![1, 2], "読めない見出しと、その下の行");
        assert!(!r.is_safe());
        // 値は読めた分だけ移っている（呼ぶ側が書くかを決める）。
        assert_eq!(ini::parse(&r.text).unwrap().get("ui", "lang"), Some("ja"));
    }

    // 壊れたら落ちる: 移した結果を読み直す確かめが無く、値が合わなくても安全と言う。
    #[test]
    fn a_result_that_does_not_read_back_is_not_safe() {
        // 雛形の読めない見出しの下にキーがあると、書いても読まれない。
        let broken = "[a]\n[b\nk = 1\n";
        let r = upgrade(broken, "[a]\nk = 7\n");
        assert_eq!(r.lost, vec![who("a", "k")], "{r:?}");
        assert!(!r.is_safe());
    }

    const OLD: &str = "[a]\nk = 1\nj = 2\nm = x\n; c = 5\n[b]\nz = 5\n[w]\nwidth = 1\n";
    const NEW: &str = "[a]\nk = 9\nj = 2\n; m = y\nc = 6\n[b]\nz = 6\n; [w]\n; width = 2\n";

    fn over(user: &str) -> Upgraded {
        upgrade_over(NEW, user, &[OLD])
    }

    // 壊れたら落ちる: 古い配る ini の既定値のままの値が移り、新しい版で既定値を変えても既存の利用者が古い既定値のままになる。
    #[test]
    fn a_value_equal_to_the_old_template_takes_the_new_templates_value() {
        let r = over("[a]\nk = 1\nj = 2\nm = x\nq = 7\n[b]\nz = 5\n");
        assert!(r.is_safe(), "{r:?}");
        assert_eq!(
            r.text,
            "[a]\nk = 9\nj = 2\n; m = y\nc = 6\nq = 7\n[b]\nz = 6\n; [w]\n; width = 2\n"
        );
        // k と z は新しい値。m は新しい雛形で注なので注のまま。j は新旧とも 2 なので普通に移した扱い。
        assert_eq!(
            r.defaulted,
            vec![who("a", "k"), who("a", "m"), who("b", "z")]
        );
        assert_eq!(r.kept, vec![who("a", "j")]);
        assert_eq!(r.carried, vec![who("a", "q")]);
    }

    // 壊れたら落ちる: 利用者が変えた値まで新しい既定値に戻る。古い雛形に無いキーの値を捨てる。
    #[test]
    fn a_changed_value_or_a_key_the_old_template_lacks_is_kept() {
        let r = over("[a]\nk = 3\nm = z\nnew = 1\n[b]\nz = 50\n");
        assert!(r.is_safe(), "{r:?}");
        let got = ini::parse(&r.text).unwrap();
        assert_eq!(got.get("a", "k"), Some("3"));
        assert_eq!(got.get("a", "m"), Some("z"));
        assert_eq!(got.get("b", "z"), Some("50"));
        assert_eq!(got.get("a", "new"), Some("1"));
        assert!(r.defaulted.is_empty(), "{r:?}");
        // 古い雛形に無い c は、新しい雛形の値 6 と違えば利用者の値。
        let r = over("[a]\nc = 5\n");
        assert_eq!(ini::parse(&r.text).unwrap().get("a", "c"), Some("5"));
        assert!(r.defaulted.is_empty(), "{r:?}");
    }

    // 壊れたら落ちる: 古い雛形で注だった行（c = 5）まで既定値として比べ、利用者が外して書いた値を消す。
    #[test]
    fn a_commented_line_of_the_old_template_is_not_a_default() {
        let r = over("[a]\nc = 5\n");
        assert_eq!(r.kept, vec![who("a", "c")]);
        assert!(r.defaulted.is_empty(), "{r:?}");
    }

    // 壊れたら落ちる: 空白や注の違い・キーと節の大文字小文字の違いで、変えていない既定値が既定値と見なされない。
    // 値の大文字小文字は読み手が保つので区別する（変えた値を消さない）。
    #[test]
    fn values_are_compared_in_the_form_the_ini_reader_returns() {
        let r = over("[A]\nK =   1   ; 自分の注\n");
        assert_eq!(r.defaulted, vec![who("a", "k")], "{r:?}");
        assert_eq!(ini::parse(&r.text).unwrap().get("a", "k"), Some("9"));
        let old = "[a]\nu = on\n";
        let r = upgrade_over("[a]\nu = off\n", "[a]\nu = ON\n", &[old]);
        assert_eq!(r.kept, vec![who("a", "u")], "{r:?}");
        assert_eq!(ini::parse(&r.text).unwrap().get("a", "u"), Some("ON"));
        let r = upgrade_over("[a]\nu = off\n", "[a]\nu = on\n", &[old]);
        assert_eq!(r.defaulted, vec![who("a", "u")], "{r:?}");
    }

    // 壊れたら落ちる: 既定値だけだった節の見出しが、値を移さないのに注から外れて空の節が残る。
    #[test]
    fn a_commented_section_stays_commented_when_all_its_values_are_old_defaults() {
        let r = over("[w]\nwidth = 1\n");
        assert_eq!(r.defaulted, vec![who("w", "width")]);
        assert!(r.text.contains("\n; [w]\n; width = 2\n"), "{}", r.text);
        assert!(!ini::parse(&r.text).unwrap().has_section("w"));
        let r = over("[w]\nwidth = 3\n");
        assert!(r.text.contains("\n[w]\nwidth = 3\n"), "{}", r.text);
    }

    // 壊れたら落ちる: 古い雛形を渡さない呼び出し（upgrade）まで既定値を落とす／結果を読み直す確かめから既定値が漏れる。
    #[test]
    fn upgrade_without_old_templates_moves_every_value() {
        let r = upgrade(NEW, "[a]\nk = 1\n");
        assert!(r.defaulted.is_empty(), "{r:?}");
        assert_eq!(ini::parse(&r.text).unwrap().get("a", "k"), Some("1"));
        let r = over("[a]\nk = 1\n");
        assert!(r.is_safe() && r.lost.is_empty(), "{r:?}");
    }

    // 壊れたら落ちる: これまでに配った配る ini の値のままの ini が、新しい配る ini の形・値にならない。
    #[test]
    fn every_shipped_ini_upgrades_to_the_current_shipped_ini() {
        let v100 = OLD_TEMPLATES[0];
        let dist = crate::ini_store::dist_ini_text();
        for old in OLD_TEMPLATES {
            let r = upgrade_known(&dist, old);
            assert!(r.is_safe(), "{r:?}");
            assert_eq!(r.text, dist);
            assert!(r.carried.is_empty(), "{r:?}");
            assert!(!r.kept.is_empty(), "{r:?}");
        }
        // 新しい版が既定値を変えたとして、v1.0.0 のままの値が新しい値になる。
        let changed = dist.replace("\r\nupnp = on\r\n", "\r\nupnp = off\r\n");
        assert_ne!(changed, dist);
        let r = upgrade_known(&changed, v100);
        assert_eq!(r.text, changed);
        assert_eq!(r.defaulted, vec![who("netplay", "upnp")], "{r:?}");
        // 利用者が変えた値は v1.0.0 から引き継いでも残る。
        let r = upgrade_known(&changed, &v100.replace("\nupnp = on\n", "\nupnp = yes\n"));
        assert_eq!(
            ini::parse(&r.text).unwrap().get("netplay", "upnp"),
            Some("yes")
        );
        assert!(r.defaulted.is_empty(), "{r:?}");
    }

    // 壊れたら落ちる: ini_templates に足したファイルを一覧に入れ忘れる（次の更新で古い既定値が固定される）。
    #[test]
    fn every_old_template_file_is_listed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ini_templates");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        let mut listed: Vec<String> = OLD_TEMPLATES.iter().map(|t| t.to_string()).collect();
        on_disk.sort();
        listed.sort();
        assert!(!listed.is_empty());
        assert_eq!(on_disk, listed);
        for t in OLD_TEMPLATES {
            assert!(ini::parse_lenient(t).dropped.is_empty(), "雛形に読めない行");
        }
    }

    #[test]
    fn keys_before_any_section_are_kept_before_the_first_section() {
        let r = upgrade(T, "top = 1\n[ui]\nlang = ja\n");
        assert!(r.is_safe(), "{r:?}");
        assert_eq!(ini::parse(&r.text).unwrap().get("", "top"), Some("1"));
        assert!(
            r.text.contains("; 2 行目\ntop = 1\n\n[netplay]"),
            "{}",
            r.text
        );
        assert_eq!(r.carried, vec![who("", "top")]);
        // 雛形の最初が見出しなら、見出しの前に置いて空行を挟む。
        let r = upgrade("[a]\nk = 1\n", "top = 1\n");
        assert_eq!(r.text, "top = 1\n\n[a]\nk = 1\n");
        assert!(r.is_safe());
    }

    #[test]
    fn a_template_without_a_final_newline_gets_one_only_when_something_is_appended() {
        let r = upgrade("[a]\nk = 1", "[a]\nk = 2\n");
        assert_eq!(r.text, "[a]\nk = 2");
        let r = upgrade("[a]\nk = 1", "[b]\nx = 2\n");
        assert_eq!(r.text, "[a]\nk = 1\n\n[b]\nx = 2\n");
    }

    #[test]
    fn rewrite_writes_through_a_temp_file_and_leaves_an_unchanged_ini_alone() {
        use crate::ini_store;
        let dir = std::env::temp_dir().join(format!("higanr_ini_upgrade_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("HiganRTool.ini");
        // ini が無ければ作らない。
        assert!(ini_store::rewrite(&p, |_| Some("x".into())).is_err());
        assert!(!p.exists());
        // Shift_JIS の ini は、値を読んで UTF-8 で書く。
        let sjis = encoding_rs::SHIFT_JIS
            .encode("[netplay]\nname = 花映塚\nenable = 0\n")
            .0
            .into_owned();
        std::fs::write(&p, &sjis).unwrap();
        let wrote = ini_store::rewrite(&p, |old| Some(upgrade(T, old).text)).unwrap();
        assert!(wrote);
        let now = std::fs::read_to_string(&p).unwrap();
        let got = ini::parse(&now).unwrap();
        assert_eq!(got.get("netplay", "name"), Some("花映塚"));
        assert_eq!(got.get("netplay", "enable"), Some("0"));
        // 同じ中身なら書かない。
        assert_eq!(
            ini_store::rewrite(&p, |old| Some(old.to_string())),
            Ok(false)
        );
        assert_eq!(ini_store::rewrite(&p, |_| None), Ok(false));
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from("HiganRTool.ini")]);
        // 読めない中身は書かない。
        std::fs::write(&p, b"\xFF\xFE[a]").unwrap();
        assert!(ini_store::rewrite(&p, |_| Some("x".into())).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"\xFF\xFE[a]");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // 壊れたら落ちる: 配る ini 自体を土台にしても値が移らない・揺れる（雛形の書き方が移し方の想定から外れた）。
    #[test]
    fn the_shipped_template_is_a_fixed_point_and_takes_a_users_values() {
        let shipped = crate::ini_store::dist_ini_text();
        let same = upgrade(&shipped, &shipped);
        assert_eq!(same.text, shipped);
        assert!(same.is_safe() && same.carried.is_empty(), "{same:?}");
        assert!(!same.kept.is_empty());
        let empty = upgrade(&shipped, "");
        assert_eq!(empty.text, shipped);
        // 利用者が全部の値を変えた ini を、配る ini の形に移しても全部残る。
        let mine = ini::parse_lenient(&shipped).ini;
        let mut user = String::new();
        for s in std::iter::once("").chain(mine.sections().iter().map(String::as_str)) {
            if !s.is_empty() {
                user.push_str(&format!("[{s}]\n"));
            }
            for (k, _) in mine.keys(s) {
                user.push_str(&format!("{k} = u_{s}_{k}\n"));
            }
        }
        let r = upgrade(&shipped, &user);
        assert!(r.is_safe() && r.carried.is_empty(), "{r:?}");
        let got = ini::parse_lenient(&r.text).ini;
        for s in std::iter::once("").chain(mine.sections().iter().map(String::as_str)) {
            for (k, _) in mine.keys(s) {
                assert_eq!(
                    got.get(s, k),
                    Some(format!("u_{s}_{k}").as_str()),
                    "{s} {k}"
                );
            }
        }
    }
}
