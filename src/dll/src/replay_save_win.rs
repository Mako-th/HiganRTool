//! 自動保存の、ゲームのスレッドの層と Win32 の層（unsafe はここにもある。1 つずつ `// SAFETY:`）。判断は `replay_save.rs`（pure）。
//! 入れるのは `HiganRTool.ini` の `[replay]` に `enable = 1` があるときだけ（`arm`）。入れなければ `on_tick` は原子変数を 1 つ読んで戻る。
//! - 入口 `on_tick`（台本・ネットワークの分かれ道 `script_win::script_on_tick` の先頭から、ゲームのスレッドで毎 tick 呼ばれる）と、ゲームのスレッドの状態
//!   （`DRIVER`。触るのはゲームのスレッドだけなので `try_lock` は必ず取れる。待たない）
//! - ゲームのメモリの読み書きと保存関数の呼び出し（`RealSaveIo`）。保存関数は素のアセンブリの踏み台 `save_call` で呼ぶ: `call` の直前に、
//!   ゲームの見積もりのループ（`0x00420CA0`）が読む 10 語を読んで記録し、そこへヘッダの本当の印を書いてから（自分のスタックの esp より下だけ）、
//!   書いた後の 10 語で見積もりを計算して、要る上限より小さければ呼ばない
//! - 保管庫のスレッド（`arm` が起こす）と、その Win32 のインターフェース（`RealVaultOs`: ゲームの exe のフォルダを読む・CP932 のパスを `CreateFileA` で
//!   開いて押さえたファイルと同じかを見る）

#[cfg(not(target_arch = "x86"))]
compile_error!(
    "replay_save_win.rs は 32 ビット（i686）専用（保存関数の呼び出しと 32 ビットのアドレスを前提にしている）"
);

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use higanr_core::autosave::{self, ChargeSync, NAME_BUF};
use higanr_core::handshake::Name;
use higanr_core::log_scrub::{Scrub, ScrubWrite};
use higanr_core::replay::Timestamp;
use higanr_core::replay_playcheck::{HEADER_DATE_OFFSET, HEADER_FIELD_LEN, HEADER_NAME_OFFSET};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileA, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, GetFileInformationByHandle,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::menu::MemRead;
use crate::menu_win::OwnProcess;
use crate::names::{LocalTime, is_same_or_inside, side_file_name};
use crate::net_config::NetSettings;
use crate::netplay::NetPlay;
use crate::replay_save::{
    CHAIN_LIMIT, CallOut, Driver, EstimateReal, Handoff, LOG_SUFFIX, NetView, Outcome, RNG_BYTES,
    STAGES, SaveIo, VaultOs, WORKER_PERIOD_MS, Watching, Worker, read_sides,
};
use crate::replay_watch::HeaderIdent;
use crate::th09_addr::{
    COMPLETED_ROUNDS, EXE_DIR, EXE_DIR_LEN, GAME_MODE, LOADING, MATCH_END_MENU, OPT_CHARGE,
    OPTIONS_COPY_PTR, P1_PLAYER_PTR, P1_STATS_PTR, P1_WINS, P2_PLAYER_PTR, P2_STATS_PTR, P2_WINS,
    REPLAY_HEADER_CHARGE, REPLAY_HEADER_PTR, REPLAY_HEADER_STAGE_FLAGS, REPLAY_MANAGER_PTR,
    REPLAY_MODE, REPLAY_NODE_NEXT, REPLAY_SAVE_FN, REPLAY_SAVE_FRAME_SHIFT, REPLAY_SAVE_SIG,
    REPLAY_STAGE_NODES, REPLAY_STAGE_STRIDE, RNG_ADDR, ROUND_FRAMES, ROUNDS_REQUIRED,
    SAVE_EST_BASE_SITE, SAVE_EST_FLAGS_EBP, SAVE_EST_PER_CHUNK, SAVE_EST_PER_STAGE, SUP_SCENE_CUR,
    SUP_SCENE_REQ, SUPERVISOR,
};
use crate::trial_summary::ReplayFacts;
use crate::win::{self, read_own_into, read_own_memory};
use higanr_core::replay_rounds::Boundary;

/// 自動保存が入っているか。0 から 1 にするのは `arm`（アドレスを置き、保管庫のスレッドを起こした後）、1 から 0 にするのはパニックを受けたとき。
pub static ARMED: AtomicU32 = AtomicU32::new(0);
pub static PANICKED: AtomicU32 = AtomicU32::new(0);
pub static HANDOFF: Handoff = Handoff::new();
/// ゲームのスレッドの状態。触るのはゲームのスレッドの `on_tick` だけ（`try_lock` で、取れなければ何もしない）。
static DRIVER: Mutex<Driver> = Mutex::new(Driver::new());
/// QueryPerformanceFrequency（`arm` が置く。0 なら所要は 0 と書く）。
static QPC_FREQ: AtomicU64 = AtomicU64::new(0);
/// 今の seg（マッチ）で保存関数を呼んだか（`Driver::saved_this_seg` のコピー。`on_tick` が毎 tick 書く）。錠を取らずに読むためのコピー。
static SEG_SAVED: AtomicU32 = AtomicU32::new(0);

/// ゲームのスレッドが直に読むアドレス（`arm` が読めると確かめてから置く。検査では自分の静的変数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveAddrs {
    pub supervisor: u32,
    pub menu_flag: u32,
    pub loading: u32,
    pub game_mode: u32,
    pub manager_ptr: u32,
    /// 1P の勝ち数・2P の勝ち数・勝ちに要る数。
    pub wins: [u32; 3],
    pub rng: u32,
    pub save_fn: u32,
    pub est_base_site: u32,
    /// `completed_rounds`・`round_frames`（ラウンドごとの記録）。
    pub completed_rounds: u32,
    pub round_frames: u32,
    /// 1P・2P の Player へのポインタの欄、スコアを持つ塊へのポインタの欄（欄そのものは直に読み、先は落ちない読み方でたどる）。
    pub players: [u32; 2],
    pub stats: [u32; 2],
    /// ゲームの設定の写しへのポインタの欄（`[欄]+0xB4` / `+0xB5` が生きた試合の 1P / 2P のチャージ型）。欄そのものは `arm` が読めると
    /// 確かめ、先は落ちない読み方でたどる。
    pub options_copy: u32,
}

const ADDR_WORDS: usize = 18;
static ADDRS: [AtomicU32; ADDR_WORDS] = [const { AtomicU32::new(0) }; ADDR_WORDS];

impl SaveAddrs {
    pub fn th09() -> SaveAddrs {
        SaveAddrs {
            supervisor: SUPERVISOR,
            menu_flag: MATCH_END_MENU,
            loading: LOADING,
            game_mode: GAME_MODE,
            manager_ptr: REPLAY_MANAGER_PTR,
            wins: [P1_WINS, P2_WINS, ROUNDS_REQUIRED],
            rng: RNG_ADDR,
            save_fn: REPLAY_SAVE_FN,
            est_base_site: SAVE_EST_BASE_SITE,
            completed_rounds: COMPLETED_ROUNDS,
            round_frames: ROUND_FRAMES,
            players: [P1_PLAYER_PTR, P2_PLAYER_PTR],
            stats: [P1_STATS_PTR, P2_STATS_PTR],
            options_copy: OPTIONS_COPY_PTR,
        }
    }

    pub fn direct_ranges(&self) -> [(u32, u32); 18] {
        [
            (self.supervisor.wrapping_add(SUP_SCENE_CUR), 4),
            (self.supervisor.wrapping_add(SUP_SCENE_REQ), 4),
            (self.menu_flag, 1),
            (self.loading, 4),
            (self.game_mode, 4),
            (self.manager_ptr, 4),
            (self.wins[0], 4),
            (self.wins[1], 4),
            (self.wins[2], 4),
            (self.rng, RNG_BYTES as u32),
            (self.est_base_site, 5),
            (self.completed_rounds, 4),
            (self.round_frames, 4),
            (self.players[0], 4),
            (self.players[1], 4),
            (self.stats[0], 4),
            (self.stats[1], 4),
            (self.options_copy, 4),
        ]
    }

    fn words(&self) -> [u32; ADDR_WORDS] {
        [
            self.supervisor,
            self.menu_flag,
            self.loading,
            self.game_mode,
            self.manager_ptr,
            self.wins[0],
            self.wins[1],
            self.wins[2],
            self.rng,
            self.save_fn,
            self.est_base_site,
            self.completed_rounds,
            self.round_frames,
            self.players[0],
            self.players[1],
            self.stats[0],
            self.stats[1],
            self.options_copy,
        ]
    }

    fn store(&self) {
        for (a, v) in ADDRS.iter().zip(self.words()) {
            a.store(v, Ordering::SeqCst);
        }
    }

    fn load() -> SaveAddrs {
        Self::from_words(core::array::from_fn(|i| ADDRS[i].load(Ordering::Acquire)))
    }

    /// `words` の逆。
    fn from_words(w: [u32; ADDR_WORDS]) -> SaveAddrs {
        SaveAddrs {
            supervisor: w[0],
            menu_flag: w[1],
            loading: w[2],
            game_mode: w[3],
            manager_ptr: w[4],
            wins: [w[5], w[6], w[7]],
            rng: w[8],
            save_fn: w[9],
            est_base_site: w[10],
            completed_rounds: w[11],
            round_frames: w[12],
            players: [w[13], w[14]],
            stats: [w[15], w[16]],
            options_copy: w[17],
        }
    }
}

pub fn armed() -> bool {
    ARMED.load(Ordering::Acquire) != 0
}

/// 自動保存の数（試験のまとめの「自動保存」の行。入れていなければ全部 0）。
pub fn facts() -> ReplayFacts {
    let o = HANDOFF.outcome_counts();
    ReplayFacts {
        attempts: HANDOFF.attempts(),
        saved: o[Outcome::Saved as usize],
        not_called: o[Outcome::NotCalled as usize],
        empty_file: o[Outcome::EmptyFile as usize],
        mark_failed: o[Outcome::MarkFailed as usize],
        prep_failed: o[Outcome::PrepFailed as usize],
        panicked: PANICKED.load(Ordering::Relaxed) != 0,
    }
}

