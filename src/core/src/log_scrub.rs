//! ログに書く前の行から、個人を特定できる文字を伏せる。
//!
//! - ユーザーのフォルダ（ドライブ直下の `Users`）の下の名前
//! - ゲームと DLL のフォルダ（`Scrub::dir`）
//! - IP アドレス（種類 + ポートにする。ポートは NAT の状態を読むために残す）
//! - 接続コードの形の長い文字の並び（base64url。`LONG_TOKEN_MIN` 文字以上）
//!
//! 書き込み側で個別に気を付けても、OS のエラーの文などに混ざる分は防げないので、ログを書く出口の最後でまとめて通す。

#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::net::{IpAddr, Ipv6Addr};

use crate::guard::{self, Scope};

/// アドレスの種類の呼び名（試験のまとめと同じ言い方）。
pub fn ip_kind(ip: IpAddr) -> &'static str {
    match guard::reach(ip) {
        Some(Scope::Loopback) => "同じ PC の中",
        Some(Scope::Lan) => "家の中",
        Some(Scope::Internet) => "外",
        None => "使えないアドレス",
    }
}

pub const USER_LABEL: &str = "<user>";

/// 語の文字。ASCII だけ（日本語の文は語の切れ目なしにアドレスやパスへ続くので、かな・漢字の直後でもアドレスとして読む）。
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

/// `Users\<名前>` の名前を伏せる。名前は区切り（`\` か `/`）までで、区切りが無いときは最初の空白まで。
pub fn hide_user(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let key: Vec<char> = "users".chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        // ドライブ直下の `Users` に限る（別の語の途中の `users\` を伏せない）。
        let at_key = i >= 3
            && is_sep(chars[i - 1])
            && chars[i - 2] == ':'
            && chars[i - 3].is_ascii_alphabetic()
            && i + key.len() + 1 < chars.len()
            && chars[i..i + key.len()]
                .iter()
                .zip(&key)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            && is_sep(chars[i + key.len()]);
        if !at_key {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let name_start = i + key.len() + 1;
        let stop = |c: char| {
            is_sep(c)
                || matches!(
                    c,
                    '"' | '\'' | ')' | '）' | ',' | '、' | ';' | ':' | '<' | '>' | '|' | '*' | '?'
                )
                || c.is_control()
        };
        let mut end = name_start;
        while end < chars.len() && !stop(chars[end]) {
            end += 1;
        }
        // 区切りが続かない（文の途中で終わる）ときは、名前に空白を含めず最初の空白まで。
        if !(end < chars.len() && is_sep(chars[end])) {
            end = name_start;
            while end < chars.len() && !stop(chars[end]) && !chars[end].is_whitespace() {
                end += 1;
            }
        }
        out.extend(&chars[i..name_start]);
        if end > name_start {
            out.push_str(USER_LABEL);
        }
        i = end.max(name_start);
    }
    out
}

/// 伏せ始める長さ。接続コードの最短（`code::MIN_CODE_CHARS` = 返す側 60 文字）より少し短い 3 文字下から。
pub const LONG_TOKEN_MIN: usize = crate::code::MIN_CODE_CHARS - 3;
pub const LONG_TOKEN_LABEL: &str = "<長い文字の並び>";

