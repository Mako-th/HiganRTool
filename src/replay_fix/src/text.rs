//! 画面の文言（日本語・英語）。用語は slow / charge（ゲームの表記）、「リプレイ」「フォルダ」。

use std::path::{Path, PathBuf};

use higanr_core::lang::{Lang, Tx};

use crate::model::{Analysis, Notice};
use crate::scan::MAX_FILES;
use crate::work::Report;

const fn tx(ja: &'static str, en: &'static str) -> Tx {
    Tx { ja, en }
}

#[derive(Clone, Copy, Debug)]
pub struct Text(pub Lang);

impl Text {
    pub fn title(self) -> &'static str {
        tx(
            "HiganRToolリプレイ修復ツール",
            "HiganRTool Replay Repair Tool",
        )
        .get(self.0)
    }

    pub fn start_hint(self) -> &'static str {
        tx(
            "チャージの設定が合わず、再生でずれるリプレイを直します。\n直したいリプレイか、リプレイの入ったフォルダを選んでください。\nこのウィンドウへドラッグしても選べます。",
            "Fixes replays that go out of sync because the charge types differ.\nChoose the replays, or a folder that contains them.\nYou can also drop them on this window.",
        )
        .get(self.0)
    }

    pub fn choose_replays(self) -> &'static str {
        tx("リプレイを選ぶ", "Choose replays").get(self.0)
    }

    pub fn choose_folder(self) -> &'static str {
        tx("フォルダを選ぶ", "Choose a folder").get(self.0)
    }

    pub fn close(self) -> &'static str {
        tx("終了", "Exit").get(self.0)
    }

    pub fn prev(self) -> &'static str {
        tx("◀ 前へ", "◀ Prev").get(self.0)
    }

    pub fn next(self) -> &'static str {
        tx("次へ ▶", "Next ▶").get(self.0)
    }

    pub fn make(self) -> &'static str {
        tx("リプレイ単体を修復", "Fix this replay").get(self.0)
    }

    pub fn make_all(self) -> &'static str {
        tx(
            "フォルダ全体のリプレイを同様に修復",
            "Fix the whole folder the same way",
        )
        .get(self.0)
    }

    pub fn file_label(self) -> &'static str {
        tx("ファイル: ", "File: ").get(self.0)
    }

    pub fn side_label(self) -> &'static str {
        tx("側", "Side").get(self.0)
    }

    pub fn name_label(self) -> &'static str {
        tx("名前", "Name").get(self.0)
    }

    pub fn current_label(self) -> &'static str {
        tx("今の設定", "Current").get(self.0)
    }

    pub fn new_value_label(self) -> &'static str {
        tx("直した後の設定", "New").get(self.0)
    }

    pub fn language_label(self) -> &'static str {
        tx("言語", "Language").get(self.0)
    }

    pub fn open_title(self) -> &'static str {
        tx("リプレイを選ぶ", "Choose replays").get(self.0)
    }

    pub fn folder_title(self) -> &'static str {
        tx("フォルダを選ぶ", "Choose a folder").get(self.0)
    }

    pub fn filter_name(self) -> &'static str {
        tx("リプレイ(*.rpy)", "Replays (*.rpy)").get(self.0)
    }

    /// 「側」の行。`mine` が自分の側なら、そちらを「自分」にする。
    pub fn side_word(self, mine: bool) -> &'static str {
        if mine {
            tx("自分", "You").get(self.0)
        } else {
            tx("相手", "Opponent").get(self.0)
        }
    }

    /// 「n / 全体」。`index` は 0 始まり。
    pub fn position(self, index: usize, len: usize) -> String {
        format!("{} / {}", index + 1, len)
    }

    /// 直せないリプレイの理由。直せるなら `None`。
    pub fn unavailable(self, a: &Analysis) -> Option<String> {
        Some(match a {
            Analysis::Fixable(_) => return None,
            Analysis::Recorded => tx(
                "新しいバージョンで保存した／直し済みのリプレイなので、修正を適用しませんでした。",
                "This replay was saved by a newer version or has already been fixed. There is nothing to fix.",
            )
            .get(self.0)
            .to_string(),
            Analysis::NoMark => tx(
                "HiganRToolで保存したリプレイではないので修正を適用しませんでした。",
                "This replay was not saved by HiganRTool, so it cannot be fixed.",
            )
            .get(self.0)
            .to_string(),
            Analysis::OddValue(a, b) => match self.0 {
                Lang::Ja => format!(
                    "チャージの設定の値（{a}, {b}）がslow / chargeのどちらでもないので、直せません。"
                ),
                Lang::En => format!(
                    "The charge values ({a}, {b}) are neither slow nor charge, so it cannot be fixed."
                ),
            },
            Analysis::Unreadable(e) => match self.0 {
                Lang::Ja => format!("読めないリプレイです（{e}）。"),
                Lang::En => format!("This replay cannot be read ({e})."),
            },
        })
    }

    pub fn notice(self, n: &Notice) -> String {
        match (n, self.0) {
            (Notice::NothingFound, Lang::Ja) => "リプレイが見つかりませんでした。".to_string(),
            (Notice::NothingFound, Lang::En) => "No replays were found.".to_string(),
            (Notice::TooMany, Lang::Ja) => {
                format!("リプレイが{MAX_FILES}本を超えたので、途中までを読み込みました。")
            }
            (Notice::TooMany, Lang::En) => {
                format!("There are more than {MAX_FILES} replays. Only the first part was loaded.")
            }
            (Notice::Made(p), Lang::Ja) => format!("作りました: {}", short(p)),
            (Notice::Made(p), Lang::En) => format!("Made: {}", short(p)),
            (Notice::Unchanged, Lang::Ja) => "変えるものがありません。".to_string(),
            (Notice::Unchanged, Lang::En) => "Nothing to change.".to_string(),
            (Notice::Exists(p), Lang::Ja) => {
                format!(
                    "同じ名前のリプレイが既にあるので、作りませんでした: {}",
                    short(p)
                )
            }
            (Notice::Exists(p), Lang::En) => {
                format!(
                    "A replay with the same name already exists, so it was not made: {}",
                    short(p)
                )
            }
            (Notice::Failed(e), Lang::Ja) => format!("作れませんでした: {e}"),
            (Notice::Failed(e), Lang::En) => format!("Could not make it: {e}"),
        }
    }

    /// 直した後の結果（単体・フォルダ全体・`--batch` で共通）。複数行。
    pub fn result_report(self, r: &Report) -> String {
        let ja = self.0 == Lang::Ja;
        let (total, made) = (r.total(), r.made.len());
        let mut lines = vec![if ja {
            format!("{total}件中{made}件を処理しました。")
        } else {
            let noun = if total == 1 { "replay" } else { "replays" };
            format!("Processed {made} of {total} {noun}.")
        }];
        if made > 0 {
            let dirs: Vec<String> = r
                .output_dirs()
                .iter()
                .map(|d| format!("\"{}\"", d.display()))
                .collect();
            let dirs = dirs.join(if ja { "、" } else { ", " });
            lines.push(if ja {
                format!("修復したリプレイは {dirs} に保存されています。")
            } else {
                format!("The fixed replays are saved in {dirs}.")
            });
        }
        let skipped = skip_ranges(&r.recorded);
        if !skipped.is_empty() {
            lines.push(self.skip_sentence(&skipped));
        }
        let reasons: [(usize, &str, &str); 4] = [
            (r.unchanged, "変えるものがない", "nothing to change"),
            (r.exists, "同じ名前が既にある", "same name already exists"),
            (
                r.no_mark,
                "HiganRToolで保存したものではない",
                "not saved by HiganRTool",
            ),
            (
                r.odd_value,
                "チャージの設定の値がslow / chargeではない",
                "charge value is neither slow nor charge",
            ),
        ];
        for (n, j, e) in reasons {
            if n > 0 {
                lines.push(if ja {
                    format!("作らなかった（{j}）: {n}本")
                } else {
                    format!("Not made ({e}): {n}")
                });
            }
        }
        if !r.failed.is_empty() {
            lines.push(if ja {
                format!("読めなかった・書けなかった: {}本", r.failed.len())
            } else {
                format!("Could not read or write: {}", r.failed.len())
            });
            for (p, e) in r.failed.iter().take(3) {
                lines.push(format!("  {}: {e}", short(p)));
            }
        }
        lines.push(
            tx(
                "花映塚での確認や、ずれるリプレイとの入れ換えは、ご自身で行ってください。",
                "Check them in the game and replace the replays that go out of sync yourself.",
            )
            .get(self.0)
            .to_string(),
        );
        lines.join("\n")
    }

    /// 「A〜B は修正が不要なのでスキップしました。」。`ranges` は空でない。
    fn skip_sentence(self, ranges: &[(String, String)]) -> String {
        let (sep, join) = match self.0 {
            Lang::Ja => ("〜", "、"),
            Lang::En => ("–", ", "),
        };
        let list = ranges
            .iter()
            .map(|(a, b)| {
                if a == b {
                    a.clone()
                } else {
                    format!("{a}{sep}{b}")
                }
            })
            .collect::<Vec<_>>()
            .join(join);
        match self.0 {
            Lang::Ja => format!("{list}は修正が不要なのでスキップしました。"),
            Lang::En if ranges.len() == 1 && ranges[0].0 == ranges[0].1 => {
                format!("{list} needs no fix and was skipped.")
            }
            Lang::En => format!("{list} need no fix and were skipped."),
        }
    }

    pub fn usage(self) -> &'static str {
        tx(
            "使い方:\n  リプレイかフォルダをこのexeに重ねて落とすと、それを選んだ状態で始まります。\n  replay_fix [--lang ja|en] [リプレイかフォルダ]...\n  replay_fix --batch <1Pの型>-<2Pの型> [--logファイル] [--lang ja|en] <リプレイかフォルダ>...\n    型はslowかcharge（例slow-charge）。終了コード: 0 = 完了 / 1 = 作れなかったものがある / 2 = 使い方 / 3 = リプレイが無い",
            "Usage:\n  Drop replays or a folder on this exe to start with them chosen.\n  replay_fix [--lang ja|en] [replay or folder]...\n  replay_fix --batch <1P type>-<2P type> [--log file] [--lang ja|en] <replay or folder>...\n    Types are slow or charge (e.g. slow-charge). Exit code: 0 = done / 1 = some could not be made / 2 = usage / 3 = no replays",
        )
        .get(self.0)
    }
}

