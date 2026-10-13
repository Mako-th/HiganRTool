//! OS の UI の言語の読み（`GetUserDefaultUILanguage`。unsafe はこの中だけ）。画面の言語の既定を決めるときに、起動のとき 1 回だけ呼ぶ。
//!
//! 読むだけで、設定にも入力にも触らない。

/// 今のユーザーの UI の言語（LANGID）。
pub fn os_ui_langid() -> u16 {
    // SAFETY: 引数も出力用のポインタも無い。ユーザーの設定を読んで LANGID を返すだけ（失敗すると 0 が返ると推定）。
    unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_os_language_call_returns_a_nonzero_langid() {
        // 値は機械によって違う。0 は言語が無い値（失敗と推定）なので、呼べたことだけを見る。
        assert_ne!(os_ui_langid(), 0);
    }
}
