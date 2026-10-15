//! rollback の予測の当たり外れを、人の入力の記録で数える道具（ゲームは動かさない。読むだけで、配る zip には入らない）。
//!
//! 使い方:
//!   `pred_eval --out <新しいフォルダ> [--csv <_net_frames.csv か .csv.gz>]... [--rpy <リプレイ.rpy>]... [--r 8] [--focus-k 9] [--skip-check]`
//!
//! - 入力の列は 1 席 1 本。`--csv` は seg ごとに 1P・2P の列（両席とも人の対戦）、`--rpy` は人の席の列だけ
//!   （`human_cpu == 0`。CPU の席は全部 0 なので捨てる）。相手も人なら両席。語は `WORD_MASK`（送る語）で切る。
//! - 片道 k tick・遅れ D ごとの予測の幅は m = max(k − D, 0)、待ちの上限 R で打ち切る（`pred_eval::width_and_wait`）。
//!   採点は m ごとに 1 度だけ行い、k・D の表は m の結果を引き直す。
//! - 学習する方法（P2・P3）は「そのマッチの中だけ学ぶ（online）」と「ほかのファイルの全列で先に学ぶ（warm。leave-one-file-out）」の両方を出す。
//! - 先に、`--csv` の列で P0 の w = 1・2・4・8 を実回線のログで先に出してある数（`DOC_CHECK`）と照らす。0.05 ポイントを超えて違えば止まる（終了コード 3）。
//!   その 1 マッチ専用の検算なので、別のログを渡すときは `--skip-check`。
//! - `--out` のフォルダは新しく作る（あれば断る）。中身は `by_m.csv` / `by_k_d.csv` / `per_stream.csv` / `streams.csv`。
//!
//! リプレイは読むだけ。プレイヤー名・日付は出さない（列の名前は通し番号）。
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::ExitCode;

use higanr_core::gzlog;
use higanr_core::pred_eval::{
    self as pe, FrameEval, MAX_M, METHODS, Method, Pool, Predictor, Summary, WORD_MASK,
};
use higanr_core::replay_compare as cmp;
use higanr_core::replay_decode as dec;

const USAGE: &str = "使い方: pred_eval --out <新しいフォルダ> [--csv <_net_frames.csv>]... [--rpy <リプレイ.rpy>]... [--r 8] [--focus-k 9] [--skip-check]";
const MAX_FILE: u64 = 64 << 20;
/// 検算の相手: 先に実回線のログで出してある数。ほかの人との実回線のマッチ（20,302 tick・1 seg）の 1P / 2P で、w tick 前の語の繰り返しが外れる割合（%）。
const DOC_CHECK: [(usize, f64, f64); 4] = [
    (1, 17.6, 12.9),
    (2, 31.6, 23.7),
    (4, 52.9, 40.7),
    (8, 66.7, 54.4),
];
/// 既存の数は小数 1 桁なので、丸めの幅 0.05 以内なら一致とする。
const CHECK_TOL: f64 = 0.05;
/// 表に出す列の名前（`METHODS` の後ろに、どれかが当たれば当たりとした上限の目安を足す）。
const ORACLE: usize = METHODS.len();
const COLS: usize = METHODS.len() + 1;
/// 学習の仕方。
const TRAINS: [&str; 2] = ["online", "warm"];
const K_MAX: usize = 12;
const D_MAX: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    /// 実回線のマッチ（両席とも人）。
    Net,
    /// リプレイの人の席（相手は CPU）。
    Cpu,
    /// リプレイの人の席（相手も人）。
    Human,
}

const GROUPS: [Group; 3] = [Group::Net, Group::Cpu, Group::Human];

impl Group {
    fn name(self) -> &'static str {
        match self {
            Group::Net => "net",
            Group::Cpu => "rpy_vs_cpu",
            Group::Human => "rpy_vs_human",
        }
    }
    fn idx(self) -> usize {
        GROUPS.iter().position(|g| *g == self).unwrap_or(0)
    }
}

