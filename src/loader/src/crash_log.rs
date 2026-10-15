//! ローダーの段階ログ（`logs\loader_YYYYMMDD.log`）と、パニック・未処理例外の記録。
//! 起動の途中で落ちたときに原因が残るようにするためのもので、書けなくても起動は続ける。
//! 個人を特定できる字は `Scrub` を通して伏せる（ゲームのパスは書かない）。
//!
//! ツールをゲームのフォルダの中に置いた人のゲームのフォルダへは何も作らない。そのためログのファイルは、ゲームのフォルダが
//! 分かって「外」と確かめるまで開かず、それまでの行はメモリに貯める（`StageLog::decide`）。

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use higanr_core::lang::Lang;
use higanr_core::log_days::{is_expired as date_is_expired, parse_date8};
use higanr_core::log_scrub::Scrub;

use crate::code_relay::is_same_or_inside;

pub use higanr_core::log_days::KEEP_LOG_DAYS;

pub const LOG_PREFIX: &str = "loader_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalStamp {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

pub fn log_file_name(t: &LocalStamp) -> String {
    format!("{LOG_PREFIX}{:04}{:02}{:02}.log", t.year, t.month, t.day)
}

/// `loader_YYYYMMDD.log` で、その日が今日から `keep_days` 日より前なら true。名前の形が違うものと、
/// 今日より先の日付（時計を戻した）は消さない。
pub fn is_expired(name: &str, today: &LocalStamp, keep_days: i64) -> bool {
    let Some(date) = name
        .strip_prefix(LOG_PREFIX)
        .and_then(|r| r.strip_suffix(".log"))
        .and_then(parse_date8)
    else {
        return false;
    };
    let today = (
        i64::from(today.year),
        i64::from(today.month),
        i64::from(today.day),
    );
    date_is_expired(today, date, keep_days)
}

/// `dir` の直下の古い `loader_…log` を消す。（消した数, 消せなかった数）。
pub fn prune(dir: &Path, today: &LocalStamp, keep_days: i64) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let (mut removed, mut failed) = (0, 0);
    for e in entries.flatten() {
        let is_file = e.file_type().is_ok_and(|t| t.is_file());
        let expired = e
            .file_name()
            .to_str()
            .is_some_and(|n| is_expired(n, today, keep_days));
        if !(is_file && expired) {
            continue;
        }
        match std::fs::remove_file(e.path()) {
            Ok(()) => removed += 1,
            Err(_) => failed += 1,
        }
    }
    (removed, failed)
}

/// ログのファイルを開く前に貯める行数の上限。起動の手順の行は数十行なので、これを超えるのは異常で、超えた分は捨てる。
const MAX_PENDING: usize = 400;

/// 開く前に貯めた 1 行。区切りの行は時刻も伏せ字も付けずに書く。
enum Pending {
    Header(String),
    Line(LocalStamp, String),
}

enum State {
    /// ゲームのフォルダが分かるまで。ファイルもフォルダも作らない。
    Waiting {
        lines: Vec<Pending>,
        dropped: usize,
    },
    Writing(File),
    /// 書かない（ゲームのフォルダの中、または開けなかった）。
    Silent(Decision),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Opened,
    /// ログの置き場がゲームのフォルダの中なので、ファイルを作らない。
    InGameFolder,
    /// フォルダを作れない・ファイルを開けない。
    OpenFailed,
}

/// ツールのフォルダ `tool_dir` がゲームのフォルダ `game_dir` と同じか、その下か。ログの置き場（`tool_dir\logs`）の判定に使う。
/// `..` や別名のパス（ジャンクション・8.3）を解いた形でも比べ、どれか 1 つでも「中」なら中とする
/// （ゲームのフォルダに書く誤りより、書かない誤りを選ぶ）。
pub fn is_inside_game(tool_dir: &Path, game_dir: &Path) -> bool {
    is_same_or_inside(tool_dir, game_dir)
        || is_same_or_inside(&resolved(tool_dir), &resolved(game_dir))
}

/// 存在する所までを `canonicalize` で解く。無い部分はそのまま足す。`\\?\` の接頭辞は外す（外さないと素のパスと比べられない）。
fn resolved(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return strip_verbatim(c);
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => resolved(parent).join(name),
        _ => p.to_path_buf(),
    }
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

