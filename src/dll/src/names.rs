//! 名前を作る処理（pure。unsafe 無し）: ログのファイル名・二重注入のミューテックスの名前・
//! 「ログの置き場がゲームのフォルダの中でないか」の判定。
#![forbid(unsafe_code)]

use std::ffi::OsStr;

use higanr_core::log_names::LOG_PREFIX;

/// 現地時刻（`GetLocalTime` の `SYSTEMTIME` からコピーする。Win32 の型を持ち込まないため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

impl LocalTime {
    pub fn display(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }
}

/// 同じ秒・同じ PID で名前がぶつかったときに試す回数の上限。
pub const LOG_NAME_ATTEMPTS: u32 = 100;

/// 1 回の注入で 1 つの回ごとの名前。隣のファイルの名前の元で、台本の実行では本ログの名前でもある（既存を上書きしない）:
/// 時刻（ミリ秒まで）と PID を入れ、それでもぶつかれば `attempt`（0 始まり）を `_2` 以降の接尾辞にする。
/// Windows のファイル名に使えない文字は入れない。
pub fn log_file_name(t: &LocalTime, pid: u32, attempt: u32) -> String {
    let base = format!(
        "{LOG_PREFIX}{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}_pid{}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.millis, pid
    );
    if attempt == 0 {
        format!("{base}.log")
    } else {
        format!("{base}_{}.log", attempt + 1)
    }
}

/// 台本でない実行の本ログの名前: その日の 1 ファイル（`dll_YYYYMMDD.log`）。起動のたびに追記する。開発用のビルドを手で起動したときも日ごと。
pub fn daily_log_file_name(t: &LocalTime) -> String {
    format!("{LOG_PREFIX}{:04}{:02}{:02}.log", t.year, t.month, t.day)
}

/// 台本・検査の実行か（本ログを回ごとの名前にする）: ローダーの `--ini` / `--script` が環境の変数を置いた、または DLL のフォルダに
/// `script.txt` がある。この実行は同じ PC で複数を重ねて起こし、回ごとに本ログを読み分けるので、日ごとにまとめない。
pub fn is_scripted_run(
    ini_env: Option<&OsStr>,
    script_env: Option<&OsStr>,
    script_file_exists: bool,
) -> bool {
    ini_env.is_some() || script_env.is_some() || script_file_exists
}

/// 本ログの隣のファイル名: 回ごとの名前（`log_file_name`）から `.log` を外して `suffix` を付ける（`_ticks.csv` など）。
/// 日ごとの本ログの名前ではなく回ごとの名前から作るので、起動ごとに別名になる。`.log` で終わらない名前にはそのまま付ける。
pub fn side_file_name(log_name: &str, suffix: &str) -> String {
    let stem = log_name.strip_suffix(".log").unwrap_or(log_name);
    format!("{stem}{suffix}")
}

/// 配るバージョンで `logs\` に残す日数。名前の日付が今日からこの日数より前のログ（`dll_` で始まる本ログと隣のファイル）は起動の
/// たびに消す（3 日前の回は残し、4 日前から消す）。日数の原本はここだけ（`win::prune_old_logs` と配る README が引く）。
/// 開発用のビルドでは消さない。
pub const KEEP_LOG_DAYS: i64 = 3;

fn day_number(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 名前の日付。`dll_YYYYMMDD-…`（回ごと・隣のファイル）か `dll_YYYYMMDD.log`（日ごと）。
fn log_name_day(name: &str) -> Option<i64> {
    let rest = name.strip_prefix(LOG_PREFIX)?;
    let digits = rest.get(..8)?;
    let tail = rest.get(8..)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) || !(tail.starts_with('-') || tail == ".log") {
        return None;
    }
    let num = |r: std::ops::Range<usize>| digits[r].parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(day_number(y, m, d))
}

/// 消してよい古いログか: 名前が `dll_YYYYMMDD-` で始まるか `dll_YYYYMMDD.log` で、その日が今日から `keep_days` 日より前。名前の形が違うもの・
/// 今日より先の日付のもの（時計を戻した）は消さない。日付はファイル名から読む（zip から出すと更新時刻が変わるため）。
pub fn is_expired_log(name: &str, today: &LocalTime, keep_days: i64) -> bool {
    let Some(day) = log_name_day(name) else {
        return false;
    };
    let now = day_number(
        i64::from(today.year),
        i64::from(today.month),
        i64::from(today.day),
    );
    now - day > keep_days
}

/// 二重注入を防ぐミューテックスの名前（`Local\HiganRTool.<pid>`）。ローダーが注入の前に同じ名前を開いてみるので、
/// ローダーと同じ形でなければならない。
pub fn mutex_name(pid: u32) -> String {
    format!("Local\\HiganRTool.{pid}")
}

