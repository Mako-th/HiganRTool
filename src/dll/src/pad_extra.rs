//! パッドの十字キー（POV ハット）を方向のビットに足す判断と設置の手順（pure。unsafe 無し）。
//!
//! 花映塚はパッド 1 台を読む関数 `0x0042B410` で lX・lY（winmm は X・Y）しか読まず、十字キーが POV で報告されるパッドでは
//! 方向が立たない。そこで、その関数への 4 つの呼び出しの rel32 を自前の入口（`pad_extra_win::pad_entry`）へ向け、入口は
//! 元の関数を同じ引数で呼んだ後で、`finish` が POV から作った方向を戻り値に OR する。ここは「戻り値にどう足すか」
//! （`finish`。デバイスは `PadIo` の向こう）と「6 行を照合して 4 つの rel32 を書く」（`install`。書く手順は
//! `hook::install_call_hook_at`）。ゲームのメモリ・Win32・COM の呼び出しは `pad_extra_win.rs`。
//!
//! 遅延: 読む時点は元の関数が返った直後（同じ tick の同じ地点）なので、入力が 1 フレーム遅れることは無い。足すのは入口 1 回
//! あたり原子変数の読み書きが数回と、winmm の経路なら `joyGetPosEx` 1 回（DirectInput の経路なら `GetDeviceState` 1 回）。
//! 1 tick に最大 5 回呼ばれるが、デバイスごとに 1 tick 1 回へはまとめない: 呼び出しごとに今の状態を読めば止まった値を返す
//! 恐れが無く、まとめるには tick を見分ける印が要る（候補の `SUP_FRAME` は、待ちの間に返す描画だけのフレームでは on_tick の
//! 途中から直に戻るので進まない）。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use higanr_core::pad_dirs::{
    Counts, JOYCAPS_HASPOV, PovMode, Route, device_index, merge_dirs, pov_dirs, route, winmm_joy_id,
};
use higanr_core::patch;

use crate::hook::{self, HookEnv};
use crate::sites::{SiteDef, SiteOutcome, Use, hex_bytes, judge};

/// デバイスの読み（本物は `pad_extra_win::RealPadIo`、検査では偽物）。
pub trait PadIo {
    /// `[0x004B3114]` と `[0x004B3118]`（IDirectInputDevice8 へのポインタ。0 = 無い）。
    fn slots(&self) -> [u32; 2];
    /// DirectInput のデバイス `device`（`slots` の値）の `DIJOYSTATE2` の `rgdwPOV[0]`。取れなければ `None`。
    fn dinput_pov0(&self, device: u32) -> Option<u32>;
    /// winmm の `joyGetPosEx(joy_id)` の `dwPOV`。取れなければ `None`。
    fn winmm_pov(&self, joy_id: u32) -> Option<u32>;
}

/// ゲームのスレッドが足す数えと、別のスレッドが置く旗。全部原子変数（ロックも確保も無い）。
pub struct PadState {
    enabled: AtomicU32,
    /// winmm の経路で `dwPOV` を読んでよいデバイス（添字 = デバイス番号）。ハットの有無を引き直すスレッドが置く
    /// （`higanr_core::pad_dirs::winmm_may_read`）。
    winmm_gate: [AtomicU32; 2],
    di: AtomicU32,
    mm: AtomicU32,
    added: AtomicU32,
    failed: AtomicU32,
    skipped: AtomicU32,
}

impl PadState {
    pub const fn new() -> Self {
        PadState {
            enabled: AtomicU32::new(0),
            winmm_gate: [AtomicU32::new(0), AtomicU32::new(0)],
            di: AtomicU32::new(0),
            mm: AtomicU32::new(0),
            added: AtomicU32::new(0),
            failed: AtomicU32::new(0),
            skipped: AtomicU32::new(0),
        }
    }

    /// 入口が POV を足し始める（rel32 を書く前に呼ぶ）。
    pub fn enable(&self) {
        self.enabled.store(1, Ordering::Release);
    }

    /// 入口が元の戻り値をそのまま返す（設置に失敗した・切ったとき）。
    pub fn disable(&self) {
        self.enabled.store(0, Ordering::Release);
    }

    pub fn set_winmm_gate(&self, dev: usize, open: bool) {
        if let Some(g) = self.winmm_gate.get(dev) {
            g.store(u32::from(open), Ordering::Release);
        }
    }

