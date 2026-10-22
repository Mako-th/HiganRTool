//! 観戦者の受け渡し（受信スレッド → ゲームのスレッド）: 組み立てが連続にした記録のリングと、接続の状態・集計の原子変数。
//!
//! - リングの書き手は観戦者の受け取り（`recv.rs`。2 本の受信スレッドのうちロックを取れた方）だけ、読み手はゲームのスレッドだけ。枠を `Relaxed` で
//!   書いてから書いた数を `Release` で進め、読み手は書いた数を `Acquire` で読んでから枠を読む（`spectate_ring` と同じ）。
//! - 1 枠 10 語: 記録 9 語（`spectate_ring::pack` の詰め方）と、使えるようになった時刻（µs の下位 32 bit。余裕の起点）。語 0 の bit 9 は
//!   「流れが飛んだ（`reset`）後の最初の記録」。
//! - 読み手の側は確保・ロック待ち・パニックしうる添字を持たない。確保は作るとき（観戦者のセッションを開くスレッド）の 1 回だけ。
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use higanr_core::rtt_probe::elapsed;
use higanr_core::spectate::{AsmCounts, TreeCounts, WatcherCounts};
use higanr_core::wire::{ByeReason, Record};

use crate::net_ring::{ADDR_WORDS, decode_addr, encode_addr};
use crate::spectate_ring::{WORDS as REC_WORDS, pack, unpack};

/// 枠の数（約 1 時間）。記録は 1 秒に約 60（マッチの 1 フレームか選択の 1 tick に 1 つ）と、マッチの間の約 1 秒ごとの対戦の状態と照合で、
/// 220,000 ÷ 62 ≈ 3,550 秒。1 枠 40 B で約 8.8 MB。
pub const FEED_LEN: usize = 220_000;
const CELL_WORDS: usize = REC_WORDS + 1;
const T_WORD: usize = REC_WORDS;
const FLAG_JUMP: u32 = 1 << 9;
/// 語 0 の bit 10: 時間を使わない記録（`Record::takes_time` が偽）。読み手が返すときに数える。
const FLAG_TIMELESS: u32 = 1 << 10;
const WC_LEN: usize = 14;
const AC_LEN: usize = 4;
const TREE_LEN: usize = 12;
const SIDE_LEN: usize = 12;
/// 受信スレッドだけが数えるもの: 照合のずれで戻らなかった数・UPnP の結果・知らせの JOIN の送った数と送らなかった数。
const OWN_LEN: usize = 4;
/// 中継の数の語: 設定・作れなかった回数・epoch・親が元のホスト・直接だけ・断られた・配る側あり ＋ 木の数 ＋ 配る側の数 ＋ 受信スレッドの数。
pub(crate) const RELAY_LEN: usize = 7 + TREE_LEN + SIDE_LEN + OWN_LEN;
/// u64 の語に u32 の値を入れるときの「値あり」の印（下位 32 bit が値）。
const SOME: u64 = 1 << 32;
/// 照合のずれの要求の語（`WatchFeed::mismatch`）: `SOME` ＋ この印 ＋ 下位 32 bit の位置。
const MISMATCH_BROKEN: u64 = 1 << 33;

/// 中継の配る側の数（`SpectateHost::counts` のうち中継に関わるもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelaySideCounts {
    /// いまの子の数（受け入れて JOIN を待っている子を含まない）。
    pub children: u64,
    pub pushed: u64,
    pub relay_jumps: u64,
    pub joined: u64,
    pub left: u64,
    pub silenced: u64,
    pub not_admitted: u64,
    pub packets_sent: u64,
    pub records_sent: u64,
    pub resends: u64,
    pub rehomes_sent: u64,
    /// 受け入れる子の候補へ送った穴あけの印。
    pub punches_sent: u64,
}

