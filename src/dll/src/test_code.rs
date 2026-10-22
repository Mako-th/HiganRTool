//! 検査用: 実行できるメモリに書いた偽のコードへ、`hook::HookEnv` を通して直に読み書きする環境（止めるスレッドは無い）。
//! 呼び出し元の機械語を書き込み、本物の `pad_extra::install_as` の書き方で rel32 を入口へ向ける検査が共有する。

use crate::hook::{HookEnv, ThreadRef, WriteDone};
use crate::ticks::CopyRow;

/// `base` から `span` バイトが検査の確保した読み書き実行できる領域。範囲の外は検査の誤りとして落とす。
pub struct RawCode {
    pub base: u32,
    pub span: u32,
}

impl HookEnv for RawCode {
    fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
        Ok(Vec::new())
    }
    fn suspend(&mut self, _t: ThreadRef) -> bool {
        true
    }
    fn resume(&mut self, _t: ThreadRef) -> bool {
        true
    }
    fn close(&mut self, _t: ThreadRef) {}
    fn eip(&mut self, _t: ThreadRef) -> Option<u32> {
        None
    }
    fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        assert!(addr >= self.base && addr + buf.len() as u32 <= self.base + self.span);
        // SAFETY: 範囲は検査が確保した領域の中（上の assert）。
        unsafe {
            core::ptr::copy_nonoverlapping(addr as usize as *const u8, buf.as_mut_ptr(), buf.len())
        };
        true
    }
    fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
        assert!(addr >= self.base && addr + bytes.len() as u32 <= self.base + self.span);
        // SAFETY: 範囲は検査が確保した読み書き実行できる領域の中（上の assert）。
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as usize as *mut u8, bytes.len())
        };
        Ok(WriteDone {
            protect_restored: true,
            flushed: true,
        })
    }
    fn set_original_target(&mut self, _target: u32) {}
    fn set_window(&mut self, _addr: u32) {}
    fn set_copy_table(&mut self, _rows: &[CopyRow]) {}
    fn set_render_only_exit(&mut self, _addr: u32) {}
    fn entry_address(&self) -> u32 {
        0
    }
    fn match_entry_address(&self) -> u32 {
        0
    }
    fn set_match_original(&mut self, _target: u32) {}
    fn sleep_ms(&mut self, _ms: u32) {}
    fn now_ms(&self) -> u64 {
        0
    }
    fn counter(&self) -> u32 {
        0
    }
}
