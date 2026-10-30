//! 配布用のHiganRTool.dllにバージョン情報を付ける。

use higanr_build_res::{Artifact, FileKind, VersionInfo, emit};

fn main() {
    emit(
        &VersionInfo {
            file_description: "HiganRTool netplay module",
            internal_name: "HiganRTool",
            original_filename: "HiganRTool.dll",
            kind: FileKind::Dll,
        },
        Artifact::Cdylib,
    );
    higanr_build_res::emit_dev_assets_cfg();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        // stdのProcessPrng（bcryptprimitives.dll）の読み込みを最初の呼び出しまで遅らせる。古いWineでも読み込み時に失敗しないため。
        // 遅延読み込みに失敗したときのフックはhiganr_prng_shim（win.rsのinstall_delay_load_hook!）。
        println!(
            "cargo:rustc-link-arg-cdylib=/DELAYLOAD:{}",
            higanr_prng_shim::PRNG_DLL
        );
        println!("cargo:rustc-link-arg-cdylib=delayimp.lib");
    }
}
