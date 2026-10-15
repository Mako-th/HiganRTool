//! 更新して起こし直した初回だけの仕事。ini を配る ini の形に合わせ、変更点を出して Enter を待つ。
//!
//! - 呼ぶのは配るバージョンの、人が起こした初回（`update::start_with`）だけ。台本・開発用の引数の起動、ツールがゲームのフォルダの中の
//!   起動では呼ばない。
//! - ini の値は引き継ぐ（`higanr_core::ini_upgrade`）。読めない行があるなど安全に合わせられないときは、ini に触らない。
//! - Enter を待つのは、標準入力も出力もコンソールのときだけ（`Host::interactive`）。パイプや検査の道具からの起動では待たない。

use std::path::Path;

use higanr_core::ini_store;
use higanr_core::ini_upgrade::{self, Upgraded};
use higanr_core::lang::{Lang, Tx};

use crate::code_relay::INI_FILE;
use crate::ui_lang::by_lang;
use crate::update::Host;

/// 変更点の文。exe に埋める。
pub const NOTES: Tx = Tx {
    ja: include_str!("../release_notes/ja.txt"),
    en: include_str!("../release_notes/en.txt"),
};

pub const ENTER_TEXT: Tx = Tx {
    ja: "Enterキーで花映塚を起動します。",
    en: "Press Enter to start Touhou 9.",
};

/// 名前を並べる数の上限（長い一覧で画面が流れないように）。
const LIST_MAX: usize = 8;

pub struct Plan<'a> {
    /// ini と exe があるフォルダ。
    pub dir: &'a Path,
    /// 土台にする、配る ini の全文。
    pub ini_template: &'a str,
    pub notes: &'a str,
    pub lang: Lang,
}

pub fn run(host: &mut dyn Host, p: &Plan<'_>) {
    upgrade_ini(host, p.dir, p.ini_template, p.lang);
    show_notes(host, p.notes, p.lang);
}

/// `dir` の ini を `template` の形に合わせる。ini が無ければ何もしない（無いときの書き出しは DLL の仕事）。
pub fn upgrade_ini(host: &mut dyn Host, dir: &Path, template: &str, lang: Lang) {
    let path = dir.join(INI_FILE);
    if !path.is_file() {
        return;
    }
    let mut report: Option<Upgraded> = None;
    let wrote = ini_store::rewrite(&path, |old| {
        let up = ini_upgrade::upgrade_known(template, old);
        let text = up.is_safe().then(|| up.text.clone());
        report = Some(up);
        text
    });
    match (wrote, report) {
        (Err(why), _) => {
            // 理由の文は日本語のログ用なので、英語の画面には出さない（ログには残す）。
            crate::crash_log::stage(&format!("ini を新しい形に合わせられなかった: {why}"));
            host.say(&match lang {
                Lang::Ja => format!(
                    "{INI_FILE}を新しい形に合わせられませんでした（{why}）。今のまま使います。"
                ),
                Lang::En => {
                    format!("Could not update {INI_FILE} to the new layout. Keeping it as it is.")
                }
            });
        }
        (Ok(_), Some(up)) if !up.is_safe() => host.say(&unsafe_text(&up, lang)),
        (Ok(true), Some(up)) => {
            let n = up.kept.len() + up.carried.len();
            host.say(&if n > 0 {
                by_lang!(
                    lang,
                    ja: "{INI_FILE}を新しい形に合わせました（今の設定{n}個を引き継ぎました）。",
                    en: "Updated {INI_FILE} to the new layout ({n} of your settings carried over)."
                )
            } else {
                by_lang!(
                    lang,
                    ja: "{INI_FILE}を新しい形に合わせました。",
                    en: "Updated {INI_FILE} to the new layout."
                )
            });
            if !up.defaulted.is_empty() {
                let d = up.defaulted.len();
                host.say(&by_lang!(
                    lang,
                    ja: "前のバージョンの既定値のままだった設定{d}個は、新しい既定値にしました。",
                    en: "{d} settings that were still at the previous version's defaults now use the new defaults."
                ));
            }
            if !up.carried.is_empty() {
                let list = names(&up.carried);
                let c = up.carried.len();
                host.say(&by_lang!(
                    lang,
                    ja: "新しい形に無い設定{c}個は、そのまま残しました: {list}",
                    en: "{c} settings that are not in the new layout were kept as they are: {list}"
                ));
            }
        }
        (Ok(_), _) => {}
    }
}

