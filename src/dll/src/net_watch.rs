//! ネットワークの、ゲームのスレッドから読み取りのスレッドへの受け渡し（pure。原子変数だけ）と、読み取りのスレッドが書き出すもの
//! （`_net_frames.csv` と終わりの集計）。
//!
//! ゲームのスレッドは lockstep のフレームを使うたびに (seg, frame, 1P, 2P) と時間（使った後の delay・余裕・待ち）を `NET_FRAMES`
//! （単一書き手のリングバッファ。`feeds::FeedRing` と同じ札の読み方）へ書き、tick ごとに数を `NET_STATUS` へコピーする。読み取りのスレッドは
//! 100 ms ごとにリングバッファを読んで CSV へ（peer_sim の記録と突き合わせる）、終わりに集計をログへ。ゲームのスレッドの側
//! （`push` / `publish`）は確保しない・パニックしない。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering, fence};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};

use higanr_core::candidate::kind_of;
use higanr_core::handshake::{FailReason, Restart};
use higanr_core::link;
use higanr_core::match_gate::NoMatch;
use higanr_core::net_menu::other_version_text;
use higanr_core::selection::Agreed;
use higanr_core::wire::{ByeReason, Check, Mode, PROTO_VER, Select};

use crate::netplay::{
    End, FrameRec, FrameTiming, NetPlay, Phase, STEP_LOG_LEN, WAIT_OVER_BOUNDS_US,
};
use crate::spectate_watch::{WatchEnd, WatchPlay};
use crate::ticks::{SlotState, classify};
use crate::trial_summary::desync_text;

/// リングバッファの長さ（4096 件 ≈ 68 秒ぶん）。
pub const NET_RING_LEN: u32 = 4096;
const NET_RING_MASK: u32 = NET_RING_LEN - 1;
const _: () = assert!(NET_RING_LEN.is_power_of_two());

/// 1 件の語: 札・frame・seg << 16 | 1P・delay << 16 | 2P・余裕（無ければ `NO_MARGIN`）・待ち・始まりの待ち（`FrameTiming::head_wait_us`）。
const SLOT_WORDS: usize = 7;
/// 余裕が分からない印（リングバッファの中だけ）。
const NO_MARGIN: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetFrame {
    pub rec: FrameRec,
    pub timing: FrameTiming,
}

/// 使ったフレームのリングバッファ。書き手はゲームのスレッドだけ（`push`）、読み手は読み取りのスレッドだけ（`read`）。
pub struct NetFrameRing {
    slots: [[AtomicU32; SLOT_WORDS]; NET_RING_LEN as usize],
    next: AtomicU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRead {
    Record(NetFrame),
    NotYet,
    Overwritten,
}

impl NetFrameRing {
    pub const fn new() -> Self {
        NetFrameRing {
            slots: [const { [const { AtomicU32::new(0) }; SLOT_WORDS] }; NET_RING_LEN as usize],
            next: AtomicU32::new(0),
        }
    }

    /// 1 件書く（札を 0 → 中身 → 札 = 番号 + 1。`feeds::FeedRing::push` と同じ順）。
    pub fn push(&self, r: &FrameRec, t: &FrameTiming) {
        let seq = self.next.load(Ordering::Relaxed);
        let slot = &self.slots[(seq & NET_RING_MASK) as usize];
        slot[0].store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        slot[1].store(r.frame, Ordering::Relaxed);
        slot[2].store(u32::from(r.seg) << 16 | u32::from(r.p1), Ordering::Relaxed);
        slot[3].store(
            u32::from(t.delay) << 16 | u32::from(r.p2),
            Ordering::Relaxed,
        );
        slot[4].store(
            t.margin_us.map_or(NO_MARGIN, |m| m.min(NO_MARGIN - 1)),
            Ordering::Relaxed,
        );
        slot[5].store(t.wait_us, Ordering::Relaxed);
        slot[6].store(t.head_wait_us, Ordering::Relaxed);
        slot[0].store(seq.wrapping_add(1), Ordering::Release);
        self.next.store(seq.wrapping_add(1), Ordering::Release);
    }

    pub fn next_seq(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }

    pub fn read(&self, seq: u32) -> FrameRead {
        let slot = &self.slots[(seq & NET_RING_MASK) as usize];
        let t1 = slot[0].load(Ordering::Acquire);
        let (frame, a, b, margin, wait, head_wait) = (
            slot[1].load(Ordering::Relaxed),
            slot[2].load(Ordering::Relaxed),
            slot[3].load(Ordering::Relaxed),
            slot[4].load(Ordering::Relaxed),
            slot[5].load(Ordering::Relaxed),
            slot[6].load(Ordering::Relaxed),
        );
        fence(Ordering::Acquire);
        let t2 = slot[0].load(Ordering::Relaxed);
        match classify(seq, t1, t2) {
            SlotState::Complete => FrameRead::Record(NetFrame {
                rec: FrameRec {
                    seg: (a >> 16) as u16,
                    frame,
                    p1: a as u16,
                    p2: b as u16,
                },
                timing: FrameTiming {
                    delay: (b >> 16) as u8,
                    margin_us: (margin != NO_MARGIN).then_some(margin),
                    wait_us: wait,
                    head_wait_us: head_wait,
                },
            }),
            SlotState::NotYet => FrameRead::NotYet,
            SlotState::Overwritten => FrameRead::Overwritten,
        }
    }

