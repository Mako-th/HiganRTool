//! 大きい CSV（`_net_frames.csv`・`_pacing.csv`・`_watch_frames.csv`）を gzip で書く・読む（unsafe 無し）。
//! 配布バージョンのログが大きすぎた（1 起動で `_net_frames.csv` 5〜12 MB・`_pacing.csv` 7〜15 MB）ので、CSV だけ gzip にする。
//!
//! 圧縮は読み取りのスレッド（`_net_frames.csv`）と書き出しのスレッド（`_pacing.csv`）で走り、ゲームのスレッドはリングバッファへ書くだけ。
//!
//! # 強制終了・落ちたとき
//!
//! ゲームを閉じるとプロセスごと消える（ライターの `Drop` は走らない）ので、終わりの印（CRC32 と長さの 8 B）の無い gzip が
//! 普通の終わり方になる。次の 2 つで備える。
//! - 書く側: `flush` のうち `SYNC_INTERVAL` ごとの 1 回だけを deflate の sync flush（空の stored ブロックで境を byte に
//!   揃える。辞書は捨てない）にする。毎回入れると圧縮率が落ちる。
//! - 読む側: `decode_lenient` は終わりの印が無くても最後の sync flush までの中身を返す（`LogText::cut` が立つ）。
//!
//! 失う行は最後の sync flush より後の約 1 秒分（60 fps で約 60 行）。間隔を 2 秒にすると圧縮後の大きさは約 2% 増に収まる
//! （1 秒は約 5% 増。`sync_flush_costs_little`）が、失う行が倍になるので 1 秒にした。

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// 書き出しの圧縮の強さ。gzip -6 と同じ（実測で `_net_frames.csv` は元の 23%、`_pacing.csv` は 33%）。
const LEVEL: u32 = 6;

/// sync flush の間隔。1 秒の呼び手に必ず当たるよう 1 秒より少し短くする（ちょうど 1 秒だと、呼びが 1 ms 早く来た回を
/// 飛ばして最大 2 秒を失う）。
pub const SYNC_INTERVAL: Duration = Duration::from_millis(900);

/// gzip で書くライター。捨てると（`Drop`）終わりの印を書く。
pub struct GzLogWriter<W: Write> {
    enc: GzEncoder<W>,
    last_sync: Instant,
    sync_interval: Duration,
}

impl<W: Write> GzLogWriter<W> {
    pub fn new(inner: W) -> Self {
        Self::with_sync_interval(inner, SYNC_INTERVAL)
    }

    /// 検査用に間隔を替えられる（`Duration::ZERO` なら `flush` のたびに sync flush）。
    pub fn with_sync_interval(inner: W, sync_interval: Duration) -> Self {
        GzLogWriter {
            enc: GzEncoder::new(inner, Compression::new(LEVEL)),
            last_sync: Instant::now(),
            sync_interval,
        }
    }

    pub fn finish(self) -> io::Result<W> {
        self.enc.finish()
    }
}

impl<W: Write> Write for GzLogWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.enc.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.last_sync.elapsed() >= self.sync_interval {
            self.enc.flush()?;
            self.last_sync = Instant::now();
        }
        Ok(())
    }
}

/// gzip で書くファイルの名前の尻。ライターを開く側（dll の `open_output`）がこれで見分ける。
pub const GZ_EXT: &str = ".gz";

pub fn is_gz_name(name: &str) -> bool {
    name.ends_with(GZ_EXT)
}

/// gzip の先頭の 2 B（RFC 1952）。1 B（`0x1f`）だけで切れたものも gzip として扱う（`0x1f` は CSV の先頭にならない）。
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0x1f) && bytes.get(1).is_none_or(|&b| b == 0x8b)
}