/// 入力の地点（ゲームのスレッド）から毎 tick。`np` はネットワークが動いていればネットワークのセッション。`watching` は観戦者の実行で見えるもの。
/// パニックは受けて外へ出さず、自動保存を止める（台本とネットワークはそのまま）。
pub fn on_tick(np: Option<&NetPlay>, watching: Watching) {
    if ARMED.load(Ordering::Acquire) == 0 {
        return;
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut d) = DRIVER.try_lock() else {
            return;
        };
        let mut io = RealSaveIo {
            a: SaveAddrs::load(),
        };
        d.tick_watching(&HANDOFF, &mut io, np.and_then(NetView::of), watching);
        SEG_SAVED.store(u32::from(d.saved_this_seg()), Ordering::Relaxed);
    }));
    if r.is_err() {
        SEG_SAVED.store(0, Ordering::Relaxed);
        ARMED.store(0, Ordering::SeqCst);
        PANICKED.store(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
thread_local! {
    /// 検査の入口: このスレッドで `on_confirmed_tick` が呼ばれた回数（`GameWindow::tick` の配線を見る。検査はスレッドごとに走る）。
    static CONFIRMED_HOOK_CALLS: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn confirmed_hook_calls() -> u32 {
    CONFIRMED_HOOK_CALLS.with(core::cell::Cell::get)
}

/// rollback の確定した tick の直後（`GameWindow::tick` から、ゲームのスレッドで）。ラウンドの境目の値をその tick のまま読む。入れていない実行は
/// 原子変数を 1 つ読んで戻る。`DRIVER` は `on_tick` が終わった後（フレームの始まりの後）にしか来ないので `try_lock` は取れる。パニックは受けて
/// 外へ出さず、自動保存を止める。
pub fn on_confirmed_tick() {
    #[cfg(test)]
    CONFIRMED_HOOK_CALLS.with(|c| c.set(c.get() + 1));
    if ARMED.load(Ordering::Acquire) == 0 {
        return;
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut d) = DRIVER.try_lock() else {
            return;
        };
        let mut io = RealSaveIo {
            a: SaveAddrs::load(),
        };
        d.note_confirmed_tick(&mut io);
    }));
    if r.is_err() {
        SEG_SAVED.store(0, Ordering::Relaxed);
        ARMED.store(0, Ordering::SeqCst);
        PANICKED.store(1, Ordering::SeqCst);
    }
}

/// この道具の自動保存が、今のマッチ（seg）で保存関数を呼んだか。入れていない実行・パニックで止めた後・この seg で呼んでいない
/// （見積もりで呼ばなかった・準備が間に合わなかった・試みていない）は false。同じ tick の `on_tick` の後に読むこと。
pub fn saved_this_seg() -> bool {
    ARMED.load(Ordering::Acquire) != 0 && SEG_SAVED.load(Ordering::Relaxed) != 0
}

/// 検査の入口: `ARMED` と `SEG_SAVED` を直に置く。`REAL_HOOK_TESTS` の錠を持つ間だけ呼び、戻すときは両方 false。
#[cfg(test)]
pub(crate) fn set_seg_saved_for_test(armed: bool, saved: bool) {
    SEG_SAVED.store(u32::from(saved), Ordering::SeqCst);
    ARMED.store(u32::from(armed), Ordering::SeqCst);
}

/// `[addr, addr + N)` を直に読む（1 バイトずつ）。呼んでよいのは `SaveAddrs::direct_ranges` の範囲の中（`arm` が読めると
/// 確かめた th09.exe の .data と .text）と、保存関数の先頭の署名の 12 バイトだけ。
fn rd<const N: usize>(addr: u32) -> [u8; N] {
    core::array::from_fn(|i| {
        // SAFETY: 呼び出し元が `arm` の確かめた範囲のアドレスだけを渡す。volatile はゲームのスレッドが書き換える欄の読みを畳ませないため。
        unsafe { core::ptr::read_volatile((addr as usize + i) as *const u8) }
    })
}

fn rd_u32(addr: u32) -> u32 {
    u32::from_le_bytes(rd::<4>(addr))
}

fn to_timestamp(t: LocalTime) -> Timestamp {
    let b = |v: u16| u8::try_from(v).unwrap_or(u8::MAX);
    Timestamp {
        year: t.year,
        month: b(t.month),
        day: b(t.day),
        hour: b(t.hour),
        minute: b(t.minute),
        second: b(t.second),
    }
}

fn qpc_now_us() -> u64 {
    let f = QPC_FREQ.load(Ordering::Relaxed);
    if f == 0 {
        return 0;
    }
    let mut q: i64 = 0;
    // SAFETY: `q` は書ける i64（LARGE_INTEGER と同じ大きさ）。XP 以降は失敗しない。
    unsafe { QueryPerformanceCounter(&mut q) };
    (u128::from(q.max(0) as u64) * 1_000_000 / u128::from(f)) as u64
}

pub struct RealSaveIo {
    pub a: SaveAddrs,
}

