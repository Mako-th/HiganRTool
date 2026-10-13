//! 道具 `replay_inputs` を本物のプロセスで走らせる検査（合成のリプレイと合成の `_net_frames.csv` だけ）。
//! 「引数 → 読む → 出す → 終了コード」の配線と、出力ファイルを上書きしないことを見る。

use std::path::PathBuf;
use std::process::{Command, Output};

use higanr_core::replay_build::{BuildSpec, build_replay};

const BIN: &str = env!("CARGO_BIN_EXE_replay_inputs");

/// 検査ごとに別の一時ディレクトリ（終わったら消す）。
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p =
            std::env::temp_dir().join(format!("higanr_replay_inputs_{tag}_{}", std::process::id()));
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

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// 合成のマッチ: 1P は Z を押し続けて時々動く、2P は時々撃つ。
fn words(n: usize) -> (Vec<u16>, Vec<u16>) {
    let p1 = (0..n)
        .map(|i| 0x0001 | if i % 7 == 0 { 0x0040 } else { 0 })
        .collect();
    let p2 = (0..n).map(|i| if i % 5 < 2 { 0x0001 } else { 0 }).collect();
    (p1, p2)
}

fn net_csv(seg: u16, p1: &[u16], p2: &[u16], first: usize) -> String {
    let mut s = String::from("seg,frame,p1,p2,delay,margin_us,wait_us\n");
    for (f, (a, b)) in p1.iter().zip(p2).enumerate().skip(first) {
        s.push_str(&format!("{seg},{},{a:04X},{b:04X},3,,0\n", f - first));
    }
    s
}

#[test]
fn compare_reports_full_match_partial_match_and_offset() {
    let t = Tmp::new("compare");
    let (p1, p2) = words(300);
    let rpy = t.path("m.rpy");
    std::fs::write(
        &rpy,
        build_replay(&BuildSpec::match_replay(0x52, p1.clone(), p2.clone())),
    )
    .unwrap();

    let mut csv = net_csv(0, &p1, &p2, 2);
    let mut p2_bad = p2.clone();
    p2_bad[100] ^= 0x0010;
    csv.push_str(net_csv(1, &p1, &p2_bad, 0).split_once('\n').unwrap().1);
    let csv_path = t.path("net.csv");
    std::fs::write(&csv_path, &csv).unwrap();

    let o = run(&[&rpy, "--compare", &csv_path]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("--seg"));

    let o = run(&[&rpy, "--compare", &csv_path, "--seg", "0", "--offset", "2"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stdout));
    assert!(
        text(&o.stdout).contains("csv の 298 行のうち 298 行"),
        "{}",
        text(&o.stdout)
    );

    let o = run(&[&rpy, "--compare", &csv_path, "--seg", "0"]);
    assert_eq!(o.status.code(), Some(1));
    let out = text(&o.stdout);
    assert!(out.contains("最初の不一致"), "{out}");
    assert!(out.contains("offset 2: 298 行"), "{out}");

    let o = run(&[&rpy, "--compare", &csv_path, "--seg", "1", "--offset", "0"]);
    assert_eq!(o.status.code(), Some(1));
    let out = text(&o.stdout);
    assert!(
        out.contains("csv の 300 行のうち 299 行（1P が違う 0 行 / 2P が違う 1 行"),
        "{out}"
    );
    assert!(out.contains("csv の 101 行目（frame 100）"), "{out}");
    let o = run(&[
        &rpy,
        "--compare",
        &csv_path,
        "--seg",
        "1",
        "--offset",
        "0",
        "--mask",
        "FFEF",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stdout));
}

/// 8 列の `_net_frames.csv`（`head_wait_us` 付き。今の形）にする。`net_csv` は 7 列の古い形（過去のログを読めることの検査）。
fn net_csv8(seg: u16, p1: &[u16], p2: &[u16], first: usize) -> String {
    let mut s = String::from("seg,frame,p1,p2,delay,margin_us,wait_us,head_wait_us\n");
    for (f, (a, b)) in p1.iter().zip(p2).enumerate().skip(first) {
        s.push_str(&format!(
            "{seg},{},{a:04X},{b:04X},3,,0,{}\n",
            f - first,
            (f * 37) % 9_000
        ));
    }
    s
}

