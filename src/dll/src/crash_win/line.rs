//! 例外の1行を固定長のバッファへ整形する（pure）。例外ハンドラの中から呼ぶのでヒープを確保せずパニックしない。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::names::LocalTime;

/// 行にする別々の例外（コードと場所のペア）の上限。以降の別の例外は数えるだけ。
pub const MAX_LINES: u32 = 32;

pub use higanr_core::exception_names::{
    ACCESS_VIOLATION, IN_PAGE_ERROR, STACK_OVERFLOW, fatal_name,
};

/// 行にした例外の（コード, 場所）のペアを覚えておく固定長の表。
/// ゲームやOSが自分で受けて続ける無害な例外が同じ場所から何度も投げられても、行の枠を使い切らないために使う。
/// ハンドラの中から呼ぶので、確保せず、ロックを取らず、パニックしない。
pub struct Kinds {
    /// 上位32ビットがコード、下位32ビットが場所。0は空き（記録する例外のコードは0でない）。
    slots: [AtomicU64; MAX_LINES as usize],
    claimed: AtomicU32,
    repeats: AtomicU32,
    dropped: AtomicU32,
}

/// `Kinds::note`の答え。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// 初めてのペア。何件目か（1から）を添えて行にする。
    Write(u32),
    /// 行にしたペアと同じ。数えるだけ。
    Repeat,
    /// 初めてのペアだが枠が無い。数えるだけ。
    Dropped,
}

impl Kinds {
    pub const fn new() -> Self {
        Kinds {
            slots: [const { AtomicU64::new(0) }; MAX_LINES as usize],
            claimed: AtomicU32::new(0),
            repeats: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        }
    }

    /// 同じペアが同時に来ると両方が空きを確保して同じ行が2回出ることがある（枠は32で頭打ちなので害は小さい）。
    pub fn note(&self, code: u32, addr: u32) -> Note {
        let key = (u64::from(code) << 32) | u64::from(addr);
        let used = (self.claimed.load(Ordering::Acquire) as usize).min(self.slots.len());
        let known = self.slots.get(..used).unwrap_or(&[]);
        if known.iter().any(|k| k.load(Ordering::Acquire) == key) {
            self.repeats.fetch_add(1, Ordering::Relaxed);
            return Note::Repeat;
        }
        if used >= self.slots.len() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Note::Dropped;
        }
        let slot = self.claimed.fetch_add(1, Ordering::AcqRel) as usize;
        match self.slots.get(slot) {
            Some(s) => {
                s.store(key, Ordering::Release);
                Note::Write(slot as u32 + 1)
            }
            None => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Note::Dropped
            }
        }
    }

    /// 行にした別々の例外の数。
    pub fn lines(&self) -> u32 {
        self.claimed.load(Ordering::Acquire).min(MAX_LINES)
    }

    /// 行にしたペアと同じで、数えただけの回数。
    pub fn repeats(&self) -> u32 {
        self.repeats.load(Ordering::Relaxed)
    }

    /// 枠が無くて行にしなかった別の例外の回数。
    pub fn dropped(&self) -> u32 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for Kinds {
    fn default() -> Self {
        Self::new()
    }
}

/// 例外の場所のモジュール。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place<'a> {
    /// ファイル名（UTF-8）と、モジュールの先頭からのRVA。
    Module(&'a [u8], u32),
    /// どのモジュールにも入っていない（または取得できなかった）。
    Unknown,
    /// スタックオーバーフローではAPIを呼ばないので調べない。
    NotLooked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen<'a> {
    /// 何件目か（1から）。
    pub nth: u32,
    pub code: u32,
    pub addr: u32,
    pub thread: u32,
    pub time: Option<LocalTime>,
    pub place: Place<'a>,
    /// アクセス違反の（種類, 対象のアドレス）。種類はExceptionInformation[0]（0読み・1書き込み・8実行）。
    pub access: Option<(usize, u32)>,
}

