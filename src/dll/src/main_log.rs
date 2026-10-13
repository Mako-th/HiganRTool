//! 本ログへ行を頼む入口（pure）。
//!
//! ウィンドウの差し替え（`window_win.rs`）とパッドの十字キー（`pad_extra_win.rs`）は本ログの書き手（`win.rs` の読み取りのスレッド）ではなく、
//! `DllMain` の下でも走る。`logs\` の中を増やさないよう別ファイルは作らず、行をここへ頼み、本ログの書き手が書く。
//! 頼む側はメモリに積むだけ（ファイルを開かない・書かない・待たない）。
//!
//! 書き手は `Draining` で本ログを包み、書き込みが行の先頭から始まるたびに積んだ行を先に書く（`writeln!` は文字列を何度かに分けて書くので、
//! 「直前が改行か」で行の先頭を見分ける）。包みの下の `ScrubWrite` が 1 行を 1 回の書き込みにまとめてファイルへ渡す
//! （書き込み中のファイルをローダーが読んでも、行の途中で切れにくいように）。書き出しの順は「書き手が次に書いた時刻」なので、各行の先頭には出どころ（`ウィンドウ:`・`pad:`）を付ける。
//! 最後の 1 回は書き手が終わるときに `Draining::drain_now` で流す。ゲームのプロセスが先に終わると、その間に積んだ行は残らない。

#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::sync::Mutex;

/// 積んでおける行の数。書き手が先に終わった後も頼む側は走り続けるので上限を置く。溢れたら捨てて数える（次の書き出しの 1 行に出る）。
pub const MAX_QUEUED_LINES: usize = 64;

struct Inner {
    /// （詳細だけの行か, 行）。
    lines: Vec<(bool, String)>,
    /// 上限で捨てた行の数（書き出すと 0 に戻す）。
    dropped: usize,
}

/// 頼まれた行の待ち行列。製品は `QUEUE` を 1 つだけ使う（検査は自前のものを作る）。
pub struct LineQueue {
    inner: Mutex<Inner>,
}

impl LineQueue {
    pub const fn new() -> Self {
        LineQueue {
            inner: Mutex::new(Inner {
                lines: Vec::new(),
                dropped: 0,
            }),
        }
    }

    pub fn push(&self, line: impl Into<String>) {
        self.push_as(false, line.into());
    }

    /// 詳細のときだけ書く行。簡潔か詳細かは書くときの `log_mode::detail()` で選ぶ（頼む側は DllMain の中で、まだ ini を読んでいない）。
    pub fn push_detail(&self, line: impl Into<String>) {
        self.push_as(true, line.into());
    }

    /// `problem` なら簡潔にも書く行、そうでなければ詳細だけの行。
    pub fn push_when(&self, problem: bool, line: impl Into<String>) {
        self.push_as(!problem, line.into());
    }

    fn push_as(&self, detail_only: bool, line: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.lines.len() >= MAX_QUEUED_LINES {
            g.dropped += 1;
        } else {
            g.lines.push((detail_only, line));
        }
    }

    /// 積んである行を足した順に `out` へ書く。書けなかった行は戻さない。錠は取り出す間だけ持つ（頼む側を待たせない）。
    pub fn drain_into(&self, out: &mut dyn Write) {
        let (lines, dropped) = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (std::mem::take(&mut g.lines), std::mem::take(&mut g.dropped))
        };
        let detail = crate::log_mode::detail();
        for (detail_only, l) in &lines {
            if detail || !detail_only {
                let _ = writeln!(out, "{l}");
            }
        }
        if dropped > 0 {
            let _ = writeln!(
                out,
                "本ログへ頼まれた行が多すぎて {dropped} 行を捨てた（上限 {MAX_QUEUED_LINES} 行）"
            );
        }
    }

    #[cfg(test)]
    pub fn peek(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lines
            .iter()
            .map(|(_, l)| l.clone())
            .collect()
    }
}

impl Default for LineQueue {
    fn default() -> Self {
        Self::new()
    }
}

static QUEUE: LineQueue = LineQueue::new();

/// 本ログへ 1 行頼む（どのスレッドからでも。`DllMain` の中からでもよい）。
pub fn push(line: impl Into<String>) {
    QUEUE.push(line);
}

/// 詳細のときだけ書く行を頼む。
pub fn push_detail(line: impl Into<String>) {
    QUEUE.push_detail(line);
}

/// `problem`（失敗・誤り）なら簡潔にも書く行、そうでなければ詳細だけの行を頼む。
pub fn push_when(problem: bool, line: impl Into<String>) {
    QUEUE.push_when(problem, line);
}