/// 中継の配る側を持つ観戦者の UPnP の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchUpnp {
    /// 頼んでいない（中継しない・ルームの経路でない・scope が internet でない・ini の upnp = off・家の中の IPv4 でない）。
    #[default]
    NotAsked,
    /// 頼んで、結果を待っている。
    Waiting,
    /// 割り当てて、自分の候補に足した。
    Mapped,
    /// ルーターの外側のアドレスが STUN で見えたアドレスと違う（二重 NAT の見込み）。割り当ては取り下げた。
    Behind,
    /// 外側のアドレスを突き合わせられない。割り当ては取り下げた。
    Unchecked,
    /// 割り当てたが、自分の候補の並びに足せない。割り当ては取り下げた。
    NoRoom,
    /// 割り当てられなかった（応じない・断られた・通信の誤り）。
    Failed,
    /// 割り当てたが、接続した受け取りに中継の配る側が無い（作れなかった）。割り当ては取り下げた。
    NoRelay,
}

impl WatchUpnp {
    fn code(self) -> u64 {
        match self {
            WatchUpnp::NotAsked => 0,
            WatchUpnp::Waiting => 1,
            WatchUpnp::Mapped => 2,
            WatchUpnp::Behind => 3,
            WatchUpnp::Unchecked => 4,
            WatchUpnp::NoRoom => 5,
            WatchUpnp::Failed => 6,
            WatchUpnp::NoRelay => 7,
        }
    }

    fn of(code: u64) -> WatchUpnp {
        match code {
            1 => WatchUpnp::Waiting,
            2 => WatchUpnp::Mapped,
            3 => WatchUpnp::Behind,
            4 => WatchUpnp::Unchecked,
            5 => WatchUpnp::NoRoom,
            6 => WatchUpnp::Failed,
            7 => WatchUpnp::NoRelay,
            _ => WatchUpnp::NotAsked,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            WatchUpnp::NotAsked => "未実行",
            WatchUpnp::Waiting => "結果待ち",
            WatchUpnp::Mapped => "割り当て済み（候補に追加）",
            WatchUpnp::Behind => "外側のアドレスが STUN と不一致（取り下げ）",
            WatchUpnp::Unchecked => "外側のアドレスの突き合わせ不能（取り下げ）",
            WatchUpnp::NoRoom => "候補の空きなし（取り下げ）",
            WatchUpnp::Failed => "割り当て不能",
            WatchUpnp::NoRelay => "中継の配信側なし（取り下げ）",
        }
    }
}

/// 観戦者の中継の木の数（本ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelayCounts {
    /// 中継する設定（ini の `[spectate] relay = 1`）。
    pub enabled: bool,
    /// 中継する設定なのに、鍵の種を引けない・リングを確保できないので中継しなかった回数。
    pub failed: u64,
    pub epoch: u64,
    pub parent_is_home: bool,
    pub direct_only: bool,
    /// 一度接続した後に元のホストに断られた（満員）。
    pub refused: bool,
    pub tree: TreeCounts,
    /// 子の観戦者へ記録を中継する側（持っていなければ None）。
    pub side: Option<RelaySideCounts>,
    /// 照合がずれたとき親が中継役だったが、直接の元のホストへ戻らなかった数（ずれた記録を前の親が運んだ・その seg でポーズが崩れた）。
    pub mismatch_stays: u64,
    pub upnp: WatchUpnp,
    /// 接続した後に、候補・回線の型の変化を元のホストへ知らせた JOIN の数。
    pub announces_sent: u64,
    /// 知らせを送らなかった回数（中継役になった後・親が中継役・元のホストのアドレスが無い）。
    pub announces_skipped: u64,
}

pub(crate) fn relay_words(c: &RelayCounts) -> [u64; RELAY_LEN] {
    let TreeCounts {
        intros_taken,
        intros_ignored,
        admits_taken,
        admits_ignored,
        control_dropped,
        rehomes,
        unreached,
        parent_silent,
        measures_taken,
        measures_ignored,
        pairs_sent,
        mismatch_returns,
    } = c.tree;
    let RelaySideCounts {
        children,
        pushed,
        relay_jumps,
        joined,
        left,
        silenced,
        not_admitted,
        packets_sent,
        records_sent,
        resends,
        rehomes_sent,
        punches_sent,
    } = c.side.unwrap_or_default();
    [
        u64::from(c.enabled),
        c.failed,
        c.epoch,
        u64::from(c.parent_is_home),
        u64::from(c.direct_only),
        u64::from(c.refused),
        u64::from(c.side.is_some()),
        intros_taken,
        intros_ignored,
        admits_taken,
        admits_ignored,
        control_dropped,
        rehomes,
        unreached,
        parent_silent,
        measures_taken,
        measures_ignored,
        pairs_sent,
        mismatch_returns,
        children,
        pushed,
        relay_jumps,
        joined,
        left,
        silenced,
        not_admitted,
        packets_sent,
        records_sent,
        resends,
        rehomes_sent,
        punches_sent,
        c.mismatch_stays,
        c.upnp.code(),
        c.announces_sent,
        c.announces_skipped,
    ]
}