    pub fn winmm_gate(&self, dev: usize) -> bool {
        self.winmm_gate
            .get(dev)
            .is_some_and(|g| g.load(Ordering::Acquire) != 0)
    }

    pub fn counts(&self) -> Counts {
        Counts {
            di: self.di.load(Ordering::Relaxed),
            mm: self.mm.load(Ordering::Relaxed),
            added: self.added.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    /// ハットが無い（旗が閉じている）ので `joyGetPosEx` を呼ばなかった回数。
    pub fn skipped(&self) -> u32 {
        self.skipped.load(Ordering::Relaxed)
    }
}

impl Default for PadState {
    fn default() -> Self {
        Self::new()
    }
}

/// 元の関数の戻り値 `ret` に、デバイス `dev` の十字キーの方向を足して返す（ゲームのスレッド。確保しない・待たない）。
/// 足せないとき（切ってある・デバイス番号が範囲外・ハットの旗が閉じている・読めない）は `ret` のまま。
pub fn finish(state: &PadState, io: &dyn PadIo, ret: u32, dev: u32) -> u32 {
    if state.enabled.load(Ordering::Acquire) == 0 {
        return ret;
    }
    let Some(idx) = device_index(dev) else {
        return ret;
    };
    let slots = io.slots();
    let pov = match route(slots) {
        Route::DirectInput => {
            state.di.fetch_add(1, Ordering::Relaxed);
            io.dinput_pov0(slots[idx])
        }
        Route::Winmm => {
            state.mm.fetch_add(1, Ordering::Relaxed);
            if !state.winmm_gate(winmm_joy_id(dev) as usize) {
                state.skipped.fetch_add(1, Ordering::Relaxed);
                return ret;
            }
            io.winmm_pov(winmm_joy_id(dev))
        }
    };
    match pov {
        Some(p) => {
            let out = merge_dirs(ret, pov_dirs(p));
            if out != ret {
                state.added.fetch_add(1, Ordering::Relaxed);
            }
            out
        }
        None => {
            state.failed.fetch_add(1, Ordering::Relaxed);
            ret
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// 照合した行の数（表の行の数と同じなら全部一致）。
    pub verified: usize,
    pub rows: usize,
    pub written: usize,
    /// 書く呼び出しの数（表の `CallRel32` の行）。
    pub call_rows: usize,
    pub lines: Vec<String>,
}

impl InstallReport {
    pub fn complete(&self) -> bool {
        self.verified == self.rows && self.written == self.call_rows && self.call_rows > 0
    }
}

/// 表の全行を照合し、**全部一致したときだけ**、`CallRel32` の行の rel32 を `entry` へ向ける。1 行でも合わない・読めないときは
/// 1 バイトも書かない。書く手順（全スレッドを止めて EIP を見て、止めたまま読み直して書く）は `hook::install_call_hook_at`。
/// 書く途中で失敗したときは書けた分だけ残る（4 つの呼び出しはどれも単独で正しい。書けなかった呼び出しは元の関数を呼ぶだけ）。
pub fn install(env: &mut dyn HookEnv, rows: &[SiteDef], entry: u32) -> InstallReport {
    let call_rows = rows
        .iter()
        .filter(|d| matches!(d.kind, Use::CallRel32 { .. }))
        .count();
    let mut r = InstallReport {
        verified: 0,
        rows: rows.len(),
        written: 0,
        call_rows,
        lines: Vec::new(),
    };
    let mut refused = false;
    for d in rows {
        // 表の行はどれも `MAX_PATCH_LEN`（16）以下。ヒープを使わない固定長で読む。
        let mut buf = [0u8; patch::MAX_PATCH_LEN];
        let n = d.expected.len();
        let actual = if n <= buf.len() && env.read_code(d.addr, &mut buf[..n]) {
            Some(&buf[..n])
        } else {
            None
        };
        match judge(d, actual) {
            SiteOutcome::Match => r.verified += 1,
            SiteOutcome::Mismatch { actual } => {
                refused = true;
                r.lines.push(format!(
                    "pad: 設置しない（{} 0x{:08X} が不一致 実際 = {} / 期待 = {}）",
                    d.name,
                    d.addr,
                    hex_bytes(&actual),
                    hex_bytes(d.expected)
                ));
            }
            SiteOutcome::Unreadable => {
                refused = true;
                r.lines.push(format!(
                    "pad: 設置しない（{} 0x{:08X} が読めない）",
                    d.name, d.addr
                ));
            }
        }
    }
    if refused {
        r.lines.push(format!(
            "pad: 照合 {}/{} 行が一致。1 バイトも書いていない",
            r.verified, r.rows
        ));
        return r;
    }
    // 全部書けて、読み返した飛び先が全部入口と一致したときは 1 行にまとめる。1 つでも欠けたら、どの呼び出しがどうだったかを全部残す。
    let mut detail = vec![format!(
        "pad: 照合 {}/{} 行が一致（呼び出し {} 行と、呼び先 0x{:08X} の頭と最後の ret 4）。呼び出しの rel32 を入口 0x{entry:08X} へ向ける",
        r.verified,
        r.rows,
        call_rows,
        rows.iter()
            .find_map(|d| match d.kind {
                Use::CallRel32 { original_target } => Some(original_target),
                _ => None,
            })
            .unwrap_or(0)
    )];
    let mut targets_ok = 0;
    for d in rows
        .iter()
        .filter(|d| matches!(d.kind, Use::CallRel32 { .. }))
    {
        let rep = hook::install_call_hook_at(env, d, entry, &mut |_, _| {});
        if rep.written() {
            r.written += 1;
            let target = rep
                .readback
                .and_then(|b| b.head5())
                .and_then(|b| patch::call_target(d.addr, &b));
            if target == Some(entry) {
                targets_ok += 1;
            }
            detail.push(format!(
                "pad: {} 0x{:08X}: 書いた（読み返した飛び先 {}）",
                d.name,
                d.addr,
                match target {
                    Some(t) if t == entry => format!("0x{t:08X} = 入口と一致"),
                    Some(t) => format!("0x{t:08X} = 入口と不一致"),
                    None => "読めない".to_string(),
                }
            ));
        } else {
            // 書けなかった理由は設置の共通の報告（止めたスレッド・EIP・保護）をそのまま載せる。
            let mut text = Vec::new();
            hook::write_report(&mut text, d, &rep);
            for l in String::from_utf8_lossy(&text).lines() {
                detail.push(format!("pad: {l}"));
            }
        }
    }
    if r.complete() && targets_ok == r.call_rows {
        r.lines.push(format!(
            "pad: 設置 {}/{} 地点（照合 {}/{} 行が一致。読み返した飛び先は {} 地点とも入口 0x{entry:08X} と一致）",
            r.written, r.call_rows, r.verified, r.rows, targets_ok
        ));
    } else {
        r.lines.extend(detail);
        r.lines
            .push(format!("pad: 設置 {}/{} 地点", r.written, r.call_rows));
    }
    r
}

/// `DllMain` で本ログへ頼む設置の行。全部書けたときは設定の行と設置の 1 行を 1 行にまとめ、書けなかったときは
/// 設定の行の後に理由の行を続ける。
pub fn attach_lines(settings: &str, report: &InstallReport) -> Vec<String> {
    match report.lines.as_slice() {
        [one] if report.complete() => {
            vec![format!("{settings}。{}", one.trim_start_matches("pad: "))]
        }
        rest => {
            let mut v = vec![settings.to_string()];
            v.extend(rest.iter().cloned());
            v
        }
    }
}

/// 本ログの 1 行: 数えのコピー。`when` は「初めて」「終わり」。
pub fn counts_line(when: &str, c: &Counts, skipped: u32, gate: [bool; 2]) -> String {
    format!(
        "pad: 数え（{when}）: 経路は DirectInput {} 回 / winmm {} 回（winmm でハットが無いので読まなかった {} 回。ハットあり: 0 番 {} / 1 番 {}）、十字キーで方向を足した回数 {}、読めなかった回数 {}",
        c.di,
        c.mm,
        skipped,
        yes_no(gate[0]),
        yes_no(gate[1]),
        c.added,
        c.failed
    )
}

/// ハットの有無の行を本ログに頼む回数の上限（初めて + 変化 3 回まで）。パッドの抜き差しで本ログが増え続けないように。
pub const MAX_CAPS_LINES: u32 = 4;

/// 別スレッドが 1 秒ごとに見たものから、本ログへ頼む行を決める: ハットの有無は変わったときだけ（高々 `MAX_CAPS_LINES` 回）、
/// 数えは「経路が初めて使われた・初めて方向を足した・初めて読めなかった」の最初の 1 回ずつ（`pad_dirs::should_log` を
/// 経過時間 0 で使う）。マッチ中に毎回は頼まない。
#[derive(Debug, Default)]
pub struct LogPlan {
    last_caps: Option<[Option<u32>; 2]>,
    caps_lines: u32,
    last_counts: Counts,
}

impl LogPlan {
    pub fn lines(
        &mut self,
        caps: [Option<u32>; 2],
        counts: &Counts,
        skipped: u32,
        gate: [bool; 2],
    ) -> Vec<String> {
        let mut v = Vec::new();
        if self.last_caps != Some(caps) {
            self.last_caps = Some(caps);
            if self.caps_lines < MAX_CAPS_LINES {
                self.caps_lines += 1;
                v.push(caps_line(caps));
            }
        }
        if higanr_core::pad_dirs::should_log(&self.last_counts, counts, 0) {
            v.push(counts_line("途中", counts, skipped, gate));
            self.last_counts = *counts;
        }
        v
    }
}

/// winmm の `joyGetDevCaps` の結果の行（添字 = デバイス番号。`None` = つながっていない）。ハット（POV）の有無は、
/// 効かなかったときにまず見る事実。2 台分を 1 行にして本ログの行を増やさない。
pub fn caps_line(caps: [Option<u32>; 2]) -> String {
    let one = |dev: usize, c: Option<u32>| match c {
        None => format!("{dev} 番 つながっていない（joyGetDevCaps が失敗）"),
        Some(c) => format!(
            "{dev} 番 wCaps = 0x{c:04X}、ハット（POV）{}",
            if c & JOYCAPS_HASPOV != 0 {
                "あり"
            } else {
                "なし（十字キーは足さない。always にすると確かめを外せる）"
            }
        ),
    };
    format!(
        "pad: winmm の確かめ: {}／{}",
        one(0, caps[0]),
        one(1, caps[1])
    )
}

fn yes_no(b: bool) -> &'static str {
    if b { "あり" } else { "なし" }
}

pub fn settings_line(mode: PovMode, source: &str) -> String {
    let m = match mode {
        PovMode::Off => "切る（[pad] pov = 0）",
        PovMode::Auto => "入り（winmm の経路は Windows が「ハットあり」と答えたデバイスだけ読む）",
        PovMode::Always => "入り・ハットの確かめなし（[pad] pov = always）",
    };
    format!("pad: 十字キー（POV）を方向に足す: {m}。設定 = {source}")
}

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};

    use super::*;
    use crate::hook::{ThreadRef, WriteDone};
    use crate::th09_addr::{PAD_READ_FN, PAD_SITES};
    use crate::ticks::CopyRow;
    use higanr_core::pad_dirs::{DIR_DOWN, DIR_LEFT, DIR_RIGHT, DIR_UP};

    /// 偽のデバイス。呼ばれた回数を数え、`panic_on` が立っていればパニックする。
    struct FakeIo {
        slots: [u32; 2],
        di: Option<u32>,
        mm: Option<u32>,
        di_calls: Cell<u32>,
        mm_calls: Cell<u32>,
        mm_last_id: Cell<u32>,
        di_last_dev: Cell<u32>,
        panic_on: bool,
    }

    impl FakeIo {
        fn new(slots: [u32; 2], di: Option<u32>, mm: Option<u32>) -> Self {
            FakeIo {
                slots,
                di,
                mm,
                di_calls: Cell::new(0),
                mm_calls: Cell::new(0),
                mm_last_id: Cell::new(99),
                di_last_dev: Cell::new(0),
                panic_on: false,
            }
        }
    }

    impl PadIo for FakeIo {
        fn slots(&self) -> [u32; 2] {
            assert!(!self.panic_on, "偽物: 読みの途中のパニック");
            self.slots
        }
        fn dinput_pov0(&self, device: u32) -> Option<u32> {
            self.di_calls.set(self.di_calls.get() + 1);
            self.di_last_dev.set(device);
            self.di
        }
        fn winmm_pov(&self, joy_id: u32) -> Option<u32> {
            self.mm_calls.set(self.mm_calls.get() + 1);
            self.mm_last_id.set(joy_id);
            self.mm
        }
    }

    fn on_state() -> PadState {
        let s = PadState::new();
        s.enable();
        s.set_winmm_gate(0, true);
        s.set_winmm_gate(1, true);
        s
    }

    const BUTTONS: u32 = 0x0000_130F; // Z・X・Shift・Esc（0x0F）と Enter（0x1000）、0x100（Ctrl）
    const TWO: [u32; 2] = [0x1000, 0x2000];
    const ONE: [u32; 2] = [0x1000, 0];

    #[test]
    fn winmm_route_adds_the_hat_and_keeps_the_buttons() {
        let s = on_state();
        for (pov, dirs) in [
            (0u32, DIR_UP),
            (4500, DIR_UP | DIR_RIGHT),
            (9000, DIR_RIGHT),
            (18_000, DIR_DOWN),
            (27_000, DIR_LEFT),
        ] {
            let io = FakeIo::new(ONE, None, Some(pov));
            assert_eq!(finish(&s, &io, BUTTONS, 0), BUTTONS | dirs, "{pov}");
            assert_eq!((io.mm_calls.get(), io.di_calls.get()), (1, 0));
        }
        let center = FakeIo::new(ONE, None, Some(0xFFFF));
        assert_eq!(finish(&s, &center, BUTTONS, 0), BUTTONS, "中心は元のまま");
        let c = s.counts();
        assert_eq!((c.mm, c.di, c.added, c.failed), (6, 0, 5, 0));
    }

    #[test]
    fn directinput_route_reads_the_slot_of_the_called_device() {
        let s = on_state();
        let io = FakeIo::new(TWO, Some(9000), Some(0));
        assert_eq!(finish(&s, &io, 0, 0), DIR_RIGHT);
        assert_eq!(io.di_last_dev.get(), TWO[0]);
        assert_eq!(finish(&s, &io, 0, 1), DIR_RIGHT);
        assert_eq!(io.di_last_dev.get(), TWO[1]);
        assert_eq!(io.mm_calls.get(), 0, "DirectInput の経路で winmm を呼んだ");
        assert_eq!(s.counts().di, 2);
        let closed = PadState::new();
        closed.enable();
        assert_eq!(finish(&closed, &io, 0, 0), DIR_RIGHT);
    }

    #[test]
    fn winmm_joy_id_follows_the_games_call_and_out_of_range_is_ignored() {
        let s = on_state();
        let io = FakeIo::new(ONE, None, Some(9000));
        finish(&s, &io, 0, 1);
        assert_eq!(io.mm_last_id.get(), 1);
        finish(&s, &io, 0, 0);
        assert_eq!(io.mm_last_id.get(), 0);
        let calls = io.mm_calls.get();
        assert_eq!(finish(&s, &io, BUTTONS, 2), BUTTONS);
        assert_eq!(finish(&s, &io, BUTTONS, u32::MAX), BUTTONS);
        assert_eq!(io.mm_calls.get(), calls, "範囲外のデバイスを読んだ");
    }

    #[test]
    fn winmm_gate_closed_means_no_read_and_no_direction() {
        let s = PadState::new();
        s.enable();
        s.set_winmm_gate(1, true);
        let io = FakeIo::new(ONE, None, Some(0));
        assert_eq!(finish(&s, &io, BUTTONS, 0), BUTTONS, "0 番は旗が閉じている");
        assert_eq!(io.mm_calls.get(), 0);
        assert_eq!(
            finish(&s, &io, BUTTONS, 1),
            BUTTONS | DIR_UP,
            "1 番は開いている"
        );
        assert_eq!((s.skipped(), s.counts().mm), (1, 2));
    }

    #[test]
    fn disabled_state_returns_the_original_value_without_touching_anything() {
        let s = PadState::new();
        s.set_winmm_gate(0, true);
        let io = FakeIo::new(ONE, None, Some(9000));
        assert_eq!(finish(&s, &io, BUTTONS, 0), BUTTONS);
        assert_eq!((io.mm_calls.get(), io.di_calls.get()), (0, 0));
        assert_eq!(s.counts(), Counts::default());
        s.enable();
        assert_eq!(finish(&s, &io, BUTTONS, 0), BUTTONS | DIR_RIGHT);
        s.disable();
        assert_eq!(finish(&s, &io, BUTTONS, 0), BUTTONS);
    }

    #[test]
    fn read_failure_keeps_the_original_and_is_counted() {
        let s = on_state();
        let io = FakeIo::new(ONE, None, None);
        assert_eq!(finish(&s, &io, BUTTONS, 0), BUTTONS);
        assert_eq!(s.counts().failed, 1);
        let di = FakeIo::new(TWO, None, None);
        assert_eq!(finish(&s, &di, BUTTONS, 1), BUTTONS);
        assert_eq!(s.counts().failed, 2);
        // スティックが右・十字キーも右: 戻り値は変わらないので「足した」に数えない。
        let same = FakeIo::new(ONE, None, Some(9000));
        assert_eq!(finish(&s, &same, DIR_RIGHT, 0), DIR_RIGHT);
        assert_eq!(s.counts().added, 0);
    }

    #[test]
    fn garbage_pov_values_add_nothing() {
        let s = on_state();
        for v in [36_000, 0xFFFF_0000, 0x1_0000, u32::MAX, 0x8000_FFFF] {
            let io = FakeIo::new(ONE, None, Some(v));
            assert_eq!(
                finish(&s, &io, 0xABCD_0000 | BUTTONS, 0),
                0xABCD_0000 | BUTTONS,
                "{v:#x}"
            );
        }
    }

    // ---- 設置 ----

    /// 偽のコードの置き場（アドレス → バイト列）。書いた回数を数え、止めている間だけ書けることを見る。
    struct FakeCode {
        mem: RefCell<Vec<(u32, Vec<u8>)>>,
        writes: Cell<u32>,
        stopped: Cell<u32>,
        threads: usize,
        write_fail_at: Option<u32>,
    }

    impl FakeCode {
        fn exe_like() -> Self {
            let mem = PAD_SITES
                .iter()
                .map(|d| (d.addr, d.expected.to_vec()))
                .collect();
            FakeCode {
                mem: RefCell::new(mem),
                writes: Cell::new(0),
                stopped: Cell::new(0),
                threads: 2,
                write_fail_at: None,
            }
        }
        fn bytes_at(&self, addr: u32) -> Vec<u8> {
            self.mem
                .borrow()
                .iter()
                .find(|(a, _)| *a == addr)
                .map(|(_, b)| b.clone())
                .expect("置いたアドレス")
        }
        fn snapshot(&self) -> Vec<(u32, Vec<u8>)> {
            self.mem.borrow().clone()
        }
    }

    impl HookEnv for FakeCode {
        fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
            Ok((1..=self.threads).map(ThreadRef).collect())
        }
        fn suspend(&mut self, _t: ThreadRef) -> bool {
            self.stopped.set(self.stopped.get() + 1);
            true
        }
        fn resume(&mut self, _t: ThreadRef) -> bool {
            self.stopped.set(self.stopped.get().saturating_sub(1));
            true
        }
        fn close(&mut self, _t: ThreadRef) {}
        fn eip(&mut self, _t: ThreadRef) -> Option<u32> {
            Some(0x0070_0000)
        }
        fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            for (base, bytes) in self.mem.borrow().iter() {
                if addr >= *base && (addr - base) as usize + buf.len() <= bytes.len() {
                    let off = (addr - base) as usize;
                    buf.copy_from_slice(&bytes[off..off + buf.len()]);
                    return true;
                }
            }
            false
        }
        fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
            assert_eq!(
                self.stopped.get() as usize,
                self.threads,
                "止めていない間に書いた"
            );
            if self.write_fail_at == Some(addr) {
                return Err(5);
            }
            for (base, mem) in self.mem.borrow_mut().iter_mut() {
                if addr >= *base && (addr - *base) as usize + bytes.len() <= mem.len() {
                    let off = (addr - *base) as usize;
                    mem[off..off + bytes.len()].copy_from_slice(bytes);
                    self.writes.set(self.writes.get() + 1);
                    return Ok(WriteDone {
                        protect_restored: true,
                        flushed: true,
                    });
                }
            }
            panic!("偽物: 置き場の外 0x{addr:08X} に書いた");
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

