//! 画面の言語（日本語・英語）の今の値と、メニューで選んだ値を ini へ書き戻す処理（unsafe 無し）。
//!
//! - 今の値は 1 か所の原子変数。ゲームのスレッドが書き（メニューで替えたとき）、見張り・ウィンドウの文を書くスレッドも読む。
//!   決め方は起動のとき（`session::load_net`）に 1 回: ini の `[ui] lang` → `auto` なら OS の UI の言語（`higanr_core::lang::resolve`）。
//! - メニューで替えたら、起動のときに読んだのと同じ ini の `lang` の行を別のスレッド（`script_win::save_ui_lang`）が書き換える。
//!   ゲームのスレッドはファイルに触らない。一時ファイルへ書いてから置き換える。ini 以外のファイルは書かない。
//! - 本ログ・CSV は言語を切り替えない（日本語のまま）。
//! - 検査では今の値をスレッドごとに持つ（並列の検査が互いの言語を書き換えない）。

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use higanr_core::lang::{Lang, set_lang_in_ini_text};

/// 言語の番号（`Lang::index`）を持つ変数。
struct LangCell(AtomicU32);

impl LangCell {
    const fn new() -> LangCell {
        LangCell(AtomicU32::new(0))
    }

    fn get(&self) -> Lang {
        Lang::from_index(self.0.load(Ordering::Relaxed) as u8)
    }

    fn set(&self, l: Lang) {
        self.0.store(l.index() as u32, Ordering::Relaxed);
    }
}

#[cfg(not(test))]
static CURRENT: LangCell = LangCell::new();

#[cfg(test)]
thread_local! {
    static CURRENT: LangCell = const { LangCell::new() };
}

/// 今の言語。起動のあいだは日本語（`set` が呼ばれるまで）。確保しない。
pub fn current() -> Lang {
    #[cfg(not(test))]
    {
        CURRENT.get()
    }
    #[cfg(test)]
    {
        CURRENT.with(LangCell::get)
    }
}

/// 今の言語を置く（起動のときの読み取りのスレッドと、メニューで替えたゲームのスレッド）。確保しない。
pub fn set(l: Lang) {
    #[cfg(not(test))]
    CURRENT.set(l);
    #[cfg(test)]
    CURRENT.with(|c| c.set(l));
}

/// ini の `[ui] lang` を `lang` に書き換える（読む → `set_lang_in_ini_text` → 一時ファイルへ書いて置き換える）。
/// 中身が変わらなければ書かない。`path` が無い（ini なしで走っている）なら書かずに誤りを返す。
pub fn save_to_ini(path: Option<&Path>, lang: Lang) -> Result<(), String> {
    let path = path.ok_or("ini の置き場が分からない（ini なしの実行）ので言語を書かない")?;
    let text = std::fs::read(path)
        .map_err(|e| format!("{} を読めない: {e}", path.display()))
        .and_then(|b| {
            String::from_utf8(b).map_err(|_| format!("{} が UTF-8 でない", path.display()))
        })?;
    let new = set_lang_in_ini_text(&text, lang);
    if new == text {
        return Ok(());
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    std::fs::write(&tmp, new).map_err(|e| format!("{} を書けない: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{} へ置き換えられない: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!(
                "higanr_ui_lang_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const INI: &str = "[netplay]\r\nenable = 1\r\n\r\n[ui]\r\n; 言語\r\nlang = auto\r\n";

    #[test]
    fn saving_rewrites_only_the_lang_line_and_leaves_no_temp_file() {
        let t = Tmp::new("save");
        let ini = t.0.join("HiganRTool.ini");
        std::fs::write(&ini, INI).unwrap();
        save_to_ini(Some(&ini), Lang::En).unwrap();
        assert_eq!(
            std::fs::read_to_string(&ini).unwrap(),
            INI.replace("lang = auto", "lang = en")
        );
        save_to_ini(Some(&ini), Lang::Ja).unwrap();
        assert_eq!(
            std::fs::read_to_string(&ini).unwrap(),
            INI.replace("lang = auto", "lang = ja"),
            "古い言語が残った"
        );
        let names: Vec<String> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["HiganRTool.ini".to_string()],
            "一時ファイルが残った"
        );
        // 前の失敗で残った一時ファイルがあっても置き換えられる。
        std::fs::write(t.0.join("HiganRTool.ini.tmp"), "半端な").unwrap();
        save_to_ini(Some(&ini), Lang::En).unwrap();
        assert!(std::fs::read_to_string(&ini).unwrap().contains("lang = en"));
        assert!(!t.0.join("HiganRTool.ini.tmp").exists());
    }

    #[test]
    fn a_written_language_is_read_back_the_way_the_start_reads_it() {
        let t = Tmp::new("roundtrip");
        let ini = t.0.join("HiganRTool.ini");
        std::fs::write(&ini, "[netplay]\nenable = 1\n").unwrap();
        for l in Lang::ALL {
            save_to_ini(Some(&ini), l).unwrap();
            let text = std::fs::read_to_string(&ini).unwrap();
            let parsed = higanr_core::ini::parse(&text).unwrap();
            assert_eq!(
                higanr_core::lang::setting_in_ini(&parsed),
                Ok(higanr_core::lang::LangSetting::Fixed(l))
            );
        }
    }

    #[test]
    fn without_a_readable_utf8_ini_nothing_is_written() {
        assert!(save_to_ini(None, Lang::En).is_err());
        let t = Tmp::new("bad");
        let missing = t.0.join("HiganRTool.ini");
        assert!(save_to_ini(Some(&missing), Lang::En).is_err());
        assert!(!missing.exists(), "無い ini を作った");
        std::fs::write(&missing, b"\xFF\xFE[ui]").unwrap();
        assert!(save_to_ini(Some(&missing), Lang::En).is_err());
        assert_eq!(std::fs::read(&missing).unwrap(), b"\xFF\xFE[ui]");
        let dirs: Vec<_> = std::fs::read_dir(&t.0).unwrap().collect();
        assert_eq!(dirs.len(), 1, "一時ファイルが残った");
    }

    #[test]
    fn the_current_language_is_one_value_shared_by_threads() {
        // 検査では thread_local だが、変数そのものは別のスレッドから読める（本番は静的な 1 つ）。
        let cell = std::sync::Arc::new(LangCell::new());
        assert_eq!(cell.get(), Lang::Ja, "起動のあいだは日本語");
        let c2 = std::sync::Arc::clone(&cell);
        std::thread::spawn(move || c2.set(Lang::En)).join().unwrap();
        assert_eq!(cell.get(), Lang::En);
    }

    #[test]
    fn set_and_current_round_trip_on_this_thread() {
        assert_eq!(current(), Lang::Ja);
        set(Lang::En);
        assert_eq!(current(), Lang::En);
        set(Lang::Ja);
        assert_eq!(current(), Lang::Ja);
    }
}
