//! 配る HiganRTool.dll にバージョン情報を付ける。

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
        // std の ProcessPrng（bcryptprimitives.dll）を最初の呼びまで遅らせる。古い Wine で読み込みの時点に落ちないため。
        // 失敗のフックは higanr_prng_shim（win.rs の install_delay_load_hook!）。
        println!(
            "cargo:rustc-link-arg-cdylib=/DELAYLOAD:{}",
            higanr_prng_shim::PRNG_DLL
        );
        println!("cargo:rustc-link-arg-cdylib=delayimp.lib");
    }
}
