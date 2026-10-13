//! 観戦者の列と対戦者の列の突き合わせ（`peer_sim --compare-spectate`）: 観戦者の CSV（`watch::frames_csv`。`seg,frame,p1,p2`）と、
//! 対戦者の記録（dll の `_net_frames.csv` か peer_sim の記録）を読み、観戦者が受け取ったマッチの入力ワードが、対戦者の使った入力ワードと同じかを見る。
//!
//! 一致の決まり（全部を満たしたとき ok）:
//! - 観戦者の行が 1 行以上ある（0 行は「比べられていない」で食い違い）。
//! - 観戦者の最初の行の (seg, frame) が対戦者の列のどこかにあり、そこから観戦者の行が全部、対戦者の同じ番目の行と同じ。
//!   対戦者の列の先頭からでなくてよい（遅れて入った観戦者は、いまの seg の始まりから流れる）が、**その seg の最初の行**であること
//!   （seg の途中から始まっていたら、先頭から流すはずの規則が守られていない）。
//! - 観戦者の最後の行の後に、対戦者の行が 1 行までしか残らない（dll が最後の語を取ってから閉じた分。`compare` と同じ許し方）。
//!   多く残っているなら、観戦者が途中で受け取りをやめている。残りの行数は必ず出す。
//!
//! 押下（`# 押下` の行が対戦者の peer_sim の記録にあるとき）も、観戦者の記録の CSV があれば比べる: 観戦者の押下の並びが、
//! 対戦者の押下の並びの末尾と同じ（先頭から観戦したなら全部と同じ）。
//!
//! 読むだけの道具（ファイルは `main.rs` が読む）。ここは文字列を受けて文字列を返す。

use std::fmt::Write as _;

use crate::compare::{is_dll_header, parse_dll_csv, parse_peer_record, verdict};
use crate::watch::FRAMES_CSV_HEADER;

/// 比べる 1 行（seg・frame・1P・2P。観戦者の列には delay が無いので、対戦者側の delay は比べない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SRow {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
}

fn row_text(r: Option<SRow>) -> String {
    r.map_or("（無い）".to_string(), |r| {
        format!(
            "seg {} frame {} 1P {:04X} 2P {:04X}",
            r.seg, r.frame, r.p1, r.p2
        )
    })
}

pub fn parse_watch_frames(text: &str) -> Result<Vec<SRow>, String> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, h)) if h.trim() == FRAMES_CSV_HEADER => {}
        Some((_, h)) => {
            return Err(format!(
                "観戦者の CSV の見出し {h:?} が {FRAMES_CSV_HEADER:?} でない"
            ));
        }
        None => return Err("観戦者の CSV が空".to_string()),
    }
    let mut out = Vec::new();
    for (i, l) in lines {
        let n = i + 1;
        if l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split(',').collect();
        let [seg, frame, p1, p2] = f.as_slice() else {
            return Err(format!("{n} 行目の列の数が 4 でない: {l:?}"));
        };
        let num = |s: &str, what: &str| {
            s.trim()
                .parse::<u32>()
                .map_err(|_| format!("{n} 行目の {what} {s:?} が読めない"))
        };
        let hex = |s: &str, what: &str| {
            u16::from_str_radix(s.trim(), 16)
                .map_err(|_| format!("{n} 行目の {what} {s:?} が 16 進で読めない"))
        };
        out.push(SRow {
            seg: u16::try_from(num(seg, "seg")?).map_err(|_| format!("{n} 行目の seg が大きい"))?,
            frame: num(frame, "frame")?,
            p1: hex(p1, "1P")?,
            p2: hex(p2, "2P")?,
        });
    }
    Ok(out)
}

/// 対戦者の記録を読む。先頭の行が dll の `_net_frames.csv` の見出しなら dll の CSV、そうでなければ peer_sim の記録。
pub fn parse_player(text: &str) -> Result<Vec<SRow>, String> {
    let first = text.lines().find(|l| !l.trim().is_empty()).map(str::trim);
    let rows: Vec<(u16, u32, u16, u16)> = if first.is_some_and(is_dll_header) {
        parse_dll_csv(text)?
            .into_iter()
            .map(|d| (d.row.seg, d.row.frame, d.row.p1, d.row.p2))
            .collect()
    } else {
        parse_peer_record(text)?
            .into_iter()
            .map(|r| (r.seg, r.frame, r.p1, r.p2))
            .collect()
    };
    Ok(rows
        .into_iter()
        .map(|(seg, frame, p1, p2)| SRow { seg, frame, p1, p2 })
        .collect())
}

