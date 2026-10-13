//! 接続コードの受け渡しの入口（ゲームの中のメニュー `code_side = game`）: クリップボードの型と、開発用のファイル版・
//! 検査用の偽物（pure。unsafe 無し）。
//!
//! 見張りのスレッド（`net_code`）がメニューの指示で呼ぶ（ゲームのスレッドは触らない）。本物は
//! `clipboard_win::SystemClipboard`（unsafe はそこ）、開発用は `FileClipboard`（ini の `clipboard = file`。置き場の
//! `clip_out.txt` / `clip_in.txt`）、検査は `FakeClipboard`。検査と実機の回は利用者のクリップボードを書き換えてはいけないので、
//! 本物のクリップボードを呼ぶ検査は書かない。
#![forbid(unsafe_code)]

use std::io::{ErrorKind, Read};
use std::path::PathBuf;

use higanr_core::code_files::{CLIP_IN_FILE, CLIP_OUT_FILE, MAX_FILE_BYTES};

/// 読む文字の上限（UTF-16 の単位の数 = 文字数）。これより後ろは読まず、読んだ分だけで `code` が「長すぎる」と断る。
/// 大きなクリップボードを丸ごとコピーしないため。
pub const MAX_READ_CHARS: usize = MAX_FILE_BYTES as usize;

/// クリップボード（に見えるインターフェース）。見張りのスレッドから 1 回ずつ呼ぶ。
pub trait Clipboard: Send {
    /// 文字を読む。文字が無い・空なら `Ok(None)`（クリップボードが空、か文字以外の中身）。`MAX_READ_CHARS` 文字まで。
    fn read(&mut self) -> Result<Option<String>, String>;
    fn write(&mut self, text: &str) -> Result<(), String>;
}

pub struct FileClipboard {
    dir: PathBuf,
}

impl FileClipboard {
    pub fn new(dir: PathBuf) -> FileClipboard {
        FileClipboard { dir }
    }
}

impl Clipboard for FileClipboard {
    fn read(&mut self) -> Result<Option<String>, String> {
        let p = self.dir.join(CLIP_IN_FILE);
        let f = match std::fs::File::open(&p) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{} を開けない: {e}", p.display())),
        };
        let mut b = Vec::new();
        f.take(MAX_FILE_BYTES)
            .read_to_end(&mut b)
            .map_err(|e| format!("{} を読めない: {e}", p.display()))?;
        if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
            return Err(format!("{} が UTF-16 で保存されている", p.display()));
        }
        let text = String::from_utf8(b).map_err(|_| format!("{} が UTF-8 でない", p.display()))?;
        let text = text.trim_start_matches('\u{FEFF}');
        Ok((!text.trim().is_empty()).then(|| text.to_string()))
    }

    fn write(&mut self, text: &str) -> Result<(), String> {
        crate::net_code::replace_file(&self.dir, CLIP_OUT_FILE, text)
    }
}

/// 検査用の偽のクリップボード（中身は共有の入れ物。呼ばれた書き込みを記録する）。
#[cfg(test)]
pub mod fake {
    use super::Clipboard;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    pub struct Inner {
        pub text: Option<String>,
        pub writes: Vec<String>,
        /// 真なら `read` / `write` が失敗する。
        pub fail_read: bool,
        pub fail_write: bool,
        /// 呼ばれた `read` の数（クリップボードを読まないはずの経路が読んでいないかを見る）。
        pub reads: u32,
    }

    #[derive(Debug, Clone, Default)]
    pub struct FakeClipboard(pub Arc<Mutex<Inner>>);

    impl FakeClipboard {
        pub fn set(&self, text: &str) {
            self.0.lock().unwrap().text = Some(text.to_string());
        }

        pub fn text(&self) -> Option<String> {
            self.0.lock().unwrap().text.clone()
        }

        pub fn writes(&self) -> Vec<String> {
            self.0.lock().unwrap().writes.clone()
        }
    }

    impl Clipboard for FakeClipboard {
        fn read(&mut self) -> Result<Option<String>, String> {
            let mut g = self.0.lock().unwrap();
            g.reads += 1;
            if g.fail_read {
                return Err("偽のクリップボードを読めない".to_string());
            }
            Ok(g.text.clone().filter(|t| !t.is_empty()))
        }

        fn write(&mut self, text: &str) -> Result<(), String> {
            let mut g = self.0.lock().unwrap();
            if g.fail_write {
                return Err("偽のクリップボードへコピーできない".to_string());
            }
            g.writes.push(text.to_string());
            g.text = Some(text.to_string());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("higanr_clip_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_file_clipboard_writes_clip_out_and_reads_clip_in() {
        let dir = temp_dir("file");
        let mut c = FileClipboard::new(dir.clone());
        assert_eq!(c.read(), Ok(None), "ファイルが無い");
        c.write("abc-DEF_123").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("clip_out.txt")).unwrap(),
            "abc-DEF_123"
        );
        assert_eq!(
            c.read(),
            Ok(None),
            "自分がコピーした文字は読まない（読むのは clip_in.txt）"
        );
        std::fs::write(dir.join("clip_in.txt"), "\u{FEFF}peer-code\r\n").unwrap();
        assert_eq!(
            c.read(),
            Ok(Some("peer-code\r\n".to_string())),
            "BOM は落とす"
        );
        std::fs::write(dir.join("clip_in.txt"), "  \r\n").unwrap();
        assert_eq!(c.read(), Ok(None), "空白だけ");
        std::fs::write(dir.join("clip_in.txt"), [0xFF, 0xFE, b'a', 0]).unwrap();
        assert!(c.read().unwrap_err().contains("UTF-16"));
        std::fs::write(dir.join("clip_in.txt"), [0xC3, 0x28]).unwrap();
        assert!(c.read().unwrap_err().contains("UTF-8"));
        std::fs::write(dir.join("clip_in.txt"), vec![b'x'; 10_000]).unwrap();
        assert_eq!(c.read().unwrap().unwrap().len(), MAX_READ_CHARS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fake_clipboard_remembers_and_can_fail() {
        let f = fake::FakeClipboard::default();
        let mut c = f.clone();
        assert_eq!(c.read(), Ok(None));
        c.write("one").unwrap();
        assert_eq!(f.text().as_deref(), Some("one"));
        assert_eq!(c.read(), Ok(Some("one".to_string())));
        f.0.lock().unwrap().fail_write = true;
        assert!(c.write("two").is_err());
        assert_eq!(f.writes(), vec!["one".to_string()]);
        f.0.lock().unwrap().fail_read = true;
        assert!(c.read().is_err());
    }
}
