//! ゲームのメニューから保存するリプレイの入口と、印を足すスレッドの Win32 の層（unsafe はここに閉じ込める。1 つずつ `// SAFETY:`）。
//!
//! 保存関数 `0x00420C60` を呼ぶゲームの 2 か所（保存の画面 `0x004297F1`・内蔵のネット対戦の自動保存 `0x00429BF6`）の rel32 を、`DllMain` の中（ゲームの主スレッドがまだ走っていない）で自前の入口 `menu_save_entry` へ向ける。
//! 入口は (1) 見積もりのループが読む場所へ本当の印を書く踏み台 `replay_save_win::save_call` 経由で保存関数を呼び（長い試合で確保を越えて書く不具合の避け方）、(2) 戻った後にパス・始めの時刻・game_mode を `SaveSignal` に書く。
//! ファイルを触るのは別のスレッドで、判断は `menu_save.rs`。ゲームのスレッドの入口は確保・ファイル I/O・ログの書き出し・待ちをしない。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "menu_save_win.rs は 32 ビット（i686）専用（ゲームの呼び出しの約束 fastcall と 32 ビットのアドレスを前提にしている）"
);

use core::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use higanr_core::update::BUILD_NUMBER;
use windows_sys::Win32::Foundation::{CloseHandle, HMODULE};
use windows_sys::Win32::System::Threading::CreateThread;

use crate::hook_win::RealHook;
use crate::main_log;
use crate::menu_save::{
    LOG_LABEL, MarkFile, MarkIo, PATH_MAX, STEP_MS, SaveDiag, SaveSignal, step,
};
use crate::menu_win::OwnProcess;
use crate::pad_extra::install_as;
use crate::replay_save::{STAGES, need_bound};
use crate::replay_save_win::{count_chunks, same_file_ansi, save_call};
use crate::th09_addr::{
    EXE_DIR, EXE_DIR_LEN, GAME_MODE, MENU_SAVE_SITES, REPLAY_HEADER_PTR, REPLAY_HEADER_STAGE_FLAGS,
    REPLAY_MODE, REPLAY_SAVE_FN, SAVE_EST_BASE,
};
use crate::win::{debug_out, read_own_into, read_own_memory};

/// 印に書く HiganRTool のバージョン名（ワークスペースの `version`）。
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ゲームのスレッドが書き、印を足すスレッドが拾う合図。
static SIGNAL: SaveSignal = SaveSignal::new();

/// 保存関数の型。MSVC の `__fastcall`（ecx = this、edx = パス、スタックに名前の 1 語、呼ばれた側が `ret 4`）は Rust の `extern "fastcall"` と同じ約束。
type SaveFn = unsafe extern "fastcall" fn(u32, u32, u32) -> u32;

/// ゲームから呼ばれる入口。2 か所の呼び出しの rel32 がここへ向く。引数は元の保存関数と同じ。
pub extern "fastcall" fn menu_save_entry(this: u32, path: u32, name: u32) -> u32 {
    entry_with(&SIGNAL, REPLAY_SAVE_FN, GAME_MODE, this, path, name)
}

/// 入口の本体。`save_fn` は保存関数、`mode_addr` は `game_mode` の語（検査では自前の変数）。ゲームのスレッドで走る。
/// 読めない・パニックしたときも、ゲームの保存は必ず呼ぶ（保存が起きないことは避ける）。
pub(crate) fn entry_with(
    signal: &SaveSignal,
    save_fn: u32,
    mode_addr: u32,
    this: u32,
    path: u32,
    name: u32,
) -> u32 {
    let started = now_ms();
    let flags = catch_unwind(|| read_flags(this)).unwrap_or(None);
    let Some(flags) = flags else {
        // SAFETY: `save_fn` は `on_attach` の照合を通した保存関数（検査では偽物）。引数はゲームから受けた値をそのまま渡す。
        return unsafe { call_plain(save_fn, this, path, name) };
    };
    // 再生の側（`+0x114` が 0 でない）は記録した保存ではないので、見積もりの直しだけして印の合図は立てない。
    let recording =
        catch_unwind(|| read_u32(this.wrapping_add(REPLAY_MODE)) == Some(0)).unwrap_or(false);
    // SAFETY: `save_fn` は照合を通した保存関数。`this` は 0 でなく、見出しのポインタ `[this+8]` が 0 でなく `flags` を読めた ReplayManager。
    // `path`・`name` はゲームから受けた引数。
    let (ret, diag) = unsafe { call_with_flags(save_fn, this, path, name, &flags) };
    if recording {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            note_saved(signal, path, started, mode_addr, diag)
        }));
    }
    ret
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn read_u32(addr: u32) -> Option<u32> {
    let mut b = [0u8; 4];
    read_own_into(addr, &mut b).then_some(u32::from_le_bytes(b))
}

