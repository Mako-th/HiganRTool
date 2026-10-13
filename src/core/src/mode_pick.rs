//! 対戦の形（rollback か delay 方式か・rollback の入力の遅れ D）を、接続コードのウィンドウで選ぶための決まり。
//!
//! ウィンドウ（ローダー）は打たれた文字を読んで `mode.txt` を書き、dll の見張りがそれを読む。文字の読み方・ファイルの形・
//! ini の `mode` / `delay` の読み方は両者で同じでなければならず（食い違うとウィンドウは「delay 方式」と言うのに
//! dll は rollback を名乗る）、dll のクレートはローダーから引けないので core に置く。Win32 にもゲームのメモリにも触らない。
//!
//! ## ウィンドウで選べるもの（`Pick`）
//!
//! 打つ文字は `0`〜`8`（rollback の入力の遅れ D のフレーム数。範囲は `delay_pick::bounds`）と `delay`（delay 方式）だけ。
//! 前後の空白は落とし、`delay` は大文字・小文字を問わず、数字は全角（`０`〜`８`）も受ける。`9` は auto の印
//! （`delay_pick::AUTO`）でウィンドウの選択ではない。
//! 接続コードは 100 文字を越える base64url なので、これらの文字と一致しえない。
//!
//! ## 受け渡しのファイル `mode.txt`（`code_files::MODE_FILE`）
//!
//! ```text
//! pid = 1234
//! pick = 1
//! ```
//!
//! `pid` は**対象のゲームのプロセス番号**。dll は自分の番号と一致するときだけ使う。置き場（`code\`）は実行を
//! またいで残るので、前の実行の選択を今の実行が黙って使わないため。書式は ini と同じ（`ini::parse`）。

#![forbid(unsafe_code)]

use crate::delay_pick::DelayChoice;
use crate::ini::{self, Ini};
use crate::rollback;
use crate::wire::Mode;

/// 望むモードの既定（ini の `mode`）。オフにするなら `mode = delay`。
pub const DEFAULT_MODE: Mode = Mode::Rollback;

/// 選べる D の上限。マッチ中の Ctrl＋数字・ウィンドウで打つ文字・ホストがゲストの要求を詰める範囲の上限（引く先は `delay_pick::bounds`）。
/// リング（128 枠）はマッチ中の `apply − F` が R + D ≤ 24 で余り、要求の期間（INPUT 1 通のワードは最大 32）は D = 8 で往復 400 ms まで。
pub const MAX_PICK_DELAY: u8 = 8;

/// ini の遅延のキー。メニューの「ディレイ」は 1 つなので、rollback の D と delay 方式の希望 latency の両方をこの 1 つから作る。
pub const DELAY_KEY: &str = "delay";

/// ini の `mode` と `delay` から決まる、この実行の既定の形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub mode: Mode,
    /// rollback のときの D（`delay = auto` なら既定値 `rollback::DEFAULT_DELAY`。auto かは `ini_delay` で見る）。
    pub delay: u8,
}

/// ini の `delay` の値 `v`（書いていなければ `None`）を読む。書かなければ auto。`auto`（大文字・小文字を問わない）か、
/// 10 進か `0x` の 16 進の 0〜`MAX_PICK_DELAY`。それ以外は断る。
pub fn ini_delay(v: Option<&str>) -> Result<DelayChoice, String> {
    let Some(v) = v else {
        return Ok(DelayChoice::Auto);
    };
    if v.trim().eq_ignore_ascii_case("auto") {
        return Ok(DelayChoice::Auto);
    }
    let n = num(v, DELAY_KEY)?;
    match u8::try_from(n) {
        Ok(d) if d <= MAX_PICK_DELAY => Ok(DelayChoice::Fixed(d)),
        _ => Err(format!(
            "{DELAY_KEY} の値 {n} は auto か 0〜{MAX_PICK_DELAY}"
        )),
    }
}

/// 10 進か `0x` の 16 進（dll の `net_config::num` と同じ形）。
fn num(s: &str, key: &str) -> Result<u64, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse::<u64>(),
    };
    r.map_err(|_| format!("{key} の値 {s:?} が数として読めない"))
}

/// ini の `mode` と `delay` を読む。`get` はキー → 値。値の誤りは理由つきで断る。
pub fn ini_shape<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Shape, String> {
    let mode = match get("mode") {
        None => DEFAULT_MODE,
        Some("rollback") => Mode::Rollback,
        Some("delay") => Mode::Delay,
        Some(v) => return Err(format!("mode の値 {v:?} は rollback か delay")),
    };
    let delay = match ini_delay(get(DELAY_KEY))? {
        DelayChoice::Auto => rollback::DEFAULT_DELAY,
        DelayChoice::Fixed(d) => d,
    };
    Ok(Shape { mode, delay })
}

