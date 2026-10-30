//! プレイヤー名（ゲーム内メニューの「名前」。pure・確保しない）。
//!
//! 名前はHELLOで相手に名乗る値（`handshake::Name`。UTF-8の32バイトまで）。iniの`name`が元になる。
//! ここが持つのはウィンドウで打つ名前の文字の決まりと編集だけ。文字の受け取りとiniの書き換えはdllの仕事。
//! 打てる文字は印字できる半角（0x20〜0x7E）のうち`` ` `` `;` `/` 以外（ルーム名と同じ）で、最大 `wire::NAME_MAX`（32バイト）= 32文字。
//! iniの`name`は検査しない（任意のUTF-8を32バイトで切る）。ウィンドウで打ち直すときは空から始める。

use crate::room::RoomName;
use crate::room_input::{Edit, TextKey, UNIT_BACKSPACE, classify_by};
use crate::wire::NAME_MAX;

/// 名前の長さの上限（バイト）。HELLOのnameのフィールド（`wire::NAME_MAX`）と同じ。
pub const PLAYER_NAME_MAX: usize = NAME_MAX;

/// 以前のバージョンが名前を覚えていたファイルの名前（置き場は接続コードの置き場）。今は読まない。起動時にDLLの`ini_save::migrate_legacy`がiniの`name`へ移して消す。
pub const PLAYER_NAME_FILE: &str = "player_name.txt";

/// `NameBuf`をアトミック変数のワードに詰めた長さ（32バイトを4バイトずつ ＋ 長さ1ワード）。
pub const NAME_BUF_WORDS: usize = PLAYER_NAME_MAX / 4 + 1;
const _: () = assert!(PLAYER_NAME_MAX.is_multiple_of(4));

/// 利用者が名前を設定する前の初期の名前（配布するiniの`name`の値）。メニューはこの名前のままの間は「設定」でプレイヤー名を設定するよう案内する。
pub const DEFAULT_NAME: &str = "HiganRTool";

/// 名前に打てる文字かどうか。文字の表はルーム名の`RoomName::accepts_char`の1か所で定義する。
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
    /// `lossy`が受けない文字を`?`に直した（`?`自体は打てる文字なので文字からは見分けられない）。
    replaced: bool,
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
            replaced: false,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_str(&self) -> &str {
        // 書き込むのは受ける文字と`?`だけなので常に正しいUTF-8。
        core::str::from_utf8(self.buf.get(..self.len).unwrap_or(&[])).unwrap_or("")
    }

    /// 受けない文字か上限なら足さずにfalse。
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

    /// 前後の空白を除いた名前。受けない文字を含めば`None`。
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

    /// 表示用: 任意のUTF-8をASCIIにする。受けない文字は`?` 1つにし、`PLAYER_NAME_MAX`文字で切る。
    pub fn lossy(s: &str) -> NameBuf {
        let mut b = NameBuf::new();
        for c in s.trim_matches(' ').chars().take(PLAYER_NAME_MAX) {
            let shown = if accepts_char(c) {
                c
            } else {
                b.replaced = true;
                '?'
            };
            if let Some(slot) = b.buf.get_mut(b.len) {
                *slot = shown as u8;
                b.len += 1;
            }
        }
        b
    }

    /// 既定の名前（`DEFAULT_NAME`）のままかどうか。
    pub fn is_default(&self) -> bool {
        self.as_str() == DEFAULT_NAME
    }

    /// `lossy`が`?`に直した文字を含まないかどうか。含むならウィンドウでの打ち直しは空から始める。
    pub fn is_editable(&self) -> bool {
        !self.replaced
    }

    pub fn parse(&self) -> Result<NameBuf, NameWhy> {
        if !self.is_editable() {
            return Err(NameWhy::BadChar);
        }
        NameBuf::from_name(self.as_str()).ok_or(NameWhy::Empty)
    }

    /// アトミック変数のワードに詰める（監視の処理・ソケットを開くスレッドへ渡す）。
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

    /// `to_words`の逆。壊れたワードでも受けるところまでで切ってパニックしない。
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