/// peer_sim の記録の `# 押下 step kind value screen` の行を、`watch::step_fields` と同じ文字の形にして取り出す。
/// 見出しの行（`# 押下 step kind value screen（入った順）`）は数として読めないので読み飛ばす。
pub fn presses_from_peer_log(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("# 押下 ")?;
            let t: Vec<&str> = rest.split_whitespace().collect();
            let [step, kind, value, screen] = t.as_slice() else {
                return None;
            };
            step.parse::<u16>().ok()?;
            screen.parse::<u8>().ok()?;
            Some(format!(
                "step={step} kind={kind} value={value} screen={screen}"
            ))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub watch_rows: usize,
    pub player_rows: usize,
    /// 観戦者の最初の行が、対戦者の列の何番目（0 から）にあたるか。無ければ None。
    pub offset: Option<usize>,
    /// 観戦者の先頭から続けて一致した行の数。
    pub matched: usize,
    /// 最初に食い違った観戦者の行の番目（0 から）と、そこの両者の行。
    pub first_diff: Option<(usize, Option<SRow>, Option<SRow>)>,
    /// 観戦者の最初の行が、対戦者の列のその seg の最初の行にあたるか。
    pub starts_at_seg_head: bool,
    /// 観戦者の最後の行の後に残った対戦者の行の数（offset が無ければ None）。
    pub tail_left: Option<usize>,
    pub ok: bool,
}

pub fn compare(watch: &[SRow], player: &[SRow]) -> Outcome {
    let offset = watch.first().and_then(|w0| {
        player
            .iter()
            .position(|p| p.seg == w0.seg && p.frame == w0.frame)
    });
    let at = offset.unwrap_or(0);
    let matched = watch
        .iter()
        .zip(player.iter().skip(at))
        .take_while(|(w, p)| w == p)
        .count();
    let first_diff = (offset.is_some() && matched < watch.len()).then(|| {
        (
            matched,
            watch.get(matched).copied(),
            player.get(at + matched).copied(),
        )
    });
    let starts_at_seg_head = offset.is_some_and(|o| {
        o == 0
            || player
                .get(o - 1)
                .zip(player.get(o))
                .is_some_and(|(prev, cur)| prev.seg != cur.seg)
    });
    let tail_left = offset.map(|o| player.len().saturating_sub(o + watch.len()));
    let ok = !watch.is_empty()
        && offset.is_some()
        && matched == watch.len()
        && starts_at_seg_head
        && tail_left.is_some_and(|t| t <= 1);
    Outcome {
        watch_rows: watch.len(),
        player_rows: player.len(),
        offset,
        matched,
        first_diff,
        starts_at_seg_head,
        tail_left,
        ok,
    }
}

/// 押下の並びの比べ。観戦者の並びが、対戦者の並びの末尾と同じ（先頭から観戦したなら全部と同じ）。戻り値は（一致したか, 文）。
pub fn compare_steps(watch: &[String], player: &[String], from_start: bool) -> (bool, String) {
    let k = watch.len();
    let ok = if from_start {
        watch == player
    } else {
        k <= player.len() && watch == &player[player.len() - k..]
    };
    let text = format!(
        "押下: 観戦者 {} 個 / 対戦者 {} 個（観戦者は{}）⇒ {}",
        k,
        player.len(),
        if from_start {
            "先頭から観戦したので全部と同じはず"
        } else {
            "途中から入ったので末尾と同じはず"
        },
        if ok { "一致" } else { "食い違い" }
    );
    (ok, text)
}

