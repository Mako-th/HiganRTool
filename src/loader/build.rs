//! 配る HiganRTool.exe にバージョン情報とアプリケーションマニフェスト（昇格を求めない・対応 OS）を付ける。

use std::path::PathBuf;

use higanr_build_res::{Artifact, FileKind, VersionInfo, emit};

mod manifest_link;

fn main() {
    emit(
        &VersionInfo {
            file_description: "HiganRTool launcher",
            internal_name: "HiganRTool",
            original_filename: "HiganRTool.exe",
            kind: FileKind::App,
        },
        Artifact::Bins,
    );
    println!("cargo:rerun-if-changed=manifest_link.rs");
    println!(
        "cargo:rerun-if-changed={}",
        manifest_link::SUPPORTED_OS_FILE
    );
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // mingw（Linux での検査用）は別のリンカーで、link.exe の引数を解せない。
    if target_os == "windows" && target_env == "msvc" {
        let dir = PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR が無い"),
        );
        let file = dir.join(manifest_link::SUPPORTED_OS_FILE);
        for arg in manifest_link::link_args(&file.to_string_lossy()) {
            println!("cargo:rustc-link-arg-bins={arg}");
        }
    }
}