/// `relay_words` の逆。
pub(crate) fn relay_counts_of(w: [u64; RELAY_LEN]) -> RelayCounts {
    let [
        enabled,
        failed,
        epoch,
        parent_is_home,
        direct_only,
        refused,
        has_side,
        intros_taken,
        intros_ignored,
        admits_taken,
        admits_ignored,
        control_dropped,
        rehomes,
        unreached,
        parent_silent,
        measures_taken,
        measures_ignored,
        pairs_sent,
        mismatch_returns,
        children,
        pushed,
        relay_jumps,
        joined,
        left,
        silenced,
        not_admitted,
        packets_sent,
        records_sent,
        resends,
        rehomes_sent,
        punches_sent,
        mismatch_stays,
        upnp,
        announces_sent,
        announces_skipped,
    ] = w;
    RelayCounts {
        enabled: enabled != 0,
        failed,
        epoch,
        parent_is_home: parent_is_home != 0,
        direct_only: direct_only != 0,
        refused: refused != 0,
        tree: TreeCounts {
            intros_taken,
            intros_ignored,
            admits_taken,
            admits_ignored,
            control_dropped,
            rehomes,
            unreached,
            parent_silent,
            measures_taken,
            measures_ignored,
            pairs_sent,
            mismatch_returns,
        },
        side: (has_side != 0).then_some(RelaySideCounts {
            children,
            pushed,
            relay_jumps,
            joined,
            left,
            silenced,
            not_admitted,
            packets_sent,
            records_sent,
            resends,
            rehomes_sent,
            punches_sent,
        }),
        mismatch_stays,
        upnp: WatchUpnp::of(upnp),
        announces_sent,
        announces_skipped,
    }
}

/// リングから読んだ 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub rec: Record,
    /// 使えるようになった時刻（µs の下位 32 bit）。
    pub t_us: u32,
    /// 流れが飛んだ後の最初の記録。
    pub jump: bool,
}

pub struct WatchFeed {
    cells: Box<[[AtomicU32; CELL_WORDS]]>,
    written: AtomicU32,
    read: AtomicU32,
    /// `written` / `read` のうち、時間を使う記録の数（書き手・読み手がそれぞれ数える）。溜まりの数と 1 表示に届いた数はこれで数え、
    /// 名前・対戦の状態・照合を入れない。
    timed_written: AtomicU32,
    timed_read: AtomicU32,
    /// 読み手が戻せなかった枠（起きないはず）。
    bad: AtomicU32,
    // ---- 受信スレッドが書く ----
    full: AtomicBool,
    broken: AtomicBool,
    has_watcher: AtomicBool,
    /// ホストの最初の STREAM を受けた（`host` を書いた後に立てる）。
    joined: AtomicBool,
    host: [AtomicU32; ADDR_WORDS],
    /// 時刻（ms）。上位 32 bit が 1 なら値あり。
    last_heard: AtomicU64,
    code_at: AtomicU64,
    joined_at: AtomicU64,
    wc: [AtomicU64; WC_LEN],
    ac: [AtomicU64; AC_LEN],
    relay: [AtomicU64; RELAY_LEN],
    /// 一度接続した後に元のホストに断られた（満員。ゲームのスレッドが溜めた記録を使い切ってから終える）。
    refused: AtomicBool,
    // ---- ゲームのスレッドが書く ----
    /// 照合がずれた（受信スレッドが取る）: ずれた Check の記録の位置（読んだ数）と、その seg でポーズが崩れていたか。
    mismatch: AtomicU64,
    /// 観戦を終えた（受信スレッドは BYE を出して以後は何もしない）。`bye` を書いた後に立てる。
    ended: AtomicBool,
    bye: AtomicU32,
}

