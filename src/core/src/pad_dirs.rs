//! パッドの十字キー（POV ハット）を、花映塚の方向のビットにする純粋な部分（unsafe 無し）。
//!
//! 花映塚は DirectInput の `DIJOYSTATE2` の lX・lY と winmm の `JOYINFOEX` の X・Y だけを読み、十字キーが POV で
//! 報告されるパッドでは方向が立たない。そこでパッド 1 台を読む関数 `0x0042B410` の戻り値に、POV から作った方向の
//! ビットを OR する（`pad_extra.rs` / `pad_extra_win.rs`）。ここは「POV → ビット」「どの経路で読むか」「ini の設定」
//! 「ログの間引き」だけで、Win32 にもゲームのメモリにも触らない。

#![forbid(unsafe_code)]

use crate::ini;

// ---- 方向のビット ----
// `0x0042B410` の末尾の組み立て: 上 `0x0042B5BF and $0x10`・下 `0x0042B5E6 and $0x20`・
// 左 `0x0042B5B0 and $0x40`・右 `0x0042B5D1 and $0x80`。
pub const DIR_UP: u32 = 0x10;
pub const DIR_DOWN: u32 = 0x20;
pub const DIR_LEFT: u32 = 0x40;
pub const DIR_RIGHT: u32 = 0x80;
pub const DIR_MASK: u32 = DIR_UP | DIR_DOWN | DIR_LEFT | DIR_RIGHT;

/// 角度を 8 つに分けた区間の方向（0 = 上、1 = 上右、2 = 右、3 = 右下、4 = 下、5 = 下左、6 = 左、7 = 左上。時計回り）。
const SECTOR_DIRS: [u32; 8] = [
    DIR_UP,
    DIR_UP | DIR_RIGHT,
    DIR_RIGHT,
    DIR_RIGHT | DIR_DOWN,
    DIR_DOWN,
    DIR_DOWN | DIR_LEFT,
    DIR_LEFT,
    DIR_LEFT | DIR_UP,
];

/// POV の値（100 分の 1 度。北 = 0、時計回り）→ 方向のビット。
///
/// - 36000 以上は 0。中心もここに入る: DirectInput の中心は `0xFFFFFFFF`、winmm の `JOY_POVCENTERED` は `0xFFFF` で、
///   どちらも 65535 以上なので範囲外の判定が中心の判定を兼ねる。範囲外の値も 0。
/// - それ以外は 45° ずつの 8 区間（中心が 0°・45°・…）に丸める。式は `((値 + 2250) % 36000) / 4500`。
///   境目の 2250 は上右の側（切り上げ）。
pub fn pov_dirs(pov: u32) -> u32 {
    if pov >= 36_000 {
        return 0;
    }
    SECTOR_DIRS[(((pov + 2250) % 36_000) / 4500) as usize]
}

/// 元の戻り値（ボタンと既に立っている方向のビット）に十字キーの方向を足す。上位 16 ビットも含め元のビットは落とさない。
pub fn merge_dirs(ret: u32, dirs: u32) -> u32 {
    ret | (dirs & DIR_MASK)
}

/// ゲームが読む経路。`[0x004B3114]` と `[0x004B3118]`（IDirectInputDevice8 へのポインタ。`th09_addr::PAD_DEVICE_SLOTS`）が**両方**
/// 0 でなければ DirectInput、そうでなければ winmm（`0x0042B419`〜`0x0042B443` の `test` / `je 0x42b5f3`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    DirectInput,
    Winmm,
}

pub fn route(slots: [u32; 2]) -> Route {
    if slots[0] != 0 && slots[1] != 0 {
        Route::DirectInput
    } else {
        Route::Winmm
    }
}

/// 呼び出しのデバイス番号（edx。呼び手は `(機器 != 0)` の 0 / 1 を渡す）→ 表の添字。範囲外は `None`（読まない）。
pub fn device_index(dev: u32) -> Option<usize> {
    (dev < 2).then_some(dev as usize)
}

/// winmm の `joyGetPosEx` に渡すデバイス番号。ゲームは `edx != 0` の 0 / 1 を渡す（`0x0042B601 setne %al` →
/// `0x0042B616 push %eax`）。
pub fn winmm_joy_id(dev: u32) -> u32 {
    u32::from(dev != 0)
}