/// 言語の選択欄に出す名前。どちらの言語で表示していても、選ぶ人が読める自分の言語の名前のまま。
pub fn lang_name(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "日本語",
        Lang::En => "English",
    }
}

/// スキップしたもの（対象の並びでの位置の昇順, パス）を、並びで続く同じフォルダのものどうしで「最初の名前, 最後の名前」にまとめる。
pub fn skip_ranges(items: &[(usize, PathBuf)]) -> Vec<(String, String)> {
    let name = |p: &Path| {
        p.file_name().map_or_else(
            || p.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut prev: Option<&(usize, PathBuf)> = None;
    for it in items {
        let continues = prev.is_some_and(|p| p.0 + 1 == it.0 && p.1.parent() == it.1.parent());
        match (continues, out.last_mut()) {
            (true, Some(last)) => last.1 = name(&it.1),
            _ => out.push((name(&it.1), name(&it.1))),
        }
        prev = Some(it);
    }
    out
}

/// 出力先の「フォルダ名\ファイル名」。画面は長いパスを全部は出さない。
pub fn short(p: &Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
    let dir = p
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned());
    match (dir, name) {
        (Some(d), Some(n)) => format!("{d}\\{n}"),
        (None, Some(n)) => n,
        _ => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::MakeResult;
    use std::path::PathBuf;

    #[test]
    fn both_languages_are_filled_and_japanese_text_has_japanese() {
        let ja = Text(Lang::Ja);
        let en = Text(Lang::En);
        let pair = [
            (ja.title(), en.title()),
            (ja.start_hint(), en.start_hint()),
            (ja.choose_replays(), en.choose_replays()),
            (ja.choose_folder(), en.choose_folder()),
            (ja.close(), en.close()),
            (ja.prev(), en.prev()),
            (ja.next(), en.next()),
            (ja.make(), en.make()),
            (ja.make_all(), en.make_all()),
            (ja.file_label(), en.file_label()),
            (ja.side_label(), en.side_label()),
            (ja.name_label(), en.name_label()),
            (ja.current_label(), en.current_label()),
            (ja.language_label(), en.language_label()),
            (ja.new_value_label(), en.new_value_label()),
            (ja.open_title(), en.open_title()),
            (ja.folder_title(), en.folder_title()),
            (ja.filter_name(), en.filter_name()),
            (ja.side_word(true), en.side_word(true)),
            (ja.side_word(false), en.side_word(false)),
            (ja.usage(), en.usage()),
        ];
        for (j, e) in pair {
            assert!(!j.is_empty() && !e.is_empty());
            assert_ne!(j, e, "{j}");
            assert!(higanr_core::lang::has_japanese(j), "{j}");
            assert!(!higanr_core::lang::has_japanese(e), "{e}");
        }
    }

    #[test]
    fn the_buttons_and_table_headings_use_the_names_of_a_standard_dialog() {
        let (ja, en) = (Text(Lang::Ja), Text(Lang::En));
        for (t, want) in [
            (
                ja,
                [
                    "リプレイ単体を修復",
                    "フォルダ全体のリプレイを同様に修復",
                    "終了",
                    "側",
                    "名前",
                    "今の設定",
                    "直した後の設定",
                    "言語",
                ],
            ),
            (
                en,
                [
                    "Fix this replay",
                    "Fix the whole folder the same way",
                    "Exit",
                    "Side",
                    "Name",
                    "Current",
                    "New",
                    "Language",
                ],
            ),
        ] {
            let got = [
                t.make(),
                t.make_all(),
                t.close(),
                t.side_label(),
                t.name_label(),
                t.current_label(),
                t.new_value_label(),
                t.language_label(),
            ];
            assert_eq!(got, want);
        }
        // 言語の選択欄の名前は、表示中の言語に関わらず自分の言語の名前。
        assert_eq!(lang_name(Lang::Ja), "日本語");
        assert_eq!(lang_name(Lang::En), "English");
        assert!(ja.prev().contains('◀') && en.next().contains('▶'));
    }

    #[test]
    fn a_made_notice_shows_the_pair_folder_and_the_file_name() {
        let p = PathBuf::from("x")
            .join("Alice vs Bob")
            .join("charge-slow")
            .join("th9_ud0008.rpy");
        assert_eq!(
            Text(Lang::Ja).notice(&Notice::Made(p.clone())),
            "作りました: charge-slow\\th9_ud0008.rpy"
        );
        assert_eq!(
            Text(Lang::En).notice(&Notice::Made(p)),
            "Made: charge-slow\\th9_ud0008.rpy"
        );
    }

    #[test]
    fn only_replays_that_cannot_be_fixed_get_a_reason() {
        let t = Text(Lang::Ja);
        assert_eq!(
            t.unavailable(&Analysis::Recorded).as_deref(),
            Some(
                "新しいバージョンで保存した／直し済みのリプレイなので、修正を適用しませんでした。"
            )
        );
        for a in [
            Analysis::NoMark,
            Analysis::OddValue(2, 0),
            Analysis::Unreadable("x".into()),
        ] {
            assert!(t.unavailable(&a).is_some());
        }
    }

    fn ud(n: usize) -> (usize, PathBuf) {
        (
            n - 1,
            PathBuf::from("rep").join(format!("th9_ud{n:04}.rpy")),
        )
    }

    fn ranges(ns: &[usize]) -> Vec<(String, String)> {
        let items: Vec<_> = ns.iter().map(|&n| ud(n)).collect();
        skip_ranges(&items)
    }

    fn names(a: &str, b: &str) -> (String, String) {
        (format!("th9_ud{a}.rpy"), format!("th9_ud{b}.rpy"))
    }

    #[test]
    fn skipped_names_are_grouped_into_ranges_of_neighbours_in_the_target_order() {
        assert_eq!(ranges(&[]), []);
        assert_eq!(ranges(&[2]), [names("0002", "0002")]);
        assert_eq!(ranges(&[5, 6, 7, 8]), [names("0005", "0008")]);
        assert_eq!(ranges(&[5, 6]), [names("0005", "0006")]);
        assert_eq!(
            ranges(&[2, 5, 6, 7, 8]),
            [names("0002", "0002"), names("0005", "0008")]
        );
        // 一つ飛ばしは続いていない。
        assert_eq!(
            ranges(&[2, 4]),
            [names("0002", "0002"), names("0004", "0004")]
        );
        // 位置が続いていても、フォルダが変われば別の範囲。
        let items = [
            (0, PathBuf::from("a").join("1.rpy")),
            (1, PathBuf::from("b").join("2.rpy")),
        ];
        assert_eq!(skip_ranges(&items).len(), 2);
    }

    #[test]
    fn the_skip_sentence_reads_in_both_languages() {
        let sentence = |lang, ns: &[usize]| Text(lang).skip_sentence(&ranges(ns));
        assert_eq!(
            sentence(Lang::Ja, &[2]),
            "th9_ud0002.rpyは修正が不要なのでスキップしました。"
        );
        assert_eq!(
            sentence(Lang::Ja, &[5, 6, 7, 8]),
            "th9_ud0005.rpy〜th9_ud0008.rpyは修正が不要なのでスキップしました。"
        );
        assert_eq!(
            sentence(Lang::Ja, &[2, 5, 6, 7, 8]),
            "th9_ud0002.rpy、th9_ud0005.rpy〜th9_ud0008.rpyは修正が不要なのでスキップしました。"
        );
        assert_eq!(
            sentence(Lang::En, &[2]),
            "th9_ud0002.rpy needs no fix and was skipped."
        );
        assert_eq!(
            sentence(Lang::En, &[5, 6, 7, 8]),
            "th9_ud0005.rpy–th9_ud0008.rpy need no fix and were skipped."
        );
        assert_eq!(
            sentence(Lang::En, &[2, 5, 6]),
            "th9_ud0002.rpy, th9_ud0005.rpy–th9_ud0006.rpy need no fix and were skipped."
        );
    }

    /// `n` 本のうち `skip`（1 始まり）を直し済み、残りを作ったことにした結果と、作った先。
    fn report(n: usize, skip: &[usize]) -> (Report, PathBuf) {
        let dir: PathBuf = ["rep", "fix_replay", "Alice vs Bob", "charge-slow"]
            .iter()
            .collect();
        let mut r = Report::default();
        for i in 1..=n {
            let (idx, src) = ud(i);
            let res = if skip.contains(&i) {
                MakeResult::Recorded
            } else {
                MakeResult::Made(dir.join(src.file_name().unwrap()))
            };
            r.add(idx, &src, res);
        }
        (r, dir)
    }

    #[test]
    fn the_result_report_follows_the_agreed_form() {
        let (r, dir) = report(8, &[2]);
        let d = dir.display();
        assert_eq!(
            Text(Lang::Ja).result_report(&r),
            format!(
                "8件中7件を処理しました。\n修復したリプレイは \"{d}\" に保存されています。\nth9_ud0002.rpyは修正が不要なのでスキップしました。\n花映塚での確認や、ずれるリプレイとの入れ換えは、ご自身で行ってください。"
            )
        );
        assert_eq!(
            Text(Lang::En).result_report(&r),
            format!(
                "Processed 7 of 8 replays.\nThe fixed replays are saved in \"{d}\".\nth9_ud0002.rpy needs no fix and was skipped.\nCheck them in the game and replace the replays that go out of sync yourself."
            )
        );
    }

    #[test]
    fn the_result_report_has_no_folder_line_when_nothing_was_made() {
        let (r, _) = report(2, &[1, 2]);
        assert_eq!(
            Text(Lang::Ja).result_report(&r),
            "2件中0件を処理しました。\nth9_ud0001.rpy〜th9_ud0002.rpyは修正が不要なのでスキップしました。\n花映塚での確認や、ずれるリプレイとの入れ換えは、ご自身で行ってください。"
        );
        let en = Text(Lang::En).result_report(&r);
        assert!(en.starts_with("Processed 0 of 2 replays.\n"), "{en}");
        assert!(!en.contains("saved in"), "{en}");
    }

    #[test]
    fn the_result_report_counts_the_other_reasons() {
        let (mut r, _) = report(1, &[]);
        r.unchanged = 2;
        r.exists = 1;
        r.failed
            .push((PathBuf::from("a").join("2.rpy"), "読めない".into()));
        let s = Text(Lang::Ja).result_report(&r);
        assert!(s.starts_with("5件中1件を処理しました。"), "{s}");
        assert!(s.contains("作らなかった（変えるものがない）: 2本"), "{s}");
        assert!(s.contains("作らなかった（同じ名前が既にある）: 1本"), "{s}");
        assert!(s.contains("読めなかった・書けなかった: 1本"), "{s}");
        assert!(s.contains("a\\2.rpy: 読めない"), "{s}");
        assert!(!s.contains("スキップ"), "{s}");
        assert!(!s.contains("チャージの設定の値"), "{s}");
    }
}
