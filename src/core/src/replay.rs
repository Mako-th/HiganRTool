//! リプレイ（`.rpy`）の平文の部分を読み、HiganRTool の印（USER チャンク）を作って読む。
//!
//! - 読むのは平文だけ: 先頭の `"T9RP"`、`comp_size`（`+0x0C`）、`comp_size` から末尾までの USER チャンクの列。
//!   `+0x18` から `comp_size` の手前（暗号化と圧縮）は読まない。印には要らず、復号を実装しても合成の標本を同じ実装で
//!   作ることになり、復号の間違いを検査が見つけられないため。
//! - `comp_size` は `0x0042063B`（実測 1455/1455）、USER の全長の丸めは `0x00420F49`。
//! - 印は thprac の `ReplaySaveParam` と同じ形（末尾に USER を 1 つ足し、`+0x08` をタグにする）。仕組みだけの参考で、
//!   コードはコピーしていない。タグは `HGRT`。
//!
//! ファイルには触らない（バイト列を受け、足すバイト列・読めた印・理由を返すだけ）。書くのは dll。ゲームスレッドの外で呼ぶので
//! ヒープを使う。どんなバイト列でもパニックしない（添字は `get`、足し算は `checked_add`）。

use crate::frame::MAX_DELAY;
use crate::replay_detail::{self, DetailFields, MatchDetail};
use crate::wire::NAME_MAX;

/// ファイルの先頭 4 バイト（`0x000`）。
pub const MAGIC: [u8; 4] = *b"T9RP";
/// `comp_size`（u32 LE）の位置（`= 0xC0 + LZSS の長さ`）。
const COMP_SIZE_OFFSET: usize = 0x0C;
/// これより短いファイルは見出しすら無い（`0x018` から暗号化）。
pub const MIN_FILE_LEN: usize = 0x18;
/// `comp_size` の下限。LZSS の長さが 0 でも `0xC0` はある。
pub const MIN_COMP_SIZE: u32 = 0xC0;
pub const USER_MAGIC: [u8; 4] = *b"USER";
/// USER チャンクの見出しの長さ（`"USER"` 4 ＋ 全長 4 ＋ `+0x08` の 4）。
pub const USER_HEADER_LEN: usize = 12;
/// 印のタグ（USER の `+0x08`）。ゲームの USER ではここは未初期化の残り物（実測ほぼ `0x7FFFFF00`）なので、`HGRT` と偶然一致することは考えない。
pub const MARK_TAG: [u8; 4] = *b"HGRT";
pub const MARK_HEAD: &str = "HIGANR";
pub const MARK_VERSION: &str = "1";

/// ファイルの平文の部分が読めなかった理由（印を付けずにログへ出す）。ゲームの保存が壊れている可能性があるので、直さずに返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
    /// 長さが `0x18` 未満。
    TooShort { len: usize },
    /// 先頭 4 バイトが `T9RP` でない。
    BadMagic,
    /// `comp_size` が `0xC0` 未満。
    CompSizeTooSmall { comp_size: u32 },
    /// `comp_size` がファイルの長さを超える。
    CompSizeBeyondEnd { comp_size: u32, len: usize },
    /// `offset` のチャンクが `"USER"` で始まらない。
    NotUser { offset: usize },
    /// `offset` のチャンクの全長が 12 未満（見出しが収まらない）。
    ChunkTooShort { offset: usize, total: u32 },
    /// `offset` のチャンクの全長がファイルの残りを超える。
    ChunkBeyondEnd {
        offset: usize,
        total: u32,
        remaining: usize,
    },
    /// 最後のチャンクの後ろに、チャンクの見出し（12 バイト）にも満たない余りがある。
    TrailingBytes { offset: usize, len: usize },
    /// USER チャンクが 1 つも無い（ゲームは必ず 1 つ書く）。
    NoUserChunk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserChunk {
    pub offset: usize,
    /// 全長（`+0x04`。見出しの 12 バイトを含む）。
    pub len: usize,
    /// `+0x08` の 4 バイト。印なら `HGRT`。
    pub tag: [u8; 4],
}

impl UserChunk {
    /// 本文（`+0x0C` から全長まで。末尾の NUL と詰め物を含む）。違うバイト列に当てても範囲外なら `None` で、パニックしない。
    pub fn body<'a>(&self, file: &'a [u8]) -> Option<&'a [u8]> {
        let start = self.offset.checked_add(USER_HEADER_LEN)?;
        let end = self.offset.checked_add(self.len)?;
        file.get(start..end)
    }

    pub fn is_mark(&self) -> bool {
        self.tag == MARK_TAG
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `+0x0C`。`[0, comp_size)` がチェックサムの範囲を含む。
    pub comp_size: u32,
    /// `comp_size` から末尾まで、隙間なく並んだ USER チャンク（1 つ以上）。
    pub chunks: Vec<UserChunk>,
}

impl Layout {
    pub fn has_mark(&self) -> bool {
        self.chunks.iter().any(UserChunk::is_mark)
    }
}

fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let s = bytes.get(off..end)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_tag(bytes: &[u8], off: usize) -> Option<[u8; 4]> {
    let end = off.checked_add(4)?;
    let s = bytes.get(off..end)?;
    Some([s[0], s[1], s[2], s[3]])
}

/// 平文の部分を読む。暗号化・圧縮された部分は読まない。残りが 12 バイト未満のところは `TrailingBytes`、
/// 12 バイト以上あって `"USER"` で始まらなければ `NotUser`（どちらも印を付けない）。
pub fn parse(bytes: &[u8]) -> Result<Layout, ReplayError> {
    let len = bytes.len();
    if len < MIN_FILE_LEN {
        return Err(ReplayError::TooShort { len });
    }
    if bytes.get(0..4) != Some(&MAGIC[..]) {
        return Err(ReplayError::BadMagic);
    }
    let comp_size = read_u32(bytes, COMP_SIZE_OFFSET).ok_or(ReplayError::TooShort { len })?;
    if comp_size < MIN_COMP_SIZE {
        return Err(ReplayError::CompSizeTooSmall { comp_size });
    }
    // i686 が対象なので u32 は必ず usize に収まる。
    let comp = comp_size as usize;
    if comp > len {
        return Err(ReplayError::CompSizeBeyondEnd { comp_size, len });
    }

    let mut chunks = Vec::new();
    let mut pos = comp;
    while pos < len {
        let remaining = len - pos;
        if remaining < USER_HEADER_LEN {
            return Err(ReplayError::TrailingBytes {
                offset: pos,
                len: remaining,
            });
        }
        if read_tag(bytes, pos) != Some(USER_MAGIC) {
            return Err(ReplayError::NotUser { offset: pos });
        }
        // remaining >= 12 を上で確かめたので読める。読めなければ余りとして断る。
        let total = read_u32(bytes, pos + 4).ok_or(ReplayError::TrailingBytes {
            offset: pos,
            len: remaining,
        })?;
        if (total as usize) < USER_HEADER_LEN {
            return Err(ReplayError::ChunkTooShort { offset: pos, total });
        }
        if total as usize > remaining {
            return Err(ReplayError::ChunkBeyondEnd {
                offset: pos,
                total,
                remaining,
            });
        }
        let tag = read_tag(bytes, pos + 8).ok_or(ReplayError::TrailingBytes {
            offset: pos,
            len: remaining,
        })?;
        chunks.push(UserChunk {
            offset: pos,
            len: total as usize,
            tag,
        });
        // total <= len - pos なので、あふれない。
        pos += total as usize;
    }
    if chunks.is_empty() {
        return Err(ReplayError::NoUserChunk);
    }
    Ok(Layout { comp_size, chunks })
}

