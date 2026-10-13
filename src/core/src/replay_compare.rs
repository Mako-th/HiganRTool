//! リプレイから取り出した入力列（`replay_decode`）を CSV に書き、dll の `_net_frames.csv`（ネットワークでの実行で使った
//! フレームの列）と突き合わせる。文字列を受けて文字列と数を返すだけの純粋な処理（ファイルは道具 `replay_inputs` が読み書きする）。
//!
//! `_net_frames.csv` の見出しの原本はここの `NET_FRAMES_HEADER`。`src/peer_sim/src/compare.rs` の `parse_dll_csv` は
//! 別のクレートで core から使えないので、先頭の 5 列だけここで読み直し、見出しは同じ定数で断る。
//!
//! 合わせ方は `offset`（`replay_frame = csv_frame + offset`）。リプレイの記録はネットワークの frame 0 の 1 tick 前から始まるので、
//! 実測した全マッチで offset 1 で全行一致した。`compare` は offset を引数で受けるだけで、既定は道具 `replay_inputs` が持つ。
//! 合わないときの手がかりに、近い offset の一致数を数える（`scan_offsets`）。

use std::fmt::Write as _;

use crate::replay_decode::{ReplayInputs, StageInputs};

/// dll の `_net_frames.csv` の見出し（dll の書き手 `net_watch::NET_FRAMES_HEADER` と `peer_sim::compare` はこれを参照する）。
/// ここで使うのは先頭の 4 列だけ。
pub const NET_FRAMES_HEADER: &str = "seg,frame,p1,p2,delay,margin_us,wait_us,head_wait_us";
/// `head_wait_us` を足す前の見出し（7 列。過去のログも読む）。
pub const NET_FRAMES_HEADER_V1: &str = "seg,frame,p1,p2,delay,margin_us,wait_us";
/// このモジュールが書く CSV の見出し。先頭の 4 列は `_net_frames.csv` と同じ入力ワードの書き方（16 進 4 桁）。
pub const INPUTS_HEADER: &str = "slot,frame,p1,p2,p3";

/// `_net_frames.csv` の 1 行（比べに使う先頭の 4 列だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetRow {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvError {
    Empty,
    BadHeader(String),
    /// `line` 行目（1 始まり。見出しが 1 行目）。
    BadRow {
        line: usize,
        what: &'static str,
    },
}

/// `_net_frames.csv` を読む。見出しが違えば断る（古い列の並びを黙って読み違えないように）。
pub fn parse_net_frames(text: &str) -> Result<Vec<NetRow>, CsvError> {
    let mut lines = text.lines().enumerate();
    let cols = match lines.next() {
        None => return Err(CsvError::Empty),
        Some((_, h)) if h.trim() == NET_FRAMES_HEADER => 8,
        Some((_, h)) if h.trim() == NET_FRAMES_HEADER_V1 => 7,
        Some((_, h)) => return Err(CsvError::BadHeader(h.trim().to_string())),
    };
    let mut out = Vec::new();
    for (i, l) in lines {
        let line = i + 1;
        if l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split(',').collect();
        if f.len() != cols {
            return Err(CsvError::BadRow {
                line,
                what: if cols == 8 {
                    "列の数が 8 でない"
                } else {
                    "列の数が 7 でない"
                },
            });
        }
        let bad = |what| CsvError::BadRow { line, what };
        out.push(NetRow {
            seg: f[0].trim().parse().map_err(|_| bad("seg"))?,
            frame: f[1].trim().parse().map_err(|_| bad("frame"))?,
            p1: u16::from_str_radix(f[2].trim(), 16).map_err(|_| bad("1P"))?,
            p2: u16::from_str_radix(f[3].trim(), 16).map_err(|_| bad("2P"))?,
        });
    }
    Ok(out)
}

