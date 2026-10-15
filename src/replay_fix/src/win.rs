//! Windows 専用の部分: 起動の入口・窓・標準のダイアログ。

mod dialogs;
mod ui;

use std::ffi::OsString;
use std::path::Path;

use higanr_core::lang::Lang;
use higanrtool_replay_fix::cli::{self, Command, EXIT_USAGE};
use higanrtool_replay_fix::text::Text;
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

fn os_lang() -> Lang {
    // SAFETY: 引数の無い読み取りだけの関数。
    Lang::from_langid(unsafe { GetUserDefaultUILanguage() })
}

fn message_box(text: &str, caption: &str) {
    let t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let c: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: 2 つの文字列とも終端 0 つきで呼びの間生きている。親の窓は無し（null）。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            t.as_ptr(),
            c.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

fn write_log(path: &Path, text: &str) {
    // 窓を出さない実行なので、書けなくても終了コードだけは正しく返す。
    let _ = std::fs::write(path, text);
}

/// 終了コードを返す。
pub fn main() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match cli::parse(args) {
        Ok(Command::Gui { paths, lang }) => {
            let lang = lang.unwrap_or_else(os_lang);
            match ui::run(lang, &paths) {
                Ok(()) => 0,
                Err(e) => {
                    message_box(&e, Text(lang).title());
                    1
                }
            }
        }
        Ok(Command::Batch {
            pair,
            paths,
            log,
            lang,
        }) => {
            let (code, text) = cli::run_batch(pair, &paths, lang.unwrap_or_else(os_lang));
            if let Some(log) = log {
                write_log(&log, &text);
            }
            code
        }
        Err(e) => {
            let lang = os_lang();
            let t = Text(lang);
            let head = if e.0.is_empty() {
                String::new()
            } else {
                format!("{}\n\n", e.0)
            };
            message_box(&format!("{head}{}", t.usage()), t.title());
            EXIT_USAGE
        }
    }
}
