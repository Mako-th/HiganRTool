//! 複数台のリプレイの印（`HGRT`）のラウンドの記録を並べて比べる道具の中身（pure。道具は `src/bin/replay_mark_cmp.rs`）。
//!
//! 印のラウンドの記録（`rN`）は、保存した台がマッチの間に生きたゲームの状態から境目の tick で読んだ値で、リプレイを再生して出した値ではない
//! （`replay_detail`）。rollback の 2 台（a・b）と、確定した入力の列だけで進む観戦者（c。素の花映塚と同じ道を通る）の印を並べると、
//! rollback の生きた状態が素の花映塚と違うか（a = b ≠ c）が分かる。
//!
//! 比べるのは、1P から見たマッチの結果・ラウンドの記録（長さ・勝者・ライフ・C2+C3・ボス・リバーサル・スコア）・記録できなかったラウンドの数。
//! 名前・時刻・通信の要約は台ごとに違って当然なので比べない。`with_inputs` のときは、リプレイの入力の語も台どうしで比べる。
#![forbid(unsafe_code)]

use std::fmt::Write as _;

use crate::replay::{self, MarkRead, MarkRole, MatchResult, Side};
use crate::replay_decode;
use crate::replay_detail::{RoundRec, SyncKind, Winner};

/// リプレイ 1 本の入力（枠ごと）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageWords {
    pub slot: u8,
    pub p1: Vec<u16>,
    pub p2: Vec<u16>,
}

/// 1 台・1 マッチ分の、比べる材料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// ファイル名だけ（フォルダ名には対戦者の名前が入るので持たない）。
    pub file: String,
    pub start_utc: Option<u64>,
    pub sync: Option<SyncKind>,
    /// 1P から見た結果。印の `result` は保存した台から見た結果なので、2P の台は勝ち負けを入れ替える。
    pub p1_result: MatchResult,
    pub rounds: Vec<RoundRec>,
    pub rounds_dropped: u32,
    pub stages: Option<Vec<StageWords>>,
}

/// 1 台ぶん（ラベルと、start の順に並べたマッチ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub label: String,
    pub entries: Vec<Entry>,
}

fn flip(r: MatchResult) -> MatchResult {
    match r {
        MatchResult::Win => MatchResult::Lose,
        MatchResult::Lose => MatchResult::Win,
        other => other,
    }
}

fn result_text(r: MatchResult) -> &'static str {
    match r {
        MatchResult::Win => "win",
        MatchResult::Lose => "lose",
        MatchResult::Draw => "draw",
        MatchResult::Abort => "abort",
    }
}

/// リプレイのバイト列から比べる材料を作る。印が無い・読めない・マッチの記録（`rN` などの欄）が無いときは理由を返す
/// （ラウンドの記録が無い印を黙って「0 ラウンド」として一致させないため）。
pub fn entry_from_bytes(file: &str, bytes: &[u8], with_inputs: bool) -> Result<Entry, String> {
    let mark = match replay::read_mark(bytes)
        .map_err(|e| format!("{file}: リプレイとして読めない（{e:?}）"))?
    {
        MarkRead::Found(m) => m,
        MarkRead::Absent => return Err(format!("{file}: 印（HGRT）が無い")),
        MarkRead::UnknownVersion => {
            return Err(format!("{file}: 知らないバージョンの印だけがある"));
        }
        MarkRead::Broken(e) => return Err(format!("{file}: 印が壊れている（{e:?}）")),
    };
    let Some(detail) = mark.detail else {
        return Err(format!(
            "{file}: 印にマッチの記録（sync・rN など）が無い（古いビルドの印）"
        ));
    };
    let p1_result = match (&mark.role, mark.side) {
        (MarkRole::Play, Side::P2) => flip(mark.result),
        _ => mark.result,
    };
    let stages = if with_inputs {
        let r = replay_decode::decode_inputs(bytes)
            .map_err(|e| format!("{file}: 入力を復号できない（{e:?}）"))?;
        Some(
            r.stages
                .into_iter()
                .map(|s| StageWords {
                    slot: s.slot,
                    p1: s.p1_inputs,
                    p2: s.p2_inputs,
                })
                .collect(),
        )
    } else {
        None
    };
    Ok(Entry {
        file: file.to_string(),
        start_utc: detail.start_utc,
        sync: detail.sync,
        p1_result,
        rounds: detail.rounds,
        rounds_dropped: detail.rounds_dropped,
        stages,
    })
}