fn opt_ms(v: u64) -> Option<u32> {
    (v & SOME != 0).then_some(v as u32)
}

fn wc_words(c: &WatcherCounts) -> [u64; WC_LEN] {
    let WatcherCounts {
        joins_sent,
        acks_sent,
        streams,
        foreign,
        not_hgrt,
        not_hgrt_v6,
        wrong_session,
        bad_tag,
        undecodable,
        other_version,
        unexpected,
        byes,
        no_keys,
        learned_host,
    } = *c;
    [
        joins_sent,
        acks_sent,
        streams,
        foreign,
        not_hgrt,
        not_hgrt_v6,
        wrong_session,
        bad_tag,
        undecodable,
        other_version,
        unexpected,
        byes,
        no_keys,
        learned_host,
    ]
}

fn wc_of(w: [u64; WC_LEN]) -> WatcherCounts {
    let [
        joins_sent,
        acks_sent,
        streams,
        foreign,
        not_hgrt,
        not_hgrt_v6,
        wrong_session,
        bad_tag,
        undecodable,
        other_version,
        unexpected,
        byes,
        no_keys,
        learned_host,
    ] = w;
    WatcherCounts {
        joins_sent,
        acks_sent,
        streams,
        foreign,
        not_hgrt,
        not_hgrt_v6,
        wrong_session,
        bad_tag,
        undecodable,
        other_version,
        unexpected,
        byes,
        no_keys,
        learned_host,
    }
}

fn bye_code(r: Option<ByeReason>) -> u32 {
    r.map_or(0, |r| r as u32)
}

impl WatchFeed {
    /// 枠の数を指定して作る（0 は 1 に上げる）。確保するのはここだけで、確保できなければ `None`。
    pub fn try_with_len(len: usize) -> Option<Self> {
        let n = len.max(1);
        let mut cells = Vec::new();
        cells.try_reserve_exact(n).ok()?;
        cells.extend((0..n).map(|_| [const { AtomicU32::new(0) }; CELL_WORDS]));
        Some(Self {
            cells: cells.into_boxed_slice(),
            written: AtomicU32::new(0),
            read: AtomicU32::new(0),
            timed_written: AtomicU32::new(0),
            timed_read: AtomicU32::new(0),
            bad: AtomicU32::new(0),
            full: AtomicBool::new(false),
            broken: AtomicBool::new(false),
            has_watcher: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            host: [const { AtomicU32::new(0) }; ADDR_WORDS],
            last_heard: AtomicU64::new(0),
            code_at: AtomicU64::new(0),
            joined_at: AtomicU64::new(0),
            wc: [const { AtomicU64::new(0) }; WC_LEN],
            ac: [const { AtomicU64::new(0) }; AC_LEN],
            relay: [const { AtomicU64::new(0) }; RELAY_LEN],
            refused: AtomicBool::new(false),
            mismatch: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            bye: AtomicU32::new(0),
        })
    }

    #[cfg(test)]
    pub fn with_len(len: usize) -> Self {
        Self::try_with_len(len).expect("検査の大きさなら確保できる")
    }

    // ---------------- 書き手（受信スレッド） ----------------

    pub(super) fn has_room(&self) -> bool {
        let w = self.written.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        (w.wrapping_sub(r) as usize) < self.cells.len()
    }

    /// 1 つ書く。満杯なら書かずに false。
    pub(super) fn push(&self, rec: &Record, jump: bool, t_us: u32) -> bool {
        if !self.has_room() {
            return false;
        }
        let w = self.written.load(Ordering::Relaxed);
        let Some(cell) = self.cells.get(w as usize % self.cells.len()) else {
            return false;
        };
        let (mut words, used) = pack(rec);
        if jump {
            words[0] |= FLAG_JUMP;
        }
        let timeless = !rec.takes_time();
        if timeless {
            words[0] |= FLAG_TIMELESS;
        }
        for (dst, v) in cell.iter().zip(words).take(used) {
            dst.store(v, Ordering::Relaxed);
        }
        if let Some(t) = cell.get(T_WORD) {
            t.store(t_us, Ordering::Relaxed);
        }
        if !timeless {
            // 読み手はこの数だけを読む（`written` と組で読まない）ので、1 つ先に見えることはあっても減りはしない。
            let n = self.timed_written.load(Ordering::Relaxed);
            self.timed_written
                .store(n.wrapping_add(1), Ordering::Release);
        }
        self.written.store(w.wrapping_add(1), Ordering::Release);
        true
    }

