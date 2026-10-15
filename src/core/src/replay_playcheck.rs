//! 素の花映塚でリプレイを再生したとき、生きた試合どおりに再生できたかを、印（`HGRT`）の `rN` と再生中に読んだ同じ tick の値で突き合わせる
//! （pure。道具は `src/bin/replay_playcheck.rs`、再生中の値を積む側は dll の `replay_watch`）。
//!
//! 再生の側は、各ラウンドの決着の tick（`completed_rounds` が増えた tick）の値を生きた試合と同じ読み方（`MatchLog`）で積み、再生を出たときに
//! ログへ 1 行を書く（`format_line`）。書式は印の `rN` の値と同じ 12 欄（`replay_detail::RoundRec`）。
//!
//! ```text
//! 再生の照合: 最後まで再生 end=complete date=26/10/14 name=12:34:56 rounds=2 r1=<12 欄> r2=<12 欄>
//! ```
//!
//! 行ごとにまとまっているので、別のスレッドの行が間に入っても読める。`date` / `name` は再生した見出し（`+0xC4` / `+0xCE`）の文字で、読めなかった
//! ときは書かない。
#![forbid(unsafe_code)]

use std::fmt::Write as _;

use crate::replay_decode;
use crate::replay_detail::{MAX_ROUND_KEYS, RoundRec, Winner};
use crate::replay_mark_cmp;

/// 再生の行の目印。ログの行の先頭に別の字（時刻など）が付いても、この語から後ろを読む。
pub const PLAY_MARK: &str = "再生の照合:";
/// 展開した本体（`replay_decode::decode_image`）の中の、見出しの日付（`yy/mm/dd` + NUL の 9 バイト）と名前（9 バイト）の位置。
/// 形式は保存関数（th09.exe）が書くもの。
pub const HEADER_DATE_OFFSET: usize = 0xC4;
pub const HEADER_NAME_OFFSET: usize = 0xCE;
pub const HEADER_FIELD_LEN: usize = 9;

/// 見出しの 9 バイトの欄を行に書ける字にする。NUL で終わる 1〜8 字の ASCII の見える字（空白は `_`）だけを受け、
/// NUL が無い・空・見えない字や ASCII 以外（Shift-JIS の名前）を含むものは `None`。
pub fn header_field(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|&b| b == 0)?;
    if end == 0 || end >= HEADER_FIELD_LEN {
        return None;
    }
    let mut out = String::with_capacity(end);
    for &b in &raw[..end] {
        match b {
            b' ' => out.push('_'),
            0x21..=0x7E => out.push(char::from(b)),
            _ => return None,
        }
    }
    Some(out)
}

/// どのリプレイかを見分ける見出しの文字。両方そろったときだけ見分けに使う。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ident {
    pub date: Option<String>,
    pub name: Option<String>,
}

impl Ident {
    pub fn from_header(date_raw: &[u8], name_raw: &[u8]) -> Ident {
        Ident {
            date: header_field(date_raw),
            name: header_field(name_raw),
        }
    }

    pub fn is_known(&self) -> bool {
        self.date.is_some() && self.name.is_some()
    }
}

/// ログの 1 行（1 回の再生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayBlock {
    /// 何行目か（1 から）。
    pub line: usize,
    pub ident: Ident,
    /// 勝ちに要る数に届くまで再生した（false は途中で抜けた）。
    pub complete: bool,
    pub rounds: Vec<RoundRec>,
    pub rounds_dropped: u32,
}

