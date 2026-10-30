//! ゲームのメニューから保存したリプレイへ印を足す判断（pure。unsafe無し）。
//!
//! ゲームのスレッドは保存関数が戻った後に`SaveSignal::record`でパス・始めの時刻・`game_mode`を書くだけ。ファイルには触らない。
//! 別のスレッドが`SaveSignal::take`で拾う。
//! `handle`がパスの形・作業フォルダ・更新時刻・既存の印をチェックしてから、そのファイルの末尾に1つだけ足す（`MarkIo`の向こうが本物のファイル。`menu_save_win.rs`）。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering, fence};

use higanr_core::replay::{self, AppendError, MarkRead, MenuMark};
use higanr_core::replay_menu_path::{parse_save_path, written_since};

use crate::replay_save::STAGES;

/// 保存の通知に載せるパスの長さの上限（ゲームが`CreateFileA`に渡す相対パスは`MAX_PATH`未満）。
pub const PATH_MAX: usize = 259;
/// 印を足すスレッドが保存の通知を見に行く間隔（ミリ秒）。
pub const STEP_MS: u64 = 200;
/// 本ログの各行の頭。
pub const LOG_LABEL: &str = "メニュー保存";

/// 保存の通知の中で「チャンクが読めなかった」を表す値（チャンクの合計は 10面 × `CHAIN_LIMIT` までなのでこれには届かない）。
const CHUNKS_UNREAD: u32 = u32::MAX;

/// ゲームのスレッド（書き手1人）から印を足すスレッド（読み手1人）への通知。
/// `seq`が偶数のときだけ中身が確定していて、書いている間は奇数になる。
/// 読み手は前後で`seq`が同じことをチェックし、書き換えの途中を読んだら捨てる。
/// ロックも確保も無い。
pub struct SaveSignal {
    seq: AtomicU32,
    len: AtomicU32,
    started_ms: AtomicU64,
    game_mode: AtomicU32,
    leftover_stages: AtomicU32,
    real_stages: AtomicU32,
    placed: AtomicU32,
    chunks: AtomicU32,
    est: AtomicU32,
    need: AtomicU32,
    path: [AtomicU8; PATH_MAX],
}

/// 保存の直し（見積もりの直し）が効いたかを示す値。ゲームのスレッドで数え（確保しない）、本ログの行に添える。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaveDiag {
    /// トランポリンが本当の印を書く前にその場所に残っていた10ワードのうち0でない数。素の花映塚はこの場所の0でない面で見積もる。
    pub leftover_stages: u32,
    /// 本当の印（ヘッダの10ワード）のうち0でない数。
    pub real_stages: u32,
    /// 書いた後に読み戻した10ワードが本当の印と同じかどうか。
    pub placed: bool,
    /// 本当の印が0でない面のチャンクの数の合計（見積もりに入るチャンク）。連鎖が読めない・長すぎるときは`None`（チャンク0個として見積もる）。
    pub chunks: Option<u32>,
    /// トランポリンが計算した見積もり。
    pub est: u32,
    /// `replay_save::need_bound`の要る量の上限。
    pub need: u32,
}

impl SaveDiag {
    /// 本ログの行に添える文。
    pub fn text(&self) -> String {
        format!(
            "保存の直し: 書く前の 10 語の 0 でない面 {}/{STAGES}・本当の印の 0 でない面 {}/{STAGES}・置いた 10 語は本当の印と{}・{}・見積もり 0x{:X}・要る量の上限 0x{:X}",
            self.leftover_stages,
            self.real_stages,
            if self.placed { "一致" } else { "不一致" },
            match self.chunks {
                Some(n) => format!("見積もりに入る塊 {n} 個"),
                None => "塊は読めなかった（塊 0 個として見積もった）".to_string(),
            },
            self.est,
            self.need,
        )
    }
}

/// 拾った通知の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    /// 保存関数に渡されたパス（ANSI、NULなし）。長すぎたときは空で`too_long`。
    pub path: Vec<u8>,
    pub too_long: bool,
    /// 保存関数を呼ぶ前の時刻（UNIX時刻のミリ秒）。
    pub started_ms: u64,
    pub game_mode: u32,
    pub diag: SaveDiag,
}

