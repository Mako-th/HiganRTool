//! 古いリプレイのチャージ型を直すウィンドウのツール（`HiganRTool_replay_fix.exe`）の、Win32に依存しない部分。
//!
//! 画面の状態（`model`）・文言（`text`）・リプレイの一覧（`scan`）・読み書き（`work`）・引数（`cli`）を、ここへ分ける。Linuxでも`cargo test`できるようにするため。
//! ウィンドウそのもの（`win`）はmain.rs側のWindows専用のモジュール。
//! リプレイの中身の書き換えは`higanr_core::replay_rewrite`。
#![forbid(unsafe_code)]

pub mod cli;
pub mod model;
pub mod scan;
pub mod text;
pub mod work;
