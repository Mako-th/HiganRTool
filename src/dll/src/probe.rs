//! 照合の手順（pure）。自分の exe の同定と、地点の表の各地点の期待バイト列の照合（読むだけ）をしてログに書く。
//! 最初のフックからは、ここの集計が全部通ったときだけ `session::run` が 1 地点を書く。
//! Win32 は `Env` の向こうに置き、ここは順番とログの文だけを持つ。同定が合わなければ 1 バイトも読まない。
//! 注入したスレッド（ゲームスレッドではない）で走るので、ファイルの I/O とヒープの確保は構わない。
#![forbid(unsafe_code)]

use std::io::Write;

use higanr_core::patch::AddrRange;
use higanr_core::pe::{Ident, IdentMismatch, check_ident};

use crate::log_mode::detail_out;
use crate::names::LocalTime;
use crate::sites::{SiteDef, SiteOutcome, check_table, hex_bytes, judge};
use crate::trial_summary::IdentState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentOutcome {
    Match,
    Mismatch(IdentMismatch),
    /// exe のパスが取れない・ファイルが読めない（理由の文）。
    Unreadable(String),
}

/// exe のファイルを読んだ結果を同定する（照合は `higanr_core::pe::check_ident`）。
pub fn ident_of_file(read: std::io::Result<Vec<u8>>, ident: &Ident<'_>) -> IdentOutcome {
    match read {
        Err(e) => IdentOutcome::Unreadable(e.to_string()),
        Ok(bytes) => match check_ident(&bytes, ident) {
            Ok(()) => IdentOutcome::Match,
            Err(m) => IdentOutcome::Mismatch(m),
        },
    }
}

/// 照合が外から得るもの。本物は `win::RealEnv`。
pub trait Env {
    fn now(&self) -> LocalTime;
    fn pid(&self) -> u32;
    fn exe_path(&self) -> Option<String>;
    fn ident(&self) -> IdentOutcome;
    /// 自分の exe が読み込まれた位置（`GetModuleHandleW(NULL)`）。
    fn image_base(&self) -> usize;
    /// 自分のプロセスのメモリ `[addr, addr + len)` を読む（読むだけ）。コミット済みで読める頁でなければ `None`。
    fn read(&mut self, addr: u32, len: usize) -> Option<Vec<u8>>;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// 地点の照合まで進んだか（同定・image base・表の検査のどれかで止まれば false）。
    pub checked_sites: bool,
    pub total: usize,
    pub matched: usize,
    pub mismatched: usize,
    pub unreadable: usize,
    /// 同定の結果（image base が違う・表の検査が落ちたときは `Match` のまま止まる）。
    pub ident: IdentState,
    /// 一致しなかった地点（不一致と読めなかったもの）の名前（試験のまとめの「フックの設置」の行）。
    pub bad_sites: Vec<&'static str>,
}

/// 起動の最初の 1 行（簡潔にも残す）。ビルド番号は配るバージョンのビルドだけ入る（開発用は 0）。
pub fn build_line() -> String {
    format!(
        "HiganRTool: ビルド {} / プロトコル {} / {}",
        higanr_core::update::BUILD_NUMBER,
        higanr_core::wire::PROTO_VER,
        if cfg!(feature = "dist") {
            "配布バージョン"
        } else {
            "開発用"
        }
    )
}

