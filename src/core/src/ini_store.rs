//! `HiganRTool.ini` の読み書き（ファイルの出し入れ。行の編集そのものは `ini_edit`）。
//!
//! - 設定の原本は ini 1 つ。メニューで決めた値は決まった時点で該当の 1 行を書き換える（`update`）。
//! - 書くのは一時ファイル（`<ini>.tmp`）へ書いてから置き換える（途中で落ちても ini が壊れない）。ini が無いときは作らない。
//! - 読み書きは 1 本の鍵の中で行う。名前・ルーム名・同期方式・言語・ゲームのパスは別々のスレッドが頼むので、
//!   読んで書く間に別の頼みが入って、片方の書き換えが消えないようにする。
//! - 以前の版が別ファイルに覚えていた値（`game_path.txt` など）は `migrate` が ini へ移す。
#![forbid(unsafe_code)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use encoding_rs::SHIFT_JIS;

use crate::ini::{self, Ini};
use crate::ini_edit::{self, Edit};

/// ini を読む大きさの上限（B）。配る ini は 3 KB ほど。
const INI_MAX: u64 = 1 << 20;
/// 引き継ぐファイルを読む大きさの上限（B）。中身は名前かパスの 1 行。
const LEGACY_MAX: u64 = 4096;

static LOCK: Mutex<()> = Mutex::new(());

/// 配る ini の原本（zip に入るものと同じファイル。コピーは作らない）。
const DIST_INI: &str = include_str!("../../../scripts/dist/HiganRTool.ini");

/// 配る ini を書き出すときの中身。`tools/make_dist.py` が zip に入れるときと同じ改行（CRLF。古いメモ帳で 1 行に潰れないように）。
pub fn dist_ini_text() -> String {
    DIST_INI.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    // 鍵を持つ者が落ちても、中身はファイルなので次の書きでやり直せる。
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>, std::io::Error> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::other("大きすぎる"));
    }
    Ok(bytes)
}

/// UTF-8 でなければ Shift_JIS として読む（dll が ini を読むのと同じ順）。どちらでもなければ `None`。
pub fn decode(bytes: &[u8]) -> Option<String> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s.to_string()),
        Err(_) => SHIFT_JIS
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(|c| c.into_owned()),
    }
}

/// ini が無いときは `initial`（あれば）を元の中身にする。
fn read_or_initial(path: &Path, initial: Option<&str>) -> Result<Vec<u8>, String> {
    match (read_capped(path, INI_MAX), initial) {
        (Err(e), Some(text)) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(text.as_bytes().to_vec())
        }
        (r, _) => r.map_err(|e| format!("{} を読めない: {e}", path.display())),
    }
}

/// 読める行だけを読んだ ini。無い・読めないなら `None`。
pub fn read(path: &Path) -> Option<Ini> {
    let text = decode(&read_capped(path, INI_MAX).ok()?)?;
    Some(ini::parse_lenient(&text).ini)
}

/// `[section] key` の値。無い・ini が読めない・空なら `None`。
pub fn get(path: &Path, section: &str, key: &str) -> Option<String> {
    read(path)?
        .get(section, key)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn write_replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let tmp = PathBuf::from(name);
    std::fs::write(&tmp, bytes).map_err(|e| format!("{} を書けない: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{} へ置き換えられない: {e}", path.display())
    })
}

