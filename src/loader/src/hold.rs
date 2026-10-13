//! 断って終わるときに、黒いウィンドウをすぐ閉じず Enter まで残すか。
//! ショートカット等で起こしたウィンドウが一瞬で閉じると、断った理由の 1 行が読めないため。
//!
//! 残すのは終了コードが 0 でなく、かつウィンドウがこの exe だけのものとき。コマンドプロンプトや検査の道具から起こしたときは
//! 親のウィンドウに理由が残るので待たない（検査が止まらない）。

use higanr_core::lang::Tx;

/// `console_procs` はウィンドウに付いているプロセスの数（取れなければ None）。
pub fn should_hold(exit_code: i32, console_procs: Option<u32>) -> bool {
    exit_code != 0 && console_procs == Some(1)
}

pub const HOLD_TEXT: Tx = Tx {
    ja: "HiganRTool: 上の理由の行を、この道具を渡してくれた人に送ってください。Enter キーで閉じます。",
    en: "HiganRTool: Send the reason lines above to the person who gave you this tool. Press Enter to close.",
};

#[cfg(test)]
mod tests {
    use super::*;

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