/// どちら側で遊んだか（`side=<1|2>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    P1,
    P2,
}

/// マッチの結果（`result=`。自分から見た結果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    Win,
    Lose,
    Draw,
    Abort,
}

impl MatchResult {
    fn as_str(self) -> &'static str {
        match self {
            MatchResult::Win => "win",
            MatchResult::Lose => "lose",
            MatchResult::Draw => "draw",
            MatchResult::Abort => "abort",
        }
    }
}

/// 手元の時刻（core は時計を持たないので dll が渡す）。書式は `YYYY-MM-DDTHH:MM:SS`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn is_leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl Timestamp {
    /// 4 桁の年、暦にある月日、`00:00:00`〜`23:59:59`（閏秒は書かない）のとき真。範囲外を書き出すと読むときに「壊れている」になる。
    pub fn is_valid(&self) -> bool {
        self.year <= 9999
            && (1..=12).contains(&self.month)
            && self.day >= 1
            && self.day <= days_in_month(self.year, self.month)
            && self.hour <= 23
            && self.minute <= 59
            && self.second <= 59
    }

    pub fn to_text(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `YYYY-MM-DDTHH:MM:SS` を読む。桁数と区切りが違う・範囲外なら `None`。
    pub fn from_text(text: &[u8]) -> Option<Timestamp> {
        if text.len() != 19 {
            return None;
        }
        let sep_ok = text[4] == b'-'
            && text[7] == b'-'
            && text[10] == b'T'
            && text[13] == b':'
            && text[16] == b':';
        if !sep_ok {
            return None;
        }
        let num = |a: usize, b: usize| -> Option<u16> {
            let mut v: u16 = 0;
            for &c in text.get(a..b)? {
                if !c.is_ascii_digit() {
                    return None;
                }
                // 4 桁まで（9999）なので u16 に収まる。
                v = v * 10 + u16::from(c - b'0');
            }
            Some(v)
        };
        let ts = Timestamp {
            year: num(0, 4)?,
            // 2 桁なので u8 に収まる。
            month: num(5, 7)? as u8,
            day: num(8, 10)? as u8,
            hour: num(11, 13)? as u8,
            minute: num(14, 16)? as u8,
            second: num(17, 19)? as u8,
        };
        ts.is_valid().then_some(ts)
    }
}

