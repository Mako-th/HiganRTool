//! 注入する DLL の import 表を読み、この Windows に無い DLL・関数を見つける。
//! Windows が DLL を読むとき、import が 1 つでも解決できないと読み込みごと失敗する（原因は `LoadLibraryW` の 0 としか分からない）。
//! 注入の前に自分のプロセスで同じ引き当てを試しておけば、何が無いかを名指しして、ゲームを起こす前に止められる。
//!
//! 読むのは通常の import 表だけ（遅延 import は読まない）。表の読み取りは `pe::parse` の節の表を使う。

use std::collections::BTreeSet;
use std::fmt::Write as _;

use higanr_core::lang::Lang;
use higanr_core::pe;

use crate::ui_lang::by_lang;
use crate::win::ErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportName {
    Name(String),
    Ordinal(u16),
}

impl ImportName {
    fn shown(&self) -> String {
        match self {
            ImportName::Name(n) => n.clone(),
            ImportName::Ordinal(o) => format!("#{o}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub dll: String,
    pub name: ImportName,
}

const E_LFANEW_AT: usize = 0x3C;
/// optional header の先頭から: NumberOfRvaAndSizes、データディレクトリの 1 番目（import 表）。
const NUMBER_OF_RVA_AND_SIZES: usize = 92;
const IMPORT_DIRECTORY: usize = 104;
const DESCRIPTOR_SIZE: usize = 20;
/// 壊れたファイルで終わらない走査をしないための上限。
const MAX_DESCRIPTORS: usize = 1024;
const MAX_THUNKS: usize = 65_536;
const MAX_NAME: usize = 512;

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn c_string(b: &[u8], off: usize) -> Option<String> {
    let tail = b.get(off..)?;
    let len = tail.iter().take(MAX_NAME).position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&tail[..len]).into_owned())
}

/// import 表の全部。import 表が無い DLL は空。表が読めない（壊れている）ときは理由。
pub fn read_imports(bytes: &[u8]) -> Result<Vec<Import>, &'static str> {
    let info = pe::parse(bytes).map_err(|_| "PE の見出しが読めない")?;
    let e_lfanew = u32_at(bytes, E_LFANEW_AT).ok_or("PE の見出しが読めない")? as usize;
    let opt = e_lfanew + 24;
    let dirs =
        u32_at(bytes, opt + NUMBER_OF_RVA_AND_SIZES).ok_or("データディレクトリが読めない")?;
    if dirs < 2 {
        return Ok(Vec::new());
    }
    let table_rva = u32_at(bytes, opt + IMPORT_DIRECTORY).ok_or("データディレクトリが読めない")?;
    if table_rva == 0 {
        return Ok(Vec::new());
    }
    let at = |rva: u32| {
        info.rva_to_file_offset(rva)
            .map(|o| o as usize)
            .ok_or("import 表がファイルの外を指している")
    };
    let mut out = Vec::new();
    let table = at(table_rva)?;
    for i in 0..MAX_DESCRIPTORS {
        let d = table + i * DESCRIPTOR_SIZE;
        let original = u32_at(bytes, d).ok_or("import 表が途中で切れている")?;
        let name_rva = u32_at(bytes, d + 12).ok_or("import 表が途中で切れている")?;
        let first = u32_at(bytes, d + 16).ok_or("import 表が途中で切れている")?;
        if original == 0 && name_rva == 0 && first == 0 {
            return Ok(out);
        }
        let dll = c_string(bytes, at(name_rva)?).ok_or("DLL の名前が読めない")?;
        let thunks = at(if original != 0 { original } else { first })?;
        for t in 0..MAX_THUNKS {
            let entry = u32_at(bytes, thunks + t * 4).ok_or("import 表が途中で切れている")?;
            if entry == 0 {
                break;
            }
            let name = if entry & 0x8000_0000 != 0 {
                ImportName::Ordinal((entry & 0xFFFF) as u16)
            } else {
                // IMAGE_IMPORT_BY_NAME: 2 バイトのヒントの後に名前。
                ImportName::Name(c_string(bytes, at(entry)? + 2).ok_or("関数の名前が読めない")?)
            };
            out.push(Import {
                dll: dll.clone(),
                name,
            });
        }
    }
    Err("DLL の数が多すぎる")
}

/// 1 つの import が、この Windows で引けるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Found,
    /// DLL が無い（`ERROR_MOD_NOT_FOUND`）。
    NoDll,
    /// DLL はあるが、その関数が無い。
    NoFunction,
    /// DLL を読めなかったが、無いとは言えない（権限・初期化の失敗など）。止める理由にはしない。
    LoadFailed(ErrorCode),
}

