//! E1-1 の入口（素のアセンブリ）と、書き出しのスレッド・ファイルの層（unsafe は入口のアセンブリだけ）。表の形・集計・設置の手順は
//! `alloc_count.rs`（pure）。
//!
//! - 入口 5 つ（`alloc_entry_0`〜`alloc_entry_4`。`th09_addr::ALLOC_SITES` の並び）。関数の先頭に書いた `E9` から飛んでくる。
//!   `[esp]`（呼び出し元の戻りアドレス）を鍵に、スレッドの別ごとの表へ数えてから、その入口のトランポリン（盗んだ命令 ＋ 元の続きへの
//!   `E9`）へ飛ぶ。**ゲームのスレッドで Rust のコードは走らない**（呼ぶ関数も無い）。
//! - 書き出しのスレッド: 1 秒ごとに表を読み、ゲームの場面の値を読み、`_alloc_sec.csv` と `_alloc_calls.csv` に 1 秒ぶんを足す。
//!   終わりに `_alloc_callers.csv` と `_alloc_table.csv`。ファイルは本ログの隣（`logs\`）に、本ログの名前の先頭で `create_new`
//!   （既存を上書きしない）。
//!
//! ゲームには、`alloc_count::install_all` が照合を通した 5 地点の detour（6〜7 バイト）のほかは何も書かない。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "alloc_win.rs は 32 ビット（i686）専用（入口のアセンブリが 32 ビットのアドレスと fs:[0x24] を前提にしている）"
);

use core::sync::atomic::{AtomicBool, Ordering};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::alloc_count::{
    self, ALLOC_MAIN_TID, ALLOC_OVERFLOW, ALLOC_TABLES, ALLOC_TOTALS, ALLOC_TRAMPS, AllocHost,
    AllocSettings, AllocStep, CALLERS_HEADER, CALLS_HEADER, CLASS_COUNT, Collector, HASH_MUL,
    HASH_SHIFT, PROBES, SITE_COUNT, SLOT_MASK, SceneObs, TABLE_HEADER, TABLE_SHIFT,
};
use crate::names::side_file_name;
use crate::stub::{self, TEB_THREAD_ID};
use crate::th09_addr::{
    ALLOC_SITES, GLOBAL_STATE, LOADING, MATCH_END_MENU, P1_WINS, P2_WINS, PAUSE, SUP_SCENE_CUR,
    SUPERVISOR,
};
use crate::ticks::SlotRead;
use crate::win::read_own_memory;