    const ENTRY: u32 = 0x1000_5000;

    #[test]
    fn install_points_the_four_calls_at_the_entry_and_touches_nothing_else() {
        let mut env = FakeCode::exe_like();
        let before = env.snapshot();
        let r = install(&mut env, PAD_SITES, ENTRY);
        assert!(r.complete(), "{:?}", r.lines);
        assert_eq!((r.verified, r.rows, r.written, r.call_rows), (6, 6, 4, 4));
        assert_eq!(env.writes.get(), 4);
        for d in PAD_SITES {
            let now = env.bytes_at(d.addr);
            match d.kind {
                Use::CallRel32 { .. } => {
                    assert_eq!(now[0], 0xE8, "{} のオペコードを変えた", d.name);
                    let rel = i32::from_le_bytes([now[1], now[2], now[3], now[4]]);
                    let target = (d.addr as i64 + 5 + i64::from(rel)) as u32;
                    assert_eq!(target, ENTRY, "{}", d.name);
                }
                _ => assert_eq!(now, d.expected, "{} は照合だけの行", d.name),
            }
        }
        assert_ne!(before, env.snapshot());
        assert!(
            r.lines.iter().any(|l| l.contains("設置 4/4 地点")),
            "{:?}",
            r.lines
        );
        assert_eq!(env.stopped.get(), 0, "止めたスレッドを再開していない");
    }

