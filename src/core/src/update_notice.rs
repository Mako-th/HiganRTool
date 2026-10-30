//! 遊んでいる間の「新しいバージョンの知らせ」の判断（pure）。
//! いつ目録を取りに行くか・取れた目録が今より新しいか・画面に出すバージョンの読みを持つ。
//! 通信とスレッドはdllの`update_check`、署名と目録の読みは`update`にある。
//! 確認してよいかどうかはローダーが決める（開発用の引数・iniの`update_check = 0`など）。
//! 決めた結果は環境変数`NOTICE_ENV`でゲームへ渡す。
//! dllはそれが`1`で`BUILD_NUMBER`が0でないときだけ確認する。
#![forbid(unsafe_code)]

use std::ffi::OsStr;

use crate::update::{self, ManifestError};

/// ローダーがdllへ新しいバージョンの知らせを許すかどうかを渡す環境変数（`1`で許す。無い・それ以外は許さない）。
pub const NOTICE_ENV: &str = "HIGANRTOOL_UPDATE_NOTICE";

/// `NOTICE_ENV`の値が「許す」かどうか。
pub fn allowed_by_env(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

/// 「妖怪 対 妖怪」の決定でメニューが開いたとき・接続が成立したときの、前の確認からの最短の間隔（秒）。開き直しのたびに取りに行かない。
pub const MENU_INTERVAL_SECS: u64 = 10 * 60;
/// マッチ終了時のメニューが出たときの、前の確認からの最短の間隔（秒）。
pub const END_MENU_INTERVAL_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// ネット対戦のメニューが開いた。
    MenuOpened,
    /// 接続が成立した。
    Connected,
    /// 接続している間に、マッチ終了時のメニューが出た。
    EndMenu,
}

impl Trigger {
    fn interval_secs(self) -> u64 {
        match self {
            Trigger::MenuOpened | Trigger::Connected => MENU_INTERVAL_SECS,
            Trigger::EndMenu => END_MENU_INTERVAL_SECS,
        }
    }
}

/// 取りに行くかの判断（ゲームのスレッドが持つ。確保しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poll {
    enabled: bool,
    found: bool,
    /// 前に取りに行ったUNIX秒。
    last: Option<u64>,
}

impl Poll {
    /// `build_number`が0（開発用のビルド）なら取りに行かない。
    pub const fn new(build_number: u32) -> Poll {
        Poll {
            enabled: build_number != 0,
            found: false,
            last: None,
        }
    }

    /// 新しいバージョンが見つかった。以後は取りに行かない。
    pub fn mark_found(&mut self) {
        self.found = true;
    }

    /// まだ取りに行きうるかどうか（取りに行かない状態ならきっかけの検出も要らない）。
    pub fn active(&self) -> bool {
        self.enabled && !self.found
    }

    /// 取りに行く要求を出すならtrue（出した時刻を覚える）。時計が巻き戻っても止まり続けないよう間隔は差の大きさで見る。
    pub fn due(&mut self, trigger: Trigger, now: u64) -> bool {
        if !self.active() {
            return false;
        }
        if let Some(last) = self.last
            && now.abs_diff(last) < trigger.interval_secs()
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    DevBuild,
}

/// 確認してよいかどうか。ローダーの許し（`NOTICE_ENV`）を通った後のdll側の歯止め。
pub fn gate(build_number: u32) -> Result<(), Skip> {
    if build_number == 0 {
        return Err(Skip::DevBuild);
    }
    Ok(())
}

/// 目録の`display`を読んだmajor.minor.patch（各0〜1023。全部0は読めない扱い）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version(u32);

const PART_BITS: u32 = 10;
const PART_MAX: u32 = (1 << PART_BITS) - 1;

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32) -> Option<Version> {
        if major > PART_MAX || minor > PART_MAX || patch > PART_MAX {
            return None;
        }
        let packed = (major << (2 * PART_BITS)) | (minor << PART_BITS) | patch;
        (packed != 0).then_some(Version(packed))
    }

    pub fn parts(self) -> (u32, u32, u32) {
        (
            self.0 >> (2 * PART_BITS),
            (self.0 >> PART_BITS) & PART_MAX,
            self.0 & PART_MAX,
        )
    }

    /// `v1.2.3`か`1.2.3`。余分な文字（`-2`など）・先頭の0・数が3つでない形は読めない。
    pub fn parse(display: &str) -> Option<Version> {
        let body = display.strip_prefix('v').unwrap_or(display);
        let mut it = body.split('.');
        let (a, b, c) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        Version::new(part(a)?, part(b)?, part(c)?)
    }
}

