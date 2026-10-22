//! `[pacing] vsync`: ゲームが custom.exe の「垂直同期を取らない」を読む 2 か所（`call 0x42ed50`。起動の設定の読み込み `0x42FB4D`・
//! 起動の計測の終わりの判定 `0x42F2AD`）を自前の入口へ向け、答えを ini の値で替える（`higanr_core::pacing::Vsync::vsync_off`）。
//! 保存されている値 `[0x4B3550]` には書かないので、`th09.cfg` の書き出しは custom.exe の値のまま。アドレスと期待バイト列は `th09_addr.rs` の「フルスクリーンの垂直同期」の節。
//!
//! 書くのは DllMain の中（`win::on_process_attach`。主スレッドは設定の読み込みまで走っていない）。ローダーロックの下なので ini はここで読まず、
//! 入口が最初に呼ばれたとき（`0x42FB4D`。起動の途中の主スレッド）に 1 回だけ読む。2 行のどちらかが合わなければ 1 バイトも書かない。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "vsync_answer_win.rs は 32 ビット（i686）専用（ゲームのアドレスと cdecl の呼びを前提にしている）"
);

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use higanr_core::pacing::{self as pace, Vsync};
use higanr_core::patch;
use windows_sys::Win32::Foundation::HMODULE;

use crate::hook::{HookEnv, WriteDone};
use crate::hook_win::RealHook;
use crate::script_win::RealScriptHost;
use crate::session::ScriptHost;
use crate::sites::SiteDef;
use crate::th09_addr::{
    D3D8_DEVICE_PTR, VSYNC_ANSWER_CALL_AT, VSYNC_ANSWER_SITES, VSYNC_FLAG_FN, VSYNC_OFF_BIT,
};
use crate::win::module_path;

static MODULE: AtomicUsize = AtomicUsize::new(0);
/// ini の `[pacing] vsync`（初めて呼ばれたときに 1 回だけ読む）。
static SETTING: OnceLock<Vsync> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static TEST_SETTING: std::cell::Cell<Option<Vsync>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
static TEST_WORD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn emit(line: String, detail_only: bool) {
    let line = format!("垂直同期: {line}");
    #[cfg(test)]
    {
        let _ = detail_only;
        TEST_LINES.with(|l| l.borrow_mut().push(line));
    }
    #[cfg(not(test))]
    if detail_only {
        crate::main_log::push_detail(line);
    } else {
        crate::main_log::push(line);
    }
}

/// 2 か所の `call 0x42ed50` を自前の入口へ向ける。**DllMain から呼ぶ**。失敗してもロードは止めない（custom.exe の設定のまま）。
pub fn install(module: HMODULE) {
    MODULE.store(module as usize, Ordering::Relaxed);
    let r = catch_unwind(AssertUnwindSafe(|| {
        // 他のスレッドを止めずに書くので、主スレッドが起動の設定の読み込みへ来る前（デバイスがまだ無い）でなければ書かない。
        // 計測の終わりの判定 `0x42F2AD` は、プロセスの中でデバイスを作り直すたびにまた走る。
        let mut env = RealHook::new();
        let mut cell = [0u8; 4];
        if !env.read_code(D3D8_DEVICE_PTR, &mut cell) {
            return Err(format!("{D3D8_DEVICE_PTR:#010X} が読み取り不能"));
        }
        if u32::from_le_bytes(cell) != 0 {
            return Err("デバイスが既に存在（ゲームの実行後の注入）".to_string());
        }
        install_at(&mut env, VSYNC_ANSWER_SITES, entry_address())
    }));
    match r {
        Ok(Ok(s)) => emit(format!("設置: {s}"), true),
        Ok(Err(why)) => emit(
            format!("設置: 非差し替え（[pacing] vsync は効かず custom.exe の設定のまま）: {why}"),
            false,
        ),
        Err(_) => emit(
            "設置: パニックを受けました（書いたかどうかは分からない）。".to_string(),
            false,
        ),
    }
}

