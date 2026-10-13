//! 起動前の更新の確認のうち、ネットから来るバイト列を読む部分（pure）。
//!
//! 配るバージョンの `HiganRTool.exe` は、花映塚を起こす前に「更新の目録」を 1 つ取り、署名を確かめ、今より新しいバージョンならウィンドウで尋ねる
//! （流れは `src/loader/src/update.rs`）。ここは目録の形・署名・バージョンの比べ・置き換えてよいファイル名・URL・設定の読みだけを持つ。
//! ファイルを書く・通信する層は持たない（検査を本物の通信なしに回すため）。
//!
//! ## 目録の形（バージョン 1。UTF-8、行の区切りは LF だけ、1 つの空白で区切る）
//!
//! ```text
//! HiganRTool-manifest 1
//! version 2400321
//! display 20261008-6e936b3
//! file HiganRTool.exe 1234567 <SHA-256 を小文字の 16 進 64 文字で>
//! file HiganRTool.dll 2345678 <同>
//! signature <Ed25519 の署名を小文字の 16 進 128 文字で>
//! ```
//!
//! 署名は、最後の `signature` の行を除いた本文のバイト列の前に [`SIGN_CONTEXT`] を付けたものに対して付ける。
//! 署名を確かめてから本文を読む。
//!
//! ## ビルド番号
//!
//! `version` と [`BUILD_NUMBER`] は同じ数（単調に増える u32）。配るバージョンのビルドのときだけ `tools/make_dist.py` が
//! 環境変数 `HIGANR_BUILD_NUMBER` で渡す: **2026-01-01 00:00 UTC からの経過分**（例: 2026-10-08 12:00 UTC は 403,920 前後）。
//! 開発用のビルド（環境変数なし）は 0 で、0 のときローダーは確かめに行かない。
//!
//! ## 差し替える場所（本物の鍵・配り場所）
//!
//! - 公開鍵: [`UPDATE_PUBLIC_KEY_HEX`]。検査用の鍵（[`TEST_PUBLIC_KEY_HEX`]）は秘密の種が公開なので、これを載せた exe には
//!   誰でも更新を署名できる。`tools/make_dist.py` は検査用の鍵に戻っていると止まる。
//! - 目録の URL: [`UPDATE_SERVER`] ＋ [`MANIFEST_PATH`]。

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

// ---- 差し替える定数 ----

/// 更新の URL を持つサーバー（ルームサーバーと同じ宿）。配った exe はこの宿へ問い合わせるので、変えるなら配り直しになる。
pub const UPDATE_SERVER: &str = "https://higanr-rooms.higan-re-room.workers.dev";
/// 目録の URL（サーバーが配り場所の最新の目録へ 302 で回す）。
pub const MANIFEST_PATH: &str = "/update/manifest";
/// ファイルの URL の先頭（この後ろに名前を付ける。サーバーが配り場所の同名のファイルへ 302 で回す）。
pub const FILES_PATH: &str = "/update/files/";

/// 配るバージョンが信じる公開鍵（Ed25519、32 バイトの小文字の 16 進）。秘密の鍵はリポジトリの外。
pub const UPDATE_PUBLIC_KEY_HEX: &str =
    "5e817e7227e26e6514b4a3a1e0e9d8ff85af5a357a617083a015fee4609623b2";
/// 検査用の鍵の公開鍵（秘密の種は `test_key::SEED`）。`UPDATE_PUBLIC_KEY_HEX` がこれと同じなら、まだ本物の鍵に替えていない。
pub const TEST_PUBLIC_KEY_HEX: &str =
    "02937f57dd46ff2563d4ecedad0546da0ec561ad38c3d316ab016de447d74fb4";

/// 署名のときに本文の前に付ける文字列（この署名を別の用途に使い回せないようにする区切り）。
pub const SIGN_CONTEXT: &[u8] = b"HiganRTool update manifest v1\n";
pub const MANIFEST_MAGIC: &str = "HiganRTool-manifest 1";

/// 目録の大きさの上限（バイト）。ネットから来る長さなので上を決める。
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024;
/// 1 ファイルの大きさの上限（バイト）。目録の `size` はこれ以下だけ受ける（署名つきでも）。
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// 確かめに行く待ちの上限（ミリ秒。名前の解決から本文の受信まで）。ネットに出られないときも、花映塚の起動をこれ以上遅らせない。
pub const CHECK_DEADLINE_MS: u64 = 2000;

/// 更新の直後に起こし直した新しい exe へ渡す環境変数の名前。あれば確かめに行かない（更新が終わらずに起こし直しが続く事故を
/// 止める歯止め）。
pub const JUST_UPDATED_ENV: &str = "HIGANRTOOL_JUST_UPDATED";

// ---- 置き換えてよいファイル ----

/// このバージョンが配るファイル名（配る zip の中身のうち、利用者の設定でないもの）。更新の片づけ（`.old` / `.new`）はこの名前だけを
/// 見る。置き換えてよい名前は [`is_allowed_name`] の規則で決まり、この表には縛られない（目録は署名で守られている）。
pub const SHIPPED_FILES: [&str; 6] = [
    "HiganRTool.exe",
    "HiganRTool.dll",
    "README.txt",
    "README_en.txt",
    "LICENSE.txt",
    "THIRD_PARTY_LICENSES.txt",
];
/// 目録に必ず載るファイル（exe だけ・dll だけの更新で、組み合わせの違う 2 つが並ぶのを断る）。
pub const REQUIRED_FILES: [&str; 2] = ["HiganRTool.exe", "HiganRTool.dll"];
/// 動いている自分自身（最後に置き換える）。
pub const SELF_EXE: &str = "HiganRTool.exe";

