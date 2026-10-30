//! `HiganRTool_replay_fix.exe`。古いリプレイのチャージ型を直すウィンドウ（中身は`higanrtool_replay_fix`と`higanr_core::replay_rewrite`）。
//! 黒いコンソールのウィンドウを出さない。引数で`--batch`を付けるとウィンドウを出さずに作る（終了コードとログのファイルで結果を返す）。
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
