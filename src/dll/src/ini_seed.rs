//! 配るバージョンで `HiganRTool.ini` が無いとき、配る ini と同じ中身を DLL のフォルダへ書き出す（unsafe 無し）。
//!
//! 更新は利用者の設定を守るため ini を置き換えない（配布の更新の置き場に ini を入れない）ので、更新の置き場のコピーだけを渡された
//! 環境では ini が無く、ネットワークなしで走ってしまう。書くのは起動のときの 1 回だけ（読み取りのスレッド。ゲームのスレッドの経路には入らない）。
//! 既にある ini は読みも書きもしない。書けなければ、埋めた中身を読み手へ返して動かす（`fallback`）。

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::net_config::Located;

/// 書き出す中身（配る ini の原本と同じ。ローダーが先に作るときと同じ中身にするため core に置く）。
pub fn dist_ini_text() -> String {
    higanr_core::ini_store::dist_ini_text()
}

/// そのビルドで書き出す中身。配るバージョンだけ（開発用のビルドは ini が無ければネットワークなし、のまま）。
pub fn default_for_build(dist: bool) -> Option<String> {
    dist.then(dist_ini_text)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Seed {
    /// 書かない経路（開発用のビルド・ローダーが ini を渡した）。
    Skipped,
    /// 既にある。触らない。
    Present,
    Written,
    /// 書けなかった理由。
    Failed(String),
}

impl Seed {
    /// 本ログに出す 1 行（書かなかったなら `None`）。
    pub fn log_line(&self) -> Option<String> {
        match self {
            Seed::Skipped | Seed::Present => None,
            Seed::Written => {
                Some("ini: HiganRTool.ini が無かったので配布用の既定を書き出した".to_string())
            }
            Seed::Failed(why) => Some(format!(
                "ini: HiganRTool.ini が無く、配布用の既定を書き出せなかった（{why}）。埋めた既定の中身で走る"
            )),
        }
    }
}

/// ini が無く、書き出せなかったときに読み手へ返す中身。
static FALLBACK: OnceLock<String> = OnceLock::new();

pub fn fallback() -> Option<&'static str> {
    FALLBACK.get().map(String::as_str)
}

/// `seed` を呼び、書けなかったら中身を `fallback` に残す（プロセスで 1 回）。
pub fn seed_process(loc: &Located, default: Option<&str>) -> Seed {
    let r = seed(loc, default);
    if let (Seed::Failed(_), Some(text)) = (&r, default) {
        let _ = FALLBACK.set(text.to_string());
    }
    r
}

/// `default` があり、ローダーが ini を渡しておらず（渡された ini が無いのは渡し方の誤り）、置き場に ini が無いときだけ書く。
/// 一時ファイルへ書いてから「あれば移さない」移し方で置くので、同時に起きた別の起動に半端な中身を読ませず、先にあった ini も潰さない。
pub fn seed(loc: &Located, default: Option<&str>) -> Seed {
    let Some(text) = default else {
        return Seed::Skipped;
    };
    if loc.from_env.is_some() {
        return Seed::Skipped;
    }
    match loc.path.try_exists() {
        Ok(true) => return Seed::Present,
        Ok(false) => {}
        Err(e) => return Seed::Failed(format!("{} を確かめられない: {e}", loc.path.display())),
    }
    let tmp = temp_path(&loc.path);
    let wrote = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{} を書けない: {e}", tmp.display()));
    if let Err(why) = wrote {
        let _ = std::fs::remove_file(&tmp);
        return Seed::Failed(why);
    }
    let moved = crate::script_win::move_no_replace(&tmp, &loc.path);
    let _ = std::fs::remove_file(&tmp);
    match moved {
        Ok(()) => Seed::Written,
        // 移す間に別の起動が置いたなら、それが正。
        Err(_) if loc.path.exists() => Seed::Present,
        Err(code) => Seed::Failed(format!(
            "{} へ置けない（GetLastError = {code}）",
            loc.path.display()
        )),
    }
}

