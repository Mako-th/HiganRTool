//! ゲームのメニューが保存関数へ渡すパスの形と、印を足してよいファイルかの判断（pure）。
//!
//! 保存関数 `0x00420C60` を呼ぶのは 2 か所で、パスは exe のフォルダを作業フォルダにして開かれる相対パス:
//! - 保存の画面（`0x004297F1`）: `sprintf("replay/th9_%.2d.rpy", 枠 + 1)`（書式は `0x0048FA84`）。枠は 25 個。
//! - 内蔵のネット対戦の自動保存（`0x00429BF6`）: `sprintf("./replay/th9_udn%.3d.rpy", n)`（書式は `0x0048FA68`）。
//!   n は 1 から、空いている最初の番号（`0x00429B8D`〜`0x00429BC0`）。1000 個とも埋まっていると 1000。
//!
//! 印を足すファイルは、この 2 つの形のパスだけに限る（`..`・絶対パス・別のフォルダは断る）。
#![forbid(unsafe_code)]

/// 保存の画面の枠の数（一覧の 25 枠。`0x00426DAB` の `cmp $0x19`）。保管庫の `pack50` の半分（`vault::PACK50_HALF`）も同じ枠の数。
pub const SLOTS: u8 = 25;
/// 内蔵のネット対戦の自動保存の番号の上限（`0x00429BBA cmp $0x3e8,%edi`）。
pub const UDN_MAX: u16 = 1000;
/// ファイルの更新時刻が保存の始まりの時刻よりこれだけ前でも新しいと見なす。FAT の更新時刻は 2 秒刻み（切り捨て）なので、
/// 同じ 2 秒では足りず、粗い時計の分を足して 3 秒にした。
pub const MTIME_SLACK_MS: u64 = 3000;

/// 保存関数に渡されたパスの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuSavePath {
    /// 保存の画面の枠（1〜25）。
    Slot(u8),
    /// 内蔵のネット対戦の自動保存の番号（1〜1000）。
    Udn(u16),
}

/// 保存の画面の枠 `n`（1〜25）のファイル名（書式 `th9_%.2d.rpy`、`0x0048FA84` の `replay/` の後ろ）。
pub fn slot_file_name(n: u16) -> String {
    format!("th9_{n:02}.rpy")
}

impl MenuSavePath {
    /// exe のフォルダからの相対パス（`/` 区切り。先頭の `./` は付けない）。
    pub fn relative(&self) -> String {
        match self {
            MenuSavePath::Slot(n) => format!("replay/{}", slot_file_name(u16::from(*n))),
            MenuSavePath::Udn(n) => format!("replay/th9_udn{n:03}.rpy"),
        }
    }
}

