//! 戻り道（`return_flow`）が読む番地を、戻り道を動かさない実機の回の `_menu.csv` で照らす（pure）。
//!
//! 読むのは一時停止のメニュー（`0x4D66F8`）と終わりのメニュー（`0x4D8C00`）の状態・フレームの数え、一時停止の旗、終わりのメニューの印。
//! DLL の読み取りのスレッドが約 1 ms ごとに読み、変わったときだけ（開発用のビルドはフレームの数えが変わるたびに）1 行書く
//! （`higanrtool_dll::menu`）。行の `seq_after` は、読んだ直後までに入力の地点が通った数（= ゲームの tick の通し番号）。
//!
//! 照らすこと（`Finding::check` の名前の前半が一時停止 `pause` / 終わりのメニュー `end`）:
//! - 状態が `return_flow::pause::STATES` / `end_menu::STATES` の範囲内（`*.state_range`）。
//! - 状態の移り方が逆アセンブルで読んだ表（`PAUSE_MOVES` / `END_MOVES`）の中（`*.transitions`）。
//! - 入力で動く移りは、前の行のフレームの数えが `MENU_READ_FRAMES` 以上（`*.gate`）。リセットする移りは新しい状態の数えが 0 から、
//!   しない移りは数えが続く（`*.frames_reset`）。
//! - 再開の確定（状態 3）・タイトルへ（状態 8）から旗が下りるまでが `CLOSE_FRAMES` tick（`pause.close_delay`）。
//! - 数えが tick より速く進まない（`*.frames_vs_ticks`）。
//! - 求めた移りが全部見えた（`*.coverage`。台本が押したものが届いたか）。
//! - 1 回目の一時停止の移りの列が `PAUSE_FIRST_MOVES` と一致する（`pause.first_moves`。読み始め前の入力が効かなかった裏づけ）。
//!
//! 母数 0 は「測れていない」: 求めたのに一時停止・終わりのメニューの区間が 1 つも無いときは `Status::NotMeasured` で、合格に数えない。
//! 合否（`Report::not_all_passed`）を決めるのは、求めた区間・求めた検査の結果だけ。求めていない区間の結果は照らして出すが数えない。
#![forbid(unsafe_code)]

use std::fmt::Write as _;

use crate::return_flow::{MENU_READ_FRAMES, end_menu, pause};

/// `_menu.csv` の見出し。`t_us` は読み取りのスレッドが始まってからの µs（QueryPerformanceCounter）。
/// 末尾の 6 欄が一時停止と終わりのメニュー（`parse_menu_csv` が列の名前で引く）。書き手は `higanrtool_dll::menu`。
pub const MENU_CSV_HEADER: &str = "seq_before,seq_after,t_us,menu_ptr,screen_id,substate,cursor,prev_screen,transitioning,anim,p1_ptr,p1_life,p2_ptr,p2_life,replay_ptr,replay_counter,replay_mode,gui_ptr,gui_talk,pause_flag,pause_state,pause_frames,end_flag,end_state,end_frames";

/// 再開の確定（一時停止の状態 3）・タイトルへ（同 8）・終わりのメニューの決定（同 4・5・6）から次へ進むまでのフレーム
/// （`cmpl $0x14,0x4(%esi)`: `0x00434ACC` / `0x00434D74` / `0x00434D9A` / `0x004359F8` / `0x00435A14` / `0x00435A30`）。
pub const CLOSE_FRAMES: u32 = 20;
/// tick の数えの許容（読むのは 1 ms ごとで、1 tick の途中で読みうる）。
const TICK_SLACK: u32 = 1;

/// 一時停止の状態の移り（`0x00434740`）。(から, へ, 数えをリセットするか)。
/// 出どころ: 0→1 `0x00434878`・1→2 `0x00434911` / `0x00434936`・1→3 `0x0043497F`・2→1 `0x00434A20` / `0x00434A4F`・
/// 2→5 `0x00434ABD`・5→4 / 4→5 `0x00434BF2` / `0x00434CF6`・5→2 `0x00434D62`・4→8 `0x00434C4E`。
/// 3→0 / 8→0 は閉じる呼びの途中（状態を 0 にしてから旗を下ろす 2 命令の間 `0x00434AD2`〜`0x00434AD8`）で読んだときの行。
const PAUSE_MOVES: [(u32, u32, bool); 11] = [
    (pause::INIT, pause::RESUME, true),
    (pause::RESUME, pause::TITLE, false),
    (pause::RESUME, pause::RESUMING, true),
    (pause::TITLE, pause::RESUME, false),
    (pause::TITLE, pause::NO, true),
    (pause::NO, pause::YES, false),
    (pause::YES, pause::NO, false),
    (pause::NO, pause::TITLE, true),
    (pause::YES, pause::LEAVING, true),
    (pause::RESUMING, pause::INIT, false),
    (pause::LEAVING, pause::INIT, false),
];