impl SaveSignal {
    pub const fn new() -> Self {
        SaveSignal {
            seq: AtomicU32::new(0),
            len: AtomicU32::new(0),
            started_ms: AtomicU64::new(0),
            game_mode: AtomicU32::new(0),
            leftover_stages: AtomicU32::new(0),
            real_stages: AtomicU32::new(0),
            placed: AtomicU32::new(0),
            chunks: AtomicU32::new(0),
            est: AtomicU32::new(0),
            need: AtomicU32::new(0),
            path: [const { AtomicU8::new(0) }; PATH_MAX],
        }
    }

    /// 通知を書く（書き手はゲームのスレッド1人だけ。確保しない・待たない）。`path`が`PATH_MAX`を超えるときは長さの印だけを書く。
    pub fn record(&self, path: &[u8], started_ms: u64, game_mode: u32, diag: SaveDiag) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1) | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let too_long = path.len() > PATH_MAX;
        let n = if too_long { 0 } else { path.len() };
        for (slot, b) in self.path.iter().zip(&path[..n]) {
            slot.store(*b, Ordering::Relaxed);
        }
        // 長すぎは`PATH_MAX + 1`で表す（0 〜 `PATH_MAX`は長さそのもの）。
        let len = if too_long { PATH_MAX + 1 } else { n };
        self.len.store(len as u32, Ordering::Relaxed);
        self.started_ms.store(started_ms, Ordering::Relaxed);
        self.game_mode.store(game_mode, Ordering::Relaxed);
        self.leftover_stages
            .store(diag.leftover_stages, Ordering::Relaxed);
        self.real_stages.store(diag.real_stages, Ordering::Relaxed);
        self.placed.store(u32::from(diag.placed), Ordering::Relaxed);
        self.chunks
            .store(diag.chunks.unwrap_or(CHUNKS_UNREAD), Ordering::Relaxed);
        self.est.store(diag.est, Ordering::Relaxed);
        self.need.store(diag.need, Ordering::Relaxed);
        self.seq
            .store(s.wrapping_add(1).wrapping_add(1) & !1, Ordering::Release);
    }

    /// 新しい通知があれば拾う。`last`は前に拾った`seq`（最初は0）。書き換えの途中・読んでいる間の書き換えは`None`（次の周期に拾い直す）。
    pub fn take(&self, last: &mut u32) -> Option<Taken> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 == *last || s1 & 1 == 1 {
            return None;
        }
        let len = self.len.load(Ordering::Relaxed) as usize;
        let too_long = len > PATH_MAX;
        let n = if too_long { 0 } else { len };
        let path: Vec<u8> = self.path[..n]
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        let started_ms = self.started_ms.load(Ordering::Relaxed);
        let game_mode = self.game_mode.load(Ordering::Relaxed);
        let diag = SaveDiag {
            leftover_stages: self.leftover_stages.load(Ordering::Relaxed),
            real_stages: self.real_stages.load(Ordering::Relaxed),
            placed: self.placed.load(Ordering::Relaxed) != 0,
            chunks: Some(self.chunks.load(Ordering::Relaxed)).filter(|&n| n != CHUNKS_UNREAD),
            est: self.est.load(Ordering::Relaxed),
            need: self.need.load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        if self.seq.load(Ordering::Relaxed) != s1 {
            return None;
        }
        *last = s1;
        Some(Taken {
            path,
            too_long,
            started_ms,
            game_mode,
            diag,
        })
    }
}

impl Default for SaveSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// 足す相手のファイル1つ（開いたままチェックと読み書きに使う）。
pub trait MarkFile {
    /// このファイルがexeのフォルダの下の同じ相対パスのファイルと同じものかどうか（作業フォルダがexeのフォルダであることのチェック）。
    fn same_as_exe_dir(&mut self) -> Result<bool, String>;
    /// 更新時刻（UNIX時刻のミリ秒）。
    fn modified_ms(&mut self) -> Result<u64, String>;
    /// 先頭から最後まで。
    fn read_all(&mut self) -> Result<Vec<u8>, String>;
    /// 末尾へ足す（前の部分は書き換えない）。
    fn append(&mut self, bytes: &[u8]) -> Result<(), String>;
}