pub trait ImportProbe {
    fn probe(&mut self, dll: &str, name: &ImportName) -> Presence;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub dll: String,
    /// None なら DLL ごと無い。
    pub function: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub checked: usize,
    pub missing: Vec<Missing>,
    /// 確かめられなかった DLL と OS のエラー。
    pub unchecked: Vec<(String, ErrorCode)>,
}

/// 全部の import を引く。DLL が無いものはその DLL を 1 回だけ数える。
pub fn check(bytes: &[u8], probe: &mut dyn ImportProbe) -> Result<Report, &'static str> {
    let imports = read_imports(bytes)?;
    let mut report = Report {
        checked: imports.len(),
        ..Report::default()
    };
    // DLL ごとの結論（無い・確かめられない）。同じ DLL の残りの関数は引かない。
    let mut settled: BTreeSet<String> = BTreeSet::new();
    for imp in &imports {
        let key = imp.dll.to_ascii_lowercase();
        if settled.contains(&key) {
            continue;
        }
        match probe.probe(&imp.dll, &imp.name) {
            Presence::Found => {}
            Presence::NoDll => {
                settled.insert(key);
                report.missing.push(Missing {
                    dll: imp.dll.clone(),
                    function: None,
                });
            }
            Presence::NoFunction => report.missing.push(Missing {
                dll: imp.dll.clone(),
                function: Some(imp.name.shown()),
            }),
            Presence::LoadFailed(code) => {
                settled.insert(key);
                report.unchecked.push((imp.dll.clone(), code));
            }
        }
    }
    Ok(report)
}

/// 画面に出す文の 1 件ぶん。
fn missing_line(m: &Missing, os: &str, lang: Lang) -> String {
    match &m.function {
        Some(f) => {
            let dll = &m.dll;
            by_lang!(
                lang,
                ja: "この Windows には {dll}!{f} がありません（Windows のバージョン: {os}）",
                en: "This Windows does not have {dll}!{f} (Windows version: {os})"
            )
        }
        None => {
            let dll = &m.dll;
            by_lang!(
                lang,
                ja: "この Windows には {dll} がありません（Windows のバージョン: {os}）",
                en: "This Windows does not have {dll} (Windows version: {os})"
            )
        }
    }
}

/// 画面に出す文。欠けているものが無ければ None。
pub fn describe(report: &Report, os: &str, lang: Lang) -> Option<String> {
    if report.missing.is_empty() {
        return None;
    }
    let mut text = String::new();
    for m in &report.missing {
        let _ = writeln!(text, "{}", missing_line(m, os, lang));
    }
    text.push_str(match lang {
        Lang::Ja => "この Windows では HiganRTool を動かせないため、何も起動していません。",
        Lang::En => "HiganRTool cannot run on this Windows, so nothing was started.",
    });
    Some(text)
}

/// 引き当ての結果をどう扱うか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 欠けているものが無い。
    Clear,
    /// 起動を止める。画面に出す文。
    Stop(String),
    /// Wine / Proton の上なので止めずに続ける。欠けていた件数（ログに残す）。
    /// Wine の loader は無い関数を代わりの実装で埋めて読み込みを成功させる（推定。実機の Wine では確かめていない）ため、
    /// 欠けて見えても動くことがある。
    Tolerated { missing: usize },
}