/// Esc（`0x0043474B push $0x8`）と Q（`0x004347AD push $0x200`）は、どの状態からでも 3 / 8 へ数えをリセットして、入力のゲートなしで移る。
fn pause_shortcut(to: u32) -> bool {
    to == pause::RESUMING || to == pause::LEAVING
}

/// 終わりのメニューの状態の移り（`0x004355F0`）。上下は循環。出どころ: 1→3 `0x00435715`・1→2 `0x0043573A`・1→4 `0x0043577F`・
/// 2→1 `0x0043585E`・2→3 `0x00435883`・3→2 `0x0043597D`・3→1 `0x004359A2`。2→5 / 3→6 は決定（`0x00435A00` 前後）。
/// 4→0 / 5→0 / 6→0 は閉じる呼びの途中（状態を 0 にしてから旗を下ろす 2 命令の間 `0x00435A1E` / `0x00435A3A` → `0x00435A4A`）で読んだときの行。
const END_MOVES: [(u32, u32, bool); 13] = [
    (end_menu::INIT, end_menu::REMATCH, true),
    (end_menu::REMATCH, end_menu::SAVE, false),
    (end_menu::REMATCH, end_menu::CHARA, false),
    (end_menu::REMATCH, 4, true),
    (end_menu::CHARA, end_menu::REMATCH, false),
    (end_menu::CHARA, end_menu::SAVE, false),
    (end_menu::CHARA, 5, true),
    (end_menu::SAVE, end_menu::CHARA, false),
    (end_menu::SAVE, end_menu::REMATCH, false),
    (end_menu::SAVE, 6, true),
    (4, end_menu::INIT, false),
    (5, end_menu::INIT, false),
    (6, end_menu::INIT, false),
];

/// 台本が通すと求める移り（`Expect`）。一時停止: 再開と「タイトルへ戻る」の往復・確かめの枠の「いいえ」「はい」の往復と「いいえ」の決定・再開の確定。
pub const PAUSE_REQUIRED: [(u32, u32); 8] = [
    (pause::INIT, pause::RESUME),
    (pause::RESUME, pause::TITLE),
    (pause::TITLE, pause::RESUME),
    (pause::RESUME, pause::RESUMING),
    (pause::TITLE, pause::NO),
    (pause::NO, pause::YES),
    (pause::YES, pause::NO),
    (pause::NO, pause::TITLE),
];
/// 終わりのメニュー: 下と上を一周ずつ（再戦 → キャラ → 保存 → 再戦、再戦 → 保存 → キャラ → 再戦）と、再戦の決定。
pub const END_REQUIRED: [(u32, u32); 8] = [
    (end_menu::INIT, end_menu::REMATCH),
    (end_menu::REMATCH, end_menu::CHARA),
    (end_menu::CHARA, end_menu::SAVE),
    (end_menu::SAVE, end_menu::REMATCH),
    (end_menu::REMATCH, end_menu::SAVE),
    (end_menu::SAVE, end_menu::CHARA),
    (end_menu::CHARA, end_menu::REMATCH),
    (end_menu::REMATCH, 4),
];

