//! 検査（tests/delay_load.rs）のexeだけに遅延読み込みのフラグを渡す。
//! 製品のexe・DLLのフラグはloaderとdllのbuild.rsが渡す。

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        // 検査用の2つのimport（存在しないDLLと、存在しても関数が無いDLL）。名前はtests/delay_load.rsと同じ。
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:higanr_absent_prng.dll");
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:version.dll");
        println!("cargo:rustc-link-arg-tests=/DELAYLOAD:shlwapi.dll");
        println!("cargo:rustc-link-arg-tests=delayimp.lib");
    }
}
