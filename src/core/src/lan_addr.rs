//! LAN のホストのアドレス（`IPv4:ポート`）の入力と読み方（pure。確保しない）。
//!
//! メニューの「クライアント」「観戦」で、ホストが画面に出したアドレスをキーボードで打つ。文字の受け方はルーム名・プレイヤー名の入力と同じ
//! （`room_input::classify_by`。全角の数字・記号は半角に直して受ける）で、受ける文字だけが違う（数字・`.`・`:`）。決めたアドレスは ini の
//! `[netplay] lan_host` に書き戻し、次に打つときの初めの値にする（書き戻すのは dll の仕事）。

use core::net::{Ipv4Addr, SocketAddrV4};

use crate::guard::{self, Scope};
use crate::room_input::{Edit, TextKey, classify_by};

/// ポートを省いたときのポート。ホストが待つポートの既定でもある（ゲームの受信ソケットの既定 `bind = auto:17723` と同じ数）。
pub const DEFAULT_PORT: u16 = 17723;

/// 入力できる長さ。`255.255.255.255:65535` の 21 文字。
pub const ADDR_MAX: usize = 21;

/// 打てる文字（数字・`.`・`:`）。
pub fn accepts_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.' || c == ':'
}

/// LAN のホストの待ち受けの場所。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanAddr {
    pub ip: Ipv4Addr,
    pub port: u16,
}

impl LanAddr {
    pub fn socket(self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip, self.port)
    }

    /// `ADDR_MAX` 文字までの入力の形（初めの値にするとき）。ポートが既定なら省く。
    pub fn to_buf(self) -> AddrBuf {
        let mut b = AddrBuf::new();
        let mut tmp = [0u8; ADDR_MAX];
        let text = write_addr(&mut tmp, self);
        for c in text.chars() {
            b.push(c);
        }
        b
    }
}

/// `addr` を `tmp` に書いた文字列（`192.168.0.2` か `192.168.0.2:19000`。ポートが既定なら省く）。確保しない。
fn write_addr(tmp: &mut [u8; ADDR_MAX], a: LanAddr) -> &str {
    let mut n = 0;
    let mut put = |b: u8| {
        if let Some(s) = tmp.get_mut(n) {
            *s = b;
            n += 1;
        }
    };
    for (i, o) in a.ip.octets().into_iter().enumerate() {
        if i > 0 {
            put(b'.');
        }
        put_decimal(&mut put, u32::from(o));
    }
    if a.port != DEFAULT_PORT {
        put(b':');
        put_decimal(&mut put, u32::from(a.port));
    }
    core::str::from_utf8(tmp.get(..n).unwrap_or(&[])).unwrap_or("")
}

fn put_decimal(put: &mut impl FnMut(u8), mut v: u32) {
    let mut digits = [0u8; 5];
    let mut n = 0;
    loop {
        if let Some(d) = digits.get_mut(n) {
            *d = b'0' + (v % 10) as u8;
        }
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        put(digits.get(n).copied().unwrap_or(b'0'));
    }
}

/// 打っているアドレス（ASCII）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AddrBuf {
    buf: [u8; ADDR_MAX],
    len: usize,
}

impl Default for AddrBuf {
    fn default() -> Self {
        AddrBuf::new()
    }
}

impl AddrBuf {
    pub const fn new() -> AddrBuf {
        AddrBuf {
            buf: [0; ADDR_MAX],
            len: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_str(&self) -> &str {
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

    /// ini の値から（読めなければ空）。決まりの外のアドレスは捨てて空にする。
    pub fn from_ini(s: Option<&str>) -> AddrBuf {
        match s.map(str::trim).and_then(|t| parse(t).ok()) {
            Some(a) => a.to_buf(),
            None => AddrBuf::new(),
        }
    }

    pub fn parse(&self) -> Result<LanAddr, AddrWhy> {
        parse(self.as_str())
    }
}

/// 打ったアドレスを断る理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrWhy {
    Empty,
    /// `192.168.0.2` か `192.168.0.2:17723` の形でない。
    Form,
    /// ポートが 1〜65535 でない。
    Port,
    /// LAN のアドレス（私設・リンクローカル。試験用に loopback も）でない。
    NotLan,
}

/// 打ったアドレスを読む。`IPv4` か `IPv4:ポート`（前後の空白は除く）。ポートを省けば `DEFAULT_PORT`。
pub fn parse(s: &str) -> Result<LanAddr, AddrWhy> {
    let t = s.trim();
    if t.is_empty() {
        return Err(AddrWhy::Empty);
    }
    let (host, port) = match t.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (t, None),
    };
    let ip: Ipv4Addr = host.parse().map_err(|_| AddrWhy::Form)?;
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.len() > 5 {
                return Err(AddrWhy::Port);
            }
            match p.parse::<u16>() {
                Ok(v) if v != 0 => v,
                _ => return Err(AddrWhy::Port),
            }
        }
    };
    if !guard::reach(core::net::IpAddr::V4(ip)).is_some_and(|r| r <= Scope::Lan) {
        return Err(AddrWhy::NotLan);
    }
    Ok(LanAddr { ip, port })
}

