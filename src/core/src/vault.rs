//! 保管庫（`Replay_HiganRTool\`）の名前を決める。ファイルには触らない（フォルダの中の名前の一覧は dll が渡し、
//! 作るのも書くのも dll）。保存はゲーム自身の `CreateFileA` を通るので、名前は CP932（Shift-JIS）で書ける文字だけにする。
//! 判定と変換は `encoding_rs`。その `SHIFT_JIS` は WHATWG の定義で、Windows の CP932 の表に合わせたもの
//! （NEC・IBM の重複の文字などの細部の一致は確かめていない）。

use encoding_rs::SHIFT_JIS;

use crate::replay::Timestamp;

/// 無害化した名前の上限（CP932 のバイト数）。
pub const NAME_MAX_CP932: usize = 32;
/// 無害化して空になったときの名前。
pub const NONAME: &str = "noname";
pub const UD_MAX: u16 = 9999;
pub const PACK50_FOLDER_MAX: u16 = 9999;
/// pack50 の 1 フォルダの半分（`th9_01`〜`th9_25` と `th9_ud0001`〜`th9_ud0025`）。25 はゲームの一覧の上限（`0x00426DAB` の `cmp $0x19`）。
pub const PACK50_HALF: u16 = 25;

/// `_` に置き換える文字（制御文字は別に見る）。
const FORBIDDEN_CHARS: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// `_` に置き換える制御文字: C0（U+0000〜U+001F）・DEL（U+007F）・C1（U+0080〜U+009F）。C1 は CP932 では U+0080 ↔ `0x80` の
/// ように往復してしまい、往復の判定では落ちないので、ここで落とす。
fn is_control(c: char) -> bool {
    let u = c as u32;
    u <= 0x1F || (0x7F..=0x9F).contains(&u)
}

/// Windows の予約名（大小文字を問わない。最初の `.` の前がこれなら同じ扱い）。`COM0` と `LPT0` と `CONIN$` `CONOUT$` も入れる
/// （`COM0` が予約かは確かめていないが、足して困るのは名前が `COM0` の人だけで、`_` が 1 文字付くだけなので疑わしい側へ倒した）。
fn is_reserved_stem(stem: &str) -> bool {
    const FIXED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    if FIXED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return true;
    }
    let b = stem.as_bytes();
    b.len() == 4
        && (b[..3].eq_ignore_ascii_case(b"COM") || b[..3].eq_ignore_ascii_case(b"LPT"))
        && b[3].is_ascii_digit()
}

/// 1 文字を CP932 に符号化して戻したとき、同じ文字に戻るか。CP932 に無い文字（絵文字など）に加え、`¥`（U+00A5 → `0x5C` = `\`）や
/// `‾`（U+203E → `0x7E`）のように別の文字に落ちるものも偽になる（`\` に落ちるとパスの区切りになる）。
fn round_trips(c: char) -> bool {
    let mut buf = [0u8; 4];
    let s: &str = c.encode_utf8(&mut buf);
    let (bytes, _, had_errors) = SHIFT_JIS.encode(s);
    if had_errors {
        return false;
    }
    let (back, decode_errors) = SHIFT_JIS.decode_without_bom_handling(&bytes);
    !decode_errors && back == s
}

fn cp932_len(c: char) -> usize {
    let mut buf = [0u8; 4];
    SHIFT_JIS.encode(c.encode_utf8(&mut buf)).0.len()
}

/// `s` を CP932 のバイト列にする（dll がパスを作るため）。往復しない文字があれば `None`（そのときは保存しない）。
pub fn to_cp932(s: &str) -> Option<Vec<u8>> {
    let (bytes, _, had_errors) = SHIFT_JIS.encode(s);
    if had_errors {
        return None;
    }
    let (back, decode_errors) = SHIFT_JIS.decode_without_bom_handling(&bytes);
    if decode_errors || back != s {
        return None;
    }
    Some(bytes.into_owned())
}

/// 末尾の半角空白と `.` を落とす（Windows が黙って落とすので、落ちた名前と食い違わないように）。全角空白は落とさないので残す。
fn trim_tail(s: &mut String) {
    let n = s.trim_end_matches([' ', '.']).len();
    s.truncate(n);
}