/// 以前のバージョンが覚えたファイル（`PLAYER_NAME_FILE`）の中身を読む（引き継ぎ用）。
/// BOMと前後の空白を除いて最初の1行だけを見る。空・制御文字を含む行は`None`。
/// 手書きの日本語の名前も通す。`PLAYER_NAME_MAX`バイトを越える分は文字の境界で切る。
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

    /// ルーム名の表との一致に加えて名前の仕様（0x20〜0x7Eのうち`` ` `` `;` `/` 以外）そのものも見る。
    #[test]
    fn the_name_chars_are_the_room_name_chars() {
        for u in 0..0x3100u32 {
            let c = char::from_u32(u).unwrap();
            assert_eq!(accepts_char(c), RoomName::accepts_char(c), "{u:#x}");
        }
        assert!(
            ['a', 'Z', '0', '-', '_', '@', '.', ' ', '"', '\\']
                .into_iter()
                .all(accepts_char)
        );
        assert!(
            ['`', ';', '/', 'あ', 'Ａ']
                .into_iter()
                .all(|c| !accepts_char(c))
        );
    }

    fn typed(text: &str) -> NameInput {
        let mut i = NameInput::default();
        for u in text.encode_utf16() {
            i.feed(u);
        }
        i
    }

    #[test]
    fn the_accepted_characters_are_printable_ascii_except_backtick_semicolon_and_slash() {
        assert_eq!(typed("Synth-Name_7").text().as_str(), "Synth-Name_7");
        assert_eq!(typed("a@b").text().as_str(), "a@b");
        assert_eq!(typed("ＡＢ０９－＿＠").text().as_str(), "AB09-_@");
        assert_eq!(
            typed("ＡＢ．／；").text().as_str(),
            "AB.",
            "全角は半角に直してから見る"
        );
        for kept in [" ", ".", "\\", ":", "?", "#", "\"", "<", "'"] {
            let got = typed(&format!("a{kept}b"));
            assert_eq!(got.text().as_str(), format!("a{kept}b"), "{kept:?}");
        }
        for dropped in ["/", ";", "`", "あ", "部", "ｱ", "　", "😀"] {
            let got = typed(&format!("a{dropped}b"));
            assert_eq!(got.text().as_str(), "ab", "{dropped:?}");
        }
        let mut i = NameInput::default();
        assert_eq!(i.feed(u16::from(b'/')), Edit::Rejected);
        assert_eq!(i.feed(u16::from(b';')), Edit::Rejected);
        assert_eq!(i.feed(u16::from(b'`')), Edit::Rejected);
        assert_eq!(i.feed(u16::from(b'.')), Edit::Changed);
        assert_eq!(i.feed(u16::from(b' ')), Edit::Changed, "間の空白は受ける");
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
        // 半角92（0x20〜0x7Eの95から`` ` `` `;` `/` を除く）＋ 全角 91（`！`〜`～` の94から同じ3つ。全角の空白は直さず受けない）。
        assert_eq!(accepted, 92 + 91);
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
        assert_eq!(parse("a  b").unwrap().as_str(), "a  b", "間の空白は残す");
        assert_eq!(parse("a-b_c").unwrap().as_str(), "a-b_c");
        assert_eq!(parse(""), Err(NameWhy::Empty));
        assert_eq!(parse("   "), Err(NameWhy::Empty));
        assert_eq!(parse("テスト"), Err(NameWhy::BadChar));
        assert_eq!(parse("a.b").unwrap().as_str(), "a.b");
        assert_eq!(parse("a;b"), Err(NameWhy::BadChar));
        // 前後の空白を除くのは`parse`と`from_name`（iniから来た文字列の入口）。`push`は打った空白をそのまま足す。
        let mut raw = NameBuf::new();
        for c in "  x ".chars() {
            raw.push(c);
        }
        assert_eq!(raw.as_str(), "  x ");
        assert_eq!(raw.parse().unwrap().as_str(), "x");
        assert_eq!(NameBuf::from_name("  y ").unwrap().as_str(), "y");
        assert_eq!(NameBuf::from_name("a b").unwrap().as_str(), "a b");
        assert_eq!(NameBuf::from_name("a.b").unwrap().as_str(), "a.b");
        for bad in ["a;b", "a`b", "a/b"] {
            assert_eq!(NameBuf::from_name(bad), None, "{bad:?}");
        }
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
        let sp = NameBuf::lossy("Alice 2.0");
        assert_eq!(sp.as_str(), "Alice 2.0");
        assert!(sp.is_editable());
        let bad = NameBuf::lossy("A;b`c/d");
        assert_eq!(bad.as_str(), "A?b?c?d");
        assert!(!bad.is_editable());
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
        // 日本語1文字 = 3バイトの途中で切らない。
        let cut = parse_file(&"あ".repeat(11)).unwrap();
        assert_eq!(cut.len(), 30);
        assert!(cut.len() <= PLAYER_NAME_MAX);
        assert_eq!(parse_file(&"a".repeat(32)).unwrap().len(), 32);
        assert_eq!(parse_file(&"a".repeat(40)).unwrap().len(), 32);
    }
}
