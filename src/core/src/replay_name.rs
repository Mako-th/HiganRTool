//! 自動保存のリプレイのファイル名のテンプレート（`HiganRTool.ini`の`[replay] name`）。
//! 解析・展開・使える文字・長さ・重複のときの名前を決める（pure。ファイルには触らない）。
//! キャラ・ラウンド数・勝者は書き終えたリプレイのバイト列から読む（`read_file_facts`）。

#![forbid(unsafe_code)]

use crate::replay::{MarkRead, MarkRole, MatchResult, Side, Timestamp, read_mark};
use crate::replay_decode::decode_inputs;
use crate::replay_detail::{RoundRec, SyncKind, Winner};
use crate::vault::is_reserved_stem;

/// 既定のテンプレート。予約の名前（`th9_udNNNN`）と同じになるので名前は変えない。
pub const DEFAULT_TEMPLATE: &str = "th9_ud{num}";
/// 展開後の名前（`.rpy`を除く）の文字数の上限。
pub const NAME_LIMIT: usize = 100;
/// 重複のときに`_2`〜`_N`を試す上限。
pub const DUP_LIMIT: u32 = 999;
const EXT: &str = ".rpy";
/// 枠9はMatch（`replay_decode`）。
const MATCH_SLOT: u8 = 9;

/// キャラID（0〜15。リプレイの面の構造体`+0x06`）の英語名。番号の並びは`TH09_Technical_Reference.md` §14.2のCharacter ID。
const CHARACTERS: [&str; 16] = [
    "Reimu", "Marisa", "Sakuya", "Youmu", "Reisen", "Cirno", "Lyrica", "Mystia", "Tewi", "Yuuka",
    "Aya", "Medicine", "Komachi", "Eiki", "Merlin", "Lunasa",
];

pub fn character_name(id: u8) -> Option<&'static str> {
    CHARACTERS.get(usize::from(id)).copied()
}

/// 名前に使える文字: 半角英数字と`- _ . ( ) [ ] +`と半角スペース。
pub fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')' | '[' | ']' | '+' | ' ')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    UnknownPlaceholder(String),
    Unclosed,
    BadChar(char),
    Empty,
    TooLong(usize),
    Reserved,
    /// 展開に要る値をリプレイから読み取れない（置き換えの名前）。名前は変えない。
    Unreadable(&'static str),
}

impl NameError {
    /// ログに書く短い名詞句。
    pub fn reason(&self) -> String {
        match self {
            NameError::UnknownPlaceholder(s) => {
                let shown: String = s
                    .chars()
                    .take(16)
                    .map(|c| if c.is_control() { '?' } else { c })
                    .collect();
                format!("不明な置き換え {{{shown}}}")
            }
            NameError::Unclosed => "閉じていない {".to_string(),
            NameError::BadChar(c) if c.is_control() => {
                format!("使用不可の文字 U+{:04X}", u32::from(*c))
            }
            NameError::BadChar(c) => format!("使用不可の文字 {c}"),
            NameError::Empty => "名前が空".to_string(),
            NameError::TooLong(n) => format!("{n} 文字の名前（上限 {NAME_LIMIT}）"),
            NameError::Reserved => "Windows の予約名".to_string(),
            NameError::Unreadable(what) => format!("{{{what}}} をリプレイから読めない"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placeholder {
    Num,
    Date,
    Time,
    Chara(Side),
    Name(Side),
    Round,
    Winner,
    Wins,
    Length,
    RLengths,
    RLifes,
    Sync,
    Delay,
    Ping,
    SeatSide,
    Result,
}

impl Placeholder {
    fn parse(inner: &str) -> Option<Placeholder> {
        [
            ("num", Placeholder::Num),
            ("date", Placeholder::Date),
            ("1pchara", Placeholder::Chara(Side::P1)),
            ("2pchara", Placeholder::Chara(Side::P2)),
            ("1pname", Placeholder::Name(Side::P1)),
            ("2pname", Placeholder::Name(Side::P2)),
            ("round", Placeholder::Round),
            ("winner", Placeholder::Winner),
            ("wins", Placeholder::Wins),
            ("time", Placeholder::Time),
            ("length", Placeholder::Length),
            ("rlengths", Placeholder::RLengths),
            ("rlifes", Placeholder::RLifes),
            ("sync", Placeholder::Sync),
            ("delay", Placeholder::Delay),
            ("ping", Placeholder::Ping),
            ("side", Placeholder::SeatSide),
            ("result", Placeholder::Result),
        ]
        .into_iter()
        .find(|(n, _)| inner.eq_ignore_ascii_case(n))
        .map(|(_, p)| p)
    }

    fn name(self) -> &'static str {
        match self {
            Placeholder::Num => "num",
            Placeholder::Date => "date",
            Placeholder::Chara(Side::P1) => "1Pchara",
            Placeholder::Chara(Side::P2) => "2Pchara",
            Placeholder::Name(Side::P1) => "1Pname",
            Placeholder::Name(Side::P2) => "2Pname",
            Placeholder::Round => "round",
            Placeholder::Winner => "winner",
            Placeholder::Wins => "wins",
            Placeholder::Time => "time",
            Placeholder::Length => "length",
            Placeholder::RLengths => "rlengths",
            Placeholder::RLifes => "rlifes",
            Placeholder::Sync => "sync",
            Placeholder::Delay => "delay",
            Placeholder::Ping => "ping",
            Placeholder::SeatSide => "side",
            Placeholder::Result => "result",
        }
    }

    /// 書き終えたリプレイを読まないと決まらないもの。
    fn needs_file(self) -> bool {
        !matches!(
            self,
            Placeholder::Num | Placeholder::Date | Placeholder::Time
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Lit(String),
    Hole(Placeholder),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pieces: Vec<Piece>,
}

/// マッチの勝者（決まらなかったマッチは`Undecided`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchWinner {
    P1,
    P2,
    Undecided,
}

/// 書き終えたリプレイから読み取れたもの。読み取れなかった項目は`None`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileFacts {
    /// 1P・2PのキャラID。
    pub chars: Option<[u8; 2]>,
    pub rounds: Option<u32>,
    pub winner: Option<MatchWinner>,
    /// 印（HGRT）の値。印が読み取れなければ`None`。
    pub mark: Option<MarkFacts>,
}

/// 印から取る値。サイドごとの値は1P・2Pの順に並べ直してある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkFacts {
    /// 1P・2Pの名前（観戦の印は1P・2Pの名前、対戦の印は自分と相手をサイドで並べたもの）。
    pub names: [String; 2],
    /// ラウンドごとの記録（先頭から）。
    pub rounds: Vec<RoundRec>,
    pub sync: Option<SyncKind>,
    pub lat: u8,
    pub ping_ms: u32,
    /// 通信の要約のpingの中央値。
    pub ping_median_ms: Option<u32>,
    /// 自分のサイド。観戦の印は`None`。
    pub seat: Option<Side>,
    /// 印の結果（対戦の印は自分から、観戦の印は1Pから見たもの）。
    pub result: MatchResult,
}

/// 展開の材料。`file`はリプレイを読んだ結果（読んでいなければ`None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub num: u16,
    /// マッチが終わった時刻（手元）。
    pub at: Timestamp,
    pub file: Option<FileFacts>,
}

fn strip_ext(s: &str) -> &str {
    let n = s.len();
    if n >= EXT.len()
        && s.is_char_boundary(n - EXT.len())
        && s[n - EXT.len()..].eq_ignore_ascii_case(EXT)
    {
        &s[..n - EXT.len()]
    } else {
        s
    }
}

impl Template {
    /// `raw`はiniの値（前後の空白はiniの読み手が除去済み）。末尾の`.rpy`は付けても付けなくても同じ。
    pub fn parse(raw: &str) -> Result<Template, NameError> {
        let src = strip_ext(raw);
        if src.is_empty() {
            return Err(NameError::Empty);
        }
        let mut pieces = Vec::new();
        let mut lit = String::new();
        let mut rest = src;
        while let Some(c) = rest.chars().next() {
            if c == '{' {
                let after = &rest[1..];
                let end = after
                    .find(['{', '}'])
                    .filter(|&i| after.as_bytes()[i] == b'}')
                    .ok_or(NameError::Unclosed)?;
                let inner = &after[..end];
                let hole = Placeholder::parse(inner)
                    .ok_or_else(|| NameError::UnknownPlaceholder(inner.to_string()))?;
                if !lit.is_empty() {
                    pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                pieces.push(Piece::Hole(hole));
                rest = &after[end + 1..];
                continue;
            }
            if !is_name_char(c) {
                return Err(NameError::BadChar(c));
            }
            lit.push(c);
            rest = &rest[c.len_utf8()..];
        }
        if !lit.is_empty() {
            pieces.push(Piece::Lit(lit));
        }
        Ok(Template { pieces })
    }

    fn holes(&self) -> impl Iterator<Item = Placeholder> + '_ {
        self.pieces.iter().filter_map(|p| match p {
            Piece::Hole(h) => Some(*h),
            Piece::Lit(_) => None,
        })
    }