/// 再生を出たときに書く 1 行。
pub fn format_line(
    complete: bool,
    ident: &Ident,
    rounds: &[RoundRec],
    rounds_dropped: u32,
) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{PLAY_MARK} {} end={}",
        if complete {
            "最後まで再生"
        } else {
            "途中で終わった"
        },
        if complete { "complete" } else { "partial" }
    );
    if let Some(d) = &ident.date {
        let _ = write!(s, " date={d}");
    }
    if let Some(n) = &ident.name {
        let _ = write!(s, " name={n}");
    }
    let _ = write!(s, " rounds={}", rounds.len());
    if rounds_dropped > 0 {
        let _ = write!(s, " rx={rounds_dropped}");
    }
    for (i, r) in rounds.iter().enumerate() {
        let _ = write!(s, " r{}={}", i + 1, r.to_value());
    }
    s
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedLog {
    pub blocks: Vec<PlayBlock>,
    /// 目印はあるが読めなかった行（行番号, 理由）。
    pub bad: Vec<(usize, String)>,
}

fn parse_line(line_no: usize, rest: &str) -> Result<PlayBlock, String> {
    let mut end = None;
    let mut ident = Ident::default();
    let mut declared = None;
    let mut dropped = 0u32;
    let mut rounds: Vec<RoundRec> = Vec::new();
    for tok in rest.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else {
            continue;
        };
        match k {
            "end" => {
                end = Some(match v {
                    "complete" => true,
                    "partial" => false,
                    _ => return Err(format!("end の値 {v:?} を読めない")),
                });
            }
            "date" => ident.date = Some(v.to_string()),
            "name" => ident.name = Some(v.to_string()),
            "rounds" => {
                declared = Some(v.parse::<usize>().map_err(|_| "rounds が数でない")?);
            }
            "rx" => dropped = v.parse().map_err(|_| "rx が数でない")?,
            _ => {
                let Some(n) = k.strip_prefix('r').and_then(|n| n.parse::<usize>().ok()) else {
                    continue;
                };
                if n != rounds.len() + 1 || n > MAX_ROUND_KEYS {
                    return Err(format!("{k} の並びが途切れている"));
                }
                let r = RoundRec::parse(v.as_bytes())
                    .ok_or_else(|| format!("{k} の値を読めない（12 欄でない・数字でない）"))?;
                rounds.push(r);
            }
        }
    }
    let complete = end.ok_or("end= が無い")?;
    if declared != Some(rounds.len()) {
        return Err(format!(
            "rounds= の数 {declared:?} と r の数 {} が合わない（行が切れた疑い）",
            rounds.len()
        ));
    }
    Ok(PlayBlock {
        line: line_no,
        ident,
        complete,
        rounds,
        rounds_dropped: dropped,
    })
}

/// ログの全文から再生の行を集める。目印の無い行は読み飛ばす。
pub fn parse_log(text: &str) -> ParsedLog {
    let mut out = ParsedLog::default();
    for (i, line) in text.lines().enumerate() {
        let Some(at) = line.find(PLAY_MARK) else {
            continue;
        };
        match parse_line(i + 1, &line[at + PLAY_MARK.len()..]) {
            Ok(b) => out.blocks.push(b),
            Err(why) => out.bad.push((i + 1, why)),
        }
    }
    out
}

/// 比べる側のリプレイ（印の `rN` と見出しの文字）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefReplay {
    /// ファイル名だけ（フォルダ名には対戦者の名前が入る）。
    pub file: String,
    pub ident: Ident,
    pub rounds: Vec<RoundRec>,
    pub rounds_dropped: u32,
}

/// リプレイのバイト列から比べる材料を作る。印が無い・読めない・ラウンドの記録が 0 件のときは理由を返す（0 件どうしを黙って一致にしない）。
/// 見出しが復号できないときは見出しの文字だけ無しにする（印があれば比べられる）。
pub fn ref_from_bytes(file: &str, bytes: &[u8]) -> Result<RefReplay, String> {
    let e = replay_mark_cmp::entry_from_bytes(file, bytes, false)?;
    if e.rounds.is_empty() {
        return Err(format!(
            "{file}: 印のラウンドの記録が 0 件（比べる材料が無い）"
        ));
    }
    let ident = replay_decode::decode_image(bytes)
        .ok()
        .and_then(|img| {
            let d = img.get(HEADER_DATE_OFFSET..HEADER_DATE_OFFSET + HEADER_FIELD_LEN)?;
            let n = img.get(HEADER_NAME_OFFSET..HEADER_NAME_OFFSET + HEADER_FIELD_LEN)?;
            Some(Ident::from_header(d, n))
        })
        .unwrap_or_default();
    Ok(RefReplay {
        file: file.to_string(),
        ident,
        rounds: e.rounds,
        rounds_dropped: e.rounds_dropped,
    })
}

