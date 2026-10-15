//! 読み込まれている d3d8.dll の場所・大きさ・バージョン情報を集める Win32 の層（unsafe はこのファイルに閉じ込める。1 つずつ `// SAFETY:`）。
//! 行の組み立ては `higanr_core::d3d8_module`。読み取りのスレッドで 1 回呼ぶ（DllMain の中では呼ばない: version.dll の API はローダーロックの外で使う）。
//! ファイルは読むだけで、ゲームのフォルダには何も書かない。

use core::ffi::c_void;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use higanr_core::d3d8_module::{self, D3d8Module, VersionFields};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};

use crate::win::module_path;

/// バージョン情報のブロックの上限（暴走した値で大きな確保をしない）。
const MAX_VERSION_BLOCK: u32 = 1 << 20;

/// 翻訳の表に無かったときに試す言語とコードページ（米国英語・日本語。リソースを作る道具が最もよく書く組）。
const FALLBACK_LANGS: [u32; 4] = [0x0409_04B0, 0x0409_04E4, 0x0411_04B0, 0x0000_04B0];

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn path_wide_nul(p: &Path) -> Vec<u16> {
    OsStr::new(p)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// プロセスに読み込まれている `d3d8.dll` の材料。読み込まれていなければ `None`。
pub fn loaded_d3d8() -> Option<D3d8Module> {
    let name = wide_nul("d3d8.dll");
    // SAFETY: 0 終わりの UTF-16 の名前。読み込み済みのモジュールを引くだけで、参照カウントは増えない。
    let h = unsafe { GetModuleHandleW(name.as_ptr()) };
    (!h.is_null()).then(|| module_facts(h))
}

/// `h` のモジュールの場所・大きさ・バージョン情報。取れなかった欄は `None`。
fn module_facts(h: HMODULE) -> D3d8Module {
    let path = module_path(h);
    let game_dir = module_path(core::ptr::null_mut())
        .and_then(|e| e.parent().map(|d| d.display().to_string()));
    D3d8Module {
        path: path.as_ref().map(|p| p.display().to_string()),
        game_dir,
        system_dirs: system_dirs(),
        size: path
            .as_deref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len()),
        version: path.as_deref().map(version_fields).unwrap_or_default(),
    }
}

/// システムのフォルダ。32 ビットのプロセスが見るもの（64 ビットの Windows では SysWOW64）に加え、Windows のフォルダの下の
/// System32 / SysWOW64 も入れる（Wine などで `GetSystemDirectoryW` と読み込み元の綴りが違っても外れないように）。
fn system_dirs() -> Vec<String> {
    let mut v = Vec::new();
    // SAFETY: 書ける `buf` の長さをそのまま渡す。
    if let Some(s) =
        fill_dir(|buf| unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) })
    {
        v.push(s);
    }
    // SAFETY: 同上。
    if let Some(w) =
        fill_dir(|buf| unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) })
    {
        v.push(format!("{w}\\System32"));
        v.push(format!("{w}\\SysWOW64"));
    }
    v
}

