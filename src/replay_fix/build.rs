//! MSVC でリンクするときだけ、コモンコントロール v6 を使うマニフェストとバージョン情報を exe に埋める。
//! マニフェストが無いと Windows 標準の部品（BUTTON・COMBOBOX）が古い灰色の立体の見た目になる。
//! 外のクレートは使わず、リンカー（link.exe）の `/MANIFEST:EMBED` と `/MANIFESTDEPENDENCY` に任せる。
//! `windows_subsystem` や UAC の設定は link.exe の既定のまま（requestedExecutionLevel は asInvoker）。
//! mingw（Linux での検査用）は別のリンカーで、v6 を指定しなくても窓は動くので何も渡さない。

use higanr_build_res::{Artifact, FileKind, VersionInfo, emit};

fn main() {
    emit(
        &VersionInfo {
            file_description: "HiganRTool replay fixer",
            internal_name: "HiganRTool_replay_fix",
            original_filename: "HiganRTool_replay_fix.exe",
            kind: FileKind::App,
        },
        Artifact::Bins,
    );
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}