// SAFETY（`#[unsafe(naked)]`。下の 5 つの入口に共通）: 本体は `naked_asm!` だけ。関数の先頭（`call` の直後で、`[esp]` が戻りアドレス）に
// 書いた `E9` から飛んでくる。
// - eax・ecx・edx・ebx・esi・edi を push し、最後に逆の順で pop してから、その入口のトランポリンへ `jmp` する（戻りアドレスを積まない）。
//   esp は push / pop で動くだけで `jmp` の時点では入った時と同じ。ecx（包みの this `0x4AD018`）も戻すので、トランポリンが実行する
//   盗んだ命令と元の続きは、入った時と同じレジスタとスタックを見る。ebp には書かない。
// - フラグは戻さない: 関数の先頭では呼ぶ側はフラグを当てにできず（保たれるのは DF = 0 だけで、入口は DF を変えない）、
//   5 つの盗んだ命令と直後の数命令はフラグを読まない。
// - 書くのは DLL の静的変数（ALLOC_TOTALS・ALLOC_OVERFLOW・ALLOC_TABLES の 1 枠）だけ。ゲームのメモリは読まない
//   （`[esp + 24]` は自分のスレッドのスタック、`fs:[0x24]` は自分の TEB）。
// - 表の番号 edi = 地点 × 2 ＋ スレッドの別（0..TABLE_COUNT）、枠の番号 edx は `and SLOT_MASK` で表の中に収まり、
//   `[ebx + edx*8]` は自分の表の中だけ。鍵（戻りアドレス）は 0 にならない。空きの枠は `lock cmpxchg` で取り、たどるのは PROBES 枠までなのでループは必ず終わる。
// - トランポリンのアドレスは、この入口へ向く `E9` を書く前に置かれる（`alloc_count::SiteDetourEnv::set_trampoline`）。
// - 静的変数は絶対アドレスで指す（再配置で直る）。ラベルは 2〜6（0 と 1 だけの名前は Intel 記法で 2 進の数と紛れる）。
macro_rules! alloc_entry {
    ($name:ident, $site:expr) => {
        /// E1-1 の入口（`ALLOC_SITES` の地点の番号の行）。Rust からは呼ばない（アドレスを取るだけ）。
        #[unsafe(naked)]
        pub extern "C" fn $name() {
            core::arch::naked_asm!(
                "push eax",
                "push ecx",
                "push edx",
                "push ebx",
                "push esi",
                "push edi",
                // esi = 呼び出し元（6 つの push の下の戻りアドレス）。
                "mov esi, dword ptr [esp + 24]",
                // edi = 表の番号（地点 × 2 ＋ スレッドの別。ゲームのスレッドなら 0、それ以外なら 1）。
                "mov edi, {site2}",
                "mov eax, dword ptr fs:[{teb_tid}]",
                "cmp eax, dword ptr [{main_tid}]",
                "je 2f",
                "inc edi",
                "2:",
                "lock inc dword ptr [{totals} + edi*4]",
                // ebx = 表の先頭。edx = 最初に見る枠（戻りアドレス × HASH_MUL の上位 SLOT_BITS ビット）。
                "mov ebx, edi",
                "shl ebx, {tshift}",
                "lea ebx, [{tables} + ebx]",
                "imul edx, esi, {mul}",
                "shr edx, {hshift}",
                "mov ecx, {probes}",
                "3:",
                "mov eax, dword ptr [ebx + edx*8]",
                "cmp eax, esi",
                "je 5f",
                "test eax, eax",
                "jnz 4f",
                // 空き（eax = 0）: 鍵を置く。取れたら ZF = 1。別のスレッドが先に置いたら eax = その鍵。
                "lock cmpxchg dword ptr [ebx + edx*8], esi",
                "je 5f",
                "cmp eax, esi",
                "je 5f",
                "4:",
                "inc edx",
                "and edx, {smask}",
                "dec ecx",
                "jnz 3b",
                // PROBES 枠たどっても入れない: 溢れに数える。
                "lock inc dword ptr [{overflow} + edi*4]",
                "jmp 6f",
                "5:",
                "lock inc dword ptr [ebx + edx*8 + 4]",
                "6:",
                "pop edi",
                "pop esi",
                "pop ebx",
                "pop edx",
                "pop ecx",
                "pop eax",
                "jmp dword ptr [{tramps} + {toff}]",
                site2 = const $site * CLASS_COUNT,
                toff = const $site * 4,
                teb_tid = const TEB_THREAD_ID,
                main_tid = sym ALLOC_MAIN_TID,
                totals = sym ALLOC_TOTALS,
                overflow = sym ALLOC_OVERFLOW,
                tables = sym ALLOC_TABLES,
                tramps = sym ALLOC_TRAMPS,
                tshift = const TABLE_SHIFT,
                mul = const HASH_MUL,
                hshift = const HASH_SHIFT,
                probes = const PROBES,
                smask = const SLOT_MASK,
            )
        }
    };
}

alloc_entry!(alloc_entry_0, 0);
alloc_entry!(alloc_entry_1, 1);
alloc_entry!(alloc_entry_2, 2);
alloc_entry!(alloc_entry_3, 3);
alloc_entry!(alloc_entry_4, 4);

const _: () = assert!(SITE_COUNT == 5);

/// 入口 `site` のアドレス（範囲の外は 0）。
pub fn entry_address(site: usize) -> u32 {
    let f: extern "C" fn() = match site {
        0 => alloc_entry_0,
        1 => alloc_entry_1,
        2 => alloc_entry_2,
        3 => alloc_entry_3,
        4 => alloc_entry_4,
        _ => return 0,
    };
    f as *const () as usize as u32
}

// ---- 本物の AllocHost ----

pub const SEC_SUFFIX: &str = "_alloc_sec.csv";
pub const CALLS_SUFFIX: &str = "_alloc_calls.csv";
pub const CALLERS_SUFFIX: &str = "_alloc_callers.csv";
pub const TABLE_SUFFIX: &str = "_alloc_table.csv";

struct Outputs {
    sec: BufWriter<File>,
    calls: BufWriter<File>,
    callers: BufWriter<File>,
    table: BufWriter<File>,
}

static STOP: AtomicBool = AtomicBool::new(false);
static WRITER: Mutex<Option<JoinHandle<Vec<String>>>> = Mutex::new(None);

/// ゲームのスレッドの ID を tick の記録から探す上限（ゲームが動いていれば 1 フレーム（約 16.7 ms）で記録が出る）。
const TID_WAIT_MS: u64 = 2000;