fn temp_path(ini: &Path) -> PathBuf {
    let mut name = ini.as_os_str().to_owned();
    name.push(format!(".{}.new", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!(
                "higanr_ini_seed_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }

        fn loc(&self) -> Located {
            Located {
                path: self.0.join(crate::net_config::INI_FILE),
                from_env: None,
            }
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(d: &Path) -> Vec<String> {
        std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// 埋めた中身が、配る ini の原本を make_dist と同じに CRLF へ直したものと 1 バイトずつ同じ。
    #[test]
    fn embedded_text_is_the_dist_ini_with_crlf() {
        let raw = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/dist/HiganRTool.ini"
        ))
        .unwrap();
        let mut want = Vec::new();
        for b in raw.iter().filter(|&&b| b != b'\r') {
            if *b == b'\n' {
                want.push(b'\r');
            }
            want.push(*b);
        }
        let text = dist_ini_text();
        assert_eq!(text.as_bytes(), &want[..]);
        assert!(text.contains("\r\n") && !text.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn only_the_dist_build_has_a_default() {
        assert_eq!(default_for_build(false), None);
        assert_eq!(default_for_build(true), Some(dist_ini_text()));
    }

    #[test]
    fn missing_ini_is_written_and_enables_the_net() {
        let t = Tmp::new("write");
        let text = dist_ini_text();
        assert_eq!(seed(&t.loc(), Some(&text)), Seed::Written);
        assert_eq!(std::fs::read(t.loc().path).unwrap(), text.as_bytes());
        assert_eq!(
            names(&t.0),
            [crate::net_config::INI_FILE],
            "一時ファイルが残った"
        );
        let read = crate::script_win::read_ini_at(&t.loc(), None)
            .unwrap()
            .unwrap();
        let s = crate::net_config::from_text(&read).unwrap();
        assert!(s.is_some(), "書き出した ini でネットワークが開かない");
        assert!(Seed::Written.log_line().unwrap().contains("書き出した"));
    }

    #[test]
    fn existing_ini_is_never_touched() {
        let t = Tmp::new("keep");
        let mine = b"[netplay]\r\nenable = 0\r\n\xE8\xA8\xAD\xE5\xAE\x9A";
        std::fs::write(t.loc().path, mine).unwrap();
        let text = dist_ini_text();
        assert_eq!(seed(&t.loc(), Some(&text)), Seed::Present);
        assert_eq!(std::fs::read(t.loc().path).unwrap(), mine);
        assert_eq!(names(&t.0).len(), 1);
        assert_eq!(Seed::Present.log_line(), None);
    }

    #[test]
    fn a_race_lost_after_the_check_does_not_replace() {
        // 確かめた後に別の起動が置いた ini を、移し方が潰さない。
        let t = Tmp::new("race");
        let tmp = temp_path(&t.loc().path);
        std::fs::write(&tmp, "既定").unwrap();
        std::fs::write(t.loc().path, "先客").unwrap();
        assert!(crate::script_win::move_no_replace(&tmp, &t.loc().path).is_err());
        assert_eq!(std::fs::read_to_string(t.loc().path).unwrap(), "先客");
    }

    #[test]
    fn dev_build_and_loader_ini_do_not_write() {
        let t = Tmp::new("skip");
        assert_eq!(seed(&t.loc(), None), Seed::Skipped);
        assert!(names(&t.0).is_empty(), "開発用のビルドが書いた");
        let from_env = Located {
            from_env: Some("HIGANRTOOL_INI"),
            ..t.loc()
        };
        let text = dist_ini_text();
        assert_eq!(seed(&from_env, Some(&text)), Seed::Skipped);
        assert!(
            names(&t.0).is_empty(),
            "ローダーが渡した ini の場所へ書いた"
        );
        assert_eq!(Seed::Skipped.log_line(), None);
    }

    #[test]
    fn unwritable_folder_returns_the_reason_and_the_embedded_text_runs() {
        let t = Tmp::new("fail");
        // フォルダが無いので一時ファイルを作れない。
        let loc = Located {
            path: t.0.join("none").join(crate::net_config::INI_FILE),
            from_env: None,
        };
        let text = dist_ini_text();
        let r = seed(&loc, Some(&text));
        let Seed::Failed(why) = &r else {
            panic!("{r:?}");
        };
        assert!(why.contains("書けない"), "{why}");
        assert!(r.log_line().unwrap().contains(why.as_str()));
        assert_eq!(crate::script_win::read_ini_at(&loc, None), Ok(None));
        let read = crate::script_win::read_ini_at(&loc, Some(&text)).unwrap();
        assert_eq!(read.as_deref(), Some(text.as_str()));
        assert!(crate::net_config::from_text(&text).unwrap().is_some());
    }
}
