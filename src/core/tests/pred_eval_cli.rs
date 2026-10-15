//! 道具 `pred_eval` を本物のプロセスで走らせる検査（合成の `_net_frames.csv` と合成のリプレイだけ）。
//! 「引数 → 席ごとの列 → 採点 → CSV の書き出し → 終了コード」の配線と、出力のフォルダを上書きしないことを見る。

use std::path::PathBuf;
use std::process::{Command, Output};

use higanr_core::replay_build::{BuildSpec, build_replay};

const BIN: &str = env!("CARGO_BIN_EXE_pred_eval");
const LEFT: u16 = 0x40;
const RIGHT: u16 = 0x80;

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("higanr_pred_eval_{tag}_{}", std::process::id()));
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

/// 1P は左右を交互、2P は右を押し続ける 200 フレームの `_net_frames.csv`。
fn net_csv() -> String {
    let mut s = String::from("seg,frame,p1,p2,delay,margin_us,wait_us\n");
    for f in 0..200u32 {
        let a = if f % 2 == 0 { LEFT } else { RIGHT };
        s.push_str(&format!("0,{f},{a:04X},{RIGHT:04X},3,,0\n"));
    }
    s
}

/// `by_m.csv` の行から、列の名前で値を引く。
fn cell(csv: &str, key: &[&str], col: &str) -> String {
    let mut lines = csv.lines();
    let head: Vec<&str> = lines.next().unwrap().split(',').collect();
    let idx = head.iter().position(|h| *h == col).unwrap();
    let want = lines
        .find(|l| {
            let f: Vec<&str> = l.split(',').collect();
            key.iter().enumerate().all(|(i, k)| f[i] == *k)
        })
        .unwrap_or_else(|| panic!("行が無い: {key:?}"));
    want.split(',').nth(idx).unwrap().to_string()
}