    /// 検査だけ: 戻せない枠（種類の欄が 0）を 1 つ書く。
    #[cfg(test)]
    pub(super) fn push_unreadable_for_test(&self) {
        assert!(self.push(&Record::Seg { seg: 0 }, false, 0));
        let w = self.written.load(Ordering::Relaxed).wrapping_sub(1);
        if let Some(c) = self.cells.get(w as usize % self.cells.len()) {
            c[0].store(0, Ordering::Relaxed);
        }
    }

    pub(super) fn mark_full(&self) {
        self.full.store(true, Ordering::Release);
    }

    pub(crate) fn mark_broken(&self) {
        self.broken.store(true, Ordering::Release);
    }

    pub(super) fn set_code_at(&self, now_ms: u32) {
        self.has_watcher.store(true, Ordering::Release);
        self.code_at
            .store(SOME | u64::from(now_ms), Ordering::Relaxed);
    }

    pub(super) fn set_joined(&self, host: SocketAddr, now_ms: u32) {
        for (dst, v) in self.host.iter().zip(encode_addr(host)) {
            dst.store(v, Ordering::Relaxed);
        }
        self.joined_at
            .store(SOME | u64::from(now_ms), Ordering::Relaxed);
        self.joined.store(true, Ordering::Release);
    }

    pub(super) fn set_heard(&self, at_ms: Option<u32>) {
        if let Some(t) = at_ms {
            self.last_heard
                .store(SOME | u64::from(t), Ordering::Release);
        }
    }

    pub(super) fn publish_counts(&self, wc: &WatcherCounts, ac: &AsmCounts) {
        for (dst, v) in self.wc.iter().zip(wc_words(wc)) {
            dst.store(v, Ordering::Relaxed);
        }
        let AsmCounts {
            stored,
            dup,
            beyond,
            jumps,
        } = *ac;
        for (dst, v) in self.ac.iter().zip([stored, dup, beyond, jumps]) {
            dst.store(v, Ordering::Relaxed);
        }
    }

    pub(super) fn publish_relay(&self, c: &RelayCounts) {
        for (dst, v) in self.relay.iter().zip(relay_words(c)) {
            dst.store(v, Ordering::Relaxed);
        }
    }

    /// 受信スレッドが周回ごとに、いまの状態をゲームのスレッドが読めるよう原子変数に書く（断りの後に入り直せたら下ろす）。
    pub(super) fn set_refused(&self, on: bool) {
        self.refused.store(on, Ordering::Release);
    }

    /// これまでに書いた数（書き手だけが呼ぶ。記録のリングの位置）。
    pub(super) fn written_pos(&self) -> u32 {
        self.written.load(Ordering::Relaxed)
    }

    /// 照合のずれの要求を取る（位置, ポーズが崩れていたか）。
    pub(super) fn take_mismatch(&self) -> Option<(u32, bool)> {
        let v = self.mismatch.swap(0, Ordering::Acquire);
        (v & SOME != 0).then_some((v as u32, v & MISMATCH_BROKEN != 0))
    }

    /// ゲームのスレッドが観戦を終えたか、と、受信スレッドが親へ出してほしい BYE。
    pub(super) fn end_request(&self) -> Option<Option<ByeReason>> {
        self.ended
            .load(Ordering::Acquire)
            .then(|| ByeReason::from_u32(self.bye.load(Ordering::Relaxed)))
    }

    // ---------------- 読み手（ゲームのスレッド） ----------------