/// 見出し（`[this+8]`）の `+0x20` の 10 語（面ごとの印）。ヒープを使わない。
fn read_flags(this: u32) -> Option<[u32; STAGES]> {
    if this == 0 {
        return None;
    }
    let hdr = read_u32(this.checked_add(REPLAY_HEADER_PTR)?)?;
    if hdr == 0 {
        return None;
    }
    let mut raw = [0u8; 4 * STAGES];
    if !read_own_into(hdr.checked_add(REPLAY_HEADER_STAGE_FLAGS)?, &mut raw) {
        return None;
    }
    let mut out = [0u32; STAGES];
    for (slot, c) in out.iter_mut().zip(raw.as_chunks::<4>().0) {
        *slot = u32::from_le_bytes(*c);
    }
    Some(out)
}

/// NUL までのパスを 1 バイトずつ読む（ページの端で止まっても落ちない）。NUL が見つからないまま `buf` が埋まれば `buf.len()`。読めなければ `None`。
fn read_cstr(addr: u32, buf: &mut [u8; PATH_MAX + 1]) -> Option<usize> {
    for (i, slot) in buf.iter_mut().enumerate() {
        let mut b = [0u8; 1];
        if !read_own_into(addr.checked_add(i as u32)?, &mut b) {
            return None;
        }
        if b[0] == 0 {
            return Some(i);
        }
        *slot = b[0];
    }
    Some(PATH_MAX + 1)
}

fn note_saved(signal: &SaveSignal, path: u32, started: u64, mode_addr: u32, diag: SaveDiag) {
    let mut buf = [0u8; PATH_MAX + 1];
    // 読めないパスは空にする（印を足す側が「形が違う」として断る）。
    let n = read_cstr(path, &mut buf).unwrap_or(0);
    let mode = read_u32(mode_addr).unwrap_or(u32::MAX);
    signal.record(&buf[..n], started, mode, diag);
}

/// 0 でない語の数。
fn nonzero(words: &[u32; STAGES]) -> u32 {
    words.iter().filter(|&&w| w != 0).count() as u32
}

/// # Safety
/// `save_fn` は ReplayManager::save（`SaveFn` の約束）のアドレス。引数は保存関数が受ける値。
unsafe fn call_plain(save_fn: u32, this: u32, path: u32, name: u32) -> u32 {
    // SAFETY: 呼び手の約束どおり、`save_fn` は `SaveFn` の約束の関数。
    let f = unsafe { core::mem::transmute::<usize, SaveFn>(save_fn as usize) };
    // SAFETY: 引数は呼び手から受けた値をそのまま渡す。
    unsafe { f(this, path, name) }
}

/// 本当の印 `flags` を見積もりのループが読む場所に書いてから保存関数を呼ぶ（`save_call`。`need` は 0 で、呼ばない守りは使わない）。
/// 見積もりには面ごとの塊の数（`count_chunks`）を渡す。
/// 戻り値は元の保存関数と同じく 0 と、踏み台が返した書く前・読み戻し・見積もりから作る `SaveDiag`（本ログ用。確保しない）。
///
/// # Safety
/// `save_fn` は ReplayManager::save のアドレスで、先頭が `th09_addr::REPLAY_SAVE_SIG` と一致するもの。`this`・`path`・`name` は保存関数が受ける値。
unsafe fn call_with_flags(
    save_fn: u32,
    this: u32,
    path: u32,
    name: u32,
    flags: &[u32; STAGES],
) -> (u32, SaveDiag) {
    let mut before = [0u32; STAGES];
    let mut after = [0u32; STAGES];
    // 読めない・長すぎる連鎖は塊 0 として見積もる（ゲームの保存は必ず呼ぶ）。本ログには「読めなかった」と出す。
    let counted = count_chunks(&mut OwnProcess, this);
    let chunks = counted.unwrap_or([0; STAGES]);
    let mut est = 0u32;
    // SAFETY（`save_call` の約束）: `save_fn` は呼び手の約束。`before`・`after`・`est` はこのスタックの書ける領域、`flags`・`chunks` は読める 10 語。
    // `need` が 0 なので見積もりで断らず、必ず保存関数を呼ぶ。踏み台が書くのは自分のスタックの esp より下の 40 バイトだけ。ゲームのスレッドから呼ぶ。
    unsafe {
        save_call(
            save_fn,
            this,
            path,
            name,
            before.as_mut_ptr() as usize as u32,
            flags.as_ptr() as usize as u32,
            after.as_mut_ptr() as usize as u32,
            chunks.as_ptr() as usize as u32,
            SAVE_EST_BASE,
            0,
            &mut est as *mut u32 as usize as u32,
        )
    };
    let diag = SaveDiag {
        leftover_stages: nonzero(&before),
        real_stages: nonzero(flags),
        placed: after == *flags,
        chunks: counted.map(|c| {
            c.iter()
                .zip(flags)
                .filter(|&(_, &f)| f != 0)
                .map(|(&n, _)| n)
                .sum()
        }),
        est,
        need: need_bound(flags, &chunks),
    };
    (0, diag)
}

// ---- 印を足すスレッドのファイル ----

/// 足すファイルを開く本物。`exe_dir` / `base` は検査が差し替える（本物は `None` で、ゲームの exe のフォルダを `0x4ACD08` から読み、
/// 相対パスは作業フォルダ基準のまま開く —— ゲームの保存と同じ道）。
pub struct RealMarkIo {
    exe_dir: Option<Vec<u8>>,
    base: Option<PathBuf>,
}