/// 印を残した側。観戦者の手元にも観戦したマッチのリプレイを残す。key は `role`・`p1`・`p2`（バージョン 1 のまま足した任意の key。
/// 知らない key は読み飛ばす約束なので、足す前の読み手も観戦の印を普通の印として読める）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkRole {
    /// 対戦者として遊んだ（`role` の key は書かない。足す前の印と同じバイト列）。
    Play,
    /// 観戦者として見た（`role=watch`）。`p1`・`p2` はマッチの 1P・2P の名前（無害化の前。UTF-8、32 バイトまで）。この印の `self_name` は
    /// 観戦した人の名前、`peer` は空、`side` は 1、`result` は 1P から見た結果、`lat`・`ping` は 0。
    Watch { p1: String, p2: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// 相手の名前。無害化の前（UTF-8、32 バイトまで。HELLO と同じ上限）。
    pub peer: String,
    pub self_name: String,
    pub side: Side,
    /// latency（0〜9。`frame::MAX_DELAY`）。
    pub lat: u8,
    pub ping_ms: u32,
    pub result: MatchResult,
    pub at: Timestamp,
    pub role: MarkRole,
    /// マッチの情報（ラウンドごとの記録・同期方式・ビルド番号・開始時刻・通信の要約。`replay_detail`）。`None` なら足す前の印と同じ
    /// バイト列になる。
    pub detail: Option<MatchDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkError {
    NotHiganr,
    MissingVersion,
    /// 第 3 欄から先に `=` の無い欄がある（空の欄を含む）。
    FieldWithoutEquals,
    DuplicateKey(&'static str),
    MissingKey(&'static str),
    BadEscape(&'static str),
    /// 値が範囲の外（名前が UTF-8 でない・32 バイトを超える、side が 1 / 2 でない、など）。
    BadValue(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendError {
    /// ファイルの平文の部分が読めない（ゲームの保存が壊れている可能性。直さずにログへ）。
    Broken(ReplayError),
    AlreadyMarked,
    /// 渡された印の値が範囲の外（書けば読むときに「壊れている」になる）。
    InvalidMark(MarkError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkRead {
    Absent,
    /// `HGRT` はあるが、どれも知らないバージョン（読み飛ばした。§2）。
    UnknownVersion,
    Broken(MarkError),
    Found(Mark),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyRead {
    UnknownVersion,
    Mark(Mark),
}

/// 値の逃がし方: `[0-9A-Za-z-_.~]` 以外のバイトを `%XX`（大文字の 16 進）にする。タブ・`=`・`%` を含む名前でも欄が崩れない。
fn escape_value(value: &[u8]) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        // 書き出しは大文字だけだが、読みは小文字も受ける（人が手で直した印でも読めるように）。
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// `%XX` を戻す。`%` の後ろが 16 進 2 桁でなければ `None`。`%` 以外のバイトはそのまま。
fn unescape_value(value: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        let b = value[i];
        if b == b'%' {
            let hi = hex_digit(*value.get(i + 1)?)?;
            let lo = hex_digit(*value.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some(out)
}

fn check_mark(mark: &Mark) -> Result<(), MarkError> {
    if mark.peer.len() > NAME_MAX {
        return Err(MarkError::BadValue("peer"));
    }
    if mark.self_name.len() > NAME_MAX {
        return Err(MarkError::BadValue("self"));
    }
    if mark.lat > MAX_DELAY {
        return Err(MarkError::BadValue("lat"));
    }
    if let MarkRole::Watch { p1, p2 } = &mark.role {
        if p1.len() > NAME_MAX {
            return Err(MarkError::BadValue("p1"));
        }
        if p2.len() > NAME_MAX {
            return Err(MarkError::BadValue("p2"));
        }
    }
    if !mark.at.is_valid() {
        return Err(MarkError::BadValue("at"));
    }
    if let Some(d) = &mark.detail {
        d.check()?;
    }
    Ok(())
}

/// 印の本文（1 行。NUL は含まない）。key の順は固定。
pub fn mark_body(mark: &Mark) -> Result<String, MarkError> {
    check_mark(mark)?;
    let side = match mark.side {
        Side::P1 => "1",
        Side::P2 => "2",
    };
    let fields: [(&str, String); 7] = [
        ("peer", escape_value(mark.peer.as_bytes())),
        ("self", escape_value(mark.self_name.as_bytes())),
        ("side", escape_value(side.as_bytes())),
        ("lat", escape_value(mark.lat.to_string().as_bytes())),
        ("ping", escape_value(mark.ping_ms.to_string().as_bytes())),
        ("result", escape_value(mark.result.as_str().as_bytes())),
        // 値の中の `:` も逃がすので、時刻の `:` は `%3A` になる。
        ("at", escape_value(mark.at.to_text().as_bytes())),
    ];
    let mut body = format!("{MARK_HEAD}\t{MARK_VERSION}");
    for (key, value) in &fields {
        body.push('\t');
        body.push_str(key);
        body.push('=');
        body.push_str(value);
    }
    // 観戦の印だけが、必須の 7 つの後ろに任意の key を足す。
    if let MarkRole::Watch { p1, p2 } = &mark.role {
        for (key, value) in [("role", "watch"), ("p1", p1.as_str()), ("p2", p2.as_str())] {
            body.push('\t');
            body.push_str(key);
            body.push('=');
            body.push_str(&escape_value(value.as_bytes()));
        }
    }
    if let Some(d) = &mark.detail {
        for (key, value) in d.fields() {
            body.push('\t');
            body.push_str(key);
            body.push('=');
            body.push_str(&escape_value(value.as_bytes()));
        }
    }
    Ok(body)
}

/// 印のチャンク 1 つ: `"USER"` ＋ 全長 ＋ `"HGRT"` ＋ 本文 ＋ NUL ＋ 詰め物の 0。
/// 全長はゲームの USER と同じく偶数へ切り上げる（`strlen + 13` を偶数へ。`0x00420F49`）。
pub fn mark_chunk(mark: &Mark) -> Result<Vec<u8>, MarkError> {
    let body = mark_body(mark)?;
    // 本文は最大でも数百バイトなので u32 に収まる。念のため確かめて、収まらなければ断る。
    let unpadded = USER_HEADER_LEN + body.len() + 1;
    let total = unpadded + (unpadded & 1);
    let total_u32 = u32::try_from(total).map_err(|_| MarkError::BadValue("body"))?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&USER_MAGIC);
    out.extend_from_slice(&total_u32.to_le_bytes());
    out.extend_from_slice(&MARK_TAG);
    out.extend_from_slice(body.as_bytes());
    out.push(0);
    out.resize(total, 0);
    Ok(out)
}

/// `file` の末尾へ足す印のチャンクを返す（dll はそのままファイルの末尾へ書く）。壊れている・既に印がある・印の値が範囲外、を分けて断る。
/// `parse` が最後のチャンクの後ろに余りが無いことを確かめているので、足したチャンクもたどれる。
pub fn chunk_to_append(file: &[u8], mark: &Mark) -> Result<Vec<u8>, AppendError> {
    let layout = parse(file).map_err(AppendError::Broken)?;
    if layout.has_mark() {
        return Err(AppendError::AlreadyMarked);
    }
    mark_chunk(mark).map_err(AppendError::InvalidMark)
}

/// バージョン 1 の key。先頭の 7 つは必須、`role`・`p1`・`p2` は観戦の印にだけ付く任意の key。マッチの情報の key は `replay_detail::ALLOWED_KEYS`。
const KEYS: [&str; 10] = [
    "peer", "self", "side", "lat", "ping", "result", "at", "role", "p1", "p2",
];

fn parse_name(key: &'static str, value: Vec<u8>) -> Result<String, MarkError> {
    if value.len() > NAME_MAX {
        return Err(MarkError::BadValue(key));
    }
    String::from_utf8(value).map_err(|_| MarkError::BadValue(key))
}

/// 1 つの `HGRT` の本文を読む。本文は最初の NUL まで。バージョンが `1` でなければ `UnknownVersion`。バージョン 1 は 7 つの key が
/// 1 回ずつそろい、値が範囲の中のときだけ `Mark`。知らない key は無視する（後のバージョンで足せるように）。
pub fn parse_mark_body(body: &[u8]) -> Result<BodyRead, MarkError> {
    let text = match body.iter().position(|&b| b == 0) {
        Some(n) => &body[..n],
        None => body,
    };
    let mut fields = text.split(|&b| b == b'\t');
    if fields.next() != Some(MARK_HEAD.as_bytes()) {
        return Err(MarkError::NotHiganr);
    }
    let version = fields.next().ok_or(MarkError::MissingVersion)?;
    if version != MARK_VERSION.as_bytes() {
        return Ok(BodyRead::UnknownVersion);
    }

    let mut values: [Option<Vec<u8>>; KEYS.len()] = Default::default();
    let mut detail_fields = DetailFields::default();
    for field in fields {
        let eq = field
            .iter()
            .position(|&b| b == b'=')
            .ok_or(MarkError::FieldWithoutEquals)?;
        let (key, value) = (&field[..eq], &field[eq + 1..]);
        let Some(idx) = KEYS.iter().position(|k| k.as_bytes() == key) else {
            // マッチの情報の key は `replay_detail` が集める。それ以外の知らない key は無視する。
            if let Some(d_idx) = replay_detail::key_index(key) {
                let bytes = unescape_value(value)
                    .ok_or(MarkError::BadEscape(replay_detail::key_name(d_idx)))?;
                detail_fields.put(d_idx, bytes)?;
            }
            continue;
        };
        let name = KEYS[idx];
        if values[idx].is_some() {
            return Err(MarkError::DuplicateKey(name));
        }
        values[idx] = Some(unescape_value(value).ok_or(MarkError::BadEscape(name))?);
    }
    let mut take = |idx: usize| values[idx].take().ok_or(MarkError::MissingKey(KEYS[idx]));
    let peer = take(0)?;
    let self_name = take(1)?;
    let side = take(2)?;
    let lat = take(3)?;
    let ping = take(4)?;
    let result = take(5)?;
    let at = take(6)?;
    // 任意の key。`role` が無い（か `play`）なら対戦者の印で、`p1`・`p2` があっても使わない（後のバージョンで意味を足せるように）。
    let role_value = values[7].take();
    let p1 = values[8].take();
    let p2 = values[9].take();
    let detail = detail_fields.finish()?;

    let side = match side.as_slice() {
        b"1" => Side::P1,
        b"2" => Side::P2,
        _ => return Err(MarkError::BadValue("side")),
    };
    let lat = match lat.as_slice() {
        [d] if d.is_ascii_digit() && *d - b'0' <= MAX_DELAY => *d - b'0',
        _ => return Err(MarkError::BadValue("lat")),
    };
    // `+` や空白を受けないように、数字だけを見てから変換する。
    if ping.is_empty() || !ping.iter().all(u8::is_ascii_digit) {
        return Err(MarkError::BadValue("ping"));
    }
    let ping_ms = std::str::from_utf8(&ping)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or(MarkError::BadValue("ping"))?;
    let result = match result.as_slice() {
        b"win" => MatchResult::Win,
        b"lose" => MatchResult::Lose,
        b"draw" => MatchResult::Draw,
        b"abort" => MatchResult::Abort,
        _ => return Err(MarkError::BadValue("result")),
    };
    let at = Timestamp::from_text(&at).ok_or(MarkError::BadValue("at"))?;
    let role = match role_value.as_deref() {
        None | Some(b"play") => MarkRole::Play,
        Some(b"watch") => MarkRole::Watch {
            p1: parse_name("p1", p1.ok_or(MarkError::MissingKey("p1"))?)?,
            p2: parse_name("p2", p2.ok_or(MarkError::MissingKey("p2"))?)?,
        },
        Some(_) => return Err(MarkError::BadValue("role")),
    };
    Ok(BodyRead::Mark(Mark {
        peer: parse_name("peer", peer)?,
        self_name: parse_name("self", self_name)?,
        side,
        lat,
        ping_ms,
        result,
        at,
        role,
        detail,
    }))
}

/// ファイルから印を読む。`comp_size` から USER を順にたどり、タグ `HGRT` のものだけを見る。知らないバージョンは読み飛ばし、
/// 最初のバージョン 1 の結果（読めた / 壊れている）を返す。ファイル自体が読めなければ `Err`。
pub fn read_mark(file: &[u8]) -> Result<MarkRead, ReplayError> {
    let layout = parse(file)?;
    let mut saw_unknown = false;
    for chunk in layout.chunks.iter().filter(|c| c.is_mark()) {
        // parse が作った位置なので必ず読める。読めなければ壊れているとは言わず飛ばす（パニックさせない）。
        let Some(body) = chunk.body(file) else {
            continue;
        };
        match parse_mark_body(body) {
            Ok(BodyRead::UnknownVersion) => saw_unknown = true,
            Ok(BodyRead::Mark(mark)) => return Ok(MarkRead::Found(mark)),
            Err(e) => return Ok(MarkRead::Broken(e)),
        }
    }
    Ok(if saw_unknown {
        MarkRead::UnknownVersion
    } else {
        MarkRead::Absent
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_detail::{CommSummary, RoundRec, SyncKind, Winner};

    struct XorShift(u32);
    impl XorShift {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    /// ゲームと同じ形の USER チャンク（全長 = strlen(本文) + 13 を偶数へ、`+0x08` は実測の残り物 `0x7FFFFF00`、末尾 NUL）。
    /// `mark_chunk` を使わず手で組む（同じ式から作ると丸めの間違いを見逃す）。
    fn game_user_chunk(text: &[u8]) -> Vec<u8> {
        let total = (text.len() + 13 + 1) & !1;
        let mut c = Vec::new();
        c.extend_from_slice(b"USER");
        c.extend_from_slice(&(total as u32).to_le_bytes());
        c.extend_from_slice(&0x7FFF_FF00u32.to_le_bytes());
        c.extend_from_slice(text);
        c.push(0);
        c.resize(total, 0);
        c
    }

    fn synthetic_replay(comp_size: u32, seed: u32) -> Vec<u8> {
        let mut rng = XorShift(seed);
        let mut f: Vec<u8> = (0..comp_size).map(|_| rng.next() as u8).collect();
        f[0..4].copy_from_slice(b"T9RP");
        f[12..16].copy_from_slice(&comp_size.to_le_bytes());
        f.extend(game_user_chunk(
            b"\x83\x76\x83\x8C\x83\x43\x83\x84\x81\x5B\x96\xBC\tNO NAME\r\n",
        ));
        f
    }

    fn sample_mark(peer: &str, self_name: &str) -> Mark {
        Mark {
            peer: peer.to_string(),
            self_name: self_name.to_string(),
            side: Side::P2,
            lat: 3,
            ping_ms: 48,
            result: MatchResult::Draw,
            at: Timestamp {
                year: 2026,
                month: 9,
                day: 25,
                hour: 23,
                minute: 5,
                second: 9,
            },
            role: MarkRole::Play,
            detail: None,
        }
    }

    fn round(n: u32, winner: Winner) -> RoundRec {
        RoundRec {
            frames: 3000 + n,
            winner,
            life: [Some(3), Some(0)],
            c23: [Some(n), Some(1)],
            boss: [Some(1), Some(0)],
            rev: [Some(0), Some(0)],
            score: [Some(10 * u64::from(n)), None],
        }
    }

    /// 3 ラウンド・通信の要約つきの、マッチの情報の見本。
    fn sample_detail() -> MatchDetail {
        MatchDetail {
            sync: Some(SyncKind::Rollback),
            build: Some(403_920),
            start_utc: Some(1_790_000_000),
            match_no: Some(3),
            comm: Some(CommSummary {
                ping_median_ms: Some(31),
                ping_max_ms: Some(120),
                stalls: 2,
                rewinds: 40,
                resim_ticks: 90,
            }),
            rounds: vec![
                round(1, Winner::P1),
                round(2, Winner::P2),
                round(3, Winner::P1),
            ],
            rounds_dropped: 0,
        }
    }

    #[test]
    fn append_then_read_round_trips_and_keeps_the_game_bytes() {
        let names = [
            ("相手のなまえ", "自分"),
            ("100%\ttab=eq sp", "a b%20c"),
            ("花映塚花映塚花映塚花xy", "0123456789abcdef0123456789abcdef"),
            ("", "~-_."),
        ];
        for (i, (peer, me)) in names.iter().enumerate() {
            assert!(peer.len() <= 32 && me.len() <= 32);
            let comp_size = 0xC0 + 17 * i as u32 + 1;
            let file = synthetic_replay(comp_size, 0x1234 + i as u32);
            let before = parse(&file).unwrap();
            assert_eq!(before.chunks.len(), 1);
            let mark = sample_mark(peer, me);
            let chunk = chunk_to_append(&file, &mark).unwrap();
            assert_eq!(chunk.len() % 2, 0, "全長は偶数");
            assert_eq!(
                u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as usize,
                chunk.len()
            );
            let mut marked = file.clone();
            marked.extend_from_slice(&chunk);

            let after = parse(&marked).unwrap();
            assert_eq!(after.comp_size, comp_size);
            assert_eq!(after.chunks.len(), 2);
            assert_eq!(after.chunks[0], before.chunks[0]);
            assert!(after.chunks[1].is_mark());
            assert_eq!(&marked[..file.len()], &file[..]);
            assert_eq!(&marked[..comp_size as usize], &file[..comp_size as usize]);
            assert_eq!(read_mark(&marked).unwrap(), MarkRead::Found(mark));
        }
    }

    #[test]
    fn mark_chunk_matches_the_hand_written_bytes() {
        let mut mark = sample_mark("a b", "Z");
        mark.side = Side::P1;
        mark.result = MatchResult::Win;
        let body = "HIGANR\t1\tpeer=a%20b\tself=Z\tside=1\tlat=3\tping=48\tresult=win\t\
                    at=2026-09-25T23%3A05%3A09";
        assert_eq!(mark_body(&mark).unwrap(), body);
        let chunk = mark_chunk(&mark).unwrap();
        let total = (12 + body.len() + 1 + 1) & !1;
        assert_eq!(chunk.len(), total);
        assert_eq!(&chunk[0..4], b"USER");
        assert_eq!(&chunk[8..12], b"HGRT");
        assert_eq!(&chunk[12..12 + body.len()], body.as_bytes());
        assert!(chunk[12 + body.len()..].iter().all(|&b| b == 0));
        let odd = sample_mark("x\ty=%", "");
        assert!(mark_body(&odd).unwrap().contains("peer=x%09y%3D%25\t"));
    }

    fn watch_mark(p1: &str, p2: &str, me: &str) -> Mark {
        Mark {
            peer: String::new(),
            self_name: me.to_string(),
            side: Side::P1,
            lat: 0,
            ping_ms: 0,
            result: MatchResult::Win,
            role: MarkRole::Watch {
                p1: p1.to_string(),
                p2: p2.to_string(),
            },
            ..sample_mark("", "")
        }
    }

    #[test]
    fn a_watch_mark_carries_role_and_both_names_and_round_trips() {
        let m = watch_mark("一郎", "B b", "見る人");
        let body = mark_body(&m).unwrap();
        assert_eq!(
            body,
            "HIGANR\t1\tpeer=\tself=%E8%A6%8B%E3%82%8B%E4%BA%BA\tside=1\tlat=0\tping=0\tresult=win\t\
             at=2026-09-25T23%3A05%3A09\trole=watch\tp1=%E4%B8%80%E9%83%8E\tp2=B%20b"
        );
        let file = synthetic_replay(0xC8, 21);
        let mut marked = file.clone();
        marked.extend(chunk_to_append(&file, &m).unwrap());
        assert_eq!(read_mark(&marked).unwrap(), MarkRead::Found(m));
        assert!(!mark_body(&sample_mark("p", "s")).unwrap().contains("role"));
    }

    #[test]
    fn a_reader_without_the_role_keys_still_reads_a_watch_mark() {
        // 足す前の読みは、`role`・`p1`・`p2` を除いた本文を読むことで模す。
        let m = watch_mark("a", "b", "me");
        let body = mark_body(&m).unwrap();
        let legacy: Vec<&str> = body
            .split('\t')
            .filter(|f| !(f.starts_with("role=") || f.starts_with("p1=") || f.starts_with("p2=")))
            .collect();
        let BodyRead::Mark(old) = parse_mark_body(legacy.join("\t").as_bytes()).unwrap() else {
            panic!("足す前の本文が読めない");
        };
        assert_eq!(old.role, MarkRole::Play);
        assert_eq!((old.self_name.as_str(), old.side), ("me", Side::P1));
    }

    #[test]
    fn watch_mark_values_are_checked_when_written_and_read() {
        let long = "x".repeat(33);
        assert_eq!(
            mark_chunk(&watch_mark(&long, "b", "me")),
            Err(MarkError::BadValue("p1"))
        );
        assert_eq!(
            mark_chunk(&watch_mark("a", &long, "me")),
            Err(MarkError::BadValue("p2"))
        );
        let good = mark_body(&watch_mark("a", "b", "me")).unwrap();
        assert!(parse_mark_body(good.as_bytes()).is_ok());
        let broken = |body: &str| parse_mark_body(body.as_bytes()).unwrap_err();
        assert_eq!(
            broken(&good.replace("\tp2=b", "")),
            MarkError::MissingKey("p2")
        );
        assert_eq!(
            broken(&good.replace("\tp1=a", "")),
            MarkError::MissingKey("p1")
        );
        assert_eq!(
            broken(&good.replace("role=watch", "role=ghost")),
            MarkError::BadValue("role")
        );
        assert_eq!(
            broken(&format!("{good}\trole=play")),
            MarkError::DuplicateKey("role")
        );
        assert_eq!(
            broken(&good.replace("p1=a", "p1=%FF")),
            MarkError::BadValue("p1")
        );
        let BodyRead::Mark(m) = parse_mark_body(format!("{V1}\tp1=zz\tp2=yy").as_bytes()).unwrap()
        else {
            panic!("読めない");
        };
        assert_eq!(m.role, MarkRole::Play);
        let BodyRead::Mark(m) = parse_mark_body(format!("{V1}\trole=play").as_bytes()).unwrap()
        else {
            panic!("読めない");
        };
        assert_eq!(m.role, MarkRole::Play);
    }

    #[test]
    fn broken_files_are_refused_with_their_own_reason() {
        let good = synthetic_replay(0xC4, 7);
        let comp = 0xC4usize;
        let mark = sample_mark("p", "s");
        let refuse = |f: &[u8]| {
            let e = parse(f).unwrap_err();
            assert_eq!(chunk_to_append(f, &mark), Err(AppendError::Broken(e)));
            assert_eq!(read_mark(f), Err(e));
            e
        };

        assert_eq!(refuse(&good[..0x17]), ReplayError::TooShort { len: 0x17 });
        assert_eq!(refuse(&[]), ReplayError::TooShort { len: 0 });

        let mut f = good.clone();
        f[0..4].copy_from_slice(b"T8RP");
        assert_eq!(refuse(&f), ReplayError::BadMagic);

        let mut f = good.clone();
        f[12..16].copy_from_slice(&0xBFu32.to_le_bytes());
        assert_eq!(
            refuse(&f),
            ReplayError::CompSizeTooSmall { comp_size: 0xBF }
        );

        let mut f = good.clone();
        let too_big = good.len() as u32 + 1;
        f[12..16].copy_from_slice(&too_big.to_le_bytes());
        assert_eq!(
            refuse(&f),
            ReplayError::CompSizeBeyondEnd {
                comp_size: too_big,
                len: good.len()
            }
        );
        f[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(refuse(&f), ReplayError::CompSizeBeyondEnd { .. }));

        let mut f = good.clone();
        f[comp..comp + 4].copy_from_slice(b"USEr");
        assert_eq!(refuse(&f), ReplayError::NotUser { offset: comp });

        let mut f = good.clone();
        f[comp + 4..comp + 8].copy_from_slice(&11u32.to_le_bytes());
        assert_eq!(
            refuse(&f),
            ReplayError::ChunkTooShort {
                offset: comp,
                total: 11
            }
        );
        // 全長 0 は「次のチャンクが同じ位置」で止まらなくなる形。
        f[comp + 4..comp + 8].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(refuse(&f), ReplayError::ChunkTooShort { .. }));

        let mut f = good.clone();
        let remaining = good.len() - comp;
        f[comp + 4..comp + 8].copy_from_slice(&(remaining as u32 + 2).to_le_bytes());
        assert_eq!(
            refuse(&f),
            ReplayError::ChunkBeyondEnd {
                offset: comp,
                total: remaining as u32 + 2,
                remaining
            }
        );
        assert!(matches!(
            refuse(&good[..good.len() - 1]),
            ReplayError::ChunkBeyondEnd { .. }
        ));

        let mut f = good.clone();
        f.extend_from_slice(&[0u8; 5]);
        assert_eq!(
            refuse(&f),
            ReplayError::TrailingBytes {
                offset: good.len(),
                len: 5
            }
        );
        let mut f = good.clone();
        f.extend_from_slice(&[0u8; 12]);
        assert_eq!(refuse(&f), ReplayError::NotUser { offset: good.len() });

        assert_eq!(refuse(&good[..comp]), ReplayError::NoUserChunk);
    }

    #[test]
    fn second_mark_is_refused_separately_from_broken() {
        let file = synthetic_replay(0xD0, 9);
        let mark = sample_mark("p", "s");
        let mut marked = file.clone();
        marked.extend(chunk_to_append(&file, &mark).unwrap());
        assert_eq!(
            chunk_to_append(&marked, &mark),
            Err(AppendError::AlreadyMarked)
        );
        // 知らないバージョンでもタグで見るので「既にある」。
        let mut other = file.clone();
        let mut c = game_user_chunk(b"HIGANR\t7\tfoo=bar");
        c[8..12].copy_from_slice(b"HGRT");
        other.extend(c);
        assert_eq!(
            chunk_to_append(&other, &mark),
            Err(AppendError::AlreadyMarked)
        );
    }

    #[test]
    fn out_of_range_mark_values_are_not_written() {
        let file = synthetic_replay(0xC0, 3);
        let mut m = sample_mark(&"a".repeat(33), "s");
        assert_eq!(
            chunk_to_append(&file, &m),
            Err(AppendError::InvalidMark(MarkError::BadValue("peer")))
        );
        m = sample_mark("p", &"b".repeat(33));
        assert_eq!(mark_chunk(&m), Err(MarkError::BadValue("self")));
        m = sample_mark("p", "s");
        m.lat = 10;
        assert_eq!(mark_chunk(&m), Err(MarkError::BadValue("lat")));
        m = sample_mark("p", "s");
        m.at.month = 2;
        m.at.day = 29; // 2026 は閏年でない
        assert_eq!(mark_chunk(&m), Err(MarkError::BadValue("at")));
        m.at.year = 2028;
        assert!(mark_chunk(&m).is_ok());
    }

    fn file_with_mark_bodies(bodies: &[&[u8]]) -> Vec<u8> {
        let mut f = synthetic_replay(0xC0, 11);
        for b in bodies {
            let mut c = game_user_chunk(b);
            c[8..12].copy_from_slice(b"HGRT");
            f.extend(c);
        }
        f
    }

    const V1: &str = "HIGANR\t1\tpeer=p\tself=s\tside=2\tlat=0\tping=0\tresult=abort\t\
                      at=2026-01-02T03%3A04%3A05";

    #[test]
    fn read_mark_skips_unknown_versions_and_ignores_unknown_keys() {
        let plain = synthetic_replay(0xC0, 5);
        assert_eq!(read_mark(&plain).unwrap(), MarkRead::Absent);

        let unknown = file_with_mark_bodies(&[b"HIGANR\t2\twhatever"]);
        assert_eq!(read_mark(&unknown).unwrap(), MarkRead::UnknownVersion);

        let both = file_with_mark_bodies(&[b"HIGANR\t2\tx", V1.as_bytes()]);
        let MarkRead::Found(m) = read_mark(&both).unwrap() else {
            panic!("バージョン 1 を読めていない");
        };
        assert_eq!(m.side, Side::P2);
        assert_eq!(m.result, MatchResult::Abort);

        let extra = format!("{V1}\tfuture=%41\tzzz=");
        let MarkRead::Found(m2) = read_mark(&file_with_mark_bodies(&[extra.as_bytes()])).unwrap()
        else {
            panic!("知らない key で断っている");
        };
        assert_eq!(m2, m);
        let lower = V1.replace("%3A", "%3a");
        assert_eq!(
            parse_mark_body(lower.as_bytes()).unwrap(),
            BodyRead::Mark(m)
        );
    }

    #[test]
    fn read_mark_reports_broken_v1_bodies() {
        let broken = |body: &str| match read_mark(&file_with_mark_bodies(&[body.as_bytes()])) {
            Ok(MarkRead::Broken(e)) => e,
            other => panic!("{body:?} が壊れているにならない: {other:?}"),
        };
        assert_eq!(
            broken(&format!("{V1}\tside=1")),
            MarkError::DuplicateKey("side")
        );
        assert_eq!(
            broken(&V1.replace("\tping=0", "")),
            MarkError::MissingKey("ping")
        );
        assert_eq!(
            broken(&V1.replace("side=2", "side=3")),
            MarkError::BadValue("side")
        );
        assert_eq!(
            broken(&V1.replace("lat=0", "lat=10")),
            MarkError::BadValue("lat")
        );
        assert_eq!(
            broken(&V1.replace("ping=0", "ping=4294967296")),
            MarkError::BadValue("ping")
        );
        assert_eq!(
            broken(&V1.replace("ping=0", "ping=%2B1")),
            MarkError::BadValue("ping")
        );
        assert_eq!(
            broken(&V1.replace("abort", "Abort")),
            MarkError::BadValue("result")
        );
        assert_eq!(broken(&V1.replace("T03", "T24")), MarkError::BadValue("at"));
        assert_eq!(
            broken(&V1.replace("peer=p", &format!("peer={}", "q".repeat(33)))),
            MarkError::BadValue("peer")
        );
        assert_eq!(
            broken(&V1.replace("self=s", "self=%FF")),
            MarkError::BadValue("self")
        );
        assert_eq!(
            broken(&V1.replace("peer=p", "peer=%G1")),
            MarkError::BadEscape("peer")
        );
        assert_eq!(
            broken(&V1.replace("peer=p", "peer=%4")),
            MarkError::BadEscape("peer")
        );
        assert_eq!(broken(&format!("{V1}\t")), MarkError::FieldWithoutEquals);
        assert_eq!(broken("HIGANX\t1"), MarkError::NotHiganr);
        assert_eq!(broken("HIGANR"), MarkError::MissingVersion);
        let edge = V1
            .replace("lat=0", "lat=9")
            .replace("ping=0", "ping=4294967295")
            .replace("2026-01-02", "2024-02-29");
        let BodyRead::Mark(m) = parse_mark_body(edge.as_bytes()).unwrap() else {
            panic!()
        };
        assert_eq!((m.lat, m.ping_ms, m.at.day), (9, u32::MAX, 29));
    }

    fn marked_with(mark: &Mark) -> Vec<u8> {
        let file = synthetic_replay(0xC8, 31);
        let mut marked = file.clone();
        marked.extend(chunk_to_append(&file, mark).unwrap());
        marked
    }

    fn detail_mark() -> Mark {
        Mark {
            detail: Some(sample_detail()),
            ..sample_mark("a b", "Z")
        }
    }

    #[test]
    fn the_match_detail_is_written_after_the_old_keys_in_the_documented_shape() {
        let m = detail_mark();
        let old = mark_body(&sample_mark("a b", "Z")).unwrap();
        let body = mark_body(&m).unwrap();
        let tail = "\tsync=rollback\tbuild=403920\tstart=1790000000\tn=3\tnet=31.120.2.40.90\
                    \tr1=3001.1.3.0.1.1.1.0.0.0.10.-\
                    \tr2=3002.2.3.0.2.1.1.0.0.0.20.-\
                    \tr3=3003.1.3.0.3.1.1.0.0.0.30.-";
        assert_eq!(body, format!("{old}{tail}"));
        assert_eq!(read_mark(&marked_with(&m)).unwrap(), MarkRead::Found(m));
    }

    #[test]
    fn a_three_round_mark_stays_small() {
        let m = detail_mark();
        let chunk = mark_chunk(&m).unwrap();
        let plain = mark_chunk(&sample_mark("a b", "Z")).unwrap();
        // 見本の名前は短い。実在の名前（各 32 バイトまで）が入っても 700 バイトに収まる。
        assert!(chunk.len() < 420, "{} バイト", chunk.len());
        let worst = Mark {
            peer: "x".repeat(32),
            self_name: "y".repeat(32),
            ..m
        };
        assert!(mark_chunk(&worst).unwrap().len() < 700);
        assert!(chunk.len() > plain.len());
    }

    #[test]
    fn a_reader_that_does_not_know_the_detail_keys_reads_the_rest() {
        // 足す前の読みは、マッチの情報のキーを除いた本文を読むことで模す。
        let m = detail_mark();
        let body = mark_body(&m).unwrap();
        let old: Vec<&str> = body
            .split('\t')
            .filter(|f| {
                !crate::replay_detail::ALLOWED_KEYS
                    .iter()
                    .any(|k| f.starts_with(&format!("{k}=")))
            })
            .collect();
        assert_eq!(old.join("\t"), mark_body(&sample_mark("a b", "Z")).unwrap());
        let BodyRead::Mark(back) = parse_mark_body(old.join("\t").as_bytes()).unwrap() else {
            panic!("足す前の本文が読めない");
        };
        assert_eq!(back, sample_mark("a b", "Z"));
        assert_eq!(back.detail, None);
        // 逆向き: 今の読みは足す前の印（V1）を読み、マッチの情報は無い。
        let BodyRead::Mark(v1) = parse_mark_body(V1.as_bytes()).unwrap() else {
            panic!("V1 が読めない");
        };
        assert_eq!(v1.detail, None);
    }

    #[test]
    fn detail_keys_may_come_in_any_order_and_with_lowercase_escapes() {
        let m = detail_mark();
        let body = mark_body(&m).unwrap();
        let (head, _) = body.split_once("\tsync=").unwrap();
        let ok = format!(
            "{head}\tr3=3003.1.3.0.3.1.1.0.0.0.30.-\tr1=3001.1.3.0.1.1.1.0.0.0.10.-\
             \tr2=3002.2.3.0.2.1.1.0.0.0.20.-\tbuild=%34%30%33%39%32%30\tn=3\tstart=1790000000\
             \tnet=31.120.2.40.90\tsync=rollback"
        );
        let BodyRead::Mark(back) = parse_mark_body(ok.as_bytes()).unwrap() else {
            panic!("読めない");
        };
        assert_eq!(back, m);
    }

    #[test]
    fn broken_detail_values_make_the_mark_broken_with_their_own_key() {
        let body = mark_body(&detail_mark()).unwrap();
        let broken = |from: &str, to: &str| match read_mark(&file_with_mark_bodies(&[body
            .replace(from, to)
            .as_bytes()]))
        {
            Ok(MarkRead::Broken(e)) => e,
            other => panic!("{from} → {to} が壊れているにならない: {other:?}"),
        };
        assert_eq!(
            broken("sync=rollback", "sync=lockstep"),
            MarkError::BadValue("sync")
        );
        assert_eq!(
            broken("build=403920", "build=x"),
            MarkError::BadValue("build")
        );
        assert_eq!(
            broken("start=1790000000", "start="),
            MarkError::BadValue("start")
        );
        assert_eq!(broken("n=3", "n=3.5"), MarkError::BadValue("n"));
        assert_eq!(broken("net=31.", "net=3a."), MarkError::BadValue("net"));
        assert_eq!(broken("r2=3002.2", "r2=3002.9"), MarkError::BadValue("r2"));
        assert_eq!(broken("\tr2=", "\tr9="), MarkError::BadValue("r3"));
        assert_eq!(broken("n=3", "n=%G1"), MarkError::BadEscape("n"));
        assert_eq!(broken("\tn=3", "\tn=3\tn=4"), MarkError::DuplicateKey("n"));
    }

    #[test]
    fn a_watcher_mark_carries_the_detail_without_the_net_part() {
        let m = Mark {
            detail: Some(MatchDetail {
                sync: None,
                match_no: None,
                comm: None,
                ..sample_detail()
            }),
            ..watch_mark("a", "b", "me")
        };
        let body = mark_body(&m).unwrap();
        for k in ["role=watch", "build=", "start=", "r1="] {
            assert!(body.contains(k), "{k}");
        }
        for k in ["sync=", "net=", "\tn="] {
            assert!(!body.contains(k), "{k}");
        }
        assert_eq!(read_mark(&marked_with(&m)).unwrap(), MarkRead::Found(m));
    }

    #[test]
    fn dropped_rounds_are_written_only_when_there_are_some() {
        let mut d = sample_detail();
        assert!(
            !mark_body(&Mark {
                detail: Some(d.clone()),
                ..sample_mark("p", "s")
            })
            .unwrap()
            .contains("rx=")
        );
        d.rounds_dropped = 2;
        let m = Mark {
            detail: Some(d),
            ..sample_mark("p", "s")
        };
        assert!(mark_body(&m).unwrap().contains("\trx=2\t"));
        assert_eq!(read_mark(&marked_with(&m)).unwrap(), MarkRead::Found(m));
    }

    #[test]
    fn too_many_rounds_are_refused_when_writing() {
        let mut d = sample_detail();
        d.rounds = vec![round(1, Winner::P1); 17];
        let m = Mark {
            detail: Some(d),
            ..sample_mark("p", "s")
        };
        assert_eq!(mark_chunk(&m), Err(MarkError::BadValue("rounds")));
    }

    /// 印に入る key は閉じた一覧だけ。IP・ポート・接続コード・ルーム名・接続 ID の欄は無い（足せばここで落ちる）。
    #[test]
    fn the_mark_has_only_the_listed_keys() {
        let mut keys: Vec<&str> = KEYS.to_vec();
        keys.extend(crate::replay_detail::ALLOWED_KEYS);
        let full = Mark {
            detail: Some(MatchDetail {
                rounds_dropped: 1,
                ..sample_detail()
            }),
            ..watch_mark("p1name", "p2name", "me")
        };
        let body = mark_body(&full).unwrap();
        let mut seen = 0;
        for f in body.split('\t').skip(2) {
            let key = f.split_once('=').map(|(k, _)| k).unwrap();
            assert!(keys.contains(&key), "一覧に無い key {key:?}");
            seen += 1;
        }
        // 必須 7 ＋ 観戦 3 ＋ sync/build/start/n/net/rx ＋ r1〜r3。
        assert_eq!(seen, 7 + 3 + 6 + 3);
        for secret in ["room", "conn", "code", "addr", "host"] {
            assert!(!keys.iter().any(|k| k.contains(secret)), "{secret}");
        }
        for secret in ["ip", "port", "sid", "cid", "session"] {
            assert!(!keys.contains(&secret), "{secret}");
        }
        // 名前の値に `r1=` のような形が入っても欄を増やせない（`=` と `\t` は逃がされる）。
        let sneaky = Mark {
            peer: "a\tr9=1".to_string(),
            ..sample_mark("p", "s")
        };
        let b = mark_body(&sneaky).unwrap();
        assert!(!b.contains("\tr9="), "{b}");
    }

    #[test]
    fn random_bytes_never_panic() {
        // 半分は T9RP と USER の形を持たせて奥まで届かせ、奥まで届いた本数が 0 でないことも見る（全部が入口で断られると測れていない）。
        let mut rng = XorShift(0x9E37_79B9);
        let mut parsed_ok = 0u32;
        let mut marks_seen = 0u32;
        let mut details_seen = 0u32;
        let detail_body = mark_body(&detail_mark()).unwrap();
        for i in 0..10_000u32 {
            let len = rng.below(400) as usize;
            let mut f: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if i % 2 == 0 {
                let comp = 0xC0 + rng.below(40);
                f = synthetic_replay(comp, rng.next() | 1);
                if rng.below(2) == 0 {
                    let body = if rng.below(2) == 0 {
                        V1.as_bytes()
                    } else {
                        detail_body.as_bytes()
                    };
                    let cut = rng.below(body.len() as u32 + 1) as usize;
                    let mut c = game_user_chunk(&body[..cut]);
                    c[8..12].copy_from_slice(b"HGRT");
                    f.extend(c);
                }
                for _ in 0..rng.below(4) {
                    let at = rng.below(f.len() as u32) as usize;
                    f[at] = rng.next() as u8;
                }
                if rng.below(4) == 0 {
                    let cut = rng.below(f.len() as u32 + 1) as usize;
                    f.truncate(cut);
                }
            }
            if parse(&f).is_ok() {
                parsed_ok += 1;
            }
            match read_mark(&f) {
                Ok(MarkRead::Found(m)) => {
                    marks_seen += 1;
                    details_seen += u32::from(m.detail.is_some());
                }
                Ok(MarkRead::Broken(_)) => marks_seen += 1,
                _ => {}
            }
            let _ = parse_mark_body(&f);
            let _ = chunk_to_append(&f, &sample_mark("p", "s"));
        }
        assert!(parsed_ok > 100, "parse が通った本数 {parsed_ok} / 10000");
        assert!(marks_seen > 100, "印まで届いた本数 {marks_seen} / 10000");
        assert!(
            details_seen > 20,
            "マッチの情報まで届いた本数 {details_seen} / 10000"
        );
    }
}
