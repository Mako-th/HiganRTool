//! 配布するexe・DLLに付けるバージョン情報（VS_VERSIONINFO）の`.res`を作る。
//! rc.exeは使わない。`.res`はlink.exeがそのまま受け付けるのでバイト列を自分で組んで`OUT_DIR`へ書く。
//! これでWindows SDKの場所探しに依存せずLinuxでも同じバイト列を作れる（`tests/against_llvm_rc.rs`がllvm-rcの出力と比べる）。
//! 開発側の資料を読む検査のためのcfg `dev_assets`を出す関数（`emit_dev_assets_cfg`）も各クレートのbuild.rsが共有する。
//! build.rsからだけ呼ぶ。配布する成果物には入らない。
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// 製品名。配布するものすべてで同じ。
pub const PRODUCT_NAME: &str = "HiganRTool";
/// 会社名のフィールド。作者名の取り決めが無いので製品名と同じにしてある。
pub const COMPANY_NAME: &str = "HiganRTool";
/// LICENSEの先頭の著作権表示に合わせる。
pub const LEGAL_COPYRIGHT: &str = "Copyright (C) 2026 HiganRTool contributors";

/// 言語はUS英語（0x0409）。文字コードはUnicode（0x04B0）。
/// StringFileInfoの表の名前 "040904B0" とVarFileInfoのTranslationはこの組み合わせから作る。
const LANG_ID: u16 = 0x0409;
const CODE_PAGE: u16 = 0x04B0;

/// `VS_FIXEDFILEINFO::dwFileType`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// VFT_APP
    App,
    /// VFT_DLL
    Dll,
}

impl FileKind {
    fn code(self) -> u32 {
        match self {
            FileKind::App => 1,
            FileKind::Dll => 2,
        }
    }
}

/// 成果物ごとに違うフィールド。
#[derive(Clone, Debug)]
pub struct VersionInfo<'a> {
    pub file_description: &'a str,
    pub internal_name: &'a str,
    pub original_filename: &'a str,
    pub kind: FileKind,
}

/// `major.minor.patch[-pre][+build]`を4要素の配列にする。Cargoのバージョンは3要素なので第4要素はいつも0。
pub fn parse_version(s: &str) -> Result<[u16; 4], String> {
    let core = s.split(['-', '+']).next().unwrap_or("");
    let mut out = [0u16; 4];
    let mut n = 0;
    for part in core.split('.') {
        if n >= 3 {
            return Err(format!("バージョン {s:?} の数が 3 つを越えている"));
        }
        out[n] = part
            .parse::<u16>()
            .map_err(|_| format!("バージョン {s:?} の {part:?} が 0..=65535 の数でない"))?;
        n += 1;
    }
    if n != 3 {
        return Err(format!("バージョン {s:?} が major.minor.patch の形でない"));
    }
    Ok(out)
}

fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn align4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

/// UTF-16LE + 終端NUL。
fn push_wstr(v: &mut Vec<u8>, s: &str) {
    for u in s.encode_utf16() {
        push_u16(v, u);
    }
    push_u16(v, 0);
}