/// 台本の 1 回目の一時停止（開いた直後の下 → 下 → 上 → Esc）で、旗が立っている間に起きる状態の移りの列。開いた直後の下は数えが
/// `MENU_READ_FRAMES` に届く前なので効かず、効けば 1→2 の後に 2→1・1→2・2→3 と続いて列が変わる。
pub const PAUSE_FIRST_MOVES: [(u32, u32); 4] = [
    (pause::INIT, pause::RESUME),
    (pause::RESUME, pause::TITLE),
    (pause::TITLE, pause::RESUME),
    (pause::RESUME, pause::RESUMING),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Menu {
    flag: Option<u32>,
    state: Option<u32>,
    frames: Option<u32>,
}

/// `_menu.csv` の 1 行（照らすのに要る欄だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub seq_before: u32,
    pub seq_after: u32,
    pub screen_id: Option<i32>,
    pub substate: Option<i32>,
    pub cursor: Option<i32>,
    pause: Menu,
    end: Menu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// 見出しの行が無い。
    NoHeader,
    /// 要る列が見出しに無い（古い版の `_menu.csv`）。
    MissingColumn(&'static str),
    /// 行 `line`（1 から）の列 `column` が数として読めない。
    BadNumber { line: usize, column: &'static str },
    /// 行 `line` の欄の数が見出しと違う。
    Width { line: usize },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NoHeader => write!(f, "見出しの行（seq_before,…）が無い"),
            ParseError::MissingColumn(c) => write!(
                f,
                "列 {c} が見出しに無い（新しいバージョンの DLL が書いた _menu.csv ではない）"
            ),
            ParseError::BadNumber { line, column } => {
                write!(f, "{line} 行目の列 {column} が数として読めない")
            }
            ParseError::Width { line } => write!(f, "{line} 行目の欄の数が見出しと違う"),
        }
    }
}

const COLUMNS: [&str; 11] = [
    "seq_before",
    "seq_after",
    "screen_id",
    "substate",
    "cursor",
    "pause_flag",
    "pause_state",
    "pause_frames",
    "end_flag",
    "end_state",
    "end_frames",
];

/// `_menu.csv` を読む。見出しの行は列の名前で引く（列の並びに依らない）。空の欄は「読めなかった」で `None`。
pub fn parse_menu_csv(text: &str) -> Result<Vec<Row>, ParseError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut lines = text.lines().enumerate();
    let (_, head) = lines
        .by_ref()
        .find(|(_, l)| l.starts_with("seq_before,"))
        .ok_or(ParseError::NoHeader)?;
    let names: Vec<&str> = head.split(',').collect();
    let mut at = [0usize; COLUMNS.len()];
    for (slot, want) in at.iter_mut().zip(COLUMNS) {
        *slot = names
            .iter()
            .position(|n| *n == want)
            .ok_or(ParseError::MissingColumn(want))?;
    }
    let mut rows = Vec::new();
    for (i, l) in lines {
        if l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split(',').collect();
        if f.len() != names.len() {
            return Err(ParseError::Width { line: i + 1 });
        }
        let bad = |k: usize| ParseError::BadNumber {
            line: i + 1,
            column: COLUMNS[k],
        };
        let num = |k: usize| -> Result<Option<i64>, ParseError> {
            let s = f[at[k]].trim();
            if s.is_empty() {
                return Ok(None);
            }
            s.parse::<i64>().map(Some).map_err(|_| bad(k))
        };
        let seq = |k: usize| -> Result<u32, ParseError> {
            num(k)?.and_then(|v| u32::try_from(v).ok()).ok_or(bad(k))
        };
        // 状態・数えは `(x as i32)` で書かれる。想定の外の値も落とさず `u32` に直して持つ（範囲の検査が見る）。
        let un = |k: usize| num(k).map(|o| o.map(|v| v as u32));
        let sn = |k: usize| num(k).map(|o| o.map(|v| v as i32));
        rows.push(Row {
            seq_before: seq(0)?,
            seq_after: seq(1)?,
            screen_id: sn(2)?,
            substate: sn(3)?,
            cursor: sn(4)?,
            pause: Menu {
                flag: un(5)?,
                state: un(6)?,
                frames: un(7)?,
            },
            end: Menu {
                flag: un(8)?,
                state: un(9)?,
                frames: un(10)?,
            },
        });
    }
    Ok(rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// 測れていない（母数 0・列が空）。合格に数えない。
    NotMeasured,
    /// 合否にしない記録。
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: String,
    pub status: Status,
    pub detail: String,
    /// 合否に数えるか。求めていない区間の結果は照らして出すが数えない。
    pub counts: bool,
}

