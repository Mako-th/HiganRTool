//! 進め直しを 1 回の描画の中で行う形の判断（pure。unsafe 無し）: 走査の呼び方と、進め直しの間に捨てる・戻す出力（SE・BGM・文字の数）の手順。
//! Win32・ゲームのメモリ・入口のアセンブリは `rollback_win.rs`。
//!
//! 本物の rollback は 1 フレームの中で k tick を進め直す。do_frame の走査 `0x42C700`（`0x42DCA9` の call）を、書き戻しのあった
//! フレームだけ k 回呼び、途中の回を描かない形でも k = 1・8・60 で D1 と D2 が一致するかを見る。
//!
//! 手順（1 フレームの中。ゲームのスレッドで）:
//! 1. 走査を 1 回呼ぶ（普段はこれだけで戻り値をそのまま返す）。書き戻しのフレームでは、この回の入力の地点で `Probe` が D1 をコピー、
//!    S を書き戻し、記録した 1 個目の入力ワードを feed する。
//! 2. 走査の後、`Probe` に「あと何回呼ぶか」を聞く（`take_owed`。書き戻しのフレームだけ `Some(k − 1)`、ほかは `None`）。
//! 3. 呼ぶ回が残っていれば、回と回の間（走査の外。臨界区間の外）で走査を呼び直す。最後の回の前に、途中の回が積んだ SE・BGM・
//!    文字の数を戻す。回の入力の地点は記録した入力ワードを順に feed する。
//! 4. 途中の回が 1 を返したら打ち切る（普通のマッチの走査はノード数 ≥ 2 を返す。1 は「3 を返すノードで抜けた」= 場面の切替・読み込み・
//!    一時停止・描画だけの出口。0 と負の値は do_frame が `0x430230` を呼んで描画へ進まずに抜ける値）。残りは次のフレームから実時間で続ける。
//!
//! 進め直しの間の出力: 走査の外の do_frame は 1 フレームに 1 回しか SE・BGM を鳴らさず（`0x42DCB5 call 0x43F1F0`）、描画で文字の数を
//! 0 に戻す（`0x00431611 call 0x430220`）。途中の回は描画も鳴らしもしないので積んだ物が溜まる。
//! - SE（`SoundManager +0x620`・`+0x650`）: 最後の回の前に id を −1・回数を 0 にする。
//! - BGM の命令（`+0x1F88` から 0x10C × 31）: 進め直しの前（D1 の点）にバックアップし、最後の回の前に戻す。
//! - 文字の数（`AsciiManager +0x8264`）: 同じくバックアップして戻す（戻さないと古い数字が重なり、256 本を超えた分は捨てられる）。
//!
//! ゲームのスレッドに確保・ロック・パニックを置かない（`SavedOutputs` は固定長。`run_scan` は `ScanIo` の呼び出しだけ）。
#![forbid(unsafe_code)]

use crate::rollback_probe::{Outcome, TrialRecord};
use crate::th09_addr::{
    ASCII_COUNT, SE_ID_CUTIN, SE_ID_CUTIN_SPELL, SE_ID_HIT, SE_ID_LAST_LIFE, SND_BGM_QUEUE,
    SND_BGM_SLOT_BYTES, SND_BGM_SLOTS, SND_SE_COUNTS, SND_SE_IDS, SND_SE_MAX_COUNT,
    SND_SE_PAN_SLOT_BYTES, SND_SE_PANS, SND_SE_SLOTS, SOUND_MANAGER,
};

/// 進め直しの形。既定は `Realtime`（`resim` を書かない ini は今までどおり動く）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResimMode {
    /// 実時間の k フレーム（描画つき）。
    #[default]
    Realtime,
    /// 1 回の描画の中で走査を k 回。
    Frame,
}