// ---- DirectInput の状態（`DIJOYSTATE2`）----
/// `DIJOYSTATE2` の大きさ。ゲームの `GetDeviceState` の呼び出しが `push $0x110`（`0x0042B4B9`）。
pub const DIJOYSTATE2_LEN: usize = 0x110;
/// `rgdwPOV[0]` の位置。lX・lY・lZ・lRx・lRy・lRz（24 B）→ rglSlider[2]（8 B）の次。
/// ゲームが `rgbButtons` を `+0x30` から読むことと合う。
pub const DIJOYSTATE2_POV0: usize = 0x20;

pub fn dinput_pov0(state: &[u8]) -> Option<u32> {
    let b = state.get(DIJOYSTATE2_POV0..DIJOYSTATE2_POV0 + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

// ---- winmm のハットの有無 ----
/// `JOYCAPS::wCaps` の「POV あり」（`JOYCAPS_HASPOV`、mmsystem.h の定義）。ハットの無いパッドは `dwPOV` に 0（= 北）を
/// 返し続けて方向が「上」に張り付くので、「ハットあり」と答えたデバイスだけ `dwPOV` を読む。
pub const JOYCAPS_HASPOV: u32 = 0x0010;

// ---- ini [pad] ----
pub const SECTION: &str = "pad";
const KEYS: [&str; 1] = ["pov"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PovMode {
    /// 設置しない（ゲームの読みのまま）。
    Off,
    /// 既定。winmm の経路では Windows が「ハットあり」と答えたデバイスだけ読む。
    Auto,
    /// winmm の経路でも「ハットあり」の確かめをしない（診断用）。
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadSettings {
    pub pov: PovMode,
}

impl Default for PadSettings {
    /// 節も ini も無ければ入り（後から ini で切れる）。
    fn default() -> Self {
        PadSettings { pov: PovMode::Auto }
    }
}

/// ini の本文から読む。`[pad]` が無ければ既定（入り）。知らないキー・値は断る（綴りの誤りで切ったつもりが入りの
/// まま走らないように）。値は `0` / `off`（切る）、`1` / `on`（入り）、`always`（確かめを外す）。
pub fn settings_from_text(text: &str) -> Result<PadSettings, String> {
    let ini = ini::parse(text).map_err(|e| e.to_string())?;
    if !ini.has_section(SECTION) {
        return Ok(PadSettings::default());
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} を知らない（読めるのは {}）",
                KEYS.join(" / ")
            ));
        }
    }
    let pov = match ini.get(SECTION, "pov") {
        None => PovMode::Auto,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "0" | "off" => PovMode::Off,
            "1" | "on" => PovMode::Auto,
            "always" => PovMode::Always,
            _ => {
                return Err(format!(
                    "pov の値 {v:?} は 0 / off / 1 / on / always のどれか"
                ));
            }
        },
    };
    Ok(PadSettings { pov })
}

/// winmm の経路でデバイス `dev` の `dwPOV` を読んでよいか。`wcaps` は `joyGetDevCaps` の `wCaps`（引けなければ `None` =
/// つながっていない）。
pub fn winmm_may_read(mode: PovMode, wcaps: Option<u32>) -> bool {
    match mode {
        PovMode::Off => false,
        PovMode::Always => true,
        PovMode::Auto => wcaps.is_some_and(|c| c & JOYCAPS_HASPOV != 0),
    }
}

// ---- ログの間引き ----
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// DirectInput の経路で読んだ回数。
    pub di: u32,
    /// winmm の経路で読んだ回数。
    pub mm: u32,
    /// 十字キーが元の戻り値に無い方向を足した回数。
    pub added: u32,
    /// 読もうとして読めなかった回数。
    pub failed: u32,
}

/// 数えが変わっても出さない間隔（ミリ秒）。1 マッチの間に行が増え続けないように。
pub const LOG_EVERY_MS: u64 = 30_000;