/// 終わりの印が無くても読めるところまで伸ばす。戻りの `bool` は印まで届かなかったか。壊れている場合も、強制終了の
/// 切れ方と区別せず `(それまでの分, true)` を返す。
pub fn decode_lenient(bytes: &[u8]) -> (Vec<u8>, bool) {
    let mut dec = GzDecoder::new(bytes);
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match dec.read(&mut buf) {
            Ok(0) => return (out, false),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => return (out, true),
        }
        // 小さな入力が巨大に伸びる gzip でメモリを食わない（実物の CSV は展開後 22 MB ほど）。
        if out.len() > MAX_DECODED {
            return (out, true);
        }
    }
}

const MAX_DECODED: usize = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogText {
    pub text: String,
    /// gzip が終わりの印まで届かず、最後の行の手前で切った。
    pub cut: bool,
}

impl LogText {
    pub fn cut_note(&self, name: &str) -> Option<String> {
        self.cut.then(|| {
            format!(
                "{name} は gzip の終わりの印が無い（ゲームを閉じた・強制終了のときは普通にこうなる）。最後の約 1 秒ぶんの行が欠けうる。読めた所までを使う"
            )
        })
    }
}

/// 先頭の 2 B で見分ける（拡張子は見ない）。切れた gzip は最後の `\n` までで切る。UTF-8 でなければ `InvalidData`。
pub fn text_from_bytes(bytes: Vec<u8>) -> io::Result<LogText> {
    let (raw, cut) = if is_gzip(&bytes) {
        decode_lenient(&bytes)
    } else {
        (bytes, false)
    };
    let mut raw = raw;
    if cut {
        let keep = raw.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        raw.truncate(keep);
    }
    let text = String::from_utf8(raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(LogText { text, cut })
}

pub fn read_log_text(path: &Path) -> io::Result<LogText> {
    text_from_bytes(std::fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実物の形の合成の行（`seg,frame,p1,p2,delay,margin_us,wait_us`。実在のログは使わない）。
    fn net_rows(n: u32) -> String {
        let mut s = String::from("seg,frame,p1,p2,delay,margin_us,wait_us\n");
        let mut x: u32 = 12345;
        for f in 0..n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let (p1, p2) = (x >> 16 & 0x03FF, x >> 6 & 0x03FF);
            s.push_str(&format!(
                "0,{f},{p1:04X},{p2:04X},3,{},{}\n",
                8_000 + (x >> 20) % 4_000,
                (x >> 3) % 700
            ));
        }
        s
    }

    fn gz_all(text: &str, interval: Duration) -> Vec<u8> {
        let mut w = GzLogWriter::with_sync_interval(Vec::new(), interval);
        w.write_all(text.as_bytes()).unwrap();
        w.finish().unwrap()
    }

    #[test]
    fn round_trip_equals_plain() {
        let text = net_rows(5_000);
        let gz = gz_all(&text, SYNC_INTERVAL);
        assert!(is_gzip(&gz));
        let back = text_from_bytes(gz).unwrap();
        assert_eq!(back.text, text);
        assert!(!back.cut);
        assert_eq!(back.cut_note("x"), None);
    }

    #[test]
    fn plain_text_passes_through() {
        let text = net_rows(10);
        let back = text_from_bytes(text.clone().into_bytes()).unwrap();
        assert_eq!(back, LogText { text, cut: false });
        assert_eq!(text_from_bytes(Vec::new()).unwrap().text, "");
    }

    #[test]
    fn killed_stream_keeps_rows_up_to_the_last_sync_flush() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Shared(Rc<RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut w = GzLogWriter::with_sync_interval(Shared(Rc::clone(&sink)), Duration::ZERO);
        let first = net_rows(300);
        w.write_all(first.as_bytes()).unwrap();
        w.flush().unwrap();
        // flush の時点の byte 列 = 強制終了で残るファイル。
        let killed = sink.borrow().clone();
        // flush の後にまだ書く（killed には入らない）。
        w.write_all(b"0,9999,0000,0000,3,,0\n").unwrap();
        drop(w);
        let ended = sink.borrow().clone();
        assert!(ended.len() > killed.len());
        let back = text_from_bytes(killed.clone()).unwrap();
        assert!(back.cut, "終わりの印が無いのに cut = false");
        assert_eq!(back.text, first, "sync flush までの行が全部読めない");
        assert!(back.cut_note("x.csv.gz").unwrap().contains("x.csv.gz"));
        let done = text_from_bytes(ended).unwrap();
        assert!(!done.cut);
        assert!(done.text.ends_with("0,9999,0000,0000,3,,0\n"));
    }

    #[test]
    fn cut_in_the_middle_of_a_row_drops_only_that_row() {
        let text = net_rows(2_000);
        let gz = gz_all(&text, Duration::ZERO);
        // 終わりの印 8 B を落とした上で、さらに 30 B 削る（deflate の途中で切れる）。
        let cut = gz[..gz.len() - 8 - 30].to_vec();
        let back = text_from_bytes(cut).unwrap();
        assert!(back.cut);
        assert!(back.text.ends_with('\n'), "半端な行が残った");
        assert!(text.starts_with(&back.text));
        let (kept, all) = (back.text.lines().count(), text.lines().count());
        assert!(kept < all && kept > all - 100, "{kept} / {all}");
        for n in [0usize, 1, 2, 5, 10, 11] {
            let t = text_from_bytes(gz[..n].to_vec()).unwrap();
            assert_eq!(t.text, "", "{n} B");
        }
    }

    #[test]
    fn flush_is_rate_limited() {
        let rows = net_rows(200);
        let size_with = |interval: Duration| {
            let mut w = GzLogWriter::with_sync_interval(Vec::new(), interval);
            for line in rows.lines() {
                w.write_all(line.as_bytes()).unwrap();
                w.write_all(b"\n").unwrap();
                w.flush().unwrap();
            }
            w.finish().unwrap().len()
        };
        let (never, always) = (
            size_with(Duration::from_secs(3600)),
            size_with(Duration::ZERO),
        );
        assert!(
            always > never,
            "毎回の sync flush が増えない: {always} <= {never}"
        );
    }

    #[test]
    fn sync_flush_costs_little() {
        let text = net_rows(60 * 60 * 10);
        let size = |every: usize| {
            let mut w = GzLogWriter::with_sync_interval(Vec::new(), Duration::ZERO);
            for (i, line) in text.lines().enumerate() {
                w.write_all(line.as_bytes()).unwrap();
                w.write_all(b"\n").unwrap();
                if every != 0 && i % every == every - 1 {
                    w.flush().unwrap();
                }
            }
            w.finish().unwrap().len()
        };
        let (none, two_s, one_s) = (size(0), size(120), size(60));
        eprintln!(
            "sync flush なし {none} B / 2 秒ごと {two_s} B（{:+.2}%）/ 1 秒ごと {one_s} B（{:+.2}%）",
            (two_s as f64 / none as f64 - 1.0) * 100.0,
            (one_s as f64 / none as f64 - 1.0) * 100.0
        );
        assert!(one_s as f64 <= none as f64 * 1.08, "{one_s} / {none}");
    }

    #[test]
    fn synthetic_rows_shrink() {
        let text = net_rows(60 * 60 * 60);
        let gz = gz_all(&text, SYNC_INTERVAL);
        let r = gz.len() as f64 / text.len() as f64;
        eprintln!(
            "合成 _net_frames.csv: {} B → gzip {} B（{:.1}%。{} 行）",
            text.len(),
            gz.len(),
            r * 100.0,
            text.lines().count()
        );
        assert!(r < 0.5, "{r}");
    }

    #[test]
    fn read_log_text_reads_both_files() {
        let d = std::env::temp_dir().join(format!("higanr_gzlog_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let text = net_rows(100);
        std::fs::write(d.join("a.csv"), &text).unwrap();
        std::fs::write(d.join("a.csv.gz"), gz_all(&text, SYNC_INTERVAL)).unwrap();
        for n in ["a.csv", "a.csv.gz"] {
            assert_eq!(read_log_text(&d.join(n)).unwrap().text, text, "{n}");
        }
        assert!(read_log_text(&d.join("無い.csv.gz")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
