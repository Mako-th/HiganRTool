//! 突き合わせ（`peer_sim --compare`）: dll の `_net_frames.csv` と peer_sim の記録を読み、使ったフレームの列 (seg, frame, 1P, 2P) が同じかを
//! 見て、dll の余裕と待ちの分布（p50 / p95 / p99 / max）と delay の変わり目を出す。
//!
//! 一致の決まり: dll の行は全部、peer_sim の同じ番目の行と (seg, frame, 1P, 2P) が同じ（`same_key`）。delay は条件に入れない: 使った後の
//! delay は書き出す時点の今の D なので、マッチの途中で D を替えた後は同じ語でも両者の delay が違いうる。違った行の数は別に出す
//! （`Outcome::delay_only`。`compare2.rs` も同じ）。peer_sim は末尾に 1 行まで多くてよい（dll が最後の語を取ってから閉じた分）。
//! どちらかが 0 行なら、比べられていないので食い違いとする（0 行を一致と言ってしまわないため。`--compare2` と同じ扱い）。
//!
//! 読むだけの道具（ファイルは `main.rs` が読む）。ここは文字列を受けて文字列を返す。

use std::collections::BTreeMap;
use std::fmt::Write as _;

// dll の `_net_frames.csv` の見出し（新旧）は `higanr_core::replay_compare` の定数。古い 7 列のログも読み、そのときの `head_wait_us` は
// 0 として扱う。
use higanr_core::replay_compare::{NET_FRAMES_HEADER, NET_FRAMES_HEADER_V1};

use crate::record::{ms3, percentile};