#[test]
fn the_eight_column_csv_with_head_wait_compares_like_the_old_seven() {
    let t = Tmp::new("eight_columns");
    let (p1, p2) = words(300);
    let rpy = t.path("m.rpy");
    std::fs::write(
        &rpy,
        build_replay(&BuildSpec::match_replay(0x52, p1.clone(), p2.clone())),
    )
    .unwrap();
    let new_path = t.path("new.csv");
    std::fs::write(&new_path, net_csv8(0, &p1, &p2, 1)).unwrap();
    let old_path = t.path("old.csv");
    std::fs::write(&old_path, net_csv(0, &p1, &p2, 1)).unwrap();
    let (n, o) = (
        run(&[&rpy, "--compare", &new_path]),
        run(&[&rpy, "--compare", &old_path]),
    );
    assert_eq!(n.status.code(), Some(0), "{}", text(&n.stdout));
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stdout));
    assert!(
        text(&n.stdout).contains("csv の 299 行のうち 299 行"),
        "{}",
        text(&n.stdout)
    );
    // 否定: 8 列の見出しの下に 7 列の行は断る（先頭の待ちが抜けた行を黙って読まない）。
    let mixed = t.path("mixed.csv");
    std::fs::write(
        &mixed,
        "seg,frame,p1,p2,delay,margin_us,wait_us,head_wait_us\n0,0,0001,0000,3,,0\n",
    )
    .unwrap();
    assert_eq!(run(&[&rpy, "--compare", &mixed]).status.code(), Some(2));
}