/// base64url の文字（英数字・`-`・`_`）が `LONG_TOKEN_MIN` 文字以上続く並びを伏せる（接続コードを誤って書いたときの備え）。
pub fn hide_long_tokens(s: &str) -> String {
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= LONG_TOKEN_MIN {
            out.push_str(LONG_TOKEN_LABEL);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if is_b64(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn is_hexish(c: char) -> bool {
    c.is_ascii_hexdigit() || c == ':' || c == '.'
}

fn parse_v4(run: &[char]) -> Option<(usize, std::net::Ipv4Addr)> {
    let mut n = 0;
    while n < run.len() && (run[n].is_ascii_digit() || run[n] == '.') {
        n += 1;
    }
    let mut text: String = run[..n].iter().collect();
    while text.ends_with('.') {
        text.pop();
    }
    let groups: Vec<&str> = text.split('.').collect();
    if groups.len() != 4 || groups.iter().any(|g| g.is_empty() || g.len() > 3) {
        return None;
    }
    let ip: std::net::Ipv4Addr = text.parse().ok()?;
    Some((text.chars().count(), ip))
}

/// 直後の `:ポート`（1〜5 桁）の長さ（`:` を含む）。
fn port_len(rest: &[char]) -> usize {
    if rest.first() != Some(&':') {
        return 0;
    }
    let digits = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
    if (1..=5).contains(&digits) && !rest.get(1 + digits).is_some_and(|c| is_word(*c)) {
        1 + digits
    } else {
        0
    }
}

/// IP アドレス（v4・v6）を「種類 + ポート」に替える。
pub fn hide_addrs(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // 語の途中・`1.2.3.4.5` のような長い数の途中からはアドレスと読まない。
        let prev_word = i > 0
            && (is_word(chars[i - 1])
                || (chars[i - 1] == '.' && i >= 2 && chars[i - 2].is_ascii_digit()));
        if c == '['
            && let Some(close) = chars[i + 1..].iter().take(48).position(|&x| x == ']')
            && let Ok(ip) = chars[i + 1..i + 1 + close]
                .iter()
                .collect::<String>()
                .parse::<Ipv6Addr>()
        {
            let after = i + close + 2;
            let pl = port_len(&chars[after.min(chars.len())..]);
            out.push_str(ip_kind(IpAddr::V6(ip)));
            out.extend(&chars[after..after + pl]);
            i = after + pl;
            continue;
        }
        if prev_word || !is_hexish(c) {
            out.push(c);
            i += 1;
            continue;
        }
        let mut run_len = 0;
        while i + run_len < chars.len() && is_hexish(chars[i + run_len]) {
            run_len += 1;
        }
        let run = &chars[i..i + run_len];
        let colons = run.iter().filter(|&&x| x == ':').count();
        if colons >= 2 {
            let mut text: String = run.iter().collect();
            while text.ends_with([':', '.']) {
                text.pop();
            }
            let looks_v6 = text.contains("::") || colons >= 7;
            if looks_v6 && let Ok(ip) = text.parse::<Ipv6Addr>() {
                out.push_str(ip_kind(IpAddr::V6(ip)));
                i += text.chars().count();
                continue;
            }
        }
        if c.is_ascii_digit()
            && let Some((len, ip)) = parse_v4(run)
            && !chars.get(i + len).is_some_and(|x| {
                is_word(*x)
                    || (*x == '.' && chars.get(i + len + 1).is_some_and(|d| d.is_ascii_digit()))
            })
        {
            let pl = port_len(&chars[i + len..]);
            out.push_str(ip_kind(IpAddr::V4(ip)));
            out.extend(&chars[i + len..i + len + pl]);
            i += len + pl;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// フォルダの接頭を置き換える表。大文字・小文字は ASCII だけ区別しない。
#[derive(Debug, Clone, Default)]
pub struct Scrub {
    dirs: Vec<(Vec<char>, String)>,
}

impl Scrub {
    pub fn new() -> Self {
        Self::default()
    }

    /// `path` で始まる文字を `label` に替える（`\` と `/` の両方の綴りで）。空のパスは足さない。
    pub fn dir(mut self, path: &str, label: &str) -> Self {
        let p = path.trim_end_matches(['\\', '/']);
        if p.is_empty() {
            return self;
        }
        for form in [p.to_string(), p.replace('\\', "/")] {
            let key: Vec<char> = form.chars().collect();
            if !self.dirs.iter().any(|(k, _)| *k == key) {
                self.dirs.push((key, label.to_string()));
            }
        }
        // 長い方を先に当てる（ゲームのフォルダが DLL のフォルダの親のとき、子が先に当たるように）。
        self.dirs.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        self
    }

    fn replace_dirs(&self, s: &str) -> String {
        if self.dirs.is_empty() {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        'scan: while i < chars.len() {
            if i == 0 || !is_word(chars[i - 1]) {
                for (key, label) in &self.dirs {
                    let end = i + key.len();
                    if end <= chars.len()
                        && chars[i..end]
                            .iter()
                            .zip(key)
                            .all(|(a, b)| a.eq_ignore_ascii_case(b))
                        && !chars.get(end).is_some_and(|c| is_word(*c))
                    {
                        out.push_str(label);
                        i = end;
                        continue 'scan;
                    }
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    pub fn line(&self, s: &str) -> String {
        hide_long_tokens(&hide_addrs(&hide_user(&self.replace_dirs(s))))
    }
}

/// 1 行ずつ `Scrub` を通して `inner` へ書く。改行が来るまで溜める（`writeln!` が 1 行を何度かに分けて書くため）。
pub struct ScrubWrite<W: Write> {
    inner: W,
    scrub: Scrub,
    pending: Vec<u8>,
}

impl<W: Write> ScrubWrite<W> {
    pub fn new(inner: W, scrub: Scrub) -> Self {
        ScrubWrite {
            inner,
            scrub,
            pending: Vec::new(),
        }
    }

    fn emit_lines(&mut self) -> io::Result<()> {
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let rest = self.pending.split_off(nl + 1);
            let mut line = std::mem::replace(&mut self.pending, rest);
            line.pop();
            let cr = line.last() == Some(&b'\r');
            if cr {
                line.pop();
            }
            let mut text = self.scrub.line(&String::from_utf8_lossy(&line));
            text.push_str(if cr { "\r\n" } else { "\n" });
            // 1 行を 1 回の書き込みにする（書き込み中のファイルをローダーが読んでも、行の途中で切れにくい）。
            self.inner.write_all(text.as_bytes())?;
        }
        Ok(())
    }
}

impl<W: Write> Write for ScrubWrite<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        self.emit_lines()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for ScrubWrite<W> {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            // 次の起動の区切りの行が、この行の続きにならないよう改行で終える。
            let mut text = self.scrub.line(&String::from_utf8_lossy(&self.pending));
            text.push('\n');
            let _ = self.inner.write_all(text.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 見本のパスは組み立てる（公開用の走査がユーザーのフォルダの形を拾う）。`~` を `Users` に、`~l` を `users` にする。
    fn u(s: &str) -> String {
        s.replace("~l", "users").replace('~', "Users")
    }

    #[test]
    fn user_name_in_a_path_is_hidden_but_the_rest_stays() {
        assert_eq!(
            hide_user(&u(r"exe: C:\~\alice\Downloads\HiganRTool\th09.exe")),
            u(r"exe: C:\~\<user>\Downloads\HiganRTool\th09.exe")
        );
        assert_eq!(
            hide_user(&u(r"c:\~l\John Smith\x.ini を開けない")),
            u(r"c:\~l\<user>\x.ini を開けない")
        );
        assert_eq!(
            hide_user(&u("C:/~/alice/AppData/a")),
            u("C:/~/<user>/AppData/a")
        );
        assert_eq!(hide_user(&u(r"\\?\C:\~\alice\a")), u(r"\\?\C:\~\<user>\a"));
        // 文の途中で終わる名前は最初の空白まで。
        assert_eq!(
            hide_user(&u(r"C:\~\alice を開けない")),
            u(r"C:\~\<user> を開けない")
        );
        assert_eq!(hide_user(&u(r"（C:\~\alice）")), u(r"（C:\~\<user>）"));
    }

    #[test]
    fn users_in_other_words_is_left_alone() {
        for s in [
            r"Users\x",
            "users are fine",
            r"data\users\x",
            r"C:\Program Files\Users\x\y",
        ] {
            assert_eq!(hide_user(s), s);
        }
    }

    #[test]
    fn ipv4_becomes_kind_and_keeps_the_port() {
        assert_eq!(hide_addrs("最後は 198.51.100.9:40000"), "最後は 外:40000");
        assert_eq!(
            hide_addrs("受信ソケット 127.0.0.1:47001 と"),
            "受信ソケット 同じ PC の中:47001 と"
        );
        assert_eq!(hide_addrs("192.168.0.5"), "家の中");
        assert_eq!(hide_addrs("a=10.1.2.3,b=8.8.8.8"), "a=家の中,b=外");
        assert_eq!(hide_addrs("0.0.0.0:1"), "使えないアドレス:1");
    }

    /// 壊れたら落ちる: かな・漢字の直後のアドレスやフォルダが語の途中に見えて伏せられない（日本語の文は空白なしで続く）。
    #[test]
    fn an_address_or_folder_right_after_japanese_text_is_hidden() {
        assert_eq!(hide_addrs("候補198.51.100.9:40000へ"), "候補外:40000へ");
        assert_eq!(hide_addrs("から[::1]:5へ"), "から同じ PC の中:5へ");
        assert_eq!(hide_addrs("受信ソケット192.168.0.5"), "受信ソケット家の中");
        let s = Scrub::new().dir("C:/G/th09", "<ゲーム>");
        assert_eq!(s.line("のC:/G/th09を開けない"), "の<ゲーム>を開けない");
        // 英数字が続くなら別のフォルダ・語のまま。
        assert_eq!(s.line("C:/G/th09b"), "C:/G/th09b");
        assert_eq!(hide_addrs("v198.51.100.9"), "v198.51.100.9");
    }

    #[test]
    fn numbers_that_are_not_addresses_stay() {
        for s in [
            "時刻: 2026-10-07 21:57:20.870",
            "花映塚 1.50a",
            "2995.199 MHz（ずれ +0.1 ppm）",
            "0x0043112D",
            "版 1.2.3",
            "1.2.3.4.5",
            "ver1.2.3.4a",
            "区分 1〜8 ms: 3 / 8〜15 ms: 3",
            "seg 1 frame 2: 3",
        ] {
            assert_eq!(hide_addrs(s), s);
        }
    }

    #[test]
    fn ipv6_in_brackets_and_bare_is_hidden() {
        assert_eq!(hide_addrs("[2001:db8::1]:47001"), "外:47001");
        assert_eq!(hide_addrs("から [::1]:5 へ"), "から 同じ PC の中:5 へ");
        assert_eq!(hide_addrs("v6 2001:db8:0:0:0:0:0:1 です"), "v6 外 です");
        assert_eq!(hide_addrs("fe80::1"), "家の中");
        assert_eq!(hide_addrs("::1"), "同じ PC の中");
    }

    #[test]
    fn the_hiding_length_stays_below_the_shortest_connection_code() {
        const {
            assert!(LONG_TOKEN_MIN < crate::code::MIN_CODE_CHARS);
            // ファイル名などの普通の語を伏せない長さ。
            assert!(LONG_TOKEN_MIN > 50);
        }
    }

    #[test]
    fn a_connection_code_shaped_run_is_hidden_and_ordinary_text_is_not() {
        // 候補 2 つ・名前 "ab" の始める側のコード（golden）。
        let code = "BQAAAAABRDMiEQDSSWsAAAAAAmFiAAECAwQFBgcICQoLDA0ODwIBBH8AAAGZtwQGIAENuAAAAAAAAAAAAAAAAVgb7NLang";
        assert_eq!(
            hide_long_tokens(&format!("コード {code} を書いた")),
            "コード <長い文字の並び> を書いた"
        );
        assert_eq!(
            hide_long_tokens(&code[..LONG_TOKEN_MIN]),
            LONG_TOKEN_LABEL,
            "下限ちょうどから伏せる"
        );
        assert_eq!(
            hide_long_tokens(&code[..LONG_TOKEN_MIN - 1]),
            code[..LONG_TOKEN_MIN - 1]
        );
        for s in [
            "dll_20261007-215720-865_pid19312_net_frames.csv.gz",
            "dll_20261007-215720-865_pid19312_pacing.csv.gz と dll_20261007-215720-865_pid19312_menu.csv",
            "HiganRTool-20261004-30cf33b",
            "地点 input_wait_call 0x0043112D 5 バイト（call 先の差し替えの予定）: 一致",
        ] {
            assert_eq!(hide_long_tokens(s), s);
        }
    }

    #[test]
    fn dir_prefixes_are_replaced_longest_first_on_path_boundaries() {
        let s = Scrub::new()
            .dir(r"C:\Games\th09", "<ゲーム>")
            .dir(r"C:\Games\th09\tool", "<ツール>");
        assert_eq!(
            s.line(r"c:\games\TH09\tool\logs\a.log を開けない"),
            r"<ツール>\logs\a.log を開けない"
        );
        assert_eq!(s.line(r"C:\Games\th09_b\x"), r"C:\Games\th09_b\x");
        assert_eq!(s.line("C:/Games/th09/Replay"), "<ゲーム>/Replay");
        assert_eq!(Scrub::new().dir("", "x").line("abc"), "abc");
    }

    #[test]
    fn writer_scrubs_whole_lines_even_when_split_in_pieces() {
        let mut out = Vec::new();
        {
            let mut w = ScrubWrite::new(&mut out, Scrub::new().dir(r"C:\G", "<g>"));
            write!(w, "exe: C:\\G\\th09.exe（").unwrap();
            writeln!(w, "198.51.100.9:7）").unwrap();
            write!(w, "{}", u(r"次 C:\~\alice\x")).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            u("exe: <g>\\th09.exe（外:7）\n次 C:\\~\\<user>\\x\n"),
            "改行の無い残りは、改行で終えて書く"
        );
    }

    /// 壊れたら落ちる: 行の本文と改行が別の書き込みになる（書き込み中のファイルを読む側が行の途中で切れた形を見うる）。
    #[test]
    fn writer_hands_each_line_to_the_inner_writer_in_one_write() {
        struct Calls(Vec<Vec<u8>>);
        impl Write for Calls {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.push(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut calls = Calls(Vec::new());
        {
            let mut w = ScrubWrite::new(&mut calls, Scrub::new());
            write!(w, "前半").unwrap();
            writeln!(w, "後半").unwrap();
            w.write_all("次\r\n三\n".as_bytes()).unwrap();
        }
        let got: Vec<&[u8]> = calls.0.iter().map(Vec::as_slice).collect();
        let want: [&[u8]; 3] = [
            "前半後半\n".as_bytes(),
            "次\r\n".as_bytes(),
            "三\n".as_bytes(),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn writer_keeps_crlf_and_splits_multibyte_writes() {
        let mut out = Vec::new();
        {
            let mut w = ScrubWrite::new(&mut out, Scrub::new());
            let bytes = "あいう\r\n".as_bytes();
            w.write_all(&bytes[..4]).unwrap();
            w.write_all(&bytes[4..]).unwrap();
        }
        assert_eq!(String::from_utf8(out).unwrap(), "あいう\r\n");
    }
}
