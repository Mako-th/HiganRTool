//! OSのUIの言語の読み取り（`GetUserDefaultUILanguage`。unsafeはこの中だけ）。画面の言語の既定を決めるため起動時に1回だけ呼ぶ。
//!
//! 読むだけ。設定にも入力にも触らない。

/// 今のユーザーのUIの言語（LANGID）。
pub fn os_ui_langid() -> u16 {
    // SAFETY: 引数も出力用のポインタも無い。ユーザーの設定を読んでLANGIDを返すだけ（失敗すると0が返ると推定）。
    unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_os_language_call_returns_a_nonzero_langid() {
        // 値は環境によって違う。0は言語が無い値（失敗と推定）なので、呼べたことだけを見る。
        assert_ne!(os_ui_langid(), 0);
    }
}
