//! マッチの終わりのリプレイの自動保存（pure。unsafe 無し）。ここに置くもの:
//! - ゲームのスレッドの判断（`Driver::tick`）: 入力の地点で毎 tick、場面とメニューの印を読み、保存を呼ぶか決め、呼ぶ前の確かめ
//!   （Match か・記録の側か・署名・見積もり）をし、乱数を退避して保存を呼び、書き戻す。ゲームのメモリとゲームの関数は `SaveIo` の向こう
//!   （本物は `replay_save_win::RealSaveIo`）。確保・ファイル・ロック待ち・パニックしうる添字を置かない。
//! - 受け渡しの入れ物（`Handoff`。原子変数だけ）: 保管庫のワーカーが準備したパス、ゲームのスレッドが出す対戦の情報（名前のためのもの）と
//!   保存の記録、呼ばなかった訳の数。
//! - 保管庫のワーカー（`Worker::step`。ゲームのスレッドの外の 1 本のスレッドで 50 ms ごと）: フォルダを作り、空いている名前を選んで
//!   0 バイトで押さえ、保存関数と同じ `CreateFileA` の経路で同じファイルに届くかを確かめてからパスを渡す。保存の後に印
//!   （`higanr_core::replay`）を足す。ファイルは `std::fs`、Win32 が要るもの（ゲームのメモリの exe のフォルダ・`CreateFileA` での
//!   確かめ）は `VaultOs` の向こう。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU8, AtomicU32, Ordering, fence};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use higanr_core::autosave::{
    self, ANSI_PATH_MAX, ChargeSync, GateScene, NAME_BUF, SaveGate, SaveKind, VAULT_DIR,
    clock_name, exe_dir_of, result_of,
};
use higanr_core::handshake::Name;
use higanr_core::replay::{self, Mark, MarkRead, MarkRole, MatchResult, Side, Timestamp};
use higanr_core::replay_rounds::{
    Boundary, MatchLog, MatchSummary, NetSample, NetTotals, SUMMARY_WORDS, SideSnap,
};
use higanr_core::update::BUILD_NUMBER;
use higanr_core::vault::{self, Folder, Split};
use higanr_core::wire::{NAME_MAX, Seat};

use crate::menu::MemRead;
use crate::names::is_same_or_inside;
use crate::netplay::{NetPlay, SCENE_MATCH};
use crate::th09_addr::{
    GAME_MODE_MATCH, PLAYER_BOSS_ATTACKS, PLAYER_BOSS_REVERSALS, PLAYER_LIFE, PLAYER_SPELL_ATTACKS,
    REPLAY_HEADER_CHARGE, REPLAY_HEADER_LEN, RNG_LEN, SAVE_EST_BASE, SAVE_EST_PER_CHUNK,
    SAVE_EST_PER_STAGE, SAVE_EST_STAGES, STATS_SCORE,
};

/// メニューの印が何 tick 続いたら呼ぶか。adonis rev5 と同じ 6。メニューは最初の 4 フレームは入力を見ず、決めてから次の要求を
/// 書くまで 20 フレームあるので、決める前に呼べる。
pub const MENU_SETTLE_TICKS: u32 = 6;
/// ネットワークが無い実行の名前（印とフォルダの名前。自分は 1P）。
pub const STANDALONE_P1: &str = "1P";
pub const STANDALONE_P2: &str = "2P";
pub const LOG_SUFFIX: &str = "_replay.log";
/// 保管庫のワーカーの回る間隔（ms）。メニューが出てから呼ぶまで 6 tick（100 ms）あるので、その間に 1 回は回る。
pub const WORKER_PERIOD_MS: u64 = 50;
/// 塊の列をたどる上限（1 塊は 3,598 フレーム ≒ 1 分。1 万は 1 週間ぶん）。越えたら読めないとする。
pub const CHAIN_LIMIT: u32 = 10_000;
/// 乱数の退避の大きさ（`th09_addr::RNG_LEN`。状態と抽選回数を含む 12 バイト）。
pub const RNG_BYTES: usize = RNG_LEN as usize;
pub const PATH_BUF: usize = ANSI_PATH_MAX + 1;
pub const STAGES: usize = SAVE_EST_STAGES;

/// ゲームの見積もり（`0x00420CA0`〜`0x00420CC8` と同じ足し方。32 ビットで回る）。`flags` はゲームが読む 10 語（呼び手のスタックの残り）、
/// `chunks` は面ごとの塊の数（先頭の塊を含む）。踏み台のアセンブリも同じ式で計算する。
pub fn game_estimate(base: u32, flags: &[u32; STAGES], chunks: &[u32; STAGES]) -> u32 {
    let mut est = base;
    for (f, c) in flags.iter().zip(chunks) {
        if *f != 0 {
            est = est
                .wrapping_add(SAVE_EST_PER_STAGE)
                .wrapping_add(c.wrapping_mul(SAVE_EST_PER_CHUNK));
        }
    }
    est
}

/// 要る量の上限: ヘッダ ＋ 本当の印が 0 でない面の、ゲーム自身の面ごと・塊ごとの足し分（推定）。上限を越える値は `u32::MAX` に留める
/// （そのときは呼ばない側へ倒れる）。
pub fn need_bound(real_flags: &[u32; STAGES], chunks: &[u32; STAGES]) -> u32 {
    let mut need = u64::from(REPLAY_HEADER_LEN);
    for (f, c) in real_flags.iter().zip(chunks) {
        if *f != 0 {
            need += u64::from(SAVE_EST_PER_STAGE) + u64::from(*c) * u64::from(SAVE_EST_PER_CHUNK);
        }
    }
    u32::try_from(need).unwrap_or(u32::MAX)
}

/// 呼ばなかった訳（数を `Handoff` に持つ。並びは `SKIP_NAMES` と同じ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Disabled = 0,
    NotMatch = 1,
    Loading = 2,
    NoManager = 3,
    Playback = 4,
    ModeUnreadable = 5,
    Signature = 6,
    EstimateUnreadable = 7,
    NotReady = 8,
    /// 観戦者の実行で、観戦しているマッチの 1P・2P の名前をまだ受けていない。
    WatchNoNames = 9,
    /// マッチの終わりのメニューを見ないまま場面 2 を抜けた（Esc から「タイトルへ」・相手が落ちた・早く終わった再戦など）。
    /// 途中で終わったマッチは保存しない。
    Unfinished = 10,
    /// 同期ズレで止めたマッチ（`NetPlay::stop_flags` が立っている）。Esc で戻って最後まで遊んでも、2 人のマッチがずれたままなので残さない。
    Desynced = 11,
}

pub const SKIP_KINDS: usize = 12;
pub const SKIP_NAMES: [&str; SKIP_KINDS] = [
    "止めた後",
    "Match でない",
    "読み込み中",
    "ReplayManager が無い",
    "再生の側",
    "+0x114 が読めない",
    "保存関数の署名が合わない",
    "見積もりの材料が読めない",
    "パスの準備が間に合わない",
    "観戦の試合の名前をまだ受けていない",
    "途中で終わった試合（保存しない）",
    "同期ズレで止めた試合（保存しない）",
];

/// ネットワークのセッションから取る、印とフォルダの名前に要る値（ゲームのスレッドで取る。確保しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetView {
    pub seat: Seat,
    /// 自分の名前（この接続の HELLO で名乗った値 `Established::own_name`）。ゲームの中のメニューで名前を変えた回でも、
    /// 起動のときの設定でなくこの値をフォルダ名と印の「自分」にする。
    pub own_name: Name,
    pub peer_name: Name,
    pub delay: u8,
    pub rtt_ms: Option<u32>,
    /// 同期ズレでマッチを止めた（`NetPlay::stop_flags` が 0 でない。Esc で戻った後も立ったまま）。
    pub desynced: bool,
    /// rollback の状態機械がマッチの tick を動かしている（`NetPlay::rollback_ready`）。この間のラウンドの境目は、入力の地点（フレームに 1 回）
    /// でなく、確定した tick の後（`Driver::note_confirmed_tick`）に拾う。
    pub rb_ready: bool,
    /// 今の対戦の形式が rollback か。
    pub rollback: bool,
    /// 接続の中で何マッチ目か（`Stats::segs_started`）。
    pub match_no: u32,
    /// 接続の始まりからの、止まった数・進め直しの数（マッチの分は差で出す）。
    pub totals: NetTotals,
}

impl NetView {
    pub fn of(np: &NetPlay) -> Option<NetView> {
        np.connected().map(|c| {
            let st = np.stats();
            let rb = np.rollback_core_stats();
            let totals = match rb {
                Some(r) => NetTotals {
                    stalls: u64::from(r.waits_ahead) + u64::from(r.waits_boundary),
                    rewinds: u64::from(r.resims),
                    resim_ticks: u64::from(r.resim_confirmed_ticks)
                        + u64::from(r.resim_predicted_ticks),
                },
                None => NetTotals {
                    stalls: st.stalls,
                    rewinds: 0,
                    resim_ticks: 0,
                },
            };
            NetView {
                seat: c.seat(),
                own_name: c.established().own_name,
                peer_name: c.established().peer_name,
                delay: c.seg_delay(),
                rtt_ms: c.liveness().rtt_ms(),
                desynced: np.stop_flags() != 0,
                rb_ready: np.rollback_ready(),
                rollback: rb.is_some(),
                match_no: st.segs_started,
                totals,
            }
        })
    }

    fn sample(&self) -> NetSample {
        NetSample {
            rollback: self.rollback,
            match_no: self.match_no,
            totals: self.totals,
        }
    }
}

/// 観戦者の実行で見えるもの（ゲームのスレッドが毎 tick 渡す）。観戦者は対戦のセッションを持たず `NetView` が無い。保存関数は呼べるが、
/// フォルダと印に使う名前（1P・2P）はホストの流れの `Name` の記録から来る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watching {
    No,
    /// 観戦者で、観戦しているマッチの名前をまだ受けていない（保存しない）。
    WaitingForNames,
    Names([Name; 2]),
}

/// フォルダの名前に要る情報（ゲームのスレッドが変わったときだけ出し、保管庫のワーカーが読む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchInfo {
    pub net: bool,
    /// 自分の席（ネットワークが無ければ 1P。観戦者も 1P）。
    pub seat: Seat,
    /// 相手の名前（ネットワークが無ければ空。観戦者も空）。
    pub peer: Name,
    /// 自分の名前（つながった接続で名乗った値。ネットワークが無ければ空。空の間はワーカーが起動のときの設定の名前を使う）。観戦者は空 ——
    /// 保存は 1P・2P の名前で、印の「自分」だけが起動のときの名前。
    pub own: Name,
    pub watching: Watching,
}

impl MatchInfo {
    pub fn of(net: Option<NetView>, watching: Watching) -> MatchInfo {
        match net {
            Some(n) => MatchInfo {
                net: true,
                seat: n.seat,
                peer: n.peer_name,
                own: n.own_name,
                watching,
            },
            None => MatchInfo {
                net: false,
                seat: Seat::P1,
                peer: Name::EMPTY,
                own: Name::EMPTY,
                watching,
            },
        }
    }
}

fn side_of(seat: Seat) -> Side {
    match seat {
        Seat::P1 => Side::P1,
        Seat::P2 => Side::P2,
    }
}