impl RealMarkIo {
    pub fn game() -> Self {
        RealMarkIo {
            exe_dir: None,
            base: None,
        }
    }
}

/// ゲームが持つ exe のフォルダ（CP932 のバイト列。最後は `/`）。
fn game_exe_dir() -> Option<Vec<u8>> {
    let raw = read_own_memory(EXE_DIR, EXE_DIR_LEN as usize)?;
    let n = raw.iter().position(|&b| b == 0)?;
    let mut d = raw[..n].to_vec();
    if !matches!(d.last(), Some(b'/' | b'\\')) {
        d.push(b'/');
    }
    Some(d)
}

struct RealMarkFile {
    file: File,
    rel: String,
    exe_dir: Vec<u8>,
}

impl MarkIo for RealMarkIo {
    fn open(&mut self, rel: &str) -> Result<Box<dyn MarkFile>, String> {
        let exe_dir = match &self.exe_dir {
            Some(d) => d.clone(),
            None => game_exe_dir().ok_or("exe のフォルダが読み取り不能")?,
        };
        let path = match &self.base {
            Some(b) => b.join(rel),
            None => PathBuf::from(rel),
        };
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        Ok(Box::new(RealMarkFile {
            file,
            rel: rel.to_string(),
            exe_dir,
        }))
    }
}

impl MarkFile for RealMarkFile {
    fn same_as_exe_dir(&mut self) -> Result<bool, String> {
        let mut p = self.exe_dir.clone();
        p.extend_from_slice(self.rel.as_bytes());
        same_file_ansi(&p, &self.file)
    }
    fn modified_ms(&mut self) -> Result<u64, String> {
        let m = self
            .file
            .metadata()
            .and_then(|m| m.modified())
            .map_err(|e| e.to_string())?;
        Ok(m.duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64))
    }
    fn read_all(&mut self) -> Result<Vec<u8>, String> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        self.file.read_to_end(&mut v).map_err(|e| e.to_string())?;
        Ok(v)
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), String> {
        // 追記の権利だけで開いてあるので、書く位置は常に末尾。
        self.file.write_all(bytes).map_err(|e| e.to_string())?;
        self.file.flush().map_err(|e| e.to_string())
    }
}

// ---- DllMain からの設置 ----

/// `DllMain`（`DLL_PROCESS_ATTACH`）から呼ぶ。4 行を照合して 2 つの呼び出しを入口へ向け、印を足すスレッドを作る。照合が 1 行でも合わなければ
/// 1 バイトも書かず、今までどおりゲームの保存になる。パニックは受けて、書いた分だけを残す。
pub fn on_attach(module: HMODULE) {
    let r = catch_unwind(AssertUnwindSafe(|| attach_inner(module)));
    if r.is_err() {
        main_log::push(format!(
            "{LOG_LABEL}: 設置の途中でパニックを受けました（書いた分は残る。入口は書いた分だけ働く）。"
        ));
    }
}

fn attach_inner(module: HMODULE) {
    let mut env = RealHook::new();
    let entry_fn: extern "fastcall" fn(u32, u32, u32) -> u32 = menu_save_entry;
    let report = install_as(
        &mut env,
        MENU_SAVE_SITES,
        entry_fn as usize as u32,
        LOG_LABEL,
    );
    // 全部書けたときは詳細だけ。書けなかったものがあれば簡潔にも残す。
    for l in &report.lines {
        main_log::push_when(!report.complete(), l.clone());
    }
    // 1 つも書けなければ入口は呼ばれないので、印を足すスレッドは要らない。
    if report.written > 0 {
        start_thread(module);
    }
}

fn start_thread(module: HMODULE) {
    // SAFETY: `mark_thread` は LPTHREAD_START_ROUTINE の形。引数は自分の DLL のハンドルで、この DLL は自分を解放しない。
    // スレッドの属性は既定。ローダーロックが外れてから走る。
    let h = unsafe {
        CreateThread(
            core::ptr::null(),
            0,
            Some(mark_thread),
            module as *const c_void,
            0,
            core::ptr::null_mut(),
        )
    };
    if h.is_null() {
        main_log::push(format!(
            "{LOG_LABEL}: 印を足すスレッドの作成不能（保存の直しだけが働く）。"
        ));
        return;
    }
    // SAFETY: いま作ったスレッドのハンドル。閉じてもスレッドは止まらない。
    unsafe { CloseHandle(h) };
}

extern "system" fn mark_thread(_param: *mut c_void) -> u32 {
    let r = catch_unwind(AssertUnwindSafe(run_thread));
    if r.is_err() {
        debug_out("HiganRTool: メニュー保存のスレッドでパニックを受けた。スレッドを終える");
    }
    0
}

fn run_thread() {
    let mut last = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(STEP_MS));
        if let Some((brief, line)) = step(
            &SIGNAL,
            &mut last,
            BUILD_NUMBER,
            TOOL_VERSION,
            &mut RealMarkIo::game(),
        ) {
            main_log::push_when(brief, line);
        }
    }
}