struct Stream {
    label: String,
    group: Group,
    /// 元のファイルの通し番号（warm の学習から同じファイルの列を除く）。
    source: usize,
    words: Vec<u16>,
}

#[derive(Debug, Default)]
struct Args {
    out: String,
    csv: Vec<String>,
    rpy: Vec<String>,
    r: usize,
    focus_k: usize,
    skip_check: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        r: 8,
        focus_k: 9,
        ..Args::default()
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} に値が要る"))
        };
        match x.as_str() {
            "--out" => a.out = value("--out")?,
            "--csv" => a.csv.push(value("--csv")?),
            "--rpy" => a.rpy.push(value("--rpy")?),
            "--r" => a.r = value("--r")?.parse().map_err(|_| "--r は 1〜16")?,
            "--focus-k" => {
                a.focus_k = value("--focus-k")?
                    .parse()
                    .map_err(|_| "--focus-k は 1〜12")?;
            }
            "--skip-check" => a.skip_check = true,
            s => return Err(format!("知らない引数 {s}")),
        }
    }
    if a.out.is_empty() {
        return Err("--out が要る".to_string());
    }
    if a.csv.is_empty() && a.rpy.is_empty() {
        return Err("--csv か --rpy が要る".to_string());
    }
    if !(1..=MAX_M).contains(&a.r) {
        return Err(format!("--r は 1〜{MAX_M}"));
    }
    if !(1..=K_MAX).contains(&a.focus_k) {
        return Err(format!("--focus-k は 1〜{K_MAX}"));
    }
    Ok(a)
}

fn read_limited(path: &str) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path} を読めない: {e}"))?;
    if meta.len() > MAX_FILE {
        return Err(format!("{path} が大きすぎる（{} B）", meta.len()));
    }
    std::fs::read(path).map_err(|e| format!("{path} を読めない: {e}"))
}

fn load_csv(path: &str, source: usize, out: &mut Vec<Stream>) -> Result<(), String> {
    let t = gzlog::text_from_bytes(read_limited(path)?)
        .map_err(|_| format!("{path} が UTF-8 でない"))?;
    if let Some(note) = t.cut_note(path) {
        eprintln!("{note}");
    }
    let rows = cmp::parse_net_frames(&t.text).map_err(|e| format!("{path} を読めない: {e:?}"))?;
    for seg in cmp::segs(&rows) {
        let seg_rows: Vec<_> = rows.iter().filter(|r| r.seg == seg).collect();
        // frame が 0 から 1 ずつ進んでいなければ、語の並びが tick の並びと違う（欠け・重複）ので断る。
        if let Some((i, r)) = seg_rows
            .iter()
            .enumerate()
            .find(|(i, r)| r.frame as usize != *i)
        {
            return Err(format!(
                "{path} の seg {seg} は frame が連続でない（{} 行目が frame {}）",
                i + 1,
                r.frame
            ));
        }
        for (seat, pick) in [("P1", 0usize), ("P2", 1)] {
            out.push(Stream {
                label: format!("net{source}-seg{seg}-{seat}"),
                group: Group::Net,
                source,
                words: seg_rows
                    .iter()
                    .map(|r| if pick == 0 { r.p1 } else { r.p2 } & WORD_MASK)
                    .collect(),
            });
        }
    }
    Ok(())
}

fn load_rpy(path: &str, source: usize, out: &mut Vec<Stream>) -> Result<(), String> {
    let r = dec::decode_inputs(&read_limited(path)?)
        .map_err(|e| format!("{path} を復号できない: {e:?}"))?;
    for st in &r.stages {
        let human = [st.p1.human_cpu == 0, st.p2.human_cpu == 0];
        let group = if human[0] && human[1] {
            Group::Human
        } else {
            Group::Cpu
        };
        for (seat, ok, inputs) in [
            ("P1", human[0], &st.p1_inputs),
            ("P2", human[1], &st.p2_inputs),
        ] {
            if ok {
                out.push(Stream {
                    label: format!("rpy{source}-slot{}-{seat}", st.slot),
                    group,
                    source,
                    words: inputs.iter().map(|w| w & WORD_MASK).collect(),
                });
            }
        }
    }
    Ok(())
}

