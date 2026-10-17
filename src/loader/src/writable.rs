//! 起動の最初に、ツールのフォルダへ書き込めるかを確かめる（配るバージョンだけ）。
//!
//! Program Files のような管理者しか書けない場所では、更新もログの保存もできない。マニフェストの無い 32bit の exe は
//! Windows が書き込みを `%LOCALAPPDATA%\VirtualStore` へ逃がし、作成は成功して見える（実測では、作ったファイルを開いた
//! ハンドルのパスも VirtualStore を指し、存在の確認は逃がされた側を本来の場所の存在として答えた）。そこで、逃がし先に
//! 同じファイルが現れたかで見分ける。書けなくても起動は止めない。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use higanr_core::lang::Lang;

use crate::ui_lang::by_lang;

/// 知らせを出してから起動を続けるまでの待ち。
pub const NOTICE_SECS: u64 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Writable,
    /// 作れたが、本来のフォルダではなく VirtualStore に作られた。
    Redirected,
    /// 作れなかった（理由は OS の文）。
    Denied(String),
}

/// 本物はファイルとコンソール、検査では偽物を差し込む。
pub trait Site {
    fn probe(&mut self, dir: &Path) -> Probe;
    fn say(&mut self, text: &str);
    fn log(&mut self, line: &str);
    fn wait(&mut self, secs: u64);
}

pub fn notice_text(lang: Lang) -> String {
    by_lang!(
        lang,
        ja: "HiganRTool: このフォルダには書き込めないため、更新とログの保存ができません。ドキュメントなど、書き込めるフォルダへ移してください。",
        en: "HiganRTool: This folder is not writable, so updates and logs cannot be saved. Please move HiganRTool to a folder you can write to, such as Documents."
    )
}

/// 書けなければ知らせて待つ。書けるかを返す。`dist` が偽なら何も試さない（開発用のビルドは作業ツリーの中から動かす）。
/// `in_game_folder` なら、ゲームのフォルダへ一時ファイルでも作らないよう試さない。
pub fn check(
    dist: bool,
    in_game_folder: bool,
    dir: &Path,
    lang: Lang,
    site: &mut dyn Site,
) -> bool {
    if !dist {
        return true;
    }
    if in_game_folder {
        site.log("フォルダへの書き込みの確認: ゲームのフォルダなので試さない");
        return true;
    }
    let why = match site.probe(dir) {
        Probe::Writable => {
            site.log("フォルダへの書き込みの確認: 書ける");
            return true;
        }
        Probe::Redirected => "書き込みが VirtualStore に逃がされた".to_string(),
        Probe::Denied(why) => why,
    };
    site.log(&format!("フォルダへの書き込みの確認: 書けない（{why}）"));
    site.say(&notice_text(lang));
    site.wait(NOTICE_SECS);
    false
}

/// 逃がされたときに作られる場所: `%LOCALAPPDATA%\VirtualStore\` の下に、ドライブ名を除いた元のパスが続く。
pub fn virtual_counterpart(file: &Path, local_app_data: &Path) -> PathBuf {
    let rest: PathBuf = file
        .components()
        .filter(|c| {
            !matches!(
                c,
                std::path::Component::Prefix(_) | std::path::Component::RootDir
            )
        })
        .collect();
    local_app_data.join("VirtualStore").join(rest)
}

/// 本物の試し。一時ファイルを作って消す。
pub struct FsProbe {
    /// `%LOCALAPPDATA%`（読めなければ逃がされたかは見分けられず、作れたら書けるとみなす）。
    pub local_app_data: Option<PathBuf>,
    pub exists: Box<dyn FnMut(&Path) -> bool>,
}

impl FsProbe {
    pub fn real() -> FsProbe {
        FsProbe {
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            exists: Box::new(|p| p.exists()),
        }
    }

    pub fn probe(&mut self, dir: &Path) -> Probe {
        let file = dir.join(format!("HiganRTool_write_test_{}.tmp", std::process::id()));
        let counterpart = self
            .local_app_data
            .as_deref()
            .map(|l| virtual_counterpart(&file, l));
        let existed = counterpart.as_deref().is_some_and(&mut self.exists);
        if let Err(e) = OpenOptions::new().write(true).create_new(true).open(&file) {
            return Probe::Denied(e.to_string());
        }
        let redirected = !existed && counterpart.as_deref().is_some_and(&mut self.exists);
        let _ = std::fs::remove_file(&file);
        if redirected && let Some(c) = &counterpart {
            let _ = std::fs::remove_file(c);
        }
        if redirected {
            Probe::Redirected
        } else {
            Probe::Writable
        }
    }
}