/// 名前の長さの上限（文字）。サーバーの `isUpdatableFileName`（`src/server/src/update.ts`）と同じ規則。
pub const MAX_NAME_LEN: usize = 64;
/// 更新が置き換えない名前（利用者のもの・道具が自分で作るもの・ゲームの exe）。大文字小文字を区別せずに比べる。
/// 目録の名前 2 つは配り場所の側の名前（`tools/make_dist.py` の `MANIFEST_ASSET` / `UNSIGNED_MANIFEST`）。
const RESERVED_NAMES: [&str; 8] = [
    "HiganRTool.ini",
    "game_path.txt",
    LAUNCHED_MARK,
    "manifest.txt",
    "manifest.unsigned.txt",
    "th09.exe",
    "logs",
    "code",
];
/// 置き換えの途中で道具が作る名前の語尾。
const TEMP_SUFFIXES: [&str; 2] = [".old", ".new"];

/// 更新が置き換えてよいファイル名か。普通のファイル名（ASCII の英数字と `.` `_` `-` だけ・1〜[`MAX_NAME_LEN`] 文字）で、
/// `.` で始まらず・終わらず・`..` を含まず、`.old` / `.new` で終わらず、利用者のもの（[`RESERVED_NAMES`]）でなく、
/// Windows の機器の名前（`CON`・`NUL`・`COM1` など。拡張子が付いても機器）でないこと。
pub fn is_allowed_name(name: &str) -> bool {
    let plain = (1..=MAX_NAME_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..");
    plain
        && !RESERVED_NAMES.iter().any(|r| name.eq_ignore_ascii_case(r))
        && !TEMP_SUFFIXES.iter().any(|s| ends_with_ignore_case(name, s))
        && !is_device_name(name)
}

fn ends_with_ignore_case(name: &str, suffix: &str) -> bool {
    name.len() >= suffix.len()
        && name.as_bytes()[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// `CON` `PRN` `AUX` `NUL` `COM0`〜`COM9` `LPT0`〜`LPT9`（最初の `.` より前。Windows は拡張子が付いても機器として開く）。
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let up = stem.to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((up.starts_with("COM") || up.starts_with("LPT"))
            && up.len() == 4
            && up.as_bytes()[3].is_ascii_digit())
}

/// 置き換えの前に名前を変えて取っておく先（`HiganRTool.exe` → `HiganRTool.exe.old`）。
pub fn old_name(name: &str) -> String {
    format!("{name}.old")
}

/// 落とした物を置き換えの前に書いておく先（`HiganRTool.exe` → `HiganRTool.exe.new`）。
pub fn new_name(name: &str) -> String {
    format!("{name}.new")
}

// ---- ビルド番号 ----

/// このビルド番号。配るバージョンのビルドのときだけ環境変数 `HIGANR_BUILD_NUMBER` から入る。無い・読めない値は 0。
pub const BUILD_NUMBER: u32 = parse_build_number(option_env!("HIGANR_BUILD_NUMBER"));

/// 10 進の数字だけの文字列を u32 にする（空・数字以外・u32 を越える値は 0）。const で `BUILD_NUMBER` を作るための関数。
pub const fn parse_build_number(s: Option<&str>) -> u32 {
    let Some(s) = s else { return 0 };
    let mut rest = s.as_bytes();
    if rest.is_empty() {
        return 0;
    }
    let mut n: u64 = 0;
    while let [c, tail @ ..] = rest {
        if *c < b'0' || *c > b'9' {
            return 0;
        }
        n = n * 10 + (*c - b'0') as u64;
        if n > u32::MAX as u64 {
            return 0;
        }
        rest = tail;
    }
    n as u32
}

/// ビルド番号の起点（2026-01-01 00:00 UTC）の、1970-01-01 からの日数。
const BUILD_EPOCH_DAYS: i64 = 20_454;
/// ビルド番号を画面に出すときの時差（分）。配る zip の名と更新のウィンドウのバージョンの名は日本の時刻なので UTC+9。
const BUILD_SHOWN_OFFSET_MIN: i64 = 9 * 60;

/// ビルド番号を、日本の時刻の（年, 月, 日, 時, 分）にする。0（開発用のビルド）は None。更新のウィンドウが見せるバージョンの名と同じ日付で
/// 書くため。確保しない。
pub fn build_stamp(n: u32) -> Option<(u32, u32, u32, u32, u32)> {
    if n == 0 {
        return None;
    }
    let minutes = i64::from(n) + BUILD_SHOWN_OFFSET_MIN;
    let days = BUILD_EPOCH_DAYS + minutes.div_euclid(24 * 60);
    let in_day = minutes.rem_euclid(24 * 60);
    // 1970-01-01 からの日数 → 年月日（グレゴリオ暦。civil_from_days の式）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some((
        u32::try_from(y).unwrap_or(0),
        m as u32,
        d as u32,
        (in_day / 60) as u32,
        (in_day % 60) as u32,
    ))
}

