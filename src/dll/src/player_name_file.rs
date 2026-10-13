//! メニューで決めたプレイヤー名を覚えるファイル（unsafe 無し）。
//!
//! 置き場は接続コードの置き場（`code_dir`）の `player_name.txt`。ゲームのフォルダには書かない。同じ PC で 2 つ起こす回は
//! ゲームごとに `code_dir` が分かれるのでファイルも分かれる。
//!
//! - 読むのは起動のとき（`session::load_net` → `ScriptHost::saved_name`）だけ。ファイルがあれば ini の `name` より優先する。
//! - 書くのはメニューで名前を決めたとき、別のスレッド（`script_win::save_player_name`）が書く。ゲームのスレッドは読み書きしない。
//!   一時ファイルへ書いてから置き換える（`std::fs::rename` は Windows で既存のファイルを置き換える）。
//! - 読めない・壊れている・大きすぎるファイルは無視する（`player_name::parse_file`）。UTF-8 の 1 行。日本語の名前も通す。

use std::io::Read;
use std::path::{Path, PathBuf};

use higanr_core::player_name::{NameBuf, PLAYER_NAME_FILE, file_text, parse_file};

/// 読む大きさの上限（B）。名前は 32 B なので十分。
const READ_MAX: u64 = 1024;

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(PLAYER_NAME_FILE)
}

/// 覚えた名前を読む。無い・読めない・壊れている・大きすぎるなら `None`。
pub fn read(dir: &Path) -> Option<String> {
    let f = std::fs::File::open(path_in(dir)).ok()?;
    let mut bytes = Vec::new();
    // 上限より 1 バイト多く読んで、越えていたら捨てる。
    f.take(READ_MAX + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > READ_MAX {
        return None;
    }
    parse_file(&String::from_utf8(bytes).ok()?)
}

/// 誤りの文はパスだけで、名前を含めない。
pub fn write(dir: &Path, name: &NameBuf) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("置き場 {} を作れない: {e}", dir.display()))?;
    let path = path_in(dir);
    let tmp = dir.join(format!("{PLAYER_NAME_FILE}.tmp"));
    std::fs::write(&tmp, file_text(name))
        .map_err(|e| format!("{} を書けない: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{} へ置き換えられない: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!(
                "higanr_player_name_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn buf(s: &str) -> NameBuf {
        NameBuf::from_name(s).unwrap()
    }

    #[test]
    fn a_written_name_reads_back_and_replaces_the_old_one() {
        let t = Tmp::new("rt");
        assert_eq!(read(&t.0), None, "ファイルが無いのに名前が出た");
        write(&t.0, &buf("Synth-One")).unwrap();
        assert_eq!(read(&t.0).as_deref(), Some("Synth-One"));
        write(&t.0, &buf("second_name")).unwrap();
        assert_eq!(
            read(&t.0).as_deref(),
            Some("second_name"),
            "古い名前が残った"
        );
        let names: Vec<String> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![PLAYER_NAME_FILE.to_string()],
            "一時ファイルが残った"
        );
        std::fs::write(t.0.join(format!("{PLAYER_NAME_FILE}.tmp")), "半端な").unwrap();
        write(&t.0, &buf("third")).unwrap();
        assert_eq!(read(&t.0).as_deref(), Some("third"));
        let nested = t.0.join("not_yet").join("code");
        write(&nested, &buf("made")).unwrap();
        assert_eq!(read(&nested).as_deref(), Some("made"));
    }

    #[test]
    fn a_broken_file_is_ignored_and_a_write_failure_never_names_the_player() {
        let t = Tmp::new("bad");
        let path = path_in(&t.0);
        for bytes in [
            &b""[..],
            b"\n",
            b"   \n",
            b"a\x07b\n",
            b"\xFF\xFE\x00",
            &[b'a'; 2000][..],
            b"\n\nabc\n",
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(read(&t.0), None, "{bytes:?}");
        }
        std::fs::write(&path, b"  ok-name \r\n").unwrap();
        assert_eq!(read(&t.0).as_deref(), Some("ok-name"));
        std::fs::write(&path, "\u{FEFF}テスト\n".as_bytes()).unwrap();
        assert_eq!(read(&t.0).as_deref(), Some("テスト"));
        std::fs::write(&path, format!("{}\n", "a".repeat(32))).unwrap();
        assert_eq!(read(&t.0).as_deref(), Some("a".repeat(32).as_str()));
        let blocker = t.0.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let err = write(&blocker, &buf("secret-name")).unwrap_err();
        assert!(
            !err.contains("secret-name"),
            "誤りの文に名前が入った: {err}"
        );
    }
}
