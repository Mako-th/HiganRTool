//! `HiganRTool.ini` の 1 行だけを書き換える（pure。ファイルは触らない）。
//!
//! メニューで決めた設定を ini に戻すためのもの。利用者が書いたコメント・空行・並び・ほかの行は 1 バイトも変えない。
//!
//! - 行単位で、書き換える行以外は元のバイト列をそのまま持つ（Shift_JIS の ini でも、触らない行は再符号化しない）。
//! - 値の後ろの「空白 + `;`」の注は残す。改行は元のファイルの最初の改行（CRLF / LF）に合わせる。
//! - キーの行が無ければ、その節の最後のキーの行の後ろ（キーが無ければ見出しの後ろ）に足す。節が無ければ末尾に節ごと足す。
//! - 同じ節に同じキーが 2 回あるときは書き換えず断る（どちらを読み手が採るかを、書き手が決めない）。
//! - 読めない行（`ini::parse_lenient` が捨てる行）には触らない。読めない見出しの下の行は、どの節にも数えない。
//! - UTF-8 でなければ Shift_JIS（CP932）として扱い、書く値が Shift_JIS で表せなければ断る。
#![forbid(unsafe_code)]

use std::fmt;

use encoding_rs::SHIFT_JIS;

use crate::ini::{self, strip_comment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit<'a> {
    /// `section` の `key` を `value` にする（無ければ足す）。
    Set {
        section: &'a str,
        key: &'a str,
        value: &'a str,
    },
    /// `section` の `key` の行を消す（無ければ何もしない）。「書かなければ既定」の設定を既定に戻すのに使う。
    Remove { section: &'a str, key: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// UTF-8 でも Shift_JIS でもない。
    Undecodable,
    /// 同じ節に同じキーが 2 回ある（行の番号は 1 から）。
    Duplicate {
        section: String,
        key: String,
        first: usize,
        line: usize,
    },
    /// 値が書けない形（理由つき）。
    BadValue(&'static str),
    /// 節かキーの名前が書けない形。
    BadName,
    /// 値が Shift_JIS で表せない（ini が Shift_JIS のとき）。
    Unencodable,
    /// 書いた結果を読み直すと、書いた値にならなかった。
    NotReadBack,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::Undecodable => write!(f, "UTF-8 でも Shift_JIS でもない"),
            EditError::Duplicate {
                section,
                key,
                first,
                line,
            } => write!(
                f,
                "[{section}] の {key} が {first} 行目と {line} 行目の 2 回ある"
            ),
            EditError::BadValue(why) => write!(f, "値を書けない（{why}）"),
            EditError::BadName => write!(f, "節かキーの名前が書けない形"),
            EditError::Unencodable => write!(f, "値が Shift_JIS で表せない"),
            EditError::NotReadBack => write!(f, "書いた結果を読み直すと値が合わない"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Enc {
    Utf8,
    Cp932,
}

impl Enc {
    fn decode(self, b: &[u8]) -> Option<String> {
        match self {
            Enc::Utf8 => std::str::from_utf8(b).ok().map(str::to_string),
            Enc::Cp932 => SHIFT_JIS
                .decode_without_bom_handling_and_without_replacement(b)
                .map(|c| c.into_owned()),
        }
    }

    /// 書く文字を符号化する。読み直して同じ文字に戻らなければ断る。
    fn encode(self, s: &str) -> Result<Vec<u8>, EditError> {
        match self {
            Enc::Utf8 => Ok(s.as_bytes().to_vec()),
            Enc::Cp932 => {
                let (bytes, _, bad) = SHIFT_JIS.encode(s);
                if bad {
                    return Err(EditError::Unencodable);
                }
                match self.decode(&bytes) {
                    Some(back) if back == s => Ok(bytes.into_owned()),
                    _ => Err(EditError::Unencodable),
                }
            }
        }
    }

    /// 復号した文字列の先頭 `n` バイト分が、元のバイト列で何バイトだったか。
    fn byte_len(self, s: &str, n: usize) -> usize {
        match self {
            Enc::Utf8 => n,
            Enc::Cp932 => SHIFT_JIS.encode(&s[..n]).0.len(),
        }
    }
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn check_value(v: &str) -> Result<(), EditError> {
    if v.is_empty() {
        return Err(EditError::BadValue("空"));
    }
    if v.chars().any(char::is_control) {
        return Err(EditError::BadValue("制御文字・改行がある"));
    }
    if v != v.trim() {
        return Err(EditError::BadValue("前後に空白がある"));
    }
    if strip_comment(v) != v {
        return Err(EditError::BadValue("空白 + ; が注に読まれる"));
    }
    Ok(())
}

/// 1 行を読んだ結果。
enum Kind {
    Other,
    /// 読める見出し（小文字の名前）。
    Header(String),
    /// 読めない見出し。次の見出しまでの行は読まれない。
    BadHeader,
    /// `キー = 値`。
    Key(String),
}

fn classify(text: &str) -> Kind {
    let t = text.trim();
    if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
        return Kind::Other;
    }
    if let Some(rest) = t.strip_prefix('[') {
        return match rest
            .strip_suffix(']')
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(n) => Kind::Header(n.to_ascii_lowercase()),
            None => Kind::BadHeader,
        };
    }
    match t.split_once('=') {
        Some((k, _)) if !k.trim().is_empty() => Kind::Key(k.trim().to_ascii_lowercase()),
        _ => Kind::Other,
    }
}

struct Doc {
    enc: Enc,
    /// 改行を含む元のバイト列。
    lines: Vec<Vec<u8>>,
    eol: &'static [u8],
}

fn body_of(line: &[u8], skip: usize) -> &[u8] {
    let end = line
        .iter()
        .rposition(|&b| b != b'\n' && b != b'\r')
        .map_or(0, |i| i + 1);
    &line[skip.min(end)..end]
}

impl Doc {
    fn parse(bytes: &[u8]) -> Result<Doc, EditError> {
        let enc = if std::str::from_utf8(bytes).is_ok() {
            Enc::Utf8
        } else {
            Enc::Cp932
        };
        let mut lines: Vec<Vec<u8>> = Vec::new();
        for l in bytes.split_inclusive(|&b| b == b'\n') {
            lines.push(l.to_vec());
        }
        for (i, l) in lines.iter().enumerate() {
            if enc.decode(body_of(l, bom_len(i, l))).is_none() {
                return Err(EditError::Undecodable);
            }
        }
        let eol: &'static [u8] = match bytes.iter().position(|&b| b == b'\n') {
            Some(i) if i > 0 && bytes[i - 1] == b'\r' => b"\r\n",
            Some(_) => b"\n",
            None => b"\r\n",
        };
        Ok(Doc { enc, lines, eol })
    }

    fn text(&self, i: usize) -> String {
        let l = &self.lines[i];
        self.enc
            .decode(body_of(l, bom_len(i, l)))
            .unwrap_or_default()
    }

    fn bytes(&self) -> Vec<u8> {
        self.lines.concat()
    }

    fn has_eol(&self, i: usize) -> bool {
        self.lines[i].ends_with(b"\n")
    }

    /// `section` の `key` の行（0 から）と、新しい行を足す位置の目印（節の最後のキーの行か見出しの行）。
    fn find(&self, section: &str, key: &str) -> (Vec<usize>, Option<usize>) {
        let (mut hits, mut anchor) = (Vec::new(), None);
        let mut in_sec = false;
        for i in 0..self.lines.len() {
            match classify(&self.text(i)) {
                Kind::Header(n) => {
                    in_sec = n == section;
                    if in_sec {
                        anchor = Some(i);
                    }
                }
                // 読めない見出しの下は、次の見出しまでどの節にも数えない。
                Kind::BadHeader => in_sec = false,
                Kind::Key(k) if in_sec => {
                    anchor = Some(i);
                    if k == key {
                        hits.push(i);
                    }
                }
                _ => {}
            }
        }
        (hits, anchor)
    }

    fn end_line(&mut self, i: usize) {
        if !self.has_eol(i) {
            self.lines[i].extend_from_slice(self.eol);
        }
    }

    fn set(&mut self, section: &str, key: &str, value: &str) -> Result<(), EditError> {
        let new_value = self.enc.encode(value)?;
        let (hits, anchor) = self.find(section, key);
        if hits.len() > 1 {
            return Err(EditError::Duplicate {
                section: section.to_string(),
                key: key.to_string(),
                first: hits[0] + 1,
                line: hits[1] + 1,
            });
        }
        if let Some(&i) = hits.first() {
            self.rewrite_value(i, &new_value, value);
            return Ok(());
        }
        let mut line = Vec::new();
        line.extend(self.enc.encode(key)?);
        line.extend_from_slice(b" = ");
        line.extend(new_value);
        match anchor {
            Some(a) => {
                let had_eol = self.has_eol(a);
                self.end_line(a);
                if had_eol {
                    line.extend_from_slice(self.eol);
                }
                self.lines.insert(a + 1, line);
            }
            None => {
                if let Some(last) = self.lines.len().checked_sub(1) {
                    self.end_line(last);
                    if !self.text(last).trim().is_empty() {
                        self.lines.push(self.eol.to_vec());
                    }
                }
                let mut head = format!("[{section}]").into_bytes();
                head.extend_from_slice(self.eol);
                self.lines.push(head);
                line.extend_from_slice(self.eol);
                self.lines.push(line);
            }
        }
        Ok(())
    }

    fn rewrite_value(&mut self, i: usize, new_value: &[u8], value: &str) {
        let skip = bom_len(i, &self.lines[i]);
        let body = self.text(i);
        let Some(eq) = body.find('=') else {
            return;
        };
        let after = &body[eq + 1..];
        let start = eq + 1 + (after.len() - after.trim_start().len());
        let old = strip_comment(after.trim());
        if old == value {
            return;
        }
        let end = start + old.len();
        let (b_start, b_end) = (
            skip + self.enc.byte_len(&body, start),
            skip + self.enc.byte_len(&body, end),
        );
        let line = &self.lines[i];
        let mut out = line[..b_start].to_vec();
        // `k=` に値を足すとき、`k=v` でなく `k= v` に揃える。
        if old.is_empty() && out.last() == Some(&b'=') {
            out.push(b' ');
        }
        out.extend_from_slice(new_value);
        // 空の値の後ろの注（`k = ; 注`）は、値と空白で離す（離さないと `;` が値に入る）。
        if old.is_empty() && line[b_end..].starts_with(b";") {
            out.push(b' ');
        }
        out.extend_from_slice(&line[b_end..]);
        self.lines[i] = out;
    }

    fn remove(&mut self, section: &str, key: &str) -> Result<(), EditError> {
        let (hits, _) = self.find(section, key);
        if hits.len() > 1 {
            return Err(EditError::Duplicate {
                section: section.to_string(),
                key: key.to_string(),
                first: hits[0] + 1,
                line: hits[1] + 1,
            });
        }
        if let Some(&i) = hits.first() {
            let keep = bom_len(i, &self.lines[i]);
            if keep > 0 {
                self.lines[i] = BOM.to_vec();
            } else {
                self.lines.remove(i);
            }
        }
        Ok(())
    }
}

/// 先頭の行にだけある UTF-8 の BOM の長さ。
fn bom_len(i: usize, line: &[u8]) -> usize {
    if i == 0 && line.starts_with(BOM) {
        BOM.len()
    } else {
        0
    }
}

/// `edits` を順に当てた ini のバイト列を返す。当てる前後で変わらない行は元のバイト列のまま。
pub fn apply(bytes: &[u8], edits: &[Edit<'_>]) -> Result<Vec<u8>, EditError> {
    let mut doc = Doc::parse(bytes)?;
    for e in edits {
        match *e {
            Edit::Set {
                section,
                key,
                value,
            } => {
                if !name_ok(section) || !name_ok(key) {
                    return Err(EditError::BadName);
                }
                check_value(value)?;
                doc.set(
                    &section.to_ascii_lowercase(),
                    &key.to_ascii_lowercase(),
                    value,
                )?;
            }
            Edit::Remove { section, key } => {
                if !name_ok(section) || !name_ok(key) {
                    return Err(EditError::BadName);
                }
                doc.remove(&section.to_ascii_lowercase(), &key.to_ascii_lowercase())?;
            }
        }
    }
    let out = doc.bytes();
    let back = Doc::parse(&out)?;
    let text: String = (0..back.lines.len())
        .map(|i| {
            let mut t = back.text(i);
            t.push('\n');
            t
        })
        .collect();
    let read = ini::parse_lenient(&text).ini;
    // 後の編集で消した・書き直したキーは、最後の編集だけを確かめる。
    let target = |e: &Edit<'_>| match *e {
        Edit::Set { section, key, .. } | Edit::Remove { section, key } => {
            (section.to_ascii_lowercase(), key.to_ascii_lowercase())
        }
    };
    for (i, e) in edits.iter().enumerate() {
        if let Edit::Set {
            section,
            key,
            value,
        } = *e
            && !edits[i + 1..].iter().any(|l| target(l) == target(e))
            && read.get(section, key) != Some(value)
        {
            return Err(EditError::NotReadBack);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(section: &'a str, key: &'a str, value: &'a str) -> Edit<'a> {
        Edit::Set {
            section,
            key,
            value,
        }
    }

    fn run(text: &str, edits: &[Edit<'_>]) -> String {
        String::from_utf8(apply(text.as_bytes(), edits).unwrap()).unwrap()
    }

    const DIST_LIKE: &str = "; 設定。\n\n[netplay]\n; 名前\nname = HiganRTool\n; ルーム\n; room =\n; サイド\n; host_seat = 1\ndelay = auto\n\n[pacing]\nmode = auto\n\n[ui]\nlang = auto\n\n; 更新\n; [loader]\n; update_check = 0\n";

    #[test]
    fn an_existing_value_changes_and_every_other_byte_stays() {
        let out = run(DIST_LIKE, &[set("netplay", "name", "Alice")]);
        assert_eq!(out, DIST_LIKE.replace("name = HiganRTool", "name = Alice"));
        let out = run(DIST_LIKE, &[set("NetPlay", "NAME", "Bob")]);
        assert_eq!(out, DIST_LIKE.replace("name = HiganRTool", "name = Bob"));
        assert_eq!(
            run(DIST_LIKE, &[set("netplay", "delay", "auto")]),
            DIST_LIKE,
            "同じ値の書き直しで変わった"
        );
    }

    #[test]
    fn a_trailing_comment_and_the_spacing_around_the_value_stay() {
        let t = "[netplay]\nname   =\t  old  ; 自分の名前\ndelay=3 ; 注\n";
        assert_eq!(
            run(t, &[set("netplay", "name", "new")]),
            "[netplay]\nname   =\t  new  ; 自分の名前\ndelay=3 ; 注\n"
        );
        assert_eq!(
            run(t, &[set("netplay", "delay", "auto")]),
            "[netplay]\nname   =\t  old  ; 自分の名前\ndelay=auto ; 注\n"
        );
        // `a;b` の `;` は値の一部（注ではない）。
        assert_eq!(
            run("[s]\nk = a;b ; c\n", &[set("s", "k", "x")]),
            "[s]\nk = x ; c\n"
        );
    }

    #[test]
    fn an_empty_value_keeps_a_comment_apart_from_the_new_value() {
        assert_eq!(
            run("[s]\nk = ; 注\n", &[set("s", "k", "v")]),
            "[s]\nk = v ; 注\n"
        );
        assert_eq!(run("[s]\nk =\n", &[set("s", "k", "v")]), "[s]\nk = v\n");
        assert_eq!(run("[s]\nk=\n", &[set("s", "k", "v")]), "[s]\nk= v\n");
    }

    #[test]
    fn a_missing_key_is_added_after_the_last_key_of_its_section() {
        let out = run(DIST_LIKE, &[set("netplay", "mode", "delay")]);
        assert_eq!(
            out,
            DIST_LIKE.replace("delay = auto\n", "delay = auto\nmode = delay\n")
        );
        let out = run("[a]\nx = 1\n\n[b]\ny = 2\n", &[set("a", "z", "3")]);
        assert_eq!(out, "[a]\nx = 1\nz = 3\n\n[b]\ny = 2\n");
        // キーの無い節は見出しの後ろ。コメントだけの節も。
        assert_eq!(
            run("[a]\n; c\n[b]\n", &[set("a", "z", "3")]),
            "[a]\nz = 3\n; c\n[b]\n"
        );
    }

    #[test]
    fn a_missing_section_is_appended_with_a_blank_line() {
        assert_eq!(
            run("[a]\nx = 1\n", &[set("loader", "game", "C:\\g\\th09.exe")]),
            "[a]\nx = 1\n\n[loader]\ngame = C:\\g\\th09.exe\n"
        );
        assert_eq!(
            run("[a]\nx = 1\n\n", &[set("loader", "game", "v")]),
            "[a]\nx = 1\n\n[loader]\ngame = v\n"
        );
        // 最後の行に改行が無い。
        assert_eq!(
            run("[a]\nx = 1", &[set("b", "y", "2")]),
            "[a]\nx = 1\n\n[b]\ny = 2\n"
        );
        assert_eq!(run("", &[set("b", "y", "2")]), "[b]\r\ny = 2\r\n");
        // コメントになっている `; [loader]` は節ではない。
        let out = run(DIST_LIKE, &[set("loader", "game", "v")]);
        assert!(out.starts_with(DIST_LIKE) && out.ends_with("\n[loader]\ngame = v\n"));
        // キーを足す先の節が最後の行（改行なし）でも崩れない。
        assert_eq!(
            run("[a]\nx = 1", &[set("a", "y", "2")]),
            "[a]\nx = 1\ny = 2"
        );
    }

    #[test]
    fn crlf_files_get_crlf_lines_and_lf_files_get_lf() {
        let crlf = DIST_LIKE.replace('\n', "\r\n");
        let out = run(
            &crlf,
            &[set("netplay", "mode", "rollback"), set("x", "y", "1")],
        );
        assert!(!out.replace("\r\n", "").contains('\n'), "LF が混ざった");
        assert!(out.contains("delay = auto\r\nmode = rollback\r\n"));
        assert!(out.ends_with("\r\n[x]\r\ny = 1\r\n"));
        assert_eq!(
            run(&crlf, &[set("netplay", "name", "Z")]),
            crlf.replace("name = HiganRTool", "name = Z")
        );
        let lf = run("[a]\nx = 1\n", &[set("a", "y", "2")]);
        assert!(!lf.contains('\r'));
    }

    #[test]
    fn remove_deletes_only_that_line() {
        let t = "[netplay]\nname = a\nhost_seat = 1 ; 1P\n; host_seat = 2\ndelay = 3\n";
        assert_eq!(
            run(
                t,
                &[Edit::Remove {
                    section: "netplay",
                    key: "host_seat"
                }]
            ),
            "[netplay]\nname = a\n; host_seat = 2\ndelay = 3\n"
        );
        assert_eq!(
            run(
                t,
                &[Edit::Remove {
                    section: "netplay",
                    key: "mode"
                }]
            ),
            t,
            "無いキーの削除で変わった"
        );
        assert_eq!(
            run(
                t,
                &[Edit::Remove {
                    section: "other",
                    key: "name"
                }]
            ),
            t,
            "別の節のキーを消した"
        );
    }

    #[test]
    fn a_duplicate_key_in_one_section_is_refused_but_other_sections_are_fine() {
        let dup = "[a]\nk = 1\nj = 2\nk = 3\n";
        assert_eq!(
            apply(dup.as_bytes(), &[set("a", "k", "9")]),
            Err(EditError::Duplicate {
                section: "a".into(),
                key: "k".into(),
                first: 2,
                line: 4
            })
        );
        assert!(matches!(
            apply(
                dup.as_bytes(),
                &[Edit::Remove {
                    section: "a",
                    key: "K"
                }]
            ),
            Err(EditError::Duplicate { .. })
        ));
        assert_eq!(
            run("[a]\nk = 1\n[b]\nk = 2\n", &[set("b", "k", "9")]),
            "[a]\nk = 1\n[b]\nk = 9\n"
        );
        // 別のキーの書き換えは、重複したキーがあっても進められる。
        assert_eq!(
            run(dup, &[set("a", "j", "7")]),
            "[a]\nk = 1\nj = 7\nk = 3\n"
        );
    }

    #[test]
    fn unreadable_lines_are_not_touched_and_do_not_count_as_a_section() {
        let t = "[a]\nfoo\n = 5\nk = 1\n[b\nk = 2\nj = 3\n[c]\nk = 4\n";
        let out = run(t, &[set("a", "k", "9"), set("c", "k", "8")]);
        assert_eq!(out, "[a]\nfoo\n = 5\nk = 9\n[b\nk = 2\nj = 3\n[c]\nk = 8\n");
        // 読めない見出しの下のキーは、前の節 `[a]` にも `[b]` にも数えない: `[a]` に足す。
        let out = run("[a]\nx = 1\n[b\nk = 2\n", &[set("a", "k", "9")]);
        assert_eq!(out, "[a]\nx = 1\nk = 9\n[b\nk = 2\n");
        // コメントの `; k = 1` は行として数えない。
        assert_eq!(
            run("[a]\n; k = 1\n# k = 2\n", &[set("a", "k", "3")]),
            "[a]\nk = 3\n; k = 1\n# k = 2\n"
        );
    }

    #[test]
    fn a_utf8_bom_stays() {
        assert_eq!(
            apply("\u{FEFF}[a]\nk = 1\n".as_bytes(), &[set("a", "k", "2")]).unwrap(),
            "\u{FEFF}[a]\nk = 2\n".as_bytes()
        );
        assert_eq!(
            apply("\u{FEFF}k = 1\n[a]\n".as_bytes(), &[set("a", "j", "2")]).unwrap(),
            "\u{FEFF}k = 1\n[a]\nj = 2\n".as_bytes()
        );
        // BOM の直後の見出し・キーも読める。
        assert_eq!(
            apply(
                "\u{FEFF}[a]\nk = 1\n".as_bytes(),
                &[Edit::Remove {
                    section: "a",
                    key: "k"
                }]
            )
            .unwrap(),
            "\u{FEFF}[a]\n".as_bytes()
        );
    }

    #[test]
    fn a_shift_jis_file_keeps_its_bytes_and_gets_shift_jis_values() {
        let sjis = |s: &str| SHIFT_JIS.encode(s).0.into_owned();
        let mut text = sjis("; 設定（日本語のコメント）\r\n[netplay]\r\nname = old ; 名前\r\n");
        let out = apply(&text, &[set("netplay", "name", "new")]).unwrap();
        assert_eq!(
            out,
            sjis("; 設定（日本語のコメント）\r\n[netplay]\r\nname = new ; 名前\r\n")
        );
        // 日本語の値は Shift_JIS で書く（UTF-8 にならない）。
        let out = apply(&text, &[set("loader", "game", "C:\\花映塚\\th09.exe")]).unwrap();
        assert!(out.starts_with(&text));
        assert!(out.ends_with(&sjis("\r\n[loader]\r\ngame = C:\\花映塚\\th09.exe\r\n")));
        assert!(std::str::from_utf8(&out).is_err());
        // 値の前に日本語がある行を書き換えても、前後は元のバイトのまま。
        text = sjis("[a]\r\n名前 = 旧 ; 注\r\nk = 1\r\n");
        let out = apply(&text, &[set("a", "k", "2")]).unwrap();
        assert_eq!(out, sjis("[a]\r\n名前 = 旧 ; 注\r\nk = 2\r\n"));
    }

    #[test]
    fn a_shift_jis_value_with_japanese_chars_is_replaced_exactly() {
        let sjis = |s: &str| SHIFT_JIS.encode(s).0.into_owned();
        // 値が日本語（1 文字 2 バイト）で、後ろに日本語の注がある。切る位置が復号した文字の長さとずれると、前後の文字が欠ける。
        let text = sjis("[loader]\r\ngame = C:\\旧い場所\\th09.exe ; 場所の注\r\nnext = 次\r\n");
        let out = apply(&text, &[set("loader", "game", "D:\\新\\th09.exe")]).unwrap();
        assert_eq!(
            out,
            sjis("[loader]\r\ngame = D:\\新\\th09.exe ; 場所の注\r\nnext = 次\r\n")
        );
        let out = apply(&text, &[set("loader", "game", "ASCII")]).unwrap();
        assert_eq!(
            out,
            sjis("[loader]\r\ngame = ASCII ; 場所の注\r\nnext = 次\r\n")
        );
    }

    #[test]
    fn untouched_shift_jis_lines_are_not_re_encoded() {
        // 0xED40（NEC 選択 IBM 拡張の「纊」）は復号して再符号化すると別の符号（0xFA5C）になる。触らない行はそのまま残る。
        let mut text = b"; \xED\x40 comment\r\n[a]\r\nk = 1\r\n".to_vec();
        let out = apply(&text, &[set("a", "k", "2")]).unwrap();
        let at = text.len() - 3;
        text[at] = b'2';
        assert_eq!(out, text);
    }

    #[test]
    fn a_value_that_shift_jis_cannot_hold_is_refused_on_a_shift_jis_file() {
        let text = SHIFT_JIS
            .encode("; 日本語\r\n[a]\r\nk = 1\r\n")
            .0
            .into_owned();
        assert_eq!(
            apply(&text, &[set("a", "k", "😀")]),
            Err(EditError::Unencodable)
        );
        assert_eq!(
            apply(&text, &[set("a", "k", "ｱ")]).map(|_| ()),
            Ok(()),
            "半角カナは表せる"
        );
        // UTF-8 のファイルには何でも書ける。
        assert!(apply(b"[a]\nk = 1\n", &[set("a", "k", "😀")]).is_ok());
        assert_eq!(
            apply(b"\xFF\xFE[a]\n", &[set("a", "k", "1")]),
            Err(EditError::Undecodable)
        );
    }

    #[test]
    fn values_that_would_be_read_differently_are_refused() {
        for (v, _) in [
            ("", "空"),
            (" a", "前の空白"),
            ("a ", "後ろの空白"),
            ("a\nb", "改行"),
            ("a\rb", "CR"),
            ("a\0b", "NUL"),
            ("a ; b", "注"),
            ("; a", "注"),
            ("a\t;b", "タブの注"),
        ] {
            assert!(
                matches!(
                    apply(b"[a]\n", &[set("a", "k", v)]),
                    Err(EditError::BadValue(_))
                ),
                "{v:?}"
            );
        }
        assert!(apply(b"[a]\n", &[set("a", "k", "a;b")]).is_ok());
        assert!(apply(b"[a]\n", &[set("a", "k", "a=b")]).is_ok());
        assert_eq!(
            apply(b"[a]\n", &[set("a b", "k", "1")]),
            Err(EditError::BadName)
        );
        assert_eq!(
            apply(b"[a]\n", &[set("a", "k=", "1")]),
            Err(EditError::BadName)
        );
        assert_eq!(
            apply(b"[a]\n", &[set("", "k", "1")]),
            Err(EditError::BadName)
        );
    }

    #[test]
    fn several_edits_apply_in_order_and_read_back() {
        let out = run(
            "[netplay]\nname = a\n",
            &[
                set("netplay", "name", "b"),
                set("netplay", "mode", "delay"),
                set("netplay", "delay", "auto"),
                set("netplay", "host_seat", "2"),
                Edit::Remove {
                    section: "netplay",
                    key: "host_seat",
                },
                set("loader", "game", "G"),
            ],
        );
        assert_eq!(
            out,
            "[netplay]\nname = b\nmode = delay\ndelay = auto\n\n[loader]\ngame = G\n"
        );
        let r = ini::parse(&out).unwrap();
        assert_eq!(r.get("netplay", "mode"), Some("delay"));
        assert_eq!(r.get("loader", "game"), Some("G"));
    }

    #[test]
    fn the_same_section_split_in_two_places_is_one_section() {
        let t = "[a]\nx = 1\n[b]\ny = 2\n[a]\nz = 3\n";
        assert_eq!(
            run(t, &[set("a", "w", "4")]),
            "[a]\nx = 1\n[b]\ny = 2\n[a]\nz = 3\nw = 4\n"
        );
        assert_eq!(
            run(t, &[set("a", "z", "9")]),
            "[a]\nx = 1\n[b]\ny = 2\n[a]\nz = 9\n"
        );
    }
}
