//! `HiganRTool.exe --selftest` —— 本物の Win32 の経路で、後始末の行 1・2・8・9 を通す。
//!
//! **花映塚は起動しない。** 的はこのローダー自身（`--selftest-target`）。偽物の検査（`launch.rs`）は手順を見、ここは
//! 「OS が本当にそう振る舞うか」（CREATE_SUSPENDED の子にリモートスレッドで LoadLibraryW が効く・読めない DLL で 0 が返る・
//! 再開した子の中に DLL が居る）を見る。行 9 は環境変数が子に届くことと、鮮度のガード（`freshness.rs`）の判定も見る。
//! 一時フォルダに読めない DLL を作るだけで、起動した的はどの結果でも必ず終わらせる。ゲームのフォルダには何も書かない。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use higanr_core::pe::{self, Ident};

use crate::launch::{self, LaunchError, LaunchRequest, TERMINATE_WAIT_MS};
use crate::ledger::{Call, Traced};
use crate::real::{self, RealWin32};
use crate::synth;
use crate::win::{WaitResult, Win32};

/// 再開した的が自分で終わるのを待つ上限（普段は 1 秒もかからない）。
const TARGET_EXIT_WAIT_MS: u32 = 10_000;

/// 的が「DLL が自分の中に居た」ときの終了コード。
pub const TARGET_OK: i32 = 0;
pub const TARGET_MISSING: i32 = 3;
/// DLL は居たが、渡した環境の変数が届いていない・値が違う。
pub const TARGET_ENV_MISSING: i32 = 4;
/// 親の環境（`SystemRoot`）が継がれていない（ブロックが親の環境を捨てた）。
pub const TARGET_PARENT_ENV_LOST: i32 = 5;

/// 行 9 で的の環境に置く変数（`--ini` / `--script` と同じ `env_block` の経路）。
const SELFTEST_ENV: &str = "HIGANRTOOL_SELFTEST_ENV";
const SELFTEST_ENV_VALUE: &str = "row9 ツール";

/// 花映塚の同定（`TH09_V150A`）と同じ 20 バイト。
const ENTRY_LEN: usize = 20;

struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "HiganRTool_selftest_{}_{}",
            std::process::id(),
            nanos
        ));
        // 前の回のフォルダを使い回さないよう、既にあれば失敗させる。
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Self(dir))
    }

    /// 読めない DLL を読ませた直後は、ウイルス対策などが掴んでいることがあるので少し待って試し直す。
    fn remove(&self) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..10 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(format!("{}: {last}", self.0.display()))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

struct Report {
    passed: usize,
    total: usize,
}

impl Report {
    fn check(&mut self, name: &str, result: Result<String, String>) {
        self.total += 1;
        match result {
            Ok(detail) => {
                self.passed += 1;
                println!("  ok  {name}: {detail}");
            }
            Err(why) => println!("  NG  {name}: {why}"),
        }
    }
}

fn ledger_clean(t: &Traced<RealWin32>) -> Result<(), String> {
    if !t.open_handles().is_empty() {
        return Err(format!("閉じていないハンドル {:?}", t.open_handles()));
    }
    if !t.bad_closes().is_empty() {
        return Err(format!("二重・未知の close {:?}", t.bad_closes()));
    }
    if !t.live_regions().is_empty() {
        return Err(format!("返していない領域 {:?}", t.live_regions()));
    }
    Ok(())
}

fn terminated_and_gone(t: &Traced<RealWin32>) -> Result<(), String> {
    let calls = t.calls();
    let term = calls
        .iter()
        .position(|c| matches!(c, Call::TerminateProcess { ok: true, .. }))
        .ok_or("TerminateProcess が成功していない")?;
    match calls.get(term + 1) {
        Some(Call::Wait {
            timeout_ms: TERMINATE_WAIT_MS,
            result: WaitResult::Signaled,
            ..
        }) => Ok(()),
        other => Err(format!(
            "殺したターゲットが消えたと確かめられない: {other:?}"
        )),
    }
}

