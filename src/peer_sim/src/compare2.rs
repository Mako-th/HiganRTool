//! 2 台の突き合わせ（`peer_sim --compare2 A.csv B.csv`）: 同じ PC で本物の花映塚を 2 つ走らせたときの、
//! 両方の dll の `_net_frames.csv` を比べる。使ったフレームの列 (seg, frame, 1P, 2P) が同じかを見て、
//! 両者の余裕と待ちの分布（p50 / p95 / p99 / max）と、待ちを 1 ms の枠で数えた表を出す。分布の書き方は `compare.rs` と同じ。
//!
//! 一致の決まり: 短い方の行は全部、長い方の同じ番目の行と (seg, frame, 1P, 2P) が同じ（`same_key`）。
//! 長い方の余りは「最後に一致した行の delay + 1」行まで（lockstep では相手の入力ワードが delay フレーム先まで届いていて、
//! BYE の後に受けた側が使い切ると、その分だけ先へ進みうる。推定で、本物どうしの実測の余りはまだ無い）。どちらかが 0 行なら比べられていないので食い違いとする。
//!
//! delay は一致の条件に入れない: 書き出す時点の今の D（`netplay/rollback.rs` の `cur_delay()`）なので、マッチの途中で D を
//! 替えた後は、同じ入力ワードを使っていても両台で違いうる。先頭から一致した行のうち delay だけが違う行の数は別の 1 行で出す（`Outcome2::delay_only`）。
//!
//! 読むだけの道具（ファイルは `main.rs` が読む）。

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::compare::{DllRow, Row, dist, row_text, same_key, verdict};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome2 {
    pub a_rows: usize,
    pub b_rows: usize,
    /// 先頭から続けて一致した行の数（delay は入らない）。
    pub matched: usize,
    /// 先頭から一致した行のうち delay だけが違った行の数（一致の判定には入れない）。
    pub delay_only: usize,
    /// 最初に食い違った番目と、そこの両者の行（片方が尽きていれば None）。
    pub first_diff: Option<(usize, Option<Row>, Option<Row>)>,
    pub tail: usize,
    /// 余りを許す上限（最後に一致した行の両台の delay の大きい方 + 1。一致が 0 行なら 0）。
    pub tail_limit: usize,
    pub ok: bool,
}

pub fn compare2(a: &[DllRow], b: &[DllRow]) -> Outcome2 {
    let matched = a
        .iter()
        .zip(b)
        .take_while(|(x, y)| same_key(&x.row, &y.row))
        .count();
    // 一致の後ろは数えない（そこは一致と言っていない行）。
    let delay_only = a
        .iter()
        .zip(b)
        .take(matched)
        .filter(|(x, y)| x.row.delay != y.row.delay)
        .count();
    let short = a.len().min(b.len());
    let tail = a.len().max(b.len()) - short;
    // 最後に一致した行の delay は途中の切替で両台が違いうるので、大きい方 + 1。
    let tail_limit = matched.checked_sub(1).map_or(0, |i| {
        let d = |rows: &[DllRow]| rows.get(i).map_or(0, |r| usize::from(r.row.delay));
        d(a).max(d(b)) + 1
    });
    let first_diff = (matched < a.len().max(b.len())).then(|| {
        (
            matched,
            a.get(matched).map(|d| d.row),
            b.get(matched).map(|d| d.row),
        )
    });
    let ok = short > 0 && matched == short && tail <= tail_limit;
    Outcome2 {
        a_rows: a.len(),
        b_rows: b.len(),
        matched,
        delay_only,
        first_diff,
        tail,
        tail_limit,
        ok,
    }
}

/// 待ちを数える枠の数（0 / 1 ms 未満 / 1〜15 ms の 1 ms ごと / 16 ms 以上）。
const WAIT_BINS: usize = 18;

/// 待ち（µs）を枠に分ける。0 は待たなかった、1 は 1 ms 未満、k + 1 は k ms 以上 k + 1 ms 未満（k = 1〜15）、17 は 16 ms 以上。
fn wait_bin(us: u64) -> usize {
    match us {
        0 => 0,
        1..=999 => 1,
        _ => ((us / 1_000) as usize + 1).min(WAIT_BINS - 1),
    }
}

