//! `logs\` のログを何日残すかと、名前の日付の古さの計算。DLL（`dll_…`）とローダー（`loader_…`）が同じ日数・同じ式で消す。

/// `logs\` に残す日数。名前の日付が今日からこの日数より前のログは起動のたびに消す（3 日前の回は残し、4 日前から消す）。
pub const KEEP_LOG_DAYS: i64 = 3;

/// 西暦 1970-01-01 を 0 とする通算日。グレゴリオ暦（閏年の規則込み）。
pub fn day_number(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ファイル名の `YYYYMMDD`（ASCII の数字ちょうど 8 文字）から（年, 月, 日）。月 1〜12・日 1〜31 の外は None。
pub fn parse_date8(digits: &str) -> Option<(i64, i64, i64)> {
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| digits[r].parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// `date` が `today` から `keep_days` 日より前なら true。今日より先（時計を戻した）は false。どちらも（年, 月, 日）。
pub fn is_expired(today: (i64, i64, i64), date: (i64, i64, i64), keep_days: i64) -> bool {
    let now = day_number(today.0, today.1, today.2);
    now - day_number(date.0, date.1, date.2) > keep_days
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_numbers_count_across_months_years_and_leap_days() {
        assert_eq!(day_number(1970, 1, 1), 0);
        assert_eq!(day_number(2026, 10, 14) - day_number(2026, 10, 13), 1);
        // 2028 は閏年（2/29 がある）、2100 は閏年ではない。
        assert_eq!(day_number(2028, 3, 1) - day_number(2028, 2, 28), 2);
        assert_eq!(day_number(2100, 3, 1) - day_number(2100, 2, 28), 1);
        assert_eq!(day_number(2027, 1, 1) - day_number(2026, 12, 31), 1);
    }

    #[test]
    fn a_date_is_read_only_from_eight_digits_with_a_real_month_and_day() {
        assert_eq!(parse_date8("20261014"), Some((2026, 10, 14)));
        for bad in [
            "2026101",
            "202610145",
            "2026101a",
            "20261310",
            "20261000",
            "20261032",
            "２０２６１０１４",
            "",
        ] {
            assert_eq!(parse_date8(bad), None, "{bad}");
        }
    }

    /// 壊れたら落ちる: 日数の境目（3 日前は残す・4 日前から消す）、未来の日付を消す、月をまたぐ数え違い。
    #[test]
    fn the_fourth_day_back_is_the_first_to_expire() {
        assert_eq!(KEEP_LOG_DAYS, 3);
        let today = (2026, 10, 14);
        assert!(!is_expired(today, (2026, 10, 14), KEEP_LOG_DAYS));
        assert!(!is_expired(today, (2026, 10, 11), KEEP_LOG_DAYS));
        assert!(is_expired(today, (2026, 10, 10), KEEP_LOG_DAYS));
        assert!(is_expired(today, (2026, 9, 30), KEEP_LOG_DAYS));
        assert!(!is_expired(today, (2026, 10, 15), KEEP_LOG_DAYS), "未来");
        assert!(!is_expired((2027, 1, 1), (2026, 12, 29), KEEP_LOG_DAYS));
        assert!(is_expired((2027, 1, 1), (2026, 12, 28), KEEP_LOG_DAYS));
    }
}