    /// `{num}`を含むか。
    pub fn has_num(&self) -> bool {
        self.holes().any(|h| h == Placeholder::Num)
    }

    /// 展開にリプレイの中身（キャラ・ラウンド数・勝者）が要るかどうか。要らなければ読まない。
    pub fn needs_file(&self) -> bool {
        self.holes().any(Placeholder::needs_file)
    }

    /// 展開した名前（`.rpy`を除く）。長さと予約名もここで見る。
    pub fn expand(&self, f: &Facts) -> Result<String, NameError> {
        let mut out = String::new();
        for p in &self.pieces {
            match p {
                Piece::Lit(s) => out.push_str(s),
                Piece::Hole(h) => out.push_str(&hole_text(*h, f)?),
            }
        }
        let n = out.chars().count();
        if n > NAME_LIMIT {
            return Err(NameError::TooLong(n));
        }
        let stem_end = out.find('.').unwrap_or(out.len());
        if is_reserved_stem(out[..stem_end].trim_end_matches(' ')) {
            return Err(NameError::Reserved);
        }
        Ok(out)
    }

    /// 既定の`th9_ud{num}`（大文字小文字違いも）かどうか。予約の名前と同じになるので付け替えない。
    pub fn is_default(&self) -> bool {
        matches!(
            self.pieces.as_slice(),
            [Piece::Lit(s), Piece::Hole(Placeholder::Num)] if s.eq_ignore_ascii_case("th9_ud")
        )
    }

    /// 既存のファイル名`file_name`がこのテンプレートで作られた名前ならその`{num}`。
    /// `{num}`は4桁の数字。ほかの置き換えは`expand`が作る形（`shape_lens`。名前だけは使える文字の1文字以上）に当てる。
    /// 文字の部分は大文字小文字を区別せずに当てる。
    /// `{num}`が2回あれば同じ番号でなければ外れ。重複で付く`_2`などの末尾も許す。
    /// 当たり方が複数あって番号が割れるとき、または試す回数が上限を越えたときは`None`（別の番号に読み違えるより数えない）。
    pub fn number_in(&self, file_name: &str) -> Option<u16> {
        if !self.has_num() {
            return None;
        }
        let stem = strip_ext(file_name);
        if stem.len() == file_name.len() {
            return None;
        }
        let mut m = Matcher::new(&self.pieces);
        match m.run(stem) {
            Run::Found(n) => return Some(n),
            Run::Failed => return None,
            Run::NoMatch => {}
        }
        let (head, tail) = stem.rsplit_once('_')?;
        if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match m.run(head) {
            Run::Found(n) => Some(n),
            Run::Failed | Run::NoMatch => None,
        }
    }

    /// 起動の直後にフォルダの既存の名前から次の予約の番号の下限（当たった最大の `{num}` + 1）を求める。
    /// `{num}`を含まないテンプレートや当たる名前が無いときは`None`。
    pub fn next_floor<S: AsRef<str>>(&self, names: &[S]) -> Option<u16> {
        names
            .iter()
            .filter_map(|n| self.number_in(n.as_ref()))
            .max()
            .map(|m| m.saturating_add(1))
    }
}

/// 逆引き1回の上限（置き換えの長さを試す回数）。置き換えが多いテンプレートで指数的に増えないよう上限を越えたら外れとする。
const MATCH_BUDGET: u32 = 20_000;

enum Run {
    Found(u16),
    NoMatch,
    /// 番号が割れた、または試す回数が上限を越えた。
    Failed,
}

struct Matcher<'a> {
    pieces: &'a [Piece],
    budget: u32,
    found: Option<u16>,
    failed: bool,
}

impl<'a> Matcher<'a> {
    fn new(pieces: &'a [Piece]) -> Self {
        Matcher {
            pieces,
            budget: MATCH_BUDGET,
            found: None,
            failed: false,
        }
    }

    /// 当たり方を全部たどり、番号が1つに決まるかを見る。
    fn run(&mut self, s: &str) -> Run {
        self.budget = MATCH_BUDGET;
        self.found = None;
        self.failed = false;
        self.go(0, s, None);
        match (self.failed, self.found) {
            (true, _) => Run::Failed,
            (false, Some(n)) => Run::Found(n),
            (false, None) => Run::NoMatch,
        }
    }