#[test]
fn default_offset_is_one_and_a_short_csv_is_called_short() {
    let t = Tmp::new("default_offset");
    let (p1, p2) = words(300);
    let rpy = t.path("m.rpy");
    std::fs::write(
        &rpy,
        build_replay(&BuildSpec::match_replay(0x52, p1.clone(), p2.clone())),
    )
    .unwrap();
    let mut full = net_csv(0, &p1, &p2, 1);
    for f in 299..339 {
        full.push_str(&format!("0,{f},0001,0000,3,,0\n"));
    }
    let full_path = t.path("full.csv");
    std::fs::write(&full_path, &full).unwrap();
    let o = run(&[&rpy, "--compare", &full_path]);
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("offset 1（既定）"), "{out}");
    assert!(out.contains("csv の 339 行のうち 299 行"), "{out}");
    assert!(out.contains("リプレイの終わりより後の行 40"), "{out}");
    assert!(!out.contains("csv が先に終わった"), "{out}");
    assert!(!out.contains("フレーム長い"), "{out}");
    let o = run(&[&rpy, "--compare", &full_path, "--offset", "0"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    assert!(!text(&o.stdout).contains("（既定）"));
    let short = net_csv(0, &p1[..250], &p2[..250], 1);
    let short_path = t.path("short.csv");
    std::fs::write(&short_path, &short).unwrap();
    let o = run(&[&rpy, "--compare", &short_path]);
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(
        out.contains("csv が先に終わった（リプレイの最後の 50 フレームは csv に無い。"),
        "{out}"
    );
    assert!(!out.contains("フレーム長い"), "{out}");
}

#[test]
fn zero_rows_and_bad_inputs_are_not_reported_as_a_match() {
    let t = Tmp::new("bad");
    let (p1, p2) = words(50);
    let rpy = t.path("m.rpy");
    let good = build_replay(&BuildSpec::match_replay(0x52, p1.clone(), p2.clone()));
    std::fs::write(&rpy, &good).unwrap();

    let empty = t.path("empty.csv");
    std::fs::write(&empty, "seg,frame,p1,p2,delay,margin_us,wait_us\n").unwrap();
    let o = run(&[&rpy, "--compare", &empty]);
    assert_eq!(o.status.code(), Some(2)); // seg が選べない（行が無い）
    let o = run(&[&rpy, "--compare", &empty, "--seg", "0"]);
    assert_eq!(o.status.code(), Some(2)); // csv に seg 0 が無い

    let wrong_header = t.path("old.csv");
    std::fs::write(&wrong_header, "seg,frame,p1,p2\n0,0,0001,0000\n").unwrap();
    assert_eq!(
        run(&[&rpy, "--compare", &wrong_header]).status.code(),
        Some(2)
    );

    let csv = t.path("net.csv");
    std::fs::write(&csv, net_csv(0, &p1, &p2, 0)).unwrap();
    assert_eq!(
        run(&[&rpy, "--compare", &csv, "--slot", "3"]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(&[&rpy, "--compare", &csv, "--offset", "0"])
            .status
            .code(),
        Some(0)
    );

    let mut broken = good.clone();
    broken[0x100] ^= 1;
    let bad = t.path("bad.rpy");
    std::fs::write(&bad, broken).unwrap();
    let o = run(&[&bad, "--compare", &csv, "--offset", "0"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).contains("ChecksumMismatch"),
        "{}",
        text(&o.stderr)
    );

    assert_eq!(run(&[&t.path("none.rpy")]).status.code(), Some(2));
    assert_eq!(run(&[]).status.code(), Some(2));
    assert_eq!(run(&[&rpy, "--seg", "0"]).status.code(), Some(2));
    assert_eq!(run(&[&rpy, "--offset", "0"]).status.code(), Some(2));
    assert_eq!(run(&[&rpy, "--offset", "1"]).status.code(), Some(2));
}

#[test]
fn out_file_is_written_once_and_never_overwritten() {
    let t = Tmp::new("out");
    let (p1, p2) = words(40);
    let rpy = t.path("m.rpy");
    std::fs::write(
        &rpy,
        build_replay(&BuildSpec::match_replay(0x52, p1.clone(), p2.clone())),
    )
    .unwrap();
    let out = t.path("inputs.csv");
    let o = run(&[&rpy, "--out", &out]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let csv = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "slot,frame,p1,p2,p3");
    assert_eq!(lines.len(), 1 + 40);
    assert_eq!(lines[1], "9,0,0041,0001,0041");
    let o = run(&[&rpy, "--out", &out]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), csv);
    let o = run(&[&rpy]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(text(&o.stdout), csv);
    assert!(!text(&o.stderr).contains("NO NAME") && !csv.contains("NO NAME"));
}

/// 合成の CSV を gzip にして返す。`finish` が false なら終わりの印の 8 B を落とす（強制終了で切れた形）。
fn gz_bytes(csv: &str, finish: bool) -> Vec<u8> {
    use std::io::Write;
    let mut w =
        higanr_core::gzlog::GzLogWriter::with_sync_interval(Vec::new(), std::time::Duration::ZERO);
    w.write_all(csv.as_bytes()).unwrap();
    w.flush().unwrap();
    let mut v = w.finish().unwrap();
    if !finish {
        v.truncate(v.len() - 8);
    }
    v
}

#[test]
fn compare_reads_a_gzip_csv_like_the_plain_one() {
    let t = Tmp::new("gz");
    let (p1, p2) = words(300);
    let rpy = t.path("m.rpy");
    std::fs::write(
        &rpy,
        build_replay(&BuildSpec::match_replay(0x52, p1.clone(), p2.clone())),
    )
    .unwrap();
    let csv = net_csv8(0, &p1, &p2, 1);
    let plain = t.path("net.csv");
    let gz = t.path("net.csv.gz");
    let cut = t.path("cut.csv.gz");
    std::fs::write(&plain, &csv).unwrap();
    std::fs::write(&gz, gz_bytes(&csv, true)).unwrap();
    std::fs::write(&cut, gz_bytes(&csv, false)).unwrap();
    let p = run(&[&rpy, "--compare", &plain]);
    assert_eq!(p.status.code(), Some(0), "{}", text(&p.stdout));
    for path in [&gz, &cut] {
        let o = run(&[&rpy, "--compare", path]);
        assert_eq!(o.status.code(), Some(0), "{path}: {}", text(&o.stdout));
        assert_eq!(text(&o.stdout), text(&p.stdout), "{path}");
    }
    assert!(!text(&run(&[&rpy, "--compare", &gz]).stderr).contains("終わりの印"));
    assert!(text(&run(&[&rpy, "--compare", &cut]).stderr).contains("終わりの印が無い"));
    let bad = t.path("bad.csv.gz");
    std::fs::write(&bad, [0x1f, 0x8b, 0x08, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).unwrap();
    assert_ne!(run(&[&rpy, "--compare", &bad]).status.code(), Some(0));
}