#[cfg(test)]
mod tests {
    //! 偽の保存関数（先頭の 12 バイトを花映塚と同じにした素のアセンブリ）と、機械語の呼び出し元を使い、入口が本物の `call` の約束
    //! （fastcall・`ret 4`・スタックの高さ）と踏み台の書く 10 語で働くかを見る。花映塚は使わない。
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use crate::menu_save::test_replay;
    use crate::sites::{SiteDef, Use};
    use crate::test_code::RawCode;
    use crate::th09_addr::{REPLAY_NODE_NEXT, REPLAY_STAGE_NODES, REPLAY_STAGE_STRIDE};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// 偽の保存関数と静的変数は 1 組しか無いので、1 本ずつ走らせる。
    static SERIAL: Mutex<()> = Mutex::new(());

    static HITS: AtomicU32 = AtomicU32::new(0);
    static SEEN_THIS: AtomicU32 = AtomicU32::new(0);
    static SEEN_PATH: AtomicU32 = AtomicU32::new(0);
    static SEEN_NAME: AtomicU32 = AtomicU32::new(0);
    static SEEN_FLAGS: [AtomicU32; STAGES] = [const { AtomicU32::new(0) }; STAGES];
    static T_SIGNAL: SaveSignal = SaveSignal::new();
    static T_MODE: AtomicU32 = AtomicU32::new(0);
    static T_LAST: AtomicU32 = AtomicU32::new(0);

    // SAFETY（`#[unsafe(naked)]`）: 花映塚の保存関数の真似。先頭の 12 バイトは署名と同じ（`.byte` で書くのは、アセンブラが `mov ebp,esp` を別の符号に
    // しうるため）。呼ばれた回数・this（edi）・パス（edx）・名前（`[ebp+8]`）・見積もりのループが読む 10 語（`[ebp - 0x1F8 + 4i]`）を静的変数へ
    // コピーして `ret 4` で戻る（名前を片付ける）。ebx・esi には触らない。書くのは検査の静的変数と自分の枠だけ。
    #[unsafe(naked)]
    extern "C" fn fake_save() {
        core::arch::naked_asm!(
            ".byte 0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x18, 0x06, 0x00, 0x00, 0x57, 0x8B, 0xF9",
            "lock inc dword ptr [{hits}]",
            "mov dword ptr [{this}], edi",
            "mov dword ptr [{path}], edx",
            "mov eax, dword ptr [ebp + 8]",
            "mov dword ptr [{name}], eax",
            "xor ecx, ecx",
            "2:",
            "mov eax, dword ptr [ebp + ecx*4 - {f}]",
            "mov dword ptr [{flags} + ecx*4], eax",
            "inc ecx",
            "cmp ecx, {stages}",
            "jl 2b",
            "pop edi",
            "mov esp, ebp",
            "pop ebp",
            "ret 4",
            hits = sym HITS,
            this = sym SEEN_THIS,
            path = sym SEEN_PATH,
            name = sym SEEN_NAME,
            flags = sym SEEN_FLAGS,
            f = const crate::th09_addr::SAVE_EST_FLAGS_EBP,
            stages = const STAGES,
        )
    }

    fn fake_addr() -> u32 {
        let f: extern "C" fn() = fake_save;
        f as usize as u32
    }

    /// 検査の入口（`menu_save_entry` と同じ形。保存関数は偽物、game_mode は自前の変数）。
    extern "fastcall" fn test_entry(this: u32, path: u32, name: u32) -> u32 {
        entry_with(
            &T_SIGNAL,
            fake_addr(),
            &T_MODE as *const AtomicU32 as usize as u32,
            this,
            path,
            name,
        )
    }

    fn test_entry_addr() -> u32 {
        let f: extern "fastcall" fn(u32, u32, u32) -> u32 = test_entry;
        f as usize as u32
    }

    const PATTERN: u32 = 0x5151_5151;

    /// ゲームと同じ形の呼び出し元: 呼ぶ前に、保存関数のループが読む場所（`E − 0x200` からの 10 語）へ模様を書き（呼び手のスタックの残り）、
    /// ecx = this・edx = パス・`push` 名前で `call`。呼んだ後でスタックの高さが元か検証し、ずれていれば eax に印を入れて戻す。
    /// 戻り値は eax（高さが合えば 0）。`call` の `E8` の位置を返す。
    fn caller(this: u32, path: u32, name: u32, target: u32, at: u32) -> (Vec<u8>, u32) {
        let mut c: Vec<u8> = vec![
            0x53, // push ebx
            0x57, // push edi
            0x89, 0xE3, // mov ebx,esp
            0x8D, 0xBC, 0x24, 0xFC, 0xFD, 0xFF, 0xFF, // lea edi,[esp-0x204]
            0xB9, 0x0A, 0x00, 0x00, 0x00, // mov ecx,10
            0xB8, // mov eax,PATTERN
        ];
        c.extend_from_slice(&PATTERN.to_le_bytes());
        c.extend_from_slice(&[0xF3, 0xAB]); // rep stosd
        c.push(0xB9);
        c.extend_from_slice(&this.to_le_bytes());
        c.push(0xBA);
        c.extend_from_slice(&path.to_le_bytes());
        c.push(0x68);
        c.extend_from_slice(&name.to_le_bytes());
        let call_at = at + c.len() as u32;
        let rel = target.wrapping_sub(call_at + 5);
        c.push(0xE8);
        c.extend_from_slice(&rel.to_le_bytes());
        c.extend_from_slice(&[
            0x39, 0xE3, // cmp ebx,esp
            0xB8, 0x00, 0x00, 0x00, 0x00, // mov eax,0
            0x74, 0x05, // je +5
            0xB8, 0x00, 0x00, 0xAD, 0x0B, // mov eax,0x0BAD0000
            0x89, 0xDC, // mov esp,ebx
            0x5F, // pop edi
            0x5B, // pop ebx
            0xC3, // ret
        ]);
        (c, call_at)
    }

