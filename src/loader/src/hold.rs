//! 断って終わるときに黒いウィンドウをすぐ閉じずEnterまで残すかどうか。
//! ショートカット等で起動したウィンドウが一瞬で閉じると断った理由の1行が読めないため。
//!
//! 残すのは終了コードが0でなく、かつウィンドウがこのexeだけのものとき。
//! コマンドプロンプトや検査のツールから起動したときは親のウィンドウに理由が残るので待たない（検査が止まらない）。

use higanr_core::lang::Tx;

/// `console_procs`はウィンドウに付いているプロセスの数（取れなければNone）。
pub fn should_hold(exit_code: i32, console_procs: Option<u32>) -> bool {
    exit_code != 0 && console_procs == Some(1)
}

pub const HOLD_TEXT: Tx = Tx {
    ja: "HiganRTool: 上の理由の行と、zipにしたlogsフォルダを添えて、開発者に伝えてください（https://github.com/Mako-th/HiganRTool/issues）。Enterキーで閉じます。",
    en: "HiganRTool: Please report this on GitHub Issues (https://github.com/Mako-th/HiganRTool/issues) with the reason lines above and the logs folder as a zip. Press Enter to close.",
};

/// 同じth09.exeが動いているため何も起動しなかったときの終了コード（`single`）。故障ではないので報告を頼む`HOLD_TEXT`は出さない。
pub const ALREADY_RUNNING_EXIT_CODE: i32 = 3;

pub const CLOSE_ONLY_TEXT: Tx = Tx {
    ja: "Enterキーで閉じます。",
    en: "Press Enter to close.",
};

/// 待つ場合に出す文。
pub fn hold_text(exit_code: i32) -> Tx {
    if exit_code == ALREADY_RUNNING_EXIT_CODE {
        CLOSE_ONLY_TEXT
    } else {
        HOLD_TEXT
    }
}

pub const PANIC_HOLD_TEXT: Tx = Tx {
    ja: "HiganRTool: 予期しないエラーで止まりました。zipにしたlogsフォルダを添えて、開発者に伝えてください（https://github.com/Mako-th/HiganRTool/issues）。Enterキーで閉じます。",
    en: "HiganRTool: Stopped by an unexpected error. Please report this on GitHub Issues (https://github.com/Mako-th/HiganRTool/issues) with the logs folder as a zip. Press Enter to close.",
};

/// 主スレッドがパニックしたとき（`crash_log::install_panic_hook`）は、ウィンドウが消える前に読めるようEnterまで待つ。
/// 待つ条件は断って終わるときと同じ（このexeだけの黒いウィンドウのとき）。
pub fn wait_after_panic() {
    if should_hold(PANIC_EXIT_CODE, crate::real::console_process_count()) {
        eprintln!("{}", PANIC_HOLD_TEXT.get(crate::crash_log::lang()));
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}

/// パニックで終わるときの終了コード（Rustの既定）。
const PANIC_EXIT_CODE: i32 = 101;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panic_text_names_the_file_to_send_in_both_languages() {
        for t in [PANIC_HOLD_TEXT.ja, PANIC_HOLD_TEXT.en] {
            assert!(t.contains("zip") && t.contains("logs"), "{t}");
        }
        for t in [
            HOLD_TEXT.ja,
            HOLD_TEXT.en,
            PANIC_HOLD_TEXT.ja,
            PANIC_HOLD_TEXT.en,
        ] {
            assert!(t.contains(higanr_core::issues::ISSUES_URL), "{t}");
            assert!(!t.contains("渡して") && !t.contains("gave you"), "{t}");
        }
        assert!(!higanr_core::lang::has_japanese(PANIC_HOLD_TEXT.en));
        // パニックは失敗なのでウィンドウを持つときは待つ。
        assert!(should_hold(PANIC_EXIT_CODE, Some(1)));
        assert!(!should_hold(PANIC_EXIT_CODE, Some(2)));
    }

    #[test]
    fn a_refusal_for_a_running_game_holds_without_asking_for_a_report() {
        assert!(should_hold(ALREADY_RUNNING_EXIT_CODE, Some(1)));
        for lang in [higanr_core::lang::Lang::Ja, higanr_core::lang::Lang::En] {
            let t = hold_text(ALREADY_RUNNING_EXIT_CODE).get(lang);
            assert!(!t.contains("Issues") && t.contains("Enter"), "{t}");
            assert_eq!(hold_text(1).get(lang), HOLD_TEXT.get(lang));
        }
        assert!(!higanr_core::lang::has_japanese(CLOSE_ONLY_TEXT.en));
    }

    #[test]
    fn holds_only_a_failed_run_that_owns_its_window() {
        assert!(should_hold(1, Some(1)));
        assert!(should_hold(2, Some(1)));
        assert!(!should_hold(0, Some(1)), "成功で残した");
        assert!(
            !should_hold(1, Some(2)),
            "コマンドプロンプトから起こしたのに待った"
        );
        assert!(!should_hold(1, Some(3)));
        assert!(!should_hold(1, None), "数が取れないのに待った");
    }
}
