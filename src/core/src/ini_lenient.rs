//! 配る `HiganRTool.ini` を、誤りのある行だけ捨てて読むための前処理（pure）。
//!
//! 1 行の誤りで節ごと（ネットワークならネット対戦ごと）使えなくならないよう、読む前に誤りの行を空行に置き換える。
//! 行の番号は変わらない。捨てた行は `IniNote` に残し、ログとメニューの 1 行に出す。
//!
//! - 書き方の誤り（`=` が無い・見出しが閉じない・キーが空・同じキーが 2 回）はどの節でも捨てる。
//! - 値の誤り・知らないキーは、利用者が触る節（`CHECKED`）だけ、その節を読む関数で確かめて捨てる。
//!   `[netplay]` と `[spectate]` は、キーごとの既定を持つ dll の `net_config` が読むときに注を足す。
//! - 実験用の節は確かめない（誤りは従来どおり、その節を読む側が断る）。
#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use crate::ini::{self, IniError};
use crate::{autosave, lang, log_cfg, pacing, pad_dirs, window_size};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// 読めない行を捨てて、既定値で動かす。
    Ignored,
    /// UTF-8 でなかったので Shift_JIS として読み直した。
    Encoding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniNote {
    pub kind: NoteKind,
    /// 行の番号（1 から）。行を持たない注は 0。
    pub line: usize,
    /// 節の名前（小文字）。分からなければ空。
    pub section: String,
    /// キー（小文字）。分からなければ空。
    pub key: String,
    /// 読めない理由（ログ用）。
    pub why: String,
}

impl IniNote {
    pub fn ignored(line: usize, section: &str, key: &str, why: String) -> IniNote {
        IniNote {
            kind: NoteKind::Ignored,
            line,
            section: section.to_string(),
            key: key.to_string(),
            why,
        }
    }

    pub fn encoding() -> IniNote {
        IniNote {
            kind: NoteKind::Encoding,
            line: 0,
            section: String::new(),
            key: String::new(),
            why: "UTF-8 でないので Shift_JIS（CP932）として読み直した".to_string(),
        }
    }