    /// 新しい件を読む（読み落としの数え方は `feeds::drain_feeds` と同じ）。
    pub fn drain(&self, next: &mut u32, lost: &mut u64, out: &mut Vec<NetFrame>) {
        let end = self.next_seq();
        let behind = end.wrapping_sub(*next);
        if behind > NET_RING_LEN {
            let skip = behind - NET_RING_LEN;
            *lost += u64::from(skip);
            *next = next.wrapping_add(skip);
        }
        while *next != end {
            match self.read(*next) {
                FrameRead::Record(r) => out.push(r),
                FrameRead::Overwritten => *lost += 1,
                FrameRead::NotYet => break,
            }
            *next = next.wrapping_add(1);
        }
    }
}

impl Default for NetFrameRing {
    fn default() -> Self {
        Self::new()
    }
}

pub static NET_FRAMES: NetFrameRing = NetFrameRing::new();

/// ゲームのスレッドが tick ごとにコピーする数（読み取りのスレッドはログにだけ使う）。欄ごとに原子変数で、欄どうしは同じ tick の値とは限らない。
pub struct NetStatus {
    w: [AtomicU32; STATUS_WORDS],
}

const STATUS_WORDS: usize = S_FMT + FMT_WORDS;
const S_PHASE: usize = 0;
const S_END: usize = 1;
const S_END_DETAIL: usize = 2;
const S_TICKS: usize = 3;
const S_PLAYED: usize = 4;
const S_ZERO: usize = 5;
const S_STALLS: usize = 6;
const S_MAX_STALL: usize = 7;
const S_HOLD: usize = 8;
const S_LOCAL_READS: usize = 9;
const S_SEGS: usize = 10;
const S_CHARGE_FAIL: usize = 11;
const S_CHECK_OK: usize = 12;
const S_DELAY: usize = 13;
// ネットの選択。
const S_MENU_FRAMES: usize = 14;
const S_MENU_APPLIED: usize = 15;
const S_MENU_HELD: usize = 16;
const S_MENU_ALIGN_MISS: usize = 17;
const S_MENU_LOST: usize = 18;
/// 置いた自分の SELECT（2 語）と揃った値（2 語）。`pack_select` / `pack_agreed`。
const S_OWN_SELECT: usize = 19;
const S_AGREED: usize = 21;
/// 入った押下の数と、押下ごとの 2 語（`StepLog` のリングバッファの並びのまま）。
const S_STEP_COUNT: usize = 23;
const S_STEPS: usize = 24;
/// latency の切替と時間: 要求した・Busy で回した・断られた・使った切替・余裕が分からなかった・待ちが時刻で戻った・その遅れの最大（µs）・
/// 遅れの目盛り 5 枠。
const S_TIMING: usize = S_STEPS + 2 * STEP_LOG_LEN;
const TIMING_WORDS: usize = 12;
/// 接続の手順の作り直し: 回数と、直前の 1 回（`restart_code`）。
const S_HS: usize = S_TIMING + TIMING_WORDS;
/// ネットワークの選択の合意なしに場面 2 へ移った tick 数（`Stats::local_scene2`。タイトルのデモなど）。
const S_LOCAL_SCENE2: usize = S_HS + 2;
/// セッションの受信の数（`link::RecvCounts`）: 届いた数・候補に無い送り元・タグの誤り・別のセッション、最後に捨てた送り元（`pack_from` の 2 語）。
/// 「来なかった」と「来たが捨てた」を見分けるため。
const S_RECV: usize = S_HS + 3;
/// 語 6・7 は、学んだ候補（`RecvCounts::learned` / `learn_refused`）の数。
const RECV_WORDS: usize = 8;
/// キャラ選択の立ち絵を追わせた回数と、書いたカーソルが前と違うのに呼ばなかった回数（`Stats::menu_portrait_called` / `menu_portrait_refused`）。
const S_PORTRAIT: usize = S_RECV + RECV_WORDS;
/// 繋がる前に選択の画面で落とした決定の押下の回数（`Stats::menu_pre_link_decides`）。
const S_PRE_LINK: usize = S_PORTRAIT + 2;
/// 繋がったか・経路の種類・繋がるまでの ms・RTT: 語 0 = 繋がった | rollback に決まった << 1 | 採った経路が学んだ候補 << 2 |
/// 経路の種類（`CandKind` の番号）<< 8、語 1 = セッションを作ってから繋がったのを見るまでの ms、語 2 = 最後に測った RTT の ms（無ければ `NO_MARGIN`）。
const S_CONN: usize = S_PRE_LINK + 1;
const CONN_WORDS: usize = 3;
/// CHECK のズレのときの、自分と相手の CHECK の全部の欄（`pack_check` の 4 語ずつ。ゲームのスレッドが `End::Desync` を見たときだけ書く）。
const S_DESYNC: usize = S_CONN + CONN_WORDS;
const DESYNC_WORDS: usize = 8;
/// 対戦の成立（READY。`higanr_core::match_gate`。`code_side = game` の実行だけ）: 語 0 = 印（`gate_flags`）、語 1〜3 = READY を送った・受けた・
/// そのうち重複の数。
const S_GATE: usize = S_DESYNC + DESYNC_WORDS;
const GATE_WORDS: usize = 4;
/// 遅れの量の選び（`higanr_core::delay_pick`）: 語 0 = 印（1 = 持っている・2 = ホスト・4 = 値を選んだ）、語 1 = 選んだ値、
/// 語 2 = Ctrl＋数字で変えた回数。
const S_PICK: usize = S_GATE + GATE_WORDS;
const PICK_WORDS: usize = 3;
/// 繋がりのセッションの INPUT の受信の数（`link::ConnCounts`）: 受理・前の seg・前の seg へのバックアップの返し・別の seg・そのほかで捨てた
/// （うち切替つきの `GuestLat`・取り決めに合わない `BadLat`）。delay 方式でも rollback でもコピーする。
const S_INPUT: usize = S_PICK + PICK_WORDS;
const INPUT_WORDS: usize = 7;
/// フレームの始まりの待ち（`NetPlay::head_wait`）: 語 0 = 上限（µs。0 = 無効）、語 1 = 始まりで待った回数、語 2 = 待った時間の合計（µs。u32 で
/// 頭打ち）、語 3 = 上限に当たって戻った回数。
const S_HEAD: usize = S_INPUT + INPUT_WORDS;
const HEAD_WORDS: usize = 4;
/// 繋がらなかった理由を読むための診断: 送った HELLO の数（候補の種類ごとに 4 語。`link::kind_slot` の並び）・相手の候補の数（4 語）・
/// 今のセッションを作った時刻（ms）。自分の候補・送りの誤り・STUN の聞き直しは受信側が持つので、読み取りのスレッドが `NetSnapshot::diag` へ足す
/// （`script_win::net_status`）。最後の 1 語は、学んだ候補（`Link::hello_sent_learned`）へ送った HELLO の数（種類の枠には混ぜない）。
const S_DIAG: usize = S_HEAD + HEAD_WORDS;
const DIAG_WORDS: usize = 2 * link::KIND_SLOTS + 2;
/// 同期ズレでマッチを止めたこと（`NetPlay::stop_flags`）: 語 0 = 印（bit0 止めた・bit1 Esc で戻った・bit2 相手の BYE(同期ズレ) で止めた）、
/// 語 1 = 止めた間の描画だけのフレームの数、語 2 = 止めるまでに使ったフレームの数。
const S_STOP: usize = S_DIAG + DIAG_WORDS;
const STOP_WORDS: usize = 3;
/// 遅れの量の切替（両者の Ctrl＋数字。`rollback::Stats` の seg の途中の D の予定 / delay 方式の `lockstep::Stats` の要求）:
/// 語 0〜10 = ネットワークへ渡した押下・そのうち同じ値・要求した・諦めた・要求を取った・要求の重ね・予定の数（rollback）・埋めた・捨てた・
/// 遅れて替えた・取り消した、語 11 = ゲートの前に持った押下、語 12・13 = seg の始まりの D を直に替えた数・auto を使った数（rollback の実行。
/// delay 方式は 0）、語 14・15 = 数字キーで渡した押下・そのうち今の値・旗と同じ（両形式）、語 16 = auto の seg の始まりの見直しで
/// 変わりにくくして据え置いた数、語 17〜 = 予定の記録（新しい `DSW_SHOWN` 個。1 件 3 語: `pack_dsw`）。
const S_DSW: usize = S_STOP + STOP_WORDS;
const DSW_HEAD_SETS: usize = 12;
const DSW_AUTO: usize = 13;
const DSW_DIGITS: usize = 14;
const DSW_DIGIT_UNCHANGED: usize = 15;
const DSW_AUTO_HELD: usize = 16;
const DSW_COUNTS: usize = 17;
const _: () = assert!(DSW_AUTO_HELD < DSW_COUNTS, "数の欄の後に予定の記録を置く");
/// 本ログに出す予定の記録の数（`rollback::D_SWITCH_LOG` の新しい方から）。
const DSW_SHOWN: usize = 8;
/// 予定の記録 1 件の語の数（`pack_dsw`）。
const DSW_REC_WORDS: usize = 3;
const DSW_WORDS: usize = DSW_COUNTS + DSW_REC_WORDS * DSW_SHOWN;
const _: () = assert!(DSW_SHOWN <= higanr_core::rollback::D_SWITCH_LOG);
/// 対戦の形式の切替（キャラセレクトの Tab）と、ホストの表示用の状態（HOST_STATE）の数: 語 0〜5 = Tab の押下・効かなかった Tab・
/// 形式の切替を求めた数・入れた形式の切替の STEP・断った・value が読めなかった（`netplay::Stats::tab_presses` ほか）、語 6 = 印
/// （bit0 両者が rollback ができる・bit1 今の形式が rollback）、語 7〜9 = セッションが入れ替えた数・形式が実際に替わった数・セッションが断った数
/// （`link::FormatStats`）、語 10〜12 = ホストが送った・ゲストが受けた・古い便として捨てた HOST_STATE、
/// 語 13〜 = 切替の記録（新しい `FORMAT_LOG` 件。1 件 2 語: `pack_fmt`）。
const S_FMT: usize = S_DSW + DSW_WORDS;
const F_TABS: usize = 0;
const F_TAB_IGNORED: usize = 1;
const F_ASKED: usize = 2;
const F_STEPS: usize = 3;
const F_REFUSED: usize = 4;
const F_BAD: usize = 5;
const F_FLAGS: usize = 6;
const F_APPLIED: usize = 7;
const F_CHANGED: usize = 8;
const F_CORE_REFUSED: usize = 9;
const F_HS_SENT: usize = 10;
const F_HS_RX: usize = 11;
const F_HS_STALE: usize = 12;
const F_LOG: usize = 13;
const FMT_REC_WORDS: usize = 2;
const FMT_WORDS: usize = F_LOG + FMT_REC_WORDS * higanr_core::link::FORMAT_LOG;
const FMT_ABLE: u32 = 1;
const FMT_ROLLBACK: u32 = 2;

/// 形式の切替の記録 1 件を 2 語に: 語 0 = 使った枠の印（bit31。空の枠 0 と区別する）| 相手の押下 << 15 | auto の旗 << 14 | 前の形式
/// （rollback = 1）<< 13 | 新しい形式 << 12 | 新しい D（0〜8 なので下位 4 ビット）、語 1 = seg。
fn pack_fmt(r: &higanr_core::link::FormatRec) -> [u32; FMT_REC_WORDS] {
    let is_rb = |m: Mode| u32::from(m == Mode::Rollback);
    [
        1 << 31
            | u32::from(r.by_peer) << 15
            | u32::from(r.value.auto) << 14
            | is_rb(r.from) << 13
            | is_rb(r.value.mode) << 12
            | u32::from(r.value.delay & 0xF),
        u32::from(r.seg),
    ]
}

fn fmt_rec_text(w: [u32; FMT_REC_WORDS]) -> Option<String> {
    if w[0] & (1 << 31) == 0 {
        return None;
    }
    let name = |b: u32| if b != 0 { "rollback" } else { "delay" };
    Some(format!(
        "seg {} で {} → {}（D {}・auto {}。{}の押下）",
        w[1],
        name(w[0] >> 13 & 1),
        name(w[0] >> 12 & 1),
        w[0] & 0xF,
        if w[0] & (1 << 14) != 0 {
            "あり"
        } else {
            "なし"
        },
        if w[0] & (1 << 15) != 0 {
            "相手"
        } else {
            "自分"
        },
    ))
}

/// 形式の切替と HOST_STATE の欄（`S_FMT` からの並び）を作る。繋がる前は `conn` が `None` で全部 0。純粋な関数なので、ネットワークを作らずに並びを検査できる。
fn fmt_words(
    s: &crate::netplay::Stats,
    conn: Option<&higanr_core::link::Connected>,
) -> [u32; FMT_WORDS] {
    let mut v = [0u32; FMT_WORDS];
    v[F_TABS] = s.tab_presses;
    v[F_TAB_IGNORED] = s.tab_ignored;
    v[F_ASKED] = s.format_asked;
    v[F_STEPS] = s.format_steps;
    v[F_REFUSED] = s.format_refused;
    v[F_BAD] = s.format_bad;
    let Some(c) = conn else {
        return v;
    };
    v[F_FLAGS] = if c.rollback_able() { FMT_ABLE } else { 0 }
        | if c.mode() == Mode::Rollback {
            FMT_ROLLBACK
        } else {
            0
        };
    let f = c.format_stats();
    v[F_APPLIED] = f.applied;
    v[F_CHANGED] = f.changed;
    v[F_CORE_REFUSED] = f.refused;
    let cc = c.counts();
    let sat = |x: u64| x.min(u64::from(u32::MAX)) as u32;
    v[F_HS_SENT] = sat(cc.host_state_sent);
    v[F_HS_RX] = sat(cc.host_state_rx);
    v[F_HS_STALE] = sat(cc.host_state_stale);
    let n = f.applied as usize;
    let shown = n.min(higanr_core::link::FORMAT_LOG);
    for k in 0..shown {
        let rec = f
            .log
            .get((n - shown + k) % higanr_core::link::FORMAT_LOG)
            .copied();
        if let Some(rec) = rec {
            for (j, w) in pack_fmt(&rec).into_iter().enumerate() {
                if let Some(slot) = v.get_mut(F_LOG + FMT_REC_WORDS * k + j) {
                    *slot = w;
                }
            }
        }
    }
    v
}

const STOP_HELD: u32 = 1;
const STOP_RELEASED: u32 = 2;
const STOP_BY_PEER: u32 = 4;
const GATE_ON: u32 = 1;
const GATE_LOCAL: u32 = 2;
const GATE_PEER: u32 = 4;
const GATE_EXPIRED: u32 = 8;
/// 相手にゲートが無いと見て閉じた（`NoMatch::PeerHasNoGate`。READY が来ないまま成立後の通信が来た）。
const GATE_NO_GATE: u32 = 16;

/// 予定の記録 1 件の 3 語: 語 0 = seg << 16 | 相手の押下 << 12 | 前の D << 4 | 新しい D（D は 0〜9 なので 4 ビットずつ）、語 1 = 替える取得の
/// フレーム、語 2 = ネットワークに載った F（ゲストが遅れて受けたら語 1 より小さい）。両台の記録を (seg, ネットワークの F, 前の D, 新しい D) で突き合わせるため。
fn pack_dsw(x: &higanr_core::rollback::DSwitch) -> [u32; DSW_REC_WORDS] {
    [
        u32::from(x.seg) << 16
            | u32::from(x.by_peer) << 12
            | u32::from(x.from & 0xF) << 4
            | u32::from(x.to & 0xF),
        x.frame,
        x.wire_frame,
    ]
}

fn unpack_dsw(w: [u32; DSW_REC_WORDS]) -> higanr_core::rollback::DSwitch {
    higanr_core::rollback::DSwitch {
        seg: (w[0] >> 16) as u16,
        frame: w[1],
        wire_frame: w[2],
        from: ((w[0] >> 4) & 0xF) as u8,
        to: (w[0] & 0xF) as u8,
        by_peer: w[0] & (1 << 12) != 0,
    }
}

/// 遅れの量の切替の欄（`S_DSW` からの並び）を作る。rollback の実行は `r`、delay 方式の実行は `l`（rollback の欄は 0 のまま）。
/// 数字キーの欄は両形式とも `s` からコピーする。確保しない。純粋な関数なので、ネットワークを作らずに並びを検査できる。
fn dsw_words(
    s: &crate::netplay::Stats,
    r: Option<&higanr_core::rollback::Stats>,
    l: Option<&higanr_core::lockstep::Stats>,
) -> [u32; DSW_WORDS] {
    let mut v = [0u32; DSW_WORDS];
    v[0] = s.delay_presses;
    v[1] = s.delay_presses_unchanged;
    v[DSW_DIGITS] = s.digit_presses;
    v[DSW_DIGIT_UNCHANGED] = s.digit_unchanged;
    if let Some(r) = r {
        v[2] = r.wish_sent;
        v[3] = r.wish_expired;
        v[4] = r.wish_taken;
        v[5] = r.wish_dup;
        v[6] = r.d_switches;
        v[7] = r.d_filled;
        v[8] = r.d_dropped;
        v[9] = r.d_late;
        v[10] = r.d_cancelled;
        v[11] = r.d_held;
        v[DSW_HEAD_SETS] = r.d_head_sets;
        v[DSW_AUTO] = r.d_auto;
        v[DSW_AUTO_HELD] = r.d_auto_held;
        let n = r.d_switches as usize;
        let shown = n.min(DSW_SHOWN);
        for k in 0..shown {
            let x = r
                .d_switch_log
                .get((n - shown + k) % higanr_core::rollback::D_SWITCH_LOG)
                .copied()
                .unwrap_or_default();
            for (j, w) in pack_dsw(&x).into_iter().enumerate() {
                if let Some(slot) = v.get_mut(DSW_COUNTS + DSW_REC_WORDS * k + j) {
                    *slot = w;
                }
            }
        }
    } else if let Some(l) = l {
        v[2] = l.wish_sent;
        v[3] = l.wish_expired;
        v[4] = l.wish_taken;
        v[5] = l.wish_dup;
        v[8] = l.discarded_captures;
        v[DSW_AUTO_HELD] = l.auto_held;
    }
    v
}

fn gate_flags(g: Option<&higanr_core::match_gate::Gate>) -> u32 {
    g.map_or(0, |g| {
        GATE_ON
            | if g.local() { GATE_LOCAL } else { 0 }
            | if g.peer() { GATE_PEER } else { 0 }
            | if g.expired() { GATE_EXPIRED } else { 0 }
            | if g.closed() == Some(NoMatch::PeerHasNoGate) {
                GATE_NO_GATE
            } else {
                0
            }
    })
}

/// CHECK を 4 語に: seg | 乱数の状態 << 16、frame、抽選回数、1P ライフ | 2P ライフ << 8 | 1 << 16（ある印）。
fn pack_check(c: &Check) -> [u32; 4] {
    [
        u32::from(c.seg) | u32::from(c.rng_state) << 16,
        c.frame,
        c.rng_count,
        u32::from(c.life_1p) | u32::from(c.life_2p) << 8 | 1 << 16,
    ]
}

/// `pack_check` の逆（印が無ければ None）。
fn unpack_check(w: [u32; 4]) -> Option<Check> {
    (w[3] & 1 << 16 != 0).then_some(Check {
        seg: w[0] as u16,
        rng_state: (w[0] >> 16) as u16,
        frame: w[1],
        rng_count: w[2],
        life_1p: w[3] as u8,
        life_2p: (w[3] >> 8) as u8,
    })
}

/// 最後に捨てた送り元を 2 語に: IPv4 のアドレス、ポート | 1 << 16（ある印）| IPv6 なら 1 << 17（IPv6 はアドレスを運ばない）。
/// 2 語は別々に読まれるので、送り元が変わった瞬間はアドレスとポートが別の送り元のものになりうる（ログ用）。
fn pack_from(a: Option<SocketAddr>) -> [u32; 2] {
    match a {
        None => [0, 0],
        Some(SocketAddr::V4(v)) => [u32::from(*v.ip()), 1 << 16 | u32::from(v.port())],
        Some(SocketAddr::V6(v)) => [0, 1 << 16 | 1 << 17 | u32::from(v.port())],
    }
}

fn from_text(ip: u32, port: u32) -> Option<String> {
    if port & 1 << 16 == 0 {
        return None;
    }
    if port & 1 << 17 != 0 {
        return Some("IPv6".to_string());
    }
    Some(format!("{}:{}", Ipv4Addr::from(ip), port & 0xFFFF))
}

/// 自分の SELECT を 2 語に: キャラ | 色 << 8 | 初期ライフ << 16 | チャージ型 << 24、マップ | 難易度 << 8 | 1 << 16（ある印）。
fn pack_select(s: Option<Select>) -> [u32; 2] {
    s.map_or([0, 0], |s| {
        [
            u32::from_le_bytes([s.chara, s.color, s.life, s.charge]),
            u32::from_le_bytes([s.map, s.difficulty, 1, 0]),
        ]
    })
}

/// 揃った値を 2 語に: 相手のキャラ・色・初期ライフ・チャージ型、マップ | 難易度 << 8 | 1 << 16 | 食い違いの印 << 24
/// （bit0 マップ・bit1 難易度・bit2 ダイジェスト）。
fn pack_agreed(a: Option<Agreed>) -> [u32; 2] {
    a.map_or([0, 0], |a| {
        let m = &a.mismatch;
        let flags = u8::from(m.map) | u8::from(m.difficulty) << 1 | u8::from(m.option_digest) << 2;
        [
            u32::from_le_bytes([a.peer.chara, a.peer.color, a.peer.life, a.peer.charge]),
            u32::from_le_bytes([a.map, a.difficulty, 1, flags]),
        ]
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetSnapshot {
    pub phase: u32,
    /// 終わりの種類（0 = まだ。`end_code`）と、その中身の数（理由の番号・フレーム・値）。
    pub end: u32,
    pub end_detail: u32,
    pub ticks: u32,
    pub played: u32,
    pub zero: u32,
    pub stalls: u32,
    pub max_stall_run: u32,
    pub hold_frames: u32,
    pub local_reads: u32,
    pub segs_started: u32,
    pub charge_write_failures: u32,
    pub check_matches: u32,
    pub delay: u32,
    /// 接続の手順を作り直した回数と、直前の 1 回（`restart_code`。0 = まだ）。
    pub hs_restarts: u32,
    pub hs_restart_last: u32,
    pub local_scene2: u32,
    /// セッションの受信の数（`link::RecvCounts`。セッションを作り直した後の分は数え直し）。
    pub recv_in: u32,
    pub recv_foreign: u32,
    pub recv_bad_tag: u32,
    pub recv_wrong_session: u32,
    /// 候補に無い送り元を学んだ数と、上限で断った数（`link::RecvCounts::learned` / `learn_refused`）。
    pub recv_learned: u32,
    pub recv_learn_refused: u32,
    /// 最後に捨てた送り元（`pack_from` の 2 語。読むなら `last_foreign_text`）。
    pub last_foreign_ip: u32,
    pub last_foreign_port: u32,
    /// 繋がりのセッションの INPUT の受信の数（`link::ConnCounts`。`input_text` で読む）。
    pub input_accepted: u32,
    pub input_prev_seg: u32,
    pub input_prev_replies: u32,
    pub input_other_seg: u32,
    /// 捨てた INPUT の全部（`input_guest_lat` / `input_bad_lat` を含む）。
    pub input_rejected: u32,
    pub input_guest_lat: u32,
    pub input_bad_lat: u32,
    // ---- 試験のまとめ（`trial_summary`）----
    /// 繋がった（ことがある）か。
    pub connected: bool,
    /// AGREE で決まった対戦の形が rollback か（false = delay 方式、または繋がる前）。
    pub rollback: bool,
    pub path_kind: u32,
    /// 採った経路が、学んだ候補（接続コードに無かった送り元。`Link::path_is_learned`）か。
    pub path_learned: bool,
    pub connect_wait_ms: u32,
    /// 最後に測った RTT（ms。測れていなければ None）。
    pub rtt_ms: Option<u32>,
    // ---- 繋がらなかった理由を読むための診断（`diag_text`）----
    /// 今のセッションが送った HELLO の数と、相手の候補の数（どちらも候補の種類ごと。`link::kind_slot` の並び）。セッションを作る前は相手の候補が全部 0。
    pub hello_sent: [u32; link::KIND_SLOTS],
    pub peer_cands: [u32; link::KIND_SLOTS],
    /// 学んだ候補へ送った HELLO の数（`hello_sent` の種類の枠には入れない）。
    pub hello_sent_learned: u32,
    /// 今のセッションを作った時刻（ネットワークを開いてからの ms）。
    pub link_started_ms: u32,
    /// 受信側が持つ診断。`NET_STATUS` の語には無く、読み取りのスレッドが `script_win::net_status` で足す（既定は全部 0 / なし）。
    pub diag: crate::net_udp::NetDiag,
    /// ネットの選択で入った押下・カーソルを合わせられなかった数・押下なしで選択を出た数・繋がる前に落とした決定の数。
    pub menu_applied: u32,
    pub menu_align_miss: u32,
    pub menu_lost: u32,
    pub menu_pre_link_decides: u32,
    /// 揃った値の食い違いの印（bit0 マップ・bit1 難易度・bit2 ダイジェスト。揃っていなければ None）。
    pub agreed_mismatch: Option<u8>,
    /// CHECK のズレ（自分の CHECK と、相手の CHECK。ズレていなければ None）。
    pub desync: Option<(Check, Check)>,
    /// 同期ズレでマッチを止めたこと（`NetPlay::stop_flags` の印）と、止めた間の描画だけのフレームの数・止めるまでに使ったフレームの数。
    pub stop: u32,
    pub stop_ticks: u32,
    pub stop_played: u32,
    /// 対戦の成立の印（`gate_flags`。0 = ゲートが無い・繋がる前）と、READY を送った・受けた・そのうち重複の数。
    pub gate: u32,
    pub ready_sent: u32,
    pub ready_received: u32,
    pub ready_dup: u32,
    /// この実行は、つながりが終わってもネットワークを開き直せる（ゲーム内のメニューの実行）。`NetStatus` の語には無く、
    /// 読み取りのスレッドが `script_win::net_status` で置く。
    pub relink: bool,
}

/// 作り直しの直前の 1 回を 1 語に（0 = まだ。下位 4 ビットはやめた理由: 1 = 同じ nonce・2 = 1 通も届かない・3 = 届いたが
/// 繋がらない。次の 4 ビットは形: 1 = 最初から・2 = 引き継ぐ）。
pub fn restart_code(last: Option<(FailReason, Restart)>) -> u32 {
    last.map_or(0, |(why, kind)| {
        let w = match why {
            FailReason::SameNonce => 1,
            FailReason::NothingReceived => 2,
            FailReason::NoHandshake => 3,
            // バージョン違い・ビルド違いは作り直さない（`Handshake::restart` が断る）ので、ここへは来ない。来ても「分からない」の側に置く。
            FailReason::OtherVersion { .. } | FailReason::OtherBuild { .. } => 4,
        };
        let k = match kind {
            Restart::Fresh => 1,
            Restart::Resume => 2,
        };
        w | k << 4
    })
}

/// `S_CONN` の語 0: bit0 = 繋がった、bit1 = AGREE で決まった形が rollback（試験のまとめの RTT の言い方が変わる）、bit2 = 採った経路が
/// 学んだ候補（コードに無かった送り元）、bit8 以上 = 経路の種類。
fn pack_conn(connected: bool, rollback: bool, learned: bool, kind: u32) -> u32 {
    u32::from(connected) | u32::from(rollback) << 1 | u32::from(learned) << 2 | kind << 8
}

pub(crate) fn phase_code(p: Phase) -> u32 {
    match p {
        Phase::Unconnected => 0,
        Phase::Selecting => 1,
        Phase::Holding => 2,
        Phase::Loading => 3,
        Phase::Lockstep => 4,
        Phase::Ended => 5,
    }
}

/// 接続の手順をやめた理由の番号（終わりの種類 1 の中身。`text` の行と試験のまとめの `link_failure` が読む）。同じ nonce 0・1 通も届かない 1・
/// 届いたが繋がらない 2（バージョン違いは中身に相手のバージョンを持つので別の種類 `END_OTHER_VERSION`）。
fn fail_code(r: FailReason) -> u32 {
    match r {
        FailReason::SameNonce => 0,
        FailReason::NothingReceived => 1,
        FailReason::NoHandshake => 2,
        FailReason::OtherVersion { theirs } => u32::from(theirs),
        FailReason::OtherBuild { theirs } => theirs,
    }
}

/// 終わりの種類: 相手の HiganRTool のバージョンが違うので、繋がる前にやめた（中身は相手のバージョン）。`end_code` の 11。
pub(crate) const END_OTHER_VERSION: u32 = 11;
/// 終わりの種類: 相手のビルドが違うので、繋がる前にやめた（中身は相手のビルド番号）。`end_code` の 12。
pub(crate) const END_OTHER_BUILD: u32 = 12;

/// 観戦者の終わりを対戦の終わりの表（`NetSnapshot::text` の `ENDS`）の番号にコピーする。ホストの終わりは「相手の BYE」、黙った・
/// 流れが切れたは「相手が黙った」、コードが貼られなかったは「接続できなかった（1 通も届かない）」。
fn watch_end_code(e: Option<WatchEnd>) -> (u32, u32) {
    match e {
        None => (0, 0),
        Some(WatchEnd::HostEnded(r)) => (2, r as u32),
        Some(WatchEnd::HostSilent | WatchEnd::Lost) => (3, 0),
        Some(WatchEnd::GaveUp) => (1, fail_code(FailReason::NothingReceived)),
        Some(WatchEnd::BadSelect(b)) => (6, b.value as u32),
        Some(WatchEnd::Stuck(st)) => (7, u32::from(st.step)),
        Some(WatchEnd::HoldTimeout) => (7, 0),
        Some(WatchEnd::Quit) => (5, higanr_core::wire::ByeReason::Cancelled as u32),
    }
}

/// 終わりの種類と中身の数。
pub fn end_code(e: Option<End>) -> (u32, u32) {
    match e {
        None => (0, 0),
        Some(End::HandshakeFailed(r @ FailReason::OtherVersion { .. })) => {
            (END_OTHER_VERSION, fail_code(r))
        }
        Some(End::HandshakeFailed(r @ FailReason::OtherBuild { .. })) => {
            (END_OTHER_BUILD, fail_code(r))
        }
        Some(End::HandshakeFailed(r)) => (1, fail_code(r)),
        Some(End::PeerBye(r)) => (2, r as u32),
        Some(End::Dead) => (3, 0),
        Some(End::Desync { local, .. }) => (4, local.frame),
        Some(End::Closed(r)) => (5, r as u32),
        Some(End::BadSelect(b)) => (6, b.value as u32),
        Some(End::MenuStuck(st)) => (7, u32::from(st.step)),
        // 成立しないまま閉じた。理由ごとに種類を分ける（中身の数は 0、相手の BYE で閉じたものだけ BYE の理由の番号）。
        Some(End::Unmatched(NoMatch::TimedOut)) => (8, 0),
        Some(End::Unmatched(NoMatch::PeerHasNoGate)) => (9, 0),
        Some(End::Unmatched(NoMatch::PeerLeft)) => (10, ByeReason::Cancelled as u32),
    }
}

impl NetStatus {
    pub const fn new() -> Self {
        NetStatus {
            w: [const { AtomicU32::new(0) }; STATUS_WORDS],
        }
    }

    /// コピーする（ゲームのスレッド。確保しない）。
    pub fn publish(&self, np: &NetPlay) {
        let s = np.stats();
        let (end, detail) = end_code(np.end());
        let c = |v: u64| v.min(u64::from(u32::MAX)) as u32;
        // rollback の実行では D は今の D（`cur_delay`。seg の途中で Ctrl＋数字の予定で替わる）。待たない作りなので lockstep の遅れとは別物。
        let (check_ok, delay) = np.connected().map_or((0, 0), |c| {
            (c.counts().check_matches, u32::from(c.cur_delay()))
        });
        let vals = [
            phase_code(np.phase()),
            end,
            detail,
            c(s.ticks),
            c(s.played),
            c(s.zero_ticks),
            c(s.stalls),
            s.max_stall_run,
            c(s.hold_frames),
            c(s.local_reads),
            s.segs_started,
            s.charge_write_failures,
            check_ok,
            delay,
        ];
        for (a, v) in self.w.iter().zip(vals) {
            a.store(v, Ordering::Relaxed);
        }
        let (own, agreed) = np.selects();
        let [o0, o1] = pack_select(own);
        let [a0, a1] = pack_agreed(agreed);
        let menu = [
            c(s.menu_frames),
            s.menu_applied,
            c(s.menu_held),
            s.menu_align_miss,
            s.menu_lost,
            o0,
            o1,
            a0,
            a1,
        ];
        for (a, v) in self.w.iter().skip(S_MENU_FRAMES).zip(menu) {
            a.store(v, Ordering::Relaxed);
        }
        let [h0, h1, h2, h3, h4] = s.wait_over_hist;
        let timing = [
            s.lat_requested,
            s.lat_busy,
            s.lat_refused,
            s.switches,
            c(s.margin_unknown),
            c(s.wait_timeouts),
            s.wait_over_max_us,
            c(h0),
            c(h1),
            c(h2),
            c(h3),
            c(h4),
        ];
        for (a, v) in self.w.iter().skip(S_TIMING).zip(timing) {
            a.store(v, Ordering::Relaxed);
        }
        for (a, v) in self
            .w
            .iter()
            .skip(S_HS)
            .zip([s.hs_restarts, restart_code(s.hs_restart_last)])
        {
            a.store(v, Ordering::Relaxed);
        }
        self.w[S_LOCAL_SCENE2].store(s.local_scene2, Ordering::Relaxed);
        self.w[S_PORTRAIT].store(s.menu_portrait_called, Ordering::Relaxed);
        self.w[S_PORTRAIT + 1].store(s.menu_portrait_refused, Ordering::Relaxed);
        self.w[S_PRE_LINK].store(s.menu_pre_link_decides, Ordering::Relaxed);
        let conn = np.connected();
        let kind = np.link_path().map_or(0, |a| u32::from(kind_of(a).to_u8()));
        let rtt = conn
            .and_then(|c| c.liveness().rtt_ms())
            .map_or(NO_MARGIN, |m| m.min(NO_MARGIN - 1));
        for (a, v) in self.w.iter().skip(S_CONN).zip([
            pack_conn(
                conn.is_some(),
                conn.is_some_and(|c| c.mode() == Mode::Rollback),
                np.link_path_is_learned(),
                kind,
            ),
            np.connect_wait_ms().unwrap_or(0),
            rtt,
        ]) {
            a.store(v, Ordering::Relaxed);
        }
        // CHECK のズレは 1 回だけ起きて終わる（`End::Desync`）ので、そのときだけ 8 語を書く（書いた後は変わらない）。
        if let Some(End::Desync { local, remote }) = np.end() {
            for (a, v) in self
                .w
                .iter()
                .skip(S_DESYNC)
                .zip(pack_check(&local).into_iter().chain(pack_check(&remote)))
            {
                a.store(v, Ordering::Relaxed);
            }
        }
        let gate = conn.and_then(|c| c.match_gate());
        let gc = gate.map(|g| g.counts()).unwrap_or_default();
        for (a, v) in self.w.iter().skip(S_GATE).zip([
            gate_flags(gate),
            gc.ready_sent,
            gc.ready_received,
            gc.ready_dup,
        ]) {
            a.store(v, Ordering::Relaxed);
        }
        let pick = np.delay_pick().map_or([0; PICK_WORDS], |p| {
            [
                1 | if p.is_host() { 2 } else { 0 } | if p.chosen().is_some() { 4 } else { 0 },
                p.chosen().map_or(0, u32::from),
                p.digits(),
            ]
        });
        for (a, v) in self.w.iter().skip(S_PICK).zip(pick) {
            a.store(v, Ordering::Relaxed);
        }
        self.publish_dsw(np, &s);
        for (a, x) in self.w.iter().skip(S_FMT).zip(fmt_words(&s, np.connected())) {
            a.store(x, Ordering::Relaxed);
        }
        // セッションが無い（繋がる前）間は 0 のまま。セッションを作り直すと数え直し（受信の数と同じ）。
        let ic = np.connected().map(|c| c.counts()).unwrap_or_default();
        let input = [
            c(ic.input_accepted),
            c(ic.input_prev_seg),
            c(ic.input_prev_replies),
            c(ic.input_other_seg),
            c(ic.input_rejected),
            c(ic.input_guest_lat),
            c(ic.input_bad_lat),
        ];
        for (a, v) in self.w.iter().skip(S_INPUT).zip(input) {
            a.store(v, Ordering::Relaxed);
        }
        let head = [
            np.head_wait_cap_us(),
            c(s.head_waits),
            c(s.head_wait_us_total),
            c(s.head_wait_caps),
        ];
        for (a, v) in self.w.iter().skip(S_HEAD).zip(head) {
            a.store(v, Ordering::Relaxed);
        }
        // 止めたときだけ書く（止めないマッチの毎 tick は分岐 1 回だけ。印は戻らず、つなぎ直しの `reset` が消す）。
        let stop = np.stop_flags();
        if stop != 0 {
            for (a, v) in self
                .w
                .iter()
                .skip(S_STOP)
                .zip([stop, c(s.stop_ticks), c(s.stop_played)])
            {
                a.store(v, Ordering::Relaxed);
            }
        }
        let diag = np
            .link_hello_sent()
            .into_iter()
            .chain(np.link_peer_candidates())
            .chain([np.link_started_ms(), np.link_hello_sent_learned()]);
        for (a, v) in self.w.iter().skip(S_DIAG).zip(diag) {
            a.store(v, Ordering::Relaxed);
        }
        let rc = np.recv_counts();
        let [ip, port] = pack_from(rc.last_foreign);
        let recv = [
            c(rc.datagrams_in),
            c(rc.foreign),
            c(rc.bad_tag),
            c(rc.wrong_session),
            ip,
            port,
            c(rc.learned),
            c(rc.learn_refused),
        ];
        for (a, v) in self.w.iter().skip(S_RECV).zip(recv) {
            a.store(v, Ordering::Relaxed);
        }
        let log = np.step_log();
        self.w[S_STEP_COUNT].store(log.count, Ordering::Relaxed);
        for (i, r) in log.iter().enumerate() {
            let st = r.step;
            let kind = u32::from(st.kind.to_u8());
            let w0 = u32::from(st.step) << 16 | u32::from(st.screen) << 8 | u32::from(st.value);
            let w1 = kind | u32::from(r.own) << 4;
            if let Some(a) = self.w.get(S_STEPS + 2 * i) {
                a.store(w0, Ordering::Relaxed);
            }
            if let Some(a) = self.w.get(S_STEPS + 2 * i + 1) {
                a.store(w1, Ordering::Relaxed);
            }
        }
    }

    /// 遅れの量の切替の数と予定の記録をコピーする（確保しない。`S_DSW` の doc）。rollback は `rollback::Stats`、delay 方式は `lockstep::Stats`
    /// （予定の欄は 0）。繋がる前は 0。
    fn publish_dsw(&self, np: &NetPlay, s: &crate::netplay::Stats) {
        let conn = np.connected();
        let r = conn.and_then(|c| c.rollback()).map(|r| r.stats());
        // rollback の実行でなければ（繋がる前も含む）delay 方式の数を見る。rollback の実行では lockstep の数を読まない。
        let l = match (&r, conn) {
            (None, Some(c)) => Some(c.lockstep().stats()),
            _ => None,
        };
        self.store_dsw(dsw_words(s, r.as_ref(), l.as_ref()));
    }

    fn store_dsw(&self, v: [u32; DSW_WORDS]) {
        for (a, x) in self.w.iter().skip(S_DSW).zip(v) {
            a.store(x, Ordering::Relaxed);
        }
    }

    /// 遅れの量の切替の行（ログ用。`timing_text` の後ろ）。予定の記録は 1 件 1 行（どちらが・いくつに・どのフレームで・埋めた / 捨てた数）。
    pub fn dsw_text(&self) -> String {
        let l = |i: usize| {
            self.w
                .get(S_DSW + i)
                .map_or(0, |a| a.load(Ordering::Relaxed))
        };
        let mut out = format!(
            "遅れの量の切替（Ctrl+数字キー）: 初めの値をネットワークへ渡した {}（今の値と同じ {}）・数字キーで渡した押下 {}（今の値・旗と同じ {}）/ 要求した {}・届いた印を見ずに諦めた {} / 相手の要求を取った {}・重ね {} / seg の途中の予定 {}（埋めた入力ワード {}・捨てた取得 {}・遅れて替えた {}・届く前に取り消した {}・ゲートの前に持った押下 {}）/ seg の先頭の D を直に替えた {}・auto を使った {}（今の D と同じだった分も含む）・そのうち変わりにくくして据え置いた {}",
            l(0),
            l(1),
            l(DSW_DIGITS),
            l(DSW_DIGIT_UNCHANGED),
            l(2),
            l(3),
            l(4),
            l(5),
            l(6),
            l(7),
            l(8),
            l(9),
            l(10),
            l(11),
            l(DSW_HEAD_SETS),
            l(DSW_AUTO),
            l(DSW_AUTO_HELD)
        );
        let shown = (l(6) as usize).min(DSW_SHOWN);
        for k in 0..shown {
            let at = DSW_COUNTS + DSW_REC_WORDS * k;
            let x = unpack_dsw([l(at), l(at + 1), l(at + 2)]);
            let (what, n) = if x.to > x.from {
                ("埋めた", x.to - x.from)
            } else {
                ("捨てる取得", x.from - x.to)
            };
            // 末尾の「ネットワークの F」は、ホストが INPUT の lat に載せた F。同じ予定なら両台で (seg, ネットワークの F, 前の D, 新しい D) が同じになる
            // （取得のフレームはゲストが遅れて受けたときネットワークの F より大きくなる）。
            out.push_str(&format!(
                "\n  seg {} の取得のフレーム {} から D {} → {}（{}。{what} {n}）ネットワークの F {}",
                x.seg,
                x.frame,
                x.from,
                x.to,
                if x.by_peer {
                    "相手の押下"
                } else {
                    "自分の押下"
                },
                x.wire_frame
            ));
        }
        out
    }

    /// 観戦者の数をコピーする（確保しない）。対戦の `publish` と同じ欄のうち観戦者に意味のあるものだけ。残りは 0 のまま。
    /// 観戦者だけの数は `spectate_watch::WATCH_STATUS`。
    pub fn publish_watch(&self, wp: &WatchPlay) {
        let s = wp.stats();
        let c = |v: u64| v.min(u64::from(u32::MAX)) as u32;
        let (end, detail) = watch_end_code(wp.end());
        let joined = wp.is_joined();
        let kind = wp.host_addr().map_or(0, |a| u32::from(kind_of(a).to_u8()));
        let vals = [
            (S_PHASE, phase_code(wp.phase())),
            (S_END, end),
            (S_END_DETAIL, detail),
            (S_TICKS, c(s.ticks)),
            (S_PLAYED, c(s.played)),
            (S_ZERO, c(s.zero_ticks)),
            (S_STALLS, c(s.stalls)),
            (S_MAX_STALL, s.max_stall_run),
            (S_HOLD, c(s.hold_frames)),
            (S_SEGS, s.matches_started),
            (S_CHARGE_FAIL, s.charge_write_failures),
            (S_MENU_FRAMES, c(s.menu_frames)),
            (S_MENU_APPLIED, s.menu_applied),
            (S_MENU_HELD, c(s.menu_held)),
            (S_MENU_ALIGN_MISS, s.menu_align_miss),
            (S_LOCAL_SCENE2, s.local_scene2),
            (S_PORTRAIT, s.menu_portrait_called),
            (S_PORTRAIT + 1, s.menu_portrait_refused),
            (S_PRE_LINK, s.pre_join_decides),
            (S_CONN, u32::from(joined) | kind << 8),
            (S_CONN + 1, s.join_wait_ms.unwrap_or(0)),
            (S_CONN + 2, NO_MARGIN),
        ];
        for (i, v) in vals {
            if let Some(a) = self.w.get(i) {
                a.store(v, Ordering::Relaxed);
            }
        }
        let log = wp.step_log();
        self.w[S_STEP_COUNT].store(log.count, Ordering::Relaxed);
        for (i, r) in log.iter().enumerate() {
            let st = r.step;
            let kind = u32::from(st.kind.to_u8());
            let w0 = u32::from(st.step) << 16 | u32::from(st.screen) << 8 | u32::from(st.value);
            if let Some(a) = self.w.get(S_STEPS + 2 * i) {
                a.store(w0, Ordering::Relaxed);
            }
            if let Some(a) = self.w.get(S_STEPS + 2 * i + 1) {
                a.store(kind | u32::from(r.own) << 4, Ordering::Relaxed);
            }
        }
    }

    pub fn menu_text(&self) -> String {
        let l = |i: usize| self.w.get(i).map_or(0, |a| a.load(Ordering::Relaxed));
        let count = l(S_STEP_COUNT);
        let mut out = format!(
            "ネットの選択: 選択のフレーム {} / 入れた押下 {} / 持ち越したフレーム {} / カーソルを合わせられなかった {} / 押下なしで選択を出た {} / 立ち絵を追わせた {} / 追わせなかった {} / 繋がる前に落とした決定 {} 回",
            l(S_MENU_FRAMES),
            l(S_MENU_APPLIED),
            l(S_MENU_HELD),
            l(S_MENU_ALIGN_MISS),
            l(S_MENU_LOST),
            l(S_PORTRAIT),
            l(S_PORTRAIT + 1),
            l(S_PRE_LINK)
        );
        let shown = (count as usize).min(STEP_LOG_LEN);
        out.push_str(&format!(
            "\n入った押下（{count} 個のうち最後の {shown} 個。step 画面 種類 value 誰）:"
        ));
        for i in 0..shown {
            let (w0, w1) = (l(S_STEPS + 2 * i), l(S_STEPS + 2 * i + 1));
            let kind = match w1 & 0xF {
                1 => "決定",
                2 => "取消",
                3 => "合わせ",
                4 => "形式",
                _ => "?",
            };
            out.push_str(&format!(
                "\n  {} {} {} {} {}",
                w0 >> 16,
                (w0 >> 8) & 0xFF,
                kind,
                w0 & 0xFF,
                if w1 & 0x10 != 0 { "自分" } else { "相手" }
            ));
        }
        let sel = |base: usize| {
            let (a, b) = (l(base).to_le_bytes(), l(base + 1).to_le_bytes());
            (a, b)
        };
        let (o, om) = sel(S_OWN_SELECT);
        if om[2] != 0 {
            out.push_str(&format!(
                "\n自分の SELECT: キャラ {} 色 {} 初期ライフ {} チャージ型 {} マップ {} 難易度 {}",
                o[0], o[1], o[2], o[3], om[0] as i8, om[1]
            ));
        }
        let (a, am) = sel(S_AGREED);
        if am[2] != 0 {
            out.push_str(&format!(
                "\n揃った値: 相手のキャラ {} 色 {} 初期ライフ {} チャージ型 {} / マップ {} 難易度 {} / 食い違い（bit0 マップ・bit1 難易度・bit2 ダイジェスト） {}",
                a[0], a[1], a[2], a[3], am[0] as i8, am[1], am[3]
            ));
        }
        out
    }

    /// 対戦の形式の切替（キャラセレクトの Tab）と HOST_STATE の行（ログ用。`S_FMT` の doc）。切替の記録は 1 件 1 行
    /// （seg・前後の形式・D・auto・どちらの押下か）。
    pub fn fmt_text(&self) -> String {
        let l = |i: usize| {
            self.w
                .get(S_FMT + i)
                .map_or(0, |a| a.load(Ordering::Relaxed))
        };
        let flags = l(F_FLAGS);
        let mut out = format!(
            "対戦の形式の切替（キャラセレクトの Tab）: rollback ができる組 {} / 今の形式 {} / Tab の押下 {}（効かなかった {}）・切替を求めた {} / 入れた切替の STEP {}（箱が入れ替えた {}・うち形式が替わった {}・断った {}+{}・value が読めない {}）\nホストの表示用の状態（HOST_STATE）: 送った {} / 受けた {} / 古い便として捨てた {}",
            if flags & FMT_ABLE != 0 {
                "はい"
            } else {
                "いいえ"
            },
            if flags & FMT_ROLLBACK != 0 {
                "rollback"
            } else {
                "delay"
            },
            l(F_TABS),
            l(F_TAB_IGNORED),
            l(F_ASKED),
            l(F_STEPS),
            l(F_APPLIED),
            l(F_CHANGED),
            l(F_REFUSED),
            l(F_CORE_REFUSED),
            l(F_BAD),
            l(F_HS_SENT),
            l(F_HS_RX),
            l(F_HS_STALE),
        );
        let shown = (l(F_APPLIED) as usize).min(higanr_core::link::FORMAT_LOG);
        for k in 0..shown {
            let at = F_LOG + FMT_REC_WORDS * k;
            if let Some(t) = fmt_rec_text([l(at), l(at + 1)]) {
                out.push_str("\n  ");
                out.push_str(&t);
            }
        }
        out
    }

    pub fn timing_text(&self) -> String {
        let l = |i: usize| {
            self.w
                .get(S_TIMING + i)
                .map_or(0, |a| a.load(Ordering::Relaxed))
        };
        let b = WAIT_OVER_BOUNDS_US;
        let p = |i: usize| {
            self.w
                .get(S_PICK + i)
                .map_or(0, |a| a.load(Ordering::Relaxed))
        };
        let pick = if p(0) & 1 == 0 {
            String::new()
        } else {
            format!(
                "遅れの量の選び: {} / 最後に作った値 {} / 数字キーを押した {} 回\n{}\n",
                if p(0) & 2 != 0 {
                    "ホスト"
                } else {
                    "ゲスト"
                },
                if p(0) & 4 != 0 {
                    p(1).to_string()
                } else {
                    "-".to_string()
                },
                p(2),
                self.dsw_text()
            )
        };
        let h = |i: usize| {
            self.w
                .get(S_HEAD + i)
                .map_or(0, |a| a.load(Ordering::Relaxed))
        };
        // フレームの始まりで相手の入力ワードを待つ（`NetPlay::head_wait`）。どちらで走ったかを読むための行。
        let head = if h(0) == 0 {
            "先頭で待つ: 無効（ini の head_wait = 0 か、ネットワークの対戦でない実行）".to_string()
        } else {
            format!(
                "先頭で待つ: 有効（上限 {} µs。ウィンドウ（b-spin / b）は格子の後、全画面（ゲーム自身の時計）はゲームがフレームを始める見込みの回だけ。rollback のモードでは待たない）/ 先頭で待った回 {} / 合計 {} µs / 上限に当たった回 {}",
                h(0),
                h(1),
                h(2),
                h(3)
            )
        };
        let fmt = self.fmt_text();
        format!(
            "{pick}latency の切替: 要求した {} / Busy で次のフレームへ回した {} / 断られた予定 {} / 使った切替 {}\n余裕が分からなかったフレーム {}（開始直後の穴埋めを含む）\n待ちが時刻で戻った {} 回。頼んだ時間からの遅れ: {} µs 未満 {} / {} 未満 {} / {} 未満 {} / {} 未満 {} / それ以上 {}（最大 {} µs）\n{head}\n{fmt}",
            l(0),
            l(1),
            l(2),
            l(3),
            l(4),
            l(5),
            b[0],
            l(7),
            b[1],
            l(8),
            b[2],
            l(9),
            b[3],
            l(10),
            l(11),
            l(6)
        )
    }

    pub fn snapshot(&self) -> NetSnapshot {
        let l = |i: usize| self.w[i].load(Ordering::Relaxed);
        let agreed = l(S_AGREED + 1).to_le_bytes();
        let check = |base: usize| unpack_check(core::array::from_fn(|i| l(base + i)));
        NetSnapshot {
            connected: l(S_CONN) & 1 != 0,
            rollback: l(S_CONN) & 2 != 0,
            path_kind: l(S_CONN) >> 8,
            path_learned: l(S_CONN) & 4 != 0,
            connect_wait_ms: l(S_CONN + 1),
            // 何もコピーしていない数（語が 0 のまま）を「RTT 0 ms」と読まないよう、繋がった印があるときだけ読む。
            rtt_ms: Some(l(S_CONN + 2)).filter(|r| *r != NO_MARGIN && l(S_CONN) & 1 != 0),
            menu_applied: l(S_MENU_APPLIED),
            menu_align_miss: l(S_MENU_ALIGN_MISS),
            menu_lost: l(S_MENU_LOST),
            menu_pre_link_decides: l(S_PRE_LINK),
            agreed_mismatch: (agreed[2] != 0).then_some(agreed[3]),
            desync: check(S_DESYNC).zip(check(S_DESYNC + 4)),
            stop: l(S_STOP),
            stop_ticks: l(S_STOP + 1),
            stop_played: l(S_STOP + 2),
            phase: l(S_PHASE),
            end: l(S_END),
            end_detail: l(S_END_DETAIL),
            ticks: l(S_TICKS),
            played: l(S_PLAYED),
            zero: l(S_ZERO),
            stalls: l(S_STALLS),
            max_stall_run: l(S_MAX_STALL),
            hold_frames: l(S_HOLD),
            local_reads: l(S_LOCAL_READS),
            segs_started: l(S_SEGS),
            charge_write_failures: l(S_CHARGE_FAIL),
            check_matches: l(S_CHECK_OK),
            delay: l(S_DELAY),
            hs_restarts: l(S_HS),
            hs_restart_last: l(S_HS + 1),
            local_scene2: l(S_LOCAL_SCENE2),
            recv_in: l(S_RECV),
            recv_foreign: l(S_RECV + 1),
            recv_bad_tag: l(S_RECV + 2),
            recv_wrong_session: l(S_RECV + 3),
            last_foreign_ip: l(S_RECV + 4),
            last_foreign_port: l(S_RECV + 5),
            recv_learned: l(S_RECV + 6),
            recv_learn_refused: l(S_RECV + 7),
            input_accepted: l(S_INPUT),
            input_prev_seg: l(S_INPUT + 1),
            input_prev_replies: l(S_INPUT + 2),
            input_other_seg: l(S_INPUT + 3),
            input_rejected: l(S_INPUT + 4),
            input_guest_lat: l(S_INPUT + 5),
            input_bad_lat: l(S_INPUT + 6),
            gate: l(S_GATE),
            ready_sent: l(S_GATE + 1),
            ready_received: l(S_GATE + 2),
            ready_dup: l(S_GATE + 3),
            hello_sent: core::array::from_fn(|i| l(S_DIAG + i)),
            peer_cands: core::array::from_fn(|i| l(S_DIAG + link::KIND_SLOTS + i)),
            link_started_ms: l(S_DIAG + 2 * link::KIND_SLOTS),
            hello_sent_learned: l(S_DIAG + 2 * link::KIND_SLOTS + 1),
            diag: crate::net_udp::NetDiag::default(),
            relink: false,
        }
    }

    /// 全部の語を 0 にする（ゲーム内のメニューの実行で、ネットワークのセッションを置く tick に。確保しない）。`publish` は大半の語を毎 tick 上書きするが、
    /// CHECK のズレの欄と入った押下のリングバッファの数より後ろの枠は上書きしないので、消さないと前のつながりの値が次の行に残る。
    pub fn reset(&self) {
        for a in &self.w {
            a.store(0, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(crate) fn fake_desync(&self, local: &Check, remote: &Check) {
        for (a, v) in self
            .w
            .iter()
            .skip(S_DESYNC)
            .zip(pack_check(local).into_iter().chain(pack_check(remote)))
        {
            a.store(v, Ordering::Relaxed);
        }
    }
}

impl Default for NetStatus {
    fn default() -> Self {
        Self::new()
    }
}

pub static NET_STATUS: NetStatus = NetStatus::new();

impl NetSnapshot {
    /// ネットワーク実行が終えた（今のつながりが終わり、この起動ではもうネットワークを開かない）。計測の終わりの判定（`observe` の `RunLen::Net`）が読む。
    /// ゲーム内のメニューの実行（`relink`）は、マッチの外へ戻ればネットワークを開き直せるので終えたことにしない。
    pub fn ended(&self) -> bool {
        self.link_ended() && !self.relink
    }

    /// 今のつながりが終わった（BYE・ズレ・相手の沈黙・接続の失敗・成立しないまま閉じたのどれか）。試験のまとめが
    /// 「繋がらずに終わった」「マッチが始まらないまま終わった」を言うのに読む。
    pub fn link_ended(&self) -> bool {
        self.phase == phase_code(Phase::Ended)
    }

    /// 接続の手順を作り直した 1 回の行（本ログ。`n` は何回目か。中身は直前の 1 回 `hs_restart_last`）。
    pub fn restart_line(&self, n: u32) -> String {
        let why = match self.hs_restart_last & 0xF {
            2 => "10 秒のあいだ相手から 1 通も届かなかった",
            3 => "相手から届いたが 10 秒で繋がらなかった",
            _ => "理由は分からない",
        };
        let kind = match self.hs_restart_last >> 4 {
            1 => "最初から作り直した",
            2 => "相手の値は引き継ぎ、自分の HELLO を送り直す",
            _ => "形は分からない",
        };
        // 診断はセッションを作った後だけ付ける。セッションの無い間は数が全部 0 で、「0 件」と「測っていない」が見分けられない。
        let diag = if self.has_link_box() {
            format!("〔{}〕", self.diag_text())
        } else {
            String::new()
        };
        format!(
            "ネットワーク: 接続の手順を作り直した（{n} 回目。{why}。{kind}）{}{diag}",
            self.dropped_recv_text()
        )
    }

    pub fn has_link_box(&self) -> bool {
        self.peer_cands.iter().sum::<u32>() > 0
    }

    /// 繋がらなかった理由を読むための診断の句（本ログの作り直しの行・「ネットワークの外」の行・試験のまとめ）。人を特定する値（アドレス・ポート）は書かない。
    pub fn diag_text(&self) -> String {
        let kinds = |n: &[u32; link::KIND_SLOTS]| {
            format!(
                "同じ PC の中 {} / 家の中 {} / 外側 {} / IPv6 {}",
                n[0], n[1], n[2], n[3]
            )
        };
        let d = &self.diag;
        let own = if d.own_cands.iter().sum::<u32>() == 0 {
            "記録なし".to_string()
        } else {
            kinds(&d.own_cands)
        };
        let age = match (d.stun_at_ms, self.has_link_box()) {
            (Some(t), true) => self.link_started_ms.checked_sub(t).map_or_else(
                || "（時刻が前後している）".to_string(),
                |ms| format!("{:.1} 秒", f64::from(ms) / 1000.0),
            ),
            (Some(_), false) => "まだ始めていない".to_string(),
            (None, _) => "外側のアドレスを得ていない".to_string(),
        };
        format!(
            "診断: 自分の候補 [{own}] / 相手の候補 [{}] / 送った HELLO [{}]（送りの誤り [{}]） / 学んだ候補 {} 個（コードに無い送り元からタグの合う手順の 1 通が届いた。上限で断った {}・学んだアドレスへ送った HELLO {}） / {} / コードの外側のアドレスを得てから接続の手順を始めるまで: {age} / {} / {} / {}",
            kinds(&self.peer_cands),
            kinds(&self.hello_sent),
            kinds(&d.send_errors),
            self.recv_learned,
            self.recv_learn_refused,
            self.hello_sent_learned,
            d.port_text(),
            d.keep_text(),
            d.second.text(),
            d.upnp.text()
        )
    }

    pub fn last_foreign_text(&self) -> Option<String> {
        from_text(self.last_foreign_ip, self.last_foreign_port)
    }

    /// 受信で捨てたものの句（作り直しの行の後ろへ付ける）。0 も出す（「1 通も来ない」と「来たが捨てた」を見分けるため）。数はセッションを作ってからの合計。
    fn dropped_recv_text(&self) -> String {
        let last = self
            .last_foreign_text()
            .map_or(String::new(), |a| format!("、最後は {a}"));
        format!(
            "（受信ソケットに届いた {} 通。捨てた受信: 候補に無い送り元 {} 通{last} / タグの誤り {} 通 / 別のセッション {} 通）",
            self.recv_in, self.recv_foreign, self.recv_bad_tag, self.recv_wrong_session
        )
    }

    /// 繋がりのセッションの INPUT の受信の句（本ログのネットワークの行の後ろへ付ける）。0 も出す。`BadLat` / `GuestLat` は捨てた数の内訳。
    pub fn input_text(&self) -> String {
        format!(
            "INPUT の受信ソケット: 受理 {} / 前の seg {}（控えを返した {}） / 別の seg で捨てた {} / ほかの理由で捨てた {}（うち lat の取り決め違い BadLat {} ・ホストが切替つきを受けた GuestLat {}）",
            self.input_accepted,
            self.input_prev_seg,
            self.input_prev_replies,
            self.input_other_seg,
            self.input_rejected,
            self.input_bad_lat,
            self.input_guest_lat
        )
    }

    pub fn text(&self) -> String {
        const PHASES: [&str; 6] = [
            "繋がる前",
            "選択",
            "止める地点",
            "読み込み",
            "lockstep",
            "終えた",
        ];
        const ENDS: [&str; 13] = [
            "まだ",
            "接続できなかった",
            "相手の BYE",
            "相手が黙った",
            "CHECK のズレ",
            "自分から終えた",
            "相手の SELECT が範囲の外",
            "押下を持ち越しすぎた（画面が分かれた）",
            "対戦が成立しないまま時間切れで閉じた",
            "相手に成立のゲートが無いので閉じた（READY が来ないまま選択・試合の通信が来た。相手が古いバージョンか offer / answer）",
            "対戦が成立しないまま相手が閉じた（相手からの BYE・取消）",
            "相手の HiganRTool のバージョンが違う",
            "相手の HiganRTool のバージョン（ビルド）が違う",
        ];
        let phase = PHASES.get(self.phase as usize).copied().unwrap_or("?");
        let end = if self.end == END_OTHER_VERSION {
            other_version_text(PROTO_VER, u8::try_from(self.end_detail).unwrap_or(u8::MAX))
        } else if self.end == END_OTHER_BUILD {
            higanr_core::net_menu::other_build_text(
                higanr_core::update::BUILD_NUMBER,
                self.end_detail,
            )
        } else {
            ENDS.get(self.end as usize)
                .copied()
                .unwrap_or("?")
                .to_string()
        };
        format!(
            "段階 {phase} / 終わり {end}（{}） / tick {} / 使ったフレーム {} / 0 を feed {} / stall {}（最長の連続 {}） / 止める地点で待った {} / ローカル入力を読んだ {} / 始まった試合 {} / チャージ型を書けなかった {} / CHECK の一致 {} / delay {} / 接続の手順を作り直した {} 回 / ネットワークの選択なしで場面 2 へ移った {} / {}{}",
            self.end_detail,
            self.ticks,
            self.played,
            self.zero,
            self.stalls,
            self.max_stall_run,
            self.hold_frames,
            self.local_reads,
            self.segs_started,
            self.charge_write_failures,
            self.check_matches,
            self.delay,
            self.hs_restarts,
            self.local_scene2,
            self.dropped_recv_text(),
            self.desync_line()
                .map_or(String::new(), |l| format!(" / {l}"))
        ) + &self
            .stop_line()
            .map_or(String::new(), |l| format!(" / {l}"))
            + &self
                .gate_line()
                .map_or(String::new(), |l| format!(" / {l}"))
            + &format!(" / {}", self.input_text())
    }

    pub fn gate_line(&self) -> Option<String> {
        if self.gate & GATE_ON == 0 {
            return None;
        }
        let yes = |bit: u32| {
            if self.gate & bit != 0 {
                "選んだ"
            } else {
                "まだ"
            }
        };
        Some(format!(
            "対戦の成立: 自分 {} / 相手 {}{} / READY 送った {} 通・受けた {} 通（うち重複 {} 通）",
            yes(GATE_LOCAL),
            yes(GATE_PEER),
            if self.gate & GATE_EXPIRED != 0 {
                " / 成立しないまま時間切れ"
            } else if self.gate & GATE_NO_GATE != 0 {
                " / 相手にゲートが無いので閉じた"
            } else {
                ""
            },
            self.ready_sent,
            self.ready_received,
            self.ready_dup
        ))
    }

    /// 同期ズレでマッチを止めたか（止めていなければ None）。自分が見つけたか相手の知らせか・方式・止めるまでに使ったフレーム・
    /// 止めた間の描画だけのフレーム・Esc で戻ったかを出す。
    pub fn stop_line(&self) -> Option<String> {
        if self.stop & STOP_HELD == 0 {
            return None;
        }
        let by = if self.stop & STOP_BY_PEER != 0 {
            "相手の BYE(同期ズレ) を受けた"
        } else {
            "自分が CHECK のズレを見つけた"
        };
        let at = self.desync.map_or(String::new(), |(l, _)| {
            format!("seg {} frame {}。", l.seg, l.frame)
        });
        Some(format!(
            "試合を止めた（{by}。{at}方式 {}。止めるまでに使ったフレーム {}。止めた間の描画だけのフレーム {}。{}）",
            if self.rollback {
                "rollback"
            } else {
                "delay 方式"
            },
            self.stop_played,
            self.stop_ticks,
            if self.stop & STOP_RELEASED != 0 {
                "Esc で戻った"
            } else {
                "まだ止めている"
            }
        ))
    }

    /// CHECK がずれたときの、ズレの時点の両者の CHECK の全部の欄（ずれていなければ None）。
    pub fn desync_line(&self) -> Option<String> {
        self.desync
            .map(|(l, r)| format!("CHECK のズレ: {}", desync_text(&l, &r)))
    }
}

/// `_net_frames.csv` の見出し。先頭の 5 列は peer_sim の記録の列の並びと同じ（seg・frame・1P・2P・使った後の delay）。後ろの 3 列は
/// dll だけの時間（µs。余裕が分からなければ空。`head_wait_us` はフレームの始まりで相手の入力ワードを待った時間）。読む道具は 7 列の古いログ
/// （`seg,frame,p1,p2,delay,margin_us,wait_us`）も読む。見出しの原本は core の `replay_compare::NET_FRAMES_HEADER`。
pub use higanr_core::replay_compare::NET_FRAMES_HEADER;

pub fn csv_row(f: &NetFrame) -> String {
    let (r, t) = (&f.rec, &f.timing);
    format!(
        "{},{},{:04X},{:04X},{},{},{},{}",
        r.seg,
        r.frame,
        r.p1,
        r.p2,
        t.delay,
        t.margin_us.map_or(String::new(), |m| m.to_string()),
        t.wait_us,
        t.head_wait_us
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameChecker {
    pub records: u64,
    /// 前の件の続き（同じ seg で frame + 1、か次の seg の frame 0）でなかった件。
    pub breaks: u64,
    last: Option<(u16, u32)>,
}

impl FrameChecker {
    pub fn check(&mut self, r: &FrameRec) {
        self.records += 1;
        let ok = match self.last {
            None => r.frame == 0,
            Some((s, f)) => {
                (r.seg == s && r.frame == f.wrapping_add(1))
                    || (r.seg == s.wrapping_add(1) && r.frame == 0)
            }
        };
        if !ok {
            self.breaks += 1;
        }
        self.last = Some((r.seg, r.frame));
    }

    pub fn write_summary(&self, out: &mut dyn Write, lost: u64) {
        let _ = writeln!(
            out,
            "ネットワークのフレーム: 読んだ {} 件 / 並びが続かなかった {} 件 / 読み落とし {} 件",
            self.records, self.breaks, lost
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seg: u16, frame: u32, p1: u16, p2: u16) -> FrameRec {
        FrameRec { seg, frame, p1, p2 }
    }

    fn tm(delay: u8, margin_us: Option<u32>, wait_us: u32) -> FrameTiming {
        tmh(delay, margin_us, wait_us, 0)
    }

    fn tmh(delay: u8, margin_us: Option<u32>, wait_us: u32, head_wait_us: u32) -> FrameTiming {
        FrameTiming {
            delay,
            margin_us,
            wait_us,
            head_wait_us,
        }
    }

    fn nf(r: FrameRec, t: FrameTiming) -> NetFrame {
        NetFrame { rec: r, timing: t }
    }

    #[test]
    fn ring_round_trip_and_overrun() {
        let ring = NetFrameRing::new();
        let (mut next, mut lost, mut out) = (0u32, 0u64, Vec::new());
        ring.drain(&mut next, &mut lost, &mut out);
        assert!(out.is_empty());
        let t1 = tmh(9, Some(0), 0xFFFF_FFFF, 0x1234);
        let t2 = tmh(0, None, 7, 0xFFFF_FFFF);
        ring.push(&rec(0xABCD, 0x1234_5678, 0xFFFF, 0x8001), &t1);
        ring.push(&rec(1, 2, 3, 4), &t2);
        ring.drain(&mut next, &mut lost, &mut out);
        assert_eq!(
            out,
            vec![
                nf(rec(0xABCD, 0x1234_5678, 0xFFFF, 0x8001), t1),
                nf(rec(1, 2, 3, 4), t2)
            ]
        );
        assert_eq!(ring.read(next), FrameRead::NotYet);
        out.clear();
        for i in 0..NET_RING_LEN + 5 {
            ring.push(&rec(0, i, 0, 0), &tm(2, Some(i), 0));
        }
        ring.drain(&mut next, &mut lost, &mut out);
        assert_eq!(lost, 5);
        assert_eq!(out.len(), NET_RING_LEN as usize);
        assert_eq!(out[0].rec.frame, 5);
        assert_eq!(out[0].timing.margin_us, Some(5));
        assert_eq!(ring.read(0), FrameRead::Overwritten);
    }

    #[test]
    fn checker_counts_breaks() {
        let mut c = FrameChecker::default();
        for r in [
            rec(0, 0, 0, 0),
            rec(0, 1, 0, 0),
            rec(1, 0, 0, 0),
            rec(1, 1, 0, 0),
            rec(1, 3, 0, 0),
            rec(1, 3, 0, 0),
            rec(3, 0, 0, 0),
        ] {
            c.check(&r);
        }
        assert_eq!((c.records, c.breaks), (7, 3));
        let mut c = FrameChecker::default();
        c.check(&rec(0, 1, 0, 0));
        assert_eq!(c.breaks, 1, "最初の件が frame 0 でない");
    }

    #[test]
    fn restarts_are_published_and_worded() {
        use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
        use crate::netplay::{Config, NetPlay};
        use higanr_core::handshake::{self, Name};
        use higanr_core::link;
        use higanr_core::wire::Seat;
        use std::net::SocketAddr;

        assert_eq!(restart_code(None), 0);
        let a = SocketAddr::from(([127, 0, 0, 1], 1));
        let b = SocketAddr::from(([127, 0, 0, 1], 2));
        use crate::netplay::Net;
        let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
        let lc = link::Config {
            handshake: handshake::Config {
                session_id: 3,
                role_nonce: 9,
                latency: 2,
                name: Name::new("x"),
                host_seat: Seat::P1,
            },
            secret: peer_sim::cli::DEFAULT_SECRET,
            peers: higanr_core::candidate::Candidates::from_addrs(&[b]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        };
        let mut np = NetPlay::new(lc, Config::default(), &mut na);
        let mut g = FakeGame::new(1, 10, 1, 1);
        let t0 = clock.get();
        let mut buf = [0u8; higanr_core::wire::MAX_DATAGRAM];
        while clock.get() < t0 + 25_000_000 {
            g.do_frame(&mut np, &mut na);
            clock.set(clock.get() + FRAME_US);
            while nb.recv(&mut buf).is_some() {}
        }
        let st = NetStatus::new();
        st.publish(&np);
        let s = st.snapshot();
        assert_eq!(s.hs_restarts, 2, "{}", s.text());
        // 作り直しの行の前半は今までの文のまま、後ろに診断（セッションを作った後だけ）が付く。
        assert_eq!(
            s.restart_line(2),
            format!(
                "ネットワーク: 接続の手順を作り直した（2 回目。10 秒のあいだ相手から 1 通も届かなかった。最初から作り直した）（受信ソケットに届いた 0 通。捨てた受信: 候補に無い送り元 0 通 / タグの誤り 0 通 / 別のセッション 0 通）〔{}〕",
                s.diag_text()
            )
        );
        assert!(
            s.diag_text().contains(&format!(
                "送った HELLO [同じ PC の中 {} / 家の中 0 / 外側 0 / IPv6 0]",
                na.sent
            )),
            "{}",
            s.diag_text()
        );
        assert!(
            !s.ended(),
            "作り直している間にネットワークを終えた: {}",
            s.text()
        );
        assert!(
            s.text().contains("接続の手順を作り直した 2 回"),
            "{}",
            s.text()
        );
        let s = NetSnapshot {
            hs_restart_last: restart_code(Some((FailReason::NoHandshake, Restart::Resume))),
            ..s
        };
        assert!(
            s.restart_line(4)
                .contains("相手から届いたが 10 秒で繋がらなかった。相手の値は引き継ぎ"),
            "{}",
            s.restart_line(4)
        );
        assert!(
            NetSnapshot {
                phase: phase_code(Phase::Ended),
                ..s
            }
            .ended()
        );
    }

    #[test]
    fn input_words_are_read_back_in_order() {
        let st = NetStatus::new();
        for i in 0..INPUT_WORDS {
            st.w[S_INPUT + i].store(100 + i as u32, Ordering::Relaxed);
        }
        let snap = st.snapshot();
        assert_eq!(
            snap.input_text(),
            "INPUT の受信ソケット: 受理 100 / 前の seg 101（控えを返した 102） / 別の seg で捨てた 103 / ほかの理由で捨てた 104（うち lat の取り決め違い BadLat 106 ・ホストが切替つきを受けた GuestLat 105）"
        );
        assert!(snap.text().contains(&snap.input_text()));
    }

    #[test]
    fn learned_candidates_are_published_and_worded() {
        use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
        use crate::netplay::{Config, Net, NetPlay};
        use higanr_core::auth::AuthKey;
        use higanr_core::handshake::{self, Name};
        use higanr_core::link;
        use higanr_core::wire::{self, Message, NAME_MAX, PROTO_VER, Seat};

        let a = SocketAddr::from(([127, 0, 0, 1], 1));
        let b = SocketAddr::from(([127, 0, 0, 1], 2));
        // 文書用の予約アドレス（RFC 5737）。
        let c1 = SocketAddr::from(([192, 0, 2, 7], 4567));
        let c2 = SocketAddr::from(([198, 51, 100, 9], 40_000));
        let secret = peer_sim::cli::DEFAULT_SECRET;
        let lc = |peers: &[SocketAddr]| link::Config {
            handshake: handshake::Config {
                session_id: 3,
                role_nonce: 9,
                latency: 2,
                name: Name::new("x"),
                host_seat: Seat::P1,
            },
            secret,
            peers: higanr_core::candidate::Candidates::from_addrs(peers).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        };
        let enc = |m: &Message| {
            let mut buf = [0u8; wire::MAX_DATAGRAM];
            let n = wire::encode(&AuthKey::derive(&secret, 3), 3, m, &mut buf);
            buf[..n].to_vec()
        };
        let hello = Message::Hello(wire::Hello {
            proto_ver: PROTO_VER,
            latency: 2,
            name_len: 0,
            mode: wire::Mode::Delay,
            can_rollback: false,
            listable: false,
            role_nonce: 77,
            name: [0; NAME_MAX],
            seat_wish: wire::SeatWish::P1,
            arrival: wire::Arrival::Unknown,
            build: higanr_core::update::BUILD_NUMBER,
            public: higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed(
                [0x77; 32],
            ))
            .public(),
        });
        let ack = Message::HelloAck {
            echo_nonce: 9,
            role_nonce: 77,
            public: higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed(
                [0x77; 32],
            ))
            .public(),
        };
        let snap_after = |inject: &[(SocketAddr, Vec<u8>)]| {
            let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
            let mut np = NetPlay::new(lc(&[b]), Config::default(), &mut na);
            let mut g = FakeGame::new(1, 10, 1, 1);
            for (from, bytes) in inject {
                na.inject_from(*from, bytes);
            }
            let t0 = clock.get();
            let mut rbuf = [0u8; wire::MAX_DATAGRAM];
            while clock.get() < t0 + 1_000_000 {
                g.do_frame(&mut np, &mut na);
                clock.set(clock.get() + FRAME_US);
                while nb.recv(&mut rbuf).is_some() {}
            }
            let st = NetStatus::new();
            st.publish(&np);
            (
                st.snapshot(),
                np.recv_counts(),
                np.link_hello_sent_learned(),
            )
        };

        let (s, rc, sent_learned) = snap_after(&[(c1, enc(&hello)), (c2, enc(&ack))]);
        assert_eq!((rc.learned, rc.learn_refused), (2, 0), "{rc:?}");
        assert_eq!(
            (s.recv_learned, s.recv_learn_refused),
            (2, 0),
            "{}",
            s.text()
        );
        assert!(sent_learned > 0, "学んだアドレスへ HELLO を送っていない");
        assert_eq!(s.hello_sent_learned, sent_learned);
        assert!(s.path_learned, "採った経路が学んだ候補の印が載っていない");
        let t = s.diag_text();
        assert!(
            t.contains(&format!(
                "学んだ候補 2 個（コードに無い送り元からタグの合う手順の 1 通が届いた。上限で断った 0・学んだアドレスへ送った HELLO {sent_learned}）"
            )),
            "{t}"
        );
        assert!(!t.contains("192.0.2") && !t.contains("198.51.100"), "{t}");

        let (s0, rc0, sent0) = snap_after(&[]);
        assert_eq!((rc0.learned, sent0), (0, 0));
        assert_eq!(
            (
                s0.recv_learned,
                s0.recv_learn_refused,
                s0.hello_sent_learned
            ),
            (0, 0, 0)
        );
        assert!(!s0.path_learned);
    }

    #[test]
    fn restart_line_shows_foreign_and_bad_tag_counts() {
        use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
        use crate::netplay::{Config, Net, NetPlay};
        use higanr_core::auth::AuthKey;
        use higanr_core::handshake::{self, Name};
        use higanr_core::link;
        use higanr_core::wire::{self, Message, Seat};

        let a = SocketAddr::from(([127, 0, 0, 1], 1));
        let b = SocketAddr::from(([127, 0, 0, 1], 2));
        // 文書用の予約アドレス（RFC 5737）。
        let c1 = SocketAddr::from(([192, 0, 2, 7], 4567));
        let c2 = SocketAddr::from(([198, 51, 100, 9], 40_000));
        let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
        let secret = peer_sim::cli::DEFAULT_SECRET;
        let lc = link::Config {
            handshake: handshake::Config {
                session_id: 3,
                role_nonce: 9,
                latency: 2,
                name: Name::new("x"),
                host_seat: Seat::P1,
            },
            secret,
            peers: higanr_core::candidate::Candidates::from_addrs(&[b]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        };
        let mut np = NetPlay::new(lc, Config::default(), &mut na);
        let mut g = FakeGame::new(1, 10, 1, 1);
        // 鍵もセッションも合った PING（送り元だけが候補に無い）。HELLO・HELLO_ACK・AGREE だと、繋がる前はタグまで通れば
        // 「学んだ候補」になって捨てられないので、手順の外の型にした。封じる鍵は検査の決め事の値でよい。
        let hello = Message::Ping { send_time: 5 };
        let mut buf = [0u8; wire::MAX_DATAGRAM];
        let dir = higanr_core::seal::DirKeys::from_raw([0x4B; 32], [0x4B; 32]);
        let n = wire::seal(
            &dir.sealer(higanr_core::seal::Sender::Link),
            &higanr_core::seal::Seq::new(),
            3,
            &hello,
            &mut buf,
        )
        .expect("封じられる");
        let good = buf[..n].to_vec();
        na.inject_from(c1, &good);
        na.inject_from(c1, &good);
        na.inject_from(c2, &good);
        // 候補の送り元からのタグの誤り 1 通。HMAC のタグを持つ HELLO_ACK を 1 ビット変える（鍵の交換の前に届いた封じた便は「鍵の前」に数える）。
        let ack = Message::HelloAck {
            echo_nonce: 9,
            role_nonce: 4,
            public: [0x42; 32],
        };
        let n = wire::encode(&AuthKey::derive(&secret, 3), 3, &ack, &mut buf);
        let mut bad = buf[..n].to_vec();
        *bad.last_mut().unwrap() ^= 1;
        na.inject_from(b, &bad);
        let t0 = clock.get();
        let mut rbuf = [0u8; wire::MAX_DATAGRAM];
        while clock.get() < t0 + 12_000_000 {
            g.do_frame(&mut np, &mut na);
            clock.set(clock.get() + FRAME_US);
            while nb.recv(&mut rbuf).is_some() {}
        }
        let st = NetStatus::new();
        st.publish(&np);
        let s = st.snapshot();
        assert_eq!(s.hs_restarts, 1, "{}", s.text());
        let line = s.restart_line(1);
        assert!(
            line.contains("10 秒のあいだ相手から 1 通も届かなかった"),
            "{line}"
        );
        assert!(
            line.contains(
                "受信ソケットに届いた 4 通。捨てた受信: 候補に無い送り元 3 通、最後は 198.51.100.9:40000 / タグの誤り 1 通"
            ),
            "{line}"
        );
        assert_eq!(s.last_foreign_text().as_deref(), Some("198.51.100.9:40000"));
        // 診断の HELLO の数は、送信側（`FakeNet::sent`）が数えた数と同じ。
        assert!(na.sent > 100, "HELLO をほとんど送っていない: {}", na.sent);
        assert_eq!(s.hello_sent, [na.sent, 0, 0, 0], "{}", s.diag_text());
        assert_eq!(s.peer_cands, [1, 0, 0, 0]);
        assert_eq!(
            s.link_started_ms, 1_000,
            "セッションを作った時刻（仮の時計の ms）"
        );
        assert!(
            line.contains(&format!(
                "〔診断: 自分の候補 [記録なし] / 相手の候補 [同じ PC の中 1 / 家の中 0 / 外側 0 / IPv6 0] / 送った HELLO [同じ PC の中 {} / 家の中 0 / 外側 0 / IPv6 0]（送りの誤り [同じ PC の中 0 / 家の中 0 / 外側 0 / IPv6 0]）",
                na.sent
            )),
            "{line}"
        );
        // セッションの無い間は診断を付けない（0 件と測っていないを混ぜない）。
        assert!(
            !NetSnapshot::default().restart_line(1).contains("診断"),
            "{}",
            NetSnapshot::default().restart_line(1)
        );
    }

    #[test]
    fn the_diag_text_reads_each_field_in_its_own_place_and_never_writes_an_address() {
        use crate::net_udp::{KeepDiag, NetDiag};
        let s = NetSnapshot {
            hello_sent: [1, 22, 333, 4444],
            peer_cands: [5, 6, 7, 8],
            recv_learned: 2,
            recv_learn_refused: 5,
            hello_sent_learned: 66,
            link_started_ms: 175_300,
            diag: NetDiag {
                own_cands: [0, 1, 2, 3],
                send_errors: [9, 8, 7, 6],
                port_kept: Some(false),
                stun_at_ms: Some(1_000),
                keep: KeepDiag {
                    installed: true,
                    running: false,
                    counts: higanr_core::stun::KeepCounts {
                        sent: 11,
                        answered: 10,
                        ignored: 2,
                        changed_port: 3,
                        changed_addr: 1,
                        differs_from_code: true,
                    },
                    send_errors: 4,
                },
                second: crate::net_udp::SecondStun::Compared(higanr_core::stun::Mapping::PortOnly),
                upnp: crate::upnp::Stats::default(),
            },
            ..NetSnapshot::default()
        };
        assert_eq!(
            s.diag_text(),
            "診断: 自分の候補 [同じ PC の中 0 / 家の中 1 / 外側 2 / IPv6 3] / 相手の候補 [同じ PC の中 5 / 家の中 6 / 外側 7 / IPv6 8] / 送った HELLO [同じ PC の中 1 / 家の中 22 / 外側 333 / IPv6 4444]（送りの誤り [同じ PC の中 9 / 家の中 8 / 外側 7 / IPv6 6]） / 学んだ候補 2 個（コードに無い送り元からタグの合う手順の 1 通が届いた。上限で断った 5・学んだアドレスへ送った HELLO 66） / 外側のポートは受信ソケットのポートと違う（ルーターが付け替えている） / コードの外側のアドレスを得てから接続の手順を始めるまで: 174.3 秒 / STUN の聞き直し: 止めた / 送った 11 回（送れなかった 4） / 答え 10 回（答えにならなかったもの 2） / 外側のアドレスが変わった 4 回（ポートだけ 3 / アドレスも 1）/ いまの外側のアドレスは、コードに載せたものと違う / 2 か所目の STUN: 外側のポートだけ 1 か所目と違う（宛先ごとに外側のポートを変える型。STUN で見えたアドレスは相手には使えない見込み） / UPnP: 使っていない（ini の upnp = off・scope が internet でない・受信ソケットが家の中のアドレスでないのどれか）"
        );
        let none = NetSnapshot {
            peer_cands: [1, 0, 0, 0],
            ..NetSnapshot::default()
        };
        let t = none.diag_text();
        assert!(
            t.contains("外側のポートは不明（STUN の答えが無い）")
                && t.contains("外側のアドレスを得ていない")
                && t.contains("STUN の聞き直し: していない")
                && t.contains("2 か所目の STUN: 聞いていない"),
            "{t}"
        );
        let same = NetSnapshot {
            diag: NetDiag {
                port_kept: Some(true),
                keep: KeepDiag {
                    installed: true,
                    running: true,
                    ..KeepDiag::default()
                },
                ..NetDiag::default()
            },
            ..none
        };
        let t = same.diag_text();
        assert!(
            t.contains("外側のポートは受信ソケットのポートと同じ")
                && t.contains("続けている")
                && t.contains("答えがまだ無い"),
            "{t}"
        );
        assert!(
            !s.diag_text()
                .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .any(|w| w.matches('.').count() >= 3),
            "{}",
            s.diag_text()
        );
    }

    #[test]
    fn a_hello_of_another_version_ends_the_net_with_both_versions() {
        use crate::netplay::fake::{FRAME_US, FakeGame, FakeNet};
        use crate::netplay::{Config, Net, NetPlay};
        use higanr_core::auth::AuthKey;
        use higanr_core::handshake::{self, Name};
        use higanr_core::link;
        use higanr_core::net_menu::Conn;
        use higanr_core::wire::{self, Message, NAME_MAX, Seat};

        for ver in [0u8, PROTO_VER] {
            let a = SocketAddr::from(([127, 0, 0, 1], 1));
            let b = SocketAddr::from(([127, 0, 0, 1], 2));
            let (mut na, mut nb, clock) = FakeNet::pair(a, b, 1_000);
            let secret = peer_sim::cli::DEFAULT_SECRET;
            let lc = link::Config {
                handshake: handshake::Config {
                    session_id: 3,
                    role_nonce: 9,
                    latency: 2,
                    name: Name::new("x"),
                    host_seat: Seat::P1,
                },
                secret,
                peers: higanr_core::candidate::Candidates::from_addrs(&[b]).unwrap(),
                key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
            };
            let mut np = NetPlay::new(lc, Config::default(), &mut na);
            let mut g = FakeGame::new(1, 10, 1, 1);
            let hello = Message::Hello(wire::Hello {
                proto_ver: PROTO_VER,
                latency: 2,
                name_len: 0,
                mode: wire::Mode::Delay,
                can_rollback: false,
                listable: false,
                role_nonce: 77,
                name: [0; NAME_MAX],
                seat_wish: wire::SeatWish::P1,
                arrival: wire::Arrival::Unknown,
                build: higanr_core::update::BUILD_NUMBER,
                public: higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed(
                    [0x77; 32],
                ))
                .public(),
            });
            let key = AuthKey::derive(&secret, 3);
            let mut buf = [0u8; wire::MAX_DATAGRAM];
            let n = wire::encode(&key, 3, &hello, &mut buf);
            let mut pkt = buf[..n - wire::TAG_LEN].to_vec();
            pkt[4] = ver;
            pkt[wire::HEADER_LEN] = ver;
            let tag = key.tag(&pkt);
            pkt.extend_from_slice(&tag);
            na.inject_from(b, &pkt);
            let t0 = clock.get();
            let mut rbuf = [0u8; wire::MAX_DATAGRAM];
            while clock.get() < t0 + 1_000_000 {
                g.do_frame(&mut np, &mut na);
                clock.set(clock.get() + FRAME_US);
                while nb.recv(&mut rbuf).is_some() {}
            }
            let st = NetStatus::new();
            st.publish(&np);
            let s = st.snapshot();
            if ver == PROTO_VER {
                assert_eq!(
                    np.end(),
                    None,
                    "今のバージョンの HELLO で終わった: {}",
                    s.text()
                );
                continue;
            }
            assert_eq!(
                np.end(),
                Some(End::HandshakeFailed(FailReason::OtherVersion { theirs: 0 })),
                "{}",
                s.text()
            );
            assert_eq!((s.end, s.end_detail), (END_OTHER_VERSION, 0));
            let want = format!(
                "終わり 相手の HiganRTool のバージョンが違います（自分 {PROTO_VER}・相手 0）。相手と同じ zip の HiganRTool を使ってください。"
            );
            assert!(s.text().contains(&want), "{}", s.text());
            assert_eq!(s.hs_restarts, 0, "バージョン違いを作り直した");
            assert_eq!(np.ui_conn(0).0, Conn::OtherVersion { theirs: 0 });
        }
    }

    #[test]
    fn foreign_source_packing() {
        let v4 = SocketAddr::from(([203, 0, 113, 5], 65_535));
        let [ip, port] = pack_from(Some(v4));
        assert_eq!(from_text(ip, port).as_deref(), Some("203.0.113.5:65535"));
        let v6 = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 9));
        let [ip, port] = pack_from(Some(v6));
        assert_eq!(from_text(ip, port).as_deref(), Some("IPv6"));
        let [ip, port] = pack_from(None);
        assert_eq!(from_text(ip, port), None);
        // ポート 0 でも「ある」印が残る。
        let z = SocketAddr::from(([0, 0, 0, 0], 0));
        let [ip, port] = pack_from(Some(z));
        assert_eq!(from_text(ip, port).as_deref(), Some("0.0.0.0:0"));
        assert!(
            !NetSnapshot::default().restart_line(1).contains("最後は"),
            "{}",
            NetSnapshot::default().restart_line(1)
        );
    }

    #[test]
    fn check_words_round_trip_and_the_unconnected_status_is_empty() {
        let hi = Check {
            seg: u16::MAX,
            frame: u32::MAX,
            rng_state: u16::MAX,
            rng_count: u32::MAX,
            life_1p: u8::MAX,
            life_2p: u8::MAX,
        };
        let zero = Check {
            seg: 0,
            frame: 0,
            rng_state: 0,
            rng_count: 0,
            life_1p: 0,
            life_2p: 0,
        };
        let mix = Check {
            seg: 0x1234,
            frame: 7,
            rng_state: 0xABCD,
            rng_count: 99,
            life_1p: 3,
            life_2p: 200,
        };
        for c in [hi, zero, mix] {
            assert_eq!(unpack_check(pack_check(&c)), Some(c), "{c:?}");
        }
        assert_eq!(unpack_check([0; 4]), None, "印の無い語を CHECK と読んだ");
        assert_ne!(pack_check(&mix), pack_check(&zero));
        let s = NetStatus::new().snapshot();
        assert!(!s.connected && s.path_kind == 0 && s.rtt_ms.is_none() && s.desync.is_none());
        assert_eq!(s.agreed_mismatch, None);
        assert!(s.desync_line().is_none());
        assert!(!s.text().contains("CHECK のズレ:"), "{}", s.text());
        let s = NetSnapshot {
            desync: Some((mix, hi)),
            ..NetSnapshot::default()
        };
        assert!(
            s.text().contains("CHECK のズレ: 自分の CHECK: seg 4660 frame 7 乱数の状態 0xABCD 抽選回数 99 1P ライフ 3 2P ライフ 200 / 相手の CHECK: seg 65535 frame 4294967295"),
            "{}",
            s.text()
        );
    }

    #[test]
    fn end_codes_and_csv() {
        use crate::netplay::BadValue;
        use higanr_core::wire::ByeReason;
        assert_eq!(end_code(None), (0, 0));
        assert_eq!(end_code(Some(End::Dead)), (3, 0));
        assert_eq!(end_code(Some(End::PeerBye(ByeReason::Cancelled))), (2, 4));
        assert_eq!(
            end_code(Some(End::BadSelect(BadValue {
                field: "キャラ",
                value: 16
            }))),
            (6, 16)
        );
        let s = NetSnapshot {
            end: 6,
            end_detail: 16,
            ..Default::default()
        };
        assert!(
            s.text().contains("相手の SELECT が範囲の外（16）"),
            "{}",
            s.text()
        );
        assert_eq!(end_code(Some(End::Unmatched(NoMatch::TimedOut))), (8, 0));
        assert_eq!(
            end_code(Some(End::Unmatched(NoMatch::PeerHasNoGate))),
            (9, 0)
        );
        assert_eq!(end_code(Some(End::Unmatched(NoMatch::PeerLeft))), (10, 4));
        // 接続の手順をやめた理由の番号は、バージョン違いを足す前と同じ数のまま。バージョン違いは別の種類 11 で、中身は相手のバージョン。
        assert_eq!(
            end_code(Some(End::HandshakeFailed(FailReason::SameNonce))),
            (1, 0)
        );
        assert_eq!(
            end_code(Some(End::HandshakeFailed(FailReason::NothingReceived))),
            (1, 1)
        );
        assert_eq!(
            end_code(Some(End::HandshakeFailed(FailReason::NoHandshake))),
            (1, 2)
        );
        assert_eq!(
            end_code(Some(End::HandshakeFailed(FailReason::OtherVersion {
                theirs: 7
            }))),
            (END_OTHER_VERSION, 7)
        );
        assert_eq!(watch_end_code(Some(WatchEnd::GaveUp)), (1, 1));
        for (end, want) in [
            (8, "対戦が成立しないまま時間切れで閉じた"),
            (9, "相手に成立のゲートが無いので閉じた"),
            (10, "対戦が成立しないまま相手が閉じた"),
        ] {
            let s = NetSnapshot {
                end,
                ..Default::default()
            };
            assert!(s.text().contains(want), "{end}: {}", s.text());
        }
    }

    #[test]
    fn the_gate_line_shows_who_chose_and_ready_counts() {
        assert_eq!(NetSnapshot::default().gate_line(), None);
        assert!(!NetSnapshot::default().text().contains("対戦の成立"));
        let s = NetSnapshot {
            gate: GATE_ON | GATE_LOCAL,
            ready_sent: 7,
            ready_received: 0,
            ready_dup: 0,
            ..Default::default()
        };
        assert_eq!(
            s.gate_line().as_deref(),
            Some(
                "対戦の成立: 自分 選んだ / 相手 まだ / READY 送った 7 通・受けた 0 通（うち重複 0 通）"
            )
        );
        assert!(s.text().contains("対戦の成立: 自分 選んだ"));
        let s = NetSnapshot {
            gate: GATE_ON | GATE_PEER | GATE_EXPIRED,
            ready_received: 3,
            ready_dup: 2,
            ..Default::default()
        };
        assert_eq!(
            s.gate_line().as_deref(),
            Some(
                "対戦の成立: 自分 まだ / 相手 選んだ / 成立しないまま時間切れ / READY 送った 0 通・受けた 3 通（うち重複 2 通）"
            )
        );
        let s = NetSnapshot {
            gate: GATE_ON | GATE_LOCAL | GATE_NO_GATE,
            ready_sent: 4,
            ..Default::default()
        };
        assert_eq!(
            s.gate_line().as_deref(),
            Some(
                "対戦の成立: 自分 選んだ / 相手 まだ / 相手にゲートが無いので閉じた / READY 送った 4 通・受けた 0 通（うち重複 0 通）"
            )
        );
        let st = NetStatus::new();
        assert_eq!(st.snapshot().gate, 0);
        assert_eq!(
            csv_row(&nf(rec(1, 42, 0x13, 0x2), tmh(3, Some(15_000), 12, 8_100))),
            "1,42,0013,0002,3,15000,12,8100"
        );
        assert_eq!(
            csv_row(&nf(rec(0, 0, 0, 0), tm(2, None, 0))),
            "0,0,0000,0000,2,,0,0"
        );
        assert_eq!(
            NET_FRAMES_HEADER.split(',').count(),
            csv_row(&nf(rec(0, 0, 0, 0), tm(2, None, 0)))
                .split(',')
                .count()
        );
    }

    #[test]
    fn the_agreed_shape_rides_in_the_conn_word() {
        let st = NetStatus::new();
        assert!(!st.snapshot().rollback);
        for (conn, rb, learned, kind) in [
            (true, true, false, 2),
            (true, false, false, 2),
            (true, false, true, 3),
            (true, true, true, 3),
            (false, false, false, 0),
        ] {
            st.w[S_CONN].store(pack_conn(conn, rb, learned, kind), Ordering::Relaxed);
            let s = st.snapshot();
            assert_eq!(
                (s.connected, s.rollback, s.path_learned, s.path_kind),
                (conn, rb, learned, kind)
            );
        }
    }

    #[test]
    fn local_scene2_is_published_and_worded() {
        let s = NetSnapshot {
            local_scene2: 3,
            ..NetSnapshot::default()
        };
        assert!(
            s.text()
                .contains("ネットワークの選択なしで場面 2 へ移った 3"),
            "{}",
            s.text()
        );
    }

    #[test]
    fn a_relinkable_run_is_not_over_when_one_link_ends_and_reset_clears_the_desync() {
        let ended = NetSnapshot {
            phase: phase_code(Phase::Ended),
            ..NetSnapshot::default()
        };
        assert!(ended.ended() && ended.link_ended());
        let relink = NetSnapshot {
            relink: true,
            ..ended
        };
        assert!(
            !relink.ended(),
            "つなぎ直せる実行でネットワークの実行が終わったことにした"
        );
        assert!(relink.link_ended());
        assert!(!NetSnapshot::default().link_ended());
        let st = NetStatus::new();
        let c = Check {
            seg: 1,
            frame: 2,
            rng_state: 3,
            rng_count: 4,
            life_1p: 5,
            life_2p: 6,
        };
        st.fake_desync(&c, &c);
        assert_eq!(st.snapshot().desync, Some((c, c)), "検査の前提");
        st.reset();
        assert_eq!(st.snapshot(), NetSnapshot::default(), "消し残した欄がある");
    }

    #[test]
    fn the_dsw_line_shows_head_sets_auto_and_digit_presses() {
        let s = crate::netplay::Stats {
            delay_presses: 4,
            delay_presses_unchanged: 1,
            digit_presses: 7,
            digit_unchanged: 2,
            ..Default::default()
        };
        let r = higanr_core::rollback::Stats {
            d_held: 6,
            d_head_sets: 3,
            d_auto: 5,
            d_auto_held: 2,
            ..Default::default()
        };
        let w = dsw_words(&s, Some(&r), None);
        assert_eq!((w[DSW_HEAD_SETS], w[DSW_AUTO]), (3, 5), "先頭の D・auto");
        assert_eq!(w[DSW_AUTO_HELD], 2, "変わりにくくして据え置いた数");
        assert_eq!(DSW_AUTO_HELD, DSW_DIGIT_UNCHANGED + 1, "数の欄の並び");
        assert_eq!((w[DSW_DIGITS], w[DSW_DIGIT_UNCHANGED]), (7, 2), "数字キー");
        assert_eq!((w[0], w[1], w[11]), (4, 1, 6), "← → とゲートの前の欄");
        let st = NetStatus::new();
        st.store_dsw(w);
        let text = st.dsw_text();
        assert!(
            text.contains("初めの値をネットワークへ渡した 4（今の値と同じ 1）"),
            "{text}"
        );
        assert!(
            text.contains("数字キーで渡した押下 7（今の値・旗と同じ 2）"),
            "{text}"
        );
        assert!(
            text.contains(
                "seg の先頭の D を直に替えた 3・auto を使った 5（今の D と同じだった分も含む）・そのうち変わりにくくして据え置いた 2"
            ),
            "{text}"
        );
        let mut r1 = r;
        r1.d_switches = 1;
        r1.d_switch_log[0] = higanr_core::rollback::DSwitch {
            seg: 2,
            frame: 9,
            wire_frame: 7,
            from: 1,
            to: 4,
            by_peer: true,
        };
        let w1 = dsw_words(&s, Some(&r1), None);
        assert_eq!(w1[DSW_AUTO_HELD], 2, "記録が据え置いた数の欄を壊した");
        assert_eq!(w1[DSW_COUNTS], pack_dsw(&r1.d_switch_log[0])[0], "記録の語");
        assert_eq!(
            (w1[DSW_DIGITS], w1[DSW_DIGIT_UNCHANGED]),
            (7, 2),
            "記録が数字の欄を壊した"
        );
        let st1 = NetStatus::new();
        st1.store_dsw(w1);
        assert!(
            st1.dsw_text().contains(
                "seg 2 の取得のフレーム 9 から D 1 → 4（相手の押下。埋めた 3）ネットワークの F 7"
            ),
            "{}",
            st1.dsw_text()
        );
        let mut r2 = r1;
        r2.d_switches = 2;
        r2.d_switch_log[1] = higanr_core::rollback::DSwitch {
            seg: 3,
            frame: 40,
            wire_frame: 31,
            from: 4,
            to: 2,
            by_peer: false,
        };
        let st2 = NetStatus::new();
        st2.store_dsw(dsw_words(&s, Some(&r2), None));
        assert!(
            st2.dsw_text().contains(
                "seg 3 の取得のフレーム 40 から D 4 → 2（自分の押下。捨てる取得 2）ネットワークの F 31"
            ),
            "{}",
            st2.dsw_text()
        );
        let l = higanr_core::lockstep::Stats {
            auto_held: 3,
            ..Default::default()
        };
        let w2 = dsw_words(&s, None, Some(&l));
        assert_eq!(w2[DSW_AUTO_HELD], 3, "delay 方式の据え置いた数");
        assert_eq!(
            (w2[DSW_HEAD_SETS], w2[DSW_AUTO]),
            (0, 0),
            "delay 方式に先頭の D は無い"
        );
        assert_eq!(
            (w2[DSW_DIGITS], w2[DSW_DIGIT_UNCHANGED]),
            (7, 2),
            "delay 方式の数字キー"
        );
    }
}
