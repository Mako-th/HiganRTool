//! 選ばれたファイルとフォルダから、直す対象のリプレイ（`.rpy`）の一覧を作る。

use std::path::{Path, PathBuf};

use crate::model::{is_fix_dir, is_pair_dir};

/// フォルダの中を下へたどる深さ（選んだフォルダが 0）。
pub const MAX_DEPTH: usize = 4;
/// 集めるリプレイの上限（指定の間違いで大きなフォルダを全部たどらないため）。
pub const MAX_FILES: usize = 20_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// パスの順。重複なし。
    pub files: Vec<PathBuf>,
    /// 上限で打ち切った。
    pub truncated: bool,
}

fn is_replay(p: &Path) -> bool {
    p.extension().is_some_and(|x| x.eq_ignore_ascii_case("rpy"))
}

/// 選ばれたもの（ファイルかフォルダ）から集める。フォルダは中を下へたどるが、出力先の名前（`fix_replay`・`slow-charge` など）のフォルダの中は
/// 読まない。ファイルは `.rpy` だけ。無いパス・読めないフォルダは黙って飛ばす。
pub fn collect(inputs: &[PathBuf]) -> Found {
    let mut found = Found::default();
    for input in inputs {
        if input.is_dir() {
            walk(input, 0, &mut found);
        } else if input.is_file() && is_replay(input) {
            push(&mut found, input.clone());
        }
        if found.truncated {
            break;
        }
    }
    found.files.sort();
    found.files.dedup();
    found
}

fn push(found: &mut Found, p: PathBuf) {
    if found.files.len() >= MAX_FILES {
        found.truncated = true;
    } else {
        found.files.push(p);
    }
}

fn walk(dir: &Path, depth: usize, found: &mut Found) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if found.truncated {
            return;
        }
        if p.is_dir() {
            let skip = p
                .file_name()
                .is_some_and(|n| is_pair_dir(n) || is_fix_dir(n));
            if depth < MAX_DEPTH && !skip {
                walk(&p, depth + 1, found);
            }
        } else if is_replay(&p) {
            push(found, p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfix_scan_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn a_folder_is_walked_down_to_the_depth_limit_but_output_folders_are_not_read() {
        let d = temp_dir("walk");
        touch(&d.join("top.rpy"));
        touch(&d.join("a").join("b").join("deep.RPY"));
        touch(&d.join("a").join("note.txt"));
        for pair in ["slow-slow", "slow-charge", "charge-slow", "charge-charge"] {
            touch(&d.join("a").join(pair).join("made.rpy"));
        }
        for fix in ["fix_replay", "Fix_Replay"] {
            touch(
                &d.join("a")
                    .join(fix)
                    .join("Alice")
                    .join("charge-slow")
                    .join("made.rpy"),
            );
            touch(&d.join(fix).join("made2.rpy"));
        }
        // 名前が似ているだけのフォルダは読む。
        touch(&d.join("fix_replays").join("kept.rpy"));
        // 深さ 4（選んだフォルダが 0）の中までは読み、5 つ下は読まない。
        touch(&d.join("1").join("2").join("3").join("4").join("limit.rpy"));
        touch(
            &d.join("1")
                .join("2")
                .join("3")
                .join("4")
                .join("5")
                .join("toodeep.rpy"),
        );
        let got = collect(std::slice::from_ref(&d));
        let names: Vec<String> = got
            .files
            .iter()
            .map(|p| {
                p.strip_prefix(&d)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            names,
            [
                "1/2/3/4/limit.rpy",
                "a/b/deep.RPY",
                "fix_replays/kept.rpy",
                "top.rpy"
            ],
            "出力先のフォルダの中を読んだか、深さの数え方が違う"
        );
        assert!(!got.truncated);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn files_are_kept_only_when_rpy_and_duplicates_collapse() {
        let d = temp_dir("files");
        touch(&d.join("a.rpy"));
        touch(&d.join("b.txt"));
        let got = collect(&[
            d.join("a.rpy"),
            d.join("b.txt"),
            d.clone(),
            d.join("missing.rpy"),
        ]);
        assert_eq!(got.files, vec![d.join("a.rpy")]);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