fn update_locked(path: &Path, initial: Option<&str>, edits: &[Edit<'_>]) -> Result<bool, String> {
    let old = read_or_initial(path, initial)?;
    let new = ini_edit::apply(&old, edits).map_err(|e| format!("{}: {e}", path.display()))?;
    if new == old {
        return Ok(false);
    }
    write_replace(path, &new)?;
    Ok(true)
}

/// `edits` を ini に当てる。変えたら `Ok(true)`、中身が同じなら書かず `Ok(false)`。ini が無い・断られたときは何も書かない。
/// 誤りの文は値を含めない（名前・ルーム名・ゲームのパスをログに残さない）。
pub fn update(path: &Path, edits: &[Edit<'_>]) -> Result<bool, String> {
    let _g = lock();
    update_locked(path, None, edits)
}

/// `update` に加えて、ini が無ければ `initial` を元に作る（ローダーが、DLL が既定の ini を書き出す前に書くとき。
/// 設定の一部だけの ini を作ると DLL が既定を書き出さなくなるので、配るバージョンは `dist_ini_text()` を渡す）。
pub fn update_creating(path: &Path, initial: &str, edits: &[Edit<'_>]) -> Result<bool, String> {
    let _g = lock();
    update_locked(path, Some(initial), edits)
}

/// `is_latest` が真のときだけ書く。確かめるのは鍵の中なので、確かめた後に別の頼みが入って古い値で上書きする隙間が無い。
/// 偽なら何も書かず `Ok(false)`（もっと新しい頼みがある）。
pub fn update_if(
    path: &Path,
    edits: &[Edit<'_>],
    is_latest: impl FnOnce() -> bool,
) -> Result<bool, String> {
    let _g = lock();
    if !is_latest() {
        return Ok(false);
    }
    update_locked(path, None, edits)
}

/// ini の全文を作り直す。`make` に今の中身（UTF-8 に直したもの）を渡し、`Some(新しい中身)` で中身が変わるときだけ
/// 一時ファイル経由で置き換える。書くのは UTF-8（Shift_JIS の ini も UTF-8 になる）。ini が無い・読めないときは何も書かない。
/// 誤りの文は中身を含めない。返すのは書いたか。
pub fn rewrite(path: &Path, make: impl FnOnce(&str) -> Option<String>) -> Result<bool, String> {
    let _g = lock();
    let old =
        read_capped(path, INI_MAX).map_err(|e| format!("{} を読めない: {e}", path.display()))?;
    let text = decode(&old)
        .ok_or_else(|| format!("{} は UTF-8 でも Shift_JIS でもない", path.display()))?;
    let Some(new) = make(&text) else {
        return Ok(false);
    };
    if new.as_bytes() == old.as_slice() {
        return Ok(false);
    }
    write_replace(path, new.as_bytes())?;
    Ok(true)
}

/// 以前の版が別ファイルに覚えていた値 1 つ。
pub struct Legacy<'a> {
    pub file: PathBuf,
    pub section: &'a str,
    pub key: &'a str,
    /// ファイルの中身から ini に書く値を取り出す。使えなければ `None`。
    pub parse: fn(&str) -> Option<String>,
    /// ini に今ある値が「まだ利用者が決めていない既定のまま」か（そうなら、ファイルの値を移す）。
    pub is_default: fn(&str) -> bool,
}

/// `items` のファイルがあれば中身を ini へ移し、移せたら（ini に既に値があって移さないときも）ファイルを消す。
/// ini に空でない値が既にあれば ini を優先する（`is_default` の値は未設定と同じ）。
/// 移せなければ（中身が使えない・ini が無い・書けない）ファイルを残す。返すのは本ログ用の文（値は含めない）。
/// ini が無いとき `initial` があれば、それを元に ini を作って移す。
pub fn migrate(ini_path: &Path, initial: Option<&str>, items: &[Legacy<'_>]) -> Vec<String> {
    let mut logs = Vec::new();
    let present: Vec<&Legacy<'_>> = items
        .iter()
        .filter(|i| std::fs::symlink_metadata(&i.file).is_ok())
        .collect();
    if present.is_empty() {
        return logs;
    }
    let _g = lock();
    let mut bytes = match read_or_initial(ini_path, initial) {
        Ok(b) => b,
        Err(why) => {
            for i in &present {
                logs.push(format!(
                    "{} を ini へ移せない（{why}）。ファイルは残す",
                    i.file.display()
                ));
            }
            return logs;
        }
    };
    let orig = bytes.clone();
    let current = decode(&bytes).map(|t| ini::parse_lenient(&t).ini);
    let (mut moved, mut superseded) = (Vec::new(), Vec::new());
    for i in present {
        let shown = i.file.display();
        let value = match read_capped(&i.file, LEGACY_MAX)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| (i.parse)(&t))
        {
            Some(v) => v,
            None => {
                logs.push(format!(
                    "{shown} の中身が使えないので移せない。ファイルは残す"
                ));
                continue;
            }
        };
        let existing = current
            .as_ref()
            .and_then(|c| c.get(i.section, i.key))
            .filter(|v| !v.is_empty() && !(i.is_default)(v));
        if existing.is_some() {
            superseded.push(i);
            continue;
        }
        let edit = Edit::Set {
            section: i.section,
            key: i.key,
            value: &value,
        };
        match ini_edit::apply(&bytes, &[edit]) {
            Ok(b) => {
                bytes = b;
                moved.push(i);
            }
            Err(e) => logs.push(format!("{shown} を ini へ移せない（{e}）。ファイルは残す")),
        }
    }
    if bytes != orig
        && let Err(why) = write_replace(ini_path, &bytes)
    {
        for i in moved {
            logs.push(format!(
                "{} を ini へ移せない（{why}）。ファイルは残す",
                i.file.display()
            ));
        }
        moved = Vec::new();
    }
    for (i, how) in moved.into_iter().map(|i| (i, "ini へ移した")).chain(
        superseded
            .into_iter()
            .map(|i| (i, "ini に値があるので移さない（ini を優先）")),
    ) {
        let shown = i.file.display();
        match std::fs::remove_file(&i.file) {
            Ok(()) => logs.push(format!("{shown}: {how}。ファイルを消した")),
            Err(e) => logs.push(format!("{shown}: {how}。ファイルを消せない（{e}）")),
        }
    }
    logs
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!(
                "higanr_ini_store_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn set<'a>(section: &'a str, key: &'a str, value: &'a str) -> Edit<'a> {
        Edit::Set {
            section,
            key,
            value,
        }
    }

    fn names(t: &Tmp) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn update_rewrites_one_line_and_leaves_no_temp_file() {
        let t = Tmp::new("update");
        let p = t.file("HiganRTool.ini");
        let text = "; 注\r\n[netplay]\r\nname = a ; n\r\n";
        std::fs::write(&p, text).unwrap();
        assert_eq!(update(&p, &[set("netplay", "name", "b")]), Ok(true));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "; 注\r\n[netplay]\r\nname = b ; n\r\n"
        );
        assert_eq!(update(&p, &[set("netplay", "name", "b")]), Ok(false));
        std::fs::write(t.file("HiganRTool.ini.tmp"), "半端な").unwrap();
        assert_eq!(update(&p, &[set("netplay", "name", "c")]), Ok(true));
        assert_eq!(names(&t), vec!["HiganRTool.ini".to_string()]);
    }

    #[test]
    fn update_never_creates_the_ini_and_leaves_a_refused_one_alone() {
        let t = Tmp::new("refuse");
        let p = t.file("HiganRTool.ini");
        assert!(update(&p, &[set("a", "k", "1")]).is_err());
        assert!(!p.exists(), "無い ini を作った");
        std::fs::write(&p, "[a]\nk = 1\nk = 2\n").unwrap();
        let err = update(&p, &[set("a", "k", "secret-value")]).unwrap_err();
        assert!(!err.contains("secret-value"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[a]\nk = 1\nk = 2\n");
        std::fs::write(&p, b"\xFF\xFE[a]").unwrap();
        assert!(update(&p, &[set("a", "k", "1")]).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"\xFF\xFE[a]");
        assert_eq!(names(&t), vec!["HiganRTool.ini".to_string()]);
    }

    #[test]
    fn update_if_writes_only_while_the_request_is_the_latest() {
        let t = Tmp::new("latest");
        let p = t.file("HiganRTool.ini");
        std::fs::write(&p, "[a]\nk = 1\n").unwrap();
        assert_eq!(update_if(&p, &[set("a", "k", "2")], || false), Ok(false));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[a]\nk = 1\n");
        assert_eq!(update_if(&p, &[set("a", "k", "3")], || true), Ok(true));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[a]\nk = 3\n");
    }

    #[test]
    fn a_shift_jis_ini_is_read_and_written_as_shift_jis() {
        let t = Tmp::new("sjis");
        let p = t.file("HiganRTool.ini");
        let sjis = |s: &str| SHIFT_JIS.encode(s).0.into_owned();
        std::fs::write(
            &p,
            sjis("; 設定\r\n[loader]\r\ngame = C:\\旧\\th09.exe\r\n"),
        )
        .unwrap();
        assert_eq!(
            get(&p, "loader", "game").as_deref(),
            Some("C:\\旧\\th09.exe")
        );
        assert_eq!(
            update(&p, &[set("loader", "game", "C:\\新しい\\th09.exe")]),
            Ok(true)
        );
        assert_eq!(
            std::fs::read(&p).unwrap(),
            sjis("; 設定\r\n[loader]\r\ngame = C:\\新しい\\th09.exe\r\n")
        );
        assert!(update(&p, &[set("loader", "game", "😀")]).is_err());
        assert_eq!(
            get(&p, "loader", "game").as_deref(),
            Some("C:\\新しい\\th09.exe")
        );
    }

    #[test]
    fn get_reads_a_value_without_its_comment_and_none_when_absent() {
        let t = Tmp::new("get");
        let p = t.file("HiganRTool.ini");
        assert_eq!(get(&p, "a", "k"), None);
        std::fs::write(&p, "[a]\nk = v ; c\ne =\nbad line\n").unwrap();
        assert_eq!(get(&p, "a", "k").as_deref(), Some("v"));
        assert_eq!(get(&p, "a", "e"), None, "空の値は無いのと同じ");
        assert_eq!(get(&p, "a", "z"), None);
        assert_eq!(get(&p, "b", "k"), None);
    }

    fn first_line(text: &str) -> Option<String> {
        text.lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
    }

    fn not_hello(v: &str) -> bool {
        v == "Hello"
    }

    fn item<'a>(t: &Tmp, file: &str, key: &'a str) -> Legacy<'a> {
        Legacy {
            file: t.file(file),
            section: "s",
            key,
            parse: first_line,
            is_default: not_hello,
        }
    }

    #[test]
    fn migrate_moves_a_value_into_the_ini_and_deletes_the_file() {
        let t = Tmp::new("mig");
        let p = t.file("HiganRTool.ini");
        std::fs::write(&p, "[s]\r\nother = 1\r\n").unwrap();
        std::fs::write(t.file("a.txt"), "from-file\r\n").unwrap();
        let logs = migrate(&p, None, &[item(&t, "a.txt", "a")]);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[s]\r\nother = 1\r\na = from-file\r\n"
        );
        assert!(!t.file("a.txt").exists());
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert!(!logs[0].contains("from-file"), "値をログに書いた: {logs:?}");
    }

    #[test]
    fn migrate_prefers_the_ini_and_still_deletes_the_file() {
        let t = Tmp::new("prefer");
        let p = t.file("HiganRTool.ini");
        std::fs::write(&p, "[s]\na = in-ini\nb = Hello\nc =\n").unwrap();
        for k in ["a", "b", "c"] {
            std::fs::write(t.file(&format!("{k}.txt")), "from-file\n").unwrap();
        }
        let items: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|k| item(&t, &format!("{k}.txt"), k))
            .collect();
        migrate(&p, None, &items);
        let ini = read(&p).unwrap();
        assert_eq!(ini.get("s", "a"), Some("in-ini"), "ini の値を上書きした");
        assert_eq!(
            ini.get("s", "b"),
            Some("from-file"),
            "既定のままの値は未設定と同じ"
        );
        assert_eq!(ini.get("s", "c"), Some("from-file"), "空の値は未設定と同じ");
        assert_eq!(names(&t), vec!["HiganRTool.ini".to_string()]);
    }

    #[test]
    fn migrate_keeps_a_file_it_cannot_move() {
        let t = Tmp::new("keep");
        let p = t.file("HiganRTool.ini");
        std::fs::write(&p, "[s]\nx = 1\n").unwrap();
        std::fs::write(t.file("empty.txt"), "\n").unwrap();
        std::fs::write(t.file("bad.txt"), "a ; b\n").unwrap();
        std::fs::write(t.file("sjis.txt"), b"\x88\xA4\n").unwrap();
        std::fs::write(t.file("ok.txt"), "fine\n").unwrap();
        let items: Vec<_> = ["empty", "bad", "sjis", "ok"]
            .iter()
            .map(|k| item(&t, &format!("{k}.txt"), k))
            .collect();
        let logs = migrate(&p, None, &items);
        assert_eq!(
            names(&t),
            vec![
                "HiganRTool.ini".to_string(),
                "bad.txt".to_string(),
                "empty.txt".to_string(),
                "sjis.txt".to_string()
            ]
        );
        assert_eq!(logs.len(), 4, "{logs:?}");
        let ini = read(&p).unwrap();
        assert_eq!(ini.get("s", "ok"), Some("fine"));
        assert_eq!(ini.get("s", "bad"), None);
    }

    #[test]
    fn migrate_keeps_everything_when_the_ini_is_missing_or_unwritable() {
        let t = Tmp::new("noini");
        std::fs::write(t.file("a.txt"), "v\n").unwrap();
        let p = t.file("HiganRTool.ini");
        let logs = migrate(&p, None, &[item(&t, "a.txt", "a")]);
        assert!(t.file("a.txt").exists() && !p.exists());
        assert_eq!(logs.len(), 1);
        std::fs::write(&p, "[s]\nk = 1\nk = 2\n[s]\n").unwrap();
        let before = std::fs::read(&p).unwrap();
        let mut dup = item(&t, "a.txt", "k");
        dup.is_default = |_| true;
        migrate(&p, None, &[dup]);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            before,
            "重複のある ini を書いた"
        );
        assert!(t.file("a.txt").exists());
    }

    #[test]
    fn a_missing_ini_is_created_from_the_initial_text_only_when_asked() {
        let t = Tmp::new("create");
        let p = t.file("HiganRTool.ini");
        std::fs::write(t.file("a.txt"), "from-file\n").unwrap();
        let initial = "; 既定\r\n[s]\r\nx = 1\r\n";
        let logs = migrate(&p, Some(initial), &[item(&t, "a.txt", "a")]);
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "; 既定\r\n[s]\r\nx = 1\r\na = from-file\r\n"
        );
        assert!(!t.file("a.txt").exists());
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            update_creating(&p, initial, &[set("s", "k", "v")]),
            Ok(true)
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "; 既定\r\n[s]\r\nx = 1\r\nk = v\r\n"
        );
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            update_creating(&p, "", &[set("loader", "game", "G")]),
            Ok(true)
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[loader]\r\ngame = G\r\n"
        );
        // 既にある ini は initial で置き換えない。
        std::fs::write(&p, "[s]\nx = 2\n").unwrap();
        update_creating(&p, initial, &[set("s", "x", "3")]).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[s]\nx = 3\n");
    }

    #[test]
    fn the_dist_ini_text_is_the_shipped_file_with_crlf() {
        let t = dist_ini_text();
        assert!(t.contains("[netplay]\r\n") && !t.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn migrate_and_update_keep_everything_when_the_ini_cannot_be_written() {
        let t = Tmp::new("nowrite");
        let p = t.file("HiganRTool.ini");
        std::fs::write(&p, "[s]\nx = 1\n").unwrap();
        // 一時ファイルの名前がフォルダで塞がっていて、書き込みが失敗する。
        std::fs::create_dir(t.file("HiganRTool.ini.tmp")).unwrap();
        std::fs::write(t.file("a.txt"), "v\n").unwrap();
        let logs = migrate(&p, None, &[item(&t, "a.txt", "a")]);
        assert!(t.file("a.txt").exists(), "移せていないのにファイルを消した");
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert!(logs[0].contains("移せない"), "{logs:?}");
        assert!(update(&p, &[set("s", "x", "2")]).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[s]\nx = 1\n");
    }

    #[test]
    fn migrate_with_no_files_does_nothing_even_without_an_ini() {
        let t = Tmp::new("nothing");
        let p = t.file("HiganRTool.ini");
        assert!(migrate(&p, Some("x"), &[item(&t, "a.txt", "a")]).is_empty());
        assert!(!p.exists());
    }

    #[test]
    fn two_threads_updating_different_keys_both_land() {
        let t = Tmp::new("race");
        let p = t.file("HiganRTool.ini");
        std::fs::write(&p, "[s]\n").unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let hs: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|k| {
                let (p, start) = (p.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for n in 0..100 {
                        let v = n.to_string();
                        update(&p, &[set("s", k, &v)]).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let ini = read(&p).unwrap();
        assert_eq!(ini.get("s", "a"), Some("99"));
        assert_eq!(ini.get("s", "b"), Some("99"));
        assert_eq!(names(&t), vec!["HiganRTool.ini".to_string()]);
    }
}