pub trait MarkIo {
    /// exeのフォルダからの相対パス`rel`（`/`区切り）を、ゲームが開いたのと同じ作業フォルダ基準で読みと追記の権利で開く。
    fn open(&mut self, rel: &str) -> Result<Box<dyn MarkFile>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Marked {
        rel: String,
        before: usize,
        added: usize,
        game_mode: u32,
    },
    /// 足さなかった。`problem`は意図しない理由（既に印があるだけなら偽）。
    Skipped {
        rel: Option<String>,
        why: String,
        problem: bool,
    },
}

impl Outcome {
    /// 簡潔なログにも書く行かどうか（足した・意図しない理由で足さなかった）。既に印があるだけなら詳細だけ。
    pub fn in_brief_log(&self) -> bool {
        !matches!(self, Outcome::Skipped { problem: false, .. })
    }

    /// 本ログの1行。
    pub fn line(&self) -> String {
        match self {
            Outcome::Marked {
                rel,
                before,
                added,
                game_mode,
            } => format!(
                "{LOG_LABEL}: 印を足した（{rel}、{before} バイトの末尾に {added} バイト。game_mode = {game_mode}）"
            ),
            Outcome::Skipped { rel, why, .. } => match rel {
                Some(r) => format!("{LOG_LABEL}: 印を足さない（{why}）: {r}"),
                None => format!("{LOG_LABEL}: 印を足さない（{why}）"),
            },
        }
    }
}

fn skipped(rel: &str, why: impl Into<String>, problem: bool) -> Outcome {
    Outcome::Skipped {
        rel: Some(rel.to_string()),
        why: why.into(),
        problem,
    }
}

/// パスの先頭60バイトまでを読める形にする（形の違うパスをログに出すとき用）。
fn shown(path: &[u8]) -> String {
    let cut = &path[..path.len().min(60)];
    format!("{:?}", String::from_utf8_lossy(cut))
}

/// 拾った通知1つについて印を足してよいかをチェックして足す。
/// 足す条件（全部満たしたときだけ）: パスが2つの形のどちらか／作業フォルダがexeのフォルダ／更新時刻が今回の保存の始め以降／平文が読めて既に印が無い。
/// 足したら読み戻して、前の部分のバイト一致・末尾の一致・印が`Menu`で読めることをチェックする。
pub fn handle(t: &Taken, build: u32, ver: &str, io: &mut dyn MarkIo) -> Outcome {
    if t.too_long {
        return Outcome::Skipped {
            rel: None,
            why: format!("パスが {PATH_MAX} バイトを超えました"),
            problem: true,
        };
    }
    let Some(kind) = parse_save_path(&t.path) else {
        return Outcome::Skipped {
            rel: None,
            why: format!(
                "保存のパスが決まった 2 つの形（replay/th9_NN.rpy・./replay/th9_udnNNN.rpy）ではありません: {}",
                shown(&t.path)
            ),
            problem: true,
        };
    };
    let rel = kind.relative();
    let mut file = match io.open(&rel) {
        Ok(f) => f,
        Err(e) => return skipped(&rel, format!("開けません: {e}"), true),
    };
    match file.same_as_exe_dir() {
        Ok(true) => {}
        Ok(false) => {
            return skipped(
                &rel,
                "作業フォルダが exe のフォルダではありません（exe のフォルダ側のファイルと別のファイル）",
                true,
            );
        }
        Err(e) => return skipped(&rel, format!("作業フォルダのチェック不能: {e}"), true),
    }
    match file.modified_ms() {
        Ok(m) if written_since(m, t.started_ms) => {}
        Ok(_) => {
            return skipped(
                &rel,
                "更新時刻が今回の保存より前です（ゲームが書けなかった古いファイルと見なします）",
                true,
            );
        }
        Err(e) => return skipped(&rel, format!("更新時刻が読み取り不能: {e}"), true),
    }
    let before = match file.read_all() {
        Ok(b) => b,
        Err(e) => return skipped(&rel, format!("読み込み不能: {e}"), true),
    };
    let mark = MenuMark {
        ver: ver.to_string(),
        build,
    };
    let chunk = match replay::chunk_to_append_menu(&before, &mark) {
        Ok(c) => c,
        Err(AppendError::AlreadyMarked) => {
            return skipped(&rel, "既に HGRT の印があります", false);
        }
        Err(e) => return skipped(&rel, format!("{e:?}"), true),
    };
    if let Err(e) = file.append(&chunk) {
        return skipped(&rel, format!("書き込み不能: {e}"), true);
    }
    let back = match file.read_all() {
        Ok(b) => b,
        Err(e) => return skipped(&rel, format!("読み戻し不能: {e}"), true),
    };
    let intact = back.len() == before.len() + chunk.len()
        && back[..before.len()] == before[..]
        && back[before.len()..] == chunk[..]
        && matches!(replay::read_mark(&back), Ok(MarkRead::Menu(m)) if m == mark);
    if !intact {
        return skipped(&rel, "読み戻した内容が追記の前後と不一致", true);
    }
    Outcome::Marked {
        rel,
        before: before.len(),
        added: chunk.len(),
        game_mode: t.game_mode,
    }
}