/// 学習する方法を、ほかのファイルの全列で先に学ばせる（列の境目では履歴だけ捨てる）。
fn trained(index: usize, streams: &[Stream], skip_source: usize) -> Method {
    let mut p = Method::new(index).unwrap_or_else(|| unreachable!("方法の添字は METHODS の範囲"));
    for s in streams.iter().filter(|s| s.source != skip_source) {
        p.reset_stream();
        for w in &s.words {
            p.observe(*w);
        }
    }
    p.reset_stream();
    p
}

/// 採点の結果: [train][group][列][m] の集計と、[train][stream][列][m] の列ごとの集計。
struct Results {
    pooled: Vec<Vec<Vec<Vec<Summary>>>>,
    per_stream: Vec<Vec<Vec<Vec<Summary>>>>,
}

fn score(streams: &[Stream]) -> Results {
    let ms = MAX_M + 1;
    let mut pooled = vec![vec![vec![vec![Summary::default(); ms]; COLS]; GROUPS.len()]; 2];
    let mut per_stream = vec![vec![vec![vec![Summary::default(); ms]; COLS]; streams.len()]; 2];
    for (ti, _) in TRAINS.iter().enumerate() {
        // 集めてから最後に summary() にする。
        let mut pools: Vec<Vec<Vec<Pool>>> =
            vec![vec![vec![Pool::default(); ms]; COLS]; GROUPS.len()];
        for (si, s) in streams.iter().enumerate() {
            // 表を使う方法は、この列のために 1 度だけ作って m ごとに複製する。
            let bases: Vec<Method> = (0..METHODS.len())
                .map(|i| {
                    if ti == 1 {
                        trained(i, streams, s.source)
                    } else {
                        Method::new(i).unwrap_or_else(|| unreachable!())
                    }
                })
                .collect();
            for m in 0..ms {
                let evals: Vec<FrameEval> = bases
                    .iter()
                    .map(|b| {
                        let mut p = b.clone();
                        p.reset_stream();
                        pe::eval_stream(&mut p, &s.words, m)
                    })
                    .collect();
                let refs: Vec<&FrameEval> = evals.iter().collect();
                let orc = pe::oracle(&refs);
                for (ci, e) in evals.iter().chain(std::iter::once(&orc)).enumerate() {
                    pools[s.group.idx()][ci][m].add(e);
                    let mut one = Pool::default();
                    one.add(e);
                    per_stream[ti][si][ci][m] = one.summary();
                }
            }
        }
        for (gi, g) in pools.iter().enumerate() {
            for (ci, c) in g.iter().enumerate() {
                for (m, pool) in c.iter().enumerate() {
                    pooled[ti][gi][ci][m] = pool.summary();
                }
            }
        }
    }
    Results { pooled, per_stream }
}

fn col_name(ci: usize) -> &'static str {
    if ci == ORACLE { "ORACLE" } else { METHODS[ci] }
}

const STAT_HEADER: &str = "streams,n,miss,miss_pct,run_n,run_p50,run_p90,run_max,err_mean,err_p90,err_max,err_ge8_pct,snap_mean,snap_p90,snap_max";

fn stat_cols(s: &Summary) -> String {
    format!(
        "{},{},{},{:.3},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
        s.streams,
        s.n,
        s.miss,
        s.miss_pct,
        s.run_n,
        s.run_p50,
        s.run_p90,
        s.run_max,
        s.err_mean,
        s.err_p90,
        s.err_max,
        s.big_pct,
        s.snap_mean,
        s.snap_p90,
        s.snap_max
    )
}

