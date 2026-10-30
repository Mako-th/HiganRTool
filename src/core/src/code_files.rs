//! 接続コードの受け渡しに使うファイルの名前と置き場の決まり。
//! dllの`net_code`とローダーの`code_relay`で同じでなければならない（食い違うとローダーは誰も読まない所へ書く）。
//! そのため両方から参照できるcoreに定義する。
//!
//! - 置き場はDLLのフォルダの`code\`（iniの`code_dir`で変えられる。絶対パスはそのまま使う。相対パスはDLLのフォルダから）。
//! - `my_code.txt` … 自分のコード（dllが書き、接続したら消す）。`peer_code.txt` … 相手のコード（人かローダーが貼る。dllは読むだけ）。
//! - `status.txt` … いまの状態（dllが書き直す。ローダーのウィンドウが「状態」の行に出す）。`mode.txt` … ウィンドウで選んだ対戦の形（ローダーが書き、dllの監視の処理が読む。対象のゲームのpidつき）。
//! - `clip_out.txt` / `clip_in.txt` … `clipboard = file`（開発用）のときのクリップボードの代わり。dllが`clip_out.txt`へ「コピーする文字」を書き、`clip_in.txt`から「貼る文字」を読む。
//! - 書き直しは一時の名前（`<名前>.tmp`）に書いてから置き換える（読み手が書きかけを見ない）。
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 置き場の既定（DLLのフォルダから）。
pub const CODE_DIR: &str = "code";
pub const MY_CODE_FILE: &str = "my_code.txt";
pub const PEER_CODE_FILE: &str = "peer_code.txt";
pub const STATUS_FILE: &str = "status.txt";
/// `peer_code.txt`を置き換える前の一時の名前（ローダーが使う）。`tmp_name(PEER_CODE_FILE)`と同じ値（検査が揃いを見る）。
pub const PEER_TMP_FILE: &str = "peer_code.txt.tmp";
/// 中身の`pid`が自分の実行と違えばdllは使わない。
pub const MODE_FILE: &str = "mode.txt";
/// `tmp_name(MODE_FILE)`と同じ値（検査が揃いを見る）。
pub const MODE_TMP_FILE: &str = "mode.txt.tmp";

pub const CLIP_OUT_FILE: &str = "clip_out.txt";
pub const CLIP_IN_FILE: &str = "clip_in.txt";
/// クリップボードの形式`CF_UNICODETEXT`（UTF-16の終端0つき。`winuser.h`の値13）。windows-sysの定義はOleのfeatureが要るのでここに定義する。
pub const CF_UNICODETEXT: u32 = 13;

/// ローダーとDLLが相手のファイルを読む間隔。人が貼ってからDLLが接続し始めるまでの待ちの上限になる。
pub const POLL: Duration = Duration::from_millis(250);

/// 読むファイルの上限（B）。いちばん長いコードは354文字でBOMと空行を足しても1 KBに届かない。越えた分は読まない。
pub const MAX_FILE_BYTES: u64 = 4096;

pub fn tmp_name(name: &str) -> String {
    format!("{name}.tmp")
}

/// 相手の名前を含む1行をファイルやコンソールへ出せる形にする: 制御文字（ESC・改行・NULなど）・行区切り・向きを変える文字は`?`にする。
/// 名前は相手が決めるのでそのままでは`status.txt`の行を増やしたりコンソールの画面制御を送り込めたりする。
pub fn plain_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            let hostile = c.is_control()
                || matches!(
                    c,
                    '\u{2028}'
                        | '\u{2029}'
                        | '\u{200E}'
                        | '\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                );
            if hostile { '?' } else { c }
        })
        .collect()
}

/// `setting`が絶対パスならそのパス。相対パスなら`dll_dir`からの位置。無ければ`dll_dir`の`code`。`dll_dir`が分からず相対パスならNone。
pub fn code_dir(dll_dir: Option<&Path>, setting: Option<&str>) -> Option<PathBuf> {
    match dll_dir {
        Some(d) => Some(code_dir_under(d, setting)),
        None => {
            let p = Path::new(setting.unwrap_or(CODE_DIR));
            p.is_absolute().then(|| p.to_path_buf())
        }
    }
}

/// DLLのフォルダが分かっている形（ローダー。常に決まる）。
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
    fn plain_line_replaces_control_and_direction_characters_only() {
        assert_eq!(plain_line("Bob\u{1b}[2J\r\nX\0y\u{85}"), "Bob?[2J??X?y?");
        assert_eq!(
            plain_line("a\u{2028}b\u{202E}c\u{2067}d\u{200F}e"),
            "a?b?c?d?e"
        );
        // 普通の名前（日本語・空白・記号・絵文字）は変えない。
        assert_eq!(plain_line("ほげ 太郎_1 (x) 😀"), "ほげ 太郎_1 (x) 😀");
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
