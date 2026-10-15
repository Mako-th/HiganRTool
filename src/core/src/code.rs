//! 接続コード。接続を始める側が作り、相手へコピペ（かルームサーバー）で渡す。相手も自分のコードを返す。
//!
//! 中身: proto_ver・種別・session_id・名前・有効期限（UNIX 秒）・候補の列。**始める側のコードだけが秘密 16 B を持つ**。
//! 末尾は、始める側はコピーの間違いを見つける 4 B の検算（SHA-256 の先頭）、返す側は `HMAC-SHA256(K, 本文)` の
//! 先頭 16 B（K は秘密と session から導く鍵。第三者が返す側のコードを偽って候補を差し替えるのを防ぐ）。
//! 文字は自前の base64url（RFC 4648 §5、`=` なし）。中身の大半が乱数の秘密とアドレスなので圧縮はしない。
//!
//! ## バイトの配置（リトルエンディアン）
//!
//! | オフセット | 幅 | 中身 |
//! |---:|---:|---|
//! | 0 | 1 | proto_ver（`wire::PROTO_VER`。パケットのバージョンと同じ数） |
//! | 1 | 4 | build（ビルド番号 `update::BUILD_NUMBER`） |
//! | 5 | 1 | 種別（1 始める側 / 2 返す側 / 3 観戦用 / 4 観戦者の返し。観戦用は始める側と同じ形、観戦者の返しは返す側と同じ形） |
//! | 6 | 4 | session_id |
//! | 10 | 8 | 有効期限（UNIX 秒。この秒までは使える） |
//! | 18 | 1 | name_len（0〜32） |
//! | 19 | name_len | name（UTF-8） |
//! | … | 16 | 秘密（始める側・観戦用だけ） |
//! | … | 32 | ホストの観戦用の公開鍵（観戦用だけ） |
//! | … | 1 | 回線の型の印（`nat::NatMark`。下位 2 bit = シンメトリックか（0 不明 / 1 違う / 2 そう）、bit 2 = UPnP で割り当てた。残りは 0） |
//! | … | 1 | 候補の数（1〜`MAX_CANDIDATES`） |
//! | … | 各 8 か 20 | 候補: 種別 u8（`CandKind`）・family u8（4 / 6）・IP（4 か 16 B。ネットワークの順）・port u16 |
//! | 末尾 | 4 / 16 | 始める側・観戦用: SHA-256(ここまで) の先頭 4 B / 返す側・観戦者の返し: HMAC-SHA256(K, ここまで) の先頭 16 B |
//!
//! 読む順: 文字 → 長さ → バージョン → 種別 →（返す側: session_id）→ 検算かタグ → **ビルド** → 中身 → 長さの過不足 → 有効期限。
//! 検算かタグを中身より先に見るのはコピーの間違いを「候補のアドレスが変」と取り違えないため、ビルドを検算の後に見るのは
//! 「相手のバージョンが違う」と取り違えないため、有効期限を最後に見るのは壊れたコードを「期限切れ」と言わないため。
//! ビルドは自分の `update::BUILD_NUMBER` と比べる（0 と 0 は同じ）。
//!
//! コピペの読み書きでしか使わないので `String` / `Vec` を使う（ゲームスレッドでは呼ばない）。
//! ネットや人から来た文字を読むので、パニックしうる添字は置かない。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use sha2::{Digest, Sha256};

use crate::auth::{self, AuthKey, SECRET_LEN, Secret};
use crate::candidate::{CandError, CandKind, Candidate, Candidates, MAX_CANDIDATES};
use crate::handshake::Name;
use crate::lang::Lang;
use crate::nat::NatMark;
use crate::net_menu::{T_SPAN_MIN, T_SPAN_SEC};
use crate::seal::PUBLIC_LEN;
use crate::update::BUILD_NUMBER;
use crate::wire::{NAME_MAX, PROTO_VER};
/// 始める側の検算の長さ（B）。
pub const CHECK_LEN: usize = 4;
/// 返す側のタグの長さ（B。パケットのタグと同じ）。
pub const CODE_TAG_LEN: usize = auth::TAG_LEN;
/// 名前より前の決まった欄の長さ（バージョン 1・build 4・種別 1・session 4・期限 8・name_len 1）。
const HEAD_LEN: usize = 19;
/// 読む文字の上限（base64 で）。いちばん長いコード（観戦用・名前 32 B・IPv6 の候補 8 つ）は
/// 19 + 32 + 16 + 32 + 1 + 1 + 20 × 8 + 4 = 265 B = 354 文字。これを越える文字は中を読まずに断る。
pub const MAX_CODE_CHARS: usize = 512;
/// 読める文字の下限（base64 で）。いちばん短いコード（返す側・名前 0 B・IPv4 の候補 1 つ）は
/// 19 + 0 + 1 + 1 + 8 + 16 = 45 B = 60 文字。これより短い文字は、ウィンドウが接続コードとして置かずに断る
/// （対戦の形を選ぶ文字 `0` `delay` や打ち間違いを `peer_code.txt` へ置かない）。
pub const MIN_CODE_CHARS: usize = 60;
/// 有効期限の既定の長さ（秒）。コピペで渡して読むまでに切れず、古いコードが何日も使えないように 30 分。
pub const DEFAULT_TTL_SECS: u64 = 30 * 60;
/// 観戦者の返し（種別 4）の有効期限の長さ（秒）。コピペを挟まず配る側がすぐ読むので 5 分。
pub const SPECTATE_REPLY_TTL_SECS: u64 = 5 * 60;

/// 秒数を人が読む長さの文字にする（60 の倍数なら「30 分」、そうでなければ「45 秒」）。
/// ウィンドウの文の数字を定数から作り、`DEFAULT_TTL_SECS` を変えても古いままにならないようにする。
pub fn span_text(secs: u64) -> String {
    span_text_in(Lang::Ja, secs)
}

/// `span_text` の言語つきバージョン（英語は「30 min」「45 s」）。単位の文字は `net_menu` の表と同じ。
pub fn span_text_in(lang: Lang, secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}{}", secs / 60, T_SPAN_MIN.get(lang))
    } else {
        format!("{secs}{}", T_SPAN_SEC.get(lang))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// 接続を始める側（秘密を持つ。末尾は検算）。
    Initiator = 1,
    /// 返す側（秘密を持たない。末尾はタグ）。
    Responder = 2,
    /// 観戦用。形は始める側と同じ（秘密を持ち末尾は検算）だが、種別の 1 B で分けて対戦の答えや対戦のコードの読みには使えない
    /// （`WrongSide` / `Inconsistent`）。秘密と session は対戦のものと**別に**引く（同じだと観戦者が対戦の鍵を作れてしまう）。
    Spectate = 3,
    /// 観戦者の返し。インターネット越しの観戦で、観戦者が観戦用のコードへの answer としてルームサーバーへ預ける。
    /// 形は返す側と同じ（秘密なし、末尾は `HMAC-SHA256(K, 本文)` の先頭 16 B）で、K と session_id は観戦用のコードのもの。
    /// 中身は観戦者の名前と候補。対戦の返しとは読み合えない（`WrongSide`）。
    SpectateReply = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    pub side: Side,
    pub session_id: u32,
    /// 始める側だけ Some。
    pub secret: Option<Secret>,
    pub name: Name,
    /// 書いた側の回線の型の印（相手が、つながらなかったときの文を選ぶのに読む）。
    pub nat: NatMark,
    pub candidates: Candidates,
    /// 有効期限（UNIX 秒。この秒までは使える）。
    pub expires: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeError {
    /// base64url にない文字（`pos` は 0 からの文字の位置。前後の空白は除いた後）。
    BadChar { pos: usize },
    /// 文字の数が base64 として成り立たない（4 で割って 1 余る）か、`MAX_CODE_CHARS` を越える。
    BadLength,
    /// 最後の文字の使わないビットが 0 でない（同じ中身に 2 通りの文字を許さない）。
    NonCanonical,
    /// 決まった欄を読むのにバイトが足りない。
    TooShort,
    /// 候補まで読んだ後、末尾の検算かタグの前にバイトが余る・足りない。
    LengthMismatch { expected: usize, actual: usize },
    /// バージョンが `wire::PROTO_VER` でない（中はコードのバージョン）。読む順はバージョンが検算より先。
    BadVersion(u8),
    /// ビルド番号が自分と違う（中はコードの値）。検算・タグの後に見るのでコピーの間違いではない。
    OtherBuild { theirs: u32 },
    /// 種別が違う（始める側のコードを読むところに返す側のコード、など）か、知らない値。
    WrongSide(u8),
    /// 返す側のコードの session_id が、自分の始めた接続のものと違う。
    WrongSession { got: u32, want: u32 },
    /// 始める側の検算が合わない（コピーの間違い）。
    BadCheck,
    /// 返す側のタグが合わない（秘密を知らない人が作った・書き換えた）。
    BadTag,
    /// 名前が 32 B を越える・UTF-8 でない。
    BadName,
    /// 回線の型の印が定義の外（中はそのバイト）。
    BadNat(u8),
    /// 候補の数が 0（繋げない）。
    NoCandidates,
    /// 候補の数が `MAX_CANDIDATES` を越える。
    TooManyCandidates(u8),
    /// 候補の中身がおかしい（知らない種別・family、種別とアドレスの食い違い、ポート 0、重なり）。
    BadCandidate { index: u8, err: Option<CandError> },
    /// 有効期限を過ぎた。
    Expired { expires: u64, now: u64 },
    /// 書くとき: 種別と秘密の有無が合わない（始める側なのに秘密が無い、など）。
    Inconsistent,
}

// ---------------- base64url（RFC 4648 §5。`=` なし） ----------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn b64_char(v: u32) -> char {
    char::from(B64[(v & 63) as usize])
}

pub fn b64_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |&b| u32::from(b));
        let b2 = chunk.get(2).map_or(0, |&b| u32::from(b));
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(b64_char(n >> 18));
        s.push(b64_char(n >> 12));
        if chunk.len() > 1 {
            s.push(b64_char(n >> 6));
        }
        if chunk.len() > 2 {
            s.push(b64_char(n));
        }
    }
    s
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>, CodeError> {
    let t = text.as_bytes();
    if t.len() % 4 == 1 {
        return Err(CodeError::BadLength);
    }
    let mut out = Vec::with_capacity(t.len() * 3 / 4);
    for (ci, chunk) in t.chunks(4).enumerate() {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = b64_val(c).ok_or(CodeError::BadChar { pos: ci * 4 + i })?;
            n |= u32::from(v) << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let keep = chunk.len() - 1; // 2 文字 → 1 B、3 文字 → 2 B、4 文字 → 3 B
        // 最後の組の使わないビット（2 文字なら下 4 bit、3 文字なら下 2 bit）は 0 でなければならない。
        let unused = match chunk.len() {
            2 => n & 0xFFFF,
            3 => n & 0xFF,
            _ => 0,
        };
        if unused != 0 {
            return Err(CodeError::NonCanonical);
        }
        out.extend_from_slice(bytes.get(..keep).unwrap_or(&[]));
    }
    Ok(out)
}

// ---------------- バイト列 ----------------

fn put_candidate(v: &mut Vec<u8>, c: &Candidate) {
    v.push(c.kind.to_u8());
    match c.addr.ip() {
        IpAddr::V4(ip) => {
            v.push(4);
            v.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            v.push(6);
            v.extend_from_slice(&ip.octets());
        }
    }
    v.extend_from_slice(&c.addr.port().to_le_bytes());
}