fn write_new(dir: &str, name: &str, text: &str) -> Result<(), String> {
    use std::io::Write;
    let path = std::path::Path::new(dir).join(name);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{} に書けない: {e}", path.display()))
}

fn write_csvs(dir: &str, streams: &[Stream], res: &Results, r: usize) -> Result<(), String> {
    let mut by_m = format!("train,group,method,m,{STAT_HEADER}\n");
    let mut by_kd = format!("train,group,method,k,d,m,m_eff,r,wait_pct,{STAT_HEADER}\n");
    for (ti, t) in TRAINS.iter().enumerate() {
        for g in GROUPS {
            for ci in 0..COLS {
                for m in 0..=MAX_M {
                    let s = &res.pooled[ti][g.idx()][ci][m];
                    if s.n > 0 {
                        let _ = writeln!(
                            by_m,
                            "{t},{},{},{m},{}",
                            g.name(),
                            col_name(ci),
                            stat_cols(s)
                        );
                    }
                }
                for k in 1..=K_MAX {
                    for d in 0..=D_MAX {
                        let (m_eff, wait) = pe::width_and_wait(k, d, r);
                        let s = &res.pooled[ti][g.idx()][ci][m_eff];
                        if s.n > 0 {
                            let m = k.saturating_sub(d);
                            let _ = writeln!(
                                by_kd,
                                "{t},{},{},{k},{d},{m},{m_eff},{r},{wait:.2},{}",
                                g.name(),
                                col_name(ci),
                                stat_cols(s)
                            );
                        }
                    }
                }
            }
        }
    }
    let mut per = format!("train,stream,group,method,m,{STAT_HEADER}\n");
    for (ti, t) in TRAINS.iter().enumerate() {
        for (si, s) in streams.iter().enumerate() {
            for ci in 0..COLS {
                for m in 0..=MAX_M {
                    let x = &res.per_stream[ti][si][ci][m];
                    if x.n > 0 {
                        let _ = writeln!(
                            per,
                            "{t},{},{},{},{m},{}",
                            s.label,
                            s.group.name(),
                            col_name(ci),
                            stat_cols(x)
                        );
                    }
                }
            }
        }
    }
    let mut st = String::from("stream,group,source,frames\n");
    for s in streams {
        let _ = writeln!(
            st,
            "{},{},{},{}",
            s.label,
            s.group.name(),
            s.source,
            s.words.len()
        );
    }
    write_new(dir, "by_m.csv", &by_m)?;
    write_new(dir, "by_k_d.csv", &by_kd)?;
    write_new(dir, "per_stream.csv", &per)?;
    write_new(dir, "streams.csv", &st)
}

/// 検算。`--csv` の列の席 1・2 ごとに、P0 の w = 1・2・4・8 を既存の数と照らす。
fn check_against_doc(streams: &[Stream]) -> bool {
    let nets: Vec<&Stream> = streams.iter().filter(|s| s.group == Group::Net).collect();
    if nets.len() < 2 {
        println!("検算: 実回線の列が無いので既存の数と照らせない（--csv を渡す）");
        return true;
    }
    let mut ok = true;
    println!(
        "== 検算: P0 の外れ率（%）を Design_Rollback §9-9 の既存の数と比べる（実回線の 1 マッチ。列 {}）",
        nets[0].label
    );
    println!(
        "{:>3} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}  {:>7}",
        "w", "1P 実", "1P 既存", "差", "2P 実", "2P 既存", "差", "n"
    );
    for (w, d1, d2) in DOC_CHECK {
        let (m1, n1) = pe::repeat_miss_count(&nets[0].words, w);
        let (m2, n2) = pe::repeat_miss_count(&nets[1].words, w);
        let p1 = 100.0 * m1 as f64 / n1 as f64;
        let p2 = 100.0 * m2 as f64 / n2 as f64;
        let hit = (p1 - d1).abs() <= CHECK_TOL + 1e-9 && (p2 - d2).abs() <= CHECK_TOL + 1e-9;
        ok &= hit;
        println!(
            "{w:>3} {p1:>8.2} {d1:>8.1} {:>+8.2} {p2:>8.2} {d2:>8.1} {:>+8.2}  {n1:>7} {}",
            p1 - d1,
            p2 - d2,
            if hit { "" } else { "不一致" }
        );
    }
    // eval_stream の P0 でも同じ数になること（採点の経路の検算）。
    for (w, _, _) in DOC_CHECK {
        for s in nets.iter().take(2) {
            let mut p = Method::new(0).unwrap_or_else(|| unreachable!());
            p.reset_stream();
            let e = pe::eval_stream(&mut p, &s.words, w);
            let (miss, n) = pe::repeat_miss_count(&s.words, w);
            if e.miss.iter().filter(|x| **x).count() as u64 != miss || e.miss.len() as u64 != n {
                println!("eval_stream の P0 が直に数えた値と違う: w={w} {}", s.label);
                ok = false;
            }
        }
    }
    ok
}