/// 照合の本体。ログの行を `out` に書く（書けなくても続ける）。最後の「スレッドを終える」の行は呼び出し側が書く。
pub fn run(
    env: &mut dyn Env,
    sites: &[SiteDef],
    forbidden: &[AddrRange],
    expected_image_base: u32,
    out: &mut dyn Write,
) -> Summary {
    let mut summary = Summary::default();
    let _ = writeln!(
        detail_out(out),
        "HiganRTool.dll（M1b の最初のフック: 照合が全部通ったときだけ、入力と待ちの call の飛び先 1 か所を書き換える。他の地点は読むだけ）"
    );
    let _ = writeln!(out, "{}", build_line());
    let _ = writeln!(out, "時刻: {}", env.now().display());
    let _ = writeln!(detail_out(out), "PID: {}", env.pid());
    let _ = writeln!(
        detail_out(out),
        "exe: {}",
        env.exe_path().as_deref().unwrap_or("（取れなかった）")
    );

    match env.ident() {
        IdentOutcome::Match => {
            summary.ident = IdentState::Match;
            let _ = writeln!(out, "同定: 一致（花映塚 1.50a）");
        }
        IdentOutcome::Mismatch(m) => {
            summary.ident = IdentState::Mismatch;
            let _ = writeln!(out, "同定: 不一致（食い違った欄: {:?}）", m.fields);
            let _ = writeln!(out, "地点の照合: しない（同定が合わないため）");
            return summary;
        }
        IdentOutcome::Unreadable(why) => {
            summary.ident = IdentState::Unreadable;
            let _ = writeln!(out, "同定: exe が読めない（{why}）");
            let _ = writeln!(out, "地点の照合: しない（同定ができないため）");
            return summary;
        }
    }

    let base = env.image_base();
    let base_ok = base == expected_image_base as usize;
    // 合っているときは詳細だけ。違うときは次の行の理由として残す。
    let _ = if base_ok {
        writeln!(
            detail_out(out),
            "image base: 0x{base:08X}（期待 0x{expected_image_base:08X}）"
        )
    } else {
        writeln!(
            out,
            "image base: 0x{base:08X}（期待 0x{expected_image_base:08X}）"
        )
    };
    if !base_ok {
        let _ = writeln!(
            out,
            "地点の照合: しない（image base が違うので、表の VA がどれも当たらない）"
        );
        return summary;
    }

    // 読み込まれた DLL が同じ表を持っている証拠として、表の検査をここでも行う。
    if let Err(e) = check_table(sites, forbidden) {
        let _ = writeln!(out, "表の検査: 落ちた（{e:?}）");
        let _ = writeln!(out, "地点の照合: しない（表が壊れているため）");
        return summary;
    }
    let _ = writeln!(
        detail_out(out),
        "表の検査: 通った（{} 行、禁止の範囲 {} 個）",
        sites.len(),
        forbidden.len()
    );

    summary.checked_sites = true;
    for def in sites {
        let actual = env.read(def.addr, def.expected.len());
        let outcome = judge(def, actual.as_deref());
        summary.total += 1;
        let head = format!(
            "地点 {} 0x{:08X} {} バイト（{}）",
            def.name,
            def.addr,
            def.expected.len(),
            def.use_label()
        );
        let _ = match &outcome {
            SiteOutcome::Match => {
                summary.matched += 1;
                writeln!(detail_out(out), "{head}: 一致")
            }
            SiteOutcome::Mismatch { actual } => {
                summary.mismatched += 1;
                summary.bad_sites.push(def.name);
                writeln!(
                    out,
                    "{head}: 不一致 実際 = {} / 期待 = {}{}",
                    hex_bytes(actual),
                    hex_bytes(def.expected),
                    // 配布のビルドでは出典が空。空の括弧を出さない
                    if def.source.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", def.source)
                    }
                )
            }
            SiteOutcome::Unreadable => {
                summary.unreadable += 1;
                summary.bad_sites.push(def.name);
                writeln!(out, "{head}: 読めない（コミット済みの読める頁ではない）")
            }
        };
    }
    let _ = writeln!(
        out,
        "集計: {} 地点のうち 一致 {} / 不一致 {} / 読めない {}",
        summary.total, summary.matched, summary.mismatched, summary.unreadable
    );
    summary
}

#[cfg(test)]
pub(crate) mod tests {
    //! 偽の `Env` で手順を見る。見たいのは「どの条件で読むか・読まないか」とログの配線。
    use super::*;
    use crate::th09_addr::{FORBIDDEN, IMAGE_BASE, SITES};
    use higanr_core::pe::IdentField;
    use std::collections::HashMap;

    pub(crate) struct Fake {
        pub(crate) ident: IdentOutcome,
        pub(crate) base: usize,
        pub(crate) mem: HashMap<u32, Vec<u8>>,
        pub(crate) reads: usize,
    }

    impl Fake {
        pub(crate) fn all_match() -> Self {
            Fake {
                ident: IdentOutcome::Match,
                base: IMAGE_BASE as usize,
                mem: SITES
                    .iter()
                    .map(|d| (d.addr, d.expected.to_vec()))
                    .collect(),
                reads: 0,
            }
        }
    }

    impl Env for Fake {
        fn now(&self) -> LocalTime {
            LocalTime {
                year: 2026,
                month: 1,
                day: 2,
                hour: 3,
                minute: 4,
                second: 5,
                millis: 6,
            }
        }
        fn pid(&self) -> u32 {
            4321
        }
        fn exe_path(&self) -> Option<String> {
            Some(r"C:\Synthetic\サンプル\game.exe".to_string())
        }
        fn ident(&self) -> IdentOutcome {
            self.ident.clone()
        }
        fn image_base(&self) -> usize {
            self.base
        }
        fn read(&mut self, addr: u32, len: usize) -> Option<Vec<u8>> {
            self.reads += 1;
            let v = self.mem.get(&addr)?;
            assert_eq!(
                v.len(),
                len,
                "0x{addr:08X} を期待バイト列の長さで読んでいない"
            );
            Some(v.clone())
        }
    }

    fn run_fake(f: &mut Fake) -> (Summary, String) {
        let mut out = Vec::new();
        let s = run(f, SITES, FORBIDDEN, IMAGE_BASE, &mut out);
        (s, String::from_utf8(out).expect("ログは UTF-8"))
    }