/// 末尾（検算かタグ）の前までを作る。`build` はふつう `BUILD_NUMBER`、`host_public` は観戦用だけ
/// （他の種別は None。合わなければ `Inconsistent`）。
fn body_bytes(
    c: &Code,
    build: u32,
    host_public: Option<&[u8; PUBLIC_LEN]>,
) -> Result<Vec<u8>, CodeError> {
    let secret_ok = match c.side {
        Side::Initiator | Side::Spectate => c.secret.is_some(),
        Side::Responder | Side::SpectateReply => c.secret.is_none(),
    };
    if !secret_ok || (c.side == Side::Spectate) != host_public.is_some() {
        return Err(CodeError::Inconsistent);
    }
    if c.candidates.is_empty() {
        return Err(CodeError::NoCandidates);
    }
    let mut v = Vec::with_capacity(320);
    // コードのバージョンはパケットのバージョンと同じ数。
    v.push(PROTO_VER);
    v.extend_from_slice(&build.to_le_bytes());
    v.push(c.side as u8);
    v.extend_from_slice(&c.session_id.to_le_bytes());
    v.extend_from_slice(&c.expires.to_le_bytes());
    let name = c.name.as_bytes();
    v.push(name.len() as u8);
    v.extend_from_slice(name);
    if let Some(s) = c.secret {
        v.extend_from_slice(&s.0);
    }
    if let Some(p) = host_public {
        v.extend_from_slice(p);
    }
    v.push(c.nat.to_byte());
    v.push(c.candidates.len() as u8);
    for cand in c.candidates.as_slice() {
        put_candidate(&mut v, cand);
    }
    Ok(v)
}

fn check_of(body: &[u8]) -> [u8; CHECK_LEN] {
    let d = Sha256::digest(body);
    let mut out = [0u8; CHECK_LEN];
    for (o, s) in out.iter_mut().zip(d.iter()) {
        *o = *s;
    }
    out
}

fn tag_of(secret: &Secret, session_id: u32, body: &[u8]) -> [u8; CODE_TAG_LEN] {
    let key = AuthKey::derive(secret, session_id);
    let full = auth::hmac_sha256(key.k(), &[body]);
    let mut out = [0u8; CODE_TAG_LEN];
    for (o, s) in out.iter_mut().zip(full.iter()) {
        *o = *s;
    }
    out
}

/// 始める側のコードを書く（`c.side` は `Initiator`、`c.secret` は Some）。
pub fn write_initiator(c: &Code) -> Result<String, CodeError> {
    want_side(c, Side::Initiator)?;
    write_with_build(c, BUILD_NUMBER, None, None)
}

/// 書く関数と `c.side` が違えば書かない（取り違えを書く側で止める）。
fn want_side(c: &Code, want: Side) -> Result<(), CodeError> {
    if c.side == want {
        Ok(())
    } else {
        Err(CodeError::Inconsistent)
    }
}

/// 観戦用のコードを書く（`c.side` は `Spectate`、`c.secret` は Some）。形は始める側と同じで、
/// 種別とホストの公開鍵 `host_public` が違う。
pub fn write_spectate(c: &Code, host_public: &[u8; PUBLIC_LEN]) -> Result<String, CodeError> {
    want_side(c, Side::Spectate)?;
    write_with_build(c, BUILD_NUMBER, None, Some(host_public))
}

/// 返す側のコードを書く（`c.side` は `Responder`、`c.secret` は None）。`secret` は始める側のコードの秘密。
pub fn write_reply(c: &Code, secret: &Secret) -> Result<String, CodeError> {
    want_side(c, Side::Responder)?;
    write_with_build(c, BUILD_NUMBER, Some(secret), None)
}

/// 観戦者の返しを書く（`c.side` は `SpectateReply`、`c.secret` は None）。`secret` と `c.session_id` は
/// 観戦用のコードのもの。普通は [`write_spectate_reply_to`] を使う。
pub fn write_spectate_reply(c: &Code, secret: &Secret) -> Result<String, CodeError> {
    want_side(c, Side::SpectateReply)?;
    write_with_build(c, BUILD_NUMBER, Some(secret), None)
}

/// 任意の種別を `build` 指定で書く。`tag_secret` は返す側・観戦者の返しだけ、`host_public` は観戦用だけ。
/// **`BUILD_NUMBER` 以外の `build` は検査が別ビルドのコードを作るためだけ**。種別と引数が合わなければ `Inconsistent`。
#[doc(hidden)]
pub fn write_with_build(
    c: &Code,
    build: u32,
    tag_secret: Option<&Secret>,
    host_public: Option<&[u8; PUBLIC_LEN]>,
) -> Result<String, CodeError> {
    let mut v = body_bytes(c, build, host_public)?;
    match (c.side, tag_secret) {
        (Side::Initiator | Side::Spectate, None) => {
            let chk = check_of(&v);
            v.extend_from_slice(&chk);
        }
        // 秘密を持たず、末尾がタグの形（種別 2・4）。
        (Side::Responder | Side::SpectateReply, Some(secret)) => {
            let tag = tag_of(secret, c.session_id, &v);
            v.extend_from_slice(&tag);
        }
        _ => return Err(CodeError::Inconsistent),
    }
    Ok(b64_encode(&v))
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodeError> {
        let end = self.at.checked_add(n).ok_or(CodeError::TooShort)?;
        let s = self.b.get(self.at..end).ok_or(CodeError::TooShort)?;
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, CodeError> {
        self.take(1).map(|s| s[0])
    }
}

fn le_u32(s: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(s);
    u32::from_le_bytes(a)
}

/// 前後の空白と先頭の BOM を除いた文字。
fn bare(text: &str) -> &str {
    text.trim().trim_start_matches('\u{FEFF}').trim()
}

/// 文字 → バイト列。前後の空白（コピペで付きがち）と先頭の BOM（U+FEFF）を除く。
/// BOM はメモ帳で保存したファイルの先頭に付き、`str::trim` は空白と見ない。
fn text_bytes(text: &str) -> Result<Vec<u8>, CodeError> {
    let t = bare(text);
    if t.len() > MAX_CODE_CHARS {
        return Err(CodeError::BadLength);
    }
    b64_decode(t)
}

/// バージョンと種別まで（検算・タグの前に見る）。
fn head(b: &[u8], want: Side) -> Result<(), CodeError> {
    // 19 B が無ければどの欄も読めない。バージョンだけは 1 B あれば先に見る（バージョン違いの短いコードを「短い」でなく「バージョンが違う」と言うため）。
    if let Some(&v) = b.first()
        && v != PROTO_VER
    {
        return Err(CodeError::BadVersion(v));
    }
    if b.len() < HEAD_LEN {
        return Err(CodeError::TooShort);
    }
    if b[5] != want as u8 {
        return Err(CodeError::WrongSide(b[5]));
    }
    Ok(())
}

fn same_build(body: &[u8]) -> Result<(), CodeError> {
    let theirs = le_u32(body.get(1..5).ok_or(CodeError::TooShort)?);
    if theirs != BUILD_NUMBER {
        return Err(CodeError::OtherBuild { theirs });
    }
    Ok(())
}

/// 末尾を外した本文の中身を読む。観戦用は、秘密の後ろのホストの公開鍵も返す。
fn parse_body(
    body: &[u8],
    side: Side,
    now: u64,
) -> Result<(Code, Option<[u8; PUBLIC_LEN]>), CodeError> {
    let mut r = Reader { b: body, at: 6 };
    let session_id = le_u32(r.take(4)?);
    let mut e = [0u8; 8];
    e.copy_from_slice(r.take(8)?);
    let expires = u64::from_le_bytes(e);
    let name_len = usize::from(r.u8()?);
    if name_len > NAME_MAX {
        return Err(CodeError::BadName);
    }
    let name_bytes = r.take(name_len)?;
    let name_str = std::str::from_utf8(name_bytes).map_err(|_| CodeError::BadName)?;
    let name = Name::new(name_str);
    let secret = match side {
        Side::Initiator | Side::Spectate => {
            let mut s = [0u8; SECRET_LEN];
            s.copy_from_slice(r.take(SECRET_LEN)?);
            Some(Secret(s))
        }
        Side::Responder | Side::SpectateReply => None,
    };
    let host_public = match side {
        Side::Spectate => {
            let mut p = [0u8; PUBLIC_LEN];
            p.copy_from_slice(r.take(PUBLIC_LEN)?);
            Some(p)
        }
        Side::Initiator | Side::Responder | Side::SpectateReply => None,
    };
    let nat_byte = r.u8()?;
    let nat = NatMark::from_byte(nat_byte).ok_or(CodeError::BadNat(nat_byte))?;
    let count = r.u8()?;
    if count == 0 {
        return Err(CodeError::NoCandidates);
    }
    if usize::from(count) > MAX_CANDIDATES {
        return Err(CodeError::TooManyCandidates(count));
    }
    let mut candidates = Candidates::EMPTY;
    for index in 0..count {
        let bad = |err| CodeError::BadCandidate { index, err };
        let kind = CandKind::from_u8(r.u8()?).ok_or(bad(None))?;
        let ip = match r.u8()? {
            4 => {
                let mut o = [0u8; 4];
                o.copy_from_slice(r.take(4)?);
                IpAddr::V4(Ipv4Addr::from(o))
            }
            6 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(r.take(16)?);
                IpAddr::V6(Ipv6Addr::from(o))
            }
            _ => return Err(bad(None)),
        };
        let p = r.take(2)?;
        let port = u16::from_le_bytes([p[0], p[1]]);
        let c = Candidate::new(kind, SocketAddr::new(ip, port)).map_err(|e| bad(Some(e)))?;
        candidates.push(c).map_err(|e| bad(Some(e)))?;
    }
    if r.at != body.len() {
        return Err(CodeError::LengthMismatch {
            expected: r.at,
            actual: body.len(),
        });
    }
    if now > expires {
        return Err(CodeError::Expired { expires, now });
    }
    Ok((
        Code {
            side,
            session_id,
            secret,
            name,
            nat,
            candidates,
            expires,
        },
        host_public,
    ))
}

/// 観戦用のコードの中身と、ホストの観戦用の公開鍵。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpectateCode {
    pub code: Code,
    pub host_public: [u8; PUBLIC_LEN],
}

/// 始める側のコードを読む（`now` は今の UNIX 秒）。
pub fn read_initiator(text: &str, now: u64) -> Result<Code, CodeError> {
    read_checked(text, Side::Initiator, now).map(|(c, _)| c)
}

/// 観戦用のコードを読む（`now` は今の UNIX 秒）。対戦の始める側のコードは `WrongSide(1)` で断る。
pub fn read_spectate(text: &str, now: u64) -> Result<SpectateCode, CodeError> {
    let (code, host_public) = read_checked(text, Side::Spectate, now)?;
    let host_public = host_public.ok_or(CodeError::Inconsistent)?;
    Ok(SpectateCode { code, host_public })
}

fn read_checked(
    text: &str,
    side: Side,
    now: u64,
) -> Result<(Code, Option<[u8; PUBLIC_LEN]>), CodeError> {
    let b = text_bytes(text)?;
    head(&b, side)?;
    let Some(split) = b.len().checked_sub(CHECK_LEN) else {
        return Err(CodeError::TooShort);
    };
    let (body, chk) = b.split_at(split);
    // 検算はコピーの間違いを見つけるためのもので、秘密を守るものではない（秘密はコードの中にある）。
    if check_of(body) != chk {
        return Err(CodeError::BadCheck);
    }
    same_build(body)?;
    parse_body(body, side, now)
}

/// 返す側のコードを読む。`secret` と `session_id` は自分が始めた接続のもの（自分の始める側のコードの中身）。
pub fn read_reply(
    text: &str,
    secret: &Secret,
    session_id: u32,
    now: u64,
) -> Result<Code, CodeError> {
    read_tagged(text, secret, session_id, now, Side::Responder)
}

