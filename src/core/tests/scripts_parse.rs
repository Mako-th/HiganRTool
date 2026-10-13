//! リポジトリの台本（`scripts/*.txt`）が全部 `parse` で読めることを見る検査。
//! DLL は読めない台本を使わずに観測だけで走るので、実機の回の前にここで落とす。

use higanr_core::script::parse;
use std::path::PathBuf;

#[test]
fn every_script_in_the_repository_parses() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} を開けない: {e}", dir.display()))
        .map(|e| e.expect("scripts/ の項目").path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "{} に台本が 1 本も無い", dir.display());
    let mut errors = Vec::new();
    let mut first_steps = None;
    for p in &names {
        let text = std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("{} を UTF-8 で読めない: {e}", p.display()));
        match parse(&text) {
            Ok(s) => {
                if p.file_name()
                    .is_some_and(|n| n == "m1d_01_match_and_pause.txt")
                {
                    first_steps = Some(s.steps.len());
                }
            }
            Err(e) => errors.push(format!("{}: {e}", p.display())),
        }
    }
    assert!(errors.is_empty(), "読めない台本:\n{}", errors.join("\n"));
    let n = first_steps.expect("scripts/m1d_01_match_and_pause.txt が無い");
    assert!(n > 0, "m1d_01_match_and_pause.txt の段が 0");
}
