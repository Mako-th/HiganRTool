//! tick ごとの記録を読む Win32 の層（unsafe はここにもある。1 つずつ `// SAFETY:`）。
//!
//! `observe::RecordEnv` の本物: リングバッファの読み取り、TSC（`rdtsc`）と QueryPerformanceCounter の対、QueryPerformanceFrequency、CPUID の
//! invariant TSC、ゲームの時計の分母 `[0x4B30C4]` の読み取り（`win::read_own_memory`。読むだけ）、入口のコピーの表の読み返し、
//! 本ログの隣のファイル（`_ticks.csv` / `_changes.csv` / `_snap.bin` / `_menu.csv`）、メニューとライフのスレッド（`menu_win`）。
//! 読む順・判定・ログの文は `observe.rs` などにだけ書く。このコードは読み取りのスレッドで走る（ゲームのスレッドではない）。

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "record_win.rs は 32 ビット（i686）専用（入口とリングバッファが 32 ビットのアドレスを前提にしている）"
);

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::PathBuf;

use higanr_core::gzlog::{self, GzLogWriter};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::draw::{self, DrawRead};
use crate::feeds::{FEED_RING, FeedRead};
use crate::menu::TickStats;
use crate::menu_win::MenuThread;
use crate::names::{is_same_or_inside, side_file_name};
use crate::net_watch::{NET_FRAMES, NetFrameRing, NetSnapshot};
use crate::observe::{D3d8Probe, MENU_SUFFIX, RecordEnv, WatchLog};
use crate::scripted::Progress;
use crate::shots::{ShotCounters, ShotHead};
use crate::stub;
use crate::th09_addr::{
    D3D8_DEVICE_PTR, D3D8_ENDSCENE_SLOT, D3D8_PRESENT_SLOT, WINDOW_ADDR, WINDOW_CLOCK_DIVISOR,
};
use crate::ticks::{CopyRow, SlotRead};
use crate::trial_summary::HostFacts;
use crate::win::{module_of_addr, read_own_memory};
use crate::{script_win, shot_win};

/// CPUID の拡張の葉の最大を返す葉と、invariant TSC の葉（`0x80000007` の EDX bit 8 が「TSC が電力の状態によらず一定の速さで
/// 進む」）。
const CPUID_EXT_MAX: u32 = 0x8000_0000;
const CPUID_POWER_MGMT: u32 = 0x8000_0007;
const INVARIANT_TSC_BIT: u32 = 1 << 8;

pub struct RealRecord {
    /// 回ごとの名前のパス（隣のファイルの名前の元。日ごとの本ログでは実在しない。`None` なら隣のファイルは開かない）。
    run_path: Option<PathBuf>,
    /// ゲームのフォルダ（exe のフォルダ）。ここと同じかその下には書かない（本ログと同じ守り）。
    exe_dir: Option<PathBuf>,
    menu: Option<MenuThread>,
}

impl RealRecord {
    pub fn new(run_path: Option<PathBuf>, exe_dir: Option<PathBuf>) -> Self {
        RealRecord {
            run_path,
            exe_dir,
            menu: None,
        }
    }

    /// 本ログの隣に、本ログの名前の先頭 ＋ `suffix` のファイルを `create_new` で開く（既存を上書きしない）。
    /// 置き場が exe のフォルダと同じかその下なら断る。
    fn open_raw(&self, suffix: &str) -> Result<(String, File), String> {
        let log = self
            .run_path
            .as_ref()
            .ok_or("本ログの置き場が決まっていない")?;
        let dir = log.parent().ok_or("本ログのフォルダが分からない")?;
        let log_name = log
            .file_name()
            .ok_or("本ログの名前が分からない")?
            .to_string_lossy();
        let name = side_file_name(&log_name, suffix);
        let path = dir.join(&name);
        let exe_dir = self
            .exe_dir
            .as_ref()
            .ok_or("exe のフォルダが分からないので、ゲームのフォルダの外か確かめられない")?;
        if is_same_or_inside(&dir.to_string_lossy(), &exe_dir.to_string_lossy()) {
            return Err(format!(
                "置き場 {} が exe のフォルダの中なので書かない",
                dir.display()
            ));
        }
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => Ok((name, f)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                Err(format!("{name} が既にある（上書きしない）"))
            }
            Err(e) => Err(format!("{} を開けない: {e}", path.display())),
        }
    }

    fn open_file(&self, suffix: &str) -> Result<(String, BufWriter<File>), String> {
        let (name, f) = self.open_raw(suffix)?;
        Ok((name, BufWriter::new(f)))
    }
}