/// 観戦者の返しを読む。`secret` と `session_id` は配る側が出した観戦用のコードのもの。読む順は `read_reply` と同じ。
pub fn read_spectate_reply(
    text: &str,
    secret: &Secret,
    session_id: u32,
    now: u64,
) -> Result<Code, CodeError> {
    read_tagged(text, secret, session_id, now, Side::SpectateReply)
}

fn read_tagged(
    text: &str,
    secret: &Secret,
    session_id: u32,
    now: u64,
    side: Side,
) -> Result<Code, CodeError> {
    let b = text_bytes(text)?;
    head(&b, side)?;
    let got = le_u32(b.get(6..10).ok_or(CodeError::TooShort)?);
    if got != session_id {
        return Err(CodeError::WrongSession {
            got,
            want: session_id,
        });
    }
    let Some(split) = b.len().checked_sub(CODE_TAG_LEN) else {
        return Err(CodeError::TooShort);
    };
    let (body, tag) = b.split_at(split);
    // タグの比べは定数時間で。
    let key = AuthKey::derive(secret, session_id);
    if !code_tag_ok(&key, body, tag) {
        return Err(CodeError::BadTag);
    }
    same_build(body)?;
    parse_body(body, side, now).map(|(c, _)| c)
}

fn code_tag_ok(key: &AuthKey, body: &[u8], tag: &[u8]) -> bool {
    // K を鍵にした HMAC の先頭 16 B。パケットのタグ（`AuthKey::verify`）と同じ式なので、同じ K・本文なら同じ値になる。
    key.verify(body, tag)
}

// ---------------- 相手のコードのファイル ----------------
//
// dll は接続コードを DLL のフォルダの `code\` のファイルで受け渡し、人が相手のコードを `peer_code.txt` に貼る。
// ここは貼られた文字を自分の側から読むところで、取り違え（自分のコードを貼った・2 人とも同じ側を選んだ）を
// 文字の誤りと分けて返す。

#[derive(Debug, Clone, Copy)]
pub enum Mine<'a> {
    /// 自分が始める側（offer）。自分のコードの中身（返す側のタグを確かめる）と文字（自分のコードを貼った取り違えを見分ける）。
    Offer { own: &'a Code, own_text: &'a str },
    /// 自分が返す側（answer）。返す側のコードをもう書いたならその文字。
    Answer { own_text: Option<&'a str> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerError {
    /// 空白と BOM しか無い（まだ貼っていない）。
    Empty,
    /// 自分のコードをそのまま貼った（相手のコードでない）。
    OwnCode,
    /// 2 人とも始める側（相手のファイルも始める側のコードで、検算まで合う）。片方が返す側に直す。
    BothOffer,
    /// 2 人とも返す側（相手のファイルも返す側のコード）。片方が始める側に直す。
    BothAnswer,
    /// そのほか（文字・長さ・検算・タグ・session・期限など。`CodeError` の理由のまま）。
    Code(CodeError),
}

/// 相手のファイルの文字 `text` を、自分の側 `mine` から読む（`now` は今の UNIX 秒）。
/// 始める側は返す側のコードを、返す側は始める側のコードを読む。
///
/// 「両方が始める側」は、種別 1 が来て始める側のコードとして検算まで合うときだけ言う（期限切れも含める。
/// 種別の取り違えの方が先に直すこと）。検算が合わない種別 1 はコピーの間違いと見て `Code(WrongSide(1))` のまま返す。
/// 返す側のコードは秘密が無いと確かめられないので、「両方が返す側」は種別 2 を見ただけで言う。
pub fn read_peer(text: &str, mine: Mine<'_>, now: u64) -> Result<Code, PeerError> {
    let t = bare(text);
    if t.is_empty() {
        return Err(PeerError::Empty);
    }
    match mine {
        Mine::Offer { own, own_text } => {
            if t == bare(own_text) {
                return Err(PeerError::OwnCode);
            }
            let Some(secret) = own.secret else {
                return Err(PeerError::Code(CodeError::Inconsistent));
            };
            match read_reply(t, &secret, own.session_id, now) {
                Err(CodeError::WrongSide(s)) if s == Side::Initiator as u8 => {
                    match read_initiator(t, now) {
                        Ok(_) | Err(CodeError::Expired { .. } | CodeError::OtherBuild { .. }) => {
                            Err(PeerError::BothOffer)
                        }
                        Err(_) => Err(PeerError::Code(CodeError::WrongSide(s))),
                    }
                }
                r => r.map_err(PeerError::Code),
            }
        }
        Mine::Answer { own_text } => {
            if own_text.is_some_and(|o| t == bare(o)) {
                return Err(PeerError::OwnCode);
            }
            match read_initiator(t, now) {
                Err(CodeError::WrongSide(s)) if s == Side::Responder as u8 => {
                    Err(PeerError::BothAnswer)
                }
                r => r.map_err(PeerError::Code),
            }
        }
    }
}

/// 始める側のコード `init` への返す側のコードを書く。session は `init` のもの、タグの鍵は `init` の秘密から。
/// 有効期限は `now` から `DEFAULT_TTL_SECS`。
pub fn write_reply_to(
    init: &Code,
    name: Name,
    nat: NatMark,
    candidates: Candidates,
    now: u64,
) -> Result<String, CodeError> {
    let (Side::Initiator, Some(secret)) = (init.side, init.secret) else {
        return Err(CodeError::Inconsistent);
    };
    write_reply(
        &Code {
            side: Side::Responder,
            session_id: init.session_id,
            secret: None,
            name,
            nat,
            candidates,
            expires: now.saturating_add(DEFAULT_TTL_SECS),
        },
        &secret,
    )
}

/// 観戦用のコード `spec` への、観戦者の返し（種別 4）を書く。session とタグの鍵は `spec` のもの。
/// 有効期限は `now` から [`SPECTATE_REPLY_TTL_SECS`]。
pub fn write_spectate_reply_to(
    spec: &Code,
    name: Name,
    candidates: Candidates,
    now: u64,
) -> Result<String, CodeError> {
    let (Side::Spectate, Some(secret)) = (spec.side, spec.secret) else {
        return Err(CodeError::Inconsistent);
    };
    write_spectate_reply(
        &Code {
            side: Side::SpectateReply,
            session_id: spec.session_id,
            secret: None,
            name,
            nat: NatMark::UNKNOWN,
            candidates,
            expires: now.saturating_add(SPECTATE_REPLY_TTL_SECS),
        },
        &secret,
    )
}

#[cfg(test)]
mod tests {
    //! 期待値は crate の外（Python 3 の `hashlib` / `hmac` / `base64`）で計算した。
    use super::*;
    use crate::nat::Symmetry;