pub fn segs(rows: &[NetRow]) -> Vec<u16> {
    let mut v: Vec<u16> = rows.iter().map(|r| r.seg).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// リプレイの全枠を CSV にする（見出し ＋ 枠ごとにフレーム順）。語は 16 進 4 桁。
pub fn inputs_csv(r: &ReplayInputs) -> String {
    let mut s = String::from(INPUTS_HEADER);
    s.push('\n');
    for st in &r.stages {
        for (f, ((a, b), c)) in st
            .p1_inputs
            .iter()
            .zip(&st.p2_inputs)
            .zip(&st.p3_inputs)
            .enumerate()
        {
            let _ = writeln!(s, "{},{},{:04X},{:04X},{:04X}", st.slot, f, a, b, c);
        }
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diff {
    /// csv の行の順（0 始まり）。
    pub row: usize,
    pub csv: NetRow,
    /// リプレイ側の語（範囲の外なら `None`）。
    pub replay: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comparison {
    pub csv_rows: usize,
    pub replay_frames: usize,
    /// 1P・2P とも同じだった行（一時停止の押下と止まっていた行は含まない）。
    pub matched: usize,
    /// 1P の語が違った行（範囲の外を除く）。
    pub p1_diff: usize,
    /// 2P の語が違った行（範囲の外を除く）。
    pub p2_diff: usize,
    /// リプレイのフレームの範囲の外に当たった行（offset が合っていないか、リプレイが短い）。
    pub out_of_range: usize,
    /// 食い違った行の数（`p1_diff` と `p2_diff` は両方違う行を 2 回数えるので、行としてはこちら）。
    pub mismatched: usize,
    /// 一時停止の押下の行: csv の語が、リプレイの語に 0x0008 のビットだけを足した形。食い違いに数えない。
    /// 止まっていた間の行（`paused_rows`）に入った分は数えない。
    pub pause_press: usize,
    pub pauses: usize,
    /// 止まっていた間の csv の行（リプレイに対応するフレームが無いので比べない）。
    pub paused_rows: usize,
    /// csv の終わりの、リプレイの終わりより後の行（比べない。食い違いに数えない）。
    pub after_end: usize,
    pub first_diff: Option<Diff>,
}

impl Comparison {
    /// 全部一致（0 行は「比べられていない」ので一致と言わない）。
    pub fn all_match(&self) -> bool {
        self.csv_rows > 0 && self.mismatched == 0
    }
}

/// 一時停止のキー（入力語の 0x0008。csv では 1P・2P のどちらの語にも出る）。押下の行は csv の語が 0x0008 だけで
/// リプレイの語は 0（実測）。止まっている間のフレームがリプレイに入らないことの原本は一時停止の旗 `0x004A7ECC`。
const PAUSE_BIT: u16 = 0x0008;
/// 止まっていた区間の後、リプレイと続けて合うことを確かめる行数。短いと、0 が続く所で偽の一致を拾う。
const RESUME_WINDOW: usize = 120;
/// 再開のウィンドウの、先頭の `RESUME_MIN_WINDOW` 行より後で違ってよい行数（実物では 0 で足りた）。
const RESUME_TOLERANCE: usize = 5;
/// これより少ない行しか残っていなければ、再開を確かめられないので一時停止とは見なさない。
const RESUME_MIN_WINDOW: usize = 30;
/// リプレイが終わった後の csv の行（食い違いに数えない）の上限。リプレイはマッチの終わりで切れるが、ネットワークの列はその後の
/// 数秒の入力も持つ（実測 119〜176 行）。10 秒 = 600 行を超えるなら offset の誤りやリプレイの欠けを疑って食い違いに数える。
const MAX_TAIL_ROWS: usize = 600;
/// 止まっていた区間の長さの上限（行）。実物は 849・1,017 行（約 14〜17 秒）。1 時間分で打ち切る。
const MAX_PAUSE_ROWS: usize = 216_000;

fn replay_word(st: &StageInputs, frame: u32, offset: i64) -> Option<(u16, u16)> {
    let idx = i64::from(frame).checked_add(offset)?;
    let idx = usize::try_from(idx).ok()?;
    Some((*st.p1_inputs.get(idx)?, *st.p2_inputs.get(idx)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowClass {
    Match,
    PausePress,
    Paused,
    /// csv の終わりの、リプレイの終わりより後の行（`MAX_TAIL_ROWS` まで）。
    AfterEnd,
    OutOfRange,
    Diff {
        p1_bad: bool,
        p2_bad: bool,
    },
}

fn classify(r: &NetRow, w: Option<(u16, u16)>, mask: u16) -> RowClass {
    let Some((a, b)) = w else {
        return RowClass::OutOfRange;
    };
    let p1_bad = (a & mask) != (r.p1 & mask);
    let p2_bad = (b & mask) != (r.p2 & mask);
    if !p1_bad && !p2_bad {
        return RowClass::Match;
    }
    // 押下: 違う側は「0x0008 だけが csv に多い」形に限る（他のビットも違えば、押下ではなく食い違い）。
    let is_press = |bad: bool, c: u16, rp: u16| {
        !bad || (((c ^ rp) & mask) == (PAUSE_BIT & mask) && (c & PAUSE_BIT & mask) != 0)
    };
    if is_press(p1_bad, r.p1, a) && is_press(p2_bad, r.p2, b) {
        RowClass::PausePress
    } else {
        RowClass::Diff { p1_bad, p2_bad }
    }
}

/// 一時停止の後の再開を探す。押下の最初の行 `press_start` 以降の行 `p` で止まり、行 `a`（`> mismatch_row`）で再開した、
/// と置く。行 `p` に当たるリプレイの位置 `r` から `RESUME_WINDOW` 行が行 `a` から先の csv と合えば採る。
/// 0 が続くと止まった行は決まらないので、`p`・`a` の小さい方から採る。`base` は「csv の frame → リプレイの位置」の差
/// （offset ＋ これまでに止まった分）。返すのは `(p, a, 再開後の base)`。行は連続した frame という前提。
fn find_resume(
    rows: &[NetRow],
    st: &StageInputs,
    base: i64,
    mask: u16,
    press_start: usize,
    mismatch_row: usize,
) -> Option<(usize, usize, i64)> {
    let n = rows.len();
    let rep_n = st.p1_inputs.len().min(st.p2_inputs.len());
    for p in press_start..=mismatch_row {
        let r = i64::from(rows[p].frame) + base;
        let Ok(ru) = usize::try_from(r) else { continue };
        let a_hi = p.saturating_add(MAX_PAUSE_ROWS).min(n);
        for a in (mismatch_row + 1)..a_hi {
            // ウィンドウは、csv の残りとリプレイの残りの短い方まで（再開の後にマッチが終わる所まで）。
            let win = RESUME_WINDOW.min(n - a).min(rep_n.saturating_sub(ru));
            if win < RESUME_MIN_WINDOW {
                break;
            }
            // 再開の直後に 1 行でも本当の食い違いがあると、止まり全体が見つからず、その先が全部食い違いになるので、
            // 先頭の `RESUME_MIN_WINDOW` 行は 1 行も違わず、残りは `RESUME_TOLERANCE` 行まで違ってよいとする。
            let bad = |q: usize| {
                (st.p1_inputs[ru + q] & mask) != (rows[a + q].p1 & mask)
                    || (st.p2_inputs[ru + q] & mask) != (rows[a + q].p2 & mask)
            };
            let ok = (0..RESUME_MIN_WINDOW).all(|q| !bad(q))
                && (RESUME_MIN_WINDOW..win).filter(|&q| bad(q)).count() <= RESUME_TOLERANCE;
            if ok {
                return Some((p, a, r - i64::from(rows[a].frame)));
            }
        }
    }
    None
}

fn tally(
    rows: &[NetRow],
    st: &StageInputs,
    class: &[RowClass],
    replays: &[Option<(u16, u16)>],
) -> Comparison {
    let mut c = Comparison {
        csv_rows: rows.len(),
        replay_frames: st.frames(),
        matched: 0,
        p1_diff: 0,
        p2_diff: 0,
        out_of_range: 0,
        mismatched: 0,
        pause_press: 0,
        pauses: 0,
        paused_rows: 0,
        after_end: 0,
        first_diff: None,
    };
    for (i, k) in class.iter().enumerate() {
        if matches!(k, RowClass::OutOfRange | RowClass::Diff { .. }) && c.first_diff.is_none() {
            c.first_diff = Some(Diff {
                row: i,
                csv: rows[i],
                replay: replays[i],
            });
        }
        match *k {
            RowClass::Match => c.matched += 1,
            RowClass::PausePress => c.pause_press += 1,
            RowClass::Paused => c.paused_rows += 1,
            RowClass::AfterEnd => c.after_end += 1,
            RowClass::OutOfRange => {
                c.out_of_range += 1;
                c.mismatched += 1;
            }
            RowClass::Diff { p1_bad, p2_bad } => {
                c.p1_diff += usize::from(p1_bad);
                c.p2_diff += usize::from(p2_bad);
                c.mismatched += 1;
            }
        }
    }
    c
}

/// 本体。`pause_aware` が偽なら、一時停止を考えない（押下の行も食い違い。`scan_offsets` が使う）。
fn compare_inner(
    rows: &[NetRow],
    st: &StageInputs,
    offset: i64,
    mask: u16,
    pause_aware: bool,
) -> Comparison {
    let mut class: Vec<RowClass> = Vec::with_capacity(rows.len());
    let mut pauses = 0usize;
    // リプレイの位置 = csv の frame + base（止まっていた分だけ base が減る）。
    let mut base = offset;
    let mut press_start: Option<usize> = None;
    // 行ごとのリプレイの語（止まっていた行は None）。
    let mut replays: Vec<Option<(u16, u16)>> = Vec::with_capacity(rows.len());
    let mut i = 0usize;
    while i < rows.len() {
        let r = &rows[i];
        let w = replay_word(st, r.frame, base);
        let mut k = classify(r, w, mask);
        if !pause_aware && k == RowClass::PausePress {
            k = RowClass::Diff {
                p1_bad: (w.map_or(0, |x| x.0) & mask) != (r.p1 & mask),
                p2_bad: (w.map_or(0, |x| x.1) & mask) != (r.p2 & mask),
            };
        }
        if k == RowClass::PausePress && press_start.is_none() {
            press_start = Some(i);
        }
        if let (RowClass::Diff { .. }, Some(ps)) = (k, press_start) {
            // 押下の後の最初の食い違い。止まっていたなら再開の行まで飛ばす。見つからなければ次の押下まで探さない。
            press_start = None;
            if let Some((p, a, new_base)) = find_resume(rows, st, base, mask, ps, i) {
                for c in class.iter_mut().skip(p) {
                    *c = RowClass::Paused;
                }
                class.resize(a, RowClass::Paused);
                replays.resize(a, None);
                pauses += 1;
                base = new_base;
                i = a;
                continue;
            }
        }
        class.push(k);
        replays.push(w);
        i += 1;
    }
    if pause_aware {
        let tail = class
            .iter()
            .rev()
            .take_while(|k| **k == RowClass::OutOfRange)
            .count();
        if (1..=MAX_TAIL_ROWS).contains(&tail) {
            let from = class.len() - tail;
            class[from..].fill(RowClass::AfterEnd);
        }
    }
    let mut c = tally(rows, st, &class, &replays);
    c.pauses = pauses;
    c
}

/// `rows`（1 つの seg の行）を、リプレイの 1 つの枠と比べる。`mask` は両側に掛けてから比べる。一時停止
/// （押下の後、リプレイのフレームが進まない区間）は飛ばし、止まっていた区間とその前の押下の行は食い違いに数えない。
pub fn compare(rows: &[NetRow], st: &StageInputs, offset: i64, mask: u16) -> Comparison {
    compare_inner(rows, st, offset, mask, true)
}

/// `-range..=range` の offset ごとの一致数を、多い順（同数なら offset の絶対値が小さい順）に返す。
/// 一時停止は考えない（601 通りで再開を探すと重いため）。
pub fn scan_offsets(rows: &[NetRow], st: &StageInputs, mask: u16, range: i64) -> Vec<(i64, usize)> {
    let mut v: Vec<(i64, usize)> = (-range..=range)
        .map(|o| (o, compare_inner(rows, st, o, mask, false).matched))
        .collect();
    v.sort_by_key(|&(o, m)| (std::cmp::Reverse(m), o.abs(), o));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_build::{BuildSpec, build_replay};
    use crate::replay_decode::decode_inputs;

    fn stage(p1: Vec<u16>, p2: Vec<u16>) -> StageInputs {
        let f = build_replay(&BuildSpec::match_replay(0x41, p1, p2));
        decode_inputs(&f).unwrap().stages.remove(0)
    }

    fn rows(seg: u16, words: &[(u16, u16)]) -> Vec<NetRow> {
        words
            .iter()
            .enumerate()
            .map(|(i, &(a, b))| NetRow {
                seg,
                frame: i as u32,
                p1: a,
                p2: b,
            })
            .collect()
    }

    #[test]
    fn parse_net_frames_reads_the_dll_columns_and_refuses_other_shapes() {
        let text =
            "seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,0001,00FF,3,,0\n1,7,8001,0010,3,120,5\n";
        let r = parse_net_frames(text).unwrap();
        assert_eq!(
            r,
            vec![
                NetRow {
                    seg: 0,
                    frame: 0,
                    p1: 0x0001,
                    p2: 0x00FF
                },
                NetRow {
                    seg: 1,
                    frame: 7,
                    p1: 0x8001,
                    p2: 0x0010
                },
            ]
        );
        assert_eq!(segs(&r), vec![0, 1]);
        assert_eq!(parse_net_frames(""), Err(CsvError::Empty));
        assert!(matches!(
            parse_net_frames("seg,frame,p1,p2\n0,0,0,0\n"),
            Err(CsvError::BadHeader(_))
        ));
        assert!(matches!(
            parse_net_frames("seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,0001,00FF\n"),
            Err(CsvError::BadRow {
                line: 2,
                what: "列の数が 7 でない"
            })
        ));
        assert!(matches!(
            parse_net_frames("seg,frame,p1,p2,delay,margin_us,wait_us\n0,0,00G1,0,0,,0\n"),
            Err(CsvError::BadRow {
                line: 2,
                what: "1P"
            })
        ));
        assert_eq!(
            parse_net_frames("seg,frame,p1,p2,delay,margin_us,wait_us\n").unwrap(),
            vec![]
        );
        let new =
            format!("{NET_FRAMES_HEADER}\n0,0,0001,00FF,3,,0,0\n1,7,8001,0010,3,120,5,8100\n");
        assert_eq!(parse_net_frames(&new).unwrap(), r);
        assert!(matches!(
            parse_net_frames(&format!("{NET_FRAMES_HEADER}\n0,0,0001,00FF,3,,0\n")),
            Err(CsvError::BadRow {
                line: 2,
                what: "列の数が 8 でない"
            })
        ));
        assert!(matches!(
            parse_net_frames(&format!("{NET_FRAMES_HEADER_V1}\n0,0,0001,00FF,3,,0,0\n")),
            Err(CsvError::BadRow {
                line: 2,
                what: "列の数が 7 でない"
            })
        ));
    }

    #[test]
    fn compare_counts_each_side_and_finds_the_first_difference() {
        let st = stage(vec![1, 2, 3, 4, 5, 6], vec![0, 0, 8, 0, 0x10, 0]);
        let same = rows(0, &[(1, 0), (2, 0), (3, 8), (4, 0), (5, 0x10), (6, 0)]);
        let c = compare(&same, &st, 0, 0xFFFF);
        assert!(c.all_match());
        assert_eq!((c.csv_rows, c.replay_frames, c.matched), (6, 6, 6));
        assert_eq!(c.first_diff, None);

        let mut r = same.clone();
        r[1].p2 = 0x40;
        r[4].p2 = 0;
        let c = compare(&r, &st, 0, 0xFFFF);
        assert!(!c.all_match());
        assert_eq!(
            (c.matched, c.p1_diff, c.p2_diff, c.out_of_range),
            (4, 0, 2, 0)
        );
        assert_eq!(c.first_diff.unwrap().row, 1);
        assert_eq!(c.first_diff.unwrap().replay, Some((2, 0)));
        assert_eq!(compare(&r, &st, 0, 0xFFBF).matched, 5);
        assert_eq!(compare(&r, &st, 0, 0xFFAF).matched, 6);
        let mut r = same.clone();
        r[5].p1 = 0;
        let c = compare(&r, &st, 0, 0xFFFF);
        assert_eq!((c.p1_diff, c.p2_diff), (1, 0));
        let mut long = same.clone();
        long.extend(
            rows(0, &[(0, 0), (0, 0)])
                .into_iter()
                .enumerate()
                .map(|(i, mut x)| {
                    x.frame = 6 + i as u32;
                    x
                }),
        );
        let c = compare(&long, &st, 0, 0xFFFF);
        assert_eq!((c.matched, c.after_end, c.out_of_range), (6, 2, 0));
        assert!(c.all_match());
        let c = compare(&same, &st, -1, 0xFFFF);
        assert_eq!((c.out_of_range, c.after_end), (1, 0));
        assert!(!c.all_match());
        assert!(!compare(&[], &st, 0, 0xFFFF).all_match());
    }

    #[test]
    fn offset_shifts_the_replay_frame_and_scan_finds_it() {
        let body: Vec<(u16, u16)> = (0..40u16)
            .map(|i| (i + 1, if i % 3 == 0 { 0x10 } else { 0 }))
            .collect();
        let mut p1 = vec![0u16; 3];
        let mut p2 = vec![0u16; 3];
        for (a, b) in &body {
            p1.push(*a);
            p2.push(*b);
        }
        let st = stage(p1, p2);
        let r = rows(0, &body);
        assert!(compare(&r, &st, 3, 0xFFFF).all_match());
        assert!(!compare(&r, &st, 0, 0xFFFF).all_match());
        assert!(!compare(&r, &st, 2, 0xFFFF).all_match());
        assert_eq!(compare(&r, &st, -1, 0xFFFF).out_of_range, 1);
        let scan = scan_offsets(&r, &st, 0xFFFF, 8);
        assert_eq!(scan[0], (3, 40));
        assert!(scan.iter().skip(1).all(|&(_, m)| m < 40));
        assert_eq!(scan.len(), 17);
    }

    #[test]
    fn inputs_csv_lists_every_frame_with_hex_words() {
        let st = stage(vec![0x0001, 0x8010], vec![0x0000, 0x0004]);
        let r = ReplayInputs {
            difficulty: 1,
            game_mode: 2,
            stages: vec![st],
        };
        assert_eq!(
            inputs_csv(&r),
            "slot,frame,p1,p2,p3\n9,0,0001,0000,0001\n9,1,8010,0004,8014\n"
        );
    }

    /// 合成のマッチの k 番目の、ゲームの入力語。0x0008 は含めない。周期を持たない式にして、ずらして合う偽の一致を作らない。
    fn game(k: usize) -> (u16, u16) {
        let k = k as u32;
        let a = (k.wrapping_mul(2_654_435_761) >> 11) as u16 & 0x03F7;
        let b = (k.wrapping_mul(40_503).wrapping_add(k * k) >> 2) as u16 & 0x03F7;
        (a, b)
    }

    #[derive(Clone, Copy)]
    enum Ev {
        Game(usize),
        /// 一時停止のキーを n フレーム押す。ゲームは進むが csv の語に 0x0008 が付く（リプレイは押下を記録しない）。
        /// `true` なら 1P。
        Press(usize, bool),
        /// 止まっている間の n 行（csv だけに入る。途中に 0x0008 の行も混ぜる）。
        Pause(usize),
    }

    /// `(csv の行, リプレイの入力列)`。リプレイの先頭に `lead` フレームの余分を置く。
    fn synth(plan: &[Ev], lead: usize) -> (Vec<NetRow>, Vec<(u16, u16)>) {
        let mut rows = Vec::new();
        let mut rep: Vec<(u16, u16)> = (0..lead).map(|_| (0, 0)).collect();
        let mut game_frames = 0usize;
        let push = |rows: &mut Vec<NetRow>, a: u16, b: u16| {
            let frame = rows.len() as u32;
            rows.push(NetRow {
                seg: 0,
                frame,
                p1: a,
                p2: b,
            });
        };
        for ev in plan {
            match *ev {
                Ev::Game(n) | Ev::Press(n, _) => {
                    for _ in 0..n {
                        let (a, b) = game(game_frames);
                        game_frames += 1;
                        rep.push((a, b));
                        let (ca, cb) = match *ev {
                            Ev::Press(_, true) => (a | PAUSE_BIT, b),
                            Ev::Press(_, false) => (a, b | PAUSE_BIT),
                            _ => (a, b),
                        };
                        push(&mut rows, ca, cb);
                    }
                }
                Ev::Pause(n) => {
                    for j in 0..n {
                        let a = if j % 9 < 3 { 0x0080 } else { 0 };
                        let b = if j == 20 {
                            PAUSE_BIT
                        } else if j % 11 < 2 {
                            0x0040
                        } else {
                            0
                        };
                        push(&mut rows, a, b);
                    }
                }
            }
        }
        (rows, rep)
    }

    fn stage_of(rep: &[(u16, u16)]) -> StageInputs {
        stage(
            rep.iter().map(|w| w.0).collect(),
            rep.iter().map(|w| w.1).collect(),
        )
    }

    fn two_pauses() -> Vec<Ev> {
        vec![
            Ev::Game(100),
            Ev::Press(6, false),
            Ev::Game(5),
            Ev::Pause(40),
            Ev::Game(190),
            Ev::Press(8, true),
            Ev::Game(3),
            Ev::Pause(70),
            Ev::Game(100),
        ]
    }

    #[test]
    fn two_pauses_are_skipped_and_every_row_is_explained() {
        for lead in [0usize, 1] {
            let (rows, rep) = synth(&two_pauses(), lead);
            let st = stage_of(&rep);
            let c = compare(&rows, &st, lead as i64, 0xFFFF);
            assert_eq!(c.csv_rows, 100 + 6 + 5 + 40 + 190 + 8 + 3 + 70 + 100);
            assert_eq!(c.pauses, 2, "lead {lead}");
            assert_eq!(c.paused_rows, 40 + 70);
            assert_eq!(c.pause_press, 6 + 8);
            assert_eq!(
                (c.mismatched, c.p1_diff, c.p2_diff, c.out_of_range),
                (0, 0, 0, 0)
            );
            assert_eq!(c.matched, 412 - 14);
            assert_eq!(c.after_end, 0);
            assert_eq!(
                c.matched + c.pause_press + c.paused_rows + c.after_end,
                c.csv_rows
            );
            assert_eq!(c.first_diff, None);
            assert!(c.all_match());
        }
    }

    #[test]
    fn pause_aware_compare_still_counts_ordinary_mismatches() {
        let (rows, rep) = synth(&two_pauses(), 0);
        let st = stage_of(&rep);
        let mut r = rows.clone();
        r[30].p1 ^= 0x0100;
        let c = compare(&r, &st, 0, 0xFFFF);
        assert_eq!((c.mismatched, c.p1_diff, c.p2_diff, c.pauses), (1, 1, 0, 2));
        assert_eq!(c.first_diff.unwrap().row, 30);
        assert!(!c.all_match());
        let mut r = rows.clone();
        r[200].p2 ^= 0x0001;
        let c = compare(&r, &st, 0, 0xFFFF);
        assert_eq!((c.mismatched, c.p2_diff, c.pauses), (1, 1, 2));
        assert_eq!(c.first_diff.unwrap().row, 200);
        let mut r = rows.clone();
        let last = r.len() - 10;
        r[last].p1 ^= 0x0002;
        let c = compare(&r, &st, 0, 0xFFFF);
        assert_eq!((c.mismatched, c.pauses), (1, 2));
        assert_eq!(c.first_diff.unwrap().row, last);
        let mut r = rows.clone();
        r[102].p2 ^= 0x0100;
        let c = compare(&r, &st, 0, 0xFFFF);
        assert_eq!(c.mismatched, 1);
        assert_eq!(c.pause_press, 6 - 1 + 8);
        let mut rep2 = rep.clone();
        rep2[30].1 |= PAUSE_BIT;
        let c = compare(&rows, &stage_of(&rep2), 0, 0xFFFF);
        assert!(!c.all_match());
    }

    #[test]
    fn pause_press_without_a_stop_and_stray_rows_are_not_hidden() {
        let plan = [Ev::Game(100), Ev::Press(6, false), Ev::Game(100)];
        let (rows, rep) = synth(&plan, 0);
        let c = compare(&rows, &stage_of(&rep), 0, 0xFFFF);
        assert_eq!((c.pauses, c.paused_rows, c.pause_press), (0, 0, 6));
        assert!(c.all_match());
        let plan = [
            Ev::Game(100),
            Ev::Press(6, false),
            Ev::Game(5),
            Ev::Pause(1),
            Ev::Game(150),
        ];
        let (rows, rep) = synth(&plan, 0);
        let c = compare(&rows, &stage_of(&rep), 0, 0xFFFF);
        assert_eq!((c.pauses, c.paused_rows), (1, 1));
        let mut bad = rows.clone();
        bad[113].p1 ^= 0x0100;
        let c = compare(&bad, &stage_of(&rep), 0, 0xFFFF);
        assert!(!c.all_match());
        assert!(c.mismatched > 0);
        let plan = [Ev::Game(100), Ev::Pause(40), Ev::Game(150)];
        let (rows, rep) = synth(&plan, 0);
        let c = compare(&rows, &stage_of(&rep), 0, 0xFFFF);
        assert_eq!((c.pauses, c.paused_rows), (0, 0));
        assert!(!c.all_match());
        assert!(c.mismatched > 0);
    }

    #[test]
    fn rows_after_the_replay_end_are_counted_apart_up_to_a_limit() {
        let (mut rows, rep) = synth(&[Ev::Game(50)], 0);
        for i in 0..176u32 {
            rows.push(NetRow {
                seg: 0,
                frame: 50 + i,
                p1: 0x0001,
                p2: 0,
            });
        }
        let st = stage_of(&rep);
        let c = compare(&rows, &st, 0, 0xFFFF);
        assert_eq!((c.matched, c.after_end, c.mismatched), (50, 176, 0));
        assert!(c.all_match());
        for i in 176..700u32 {
            rows.push(NetRow {
                seg: 0,
                frame: 50 + i,
                p1: 0x0001,
                p2: 0,
            });
        }
        let c = compare(&rows, &st, 0, 0xFFFF);
        assert_eq!((c.after_end, c.out_of_range), (0, 700));
        assert!(!c.all_match());
        assert_eq!(c.first_diff.unwrap().row, 50);
    }
}
