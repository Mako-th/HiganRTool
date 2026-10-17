//! サスペンド起動の経路。失敗の後始末は下の「行」ごとに決まる。
//!
//! 行 1 DLL の確認 → 2 ゲームの同定 → 3 `CreateProcessW(CREATE_SUSPENDED)` → 4 `VirtualAllocEx` →
//! 5 `WriteProcessMemory`（DLL の絶対パス、UTF-16）→ 6 `CreateRemoteThread(LoadLibraryW)` → 7 終わりを待つ →
//! 8 戻り値を見る → 9 領域を返して `ResumeThread`。
//!
//! 失敗したら、一度も走っていないゲームを `TerminateProcess` で消して「始めずに終える」（ツールの無いゲームが
//! 立ち上がると取り違えるため）。どの行でも、開いたハンドルは全部閉じる。unsafe は無い（Win32 は `Win32` トレイト越し）。
//! 仕組みは thprac の `RunGame` / `LoadRemoteLibrary` と同じ（コードはコピーしていない）。違い: 失敗したらゲームを殺す、
//! パスの終端の 0 を自分で書く、待ちに上限を置く。

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use higanr_core::lang::Tx;
use higanr_core::pe::{self, Ident, IdentField, PeError};

use crate::win::{ErrorCode, Handle, LaunchNote, ProcessInfo, Remote, Spawn, WaitResult, Win32};

/// リモートスレッドを待つ上限（行 7）。DllMain は重い仕事をしない約束なので普段は数十ミリ秒。
/// ウイルス対策ソフトが初回の読み込みを止めている間（クラウド照会で長引くことがある。実測はしていない）も待てるよう長く取る。
pub const REMOTE_THREAD_TIMEOUT_MS: u32 = 60_000;

/// 最初の待ち。ここまでに終わらず、ゲームのプロセスが生きていれば知らせて `REMOTE_THREAD_TIMEOUT_MS` まで延ばす。
pub const REMOTE_THREAD_NOTICE_MS: u32 = 10_000;

/// 消えたかを待つ上限。`TerminateProcess` は終わりを待たずに戻るため。
pub const TERMINATE_WAIT_MS: u32 = 5_000;

/// 値に意味は無い（0 以外であればよい）。
pub const ABORT_EXIT_CODE: u32 = 1;

/// 起動の前（DLL を入れる前。主スレッドは止まったまま）に子のメモリへ書く 1 か所。
/// 書く前に `expect` と照合し、書いた後に `new` かを読み直す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodePatch {
    pub at: usize,
    pub expect: &'static [u8],
    pub new: &'static [u8],
}

/// 2 つ目の花映塚だけに当てる書き換え（`--second`）。
/// `0x0042D912 call [0x48E0AC]`（CreateMutexA、名前 "Touhou 08 App"）
/// → `0x0042D91D call [0x48E08C]`（GetLastError）→
/// `0x0042D923 cmp eax,0xB7`（ERROR_ALREADY_EXISTS）→ `0x0042D928 75 19 jne 0x42D943`。
/// 1 つ目が居ると jne が通らず -1 を返して終わる。jne を jmp（`EB`）にすると 2 つ目も先へ進む。
/// ミューテックスはゲームの中で待つ・放す所が無い（`0x4B30A8` の読み手は `0x0042DA73` の 1 か所）。
pub const SECOND_INSTANCE: CodePatch = CodePatch {
    at: 0x0042_D928,
    expect: &[0x75, 0x19],
    new: &[0xEB, 0x19],
};

/// 別に起動している花映塚と独立させる書き換え（毎回当てる）。名前つきの印を持つ花映塚が居る間に別の花映塚を起動すると、
/// 後から起動した方が「二つは起動できません」で終わる際に th09.cfg を全部 0 で書き出すため（実測）。
/// `0x0042D909 68 44 02 49 00 push $0x490244`（CreateMutexA の名前 "Touhou 08 App"）
/// を `6A 00 90 90 90`（push 0 と NOP 3 つ）に
/// すると名前なしの印になり、GetLastError が ERROR_ALREADY_EXISTS を返さないので `0x0042D928` の jne も通る
/// （`SECOND_INSTANCE` は要らない）。
pub const ISOLATED_INSTANCE: CodePatch = CodePatch {
    at: 0x0042_D909,
    expect: &[0x68, 0x44, 0x02, 0x49, 0x00],
    new: &[0x6A, 0x00, 0x90, 0x90, 0x90],
};

/// 起動の指定。パスはどれも絶対パス。
#[derive(Debug, Clone)]
pub struct LaunchRequest<'a> {
    pub game: PathBuf,
    /// この絶対パスのまま `LoadLibraryW` に渡す（ゲームのフォルダへコピーしない）。
    pub dll: PathBuf,
    /// 花映塚には渡さない。自己検査の的にだけ使う。
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    /// 本番は `TH09_V150A`、自己検査はローダー自身の値。
    pub ident: Ident<'a>,
    /// 成功したとき、プロセスのハンドルを閉じずに返すか（自己検査が的の終了コードを見るため。閉じるのは呼んだ側）。
    pub keep_process_handle: bool,
    /// 起動の前に書く所。本番は毎回 `ISOLATED_INSTANCE`、`--second` なら `SECOND_INSTANCE` も。
    pub patches: Vec<CodePatch>,
    pub no_activate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    pub pid: u32,
    pub module: u32,
    pub process: Option<Handle>,
    pub cleanup_issues: Vec<CleanupIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    DllPathInvalid(Tx),
    DllUnreadable(String),
    DllNotPe32I386(PeError),
    GamePathInvalid,
    GameUnreadable(String),
    GameIdentMismatch(Vec<IdentField>),
    LoadLibraryAddress(ErrorCode),
    CreateProcess(ErrorCode),
    VirtualAlloc(ErrorCode),
    WriteMemory(ErrorCode),
    CreateRemoteThread(ErrorCode),
    RemoteThreadTimedOut,
    RemoteThreadWaitFailed(ErrorCode),
    LoadLibraryReturnedZero,
    ExitCodeUnavailable(ErrorCode),
    ResumeFailed(ErrorCode),
    PatchRead(usize, ErrorCode),
    PatchMismatch { at: usize, found: Vec<u8> },
    PatchWrite(usize, ErrorCode),
    PatchVerify { at: usize, found: Option<Vec<u8>> },
}