fn stamp_text(t: &LocalStamp) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03} ",
        t.hour, t.minute, t.second, t.millis
    )
}

/// 1 行ずつ追記して、そのたびに OS へ渡す。開けなかったときは何もしない。
pub struct StageLog {
    dir: PathBuf,
    state: State,
    scrub: Scrub,
    clock: fn() -> LocalStamp,
}

impl StageLog {
    /// ファイルにもフォルダにも触らない。`decide` までの行はメモリに貯める。
    pub fn new(dir: &Path, clock: fn() -> LocalStamp, scrub: Scrub) -> StageLog {
        StageLog {
            dir: dir.to_path_buf(),
            state: State::Waiting {
                lines: Vec::new(),
                dropped: 0,
            },
            scrub,
            clock,
        }
    }

    /// ログの置き場がゲームのフォルダの中でなければ、フォルダとファイルを作って貯めた行を書く。中なら以後も何も書かない。
    /// 決めた後に呼んでも結果は変わらない。
    pub fn decide(&mut self, inside_game_folder: bool) -> Decision {
        if !self.is_waiting() {
            return self.decision();
        }
        let State::Waiting { lines, dropped } =
            std::mem::replace(&mut self.state, State::Silent(Decision::OpenFailed))
        else {
            return Decision::OpenFailed;
        };
        if inside_game_folder {
            self.state = State::Silent(Decision::InGameFolder);
            return Decision::InGameFolder;
        }
        let _ = std::fs::create_dir_all(&self.dir);
        let Ok(file) = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.dir.join(log_file_name(&(self.clock)())))
        else {
            return Decision::OpenFailed;
        };
        self.state = State::Writing(file);
        for l in lines {
            match l {
                Pending::Header(h) => self.write_raw(&h),
                Pending::Line(t, text) => {
                    let line = format!("{}{}", stamp_text(&t), self.scrub.line(&text));
                    self.write_raw(&line);
                }
            }
        }
        if dropped > 0 {
            self.line(&format!(
                "開く前に貯めきれなかった行を {dropped} 行捨てた（上限 {MAX_PENDING} 行）"
            ));
        }
        Decision::Opened
    }

    fn decision(&self) -> Decision {
        match &self.state {
            State::Writing(_) => Decision::Opened,
            State::Silent(d) => *d,
            State::Waiting { .. } => Decision::OpenFailed,
        }
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Writing(_))
    }

    /// まだゲームのフォルダが分からず、行を貯めている間。
    pub fn is_waiting(&self) -> bool {
        matches!(self.state, State::Waiting { .. })
    }

    /// 以降の行で、`path` で始まる字を `label` に替える。貯めた行にも、書き出すときに効く。
    pub fn hide_dir(&mut self, path: &str, label: &str) {
        self.scrub = std::mem::take(&mut self.scrub).dir(path, label);
    }

    /// 未処理例外のフィルターへ渡す、同じファイルを指す別のハンドル。
    pub fn clone_file(&self) -> Option<File> {
        match &self.state {
            State::Writing(f) => f.try_clone().ok(),
            _ => None,
        }
    }

    /// 起動の区切り。同じ日の起動は同じファイルに続くので、どこからが今回かを見分ける。
    pub fn begin_run(&mut self, pid: u32) {
        let t = (self.clock)();
        let head = format!(
            "==== 起動 pid {pid} / {:04}-{:02}-{:02} {:02}:{:02}:{:02} ====",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        );
        match &mut self.state {
            State::Waiting { lines, .. } => lines.push(Pending::Header(head)),
            State::Writing(_) => self.write_raw(&head),
            State::Silent(_) => {}
        }
    }

    pub fn line(&mut self, text: &str) {
        let t = (self.clock)();
        for l in text.lines() {
            match &mut self.state {
                State::Waiting { lines, dropped } => {
                    if lines.len() < MAX_PENDING {
                        lines.push(Pending::Line(t, l.to_string()));
                    } else {
                        *dropped += 1;
                    }
                }
                State::Writing(_) => {
                    let line = format!("{}{}", stamp_text(&t), self.scrub.line(l));
                    self.write_raw(&line);
                }
                State::Silent(_) => {}
            }
        }
    }

    fn write_raw(&mut self, line: &str) {
        let State::Writing(f) = &mut self.state else {
            return;
        };
        // 1 行を 1 回の書き込みにする（2 つ起動しても行の途中で混ざらない）。
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        let _ = f.write_all(&bytes);
        let _ = f.flush();
    }
}

