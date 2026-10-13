//! 本ログの名前と、本ログの中の「起動の区切り」の行。DLL が書き、ローダーが読む。
//!
//! 本ログは日ごとの 1 ファイル（`dll_YYYYMMDD.log`）に起動のたびに追記するので、1 つの実行の範囲は区切りの行で見分ける。

#![forbid(unsafe_code)]

/// 本ログと隣のファイルの名前の先頭。
pub const LOG_PREFIX: &str = "dll_";

/// 起動の区切りの行の先頭。
pub const RUN_HEADER_PREFIX: &str = "==== 起動 pid ";

const RUN_HEADER_SUFFIX: &str = " ====";
const SEP: &str = " / ";

/// 区切りの行: `==== 起動 pid <PID> / <時刻> / <回ごとの名前の元> ====`。回ごとの名前の元は、隣のファイル
/// （`<元>_net_frames.csv.gz` など）の名前の先頭。
pub fn run_header(pid: u32, time: &str, stem: &str) -> String {
    format!("{RUN_HEADER_PREFIX}{pid}{SEP}{time}{SEP}{stem}{RUN_HEADER_SUFFIX}")
}

/// 日ごとの本ログの名前（`dll_YYYYMMDD.log`）か。
pub fn is_daily_log_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(LOG_PREFIX)
        .and_then(|r| r.strip_suffix(".log"))
    else {
        return false;
    };
    rest.len() == 8 && rest.bytes().all(|b| b.is_ascii_digit())
}

/// 区切りの行から（PID, 回ごとの名前の元）。形が違えば None。
fn parse_header(line: &str) -> Option<(u32, &str)> {
    let body = line
        .strip_prefix(RUN_HEADER_PREFIX)?
        .strip_suffix(RUN_HEADER_SUFFIX)?;
    let mut parts = body.splitn(3, SEP);
    let pid = parts.next()?.parse().ok()?;
    let _time = parts.next()?;
    let stem = parts.next()?;
    Some((pid, stem))
}

/// 1 行（改行を除く）が区切りの行ならその PID。大きな本ログを行ごとに読む側が、全体を持たずに区切りを探すのに使う。
pub fn header_pid(line: &str) -> Option<u32> {
    parse_header(line).map(|(p, _)| p)
}

/// `pid` の実行の範囲: その PID の最後の区切りの行から、次の区切りの行（どの PID でも）の手前まで。
#[derive(Debug, PartialEq, Eq)]
pub struct RunSection<'a> {
    pub stem: &'a str,
    pub text: &'a str,
}

pub fn last_run_section(text: &str, pid: u32) -> Option<RunSection<'_>> {
    let mut headers: Vec<(usize, u32, &str)> = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if let Some((p, stem)) = parse_header(line.trim_end_matches(['\r', '\n'])) {
            headers.push((at, p, stem));
        }
        at += line.len();
    }
    let i = headers.iter().rposition(|&(_, p, _)| p == pid)?;
    let (start, _, stem) = headers[i];
    let end = headers.get(i + 1).map_or(text.len(), |h| h.0);
    Some(RunSection {
        stem,
        text: &text[start..end],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEM: &str = "dll_20261013-070309-007_pid4242";

    #[test]
    fn header_has_pid_time_and_stem_in_a_fixed_shape() {
        assert_eq!(
            run_header(4242, "2026-10-13 07:03:09.007", STEM),
            "==== 起動 pid 4242 / 2026-10-13 07:03:09.007 / dll_20261013-070309-007_pid4242 ===="
        );
    }

    #[test]
    fn the_last_section_of_the_pid_runs_to_the_next_header_of_any_pid() {
        let h = |pid, stem| run_header(pid, "t", stem);
        let text = format!(
            "{}\nA1\nA2\n{}\nB1\n{}\nA3\n{}\nB2\n",
            h(1, "dll_a"),
            h(2, "dll_b"),
            h(1, "dll_a2"),
            h(2, "dll_b2")
        );
        let a = last_run_section(&text, 1).unwrap();
        assert_eq!(a.stem, "dll_a2");
        assert_eq!(a.text, format!("{}\nA3\n", h(1, "dll_a2")));
        let b = last_run_section(&text, 2).unwrap();
        assert_eq!(b.stem, "dll_b2");
        assert_eq!(b.text, format!("{}\nB2\n", h(2, "dll_b2")));
        assert_eq!(last_run_section(&text, 3), None);
        assert_eq!(last_run_section("", 1), None);
    }

    #[test]
    fn only_a_whole_line_header_with_the_same_pid_counts() {
        let h = run_header(12, "t", "dll_x");
        // PID の前方一致（12 と 123）・行の途中・CRLF・形の崩れ。
        let text = format!(
            "{}\nx\n{}\r\ny\r\n前置き {h}\n==== 起動 pid 12 / t ====\n",
            run_header(123, "t", "dll_long"),
            h
        );
        let s = last_run_section(&text, 12).unwrap();
        assert_eq!(s.stem, "dll_x");
        assert!(s.text.starts_with(&h) && s.text.contains("y\r\n前置き"));
        assert_eq!(last_run_section(&text, 123).unwrap().stem, "dll_long");
        assert_eq!(last_run_section(&text, 1), None);
    }

    #[test]
    fn daily_names_are_eight_digits_and_nothing_else() {
        assert!(is_daily_log_name("dll_20261013.log"));
        for n in [
            "dll_20261013-070309-007_pid4242.log",
            "dll_20261013_replay.log",
            "dll_2026101.log",
            "dll_202610133.log",
            "dll_2026101a.log",
            "dll_20261013.log.gz",
            "x_20261013.log",
            "dll_20261013",
        ] {
            assert!(!is_daily_log_name(n), "{n}");
        }
    }

    /// 壊れたら落ちる: 区切りでない行（前置きつき・形の崩れ）を区切りと見る、または区切りの PID を読み違える。
    #[test]
    fn header_pid_reads_only_a_whole_header_line() {
        assert_eq!(header_pid(&run_header(4242, "t", STEM)), Some(4242));
        assert_eq!(
            header_pid(&format!("前置き {}", run_header(1, "t", STEM))),
            None
        );
        assert_eq!(header_pid("==== 起動 pid 12 / t ===="), None);
        assert_eq!(header_pid("普通の行"), None);
    }
}
