//! 不具合の報告先（公開用リポジトリのIssues）。
//! 画面の文にURLを書く側（ローダー・DLL）が文に含めたURLとこの定数が合っているかどうかを検査する。

pub const ISSUES_URL: &str = "https://github.com/Mako-th/HiganRTool/issues";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_points_at_the_public_repository_issues() {
        assert_eq!(ISSUES_URL, "https://github.com/Mako-th/HiganRTool/issues");
        assert!(
            !ISSUES_URL.contains("-dev"),
            "開発用のリポジトリを指している"
        );
    }
}