static LOG: Mutex<Option<StageLog>> = Mutex::new(None);

pub fn install(log: StageLog) {
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
}

/// ログが入っていれば 1 行書く（開く前なら貯める。ゲームのフォルダの中なら捨てる）。
pub fn stage(text: &str) {
    if let Some(l) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        l.line(text);
    }
}

pub fn hide_dir(path: &str, label: &str) {
    if let Some(l) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        l.hide_dir(path, label);
    }
}

/// ゲームのフォルダが分かるのを待っているか。
pub fn is_waiting() -> bool {
    LOG.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(StageLog::is_waiting)
}

/// `StageLog::decide`。ログが入っていなければ開けなかった扱い。
pub fn decide(inside_game_folder: bool) -> Decision {
    LOG.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map_or(Decision::OpenFailed, |l| l.decide(inside_game_folder))
}

/// 開いたログと同じファイルを指す別のハンドル。
pub fn clone_file() -> Option<File> {
    LOG.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(StageLog::clone_file)
}

/// パニックのフックから書く用。錠が取れなければ（書き込み中のスレッドがパニックした）諦める。
fn stage_try(text: &str) {
    if let Ok(mut g) = LOG.try_lock()
        && let Some(l) = g.as_mut()
    {
        l.line(text);
    }
}

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_lang(lang: Lang) {
    ENGLISH.store(lang == Lang::En, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if ENGLISH.load(Ordering::Relaxed) {
        Lang::En
    } else {
        Lang::Ja
    }
}

// ---- パニック ----

pub fn panic_text(thread: &str, message: &str, location: Option<(&str, u32, u32)>) -> String {
    match location {
        Some((file, line, col)) => {
            format!("パニック（スレッド {thread}）: {message} @ {file}:{line}:{col}")
        }
        None => format!("パニック（スレッド {thread}）: {message}"),
    }
}

type Hook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// パニックの文と場所を `sink` に渡し、元のフック（`default`）を呼ぶ。主スレッドのパニックなら、その後 `hold` を呼ぶ
/// （窓が消える前に読めるようにする。他のスレッドは呼び元が続くので待たない）。
pub fn panic_hook(default: Hook, sink: fn(&str), hold: fn()) -> Hook {
    Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("（文なし）");
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("（名前なし）");
        let location = info.location().map(|l| (l.file(), l.line(), l.column()));
        sink(&panic_text(name, message, location));
        default(info);
        if name == "main" {
            hold();
        }
    })
}

/// 本番用: ログへ書き、主スレッドなら `hold` で待つ。
pub fn install_panic_hook(hold: fn()) {
    let default = std::panic::take_hook();
    std::panic::set_hook(panic_hook(default, stage_try, hold));
}

// ---- 未処理例外 ----

