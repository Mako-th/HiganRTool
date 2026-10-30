//! ローダーが使うWin32の呼び出しを1つのトレイトにまとめたもの。
//! 本物のプロセスを作らずに後始末の各地点を`cargo test`で検査するため、呼び出しを差し替えられるようにしてある。
//! 検査では好きな地点で失敗させる偽物を差し込む。本番（`real::RealWin32`）と自己検査では本物を差し込む。
//! ハンドルと相手のアドレスは生のポインタでなく数値のラッパーで渡す。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(pub usize);

/// 相手のプロセスの中のアドレス。こちらでは読めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Remote(pub usize);

pub type ErrorCode = u32;

/// `process`と`thread`は呼んだ側が閉じる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessInfo {
    pub process: Handle,
    pub thread: Handle,
    pub pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    Signaled,
    TimedOut,
    Failed(ErrorCode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub exe: PathBuf,
    /// 花映塚には何も渡さない。自己検査の的にだけ渡す。
    pub args: Vec<OsString>,
    /// 作業フォルダ。花映塚はファイルを作業フォルダ基準で読むので、ゲームのフォルダにする。
    pub cwd: PathBuf,
    /// 足す・置き換える環境変数。空なら親の環境をそのまま継がせる（lpEnvironment = NULL）。
    pub env: Vec<(OsString, OsString)>,
    /// 最初のウィンドウを前面に出さずに起動する（`real::startup_info`）。
    pub no_activate: bool,
}

/// 起動の途中で画面とログに知らせたいイベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchNote {
    /// DLLの読み込みが最初の待ちで終わらず、ゲームのプロセスは生きているので待ちを延ばす。
    SlowLoad,
}

/// どれも呼んだら何が起きたかを返すだけ。後始末は呼んだ側（`launch.rs`）が決める。
pub trait Win32 {
    fn read_file(&mut self, path: &Path) -> Result<Vec<u8>, String>;

    /// kernel32.dllは同じ起動の間どのプロセスでも同じアドレスなのでリモートスレッドの入口にそのまま使える。
    fn load_library_w_address(&mut self) -> Result<usize, ErrorCode>;

    fn create_process_suspended(&mut self, spawn: &Spawn) -> Result<ProcessInfo, ErrorCode>;

    fn virtual_alloc(&mut self, process: Handle, size: usize) -> Result<Remote, ErrorCode>;

    /// 全部書き込めなかったら失敗。
    fn write_memory(&mut self, process: Handle, at: Remote, data: &[u8]) -> Result<(), ErrorCode>;

    /// 全部読み取れなかったら失敗。
    fn read_memory(&mut self, process: Handle, at: usize, len: usize)
    -> Result<Vec<u8>, ErrorCode>;

    /// 保護を書き込み可能にして書き込み、元に戻して`FlushInstructionCache`を呼ぶ。照合と読み直しは呼んだ側が行う。
    fn patch_code(&mut self, process: Handle, at: usize, data: &[u8]) -> Result<(), ErrorCode>;

    /// 返したハンドルは呼んだ側が閉じる。
    fn create_remote_thread(
        &mut self,
        process: Handle,
        start: usize,
        param: Remote,
    ) -> Result<Handle, ErrorCode>;

    fn wait(&mut self, handle: Handle, timeout_ms: u32) -> WaitResult;

    /// `LoadLibraryW`の戻り値（読み込んだDLLのアドレス。0なら失敗）がここに来る。
    fn thread_exit_code(&mut self, thread: Handle) -> Result<u32, ErrorCode>;

    fn virtual_free(&mut self, process: Handle, at: Remote) -> Result<(), ErrorCode>;

    /// 戻り値は呼ぶ前の停止の回数。
    fn resume_thread(&mut self, thread: Handle) -> Result<u32, ErrorCode>;

    fn terminate_process(&mut self, process: Handle, exit_code: u32) -> Result<(), ErrorCode>;

    /// 失敗しても呼んだ側にできることは無い。
    fn close_handle(&mut self, handle: Handle) -> Result<(), ErrorCode>;

    /// 知らせを受け取る側（画面とログに出す）。受けない実装は何もしない。
    fn note(&mut self, _note: LaunchNote) {}
}