    fn go(&mut self, pi: usize, s: &str, num: Option<u16>) {
        let Some(piece) = self.pieces.get(pi) else {
            if let (true, Some(n)) = (s.is_empty(), num) {
                match self.found {
                    Some(f) if f != n => self.failed = true,
                    _ => self.found = Some(n),
                }
            }
            return;
        };
        match piece {
            Piece::Lit(l) => {
                let n = l.len();
                if s.len() < n || !s.is_char_boundary(n) || !s[..n].eq_ignore_ascii_case(l) {
                    return;
                }
                self.go(pi + 1, &s[n..], num);
            }
            Piece::Hole(Placeholder::Num) => {
                let Some(d) = s.as_bytes().get(..4) else {
                    return;
                };
                if !d.iter().all(u8::is_ascii_digit) {
                    return;
                }
                let Ok(n) = s[..4].parse::<u16>() else {
                    return;
                };
                if num.is_some_and(|x| x != n) {
                    return;
                }
                self.go(pi + 1, &s[4..], Some(n));
            }
            Piece::Hole(h) => {
                for len in shape_lens(*h, s) {
                    if self.failed {
                        return;
                    }
                    if self.budget == 0 {
                        self.failed = true;
                        return;
                    }
                    self.budget -= 1;
                    self.go(pi + 1, &s[len..], num);
                }
            }
        }
    }
}

fn digit_run(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

fn starts_with_ci(s: &str, p: &str) -> bool {
    s.len() >= p.len() && s.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
}

fn word_lens(s: &str, words: &[&str]) -> Vec<usize> {
    words
        .iter()
        .filter(|w| starts_with_ci(s, w))
        .map(|w| w.len())
        .collect()
}

/// `{length}`の1区間（`3m42s`）の長さ。
fn clock_lens(s: &str) -> Vec<usize> {
    let d = digit_run(s);
    let b = s.as_bytes();
    let ok = d > 0
        && b.get(d).is_some_and(|c| c.eq_ignore_ascii_case(&b'm'))
        && b.get(d + 1).is_some_and(u8::is_ascii_digit)
        && b.get(d + 2).is_some_and(u8::is_ascii_digit)
        && b.get(d + 3).is_some_and(|c| c.eq_ignore_ascii_case(&b's'));
    if ok { vec![d + 4] } else { Vec::new() }
}

/// ちょうど`n`桁の数字（`{date}`・`{time}`）。
fn digits_lens(s: &str, n: usize) -> Vec<usize> {
    if s.as_bytes()
        .get(..n)
        .is_some_and(|d| d.iter().all(u8::is_ascii_digit))
    {
        vec![n]
    } else {
        Vec::new()
    }
}

fn count_lens(s: &str) -> Vec<usize> {
    (1..=digit_run(s)).collect()
}

/// ライフの値（数字か、読めなかったときの`x`）。
fn life_lens(s: &str) -> Vec<usize> {
    if starts_with_ci(s, "x") {
        vec![1]
    } else {
        count_lens(s)
    }
}

/// `<側>vs<側>`の長さ（`{wins}`・`{rlifes}`の1区間）。
fn versus_lens(s: &str, side: fn(&str) -> Vec<usize>) -> Vec<usize> {
    let mut out = Vec::new();
    for a in side(s) {
        if starts_with_ci(&s[a..], "vs") {
            out.extend(side(&s[a + 2..]).into_iter().map(|b| a + 2 + b));
        }
    }
    out
}

fn wins_lens(s: &str) -> Vec<usize> {
    versus_lens(s, count_lens)
}

fn lifes_lens(s: &str) -> Vec<usize> {
    versus_lens(s, life_lens)
}

/// `-`でつないだ区間の並び（`{rlengths}`・`{rlifes}`）の、1区間以上の長さ。
fn joined_lens(s: &str, pos: usize, item: fn(&str) -> Vec<usize>, out: &mut Vec<usize>) {
    for l in item(&s[pos..]) {
        let end = pos + l;
        out.push(end);
        if s.as_bytes().get(end) == Some(&b'-') {
            joined_lens(s, end + 1, item, out);
        }
    }
}

/// 置き換え`h`が`s`の先頭に当たる長さ（バイト）の全部。形は`hole_text`が作る値と合わせる。
/// 名前だけは使える文字の1文字以上なら何でも当たる。
fn shape_lens(h: Placeholder, s: &str) -> Vec<usize> {
    match h {
        Placeholder::Num => Vec::new(),
        Placeholder::Date => digits_lens(s, 8),
        Placeholder::Time => digits_lens(s, 4),
        Placeholder::Chara(_) => word_lens(s, &CHARACTERS),
        Placeholder::Name(_) => s
            .char_indices()
            .take_while(|&(_, c)| is_name_char(c))
            .map(|(i, c)| i + c.len_utf8())
            .collect(),
        Placeholder::Round => count_lens(s),
        Placeholder::Winner | Placeholder::SeatSide => word_lens(s, &["1P", "2P", "none"]),
        Placeholder::Wins => wins_lens(s),
        Placeholder::Length => clock_lens(s),
        Placeholder::RLengths => {
            let mut out = Vec::new();
            joined_lens(s, 0, clock_lens, &mut out);
            out
        }
        Placeholder::RLifes => {
            let mut out = Vec::new();
            joined_lens(s, 0, lifes_lens, &mut out);
            out
        }
        Placeholder::Sync => word_lens(s, &["rollback", "delay", "none"]),
        Placeholder::Delay => {
            let d = digit_run(s);
            if d > 0 && starts_with_ci(&s[d..], "F") {
                vec![d + 1]
            } else {
                Vec::new()
            }
        }
        Placeholder::Ping => {
            let d = digit_run(s);
            if d > 0 && starts_with_ci(&s[d..], "ms") {
                vec![d + 2]
            } else {
                Vec::new()
            }
        }
        Placeholder::Result => word_lens(s, &["win", "lose", "draw", "none"]),
    }
}

/// 経過時間の書式: 分は0で埋めず、秒は2桁（`3m42s`、`0m45s`）。60フレーム = 1秒で切り捨て。
fn clock(frames: u64) -> String {
    let secs = frames / 60;
    format!("{}m{:02}s", secs / 60, secs % 60)
}

/// プレイヤー名を名前に使える文字だけにする。使えない文字は`-`、空なら`-`。
fn clean_player_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if is_name_char(c) { c } else { '-' })
        .collect();
    if s.is_empty() { "-".to_string() } else { s }
}