    /// 呼び出し元を実行できるメモリに書く。`f` は実行して戻り値（eax）を返す関数と、メモリの先頭・`E8` の位置を受ける。
    fn with_code<R>(
        this: u32,
        path: u32,
        name: u32,
        target: u32,
        f: impl FnOnce(&dyn Fn() -> u32, u32, u32) -> R,
    ) -> R {
        // SAFETY: 読み書き実行できる 0x1000 バイトを確保する（解放は最後）。
        let mem = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as usize;
        assert_ne!(mem, 0);
        let (code, call_at) = caller(this, path, name, target, mem as u32);
        assert!(code.len() < 0x100);
        // SAFETY: 確保した 0x1000 バイトの先頭へ、それより短い機械語を書く。
        unsafe { core::ptr::copy_nonoverlapping(code.as_ptr(), mem as *mut u8, code.len()) };
        let run = || -> u32 {
            // SAFETY: 先頭は上で書いた呼び出し元（cdecl で呼べる: ebx・edi を保ち、eax に戻す）。
            let g = unsafe { core::mem::transmute::<usize, extern "C" fn() -> u32>(mem) };
            g()
        };
        let r = f(&run, mem as u32, call_at);
        // SAFETY: 上で確保した領域を 1 回だけ解放する。
        unsafe { VirtualFree(mem as *mut c_void, 0, MEM_RELEASE) };
        r
    }

    fn flags_seen() -> [u32; STAGES] {
        core::array::from_fn(|i| SEEN_FLAGS[i].load(Ordering::SeqCst))
    }

    /// 語ごとに違う値で、0 の面を混ぜる（0 の面を取り違えると値が合わない）。
    fn real_flags() -> [u32; STAGES] {
        let mut r: [u32; STAGES] = core::array::from_fn(|i| 0x0BB4_7000 + i as u32 * 0x44);
        r[1] = 0;
        r[4] = 0;
        r
    }

    /// ReplayManager の見本: `+8` が見出しへのポインタ、`+0x114` が再生の側か。見出しの `+0x20` に印。
    struct World {
        manager: Box<[u32; 0x48]>,
        _header: Box<[u32; 0x7B]>,
        /// 面ごとの塊の連鎖に足した塊（`+0x14` が次）。Vec が伸びてもアドレスが動かないように Box に入れる。
        #[allow(clippy::vec_box)]
        nodes: Vec<Box<[u32; 6]>>,
    }

    fn world(flags: [u32; STAGES], replay_mode: u32) -> World {
        let mut header = Box::new([0u32; 0x7B]);
        header[REPLAY_HEADER_STAGE_FLAGS as usize / 4..][..STAGES].copy_from_slice(&flags);
        let mut manager = Box::new([0u32; 0x48]);
        manager[REPLAY_HEADER_PTR as usize / 4] = header.as_ptr() as usize as u32;
        manager[REPLAY_MODE as usize / 4] = replay_mode;
        World {
            manager,
            _header: header,
            nodes: Vec::new(),
        }
    }

    impl World {
        fn this(&self) -> u32 {
            self.manager.as_ptr() as usize as u32
        }

        /// 面 `stage` の塊の連鎖に、先頭の塊の後ろへ `extra` 個連結する。
        fn chain(&mut self, stage: usize, extra: usize) {
            let next_at = |stage: usize| {
                (REPLAY_STAGE_NODES as usize
                    + REPLAY_STAGE_STRIDE as usize * stage
                    + REPLAY_NODE_NEXT as usize)
                    / 4
            };
            let link = next_at(stage);
            let mut prev: Option<usize> = None;
            for _ in 0..extra {
                let mut node = Box::new([0u32; 6]);
                let addr = node.as_mut_ptr() as usize as u32;
                match prev {
                    None => self.manager[link] = addr,
                    Some(i) => self.nodes[i][REPLAY_NODE_NEXT as usize / 4] = addr,
                }
                self.nodes.push(node);
                prev = Some(self.nodes.len() - 1);
            }
        }
    }

    const PATH: &[u8] = b"replay/th9_05.rpy\0";
    const NAME: [u8; 12] = *b"12:34:56\0\0\0\0";