/// ゲームのスレッドの ID を、入力のフックが書く tick の記録から `wait_ms` の間探す（見つからなければ `None`）。E1-1 の数えと
/// rollback の設置が共に使う。
pub fn wait_game_thread_id(wait_ms: u64) -> Option<u32> {
    let t0 = Instant::now();
    loop {
        let next = stub::next_seq();
        if next > 0
            && let SlotRead::Record(s) = stub::read_slot(next.wrapping_sub(1))
            && s.rec.tid != 0
        {
            return Some(s.rec.tid);
        }
        if t0.elapsed() >= Duration::from_millis(wait_ms) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub struct RealAllocHost {
    ini: Option<Result<Option<String>, String>>,
    run_path: Option<PathBuf>,
    outputs: Option<Outputs>,
}

fn open_new(dir: &Path, log_name: &str, suffix: &str) -> Result<(BufWriter<File>, String), String> {
    let name = side_file_name(log_name, suffix);
    let path = dir.join(&name);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map(|f| (BufWriter::new(f), name))
        .map_err(|e| format!("{} を開けない: {e}", path.display()))
}

impl AllocHost for RealAllocHost {
    fn read_ini(&mut self) -> Result<Option<String>, String> {
        self.ini.take().unwrap_or(Ok(None))
    }

    fn game_thread_id(&mut self) -> Option<u32> {
        wait_game_thread_id(TID_WAIT_MS)
    }

    fn open_outputs(&mut self) -> Result<String, String> {
        let log = self
            .run_path
            .as_ref()
            .ok_or("本ログの置き場が決まっていない")?;
        let dir = log.parent().ok_or("本ログのフォルダが分からない")?;
        let log_name = log
            .file_name()
            .ok_or("本ログの名前が分からない")?
            .to_string_lossy()
            .into_owned();
        let (sec, n1) = open_new(dir, &log_name, SEC_SUFFIX)?;
        let (calls, n2) = open_new(dir, &log_name, CALLS_SUFFIX)?;
        let (callers, n3) = open_new(dir, &log_name, CALLERS_SUFFIX)?;
        let (table, n4) = open_new(dir, &log_name, TABLE_SUFFIX)?;
        self.outputs = Some(Outputs {
            sec,
            calls,
            callers,
            table,
        });
        Ok(format!("{} / {n1} / {n2} / {n3} / {n4}", dir.display()))
    }

    fn prepare_counters(&mut self, main_tid: u32) {
        alloc_count::reset_counters();
        ALLOC_MAIN_TID.store(main_tid, Ordering::SeqCst);
    }

    fn entry_address(&self, site: usize) -> u32 {
        entry_address(site)
    }

    fn set_trampoline(&mut self, site: usize, addr: u32) {
        if let Some(t) = ALLOC_TRAMPS.get(site) {
            t.store(addr, Ordering::SeqCst);
        }
    }

    fn start_writer(
        &mut self,
        s: &AllocSettings,
        installed: [bool; SITE_COUNT],
    ) -> Result<(), String> {
        let outs = self
            .outputs
            .take()
            .ok_or("書き出しのファイルを開いていない")?;
        let seconds = s.seconds;
        STOP.store(false, Ordering::SeqCst);
        let h = std::thread::Builder::new()
            .name("higanr_alloc_writer".to_string())
            .spawn(move || writer(outs, seconds, installed))
            .map_err(|e| format!("スレッドを起こせない: {e}"))?;
        match WRITER.lock() {
            Ok(mut g) => *g = Some(h),
            Err(p) => *p.into_inner() = Some(h),
        }
        Ok(())
    }

    fn finish(&mut self) -> Option<Vec<String>> {
        STOP.store(true, Ordering::SeqCst);
        let h = match WRITER.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        }?;
        Some(match h.join() {
            Ok(lines) => lines,
            Err(_) => vec![
                "書き出しのスレッドがパニックで終わった（書けた行までがファイルに残る）"
                    .to_string(),
            ],
        })
    }
}

/// `session::ScriptHost::alloc_count` の本物。`ini` は DLL のフォルダの `HiganRTool.ini` の本文、`run_path` は回ごとの名前のパス
/// （書き出しのファイルの名前の元）。
pub fn on_step(
    ini: Result<Option<String>, String>,
    run_path: Option<PathBuf>,
    step: AllocStep<'_>,
    out: &mut dyn Write,
) {
    let mut host = RealAllocHost {
        ini: Some(ini),
        run_path,
        outputs: None,
    };
    match step {
        AllocStep::Arm {
            hook_env,
            forbidden,
        } => {
            // E1-2（巻き戻しの実験）: ini に [rollback_probe] があればそちらを入れ、E1-1 の数えへは進まない（同じ地点を使う）。
            if let Some(Ok(Some(text))) = host.ini.as_ref()
                && !matches!(crate::rollback_probe::settings_from_text(text), Ok(None))
            {
                let text = text.clone();
                let tid = host.game_thread_id();
                let _ = crate::rollback_win::arm(&text, hook_env, forbidden, tid, out);
                return;
            }
            let _ = alloc_count::arm(&mut host, hook_env, ALLOC_SITES, forbidden, out);
        }
        AllocStep::Finish => {
            crate::rollback_win::finish(out);
            alloc_count::finish(&mut host, out)
        }
    }
}

// ---- 書き出しのスレッド ----

fn read_u32(addr: u32) -> Option<u32> {
    read_own_memory(addr, 4).and_then(|b| b.try_into().ok().map(u32::from_le_bytes))
}

fn read_u8(addr: u32) -> Option<u8> {
    read_own_memory(addr, 1).and_then(|b| b.first().copied())
}

/// ゲームの場面の値を読む（読めない頁は読まない）。どれも 4 バイト以下の 1 つの値なので、書いている最中でも前か後のどちらかの
/// 値が読める。
fn read_obs() -> SceneObs {
    SceneObs {
        scene: read_u32(SUPERVISOR + SUP_SCENE_CUR),
        loading: read_u32(LOADING),
        global: read_u32(GLOBAL_STATE),
        end_menu: read_u8(MATCH_END_MENU),
        pause: read_u8(PAUSE),
        wins: [read_u32(P1_WINS), read_u32(P2_WINS)],
        tick_seq: stub::next_seq(),
    }
}

const POLL_MS: u64 = 20;

fn put(w: &mut BufWriter<File>, line: &str, errors: &mut u32) {
    if writeln!(w, "{line}").is_err() {
        *errors += 1;
    }
}

fn writer(mut o: Outputs, seconds: u32, installed: [bool; SITE_COUNT]) -> Vec<String> {
    let t0 = Instant::now();
    let ms = || t0.elapsed().as_millis() as u64;
    let mut c = Collector::new(0, &read_obs(), alloc_count::snapshot(), installed);
    let mut errs = 0u32;
    put(&mut o.sec, &alloc_count::sec_header(ALLOC_SITES), &mut errs);
    put(&mut o.calls, CALLS_HEADER, &mut errs);
    let limit = seconds as u64 * 1000;
    let mut next = 1000u64;
    let mut last_end = 0u64;
    loop {
        let now = ms();
        let stop = STOP.load(Ordering::SeqCst);
        if now >= next || (stop && now > last_end) {
            let r = c.sample(now, &read_obs(), alloc_count::snapshot());
            put(&mut o.sec, &alloc_count::sec_line(r), &mut errs);
            for l in alloc_count::calls_lines(r, ALLOC_SITES) {
                put(&mut o.calls, &l, &mut errs);
            }
            // 1 秒ごとに流す（途中で DLL ごと落ちても、そこまでの行は残る）。
            if o.sec.flush().is_err() || o.calls.flush().is_err() {
                errs += 1;
            }
            last_end = now;
            // 遅れても期間は 1 つずつ閉じる（遅れた期間は長さが 1 秒に近くないので表に入らない）。
            while next <= now {
                next += 1000;
            }
        }
        if stop || now >= limit {
            break;
        }
        std::thread::sleep(Duration::from_millis(
            POLL_MS.min(next.saturating_sub(now)).max(1),
        ));
    }
    let last = alloc_count::snapshot();
    put(&mut o.callers, CALLERS_HEADER, &mut errs);
    for ((t, k), total) in c.caller_totals() {
        // 戻りアドレスの手前 6 バイト（呼んだ命令）。読めなければ unreadable と書く。
        let before = read_own_memory(k.wrapping_sub(6), 6).and_then(|b| b.try_into().ok());
        let line = alloc_count::callers_line(ALLOC_SITES, t, k, before, total);
        put(&mut o.callers, &line, &mut errs);
    }
    put(&mut o.table, TABLE_HEADER, &mut errs);
    for r in c.table() {
        put(
            &mut o.table,
            &alloc_count::table_line(&r, ALLOC_SITES),
            &mut errs,
        );
    }
    for w in [&mut o.sec, &mut o.calls, &mut o.callers, &mut o.table] {
        if w.flush().is_err() {
            errs += 1;
        }
    }
    let mut lines = alloc_count::summary_lines(ALLOC_SITES, &c, &last);
    lines.push(format!(
        "書き出しを終えた（{} ミリ秒。書けなかった行・流せなかった回 {errs}）",
        ms()
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_count::{
        SLOTS, Snapshot, TABLE_COUNT, home_slot, install_all, snapshot, table_index,
    };
    use crate::hook_win::RealHook;
    use crate::sites::SiteDef;
    use crate::stub::testkit::{
        AFTER, EXP_RET, PRE_ESP, REAL_HOOK_TESTS, REG_EAX, REG_EBP, REG_EBX, REG_ECX, REG_EDI,
        REG_EDX, SEEN, TARGET_HITS, call_with_regs, fake_this_addr, load, seen_target_addr,
    };
    use core::ffi::c_void;
    use core::sync::atomic::AtomicU32;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc,
        VirtualFree, VirtualProtect,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        REAL_HOOK_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn my_tid() -> u32 {
        // SAFETY: 引数の無い問い合わせ。
        unsafe { GetCurrentThreadId() }
    }

    fn clear() {
        alloc_count::reset_counters();
        ALLOC_MAIN_TID.store(0, Ordering::SeqCst);
        for t in ALLOC_TRAMPS.iter() {
            t.store(0, Ordering::SeqCst);
        }
    }

    struct Page {
        base: *mut c_void,
    }

    impl Page {
        fn build(len: usize, fill: impl FnOnce(u32) -> Vec<u8>) -> Self {
            let len = len.div_ceil(4096) * 4096;
            // SAFETY: 新しく確保する（アドレスは OS に任せる）。失敗は下で見る。
            let base = unsafe {
                VirtualAlloc(
                    core::ptr::null(),
                    len,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            assert!(!base.is_null(), "VirtualAlloc");
            let code = fill(base as usize as u32);
            assert!(code.len() <= len);
            for (i, &b) in code.iter().enumerate() {
                // SAFETY: いま確保した読み書きのできる頁の中（`code.len() <= len`）。
                unsafe { core::ptr::write_volatile((base as usize + i) as *mut u8, b) };
            }
            let mut old = 0;
            // SAFETY: いま確保した頁の保護を変えるだけ。
            let ok = unsafe { VirtualProtect(base, len, PAGE_EXECUTE_READ, &mut old) };
            assert_ne!(ok, 0);
            Page { base }
        }
        fn addr(&self) -> u32 {
            self.base as usize as u32
        }
        fn bytes(&self, off: usize, n: usize) -> Vec<u8> {
            read_own_memory(self.addr() + off as u32, n).expect("読める")
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            // SAFETY: `build` で確保した頁を 1 回だけ返す。この頁へ飛ぶものは検査の中にしか無い。
            unsafe { VirtualFree(self.base, 0, MEM_RELEASE) };
        }
    }

    fn table_of(s: &Snapshot, t: usize) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = s
            .slots
            .iter()
            .filter(|x| x.0 as usize == t)
            .map(|x| (x.1, x.2))
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn entries_keep_registers_and_count_the_return_address() {
        let _g = lock();
        for (site, tramp) in ALLOC_TRAMPS.iter().enumerate() {
            clear();
            ALLOC_MAIN_TID.store(my_tid(), Ordering::SeqCst);
            tramp.store(seen_target_addr(), Ordering::SeqCst);
            let hits0 = TARGET_HITS.load(Ordering::SeqCst);
            call_with_regs(entry_address(site));
            assert_eq!(
                TARGET_HITS.load(Ordering::SeqCst),
                hits0 + 1,
                "地点 {site}: トランポリンへ飛んでいない"
            );
            let seen = load(&SEEN);
            let after = load(&AFTER);
            assert_eq!(
                &seen[..7],
                &[
                    REG_EAX,
                    REG_ECX,
                    REG_EDX,
                    REG_EBX,
                    fake_this_addr(),
                    REG_EDI,
                    REG_EBP
                ],
                "地点 {site}: トランポリンが見たレジスタ"
            );
            assert_eq!(
                seen[7],
                EXP_RET.load(Ordering::SeqCst),
                "地点 {site}: 戻りアドレス"
            );
            assert_eq!(
                seen[8],
                PRE_ESP.load(Ordering::SeqCst) - 4,
                "地点 {site}: esp"
            );
            assert_eq!(after[3], REG_EBX);
            let s = snapshot();
            let ret = EXP_RET.load(Ordering::SeqCst);
            for t in 0..TABLE_COUNT {
                let want: Vec<(u32, u32)> = if t == table_index(site, 0) {
                    vec![(ret, 1)]
                } else {
                    vec![]
                };
                assert_eq!(table_of(&s, t), want, "地点 {site} 表 {t}");
                assert_eq!(
                    s.totals[t],
                    u32::from(t == table_index(site, 0)),
                    "地点 {site} 表 {t} の総数"
                );
            }
            let base = (table_index(site, 0) * SLOTS + home_slot(ret) as usize) * 2;
            assert_eq!(
                ALLOC_TABLES.words[base].load(Ordering::SeqCst),
                ret,
                "地点 {site}: 最初に見る枠が home_slot と違う"
            );
        }
        clear();
    }

    /// `call [slot]`（`FF 15 abs32`）を `n` 個並べて最後に `ret` を置いた頁（`n` 個の違う戻りアドレス = 頁 + 6(i+1) から
    /// 入口を呼ぶ）と、トランポリンの代わりの `ret` のアドレス。`slot` は頁の終わりに置き、入口のアドレスを入れる。
    fn call_page(entry: u32, n: usize) -> (Page, u32) {
        let ret_off = n * 6;
        let slot_off = ret_off + 4; // [call ×n][ret][int3 ×3][slot 4]
        let page = Page::build(slot_off + 4, |base| {
            let slot = (base + slot_off as u32).to_le_bytes();
            let mut code = Vec::with_capacity(slot_off + 4);
            for _ in 0..n {
                code.extend_from_slice(&[0xFF, 0x15]);
                code.extend_from_slice(&slot);
            }
            code.push(0xC3);
            code.extend_from_slice(&[0xCC; 3]);
            code.extend_from_slice(&entry.to_le_bytes());
            code
        });
        let ret = page.addr() + ret_off as u32;
        (page, ret)
    }

    fn run_page(p: &Page) {
        // SAFETY: 頁は `call [slot]` の列と `ret`。入口は全部のレジスタを戻してトランポリン（頁の `ret`）へ飛ぶので、各 `call` は次の
        // 命令へ戻る。
        let f: extern "C" fn() = unsafe { core::mem::transmute(p.addr() as usize) };
        f();
    }

    #[test]
    fn concurrent_counts_add_up_exactly() {
        let _g = lock();
        clear();
        const SITE: usize = 2;
        const CALLERS: usize = 8;
        const ROUNDS: u32 = 2000;
        const OTHERS: u32 = 4;
        let (page, ret) = call_page(entry_address(SITE), CALLERS);
        ALLOC_TRAMPS[SITE].store(ret, Ordering::SeqCst);
        ALLOC_MAIN_TID.store(my_tid(), Ordering::SeqCst);
        let start = std::sync::Arc::new(std::sync::Barrier::new(OTHERS as usize + 1));
        let addr = page.addr();
        let hs: Vec<_> = (0..OTHERS)
            .map(|_| {
                let b = start.clone();
                std::thread::spawn(move || {
                    b.wait();
                    // SAFETY: `run_page` と同じ（頁は検査の終わりまで生きている。join の後で返す）。
                    let f: extern "C" fn() = unsafe { core::mem::transmute(addr as usize) };
                    for _ in 0..ROUNDS {
                        f();
                    }
                })
            })
            .collect();
        start.wait();
        for _ in 0..ROUNDS {
            run_page(&page);
        }
        for h in hs {
            h.join().unwrap();
        }
        let s = snapshot();
        let calls: Vec<u32> = (0..CALLERS)
            .map(|i| page.addr() + (i as u32 + 1) * 6)
            .collect();
        let main = table_of(&s, table_index(SITE, 0));
        let other = table_of(&s, table_index(SITE, 1));
        assert_eq!(main, calls.iter().map(|&c| (c, ROUNDS)).collect::<Vec<_>>());
        assert_eq!(
            other,
            calls
                .iter()
                .map(|&c| (c, ROUNDS * OTHERS))
                .collect::<Vec<_>>()
        );
        assert_eq!(s.totals[table_index(SITE, 0)], ROUNDS * CALLERS as u32);
        assert_eq!(
            s.totals[table_index(SITE, 1)],
            ROUNDS * OTHERS * CALLERS as u32
        );
        assert_eq!(s.overflow, [0; TABLE_COUNT]);
        clear();
    }

    #[test]
    fn overflow_is_counted_when_the_table_is_full() {
        let _g = lock();
        clear();
        const SITE: usize = 4;
        const N: usize = 1100;
        let (page, ret) = call_page(entry_address(SITE), N);
        ALLOC_TRAMPS[SITE].store(ret, Ordering::SeqCst);
        ALLOC_MAIN_TID.store(my_tid(), Ordering::SeqCst);
        run_page(&page);
        let s = snapshot();
        let t = table_index(SITE, 0);
        let keys = table_of(&s, t);
        let sum: u32 = keys.iter().map(|k| k.1).sum();
        assert_eq!(s.totals[t], N as u32);
        assert!(keys.len() <= SLOTS);
        assert!(keys.iter().all(|k| k.1 == 1), "同じ鍵が 2 回数えられた");
        assert!(
            s.overflow[t] as usize >= N - SLOTS,
            "溢れ {} が {} より少ない",
            s.overflow[t],
            N - SLOTS
        );
        assert_eq!(sum + s.overflow[t], s.totals[t], "表の和 ＋ 溢れ ≠ 総数");
        for other in 0..TABLE_COUNT {
            if other != t {
                assert!(table_of(&s, other).is_empty());
                assert_eq!(s.overflow[other], 0);
            }
        }
        clear();
    }

    static ABS: AtomicU32 = AtomicU32::new(0x1234_5678);

    /// 5 つの合成の関数（どれも `ALLOC_SITES` の各行と同じ形の先頭）と、その表の行。
    /// - 0 / 1: `push ebp; mov ebp,esp; mov eax,[ebp+8]` ＋ `add eax,7; pop ebp; ret` → 引数 + 7
    /// - 2: `push [ABS]` ＋ `pop eax; ret` → ABS の値
    /// - 3: `push 1; push [esp+8]` ＋ `pop eax; pop ecx; ret` → 引数（esp 相対の盗んだ命令が同じアドレスを読むか）
    /// - 4: `push 0xC; push imm32` ＋ `pop eax; pop ecx; ret` → imm32
    fn synthetic_functions() -> (Page, Vec<SiteDef>) {
        let abs = (&ABS as *const AtomicU32 as usize as u32).to_le_bytes();
        let imm: u32 = 0x0049_53F0;
        let heads: [Vec<u8>; 5] = [
            vec![0x55, 0x8B, 0xEC, 0x8B, 0x45, 0x08],
            vec![0x55, 0x8B, 0xEC, 0x8B, 0x45, 0x08],
            vec![0xFF, 0x35, abs[0], abs[1], abs[2], abs[3]],
            vec![0x6A, 0x01, 0xFF, 0x74, 0x24, 0x08],
            {
                let mut v = vec![0x6A, 0x0C, 0x68];
                v.extend_from_slice(&imm.to_le_bytes());
                v
            },
        ];
        let tails: [&[u8]; 5] = [
            &[0x83, 0xC0, 0x07, 0x5D, 0xC3],
            &[0x83, 0xC0, 0x07, 0x5D, 0xC3],
            &[0x58, 0xC3],
            &[0x58, 0x59, 0xC3],
            &[0x58, 0x59, 0xC3],
        ];
        let mut code = vec![0xCCu8; 5 * 0x40];
        for i in 0..5 {
            let at = i * 0x40;
            code[at..at + heads[i].len()].copy_from_slice(&heads[i]);
            code[at + heads[i].len()..at + heads[i].len() + tails[i].len()]
                .copy_from_slice(tails[i]);
        }
        let page = Page::build(code.len(), |_| code);
        let defs = (0..5)
            .map(|i| SiteDef {
                name: ALLOC_SITES[i].name,
                addr: page.addr() + (i * 0x40) as u32,
                expected: Box::leak(heads[i].clone().into_boxed_slice()),
                kind: ALLOC_SITES[i].kind,
                source: "検査の領域",
            })
            .collect();
        (page, defs)
    }

    /// 呼ぶ `call` 命令をこの関数の中の 1 か所に限る（畳み込まれると呼び出し元が増える）。
    #[inline(never)]
    fn call1(addr: u32, x: u32) -> u32 {
        // SAFETY: `synthetic_functions` の関数（cdecl、引数 1 つ、戻り値 u32）。detour の後も同じ働き（検査が見る）。
        let f: extern "C" fn(u32) -> u32 = unsafe { core::mem::transmute(addr as usize) };
        f(x)
    }

    #[test]
    fn real_install_all_keeps_each_function_working() {
        let _g = lock();
        clear();
        let (page, defs) = synthetic_functions();
        let before: Vec<u32> = (0..5).map(|i| call1(defs[i].addr, 35)).collect();
        assert_eq!(before, vec![42, 42, 0x1234_5678, 35, 0x0049_53F0]);
        let draw_tramp = crate::draw::DRAW_TRAMPOLINE.load(Ordering::SeqCst);
        let mut env = RealHook::new();
        let mut host = RealAllocHost {
            ini: None,
            run_path: None,
            outputs: None,
        };
        ALLOC_MAIN_TID.store(my_tid(), Ordering::SeqCst);
        let mut out = Vec::new();
        let written = install_all(&mut env, &mut host, &defs, &[], &mut out);
        let log = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(written, [true; SITE_COUNT], "{log}");
        assert_eq!(
            crate::draw::DRAW_TRAMPOLINE.load(Ordering::SeqCst),
            draw_tramp,
            "描画の入口のトランポリンを書き換えた"
        );
        for (i, d) in defs.iter().enumerate() {
            let b = page.bytes(i * 0x40, d.expected.len());
            let head: [u8; 5] = b[..5].try_into().unwrap();
            assert_eq!(
                higanr_core::patch::jmp_target(d.addr, &head),
                Some(entry_address(i)),
                "地点 {i} の E9"
            );
            assert!(b[5..].iter().all(|&x| x == 0x90), "地点 {i} の NOP");
            assert_ne!(ALLOC_TRAMPS[i].load(Ordering::SeqCst), 0);
        }
        for round in 1..=3u32 {
            let after: Vec<u32> = (0..5).map(|i| call1(defs[i].addr, 35)).collect();
            assert_eq!(after, before, "detour の後で働きが変わった（{round} 回目）");
        }
        let s = snapshot();
        for site in 0..SITE_COUNT {
            let t = table_index(site, 0);
            assert_eq!(s.totals[t], 3, "地点 {site} の総数");
            let keys = table_of(&s, t);
            assert_eq!(
                keys.len(),
                1,
                "地点 {site} の呼び出し元の数（call1 の中の 1 か所）"
            );
            assert_eq!(keys[0].1, 3);
        }
        for t in ALLOC_TRAMPS.iter() {
            let a = t.load(Ordering::SeqCst);
            // SAFETY: `RealHook::alloc_trampoline` が確保した頁。この頁へ飛ぶ E9 は下の `drop(page)` で消える頁の中だけ。
            unsafe { VirtualFree(a as usize as *mut c_void, 0, MEM_RELEASE) };
        }
        drop(page);
        clear();
    }

    #[test]
    fn real_install_all_writes_nothing_on_one_mismatch() {
        let _g = lock();
        clear();
        let (page, mut defs) = synthetic_functions();
        let before: Vec<Vec<u8>> = (0..5).map(|i| page.bytes(i * 0x40, 8)).collect();
        let mut bad = defs[3].expected.to_vec();
        bad[5] = 0x0C;
        defs[3] = SiteDef {
            expected: Box::leak(bad.into_boxed_slice()),
            ..defs[3]
        };
        let mut env = RealHook::new();
        let mut host = RealAllocHost {
            ini: None,
            run_path: None,
            outputs: None,
        };
        let mut out = Vec::new();
        let written = install_all(&mut env, &mut host, &defs, &[], &mut out);
        assert_eq!(written, [false; SITE_COUNT]);
        let after: Vec<Vec<u8>> = (0..5).map(|i| page.bytes(i * 0x40, 8)).collect();
        assert_eq!(after, before, "書いた");
        assert!(ALLOC_TRAMPS.iter().all(|t| t.load(Ordering::SeqCst) == 0));
        assert_eq!(call1(defs[0].addr, 1), 8);
        drop(page);
        clear();
    }

    #[test]
    fn entry_addresses_are_distinct() {
        let mut v: Vec<u32> = (0..SITE_COUNT).map(entry_address).collect();
        assert!(v.iter().all(|&a| a != 0));
        v.sort_unstable();
        v.dedup();
        assert_eq!(v.len(), SITE_COUNT);
        assert_eq!(entry_address(SITE_COUNT), 0);
    }

    #[test]
    fn writer_writes_files_and_stops() {
        let _g = lock();
        clear();
        let dir =
            std::env::temp_dir().join(format!("higanr_alloc_{}_{}", std::process::id(), my_tid()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("dll_test.log");
        let mut host = RealAllocHost {
            ini: None,
            run_path: Some(log.clone()),
            outputs: None,
        };
        host.open_outputs().expect("開ける");
        let mut again = RealAllocHost {
            ini: None,
            run_path: Some(log),
            outputs: None,
        };
        assert!(again.open_outputs().is_err(), "既存のファイルを開き直した");
        let (page, ret) = call_page(entry_address(1), 3);
        ALLOC_TRAMPS[1].store(ret, Ordering::SeqCst);
        ALLOC_MAIN_TID.store(1, Ordering::SeqCst); // 自分は「それ以外」
        host.start_writer(&AllocSettings { seconds: 30 }, [true; SITE_COUNT])
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        run_page(&page);
        std::thread::sleep(Duration::from_millis(900));
        let lines = host.finish().expect("集計の文");
        assert!(
            lines.iter().any(|l| l.starts_with("書き出しを終えた")),
            "{lines:?}"
        );
        let read = |s: &str| std::fs::read_to_string(dir.join(format!("dll_test{s}"))).unwrap();
        let sec = read(SEC_SUFFIX);
        assert!(
            sec.lines().count() >= 3,
            "見出し ＋ 1 秒 ＋ 最後の期間: {sec}"
        );
        let calls = read(CALLS_SUFFIX);
        assert_eq!(
            calls
                .lines()
                .filter(|l| l.contains(",free_wrap,other,"))
                .count(),
            3,
            "{calls}"
        );
        let callers = read(CALLERS_SUFFIX);
        assert!(
            callers.lines().filter(|l| l.contains(",call_abs,")).count() == 3,
            "{callers}"
        );
        let table = read(TABLE_SUFFIX);
        assert!(table.starts_with(TABLE_HEADER));
        let all: Vec<&str> = table
            .lines()
            .find(|l| l.starts_with("all,free_wrap,other,all,"))
            .expect("all の行")
            .split(',')
            .collect();
        assert_eq!(all[6], "3", "総数: {all:?}");
        assert!(
            table
                .lines()
                .any(|l| l.starts_with("match,free_wrap,other,all,1,0,0,,"))
        );
        assert!(host.finish().is_none());
        drop(page);
        let _ = std::fs::remove_dir_all(&dir);
        clear();
    }
}