/// ini の本文の `[section]` から `ini_shape`。読めない ini は None。
pub fn ini_shape_of_text(text: &str, section: &str) -> Option<Result<Shape, String>> {
    let ini: Ini = ini::parse(text).ok()?;
    Some(ini_shape(|k| ini.get(section, k)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// rollback。D（0〜`MAX_PICK_DELAY`）。
    Rollback(u8),
    Delay,
}

impl Pick {
    /// ウィンドウに打たれた文字（`choose`・ファイルの `pick` の値も）。読めなければ None（接続コードとして扱う側へ戻る）。
    pub fn parse(s: &str) -> Option<Pick> {
        let t = s.trim().trim_start_matches('\u{FEFF}').trim();
        if t.eq_ignore_ascii_case("delay") {
            return Some(Pick::Delay);
        }
        let mut cs = t.chars();
        let (c, rest) = (cs.next()?, cs.next());
        if rest.is_some() {
            return None;
        }
        // ASCII の 0〜9 と全角の ０〜９。範囲の外（auto の 9 を含む）は選択ではない。
        let d = match c {
            '0'..='9' => c as u8 - b'0',
            '\u{FF10}'..='\u{FF19}' => (c as u32 - 0xFF10) as u8,
            _ => return None,
        };
        (d <= crate::delay_pick::bounds(Mode::Rollback).1).then_some(Pick::Rollback(d))
    }

    /// `parse` が読める ASCII の文字（ファイルと run に書く形）。
    pub fn token(self) -> String {
        match self {
            Pick::Rollback(d) => d.to_string(),
            Pick::Delay => "delay".to_string(),
        }
    }
}

pub fn render(pid: u32, pick: Pick) -> String {
    format!(
        "; 接続コードのウィンドウで選んだ対戦の形。pid は対象のゲームのプロセス番号（別の実行の残りを使わないため）\r\npid = {pid}\r\npick = {}\r\n",
        pick.token()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRead {
    /// 自分の実行の選択。
    Mine(Pick),
    /// 別の実行のもの（前の実行の残り。使わない）。
    OtherRun { pid: u32 },
    /// 読めない（形が違う・文字が足りない・UTF-8 でない）。使わない。
    Unreadable,
}

pub fn read_for(bytes: &[u8], my_pid: u32) -> FileRead {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return FileRead::Unreadable;
    };
    let Ok(ini) = ini::parse(text) else {
        return FileRead::Unreadable;
    };
    let (Some(pid), Some(pick)) = (ini.get("", "pid"), ini.get("", "pick")) else {
        return FileRead::Unreadable;
    };
    let (Ok(pid), Some(pick)) = (pid.parse::<u32>(), Pick::parse(pick)) else {
        return FileRead::Unreadable;
    };
    if pid == my_pid {
        FileRead::Mine(pick)
    } else {
        FileRead::OtherRun { pid }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Secret;
    use crate::candidate::Candidates;
    use crate::code::{self, Code};
    use crate::handshake::Name;

    #[test]
    fn typed_picks_are_exactly_zero_to_eight_and_delay() {
        for (s, want) in [
            ("0", Pick::Rollback(0)),
            ("1", Pick::Rollback(1)),
            ("2", Pick::Rollback(2)),
            ("3", Pick::Rollback(3)),
            ("8", Pick::Rollback(8)),
            ("\u{FF18}", Pick::Rollback(8)),
            (" 1 ", Pick::Rollback(1)),
            ("\u{3000}２\u{3000}", Pick::Rollback(2)),
            ("０", Pick::Rollback(0)),
            ("\u{FEFF}1\r", Pick::Rollback(1)),
            ("delay", Pick::Delay),
            ("DELAY", Pick::Delay),
            ("  Delay\t", Pick::Delay),
        ] {
            assert_eq!(Pick::parse(s), Some(want), "{s:?}");
        }
        for s in [
            "",
            " ",
            "9",
            "\u{FF19}",
            "10",
            "01",
            "-1",
            "+1",
            "1.0",
            "rollback",
            "delay 1",
            "de lay",
            "ｄｅｌａｙ",
            "d",
            "１２",
            "0x1",
            "一",
        ] {
            assert_eq!(Pick::parse(s), None, "{s:?}");
        }
        for p in [
            Pick::Rollback(0),
            Pick::Rollback(1),
            Pick::Rollback(2),
            Pick::Rollback(8),
            Pick::Delay,
        ] {
            assert_eq!(Pick::parse(&p.token()), Some(p));
        }
    }

    #[test]
    fn a_connection_code_is_never_a_pick() {
        let secret = Secret([0x21; 16]);
        let name = Name::new("t");
        let mk = |side| Code {
            side,
            session_id: 0x5EED_C0DE,
            secret: (side != code::Side::Responder).then_some(secret),
            name,
            nat: crate::nat::NatMark::UNKNOWN,
            candidates: Candidates::from_addrs(&["127.0.0.1:41001".parse().unwrap()]).unwrap(),
            expires: 1_900_000_000,
        };
        let init = code::write_initiator(&mk(code::Side::Initiator)).unwrap();
        let reply = code::write_reply(&mk(code::Side::Responder), &secret).unwrap();
        let spec = code::write_spectate(&mk(code::Side::Spectate), &[9; 32]).unwrap();
        for c in [&init, &reply, &spec] {
            assert_eq!(Pick::parse(c), None, "{c}");
            assert!(c.chars().count() >= code::MIN_CODE_CHARS, "{c}");
        }
        // 選択の文字（最長は `delay` の 5 文字）は、どの接続コードよりもずっと短い。
        assert!(code::MIN_CODE_CHARS > "delay".len() * 8);
    }

    #[test]
    fn the_file_round_trips_and_only_my_pid_counts() {
        for p in [Pick::Rollback(0), Pick::Rollback(2), Pick::Delay] {
            let t = render(4242, p);
            assert_eq!(read_for(t.as_bytes(), 4242), FileRead::Mine(p));
            assert_eq!(
                read_for(t.as_bytes(), 4243),
                FileRead::OtherRun { pid: 4242 }
            );
        }
        for bad in [
            "",
            "pid = 7\r\n",
            "pick = 1\r\n",
            "pid = x\r\npick = 1\r\n",
            "pid = 7\r\npick = 9\r\n",
            "pid = 7\r\npick = rollback\r\n",
            "pid = -7\r\npick = 1\r\n",
            "pid = 7\r\npick = 1\r\npick = 0\r\n",
            "garbage",
        ] {
            assert_eq!(read_for(bad.as_bytes(), 7), FileRead::Unreadable, "{bad:?}");
        }
        assert_eq!(read_for(&[0xFF, 0xFE, 0x00], 7), FileRead::Unreadable);
        assert_eq!(
            read_for(
                "\u{FEFF}# memo\r\npid = 7\r\npick = delay\r\n".as_bytes(),
                7
            ),
            FileRead::Mine(Pick::Delay)
        );
    }

    #[test]
    fn the_ini_shape_has_the_defaults_and_refuses_bad_values() {
        let shape = |t: &str| ini_shape_of_text(t, "netplay").unwrap();
        assert_eq!(
            shape("[netplay]\n"),
            Ok(Shape {
                mode: Mode::Rollback,
                delay: 1
            })
        );
        assert_eq!(
            shape("[netplay]\nmode = delay\ndelay = 0\n"),
            Ok(Shape {
                mode: Mode::Delay,
                delay: 0
            })
        );
        assert_eq!(
            shape("[NetPlay]\nMODE = rollback\nDelay = 0x8\n"),
            Ok(Shape {
                mode: Mode::Rollback,
                delay: 8
            })
        );
        assert_eq!(
            shape("[netplay]\ndelay = AUTO\n"),
            Ok(Shape {
                mode: Mode::Rollback,
                delay: 1
            })
        );
        // 古いキーは読まない（D は `delay` だけから。書いていなければ auto の数）。
        assert_eq!(
            shape("[netplay]\nrollback_delay = 5\nlatency = 6\n")
                .unwrap()
                .delay,
            1
        );
        assert_eq!(
            shape("[netplay]\nrollback_delay = 5\ndelay = 3\n")
                .unwrap()
                .delay,
            3
        );
        for (t, key) in [
            ("mode = Rollback", "mode"),
            ("mode = 1", "mode"),
            ("delay = 9", "delay"),
            ("delay = 10", "delay"),
            ("delay = x", "delay"),
            ("delay = 300", "delay"),
            ("delay = autoo", "delay"),
            ("delay =", "delay"),
        ] {
            let e = shape(&format!("[netplay]\n{t}\n")).unwrap_err();
            assert!(e.contains(key), "{t}: {e}");
        }
        assert!(ini_shape_of_text("[netplay\n", "netplay").is_none());
    }

    #[test]
    fn the_ini_delay_is_auto_or_zero_to_eight_and_unwritten_is_auto() {
        use DelayChoice::{Auto, Fixed};
        assert_eq!(ini_delay(None), Ok(Auto), "書かなければ auto");
        for (v, want) in [
            ("auto", Auto),
            (" AUTO ", Auto),
            ("0", Fixed(0)),
            ("3", Fixed(3)),
            ("8", Fixed(8)),
            ("0x5", Fixed(5)),
        ] {
            assert_eq!(ini_delay(Some(v)), Ok(want), "{v:?}");
        }
        for v in ["9", "10", "-1", "", "1.5", "\u{FF11}", "autoo", "300", "x"] {
            let e = ini_delay(Some(v)).unwrap_err();
            assert!(e.contains("delay"), "{v:?}: {e}");
        }
    }
}
