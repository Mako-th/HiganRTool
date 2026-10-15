//! ゲーム内のメニューの「ルーム名の入力」（pure。確保しない）。
//!
//! ルーム名はゲームの中（花映塚のネット対戦のメニュー）でキーボードから打って決め、決めた名は ini の `room` に書き戻す（次の初めの値になる）。
//! ここは文字の受け方・編集・初めの値だけで、ウィンドウから文字を受けること（`WM_CHAR`）と ini の書き換えは dll（`text_input_win` / `ini_save`）の仕事。
//!
//! - 受ける文字は `room::RoomName::accepts_char`（ASCII の英数字・`-`・`_`）。全角の英数字・`－`・`＿` は半角に直して受け、
//!   ほかの文字は捨てる。32 文字（`room::ROOM_NAME_MAX`）まで。
//! - Backspace（0x08。Ctrl+Backspace の 0x7F も）で 1 文字消す。Enter（0x0D。0x0A も）で決める。決めるガードは `RoomName::parse`
//!   （空・`.` の名などは理由つきで断り、メニューが理由の行を出して入力に留まる）。
//! - 文字は UTF-16 の 1 単位で来る。基本面の外の文字（絵文字）は 2 単位に割れて来るが、どちらも `fold_unit` が捨てる。
//!
//! ルーム名は本ログに書かないので、`RoomBuf` の `Debug` は中身を書かない。

use crate::room::{ROOM_NAME_MAX, RoomName, RoomNameError};

/// 以前の版がルーム名を覚えていたファイルの名前（置き場は接続コードの置き場）。今は読まず、起動のときに ini の `room` へ移して消す。
pub const ROOM_LAST_FILE: &str = "room_last.txt";

/// `WM_CHAR` の制御文字（Backspace・Enter・改行・Ctrl+Backspace の 0x7F）。
pub const UNIT_BACKSPACE: u16 = 0x08;
pub const UNIT_DELETE: u16 = 0x7F;
pub const UNIT_ENTER: u16 = 0x0D;
pub const UNIT_LINE_FEED: u16 = 0x0A;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RoomBuf {
    buf: [u8; ROOM_NAME_MAX],
    len: usize,
}

/// `RoomBuf` を原子変数の語に詰めた長さ（32 バイトを 4 バイトずつ ＋ 長さ 1 語）。
pub const ROOM_BUF_WORDS: usize = ROOM_NAME_MAX / 4 + 1;
const _: () = assert!(ROOM_NAME_MAX.is_multiple_of(4));

impl Default for RoomBuf {
    fn default() -> Self {
        RoomBuf::new()
    }
}

impl core::fmt::Debug for RoomBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RoomBuf(<ルーム名は記録しない>)")
    }
}

