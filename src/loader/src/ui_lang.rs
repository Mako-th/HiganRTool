//! 画面の言語の決め方。ini の `[ui] lang`（`auto` / `ja` / `en`）→ `auto` なら OS の UI の言語（`higanr_core::lang::resolve`）。
//!
//! 読めない ini・誤った `[ui]` は `auto` として扱う（言語が決まらずに起動を止めない）。ini の場所は dll と同じ決め方
//! （`code_relay::ini_location`）。メニューで選んだ値は dll が ini の `lang` の行へ書き戻す。

use std::ffi::OsStr;
use std::path::Path;

use higanr_core::lang::{self, Lang, LangSetting};

use crate::code_relay;

/// 2 つの書式のうち言語に合う方で `format!` する。語順が違う文は、片方を機械的にコピーせずそれぞれ書く。
macro_rules! by_lang {
    ($lang:expr, ja: $ja:literal, en: $en:literal $(, $($arg:tt)*)?) => {
        match $lang {
            ::higanr_core::lang::Lang::Ja => format!($ja $(, $($arg)*)?),
            ::higanr_core::lang::Lang::En => format!($en $(, $($arg)*)?),
        }
    };
}
pub(crate) use by_lang;

/// 材料から言語を決める。ini が読めなかった・`[ui]` が誤っていたときは `Auto` 扱い。
pub fn decide(ini_text: Option<&str>, os_langid: u16) -> Lang {
    let setting = ini_text
        .and_then(|t| higanr_core::ini::parse(t).ok())
        .and_then(|ini| lang::setting_in_ini(&ini).ok())
        .unwrap_or(LangSetting::Auto);
    lang::resolve(setting, os_langid)
}

/// ini の場所と DLL のフォルダから、ini を読んで決める。`inherited` は親から継いだ ini の環境変数。
pub fn detect_with(
    ini_flag: Option<&Path>,
    inherited: Option<&OsStr>,
    dll_dir: &Path,
    os_langid: u16,
) -> Lang {
    let ini_text = code_relay::ini_location(ini_flag, inherited, dll_dir)
        .and_then(|p| std::fs::read_to_string(p).ok());
    decide(ini_text.as_deref(), os_langid)
}

