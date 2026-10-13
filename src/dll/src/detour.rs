//! inline detour（5 バイトの `E9 rel32` ＋ トランポリン）の設置（pure）。
//!
//! 表の `Use::Detour` の行に `E9 rel32`（命令の全長が 5 バイトより長ければ残りは NOP）を書く。トランポリン（元の命令列 ＋ 戻りの `E9`）は
//! 止める前に確保して書き、実行できる保護にしておく（止めている間はヒープも確保も使わない）。止める → EIP を全長で見る →
//! 止めたまま全長を読み直す → 全長を書く → 再開する、の手順と 50 回の上限は call 先の差し替えと同じ（`hook::run_attempts`）。
//! トランポリンのアドレスは JMP を書く前に入口の静的変数に置く。書いた後に読み返し、飛び先が入口と一致するかをログに書く。外す手順は作らない。
//! バイトの組み立てと署名の照合は `higanr_core::patch`、Win32 は `DetourEnv` の向こう（本物は `hook_win::RealHook`）。
#![forbid(unsafe_code)]

use std::io::Write;

use higanr_core::patch::{self, PatchBuf, SiteError, TrampolineBuf};

use crate::hook::{
    Abort, CodeBytes, HookEnv, Rewrite, Tried, Tries, abort_text, gave_up_text, run_attempts,
    tries_line, written_text,
};
use crate::sites::{SiteDef, Use, hex_bytes};

