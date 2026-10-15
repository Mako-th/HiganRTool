//! 引数の読み方と、窓を出さない `--batch`（検査と手順の自動化用）。

use std::ffi::OsString;
use std::path::PathBuf;

use higanr_core::lang::Lang;

use crate::model::{Notice, Pair, parse_pair};
use crate::scan::collect;
use crate::text::Text;
use crate::work::make_all;

pub const EXIT_OK: i32 = 0;
/// 作れなかったリプレイがある（同じ名前が既にある・印が無い・読めない など）。
pub const EXIT_PROBLEM: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
/// 対象のリプレイが 1 本も見つからなかった。
pub const EXIT_NOTHING: i32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Gui {
        paths: Vec<PathBuf>,
        lang: Option<Lang>,
    },
    Batch {
        pair: Pair,
        paths: Vec<PathBuf>,
        log: Option<PathBuf>,
        lang: Option<Lang>,
    },
}

/// 引数の誤りの理由（使い方と一緒に出す）。
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut paths = Vec::new();
    let mut lang = None;
    let mut log = None;
    let mut batch: Option<Pair> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--lang") => {
                let v = it.next().ok_or_else(|| UsageError("--lang".into()))?;
                lang = Some(
                    v.to_str()
                        .and_then(Lang::from_word)
                        .ok_or_else(|| UsageError("--lang ja|en".into()))?,
                );
            }
            Some("--log") => {
                log = Some(PathBuf::from(
                    it.next().ok_or_else(|| UsageError("--log".into()))?,
                ));
            }
            Some("--batch") => {
                let v = it.next().ok_or_else(|| UsageError("--batch".into()))?;
                batch = Some(
                    v.to_str()
                        .and_then(parse_pair)
                        .ok_or_else(|| UsageError("--batch slow-charge".into()))?,
                );
            }
            Some("-h" | "--help" | "/?") => return Err(UsageError(String::new())),
            Some(s) if s.starts_with("--") => return Err(UsageError(s.to_string())),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    match batch {
        Some(_) if paths.is_empty() => Err(UsageError("--batch".into())),
        Some(pair) => Ok(Command::Batch {
            pair,
            paths,
            log,
            lang,
        }),
        None if log.is_some() => Err(UsageError("--log".into())),
        None => Ok(Command::Gui { paths, lang }),
    }
}

/// `--batch`: 窓を出さずに、見つけたリプレイ全部へ同じ値で作る。(終了コード, 結果の文)。
pub fn run_batch(pair: Pair, inputs: &[PathBuf], lang: Lang) -> (i32, String) {
    let text = Text(lang);
    let found = collect(inputs);
    if found.files.is_empty() {
        return (EXIT_NOTHING, text.notice(&Notice::NothingFound));
    }
    let report = make_all(&found.files, pair);
    let mut out = text.result_report(&report);
    if found.truncated {
        out.push('\n');
        out.push_str(&text.notice(&Notice::TooMany));
    }
    let code = if report.has_problem() || found.truncated {
        EXIT_PROBLEM
    } else {
        EXIT_OK
    };
    (code, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use std::path::Path;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn paths_alone_start_the_window_with_them_chosen() {
        assert_eq!(
            parse(args(&["a.rpy", "dir"])),
            Ok(Command::Gui {
                paths: vec!["a.rpy".into(), "dir".into()],
                lang: None
            })
        );
        assert_eq!(
            parse(args(&[])),
            Ok(Command::Gui {
                paths: vec![],
                lang: None
            })
        );
        assert_eq!(
            parse(args(&["--lang", "en"])),
            Ok(Command::Gui {
                paths: vec![],
                lang: Some(Lang::En)
            })
        );
    }

    #[test]
    fn batch_takes_a_pair_in_1p_dash_2p_order_and_paths() {
        assert_eq!(
            parse(args(&["--batch", "slow-charge", "--log", "l.txt", "d"])),
            Ok(Command::Batch {
                pair: (Kind::Slow, Kind::Charge),
                paths: vec!["d".into()],
                log: Some("l.txt".into()),
                lang: None
            })
        );
    }

    /// `root\Alice vs Bob\th9_ud0001〜0004.rpy`。1・3 本目は直せて、2・4 本目は直し済み。
    fn write_samples(root: &Path) {
        use crate::work::tests::replay;
        use higanr_core::replay::Side;
        use higanr_core::replay_detail::MatchDetail;
        let dir = root.join("Alice vs Bob");
        std::fs::create_dir_all(&dir).unwrap();
        let fixed = MatchDetail {
            charge: Some([0, 0]),
            ..Default::default()
        };
        for n in 1..=4 {
            let detail = (n % 2 == 0).then(|| fixed.clone());
            let bytes = replay(0, 0, Side::P1, detail, true);
            std::fs::write(dir.join(format!("th9_ud{n:04}.rpy")), bytes).unwrap();
        }
    }

    #[test]
    fn batch_writes_under_fix_replay_and_reports_the_skipped_names() {
        let root = crate::work::tests::temp_dir("cli");
        write_samples(&root);
        let pair = (Kind::Charge, Kind::Slow);
        let (code, text) = run_batch(pair, std::slice::from_ref(&root), Lang::Ja);
        let out = root
            .join("fix_replay")
            .join("Alice vs Bob")
            .join("charge-slow");
        assert_eq!(code, EXIT_OK, "{text}");
        assert_eq!(
            text,
            format!(
                "4件中2件を処理しました。\n修復したリプレイは \"{}\" に保存されています。\nth9_ud0002.rpy、th9_ud0004.rpyは修正が不要なのでスキップしました。\n実際のゲームでの確認や、壊れたリプレイとの入れ換えはご自身で行ってください。",
                out.display()
            )
        );
        assert!(out.join("th9_ud0001.rpy").is_file() && out.join("th9_ud0003.rpy").is_file());
        // 出力先の中は読まないので、もう一度でも対象は元の 4 本のまま。
        let (code, text) = run_batch(pair, std::slice::from_ref(&root), Lang::En);
        assert_eq!(code, EXIT_PROBLEM, "{text}");
        assert!(text.starts_with("Processed 0 of 4 replays.\n"), "{text}");
        assert!(
            text.contains("Not made (same name already exists): 2"),
            "{text}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wrong_arguments_are_refused() {
        for bad in [
            &["--batch"][..],
            &["--batch", "slow"],
            &["--batch", "slow-charge"],
            &["--log", "x", "a.rpy"],
            &["--lang", "fr"],
            &["--nope"],
            &["--help"],
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?}");
        }
    }
}