pub fn wrap<W: Write>(inner: W) -> Draining<'static, W> {
    Draining::new(inner, &QUEUE)
}

#[cfg(test)]
pub fn peek() -> Vec<String> {
    QUEUE.peek()
}

pub struct Draining<'a, W: Write> {
    inner: W,
    queue: &'a LineQueue,
    /// 直前に書いたバイトが改行（か、まだ何も書いていない）= いま行の先頭。
    at_line_start: bool,
}

impl<'a, W: Write> Draining<'a, W> {
    pub fn new(inner: W, queue: &'a LineQueue) -> Self {
        Draining {
            inner,
            queue,
            at_line_start: true,
        }
    }

    /// いま行の先頭なら、積んである行を書く（書き手が終わるときの最後の 1 回）。行の途中なら何もしない。
    pub fn drain_now(&mut self) {
        if self.at_line_start {
            self.queue.drain_into(&mut self.inner);
        }
    }
}

impl<W: Write> Write for Draining<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.at_line_start {
            self.queue.drain_into(&mut self.inner);
        }
        let n = self.inner.write(buf)?;
        if n > 0 {
            self.at_line_start = buf[n - 1] == b'\n';
        }
        Ok(n)
    }

    /// 行の先頭なら、積んである行も先に書く（簡潔なログは 1 秒ごとの行が無いので、頼まれた行が次の書き込みまで残らないように）。
    fn flush(&mut self) -> io::Result<()> {
        self.drain_now();
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn lines_queued_before_the_log_exists_come_first() {
        let q = LineQueue::new();
        q.push("ウィンドウ: 設置: 差し替えた");
        q.push("pad: 設置 4/4 地点");
        let mut log = Draining::new(Vec::new(), &q);
        writeln!(log, "本ログの最初の行").unwrap();
        assert_eq!(
            text(log.inner),
            "ウィンドウ: 設置: 差し替えた\npad: 設置 4/4 地点\n本ログの最初の行\n"
        );
    }

    #[test]
    fn lines_queued_after_the_log_is_open_are_written_at_the_next_write() {
        let q = LineQueue::new();
        let mut log = Draining::new(Vec::new(), &q);
        writeln!(log, "A").unwrap();
        q.push("後から 1");
        q.push("後から 2");
        writeln!(log, "B").unwrap();
        writeln!(log, "C").unwrap();
        q.push("後から 3");
        log.drain_now();
        assert_eq!(text(log.inner), "A\n後から 1\n後から 2\nB\nC\n後から 3\n");
    }

    /// 壊れたら落ちる: 簡潔なログ（1 秒ごとの行が無い）で、別のスレッドが頼んだ行が次の書き込みまで残る。
    #[test]
    fn flush_writes_the_queued_lines_when_at_a_line_start() {
        let q = LineQueue::new();
        let mut log = Draining::new(Vec::new(), &q);
        writeln!(log, "A").unwrap();
        q.push("頼まれた行");
        log.flush().unwrap();
        assert_eq!(text(log.inner.clone()), "A\n頼まれた行\n");
        write!(log, "途中").unwrap();
        q.push("行の途中では書かない");
        log.flush().unwrap();
        assert_eq!(text(log.inner.clone()), "A\n頼まれた行\n途中");
    }

    #[test]
    fn a_queued_line_never_splits_a_log_line() {
        let q = LineQueue::new();
        let mut log = Draining::new(Vec::new(), &q);
        write!(log, "前半").unwrap();
        q.push("割り込み");
        log.drain_now(); // 行の途中なので何も書かない
        writeln!(log, "後半").unwrap();
        writeln!(log, "次").unwrap();
        assert_eq!(text(log.inner), "前半後半\n割り込み\n次\n");
    }

    #[test]
    fn the_queue_is_capped_and_says_how_many_it_dropped() {
        let q = LineQueue::new();
        for i in 0..MAX_QUEUED_LINES + 3 {
            q.push(format!("行 {i}"));
        }
        assert_eq!(q.peek().len(), MAX_QUEUED_LINES);
        let mut out = Vec::new();
        q.drain_into(&mut out);
        let t = text(out);
        assert_eq!(t.lines().count(), MAX_QUEUED_LINES + 1);
        assert!(t.contains("3 行を捨てた"), "{t}");
        assert!(q.peek().is_empty(), "流した行を残している");
    }

    #[test]
    fn detail_only_lines_are_dropped_in_brief_but_the_rest_stay() {
        for (detail, want) in [(true, "詳細\n異常\n"), (false, "異常\n")] {
            let q = LineQueue::new();
            q.push_detail("詳細");
            q.push("異常");
            let mut out = Vec::new();
            crate::log_mode::with_detail(detail, || q.drain_into(&mut out));
            assert_eq!(text(out), want, "detail = {detail}");
            assert!(q.peek().is_empty());
        }
    }

    /// 壊れたら落ちる: 失敗の行が詳細だけに回る（簡潔で消える）、または成功の行が簡潔に出る。
    #[test]
    fn push_when_routes_problems_to_brief_and_the_rest_to_detail_only() {
        for (detail, want) in [(true, "問題\nうまくいった\n"), (false, "問題\n")] {
            let q = LineQueue::new();
            q.push_when(true, "問題");
            q.push_when(false, "うまくいった");
            let mut out = Vec::new();
            crate::log_mode::with_detail(detail, || q.drain_into(&mut out));
            assert_eq!(text(out), want, "detail = {detail}");
        }
    }

    #[test]
    fn a_real_log_file_gets_the_lines_and_no_other_file_appears() {
        let dir = std::env::temp_dir().join(format!("main_log_real_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dll_test.log");
        let q = LineQueue::new();
        q.push("ウィンドウ: 前に積んだ");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut log = Draining::new(f, &q);
        writeln!(log, "本ログ").unwrap();
        q.push("pad: 後から");
        log.drain_now();
        drop(log);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "ウィンドウ: 前に積んだ\n本ログ\npad: 後から\n"
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["dll_test.log".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_probe_thread_wraps_the_main_log_before_the_session_and_drains_at_the_end() {
        let src = include_str!("win.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let pos = |needle: &str| {
            let hits: Vec<_> = body.match_indices(needle).collect();
            assert_eq!(hits.len(), 1, "{needle} が {} 回", hits.len());
            hits[0].0
        };
        let header = pos("higanr_core::log_names::run_header(");
        let header_write = pos("file.write_all(header.as_bytes())");
        let wrap = pos("crate::main_log::wrap(ScrubWrite::new(");
        let run = pos("session::run(");
        let final_counts = pos("crate::pad_extra_win::push_final_counts()");
        let drain = pos("file.drain_now()");
        assert!(
            header < header_write
                && header_write < wrap
                && wrap < run
                && run < final_counts
                && final_counts < drain,
            "並びが違う: header {header} / write {header_write} / wrap {wrap} / run {run} / final {final_counts} / drain {drain}"
        );
    }

    /// 壊れたら落ちる: 簡潔 / 詳細を決める前にログへ書き始める、既定がビルドの種類に結び付いていない、
    /// 本ログの行が伏せる処理を通らずにファイルへ行く。
    #[test]
    fn the_mode_is_decided_from_the_ini_and_the_build_default_before_the_first_line() {
        let src = include_str!("win.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let pos = |needle: &str| {
            let hits: Vec<_> = body.match_indices(needle).collect();
            assert_eq!(hits.len(), 1, "{needle} が {} 回", hits.len());
            hits[0].0
        };
        let scrub = pos("crate::main_log::wrap(ScrubWrite::new(");
        let choose = pos("higanr_core::log_cfg::choose(");
        let default = pos("crate::log_mode::default_detail(cfg!(feature = \"dist\"))");
        let set = pos("crate::log_mode::set_detail(choice.detail)");
        let first_line = pos("mode_line(choice.detail)");
        let session = pos("session::run(");
        assert!(
            scrub < choose && choose < default && default < set && set < first_line,
            "並びが違う"
        );
        assert!(first_line < session, "モードの行はセッションの前");
        assert!(
            body.contains("script.read_net_ini()"),
            "ini は他の設定と同じ入口で読む"
        );
        // 読み取りのスレッドがパニックで止まった行は簡潔にも残す。
        let panic_arm = body
            .split("Err(p) => writeln!(")
            .nth(1)
            .expect("パニックの腕");
        assert!(
            panic_arm.trim_start().starts_with("file,"),
            "パニックの行を詳細だけにしない"
        );
    }

    #[test]
    fn window_and_pad_do_not_open_side_log_files() {
        for (name, src) in [
            ("window_win.rs", include_str!("window_win.rs")),
            ("pad_extra_win.rs", include_str!("pad_extra_win.rs")),
            ("pad_extra.rs", include_str!("pad_extra.rs")),
        ] {
            let body = src.split("#[cfg(test)]").next().unwrap();
            for (i, line) in body.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for banned in [
                    "OpenOptions",
                    "File::create",
                    "side_file_name",
                    "_window.log",
                    "_pad.log",
                ] {
                    assert!(
                        !code.contains(banned),
                        "{name}:{} に {banned}: {code}",
                        i + 1
                    );
                }
            }
        }
    }
}