/// アドレスの入力（ウィンドウから来た文字を受けて編集する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AddrInput {
    buf: AddrBuf,
}

impl AddrInput {
    pub fn new(initial: AddrBuf) -> AddrInput {
        AddrInput { buf: initial }
    }

    pub fn text(&self) -> &AddrBuf {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr::new(a, b, c, d)
    }

    fn typed(text: &str) -> AddrInput {
        let mut i = AddrInput::default();
        for u in text.encode_utf16() {
            i.feed(u);
        }
        i
    }

    #[test]
    fn the_port_is_optional_and_defaults() {
        assert_eq!(
            parse("192.168.0.2"),
            Ok(LanAddr {
                ip: ip(192, 168, 0, 2),
                port: DEFAULT_PORT
            })
        );
        assert_eq!(
            parse(" 10.0.0.5:19000 "),
            Ok(LanAddr {
                ip: ip(10, 0, 0, 5),
                port: 19000
            })
        );
        assert_eq!(
            parse("169.254.7.9:1").map(|a| a.port),
            Ok(1),
            "ポート 1 は使える"
        );
        assert_eq!(parse("172.16.0.1:65535").map(|a| a.port), Ok(65535));
    }

    #[test]
    fn every_bad_form_says_why() {
        for (text, why) in [
            ("", AddrWhy::Empty),
            ("   ", AddrWhy::Empty),
            ("192.168.0", AddrWhy::Form),
            ("192.168.0.2.1", AddrWhy::Form),
            ("192.168.0.256", AddrWhy::Form),
            ("192.168.0.2:", AddrWhy::Port),
            ("192.168.0.2:0", AddrWhy::Port),
            ("192.168.0.2:65536", AddrWhy::Port),
            ("192.168.0.2:123456", AddrWhy::Port),
            ("192.168.0.2:12:3", AddrWhy::Port),
            ("192.168.0.2:-1", AddrWhy::Port),
            (":17723", AddrWhy::Form),
            ("a.b.c.d", AddrWhy::Form),
            ("8.8.8.8", AddrWhy::NotLan),
            ("100.64.0.1", AddrWhy::NotLan),
            ("203.0.113.5:17723", AddrWhy::NotLan),
            ("0.0.0.0", AddrWhy::NotLan),
            ("255.255.255.255", AddrWhy::NotLan),
            ("224.0.0.1", AddrWhy::NotLan),
        ] {
            assert_eq!(parse(text), Err(why), "{text:?}");
        }
        // 試験のための loopback は通す。
        assert!(parse("127.0.0.1:47001").is_ok());
    }

    #[test]
    fn typing_folds_full_width_and_refuses_other_characters() {
        let i = typed("１９２．１６８．０．２：１９０００");
        assert_eq!(i.text().as_str(), "192.168.0.2:19000");
        assert_eq!(i.text().parse().map(|a| a.port), Ok(19000));
        let mut i = typed("192");
        assert_eq!(i.feed(u16::from(b'x')), Edit::Rejected);
        assert_eq!(i.feed(u16::from(b' ')), Edit::Rejected);
        assert_eq!(i.feed(0x08), Edit::Changed);
        assert_eq!(i.text().as_str(), "19");
        assert_eq!(i.feed(0x0D), Edit::Enter);
        assert_eq!(AddrInput::default().feed(0x08), Edit::Nothing);
    }

    #[test]
    fn typing_stops_at_the_longest_address() {
        let mut i = AddrInput::default();
        for _ in 0..40 {
            i.feed(u16::from(b'1'));
        }
        assert_eq!(i.text().as_str().len(), ADDR_MAX);
        assert_eq!("255.255.255.255:65535".len(), ADDR_MAX);
    }

    #[test]
    fn an_address_round_trips_through_the_input_buffer_and_the_ini_text() {
        for a in [
            LanAddr {
                ip: ip(192, 168, 1, 20),
                port: DEFAULT_PORT,
            },
            LanAddr {
                ip: ip(10, 255, 0, 3),
                port: 19000,
            },
            LanAddr {
                ip: ip(169, 254, 255, 254),
                port: 65535,
            },
        ] {
            let b = a.to_buf();
            assert_eq!(b.parse(), Ok(a), "{}", b.as_str());
            assert_eq!(AddrBuf::from_ini(Some(b.as_str())).parse(), Ok(a));
        }
        // 既定のポートは書かない。
        let a = LanAddr {
            ip: ip(192, 168, 1, 20),
            port: DEFAULT_PORT,
        };
        assert_eq!(a.to_buf().as_str(), "192.168.1.20");
        // ini の値が決まりの外なら空から始める。
        for bad in [
            None,
            Some(""),
            Some("8.8.8.8"),
            Some("x"),
            Some("1.2.3.4:0"),
        ] {
            assert!(AddrBuf::from_ini(bad).is_empty(), "{bad:?}");
        }
    }
}