fn rdtsc() -> u64 {
    // SAFETY: `rdtsc` は読むだけの命令で、ユーザーモードで許されている。引数も副作用も無い。
    unsafe { core::arch::x86::_rdtsc() }
}

impl RecordEnv for RealRecord {
    fn next_seq(&self) -> u32 {
        stub::next_seq()
    }

    fn read_slot(&self, seq: u32) -> SlotRead {
        stub::read_slot(seq)
    }

    fn tsc_qpc(&mut self) -> (u64, i64) {
        let a = rdtsc();
        let mut q: i64 = 0;
        // SAFETY: `q` は書ける i64（LARGE_INTEGER と同じ大きさ）。XP 以降は失敗しない。
        unsafe { QueryPerformanceCounter(&mut q) };
        let b = rdtsc();
        (a.wrapping_add(b.wrapping_sub(a) / 2), q)
    }

    fn qpc_frequency(&self) -> i64 {
        let mut f: i64 = 0;
        // SAFETY: 同上。
        unsafe { QueryPerformanceFrequency(&mut f) };
        f
    }

    fn invariant_tsc(&self) -> Option<bool> {
        // 葉 0x80000007 は、拡張の葉の最大がそこまであるときだけ問う（無い葉を問うと別の葉の値が返る CPU がある）。
        let max = core::arch::x86::__cpuid(CPUID_EXT_MAX).eax;
        if max < CPUID_POWER_MGMT {
            return None;
        }
        let edx = core::arch::x86::__cpuid(CPUID_POWER_MGMT).edx;
        Some(edx & INVARIANT_TSC_BIT != 0)
    }