impl SaveIo for RealSaveIo {
    fn scenes(&mut self) -> (u32, u32) {
        (
            rd_u32(self.a.supervisor.wrapping_add(SUP_SCENE_CUR)),
            rd_u32(self.a.supervisor.wrapping_add(SUP_SCENE_REQ)),
        )
    }
    fn menu_flag(&mut self) -> u8 {
        rd::<1>(self.a.menu_flag)[0]
    }
    fn loading(&mut self) -> u32 {
        rd_u32(self.a.loading)
    }
    fn game_mode(&mut self) -> u32 {
        rd_u32(self.a.game_mode)
    }
    fn replay_manager(&mut self) -> u32 {
        rd_u32(self.a.manager_ptr)
    }
    fn replay_mode(&mut self, rm: u32) -> Option<u32> {
        OwnProcess.u32_at(rm.checked_add(REPLAY_MODE)?)
    }
    fn replay_ident(&mut self, rm: u32) -> Option<HeaderIdent> {
        let hdr = OwnProcess.u32_at(rm.checked_add(REPLAY_HEADER_PTR)?)?;
        if hdr == 0 {
            return None;
        }
        // 確保しない・落ちない読み方だけを使う（ゲームのスレッド）。
        let mut id = HeaderIdent {
            date: [0; HEADER_FIELD_LEN],
            name: [0; HEADER_FIELD_LEN],
        };
        let date_at = hdr.checked_add(HEADER_DATE_OFFSET as u32)?;
        let name_at = hdr.checked_add(HEADER_NAME_OFFSET as u32)?;
        (read_own_into(date_at, &mut id.date) && read_own_into(name_at, &mut id.name)).then_some(id)
    }
    fn signature_ok(&mut self) -> bool {
        rd::<12>(self.a.save_fn) == REPLAY_SAVE_SIG
    }
    fn estimate_real(&mut self, rm: u32) -> Option<EstimateReal> {
        let site = rd::<5>(self.a.est_base_site);
        if site[0] != 0xB9 {
            return None;
        }
        let base = u32::from_le_bytes([site[1], site[2], site[3], site[4]]);
        let mut m = OwnProcess;
        let hdr = m.u32_at(rm.checked_add(REPLAY_HEADER_PTR)?)?;
        if hdr == 0 {
            return None;
        }
        let mut flags = [0u32; STAGES];
        let mut chunks = [0u32; STAGES];
        for i in 0..STAGES as u32 {
            let at = hdr.checked_add(REPLAY_HEADER_STAGE_FLAGS + 4 * i)?;
            flags[i as usize] = m.u32_at(at)?;
            // 先頭の塊は ReplayManager の中に埋め込まれていて、そこから次をたどる（0x00420CB0〜0x00420CBF）。
            let head = rm.checked_add(REPLAY_STAGE_NODES + REPLAY_STAGE_STRIDE * i)?;
            let mut n = 1u32;
            let mut p = m.u32_at(head.checked_add(REPLAY_NODE_NEXT)?)?;
            while p != 0 {
                n += 1;
                if n > CHAIN_LIMIT {
                    return None;
                }
                p = m.u32_at(p.checked_add(REPLAY_NODE_NEXT)?)?;
            }
            chunks[i as usize] = n;
        }
        Some(EstimateReal {
            base,
            flags,
            chunks,
        })
    }
    fn wins(&mut self) -> [u32; 3] {
        self.a.wins.map(rd_u32)
    }
    fn read_rng(&mut self) -> [u8; RNG_BYTES] {
        rd::<RNG_BYTES>(self.a.rng)
    }
    fn write_rng(&mut self, v: &[u8; RNG_BYTES]) {
        for (i, b) in v.iter().enumerate() {
            // SAFETY: `[rng, rng + 12)` は `arm` が読めると確かめた th09.exe の .data（乱数の状態と抽選回数 —— `th09_addr::RNG_ADDR`）。
            // 書くのは同じスレッドが直前に読んだ値だけ（保存が引いた 3 回を無かったことにする）。
            unsafe { core::ptr::write_volatile((self.a.rng as usize + i) as *mut u8, *b) };
        }
    }
    fn local_time(&mut self) -> Timestamp {
        to_timestamp(win::local_time())
    }
    fn now_us(&mut self) -> u64 {
        qpc_now_us()
    }
    fn unix_secs(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
    fn completed_rounds(&mut self) -> u32 {
        rd_u32(self.a.completed_rounds)
    }
    fn read_boundary(&mut self) -> Boundary {
        Boundary {
            frames: rd_u32(self.a.round_frames),
            wins: [rd_u32(self.a.wins[0]), rd_u32(self.a.wins[1])],
            side: read_sides(&mut OwnProcess, self.a.players, self.a.stats),
        }
    }
    fn sync_header_charge(&mut self, rm: u32) -> ChargeSync {
        let mut m = OwnProcess;
        let copy = m.u32_at(self.a.options_copy);
        let header = rm
            .checked_add(REPLAY_HEADER_PTR)
            .and_then(|at| m.u32_at(at));
        // 確保しない読み書きだけを使う（ゲームのスレッド）。
        autosave::sync_header_charge(
            copy,
            header,
            OPT_CHARGE,
            REPLAY_HEADER_CHARGE,
            |at| {
                let mut b = [0u8; 1];
                read_own_into(at, &mut b).then_some(b[0])
            },
            write_own_u8,
        )
    }
    fn call_save(
        &mut self,
        rm: u32,
        path_addr: u32,
        name: &[u8; NAME_BUF],
        est: &EstimateReal,
        need: u32,
    ) -> CallOut {
        let mut seen = [0u32; STAGES];
        let mut written = [0u32; STAGES];
        let mut game_est = 0u32;
        // SAFETY（`save_call` の約束）: `save_fn` は `arm` の照合とこの tick の `signature_ok` で、先頭の 12 バイトが花映塚の保存関数と
        // 同じと確かめたアドレス。`rm` は `[0x4A7E64]` の 0 でない値で `+0x114` が 0（記録の側）の ReplayManager（場面 2 の読み込み中でない tick。読み込みのスレッドは触っていない）。`path_addr` は
        // `Handoff` のパスのバッファ（NUL で終わる CP932。この呼び出しの間、持ち主はゲームのスレッド）、`name` は NUL で終わる 8 文字。`seen`・`written`・`game_est` はこのスタックの
        // 書けるバッファ、`est.flags` と `est.chunks` は読める 10 語。踏み台が書くのは自分のスタックの esp より下の 40 バイトだけ。
        // ゲームのスレッドから呼ぶ（ゲーム自身の保存も同じスレッド）。
        let called = unsafe {
            save_call(
                self.a.save_fn,
                rm,
                path_addr,
                name.as_ptr() as usize as u32,
                seen.as_mut_ptr() as usize as u32,
                est.flags.as_ptr() as usize as u32,
                written.as_mut_ptr() as usize as u32,
                est.chunks.as_ptr() as usize as u32,
                est.base,
                need,
                &mut game_est as *mut u32 as usize as u32,
            )
        };
        CallOut {
            called: called != 0,
            seen,
            written,
            game_est,
        }
    }
}

/// 自分のプロセスへ 1 バイトを落ちない書き方で書く（アドレスが確保されていない・書けないなら false。例外は出ない）。
fn write_own_u8(addr: u32, v: u8) -> bool {
    let mut n: usize = 0;
    // SAFETY: 自分のプロセスの擬似ハンドルと、書くアドレス・書く元（スタックの `v`）・長さ 1・書けたバイト数の書く先を渡すだけ。
    // アドレスが確保されていない・書けないなら、OS が失敗を返す。
    let ok = unsafe {
        WriteProcessMemory(
            GetCurrentProcess(),
            addr as usize as *const core::ffi::c_void,
            &v as *const u8 as *const core::ffi::c_void,
            1,
            &mut n,
        )
    };
    ok != 0 && n == 1
}

/// `call` の時点の esp（E）から、ゲームの見積もりのループが読む 10 語の始まりまで（`E − 0x200`）。
const SNAP_BELOW: u32 = REPLAY_SAVE_FRAME_SHIFT + SAVE_EST_FLAGS_EBP;

// SAFETY（`#[unsafe(naked)]`）: cdecl の関数（引数 11 個 `[esp+4]`〜`[esp+44]`、戻り値 eax）。ebx・esi・edi・ebp は push して最後に pop で戻す。
// - 名前を積んだ直後の esp を E とする。`[E − 0x200 + 4i]`（i = 0〜9）は、保存関数の先頭の `push ebp; mov ebp,esp` の後の
//   `-0x1F8(%ebp,%esi,4)`（`0x00420CA0`）と同じアドレス（ebp = E − 8）。保存関数の先頭から `0x00420CA0` までにそこへ書く命令は無い。
// - 順に: ① その 10 語を `before` へコピーする。② 本当の印（ヘッダ `[this+8]` の `+0x20` の 10 語のコピー `flags`）をそこへ書く。③ 読み戻して
//   `after` へコピーする。④ `after` で見積もりを計算する。ゲームのループは印が 0 かどうかしか見ない（`0x00420CA7 test %eax,%eax`）ので、
//   本当の印を置けば、ゲームが確保の後 `0x00420CEB` でヘッダをコピーしてから使うのと同じ面を数える。コピーは確保の後に同じ値で上書きされるので、
//   書いた値がゲームのその後に残ることは無い。
// - 書くのはこのスレッドのスタックの、esp より 0x200〜0x1DC バイト下の 40 バイトだけ（ゲームのメモリにもコードにも書かない）。
//   esp より下は保存関数の枠になる所で呼び手の値は無く、1 頁（4 KiB）の内側なのでスタックの見張りの頁を越えない。割り込みはユーザーモードの
//   スタックを使わず、この間にシステムコールも無い。②から `call` までの間に push も呼び出しも挟まない（挟めば書いた 10 語が E からずれるか、
//   呼んだ関数の枠で壊れる）。見積もりは eax・ecx・edx・esi だけで計算し、`est_out` へ書き、`need` より小さければ
//   名前を積み下ろして 0 を返す（保存関数を呼ばない）。
// - 呼ぶときは ecx = this、edx = path、`[esp]` = 名前（fastcall。保存関数は `ret $0x4` で名前を片付ける）。保存関数は ebx・esi・edi・ebp を保つ
//   （`0x00420C69`・`0x00420C7F`〜`0x00420C80` の push と `0x0042112B`〜`0x00421132` の pop。早い出口 `0x0042112D` は ebx・esi に触らない）。
//   DF は呼ぶ側の Rust の決まりで 0。
/// 保存関数を呼ぶ踏み台（`RealSaveIo::call_save` と検査からだけ呼ぶ）。
#[allow(clippy::too_many_arguments)]
// 引数の並びは素のアセンブリの `[ebp + 8]`〜`[ebp + 48]` と 1 対 1（構造体にするとアドレスの計算が増える）
#[unsafe(naked)]
unsafe extern "C" fn save_call(
    _func: u32,
    _this: u32,
    _path: u32,
    _name: u32,
    _before: u32,
    _flags: u32,
    _after: u32,
    _chunks: u32,
    _base: u32,
    _need: u32,
    _est_out: u32,
) -> u32 {
    core::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "push dword ptr [ebp + 20]",
        // esp = E。① 書く前の 10 語をコピーする。
        "mov edi, dword ptr [ebp + 24]",
        "xor ecx, ecx",
        "6:",
        "mov ebx, dword ptr [esp + ecx*4 - {b}]",
        "mov dword ptr [edi + ecx*4], ebx",
        "inc ecx",
        "cmp ecx, {stages}",
        "jl 6b",
        // ② 本当の印を、ゲームのループが読む所へ書く。
        "mov esi, dword ptr [ebp + 28]",
        "xor ecx, ecx",
        "7:",
        "mov ebx, dword ptr [esi + ecx*4]",
        "mov dword ptr [esp + ecx*4 - {b}], ebx",
        "inc ecx",
        "cmp ecx, {stages}",
        "jl 7b",
        // ③ 書いた 10 語を読み戻す（ここから call まで push も呼び出しも無い）。
        "mov edi, dword ptr [ebp + 32]",
        "xor ecx, ecx",
        "8:",
        "mov ebx, dword ptr [esp + ecx*4 - {b}]",
        "mov dword ptr [edi + ecx*4], ebx",
        "inc ecx",
        "cmp ecx, {stages}",
        "jl 8b",
        // ④ ゲームの見積もり（eax）= 初めの値 ＋ 読み戻した印が 0 でない面の 0x60 ＋ 0x54D9 × 塊の数。
        "mov esi, dword ptr [ebp + 36]",
        "mov eax, dword ptr [ebp + 40]",
        "xor ecx, ecx",
        "2:",
        "cmp dword ptr [edi + ecx*4], 0",
        "je 3f",
        "add eax, {per_stage}",
        "mov edx, dword ptr [esi + ecx*4]",
        "imul edx, edx, {per_chunk}",
        "add eax, edx",
        "3:",
        "inc ecx",
        "cmp ecx, {stages}",
        "jl 2b",
        "mov edx, dword ptr [ebp + 48]",
        "mov dword ptr [edx], eax",
        "cmp eax, dword ptr [ebp + 44]",
        "jb 4f",
        "mov eax, dword ptr [ebp + 8]",
        "mov ecx, dword ptr [ebp + 12]",
        "mov edx, dword ptr [ebp + 16]",
        "call eax",
        "mov eax, 1",
        "jmp 5f",
        "4:",
        // 呼ばない: 積んだ名前を下ろす。
        "add esp, 4",
        "xor eax, eax",
        "5:",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        b = const SNAP_BELOW,
        per_stage = const SAVE_EST_PER_STAGE,
        per_chunk = const SAVE_EST_PER_CHUNK,
        stages = const STAGES,
    )
}

// ---- 保管庫のスレッドの Win32 のインターフェース ----

