//! 道具の画面の言語（日本語・英語）。pure。
//!
//! 決まり方は、ini の `[ui] lang`（`auto` / `ja` / `en`）→ `auto` なら OS の UI の言語（`resolve`）。メニューで選んだ値は
//! ini の `lang` の行へ書き戻す（`set_lang_in_ini_text`）。
//! ini の読み書きと OS の言語の取得は dll とローダーの仕事で、ここは文字の解釈と選び方、書き換え後の文だけ。
//! ログ（本ログ・CSV）は切り替えない。

use crate::ini::Ini;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Lang {
    #[default]
    Ja,
    En,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Ja, Lang::En];

    /// ini とファイルの語（`ja` / `en`）。
    pub const fn word(self) -> &'static str {
        match self {
            Lang::Ja => "ja",
            Lang::En => "en",
        }
    }

    pub fn from_word(s: &str) -> Option<Lang> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("ja") {
            Some(Lang::Ja)
        } else if s.eq_ignore_ascii_case("en") {
            Some(Lang::En)
        } else {
            None
        }
    }

    /// Windows の LANGID（`GetUserDefaultUILanguage` の値）から。主言語が日本語（`LANG_JAPANESE` = 0x11）なら日本語、ほかは英語。
    pub const fn from_langid(id: u16) -> Lang {
        if id & 0x3FF == 0x11 {
            Lang::Ja
        } else {
            Lang::En
        }
    }

    /// メニューの ← → で回す次の値。
    pub const fn next(self) -> Lang {
        match self {
            Lang::Ja => Lang::En,
            Lang::En => Lang::Ja,
        }
    }

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn from_index(i: u8) -> Lang {
        if i == 1 { Lang::En } else { Lang::Ja }
    }
}

/// ini の `[ui] lang` の値。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LangSetting {
    /// OS の UI の言語に従う（既定）。
    #[default]
    Auto,
    Fixed(Lang),
}

pub const SECTION: &str = "ui";
const KEYS: [&str; 1] = ["lang"];

/// `[ui]` を読む。節か `lang` が無ければ `Auto`。知らないキー・`auto` / `ja` / `en` 以外の値は断る。
pub fn setting_in_ini(ini: &Ini) -> Result<LangSetting, String> {
    if !ini.has_section(SECTION) {
        return Ok(LangSetting::Auto);
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} を知らない（読めるのは {}）",
                KEYS.join(" / ")
            ));
        }
    }
    match ini.get(SECTION, "lang") {
        None => Ok(LangSetting::Auto),
        Some(v) if v.trim().eq_ignore_ascii_case("auto") => Ok(LangSetting::Auto),
        Some(v) => Lang::from_word(v)
            .map(LangSetting::Fixed)
            .ok_or_else(|| format!("[{SECTION}] lang の値 {v:?} は auto / ja / en")),
    }
}

/// 使う言語。ini が固定していればそれ、`Auto` なら OS の言語。
pub fn resolve(ini: LangSetting, os_langid: u16) -> Lang {
    match ini {
        LangSetting::Fixed(l) => l,
        LangSetting::Auto => Lang::from_langid(os_langid),
    }
}

