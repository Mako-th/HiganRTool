//! 前に使ったルーム名を覚えるファイル（ルーム名の入力の初めの値。pure。unsafe 無し）。
//!
//! 置き場は接続コードの置き場（`code_dir`。既定は DLL のフォルダの `code\`。ゲームのフォルダの中なら `script_win` が開かない）の
//! `room_last.txt`（`room_input::ROOM_LAST_FILE`）。同じ PC で花映塚を 2 つ起こす回は、ゲームごとに `code_dir` を分けてあるのでファイルも分かれる。
//!
//! - 読むのは起動のとき（`script_win::arm_game_ui`。読み取りのスレッド）だけ。ゲームのスレッドは読まない。
//! - 書くのは見張りのスレッド（`net_code_game`）がルーム名を受けたとき。ゲームのスレッドは書かない（ブロッキング I/O を入れない）。
//!   書きかけを読まれないよう同じフォルダの一時ファイルへ書いてから置き換え（`std::fs::rename` は Windows で既にあるファイルを置き換える）、
//!   一時ファイルを 2 本のスレッドが取り合わないよう、書く経路はすべて鍵（`WRITE_LOCK`）の中を通る。
//! - 読めない・壊れている・大きすぎるファイルは無視して空から始める（`room_input::parse_last`）。
//! - ルーム名は本ログに書かないので、誤りの文は名を含めない（パスだけ）。

use std::io::Read;
use std::path::{Path, PathBuf};

use higanr_core::room_input::{ROOM_LAST_FILE, RoomBuf, last_text, parse_last};

/// 読む大きさの上限（バイト）。ルーム名は 32 文字なので十分で、越えるファイルはルーム名のファイルではない。
const READ_MAX: u64 = 1024;

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(ROOM_LAST_FILE)
}

pub fn read(dir: &Path) -> Option<RoomBuf> {
    let f = std::fs::File::open(path_in(dir)).ok()?;
    let mut bytes = Vec::new();
    f.take(READ_MAX + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > READ_MAX {
        return None;
    }
    parse_last(&String::from_utf8(bytes).ok()?)
}

/// 書く仕事の鍵。一時ファイル（`room_last.txt.tmp`）は 1 つの名前なので、2 本のスレッドが同時に書くと片方の書き込みがもう片方の一時ファイルを壊し、
/// 置き換え（rename）が「無い」で失敗する。書く経路は見張りのスレッドの `remember_room` と `script_win::save_room_name` の 2 つで、
/// どちらも `write` / `write_if_latest` を通るので、鍵はここに 1 つ置く。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn write(dir: &Path, name: &RoomBuf) -> Result<(), String> {
    write_if_latest(dir, name, || true)
}

/// `is_latest` が真のときだけ覚える。確かめるのは鍵の中なので、確かめた後に別の回が書いて古い名で上書きする隙間が無い。
/// 真でなければ何も書かずに `Ok`（新しい名がもうある）。
pub fn write_if_latest(
    dir: &Path,
    name: &RoomBuf,
    is_latest: impl FnOnce() -> bool,
) -> Result<(), String> {
    // 鍵を持つ者が途中で落ちても中身は次の書きで作り直すので、毒を無視して進む。
    let _g = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !is_latest() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("置き場 {} を作れない: {e}", dir.display()))?;
    let path = path_in(dir);
    let tmp = dir.join(format!("{ROOM_LAST_FILE}.tmp"));
    std::fs::write(&tmp, last_text(name))
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
                "higanr_room_last_{tag}_{}_{}",
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

    fn buf(s: &str) -> RoomBuf {
        RoomBuf::from_name(s).unwrap()
    }

    #[test]
    fn a_written_name_reads_back_and_replaces_the_old_one() {
        let t = Tmp::new("rt");
        assert_eq!(read(&t.0), None, "ファイルが無いのに名が出た");
        write(&t.0, &buf("Room-1")).unwrap();
        assert_eq!(read(&t.0), Some(buf("Room-1")));
        write(&t.0, &buf("second_room")).unwrap();
        assert_eq!(read(&t.0), Some(buf("second_room")), "古い名が残った");
        let names: Vec<String> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![ROOM_LAST_FILE.to_string()],
            "一時ファイルが残った"
        );
        std::fs::write(t.0.join(format!("{ROOM_LAST_FILE}.tmp")), "半端な").unwrap();
        write(&t.0, &buf("third")).unwrap();
        assert_eq!(read(&t.0), Some(buf("third")));
        let nested = t.0.join("not_yet").join("code");
        write(&nested, &buf("made")).unwrap();
        assert_eq!(read(&nested), Some(buf("made")));
    }

    #[test]
    fn a_broken_file_is_ignored_and_a_write_failure_never_names_the_room() {
        let t = Tmp::new("bad");
        let path = path_in(&t.0);
        for bytes in [
            &b""[..],
            b"\n",
            b"a b\n",
            "部屋\n".as_bytes(),
            b"\xFF\xFE\x00",
            b"..\n",
            &[b'a'; 2000][..],
            b"\n\nabc\n",
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(read(&t.0), None, "{bytes:?}");
        }
        std::fs::write(&path, b"  ok-name \r\n").unwrap();
        assert_eq!(read(&t.0), Some(buf("ok-name")));
        let long = format!("{}\n", "a".repeat(32));
        std::fs::write(&path, long).unwrap();
        assert_eq!(read(&t.0), Some(buf(&"a".repeat(32))));
        let blocker = t.0.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let err = write(&blocker, &buf("secret-room")).unwrap_err();
        assert!(
            !err.contains("secret-room"),
            "誤りの文にルーム名が入った: {err}"
        );
    }

    /// 300 回ずつ書く（取り合いは数十回に 1 回ほどで起きうる）。
    #[test]
    fn two_threads_writing_at_once_both_succeed_and_leave_one_of_the_names() {
        let t = Tmp::new("race");
        let dir = t.0.clone();
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["alpha-room", "beta-room"]
            .into_iter()
            .map(|s| {
                let (dir, start) = (dir.clone(), start.clone());
                std::thread::spawn(move || {
                    let n = buf(s);
                    start.wait();
                    (0..300)
                        .map(|_| write(&dir, &n))
                        .collect::<Vec<Result<(), String>>>()
                })
            })
            .collect();
        for h in handles {
            for r in h.join().unwrap() {
                assert!(r.is_ok(), "同時に書いて失敗した: {r:?}");
            }
        }
        let got = read(&t.0).expect("書いた名が読めない");
        assert!(
            got == buf("alpha-room") || got == buf("beta-room"),
            "書いていない名が残った"
        );
        let names: Vec<String> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![ROOM_LAST_FILE.to_string()],
            "一時ファイルが残った"
        );
    }

    #[test]
    fn a_stale_name_is_not_written_when_a_newer_one_exists() {
        let t = Tmp::new("latest");
        write(&t.0, &buf("kept")).unwrap();
        assert_eq!(write_if_latest(&t.0, &buf("stale"), || false), Ok(()));
        assert_eq!(read(&t.0), Some(buf("kept")), "古い名が上書きした");
        write_if_latest(&t.0, &buf("fresh"), || true).unwrap();
        assert_eq!(read(&t.0), Some(buf("fresh")));
    }
}