/// dll の `_net_frames.csv` の見出しか（新旧どちらか）。`parse_dll_csv` を呼ぶかの見分けに使う。
pub fn is_dll_header(h: &str) -> bool {
    let h = h.trim();
    h == NET_FRAMES_HEADER || h == NET_FRAMES_HEADER_V1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
    pub delay: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DllRow {
    pub row: Row,
    pub margin_us: Option<u64>,
    pub wait_us: u64,
    /// フレームの始まりで相手の入力ワードを待った時間（µs。7 列の古いログは 0）。
    pub head_wait_us: u64,
}

fn num<T: std::str::FromStr>(s: &str, what: &str, line: usize) -> Result<T, String> {
    s.trim()
        .parse::<T>()
        .map_err(|_| format!("{line} 行目の {what} {s:?} が読めない"))
}

fn hex16(s: &str, what: &str, line: usize) -> Result<u16, String> {
    u16::from_str_radix(s.trim(), 16)
        .map_err(|_| format!("{line} 行目の {what} {s:?} が 16 進で読めない"))
}

/// dll の `_net_frames.csv` を読む。見出しが新旧どちらでもなければ断る（古い 4 列の CSV を黙って読み違えないように）。見出しが 7 列の
/// 古い形なら行も 7 列で、`head_wait_us` は 0。新しい形（8 列）の行が 7 列、古い形の行が 8 列なら、見出しと行の食い違いとして断る。
pub fn parse_dll_csv(text: &str) -> Result<Vec<DllRow>, String> {
    let mut lines = text.lines().enumerate();
    let cols = match lines.next() {
        Some((_, h)) if h.trim() == NET_FRAMES_HEADER => 8,
        Some((_, h)) if h.trim() == NET_FRAMES_HEADER_V1 => 7,
        Some((_, h)) => {
            return Err(format!(
                "dll の CSV の見出し {h:?} が {NET_FRAMES_HEADER:?}（古い形は {NET_FRAMES_HEADER_V1:?}）でない"
            ));
        }
        None => return Err("dll の CSV が空".to_string()),
    };
    let mut out = Vec::new();
    for (i, l) in lines {
        let n = i + 1;
        if l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split(',').collect();
        if f.len() != cols {
            return Err(format!("{n} 行目の列の数が {cols} でない: {l:?}"));
        }
        // 列の数は上で見た（7 か 8）ので、先頭の 7 つの後ろは 0 個（古い形）か 1 個（フレームの始まりの待ち）。
        let [seg, frame, p1, p2, delay, margin, wait, rest @ ..] = f.as_slice() else {
            return Err(format!("{n} 行目の列が足りない: {l:?}"));
        };
        let head_wait_us = match rest.first() {
            Some(h) => num(h, "先頭の待ち", n)?,
            None => 0,
        };
        out.push(DllRow {
            row: Row {
                seg: num(seg, "seg", n)?,
                frame: num(frame, "frame", n)?,
                p1: hex16(p1, "1P", n)?,
                p2: hex16(p2, "2P", n)?,
                delay: num(delay, "delay", n)?,
            },
            margin_us: if margin.trim().is_empty() {
                None
            } else {
                Some(num(margin, "余裕", n)?)
            },
            wait_us: num(wait, "待ち", n)?,
            head_wait_us,
        });
    }
    Ok(out)
}

/// peer_sim の記録を読む（`#` で始まらない空でない行が `seg frame 1P 2P delay`）。
pub fn parse_peer_record(text: &str) -> Result<Vec<Row>, String> {
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let n = i + 1;
        let t = l.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let [seg, frame, p1, p2, delay] = f.as_slice() else {
            return Err(format!("{n} 行目の列の数が 5 でない: {l:?}"));
        };
        out.push(Row {
            seg: num(seg, "seg", n)?,
            frame: num(frame, "frame", n)?,
            p1: hex16(p1, "1P", n)?,
            p2: hex16(p2, "2P", n)?,
            delay: num(delay, "delay", n)?,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub dll_rows: usize,
    pub peer_rows: usize,
    pub matched: usize,
    /// 先頭から一致した行のうち、delay だけが違った行の数。一致の判定には入れない（出力の 1 行）。
    pub delay_only: usize,
    /// 最初に食い違った番目と、そこの両者の行（片方が尽きていれば None）。
    pub first_diff: Option<(usize, Option<Row>, Option<Row>)>,
    pub ok: bool,
}

/// 一致の決まりに入れる欄（seg・frame・1P・2P）が同じか。delay は入れない（使った後の delay は書き出す時点の今の D なので、マッチの途中で
/// D を替えた後は語が同じでも違いうる）。`compare2` もこの関数を使う。
pub(crate) fn same_key(x: &Row, y: &Row) -> bool {
    x.seg == y.seg && x.frame == y.frame && x.p1 == y.p1 && x.p2 == y.p2
}

pub fn compare(dll: &[DllRow], peer: &[Row]) -> Outcome {
    let matched = dll
        .iter()
        .zip(peer)
        .take_while(|(d, p)| same_key(&d.row, p))
        .count();
    // 先頭から一致した行（欄は同じ）の中で、delay だけが違う行の数。一致の後ろは数えない（そこは一致と言っていない行）。
    let delay_only = dll
        .iter()
        .zip(peer)
        .take(matched)
        .filter(|(d, p)| d.row.delay != p.delay)
        .count();
    let first_diff = (matched < dll.len().max(peer.len())).then(|| {
        (
            matched,
            dll.get(matched).map(|d| d.row),
            peer.get(matched).copied(),
        )
    });
    let ok =
        !dll.is_empty() && !peer.is_empty() && matched == dll.len() && peer.len() <= dll.len() + 1;
    Outcome {
        dll_rows: dll.len(),
        peer_rows: peer.len(),
        matched,
        delay_only,
        first_diff,
        ok,
    }
}

/// p50 / p95 / p99 / max の文（µs の列から。空なら「-」）。`compare2` も使う。
pub(crate) fn dist(v: &mut [u64]) -> String {
    v.sort_unstable();
    format!(
        "p50 {} / p95 {} / p99 {} / max {}",
        ms3(percentile(v, 500)),
        ms3(percentile(v, 950)),
        ms3(percentile(v, 990)),
        ms3(v.last().copied())
    )
}

/// 突き合わせの判定の語（`--compare` と `--compare2` で同じ言い方にする）。0 行の側があれば、食い違いの中でも「比べられていない」と分けて書く。
pub(crate) fn verdict(ok: bool, rows_a: usize, rows_b: usize) -> &'static str {
    if ok {
        "一致"
    } else if rows_a == 0 || rows_b == 0 {
        "食い違い（どちらかが 0 行で、比べられていない）"
    } else {
        "食い違い"
    }
}

pub(crate) fn row_text(r: Option<Row>) -> String {
    r.map_or("（無い）".to_string(), |r| {
        format!(
            "seg {} frame {} 1P {:04X} 2P {:04X} delay {}",
            r.seg, r.frame, r.p1, r.p2, r.delay
        )
    })
}

pub fn report(dll: &[DllRow], peer: &[Row]) -> (String, bool) {
    let o = compare(dll, peer);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "列: dll {} 行 / peer_sim {} 行 / 先頭から一致 {} 行 ⇒ {}",
        o.dll_rows,
        o.peer_rows,
        o.matched,
        verdict(o.ok, o.dll_rows, o.peer_rows)
    );
    let _ = writeln!(
        s,
        "  delay だけが違った行（先頭から一致した行のうち。一致の判定には入れない）: {} 行",
        o.delay_only
    );
    if let Some((i, d, p)) = o.first_diff {
        let _ = writeln!(
            s,
            "  {i} 番目（0 から）: dll {} / peer_sim {}",
            row_text(d),
            row_text(p)
        );
    }
    let segs: BTreeMap<u16, usize> = dll.iter().fold(BTreeMap::new(), |mut m, d| {
        *m.entry(d.row.seg).or_insert(0) += 1;
        m
    });
    let _ = writeln!(
        s,
        "seg ごとのフレーム（dll）: {}",
        segs.iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    // delay の変わり目（使った後の値が前の行と違う行）。
    let mut changes = Vec::new();
    for w in dll.windows(2) {
        if let [a, b] = w
            && a.row.delay != b.row.delay
        {
            changes.push(format!(
                "seg {} frame {}: {} → {}",
                b.row.seg, b.row.frame, a.row.delay, b.row.delay
            ));
        }
    }
    let _ = writeln!(
        s,
        "delay: 最初 {} / 変わり目 {} 個{}",
        dll.first()
            .map_or("-".to_string(), |d| d.row.delay.to_string()),
        changes.len(),
        if changes.is_empty() {
            String::new()
        } else {
            format!("（{}）", changes.join(" / "))
        }
    );
    let mut margins: Vec<u64> = dll.iter().filter_map(|d| d.margin_us).collect();
    let unknown = dll.len() - margins.len();
    let _ = writeln!(
        s,
        "dll の余裕（相手の語が揃ってから使うまで。{} フレームのうち {} 件。分からない {} 件は seg の始まりの穴埋め）: {}",
        dll.len(),
        margins.len(),
        unknown,
        dist(&mut margins)
    );
    let mut by_delay: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
    for d in dll {
        if let Some(m) = d.margin_us {
            by_delay.entry(d.row.delay).or_default().push(m);
        }
    }
    for (k, v) in by_delay.iter_mut() {
        let _ = writeln!(s, "  delay {k}（{} 件）: {}", v.len(), dist(v));
    }
    let mut waits: Vec<u64> = dll.iter().map(|d| d.wait_us).collect();
    let waited = waits.iter().filter(|&&w| w >= 1_000).count();
    let _ = writeln!(
        s,
        "dll の待ち（使った tick で、取得の後に相手の入力ワードを待った時間。{} フレームのうち 1 ms 以上 {} 件）: {}",
        dll.len(),
        waited,
        dist(&mut waits)
    );
    let mut heads: Vec<u64> = dll.iter().map(|d| d.head_wait_us).collect();
    let head_waited = heads.iter().filter(|&&w| w >= 1_000).count();
    let _ = writeln!(
        s,
        "dll の先頭の待ち（使った tick の先頭で、入力を読む前に相手の入力ワードを待った時間。列の無い古いログは 0。{} フレームのうち 1 ms 以上 {} 件）: {}",
        dll.len(),
        head_waited,
        dist(&mut heads)
    );
    (s, o.ok)
}

#[cfg(test)]
mod tests {
    //! 見出し・16 進の語・空の余裕・`#` の行の読み、一致の決まり（1 行の食い違い・dll の余り・peer_sim の余り 1 行と 2 行・delay だけの違い）、
    //! delay の変わり目を見る。
    use super::*;

    const CSV: &str = "seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,0000,0000,2,,0\n0,1,0000,0000,2,,5\n0,2,001A,0040,2,31000,0\n0,3,0010,0004,5,30000,2500\n1,0,0000,0000,5,,0\n";
    const REC: &str = "# peer_sim の記録\n# seg frame 1P 2P delay（delay はそのフレームを使った後の値）\n0 0 0000 0000 2\n0 1 0000 0000 2\n0 2 001A 0040 2\n0 3 0010 0004 5\n1 0 0000 0000 5\n# ---- 要約 ----\n# 終わり方: x\n";

    #[test]
    fn reads_both_and_agrees() {
        let d = parse_dll_csv(CSV).unwrap();
        let p = parse_peer_record(REC).unwrap();
        assert_eq!(d.len(), 5);
        assert_eq!(d[2].row.p1, 0x1A);
        assert_eq!(d[0].margin_us, None);
        assert_eq!(d[3].margin_us, Some(30_000));
        assert_eq!(d[3].wait_us, 2_500);
        assert_eq!(p.len(), 5);
        let (text, ok) = report(&d, &p);
        assert!(ok, "{text}");
        assert!(
            text.contains("変わり目 1 個（seg 0 frame 3: 2 → 5）"),
            "{text}"
        );
        assert!(
            text.contains("5 フレームのうち 2 件。分からない 3 件"),
            "{text}"
        );
        assert!(text.contains("1 ms 以上 1 件"), "{text}");
    }

    #[test]
    fn differences_and_tails() {
        let d = parse_dll_csv(CSV).unwrap();
        let mut p = parse_peer_record(REC).unwrap();
        p.push(Row {
            seg: 1,
            frame: 1,
            p1: 0,
            p2: 0,
            delay: 5,
        });
        assert!(compare(&d, &p).ok);
        p.push(Row {
            seg: 1,
            frame: 2,
            p1: 0,
            p2: 0,
            delay: 5,
        });
        assert!(!compare(&d, &p).ok);
        let p4 = parse_peer_record(REC).unwrap()[..4].to_vec();
        let o = compare(&d, &p4);
        assert!(!o.ok);
        assert_eq!(o.first_diff.map(|x| x.0), Some(4));
        let mut p = parse_peer_record(REC).unwrap();
        p[2].p2 = 0x44;
        let o = compare(&d, &p);
        assert!(!o.ok);
        assert_eq!(o.matched, 2);
        let mut p = parse_peer_record(REC).unwrap();
        p[3].delay = 2;
        let o = compare(&d, &p);
        assert!(o.ok, "{o:?}");
        assert_eq!((o.matched, o.delay_only), (5, 1));
        for (what, edit) in [
            ("seg", (|r: &mut Row| r.seg = 7) as fn(&mut Row)),
            ("frame", |r| r.frame = 9),
            ("1P", |r| r.p1 = 0x11),
            ("2P", |r| r.p2 = 0x44),
        ] {
            for delay in [5u8, 2] {
                let mut p = parse_peer_record(REC).unwrap();
                edit(&mut p[3]);
                p[3].delay = delay;
                let o = compare(&d, &p);
                assert!(!o.ok, "{what} の違い（delay {delay}）が一致と言われた");
                assert_eq!(o.matched, 3, "{what}");
                assert_eq!(o.delay_only, 0, "{what}");
            }
        }
        let mut p = parse_peer_record(REC).unwrap();
        p[1].delay = 9;
        p[3].p2 = 0x44;
        let o = compare(&d, &p);
        assert!(!o.ok);
        assert_eq!((o.matched, o.delay_only), (3, 1));
    }

    #[test]
    fn delay_only_is_reported_and_does_not_decide() {
        let d = parse_dll_csv(CSV).unwrap();
        let mut p = parse_peer_record(REC).unwrap();
        p[0].delay = 4;
        p[2].delay = 1;
        p[4].delay = 7;
        let (text, ok) = report(&d, &p);
        assert!(ok, "{text}");
        assert!(text.contains("⇒ 一致\n"), "{text}");
        assert!(
            text.contains(
                "delay だけが違った行（先頭から一致した行のうち。一致の判定には入れない）: 3 行"
            ),
            "{text}"
        );
        assert!(
            text.contains("変わり目 1 個（seg 0 frame 3: 2 → 5）"),
            "{text}"
        );
    }

    #[test]
    fn empty_sides_are_not_measured() {
        let d = parse_dll_csv(CSV).unwrap();
        let p = parse_peer_record(REC).unwrap();
        let d0 = parse_dll_csv(&format!("{NET_FRAMES_HEADER}\n")).unwrap();
        assert!(d0.is_empty());
        let p0 = parse_peer_record("# peer_sim の記録\n# ---- 要約 ----\n").unwrap();
        assert!(p0.is_empty());
        for (dll, peer, what) in [
            (&d0, &p0, "両方 0 行"),
            (&d0, &p[..1].to_vec(), "dll 0 行・peer_sim 1 行"),
            (&d0, &p, "dll 0 行・peer_sim 5 行"),
            (&d, &p0, "dll 5 行・peer_sim 0 行"),
        ] {
            assert!(!compare(dll, peer).ok, "{what}");
            let (text, ok) = report(dll, peer);
            assert!(!ok, "{what}");
            assert!(
                text.contains("⇒ 食い違い（どちらかが 0 行で、比べられていない）"),
                "{what}: {text}"
            );
        }
        // 対照: 1 行どうし・peer_sim の 1 行の余りは一致のまま（0 行のガードが全部を断っているのではない）。
        assert!(compare(&d[..1], &p[..1]).ok);
        assert!(compare(&d[..1], &p[..2]).ok);
        let (text, ok) = report(&d, &p);
        assert!(ok && text.contains("⇒ 一致\n"), "{text}");
        let (text, _) = report(&d[..1], &p[1..]);
        assert!(text.contains("⇒ 食い違い\n"), "{text}");
    }

    #[test]
    fn refuses_other_shapes() {
        assert!(parse_dll_csv("seg,frame,p1,p2\n0,0,0000,0000\n").is_err());
        assert!(parse_dll_csv("").is_err());
        assert!(parse_dll_csv(&format!("{NET_FRAMES_HEADER}\n0,0,0000,0000,2,\n")).is_err());
        // 16 進でない語（列の数は合わせてある。列の数で落ちているのではないこと）。
        assert!(parse_dll_csv(&format!("{NET_FRAMES_HEADER}\n0,0,00G0,0000,2,,0,0\n")).is_err());
        assert!(parse_dll_csv(&format!("{NET_FRAMES_HEADER_V1}\n0,0,00G0,0000,2,,0\n")).is_err());
        assert!(parse_peer_record("0 0 0000 0000\n").is_err());
    }

    #[test]
    fn reads_the_head_wait_column_and_still_reads_the_old_seven() {
        let new = format!(
            "{NET_FRAMES_HEADER}\n0,0,0000,0000,2,,0,0\n0,1,0000,0000,2,,5,0\n0,2,001A,0040,2,31000,0,7300\n0,3,0010,0004,5,30000,2500,12\n"
        );
        let d = parse_dll_csv(&new).unwrap();
        assert_eq!(d.len(), 4);
        assert_eq!(
            (
                d[2].wait_us,
                d[2].head_wait_us,
                d[3].wait_us,
                d[3].head_wait_us
            ),
            (0, 7_300, 2_500, 12),
            "待ちと先頭の待ちの列が入れ替わった"
        );
        let old = parse_dll_csv(CSV).unwrap();
        assert!(
            old.iter().all(|r| r.head_wait_us == 0),
            "古いログの先頭の待ち"
        );
        assert_eq!(old[3].wait_us, 2_500);
        assert!(parse_dll_csv(&format!("{NET_FRAMES_HEADER}\n0,0,0000,0000,2,,0\n")).is_err());
        assert!(parse_dll_csv(&format!("{NET_FRAMES_HEADER_V1}\n0,0,0000,0000,2,,0,0\n")).is_err());
        assert!(parse_dll_csv(&format!("{NET_FRAMES_HEADER}\n0,0,0000,0000,2,,0,x\n")).is_err());
        let p = parse_peer_record(REC).unwrap();
        let (text, _) = report(&d, &p[..4]);
        assert!(
            text.contains("dll の先頭の待ち") && text.contains("4 フレームのうち 1 ms 以上 1 件"),
            "{text}"
        );
        assert!(is_dll_header(NET_FRAMES_HEADER) && is_dll_header(NET_FRAMES_HEADER_V1));
        assert!(!is_dll_header("seg,frame,p1,p2"));
    }
}