/// 何を求めるか。`false` の区間は、あれば照らして結果を出すが、合否には数えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Expect {
    /// 一時停止の `PAUSE_REQUIRED` の移りが全部見えること。
    pub pause: bool,
    /// 終わりのメニューの `END_REQUIRED` の移りが全部見えること。
    pub end: bool,
    /// 1 回目の一時停止の移りの列が `PAUSE_FIRST_MOVES` と一致すること（`pause` を求めなくても、これだけ求められる）。
    pub pause_first: bool,
    /// 最後の行の画面とカーソル（戻り道の到着: 画面 6・カーソル 4）。
    pub final_screen: Option<(u32, i32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// 旗が立った区間の数（一時停止, 終わりのメニュー）。
    pub episodes: (usize, usize),
}

impl Report {
    pub fn failed(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.counts && f.status == Status::Fail)
    }

    /// 求めたものが落ちた、か測れていない。合格に数えない。求めていない区間の結果は数えない。
    pub fn not_all_passed(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.counts && matches!(f.status, Status::Fail | Status::NotMeasured))
    }

    pub fn find(&self, check: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.check == check)
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for f in &self.findings {
            let mark = match f.status {
                Status::Pass => "合格",
                Status::Fail => "不合格",
                Status::NotMeasured => "測れていない",
                Status::Info => "記録",
            };
            let skip = if f.counts {
                ""
            } else {
                "・求めていない"
            };
            let _ = writeln!(s, "[{mark}{skip}] {}: {}", f.check, f.detail);
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pause,
    End,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Pause => "pause",
            Kind::End => "end",
        }
    }

    fn menu(self, r: &Row) -> Menu {
        match self {
            Kind::Pause => r.pause,
            Kind::End => r.end,
        }
    }

    fn states(self) -> u32 {
        match self {
            Kind::Pause => pause::STATES,
            Kind::End => end_menu::STATES,
        }
    }

    fn moves(self) -> &'static [(u32, u32, bool)] {
        match self {
            Kind::Pause => &PAUSE_MOVES,
            Kind::End => &END_MOVES,
        }
    }

    fn required(self) -> &'static [(u32, u32)] {
        match self {
            Kind::Pause => &PAUSE_REQUIRED,
            Kind::End => &END_REQUIRED,
        }
    }

    /// 近道（どの状態からでも、数えをリセットして、入力のゲートなしで移る）か。
    fn shortcut(self, to: u32) -> bool {
        self == Kind::Pause && pause_shortcut(to)
    }
}

/// 旗が立っている行の連なり `rows[first..=last]`。`close` は旗が下りた最初の行。
#[derive(Debug, Clone, Copy)]
struct Episode {
    first: usize,
    last: usize,
    close: Option<usize>,
}