/// 比べるための形: 区切りを `\` にそろえ、末尾の `\` を落とし、小文字にする（Windows のパスは大文字小文字を区別しない）。
fn normalize(p: &str) -> String {
    let mut s: String = p.replace('/', "\\").to_lowercase();
    while s.ends_with('\\') {
        s.pop();
    }
    s
}

/// `dir` が `root` と同じか、その下にあるか。部品の境目で比べる（`C:\a\game2` は `C:\a\game` の下ではない）。
/// ログの置き場がゲームのフォルダの中なら書かない、の判定に使う。`..` や 8.3 形式は解かないので、
/// 両方とも `GetModuleFileNameW` が返した形のまま渡すこと。
pub fn is_same_or_inside(dir: &str, root: &str) -> bool {
    let d = normalize(dir);
    let r = normalize(root);
    if r.is_empty() {
        return false;
    }
    d == r || (d.starts_with(&r) && d[r.len()..].starts_with('\\'))
}

#[cfg(test)]
mod tests {
    //! パスは合成の名前だけ。
    use super::*;

    fn t(second: u16, millis: u16) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 9,
            day: 5,
            hour: 7,
            minute: 3,
            second,
            millis,
        }
    }

    #[test]
    fn log_file_name_has_zero_padded_time_and_pid() {
        assert_eq!(
            log_file_name(&t(9, 7), 4242, 0),
            "dll_20260905-070309-007_pid4242.log"
        );
    }

    #[test]
    fn log_file_name_differs_for_every_attempt() {
        let mut seen = std::collections::HashSet::new();
        for a in 0..LOG_NAME_ATTEMPTS {
            assert!(seen.insert(log_file_name(&t(1, 2), 1, a)), "attempt {a}");
        }
        assert_eq!(seen.len(), LOG_NAME_ATTEMPTS as usize);
    }

    #[test]
    fn log_file_name_differs_by_time_and_pid() {
        let a = log_file_name(&t(1, 2), 100, 0);
        assert_ne!(a, log_file_name(&t(1, 3), 100, 0));
        assert_ne!(a, log_file_name(&t(2, 2), 100, 0));
        assert_ne!(a, log_file_name(&t(1, 2), 101, 0));
    }

    #[test]
    fn log_file_name_has_no_reserved_characters() {
        let n = log_file_name(&t(59, 999), u32::MAX, LOG_NAME_ATTEMPTS - 1);
        for c in ['\\', '/', ':', '*', '?', '"', '<', '>', '|', ' '] {
            assert!(!n.contains(c), "{n} に {c:?}");
        }
        assert!(n.ends_with(".log"));
    }

    #[test]
    fn side_file_names_follow_the_log_name() {
        let a = log_file_name(&t(9, 7), 4242, 0);
        assert_eq!(
            side_file_name(&a, "_ticks.csv"),
            "dll_20260905-070309-007_pid4242_ticks.csv"
        );
        let b = log_file_name(&t(9, 7), 4242, 1);
        assert_eq!(
            side_file_name(&b, "_snap.bin"),
            "dll_20260905-070309-007_pid4242_2_snap.bin"
        );
        let c = log_file_name(&t(9, 8), 4242, 0);
        let mut seen = std::collections::HashSet::new();
        for log in [&a, &b, &c] {
            for s in ["_ticks.csv", "_changes.csv", "_menu.csv", "_snap.bin"] {
                assert!(seen.insert(side_file_name(log, s)), "{log} {s}");
            }
        }
        assert_eq!(side_file_name("x", "_ticks.csv"), "x_ticks.csv");
    }

    /// 壊れたら落ちる: 日ごとの名前に時刻・PID が入る（起動ごとに別ファイルへ戻る）。
    #[test]
    fn daily_log_name_is_the_date_only() {
        assert_eq!(daily_log_file_name(&t(9, 7)), "dll_20260905.log");
        assert_eq!(
            daily_log_file_name(&t(9, 7)),
            daily_log_file_name(&t(58, 999))
        );
        let next_day = LocalTime { day: 6, ..t(9, 7) };
        assert_ne!(
            daily_log_file_name(&t(9, 7)),
            daily_log_file_name(&next_day)
        );
        assert!(higanr_core::log_names::is_daily_log_name(
            &daily_log_file_name(&t(1, 1))
        ));
    }

    #[test]
    fn a_scripted_run_is_told_by_the_loader_variables_or_the_script_file() {
        let v = Some(OsStr::new("x"));
        assert!(!is_scripted_run(None, None, false));
        assert!(is_scripted_run(v, None, false));
        assert!(is_scripted_run(None, v, false));
        assert!(is_scripted_run(None, None, true));
    }

    fn day(year: u16, month: u16, day: u16) -> LocalTime {
        LocalTime {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            millis: 0,
        }
    }

    #[test]
    fn expired_logs_are_decided_by_the_date_in_the_name() {
        assert_eq!(KEEP_LOG_DAYS, 3, "本人が決めた保存の日数");
        let today = day(2026, 10, 8);
        let name = |y, m, d| log_file_name(&day(y, m, d), 4242, 0);
        assert!(!is_expired_log(&name(2026, 10, 8), &today, KEEP_LOG_DAYS));
        assert!(
            !is_expired_log(&name(2026, 10, 5), &today, KEEP_LOG_DAYS),
            "3 日前は残す"
        );
        assert!(
            is_expired_log(&name(2026, 10, 4), &today, KEEP_LOG_DAYS),
            "4 日前は消す"
        );
        for suffix in ["_net_frames.csv.gz", "_pacing.csv.gz", "_net_frames.csv"] {
            assert!(
                is_expired_log(
                    &side_file_name(&name(2026, 10, 4), suffix),
                    &today,
                    KEEP_LOG_DAYS
                ),
                "{suffix}（4 日前）は消す"
            );
            assert!(
                !is_expired_log(
                    &side_file_name(&name(2026, 10, 5), suffix),
                    &today,
                    KEEP_LOG_DAYS
                ),
                "{suffix}（3 日前）は残す"
            );
        }
        let daily = |y, m, d| daily_log_file_name(&day(y, m, d));
        assert!(!is_expired_log(&daily(2026, 10, 8), &today, KEEP_LOG_DAYS));
        assert!(
            !is_expired_log(&daily(2026, 10, 5), &today, KEEP_LOG_DAYS),
            "日ごとの本ログも 3 日前は残す"
        );
        assert!(
            is_expired_log(&daily(2026, 10, 4), &today, KEEP_LOG_DAYS),
            "日ごとの本ログも 4 日前は消す"
        );
        assert!(!is_expired_log(&daily(2027, 1, 1), &today, KEEP_LOG_DAYS));
        // 月・年と閏日をまたぐ: 2028-03-01 から 3 日前は 2028-02-27（2028 年は閏年で 2/29 がある）。
        let mar = day(2028, 3, 1);
        assert!(!is_expired_log(&name(2028, 2, 27), &mar, KEEP_LOG_DAYS));
        assert!(is_expired_log(&name(2028, 2, 26), &mar, KEEP_LOG_DAYS));
        let jan = day(2027, 1, 2);
        assert!(!is_expired_log(&name(2026, 12, 30), &jan, KEEP_LOG_DAYS));
        assert!(is_expired_log(&name(2026, 12, 29), &jan, KEEP_LOG_DAYS));
        let mar27 = day(2027, 3, 1);
        assert!(!is_expired_log(&name(2027, 2, 26), &mar27, KEEP_LOG_DAYS));
        assert!(is_expired_log(&name(2027, 2, 25), &mar27, KEEP_LOG_DAYS));
        for n in [
            "HiganRTool.ini",
            "loader_20200101-000000-000_pid1.log",
            "dll_2020010-000000_pid1.log",
            "dll_20201301-000000-000_pid1.log",
            "dll_20200100-000000-000_pid1.log",
            "dll_20200101_000000-000_pid1.log",
            "dll_２０２００１０１-000000-000_pid1.log",
            "dll_20200101.txt",
            "dll_20200101.log.bak",
            "dll_20200101",
            "dll_20201301.log",
        ] {
            assert!(!is_expired_log(n, &today, KEEP_LOG_DAYS), "{n}");
        }
        assert!(
            !is_expired_log(&name(2027, 1, 1), &today, KEEP_LOG_DAYS),
            "先の日付"
        );
    }

    #[test]
    fn local_time_display_form() {
        assert_eq!(t(9, 7).display(), "2026-09-05 07:03:09.007");
    }

    #[test]
    fn mutex_name_matches_the_design() {
        assert_eq!(mutex_name(1234), "Local\\HiganRTool.1234");
        assert_ne!(mutex_name(1234), mutex_name(1235));
    }

    #[test]
    fn inside_is_detected_regardless_of_case_and_separators() {
        let root = r"C:\Games\SampleGame";
        assert!(is_same_or_inside(r"C:\Games\SampleGame", root));
        assert!(is_same_or_inside(r"C:\Games\SampleGame\logs", root));
        assert!(is_same_or_inside(r"c:\games\samplegame\LOGS\", root));
        assert!(is_same_or_inside("C:/Games/SampleGame/logs", root));
        assert!(is_same_or_inside(
            r"C:\Games\サンプル\logs",
            r"C:\Games\サンプル\"
        ));
    }

    #[test]
    fn prefix_only_is_not_inside() {
        let root = r"C:\Games\SampleGame";
        assert!(!is_same_or_inside(r"C:\Games\SampleGame2\logs", root));
        assert!(!is_same_or_inside(
            r"C:\Tools\HiganRTool\build\bin\logs",
            root
        ));
        assert!(!is_same_or_inside(r"C:\Games", root));
        assert!(!is_same_or_inside(r"C:\x", ""));
    }
}
