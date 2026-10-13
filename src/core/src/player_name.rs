//! プレイヤー名（ゲーム内メニューの「名前」。pure・確保しない）。
//!
//! 名前は HELLO で相手に名乗る値（`handshake::Name`。UTF-8 の 32 バイトまで）で、ini の `name` が元になる。
//! ここはウィンドウで打つ名前の文字の決まり・編集・覚えるファイルの文字だけで、文字の受け取りとファイルの読み書きは dll の仕事。
//! 打てる文字は ASCII の英数字・`-`・`_`・`@`（ルーム名と同じ）で、最大 `wire::NAME_MAX`（32 バイト）= 32 文字。
//! ini の `name` や手書きのファイルの名前は検査しない（任意の UTF-8 を 32 バイトで切る）。ウィンドウで打ち直すときは空から始める。

use crate::room::RoomName;
use crate::room_input::{Edit, TextKey, UNIT_BACKSPACE, classify_by};
use crate::wire::NAME_MAX;

/// 名前の長さの上限（バイト）。HELLO の name の欄（`wire::NAME_MAX`）と同じ。
pub const PLAYER_NAME_MAX: usize = NAME_MAX;

/// 覚えるファイルの名前（`code_dir` の側に置き、ゲームのフォルダには書かない）。
pub const PLAYER_NAME_FILE: &str = "player_name.txt";

/// `NameBuf` を原子変数の語に詰めた長さ（32 バイトを 4 バイトずつ ＋ 長さ 1 語）。
pub const NAME_BUF_WORDS: usize = PLAYER_NAME_MAX / 4 + 1;
const _: () = assert!(PLAYER_NAME_MAX.is_multiple_of(4));

