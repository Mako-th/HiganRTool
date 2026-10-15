//! 自作の `.res` を llvm-rc が同じ内容の `.rc` から作る `.res` とバイト単位で比べる。
//! llvm-rc が無い環境（Windows の開発機など）では飛ばす。`REQUIRE_LLVM_RC=1` なら無いこと自体を失敗にする。

use higanr_build_res::{FileKind, VersionInfo, parse_version, version_res};
use std::path::Path;
use std::process::Command;

fn rc_source(info: &VersionInfo<'_>, v: [u16; 4]) -> String {
    let [a, b, c, d] = v;
    let kind = match info.kind {
        FileKind::App => "VFT_APP",
        FileKind::Dll => "VFT_DLL",
    };
    format!(
        r#"#include <windows.h>
LANGUAGE LANG_ENGLISH, SUBLANG_ENGLISH_US
1 VERSIONINFO
FILEVERSION {a},{b},{c},{d}
PRODUCTVERSION {a},{b},{c},{d}
FILEFLAGSMASK 0x3F
FILEFLAGS 0
FILEOS VOS_NT_WINDOWS32
FILETYPE {kind}
FILESUBTYPE 0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "{company}"
      VALUE "FileDescription", "{desc}"
      VALUE "FileVersion", "{a}.{b}.{c}.{d}"
      VALUE "InternalName", "{internal}"
      VALUE "LegalCopyright", "{copyright}"
      VALUE "OriginalFilename", "{orig}"
      VALUE "ProductName", "{product}"
      VALUE "ProductVersion", "{a}.{b}.{c}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 0x04B0
  END
END
"#,
        company = higanr_build_res::COMPANY_NAME,
        desc = info.file_description,
        internal = info.internal_name,
        copyright = higanr_build_res::LEGAL_COPYRIGHT,
        orig = info.original_filename,
        product = higanr_build_res::PRODUCT_NAME,
    )
}

fn llvm_rc_res(dir: &Path, rc: &str) -> Option<Vec<u8>> {
    let rc_path = dir.join("v.rc");
    let res_path = dir.join("v.res");
    std::fs::write(&rc_path, rc).unwrap();
    // windows.h の代わりに最小の定義を置く（llvm-rc は SDK が無くても動く）。
    std::fs::write(
        dir.join("windows.h"),
        "#define LANG_ENGLISH 0x09\n#define SUBLANG_ENGLISH_US 0x01\n\
         #define VOS_NT_WINDOWS32 0x00040004L\n#define VFT_APP 0x1L\n#define VFT_DLL 0x2L\n",
    )
    .unwrap();
    let out = match Command::new("llvm-rc")
        .args(["/FO"])
        .arg(&res_path)
        .arg(format!("/I{}", dir.display()))
        .arg(&rc_path)
        .output()
    {
        Ok(o) => o,
        Err(_) => return None,
    };
    assert!(
        out.status.success(),
        "llvm-rc が失敗: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(std::fs::read(res_path).unwrap())
}

#[test]
fn matches_llvm_rc_byte_for_byte() {
    let dir = std::env::temp_dir().join(format!("higanr_build_res_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        (
            VersionInfo {
                file_description: "HiganRTool launcher",
                internal_name: "HiganRTool",
                original_filename: "HiganRTool.exe",
                kind: FileKind::App,
            },
            parse_version("1.0.0").unwrap(),
        ),
        (
            VersionInfo {
                file_description: "HiganRTool netplay module",
                internal_name: "HiganRTool",
                original_filename: "HiganRTool.dll",
                kind: FileKind::Dll,
            },
            parse_version("12.34.567-rc.1+meta").unwrap(),
        ),
    ];
    let mut ran = false;
    for (info, ver) in &cases {
        let Some(want) = llvm_rc_res(&dir, &rc_source(info, *ver)) else {
            assert!(
                std::env::var_os("REQUIRE_LLVM_RC").is_none(),
                "REQUIRE_LLVM_RC=1 なのに llvm-rc が無い"
            );
            eprintln!("llvm-rc が無いので比べていない");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        ran = true;
        let got = version_res(info, *ver);
        assert_eq!(
            got, want,
            "llvm-rc の .res と違う（{}）",
            info.original_filename
        );
    }
    assert!(ran);
    let _ = std::fs::remove_dir_all(&dir);
}