#[test]
fn csv_and_replays_are_split_into_seat_streams_and_scored() {
    let t = Tmp::new("score");
    std::fs::write(t.path("n.csv"), net_csv()).unwrap();
    // 人 対 CPU: 1P の列だけが対象。人 対 人: 両席。
    let w: Vec<u16> = (0..100)
        .map(|i| if i % 4 < 2 { LEFT } else { RIGHT })
        .collect();
    let mut cpu = BuildSpec::match_replay(0x52, w.clone(), vec![0; 100]);
    cpu.stages[0].p2_human_cpu = 1;
    std::fs::write(t.path("cpu.rpy"), build_replay(&cpu)).unwrap();
    let hh = BuildSpec::match_replay(0x53, w.clone(), w.clone());
    std::fs::write(t.path("hh.rpy"), build_replay(&hh)).unwrap();

    let out = t.path("out");
    let o = run(&[
        "--out",
        &out,
        "--csv",
        &t.path("n.csv"),
        "--rpy",
        &t.path("cpu.rpy"),
        "--rpy",
        &t.path("hh.rpy"),
        "--skip-check",
    ]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let so = text(&o.stdout);
    assert!(so.contains("net: 列 2 本・400 tick"), "{so}");
    assert!(so.contains("rpy_vs_cpu: 列 1 本・100 tick"), "{so}");
    assert!(so.contains("rpy_vs_human: 列 2 本・200 tick"), "{so}");

    let by_m = std::fs::read_to_string(format!("{out}/by_m.csv")).unwrap();
    // 実回線の 2 席・m = 1: 1P は 199 tick 全部外れ、2P は外れ無し。
    assert_eq!(cell(&by_m, &["online", "net", "P0", "1"], "n"), "398");
    assert_eq!(cell(&by_m, &["online", "net", "P0", "1"], "miss"), "199");
    // m = 2: 交互の列は 2 つ前と同じなので外れ無し。
    assert_eq!(cell(&by_m, &["online", "net", "P0", "2"], "miss"), "0");
    // 人 対 CPU は 1 本・100 tick の m = 1 で 99 tick を採点する。
    assert_eq!(cell(&by_m, &["online", "rpy_vs_cpu", "P0", "1"], "n"), "99");
    assert_eq!(
        cell(&by_m, &["online", "rpy_vs_cpu", "P0", "1"], "streams"),
        "1"
    );
    assert_eq!(
        cell(&by_m, &["online", "rpy_vs_human", "P0", "1"], "streams"),
        "2"
    );

    // 席の取り違え: 外れるのは 1P の列、2P の列は外れない。
    let per = std::fs::read_to_string(format!("{out}/per_stream.csv")).unwrap();
    let seat = |s: &str| cell(&per, &["online", s, "net", "P0", "1"], "miss");
    assert_eq!(
        (seat("net0-seg0-P1").as_str(), seat("net0-seg0-P2").as_str()),
        ("199", "0")
    );

    // 元のファイルが 1 本だけのときは「ほかのファイルで先に学ぶ」ものが無く、warm は online と同じになる
    // （同じファイルの列を学習に混ぜると、学んだ表が使われて外れの数が変わる）。
    let one = t.path("out_one");
    let o1 = run(&["--out", &one, "--csv", &t.path("n.csv"), "--skip-check"]);
    assert!(o1.status.success(), "{}", text(&o1.stderr));
    let by_m1 = std::fs::read_to_string(format!("{one}/by_m.csv")).unwrap();
    for method in ["P2", "P3"] {
        for m in ["1", "4"] {
            assert_eq!(
                cell(&by_m1, &["warm", "net", method, m], "miss"),
                cell(&by_m1, &["online", "net", method, m], "miss"),
                "{method} m={m}"
            );
        }
    }
    // 同じ内容のファイルが 2 本なら、片方で学んだ表をもう片方に使う（交互の列を学んだ P3 は慣れる前の外れが無い）。
    let two = t.path("out_two");
    let n = t.path("n.csv");
    let o2 = run(&["--out", &two, "--csv", &n, "--csv", &n, "--skip-check"]);
    assert!(o2.status.success(), "{}", text(&o2.stderr));
    let by_m2 = std::fs::read_to_string(format!("{two}/by_m.csv")).unwrap();
    let miss = |train: &str| -> u32 {
        cell(&by_m2, &[train, "net", "P3", "1"], "miss")
            .parse()
            .unwrap()
    };
    assert!(
        miss("warm") < miss("online"),
        "{} {}",
        miss("warm"),
        miss("online")
    );
    assert_eq!(
        cell(&by_m2, &["warm", "net", "P0", "1"], "miss"),
        cell(&by_m2, &["online", "net", "P0", "1"], "miss"),
        "学ばない方法は同じ"
    );

    // k・D の表: k = 9・D = 0 は m = 9 を R = 8 で打ち切り、1 − 8/9 ≒ 11.11 % 待つ。
    let kd = std::fs::read_to_string(format!("{out}/by_k_d.csv")).unwrap();
    assert_eq!(cell(&kd, &["online", "net", "P0", "9", "0"], "m_eff"), "8");
    assert_eq!(
        cell(&kd, &["online", "net", "P0", "9", "0"], "wait_pct"),
        "11.11"
    );
    assert_eq!(
        cell(&kd, &["online", "net", "P0", "9", "1"], "wait_pct"),
        "0.00"
    );
    let st = std::fs::read_to_string(format!("{out}/streams.csv")).unwrap();
    assert_eq!(st.lines().count(), 1 + 5);
    assert!(std::path::Path::new(&format!("{out}/per_stream.csv")).exists());

    // 同じ名前のフォルダには書かない。
    let again = run(&["--out", &out, "--csv", &t.path("n.csv"), "--skip-check"]);
    assert_eq!(again.status.code(), Some(2), "{}", text(&again.stderr));
}

#[test]
fn check_against_known_numbers_stops_on_mismatch() {
    let t = Tmp::new("check");
    std::fs::write(t.path("n.csv"), net_csv()).unwrap();
    let out = t.path("out");
    let o = run(&["--out", &out, "--csv", &t.path("n.csv")]);
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stdout));
    let so = text(&o.stdout);
    assert!(
        so.contains("不一致") && so.contains("検算が合わない"),
        "{so}"
    );
    assert!(
        !std::path::Path::new(&out).exists(),
        "検算が合わないときは結果を書かない"
    );
}

#[test]
fn rejects_bad_arguments() {
    for args in [vec![], vec!["--out", "x"], vec!["--bogus"]] {
        assert_eq!(run(&args).status.code(), Some(2), "{args:?}");
    }
    let t = Tmp::new("args");
    std::fs::write(t.path("n.csv"), net_csv()).unwrap();
    for bad in [["--r", "0"], ["--r", "17"], ["--focus-k", "13"]] {
        let o = run(&[
            "--out",
            &t.path("o"),
            "--csv",
            &t.path("n.csv"),
            "--skip-check",
            bad[0],
            bad[1],
        ]);
        assert_eq!(o.status.code(), Some(2), "{bad:?}");
    }
}