impl ResimMode {
    pub fn name(self) -> &'static str {
        match self {
            ResimMode::Realtime => "realtime",
            ResimMode::Frame => "frame",
        }
    }

    pub fn parse(v: &str) -> Result<ResimMode, String> {
        match v {
            "realtime" => Ok(ResimMode::Realtime),
            "frame" => Ok(ResimMode::Frame),
            _ => Err(format!("resim の値 {v:?} は realtime か frame")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanAbort {
    /// 1（ノードの `3` = 抜ける）。
    One,
    /// 0（ノードの `4`。do_frame は `0x430230` を呼んで 1 を返し、描画へ進まない）。
    Zero,
    Negative,
}

pub fn abort_of(ret: i32) -> Option<ScanAbort> {
    match ret {
        1 => Some(ScanAbort::One),
        0 => Some(ScanAbort::Zero),
        r if r < 0 => Some(ScanAbort::Negative),
        _ => None,
    }
}

pub trait ScanIo {
    fn call_scan(&mut self) -> i32;
    /// このフレームであと何回呼ぶか。書き戻しのフレームだけ `Some(k − 1)`（k = 1 は `Some(0)`: 追加は無いが最初の回の記録は取る）、ほかは `None`。`call_scan` の後に 1 回だけ呼ぶ。
    fn take_owed(&mut self) -> Option<u32>;
    fn before_last_call(&mut self);
    fn after_call(&mut self, index: u32, ret: i32);
    fn aborted(&mut self, index: u32, why: ScanAbort);
}

/// do_frame の走査の call の代わり。普段は `call_scan` 1 回と `take_owed` 1 回で戻り値をそのまま返す。書き戻しのフレームは `1 + owed` 回呼び、
/// 途中の回が打ち切りの値を返したらその値をそのまま返して残りを呼ばない。
pub fn run_scan(io: &mut dyn ScanIo) -> i32 {
    let mut ret = io.call_scan();
    let Some(owed) = io.take_owed() else {
        return ret;
    };
    io.after_call(0, ret);
    if let Some(why) = abort_of(ret) {
        io.aborted(0, why);
        return ret;
    }
    let mut done = 0u32;
    while done < owed {
        if done + 1 == owed {
            io.before_last_call();
        }
        ret = io.call_scan();
        done += 1;
        io.after_call(done, ret);
        if let Some(why) = abort_of(ret) {
            io.aborted(done, why);
            return ret;
        }
    }
    ret
}

pub const BGM_WORDS: usize = (SND_BGM_SLOT_BYTES * SND_BGM_SLOTS / 4) as usize;
const _: () = assert!(SND_BGM_SLOT_BYTES.is_multiple_of(4));

/// 出力を読み書きしてよい範囲 `[始まり, 終わり)`（SE の id と回数 / BGM の待ち行列 / 文字の数）。本物の読み書き（`rollback_win::RawOutputs`）はこの外を断る。
pub const OUTPUT_RANGES: [(u32, u32); 3] = [
    (
        SOUND_MANAGER + SND_SE_IDS,
        SOUND_MANAGER + SND_SE_COUNTS + SND_SE_SLOTS * 4,
    ),
    (
        SOUND_MANAGER + SND_BGM_QUEUE,
        SOUND_MANAGER + SND_BGM_QUEUE + SND_BGM_SLOT_BYTES * SND_BGM_SLOTS,
    ),
    (ASCII_COUNT, ASCII_COUNT + 4),
];

pub fn output_addr_allowed(addr: u32) -> bool {
    addr.is_multiple_of(4)
        && OUTPUT_RANGES
            .iter()
            .any(|&(s, e)| s <= addr && addr.checked_add(4).is_some_and(|x| x <= e))
}

/// 読むだけの範囲: SE のパンの欄 `[+0x680, +0x680 + 12 × 0x200)`。進め直しの回が積んだ SE のパンをバックアップするため。書かない（id と回数を空にすればパンは消える）。
pub const OUTPUT_READ_RANGES: [(u32, u32); 1] = [(
    SOUND_MANAGER + SND_SE_PANS,
    SOUND_MANAGER + SND_SE_PANS + SND_SE_SLOTS * SND_SE_PAN_SLOT_BYTES,
)];

pub fn output_addr_readable(addr: u32) -> bool {
    output_addr_allowed(addr)
        || (addr.is_multiple_of(4)
            && OUTPUT_READ_RANGES
                .iter()
                .any(|&(s, e)| s <= addr && addr.checked_add(4).is_some_and(|x| x <= e)))
}

pub trait OutputMem {
    fn read_u32(&mut self, addr: u32) -> Option<u32>;
    fn write_u32(&mut self, addr: u32, v: u32) -> bool;
}

#[derive(Clone, Copy)]
pub struct SavedOutputs {
    pub ascii_count: u32,
    pub bgm: [u32; BGM_WORDS],
    /// 全部読めてバックアップしたか（読めなかったら戻さない。一部だけ戻すと壊れた状態を作る）。
    pub valid: bool,
}

impl SavedOutputs {
    pub const EMPTY: SavedOutputs = SavedOutputs {
        ascii_count: 0,
        bgm: [0; BGM_WORDS],
        valid: false,
    };
}

pub fn capture_outputs(mem: &mut dyn OutputMem, out: &mut SavedOutputs) -> bool {
    out.valid = false;
    let Some(count) = mem.read_u32(ASCII_COUNT) else {
        return false;
    };
    for (i, w) in out.bgm.iter_mut().enumerate() {
        match mem.read_u32(SOUND_MANAGER + SND_BGM_QUEUE + i as u32 * 4) {
            Some(v) => *w = v,
            None => return false,
        }
    }
    out.ascii_count = count;
    out.valid = true;
    true
}

/// 最後の回の前に出力を戻す: SE の id を −1・回数を 0 に、BGM の待ち行列と文字の数をバックアップの値へ。バックアップが無効なら何も書かず false。書きは全部が書けたときだけ true。
pub fn apply_outputs(mem: &mut dyn OutputMem, saved: &SavedOutputs) -> bool {
    if !saved.valid {
        return false;
    }
    let mut ok = true;
    for i in 0..SND_SE_SLOTS {
        ok &= mem.write_u32(SOUND_MANAGER + SND_SE_IDS + i * 4, u32::MAX);
        ok &= mem.write_u32(SOUND_MANAGER + SND_SE_COUNTS + i * 4, 0);
    }
    for (i, &w) in saved.bgm.iter().enumerate() {
        ok &= mem.write_u32(SOUND_MANAGER + SND_BGM_QUEUE + i as u32 * 4, w);
    }
    ok &= mem.write_u32(ASCII_COUNT, saved.ascii_count);
    ok
}

// ---- 進め直しの全部の回の後に、進め直しの前の値へ戻す ----
//
// `apply_outputs` は「1 回の do_frame の中で本物の走査の後に鳴らす」形の規則。Present の後の進め直しは全部が追加の回なので、
// 進め直しの回の出力は全部捨て、進め直しの前の値（SE の id と回数も含む）へそのまま戻す。バックアップする所は `OUTPUT_RANGES` の 3 つだけ。
/// 進め直しの前にバックアップする物（固定長。約 8.4 KB）。
#[derive(Clone, Copy)]
pub struct FullOutputs {
    pub se_ids: [u32; SND_SE_SLOTS as usize],
    pub se_counts: [u32; SND_SE_SLOTS as usize],
    pub bgm: [u32; BGM_WORDS],
    pub ascii_count: u32,
    pub valid: bool,
}

impl FullOutputs {
    pub const EMPTY: FullOutputs = FullOutputs {
        se_ids: [0; SND_SE_SLOTS as usize],
        se_counts: [0; SND_SE_SLOTS as usize],
        bgm: [0; BGM_WORDS],
        ascii_count: 0,
        valid: false,
    };
}

pub fn capture_full(mem: &mut dyn OutputMem, out: &mut FullOutputs) -> bool {
    out.valid = false;
    for i in 0..SND_SE_SLOTS as usize {
        let (Some(id), Some(n)) = (
            mem.read_u32(SOUND_MANAGER + SND_SE_IDS + i as u32 * 4),
            mem.read_u32(SOUND_MANAGER + SND_SE_COUNTS + i as u32 * 4),
        ) else {
            return false;
        };
        out.se_ids[i] = id;
        out.se_counts[i] = n;
    }
    for (i, w) in out.bgm.iter_mut().enumerate() {
        match mem.read_u32(SOUND_MANAGER + SND_BGM_QUEUE + i as u32 * 4) {
            Some(v) => *w = v,
            None => return false,
        }
    }
    let Some(count) = mem.read_u32(ASCII_COUNT) else {
        return false;
    };
    out.ascii_count = count;
    out.valid = true;
    true
}

pub fn restore_full(mem: &mut dyn OutputMem, saved: &FullOutputs) -> bool {
    if !saved.valid {
        return false;
    }
    let mut ok = true;
    for i in 0..SND_SE_SLOTS as usize {
        ok &= mem.write_u32(SOUND_MANAGER + SND_SE_IDS + i as u32 * 4, saved.se_ids[i]);
        ok &= mem.write_u32(
            SOUND_MANAGER + SND_SE_COUNTS + i as u32 * 4,
            saved.se_counts[i],
        );
    }
    for (i, &w) in saved.bgm.iter().enumerate() {
        ok &= mem.write_u32(SOUND_MANAGER + SND_BGM_QUEUE + i as u32 * 4, w);
    }
    ok &= mem.write_u32(ASCII_COUNT, saved.ascii_count);
    ok
}

// ---- 進め直しで初めて起きた SE を遅れて鳴らす ----
//
// `restore_full` は進め直しの回が積んだ SE を全部捨てる。予測が外れて「相手の入力の変わり目で起きる出来事」（カットイン・被弾）は
// 生きた tick では起きず（予測は相手の直前の確定の入力ワードの繰り返し）、進め直しの確定の回で初めて起きるので、その SE は一度も鳴らずに消えていた。
// 直し: フレームごとに「鳴らした（積まれた）SE の id の集合」を覚え、進め直しの tick ごとに積まれた SE を読んで空にし、前後 2 フレームの
// どこにも無い id だけを、`restore_full` の後に積み直す。次の do_frame の `0x43F1F0` が鳴らす。
/// SE の id を追える上限（ビット集合の大きさ）。ゲームの表は id < 0x86 のはず（`0x43E2F0` が `+0x408 + id × 4` を書き、`+0x620` の手前の 0x218 B が表の大きさ）。
/// 越えた id は追わず（積み直さず）数える。
pub const SE_ID_LIMIT: u32 = 192;
/// フレームごとの記録のリングの長さ。巻き戻しの幅（N − F ≤ `MAX_AHEAD` = 16）に前後 2 フレームを足しても足りる大きさ。
pub const SE_RING: usize = 32;
pub const SE_NEAR: u32 = 2;
pub const SE_PENDING_CAP: usize = 32;

/// 被弾音（`SE_ID_HIT`・`SE_ID_LAST_LIFE`）の鳴らし方（ini の `rollback_hit_se`）。予測で進めた tick の被弾は確定で起きなかったり位置がずれたりしうるが、
/// 鳴らした音は取り消せないので、鳴る前に待つ。delay 方式には予測が無いので効かない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HitSe {
    /// 確定した tick の被弾だけを即座に鳴らし、未確定の tick の被弾は確定の回で初めて出たときに積み直して鳴らす。
    #[default]
    Confirmed,
    /// 今までどおり、生きた tick の被弾を即座に鳴らす（予測が外れても取り消せない）。
    Live,
}

impl HitSe {
    pub fn parse(v: &str) -> Result<HitSe, String> {
        match v {
            "confirmed" => Ok(HitSe::Confirmed),
            "live" => Ok(HitSe::Live),
            _ => Err(format!("rollback_hit_se の値 {v:?} は confirmed か live")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            HitSe::Confirmed => "confirmed",
            HitSe::Live => "live",
        }
    }
}

pub const fn is_hit_se(id: u32) -> bool {
    id == SE_ID_HIT || id == SE_ID_LAST_LIFE
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeEvent {
    pub id: u32,
    pub pan: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct SeQueue {
    pub n: usize,
    pub ev: [SeEvent; SND_SE_SLOTS as usize],
    pub count: [u32; SND_SE_SLOTS as usize],
    pub ok: bool,
}

impl SeQueue {
    pub const EMPTY: SeQueue = SeQueue {
        n: 0,
        ev: [SeEvent { id: 0, pan: 0 }; SND_SE_SLOTS as usize],
        count: [0; SND_SE_SLOTS as usize],
        ok: true,
    };

    pub fn events(&self) -> impl Iterator<Item = (SeEvent, u32)> + '_ {
        self.ev
            .iter()
            .zip(self.count.iter())
            .take(self.n)
            .map(|(e, c)| (*e, *c))
    }

    fn count_of(&self, id: u32) -> u32 {
        self.events()
            .find(|(e, _)| e.id == id)
            .map_or(0, |(_, c)| c)
    }

    fn push_slot(&mut self, e: SeEvent, count: u32) -> bool {
        match (self.ev.get_mut(self.n), self.count.get_mut(self.n)) {
            (Some(se), Some(sc)) => {
                *se = e;
                *sc = count;
                self.n += 1;
                true
            }
            _ => false,
        }
    }
}

/// SE の待ち行列を読む（書かない）。空き枠（id が負）か 12 枠で止める（鳴らす `0x43F1F0` も最初の空き枠で止めるので読み落としは無い）。パンは回数個の平均（ゲームが使うのと同じ `sum / count`）。
pub fn read_se_queue(mem: &mut dyn OutputMem) -> SeQueue {
    let mut q = SeQueue::EMPTY;
    for slot in 0..SND_SE_SLOTS {
        let Some(id) = mem.read_u32(SOUND_MANAGER + SND_SE_IDS + slot * 4) else {
            q.ok = false;
            return q;
        };
        if (id as i32) < 0 {
            return q;
        }
        let Some(count) = mem.read_u32(SOUND_MANAGER + SND_SE_COUNTS + slot * 4) else {
            q.ok = false;
            return q;
        };
        let count = count.min(SND_SE_MAX_COUNT);
        let mut sum = 0i32;
        for k in 0..count {
            let addr = SOUND_MANAGER + SND_SE_PANS + slot * SND_SE_PAN_SLOT_BYTES + k * 4;
            let Some(p) = mem.read_u32(addr) else {
                q.ok = false;
                return q;
            };
            sum = sum.wrapping_add(p as i32);
        }
        let pan = i32::try_from(count)
            .ok()
            .and_then(|c| sum.checked_div(c))
            .unwrap_or(0);
        if let (Some(e), Some(c)) = (q.ev.get_mut(q.n), q.count.get_mut(q.n)) {
            *e = SeEvent { id, pan };
            *c = count;
            q.n += 1;
        }
    }
    q
}

pub fn clear_se_queue(mem: &mut dyn OutputMem) -> bool {
    let mut ok = true;
    for slot in 0..SND_SE_SLOTS {
        ok &= mem.write_u32(SOUND_MANAGER + SND_SE_IDS + slot * 4, u32::MAX);
        ok &= mem.write_u32(SOUND_MANAGER + SND_SE_COUNTS + slot * 4, 0);
    }
    ok
}

/// フレームごとの SE の id の集合のリング（確保なし）。フレーム番号を印にして、印が違う枠は空とみなす（seg が変わるとフレーム番号が 0 に戻るので `clear` で捨てる）。
#[derive(Clone, Copy)]
struct SeRing {
    frame: [Option<u32>; SE_RING],
    bits: [[u64; 3]; SE_RING],
}

const _: () = assert!(SE_ID_LIMIT as usize <= 3 * 64);

impl SeRing {
    const EMPTY: SeRing = SeRing {
        frame: [None; SE_RING],
        bits: [[0; 3]; SE_RING],
    };

    fn slot(frame: u32) -> usize {
        frame as usize % SE_RING
    }

    fn add(&mut self, frame: u32, id: u32) -> bool {
        if id >= SE_ID_LIMIT {
            return false;
        }
        let s = Self::slot(frame);
        let (Some(tag), Some(bits)) = (self.frame.get_mut(s), self.bits.get_mut(s)) else {
            return false;
        };
        if *tag != Some(frame) {
            *tag = Some(frame);
            *bits = [0; 3];
        }
        if let Some(w) = bits.get_mut(id as usize / 64) {
            *w |= 1u64 << (id % 64);
        }
        true
    }

    fn has(&self, frame: u32, id: u32) -> bool {
        let s = Self::slot(frame);
        if self.frame.get(s).copied().flatten() != Some(frame) {
            return false;
        }
        self.bits
            .get(s)
            .and_then(|b| b.get(id as usize / 64))
            .is_some_and(|w| w & (1u64 << (id % 64)) != 0)
    }

    fn has_near(&self, frame: u32, id: u32, near: u32) -> bool {
        let from = frame.saturating_sub(near);
        let to = frame.saturating_add(near);
        (from..=to).any(|f| self.has(f, id))
    }

    fn take_near(&mut self, frame: u32, id: u32, near: u32) -> bool {
        for d in 0..=near {
            for f in [frame.saturating_sub(d), frame.saturating_add(d)] {
                if self.has(f, id) {
                    let s = Self::slot(f);
                    if let Some(w) = self
                        .bits
                        .get_mut(s)
                        .and_then(|b| b.get_mut(id as usize / 64))
                    {
                        *w &= !(1u64 << (id % 64));
                    }
                    return true;
                }
            }
        }
        false
    }
}

/// フレームごとの「鳴らした（積まれた）SE の id の集合」と、被弾音を確定まで待たせた（鳴らさずに待ち行列から抜いた）id の集合。確保なし。`clear` で両方を捨てる。
#[derive(Clone, Copy)]
pub struct SePlayed {
    sounded: SeRing,
    /// 生きた tick が積んだが確定まで鳴らさずに待たせた被弾音。確定の回が同じ出来事を見つけたら消す。消えずに残ったものが「確定では起きなかった被弾音」（幻）。
    held: SeRing,
}

impl SePlayed {
    pub const EMPTY: SePlayed = SePlayed {
        sounded: SeRing::EMPTY,
        held: SeRing::EMPTY,
    };

    pub fn clear(&mut self) {
        *self = SePlayed::EMPTY;
    }

    pub fn add(&mut self, frame: u32, id: u32) -> bool {
        self.sounded.add(frame, id)
    }

    pub fn has_near(&self, frame: u32, id: u32, near: u32) -> bool {
        self.sounded.has_near(frame, id, near)
    }

    fn hold(&mut self, frame: u32, id: u32) -> bool {
        self.held.add(frame, id)
    }

    fn take_held_near(&mut self, frame: u32, id: u32, near: u32) -> bool {
        self.held.take_near(frame, id, near)
    }
}

#[derive(Clone, Copy)]
pub struct SePending {
    n: usize,
    ev: [(SeEvent, u32); SE_PENDING_CAP],
    /// この進め直しで更新した一番新しいフレーム（遅れ = `top + 1 − frame` の測りに使う）。
    top: u32,
}

impl SePending {
    pub const EMPTY: SePending = SePending {
        n: 0,
        ev: [(SeEvent { id: 0, pan: 0 }, 0); SE_PENDING_CAP],
        top: 0,
    };

    pub fn clear(&mut self) {
        self.n = 0;
        self.top = 0;
    }

    fn push(&mut self, e: SeEvent, frame: u32) -> bool {
        match self.ev.get_mut(self.n) {
            Some(slot) => {
                *slot = (e, frame);
                self.n += 1;
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeStats {
    pub live_ids: u32,
    pub live_cutin: u32,
    pub live_spell: u32,
    /// 進め直しの tick で積まれた SE の数（id × tick）。下の 4 つの和 ＋ 追えなかった id。確定まで待たせる被弾音の予測の回の分は数えない（確定の回で数える）。
    pub resim_ids: u32,
    /// 進め直しで初めて出た（前後 2 フレームのどこにも無かった）SE の数と、うちカットイン（0xE）・スペルカードのカットイン（0x35）。
    pub new_ids: u32,
    pub new_cutin: u32,
    pub new_spell: u32,
    pub suppressed: u32,
    pub replayed: u32,
    pub replayed_cutin: u32,
    pub replayed_spell: u32,
    pub late_sum: u64,
    pub late_max: u32,
    pub refused: u32,
    pub dropped: u32,
    pub unreadable: u32,
    pub untracked: u32,
    pub hit_mode: HitSe,
    /// 被弾音: 未確定の生きた tick が積んだので確定まで鳴らさずに待たせた数（`HitSe::Confirmed` のときだけ増える。`live_ids` にはこの分を数えない）。
    pub hit_waited: u32,
    /// 確定の回が同じ出来事（前後 2 フレーム）を出して、待たせたものと対応が付いた数。待たせた数との差が、確定では起きなかった被弾音（幻）。
    pub hit_matched: u32,
    /// 遅れて積み直した被弾音の数（`replayed` のうち `is_hit_se`）。待たせたものに対応が付かなかった分は、予測では起きず確定で初めて起きた被弾。
    pub replayed_hit: u32,
    /// `HitSe::Live` のときだけ増える。予測のうちに鳴らした被弾音の数: 未確定の生きた tick が積んだ分と、進め直しの予測の回で初めて出て記録に積めた分。
    /// 記録が溢れて積めなかった分は鳴らないので数えない。確定の回の突き合わせ（`hit_live_matched`）が枠ごとなので、1 つの枠（id × tick）を 1 と数える。
    pub hit_live_sounded: u32,
    /// 上のうち、確定の回が同じ出来事（前後 2 フレーム）を出して対応が付いた数。鳴らした数との差が、鳴ったのに確定では起きなかった被弾音（幻）。
    pub hit_live_matched: u32,
}

/// 生きた tick（`pre` = 走査の前、`post` = 走査の後の待ち行列）で積まれた SE を `frame` の記録に足す。走査の前に積まれていた分（直前の進め直しが遅れて積み直したもの）は含めない。
pub fn note_live_se(
    played: &mut SePlayed,
    stats: &mut SeStats,
    frame: u32,
    pre: &SeQueue,
    post: &SeQueue,
) {
    if !post.ok {
        stats.unreadable = stats.unreadable.saturating_add(1);
    }
    // 走査の前が読めなかったときは、後の分を全部この tick のものとみなす。
    let before = if pre.ok { *pre } else { SeQueue::EMPTY };
    for (e, c) in post.events() {
        if c > before.count_of(e.id) {
            stats.live_ids = stats.live_ids.saturating_add(1);
            match e.id {
                SE_ID_CUTIN => stats.live_cutin = stats.live_cutin.saturating_add(1),
                SE_ID_CUTIN_SPELL => stats.live_spell = stats.live_spell.saturating_add(1),
                _ => {}
            }
            if !played.add(frame, e.id) {
                stats.untracked = stats.untracked.saturating_add(1);
            }
        }
    }
}

/// 未確定の生きた tick（`frame`）が積んだ被弾音を、確定まで鳴らさずに待たせる（`HitSe::Confirmed`）。走査で増えた分のうち `is_hit_se` の id を引いた残りの
/// 待ち行列を返す（呼び手がゲームの待ち行列をこの残りへ書き直す）。待たせた被弾音は `played` の待ちの記録に足し、鳴った記録（`add`）には足さない。
/// 走査の前に積まれていた分は引かない。引くものが無い・読めなかったら None。
pub fn hold_hit_se(
    played: &mut SePlayed,
    stats: &mut SeStats,
    frame: u32,
    pre: &SeQueue,
    post: &SeQueue,
) -> Option<SeQueue> {
    if !pre.ok || !post.ok {
        return None;
    }
    let mut rest = SeQueue::EMPTY;
    let mut held = 0u32;
    for (e, c) in post.events() {
        let before = pre.count_of(e.id);
        let (keep, pan) = if is_hit_se(e.id) && c > before {
            held = held.saturating_add(c - before);
            if !played.hold(frame, e.id) {
                stats.untracked = stats.untracked.saturating_add(1);
            }
            let pan = pre
                .events()
                .find(|(p, _)| p.id == e.id)
                .map_or(e.pan, |(p, _)| p.pan);
            (before, pan)
        } else {
            (c, e.pan)
        };
        if keep > 0 {
            rest.push_slot(SeEvent { id: e.id, pan }, keep);
        }
    }
    if held == 0 {
        return None;
    }
    stats.hit_waited = stats.hit_waited.saturating_add(held);
    Some(rest)
}

/// `HitSe::Live` で未確定の生きた tick が積んだ被弾音を、待たせずに（鳴る）待ちの記録にだけ覚える。確定の回が同じ出来事を出せば `sort_resim_se` が消して
/// `hit_live_matched` に数える。残ったものが鳴ったのに確定では起きなかった被弾音。固定長の加算だけで確保しない。
pub fn note_live_hit(
    played: &mut SePlayed,
    stats: &mut SeStats,
    frame: u32,
    pre: &SeQueue,
    post: &SeQueue,
) {
    if !pre.ok || !post.ok {
        return;
    }
    for (e, c) in post.events() {
        if is_hit_se(e.id) && c > pre.count_of(e.id) {
            if played.hold(frame, e.id) {
                stats.hit_live_sounded = stats.hit_live_sounded.saturating_add(1);
            } else {
                stats.untracked = stats.untracked.saturating_add(1);
            }
        }
    }
}

/// 進め直しの tick（`frame`）が積んだ SE（`q`。空にする前に読んだもの）を仕分ける: 前後 2 フレームのどこにも無い id は新しいので記録に足して積み直しの列へ、あるものは抑える。
///
/// `hit` が `Confirmed` のとき、被弾音は予測の回（`confirmed` が偽）では鳴らさず記録にも足さない。確定の回で出たものは、未確定の生きた tick が待たせたものの前後 2 フレームに
/// あれば対応を付ける（ずれて対応が付かなくても鳴るのは確定の回の 1 回だけ）。`hit` が `Live` で予測の回に初めて出た被弾音は、記録に積めたときだけ `hit_live_sounded` に数えて待ちの記録に覚える。
pub fn sort_resim_se(
    played: &mut SePlayed,
    pending: &mut SePending,
    stats: &mut SeStats,
    frame: u32,
    q: &SeQueue,
    hit: HitSe,
    confirmed: bool,
) {
    if !q.ok {
        stats.unreadable = stats.unreadable.saturating_add(1);
    }
    pending.top = pending.top.max(frame);
    for (e, _) in q.events() {
        let waits = hit == HitSe::Confirmed && is_hit_se(e.id);
        // 予測の回の被弾音は新しい／抑えたのどちらにも数えない（確定の回で数える）ので `resim_ids` にも足さない。
        if waits && !confirmed {
            continue;
        }
        stats.resim_ids = stats.resim_ids.saturating_add(1);
        if e.id >= SE_ID_LIMIT {
            stats.untracked = stats.untracked.saturating_add(1);
            continue;
        }
        if is_hit_se(e.id) && confirmed && played.take_held_near(frame, e.id, SE_NEAR) {
            match hit {
                HitSe::Confirmed => stats.hit_matched = stats.hit_matched.saturating_add(1),
                HitSe::Live => stats.hit_live_matched = stats.hit_live_matched.saturating_add(1),
            }
        }
        if played.has_near(frame, e.id, SE_NEAR) {
            stats.suppressed = stats.suppressed.saturating_add(1);
            continue;
        }
        played.add(frame, e.id);
        stats.new_ids = stats.new_ids.saturating_add(1);
        match e.id {
            SE_ID_CUTIN => stats.new_cutin = stats.new_cutin.saturating_add(1),
            SE_ID_CUTIN_SPELL => stats.new_spell = stats.new_spell.saturating_add(1),
            _ => {}
        }
        // `Live` の予測の回で初めて出た被弾音は、積めたのでこのあと積み直されて実際に鳴る。積めなかった分は鳴らないので数えず、待ちの記録にも覚えない。
        if !pending.push(e, frame) {
            stats.dropped = stats.dropped.saturating_add(1);
        } else if hit == HitSe::Live && !confirmed && is_hit_se(e.id) {
            if played.hold(frame, e.id) {
                stats.hit_live_sounded = stats.hit_live_sounded.saturating_add(1);
            } else {
                stats.untracked = stats.untracked.saturating_add(1);
            }
        }
    }
}

pub fn flush_se(
    pending: &mut SePending,
    stats: &mut SeStats,
    push: &mut dyn FnMut(SeEvent) -> bool,
) {
    for (e, frame) in pending.ev.iter().take(pending.n) {
        if !push(*e) {
            stats.refused = stats.refused.saturating_add(1);
            continue;
        }
        stats.replayed = stats.replayed.saturating_add(1);
        if is_hit_se(e.id) {
            stats.replayed_hit = stats.replayed_hit.saturating_add(1);
        }
        match e.id {
            SE_ID_CUTIN => stats.replayed_cutin = stats.replayed_cutin.saturating_add(1),
            SE_ID_CUTIN_SPELL => stats.replayed_spell = stats.replayed_spell.saturating_add(1),
            _ => {}
        }
        let late = pending.top.saturating_add(1).saturating_sub(*frame);
        stats.late_sum = stats.late_sum.saturating_add(u64::from(late));
        stats.late_max = stats.late_max.max(late);
    }
    pending.clear();
}

pub fn drop_pending_se(pending: &mut SePending, stats: &mut SeStats) {
    stats.dropped = stats
        .dropped
        .saturating_add(u32::try_from(pending.n).unwrap_or(u32::MAX));
    pending.clear();
}

pub fn code_head_matches(base: u32, head: &[u8], read: &mut dyn FnMut(u32) -> Option<u32>) -> bool {
    head.chunks(4).enumerate().all(|(i, want)| {
        let at = base.wrapping_add(i as u32 * 4);
        read(at).is_some_and(|w| w.to_le_bytes().iter().zip(want).all(|(a, b)| a == b))
    })
}

pub fn se_summary_line(s: &SeStats) -> String {
    let mean = if s.replayed == 0 {
        "—".to_string()
    } else {
        format!("{:.2}", s.late_sum as f64 / f64::from(s.replayed))
    };
    let live_phantom = if s.hit_mode == HitSe::Live {
        format!(
            "。live の被弾音（予測のうちに鳴らした）: 鳴らした {}・うち確定の回で見つかった {}・見つからなかった幻の被弾音（鳴ったのに確定では起きなかった。走り終わりの未確定の分を含む） {}",
            s.hit_live_sounded,
            s.hit_live_matched,
            s.hit_live_sounded.saturating_sub(s.hit_live_matched)
        )
    } else {
        String::new()
    };
    format!(
        "巻き戻し（製品の経路）: SE 生きた tick で積まれた {}（カットイン 0xE {} / スペルカード 0x35 {}）・進め直しの tick で積まれた {}（うち前後 2 フレームのどこにも無く初めて出た {}（カットイン 0xE {} / スペルカード 0x35 {}）・前後 2 フレームの中で抑えた {}・追えない id {}）。遅れて積み直した {}（カットイン 0xE {} / スペルカード 0x35 {}。遅れ N − 出来事のフレーム 平均 {} / 最大 {}）・頭のバイト列が合わず積まなかった {}・捨てた {}・読めなかった {}。鳴ったカットイン（生きた tick ＋ 遅れて積み直した）0xE {} / スペルカード 0x35 {}。被弾音（id 4・0x30。rollback_hit_se = {}）: 確定まで待った {}・うち確定の回で見つかった {}・見つからず鳴らさなかった（確定では起きなかった。走り終わりの未確定の分を含む） {}・遅れて積み直した {}{}",
        s.live_ids,
        s.live_cutin,
        s.live_spell,
        s.resim_ids,
        s.new_ids,
        s.new_cutin,
        s.new_spell,
        s.suppressed,
        s.untracked,
        s.replayed,
        s.replayed_cutin,
        s.replayed_spell,
        mean,
        s.late_max,
        s.refused,
        s.dropped,
        s.unreadable,
        s.live_cutin.saturating_add(s.replayed_cutin),
        s.live_spell.saturating_add(s.replayed_spell),
        s.hit_mode.name(),
        s.hit_waited,
        s.hit_matched,
        s.hit_waited.saturating_sub(s.hit_matched),
        s.replayed_hit,
        live_phantom
    )
}

/// 走査の戻り値の分け方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanRet {
    Normal,
    /// 1（ノードが 3 を返して抜けた）。一時停止の間は GameManager（優先度 2）が 3 を返すので、止まっている tick もここ。入力の地点（優先度 0）は走っているので普通の tick として進め直せる。
    Stopped,
    /// 0 か負の値（ノードの 4 / 5。do_frame は `0x430230` を呼んで描画へ進まずに抜ける）。異常として扱い、その tick は予測に使わない。
    Abnormal,
}

pub fn scan_ret_class(ret: i32) -> ScanRet {
    match ret {
        r if r >= 2 => ScanRet::Normal,
        1 => ScanRet::Stopped,
        _ => ScanRet::Abnormal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResimRec {
    pub frame: bool,
    pub calls: u32,
    pub total_us: u32,
    pub max_us: u32,
    pub first_us: u32,
    pub abort: Option<ScanAbort>,
    pub scene_req: u32,
    pub outputs_restored: Option<bool>,
}

pub const MAX_SAMPLES: usize = 4096;

/// 標本の分位（小さい方から数えて `p` 分の 1 の位置。標本が 0 個なら `None`）。`p` は 0〜100。順位は `ceil(p / 100 × n)` 番目（p50 の偶数個は下側の中央）。
pub fn percentile(sorted: &[u32], p: u32) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let rank = (n * p as usize).div_ceil(100).clamp(1, n);
    sorted.get(rank - 1).copied()
}

fn us_ms(us: u32) -> String {
    format!("{:.2}", f64::from(us) / 1000.0)
}

pub fn trial_line(r: &ResimRec) -> Option<String> {
    if !r.frame {
        return None;
    }
    let abort = match r.abort {
        None => "打ち切りなし".to_string(),
        Some(a) => format!("打ち切り（{}）", abort_text(a)),
    };
    let out = match r.outputs_restored {
        None => "出力は戻していない",
        Some(true) => "SE・BGM・文字の数を最後の回の前に戻した",
        Some(false) => "出力を戻せなかった",
    };
    Some(format!(
        "巻き戻し:   フレームの中の進め直し: 追加の走査 {} 回（計 {} ms・最大 {} ms）、最初の回 {} ms（書き戻しを含む）。{abort}。{out}。進め直しの後で場面の要求が立っていた回 {}",
        r.calls,
        us_ms(r.total_us),
        us_ms(r.max_us),
        us_ms(r.first_us),
        r.scene_req
    ))
}

fn abort_text(a: ScanAbort) -> &'static str {
    match a {
        ScanAbort::One => "走査が 1 を返した = ノードが 3 を返した",
        ScanAbort::Zero => "走査が 0 を返した",
        ScanAbort::Negative => "走査が負の値を返した",
    }
}

/// 終わりの集計（本ログ）。`samples` は追加の回 1 回ずつの時間（マイクロ秒）、`dropped` は置き場が足りずに落とした数。フレームの形の回が 1 つも無ければ空。
pub fn summary_lines(rs: &[TrialRecord], samples: &[u32], dropped: u32) -> Vec<String> {
    let mut v = Vec::new();
    let frames: Vec<&TrialRecord> = rs.iter().filter(|r| r.resim.frame).collect();
    if frames.is_empty() {
        return v;
    }
    let count = |f: &dyn Fn(&TrialRecord) -> bool| frames.iter().filter(|r| f(r)).count();
    let done = count(&|r| r.n > 0);
    v.push(format!(
        "巻き戻し（フレームの形）: 記録 {} 件のうち、書き戻しまで進んだ回 {}（一致 {} / 違い {}）。追加の走査を呼んだ回 {}（k = 1 は追加が 0 回）",
        frames.len(),
        done,
        count(&|r| r.outcome == Outcome::Match),
        count(&|r| r.outcome == Outcome::Mismatch),
        count(&|r| r.resim.calls > 0)
    ));
    let aborted = |a: ScanAbort| count(&|r| r.resim.abort == Some(a));
    v.push(format!(
        "巻き戻し（フレームの形）: 打ち切り {} 回（走査が 1 を返した {} / 0 を返した {} / 負の値 {}）、最後の回の前に出力を戻せた回 {} / 戻せなかった回 {}、進め直しの後で場面の要求が立っていた回 {}（延べ {}）",
        count(&|r| r.resim.abort.is_some()),
        aborted(ScanAbort::One),
        aborted(ScanAbort::Zero),
        aborted(ScanAbort::Negative),
        count(&|r| r.resim.outputs_restored == Some(true)),
        count(&|r| r.resim.outputs_restored == Some(false)),
        count(&|r| r.resim.scene_req > 0),
        frames.iter().map(|r| u64::from(r.resim.scene_req)).sum::<u64>()
    ));
    let mut s = samples.to_vec();
    s.sort_unstable();
    match (percentile(&s, 50), percentile(&s, 99), s.last()) {
        (Some(p50), Some(p99), Some(&max)) => v.push(format!(
            "巻き戻し（フレームの形）: 追加の走査 1 回ごとの時間（n = {}、落とした標本 {dropped}）: p50 {} ms / p99 {} ms / 最大 {} ms（QPC）",
            s.len(),
            us_ms(p50),
            us_ms(p99),
            us_ms(max)
        )),
        _ => v.push(format!(
            "巻き戻し（フレームの形）: 追加の走査の時間の標本が 0 個（n = 0。k = 1 だけか、書き戻しまで進めなかった。落とした標本 {dropped}）"
        )),
    }
    let firsts: Vec<u32> = frames
        .iter()
        .filter(|r| r.n > 0)
        .map(|r| r.resim.first_us)
        .collect();
    if let Some(max) = firsts.iter().max() {
        v.push(format!(
            "巻き戻し（フレームの形）: 最初の回（書き戻しと 1 個目の入力ワードを含む）の時間（n = {}）: 平均 {} ms / 最大 {} ms",
            firsts.len(),
            us_ms((firsts.iter().map(|&x| u64::from(x)).sum::<u64>() / firsts.len() as u64) as u32),
            us_ms(*max)
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeScan {
        rets: Vec<i32>,
        owed: Option<u32>,
        ev: Vec<Ev>,
        calls: u32,
        taken: u32,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Ev {
        Call(u32),
        Owed,
        BeforeLast,
        After(u32, i32),
        Aborted(u32, ScanAbort),
    }

    impl FakeScan {
        fn new(owed: Option<u32>, rets: &[i32]) -> Self {
            FakeScan {
                rets: rets.to_vec(),
                owed,
                ev: Vec::new(),
                calls: 0,
                taken: 0,
            }
        }
    }

    impl ScanIo for FakeScan {
        fn call_scan(&mut self) -> i32 {
            let i = self.calls;
            self.calls += 1;
            self.ev.push(Ev::Call(i));
            *self.rets.get(i as usize).or(self.rets.last()).unwrap_or(&7)
        }
        fn take_owed(&mut self) -> Option<u32> {
            self.ev.push(Ev::Owed);
            self.taken += 1;
            self.owed
        }
        fn before_last_call(&mut self) {
            self.ev.push(Ev::BeforeLast);
        }
        fn after_call(&mut self, index: u32, ret: i32) {
            self.ev.push(Ev::After(index, ret));
        }
        fn aborted(&mut self, index: u32, why: ScanAbort) {
            self.ev.push(Ev::Aborted(index, why));
        }
    }

    #[test]
    fn a_normal_frame_calls_the_scan_once_and_returns_its_value() {
        for r in [0, 1, -1, 2, 40, i32::MAX] {
            let mut io = FakeScan::new(None, &[r]);
            assert_eq!(run_scan(&mut io), r);
            assert_eq!(io.calls, 1, "ret {r}");
            assert_eq!(io.ev, vec![Ev::Call(0), Ev::Owed], "ret {r}");
        }
    }

    #[test]
    fn a_resim_frame_without_extra_calls_only_records_the_first() {
        let mut io = FakeScan::new(Some(0), &[9, 9]);
        assert_eq!(run_scan(&mut io), 9);
        assert_eq!(io.ev, vec![Ev::Call(0), Ev::Owed, Ev::After(0, 9)]);
        let mut io = FakeScan::new(Some(0), &[1, 9]);
        assert_eq!(run_scan(&mut io), 1);
        assert_eq!(
            io.ev,
            vec![
                Ev::Call(0),
                Ev::Owed,
                Ev::After(0, 1),
                Ev::Aborted(0, ScanAbort::One)
            ]
        );
    }

    #[test]
    fn a_resim_frame_calls_n_plus_one_times_and_restores_before_the_last() {
        for owed in [1u32, 2, 7, 59] {
            let rets: Vec<i32> = (0..=owed as i32).map(|i| 10 + i).collect();
            let mut io = FakeScan::new(Some(owed), &rets);
            let got = run_scan(&mut io);
            assert_eq!(got, 10 + owed as i32, "最後の回の戻り値");
            assert_eq!(io.calls, owed + 1, "owed {owed}");
            let mut want = vec![Ev::Call(0), Ev::Owed, Ev::After(0, 10)];
            for i in 1..=owed {
                if i == owed {
                    want.push(Ev::BeforeLast);
                }
                want.push(Ev::Call(i));
                want.push(Ev::After(i, 10 + i as i32));
            }
            assert_eq!(io.ev, want, "owed {owed}");
            assert_eq!(
                io.ev.iter().filter(|e| **e == Ev::BeforeLast).count(),
                1,
                "戻すのは 1 回だけ"
            );
        }
    }

    #[test]
    fn an_abort_value_stops_the_loop_and_is_returned_as_it_is() {
        for (bad, why) in [
            (1, ScanAbort::One),
            (0, ScanAbort::Zero),
            (-1, ScanAbort::Negative),
        ] {
            let mut io = FakeScan::new(Some(5), &[9, bad, 9, 9, 9, 9]);
            assert_eq!(run_scan(&mut io), bad);
            assert_eq!(io.calls, 2, "{bad}: 打ち切った後も呼んだ");
            assert_eq!(
                io.ev,
                vec![
                    Ev::Call(0),
                    Ev::Owed,
                    Ev::After(0, 9),
                    Ev::Call(1),
                    Ev::After(1, bad),
                    Ev::Aborted(1, why)
                ],
                "{bad}"
            );
            let mut io = FakeScan::new(Some(3), &[bad, 9]);
            assert_eq!(run_scan(&mut io), bad);
            assert_eq!(io.calls, 1);
            assert_eq!(io.ev.last(), Some(&Ev::Aborted(0, why)));
            let mut io = FakeScan::new(Some(2), &[9, 9, bad]);
            assert_eq!(run_scan(&mut io), bad);
            assert_eq!(io.calls, 3);
            assert!(io.ev.contains(&Ev::BeforeLast));
            assert_eq!(io.ev.last(), Some(&Ev::Aborted(2, why)));
        }
        let mut io = FakeScan::new(Some(2), &[2, 2, 2]);
        assert_eq!(run_scan(&mut io), 2);
        assert_eq!(io.calls, 3);
        assert!(!io.ev.iter().any(|e| matches!(e, Ev::Aborted(..))));
    }

    #[test]
    fn abort_values_are_classified() {
        assert_eq!(abort_of(1), Some(ScanAbort::One));
        assert_eq!(abort_of(0), Some(ScanAbort::Zero));
        assert_eq!(abort_of(-1), Some(ScanAbort::Negative));
        assert_eq!(abort_of(i32::MIN), Some(ScanAbort::Negative));
        assert_eq!(abort_of(2), None);
        assert_eq!(abort_of(i32::MAX), None);
    }

    struct FakeMem {
        w: std::collections::HashMap<u32, u32>,
        deny_write: Option<u32>,
        deny_read: Option<u32>,
        writes: u32,
    }

    impl FakeMem {
        fn new() -> Self {
            FakeMem {
                w: std::collections::HashMap::new(),
                deny_write: None,
                deny_read: None,
                writes: 0,
            }
        }
        fn get(&self, a: u32) -> u32 {
            self.w.get(&a).copied().unwrap_or(0)
        }
    }

    impl OutputMem for FakeMem {
        fn read_u32(&mut self, addr: u32) -> Option<u32> {
            assert!(output_addr_allowed(addr), "範囲の外を読んだ: {addr:08X}");
            (self.deny_read != Some(addr)).then(|| self.get(addr))
        }
        fn write_u32(&mut self, addr: u32, v: u32) -> bool {
            assert!(output_addr_allowed(addr), "範囲の外へ書いた: {addr:08X}");
            self.writes += 1;
            if self.deny_write == Some(addr) {
                return false;
            }
            self.w.insert(addr, v);
            true
        }
    }

    fn before() -> FakeMem {
        let mut m = FakeMem::new();
        m.w.insert(ASCII_COUNT, 3);
        let bgm = SOUND_MANAGER + SND_BGM_QUEUE;
        m.w.insert(bgm, 0x11);
        m.w.insert(bgm + 8, 0x22);
        m.w.insert(bgm + SND_BGM_SLOT_BYTES, 0x33);
        m.w.insert(bgm + SND_BGM_SLOT_BYTES * SND_BGM_SLOTS - 4, 0x44);
        for i in 0..SND_SE_SLOTS {
            m.w.insert(SOUND_MANAGER + SND_SE_IDS + i * 4, u32::MAX);
        }
        m.w.insert(SOUND_MANAGER + SND_SE_IDS, 5);
        m.w.insert(SOUND_MANAGER + SND_SE_COUNTS, 1);
        m
    }

    fn dirty(m: &mut FakeMem) {
        for i in 0..SND_SE_SLOTS {
            m.w.insert(SOUND_MANAGER + SND_SE_IDS + i * 4, 3 + i);
            m.w.insert(SOUND_MANAGER + SND_SE_COUNTS + i * 4, 2);
        }
        let bgm = SOUND_MANAGER + SND_BGM_QUEUE;
        m.w.insert(bgm, 0x99);
        m.w.insert(bgm + SND_BGM_SLOT_BYTES * SND_BGM_SLOTS - 4, 0x98);
        m.w.insert(ASCII_COUNT, 40);
    }

    #[test]
    fn outputs_are_restored_from_the_copy_and_se_is_cleared() {
        let mut m = before();
        let mut saved = SavedOutputs::EMPTY;
        assert!(capture_outputs(&mut m, &mut saved));
        assert!(saved.valid);
        assert_eq!(saved.ascii_count, 3);
        dirty(&mut m);
        let pan = SOUND_MANAGER + 0x680;
        m.w.insert(pan, 0x77);
        m.w.insert(SOUND_MANAGER + 0x408, 0x66);
        m.writes = 0;
        assert!(apply_outputs(&mut m, &saved));
        for i in 0..SND_SE_SLOTS {
            assert_eq!(
                m.get(SOUND_MANAGER + SND_SE_IDS + i * 4),
                u32::MAX,
                "id {i}"
            );
            assert_eq!(m.get(SOUND_MANAGER + SND_SE_COUNTS + i * 4), 0, "回数 {i}");
        }
        let bgm = SOUND_MANAGER + SND_BGM_QUEUE;
        assert_eq!(m.get(bgm), 0x11);
        assert_eq!(m.get(bgm + 8), 0x22);
        assert_eq!(m.get(bgm + SND_BGM_SLOT_BYTES), 0x33);
        assert_eq!(m.get(bgm + SND_BGM_SLOT_BYTES * SND_BGM_SLOTS - 4), 0x44);
        assert_eq!(m.get(ASCII_COUNT), 3);
        assert_eq!(m.get(pan), 0x77, "パンの欄は触らない");
        assert_eq!(m.get(SOUND_MANAGER + 0x408), 0x66, "+0x408 は触らない");
        assert_eq!(m.writes, SND_SE_SLOTS * 2 + BGM_WORDS as u32 + 1);
        assert_eq!(BGM_WORDS, 2077);
    }

    #[test]
    fn full_outputs_return_every_field_to_the_value_before() {
        let mut m = before();
        let mut saved = FullOutputs::EMPTY;
        assert!(capture_full(&mut m, &mut saved));
        let want: Vec<u32> = (0..SND_SE_SLOTS)
            .flat_map(|i| {
                [
                    m.get(SOUND_MANAGER + SND_SE_IDS + i * 4),
                    m.get(SOUND_MANAGER + SND_SE_COUNTS + i * 4),
                ]
            })
            .collect();
        dirty(&mut m);
        let pan = SOUND_MANAGER + 0x680;
        m.w.insert(pan, 0x77);
        m.w.insert(SOUND_MANAGER + 0x408, 0x66);
        m.writes = 0;
        assert!(restore_full(&mut m, &saved));
        let got: Vec<u32> = (0..SND_SE_SLOTS)
            .flat_map(|i| {
                [
                    m.get(SOUND_MANAGER + SND_SE_IDS + i * 4),
                    m.get(SOUND_MANAGER + SND_SE_COUNTS + i * 4),
                ]
            })
            .collect();
        assert_eq!(got, want, "SE の id と回数");
        assert_eq!(
            m.get(SOUND_MANAGER + SND_SE_IDS),
            5,
            "進め直しの前に積まれていた SE"
        );
        let bgm = SOUND_MANAGER + SND_BGM_QUEUE;
        assert_eq!(m.get(bgm), 0x11);
        assert_eq!(m.get(bgm + 8), 0x22);
        assert_eq!(m.get(bgm + SND_BGM_SLOT_BYTES * SND_BGM_SLOTS - 4), 0x44);
        assert_eq!(m.get(ASCII_COUNT), 3);
        assert_eq!((m.get(pan), m.get(SOUND_MANAGER + 0x408)), (0x77, 0x66));
        assert_eq!(m.writes, SND_SE_SLOTS * 2 + BGM_WORDS as u32 + 1);
        let mut m = before();
        m.deny_read = Some(SOUND_MANAGER + SND_SE_COUNTS + 11 * 4);
        let mut s = FullOutputs::EMPTY;
        assert!(!capture_full(&mut m, &mut s));
        assert!(!restore_full(&mut m, &s));
        assert_eq!(m.writes, 0);
    }

    #[test]
    fn scan_returns_are_classified_the_m8_way() {
        assert_eq!(scan_ret_class(2), ScanRet::Normal);
        assert_eq!(scan_ret_class(40), ScanRet::Normal);
        assert_eq!(scan_ret_class(i32::MAX), ScanRet::Normal);
        assert_eq!(scan_ret_class(1), ScanRet::Stopped);
        assert_eq!(scan_ret_class(0), ScanRet::Abnormal);
        assert_eq!(scan_ret_class(-1), ScanRet::Abnormal);
        assert_eq!(scan_ret_class(i32::MIN), ScanRet::Abnormal);
    }

    #[test]
    fn a_failed_capture_writes_nothing_and_a_failed_write_is_reported() {
        let mut m = before();
        let mut saved = SavedOutputs::EMPTY;
        assert!(!apply_outputs(&mut m, &saved));
        assert_eq!(m.writes, 0);
        for bad in [
            SOUND_MANAGER + SND_BGM_QUEUE + 100 * 4,
            SOUND_MANAGER + SND_BGM_QUEUE,
            ASCII_COUNT,
        ] {
            let mut m = before();
            m.deny_read = Some(bad);
            let mut s = SavedOutputs::EMPTY;
            s.valid = true;
            assert!(!capture_outputs(&mut m, &mut s), "{bad:08X}");
            assert!(!s.valid, "読めなかったのに有効: {bad:08X}");
            assert!(!apply_outputs(&mut m, &s));
            assert_eq!(m.writes, 0);
        }
        assert!(capture_outputs(&mut m, &mut saved));
        for bad in [
            SOUND_MANAGER + SND_SE_IDS + 4,
            SOUND_MANAGER + SND_BGM_QUEUE + 4 * 2000,
            ASCII_COUNT,
        ] {
            let mut m2 = before();
            m2.deny_write = Some(bad);
            assert!(!apply_outputs(&mut m2, &saved), "{bad:08X}");
        }
    }

    #[test]
    fn only_the_named_ranges_may_be_touched() {
        for (s, e) in OUTPUT_RANGES {
            assert!(output_addr_allowed(s), "{s:08X}");
            assert!(output_addr_allowed(e - 4), "{e:08X}");
            assert!(!output_addr_allowed(e), "{e:08X} の 1 語先");
            assert!(!output_addr_allowed(s - 4), "{s:08X} の 1 語前");
            assert!(!output_addr_allowed(s + 1), "境界に揃わない");
        }
        assert!(!output_addr_allowed(SOUND_MANAGER + 0x680), "パンの欄");
        assert!(!output_addr_allowed(SOUND_MANAGER + 0x408), "+0x408");
        assert!(!output_addr_allowed(u32::MAX - 3), "32 ビットの端");
        let words: Vec<u32> = OUTPUT_RANGES.iter().map(|&(s, e)| (e - s) / 4).collect();
        assert_eq!(words, [24, 2077, 1]);
    }

    /// 「最後の回の前に戻す」が、最後の回の積んだ物を消さず、途中の回の積んだ物だけを消すことを見る。
    struct MemScan<'a> {
        mem: &'a mut FakeMem,
        saved: SavedOutputs,
        owed: u32,
        calls: u32,
    }

    impl ScanIo for MemScan<'_> {
        fn call_scan(&mut self) -> i32 {
            let i = self.calls;
            self.calls += 1;
            self.mem
                .w
                .insert(SOUND_MANAGER + SND_SE_IDS + i * 4, 100 + i);
            self.mem.w.insert(SOUND_MANAGER + SND_SE_COUNTS + i * 4, 1);
            let n = self.mem.get(ASCII_COUNT);
            self.mem.w.insert(ASCII_COUNT, n + 5);
            9
        }
        fn take_owed(&mut self) -> Option<u32> {
            Some(self.owed)
        }
        fn before_last_call(&mut self) {
            assert!(apply_outputs(self.mem, &self.saved));
        }
        fn after_call(&mut self, _: u32, _: i32) {}
        fn aborted(&mut self, _: u32, _: ScanAbort) {}
    }

    #[test]
    fn only_the_last_call_keeps_its_se_and_text() {
        for owed in [1u32, 2, 4] {
            let mut m = before();
            let mut saved = SavedOutputs::EMPTY;
            assert!(capture_outputs(&mut m, &mut saved));
            let mut io = MemScan {
                mem: &mut m,
                saved,
                owed,
                calls: 0,
            };
            assert_eq!(run_scan(&mut io), 9);
            let last = owed; // 最後の回の番号（0 始まり）
            for i in 0..SND_SE_SLOTS.min(owed + 1) {
                let id = m.get(SOUND_MANAGER + SND_SE_IDS + i * 4);
                let cnt = m.get(SOUND_MANAGER + SND_SE_COUNTS + i * 4);
                if i == last {
                    assert_eq!(
                        (id, cnt),
                        (100 + i, 1),
                        "最後の回の SE が消えた（owed {owed}）"
                    );
                } else {
                    assert_eq!(
                        (id, cnt),
                        (u32::MAX, 0),
                        "枠 {i} に途中の回の SE が残る（owed {owed}）"
                    );
                }
            }
            assert_eq!(m.get(ASCII_COUNT), 3 + 5, "owed {owed}");
            assert_eq!(m.get(SOUND_MANAGER + SND_BGM_QUEUE), 0x11);
        }
    }

    #[test]
    fn percentile_uses_the_nearest_rank() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[7], 50), Some(7));
        assert_eq!(percentile(&[7], 99), Some(7));
        let v: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&v, 50), Some(50));
        assert_eq!(percentile(&v, 99), Some(99));
        assert_eq!(percentile(&v, 100), Some(100));
        assert_eq!(percentile(&v, 0), Some(1));
        let w = [10, 20, 30, 40];
        assert_eq!(percentile(&w, 50), Some(20));
        assert_eq!(percentile(&w, 75), Some(30));
        assert_eq!(percentile(&w, 99), Some(40));
    }

    #[test]
    fn summary_reports_counts_with_n_and_stays_silent_for_realtime() {
        let mut a = TrialRecord::EMPTY;
        a.n = 32;
        a.outcome = Outcome::Match;
        a.resim = ResimRec {
            frame: true,
            calls: 7,
            total_us: 2100,
            max_us: 400,
            first_us: 1800,
            abort: None,
            scene_req: 0,
            outputs_restored: Some(true),
        };
        let mut b = a;
        b.outcome = Outcome::Mismatch;
        b.resim.abort = Some(ScanAbort::One);
        b.resim.scene_req = 2;
        b.resim.outputs_restored = None;
        let mut c = TrialRecord::EMPTY;
        c.resim.frame = true; // 捨てた回（書き戻していない）
        let realtime = TrialRecord::EMPTY;
        assert!(summary_lines(&[realtime, realtime], &[1, 2], 0).is_empty());
        let v = summary_lines(&[a, b, c, realtime], &[100, 200, 300, 400, 5000], 3);
        let text = v.join("\n");
        assert!(
            text.contains("記録 3 件のうち、書き戻しまで進んだ回 2（一致 1 / 違い 1）"),
            "{text}"
        );
        assert!(
            text.contains("打ち切り 1 回（走査が 1 を返した 1 / 0 を返した 0 / 負の値 0）"),
            "{text}"
        );
        assert!(text.contains("戻せた回 1 / 戻せなかった回 0"), "{text}");
        assert!(
            text.contains("場面の要求が立っていた回 1（延べ 2）"),
            "{text}"
        );
        assert!(
            text.contains("n = 5、落とした標本 3）: p50 0.30 ms / p99 5.00 ms / 最大 5.00 ms"),
            "{text}"
        );
        let none = summary_lines(&[a], &[], 0).join("\n");
        assert!(none.contains("標本が 0 個（n = 0"), "{none}");
    }

    #[test]
    fn a_trial_line_only_for_the_frame_form() {
        assert_eq!(trial_line(&ResimRec::default()), None);
        let mut r = ResimRec {
            frame: true,
            calls: 59,
            total_us: 30_000,
            max_us: 900,
            first_us: 2500,
            abort: None,
            scene_req: 0,
            outputs_restored: Some(true),
        };
        let l = trial_line(&r).unwrap();
        assert!(
            l.contains("追加の走査 59 回（計 30.00 ms・最大 0.90 ms）"),
            "{l}"
        );
        assert!(
            l.contains("打ち切りなし") && l.contains("最後の回の前に戻した"),
            "{l}"
        );
        r.abort = Some(ScanAbort::Zero);
        r.outputs_restored = None;
        let l = trial_line(&r).unwrap();
        assert!(
            l.contains("打ち切り（走査が 0 を返した）") && l.contains("出力は戻していない"),
            "{l}"
        );
    }

    #[test]
    fn resim_values_are_read_strictly() {
        assert_eq!(ResimMode::parse("realtime"), Ok(ResimMode::Realtime));
        assert_eq!(ResimMode::parse("frame"), Ok(ResimMode::Frame));
        for bad in ["", "Frame", "FRAME", "real", "1", "frame "] {
            assert!(ResimMode::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(ResimMode::default(), ResimMode::Realtime);
        assert_eq!(ResimMode::Frame.name(), "frame");
    }

    struct SeMem(std::collections::HashMap<u32, u32>);

    impl SeMem {
        fn empty() -> Self {
            let mut m = SeMem(std::collections::HashMap::new());
            for i in 0..SND_SE_SLOTS {
                m.0.insert(SOUND_MANAGER + SND_SE_IDS + i * 4, u32::MAX);
                m.0.insert(SOUND_MANAGER + SND_SE_COUNTS + i * 4, 0);
            }
            m
        }
        fn put(&mut self, slot: u32, id: u32, pans: &[i32]) {
            self.0.insert(SOUND_MANAGER + SND_SE_IDS + slot * 4, id);
            self.0
                .insert(SOUND_MANAGER + SND_SE_COUNTS + slot * 4, pans.len() as u32);
            for (k, p) in pans.iter().enumerate() {
                self.0.insert(
                    SOUND_MANAGER + SND_SE_PANS + slot * SND_SE_PAN_SLOT_BYTES + k as u32 * 4,
                    *p as u32,
                );
            }
        }
    }

    impl OutputMem for SeMem {
        fn read_u32(&mut self, addr: u32) -> Option<u32> {
            assert!(output_addr_readable(addr), "読んでよい範囲の外: {addr:08X}");
            Some(self.0.get(&addr).copied().unwrap_or(0))
        }
        fn write_u32(&mut self, addr: u32, v: u32) -> bool {
            assert!(output_addr_allowed(addr), "書いてよい範囲の外: {addr:08X}");
            self.0.insert(addr, v);
            true
        }
    }

    fn q_of(ids: &[u32]) -> SeQueue {
        let mut q = SeQueue::EMPTY;
        for (i, &id) in ids.iter().enumerate() {
            q.ev[i] = SeEvent { id, pan: 0 };
            q.count[i] = 1;
            q.n += 1;
        }
        q
    }

    #[test]
    fn the_se_queue_is_read_like_the_game_plays_it_and_cleared() {
        let mut m = SeMem::empty();
        m.put(0, 0x0E, &[-10, 30, 4]); // 平均 8
        m.put(1, 0x35, &[7]);
        m.put(3, 0x22, &[1]); // 枠 2 が空きなので読まない
        let q = read_se_queue(&mut m);
        assert!(q.ok);
        assert_eq!(q.n, 2);
        let got: Vec<(u32, i32, u32)> = q.events().map(|(e, c)| (e.id, e.pan, c)).collect();
        assert_eq!(got, vec![(0x0E, 8, 3), (0x35, 7, 1)]);
        let mut m2 = SeMem::empty();
        m2.put(0, 5, &[-4, -3]);
        assert_eq!(read_se_queue(&mut m2).ev[0].pan, -3);
        let mut m3 = SeMem::empty();
        m3.put(0, 9, &[]);
        let q3 = read_se_queue(&mut m3);
        assert_eq!((q3.n, q3.ev[0].pan), (1, 0));
        m.0.insert(SOUND_MANAGER + 0x408, 0x66);
        let pan0 = SOUND_MANAGER + SND_SE_PANS;
        assert!(clear_se_queue(&mut m));
        assert_eq!(read_se_queue(&mut m).n, 0);
        for i in 0..SND_SE_SLOTS {
            assert_eq!(m.0[&(SOUND_MANAGER + SND_SE_IDS + i * 4)], u32::MAX);
            assert_eq!(m.0[&(SOUND_MANAGER + SND_SE_COUNTS + i * 4)], 0);
        }
        assert_eq!(m.0[&(SOUND_MANAGER + 0x408)], 0x66);
        assert_eq!(m.0[&pan0] as i32, -10, "パンの欄は触らない");
    }

    #[test]
    fn the_pan_area_is_readable_but_not_writable() {
        let (s, e) = OUTPUT_READ_RANGES[0];
        assert_eq!(s, SOUND_MANAGER + 0x680);
        assert_eq!(e - s, 12 * 0x200);
        assert!(output_addr_readable(s) && output_addr_readable(e - 4));
        assert!(!output_addr_readable(e), "終わりの 1 語先");
        assert!(!output_addr_readable(s + 1), "境界に揃わない");
        assert!(!output_addr_readable(SOUND_MANAGER + 0x408), "+0x408");
        assert!(
            !output_addr_allowed(s) && !output_addr_allowed(e - 4),
            "書けてはいけない"
        );
        for (a, _) in OUTPUT_RANGES {
            assert!(output_addr_readable(a));
        }
        assert_eq!(SOUND_MANAGER + SND_SE_COUNTS + SND_SE_SLOTS * 4, s);
    }

    #[test]
    fn the_played_ring_matches_a_two_frame_window_and_forgets_stale_frames() {
        let mut p = SePlayed::EMPTY;
        assert!(p.add(10, 0x35));
        for f in 8..=12 {
            assert!(p.has_near(f, 0x35, SE_NEAR), "{f}");
        }
        for f in [7, 13, 0, 100] {
            assert!(!p.has_near(f, 0x35, SE_NEAR), "{f}");
        }
        assert!(!p.has_near(10, 0x36, SE_NEAR), "別の id");
        assert!(p.has_near(10, 0x35, 0) && !p.has_near(11, 0x35, 0));
        assert!(p.add(42, 0x01));
        assert!(!p.has_near(10, 0x35, 0), "古い記録が残っている");
        assert!(p.has_near(42, 0x01, 0));
        assert!(!p.has_near(10, 0x01, 0));
        let mut p = SePlayed::EMPTY;
        p.add(0, 7);
        assert!(
            p.has_near(0, 7, SE_NEAR) && p.has_near(2, 7, SE_NEAR) && !p.has_near(3, 7, SE_NEAR)
        );
        assert!(!p.add(5, SE_ID_LIMIT));
        assert!(!p.has_near(5, SE_ID_LIMIT, SE_NEAR));
        assert!(p.add(5, SE_ID_LIMIT - 1) && p.has_near(5, SE_ID_LIMIT - 1, 0));
        p.clear();
        assert!(!p.has_near(0, 7, SE_NEAR));
    }

    #[test]
    fn sorting_a_resim_tick_adds_only_new_ids_and_counts_them_by_id() {
        let mut played = SePlayed::EMPTY;
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            20,
            &q_of(&[0x0E, 0x35, 0x10]),
            HitSe::Live,
            true,
        );
        assert_eq!(
            (s.resim_ids, s.new_ids, s.new_cutin, s.new_spell),
            (3, 3, 1, 1)
        );
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            21,
            &q_of(&[0x0E]),
            HitSe::Live,
            true,
        );
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            22,
            &q_of(&[0x0E]),
            HitSe::Live,
            true,
        );
        assert_eq!((s.suppressed, s.new_ids), (2, 3));
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            23,
            &q_of(&[0x0E]),
            HitSe::Live,
            true,
        );
        assert_eq!((s.suppressed, s.new_ids, s.new_cutin), (2, 4, 2));
        let mut order = Vec::new();
        flush_se(&mut pending, &mut s, &mut |e| {
            order.push(e.id);
            true
        });
        assert_eq!(order, vec![0x0E, 0x35, 0x10, 0x0E]);
        assert_eq!((s.replayed, s.replayed_cutin, s.replayed_spell), (4, 2, 1));
        assert_eq!(s.refused, 0);
        assert_eq!((s.late_max, s.late_sum), (4, 4 + 4 + 4 + 1));
        flush_se(&mut pending, &mut s, &mut |_| {
            panic!("空のはずの列から積んだ")
        });
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            90,
            &q_of(&[0x01]),
            HitSe::Live,
            true,
        );
        flush_se(&mut pending, &mut s, &mut |_| false);
        assert_eq!((s.refused, s.replayed), (1, 0));
        let mut s = SeStats::default();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            91,
            &q_of(&[SE_ID_LIMIT]),
            HitSe::Live,
            true,
        );
        assert_eq!((s.untracked, s.new_ids), (1, 0));
        assert!(pending.n == 0);
        let mut played = SePlayed::EMPTY;
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        for i in 0..40u32 {
            sort_resim_se(
                &mut played,
                &mut pending,
                &mut s,
                100 + i * 10,
                &q_of(&[0x0E]),
                HitSe::Live,
                true,
            );
        }
        assert_eq!((s.new_ids, s.dropped), (40, 8));
        drop_pending_se(&mut pending, &mut s);
        assert_eq!(s.dropped, 8 + SE_PENDING_CAP as u32);
        assert_eq!(pending.n, 0);
    }

    #[test]
    fn a_predicted_live_hit_the_full_pending_list_refuses_is_not_counted() {
        let mut played = SePlayed::EMPTY;
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        for i in 0..SE_PENDING_CAP as u32 {
            assert!(pending.push(SeEvent { id: 0x0E, pan: 0 }, 100 + i * 10));
        }
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            200,
            &q_of(&[SE_ID_HIT]),
            HitSe::Live,
            false,
        );
        assert_eq!(
            (s.dropped, s.hit_live_sounded, s.untracked),
            (1, 0, 0),
            "積めなかった被弾音を数えた: {s:?}"
        );
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            200,
            &q_of(&[SE_ID_HIT]),
            HitSe::Live,
            true,
        );
        assert_eq!(s.hit_live_matched, 0, "積めなかった分に対応が付いた: {s:?}");
        let mut played = SePlayed::EMPTY;
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            200,
            &q_of(&[SE_ID_HIT]),
            HitSe::Live,
            false,
        );
        assert_eq!((s.dropped, s.hit_live_sounded), (0, 1), "{s:?}");
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            200,
            &q_of(&[SE_ID_HIT]),
            HitSe::Live,
            true,
        );
        assert_eq!((s.hit_live_matched, s.hit_live_sounded), (1, 1), "{s:?}");
    }

    #[test]
    fn live_se_counts_only_what_the_scan_added() {
        let mut played = SePlayed::EMPTY;
        let mut s = SeStats::default();
        note_live_se(&mut played, &mut s, 30, &q_of(&[7]), &q_of(&[7]));
        assert_eq!(s.live_ids, 0);
        assert!(!played.has_near(30, 7, 0));
        let mut post = q_of(&[7, 9]);
        post.count[0] = 2;
        note_live_se(&mut played, &mut s, 31, &q_of(&[7]), &post);
        assert_eq!(s.live_ids, 2);
        assert!(played.has_near(31, 7, 0) && played.has_near(31, 9, 0));
        let mut pre = q_of(&[7]);
        pre.ok = false;
        note_live_se(&mut played, &mut s, 40, &pre, &q_of(&[7]));
        assert_eq!(s.live_ids, 3);
        let mut s2 = SeStats::default();
        let mut p2 = SePlayed::EMPTY;
        note_live_se(
            &mut p2,
            &mut s2,
            3,
            &SeQueue::EMPTY,
            &q_of(&[SE_ID_CUTIN, SE_ID_CUTIN_SPELL, 0x10]),
        );
        assert_eq!((s2.live_ids, s2.live_cutin, s2.live_spell), (3, 1, 1));
        let mut post = q_of(&[4]);
        post.ok = false;
        note_live_se(&mut played, &mut s, 50, &SeQueue::EMPTY, &post);
        assert_eq!((s.unreadable, s.live_ids), (1, 4));
    }

    #[test]
    fn the_code_head_check_refuses_a_single_different_byte() {
        use crate::th09_addr::{SE_PUSH_FN, SE_PUSH_HEAD};
        let mem_of = |code: &[u8]| {
            let code = code.to_vec();
            move |a: u32| -> Option<u32> {
                let i = a.checked_sub(SE_PUSH_FN)? as usize;
                (i < code.len()).then(|| {
                    let mut w = [0u8; 4];
                    for (k, b) in w.iter_mut().enumerate() {
                        *b = code.get(i + k).copied().unwrap_or(0xCC);
                    }
                    u32::from_le_bytes(w)
                })
            }
        };
        let mut ok = mem_of(&SE_PUSH_HEAD);
        assert!(code_head_matches(SE_PUSH_FN, &SE_PUSH_HEAD, &mut ok));
        for i in 0..SE_PUSH_HEAD.len() {
            let mut bad = SE_PUSH_HEAD;
            bad[i] ^= 0x01;
            let mut r = mem_of(&bad);
            assert!(
                !code_head_matches(SE_PUSH_FN, &SE_PUSH_HEAD, &mut r),
                "{i} バイト目が違うのに合うと言った"
            );
        }
        let mut short = mem_of(&SE_PUSH_HEAD[..8]);
        assert!(!code_head_matches(SE_PUSH_FN, &SE_PUSH_HEAD, &mut short));
        assert_eq!(SE_PUSH_HEAD.len(), 25);
        assert_eq!(SE_PUSH_HEAD[0], 0x55, "push %ebp");
    }

    #[test]
    fn the_se_summary_names_every_count() {
        let l = se_summary_line(&SeStats::default());
        assert!(
            l.contains("遅れ N − 出来事のフレーム 平均 — / 最大 0"),
            "{l}"
        );
        let s = SeStats {
            live_ids: 11,
            live_cutin: 8,
            live_spell: 3,
            resim_ids: 22,
            new_ids: 5,
            new_cutin: 2,
            new_spell: 1,
            suppressed: 15,
            replayed: 4,
            replayed_cutin: 2,
            replayed_spell: 1,
            late_sum: 12,
            late_max: 6,
            refused: 1,
            dropped: 3,
            unreadable: 7,
            untracked: 9,
            hit_mode: HitSe::Confirmed,
            hit_waited: 8,
            hit_matched: 5,
            replayed_hit: 6,
            hit_live_sounded: 3,
            hit_live_matched: 2,
        };
        let l = se_summary_line(&s);
        for want in [
            "生きた tick で積まれた 11（カットイン 0xE 8 / スペルカード 0x35 3）",
            "鳴ったカットイン（生きた tick ＋ 遅れて積み直した）0xE 10 / スペルカード 0x35 4",
            "進め直しの tick で積まれた 22",
            "初めて出た 5（カットイン 0xE 2 / スペルカード 0x35 1）",
            "抑えた 15",
            "追えない id 9",
            "遅れて積み直した 4（カットイン 0xE 2 / スペルカード 0x35 1。遅れ N − 出来事のフレーム 平均 3.00 / 最大 6）",
            "積まなかった 1",
            "捨てた 3",
            "読めなかった 7",
            "被弾音（id 4・0x30。rollback_hit_se = confirmed）: 確定まで待った 8・うち確定の回で見つかった 5・見つからず鳴らさなかった（確定では起きなかった。走り終わりの未確定の分を含む） 3・遅れて積み直した 6",
        ] {
            assert!(l.contains(want), "{want} が無い: {l}");
        }
        assert!(
            l.ends_with("遅れて積み直した 6"),
            "confirmed の行の末尾が変わった: {l}"
        );
        let live = SeStats {
            hit_mode: HitSe::Live,
            ..s
        };
        let l = se_summary_line(&live);
        assert!(
            l.ends_with("鳴らした 3・うち確定の回で見つかった 2・見つからなかった幻の被弾音（鳴ったのに確定では起きなかった。走り終わりの未確定の分を含む） 1"),
            "{l}"
        );
    }

    #[test]
    fn the_hit_se_ids_and_the_ini_spellings_are_exact() {
        for id in 0..SE_ID_LIMIT {
            assert_eq!(is_hit_se(id), id == 4 || id == 0x30, "{id:#X}");
        }
        assert_eq!(HitSe::default(), HitSe::Confirmed);
        assert_eq!(HitSe::parse("confirmed"), Ok(HitSe::Confirmed));
        assert_eq!(HitSe::parse("live"), Ok(HitSe::Live));
        for bad in ["", "Live", "CONFIRMED", "on", "0", "confirmed "] {
            assert!(HitSe::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(HitSe::Confirmed.name(), "confirmed");
        assert_eq!(HitSe::Live.name(), "live");
    }

    #[test]
    fn the_held_ring_takes_the_nearest_within_two_frames_and_is_separate_from_sounded() {
        let mut p = SePlayed::EMPTY;
        assert!(p.hold(10, 4) && p.hold(12, 4));
        assert!(!p.has_near(10, 4, 0), "待たせただけで鳴った記録に入った");
        assert!(!p.take_held_near(15, 4, SE_NEAR), "5 フレーム離れて消えた");
        assert!(!p.take_held_near(10, 0x30, SE_NEAR), "別の id が消えた");
        assert!(p.take_held_near(11, 4, SE_NEAR));
        assert!(p.take_held_near(11, 4, SE_NEAR));
        assert!(!p.take_held_near(11, 4, SE_NEAR), "3 つ目が消えた");
        for (f, ok) in [(8, true), (12, true), (7, false), (13, false)] {
            let mut p = SePlayed::EMPTY;
            p.hold(10, 4);
            assert_eq!(p.take_held_near(f, 4, SE_NEAR), ok, "{f}");
        }
        let mut p = SePlayed::EMPTY;
        p.hold(10, 4);
        p.hold(13, 4);
        assert!(p.take_held_near(12, 4, SE_NEAR));
        assert!(!p.take_held_near(13, 4, 0), "近い方が残った");
        assert!(p.take_held_near(10, 4, 0), "遠い方が消えた");
        let mut p = SePlayed::EMPTY;
        p.hold(10, 4);
        p.clear();
        assert!(!p.take_held_near(10, 4, SE_NEAR));
        assert!(!p.hold(1, SE_ID_LIMIT), "追えない id を足せた");
    }

    #[test]
    fn holding_hits_subtracts_only_what_the_scan_added() {
        let mut played = SePlayed::EMPTY;
        let mut s = SeStats::default();
        let mut pre = q_of(&[7, 4]);
        pre.ev[0].pan = -5;
        pre.ev[1].pan = 33;
        let mut post = q_of(&[7, 4, SE_ID_LAST_LIFE, SE_ID_CUTIN]);
        post.ev[0].pan = -5;
        post.ev[1].pan = 11;
        post.count[1] = 3;
        let a0 = crate::test_alloc::count();
        let rest = hold_hit_se(&mut played, &mut s, 50, &pre, &post).expect("抜く");
        assert_eq!(crate::test_alloc::count(), a0, "被弾音を抜く関数が確保した");
        let got: Vec<(u32, i32, u32)> = rest.events().map(|(e, c)| (e.id, e.pan, c)).collect();
        assert_eq!(
            got,
            vec![(7, -5, 1), (4, 33, 1), (SE_ID_CUTIN, 0, 1)],
            "前からあった分とほかの SE だけが残る"
        );
        assert_eq!(s.hit_waited, 3, "走査が足した 2 + 1");
        assert!(!played.has_near(50, 4, 0) && !played.has_near(50, 0x30, 0));
        assert!(played.take_held_near(50, 4, 0) && played.take_held_near(50, 0x30, 0));
        let mut fresh = SePlayed::EMPTY;
        for (pre, post) in [
            (q_of(&[7]), q_of(&[7, SE_ID_CUTIN])),
            (q_of(&[4]), q_of(&[4])),
            (SeQueue::EMPTY, SeQueue::EMPTY),
        ] {
            let mut s = SeStats::default();
            assert!(hold_hit_se(&mut fresh, &mut s, 1, &pre, &post).is_none());
            assert_eq!(s, SeStats::default());
        }
        let mut bad = q_of(&[4]);
        bad.ok = false;
        let mut s = SeStats::default();
        assert!(hold_hit_se(&mut fresh, &mut s, 1, &SeQueue::EMPTY, &bad).is_none());
        assert!(hold_hit_se(&mut fresh, &mut s, 1, &bad, &q_of(&[4])).is_none());
        assert_eq!(s.hit_waited, 0);
    }

    #[test]
    fn noting_a_live_hit_counts_only_what_the_scan_added() {
        let mut played = SePlayed::EMPTY;
        let mut s = SeStats::default();
        let pre = q_of(&[7, 4]);
        let post = q_of(&[7, 4, SE_ID_LAST_LIFE, SE_ID_CUTIN]);
        let a0 = crate::test_alloc::count();
        note_live_hit(&mut played, &mut s, 50, &pre, &post);
        assert_eq!(crate::test_alloc::count(), a0, "確保した");
        assert_eq!(s.hit_live_sounded, 1, "0x30 だけが走査の増やした被弾音");
        assert!(
            !played.has_near(50, SE_ID_LAST_LIFE, 0),
            "鳴った記録に足した"
        );
        assert!(played.take_held_near(50, SE_ID_LAST_LIFE, 0));
        assert!(!played.take_held_near(50, 4, 0), "前からあった 4 を覚えた");
        assert_eq!(s.hit_waited, 0, "confirmed 側の数に入った");
        let mut bad = q_of(&[4]);
        bad.ok = false;
        let mut s = SeStats::default();
        note_live_hit(&mut played, &mut s, 51, &SeQueue::EMPTY, &bad);
        note_live_hit(&mut played, &mut s, 51, &bad, &q_of(&[4]));
        assert_eq!(s, SeStats::default());
    }

    #[test]
    fn sorting_waits_only_for_hit_ids_in_predicted_resim_ticks() {
        let mut played = SePlayed::EMPTY;
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            20,
            &q_of(&[SE_ID_HIT, SE_ID_LAST_LIFE, SE_ID_CUTIN, 0x10]),
            HitSe::Confirmed,
            false,
        );
        assert_eq!((s.resim_ids, s.new_ids, s.suppressed), (2, 2, 0));
        assert_eq!(pending.n, 2);
        assert!(!played.has_near(20, SE_ID_HIT, SE_NEAR), "記録に足した");
        assert!(played.hold(22, SE_ID_HIT));
        let q = q_of(&[SE_ID_HIT]);
        let a0 = crate::test_alloc::count();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            20,
            &q,
            HitSe::Confirmed,
            true,
        );
        assert_eq!(crate::test_alloc::count(), a0, "仕分けが確保した");
        assert_eq!((s.hit_matched, s.new_ids, s.resim_ids), (1, 3, 3));
        assert_eq!(pending.n, 3);
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            21,
            &q_of(&[SE_ID_HIT]),
            HitSe::Confirmed,
            true,
        );
        assert_eq!((s.hit_matched, s.suppressed, s.new_ids), (1, 1, 3));
        let mut played = SePlayed::EMPTY;
        let mut pending = SePending::EMPTY;
        let mut s = SeStats::default();
        sort_resim_se(
            &mut played,
            &mut pending,
            &mut s,
            20,
            &q_of(&[SE_ID_HIT]),
            HitSe::Live,
            false,
        );
        assert_eq!((s.new_ids, s.hit_matched, pending.n), (1, 0, 1));
        let mut order = Vec::new();
        flush_se(&mut pending, &mut s, &mut |e| {
            order.push(e.id);
            true
        });
        assert_eq!((order, s.replayed, s.replayed_hit), (vec![SE_ID_HIT], 1, 1));
    }
}