fn entry_address() -> u32 {
    vsync_answer_entry as *const () as usize as u32
}

/// 照合と書き込みに使うコードの読み書き（本物は `RealHook`。検査は 2 行が収まる大きさの偽物）。
trait CodeMem {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool;
    fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32>;
}

impl<T: HookEnv> CodeMem for T {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        self.read_code(addr, buf)
    }
    fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
        self.write_code(addr, bytes)
    }
}

/// 全部の行を読んで照合し、全部合ったときだけ各行の `call` の 5 バイトを書く。書いた後に読み直して検証する。
fn install_at(env: &mut dyn CodeMem, sites: &[SiteDef], entry: u32) -> Result<String, String> {
    for s in sites {
        let mut seen = vec![0u8; s.expected.len()];
        if !env.read(s.addr, &mut seen) {
            return Err(format!(
                "{:#010X} から {} バイトの読み取り不能（th09.exe のないプロセスの可能性）",
                s.addr,
                seen.len()
            ));
        }
        patch::verify(&seen, s.expected).map_err(|e| {
            format!(
                "{:#010X} の命令が期待と不一致（{e:?}）。別のバージョンの th09.exe か、このフック地点を書き換えるツールがあります",
                s.addr
            )
        })?;
    }
    let mut done_all = true;
    for (i, &at) in VSYNC_ANSWER_CALL_AT[..sites.len()].iter().enumerate() {
        let new = patch::call_rel32(at, entry);
        let mut back = [0u8; 5];
        let failed = match env.write(at, &new) {
            Err(code) => Some(format!(
                "{at:#010X} へ書き込み不能（GetLastError = {code}）"
            )),
            Ok(done) => {
                done_all &= done.protect_restored && done.flushed;
                (!env.read(at, &mut back) || back != new)
                    .then(|| format!("{at:#010X} へ書いた後の読み直しが不一致: {back:02X?}"))
            }
        };
        if let Some(why) = failed {
            // 前の行に書いた call を元のバイト列へ戻す（片方だけ替わった形を残さない）。
            let mut undone = true;
            for (p, &pat) in sites[..i].iter().zip(&VSYNC_ANSWER_CALL_AT) {
                let off = (pat - p.addr) as usize;
                undone &= env.write(pat, &p.expected[off..off + 5]).is_ok();
            }
            return Err(format!("{why}（前の行は元に戻しました: {undone}）"));
        }
    }
    Ok(format!(
        "{} の call {VSYNC_FLAG_FN:#010X} を入口 {entry:#010X} への call に差し替えました（保護を戻せて命令キャッシュを流せた: {done_all}）",
        VSYNC_ANSWER_CALL_AT[..sites.len()]
            .iter()
            .map(|a| format!("{a:#010X}"))
            .collect::<Vec<_>>()
            .join("・")
    ))
}

fn setting() -> Vsync {
    #[cfg(test)]
    if let Some(f) = TEST_SETTING.with(std::cell::Cell::get) {
        return f;
    }
    *SETTING.get_or_init(load_setting)
}

fn load_setting() -> Vsync {
    let module = MODULE.load(Ordering::Relaxed) as HMODULE;
    let parent = |p: Option<PathBuf>| p.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    let mut host = RealScriptHost {
        dll_dir: parent(module_path(module)),
        run_path: None,
        exe_dir: parent(module_path(core::ptr::null_mut())),
    };
    let text = match host.read_net_ini() {
        Ok(Some(t)) => t,
        Ok(None) => return pace::DEFAULT_VSYNC,
        Err(why) => {
            emit(
                format!("ini 読み込み不能（{why}）。custom.exe の設定のままです。"),
                false,
            );
            return pace::DEFAULT_VSYNC;
        }
    };
    // `read_net_ini` は読めない行を捨てた本文を返す（捨てた行の注はペースの側が本ログに出す）。
    match higanr_core::ini::parse(&text).map_err(|e| e.to_string()) {
        Ok(ini) => match pace::mode_from_ini(&ini) {
            Ok(c) => c.vsync,
            Err(e) => {
                emit(format!("{e}。custom.exe の設定のままです。"), false);
                pace::DEFAULT_VSYNC
            }
        },
        Err(e) => {
            emit(
                format!("ini の書き方が違います（{e}）。custom.exe の設定のままです。"),
                false,
            );
            pace::DEFAULT_VSYNC
        }
    }
}