fn bin_label(i: usize) -> String {
    match i {
        0 => "0".to_string(),
        1 => "1 ms 未満".to_string(),
        i if i == WAIT_BINS - 1 => "16 ms 以上".to_string(),
        i => format!("{}〜{} ms", i - 1, i),
    }
}

fn wait_table(rows: &[DllRow]) -> String {
    let mut bins = [0usize; WAIT_BINS];
    for r in rows {
        if let Some(b) = bins.get_mut(wait_bin(r.wait_us)) {
            *b += 1;
        }
    }
    let parts: Vec<String> = bins
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(i, n)| format!("{}: {n}", bin_label(i)))
        .collect();
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" / ")
    }
}

fn side(s: &mut String, label: &str, rows: &[DllRow]) {
    let mut margins: Vec<u64> = rows.iter().filter_map(|d| d.margin_us).collect();
    let unknown = rows.len() - margins.len();
    let _ = writeln!(
        s,
        "{label} の余裕（相手の語が揃ってから使うまで。{} フレームのうち {} 件。分からない {} 件は seg の始まりの穴埋め）: {}",
        rows.len(),
        margins.len(),
        unknown,
        dist(&mut margins)
    );
    let mut by_delay: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
    for d in rows {
        if let Some(m) = d.margin_us {
            by_delay.entry(d.row.delay).or_default().push(m);
        }
    }
    for (k, v) in by_delay.iter_mut() {
        let _ = writeln!(s, "  delay {k}（{} 件）: {}", v.len(), dist(v));
    }
    let mut waits: Vec<u64> = rows.iter().map(|d| d.wait_us).collect();
    let mut long: Vec<u64> = waits.iter().copied().filter(|&w| w >= 1_000).collect();
    let _ = writeln!(
        s,
        "{label} の待ち（使った tick で、取得の後に相手の入力ワードを待った時間。{} フレームのうち 1 ms 以上 {} 件）: {}",
        rows.len(),
        long.len(),
        dist(&mut waits)
    );
    let _ = writeln!(
        s,
        "  1 ms 以上の待ちだけ（{} 件）: {}",
        long.len(),
        dist(&mut long)
    );
    let _ = writeln!(
        s,
        "  待ちの枠（{} フレーム。0 でない枠だけ）: {}",
        rows.len(),
        wait_table(rows)
    );
    let mut heads: Vec<u64> = rows.iter().map(|d| d.head_wait_us).collect();
    let head_long = heads.iter().filter(|&&w| w >= 1_000).count();
    let _ = writeln!(
        s,
        "{label} の先頭の待ち（使った tick の先頭で、入力を読む前に相手の入力ワードを待った時間。列の無い古いログは 0。{} フレームのうち 1 ms 以上 {} 件）: {}",
        rows.len(),
        head_long,
        dist(&mut heads)
    );
}

