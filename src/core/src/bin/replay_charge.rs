//! リプレイの見出しの 1P / 2P のチャージ型を読む・書き換えた写しを作る道具（再生のずれの切り分け用。中身は `replay_rewrite`）。
//!
//! 使い方:
//!   `replay_charge <in.rpy>`                         … 見出しの (1P, 2P) のチャージ型を出す
//!   `replay_charge <in.rpy> <1P> <2P> <out.rpy>`     … チャージ型だけを書き換えた写しを `<out.rpy>` に作る
//!
//! - 元のファイルは読むだけ。`<out.rpy>` が既にあれば断る（上書きしない）。ゲームの `replay\` へ置くのは人の手で。
//! - 終了コード: 0 = できた / 2 = 使い方・読めないファイル・書けない。
#![forbid(unsafe_code)]

use std::process::ExitCode;

use higanr_core::replay_decode::decode_image;
use higanr_core::replay_rewrite::{charge, with_charge};

const USAGE: &str =
    "使い方:\n  replay_charge <in.rpy>\n  replay_charge <in.rpy> <1P> <2P> <out.rpy>";

/// これより大きいファイルは読まない（壊れた指定でメモリを食わない）。
const MAX_FILE: u64 = 64 << 20;

fn read(path: &str) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{path} を開けない: {e}"))?
        .len();
    if len > MAX_FILE {
        return Err(format!("{path} が大きすぎる（{len} バイト）"));
    }
    std::fs::read(path).map_err(|e| format!("{path} を読めない: {e}"))
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [input] => {
            let file = read(input)?;
            let image = decode_image(&file).map_err(|e| format!("{input} を読めない: {e:?}"))?;
            let (p1, p2) = charge(&image).ok_or("見出しが短い")?;
            println!("{input}: 1P のチャージ型 {p1} / 2P のチャージ型 {p2}");
            Ok(())
        }
        [input, p1, p2, out] => {
            let p1: u8 = p1.parse().map_err(|_| "1P は 0〜255")?;
            let p2: u8 = p2.parse().map_err(|_| "2P は 0〜255")?;
            if std::path::Path::new(out).exists() {
                return Err(format!("{out} は既にある（上書きしない）"));
            }
            let file = read(input)?;
            let new =
                with_charge(&file, p1, p2).map_err(|e| format!("{input} を読めない: {e:?}"))?;
            std::fs::write(out, &new).map_err(|e| format!("{out} に書けない: {e}"))?;
            println!(
                "{out}: 1P のチャージ型 {p1} / 2P のチャージ型 {p2} で書いた（{} バイト）",
                new.len()
            );
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