fn saved_word_addr() -> u32 {
    #[cfg(test)]
    {
        &TEST_WORD as *const _ as usize as u32
    }
    #[cfg(not(test))]
    {
        crate::th09_addr::VSYNC_OFF_WORD
    }
}

/// 答えの行（1 起動に 1 回。保存値と違う答えのときは簡潔にも出す）。
static LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn note_answer(f: Vsync, saved: bool, answer: bool) {
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    let word = |b: bool| if b { "取らない" } else { "取る" };
    emit(
        format!(
            "[pacing] vsync = {}。custom.exe の保存値は「{}」、ゲームへの答えは「{}」",
            f.name(),
            word(saved),
            word(answer)
        ),
        answer == saved,
    );
}

/// 差し替えた呼びの入口（cdecl、引数なし。元の `0x42ED50` は ecx = Supervisor の this で、どちらの地点でも `0x4B3100`）。
/// 0 か 1 を返す。ebx・esi・edi・ebp は cdecl の約束で保たれ、ecx・edx は呼んだ後に読まれない。FPU には触らない。
/// ini の読みとログは最初の 1 回だけで、パニックは受けて保存値のままの答えを返す。
extern "C" fn vsync_answer_entry() -> u32 {
    // SAFETY: `[0x4B3550]` は Supervisor の中の 4 バイト（.data。読める）。元の関数も同じ語を読む。
    let word = unsafe { core::ptr::read_volatile(saved_word_addr() as usize as *const u32) };
    let saved = word & VSYNC_OFF_BIT != 0;
    let r = catch_unwind(AssertUnwindSafe(|| {
        let f = setting();
        let answer = f.vsync_off(saved);
        note_answer(f, saved, answer);
        answer
    }));
    u32::from(r.unwrap_or(saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::testkit::{
        AFTER, PRE_ESP, REAL_HOOK_TESTS, REG_EBP, REG_EBX, REG_EDI, call_with_regs, fake_this_addr,
        load,
    };

    fn lines() -> Vec<String> {
        TEST_LINES.with(|l| l.borrow().clone())
    }

    /// 答えの表は game=保存値・on=0・off=1。bit 0x20 以外を見ない。ebx / esi / edi / ebp / esp を変えない。
    #[test]
    fn entry_answers_by_the_setting_and_never_writes_the_saved_word() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entry_address();
        for f in Vsync::ALL {
            for word in [0u32, 0x20, 0x24, 0x04, 0xFFFF_FFDF, u32::MAX] {
                TEST_SETTING.with(|s| s.set(Some(f)));
                TEST_WORD.store(word, Ordering::SeqCst);
                call_with_regs(entry);
                let a = load(&AFTER);
                let want = match f {
                    Vsync::Game => u32::from(word & 0x20 != 0),
                    Vsync::On => 0,
                    Vsync::Off => 1,
                };
                assert_eq!(a[0], want, "{} / {word:#X}", f.name());
                assert_eq!(TEST_WORD.load(Ordering::SeqCst), word, "保存値を書いた");
                assert_eq!(a[3], REG_EBX);
                assert_eq!(a[4], fake_this_addr());
                assert_eq!(a[5], REG_EDI);
                assert_eq!(a[6], REG_EBP);
                assert_eq!(a[7], PRE_ESP.load(Ordering::SeqCst));
            }
        }
        TEST_SETTING.with(|s| s.set(None));
    }

    #[test]
    fn the_answer_is_logged_once() {
        let _g = REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        LOGGED.store(false, Ordering::SeqCst);
        TEST_LINES.with(|l| l.borrow_mut().clear());
        TEST_SETTING.with(|s| s.set(Some(Vsync::On)));
        TEST_WORD.store(0x24, Ordering::SeqCst);
        call_with_regs(entry_address());
        call_with_regs(entry_address());
        let l = lines();
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(
            l[0].contains("vsync = on")
                && l[0].contains("保存値は「取らない」")
                && l[0].contains("答えは「取る」"),
            "{l:?}"
        );
        TEST_SETTING.with(|s| s.set(None));
    }

    #[test]
    fn install_writes_both_calls_only_when_both_rows_match() {
        struct Mem {
            base: u32,
            mem: Vec<u8>,
            writes: usize,
            fail_at: Option<u32>,
        }
        impl CodeMem for Mem {
            fn read(&mut self, addr: u32, buf: &mut [u8]) -> bool {
                let Some(o) = addr.checked_sub(self.base).map(|o| o as usize) else {
                    return false;
                };
                match self.mem.get(o..o + buf.len()) {
                    Some(s) => {
                        buf.copy_from_slice(s);
                        true
                    }
                    None => false,
                }
            }
            fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
                if self.fail_at == Some(addr) {
                    return Err(5);
                }
                let o = (addr - self.base) as usize;
                self.mem[o..o + bytes.len()].copy_from_slice(bytes);
                self.writes += 1;
                Ok(WriteDone {
                    protect_restored: true,
                    flushed: true,
                })
            }
        }
        let lo = VSYNC_ANSWER_SITES.iter().map(|s| s.addr).min().unwrap();
        let hi = VSYNC_ANSWER_SITES
            .iter()
            .map(|s| s.addr + s.expected.len() as u32)
            .max()
            .unwrap();
        let fresh = || {
            let mut f = Mem {
                base: lo,
                mem: vec![0xCC; (hi - lo) as usize],
                writes: 0,
                fail_at: None,
            };
            for s in VSYNC_ANSWER_SITES {
                let o = (s.addr - lo) as usize;
                f.mem[o..o + s.expected.len()].copy_from_slice(s.expected);
            }
            f
        };
        let entry = 0x1000_2000;
        for s in VSYNC_ANSWER_SITES {
            for i in 0..s.expected.len() {
                let mut f = fresh();
                f.mem[(s.addr - lo) as usize + i] ^= 0xFF;
                let before = f.mem.clone();
                assert!(
                    install_at(&mut f, VSYNC_ANSWER_SITES, entry).is_err(),
                    "{} {i}",
                    s.name
                );
                assert_eq!(f.writes, 0, "{} {i}", s.name);
                assert_eq!(f.mem, before);
            }
        }
        let mut f = fresh();
        let before = f.mem.clone();
        let msg = install_at(&mut f, VSYNC_ANSWER_SITES, entry).expect("全部合えば書く");
        assert!(msg.contains("差し替えました"), "{msg}");
        assert_eq!(f.writes, 2);
        let mut changed = Vec::new();
        for (i, (a, b)) in before.iter().zip(f.mem.iter()).enumerate() {
            if a != b {
                changed.push(lo + i as u32);
            }
        }
        for at in VSYNC_ANSWER_CALL_AT {
            let o = (at - lo) as usize;
            let call: [u8; 5] = f.mem[o..o + 5].try_into().unwrap();
            assert_eq!(patch::call_target(at, &call), Some(entry));
        }
        assert!(
            changed.iter().all(|a| VSYNC_ANSWER_CALL_AT
                .iter()
                .any(|c| (*c + 1..*c + 5).contains(a))),
            "call の rel32 の外を書いた: {changed:X?}"
        );
        // 2 か所目の書き込みが失敗したら、1 か所目も元のバイト列に戻っている（片方だけ替わった形を残さない）。
        let mut f = fresh();
        f.fail_at = Some(VSYNC_ANSWER_CALL_AT[1]);
        let before = f.mem.clone();
        let e = install_at(&mut f, VSYNC_ANSWER_SITES, entry).unwrap_err();
        assert!(e.contains("前の行は元に戻しました: true"), "{e}");
        assert_eq!(f.mem, before);
    }
}