fn truncate_cp932(s: &mut String, max: usize) {
    let mut used = 0usize;
    let mut cut = s.len();
    for (i, c) in s.char_indices() {
        let n = cp932_len(c);
        if used + n > max {
            cut = i;
            break;
        }
        used += n;
    }
    s.truncate(cut);
}

/// 予約名なら、最初の `.` の前の直後に `_` を差し込む。`CON.txt_` は最初の `.` の前がまだ `CON` なので、`CON_.txt` にする。
fn fix_reserved(s: &mut String) {
    let stem_end = s.find('.').unwrap_or(s.len());
    let stem = s[..stem_end].trim_end_matches(' ');
    if is_reserved_stem(stem) {
        let at = stem.len();
        s.insert(at, '_');
    }
}

/// 名前の無害化。入力は UTF-8 の名前、出力は CP932 で往復できる文字だけの、フォルダ名に使える名前。
///
/// 順序: 文字の置き換え → 末尾の空白と `.` を落とす → 32 バイトに切る → もう一度末尾を落とす → 空なら `noname` →
/// 予約名なら `_` → `_` で 32 バイトを超えたらもう一度切って末尾を落とす。予約名の確かめを切った後に置くのは、
/// 切って末尾を落とした結果が予約名になることがあるため（例: `COM1` ＋ 空白 28 個 ＋ `x`）。
pub fn sanitize_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if is_control(c) || FORBIDDEN_CHARS.contains(&c) || !round_trips(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    trim_tail(&mut s);
    truncate_cp932(&mut s, NAME_MAX_CP932);
    trim_tail(&mut s);
    if s.is_empty() {
        return NONAME.to_string();
    }
    fix_reserved(&mut s);
    // 予約名の最初の `.` の前は 4 文字 ＋ `_` なので、ここで切っても `_` までは残る。
    truncate_cp932(&mut s, NAME_MAX_CP932);
    trim_tail(&mut s);
    s
}

/// 保管庫の区切り方（`HiganRTool.ini` で選ぶ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Peer,
    Date,
    Flat,
    Pack50,
}

impl Split {
    /// 前後の空白を除き、大小文字を問わない。知らない値は `None`。
    pub fn parse(value: &str) -> Option<Split> {
        let v = value.trim();
        [
            ("peer", Split::Peer),
            ("date", Split::Date),
            ("flat", Split::Flat),
            ("pack50", Split::Pack50),
        ]
        .into_iter()
        .find(|(name, _)| v.eq_ignore_ascii_case(name))
        .map(|(_, split)| split)
    }