fn pct_row(res: &Results, ti: usize, g: Group, m: usize, f: impl Fn(&Summary) -> f64) -> String {
    let mut line = String::new();
    for ci in 0..COLS {
        let _ = write!(line, " {:>7.2}", f(&res.pooled[ti][g.idx()][ci][m]));
    }
    line
}

fn print_tables(streams: &[Stream], res: &Results, a: &Args) {
    let header = {
        let mut h = String::new();
        for ci in 0..COLS {
            let _ = write!(h, " {:>7}", col_name(ci));
        }
        h
    };
    for g in GROUPS {
        let n_streams = streams.iter().filter(|s| s.group == g).count();
        if n_streams == 0 {
            continue;
        }
        let frames: usize = streams
            .iter()
            .filter(|s| s.group == g)
            .map(|s| s.words.len())
            .sum();
        println!(
            "\n==== {}: 列 {n_streams} 本・{frames} tick（学習: online = そのマッチの中だけ）====",
            g.name()
        );
        println!(
            "-- 外れ率 %（予測した語が本物と違う tick の割合。m = 予測の幅 = 片道 k − 遅れ D）"
        );
        println!("{:>3} {:>9}{header}", "m", "n");
        for m in [1usize, 2, 3, 4, 5, 6, 8, 9, 10, 12] {
            println!(
                "{m:>3} {:>9}{}",
                res.pooled[0][g.idx()][0][m].n,
                pct_row(res, 0, g, m, |s| s.miss_pct)
            );
        }
        println!(
            "-- 位置のずれの平均 px（窓の予測と本物の積分の差。高速 {} / 低速 {} px。端のクランプなし）",
            pe::SPEED_FAST,
            pe::SPEED_SLOW
        );
        println!("{:>3} {:>9}{header}", "m", "n");
        for m in [1usize, 2, 3, 4, 5, 6, 8, 9, 10, 12] {
            println!(
                "{m:>3} {:>9}{}",
                res.pooled[0][g.idx()][0][m].n,
                pct_row(res, 0, g, m, |s| s.err_mean)
            );
        }
        println!("-- 位置のずれが {} px 以上の tick の割合 %", pe::BIG_ERR);
        println!("{:>3} {:>9}{header}", "m", "n");
        for m in [2usize, 4, 6, 8, 9, 12] {
            println!(
                "{m:>3} {:>9}{}",
                res.pooled[0][g.idx()][0][m].n,
                pct_row(res, 0, g, m, |s| s.big_pct)
            );
        }
        println!("-- 外れが続く長さ（tick）p50 / p90 / 最大（連の本数）。P0 / P2 / P3");
        for m in [2usize, 4, 8, 9] {
            let f = |ci: usize| {
                let s = &res.pooled[0][g.idx()][ci][m];
                format!("{}/{}/{} ({})", s.run_p50, s.run_p90, s.run_max, s.run_n)
            };
            println!("  m={m:<2}  P0 {:<22} P2 {:<22} P3 {}", f(0), f(2), f(3));
        }
        println!(
            "-- warm（ほかのファイルの全列で先に学んだ表）と online の差。外れ率 % / ずれの平均 px"
        );
        println!(
            "{:>3} {:>16} {:>16}",
            "m", "P2 online→warm", "P3 online→warm"
        );
        for m in [2usize, 4, 8, 9] {
            let f = |ci: usize| {
                let on = &res.pooled[0][g.idx()][ci][m];
                let wa = &res.pooled[1][g.idx()][ci][m];
                format!(
                    "{:.1}→{:.1} / {:.2}→{:.2}",
                    on.miss_pct, wa.miss_pct, on.err_mean, wa.err_mean
                )
            };
            println!("{m:>3} {:>16} {:>16}", f(2), f(3));
        }
    }

    // k 固定で D を振った表（実際の遅れ設定の見え方）。
    let k = a.focus_k;
    println!(
        "\n==== 片道 k = {k} tick（≒ {} ms）で遅れ D を振る（R = {}。online）====",
        (k as f64 * 1000.0 / 60.0).round(),
        a.r
    );
    for g in GROUPS {
        if !streams.iter().any(|s| s.group == g) {
            continue;
        }
        println!("-- {}", g.name());
        println!(
            "{:>2} {:>3} {:>6} | {:>7} {:>9} {:>9} | {:>7} {:>9} {:>9} | {:>8}",
            "D",
            "m",
            "待ち%",
            "P0外れ%",
            "最良外れ%",
            "最良の方法",
            "P0ずれpx",
            "最良ずれpx",
            "最良の方法",
            "n"
        );
        for d in 0..=D_MAX {
            let (m_eff, wait) = pe::width_and_wait(k, d, a.r);
            let cell = |ci: usize| &res.pooled[0][g.idx()][ci][m_eff];
            let best_miss = (0..METHODS.len())
                .min_by(|x, y| cell(*x).miss_pct.total_cmp(&cell(*y).miss_pct))
                .unwrap_or(0);
            let best_err = (0..METHODS.len())
                .min_by(|x, y| cell(*x).err_mean.total_cmp(&cell(*y).err_mean))
                .unwrap_or(0);
            println!(
                "{d:>2} {m_eff:>3} {wait:>6.1} | {:>7.2} {:>9.2} {:>9} | {:>7.2} {:>9.2} {:>9} | {:>8}",
                cell(0).miss_pct,
                cell(best_miss).miss_pct,
                METHODS[best_miss],
                cell(0).err_mean,
                cell(best_err).err_mean,
                METHODS[best_err],
                cell(0).n
            );
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let a = parse_args(args)?;
    let mut streams = Vec::new();
    let mut source = 0usize;
    for p in &a.csv {
        load_csv(p, source, &mut streams)?;
        source += 1;
    }
    for p in &a.rpy {
        load_rpy(p, source, &mut streams)?;
        source += 1;
    }
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for s in &streams {
        let e = counts.entry(s.group.name()).or_default();
        e.0 += 1;
        e.1 += s.words.len();
    }
    println!("== 入力");
    for (g, (n, f)) in &counts {
        println!("  {g}: 列 {n} 本・{f} tick");
    }
    println!("  元ファイル {source} 本");
    if !a.skip_check && !check_against_doc(&streams) {
        println!(
            "★検算が合わない。ここで止める（結果は書かない。別の回線のログなら --skip-check）"
        );
        return Ok(ExitCode::from(3));
    }
    std::fs::create_dir(&a.out).map_err(|e| {
        format!(
            "{} を作れない（あれば上書きしないので別の名前で）: {e}",
            a.out
        )
    })?;
    let res = score(&streams);
    write_csvs(&a.out, &streams, &res, a.r)?;
    print_tables(&streams, &res, &a);
    println!("\nCSV: {}", a.out);
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