/// 席ごとの、境目の tick の値（ライフ・3 つのカウンタ・スコア）。`players` / `stats` は席ごとのポインタの欄のアドレス。
/// ポインタが 0（マッチの外・片付け済み）や読めないアドレス、塊の欄が読めない席は `None`（落ちない読み方 `MemRead` だけを使う）。
pub fn read_sides(
    m: &mut dyn MemRead,
    players: [u32; 2],
    stats: [u32; 2],
) -> [Option<SideSnap>; 2] {
    core::array::from_fn(|i| {
        let player = m.u32_at(*players.get(i)?).filter(|p| *p != 0)?;
        let st = m.u32_at(*stats.get(i)?).filter(|p| *p != 0)?;
        let mut field = |base: u32, off: u32| m.u32_at(base.checked_add(off)?);
        Some(SideSnap {
            life: field(player, PLAYER_LIFE)?,
            c23: field(player, PLAYER_SPELL_ATTACKS)?,
            boss: field(player, PLAYER_BOSS_ATTACKS)?,
            rev: field(player, PLAYER_BOSS_REVERSALS)?,
            score: field(st, STATS_SCORE)?,
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveRecord {
    pub kind: SaveKind,
    pub called: bool,
    pub rng_restored: bool,
    pub result: MatchResult,
    pub side: Side,
    pub lat: u8,
    pub ping_ms: u32,
    pub at: Timestamp,
    /// 保存関数の呼び出し（踏み台の中の見積もりを含む）の所要（µs）。
    pub duration_us: u32,
    pub base: u32,
    /// 書いた後のゲームの見積もり（踏み台が計算した値。呼ぶかどうかはこれで決めた）。
    pub game_est: u32,
    pub need: u32,
    /// ゲームの見積もりが読む 10 語の、書く前の値（呼び手のスタックの残り）。
    pub seen: [u32; STAGES],
    pub real: [u32; STAGES],
    /// 踏み台が本当の印を書いて読み戻した 10 語（ゲームの見積もりが読む値）。
    pub written: [u32; STAGES],
    pub chunks: [u32; STAGES],
    /// 乱数の 12 バイト: 呼ぶ前・呼んだ後（書き戻す前）・書き戻した後。
    pub rng: [[u8; RNG_BYTES]; 3],
    pub manager: u32,
    /// 1P の勝ち数・2P の勝ち数・勝ちに要る数。
    pub wins: [u32; 3],
    /// 見出しのチャージ型を設定の写しに合わせた結果（保存の直前）。
    pub charge: ChargeSync,
}

/// 記録の語の数（先頭の 13 語、10 語の配列 4 つ、乱数 12 バイト × 3）。
const RECORD_ARRAYS: usize = 4;
pub const RECORD_WORDS: usize = 13 + RECORD_ARRAYS * STAGES + 9;

fn result_code(r: MatchResult) -> u32 {
    match r {
        MatchResult::Win => 0,
        MatchResult::Lose => 1,
        MatchResult::Draw => 2,
        MatchResult::Abort => 3,
    }
}

fn result_from_code(c: u32) -> Option<MatchResult> {
    [
        MatchResult::Win,
        MatchResult::Lose,
        MatchResult::Draw,
        MatchResult::Abort,
    ]
    .get(c as usize)
    .copied()
}

impl SaveRecord {
    pub fn to_words(self) -> [u32; RECORD_WORDS] {
        let mut w = [0u32; RECORD_WORDS];
        let (leaving, menu_seen) = match self.kind {
            SaveKind::MenuShown => (false, false),
            SaveKind::Leaving { menu_seen } => (true, menu_seen),
        };
        w[0] = u32::from(leaving)
            | u32::from(menu_seen) << 1
            | u32::from(self.called) << 2
            | u32::from(self.rng_restored) << 3
            | result_code(self.result) << 8
            | u32::from(self.side == Side::P2) << 16
            | u32::from(self.lat) << 24;
        w[1] = self.ping_ms;
        let a = &self.at;
        w[2] = u32::from(a.year) << 16 | u32::from(a.month) << 8 | u32::from(a.day);
        w[3] = u32::from(a.hour) << 16 | u32::from(a.minute) << 8 | u32::from(a.second);
        w[4] = self.duration_us;
        w[5] = self.base;
        w[6] = self.game_est;
        w[7] = self.need;
        w[8] = self.manager;
        w[9..12].copy_from_slice(&self.wins);
        w[12] = self.charge.to_word();
        let mut i = 13;
        for arr in [&self.seen, &self.real, &self.written, &self.chunks] {
            w[i..i + STAGES].copy_from_slice(arr);
            i += STAGES;
        }
        for r in &self.rng {
            for c in r.as_chunks::<4>().0 {
                w[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                i += 1;
            }
        }
        w
    }

    pub fn from_words(w: &[u32; RECORD_WORDS]) -> Option<SaveRecord> {
        let f = w[0];
        let kind = if f & 1 == 0 {
            SaveKind::MenuShown
        } else {
            SaveKind::Leaving {
                menu_seen: f & 2 != 0,
            }
        };
        let at = Timestamp {
            year: (w[2] >> 16) as u16,
            month: (w[2] >> 8) as u8,
            day: w[2] as u8,
            hour: (w[3] >> 16) as u8,
            minute: (w[3] >> 8) as u8,
            second: w[3] as u8,
        };
        let arr = |start: usize| -> [u32; STAGES] {
            let mut a = [0u32; STAGES];
            a.copy_from_slice(&w[start..start + STAGES]);
            a
        };
        let base = 13 + RECORD_ARRAYS * STAGES;
        let mut rng = [[0u8; RNG_BYTES]; 3];
        for (k, r) in rng.iter_mut().enumerate() {
            for j in 0..RNG_BYTES / 4 {
                r[j * 4..j * 4 + 4].copy_from_slice(&w[base + k * 3 + j].to_le_bytes());
            }
        }
        Some(SaveRecord {
            kind,
            called: f & 4 != 0,
            rng_restored: f & 8 != 0,
            result: result_from_code((f >> 8) & 0xFF)?,
            side: if (f >> 16) & 1 != 0 {
                Side::P2
            } else {
                Side::P1
            },
            lat: (f >> 24) as u8,
            ping_ms: w[1],
            at,
            duration_us: w[4],
            base: w[5],
            game_est: w[6],
            need: w[7],
            manager: w[8],
            wins: [w[9], w[10], w[11]],
            charge: ChargeSync::from_word(w[12])?,
            seen: arr(13),
            real: arr(13 + STAGES),
            written: arr(13 + 2 * STAGES),
            chunks: arr(13 + 3 * STAGES),
            rng,
        })
    }
}

/// パスのバッファの持ち主（`Handoff::state`）。
pub const ST_EMPTY: u32 = 0;
/// 保管庫のワーカーが準備している（パスのバッファはワーカーのもの）。
pub const ST_PREPARING: u32 = 1;
/// 準備が済んだ（ゲームのスレッドが取ってよい）。
pub const ST_READY: u32 = 2;
/// ゲームのスレッドが取った（パスのバッファと記録はゲームのスレッドのもの）。
pub const ST_CALLING: u32 = 3;
/// 記録を書き終えた（保管庫のワーカーが読む）。
pub const ST_USED: u32 = 4;

const NAME_WORDS: usize = 1 + NAME_MAX / 4;
/// 情報の語の数（ネットワークか・席・観戦の状態の 1 語 ＋ 名前 4 つ: 相手 / 観戦の 1P / 観戦の 2P / 自分）。
const INFO_WORDS: usize = 1 + 4 * NAME_WORDS;
/// 情報の語 0 の bit 2〜3: 観戦の状態（0 観戦でない / 1 名前を待っている / 2 名前を受けた）。
const INFO_WATCH_SHIFT: u32 = 2;

fn name_to_words(n: &Name) -> [u32; NAME_WORDS] {
    let mut w = [0u32; NAME_WORDS];
    let name = n.as_bytes();
    w[0] = name.len() as u32;
    let mut bytes = [0u8; NAME_MAX];
    for (d, s) in bytes.iter_mut().zip(name) {
        *d = *s;
    }
    for (a, c) in w[1..].iter_mut().zip(bytes.as_chunks::<4>().0) {
        *a = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }
    w
}

fn name_from_words(w: &[u32]) -> Name {
    let mut bytes = Vec::with_capacity(NAME_MAX);
    for x in w.iter().skip(1).take(NAME_MAX / 4) {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    bytes.truncate((w.first().copied().unwrap_or(0) as usize).min(NAME_MAX));
    Name::new(std::str::from_utf8(&bytes).unwrap_or(""))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Saved = 0,
    /// 保存関数を呼ばなかった（書き込みが足りない見込み）。
    NotCalled = 1,
    /// 呼んだが、ゲームが保存先に書かなかった（0 バイト）。
    EmptyFile = 2,
    MarkFailed = 3,
    /// 保存先の準備ができなかった（2 秒ごとに試し直すので、続く間は増える）。
    PrepFailed = 4,
}

pub const OUTCOME_KINDS: usize = 5;

/// ゲームのスレッドと保管庫のワーカーの受け渡し（原子変数だけ）。パスのバッファと記録は、`state` が表す持ち主だけが触る
/// （持ち主を移すのは Release の書き・Acquire の読み・compare-exchange）。情報は seqlock（書くのはゲームのスレッドだけ）。
pub struct Handoff {
    state: AtomicU32,
    path: [AtomicU8; PATH_BUF],
    info_seq: AtomicU32,
    info: [AtomicU32; INFO_WORDS],
    record: [AtomicU32; RECORD_WORDS],
    /// 保存の記録と同じ持ち主の規則（ゲームのスレッドが書いてから `ST_USED` にし、保管庫のワーカーはその後に読む）で渡す、マッチの記録。
    summary: [AtomicU32; SUMMARY_WORDS],
    skips: [AtomicU32; SKIP_KINDS],
    retries: [AtomicU32; SKIP_KINDS],
    attempts: AtomicU32,
    outcomes: [AtomicU32; OUTCOME_KINDS],
    /// パスが要る（Match の seg に入っていて、まだ保存していない）。ゲームのスレッドが毎 tick 書き、ワーカーが読む。ワーカーは
    /// これが立っていなければ新しい名前を押さえない（マッチをしない実行に 0 バイトの名前を残さない）。
    want: AtomicU32,
}

impl Default for Handoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Handoff {
    pub const fn new() -> Self {
        Handoff {
            state: AtomicU32::new(ST_EMPTY),
            path: [const { AtomicU8::new(0) }; PATH_BUF],
            info_seq: AtomicU32::new(0),
            info: [const { AtomicU32::new(0) }; INFO_WORDS],
            record: [const { AtomicU32::new(0) }; RECORD_WORDS],
            summary: [const { AtomicU32::new(0) }; SUMMARY_WORDS],
            skips: [const { AtomicU32::new(0) }; SKIP_KINDS],
            retries: [const { AtomicU32::new(0) }; SKIP_KINDS],
            attempts: AtomicU32::new(0),
            outcomes: [const { AtomicU32::new(0) }; OUTCOME_KINDS],
            want: AtomicU32::new(0),
        }
    }

    pub fn wanted(&self) -> bool {
        self.want.load(Ordering::Relaxed) != 0
    }

    pub fn state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    fn cas(&self, from: u32, to: u32) -> bool {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// パスのバッファのアドレス（ゲームの保存関数へ渡す。`AtomicU8` は `u8` と同じ並び）。
    pub fn path_addr(&self) -> u32 {
        self.path.as_ptr() as usize as u32
    }

    /// パスを置く（保管庫のワーカー。`ST_PREPARING` の間だけ）。NUL を足して入れ物に収まらなければ false。
    fn put_path(&self, ansi: &[u8]) -> bool {
        if ansi.len() >= PATH_BUF || ansi.contains(&0) {
            return false;
        }
        for (i, a) in self.path.iter().enumerate() {
            a.store(ansi.get(i).copied().unwrap_or(0), Ordering::Relaxed);
        }
        true
    }

    #[cfg(test)]
    pub fn path_bytes(&self) -> Vec<u8> {
        self.path
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .take_while(|&b| b != 0)
            .collect()
    }

    fn publish_info(&self, info: &MatchInfo) {
        let s = self.info_seq.load(Ordering::Relaxed);
        self.info_seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let (watch_state, p1, p2) = match info.watching {
            Watching::No => (0u32, Name::EMPTY, Name::EMPTY),
            Watching::WaitingForNames => (1, Name::EMPTY, Name::EMPTY),
            Watching::Names([a, b]) => (2, a, b),
        };
        self.info[0].store(
            u32::from(info.net)
                | u32::from(info.seat == Seat::P2) << 1
                | watch_state << INFO_WATCH_SHIFT,
            Ordering::Relaxed,
        );
        for (k, n) in [info.peer, p1, p2, info.own].iter().enumerate() {
            let base = 1 + k * NAME_WORDS;
            for (a, v) in self.info[base..base + NAME_WORDS]
                .iter()
                .zip(name_to_words(n))
            {
                a.store(v, Ordering::Relaxed);
            }
        }
        self.info_seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// 情報を読む（保管庫のワーカー）。まだ出ていなければ `None`。返す 1 つ目は何回目に出したものか。
    pub fn read_info(&self) -> Option<(u32, MatchInfo)> {
        for _ in 0..100 {
            let s1 = self.info_seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let w: Vec<u32> = self
                .info
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .collect();
            fence(Ordering::Acquire);
            if self.info_seq.load(Ordering::Relaxed) != s1 {
                continue;
            }
            let name_at = |k: usize| {
                let base = 1 + k * NAME_WORDS;
                name_from_words(&w[base..base + NAME_WORDS])
            };
            let watching = match (w[0] >> INFO_WATCH_SHIFT) & 3 {
                1 => Watching::WaitingForNames,
                2 => Watching::Names([name_at(1), name_at(2)]),
                _ => Watching::No,
            };
            return Some((
                s1 / 2,
                MatchInfo {
                    net: w[0] & 1 != 0,
                    seat: if w[0] & 2 != 0 { Seat::P2 } else { Seat::P1 },
                    peer: name_at(0),
                    own: name_at(3),
                    watching,
                },
            ));
        }
        None
    }

    fn bump(&self, table: &[AtomicU32; SKIP_KINDS], s: Skip) {
        table[s as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// 呼ばなかった訳の数（seg を終えたもの）と、次の tick にもう 1 回試した数。
    pub fn skip_counts(&self) -> ([u32; SKIP_KINDS], [u32; SKIP_KINDS]) {
        let l =
            |t: &[AtomicU32; SKIP_KINDS]| core::array::from_fn(|i| t[i].load(Ordering::Relaxed));
        (l(&self.skips), l(&self.retries))
    }

    pub fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::Relaxed)
    }

    fn note_outcome(&self, o: Outcome) {
        self.outcomes[o as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn outcome_counts(&self) -> [u32; OUTCOME_KINDS] {
        core::array::from_fn(|i| self.outcomes[i].load(Ordering::Relaxed))
    }

    fn publish_record(&self, r: &SaveRecord) {
        for (a, v) in self.record.iter().zip(r.to_words()) {
            a.store(v, Ordering::Relaxed);
        }
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.state.store(ST_USED, Ordering::Release);
    }

    pub fn read_record(&self) -> Option<SaveRecord> {
        let w: [u32; RECORD_WORDS] =
            core::array::from_fn(|i| self.record[i].load(Ordering::Relaxed));
        SaveRecord::from_words(&w)
    }

    /// マッチの記録を置く（ゲームのスレッド。`publish_record` の前に呼ぶ —— ワーカーは `ST_USED` を見てから読む）。
    fn publish_summary(&self, s: &MatchSummary) {
        for (a, v) in self.summary.iter().zip(s.to_words()) {
            a.store(v, Ordering::Relaxed);
        }
    }

    pub fn read_summary(&self) -> MatchSummary {
        let w: [u32; SUMMARY_WORDS] =
            core::array::from_fn(|i| self.summary[i].load(Ordering::Relaxed));
        MatchSummary::from_words(&w)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstimateReal {
    /// 見積もりの初めの値（`0x420C8F` が `B9` のときの imm32）。
    pub base: u32,
    /// ヘッダ `[this+8]` の `+0x20` からの 10 語。
    pub flags: [u32; STAGES],
    /// 面ごとの塊の数（先頭の塊を含む）。
    pub chunks: [u32; STAGES],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallOut {
    pub called: bool,
    /// ゲームの見積もりが読む 10 語の、書く前の値（名前を積んだ直後に読んだ）。
    pub seen: [u32; STAGES],
    pub written: [u32; STAGES],
    pub game_est: u32,
}

/// ゲームのメモリとゲームの関数（本物は `replay_save_win::RealSaveIo`）。どれも確保しない・パニックしない。
pub trait SaveIo {
    /// （今の場面, 次の要求）。
    fn scenes(&mut self) -> (u32, u32);
    fn menu_flag(&mut self) -> u8;
    fn loading(&mut self) -> u32;
    fn game_mode(&mut self) -> u32;
    fn replay_manager(&mut self) -> u32;
    /// `[rm + 0x114]`（落ちない読み方。読めなければ `None`）。
    fn replay_mode(&mut self, rm: u32) -> Option<u32>;
    /// 見出し `[rm + 8]` の日付と名前（再生の見張りが、どのリプレイかを行に残すため）。読めなければ `None`。
    fn replay_ident(&mut self, _rm: u32) -> Option<crate::replay_watch::HeaderIdent> {
        None
    }
    fn signature_ok(&mut self) -> bool;
    fn estimate_real(&mut self, rm: u32) -> Option<EstimateReal>;
    /// （1P の勝ち数, 2P の勝ち数, 勝ちに要る数）。
    fn wins(&mut self) -> [u32; 3];
    fn read_rng(&mut self) -> [u8; RNG_BYTES];
    fn write_rng(&mut self, v: &[u8; RNG_BYTES]);
    fn local_time(&mut self) -> Timestamp;
    fn now_us(&mut self) -> u64;
    /// 現在の UTC の UNIX 秒（マッチの開始時刻の印。マッチごとに 1 回）。
    fn unix_secs(&mut self) -> u64;
    /// `completed_rounds`（ラウンドが終わるたびに増える数。毎 tick 読む）。
    fn completed_rounds(&mut self) -> u32;
    /// いまの tick の値を境目として読む（`completed_rounds` が増えた tick にだけ呼ぶ。席の塊が読めなければその席は `None`）。
    fn read_boundary(&mut self) -> Boundary;
    /// 保存関数を呼ぶ直前に、見出し `[[0x4A7E64]+8]` のチャージ型（`+0x190` / `+0x191`）を、ゲームの設定の写し `[0x4A7E78]` の
    /// `+0xB4` / `+0xB5`（生きた試合が使った値）に合わせる。ポインタが 0 か読めないときは書かない。
    fn sync_header_charge(&mut self, rm: u32) -> ChargeSync;
    /// 踏み台で保存関数を呼ぶ。ゲームの見積もりが読む 10 語へ `est.flags`（本当の印）を書いてから見積もりを計算し、`need` より小さければ呼ばない。
    fn call_save(
        &mut self,
        rm: u32,
        path_addr: u32,
        name: &[u8; NAME_BUF],
        est: &EstimateReal,
        need: u32,
    ) -> CallOut;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// 記録を出した（呼んだか、見積もりで呼ばなかったか）。
    Recorded,
    Skipped(Skip),
    /// 次の tick にもう 1 回（メニューの間だけ）。
    Retry(Skip),
}

#[derive(Debug)]
pub struct Driver {
    gate: SaveGate,
    last_info: Option<MatchInfo>,
    disabled: bool,
    /// 今の seg（マッチ）の記録: ラウンドの境目・ping・止まった数。`gate.epoch()` が替わったら捨てる。
    log: MatchLog,
    epoch: u32,
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver {
    pub const fn new() -> Self {
        Driver {
            gate: SaveGate::new(MENU_SETTLE_TICKS),
            last_info: None,
            disabled: false,
            log: MatchLog::new(),
            epoch: 0,
        }
    }

    /// マッチの記録（検査が見る）。
    #[cfg(test)]
    pub fn summary(&self) -> MatchSummary {
        self.log.summary()
    }

    /// seg の始まりで記録を捨て、マッチの中（読み込みが済んだ Match）ならマッチの開始と ping・止まった数・境目を積む。ゲームのスレッドで毎 tick
    /// 呼ぶので、確保しない。境目の見張りは `completed_rounds` の 4 バイトの比べ 1 回で、増えた tick にだけ席の塊を読む。
    /// rollback の状態機械が tick を動かしている間（`rb_ready`）は、ここ（フレームに 1 回、予測の tick の後）では境目を見ない:
    /// 予測の tick の境目は書き戻しで消えるので、確定した tick の後（`note_confirmed_tick`）だけで拾う。
    fn update_log(&mut self, io: &mut dyn SaveIo, net: Option<NetView>, scene: GateScene) {
        let epoch = self.gate.epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.log.begin_seg();
        }
        if !scene.in_match || scene.loading || io.game_mode() != GAME_MODE_MATCH {
            return;
        }
        if !self.log.is_started() {
            let completed = io.completed_rounds();
            self.log
                .start(io.unix_secs(), completed, net.map(|n| n.sample()));
        }
        if let Some(n) = net {
            self.log.sample(n.rtt_ms, Some(n.sample()));
        }
        if !net.is_some_and(|n| n.rb_ready) {
            let completed = io.completed_rounds();
            self.log.observe_rounds(completed, || io.read_boundary());
        }
    }

    /// rollback の確定した tick の直後（ゲームのスレッド）。この tick の状態は確定した状態そのものなので、境目の値はここで読む。
    pub fn note_confirmed_tick(&mut self, io: &mut dyn SaveIo) {
        if !self.log.is_started() {
            return;
        }
        let completed = io.completed_rounds();
        self.log.observe_rounds(completed, || io.read_boundary());
    }

    /// この seg で保存関数を呼んだか（`SaveGate::called`）。見積もりで呼ばなかった・準備が間に合わなかった・試みていない seg は false。
    /// ネットワークの対戦の「リプレイを保存して終了」でゲームの保存の画面を飛ばしてよいかの材料。呼んだ後にゲームがファイルを書けたかは
    /// 保管庫のワーカーが後で見るので、ここには入らない。
    pub fn saved_this_seg(&self) -> bool {
        self.gate.called()
    }

    /// 観戦者でない実行の形（検査が使う。本物の経路は `tick_watching`）。
    #[cfg(test)]
    pub fn tick(
        &mut self,
        h: &Handoff,
        io: &mut dyn SaveIo,
        net: Option<NetView>,
    ) -> Option<Attempt> {
        self.tick_watching(h, io, net, Watching::No)
    }

    /// `tick` に、観戦者の実行で見えるもの（`Watching`）を足した形。観戦者は `net` が `None` で、`watching` が 1P・2P の名前を持つ。
    pub fn tick_watching(
        &mut self,
        h: &Handoff,
        io: &mut dyn SaveIo,
        net: Option<NetView>,
        watching: Watching,
    ) -> Option<Attempt> {
        let info = MatchInfo::of(net, watching);
        if self.last_info != Some(info) {
            h.publish_info(&info);
            self.last_info = Some(info);
        }
        let (cur, req) = io.scenes();
        let in_match = cur == SCENE_MATCH;
        let scene = GateScene {
            in_match,
            leaving: in_match && req != SCENE_MATCH,
            menu: in_match && io.menu_flag() != 0,
            loading: in_match && io.loading() != 0,
        };
        let due = self.gate.tick(scene);
        // この tick の分の記録を先に積む（保存の記録へ入れるのは、この tick の値を含めたもの）。
        self.update_log(io, net, scene);
        let out = due.map(|kind| {
            let a = self.attempt(kind, h, io, net);
            match a {
                Attempt::Recorded => self.gate.finish(),
                Attempt::Skipped(s) => {
                    h.bump(&h.skips, s);
                    self.gate.finish();
                }
                Attempt::Retry(s) => h.bump(&h.retries, s),
            }
            a
        });
        let want = in_match && !self.gate.done() && io.game_mode() == GAME_MODE_MATCH;
        h.want.store(u32::from(want), Ordering::Relaxed);
        out
    }

    fn attempt(
        &mut self,
        kind: SaveKind,
        h: &Handoff,
        io: &mut dyn SaveIo,
        net: Option<NetView>,
    ) -> Attempt {
        // メニューの間は次の tick にもう 1 回試せる。抜ける tick は 1 回きり。
        let later = |s: Skip| match kind {
            SaveKind::MenuShown => Attempt::Retry(s),
            SaveKind::Leaving { .. } => Attempt::Skipped(s),
        };
        if self.disabled {
            return Attempt::Skipped(Skip::Disabled);
        }
        // 保存するのはマッチの終わりのメニューまで行ったマッチだけ。保険の経路（場面 2 を抜ける tick）は、メニューを見たのに保存を逃した
        // （メニューが出てから `MENU_SETTLE_TICKS` に届く前に抜けた・パスの準備が間に合わなかった）ときだけ働く。
        if kind == (SaveKind::Leaving { menu_seen: false }) {
            return Attempt::Skipped(Skip::Unfinished);
        }
        if net.is_some_and(|n| n.desynced) {
            return Attempt::Skipped(Skip::Desynced);
        }
        if io.game_mode() != GAME_MODE_MATCH {
            return Attempt::Skipped(Skip::NotMatch);
        }
        if io.loading() != 0 {
            return later(Skip::Loading);
        }
        let rm = io.replay_manager();
        if rm == 0 {
            return Attempt::Skipped(Skip::NoManager);
        }
        match io.replay_mode(rm) {
            Some(0) => {}
            Some(_) => return Attempt::Skipped(Skip::Playback),
            None => return Attempt::Skipped(Skip::ModeUnreadable),
        }
        if !io.signature_ok() {
            // 花映塚の保存関数でないものを呼ばない。以後の seg でも呼ばない。
            self.disabled = true;
            return Attempt::Skipped(Skip::Signature);
        }
        let Some(est) = io.estimate_real(rm) else {
            return Attempt::Skipped(Skip::EstimateUnreadable);
        };
        let need = need_bound(&est.flags, &est.chunks);
        // 観戦者で、観戦しているマッチの名前をまだ受けていない（フォルダと印に使う名前が無い）。パスの準備の遅れ（`NotReady`）と
        // 取り違えないよう、先に断る。
        if self
            .last_info
            .is_some_and(|i| i.watching == Watching::WaitingForNames)
        {
            return later(Skip::WatchNoNames);
        }
        if !h.cas(ST_READY, ST_CALLING) {
            return later(Skip::NotReady);
        }
        let t = io.local_time();
        let name = clock_name(&t);
        let wins = io.wins();
        let seat = net.map_or(Seat::P1, |n| n.seat);
        let side = side_of(seat);
        let result = result_of(kind, side, wins[0], wins[1], wins[2]);
        // 見出しは読み込みスレッドが手元の cfg から写したままなので、生きた試合の値に直してから保存する。
        let charge = io.sync_header_charge(rm);
        let before = io.read_rng();
        let t0 = io.now_us();
        let out = io.call_save(rm, h.path_addr(), &name, &est, need);
        if out.called {
            self.gate.mark_called();
        }
        let t1 = io.now_us();
        let after_call = io.read_rng();
        io.write_rng(&before);
        let after_restore = io.read_rng();
        // 記録の前に置く: ワーカーは `ST_USED`（`publish_record` が立てる）を見てから読む。
        h.publish_summary(&self.log.summary());
        h.publish_record(&SaveRecord {
            kind,
            called: out.called,
            rng_restored: after_restore == before,
            result,
            side,
            lat: net.map_or(0, |n| n.delay),
            ping_ms: net.and_then(|n| n.rtt_ms).unwrap_or(0),
            at: t,
            duration_us: u32::try_from(t1.saturating_sub(t0)).unwrap_or(u32::MAX),
            base: est.base,
            game_est: out.game_est,
            need,
            seen: out.seen,
            real: est.flags,
            written: out.written,
            chunks: est.chunks,
            rng: [before, after_call, after_restore],
            manager: rm,
            wins,
            charge,
        });
        Attempt::Recorded
    }
}

/// 保管庫のワーカーが Win32 とゲームのメモリに頼むこと（本物は `replay_save_win::RealVaultOs`）。
pub trait VaultOs {
    /// ゲームが持つ exe のフォルダのバッファ（`0x4ACD08` の 0x104 バイト）。読めなければ `None`。
    fn exe_dir_box(&mut self) -> Option<Vec<u8>>;
    /// CP932 のパス `ansi` を保存関数と同じ `CreateFileA` の経路で開き、`file` と同じファイルか。
    fn same_file_ansi(&mut self, ansi: &[u8], file: &fs::File) -> Result<bool, String>;
    fn now(&mut self) -> Timestamp;
    /// 押さえている 0 バイトのファイルの更新時刻を今にする（中身は変えない）。ゲームの保存が同時に開いても断られない
    /// 開き方で（本物は属性だけの権利）。
    fn touch(&mut self, file: &Path) -> Result<(), String>;
}

#[derive(Debug, Clone)]
struct Prepared {
    info_gen: u32,
    info: MatchInfo,
    date: (u16, u8, u8),
    file: PathBuf,
}

pub struct Worker {
    pub split: Split,
    /// 起動のときの設定の自分の名前（HELLO と同じく 32 バイトに切ったもの）。ネットワークが無ければ使わない。対戦のフォルダ名と印の「自分」は
    /// つながった接続で名乗った名前（`MatchInfo::own`）を使い、これはそれが空のときの代わりと観戦者の印の「自分」。
    pub self_name: String,
    /// ネットワークを置いた実行（ini の `[netplay]` が `enable = 1`）。立っていれば、つながっていて相手の名前が分かっている間にだけ名前を
    /// 押さえる（`may_reserve`）。単独の実行は今まで通り `1P vs 2P` で押さえる。
    pub net_run: bool,
    out: Box<dyn Write + Send>,
    /// 保存の失敗の行（ワーカーのログに書く行と同じ文）を本ログへも流す入口。簡潔なログではワーカーのログを開かないので、失敗の理由が消えないように。
    problem_sink: Box<dyn Fn(String) + Send>,
    prepared: Option<Prepared>,
    last_skips: ([u32; SKIP_KINDS], [u32; SKIP_KINDS]),
    last_error: Option<String>,
    watch_logged: Watching,
    /// 準備に失敗した後、次に試すまでの回数（50 ms × この数）。
    backoff: u32,
    saves: u32,
    marks: u32,
    /// 見出しのチャージ型を合わせられなかった保存の数。
    charge_failures: u32,
    /// このワーカー（このプロセス）が押さえた名前。古い予約を消すときに、これは消さない。
    mine: Vec<PathBuf>,
    since_touch: u32,
    touch_error: Option<String>,
}

const BACKOFF_STEPS: u32 = 40;

/// 前の実行が残した 0 バイトの予約（`th9_ud*.rpy`）を消すまでの古さ（秒）。ゲームが終わるとワーカーのスレッドは押さえた名前を放さずに
/// 消える（実機で `th9_ud0002`・`th9_ud0004` が 0 バイトで残った）。同じ PC でもう 1 つのゲームが押さえている名前を消さないため、
/// 生きているワーカーは押さえている間 `TOUCH_STEPS` ごとに更新時刻を新しくし、それより十分古いものだけを消す。
pub const STALE_RESERVATION_SECS: u64 = 10 * 60;
/// 押さえている名前の更新時刻を新しくする間隔（回。50 ms × 1,200 = 60 秒）。
pub const TOUCH_STEPS: u32 = 1_200;

fn is_ud_reservation_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("th9_ud") && lower.ends_with(".rpy")
}

fn date_of(t: &Timestamp) -> (u16, u8, u8) {
    (t.year, t.month, t.day)
}

fn hex_words(a: &[u32]) -> String {
    a.iter()
        .map(|v| format!("{v:X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// ログに書く保管庫のファイルの名前（フォルダは相手の名前を含むので書かない）。
fn shown_file(p: &Path) -> String {
    p.file_name()
        .map_or(String::new(), |n| n.to_string_lossy().into_owned())
}

/// ログに書く保管庫のフォルダ。保管庫そのもの以外は相手の名前が入った名前なので書かない。
fn shown_dir(p: &Path) -> String {
    if p.file_name().is_some_and(|n| n == VAULT_DIR) {
        VAULT_DIR.to_string()
    } else {
        "保存先のフォルダ".to_string()
    }
}

fn hex_bytes(a: &[u8]) -> String {
    a.iter()
        .map(|v| format!("{v:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn list_names(dir: &Path, dirs: bool) -> Result<Vec<String>, String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("{} を読めない: {e}", shown_dir(dir)))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir == dirs
            && let Ok(n) = e.file_name().into_string()
        {
            out.push(n);
        }
    }
    Ok(out)
}

impl Worker {
    pub fn new(
        split: Split,
        self_name: String,
        net_run: bool,
        out: Box<dyn Write + Send>,
    ) -> Worker {
        Worker {
            split,
            self_name,
            net_run,
            out,
            problem_sink: Box::new(|_| {}),
            prepared: None,
            last_skips: ([0; SKIP_KINDS], [0; SKIP_KINDS]),
            last_error: None,
            watch_logged: Watching::No,
            backoff: 0,
            saves: 0,
            marks: 0,
            charge_failures: 0,
            mine: Vec::new(),
            since_touch: 0,
            touch_error: None,
        }
    }

    /// 失敗の行の流し先を決める（本物は本ログへ頼む）。
    pub fn with_problem_sink(mut self, sink: Box<dyn Fn(String) + Send>) -> Worker {
        self.problem_sink = sink;
        self
    }

    /// 保存の失敗の行を、ワーカーのログと本ログの両方へ。
    fn problem(&mut self, line: String) {
        let _ = writeln!(self.out, "{line}");
        (self.problem_sink)(format!("自動保存: {line}"));
    }

    /// 押さえている名前（準備が済んで、ゲームのスレッドが取るのを待っている間）の更新時刻を `TOUCH_STEPS` ごとに新しくする。
    fn keep_reservation_fresh(&mut self, h: &Handoff, os: &mut dyn VaultOs) {
        let Some(p) = self.prepared.as_ref() else {
            self.since_touch = 0;
            return;
        };
        if h.state() != ST_READY {
            return;
        }
        self.since_touch += 1;
        if self.since_touch < TOUCH_STEPS {
            return;
        }
        self.since_touch = 0;
        match os.touch(&p.file) {
            Ok(()) => self.touch_error = None,
            Err(why) => {
                if self.touch_error.as_deref() != Some(why.as_str()) {
                    let _ = writeln!(
                        self.out,
                        "準備: 押さえた {} の更新時刻を新しくできない（{why}）",
                        shown_file(&p.file)
                    );
                    self.touch_error = Some(why);
                }
            }
        }
    }

    /// `dir`（保管庫 `root` の中）の 0 バイトの `th9_ud*.rpy` のうち、このワーカーが押さえたものでなく、更新時刻が
    /// `STALE_RESERVATION_SECS` より前のものを消す。返すのは（消した名前, 消せなかった数）。
    fn sweep_stale(
        &self,
        dir: &Path,
        root: &Path,
        now: std::time::SystemTime,
    ) -> (Vec<String>, u32) {
        let mut gone = Vec::new();
        let mut failed = 0u32;
        let Ok(rd) = fs::read_dir(dir) else {
            return (gone, failed);
        };
        for e in rd.flatten() {
            // DirEntry の種類と大きさはリンクをたどらない（リンクは消さない）。
            let Ok(t) = e.file_type() else { continue };
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if !t.is_file() || !is_ud_reservation_name(&name) {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let old = m
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age.as_secs() >= STALE_RESERVATION_SECS);
            if m.len() != 0 || !old {
                continue;
            }
            let path = dir.join(&name);
            let p = path.to_string_lossy();
            if self
                .mine
                .iter()
                .any(|x| x.to_string_lossy().eq_ignore_ascii_case(&p))
            {
                continue;
            }
            // 消してよいのは保管庫の中だけ。
            if p.eq_ignore_ascii_case(&root.to_string_lossy())
                || !is_same_or_inside(&p, &root.to_string_lossy())
            {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => gone.push(name),
                Err(_) => failed += 1,
            }
        }
        gone.sort();
        (gone, failed)
    }

    pub fn step(&mut self, h: &Handoff, os: &mut dyn VaultOs) {
        if h.state() == ST_USED {
            self.finish_save(h);
        }
        self.log_skips(h);
        let Some((info_gen, info)) = h.read_info() else {
            return;
        };
        if info.watching != self.watch_logged {
            self.watch_logged = info.watching;
            match info.watching {
                Watching::No => {}
                Watching::WaitingForNames => {
                    let _ = writeln!(
                        self.out,
                        "観戦者の走り: 観戦の試合の名前をまだ受けていない（受けるまで保存しない）"
                    );
                }
                Watching::Names(_) => {
                    let _ = writeln!(
                        self.out,
                        "観戦者の走り: 観戦の試合の名前を受けた（名前は記録しない）"
                    );
                }
            }
        }
        let today = date_of(&os.now());
        let may = self.may_reserve(&info);
        if h.state() == ST_READY {
            // ネットワークの実行でつながりが終わったときも情報が変わる（net = false・名前が空）ので、ここで使われなかった予約を放す
            // （0 バイトのままの、自分が押さえた名前だけを消す）。
            let stale = self.prepared.as_ref().is_none_or(|p| {
                (p.info_gen != info_gen && p.info != info)
                    || (self.split == Split::Date && p.date != today)
            });
            if stale && h.cas(ST_READY, ST_PREPARING) {
                self.release_reservation(if may {
                    "情報が変わった"
                } else {
                    "ネットワークの実行で、つながりが終わった（相手の名前が無い）"
                });
                h.state.store(ST_EMPTY, Ordering::Release);
            }
        }
        self.keep_reservation_fresh(h, os);
        // ネットワークの実行でつながっていない間は押さえない。つなぎ直しの片づけでネットワークを手放した後に、ゲームのスレッドが「パスが要る」を立て、
        // 相手の名前の無い情報のまま `1P vs 2P\th9_ud000N.rpy` を 0 バイトで押さえ、マッチが来ずに残ったことがある。
        if h.state() == ST_EMPTY && h.wanted() && may {
            if self.backoff > 0 {
                self.backoff -= 1;
                return;
            }
            if !h.cas(ST_EMPTY, ST_PREPARING) {
                return;
            }
            match self.prepare(h, os, info_gen, info, today) {
                Ok(()) => {
                    self.last_error = None;
                    h.state.store(ST_READY, Ordering::Release);
                }
                Err(why) => {
                    h.note_outcome(Outcome::PrepFailed);
                    if self.last_error.as_deref() != Some(why.as_str()) {
                        self.problem(format!("準備: できない（{why}）。2 秒ごとに試し直す"));
                        self.last_error = Some(why);
                    }
                    self.backoff = BACKOFF_STEPS;
                    h.state.store(ST_EMPTY, Ordering::Release);
                }
            }
        }
    }

    /// この情報で名前を押さえてよいか。ネットワークの実行では、つながっていて相手の名前が分かっているときだけ（つながっていないマッチは
    /// ネットワークのマッチでないので、保管庫に残さない）。単独の実行はいつでも。観戦者の実行は、観戦しているマッチの 1P・2P の名前を
    /// 受けているときだけ。
    fn may_reserve(&self, info: &MatchInfo) -> bool {
        match info.watching {
            Watching::Names(_) => true,
            Watching::WaitingForNames => false,
            Watching::No => !self.net_run || (info.net && !info.peer.as_str().is_empty()),
        }
    }

    /// （自分の名前, 相手の名前）。観戦者の実行では使わない。自分の名前は、つながった接続で名乗った名前（`info.own`）。
    /// 空なら起動のときの設定の名前（`self_name`）。
    fn names(&self, info: &MatchInfo) -> (String, String) {
        if info.net {
            let own = match info.own.as_str() {
                "" => self.self_name.clone(),
                n => n.to_string(),
            };
            (own, info.peer.as_str().to_string())
        } else {
            (STANDALONE_P1.to_string(), STANDALONE_P2.to_string())
        }
    }

    /// （1P の名前, 2P の名前）。フォルダ名（区切り方 `peer` の `<1P> vs <2P>`）の並び。対戦者は自分の席で入れ替え、観戦者は
    /// 流れの `Name` の記録の 1P・2P のまま。
    fn pair(&self, info: &MatchInfo) -> (String, String) {
        if let Watching::Names([a, b]) = info.watching {
            return (a.as_str().to_string(), b.as_str().to_string());
        }
        let (me, peer) = self.names(info);
        match info.seat {
            Seat::P1 => (me, peer),
            Seat::P2 => (peer, me),
        }
    }

    fn prepare(
        &mut self,
        h: &Handoff,
        os: &mut dyn VaultOs,
        info_gen: u32,
        info: MatchInfo,
        today: (u16, u8, u8),
    ) -> Result<(), String> {
        let bx = os
            .exe_dir_box()
            .ok_or("ゲームの exe のフォルダ（0x4ACD08）が読めない")?;
        let exe = exe_dir_of(&bx)
            .map_err(|e| format!("ゲームの exe のフォルダの形が違う（{e:?}）"))?
            .to_vec();
        let exe_str = autosave::decode_cp932(&exe[..exe.len() - 1])
            .ok_or("exe のフォルダが CP932 で読めない")?;
        let exe_dir = PathBuf::from(&exe_str);
        let root = exe_dir.join(VAULT_DIR);
        let (p1, p2) = self.pair(&info);
        let now = os.now();
        let stamp = Timestamp {
            year: today.0,
            month: today.1,
            day: today.2,
            ..now
        };
        let (folder, fixed_file): (Option<String>, Option<String>) =
            match vault::folder_for(self.split, &p1, &p2, &stamp) {
                Folder::Root => (None, None),
                Folder::Sub(s) => (Some(s), None),
                Folder::Pack50 => {
                    fs::create_dir_all(&root)
                        .map_err(|e| format!("{} を作れない: {e}", shown_dir(&root)))?;
                    let last = vault::pack50_last_folder(&list_names(&root, true)?);
                    let in_last = match last {
                        Some(n) => {
                            let d = root.join(vault::pack50_folder_name(n));
                            list_names(&d, false).unwrap_or_default()
                        }
                        None => Vec::new(),
                    };
                    let (n, f) = vault::pack50_next(last, &in_last)
                        .ok_or("pack50 のフォルダの番号が 9999 を越える")?;
                    (Some(vault::pack50_folder_name(n)), Some(f))
                }
            };
        if let Some(f) = &folder
            && !autosave::is_single_part(f)
        {
            return Err("フォルダ名が 1 つの部品でない".to_string());
        }
        let dir = match &folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        // 書いてよいのは保管庫の中だけ。
        if !is_same_or_inside(&dir.to_string_lossy(), &root.to_string_lossy())
            || !is_same_or_inside(&root.to_string_lossy(), &exe_dir.to_string_lossy())
        {
            return Err(format!("{} が保管庫の中でない", shown_dir(&dir)));
        }
        fs::create_dir_all(&dir).map_err(|e| format!("{} を作れない: {e}", shown_dir(&dir)))?;
        // 前の実行が放さずに残した 0 バイトの予約を先に消す（消さないと番号が 1 つずつ飛び、0 バイトの名前が溜まる）。
        let (gone, failed) = self.sweep_stale(&dir, &root, std::time::SystemTime::now());
        if !gone.is_empty() || failed > 0 {
            let _ = writeln!(
                self.out,
                "準備: {} の、前の走りが残した 0 バイトの予約（{} 分より古い th9_ud*.rpy）を {} 件消した（{}）、消せなかった {} 件",
                shown_dir(&dir),
                STALE_RESERVATION_SECS / 60,
                gone.len(),
                gone.join(" "),
                failed
            );
        }
        let mut reserved = None;
        for _ in 0..5 {
            let name = match &fixed_file {
                Some(f) => f.clone(),
                None => vault::next_ud_name(&list_names(&dir, false)?)
                    .ok_or("th9_ud9999.rpy まで埋まっている")?,
            };
            let path = dir.join(&name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => {
                    self.mine.push(path.clone());
                    reserved = Some((name, path, f));
                    break;
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists && fixed_file.is_none() => {
                    continue;
                }
                Err(e) => return Err(format!("{} を押さえられない: {e}", shown_file(&path))),
            }
        }
        let (name, path, file) = reserved.ok_or("空いている名前を押さえられない（5 回）")?;
        let give_up = |why: String| {
            let _ = fs::remove_file(&path);
            Err(why)
        };
        let ansi = match autosave::ansi_vault_path(&exe, folder.as_deref(), &name) {
            Ok(a) => a,
            Err(e) => return give_up(format!("保存関数へ渡すパスを作れない（{e:?}）")),
        };
        match os.same_file_ansi(&ansi, &file) {
            Ok(true) => {}
            Ok(false) => {
                return give_up(format!(
                    "CP932 のパス（{} バイト）で開くと、押さえた {} と別のファイルになる",
                    ansi.len(),
                    shown_file(&path)
                ));
            }
            Err(e) => return give_up(format!("CP932 のパスで開けない（{e}）")),
        }
        drop(file);
        if !h.put_path(&ansi) {
            return give_up("パスが入れ物に入らない".to_string());
        }
        let _ = writeln!(
            self.out,
            "準備: {}（CP932 で {} バイト。CreateFileA の経路で同じファイルと確かめた）",
            shown_file(&path),
            ansi.len()
        );
        self.prepared = Some(Prepared {
            info_gen,
            info,
            date: today,
            file: path,
        });
        self.since_touch = 0;
        Ok(())
    }

    /// 押さえた名前が 0 バイトのままなら消す（使わなくなった準備）。消すのはこのワーカーが押さえた名前（`prepared`）だけで、保存済みは残す。
    fn release_reservation(&mut self, why: &str) {
        if let Some(p) = self.prepared.take()
            && fs::metadata(&p.file).is_ok_and(|m| m.len() == 0)
        {
            let _ = fs::remove_file(&p.file);
            let _ = writeln!(self.out, "準備: {why}ので {} を放した", shown_file(&p.file));
        }
    }

    fn finish_save(&mut self, h: &Handoff) {
        let Some(r) = h.read_record() else {
            self.problem("★ 保存の記録が読めない（結果の番号が範囲の外）".to_string());
            h.state.store(ST_READY, Ordering::Release);
            return;
        };
        self.saves += 1;
        self.log_record(&r, h.attempts());
        let Some(p) = self.prepared.clone() else {
            self.problem("★ 準備の控えが無いのに記録が来た".to_string());
            h.state.store(ST_EMPTY, Ordering::Release);
            return;
        };
        if !r.called {
            h.note_outcome(Outcome::NotCalled);
            h.state.store(ST_READY, Ordering::Release);
            return;
        }
        let len = fs::metadata(&p.file).map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            h.note_outcome(Outcome::EmptyFile);
            self.problem(format!(
                "★ ゲームが {} に書かなかった（0 バイト。開けなかった見込み）。同じ名前を次にも使う",
                shown_file(&p.file)
            ));
            h.state.store(ST_READY, Ordering::Release);
            return;
        }
        let summary = h.read_summary();
        let mark = self.mark_of(&p.info, &r, &summary);
        match self.append_mark(&p.file, &mark) {
            Ok(n) => {
                self.marks += 1;
                h.note_outcome(Outcome::Saved);
                let _ = writeln!(
                    self.out,
                    "印: {}（{len} バイト）の末尾に {n} バイトを足した（result = {:?}、ラウンドの記録 {} 件・捨てた {} 件）",
                    shown_file(&p.file),
                    r.result,
                    summary.rounds.iter().map_while(|b| *b).count(),
                    summary.rounds_dropped
                );
            }
            Err(why) => {
                h.note_outcome(Outcome::MarkFailed);
                self.problem(format!("★ 印を足せない（{why}）: {}", shown_file(&p.file)));
            }
        }
        self.prepared = None;
        h.state.store(ST_EMPTY, Ordering::Release);
    }

    /// 保存の記録と対戦の情報から印を作る。観戦者の印は `role=watch` と 1P・2P の名前を持ち、`self` は観戦した人の名前、`peer` は空、
    /// `side` は 1、結果は 1P から見たもの、`lat`・`ping` は 0。どちらの印にも、マッチの記録（`summary`。ネットワークの対戦でなければ同期方式・通信の要約・
    /// 何マッチ目は付かない）を足す。
    fn mark_of(&self, info: &MatchInfo, r: &SaveRecord, summary: &MatchSummary) -> Mark {
        let mut detail = summary.detail(BUILD_NUMBER);
        // 見出しへ書いたのと同じ値だけを印に残す（書けなかったときは項目を出さない）。
        if let ChargeSync::Written { p1, p2 } = r.charge {
            detail.charge = Some([p1, p2]);
        }
        let detail = Some(detail);
        if let Watching::Names([a, b]) = info.watching {
            return Mark {
                peer: String::new(),
                self_name: self.self_name.clone(),
                side: Side::P1,
                lat: 0,
                ping_ms: 0,
                result: r.result,
                at: r.at,
                role: MarkRole::Watch {
                    p1: a.as_str().to_string(),
                    p2: b.as_str().to_string(),
                },
                detail,
            };
        }
        let (me, peer) = self.names(info);
        Mark {
            peer,
            self_name: me,
            side: r.side,
            lat: r.lat,
            ping_ms: r.ping_ms,
            result: r.result,
            at: r.at,
            role: MarkRole::Play,
            detail,
        }
    }

    fn append_mark(&mut self, file: &Path, mark: &Mark) -> Result<usize, String> {
        let bytes = fs::read(file).map_err(|e| format!("読めない: {e}"))?;
        let chunk = replay::chunk_to_append(&bytes, mark).map_err(|e| format!("{e:?}"))?;
        let mut f = OpenOptions::new()
            .append(true)
            .open(file)
            .map_err(|e| format!("足すファイルを開けない: {e}"))?;
        f.write_all(&chunk).map_err(|e| format!("書けない: {e}"))?;
        drop(f);
        let back = fs::read(file).map_err(|e| format!("読み戻せない: {e}"))?;
        match replay::read_mark(&back) {
            Ok(MarkRead::Found(m)) if &m == mark => Ok(chunk.len()),
            other => Err(format!("読み戻した印が違う（{other:?}）")),
        }
    }

    fn log_record(&mut self, r: &SaveRecord, attempts: u32) {
        let kind = match r.kind {
            SaveKind::MenuShown => "試合の終わりのメニュー".to_string(),
            SaveKind::Leaving { menu_seen } => format!(
                "場面 2 を抜ける tick（保険。メニューを{}）",
                if menu_seen {
                    "見た"
                } else {
                    "見ていない"
                }
            ),
        };
        let t = &r.at;
        let _ = writeln!(
            self.out,
            "保存 {}（ゲームのスレッドの試み {attempts} 回目）: 訳 = {kind}、呼んだ = {}、所要 = {} µs、結果 = {:?}、席 = {:?}、lat = {}、ping = {} ms、名前 = {:02}:{:02}:{:02}、ReplayManager = 0x{:08X}、勝ち数 1P/2P/要る数 = {}/{}/{}",
            self.saves,
            if r.called {
                "はい"
            } else {
                "いいえ（越える見込み）"
            },
            r.duration_us,
            r.result,
            r.side,
            r.lat,
            r.ping_ms,
            t.hour,
            t.minute,
            t.second,
            r.manager,
            r.wins[0],
            r.wins[1],
            r.wins[2],
        );
        self.log_charge(r.charge);
        let _ = writeln!(
            self.out,
            "  乱数: 前 [{}] / 呼んだ後 [{}] / 戻した後 [{}]（前と同じ = {}）",
            hex_bytes(&r.rng[0]),
            hex_bytes(&r.rng[1]),
            hex_bytes(&r.rng[2]),
            if r.rng_restored {
                "はい"
            } else {
                "★いいえ"
            }
        );
        let before = game_estimate(r.base, &r.seen, &r.chunks);
        let _ = writeln!(
            self.out,
            "  見積もり: 初めの値 0x{:X}、ゲームが読む 10 語（書く前）[{}]、書く前のゲームの見積もり 0x{before:X}、本当の印 [{}]、書いた 10 語（読み戻し）[{}]、塊の数 [{}]、書いた後のゲームの見積もり 0x{:X}、要る上限 0x{:X}（{}）",
            r.base,
            hex_words(&r.seen),
            hex_words(&r.real),
            hex_words(&r.written),
            hex_words(&r.chunks),
            r.game_est,
            r.need,
            if r.game_est < r.need {
                "★足りない"
            } else {
                "足りる"
            }
        );
        // 踏み台（素のアセンブリ）の見積もりと、同じ式の Rust のコピー。読み戻した 10 語が本当の印と違えば、書く位置か読み戻しが
        // 崩れている。初めの値が 1.50a の素の値でなければ saverpyfix などを当てた exe。
        let mirror = game_estimate(r.base, &r.written, &r.chunks);
        if mirror != r.game_est || r.written != r.real || r.base != SAVE_EST_BASE {
            let _ = writeln!(
                self.out,
                "  ★ 見積もりの食い違い: 踏み台 0x{:X} / Rust の式 0x{mirror:X}、書いた 10 語が本当の印と{}、初めの値 0x{:X}（1.50a の素の値は 0x{SAVE_EST_BASE:X}）",
                r.game_est,
                if r.written == r.real {
                    "同じ"
                } else {
                    "★違う"
                },
                r.base
            );
        }
    }

    /// 見出しのチャージ型の行。書けなかった回は数えて、理由を ★ 付きで出す。
    fn log_charge(&mut self, c: ChargeSync) {
        let why = match c {
            ChargeSync::Written { p1, p2 } => {
                let _ = writeln!(
                    self.out,
                    "  見出しのチャージ型: 1P = {p1} / 2P = {p2} を設定の写しから見出し（+0x{REPLAY_HEADER_CHARGE:X} / +0x{:X}）へ書いた",
                    REPLAY_HEADER_CHARGE + 1
                );
                return;
            }
            ChargeSync::NotTried => "試みていない",
            ChargeSync::NoCopy => "設定の写しのポインタが 0 か読めない",
            ChargeSync::NoHeader => "見出しのポインタが 0 か読めない",
            ChargeSync::CopyUnreadable => "設定の写しの中のチャージ型が読めない",
            ChargeSync::WriteFailed => "見出しへ書けなかった",
        };
        self.charge_failures += 1;
        let _ = writeln!(
            self.out,
            "  ★ 見出しのチャージ型を合わせられなかった（{why}。この実行で {} 回目）。見出しは手元の cfg の値のまま",
            self.charge_failures
        );
    }

    fn log_skips(&mut self, h: &Handoff) {
        let now = h.skip_counts();
        if now == self.last_skips {
            return;
        }
        for (label, cur, prev) in [
            ("呼ばなかった", &now.0, &self.last_skips.0),
            ("次の tick にもう 1 回", &now.1, &self.last_skips.1),
        ] {
            for i in 0..SKIP_KINDS {
                if cur[i] != prev[i] {
                    let _ = writeln!(self.out, "{label}: {}（累計 {} 回）", SKIP_NAMES[i], cur[i]);
                }
            }
        }
        self.last_skips = now;
    }

    #[cfg(test)]
    pub fn counts(&self) -> (u32, u32) {
        (self.saves, self.marks)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::th09_addr::{
        EXE_DIR, EXE_DIR_LEN, GAME_MODE, LOADING, MATCH_END_MENU, P1_WINS, P2_WINS,
        REPLAY_HEADER_PTR, REPLAY_HEADER_STAGE_FLAGS, REPLAY_MANAGER_PTR, REPLAY_MODE,
        REPLAY_NODE_NEXT, REPLAY_SAVE_FN, REPLAY_SAVE_FRAME_SHIFT, REPLAY_SAVE_SIG,
        REPLAY_STAGE_NODES, REPLAY_STAGE_STRIDE, ROUNDS_REQUIRED, SAVE_EST_BASE,
        SAVE_EST_BASE_SITE, SAVE_EST_FLAGS_EBP,
    };
    use higanr_core::autosave::NAME_LEN;
    use higanr_core::replay_detail::{SyncKind, Winner};
    use std::sync::{Arc, Mutex};

    pub(crate) struct FakeGame {
        pub cur: u32,
        pub req: u32,
        pub menu: u8,
        pub loading: u32,
        pub mode: u32,
        pub rm: u32,
        pub replay_mode: Option<u32>,
        /// `replay_mode` を読まれた回数。
        pub mode_reads: u32,
        pub ident: Option<crate::replay_watch::HeaderIdent>,
        pub sig_ok: bool,
        pub est: Option<EstimateReal>,
        pub seen: [u32; STAGES],
        pub wins: [u32; 3],
        pub rng: [u8; RNG_BYTES],
        pub calls: u32,
        pub names: Vec<[u8; NAME_BUF]>,
        pub paths: Vec<u32>,
        pub time: Timestamp,
        pub clock: u64,
        /// ラウンドの記録の材料: `completed_rounds`・`round_frames`・席の塊・UTC の秒・境目を読んだ回数。
        pub completed: u32,
        pub round_frames: u32,
        pub sides: [Option<SideSnap>; 2],
        pub utc: u64,
        pub boundary_reads: u32,
        /// `sync_header_charge` が返す値、呼ばれた回数、保存関数が呼ばれた時点でそれが済んでいたか（確保しない形で持つ）。
        pub charge: ChargeSync,
        pub syncs: u32,
        pub synced_at_call: Option<bool>,
    }

    impl Default for FakeGame {
        fn default() -> Self {
            FakeGame {
                cur: SCENE_MATCH,
                req: SCENE_MATCH,
                menu: 0,
                loading: 0,
                mode: GAME_MODE_MATCH,
                rm: 0x0123_4560,
                replay_mode: Some(0),
                mode_reads: 0,
                ident: None,
                sig_ok: true,
                est: Some(EstimateReal {
                    base: SAVE_EST_BASE,
                    flags: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0x1EC],
                    chunks: [1; STAGES],
                }),
                seen: [0; STAGES],
                wins: [0, 2, 2],
                rng: [0x11, 0x22, 0, 0, 0, 0, 0, 0, 0x33, 0x44, 0x55, 0x66],
                calls: 0,
                names: Vec::new(),
                paths: Vec::new(),
                time: Timestamp {
                    year: 2026,
                    month: 9,
                    day: 28,
                    hour: 7,
                    minute: 8,
                    second: 9,
                },
                clock: 1_000,
                completed: 0,
                round_frames: 0,
                sides: [None; 2],
                utc: 1_790_000_000,
                boundary_reads: 0,
                charge: ChargeSync::Written { p1: 1, p2: 0 },
                syncs: 0,
                synced_at_call: None,
            }
        }
    }

    impl SaveIo for FakeGame {
        fn scenes(&mut self) -> (u32, u32) {
            (self.cur, self.req)
        }
        fn menu_flag(&mut self) -> u8 {
            self.menu
        }
        fn loading(&mut self) -> u32 {
            self.loading
        }
        fn game_mode(&mut self) -> u32 {
            self.mode
        }
        fn replay_manager(&mut self) -> u32 {
            self.rm
        }
        fn replay_mode(&mut self, _rm: u32) -> Option<u32> {
            self.mode_reads += 1;
            self.replay_mode
        }
        fn replay_ident(&mut self, _rm: u32) -> Option<crate::replay_watch::HeaderIdent> {
            self.ident
        }
        fn signature_ok(&mut self) -> bool {
            self.sig_ok
        }
        fn estimate_real(&mut self, _rm: u32) -> Option<EstimateReal> {
            self.est
        }
        fn wins(&mut self) -> [u32; 3] {
            self.wins
        }
        fn read_rng(&mut self) -> [u8; RNG_BYTES] {
            self.rng
        }
        fn write_rng(&mut self, v: &[u8; RNG_BYTES]) {
            self.rng = *v;
        }
        fn local_time(&mut self) -> Timestamp {
            self.time
        }
        fn now_us(&mut self) -> u64 {
            self.clock += 700;
            self.clock
        }
        fn unix_secs(&mut self) -> u64 {
            self.utc
        }
        fn completed_rounds(&mut self) -> u32 {
            self.completed
        }
        fn read_boundary(&mut self) -> Boundary {
            self.boundary_reads += 1;
            Boundary {
                frames: self.round_frames,
                wins: [self.wins[0], self.wins[1]],
                side: self.sides,
            }
        }
        fn sync_header_charge(&mut self, _rm: u32) -> ChargeSync {
            self.syncs += 1;
            self.charge
        }
        fn call_save(
            &mut self,
            _rm: u32,
            path_addr: u32,
            name: &[u8; NAME_BUF],
            est: &EstimateReal,
            need: u32,
        ) -> CallOut {
            self.synced_at_call = Some(self.syncs > 0);
            // 踏み台の約束のコピー: 書く前の 10 語を返し、本当の印を書いて、書いた後の 10 語で見積もる。
            let written = est.flags;
            let game_est = game_estimate(est.base, &written, &est.chunks);
            if game_est < need {
                return CallOut {
                    called: false,
                    seen: self.seen,
                    written,
                    game_est,
                };
            }
            self.calls += 1;
            self.names.push(*name);
            self.paths.push(path_addr);
            // 本物の保存は乱数を 3 回引く（状態が変わり、抽選回数 +4 の語が 3 増える）。
            self.rng[0] ^= 0x5A;
            self.rng[4] = self.rng[4].wrapping_add(3);
            CallOut {
                called: true,
                seen: self.seen,
                written,
                game_est,
            }
        }
    }

    pub(crate) fn ready(h: &Handoff) {
        h.state.store(ST_EMPTY, Ordering::SeqCst);
        assert!(h.cas(ST_EMPTY, ST_PREPARING));
        assert!(h.put_path(b"C:\\G/Replay_HiganRTool\\x\\th9_ud0001.rpy"));
        h.state.store(ST_READY, Ordering::SeqCst);
    }

    fn menu_ticks(d: &mut Driver, h: &Handoff, g: &mut FakeGame, n: u32) -> Vec<Attempt> {
        g.menu = 1;
        (0..n).filter_map(|_| d.tick(h, g, None)).collect()
    }

    #[test]
    fn signature_mismatch_means_no_call() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            sig_ok: false,
            ..FakeGame::default()
        };
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS + 10);
        assert_eq!(a, vec![Attempt::Skipped(Skip::Signature)]);
        assert_eq!(g.calls, 0);
        assert_eq!(h.state(), ST_READY, "パスを取った");
        g.req = 10;
        d.tick(&h, &mut g, None);
        g.req = SCENE_MATCH;
        g.sig_ok = true;
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(a, vec![Attempt::Skipped(Skip::Disabled)]);
        assert_eq!(g.calls, 0);
        assert_eq!(h.skip_counts().0[Skip::Signature as usize], 1);
        let mut d = Driver::new();
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 1);
    }

    /// 壊れたら落ちる: 保存の前に見出しを直さない・保存の後に直す・パスを取れない tick で直す・記録に結果を残さない。
    #[test]
    fn the_header_charge_is_synced_right_before_the_call_and_only_when_calling() {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        // パスの準備が無い（ST_READY でない）間は、保存を試みても見出しに触らない。
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(a, vec![Attempt::Retry(Skip::NotReady)]);
        assert_eq!((g.syncs, g.synced_at_call), (0, None));
        ready(&h);
        let a = menu_ticks(&mut d, &h, &mut g, 1);
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!((g.syncs, g.synced_at_call), (1, Some(true)));
        let r = h.read_record().unwrap();
        assert_eq!(r.charge, ChargeSync::Written { p1: 1, p2: 0 });
    }

    /// 壊れたら落ちる: 席の入れ替え・書けなかったのに印へ残す・失敗を数えない。
    #[test]
    fn the_mark_keeps_the_charge_that_was_written_and_a_failure_is_counted() {
        for (charge, want, failed) in [
            (ChargeSync::Written { p1: 1, p2: 0 }, Some([1, 0]), false),
            (ChargeSync::Written { p1: 0, p2: 1 }, Some([0, 1]), false),
            (ChargeSync::NoCopy, None, true),
            (ChargeSync::NoHeader, None, true),
            (ChargeSync::CopyUnreadable, None, true),
            (ChargeSync::WriteFailed, None, true),
        ] {
            let dir = temp_dir("charge_mark");
            let h = Handoff::new();
            let mut d = Driver::new();
            let mut g = FakeGame {
                charge,
                ..FakeGame::default()
            };
            d.tick(&h, &mut g, None);
            let (mut w, log) = worker(Split::Peer);
            let mut os = FakeOs::at(&dir);
            w.step(&h, &mut os);
            assert_eq!(h.state(), ST_READY, "{}", log.text());
            menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
            assert_eq!(h.state(), ST_USED);
            let f = dir.join(VAULT_DIR).join("1P vs 2P").join("th9_ud0001.rpy");
            fs::write(&f, synthetic_replay()).unwrap();
            w.step(&h, &mut os);
            let text = log.text();
            let (_, det) = read_detail(&f);
            assert_eq!(det.charge, want, "{charge:?}\n{text}");
            assert_eq!(
                text.contains("★ 見出しのチャージ型を合わせられなかった"),
                failed,
                "{charge:?}\n{text}"
            );
            if let ChargeSync::Written { p1, p2 } = charge {
                assert!(
                    text.contains(&format!("見出しのチャージ型: 1P = {p1} / 2P = {p2} を")),
                    "{text}"
                );
                assert!(text.contains("+0x190 / +0x191"), "{text}");
            } else {
                assert!(text.contains("この実行で 1 回目"), "{text}");
            }
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn rng_is_restored_after_the_call() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        let before = g.rng;
        menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(g.calls, 1);
        assert_eq!(g.rng, before, "書き戻していない");
        let r = h.read_record().unwrap();
        assert!(r.rng_restored);
        assert_eq!(r.rng[0], before);
        assert_ne!(r.rng[1], before, "呼んだ後の値が残っていない");
        assert_eq!(r.rng[1][4], before[4] + 3);
        assert_eq!(r.rng[2], before);
        assert_eq!(h.state(), ST_USED);
    }

    #[test]
    fn name_is_hh_mm_ss_and_path_is_the_box() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        let n = g.names[0];
        assert_eq!(&n[..NAME_LEN + 1], b"07:08:09\0");
        assert_eq!(n.iter().position(|&b| b == 0), Some(NAME_LEN));
        assert_eq!(g.paths, vec![h.path_addr()]);
        let r = h.read_record().unwrap();
        assert_eq!(r.at, g.time);
    }

    #[test]
    fn one_call_per_seg() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        menu_ticks(&mut d, &h, &mut g, 200);
        assert_eq!(g.calls, 1);
        ready(&h);
        g.menu = 0;
        g.req = 10; // 再戦の切替の tick
        assert_eq!(d.tick(&h, &mut g, None), None);
        assert_eq!(
            d.tick(&h, &mut g, None),
            None,
            "描画だけのフレームの繰り返し"
        );
        assert_eq!(g.calls, 1, "保険がもう 1 回呼んだ");
        g.req = SCENE_MATCH;
        g.loading = 1;
        d.tick(&h, &mut g, None);
        g.loading = 0;
        for _ in 0..100 {
            d.tick(&h, &mut g, None);
        }
        menu_ticks(&mut d, &h, &mut g, 50);
        assert_eq!(g.calls, 2, "2 試合目");
    }

    #[test]
    fn saved_this_seg_is_true_only_after_the_save_function_ran() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS - 1);
        assert!(!d.saved_this_seg(), "呼ぶ前に真");
        menu_ticks(&mut d, &h, &mut g, 1);
        assert_eq!(g.calls, 1);
        assert!(d.saved_this_seg());
        g.menu = 0;
        g.req = 6;
        assert_eq!(d.tick(&h, &mut g, None), None);
        assert!(d.saved_this_seg(), "抜ける tick に偽");
        g.req = 10;
        d.tick(&h, &mut g, None);
        g.req = SCENE_MATCH;
        d.tick(&h, &mut g, None);
        assert!(!d.saved_this_seg(), "再戦の後に前の試合の真が残った");
        ready(&h);
        menu_ticks(&mut d, &h, &mut g, 2);
        g.menu = 0;
        g.req = 6;
        assert_eq!(d.tick(&h, &mut g, None), Some(Attempt::Recorded));
        assert!(d.saved_this_seg(), "保険で呼んだ tick に偽");
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            est: Some(EstimateReal {
                base: 0x100,
                flags: [0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                chunks: [1, 1, 1, 1, 1, 1, 1, 1, 1, 4],
            }),
            ..FakeGame::default()
        };
        assert_eq!(
            menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS),
            vec![Attempt::Recorded]
        );
        assert_eq!(g.calls, 0);
        assert!(!d.saved_this_seg(), "呼ばなかったのに真");
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS + 3);
        g.menu = 0;
        g.req = 6;
        d.tick(&h, &mut g, None);
        assert_eq!(g.calls, 0);
        assert!(!d.saved_this_seg(), "準備が間に合わなかったのに真");
    }

    #[test]
    fn leaving_insurance_and_its_refusals() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        for _ in 0..30 {
            d.tick(&h, &mut g, None);
        }
        g.req = 1;
        assert_eq!(
            d.tick(&h, &mut g, None),
            Some(Attempt::Skipped(Skip::Unfinished)),
            "メニューを見ない抜けは保存しない"
        );
        assert_eq!(g.calls, 0);
        assert_eq!(h.state(), ST_READY, "記録が出た（保存の試みに入った）");
        assert_eq!(h.state(), ST_READY, "予約が壊れた");
        assert_eq!(h.skip_counts().0[Skip::Unfinished as usize], 1);
        assert!(!d.saved_this_seg());
        // 同じ抜ける tick の繰り返し（描画だけのフレーム）では数え直さない。
        assert_eq!(d.tick(&h, &mut g, None), None);
        assert_eq!(h.skip_counts().0[Skip::Unfinished as usize], 1);
        // メニューを見た（5 tick。`MENU_SETTLE_TICKS` に届かない）後で抜けたマッチは、保険で保存する（勝ち数で決める）。
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        for _ in 0..30 {
            d.tick(&h, &mut g, None);
        }
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS - 1);
        assert!(a.is_empty());
        g.menu = 0;
        g.req = 1;
        assert_eq!(d.tick(&h, &mut g, None), Some(Attempt::Recorded));
        let r = h.read_record().unwrap();
        assert_eq!(r.kind, SaveKind::Leaving { menu_seen: true });
        assert_eq!(r.result, MatchResult::Lose, "勝ち数 0/2/2 の 1P");
        assert_eq!(g.calls, 1);
        h.state.store(ST_EMPTY, Ordering::SeqCst);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        d.tick(&h, &mut g, None);
        menu_ticks(&mut d, &h, &mut g, 2);
        g.menu = 0;
        g.req = 1;
        assert_eq!(
            d.tick(&h, &mut g, None),
            Some(Attempt::Skipped(Skip::NotReady))
        );
        ready(&h);
        g.cur = 1;
        for _ in 0..10 {
            assert_eq!(d.tick(&h, &mut g, None), None);
        }
        assert_eq!(g.calls, 0);
        type Break = fn(&mut FakeGame);
        let cases: [(Break, Skip); 5] = [
            (|g| g.mode = 0, Skip::NotMatch),
            (|g| g.replay_mode = Some(1), Skip::Playback),
            (|g| g.replay_mode = None, Skip::ModeUnreadable),
            (|g| g.rm = 0, Skip::NoManager),
            (|g| g.est = None, Skip::EstimateUnreadable),
        ];
        for (brk, want) in cases {
            let h = Handoff::new();
            ready(&h);
            let mut d = Driver::new();
            let mut g = FakeGame::default();
            brk(&mut g);
            let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS + 5);
            assert_eq!(a, vec![Attempt::Skipped(want)], "{want:?}");
            assert_eq!(g.calls, 0, "{want:?}");
            assert_eq!(h.state(), ST_READY, "{want:?}");
        }
    }

    #[test]
    fn menu_retries_until_the_path_is_ready() {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS + 2);
        assert_eq!(a, vec![Attempt::Retry(Skip::NotReady); 3]);
        ready(&h);
        g.loading = 1;
        // 読み込み中の印は数えない（場面の読みとして）ので、メニューの数えから始め直す。
        assert!(menu_ticks(&mut d, &h, &mut g, 3).is_empty());
        g.loading = 0;
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 1);
        assert_eq!(h.skip_counts().1[Skip::NotReady as usize], 3);
    }

    #[test]
    fn long_match_is_saved_and_a_short_estimate_is_still_refused() {
        let long = EstimateReal {
            base: SAVE_EST_BASE,
            flags: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0BB4_79F8],
            chunks: [1, 1, 1, 1, 1, 1, 1, 1, 1, 4],
        };
        for base in [SAVE_EST_BASE, 0x801EC] {
            let h = Handoff::new();
            ready(&h);
            let mut d = Driver::new();
            let mut g = FakeGame {
                est: Some(EstimateReal { base, ..long }),
                ..FakeGame::default()
            };
            let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
            assert_eq!(a, vec![Attempt::Recorded]);
            assert_eq!(g.calls, 1, "0x{base:X}: 4 塊の試合を呼ばない");
            let r = h.read_record().unwrap();
            assert!(r.called);
            assert_eq!(r.seen, [0; STAGES], "書く前の 10 語");
            assert_eq!(r.written, long.flags, "書いた 10 語");
            assert_eq!(r.game_est, base + 0x60 + 4 * 0x54D9);
            assert_eq!(r.need, 0x1EC + 0x60 + 4 * 0x54D9);
            assert_eq!(game_estimate(r.base, &r.seen, &r.chunks), base, "書く前");
        }
        // 書いた後でも足りない（初めの値がヘッダより小さい、とする）: 呼ばずに記録を出す。
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            est: Some(EstimateReal {
                base: 0x100,
                ..long
            }),
            ..FakeGame::default()
        };
        let a = menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 0, "足りないのに呼んだ");
        let r = h.read_record().unwrap();
        assert!(!r.called);
        assert!(r.game_est < r.need);
        assert_eq!(r.written, long.flags);
    }

    #[test]
    fn estimate_formulas() {
        let mut flags = [0u32; STAGES];
        let mut chunks = [1u32; STAGES];
        assert_eq!(game_estimate(0x101EC, &flags, &chunks), 0x101EC);
        flags[9] = 1;
        chunks[9] = 5;
        assert_eq!(
            game_estimate(0x101EC, &flags, &chunks),
            0x101EC + 0x60 + 5 * 0x54D9
        );
        flags[0] = 7;
        assert_eq!(
            game_estimate(0x101EC, &flags, &chunks),
            0x101EC + 2 * 0x60 + 6 * 0x54D9
        );
        assert_eq!(need_bound(&flags, &chunks), 0x1EC + 2 * 0x60 + 6 * 0x54D9);
        chunks[3] = u32::MAX;
        flags[3] = 1;
        let wrapped = 0x101EC_u32
            .wrapping_add(3 * 0x60)
            .wrapping_add((6u32).wrapping_mul(0x54D9))
            .wrapping_add(u32::MAX.wrapping_mul(0x54D9));
        assert_eq!(game_estimate(0x101EC, &flags, &chunks), wrapped);
        assert_eq!(need_bound(&flags, &chunks), u32::MAX);
    }

    #[test]
    fn record_words_round_trip() {
        let r = SaveRecord {
            kind: SaveKind::Leaving { menu_seen: true },
            called: true,
            rng_restored: false,
            result: MatchResult::Lose,
            side: Side::P2,
            lat: 7,
            ping_ms: 123_456,
            at: Timestamp {
                year: 2031,
                month: 12,
                day: 30,
                hour: 23,
                minute: 1,
                second: 59,
            },
            duration_us: 98_765,
            base: 0x801EC,
            game_est: 0xABCDE,
            need: 0x12345,
            seen: core::array::from_fn(|i| 0x1000 + i as u32),
            real: core::array::from_fn(|i| 0x2000 + i as u32 * 3),
            written: core::array::from_fn(|i| 0x3000 + i as u32 * 5),
            chunks: core::array::from_fn(|i| 1 + i as u32 * 7),
            rng: [
                core::array::from_fn(|i| i as u8),
                core::array::from_fn(|i| 0x40 + i as u8),
                core::array::from_fn(|i| 0x80 + i as u8),
            ],
            manager: 0x0BAD_F00D,
            wins: [1, 2, 3],
            charge: ChargeSync::Written { p1: 0, p2: 1 },
        };
        assert_eq!(SaveRecord::from_words(&r.to_words()), Some(r));
        let mut w = r.to_words();
        w[0] = (w[0] & !0xFF00) | 9 << 8;
        assert_eq!(SaveRecord::from_words(&w), None, "範囲の外の結果");
    }

    #[test]
    fn info_is_published_on_change() {
        let h = Handoff::new();
        assert_eq!(h.read_info(), None);
        let mut d = Driver::new();
        let mut g = FakeGame {
            cur: 1,
            ..FakeGame::default()
        };
        d.tick(&h, &mut g, None);
        d.tick(&h, &mut g, None);
        let (gen1, i1) = h.read_info().unwrap();
        assert_eq!(gen1, 1);
        assert!(!i1.net);
        let nv = NetView {
            seat: Seat::P2,
            own_name: Name::new("じぶん"),
            peer_name: Name::new("あいて"),
            delay: 3,
            rtt_ms: Some(40),
            desynced: false,
            rb_ready: false,
            rollback: false,
            match_no: 1,
            totals: NetTotals::default(),
        };
        d.tick(&h, &mut g, Some(nv));
        d.tick(
            &h,
            &mut g,
            Some(NetView {
                rtt_ms: Some(41),
                desynced: false,
                delay: 4,
                ..nv
            }),
        );
        let (gen2, i2) = h.read_info().unwrap();
        assert_eq!(gen2, 2, "lat と ping だけの変化で出し直した");
        assert!(i2.net);
        assert_eq!(i2.seat, Seat::P2);
        assert_eq!(i2.peer.as_str(), "あいて");
    }

    #[test]
    fn game_thread_path_does_not_allocate() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        g.names.reserve(4);
        g.paths.reserve(4);
        let nv = NetView {
            seat: Seat::P1,
            own_name: Name::new("o"),
            peer_name: Name::new("p"),
            delay: 2,
            rtt_ms: None,
            desynced: false,
            rb_ready: false,
            rollback: false,
            match_no: 1,
            totals: NetTotals::default(),
        };
        let a0 = crate::test_alloc::count();
        g.menu = 1;
        for _ in 0..(MENU_SETTLE_TICKS + 3) {
            d.tick(&h, &mut g, Some(nv));
        }
        g.req = 10;
        d.tick(&h, &mut g, None);
        let a1 = crate::test_alloc::count();
        assert_eq!(g.calls, 1, "保存の経路を通っていない");
        assert_eq!(a1, a0, "ゲームのスレッドの経路で確保した");
    }

    #[test]
    fn the_watcher_path_on_the_game_thread_does_not_allocate() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        g.names.reserve(4);
        g.paths.reserve(4);
        let waiting = Watching::WaitingForNames;
        let named = names_of("いちろう", "じろう");
        let a0 = crate::test_alloc::count();
        g.menu = 1;
        for _ in 0..(MENU_SETTLE_TICKS + 2) {
            d.tick_watching(&h, &mut g, None, waiting);
        }
        for _ in 0..(MENU_SETTLE_TICKS + 3) {
            d.tick_watching(&h, &mut g, None, named);
        }
        g.req = 10;
        d.tick_watching(&h, &mut g, None, named);
        let a1 = crate::test_alloc::count();
        assert_eq!(g.calls, 1, "保存の経路を通っていない");
        assert_eq!(a1, a0, "観戦者の経路でゲームのスレッドが確保した");
    }

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "higanrtool_replay_save_{tag}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 検査の「exe のフォルダ」（一時ディレクトリ）と、保存関数の `CreateFileA` の代わり。
    pub(crate) struct FakeOs {
        exe_box: Option<Vec<u8>>,
        same: Result<bool, String>,
        now: Timestamp,
        asked: Vec<Vec<u8>>,
        touched: Vec<PathBuf>,
    }

    impl FakeOs {
        pub(crate) fn at(dir: &Path) -> FakeOs {
            let mut b = vec![0u8; EXE_DIR_LEN as usize];
            let s = format!("{}/", dir.display());
            b[..s.len()].copy_from_slice(s.as_bytes());
            FakeOs {
                exe_box: Some(b),
                same: Ok(true),
                now: Timestamp {
                    year: 2026,
                    month: 9,
                    day: 28,
                    hour: 1,
                    minute: 2,
                    second: 3,
                },
                asked: Vec::new(),
                touched: Vec::new(),
            }
        }
    }

    impl VaultOs for FakeOs {
        fn exe_dir_box(&mut self) -> Option<Vec<u8>> {
            self.exe_box.clone()
        }
        fn same_file_ansi(&mut self, ansi: &[u8], _file: &fs::File) -> Result<bool, String> {
            self.asked.push(ansi.to_vec());
            self.same.clone()
        }
        fn now(&mut self) -> Timestamp {
            self.now
        }
        fn touch(&mut self, file: &Path) -> Result<(), String> {
            self.touched.push(file.to_path_buf());
            set_age(file, 0);
            Ok(())
        }
    }

    pub(crate) fn set_age(file: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        OpenOptions::new()
            .write(true)
            .open(file)
            .and_then(|f| f.set_modified(t))
            .unwrap();
    }

    #[derive(Clone, Default)]
    pub(crate) struct Log(Arc<Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Log {
        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// 合成のリプレイ（平文の部分と USER が 1 つ。`replay.rs` の検査と同じ形を組み立てる）。
    pub(crate) fn synthetic_replay() -> Vec<u8> {
        let comp = 0xC0u32;
        let mut b = vec![0u8; comp as usize];
        b[..4].copy_from_slice(b"T9RP");
        b[0x0C..0x10].copy_from_slice(&comp.to_le_bytes());
        for (i, x) in b.iter_mut().enumerate().skip(0x18) {
            *x = (i * 7) as u8;
        }
        let body = b"GAME\0\0";
        b.extend_from_slice(b"USER");
        b.extend_from_slice(&((12 + body.len()) as u32).to_le_bytes());
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(body);
        b
    }

    fn worker(split: Split) -> (Worker, Log) {
        let log = Log::default();
        (
            Worker::new(split, "じぶん".to_string(), false, Box::new(log.clone())),
            log,
        )
    }

    pub(crate) fn worker_net(split: Split) -> (Worker, Log) {
        let log = Log::default();
        (
            Worker::new(split, "じぶん".to_string(), true, Box::new(log.clone())),
            log,
        )
    }

    fn publish(h: &Handoff, net: Option<NetView>) {
        Driver::new().tick(h, &mut FakeGame::default(), net);
        assert!(h.wanted());
    }

    #[test]
    fn worker_reserves_only_for_a_match_seg() {
        let dir = temp_dir("want");
        let h = Handoff::new();
        let (mut w, _) = worker(Split::Flat);
        let mut os = FakeOs::at(&dir);
        let mut d = Driver::new();
        let mut g = FakeGame {
            cur: 1,
            ..FakeGame::default()
        };
        d.tick(&h, &mut g, None);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_EMPTY, "タイトルで押さえた");
        g.cur = SCENE_MATCH;
        g.mode = 0;
        d.tick(&h, &mut g, None);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_EMPTY, "Story で押さえた");
        assert!(!dir.join(VAULT_DIR).exists());
        g.mode = GAME_MODE_MATCH;
        d.tick(&h, &mut g, None);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "Match で押さえない");
        menu_ticks(&mut d, &h, &mut g, MENU_SETTLE_TICKS);
        assert_eq!(h.state(), ST_USED);
        fs::write(
            dir.join(VAULT_DIR).join("th9_ud0001.rpy"),
            synthetic_replay(),
        )
        .unwrap();
        w.step(&h, &mut os);
        d.tick(&h, &mut g, None);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_EMPTY, "済んだ seg で次を押さえた");
        assert_eq!(list_names(&dir.join(VAULT_DIR), false).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_reserves_the_first_free_name() {
        let dir = temp_dir("free");
        let folder = dir.join(VAULT_DIR).join("1P vs 2P");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("th9_ud0001.rpy"), b"x").unwrap();
        fs::write(folder.join("TH9_UD0002.RPY"), b"x").unwrap();
        fs::write(folder.join("th9_ud0004.rpy"), b"x").unwrap();
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, log) = worker(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        let want = format!(
            "{}/Replay_HiganRTool\\1P vs 2P\\th9_ud0003.rpy",
            dir.display()
        );
        assert_eq!(h.path_bytes(), want.as_bytes());
        assert_eq!(os.asked, vec![want.as_bytes().to_vec()]);
        let f = folder.join("th9_ud0003.rpy");
        assert_eq!(fs::metadata(&f).unwrap().len(), 0, "押さえていない");
        assert_eq!(fs::read(folder.join("th9_ud0001.rpy")).unwrap(), b"x");
        w.step(&h, &mut os);
        assert_eq!(h.path_bytes(), want.as_bytes());
        assert_eq!(os.asked.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_refuses_mismatched_paths() {
        let dir = temp_dir("mismatch");
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, log) = worker(Split::Flat);
        let mut os = FakeOs::at(&dir);
        os.same = Ok(false);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_EMPTY);
        assert!(log.text().contains("別のファイルになる"), "{}", log.text());
        assert_eq!(h.outcome_counts()[Outcome::PrepFailed as usize], 1);
        assert_eq!(h.outcome_counts()[Outcome::Saved as usize], 0);
        assert!(
            list_names(&dir.join(VAULT_DIR), false).unwrap().is_empty(),
            "押さえた名前を残した"
        );
        for bx in [b"./\0".to_vec(), vec![0u8; 8]] {
            let h = Handoff::new();
            publish(&h, None);
            let (mut w, log) = worker(Split::Flat);
            let d2 = temp_dir("relative");
            let mut os = FakeOs::at(&d2);
            os.exe_box = Some(bx);
            w.step(&h, &mut os);
            assert_eq!(h.state(), ST_EMPTY);
            assert!(log.text().contains("形が違う"), "{}", log.text());
            assert!(!d2.join(VAULT_DIR).exists());
            let _ = fs::remove_dir_all(&d2);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_marks_after_the_save() {
        let dir = temp_dir("mark");
        let h = Handoff::new();
        let nv = NetView {
            seat: Seat::P2,
            own_name: Name::new("じぶん"),
            peer_name: Name::new("あいて"),
            delay: 3,
            rtt_ms: Some(42),
            desynced: false,
            rb_ready: false,
            rollback: false,
            match_no: 1,
            totals: NetTotals::default(),
        };
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        d.tick(&h, &mut g, Some(nv));
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        let folder = dir.join(VAULT_DIR).join("あいて vs じぶん");
        let f1 = folder.join("th9_ud0001.rpy");
        g.menu = 1;
        for _ in 0..MENU_SETTLE_TICKS {
            d.tick(&h, &mut g, Some(nv));
        }
        assert_eq!(h.state(), ST_USED);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY);
        assert!(log.text().contains("書かなかった"), "{}", log.text());
        assert_eq!(h.path_bytes(), os.asked[0], "同じ名前を次にも使う");
        g.menu = 0;
        g.req = 10;
        d.tick(&h, &mut g, Some(nv));
        g.req = SCENE_MATCH;
        g.wins = [2, 1, 2];
        g.menu = 1;
        for _ in 0..MENU_SETTLE_TICKS {
            d.tick(&h, &mut g, Some(nv));
        }
        fs::write(&f1, synthetic_replay()).unwrap();
        w.step(&h, &mut os);
        let text = log.text();
        assert!(text.contains("印: "), "{text}");
        let m = match replay::read_mark(&fs::read(&f1).unwrap()).unwrap() {
            MarkRead::Found(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(m.peer, "あいて");
        assert_eq!(m.self_name, "じぶん");
        assert_eq!(m.side, Side::P2);
        assert_eq!(m.result, MatchResult::Lose, "2P から見て 1P の 2 勝");
        assert_eq!((m.lat, m.ping_ms), (3, 42));
        assert_eq!(m.at, g.time);
        // 平文の部分は 1 バイトも変えない。
        assert_eq!(&fs::read(&f1).unwrap()[..0xC0], &synthetic_replay()[..0xC0]);
        assert_eq!(h.state(), ST_EMPTY);
        g.menu = 0;
        g.req = 10;
        d.tick(&h, &mut g, Some(nv));
        g.req = SCENE_MATCH;
        d.tick(&h, &mut g, Some(nv));
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY);
        assert!(h.path_bytes().ends_with(b"th9_ud0002.rpy"));
        assert_eq!(w.counts(), (2, 1));
        let mut want = [0u32; OUTCOME_KINDS];
        want[Outcome::Saved as usize] = 1;
        want[Outcome::EmptyFile as usize] = 1;
        assert_eq!(h.outcome_counts(), want);
        assert!(text.contains("乱数: 前 [11 22 00"), "{text}");
        assert!(text.contains("見積もり: 初めの値 0x101EC"), "{text}");
        assert!(
            text.contains("書いた 10 語（読み戻し）[0 0 0 0 0 0 0 0 0 1EC]"),
            "{text}"
        );
        assert!(
            text.contains("書く前のゲームの見積もり 0x101EC、"),
            "{text}"
        );
        assert!(!text.contains("★ 見積もりの食い違い"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 壊れたら落ちる: 保存の失敗の理由が、簡潔なログ（ワーカーのログを捨てる）に流れない。失敗でない行が流れる。
    #[test]
    fn save_failures_go_to_the_problem_sink_as_well_as_the_worker_log() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (w, log) = worker(Split::Peer);
        let sink = Arc::clone(&seen);
        let mut w = w.with_problem_sink(Box::new(move |l| sink.lock().unwrap().push(l)));
        w.problem("★ 印を足せない（理由）: th9_ud0001.rpy".to_string());
        assert!(
            log.text().contains("★ 印を足せない（理由）"),
            "{}",
            log.text()
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["自動保存: ★ 印を足せない（理由）: th9_ud0001.rpy".to_string()]
        );
        // 失敗の 5 か所（準備の失敗・記録が読めない・バックアップが無い・0 バイト・印を足せない）が全部この入口を通る。
        let src = include_str!("replay_save.rs");
        let body = src
            .split("\n#[cfg(test)]\npub(crate) mod tests")
            .next()
            .unwrap();
        assert_eq!(body.matches("self.problem(").count(), 5);
        assert!(
            !body.contains("writeln!(self.out, \"★"),
            "★ の行を入口を通さずに書いている"
        );
    }

    fn watch_tick(
        d: &mut Driver,
        h: &Handoff,
        g: &mut FakeGame,
        watching: Watching,
    ) -> Option<Attempt> {
        d.tick_watching(h, g, None, watching)
    }

    /// 合成の名前（実在の名前でない）。
    fn names_of(p1: &str, p2: &str) -> Watching {
        Watching::Names([Name::new(p1), Name::new(p2)])
    }

    #[test]
    fn a_watcher_saves_into_the_1p_vs_2p_folder_with_a_watch_mark() {
        let dir = temp_dir("watch_save");
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        let watching = names_of("いちろう", "じろう");
        watch_tick(&mut d, &h, &mut g, watching);
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        assert_eq!(vault_files(&dir), vec!["いちろう vs じろう/th9_ud0001.rpy"]);
        let text = log.text();
        assert!(text.contains("観戦の試合の名前を受けた"), "{text}");
        // ワーカーのログには名前も、名前を含むフォルダも書かない。
        assert!(
            !text.contains("いちろう") && !text.contains("じろう"),
            "{text}"
        );
        g.wins = [1, 2, 2];
        g.menu = 1;
        let a: Vec<_> = (0..MENU_SETTLE_TICKS)
            .filter_map(|_| watch_tick(&mut d, &h, &mut g, watching))
            .collect();
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 1);
        let f1 = dir
            .join(VAULT_DIR)
            .join("いちろう vs じろう")
            .join("th9_ud0001.rpy");
        fs::write(&f1, synthetic_replay()).unwrap();
        w.step(&h, &mut os);
        assert!(log.text().contains("印: "), "{}", log.text());
        let MarkRead::Found(m) = replay::read_mark(&fs::read(&f1).unwrap()).unwrap() else {
            panic!("印が読めない");
        };
        assert_eq!(
            m.role,
            MarkRole::Watch {
                p1: "いちろう".to_string(),
                p2: "じろう".to_string()
            }
        );
        assert_eq!(m.self_name, "じぶん", "観戦した人の名前");
        assert_eq!(m.peer, "");
        assert_eq!((m.side, m.lat, m.ping_ms), (Side::P1, 0, 0));
        assert_eq!(m.result, MatchResult::Lose, "1P から見た結果（2P の 2 勝）");
        // 再戦（次の seg）でも、名前はそのまま次のパスを準備する（2 本目は同じフォルダの th9_ud0002）。
        g.menu = 0;
        g.req = 10;
        watch_tick(&mut d, &h, &mut g, watching);
        g.req = SCENE_MATCH;
        watch_tick(&mut d, &h, &mut g, watching);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        assert!(h.path_bytes().ends_with(b"th9_ud0002.rpy"));
        assert_eq!(
            vault_files(&dir),
            vec![
                "いちろう vs じろう/th9_ud0001.rpy",
                "いちろう vs じろう/th9_ud0002.rpy"
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_watcher_without_names_skips_for_the_real_reason_and_saves_once_names_arrive() {
        let dir = temp_dir("watch_nonames");
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        g.menu = 1;
        let a: Vec<_> = (0..MENU_SETTLE_TICKS + 1)
            .filter_map(|_| watch_tick(&mut d, &h, &mut g, Watching::WaitingForNames))
            .collect();
        for _ in 0..3 {
            w.step(&h, &mut os);
        }
        assert_eq!(a, vec![Attempt::Retry(Skip::WatchNoNames); 2]);
        assert_eq!(g.calls, 0, "名前の無い試合で保存関数を呼んだ");
        assert_eq!(h.state(), ST_EMPTY, "名前が無いのに名前を押さえた");
        assert!(vault_files(&dir).is_empty(), "{:?}", vault_files(&dir));
        let (skips, retries) = h.skip_counts();
        assert_eq!(retries[Skip::WatchNoNames as usize], 2);
        assert_eq!(
            retries[Skip::NotReady as usize],
            0,
            "本当の訳でなく準備の遅れと数えた"
        );
        assert_eq!(skips, [0; SKIP_KINDS]);
        let text = log.text();
        assert!(
            text.contains("次の tick にもう 1 回: 観戦の試合の名前をまだ受けていない（累計 2 回）"),
            "{text}"
        );
        assert!(!text.contains("パスの準備が間に合わない"), "{text}");
        assert!(
            text.contains("観戦者の走り: 観戦の試合の名前をまだ受けていない"),
            "{text}"
        );
        let watching = names_of("A", "B");
        watch_tick(&mut d, &h, &mut g, watching);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        let a: Vec<_> = (0..MENU_SETTLE_TICKS)
            .filter_map(|_| watch_tick(&mut d, &h, &mut g, watching))
            .collect();
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 1);
        let h2 = Handoff::new();
        let mut d2 = Driver::new();
        let mut g2 = FakeGame::default();
        let a = menu_ticks(&mut d2, &h2, &mut g2, MENU_SETTLE_TICKS);
        assert_eq!(a, vec![Attempt::Retry(Skip::NotReady)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn info_carries_the_watch_state_and_both_names() {
        let h = Handoff::new();
        let long = "あいうえおかきくけこ12"; // 30 + 2 = 32 バイト
        assert_eq!(long.len(), 32);
        for watching in [
            Watching::No,
            Watching::WaitingForNames,
            names_of("一", "ツー"),
            names_of(long, ""),
            names_of("", long),
        ] {
            for net in [None, Some(net_view("あいて"))] {
                let info = MatchInfo::of(net, watching);
                h.publish_info(&info);
                let (_, got) = h.read_info().unwrap();
                assert_eq!(got, info, "{watching:?} {net:?}");
            }
        }
    }

    #[test]
    fn worker_reprepares_when_names_change_and_pack50() {
        let dir = temp_dir("reprep");
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, log) = worker(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        let old = dir.join(VAULT_DIR).join("1P vs 2P").join("th9_ud0001.rpy");
        assert!(old.exists());
        let nv = NetView {
            seat: Seat::P1,
            own_name: Name::new("じぶん"),
            peer_name: Name::new("B"),
            delay: 2,
            rtt_ms: None,
            desynced: false,
            rb_ready: false,
            rollback: false,
            match_no: 1,
            totals: NetTotals::default(),
        };
        publish(&h, Some(nv));
        w.step(&h, &mut os);
        assert!(!old.exists(), "放していない: {}", log.text());
        let want = vault::to_cp932("Replay_HiganRTool\\じぶん vs B\\th9_ud0001.rpy").unwrap();
        assert!(
            h.path_bytes().ends_with(&want),
            "{:?}",
            String::from_utf8_lossy(&h.path_bytes())
        );
        let d2 = temp_dir("pack50");
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, _) = worker(Split::Pack50);
        let mut os = FakeOs::at(&d2);
        w.step(&h, &mut os);
        assert!(
            h.path_bytes()
                .ends_with(b"Replay_HiganRTool\\0001\\th9_01.rpy")
        );
        assert!(d2.join(VAULT_DIR).join("0001").join("th9_01.rpy").exists());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn worker_sweeps_stale_reservations_before_choosing() {
        let dir = temp_dir("sweep");
        let folder = dir.join(VAULT_DIR).join("1P vs 2P");
        fs::create_dir_all(&folder).unwrap();
        let old = STALE_RESERVATION_SECS + 60;
        let put = |name: &str, body: &[u8], age: u64| {
            let p = folder.join(name);
            fs::write(&p, body).unwrap();
            set_age(&p, age);
        };
        put("th9_ud0001.rpy", b"", old);
        put("TH9_UD0002.RPY", b"", old);
        put("th9_ud0003.rpy", b"", STALE_RESERVATION_SECS - 300);
        put("th9_ud0004.rpy", b"x", old);
        put("th9_05.rpy", b"", old);
        put("other.txt", b"", old);
        fs::create_dir_all(folder.join("th9_ud0006.rpy")).unwrap();
        let outside = dir.join("th9_ud0001.rpy");
        fs::write(&outside, b"").unwrap();
        set_age(&outside, old);
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, log) = worker(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        let text = log.text();
        assert!(
            text.contains("を 2 件消した（TH9_UD0002.RPY th9_ud0001.rpy）、消せなかった 0 件"),
            "{text}"
        );
        assert!(h.path_bytes().ends_with(b"\\1P vs 2P\\th9_ud0001.rpy"));
        assert_eq!(
            fs::metadata(folder.join("th9_ud0001.rpy")).unwrap().len(),
            0
        );
        assert!(!folder.join("TH9_UD0002.RPY").exists());
        for kept in [
            "th9_ud0003.rpy",
            "th9_ud0004.rpy",
            "th9_05.rpy",
            "other.txt",
        ] {
            assert!(folder.join(kept).exists(), "{kept} を消した");
        }
        assert!(folder.join("th9_ud0006.rpy").is_dir());
        assert!(outside.exists(), "保管庫の外を消した");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_spares_its_own_and_stays_inside_the_vault() {
        let dir = temp_dir("sweep_own");
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, _) = worker(Split::Flat);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        let root = dir.join(VAULT_DIR);
        let own = root.join("th9_ud0001.rpy");
        assert!(own.exists());
        set_age(&own, STALE_RESERVATION_SECS + 60);
        let now = std::time::SystemTime::now();
        assert_eq!(w.sweep_stale(&root, &root, now), (Vec::new(), 0));
        assert!(own.exists(), "自分が押さえた名前を消した");
        let other = Worker::new(Split::Flat, String::new(), false, Box::new(Log::default()));
        assert_eq!(
            other.sweep_stale(&root, &root, now),
            (vec!["th9_ud0001.rpy".to_string()], 0)
        );
        let outside = dir.join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        let f = outside.join("th9_ud0009.rpy");
        fs::write(&f, b"").unwrap();
        set_age(&f, STALE_RESERVATION_SECS + 60);
        assert_eq!(other.sweep_stale(&outside, &root, now), (Vec::new(), 0));
        assert!(f.exists(), "保管庫の外を消した");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_keeps_its_reservation_fresh() {
        let dir = temp_dir("touch");
        let h = Handoff::new();
        publish(&h, None);
        let (mut w, _) = worker(Split::Flat);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY);
        for _ in 1..TOUCH_STEPS {
            w.step(&h, &mut os);
        }
        assert!(os.touched.is_empty(), "{} 回で新しくした", os.touched.len());
        w.step(&h, &mut os);
        assert_eq!(os.touched, vec![dir.join(VAULT_DIR).join("th9_ud0001.rpy")]);
        for _ in 0..TOUCH_STEPS {
            w.step(&h, &mut os);
        }
        assert_eq!(os.touched.len(), 2);
        let d2 = temp_dir("touch_none");
        let h2 = Handoff::new();
        let (mut w2, _) = worker(Split::Flat);
        let mut os2 = FakeOs::at(&d2);
        for _ in 0..=TOUCH_STEPS {
            w2.step(&h2, &mut os2);
        }
        assert!(os2.touched.is_empty());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&d2);
    }

    pub(crate) fn vault_files(dir: &Path) -> Vec<String> {
        fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
            let Ok(rd) = fs::read_dir(d) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    out.push(
                        p.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let root = dir.join(VAULT_DIR);
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        out.sort();
        out
    }

    fn sample_record() -> SaveRecord {
        SaveRecord {
            kind: SaveKind::Leaving { menu_seen: true },
            called: true,
            rng_restored: true,
            result: MatchResult::Win,
            side: Side::P1,
            lat: 1,
            ping_ms: 10,
            at: Timestamp {
                year: 2031,
                month: 1,
                day: 2,
                hour: 3,
                minute: 4,
                second: 5,
            },
            duration_us: 0,
            base: 0,
            game_est: 0,
            need: 0,
            seen: [0; STAGES],
            real: [0; STAGES],
            written: [0; STAGES],
            chunks: [0; STAGES],
            rng: [[0; RNG_BYTES]; 3],
            manager: 0,
            wins: [2, 0, 2],
            charge: ChargeSync::Written { p1: 1, p2: 0 },
        }
    }

    #[test]
    fn the_folder_and_mark_use_the_name_given_in_the_hello_not_the_startup_name() {
        let dir = temp_dir("own_name");
        let h = Handoff::new();
        let mut nv = net_view("あいて");
        nv.own_name = Name::new("かえた");
        publish(&h, Some(nv));
        let (_, i) = h.read_info().unwrap();
        assert_eq!(
            (i.own.as_str(), i.peer.as_str()),
            ("かえた", "あいて"),
            "情報の出し入れ"
        );
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        let want = vault::to_cp932("Replay_HiganRTool\\かえた vs あいて\\th9_ud0001.rpy").unwrap();
        assert!(
            h.path_bytes().ends_with(&want),
            "{:?} / {}",
            String::from_utf8_lossy(&h.path_bytes()),
            log.text()
        );
        nv.seat = Seat::P2;
        let info = MatchInfo::of(Some(nv), Watching::No);
        assert_eq!(w.pair(&info), ("あいて".to_string(), "かえた".to_string()));
        assert_eq!(
            w.mark_of(&info, &sample_record(), &MatchSummary::EMPTY)
                .self_name,
            "かえた"
        );
        nv.own_name = Name::EMPTY;
        let info = MatchInfo::of(Some(nv), Watching::No);
        assert_eq!(w.pair(&info), ("あいて".to_string(), "じぶん".to_string()));
        // 観戦者: 1P・2P の名前のまま。印の「自分」は起動のときの名前。
        let names = [Name::new("いち"), Name::new("に")];
        let info = MatchInfo::of(None, Watching::Names(names));
        assert_eq!(w.pair(&info), ("いち".to_string(), "に".to_string()));
        assert_eq!(
            w.mark_of(&info, &sample_record(), &MatchSummary::EMPTY)
                .self_name,
            "じぶん"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn net_view(peer: &str) -> NetView {
        NetView {
            seat: Seat::P1,
            own_name: Name::new("じぶん"),
            peer_name: Name::new(peer),
            delay: 1,
            rtt_ms: Some(1),
            desynced: false,
            rb_ready: false,
            rollback: false,
            match_no: 1,
            totals: NetTotals::default(),
        }
    }

    #[test]
    fn a_watcher_does_not_save_a_match_that_ended_before_the_menu() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        let watching = names_of("いちろう", "じろう");
        for _ in 0..30 {
            watch_tick(&mut d, &h, &mut g, watching);
        }
        g.req = 1;
        assert_eq!(
            watch_tick(&mut d, &h, &mut g, watching),
            Some(Attempt::Skipped(Skip::Unfinished))
        );
        assert_eq!(g.calls, 0);
        assert_eq!(h.skip_counts().0[Skip::Unfinished as usize], 1);
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            menu: 1,
            ..FakeGame::default()
        };
        let a: Vec<_> = (0..MENU_SETTLE_TICKS)
            .filter_map(|_| watch_tick(&mut d, &h, &mut g, watching))
            .collect();
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 1);
    }

    #[test]
    fn a_desynced_match_is_not_saved_even_after_the_menu() {
        let stopped = NetView {
            desynced: true,
            ..net_view("あいて")
        };
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            menu: 1,
            ..FakeGame::default()
        };
        let a: Vec<_> = (0..MENU_SETTLE_TICKS + 3)
            .filter_map(|_| d.tick(&h, &mut g, Some(stopped)))
            .collect();
        assert_eq!(a, vec![Attempt::Skipped(Skip::Desynced)]);
        assert_eq!(g.calls, 0);
        assert_eq!(h.state(), ST_READY, "記録が出た");
        assert_eq!(h.skip_counts().0[Skip::Desynced as usize], 1);
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            menu: 1,
            ..FakeGame::default()
        };
        d.tick(&h, &mut g, Some(stopped));
        g.menu = 0;
        g.req = 1;
        assert_eq!(
            d.tick(&h, &mut g, Some(stopped)),
            Some(Attempt::Skipped(Skip::Desynced))
        );
        assert_eq!(g.calls, 0);
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = FakeGame {
            menu: 1,
            ..FakeGame::default()
        };
        let ok = net_view("あいて");
        let a: Vec<_> = (0..MENU_SETTLE_TICKS)
            .filter_map(|_| d.tick(&h, &mut g, Some(ok)))
            .collect();
        assert_eq!(a, vec![Attempt::Recorded]);
        assert_eq!(g.calls, 1);
    }

    #[test]
    fn net_run_reserves_only_while_connected_with_a_peer_name() {
        let dir = temp_dir("net_gate");
        let h = Handoff::new();
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        publish(&h, None);
        for _ in 0..3 {
            w.step(&h, &mut os);
        }
        assert_eq!(h.state(), ST_EMPTY, "つながっていないのに押さえた");
        assert!(vault_files(&dir).is_empty(), "{:?}", vault_files(&dir));
        assert!(os.asked.is_empty());
        publish(&h, Some(net_view("")));
        for _ in 0..3 {
            w.step(&h, &mut os);
        }
        assert_eq!(h.state(), ST_EMPTY, "相手の名前が無いのに押さえた");
        assert!(vault_files(&dir).is_empty(), "{:?}", vault_files(&dir));
        publish(&h, Some(net_view("B")));
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        assert_eq!(vault_files(&dir), vec!["じぶん vs B/th9_ud0001.rpy"]);
        let d2 = temp_dir("solo_gate");
        let h2 = Handoff::new();
        publish(&h2, None);
        let (mut w2, _) = worker(Split::Peer);
        let mut os2 = FakeOs::at(&d2);
        w2.step(&h2, &mut os2);
        assert_eq!(h2.state(), ST_READY);
        assert_eq!(vault_files(&d2), vec!["1P vs 2P/th9_ud0001.rpy"]);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn net_run_releases_the_unused_reservation_when_the_link_ends() {
        let dir = temp_dir("net_end");
        let h = Handoff::new();
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        publish(&h, Some(net_view("B")));
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        let folder = dir.join(VAULT_DIR).join("じぶん vs B");
        let own = folder.join("th9_ud0001.rpy");
        assert_eq!(fs::metadata(&own).unwrap().len(), 0);
        fs::write(folder.join("th9_ud0002.rpy"), synthetic_replay()).unwrap();
        fs::write(folder.join("th9_ud0003.rpy"), b"").unwrap();
        publish(&h, None);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_EMPTY);
        assert!(!own.exists(), "使われなかった予約を残した: {}", log.text());
        assert!(
            log.text()
                .contains("つながりが終わった（相手の名前が無い）ので"),
            "{}",
            log.text()
        );
        for _ in 0..3 {
            w.step(&h, &mut os);
        }
        assert_eq!(
            vault_files(&dir),
            vec!["じぶん vs B/th9_ud0002.rpy", "じぶん vs B/th9_ud0003.rpy"],
            "放した後に押さえた・自分のでないものを消した"
        );
        assert_eq!(
            fs::read(folder.join("th9_ud0002.rpy")).unwrap(),
            synthetic_replay()
        );
        let d2 = temp_dir("net_end_written");
        let h2 = Handoff::new();
        let (mut w2, _) = worker_net(Split::Peer);
        let mut os2 = FakeOs::at(&d2);
        publish(&h2, Some(net_view("B")));
        w2.step(&h2, &mut os2);
        let own2 = d2
            .join(VAULT_DIR)
            .join("じぶん vs B")
            .join("th9_ud0001.rpy");
        fs::write(&own2, synthetic_replay()).unwrap();
        publish(&h2, None);
        w2.step(&h2, &mut os2);
        assert_eq!(h2.state(), ST_EMPTY);
        assert_eq!(
            fs::read(&own2).unwrap(),
            synthetic_replay(),
            "書いた後を消した"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn net_run_after_save_and_teardown_leaves_no_empty_file() {
        let dir = temp_dir("net_flow");
        let h = Handoff::new();
        let nv = net_view("B");
        let mut d = Driver::new();
        let mut g = FakeGame::default();
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        d.tick(&h, &mut g, Some(nv));
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        g.menu = 1;
        for _ in 0..MENU_SETTLE_TICKS {
            d.tick(&h, &mut g, Some(nv));
        }
        assert_eq!(g.calls, 1);
        let f = dir
            .join(VAULT_DIR)
            .join("じぶん vs B")
            .join("th9_ud0001.rpy");
        fs::write(&f, synthetic_replay()).unwrap();
        w.step(&h, &mut os);
        assert_eq!(
            h.outcome_counts()[Outcome::Saved as usize],
            1,
            "{}",
            log.text()
        );
        g.menu = 0;
        g.req = 1;
        d.tick(&h, &mut g, None);
        g.cur = 1;
        d.tick(&h, &mut g, None);
        g.cur = SCENE_MATCH;
        g.req = SCENE_MATCH;
        d.tick(&h, &mut g, None);
        assert!(h.wanted(), "検査の前提: パスが要ると立っている");
        for _ in 0..5 {
            w.step(&h, &mut os);
            d.tick(&h, &mut g, None);
        }
        assert_eq!(h.state(), ST_EMPTY);
        assert_eq!(
            vault_files(&dir),
            vec!["じぶん vs B/th9_ud0001.rpy"],
            "{}",
            log.text()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 偽の台本の入口（`session` の検査の偽物と同じ形の最小のもの）。自動保存の入口を上書きする。
    struct ReplayHost {
        script: Option<Vec<u8>>,
        replay: Result<Option<String>, String>,
        events: Vec<&'static str>,
    }

    impl crate::session::ScriptHost for ReplayHost {
        fn read_script(&mut self) -> Result<Option<Vec<u8>>, String> {
            self.events.push("read");
            Ok(self.script.clone())
        }
        fn move_script(&mut self) -> Result<String, String> {
            self.events.push("move");
            Ok("synthetic_script.txt".to_string())
        }
        fn prepare_shots(&mut self) -> Result<(), String> {
            self.events.push("prepare");
            Ok(())
        }
        fn arm(
            &mut self,
            _s: higanr_core::script::Script,
            _feed: u32,
            _local: u32,
        ) -> Result<(), String> {
            self.events.push("arm");
            Ok(())
        }
        fn shots_on(&mut self) {
            self.events.push("shots_on");
        }
        fn start(&mut self) -> Result<(), String> {
            self.events.push("start");
            Ok(())
        }
        fn arm_replay(
            &mut self,
            _net: Option<&crate::net_config::NetSettings>,
        ) -> Result<Option<String>, String> {
            self.events.push("arm_replay");
            self.replay.clone()
        }
    }

    fn run_session(host: &mut ReplayHost) -> String {
        use crate::th09_addr::{FORBIDDEN, HOOK_SITE, IMAGE_BASE, SITES, WINDOW_ADDR};
        let def = SITES.iter().find(|d| d.name == HOOK_SITE).unwrap();
        let mut env = crate::probe::tests::Fake::all_match();
        let mut h = crate::hook::tests::FakeHook::new(2);
        h.mem_base = def.addr;
        h.mem[..5].copy_from_slice(def.expected);
        h.counter_fn = |now| (now / 1000 * 60) as u32;
        let mut rec = crate::observe::tests::FakeRecord::new(Vec::new(), 0);
        let mut out = Vec::new();
        crate::session::run(
            &mut env,
            &mut h,
            &mut rec,
            host,
            SITES,
            FORBIDDEN,
            IMAGE_BASE,
            crate::session::Sites {
                hook_site: HOOK_SITE,
                draw_site: "draw_before_endscene",
                exit_site: "render_only_exit",
            },
            WINDOW_ADDR,
            &crate::ticks::COPY_TABLE,
            60,
            &mut out,
        );
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn session_arms_replay_before_the_gate_and_only_with_a_script() {
        let script = b"settle=5\nscene=1 wait=3 p1=Z\n".to_vec();
        let mut host = ReplayHost {
            script: Some(script.clone()),
            replay: Ok(Some("合成で入れた".to_string())),
            events: Vec::new(),
        };
        let log = run_session(&mut host);
        assert_eq!(
            host.events,
            vec!["read", "move", "prepare", "arm", "arm_replay", "start"]
        );
        assert!(log.contains("自動保存: 合成で入れた"), "{log}");
        let mut host = ReplayHost {
            script: Some(script),
            replay: Err("合成の失敗".to_string()),
            events: Vec::new(),
        };
        let log = run_session(&mut host);
        assert_eq!(host.events.last(), Some(&"start"));
        assert!(
            log.contains("自動保存: 入れられない（合成の失敗）"),
            "{log}"
        );
        let mut host = ReplayHost {
            script: None,
            replay: Ok(Some("x".to_string())),
            events: Vec::new(),
        };
        run_session(&mut host);
        assert_eq!(host.events, vec!["read"]);
    }

    #[test]
    fn ini_samples_turn_the_saver_on() {
        for text in [
            include_str!("../../../scripts/m5b_replay.ini"),
            include_str!("../../../scripts/m5b_netplay.ini"),
        ] {
            let ini = higanr_core::ini::parse(text).expect("ini");
            let s = autosave::settings_from_ini(&ini).expect("[replay]");
            assert_eq!(s.map(|s| s.split), Some(Split::Peer));
        }
        let net = crate::net_config::from_text(include_str!("../../../scripts/m5b_netplay.ini"))
            .expect("[netplay]")
            .expect("enable = 1");
        assert_eq!(net.name, "HiganRTool");
        assert_eq!(
            crate::net_config::from_text(include_str!("../../../scripts/m5b_replay.ini")),
            Ok(None),
            "単独の見本でネットワークを開く"
        );
    }

    // ---- ラウンドごとの記録・同期方式・通信の要約（印の `r1`〜 / `sync` / `net` …）----

    /// 勝ち数が 0 対 0 の始まりから始まる偽のゲーム（`FakeGame::default()` は 2P が 2 勝している）。
    fn fresh_game() -> FakeGame {
        FakeGame {
            wins: [0, 0, 2],
            ..FakeGame::default()
        }
    }

    fn snap(life: u32, c23: u32, boss: u32, rev: u32, score: u32) -> Option<SideSnap> {
        Some(SideSnap {
            life,
            c23,
            boss,
            rev,
            score,
        })
    }

    /// 1 ラウンドぶん（`len` tick）を回し、最後の tick の中で `completed_rounds` と勝ち数を増やす（その tick の席の塊が `at_end`）。
    /// 境目の後の `after` tick は、結果の演出の間として `round_frames` が進み、1P の C2 + C3 も動く。
    #[allow(clippy::too_many_arguments)]
    fn play_round(
        d: &mut Driver,
        h: &Handoff,
        g: &mut FakeGame,
        net: Option<NetView>,
        len: u32,
        winner: usize,
        at_end: [Option<SideSnap>; 2],
        after: u32,
    ) {
        for t in 1..=len {
            g.round_frames = t;
            if t == len {
                g.completed += 1;
                g.wins[winner] += 1;
                g.sides = at_end;
            }
            d.tick(h, g, net);
        }
        for t in 1..=after {
            g.round_frames = len + t;
            if let Some(s) = g.sides[0].as_mut() {
                s.c23 += 1;
            }
            d.tick(h, g, net);
        }
        g.round_frames = 0;
    }

    #[test]
    fn a_boundary_is_read_on_the_tick_the_count_rose_and_not_later() {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame {
            sides: [snap(7, 0, 0, 0, 100), snap(3, 0, 0, 0, 40)],
            ..fresh_game()
        };
        d.tick(&h, &mut g, None);
        play_round(
            &mut d,
            &h,
            &mut g,
            None,
            100,
            0,
            [snap(7, 0, 0, 0, 100), snap(0, 0, 0, 0, 40)],
            60,
        );
        assert_eq!(g.boundary_reads, 1, "増えた tick の 1 回だけ読む");
        let s = d.summary();
        let b = s.rounds[0].unwrap();
        assert_eq!(b.frames, 100, "境目の tick の round_frames");
        assert_eq!(b.wins, [1, 0]);
        assert_eq!(
            b.side[0].unwrap().c23,
            0,
            "境目の後に動いたカウンタを読んだ"
        );
        assert_eq!(b.side[1].unwrap().life, 0, "負けた側の終わりのライフ");
        assert_eq!(s.rounds[1], None);
    }

    #[test]
    fn counters_in_the_summary_are_cumulative_and_the_detail_turns_them_into_differences() {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = fresh_game();
        d.tick(&h, &mut g, None);
        let rounds = [
            (1000, 0, [snap(4, 2, 1, 0, 100), snap(0, 1, 0, 0, 40)]),
            (800, 1, [snap(0, 5, 3, 1, 250), snap(6, 1, 2, 1, 190)]),
            (600, 0, [snap(2, 6, 3, 1, 310), snap(0, 4, 2, 1, 200)]),
        ];
        for (len, winner, at_end) in rounds {
            play_round(&mut d, &h, &mut g, None, len, winner, at_end, 30);
        }
        let s = d.summary();
        assert_eq!(s.rounds.iter().flatten().count(), 3);
        assert_eq!(s.rounds[1].unwrap().side[0].unwrap().c23, 5, "境目の累計");
        let det = s.detail(7);
        let r = &det.rounds;
        assert_eq!(
            r.iter().map(|x| x.frames).collect::<Vec<_>>(),
            [1000, 800, 600]
        );
        assert_eq!(
            r.iter().map(|x| x.winner).collect::<Vec<_>>(),
            [Winner::P1, Winner::P2, Winner::P1]
        );
        assert_eq!(r[0].c23, [Some(2), Some(1)]);
        assert_eq!(r[1].c23, [Some(3), Some(0)]);
        assert_eq!(r[2].c23, [Some(1), Some(3)]);
        assert_eq!(r[1].boss, [Some(2), Some(2)]);
        assert_eq!(r[1].rev, [Some(1), Some(1)]);
        assert_eq!(r[2].rev, [Some(0), Some(0)]);
        assert_eq!(r[0].life, [Some(4), Some(0)]);
        assert_eq!(
            r[2].score,
            [Some(3100), Some(2000)],
            "画面のスコア = 内部 × 10"
        );
    }

    #[test]
    fn under_rollback_only_confirmed_ticks_record_a_boundary() {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = fresh_game();
        let nv = NetView {
            rb_ready: true,
            rollback: true,
            ..net_view("あいて")
        };
        d.tick(&h, &mut g, Some(nv));
        // フレームの始まりに見えるのは予測の tick の後の状態。そこで終わっていたラウンドは、書き戻しで消えるかもしれないので積まない。
        g.round_frames = 500;
        g.completed = 1;
        g.wins = [1, 0, 2];
        g.sides = [snap(5, 1, 0, 0, 10), snap(0, 0, 0, 0, 5)];
        d.tick(&h, &mut g, Some(nv));
        assert_eq!(g.boundary_reads, 0, "予測の状態の境目を読んだ");
        assert_eq!(d.summary().rounds[0], None);
        // 書き戻し。
        g.completed = 0;
        g.wins = [0, 0, 2];
        g.round_frames = 480;
        d.tick(&h, &mut g, Some(nv));
        assert_eq!(d.summary().rounds[0], None);
        // 確定の語で進め直すと、別の tick（別の勝者）でラウンドが終わった。確定した tick の後で読む。
        g.round_frames = 503;
        g.completed = 1;
        g.wins = [0, 1, 2];
        g.sides = [snap(0, 2, 0, 0, 12), snap(4, 0, 0, 0, 5)];
        d.note_confirmed_tick(&mut g);
        assert_eq!(g.boundary_reads, 1);
        let b = d.summary().rounds[0].unwrap();
        assert_eq!((b.frames, b.wins), (503, [0, 1]));
        // その先の予測の状態（次のラウンドの境目も）は積まず、確定した値を上書きしない。
        g.round_frames = 520;
        g.sides = [snap(0, 9, 0, 0, 99), snap(4, 9, 0, 0, 99)];
        d.tick(&h, &mut g, Some(nv));
        g.completed = 2;
        d.tick(&h, &mut g, Some(nv));
        g.completed = 1;
        d.tick(&h, &mut g, Some(nv));
        assert_eq!(g.boundary_reads, 1);
        assert_eq!(d.summary().rounds[0], Some(b));
        assert_eq!(d.summary().rounds[1], None);
        // 2 ラウンド目も確定した tick で拾う。
        g.round_frames = 700;
        g.completed = 2;
        d.note_confirmed_tick(&mut g);
        assert_eq!(d.summary().rounds[1].unwrap().frames, 700);
        assert_eq!(d.summary().rounds[0], Some(b));
    }

    #[test]
    fn a_confirmed_tick_before_the_match_started_records_nothing() {
        let mut d = Driver::new();
        let mut g = FakeGame {
            completed: 1,
            ..fresh_game()
        };
        d.note_confirmed_tick(&mut g);
        assert_eq!(g.boundary_reads, 0);
        assert_eq!(d.summary(), MatchSummary::EMPTY);
    }

    #[test]
    fn a_boundary_that_disappears_in_the_frame_start_path_is_dropped_and_a_replayed_one_overwrites()
    {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = fresh_game();
        d.tick(&h, &mut g, None);
        g.completed = 1;
        g.round_frames = 100;
        d.tick(&h, &mut g, None);
        g.completed = 2;
        g.round_frames = 220;
        d.tick(&h, &mut g, None);
        assert_eq!(d.summary().rounds.iter().flatten().count(), 2);
        // 2 ラウンド目が書き戻しで消えた。終わらないままなら 2 ラウンド目は残らない。
        g.completed = 1;
        d.tick(&h, &mut g, None);
        assert_eq!(d.summary().rounds.iter().flatten().count(), 1);
        // 別の tick でもう一度終わったら、その値で置き換わる。
        g.completed = 2;
        g.round_frames = 231;
        d.tick(&h, &mut g, None);
        assert_eq!(d.summary().rounds[1].unwrap().frames, 231);
    }

    #[test]
    fn the_match_log_starts_after_loading_and_is_cleared_for_a_rematch() {
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = FakeGame {
            loading: 1,
            utc: 1_790_000_000,
            ..fresh_game()
        };
        for _ in 0..3 {
            d.tick(&h, &mut g, None);
        }
        assert_eq!(d.summary().start_utc, None, "読み込み中に開始にした");
        g.loading = 0;
        g.utc = 1_790_000_100;
        d.tick(&h, &mut g, None);
        g.utc = 1_790_009_999;
        d.tick(&h, &mut g, None);
        assert_eq!(
            d.summary().start_utc,
            Some(1_790_000_100),
            "開始時刻はマッチの始まりの 1 回"
        );
        play_round(
            &mut d,
            &h,
            &mut g,
            None,
            50,
            0,
            [snap(1, 0, 0, 0, 1), snap(0, 0, 0, 0, 1)],
            5,
        );
        assert_eq!(d.summary().rounds.iter().flatten().count(), 1);
        // 再戦: 抜ける tick の後、場面 2 のまま戻って読み込みが始まる。前のマッチの記録は残らない。
        g.req = 10;
        d.tick(&h, &mut g, None);
        g.req = SCENE_MATCH;
        g.loading = 1;
        g.completed = 0;
        g.wins = [0, 0, 2];
        d.tick(&h, &mut g, None);
        assert_eq!(d.summary(), MatchSummary::EMPTY);
        g.loading = 0;
        g.utc = 1_790_000_300;
        d.tick(&h, &mut g, None);
        let s = d.summary();
        assert_eq!(s.start_utc, Some(1_790_000_300));
        assert_eq!(s.rounds.iter().flatten().count(), 0);
        // Story など Match でない場面では積まない。
        let mut d = Driver::new();
        let mut g = FakeGame {
            mode: 0,
            ..fresh_game()
        };
        d.tick(&h, &mut g, None);
        assert_eq!(d.summary().start_utc, None);
    }

    fn mk_net(rtt: u32, stalls: u64, rollback: bool) -> NetView {
        NetView {
            rtt_ms: Some(rtt),
            match_no: 3,
            rollback,
            totals: NetTotals {
                stalls,
                rewinds: if rollback { stalls * 10 } else { 0 },
                resim_ticks: if rollback { stalls * 30 } else { 0 },
            },
            ..net_view("あいて")
        }
    }

    fn read_detail(f: &Path) -> (Mark, higanr_core::replay_detail::MatchDetail) {
        let MarkRead::Found(m) = replay::read_mark(&fs::read(f).unwrap()).unwrap() else {
            panic!("印が読めない");
        };
        let det = m.detail.clone().expect("マッチの情報が無い");
        (m, det)
    }

    #[test]
    fn the_mark_carries_the_rounds_and_the_match_facts_of_a_net_match() {
        for rollback in [false, true] {
            let dir = temp_dir(if rollback {
                "detail_rb"
            } else {
                "detail_delay"
            });
            let h = Handoff::new();
            let mut d = Driver::new();
            let mut g = fresh_game();
            d.tick(&h, &mut g, Some(mk_net(30, 5, rollback)));
            let (mut w, log) = worker_net(Split::Peer);
            let mut os = FakeOs::at(&dir);
            w.step(&h, &mut os);
            assert_eq!(h.state(), ST_READY, "{}", log.text());
            // 1P が 2 本取る。ping は 30 → 50 → 40 ms、止まった数は 5 → 6 → 7。
            let plan = [
                (
                    mk_net(30, 5, rollback),
                    120,
                    [snap(4, 2, 1, 1, 100), snap(0, 0, 0, 0, 50)],
                ),
                (
                    mk_net(50, 6, rollback),
                    90,
                    [snap(3, 3, 1, 1, 220), snap(0, 1, 0, 0, 60)],
                ),
            ];
            for (nv, len, at_end) in plan {
                if rollback {
                    // 確定した tick の後で拾う。フレームの始まりの経路は境目を読まない。
                    let nv = Some(NetView {
                        rb_ready: true,
                        ..nv
                    });
                    for t in 1..=len {
                        g.round_frames = t;
                        d.tick(&h, &mut g, nv);
                        if t == len {
                            g.completed += 1;
                            g.wins[0] += 1;
                            g.sides = at_end;
                            d.note_confirmed_tick(&mut g);
                        }
                    }
                    g.round_frames = 0;
                } else {
                    play_round(&mut d, &h, &mut g, Some(nv), len, 0, at_end, 10);
                }
            }
            g.menu = 1;
            for _ in 0..MENU_SETTLE_TICKS {
                d.tick(&h, &mut g, Some(mk_net(40, 7, rollback)));
            }
            assert_eq!(h.state(), ST_USED);
            let f = dir
                .join(VAULT_DIR)
                .join("じぶん vs あいて")
                .join("th9_ud0001.rpy");
            fs::write(&f, synthetic_replay()).unwrap();
            w.step(&h, &mut os);
            assert!(log.text().contains("ラウンドの記録 2 件"), "{}", log.text());
            let (m, det) = read_detail(&f);
            assert_eq!(m.result, MatchResult::Win);
            assert_eq!(
                det.sync,
                Some(if rollback {
                    SyncKind::Rollback
                } else {
                    SyncKind::Delay
                })
            );
            assert_eq!(det.build, Some(BUILD_NUMBER));
            assert_eq!(det.start_utc, Some(1_790_000_000));
            assert_eq!(det.match_no, Some(3));
            let c = det.comm.expect("通信の要約");
            // 最初の 1 tick から数える。ping 30 は 121+ 、50 は 90+、40 は 6 の標本。中央値は 30、最大は 50。
            assert_eq!((c.ping_median_ms, c.ping_max_ms), (Some(30), Some(50)));
            assert_eq!(c.stalls, 2, "7 − 5（マッチの始まりの累計との差）");
            assert_eq!(
                (c.rewinds, c.resim_ticks),
                if rollback { (20, 60) } else { (0, 0) }
            );
            assert_eq!(det.rounds.len(), 2);
            assert_eq!(det.rounds[0].frames, 120);
            assert_eq!(det.rounds[1].frames, 90);
            assert_eq!(det.rounds[0].winner, Winner::P1);
            assert_eq!(det.rounds[1].c23, [Some(1), Some(1)], "差");
            assert_eq!(det.rounds_dropped, 0);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_standalone_mark_has_the_rounds_but_no_net_keys() {
        let dir = temp_dir("detail_alone");
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = fresh_game();
        d.tick(&h, &mut g, None);
        let (mut w, log) = worker(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        play_round(
            &mut d,
            &h,
            &mut g,
            None,
            60,
            1,
            [snap(0, 0, 0, 0, 1), snap(3, 0, 0, 0, 2)],
            5,
        );
        g.wins = [0, 2, 2];
        g.menu = 1;
        for _ in 0..MENU_SETTLE_TICKS {
            d.tick(&h, &mut g, None);
        }
        let f = dir.join(VAULT_DIR).join("1P vs 2P").join("th9_ud0001.rpy");
        fs::write(&f, synthetic_replay()).unwrap();
        w.step(&h, &mut os);
        let (_, det) = read_detail(&f);
        assert_eq!((det.sync, det.match_no, det.comm), (None, None, None));
        assert_eq!(det.build, Some(BUILD_NUMBER));
        assert_eq!(det.start_utc, Some(1_790_000_000));
        assert_eq!(det.rounds.len(), 1);
        assert_eq!(det.rounds[0].winner, Winner::P2);
        let bytes = fs::read(&f).unwrap();
        let tail = String::from_utf8_lossy(&bytes[0xC0..]).into_owned();
        for k in ["\tsync=", "\tn=", "\tnet="] {
            assert!(!tail.contains(k), "{k}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_watcher_mark_has_the_rounds_of_the_watched_match_and_no_net_keys() {
        let dir = temp_dir("detail_watch");
        let h = Handoff::new();
        let mut d = Driver::new();
        let mut g = fresh_game();
        let watching = names_of("いちろう", "じろう");
        watch_tick(&mut d, &h, &mut g, watching);
        let (mut w, log) = worker_net(Split::Peer);
        let mut os = FakeOs::at(&dir);
        w.step(&h, &mut os);
        assert_eq!(h.state(), ST_READY, "{}", log.text());
        // 観戦者の手元のゲームは配られた語から同じマッチを再現するので、同じ tick で境目を通る。
        for (len, winner) in [(70, 1), (80, 1)] {
            for t in 1..=len {
                g.round_frames = t;
                if t == len {
                    g.completed += 1;
                    g.wins[winner] += 1;
                    g.sides = [snap(0, 0, 0, 0, 1), snap(2, 0, 0, 0, 1)];
                }
                watch_tick(&mut d, &h, &mut g, watching);
            }
        }
        g.menu = 1;
        for _ in 0..MENU_SETTLE_TICKS {
            watch_tick(&mut d, &h, &mut g, watching);
        }
        let f = dir
            .join(VAULT_DIR)
            .join("いちろう vs じろう")
            .join("th9_ud0001.rpy");
        fs::write(&f, synthetic_replay()).unwrap();
        w.step(&h, &mut os);
        let (m, det) = read_detail(&f);
        assert!(matches!(m.role, MarkRole::Watch { .. }));
        assert_eq!(det.rounds.len(), 2);
        assert_eq!(det.rounds[1].frames, 80);
        assert_eq!((det.sync, det.match_no, det.comm), (None, None, None));
        assert_eq!(det.start_utc, Some(1_790_000_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_game_thread_path_with_round_boundaries_does_not_allocate() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = fresh_game();
        g.names.reserve(4);
        g.paths.reserve(4);
        let nv = mk_net(33, 1, true);
        d.tick(
            &h,
            &mut g,
            Some(NetView {
                rb_ready: false,
                ..nv
            }),
        );
        let a0 = crate::test_alloc::count();
        for (len, winner) in [(40, 0usize), (30, 1), (20, 0)] {
            play_round(
                &mut d,
                &h,
                &mut g,
                Some(NetView {
                    rb_ready: false,
                    ..nv
                }),
                len,
                winner,
                [snap(1, 1, 1, 1, 1), snap(2, 2, 2, 2, 2)],
                3,
            );
        }
        let rb = NetView {
            rb_ready: true,
            ..nv
        };
        d.tick(&h, &mut g, Some(rb));
        g.completed += 1;
        d.note_confirmed_tick(&mut g);
        g.menu = 1;
        for _ in 0..(MENU_SETTLE_TICKS + 2) {
            d.tick(&h, &mut g, Some(nv));
        }
        let a1 = crate::test_alloc::count();
        assert_eq!(g.calls, 1, "保存の経路を通っていない");
        assert!(g.boundary_reads >= 4, "境目を読んでいない");
        assert_eq!(a1, a0, "ラウンドの記録でゲームのスレッドが確保した");
    }

    /// 席の塊の読み: 落ちない読み方（`MemRead`）だけを使い、ポインタが 0 の席・欄が読めない席は `None`。
    #[test]
    fn read_sides_reads_four_fields_and_the_score_and_gives_none_for_what_it_cannot_read() {
        use std::collections::HashMap;
        #[derive(Default)]
        struct Mem(HashMap<u32, u32>);
        impl MemRead for Mem {
            fn u32_at(&mut self, addr: u32) -> Option<u32> {
                self.0.get(&addr).copied()
            }
        }
        let (pp, sp) = ([0x100, 0x104], [0x108, 0x10C]);
        let (p1, p2, s1, s2) = (0x1000, 0x2000, 0x3000, 0x4000);
        let mut m = Mem::default();
        m.0.extend([(pp[0], p1), (pp[1], p2), (sp[0], s1), (sp[1], s2)]);
        for (base, v) in [(p1, 4), (p2, 9)] {
            m.0.extend([
                (base + PLAYER_LIFE, v),
                (base + PLAYER_SPELL_ATTACKS, v + 1),
                (base + PLAYER_BOSS_ATTACKS, v + 2),
                (base + PLAYER_BOSS_REVERSALS, v + 3),
            ]);
        }
        m.0.extend([(s1 + STATS_SCORE, 777), (s2 + STATS_SCORE, 888)]);
        let got = read_sides(&mut m, pp, sp);
        assert_eq!(got, [snap(4, 5, 6, 7, 777), snap(9, 10, 11, 12, 888)]);
        // 2P の Player が片付け済み（ポインタ 0）なら、2P だけ読めない。
        m.0.insert(pp[1], 0);
        let got = read_sides(&mut m, pp, sp);
        assert_eq!((got[0], got[1]), (snap(4, 5, 6, 7, 777), None));
        // 1P のカウンタの欄が 1 つ読めなければ、1P だけ読めない。
        m.0.insert(pp[1], p2);
        m.0.remove(&(p1 + PLAYER_BOSS_REVERSALS));
        let got = read_sides(&mut m, pp, sp);
        assert_eq!((got[0], got[1]), (None, snap(9, 10, 11, 12, 888)));
        // スコアの塊が読めなければ、その席は読めない（ライフだけ、とはしない）。
        m.0.insert(p1 + PLAYER_BOSS_REVERSALS, 7);
        m.0.remove(&sp[0]);
        let got = read_sides(&mut m, pp, sp);
        assert_eq!(got[0], None);
        // アドレスの足し算があふれるポインタは読まない。
        m.0.insert(sp[0], s1);
        m.0.insert(pp[0], u32::MAX - 4);
        assert_eq!(read_sides(&mut m, pp, sp)[0], None);
    }

    /// 壊れたら落ちる: マッチの記録が Handoff を通ってワーカーに届く（語のコピーの取りこぼし）。
    #[test]
    fn the_summary_reaches_the_worker_through_the_handoff() {
        let h = Handoff::new();
        ready(&h);
        let mut d = Driver::new();
        let mut g = fresh_game();
        let nv = mk_net(61, 2, true);
        d.tick(&h, &mut g, Some(nv));
        play_round(
            &mut d,
            &h,
            &mut g,
            Some(NetView {
                rb_ready: false,
                ..nv
            }),
            55,
            1,
            [snap(1, 2, 3, 4, 5), snap(6, 7, 8, 9, 10)],
            0,
        );
        menu_ticks_net(&mut d, &h, &mut g, nv);
        assert_eq!(h.state(), ST_USED);
        let got = h.read_summary();
        assert_eq!(got, d.summary());
        assert_eq!(got.rounds[0].unwrap().frames, 55);
        assert_eq!(got.net.unwrap().match_no, 3);
        assert!(got.net.unwrap().rollback);
    }

    fn menu_ticks_net(d: &mut Driver, h: &Handoff, g: &mut FakeGame, nv: NetView) {
        g.menu = 1;
        for _ in 0..MENU_SETTLE_TICKS {
            d.tick(h, g, Some(nv));
        }
    }

    #[test]
    fn addresses_appear_in_the_note() {
        const NOTE: &str = include_str!("../../../analysis/notes/th09_replay_save_M5b.md");
        let sig = REPLAY_SAVE_SIG
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let base = SAVE_EST_BASE.to_le_bytes();
        let want = [
            format!("`0x{REPLAY_SAVE_FN:08X}`"),
            format!("`{sig}`"),
            format!("`0x{MATCH_END_MENU:X}`"),
            format!("`0x{EXE_DIR:X}`（`char[0x{EXE_DIR_LEN:X}]`）"),
            format!(
                "`0x{:X}` の 5 バイトが `B9 {:02X} {:02X} {:02X} {:02X}`",
                SAVE_EST_BASE_SITE, base[0], base[1], base[2], base[3]
            ),
            format!(
                "`0x{:X} mov -0x{SAVE_EST_FLAGS_EBP:x}(%ebp,%esi,4),%eax`",
                0x0042_0CA0
            ),
            format!("`0x00420CAB add $0x{SAVE_EST_PER_STAGE:x},%ecx`"),
            format!("`0x00420CB7 add $0x{SAVE_EST_PER_CHUNK:x},%ecx`"),
            format!("`0x00420CC5 cmp $0x{STAGES:x},%esi`"),
            format!("`0x00420CDA mov 0x{REPLAY_HEADER_PTR:x}(%edi),%esi`"),
            format!("0x{:X} 語", REPLAY_HEADER_LEN / 4),
            format!("= `+0x{REPLAY_HEADER_STAGE_FLAGS:X}`"),
            format!("`0x00420C8C lea 0x{REPLAY_STAGE_NODES:x}(%edi),%edx`"),
            format!("`0x00420CC2 add $0x{REPLAY_STAGE_STRIDE:x},%edx`"),
            format!("`0x00420CB4 mov 0x{REPLAY_NODE_NEXT:x}(%eax),%eax`"),
            format!("`0x00415D8A mov 0x{P1_WINS:x}(,%edi,4),%edx`"),
            format!("`0x00415D99 mov 0x{ROUNDS_REQUIRED:x},%ecx`"),
            format!("2P `0x{P2_WINS:X}`"),
            format!(
                "`[E − 0x{:X} + 4i]`",
                REPLAY_SAVE_FRAME_SHIFT + SAVE_EST_FLAGS_EBP
            ),
            format!("`[esi+4]+1 == {MENU_SETTLE_TICKS}`"),
            format!("`0x{GAME_MODE:X}` が {GAME_MODE_MATCH}（Match"),
            format!("`[0x{REPLAY_MANAGER_PTR:X}]` が 0 でない / `+0x{REPLAY_MODE:X}` が 0"),
            format!("読み込み `0x{LOADING:X}`"),
        ];
        for w in want {
            assert!(NOTE.contains(&w), "ノートに {w} が無い");
        }
        // 署名の先頭は push ebp; mov ebp,esp（ebp = call の時点の esp − 8 の根拠）。
        assert_eq!(REPLAY_SAVE_SIG[..3], [0x55, 0x8B, 0xEC]);
        assert_eq!(REPLAY_SAVE_FRAME_SHIFT, 8);
        assert_eq!(P2_WINS - P1_WINS, 4);
        assert_eq!(RNG_BYTES, 12);
    }
}