    /// 無い・知らない値は既定の `peer`。知らない値をログに出したいなら、dll は先に `parse` を呼んで `None` を見る。
    pub fn from_ini(value: Option<&str>) -> Split {
        value.and_then(Split::parse).unwrap_or(Split::Peer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Folder {
    /// `Replay_HiganRTool\` の直下（`flat`）。
    Root,
    /// `Replay_HiganRTool\<名前>\`（`peer` / `date`）。
    Sub(String),
    /// 番号のフォルダ。中身の一覧が要るので `pack50_last_folder` と `pack50_next` で決める。
    Pack50,
}

/// `peer` のフォルダ名: `<1P の名前> vs <2P の名前>`（無害化した名前。1P が先 —— adonis と同じ）。
pub fn peer_folder_name(p1: &str, p2: &str) -> String {
    format!("{} vs {}", sanitize_name(p1), sanitize_name(p2))
}

/// `date` のフォルダ名: `YYYY-MM-DD`（マッチの終わりの手元の日付）。
pub fn date_folder_name(date: &Timestamp) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

pub fn pack50_folder_name(n: u16) -> String {
    format!("{n:04}")
}

/// 区切り方ごとのフォルダ。`p1` / `p2` は無害化の前の名前、`date` はマッチの終わりの手元の時刻。
pub fn folder_for(split: Split, p1: &str, p2: &str, date: &Timestamp) -> Folder {
    match split {
        Split::Peer => Folder::Sub(peer_folder_name(p1, p2)),
        Split::Date => Folder::Sub(date_folder_name(date)),
        Split::Flat => Folder::Root,
        Split::Pack50 => Folder::Pack50,
    }
}

fn fixed_digits(s: &str, width: usize) -> Option<u16> {
    if s.len() != width || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `th9_udNNNN.rpy`（大小文字を問わない。NNNN は 4 桁の 0001〜9999）の番号。形が違えば `None`。
fn ud_number(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let n = fixed_digits(lower.strip_prefix("th9_ud")?.strip_suffix(".rpy")?, 4)?;
    (1..=UD_MAX).contains(&n).then_some(n)
}

/// `th9_NN.rpy`（大小文字を問わない。NN は 2 桁の 01〜25）の番号。ゲームの `"replay/th9_%.2d.rpy"`（`0x004297F1`）と同じ形。
fn slot_number(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let n = fixed_digits(lower.strip_prefix("th9_")?.strip_suffix(".rpy")?, 2)?;
    (1..=PACK50_HALF).contains(&n).then_some(n)
}

fn ud_name(n: u16) -> String {
    format!("th9_ud{n:04}.rpy")
}

fn slot_name(n: u16) -> String {
    format!("th9_{n:02}.rpy")
}

/// `names` で使われていない一番小さい番号の `th9_udNNNN.rpy`。形でない名前は数えない。9999 まで埋まっていたら `None`。
pub fn next_ud_name<S: AsRef<str>>(names: &[S]) -> Option<String> {
    let mut used = vec![false; UD_MAX as usize + 1];
    for n in names.iter().filter_map(|s| ud_number(s.as_ref())) {
        used[n as usize] = true;
    }
    (1..=UD_MAX).find(|&n| !used[n as usize]).map(ud_name)
}

/// `pack50` の最後の番号のフォルダ。4 桁の数字ちょうどで 0001〜9999 の名前だけを数える。1 つも無ければ `None`。
pub fn pack50_last_folder<S: AsRef<str>>(dir_names: &[S]) -> Option<u16> {
    dir_names
        .iter()
        .filter_map(|s| fixed_digits(s.as_ref(), 4))
        .filter(|n| (1..=PACK50_FOLDER_MAX).contains(n))
        .max()
}

/// `pack50` の次の（フォルダ番号, ファイル名）。`last` は最後の番号のフォルダ、`names_in_last` はその中の名前の一覧。
/// `th9_01`〜`th9_25` の空きの一番小さい方 → `th9_ud0001`〜`th9_ud0025` の空きの一番小さい方 → どちらも埋まっていたら
/// 次の番号のフォルダの `th9_01.rpy`（フォルダが 1 つも無ければ `0001`）。フォルダ番号が 9999 を超えるなら `None`。
pub fn pack50_next<S: AsRef<str>>(last: Option<u16>, names_in_last: &[S]) -> Option<(u16, String)> {
    let Some(folder) = last else {
        return Some((1, slot_name(1)));
    };
    let mut slots = [false; PACK50_HALF as usize + 1];
    let mut uds = [false; PACK50_HALF as usize + 1];
    for name in names_in_last {
        let name = name.as_ref();
        if let Some(n) = slot_number(name) {
            slots[n as usize] = true;
        }
        if let Some(n) = ud_number(name).filter(|&n| n <= PACK50_HALF) {
            uds[n as usize] = true;
        }
    }
    if let Some(n) = (1..=PACK50_HALF).find(|&n| !slots[n as usize]) {
        return Some((folder, slot_name(n)));
    }
    if let Some(n) = (1..=PACK50_HALF).find(|&n| !uds[n as usize]) {
        return Some((folder, ud_name(n)));
    }
    let next = folder.checked_add(1).filter(|&n| n <= PACK50_FOLDER_MAX)?;
    Some((next, slot_name(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::iter::empty;

    fn is_reserved_name(s: &str) -> bool {
        let stem = s[..s.find('.').unwrap_or(s.len())].trim_end_matches(' ');
        is_reserved_stem(stem)
    }

    #[test]
    fn reserved_names_get_an_underscore_after_the_stem() {
        assert_eq!(sanitize_name("CON"), "CON_");
        assert_eq!(sanitize_name("con"), "con_");
        assert_eq!(sanitize_name("CON.txt"), "CON_.txt");
        assert_eq!(sanitize_name("COM1"), "COM1_");
        assert_eq!(sanitize_name("LPT9"), "LPT9_");
        assert_eq!(sanitize_name("com0"), "com0_");
        assert_eq!(sanitize_name("CON "), "CON_");
        assert_eq!(sanitize_name("CON .txt"), "CON_ .txt");
        assert_eq!(sanitize_name("nul.tar.gz"), "nul_.tar.gz");
        assert_eq!(sanitize_name("Aux"), "Aux_");
        assert_eq!(sanitize_name("prn"), "prn_");
        assert_eq!(sanitize_name("COM10"), "COM10");
        assert_eq!(sanitize_name("CONSOLE"), "CONSOLE");
        assert_eq!(sanitize_name("xCON"), "xCON");
        assert_eq!(sanitize_name("COMx"), "COMx");
        // 切って末尾を落とした結果が予約名になる形（順序の検査）。
        let tricky = format!("COM1{}x", " ".repeat(28));
        assert_eq!(tricky.len(), 33);
        assert_eq!(sanitize_name(&tricky), "COM1_");
        let long = format!("CON.{}", "a".repeat(28));
        assert_eq!(long.len(), 32);
        assert_eq!(sanitize_name(&long), format!("CON_.{}", "a".repeat(27)));
    }

    #[test]
    fn forbidden_and_control_chars_become_underscores() {
        assert_eq!(
            sanitize_name(r#"a\b/c:d*e?f"g<h>i|j"#),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_name("a\tb\u{7F}c\u{0}d\u{1F}e\r\n"), "a_b_c_d_e__");
        assert_eq!(sanitize_name("a%b=c~d-e_f#g"), "a%b=c~d-e_f#g");
    }

    #[test]
    fn c1_control_chars_become_underscores() {
        assert!(round_trips('\u{80}'), "前提: U+0080 は CP932 で往復する");
        assert_eq!(sanitize_name("a\u{80}b"), "a_b");
        assert_eq!(sanitize_name("\u{80}\u{85}\u{9F}x"), "___x");
        assert_eq!(sanitize_name("~"), "~");
        assert!(is_control('\u{9F}') && !is_control('\u{A0}') && !is_control('~'));
    }

    #[test]
    fn console_names_are_reserved_too() {
        assert_eq!(sanitize_name("CONIN$"), "CONIN$_");
        assert_eq!(sanitize_name("conout$"), "conout$_");
        assert_eq!(sanitize_name("CONOUT$.log"), "CONOUT$_.log");
        assert_eq!(sanitize_name("ConIn$ "), "ConIn$_");
        assert_eq!(sanitize_name("CONIN$ .x"), "CONIN$_ .x");
        assert_eq!(sanitize_name("CONIN"), "CONIN");
        assert_eq!(sanitize_name("CONOUT$x"), "CONOUT$x");
    }

    #[test]
    fn trailing_dots_and_spaces_are_dropped_and_empty_becomes_noname() {
        assert_eq!(sanitize_name("name. . "), "name");
        assert_eq!(sanitize_name(" lead"), " lead");
        assert_eq!(sanitize_name("x\u{3000}"), "x\u{3000}");
        assert_eq!(sanitize_name(""), NONAME);
        assert_eq!(sanitize_name(".."), NONAME);
        assert_eq!(sanitize_name(" . "), NONAME);
        assert_eq!(sanitize_name("..a"), "..a");
    }

    #[test]
    fn chars_that_do_not_round_trip_through_cp932_are_replaced() {
        assert_eq!(sanitize_name("¥100"), "_100");
        assert_eq!(sanitize_name("a‾b"), "a_b");
        assert_eq!(sanitize_name("1−2"), "1_2");
        assert_eq!(sanitize_name("a😀b"), "a_b");
        assert_eq!(sanitize_name("한"), "_");
        assert_eq!(sanitize_name("博麗ﾚｲﾑ～"), "博麗ﾚｲﾑ～");
        assert_eq!(sanitize_name("①Ⅱ㈱"), "①Ⅱ㈱");
    }

    #[test]
    fn length_is_cut_at_32_cp932_bytes_without_splitting_a_char() {
        let full16 = "あ".repeat(16); // CP932 で 32 バイト
        assert_eq!(sanitize_name(&full16), full16);
        assert_eq!(sanitize_name(&"あ".repeat(17)), full16);
        assert_eq!(
            sanitize_name(&format!("a{}", "あ".repeat(16))),
            format!("a{}", "あ".repeat(15))
        );
        let kana = "ｱ".repeat(40);
        assert_eq!(sanitize_name(&kana), "ｱ".repeat(32));
        let dot = format!("{}.b", "a".repeat(31));
        assert_eq!(sanitize_name(&dot), "a".repeat(31));
        assert_eq!(sanitize_name(&"😀".repeat(40)), "_".repeat(32));
    }

    #[test]
    fn sanitized_output_always_round_trips_and_is_stable() {
        let ranges: [(u32, u32); 9] = [
            (0x00, 0x7F),
            (0x80, 0xFF),
            (0x2000, 0x22FF),
            (0x3000, 0x30FF),
            (0x4E00, 0x9FFF),
            (0xFF00, 0xFFEF),
            (0x1F600, 0x1F64F),
            (0xE000, 0xE0FF),
            (0xAC00, 0xACFF),
        ];
        let mut x: u32 = 0x2545_F491;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut samples: Vec<String> = [
            "CON",
            "..",
            "",
            "¥",
            "COM1 .x",
            "a. . .",
            "\u{80}",
            "CONIN$.\u{85}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for _ in 0..3000 {
            let len = next() % 45;
            let s: String = (0..len)
                .filter_map(|_| {
                    let (lo, hi) = ranges[(next() % ranges.len() as u32) as usize];
                    char::from_u32(lo + next() % (hi - lo + 1))
                })
                .collect();
            samples.push(s);
        }
        // C1 を含む標本が混ざっていること（0 本なら C1 の性質は測れていない）。
        let with_c1 = samples
            .iter()
            .filter(|s| s.chars().any(|c| ('\u{80}'..='\u{9F}').contains(&c)))
            .count();
        assert!(with_c1 > 100, "C1 を含む標本 {with_c1} / {}", samples.len());
        for s in &samples {
            let out = sanitize_name(s);
            let bytes = to_cp932(&out).unwrap_or_else(|| panic!("{s:?} → {out:?} が往復しない"));
            assert!(
                !out.is_empty() && bytes.len() <= NAME_MAX_CP932,
                "{s:?} → {out:?}"
            );
            assert!(!out.ends_with([' ', '.']), "{s:?} → {out:?}");
            assert!(!is_reserved_name(&out), "{s:?} → {out:?}");
            assert!(
                !out.chars().any(|c| c <= '\u{1F}'
                    || ('\u{7F}'..='\u{9F}').contains(&c)
                    || FORBIDDEN_CHARS.contains(&c)),
                "{s:?} → {out:?}"
            );
            assert_eq!(sanitize_name(&out), out, "2 回目で変わる: {s:?}");
        }
    }

    #[test]
    fn to_cp932_gives_the_bytes_or_refuses() {
        assert_eq!(to_cp932("ア"), Some(vec![0x83, 0x41]));
        assert_eq!(to_cp932("ｱ"), Some(vec![0xB1]));
        assert_eq!(to_cp932(r"C:\a b\x.rpy"), Some(br"C:\a b\x.rpy".to_vec()));
        assert_eq!(to_cp932("¥"), None);
        assert_eq!(to_cp932("a😀"), None);
        assert_eq!(to_cp932(""), Some(vec![]));
    }

    #[test]
    fn split_reads_the_ini_value_with_peer_as_default() {
        assert_eq!(Split::from_ini(None), Split::Peer);
        assert_eq!(Split::from_ini(Some("")), Split::Peer);
        assert_eq!(Split::from_ini(Some("pack")), Split::Peer);
        assert_eq!(Split::from_ini(Some("DATE")), Split::Date);
        assert_eq!(Split::from_ini(Some(" flat\r")), Split::Flat);
        assert_eq!(Split::from_ini(Some("Pack50")), Split::Pack50);
        assert_eq!(Split::from_ini(Some("peer")), Split::Peer);
        assert_eq!(Split::parse("pack5"), None);
    }

    #[test]
    fn folders_follow_the_split() {
        let at = Timestamp {
            year: 2026,
            month: 9,
            day: 5,
            hour: 1,
            minute: 2,
            second: 3,
        };
        assert_eq!(
            folder_for(Split::Peer, "CON", "a/b.", &at),
            Folder::Sub("CON_ vs a_b".to_string())
        );
        assert_eq!(
            folder_for(Split::Peer, "", "二人目", &at),
            Folder::Sub("noname vs 二人目".to_string())
        );
        assert_eq!(
            folder_for(Split::Peer, "a@b", "c_d@", &at),
            Folder::Sub("a@b vs c_d@".to_string()),
            "プレイヤー名の @ はフォルダ名で _ に変わらない"
        );
        assert_eq!(
            folder_for(Split::Date, "x", "y", &at),
            Folder::Sub("2026-09-05".to_string())
        );
        assert_eq!(folder_for(Split::Flat, "x", "y", &at), Folder::Root);
        assert_eq!(folder_for(Split::Pack50, "x", "y", &at), Folder::Pack50);
        assert_eq!(pack50_folder_name(7), "0007");
    }

    #[test]
    fn next_ud_name_takes_the_smallest_free_number() {
        let none: [&str; 0] = [];
        assert_eq!(next_ud_name(&none).as_deref(), Some("th9_ud0001.rpy"));
        assert_eq!(
            next_ud_name(&["th9_ud0001.rpy", "th9_ud0003.rpy"]).as_deref(),
            Some("th9_ud0002.rpy")
        );
        assert_eq!(
            next_ud_name(&["TH9_UD0001.RPY", "Th9_Ud0002.Rpy"]).as_deref(),
            Some("th9_ud0003.rpy")
        );
        let odd = [
            "th9_ud1.rpy",
            "th9_ud00001.rpy",
            "foo.rpy",
            "th9_ud0000.rpy",
            "th9_ud000a.rpy",
            "th9_ud0001.rpy.bak",
            "th9_01.rpy",
            "th9_ud+001.rpy",
        ];
        assert_eq!(next_ud_name(&odd).as_deref(), Some("th9_ud0001.rpy"));
    }

    #[test]
    fn next_ud_name_stops_at_9999() {
        let mut names: Vec<String> = (1..=9998).map(|n| format!("th9_ud{n:04}.rpy")).collect();
        assert_eq!(next_ud_name(&names).as_deref(), Some("th9_ud9999.rpy"));
        names.push("TH9_UD9999.rpy".to_string());
        assert_eq!(next_ud_name(&names), None);
    }

    fn pack(slots: impl Iterator<Item = u16>, uds: impl Iterator<Item = u16>) -> Vec<String> {
        slots
            .map(|n| format!("th9_{n:02}.rpy"))
            .chain(uds.map(|n| format!("TH9_UD{n:04}.RPY")))
            .collect()
    }

    #[test]
    fn pack50_fills_numbered_then_ud_then_the_next_folder() {
        let none: [&str; 0] = [];
        assert_eq!(
            pack50_next(None, &none),
            Some((1, "th9_01.rpy".to_string()))
        );
        assert_eq!(
            pack50_next(Some(3), &none),
            Some((3, "th9_01.rpy".to_string()))
        );
        assert_eq!(
            pack50_next(Some(1), &pack(1..=24, empty())),
            Some((1, "th9_25.rpy".to_string()))
        );
        let mut gap = pack(1..=25, empty());
        gap.retain(|n| n != "th9_07.rpy");
        assert_eq!(
            pack50_next(Some(1), &gap),
            Some((1, "th9_07.rpy".to_string()))
        );
        assert_eq!(
            pack50_next(Some(1), &pack(1..=25, empty())),
            Some((1, "th9_ud0001.rpy".to_string()))
        );
        assert_eq!(
            pack50_next(Some(1), &pack(1..=25, 1..=24)),
            Some((1, "th9_ud0025.rpy".to_string()))
        );
        assert_eq!(
            pack50_next(Some(1), &pack(1..=25, 1..=25)),
            Some((2, "th9_01.rpy".to_string()))
        );
        let mut beyond = pack(1..=25, 2..=26);
        beyond.push("th9_26.rpy".to_string());
        assert_eq!(
            pack50_next(Some(4), &beyond),
            Some((4, "th9_ud0001.rpy".to_string()))
        );
        assert_eq!(
            pack50_next(Some(9998), &pack(1..=25, 1..=25)),
            Some((9999, "th9_01.rpy".to_string()))
        );
        assert_eq!(pack50_next(Some(9999), &pack(1..=25, 1..=25)), None);
        assert_eq!(pack50_next(Some(u16::MAX), &pack(1..=25, 1..=25)), None);
    }

    #[test]
    fn pack50_last_folder_counts_only_four_digit_names() {
        let dirs = [
            "0001", "0003", "abc", "00009", "0000", "12345", "0002x", "0002", "a vs b",
        ];
        assert_eq!(pack50_last_folder(&dirs), Some(3));
        let none: [&str; 0] = [];
        assert_eq!(pack50_last_folder(&none), None);
        assert_eq!(pack50_last_folder(&["2026-09-25"]), None);
    }
}
