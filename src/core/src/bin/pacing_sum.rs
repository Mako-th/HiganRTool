//! `_pacing.csv.gz`（DLL が `build\bin\logs\` に書く。平文の `_pacing.csv` も読む）をあとから集計する道具。
//! gzip が強制終了で切れていたら、読めた所までを使って標準エラーに知らせる。読むだけで、どのファイルにも書かない。
//!
//! 使い方: `cargo run --release -p higanr_core --bin pacing_sum -- [--fps N] [--frames A-B] <_pacing.csv.gz> [...]`
//! ファイルごとに、形（game / b / b-spin / b-hybrid / vpatch / cycle / auto）と、周期・起床の遅れ・入力 → Present の前 / 後・
//! Present の長さの p50 / p95 / p99 / max と母数を出す（集計の式は DLL と同じ `higanr_core::pacing::Splitter`）。
//! 方式や効率クラスが複数あれば、その別にも出す。
//! 列 `glide_req_ppm` `glide_moved_us` に 0 でない要求があれば「格子の寄せ」の行を、b-hybrid の列があれば
//! 使用率の見込みと寝過ごしの行も出す。その列の無い古い CSV では出ない。
//! `--fps` は長い周期の閾値に使うペース（既定 60）。CSV には書いていないので、本ログの「ペース: B を入れた…」（vpatch では「ペース: vpatch の先頭のフックを入れた…」）の N を渡す。
//! `--frames A-B` は Present の番号（CSV の `frame`）の範囲（両端を含む）の行だけを集計する。場面の境目は人が決めて渡す。
#![forbid(unsafe_code)]

use std::process::ExitCode;

use higanr_core::gzlog;
use higanr_core::pacing::{DEFAULT_FPS, parse_fps, summarize_csv};

const USAGE: &str =
    "使い方: pacing_sum [--fps N] [--frames A-B] <_pacing.csv.gz か _pacing.csv> [...]";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    fps: u64,
    /// Present の番号の範囲（両端を含む）。
    frames: Option<(u32, u32)>,
    paths: Vec<String>,
}

/// `A-B`（10 進、A ≤ B）。
fn parse_frames(v: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--frames の値 {v:?} は A-B（10 進の整数、A ≤ B）");
    let (a, b) = v.split_once('-').ok_or_else(bad)?;
    let num = |s: &str| -> Result<u32, String> {
        (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
            .ok_or_else(bad)
    };
    let (a, b) = (num(a)?, num(b)?);
    if a > b {
        return Err(bad());
    }
    Ok((a, b))
}

/// `--fps N` と `--frames A-B` はどこに置いてもよく、どちらも 1 回だけ。
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut fps = None;
    let mut frames = None;
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--fps" {
            if fps.is_some() {
                return Err("--fps が 2 回ある".to_string());
            }
            let v = it.next().ok_or("--fps の後に値が無い")?;
            fps = Some(parse_fps(v)?);
        } else if a == "--frames" {
            if frames.is_some() {
                return Err("--frames が 2 回ある".to_string());
            }
            let v = it.next().ok_or("--frames の後に値が無い")?;
            frames = Some(parse_frames(v)?);
        } else {
            paths.push(a.clone());
        }
    }
    if paths.is_empty() {
        return Err("集計する CSV が無い".to_string());
    }
    Ok(Args {
        fps: fps.unwrap_or(DEFAULT_FPS),
        frames,
        paths,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Args { fps, frames, paths } = match parse_args(&args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut failed = false;
    for p in &paths {
        let text = match gzlog::read_log_text(std::path::Path::new(p)) {
            Ok(t) => {
                if let Some(note) = t.cut_note(p) {
                    eprintln!("{note}");
                }
                t.text
            }
            Err(e) => {
                eprintln!("{p}: 読めない（{e}）");
                failed = true;
                continue;
            }
        };
        match summarize_csv(&text, fps, frames) {
            Ok((pre, report)) => {
                let range = match frames {
                    Some((a, b)) => format!("、フレーム {a}〜{b} だけ"),
                    None => String::new(),
                };
                let cycle = match pre.cycle_secs {
                    Some(c) => format!("、{c} 秒ごとに入れ替え"),
                    None => String::new(),
                };
                println!(
                    "== {p}（mode = {}{cycle}、ペース {fps} fps{range}）",
                    pre.mode.name()
                );
                for l in report.lines() {
                    println!("{l}");
                }
            }
            Err(e) => {
                eprintln!("{p}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn ok(fps: u64, frames: Option<(u32, u32)>, paths: &[&str]) -> Result<Args, String> {
        Ok(Args {
            fps,
            frames,
            paths: a(paths),
        })
    }

    #[test]
    fn args_take_fps_anywhere_and_default_to_60() {
        assert_eq!(parse_args(&a(&["x.csv"])), ok(60, None, &["x.csv"]));
        assert_eq!(
            parse_args(&a(&["--fps", "240", "x.csv", "y.csv"])),
            ok(240, None, &["x.csv", "y.csv"])
        );
        assert_eq!(
            parse_args(&a(&["x.csv", "--fps", "240"])),
            ok(240, None, &["x.csv"])
        );
        for bad in [
            &["--fps", "0", "x.csv"][..],
            &["--fps", "1001", "x.csv"],
            &["--fps", "240.0", "x.csv"],
            &["x.csv", "--fps"],
            &["--fps", "240"],
            &["--fps", "240", "--fps", "60", "x.csv"],
            &[],
        ] {
            assert!(parse_args(&a(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn args_take_a_frame_range() {
        assert_eq!(
            parse_args(&a(&["--frames", "476-7302", "x.csv"])),
            ok(60, Some((476, 7_302)), &["x.csv"])
        );
        assert_eq!(
            parse_args(&a(&["x.csv", "--frames", "5-5", "--fps", "240"])),
            ok(240, Some((5, 5)), &["x.csv"])
        );
        for bad in [
            &["--frames", "10-5", "x.csv"][..],
            &["--frames", "5", "x.csv"],
            &["--frames", "-5", "x.csv"],
            &["--frames", "5-", "x.csv"],
            &["--frames", "a-b", "x.csv"],
            &["--frames", "1-2-3", "x.csv"],
            &["--frames", "1-2", "--frames", "3-4", "x.csv"],
            &["x.csv", "--frames"],
        ] {
            assert!(parse_args(&a(bad)).is_err(), "{bad:?}");
        }
    }
}
