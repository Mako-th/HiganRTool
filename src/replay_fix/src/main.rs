//! `HiganRTool_replay_fix.exe`。古いリプレイのチャージ型を直す窓（中身は `higanrtool_replay_fix` と `higanr_core::replay_rewrite`）。
//! 黒いコンソールの窓を出さない。引数で `--batch` を付けると窓を出さずに作る（終了コードとログのファイルで結果を返す）。
#![windows_subsystem = "windows"]

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    std::process::exit(win::main());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("replay_fix runs on Windows only.");
    std::process::exit(2);
}
