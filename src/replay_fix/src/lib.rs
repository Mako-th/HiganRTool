//! 古いリプレイのチャージ型を直す窓の道具（`HiganRTool_replay_fix.exe`）の、Win32 に依存しない部分。
//!
//! 画面の状態（`model`）・文言（`text`）・リプレイの一覧（`scan`）・読み書き（`work`）・引数（`cli`）を、Linux でも
//! `cargo test` できるようにここへ分ける。窓そのもの（`win`）は main.rs 側の Windows 専用のモジュール。
//! リプレイの中身の書き換えは `higanr_core::replay_rewrite`。
#![forbid(unsafe_code)]

pub mod cli;
pub mod model;
pub mod scan;
pub mod text;
pub mod work;