    /// 検査だけ: 書いてあって読んでいない数（時間を使わない記録も含む。溜まりは `timed_len`）。
    #[cfg(test)]
    pub fn len(&self) -> u32 {
        let r = self.read.load(Ordering::Relaxed);
        self.written.load(Ordering::Acquire).wrapping_sub(r)
    }

    /// 検査だけ: これまでに書いた数（時間を使わない記録も含む。1 表示に届いた数は `timed_written`）。
    #[cfg(test)]
    pub fn written(&self) -> u32 {
        self.written.load(Ordering::Acquire)
    }

    /// 次の 1 つを読む（進めない）。
    pub fn peek(&self) -> Option<Entry> {
        loop {
            let r = self.read.load(Ordering::Relaxed);
            if self.written.load(Ordering::Acquire) == r {
                return None;
            }
            let cell = self.cells.get(r as usize % self.cells.len())?;
            let mut words = [0u32; REC_WORDS];
            for (dst, src) in words.iter_mut().zip(cell.iter()) {
                *dst = src.load(Ordering::Relaxed);
            }
            let t_us = cell.get(T_WORD).map_or(0, |a| a.load(Ordering::Relaxed));
            match unpack(&words) {
                Some(rec) => {
                    return Some(Entry {
                        rec,
                        t_us,
                        jump: words[0] & FLAG_JUMP != 0,
                    });
                }
                None => {
                    self.bad.fetch_add(1, Ordering::Relaxed);
                    self.advance();
                }
            }
        }
    }

    /// 次に `peek` で読む記録の位置（読み手だけが呼ぶ。`written_pos` と同じ数え方）。
    pub fn read_pos(&self) -> u32 {
        self.read.load(Ordering::Relaxed)
    }

    /// 照合がずれた: `pos`（`read_pos` で取った Check の記録の位置）と、その seg でポーズが崩れていたか。受信スレッドが、親が中継役で、
    /// 記録がいまの親から来たなら直接へ戻る。取られる前に次が来たら上書きする。
    pub fn request_mismatch(&self, pos: u32, broken: bool) {
        let v = SOME | if broken { MISMATCH_BROKEN } else { 0 } | u64::from(pos);
        self.mismatch.store(v, Ordering::Release);
    }

    /// `peek` で読んだ 1 つを使い終えた（枠を返す）。
    pub fn advance(&self) {
        let r = self.read.load(Ordering::Relaxed);
        if self.written.load(Ordering::Acquire) != r {
            let timed = self
                .cells
                .get(r as usize % self.cells.len())
                .is_some_and(|c| c[0].load(Ordering::Relaxed) & FLAG_TIMELESS == 0);
            if timed {
                let n = self.timed_read.load(Ordering::Relaxed);
                self.timed_read.store(n.wrapping_add(1), Ordering::Relaxed);
            }
            self.read.store(r.wrapping_add(1), Ordering::Release);
        }
    }

    /// これまでに書いた、時間を使う記録の数（1 表示の間に届いた数を差で取る）。
    pub fn timed_written(&self) -> u32 {
        self.timed_written.load(Ordering::Acquire)
    }