impl RoomBuf {
    pub const fn new() -> RoomBuf {
        RoomBuf {
            buf: [0; ROOM_NAME_MAX],
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
        core::str::from_utf8(self.buf.get(..self.len).unwrap_or(&[])).unwrap_or("")
    }

    pub fn push(&mut self, c: char) -> bool {
        if !RoomName::accepts_char(c) {
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

    pub fn from_name(s: &str) -> Option<RoomBuf> {
        RoomName::parse(s).ok()?;
        let mut b = RoomBuf::new();
        for c in s.chars() {
            if !b.push(c) {
                return None;
            }
        }
        Some(b)
    }

    pub fn parse(&self) -> Result<RoomName, RoomNameError> {
        RoomName::parse(self.as_str())
    }

    /// 原子変数の語に詰める（見張りのスレッドへ渡す入れ物 `net_ui::RoomNameBox`）。
    pub fn to_words(&self) -> [u32; ROOM_BUF_WORDS] {
        let mut w = [0u32; ROOM_BUF_WORDS];
        for (d, chunk) in w.iter_mut().zip(self.buf.chunks(4)) {
            let mut b = [0u8; 4];
            for (x, s) in b.iter_mut().zip(chunk) {
                *x = *s;
            }
            *d = u32::from_le_bytes(b);
        }
        w[ROOM_BUF_WORDS - 1] = self.len as u32;
        w
    }

    /// `to_words` の逆。文字として受けない値・長すぎる長さは、受けるところまでで切る。
    pub fn from_words(w: &[u32; ROOM_BUF_WORDS]) -> RoomBuf {
        let len = (w[ROOM_BUF_WORDS - 1] as usize).min(ROOM_NAME_MAX);
        let mut bytes = [0u8; ROOM_NAME_MAX];
        for (chunk, word) in bytes.chunks_mut(4).zip(w) {
            for (d, s) in chunk.iter_mut().zip(word.to_le_bytes()) {
                *d = s;
            }
        }
        let mut b = RoomBuf::new();
        for &x in bytes.iter().take(len) {
            if !b.push(char::from(x)) {
                break;
            }
        }
        b
    }
}

/// ウィンドウから来た文字 1 単位を、受けられる ASCII の文字にする。半角の ASCII はそのまま、全角の `！`〜`～`（U+FF01〜U+FF5E）は半角（0x21〜0x7E）に直す。
/// 受けるかどうかは `RoomName::accepts_char` が決めるので、ここでは文字の形を直すだけ。
pub fn fold_unit(unit: u16) -> Option<char> {
    match unit {
        0x20..=0x7E => Some(char::from(unit as u8)),
        0xFF01..=0xFF5E => Some(char::from((unit - 0xFEE0) as u8)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    Char(char),
    Backspace,
    Enter,
    /// 文字として打たれたがルーム名に使えない（半角カナ・記号・日本語・空白・絵文字の割れた単位）。画面に一言出す元。
    Rejected,
    Ignore,
}

pub fn classify(unit: u16) -> TextKey {
    classify_by(unit, RoomName::accepts_char)
}

/// `classify` の、受ける文字の決まり `accepts` を渡す形。プレイヤー名の入力（`player_name`）が、制御文字・全角の直し方・
/// Backspace / Enter の読みをルーム名と同じにして、受ける文字だけ替えるために使う。
pub fn classify_by(unit: u16, accepts: fn(char) -> bool) -> TextKey {
    match unit {
        UNIT_BACKSPACE | UNIT_DELETE => TextKey::Backspace,
        UNIT_ENTER | UNIT_LINE_FEED => TextKey::Enter,
        0..0x20 => TextKey::Ignore,
        u => match fold_unit(u) {
            Some(c) if accepts(c) => TextKey::Char(c),
            _ => TextKey::Rejected,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Nothing,
    /// 使えない文字が打たれて捨てた（呼び手が「英数字と - _ だけ使えます。」を出す）。
    Rejected,
    Changed,
    Enter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoomInput {
    buf: RoomBuf,
}

impl RoomInput {
    pub fn new(initial: RoomBuf) -> RoomInput {
        RoomInput { buf: initial }
    }

    pub fn text(&self) -> &RoomBuf {
        &self.buf
    }

    pub fn feed(&mut self, unit: u16) -> Edit {
        match classify(unit) {
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
}

/// 入力の初めの値: ini の `room`（文字の決まりを通るもの）→ 空。ini の文字が決まりの外なら黙って空に落とす。
pub fn initial(ini: Option<&str>) -> RoomBuf {
    initial_from(ini).0
}

/// 初めの値の出どころ（本ログの 1 行に出どころだけ書く。名は書かない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialFrom {
    Ini,
    Empty,
}

pub fn initial_from(ini: Option<&str>) -> (RoomBuf, InitialFrom) {
    match ini.and_then(RoomBuf::from_name) {
        Some(b) => (b, InitialFrom::Ini),
        None => (RoomBuf::new(), InitialFrom::Empty),
    }
}

/// 以前の版が覚えたファイル（`ROOM_LAST_FILE`）の中身を読む（引き継ぎ用）。先頭の BOM と前後の空白は除き、最初の 1 行だけを見る。
/// 決まりの外・空・壊れた文字は `None`。
pub fn parse_last(text: &str) -> Option<RoomBuf> {
    let first = text.trim_start_matches('\u{FEFF}').lines().next()?;
    RoomBuf::from_name(first.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> RoomInput {
        let mut i = RoomInput::default();
        for u in text.encode_utf16() {
            i.feed(u);
        }
        i
    }

    #[test]
    fn full_width_alphanumerics_fold_to_ascii_and_the_rest_is_dropped() {
        assert_eq!(
            typed("ＡＢＣａｂｃ０９－＿＠").text().as_str(),
            "ABCabc09-_@"
        );
        assert_eq!(typed("Synth-Room_7").text().as_str(), "Synth-Room_7");
        assert_eq!(typed("a@b").text().as_str(), "a@b");
        for dropped in [
            "！", "．", "　", " ", ".", "/", "\\", "部", "あ", "ｱ", "?", "#", "😀",
        ] {
            let got = typed(&format!("a{dropped}b"));
            assert_eq!(got.text().as_str(), "ab", "{dropped:?}");
        }
        assert_eq!(typed("ab").text().as_str(), "ab");
        assert_eq!(typed("a\u{7F}xb").text().as_str(), "xb");
        // 範囲の境目: 全角の `！`（FF01）の手前（FF00）と `～`（FF5E）の後ろ（FF5F）は直さない。
        assert_eq!(fold_unit(0xFF00), None);
        assert_eq!(fold_unit(0xFF5F), None);
        assert_eq!(fold_unit(0xFF01), Some('!'));
        assert_eq!(fold_unit(0xFF5E), Some('~'));
    }

    #[test]
    fn the_input_filter_is_the_same_gate_as_room_name_parse() {
        let mut accepted = 0usize;
        for u in 0..=u16::MAX {
            if let TextKey::Char(c) = classify(u) {
                accepted += 1;
                assert!(
                    RoomName::parse(&c.to_string()).is_ok(),
                    "入力は受けるが parse は断る: {u:#06x}"
                );
            }
        }
        // 母数が 0 のまま通らないように数で縛る。
        assert_eq!(accepted, 65 + 65);
        for c in (0u8..=0x7F).map(char::from) {
            let parse_ok = RoomName::parse(&c.to_string()).is_ok();
            let input_ok = matches!(classify(u16::from(c as u8)), TextKey::Char(x) if x == c);
            assert_eq!(parse_ok, input_ok, "{c:?}");
        }
    }

    #[test]
    fn the_buffer_stops_at_32_and_backspace_enter_work() {
        let mut i = typed(&"a".repeat(32));
        assert_eq!(i.text().len(), 32);
        assert_eq!(i.feed(u16::from(b'b')), Edit::Nothing, "33 文字目を受けた");
        assert_eq!(i.text().len(), 32);
        assert_eq!(i.feed(UNIT_BACKSPACE), Edit::Changed);
        assert_eq!(i.text().len(), 31);
        assert_eq!(i.feed(u16::from(b'b')), Edit::Changed, "消した後に入らない");
        assert_eq!(i.text().as_str(), format!("{}b", "a".repeat(31)));
        let mut e = RoomInput::default();
        assert_eq!(e.feed(UNIT_BACKSPACE), Edit::Nothing);
        assert_eq!(e.feed(UNIT_ENTER), Edit::Enter);
        assert_eq!(e.feed(UNIT_LINE_FEED), Edit::Enter);
        assert!(e.text().is_empty(), "Enter が文字を変えた");
        let mut s = RoomInput::new(RoomBuf::from_name("last-room").unwrap());
        assert_eq!(s.feed(u16::from(b'2')), Edit::Changed);
        s.feed(UNIT_BACKSPACE);
        s.feed(UNIT_BACKSPACE);
        assert_eq!(s.text().as_str(), "last-roo");
        for u in [0x1B, 0x09, 0x01] {
            assert_eq!(s.feed(u), Edit::Nothing, "{u:#x}");
        }
    }

    #[test]
    fn unusable_typed_characters_are_reported_but_control_keys_are_not() {
        let mut i = RoomInput::new(RoomBuf::from_name("ab").unwrap());
        for dropped in [
            'ｱ', 'ﾝ', '部', 'あ', '！', '．', '　', ' ', '.', '/', '?', '#', '\u{80}',
        ] {
            let mut b = [0u16; 2];
            let u = dropped.encode_utf16(&mut b)[0];
            assert_eq!(i.feed(u), Edit::Rejected, "{dropped:?}");
        }
        for u in "😀".encode_utf16() {
            assert_eq!(i.feed(u), Edit::Rejected, "{u:#06x}");
        }
        assert_eq!(i.text().as_str(), "ab", "捨てた文字が名前に入った");
        assert_eq!(i.feed(u16::from(b'c')), Edit::Changed);
        assert_eq!(i.feed(0xFF21), Edit::Changed, "全角の A");
        assert_eq!(i.feed(UNIT_BACKSPACE), Edit::Changed);
        assert_eq!(i.feed(UNIT_ENTER), Edit::Enter);
        for u in (0u16..0x20).filter(|u| ![0x08, 0x0A, 0x0D].contains(u)) {
            assert_eq!(i.feed(u), Edit::Nothing, "{u:#x}");
        }
        assert_eq!(i.feed(0x7F), Edit::Changed, "0x7F は Backspace");
    }

    #[test]
    fn deciding_uses_the_room_name_gate() {
        assert_eq!(RoomBuf::new().parse().unwrap_err(), RoomNameError::Empty);
        assert!(typed("Synth-Room_7").text().parse().is_ok());
        assert_eq!(
            typed("Synth-Room_7").text().parse().unwrap().as_str(),
            "Synth-Room_7"
        );
        // `.` は入力が受けないが、覚えたファイルや ini からは来うる: `from_name` が断る。
        assert_eq!(RoomBuf::from_name("."), None);
        assert_eq!(RoomBuf::from_name(".."), None);
        assert_eq!(RoomBuf::from_name(""), None);
        assert_eq!(RoomBuf::from_name("a b"), None);
        assert_eq!(RoomBuf::from_name("部屋"), None);
        assert_eq!(RoomBuf::from_name(&"a".repeat(33)), None);
        assert_eq!(
            RoomBuf::from_name(&"a".repeat(32)).unwrap().as_str(),
            "a".repeat(32)
        );
    }

    #[test]
    fn the_initial_value_is_the_ini_room_or_empty() {
        assert_eq!(initial(Some("from-ini")).as_str(), "from-ini");
        for bad in [
            None,
            Some(""),
            Some("花 映塚"),
            Some("bad name"),
            Some(".."),
        ] {
            assert_eq!(initial(bad).as_str(), "", "{bad:?}");
            assert_eq!(initial_from(bad).1, InitialFrom::Empty, "{bad:?}");
        }
        assert_eq!(initial_from(Some("x")).1, InitialFrom::Ini);
    }

    #[test]
    fn the_old_remembered_file_is_read_and_a_broken_one_is_ignored() {
        let name = RoomBuf::from_name("Room_9-x").unwrap();
        assert_eq!(parse_last("Room_9-x\n"), Some(name));
        assert_eq!(parse_last("\u{FEFF}Room_9-x\r\n"), Some(name));
        assert_eq!(parse_last("  Room_9-x  \n"), Some(name));
        assert_eq!(parse_last("Room_9-x"), Some(name));
        for bad in ["", "\n", "   \n", "a b\n", "部屋\n", "..\n", "\nRoom_9-x\n"] {
            assert_eq!(parse_last(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn words_round_trip_and_garbage_words_are_cut() {
        for n in [0usize, 1, 3, 4, 5, 31, 32] {
            let name = RoomBuf::from_name(&"Ab1-_".repeat(7)[..n]).unwrap_or_default();
            assert_eq!(name.len(), n);
            assert_eq!(RoomBuf::from_words(&name.to_words()), name, "{n}");
        }
        let mut w = [0xFFFF_FFFFu32; ROOM_BUF_WORDS];
        assert!(
            RoomBuf::from_words(&w).is_empty(),
            "文字でないバイトを入れた"
        );
        w[0] = u32::from_le_bytes(*b"ab\0c");
        w[ROOM_BUF_WORDS - 1] = 4;
        assert_eq!(
            RoomBuf::from_words(&w).as_str(),
            "ab",
            "文字でないバイトで切る"
        );
        let secret = RoomBuf::from_name("secret-room").unwrap();
        assert!(!format!("{secret:?}").contains("secret"));
        assert!(!format!("{:?}", RoomInput::new(secret)).contains("secret"));
    }
}
