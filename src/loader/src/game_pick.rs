//! 花映塚の th09.exe の場所を、初回だけ人に選ばせて覚える（`--game` をリンク先に足させる方式は不慣れな人には難しいため）。
//!
//! 判断だけをここに置き、ウィンドウ・ファイル・画面は `Host` の向こうへ出す。覚えるファイル `game_path.txt` は HiganRTool.exe と
//! 同じフォルダ（花映塚のフォルダの外）に置き、中身は th09.exe のフルパス 1 行。`--game` を付けた起動は読み書きしない。

use std::io;
use std::path::{Path, PathBuf};

use higanr_core::lang::{Lang, Tx};
use higanr_core::pe::{self, Ident, IdentField, TH09_V150A};

use crate::ui_lang::by_lang;

pub const SAVED_FILE: &str = "game_path.txt";

pub const GAME_EXE: &str = "th09.exe";

pub const DIALOG_TITLE: Tx = Tx {
    ja: "花映塚のフォルダを選んでください（th09.exe が入っているフォルダ）",
    en: "Choose the Touhou 9 folder (the folder that contains th09.exe)",
};

/// 初めに開く場所にだけ使う（在れば）。
pub const STANDARD_SUBDIR: &str = "上海アリス幻樂団\\東方花映塚";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Folder(PathBuf),
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickError {
    Cancelled,
    DialogFailed(String),
}

impl PickError {
    pub fn describe(&self, lang: Lang) -> String {
        match self {
            PickError::Cancelled => match lang {
                Lang::Ja => "花映塚のフォルダが選ばれなかったので、何も起動していません。もう一度起動して、フォルダを選んでください。",
                Lang::En => "No Touhou 9 folder was chosen. Nothing was started. Start again and choose the folder.",
            }
            .to_string(),
            PickError::DialogFailed(why) => by_lang!(
                lang,
                ja: "フォルダを選ぶウィンドウを開けませんでした（{why}）。何も起動していません。この文をこの道具を渡してくれた人に送ってください。",
                en: "Could not open the folder picker ({why}). Nothing was started. Send this message to the person who gave you this tool."
            ),
        }
    }
}

/// ウィンドウ・ファイル・画面の外。本物は `main.rs` の `RealPickHost`、検査は偽物。
pub trait Host {
    fn lang(&self) -> Lang;
    fn read_saved(&mut self) -> Option<String>;
    fn write_saved(&mut self, text: &str) -> Result<(), String>;
    fn read_exe(&mut self, exe: &Path) -> io::Result<Vec<u8>>;
    fn is_dir(&mut self, dir: &Path) -> bool;
    fn standard_dir(&mut self) -> Option<PathBuf>;
    fn ask_folder(&mut self, initial: Option<&Path>) -> Answer;
    fn say(&mut self, line: &str);
    /// 黒いウィンドウへ出し、メッセージボックスでも見せる（次の選び直しのウィンドウが前に出ても読めるように）。
    fn warn(&mut self, text: &str);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    Ok,
    Missing,
    Unreadable(String),
    NotV150a(Vec<IdentField>, Vec<String>),
}

fn check_exe<H: Host>(h: &mut H, exe: &Path) -> Check {
    match h.read_exe(exe) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Check::Missing,
        Err(e) => Check::Unreadable(e.to_string()),
        Ok(bytes) => match pe::check_ident(&bytes, &TH09_V150A) {
            Ok(()) => Check::Ok,
            Err(m) => Check::NotV150a(m.fields, ident_details(exe, &bytes, h.lang())),
        },
    }
}

/// 最初の行を trim する（BOM も外す）。空・相対パスは None（相対は起動した場所で指す先が変わる）。
pub fn parse_saved(text: &str) -> Option<PathBuf> {
    let line = text.trim_start_matches('\u{feff}').lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let p = PathBuf::from(line);
    p.is_absolute().then_some(p)
}

pub fn saved_text(exe: &Path) -> Option<String> {
    exe.to_str().map(|s| format!("{s}\r\n"))
}