/// `[ui]` の `lang` の行の値だけを `lang` の語に替えた ini の文を返す。ほかの行・コメント・空行・行末・先頭の BOM は変えない。
/// `[ui]` に `lang` が無ければその節の最後のキーの後ろに足し、`[ui]` が無ければ末尾に節ごと足す。足す行の行末は、
/// 文の最初の行末（CRLF / LF）に合わせる（改行が無い文は CRLF）。`;` `#` で始まる行は節の見出しにも値の行にも数えない
/// （`ini::parse` と同じ読み方）。
pub fn set_lang_in_ini_text(text: &str, lang: Lang) -> String {
    let nl = match text.find('\n') {
        Some(i) if text[..i].ends_with('\r') => "\r\n",
        Some(_) => "\n",
        None => "\r\n",
    };
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let mut in_ui = false;
    let mut ui_last: Option<usize> = None;
    let mut replaced = false;
    for (i, line) in lines.iter_mut().enumerate() {
        let body = line.trim_end_matches(['\r', '\n']);
        let t = body.strip_prefix('\u{FEFF}').unwrap_or(body).trim();
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix('[') {
            if let Some(name) = rest.strip_suffix(']') {
                in_ui = name.trim().eq_ignore_ascii_case(SECTION);
                if in_ui {
                    ui_last = Some(i);
                }
            }
            continue;
        }
        if !in_ui {
            continue;
        }
        ui_last = Some(i);
        let Some(eq) = body.find('=') else {
            continue;
        };
        if !body[..eq].trim().eq_ignore_ascii_case(KEYS[0]) {
            continue;
        }
        let after = &body[eq + 1..];
        let start = eq + 1 + (after.len() - after.trim_start().len());
        let end = start + after.trim().len();
        *line = format!(
            "{}{}{}{}",
            &body[..start],
            lang.word(),
            &body[end..],
            &line[body.len()..]
        );
        replaced = true;
    }
    if !replaced {
        let new_line = format!("lang = {}", lang.word());
        match ui_last {
            Some(i) => {
                if lines[i].ends_with('\n') {
                    lines.insert(i + 1, format!("{new_line}{nl}"));
                } else {
                    lines[i].push_str(nl);
                    lines.insert(i + 1, new_line);
                }
            }
            None => {
                if let Some(last) = lines.last_mut() {
                    if !last.ends_with('\n') {
                        last.push_str(nl);
                    }
                    if !last.trim().is_empty() {
                        lines.push(nl.to_string());
                    }
                }
                lines.push(format!("[{SECTION}]{nl}"));
                lines.push(format!("{new_line}{nl}"));
            }
        }
    }
    lines.concat()
}

/// 日本語の文字（かな・カナ・漢字・全角の記号と空白）か。英語の文に日本語が混ざっていないかを見る検査用。
pub fn is_japanese_char(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{FF00}'..='\u{FFEF}')
}

pub fn has_japanese(s: &str) -> bool {
    s.chars().any(is_japanese_char)
}

/// 2 つの言語の同じ文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tx {
    pub ja: &'static str,
    pub en: &'static str,
}