fn own_ident(exe_bytes: &[u8]) -> Result<Ident<'_>, String> {
    let info = pe::parse(exe_bytes).map_err(|e| format!("自分の exe が読めない: {e:?}"))?;
    let text = info
        .section_by_name(".text")
        .ok_or("自分の exe に .text が無い")?;
    let entry = info
        .entry_bytes(exe_bytes, ENTRY_LEN)
        .ok_or("自分の exe の入口が読めない")?;
    Ok(Ident {
        time_date_stamp: info.time_date_stamp,
        text_size_of_raw_data: text.size_of_raw_data,
        entry_bytes: entry,
    })
}

pub fn run(dll: Option<PathBuf>) -> i32 {
    match run_inner(dll) {
        Ok(code) => code,
        Err(e) => {
            println!("自己検査を始められませんでした: {e}");
            1
        }
    }
}

fn run_inner(dll: Option<PathBuf>) -> Result<i32, String> {
    real::quiet_error_mode();

    let me = std::env::current_exe().map_err(|e| format!("自分の exe の場所: {e}"))?;
    let me = std::path::absolute(&me).map_err(|e| e.to_string())?;
    let good_dll = match dll {
        Some(d) => std::path::absolute(&d).map_err(|e| e.to_string())?,
        None => crate::default_dll_path(&me)?,
    };
    let me_bytes = std::fs::read(&me).map_err(|e| format!("{}: {e}", me.display()))?;
    let ident = own_ident(&me_bytes)?;

    let tmp = TempDir::create()?;
    let missing_dll = tmp.0.join("missing.dll");
    let bad_dll = tmp.0.join("not_loadable.dll");
    // core の読み取りは通る（行 1 で弾かれない）が、Windows のローダーは読めない合成の PE。
    std::fs::write(&bad_dll, synth::pe32(0, 0x200, &[0xC3]))
        .map_err(|e| format!("{}: {e}", bad_dll.display()))?;

    println!("HiganRTool --selftest（花映塚は起動しない。ターゲットはこの exe 自身）");
    println!("  ターゲット: {}", me.display());
    println!("  DLL（行 9）: {}", good_dll.display());
    println!("  一時フォルダ: {}", tmp.0.display());

    // 行 9（`keep`）だけ的の環境に変数を置く（行 1・2・8 は親の環境を継ぐ経路。両方を通す）。
    fn make_req<'a>(me: &Path, dll: &Path, ident: Ident<'a>, keep: bool) -> LaunchRequest<'a> {
        LaunchRequest {
            game: me.to_path_buf(),
            dll: dll.to_path_buf(),
            args: vec!["--selftest-target".into(), dll.as_os_str().to_owned()],
            env: if keep {
                vec![(SELFTEST_ENV.into(), SELFTEST_ENV_VALUE.into())]
            } else {
                Vec::new()
            },
            ident,
            keep_process_handle: keep,
            patches: Vec::new(),
            // 行 9 だけ前面にしない起こし方（STARTF_USESHOWWINDOW つきの STARTUPINFOW を受け付けることを通す）。
            no_activate: keep,
        }
    }
    // 親の環境に同じ名前が既にあると、行 9 の「届いた」が継いだだけで通ってしまう。先に断る。
    if std::env::var_os(SELFTEST_ENV).is_some() {
        return Err(format!(
            "この自己検査の環境に {SELFTEST_ENV} が既にある（行 9 の環境の確かめができない）"
        ));
    }
    let req = |dll: &Path, ident, keep| make_req(&me, dll, ident, keep);
    let mut report = Report {
        passed: 0,
        total: 0,
    };

    // 鮮度のガードを実機の起動と同じ入力で回す。断る判定は NG（`--game` の起動も断られるため）。
    report.check(
        "鮮度のガード（build.bat の後の build\\bin は断らない）",
        match crate::freshness::check(&me, &good_dll) {
            crate::freshness::Verdict::Fresh => Ok(
                "開発の形。DLL と exe はどちらも cargo の依存の一覧のソースより新しいか同じ".into(),
            ),
            crate::freshness::Verdict::NotDev => {
                Ok("開発の形ではない（リポジトリの build\\bin の外）ので調べていない".into())
            }
            crate::freshness::Verdict::Stale(list) => Err(list
                .iter()
                .map(crate::freshness::describe)
                .collect::<Vec<_>>()
                .join(" / ")
                + "。build.bat を先に回してください"),
        },
    );

    // 的の判定の否定側: DLL を読み込んでいないこのプロセスで、終了コード 0 が恒真でないことを確かめる。
    report.check(
        "ターゲットの判定（否定側）",
        match target(&good_dll) {
            TARGET_MISSING => Ok("DLL を読んでいないこのプロセスでは「居ない」と判定した".into()),
            other => Err(format!(
                "DLL を読んでいないのに {other} を返した（行 9 の確認が恒真になっている）"
            )),
        },
    );

    // 行 1: DLL が無い → 何も起動しない。
    {
        let mut t = Traced::new(RealWin32);
        let r = launch::launch(&mut t, &req(&missing_dll, ident, false));
        report.check(
            "行 1 DLL が無い",
            match r {
                Err(f) if matches!(f.error, LaunchError::DllUnreadable(_)) => {
                    if t.count("CreateProcessSuspended") != 0 {
                        Err("プロセスを作った".into())
                    } else {
                        ledger_clean(&t)
                            .map(|_| "何も起動せずに終えた（CreateProcess 0 回）".into())
                    }
                }
                other => Err(format!("期待は行 1 の失敗、実際は {other:?}")),
            },
        );
    }

    // 行 2（追加）: 花映塚の同定の値で自分を見る → 合わないので何も起動しない。
    {
        let mut t = Traced::new(RealWin32);
        let r = launch::launch(&mut t, &req(&good_dll, pe::TH09_V150A, false));
        report.check(
            "行 2 同定が合わない",
            match r {
                Err(f) if matches!(f.error, LaunchError::GameIdentMismatch(_)) => {
                    if t.count("CreateProcessSuspended") != 0 {
                        Err("プロセスを作った".into())
                    } else {
                        ledger_clean(&t)
                            .map(|_| format!("{:?} で止めた（CreateProcess 0 回）", f.error))
                    }
                }
                other => Err(format!("期待は行 2 の失敗、実際は {other:?}")),
            },
        );
    }

    // 行 8: 読めない DLL → LoadLibraryW が 0 → 的を殺す。
    {
        let mut t = Traced::new(RealWin32);
        let r = launch::launch(&mut t, &req(&bad_dll, ident, false));
        let pid = t
            .created_pids()
            .first()
            .map_or_else(|| "不明".to_string(), |p| p.to_string());
        report.check(
            "行 8 LoadLibraryW が 0",
            match r {
                Err(f) if f.error == LaunchError::LoadLibraryReturnedZero => {
                    if !f.cleanup_issues.is_empty() {
                        Err(format!("後始末の問題 {:?}", f.cleanup_issues))
                    } else {
                        terminated_and_gone(&t).and_then(|_| ledger_clean(&t)).map(|_| {
                            format!("ターゲット（pid {pid}）を再開せずに終わらせ、ハンドルと領域を全部返した")
                        })
                    }
                }
                other => Err(format!("期待は行 8 の失敗、実際は {other:?}")),
            },
        );
    }

    // 行 9: 本物の DLL → 注入して再開。的は自分の中に DLL が居ることを確かめて 0 で終わる。
    {
        let mut t = Traced::new(RealWin32);
        let r = launch::launch(&mut t, &req(&good_dll, ident, true));
        let result = match r {
            Ok(ok) if ok.process.is_none() => Err(format!(
                "プロセスのハンドルが返らない（keep_process_handle を立てたのに）: {ok:?}"
            )),
            Ok(ok) => {
                let process = ok.process.unwrap_or(crate::win::Handle(0));
                let waited = t.wait(process, TARGET_EXIT_WAIT_MS);
                let mut outcome = match waited {
                    WaitResult::Signaled => match t.inner.process_exit_code(process) {
                        Ok(c) if c as i32 == TARGET_OK => Ok(format!(
                            "注入して再開した（pid {}、DLL のアドレス 0x{:08X}）。ターゲットは DLL が自分の中に居ること、渡した環境の変数と親の環境が届いたことを確かめて終了コード 0 で終わった",
                            ok.pid, ok.module
                        )),
                        Ok(c) => Err(format!(
                            "ターゲットの終了コードが {c}（{TARGET_MISSING} は DLL が居なかった、{TARGET_ENV_MISSING} は渡した環境の変数が届かなかった、{TARGET_PARENT_ENV_LOST} は親の環境が継がれなかった）"
                        )),
                        Err(e) => Err(format!(
                            "ターゲットの終了コードが読めない（GetLastError {e}）"
                        )),
                    },
                    other => {
                        let _ = t.terminate_process(process, launch::ABORT_EXIT_CODE);
                        let _ = t.wait(process, TERMINATE_WAIT_MS);
                        Err(format!(
                            "ターゲットが {TARGET_EXIT_WAIT_MS} ms で終わらなかったので殺した（{other:?}）"
                        ))
                    }
                };
                let _ = t.close_handle(process);
                if !ok.cleanup_issues.is_empty() {
                    outcome = Err(format!("後始末の問題 {:?}", ok.cleanup_issues));
                }
                if t.count("ResumeThread") != 1 {
                    outcome = Err(format!("ResumeThread が {} 回", t.count("ResumeThread")));
                }
                outcome.and_then(|d| ledger_clean(&t).map(|_| d))
            }
            Err(f) => Err(format!("期待は成功、実際は {f:?}")),
        };
        report.check("行 9 成功", result);
    }

    let removed = tmp.remove();
    report.check(
        "一時フォルダを消した",
        removed.map(|_| format!("{} を消した", tmp.0.display())),
    );

    println!(
        "自己検査: {} 本のうち {} 本が通った",
        report.total, report.passed
    );
    Ok(if report.passed == report.total { 0 } else { 1 })
}