pub fn detect(ini_flag: Option<&Path>, dll_dir: &Path, os_langid: u16) -> Lang {
    let inherited = std::env::var_os(higanr_core::ini::PATH_ENV);
    detect_with(ini_flag, inherited.as_deref(), dll_dir, os_langid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli, code_relay as relay, game_pick, hold, launch};
    use higanr_core::lang::{Tx, has_japanese};

    const JA_OS: u16 = 0x0411;
    const EN_OS: u16 = 0x0409;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "HiganRTool_loader_lang_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ini_decides_and_auto_follows_the_os() {
        let en_ini = "[ui]\nlang = en\n";
        let ja_ini = "[ui]\nlang = ja\n";
        assert_eq!(decide(None, JA_OS), Lang::Ja);
        assert_eq!(decide(None, EN_OS), Lang::En);
        assert_eq!(decide(Some(en_ini), JA_OS), Lang::En);
        assert_eq!(decide(Some(ja_ini), EN_OS), Lang::Ja);
        assert_eq!(decide(Some("[ui]\nlang = auto\n"), EN_OS), Lang::En);
        assert_eq!(decide(Some("[ui]\nlang = auto\n"), JA_OS), Lang::Ja);
    }

    #[test]
    fn a_broken_ini_falls_to_the_os_language() {
        assert_eq!(decide(Some("[ui]\nlang = klingon\n"), JA_OS), Lang::Ja);
        assert_eq!(decide(Some("[ui]\nlang = klingon\n"), EN_OS), Lang::En);
        assert_eq!(decide(Some("[ui\nlang = en\n"), JA_OS), Lang::Ja);
    }

    #[test]
    fn detect_reads_the_ini_the_arguments_and_the_environment_name() {
        let root = temp_dir("detect");
        let dll_dir = root.join("bin");
        std::fs::create_dir_all(&dll_dir).unwrap();
        // ini が無い: OS の言語。
        assert_eq!(detect_with(None, None, &dll_dir, EN_OS), Lang::En);
        assert_eq!(detect_with(None, None, &dll_dir, JA_OS), Lang::Ja);
        // DLL のフォルダの ini の固定が OS に勝つ。
        std::fs::write(dll_dir.join("HiganRTool.ini"), "[ui]\nlang = en\n").unwrap();
        assert_eq!(detect_with(None, None, &dll_dir, JA_OS), Lang::En);
        // --ini で別の ini を指すと、そちらの [ui] に従う。
        let other = root.join("other.ini");
        std::fs::write(&other, "[ui]\nlang = ja\n").unwrap();
        assert_eq!(detect_with(Some(&other), None, &dll_dir, EN_OS), Lang::Ja);
        // 継いだ環境変数の ini（絶対パス）はそれに従い、空なら ini は無し（dll もネットワークの準備をしない）。
        assert_eq!(
            detect_with(None, Some(other.as_os_str()), &dll_dir, EN_OS),
            Lang::Ja
        );
        assert_eq!(
            detect_with(None, Some(OsStr::new("")), &dll_dir, EN_OS),
            Lang::En
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 日英の組の一覧。ここに無い定数は検査から漏れるので、`Tx` の定数を足したらここにも足す。
    fn all_tx() -> Vec<(&'static str, Tx)> {
        vec![
            ("cli::USAGE", cli::USAGE),
            ("code_relay::GAME_MENU_HINT", relay::GAME_MENU_HINT),
            ("code_relay::CLEARED_PEER", relay::CLEARED_PEER),
            ("code_relay::CLEANED_UP_MY_CODE", relay::CLEANED_UP_MY_CODE),
            ("code_relay::REFUSE_OWN", relay::REFUSE_OWN),
            ("code_relay::REFUSE_SAME", relay::REFUSE_SAME),
            ("code_relay::REFUSE_SHORT", relay::REFUSE_SHORT),
            ("code_relay::LINGER", relay::LINGER),
            ("code_relay::AFTER_FINISH", relay::AFTER_FINISH),
            ("code_relay::CLOSE_HINT", relay::CLOSE_HINT),
            ("game_pick::DIALOG_TITLE", game_pick::DIALOG_TITLE),
            ("hold::HOLD_TEXT", hold::HOLD_TEXT),
            ("launch::DLL_NOT_ABSOLUTE", launch::DLL_NOT_ABSOLUTE),
            ("launch::DLL_PATH_HAS_NUL", launch::DLL_PATH_HAS_NUL),
        ]
    }

    #[test]
    fn every_pair_has_both_languages_and_english_has_no_japanese() {
        for (name, t) in all_tx() {
            assert!(!t.ja.trim().is_empty(), "{name} の日本語が空");
            assert!(!t.en.trim().is_empty(), "{name} の英語が空");
            assert_ne!(t.ja, t.en, "{name} が日英で同じ");
            assert!(has_japanese(t.ja), "{name} の日本語に日本語の文字が無い");
            // ゲームの項目の名前は訳さないので数えない。英語の文に日本語の括弧は使わない。
            let without_game_item = t.en.replace("妖怪 対 妖怪", "");
            assert!(
                !has_japanese(&without_game_item),
                "{name} の英語に日本語の文字: {}",
                t.en
            );
        }
    }

    #[test]
    fn english_run_shows_english_for_usage_refusal_and_folder_title() {
        assert!(cli::USAGE.get(Lang::En).starts_with("Usage:"));
        assert!(cli::USAGE.get(Lang::Ja).starts_with("使い方:"));
        let refuse = match relay::judge_paste("OWN", Some("OWN"), None) {
            relay::Paste::Refuse(t) => t,
            other => panic!("{other:?}"),
        };
        assert!(refuse.get(Lang::En).contains("your own connection code"));
        assert!(refuse.get(Lang::Ja).contains("自分の接続コード"));
        assert!(game_pick::DIALOG_TITLE.get(Lang::En).contains("th09.exe"));
        assert!(!has_japanese(game_pick::DIALOG_TITLE.get(Lang::En)));
        // 組み立てる文も、英語では日本語の文字を含まない。
        let ident =
            game_pick::ident_mismatch_text(&[higanr_core::pe::IdentField::EntryBytes], Lang::En);
        assert!(!has_japanese(&ident), "{ident}");
        assert!(ident.contains("custom.exe") && ident.contains("EntryBytes"));
        let e = game_pick::PickError::Cancelled.describe(Lang::En);
        assert!(
            !has_japanese(&e) && e.contains("Nothing was started"),
            "{e}"
        );
        let args = [std::ffi::OsString::from("--bogus")];
        let err = cli::parse(&args, Lang::En).unwrap_err();
        assert!(!has_japanese(&err) && err.contains("--bogus"), "{err}");
    }
}