    pub fn log_text(&self) -> String {
        match self.kind {
            NoteKind::Encoding => format!("HiganRTool.ini: {}", self.why),
            NoteKind::Ignored => {
                let at = match (self.section.is_empty(), self.key.is_empty()) {
                    (true, _) => String::new(),
                    (false, true) => format!(" [{}]", self.section),
                    (false, false) => format!(" [{}] {}", self.section, self.key),
                };
                format!(
                    "HiganRTool.ini の {} 行目{at} を読めない（{}）。この行は使わず、既定値で動かす",
                    self.line, self.why
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    /// 誤りの行を空行にした本文。`ini::parse` が断らない。
    pub text: String,
    /// 行の番号の順。
    pub notes: Vec<IniNote>,
}

type Check = fn(&str) -> Result<(), String>;

fn parsed(t: &str) -> Result<ini::Ini, String> {
    ini::parse(t).map_err(|e| e.to_string())
}

/// 値を確かめる節と、その節を読む関数。
const CHECKED: [(&str, Check); 6] = [
    (pacing::SECTION, |t| {
        pacing::mode_from_ini(&parsed(t)?).map(|_| ())
    }),
    (autosave::SECTION, |t| {
        autosave::settings_from_ini(&parsed(t)?).map(|_| ())
    }),
    (pad_dirs::SECTION, |t| {
        pad_dirs::settings_from_text(t).map(|_| ())
    }),
    (lang::SECTION, |t| {
        lang::setting_in_ini(&parsed(t)?).map(|_| ())
    }),
    (log_cfg::SECTION, |t| {
        log_cfg::choose(Some(t), false).note.map_or(Ok(()), Err)
    }),
    (window_size::SECTION, |t| {
        window_size::from_ini(&parsed(t)?).map(|_| ())
    }),
];

/// `lines`（1 から）の行を空行にする。改行はそのまま残す。
fn blank_lines(text: &str, lines: &BTreeSet<usize>) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, raw) in text.split_inclusive('\n').enumerate() {
        if lines.contains(&(i + 1)) {
            out.push_str(&raw[raw.trim_end_matches(['\r', '\n']).len()..]);
        } else {
            out.push_str(raw);
        }
    }
    out
}

pub fn clean(text: &str) -> Cleaned {
    let r = ini::parse_lenient(text);
    let mut notes: Vec<IniNote> = r
        .skipped
        .iter()
        .map(|s| {
            let key = match &s.error {
                IniError::Duplicate { key, .. } => key.as_str(),
                _ => "",
            };
            IniNote::ignored(s.error.line(), &s.section, key, s.error.to_string())
        })
        .collect();
    let mut dropped: BTreeSet<usize> = r.dropped.iter().copied().collect();
    let mut body = blank_lines(text, &dropped);
    for (section, check) in CHECKED {
        filter_section(&mut body, &mut dropped, section, check, &mut notes);
    }
    notes.sort_by_key(|n| n.line);
    Cleaned { text: body, notes }
}

/// `section` の行を、先頭から 1 行ずつ「この行までを足しても `check` が通るか」で採るか捨てるかを決める。
/// 後ろの行を足した形が通れば、残りは全部採る（後ろの行との組み合わせで通る行を、順番のせいで捨てない）。
/// 最後まで通らない組み合わせが残ったら、その節の行を全部捨てる。
fn filter_section(
    body: &mut String,
    dropped: &mut BTreeSet<usize>,
    section: &str,
    check: Check,
    notes: &mut Vec<IniNote>,
) {
    let Ok(ini) = ini::parse(body) else {
        return;
    };
    let keys: Vec<(String, usize)> = ini
        .keys(section)
        .into_iter()
        .map(|(k, l)| (k.to_string(), l))
        .collect();
    if keys.is_empty() || check(body).is_ok() {
        return;
    }
    let mut rejected: BTreeSet<usize> = BTreeSet::new();
    for (i, (key, line)) in keys.iter().enumerate() {
        if check(&blank_lines(body, &rejected)).is_ok() {
            break;
        }
        let later: BTreeSet<usize> = keys[i + 1..].iter().map(|(_, l)| *l).collect();
        let upto = rejected.union(&later).copied().collect();
        if let Err(why) = check(&blank_lines(body, &upto)) {
            rejected.insert(*line);
            notes.push(IniNote::ignored(*line, section, key, why));
        }
    }
    if let Err(why) = check(&blank_lines(body, &rejected)) {
        for (key, line) in &keys {
            if rejected.insert(*line) {
                notes.push(IniNote::ignored(*line, section, key, why.clone()));
            }
        }
    }
    dropped.extend(&rejected);
    *body = blank_lines(body, &rejected);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIST: &str = include_str!("../../../scripts/dist/HiganRTool.ini");

    fn lines_of(c: &Cleaned) -> Vec<(usize, &str)> {
        c.notes.iter().map(|n| (n.line, n.key.as_str())).collect()
    }

    #[test]
    fn the_shipped_ini_has_no_notes_and_stays_as_written() {
        let c = clean(DIST);
        assert_eq!(c.notes, vec![]);
        assert_eq!(c.text, DIST);
    }

    /// 配る ini の `[pacing] mode` を手で替えても、配った行は 1 行も断られない。`auto_window` は注なので、
    /// 外して書いたときだけ（`mode = auto` 以外では）その 1 行が断られる。
    #[test]
    fn changing_the_pacing_mode_by_hand_drops_nothing_from_the_shipped_ini() {
        let changed = DIST.replace("\nmode = auto\n", "\nmode = game\n");
        assert_ne!(changed, DIST);
        assert_eq!(
            clean(&changed).notes,
            vec![],
            "配った行は mode を替えても使える"
        );
        let opened = changed.replace("; auto_window = b-hybrid", "auto_window = b-hybrid");
        assert_ne!(opened, changed);
        let c = clean(&opened);
        assert_eq!(c.notes.len(), 1, "{:?}", c.notes);
        assert_eq!(
            (c.notes[0].section.as_str(), c.notes[0].key.as_str()),
            ("pacing", "auto_window")
        );
        assert!(pacing::mode_from_ini(&ini::parse(&c.text).unwrap()).is_ok());
    }

    #[test]
    fn syntax_errors_are_dropped_with_their_line_numbers() {
        let c = clean("[pad]\npov = 1\nfoo\n[ui\nlang = en\n[log]\ndetail = 1\ndetail = 0\n");
        assert_eq!(lines_of(&c), vec![(3, ""), (4, ""), (8, "detail")]);
        assert_eq!(c.text, "[pad]\npov = 1\n\n\n\n[log]\ndetail = 1\n\n");
        assert!(ini::parse(&c.text).is_ok());
        assert_eq!(c.notes[0].section, "pad");
    }

    #[test]
    fn crlf_is_kept_when_a_line_is_blanked() {
        let c = clean("[pad]\r\nfoo\r\npov = 0\r\n");
        assert_eq!(c.text, "[pad]\r\n\r\npov = 0\r\n");
    }

    #[test]
    fn a_bad_value_or_unknown_key_drops_only_that_line() {
        let c = clean(
            "[pacing]\nmode = bogus\n[replay]\nenable = 1\nlayout = date\nlayuot = x\n[ui]\nlang = fr\n[log]\ndetail = 2\n[pad]\npov = 0\n",
        );
        assert_eq!(
            lines_of(&c),
            vec![(2, "mode"), (6, "layuot"), (8, "lang"), (10, "detail")]
        );
        let ini = ini::parse(&c.text).unwrap();
        assert_eq!(ini.get("replay", "layout"), Some("date"));
        assert_eq!(ini.get("pad", "pov"), Some("0"));
        assert_eq!(ini.get("pacing", "mode"), None);
        assert!(pacing::mode_from_ini(&ini).is_ok());
    }

    #[test]
    fn a_key_that_is_only_valid_with_a_later_key_is_kept() {
        let c = clean("[pacing]\nfps = 50\nmode = b-spin\n");
        assert_eq!(c.notes, vec![]);
        let c = clean("[window]\nx = 10\nwidth = 640\ny = 20\n");
        assert_eq!(c.notes, vec![]);
    }

    #[test]
    fn a_key_that_needs_a_bad_key_is_dropped_too() {
        let c = clean("[pacing]\nfps = 50\nmode = bogus\n");
        assert_eq!(lines_of(&c), vec![(2, "fps"), (3, "mode")]);
        let c = clean("[window]\nx = 10\ny = zz\n");
        assert_eq!(lines_of(&c), vec![(2, "x"), (3, "y")]);
    }

    #[test]
    fn sections_that_are_not_checked_are_left_alone() {
        let text = "[netplay]\nenable = 1\nupnp = yes\n[spectate]\nenable = off\n[rollback_probe]\nzzz = 1\n";
        let c = clean(text);
        assert_eq!(c.notes, vec![]);
        assert_eq!(c.text, text);
    }

    #[test]
    fn a_value_with_a_trailing_comment_is_valid() {
        let c = clean("[pad]\npov = 0 ; 読まない\n[log]\ndetail = 1 ; 詳細\n");
        assert_eq!(c.notes, vec![]);
    }

    #[test]
    fn the_log_text_names_line_section_and_key() {
        let n = IniNote::ignored(12, "netplay", "upnp", "x".to_string());
        assert!(
            n.log_text()
                .starts_with("HiganRTool.ini の 12 行目 [netplay] upnp")
        );
        assert!(IniNote::encoding().log_text().contains("Shift_JIS"));
    }
}