/// 複数のブロックから比べる 1 つを選んだ根拠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickBy {
    /// 見出しの日付と名前が両方そろって合うもの。
    Header,
    /// 見出しでは見分けられず、ラウンドの数と各ラウンドの長さが合うもの。
    RoundLengths,
    /// どれも合わず、最後のブロック。
    LastBlock,
}

impl PickBy {
    pub fn text(self) -> &'static str {
        match self {
            PickBy::Header => "見出し（日付・名前）が合うもの",
            PickBy::RoundLengths => "見出しで見分けられず、ラウンドの数と長さが合うもの",
            PickBy::LastBlock => {
                "見出しもラウンドの長さも合うものが無いので最後のブロック（別のリプレイを比べている疑い）"
            }
        }
    }
}

/// 比べるブロックを選ぶ。各段で複数あれば最後（一番新しい再生）。ブロックが無ければ `None`。
pub fn pick(r: &RefReplay, blocks: &[PlayBlock]) -> Option<(usize, PickBy)> {
    if r.ident.is_known()
        && let Some(i) = blocks.iter().rposition(|b| b.ident == r.ident)
    {
        return Some((i, PickBy::Header));
    }
    let same_lengths = |b: &PlayBlock| {
        b.rounds.len() == r.rounds.len()
            && b.rounds
                .iter()
                .zip(&r.rounds)
                .all(|(x, y)| x.frames == y.frames)
    };
    if let Some(i) = blocks.iter().rposition(same_lengths) {
        return Some((i, PickBy::RoundLengths));
    }
    blocks.len().checked_sub(1).map(|i| (i, PickBy::LastBlock))
}

/// 1 項目の違い。`expected` は印、`actual` は再生。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// 何ラウンド目か（1 から）。0 はマッチ全体の項目。
    pub round: usize,
    pub item: String,
    pub expected: String,
    pub actual: String,
}

/// 1 ラウンドで比べる項目の数（長さ・勝者・ライフ・C2+C3・ボス・リバーサル・スコアの 1P / 2P）。
pub const ITEMS_PER_ROUND: usize = 12;

fn opt32(v: Option<u32>) -> String {
    v.map_or_else(|| "-".to_string(), |x| x.to_string())
}

fn opt64(v: Option<u64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| x.to_string())
}

fn winner_text(w: Winner) -> &'static str {
    match w {
        Winner::P1 => "1P",
        Winner::P2 => "2P",
        Winner::Unknown => "-",
    }
}

fn round_diffs(n: usize, x: &RoundRec, y: &RoundRec, out: &mut Vec<Diff>) {
    let mut put = |item: String, e: String, a: String| {
        if e != a {
            out.push(Diff {
                round: n,
                item,
                expected: e,
                actual: a,
            });
        }
    };
    put("長さ".into(), x.frames.to_string(), y.frames.to_string());
    put(
        "勝者".into(),
        winner_text(x.winner).into(),
        winner_text(y.winner).into(),
    );
    let pairs = [
        ("ライフ", x.life, y.life),
        ("C2+C3", x.c23, y.c23),
        ("ボス", x.boss, y.boss),
        ("リバーサル", x.rev, y.rev),
    ];
    for (name, xs, ys) in pairs {
        for side in 0..2 {
            put(
                format!("{name} {}P", side + 1),
                opt32(xs[side]),
                opt32(ys[side]),
            );
        }
    }
    for side in 0..2 {
        put(
            format!("スコア {}P", side + 1),
            opt64(x.score[side]),
            opt64(y.score[side]),
        );
    }
}