impl Tx {
    pub const fn get(self, lang: Lang) -> &'static str {
        match lang {
            Lang::Ja => self.ja,
            Lang::En => self.en,
        }
    }

    /// 両方の言語の文字（文字の表に入れる文字を集めるとき）。
    pub const fn both(self) -> [&'static str; 2] {
        [self.ja, self.en]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ini;

    #[test]
    fn langid_japanese_primary_is_ja_and_others_are_en() {
        assert_eq!(Lang::from_langid(0x0411), Lang::Ja);
        assert_eq!(Lang::from_langid(0x0409), Lang::En);
        assert_eq!(Lang::from_langid(0x0809), Lang::En);
        assert_eq!(Lang::from_langid(0x0804), Lang::En);
        assert_eq!(Lang::from_langid(0), Lang::En);
    }

    #[test]
    fn ini_beats_os_and_auto_follows_os() {
        let ja_os = 0x0411;
        let en_os = 0x0409;
        assert_eq!(resolve(LangSetting::Auto, ja_os), Lang::Ja);
        assert_eq!(resolve(LangSetting::Auto, en_os), Lang::En);
        assert_eq!(resolve(LangSetting::Fixed(Lang::En), ja_os), Lang::En);
        assert_eq!(resolve(LangSetting::Fixed(Lang::Ja), en_os), Lang::Ja);
    }

    fn setting(text: &str) -> Result<LangSetting, String> {
        setting_in_ini(&ini::parse(text).unwrap())
    }

    #[test]
    fn ini_reads_auto_ja_en_and_defaults_to_auto() {
        assert_eq!(setting(""), Ok(LangSetting::Auto));
        assert_eq!(setting("[ui]\n"), Ok(LangSetting::Auto));
        assert_eq!(setting("[ui]\nlang = auto\n"), Ok(LangSetting::Auto));
        assert_eq!(
            setting("[ui]\nlang = ja\n"),
            Ok(LangSetting::Fixed(Lang::Ja))
        );
        assert_eq!(
            setting("[ui]\nlang = EN\n"),
            Ok(LangSetting::Fixed(Lang::En))
        );
    }

    #[test]
    fn ini_refuses_unknown_key_and_value() {
        assert!(setting("[ui]\nlanguage = en\n").is_err());
        assert!(setting("[ui]\nlang = english\n").is_err());
        assert!(setting("[ui]\nlang = \n").is_err());
    }

    /// 配る ini と同じ形（コメントの `; [loader]` と `; lang` の行、節の間の空行、[ui] が途中にある）の合成。
    const DIST_LIKE: &str = "; 設定。; か # で始まる行は読まない。\n\n[netplay]\nenable = 1\n; lang = ja\nname = HiganRTool\n\n[pacing]\nmode = auto\n\n[ui]\n; 言語。auto / ja / en\nlang = auto\n\n; 更新の確認をやめるとき\n; [loader]\n; update_check = 0\n";

    fn crlf(s: &str) -> String {
        s.replace('\n', "\r\n")
    }

    #[test]
    fn only_the_lang_value_changes_and_every_other_byte_stays() {
        for text in [DIST_LIKE.to_string(), crlf(DIST_LIKE)] {
            for l in Lang::ALL {
                let out = set_lang_in_ini_text(&text, l);
                assert_eq!(
                    out,
                    text.replace("lang = auto", &format!("lang = {}", l.word()))
                );
                assert_eq!(setting(&out), Ok(LangSetting::Fixed(l)));
                assert_eq!(
                    set_lang_in_ini_text(&out, l),
                    out,
                    "同じ値の書き直しで変わった"
                );
            }
        }
        // 値だけが変わる: 前後の空白・大文字小文字・行末の空白・コメントでない `[ui]` の見出しの書き方はそのまま。
        let odd = "[UI]\n  LANG\t=   ja  \n";
        assert_eq!(
            set_lang_in_ini_text(odd, Lang::En),
            "[UI]\n  LANG\t=   en  \n"
        );
        let empty = "[ui]\nlang =\nname = x\n";
        assert_eq!(
            set_lang_in_ini_text(empty, Lang::Ja),
            "[ui]\nlang =ja\nname = x\n"
        );
    }

    #[test]
    fn a_leading_bom_and_a_missing_final_newline_are_kept() {
        let t = "\u{FEFF}[ui]\r\nlang = en";
        assert_eq!(
            set_lang_in_ini_text(t, Lang::Ja),
            "\u{FEFF}[ui]\r\nlang = ja"
        );
        let t = "\u{FEFF}[netplay]\r\nenable = 1\r\n";
        let out = set_lang_in_ini_text(t, Lang::En);
        assert!(
            out.starts_with("\u{FEFF}[netplay]\r\nenable = 1\r\n"),
            "{out:?}"
        );
        assert_eq!(setting(&out), Ok(LangSetting::Fixed(Lang::En)));
    }

    #[test]
    fn a_missing_lang_line_is_added_inside_the_ui_section() {
        let t = "[ui]\n; lang = ja\nother = 1\n\n[pad]\npov = 1\n";
        assert_eq!(
            set_lang_in_ini_text(t, Lang::En),
            "[ui]\n; lang = ja\nother = 1\nlang = en\n\n[pad]\npov = 1\n"
        );
        // 見出しだけの節・末尾に改行の無い節。
        assert_eq!(
            set_lang_in_ini_text("[ui]\n", Lang::Ja),
            "[ui]\nlang = ja\n"
        );
        assert_eq!(set_lang_in_ini_text("[ui]", Lang::Ja), "[ui]\r\nlang = ja");
        assert_eq!(
            set_lang_in_ini_text("[ui]\r\nx = 1", Lang::En),
            "[ui]\r\nx = 1\r\nlang = en"
        );
    }

    #[test]
    fn a_missing_ui_section_is_appended_with_the_file_line_ending() {
        let lf = "[netplay]\nenable = 1\n";
        assert_eq!(
            set_lang_in_ini_text(lf, Lang::En),
            "[netplay]\nenable = 1\n\n[ui]\nlang = en\n"
        );
        let crlf_text = "[netplay]\r\nenable = 1\r\n\r\n";
        assert_eq!(
            set_lang_in_ini_text(crlf_text, Lang::Ja),
            "[netplay]\r\nenable = 1\r\n\r\n[ui]\r\nlang = ja\r\n"
        );
        assert_eq!(
            set_lang_in_ini_text("[netplay]\nenable = 1", Lang::En),
            "[netplay]\nenable = 1\n\n[ui]\nlang = en\n"
        );
        assert_eq!(set_lang_in_ini_text("", Lang::En), "[ui]\r\nlang = en\r\n");
        // コメントの `; [ui]` は節ではない。
        let c = "; [ui]\n; lang = ja\n[pad]\npov = 1\n";
        let out = set_lang_in_ini_text(c, Lang::En);
        assert!(
            out.starts_with(c) && out.ends_with("[ui]\nlang = en\n"),
            "{out:?}"
        );
        assert_eq!(setting(&out), Ok(LangSetting::Fixed(Lang::En)));
    }

    #[test]
    fn a_lang_line_in_another_section_or_a_comment_is_not_touched() {
        let t = "[netplay]\nlang = ja\n# lang = ja\n;lang = ja\n[ui]\n#lang = ja\n";
        let out = set_lang_in_ini_text(t, Lang::En);
        assert_eq!(
            out,
            "[netplay]\nlang = ja\n# lang = ja\n;lang = ja\n[ui]\nlang = en\n#lang = ja\n"
        );
        assert_eq!(setting(&out), Ok(LangSetting::Fixed(Lang::En)));
    }

    #[test]
    fn has_japanese_sees_kana_kanji_fullwidth_and_the_marks() {
        for s in [
            "あ",
            "ア",
            "漢",
            "ー",
            "、",
            "。",
            "「",
            "」",
            "（",
            "）",
            "〜",
            "\u{3000}",
            "Ａ",
            "x 日本語 y",
        ] {
            assert!(has_japanese(s), "{s:?}");
        }
        for s in [
            "",
            "English only.",
            "(ascii) \"quote\" ~ - ->",
            "ß é ü ←→↑↓",
        ] {
            assert!(!has_japanese(s), "{s:?}");
        }
    }

    #[test]
    fn next_cycles_through_both() {
        assert_eq!(Lang::Ja.next(), Lang::En);
        assert_eq!(Lang::En.next(), Lang::Ja);
        for l in Lang::ALL {
            assert_eq!(Lang::from_index(l.index() as u8), l);
        }
    }

    #[test]
    fn all_lists_each_language_once() {
        assert_eq!(Lang::ALL.len(), 2);
        assert!(Lang::ALL.contains(&Lang::Ja) && Lang::ALL.contains(&Lang::En));
        for (i, l) in Lang::ALL.iter().enumerate() {
            assert_eq!(l.index(), i);
        }
    }

    #[test]
    fn both_lists_each_language_once() {
        let t = Tx {
            ja: "あ", en: "ß"
        };
        assert_eq!(t.both(), ["あ", "ß"]);
        for l in Lang::ALL {
            assert!(t.both().contains(&t.get(l)));
        }
    }

    #[test]
    fn tx_picks_by_lang() {
        let t = Tx {
            ja: "はい",
            en: "Yes",
        };
        assert_eq!(t.get(Lang::Ja), "はい");
        assert_eq!(t.get(Lang::En), "Yes");
    }
}