    /// 書いてあって読んでいない、時間を使う記録の数（溜まり）。
    pub fn timed_len(&self) -> u32 {
        let n = self
            .timed_written()
            .wrapping_sub(self.timed_read.load(Ordering::Relaxed));
        n.min(self.cells.len() as u32)
    }

    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Acquire)
    }

    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Acquire)
    }

    pub fn is_joined(&self) -> bool {
        self.joined.load(Ordering::Acquire)
    }

    pub fn host_addr(&self) -> Option<SocketAddr> {
        if !self.is_joined() {
            return None;
        }
        let mut w = [0u32; ADDR_WORDS];
        for (dst, src) in w.iter_mut().zip(self.host.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        Some(decode_addr(&w))
    }

    /// 最後に何かを聞いてからの ms（何も聞いていなければ `None`）。
    pub fn silent_ms(&self, now_ms: u32) -> Option<u32> {
        // 受信スレッドの書いた時刻が、読んだ `now_ms` より少し後のこともある（`elapsed` は 0 にする）。
        opt_ms(self.last_heard.load(Ordering::Acquire)).map(|t| elapsed(now_ms, t))
    }

    /// 受信スレッドが `Watcher` を受け取ってから、ホストの最初の STREAM を受けるまでの ms。
    pub fn join_wait_ms(&self) -> Option<u32> {
        let code = opt_ms(self.code_at.load(Ordering::Relaxed))?;
        let joined = self
            .is_joined()
            .then(|| opt_ms(self.joined_at.load(Ordering::Relaxed)))??;
        Some(elapsed(joined, code))
    }

    pub fn counts(&self) -> Option<WatcherCounts> {
        if !self.has_watcher.load(Ordering::Acquire) {
            return None;
        }
        let mut w = [0u64; WC_LEN];
        for (dst, src) in w.iter_mut().zip(self.wc.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        Some(wc_of(w))
    }

    pub fn asm_counts(&self) -> Option<AsmCounts> {
        if !self.has_watcher.load(Ordering::Acquire) {
            return None;
        }
        let l = |i: usize| self.ac.get(i).map_or(0, |a| a.load(Ordering::Relaxed));
        Some(AsmCounts {
            stored: l(0),
            dup: l(1),
            beyond: l(2),
            jumps: l(3),
        })
    }

    pub fn relay_counts(&self) -> Option<RelayCounts> {
        if !self.has_watcher.load(Ordering::Acquire) {
            return None;
        }
        let mut w = [0u64; RELAY_LEN];
        for (dst, src) in w.iter_mut().zip(self.relay.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        Some(relay_counts_of(w))
    }

    pub fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    /// 読み手が戻せなかった枠の数（起きないはず。本ログに出す）。
    pub fn bad(&self) -> u32 {
        self.bad.load(Ordering::Relaxed)
    }

    /// 観戦を終えた。`bye` があれば受信スレッドがホストへ 1 通出す。2 回目からは何もしない。
    pub fn end(&self, bye: Option<ByeReason>) {
        if self.ended.load(Ordering::Relaxed) {
            return;
        }
        self.bye.store(bye_code(bye), Ordering::Relaxed);
        self.ended.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::wire::Seat;

    fn frame(n: u32) -> Record {
        Record::Frame {
            frame: n,
            p1: n as u16,
            p2: 0x8000 | n as u16,
        }
    }

    #[test]
    fn records_times_and_the_jump_mark_come_back_in_order_and_a_full_feed_refuses() {
        let f = WatchFeed::with_len(4);
        let name = Record::name(Seat::P2, &[b'q'; 32]);
        assert!(f.push(&name, true, 7));
        assert!(f.push(&frame(1), false, u32::MAX));
        assert!(f.push(&frame(2), false, 9));
        assert!(f.push(&Record::name(Seat::P1, b"a"), false, 10));
        assert!(!f.has_room());
        assert!(!f.push(&frame(3), false, 11), "満杯なのに書いた");
        assert_eq!(f.len(), 4);
        let want = [
            (name, 7, true),
            (frame(1), u32::MAX, false),
            (frame(2), 9, false),
            (Record::name(Seat::P1, b"a"), 10, false),
        ];
        for (rec, t_us, jump) in want {
            let e = f.peek().expect("読める");
            assert_eq!(f.peek(), Some(e), "peek で進んだ");
            assert_eq!(e, Entry { rec, t_us, jump });
            f.advance();
        }
        assert_eq!(f.peek(), None);
        f.advance();
        assert_eq!(f.len(), 0, "空のときの advance で進んだ");
        // 長い名前の枠に短い記録・短い名前を書いても、前の中身が混ざらない。
        assert!(f.push(&frame(4), false, 1));
        assert_eq!(f.peek().map(|e| e.rec), Some(frame(4)));
        assert_eq!((f.written(), f.bad()), (5, 0));
    }

    #[test]
    fn names_stats_and_checks_are_left_out_of_the_timed_counts() {
        let f = WatchFeed::with_len(16);
        let check = Record::Check {
            kind: higanr_core::wire::CheckKind::Game,
            check: higanr_core::wire::WatchCheck {
                frame: 60,
                rng_state: 1,
                rng_count: 2,
                life_1p: 3,
                life_2p: 4,
            },
        };
        let recs = [
            frame(0),
            Record::stats(Some(9), 1, None),
            check,
            Record::name(Seat::P1, b"n"),
            frame(1),
        ];
        for r in &recs {
            assert!(f.push(r, false, 0));
        }
        assert_eq!((f.len(), f.timed_len(), f.timed_written()), (5, 2, 2));
        let mut timed_left = Vec::new();
        for r in &recs {
            assert_eq!(
                f.peek().map(|e| e.rec),
                Some(*r),
                "印の bit で中身が変わった"
            );
            f.advance();
            timed_left.push(f.timed_len());
        }
        assert_eq!(timed_left, vec![1, 1, 1, 1, 0]);
        assert_eq!(f.timed_written(), 2, "読んでも書いた数は減らない");
    }

    #[test]
    fn the_production_feed_holds_about_an_hour_in_under_9_mb() {
        // 1 秒に 60 記録（マッチの語か選択の語）と、約 1 秒ごとの対戦の状態 1 記録・照合 1 記録。
        let secs = FEED_LEN / 62;
        assert!((3_500..=3_700).contains(&secs), "{secs} 秒");
        let bytes = FEED_LEN * std::mem::size_of::<[AtomicU32; CELL_WORDS]>();
        assert!(bytes <= 9_000_000, "{bytes} B");
    }

    #[test]
    fn a_feed_too_large_to_reserve_is_refused_instead_of_aborting() {
        assert!(WatchFeed::try_with_len(usize::MAX / 64).is_none());
        assert!(WatchFeed::try_with_len(8).is_some());
    }

    #[test]
    fn the_reader_never_allocates() {
        let f = WatchFeed::with_len(64);
        assert!(f.push(&frame(0), false, 0));
        let _ = f.peek();
        f.advance();
        let before = crate::test_alloc::count();
        for n in 0..200u32 {
            f.push(&frame(n), n == 3, n);
            let _ = f.peek();
            f.advance();
            let _ = (f.len(), f.is_joined(), f.host_addr(), f.silent_ms(n));
            let _ = (f.counts(), f.asm_counts(), f.join_wait_ms());
        }
        f.end(Some(ByeReason::Cancelled));
        assert_eq!(crate::test_alloc::count() - before, 0, "受け渡しで確保した");
    }

    #[test]
    fn the_states_cross_with_their_values() {
        let f = WatchFeed::with_len(1);
        assert_eq!(
            (f.counts(), f.asm_counts(), f.host_addr()),
            (None, None, None)
        );
        assert_eq!(
            (f.silent_ms(5), f.join_wait_ms(), f.end_request()),
            (None, None, None)
        );
        f.set_code_at(100);
        f.set_heard(Some(u32::MAX - 5));
        assert_eq!(f.silent_ms(10), Some(16), "折り返しをまたぐ沈黙");
        f.set_heard(Some(200));
        assert_eq!(f.silent_ms(150), Some(0), "後に書かれた時刻");
        let host = "[2001:db8::1]:47001".parse().unwrap();
        f.set_joined(host, 350);
        assert_eq!((f.host_addr(), f.join_wait_ms()), (Some(host), Some(250)));
        let wc = WatcherCounts {
            joins_sent: 1,
            acks_sent: 2,
            streams: 3,
            foreign: 4,
            not_hgrt: 5,
            not_hgrt_v6: 6,
            wrong_session: 7,
            bad_tag: 8,
            undecodable: 9,
            other_version: 10,
            unexpected: 11,
            byes: 12,
            no_keys: 13,
            learned_host: u64::MAX,
        };
        let ac = AsmCounts {
            stored: 21,
            dup: 22,
            beyond: 23,
            jumps: 24,
        };
        f.publish_counts(&wc, &ac);
        assert_eq!((f.counts(), f.asm_counts()), (Some(wc), Some(ac)));
        f.end(Some(ByeReason::Cancelled));
        f.end(None);
        assert_eq!(
            f.end_request(),
            Some(Some(ByeReason::Cancelled)),
            "2 回目で上書きした"
        );
        let g = WatchFeed::with_len(1);
        g.end(None);
        assert_eq!(g.end_request(), Some(None));
    }
}