/// `a_name` / `b_name` は表示に使う名前。
pub fn report2(a: &[DllRow], b: &[DllRow], a_name: &str, b_name: &str) -> (String, bool) {
    let o = compare2(a, b);
    let mut s = String::new();
    let _ = writeln!(s, "A = {a_name}");
    let _ = writeln!(s, "B = {b_name}");
    let longer = if o.a_rows >= o.b_rows { "A" } else { "B" };
    let _ = writeln!(
        s,
        "列: A {} 行 / B {} 行 / 先頭から一致 {} 行 / 余り {} 行（{}の側。許す上限は最後に一致した行の delay + 1 = {}）⇒ {}",
        o.a_rows,
        o.b_rows,
        o.matched,
        o.tail,
        if o.tail == 0 { "どちら" } else { longer },
        o.tail_limit,
        verdict(o.ok, o.a_rows, o.b_rows)
    );
    let _ = writeln!(
        s,
        "  delay だけが違った行（先頭から一致した行のうち。一致の判定には入れない）: {} 行",
        o.delay_only
    );
    if let Some((i, x, y)) = o.first_diff {
        let _ = writeln!(
            s,
            "  {i} 番目（0 から）: A {} / B {}",
            row_text(x),
            row_text(y)
        );
    }
    let segs: BTreeMap<u16, usize> = a.iter().fold(BTreeMap::new(), |mut m, d| {
        *m.entry(d.row.seg).or_insert(0) += 1;
        m
    });
    let _ = writeln!(
        s,
        "seg ごとのフレーム（A）: {}",
        segs.iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let changes = a
        .windows(2)
        .filter(|w| matches!(w, [x, y] if x.row.delay != y.row.delay))
        .count();
    let _ = writeln!(
        s,
        "delay（A）: 最初 {} / 変わり目 {} 個",
        a.first()
            .map_or("-".to_string(), |d| d.row.delay.to_string()),
        changes
    );
    side(&mut s, "A", a);
    side(&mut s, "B", b);
    (s, o.ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::parse_dll_csv;

    const A: &str = "seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,0000,0000,2,,0\n0,1,0000,0000,2,,0\n0,2,001A,0040,2,31000,0\n0,3,0010,0004,2,500,4100\n1,0,0000,0000,2,,999\n";
    const B: &str = "seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,0000,0000,2,,0\n0,1,0000,0000,2,,0\n0,2,001A,0040,2,0,1000\n0,3,0010,0004,2,0,16000\n1,0,0000,0000,2,,15999\n";

    fn rows(t: &str) -> Vec<DllRow> {
        parse_dll_csv(t).unwrap()
    }

    fn extra(seg: u16, frame: u32, delay: u8) -> DllRow {
        DllRow {
            row: Row {
                seg,
                frame,
                p1: 0,
                p2: 0,
                delay,
            },
            margin_us: Some(0),
            wait_us: 0,
            head_wait_us: 0,
        }
    }

    #[test]
    fn same_columns_agree_and_times_are_per_side() {
        let (a, b) = (rows(A), rows(B));
        let o = compare2(&a, &b);
        assert!(o.ok, "{o:?}");
        assert_eq!((o.matched, o.tail, o.tail_limit), (5, 0, 3));
        let (text, ok) = report2(&a, &b, "a.csv", "b.csv");
        assert!(ok, "{text}");
        assert!(text.contains("⇒ 一致"), "{text}");
        assert!(
            text.contains(
                "待ちの枠（5 フレーム。0 でない枠だけ）: 0: 3 / 1 ms 未満: 1 / 4〜5 ms: 1"
            ),
            "{text}"
        );
        assert!(
            text.contains("0: 2 / 1〜2 ms: 1 / 15〜16 ms: 1 / 16 ms 以上: 1"),
            "{text}"
        );
        assert!(text.contains("A の待ち（使った tick で、取得の後に相手の入力ワードを待った時間。5 フレームのうち 1 ms 以上 1 件）"), "{text}");
        assert!(text.contains("B の待ち（使った tick で、取得の後に相手の入力ワードを待った時間。5 フレームのうち 1 ms 以上 3 件）"), "{text}");
        assert!(
            text.contains(
                "A の余裕（相手の語が揃ってから使うまで。5 フレームのうち 2 件。分からない 3 件"
            ),
            "{text}"
        );
    }

    #[test]
    fn differences_tails_and_empty() {
        let (a, b) = (rows(A), rows(B));
        let mut b1 = b.clone();
        b1[2].row.p2 = 0x44;
        let o = compare2(&a, &b1);
        assert!(!o.ok);
        assert_eq!(o.matched, 2);
        assert_eq!(o.first_diff.map(|x| x.0), Some(2));
        // delay だけの違いは一致。数だけ別に出す。
        let mut b2 = b.clone();
        b2[4].row.delay = 3;
        let o = compare2(&a, &b2);
        assert!(o.ok, "{o:?}");
        assert_eq!((o.matched, o.delay_only), (5, 1));
        // 欄の違いは食い違い。delay が同じでも違っても。
        for (what, edit) in [
            ("seg", (|r: &mut Row| r.seg = 7) as fn(&mut Row)),
            ("frame", |r| r.frame = 9),
            ("1P", |r| r.p1 = 0x11),
            ("2P", |r| r.p2 = 0x44),
        ] {
            for delay in [2u8, 5] {
                let mut b4 = b.clone();
                edit(&mut b4[3].row);
                b4[3].row.delay = delay;
                let o = compare2(&a, &b4);
                assert!(!o.ok, "{what} の違い（delay {delay}）が一致と言われた");
                assert_eq!(o.matched, 3, "{what}");
                assert_eq!(o.delay_only, 0, "{what}");
            }
        }
        // 食い違いより前の delay だけの違いは一致の行として数える（後ろは数えない）。
        let mut b5 = b.clone();
        b5[1].row.delay = 9;
        b5[3].row.p2 = 0x44;
        let o = compare2(&a, &b5);
        assert!(!o.ok);
        assert_eq!((o.matched, o.delay_only), (3, 1));
        // 余りは最後の delay + 1 = 3 行まで。B が 3 行多い ⇒ 一致、4 行多い ⇒ 食い違い。
        let mut b3 = b.clone();
        for f in 1..=3 {
            b3.push(extra(1, f, 2));
        }
        let o = compare2(&a, &b3);
        assert!(o.ok, "{o:?}");
        assert_eq!(o.tail, 3);
        b3.push(extra(1, 4, 2));
        assert!(!compare2(&a, &b3).ok);
        // 最後に一致した行の delay が両台で違う（A 2・B 4）: 上限は大きい方 + 1 = 5。
        let mut b6 = b.clone();
        b6[4].row.delay = 4;
        let mut long_b = b6.clone();
        let mut long_a = a.clone();
        for f in 1..=5 {
            long_b.push(extra(1, f, 4));
            long_a.push(extra(1, f, 2));
        }
        let o = compare2(&a, &long_b);
        assert!(o.ok && o.tail_limit == 5, "{o:?}");
        assert!(compare2(&long_b, &a).ok, "大きい delay が A の側");
        assert!(compare2(&long_a, &b6).ok);
        long_b.push(extra(1, 6, 4));
        assert!(!compare2(&a, &long_b).ok);
        let mut a3 = a.clone();
        for f in 1..=3 {
            a3.push(extra(1, f, 2));
        }
        assert!(compare2(&a3, &b).ok);
        let (text, _) = report2(&a3, &b, "a", "b");
        assert!(text.contains("余り 3 行（Aの側"), "{text}");
        // 0 行は比べられていない。
        assert!(!compare2(&[], &[]).ok);
        assert!(!compare2(&a, &[]).ok);
        let (text, ok) = report2(&[], &b, "a", "b");
        assert!(!ok);
        assert!(text.contains("比べられていない"), "{text}");
    }

    #[test]
    fn delay_only_is_reported_and_does_not_decide() {
        let (a, mut b) = (rows(A), rows(B));
        b[0].row.delay = 4;
        b[2].row.delay = 1;
        b[4].row.delay = 7;
        let (text, ok) = report2(&a, &b, "a.csv", "b.csv");
        assert!(ok, "{text}");
        assert!(text.contains("⇒ 一致\n"), "{text}");
        assert!(
            text.contains(
                "delay だけが違った行（先頭から一致した行のうち。一致の判定には入れない）: 3 行"
            ),
            "{text}"
        );
        assert!(text.contains("先頭から一致 5 行 / 余り 0 行"), "{text}");
    }

    #[test]
    fn wait_bins_edges() {
        assert_eq!(wait_bin(0), 0);
        assert_eq!(wait_bin(1), 1);
        assert_eq!(wait_bin(999), 1);
        assert_eq!(wait_bin(1_000), 2);
        assert_eq!(wait_bin(3_999), 4);
        assert_eq!(wait_bin(4_000), 5);
        assert_eq!(wait_bin(15_999), 16);
        assert_eq!(wait_bin(16_000), 17);
        assert_eq!(wait_bin(u64::MAX), 17);
        assert_eq!(bin_label(5), "4〜5 ms");
    }
}
