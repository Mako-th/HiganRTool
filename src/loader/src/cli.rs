//! コマンドラインの読み取り（pure）。
//!
//! ```text
//! HiganRTool.exe [--choose-game] [--dll <DLL のパス>]
//! HiganRTool.exe --game <th09.exe のパス> [--dll <DLL のパス>] [--ini <ini のパス>] [--script <台本のパス>]
//! HiganRTool.exe --selftest [--dll <DLL のパス>]    （feature `dist` なしのビルドだけ）
//! ```
//!
//! `--game` なしは覚えた th09.exe の場所を使い、無い・使えないときだけウィンドウで選ばせる（`--choose-game` なら常にウィンドウ）。
//! `--ini` / `--script` は DLL が読む場所を環境変数で子へ渡す（同じ PC の 2 つのゲームに別々のポートと台本を持たせる）。
//! `--selftest-target` は自己検査の内部の入口。自己検査は配るビルド（feature `dist`）には入れない。

use std::ffi::OsString;
use std::path::PathBuf;

use higanr_core::lang::{Lang, Tx};
use higanr_core::mode_pick::Pick;

use crate::ui_lang::by_lang;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameSource {
    /// ini の `[loader] game` は読み書きしない。
    Path(PathBuf),
    /// `--game` なし。`choose` なら覚えた場所があってもウィンドウを出す。
    Remembered { choose: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Launch {
        game: GameSource,
        dll: Option<PathBuf>,
        ini: Option<PathBuf>,
        script: Option<PathBuf>,
        /// 起動前にプロセスのメモリを 1 か所書き換える（`launch::SECOND_INSTANCE`）。
        second: bool,
        no_clipboard: bool,
        /// ウィンドウが開いた直後に、`0`〜`8` か `delay` と打ったのと同じ経路で対戦の形を選ぶ。ウィンドウを開かない回には効かない。
        choose: Option<Pick>,
        /// `--script` ありなら無くても同じ。
        background: bool,
    },
    #[cfg(not(feature = "dist"))]
    SelfTest {
        dll: Option<PathBuf>,
    },
    #[cfg(not(feature = "dist"))]
    SelfTestTarget {
        dll: PathBuf,
    },
    Help,
}

/// 自己検査の行。配るビルドの使い方には出さない（`concat!` は展開したマクロの文字列も受ける）。
#[cfg(not(feature = "dist"))]
macro_rules! selftest_usage_ja {
    () => {
        "
  HiganRTool.exe --selftest [--dll <DLL のパス>]
      花映塚は起動しない。この exe 自身をターゲットにして、起動と読み込みの経路を確かめる。"
    };
}
#[cfg(feature = "dist")]
macro_rules! selftest_usage_ja {
    () => {
        ""
    };
}
#[cfg(not(feature = "dist"))]
macro_rules! selftest_usage_en {
    () => {
        "
  HiganRTool.exe --selftest [--dll <DLL path>]
      Does not start the game. Uses this exe itself as the target and checks the launch and DLL loading."
    };
}
#[cfg(feature = "dist")]
macro_rules! selftest_usage_en {
    () => {
        ""
    };
}

pub const USAGE: Tx = Tx {
    ja: concat!(
        "\
使い方:
  HiganRTool.exe [--choose-game] [--dll <DLLのパス>]
      花映塚のフォルダを選ぶウィンドウを出して、選んだ場所を覚え、花映塚を起動する（次からはウィンドウを出さない）。
      --choose-gameは、覚えた場所があっても選び直すウィンドウを出す。
  HiganRTool.exe --game <th09.exeのパス> [--dll <DLLのパス>] [--ini <iniのパス>] [--script <台本のパス>]
      花映塚を止めた状態で起動し、DLLを読み込ませてから動かす。
      --dllを省くと、このexeと同じフォルダのHiganRTool.dll。
      --ini / --scriptはDLLが読むiniと台本（省くとDLLのフォルダのHiganRTool.iniとscript.txt）。
      ここで渡した台本は、読んだ後にlogs\\ へ移さない。
      iniの[netplay]にcode_side（offer / answer）があれば、起動の後もこのウィンドウに残って接続コードをやり取りする
      （自分のコードを出してクリップボードへコピーする。相手のコードはこのウィンドウに貼ってEnter）。
      --secondは同じPCの2つ目の起動（起動の前にプロセスのメモリの1バイトを書き換える。ゲームのファイルは変えない）。
      --no-clipboardは、接続コードをウィンドウに出すだけでクリップボードへコピーしない（開発で2台を起こすとき、自分の
      クリップボードを書き換えないため。付けなければコピーする）。
      --chooseの後の0〜8かdelayは、接続コードのウィンドウが開いた直後に、その文字を打ったのと同じ経路で同期方式を選ぶ
      （開発の実機の回用。人はウィンドウに打つ）。
      --backgroundは、起こした花映塚のウィンドウを前面に出さない（作業中のウィンドウから入力を奪わない。台本なしの開発の起動用。
      --scriptを付けた起動は、付けなくても同じ。付けない普通の起動は今までどおり前面に出る）。",
        selftest_usage_ja!()
    ),
    en: concat!(
        "\
Usage:
  HiganRTool.exe [--choose-game] [--dll <DLL path>]
      Opens a folder picker, remembers the folder you choose, and starts Touhou 9 (the picker does not open from the next launch).
      --choose-game opens the picker even if a folder is remembered.
  HiganRTool.exe --game <path to th09.exe> [--dll <DLL path>] [--ini <ini path>] [--script <script path>]
      Starts the game suspended, loads the DLL, then lets the game run.
      Without --dll, the HiganRTool.dll in the same folder as this exe is used.
      --ini / --script are the ini and script the DLL reads (default: HiganRTool.ini and script.txt in the DLL folder).
      A script given here is not moved to logs\\ after it is read.
      If [netplay] in the ini has code_side (offer / answer), this window stays open after launch to exchange connection codes
      (your code is shown and copied to the clipboard; paste your opponent's code here and press Enter).
      --second is a second launch on the same PC (it changes 1 byte of the game process memory before launch; the game files are not changed).
      --no-clipboard only shows your connection code in this window and does not copy it (for starting two games during
      development, so your clipboard is left alone; without it, the code is copied).
      A 0-8 or delay after --choose picks the netcode right after the connection code window opens, the same way as typing it there
      (for development runs; people type it in the window).
      --background does not bring the game window to the front (so it does not take input from the window you are working in; for
      launches without a script. A launch with --script does the same without it. A normal launch without it comes to the front as before).",
        selftest_usage_en!()
    ),
};

pub fn parse(args: &[OsString], lang: Lang) -> Result<Command, String> {
    let mut game: Option<PathBuf> = None;
    let mut dll: Option<PathBuf> = None;
    let mut ini: Option<PathBuf> = None;
    let mut script: Option<PathBuf> = None;
    #[cfg(not(feature = "dist"))]
    let mut selftest = false;
    // 配るビルドには自己検査の引数が無い（下の分岐は常に「なし」になる）。
    #[cfg(feature = "dist")]
    let selftest = false;
    let mut second = false;
    let mut no_clipboard = false;
    let mut choose_game = false;
    let mut background = false;
    let mut choose: Option<Pick> = None;
    #[cfg(not(feature = "dist"))]
    let mut target: Option<PathBuf> = None;
    #[cfg(feature = "dist")]
    let target: Option<PathBuf> = None;
    let mut help = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a_str = a.to_string_lossy();
        let mut value = |name: &str| -> Result<PathBuf, String> {
            it.next().map(PathBuf::from).ok_or_else(
                || by_lang!(lang, ja: "{name}の後にパスがありません", en: "No path after {name}"),
            )
        };
        match a_str.as_ref() {
            "--game" => set_once(&mut game, value("--game")?, "--game", lang)?,
            "--dll" => set_once(&mut dll, value("--dll")?, "--dll", lang)?,
            "--ini" => set_once(&mut ini, value("--ini")?, "--ini", lang)?,
            "--script" => set_once(&mut script, value("--script")?, "--script", lang)?,
            #[cfg(not(feature = "dist"))]
            "--selftest-target" => set_once(
                &mut target,
                value("--selftest-target")?,
                "--selftest-target",
                lang,
            )?,
            #[cfg(not(feature = "dist"))]
            "--selftest" => selftest = true,
            "--second" => {
                if second {
                    return Err(twice(lang, "--second"));
                }
                second = true
            }
            "--no-clipboard" => {
                if no_clipboard {
                    return Err(twice(lang, "--no-clipboard"));
                }
                no_clipboard = true
            }
            "--background" => {
                if background {
                    return Err(twice(lang, "--background"));
                }
                background = true
            }
            "--choose-game" => {
                if choose_game {
                    return Err(twice(lang, "--choose-game"));
                }
                choose_game = true
            }
            "--choose" => {
                let v = value("--choose")?;
                let pick = Pick::parse(&v.to_string_lossy()).ok_or_else(|| {
                    let v = v.to_string_lossy();
                    by_lang!(
                        lang,
                        ja: "--chooseの値{v:?}は0〜8かdelayのどれか",
                        en: "The value {v:?} for --choose must be 0-8 or delay"
                    )
                })?;
                if choose.replace(pick).is_some() {
                    return Err(twice(lang, "--choose"));
                }
            }
            "--help" | "-h" | "/?" => help = true,
            other => {
                return Err(
                    by_lang!(lang, ja: "知らない引数です: {other}", en: "Unknown argument: {other}"),
                );
            }
        }
    }

    if help {
        return Ok(Command::Help);
    }
    // 自己検査と的には渡す先が無い。黙って捨てない。
    if game.is_none()
        && (ini.is_some()
            || script.is_some()
            || second
            || no_clipboard
            || choose.is_some()
            || background)
    {
        return Err(match lang {
            Lang::Ja => "--ini / --script / --second / --no-clipboard / --choose / --backgroundは--gameと一緒にだけ使えます",
            Lang::En => "--ini / --script / --second / --no-clipboard / --choose / --background can only be used together with --game",
        }
        .to_string());
    }
    if choose_game && (game.is_some() || selftest || target.is_some()) {
        return Err(choose_game_conflict(lang));
    }
    match (game, selftest, target) {
        (game, false, None) => Ok(Command::Launch {
            game: match game {
                Some(g) => GameSource::Path(g),
                None => GameSource::Remembered {
                    choose: choose_game,
                },
            },
            dll,
            ini,
            script,
            second,
            no_clipboard,
            choose,
            background,
        }),
        #[cfg(not(feature = "dist"))]
        (None, true, None) => Ok(Command::SelfTest { dll }),
        #[cfg(not(feature = "dist"))]
        (None, false, Some(dll_t)) if dll.is_none() => Ok(Command::SelfTestTarget { dll: dll_t }),
        _ => Err(only_one_of(lang)),
    }
}

#[cfg(not(feature = "dist"))]
fn choose_game_conflict(lang: Lang) -> String {
    match lang {
        Lang::Ja => "--choose-gameは--game / --selftestと一緒には使えません",
        Lang::En => "--choose-game cannot be used together with --game / --selftest",
    }
    .to_string()
}

#[cfg(feature = "dist")]
fn choose_game_conflict(lang: Lang) -> String {
    match lang {
        Lang::Ja => "--choose-gameは--gameと一緒には使えません",
        Lang::En => "--choose-game cannot be used together with --game",
    }
    .to_string()
}

#[cfg(not(feature = "dist"))]
fn only_one_of(lang: Lang) -> String {
    match lang {
        Lang::Ja => "--game / --selftest / --selftest-targetは1つだけ指定してください",
        Lang::En => "Give only one of --game / --selftest / --selftest-target",
    }
    .to_string()
}

// 配るビルドでは `selftest` が常に false・`target` が常に None で、実際にはここへ来ない。
#[cfg(feature = "dist")]
fn only_one_of(lang: Lang) -> String {
    match lang {
        Lang::Ja => "引数の組み合わせが正しくありません",
        Lang::En => "Invalid combination of arguments",
    }
    .to_string()
}

/// 起こしたウィンドウを前面にしないか。`--script` か `--background` ならしない。台本の無い普通の起動は前面に出す。
pub fn launches_in_background(background: bool, script: Option<&PathBuf>) -> bool {
    background || script.is_some()
}

fn twice(lang: Lang, name: &str) -> String {
    by_lang!(lang, ja: "{name}が2回あります", en: "{name} is given twice")
}

fn set_once(slot: &mut Option<PathBuf>, v: PathBuf, name: &str, lang: Lang) -> Result<(), String> {
    if slot.is_some() {
        return Err(twice(lang, name));
    }
    *slot = Some(v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Result<Command, String> {
        parse(&v.iter().map(OsString::from).collect::<Vec<_>>(), Lang::Ja)
    }

    #[test]
    fn parses_the_three_forms_and_rejects_mixtures() {
        assert_eq!(
            p(&["--game", "C:\\g\\a.exe"]),
            Ok(Command::Launch {
                game: GameSource::Path("C:\\g\\a.exe".into()),
                dll: None,
                ini: None,
                script: None,
                second: false,
                no_clipboard: false,
                choose: None,
                background: false,
            })
        );
        assert_eq!(
            p(&["--dll", "d.dll", "--game", "a.exe"]),
            Ok(Command::Launch {
                game: GameSource::Path("a.exe".into()),
                dll: Some("d.dll".into()),
                ini: None,
                script: None,
                second: false,
                no_clipboard: false,
                choose: None,
                background: false,
            })
        );
        #[cfg(not(feature = "dist"))]
        {
            assert_eq!(p(&["--selftest"]), Ok(Command::SelfTest { dll: None }));
            assert_eq!(
                p(&["--selftest-target", "x.dll"]),
                Ok(Command::SelfTestTarget {
                    dll: "x.dll".into()
                })
            );
        }
        assert_eq!(p(&["-h"]), Ok(Command::Help));
        assert_eq!(
            p(&[]),
            Ok(Command::Launch {
                game: GameSource::Remembered { choose: false },
                dll: None,
                ini: None,
                script: None,
                second: false,
                no_clipboard: false,
                choose: None,
                background: false,
            })
        );
        assert!(p(&["--game"]).is_err());
        assert!(p(&["--game", "a.exe", "--selftest"]).is_err());
        assert!(p(&["--game", "a.exe", "--game", "b.exe"]).is_err());
        assert!(p(&["--selftest-target", "x.dll", "--dll", "y.dll"]).is_err());
        assert!(p(&["--vpatch"]).is_err());
    }

    #[test]
    fn ini_and_script_go_with_game_only() {
        assert_eq!(
            p(&[
                "--game",
                "a.exe",
                "--script",
                "C:\\s\\two_01_host.txt",
                "--ini",
                "C:\\s\\two_host.ini"
            ]),
            Ok(Command::Launch {
                game: GameSource::Path("a.exe".into()),
                dll: None,
                ini: Some("C:\\s\\two_host.ini".into()),
                script: Some("C:\\s\\two_01_host.txt".into()),
                second: false,
                no_clipboard: false,
                choose: None,
                background: false,
            })
        );
        assert_eq!(
            p(&["--game", "a.exe", "--ini", "x.ini"]),
            Ok(Command::Launch {
                game: GameSource::Path("a.exe".into()),
                dll: None,
                ini: Some("x.ini".into()),
                script: None,
                second: false,
                no_clipboard: false,
                choose: None,
                background: false,
            })
        );
        assert!(p(&["--game", "a.exe", "--ini"]).is_err());
        assert!(p(&["--game", "a.exe", "--ini", "a", "--ini", "b"]).is_err());
        assert!(p(&["--game", "a.exe", "--script", "a", "--script", "b"]).is_err());
        assert!(p(&["--selftest", "--ini", "x.ini"]).is_err());
        assert!(p(&["--selftest", "--script", "x.txt"]).is_err());
        assert!(p(&["--ini", "x.ini"]).is_err());
    }

    #[test]
    fn second_goes_with_game_only() {
        assert_eq!(
            p(&["--game", "a.exe", "--second"]),
            Ok(Command::Launch {
                game: GameSource::Path("a.exe".into()),
                dll: None,
                ini: None,
                script: None,
                second: true,
                no_clipboard: false,
                choose: None,
                background: false,
            })
        );
        assert!(p(&["--game", "a.exe", "--second", "--second"]).is_err());
        assert!(p(&["--selftest", "--second"]).is_err());
        assert!(p(&["--second"]).is_err());
    }

    #[test]
    fn no_clipboard_goes_with_game_only_and_defaults_off() {
        let launch = |args: &[&str]| match p(args) {
            Ok(Command::Launch { no_clipboard, .. }) => Some(no_clipboard),
            _ => None,
        };
        assert_eq!(launch(&["--game", "a.exe", "--no-clipboard"]), Some(true));
        assert_eq!(launch(&["--no-clipboard", "--game", "a.exe"]), Some(true));
        assert_eq!(launch(&["--game", "a.exe"]), Some(false));
        assert!(p(&["--game", "a.exe", "--no-clipboard", "--no-clipboard"]).is_err());
        assert!(p(&["--selftest", "--no-clipboard"]).is_err());
        assert!(p(&["--no-clipboard"]).is_err());
        assert!(USAGE.ja.contains("--no-clipboard"));
    }

    #[test]
    fn choose_is_the_same_reading_as_the_typed_pick() {
        let choose = |args: &[&str]| match p(args) {
            Ok(Command::Launch { choose, .. }) => Some(choose),
            _ => None,
        };
        assert_eq!(choose(&["--game", "a.exe"]), Some(None));
        for (v, want) in [
            ("0", Pick::Rollback(0)),
            ("2", Pick::Rollback(2)),
            ("8", Pick::Rollback(8)),
            ("delay", Pick::Delay),
            ("DELAY", Pick::Delay),
        ] {
            assert_eq!(
                choose(&["--game", "a.exe", "--choose", v]),
                Some(Some(want)),
                "{v}"
            );
        }
        // 9 は auto の印でウィンドウの選択ではない。auto の語も受けない。
        for bad in ["9", "auto", "rollback", "", "10"] {
            assert!(p(&["--game", "a.exe", "--choose", bad]).is_err(), "{bad:?}");
        }
        assert!(p(&["--game", "a.exe", "--choose"]).is_err());
        assert!(p(&["--game", "a.exe", "--choose", "0", "--choose", "1"]).is_err());
        assert!(p(&["--selftest", "--choose", "0"]).is_err());
        assert!(p(&["--choose", "0"]).is_err());
        assert!(USAGE.ja.contains("--choose"));
        let e = p(&["--game", "a.exe", "--choose", "9"]).unwrap_err();
        assert!(e.contains("0〜8かdelay"), "{e}");
        assert!(
            USAGE.ja.contains("--chooseの後の0〜8かdelay"),
            "{}",
            USAGE.ja
        );
        assert!(
            !USAGE.ja.contains("auto"),
            "使い方が auto を選べると言っている"
        );
    }

    #[test]
    fn background_and_script_pick_the_quiet_start() {
        let bg = |args: &[&str]| match p(args) {
            Ok(Command::Launch {
                background, script, ..
            }) => Some(launches_in_background(background, script.as_ref())),
            _ => None,
        };
        assert_eq!(bg(&["--game", "a.exe", "--script", "s.txt"]), Some(true));
        assert_eq!(bg(&["--game", "a.exe", "--background"]), Some(true));
        assert_eq!(
            bg(&["--game", "a.exe", "--background", "--script", "s.txt"]),
            Some(true)
        );
        assert_eq!(bg(&["--game", "a.exe"]), Some(false));
        assert_eq!(bg(&["--game", "a.exe", "--ini", "x.ini"]), Some(false));
        assert_eq!(bg(&["--game", "a.exe", "--second"]), Some(false));
        assert_eq!(bg(&[]), Some(false));
        assert!(p(&["--game", "a.exe", "--background", "--background"]).is_err());
        assert!(p(&["--selftest", "--background"]).is_err());
        assert!(p(&["--background"]).is_err());
        assert!(USAGE.ja.contains("--background"));
    }

    #[test]
    fn choose_game_goes_with_the_remembered_form_only() {
        let src = |args: &[&str]| match p(args) {
            Ok(Command::Launch { game, .. }) => Some(game),
            _ => None,
        };
        assert_eq!(
            src(&["--choose-game"]),
            Some(GameSource::Remembered { choose: true })
        );
        assert_eq!(src(&[]), Some(GameSource::Remembered { choose: false }));
        assert_eq!(
            src(&["--game", "a.exe"]),
            Some(GameSource::Path("a.exe".into()))
        );
        assert!(p(&["--choose-game", "--choose-game"]).is_err());
        assert!(p(&["--choose-game", "--game", "a.exe"]).is_err());
        assert!(p(&["--game", "a.exe", "--choose-game"]).is_err());
        assert!(p(&["--choose-game", "--selftest"]).is_err());
        assert!(p(&["--choose-game", "--selftest-target", "x.dll"]).is_err());
        assert!(p(&["--choose-game", "--ini", "x.ini"]).is_err());
        assert!(p(&["--second"]).is_err());
        assert!(USAGE.ja.contains("--choose-game"));
    }

    // 何が壊れたら落ちるか: 使い方の連結（`concat!`）が崩れて本体の説明が欠ける・消える、
    // または自己検査の行が dist の有無と逆に出る。
    #[test]
    fn usage_lists_selftest_only_without_dist() {
        for t in [USAGE.ja, USAGE.en] {
            assert!(t.contains("--game") && t.contains("--background"));
            assert!(t.trim_end().len() == t.len(), "末尾に余計な空白");
            assert_eq!(t.contains("--selftest"), !cfg!(feature = "dist"), "{t}");
        }
    }

    // 何が壊れたら落ちるか: 配るビルドで自己検査の引数が受け付けられる・内部の入口が残る。
    // `dist` の有無は cfg なので、この検査が見るのは今のビルドの側だけ（dist なしでは「受け付ける」側）。
    #[test]
    fn selftest_arguments_exist_only_without_dist() {
        for args in [&["--selftest"][..], &["--selftest-target", "x.dll"][..]] {
            assert_eq!(p(args).is_ok(), !cfg!(feature = "dist"), "{args:?}");
        }
        #[cfg(feature = "dist")]
        {
            let e = p(&["--selftest"]).unwrap_err();
            assert!(e.contains("知らない引数"), "{e}");
            assert!(!only_one_of(Lang::Ja).contains("selftest"));
            assert!(!only_one_of(Lang::En).contains("selftest"));
            assert!(!choose_game_conflict(Lang::Ja).contains("selftest"));
            assert!(!choose_game_conflict(Lang::En).contains("selftest"));
        }
    }
}
