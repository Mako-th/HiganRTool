//! 道具 `pacing_sum` を本物のプロセスで走らせる検査（合成の `_pacing.csv` だけ）。平文でも `.csv.gz` でも読む・
//! 切れた gzip は読めた所まで使って知らせる、の配線を見る。

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use higanr_core::gzlog::GzLogWriter;
use higanr_core::pacing::{CSV_HEADER, FrameRow, Mode, Preamble, csv_preamble};

const BIN: &str = env!("CARGO_BIN_EXE_pacing_sum");

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p =
            std::env::temp_dir().join(format!("higanr_pacing_sum_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 合成の `_pacing.csv`（60 fps の b。入力 → Present の前は行ごとに違う値）。
fn pacing_csv(rows: u32) -> String {
    let qpf = 10_000_000;
    let mut s = format!(
        "{}\n{CSV_HEADER}\n",
        csv_preamble(&Preamble::new(qpf, Mode::B))
    );
    let mut prev: Option<FrameRow> = None;
    for f in 0..rows {
        let begin = 5_000_000 + u64::from(f) * 166_667;
        let r = FrameRow {
            frame: f,
            inputs: 1,
            input: begin - 1_000 - u64::from(f % 13) * 50,
            present_begin: begin,
            present_end: begin + 2_000,
            ..Default::default()
        };
        s.push_str(&r.csv_line(prev.as_ref(), qpf));
        s.push('\n');
        prev = Some(r);
    }
    s
}

/// `finish` が false なら終わりの印の 8 B を落とす（強制終了で切れた形）。
fn gz_bytes(csv: &str, finish: bool) -> Vec<u8> {
    let mut w = GzLogWriter::with_sync_interval(Vec::new(), std::time::Duration::ZERO);
    w.write_all(csv.as_bytes()).unwrap();
    w.flush().unwrap();
    let mut v = w.finish().unwrap();
    if !finish {
        v.truncate(v.len() - 8);
    }
    v
}

fn run(path: &str) -> (Option<i32>, String, String) {
    let o = Command::new(BIN).arg(path).output().unwrap();
    (
        o.status.code(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn pacing_sum_reads_plain_gzip_and_cut_gzip() {
    let t = Tmp::new("sum");
    let csv = pacing_csv(600);
    let (plain, gz, cut) = (t.path("p.csv"), t.path("p.csv.gz"), t.path("c.csv.gz"));
    std::fs::write(&plain, &csv).unwrap();
    std::fs::write(&gz, gz_bytes(&csv, true)).unwrap();
    std::fs::write(&cut, gz_bytes(&csv, false)).unwrap();
    let (code, base, _) = run(&plain);
    assert_eq!(code, Some(0), "{base}");
    assert!(base.contains("600"), "母数 600 が出ない: {base}");
    let body = |s: &str| s.lines().skip(1).collect::<Vec<_>>().join("\n");
    for p in [&gz, &cut] {
        let (code, out, err) = run(p);
        assert_eq!(code, Some(0), "{p}: {out}{err}");
        assert_eq!(body(&out), body(&base), "{p}");
    }
    assert!(!run(&gz).2.contains("終わりの印"));
    assert!(run(&cut).2.contains("終わりの印が無い"));
    let (code, _, err) = run(&t.path("無い.csv.gz"));
    assert_ne!(code, Some(0));
    assert!(err.contains("読めない"), "{err}");
}