pub fn judge(report: &Report, os: &str, wine: bool, lang: Lang) -> Verdict {
    match describe(report, os, lang) {
        None => Verdict::Clear,
        Some(_) if wine => Verdict::Tolerated {
            missing: report.missing.len(),
        },
        Some(text) => Verdict::Stop(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth;
    use std::collections::HashMap;

    const TEXT_FILE_OFFSET: usize = 0x200;
    const TEXT_RVA: u32 = 0x1000;

    /// 合成の PE に import 表を書く。`libs` は（DLL 名, 関数）。関数は名前か序数。
    fn pe_with_imports(libs: &[(&str, &[ImportName])]) -> Vec<u8> {
        let mut b = synth::pe32(1, 0x1000, &[0xC3]);
        b.resize(TEXT_FILE_OFFSET + 0x1000, 0);
        let rva = |off: usize| TEXT_RVA + off as u32;
        let put = |b: &mut Vec<u8>, off: usize, v: u32| {
            b[TEXT_FILE_OFFSET + off..TEXT_FILE_OFFSET + off + 4].copy_from_slice(&v.to_le_bytes());
        };
        let strings_at = 0x400usize;
        let mut cursor = strings_at;
        let mut thunks_at = 0x200usize;
        // 記述子は 0x100 から。
        for (i, (dll, names)) in libs.iter().enumerate() {
            let d = 0x100 + i * DESCRIPTOR_SIZE;
            let dll_at = cursor;
            b[TEXT_FILE_OFFSET + cursor..TEXT_FILE_OFFSET + cursor + dll.len()]
                .copy_from_slice(dll.as_bytes());
            cursor += dll.len() + 1;
            put(&mut b, d, rva(thunks_at));
            put(&mut b, d + 12, rva(dll_at));
            put(&mut b, d + 16, rva(thunks_at));
            for n in names.iter() {
                let entry = match n {
                    ImportName::Ordinal(o) => 0x8000_0000 | u32::from(*o),
                    ImportName::Name(s) => {
                        let at = cursor;
                        // 2 バイトのヒントを空けて名前。
                        b[TEXT_FILE_OFFSET + at + 2..TEXT_FILE_OFFSET + at + 2 + s.len()]
                            .copy_from_slice(s.as_bytes());
                        cursor += 2 + s.len() + 1;
                        rva(at)
                    }
                };
                put(&mut b, thunks_at, entry);
                thunks_at += 4;
            }
            thunks_at += 4; // 終わりの 0
        }
        // optional header: NumberOfRvaAndSizes と import 表のディレクトリ。
        let opt = 0x40 + 24;
        b[opt + NUMBER_OF_RVA_AND_SIZES..opt + NUMBER_OF_RVA_AND_SIZES + 4]
            .copy_from_slice(&16u32.to_le_bytes());
        b[opt + IMPORT_DIRECTORY..opt + IMPORT_DIRECTORY + 4]
            .copy_from_slice(&rva(0x100).to_le_bytes());
        b
    }

    fn name(s: &str) -> ImportName {
        ImportName::Name(s.to_string())
    }

    /// 持っている (DLL, 関数) の表から答える偽物。DLL の名前は大文字小文字を区別しない。
    struct Table {
        have: HashMap<String, Vec<String>>,
        failing: Vec<(String, ErrorCode)>,
        asked: Vec<String>,
    }

    impl Table {
        fn new(have: &[(&str, &[&str])]) -> Table {
            Table {
                have: have
                    .iter()
                    .map(|(d, f)| {
                        (
                            d.to_ascii_lowercase(),
                            f.iter().map(|s| s.to_string()).collect(),
                        )
                    })
                    .collect(),
                failing: Vec::new(),
                asked: Vec::new(),
            }
        }
    }

    impl ImportProbe for Table {
        fn probe(&mut self, dll: &str, n: &ImportName) -> Presence {
            let key = dll.to_ascii_lowercase();
            self.asked.push(format!("{dll}!{}", n.shown()));
            if let Some((_, code)) = self.failing.iter().find(|(d, _)| *d == key) {
                return Presence::LoadFailed(*code);
            }
            match self.have.get(&key) {
                None => Presence::NoDll,
                Some(f) if f.contains(&n.shown()) => Presence::Found,
                Some(_) => Presence::NoFunction,
            }
        }
    }

    fn sample() -> Vec<u8> {
        pe_with_imports(&[
            (
                "KERNEL32.dll",
                &[
                    name("CreateFileW"),
                    name("GetSystemCpuSetInformation"),
                    ImportName::Ordinal(7),
                ],
            ),
            ("winhttp.dll", &[name("WinHttpOpen"), name("WinHttpClose")]),
        ])
    }

    #[test]
    fn the_import_table_is_read_by_name_and_by_ordinal() {
        let imports = read_imports(&sample()).unwrap();
        let shown: Vec<String> = imports
            .iter()
            .map(|i| format!("{}!{}", i.dll, i.name.shown()))
            .collect();
        assert_eq!(
            shown,
            [
                "KERNEL32.dll!CreateFileW",
                "KERNEL32.dll!GetSystemCpuSetInformation",
                "KERNEL32.dll!#7",
                "winhttp.dll!WinHttpOpen",
                "winhttp.dll!WinHttpClose"
            ]
        );
    }

    #[test]
    fn a_dll_without_an_import_table_has_no_imports() {
        assert_eq!(
            read_imports(&synth::pe32(1, 0x200, &[0xC3])),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_broken_table_is_reported_not_guessed() {
        let mut b = sample();
        // import 表をどの節にも入らない場所へ向ける。
        let opt = 0x40 + 24;
        b[opt + IMPORT_DIRECTORY..opt + IMPORT_DIRECTORY + 4]
            .copy_from_slice(&0x0090_0000u32.to_le_bytes());
        assert_eq!(read_imports(&b), Err("import 表がファイルの外を指している"));
        assert!(read_imports(&b[..0x30]).is_err());
        assert!(read_imports(b"not a pe").is_err());
        // 記述子の列が終わらないまま切れても、止まって失敗を返す。
        let mut cut = sample();
        cut.truncate(TEXT_FILE_OFFSET + 0x108);
        assert!(read_imports(&cut).is_err());
    }

    #[test]
    fn a_missing_function_is_named_and_a_present_one_is_not() {
        let mut probe = Table::new(&[
            ("kernel32.dll", &["CreateFileW", "#7"]),
            ("winhttp.dll", &["WinHttpOpen", "WinHttpClose"]),
        ]);
        let r = check(&sample(), &mut probe).unwrap();
        assert_eq!(r.checked, 5);
        assert_eq!(
            r.missing,
            [Missing {
                dll: "KERNEL32.dll".into(),
                function: Some("GetSystemCpuSetInformation".into())
            }]
        );
        let text = describe(&r, "10.0.10240", Lang::Ja).unwrap();
        assert!(
            text.contains(
                "この Windows には KERNEL32.dll!GetSystemCpuSetInformation がありません（Windows のバージョン: 10.0.10240）"
            ),
            "{text}"
        );
        assert!(
            !text.contains("CreateFileW") && !text.contains("winhttp"),
            "{text}"
        );
        assert!(text.contains("何も起動していません"), "{text}");
    }

    #[test]
    fn a_missing_dll_is_named_once() {
        let mut probe = Table::new(&[(
            "kernel32.dll",
            &["CreateFileW", "GetSystemCpuSetInformation", "#7"],
        )]);
        let r = check(&sample(), &mut probe).unwrap();
        assert_eq!(
            r.missing,
            [Missing {
                dll: "winhttp.dll".into(),
                function: None
            }]
        );
        let text = describe(&r, "10.0.1", Lang::En).unwrap();
        assert!(
            text.contains("does not have winhttp.dll (Windows version: 10.0.1)"),
            "{text}"
        );
        assert!(!text.contains("WinHttpOpen"));
    }

    #[test]
    fn nothing_missing_means_no_message() {
        let mut probe = Table::new(&[
            (
                "kernel32.dll",
                &["CreateFileW", "GetSystemCpuSetInformation", "#7"],
            ),
            ("winhttp.dll", &["WinHttpOpen", "WinHttpClose"]),
        ]);
        let r = check(&sample(), &mut probe).unwrap();
        assert!(r.missing.is_empty() && r.unchecked.is_empty());
        assert_eq!(describe(&r, "10.0.1", Lang::Ja), None);
    }

    #[test]
    fn a_dll_that_could_not_be_loaded_is_not_called_missing() {
        let mut probe = Table::new(&[("winhttp.dll", &["WinHttpOpen", "WinHttpClose"])]);
        probe.failing.push(("kernel32.dll".into(), 5));
        let r = check(&sample(), &mut probe).unwrap();
        assert!(r.missing.is_empty(), "{r:?}");
        assert_eq!(r.unchecked, [("KERNEL32.dll".to_string(), 5)]);
        assert_eq!(describe(&r, "10.0.1", Lang::Ja), None);
        assert_eq!(
            probe
                .asked
                .iter()
                .filter(|a| a.starts_with("KERNEL32"))
                .count(),
            1,
            "確かめられなかった DLL の関数を引き続けた: {:?}",
            probe.asked
        );
    }

    /// 壊れたら落ちる: Windows で欠けているのに止めない、または Wine で欠けた関数が見えただけで止める。
    #[test]
    fn missing_imports_stop_a_windows_launch_but_not_a_wine_launch() {
        let mut probe = Table::new(&[
            ("kernel32.dll", &["CreateFileW", "#7"]),
            ("winhttp.dll", &["WinHttpOpen", "WinHttpClose"]),
        ]);
        let r = check(&sample(), &mut probe).unwrap();
        assert_eq!(r.missing.len(), 1);
        match judge(&r, "10.0.1", false, Lang::Ja) {
            Verdict::Stop(t) => assert!(t.contains("GetSystemCpuSetInformation"), "{t}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            judge(&r, "10.0.1", true, Lang::Ja),
            Verdict::Tolerated { missing: 1 }
        );
        // 欠けが無ければどちらも何も言わない。
        let none = Report::default();
        for wine in [false, true] {
            assert_eq!(judge(&none, "10.0.1", wine, Lang::Ja), Verdict::Clear);
        }
    }

    #[test]
    fn english_message_has_no_japanese() {
        let r = Report {
            checked: 1,
            missing: vec![Missing {
                dll: "a.dll".into(),
                function: Some("F".into()),
            }],
            unchecked: Vec::new(),
        };
        let t = describe(&r, "10.0.1", Lang::En).unwrap();
        assert!(!higanr_core::lang::has_japanese(&t), "{t}");
    }
}