/// 本番: 試しは `FsProbe`、知らせはコンソール、記録は段階ログ。
pub struct RealSite(pub FsProbe);

impl Site for RealSite {
    fn probe(&mut self, dir: &Path) -> Probe {
        self.0.probe(dir)
    }

    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn log(&mut self, line: &str) {
        crate::crash_log::stage(line);
    }

    fn wait(&mut self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lang::has_japanese;

    #[derive(Default)]
    struct Fake {
        result: Option<Probe>,
        probed: Vec<PathBuf>,
        said: Vec<String>,
        logged: Vec<String>,
        waited: Vec<u64>,
    }

    impl Site for Fake {
        fn probe(&mut self, dir: &Path) -> Probe {
            self.probed.push(dir.to_path_buf());
            self.result.clone().expect("結果を決めてある")
        }
        fn say(&mut self, text: &str) {
            self.said.push(text.to_string());
        }
        fn log(&mut self, line: &str) {
            self.logged.push(line.to_string());
        }
        fn wait(&mut self, secs: u64) {
            self.waited.push(secs);
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "HiganRTool_loader_writable_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 壊れたら落ちる: 書けないのに知らせない・待たない・ログに残さない、または書けるのに知らせる。
    #[test]
    fn an_unwritable_folder_is_announced_logged_and_waited_for_then_the_launch_goes_on() {
        let dir = Path::new("C:\\Tool");
        for bad in [Probe::Redirected, Probe::Denied("denied".to_string())] {
            let mut site = Fake {
                result: Some(bad.clone()),
                ..Fake::default()
            };
            assert!(!check(true, false, dir, Lang::Ja, &mut site), "{bad:?}");
            assert_eq!(site.said, [notice_text(Lang::Ja)], "{bad:?}");
            assert_eq!(site.waited, [3], "{bad:?}");
            assert_eq!(site.logged.len(), 1, "{bad:?}");
            assert!(site.logged[0].contains("書けない"), "{:?}", site.logged);
        }
        let mut site = Fake {
            result: Some(Probe::Denied("os error 5".to_string())),
            ..Fake::default()
        };
        check(true, false, dir, Lang::Ja, &mut site);
        assert!(site.logged[0].contains("os error 5"), "{:?}", site.logged);
        let mut site = Fake {
            result: Some(Probe::Redirected),
            ..Fake::default()
        };
        check(true, false, dir, Lang::Ja, &mut site);
        assert!(site.logged[0].contains("VirtualStore"), "{:?}", site.logged);

        let mut ok = Fake {
            result: Some(Probe::Writable),
            ..Fake::default()
        };
        assert!(check(true, false, dir, Lang::Ja, &mut ok));
        assert!(ok.said.is_empty() && ok.waited.is_empty(), "{:?}", ok.said);
        assert_eq!(ok.probed, [dir.to_path_buf()]);
    }

    /// 壊れたら落ちる: 開発用のビルドで試す（作業ツリーの中から動かすときに余計な知らせと待ちが出る）。
    #[test]
    fn a_non_dist_build_does_not_probe() {
        let mut site = Fake {
            result: Some(Probe::Denied("x".to_string())),
            ..Fake::default()
        };
        assert!(check(
            false,
            false,
            Path::new("C:\\Tool"),
            Lang::Ja,
            &mut site
        ));
        assert!(site.probed.is_empty() && site.said.is_empty() && site.waited.is_empty());
    }

    /// 壊れたら落ちる: ゲームのフォルダで一時ファイルを作る。
    #[test]
    fn the_game_folder_is_never_probed() {
        let mut site = Fake {
            result: Some(Probe::Denied("x".to_string())),
            ..Fake::default()
        };
        assert!(check(
            true,
            true,
            Path::new("C:\\Game"),
            Lang::Ja,
            &mut site
        ));
        assert!(site.probed.is_empty() && site.said.is_empty() && site.waited.is_empty());
        assert_eq!(site.logged.len(), 1);
    }

    #[test]
    fn the_notice_is_in_both_languages() {
        let ja = notice_text(Lang::Ja);
        assert!(
            ja.contains("書き込めないため、更新とログの保存ができません"),
            "{ja}"
        );
        assert!(ja.contains("書き込めるフォルダへ移してください。"), "{ja}");
        let en = notice_text(Lang::En);
        assert!(!has_japanese(&en), "{en}");
        assert!(
            en.contains("not writable") && en.contains("Documents"),
            "{en}"
        );
    }

    #[test]
    fn the_counterpart_drops_the_drive_and_keeps_the_rest() {
        let c = virtual_counterpart(
            Path::new("C:\\Program Files (x86)\\Tool\\x.tmp"),
            Path::new("X:\\Home\\u\\AppData\\Local"),
        );
        assert_eq!(
            c,
            Path::new(
                "X:\\Home\\u\\AppData\\Local\\VirtualStore\\Program Files (x86)\\Tool\\x.tmp"
            )
        );
        let c = virtual_counterpart(Path::new("\\\\?\\D:\\a\\b.tmp"), Path::new("C:\\L"));
        assert_eq!(c, Path::new("C:\\L\\VirtualStore\\a\\b.tmp"));
    }

    /// 壊れたら落ちる: 書ける場所で一時ファイルを残す・書けると見なさない、または書けない場所を書けると見なす。
    #[test]
    fn a_real_writable_folder_passes_and_leaves_nothing_behind() {
        let dir = temp_dir("ok");
        let local = temp_dir("ok_local");
        let mut p = FsProbe {
            local_app_data: Some(local.clone()),
            exists: Box::new(|p| p.exists()),
        };
        assert_eq!(p.probe(&dir), Probe::Writable);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "一時ファイルが残った"
        );
        assert_eq!(std::fs::read_dir(&local).unwrap().count(), 0);
        // %LOCALAPPDATA% が読めなくても、作れれば書けるとみなす。
        let mut p = FsProbe {
            local_app_data: None,
            exists: Box::new(|p| p.exists()),
        };
        assert_eq!(p.probe(&dir), Probe::Writable);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&local);
    }

    #[test]
    fn a_folder_that_cannot_hold_a_file_is_denied() {
        let dir = temp_dir("denied");
        let mut p = FsProbe {
            local_app_data: Some(dir.clone()),
            exists: Box::new(|p| p.exists()),
        };
        // フォルダが無い・フォルダの場所にファイルがある。
        assert!(matches!(p.probe(&dir.join("無い")), Probe::Denied(_)));
        let blocker = dir.join("file");
        std::fs::write(&blocker, "x").unwrap();
        assert!(matches!(p.probe(&blocker), Probe::Denied(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 壊れたら落ちる: 作れたのに逃がし先に現れたファイルを見逃す（Program Files で何も知らせない）、または元から逃がし先にあった
    /// ファイルを逃がされたと誤る。
    #[test]
    fn a_file_that_appears_in_the_virtual_store_is_redirected_and_cleaned() {
        let dir = temp_dir("redir");
        let local = temp_dir("redir_local");
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<PathBuf>::new()));
        let seen = std::rc::Rc::clone(&asked);
        let (d, l) = (dir.clone(), local.clone());
        // 作る前は無く、作った後で逃がし先に現れる。
        let mut p = FsProbe {
            local_app_data: Some(local.clone()),
            exists: Box::new(move |q| {
                let mut v = seen.borrow_mut();
                v.push(q.to_path_buf());
                if v.len() == 2 {
                    let name = format!("HiganRTool_write_test_{}.tmp", std::process::id());
                    let want = virtual_counterpart(&d.join(name), &l);
                    std::fs::create_dir_all(want.parent().unwrap()).unwrap();
                    std::fs::write(&want, "x").unwrap();
                }
                q.exists()
            }),
        };
        assert_eq!(p.probe(&dir), Probe::Redirected);
        let asked = asked.borrow();
        assert_eq!(asked.len(), 2);
        assert!(
            asked[0].starts_with(local.join("VirtualStore")),
            "{asked:?}"
        );
        assert!(!asked[1].exists(), "逃がし先のファイルが残った");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        // 逃がし先に元からあった同名のファイルは、逃がしの印にしない。
        let mut p = FsProbe {
            local_app_data: Some(local.clone()),
            exists: Box::new(|_| true),
        };
        assert_eq!(p.probe(&dir), Probe::Writable);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&local);
    }
}
