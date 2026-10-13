//! 接続コードの受け渡しに使うファイルの名前と置き場の決まり。dll の `net_code` とローダーの `code_relay` で同じでなければ
//! ならない（食い違うとローダーは誰も読まない所へ書く）ので、両方から引ける core に置く。
//!
//! - 置き場は DLL のフォルダの `code\`（ini の `code_dir` で変えられる。絶対パスならそのまま、相対なら DLL のフォルダから）。
//! - `my_code.txt` … 自分のコード（dll が書き、繋がったら消す）。
//! - `peer_code.txt` … 相手のコード（人かローダーが貼る。dll は読むだけ）。
//! - `status.txt` … いまの状態（dll が書き直す。ローダーのウィンドウが「状態」の行に出す）。
//! - `mode.txt` … ウィンドウで選んだ対戦の形（ローダーが書き、dll の見張りが読む。対象のゲームの pid つき）。
//! - `clip_out.txt` / `clip_in.txt` … `clipboard = file`（開発用）のときのクリップボードの代わり。dll が `clip_out.txt` へ
//!   「コピーする文字」を書き、`clip_in.txt` から「貼る文字」を読む。
//! - 書き直しは一時の名前（`<名前>.tmp`）に書いてから置き換える（読み手が書きかけを見ない）。
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 置き場の既定（DLL のフォルダから）。
pub const CODE_DIR: &str = "code";
pub const MY_CODE_FILE: &str = "my_code.txt";
pub const PEER_CODE_FILE: &str = "peer_code.txt";
pub const STATUS_FILE: &str = "status.txt";
/// `peer_code.txt` を置き換える前の一時の名前（ローダーが使う）。`tmp_name(PEER_CODE_FILE)` と同じ値（検査が揃いを見る）。
pub const PEER_TMP_FILE: &str = "peer_code.txt.tmp";
/// 中身の `pid` が自分の実行と違えば dll は使わない。
pub const MODE_FILE: &str = "mode.txt";
/// `tmp_name(MODE_FILE)` と同じ値（検査が揃いを見る）。
pub const MODE_TMP_FILE: &str = "mode.txt.tmp";

pub const CLIP_OUT_FILE: &str = "clip_out.txt";
pub const CLIP_IN_FILE: &str = "clip_in.txt";
/// クリップボードの形式 `CF_UNICODETEXT`（UTF-16 の終端 0 つき。`winuser.h` の値 13）。windows-sys の定義は Ole の feature が要るので、ここに置く。
pub const CF_UNICODETEXT: u32 = 13;

/// 相手のファイルを読む間隔。人が貼ってから繋ぎ始めるまでの待ちの上限になる。
pub const POLL: Duration = Duration::from_millis(250);

/// 読むファイルの上限（B）。いちばん長いコードは 354 文字で、BOM と空行を足しても 1 KB に届かない。越えた分は読まない。
pub const MAX_FILE_BYTES: u64 = 4096;

pub fn tmp_name(name: &str) -> String {
    format!("{name}.tmp")
}

/// `setting` が絶対パスならそれ、相対なら `dll_dir` から、無ければ `dll_dir` の `code`。`dll_dir` が分からず相対なら None。
pub fn code_dir(dll_dir: Option<&Path>, setting: Option<&str>) -> Option<PathBuf> {
    match dll_dir {
        Some(d) => Some(code_dir_under(d, setting)),
        None => {
            let p = Path::new(setting.unwrap_or(CODE_DIR));
            p.is_absolute().then(|| p.to_path_buf())
        }
    }
}

/// DLL のフォルダが分かっている形（ローダー。常に決まる）。
pub fn code_dir_under(dll_dir: &Path, setting: Option<&str>) -> PathBuf {
    let p = Path::new(setting.unwrap_or(CODE_DIR));
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dll_dir.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_names_are_fixed_and_do_not_collide() {
        assert_eq!(
            [CODE_DIR, MY_CODE_FILE, PEER_CODE_FILE, STATUS_FILE],
            ["code", "my_code.txt", "peer_code.txt", "status.txt"]
        );
        assert_eq!(tmp_name(PEER_CODE_FILE), PEER_TMP_FILE);
        assert_eq!((MODE_FILE, MODE_TMP_FILE), ("mode.txt", "mode.txt.tmp"));
        assert_eq!(tmp_name(MODE_FILE), MODE_TMP_FILE);
        assert_eq!(
            (CLIP_OUT_FILE, CLIP_IN_FILE),
            ("clip_out.txt", "clip_in.txt")
        );
        let names = [
            MY_CODE_FILE.to_string(),
            PEER_CODE_FILE.to_string(),
            STATUS_FILE.to_string(),
            MODE_FILE.to_string(),
            tmp_name(MY_CODE_FILE),
            tmp_name(STATUS_FILE),
            PEER_TMP_FILE.to_string(),
            MODE_TMP_FILE.to_string(),
            CLIP_OUT_FILE.to_string(),
            CLIP_IN_FILE.to_string(),
            tmp_name(CLIP_OUT_FILE),
        ];
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(POLL, Duration::from_millis(250));
        assert_eq!(MAX_FILE_BYTES, 4096);
    }

    #[test]
    fn code_dir_is_under_the_dll_folder_unless_absolute() {
        let dll = Path::new(r"C:\tool\bin");
        for (setting, want) in [
            (None, r"C:\tool\bin\code"),
            (Some("code_b"), r"C:\tool\bin\code_b"),
            (Some(r"D:\x\codes"), r"D:\x\codes"),
        ] {
            assert_eq!(code_dir(Some(dll), setting), Some(PathBuf::from(want)));
            assert_eq!(code_dir_under(dll, setting), PathBuf::from(want));
        }
        assert_eq!(code_dir(None, None), None);
        assert_eq!(code_dir(None, Some("code_b")), None);
        assert_eq!(
            code_dir(None, Some(r"D:\x\codes")),
            Some(PathBuf::from(r"D:\x\codes"))
        );
    }
}