/// 比べた結果。`items` は比べた項目の数（共通のラウンド × `ITEMS_PER_ROUND`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub items: usize,
    pub diffs: Vec<Diff>,
}

impl Comparison {
    pub fn is_equal(&self) -> bool {
        self.diffs.is_empty()
    }
}

/// 印と再生のブロックを項目ごとに比べる。ラウンド数が違えば共通のラウンドだけ項目を比べ、数の違いも 1 項目として足す。
pub fn compare(r: &RefReplay, b: &PlayBlock) -> Comparison {
    let mut diffs = Vec::new();
    if r.rounds.len() != b.rounds.len() {
        diffs.push(Diff {
            round: 0,
            item: "ラウンド数".into(),
            expected: r.rounds.len().to_string(),
            actual: b.rounds.len().to_string(),
        });
    }
    let common = r.rounds.len().min(b.rounds.len());
    for (i, (x, y)) in r.rounds.iter().zip(&b.rounds).enumerate() {
        round_diffs(i + 1, x, y, &mut diffs);
    }
    if r.rounds_dropped != b.rounds_dropped {
        diffs.push(Diff {
            round: 0,
            item: "記録できなかったラウンド".into(),
            expected: r.rounds_dropped.to_string(),
            actual: b.rounds_dropped.to_string(),
        });
    }
    Comparison {
        items: common * ITEMS_PER_ROUND,
        diffs,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub text: String,
    /// 比べられて、全部一致した。
    pub equal: bool,
}

/// 道具の本体: ログの全文とリプレイから、選んで・比べて・文にする。再生の行が 1 つも読めなければ `Err`（測れていない）。
pub fn run(r: &RefReplay, log: &str) -> Result<Report, String> {
    let parsed = parse_log(log);
    let mut text = String::new();
    for (line, why) in &parsed.bad {
        let _ = writeln!(
            text,
            "注意: ログの {line} 行目の再生の行を読めない（{why}）"
        );
    }
    let Some((idx, by)) = pick(r, &parsed.blocks) else {
        return Err(if parsed.bad.is_empty() {
            format!(
                "ログに再生の行（{PLAY_MARK}）が無い（再生の見張りが入っていない版か、最初のラウンドが決着する前に再生を出た）。比べられない"
            )
        } else {
            format!("{text}ログの再生の行がどれも読めない。比べられない")
        });
    };
    let b = &parsed.blocks[idx];
    let c = compare(r, b);
    let _ = writeln!(
        text,
        "リプレイ {}: 印のラウンド {} 件",
        r.file,
        r.rounds.len()
    );
    let _ = writeln!(
        text,
        "ログの再生の行 {} 件のうち {} 件目（{} 行目）を比べた。選び方: {}",
        parsed.blocks.len(),
        idx + 1,
        b.line,
        by.text()
    );
    if !b.complete {
        let _ = writeln!(
            text,
            "この再生は途中で終わった（勝ちに要る数に届く前に試合の場面を出た）。ラウンドが足りないのは当然の可能性がある"
        );
    }
    if c.is_equal() {
        let _ = writeln!(
            text,
            "結果: 全部一致（{} ラウンド × {ITEMS_PER_ROUND} 項目 = {} 項目）",
            r.rounds.len(),
            c.items
        );
    } else {
        let _ = writeln!(
            text,
            "結果: 違いあり（共通の {} 項目のうち、違う項目と数の違いが {} 件）。左が印、右が再生:",
            c.items,
            c.diffs.len()
        );
        for d in &c.diffs {
            if d.round == 0 {
                let _ = writeln!(text, "  {}: {} / {}", d.item, d.expected, d.actual);
            } else {
                let _ = writeln!(
                    text,
                    "  r{} {}: {} / {}",
                    d.round, d.item, d.expected, d.actual
                );
            }
        }
    }
    Ok(Report {
        text,
        equal: c.is_equal(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{self, Mark, MarkRole, MatchResult, Side, Timestamp};
    use crate::replay_build::{BuildSpec, build_replay};
    use crate::replay_detail::MatchDetail;

    fn rec(frames: u32, winner: Winner, score1: u64) -> RoundRec {
        RoundRec {
            frames,
            winner,
            life: [Some(2), Some(0)],
            c23: [Some(29), Some(32)],
            boss: [Some(13), Some(13)],
            rev: [Some(12), Some(13)],
            score: [Some(score1), Some(9_296_590)],
        }
    }

    fn two_rounds() -> Vec<RoundRec> {
        vec![
            rec(9781, Winner::P1, 10_654_030),
            rec(6582, Winner::P1, 19_560_280),
        ]
    }

    fn ident(date: &str, name: &str) -> Ident {
        Ident {
            date: Some(date.into()),
            name: Some(name.into()),
        }
    }

    /// 合成のリプレイ（見出しの日付は `26/01/01`、名前は `NO NAME`）に、`rounds` の印を付けたもの。
    fn replay_bytes(rounds: Vec<RoundRec>) -> Vec<u8> {
        let mut f = build_replay(&BuildSpec::match_replay(0x52, vec![1, 2], vec![3, 4]));
        let mark = Mark {
            peer: "p".into(),
            self_name: "s".into(),
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
        f
    }

    fn block(id: &Ident, complete: bool, rounds: &[RoundRec]) -> String {
        format_line(complete, id, rounds, 0)
    }

    #[test]
    fn header_fields_keep_visible_ascii_and_refuse_the_rest() {
        assert_eq!(header_field(b"26/10/14\0"), Some("26/10/14".to_string()));
        assert_eq!(header_field(b"12:34:56\0"), Some("12:34:56".to_string()));
        assert_eq!(header_field(b"NO NAME\0\0"), Some("NO_NAME".to_string()));
        assert_eq!(header_field(b"\0garbage!!"), None, "空");
        assert_eq!(header_field(b"12345678X"), None, "NUL が無い");
        assert_eq!(header_field(b"ab\x01cd\0\0\0\0"), None, "見えない字");
        assert_eq!(header_field(&[0x93, 0xFA, 0x96, 0x7B, 0, 0, 0, 0, 0]), None);
        assert_eq!(header_field(b"12 34\x00\x7f\0"), Some("12_34".to_string()));
    }

    #[test]
    fn a_line_round_trips_and_keeps_the_mark_form() {
        let id = ident("26/10/14", "12:34:56");
        let line = format_line(true, &id, &two_rounds(), 0);
        assert!(line.starts_with(PLAY_MARK));
        assert!(line.contains(" r1=9781.1.2.0.29.32.13.13.12.13.10654030.9296590"));
        // 別の字が先頭に付いても読める（ログの書き手が時刻などを足す場合）。
        let p = parse_log(&format!("noise\n[00:01] {line}\nnoise\n"));
        assert!(p.bad.is_empty());
        assert_eq!(
            p.blocks,
            vec![PlayBlock {
                line: 2,
                ident: id,
                complete: true,
                rounds: two_rounds(),
                rounds_dropped: 0,
            }]
        );
        let partial = format_line(false, &Ident::default(), &two_rounds()[..1], 3);
        assert!(partial.contains("途中で終わった end=partial"));
        let p = parse_log(&partial);
        assert_eq!(p.blocks.len(), 1);
        assert!(!p.blocks[0].complete);
        assert_eq!(p.blocks[0].rounds_dropped, 3);
        assert_eq!(p.blocks[0].ident, Ident::default());
    }

    #[test]
    fn damaged_lines_are_reported_not_silently_dropped() {
        let good = format_line(true, &Ident::default(), &two_rounds(), 0);
        for (bad, why) in [
            (good.replace(" r2=", " r3="), "並び"),
            (good.replace("rounds=2", "rounds=3"), "合わない"),
            (good.replace("end=complete", "end=maybe"), "end"),
            (good.replace("end=complete", ""), "end="),
            (good.replace(".13.12.13.", ".13.x.13."), "12 欄でない"),
            (good[..good.len() - 9].to_string(), "読めない"),
        ] {
            let p = parse_log(&bad);
            assert!(p.blocks.is_empty(), "{bad}");
            assert_eq!(p.bad.len(), 1, "{bad}");
            assert!(p.bad[0].1.contains(why), "{why}: {:?}", p.bad[0]);
        }
    }

    #[test]
    fn identical_rounds_are_all_equal_and_the_item_count_is_stated() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let log = block(&Ident::default(), true, &two_rounds());
        let rep = run(&r, &log).unwrap();
        assert!(rep.equal, "{}", rep.text);
        assert!(
            rep.text
                .contains("全部一致（2 ラウンド × 12 項目 = 24 項目）")
        );
    }

    #[test]
    fn one_changed_item_is_named_with_both_values() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let mut played = two_rounds();
        played[1].score[0] = Some(19_560_310);
        let rep = run(&r, &block(&Ident::default(), true, &played)).unwrap();
        assert!(!rep.equal);
        assert!(
            rep.text.contains("r2 スコア 1P: 19560280 / 19560310"),
            "{}",
            rep.text
        );
        assert_eq!(
            rep.text.matches("  r").count(),
            1,
            "違うのはその 1 項目だけ"
        );
        // 項目ごと: 長さ・勝者・各カウンタの 1P / 2P・不明の値も別の項目として出る。
        let mut p = two_rounds();
        p[0].frames += 1;
        p[0].winner = Winner::P2;
        p[0].life[1] = None;
        p[0].c23[1] = Some(33);
        p[1].boss[0] = Some(14);
        p[1].rev[1] = Some(0);
        let c = compare(
            &RefReplay {
                file: "x".into(),
                ident: Ident::default(),
                rounds: two_rounds(),
                rounds_dropped: 0,
            },
            &PlayBlock {
                line: 1,
                ident: Ident::default(),
                complete: true,
                rounds: p,
                rounds_dropped: 0,
            },
        );
        let items: Vec<(usize, &str)> =
            c.diffs.iter().map(|d| (d.round, d.item.as_str())).collect();
        assert_eq!(
            items,
            vec![
                (1, "長さ"),
                (1, "勝者"),
                (1, "ライフ 2P"),
                (1, "C2+C3 2P"),
                (2, "ボス 1P"),
                (2, "リバーサル 2P"),
            ]
        );
        assert_eq!(c.diffs[2].actual, "-");
    }

    #[test]
    fn a_different_round_count_is_one_item_and_the_common_rounds_are_still_compared() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let mut one = two_rounds()[..1].to_vec();
        one[0].life[0] = Some(1);
        let rep = run(&r, &block(&Ident::default(), false, &one)).unwrap();
        assert!(!rep.equal);
        assert!(rep.text.contains("ラウンド数: 2 / 1"), "{}", rep.text);
        assert!(rep.text.contains("r1 ライフ 1P: 2 / 1"), "{}", rep.text);
        assert!(rep.text.contains("途中で終わった"), "{}", rep.text);
        assert!(rep.text.contains("共通の 12 項目"), "{}", rep.text);
        // 余分に積んだ再生も数の違い。
        let mut three = two_rounds();
        three.push(rec(100, Winner::P2, 1));
        let rep = run(&r, &block(&Ident::default(), true, &three)).unwrap();
        assert!(rep.text.contains("ラウンド数: 2 / 3"), "{}", rep.text);
        assert!(!rep.equal);
    }

    #[test]
    fn the_header_picks_the_matching_block_even_when_a_later_one_has_the_same_lengths() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        assert_eq!(r.ident, ident("26/01/01", "NO_NAME"), "合成の見出し");
        let mut other = two_rounds();
        other[0].life[0] = Some(0);
        let log = [
            block(&ident("26/01/01", "NO_NAME"), true, &two_rounds()),
            block(&ident("26/01/02", "NO_NAME"), true, &other),
            block(&Ident::default(), true, &other),
        ]
        .join("\n");
        let p = parse_log(&log);
        assert_eq!(pick(&r, &p.blocks), Some((0, PickBy::Header)));
        let rep = run(&r, &log).unwrap();
        assert!(rep.equal, "{}", rep.text);
        assert!(rep.text.contains("3 件のうち 1 件目"), "{}", rep.text);
        // 同じ見出しが 2 回あれば最後（一番新しい再生）。
        let log = [
            block(&ident("26/01/01", "NO_NAME"), true, &other),
            block(&ident("26/01/01", "NO_NAME"), true, &two_rounds()),
        ]
        .join("\n");
        let rep = run(&r, &log).unwrap();
        assert!(rep.equal, "{}", rep.text);
        assert!(rep.text.contains("2 件のうち 2 件目"), "{}", rep.text);
    }

    #[test]
    fn without_a_usable_header_lengths_pick_and_then_the_last_block() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let mut other = two_rounds();
        other[1].frames = 7000;
        let none = Ident::default();
        let blocks = parse_log(
            &[
                block(&none, true, &two_rounds()),
                block(&none, true, &other),
            ]
            .join("\n"),
        )
        .blocks;
        assert_eq!(pick(&r, &blocks), Some((0, PickBy::RoundLengths)));
        // 見出しが合わない（別の日）ブロックしか無くても、長さが合えば長さで選ぶ。
        let blocks = parse_log(&block(&ident("25/01/01", "00:00:00"), true, &two_rounds())).blocks;
        assert_eq!(pick(&r, &blocks), Some((0, PickBy::RoundLengths)));
        // どれも合わなければ最後。選び方の文に「疑い」が出る。
        let blocks =
            parse_log(&[block(&none, true, &other), block(&none, false, &other[..1])].join("\n"))
                .blocks;
        assert_eq!(pick(&r, &blocks), Some((1, PickBy::LastBlock)));
        let rep = run(&r, &[block(&none, true, &other)].join("\n")).unwrap();
        assert!(rep.text.contains("別のリプレイを比べている疑い"));
        assert!(!rep.equal);
        assert_eq!(pick(&r, &[]), None);
    }

    #[test]
    fn nothing_to_compare_is_an_error_not_a_match() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let e = run(&r, "何も無いログ\n花映塚 1.50a\n").unwrap_err();
        assert!(e.contains("再生の行"), "{e}");
        let e = run(&r, &format!("{PLAY_MARK} end=complete rounds=1")).unwrap_err();
        assert!(e.contains("どれも読めない"), "{e}");
        // 印のラウンドが 0 件のリプレイは材料にしない。
        let e = ref_from_bytes("z.rpy", &replay_bytes(Vec::new())).unwrap_err();
        assert!(e.contains("0 件"), "{e}");
        // 印の無いリプレイ。
        let plain = build_replay(&BuildSpec::match_replay(0x52, vec![1], vec![2]));
        assert!(ref_from_bytes("p.rpy", &plain).is_err());
    }

    #[test]
    fn a_damaged_line_beside_a_good_one_is_warned_and_the_good_one_is_used() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let good = block(&Ident::default(), true, &two_rounds());
        let log = format!("{}\n{good}\n", good.replace("rounds=2", "rounds=9"));
        let rep = run(&r, &log).unwrap();
        assert!(rep.equal);
        assert!(rep.text.contains("注意: ログの 1 行目"), "{}", rep.text);
    }

    #[test]
    fn dropped_round_counts_are_compared() {
        let r = ref_from_bytes("a.rpy", &replay_bytes(two_rounds())).unwrap();
        let log = format_line(true, &Ident::default(), &two_rounds(), 2);
        let rep = run(&r, &log).unwrap();
        assert!(!rep.equal);
        assert!(
            rep.text.contains("記録できなかったラウンド: 0 / 2"),
            "{}",
            rep.text
        );
    }
}