/// 目録のバージョンが、いまのバージョンより新しいか。**同じ・古いは使わない**（古いバージョンへ戻す攻撃と、同じバージョンの再配布を断る）。
pub fn is_newer(current: u32, offered: u32) -> bool {
    offered > current
}

// ---- 起こせた印 ----

/// 新しいバージョンが花映塚を起こせたときに書く印の名前（ローダーの exe のフォルダ）。中身はビルド番号（10 進 1 行）。一つ前のバージョンの
/// `.old` は、この印が今のバージョンと同じになった次の起動で消す。起こす前に消すと、起こせなかった新しいバージョンから元のバージョンへ戻せなくなる。
pub const LAUNCHED_MARK: &str = "update_launched.txt";

pub fn launched_mark_text(build: u32) -> String {
    format!("{build}\n")
}

/// 印の中身が今のビルド番号と同じか。読めない中身・ビルド番号 0（開発のビルド）・別のバージョンの印は false（`.old` を消さない側へ倒す）。
pub fn launched_mark_matches(text: Option<&str>, build: u32) -> bool {
    build != 0 && text.and_then(|t| t.trim().parse::<u32>().ok()) == Some(build)
}

// ---- 目録 ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    /// ウィンドウに出すバージョンの名（`[0-9A-Za-z._-]` の 1〜32 文字）。
    pub display: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    TooLarge,
    /// 最後の行が `signature <128 文字の 16 進>` の形でない（署名が無い・欠けた）。
    NoSignature,
    /// 公開鍵が使えない（定数の誤り）。
    BadPublicKey,
    /// 署名が合わない（書き換えられた・別の鍵）。
    SignatureMismatch,
    NotUtf8,
    /// 本文の形が違う。`line` は 1 から、`why` は直し方が分かる短い文。
    BadBody {
        line: usize,
        why: String,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::TooLarge => {
                write!(f, "目録が大きすぎる（{MAX_MANIFEST_BYTES} バイトまで）")
            }
            ManifestError::NoSignature => write!(f, "目録に署名の行が無い"),
            ManifestError::BadPublicKey => write!(f, "公開鍵が使えない"),
            ManifestError::SignatureMismatch => write!(f, "目録の署名が合わない"),
            ManifestError::NotUtf8 => write!(f, "目録が UTF-8 でない"),
            ManifestError::BadBody { line, why } => write!(f, "目録の {line} 行目: {why}"),
        }
    }
}

fn bad(line: usize, why: &str) -> ManifestError {
    ManifestError::BadBody {
        line,
        why: why.to_string(),
    }
}

pub fn signing_message(body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIGN_CONTEXT.len() + body.len());
    m.extend_from_slice(SIGN_CONTEXT);
    m.extend_from_slice(body);
    m
}

/// 本文（LF で終わる）に署名の行を足した、配る目録の全体。署名の道具が使う。
pub fn compose_signed(body: &str, signature: &[u8; 64]) -> Result<String, ManifestError> {
    if !body.ends_with('\n') {
        return Err(bad(1, "本文が LF で終わっていない"));
    }
    Ok(format!("{body}signature {}\n", to_hex(signature)))
}

/// 目録の全体を（本文, 署名）に分ける。署名の行が最後の 1 行で、その前までが本文。
fn split_signed(bytes: &[u8]) -> Result<(&[u8], [u8; 64]), ManifestError> {
    let without_last_lf = bytes
        .strip_suffix(b"\n")
        .ok_or(ManifestError::NoSignature)?;
    let line_start = without_last_lf
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let (body, line) = bytes.split_at(line_start);
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let hex = line
        .strip_prefix(b"signature ")
        .ok_or(ManifestError::NoSignature)?;
    let hex = std::str::from_utf8(hex).map_err(|_| ManifestError::NoSignature)?;
    let mut sig = [0u8; 64];
    from_hex_exact(hex, &mut sig).ok_or(ManifestError::NoSignature)?;
    Ok((body, sig))
}

/// 署名を確かめてから本文を読む。`public_key` は 32 バイトの Ed25519 の公開鍵。確かめに通らなければ本文は読まない。
pub fn verify_and_parse(bytes: &[u8], public_key: &[u8; 32]) -> Result<Manifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge);
    }
    let (body, sig) = split_signed(bytes)?;
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| ManifestError::BadPublicKey)?;
    // verify_strict: 署名の形の揺れ（同じ中身の別の署名）と小さい位数の点を断る。
    key.verify_strict(&signing_message(body), &Signature::from_bytes(&sig))
        .map_err(|_| ManifestError::SignatureMismatch)?;
    let text = std::str::from_utf8(body).map_err(|_| ManifestError::NotUtf8)?;
    parse_body(text)
}

