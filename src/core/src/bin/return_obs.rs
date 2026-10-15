//! 戻り道を動かさない実機の回の `_menu.csv`（DLL が `build\bin\logs\` に書く）を、戻り道が読む番地の想定
//! （`higanr_core::return_flow` の定数）と照らす道具。読むだけで、どのファイルにも書かない。
//!
//! 使い方: `cargo run --release -p higanr_core --bin return_obs -- <_menu.csv> [--pause] [--pause-first] [--end] [--final-screen 画面:カーソル]`
//! - `--pause` / `--end`: 台本が通すはずの一時停止 / 終わりのメニューの移り（`PAUSE_REQUIRED` / `END_REQUIRED`）が全部見えることを求める。
//! - `--pause-first`: 1 回目の一時停止の移りの列が `PAUSE_FIRST_MOVES`（開いた直後の下が効かない台本の流れ）と一致することを求める。
//! - `--final-screen 6:4`: 最後の行が画面 6・カーソル 4（戻り道の到着）であることを求める。
//!
//! 終了コード: 0 = 求めたものが全部合格、1 = 求めたものが不合格か測れていない、2 = 使い方かファイルの誤り。求めていない区間の結果は出すが数えない。
#![forbid(unsafe_code)]

use std::process::ExitCode;

use higanr_core::return_obs::{Expect, judge, parse_menu_csv};

const USAGE: &str = "使い方: return_obs <_menu.csv> [--pause] [--pause-first] [--end] [--final-screen 画面:カーソル]";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    path: String,
    want: Expect,
}

fn parse_final(v: &str) -> Result<(u32, i32), String> {
    let bad = || format!("--final-screen の値 {v:?} は 画面:カーソル（例 6:4）");
    let (a, b) = v.split_once(':').ok_or_else(bad)?;
    Ok((
        a.parse::<u32>().map_err(|_| bad())?,
        b.parse::<i32>().map_err(|_| bad())?,
    ))
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut want = Expect::default();
    let mut path: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pause" => want.pause = true,
            "--pause-first" => want.pause_first = true,
            "--end" => want.end = true,
            "--final-screen" => {
                if want.final_screen.is_some() {
                    return Err("--final-screen が 2 回ある".to_string());
                }
                let v = it.next().ok_or("--final-screen の後に値が無い")?;
                want.final_screen = Some(parse_final(v)?);
            }
            s if s.starts_with("--") => return Err(format!("知らない引数 {s}")),
            _ => {
                if path.replace(a.clone()).is_some() {
                    return Err("ファイルが 2 つある（1 つだけ渡す）".to_string());
                }
            }
        }
    }
    Ok(Args {
        path: path.ok_or("ファイルが無い")?,
        want,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse_args(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let text = match std::fs::read(&a.path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("{} を読めない: {e}", a.path);
            return ExitCode::from(2);
        }
    };
    let rows = match parse_menu_csv(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: {e}", a.path);
            return ExitCode::from(2);
        }
    };
    let report = judge(&rows, &a.want);
    println!(
        "{}: {} 行、一時停止の区間 {}・終わりのメニューの区間 {}",
        a.path,
        rows.len(),
        report.episodes.0,
        report.episodes.1
    );
    print!("{}", report.render());
    if report.not_all_passed() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn arguments_are_read_in_any_order_and_bad_ones_are_refused() {
        let a = parse_args(&s(&[
            "--end",
            "--pause-first",
            "x.csv",
            "--final-screen",
            "6:4",
        ]))
        .unwrap();
        assert_eq!(a.path, "x.csv");
        assert_eq!(
            a.want,
            Expect {
                pause: false,
                end: true,
                pause_first: true,
                final_screen: Some((6, 4))
            }
        );
        for bad in [
            s(&[]),
            s(&["a", "b"]),
            s(&["a", "--nothing"]),
            s(&["a", "--final-screen"]),
            s(&["a", "--final-screen", "6"]),
            s(&["a", "--final-screen", "6:4", "--final-screen", "6:4"]),
        ] {
            assert!(parse_args(&bad).is_err(), "{bad:?}");
        }
    }
}