/// 数字だけの欄（`min_width` 桁以上 4 桁まで）を数にする。`+`・空白・数字以外が混ざれば `None`。桁数の厳密な形は呼び手が見る。
fn digits(s: &str, min_width: usize) -> Option<u16> {
    if s.len() < min_width || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// 保存関数に渡されたパス（NUL を含まない CP932 のバイト列）を、2 つの形のどちらかとして読む。形が違えば `None`。
/// 読めたものは `relative()` が元のパスと同じ文字になる（先頭の `./` を除いて）ので、`..` や別のフォルダは通らない。
pub fn parse_save_path(path: &[u8]) -> Option<MenuSavePath> {
    let s = core::str::from_utf8(path).ok()?;
    if let Some(num) = s
        .strip_prefix("./replay/th9_udn")
        .and_then(|r| r.strip_suffix(".rpy"))
    {
        let n = digits(num, 3)?;
        // `%.3d` は 3 桁未満を 0 で埋める。余分な 0（0001）は作らない。
        return ((1..=UDN_MAX).contains(&n) && (n >= 1000 || num.len() == 3))
            .then_some(MenuSavePath::Udn(n));
    }
    let num = s
        .strip_prefix("replay/th9_")
        .and_then(|r| r.strip_suffix(".rpy"))?;
    let n = digits(num, 2)?;
    (num.len() == 2 && (1..=u16::from(SLOTS)).contains(&n)).then_some(MenuSavePath::Slot(n as u8))
}

/// ファイルの更新時刻（UNIX 時刻のミリ秒）が、今回の保存の始まり以降か。失敗して前の保存の古いファイルが残っているときに、
/// そのファイルへ印を足さないための判断。
pub fn written_since(mtime_ms: u64, started_ms: u64) -> bool {
    mtime_ms.saturating_add(MTIME_SLACK_MS) >= started_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_games_forms_are_read() {
        assert_eq!(
            parse_save_path(b"replay/th9_01.rpy"),
            Some(MenuSavePath::Slot(1))
        );
        assert_eq!(
            parse_save_path(b"replay/th9_25.rpy"),
            Some(MenuSavePath::Slot(25))
        );
        assert_eq!(
            parse_save_path(b"./replay/th9_udn001.rpy"),
            Some(MenuSavePath::Udn(1))
        );
        assert_eq!(
            parse_save_path(b"./replay/th9_udn999.rpy"),
            Some(MenuSavePath::Udn(999))
        );
        assert_eq!(
            parse_save_path(b"./replay/th9_udn1000.rpy"),
            Some(MenuSavePath::Udn(1000))
        );
    }

    #[test]
    fn the_games_own_format_strings_make_paths_that_are_read() {
        // 書式 `replay/th9_%.2d.rpy` / `./replay/th9_udn%.3d.rpy` をこちらで展開して、読み手と食い違わないか。
        for n in 1..=SLOTS {
            let p = format!("replay/th9_{n:02}.rpy");
            let r = parse_save_path(p.as_bytes()).unwrap();
            assert_eq!(r, MenuSavePath::Slot(n));
            assert_eq!(r.relative(), p);
        }
        for n in 1..=UDN_MAX {
            let p = format!("./replay/th9_udn{n:03}.rpy");
            let r = parse_save_path(p.as_bytes()).unwrap();
            assert_eq!(r, MenuSavePath::Udn(n));
            assert_eq!(format!("./{}", r.relative()), p);
        }
    }

    #[test]
    fn other_shapes_are_refused() {
        for bad in [
            &b""[..],
            b"replay/th9_00.rpy",
            b"replay/th9_26.rpy",
            b"replay/th9_1.rpy",
            b"replay/th9_001.rpy",
            b"replay/th9_+1.rpy",
            b"replay/th9_ 1.rpy",
            b"./replay/th9_01.rpy",
            b"replay/th9_udn001.rpy",
            b"./replay/th9_udn000.rpy",
            b"./replay/th9_udn01.rpy",
            b"./replay/th9_udn0001.rpy",
            b"./replay/th9_udn1001.rpy",
            b"./replay/th9_udn10000.rpy",
            b"../replay/th9_01.rpy",
            b"replay/../th9_01.rpy",
            b"replay\\th9_01.rpy",
            b"/replay/th9_01.rpy",
            b"C:/replay/th9_01.rpy",
            b"replay/th9_01.rpy ",
            b"replay/th9_01.rpy\0",
            b"replay/th9_01.RPY",
            b"Replay/th9_01.rpy",
            b"other/th9_01.rpy",
            b"replay/th9_01.rpyx",
            b"\x83\x8a/th9_01.rpy",
        ] {
            assert_eq!(
                parse_save_path(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn an_old_file_does_not_count_as_this_save() {
        let start = 1_000_000_000_000u64;
        assert!(written_since(start, start));
        assert!(written_since(start + 5_000, start));
        // FAT の 2 秒刻みで切り捨てた時刻（最大 2 秒前）に、粗い時計の分が加わっても通る。
        assert!(written_since(start - 2_000, start));
        assert!(written_since(start - 2_999, start));
        assert!(written_since(start - MTIME_SLACK_MS, start));
        assert!(!written_since(start - MTIME_SLACK_MS - 1, start));
        assert!(!written_since(0, start));
        assert!(written_since(u64::MAX, u64::MAX));
    }
}
