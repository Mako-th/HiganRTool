//! 検査（tests/delay_load.rs）の exe だけに、遅延読み込みの旗を渡す。製品の exe・DLL の旗は loader と dll の build.rs が渡す。

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        // 検査用の 2 つの import（存在しない DLL と、存在しても関数が無い DLL）。名前は tests/delay_load.rs と同じ。
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:higanr_absent_prng.dll");
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:version.dll");
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:shlwapi.dll");
        println!("cargo:rustc-link-arg-tests=delayimp.lib");
    }
}