/// 食い違った欄の名前は原因の切り分けに使うのでそのまま出す。
pub fn ident_mismatch_text(fields: &[IdentField], lang: Lang) -> String {
    by_lang!(
        lang,
        ja: "選んだ exe が東方花映塚 ver.1.50a の th09.exe と一致しません。花映塚の th09.exe を選んでいるか（custom.exe や vpatch.exe ではないか）、ver.1.50a の修正パッチを当ててあるかを確かめてください（食い違った欄: {fields:?}）",
        en: "The chosen exe does not match th09.exe of Touhou 9 ver.1.50a. Check that you chose th09.exe of Touhou 9 (not custom.exe or vpatch.exe) and that the ver.1.50a patch is applied (mismatched fields: {fields:?})"
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 実際に読んだファイルと値・期待の値を 3 行で説明する（そのまま送れる形）。
pub fn ident_details(path: &Path, bytes: &[u8], lang: Lang) -> Vec<String> {
    ident_details_against(path, bytes, &TH09_V150A, lang)
}

fn ident_details_against(path: &Path, bytes: &[u8], want: &Ident<'_>, lang: Lang) -> Vec<String> {
    let shown = path.display();
    let n = bytes.len();
    let mut lines = vec![by_lang!(
        lang,
        ja: "読んだファイル: {shown}（{n} バイト）",
        en: "File read: {shown} ({n} bytes)"
    )];
    lines.push(match pe::parse(bytes) {
        Ok(info) => {
            let text = info.section_by_name(".text").map_or_else(
                || match lang {
                    Lang::Ja => "節が無い".to_string(),
                    Lang::En => "no such section".to_string(),
                },
                |s| match lang {
                    Lang::Ja => format!("0x{:08X}（{}）", s.size_of_raw_data, s.size_of_raw_data),
                    Lang::En => format!("0x{:08X} ({})", s.size_of_raw_data, s.size_of_raw_data),
                },
            );
            let entry = info.entry_bytes(bytes, want.entry_bytes.len()).map_or_else(
                || match lang {
                    Lang::Ja => "読めない".to_string(),
                    Lang::En => "unreadable".to_string(),
                },
                hex,
            );
            let stamp = info.time_date_stamp;
            let entry_len = want.entry_bytes.len();
            by_lang!(
                lang,
                ja: "読んだ値: TimeDateStamp 0x{stamp:08X}、.text の SizeOfRawData {text}、入口の先頭 {entry_len} バイト {entry}",
                en: "Value read: TimeDateStamp 0x{stamp:08X}, .text SizeOfRawData {text}, first {entry_len} bytes of the entry {entry}"
            )
        }
        Err(e) => {
            let head_len = n.min(16);
            let head = hex(&bytes[..head_len]);
            by_lang!(
                lang,
                ja: "読んだ値: 32bit の PE として読めません（{e:?}）。ファイルの先頭 {head_len} バイト {head}",
                en: "Value read: not readable as a 32-bit PE ({e:?}). First {head_len} bytes of the file: {head}"
            )
        }
    });
    let (stamp, raw) = (want.time_date_stamp, want.text_size_of_raw_data);
    let (entry_len, entry) = (want.entry_bytes.len(), hex(want.entry_bytes));
    lines.push(by_lang!(
        lang,
        ja: "期待の値: TimeDateStamp 0x{stamp:08X}、.text の SizeOfRawData 0x{raw:08X}（{raw}）、入口の先頭 {entry_len} バイト {entry}",
        en: "Expected: TimeDateStamp 0x{stamp:08X}, .text SizeOfRawData 0x{raw:08X} ({raw}), first {entry_len} bytes of the entry {entry}"
    ));
    lines
}

/// ドライブ名の後に `\` や `/` が無い形（`D:原作\th09.exe`）か。起こした場所で指す先が変わる。
pub fn is_drive_relative(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    drive && (b.len() == 2 || (b[2] != b'\\' && b[2] != b'/'))
}

pub fn drive_relative_message(orig: &Path, lang: Lang) -> String {
    let orig = orig.display();
    by_lang!(
        lang,
        ja: "--game の {orig} はドライブ名の後に \\ がありません。ドライブ名の後に \\ を足してください（例 D:\\Games\\th09\\th09.exe）",
        en: "The --game path {orig} has no \\ after the drive name. Add a \\ after the drive name (e.g. D:\\Games\\th09\\th09.exe)"
    )
}

/// `force_dialog` は `--choose-game`（覚えた場所が使えてもウィンドウを出す）。
pub fn resolve<H: Host>(h: &mut H, force_dialog: bool) -> Result<PathBuf, PickError> {
    let lang = h.lang();
    let saved = h.read_saved().as_deref().and_then(parse_saved);
    let mut initial: Option<PathBuf> = None;
    if let Some(exe) = &saved {
        initial = exe.parent().map(Path::to_path_buf);
        if !force_dialog {
            let shown = exe.display();
            match check_exe(h, exe) {
                Check::Ok => return Ok(exe.clone()),
                Check::Missing => h.say(&by_lang!(
                    lang,
                    ja: "花映塚のフォルダ（{shown}）が見つかりません。フォルダを選び直してください。",
                    en: "The Touhou 9 folder ({shown}) was not found. Choose the folder again."
                )),
                Check::Unreadable(why) => h.say(&by_lang!(
                    lang,
                    ja: "前に選んだ {shown} が読めません（{why}）。花映塚のフォルダを選び直してください。",
                    en: "The previously chosen {shown} cannot be read ({why}). Choose the Touhou 9 folder again."
                )),
                Check::NotV150a(fields, _) => {
                    let mismatch = ident_mismatch_text(&fields, lang);
                    h.say(&by_lang!(
                        lang,
                        ja: "前に選んだ {shown}: {mismatch}",
                        en: "Previously chosen {shown}: {mismatch}"
                    ))
                }
            }
        }
    }
    if !initial.as_deref().is_some_and(|d| h.is_dir(d)) {
        initial = h.standard_dir();
    }

    loop {
        let dir = match h.ask_folder(initial.as_deref()) {
            Answer::Folder(d) => d,
            Answer::Cancelled => return Err(PickError::Cancelled),
            Answer::Failed(why) => return Err(PickError::DialogFailed(why)),
        };
        let exe = dir.join(GAME_EXE);
        match check_exe(h, &exe) {
            Check::Ok => {
                remember(h, &exe);
                return Ok(exe);
            }
            Check::Missing => {
                let shown = dir.display();
                h.warn(&by_lang!(
                    lang,
                    ja: "選んだフォルダに {GAME_EXE} がありません。花映塚の {GAME_EXE} が入っているフォルダを選び直してください。\n選んだフォルダ: {shown}",
                    en: "There is no {GAME_EXE} in the chosen folder. Choose the folder that contains th09.exe of Touhou 9.\nChosen folder: {shown}"
                ))
            }
            Check::Unreadable(why) => {
                let shown = exe.display();
                h.warn(&by_lang!(
                    lang,
                    ja: "{shown} が読めません（{why}）。花映塚のフォルダを選び直してください。",
                    en: "{shown} cannot be read ({why}). Choose the Touhou 9 folder again."
                ))
            }
            Check::NotV150a(fields, details) => h.warn(&format!(
                "{}\n{}",
                ident_mismatch_text(&fields, lang),
                details.join("\n")
            )),
        }
        // 選び直しは、いま選んだフォルダから始める。
        initial = Some(dir);
    }
}

fn remember<H: Host>(h: &mut H, exe: &Path) {
    let lang = h.lang();
    let Some(text) = saved_text(exe) else {
        h.say(match lang {
            Lang::Ja => "選んだ場所を覚えられませんでした（パスが UTF-8 にできません）。次の起動でも選んでください。",
            Lang::En => "Could not remember the chosen location (the path cannot be turned into UTF-8). Choose it again next time.",
        });
        return;
    };
    match h.write_saved(&text) {
        Ok(()) => {
            let shown = exe.display();
            h.say(&by_lang!(
                lang,
                ja: "選んだ場所を覚えました: {shown}",
                en: "Remembered the chosen location: {shown}"
            ))
        }
        Err(why) => h.say(&by_lang!(
            lang,
            ja: "選んだ場所を覚えられませんでした（{why}）。次の起動でも選んでください。",
            en: "Could not remember the chosen location ({why}). Choose it again next time."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lang::has_japanese;
    use std::collections::{HashMap, VecDeque};

    /// 偽のウィンドウ。答えは台本（`answers`）、尽きたら取り消し。
    #[derive(Default)]
    struct Fake {
        lang: Lang,
        saved: Option<String>,
        write_fails: bool,
        files: HashMap<PathBuf, Vec<u8>>,
        unreadable: Vec<PathBuf>,
        dirs: Vec<PathBuf>,
        standard: Option<PathBuf>,
        answers: VecDeque<Answer>,
        asked_initial: Vec<Option<PathBuf>>,
        said: Vec<String>,
        warned: Vec<String>,
        written: Vec<String>,
    }

    impl Host for Fake {
        fn lang(&self) -> Lang {
            self.lang
        }
        fn read_saved(&mut self) -> Option<String> {
            self.saved.clone()
        }
        fn write_saved(&mut self, text: &str) -> Result<(), String> {
            if self.write_fails {
                return Err("write refused".into());
            }
            self.written.push(text.to_string());
            self.saved = Some(text.to_string());
            Ok(())
        }
        fn read_exe(&mut self, exe: &Path) -> io::Result<Vec<u8>> {
            if self.unreadable.iter().any(|p| p == exe) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "拒否"));
            }
            self.files
                .get(exe)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn is_dir(&mut self, dir: &Path) -> bool {
            self.dirs.iter().any(|d| d == dir)
        }
        fn standard_dir(&mut self) -> Option<PathBuf> {
            self.standard.clone()
        }
        fn ask_folder(&mut self, initial: Option<&Path>) -> Answer {
            self.asked_initial.push(initial.map(Path::to_path_buf));
            self.answers.pop_front().unwrap_or(Answer::Cancelled)
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn warn(&mut self, text: &str) {
            self.warned.push(text.to_string());
        }
    }

    fn good_exe() -> Vec<u8> {
        synth_exe(
            TH09_V150A.time_date_stamp,
            TH09_V150A.text_size_of_raw_data,
            TH09_V150A.entry_bytes,
        )
    }

    fn synth_exe(stamp: u32, text_raw: u32, entry: &[u8]) -> Vec<u8> {
        let e_lfanew = 0x40usize;
        let fh = e_lfanew + 4;
        let oh = fh + 20;
        let opt_size = 224usize;
        let sec = oh + opt_size;
        let raw_off = 0x200usize;
        let mut b = vec![0u8; raw_off + 0x40];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3C..0x40].copy_from_slice(&(e_lfanew as u32).to_le_bytes());
        b[e_lfanew..e_lfanew + 4].copy_from_slice(b"PE\0\0");
        b[fh..fh + 2].copy_from_slice(&0x014Cu16.to_le_bytes());
        b[fh + 2..fh + 4].copy_from_slice(&1u16.to_le_bytes());
        b[fh + 4..fh + 8].copy_from_slice(&stamp.to_le_bytes());
        b[fh + 16..fh + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());
        b[oh..oh + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
        b[oh + 16..oh + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        b[oh + 28..oh + 32].copy_from_slice(&0x0040_0000u32.to_le_bytes());
        b[sec..sec + 5].copy_from_slice(b".text");
        b[sec + 8..sec + 12].copy_from_slice(&0x1000u32.to_le_bytes()); // virtual_size
        b[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // virtual_address
        b[sec + 16..sec + 20].copy_from_slice(&text_raw.to_le_bytes());
        b[sec + 20..sec + 24].copy_from_slice(&(raw_off as u32).to_le_bytes());
        // 節の実体がファイルの範囲に収まらない大きさでも、入口の位置の読み出しは範囲内。
        b[raw_off..raw_off + entry.len()].copy_from_slice(entry);
        b
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn with_exe(dir: &str, bytes: Vec<u8>) -> (Fake, PathBuf) {
        let mut f = Fake::default();
        let exe = p(dir).join(GAME_EXE);
        f.files.insert(exe.clone(), bytes);
        (f, exe)
    }

    #[test]
    fn the_synthetic_exe_is_accepted_and_a_changed_one_is_not() {
        assert!(pe::check_ident(&good_exe(), &TH09_V150A).is_ok());
        let bad = synth_exe(TH09_V150A.time_date_stamp + 1, 0x1000, &[0u8; 20]);
        let m = pe::check_ident(&bad, &TH09_V150A).unwrap_err();
        assert_eq!(m.fields.len(), 3);
    }

    #[test]
    fn a_remembered_good_exe_starts_without_a_window() {
        let (mut f, exe) = with_exe("X:\\g", good_exe());
        f.saved = Some(format!("{}\r\n", exe.display()));
        assert_eq!(resolve(&mut f, false), Ok(exe));
        assert!(f.asked_initial.is_empty(), "ウィンドウを出した");
        assert!(f.written.is_empty(), "書き直した");
    }

    #[test]
    fn choose_game_asks_even_when_the_remembered_exe_is_good() {
        let (mut f, exe) = with_exe("X:\\g", good_exe());
        f.saved = Some(format!("{}\n", exe.display()));
        f.dirs.push(p("X:\\g"));
        f.answers.push_back(Answer::Cancelled);
        assert_eq!(resolve(&mut f, true), Err(PickError::Cancelled));
        assert_eq!(f.asked_initial, vec![Some(p("X:\\g"))]);
        assert!(f.written.is_empty());
    }

    #[test]
    fn a_missing_or_unusable_saved_file_falls_to_the_window() {
        for saved in [
            None,
            Some("".to_string()),
            Some("   \r\n".to_string()),
            Some("th09.exe\r\n".to_string()),
            Some("..\\g\\th09.exe".to_string()),
        ] {
            let mut f = Fake {
                saved: saved.clone(),
                ..Fake::default()
            };
            assert_eq!(
                resolve(&mut f, false),
                Err(PickError::Cancelled),
                "{saved:?}"
            );
            assert_eq!(f.asked_initial.len(), 1, "{saved:?}");
        }
        assert_eq!(
            parse_saved("\u{feff}C:\\a b\\th09.exe\r\nゴミ\r\n"),
            Some(p("C:\\a b\\th09.exe"))
        );
    }

    #[test]
    fn a_stale_remembered_exe_asks_again_and_says_why() {
        let mut f = Fake {
            saved: Some("X:\\gone\\th09.exe".into()),
            ..Fake::default()
        };
        assert_eq!(resolve(&mut f, false), Err(PickError::Cancelled));
        assert_eq!(f.asked_initial.len(), 1);
        assert!(f.said[0].contains("見つかりません"), "{:?}", f.said);
        let mut f = Fake {
            saved: Some("X:\\g\\th09.exe".into()),
            ..Fake::default()
        };
        f.files
            .insert(p("X:\\g\\th09.exe"), synth_exe(1, 0x1000, &[0u8; 20]));
        assert_eq!(resolve(&mut f, false), Err(PickError::Cancelled));
        assert!(f.said[0].contains("一致しません"), "{:?}", f.said);
        let mut f = Fake {
            saved: Some("X:\\g\\th09.exe".into()),
            ..Fake::default()
        };
        f.unreadable.push(p("X:\\g\\th09.exe"));
        assert_eq!(resolve(&mut f, false), Err(PickError::Cancelled));
        assert!(f.said[0].contains("読めません"), "{:?}", f.said);
    }

    #[test]
    fn the_first_pick_is_remembered_as_the_full_exe_path() {
        let (mut f, exe) = with_exe("X:\\game", good_exe());
        f.answers.push_back(Answer::Folder(p("X:\\game")));
        assert_eq!(resolve(&mut f, false), Ok(exe.clone()));
        assert_eq!(f.written, vec![format!("{}\r\n", exe.display())]);
        assert_eq!(parse_saved(&f.written[0]), Some(exe));
        assert!(f.said.iter().any(|l| l.contains("覚えました")));
    }

    #[test]
    fn the_window_opens_at_the_remembered_folder_then_the_standard_one() {
        let ask = |saved: Option<&str>, dirs: &[&str], standard: Option<&str>| {
            let mut f = Fake {
                saved: saved.map(String::from),
                dirs: dirs.iter().map(|d| p(d)).collect(),
                standard: standard.map(p),
                ..Fake::default()
            };
            let _ = resolve(&mut f, true);
            f.asked_initial.remove(0)
        };
        assert_eq!(ask(None, &[], Some("S:\\std")), Some(p("S:\\std")));
        assert_eq!(ask(None, &[], None), None);
        let saved = Some("X:\\g\\th09.exe");
        assert_eq!(ask(saved, &["X:\\g"], Some("S:\\std")), Some(p("X:\\g")));
        assert_eq!(ask(saved, &[], Some("S:\\std")), Some(p("S:\\std")));
        assert_eq!(ask(saved, &[], None), None);
    }

    #[test]
    fn a_folder_without_the_exe_is_refused_and_asked_again() {
        let (mut f, exe) = with_exe("X:\\good", good_exe());
        f.answers.push_back(Answer::Folder(p("X:\\empty")));
        f.answers.push_back(Answer::Folder(p("X:\\good")));
        assert_eq!(resolve(&mut f, false), Ok(exe));
        assert_eq!(f.warned.len(), 1);
        assert!(f.warned[0].contains("選んだフォルダに th09.exe がありません"));
        assert_eq!(f.asked_initial.len(), 2);
        assert_eq!(f.asked_initial[1], Some(p("X:\\empty")));
        assert_eq!(f.written.len(), 1, "失敗した選びを覚えた");
        assert!(f.written[0].contains("good"));
    }

    #[test]
    fn a_non_150a_exe_is_refused_with_the_values_and_asked_again() {
        let mut f = Fake::default();
        f.files.insert(
            p("X:\\old\\th09.exe"),
            synth_exe(0x1234_5678, 0x2000, &[0xAA; 20]),
        );
        f.answers.push_back(Answer::Folder(p("X:\\old")));
        assert_eq!(resolve(&mut f, false), Err(PickError::Cancelled));
        assert_eq!(
            f.asked_initial.len(),
            2,
            "選び直しのウィンドウを出さなかった"
        );
        let w = &f.warned[0];
        for want in [
            "一致しません",
            "custom.exe",
            "vpatch.exe",
            "修正パッチ",
            "TimeDateStamp",
            "X:\\old\\th09.exe",
            "バイト）",
            "0x12345678",
            "0x00002000",
            "AA AA AA AA",
            "0x4346B9BB",
            "0x0008CC00",
            "6A 60 68 A8",
        ] {
            assert!(w.contains(want), "{want} が無い: {w}");
        }
        assert!(f.written.is_empty(), "通らなかった場所を覚えた");
    }

    #[test]
    fn cancel_and_dialog_failure_end_without_writing() {
        let mut f = Fake::default();
        f.answers.push_back(Answer::Cancelled);
        let e = resolve(&mut f, false).unwrap_err();
        assert_eq!(e, PickError::Cancelled);
        assert!(f.written.is_empty());
        assert!(e.describe(Lang::Ja).contains("何も起動していません"));
        assert!(e.describe(Lang::Ja).contains("もう一度"));

        let mut f = Fake::default();
        f.answers.push_back(Answer::Failed("0x80004005".into()));
        let e = resolve(&mut f, false).unwrap_err();
        assert!(matches!(&e, PickError::DialogFailed(w) if w == "0x80004005"));
        assert!(e.describe(Lang::Ja).contains("0x80004005"));
        assert!(f.written.is_empty());
    }

    #[test]
    fn a_write_failure_does_not_stop_the_launch_but_is_said() {
        let (mut f, exe) = with_exe("X:\\game", good_exe());
        f.write_fails = true;
        f.answers.push_back(Answer::Folder(p("X:\\game")));
        assert_eq!(resolve(&mut f, false), Ok(exe));
        assert!(
            f.said.iter().any(|l| l.contains("覚えられませんでした")),
            "{:?}",
            f.said
        );
    }

    #[test]
    fn the_mismatch_text_says_what_to_check_and_which_fields() {
        let t = ident_mismatch_text(
            &[IdentField::TimeDateStamp, IdentField::EntryBytes],
            Lang::Ja,
        );
        assert!(t.contains("custom.exe") && t.contains("vpatch.exe") && t.contains("修正パッチ"));
        assert!(t.contains("TimeDateStamp") && t.contains("EntryBytes"));
        assert!(!t.contains("TextSizeOfRawData"));
    }

    #[test]
    fn details_of_a_non_pe_file_show_the_reason_and_the_head_bytes() {
        let d = ident_details(Path::new("X:\\a\\th09.exe"), b"<html>", Lang::Ja);
        assert_eq!(d.len(), 3);
        assert!(d[0].contains("6 バイト"), "{d:?}");
        assert!(d[1].contains("TooShortForDosHeader") && d[1].contains("3C 68 74 6D 6C 3E"));
        assert!(d[2].starts_with("期待の値"));
        let d = ident_details(Path::new("X:\\a\\th09.exe"), &[], Lang::Ja);
        assert!(d[1].contains("0 バイト"), "{d:?}");
    }

    #[test]
    fn english_run_says_every_pick_message_in_english() {
        // 覚えた場所が消えている → 選び直し → フォルダに th09.exe が無い → 1.50a でない → 覚えられない、を英語で通す。
        let mut f = Fake {
            lang: Lang::En,
            saved: Some("X:\\gone\\th09.exe".into()),
            write_fails: true,
            ..Fake::default()
        };
        f.files.insert(
            p("X:\\old\\th09.exe"),
            synth_exe(0x1234_5678, 0x2000, &[0xAA; 20]),
        );
        f.files.insert(p("X:\\game\\th09.exe"), good_exe());
        f.answers.push_back(Answer::Folder(p("X:\\empty")));
        f.answers.push_back(Answer::Folder(p("X:\\old")));
        f.answers.push_back(Answer::Folder(p("X:\\game")));
        assert_eq!(resolve(&mut f, false), Ok(p("X:\\game\\th09.exe")));
        assert_eq!(f.warned.len(), 2);
        assert!(f.warned[0].contains("There is no th09.exe in the chosen folder"));
        assert!(f.warned[1].contains("custom.exe") && f.warned[1].contains("Expected:"));
        assert!(f.said[0].contains("was not found"), "{:?}", f.said);
        assert!(f.said.iter().any(|l| l.contains("Could not remember")));
        for l in f.said.iter().chain(&f.warned) {
            assert!(!has_japanese(l), "英語の回に日本語: {l}");
        }
        for e in [PickError::Cancelled, PickError::DialogFailed("0x1".into())] {
            assert!(!has_japanese(&e.describe(Lang::En)), "{e:?}");
        }
        let m = drive_relative_message(Path::new("D:x\\th09.exe"), Lang::En);
        assert!(
            m.contains("Add a \\ after the drive name") && !has_japanese(&m),
            "{m}"
        );
        // 英語と日本語で、値（アドレス・長さ）の欄は同じ数だけ出る。
        let ja = ident_details(Path::new("X:\\a\\th09.exe"), &good_exe(), Lang::Ja);
        let en = ident_details(Path::new("X:\\a\\th09.exe"), &good_exe(), Lang::En);
        assert_eq!(ja.len(), en.len());
        assert!(en.iter().all(|l| !has_japanese(l)), "{en:?}");
    }

    #[test]
    fn drive_relative_paths_are_detected() {
        for yes in ["D:原作\\th09.exe", "d:th09.exe", "D:", "C:..\\x"] {
            assert!(is_drive_relative(Path::new(yes)), "{yes}");
        }
        for no in [
            "D:\\原作\\th09.exe",
            "D:/g/th09.exe",
            "\\\\srv\\share\\th09.exe",
            "th09.exe",
            ".\\th09.exe",
            "\\th09.exe",
            "D:\\",
            "",
        ] {
            assert!(!is_drive_relative(Path::new(no)), "{no}");
        }
        let m = drive_relative_message(Path::new("D:原作\\th09.exe"), Lang::Ja);
        assert!(m.contains("D:原作\\th09.exe") && m.contains("ドライブ名の後に \\ を足して"));
        assert!(m.contains("例 D:\\"), "{m}");
    }
}