/// start（無ければ後ろ）→ ファイル名の順に並べる。同じ台のマッチを時刻の順に k 番目どうしで対にするため。
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        (a.start_utc.is_none(), a.start_utc, &a.file).cmp(&(
            b.start_utc.is_none(),
            b.start_utc,
            &b.file,
        ))
    });
}

fn opt(v: Option<u32>) -> String {
    v.map_or_else(|| "-".to_string(), |x| x.to_string())
}

fn opt64(v: Option<u64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| x.to_string())
}

fn winner_text(w: Winner) -> &'static str {
    match w {
        Winner::P1 => "1",
        Winner::P2 => "2",
        Winner::Unknown => "-",
    }
}

fn round_diffs(i: usize, x: &RoundRec, y: &RoundRec, out: &mut Vec<String>) {
    let tag = format!("r{}", i + 1);
    if x.frames != y.frames {
        out.push(format!("{tag}.長さ {} / {}", x.frames, y.frames));
    }
    if x.winner != y.winner {
        out.push(format!(
            "{tag}.勝者 {} / {}",
            winner_text(x.winner),
            winner_text(y.winner)
        ));
    }
    let pairs = [
        ("ライフ", x.life, y.life),
        ("C2+C3", x.c23, y.c23),
        ("ボス", x.boss, y.boss),
        ("リバーサル", x.rev, y.rev),
    ];
    for (name, xs, ys) in pairs {
        for side in 0..2 {
            if xs[side] != ys[side] {
                out.push(format!(
                    "{tag}.{name} {}P {} / {}",
                    side + 1,
                    opt(xs[side]),
                    opt(ys[side])
                ));
            }
        }
    }
    for side in 0..2 {
        if x.score[side] != y.score[side] {
            out.push(format!(
                "{tag}.スコア {}P {} / {}",
                side + 1,
                opt64(x.score[side]),
                opt64(y.score[side])
            ));
        }
    }
}

/// 印の違い（空なら一致）。「左 / 右」の順に値を並べる。
pub fn mark_diffs(a: &Entry, b: &Entry) -> Vec<String> {
    let mut out = Vec::new();
    if a.p1_result != b.p1_result {
        out.push(format!(
            "1P結果 {} / {}",
            result_text(a.p1_result),
            result_text(b.p1_result)
        ));
    }
    if a.rounds.len() != b.rounds.len() {
        out.push(format!(
            "ラウンド数 {} / {}",
            a.rounds.len(),
            b.rounds.len()
        ));
    }
    for (i, (x, y)) in a.rounds.iter().zip(&b.rounds).enumerate() {
        round_diffs(i, x, y, &mut out);
    }
    if a.rounds_dropped != b.rounds_dropped {
        out.push(format!(
            "記録できなかったラウンド {} / {}",
            a.rounds_dropped, b.rounds_dropped
        ));
    }
    out
}

/// 入力の比べ。`diffs` は共通の範囲で違った所（空なら一致）、`note` は長さなど違いに数えない事実。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputCmp {
    pub diffs: Vec<String>,
    pub note: String,
}

/// 共通のフレームの範囲の語を比べる。長さの違いは数えず `note` に出す（記録の終わりが台ごとに数フレームずれうるため。
/// 数えるかどうかの線はこちらで決めず、数を見せる）。
pub fn input_cmp(a: &[StageWords], b: &[StageWords]) -> InputCmp {
    let mut c = InputCmp::default();
    if a.len() != b.len() {
        c.diffs.push(format!("枠の数 {} / {}", a.len(), b.len()));
    }
    for (x, y) in a.iter().zip(b) {
        if x.slot != y.slot {
            c.diffs.push(format!("枠の番号 {} / {}", x.slot, y.slot));
            continue;
        }
        let common = x.p1.len().min(y.p1.len()).min(x.p2.len()).min(y.p2.len());
        let first = (0..common).find(|&f| x.p1[f] != y.p1[f] || x.p2[f] != y.p2[f]);
        if let Some(f) = first {
            let bad = (f..common)
                .filter(|&k| x.p1[k] != y.p1[k] || x.p2[k] != y.p2[k])
                .count();
            c.diffs.push(format!(
                "枠 {} のフレーム {f}: 1P {:04X} / {:04X}・2P {:04X} / {:04X}（共通の {common} フレームのうち {bad} フレームが違う）",
                x.slot, x.p1[f], y.p1[f], x.p2[f], y.p2[f]
            ));
        }
        let _ = write!(
            c.note,
            "枠 {} の長さ {} / {}（共通の {common} フレームを比べた）。",
            x.slot,
            x.p1.len(),
            y.p1.len()
        );
    }
    c
}