/// フォルダを返す Win32 の関数（戻り値は書いた文字数。足りなければ必要な長さ、0 は失敗）を呼ぶ。
fn fill_dir(call: impl Fn(&mut [u16]) -> u32) -> Option<String> {
    let mut buf = vec![0u16; 260];
    let mut n = call(&mut buf) as usize;
    if n > buf.len() {
        buf = vec![0u16; n];
        n = call(&mut buf) as usize;
    }
    (n > 0 && n <= buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

/// ファイルのバージョン情報の 4 欄（読むだけ）。
fn version_fields(path: &Path) -> VersionFields {
    let Some(block) = read_version_block(path) else {
        return VersionFields::default();
    };
    let mut langs = translations(&block);
    langs.extend(FALLBACK_LANGS);
    let get = |name: &str| {
        langs.iter().find_map(|l| {
            query_string(&block, &format!("\\StringFileInfo\\{l:08x}\\{name}"))
                .filter(|s| !s.trim().is_empty())
        })
    };
    VersionFields {
        description: get("FileDescription"),
        product: get("ProductName"),
        version: get("FileVersion"),
        company: get("CompanyName"),
    }
}

fn read_version_block(path: &Path) -> Option<Vec<u8>> {
    let w = path_wide_nul(path);
    let mut handle = 0u32;
    // SAFETY: 0 終わりの UTF-16 のパスと、書ける u32。ファイルを読むだけ。
    let size = unsafe { GetFileVersionInfoSizeW(w.as_ptr(), &mut handle) };
    if size == 0 || size > MAX_VERSION_BLOCK {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: `block` は `size` バイトの書ける領域（GetFileVersionInfoSizeW が答えた大きさ）。第 2 引数は予約で 0。
    let ok = unsafe { GetFileVersionInfoW(w.as_ptr(), 0, size, block.as_mut_ptr().cast()) };
    (ok != 0).then_some(block)
}

/// `block` の中を指す `VerQueryValueW` の答え（先頭のアドレス, 長さ）。答えが `block` の外を指していたら `None`。
/// 長さの単位は呼ぶ側が決める（文字列は文字数、翻訳の表はバイト）。
fn query(block: &[u8], sub: &str) -> Option<(usize, u32)> {
    let w = wide_nul(sub);
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `block` は GetFileVersionInfoW が埋めたブロックで、呼び出しの間生きている。名前は 0 終わりの UTF-16。
    // 答えのポインタは `block` の中を指す（下で範囲を確かめてから読む）。
    let ok = unsafe { VerQueryValueW(block.as_ptr().cast(), w.as_ptr(), &mut ptr, &mut len) };
    let start = ptr as usize;
    let base = block.as_ptr() as usize;
    (ok != 0 && !ptr.is_null() && len != 0 && start >= base && start < base + block.len())
        .then_some((start, len))
}

/// 文字列の値（`\StringFileInfo\<言語とコードページ>\<名前>`）。
fn query_string(block: &[u8], sub: &str) -> Option<String> {
    let (start, len) = query(block, sub)?;
    let end = block.as_ptr() as usize + block.len();
    // 長さが範囲を越えていても、ブロックの終わりまでで切る。0 で終わるところまで読む。
    let avail = (end - start) / 2;
    let n = (len as usize).min(avail);
    let units: Vec<u16> = (0..n)
        .map(|i| {
            // SAFETY: `start + 2 * i + 2 <= end` を上の `avail` で確かめた（`block` の中）。揃っていなくてもよい読み。
            unsafe { ((start + 2 * i) as *const u16).read_unaligned() }
        })
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// 翻訳の表（言語 16 ビット + コードページ 16 ビットの組）。`StringFileInfo` の枝の名前（上位 16 ビットが言語）にそろえた値。
fn translations(block: &[u8]) -> Vec<u32> {
    let Some((start, bytes)) = query(block, "\\VarFileInfo\\Translation") else {
        return Vec::new();
    };
    let end = block.as_ptr() as usize + block.len();
    let n = ((bytes as usize).min(end - start) / 4).min(8);
    (0..n)
        .map(|i| {
            // SAFETY: `start + 4 * i + 4 <= end` を上の `min` で確かめた（`block` の中）。揃っていなくてもよい読み。
            let v = unsafe { ((start + 4 * i) as *const u32).read_unaligned() };
            // 表の組は 下位 16 ビットが言語、上位 16 ビットがコードページ。枝の名前は 言語 → コードページの順。
            v.rotate_right(16)
        })
        .collect()
}

/// 本ログの 1 行。読み込まれていなければ「読み込まれていない」。この後の起動の手順を止めないよう、パニックは受けて 1 行にする。
pub fn d3d8_line() -> String {
    std::panic::catch_unwind(|| d3d8_module::line(loaded_d3d8().as_ref()))
        .unwrap_or_else(|_| "D3D8: d3d8.dll の情報を集められなかった（パニック）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel32() -> HMODULE {
        let name = wide_nul("kernel32.dll");
        // SAFETY: 0 終わりの UTF-16 の名前。kernel32 は全プロセスに読み込み済み。
        unsafe { GetModuleHandleW(name.as_ptr()) }
    }

    /// 壊れたら落ちる: バージョン情報の読み方（翻訳の表の並び・文字列の長さの単位・VerQueryValueW の呼び方）。
    /// kernel32 は Windows にも Wine にもあり、FileVersion と CompanyName を持つ。
    #[test]
    fn version_fields_of_a_real_system_dll_are_read() {
        let p = module_path(kernel32()).expect("kernel32 のパス");
        let v = version_fields(&p);
        assert!(
            v.version.as_deref().is_some_and(|s| s.contains('.')),
            "{v:?}"
        );
        assert!(v.company.as_deref().is_some_and(|s| !s.is_empty()), "{v:?}");
    }

    /// 壊れたら落ちる: 翻訳の表の並び（言語とコードページの入れ替わり）。入れ替わっても既定の組が当たって欄は読めてしまうので、表そのものを見る。
    #[test]
    fn translation_table_is_language_then_code_page() {
        let p = module_path(kernel32()).expect("kernel32 のパス");
        let block = read_version_block(&p).expect("バージョン情報のブロック");
        let t = translations(&block);
        assert!(!t.is_empty(), "翻訳の表が空");
        assert!(
            t.iter()
                .all(|v| matches!(v & 0xFFFF, 0 | 0x03A4 | 0x04B0 | 0x04E4)),
            "下位 16 ビットがコードページになっていない: {t:08x?}"
        );
    }

    /// 壊れたら落ちる: システムのフォルダの判定（本物の `GetSystemDirectoryW` と読み込み元の綴りが合わない）。
    #[test]
    fn a_system_dll_is_placed_in_the_system_folder() {
        let m = module_facts(kernel32());
        let line = d3d8_module::line(Some(&m));
        assert!(line.contains("システムのフォルダにある"), "{line}");
        assert!(m.size.is_some_and(|s| s > 0), "{m:?}");
    }

    /// 壊れたら落ちる: 検査の exe は d3d8.dll を読み込まないのに「読み込まれている」と書く。
    #[test]
    fn a_dll_that_is_not_loaded_is_reported_as_such() {
        let name = wide_nul("d3d8.dll");
        // SAFETY: 0 終わりの UTF-16 の名前。読み込み済みかを引くだけ。
        let loaded = unsafe { !GetModuleHandleW(name.as_ptr()).is_null() };
        if !loaded {
            assert_eq!(loaded_d3d8(), None);
            assert!(d3d8_line().contains("読み込まれていない"));
        }
    }

    #[test]
    fn a_missing_file_gives_empty_fields_not_a_panic() {
        let v = version_fields(Path::new("Z:\\no\\such\\dir\\none.dll"));
        assert_eq!(v, VersionFields::default());
    }
}