/// 本文（署名の行を除いたもの）の形を確かめて読む。署名の道具は署名の前にこれを通す（壊れた目録に署名しない）。
pub fn parse_body(text: &str) -> Result<Manifest, ManifestError> {
    if !text.ends_with('\n') {
        return Err(bad(1, "本文が LF で終わっていない"));
    }
    let mut lines = text.split('\n');
    // 末尾の LF のあとの空要素は、ここで捨てる（それ以外の空行は下で断る）。
    let mut all: Vec<&str> = lines.by_ref().collect();
    all.pop();
    let mut it = all.into_iter().enumerate().map(|(i, l)| (i + 1, l));

    let (n, l) = it.next().ok_or_else(|| bad(1, "空の本文"))?;
    if l != MANIFEST_MAGIC {
        return Err(bad(
            n,
            "1 行目が HiganRTool-manifest 1 でない（知らないバージョンの目録）",
        ));
    }
    let (n, l) = it.next().ok_or_else(|| bad(2, "version の行が無い"))?;
    let version = l
        .strip_prefix("version ")
        .ok_or_else(|| bad(n, "version の行でない"))
        .and_then(|v| {
            parse_decimal_u32(v).ok_or_else(|| bad(n, "version が 1 以上の整数でない"))
        })?;
    let (n, l) = it.next().ok_or_else(|| bad(3, "display の行が無い"))?;
    let display = l
        .strip_prefix("display ")
        .ok_or_else(|| bad(n, "display の行でない"))?;
    if !is_display_text(display) {
        return Err(bad(n, "display は 1〜32 文字の [0-9A-Za-z._-]"));
    }
    let mut files: Vec<FileEntry> = Vec::new();
    for (n, l) in it {
        let rest = l
            .strip_prefix("file ")
            .ok_or_else(|| bad(n, "file の行でない"))?;
        let parts: Vec<&str> = rest.split(' ').collect();
        let [name, size, hash] = parts[..] else {
            return Err(bad(n, "file の行は「file 名前 大きさ SHA-256」の 4 つ"));
        };
        if !is_allowed_name(name) {
            return Err(bad(n, "置き換えてよいファイル名でない"));
        }
        // Windows は大文字小文字を区別しないので、綴りの違うだけの名前も同じファイルとして 2 回と数える。
        if files.iter().any(|f| f.name.eq_ignore_ascii_case(name)) {
            return Err(bad(n, "同じファイルが 2 回ある"));
        }
        let size = parse_decimal_u64(size)
            .filter(|s| (1..=MAX_FILE_BYTES).contains(s))
            .ok_or_else(|| bad(n, "大きさが 1 以上、上限以下の整数でない"))?;
        let mut sha256 = [0u8; 32];
        from_hex_exact(hash, &mut sha256)
            .ok_or_else(|| bad(n, "SHA-256 が小文字の 16 進 64 文字でない"))?;
        files.push(FileEntry {
            name: name.to_string(),
            size,
            sha256,
        });
    }
    for need in REQUIRED_FILES {
        if !files.iter().any(|f| f.name == need) {
            return Err(bad(files.len() + 4, &format!("{need} が載っていない")));
        }
    }
    Ok(Manifest {
        version,
        display: display.to_string(),
        files,
    })
}