    #[test]
    fn ident_of_file_maps_read_errors_and_mismatches() {
        use higanr_core::pe::TH09_V150A;
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "合成");
        assert!(matches!(
            ident_of_file(Err(err), &TH09_V150A),
            IdentOutcome::Unreadable(_)
        ));
        assert_eq!(
            ident_of_file(Ok(b"not a PE".to_vec()), &TH09_V150A),
            IdentOutcome::Mismatch(IdentMismatch {
                fields: vec![
                    IdentField::TimeDateStamp,
                    IdentField::TextSizeOfRawData,
                    IdentField::EntryBytes
                ]
            })
        );
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        match ident_of_file(Ok(me), &TH09_V150A) {
            IdentOutcome::Mismatch(m) => assert!(m.fields.contains(&IdentField::TimeDateStamp)),
            other => panic!("検査の exe が {other:?}"),
        }
    }

    #[test]
    fn ident_mismatch_reads_nothing() {
        let mut f = Fake::all_match();
        f.ident = IdentOutcome::Mismatch(IdentMismatch {
            fields: vec![IdentField::TimeDateStamp, IdentField::EntryBytes],
        });
        let (s, log) = run_fake(&mut f);
        assert_eq!(f.reads, 0);
        assert!(!s.checked_sites);
        assert!(log.contains("同定: 不一致（食い違った欄: [TimeDateStamp, EntryBytes]）"));
        assert!(log.contains("地点の照合: しない（同定が合わないため）"));
        assert!(!log.contains("地点 "));
    }

    #[test]
    fn unreadable_exe_reads_nothing() {
        let mut f = Fake::all_match();
        f.ident = IdentOutcome::Unreadable("合成の理由".to_string());
        let (s, log) = run_fake(&mut f);
        assert_eq!(f.reads, 0);
        assert!(!s.checked_sites);
        assert!(log.contains("同定: exe が読めない（合成の理由）"));
    }

    #[test]
    fn wrong_image_base_reads_nothing() {
        let mut f = Fake::all_match();
        f.base = 0x0100_0000;
        let (s, log) = run_fake(&mut f);
        assert_eq!(f.reads, 0);
        assert!(!s.checked_sites);
        assert!(log.contains(&format!(
            "image base: 0x01000000（期待 0x{IMAGE_BASE:08X}）"
        )));
    }

    #[test]
    fn broken_table_reads_nothing() {
        let mut f = Fake::all_match();
        let mut table: Vec<SiteDef> = SITES.to_vec();
        table[1].addr = table[2].addr; // 2 行を重ねる
        let mut out = Vec::new();
        let s = run(&mut f, &table, FORBIDDEN, IMAGE_BASE, &mut out);
        assert_eq!(f.reads, 0);
        assert!(!s.checked_sites);
        assert!(String::from_utf8(out).unwrap().contains("表の検査: 落ちた"));
    }

    #[test]
    fn all_sites_match_when_memory_is_original() {
        let mut f = Fake::all_match();
        let (s, log) = run_fake(&mut f);
        assert_eq!(f.reads, SITES.len());
        assert_eq!(
            s,
            Summary {
                checked_sites: true,
                total: SITES.len(),
                matched: SITES.len(),
                mismatched: 0,
                unreadable: 0,
                ident: IdentState::Match,
                bad_sites: vec![],
            }
        );
        assert!(log.contains("時刻: 2026-01-02 03:04:05.006"));
        assert!(log.contains("PID: 4321"));
        assert!(log.contains(r"exe: C:\Synthetic\サンプル\game.exe"));
        assert!(log.contains("同定: 一致"));
        assert_eq!(log.matches(": 一致\n").count(), SITES.len());
        assert!(log.contains(&format!(
            "集計: {} 地点のうち 一致 {} / 不一致 0 / 読めない 0",
            SITES.len(),
            SITES.len()
        )));
    }

    /// 配るバージョンのビルドでは走らせない（行の末尾の出典は開発用のビルドだけが持つ）。
    #[cfg(not(feature = "dist"))]
    #[test]
    fn mismatch_and_unreadable_are_reported_and_the_rest_still_checked() {
        let mut f = Fake::all_match();
        let draw = SITES
            .iter()
            .find(|d| d.name == "draw_before_endscene")
            .unwrap();
        let mut bytes = draw.expected.to_vec();
        bytes[2] ^= 0xFF;
        f.mem.insert(draw.addr, bytes.clone());
        let decide = SITES.iter().find(|d| d.name == "title_decide").unwrap();
        f.mem.remove(&decide.addr);

        let (s, log) = run_fake(&mut f);
        assert_eq!(f.reads, SITES.len());
        assert_eq!(s.matched, SITES.len() - 2);
        assert_eq!(s.mismatched, 1);
        assert_eq!(s.unreadable, 1);
        let want_mismatch = format!(
            "地点 draw_before_endscene 0x{:08X} {} バイト（inline detour の予定）: 不一致 実際 = {} / 期待 = {}（ノート §1-2",
            draw.addr,
            draw.expected.len(),
            hex_bytes(&bytes),
            hex_bytes(draw.expected)
        );
        assert!(log.contains(&want_mismatch), "{log}");
        let want_unreadable = format!(
            "地点 title_decide 0x{:08X} {} バイト（inline detour の予定）: 読めない",
            decide.addr,
            decide.expected.len()
        );
        assert!(log.contains(&want_unreadable), "{log}");
    }
}
