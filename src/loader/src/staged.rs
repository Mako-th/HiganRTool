//! 起動の各段（プロセスを作る・領域を確保する・書き込む・読み込みのスレッドを作る・LoadLibraryW の戻り・再開）を、
//! 呼ぶ前に 1 行ログへ書いてから素通しで呼ぶ包み。呼んだ先で落ちても、どの段だったかが最後の行に残る。
//! 結果は呼んだ後に続けて書く。パスは書かない。

use std::path::Path;

use crate::launch::{REMOTE_THREAD_NOTICE_MS, REMOTE_THREAD_TIMEOUT_MS};
use crate::win::{ErrorCode, Handle, LaunchNote, ProcessInfo, Remote, Spawn, WaitResult, Win32};

pub struct Logged<'a, W: Win32> {
    pub inner: W,
    log: &'a dyn Fn(&str),
    /// 画面に出す知らせ。ログへは包みが書く。
    announce: &'a dyn Fn(LaunchNote),
}

impl<'a, W: Win32> Logged<'a, W> {
    pub fn new(inner: W, log: &'a dyn Fn(&str), announce: &'a dyn Fn(LaunchNote)) -> Self {
        Logged {
            inner,
            log,
            announce,
        }
    }

    fn result<T>(&self, r: &Result<T, ErrorCode>, ok: impl FnOnce(&T) -> String) {
        match r {
            Ok(v) => (self.log)(&format!("  → 成功{}", ok(v))),
            Err(e) => (self.log)(&format!("  → 失敗 GetLastError {e}")),
        }
    }
}

impl<W: Win32> Win32 for Logged<'_, W> {
    fn read_file(&mut self, path: &Path) -> Result<Vec<u8>, String> {
        self.inner.read_file(path)
    }

    fn load_library_w_address(&mut self) -> Result<usize, ErrorCode> {
        let r = self.inner.load_library_w_address();
        if let Err(e) = &r {
            (self.log)(&format!(
                "kernel32 の LoadLibraryW のアドレスが引けない GetLastError {e}"
            ));
        }
        r
    }

    fn create_process_suspended(&mut self, spawn: &Spawn) -> Result<ProcessInfo, ErrorCode> {
        (self.log)("CreateProcessW を呼ぶ（ゲームを停止した状態で起動）");
        let r = self.inner.create_process_suspended(spawn);
        self.result(&r, |pi| format!(" pid {}", pi.pid));
        r
    }

    fn virtual_alloc(&mut self, process: Handle, size: usize) -> Result<Remote, ErrorCode> {
        (self.log)(&format!("VirtualAllocEx を呼ぶ（{size} バイト）"));
        let r = self.inner.virtual_alloc(process, size);
        self.result(&r, |_| String::new());
        r
    }

    fn write_memory(&mut self, process: Handle, at: Remote, data: &[u8]) -> Result<(), ErrorCode> {
        (self.log)(&format!(
            "WriteProcessMemory を呼ぶ（{} バイト）",
            data.len()
        ));
        let r = self.inner.write_memory(process, at, data);
        self.result(&r, |_| String::new());
        r
    }

    fn read_memory(
        &mut self,
        process: Handle,
        at: usize,
        len: usize,
    ) -> Result<Vec<u8>, ErrorCode> {
        self.inner.read_memory(process, at, len)
    }

    fn patch_code(&mut self, process: Handle, at: usize, data: &[u8]) -> Result<(), ErrorCode> {
        (self.log)(&format!(
            "ゲームの起動前の書き換え: 0x{at:08X} に {} バイト",
            data.len()
        ));
        let r = self.inner.patch_code(process, at, data);
        self.result(&r, |_| String::new());
        r
    }

    fn create_remote_thread(
        &mut self,
        process: Handle,
        start: usize,
        param: Remote,
    ) -> Result<Handle, ErrorCode> {
        (self.log)("CreateRemoteThread を呼ぶ（LoadLibraryW で DLL を読み込む）");
        let r = self.inner.create_remote_thread(process, start, param);
        self.result(&r, |_| String::new());
        r
    }

    /// 待ち 0（生きているかの問い合わせ）は、ゲームが続く間に何度も呼ばれるのでログに書かない。
    fn wait(&mut self, handle: Handle, timeout_ms: u32) -> WaitResult {
        if timeout_ms == 0 {
            return self.inner.wait(handle, timeout_ms);
        }
        (self.log)(&format!("待つ（最大 {timeout_ms} ms）"));
        let r = self.inner.wait(handle, timeout_ms);
        (self.log)(&match r {
            WaitResult::Signaled => "  → 終わった".to_string(),
            WaitResult::TimedOut => "  → 時間切れ".to_string(),
            WaitResult::Failed(e) => format!("  → 失敗 GetLastError {e}"),
        });
        r
    }

    fn thread_exit_code(&mut self, thread: Handle) -> Result<u32, ErrorCode> {
        let r = self.inner.thread_exit_code(thread);
        (self.log)(&match &r {
            Ok(0) => "LoadLibraryW の戻り: 0（DLL を読み込めなかった）".to_string(),
            Ok(m) => format!("LoadLibraryW の戻り: 0x{m:08X}（DLL を読み込めた）"),
            Err(e) => format!("LoadLibraryW の戻りが読めない GetLastError {e}"),
        });
        r
    }

    fn virtual_free(&mut self, process: Handle, at: Remote) -> Result<(), ErrorCode> {
        let r = self.inner.virtual_free(process, at);
        if let Err(e) = &r {
            (self.log)(&format!("VirtualFreeEx が失敗 GetLastError {e}"));
        }
        r
    }

    fn resume_thread(&mut self, thread: Handle) -> Result<u32, ErrorCode> {
        (self.log)("ResumeThread を呼ぶ（ゲームを動かす）");
        let r = self.inner.resume_thread(thread);
        self.result(&r, |_| String::new());
        r
    }

    fn terminate_process(&mut self, process: Handle, exit_code: u32) -> Result<(), ErrorCode> {
        (self.log)("TerminateProcess を呼ぶ（動かさずに終える）");
        let r = self.inner.terminate_process(process, exit_code);
        self.result(&r, |_| String::new());
        r
    }

    fn close_handle(&mut self, handle: Handle) -> Result<(), ErrorCode> {
        let r = self.inner.close_handle(handle);
        if let Err(e) = &r {
            (self.log)(&format!("CloseHandle が失敗 GetLastError {e}"));
        }
        r
    }

    fn note(&mut self, note: LaunchNote) {
        match note {
            LaunchNote::SlowLoad => (self.log)(&format!(
                "DLL の読み込みが {} 秒で終わらない。ゲームのプロセスは生きているので最大 {} 秒まで待つ",
                REMOTE_THREAD_NOTICE_MS / 1000,
                REMOTE_THREAD_TIMEOUT_MS / 1000
            )),
        }
        (self.announce)(note);
        self.inner.note(note);
    }
}