/// 印を足すスレッドの1周期ぶん: 通知があれば`handle`で処理し、簡潔なログにも書くかどうかと本ログの行（保存の直しの値を添える）を返す。
pub fn step(
    signal: &SaveSignal,
    last: &mut u32,
    build: u32,
    ver: &str,
    io: &mut dyn MarkIo,
) -> Option<(bool, String)> {
    let t = signal.take(last)?;
    let o = handle(&t, build, ver, io);
    Some((
        o.in_brief_log(),
        format!("{} ／ {}", o.line(), t.diag.text()),
    ))
}

/// 合成のリプレイ（`T9RP`・`comp_size`・ゲームと同じ形のUSER 1つ）。
#[cfg(test)]
pub(crate) fn test_replay() -> Vec<u8> {
    let comp_size = 0xC0u32;
    let mut f: Vec<u8> = (0..comp_size).map(|i| (i * 7 + 3) as u8).collect();
    f[0..4].copy_from_slice(b"T9RP");
    f[12..16].copy_from_slice(&comp_size.to_le_bytes());
    let text = b"NO NAME\r\n";
    let total = (text.len() + 13 + 1) & !1;
    f.extend_from_slice(b"USER");
    f.extend_from_slice(&(total as u32).to_le_bytes());
    f.extend_from_slice(&0x7FFF_FF00u32.to_le_bytes());
    f.extend_from_slice(text);
    f.push(0);
    f.resize(comp_size as usize + total, 0);
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use higanr_core::replay::{Mark, MarkRole, MatchResult, Side, Timestamp};
    use higanr_core::replay_menu_path::MTIME_SLACK_MS;

    #[derive(Default)]
    struct Disk {
        bytes: Vec<u8>,
        mtime_ms: u64,
        same_dir: Option<bool>,
        open_err: Option<String>,
        append_err: bool,
        corrupt_on_append: bool,
        opened: Vec<String>,
        appended: usize,
    }

    struct FakeIo(Rc<RefCell<Disk>>);
    struct FakeFile(Rc<RefCell<Disk>>);

    impl MarkIo for FakeIo {
        fn open(&mut self, rel: &str) -> Result<Box<dyn MarkFile>, String> {
            let mut d = self.0.borrow_mut();
            d.opened.push(rel.to_string());
            if let Some(e) = &d.open_err {
                return Err(e.clone());
            }
            drop(d);
            Ok(Box::new(FakeFile(self.0.clone())))
        }
    }

    impl MarkFile for FakeFile {
        fn same_as_exe_dir(&mut self) -> Result<bool, String> {
            self.0
                .borrow()
                .same_dir
                .ok_or_else(|| "確かめられない".to_string())
        }
        fn modified_ms(&mut self) -> Result<u64, String> {
            Ok(self.0.borrow().mtime_ms)
        }
        fn read_all(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.0.borrow().bytes.clone())
        }
        fn append(&mut self, bytes: &[u8]) -> Result<(), String> {
            let mut d = self.0.borrow_mut();
            if d.append_err {
                return Err("拒まれた".to_string());
            }
            d.appended += 1;
            d.bytes.extend_from_slice(bytes);
            if d.corrupt_on_append {
                d.bytes[3] ^= 0xFF;
            }
            Ok(())
        }
    }

    const START: u64 = 1_790_000_000_000;

    fn taken(path: &[u8]) -> Taken {
        Taken {
            path: path.to_vec(),
            too_long: false,
            started_ms: START,
            game_mode: 2,
            diag: SaveDiag::default(),
        }
    }

    fn disk(bytes: Vec<u8>) -> Rc<RefCell<Disk>> {
        Rc::new(RefCell::new(Disk {
            bytes,
            mtime_ms: START + 300,
            same_dir: Some(true),
            ..Default::default()
        }))
    }

    fn run(d: &Rc<RefCell<Disk>>, path: &[u8]) -> Outcome {
        handle(&taken(path), 411_272, "7.8.9", &mut FakeIo(d.clone()))
    }

    #[test]
    fn a_fresh_unmarked_file_gets_the_menu_mark_and_nothing_else_changes() {
        let orig = test_replay();
        let d = disk(orig.clone());
        let o = run(&d, b"replay/th9_05.rpy");
        let Outcome::Marked {
            rel,
            before,
            added,
            game_mode,
        } = o
        else {
            panic!("{o:?}");
        };
        assert_eq!(rel, "replay/th9_05.rpy");
        assert_eq!((before, game_mode), (orig.len(), 2));
        let after = d.borrow().bytes.clone();
        assert_eq!(after.len(), orig.len() + added);
        assert_eq!(&after[..orig.len()], &orig[..], "足す前の部分はバイト一致");
        assert_eq!(
            replay::read_mark(&after).unwrap(),
            MarkRead::Menu(MenuMark {
                ver: "7.8.9".into(),
                build: 411_272
            })
        );
        assert_eq!(d.borrow().opened, vec!["replay/th9_05.rpy".to_string()]);
        // ゲームの自動保存の形（`./`つき）も同じ相対パスで開く。
        let d2 = disk(orig);
        let Outcome::Marked { rel, .. } = run(&d2, b"./replay/th9_udn012.rpy") else {
            panic!("udn");
        };
        assert_eq!(rel, "replay/th9_udn012.rpy");
        assert_eq!(
            d2.borrow().opened,
            vec!["replay/th9_udn012.rpy".to_string()]
        );
    }

    #[test]
    fn a_path_of_another_shape_is_refused_before_any_file_is_opened() {
        let d = disk(test_replay());
        for bad in [
            &b"../replay/th9_01.rpy"[..],
            b"C:/replay/th9_01.rpy",
            b"replay/th9_26.rpy",
            b"other.rpy",
            b"",
        ] {
            let o = run(&d, bad);
            assert!(
                matches!(
                    o,
                    Outcome::Skipped {
                        rel: None,
                        problem: true,
                        ..
                    }
                ),
                "{o:?}"
            );
        }
        let long = Taken {
            too_long: true,
            ..taken(b"")
        };
        assert!(matches!(
            handle(&long, 0, "1.1.1", &mut FakeIo(d.clone())),
            Outcome::Skipped { problem: true, .. }
        ));
        assert!(d.borrow().opened.is_empty());
        assert_eq!(d.borrow().appended, 0);
    }

    #[test]
    fn a_different_working_folder_or_an_unverifiable_one_adds_nothing() {
        let orig = test_replay();
        for same in [Some(false), None] {
            let d = disk(orig.clone());
            d.borrow_mut().same_dir = same;
            let o = run(&d, b"replay/th9_01.rpy");
            assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
            assert_eq!(d.borrow().bytes, orig);
        }
    }

    #[test]
    fn an_old_file_left_by_a_failed_save_gets_no_mark() {
        let orig = test_replay();
        let d = disk(orig.clone());
        d.borrow_mut().mtime_ms = START - MTIME_SLACK_MS - 1;
        let o = run(&d, b"replay/th9_01.rpy");
        assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
        assert_eq!(d.borrow().bytes, orig);
        // 余裕の内側ならファイルシステムの時刻の粒度として通す。
        d.borrow_mut().mtime_ms = START - MTIME_SLACK_MS;
        assert!(matches!(
            run(&d, b"replay/th9_01.rpy"),
            Outcome::Marked { .. }
        ));
    }

    #[test]
    fn marked_broken_and_unopenable_files_are_left_alone() {
        let orig = test_replay();
        // ネット対戦の印が既にある。
        let mut marked = orig.clone();
        let m = Mark {
            peer: "a".into(),
            self_name: "b".into(),
            side: Side::P1,
            lat: 0,
            ping_ms: 0,
            result: MatchResult::Win,
            at: Timestamp {
                year: 2026,
                month: 10,
                day: 19,
                hour: 1,
                minute: 2,
                second: 3,
            },
            role: MarkRole::Play,
            detail: None,
        };
        marked.extend(replay::chunk_to_append(&orig, &m).unwrap());
        let d = disk(marked.clone());
        let o = run(&d, b"replay/th9_01.rpy");
        assert!(
            matches!(o, Outcome::Skipped { problem: false, .. }),
            "既に印があるのは問題ではない: {o:?}"
        );
        assert_eq!(d.borrow().bytes, marked);
        // 壊れている（切れている）。
        let cut = orig[..orig.len() - 1].to_vec();
        let d = disk(cut.clone());
        assert!(matches!(
            run(&d, b"replay/th9_01.rpy"),
            Outcome::Skipped { problem: true, .. }
        ));
        assert_eq!(d.borrow().bytes, cut);
        // 開けない。
        let d = disk(orig.clone());
        d.borrow_mut().open_err = Some("存在しない".into());
        assert!(matches!(
            run(&d, b"replay/th9_01.rpy"),
            Outcome::Skipped { problem: true, .. }
        ));
        // 書き込めない。
        let d = disk(orig.clone());
        d.borrow_mut().append_err = true;
        assert!(matches!(
            run(&d, b"replay/th9_01.rpy"),
            Outcome::Skipped { problem: true, .. }
        ));
        assert_eq!(d.borrow().bytes, orig);
    }

    #[test]
    fn a_readback_that_changed_the_earlier_bytes_is_reported() {
        let d = disk(test_replay());
        d.borrow_mut().corrupt_on_append = true;
        let o = run(&d, b"replay/th9_01.rpy");
        assert!(matches!(o, Outcome::Skipped { problem: true, .. }), "{o:?}");
    }

    #[test]
    fn thprac_user_chunk_before_the_mark_does_not_stop_it() {
        let mut f = test_replay();
        let text = b"THPRAC body";
        let total = (12 + text.len() + 1 + 1) & !1;
        f.extend_from_slice(b"USER");
        f.extend_from_slice(&(total as u32).to_le_bytes());
        f.extend_from_slice(b"PRAC");
        f.extend_from_slice(text);
        f.resize(f.len() + (total - 12 - text.len()), 0);
        let d = disk(f.clone());
        let o = run(&d, b"replay/th9_01.rpy");
        assert!(matches!(o, Outcome::Marked { .. }), "{o:?}");
        assert_eq!(&d.borrow().bytes[..f.len()], &f[..]);
    }

    #[test]
    fn the_signal_carries_the_path_once_and_a_second_take_sees_nothing() {
        let s = SaveSignal::new();
        let mut last = 0;
        assert_eq!(s.take(&mut last), None, "合図が無い");
        s.record(b"replay/th9_07.rpy", START, 1, SaveDiag::default());
        let t = s.take(&mut last).unwrap();
        assert_eq!(t.path, b"replay/th9_07.rpy");
        assert_eq!((t.started_ms, t.game_mode, t.too_long), (START, 1, false));
        assert_eq!(s.take(&mut last), None, "同じ合図を 2 回拾わない");
        s.record(
            b"./replay/th9_udn001.rpy",
            START + 1,
            2,
            SaveDiag::default(),
        );
        let t2 = s.take(&mut last).unwrap();
        assert_eq!(t2.path, b"./replay/th9_udn001.rpy");
        assert_eq!(t2.started_ms, START + 1);
        // 前より短いパスでは前の残りを拾わない。
        s.record(b"a", START, 0, SaveDiag::default());
        assert_eq!(s.take(&mut last).unwrap().path, b"a");
        // 長すぎるパス。
        s.record(&[b'x'; PATH_MAX + 1], START, 0, SaveDiag::default());
        let t3 = s.take(&mut last).unwrap();
        assert!(t3.too_long && t3.path.is_empty());
        s.record(&[b'x'; PATH_MAX], START, 0, SaveDiag::default());
        let t4 = s.take(&mut last).unwrap();
        assert!(!t4.too_long && t4.path.len() == PATH_MAX);
    }

    #[test]
    fn a_signal_being_written_is_not_taken() {
        let s = SaveSignal::new();
        let mut last = 0;
        // 書いている間（seqが奇数）を作る。
        s.seq.store(1, Ordering::Relaxed);
        assert_eq!(s.take(&mut last), None);
        assert_eq!(last, 0, "拾えなかったので前の番号は進めない");
        s.seq.store(2, Ordering::Relaxed);
        assert!(s.take(&mut last).is_some());
    }

    #[test]
    fn step_turns_a_signal_into_one_log_line() {
        let orig = test_replay();
        let d = disk(orig);
        let s = SaveSignal::new();
        let mut last = 0;
        let mut io = FakeIo(d.clone());
        assert_eq!(step(&s, &mut last, 1, "1.1.1", &mut io), None);
        s.record(b"replay/th9_03.rpy", START, 2, sample_diag());
        let (brief, line) = step(&s, &mut last, 1, "1.1.1", &mut io).unwrap();
        assert!(brief, "足した行は簡潔なログにも書く");
        assert!(
            line.starts_with("メニュー保存: 印を足した（replay/th9_03.rpy"),
            "{line}"
        );
        assert_eq!(step(&s, &mut last, 1, "1.1.1", &mut io), None);
    }

    fn sample_diag() -> SaveDiag {
        SaveDiag {
            leftover_stages: 3,
            real_stages: 7,
            placed: true,
            chunks: Some(9),
            est: 0x1_01EC + 7 * 0x60 + 9 * 0x54D9,
            need: 0x1EC + 7 * 0x60 + 9 * 0x54D9,
        }
    }

    #[test]
    fn the_save_diag_travels_with_the_signal_into_the_log_line() {
        let s = SaveSignal::new();
        let mut last = 0;
        s.record(b"replay/th9_03.rpy", START, 2, sample_diag());
        assert_eq!(s.take(&mut last).unwrap().diag, sample_diag());
        for placed in [true, false] {
            let d = disk(test_replay());
            let mut io = FakeIo(d.clone());
            let diag = SaveDiag {
                placed,
                ..sample_diag()
            };
            s.record(b"replay/th9_03.rpy", START, 2, diag);
            let (_, line) = step(&s, &mut last, 1, "1.1.1", &mut io).unwrap();
            let want_placed = if placed { "と一致" } else { "と不一致" };
            for w in [
                "書く前の 10 語の 0 でない面 3/10",
                "本当の印の 0 でない面 7/10",
                want_placed,
                "見積もりに入る塊 9 個",
                "見積もり 0x4002D",
                "要る量の上限 0x3002D",
            ] {
                assert!(line.contains(w), "{w} が無い: {line}");
            }
        }
        // チャンクが読めなかったときはそう分かる形にする（チャンクの数を出さない）。
        s.record(
            b"replay/th9_03.rpy",
            START,
            2,
            SaveDiag {
                chunks: None,
                ..sample_diag()
            },
        );
        let d = disk(test_replay());
        let (_, line) = step(&s, &mut last, 1, "1.1.1", &mut FakeIo(d)).unwrap();
        assert!(line.contains("塊は読めなかった"), "{line}");
        assert!(!line.contains("見積もりに入る塊"), "{line}");
        // 足さなかった行にも添える。
        let d = disk(test_replay());
        d.borrow_mut().same_dir = Some(false);
        s.record(b"replay/th9_03.rpy", START, 2, sample_diag());
        let (_, line) = step(&s, &mut last, 1, "1.1.1", &mut FakeIo(d)).unwrap();
        assert!(
            line.contains("印を足さない") && line.contains("保存の直し"),
            "{line}"
        );
    }
}
