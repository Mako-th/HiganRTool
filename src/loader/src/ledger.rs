//! Win32 の呼び出しを素通しにしながら、呼ばれた順と「開いたのに閉じていないハンドル」「確保したのに返していない領域」を
//! 帳簿に付ける包み。偽物の検査（`launch.rs`）と自己検査（`selftest.rs`）で共用する。本番の起動では包まない。

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::win::{ErrorCode, Handle, ProcessInfo, Remote, Spawn, WaitResult, Win32};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    ReadFile(PathBuf),
    LoadLibraryWAddress,
    CreateProcessSuspended {
        exe: PathBuf,
        cwd: PathBuf,
        env: Vec<(OsString, OsString)>,
        no_activate: bool,
        ok: bool,
    },
    VirtualAlloc {
        process: Handle,
        size: usize,
        ok: bool,
    },
    WriteMemory {
        process: Handle,
        at: Remote,
        data: Vec<u8>,
        ok: bool,
    },
    ReadMemory {
        process: Handle,
        at: usize,
        len: usize,
        ok: bool,
    },
    PatchCode {
        process: Handle,
        at: usize,
        data: Vec<u8>,
        ok: bool,
    },
    CreateRemoteThread {
        process: Handle,
        start: usize,
        param: Remote,
        ok: bool,
    },
    Wait {
        handle: Handle,
        timeout_ms: u32,
        result: WaitResult,
    },
    ThreadExitCode {
        thread: Handle,
        result: Result<u32, ErrorCode>,
    },
    VirtualFree {
        process: Handle,
        at: Remote,
        ok: bool,
    },
    ResumeThread {
        thread: Handle,
        ok: bool,
    },
    TerminateProcess {
        process: Handle,
        ok: bool,
    },
    CloseHandle {
        handle: Handle,
    },
}

impl Call {
    pub fn name(&self) -> &'static str {
        match self {
            Call::ReadFile(_) => "ReadFile",
            Call::LoadLibraryWAddress => "LoadLibraryWAddress",
            Call::CreateProcessSuspended { .. } => "CreateProcessSuspended",
            Call::VirtualAlloc { .. } => "VirtualAlloc",
            Call::WriteMemory { .. } => "WriteMemory",
            Call::ReadMemory { .. } => "ReadMemory",
            Call::PatchCode { .. } => "PatchCode",
            Call::CreateRemoteThread { .. } => "CreateRemoteThread",
            Call::Wait { .. } => "Wait",
            Call::ThreadExitCode { .. } => "ThreadExitCode",
            Call::VirtualFree { .. } => "VirtualFree",
            Call::ResumeThread { .. } => "ResumeThread",
            Call::TerminateProcess { .. } => "TerminateProcess",
            Call::CloseHandle { .. } => "CloseHandle",
        }
    }
}

pub struct Traced<W: Win32> {
    pub inner: W,
    calls: Vec<Call>,
    open_handles: BTreeSet<Handle>,
    live_regions: BTreeSet<(Handle, Remote)>,
    /// 開いた覚えの無い、または 2 回目のハンドルを閉じようとした記録。
    bad_closes: Vec<Handle>,
    created_pids: Vec<u32>,
}

impl<W: Win32> Traced<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            calls: Vec::new(),
            open_handles: BTreeSet::new(),
            live_regions: BTreeSet::new(),
            bad_closes: Vec::new(),
            created_pids: Vec::new(),
        }
    }

    pub fn calls(&self) -> &[Call] {
        &self.calls
    }

    #[cfg(test)]
    pub fn call_names(&self) -> Vec<&'static str> {
        self.calls
            .iter()
            .filter(|c| !matches!(c, Call::ReadFile(_)))
            .map(Call::name)
            .collect()
    }

    pub fn count(&self, name: &str) -> usize {
        self.calls.iter().filter(|c| c.name() == name).count()
    }

    pub fn open_handles(&self) -> &BTreeSet<Handle> {
        &self.open_handles
    }

    pub fn live_regions(&self) -> &BTreeSet<(Handle, Remote)> {
        &self.live_regions
    }

    pub fn bad_closes(&self) -> &[Handle] {
        &self.bad_closes
    }

    pub fn created_pids(&self) -> &[u32] {
        &self.created_pids
    }

    fn opened(&mut self, h: Handle) {
        // 同じ値が 2 度開くのは本物では起こらない。起きたら二重の close と同じ扱いで記録する。
        if !self.open_handles.insert(h) {
            self.bad_closes.push(h);
        }
    }
}