fn unsafe_text(up: &Upgraded, lang: Lang) -> String {
    if up.unreadable.is_empty() {
        let list = names(&up.lost);
        return by_lang!(
            lang,
            ja: "{INI_FILE}を新しい形に合わせた結果を確かめられなかったので（{list}）、今のまま使います。",
            en: "Could not verify {INI_FILE} after updating it to the new layout ({list}). Keeping it as it is."
        );
    }
    let lines = up
        .unreadable
        .iter()
        .take(LIST_MAX)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    by_lang!(
        lang,
        ja: "{INI_FILE}に読めない行があるので（{lines}行目）、新しい形に合わせず、今のまま使います。",
        en: "{INI_FILE} has lines that cannot be read (line {lines}), so it was not updated to the new layout. Keeping it as it is."
    )
}

fn names(keys: &[ini_upgrade::KeyRef]) -> String {
    let mut list = keys
        .iter()
        .take(LIST_MAX)
        .map(ini_upgrade::KeyRef::shown)
        .collect::<Vec<_>>()
        .join(", ");
    if keys.len() > LIST_MAX {
        list.push_str(" ...");
    }
    list
}

/// 変更点を出す。コンソールが人のときだけ Enter を待つ。文が空なら何も出さず待たない。
pub fn show_notes(host: &mut dyn Host, notes: &str, lang: Lang) {
    let notes = notes.trim();
    if notes.is_empty() {
        return;
    }
    host.show(&format!("\n{notes}\n"));
    if host.interactive() {
        host.show(ENTER_TEXT.get(lang));
        host.wait_enter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lang::has_japanese;
    use std::ffi::OsString;

    #[derive(Default)]
    struct Rec {
        interactive: bool,
        said: Vec<String>,
        shown: Vec<String>,
        waits: usize,
    }

    impl Host for Rec {
        fn ask_update(&mut self, _text: &str) -> bool {
            panic!("尋ねない")
        }
        fn tell(&mut self, _text: &str) {
            panic!("ウィンドウで知らせない")
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn show(&mut self, text: &str) {
            self.shown.push(text.to_string());
        }
        fn progress(&mut self, _line: &str) {}
        fn progress_end(&mut self) {}
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn wait_enter(&mut self) {
            self.waits += 1;
        }
        fn restart(&mut self, _exe: &Path, _args: &[OsString]) -> Result<i32, String> {
            panic!("起こし直さない")
        }
    }

    const TEMPLATE: &str = "; 説明\r\n[netplay]\r\nenable = 1\r\nname = HiganRTool\r\n; room =\r\n\r\n[ui]\r\nlang = auto\r\n";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "HiganRTool_loader_post_update_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("一時フォルダ");
        dir
    }

    // 壊れたら落ちる: 更新で利用者の設定（名前・ウィンドウの大きさ）が雛形の値に戻る、または新しいキーが入らない。
    #[test]
    fn the_ini_is_rewritten_in_the_new_layout_keeping_the_users_values() {
        let dir = temp_dir("rewrite");
        let ini = dir.join(INI_FILE);
        std::fs::write(
            &ini,
            "[netplay]\nname = ユーザー\nroom = r1\nold = 1\n[window]\nwidth = 800\n",
        )
        .expect("ini");
        let mut host = Rec::default();
        upgrade_ini(&mut host, &dir, TEMPLATE, Lang::Ja);
        let now = std::fs::read_to_string(&ini).expect("読める");
        let got = higanr_core::ini::parse(&now).expect("読める ini");
        assert_eq!(got.get("netplay", "name"), Some("ユーザー"));
        assert_eq!(got.get("netplay", "room"), Some("r1"));
        assert_eq!(
            got.get("netplay", "enable"),
            Some("1"),
            "新しいキーが入っていない"
        );
        assert_eq!(
            got.get("netplay", "old"),
            Some("1"),
            "雛形に無い設定が消えた"
        );
        assert_eq!(got.get("window", "width"), Some("800"));
        assert!(
            now.contains("; 説明\r\n"),
            "雛形の注と改行になっていない: {now:?}"
        );
        assert_eq!(host.said.len(), 2, "{:?}", host.said);
        assert!(host.said[0].contains("4個"), "{}", host.said[0]);
        assert!(host.said[1].contains("[netplay] old"), "{}", host.said[1]);
        assert!(
            !host
                .said
                .iter()
                .any(|l| l.contains("ユーザー") || l.contains("800")),
            "値を画面に出した"
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("読める")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(INI_FILE)],
            "余計なファイル（バックアップ・一時ファイル）が残った"
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: 前のバージョンの既定値のままの設定が新しい既定値にならない（古い値が固定される）、
    // または利用者が変えた値まで新しい既定値に戻る。数を画面に言わない。
    #[test]
    fn settings_still_at_the_old_defaults_take_the_new_defaults() {
        let new_template = TEMPLATE.replace("enable = 1", "enable = 2");
        for (lang, said_defaults) in [
            (Lang::Ja, "既定値にしました"),
            (Lang::En, "use the new defaults"),
        ] {
            let dir = temp_dir("old_defaults");
            let ini = dir.join(INI_FILE);
            std::fs::write(&ini, "[netplay]\nenable = 1\nname = ユーザー\n").expect("ini");
            let mut host = Rec::default();
            upgrade_ini(&mut host, &dir, &new_template, lang);
            let got = higanr_core::ini::parse(&std::fs::read_to_string(&ini).expect("読める"))
                .expect("読める ini");
            assert_eq!(got.get("netplay", "enable"), Some("2"));
            assert_eq!(got.get("netplay", "name"), Some("ユーザー"));
            assert_eq!(host.said.len(), 2, "{:?}", host.said);
            assert!(host.said[0].contains("1個") || host.said[0].contains("1 of"));
            assert!(host.said[1].contains(said_defaults), "{}", host.said[1]);
            assert!(
                host.said[1].contains("1 ") || host.said[1].contains("1個"),
                "{}",
                host.said[1]
            );

            std::fs::write(&ini, "[netplay]\nenable = 0\n").expect("ini");
            let mut host = Rec::default();
            upgrade_ini(&mut host, &dir, &new_template, lang);
            let got = higanr_core::ini::parse(&std::fs::read_to_string(&ini).expect("読める"))
                .expect("読める ini");
            assert_eq!(got.get("netplay", "enable"), Some("0"), "変えた値が戻った");
            assert_eq!(host.said.len(), 1, "{:?}", host.said);
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    // 壊れたら落ちる: ini が無いのに作る（DLL が書き出す既定と取り合う）、読めない行のある ini を黙って書き換えて行を失う。
    #[test]
    fn no_ini_means_nothing_and_an_unreadable_ini_is_left_alone() {
        let dir = temp_dir("leave");
        let mut host = Rec::default();
        upgrade_ini(&mut host, &dir, TEMPLATE, Lang::Ja);
        assert!(!dir.join(INI_FILE).exists());
        assert!(host.said.is_empty());

        let ini = dir.join(INI_FILE);
        let broken = "[netplay]\nname = a\nthis has no equals\n";
        std::fs::write(&ini, broken).expect("ini");
        upgrade_ini(&mut host, &dir, TEMPLATE, Lang::Ja);
        assert_eq!(std::fs::read_to_string(&ini).expect("読める"), broken);
        assert_eq!(host.said.len(), 1);
        assert!(host.said[0].contains("3行目"), "{}", host.said[0]);

        let sjis_garbage = b"\xFF\xFE[a]";
        std::fs::write(&ini, sjis_garbage).expect("ini");
        let mut host = Rec::default();
        upgrade_ini(&mut host, &dir, TEMPLATE, Lang::En);
        assert_eq!(std::fs::read(&ini).expect("読める"), sjis_garbage);
        assert!(
            host.said[0].starts_with("Could not update HiganRTool.ini"),
            "{}",
            host.said[0]
        );
        assert!(!has_japanese(&host.said[0]), "{}", host.said[0]);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: すでに新しい形の ini を毎回書き直す・「合わせました」と言う。
    #[test]
    fn an_ini_already_in_the_new_layout_is_not_touched_or_announced() {
        let dir = temp_dir("same");
        let ini = dir.join(INI_FILE);
        std::fs::write(&ini, TEMPLATE).expect("ini");
        let before = std::fs::metadata(&ini)
            .expect("情報")
            .modified()
            .expect("時刻");
        let mut host = Rec::default();
        upgrade_ini(&mut host, &dir, TEMPLATE, Lang::Ja);
        assert!(host.said.is_empty(), "{:?}", host.said);
        assert_eq!(std::fs::read_to_string(&ini).expect("読める"), TEMPLATE);
        assert_eq!(
            std::fs::metadata(&ini)
                .expect("情報")
                .modified()
                .expect("時刻"),
            before
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: 検査の道具・台本の起動でも Enter を待って止まる／人の起動で待たずに流れてしまう。
    #[test]
    fn enter_is_awaited_only_on_a_human_console() {
        let mut human = Rec {
            interactive: true,
            ..Rec::default()
        };
        show_notes(&mut human, "見出し\n・1 つ目\n", Lang::Ja);
        assert_eq!(human.waits, 1);
        assert!(human.shown[0].contains("・1 つ目"), "{:?}", human.shown);
        assert_eq!(
            human.shown.last().map(String::as_str),
            Some("Enterキーで花映塚を起動します。")
        );

        let mut piped = Rec::default();
        show_notes(&mut piped, "見出し\n・1 つ目\n", Lang::Ja);
        assert_eq!(piped.waits, 0, "コンソールが人でないのに待った");
        assert_eq!(piped.shown.len(), 1, "待たないのに『Enter で』と出した");
        assert!(piped.shown[0].contains("・1 つ目"), "変更点は出す");

        let mut empty = Rec {
            interactive: true,
            ..Rec::default()
        };
        show_notes(&mut empty, " \n", Lang::En);
        assert_eq!((empty.waits, empty.shown.len()), (0, 0), "空の文で待った");
    }

    // 壊れたら落ちる: 英語の画面に日本語の変更点・日本語の Enter の案内が出る。
    #[test]
    fn the_notes_and_the_prompt_follow_the_language() {
        for lang in Lang::ALL {
            let mut host = Rec {
                interactive: true,
                ..Rec::default()
            };
            show_notes(&mut host, NOTES.get(lang), lang);
            let all = host.shown.join("\n");
            let all = all.replace(crate::ui_lang::GAME_ITEM_NAME, "");
            assert_eq!(has_japanese(&all), lang == Lang::Ja, "{lang:?}: {all}");
            assert_eq!(host.waits, 1);
        }
        assert!(!NOTES.ja.trim().is_empty() && !NOTES.en.trim().is_empty());
        // ゲームのメニューの名前は訳さない。それ以外の日本語が混ざれば落ちる。
        let en = NOTES.en.replace(crate::ui_lang::GAME_ITEM_NAME, "");
        assert!(!has_japanese(&en), "英語の変更点に日本語がある");
    }
}