fn life_text(v: Option<u32>) -> String {
    v.map_or_else(|| "x".to_string(), |x| x.to_string())
}

fn hole_text(h: Placeholder, f: &Facts) -> Result<String, NameError> {
    let unreadable = NameError::Unreadable(h.name());
    let file = || f.file.as_ref().ok_or(unreadable.clone());
    let mark = || file()?.mark.as_ref().ok_or(unreadable.clone());
    // ラウンドの記録が要る置き換え。1件も無ければ読み取れなかったものとする。
    let rounds = || {
        let m = mark()?;
        if m.rounds.is_empty() {
            Err(unreadable.clone())
        } else {
            Ok(&m.rounds)
        }
    };
    Ok(match h {
        Placeholder::Num => format!("{:04}", f.num),
        Placeholder::Date => format!("{:04}{:02}{:02}", f.at.year, f.at.month, f.at.day),
        Placeholder::Time => format!("{:02}{:02}", f.at.hour, f.at.minute),
        Placeholder::Chara(side) => {
            let ids = file()?.chars.ok_or(unreadable.clone())?;
            let id = ids[usize::from(side == Side::P2)];
            character_name(id).ok_or(unreadable)?.to_string()
        }
        Placeholder::Name(side) => clean_player_name(&mark()?.names[usize::from(side == Side::P2)]),
        Placeholder::Round => file()?.rounds.ok_or(unreadable)?.to_string(),
        Placeholder::Winner => match file()?.winner.ok_or(unreadable)? {
            MatchWinner::P1 => "1P",
            MatchWinner::P2 => "2P",
            MatchWinner::Undecided => "none",
        }
        .to_string(),
        Placeholder::Wins => {
            let rs = rounds()?;
            let won = |w: Winner| rs.iter().filter(|r| r.winner == w).count();
            format!("{}vs{}", won(Winner::P1), won(Winner::P2))
        }
        Placeholder::Length => clock(rounds()?.iter().map(|r| u64::from(r.frames)).sum()),
        Placeholder::RLengths => rounds()?
            .iter()
            .map(|r| clock(u64::from(r.frames)))
            .collect::<Vec<_>>()
            .join("-"),
        Placeholder::RLifes => rounds()?
            .iter()
            .map(|r| format!("{}vs{}", life_text(r.life[0]), life_text(r.life[1])))
            .collect::<Vec<_>>()
            .join("-"),
        Placeholder::Sync => match mark()?.sync {
            Some(SyncKind::Rollback) => "rollback",
            Some(SyncKind::Delay) => "delay",
            None => "none",
        }
        .to_string(),
        Placeholder::Delay => format!("{}F", mark()?.lat),
        Placeholder::Ping => {
            let m = mark()?;
            format!("{}ms", m.ping_median_ms.unwrap_or(m.ping_ms))
        }
        Placeholder::SeatSide => match mark()?.seat {
            Some(Side::P1) => "1P",
            Some(Side::P2) => "2P",
            None => "none",
        }
        .to_string(),
        Placeholder::Result => {
            let m = mark()?;
            match (m.seat, m.result) {
                (None, _) | (_, MatchResult::Abort) => "none",
                (Some(_), MatchResult::Win) => "win",
                (Some(_), MatchResult::Lose) => "lose",
                (Some(_), MatchResult::Draw) => "draw",
            }
            .to_string()
        }
    })
}

/// `base`の`k`番目の候補のファイル名。1番目は`<base>.rpy`、2番目からは`<base>_<k>.rpy`。
pub fn candidate_file(base: &str, k: u32) -> String {
    if k <= 1 {
        format!("{base}{EXT}")
    } else {
        format!("{base}_{k}{EXT}")
    }
}

/// 印の結果とサイドから求めるマッチの勝者（引き分け・途中終了は決まらない）。
fn winner_of(result: MatchResult, side: Side) -> MatchWinner {
    let mine = match side {
        Side::P1 => MatchWinner::P1,
        Side::P2 => MatchWinner::P2,
    };
    let theirs = match side {
        Side::P1 => MatchWinner::P2,
        Side::P2 => MatchWinner::P1,
    };
    match result {
        MatchResult::Win => mine,
        MatchResult::Lose => theirs,
        MatchResult::Draw | MatchResult::Abort => MatchWinner::Undecided,
    }
}

