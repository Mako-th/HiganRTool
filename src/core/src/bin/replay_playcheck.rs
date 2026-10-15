//! 素の花映塚でリプレイを再生したとき、生きた試合どおりに再生できたかを確かめる道具。リプレイの印（`HGRT`）の `r1`〜 と、ツールの DLL が
//! 再生中に読んでログへ書いた同じ tick の値（`再生の照合:` の行）を、項目ごとに突き合わせる（中身は `replay_playcheck`）。
//!
//! 使い方:
//!   `replay_playcheck <リプレイ.rpy> <ログ>`
//!
//! - `<ログ>` は DLL の本ログ（`dll_*.log`）。再生の行が複数あれば、見出し（日付・名前）が合うもの、見分けられなければラウンドの長さが合うもの、
//!   それも無ければ最後の行を比べる（どれで選んだかを出す）。
//! - 終了コード: 0 = 全部一致 / 1 = 違いあり / 2 = 使い方・読めないファイル・比べる材料が無い。
//!
//! 読むだけ。プレイヤー名・日付は出さない（ファイル名だけ）。
#![forbid(unsafe_code)]

use std::process::ExitCode;

use higanr_core::replay_playcheck as pc;

const USAGE: &str = "使い方:\n  replay_playcheck <リプレイ.rpy> <ログ>";

/// これより大きいファイルは読まない（壊れた指定でメモリを食わない）。
const MAX_REPLAY: u64 = 64 << 20;
const MAX_LOG: u64 = 256 << 20;

fn read(path: &str, max: u64) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{path} を開けない: {e}"))?
        .len();
    if len > max {
        return Err(format!("{path} が大きすぎる（{len} バイト）"));
    }
    std::fs::read(path).map_err(|e| format!("{path} を読めない: {e}"))
}

fn run(args: &[String]) -> Result<bool, String> {
    let [rpy, log] = args else {
        return Err(USAGE.to_string());
    };
    let bytes = read(rpy, MAX_REPLAY)?;
    let name = std::path::Path::new(rpy)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let r = pc::ref_from_bytes(&name, &bytes)?;
    let log_bytes = read(log, MAX_LOG)?;
    let report = pc::run(&r, &String::from_utf8_lossy(&log_bytes))?;
    print!("{}", report.text);
    Ok(report.equal)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