/// `s`を1行（改行つき）にして`out`の先頭へ書き、書いたバイト数を返す。
/// 入らない部品からは丸ごと省く（UTF-8の途中で切らない。後ろの部品だけが残って読み違えることもない）。
pub fn format_seen(out: &mut [u8], s: &Seen<'_>) -> usize {
    let mut b = Buf::new(out);
    b.put("例外を見ました（落ちたとは限らない・".as_bytes());
    b.dec(s.nth);
    b.put(" 件目".as_bytes());
    if s.nth == MAX_LINES {
        b.put("。以降の別の例外は数えるだけ".as_bytes());
    }
    b.put("）: ".as_bytes());
    if let Some(t) = s.time {
        b.dec_pad(t.hour.into(), 2);
        b.put(b":");
        b.dec_pad(t.minute.into(), 2);
        b.put(b":");
        b.dec_pad(t.second.into(), 2);
        b.put(b".");
        b.dec_pad(t.millis.into(), 3);
        b.put(b" ");
    }
    b.hex8(s.code);
    b.put(b" ");
    b.put(fatal_name(s.code).unwrap_or("不明なコード").as_bytes());
    if let Some((kind, target)) = s.access {
        b.put("（".as_bytes());
        b.hex8(target);
        b.put(" の".as_bytes());
        match kind {
            0 => b.put("読み".as_bytes()),
            1 => b.put("書き込み".as_bytes()),
            8 => b.put("実行（DEP）".as_bytes()),
            k => {
                b.put("種類 ".as_bytes());
                b.dec(k as u32);
            }
        }
        b.put("）".as_bytes());
    }
    b.put(" / 場所 ".as_bytes());
    b.hex8(s.addr);
    match s.place {
        Place::Module(name, rva) => {
            b.put(b" ");
            b.put(name);
            b.put(b"+");
            b.hex8(rva);
        }
        Place::Unknown => b.put("（モジュールの外）".as_bytes()),
        Place::NotLooked => b.put("（モジュールは引かない）".as_bytes()),
    }
    b.put(" / スレッド ".as_bytes());
    b.dec(s.thread);
    b.finish()
}

/// パス（UTF-16）の最後の部分（ファイル名）をUTF-8で`out`へ書き、書いたバイト数を返す。入らない文字の手前で止める。
/// 不正なUTF-16は置き換え文字にする。フォルダはユーザー名を含みうるので書かない。
pub fn file_name_utf8(wide: &[u16], out: &mut [u8]) -> usize {
    let start = wide
        .iter()
        .rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
        .map_or(0, |i| i + 1);
    let mut len = 0;
    for c in char::decode_utf16(wide.iter().skip(start).copied()) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mut tmp = [0u8; 4];
        let enc = c.encode_utf8(&mut tmp).as_bytes();
        let Some(dst) = out.get_mut(len..len + enc.len()) else {
            break;
        };
        dst.copy_from_slice(enc);
        len += enc.len();
    }
    len
}

/// 末尾の1バイトを改行のために残す書き手。
struct Buf<'a> {
    out: &'a mut [u8],
    len: usize,
    full: bool,
}

