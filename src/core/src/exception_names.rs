//! 例外コードの名前の表。DLL の例外ハンドラ（落ちうるコードだけを記録する）とローダーの未処理例外のフィルターが、
//! 同じコードを同じ名前でログに書く。値は Windows SDK の ntstatus.h / winnt.h の NTSTATUS。

pub const ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const IN_PAGE_ERROR: u32 = 0xC000_0006;
pub const STACK_OVERFLOW: u32 = 0xC000_00FD;
/// デバッガに渡す通知で、落ちる例外ではない。デバッガが無いまま未処理になった場合だけローダーのフィルターが書く。
pub const BREAKPOINT: u32 = 0x8000_0003;

/// 記録する（プロセスが落ちうる）例外コードの名前。C++ の例外・デバッガへの通知・スレッドの名付けなどは None。
pub fn fatal_name(code: u32) -> Option<&'static str> {
    Some(match code {
        ACCESS_VIOLATION => "アクセス違反",
        IN_PAGE_ERROR => "ページの読み込みの失敗",
        0xC000_001D => "不正な命令",
        0xC000_0096 => "特権命令",
        0xC000_0094 => "整数の 0 除算",
        0xC000_0095 => "整数のオーバーフロー",
        0xC000_008C => "配列の範囲外",
        STACK_OVERFLOW => "スタックオーバーフロー",
        0xC000_0409 => "スタックバッファオーバーラン",
        0xC000_0374 => "ヒープの破損",
        0xC000_0025 => "続行できない例外",
        0xC000_0026 => "不正な処理の指定",
        _ => return None,
    })
}

/// `fatal_name` に、未処理で届きうるブレークポイントを足した表。
pub fn name(code: u32) -> Option<&'static str> {
    match code {
        BREAKPOINT => Some("ブレークポイント"),
        _ => fatal_name(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 壊れたら落ちる: 落ちうるコードの取りこぼし、または普段から投げられるコードを記録の対象にする。
    #[test]
    fn only_codes_that_can_kill_the_process_are_fatal() {
        for code in [
            0xC000_0005,
            0xC000_0006,
            0xC000_001D,
            0xC000_0094,
            0xC000_00FD,
            0xC000_0409,
            0xC000_0374,
        ] {
            assert!(fatal_name(code).is_some(), "{code:08X}");
        }
        for code in [
            0xE06D_7363, // C++ の例外（Rust のパニックも）
            BREAKPOINT,
            0x4001_0006, // OutputDebugString
            0x406D_1388, // スレッドの名付け
            0x8000_0001, // ガードページ
            0xC000_008F, // 浮動小数点（不正確な結果）
        ] {
            assert!(fatal_name(code).is_none(), "{code:08X}");
        }
    }

    #[test]
    fn the_breakpoint_has_a_name_but_is_not_fatal() {
        assert_eq!(name(BREAKPOINT), Some("ブレークポイント"));
        assert_eq!(fatal_name(BREAKPOINT), None);
        assert_eq!(name(0xE06D_7363), None);
    }

    #[test]
    fn every_fatal_name_is_the_same_through_both_lookups() {
        let mut seen = 0;
        for code in 0xC000_0000u32..0xC000_0500 {
            if let Some(n) = fatal_name(code) {
                assert_eq!(name(code), Some(n));
                seen += 1;
            }
        }
        assert_eq!(seen, 12, "表の 12 コード");
    }
}
