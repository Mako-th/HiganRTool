//! 道具 `replay_mark_cmp` を本物のプロセスで走らせる検査（合成のリプレイと合成の印だけ）。
//! 「引数 → フォルダをたどる → 印を読む → 並べる → 終了コード」の配線を見る。比べ方そのものは `replay_mark_cmp` の単体検査。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use higanr_core::replay::{self, Mark, MarkRole, MatchResult, Side, Timestamp};
use higanr_core::replay_build::{BuildSpec, build_replay};
use higanr_core::replay_detail::{MatchDetail, RoundRec, SyncKind, Winner};

const BIN: &str = env!("CARGO_BIN_EXE_replay_mark_cmp");

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("higanr_mark_cmp_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
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

fn round(frames: u32, winner: Winner) -> RoundRec {
    RoundRec {
        frames,
        winner,
        life: [Some(0), Some(4)],
        c23: [Some(1), Some(0)],
        boss: [Some(0), Some(0)],
        rev: [Some(0), Some(0)],
        score: [Some(500), Some(900)],
    }
}

fn rounds(shift: u32) -> Vec<RoundRec> {
    vec![round(1800 + shift, Winner::P2), round(2400, Winner::P2)]
}

/// `start` の秒に始まったマッチの合成のリプレイ（2P の台。結果は自分から見て win）。`p2_word` は 2P の 2 フレーム目の語。
fn replay_bytes(start: u64, rounds: Vec<RoundRec>, p2_word: u16) -> Vec<u8> {
    replay_bytes_with(start, rounds, p2_word, MatchResult::Win)
}

fn replay_bytes_with(
    start: u64,
    rounds: Vec<RoundRec>,
    p2_word: u16,
    result: MatchResult,
) -> Vec<u8> {
    let mut f = build_replay(&BuildSpec::match_replay(
        0x52,
        vec![1, 2, 3, 4],
        vec![0, p2_word, 0, 0],
    ));
    let mark = Mark {
        peer: "peer".to_string(),
        self_name: "self".to_string(),
        side: Side::P2,
        lat: 4,
        ping_ms: 200,
        result,
        at: Timestamp {
            year: 2026,
            month: 10,
            day: 14,
            hour: 12,
            minute: 0,
            second: 0,
        },
        role: MarkRole::Play,
        detail: Some(MatchDetail {
            sync: Some(SyncKind::Rollback),
            start_utc: Some(start),
            rounds,
            ..MatchDetail::default()
        }),
    };
    let chunk = replay::chunk_to_append(&f, &mark).unwrap();
    f.extend(chunk);
    f
}

/// 台のフォルダに「相手の名前のフォルダ」を 1 段挟んで、マッチごとのリプレイを置く（保管庫と同じ形）。
fn put(dir: &Path, n: usize, bytes: &[u8]) {
    let sub = dir.join("x vs y");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(format!("th9_ud{n:04}.rpy")), bytes).unwrap();
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn three_equal_devices_exit_zero_and_a_differing_observer_exits_one() {
    let t = Tmp::new("equal");
    let (a, b, c) = (t.dir("a"), t.dir("b"), t.dir("c"));
    for (i, base) in [1_790_000_000u64, 1_790_000_100].iter().enumerate() {
        put(&a, i + 1, &replay_bytes(*base, rounds(0), 8));
        put(&b, i + 1, &replay_bytes(*base + 1, rounds(0), 8));
        put(&c, i + 1, &replay_bytes(*base + 3, rounds(0), 8));
    }
    let o = run(&[&s(&a), &s(&b), &s(&c)]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    assert!(text(&o.stdout).contains("a = b = c"), "{}", text(&o.stdout));
    assert!(text(&o.stdout).contains("a 2 本 / b 2 本 / c 2 本"));

    // c の 2 マッチ目だけ、1 ラウンド目の長さが 7 フレーム違う。
    put(&c, 2, &replay_bytes(1_790_000_103, rounds(7), 8));
    let o = run(&[&s(&a), &s(&b), &s(&c)]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains("a = b ≠ c"), "{out}");
    assert!(out.contains("r1.長さ 1800 / 1807"), "{out}");
    assert!(out.contains("a と c: 一致 1 本 / 比べた 2 本"), "{out}");
    // 名前・日付は出さない。
    for secret in ["peer", "self", "2026-10-14"] {
        assert!(!out.contains(secret), "{secret} が出ている: {out}");
    }
}

#[test]
fn a_single_file_per_device_and_two_devices_work() {
    let t = Tmp::new("files");
    let (a, b) = (t.dir("a"), t.dir("b"));
    put(&a, 1, &replay_bytes(1_790_000_000, rounds(0), 8));
    put(&b, 1, &replay_bytes(1_790_000_001, rounds(0), 8));
    let fa = a.join("x vs y").join("th9_ud0001.rpy");
    let fb = b.join("x vs y").join("th9_ud0001.rpy");
    let o = run(&[&s(&fa), &s(&fb)]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stdout));
    assert!(text(&o.stdout).contains("a と b: 一致 1 本 / 比べた 1 本"));
}

#[test]
fn since_drops_older_marks_and_unreadable_replays_but_without_it_they_are_errors() {
    let t = Tmp::new("since");
    let (a, b) = (t.dir("a"), t.dir("b"));
    // a には前の回のリプレイ（印なし）と古い start の印、今の回の印がある。
    put(
        &a,
        1,
        &build_replay(&BuildSpec::match_replay(0x52, vec![1], vec![2])),
    );
    put(&a, 2, &replay_bytes(1_700_000_000, rounds(99), 8));
    put(&a, 3, &replay_bytes(1_790_000_000, rounds(0), 8));
    put(&b, 1, &replay_bytes(1_790_000_001, rounds(0), 8));

    let o = run(&[&s(&a), &s(&b)]);
    assert_eq!(o.status.code(), Some(2), "印の無いリプレイは断る");
    assert!(
        text(&o.stderr).contains("印（HGRT）が無い"),
        "{}",
        text(&o.stderr)
    );

    let o = run(&["--since", "1790000000", &s(&a), &s(&b)]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    assert!(
        text(&o.stdout).contains("a 1 本 / b 1 本"),
        "{}",
        text(&o.stdout)
    );
    assert!(
        text(&o.stderr).contains("読めないので除いた"),
        "除いた事実を残す: {}",
        text(&o.stderr)
    );
}

#[test]
fn inputs_flag_compares_the_replay_words_between_devices() {
    let t = Tmp::new("inputs");
    let (a, b) = (t.dir("a"), t.dir("b"));
    put(&a, 1, &replay_bytes(1_790_000_000, rounds(0), 8));
    // 印は同じで、2P の 2 フレーム目の語だけ違う。
    put(&b, 1, &replay_bytes(1_790_000_001, rounds(0), 9));
    let o = run(&[&s(&a), &s(&b)]);
    assert_eq!(o.status.code(), Some(0), "--inputs なしでは印だけを見る");
    let o = run(&["--inputs", &s(&a), &s(&b)]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains("入力が違う") && out.contains("フレーム 1"),
        "{out}"
    );
}

#[test]
fn usage_errors_exit_two() {
    let t = Tmp::new("usage");
    let a = t.dir("a");
    put(&a, 1, &replay_bytes(1_790_000_000, rounds(0), 8));
    assert_eq!(run(&[]).status.code(), Some(2));
    assert_eq!(run(&[&s(&a)]).status.code(), Some(2), "台が 1 つ");
    assert_eq!(
        run(&[&s(&a), &s(&a), &s(&a), &s(&a)]).status.code(),
        Some(2),
        "台が 4 つ"
    );
    assert_eq!(run(&["--bogus", &s(&a), &s(&a)]).status.code(), Some(2));
    assert_eq!(
        run(&["--since", "x", &s(&a), &s(&a)]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(&[&s(&a), "no_such_dir_for_mark_cmp"]).status.code(),
        Some(2)
    );
    let empty = t.dir("empty");
    let o = run(&[&s(&a), &s(&empty)]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).contains("リプレイ（.rpy）が無い"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn skip_abort_drops_the_cut_off_match_and_says_so() {
    let t = Tmp::new("abort");
    let (a, b) = (t.dir("a"), t.dir("b"));
    put(&a, 1, &replay_bytes(1_790_000_000, rounds(0), 8));
    // a だけに、台本の終わりで切れた途中のマッチ（結果 abort。ラウンドは 1 件だけ）がある。
    put(
        &a,
        2,
        &replay_bytes_with(
            1_790_000_100,
            rounds(0)[..1].to_vec(),
            8,
            MatchResult::Abort,
        ),
    );
    put(&b, 1, &replay_bytes(1_790_000_001, rounds(0), 8));
    let o = run(&[&s(&a), &s(&b)]);
    assert_eq!(o.status.code(), Some(1), "本数の差は一致にしない");
    assert!(
        text(&o.stdout).contains("片方にしか無い 1 本"),
        "{}",
        text(&o.stdout)
    );
    let o = run(&["--skip-abort", &s(&a), &s(&b)]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    assert!(
        text(&o.stdout).contains("a 1 本 / b 1 本"),
        "{}",
        text(&o.stdout)
    );
    assert!(
        text(&o.stderr).contains("abort のリプレイ 1 本を除いた"),
        "{}",
        text(&o.stderr)
    );
}