impl<W: Win32> Win32 for Traced<W> {
    fn read_file(&mut self, path: &Path) -> Result<Vec<u8>, String> {
        self.calls.push(Call::ReadFile(path.to_path_buf()));
        self.inner.read_file(path)
    }

    fn load_library_w_address(&mut self) -> Result<usize, ErrorCode> {
        self.calls.push(Call::LoadLibraryWAddress);
        self.inner.load_library_w_address()
    }

    fn create_process_suspended(&mut self, spawn: &Spawn) -> Result<ProcessInfo, ErrorCode> {
        let r = self.inner.create_process_suspended(spawn);
        self.calls.push(Call::CreateProcessSuspended {
            exe: spawn.exe.clone(),
            cwd: spawn.cwd.clone(),
            env: spawn.env.clone(),
            no_activate: spawn.no_activate,
            ok: r.is_ok(),
        });
        if let Ok(pi) = &r {
            self.opened(pi.process);
            self.opened(pi.thread);
            self.created_pids.push(pi.pid);
        }
        r
    }

    fn virtual_alloc(&mut self, process: Handle, size: usize) -> Result<Remote, ErrorCode> {
        let r = self.inner.virtual_alloc(process, size);
        self.calls.push(Call::VirtualAlloc {
            process,
            size,
            ok: r.is_ok(),
        });
        if let Ok(at) = r {
            self.live_regions.insert((process, at));
        }
        r
    }

    fn write_memory(&mut self, process: Handle, at: Remote, data: &[u8]) -> Result<(), ErrorCode> {
        let r = self.inner.write_memory(process, at, data);
        self.calls.push(Call::WriteMemory {
            process,
            at,
            data: data.to_vec(),
            ok: r.is_ok(),
        });
        r
    }

    fn read_memory(
        &mut self,
        process: Handle,
        at: usize,
        len: usize,
    ) -> Result<Vec<u8>, ErrorCode> {
        let r = self.inner.read_memory(process, at, len);
        self.calls.push(Call::ReadMemory {
            process,
            at,
            len,
            ok: r.is_ok(),
        });
        r
    }

    fn patch_code(&mut self, process: Handle, at: usize, data: &[u8]) -> Result<(), ErrorCode> {
        let r = self.inner.patch_code(process, at, data);
        self.calls.push(Call::PatchCode {
            process,
            at,
            data: data.to_vec(),
            ok: r.is_ok(),
        });
        r
    }

    fn create_remote_thread(
        &mut self,
        process: Handle,
        start: usize,
        param: Remote,
    ) -> Result<Handle, ErrorCode> {
        let r = self.inner.create_remote_thread(process, start, param);
        self.calls.push(Call::CreateRemoteThread {
            process,
            start,
            param,
            ok: r.is_ok(),
        });
        if let Ok(h) = r {
            self.opened(h);
        }
        r
    }

    fn wait(&mut self, handle: Handle, timeout_ms: u32) -> WaitResult {
        let result = self.inner.wait(handle, timeout_ms);
        self.calls.push(Call::Wait {
            handle,
            timeout_ms,
            result,
        });
        result
    }

    fn thread_exit_code(&mut self, thread: Handle) -> Result<u32, ErrorCode> {
        let result = self.inner.thread_exit_code(thread);
        self.calls.push(Call::ThreadExitCode { thread, result });
        result
    }

    fn virtual_free(&mut self, process: Handle, at: Remote) -> Result<(), ErrorCode> {
        let r = self.inner.virtual_free(process, at);
        self.calls.push(Call::VirtualFree {
            process,
            at,
            ok: r.is_ok(),
        });
        if r.is_ok() {
            self.live_regions.remove(&(process, at));
        }
        r
    }

    fn resume_thread(&mut self, thread: Handle) -> Result<u32, ErrorCode> {
        let r = self.inner.resume_thread(thread);
        self.calls.push(Call::ResumeThread {
            thread,
            ok: r.is_ok(),
        });
        r
    }

    fn terminate_process(&mut self, process: Handle, exit_code: u32) -> Result<(), ErrorCode> {
        let r = self.inner.terminate_process(process, exit_code);
        self.calls.push(Call::TerminateProcess {
            process,
            ok: r.is_ok(),
        });
        r
    }

    fn close_handle(&mut self, handle: Handle) -> Result<(), ErrorCode> {
        self.calls.push(Call::CloseHandle { handle });
        if !self.open_handles.remove(&handle) {
            self.bad_closes.push(handle);
        }
        self.inner.close_handle(handle)
    }
}
