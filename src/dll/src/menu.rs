//! メニューとライフの約 1 ms ごとの読み取りの、判定と行と間隔の集計（pure。unsafe 無し）。
//!
//! MainMenu `[0x4AC8C0]` は解放された後も 0 に戻らないので、入口ではたどらない。別のスレッドが約 1 ms ごとに、落ちない読み方
//! （自分のプロセスへの `ReadProcessMemory`。本物は `menu_win.rs`）で読み、変わったときだけ、読む前と読んだ後の番号（`stub::SEQ`）を
//! 添えて 1 行書く。`anim` は毎フレーム変わるので値だけ残す。ReplayManager のカウンタ（値だけ）・Live / Replay・Gui の会話の状態と、
//! その 2 つのポインタも読む。
//!
//! `seq_before` は読む直前に入口が取った番号の数（= 次に取られる番号）、`seq_after` は読んだ直後のそれ。2 つが同じなら読んでいる間に
//! 入口は通っていない。違えばその間に入口が通ったので、フレームの判定が ±1 になる。
//!
//! 一時停止（`PAUSE` のバイトと `PAUSE_MENU` の状態・フレームの数え）と終わりのメニュー（`MATCH_END_MENU` のバイトと `END_MENU`）も読む。
//! 戻り道（`higanr_core::return_flow`）が読む番地を、戻り道を動かさない実機の回で確かめる材料（照らす側は `higanr_core::return_obs`）。
//! フレームの数えは毎フレーム変わるので、開発用のビルドだけが変化の判定に入れる（`MenuWatch::new(true)`）。
#![forbid(unsafe_code)]

use std::io::Write;

use crate::th09_addr::{
    END_MENU, GUI_PTR, GUI_TALK, MAIN_MENU_PTR, MATCH_END_MENU, MENU_ANIM, MENU_CURSOR,
    MENU_OBJ_FRAMES, MENU_OBJ_STATE, MENU_PREV_SCREEN, MENU_SCREEN_ID, MENU_SUBSTATE,
    MENU_TRANSITIONING, P1_PLAYER_PTR, P2_PLAYER_PTR, PAUSE, PAUSE_MENU, PLAYER_LIFE,
    REPLAY_COUNTER, REPLAY_MANAGER_PTR, REPLAY_MODE,
};

