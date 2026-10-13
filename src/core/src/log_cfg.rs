//! ini の `[log]`（ログを簡潔にするか詳細にするか）の読み方。
//!
//! `detail = 0` は簡潔（本ログ 1 本だけ）、`1` は詳細（CSV なども書く）。書かなければ呼び手の既定。
//! 知らないキー・読めない値は、`[netplay]` などと同じく節ごと使わず既定に戻し、その理由を `note` に返す。

#![forbid(unsafe_code)]

use crate::ini;

pub const SECTION: &str = "log";
/// DLL が簡潔なログの先頭近くに書く行の先頭。ローダーがこれで「CSV が無いのは正常」と知る。
pub const BRIEF_MARK: &str = "ログ: 簡潔";
pub const DETAIL_MARK: &str = "ログ: 詳細";

/// 簡潔なログの本文か（先頭近くに印の行がある）。
pub fn is_brief_log(text: &str) -> bool {
    text.lines().any(|l| l.starts_with(BRIEF_MARK))
}

/// 簡潔なログに「うまくいかなかった」印の行があるか: 試験のまとめ・状態の変化の × 項目、★の行、読み取りのスレッドのパニック、
/// CHECK のズレ。ローダーが「詳しい記録の頼み方」を出す回を選ぶのに使う。
pub fn brief_log_has_trouble(text: &str) -> bool {
    text.lines().any(|l| {
        l.starts_with("× ")
            || l.starts_with('★')
            || l.starts_with("終了: パニック")
            || l.contains("CHECK のズレ")
    })
}
const KEYS: [&str; 1] = ["detail"];
/// `note` に値をコピーするときの長さの上限（長い値でログが膨らまないように）。
const ECHO_MAX: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChoice {
    pub detail: bool,
    /// 既定へ戻したときの理由（本ログに 1 行書く）。ini を使えた・書いていないときは None。
    pub note: Option<String>,
}

fn mode_word(detail: bool) -> &'static str {
    if detail { "詳細" } else { "簡潔" }
}

fn echo(v: &str) -> String {
    let mut s: String = v.chars().take(ECHO_MAX).collect();
    if v.chars().count() > ECHO_MAX {
        s.push('…');
    }
    s
}

/// ini の本文（読めなければ None）から詳細にするかを決める。`default_detail` は書いていないときの値。
pub fn choose(text: Option<&str>, default_detail: bool) -> LogChoice {
    let fallback = |why: String| LogChoice {
        detail: default_detail,
        note: Some(format!(
            "ログ: {why}。既定の{}にする",
            mode_word(default_detail)
        )),
    };
    let Some(text) = text else {
        return LogChoice {
            detail: default_detail,
            note: None,
        };
    };
    let ini = match ini::parse(text) {
        Ok(i) => i,
        Err(e) => return fallback(format!("ini を読めない（{e}）")),
    };
    if !ini.has_section(SECTION) {
        return LogChoice {
            detail: default_detail,
            note: None,
        };
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return fallback(format!(
                "[{SECTION}] の {line} 行目のキー {:?} を知らない（読めるのは {}）",
                echo(k),
                KEYS.join(" / ")
            ));
        }
    }
    match ini.get(SECTION, "detail").map(str::trim) {
        None => LogChoice {
            detail: default_detail,
            note: None,
        },
        Some("0") => LogChoice {
            detail: false,
            note: None,
        },
        Some("1") => LogChoice {
            detail: true,
            note: None,
        },
        Some(v) => fallback(format!(
            "[{SECTION}] detail の値 {:?} は 0（簡潔）か 1（詳細）",
            echo(v)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ini_section_and_key_take_the_default() {
        for d in [false, true] {
            for text in [
                None,
                Some(""),
                Some("[netplay]\nenable = 1\n"),
                Some("[log]\n"),
            ] {
                assert_eq!(
                    choose(text, d),
                    LogChoice {
                        detail: d,
                        note: None
                    },
                    "{text:?} 既定 {d}"
                );
            }
        }
    }

    #[test]
    fn zero_and_one_override_the_default_either_way() {
        assert!(choose(Some("[log]\ndetail = 1\n"), false).detail);
        assert!(!choose(Some("[log]\ndetail = 0\n"), true).detail);
        assert!(choose(Some("[LOG]\n DETAIL = 1 \n"), false).detail);
        assert_eq!(choose(Some("[log]\ndetail = 1\n"), false).note, None);
    }

    #[test]
    fn bad_value_falls_back_to_the_default_and_says_so() {
        for v in ["2", "yes", "", "-1", "01", "true"] {
            for d in [false, true] {
                let c = choose(Some(&format!("[log]\ndetail = {v}\n")), d);
                assert_eq!(c.detail, d, "値 {v:?}");
                let note = c.note.expect("理由の行");
                assert!(note.contains("detail の値"), "{note}");
                assert!(note.contains(mode_word(d)), "{note}");
            }
        }
    }

    #[test]
    fn unknown_key_drops_the_whole_section() {
        let c = choose(Some("[log]\ndetail = 1\nverbose = 1\n"), false);
        assert!(!c.detail, "知らないキーがあれば detail = 1 も使わない");
        assert!(c.note.unwrap().contains("verbose"));
    }

    #[test]
    fn unreadable_ini_falls_back_with_a_note() {
        let c = choose(Some("[log]\ndetail 1\n"), true);
        assert!(c.detail);
        assert!(c.note.unwrap().contains("読めない"));
    }

    /// 壊れたら落ちる: 配る ini から `[log] detail = 0` が消える・書き方が崩れて既定に戻る（開発用の既定は詳細）。
    #[test]
    fn the_distributed_ini_asks_for_the_brief_log_whichever_the_default() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini");
        for default in [false, true] {
            let c = choose(Some(dist), default);
            assert!(!c.detail, "既定 {default}");
            assert_eq!(c.note, None);
        }
        assert!(dist.contains("[log]") && dist.contains("detail = 0"));
    }

    /// 壊れたら落ちる: 案内どおりの手で ini が誤りになる（`[log]` が 2 回・`detail` が 2 回）、または詳細にならない。
    #[test]
    fn the_hint_steps_make_a_valid_ini_with_the_detailed_log() {
        let dist = include_str!("../../../scripts/dist/HiganRTool.ini");
        // [log] がある配る ini: detail = 0 を 1 に変える。
        assert_eq!(dist.matches("detail = 0").count(), 1);
        let changed = dist.replace("detail = 0", "detail = 1");
        // [log] が無い古い ini: 2 行を足す。
        let old = dist
            .replace("[log]", "[nolog]")
            .replace("detail = 0", "detail_x = 0");
        let old: String = old
            .lines()
            .filter(|l| !l.starts_with("[nolog]") && !l.starts_with("detail_x"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!old.contains("[log]"));
        let added = format!("{old}\n[log]\ndetail = 1\n");
        for (what, text) in [("0 を 1 に変えた", &changed), ("2 行を足した", &added)] {
            for default in [false, true] {
                let c = choose(Some(text), default);
                assert!(c.detail, "{what}: 既定 {default}");
                assert_eq!(c.note, None, "{what}");
            }
        }
        // 古い案内（足すだけ）だと、配る ini では節が 2 回になって ini 全体が読めない。
        let doubled = format!("{dist}\n[log]\ndetail = 1\n");
        assert!(choose(Some(&doubled), false).note.is_some());
    }

    #[test]
    fn a_long_value_is_cut_in_the_note() {
        let c = choose(
            Some(&format!("[log]\ndetail = {}\n", "x".repeat(500))),
            false,
        );
        assert!(c.note.unwrap().chars().count() < 120);
    }
}