/// 突き合わせの文と、一致したか。`steps` は（観戦者の押下, 対戦者の押下, 観戦者がリングバッファの先頭から受けたか）で、対戦者が peer_sim の
/// 記録で、観戦者の記録の CSV もあるときだけ渡す。「リングバッファの先頭から」は、観戦者の記録の最初の番号が 0 のとき（マッチの入力ワードの先頭が
/// 対戦者の列の先頭であることとは別: 押下・SELECT は `Seg(0)` の後ろ（マッチの入力ワードの前）に積まれるので、seg 0 のマッチの入力ワードの先頭から
/// 入った観戦者でも、リングバッファの先頭から入っていれば押下・SELECT を受けている）。
pub fn report(
    watch: &[SRow],
    player: &[SRow],
    steps: Option<(&[String], &[String], bool)>,
) -> (String, bool) {
    let o = compare(watch, player);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "列: 観戦者 {} 行 / 対戦者 {} 行 / 観戦者は対戦者の {} 行目から / 先頭から一致 {} 行 / 観戦者の後に残った対戦者の行 {} ⇒ {}",
        o.watch_rows,
        o.player_rows,
        o.offset
            .map_or("（見つからない）".to_string(), |v| v.to_string()),
        o.matched,
        o.tail_left
            .map_or("（不明）".to_string(), |v| v.to_string()),
        verdict(o.ok, o.watch_rows, o.player_rows)
    );
    if o.offset.is_some() && !o.starts_at_seg_head {
        let _ = writeln!(
            s,
            "  観戦者の最初の行が、対戦者の seg の途中（始まりではない）にあたる（いまの seg の始まりから流す規則に反する）"
        );
    }
    if let Some(t) = o.tail_left
        && t > 1
    {
        let _ = writeln!(
            s,
            "  観戦者は最後まで受け取っていない（対戦者の行が {t} 行残っている）"
        );
    }
    if o.offset.is_none() && !watch.is_empty() {
        let _ = writeln!(
            s,
            "  観戦者の最初の行 {} が対戦者の列に無い",
            row_text(watch.first().copied())
        );
    }
    if let Some((i, w, p)) = o.first_diff {
        let _ = writeln!(
            s,
            "  観戦者の {i} 番目（0 から）: 観戦者 {} / 対戦者 {}",
            row_text(w),
            row_text(p)
        );
    }
    let mut ok = o.ok;
    if let Some((w, p, from_start)) = steps {
        let (sok, text) = compare_steps(w, p, from_start);
        ok &= sok;
        let _ = writeln!(s, "{text}");
    }
    (s, ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(seg: u16, frame: u32, p1: u16, p2: u16) -> SRow {
        SRow { seg, frame, p1, p2 }
    }

    fn player() -> Vec<SRow> {
        vec![
            r(0, 0, 1, 2),
            r(0, 1, 3, 4),
            r(1, 0, 5, 6),
            r(1, 1, 7, 8),
            r(1, 2, 9, 10),
        ]
    }

    #[test]
    fn full_and_late_join_from_a_seg_head_match() {
        let p = player();
        let o = compare(&p, &p);
        assert!(
            o.ok && o.offset == Some(0) && o.tail_left == Some(0),
            "{o:?}"
        );
        let o = compare(&p[2..], &p);
        assert!(o.ok && o.offset == Some(2), "{o:?}");
    }

    #[test]
    fn every_kind_of_difference_is_caught() {
        let p = player();
        let mut w = p.clone();
        w[3].p2 ^= 1;
        assert!(!compare(&w, &p).ok);
        let mut w = p.clone();
        (w[1].p1, w[1].p2) = (w[1].p2, w[1].p1);
        let o = compare(&w, &p);
        assert!(!o.ok && o.first_diff.is_some_and(|d| d.0 == 1), "{o:?}");
        assert!(!compare(&p[3..], &p).ok);
        assert!(!compare(&p[..3], &p).ok);
        assert!(compare(&p[..4], &p).ok);
        let o = compare(&[], &p);
        assert!(!o.ok, "{o:?}");
        assert!(report(&[], &p, None).0.contains("比べられていない"));
        assert!(!compare(&[r(9, 0, 1, 2)], &p).ok);
    }

    #[test]
    fn steps_must_equal_all_from_the_start_and_the_tail_when_late() {
        let a = "step=1 kind=Decide value=0x00 screen=0".to_string();
        let b = "step=2 kind=Decide value=0x01 screen=7".to_string();
        let both = vec![a.clone(), b.clone()];
        assert!(compare_steps(&both, &both, true).0);
        assert!(!compare_steps(std::slice::from_ref(&b), &both, true).0);
        assert!(compare_steps(std::slice::from_ref(&b), &both, false).0);
        assert!(compare_steps(&[], &both, false).0);
        assert!(!compare_steps(std::slice::from_ref(&a), &both, false).0);
    }

    #[test]
    fn readers_take_the_watcher_csv_dll_csv_and_peer_log() {
        let w = parse_watch_frames("seg,frame,p1,p2\n0,0,00C4,BEEF\n\n1,2,0001,0002\n").unwrap();
        assert_eq!(w, vec![r(0, 0, 0xC4, 0xBEEF), r(1, 2, 1, 2)]);
        assert!(parse_watch_frames("seg,frame,p1,p2,delay\n").is_err());
        assert!(parse_watch_frames("seg,frame,p1,p2\n0,0,ZZ,1\n").is_err());
        let dll = "seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,00C4,BEEF,3,100,0\n";
        assert_eq!(parse_player(dll).unwrap(), vec![r(0, 0, 0xC4, 0xBEEF)]);
        // 末尾へ `head_wait_us` を足した 8 列の CSV も dll の CSV として読む（見出しの比べが古い形だけだと、peer_sim の記録として読んで落ちる）。
        let dll8 =
            "seg,frame,p1,p2,delay,margin_us,wait_us,head_wait_us\n0,0,00C4,BEEF,3,100,0,8000\n";
        assert_eq!(parse_player(dll8).unwrap(), vec![r(0, 0, 0xC4, 0xBEEF)]);
        let log = "# 引数: x\n# 押下 step kind value screen（入った順）\n# 押下 3 Decide 0x0A 7\n0 0 00C4 BEEF 3\n";
        assert_eq!(parse_player(log).unwrap(), vec![r(0, 0, 0xC4, 0xBEEF)]);
        assert_eq!(
            presses_from_peer_log(log),
            vec!["step=3 kind=Decide value=0x0A screen=7".to_string()]
        );
    }
}