pub trait MemRead {
    fn u32_at(&mut self, addr: u32) -> Option<u32>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuSample {
    pub menu_ptr: Option<u32>,
    pub screen_id: Option<u32>,
    pub substate: Option<u32>,
    pub cursor: Option<u32>,
    pub prev_screen: Option<u32>,
    pub transitioning: Option<u32>,
    pub anim: Option<u32>,
    pub p1_ptr: Option<u32>,
    pub p1_life: Option<u32>,
    pub p2_ptr: Option<u32>,
    pub p2_life: Option<u32>,
    pub replay_ptr: Option<u32>,
    pub replay_counter: Option<u32>,
    pub replay_mode: Option<u32>,
    pub gui_ptr: Option<u32>,
    pub gui_talk: Option<u32>,
    /// 一時停止のバイト（下位 1 バイト）と、一時停止のメニューの状態・フレームの数え。メニューが出ていない間の状態・数えは前の値のまま。
    pub pause_flag: Option<u32>,
    pub pause_state: Option<u32>,
    pub pause_frames: Option<u32>,
    /// マッチの終わりのメニューの印（下位 1 バイト）と、そのメニューの状態・フレームの数え。
    pub end_flag: Option<u32>,
    pub end_state: Option<u32>,
    pub end_frames: Option<u32>,
}

fn deref(m: &mut dyn MemRead, ptr: Option<u32>, off: u32) -> Option<u32> {
    let p = ptr.filter(|&p| p != 0)?;
    m.u32_at(p.checked_add(off)?)
}

/// 1 回の読み（ポインタは 1 回ずつ読み、その値から欄をたどる）。
pub fn read_sample(m: &mut dyn MemRead) -> MenuSample {
    let menu_ptr = m.u32_at(MAIN_MENU_PTR);
    let p1_ptr = m.u32_at(P1_PLAYER_PTR);
    let p2_ptr = m.u32_at(P2_PLAYER_PTR);
    let replay_ptr = m.u32_at(REPLAY_MANAGER_PTR);
    let gui_ptr = m.u32_at(GUI_PTR);
    MenuSample {
        menu_ptr,
        screen_id: deref(m, menu_ptr, MENU_SCREEN_ID),
        substate: deref(m, menu_ptr, MENU_SUBSTATE),
        cursor: deref(m, menu_ptr, MENU_CURSOR),
        prev_screen: deref(m, menu_ptr, MENU_PREV_SCREEN),
        transitioning: deref(m, menu_ptr, MENU_TRANSITIONING),
        anim: deref(m, menu_ptr, MENU_ANIM),
        p1_ptr,
        p1_life: deref(m, p1_ptr, PLAYER_LIFE),
        p2_ptr,
        p2_life: deref(m, p2_ptr, PLAYER_LIFE),
        replay_ptr,
        replay_counter: deref(m, replay_ptr, REPLAY_COUNTER),
        replay_mode: deref(m, replay_ptr, REPLAY_MODE),
        gui_ptr,
        gui_talk: deref(m, gui_ptr, GUI_TALK),
        // 旗は隣の欄を巻き込まないよう 1 バイトだけ使う。メニューの状態・数えは固定のアドレスで、ポインタをたどらない。
        pause_flag: m.u32_at(PAUSE).map(|v| v & 0xFF),
        pause_state: m.u32_at(PAUSE_MENU + MENU_OBJ_STATE),
        pause_frames: m.u32_at(PAUSE_MENU + MENU_OBJ_FRAMES),
        end_flag: m.u32_at(MATCH_END_MENU).map(|v| v & 0xFF),
        end_state: m.u32_at(END_MENU + MENU_OBJ_STATE),
        end_frames: m.u32_at(END_MENU + MENU_OBJ_FRAMES),
    }
}

impl MenuSample {
    /// 変化の判定に使う欄（5 欄とライフ 2 つ、Live / Replay と会話の状態、一時停止と終わりのメニューの旗と状態）。ポインタ・`anim`・
    /// ReplayManager のカウンタは入れない（ポインタが 0 になればその先の欄が「読めない」に変わるので行は出る）。メニューのフレームの数えは
    /// 毎フレーム変わるので、`per_frame` のときだけ入れる（メニューが開いている間は 1 フレームに 1 行になる）。
    fn key(&self, per_frame: bool) -> [Option<u32>; KEY_LEN] {
        let frames = |v: Option<u32>| if per_frame { v } else { None };
        [
            self.screen_id,
            self.substate,
            self.cursor,
            self.prev_screen,
            self.transitioning,
            self.p1_life,
            self.p2_life,
            self.replay_mode,
            self.gui_talk,
            self.pause_flag,
            self.pause_state,
            frames(self.pause_frames),
            self.end_flag,
            self.end_state,
            frames(self.end_frames),
        ]
    }
}

/// 変化の判定に使う欄の数（`MenuSample::key`）。
const KEY_LEN: usize = 15;

/// `_menu.csv` の見出し。照らす側（`return_obs::parse_menu_csv`）と同じ 1 つを使う。
pub use higanr_core::return_obs::MENU_CSV_HEADER;

fn dec(v: Option<u32>) -> String {
    v.map(|x| (x as i32).to_string()).unwrap_or_default()
}

fn hex(v: Option<u32>) -> String {
    v.map(|x| format!("0x{x:08X}")).unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct MenuWatch {
    prev: Option<[Option<u32>; KEY_LEN]>,
    per_frame: bool,
    pub samples: u64,
    pub lines: u64,
}

impl MenuWatch {
    /// `per_frame` が真なら、メニューのフレームの数えが変わるたびに行を出す（開発用のビルドだけ。配るバージョンは状態の変化だけ）。
    pub fn new(per_frame: bool) -> MenuWatch {
        MenuWatch {
            per_frame,
            ..MenuWatch::default()
        }
    }

    pub fn observe(
        &mut self,
        s: &MenuSample,
        seq_before: u32,
        seq_after: u32,
        t_us: u64,
    ) -> Option<String> {
        self.samples += 1;
        let key = s.key(self.per_frame);
        if self.prev == Some(key) {
            return None;
        }
        self.prev = Some(key);
        self.lines += 1;
        Some(format!(
            "{seq_before},{seq_after},{t_us},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            hex(s.menu_ptr),
            dec(s.screen_id),
            dec(s.substate),
            dec(s.cursor),
            dec(s.prev_screen),
            dec(s.transitioning),
            dec(s.anim),
            hex(s.p1_ptr),
            dec(s.p1_life),
            hex(s.p2_ptr),
            dec(s.p2_life),
            hex(s.replay_ptr),
            dec(s.replay_counter),
            dec(s.replay_mode),
            hex(s.gui_ptr),
            dec(s.gui_talk),
            dec(s.pause_flag),
            dec(s.pause_state),
            dec(s.pause_frames),
            dec(s.end_flag),
            dec(s.end_state),
            dec(s.end_frames),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerMode {
    HighResolution,
    Sleep1,
}

/// 間隔（前の読みから今の読みまで）の区分の境目（ミリ秒）。1 ms と `Sleep(1)` の既定の間隔（約 15.6 ms）の前後を細かく分ける。
pub const TICK_EDGES_MS: [f64; 10] = [0.5, 0.9, 1.1, 1.5, 2.0, 3.0, 5.0, 10.0, 15.0, 17.0];

/// 間隔の集計（道具の精度）。区分ごとの数と最小・最大・和だけ持つ（15 分で約 90 万回）。
#[derive(Debug, Clone, PartialEq)]
pub struct TickStats {
    pub mode: TimerMode,
    pub count: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub sum_us: u64,
    pub buckets: [u64; TICK_EDGES_MS.len() + 1],
    pub lines: u64,
    pub samples: u64,
    pub write_error: Option<String>,
}

impl TickStats {
    pub fn new(mode: TimerMode) -> Self {
        TickStats {
            mode,
            count: 0,
            min_us: u64::MAX,
            max_us: 0,
            sum_us: 0,
            buckets: [0; TICK_EDGES_MS.len() + 1],
            lines: 0,
            samples: 0,
            write_error: None,
        }
    }

    pub fn add(&mut self, us: u64) {
        self.count += 1;
        self.min_us = self.min_us.min(us);
        self.max_us = self.max_us.max(us);
        self.sum_us = self.sum_us.saturating_add(us);
        let ms = us as f64 / 1000.0;
        let k = TICK_EDGES_MS
            .iter()
            .position(|&e| ms < e)
            .unwrap_or(TICK_EDGES_MS.len());
        self.buckets[k] += 1;
    }

    pub fn write_summary(&self, out: &mut dyn Write, label: &str) {
        let mode = match self.mode {
            TimerMode::HighResolution => "高分解能の待機タイマー",
            TimerMode::Sleep1 => "Sleep(1)（高分解能の待機タイマーが取れなかった）",
        };
        let _ = writeln!(
            out,
            "メニューとライフ（{label}）: 読み {} 回、変化の行 {} 行（最初の 1 行を含む）。間隔は {mode}",
            self.samples, self.lines
        );
        if self.count == 0 {
            let _ = writeln!(
                out,
                "メニューとライフ（{label}）: 間隔はまだ測れていない（0 回）"
            );
        } else {
            let mut parts = Vec::new();
            for (k, n) in self.buckets.iter().enumerate() {
                let label = match k {
                    0 => format!("{} ms 未満", TICK_EDGES_MS[0]),
                    k if k == TICK_EDGES_MS.len() => format!("{} ms 以上", TICK_EDGES_MS[k - 1]),
                    k => format!("{}〜{} ms", TICK_EDGES_MS[k - 1], TICK_EDGES_MS[k]),
                };
                parts.push(format!("{label}: {n}"));
            }
            let _ = writeln!(
                out,
                "メニューとライフ（{label}）: 実際の間隔 {} 回、最小 {} µs / 平均 {:.1} µs / 最大 {} µs。区分 {}",
                self.count,
                self.min_us,
                self.sum_us as f64 / self.count as f64,
                self.max_us,
                parts.join(" / ")
            );
        }
        if let Some(e) = &self.write_error {
            let _ = writeln!(
                out,
                "メニューとライフ（{label}）: _menu.csv に書けなくなった（{e}）"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeMem(HashMap<u32, u32>);
    impl MemRead for FakeMem {
        fn u32_at(&mut self, addr: u32) -> Option<u32> {
            self.0.get(&addr).copied()
        }
    }

    const MENU: u32 = 0x0300_0000;
    const P1: u32 = 0x0310_0000;
    const P2: u32 = 0x0320_0000;
    const REPLAY: u32 = 0x0330_0000;
    const GUI: u32 = 0x0340_0000;

    fn mem() -> FakeMem {
        let mut m = FakeMem::default();
        for (a, v) in [
            (MAIN_MENU_PTR, MENU),
            (MENU + MENU_SCREEN_ID, 8),
            (MENU + MENU_SUBSTATE, 1),
            (MENU + MENU_CURSOR, 3),
            (MENU + MENU_PREV_SCREEN, 7),
            (MENU + MENU_TRANSITIONING, 0),
            (MENU + MENU_ANIM, 100),
            (P1_PLAYER_PTR, P1),
            (P1 + PLAYER_LIFE, 10),
            (P2_PLAYER_PTR, P2),
            (P2 + PLAYER_LIFE, 9),
            (REPLAY_MANAGER_PTR, REPLAY),
            (REPLAY + REPLAY_COUNTER, 40),
            (REPLAY + REPLAY_MODE, 1),
            (GUI_PTR, GUI),
            (GUI + GUI_TALK, u32::MAX - 1), // −2（会話中）
            // 旗は下位 1 バイトだけ使う（上位の欄は無視される）。
            (PAUSE, 0x7F00_0001),
            (PAUSE_MENU + MENU_OBJ_STATE, 2),
            (PAUSE_MENU + MENU_OBJ_FRAMES, 9),
            (MATCH_END_MENU, 0x1234_5600),
            (END_MENU + MENU_OBJ_STATE, 3),
            (END_MENU + MENU_OBJ_FRAMES, 40),
        ] {
            m.0.insert(a, v);
        }
        m
    }

    #[test]
    fn sample_follows_the_pointers() {
        let mut m = mem();
        let s = read_sample(&mut m);
        assert_eq!(
            (
                s.screen_id,
                s.substate,
                s.cursor,
                s.prev_screen,
                s.transitioning,
                s.anim
            ),
            (Some(8), Some(1), Some(3), Some(7), Some(0), Some(100))
        );
        assert_eq!((s.p1_life, s.p2_life), (Some(10), Some(9)));
        assert_eq!(
            (s.replay_counter, s.replay_mode, s.gui_talk),
            (Some(40), Some(1), Some(u32::MAX - 1))
        );
        m.0.insert(P2_PLAYER_PTR, 0);
        m.0.insert(PLAYER_LIFE, 77);
        m.0.remove(&GUI_PTR);
        let s = read_sample(&mut m);
        assert_eq!((s.p2_ptr, s.p2_life), (Some(0), None));
        assert_eq!((s.gui_ptr, s.gui_talk), (None, None));
    }

    /// 壊れたら落ちる: 一時停止・終わりのメニューの旗が 4 バイト丸ごと読まれる（隣の欄が混ざる）、状態とフレームの数えの番地が入れ替わる。
    #[test]
    fn pause_and_end_menu_are_read_from_their_own_addresses() {
        let mut m = mem();
        let s = read_sample(&mut m);
        assert_eq!(
            (s.pause_flag, s.pause_state, s.pause_frames),
            (Some(1), Some(2), Some(9))
        );
        assert_eq!(
            (s.end_flag, s.end_state, s.end_frames),
            (Some(0), Some(3), Some(40))
        );
        m.0.remove(&PAUSE);
        m.0.remove(&(END_MENU + MENU_OBJ_FRAMES));
        let s = read_sample(&mut m);
        assert_eq!((s.pause_flag, s.end_frames), (None, None));
    }

    #[test]
    fn only_judged_fields_make_lines() {
        let mut m = mem();
        let mut w = MenuWatch::default();
        let s0 = read_sample(&mut m);
        let first = w.observe(&s0, 5, 5, 0).expect("最初の読みは 1 行");
        assert_eq!(
            first,
            format!(
                "5,5,0,0x{MENU:08X},8,1,3,7,0,100,0x{P1:08X},10,0x{P2:08X},9,0x{REPLAY:08X},40,1,0x{GUI:08X},-2,1,2,9,0,3,40"
            )
        );
        assert_eq!(
            first.split(',').count(),
            MENU_CSV_HEADER.split(',').count(),
            "行の欄の数と見出しが違う"
        );
        m.0.insert(MENU + MENU_ANIM, 101);
        m.0.insert(REPLAY + REPLAY_COUNTER, 41);
        assert_eq!(w.observe(&read_sample(&mut m), 5, 6, 1000), None);
        m.0.insert(REPLAY_MANAGER_PTR, REPLAY + 0x1000);
        m.0.insert(REPLAY + 0x1000 + REPLAY_MODE, 1);
        m.0.insert(REPLAY + 0x1000 + REPLAY_COUNTER, 3);
        assert_eq!(w.observe(&read_sample(&mut m), 6, 6, 2000), None);
        type Change = fn(&mut FakeMem);
        let changes: [(&str, Change); 13] = [
            ("screen_id", |m| {
                m.0.insert(MENU + MENU_SCREEN_ID, 16);
            }),
            ("substate", |m| {
                m.0.insert(MENU + MENU_SUBSTATE, 0);
            }),
            ("cursor", |m| {
                m.0.insert(MENU + MENU_CURSOR, 4);
            }),
            ("prev_screen", |m| {
                m.0.insert(MENU + MENU_PREV_SCREEN, 8);
            }),
            ("transitioning", |m| {
                m.0.insert(MENU + MENU_TRANSITIONING, 1);
            }),
            ("p1_life", |m| {
                m.0.insert(P1 + PLAYER_LIFE, 8);
            }),
            ("p2_life", |m| {
                m.0.remove(&(P2 + PLAYER_LIFE));
            }),
            ("replay_mode", |m| {
                m.0.insert(REPLAY + 0x1000 + REPLAY_MODE, 0);
            }),
            ("gui_talk", |m| {
                m.0.insert(GUI + GUI_TALK, 0x10);
            }),
            ("pause_flag", |m| {
                m.0.insert(PAUSE, 0x7F00_0000);
            }),
            ("pause_state", |m| {
                m.0.insert(PAUSE_MENU + MENU_OBJ_STATE, 3);
            }),
            ("end_flag", |m| {
                m.0.insert(MATCH_END_MENU, 0x1234_5601);
            }),
            ("end_state", |m| {
                m.0.insert(END_MENU + MENU_OBJ_STATE, 1);
            }),
        ];
        for (name, change) in changes {
            change(&mut m);
            let line = w.observe(&read_sample(&mut m), 7, 7, 3000);
            assert!(line.is_some(), "{name} の変化で行が出ない");
            assert_eq!(w.observe(&read_sample(&mut m), 7, 7, 3001), None, "{name}");
        }
        assert_eq!(w.lines, 14);
        assert_eq!(w.samples, 29);
    }

    /// 壊れたら落ちる: DLL が書く `_menu.csv` の見出しと行を、照らす側（`higanr_core::return_obs`）が読めなくなる（列の名前・数の食い違い）。
    #[test]
    fn a_row_written_here_is_read_by_the_checker() {
        let mut m = mem();
        let mut w = MenuWatch::new(true);
        let line = w.observe(&read_sample(&mut m), 7, 8, 0).unwrap();
        let rows = higanr_core::return_obs::parse_menu_csv(&format!("{MENU_CSV_HEADER}\n{line}\n"))
            .unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(
            (r.seq_before, r.seq_after, r.screen_id, r.cursor),
            (7, 8, Some(8), Some(3))
        );
        // 一時停止の旗が立ち、終わりのメニューの印は下りている。
        let report = higanr_core::return_obs::judge(&rows, &Default::default());
        assert_eq!(report.episodes, (1, 0));
    }

    /// 壊れたら落ちる: フレームの数えが変わるだけで配るバージョンの行が増える（ログが膨らむ）、開発用のビルドで 1 フレームごとの行が出ない
    /// （戻り道の照らしが「前の行のフレーム数」を引けない）。旗の上位バイトだけの変化は、どちらでも行にならない。
    #[test]
    fn frame_counts_make_lines_only_when_asked() {
        let bump = |m: &mut FakeMem| {
            m.0.insert(PAUSE_MENU + MENU_OBJ_FRAMES, 10);
            m.0.insert(END_MENU + MENU_OBJ_FRAMES, 41);
        };
        for (per_frame, want) in [(false, None), (true, Some(()))] {
            let mut m = mem();
            let mut w = MenuWatch::new(per_frame);
            assert!(w.observe(&read_sample(&mut m), 1, 1, 0).is_some());
            bump(&mut m);
            let got = w.observe(&read_sample(&mut m), 2, 2, 16_000).map(|_| ());
            assert_eq!(got, want, "per_frame = {per_frame}");
            m.0.insert(PAUSE, 0x0100_0001);
            let flag_high_byte = w.observe(&read_sample(&mut m), 3, 3, 33_000);
            assert_eq!(flag_high_byte, None, "旗の上位バイトで行が出た");
        }
    }

    #[test]
    fn tick_stats_buckets_and_summary() {
        let mut t = TickStats::new(TimerMode::HighResolution);
        let mut out = Vec::new();
        t.write_summary(&mut out, "途中");
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("間隔はまだ測れていない（0 回）"), "{log}");
        for us in [400, 1000, 1099, 1100, 15_600, 20_000] {
            t.add(us);
        }
        assert_eq!(t.buckets, [1, 0, 2, 1, 0, 0, 0, 0, 0, 1, 1]);
        assert_eq!((t.count, t.min_us, t.max_us), (6, 400, 20_000));
        t.samples = 6;
        t.lines = 2;
        let mut out = Vec::new();
        t.write_summary(&mut out, "終わり");
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("読み 6 回、変化の行 2 行"), "{log}");
        assert!(log.contains("高分解能の待機タイマー"), "{log}");
        assert!(
            log.contains("実際の間隔 6 回、最小 400 µs / 平均 6533.2 µs / 最大 20000 µs"),
            "{log}"
        );
        assert!(log.contains("0.9〜1.1 ms: 2"), "{log}");
        assert!(log.contains("17 ms 以上: 1"), "{log}");
        let s = TickStats::new(TimerMode::Sleep1);
        let mut out = Vec::new();
        s.write_summary(&mut out, "x");
        assert!(String::from_utf8(out).unwrap().contains("Sleep(1)"));
    }
}