fn is_display_text(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 先頭が 0 でない 10 進の数字だけ（`+`・空白・先頭の 0 を断る。0 も断る）。
fn parse_decimal_u32(s: &str) -> Option<u32> {
    parse_decimal_u64(s).and_then(|v| u32::try_from(v).ok())
}

fn parse_decimal_u64(s: &str) -> Option<u64> {
    let mut chars = s.bytes();
    if !matches!(chars.next(), Some(b'1'..=b'9')) || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

// ---- ハッシュと 16 進 ----

/// SHA-256（パッケージ `sha2`）。ローダーはこれで落としたファイルを目録と照らす。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    s
}

/// 小文字の 16 進を `out` の長さちょうどに読む（長さ違い・大文字・16 進でない文字は None）。
pub fn from_hex_exact(s: &str, out: &mut [u8]) -> Option<()> {
    let b = s.as_bytes();
    if b.len() != out.len() * 2 {
        return None;
    }
    let val = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    // 長さは上で偶数と確かめてある（端数 `_` は空）。
    let (pairs, _) = b.as_chunks::<2>();
    for (o, [hi, lo]) in out.iter_mut().zip(pairs) {
        *o = (val(*hi)? << 4) | val(*lo)?;
    }
    Some(())
}

/// 配るバージョンが信じる公開鍵（32 バイト）。定数が 16 進として読めなければ None（検査が落とす）。
pub fn update_public_key() -> Option<[u8; 32]> {
    let mut k = [0u8; 32];
    from_hex_exact(UPDATE_PUBLIC_KEY_HEX, &mut k)?;
    Some(k)
}

pub fn uses_test_key() -> bool {
    UPDATE_PUBLIC_KEY_HEX == TEST_PUBLIC_KEY_HEX
}

// ---- URL ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// `http://宿[:ポート]/道` か `https://…` を分ける。宿は ASCII の文字・数字・`.`・`-` だけ（括弧つきの IPv6 は使わない）。
/// 製品の URL は定数、検査の URL は loopback なので、ネットから来る文字列には使わない。
pub fn parse_url(url: &str) -> Option<Endpoint> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0)?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    let ok_host = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    ok_host.then(|| Endpoint {
        secure,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// 目録の URL。`server` は `UPDATE_SERVER` の形（末尾の `/` は付けても付けなくてもよい）。
pub fn manifest_url(server: &str) -> String {
    format!("{}{MANIFEST_PATH}", server.trim_end_matches('/'))
}

/// ファイルの URL。置き換えてよい名前以外は None（呼ぶ側が名前の検査を忘れても、URL の段階で止める）。
pub fn file_url(server: &str, name: &str) -> Option<String> {
    is_allowed_name(name).then(|| format!("{}{FILES_PATH}{name}", server.trim_end_matches('/')))
}

// ---- 設定（HiganRTool.ini の [loader] update_check）----

/// ini の `[loader]` の `update_check`。`0` で確かめない、`1` か無記入で確かめる。それ以外の値・読めない ini は `Err` で、
/// 呼ぶ側は確かめる側に倒す（書き損じで更新が黙って止まらないように）。ini が無ければ確かめる。
pub fn update_check_enabled(ini_text: Option<&str>) -> Result<bool, String> {
    let Some(text) = ini_text else {
        return Ok(true);
    };
    let ini = crate::ini::parse(text).map_err(|e| format!("HiganRTool.ini が読めない（{e}）"))?;
    match ini.get("loader", "update_check") {
        None | Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(v) => Err(format!(
            "[loader] の update_check の値 {v:?} が読めない（0 か 1）"
        )),
    }
}

/// 検査用の鍵（公開の種）。署名を作る側の部品で、cargo の機能 `test_key` を開けた検査と core 自身の検査でだけ入る。
#[cfg(any(test, feature = "test_key"))]
pub mod test_key {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// 検査用の秘密の鍵の種（公開してよい）。
    pub const SEED: [u8; 32] = *b"HiganRTool-update-TEST-KEY-0001!";

    pub fn public_key() -> [u8; 32] {
        SigningKey::from_bytes(&SEED).verifying_key().to_bytes()
    }

    pub fn sign_with(seed: &[u8; 32], body: &str) -> String {
        let sig = SigningKey::from_bytes(seed).sign(&signing_message(body.as_bytes()));
        compose_signed(body, &sig.to_bytes()).expect("本文が LF で終わっている")
    }

    pub fn sign(body: &str) -> String {
        sign_with(&SEED, body)
    }

    /// 合成の本文（バージョン `version`、exe と dll と README.txt）。ファイルの中身は `file_bytes` で作る。
    pub fn sample_body(version: u32) -> String {
        let mut s = format!("{MANIFEST_MAGIC}\nversion {version}\ndisplay 20261008-abcdef0\n");
        for name in ["HiganRTool.exe", "HiganRTool.dll", "README.txt"] {
            let data = file_bytes(name, version);
            s.push_str(&format!(
                "file {name} {} {}\n",
                data.len(),
                to_hex(&sha256(&data))
            ));
        }
        s
    }

    pub fn file_bytes(name: &str, version: u32) -> Vec<u8> {
        format!("synthetic {name} for build {version}\n")
            .repeat(40)
            .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_key::{self, sample_body, sign, sign_with};
    use super::*;

    fn key() -> [u8; 32] {
        test_key::public_key()
    }

    #[test]
    fn build_stamp_is_the_japanese_time_of_the_minute_count() {
        assert_eq!(build_stamp(0), None);
        assert_eq!(build_stamp(1), Some((2026, 1, 1, 9, 1)));
        assert_eq!(build_stamp(403_920), Some((2026, 10, 8, 21, 0)));
        assert_eq!(build_stamp(403_920 + 180), Some((2026, 10, 9, 0, 0)));
        assert_eq!(build_stamp(403_920 + 179), Some((2026, 10, 8, 23, 59)));
        assert_eq!(build_stamp(1_136_160), Some((2028, 2, 29, 9, 0)));
        assert!(build_stamp(u32::MAX).is_some());
    }

    #[test]
    fn launched_mark_matches_only_the_same_nonzero_build() {
        let t = launched_mark_text(403_920);
        assert_eq!(t, "403920\n");
        assert!(launched_mark_matches(Some(&t), 403_920));
        assert!(!launched_mark_matches(Some(&t), 403_921));
        assert!(!launched_mark_matches(None, 403_920));
        assert!(!launched_mark_matches(Some("0\n"), 0));
        assert!(!launched_mark_matches(Some(""), 0));
        assert!(!launched_mark_matches(Some("garbage"), 403_920));
        assert!(!launched_mark_matches(Some("-1"), 403_920));
    }

    #[test]
    fn public_key_constants_are_well_formed() {
        assert_eq!(TEST_PUBLIC_KEY_HEX, to_hex(&key()));
        let k = update_public_key().expect("32 バイトの 16 進");
        assert!(VerifyingKey::from_bytes(&k).is_ok(), "曲線の点でない");
    }

    #[test]
    fn a_signed_manifest_is_read() {
        let body = sample_body(777);
        let m = verify_and_parse(sign(&body).as_bytes(), &key()).expect("読める");
        assert_eq!(m.version, 777);
        assert_eq!(m.display, "20261008-abcdef0");
        let names: Vec<&str> = m.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["HiganRTool.exe", "HiganRTool.dll", "README.txt"]);
        let exe = test_key::file_bytes("HiganRTool.exe", 777);
        assert_eq!(m.files[0].size, exe.len() as u64);
        assert_eq!(m.files[0].sha256, sha256(&exe));
    }

    #[test]
    fn tampered_or_foreign_signatures_are_refused() {
        let body = sample_body(5);
        let good = sign(&body);
        assert!(verify_and_parse(good.as_bytes(), &key()).is_ok());
        let evil = good.replacen("version 5\n", "version 6\n", 1);
        assert_eq!(
            verify_and_parse(evil.as_bytes(), &key()),
            Err(ManifestError::SignatureMismatch)
        );
        let other = sign_with(&[7u8; 32], &body);
        assert_eq!(
            verify_and_parse(other.as_bytes(), &key()),
            Err(ManifestError::SignatureMismatch)
        );
        let mut bytes = good.clone().into_bytes();
        let i = bytes.len() - 5;
        bytes[i] = if bytes[i] == b'0' { b'1' } else { b'0' };
        assert!(verify_and_parse(&bytes, &key()).is_err());
        use ed25519_dalek::{Signer, SigningKey};
        let raw = SigningKey::from_bytes(&test_key::SEED).sign(body.as_bytes());
        let no_ctx = compose_signed(&body, &raw.to_bytes()).unwrap();
        assert_eq!(
            verify_and_parse(no_ctx.as_bytes(), &key()),
            Err(ManifestError::SignatureMismatch)
        );
    }

    #[test]
    fn missing_or_malformed_signature_lines_are_refused() {
        let body = sample_body(5);
        for bad in [
            body.clone(),
            format!("{body}signature \n"),
            format!("{body}signature {}\n", "A".repeat(128)),
            format!("{body}signature {}\n", "a".repeat(127)),
            format!("{body}signature {}", "a".repeat(128)),
            format!("{body}sig {}\n", "a".repeat(128)),
            String::new(),
            "\n".to_string(),
        ] {
            assert_eq!(
                verify_and_parse(bad.as_bytes(), &key()),
                Err(ManifestError::NoSignature),
                "{bad:?}"
            );
        }
        let tail = format!("{}file HiganRTool.ini 1 {}\n", sign(&body), "0".repeat(64));
        assert!(verify_and_parse(tail.as_bytes(), &key()).is_err());
        let huge = format!("{}{}", "x\n".repeat(MAX_MANIFEST_BYTES), sign(&body));
        assert_eq!(
            verify_and_parse(huge.as_bytes(), &key()),
            Err(ManifestError::TooLarge)
        );
    }

    #[test]
    fn signed_but_malformed_bodies_are_refused() {
        let good = sample_body(9);
        let sha = to_hex(&sha256(b"x"));
        let exe = format!("file HiganRTool.exe 10 {sha}\n");
        let dll = format!("file HiganRTool.dll 10 {sha}\n");
        let head = format!("{MANIFEST_MAGIC}\nversion 9\ndisplay v1\n");
        let cases: Vec<(String, &str)> = vec![
            (
                good.replace("manifest 1", "manifest 2"),
                "知らないバージョン",
            ),
            (good.replace("version 9", "version 0"), "version 0"),
            (good.replace("version 9", "version 09"), "先頭 0"),
            (good.replace("version 9", "version +9"), "符号"),
            (good.replace("version 9", "version 4294967296"), "u32 超え"),
            (
                good.replace("display 20261008", "display a b"),
                "表示に空白",
            ),
            (
                good.replace("display 20261008-abcdef0", "display "),
                "表示が空",
            ),
            (
                good.replace("display 20261008", "display \u{3042}"),
                "表示に非 ASCII",
            ),
            (format!("{head}{exe}{dll}{exe}"), "同じファイル"),
            (
                format!("{head}file HiganRTool.exe 0 {sha}\n{dll}"),
                "大きさ 0",
            ),
            (
                format!(
                    "{head}file HiganRTool.exe {} {sha}\n{dll}",
                    MAX_FILE_BYTES + 1
                ),
                "上限超え",
            ),
            (
                format!("{head}file HiganRTool.exe 10 {}\n{dll}", sha.to_uppercase()),
                "大文字",
            ),
            (
                format!("{head}file HiganRTool.exe 10 {}\n{dll}", &sha[1..]),
                "短い",
            ),
            (format!("{head}{exe}"), "dll が無い"),
            (format!("{head}{dll}"), "exe が無い"),
            (format!("{head}{exe}{dll}\n"), "空行"),
            (format!("{head}{exe}{dll}extra 1\n"), "知らない行"),
            (
                format!("{head}file  HiganRTool.exe 10 {sha}\n{dll}"),
                "空白が 2 つ",
            ),
            (good.replace('\n', "\r\n"), "CR"),
            (
                format!("version 9\n{MANIFEST_MAGIC}\ndisplay v1\n{exe}{dll}"),
                "行の順",
            ),
        ];
        for (body, what) in cases {
            let signed = sign(&body);
            assert!(
                matches!(
                    verify_and_parse(signed.as_bytes(), &key()),
                    Err(ManifestError::BadBody { .. })
                ),
                "{what}: 通った/別の誤り {:?}",
                verify_and_parse(signed.as_bytes(), &key())
            );
        }
        // 比べ用: 上の head ＋ exe ＋ dll は通る（誤りの印が別の所から来ていない）。
        assert!(verify_and_parse(sign(&format!("{head}{exe}{dll}")).as_bytes(), &key()).is_ok());
    }

    #[test]
    fn names_that_follow_the_rule_pass_and_the_rest_are_refused_even_when_signed() {
        let sha = to_hex(&sha256(b"x"));
        let exe = format!("file HiganRTool.exe 10 {sha}\n");
        let dll = format!("file HiganRTool.dll 10 {sha}\n");
        let manifest_with = |name: &str| {
            let body = format!(
                "{MANIFEST_MAGIC}\nversion 9\ndisplay v1\n{exe}{dll}file {name} 10 {sha}\n"
            );
            verify_and_parse(sign(&body).as_bytes(), &key())
        };
        for ok in NAME_RULE_ACCEPT {
            assert!(is_allowed_name(ok), "{ok:?}");
            // 配る表に無い名前（NOTICE.txt など）も、署名つきの目録なら受ける。
            let m = manifest_with(ok);
            if ok == "HiganRTool.exe" || ok == "HiganRTool.dll" {
                assert!(m.is_err(), "{ok}: 同じ名前が 2 回あるのに通した");
            } else {
                assert!(m.is_ok(), "{ok}: {m:?}");
            }
            assert!(file_url("https://s.example", ok).is_some(), "{ok:?}");
        }
        for bad in NAME_RULE_REJECT {
            assert!(!is_allowed_name(bad), "{bad:?}");
            assert_eq!(file_url("https://s.example", bad), None, "{bad:?}");
            if !bad.contains(' ') && !bad.is_empty() {
                assert!(
                    matches!(manifest_with(bad), Err(ManifestError::BadBody { .. })),
                    "署名つきの目録で {bad:?} を通した（または別の誤り）"
                );
            }
        }
        assert_eq!(
            file_url("https://s.example/", "README.txt").as_deref(),
            Some("https://s.example/update/files/README.txt")
        );
    }

    /// サーバー（`src/server/test/update.test.ts` の `ACCEPT` / `REJECT`）と同じ表。`tools/make_dist.py` が 2 つの一致を見る。
    const NAME_RULE_ACCEPT: [&str; 17] = [
        "HiganRTool.exe",
        "HiganRTool.dll",
        "README.txt",
        "README_en.txt",
        "LICENSE.txt",
        "THIRD_PARTY_LICENSES.txt",
        "NOTICE.txt",
        "a",
        "A-b_c.d",
        "x.y.z",
        "logs.txt",
        "extra_notes.txt",
        "oldfile",
        "x.older",
        "console.txt",
        "COM10.txt",
        "COM.txt",
    ];
    const NAME_RULE_REJECT: [&str; 40] = [
        "",
        ".",
        "..",
        ".hidden",
        "x.",
        "a..b",
        "a/b",
        "a\\b",
        "..\\HiganRTool.exe",
        "../HiganRTool.exe",
        "C:x.dll",
        "x:y",
        "HiganRTool.exe:stream",
        "HiganRTool.exe ",
        "a b",
        "日本語.txt",
        "Ａ.txt",
        "%2e%2e",
        "HiganRTool.ini",
        "HIGANRTOOL.INI",
        "higanrtool.Ini",
        "game_path.txt",
        "update_launched.txt",
        "manifest.txt",
        "Manifest.unsigned.txt",
        "x.old",
        "HiganRTool.exe.old",
        "HiganRTool.dll.new",
        "x.NEW",
        "logs",
        "LOGS",
        "Code",
        "th09.exe",
        "TH09.EXE",
        "NUL",
        "con",
        "Aux.txt",
        "COM1",
        "lpt9.log",
        "logs\\x.csv",
    ];

    #[test]
    fn the_name_length_limit_is_64_characters() {
        assert!(is_allowed_name(&"a".repeat(MAX_NAME_LEN)));
        assert!(!is_allowed_name(&"a".repeat(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn a_manifest_without_the_exe_or_the_dll_is_refused_even_with_new_names() {
        let sha = to_hex(&sha256(b"x"));
        let line = |n: &str| format!("file {n} 10 {sha}\n");
        let head = format!("{MANIFEST_MAGIC}\nversion 9\ndisplay v1\n");
        for body in [
            format!("{head}{}{}", line("HiganRTool.exe"), line("NOTICE.txt")),
            format!("{head}{}{}", line("HiganRTool.dll"), line("NOTICE.txt")),
            format!("{head}{}", line("NOTICE.txt")),
        ] {
            assert!(
                matches!(
                    verify_and_parse(sign(&body).as_bytes(), &key()),
                    Err(ManifestError::BadBody { .. })
                ),
                "{body}"
            );
        }
        let ok = format!(
            "{head}{}{}{}",
            line("HiganRTool.exe"),
            line("HiganRTool.dll"),
            line("NOTICE.txt")
        );
        let m = verify_and_parse(sign(&ok).as_bytes(), &key()).expect("新しい名前を含む目録");
        assert_eq!(m.files.len(), 3);
    }

    #[test]
    fn names_that_differ_only_in_case_count_as_the_same_file() {
        let sha = to_hex(&sha256(b"x"));
        let line = |n: &str| format!("file {n} 10 {sha}\n");
        let body = format!(
            "{MANIFEST_MAGIC}\nversion 9\ndisplay v1\n{}{}{}{}",
            line("HiganRTool.exe"),
            line("HiganRTool.dll"),
            line("NOTICE.txt"),
            line("notice.TXT")
        );
        assert!(verify_and_parse(sign(&body).as_bytes(), &key()).is_err());
    }

    #[test]
    fn shipped_files_all_follow_the_rule() {
        for n in SHIPPED_FILES {
            assert!(is_allowed_name(n), "{n}");
        }
        for n in REQUIRED_FILES {
            assert!(SHIPPED_FILES.contains(&n));
        }
    }

    #[test]
    fn only_a_strictly_newer_version_counts() {
        assert!(is_newer(10, 11));
        assert!(is_newer(0, 1));
        assert!(!is_newer(10, 10), "同じバージョンを新しいと言った");
        assert!(!is_newer(10, 9));
        assert!(!is_newer(u32::MAX, u32::MAX));
    }

    #[test]
    fn build_number_parser() {
        assert_eq!(parse_build_number(None), 0);
        assert_eq!(parse_build_number(Some("")), 0);
        assert_eq!(parse_build_number(Some("399360")), 399_360);
        assert_eq!(parse_build_number(Some("4294967295")), u32::MAX);
        for bad in [
            "4294967296",
            "12a",
            "-1",
            " 1",
            "1 ",
            "+1",
            "9999999999999999999999",
        ] {
            assert_eq!(parse_build_number(Some(bad)), 0, "{bad:?}");
        }
    }

    #[test]
    fn hex_round_trip_and_strictness() {
        let d = sha256(b"abc");
        assert_eq!(
            to_hex(&d),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut back = [0u8; 32];
        assert!(from_hex_exact(&to_hex(&d), &mut back).is_some());
        assert_eq!(back, d);
        for bad in [
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "ba78",
            &format!("{}g", "a".repeat(63)),
        ] {
            assert!(from_hex_exact(bad, &mut back).is_none(), "{bad}");
        }
    }

    #[test]
    fn url_parts() {
        assert_eq!(
            parse_url("https://a.example/update/manifest"),
            Some(Endpoint {
                secure: true,
                host: "a.example".into(),
                port: 443,
                path: "/update/manifest".into()
            })
        );
        assert_eq!(
            parse_url("http://127.0.0.1:8123"),
            Some(Endpoint {
                secure: false,
                host: "127.0.0.1".into(),
                port: 8123,
                path: "/".into()
            })
        );
        for bad in [
            "ftp://a/",
            "a.example/x",
            "http://",
            "http://:80/",
            "http://a:0/",
            "http://a:99999/",
            "http://a b/",
        ] {
            assert_eq!(parse_url(bad), None, "{bad}");
        }
        assert_eq!(
            manifest_url("https://s.example/"),
            "https://s.example/update/manifest"
        );
        assert!(manifest_url(UPDATE_SERVER).ends_with(MANIFEST_PATH));
        assert!(parse_url(&manifest_url(UPDATE_SERVER)).is_some_and(|e| e.secure));
    }

    #[test]
    fn update_check_setting() {
        assert_eq!(update_check_enabled(None), Ok(true));
        assert_eq!(
            update_check_enabled(Some("[netplay]\nenable = 1\n")),
            Ok(true)
        );
        assert_eq!(
            update_check_enabled(Some("[loader]\nupdate_check = 1\n")),
            Ok(true)
        );
        assert_eq!(
            update_check_enabled(Some("[loader]\nupdate_check = 0\n")),
            Ok(false)
        );
        assert_eq!(
            update_check_enabled(Some("[Loader]\nUpdate_Check = 0\n")),
            Ok(false)
        );
        assert_eq!(
            update_check_enabled(Some("[netplay]\nupdate_check = 0\n")),
            Ok(true)
        );
        assert!(update_check_enabled(Some("[loader]\nupdate_check = off\n")).is_err());
        assert!(update_check_enabled(Some("[loader\nupdate_check = 0\n")).is_err());
    }

    #[test]
    fn the_body_made_by_make_dist_is_read() {
        let text =
            include_str!("../../../tests/fixtures/update/manifest_body.txt").replace("\r\n", "\n");
        let m = parse_body(&text).expect("make_dist の本文が読める");
        assert_eq!(m.version, 399_360);
        assert_eq!(m.display, "20261008-abcdef0");
        assert_eq!(m.files.len(), 3);
        for (f, size) in m.files.iter().zip([25u64, 25, 21]) {
            let data = format!("synthetic {}\n", f.name).into_bytes();
            assert_eq!(f.size, size, "{}", f.name);
            assert_eq!(f.size, data.len() as u64);
            assert_eq!(f.sha256, sha256(&data), "{}", f.name);
        }
        assert!(verify_and_parse(sign(&text).as_bytes(), &key()).is_ok());
    }

    #[test]
    fn compose_requires_a_lf_terminated_body() {
        assert!(compose_signed("abc", &[0u8; 64]).is_err());
        let s = compose_signed("abc\n", &[0xabu8; 64]).unwrap();
        assert_eq!(s, format!("abc\nsignature {}\n", "ab".repeat(64)));
    }
}
