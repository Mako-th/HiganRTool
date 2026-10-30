//! ローダーに埋め込むアプリケーションマニフェストをリンカー（link.exe）へ渡す引数。
//! build.rsが使う。引数の中身を固定する検査（このファイルを`main.rs`の検査へも取り込む）も同じ定義を読む。
//!
//! 実行レベルは`/MANIFESTUAC`に任せる（昇格を求めない・UIアクセスなし）。対応OSだけをXMLで追加する。
//! XML側に`requestedExecutionLevel`は書かない（リンカーが作る分と二重になる）。

/// `CARGO_MANIFEST_DIR`からの相対パス。
pub const SUPPORTED_OS_FILE: &str = "manifest/loader.manifest";

pub const EMBED: &str = "/MANIFEST:EMBED";
pub const UAC: &str = "/MANIFESTUAC:level='asInvoker' uiAccess='false'";

/// `supported_os_file`は絶対パス。`/MANIFESTINPUT`は相対パスをリンカーの作業フォルダ基準で解決するため。
pub fn link_args(supported_os_file: &str) -> [String; 3] {
    [
        EMBED.to_string(),
        UAC.to_string(),
        format!("/MANIFESTINPUT:{supported_os_file}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = include_str!("manifest/loader.manifest");
    const BUILD_RS: &str = include_str!("build.rs");

    #[test]
    fn link_args_embed_with_as_invoker_and_the_xml_input() {
        let a = link_args("C:\\x\\loader.manifest");
        assert_eq!(a[0], "/MANIFEST:EMBED");
        assert_eq!(a[1], "/MANIFESTUAC:level='asInvoker' uiAccess='false'");
        assert_eq!(a[2], "/MANIFESTINPUT:C:\\x\\loader.manifest");
        assert!(!UAC.contains("requireAdministrator") && !UAC.contains("true"));
    }

    #[test]
    fn the_xml_file_build_rs_points_at_exists_and_is_the_one_checked_here() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SUPPORTED_OS_FILE);
        assert_eq!(std::fs::read_to_string(path).unwrap(), XML);
    }

    #[test]
    fn xml_lists_the_supported_os_and_asks_for_no_elevation() {
        for guid in [
            "{35138b9a-5d96-4fbd-8e2d-a2440225f93a}", // 7
            "{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}", // 8
            "{1f676c76-80e1-4239-95bb-83d0f6d0da78}", // 8.1
            "{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}", // 10 / 11
        ] {
            assert_eq!(XML.matches(guid).count(), 1, "{guid}");
        }
        assert_eq!(XML.matches("<supportedOS ").count(), 4);
        assert!(XML.starts_with("<?xml version=\"1.0\""));
        assert!(XML.contains("urn:schemas-microsoft-com:compatibility.v1"));
        for banned in [
            "requestedExecutionLevel",
            "requireAdministrator",
            "highestAvailable",
            "uiAccess",
        ] {
            assert!(!XML.contains(banned), "{banned}");
        }
    }

    // build.rsの文面を読む検査なので実際にlink.exeへ渡った引数までは見ていない。
    #[test]
    fn build_rs_passes_the_args_to_the_msvc_linker_bins_only() {
        assert!(BUILD_RS.contains("manifest_link::link_args("));
        assert!(BUILD_RS.contains("cargo:rustc-link-arg-bins="));
        assert!(BUILD_RS.contains("\"msvc\""));
        assert!(BUILD_RS.contains("manifest_link::SUPPORTED_OS_FILE"));
    }
}