    fn game_clock_divisor(&mut self) -> Option<u32> {
        let b = read_own_memory(WINDOW_ADDR + WINDOW_CLOCK_DIVISOR, 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn copy_table(&self) -> Vec<CopyRow> {
        stub::copy_table()
    }

    fn open_output(&mut self, suffix: &str) -> Result<(String, Box<dyn Write>), String> {
        // `.gz` で終わる名前（大きい CSV）は gzip で書く。圧縮は書き込み先を持つスレッドで行い、ゲームのスレッドには乗らない。
        // `BufWriter` は挟まない（圧縮器が自分で塊にして書く）。ゲームを閉じるとこのスレッドごと消えて `Drop` が走らないので、
        // ファイルは sync flush のたびに終わりの印の仮を書く形（`new_seekable`）で開く。
        if gzlog::is_gz_name(suffix) {
            let (name, f) = self.open_raw(suffix)?;
            return Ok((name, Box::new(GzLogWriter::new_seekable(f))));
        }
        let (name, w) = self.open_file(suffix)?;
        Ok((name, Box::new(w)))
    }

    fn start_menu(&mut self) -> Result<String, String> {
        if self.menu.is_some() {
            return Err("もう起こしてある".to_string());
        }
        let (name, w) = self.open_file(MENU_SUFFIX)?;
        self.menu = Some(MenuThread::start(Box::new(w))?);
        Ok(name)
    }

    fn menu_stats(&mut self) -> Option<TickStats> {
        self.menu.as_ref().map(MenuThread::stats)
    }

    fn stop_menu(&mut self) -> Option<TickStats> {
        self.menu.take().map(MenuThread::stop)
    }

    fn draw_next_seq(&self) -> u32 {
        draw::draw_next_seq()
    }

    fn read_draw_slot(&self, seq: u32) -> DrawRead {
        draw::read_draw_slot(seq)
    }

    fn draw_calls(&self) -> u32 {
        draw::draw_calls()
    }

    fn start_render_only(&mut self, n: u32) -> bool {
        stub::try_start_render_only(n)
    }

    fn render_only_left(&self) -> u32 {
        stub::render_only_left()
    }

    fn feed_next_seq(&self) -> u32 {
        FEED_RING.next_seq()
    }

    fn read_feed(&self, seq: u32) -> FeedRead {
        FEED_RING.read(seq)
    }

    fn script_progress(&self) -> Progress {
        script_win::progress()
    }

    fn take_shot(&mut self) -> Option<(usize, ShotHead)> {
        shot_win::take_done()
    }

    fn copy_shot(&self, slot: usize, len: usize) -> Vec<u8> {
        shot_win::copy_pixels(slot, len)
    }

    fn release_shot(&mut self, slot: usize) {
        shot_win::release(slot);
    }

    fn shot_counters(&self) -> ShotCounters {
        shot_win::counters()
    }

    fn net_frames(&self) -> Option<&NetFrameRing> {
        script_win::net_armed().then_some(&NET_FRAMES)
    }

    fn net_status(&self) -> Option<(NetSnapshot, String)> {
        // ゲーム内のメニューの実行は、ネットワークを開く前でも本ログの「ネットワークの外」の行を出す。
        (script_win::net_armed() || script_win::net_pending()).then(script_win::net_status)
    }

    fn watch_log(&mut self) -> WatchLog {
        WatchLog {
            ended: script_win::take_watch_ends(),
            live: script_win::watch_live_text(),
        }
    }

    fn host_facts(&mut self) -> HostFacts {
        script_win::trial_host()
    }

    fn d3d8_probe(&mut self) -> D3d8Probe {
        // どれも読むだけ（`read_own_memory` はコミット済みで読める頁でなければ `None`）。デバイスの関数は呼ばない。ポインタを
        // たどる順: [0x4B3108] = device → [device] = vtable → [vtable+スロット]。
        let read_u32 = |addr: u32| -> Option<u32> {
            let b = read_own_memory(addr, 4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let device = read_u32(D3D8_DEVICE_PTR);
        let vtable = device.and_then(read_u32);
        let endscene = vtable.and_then(|v| read_u32(v.wrapping_add(D3D8_ENDSCENE_SLOT)));
        let present = vtable.and_then(|v| read_u32(v.wrapping_add(D3D8_PRESENT_SLOT)));
        let module = |a: Option<u32>| a.and_then(module_of_addr).map(|p| p.display().to_string());
        D3d8Probe {
            device,
            vtable,
            endscene,
            present,
            endscene_module: module(endscene),
            present_module: module(present),
        }
    }

    fn overlay_stop_line(&mut self) -> Option<String> {
        let (fail, hr, detail) = crate::overlay_win::take_stop_report()?;
        Some(crate::overlay::stop_line(fail, hr, detail))
    }
}

#[cfg(test)]
mod tests {
    //! 本物の Win32 を検査のプロセス自身に当てる（花映塚は使わない）。ファイルは一時ディレクトリだけ。
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "higanrtool_record_win_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tsc_and_qpc_move_forward() {
        let mut r = RealRecord::new(None, None);
        let a = r.tsc_qpc();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = r.tsc_qpc();
        assert!(b.0 > a.0, "TSC: {a:?} → {b:?}");
        assert!(b.1 > a.1, "QPC: {a:?} → {b:?}");
        assert!(r.qpc_frequency() > 0);
        let _ = r.invariant_tsc();
    }

    #[test]
    fn open_output_is_create_new_and_guarded() {
        let d = temp_dir("open");
        let mut r = RealRecord::new(Some(d.join("dll_x.log")), Some(d.join("elsewhere")));
        for suffix in ["_ticks.csv", "_changes.csv", "_snap.bin"] {
            let (name, mut w) = r.open_output(suffix).expect("1 回目は開ける");
            assert_eq!(name, format!("dll_x{suffix}"));
            w.write_all(b"a\n").unwrap();
            w.flush().unwrap();
            drop(w);
            let path = d.join(&name);
            assert_eq!(std::fs::read(&path).unwrap(), b"a\n");
            let e = r
                .open_output(suffix)
                .err()
                .expect("2 回目は既にあるので断る");
            assert!(e.contains("既にある"), "{e}");
            assert_eq!(std::fs::read(&path).unwrap(), b"a\n", "上書きした");
        }

        let mut inside = RealRecord::new(Some(d.join("y.log")), Some(d.clone()));
        assert!(inside.open_output("_ticks.csv").is_err());
        assert!(inside.start_menu().is_err());
        assert!(!d.join("y_ticks.csv").exists());
        assert!(!d.join("y_menu.csv").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_output_gz_name_writes_gzip_and_others_stay_plain() {
        let d = temp_dir("gz");
        let mut r = RealRecord::new(Some(d.join("dll_g.log")), Some(d.join("elsewhere")));
        let (name, mut w) = r.open_output("_net_frames.csv.gz").unwrap();
        assert_eq!(name, "dll_g_net_frames.csv.gz");
        let rows = "seg,frame\n0,0\n0,1\n";
        w.write_all(rows.as_bytes()).unwrap();
        w.flush().unwrap();
        drop(w);
        let path = d.join(&name);
        let raw = std::fs::read(&path).unwrap();
        assert!(gzlog::is_gzip(&raw), "平文で書いた");
        let back = gzlog::read_log_text(&path).unwrap();
        assert_eq!((back.text.as_str(), back.cut), (rows, false));
        let (name2, mut w2) = r.open_output("_ticks.csv").unwrap();
        w2.write_all(rows.as_bytes()).unwrap();
        w2.flush().unwrap();
        drop(w2);
        assert_eq!(std::fs::read(d.join(name2)).unwrap(), rows.as_bytes());
        assert!(
            r.open_output("_net_frames.csv.gz")
                .err()
                .expect("2 回目は断る")
                .contains("既にある")
        );
        assert_eq!(std::fs::read(&path).unwrap(), raw, "上書きした");
        let mut inside = RealRecord::new(Some(d.join("y.log")), Some(d.clone()));
        assert!(inside.open_output("_pacing.csv.gz").is_err());
        assert!(!d.join("y_pacing.csv.gz").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_output_gz_is_a_complete_gzip_while_the_game_still_runs() {
        let d = temp_dir("gz_live");
        let mut r = RealRecord::new(Some(d.join("dll_l.log")), Some(d.join("elsewhere")));
        let (name, mut w) = r.open_output("_net_frames.csv.gz").unwrap();
        let rows = "seg,frame\n0,0\n0,1\n";
        w.write_all(rows.as_bytes()).unwrap();
        w.flush().unwrap();
        // 閉じずに（`Drop` を走らせずに）読む = ゲームを閉じた後のファイル。
        let live = gzlog::read_log_text(&d.join(&name)).unwrap();
        assert_eq!((live.text.as_str(), live.cut), (rows, false));
        drop(w);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn menu_thread_starts_once_and_stops() {
        let d = temp_dir("menu");
        let mut r = RealRecord::new(Some(d.join("dll_m.log")), Some(d.join("elsewhere")));
        assert!(r.menu_stats().is_none());
        assert_eq!(r.start_menu().as_deref(), Ok("dll_m_menu.csv"));
        assert!(r.start_menu().is_err(), "2 本目を起こした");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let s = r.stop_menu().expect("止めた後の集計");
        assert!(s.samples >= 1);
        assert!(r.stop_menu().is_none());
        let text = std::fs::read_to_string(d.join("dll_m_menu.csv")).unwrap();
        assert!(text.starts_with(crate::menu::MENU_CSV_HEADER));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn game_clock_divisor_does_not_crash_outside_the_game() {
        let mut r = RealRecord::new(None, None);
        let _ = r.game_clock_divisor();
    }
}