impl LaunchError {
    /// 後始末の行番号。`LoadLibraryAddress` は行 3 と同じ扱い、`RemoteThreadWaitFailed` は行 7、`ExitCodeUnavailable` は行 8、
    /// `ResumeFailed` は行 9 の後、`Patch*` は行 3 と 4 の間（後始末は行 4 と同じ）。
    pub fn row(&self) -> u8 {
        match self {
            LaunchError::DllPathInvalid(_)
            | LaunchError::DllUnreadable(_)
            | LaunchError::DllNotPe32I386(_) => 1,
            LaunchError::GamePathInvalid
            | LaunchError::GameUnreadable(_)
            | LaunchError::GameIdentMismatch(_) => 2,
            LaunchError::LoadLibraryAddress(_) | LaunchError::CreateProcess(_) => 3,
            LaunchError::VirtualAlloc(_)
            | LaunchError::PatchRead(..)
            | LaunchError::PatchMismatch { .. }
            | LaunchError::PatchWrite(..)
            | LaunchError::PatchVerify { .. } => 4,
            LaunchError::WriteMemory(_) => 5,
            LaunchError::CreateRemoteThread(_) => 6,
            LaunchError::RemoteThreadTimedOut | LaunchError::RemoteThreadWaitFailed(_) => 7,
            LaunchError::LoadLibraryReturnedZero | LaunchError::ExitCodeUnavailable(_) => 8,
            LaunchError::ResumeFailed(_) => 9,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupIssue {
    FreeFailed(ErrorCode),
    TerminateFailed(ErrorCode),
    TerminateNotConfirmed(WaitResult),
    CloseFailed(Handle, ErrorCode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFailure {
    pub error: LaunchError,
    pub cleanup_issues: Vec<CleanupIssue>,
}

pub const DLL_NOT_ABSOLUTE: Tx = Tx {
    ja: "絶対パスではありません",
    en: "not an absolute path",
};
pub const DLL_PATH_HAS_NUL: Tx = Tx {
    ja: "パスの途中に0があります",
    en: "the path contains a NUL character",
};

/// UTF-16LE・終端の 0 つき。絶対パスに限るのは、相対だとゲームのフォルダの同名の別 DLL を読みうるため。
pub fn dll_path_bytes(dll: &Path) -> Result<Vec<u8>, LaunchError> {
    if !dll.is_absolute() {
        return Err(LaunchError::DllPathInvalid(DLL_NOT_ABSOLUTE));
    }
    let wide: Vec<u16> = dll.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(LaunchError::DllPathInvalid(DLL_PATH_HAS_NUL));
    }
    let mut bytes = Vec::with_capacity((wide.len() + 1) * 2);
    for w in wide.iter().copied().chain(std::iter::once(0u16)) {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    Ok(bytes)
}

pub fn launch<W: Win32>(w: &mut W, req: &LaunchRequest<'_>) -> Result<Launched, LaunchFailure> {
    let fail = |error| LaunchFailure {
        error,
        cleanup_issues: Vec::new(),
    };

    // 行 1: 何も始める前に見る（後始末は無い）。
    let path_bytes = dll_path_bytes(&req.dll).map_err(fail)?;
    let dll_bytes = w
        .read_file(&req.dll)
        .map_err(|e| fail(LaunchError::DllUnreadable(e)))?;
    pe::parse(&dll_bytes).map_err(|e| fail(LaunchError::DllNotPe32I386(e)))?;

    let cwd = req
        .game
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| fail(LaunchError::GamePathInvalid))?
        .to_path_buf();
    let game_bytes = w
        .read_file(&req.game)
        .map_err(|e| fail(LaunchError::GameUnreadable(e)))?;
    pe::check_ident(&game_bytes, &req.ident)
        .map_err(|m| fail(LaunchError::GameIdentMismatch(m.fields)))?;

    // 行 3 の前: アドレスを先に引いておく（プロセスを作ってから失敗すると、殺すものが 1 つ増える）。
    let load_library_w = w
        .load_library_w_address()
        .map_err(|e| fail(LaunchError::LoadLibraryAddress(e)))?;

    let spawn = Spawn {
        exe: req.game.clone(),
        args: req.args.clone(),
        cwd,
        env: req.env.clone(),
        no_activate: req.no_activate,
    };
    let pi = w
        .create_process_suspended(&spawn)
        .map_err(|e| fail(LaunchError::CreateProcess(e)))?;

    // 行 4〜9。ここから先はプロセスがあるので、どの行でもハンドルを閉じてから戻る。
    let mut issues = Vec::new();
    let result = apply_patches(w, &pi, &req.patches, &mut issues)
        .and_then(|()| inject_and_resume(w, &pi, &path_bytes, load_library_w, &mut issues));

    close(w, pi.thread, &mut issues);
    match result {
        Ok(module) => {
            let process = if req.keep_process_handle {
                Some(pi.process)
            } else {
                close(w, pi.process, &mut issues);
                None
            };
            Ok(Launched {
                pid: pi.pid,
                module,
                process,
                cleanup_issues: issues,
            })
        }
        Err(error) => {
            close(w, pi.process, &mut issues);
            Err(LaunchFailure {
                error,
                cleanup_issues: issues,
            })
        }
    }
}

/// 起動の前の書き換え（行 3 と 4 の間）。1 か所ずつ 読む → 照合 → 書く → 読み直す。合わなければ始めずに終える（後始末は行 4 と同じ）。
fn apply_patches<W: Win32>(
    w: &mut W,
    pi: &ProcessInfo,
    patches: &[CodePatch],
    issues: &mut Vec<CleanupIssue>,
) -> Result<(), LaunchError> {
    for p in patches {
        if let Err(e) = apply_patch(w, pi, p) {
            abort_game(w, pi, issues);
            return Err(e);
        }
    }
    Ok(())
}

fn apply_patch<W: Win32>(w: &mut W, pi: &ProcessInfo, p: &CodePatch) -> Result<(), LaunchError> {
    if p.expect.len() != p.new.len() || p.expect.is_empty() {
        return Err(LaunchError::PatchMismatch {
            at: p.at,
            found: Vec::new(),
        });
    }
    let found = w
        .read_memory(pi.process, p.at, p.expect.len())
        .map_err(|e| LaunchError::PatchRead(p.at, e))?;
    if found != p.expect {
        return Err(LaunchError::PatchMismatch { at: p.at, found });
    }
    w.patch_code(pi.process, p.at, p.new)
        .map_err(|e| LaunchError::PatchWrite(p.at, e))?;
    match w.read_memory(pi.process, p.at, p.new.len()) {
        Ok(back) if back == p.new => Ok(()),
        Ok(back) => Err(LaunchError::PatchVerify {
            at: p.at,
            found: Some(back),
        }),
        Err(_) => Err(LaunchError::PatchVerify {
            at: p.at,
            found: None,
        }),
    }
}

/// 行 4〜9。リモートスレッドのハンドルはここで閉じ、プロセスとスレッドは呼んだ側が閉じる。
fn inject_and_resume<W: Win32>(
    w: &mut W,
    pi: &ProcessInfo,
    path_bytes: &[u8],
    load_library_w: usize,
    issues: &mut Vec<CleanupIssue>,
) -> Result<u32, LaunchError> {
    let region = match w.virtual_alloc(pi.process, path_bytes.len()) {
        Ok(r) => r,
        Err(e) => {
            abort_game(w, pi, issues);
            return Err(LaunchError::VirtualAlloc(e));
        }
    };

    // 行 5: 領域を返してから殺す（殺した後のプロセスには VirtualFreeEx が効かず、失敗が混ざる）。
    if let Err(e) = w.write_memory(pi.process, region, path_bytes) {
        free(w, pi, region, issues);
        abort_game(w, pi, issues);
        return Err(LaunchError::WriteMemory(e));
    }

    let remote = match w.create_remote_thread(pi.process, load_library_w, region) {
        Ok(h) => h,
        Err(e) => {
            free(w, pi, region, issues);
            abort_game(w, pi, issues);
            return Err(LaunchError::CreateRemoteThread(e));
        }
    };

    // 行 7: 時間切れでは領域を返さない（スレッドがまだパスを読んでいるかもしれない）。プロセスごと消せば領域も返る。
    let mut waited = w.wait(remote, REMOTE_THREAD_NOTICE_MS);
    if waited == WaitResult::TimedOut && w.wait(pi.process, 0) == WaitResult::TimedOut {
        w.note(LaunchNote::SlowLoad);
        waited = w.wait(remote, REMOTE_THREAD_TIMEOUT_MS - REMOTE_THREAD_NOTICE_MS);
    }
    match waited {
        WaitResult::Signaled => {}
        other => {
            close(w, remote, issues);
            abort_game(w, pi, issues);
            return Err(match other {
                WaitResult::Failed(e) => LaunchError::RemoteThreadWaitFailed(e),
                _ => LaunchError::RemoteThreadTimedOut,
            });
        }
    }

    let exit = w.thread_exit_code(remote);
    close(w, remote, issues);
    free(w, pi, region, issues);

    // 行 8: 0 は `LoadLibraryW` の失敗。32bit の相手なので終了コードに HMODULE がそのまま入る。
    let module = match exit {
        Ok(0) => {
            abort_game(w, pi, issues);
            return Err(LaunchError::LoadLibraryReturnedZero);
        }
        Err(e) => {
            abort_game(w, pi, issues);
            return Err(LaunchError::ExitCodeUnavailable(e));
        }
        Ok(m) => m,
    };

    if let Err(e) = w.resume_thread(pi.thread) {
        abort_game(w, pi, issues);
        return Err(LaunchError::ResumeFailed(e));
    }
    Ok(module)
}

fn abort_game<W: Win32>(w: &mut W, pi: &ProcessInfo, issues: &mut Vec<CleanupIssue>) {
    if let Err(e) = w.terminate_process(pi.process, ABORT_EXIT_CODE) {
        issues.push(CleanupIssue::TerminateFailed(e));
        return;
    }
    match w.wait(pi.process, TERMINATE_WAIT_MS) {
        WaitResult::Signaled => {}
        other => issues.push(CleanupIssue::TerminateNotConfirmed(other)),
    }
}

fn free<W: Win32>(w: &mut W, pi: &ProcessInfo, region: Remote, issues: &mut Vec<CleanupIssue>) {
    if let Err(e) = w.virtual_free(pi.process, region) {
        issues.push(CleanupIssue::FreeFailed(e));
    }
}

fn close<W: Win32>(w: &mut W, h: Handle, issues: &mut Vec<CleanupIssue>) {
    if let Err(e) = w.close_handle(h) {
        issues.push(CleanupIssue::CloseFailed(h, e));
    }
}

#[cfg(test)]
mod tests {
    //! 行 1〜9 を偽物の Win32（`Fake`。失敗地点だけを決める）で通す。呼び出しの順とハンドル・領域の帳簿は `ledger::Traced`。
    //! ファイルは合成の PE。
    use super::*;
    use crate::ledger::{Call, Traced};
    use crate::staged::Logged;
    use crate::synth;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    const GAME: &str = r"C:\fake\game\target.exe";
    const GAME_DIR: &str = r"C:\fake\game";
    const DLL: &str = r"C:\fake\tool\HiganRTool.dll";
    const ENTRY: [u8; 6] = [0x6A, 0x01, 0x90, 0x90, 0xC3, 0xCC];
    const STAMP: u32 = 0x5A5A_0001;
    const TEXT_SIZE: u32 = 0x0001_0000;
    const LLW: usize = 0x7700_1234;
    const MODULE: u32 = 0x1000_0000;
    const PID: u32 = 4242;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailAt {
        None,
        LoadLibraryAddress,
        CreateProcess,
        VirtualAlloc,
        WriteMemory,
        CreateRemoteThread,
        RemoteWaitTimeout,
        RemoteWaitFailed,
        LoadLibraryZero,
        ExitCodeUnavailable,
        Resume,
        /// 行 9 は成功のまま、領域を返すのだけ失敗する。
        FreeOnly,
        PatchWrite,
        PatchLies,
        /// 最初の待ちは時間切れ、ゲームのプロセスは生きていて、延ばした待ちで終わる。
        SlowLoad,
        /// 最初の待ちも延ばした待ちも時間切れ（プロセスは生きている）。
        SlowLoadNeverEnds,
    }

    struct Fake {
        files: HashMap<PathBuf, Vec<u8>>,
        fail: FailAt,
        next_handle: usize,
        process: Option<Handle>,
        remote_thread: Option<Handle>,
        mem: HashMap<usize, u8>,
        remote_waits: u32,
        notes: Vec<LaunchNote>,
        /// ログの行の置き場（`staged::Logged` の検査用）。呼ばれた瞬間の最後の行を `at_call` に残す。
        trail: Option<Rc<RefCell<Vec<String>>>>,
        at_call: Vec<(&'static str, String)>,
    }

    impl Fake {
        fn new(fail: FailAt) -> Self {
            let mut files = HashMap::new();
            files.insert(PathBuf::from(DLL), synth::pe32(1, 0x200, &[0xC3]));
            files.insert(PathBuf::from(GAME), synth::pe32(STAMP, TEXT_SIZE, &ENTRY));
            Self {
                files,
                fail,
                // 値はわざと 4 の倍数の途中から（0 や 1 から始めると「閉じ忘れ」と「未使用」が紛れる）
                next_handle: 0x1C4,
                process: None,
                remote_thread: None,
                mem: HashMap::new(),
                remote_waits: 0,
                notes: Vec::new(),
                trail: None,
                at_call: Vec::new(),
            }
        }
        fn mark(&mut self, what: &'static str) {
            if let Some(t) = &self.trail {
                let last = t.borrow().last().cloned().unwrap_or_default();
                self.at_call.push((what, last));
            }
        }
        fn with_mem(mut self, at: usize, bytes: &[u8]) -> Self {
            for (i, b) in bytes.iter().enumerate() {
                self.mem.insert(at + i, *b);
            }
            self
        }
        fn handle(&mut self) -> Handle {
            let h = Handle(self.next_handle);
            self.next_handle += 4;
            h
        }
    }

    impl Win32 for Fake {
        fn read_file(&mut self, path: &Path) -> Result<Vec<u8>, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| "ファイルが無い（偽物）".to_string())
        }
        fn load_library_w_address(&mut self) -> Result<usize, ErrorCode> {
            if self.fail == FailAt::LoadLibraryAddress {
                return Err(127);
            }
            Ok(LLW)
        }
        fn create_process_suspended(&mut self, _spawn: &Spawn) -> Result<ProcessInfo, ErrorCode> {
            self.mark("CreateProcess");
            if self.fail == FailAt::CreateProcess {
                return Err(5); // ERROR_ACCESS_DENIED
            }
            let process = self.handle();
            let thread = self.handle();
            self.process = Some(process);
            Ok(ProcessInfo {
                process,
                thread,
                pid: PID,
            })
        }
        fn virtual_alloc(&mut self, _p: Handle, _size: usize) -> Result<Remote, ErrorCode> {
            self.mark("VirtualAlloc");
            if self.fail == FailAt::VirtualAlloc {
                return Err(8);
            }
            Ok(Remote(0x0023_0000))
        }
        fn write_memory(&mut self, _p: Handle, _at: Remote, _d: &[u8]) -> Result<(), ErrorCode> {
            self.mark("WriteMemory");
            if self.fail == FailAt::WriteMemory {
                return Err(299);
            }
            Ok(())
        }
        fn read_memory(&mut self, _p: Handle, at: usize, len: usize) -> Result<Vec<u8>, ErrorCode> {
            (0..len)
                .map(|i| self.mem.get(&(at + i)).copied().ok_or(299))
                .collect()
        }
        fn patch_code(&mut self, _p: Handle, at: usize, data: &[u8]) -> Result<(), ErrorCode> {
            match self.fail {
                FailAt::PatchWrite => Err(5),
                FailAt::PatchLies => Ok(()),
                _ => {
                    for (i, b) in data.iter().enumerate() {
                        self.mem.insert(at + i, *b);
                    }
                    Ok(())
                }
            }
        }
        fn create_remote_thread(
            &mut self,
            _p: Handle,
            _start: usize,
            _param: Remote,
        ) -> Result<Handle, ErrorCode> {
            self.mark("CreateRemoteThread");
            if self.fail == FailAt::CreateRemoteThread {
                return Err(5);
            }
            let h = self.handle();
            self.remote_thread = Some(h);
            Ok(h)
        }
        fn wait(&mut self, handle: Handle, ms: u32) -> WaitResult {
            // 失敗を仕込むのはリモートスレッドの待ちだけ。
            if Some(handle) == self.remote_thread {
                self.remote_waits += 1;
                match self.fail {
                    FailAt::RemoteWaitTimeout | FailAt::SlowLoadNeverEnds => {
                        return WaitResult::TimedOut;
                    }
                    FailAt::SlowLoad if self.remote_waits == 1 => return WaitResult::TimedOut,
                    FailAt::RemoteWaitFailed => return WaitResult::Failed(6),
                    _ => {}
                }
            }
            // 生きているかの問い合わせ（待ち 0）にだけ「生きている」と答える。
            if Some(handle) == self.process
                && ms == 0
                && matches!(self.fail, FailAt::SlowLoad | FailAt::SlowLoadNeverEnds)
            {
                return WaitResult::TimedOut;
            }
            WaitResult::Signaled
        }
        fn note(&mut self, note: LaunchNote) {
            self.notes.push(note);
        }
        fn thread_exit_code(&mut self, _t: Handle) -> Result<u32, ErrorCode> {
            match self.fail {
                FailAt::LoadLibraryZero => Ok(0),
                FailAt::ExitCodeUnavailable => Err(6),
                _ => Ok(MODULE),
            }
        }
        fn virtual_free(&mut self, _p: Handle, _at: Remote) -> Result<(), ErrorCode> {
            if self.fail == FailAt::FreeOnly {
                return Err(87);
            }
            Ok(())
        }
        fn resume_thread(&mut self, _t: Handle) -> Result<u32, ErrorCode> {
            self.mark("Resume");
            if self.fail == FailAt::Resume {
                return Err(6);
            }
            Ok(1)
        }
        fn terminate_process(&mut self, _p: Handle, _code: u32) -> Result<(), ErrorCode> {
            Ok(())
        }
        fn close_handle(&mut self, _h: Handle) -> Result<(), ErrorCode> {
            Ok(())
        }
    }

    fn request(keep: bool) -> LaunchRequest<'static> {
        LaunchRequest {
            game: PathBuf::from(GAME),
            dll: PathBuf::from(DLL),
            args: Vec::new(),
            env: Vec::new(),
            ident: Ident {
                time_date_stamp: STAMP,
                text_size_of_raw_data: TEXT_SIZE,
                entry_bytes: &ENTRY,
            },
            keep_process_handle: keep,
            patches: Vec::new(),
            no_activate: false,
        }
    }

    fn run(fake: Fake, req: &LaunchRequest<'_>) -> (Result<Launched, LaunchFailure>, Traced<Fake>) {
        let mut t = Traced::new(fake);
        let r = launch(&mut t, req);
        (r, t)
    }

    fn run_fail(fail: FailAt) -> (LaunchFailure, Traced<Fake>) {
        let (r, t) = run(Fake::new(fail), &request(false));
        (r.expect_err("失敗するはず"), t)
    }

    fn assert_all_closed(t: &Traced<Fake>) {
        assert!(
            t.open_handles().is_empty(),
            "閉じていないハンドル: {:?}",
            t.open_handles()
        );
        assert!(
            t.bad_closes().is_empty(),
            "二重・未知の close: {:?}",
            t.bad_closes()
        );
    }

    fn assert_aborted(t: &Traced<Fake>) {
        assert_eq!(t.count("TerminateProcess"), 1, "{:?}", t.call_names());
        assert_eq!(t.count("ResumeThread"), 0, "{:?}", t.call_names());
        let names = t.call_names();
        let term = names.iter().position(|n| *n == "TerminateProcess").unwrap();
        assert_eq!(
            names.get(term + 1),
            Some(&"Wait"),
            "殺した後に待っていない: {names:?}"
        );
        let proc_h = t.inner.process.expect("プロセスは作られている");
        assert!(
            t.calls()
                .iter()
                .any(|c| matches!(c, Call::Wait { handle, timeout_ms: TERMINATE_WAIT_MS, .. } if *handle == proc_h)),
            "殺したプロセスを待っていない"
        );
    }

    fn assert_nothing_started(t: &Traced<Fake>) {
        for n in [
            "VirtualAlloc",
            "WriteMemory",
            "CreateRemoteThread",
            "ResumeThread",
            "TerminateProcess",
            "CloseHandle",
        ] {
            assert_eq!(t.count(n), 0, "{n} が呼ばれた: {:?}", t.call_names());
        }
        assert!(t.live_regions().is_empty());
        assert_all_closed(t);
    }

    fn request_second() -> LaunchRequest<'static> {
        let mut r = request(false);
        r.patches = vec![SECOND_INSTANCE];
        r
    }

    #[test]
    fn isolated_instance_patch_drops_only_the_mutex_name() {
        assert_eq!(ISOLATED_INSTANCE.at, 0x0042_D909);
        assert_eq!(ISOLATED_INSTANCE.expect, &[0x68, 0x44, 0x02, 0x49, 0x00]);
        assert_eq!(ISOLATED_INSTANCE.new, &[0x6A, 0x00, 0x90, 0x90, 0x90]);
        assert_eq!(ISOLATED_INSTANCE.expect.len(), ISOLATED_INSTANCE.new.len());
        // 書き換えの範囲が jne（SECOND_INSTANCE）と重ならない。
        assert!(ISOLATED_INSTANCE.at + ISOLATED_INSTANCE.new.len() <= SECOND_INSTANCE.at);
        let fake = Fake::new(FailAt::None).with_mem(ISOLATED_INSTANCE.at, ISOLATED_INSTANCE.expect);
        let mut req = request(false);
        req.patches = vec![ISOLATED_INSTANCE];
        let (r, t) = run(fake, &req);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(t.inner.mem[&ISOLATED_INSTANCE.at], 0x6A);
        assert_eq!(t.inner.mem[&(ISOLATED_INSTANCE.at + 1)], 0x00);
        assert_eq!(t.inner.mem[&(ISOLATED_INSTANCE.at + 4)], 0x90);
    }

    #[test]
    fn second_instance_patch_is_one_jne_to_jmp() {
        assert_eq!(SECOND_INSTANCE.at, 0x0042_D928);
        assert_eq!(SECOND_INSTANCE.expect, &[0x75, 0x19]);
        assert_eq!(SECOND_INSTANCE.new, &[0xEB, 0x19]);
        assert_eq!(SECOND_INSTANCE.expect.len(), SECOND_INSTANCE.new.len());
    }

    #[test]
    fn patch_is_checked_written_and_reread_before_injection() {
        let fake = Fake::new(FailAt::None).with_mem(SECOND_INSTANCE.at, SECOND_INSTANCE.expect);
        let (r, t) = run(fake, &request_second());
        assert!(r.is_ok(), "{r:?}");
        let names = t.call_names();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert_eq!(t.count("PatchCode"), 1, "{names:?}");
        assert_eq!(t.count("ReadMemory"), 2, "{names:?}");
        assert!(pos("CreateProcessSuspended") < pos("ReadMemory"));
        assert!(pos("ReadMemory") < pos("PatchCode"));
        assert!(
            pos("PatchCode") < pos("VirtualAlloc"),
            "注入より前に書く: {names:?}"
        );
        assert_eq!(t.inner.mem[&SECOND_INSTANCE.at], 0xEB);
        assert!(t.calls().iter().any(|c| matches!(c,
            Call::PatchCode { at, data, ok: true, .. } if *at == SECOND_INSTANCE.at && data == &[0xEB, 0x19])));
        assert_all_closed(&t);
    }

    #[test]
    fn patch_mismatch_writes_nothing_and_aborts() {
        let fake = Fake::new(FailAt::None).with_mem(SECOND_INSTANCE.at, &[0x74, 0x19]);
        let (r, t) = run(fake, &request_second());
        let f = r.unwrap_err();
        assert!(
            matches!(&f.error, LaunchError::PatchMismatch { found, .. } if found == &[0x74, 0x19]),
            "{f:?}"
        );
        assert_eq!(f.error.row(), 4);
        assert_eq!(t.count("PatchCode"), 0);
        assert_eq!(t.count("VirtualAlloc"), 0);
        assert_aborted(&t);
        assert_all_closed(&t);
    }

    #[test]
    fn patch_unreadable_writes_nothing_and_aborts() {
        let (r, t) = run(Fake::new(FailAt::None), &request_second());
        let f = r.unwrap_err();
        assert!(matches!(f.error, LaunchError::PatchRead(..)), "{f:?}");
        assert_eq!(t.count("PatchCode"), 0);
        assert_aborted(&t);
        assert_all_closed(&t);
    }

    #[test]
    fn patch_write_failure_aborts() {
        let fake =
            Fake::new(FailAt::PatchWrite).with_mem(SECOND_INSTANCE.at, SECOND_INSTANCE.expect);
        let (r, t) = run(fake, &request_second());
        let f = r.unwrap_err();
        assert!(matches!(f.error, LaunchError::PatchWrite(..)), "{f:?}");
        assert_eq!(t.count("VirtualAlloc"), 0);
        assert_aborted(&t);
        assert_all_closed(&t);
    }

    #[test]
    fn patch_reread_mismatch_aborts() {
        let fake =
            Fake::new(FailAt::PatchLies).with_mem(SECOND_INSTANCE.at, SECOND_INSTANCE.expect);
        let (r, t) = run(fake, &request_second());
        let f = r.unwrap_err();
        assert!(
            matches!(&f.error, LaunchError::PatchVerify { found: Some(b), .. } if b == &[0x75, 0x19]),
            "{f:?}"
        );
        assert_eq!(t.count("VirtualAlloc"), 0);
        assert_aborted(&t);
        assert_all_closed(&t);
    }

    #[test]
    fn no_patch_requested_touches_no_code() {
        let (r, t) = run(Fake::new(FailAt::None), &request(false));
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(t.count("ReadMemory"), 0);
        assert_eq!(t.count("PatchCode"), 0);
    }

    #[test]
    fn row1_dll_missing_starts_nothing() {
        let mut fake = Fake::new(FailAt::None);
        fake.files.remove(&PathBuf::from(DLL));
        let (r, t) = run(fake, &request(false));
        let f = r.unwrap_err();
        assert!(matches!(f.error, LaunchError::DllUnreadable(_)), "{f:?}");
        assert_eq!(f.error.row(), 1);
        assert_eq!(t.count("CreateProcessSuspended"), 0);
        assert_nothing_started(&t);
    }

    #[test]
    fn row1_dll_not_pe32_i386_starts_nothing() {
        let mut pe32plus = synth::pe32(1, 0x200, &[0xC3]);
        pe32plus[0x58] = 0x0B; // optional header の magic を 0x20B（PE32+）へ
        pe32plus[0x59] = 0x02;
        for (bytes, want) in [
            (pe32plus, PeError::Pe32Plus),
            (
                b"not a PE at all, just text padding....................................".to_vec(),
                PeError::NotMz,
            ),
        ] {
            let mut fake = Fake::new(FailAt::None);
            fake.files.insert(PathBuf::from(DLL), bytes);
            let (r, t) = run(fake, &request(false));
            let f = r.unwrap_err();
            assert_eq!(f.error, LaunchError::DllNotPe32I386(want));
            assert_eq!(f.error.row(), 1);
            assert_eq!(t.count("CreateProcessSuspended"), 0);
            assert_nothing_started(&t);
        }
    }

    #[test]
    fn row1_relative_dll_path_rejected_before_reading() {
        let mut req = request(false);
        req.dll = PathBuf::from("HiganRTool.dll");
        let (r, t) = run(Fake::new(FailAt::None), &req);
        let f = r.unwrap_err();
        assert!(matches!(f.error, LaunchError::DllPathInvalid(_)), "{f:?}");
        assert_eq!(f.error.row(), 1);
        assert!(t.calls().is_empty(), "{:?}", t.calls());
    }

    #[test]
    fn row2_ident_mismatch_starts_nothing() {
        let mut fake = Fake::new(FailAt::None);
        // 時刻だけ違う exe（他の 2 欄は合う）でも止まること。
        fake.files.insert(
            PathBuf::from(GAME),
            synth::pe32(STAMP + 1, TEXT_SIZE, &ENTRY),
        );
        let (r, t) = run(fake, &request(false));
        let f = r.unwrap_err();
        assert_eq!(
            f.error,
            LaunchError::GameIdentMismatch(vec![IdentField::TimeDateStamp])
        );
        assert_eq!(f.error.row(), 2);
        assert_eq!(t.count("CreateProcessSuspended"), 0);
        assert_nothing_started(&t);
    }

    #[test]
    fn row2_game_unreadable_starts_nothing() {
        let mut fake = Fake::new(FailAt::None);
        fake.files.remove(&PathBuf::from(GAME));
        let (r, t) = run(fake, &request(false));
        let f = r.unwrap_err();
        assert!(matches!(f.error, LaunchError::GameUnreadable(_)), "{f:?}");
        assert_eq!(f.error.row(), 2);
        assert_nothing_started(&t);
    }

    #[test]
    fn row3_create_process_failure_ends() {
        let (f, t) = run_fail(FailAt::CreateProcess);
        assert_eq!(f.error, LaunchError::CreateProcess(5));
        assert_eq!(f.error.row(), 3);
        assert_eq!(
            t.call_names(),
            vec!["LoadLibraryWAddress", "CreateProcessSuspended"]
        );
        assert_nothing_started(&t);
    }

    #[test]
    fn row3_load_library_address_failure_before_process() {
        let (f, t) = run_fail(FailAt::LoadLibraryAddress);
        assert_eq!(f.error, LaunchError::LoadLibraryAddress(127));
        assert_eq!(f.error.row(), 3);
        assert_eq!(t.count("CreateProcessSuspended"), 0);
        assert_nothing_started(&t);
    }

    #[test]
    fn row4_alloc_failure_terminates_and_closes() {
        let (f, t) = run_fail(FailAt::VirtualAlloc);
        assert_eq!(f.error, LaunchError::VirtualAlloc(8));
        assert_eq!(f.error.row(), 4);
        assert_aborted(&t);
        assert_eq!(t.count("VirtualFree"), 0);
        assert_all_closed(&t);
        assert!(t.live_regions().is_empty());
    }

    #[test]
    fn row5_write_failure_frees_then_terminates() {
        let (f, t) = run_fail(FailAt::WriteMemory);
        assert_eq!(f.error, LaunchError::WriteMemory(299));
        assert_eq!(f.error.row(), 5);
        assert_aborted(&t);
        assert!(
            t.live_regions().is_empty(),
            "領域が残った: {:?}",
            t.live_regions()
        );
        let names = t.call_names();
        let free = names.iter().position(|n| *n == "VirtualFree").unwrap();
        let term = names.iter().position(|n| *n == "TerminateProcess").unwrap();
        assert!(free < term, "返す前に殺した: {names:?}");
        assert_all_closed(&t);
    }

    #[test]
    fn row6_remote_thread_failure_frees_then_terminates() {
        let (f, t) = run_fail(FailAt::CreateRemoteThread);
        assert_eq!(f.error, LaunchError::CreateRemoteThread(5));
        assert_eq!(f.error.row(), 6);
        assert_aborted(&t);
        assert!(
            t.live_regions().is_empty(),
            "領域が残った: {:?}",
            t.live_regions()
        );
        let names = t.call_names();
        let free = names.iter().position(|n| *n == "VirtualFree").unwrap();
        let term = names.iter().position(|n| *n == "TerminateProcess").unwrap();
        assert!(free < term, "返す前に殺した: {names:?}");
        assert_all_closed(&t);
    }

    #[test]
    fn row7_timeout_keeps_region_and_terminates() {
        for (fail, want) in [
            (FailAt::RemoteWaitTimeout, LaunchError::RemoteThreadTimedOut),
            (
                FailAt::RemoteWaitFailed,
                LaunchError::RemoteThreadWaitFailed(6),
            ),
        ] {
            let (f, t) = run_fail(fail);
            assert_eq!(f.error, want);
            assert_eq!(f.error.row(), 7);
            assert_aborted(&t);
            assert_eq!(t.count("VirtualFree"), 0, "{:?}", t.call_names());
            assert_eq!(t.live_regions().len(), 1, "行 7 だけは領域を返さない");
            assert_eq!(t.count("ThreadExitCode"), 0);
            let remote = t.inner.remote_thread.unwrap();
            assert!(t.calls().iter().any(|c| matches!(
                c,
                Call::Wait { handle, timeout_ms: 10_000, .. } if *handle == remote
            )));
            assert_all_closed(&t);
        }
    }

    fn remote_waits(t: &Traced<Fake>) -> Vec<u32> {
        let remote = t.inner.remote_thread.unwrap();
        t.calls()
            .iter()
            .filter_map(|c| match c {
                Call::Wait {
                    handle, timeout_ms, ..
                } if *handle == remote => Some(*timeout_ms),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn row7_a_slow_load_is_announced_and_waited_for_while_the_game_lives() {
        let (r, t) = run(Fake::new(FailAt::SlowLoad), &request(false));
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(t.inner.notes, vec![LaunchNote::SlowLoad]);
        assert_eq!(
            remote_waits(&t),
            vec![
                REMOTE_THREAD_NOTICE_MS,
                REMOTE_THREAD_TIMEOUT_MS - REMOTE_THREAD_NOTICE_MS
            ]
        );
        assert_eq!(t.count("ResumeThread"), 1);
        assert_eq!(t.count("TerminateProcess"), 0);
        assert_all_closed(&t);
    }

    #[test]
    fn row7_gives_up_after_the_full_limit_even_while_the_game_lives() {
        let (f, t) = run_fail(FailAt::SlowLoadNeverEnds);
        assert_eq!(f.error, LaunchError::RemoteThreadTimedOut);
        assert_eq!(t.inner.notes, vec![LaunchNote::SlowLoad]);
        assert_eq!(
            remote_waits(&t).iter().sum::<u32>(),
            REMOTE_THREAD_TIMEOUT_MS
        );
        assert_aborted(&t);
        assert_all_closed(&t);
    }

    #[test]
    fn row7_does_not_extend_the_wait_when_the_game_is_already_gone() {
        let (f, t) = run_fail(FailAt::RemoteWaitTimeout);
        assert_eq!(f.error, LaunchError::RemoteThreadTimedOut);
        assert!(t.inner.notes.is_empty(), "{:?}", t.inner.notes);
        assert_eq!(remote_waits(&t), vec![REMOTE_THREAD_NOTICE_MS]);
    }

    #[test]
    fn a_normal_load_waits_once_and_says_nothing() {
        let (r, t) = run(Fake::new(FailAt::None), &request(false));
        assert!(r.is_ok());
        assert!(t.inner.notes.is_empty());
        assert_eq!(remote_waits(&t), vec![REMOTE_THREAD_NOTICE_MS]);
    }

    struct LoggedRun {
        result: Result<Launched, LaunchFailure>,
        lines: Vec<String>,
        /// 各段が呼ばれた瞬間の、ログの最後の行。
        at_call: Vec<(&'static str, String)>,
        shown: Vec<LaunchNote>,
    }

    /// 段階ログの包みで `fake` を起動する。
    fn run_logged(mut fake: Fake) -> LoggedRun {
        let trail = Rc::new(RefCell::new(Vec::<String>::new()));
        fake.trail = Some(Rc::clone(&trail));
        let sink = {
            let t = Rc::clone(&trail);
            move |s: &str| t.borrow_mut().push(s.to_string())
        };
        let shown = RefCell::new(Vec::new());
        let announce = |n| shown.borrow_mut().push(n);
        let mut w = Logged::new(fake, &sink, &announce);
        let r = launch(&mut w, &request(false));
        let at_call = std::mem::take(&mut w.inner.at_call);
        drop(w);
        let lines = trail.borrow().clone();
        LoggedRun {
            result: r,
            lines,
            at_call,
            shown: shown.into_inner(),
        }
    }

    fn index_of(lines: &[String], needle: &str) -> usize {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} の行が無い: {lines:#?}"))
    }

    // 何が壊れたら落ちるか: 段の行を呼ぶ後に書く・段を飛ばす・順を誤ると、落ちた回の最後の行が「どの段で止まったか」を示さない。
    #[test]
    fn every_stage_is_logged_before_its_call_and_in_order() {
        let LoggedRun {
            result,
            lines,
            at_call,
            shown,
        } = run_logged(Fake::new(FailAt::None));
        assert!(result.is_ok(), "{result:?}");
        assert!(shown.is_empty());
        let order = [
            "CreateProcessW を呼ぶ",
            "VirtualAllocEx を呼ぶ",
            "WriteProcessMemory を呼ぶ",
            "CreateRemoteThread を呼ぶ",
            "LoadLibraryW の戻り: 0x10000000（DLL を読み込めた）",
            "ResumeThread を呼ぶ",
        ];
        let at: Vec<usize> = order.iter().map(|n| index_of(&lines, n)).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{at:?} {lines:#?}");
        let want = [
            ("CreateProcess", "CreateProcessW を呼ぶ"),
            ("VirtualAlloc", "VirtualAllocEx を呼ぶ"),
            ("WriteMemory", "WriteProcessMemory を呼ぶ"),
            ("CreateRemoteThread", "CreateRemoteThread を呼ぶ"),
            ("Resume", "ResumeThread を呼ぶ"),
        ];
        assert_eq!(at_call.len(), want.len(), "{at_call:?}");
        for ((what, last), (want_what, phrase)) in at_call.iter().zip(want) {
            assert_eq!(*what, want_what);
            assert!(
                last.contains(phrase),
                "{what} が呼ばれた瞬間の最後の行が「{phrase}」でない: {last}"
            );
        }
        assert!(
            lines.iter().any(|l| l.contains("→ 成功 pid 4242")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("最大 0 ms")),
            "生死の問い合わせ（待ち 0）まで書いた"
        );
    }

    #[test]
    fn a_failed_stage_and_a_zero_load_library_are_logged() {
        let LoggedRun { result, lines, .. } = run_logged(Fake::new(FailAt::CreateProcess));
        assert_eq!(result.unwrap_err().error, LaunchError::CreateProcess(5));
        assert!(
            lines.iter().any(|l| l.contains("→ 失敗 GetLastError 5")),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("VirtualAllocEx")));

        let LoggedRun { result, lines, .. } = run_logged(Fake::new(FailAt::LoadLibraryZero));
        assert_eq!(
            result.unwrap_err().error,
            LaunchError::LoadLibraryReturnedZero
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("LoadLibraryW の戻り: 0（DLL を読み込めなかった）")),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("ResumeThread")));
    }

    #[test]
    fn a_slow_load_is_logged_and_announced_once() {
        let LoggedRun {
            result,
            lines,
            shown,
            ..
        } = run_logged(Fake::new(FailAt::SlowLoad));
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(shown, vec![LaunchNote::SlowLoad]);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("10 秒で終わらない") && l.contains("最大 60 秒")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("待つ（最大 50000 ms）")),
            "{lines:#?}"
        );
    }

    #[test]
    fn row8_load_library_zero_frees_and_terminates() {
        for (fail, want) in [
            (
                FailAt::LoadLibraryZero,
                LaunchError::LoadLibraryReturnedZero,
            ),
            (
                FailAt::ExitCodeUnavailable,
                LaunchError::ExitCodeUnavailable(6),
            ),
        ] {
            let (f, t) = run_fail(fail);
            assert_eq!(f.error, want);
            assert_eq!(f.error.row(), 8);
            assert_aborted(&t);
            assert!(
                t.live_regions().is_empty(),
                "領域が残った: {:?}",
                t.live_regions()
            );
            assert_all_closed(&t);
            assert!(f.cleanup_issues.is_empty(), "{:?}", f.cleanup_issues);
        }
    }

    #[test]
    fn row9_success_frees_and_resumes() {
        let (r, t) = run(Fake::new(FailAt::None), &request(false));
        let ok = r.expect("成功するはず");
        assert_eq!(ok.pid, PID);
        assert_eq!(ok.module, MODULE);
        assert_eq!(ok.process, None);
        assert!(ok.cleanup_issues.is_empty());
        assert_eq!(
            t.call_names(),
            vec![
                "LoadLibraryWAddress",
                "CreateProcessSuspended",
                "VirtualAlloc",
                "WriteMemory",
                "CreateRemoteThread",
                "Wait",
                "ThreadExitCode",
                "CloseHandle", // リモートスレッド
                "VirtualFree",
                "ResumeThread",
                "CloseHandle", // 主スレッド
                "CloseHandle", // プロセス
            ]
        );
        assert!(t.live_regions().is_empty());
        assert_all_closed(&t);

        // UTF-16LE ＋ 終端の 0（LoadLibraryW は 0 終端の LPCWSTR を読む）
        let want: Vec<u8> = DLL
            .encode_utf16()
            .chain([0])
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let written = t.calls().iter().find_map(|c| match c {
            Call::WriteMemory { data, .. } => Some(data.clone()),
            _ => None,
        });
        assert_eq!(written, Some(want.clone()));
        assert!(
            t.calls()
                .iter()
                .any(|c| matches!(c, Call::VirtualAlloc { size, .. } if *size >= want.len()))
        );
        assert!(t.calls().iter().any(|c| matches!(
            c,
            Call::CreateRemoteThread {
                start: LLW,
                param: Remote(0x0023_0000),
                ..
            }
        )));
        assert!(t.calls().iter().any(|c| matches!(
            c,
            Call::CreateProcessSuspended { cwd, .. } if cwd == Path::new(GAME_DIR)
        )));
        let thread_resumed = t.calls().iter().find_map(|c| match c {
            Call::ResumeThread { thread, .. } => Some(*thread),
            _ => None,
        });
        assert_ne!(thread_resumed, t.inner.remote_thread);
    }

    #[test]
    fn env_reaches_create_process_and_is_empty_by_default() {
        let env = vec![
            (
                OsString::from(higanr_core::ini::PATH_ENV),
                OsString::from(r"C:\fake\s\two_host.ini"),
            ),
            (
                OsString::from(higanr_core::script::PATH_ENV),
                OsString::from(r"C:\fake\s\two_01_host.txt"),
            ),
        ];
        let mut req = request(false);
        req.env = env.clone();
        let (r, t) = run(Fake::new(FailAt::None), &req);
        r.expect("成功するはず");
        let got = t.calls().iter().find_map(|c| match c {
            Call::CreateProcessSuspended { env, .. } => Some(env.clone()),
            _ => None,
        });
        assert_eq!(got, Some(env));
        let (r, t) = run(Fake::new(FailAt::None), &request(false));
        r.expect("成功するはず");
        assert!(t.calls().iter().any(|c| matches!(
            c,
            Call::CreateProcessSuspended { env, .. } if env.is_empty()
        )));
    }

    #[test]
    fn no_activate_reaches_create_process_only_when_asked() {
        let seen = |want: bool| {
            let mut req = request(false);
            req.no_activate = want;
            let (r, t) = run(Fake::new(FailAt::None), &req);
            r.expect("成功するはず");
            t.calls()
                .iter()
                .find_map(|c| match c {
                    Call::CreateProcessSuspended { no_activate, .. } => Some(*no_activate),
                    _ => None,
                })
                .expect("プロセスを作ったはず")
        };
        assert!(seen(true));
        assert!(!seen(false));
    }

    #[test]
    fn row9_keep_process_handle_returns_it_open() {
        let (r, mut t) = run(Fake::new(FailAt::None), &request(true));
        let ok = r.expect("成功するはず");
        let h = ok.process.expect("ハンドルを返すはず");
        assert_eq!(
            t.open_handles().iter().copied().collect::<Vec<_>>(),
            vec![h]
        );
        t.close_handle(h).unwrap();
        assert_all_closed(&t);
    }

    #[test]
    fn row9_free_failure_still_resumes_and_reports() {
        let (r, t) = run(Fake::new(FailAt::FreeOnly), &request(false));
        let ok = r.expect("成功するはず");
        assert_eq!(ok.cleanup_issues, vec![CleanupIssue::FreeFailed(87)]);
        assert_eq!(t.count("ResumeThread"), 1);
        assert_eq!(t.count("TerminateProcess"), 0);
        assert_all_closed(&t);
    }

    #[test]
    fn row9_resume_failure_terminates() {
        let (f, t) = run_fail(FailAt::Resume);
        assert_eq!(f.error, LaunchError::ResumeFailed(6));
        assert_eq!(f.error.row(), 9);
        assert_eq!(t.count("TerminateProcess"), 1);
        assert!(t.live_regions().is_empty());
        assert_all_closed(&t);
    }

    #[test]
    fn every_failure_point_maps_to_its_table_row() {
        let table = [
            (FailAt::CreateProcess, 3),
            (FailAt::VirtualAlloc, 4),
            (FailAt::WriteMemory, 5),
            (FailAt::CreateRemoteThread, 6),
            (FailAt::RemoteWaitTimeout, 7),
            (FailAt::LoadLibraryZero, 8),
        ];
        for (fail, row) in table {
            let (f, t) = run_fail(fail);
            assert_eq!(f.error.row(), row, "{fail:?}");
            assert_all_closed(&t);
        }
    }

    #[test]
    fn dll_path_bytes_is_utf16le_with_terminator() {
        let p = Path::new("C:\\ツール\\a.dll");
        let b = dll_path_bytes(p).unwrap();
        let want: Vec<u8> = "C:\\ツール\\a.dll"
            .encode_utf16()
            .chain([0])
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(b, want);
        assert!(dll_path_bytes(Path::new("rel\\a.dll")).is_err());
        let with_nul = PathBuf::from(OsString::from("C:\\a\0b.dll"));
        assert!(dll_path_bytes(&with_nul).is_err());
    }
}