/// VERSIONINFOの入れ子の1ブロック。
/// `value_len`はwValueLength（文字列ならワード数・バイナリならバイト数）。`text`はwType。
/// wLengthは「最後の子の中身の終わりまで」。子の間のパディングは含む。最後の子の後ろのパディングは含まない（rc.exe / llvm-rcの出力と同じ数え方）。
fn block(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0]; // wLengthは最後に書き込む
    push_u16(&mut b, value_len);
    push_u16(&mut b, u16::from(text));
    push_wstr(&mut b, key);
    align4(&mut b);
    b.extend_from_slice(value);
    for c in children {
        align4(&mut b);
        b.extend_from_slice(c);
    }
    let len = u16::try_from(b.len()).expect("VERSIONINFO のブロックが 64 KiB を越えた");
    b[..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn string_entry(key: &str, value: &str) -> Vec<u8> {
    let mut v = Vec::new();
    push_wstr(&mut v, value);
    // wValueLengthは終端のNULを含むワード数。
    let words = u16::try_from(v.len() / 2).expect("値が長すぎる");
    block(key, &v, words, true, &[])
}

/// VS_VERSIONINFO本体（RT_VERSIONの中身）。
pub fn version_info_data(info: &VersionInfo<'_>, version: [u16; 4]) -> Vec<u8> {
    let [a, b, c, d] = version;
    let mut fixed = Vec::new();
    push_u32(&mut fixed, 0xFEEF_04BD); // dwSignature
    push_u32(&mut fixed, 0x0001_0000); // dwStrucVersion
    for _ in 0..2 {
        // dwFileVersionMS/LSの次にdwProductVersionMS/LS（MS = major.minor・LS = patch.build）。FileVersionと同じ値にする
        push_u32(&mut fixed, (u32::from(a) << 16) | u32::from(b));
        push_u32(&mut fixed, (u32::from(c) << 16) | u32::from(d));
    }
    push_u32(&mut fixed, 0x3F); // dwFileFlagsMask（VS_FFI_FILEFLAGSMASK）
    push_u32(&mut fixed, 0); // dwFileFlags
    push_u32(&mut fixed, 0x0004_0004); // dwFileOS（VOS_NT_WINDOWS32）
    push_u32(&mut fixed, info.kind.code()); // dwFileType
    push_u32(&mut fixed, 0); // dwFileSubtype
    push_u32(&mut fixed, 0); // dwFileDateMS
    push_u32(&mut fixed, 0); // dwFileDateLS
    debug_assert_eq!(fixed.len(), 52);

    let file_version = format!("{a}.{b}.{c}.{d}");
    let product_version = format!("{a}.{b}.{c}");
    let strings = [
        string_entry("CompanyName", COMPANY_NAME),
        string_entry("FileDescription", info.file_description),
        string_entry("FileVersion", &file_version),
        string_entry("InternalName", info.internal_name),
        string_entry("LegalCopyright", LEGAL_COPYRIGHT),
        string_entry("OriginalFilename", info.original_filename),
        string_entry("ProductName", PRODUCT_NAME),
        string_entry("ProductVersion", &product_version),
    ];
    let table = block(
        &format!("{LANG_ID:04X}{CODE_PAGE:04X}"),
        &[],
        0,
        true,
        &strings,
    );
    let string_file_info = block("StringFileInfo", &[], 0, true, &[table]);

    let mut translation = Vec::new();
    push_u16(&mut translation, LANG_ID);
    push_u16(&mut translation, CODE_PAGE);
    let translation = block("Translation", &translation, 4, false, &[]);
    let var_file_info = block("VarFileInfo", &[], 0, true, &[translation]);

    block(
        "VS_VERSION_INFO",
        &fixed,
        52,
        false,
        &[string_file_info, var_file_info],
    )
}

/// `.res`ファイル全体。先頭の空のリソース（32バイト）に続けてRT_VERSION（名前はID 1）を1つ書く。
pub fn version_res(info: &VersionInfo<'_>, version: [u16; 4]) -> Vec<u8> {
    const RT_VERSION: u16 = 16;
    const MOVEABLE_PURE: u16 = 0x0030;
    let mut out = Vec::new();
    // 空のリソース: DataSize 0 / HeaderSize 32 / TYPE・NAMEは序数0 / 以降0。.resと分かるように必ず先頭に要る。
    push_u32(&mut out, 0);
    push_u32(&mut out, 32);
    for v in [0xFFFFu16, 0, 0xFFFF, 0] {
        push_u16(&mut out, v);
    }
    for _ in 0..4 {
        push_u32(&mut out, 0);
    }
    debug_assert_eq!(out.len(), 32);

    let data = version_info_data(info, version);
    push_u32(
        &mut out,
        u32::try_from(data.len()).expect("データが大きすぎる"),
    );
    push_u32(&mut out, 32); // HeaderSize（型と名前が序数なので固定）
    for v in [0xFFFFu16, RT_VERSION, 0xFFFF, 1] {
        push_u16(&mut out, v);
    }
    push_u32(&mut out, 0); // DataVersion
    push_u16(&mut out, MOVEABLE_PURE);
    push_u16(&mut out, LANG_ID);
    push_u32(&mut out, 0); // Version
    push_u32(&mut out, 0); // Characteristics
    out.extend_from_slice(&data);
    align4(&mut out);
    out
}

/// 成果物の種類。リンカーへ渡す引数の宛先が違う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Artifact {
    Bins,
    Cdylib,
}

/// build.rsから呼ぶ。MSVCでリンクするときだけ`.res`を書いてlink.exeに渡す。
/// バージョンは呼び出し側のクレートの`CARGO_PKG_VERSION`（ワークスペースの1か所）から取る。
/// mingw（Linuxの検査用）はリンカーが .resを読めない。配布するものでもないので何もしない。
pub fn emit(info: &VersionInfo<'_>, artifact: Artifact) {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }
    let pkg = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION が無い");
    let version = parse_version(&pkg).unwrap_or_else(|e| panic!("{e}"));
    let out =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR が無い")).join("versioninfo.res");
    std::fs::write(&out, version_res(info, version)).expect("versioninfo.res を書けない");
    let kind = match artifact {
        Artifact::Bins => "bins",
        Artifact::Cdylib => "cdylib",
    };
    println!("cargo:rustc-link-arg-{kind}={}", out.display());
}

