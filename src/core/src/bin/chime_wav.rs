//! 繋がった知らせの音（`higanr_core::chime`）を WAV に書き出して試し聴きする道具。
//! `cargo run --release -p higanr_core --bin chime_wav -- <出力.wav>`。同名のファイルがあれば断る。
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

use higanr_core::chime;

const USAGE: &str = "使い方: chime_wav <出力.wav>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let bytes = chime::connected_wav();
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(&bytes));
    match written {
        Ok(()) => {
            println!(
                "{path} に書いた（{} B、{} Hz・{} 秒）",
                bytes.len(),
                chime::RATE,
                chime::SECONDS
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path} に書けない: {e}");
            ExitCode::FAILURE
        }
    }
}