    /// 検査の合図を、前に拾った番号を覚えたまま拾う（拾い済みのものは `None`）。
    fn drain_signal() -> Option<crate::menu_save::Taken> {
        let mut last = T_LAST.load(Ordering::SeqCst);
        let t = T_SIGNAL.take(&mut last);
        T_LAST.store(last, Ordering::SeqCst);
        t
    }

    /// 検査ごとに、拾い済みでない合図を読み捨てて、偽の保存関数の記録を空にする。
    fn reset() {
        let _ = drain_signal();
        HITS.store(0, Ordering::SeqCst);
        for f in &SEEN_FLAGS {
            f.store(0, Ordering::SeqCst);
        }
    }

    /// 呼び出し元から入口へ（rel32 は最初から入口）。
    fn call_through_entry(this: u32, path: u32) -> u32 {
        with_code(
            this,
            path,
            NAME.as_ptr() as usize as u32,
            test_entry_addr(),
            |run, _, _| run(),
        )
    }

    #[test]
    fn entry_gives_the_game_loop_the_real_flags_and_records_the_save() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        T_MODE.store(2, Ordering::SeqCst);
        let mut w = world(real_flags(), 0);
        // 印のある面 2 に塊を 2 つ足す。印の無い面 1 の塊は見積もりに入らない。
        w.chain(2, 2);
        w.chain(1, 1);
        let before = now_ms();
        let ret = call_through_entry(w.this(), PATH.as_ptr() as usize as u32);
        let after = now_ms();
        assert_eq!(
            ret, 0,
            "戻ったときのスタックの高さが元と違う（ret 4 の約束が崩れた）"
        );
        assert_eq!(HITS.load(Ordering::SeqCst), 1);
        assert_eq!(SEEN_THIS.load(Ordering::SeqCst), w.this());
        assert_eq!(
            SEEN_PATH.load(Ordering::SeqCst),
            PATH.as_ptr() as usize as u32
        );
        assert_eq!(
            SEEN_NAME.load(Ordering::SeqCst),
            NAME.as_ptr() as usize as u32
        );
        assert_eq!(
            flags_seen(),
            real_flags(),
            "保存関数のループが読む 10 語が本当の印でない（呼び手のスタックの残りのまま）"
        );
        let t = drain_signal().expect("合図が立っていない");
        assert_eq!(t.path, b"replay/th9_05.rpy");
        assert_eq!(t.game_mode, 2);
        assert!(!t.too_long);
        // 本当の印は 8 面（r[1]・r[4] が 0）。塊は印のある面の先頭 8 個 + 足した 2 個。書く前の 10 語は踏み台の足元に残っていた値で、
        // 入口の枠の深さで変わるので数は決めない。
        let real_n = real_flags().iter().filter(|&&f| f != 0).count() as u32;
        assert_eq!(real_n, 8);
        let per_stage = crate::th09_addr::SAVE_EST_PER_STAGE;
        let per_chunk = crate::th09_addr::SAVE_EST_PER_CHUNK;
        assert!(t.diag.leftover_stages <= STAGES as u32, "{:?}", t.diag);
        assert_eq!(
            SaveDiag {
                leftover_stages: 0,
                ..t.diag
            },
            SaveDiag {
                leftover_stages: 0,
                real_stages: real_n,
                placed: true,
                chunks: Some(10),
                est: SAVE_EST_BASE + real_n * per_stage + 10 * per_chunk,
                need: crate::th09_addr::REPLAY_HEADER_LEN + real_n * per_stage + 10 * per_chunk,
            },
            "ログに出す直しの値が踏み台の返した値と合わない"
        );
        assert!(
            (before..=after).contains(&t.started_ms),
            "始めの時刻が呼ぶ前後の間にない: {} / {before}..{after}",
            t.started_ms
        );
    }

    #[test]
    fn an_unreadable_chunk_chain_still_saves_and_says_so() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        T_MODE.store(2, Ordering::SeqCst);
        let mut w = world(real_flags(), 0);
        w.chain(2, 1);
        // 面 3 の先頭の塊の次が読めない所を指す。
        let at = (REPLAY_STAGE_NODES + REPLAY_STAGE_STRIDE * 3 + REPLAY_NODE_NEXT) as usize / 4;
        w.manager[at] = 0x0000_1000;
        let ret = call_through_entry(w.this(), PATH.as_ptr() as usize as u32);
        assert_eq!(ret, 0);
        assert_eq!(HITS.load(Ordering::SeqCst), 1, "保存関数が呼ばれていない");
        assert_eq!(flags_seen(), real_flags());
        let d = drain_signal().expect("合図").diag;
        assert_eq!(d.chunks, None);
        assert!(d.placed);
        assert_eq!(
            d.est,
            SAVE_EST_BASE + 8 * crate::th09_addr::SAVE_EST_PER_STAGE,
            "読めない塊は 0 個として見積もる"
        );
    }

    #[test]
    fn without_the_entry_the_game_loop_reads_the_callers_leftover() {
        // 前提の確認（変異が効く土台）: 入口を通さないと、ループが読むのは呼び手のスタックの残り（ここでは模様）。
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let w = world(real_flags(), 0);
        let ret = with_code(
            w.this(),
            PATH.as_ptr() as usize as u32,
            NAME.as_ptr() as usize as u32,
            fake_addr(),
            |run, _, _| run(),
        );
        assert_eq!(ret, 0);
        assert_eq!(flags_seen(), [PATTERN; STAGES]);
        assert!(drain_signal().is_none());
    }

    #[test]
    fn installing_rewrites_the_rel32_and_switches_the_behaviour() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let w = world(real_flags(), 0);
        let path = PATH.as_ptr() as usize as u32;
        let name = NAME.as_ptr() as usize as u32;
        with_code(w.this(), path, name, fake_addr(), |run, base, call_at| {
            assert_eq!(run(), 0);
            assert_eq!(
                flags_seen(),
                [PATTERN; STAGES],
                "書き換える前は元の保存関数"
            );
            assert!(drain_signal().is_none());
            // 本物の `install_as` の書き方で、呼び出しの rel32 だけを入口へ向ける。
            let want: &'static [u8] = Box::leak(
                {
                    let mut v = vec![0xE8];
                    v.extend_from_slice(&fake_addr().wrapping_sub(call_at + 5).to_le_bytes());
                    v
                }
                .into_boxed_slice(),
            );
            let rows = [SiteDef {
                name: "test_menu_call",
                addr: call_at,
                expected: want,
                kind: Use::CallRel32 {
                    original_target: fake_addr(),
                },
                source: "合成",
            }];
            let report = install_as(
                &mut RawCode { base, span: 0x100 },
                &rows,
                test_entry_addr(),
                LOG_LABEL,
            );
            assert!(report.complete(), "{:?}", report.lines);
            assert!(report.lines[0].starts_with(LOG_LABEL), "{:?}", report.lines);
            reset();
            assert_eq!(run(), 0);
            assert_eq!(HITS.load(Ordering::SeqCst), 1);
            assert_eq!(flags_seen(), real_flags());
            assert!(drain_signal().is_some());
        });
    }

    #[test]
    fn a_replay_in_playback_side_is_fixed_but_not_signalled() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        let w = world(real_flags(), 1);
        let ret = call_through_entry(w.this(), PATH.as_ptr() as usize as u32);
        assert_eq!(ret, 0);
        assert_eq!(flags_seen(), real_flags());
        assert!(drain_signal().is_none(), "再生の側の保存に印の合図を立てた");
    }

    #[test]
    fn an_unreadable_manager_or_header_calls_the_original_untouched() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // this = 0、見出しのポインタが 0、見出しが読めない（確保されていないアドレス）のどれでも、保存関数は呼ばれ、何も書かず、合図も無い。
        let mut zero_header = world(real_flags(), 0);
        zero_header.manager[REPLAY_HEADER_PTR as usize / 4] = 0;
        let mut bad_header = world(real_flags(), 0);
        bad_header.manager[REPLAY_HEADER_PTR as usize / 4] = 0x0000_1000;
        for this in [0, zero_header.this(), bad_header.this()] {
            reset();
            let ret = call_through_entry(this, PATH.as_ptr() as usize as u32);
            assert_eq!(ret, 0);
            assert_eq!(
                HITS.load(Ordering::SeqCst),
                1,
                "保存関数が呼ばれていない（this = {this:#X}）"
            );
            assert_ne!(
                flags_seen(),
                real_flags(),
                "this = {this:#X}: 何も置かないはず"
            );
            assert!(drain_signal().is_none());
        }
    }

    #[test]
    fn an_unreadable_or_overlong_path_is_signalled_as_such() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let w = world(real_flags(), 0);
        // 読めないパス（確保されていないアドレス）。
        reset();
        call_through_entry(w.this(), 0x0000_2000);
        assert_eq!(drain_signal().expect("合図").path, b"");
        // NUL が `PATH_MAX` 以内に無いパス。
        let long = vec![b'a'; PATH_MAX + 10];
        let mut long_z = long.clone();
        long_z.push(0);
        reset();
        call_through_entry(w.this(), long_z.as_ptr() as usize as u32);
        let t = drain_signal().expect("合図");
        assert!(t.too_long, "{t:?}");
        // ちょうど `PATH_MAX` バイトは通る。
        let mut max = vec![b'a'; PATH_MAX];
        max.push(0);
        reset();
        call_through_entry(w.this(), max.as_ptr() as usize as u32);
        let t = drain_signal().expect("合図");
        assert!(!t.too_long && t.path.len() == PATH_MAX, "{t:?}");
    }

    #[test]
    fn the_game_thread_entry_has_no_heap_io_log_or_wait() {
        let src = include_str!("menu_save_win.rs");
        let body = src
            .split("pub extern \"fastcall\" fn menu_save_entry")
            .nth(1)
            .expect("入口");
        let body = body
            .split("// ---- 印を足すスレッドのファイル ----")
            .next()
            .unwrap();
        for (i, line) in body.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for banned in [
                "Vec",
                "vec!",
                "format!",
                "String",
                "Box",
                "main_log",
                "std::fs",
                "File",
                "sleep",
                "Mutex",
                "println",
                "unwrap()",
                "expect(",
                "to_vec",
                "to_string",
                "to_owned",
                "collect",
                "clone()",
                "push(",
                "extend",
                "into_boxed",
                "Rc<",
                "Rc::",
                "Arc<",
                "Arc::",
                "Cow",
                "HashMap",
                "BTreeMap",
                "alloc",
            ] {
                assert!(
                    !code.contains(banned),
                    "menu_save_win.rs:{} に {banned}: {code}",
                    i + 1
                );
            }
        }
    }

    #[test]
    fn the_tool_version_fits_in_the_menu_mark() {
        let m = higanr_core::replay::MenuMark {
            ver: TOOL_VERSION.to_string(),
            build: BUILD_NUMBER,
        };
        assert!(higanr_core::replay::menu_mark_chunk(&m).is_ok(), "{m:?}");
    }

    #[test]
    fn a_foreign_process_is_refused_with_the_real_reader_and_nothing_is_written() {
        let r = install_as(
            &mut RealHook::new(),
            MENU_SAVE_SITES,
            test_entry_addr(),
            LOG_LABEL,
        );
        assert_eq!(r.written, 0, "{:?}", r.lines);
        assert!(!r.complete());
        assert!(
            r.lines
                .iter()
                .any(|l| l.contains("1 バイトも書いていません")),
            "{:?}",
            r.lines
        );
    }

    // ---- 本物のファイル（一時フォルダ。ゲームのフォルダは使わない）----

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("menu_save_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("replay")).unwrap();
        d
    }

    fn io_for(exe: &std::path::Path, cwd: &std::path::Path) -> RealMarkIo {
        let mut e = exe.to_string_lossy().replace('\\', "/").into_bytes();
        e.push(b'/');
        RealMarkIo {
            exe_dir: Some(e),
            base: Some(cwd.to_path_buf()),
        }
    }

    fn taken(path: &[u8], started_ms: u64) -> crate::menu_save::Taken {
        crate::menu_save::Taken {
            path: path.to_vec(),
            too_long: false,
            started_ms,
            game_mode: 0,
            diag: crate::menu_save::SaveDiag::default(),
        }
    }

    #[test]
    fn real_files_get_one_mark_at_the_end_in_the_games_own_folder_only() {
        use crate::menu_save::{Outcome, handle};
        let d = temp_dir("a");
        let orig = test_replay();
        let file = d.join("replay/th9_05.rpy");
        std::fs::write(&file, &orig).unwrap();
        let o = handle(
            &taken(b"replay/th9_05.rpy", now_ms()),
            9,
            "1.1.1",
            &mut io_for(&d, &d),
        );
        assert!(matches!(o, Outcome::Marked { .. }), "{o:?}");
        let marked = std::fs::read(&file).unwrap();
        assert_eq!(&marked[..orig.len()], &orig[..]);
        assert!(marked.len() > orig.len());
        // 2 回目は既に印があるので足さない。
        let o = handle(
            &taken(b"replay/th9_05.rpy", now_ms()),
            9,
            "1.1.1",
            &mut io_for(&d, &d),
        );
        assert!(
            matches!(o, Outcome::Skipped { problem: false, .. }),
            "{o:?}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), marked);

        // 作業フォルダ（cwd）が exe のフォルダでない: 同じ相対パスのファイルが別の場所にあっても足さない。
        let other = temp_dir("b");
        let theirs = other.join("replay/th9_06.rpy");
        std::fs::write(&theirs, &orig).unwrap();
        std::fs::write(d.join("replay/th9_06.rpy"), &orig).unwrap();
        let o = handle(
            &taken(b"replay/th9_06.rpy", now_ms()),
            9,
            "1.1.1",
            &mut io_for(&d, &other),
        );
        assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
        assert_eq!(std::fs::read(&theirs).unwrap(), orig);
        assert_eq!(std::fs::read(d.join("replay/th9_06.rpy")).unwrap(), orig);

        // exe のフォルダ側に対応するファイルが無い: 検証できないので足さない。
        std::fs::write(other.join("replay/th9_07.rpy"), &orig).unwrap();
        let o = handle(
            &taken(b"replay/th9_07.rpy", now_ms()),
            9,
            "1.1.1",
            &mut io_for(&d, &other),
        );
        assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
        assert_eq!(
            std::fs::read(other.join("replay/th9_07.rpy")).unwrap(),
            orig
        );

        // 開けない（ファイルが無い）。
        let o = handle(
            &taken(b"replay/th9_08.rpy", now_ms()),
            9,
            "1.1.1",
            &mut io_for(&d, &d),
        );
        assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
        assert!(
            !d.join("replay/th9_08.rpy").exists(),
            "足す側がファイルを作った"
        );

        // 保存に失敗して古いファイルが残っている（更新時刻が 1 時間前）。
        let old = d.join("replay/th9_09.rpy");
        std::fs::write(&old, &orig).unwrap();
        let f = OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        drop(f);
        let o = handle(
            &taken(b"replay/th9_09.rpy", now_ms()),
            9,
            "1.1.1",
            &mut io_for(&d, &d),
        );
        assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
        assert_eq!(std::fs::read(&old).unwrap(), orig);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&other);
    }
}