/// 例外のフィルターの中で使う固定長の書き込み先（確保しない）。入りきらない分は捨てる。
pub struct StackBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackBuf<N> {
    pub const fn new() -> Self {
        StackBuf {
            buf: [0; N],
            len: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> Default for StackBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // 文字の途中で切らない（日本語の文が壊れたログにならないように）。
        let mut n = s.len().min(N - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

pub struct ExceptionFacts<'a> {
    pub code: u32,
    pub address: usize,
    /// アドレスが入っているモジュールのファイル名（フォルダを含まない。ASCII）。
    pub module: &'a [u8],
    pub rva: u32,
    /// アクセス違反のときの（種類 0 読み / 1 書き / 8 実行, 触ったアドレス）。
    pub access: Option<(usize, usize)>,
}

fn exception_name(code: u32) -> &'static str {
    higanr_core::exception_names::name(code).unwrap_or("")
}

/// 例外の 1 行。確保しない（`fmt::Write` の先が固定長のときだけ使う）。
pub fn format_exception(out: &mut impl fmt::Write, f: &ExceptionFacts<'_>) -> fmt::Result {
    write!(
        out,
        "未処理の例外: コード 0x{:08X}（{}）アドレス 0x{:08X} = ",
        f.code,
        exception_name(f.code),
        f.address
    )?;
    if f.module.is_empty() {
        out.write_str("(モジュール不明)")?;
    } else {
        for &b in f.module {
            out.write_char(if b.is_ascii_graphic() { b as char } else { '?' })?;
        }
        write!(out, "+0x{:X}", f.rva)?;
    }
    if let Some((kind, at)) = f.access {
        let kind = match kind {
            0 => "読み",
            1 => "書き",
            8 => "実行",
            _ => "?",
        };
        write!(out, " / {kind} 0x{at:08X}")?;
    }
    out.write_char('\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn stamp(y: u16, m: u16, d: u16) -> LocalStamp {
        LocalStamp {
            year: y,
            month: m,
            day: d,
            hour: 3,
            minute: 55,
            second: 1,
            millis: 12,
        }
    }

    fn fixed_clock() -> LocalStamp {
        stamp(2026, 10, 14)
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "HiganRTool_loader_crash_log_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 置き場がゲームのフォルダの外と分かった後の状態にして返す。
    fn opened(dir: &Path, scrub: Scrub) -> StageLog {
        let mut log = StageLog::new(dir, fixed_clock, scrub);
        assert_eq!(log.decide(false), Decision::Opened);
        log
    }

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("loader_20261014.log")).unwrap()
    }

    #[test]
    fn the_log_is_named_by_day_and_only_old_loader_logs_are_pruned() {
        assert_eq!(log_file_name(&stamp(2026, 1, 9)), "loader_20260109.log");
        let today = stamp(2026, 10, 14);
        // 3 日前は残し、4 日前から消す。月をまたいでも日数で数える。
        assert!(!is_expired("loader_20261011.log", &today, KEEP_LOG_DAYS));
        assert!(is_expired("loader_20261010.log", &today, KEEP_LOG_DAYS));
        assert!(is_expired("loader_20260930.log", &today, KEEP_LOG_DAYS));
        assert!(!is_expired("loader_20261015.log", &today, KEEP_LOG_DAYS));
        for n in [
            "dll_20200101.log",
            "loader_2020.log",
            "loader_20261310.log",
            "loader_20200101.txt",
            "loader_2020010a.log",
        ] {
            assert!(!is_expired(n, &today, KEEP_LOG_DAYS), "{n}");
        }
        let d = temp_dir("prune");
        for n in [
            "loader_20200101.log",
            "loader_20261014.log",
            "dll_20200101.log",
            "notes.txt",
        ] {
            std::fs::write(d.join(n), "x").unwrap();
        }
        std::fs::create_dir(d.join("loader_20190101.log")).unwrap();
        assert_eq!(prune(&d, &today, KEEP_LOG_DAYS), (1, 0));
        let mut left: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "dll_20200101.log",
                "loader_20190101.log",
                "loader_20261014.log",
                "notes.txt"
            ]
        );
        assert_eq!(prune(&d.join("無い"), &today, KEEP_LOG_DAYS), (0, 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lines_are_appended_in_order_with_a_time_and_a_run_header() {
        let d = temp_dir("lines");
        let mut log = opened(&d, Scrub::new());
        log.begin_run(4321);
        log.line("一つ目");
        log.line("二つ目\n三つ目");
        // 同じ日の次の起動は同じファイルに続く。
        let mut again = opened(&d, Scrub::new());
        again.begin_run(77);
        again.line("次の起動");
        assert_eq!(
            read(&d),
            "==== 起動 pid 4321 / 2026-10-14 03:55:01 ====\r\n\
             03:55:01.012 一つ目\r\n\
             03:55:01.012 二つ目\r\n\
             03:55:01.012 三つ目\r\n\
             ==== 起動 pid 77 / 2026-10-14 03:55:01 ====\r\n\
             03:55:01.012 次の起動\r\n"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_line_is_on_disk_before_line_returns() {
        let d = temp_dir("flush");
        let mut log = opened(&d, Scrub::new());
        log.line("CreateProcessW を呼ぶ");
        // 開いたままのログを別のハンドルで読む（落ちる直前の行が残っているか）。
        assert!(read(&d).ends_with("CreateProcessW を呼ぶ\r\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn personal_text_is_hidden_before_it_reaches_the_file() {
        let d = temp_dir("scrub");
        let tool = r"E:\Games\Tool";
        let mut log = opened(&d, Scrub::new().dir(tool, "<tool>"));
        log.line(&format!(
            "読めない: E:{0}Games{0}Tool{0}HiganRTool.dll と C:{0}Users{0}Taro{0}Desktop{0}x.exe",
            '\\'
        ));
        log.line("接続先 203.0.113.9:7000");
        let text = read(&d);
        assert!(text.contains("<tool>\\HiganRTool.dll"), "{text}");
        assert!(!text.contains("E:\\Games"), "{text}");
        assert!(!text.contains("Taro"), "{text}");
        assert!(!text.contains("203.0.113.9"), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_directory_hidden_later_is_hidden_from_then_on() {
        let d = temp_dir("hide_later");
        let mut log = opened(&d, Scrub::new());
        log.line(r"前: F:\Games\Th09\th09.exe");
        log.hide_dir(r"F:\Games\Th09", "<ゲーム>");
        log.line(r"後: F:\Games\Th09\th09.exe");
        let text = read(&d);
        assert!(text.contains(r"前: F:\Games\Th09\th09.exe"), "{text}");
        assert!(text.contains(r"後: <ゲーム>\th09.exe"), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_log_that_cannot_be_opened_is_silent() {
        let d = temp_dir("blocked");
        // フォルダの場所にファイルがあって開けない。
        let blocker = d.join("logs");
        std::fs::write(&blocker, "x").unwrap();
        let mut log = StageLog::new(&blocker, fixed_clock, Scrub::new());
        log.line("開く前");
        assert_eq!(log.decide(false), Decision::OpenFailed);
        assert!(!log.is_open());
        assert!(log.clone_file().is_none());
        log.begin_run(1);
        log.line("書けない");
        assert_eq!(
            log.decide(false),
            Decision::OpenFailed,
            "決めた結果は変わらない"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tree(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                out.push(e.path().strip_prefix(dir).unwrap().display().to_string());
                if e.path().is_dir() {
                    stack.push(e.path());
                }
            }
        }
        out.sort();
        out
    }

    /// 壊れたら落ちる: ゲームのフォルダが分かる前にフォルダやファイルを作る。
    #[test]
    fn nothing_is_created_while_the_game_folder_is_unknown() {
        let tool = temp_dir("waiting");
        let mut log = StageLog::new(&tool.join("logs"), fixed_clock, Scrub::new());
        log.begin_run(5);
        log.line("更新の確認を始める");
        assert!(log.is_waiting() && !log.is_open());
        assert!(log.clone_file().is_none());
        assert!(tree(&tool).is_empty(), "{:?}", tree(&tool));
        let _ = std::fs::remove_dir_all(&tool);
    }

    /// 壊れたら落ちる: ゲームのフォルダの中（同じ・下の階層）にログを作る、または外なのに作らない・貯めた行を落とす・
    /// 区切りの行を先頭に置かない・貯めた行の伏せ字を効かせない。ゲームのフォルダは合成の一時フォルダ。
    #[test]
    fn the_log_is_created_only_outside_the_game_folder() {
        let root = temp_dir("placement");
        let game = root.join("game");
        let cases = [
            ("game", false),
            ("game/tools", false),
            ("game/tools/deeper", false),
            // 名前の頭だけ同じ別のフォルダ・兄弟のフォルダ・ゲームの親は外。
            ("game2", true),
            ("other", true),
            (".", true),
        ];
        for (rel, outside) in cases {
            let tool = root.join(rel);
            std::fs::create_dir_all(&tool).unwrap();
            std::fs::create_dir_all(&game).unwrap();
            let before = tree(&game);
            let secret = format!(r"X:\Secret\{}\th09.exe", rel.replace('/', "_"));
            let mut log = StageLog::new(&tool.join("logs"), fixed_clock, Scrub::new());
            log.begin_run(9);
            log.line(&format!("ゲーム {secret}"));
            log.hide_dir(&format!(r"X:\Secret\{}", rel.replace('/', "_")), "<ゲーム>");
            let inside = is_inside_game(&tool, &game);
            assert_eq!(inside, !outside, "{rel}");
            let expect = if outside {
                Decision::Opened
            } else {
                Decision::InGameFolder
            };
            assert_eq!(log.decide(inside), expect, "{rel}");
            log.line("決めた後の行");
            let file = tool.join("logs").join("loader_20261014.log");
            if outside {
                let text = std::fs::read_to_string(&file).unwrap();
                assert!(
                    text.starts_with("==== 起動 pid 9 / "),
                    "{rel}: 区切りの行が先頭にない\n{text}"
                );
                let (a, b) = (
                    text.find("<ゲーム>\\th09.exe").unwrap(),
                    text.find("決めた後の行").unwrap(),
                );
                assert!(a < b, "{rel}: 貯めた行が先に出ない\n{text}");
                assert!(
                    !text.contains("Secret"),
                    "{rel}: 貯めた行に伏せ字が効いていない\n{text}"
                );
            } else {
                assert!(
                    !tool.join("logs").exists(),
                    "{rel}: ゲームのフォルダの中に logs を作った"
                );
                assert!(log.clone_file().is_none());
            }
            assert_eq!(
                tree(&game),
                before,
                "{rel}: ゲームのフォルダの中身が変わった"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 壊れたら落ちる: 貯めている間に行が溢れても無限に貯める、または溢れたことを残さない。
    #[test]
    fn the_waiting_lines_have_a_limit_and_the_overflow_is_reported() {
        let d = temp_dir("overflow");
        let mut log = StageLog::new(&d, fixed_clock, Scrub::new());
        for i in 0..MAX_PENDING + 7 {
            log.line(&format!("行 {i}"));
        }
        assert_eq!(log.decide(false), Decision::Opened);
        let text = read(&d);
        assert!(
            text.contains(&format!("行 {}\r\n", MAX_PENDING - 1)),
            "{text}"
        );
        assert!(!text.contains(&format!("行 {MAX_PENDING}\r\n")), "{text}");
        assert!(text.contains("7 行捨てた"), "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 壊れたら落ちる: `..` や別の綴りのパスでゲームのフォルダの中を外と見誤る。
    #[test]
    fn a_path_that_climbs_back_into_the_game_folder_is_inside() {
        let root = temp_dir("climb");
        let game = root.join("game");
        let sub = game.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        let tricky = root.join("other").join("..").join("game").join("sub");
        assert!(is_inside_game(&tricky, &game));
        let upper = PathBuf::from(sub.to_string_lossy().to_uppercase());
        assert!(is_inside_game(&upper, &game));
        assert!(!is_inside_game(&root.join("other"), &game));
        assert!(!is_inside_game(&root.join("game2").join("x"), &game));
        // ゲームのフォルダが無い（まだ存在しない）ときも、綴りが中なら中。
        assert!(is_inside_game(
            &root.join("nogame").join("logs"),
            &root.join("nogame")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_cloned_handle_appends_to_the_same_file() {
        let d = temp_dir("clone");
        let mut log = opened(&d, Scrub::new());
        log.line("先");
        let mut other = log.clone_file().expect("開いている");
        other.write_all(b"filter line\n").unwrap();
        log.line("後");
        let text = read(&d);
        let (a, b, c) = (
            text.find("先").unwrap(),
            text.find("filter line").unwrap(),
            text.find("後").unwrap(),
        );
        assert!(a < b && b < c, "{text}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn panic_text_names_thread_message_and_place() {
        assert_eq!(
            panic_text("main", "boom", Some(("src\\main.rs", 12, 5))),
            "パニック（スレッド main）: boom @ src\\main.rs:12:5"
        );
        assert_eq!(
            panic_text("t", "boom", None),
            "パニック（スレッド t）: boom"
        );
    }

    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static HOLDS: AtomicU32 = AtomicU32::new(0);
    static DEFAULTS: AtomicU32 = AtomicU32::new(0);

    fn record(s: &str) {
        SEEN.lock().unwrap().push(s.to_string());
    }

    fn held() {
        HOLDS.fetch_add(1, Ordering::SeqCst);
    }

    // パニックのフックは全体で 1 つなので、このテストだけが差し替える。元のフックは戻す。
    #[test]
    fn the_panic_hook_logs_the_message_and_place_and_waits_only_on_the_main_thread() {
        let before = std::panic::take_hook();
        // 並んで走る他の検査がパニックしても数えないよう、このテストが起こすスレッドの分だけ数える。
        let default: Hook = Box::new(|_| {
            let t = std::thread::current();
            if matches!(t.name(), Some("worker" | "main")) {
                DEFAULTS.fetch_add(1, Ordering::SeqCst);
            }
        });
        std::panic::set_hook(panic_hook(default, record, held));
        let run = |name: &str, body: fn()| {
            let t = std::thread::Builder::new()
                .name(name.to_string())
                .spawn(body)
                .unwrap();
            assert!(t.join().is_err());
        };
        run("worker", || panic!("literal"));
        run("main", || panic!("formatted {}", 7));
        let _ = std::panic::take_hook();
        std::panic::set_hook(before);

        let seen: Vec<String> = SEEN
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("スレッド worker）") || s.contains("スレッド main）"))
            .cloned()
            .collect();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].starts_with("パニック（スレッド worker）: literal @ ")
                && seen[0].contains("crash_log.rs:"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("パニック（スレッド main）: formatted 7 @ "),
            "{}",
            seen[1]
        );
        assert_eq!(HOLDS.load(Ordering::SeqCst), 1, "待ったのは主スレッドだけ");
        assert_eq!(DEFAULTS.load(Ordering::SeqCst), 2, "元のフックも呼ぶ");
    }

    #[test]
    fn exception_line_has_code_module_rva_and_access() {
        let mut b = StackBuf::<256>::new();
        format_exception(
            &mut b,
            &ExceptionFacts {
                code: 0xC000_0005,
                address: 0x0040_1234,
                module: b"HiganRTool.exe",
                rva: 0x1234,
                access: Some((1, 0)),
            },
        )
        .unwrap();
        assert_eq!(
            std::str::from_utf8(b.as_bytes()).unwrap(),
            "未処理の例外: コード 0xC0000005（アクセス違反）アドレス 0x00401234 = HiganRTool.exe+0x1234 / 書き 0x00000000\n"
        );
        let mut b = StackBuf::<256>::new();
        format_exception(
            &mut b,
            &ExceptionFacts {
                code: 0xE06D_7363,
                address: 0x7700_0000,
                module: b"",
                rva: 0,
                access: None,
            },
        )
        .unwrap();
        let s = std::str::from_utf8(b.as_bytes()).unwrap();
        assert!(
            s.contains("0xE06D7363（）") && s.contains("(モジュール不明)"),
            "{s}"
        );
    }

    /// 壊れたら落ちる: ローダーだけが別の名前の表を持つ（DLL のログと同じコードが違う名前で書かれる）。
    #[test]
    fn exception_names_come_from_the_table_the_dll_log_also_uses() {
        use higanr_core::exception_names::{BREAKPOINT, fatal_name};
        for code in [
            0xC000_0005u32,
            0xC000_0094,
            0xC000_00FD,
            0xC000_0374,
            0xC000_0409,
        ] {
            let want = fatal_name(code).expect("共有の表にある");
            assert_eq!(exception_name(code), want, "{code:08X}");
        }
        assert_eq!(exception_name(BREAKPOINT), "ブレークポイント");
        assert_eq!(exception_name(0xE06D_7363), "");
    }

    #[test]
    fn the_stack_buffer_drops_the_overflow_without_failing() {
        use std::fmt::Write as _;
        let mut b = StackBuf::<8>::new();
        assert!(write!(b, "0123456789").is_ok());
        assert_eq!(b.as_bytes(), b"01234567");
        assert!(write!(b, "x").is_ok());
        assert_eq!(b.as_bytes(), b"01234567");
        let mut j = StackBuf::<8>::new();
        write!(j, "ab例外例外").unwrap();
        assert_eq!(
            std::str::from_utf8(j.as_bytes()),
            Ok("ab例外"),
            "文字の途中で切った"
        );
    }
}
