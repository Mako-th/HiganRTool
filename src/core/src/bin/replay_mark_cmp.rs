//! 2〜3 台のリプレイの印（`HGRT`）のラウンドの記録（長さ・勝者・ライフ・C2+C3・ボス・リバーサル・スコア）を並べて比べる道具。
//! rollback の 2 台（a・b）と、素の花映塚と同じ道で進む観戦者（c）の印を並べ、a = b ≠ c かを見る（中身は `replay_mark_cmp`）。
//!
//! 使い方:
//!   `replay_mark_cmp [--since <UNIX 秒>] [--skip-abort] [--inputs] <a> <b> [<c>]`
//!
//! - `<a>` `<b>` `<c>` は、その台のリプレイ（`.rpy`）1 本か、フォルダ（中を下へたどって `.rpy` を全部集める）。
//!   台ごとに印の start の順に並べ、同じ番目どうしを同じマッチとして比べる。
//! - `--since`: start がこの UNIX 秒より前の印を除く（保管庫に前の回のリプレイが残っているとき）。印が読めないリプレイは
//!   標準エラーに理由を出して除く（`--since` が無ければ断る）。start の無い印があれば断る。
//! - `--skip-abort`: 1P から見た結果が abort のリプレイ（台本の終わりで途中のまま切れたマッチの保険の保存）を除く。除いた本数は標準エラーに出す。
//! - `--inputs`: リプレイの入力の語も台どうしで比べる（共通のフレームの範囲）。
//! - 終了コード: 0 = 全部の台に同じ本数があり、全部のマッチが全部の台で一致 / 1 = 違い・本数の差・ラウンドの記録が 0 件のリプレイがある /
//!   2 = 使い方・読めないファイル。
//!
//! リプレイは読むだけ。プレイヤー名・日付は出さない（ファイル名だけ）。
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use higanr_core::replay::MatchResult;
use higanr_core::replay_mark_cmp::{self as cmp, Device, Entry};

const USAGE: &str = "使い方:\n  replay_mark_cmp [--since <UNIX 秒>] [--skip-abort] [--inputs] <a> <b> [<c>]\n  各引数はリプレイ（.rpy）1 本かフォルダ";

/// これより大きいファイルは読まない（壊れた指定でメモリを食わない）。
const MAX_FILE: u64 = 64 << 20;
/// 1 台あたりに集めるリプレイの上限（フォルダの指定を間違えて保管庫の全部を読まないため）。
const MAX_FILES: usize = 400;
/// フォルダをたどる深さの上限（保管庫は「相手の名前のフォルダ」の 1 段）。
const MAX_DEPTH: usize = 4;

#[derive(Debug, Default)]
struct Args {
    since: Option<u64>,
    inputs: bool,
    skip_abort: bool,
    paths: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--since" => {
                let v = it.next().ok_or("--since に値が要る")?;
                a.since = Some(v.parse().map_err(|_| "--since は UNIX 秒（10 進）")?);
            }
            "--inputs" => a.inputs = true,
            "--skip-abort" => a.skip_abort = true,
            s if s.starts_with("--") => return Err(format!("知らない引数 {s}")),
            s => a.paths.push(s.to_string()),
        }
    }
    if !(2..=3).contains(&a.paths.len()) {
        return Err(format!(
            "台は 2 つか 3 つ（a b [c]）。{} 個渡された",
            a.paths.len()
        ));
    }
    Ok(a)
}

fn collect(path: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("{} を読めない: {e}", path.display()))?;
    if meta.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if depth > MAX_DEPTH {
        return Err(format!(
            "{} は深すぎる（{MAX_DEPTH} 段まで）",
            path.display()
        ));
    }
    let mut names: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("{} を開けない: {e}", path.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    names.sort();
    for p in names {
        let is_dir = std::fs::metadata(&p).is_ok_and(|m| m.is_dir());
        if is_dir {
            collect(&p, depth + 1, out)?;
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("rpy")) {
            out.push(p);
        }
        if out.len() > MAX_FILES {
            return Err(format!(
                "リプレイが {MAX_FILES} 本を超えた（指定が広すぎる）"
            ));
        }
    }
    Ok(())
}

fn load(label: &str, path: &str, a: &Args) -> Result<Device, String> {
    let mut files = Vec::new();
    collect(Path::new(path), 0, &mut files)?;
    if files.is_empty() {
        return Err(format!("{label}: {path} にリプレイ（.rpy）が無い"));
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut skipped_abort = 0usize;
    for f in &files {
        let meta = std::fs::metadata(f).map_err(|e| format!("{} を読めない: {e}", f.display()))?;
        if meta.len() > MAX_FILE {
            return Err(format!("{} が大きすぎる（{} B）", f.display(), meta.len()));
        }
        let bytes = std::fs::read(f).map_err(|e| format!("{} を読めない: {e}", f.display()))?;
        let name = f
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let e = match cmp::entry_from_bytes(&name, &bytes, a.inputs) {
            Ok(e) => e,
            // 前の回のリプレイ（印が古い・無い）が残っていても、今の回だけを見られるように。読めなかった事実は残す。
            Err(why) if a.since.is_some() => {
                eprintln!("{label}: 読めないので除いた — {why}");
                continue;
            }
            Err(why) => return Err(format!("{label}: {why}")),
        };
        if let Some(since) = a.since {
            match e.start_utc {
                None => {
                    return Err(format!(
                        "{label}: {name} の印に start が無いので --since で絞れない"
                    ));
                }
                Some(t) if t < since => continue,
                Some(_) => {}
            }
        }
        if a.skip_abort && e.p1_result == MatchResult::Abort {
            skipped_abort += 1;
            continue;
        }
        entries.push(e);
    }
    if skipped_abort > 0 {
        eprintln!("{label}: abort のリプレイ {skipped_abort} 本を除いた");
    }
    cmp::sort_entries(&mut entries);
    Ok(Device {
        label: label.to_string(),
        entries,
    })
}

fn run(args: &[String]) -> Result<bool, String> {
    let a = parse_args(args)?;
    let labels = ["a", "b", "c"];
    let mut devs = Vec::new();
    for (label, path) in labels.iter().zip(&a.paths) {
        devs.push(load(label, path, &a)?);
    }
    let r = cmp::report(&devs, a.inputs);
    print!("{}", r.text);
    Ok(r.all_equal)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