fn part(s: &str) -> Option<u32> {
    let canonical = !s.is_empty()
        && s.len() <= 4
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s == "0" || !s.starts_with('0'));
    canonical.then(|| s.parse().ok()).flatten()
}

/// 見つかった新しいバージョン。スレッドの間は`to_raw`の1ワードで渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Found {
    #[default]
    None,
    /// バージョンが読めなければ`None`（バージョン無しの文を出す）。
    Newer(Option<Version>),
}

const FOUND_BIT: u32 = 1 << 31;

impl Found {
    /// 0は見つかっていない。
    pub fn to_raw(self) -> u32 {
        match self {
            Found::None => 0,
            Found::Newer(v) => FOUND_BIT | v.map_or(0, |v| v.0),
        }
    }

    pub fn from_raw(raw: u32) -> Found {
        if raw & FOUND_BIT == 0 {
            return Found::None;
        }
        Found::Newer(match raw & !FOUND_BIT {
            0 => None,
            v if v >> (3 * PART_BITS) == 0 => Some(Version(v)),
            _ => None,
        })
    }

    pub fn is_found(self) -> bool {
        self != Found::None
    }
}

/// ネットから取った目録を検証して今のビルド`current`より新しければ`Some`。署名が合わない・形が違うのは`Err`。
/// 同じか古いバージョンは`None`（署名つきでも使わない）。
pub fn judge(
    bytes: &[u8],
    public_key: &[u8; 32],
    current: u32,
) -> Result<Option<Found>, ManifestError> {
    let m = update::verify_and_parse(bytes, public_key)?;
    Ok(update::is_newer(current, m.version).then(|| Found::Newer(Version::parse(&m.display))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::test_key::{public_key, sample_body, sign, sign_with};

    const NOW: u64 = 1_000_000;

    fn body_with_display(version: u32, display: &str) -> String {
        sample_body(version).replace("display 20261008-abcdef0", &format!("display {display}"))
    }

    #[test]
    fn a_dev_build_never_asks() {
        let mut p = Poll::new(0);
        for t in [Trigger::MenuOpened, Trigger::Connected, Trigger::EndMenu] {
            assert!(!p.due(t, NOW), "{t:?}");
        }
        assert!(!p.active());
    }

    #[test]
    fn the_menu_waits_ten_minutes_and_the_end_menu_thirty_after_the_previous_ask() {
        let mut p = Poll::new(7);
        assert!(p.due(Trigger::MenuOpened, NOW), "最初は取りに行く");
        assert!(!p.due(Trigger::MenuOpened, NOW + MENU_INTERVAL_SECS - 1));
        assert!(
            !p.due(Trigger::Connected, NOW + 1),
            "接続の成立も間隔を守る"
        );
        assert!(p.due(Trigger::MenuOpened, NOW + MENU_INTERVAL_SECS));
        let t = NOW + MENU_INTERVAL_SECS;
        assert!(
            !p.due(Trigger::EndMenu, t + END_MENU_INTERVAL_SECS - 1),
            "終わりのメニューは 30 分"
        );
        assert!(p.due(Trigger::EndMenu, t + END_MENU_INTERVAL_SECS));
        // 取りに行った時刻はきっかけの種類によらず次の間隔の起点になる。
        assert!(!p.due(Trigger::MenuOpened, t + END_MENU_INTERVAL_SECS + 1));
    }

    #[test]
    fn the_first_ask_by_the_end_menu_is_not_delayed() {
        let mut p = Poll::new(7);
        assert!(p.due(Trigger::EndMenu, NOW));
    }

    #[test]
    fn a_clock_set_back_does_not_block_asking_for_good() {
        let mut p = Poll::new(7);
        assert!(p.due(Trigger::MenuOpened, NOW));
        assert!(p.due(Trigger::MenuOpened, NOW - 86_400));
    }

    #[test]
    fn once_found_it_never_asks_again() {
        let mut p = Poll::new(7);
        p.mark_found();
        assert!(!p.active());
        for t in [Trigger::MenuOpened, Trigger::Connected, Trigger::EndMenu] {
            assert!(!p.due(t, NOW + 100_000), "{t:?}");
        }
    }

    #[test]
    fn the_gate_stops_only_dev_builds() {
        assert_eq!(gate(0), Err(Skip::DevBuild));
        assert_eq!(gate(1), Ok(()));
        assert_eq!(gate(5), Ok(()));
    }

    #[test]
    fn only_the_value_one_lets_the_notice_through() {
        assert!(allowed_by_env(Some(OsStr::new("1"))));
        for no in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("11"),
            Some(" 1"),
        ] {
            assert!(!allowed_by_env(no.map(OsStr::new)), "{no:?}");
        }
    }

    #[test]
    fn display_is_read_as_three_numbers_or_not_at_all() {
        let v = |s: &str| Version::parse(s).map(Version::parts);
        assert_eq!(v("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(v("1.2.3"), Some((1, 2, 3)));
        assert_eq!(v("v10.0.1023"), Some((10, 0, 1023)));
        for bad in [
            "",
            "v",
            "v1.2",
            "v1.2.3.4",
            "v1.2.3-2",
            "v1.2.x",
            "v01.2.3",
            "v1..3",
            "v1.2.1024",
            "v0.0.0",
            "20261008-abcdef0",
            "V1.2.3",
            "vv1.2.3",
            "v1.2.+3",
            "v1.2.12345",
        ] {
            assert_eq!(v(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn found_survives_the_one_word_hand_over() {
        for f in [
            Found::None,
            Found::Newer(None),
            Found::Newer(Version::new(1, 2, 3)),
            Found::Newer(Version::new(1023, 1023, 1023)),
            Found::Newer(Version::new(0, 0, 1)),
        ] {
            assert_eq!(Found::from_raw(f.to_raw()), f, "{f:?}");
        }
        assert_eq!(Found::None.to_raw(), 0);
        assert_ne!(Found::Newer(None).to_raw(), 0, "見つかったがバージョン無し");
        // 範囲の外のワード（書き手の誤り）はバージョン無しとして読む。
        assert_eq!(Found::from_raw(FOUND_BIT | (1 << 30)), Found::Newer(None));
    }

    #[test]
    fn a_signed_newer_manifest_is_found_with_its_version() {
        let m = sign(&body_with_display(200, "v1.2.9"));
        assert_eq!(
            judge(m.as_bytes(), &public_key(), 100),
            Ok(Some(Found::Newer(Version::new(1, 2, 9))))
        );
    }

    #[test]
    fn a_newer_manifest_with_an_unreadable_display_is_found_without_a_version() {
        for d in ["20261008-abcdef0", "v1.2.9-2", "v1.2"] {
            let m = sign(&body_with_display(200, d));
            assert_eq!(
                judge(m.as_bytes(), &public_key(), 100),
                Ok(Some(Found::Newer(None))),
                "{d}"
            );
        }
    }

    #[test]
    fn the_same_or_an_older_version_is_not_news() {
        let m = sign(&body_with_display(100, "v1.2.9"));
        assert_eq!(judge(m.as_bytes(), &public_key(), 100), Ok(None), "同じ");
        assert_eq!(judge(m.as_bytes(), &public_key(), 101), Ok(None), "古い");
        assert_eq!(
            judge(m.as_bytes(), &public_key(), 99).map(|f| f.is_some()),
            Ok(true)
        );
    }

    #[test]
    fn a_manifest_signed_by_another_key_or_tampered_is_refused() {
        let body = body_with_display(200, "v9.9.9");
        let other = sign_with(b"another-seed-another-seed-xxxxxx", &body);
        assert_eq!(
            judge(other.as_bytes(), &public_key(), 100),
            Err(ManifestError::SignatureMismatch)
        );
        let signed = sign(&body);
        let tampered = signed.replace("display v9.9.9", "display v9.9.8");
        assert_eq!(
            judge(tampered.as_bytes(), &public_key(), 100),
            Err(ManifestError::SignatureMismatch)
        );
        assert_eq!(
            judge(b"not a manifest", &public_key(), 100),
            Err(ManifestError::NoSignature)
        );
        assert_eq!(
            judge(&[b'a'; update::MAX_MANIFEST_BYTES + 1], &public_key(), 100),
            Err(ManifestError::TooLarge)
        );
    }
}
