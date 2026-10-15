//! 道具 `replay_playcheck` を本物のプロセスで走らせる検査（合成のリプレイと合成のログだけ）。
//! 「引数 → ファイルを読む → ログの再生の行を選ぶ → 比べる → 終了コード」の配線を見る。比べ方そのものは `replay_playcheck` の単体検査。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use higanr_core::replay::{self, Mark, MarkRole, MatchResult, Side, Timestamp};
use higanr_core::replay_build::{BuildSpec, build_replay};
use higanr_core::replay_detail::{MatchDetail, RoundRec, Winner};
use higanr_core::replay_playcheck::{Ident, format_line};

const BIN: &str = env!("CARGO_BIN_EXE_replay_playcheck");

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("higanr_playcheck_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
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

fn round(frames: u32, score1: u64) -> RoundRec {
    RoundRec {
        frames,
        winner: Winner::P1,
        life: [Some(2), Some(0)],
        c23: [Some(29), Some(32)],
        boss: [Some(13), Some(13)],
        rev: [Some(12), Some(13)],
        score: [Some(score1), Some(9_296_590)],
    }
}

fn rounds() -> Vec<RoundRec> {
    vec![round(9781, 10_654_030), round(6582, 19_560_280)]
}

fn replay_bytes(rounds: Vec<RoundRec>, marked: bool) -> Vec<u8> {
    let mut f = build_replay(&BuildSpec::match_replay(0x52, vec![1, 2], vec![3, 4]));
    if marked {
        let mark = Mark {
            peer: "peer".to_string(),
            self_name: "self".to_string(),
            side: Side::P1,
            lat: 1,
            ping_ms: 0,
            result: MatchResult::Win,
            at: Timestamp {
                year: 2026,
                month: 10,
                day: 14,
                hour: 1,
                minute: 2,
                second: 3,
            },
            role: MarkRole::Play,
            detail: Some(MatchDetail {
                start_utc: Some(5),
                rounds,
                ..MatchDetail::default()
            }),
        };
        let chunk = replay::chunk_to_append(&f, &mark).unwrap();
        f.extend(chunk);
    }
    f
}

/// 合成のリプレイの見出し（日付 `26/01/01`・名前 `NO NAME`）と同じ。
fn ident() -> Ident {
    Ident {
        date: Some("26/01/01".into()),
        name: Some("NO_NAME".into()),
    }
}

fn log_with(lines: &[String]) -> String {
    format!("起動\n{}\n終了\n", lines.join("\n時刻 21:57:20 の別の行\n"))
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn an_identical_playback_exits_zero_and_hides_the_header_text() {
    let t = Tmp::new("eq");
    let rpy = t.file("th9_ud0008.rpy", &replay_bytes(rounds(), true));
    let log = t.file(
        "dll.log",
        log_with(&[format_line(true, &ident(), &rounds(), 0)]).as_bytes(),
    );
    let o = run(&[&rpy, &log]);
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(
        out.contains("全部一致（2 ラウンド × 12 項目 = 24 項目）"),
        "{out}"
    );
    assert!(out.contains("th9_ud0008.rpy"), "{out}");
    assert!(
        !out.contains("26/01/01") && !out.contains("NO_NAME"),
        "日付と名前は出さない: {out}"
    );
}

#[test]
fn one_differing_item_exits_one_and_names_it() {
    let t = Tmp::new("diff");
    let rpy = t.file("a.rpy", &replay_bytes(rounds(), true));
    let mut played = rounds();
    played[1].score[0] = Some(19_560_310);
    let log = t.file(
        "dll.log",
        log_with(&[format_line(true, &ident(), &played, 0)]).as_bytes(),
    );
    let o = run(&[&rpy, &log]);
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.contains("r2 スコア 1P: 19560280 / 19560310"), "{out}");
}

#[test]
fn the_block_with_the_matching_header_is_the_one_compared() {
    let t = Tmp::new("pick");
    let rpy = t.file("a.rpy", &replay_bytes(rounds(), true));
    let mut other = rounds();
    other[0].life[0] = Some(1);
    let later = Ident {
        date: Some("26/01/02".into()),
        name: Some("NO_NAME".into()),
    };
    let log = t.file(
        "dll.log",
        log_with(&[
            format_line(true, &ident(), &rounds(), 0),
            format_line(true, &later, &other, 0),
        ])
        .as_bytes(),
    );
    let o = run(&[&rpy, &log]);
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("2 件のうち 1 件目"), "{out}");
}

#[test]
fn nothing_to_compare_and_bad_usage_exit_two() {
    let t = Tmp::new("bad");
    let rpy = t.file("a.rpy", &replay_bytes(rounds(), true));
    let unmarked = t.file("u.rpy", &replay_bytes(Vec::new(), false));
    let empty_marked = t.file("e.rpy", &replay_bytes(Vec::new(), true));
    let no_play = t.file("none.log", "再生の行の無いログ\n".as_bytes());
    let log = t.file(
        "dll.log",
        log_with(&[format_line(true, &ident(), &rounds(), 0)]).as_bytes(),
    );
    let missing = s(&t.0.join("missing"));
    for (args, why) in [
        (vec![&rpy[..]], "引数が足りない"),
        (vec![&rpy[..], &log, &log], "引数が多い"),
        (vec![&rpy[..], &no_play], "ログに再生の行が無い"),
        (vec![&unmarked[..], &log], "リプレイに印が無い"),
        (vec![&empty_marked[..], &log], "印のラウンドが 0 件"),
        (vec![&missing[..], &log], "リプレイが無い"),
        (vec![&rpy[..], &missing], "ログが無い"),
    ] {
        let o = run(&args);
        assert_eq!(o.status.code(), Some(2), "{why}: {}", text(&o.stdout));
        assert!(!o.stderr.is_empty(), "{why}: 理由を標準エラーに出す");
    }
}