fn file_id(h: HANDLE) -> Result<(u32, u32, u32), String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `h` は呼び出しの間開いているファイルのハンドル、`info` は書ける構造体そのもの。
    let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
    if ok == 0 {
        // SAFETY: 引数の無い呼び出し。直前の呼び出しの結果を指す。
        return Err(format!("GetFileInformationByHandle が失敗（{}）", unsafe {
            GetLastError()
        }));
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// CP932 のパス `ansi`（NUL なし）を、保存関数と同じ `CreateFileA` の経路で開き、`file` と同じファイル（ボリュームの番号と
/// ファイルの番号が同じ）か。開くのは属性を読むためだけ。
pub fn same_file_ansi(ansi: &[u8], file: &File) -> Result<bool, String> {
    if ansi.contains(&0) {
        return Err("パスに NUL がある".to_string());
    }
    let mut z = ansi.to_vec();
    z.push(0);
    // SAFETY: `z` は NUL で終わるバイト列で呼び出しの間生きている。属性を読む権利だけ、共有は全部許す、既にあるものだけを開く。
    // 返したハンドルは下で閉じる。
    let h = unsafe {
        CreateFileA(
            z.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: 引数の無い呼び出し。直前の CreateFileA の結果を指す。
        return Err(format!(
            "CreateFileA が開けない（GetLastError = {}）",
            unsafe { GetLastError() }
        ));
    }
    let a = file_id(h);
    // SAFETY: 上で開いた、まだ閉じていないハンドル。
    unsafe { CloseHandle(h) };
    let b = file_id(file.as_raw_handle() as HANDLE)?;
    Ok(a? == b)
}

/// 押さえたファイルを、更新時刻を書くためだけに開く（属性の書き込みの権利 `FILE_WRITE_ATTRIBUTES` だけ、共有は全部許す、
/// 既にあるものだけ）。データの読み書きの権利を持たないハンドルは共有の判定に数えられないので、ゲームの保存が
/// `CreateFileA(GENERIC_WRITE, 共有 0, CREATE_ALWAYS)` で開いても断られない（検査 `touch_handle_does_not_block_the_games_create_always`）。
fn open_attributes_only(file: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(file)
}

pub struct RealVaultOs;

impl VaultOs for RealVaultOs {
    fn exe_dir_box(&mut self) -> Option<Vec<u8>> {
        read_own_memory(EXE_DIR, EXE_DIR_LEN as usize)
    }
    fn same_file_ansi(&mut self, ansi: &[u8], file: &File) -> Result<bool, String> {
        same_file_ansi(ansi, file)
    }
    fn now(&mut self) -> Timestamp {
        to_timestamp(win::local_time())
    }
    fn touch(&mut self, file: &Path) -> Result<(), String> {
        let f = open_attributes_only(file).map_err(|e| format!("開けない: {e}"))?;
        f.set_modified(std::time::SystemTime::now())
            .map_err(|e| format!("更新時刻を書けない: {e}"))
    }
}

// ---- 入れる（読み取りのスレッド） ----

/// 保管庫のスレッドのログの書き込み関数。簡潔なときはファイルを作らず捨てる。詳細のときも、ユーザー名とゲームのフォルダ・IP のアドレスは伏せて書く。
fn open_worker_log(
    detail: bool,
    dir: &Path,
    name: &str,
    exe: &Path,
) -> Result<Box<dyn Write + Send>, String> {
    if !detail {
        return Ok(Box::new(std::io::sink()));
    }
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(name))
        .map_err(|e| format!("{name} を開けない: {e}"))?;
    Ok(Box::new(ScrubWrite::new(
        f,
        Scrub::new().dir(&exe.to_string_lossy(), "<ゲーム>"),
    )))
}

/// `HiganRTool.ini` の本文から `[replay]` を読み、`enable = 1` なら自動保存を入れる（保存関数の署名とアドレスを確かめ、保管庫のスレッドのログを
/// 開き、保管庫のスレッドを起こし、`ARMED` = 1）。入れたらその文、設定に無ければ `Ok(None)`。台本の分かれ道を開く前に呼ぶ。
pub fn arm(
    ini_text: Option<&str>,
    net: Option<&NetSettings>,
    run_path: Option<&Path>,
    exe_dir: Option<&Path>,
) -> Result<Option<String>, String> {
    let Some(text) = ini_text else {
        return Ok(None);
    };
    let ini = higanr_core::ini::parse(text).map_err(|e| e.to_string())?;
    let Some(settings) = autosave::settings_from_ini(&ini)? else {
        return Ok(None);
    };
    if ARMED.load(Ordering::SeqCst) != 0 {
        return Err("もう入っている".to_string());
    }
    let sig = read_own_memory(REPLAY_SAVE_FN, REPLAY_SAVE_SIG.len())
        .ok_or(format!("保存関数 0x{REPLAY_SAVE_FN:08X} の先頭が読めない"))?;
    if sig != REPLAY_SAVE_SIG {
        return Err(format!(
            "保存関数 0x{REPLAY_SAVE_FN:08X} の先頭が署名と違う（{:02X?}）ので呼ばない",
            sig
        ));
    }
    let a = SaveAddrs::th09();
    for (addr, len) in a.direct_ranges() {
        if read_own_memory(addr, len as usize).is_none() {
            return Err(format!(
                "直に読むアドレス 0x{addr:08X}（{len} バイト）が読めない"
            ));
        }
    }
    let log = run_path.ok_or("本ログの置き場が決まっていない")?;
    let dir = log.parent().ok_or("本ログのフォルダが分からない")?;
    let exe = exe_dir.ok_or("exe のフォルダが分からない")?;
    if is_same_or_inside(&dir.to_string_lossy(), &exe.to_string_lossy()) {
        return Err(format!(
            "{} が exe のフォルダの中なので書かない",
            dir.display()
        ));
    }
    let name = side_file_name(
        &log.file_name()
            .ok_or("本ログの名前が分からない")?
            .to_string_lossy(),
        LOG_SUFFIX,
    );
    let detail = crate::log_mode::detail();
    let mut file = open_worker_log(detail, dir, &name, exe)?;
    let self_name = net.map_or(String::new(), |s| Name::new(&s.name).as_str().to_string());
    let _ = writeln!(
        file,
        "M5b の自動保存（区切り方 {:?}）。保存関数 0x{REPLAY_SAVE_FN:08X} の署名が一致",
        settings.split,
    );
    let mut f: i64 = 0;
    // SAFETY: `f` は書ける i64。
    unsafe { QueryPerformanceFrequency(&mut f) };
    QPC_FREQ.store(f.max(0) as u64, Ordering::SeqCst);
    a.store();
    // ネットワークを置いた実行では、つながっている間にだけ名前を押さえる（`Worker::may_reserve`）。
    let mut worker = Worker::new(settings.split, self_name, net.is_some(), file)
        .with_problem_sink(Box::new(crate::main_log::push));
    std::thread::Builder::new()
        .name("higanrtool-replay".to_string())
        .spawn(move || {
            let mut os = RealVaultOs;
            let r = catch_unwind(AssertUnwindSafe(|| {
                loop {
                    worker.step(&HANDOFF, &mut os);
                    std::thread::sleep(Duration::from_millis(WORKER_PERIOD_MS));
                }
            }));
            if r.is_err() {
                ARMED.store(0, Ordering::SeqCst);
                PANICKED.store(1, Ordering::SeqCst);
                win::debug_out("HiganRTool: 保管庫のスレッドでパニックを受けた。自動保存を止める");
            }
        })
        .map_err(|e| format!("保管庫のスレッドを起こせない: {e}"))?;
    ARMED.store(1, Ordering::Release);
    Ok(Some(if detail {
        format!(
            "入れた（区切り方 {:?}。保管庫のスレッドのログは {name}）",
            settings.split
        )
    } else {
        format!(
            "入れた（区切り方 {:?}。保管庫のスレッドのログは書かない）",
            settings.split
        )
    }))
}

#[cfg(test)]
mod tests {
    //! 本物の踏み台と `RealSaveIo` を、偽の保存関数（先頭の 12 バイトを花映塚と同じにした素のアセンブリ）と偽のゲームのメモリで通す。
    use super::*;
    use crate::replay_save::{
        Attempt, ST_EMPTY, ST_READY, ST_USED, Skip, game_estimate, need_bound,
    };
    use crate::th09_addr::{GAME_MODE_MATCH, SAVE_EST_BASE};
    use core::sync::atomic::AtomicU8;

    /// 偽の保存関数と偽のゲームは 1 組しか無いので、1 本ずつ走らせる。
    static SERIAL: Mutex<()> = Mutex::new(());

    static HITS: AtomicU32 = AtomicU32::new(0);
    static SEEN_THIS: AtomicU32 = AtomicU32::new(0);
    static SEEN_PATH: AtomicU32 = AtomicU32::new(0);
    static SEEN_NAME: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
    static SEEN_FLAGS: [AtomicU32; STAGES] = [const { AtomicU32::new(0) }; STAGES];
    static FAKE_RNG: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
    /// 偽の保存関数が呼ばれた時点の、見出しの `+0x190` / `+0x191`（下位 2 バイト）。
    static SEEN_CHARGE: AtomicU32 = AtomicU32::new(u32::MAX);
    /// 1 のときだけ、偽の保存関数が `this` を ReplayManager として見出しを読む（踏み台の検査の `this` は適当な値）。
    static PROBE_HEADER: AtomicU32 = AtomicU32::new(0);

    // SAFETY（`#[unsafe(naked)]`）: 花映塚の保存関数の真似。先頭の 12 バイトは署名と同じ（`.byte` で書くのは、アセンブラが
    // `mov ebp,esp` を別の符号にしうるため）。呼ばれた回数・this・path・名前・ゲームの見積もりのループが読む 10 語を検査の
    // 静的変数へコピー、偽の乱数をかき混ぜ、`ret 4` で戻る（本物と同じく名前を片付ける）。ebx・esi には触らない。
    // 書くのは検査の静的変数と自分の枠だけ。
    #[unsafe(naked)]
    extern "C" fn fake_save() {
        core::arch::naked_asm!(
            ".byte 0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x18, 0x06, 0x00, 0x00, 0x57, 0x8B, 0xF9",
            "lock inc dword ptr [{hits}]",
            "mov dword ptr [{this}], edi",
            "mov dword ptr [{path}], edx",
            "mov eax, dword ptr [ebp + 8]",
            "mov ecx, dword ptr [eax]",
            "mov dword ptr [{name}], ecx",
            "mov ecx, dword ptr [eax + 4]",
            "mov dword ptr [{name} + 4], ecx",
            "mov ecx, dword ptr [eax + 8]",
            "mov dword ptr [{name} + 8], ecx",
            "mov ecx, dword ptr [ebp - {f}]",
            "mov dword ptr [{flags}], ecx",
            "mov ecx, dword ptr [ebp - {f} + 4]",
            "mov dword ptr [{flags} + 4], ecx",
            "mov ecx, dword ptr [ebp - {f} + 8]",
            "mov dword ptr [{flags} + 8], ecx",
            "mov ecx, dword ptr [ebp - {f} + 12]",
            "mov dword ptr [{flags} + 12], ecx",
            "mov ecx, dword ptr [ebp - {f} + 16]",
            "mov dword ptr [{flags} + 16], ecx",
            "mov ecx, dword ptr [ebp - {f} + 20]",
            "mov dword ptr [{flags} + 20], ecx",
            "mov ecx, dword ptr [ebp - {f} + 24]",
            "mov dword ptr [{flags} + 24], ecx",
            "mov ecx, dword ptr [ebp - {f} + 28]",
            "mov dword ptr [{flags} + 28], ecx",
            "mov ecx, dword ptr [ebp - {f} + 32]",
            "mov dword ptr [{flags} + 32], ecx",
            "mov ecx, dword ptr [ebp - {f} + 36]",
            "mov dword ptr [{flags} + 36], ecx",
            "cmp dword ptr [{probe}], 0",
            "je 2f",
            "mov eax, dword ptr [edi + {hdr}]",
            "movzx ecx, word ptr [eax + {charge}]",
            "mov dword ptr [{seen_charge}], ecx",
            "2:",
            "xor dword ptr [{rng}], 0x5A5A",
            "add dword ptr [{rng} + 4], 3",
            "pop edi",
            "mov esp, ebp",
            "pop ebp",
            "ret 4",
            hits = sym HITS,
            this = sym SEEN_THIS,
            path = sym SEEN_PATH,
            name = sym SEEN_NAME,
            flags = sym SEEN_FLAGS,
            rng = sym FAKE_RNG,
            seen_charge = sym SEEN_CHARGE,
            probe = sym PROBE_HEADER,
            hdr = const REPLAY_HEADER_PTR,
            charge = const REPLAY_HEADER_CHARGE,
            f = const SAVE_EST_FLAGS_EBP,
        )
    }

    // SAFETY（`#[unsafe(naked)]`）: 先頭の 12 バイトの最後の 1 バイトだけが違う偽物。呼ばれたら数える（署名が合わないのに
    // 呼んだことが分かる）。本体は `fake_save` と同じ形。
    #[unsafe(naked)]
    extern "C" fn wrong_save() {
        core::arch::naked_asm!(
            ".byte 0x55, 0x8B, 0xEC, 0x81, 0xEC, 0x18, 0x06, 0x00, 0x00, 0x57, 0x8B, 0xF8",
            "lock inc dword ptr [{hits}]",
            "pop edi",
            "mov esp, ebp",
            "pop ebp",
            "ret 4",
            hits = sym HITS,
        )
    }

    fn addr_of_fn(f: extern "C" fn()) -> u32 {
        f as *const () as usize as u32
    }

    const HARNESS_ARGS: u32 = 11 * 4;
    const HARNESS_BELOW: u32 = HARNESS_ARGS + 4 + 4 + 12 + 4 + SNAP_BELOW;
    const TABLE_WORDS: usize = 11 + STAGES;

    // SAFETY（`#[unsafe(naked)]`）: cdecl（引数 1 つ = 21 語の表のアドレス）。呼ぶ側が保つレジスタを push / pop で戻す。踏み台を
    // 呼ぶ前に、`E − 0x200` からの 10 語へ模様（本物のゲームで「呼び手のスタックの残り」に当たる値）を書く。そこは踏み台の積みと
    // 保存関数の枠より下で、呼ぶ前に上書きされない。引数を積んで `save_call` を呼び、片付けて戻り値をそのまま返す。
    #[unsafe(naked)]
    unsafe extern "C" fn harness(_table: u32) -> u32 {
        core::arch::naked_asm!(
            "push ebp",
            "mov ebp, esp",
            "push ebx",
            "push esi",
            "push edi",
            "mov esi, dword ptr [ebp + 8]",
            "xor ecx, ecx",
            "2:",
            "mov eax, dword ptr [esi + ecx*4 + {pat}]",
            "mov dword ptr [esp + ecx*4 - {g}], eax",
            "inc ecx",
            "cmp ecx, {stages}",
            "jl 2b",
            "push dword ptr [esi + 40]",
            "push dword ptr [esi + 36]",
            "push dword ptr [esi + 32]",
            "push dword ptr [esi + 28]",
            "push dword ptr [esi + 24]",
            "push dword ptr [esi + 20]",
            "push dword ptr [esi + 16]",
            "push dword ptr [esi + 12]",
            "push dword ptr [esi + 8]",
            "push dword ptr [esi + 4]",
            "push dword ptr [esi]",
            "call {call}",
            "add esp, {args}",
            "pop edi",
            "pop esi",
            "pop ebx",
            "pop ebp",
            "ret",
            g = const HARNESS_BELOW,
            args = const HARNESS_ARGS,
            pat = const HARNESS_ARGS,
            stages = const STAGES,
            call = sym save_call,
        )
    }

    struct Run {
        ret: u32,
        before: [u32; STAGES],
        after: [u32; STAGES],
        est: u32,
    }

    /// `pattern` は呼び手のスタックの残り（書く前の値）、`real` はヘッダの本当の印。
    fn run_harness(
        func: u32,
        pattern: [u32; STAGES],
        real: [u32; STAGES],
        chunks: [u32; STAGES],
        base: u32,
        need: u32,
    ) -> Run {
        PROBE_HEADER.store(0, Ordering::SeqCst);
        let name: [u8; NAME_BUF] = *b"12:34:56\0\0\0\0";
        let path = b"C:\\x\0";
        let mut before = [0xFFFF_FFFFu32; STAGES];
        let mut after = [0xEEEE_EEEEu32; STAGES];
        let mut est = 0u32;
        let mut t = [0u32; TABLE_WORDS];
        t[0] = func;
        t[1] = 0x00AB_CDE0;
        t[2] = path.as_ptr() as usize as u32;
        t[3] = name.as_ptr() as usize as u32;
        t[4] = before.as_mut_ptr() as usize as u32;
        t[5] = real.as_ptr() as usize as u32;
        t[6] = after.as_mut_ptr() as usize as u32;
        t[7] = chunks.as_ptr() as usize as u32;
        t[8] = base;
        t[9] = need;
        t[10] = &mut est as *mut u32 as usize as u32;
        t[11..].copy_from_slice(&pattern);
        // SAFETY: 表の 21 語は上で埋めた。関数は `fake_save` か `wrong_save`（先頭の 12 バイトの後で釣り合わせて `ret 4`）。
        // 名前・パス・模様・印・塊のバッファはこの関数のスタックで、呼び出しの間生きている。
        let ret = unsafe { harness(t.as_ptr() as usize as u32) };
        Run {
            ret,
            before,
            after,
            est,
        }
    }

    fn flags_seen() -> [u32; STAGES] {
        core::array::from_fn(|i| SEEN_FLAGS[i].load(Ordering::SeqCst))
    }

    /// 本当の印の標本: 語ごとに違う値で、0 の面を混ぜる（0 の面を数えたら見積もりが変わる）。
    fn real_sample() -> [u32; STAGES] {
        let mut r: [u32; STAGES] = core::array::from_fn(|i| 0x0BB4_7000 + i as u32 * 0x44);
        r[1] = 0;
        r[4] = 0;
        r
    }

    #[test]
    fn save_call_writes_the_real_flags_where_the_game_loop_reads() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pattern: [u32; STAGES] = core::array::from_fn(|i| 0x5100_0000 + i as u32 * 0x111);
        let real = real_sample();
        let mut chunks = [1u32; STAGES];
        chunks[9] = 7;
        chunks[2] = 3;
        let h0 = HITS.load(Ordering::SeqCst);
        let r = run_harness(
            addr_of_fn(fake_save),
            pattern,
            real,
            chunks,
            SAVE_EST_BASE,
            0,
        );
        assert_eq!(r.ret, 1);
        assert_eq!(HITS.load(Ordering::SeqCst), h0 + 1);
        assert_eq!(
            r.before, pattern,
            "書く前の 10 語が呼び手のスタックの残りでない"
        );
        assert_eq!(r.after, real, "読み戻した 10 語が本当の印でない");
        assert_eq!(
            flags_seen(),
            real,
            "偽の保存関数が -0x1F8(%ebp) から読んだ 10 語が本当の印でない"
        );
        assert_eq!(r.est, game_estimate(SAVE_EST_BASE, &real, &chunks));
        assert_eq!(SEEN_THIS.load(Ordering::SeqCst), 0x00AB_CDE0);
        let name: Vec<u8> = SEEN_NAME
            .iter()
            .flat_map(|a| a.load(Ordering::SeqCst).to_le_bytes())
            .collect();
        assert_eq!(&name, b"12:34:56\0\0\0\0");
    }

    #[test]
    fn save_call_saves_long_matches_and_keeps_the_guard() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut real = [0u32; STAGES];
        real[9] = 0x0BB4_79F8;
        let mut chunks = [1u32; STAGES];
        chunks[9] = 4;
        let need = need_bound(&real, &chunks);
        assert_eq!(need, 0x1EC + 0x60 + 4 * 0x54D9);
        for base in [SAVE_EST_BASE, 0x801EC] {
            let h0 = HITS.load(Ordering::SeqCst);
            let r = run_harness(addr_of_fn(fake_save), [0; STAGES], real, chunks, base, need);
            assert_eq!(r.ret, 1, "0x{base:X}: 塊 4 つの試合を呼ばない");
            assert_eq!(HITS.load(Ordering::SeqCst), h0 + 1);
            assert_eq!(r.before, [0; STAGES]);
            assert_eq!(r.after, real);
            assert_eq!(r.est, base + 0x60 + 4 * 0x54D9);
            assert_eq!(flags_seen(), real);
        }
        let h0 = HITS.load(Ordering::SeqCst);
        let r = run_harness(
            addr_of_fn(fake_save),
            [0; STAGES],
            real,
            chunks,
            0x100,
            need,
        );
        assert_eq!(r.ret, 0);
        assert_eq!(HITS.load(Ordering::SeqCst), h0, "足りないのに呼んだ");
        assert_eq!(r.est, 0x100 + 0x60 + 4 * 0x54D9);
        assert_eq!(r.after, real, "呼ばないときも書いた 10 語を読み戻す");
        let r = run_harness(
            addr_of_fn(fake_save),
            [7; STAGES],
            [0; STAGES],
            chunks,
            SAVE_EST_BASE,
            0x1EC,
        );
        assert_eq!(r.ret, 1);
        assert_eq!(r.est, SAVE_EST_BASE);
        assert_eq!(r.before, [7; STAGES]);
        assert_eq!(r.after, [0; STAGES]);
    }

    #[repr(C, align(16))]
    struct Words<const N: usize>([AtomicU32; N]);
    static SUP: Words<{ 0x600 / 4 }> = Words([const { AtomicU32::new(0) }; 0x600 / 4]);
    /// menu, loading, game_mode, manager_ptr, p1 wins, p2 wins, required, completed_rounds, round_frames, 1P / 2P の Player のポインタ、1P / 2P の stats のポインタ
    static GLOBALS: Words<14> = Words([const { AtomicU32::new(0) }; 14]);
    /// 偽の設定の写し（`+0xB5` まで）。`GLOBALS` の 13 番目が、これを指すポインタの欄。
    static OPT_COPY: Words<{ 0xC0 / 4 }> = Words([const { AtomicU32::new(0) }; 0xC0 / 4]);
    static MENU_BYTE: AtomicU8 = AtomicU8::new(0);
    /// 偽の ReplayManager（+0x114 まで）と偽のヘッダ（+0x48 まで）と、面 9 の次の塊（2 つ。長いマッチの検査では 3 つ）。
    static RM: Words<{ 0x120 / 4 }> = Words([const { AtomicU32::new(0) }; 0x120 / 4]);
    static HDR: Words<{ 0x1F0 / 4 }> = Words([const { AtomicU32::new(0) }; 0x1F0 / 4]);
    static NODES: Words<{ 0x80 / 4 }> = Words([const { AtomicU32::new(0) }; 0x80 / 4]);
    static BASE_SITE: [AtomicU8; 8] = [const { AtomicU8::new(0) }; 8];
    /// 偽の Player（`+0xB8` まで）と、スコアを持つ塊（`+0x04` まで）。
    static PLAYER_1: Words<{ 0x100 / 4 }> = Words([const { AtomicU32::new(0) }; 0x100 / 4]);
    static PLAYER_2: Words<{ 0x100 / 4 }> = Words([const { AtomicU32::new(0) }; 0x100 / 4]);
    static STATS_1: Words<4> = Words([const { AtomicU32::new(0) }; 4]);
    static STATS_2: Words<4> = Words([const { AtomicU32::new(0) }; 4]);

    fn a32<const N: usize>(w: &'static Words<N>, i: usize) -> u32 {
        &w.0[i] as *const AtomicU32 as usize as u32
    }

    fn fake_addrs(save_fn: u32) -> SaveAddrs {
        SaveAddrs {
            supervisor: &SUP as *const _ as usize as u32,
            menu_flag: &MENU_BYTE as *const AtomicU8 as usize as u32,
            loading: a32(&GLOBALS, 1),
            game_mode: a32(&GLOBALS, 2),
            manager_ptr: a32(&GLOBALS, 3),
            wins: [a32(&GLOBALS, 4), a32(&GLOBALS, 5), a32(&GLOBALS, 6)],
            rng: &FAKE_RNG as *const _ as usize as u32,
            save_fn,
            est_base_site: &BASE_SITE as *const _ as usize as u32,
            completed_rounds: a32(&GLOBALS, 7),
            round_frames: a32(&GLOBALS, 8),
            players: [a32(&GLOBALS, 9), a32(&GLOBALS, 10)],
            stats: [a32(&GLOBALS, 11), a32(&GLOBALS, 12)],
            options_copy: a32(&GLOBALS, 13),
        }
    }

    /// 偽の設定の写しのチャージ型（1P・2P）と、偽の見出しの `+0x190` / `+0x191`（下位 2 バイト）。
    fn set_copy_charge(p1: u8, p2: u8) {
        let w = u32::from(p1) | u32::from(p2) << 8;
        OPT_COPY.0[(OPT_CHARGE / 4) as usize].store(w << ((OPT_CHARGE % 4) * 8), Ordering::SeqCst);
    }

    fn header_charge() -> [u8; 2] {
        let w = HDR.0[(REPLAY_HEADER_CHARGE / 4) as usize].load(Ordering::SeqCst);
        let sh = (REPLAY_HEADER_CHARGE % 4) * 8;
        [(w >> sh) as u8, (w >> (sh + 8)) as u8]
    }

    fn setup_fake_game() {
        SUP.0[(SUP_SCENE_CUR / 4) as usize].store(2, Ordering::SeqCst);
        SUP.0[(SUP_SCENE_REQ / 4) as usize].store(2, Ordering::SeqCst);
        MENU_BYTE.store(1, Ordering::SeqCst);
        GLOBALS.0[1].store(0, Ordering::SeqCst);
        GLOBALS.0[2].store(GAME_MODE_MATCH, Ordering::SeqCst);
        let rm = &RM as *const _ as usize as u32;
        GLOBALS.0[3].store(rm, Ordering::SeqCst);
        GLOBALS.0[4].store(0, Ordering::SeqCst);
        GLOBALS.0[5].store(2, Ordering::SeqCst);
        GLOBALS.0[6].store(2, Ordering::SeqCst);
        for w in RM.0.iter() {
            w.store(0, Ordering::SeqCst);
        }
        RM.0[(REPLAY_HEADER_PTR / 4) as usize]
            .store(&HDR as *const _ as usize as u32, Ordering::SeqCst);
        RM.0[(REPLAY_MODE / 4) as usize].store(0, Ordering::SeqCst);
        let head9 = (REPLAY_STAGE_NODES + REPLAY_STAGE_STRIDE * 9 + REPLAY_NODE_NEXT) / 4;
        RM.0[head9 as usize].store(a32(&NODES, 0), Ordering::SeqCst);
        NODES.0[(REPLAY_NODE_NEXT / 4) as usize].store(a32(&NODES, 8), Ordering::SeqCst);
        NODES.0[(8 * 4 + REPLAY_NODE_NEXT) as usize / 4].store(0, Ordering::SeqCst);
        for w in HDR.0.iter() {
            w.store(0, Ordering::SeqCst);
        }
        HDR.0[(REPLAY_HEADER_STAGE_FLAGS / 4 + 9) as usize].store(0x1EC, Ordering::SeqCst);
        for (a, b) in BASE_SITE.iter().zip([0xB9u8, 0xEC, 0x01, 0x01, 0x00]) {
            a.store(b, Ordering::SeqCst);
        }
        GLOBALS.0[13].store(a32(&OPT_COPY, 0), Ordering::SeqCst);
        for w in OPT_COPY.0.iter() {
            w.store(0, Ordering::SeqCst);
        }
        // 見出しは手元の cfg の値（0 / 0）、生きた試合の値は 1 / 0。
        set_copy_charge(1, 0);
        SEEN_CHARGE.store(u32::MAX, Ordering::SeqCst);
        PROBE_HEADER.store(1, Ordering::SeqCst);
        FAKE_RNG[0].store(0x0000_BEEF, Ordering::SeqCst);
        FAKE_RNG[1].store(17, Ordering::SeqCst);
        FAKE_RNG[2].store(0x1234_5678, Ordering::SeqCst);
        GLOBALS.0[7].store(0, Ordering::SeqCst);
        GLOBALS.0[8].store(0, Ordering::SeqCst);
        for (cell, block) in [
            (9, a32(&PLAYER_1, 0)),
            (10, a32(&PLAYER_2, 0)),
            (11, a32(&STATS_1, 0)),
            (12, a32(&STATS_2, 0)),
        ] {
            GLOBALS.0[cell].store(block, Ordering::SeqCst);
        }
        for w in PLAYER_1
            .0
            .iter()
            .chain(&PLAYER_2.0)
            .chain(&STATS_1.0)
            .chain(&STATS_2.0)
        {
            w.store(0, Ordering::SeqCst);
        }
    }

    fn rng_now() -> [u32; 3] {
        core::array::from_fn(|i| FAKE_RNG[i].load(Ordering::SeqCst))
    }

    fn ready(h: &Handoff) {
        assert_eq!(h.state(), ST_EMPTY);
        crate::replay_save::tests::ready(h);
    }

    #[test]
    fn real_io_saves_once_and_restores_the_rng() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        setup_fake_game();
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut io = RealSaveIo {
            a: fake_addrs(addr_of_fn(fake_save)),
        };
        let before = rng_now();
        let h0 = HITS.load(Ordering::SeqCst);
        let mut out = Vec::new();
        for _ in 0..(crate::replay_save::MENU_SETTLE_TICKS + 20) {
            if let Some(a) = d.tick(&h, &mut io, None) {
                out.push(a);
            }
        }
        assert_eq!(out, vec![Attempt::Recorded]);
        assert_eq!(HITS.load(Ordering::SeqCst), h0 + 1);
        assert_eq!(rng_now(), before, "乱数を書き戻していない");
        assert_eq!(h.state(), ST_USED);
        let r = h.read_record().unwrap();
        assert!(r.called && r.rng_restored);
        assert_ne!(r.rng[1], r.rng[0], "偽の保存関数がかき混ぜた値が記録に無い");
        assert_eq!(r.written, flags_seen());
        assert_eq!(r.written, r.real);
        assert_eq!(r.chunks, [1, 1, 1, 1, 1, 1, 1, 1, 1, 3]);
        assert_eq!(r.real[9], 0x1EC);
        assert_eq!(r.base, SAVE_EST_BASE);
        assert_eq!(r.game_est, SAVE_EST_BASE + 0x60 + 3 * 0x54D9);
        assert_eq!(r.manager, &RM as *const _ as usize as u32);
        assert_eq!(SEEN_THIS.load(Ordering::SeqCst), r.manager);
        assert_eq!(SEEN_PATH.load(Ordering::SeqCst), h.path_addr());
        assert_eq!(r.wins, [0, 2, 2]);
        assert!(r.duration_us < 10_000_000);
        // 見出しは保存関数が呼ばれた時点で、設定の写しの値（1P = 1 / 2P = 0）になっている。
        assert_eq!(r.charge, ChargeSync::Written { p1: 1, p2: 0 });
        assert_eq!(
            SEEN_CHARGE.load(Ordering::SeqCst),
            1,
            "呼ぶ前に直していない"
        );
        assert_eq!(header_charge(), [1, 0]);
    }

    /// 壊れたら落ちる: 席の入れ替え・写しの別の欄を読む・ポインタが 0 でも書く・見出しの別の欄へ書く。
    #[test]
    fn real_io_copies_the_charge_into_the_header_seat_for_seat_and_refuses_bad_pointers() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        setup_fake_game();
        let rm = &RM as *const _ as usize as u32;
        let mut io = RealSaveIo {
            a: fake_addrs(addr_of_fn(fake_save)),
        };
        // 2P だけチャージ型。
        set_copy_charge(0, 1);
        assert_eq!(
            io.sync_header_charge(rm),
            ChargeSync::Written { p1: 0, p2: 1 }
        );
        assert_eq!(header_charge(), [0, 1]);
        // 1P だけ。隣の欄（+0x18F / +0x192）には触らない。
        HDR.0[(REPLAY_HEADER_CHARGE / 4) as usize].store(0xAABB_CCDD, Ordering::SeqCst);
        HDR.0[(REPLAY_HEADER_CHARGE / 4 + 1) as usize].store(0x1122_3344, Ordering::SeqCst);
        set_copy_charge(1, 0);
        assert_eq!(
            io.sync_header_charge(rm),
            ChargeSync::Written { p1: 1, p2: 0 }
        );
        let w0 = HDR.0[(REPLAY_HEADER_CHARGE / 4) as usize].load(Ordering::SeqCst);
        let sh = (REPLAY_HEADER_CHARGE % 4) * 8;
        let mut want = 0xAABB_CCDDu32;
        want = (want & !(0xFFFF << sh)) | (1 << sh);
        assert_eq!(w0, want, "2 バイトだけ書く");
        assert_eq!(
            HDR.0[(REPLAY_HEADER_CHARGE / 4 + 1) as usize].load(Ordering::SeqCst),
            0x1122_3344
        );
        // 悪いポインタでは書かない（落ちない）。
        HDR.0[(REPLAY_HEADER_CHARGE / 4) as usize].store(0, Ordering::SeqCst);
        let good = a32(&OPT_COPY, 0);
        for (cell, cell_value, rm_arg, want) in [
            // 写しのポインタの欄が読めない・値が 0・値が読めない場所を指す
            (0x10, good, rm, ChargeSync::NoCopy),
            (a32(&GLOBALS, 13), 0, rm, ChargeSync::NoCopy),
            (a32(&GLOBALS, 13), 0x10, rm, ChargeSync::CopyUnreadable),
            // ReplayManager が 0 か読めない場所
            (a32(&GLOBALS, 13), good, 0, ChargeSync::NoHeader),
            (a32(&GLOBALS, 13), good, 0x10, ChargeSync::NoHeader),
        ] {
            GLOBALS.0[13].store(cell_value, Ordering::SeqCst);
            let mut io = RealSaveIo {
                a: SaveAddrs {
                    options_copy: cell,
                    ..fake_addrs(addr_of_fn(fake_save))
                },
            };
            assert_eq!(
                io.sync_header_charge(rm_arg),
                want,
                "{cell:#x} {cell_value:#x} {rm_arg:#x}"
            );
            assert_eq!(header_charge(), [0, 0], "書いてはいけない");
        }
        // 見出しのポインタが 0 の ReplayManager。
        GLOBALS.0[13].store(a32(&OPT_COPY, 0), Ordering::SeqCst);
        RM.0[(REPLAY_HEADER_PTR / 4) as usize].store(0, Ordering::SeqCst);
        assert_eq!(io.sync_header_charge(rm), ChargeSync::NoHeader);
        // 見出しが読めない場所を指す。
        RM.0[(REPLAY_HEADER_PTR / 4) as usize].store(0x10, Ordering::SeqCst);
        assert_eq!(io.sync_header_charge(rm), ChargeSync::WriteFailed);
    }

    /// 壊れたら落ちる: 日付と名前の位置の取り違え・見出しが 0 や読めない場所のときに値を返す。
    #[test]
    fn real_io_reads_the_header_date_and_name_from_their_offsets_and_refuses_bad_pointers() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        setup_fake_game();
        let rm = &RM as *const _ as usize as u32;
        let mut io = RealSaveIo {
            a: fake_addrs(addr_of_fn(fake_save)),
        };
        let put = |off: usize, bytes: &[u8]| {
            for (i, b) in bytes.iter().enumerate() {
                let at = off + i;
                let sh = (at % 4) * 8;
                let w = &HDR.0[at / 4];
                let old = w.load(Ordering::SeqCst);
                w.store(
                    (old & !(0xFF << sh)) | (u32::from(*b) << sh),
                    Ordering::SeqCst,
                );
            }
        };
        put(HEADER_DATE_OFFSET, b"26/10/14\0");
        put(HEADER_NAME_OFFSET, b"12:34:56\0");
        assert_eq!(
            io.replay_ident(rm),
            Some(HeaderIdent {
                date: *b"26/10/14\0",
                name: *b"12:34:56\0",
            })
        );
        // 隣の欄（日付の直前の語・名前の直後の難易度）は読まない。
        put(HEADER_DATE_OFFSET - 1, b"Z");
        put(HEADER_NAME_OFFSET + HEADER_FIELD_LEN, b"Q");
        let got = io.replay_ident(rm).unwrap();
        assert_eq!(&got.date, b"26/10/14\0");
        assert_eq!(&got.name, b"12:34:56\0");
        RM.0[(REPLAY_HEADER_PTR / 4) as usize].store(0, Ordering::SeqCst);
        assert_eq!(io.replay_ident(rm), None, "見出しが 0");
        RM.0[(REPLAY_HEADER_PTR / 4) as usize].store(0x10, Ordering::SeqCst);
        assert_eq!(io.replay_ident(rm), None, "見出しが読めない場所");
        assert_eq!(io.replay_ident(0), None, "ReplayManager が 0");
        assert_eq!(io.replay_ident(0x10), None, "ReplayManager が読めない場所");
    }

    #[test]
    fn real_io_saves_a_four_chunk_match() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for site in [
            [0xB9u8, 0xEC, 0x01, 0x01, 0x00],
            [0xB9u8, 0xEC, 0x01, 0x08, 0x00],
        ] {
            setup_fake_game();
            NODES.0[(8 * 4 + REPLAY_NODE_NEXT) as usize / 4]
                .store(a32(&NODES, 16), Ordering::SeqCst);
            NODES.0[(16 * 4 + REPLAY_NODE_NEXT) as usize / 4].store(0, Ordering::SeqCst);
            for (a, b) in BASE_SITE.iter().zip(site) {
                a.store(b, Ordering::SeqCst);
            }
            let base = u32::from_le_bytes([site[1], site[2], site[3], site[4]]);
            let h = Handoff::new();
            ready(&h);
            let mut d = Driver::new();
            let mut io = RealSaveIo {
                a: fake_addrs(addr_of_fn(fake_save)),
            };
            let h0 = HITS.load(Ordering::SeqCst);
            let mut out = Vec::new();
            for _ in 0..(crate::replay_save::MENU_SETTLE_TICKS + 5) {
                if let Some(a) = d.tick(&h, &mut io, None) {
                    out.push(a);
                }
            }
            assert_eq!(out, vec![Attempt::Recorded]);
            assert_eq!(HITS.load(Ordering::SeqCst), h0 + 1, "0x{base:X}: 呼ばない");
            let r = h.read_record().unwrap();
            assert!(r.called);
            assert_eq!(r.chunks[9], 4);
            assert_eq!(r.need, 0x1EC + 0x60 + 4 * 0x54D9);
            assert_eq!(r.base, base);
            assert_eq!(r.game_est, base + 0x60 + 4 * 0x54D9);
            assert_eq!(r.written, r.real);
            assert_eq!(flags_seen(), r.real);
        }
    }

    #[test]
    fn real_io_refuses_wrong_signature_and_playback() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        type Break = fn();
        let cases: [(u32, Break, Skip); 3] = [
            (addr_of_fn(wrong_save), || {}, Skip::Signature),
            (
                addr_of_fn(fake_save),
                || RM.0[(REPLAY_MODE / 4) as usize].store(1, Ordering::SeqCst),
                Skip::Playback,
            ),
            (
                addr_of_fn(fake_save),
                || BASE_SITE[0].store(0xB8, Ordering::SeqCst),
                Skip::EstimateUnreadable,
            ),
        ];
        for (f, brk, want) in cases {
            setup_fake_game();
            brk();
            let h = Handoff::new();
            ready(&h);
            let mut d = Driver::new();
            let mut io = RealSaveIo { a: fake_addrs(f) };
            let h0 = HITS.load(Ordering::SeqCst);
            let before = rng_now();
            let mut out = Vec::new();
            for _ in 0..(crate::replay_save::MENU_SETTLE_TICKS + 5) {
                if let Some(a) = d.tick(&h, &mut io, None) {
                    out.push(a);
                }
            }
            assert_eq!(out, vec![Attempt::Skipped(want)], "{want:?}");
            assert_eq!(HITS.load(Ordering::SeqCst), h0, "{want:?}: 呼んだ");
            assert_eq!(h.state(), ST_READY, "{want:?}");
            assert_eq!(rng_now(), before, "{want:?}");
        }
    }

    #[test]
    fn same_file_ansi_compares_identity() {
        let d = crate::replay_save::tests::temp_dir("identity");
        let a = d.join("a.rpy");
        let b = d.join("b.rpy");
        let fa = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&a)
            .unwrap();
        std::fs::write(&b, b"x").unwrap();
        let ansi = |p: &Path| p.to_str().unwrap().as_bytes().to_vec();
        assert_eq!(same_file_ansi(&ansi(&a), &fa), Ok(true));
        assert_eq!(same_file_ansi(&ansi(&b), &fa), Ok(false));
        assert!(same_file_ansi(&ansi(&d.join("none.rpy")), &fa).is_err());
        drop(fa);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn touch_handle_does_not_block_the_games_create_always() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = crate::replay_save::tests::temp_dir("touch_real");
        let f = d.join("th9_ud0001.rpy");
        std::fs::write(&f, b"").unwrap();
        crate::replay_save::tests::set_age(&f, 3600);
        RealVaultOs.touch(&f).unwrap();
        let m = std::fs::metadata(&f).unwrap();
        assert_eq!(m.len(), 0);
        let age = std::time::SystemTime::now()
            .duration_since(m.modified().unwrap())
            .unwrap_or_default();
        assert!(age.as_secs() < 60, "新しくなっていない（{age:?}）");
        let held = open_attributes_only(&f).unwrap();
        let game = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&f);
        assert!(game.is_ok(), "ゲームの開き方が断られた: {game:?}");
        game.unwrap().write_all(b"T9RP").unwrap();
        drop(held);
        assert_eq!(std::fs::read(&f).unwrap(), b"T9RP");
        assert!(RealVaultOs.touch(&d.join("none.rpy")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn script_tick_drives_the_saver_and_keeps_the_gate_open() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _h = crate::stub::testkit::REAL_HOOK_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use crate::script_win::{self as sw, tests as swt};
        let run = |armed: bool| -> (u32, u32) {
            sw::stop();
            crate::shot_win::set_off();
            crate::stub::testkit::set_fake(1, 3, 3, 1.0, 2.0);
            for a in swt::FAKE_DATA.iter() {
                a.store(0, Ordering::SeqCst);
            }
            sw::arm(
                higanr_core::script::parse("scene=3 wait=1 p1=Z hold=2\n").unwrap(),
                swt::fake_addrs(),
                swt::feed_fn_addr(),
                swt::local_fn_addr(),
            )
            .expect("arm");
            sw::start().expect("start");
            setup_fake_game();
            fake_addrs(addr_of_fn(fake_save)).store();
            ARMED.store(u32::from(armed), Ordering::SeqCst);
            let r0 = HANDOFF.skip_counts().1[Skip::NotReady as usize];
            for k in 0..30 {
                sw::script_on_tick(k);
            }
            let on = sw::SCRIPT_ON.load(Ordering::SeqCst);
            let retries = HANDOFF.skip_counts().1[Skip::NotReady as usize] - r0;
            ARMED.store(0, Ordering::SeqCst);
            sw::stop();
            assert_eq!(
                sw::progress().state,
                crate::scripted::RunState::Done,
                "台本が終わっていない"
            );
            (on, retries)
        };
        let (on, retries) = run(true);
        assert_eq!(on, 1, "自動保存が入っているのに分かれ道を閉じた");
        assert!(retries > 0, "script_on_tick から on_tick が呼ばれていない");
        let (on, retries) = run(false);
        assert_eq!(on, 0, "入っていないのに分かれ道を閉じない");
        assert_eq!(retries, 0);
    }

    #[test]
    fn the_worker_log_is_not_created_when_brief_and_hides_the_game_folder_and_addresses_when_detailed()
     {
        let dir = std::env::temp_dir().join(format!("higanr_worker_log_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 公開用の走査が拾わないよう、ユーザーのフォルダのパスは組み立てて作る。
        let home = format!("C:{0}Users{0}alice", '\\');
        let game = format!("{home}\\Games\\th09");
        let exe = Path::new(&game);
        let mut w = open_worker_log(false, &dir, "a_replay.log", exe).unwrap();
        writeln!(w, "簡潔のときは捨てる").unwrap();
        drop(w);
        assert!(
            !dir.join("a_replay.log").exists(),
            "簡潔なのにファイルを作った"
        );
        let mut w = open_worker_log(true, &dir, "b_replay.log", exe).unwrap();
        writeln!(
            w,
            "準備: {game}\\Replay_HiganRTool\\x を開けない 198.51.100.9:40000"
        )
        .unwrap();
        drop(w);
        let text = std::fs::read_to_string(dir.join("b_replay.log")).unwrap();
        assert_eq!(
            text,
            "準備: <ゲーム>\\Replay_HiganRTool\\x を開けない 外:40000\n"
        );
        assert!(
            open_worker_log(true, &dir, "b_replay.log", exe).is_err(),
            "上書きしない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_io_reads_the_round_boundary_from_the_games_memory_and_survives_bad_pointers() {
        use crate::th09_addr::{
            PLAYER_BOSS_ATTACKS, PLAYER_BOSS_REVERSALS, PLAYER_LIFE, PLAYER_SPELL_ATTACKS,
            STATS_SCORE,
        };
        use higanr_core::replay_rounds::SideSnap;
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        setup_fake_game();
        let mut io = RealSaveIo {
            a: fake_addrs(addr_of_fn(fake_save)),
        };
        assert_eq!(io.completed_rounds(), 0);
        GLOBALS.0[7].store(2, Ordering::SeqCst);
        GLOBALS.0[8].store(3599, Ordering::SeqCst);
        GLOBALS.0[4].store(1, Ordering::SeqCst);
        GLOBALS.0[5].store(1, Ordering::SeqCst);
        for (block, base) in [(&PLAYER_1, 4u32), (&PLAYER_2, 9)] {
            for (off, v) in [
                (PLAYER_LIFE, base),
                (PLAYER_SPELL_ATTACKS, base + 1),
                (PLAYER_BOSS_ATTACKS, base + 2),
                (PLAYER_BOSS_REVERSALS, base + 3),
            ] {
                block.0[(off / 4) as usize].store(v, Ordering::SeqCst);
            }
        }
        STATS_1.0[(STATS_SCORE / 4) as usize].store(1234, Ordering::SeqCst);
        STATS_2.0[(STATS_SCORE / 4) as usize].store(5678, Ordering::SeqCst);
        assert_eq!(io.completed_rounds(), 2);
        let b = io.read_boundary();
        let side = |life, c23, boss, rev, score| {
            Some(SideSnap {
                life,
                c23,
                boss,
                rev,
                score,
            })
        };
        assert_eq!(b.frames, 3599);
        assert_eq!(b.wins, [1, 1]);
        assert_eq!(b.side, [side(4, 5, 6, 7, 1234), side(9, 10, 11, 12, 5678)]);
        // 2P の Player が片付け済み（ポインタ 0）、1P のスコアの塊のポインタが読めないアドレス: その席だけ読めない。落ちない。
        GLOBALS.0[10].store(0, Ordering::SeqCst);
        GLOBALS.0[11].store(0x10, Ordering::SeqCst);
        let b = io.read_boundary();
        assert_eq!(b.side, [None, None]);
        assert_eq!((b.frames, b.wins), (3599, [1, 1]));
        GLOBALS.0[11].store(a32(&STATS_1, 0), Ordering::SeqCst);
        assert_eq!(io.read_boundary().side[0], side(4, 5, 6, 7, 1234));
        // 悪い時計や UTC でない値でなく、いまの時刻（2023 年以後）が返る。
        assert!(io.unix_secs() > 1_700_000_000);
    }

    #[test]
    fn the_addresses_for_the_round_log_are_the_ones_found_in_the_disassembly() {
        use crate::th09_addr::{
            PLAYER_BOSS_ATTACKS, PLAYER_BOSS_REVERSALS, PLAYER_SPELL_ATTACKS, STATS_SCORE,
        };
        let a = SaveAddrs::th09();
        // 値は逆アセンブルの命令の直値（`0x00415D81 incl 0x4a7e90`、`0x0041B069 mov $0x4a7dac,%esi` など）。名前では引けないので値で書く。
        assert_eq!(a.completed_rounds, 0x004A_7E90);
        assert_eq!(a.round_frames, 0x004A_7E40);
        assert_eq!(a.players, [0x004A_7D94, 0x004A_7DCC]);
        assert_eq!(a.stats, [0x004A_7DAC, 0x004A_7DE4]);
        assert_eq!(a.players[1] - a.players[0], 0x38, "陣の間隔");
        assert_eq!(a.stats[1] - a.stats[0], 0x38, "陣の間隔");
        assert_eq!(
            (
                PLAYER_SPELL_ATTACKS,
                PLAYER_BOSS_ATTACKS,
                PLAYER_BOSS_REVERSALS,
                STATS_SCORE
            ),
            (0xB0, 0xB4, 0xB8, 0x04)
        );
        // 起動のとき読めると確かめる範囲に、新しい欄が全部入っている（入っていないと直読みが外れる）。
        let ranges = a.direct_ranges();
        for addr in [
            a.completed_rounds,
            a.round_frames,
            a.players[0],
            a.players[1],
            a.stats[0],
            a.stats[1],
            a.options_copy,
        ] {
            assert!(ranges.contains(&(addr, 4)), "0x{addr:08X}");
        }
        // 設定の写しのポインタの欄と、チャージ型の位置は、ゲーム側の名前と同じ値（`net_game::write_charge` が生きた試合で書く先）。
        assert_eq!(a.options_copy, 0x004A_7E78);
        assert_eq!(OPT_CHARGE, 0xB4);
        // 見出しの位置は、リプレイを書き換える側（core）の位置 `+0x190` と同じ。
        assert_eq!(
            REPLAY_HEADER_CHARGE as usize,
            higanr_core::replay_rewrite::CHARGE_OFFSET
        );
        assert_eq!(REPLAY_HEADER_CHARGE, 0x190);
        // 語へのコピーと戻しで欄が入れ替わらない（全部違う値で見る）。
        let distinct = SaveAddrs {
            supervisor: 1,
            menu_flag: 2,
            loading: 3,
            game_mode: 4,
            manager_ptr: 5,
            wins: [6, 7, 8],
            rng: 9,
            save_fn: 10,
            est_base_site: 11,
            completed_rounds: 12,
            round_frames: 13,
            players: [14, 15],
            stats: [16, 17],
            options_copy: 18,
        };
        assert_eq!(SaveAddrs::from_words(distinct.words()), distinct);
        let mut seen = distinct.words().to_vec();
        seen.sort_unstable();
        assert_eq!(seen, (1..=18).collect::<Vec<u32>>());
    }

    #[test]
    fn the_confirmed_tick_hook_does_nothing_when_the_saver_is_off() {
        // 入れていない実行（`ARMED` = 0）で呼んでも、ゲームのメモリに触らず、パニックもしない（他の検査が `ARMED` を立てている間に当たっても、
        // マッチが始まっていない記録は読まずに戻る）。
        on_confirmed_tick();
    }

    #[test]
    fn arm_is_off_by_default_and_checks_the_signature() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(arm(None, None, None, None), Ok(None));
        assert_eq!(
            arm(Some("[netplay]\nenable = 1\n"), None, None, None),
            Ok(None)
        );
        assert_eq!(
            arm(Some("[replay]\nenable = 0\n"), None, None, None),
            Ok(None)
        );
        assert!(arm(Some("[replay]\nenable = 1\nlayout = x\n"), None, None, None).is_err());
        let e = arm(Some("[replay]\nenable = 1\n"), None, None, None).unwrap_err();
        assert!(e.contains("保存関数"), "{e}");
        assert_eq!(ARMED.load(Ordering::SeqCst), 0);
        assert!(!armed());
    }
}