/// 名前に打てる文字か（ASCII の英数字・`-`・`_`・`@`）。文字の表はルーム名の `RoomName::accepts_char` の 1 か所に置く。
pub fn accepts_char(c: char) -> bool {
    RoomName::accepts_char(c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameWhy {
    Empty,
    BadChar,
}

/// 打っている名前（ASCII）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NameBuf {
    buf: [u8; PLAYER_NAME_MAX],
    len: usize,
}

impl Default for NameBuf {
    fn default() -> Self {
        NameBuf::new()
    }
}

impl NameBuf {
    pub const fn new() -> NameBuf {
        NameBuf {
            buf: [0; PLAYER_NAME_MAX],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_str(&self) -> &str {
        // 入れるのは受ける文字と `?` だけなので、常に正しい UTF-8。
        core::str::from_utf8(self.buf.get(..self.len).unwrap_or(&[])).unwrap_or("")
    }

    /// 受けない文字か上限なら足さずに false。
    pub fn push(&mut self, c: char) -> bool {
        if !accepts_char(c) {
            return false;
        }
        match self.buf.get_mut(self.len) {
            Some(slot) => {
                *slot = c as u8;
                self.len += 1;
                true
            }
            None => false,
        }
    }

    pub fn pop(&mut self) -> bool {
        if self.len == 0 {
            return false;
        }
        self.len -= 1;
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = 0;
        }
        true
    }

    /// 前後の空白を除いた名前。受けない文字を含めば `None`。
    pub fn from_name(s: &str) -> Option<NameBuf> {
        let t = s.trim_matches(' ');
        if t.is_empty() {
            return None;
        }
        let mut b = NameBuf::new();
        for c in t.chars() {
            if !b.push(c) {
                return None;
            }
        }
        Some(b)
    }

    /// 表示用: 任意の UTF-8 を ASCII にする。受けない文字は `?` 1 つにし、`PLAYER_NAME_MAX` 文字で切る。
    pub fn lossy(s: &str) -> NameBuf {
        let mut b = NameBuf::new();
        for c in s.trim_matches(' ').chars().take(PLAYER_NAME_MAX) {
            let shown = if accepts_char(c) { c } else { '?' };
            if let Some(slot) = b.buf.get_mut(b.len) {
                *slot = shown as u8;
                b.len += 1;
            }
        }
        b
    }

    /// `lossy` が `?` に直した文字を含まないか。含むならウィンドウでの打ち直しは空から始める。
    pub fn is_editable(&self) -> bool {
        self.as_str().chars().all(accepts_char)
    }

    pub fn parse(&self) -> Result<NameBuf, NameWhy> {
        if !self.is_editable() {
            return Err(NameWhy::BadChar);
        }
        NameBuf::from_name(self.as_str()).ok_or(NameWhy::Empty)
    }

    /// 原子変数の語に詰める（見張り・ソケットを開くスレッドへ渡す）。
    pub fn to_words(&self) -> [u32; NAME_BUF_WORDS] {
        let mut w = [0u32; NAME_BUF_WORDS];
        for (d, chunk) in w.iter_mut().zip(self.buf.chunks(4)) {
            let mut b = [0u8; 4];
            for (x, s) in b.iter_mut().zip(chunk) {
                *x = *s;
            }
            *d = u32::from_le_bytes(b);
        }
        w[NAME_BUF_WORDS - 1] = self.len as u32;
        w
    }

    /// `to_words` の逆。壊れた語でも、受けるところまでで切って落ちない。
    pub fn from_words(w: &[u32; NAME_BUF_WORDS]) -> NameBuf {
        let len = (w[NAME_BUF_WORDS - 1] as usize).min(PLAYER_NAME_MAX);
        let mut bytes = [0u8; PLAYER_NAME_MAX];
        for (chunk, word) in bytes.chunks_mut(4).zip(w) {
            for (d, s) in chunk.iter_mut().zip(word.to_le_bytes()) {
                *d = s;
            }
        }
        let mut b = NameBuf::new();
        for &x in bytes.iter().take(len) {
            if !b.push(char::from(x)) {
                break;
            }
        }
        b
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NameInput {
    buf: NameBuf,
}

impl NameInput {
    pub fn new(initial: NameBuf) -> NameInput {
        NameInput { buf: initial }
    }

    pub fn text(&self) -> &NameBuf {
        &self.buf
    }

    pub fn feed(&mut self, unit: u16) -> Edit {
        match classify_by(unit, accepts_char) {
            TextKey::Char(c) => {
                if self.buf.push(c) {
                    Edit::Changed
                } else {
                    Edit::Nothing
                }
            }
            TextKey::Backspace => {
                if self.buf.pop() {
                    Edit::Changed
                } else {
                    Edit::Nothing
                }
            }
            TextKey::Enter => Edit::Enter,
            TextKey::Rejected => Edit::Rejected,
            TextKey::Ignore => Edit::Nothing,
        }
    }

    pub const BACKSPACE: u16 = UNIT_BACKSPACE;
}

pub fn file_text(name: &NameBuf) -> String {
    format!("{}\n", name.as_str())
}

/// 覚えたファイルの中身を読む。BOM と前後の空白を除き、最初の 1 行だけを見る。空・制御文字を含む行は `None`。
/// 手書きの日本語の名前も通し、`PLAYER_NAME_MAX` バイトを越える分は文字の境で切る。
pub fn parse_file(text: &str) -> Option<String> {
    let first = text.trim_start_matches('\u{FEFF}').lines().next()?.trim();
    if first.is_empty() || first.chars().any(char::is_control) {
        return None;
    }
    let mut end = 0;
    for (i, c) in first.char_indices() {
        let e = i + c.len_utf8();
        if e > PLAYER_NAME_MAX {
            break;
        }
        end = e;
    }
    let cut = first.get(..end)?.trim();
    (!cut.is_empty()).then(|| cut.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_input::{UNIT_ENTER, UNIT_LINE_FEED};

    /// 壊れたら落ちる: 名前の文字の表がルーム名の表から離れる、または表が名前の仕様（英数字・`-`・`_`・`@`）から外れる。
    #[test]
    fn the_name_chars_are_the_room_name_chars() {
        for u in 0..0x3100u32 {
            let c = char::from_u32(u).unwrap();
            assert_eq!(accepts_char(c), RoomName::accepts_char(c), "{u:#x}");
        }
        assert!(['a', 'Z', '0', '-', '_', '@'].into_iter().all(accepts_char));
        assert!(![' ', '.', '/', 'あ', 'Ａ'].into_iter().any(accepts_char));
    }

    fn typed(text: &str) -> NameInput {
        let mut i = NameInput::default();
        for u in text.encode_utf16() {
            i.feed(u);
        }
        i
    }

    #[test]
    fn the_accepted_characters_are_ascii_alphanumerics_dash_underscore_and_at() {
        assert_eq!(typed("Synth-Name_7").text().as_str(), "Synth-Name_7");
        assert_eq!(typed("a@b").text().as_str(), "a@b");
        assert_eq!(typed("ＡＢ０９－＿＠").text().as_str(), "AB09-_@");
        for dropped in [
            " ", ".", "/", "\\", ":", "?", "#", "あ", "部", "ｱ", "　", "！", "😀",
        ] {
            let got = typed(&format!("a{dropped}b"));
            assert_eq!(got.text().as_str(), "ab", "{dropped:?}");
        }
        assert_eq!(typed("ab").text().as_str(), "ab", "隣の肯定形");
        let mut i = NameInput::default();
        assert_eq!(i.feed(u16::from(b'.')), Edit::Rejected);
        assert_eq!(i.feed(u16::from(b' ')), Edit::Rejected, "空白は受けない");
        assert_eq!(i.feed(u16::from(b'-')), Edit::Changed);
        assert_eq!(i.feed(u16::from(b'_')), Edit::Changed);
        assert_eq!(i.feed(u16::from(b'@')), Edit::Changed);
        assert_eq!(i.feed(0x1B), Edit::Nothing, "Esc は文字ではない");
        assert_eq!(i.feed(UNIT_ENTER), Edit::Enter);
        assert_eq!(i.feed(UNIT_LINE_FEED), Edit::Enter);
        let mut accepted = 0usize;
        for u in 0..=u16::MAX {
            let mut x = NameInput::default();
            let got = x.feed(u) == Edit::Changed && !x.text().is_empty();
            let folded = crate::room_input::fold_unit(u).is_some_and(accepts_char)
                && !matches!(u, 0..0x20 | 0x7F);
            assert_eq!(got, folded, "{u:#06x}");
            accepted += usize::from(got);
        }
        // 半角 65（英数字 62 ＋ `-` `_` `@`）＋ 全角 65。空白は受けない。
        assert_eq!(accepted, 65 + 65);
    }

    #[test]
    fn the_buffer_stops_at_the_hello_limit_and_backspace_works() {
        assert_eq!(PLAYER_NAME_MAX, NAME_MAX);
        let mut i = typed(&"a".repeat(PLAYER_NAME_MAX));
        assert_eq!(i.text().len(), PLAYER_NAME_MAX);
        assert_eq!(i.feed(u16::from(b'b')), Edit::Nothing);
        assert_eq!(i.feed(NameInput::BACKSPACE), Edit::Changed);
        assert_eq!(i.feed(u16::from(b'b')), Edit::Changed);
        assert_eq!(i.text().as_str(), format!("{}b", "a".repeat(31)));
        let mut e = NameInput::default();
        assert_eq!(e.feed(NameInput::BACKSPACE), Edit::Nothing);
        let mut s = NameInput::new(NameBuf::from_name("Alice").unwrap());
        s.feed(NameInput::BACKSPACE);
        assert_eq!(s.text().as_str(), "Alic");
    }

    #[test]
    fn deciding_trims_the_ends_and_refuses_empty_and_unusable_names() {
        let parse = |s: &str| NameBuf::lossy(s).parse();
        assert_eq!(parse("  Bob  ").unwrap().as_str(), "Bob");
        assert_eq!(parse("a  b"), Err(NameWhy::BadChar), "内側の空白は受けない");
        assert_eq!(parse("a-b_c").unwrap().as_str(), "a-b_c");
        assert_eq!(parse(""), Err(NameWhy::Empty));
        assert_eq!(parse("   "), Err(NameWhy::Empty));
        assert_eq!(parse("テスト"), Err(NameWhy::BadChar));
        assert_eq!(parse("a.b"), Err(NameWhy::BadChar));
        // 前後の空白を除くのは `from_name`（ini・ファイルから来た文字列の入口）で、`push` は空白を足さない。
        let mut raw = NameBuf::new();
        for c in "  x ".chars() {
            raw.push(c);
        }
        assert_eq!(raw.as_str(), "x");
        assert_eq!(raw.parse().unwrap().as_str(), "x");
        assert_eq!(NameBuf::from_name("  y ").unwrap().as_str(), "y");
        assert_eq!(NameBuf::from_name("a b"), None, "内側の空白");
        assert_eq!(NameBuf::from_name("a.b"), None);
        assert_eq!(NameBuf::from_name(&"a".repeat(33)), None);
    }

    #[test]
    fn a_name_from_ini_is_shown_lossily_and_only_a_clean_one_is_editable() {
        let jp = NameBuf::lossy("テスト");
        assert_eq!(jp.as_str(), "???");
        assert!(!jp.is_editable());
        let ok = NameBuf::lossy("Alice-2_b");
        assert_eq!(ok.as_str(), "Alice-2_b");
        assert!(ok.is_editable());
        let sp = NameBuf::lossy("Alice 2");
        assert_eq!(sp.as_str(), "Alice?2");
        assert!(!sp.is_editable());
        let long = NameBuf::lossy(&"あ".repeat(40));
        assert_eq!(long.len(), PLAYER_NAME_MAX);
        assert_eq!(NameBuf::lossy("  Bob ").as_str(), "Bob");
    }

    #[test]
    fn words_round_trip_and_broken_words_do_not_panic() {
        let n = NameBuf::from_name("Round-Trip_1x").unwrap();
        assert_eq!(NameBuf::from_words(&n.to_words()), n);
        let mut w = [0xFFFF_FFFFu32; NAME_BUF_WORDS];
        let b = NameBuf::from_words(&w);
        assert!(b.is_empty(), "受けない値の語から名前ができた");
        w[NAME_BUF_WORDS - 1] = u32::MAX;
        let _ = NameBuf::from_words(&w);
        let a = NameBuf::from_name(&"a".repeat(PLAYER_NAME_MAX)).unwrap();
        assert_eq!(NameBuf::from_words(&a.to_words()).len(), PLAYER_NAME_MAX);
    }

    #[test]
    fn the_name_file_is_read_leniently_but_never_longer_than_hello() {
        assert_eq!(parse_file("Alice\n").as_deref(), Some("Alice"));
        assert_eq!(
            parse_file("\u{FEFF}  Bob \r\nsecond\n").as_deref(),
            Some("Bob")
        );
        assert_eq!(parse_file("テスト\n").as_deref(), Some("テスト"));
        for bad in ["", "\n", "   \n", "a\u{7}b\n", "\n\nabc\n"] {
            assert_eq!(parse_file(bad), None, "{bad:?}");
        }
        // 日本語 1 文字 = 3 バイトの途中で切らない。
        let cut = parse_file(&"あ".repeat(11)).unwrap();
        assert_eq!(cut.len(), 30);
        assert!(cut.len() <= PLAYER_NAME_MAX);
        assert_eq!(parse_file(&"a".repeat(32)).unwrap().len(), 32);
        assert_eq!(parse_file(&"a".repeat(40)).unwrap().len(), 32);
        let n = NameBuf::from_name("Written_1").unwrap();
        assert_eq!(parse_file(&file_text(&n)).as_deref(), Some("Written_1"));
    }
}