/// 書き終えたリプレイ（HGRTの印を足した後）から展開に要る値を読む。キャラは枠9の面の構造体から読み、ほかは印から読む。
/// ラウンドの記録が1件も無い印は数えられなかったものとして`rounds`を`None`にする。
pub fn read_file_facts(file: &[u8]) -> FileFacts {
    let chars = decode_inputs(file).ok().and_then(|r| {
        let st = r
            .stages
            .iter()
            .find(|s| s.slot == MATCH_SLOT)
            .or(r.stages.last())?;
        Some([st.p1.character, st.p2.character])
    });
    let Ok(MarkRead::Found(m)) = read_mark(file) else {
        return FileFacts {
            chars,
            ..FileFacts::default()
        };
    };
    let rounds = m
        .detail
        .as_ref()
        .map(|d| d.rounds.len() as u32 + d.rounds_dropped)
        .filter(|&n| n > 0);
    let (names, seat) = match &m.role {
        MarkRole::Watch { p1, p2 } => ([p1.clone(), p2.clone()], None),
        MarkRole::Play => (
            match m.side {
                Side::P1 => [m.self_name.clone(), m.peer.clone()],
                Side::P2 => [m.peer.clone(), m.self_name.clone()],
            },
            Some(m.side),
        ),
    };
    let detail = m.detail.as_ref();
    FileFacts {
        chars,
        rounds,
        winner: Some(winner_of(m.result, m.side)),
        mark: Some(MarkFacts {
            names,
            rounds: detail.map(|d| d.rounds.clone()).unwrap_or_default(),
            sync: detail.and_then(|d| d.sync),
            lat: m.lat,
            ping_ms: m.ping_ms,
            ping_median_ms: detail
                .and_then(|d| d.comm.as_ref())
                .and_then(|c| c.ping_median_ms),
            seat,
            result: m.result,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{Mark, MarkRole, chunk_to_append};
    use crate::replay_build::{BuildSpec, build_replay};
    use crate::replay_detail::{MatchDetail, RoundRec, Winner};

    fn at() -> Timestamp {
        Timestamp {
            year: 2026,
            month: 10,
            day: 28,
            hour: 23,
            minute: 13,
            second: 59,
        }
    }

    fn facts(file: Option<FileFacts>) -> Facts {
        Facts {
            num: 7,
            at: at(),
            file,
        }
    }

    fn full() -> FileFacts {
        FileFacts {
            chars: Some([0, 15]),
            rounds: Some(3),
            winner: Some(MatchWinner::P2),
            mark: None,
        }
    }

    fn expand(t: &str, f: &Facts) -> Result<String, NameError> {
        Template::parse(t)?.expand(f)
    }

    #[test]
    fn the_default_template_gives_the_reserved_name() {
        assert_eq!(
            expand(DEFAULT_TEMPLATE, &facts(None)).as_deref(),
            Ok("th9_ud0007")
        );
        assert_eq!(candidate_file("th9_ud0007", 1), "th9_ud0007.rpy");
    }

    #[test]
    fn every_placeholder_expands() {
        let f = facts(Some(full()));
        assert_eq!(
            expand(
                "{date}-{time}_{1Pchara}VS{2Pchara}_{round}R_{winner}win",
                &f
            )
            .as_deref(),
            Ok("20261028-2313_ReimuVSLunasa_3R_2Pwin")
        );
        assert_eq!(expand("{num}", &f).as_deref(), Ok("0007"));
    }

    #[test]
    fn placeholder_names_ignore_case_but_literals_keep_it() {
        let f = facts(Some(full()));
        assert_eq!(
            expand("Ab{NUM}{1pCHARA}{Winner}", &f).as_deref(),
            Ok("Ab0007Reimu2P")
        );
    }

    #[test]
    fn a_trailing_rpy_is_not_doubled() {
        let f = facts(None);
        for t in ["a{num}.rpy", "a{num}.RPY", "a{num}"] {
            assert_eq!(expand(t, &f).as_deref(), Ok("a0007"), "{t}");
        }
        assert_eq!(expand("a.rpy.rpy", &f).as_deref(), Ok("a.rpy"));
        assert_eq!(expand(".rpy", &f), Err(NameError::Empty));
    }

    #[test]
    fn all_sixteen_characters_have_their_english_names() {
        let want = [
            "Reimu", "Marisa", "Sakuya", "Youmu", "Reisen", "Cirno", "Lyrica", "Mystia", "Tewi",
            "Yuuka", "Aya", "Medicine", "Komachi", "Eiki", "Merlin", "Lunasa",
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(character_name(i as u8), Some(*w));
        }
        assert_eq!(character_name(16), None);
    }

    #[test]
    fn a_syntax_error_names_its_reason() {
        let cases: [(&str, &str); 6] = [
            ("a{foo}", "不明な置き換え {foo}"),
            ("a{}", "不明な置き換え {}"),
            ("a{num", "閉じていない {"),
            ("a{n{num}", "閉じていない {"),
            ("a:b", "使用不可の文字 :"),
            ("a}b", "使用不可の文字 }"),
        ];
        for (t, why) in cases {
            assert_eq!(Template::parse(t).unwrap_err().reason(), why, "{t}");
        }
        assert_eq!(
            Template::parse("日本語").unwrap_err(),
            NameError::BadChar('日')
        );
        assert_eq!(
            Template::parse("a\tb").unwrap_err().reason(),
            "使用不可の文字 U+0009"
        );
    }

    #[test]
    fn every_allowed_literal_character_is_accepted() {
        let f = facts(None);
        let all = "azAZ09-_.()[]+ x";
        assert_eq!(expand(all, &f).as_deref(), Ok(all));
        for c in [
            '\\', '/', ':', '*', '?', '"', '<', '>', '|', ',', ';', '=', '&', '%',
        ] {
            assert_eq!(
                Template::parse(&format!("a{c}")).unwrap_err(),
                NameError::BadChar(c)
            );
        }
    }

    #[test]
    fn a_value_that_cannot_be_read_is_reported_by_its_placeholder() {
        let f = facts(None);
        assert_eq!(expand("{round}", &f), Err(NameError::Unreadable("round")));
        let part = FileFacts {
            chars: None,
            ..full()
        };
        assert_eq!(
            expand("{2Pchara}", &facts(Some(part))),
            Err(NameError::Unreadable("2Pchara"))
        );
        let bad_id = FileFacts {
            chars: Some([0, 16]),
            ..full()
        };
        assert!(expand("{1Pchara}", &facts(Some(bad_id.clone()))).is_ok());
        assert_eq!(
            expand("{2Pchara}", &facts(Some(bad_id))),
            Err(NameError::Unreadable("2Pchara"))
        );
        assert!(!Template::parse("{num}{date}{time}").unwrap().needs_file());
        assert!(Template::parse("{num}{winner}").unwrap().needs_file());
    }

    #[test]
    fn an_undecided_match_is_none() {
        let f = FileFacts {
            winner: Some(MatchWinner::Undecided),
            ..full()
        };
        assert_eq!(expand("{winner}", &facts(Some(f))).as_deref(), Ok("none"));
    }

    #[test]
    fn the_expanded_name_is_at_most_100_characters() {
        let f = facts(None);
        let hundred = "a".repeat(100);
        assert_eq!(expand(&hundred, &f).as_deref(), Ok(hundred.as_str()));
        let long = "a".repeat(97) + "{num}";
        assert_eq!(expand(&long, &f), Err(NameError::TooLong(101)));
        assert_eq!(
            NameError::TooLong(101).reason(),
            "101 文字の名前（上限 100）"
        );
    }

    #[test]
    fn windows_reserved_names_are_refused() {
        let f = facts(None);
        for t in ["con", "NUL", "com1", "Lpt9.x", "aux "] {
            assert_eq!(expand(t, &f), Err(NameError::Reserved), "{t}");
        }
        assert!(expand("con1", &f).is_ok());
    }

    fn number_in(t: &str, name: &str) -> Option<u16> {
        Template::parse(t).unwrap().number_in(name)
    }

    #[test]
    fn a_name_made_by_the_template_gives_back_its_number() {
        let t = "{num}_{date}-{time}_{1Pchara}VS{2Pchara}-{round}R_{winner}win";
        assert_eq!(
            number_in(t, "0042_20261028-2313_ReimuVSLunasa-3R_2Pwin.rpy"),
            Some(42)
        );
        assert_eq!(
            number_in("a{num}b", "A0007B.RPY"),
            Some(7),
            "大文字小文字は区別しない"
        );
        assert_eq!(number_in("{num}", "0001_2.rpy"), Some(1), "重複の末尾 _2");
        assert_eq!(number_in("a-{num}", "a-0003_17.rpy"), Some(3));
    }

    #[test]
    fn a_name_not_made_by_the_template_gives_nothing() {
        for (t, name) in [
            ("a{num}b", "a007b.rpy"),
            ("a{num}b", "a00007b.rpy"),
            ("a{num}b", "a000xb.rpy"),
            ("a{num}b", "a0007c.rpy"),
            ("a{num}b", "a0007b.txt"),
            ("a{num}b", "a0007b"),
            ("a{num}-{round}R", "a0007-R.rpy"),
            ("a{num}", "xa0007.rpy"),
            ("a{num}", "a0007x.rpy"),
            ("a{num}", "a0007_x.rpy"),
            ("a{num}", "a0007_.rpy"),
        ] {
            assert_eq!(number_in(t, name), None, "{t} / {name}");
        }
        assert_eq!(number_in("fixed", "fixed.rpy"), None, "{{num}} が無い");
    }

    #[test]
    fn a_template_with_num_twice_needs_the_same_number_both_times() {
        assert_eq!(number_in("{num}-{num}", "0005-0005.rpy"), Some(5));
        assert_eq!(number_in("{num}-{num}", "0005-0006.rpy"), None);
    }

    #[test]
    fn the_floor_is_one_past_the_largest_number_found() {
        let t = Template::parse("{num}-a").unwrap();
        let names = [
            "0003-a.rpy",
            "0009-a_2.rpy",
            "0004-b.rpy",
            "readme.txt",
            "th9_ud0100.rpy",
        ];
        assert_eq!(t.next_floor(&names), Some(10));
        assert_eq!(t.next_floor(&["0004-b.rpy"]), None);
        assert_eq!(t.next_floor::<&str>(&[]), None);
        assert_eq!(Template::parse("x").unwrap().next_floor(&["x.rpy"]), None);
        assert_eq!(t.next_floor(&["9999-a.rpy"]), Some(10_000));
    }

    #[test]
    fn matching_many_placeholders_stays_bounded() {
        let t = Template::parse(&"{1Pname}".repeat(15)).unwrap();
        assert_eq!(t.number_in(&format!("{}.rpy", "z".repeat(60))), None);
        // 名前の切り方が何通りもあり（どれも番号は7）、試す回数の上限を越えたら数えない。
        let t = Template::parse(&format!("{}{{num}}", "{1Pname}".repeat(15))).unwrap();
        assert_eq!(t.number_in(&format!("{}0007.rpy", "z".repeat(20))), None);
        let t = Template::parse(&format!("{}{{num}}", "{1Pname}".repeat(2))).unwrap();
        assert_eq!(t.number_in("zz0007.rpy"), Some(7), "少なければ数える");
    }

    #[test]
    fn a_split_that_gives_two_numbers_is_not_counted() {
        let t = "{1Pname}{num}{2Pname}";
        assert_eq!(number_in(t, "a00010002b.rpy"), None);
        assert_eq!(number_in(t, "a0001b.rpy"), Some(1));
        // 数字が続くテンプレート: `{time}`と`{num}`の取り方が2通りあり、番号が2と3に割れる。
        let t = "{1Pname}{time}{num}{2Pname}";
        assert_eq!(number_in(t, "a000100020003b.rpy"), None);
        assert_eq!(number_in(t, "a00010002b.rpy"), Some(2));
    }

    #[test]
    fn other_placeholders_are_read_back_by_their_own_shape() {
        for (t, name, want) in [
            ("{date}-{num}-{1Pchara}", "20261028-0005-Reimu.rpy", 5),
            ("{date}-{time}-{num}", "20261028-2313-0005.rpy", 5),
            ("{date}{time}{num}", "2026102823130005.rpy", 5),
            ("{1Pname}{num}{date}{time}", "Bob0005202610282313.rpy", 5),
            ("{1Pname}{num}{date}", "Bob000520261028.rpy", 5),
            ("{date}{num}{round}", "2026102800053.rpy", 5),
            ("{time}{num}{round}", "231300053.rpy", 5),
            ("{round}{num}", "120007.rpy", 7),
            ("{wins}{num}", "2vs100031.rpy", 31),
            ("{num}{length}", "00073m42s.rpy", 7),
            ("{num}{delay}{ping}", "00072F33ms.rpy", 7),
            ("{num}{winner}{side}{result}", "00071P2Plose.rpy", 7),
        ] {
            assert_eq!(number_in(t, name), Some(want), "{t} / {name}");
        }
        for (t, name) in [
            ("{date}-{num}", "2026102-0005.rpy"),
            ("{date}{num}", "2026-10-28-0005.rpy"),
            ("{1Pchara}{num}", "Reimux0005.rpy"),
            ("{winner}{num}", "3P0005.rpy"),
            ("{wins}{num}", "2-10005.rpy"),
            ("{length}{num}", "3m4s0005.rpy"),
            ("{time}{num}", "231x0005.rpy"),
            ("{time}{num}", "23130005x.rpy"),
            ("{delay}{num}", "2f0005x.rpy"),
            ("{rlifes}{num}", "1vs0-0005.rpy"),
        ] {
            assert_eq!(number_in(t, name), None, "{t} / {name}");
        }
    }

    #[test]
    fn only_the_exact_default_is_the_default() {
        for (t, want) in [
            ("th9_ud{num}", true),
            ("TH9_UD{NUM}.rpy", true),
            ("th9_ud{num}x", false),
            ("th9_ud{date}", false),
            ("th9_ud_{num}", false),
            ("{num}", false),
        ] {
            assert_eq!(Template::parse(t).unwrap().is_default(), want, "{t}");
        }
    }

    #[test]
    fn duplicates_get_a_numbered_suffix() {
        assert_eq!(candidate_file("x", 1), "x.rpy");
        assert_eq!(candidate_file("x", 2), "x_2.rpy");
        assert_eq!(candidate_file("x", 10), "x_10.rpy");
    }

    fn marked(
        result: MatchResult,
        side: Side,
        rounds: usize,
        dropped: u32,
        chars: [u8; 2],
    ) -> Vec<u8> {
        let mut spec = BuildSpec::match_replay(0x60, vec![0; 30], vec![0; 30]);
        spec.stages[0].p1_character = chars[0];
        spec.stages[0].p2_character = chars[1];
        let mut f = build_replay(&spec);
        let rec = RoundRec {
            frames: 100,
            winner: Winner::P1,
            life: [None; 2],
            c23: [None; 2],
            boss: [None; 2],
            rev: [None; 2],
            score: [None; 2],
        };
        let mark = Mark {
            peer: "B".to_string(),
            self_name: "A".to_string(),
            side,
            lat: 1,
            ping_ms: 1,
            result,
            at: at(),
            role: MarkRole::Play,
            detail: Some(MatchDetail {
                rounds: vec![rec; rounds],
                rounds_dropped: dropped,
                ..MatchDetail::default()
            }),
        };
        let chunk = chunk_to_append(&f, &mark).unwrap();
        f.extend_from_slice(&chunk);
        f
    }

    #[test]
    fn facts_come_from_the_finished_file() {
        let f = read_file_facts(&marked(MatchResult::Win, Side::P1, 3, 0, [4, 12]));
        assert_eq!(
            (f.chars, f.rounds, f.winner),
            (Some([4, 12]), Some(3), Some(MatchWinner::P1))
        );
        let f = read_file_facts(&marked(MatchResult::Win, Side::P2, 2, 1, [0, 1]));
        assert_eq!(f.winner, Some(MatchWinner::P2));
        assert_eq!(f.rounds, Some(3), "捨てた分も数える");
        let f = read_file_facts(&marked(MatchResult::Lose, Side::P2, 2, 0, [0, 1]));
        assert_eq!(f.winner, Some(MatchWinner::P1));
        for r in [MatchResult::Draw, MatchResult::Abort] {
            let f = read_file_facts(&marked(r, Side::P1, 2, 0, [0, 1]));
            assert_eq!(f.winner, Some(MatchWinner::Undecided));
        }
    }

    fn rec(frames: u32, winner: Winner, life: [Option<u32>; 2]) -> RoundRec {
        RoundRec {
            frames,
            winner,
            life,
            c23: [None; 2],
            boss: [None; 2],
            rev: [None; 2],
            score: [None; 2],
        }
    }

    /// 2P側で遊んだ3ラウンドの対戦の印。1PはBob、2PはAlice。1Pが2本取って勝ち（自分は2Pなのでlose）。
    fn play_mark() -> Mark {
        Mark {
            peer: "Bob".to_string(),
            self_name: "Alice".to_string(),
            side: Side::P2,
            lat: 2,
            ping_ms: 33,
            result: MatchResult::Lose,
            at: at(),
            role: MarkRole::Play,
            detail: Some(MatchDetail {
                sync: Some(SyncKind::Rollback),
                rounds: vec![
                    rec(3900, Winner::P1, [Some(10), Some(0)]),
                    rec(4920, Winner::P2, [Some(0), Some(3)]),
                    rec(4500, Winner::P1, [Some(5), Some(0)]),
                ],
                ..MatchDetail::default()
            }),
        }
    }

    /// 印から作ったリプレイを読み、テンプレートを展開する。
    fn name_for(t: &str, mark: &Mark) -> Result<String, NameError> {
        name_with_num(t, mark, 1)
    }

    fn name_with_num(t: &str, mark: &Mark, num: u16) -> Result<String, NameError> {
        let mut f = build_replay(&BuildSpec::match_replay(0x60, vec![0; 30], vec![0; 30]));
        let chunk = chunk_to_append(&f, mark).unwrap();
        f.extend_from_slice(&chunk);
        Template::parse(t)?.expand(&Facts {
            num,
            at: at(),
            file: Some(read_file_facts(&f)),
        })
    }

    /// 1つの置き換えを含むテンプレートで作った名前を`number_in`が読み戻せる。
    #[test]
    fn every_placeholder_is_read_back_from_what_expand_makes() {
        let mut sixteen = play_mark();
        sixteen.detail.as_mut().unwrap().rounds = (0..16)
            .map(|i| rec(600 * (i + 1), Winner::P2, [Some(i), Some(0)]))
            .collect();
        let mut unknown = play_mark();
        unknown.detail.as_mut().unwrap().rounds = vec![
            rec(60, Winner::Unknown, [None, Some(2)]),
            rec(216_000, Winner::P2, [Some(7), None]),
        ];
        unknown.result = MatchResult::Abort;
        unknown.detail.as_mut().unwrap().sync = None;
        let mut odd_names = play_mark();
        odd_names.self_name = "日本語".to_string();
        odd_names.peer = "A:b/c (1)_x".to_string();
        let mut watch = play_mark();
        watch.role = MarkRole::Watch {
            p1: "Xx".to_string(),
            p2: "9".to_string(),
        };
        let mut delay = play_mark();
        delay.detail.as_mut().unwrap().sync = Some(SyncKind::Delay);
        delay.result = MatchResult::Draw;
        let marks = [play_mark(), sixteen, unknown, odd_names, watch, delay];
        let names = [
            "date", "1Pchara", "2Pchara", "1Pname", "2Pname", "round", "winner", "wins", "time",
            "length", "rlengths", "rlifes", "sync", "delay", "ping", "side", "result",
        ];
        for p in names {
            let mut made = 0;
            for form in [
                "{P}-{num}",
                "{num}-{P}",
                "{P}{num}",
                "{num}{P}",
                "x{P}y{num}",
            ] {
                let t = form.replace("{P}", &format!("{{{p}}}"));
                for m in &marks {
                    let Ok(stem) = name_with_num(&t, m, 42) else {
                        continue;
                    };
                    made += 1;
                    for file in [format!("{stem}.rpy"), format!("{stem}_2.rpy")] {
                        assert_eq!(number_in(&t, &file), Some(42), "{t} / {file}");
                    }
                }
            }
            assert!(made >= 20, "{p}: 作れた名前が {made} 本しかない");
        }
    }

    #[test]
    fn each_mark_placeholder_gives_the_hand_written_text() {
        let m = play_mark();
        for (t, want) in [
            ("{1Pname}", "Bob"),
            ("{2Pname}", "Alice"),
            ("{wins}", "2vs1"),
            ("{length}", "3m42s"),
            ("{date}", "20261028"),
            ("{time}", "2313"),
            ("{rlengths}", "1m05s-1m22s-1m15s"),
            ("{rlifes}", "10vs0-0vs3-5vs0"),
            ("{sync}", "rollback"),
            ("{delay}", "2F"),
            ("{ping}", "33ms"),
            ("{side}", "2P"),
            ("{result}", "lose"),
            ("{round}", "3"),
            ("{winner}", "1P"),
        ] {
            assert_eq!(name_for(t, &m).as_deref(), Ok(want), "{t}");
        }
        assert_eq!(
            name_for("{1Pname}_{wins}_{length}_{side}{result}", &m).as_deref(),
            Ok("Bob_2vs1_3m42s_2Plose")
        );
        assert!(Template::parse("{1Pname}").unwrap().needs_file());
        assert!(Template::parse("{ping}").unwrap().needs_file());
    }

    #[test]
    fn names_follow_the_seat_and_unusable_characters_become_a_hyphen() {
        let mut m = play_mark();
        m.side = Side::P1;
        assert_eq!(
            name_for("{1Pname}+{2Pname}", &m).as_deref(),
            Ok("Alice+Bob")
        );
        m.self_name = "日本語".to_string();
        m.peer = "A:b/c (1)_x".to_string();
        assert_eq!(name_for("{1Pname}", &m).as_deref(), Ok("---"));
        assert_eq!(name_for("{2Pname}", &m).as_deref(), Ok("A-b-c (1)_x"));
        m.peer = String::new();
        assert_eq!(name_for("{2Pname}", &m).as_deref(), Ok("-"), "空は -");
    }

    #[test]
    fn a_watchers_mark_has_the_seat_names_and_no_side_or_result() {
        let mut m = play_mark();
        m.role = MarkRole::Watch {
            p1: "Xx".to_string(),
            p2: "Y y".to_string(),
        };
        m.side = Side::P1;
        m.result = MatchResult::Win;
        m.self_name = "Me".to_string();
        m.peer = String::new();
        m.lat = 0;
        m.ping_ms = 0;
        assert_eq!(
            name_for(
                "{1Pname}_{2Pname}_{side}_{result}_{winner}_{delay}_{ping}",
                &m
            )
            .as_deref(),
            Ok("Xx_Y y_none_none_1P_0F_0ms")
        );
    }

    #[test]
    fn result_is_win_lose_draw_and_none_for_an_aborted_match() {
        let mut m = play_mark();
        for (r, want) in [
            (MatchResult::Win, "win"),
            (MatchResult::Lose, "lose"),
            (MatchResult::Draw, "draw"),
            (MatchResult::Abort, "none"),
        ] {
            m.result = r;
            assert_eq!(name_for("{result}", &m).as_deref(), Ok(want));
        }
    }

    #[test]
    fn times_have_a_plain_minute_and_a_two_digit_second_rounded_down() {
        let mut m = play_mark();
        for (frames, want) in [
            (2700, "0m45s"),
            (59, "0m00s"),
            (3599, "0m59s"),
            (3600, "1m00s"),
            (3659, "1m00s"),
            (216_000, "60m00s"),
        ] {
            m.detail.as_mut().unwrap().rounds = vec![rec(frames, Winner::P1, [None; 2])];
            assert_eq!(name_for("{length}", &m).as_deref(), Ok(want), "{frames}");
            assert_eq!(name_for("{rlengths}", &m).as_deref(), Ok(want), "{frames}");
        }
        // 合計の切り捨ては1回（ラウンドごとに切り捨てた秒の和ではない）。
        m.detail.as_mut().unwrap().rounds = vec![
            rec(90, Winner::P1, [None; 2]),
            rec(90, Winner::P1, [None; 2]),
        ];
        assert_eq!(name_for("{length}", &m).as_deref(), Ok("0m03s"));
        assert_eq!(name_for("{rlengths}", &m).as_deref(), Ok("0m01s-0m01s"));
    }

    #[test]
    fn unreadable_life_is_x_and_unknown_round_winners_are_not_counted() {
        let mut m = play_mark();
        m.detail.as_mut().unwrap().rounds = vec![
            rec(60, Winner::Unknown, [None, Some(2)]),
            rec(60, Winner::P2, [Some(7), None]),
        ];
        assert_eq!(name_for("{rlifes}", &m).as_deref(), Ok("xvs2-7vsx"));
        assert_eq!(name_for("{wins}", &m).as_deref(), Ok("0vs1"));
    }

    #[test]
    fn ping_prefers_the_median_and_sync_is_none_without_a_record() {
        use crate::replay_detail::CommSummary;
        let mut m = play_mark();
        m.detail.as_mut().unwrap().comm = Some(CommSummary {
            ping_median_ms: Some(20),
            ping_max_ms: Some(90),
            stalls: 0,
            rewinds: 0,
            resim_ticks: 0,
        });
        assert_eq!(name_for("{ping}", &m).as_deref(), Ok("20ms"));
        m.detail.as_mut().unwrap().comm = Some(CommSummary {
            ping_median_ms: None,
            ping_max_ms: None,
            stalls: 0,
            rewinds: 0,
            resim_ticks: 0,
        });
        assert_eq!(name_for("{ping}", &m).as_deref(), Ok("33ms"));
        m.detail.as_mut().unwrap().sync = Some(SyncKind::Delay);
        assert_eq!(name_for("{sync}", &m).as_deref(), Ok("delay"));
        m.detail.as_mut().unwrap().sync = None;
        assert_eq!(name_for("{sync}", &m).as_deref(), Ok("none"));
    }

    #[test]
    fn a_mark_without_rounds_cannot_fill_the_round_placeholders_but_can_fill_the_rest() {
        let mut m = play_mark();
        m.detail.as_mut().unwrap().rounds.clear();
        for (t, what) in [
            ("{wins}", "wins"),
            ("{length}", "length"),
            ("{rlengths}", "rlengths"),
            ("{rlifes}", "rlifes"),
            ("{round}", "round"),
        ] {
            assert_eq!(name_for(t, &m), Err(NameError::Unreadable(what)), "{t}");
        }
        assert_eq!(name_for("{delay}-{side}", &m).as_deref(), Ok("2F-2P"));
    }

    #[test]
    fn a_name_over_100_characters_is_refused_whatever_the_placeholder() {
        let mut m = play_mark();
        // 印に入るラウンドは最大16。`0m10s`の5文字 × 16 ＋ 区切り15 = 95文字、`1vs0`の4文字 × 16 ＋ 15 = 79文字。
        m.detail.as_mut().unwrap().rounds = (0..16)
            .map(|_| rec(600, Winner::P1, [Some(1), Some(0)]))
            .collect();
        assert_eq!(name_for("{rlengths}", &m).map(|s| s.len()), Ok(95));
        assert_eq!(
            name_for("{rlengths}x{rlifes}", &m),
            Err(NameError::TooLong(175))
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_gives_nothing() {
        assert_eq!(read_file_facts(b"not a replay"), FileFacts::default());
        assert_eq!(read_file_facts(&[]), FileFacts::default());
        let f = read_file_facts(&marked(MatchResult::Win, Side::P1, 0, 0, [0, 1]));
        assert_eq!(f.rounds, None, "ラウンドの記録が無い印は数えない");
        assert_eq!(f.chars, Some([0, 1]));
        let unmarked = build_replay(&BuildSpec::match_replay(0x60, vec![0; 30], vec![0; 30]));
        let f = read_file_facts(&unmarked);
        assert_eq!((f.rounds, f.winner), (None, None));
        assert!(f.chars.is_some());
    }
}