/// inline detour が外から得るもの（`HookEnv` に、トランポリンの頁の確保・書き・実行できる保護・解放と、入口の静的変数を足したもの）。
pub trait DetourEnv: HookEnv {
    /// トランポリンの頁を `len` バイトぶん確保する（**止める前に**呼ぶ）。返すのは頁の先頭。確保できなければ `Err(GetLastError)`。
    fn alloc_trampoline(&mut self, len: usize) -> Result<u32, u32>;
    /// 確保したトランポリンに `bytes` を書き、実行できる保護にする。アドレスが決まってから戻りの `E9` の rel32 を作るので、
    /// `alloc_trampoline` の後に呼ぶ。
    fn write_and_seal_trampoline(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32>;
    /// 確保したトランポリンの頁を返す（まだ誰も指していない頁だけ）。
    fn free_trampoline(&mut self, addr: u32);
    /// 入口がトランポリンへ飛ぶためのアドレスを置く（JMP を書く前に呼ぶ。0 の間はこの入口へ向く JMP は無い）。
    fn set_trampoline(&mut self, addr: u32);
    /// 差し替え先の入口。`HookEnv::entry_address` は call 先の差し替えの入口なので、別に持つ。
    fn detour_entry_address(&self) -> u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrampError {
    /// 頁を確保できない（`GetLastError`）。
    Alloc(u32),
    /// 書けない・実行できる保護にできない（`GetLastError`）。
    Commit(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetourRefusal {
    /// 表の行が inline detour（`Use::Detour`）でない。
    NotDetourRow,
    /// 署名の照合（`check_site`）に落ちた（長さ不足・相対の分岐・禁止の範囲・境目の和）。
    Site(SiteError),
    /// `E9 rel32` を置けない（全長が 5..=16 でない）。
    PatchShape,
    /// トランポリンが長すぎる（盗んだ命令 ＋ 5 が上限を越える）。
    TrampolineTooLong,
    /// いまのバイト列が読めない。
    Unreadable,
    /// いまのバイト列が期待と違う。
    Mismatch(CodeBytes),
    /// トランポリンを用意できない。
    Trampoline(TrampError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetourOutcome {
    Refused(DetourRefusal),
    Aborted(Abort),
    GaveUp { last_eip: u32 },
    Written,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetourPlan {
    pub addr: u32,
    /// いまあるはずの、書き換える命令の全長のバイト列。
    pub expected: &'static [u8],
    /// 入口のアドレス（`E9 rel32` の飛び先）。
    pub entry: u32,
    /// `addr` に書く `E9 rel32`（＋ 残りが NOP）。書くのは全長ぶん。
    pub patch: PatchBuf,
}

impl DetourPlan {
    /// トランポリンの中身を、確保したアドレス `tramp_at` から作る。
    pub fn trampoline(&self, tramp_at: u32) -> Option<TrampolineBuf> {
        let site = self.core_site();
        patch::trampoline_bytes(&site, tramp_at)
    }

    fn core_site(&self) -> patch::Site {
        patch::Site {
            name: "detour",
            addr: self.addr,
            expected: self.expected,
            // 戻りの JMP の計算は境目を使わないので、空でよい（`trampoline_bytes` は `expected` の長さだけを見る）。
            instr_lens: &[],
        }
    }

    /// 止めている間の手順に渡す形（EIP を見る範囲・読み直す範囲・書く範囲はどれも命令の全長）。
    pub fn rewrite(&self) -> Rewrite {
        // 全長は 5..=16（`PatchShape` を通っている）なので `CodeBytes` に収まる。
        let expected = CodeBytes::new(self.expected).unwrap_or(CodeBytes::from([0u8; 5]));
        let write = CodeBytes::new(&self.patch.bytes[..self.patch.len]).unwrap_or(expected);
        Rewrite {
            addr: self.addr,
            expected,
            write_at: self.addr,
            write,
        }
    }
}

/// 表の行と入口のアドレスから、書く中身を決める（署名の照合は `higanr_core::patch::check_site`）。
pub fn plan_detour(
    def: &SiteDef,
    entry: u32,
    forbidden: &[patch::AddrRange],
) -> Result<DetourPlan, DetourRefusal> {
    let Use::Detour { instr_lens } = def.kind else {
        return Err(DetourRefusal::NotDetourRow);
    };
    let site = patch::Site {
        name: def.name,
        addr: def.addr,
        expected: def.expected,
        instr_lens,
    };
    patch::check_site(&site, forbidden).map_err(DetourRefusal::Site)?;
    let patch = patch::patch_bytes(&site, entry).ok_or(DetourRefusal::PatchShape)?;
    // トランポリンの長さはアドレスに依らない（盗んだ長さ ＋ 5）ので、アドレス 0 で置けるかだけ先に見る。
    if patch::trampoline_bytes(&site, 0).is_none() {
        return Err(DetourRefusal::TrampolineTooLong);
    }
    Ok(DetourPlan {
        addr: def.addr,
        expected: def.expected,
        entry,
        patch,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetourReport {
    pub outcome: DetourOutcome,
    pub plan: Option<DetourPlan>,
    pub trampoline: Option<u32>,
    pub tries: Tries,
    /// 書いた後に読み返した全長のバイト列（書いたときだけ）。
    pub readback: Option<CodeBytes>,
}

impl DetourReport {
    pub fn written(&self) -> bool {
        matches!(self.outcome, DetourOutcome::Written)
    }
}

/// inline detour を 1 地点に設置する。条件（同定・image base・表・全地点の照合）は呼び出し側が先に確かめる。
pub fn install_detour(
    env: &mut dyn DetourEnv,
    def: &SiteDef,
    forbidden: &[patch::AddrRange],
) -> DetourReport {
    let mut report = DetourReport {
        outcome: DetourOutcome::Refused(DetourRefusal::Unreadable),
        plan: None,
        trampoline: None,
        tries: Tries::default(),
        readback: None,
    };
    let plan = match plan_detour(def, env.detour_entry_address(), forbidden) {
        Ok(p) => p,
        Err(e) => {
            report.outcome = DetourOutcome::Refused(e);
            return report;
        }
    };
    report.plan = Some(plan);
    let mut now = [0u8; higanr_core::patch::MAX_PATCH_LEN];
    let n = plan.expected.len();
    if !env.read_code(plan.addr, &mut now[..n]) {
        report.outcome = DetourOutcome::Refused(DetourRefusal::Unreadable);
        return report;
    }
    if &now[..n] != plan.expected {
        report.outcome = DetourOutcome::Refused(DetourRefusal::Mismatch(
            CodeBytes::new(&now[..n]).unwrap_or(CodeBytes::from([0u8; 5])),
        ));
        return report;
    }
    // トランポリンは止める前に確保し、アドレスが決まってから戻りの `E9` の rel32 を作って書き、実行できる保護にする。
    let tramp_len = n + 5;
    let tramp_at = match env.alloc_trampoline(tramp_len) {
        Ok(a) => a,
        Err(code) => {
            report.outcome =
                DetourOutcome::Refused(DetourRefusal::Trampoline(TrampError::Alloc(code)));
            return report;
        }
    };
    report.trampoline = Some(tramp_at);
    let Some(tbytes) = plan.trampoline(tramp_at) else {
        env.free_trampoline(tramp_at);
        report.outcome = DetourOutcome::Refused(DetourRefusal::TrampolineTooLong);
        return report;
    };
    if let Err(code) = env.write_and_seal_trampoline(tramp_at, &tbytes.bytes[..tbytes.len]) {
        env.free_trampoline(tramp_at);
        report.outcome =
            DetourOutcome::Refused(DetourRefusal::Trampoline(TrampError::Commit(code)));
        return report;
    }
    env.set_trampoline(tramp_at);

    let tried = run_attempts(env, &plan.rewrite(), &mut report.tries);
    match tried {
        Tried::Written(_) => {
            report.outcome = DetourOutcome::Written;
            let mut back = [0u8; higanr_core::patch::MAX_PATCH_LEN];
            if env.read_code(plan.addr, &mut back[..n]) {
                report.readback = CodeBytes::new(&back[..n]);
            }
        }
        Tried::Aborted(a) => {
            report.outcome = DetourOutcome::Aborted(a);
            // まだ誰も指していないトランポリンは返す（設置の JMP を書いていないので、この頁へは飛べない）。
            env.free_trampoline(tramp_at);
        }
        Tried::GaveUp { last_eip } => {
            report.outcome = DetourOutcome::GaveUp { last_eip };
            env.free_trampoline(tramp_at);
        }
    }
    report
}

fn refusal_text(e: &DetourRefusal, expected: &[u8]) -> String {
    match e {
        DetourRefusal::NotDetourRow => "表の行が inline detour ではない".to_string(),
        DetourRefusal::Site(s) => format!("署名の照合に落ちた（{s:?}）"),
        DetourRefusal::PatchShape => {
            "全長が 5〜16 バイトでないので E9 rel32 を置けない".to_string()
        }
        DetourRefusal::TrampolineTooLong => "トランポリンが長すぎる".to_string(),
        DetourRefusal::Unreadable => "いまのバイト列が読めない".to_string(),
        DetourRefusal::Mismatch(now) => format!(
            "いまのバイト列 {} が期待 {} と違う",
            hex_bytes(now.as_slice()),
            hex_bytes(expected)
        ),
        DetourRefusal::Trampoline(TrampError::Alloc(code)) => {
            format!("トランポリンの頁を確保できない（GetLastError = {code}）")
        }
        DetourRefusal::Trampoline(TrampError::Commit(code)) => {
            format!("トランポリンに書けない・実行できる保護にできない（GetLastError = {code}）")
        }
    }
}

/// 何も問題なく書けたときは詳細だけ（`hook::write_report` と同じ分け方）。
pub fn write_report(out: &mut dyn Write, def: &SiteDef, r: &DetourReport) {
    if report_is_clean(r) {
        write_report_all(&mut crate::log_mode::detail_out(out), def, r);
    } else {
        write_report_all(out, def, r);
    }
}

fn report_is_clean(r: &DetourReport) -> bool {
    let (DetourOutcome::Written, Some(p)) = (&r.outcome, &r.plan) else {
        return false;
    };
    let readback_ok = r
        .readback
        .and_then(|b| b.head5())
        .and_then(|b| patch::jmp_target(p.addr, &b))
        == Some(p.entry);
    r.tries.resume_failures == 0 && readback_ok
}

fn write_report_all(out: &mut dyn Write, def: &SiteDef, r: &DetourReport) {
    let _ = match (&r.plan, r.trampoline) {
        (Some(p), Some(t)) => writeln!(
            out,
            "inline detour: 地点 {} 0x{:08X}（{}）に E9 で入口 0x{:08X} へ。トランポリン 0x{t:08X}（盗んだ {} バイト ＋ 戻り）",
            def.name,
            def.addr,
            def.use_label(),
            p.entry,
            p.expected.len()
        ),
        _ => writeln!(
            out,
            "inline detour: 地点 {} 0x{:08X}（{}）",
            def.name,
            def.addr,
            def.use_label()
        ),
    };
    let head = match &r.outcome {
        DetourOutcome::Refused(e) => format!(
            "書かない（止める前に断った: {}）",
            refusal_text(e, def.expected)
        ),
        DetourOutcome::Aborted(a) => abort_text(a, def.expected),
        DetourOutcome::GaveUp { last_eip } => gave_up_text(*last_eip),
        DetourOutcome::Written => written_text(
            &crate::hook::WriteDone {
                protect_restored: true,
                flushed: true,
            },
            &format!(
                "E9 rel32 ＋ {} バイトの NOP",
                r.plan.map_or(0, |p| p.patch.len - 5)
            ),
        ),
    };
    let _ = writeln!(out, "inline detour: {head}");
    let _ = writeln!(out, "{}", tries_line("inline detour の設置", &r.tries));
    if let (DetourOutcome::Written, Some(p)) = (&r.outcome, &r.plan) {
        let _ = match r.readback.and_then(|b| b.head5()) {
            Some(b) => {
                let target = patch::jmp_target(p.addr, &b);
                let verdict = if target == Some(p.entry) {
                    "入口と一致"
                } else {
                    "入口と不一致"
                };
                match target {
                    Some(t) => writeln!(
                        out,
                        "読み返し: {} → E9 の飛び先 0x{t:08X}（{verdict}）",
                        hex_bytes(b.as_slice())
                    ),
                    None => writeln!(
                        out,
                        "読み返し: {}（E9 で始まらない。{verdict}）",
                        hex_bytes(&b)
                    ),
                }
            }
            None => writeln!(out, "読み返し: 読めない"),
        };
    }
}

#[cfg(test)]
mod tests {
    //! 偽の `DetourEnv` で手順を見る。実物の入口・トランポリンは `real_*` の検査で、検査のプロセス自身に当てる。
    use super::*;
    use crate::hook::tests::{ENTRY, FakeHook, ORIGINAL, SITE_ADDR};

    /// 合成の inline detour の行（`A1 abs32` = `mov eax,[abs]`。相対命令を含まない 5 バイト）。
    fn detour_def() -> SiteDef {
        SiteDef {
            name: "synthetic_detour",
            addr: SITE_ADDR,
            expected: &[0xA1, 0x08, 0x31, 0x4B, 0x00],
            kind: Use::Detour { instr_lens: &[5] },
            source: "合成",
        }
    }

    /// `FakeHook` に detour の入口とトランポリンの模型を足す。
    fn fake_for_detour() -> FakeHook {
        let mut f = FakeHook::new(3);
        let def = detour_def();
        f.mem = [0x90; 16];
        f.mem[..5].copy_from_slice(def.expected);
        f
    }

    impl DetourEnv for FakeHook {
        fn alloc_trampoline(&mut self, _len: usize) -> Result<u32, u32> {
            if self.stopped_now != 0 {
                self.tramp_while_stopped += 1;
            }
            if let Some(code) = self.tramp_alloc_fail {
                return Err(code);
            }
            self.tramp_allocs += 1;
            Ok(self.tramp_addr)
        }
        fn write_and_seal_trampoline(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32> {
            if self.stopped_now != 0 {
                self.tramp_while_stopped += 1;
            }
            if let Some(code) = self.tramp_commit_fail {
                return Err(code);
            }
            self.tramp_committed = Some((addr, bytes.to_vec()));
            Ok(())
        }
        fn free_trampoline(&mut self, addr: u32) {
            if self.stopped_now != 0 {
                self.tramp_while_stopped += 1;
            }
            self.tramp_released.push(addr);
            if self.tramp_committed.as_ref().map(|(a, _)| *a) == Some(addr) {
                self.tramp_committed = None;
            }
        }
        fn set_trampoline(&mut self, addr: u32) {
            self.tramp_set.push(addr);
        }
        fn detour_entry_address(&self) -> u32 {
            self.detour_entry
        }
    }

    // ---- 計画 ----

    #[test]
    fn plan_encodes_jmp_and_trampoline() {
        let def = detour_def();
        let p = plan_detour(&def, ENTRY, &[]).expect("計画できる");
        // E9 rel32: ENTRY - (SITE_ADDR + 5) = 0x10002000 - 0x00601005 = 0x0FA00FFB。
        assert_eq!(&p.patch.bytes[..5], &[0xE9, 0xFB, 0x0F, 0xA0, 0x0F]);
        assert_eq!(p.patch.len, 5);
        let rw = p.rewrite();
        assert_eq!(rw.expected.len(), 5);
        assert_eq!(rw.write_at, SITE_ADDR);
        assert_eq!(rw.write.len(), 5);
        assert!(!rw.write.is_empty());
        // トランポリン（アドレス 0x00800000）: 盗んだ 5 バイト ＋ 戻り E9（0x00800005 → SITE_ADDR+5 = 0x00601005）。
        let t = p.trampoline(0x0080_0000).expect("トランポリン");
        assert_eq!(&t.bytes[..5], def.expected);
        // rel = 0x00601005 - (0x00800005 + 5) = -0x1FF005 = 0xFFE00FFB。
        assert_eq!(&t.bytes[5..10], &[0xE9, 0xFB, 0x0F, 0xE0, 0xFF]);
        assert_eq!(t.len, 10);
    }

    #[test]
    fn plan_rejects_bad_rows() {
        let call = SiteDef {
            name: "c",
            addr: SITE_ADDR,
            expected: &[0xE8, 0, 0, 0, 0],
            kind: Use::CallRel32 {
                original_target: ORIGINAL,
            },
            source: "合成",
        };
        assert_eq!(
            plan_detour(&call, ENTRY, &[]),
            Err(DetourRefusal::NotDetourRow)
        );
        let rel = SiteDef {
            expected: &[0xE9, 0, 0, 0, 0],
            kind: Use::Detour { instr_lens: &[5] },
            ..call
        };
        assert!(matches!(
            plan_detour(&rel, ENTRY, &[]),
            Err(DetourRefusal::Site(_))
        ));
        let forbidden = [patch::AddrRange {
            start: SITE_ADDR,
            end: SITE_ADDR + 4,
        }];
        assert!(matches!(
            plan_detour(&detour_def(), ENTRY, &forbidden),
            Err(DetourRefusal::Site(_))
        ));
    }

    // ---- 手順（偽物） ----

    /// どの経路でも守ること: 止めた数と再開した数が同じ、開いたハンドルを全部閉じた、止めている間にヒープ・確保をしていない、
    /// トランポリンのアドレスを JMP より前に置いた。
    fn assert_detour_balanced(f: &FakeHook, r: &DetourReport) {
        assert_eq!(f.suspends, f.resumes, "止めた数と再開した数");
        assert!(f.all_resumed(), "止めたままのスレッドがある");
        assert_eq!(f.opened_handles, f.closes, "開いたハンドルと閉じた数");
        assert_eq!(f.alloc_violations, 0, "止めている間にヒープを使った");
        assert_eq!(
            f.tramp_while_stopped, 0,
            "止めている間にトランポリンを確保・書き・返した"
        );
        assert_eq!(r.tries.suspended_total, f.suspends);
    }

    #[test]
    fn detour_writes_jmp_and_prepares_trampoline() {
        let def = detour_def();
        let mut f = fake_for_detour();
        let r = install_detour(&mut f, &def, &[]);
        assert!(r.written(), "{r:?}");
        assert_eq!(r.outcome, DetourOutcome::Written);
        assert_eq!(f.mem[0], 0xE9, "オペコードが E9 でない");
        assert_eq!(
            patch::jmp_target(SITE_ADDR, &f.mem[..5].try_into().unwrap()),
            Some(f.detour_entry)
        );
        assert_eq!(f.tramp_allocs, 1);
        assert_eq!(f.tramp_set, vec![f.tramp_addr]);
        assert_eq!(r.trampoline, Some(f.tramp_addr));
        assert_eq!(
            f.writes_before_setup, 0,
            "トランポリンのアドレスを置く前に JMP を書いた"
        );
        let (addr, bytes) = f
            .tramp_committed
            .clone()
            .expect("トランポリンを書いていない");
        assert_eq!(addr, f.tramp_addr);
        assert_eq!(&bytes[..5], def.expected, "盗んだ命令をコピーしていない");
        assert!(f.tramp_released.is_empty(), "書いたのに返した");
        let back = r.readback.and_then(|b| b.head5()).unwrap();
        assert_eq!(patch::jmp_target(SITE_ADDR, &back), Some(f.detour_entry));
        assert_detour_balanced(&f, &r);
    }

    #[test]
    fn detour_alloc_fail_writes_nothing() {
        let def = detour_def();
        let mut f = fake_for_detour();
        f.tramp_alloc_fail = Some(8);
        let r = install_detour(&mut f, &def, &[]);
        assert_eq!(
            r.outcome,
            DetourOutcome::Refused(DetourRefusal::Trampoline(TrampError::Alloc(8)))
        );
        assert_eq!(f.writes, 0, "確保できないのに書いた");
        assert!(f.tramp_set.is_empty(), "確保できないのにアドレスを置いた");
        assert_eq!(f.opens, 0, "確保できないのにスレッドを止めた");
        assert_eq!(f.mem[0], 0xA1, "書き換えた");
    }

    #[test]
    fn detour_commit_fail_frees_and_writes_nothing() {
        let def = detour_def();
        let mut f = fake_for_detour();
        f.tramp_commit_fail = Some(9);
        let r = install_detour(&mut f, &def, &[]);
        assert_eq!(
            r.outcome,
            DetourOutcome::Refused(DetourRefusal::Trampoline(TrampError::Commit(9)))
        );
        assert_eq!(f.writes, 0);
        assert_eq!(
            f.tramp_released,
            vec![f.tramp_addr],
            "書けないのに頁を返さない"
        );
        assert!(f.tramp_set.is_empty());
        assert_eq!(f.opens, 0);
    }

    #[test]
    fn detour_mismatch_before_stopping_touches_nothing() {
        let def = detour_def();
        let mut f = fake_for_detour();
        f.mem[2] ^= 0xFF;
        let r = install_detour(&mut f, &def, &[]);
        assert!(matches!(
            r.outcome,
            DetourOutcome::Refused(DetourRefusal::Mismatch(_))
        ));
        assert_eq!(f.tramp_allocs, 0);
        assert_eq!(f.writes, 0);
        assert_eq!(f.opens, 0);
    }

    #[test]
    fn detour_retries_while_eip_in_range_then_writes() {
        let def = detour_def();
        let mut f = fake_for_detour();
        f.eip = Box::new(|attempt, i| {
            if i == 1 && attempt <= 3 {
                Some(SITE_ADDR + 4) // 全長 5 の中（決め打ち 4 でなく addr+4）
            } else {
                Some(0x0070_0000)
            }
        });
        let r = install_detour(&mut f, &def, &[]);
        assert!(r.written());
        assert_eq!(r.tries.attempts, 4);
        assert_eq!(r.tries.in_range, 3);
        assert_eq!(f.writes, 1);
        assert_detour_balanced(&f, &r);
    }

    #[test]
    fn detour_reread_mismatch_frees_and_writes_nothing() {
        let def = detour_def();
        let mut f = fake_for_detour();
        f.mutate_on_first_suspend = Some([0xB8, 0, 0, 0, 0]); // mov eax,imm32
        let r = install_detour(&mut f, &def, &[]);
        assert!(matches!(
            r.outcome,
            DetourOutcome::Aborted(crate::hook::Abort::RereadMismatch(_))
        ));
        assert_eq!(f.writes, 0);
        assert_eq!(
            f.tramp_released,
            vec![f.tramp_addr],
            "断ったのに頁を返さない"
        );
        assert_detour_balanced(&f, &r);
    }

    #[test]
    fn detour_gives_up_and_frees() {
        let def = detour_def();
        let mut f = fake_for_detour();
        f.eip = Box::new(|_, i| Some(if i == 0 { SITE_ADDR } else { 0x0070_0000 }));
        let r = install_detour(&mut f, &def, &[]);
        assert!(matches!(r.outcome, DetourOutcome::GaveUp { .. }));
        assert_eq!(f.writes, 0);
        assert_eq!(f.tramp_released, vec![f.tramp_addr]);
        assert_detour_balanced(&f, &r);
    }
    // 本物の Win32 で detour を付ける検査は `hook_win.rs` の tests に置く（`real_install_detour_redirects_through_draw_entry`）。
}
