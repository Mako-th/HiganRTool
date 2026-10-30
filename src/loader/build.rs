//! 配布するHiganRTool.exeにバージョン情報とアプリケーションマニフェスト（昇格を求めない・対応OS）を付ける。

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
    // mingw（Linuxでの検査用）は別のリンカーなのでlink.exeの引数を解釈できない。
    if target_os == "windows" && target_env == "msvc" {
        let dir = PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR が無い"),
        );
        let file = dir.join(manifest_link::SUPPORTED_OS_FILE);
        for arg in manifest_link::link_args(&file.to_string_lossy()) {
            println!("cargo:rustc-link-arg-bins={arg}");
        }
        // stdのProcessPrng（bcryptprimitives.dll）を最初の呼び出しまで遅延ロードにする。古いWineで読み込み時に異常終了しないため。
        // 失敗時のフックはhiganr_prng_shim（real.rsのinstall_delay_load_hook!）。
        println!(
            "cargo:rustc-link-arg-bins=/DELAYLOAD:{}",
            higanr_prng_shim::PRNG_DLL
        );
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