    #[test]
    fn log_plan_asks_for_few_lines() {
        let mut p = LogPlan::default();
        let caps = [Some(0x13), None];
        let gate = [true, false];
        let mut c = Counts::default();
        let v = p.lines(caps, &c, 0, gate);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("ハット（POV）あり"));
        c.mm = 10;
        let v = p.lines(caps, &c, 0, gate);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("数え（途中）"), "{v:?}");
        for i in 0..3600 {
            c.mm = 100 + i * 60;
            assert!(p.lines(caps, &c, 0, gate).is_empty(), "{i} 秒目");
        }
        c.added = 1;
        assert_eq!(p.lines(caps, &c, 0, gate).len(), 1);
        for i in 0..3600 {
            c.added = 2 + i;
            assert!(p.lines(caps, &c, 0, gate).is_empty(), "{i} 秒目");
        }
        c.failed = 1;
        assert_eq!(p.lines(caps, &c, 0, gate).len(), 1);
        c.failed = 50;
        assert!(p.lines(caps, &c, 0, gate).is_empty());
        let mut caps_total = 1;
        for i in 0..100u32 {
            let flap = [if i % 2 == 0 { None } else { Some(0x13) }, None];
            caps_total += p.lines(flap, &c, 0, gate).len();
        }
        assert_eq!(caps_total, MAX_CAPS_LINES as usize);
    }

    #[test]
    fn attach_lines_is_one_line_when_all_written_and_keeps_every_reason_otherwise() {
        let settings = settings_line(PovMode::Auto, "X:\\合成\\HiganRTool.ini");
        let mut env = FakeCode::exe_like();
        let ok = install(&mut env, PAD_SITES, ENTRY);
        assert_eq!(ok.lines.len(), 1, "{:?}", ok.lines);
        let v = attach_lines(&settings, &ok);
        assert_eq!(v.len(), 1, "{v:?}");
        for want in [
            "十字キー（POV）を方向に足す",
            "HiganRTool.ini",
            "設置 4/4 地点",
            "照合 6/6 行",
            "入口 0x10005000",
        ] {
            assert!(v[0].contains(want), "{want} が {} に無い", v[0]);
        }
        assert!(!v[0].contains("pad: 設置"), "二重の頭: {}", v[0]);
        let mut env = FakeCode::exe_like();
        env.mem.borrow_mut()[2].1[0] ^= 0x01;
        let bad = install(&mut env, PAD_SITES, ENTRY);
        let v = attach_lines(&settings, &bad);
        assert!(v.len() >= 3, "{v:?}");
        assert_eq!(v[0], settings);
        assert!(v.iter().any(|l| l.contains("不一致")), "{v:?}");
        assert!(
            v.iter().any(|l| l.contains("1 バイトも書いていない")),
            "{v:?}"
        );
        let mut env = FakeCode::exe_like();
        env.write_fail_at = Some(PAD_SITES[1].addr + 1);
        let part = install(&mut env, PAD_SITES, ENTRY);
        let v = attach_lines(&settings, &part);
        assert!(v.iter().any(|l| l.contains("頁の保護")), "{v:?}");
        assert!(v.iter().any(|l| l.contains("設置 3/4 地点")), "{v:?}");
    }

    #[test]
    fn install_writes_nothing_when_any_row_differs_by_one_byte() {
        let mut cases = 0;
        for (i, d) in PAD_SITES.iter().enumerate() {
            for pos in [0, d.expected.len() - 1] {
                let mut env = FakeCode::exe_like();
                env.mem.borrow_mut()[i].1[pos] ^= 0x01;
                let before = env.snapshot();
                let r = install(&mut env, PAD_SITES, ENTRY);
                assert!(!r.complete(), "{} の {pos} 番目", d.name);
                assert_eq!(r.written, 0, "{} の {pos} 番目で書いた", d.name);
                assert_eq!(env.writes.get(), 0);
                assert_eq!(before, env.snapshot(), "{} の {pos} 番目", d.name);
                assert_eq!(r.verified, PAD_SITES.len() - 1);
                assert!(
                    r.lines
                        .iter()
                        .any(|l| l.contains(d.name) && l.contains("不一致")),
                    "{:?}",
                    r.lines
                );
                assert_eq!(env.stopped.get(), 0);
                cases += 1;
            }
        }
        assert_eq!(cases, 12);
        let mut env = FakeCode::exe_like();
        env.mem.borrow_mut().remove(4);
        let r = install(&mut env, PAD_SITES, ENTRY);
        assert!(!r.complete());
        assert_eq!(env.writes.get(), 0);
        assert!(
            r.lines.iter().any(|l| l.contains("読めない")),
            "{:?}",
            r.lines
        );
    }

    #[test]
    fn install_reports_a_failed_write_and_still_tries_the_rest() {
        let mut env = FakeCode::exe_like();
        // 書けないアドレスは rel32 の位置（オペコードの次）。
        env.write_fail_at = Some(PAD_SITES[1].addr + 1);
        let r = install(&mut env, PAD_SITES, ENTRY);
        assert!(!r.complete());
        assert_eq!((r.written, r.call_rows), (3, 4));
        assert_eq!(
            env.bytes_at(PAD_SITES[1].addr),
            PAD_SITES[1].expected,
            "書けなかった行は元のまま"
        );
        assert!(
            r.lines.iter().any(|l| l.contains("設置 3/4 地点")),
            "{:?}",
            r.lines
        );
        assert!(
            r.lines.iter().any(|l| l.contains("頁の保護")),
            "{:?}",
            r.lines
        );
    }

    #[test]
    fn all_call_rows_share_the_original_target() {
        let targets: Vec<u32> = PAD_SITES
            .iter()
            .filter_map(|d| match d.kind {
                Use::CallRel32 { original_target } => Some(original_target),
                _ => None,
            })
            .collect();
        assert_eq!(targets, vec![PAD_READ_FN; 4]);
    }

    #[test]
    fn log_lines_show_the_route_counts_and_the_hat_flags() {
        let c = Counts {
            di: 0,
            mm: 123,
            added: 4,
            failed: 0,
        };
        let l = counts_line("終わり", &c, 7, [true, false]);
        for want in [
            "数え（終わり）",
            "winmm 123 回",
            "DirectInput 0 回",
            "足した回数 4",
            "0 番 あり",
            "1 番 なし",
            "読めなかった回数 0",
            "7 回",
        ] {
            assert!(l.contains(want), "{want} が {l} に無い");
        }
        let c = caps_line([Some(0x0013), None]);
        assert_eq!(c.lines().count(), 1);
        assert!(c.contains("0 番 wCaps = 0x0013、ハット（POV）あり"), "{c}");
        assert!(c.contains("1 番 つながっていない"), "{c}");
        assert!(caps_line([Some(0x0003), Some(0x0003)]).contains("ハット（POV）なし"));
        assert!(settings_line(PovMode::Off, "x").contains("切る"));
        assert!(settings_line(PovMode::Always, "x").contains("always"));
        assert!(settings_line(PovMode::Auto, "x").contains("ハットあり"));
    }
}