    fn secret() -> Secret {
        crate::auth::tests::golden_secret()
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn span_text_writes_minutes_only_for_whole_minutes() {
        assert_eq!(span_text(30 * 60), "30分");
        assert_eq!(span_text(45 * 60), "45分");
        assert_eq!(span_text(60), "1分");
        assert_eq!(span_text(90), "90秒");
        assert_eq!(span_text(59), "59秒");
        assert_eq!(span_text(1), "1秒");
        assert_eq!(span_text(0), "0秒");
    }

    #[test]
    fn span_text_in_english_uses_min_and_s() {
        for (secs, en) in [
            (1800, "30 min"),
            (60, "1 min"),
            (90, "90 s"),
            (59, "59 s"),
            (0, "0 s"),
        ] {
            assert_eq!(span_text_in(Lang::En, secs), en);
            assert_eq!(span_text_in(Lang::Ja, secs), span_text(secs));
        }
    }

    const EXPIRES: u64 = 1_800_000_000;
    const SID: u32 = 0x1122_3344;

    /// 始める側の golden の本文（67 B）。build は開発用の 0（`HIGANR_BUILD_NUMBER` を付けて検査を回すと、
    /// golden の読みはビルド違いで落ちる）。
    #[rustfmt::skip]
    const INIT_BODY: [u8; 67] = [
        0x08,                   // バージョン 8（`PROTO_VER`）
        0x00, 0x00, 0x00, 0x00, // build 0（開発用）
        0x01,                   // 始める側
        0x44, 0x33, 0x22, 0x11, // session 0x11223344
        0x00, 0xD2, 0x49, 0x6B, 0x00, 0x00, 0x00, 0x00, // 期限 1800000000 = 0x6B49D200
        0x02, b'a', b'b',       // 名前 "ab"
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, // 秘密
        0x06,                   // 回線の型の印: シンメトリック（bit 1）・UPnP で割り当て済み（bit 2）
        0x02,                   // 候補 2 つ
        0x01, 0x04, 127, 0, 0, 1, 0x99, 0xB7, // loopback 127.0.0.1:47001
        0x04, 0x06, 0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x58, 0x1B, // IPv6 [2001:db8::1]:7000
    ];
    /// 検算（Python: `hashlib.sha256(INIT_BODY).digest()[:4]`）。
    const INIT_CHECK: [u8; 4] = [0xB2, 0xFB, 0x0F, 0xBB];
    /// 文字（Python: `base64.urlsafe_b64encode(INIT_BODY + INIT_CHECK).rstrip(b"=")`）。
    const INIT_TEXT: &str = "CAAAAAABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwYCAQR_AAABmbcEBiABDbgAAAAAAAAAAAAAAAFYG7L7D7s";
    /// バージョン 7 の始める側のコードの文字（形はバージョン 8 と同じで、先頭の 1 B と検算だけが違う）。
    const INIT_TEXT_V7: &str = "BwAAAAABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwYCAQR_AAABmbcEBiABDbgAAAAAAAAAAAAAAAFYG6w7Or0";
    /// バージョン 6 の始める側のコードの文字（形はバージョン 8 と同じで、先頭の 1 B と検算だけが違う）。
    const INIT_TEXT_V6: &str = "BgAAAAABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwYCAQR_AAABmbcEBiABDbgAAAAAAAAAAAAAAAFYG7UpLvs";
    /// バージョン 5 の始める側のコードの文字（回線の型の印の 1 B が無い形）。
    const INIT_TEXT_V5: &str = "BQAAAAABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwIBBH8AAAGZtwQGIAENuAAAAAAAAAAAAAAAAVgb7NLang";
    /// バージョン 4 の始める側のコードの文字（build の 4 B が無い形）。
    const INIT_TEXT_V4: &str =
        "BAFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBtTz18H";
    /// ビルド 403920 の始める側のコードの文字（`INIT_BODY` の build だけを変えて検算を付け直した）。
    const INIT_TEXT_B403920: &str = "CNApBgABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwYCAQR_AAABmbcEBiABDbgAAAAAAAAAAAAAAAFYG3mwXng";
    /// バージョン 3 の始める側のコードの文字。
    const INIT_TEXT_V3: &str =
        "AwFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBtQIsU3";
    /// バージョン 2 の始める側のコードの文字。
    const INIT_TEXT_V2: &str =
        "AgFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBv0WQDS";
    /// バージョン 1 の始める側のコードの文字。
    const INIT_TEXT_V1: &str =
        "AQFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBs_wJYi";
    /// バージョン 0 の始める側のコードの文字。
    const INIT_TEXT_V0: &str =
        "AAFEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBsEJ6ZI";

    /// 返す側の golden の本文（31 B）。
    #[rustfmt::skip]
    const REPLY_BODY: [u8; 31] = [
        0x08, 0x00, 0x00, 0x00, 0x00, 0x02, // バージョン 8・build 0・返す側
        0x44, 0x33, 0x22, 0x11,
        0x00, 0xD2, 0x49, 0x6B, 0x00, 0x00, 0x00, 0x00,
        0x02, b'c', b'd',
        0x01,                   // 回線の型の印: シンメトリックでない・UPnP なし
        0x01,
        0x02, 0x04, 192, 168, 0, 2, 0x59, 0x1B, // host 192.168.0.2:7001
    ];
    /// タグ（Python: `hmac.new(K, REPLY_BODY, sha256).digest()[:16]`。K は `auth.rs` の `K_GOLDEN`）。
    const REPLY_TAG: [u8; 16] = [
        0xE0, 0x9B, 0x07, 0xA7, 0x2E, 0x34, 0x8E, 0x3D, 0xF7, 0x78, 0xBC, 0xAD, 0x91, 0xE1, 0xA6,
        0x2C,
    ];
    const REPLY_TEXT: &str = "CAAAAAACRDMiEQDSSWsAAAAAAmNkAQECBMCoAAJZG-CbB6cuNI4993i8rZHhpiw";
    /// バージョン 7 の返す側のコードの文字。
    const REPLY_TEXT_V7: &str = "BwAAAAACRDMiEQDSSWsAAAAAAmNkAQECBMCoAAJZG6ShM8nFqtf1BSjgkXrB2Y8";
    /// バージョン 6 の返す側のコードの文字。
    const REPLY_TEXT_V6: &str = "BgAAAAACRDMiEQDSSWsAAAAAAmNkAQECBMCoAAJZG6EIdoTV8ABRMEBjMpbG2KU";
    /// バージョン 5 の返す側のコードの文字。
    const REPLY_TEXT_V5: &str = "BQAAAAACRDMiEQDSSWsAAAAAAmNkAQIEwKgAAlkbNIuZlsf0uRw7CxUvTKwQuA";
    /// バージョン 4 の返す側のコードの文字。
    const REPLY_TEXT_V4: &str = "BAJEMyIRANJJawAAAAACY2QBAgTAqAACWRvhJKMiyeMasvTBDRmqSi4J";
    /// バージョン 3 の返す側のコードの文字。
    const REPLY_TEXT_V3: &str = "AwJEMyIRANJJawAAAAACY2QBAgTAqAACWRtancCpZeKsY2fXVx9TwLSe";
    /// バージョン 2 の返す側のコードの文字。
    const REPLY_TEXT_V2: &str = "AgJEMyIRANJJawAAAAACY2QBAgTAqAACWRuwURwTi--NFss_V-Skc_9p";
    /// バージョン 1 の返す側のコードの文字。
    const REPLY_TEXT_V1: &str = "AQJEMyIRANJJawAAAAACY2QBAgTAqAACWRv1ZZSmOZlbq3_AOVTJo3x7";
    /// バージョン 0 の返す側のコードの文字。
    const REPLY_TEXT_V0: &str = "AAJEMyIRANJJawAAAAACY2QBAgTAqAACWRsWcDWIpgqixSIkh5agOXfx";

    fn init_code() -> Code {
        let mut c = Candidates::EMPTY;
        c.push(Candidate::new(CandKind::Loopback, sa("127.0.0.1:47001")).unwrap())
            .unwrap();
        c.push(Candidate::new(CandKind::Ipv6, sa("[2001:db8::1]:7000")).unwrap())
            .unwrap();
        Code {
            side: Side::Initiator,
            session_id: SID,
            secret: Some(secret()),
            name: Name::new("ab"),
            nat: NatMark {
                symmetry: Symmetry::Yes,
                upnp: true,
            },
            candidates: c,
            expires: EXPIRES,
        }
    }

    fn reply_code() -> Code {
        Code {
            side: Side::Responder,
            session_id: SID,
            secret: None,
            name: Name::new("cd"),
            nat: NatMark {
                symmetry: Symmetry::No,
                upnp: false,
            },
            candidates: Candidates::from_addrs(&[sa("192.168.0.2:7001")]).unwrap(),
            expires: EXPIRES,
        }
    }

    #[test]
    fn the_shortest_code_has_min_code_chars() {
        let reply = write_reply(
            &Code {
                name: Name::new(""),
                ..reply_code()
            },
            &secret(),
        )
        .unwrap();
        assert_eq!(reply.len(), MIN_CODE_CHARS, "{reply}");
        for side in [Side::Initiator, Side::Spectate] {
            let c = Code {
                side,
                secret: Some(secret()),
                name: Name::new(""),
                ..reply_code()
            };
            let text = if side == Side::Initiator {
                write_initiator(&c)
            } else {
                write_spectate(&c, &HOST_PUBLIC)
            }
            .unwrap();
            assert!(text.len() > MIN_CODE_CHARS, "{side:?}: {text}");
        }
    }

    #[test]
    fn base64url_matches_rfc4648_samples() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
            (&[0xFB, 0xFF], "-_8"),
        ] {
            assert_eq!(b64_encode(raw), text);
            assert_eq!(b64_decode(text).unwrap(), raw, "{text}");
        }
        assert_eq!(b64_decode("Zg=="), Err(CodeError::BadChar { pos: 2 }));
        assert_eq!(b64_decode("+_8"), Err(CodeError::BadChar { pos: 0 }));
        assert_eq!(b64_decode("-/8"), Err(CodeError::BadChar { pos: 1 }));
        assert_eq!(b64_decode("Zm9 v"), Err(CodeError::BadLength));
        assert_eq!(b64_decode("Zm9vY"), Err(CodeError::BadLength));
        assert_eq!(b64_decode("Zh"), Err(CodeError::NonCanonical)); // "Zg" と同じ 1 B
        assert_eq!(b64_decode("Zm9"), Err(CodeError::NonCanonical)); // "Zm8" と同じ 2 B
    }

    #[test]
    fn golden_codes_match_hand_written_bytes() {
        let mut want = INIT_BODY.to_vec();
        want.extend_from_slice(&INIT_CHECK);
        assert_eq!(b64_encode(&want), INIT_TEXT, "表から手で書いた列の文字列");
        assert_eq!(write_initiator(&init_code()).unwrap(), INIT_TEXT);
        assert_eq!(read_initiator(INIT_TEXT, EXPIRES), Ok(init_code()));

        let mut want = REPLY_BODY.to_vec();
        want.extend_from_slice(&REPLY_TAG);
        assert_eq!(b64_encode(&want), REPLY_TEXT);
        assert_eq!(write_reply(&reply_code(), &secret()).unwrap(), REPLY_TEXT);
        assert_eq!(
            read_reply(REPLY_TEXT, &secret(), SID, EXPIRES),
            Ok(reply_code())
        );
        assert_eq!(
            read_initiator(&format!("  {INIT_TEXT}\r\n"), 0),
            Ok(init_code())
        );
    }

    /// IPv6 を入れた自分の候補（世界に通じる IPv6 ＋ LAN ＋ 外側 IPv4）の始める側のコードの本文（75 B）。
    /// 候補の並びは優先の順（IPv6 → LAN → 外側）。
    #[rustfmt::skip]
    const DUAL_BODY: [u8; 75] = [
        0x08, 0x00, 0x00, 0x00, 0x00, // バージョン 8・build 0
        0x01, 0x44, 0x33, 0x22, 0x11, 0x00, 0xD2, 0x49, 0x6B, 0x00, 0x00, 0x00, 0x00, // 始める側・session・期限
        0x02, 0x61, 0x62,                                                                   // 名前 "ab"
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, // 秘密
        0x06,                                                                               // 回線の型の印
        0x03,                                                                               // 候補 3 つ
        0x04, 0x06, 0x24, 0x00, 0xCB, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x99, 0xB7,   // 種別 4・family 6・[2400:cb00::7]:47001
        0x02, 0x04, 0xC0, 0xA8, 0x00, 0x02, 0x99, 0xB7,                                     // 種別 2・192.168.0.2:47001
        0x03, 0x04, 0xCB, 0x00, 0x71, 0x05, 0x99, 0xB7,                                     // 種別 3・203.0.113.5:47001
    ];
    /// 検算（Python: `hashlib.sha256(DUAL_BODY).digest()[:4]`）。
    const DUAL_CHECK: [u8; 4] = [0x76, 0xCF, 0x51, 0x57];
    /// 文字（Python: `base64.urlsafe_b64encode(DUAL_BODY + DUAL_CHECK).rstrip(b"=")`）。
    const DUAL_TEXT: &str = "CAAAAAABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwYDBAYkAMsAAAAAAAAAAAAAAAAHmbcCBMCoAAKZtwMEywBxBZm3ds9RVw";

    #[test]
    fn a_code_with_a_global_ipv6_candidate_keeps_the_version_2_shape_and_round_trips() {
        let own = |extra: &[&str]| {
            let mut c = Candidates::EMPTY;
            // 並べる順をわざと優先の逆にする（`Candidates` が優先の順に直す）。
            for a in extra {
                c.push(Candidate::guess(sa(a)).unwrap()).unwrap();
            }
            c
        };
        let candidates = own(&[
            "203.0.113.5:47001",
            "192.168.0.2:47001",
            "[2400:cb00::7]:47001",
        ]);
        assert_eq!(
            candidates
                .as_slice()
                .iter()
                .map(|c| c.kind)
                .collect::<Vec<_>>(),
            [CandKind::Ipv6, CandKind::Host, CandKind::External]
        );
        let code = Code {
            candidates,
            ..init_code()
        };
        let mut want = DUAL_BODY.to_vec();
        want.extend_from_slice(&DUAL_CHECK);
        assert_eq!(b64_encode(&want), DUAL_TEXT, "表から手で組んだ列の文字列");
        assert_eq!(write_initiator(&code).unwrap(), DUAL_TEXT);
        let read = read_initiator(DUAL_TEXT, EXPIRES).unwrap();
        assert_eq!(read, code);
        assert_eq!(
            read.candidates.as_slice()[0].addr,
            sa("[2400:cb00::7]:47001")
        );

        let reply = Code {
            candidates: own(&["[2400:cb00::7]:47001"]),
            ..reply_code()
        };
        let text = write_reply(&reply, &secret()).unwrap();
        assert_eq!(read_reply(&text, &secret(), SID, EXPIRES), Ok(reply));
    }

    #[test]
    fn a_leading_bom_is_ignored() {
        assert_eq!(
            read_initiator(&format!("\u{FEFF}{INIT_TEXT}\r\n"), 0),
            Ok(init_code())
        );
        assert_eq!(
            read_reply(&format!("\u{FEFF}{REPLY_TEXT}"), &secret(), SID, EXPIRES),
            Ok(reply_code())
        );
        // 途中の BOM（UTF-8 で 3 B）。文字の数が 4 で割って 1 余るので、長さのガードが文字のガードより先に落とす。
        assert!(matches!(
            read_initiator(
                &format!("{}\u{FEFF}{}", &INIT_TEXT[..8], &INIT_TEXT[8..]),
                0
            ),
            Err(CodeError::BadChar { pos: 8 } | CodeError::BadLength)
        ));
        // 末尾の BOM もコピーの間違いの文字（先頭だけを許す）。
        assert!(matches!(
            read_initiator(&format!("{INIT_TEXT}\u{FEFF}"), 0),
            Err(CodeError::BadChar { .. } | CodeError::BadLength)
        ));
        // 長さのガードに頼らない形（観戦用で文字の数が 4 の倍数のもの + BOM の 3 B）でも、末尾の BOM は文字のガードで落ちる。
        let aligned = write_spectate(
            &Code {
                name: Name::new("abcd"),
                ..spectate_code()
            },
            &HOST_PUBLIC,
        )
        .unwrap();
        assert_eq!(aligned.len() % 4, 0);
        assert!(matches!(
            read_spectate(&format!("{aligned}\u{FEFF}"), EXPIRES),
            Err(CodeError::BadChar { .. })
        ));
    }

    #[test]
    fn line_breaks_only_around_the_code_are_ignored() {
        for t in [
            format!("{INIT_TEXT}\n"),
            format!("{INIT_TEXT}\r\n"),
            format!("\r\n\r\n{INIT_TEXT}\r\n\r\n"),
            format!("\u{FEFF}\r\n{INIT_TEXT}"),
        ] {
            assert_eq!(read_initiator(&t, 0), Ok(init_code()), "{t:?}");
        }
        for t in [
            format!("{}\n{}", &INIT_TEXT[..40], &INIT_TEXT[40..]),
            format!("{}\r\n{}", &INIT_TEXT[..40], &INIT_TEXT[40..]),
        ] {
            // LF 1 文字なら `BadLength`、CRLF なら `BadChar`。
            assert!(
                matches!(
                    read_initiator(&t, 0),
                    Err(CodeError::BadChar { pos: 40 } | CodeError::BadLength)
                ),
                "{t:?}"
            );
        }
    }

    /// 本文と検算をつなげて末尾の `n` B を落とした文字（文字の切り口が正準のまま、検算だけが合わなくなる）。
    fn without_last_bytes(body: &[u8], check: &[u8], n: usize) -> String {
        let mut v = body.to_vec();
        v.extend_from_slice(check);
        v.truncate(v.len() - n);
        b64_encode(&v)
    }

    fn mine_offer() -> Mine<'static> {
        static OWN: std::sync::OnceLock<Code> = std::sync::OnceLock::new();
        Mine::Offer {
            own: OWN.get_or_init(init_code),
            own_text: INIT_TEXT,
        }
    }

    /// golden と別の始める側のコード（別の秘密・session）。
    fn other_init_text(expires: u64) -> String {
        let mut c = init_code();
        c.session_id = SID ^ 0xFF;
        let mut s = secret();
        s.0[3] ^= 0x55;
        c.secret = Some(s);
        c.expires = expires;
        write_initiator(&c).unwrap()
    }

    #[test]
    fn peer_file_reads_tell_mixups_from_broken_codes() {
        let now = EXPIRES;
        assert_eq!(read_peer(REPLY_TEXT, mine_offer(), now), Ok(reply_code()));
        assert_eq!(
            read_peer(&format!("\u{FEFF}{REPLY_TEXT}\r\n"), mine_offer(), now),
            Ok(reply_code())
        );
        let answer = Mine::Answer { own_text: None };
        assert_eq!(read_peer(INIT_TEXT, answer, now), Ok(init_code()));
        for t in ["", "  \r\n", "\u{FEFF}", "\u{FEFF}\r\n"] {
            assert_eq!(
                read_peer(t, mine_offer(), now),
                Err(PeerError::Empty),
                "{t:?}"
            );
            assert_eq!(read_peer(t, answer, now), Err(PeerError::Empty), "{t:?}");
        }
        assert_eq!(
            read_peer(&format!("{INIT_TEXT}\r\n"), mine_offer(), now),
            Err(PeerError::OwnCode)
        );
        assert_eq!(
            read_peer(
                REPLY_TEXT,
                Mine::Answer {
                    own_text: Some(REPLY_TEXT)
                },
                now
            ),
            Err(PeerError::OwnCode)
        );
        assert_eq!(
            read_peer(&other_init_text(EXPIRES), mine_offer(), now),
            Err(PeerError::BothOffer)
        );
        assert_eq!(
            read_peer(&other_init_text(EXPIRES - 1), mine_offer(), now),
            Err(PeerError::BothOffer)
        );
        // 種別 1 だが検算が合わない（コピーの間違い）: 言い切らない。
        let mut v = INIT_BODY.to_vec();
        v[24] ^= 0x04;
        v.extend_from_slice(&INIT_CHECK);
        assert_eq!(
            read_peer(&b64_encode(&v), mine_offer(), now),
            Err(PeerError::Code(CodeError::WrongSide(1)))
        );
        assert_eq!(
            read_peer(REPLY_TEXT, answer, now),
            Err(PeerError::BothAnswer)
        );
        assert!(matches!(
            read_peer(REPLY_TEXT, mine_offer(), EXPIRES + 1),
            Err(PeerError::Code(CodeError::Expired { .. }))
        ));
        assert!(matches!(
            read_peer(INIT_TEXT, answer, EXPIRES + 1),
            Err(PeerError::Code(CodeError::Expired { .. }))
        ));
        let mut t = REPLY_TEXT.to_string();
        t.replace_range(10..11, "+");
        assert_eq!(
            read_peer(&t, mine_offer(), now),
            Err(PeerError::Code(CodeError::BadChar { pos: 10 }))
        );
        assert!(matches!(
            read_peer(&without_last_bytes(&INIT_BODY, &INIT_CHECK, 3), answer, now),
            Err(PeerError::Code(CodeError::BadCheck))
        ));
        let mut other = init_code();
        other.session_id = SID ^ 1;
        let other_text = write_initiator(&other).unwrap();
        assert!(matches!(
            read_peer(
                REPLY_TEXT,
                Mine::Offer {
                    own: &other,
                    own_text: &other_text
                },
                now
            ),
            Err(PeerError::Code(CodeError::WrongSession { .. }))
        ));
    }

    #[test]
    fn reply_to_an_initiator_reads_back_on_the_initiator_side() {
        let cands = Candidates::from_addrs(&[sa("127.0.0.1:47002")]).unwrap();
        let t = write_reply_to(
            &init_code(),
            Name::new("cd"),
            reply_code().nat,
            cands,
            1_000,
        )
        .unwrap();
        let c = read_peer(&t, mine_offer(), 1_000).unwrap();
        assert_eq!(
            (c.side, c.session_id, c.secret, c.candidates, c.expires),
            (Side::Responder, SID, None, cands, 1_000 + DEFAULT_TTL_SECS)
        );
        assert_eq!(
            write_reply_to(&reply_code(), Name::new("cd"), NatMark::UNKNOWN, cands, 0),
            Err(CodeError::Inconsistent)
        );
        let mut no_secret = init_code();
        no_secret.secret = None;
        assert_eq!(
            write_reply_to(&no_secret, Name::new("cd"), NatMark::UNKNOWN, cands, 0),
            Err(CodeError::Inconsistent)
        );
    }

    /// 観戦用のコードのホストの公開鍵の標本（40 41 … 5F。欄のずれ・逆順が文字で見えるように）。
    const HOST_PUBLIC: [u8; 32] = [
        0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E,
        0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x5D,
        0x5E, 0x5F,
    ];
    /// 観戦用のコードの文字（Python: INIT_BODY の種別の 1 B を 3 にし、秘密の後ろに `HOST_PUBLIC` の 32 B を挟んで
    /// 検算を付け、`base64.urlsafe_b64encode(..).rstrip(b"=")`）。
    const SPECTATE_TEXT: &str = "CAAAAAADRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0OD0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fBgIBBH8AAAGZtwQGIAENuAAAAAAAAAAAAAAAAVgbbnxN_Q";
    /// バージョン 7 の観戦用のコードの文字。
    const SPECTATE_TEXT_V7: &str = "BwAAAAADRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0OD0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fBgIBBH8AAAGZtwQGIAENuAAAAAAAAAAAAAAAAVgbhcFBcQ";
    /// バージョン 6 の観戦用のコードの文字。
    const SPECTATE_TEXT_V6: &str = "BgAAAAADRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0OD0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fBgIBBH8AAAGZtwQGIAENuAAAAAAAAAAAAAAAAVgbKz_PJg";
    /// バージョン 5 の観戦用のコードの文字。
    const SPECTATE_TEXT_V5: &str = "BQAAAAADRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0OD0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBsHA7Ws";
    /// バージョン 4 の観戦用のコードの文字。
    const SPECTATE_TEXT_V4: &str =
        "BANEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBtkj-kS";
    /// バージョン 3 の観戦用のコードの文字。
    const SPECTATE_TEXT_V3: &str =
        "AwNEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBtfJGw8";
    /// バージョン 2 の観戦用のコードの文字。
    const SPECTATE_TEXT_V2: &str =
        "AgNEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBvvJejz";
    /// バージョン 1 の観戦用のコードの文字。
    const SPECTATE_TEXT_V1: &str =
        "AQNEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBtHoq-G";
    /// バージョン 0 の観戦用のコードの文字。
    const SPECTATE_TEXT_V0: &str =
        "AANEMyIRANJJawAAAAACYWIAAQIDBAUGBwgJCgsMDQ4PAgEEfwAAAZm3BAYgAQ24AAAAAAAAAAAAAAABWBu_oMCs";

    fn spectate_code() -> Code {
        Code {
            side: Side::Spectate,
            ..init_code()
        }
    }

    #[test]
    fn version_0_codes_are_refused_with_their_version() {
        // 観戦用は中身にホストの公開鍵が付くので、比べるのは中身の `Code` だけ。
        let read_spectate = |t: &str, now: u64| read_spectate(t, now).map(|s| s.code);
        let old = Err(CodeError::BadVersion(0));
        assert_eq!(read_initiator(INIT_TEXT_V0, EXPIRES), old);
        assert_eq!(read_reply(REPLY_TEXT_V0, &secret(), SID, EXPIRES), old);
        assert_eq!(read_spectate(SPECTATE_TEXT_V0, EXPIRES), old);
        assert_eq!(
            read_peer(REPLY_TEXT_V0, mine_offer(), EXPIRES),
            Err(PeerError::Code(CodeError::BadVersion(0)))
        );
        assert_eq!(
            read_peer(INIT_TEXT_V0, Mine::Answer { own_text: None }, EXPIRES),
            Err(PeerError::Code(CodeError::BadVersion(0)))
        );
        let v1 = Err(CodeError::BadVersion(1));
        assert_eq!(read_initiator(INIT_TEXT_V1, EXPIRES), v1);
        assert_eq!(read_reply(REPLY_TEXT_V1, &secret(), SID, EXPIRES), v1);
        assert_eq!(read_spectate(SPECTATE_TEXT_V1, EXPIRES), v1);
        let v2 = Err(CodeError::BadVersion(2));
        assert_eq!(read_initiator(INIT_TEXT_V2, EXPIRES), v2);
        assert_eq!(read_reply(REPLY_TEXT_V2, &secret(), SID, EXPIRES), v2);
        assert_eq!(read_spectate(SPECTATE_TEXT_V2, EXPIRES), v2);
        let v3 = Err(CodeError::BadVersion(3));
        assert_eq!(read_initiator(INIT_TEXT_V3, EXPIRES), v3);
        assert_eq!(read_reply(REPLY_TEXT_V3, &secret(), SID, EXPIRES), v3);
        assert_eq!(read_spectate(SPECTATE_TEXT_V3, EXPIRES), v3);
        // バージョンの 1 B は build の欄より前なので、バージョン 4 の短い形でも「短い」でなく「バージョンが違う」。
        let v4 = Err(CodeError::BadVersion(4));
        assert_eq!(read_initiator(INIT_TEXT_V4, EXPIRES), v4);
        assert_eq!(read_reply(REPLY_TEXT_V4, &secret(), SID, EXPIRES), v4);
        assert_eq!(read_spectate(SPECTATE_TEXT_V4, EXPIRES), v4);
        assert_eq!(
            read_peer(INIT_TEXT_V4, Mine::Answer { own_text: None }, EXPIRES),
            Err(PeerError::Code(CodeError::BadVersion(4)))
        );
        // バージョン 5（回線の型の印が無い形）は、文字が合っていてもバージョンで断る（印の 1 B ぶん欄がずれるので、読み進めない）。
        let v5 = Err(CodeError::BadVersion(5));
        assert_eq!(read_initiator(INIT_TEXT_V5, EXPIRES), v5);
        assert_eq!(read_reply(REPLY_TEXT_V5, &secret(), SID, EXPIRES), v5);
        assert_eq!(read_spectate(SPECTATE_TEXT_V5, EXPIRES), v5);
        assert_eq!(
            read_peer(INIT_TEXT_V5, Mine::Answer { own_text: None }, EXPIRES),
            Err(PeerError::Code(CodeError::BadVersion(5)))
        );
        // バージョン 6 は形が同じでも、HELLO の +3 の bit2 を読めない相手なのでバージョンで断る。
        let v6 = Err(CodeError::BadVersion(6));
        assert_eq!(read_initiator(INIT_TEXT_V6, EXPIRES), v6);
        assert_eq!(read_reply(REPLY_TEXT_V6, &secret(), SID, EXPIRES), v6);
        assert_eq!(read_spectate(SPECTATE_TEXT_V6, EXPIRES), v6);
        assert_eq!(
            read_peer(REPLY_TEXT_V6, mine_offer(), EXPIRES),
            Err(PeerError::Code(CodeError::BadVersion(6)))
        );
        // バージョン 7 も形は同じだが、MENU の末尾の 4 B を持たない相手なのでバージョンで断る。
        let v7 = Err(CodeError::BadVersion(7));
        assert_eq!(read_initiator(INIT_TEXT_V7, EXPIRES), v7);
        assert_eq!(read_reply(REPLY_TEXT_V7, &secret(), SID, EXPIRES), v7);
        assert_eq!(read_spectate(SPECTATE_TEXT_V7, EXPIRES), v7);
        assert_eq!(
            read_peer(REPLY_TEXT_V7, mine_offer(), EXPIRES),
            Err(PeerError::Code(CodeError::BadVersion(7)))
        );
        assert_eq!(read_initiator(INIT_TEXT, EXPIRES), Ok(init_code()));
        assert_eq!(read_spectate(SPECTATE_TEXT, EXPIRES), Ok(spectate_code()));
    }

    #[test]
    fn codes_of_another_build_are_refused_with_its_build() {
        assert_eq!(BUILD_NUMBER, 0, "検査は開発用の建て方（build 0）で回す前提");
        let other = 403_920u32;
        let want = Err(CodeError::OtherBuild { theirs: other });
        assert_eq!(read_initiator(INIT_TEXT_B403920, EXPIRES), want);
        assert_eq!(
            write_with_build(&init_code(), other, None, None).unwrap(),
            INIT_TEXT_B403920
        );
        let reply = write_with_build(&reply_code(), other, Some(&secret()), None).unwrap();
        assert_eq!(read_reply(&reply, &secret(), SID, EXPIRES), want);
        let spec = write_with_build(&spectate_code(), other, None, Some(&HOST_PUBLIC)).unwrap();
        assert_eq!(read_spectate(&spec, EXPIRES).map(|s| s.code), want);
        let sr = write_with_build(&spectate_reply_code(), other, Some(&secret()), None).unwrap();
        assert_eq!(read_spectate_reply(&sr, &secret(), SID, EXPIRES), want);
        assert_eq!(
            read_peer(INIT_TEXT_B403920, Mine::Answer { own_text: None }, EXPIRES),
            Err(PeerError::Code(CodeError::OtherBuild { theirs: other }))
        );
        assert_eq!(
            read_peer(&reply, mine_offer(), EXPIRES),
            Err(PeerError::Code(CodeError::OtherBuild { theirs: other }))
        );
        // コピーの間違い（検算・タグが合わない）は、ビルドが違っても `BadCheck`・`BadTag`（ビルドは後で見る）。
        let mut b = INIT_BODY;
        b[1] = 0x50; // build を 0x50 に（検算はバージョン 0 の build のまま）
        let mut v = b.to_vec();
        v.extend_from_slice(&INIT_CHECK);
        assert_eq!(
            read_initiator(&b64_encode(&v), EXPIRES),
            Err(CodeError::BadCheck)
        );
        let mut rb = REPLY_BODY.to_vec();
        rb[2] = 0x01;
        rb.extend_from_slice(&REPLY_TAG);
        assert_eq!(
            read_reply(&b64_encode(&rb), &secret(), SID, EXPIRES),
            Err(CodeError::BadTag)
        );
        assert_eq!(
            write_with_build(&init_code(), 0, None, None).unwrap(),
            INIT_TEXT
        );
        assert_eq!(read_initiator(INIT_TEXT, EXPIRES), Ok(init_code()));
    }

    #[test]
    fn the_code_and_the_packets_carry_the_same_version() {
        let code = b64_decode(&write_initiator(&init_code()).unwrap()).unwrap();
        let spectate =
            b64_decode(&write_spectate(&spectate_code(), &HOST_PUBLIC).unwrap()).unwrap();
        let reply = b64_decode(&write_reply(&reply_code(), &secret()).unwrap()).unwrap();
        let spectate_reply =
            b64_decode(&write_spectate_reply(&spectate_reply_code(), &secret()).unwrap()).unwrap();
        let mut buf = [0u8; crate::wire::MAX_DATAGRAM];
        let hello = crate::wire::Message::Hello(crate::wire::Hello {
            proto_ver: PROTO_VER,
            latency: 0,
            name_len: 0,
            mode: crate::wire::Mode::Delay,
            can_rollback: false,
            listable: false,
            role_nonce: 1,
            name: [0; NAME_MAX],
            seat_wish: crate::wire::SeatWish::P1,
            arrival: crate::wire::Arrival::Unknown,
            build: BUILD_NUMBER,
            public: [0; PUBLIC_LEN],
        });
        let key = AuthKey::derive(&secret(), SID);
        let n = crate::wire::encode(&key, SID, &hello, &mut buf);
        assert!(n > crate::wire::HEADER_LEN);
        let packet_ver = buf[4];
        assert_eq!(
            buf[crate::wire::HEADER_LEN],
            packet_ver,
            "HELLO 本文のバージョン"
        );
        for (what, b) in [
            ("始める側", &code),
            ("観戦用", &spectate),
            ("返す側", &reply),
            ("観戦者の返し", &spectate_reply),
        ] {
            assert_eq!(
                b[0], packet_ver,
                "{what}のコードのバージョンとパケットのバージョン"
            );
        }
        // 書いた物のバージョンが今の数（8）である。数を上げるなら golden も一緒に直す。
        assert_eq!(packet_ver, 8);
        // ビルド番号も、コードと HELLO で同じ数（`BUILD_NUMBER`）。コードは offset 1、HELLO は本文の +46。
        for (what, b) in [
            ("始める側", &code),
            ("観戦用", &spectate),
            ("返す側", &reply),
            ("観戦者の返し", &spectate_reply),
        ] {
            assert_eq!(le_u32(&b[1..5]), BUILD_NUMBER, "{what}のビルド");
        }
        let hb = crate::wire::HEADER_LEN + 46;
        assert_eq!(le_u32(&buf[hb..hb + 4]), BUILD_NUMBER, "HELLO のビルド");
    }

    #[test]
    fn golden_spectate_code() {
        assert_eq!(
            write_spectate(&spectate_code(), &HOST_PUBLIC).unwrap(),
            SPECTATE_TEXT
        );
        assert_eq!(
            read_spectate(SPECTATE_TEXT, EXPIRES),
            Ok(SpectateCode {
                code: spectate_code(),
                host_public: HOST_PUBLIC
            })
        );
        // 公開鍵は秘密の直後（本文の 37 B 目から 32 B）。
        let b = b64_decode(SPECTATE_TEXT).unwrap();
        assert_eq!(&b[37..69], &HOST_PUBLIC[..]);
        assert_ne!(SPECTATE_TEXT, INIT_TEXT);
    }

    #[test]
    fn spectate_and_versus_codes_do_not_cross() {
        let cands = Candidates::from_addrs(&[sa("127.0.0.1:47002")]).unwrap();
        assert_eq!(
            read_initiator(SPECTATE_TEXT, EXPIRES),
            Err(CodeError::WrongSide(3))
        );
        assert_eq!(
            read_spectate(INIT_TEXT, EXPIRES),
            Err(CodeError::WrongSide(1))
        );
        assert_eq!(
            read_spectate(REPLY_TEXT, EXPIRES),
            Err(CodeError::WrongSide(2))
        );
        assert_eq!(
            read_reply(SPECTATE_TEXT, &secret(), SID, EXPIRES),
            Err(CodeError::WrongSide(3))
        );
        let own_text = write_initiator(&init_code()).unwrap();
        let own = init_code();
        for mine in [
            Mine::Offer {
                own: &own,
                own_text: &own_text,
            },
            Mine::Answer { own_text: None },
        ] {
            assert_eq!(
                read_peer(SPECTATE_TEXT, mine, EXPIRES),
                Err(PeerError::Code(CodeError::WrongSide(3)))
            );
        }
        assert_eq!(
            write_reply_to(
                &spectate_code(),
                Name::new("cd"),
                NatMark::UNKNOWN,
                cands,
                0
            ),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_initiator(&spectate_code()),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_spectate(&init_code(), &HOST_PUBLIC),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_reply(&spectate_code(), &secret()),
            Err(CodeError::Inconsistent)
        );
        let mut no_secret = spectate_code();
        no_secret.secret = None;
        assert_eq!(
            write_spectate(&no_secret, &HOST_PUBLIC),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_with_build(&init_code(), 0, None, Some(&HOST_PUBLIC)),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_with_build(&spectate_code(), 0, None, None),
            Err(CodeError::Inconsistent)
        );
        // 種別の 1 B を 3 → 1 に書き換えても、検算が合わず始める側のコードとして読めない。
        let mut bytes = INIT_BODY.to_vec();
        bytes[5] = 3;
        let mut forged = bytes.clone();
        forged[5] = 1;
        forged.extend_from_slice(&check_of(&bytes)); // 3 のときの検算のまま
        assert_eq!(
            read_initiator(&b64_encode(&forged), EXPIRES),
            Err(CodeError::BadCheck)
        );
    }

    /// 観戦者の返し（種別 4）の golden の本文（`REPLY_BODY` の種別の 1 B だけを 4 にした 31 B）。
    #[rustfmt::skip]
    const SPECTATE_REPLY_BODY: [u8; 31] = [
        0x08, 0x00, 0x00, 0x00, 0x00, 0x04, // バージョン 8・build 0・観戦者の返し
        0x44, 0x33, 0x22, 0x11, // 観戦用のコードの session 0x11223344
        0x00, 0xD2, 0x49, 0x6B, 0x00, 0x00, 0x00, 0x00, // 期限 1800000000
        0x02, b'c', b'd',       // 名前 "cd"
        0x01,                   // 回線の型の印
        0x01,                   // 候補 1 つ
        0x02, 0x04, 192, 168, 0, 2, 0x59, 0x1B, // host 192.168.0.2:7001
    ];
    /// タグ（Python: `hmac.new(K, SPECTATE_REPLY_BODY, sha256).digest()[:16]`。K は観戦用のコードの秘密と
    /// session から導く。`auth.rs` の `K_GOLDEN`）。
    const SPECTATE_REPLY_TAG: [u8; 16] = [
        0x60, 0x3A, 0x6F, 0xFA, 0xF9, 0x51, 0x13, 0xAB, 0x00, 0x3A, 0x81, 0x5A, 0x72, 0xA8, 0x69,
        0x9C,
    ];
    /// 文字（Python: `base64.urlsafe_b64encode(SPECTATE_REPLY_BODY + SPECTATE_REPLY_TAG).rstrip(b"=")`）。
    const SPECTATE_REPLY_TEXT: &str =
        "CAAAAAAERDMiEQDSSWsAAAAAAmNkAQECBMCoAAJZG2A6b_r5UROrADqBWnKoaZw";

    fn spectate_reply_code() -> Code {
        Code {
            side: Side::SpectateReply,
            ..reply_code()
        }
    }

    #[test]
    fn golden_spectate_reply_code() {
        let mut want = SPECTATE_REPLY_BODY.to_vec();
        want.extend_from_slice(&SPECTATE_REPLY_TAG);
        assert_eq!(b64_encode(&want), SPECTATE_REPLY_TEXT);
        assert_eq!(
            write_spectate_reply(&spectate_reply_code(), &secret()).unwrap(),
            SPECTATE_REPLY_TEXT
        );
        assert_eq!(
            read_spectate_reply(SPECTATE_REPLY_TEXT, &secret(), SID, EXPIRES),
            Ok(spectate_reply_code())
        );
        assert_ne!(SPECTATE_REPLY_TEXT, REPLY_TEXT);
        assert_eq!(SPECTATE_REPLY_BODY[..5], REPLY_BODY[..5]);
        assert_eq!(SPECTATE_REPLY_BODY[6..], REPLY_BODY[6..]);
        assert_ne!(SPECTATE_REPLY_TAG, REPLY_TAG);
    }

    #[test]
    fn spectate_reply_and_other_kinds_do_not_cross() {
        let mut wrong_secret = secret();
        wrong_secret.0[0] ^= 1;
        let t4 = SPECTATE_REPLY_TEXT;
        assert_eq!(read_initiator(t4, EXPIRES), Err(CodeError::WrongSide(4)));
        assert_eq!(read_spectate(t4, EXPIRES), Err(CodeError::WrongSide(4)));
        assert_eq!(
            read_reply(t4, &secret(), SID, EXPIRES),
            Err(CodeError::WrongSide(4))
        );
        // 秘密が違っても session が違っても、種別が先。
        assert_eq!(
            read_reply(t4, &wrong_secret, SID ^ 1, EXPIRES),
            Err(CodeError::WrongSide(4))
        );
        for mine in [
            mine_offer(),
            Mine::Answer { own_text: None },
            Mine::Answer { own_text: Some(t4) },
        ] {
            let got = read_peer(t4, mine, EXPIRES);
            assert!(
                matches!(
                    got,
                    Err(PeerError::Code(CodeError::WrongSide(4))) | Err(PeerError::OwnCode)
                ),
                "{got:?}"
            );
        }
        assert_eq!(
            read_spectate_reply(INIT_TEXT, &secret(), SID, EXPIRES),
            Err(CodeError::WrongSide(1))
        );
        assert_eq!(
            read_spectate_reply(REPLY_TEXT, &secret(), SID, EXPIRES),
            Err(CodeError::WrongSide(2))
        );
        assert_eq!(
            read_spectate_reply(SPECTATE_TEXT, &secret(), SID, EXPIRES),
            Err(CodeError::WrongSide(3))
        );
        // 種別の 1 B を 4 → 2 に書き換えても、タグが種別を含むので返す側のコードとして偽れない。
        let mut forged = SPECTATE_REPLY_BODY.to_vec();
        forged[5] = 2;
        forged.extend_from_slice(&SPECTATE_REPLY_TAG);
        assert_eq!(
            read_reply(&b64_encode(&forged), &secret(), SID, EXPIRES),
            Err(CodeError::BadTag)
        );
        assert_eq!(
            write_spectate_reply(&reply_code(), &secret()),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_reply(&spectate_reply_code(), &secret()),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_initiator(&spectate_reply_code()),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(
            write_spectate(&spectate_reply_code(), &HOST_PUBLIC),
            Err(CodeError::Inconsistent)
        );
        let mut with_secret = spectate_reply_code();
        with_secret.secret = Some(secret());
        assert_eq!(
            write_spectate_reply(&with_secret, &secret()),
            Err(CodeError::Inconsistent)
        );
        assert!(read_spectate_reply(t4, &secret(), SID, EXPIRES).is_ok());
    }

    #[test]
    fn spectate_reply_poisons_are_refused_with_their_own_reason() {
        let now = EXPIRES;
        let t = SPECTATE_REPLY_TEXT;
        assert_eq!(
            read_spectate_reply(t, &secret(), SID ^ 1, now),
            Err(CodeError::WrongSession {
                got: SID,
                want: SID ^ 1
            })
        );
        let mut other = secret();
        other.0[7] ^= 0x10;
        assert_eq!(
            read_spectate_reply(t, &other, SID, now),
            Err(CodeError::BadTag)
        );
        let mut v = SPECTATE_REPLY_BODY.to_vec();
        v[25] ^= 0x01;
        v.extend_from_slice(&SPECTATE_REPLY_TAG);
        assert_eq!(
            read_spectate_reply(&b64_encode(&v), &secret(), SID, now),
            Err(CodeError::BadTag)
        );
        let mut tag = SPECTATE_REPLY_TAG;
        tag[0] ^= 0x01;
        let mut v = SPECTATE_REPLY_BODY.to_vec();
        v.extend_from_slice(&tag);
        assert_eq!(
            read_spectate_reply(&b64_encode(&v), &secret(), SID, now),
            Err(CodeError::BadTag)
        );
        // 期限: 1 秒過ぎで断り、ちょうどは読める。タグが悪いときはタグが先。
        assert_eq!(
            read_spectate_reply(t, &secret(), SID, EXPIRES + 1),
            Err(CodeError::Expired {
                expires: EXPIRES,
                now: EXPIRES + 1
            })
        );
        assert!(read_spectate_reply(t, &secret(), SID, EXPIRES).is_ok());
        assert_eq!(
            read_spectate_reply(&b64_encode(&v), &secret(), SID, EXPIRES + 1),
            Err(CodeError::BadTag)
        );
        // バージョン違い（タグも合わなくなるが、バージョンが先）。
        for ver in [0u8, 1, 2, 3, 4, 5, 6, 7, 9] {
            let mut v = SPECTATE_REPLY_BODY.to_vec();
            v[0] = ver;
            v.extend_from_slice(&SPECTATE_REPLY_TAG);
            assert_eq!(
                read_spectate_reply(&b64_encode(&v), &secret(), SID, now),
                Err(CodeError::BadVersion(ver))
            );
        }
        let signed = |body: &[u8]| {
            let mut v = body.to_vec();
            v.extend_from_slice(&tag_of(&secret(), SID, body));
            b64_encode(&v)
        };
        let mut long = SPECTATE_REPLY_BODY.to_vec();
        long.push(0);
        assert_eq!(
            read_spectate_reply(&signed(&long), &secret(), SID, now),
            Err(CodeError::LengthMismatch {
                expected: 31,
                actual: 32
            })
        );
        let mut none = SPECTATE_REPLY_BODY;
        none[22] = 0;
        assert_eq!(
            read_spectate_reply(&signed(&none), &secret(), SID, now),
            Err(CodeError::NoCandidates)
        );
        // 付け直したタグが合っていれば読める（上の毒がタグでなく本文の理由で落ちている証拠）。
        assert!(read_spectate_reply(&signed(&SPECTATE_REPLY_BODY), &secret(), SID, now).is_ok());
        let mut bad = t.to_string();
        bad.replace_range(10..11, "+");
        assert_eq!(
            read_spectate_reply(&bad, &secret(), SID, now),
            Err(CodeError::BadChar { pos: 10 })
        );
        assert_eq!(
            read_spectate_reply("", &secret(), SID, now),
            Err(CodeError::TooShort)
        );
        assert_eq!(
            read_spectate_reply(&"A".repeat(MAX_CODE_CHARS + 4), &secret(), SID, now),
            Err(CodeError::BadLength)
        );
    }

    #[test]
    fn spectate_reply_to_a_spectate_code_reads_back_on_the_casters_side() {
        let cands = Candidates::from_addrs(&[sa("127.0.0.1:47002")]).unwrap();
        let spec = spectate_code();
        let t = write_spectate_reply_to(&spec, Name::new("ef"), cands, 1_000).unwrap();
        let c = read_spectate_reply(&t, &secret(), SID, 1_000).unwrap();
        assert_eq!(
            (c.side, c.session_id, c.secret, c.candidates, c.expires),
            (
                Side::SpectateReply,
                SID,
                None,
                cands,
                1_000 + SPECTATE_REPLY_TTL_SECS
            )
        );
        assert_eq!(SPECTATE_REPLY_TTL_SECS, 300, "メインが決めた 5 分");
        assert_eq!(c.name, Name::new("ef"));
        assert!(matches!(
            read_spectate_reply(&t, &secret(), SID, 1_000 + SPECTATE_REPLY_TTL_SECS + 1),
            Err(CodeError::Expired { .. })
        ));
        let mut other = secret();
        other.0[1] ^= 1;
        assert_eq!(
            read_spectate_reply(&t, &other, SID, 1_000),
            Err(CodeError::BadTag)
        );
        for bad in [init_code(), reply_code(), spectate_reply_code(), {
            let mut c = spectate_code();
            c.secret = None;
            c
        }] {
            assert_eq!(
                write_spectate_reply_to(&bad, Name::new("ef"), cands, 0),
                Err(CodeError::Inconsistent),
                "{:?}",
                bad.side
            );
        }
        // 期限の足し算は飽和する（時計が壊れていてもパニックしない）。
        assert!(write_spectate_reply_to(&spec, Name::new("ef"), cands, u64::MAX).is_ok());
    }

    /// 本文を少し変え、末尾を付け直して文字にする（毒が狙ったガードまで届くように）。
    fn init_text_of(body: &[u8]) -> String {
        let mut v = body.to_vec();
        v.extend_from_slice(&check_of(body));
        b64_encode(&v)
    }

    #[test]
    fn poisons_are_refused_with_their_own_reason() {
        let now = EXPIRES;
        let mut t = INIT_TEXT.to_string();
        t.replace_range(10..11, "+");
        assert_eq!(read_initiator(&t, now), Err(CodeError::BadChar { pos: 10 }));
        // バージョンの 1 B だけ合う先頭（"CAAA"）は、バージョン違いでなく短い。
        assert_eq!(read_initiator("CAAA", now), Err(CodeError::TooShort));
        // バージョン 0 の先頭（"AAFE"）は、短くてもバージョン違いが先。
        assert_eq!(read_initiator("AAFE", now), Err(CodeError::BadVersion(0)));
        assert_eq!(read_initiator("", now), Err(CodeError::TooShort));
        assert_eq!(
            read_initiator(&without_last_bytes(&INIT_BODY, &INIT_CHECK, 3), now),
            Err(CodeError::BadCheck)
        );
        let mut b = INIT_BODY.to_vec();
        b.push(0);
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::LengthMismatch {
                expected: 67,
                actual: 68
            })
        );
        assert_eq!(
            read_initiator(&init_text_of(&INIT_BODY[..66]), now),
            Err(CodeError::TooShort)
        );
        // バージョン違い（検算は付け直してあるので、バージョンのガードだけが落とす）。
        for v in [0u8, 1, 2, 3, 4, 5, 6, 7, 9] {
            let mut b = INIT_BODY;
            b[0] = v;
            assert_eq!(
                read_initiator(&init_text_of(&b), now),
                Err(CodeError::BadVersion(v))
            );
        }
        assert_eq!(
            read_initiator(REPLY_TEXT, now),
            Err(CodeError::WrongSide(2))
        );
        assert_eq!(
            read_reply(INIT_TEXT, &secret(), SID, now),
            Err(CodeError::WrongSide(1))
        );
        let mut b = INIT_BODY;
        b[5] = 3;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::WrongSide(3))
        );
        assert_eq!(
            read_initiator(INIT_TEXT, EXPIRES + 1),
            Err(CodeError::Expired {
                expires: EXPIRES,
                now: EXPIRES + 1
            })
        );
        assert!(read_initiator(INIT_TEXT, EXPIRES).is_ok());
        assert!(matches!(
            read_reply(REPLY_TEXT, &secret(), SID, EXPIRES + 1),
            Err(CodeError::Expired { .. })
        ));
        for i in [3usize, 24, 37, 44, 66] {
            let mut v = INIT_BODY.to_vec();
            v[i] ^= 0x04;
            v.extend_from_slice(&INIT_CHECK);
            assert_eq!(
                read_initiator(&b64_encode(&v), now),
                Err(CodeError::BadCheck),
                "本文の {i} B 目"
            );
        }
        let mut v = INIT_BODY.to_vec();
        v.extend_from_slice(&[0x04, 0x27, 0xA6, 0x49]);
        assert_eq!(
            read_initiator(&b64_encode(&v), now),
            Err(CodeError::BadCheck)
        );
        let mut v = REPLY_BODY.to_vec();
        v[25] ^= 0x01; // 候補のアドレスを差し替える
        v.extend_from_slice(&REPLY_TAG);
        assert_eq!(
            read_reply(&b64_encode(&v), &secret(), SID, now),
            Err(CodeError::BadTag)
        );
        let mut v = REPLY_BODY.to_vec();
        let mut t = REPLY_TAG;
        t[15] ^= 0x80;
        v.extend_from_slice(&t);
        assert_eq!(
            read_reply(&b64_encode(&v), &secret(), SID, now),
            Err(CodeError::BadTag)
        );
        let mut other = secret();
        other.0[0] ^= 1;
        assert_eq!(
            read_reply(REPLY_TEXT, &other, SID, now),
            Err(CodeError::BadTag)
        );
        assert_eq!(
            read_reply(REPLY_TEXT, &secret(), SID ^ 1, now),
            Err(CodeError::WrongSession {
                got: SID,
                want: SID ^ 1
            })
        );
        let at_count = INIT_BODY.len() - 8 - 20 - 1;
        assert_eq!(INIT_BODY[at_count], 2, "検査の前提（候補の数の位置）");
        assert_eq!(INIT_BODY[at_count - 1], 0x06, "検査の前提（型の印の位置）");
        let mut b = INIT_BODY;
        b[at_count] = 9;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::TooManyCandidates(9))
        );
        b[at_count] = 255;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::TooManyCandidates(255))
        );
        b[at_count] = 0;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::NoCandidates)
        );
        let c0 = at_count + 1;
        let mut b = INIT_BODY;
        b[c0] = 9;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::BadCandidate {
                index: 0,
                err: None
            })
        );
        let mut b = INIT_BODY;
        b[c0 + 1] = 5;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::BadCandidate {
                index: 0,
                err: None
            })
        );
        let mut b = INIT_BODY;
        b[c0 + 2] = 192;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::BadCandidate {
                index: 0,
                err: Some(CandError::KindMismatch)
            })
        );
        let mut b = INIT_BODY;
        b[c0 + 6] = 0;
        b[c0 + 7] = 0;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::BadCandidate {
                index: 0,
                err: Some(CandError::PortZero)
            })
        );
        let mut dup = INIT_BODY[..at_count].to_vec();
        dup.push(2);
        dup.extend_from_slice(&INIT_BODY[c0..c0 + 8]);
        dup.extend_from_slice(&INIT_BODY[c0..c0 + 8]);
        assert_eq!(
            read_initiator(&init_text_of(&dup), now),
            Err(CodeError::BadCandidate {
                index: 1,
                err: Some(CandError::Duplicate)
            })
        );
        let mut b = INIT_BODY;
        b[18] = 33;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::BadName)
        );
        let mut b = INIT_BODY;
        b[19] = 0xFF;
        assert_eq!(
            read_initiator(&init_text_of(&b), now),
            Err(CodeError::BadName)
        );
        let long = "A".repeat(MAX_CODE_CHARS + 4);
        assert_eq!(read_initiator(&long, now), Err(CodeError::BadLength));
    }

    /// 型の印の 1 B の位置（始める側は秘密の後ろ、返す側は名前の後ろ）。印は検算・タグの内側にあるので、書き換えるとコピーの間違いと見える。
    #[test]
    fn the_nat_mark_round_trips_in_every_kind_of_code_and_undefined_bytes_are_refused() {
        let now = EXPIRES;
        let marks = [
            (Symmetry::Unknown, false),
            (Symmetry::No, false),
            (Symmetry::Yes, false),
            (Symmetry::Unknown, true),
            (Symmetry::No, true),
            (Symmetry::Yes, true),
        ];
        for (symmetry, upnp) in marks {
            let nat = NatMark { symmetry, upnp };
            let init = Code { nat, ..init_code() };
            assert_eq!(
                read_initiator(&write_initiator(&init).unwrap(), now),
                Ok(init),
                "{nat:?}"
            );
            let reply = Code {
                nat,
                ..reply_code()
            };
            let t = write_reply(&reply, &secret()).unwrap();
            assert_eq!(read_reply(&t, &secret(), SID, now), Ok(reply), "{nat:?}");
            let spec = Code {
                nat,
                ..spectate_code()
            };
            let t = write_spectate(&spec, &HOST_PUBLIC).unwrap();
            assert_eq!(read_spectate(&t, now).map(|s| s.code), Ok(spec), "{nat:?}");
        }
        // 印が違えば文字も違う（印がコードの本文に入っている）。
        let a = write_initiator(&init_code()).unwrap();
        let b = write_initiator(&Code {
            nat: NatMark::UNKNOWN,
            ..init_code()
        })
        .unwrap();
        assert_ne!(a, b);
        // 定義の外の印は、検算・タグを付け直しても断る（型の値 3、定義の外の bit）。
        for bad in [0x03u8, 0x07, 0x08, 0x10, 0x80, 0xFF] {
            let mut ib = INIT_BODY;
            ib[37] = bad;
            assert_eq!(
                read_initiator(&init_text_of(&ib), now),
                Err(CodeError::BadNat(bad)),
                "始める側 {bad:#x}"
            );
            let mut rb = REPLY_BODY;
            rb[21] = bad;
            assert_eq!(
                read_reply(&reply_text_of_any(&rb), &secret(), SID, now),
                Err(CodeError::BadNat(bad)),
                "返す側 {bad:#x}"
            );
        }
        // 位置の前提（始める側 37 B 目・返す側 21 B 目が印）。
        assert_eq!(INIT_BODY[37], 0x06);
        assert_eq!(REPLY_BODY[21], 0x01);
    }

    #[test]
    fn writers_refuse_inconsistent_codes() {
        let mut c = init_code();
        c.secret = None;
        assert_eq!(write_initiator(&c), Err(CodeError::Inconsistent));
        let mut c = reply_code();
        c.secret = Some(secret());
        assert_eq!(write_reply(&c, &secret()), Err(CodeError::Inconsistent));
        assert_eq!(
            write_reply(&init_code(), &secret()),
            Err(CodeError::Inconsistent)
        );
        assert_eq!(write_initiator(&reply_code()), Err(CodeError::Inconsistent));
        let mut c = init_code();
        c.candidates = Candidates::EMPTY;
        assert_eq!(write_initiator(&c), Err(CodeError::NoCandidates));
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_candidate(r: &mut Rng) -> Candidate {
        let port = 1 + r.below(65_535) as u16;
        let (kind, ip) = match r.below(5) {
            0 => (
                CandKind::Loopback,
                IpAddr::V4(Ipv4Addr::new(
                    127,
                    r.next() as u8,
                    r.next() as u8,
                    r.next() as u8,
                )),
            ),
            1 => (CandKind::Loopback, IpAddr::V6(Ipv6Addr::LOCALHOST)),
            2 => (
                CandKind::Host,
                IpAddr::V4(Ipv4Addr::new(
                    192,
                    168,
                    r.next() as u8,
                    1 + r.below(254) as u8,
                )),
            ),
            3 => (
                CandKind::External,
                IpAddr::V4(Ipv4Addr::new(1 + r.below(100) as u8, r.next() as u8, 0, 1)),
            ),
            _ => (
                CandKind::Ipv6,
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, r.next() as u16, 1)),
            ),
        };
        Candidate::new(kind, SocketAddr::new(ip, port)).unwrap()
    }

    #[test]
    fn roundtrip_100k_random_codes() {
        let mut r = Rng(0xC0DE_0000_0000_0001);
        let mut name_lens = [0u32; 33];
        let mut cand_counts = [0u32; MAX_CANDIDATES + 1];
        for i in 0..100_000u32 {
            let mut name = String::new();
            let nl = r.below(33) as usize;
            for _ in 0..nl {
                name.push(char::from(0x20 + r.below(0x5F) as u8));
            }
            let mut cands = Candidates::EMPTY;
            let want = 1 + r.below(MAX_CANDIDATES as u64) as usize;
            while cands.len() < want {
                let _ = cands.push(random_candidate(&mut r));
            }
            name_lens[nl] += 1;
            cand_counts[cands.len()] += 1;
            let mut s = [0u8; SECRET_LEN];
            for b in s.iter_mut() {
                *b = r.next() as u8;
            }
            let secret = Secret(s);
            let session_id = r.next() as u32;
            let expires = r.next();
            let initiator = r.below(2) == 0;
            let c = Code {
                side: if initiator {
                    Side::Initiator
                } else {
                    Side::Responder
                },
                session_id,
                secret: initiator.then_some(secret),
                name: Name::new(&name),
                nat: NatMark {
                    symmetry: [Symmetry::Unknown, Symmetry::No, Symmetry::Yes][r.below(3) as usize],
                    upnp: r.below(2) == 0,
                },
                candidates: cands,
                expires,
            };
            let got = if initiator {
                let t = write_initiator(&c).unwrap();
                assert!(t.len() <= MAX_CODE_CHARS, "iter {i}: {} 文字", t.len());
                read_initiator(&t, 0)
            } else {
                let t = write_reply(&c, &secret).unwrap();
                read_reply(&t, &secret, session_id, 0)
            };
            assert_eq!(got, Ok(c), "iter {i}");
        }
        assert!(name_lens[0] > 0 && name_lens[32] > 0, "{name_lens:?}");
        assert!(
            cand_counts[1] > 0 && cand_counts[MAX_CANDIDATES] > 0,
            "{cand_counts:?}"
        );
    }

    #[test]
    fn readers_never_panic_on_garbage() {
        let mut r = Rng(0x6A2B_A6E0);
        for i in 0..50_000u32 {
            let n = r.below(120) as usize;
            let mut b: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
            if i % 2 == 0 && n >= 6 {
                // バージョン・ビルドと種別だけは通して、中身を読む所まで届かせる（検算も付け直す）。
                b[0] = PROTO_VER;
                b[1..5].copy_from_slice(&BUILD_NUMBER.to_le_bytes());
                b[5] = 1;
                let t = init_text_of(&b);
                let _ = read_initiator(&t, 0);
                b[5] = 3;
                let _ = read_spectate(&init_text_of(&b), 0);
                b[5] = 2;
                let _ = read_reply(&reply_text_of_any(&b), &secret(), le_u32_or0(&b), 0);
                b[5] = 4;
                let _ = read_spectate_reply(&reply_text_of_any(&b), &secret(), le_u32_or0(&b), 0);
            }
            let t = b64_encode(&b);
            let _ = read_initiator(&t, 0);
            let _ = read_reply(&t, &secret(), 1, 0);
            let s: String = (0..n).map(|_| char::from(r.next() as u8 & 0x7F)).collect();
            let _ = read_initiator(&s, 0);
        }
    }

    fn le_u32_or0(b: &[u8]) -> u32 {
        b.get(6..10).map_or(0, le_u32)
    }

    fn reply_text_of_any(body: &[u8]) -> String {
        let mut v = body.to_vec();
        v.extend_from_slice(&tag_of(&secret(), le_u32_or0(body), body));
        b64_encode(&v)
    }
}