/// 対にした start がこれ以上離れていたら、別のマッチを並べた疑いとして書く（秒。同じ PC の台どうしなら数秒のはず）。
const START_FAR_SEC: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub text: String,
    /// 全部の台に同じ本数（1 以上）があり、全部のマッチが全部の台で一致し、ラウンドの記録が 0 件のマッチも無い。
    pub all_equal: bool,
}

/// 1 組の台の集計。
#[derive(Default, Clone, Copy)]
struct PairTally {
    compared: usize,
    equal: usize,
    /// 片方にしかマッチが無かった本数。
    unpaired: usize,
}

fn entry_line(label: &str, e: &Entry) -> String {
    format!(
        "  {label}: {}  1P結果 {}  同期 {}  start {}  ラウンド {} 件{}",
        e.file,
        result_text(e.p1_result),
        match e.sync {
            Some(SyncKind::Rollback) => "rollback",
            Some(SyncKind::Delay) => "delay",
            None => "-",
        },
        opt64(e.start_utc),
        e.rounds.len(),
        if e.rounds_dropped > 0 {
            format!("（記録できなかった {} 件）", e.rounds_dropped)
        } else {
            String::new()
        }
    )
}

/// 台が 2 つ以上のとき、同じ番目のマッチどうしを並べて比べる。
pub fn report(devs: &[Device], with_inputs: bool) -> Report {
    let mut text = String::new();
    let counts: Vec<String> = devs
        .iter()
        .map(|d| format!("{} {} 本", d.label, d.entries.len()))
        .collect();
    let _ = writeln!(
        text,
        "リプレイの印: {}（台ごとに start の順に並べ、同じ番目どうしを比べる）",
        counts.join(" / ")
    );
    let n_max = devs.iter().map(|d| d.entries.len()).max().unwrap_or(0);
    let mut pairs: Vec<(usize, usize, PairTally)> = Vec::new();
    for i in 0..devs.len() {
        for j in i + 1..devs.len() {
            pairs.push((i, j, PairTally::default()));
        }
    }
    let mut empty_rounds = 0usize;
    let mut inputs_bad = 0usize;
    for k in 0..n_max {
        let _ = writeln!(text, "-- マッチ {} --", k + 1);
        let present: Vec<Option<&Entry>> = devs.iter().map(|d| d.entries.get(k)).collect();
        for (d, e) in devs.iter().zip(&present) {
            match e {
                Some(e) => {
                    let _ = writeln!(text, "{}", entry_line(&d.label, e));
                    if e.rounds.is_empty() {
                        empty_rounds += 1;
                    }
                }
                None => {
                    let _ = writeln!(text, "  {}: （このマッチのリプレイが無い）", d.label);
                }
            }
        }
        let rounds_max = present
            .iter()
            .flatten()
            .map(|e| e.rounds.len())
            .max()
            .unwrap_or(0);
        for r in 0..rounds_max {
            for (d, e) in devs.iter().zip(&present) {
                let v = e
                    .and_then(|e| e.rounds.get(r))
                    .map_or_else(|| "（無い）".to_string(), RoundRec::to_value);
                let _ = writeln!(text, "  r{} {}: {v}", r + 1, d.label);
            }
        }
        let starts: Vec<u64> = present
            .iter()
            .flatten()
            .filter_map(|e| e.start_utc)
            .collect();
        if let (Some(lo), Some(hi)) = (starts.iter().min(), starts.iter().max())
            && hi - lo >= START_FAR_SEC
        {
            let _ = writeln!(
                text,
                "  ※start が {} 秒離れている（別のマッチを並べている疑い。--since で範囲を絞るか、フォルダの中身を確かめる）",
                hi - lo
            );
        }
        for (i, j, t) in &mut pairs {
            let (la, lb) = (&devs[*i].label, &devs[*j].label);
            match (present[*i], present[*j]) {
                (Some(x), Some(y)) => {
                    t.compared += 1;
                    let d = mark_diffs(x, y);
                    if d.is_empty() {
                        t.equal += 1;
                        let _ = writeln!(text, "  {la} と {lb}: 印は一致");
                    } else {
                        let _ = writeln!(text, "  {la} と {lb}: 印が違う — {}", d.join("、"));
                    }
                    if with_inputs && let (Some(sx), Some(sy)) = (&x.stages, &y.stages) {
                        let c = input_cmp(sx, sy);
                        if c.diffs.is_empty() {
                            let _ = writeln!(
                                text,
                                "  {la} と {lb}: 入力は共通の範囲で一致。{}",
                                c.note
                            );
                        } else {
                            inputs_bad += 1;
                            let _ = writeln!(
                                text,
                                "  {la} と {lb}: ★入力が違う — {}",
                                c.diffs.join("、")
                            );
                        }
                    }
                }
                (None, None) => {}
                _ => t.unpaired += 1,
            }
        }
    }
    let _ = writeln!(text, "== まとめ ==");
    let mut all_pairs_equal = n_max > 0;
    for (i, j, t) in &pairs {
        let _ = writeln!(
            text,
            "  {} と {}: 一致 {} 本 / 比べた {} 本（片方にしか無い {} 本）",
            devs[*i].label, devs[*j].label, t.equal, t.compared, t.unpaired
        );
        if t.compared == 0 || t.equal != t.compared || t.unpaired != 0 {
            all_pairs_equal = false;
        }
    }
    if empty_rounds > 0 {
        let _ = writeln!(
            text,
            "  ★ラウンドの記録が 0 件のリプレイが {empty_rounds} 本ある（境目を読めていない。その印は比べられていない）"
        );
    }
    if inputs_bad > 0 {
        let _ = writeln!(text, "  ★入力が違った組が {inputs_bad} 組ある");
    }
    if n_max == 0 {
        let _ = writeln!(text, "  ★どの台にもリプレイが無い（比べられていない）");
    }
    let tally = |a: usize, b: usize| {
        pairs
            .iter()
            .find(|(i, j, _)| (*i, *j) == (a, b))
            .map(|(_, _, t)| *t)
    };
    if devs.len() == 3 {
        let full = |t: Option<PairTally>| {
            t.is_some_and(|t| t.compared > 0 && t.equal == t.compared && t.unpaired == 0)
        };
        let (ab, ac, bc) = (full(tally(0, 1)), full(tally(0, 2)), full(tally(1, 2)));
        let (a, b, c) = (&devs[0].label, &devs[1].label, &devs[2].label);
        let hint = match (ab, ac, bc) {
            (true, true, true) => format!(
                "{a} = {b} = {c}: 3 台の印は一致。生きた状態は台によらず同じなので、再生側の別の原因を疑う"
            ),
            (true, false, false) => format!(
                "{a} = {b} ≠ {c}: 対戦の 2 台は同じで観戦者だけ違う。rollback の生きた状態が素の花映塚と違う疑い"
            ),
            (false, _, _) => format!(
                "{a} ≠ {b}: 対戦の 2 台の間でも違う。先に CHECK と列の一致（peer_sim --compare2）を見る"
            ),
            _ => format!("{a}・{b}・{c} の一致が揃わない組み合わせ。上の組ごとの行を見る"),
        };
        let _ = writeln!(text, "  読み方: {hint}");
    }
    Report {
        text,
        all_equal: all_pairs_equal && empty_rounds == 0 && inputs_bad == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{Mark, Timestamp};
    use crate::replay_build::{BuildSpec, build_replay};
    use crate::replay_detail::MatchDetail;

    fn round(n: u32, winner: Winner) -> RoundRec {
        RoundRec {
            frames: 1500 + n,
            winner,
            life: [Some(0), Some(3)],
            c23: [Some(n), Some(0)],
            boss: [Some(0), Some(0)],
            rev: [Some(0), Some(0)],
            score: [Some(1000 + u64::from(n)), Some(2000)],
        }
    }

    fn entry(file: &str, start: u64, rounds: Vec<RoundRec>) -> Entry {
        Entry {
            file: file.to_string(),
            start_utc: Some(start),
            sync: Some(SyncKind::Rollback),
            p1_result: MatchResult::Lose,
            rounds,
            rounds_dropped: 0,
            stages: None,
        }
    }

    fn three(r1: Vec<RoundRec>) -> Vec<RoundRec> {
        let mut v = vec![round(1, Winner::P2), round(2, Winner::P2)];
        v.extend(r1);
        v
    }

    fn dev(label: &str, entries: Vec<Entry>) -> Device {
        Device {
            label: label.to_string(),
            entries,
        }
    }

    #[test]
    fn identical_rounds_are_equal_and_each_field_is_named_when_it_differs() {
        let a = entry("a.rpy", 100, three(vec![]));
        assert_eq!(mark_diffs(&a, &a.clone()), Vec::<String>::new());

        // 1 欄ずつ変える。欄ごとに別の文言で落ちる（どれかの比べが抜けても 1 つは素通りしない）。
        let mut cases: Vec<(Entry, &str)> = Vec::new();
        let mut b = a.clone();
        b.rounds[1].frames += 1;
        cases.push((b, "r2.長さ 1502 / 1503"));
        let mut b = a.clone();
        b.rounds[0].winner = Winner::P1;
        cases.push((b, "r1.勝者 2 / 1"));
        let mut b = a.clone();
        b.rounds[0].life[1] = Some(2);
        cases.push((b, "r1.ライフ 2P 3 / 2"));
        let mut b = a.clone();
        b.rounds[1].c23[0] = None;
        cases.push((b, "r2.C2+C3 1P 2 / -"));
        let mut b = a.clone();
        b.rounds[0].boss[0] = Some(1);
        cases.push((b, "r1.ボス 1P 0 / 1"));
        let mut b = a.clone();
        b.rounds[0].rev[1] = Some(4);
        cases.push((b, "r1.リバーサル 2P 0 / 4"));
        let mut b = a.clone();
        b.rounds[1].score[1] = Some(2010);
        cases.push((b, "r2.スコア 2P 2000 / 2010"));
        let mut b = a.clone();
        b.rounds.pop();
        cases.push((b, "ラウンド数 2 / 1"));
        let mut b = a.clone();
        b.p1_result = MatchResult::Win;
        cases.push((b, "1P結果 lose / win"));
        let mut b = a.clone();
        b.rounds_dropped = 1;
        cases.push((b, "記録できなかったラウンド 0 / 1"));
        for (b, want) in cases {
            let d = mark_diffs(&a, &b);
            assert!(d.iter().any(|x| x == want), "{want:?} が出ない: {d:?}");
        }
    }

    #[test]
    fn names_times_and_comm_summary_are_not_compared() {
        let a = entry("a.rpy", 100, three(vec![]));
        let mut b = a.clone();
        b.file = "b.rpy".to_string();
        b.start_utc = Some(105);
        b.sync = None;
        assert!(mark_diffs(&a, &b).is_empty());
    }

    #[test]
    fn three_devices_equal_give_the_replay_side_hint_and_a_zero_exit() {
        let m = |f: &str, s| entry(f, s, three(vec![]));
        let devs = [
            dev("a", vec![m("a1.rpy", 100), m("a2.rpy", 200)]),
            dev("b", vec![m("b1.rpy", 101), m("b2.rpy", 201)]),
            dev("c", vec![m("c1.rpy", 103), m("c2.rpy", 203)]),
        ];
        let r = report(&devs, false);
        assert!(r.all_equal, "{}", r.text);
        assert!(r.text.contains("a = b = c"), "{}", r.text);
        assert!(
            r.text.contains("a と b: 一致 2 本 / 比べた 2 本"),
            "{}",
            r.text
        );
    }

    #[test]
    fn only_the_observer_differing_gives_the_h1_hint() {
        let m = |f: &str, s| entry(f, s, three(vec![]));
        let mut bad = m("c2.rpy", 203);
        bad.rounds[0].frames += 7;
        let devs = [
            dev("a", vec![m("a1.rpy", 100), m("a2.rpy", 200)]),
            dev("b", vec![m("b1.rpy", 101), m("b2.rpy", 201)]),
            dev("c", vec![m("c1.rpy", 103), bad]),
        ];
        let r = report(&devs, false);
        assert!(!r.all_equal);
        assert!(r.text.contains("a = b ≠ c"), "{}", r.text);
        assert!(
            r.text.contains("a と c: 一致 1 本 / 比べた 2 本"),
            "{}",
            r.text
        );
        assert!(r.text.contains("r1.長さ 1501 / 1508"), "{}", r.text);
    }

    #[test]
    fn the_two_players_differing_is_not_read_as_an_observer_problem() {
        let m = |f: &str, s| entry(f, s, three(vec![]));
        let mut bad = m("b1.rpy", 101);
        bad.rounds[1].score[0] = Some(5);
        let devs = [
            dev("a", vec![m("a1.rpy", 100)]),
            dev("b", vec![bad]),
            dev("c", vec![m("c1.rpy", 103)]),
        ];
        let r = report(&devs, false);
        assert!(!r.all_equal);
        assert!(r.text.contains("a ≠ b"), "{}", r.text);
        assert!(!r.text.contains("a = b ≠ c"), "{}", r.text);
    }

    #[test]
    fn a_missing_replay_or_no_rounds_is_never_an_all_equal() {
        let m = |f: &str, s| entry(f, s, three(vec![]));
        // 片方の台のマッチが少ない。
        let r = report(
            &[
                dev("a", vec![m("a1.rpy", 100), m("a2.rpy", 200)]),
                dev("b", vec![m("b1.rpy", 101)]),
            ],
            false,
        );
        assert!(!r.all_equal);
        assert!(r.text.contains("片方にしか無い 1 本"), "{}", r.text);
        assert!(r.text.contains("このマッチのリプレイが無い"), "{}", r.text);
        // 両方ともラウンドの記録が 0 件（空どうしが等しいだけ）。
        let e = |f: &str| entry(f, 100, vec![]);
        let r = report(
            &[dev("a", vec![e("a1.rpy")]), dev("b", vec![e("b1.rpy")])],
            false,
        );
        assert!(!r.all_equal, "{}", r.text);
        assert!(r.text.contains("ラウンドの記録が 0 件"), "{}", r.text);
        // どの台にも無い。
        let r = report(&[dev("a", vec![]), dev("b", vec![])], false);
        assert!(!r.all_equal);
        assert!(r.text.contains("比べられていない"), "{}", r.text);
    }

    #[test]
    fn far_apart_starts_are_flagged_as_a_possible_mispairing() {
        let m = |f: &str, s| entry(f, s, three(vec![]));
        let r = report(
            &[
                dev("a", vec![m("a1.rpy", 100)]),
                dev("b", vec![m("b1.rpy", 100 + START_FAR_SEC)]),
            ],
            false,
        );
        assert!(r.text.contains("離れている"), "{}", r.text);
        let r = report(
            &[
                dev("a", vec![m("a1.rpy", 100)]),
                dev("b", vec![m("b1.rpy", 100 + START_FAR_SEC - 1)]),
            ],
            false,
        );
        assert!(!r.text.contains("離れている"), "{}", r.text);
    }

    #[test]
    fn sorting_puts_the_earlier_start_first_and_unknown_starts_last() {
        let mut v = vec![
            entry("z.rpy", 300, vec![]),
            Entry {
                start_utc: None,
                ..entry("a.rpy", 0, vec![])
            },
            entry("y.rpy", 100, vec![]),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(names, ["y.rpy", "z.rpy", "a.rpy"]);
    }

    fn stage(p1: Vec<u16>, p2: Vec<u16>) -> StageWords {
        StageWords { slot: 9, p1, p2 }
    }

    #[test]
    fn inputs_are_compared_on_the_common_range_and_the_length_gap_is_only_a_note() {
        let a = vec![stage(vec![1, 2, 3, 4], vec![0, 0, 8, 0])];
        let same_longer = vec![stage(vec![1, 2, 3, 4, 5], vec![0, 0, 8, 0, 0])];
        let c = input_cmp(&a, &same_longer);
        assert!(c.diffs.is_empty(), "{c:?}");
        assert!(c.note.contains("長さ 4 / 5"), "{c:?}");
        // 2P の語が 1 フレームだけ違う。
        let b = vec![stage(vec![1, 2, 3, 4], vec![0, 0, 9, 0])];
        let c = input_cmp(&a, &b);
        assert_eq!(c.diffs.len(), 1, "{c:?}");
        assert!(
            c.diffs[0].contains("フレーム 2") && c.diffs[0].contains("2P 0008 / 0009"),
            "{c:?}"
        );
        // 枠の数が違う。
        assert!(!input_cmp(&a, &[]).diffs.is_empty());
    }

    // ---- バイト列から ----

    fn mark(side: Side, result: MatchResult, role: MarkRole, rounds: Vec<RoundRec>) -> Mark {
        Mark {
            peer: "peer".to_string(),
            self_name: "self".to_string(),
            side,
            lat: 1,
            ping_ms: 10,
            result,
            at: Timestamp {
                year: 2026,
                month: 10,
                day: 14,
                hour: 1,
                minute: 2,
                second: 3,
            },
            role,
            detail: Some(MatchDetail {
                sync: Some(SyncKind::Rollback),
                start_utc: Some(1_790_000_000),
                rounds,
                ..MatchDetail::default()
            }),
        }
    }

    fn replay_with(m: &Mark) -> Vec<u8> {
        let mut f = build_replay(&BuildSpec::match_replay(0x52, vec![1, 2, 3], vec![0, 8, 0]));
        f.extend(replay::chunk_to_append(&f, m).unwrap());
        f
    }

    #[test]
    fn a_two_p_replay_is_read_from_the_first_players_side() {
        let rounds = vec![round(1, Winner::P1)];
        // 2P の台の「win」は 1P から見れば lose。観戦者の印は元から 1P から見た結果。
        let p2_win = replay_with(&mark(
            Side::P2,
            MatchResult::Win,
            MarkRole::Play,
            rounds.clone(),
        ));
        let p1_lose = replay_with(&mark(
            Side::P1,
            MatchResult::Lose,
            MarkRole::Play,
            rounds.clone(),
        ));
        let watch = replay_with(&mark(
            Side::P1,
            MatchResult::Lose,
            MarkRole::Watch {
                p1: "p1".to_string(),
                p2: "p2".to_string(),
            },
            rounds,
        ));
        for (name, bytes) in [("b", &p2_win), ("a", &p1_lose), ("c", &watch)] {
            let e = entry_from_bytes(name, bytes, true).unwrap();
            assert_eq!(e.p1_result, MatchResult::Lose, "{name}");
            assert_eq!(e.rounds.len(), 1, "{name}");
            assert_eq!(e.start_utc, Some(1_790_000_000), "{name}");
            let st = e.stages.expect("入力を読む指定");
            assert_eq!(st[0].p1, vec![1, 2, 3], "{name}");
        }
        assert_eq!(entry_from_bytes("a", &p1_lose, false).unwrap().stages, None);
    }

    #[test]
    fn replays_without_a_mark_or_without_the_match_record_are_refused_with_a_reason() {
        let plain = build_replay(&BuildSpec::match_replay(0x52, vec![1], vec![2]));
        let e = entry_from_bytes("x.rpy", &plain, false).unwrap_err();
        assert!(e.contains("印（HGRT）が無い"), "{e}");
        let mut m = mark(Side::P1, MatchResult::Win, MarkRole::Play, vec![]);
        m.detail = None;
        let e = entry_from_bytes("y.rpy", &replay_with(&m), false).unwrap_err();
        assert!(e.contains("マッチの記録"), "{e}");
        assert!(entry_from_bytes("z.rpy", b"not a replay", false).is_err());
    }
}