impl<'a> Buf<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        let full = out.is_empty();
        Buf { out, len: 0, full }
    }

    fn put(&mut self, bytes: &[u8]) {
        if self.full {
            return;
        }
        let end = self.len + bytes.len();
        // 最後の1バイトは改行の分。
        let room = self.out.len().saturating_sub(1);
        match self.out.get_mut(self.len..end) {
            Some(dst) if end <= room => {
                dst.copy_from_slice(bytes);
                self.len = end;
            }
            _ => self.full = true,
        }
    }

    fn hex8(&mut self, v: u32) {
        const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
        let mut s = *b"0x00000000";
        for (i, d) in s[2..].iter_mut().enumerate() {
            *d = DIGITS[((v >> (28 - 4 * i)) & 0xF) as usize];
        }
        self.put(&s);
    }

    fn dec(&mut self, v: u32) {
        self.dec_pad(v, 1);
    }

    /// 10進。`width`桁に満たなければ先頭を0で埋める（最大10桁）。
    fn dec_pad(&mut self, mut v: u32, width: usize) {
        let mut s = [b'0'; 10];
        let mut n = 0;
        while v > 0 || n < width.clamp(1, 10) {
            s[9 - n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
            if n == 10 {
                break;
            }
        }
        self.put(&s[10 - n..]);
    }

    fn finish(self) -> usize {
        match self.out.get_mut(self.len) {
            Some(nl) => {
                *nl = b'\n';
                self.len + 1
            }
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample<'a>(name: &'a [u8]) -> Seen<'a> {
        Seen {
            nth: 1,
            code: ACCESS_VIOLATION,
            addr: 0x0042_A1B3,
            thread: 4321,
            time: Some(LocalTime {
                year: 2026,
                month: 10,
                day: 14,
                hour: 9,
                minute: 5,
                second: 7,
                millis: 42,
            }),
            place: Place::Module(name, 0x0002_A1B3),
            access: Some((1, 0x10)),
        }
    }

    fn text(s: &Seen<'_>, cap: usize) -> String {
        let mut buf = vec![0u8; cap];
        let n = format_seen(&mut buf, s);
        String::from_utf8(buf[..n].to_vec()).expect("UTF-8 の途中で切らない")
    }

    /// フィールドの順・16進の桁・時刻の0埋め・回数の文も見る。
    #[test]
    fn an_access_violation_line_names_module_rva_and_target() {
        assert_eq!(
            text(&sample(b"th09.exe"), 512),
            "例外を見ました（落ちたとは限らない・1 件目）: 09:05:07.042 0xC0000005 アクセス違反（0x00000010 の書き込み） / 場所 0x0042A1B3 th09.exe+0x0002A1B3 / スレッド 4321\n"
        );
    }

    #[test]
    fn the_last_written_line_says_the_rest_are_only_counted() {
        let mut s = sample(b"d3d8.dll");
        s.nth = MAX_LINES;
        assert!(text(&s, 512).contains("32 件目。以降の別の例外は数えるだけ）"));
        s.nth = MAX_LINES - 1;
        assert!(!text(&s, 512).contains("以降"));
    }

    #[test]
    fn stack_overflow_and_unknown_places_have_their_own_words() {
        let s = Seen {
            nth: 2,
            code: STACK_OVERFLOW,
            addr: 0x0040_1000,
            thread: 7,
            time: None,
            place: Place::NotLooked,
            access: None,
        };
        assert_eq!(
            text(&s, 512),
            "例外を見ました（落ちたとは限らない・2 件目）: 0xC00000FD スタックオーバーフロー / 場所 0x00401000（モジュールは引かない） / スレッド 7\n"
        );
        let u = Seen {
            place: Place::Unknown,
            access: Some((8, 0x1234_5678)),
            code: ACCESS_VIOLATION,
            ..s
        };
        let t = text(&u, 512);
        assert!(t.contains("（0x12345678 の実行（DEP））"), "{t}");
        assert!(t.contains("（モジュールの外）"), "{t}");
    }

    #[test]
    fn short_buffers_never_overflow_and_end_with_a_newline() {
        let s = sample("ハイガン.dll".as_bytes());
        let whole = text(&s, 512);
        for cap in 0..whole.len() + 4 {
            let mut buf = vec![0xAAu8; cap + 8];
            let n = format_seen(&mut buf[..cap], &s);
            assert!(n <= cap);
            assert!(buf[cap..].iter().all(|&b| b == 0xAA), "cap {cap}");
            if cap == 0 {
                assert_eq!(n, 0);
                continue;
            }
            let t = std::str::from_utf8(&buf[..n]).expect("UTF-8");
            assert!(t.ends_with('\n'), "cap {cap}");
            assert!(
                whole.starts_with(t.trim_end_matches('\n')),
                "cap {cap}: {t}"
            );
        }
    }

    #[test]
    fn the_code_names_come_from_the_shared_table() {
        for code in [0xC000_0094u32, 0xC000_0409, 0xC000_0374] {
            let mut s = sample(b"a");
            s.code = code;
            s.access = None;
            let want = higanr_core::exception_names::fatal_name(code).unwrap();
            assert!(
                text(&s, 512).contains(&format!("0x{code:08X}")),
                "{code:08X}"
            );
            assert!(text(&s, 512).contains(want), "{code:08X}: {want}");
        }
    }

    #[test]
    fn decimals_cover_zero_and_the_largest_value() {
        let mut s = sample(b"a");
        s.thread = 0;
        assert!(text(&s, 512).ends_with("スレッド 0\n"));
        s.thread = u32::MAX;
        assert!(text(&s, 512).ends_with("スレッド 4294967295\n"));
    }

    #[test]
    fn file_name_drops_folders_and_cuts_on_char_boundaries() {
        let wide: Vec<u16> = format!("C:{0}Users{0}someone{0}ゲーム{0}HiganRTool.dll", '\\')
            .encode_utf16()
            .collect();
        let mut out = [0u8; 64];
        let n = file_name_utf8(&wide, &mut out);
        assert_eq!(&out[..n], b"HiganRTool.dll");

        let wide: Vec<u16> = "x/彼岸.dll".encode_utf16().collect();
        let mut out = [0u8; 4];
        let n = file_name_utf8(&wide, &mut out);
        assert_eq!(&out[..n], "彼".as_bytes(), "2 文字目は入らないので落とす");

        let n = file_name_utf8(&[0xD800, u16::from(b'a')], &mut [0u8; 8]);
        assert_eq!(n, 4, "孤立したサロゲートは置き換え文字（3 バイト）");
        assert_eq!(file_name_utf8(&[], &mut [0u8; 8]), 0);
    }

    #[test]
    fn a_repeat_of_the_same_code_and_place_does_not_use_a_line() {
        let k = Kinds::new();
        assert_eq!(k.note(ACCESS_VIOLATION, 0x0040_1000), Note::Write(1));
        for _ in 0..100 {
            assert_eq!(k.note(ACCESS_VIOLATION, 0x0040_1000), Note::Repeat);
        }
        // 場所だけ・コードだけが違うものは別のペア。
        assert_eq!(k.note(ACCESS_VIOLATION, 0x0040_1001), Note::Write(2));
        assert_eq!(k.note(0xC000_001D, 0x0040_1000), Note::Write(3));
        assert_eq!((k.lines(), k.repeats(), k.dropped()), (3, 100, 0));
    }

    #[test]
    fn distinct_pairs_fill_thirty_two_lines_then_are_only_counted() {
        let k = Kinds::new();
        for i in 0..MAX_LINES {
            assert_eq!(k.note(ACCESS_VIOLATION, 0x1000 + i), Note::Write(i + 1));
        }
        for i in 0..5 {
            assert_eq!(k.note(ACCESS_VIOLATION, 0x9000 + i), Note::Dropped);
        }
        // 上限の後でも行にしたペアの繰り返しは繰り返しとして数える。
        assert_eq!(k.note(ACCESS_VIOLATION, 0x1000), Note::Repeat);
        assert_eq!(
            k.note(ACCESS_VIOLATION, 0x1000 + MAX_LINES - 1),
            Note::Repeat
        );
        assert_eq!((k.lines(), k.repeats(), k.dropped()), (MAX_LINES, 2, 5));
    }

    #[test]
    fn many_threads_never_take_more_than_the_slots() {
        let k = Kinds::new();
        std::thread::scope(|sc| {
            for t in 0..8u32 {
                let k = &k;
                sc.spawn(move || {
                    for i in 0..100 {
                        k.note(ACCESS_VIOLATION, t * 1000 + i);
                    }
                });
            }
        });
        assert_eq!(k.lines(), MAX_LINES);
        assert_eq!(k.dropped() + k.repeats() + MAX_LINES, 800);
    }
}
