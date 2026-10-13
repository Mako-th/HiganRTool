//! 突き合わせの入力の CSV を読む。平文の `.csv` と dll が書く `.csv.gz` を、先頭の 2 B で見分ける。
//!
//! gzip が終わりの印を失っていても、最後の sync flush までの行を読み、標準エラーに 1 行出す
//! （切れた CSV どうしの比べは「最後が違う」ように見えうるため）。

use std::path::Path;

use higanr_core::gzlog;

/// 読めなければ `<パス> が読めない: …`。gzip が切れていたら、読めた所までを返して標準エラーに知らせる。
pub fn read_csv_text(path: &Path) -> Result<String, String> {
    let t =
        gzlog::read_log_text(path).map_err(|e| format!("{} が読めない: {e}", path.display()))?;
    if let Some(note) = t.cut_note(&path.display().to_string()) {
        eprintln!("{note}");
    }
    Ok(t.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::gzlog::GzLogWriter;
    use std::io::Write;

    fn gz(text: &str, finish: bool) -> Vec<u8> {
        let mut w = GzLogWriter::with_sync_interval(Vec::new(), std::time::Duration::ZERO);
        w.write_all(text.as_bytes()).unwrap();
        w.flush().unwrap();
        if finish {
            w.finish().unwrap()
        } else {
            // `finish` でしか取れないので、終わりの 8 B（CRC32 ＋ 長さ）を落として切れを真似る。
            let mut v = w.finish().unwrap();
            v.truncate(v.len() - 8);
            v
        }
    }

    #[test]
    fn reads_plain_gzip_and_cut_gzip_into_the_same_rows() {
        let d = std::env::temp_dir().join(format!("peer_sim_csv_input_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut text = format!("{}\n", higanr_core::replay_compare::NET_FRAMES_HEADER);
        for f in 0..50 {
            text.push_str(&format!("0,{f},{:04X},{:04X},3,,0,0\n", f * 3, f * 5));
        }
        std::fs::write(d.join("a.csv"), &text).unwrap();
        std::fs::write(d.join("a.csv.gz"), gz(&text, true)).unwrap();
        std::fs::write(d.join("cut.csv.gz"), gz(&text, false)).unwrap();
        let rows = |n: &str| crate::compare::parse_dll_csv(&read_csv_text(&d.join(n)).unwrap());
        let plain = rows("a.csv").unwrap();
        assert_eq!(plain.len(), 50);
        assert_eq!(rows("a.csv.gz").unwrap(), plain);
        assert_eq!(
            rows("cut.csv.gz").unwrap(),
            plain,
            "切れた gzip の行を落とした"
        );
        let e = read_csv_text(&d.join("無い.csv.gz")).unwrap_err();
        assert!(e.contains("が読めない"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
