//! ログの簡潔 / 詳細の切り替え（pure。unsafe 無し）。
//!
//! 簡潔は「本ログ 1 本だけ。起動・ネットワーク・試合の節目・異常・終わりのまとめ」、詳細は今までの全部（CSV も）。切り替えは ini の `[log] detail`
//! （`higanr_core::log_cfg`）。決めるのは読み取りのスレッドの始まり（ログを開いた直後）で、それより前にログへ頼まれた行は
//! 書くときの値で選ぶ。
//!
//! 書き方: 詳細だけの行は `writeln!(detail_out(out), ...)`、異常と節目の行は今までどおり `writeln!(out, ...)`。
//! 迷う行は簡潔に残す（異常の行を落とさないほうを優先する）。

#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// 配るバージョンは簡潔、開発用は詳細（実機の回の解析が CSV に頼っている）。
pub const fn default_detail(dist: bool) -> bool {
    !dist
}

/// 検査のビルドは常に詳細から始める（既存の検査は詳細の行を見る）。簡潔を見る検査は `with_detail(false, ..)` で包む。
static DETAIL: AtomicBool = AtomicBool::new(true);

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// いま詳細か。
pub fn detail() -> bool {
    #[cfg(test)]
    if let Some(v) = OVERRIDE.with(std::cell::Cell::get) {
        return v;
    }
    DETAIL.load(Ordering::Relaxed)
}

/// 読み取りのスレッドの始まりで 1 回置く。
pub fn set_detail(on: bool) {
    DETAIL.store(on, Ordering::Relaxed);
}

/// 検査だけ: この検査のスレッドの間だけ値を決める（検査は並列に走るので、全体の値は触らない）。
#[cfg(test)]
pub fn with_detail<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<bool>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }
    let _restore = Restore(OVERRIDE.with(|o| o.replace(Some(on))));
    f()
}

/// パスの最後の 1 要素（ログにフォルダの並びを書かないため）。
pub fn leaf(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
}

/// 詳細のときだけ中へ流す書き込み先。簡潔のときは書いたことにして捨てる。
pub struct DetailOut<'a> {
    inner: &'a mut dyn Write,
    on: bool,
}

pub fn detail_out(out: &mut dyn Write) -> DetailOut<'_> {
    DetailOut {
        inner: out,
        on: detail(),
    }
}

impl Write for DetailOut<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.on {
            self.inner.write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.on { self.inner.flush() } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_defaults_to_brief_and_dev_to_detail() {
        assert!(!default_detail(true));
        assert!(default_detail(false));
        // このビルドで、`[log]` の無い ini（古い ini）のときに実際に決まる値: 配るバージョンは簡潔、開発バージョンは詳細。
        let c = higanr_core::log_cfg::choose(
            Some("[netplay]\nenable = 1\n"),
            default_detail(cfg!(feature = "dist")),
        );
        assert_eq!(c.detail, !cfg!(feature = "dist"));
        assert_eq!(c.note, None);
    }

    #[test]
    fn detail_out_drops_lines_in_brief_and_passes_them_in_detail() {
        let mut buf = Vec::new();
        with_detail(false, || {
            writeln!(detail_out(&mut buf), "詳細だけ").unwrap();
        });
        assert!(buf.is_empty());
        with_detail(true, || {
            writeln!(detail_out(&mut buf), "詳細だけ").unwrap();
        });
        assert_eq!(String::from_utf8(buf).unwrap(), "詳細だけ\n");
    }

    #[test]
    fn leaf_keeps_only_the_last_component() {
        assert_eq!(leaf(r"D:\games\alice\code_a"), "code_a");
        assert_eq!(leaf("code"), "code");
        assert_eq!(leaf("a/b/"), "b");
        assert_eq!(leaf(""), "");
    }

    #[test]
    fn override_is_per_thread_and_restored() {
        with_detail(false, || {
            assert!(!detail());
            std::thread::spawn(|| assert!(detail())).join().unwrap();
            with_detail(true, || assert!(detail()));
            assert!(!detail());
        });
        assert!(detail());
    }
}