/// 開発リポジトリにだけある資料の場所（ルートからのパスの要素）。実機の検証の定義・解析のメモ・設計資料。
/// 公開用に書き出したツリーには`scripts/dist`と`tests/fixtures`しか無いのでこれらの有無で見分ける。
const DEV_DIRS: [&[&str]; 2] = [&["scripts", "runs"], &["analysis", "notes"]];
const DEV_FILES: [&[&str]; 1] = [&["docs", "Design_Architecture.md"]];

fn join_parts(root: &Path, parts: &[&str]) -> PathBuf {
    parts.iter().fold(root.to_path_buf(), |p, s| p.join(s))
}

/// 開発リポジトリにだけある資料が`repo_root`の下に全部あるかどうか。
pub fn dev_assets_present(repo_root: &Path) -> bool {
    DEV_DIRS.iter().all(|d| join_parts(repo_root, d).is_dir())
        && DEV_FILES.iter().all(|f| join_parts(repo_root, f).is_file())
}

/// build.rsから呼ぶ。`dev_assets_present`が真ならcfg `dev_assets`を立てる。
/// その資料を`include_str!`で読む検査は`#[cfg(dev_assets)]`の下に書く。資料の無い公開側ではビルドしない。
/// クレートは`<repo>/src/<name>/`にある前提（`CARGO_MANIFEST_DIR`の2つ上がリポジトリのルート）。
pub fn emit_dev_assets_cfg() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(dev_assets)");
    let dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR が無い"));
    if dev_assets_present(&dir.join("../..")) {
        println!("cargo::rustc-cfg=dev_assets");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_assets_need_every_part() {
        let tmp = std::env::temp_dir().join(format!(
            "higanr_build_res_dev_assets_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(!dev_assets_present(&tmp), "空の根が真");
        let make = |root: &Path, skip: Option<usize>| {
            let all: Vec<(&[&str], bool)> = DEV_DIRS
                .iter()
                .map(|d| (*d, true))
                .chain(DEV_FILES.iter().map(|f| (*f, false)))
                .collect();
            for (i, (parts, is_dir)) in all.into_iter().enumerate() {
                if Some(i) == skip {
                    continue;
                }
                let at = join_parts(root, parts);
                if is_dir {
                    std::fs::create_dir_all(&at).unwrap();
                } else {
                    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
                    std::fs::write(&at, "x").unwrap();
                }
            }
        };
        for i in 0..DEV_DIRS.len() + DEV_FILES.len() {
            let root = tmp.join(format!("missing{i}"));
            make(&root, Some(i));
            assert!(!dev_assets_present(&root), "{i} 番目を欠いたのに真");
        }
        let full = tmp.join("full");
        make(&full, None);
        assert!(dev_assets_present(&full));
        // 公開用に残るscripts/distだけでは真にならない。
        let public = tmp.join("public");
        std::fs::create_dir_all(public.join("scripts").join("dist")).unwrap();
        assert!(!dev_assets_present(&public));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn parse_version_takes_three_numbers_and_ignores_pre_release() {
        assert_eq!(parse_version("1.0.0"), Ok([1, 0, 0, 0]));
        assert_eq!(parse_version("2.10.65535-rc.1+x"), Ok([2, 10, 65535, 0]));
    }

    #[test]
    fn parse_version_refuses_bad_shapes() {
        for bad in ["", "1", "1.2", "1.2.3.4", "1.2.x", "1.2.65536", "-1.2.3"] {
            assert!(parse_version(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn version_numbers_land_in_fixed_file_info_and_strings() {
        let info = VersionInfo {
            file_description: "d",
            internal_name: "i",
            original_filename: "o.exe",
            kind: FileKind::App,
        };
        let data = version_info_data(&info, [3, 4, 5, 0]);
        let sig = 0xFEEF_04BDu32.to_le_bytes();
        let at = data.windows(4).position(|w| w == sig).unwrap();
        let word = |i: usize| u32::from_le_bytes(data[at + i..at + i + 4].try_into().unwrap());
        // FileVersionのMS/LSに続けてProductVersionのMS/LS
        assert_eq!((word(8), word(12)), ((3 << 16) | 4, 5 << 16));
        assert_eq!((word(16), word(20)), ((3 << 16) | 4, 5 << 16));
        let utf16 = |s: &str| {
            s.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>()
        };
        let has = |s: &str| data.windows(s.len() * 2).any(|w| w == utf16(s));
        assert!(has("3.4.5.0\0") && has("3.4.5\0"));
        assert_eq!(
            usize::from(u16::from_le_bytes([data[0], data[1]])),
            data.len()
        );
    }

    #[test]
    fn version_change_changes_the_bytes() {
        let info = VersionInfo {
            file_description: "d",
            internal_name: "i",
            original_filename: "o.exe",
            kind: FileKind::Dll,
        };
        assert_ne!(
            version_res(&info, [1, 0, 0, 0]),
            version_res(&info, [1, 0, 1, 0])
        );
    }
}