pub fn target(dll: &Path) -> i32 {
    target_verdict(
        real::module_loaded(dll),
        std::env::var_os(SELFTEST_ENV).as_deref(),
        std::env::var_os("SystemRoot").is_some(),
    )
}

/// 的の判定（pure）。DLL が居ない → `TARGET_MISSING`、渡した変数が無い・違う → `TARGET_ENV_MISSING`、
/// 親の環境が無い → `TARGET_PARENT_ENV_LOST`。
fn target_verdict(loaded: bool, env: Option<&std::ffi::OsStr>, system_root: bool) -> i32 {
    if !loaded {
        TARGET_MISSING
    } else if env != Some(std::ffi::OsStr::new(SELFTEST_ENV_VALUE)) {
        TARGET_ENV_MISSING
    } else if !system_root {
        TARGET_PARENT_ENV_LOST
    } else {
        TARGET_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn target_verdict_needs_all_three() {
        let v = Some(OsStr::new(SELFTEST_ENV_VALUE));
        assert_eq!(target_verdict(true, v, true), TARGET_OK);
        assert_eq!(target_verdict(false, v, true), TARGET_MISSING);
        assert_eq!(target_verdict(true, None, true), TARGET_ENV_MISSING);
        assert_eq!(
            target_verdict(true, Some(OsStr::new("row9")), true),
            TARGET_ENV_MISSING
        );
        assert_eq!(target_verdict(true, v, false), TARGET_PARENT_ENV_LOST);
    }
}