fn episodes(rows: &[Row], kind: Kind) -> Vec<Episode> {
    let mut out: Vec<Episode> = Vec::new();
    let mut open: Option<usize> = None;
    for (i, r) in rows.iter().enumerate() {
        let Some(flag) = kind.menu(r).flag else {
            continue;
        };
        match (flag != 0, open) {
            (true, None) => open = Some(i),
            (false, Some(first)) => {
                out.push(Episode {
                    first,
                    last: i - 1,
                    close: Some(i),
                });
                open = None;
            }
            _ => {}
        }
    }
    if let Some(first) = open {
        out.push(Episode {
            first,
            last: rows.len() - 1,
            close: None,
        });
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct Move {
    from: u32,
    to: u32,
    /// 新しい状態の最初の行。
    row: usize,
    /// 古い状態の最後の行のフレームの数え。
    prev_frames: u32,
    new_frames: u32,
}

/// 区間の状態の移り。旗が立った直後の最初の行は前の値が残っていることがある（メニューの関数が 1 回も走っていない）ので、
/// 状態が `INIT` のときだけ起点にし、そうでなければ 2 行目から数える。読めない行は飛ばす。
fn moves_of(rows: &[Row], ep: &Episode, kind: Kind) -> Vec<Move> {
    let mut out = Vec::new();
    let mut prev: Option<(u32, u32)> = None;
    for (n, r) in rows[ep.first..=ep.last].iter().enumerate() {
        let m = kind.menu(r);
        let (Some(state), Some(frames)) = (m.state, m.frames) else {
            continue;
        };
        if n == 0 && state != 0 {
            continue;
        }
        if let Some((ps, pf)) = prev
            && ps != state
        {
            out.push(Move {
                from: ps,
                to: state,
                row: ep.first + n,
                prev_frames: pf,
                new_frames: frames,
            });
        }
        prev = Some((state, frames));
    }
    out
}

/// 結果 1 件。落ちた詳細が無ければ `pass_text`、あれば詳細を並べて `Fail`。
fn verdict(out: &mut Vec<Finding>, kind: Kind, suffix: &str, bad: &[String], pass_text: String) {
    let (status, detail) = if bad.is_empty() {
        (Status::Pass, pass_text)
    } else {
        (Status::Fail, bad.join(" / "))
    };
    out.push(Finding {
        check: format!("{}.{suffix}", kind.name()),
        status,
        detail,
        counts: true,
    });
}

fn note(out: &mut Vec<Finding>, kind: Kind, suffix: &str, status: Status, detail: String) {
    out.push(Finding {
        check: format!("{}.{suffix}", kind.name()),
        status,
        detail,
        counts: true,
    });
}

/// 読めない行と範囲の外の状態。
fn check_readable(rows: &[Row], eps: &[Episode], kind: Kind, out: &mut Vec<Finding>) {
    let (mut unread, mut total) = (0usize, 0usize);
    let mut out_of_range = Vec::new();
    for r in eps.iter().flat_map(|ep| &rows[ep.first..=ep.last]) {
        total += 1;
        let m = kind.menu(r);
        match (m.state, m.frames) {
            (Some(s), Some(_)) => {
                if s >= kind.states() {
                    out_of_range.push(format!("seq {} の状態 {s}", r.seq_after));
                }
            }
            _ => unread += 1,
        }
    }
    let unreadable = if unread == 0 {
        Vec::new()
    } else {
        vec![format!(
            "状態か数えが読めなかった行 {unread}（{total} 行のうち）"
        )]
    };
    verdict(
        out,
        kind,
        "readable",
        &unreadable,
        format!("区間の中の {total} 行が全部読めた"),
    );
    verdict(
        out,
        kind,
        "state_range",
        &out_of_range,
        format!(
            "全部の行の状態が 0〜{}（STATES = {}）",
            kind.states() - 1,
            kind.states()
        ),
    );
}

/// 状態の移りの表・入力のゲート・数えのリセット。見えた移りを返す。
fn check_moves(
    rows: &[Row],
    eps: &[Episode],
    kind: Kind,
    out: &mut Vec<Finding>,
) -> Vec<(u32, u32)> {
    let (mut bad_moves, mut bad_gate, mut bad_reset) = (Vec::new(), Vec::new(), Vec::new());
    let mut seen = Vec::new();
    let (mut shortcut_early, mut n_moves) = (0usize, 0usize);
    for m in eps.iter().flat_map(|ep| moves_of(rows, ep, kind)) {
        n_moves += 1;
        let seq = rows[m.row].seq_after;
        seen.push((m.from, m.to));
        let shortcut = kind.shortcut(m.to);
        let reset = match kind.moves().iter().find(|t| t.0 == m.from && t.1 == m.to) {
            Some(t) => t.2,
            None if shortcut => true,
            None => {
                bad_moves.push(format!("seq {seq}: {}→{}", m.from, m.to));
                continue;
            }
        };
        // 入力で動く移り（0→1 は自動）は、前の状態のフレームの数えが読み始めの数え以上。近道は数えを問わない。
        if m.from != 0 && m.prev_frames < MENU_READ_FRAMES {
            if shortcut {
                shortcut_early += 1;
            } else {
                bad_gate.push(format!(
                    "seq {seq}: {}→{}（前の数え {}）",
                    m.from, m.to, m.prev_frames
                ));
            }
        }
        // リセットする移りは新しい状態の最初の行の数えが 0 か 1（0 にして、同じ呼びの終わりで 1 足す）、しない移りは数えが続く。
        let ok = if reset {
            m.new_frames <= 1 + TICK_SLACK
        } else {
            m.new_frames >= m.prev_frames
        };
        if !ok {
            bad_reset.push(format!(
                "seq {seq}: {}→{}（数え {}→{}、{}はず）",
                m.from,
                m.to,
                m.prev_frames,
                m.new_frames,
                if reset {
                    "リセットする"
                } else {
                    "続く"
                }
            ));
        }
    }
    verdict(
        out,
        kind,
        "transitions",
        &bad_moves,
        format!("状態の移り {n_moves} 回がすべて表の中"),
    );
    verdict(
        out,
        kind,
        "gate",
        &bad_gate,
        format!(
            "入力で動いた移りの前の数えが全部 {MENU_READ_FRAMES} 以上（近道で足りなかった移り {shortcut_early} 回は数えない）"
        ),
    );
    verdict(
        out,
        kind,
        "frames_reset",
        &bad_reset,
        "数えのリセットの有無が表のとおり".to_string(),
    );
    seen
}

/// 数えは tick より速く進まない（1 tick に 1 回の更新）。
fn check_ticks(rows: &[Row], eps: &[Episode], kind: Kind, out: &mut Vec<Finding>) {
    let mut too_fast = Vec::new();
    for ep in eps {
        let mut prev: Option<(u32, u32, u32)> = None;
        for r in &rows[ep.first..=ep.last] {
            let m = kind.menu(r);
            let (Some(s), Some(f)) = (m.state, m.frames) else {
                continue;
            };
            if let Some((ps, pf, pseq)) = prev
                && ps == s
                && f > pf
            {
                let (df, dseq) = (f - pf, r.seq_after.saturating_sub(pseq));
                if df > dseq + TICK_SLACK {
                    too_fast.push(format!(
                        "seq {}: 数え +{df} が tick +{dseq} より速い",
                        r.seq_after
                    ));
                }
            }
            prev = Some((s, f, r.seq_after));
        }
    }
    verdict(
        out,
        kind,
        "frames_vs_ticks",
        &too_fast,
        "数えの進みが tick の進み以下".to_string(),
    );
}

/// 再開の確定（3）かタイトルへ（8）に入ってから旗が下りるまで `CLOSE_FRAMES` tick。下りた行の状態は 0。
fn check_close(rows: &[Row], eps: &[Episode], out: &mut Vec<Finding>) {
    let (mut bad, mut seen) = (Vec::new(), 0usize);
    for ep in eps {
        let Some(close) = ep.close else { continue };
        let entered = rows[ep.first..=ep.last].iter().find(
            |r| matches!(r.pause.state, Some(s) if s == pause::RESUMING || s == pause::LEAVING),
        );
        let Some(r) = entered else {
            bad.push(format!(
                "seq {}: 旗が下りたのに、状態 {} も {} も通っていない",
                rows[close].seq_after,
                pause::RESUMING,
                pause::LEAVING
            ));
            continue;
        };
        seen += 1;
        let delta = rows[close].seq_after.saturating_sub(r.seq_after);
        let lo = CLOSE_FRAMES.saturating_sub(TICK_SLACK);
        if !(lo..=CLOSE_FRAMES + TICK_SLACK).contains(&delta) {
            bad.push(format!(
                "seq {}: 状態 {} に入ってから旗が下りるまで {delta} tick（{CLOSE_FRAMES} のはず）",
                r.seq_after,
                r.pause.state.unwrap_or(0)
            ));
        }
        if rows[close].pause.state != Some(pause::INIT) {
            bad.push(format!(
                "seq {}: 旗が下りた行の状態が 0 でない（{:?}）",
                rows[close].seq_after, rows[close].pause.state
            ));
        }
    }
    if bad.is_empty() && seen == 0 {
        note(
            out,
            Kind::Pause,
            "close_delay",
            Status::NotMeasured,
            "旗が下りた区間が 1 つも無い（閉じるところまで読めていない）".to_string(),
        );
    } else {
        verdict(
            out,
            Kind::Pause,
            "close_delay",
            &bad,
            format!(
                "閉じた {seen} 回とも、確定から旗が下りるまで {CLOSE_FRAMES}±{TICK_SLACK} tick で状態は 0"
            ),
        );
    }
}

/// 区間ごとの照らし。求めていない区間（`want` が偽）の結果は出すが、合否に数えない。
fn judge_kind(rows: &[Row], kind: Kind, want: bool, out: &mut Vec<Finding>) -> usize {
    let from = out.len();
    let n = judge_episodes(rows, kind, want, out);
    if !want {
        for f in &mut out[from..] {
            f.counts = false;
        }
    }
    n
}

fn judge_episodes(rows: &[Row], kind: Kind, want: bool, out: &mut Vec<Finding>) -> usize {
    let eps = episodes(rows, kind);
    note(
        out,
        kind,
        "episodes",
        Status::Info,
        format!("{}の旗が立った区間 {} 個", kind.name(), eps.len()),
    );
    if eps.is_empty() {
        if want {
            note(
                out,
                kind,
                "coverage",
                Status::NotMeasured,
                "旗が立った区間が 1 つも無い（台本が開かなかった、か読めていない）".to_string(),
            );
        }
        return 0;
    }
    check_readable(rows, &eps, kind, out);
    let seen = check_moves(rows, &eps, kind, out);
    check_ticks(rows, &eps, kind, out);
    if kind == Kind::Pause {
        check_close(rows, &eps, out);
    }
    if want {
        let missing: Vec<String> = kind
            .required()
            .iter()
            .filter(|t| !seen.contains(t))
            .map(|(a, b)| format!("{a}→{b}"))
            .collect();
        if missing.is_empty() {
            note(
                out,
                kind,
                "coverage",
                Status::Pass,
                format!("求めた移り {} 種類が全部見えた", kind.required().len()),
            );
        } else {
            note(
                out,
                kind,
                "coverage",
                Status::Fail,
                format!("見えなかった移り: {}", missing.join(" / ")),
            );
        }
    }
    eps.len()
}

/// 1 回目の一時停止の区間の移りの列が `PAUSE_FIRST_MOVES` と一致するか。閉じる呼びの途中で読んだ 3→0 の行は列の終わりから外す。
fn check_first_moves(rows: &[Row], out: &mut Vec<Finding>) {
    let show = |v: &[(u32, u32)]| {
        v.iter()
            .map(|(a, b)| format!("{a}→{b}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let eps = episodes(rows, Kind::Pause);
    let Some(ep) = eps.first() else {
        note(
            out,
            Kind::Pause,
            "first_moves",
            Status::NotMeasured,
            "旗が立った区間が 1 つも無い（1 回目の一時停止を読めていない）".to_string(),
        );
        return;
    };
    let mut seq: Vec<(u32, u32)> = moves_of(rows, ep, Kind::Pause)
        .iter()
        .map(|m| (m.from, m.to))
        .collect();
    if seq.last() == Some(&(pause::RESUMING, pause::INIT)) {
        seq.pop();
    }
    let (status, detail) = if seq == PAUSE_FIRST_MOVES {
        (
            Status::Pass,
            format!(
                "1 回目の区間の移りが {}（開いた直後の下は効かなかった）",
                show(&seq)
            ),
        )
    } else {
        (
            Status::Fail,
            format!(
                "1 回目の区間の移りが {}（求めた: {}。開いた直後の下が効いた、か台本の押しが届いていない）",
                show(&seq),
                show(&PAUSE_FIRST_MOVES)
            ),
        )
    };
    note(out, Kind::Pause, "first_moves", status, detail);
}

/// `_menu.csv` の行を照らす。`want` で求めたものだけが合否に効く。
pub fn judge(rows: &[Row], want: &Expect) -> Report {
    let mut findings = Vec::new();
    let p = judge_kind(rows, Kind::Pause, want.pause, &mut findings);
    let e = judge_kind(rows, Kind::End, want.end, &mut findings);
    if want.pause_first {
        check_first_moves(rows, &mut findings);
    }
    if let Some((screen, cursor)) = want.final_screen {
        let (status, detail) = match rows.last() {
            None => (Status::NotMeasured, "行が 1 つも無い".to_string()),
            Some(r) => {
                let ok = r.screen_id == i32::try_from(screen).ok()
                    && r.cursor == Some(cursor)
                    && r.substate == Some(1);
                (
                    if ok { Status::Pass } else { Status::Fail },
                    format!(
                        "最後の行（seq {}）の画面 {:?}・カーソル {:?}・サブ状態 {:?}（求めた: 画面 {screen}・カーソル {cursor}・サブ状態 1）",
                        r.seq_after, r.screen_id, r.cursor, r.substate
                    ),
                )
            }
        };
        findings.push(Finding {
            check: "final_screen".to_string(),
            status,
            detail,
            counts: true,
        });
    }
    Report {
        findings,
        episodes: (p, e),
    }
}

#[cfg(test)]
mod tests;