/// いま 1 行出すか。経路が初めて使われた・初めて方向を足した・初めて読めなかったときはすぐ出し、その後は
/// 足した回数か読めなかった回数が変わったときだけ `LOG_EVERY_MS` ごと。呼び出しの回数だけが変わっても出さない。
pub fn should_log(prev: &Counts, now: &Counts, since_last_ms: u64) -> bool {
    let first = (prev.di == 0 && now.di > 0)
        || (prev.mm == 0 && now.mm > 0)
        || (prev.added == 0 && now.added > 0)
        || (prev.failed == 0 && now.failed > 0);
    if first {
        return true;
    }
    (now.added != prev.added || now.failed != prev.failed) && since_last_ms >= LOG_EVERY_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTERS: [(u32, u32); 8] = [
        (0, DIR_UP),
        (4500, DIR_UP | DIR_RIGHT),
        (9000, DIR_RIGHT),
        (13_500, DIR_RIGHT | DIR_DOWN),
        (18_000, DIR_DOWN),
        (22_500, DIR_DOWN | DIR_LEFT),
        (27_000, DIR_LEFT),
        (31_500, DIR_LEFT | DIR_UP),
    ];

    #[test]
    fn the_eight_hat_directions_map_to_the_games_bits() {
        assert_eq!(
            (DIR_UP, DIR_DOWN, DIR_LEFT, DIR_RIGHT),
            (0x10, 0x20, 0x40, 0x80)
        );
        for (angle, dirs) in CENTERS {
            assert_eq!(pov_dirs(angle), dirs, "角度 {angle}");
        }
    }

    #[test]
    fn sector_boundaries_round_toward_the_next_direction() {
        let table = [
            (2249, DIR_UP),
            (2250, DIR_UP | DIR_RIGHT),
            (6749, DIR_UP | DIR_RIGHT),
            (6750, DIR_RIGHT),
            (11_249, DIR_RIGHT),
            (11_250, DIR_RIGHT | DIR_DOWN),
            (15_750, DIR_DOWN),
            (20_250, DIR_DOWN | DIR_LEFT),
            (24_750, DIR_LEFT),
            (29_250, DIR_LEFT | DIR_UP),
            (33_749, DIR_LEFT | DIR_UP),
            (33_750, DIR_UP),
            (35_999, DIR_UP),
        ];
        for (angle, dirs) in table {
            assert_eq!(pov_dirs(angle), dirs, "角度 {angle}");
        }
    }

    #[test]
    fn center_and_out_of_range_give_no_direction() {
        for v in [
            0xFFFF_FFFF,
            0x0000_FFFF,
            0xABCD_FFFF,
            36_000,
            36_001,
            0x0001_0000,
            0xFFFF_0000,
            40_000,
            u32::MAX - 1,
        ] {
            assert_eq!(pov_dirs(v), 0, "{v:#x}");
        }
    }

    #[test]
    fn direction_bits_are_inside_the_send_mask_and_apart_from_the_buttons() {
        use crate::input_model::SEND_MASK;
        assert_eq!(u32::from(SEND_MASK) & DIR_MASK, DIR_MASK);
        const BUTTONS: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0008; // Z・X・Shift・Esc
        assert_eq!(DIR_MASK & BUTTONS, 0);
    }

    #[test]
    fn every_valid_angle_gives_one_or_two_non_opposite_directions() {
        for a in 0..36_000u32 {
            let d = pov_dirs(a);
            let n = d.count_ones();
            assert!(n == 1 || n == 2, "角度 {a} -> {d:#x}");
            assert_eq!(d & !DIR_MASK, 0);
            assert!(d & (DIR_UP | DIR_DOWN) != (DIR_UP | DIR_DOWN), "{a}");
            assert!(d & (DIR_LEFT | DIR_RIGHT) != (DIR_LEFT | DIR_RIGHT), "{a}");
        }
    }

    #[test]
    fn merge_keeps_every_original_bit() {
        let ret = 0xABCD_0F0F;
        assert_eq!(merge_dirs(ret, DIR_UP), ret | DIR_UP);
        assert_eq!(merge_dirs(ret, 0), ret);
        assert_eq!(
            merge_dirs(ret, 0xFFFF_FFFF),
            ret | DIR_MASK,
            "方向以外は混ぜない"
        );
        assert_eq!(merge_dirs(DIR_RIGHT, DIR_LEFT), DIR_RIGHT | DIR_LEFT);
        assert_eq!(merge_dirs(0, 0), 0);
    }

    #[test]
    fn route_is_directinput_only_when_both_slots_are_set() {
        assert_eq!(route([1, 1]), Route::DirectInput);
        assert_eq!(route([0x10, 0xFFFF_FFF0]), Route::DirectInput);
        assert_eq!(route([1, 0]), Route::Winmm);
        assert_eq!(route([0, 1]), Route::Winmm);
        assert_eq!(route([0, 0]), Route::Winmm);
    }

    #[test]
    fn device_numbers_follow_the_games_calls() {
        assert_eq!(device_index(0), Some(0));
        assert_eq!(device_index(1), Some(1));
        assert_eq!(device_index(2), None);
        assert_eq!(device_index(u32::MAX), None);
        assert_eq!(
            (winmm_joy_id(0), winmm_joy_id(1), winmm_joy_id(7)),
            (0, 1, 1)
        );
    }

    #[test]
    fn dinput_pov0_reads_offset_0x20_little_endian() {
        assert_eq!(DIJOYSTATE2_POV0 + 4 * 4, 0x30, "POV 4 本の次が rgbButtons");
        let mut s = [0u8; DIJOYSTATE2_LEN];
        s[0x1C] = 0x77; // 手前の欄（rglSlider[1]）の値は読まない
        s[0x20..0x24].copy_from_slice(&9000u32.to_le_bytes());
        s[0x24..0x28].copy_from_slice(&18_000u32.to_le_bytes());
        assert_eq!(dinput_pov0(&s), Some(9000));
        assert_eq!(dinput_pov0(&s[..0x24]), Some(9000));
        assert_eq!(dinput_pov0(&s[..0x23]), None);
        assert_eq!(dinput_pov0(&[]), None);
    }

    #[test]
    fn ini_pad_section_is_parsed_strictly() {
        let on = PadSettings::default();
        assert_eq!(on.pov, PovMode::Auto);
        assert_eq!(settings_from_text(""), Ok(on));
        assert_eq!(settings_from_text("[netplay]\nenable = 1\n"), Ok(on));
        assert_eq!(settings_from_text("[pad]\n"), Ok(on));
        for (v, m) in [
            ("0", PovMode::Off),
            ("off", PovMode::Off),
            ("OFF", PovMode::Off),
            ("1", PovMode::Auto),
            ("on", PovMode::Auto),
            ("always", PovMode::Always),
            ("Always", PovMode::Always),
        ] {
            assert_eq!(
                settings_from_text(&format!("[pad]\npov = {v}\n")),
                Ok(PadSettings { pov: m }),
                "{v}"
            );
        }
        assert_eq!(
            settings_from_text("\u{FEFF}[PAD]\r\nPOV = 0\r\n"),
            Ok(PadSettings { pov: PovMode::Off })
        );
        for bad in [
            "[pad]\npov = no\n",
            "[pad]\npov = 2\n",
            "[pad]\npov =\n",
            "[pad]\npov_enable = 0\n",
            "[pad]\npov = 0\npov = 1\n",
            "[pad]\npov\n",
        ] {
            assert!(settings_from_text(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            settings_from_text("[pad]\n; pov = 0\n"),
            Ok(PadSettings::default())
        );
    }

    #[test]
    fn winmm_reads_only_devices_that_report_a_hat() {
        let with = Some(JOYCAPS_HASPOV | 0x0001);
        let without = Some(0x0001 | 0x0020 | 0x0040);
        assert!(winmm_may_read(PovMode::Auto, with));
        assert!(!winmm_may_read(PovMode::Auto, without));
        assert!(!winmm_may_read(PovMode::Auto, None));
        assert!(winmm_may_read(PovMode::Always, without));
        assert!(winmm_may_read(PovMode::Always, None));
        assert!(!winmm_may_read(PovMode::Off, with));
        assert_eq!(JOYCAPS_HASPOV, 0x10);
    }

    #[test]
    fn log_gate_prints_firsts_at_once_and_then_every_interval() {
        let z = Counts::default();
        let c = |di, mm, added, failed| Counts {
            di,
            mm,
            added,
            failed,
        };
        assert!(!should_log(&z, &z, 0));
        assert!(should_log(&z, &c(0, 1, 0, 0), 0), "winmm の経路が初めて");
        assert!(
            should_log(&z, &c(1, 0, 0, 0), 0),
            "DirectInput の経路が初めて"
        );
        let seen = c(0, 100, 0, 0);
        assert!(!should_log(&seen, &c(0, 99_999, 0, 0), 60_000), "回数だけ");
        assert!(should_log(&seen, &c(0, 120, 1, 0), 0), "初めて足した");
        assert!(should_log(&seen, &c(0, 120, 0, 1), 0), "初めて読めなかった");
        let after = c(0, 200, 5, 0);
        assert!(!should_log(&after, &c(0, 300, 9, 0), LOG_EVERY_MS - 1));
        assert!(should_log(&after, &c(0, 300, 9, 0), LOG_EVERY_MS));
        assert!(
            !should_log(&after, &c(0, 300, 5, 0), LOG_EVERY_MS * 10),
            "足した回数が同じ"
        );
    }
}
