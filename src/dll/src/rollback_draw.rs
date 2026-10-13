//! 描画用の VM の欄の診断と除外（pure。unsafe 無し。呼ぶ側の Win32 は `rollback_win.rs`）。
//!
//! 背景: 1 回の描画の中で走査を k 回呼ぶ形で k = 8・60 の多くが違い、違ったバイトは描画の列が書く「描画用の VM の欄」
//! （ANM の VM の回転 z と画面座標 pos）だった。期間の途中で消えた物の枠には、実時間の形では消える直前の描画の値が、この形では一度も描かれていない値が残る。決定的な状態は動いていない見込み（推定）。
//!
//! 1. 診断: 違ったバイトを、範囲（弾・敵・自機ショット・arena）・枠・枠の中の欄・その枠が D1 と D2 で生きているか、に分けて数える。
//!    数えるだけで比べの結果は変えない。数える対象は「比べる所」だけ（表の `region_skips` が外した所は数えない）なので、
//!    `skip_draw_fields = none` のとき、行の違いのバイト数の合計は `RegionResult::diff.bytes` と一致する。
//! 2. 除外（ini `skip_draw_fields = dead`）: D1 の時点で生きていない枠の描画用の欄（VM ごとの回転 z の 4 バイトと pos の 12 バイト）を
//!    比べから外す。生きている枠の欄は外さない（本物のずれを隠さないため）。arena は、D1 の割り当て器を歩けたときだけ、
//!    D1 で空きの VM 大の塊（求められた大きさが VM の倍数）の同じ欄を外す（歩けなければ外さず、そのことを書く）。
//! 3. 全部の除外（`skip_draw_fields = all`）: 生死に関係なく、枠の配列の VM（診断だけの候補 VM の敵 `+0x2AC` も含む）の回転 z と pos、
//!    arena の使用中の VM の配列（札 `0x0040CB17` / `0x00440DB9`）の同じ欄を外す。arena の空きの塊は外さない（解放で中身が 0 になる
//!    ので D1 と D2 で同じはず）。使用中の VM の配列の描画欄の外（更新が読む欄）と、世界の欄（弾 `+0xD4C` / `+0xDBE` など）は外さない。
//! 4. レーザー: 残りの 28 B は、弾の塊の中のレーザー（`+0x24D424`・0x59C 刻み・48 本）の 0 本目の、描画だけが書く欄だった。
//!    レーザーを枠の配列として足し、`all` で 2 本の VM の回転 z・pos のほかに、配列ごとの「描画だけの欄」（`RbSlotArray::draw_only`。
//!    `+0x596`・VM 1 の色 α・拡大）も外す。旗の低位 1 B `+0x49C`（描画 0x57 / 生成 0x17）は外し、上位 3 B `+0x49D`〜・世界の欄 `+0x548`〜`+0x599`・
//!    生死 `+0x584` は外さない。レーザーは弾と同じ範囲（`p?_bullets`）を名指すので、範囲の比べは `compare_region_slots` が 2 本の行を続けて数え、
//!    範囲の「枠の外」は弾の行だけが数える（二重に数えない）。診断はレーザーの欄を（VM 0 の pos / VM 1 の pos / 回転 z / ほかの描画だけの欄 /
//!    世界の欄 / 枠の中のほか）に分け、回ごとの行の「欄別」にバイト数を全部出す。
//! 5. 補間の始点: VM の `+0x238`〜`+0x243`（12 B）は、補間を始める命令が pos `+0x208` からコピーするだけの欄で、pos を外している VM では同じ扱いにする
//!    （`FIELDS` の 3 つ目）。診断は「補間の始点」の欄に別に数える。始点の手前（`+0x228`〜`+0x237`）と終点（`+0x244`〜）は外さない。
//!
//! 診断の数は、どの `skip_draw_fields` でも「比べから外す前」の違いを数える（外した分は「描画欄を比べから外した N B」に別に出る）。
//! だから `all` では「生きている枠の違い」に生きている枠の描画欄が入ったまま残る。合格を読むのは、除外の対象でない欄の違い
//! （「描画用の欄以外の違い」と「生きている枠の描画欄以外の違い」）。どの欄が「描画用の欄」に入るかは `skip_draw_fields` で変わる（`DiagRow::is_spec_draw`）。
//!
//! 「生きているか」は D1 の時点の値で決める。D2 が生きていても D1 で死んでいれば外すが、そういう枠は生死の旗そのものが違うので、旗の欄で食い違いが残り本物のずれは隠れない。
//!
//! ゲームのスレッドで走る（既に D2 の地点の比べの中）。固定長の表だけを使い、確保・パニックを置かない。
#![forbid(unsafe_code)]

use higanr_core::arena::{ArenaMem, ArenaState, BlockInfo, HEADER};

use crate::rollback_probe::{ExtraSkips, Region, RegionResult, Skips, TrialRecord};
use crate::th09_addr as a;

pub const INI_KEY: &str = "skip_draw_fields";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkipDraw {
    #[default]
    None,
    Dead,
    /// 生死に関係なく、枠の配列の VM（候補 VM を含む）と arena の使用中の VM の配列の描画用の欄を比べから外す。
    All,
}

impl SkipDraw {
    pub fn name(self) -> &'static str {
        match self {
            SkipDraw::None => "none",
            SkipDraw::Dead => "dead",
            SkipDraw::All => "all",
        }
    }

    pub fn parse(v: &str) -> Result<SkipDraw, String> {
        match v {
            "none" => Ok(SkipDraw::None),
            "dead" => Ok(SkipDraw::Dead),
            "all" => Ok(SkipDraw::All),
            _ => Err(format!("{INI_KEY} の値 {v:?} は none・dead・all のどれか")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliveRule {
    U16NotIn { off: u32, dead: &'static [u16] },
    U32Bit0 { off: u32 },
}

impl AliveRule {
    pub fn alive(&self, a: &[u8], base: usize) -> Option<bool> {
        match *self {
            AliveRule::U16NotIn { off, dead } => {
                let p = base.checked_add(off as usize)?;
                let b = a.get(p..p.checked_add(2)?)?;
                Some(!dead.contains(&u16::from_le_bytes([b[0], b[1]])))
            }
            AliveRule::U32Bit0 { off } => {
                let p = base.checked_add(off as usize)?;
                let b = a.get(p..p.checked_add(4)?)?;
                Some(b[0] & 1 != 0)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RbSlotArray {
    pub label: &'static str,
    pub region: &'static str,
    pub base: u32,
    pub stride: u32,
    pub count: u32,
    pub alive: AliveRule,
    /// 描画用の欄（回転 z・pos）を持つ VM の枠の中の位置（小さい順）。除外の対象。
    pub vms: &'static [u32],
    /// 診断だけで数える VM（除外はしない）。
    pub probe_vms: &'static [u32],
    /// VM の回転 z・pos のほかに、描画だけが書く欄（枠の中の位置, 長さ。小さい順。VM の欄とも重ならない）。VM の欄と同じ扱いで、`dead` では D1 で死んでいる枠のものだけ、
    /// `all` では全部の枠のものを比べから外す（レーザーの `+0x596`・色 α・拡大）。
    pub draw_only: &'static [(u32, u32)],
    pub world: &'static [(u32, u32)],
    pub pos_by_vm: bool,
    pub source: &'static str,
}

/// VM の描画用の欄（VM の先頭からの位置と長さ。小さい順）。回転 z・pos・補間の始点（pos からコピーされるだけの欄）。
const FIELDS: [(u32, u32); 3] = [
    (a::VM_ROT_Z_OFF, a::VM_ROT_Z_LEN),
    (a::VM_POS_OFF, a::VM_POS_LEN),
    (a::VM_INTERP_START_OFF, a::VM_INTERP_START_LEN),
];

/// 行の数の上限（本物は枠の配列 8（レーザーが 2 本）＋ arena = 9）。
pub const MAX_ROWS: usize = 10;
/// 欄の種類の上限（枠は 11・arena は 8）。
pub const MAX_CATS: usize = 11;
pub const MAX_SAMPLES: usize = 3;
pub const NO_SLOT: u32 = u32::MAX;

pub const CAT_OUTSIDE: usize = 0;
pub const CAT_OTHER: usize = 1;
pub const CAT_ROT: usize = 2;
pub const CAT_POS: usize = 3;
pub const CAT_CAND_ROT: usize = 4;
pub const CAT_CAND_POS: usize = 5;
pub const CAT_DRAW_ONLY: usize = 6;
pub const CAT_WORLD: usize = 7;
pub const CAT_POS_VM1: usize = 8;
/// VM の補間の始点 `+0x238`〜`+0x243`（pos からコピーされるだけの欄）と、診断だけの候補 VM のもの。
pub const CAT_INTERP: usize = 9;
pub const CAT_CAND_INTERP: usize = 10;
const SLOT_CAT_NAMES: [&str; 11] = [
    "枠の外",
    "枠の中の描画欄の外",
    "回転 z",
    "pos",
    "候補 VM の回転 z",
    "候補 VM の pos",
    "ほかの描画だけの欄",
    "世界の欄",
    "VM 1 の pos",
    "補間の始点",
    "候補 VM の補間の始点",
];

pub const ACAT_HEADER: usize = 1;
pub const ACAT_USED_VM_DRAW: usize = 2;
pub const ACAT_USED_VM_OTHER: usize = 3;
pub const ACAT_USED_OTHER: usize = 4;
pub const ACAT_FREE_VM_DRAW: usize = 5;
pub const ACAT_FREE_VM_OTHER: usize = 6;
pub const ACAT_FREE_OTHER: usize = 7;
const ARENA_CAT_NAMES: [&str; 8] = [
    "塊の外",
    "塊の頭",
    "使用中の VM の配列の描画欄",
    "使用中の VM の配列の描画欄の外",
    "使用中のほかの塊",
    "空きの VM 大の塊の描画欄",
    "空きの VM 大の塊の描画欄の外",
    "空きのほかの塊",
];

const CLS_NAMES: [&str; 4] = [
    "D1 死・D2 死",
    "D1 死・D2 生",
    "D1 生・D2 死",
    "D1 生・D2 生",
];

fn cls_of(alive1: bool, alive2: bool) -> usize {
    usize::from(alive1) * 2 + usize::from(alive2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Empty,
    Slots(&'static RbSlotArray),
    Arena,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub cat: u8,
    pub cls: u8,
    pub slot: u32,
    pub off: u32,
    pub w1: u32,
    pub w2: u32,
}

impl Sample {
    pub const EMPTY: Sample = Sample {
        cat: 0,
        cls: 0,
        slot: 0,
        off: 0,
        w1: 0,
        w2: 0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagRow {
    pub kind: RowKind,
    pub total: u32,
    pub bytes: [[u32; MAX_CATS]; 4],
    pub slots: [u32; 4],
    pub first_slot: [u32; 4],
    pub draw_skipped: u32,
    pub samples: [Sample; MAX_SAMPLES],
    pub n_samples: u8,
    pub mode: SkipDraw,
}

impl DiagRow {
    pub const EMPTY: DiagRow = DiagRow {
        kind: RowKind::Empty,
        mode: SkipDraw::None,
        total: 0,
        bytes: [[0; MAX_CATS]; 4],
        slots: [0; 4],
        first_slot: [NO_SLOT; 4],
        draw_skipped: 0,
        samples: [Sample::EMPTY; MAX_SAMPLES],
        n_samples: 0,
    };

    pub fn new(kind: RowKind) -> DiagRow {
        DiagRow {
            kind,
            ..DiagRow::EMPTY
        }
    }

    pub fn with_mode(kind: RowKind, mode: SkipDraw) -> DiagRow {
        DiagRow {
            kind,
            mode,
            ..DiagRow::EMPTY
        }
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            RowKind::Slots(s) => s.label,
            RowKind::Arena => "arena",
            RowKind::Empty => "?",
        }
    }

    fn unit(&self) -> &'static str {
        match self.kind {
            RowKind::Arena => "塊",
            _ => "枠",
        }
    }

    fn cat_name(&self, cat: usize) -> &'static str {
        let names: &[&str] = match self.kind {
            RowKind::Arena => &ARENA_CAT_NAMES,
            RowKind::Slots(s) if s.pos_by_vm && cat == CAT_POS => return "VM 0 の pos",
            _ => &SLOT_CAT_NAMES,
        };
        names.get(cat).copied().unwrap_or("?")
    }

    /// 除外の対象になる描画用の欄か。`none`・`dead` では診断だけの候補 VM の欄は含めず、`all` では含める。`all` でも arena の空きの VM 大の塊の欄は含めない
    /// （0 で埋めるので、違えば本物のずれ）。描画だけの欄は VM の回転 z・pos と同じ扱い。世界の欄は外さないので含めない。
    pub fn is_spec_draw(&self, cat: usize) -> bool {
        let all = self.mode == SkipDraw::All;
        match self.kind {
            RowKind::Slots(_) => {
                cat == CAT_ROT
                    || cat == CAT_POS
                    || cat == CAT_POS_VM1
                    || cat == CAT_INTERP
                    || cat == CAT_DRAW_ONLY
                    || (all
                        && (cat == CAT_CAND_ROT || cat == CAT_CAND_POS || cat == CAT_CAND_INTERP))
            }
            RowKind::Arena => cat == ACAT_USED_VM_DRAW || (!all && cat == ACAT_FREE_VM_DRAW),
            RowKind::Empty => false,
        }
    }

    pub fn outside(&self) -> u32 {
        self.bytes[0][CAT_OUTSIDE]
    }

    pub fn live(&self) -> u32 {
        (1..4)
            .flat_map(|c| self.bytes[c][1..].iter())
            .fold(0u32, |s, &n| s.saturating_add(n))
    }

    pub fn draw(&self) -> u32 {
        (0..MAX_CATS)
            .filter(|&cat| self.is_spec_draw(cat))
            .flat_map(|cat| (0..4).map(move |c| (c, cat)))
            .fold(0u32, |s, (c, cat)| s.saturating_add(self.bytes[c][cat]))
    }

    pub fn non_draw(&self) -> u32 {
        self.total.saturating_sub(self.draw())
    }

    pub fn live_non_draw(&self) -> u32 {
        (1..4)
            .flat_map(|c| (1..MAX_CATS).map(move |cat| (c, cat)))
            .filter(|&(_, cat)| !self.is_spec_draw(cat))
            .fold(0u32, |s, (c, cat)| s.saturating_add(self.bytes[c][cat]))
    }

    fn class_bytes(&self, c: usize) -> u32 {
        self.bytes[c][1..]
            .iter()
            .fold(0u32, |s, &n| s.saturating_add(n))
    }

    fn add(&mut self, cls: usize, cat: usize, at: (u32, u32), vals: (&[u8], &[u8], usize)) {
        let ((slot, off), (a1, a2, p)) = (at, vals);
        self.total = self.total.saturating_add(1);
        let b = &mut self.bytes[cls][cat];
        *b = b.saturating_add(1);
        // 例は「描画用の欄以外」か「生きている枠の欄」だけ（除外の候補に入る違いは数だけで足りる）。同じ語の 2 バイト目以降は足さない。
        let interesting = !self.is_spec_draw(cat) || (cls != 0 && cat != CAT_OUTSIDE);
        if !interesting || self.n_samples as usize >= MAX_SAMPLES {
            return;
        }
        if let Some(l) = self.samples[..self.n_samples as usize].last()
            && l.slot == slot
            && l.off / 4 == off / 4
        {
            return;
        }
        self.samples[self.n_samples as usize] = Sample {
            cat: cat as u8,
            cls: cls as u8,
            slot,
            off,
            w1: word(a1, p),
            w2: word(a2, p),
        };
        self.n_samples += 1;
    }

    fn note_slot(&mut self, cls: usize, slot: u32) {
        self.slots[cls] = self.slots[cls].saturating_add(1);
        if self.first_slot[cls] == NO_SLOT {
            self.first_slot[cls] = slot;
        }
    }

    pub fn merge(&mut self, o: &DiagRow) {
        self.mode = o.mode;
        self.total = self.total.saturating_add(o.total);
        self.draw_skipped = self.draw_skipped.saturating_add(o.draw_skipped);
        for c in 0..4 {
            for k in 0..MAX_CATS {
                self.bytes[c][k] = self.bytes[c][k].saturating_add(o.bytes[c][k]);
            }
            self.slots[c] = self.slots[c].saturating_add(o.slots[c]);
            if self.first_slot[c] == NO_SLOT {
                self.first_slot[c] = o.first_slot[c];
            }
        }
        for s in &o.samples[..o.n_samples as usize] {
            if (self.n_samples as usize) < MAX_SAMPLES {
                self.samples[self.n_samples as usize] = *s;
                self.n_samples += 1;
            }
        }
    }
}

fn word(a: &[u8], p: usize) -> u32 {
    let mut b = [0u8; 4];
    for (i, x) in b.iter_mut().enumerate() {
        if let Some(&v) = a.get(p + i) {
            *x = v;
        }
    }
    u32::from_le_bytes(b)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaWalk {
    NotRun,
    Ok,
    Broken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawDiag {
    pub valid: bool,
    pub mode: SkipDraw,
    pub arena_walk: ArenaWalk,
    pub n: u8,
    pub rows: [DiagRow; MAX_ROWS],
}

impl DrawDiag {
    pub const EMPTY: DrawDiag = DrawDiag {
        valid: false,
        mode: SkipDraw::None,
        arena_walk: ArenaWalk::NotRun,
        n: 0,
        rows: [DiagRow::EMPTY; MAX_ROWS],
    };

    pub fn begin(&mut self, specs: &'static [RbSlotArray], mode: SkipDraw) {
        *self = DrawDiag::EMPTY;
        self.valid = true;
        self.mode = mode;
        for (i, s) in specs.iter().take(MAX_ROWS - 1).enumerate() {
            self.rows[i] = DiagRow::with_mode(RowKind::Slots(s), mode);
            self.n += 1;
        }
        self.rows[self.n as usize] = DiagRow::with_mode(RowKind::Arena, mode);
        self.n += 1;
    }

    pub fn rows(&self) -> &[DiagRow] {
        &self.rows[..self.n as usize]
    }

    /// 枠の配列 `label` の行（同じ範囲を名指す枠の配列が 2 本ある（弾とレーザー）ので、範囲の名前でなく行の名前で引く）。
    fn row_of_label(&mut self, label: &str) -> Option<&mut DiagRow> {
        let n = self.n as usize;
        self.rows[..n]
            .iter_mut()
            .find(|r| matches!(r.kind, RowKind::Slots(s) if s.label == label))
    }
}

fn slot_cat(spec: &RbSlotArray, off: u32) -> usize {
    let lists = [
        (spec.vms, [CAT_ROT, CAT_POS, CAT_INTERP]),
        (
            spec.probe_vms,
            [CAT_CAND_ROT, CAT_CAND_POS, CAT_CAND_INTERP],
        ),
    ];
    for (vms, cats) in lists {
        for (n, &vm) in vms.iter().enumerate() {
            for (i, &(fo, fl)) in FIELDS.iter().enumerate() {
                let s = vm + fo;
                if off >= s && off < s + fl {
                    // pos だけ VM の順で数え分ける（回転 z は分けない）。診断だけの VM は分けない。
                    let later = spec.pos_by_vm && n >= 1 && cats[i] == CAT_POS;
                    return if later { CAT_POS_VM1 } else { cats[i] };
                }
            }
        }
    }
    // 描画だけの欄を先に引く（世界の欄と重なるレーザーの `+0x596` は描画だけの欄に数える）。
    let inside = |list: &[(u32, u32)]| list.iter().any(|&(s, l)| off >= s && off < s + l);
    if inside(spec.draw_only) {
        CAT_DRAW_ONLY
    } else if inside(spec.world) {
        CAT_WORLD
    } else {
        CAT_OTHER
    }
}

fn in_skips(skips: &Skips, p: usize) -> bool {
    skips
        .ranges()
        .iter()
        .any(|&(s, e)| (s as usize) <= p && p < e as usize)
}

fn unskipped_len(skips: &Skips, lo: u32, hi: u32) -> u32 {
    let mut cur = lo;
    let mut n = 0u32;
    for &(s, e) in skips.ranges() {
        let (s, e) = (s.min(hi), e.min(hi));
        if s > cur {
            n += s - cur;
        }
        cur = cur.max(e);
    }
    n + hi.saturating_sub(cur)
}

/// `[lo, hi)`（両方の長さの中）の違いを、枠の外の違いとして数える。`claimed` は「ほかの枠の配列が占める所」（そちらの行が数える）。
fn count_outside(
    row: &mut DiagRow,
    (a1, a2): (&[u8], &[u8]),
    skips: &Skips,
    claimed: &dyn Fn(usize) -> bool,
    (lo, hi): (usize, usize),
) {
    let hi = hi.min(a1.len()).min(a2.len());
    if lo >= hi || a1[lo..hi] == a2[lo..hi] {
        return;
    }
    for p in lo..hi {
        if a1[p] != a2[p] && !in_skips(skips, p) && !claimed(p) {
            row.add(0, CAT_OUTSIDE, (NO_SLOT, p as u32), (a1, a2, p));
        }
    }
}

fn count_length_gap(row: &mut DiagRow, a1: &[u8], a2: &[u8], skips: &Skips) {
    let (n, m) = (a1.len().min(a2.len()), a1.len().max(a2.len()));
    if n < m {
        let k = unskipped_len(skips, n as u32, m as u32);
        row.total = row.total.saturating_add(k);
        row.bytes[0][CAT_OUTSIDE] = row.bytes[0][CAT_OUTSIDE].saturating_add(k);
    }
}

/// 同じ範囲を 2 本の枠の配列（弾とレーザー）が名指すとき、範囲の「枠の外」を数える担当は表で先に出てくる方（弾）だけ（範囲の全部の違いが行の合計と一致するように）。
/// `specs` に `spec` の範囲が無い（検査の偽の枠の配列）なら、その 1 本が担当。
fn owns_outside(spec: &RbSlotArray, specs: &[RbSlotArray]) -> bool {
    specs
        .iter()
        .find(|s| s.region == spec.region)
        .is_none_or(|s| s.label == spec.label)
}

fn in_sibling_slots(spec: &RbSlotArray, specs: &[RbSlotArray], n: usize, p: usize) -> bool {
    specs
        .iter()
        .filter(|s| s.region == spec.region && s.label != spec.label)
        .any(|s| {
            let (b, stride) = (s.base as usize, s.stride as usize);
            let whole = (n.saturating_sub(b) / stride.max(1)).min(s.count as usize);
            p >= b && p < b + whole * stride
        })
}

/// 枠の配列 `spec` の範囲（D1 のコピー `a1`・今 `a2`）の違いを `row` へ数える。`specs` は表の全部（同じ範囲を名指すほかの枠の配列が占める所は数えない。
/// `owns_outside` でなければ、枠の外も数えない）。
pub fn analyze_slots(
    spec: &'static RbSlotArray,
    specs: &[RbSlotArray],
    (a1, a2): (&[u8], &[u8]),
    skips: &Skips,
    row: &mut DiagRow,
) {
    let n = a1.len().min(a2.len());
    let (base, stride) = (spec.base as usize, spec.stride as usize);
    let owner = owns_outside(spec, specs);
    let claimed = |p: usize| in_sibling_slots(spec, specs, n, p);
    if owner {
        count_outside(row, (a1, a2), skips, &claimed, (0, base.min(n)));
    }
    let mut done = 0usize;
    while done < spec.count as usize {
        let lo = base + done * stride;
        let hi = lo + stride;
        if hi > n {
            break;
        }
        let slot = done;
        done += 1;
        if a1[lo..hi] == a2[lo..hi] {
            continue;
        }
        let al1 = spec.alive.alive(a1, lo).unwrap_or(false);
        let al2 = spec.alive.alive(a2, lo).unwrap_or(false);
        let cls = cls_of(al1, al2);
        let mut any = false;
        for p in lo..hi {
            if a1[p] != a2[p] && !in_skips(skips, p) {
                let off = (p - lo) as u32;
                row.add(cls, slot_cat(spec, off), (slot as u32, off), (a1, a2, p));
                any = true;
            }
        }
        if any {
            row.note_slot(cls, slot as u32);
        }
    }
    if owner {
        count_outside(
            row,
            (a1, a2),
            skips,
            &claimed,
            ((base + done * stride).min(n), n),
        );
        count_length_gap(row, a1, a2, skips);
    }
}

/// 1 つの枠の中の、除外の対象の欄の最大の数（`slot_ranges` の置き場）。
pub const MAX_SLOT_RANGES: usize = 24;

/// `mode` で外す欄を、1 つの枠の中の位置（始まり, 終わり）で小さい順に `out` へ置き、数を返す。`Dead` は除外の VM の欄と `draw_only`、`All` はそれに診断だけの候補 VM の欄を足す。
/// `None` は 0。置き場に入りきらない分は置かない。小さい順への並べ替えは挿入（確保しない）。
fn slot_ranges(
    spec: &RbSlotArray,
    mode: SkipDraw,
    out: &mut [(u32, u32); MAX_SLOT_RANGES],
) -> usize {
    if mode == SkipDraw::None {
        return 0;
    }
    let mut n = 0usize;
    let mut put = |s: u32, e: u32| {
        if n < MAX_SLOT_RANGES {
            let mut i = n;
            while i > 0 && out[i - 1].0 > s {
                out[i] = out[i - 1];
                i -= 1;
            }
            out[i] = (s, e);
            n += 1;
        }
    };
    let probes: &[u32] = if mode == SkipDraw::All {
        spec.probe_vms
    } else {
        &[]
    };
    for &vm in spec.vms.iter().chain(probes) {
        for &(fo, fl) in &FIELDS {
            put(vm + fo, vm + fo + fl);
        }
    }
    for &(o, l) in spec.draw_only {
        put(o, o + l);
    }
    n
}

/// `all` で 1 回の比べに外す、枠の配列の分のバイト数（表から数える）。本ログの内訳の期待値と、`.run` の合格の数の原本。
pub fn all_skip_bytes_per_compare(specs: &[RbSlotArray]) -> u32 {
    let mut rel = [(0u32, 0u32); MAX_SLOT_RANGES];
    specs.iter().fold(0u32, |sum, spec| {
        let n = slot_ranges(spec, SkipDraw::All, &mut rel);
        let per_slot: u32 = rel[..n].iter().map(|&(s, e)| e - s).sum();
        sum.saturating_add(per_slot.saturating_mul(spec.count))
    })
}

/// 枠の配列 `spec` の描画用の欄を、小さい順に `emit` へ渡す。`Dead` なら D1 の時点で死んでいる枠の除外の VM だけ、`All` なら生死に関係なく全部の枠の除外の VM と
/// 診断だけの候補 VM、`None` は何も渡さない。枠がコピーの中に丸ごと入らないもの・（`Dead` で）生死の欄が読めないものは渡さない（読めない枠は外さない）。
pub fn walk_slot_skips(
    spec: &RbSlotArray,
    a1: &[u8],
    mode: SkipDraw,
    emit: &mut dyn FnMut(u32, u32),
) {
    let mut rel = [(0u32, 0u32); MAX_SLOT_RANGES];
    let n = slot_ranges(spec, mode, &mut rel);
    if n == 0 {
        return;
    }
    let (base, stride) = (spec.base as usize, spec.stride as usize);
    for slot in 0..spec.count as usize {
        let lo = base + slot * stride;
        if lo + stride > a1.len() {
            break;
        }
        if mode == SkipDraw::Dead && spec.alive.alive(a1, lo) != Some(false) {
            continue;
        }
        for &(s, e) in &rel[..n] {
            emit((lo + s as usize) as u32, (lo + e as usize) as u32);
        }
    }
}

/// 読むだけの `ArenaMem`（D1 のコピーのバイト列。`top` の外は 0 を返す）。
pub struct SliceMem<'a>(pub &'a [u8]);

impl ArenaMem for SliceMem<'_> {
    fn len(&self) -> u32 {
        self.0.len() as u32
    }
    fn read(&self, off: u32) -> u32 {
        word(self.0, off as usize)
    }
    fn write(&mut self, _off: u32, _v: u32) {
        // 診断はコピーを書き換えない。
    }
}

/// 求められた大きさ（塊の級を越えない範囲）が VM の配列の大きさか（`0x2A4` の 1 倍以上の倍数）。空きの塊の札は失われるので、これで推す。
/// 先頭が壊れて大きさが級より大きいときに長い繰り返しにならないよう、級で切る。
fn vm_array_len(size: u32, payload_bytes: u32) -> u32 {
    size.min(payload_bytes)
}

fn is_vm_sized(size: u32, payload_bytes: u32) -> bool {
    let s = vm_array_len(size, payload_bytes);
    s >= a::VM_BYTES && s.is_multiple_of(a::VM_BYTES)
}

/// 塊の VM の本数。使用中で札がある塊は札で決める: Ex（`ROLLBACK_ARENA_EX_TAG`）は `n × 0x2A4 + 追加` なので表（`a::ex_vm_count`。表に無い大きさは 0 本）から引く。
/// ほかの札と、札を失った空きの塊（`is_vm_sized` で推す）は、大きさを `0x2A4` で割った数。
fn vm_count(b: &BlockInfo, vm_tag: Option<u32>) -> u32 {
    let len = vm_array_len(b.size, b.payload_bytes);
    match vm_tag {
        Some(a::ROLLBACK_ARENA_EX_TAG) if b.used => a::ex_vm_count(len),
        _ => len / a::VM_BYTES,
    }
}

fn in_vm_draw_field(poff: u32, n_vm: u32) -> bool {
    let (vm, within) = (poff / a::VM_BYTES, poff % a::VM_BYTES);
    vm < n_vm
        && FIELDS
            .iter()
            .any(|&(fo, fl)| within >= fo && within < fo + fl)
}

pub fn walk_ok<M: ArenaMem>(st: &ArenaState, mem: &M) -> bool {
    let mut at = 0u32;
    while let Some(b) = st.block_at(mem, at) {
        at += HEADER + b.payload_bytes;
    }
    at >= st.top && st.top == mem.len()
}

/// D1 の割り当て器 `st1`（コピー `mem1`）を歩き、除外の対象の塊の描画用の欄を、小さい順に `emit` へ渡す。`Dead` なら D1 で空きの VM 大の塊、
/// `All` なら D1 で使用中で札が `vm_tags` に入る塊（空きの塊は 0 で埋まっているので外さない）。`None` は何も渡さない。
pub fn walk_arena_skips<M: ArenaMem>(
    st1: &ArenaState,
    mem1: &M,
    (mode, vm_tags): (SkipDraw, &[u32]),
    emit: &mut dyn FnMut(u32, u32),
) {
    let mut at = 0u32;
    while let Some(b) = st1.block_at(mem1, at) {
        let p = at + HEADER;
        let target = match mode {
            SkipDraw::None => false,
            SkipDraw::Dead => !b.used && is_vm_sized(b.size, b.payload_bytes),
            SkipDraw::All => b.used && b.tag.is_some_and(|t| vm_tags.contains(&t)),
        };
        if target {
            for i in 0..vm_count(&b, b.tag) {
                let vm = p + i * a::VM_BYTES;
                for &(fo, fl) in &FIELDS {
                    emit(vm + fo, vm + fo + fl);
                }
            }
        }
        at = p + b.payload_bytes;
    }
}

/// arena の違いを `row` へ数える。塊は D1 の割り当て器 `st1` で歩き、D2 の生死は同じ位置の塊を `st2`・`mem2` で引く。歩き終えた先・長さの違いは塊の外。
pub fn analyze_arena<M: ArenaMem>(
    vm_tags: &[u32],
    a1: &[u8],
    a2: &[u8],
    skips: &Skips,
    (st1, st2, mem2): (&ArenaState, &ArenaState, &M),
    row: &mut DiagRow,
) {
    let n = a1.len().min(a2.len());
    let mem1 = SliceMem(a1);
    let mut at = 0u32;
    while let Some(b) = st1.block_at(&mem1, at) {
        let end = at + HEADER + b.payload_bytes;
        let (lo, hi) = (at as usize, (end as usize).min(n));
        if lo < hi && a1[lo..hi] != a2[lo..hi] {
            let al2 = st2.block_at(mem2, at).is_some_and(|x| x.used);
            let cls = cls_of(b.used, al2);
            let vm_tag = b.tag.is_some_and(|t| vm_tags.contains(&t));
            let n_vm = vm_count(&b, b.tag.filter(|_| vm_tag));
            let mut any = false;
            for p in lo..hi {
                if a1[p] == a2[p] || in_skips(skips, p) {
                    continue;
                }
                let off = (p - lo) as u32;
                let (cat, off_in) = if off < HEADER {
                    (ACAT_HEADER, off)
                } else {
                    let poff = off - HEADER;
                    let draw = in_vm_draw_field(poff, n_vm);
                    let cat = match (b.used, vm_tag, is_vm_sized(b.size, b.payload_bytes)) {
                        (true, true, _) if draw => ACAT_USED_VM_DRAW,
                        (true, true, _) => ACAT_USED_VM_OTHER,
                        (true, false, _) => ACAT_USED_OTHER,
                        (false, _, true) if draw => ACAT_FREE_VM_DRAW,
                        (false, _, true) => ACAT_FREE_VM_OTHER,
                        (false, _, false) => ACAT_FREE_OTHER,
                    };
                    (cat, poff)
                };
                row.add(cls, cat, (at, off_in), (a1, a2, p));
                any = true;
            }
            if any {
                row.note_slot(cls, at);
            }
        }
        at = end;
    }
    count_outside(row, (a1, a2), skips, &|_| false, ((at as usize).min(n), n));
    count_length_gap(row, a1, a2, skips);
}

fn skipped_len(walk: ExtraSkips) -> u32 {
    let mut n = 0u32;
    walk(&mut |s, e| n = n.saturating_add(e.saturating_sub(s)));
    n
}

/// 枠の配列 `spec` 1 本の診断: 違いがあれば行 `row` へ数え、`mode` で外す欄のバイト数を `row.draw_skipped` に置く。診断は外す前の違いを数える。
pub fn count_slots(
    spec: &'static RbSlotArray,
    specs: &[RbSlotArray],
    (a1, a2): (&[u8], &[u8]),
    skips: &Skips,
    mode: SkipDraw,
    row: &mut DiagRow,
) {
    row.mode = mode;
    if a1 != a2 {
        analyze_slots(spec, specs, (a1, a2), skips, row);
    }
    row.draw_skipped = skipped_len(&|emit: &mut dyn FnMut(u32, u32)| {
        walk_slot_skips(spec, a1, mode, emit);
    });
}

/// 範囲 `region`（弾・敵・自機の塊）の比べ: この範囲を名指す枠の配列（弾の範囲は弾とレーザーの 2 本）の全部について診断を数え、`mode` が `Dead` なら D1 で死んでいる枠の
/// 描画用の欄を、`All` なら生死に関係なく全部の枠の描画用の欄を外して比べる。`None` なら `RegionResult::of` と同じ結果。名指す枠の配列が表に無い範囲は `None`。
///
/// 除外の区間は表の順に渡す。同じ範囲の枠の配列は表で `base` の小さい順に並び、占める所が重ならない（表の検査 `slot_table_is_consistent`）ので全体でも小さい順になる。
pub fn compare_region_slots(
    r: &Region,
    (region, specs): (&str, &'static [RbSlotArray]),
    (a1, a2): (&[u8], &[u8]),
    skips: &Skips,
    mode: SkipDraw,
    diag: &mut DrawDiag,
) -> Option<RegionResult> {
    let mut found = false;
    for spec in specs.iter().filter(|s| s.region == region) {
        found = true;
        // 行が無い（表が行の上限を越えた）枠の配列は診断を数えない。除外は行に依らず下で掛ける。
        if let Some(row) = diag.row_of_label(spec.label) {
            count_slots(spec, specs, (a1, a2), skips, mode, row);
        }
    }
    if !found {
        return None;
    }
    let walk = |emit: &mut dyn FnMut(u32, u32)| {
        for spec in specs.iter().filter(|s| s.region == region) {
            walk_slot_skips(spec, a1, mode, emit);
        }
    };
    Some(RegionResult::of_with(r, a1, a2, skips, &walk))
}

/// arena の比べ。`st1` は D1 の割り当て器。歩けなければ（`ArenaWalk::Broken`）除外はしない。
pub fn compare_arena<M: ArenaMem>(
    r: &Region,
    vm_tags: &[u32],
    (a1, a2): (&[u8], &[u8]),
    skips: &Skips,
    mode: SkipDraw,
    states: (&ArenaState, &ArenaState, &M),
    (row, walk_state): (&mut DiagRow, &mut ArenaWalk),
) -> RegionResult {
    let (st1, _, _) = states;
    let mem1 = SliceMem(a1);
    let ok = walk_ok(st1, &mem1);
    *walk_state = if ok { ArenaWalk::Ok } else { ArenaWalk::Broken };
    row.mode = mode;
    if a1 != a2 {
        analyze_arena(vm_tags, a1, a2, skips, states, row);
    }
    let walk = |emit: &mut dyn FnMut(u32, u32)| {
        if ok {
            walk_arena_skips(st1, &mem1, (mode, vm_tags), emit);
        }
    };
    row.draw_skipped = skipped_len(&walk);
    RegionResult::of_with(r, a1, a2, skips, &walk)
}

pub fn arena_row(diag: &mut DrawDiag) -> Option<(&mut DiagRow, &mut ArenaWalk)> {
    let i = diag.rows[..diag.n as usize]
        .iter()
        .position(|r| r.kind == RowKind::Arena)?;
    Some((&mut diag.rows[i], &mut diag.arena_walk))
}

fn hex_f32(w: u32) -> String {
    format!("0x{w:08X}（f32 {}）", f32::from_bits(w))
}

fn row_text(prefix: &str, row: &DiagRow) -> Option<String> {
    if row.total == 0 {
        return None;
    }
    let unit = row.unit();
    let classes = [3usize, 2, 1, 0]
        .iter()
        .map(|&c| {
            format!(
                "{} {} B（{} {unit}）",
                CLS_NAMES[c],
                row.class_bytes(c),
                row.slots[c]
            )
        })
        .collect::<Vec<_>>()
        .join(" / ");
    let cats = (0..MAX_CATS)
        .filter_map(|k| {
            let n: u32 = (0..4).map(|c| row.bytes[c][k]).sum();
            (n > 0).then(|| format!("{} {n} B", row.cat_name(k)))
        })
        .collect::<Vec<_>>()
        .join(" / ");
    Some(format!(
        "{prefix}{}: 違い {} B（比べから外す前の数）。D1・D2 の生死別: {classes}。{unit}の外 {} B。欄別: {cats}。生きている{unit}の違い {} B（うち描画欄以外 {} B）・描画用の欄の違い {} B・描画用の欄以外の違い {} B。描画欄を比べから外した {} B",
        row.name(),
        row.total,
        row.outside(),
        row.live(),
        row.live_non_draw(),
        row.draw(),
        row.non_draw(),
        row.draw_skipped
    ))
}

fn sample_lines(prefix: &str, row: &DiagRow) -> Vec<String> {
    row.samples[..row.n_samples as usize]
        .iter()
        .map(|s| {
            let at = if s.slot == NO_SLOT {
                format!("{}の外 範囲の +0x{:X}", row.unit(), s.off)
            } else if row.kind == RowKind::Arena {
                format!("塊の頭 +0x{:X} の中身の +0x{:X}", s.slot, s.off)
            } else {
                format!("{} 番の枠の +0x{:X}", s.slot, s.off)
            };
            format!(
                "{prefix}{} の描画欄以外の例: {at}（{}、{}）: D1 {} / D2 {}",
                row.name(),
                CLS_NAMES.get(s.cls as usize).copied().unwrap_or("?"),
                row.cat_name(s.cat as usize),
                hex_f32(s.w1),
                hex_f32(s.w2)
            )
        })
        .collect()
}

pub fn trial_lines(d: &DrawDiag) -> Vec<String> {
    let mut v = Vec::new();
    if !d.valid {
        return v;
    }
    let prefix = "巻き戻し:   診断 ";
    for row in d.rows() {
        if let Some(l) = row_text(prefix, row) {
            v.push(l);
            v.extend(sample_lines(prefix, row));
        }
    }
    if d.arena_walk == ArenaWalk::Broken {
        v.push(format!(
            "{prefix}arena: D1 の割り当て器を最後まで歩けなかった（塊ごとの分けは歩けた所まで。skip_draw_fields = {} でも arena の描画欄は外していない）",
            d.mode.name()
        ));
    }
    v
}

pub fn summary_lines(rs: &[TrialRecord], mode: SkipDraw) -> Vec<String> {
    let mut v = Vec::new();
    let valid: Vec<&TrialRecord> = rs.iter().filter(|r| r.draw.valid).collect();
    let head = "巻き戻し: 診断の口";
    if valid.is_empty() {
        v.push(format!(
            "{head}: 比べた回が 0 回（診断は測れていない。skip_draw_fields = {}）",
            mode.name()
        ));
        return v;
    }
    let mut agg: Vec<DiagRow> = Vec::new();
    for r in &valid {
        for row in r.draw.rows() {
            match agg.iter_mut().find(|x| x.kind == row.kind) {
                Some(x) => x.merge(row),
                None => {
                    let mut x = DiagRow::new(row.kind);
                    x.merge(row);
                    agg.push(x);
                }
            }
        }
    }
    let differing = valid
        .iter()
        .filter(|r| r.draw.rows().iter().any(|x| x.total > 0))
        .count();
    let sum = |f: &dyn Fn(&DiagRow) -> u32| agg.iter().fold(0u32, |s, x| s.saturating_add(f(x)));
    let (live, live_non_draw, draw, non_draw, outside, skipped) = (
        sum(&DiagRow::live),
        sum(&DiagRow::live_non_draw),
        sum(&DiagRow::draw),
        sum(&DiagRow::non_draw),
        sum(&DiagRow::outside),
        sum(&|x: &DiagRow| x.draw_skipped),
    );
    v.push(format!(
        "{head}: 比べた回 {} 回のうち違いのあった回 {differing} 回（skip_draw_fields = {}。バイト数は回をまたいだ延べ。生死は D1 の時点と D2 の時点で分ける）",
        valid.len(),
        mode.name()
    ));
    v.push(format!(
        "{head}: 生きている枠の違い {live} バイト / 描画用の欄の違い {draw} バイト / 描画用の欄以外の違い {non_draw} バイト（うち枠・塊の外 {outside}）/ 描画用の欄を比べから外したバイト {skipped}"
    ));
    // 内訳: 枠の配列の分は状態に依らず一定（`all` なら弾 536 × 5 VM × 2 陣 ＋ 敵 128 × 2 VM × 2 ＋ 自機ショット 128 × 1 × 2 = 6,128 VM × 28 B = 171,584 B / 回、
    // レーザー 48 本 × (2 VM × 28 B ＋ 描画だけの欄 15 B) × 2 陣 = 6,816 B で 178,400 B / 回）。arena の分は使用中の VM の配列の数で動く。
    let (skipped_slots, skipped_arena) = agg.iter().fold((0u32, 0u32), |(s, r), x| match x.kind {
        RowKind::Slots(_) => (s.saturating_add(x.draw_skipped), r),
        _ => (s, r.saturating_add(x.draw_skipped)),
    });
    let expected = if mode == SkipDraw::All {
        let per = all_skip_bytes_per_compare(a::ROLLBACK_SLOT_ARRAYS);
        format!(
            "（表から数えた期待 {per} バイト × {} 回 = {} バイト）",
            valid.len(),
            per.saturating_mul(valid.len() as u32)
        )
    } else {
        String::new()
    };
    v.push(format!(
        "{head}: 描画用の欄を比べから外したバイトの内訳: 枠の配列の分 {skipped_slots} バイト / arena の分 {skipped_arena} バイト（比べた回 {} 回）{expected}",
        valid.len()
    ));
    v.push(format!(
        "{head}: 生きている枠の描画欄以外の違い {live_non_draw} バイト（上の「生きている枠の違い」{live} バイトのうち、除外の対象でない欄の分。診断の数は比べから外す前の違いで、skip_draw_fields = {} で外したのは「描画用の欄を比べから外したバイト」）",
        mode.name()
    ));
    let (ok, broken) = (
        valid
            .iter()
            .filter(|r| r.draw.arena_walk == ArenaWalk::Ok)
            .count(),
        valid
            .iter()
            .filter(|r| r.draw.arena_walk == ArenaWalk::Broken)
            .count(),
    );
    v.push(format!(
        "{head}: arena の塊を D1 の割り当て器で歩けた回 {ok} / 歩けなかった回 {broken}（母数は比べた回 {}。歩けなければ arena の描画欄は外さない）",
        valid.len()
    ));
    for row in &agg {
        if let Some(l) = row_text("巻き戻し: 診断 ", row) {
            v.push(l);
            v.extend(sample_lines("巻き戻し: 診断 ", row));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollback_probe::{RegionKind, compare_bytes, no_extra_skips, skip_draw_from_text};

    const NOTE: &str = include_str!("../../../analysis/notes/th09_rollback_E1.md");

    /// VM 1 本あたりに `all` / `dead` が外すバイト数 = 回転 z 4 ＋ pos 12 ＋ 補間の始点 12。
    const VM_SKIP: u32 = 4 + 12 + 12;
    const L_SKIP: u32 = 2 * VM_SKIP + 15;

    // ---- 偽の枠の配列 ----
    // 枠 4 つ・stride 0x600・VM は枠の +0x4 と +0x2A8。生死の欄は枠の +0x5E0（u16。0 と 6 が死）。範囲は `[0, 0x40)` が枠の前・`[0x1840, 0x1860)` が枠の後。
    const BASE: usize = 0x40;
    const STRIDE: usize = 0x600;
    const COUNT: usize = 4;
    const LEN: usize = BASE + STRIDE * COUNT + 0x20;
    const FLAG: usize = 0x5E0;
    static SPEC: RbSlotArray = RbSlotArray {
        label: "t_bullets",
        region: "t_region",
        base: BASE as u32,
        stride: STRIDE as u32,
        count: COUNT as u32,
        alive: AliveRule::U16NotIn {
            off: FLAG as u32,
            dead: &[0, 6],
        },
        vms: &[0x4, 0x2A8],
        probe_vms: &[0x1A0],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: "",
    };
    /// 除外の VM 1 本（`+0x4`）と診断だけの候補 VM 1 本（`+0x2A8`）。VM は 0x2A4 ずつ離す（重なると欄の並びが小さい順にならない）。
    static SPEC_A: RbSlotArray = RbSlotArray {
        label: "t_bullets",
        region: "t_region",
        base: BASE as u32,
        stride: STRIDE as u32,
        count: COUNT as u32,
        alive: AliveRule::U16NotIn {
            off: FLAG as u32,
            dead: &[0, 6],
        },
        vms: &[0x4],
        probe_vms: &[0x2A8],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: "",
    };
    static SPEC_B: RbSlotArray = RbSlotArray {
        label: "t_bullets",
        region: "t_region",
        base: BASE as u32,
        stride: STRIDE as u32,
        count: COUNT as u32,
        alive: AliveRule::U16NotIn {
            off: FLAG as u32,
            dead: &[0, 6],
        },
        vms: &[0x2A8],
        probe_vms: &[0x4],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: "",
    };
    static SPEC_BIT: RbSlotArray = RbSlotArray {
        label: "t_enemies",
        region: "t_bit",
        base: BASE as u32,
        stride: STRIDE as u32,
        count: COUNT as u32,
        alive: AliveRule::U32Bit0 { off: FLAG as u32 },
        vms: &[0x8],
        probe_vms: &[],
        draw_only: &[],
        world: &[],
        pos_by_vm: false,
        source: "",
    };

    fn region(len: usize) -> Region {
        Region {
            kind: RegionKind::Block(0),
            addr: 0x1000,
            len: len as u32,
            cap: len as u32,
            buf_off: 0,
        }
    }

    fn slot(i: usize) -> usize {
        BASE + i * STRIDE
    }

    fn set16(a: &mut [u8], p: usize, v: u16) {
        a[p..p + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn pair(flags1: [u16; COUNT], flags2: [u16; COUNT]) -> (Vec<u8>, Vec<u8>) {
        let (mut a1, mut a2) = (vec![0u8; LEN], vec![0u8; LEN]);
        for i in 0..COUNT {
            set16(&mut a1, slot(i) + FLAG, flags1[i]);
            set16(&mut a2, slot(i) + FLAG, flags2[i]);
        }
        (a1, a2)
    }

    fn dirty_draw(a2: &mut [u8], i: usize, vm: usize) {
        for o in (vm + 0x8..vm + 0xC).chain(vm + 0x208..vm + 0x214) {
            a2[slot(i) + o] = 1;
        }
    }

    fn run(
        spec: &'static RbSlotArray,
        a1: &[u8],
        a2: &[u8],
        skips: &Skips,
        mode: SkipDraw,
    ) -> (RegionResult, DiagRow) {
        let mut row = DiagRow::new(RowKind::Slots(spec));
        count_slots(spec, &[], (a1, a2), skips, mode, &mut row);
        let walk = |emit: &mut dyn FnMut(u32, u32)| walk_slot_skips(spec, a1, mode, emit);
        let res = RegionResult::of_with(&region(a1.len()), a1, a2, skips, &walk);
        (res, row)
    }

    #[test]
    fn alive_rules_read_the_flag_fields() {
        let mut a = vec![0u8; 0x20];
        let bullet = AliveRule::U16NotIn {
            off: 4,
            dead: &[0, 6],
        };
        let shot = AliveRule::U16NotIn { off: 4, dead: &[0] };
        for (v, b, s) in [
            (0u16, false, false),
            (1, true, true),
            (2, true, true),
            (5, true, true),
            (6, false, true),
            (7, true, true),
        ] {
            set16(&mut a, 4, v);
            assert_eq!(bullet.alive(&a, 0), Some(b), "弾 {v}");
            assert_eq!(shot.alive(&a, 0), Some(s), "自機ショット {v}");
        }
        let bit = AliveRule::U32Bit0 { off: 8 };
        for (v, want) in [
            (0u32, false),
            (1, true),
            (2, false),
            (0x8000_0000, false),
            (0x101, true),
        ] {
            a[8..12].copy_from_slice(&v.to_le_bytes());
            assert_eq!(bit.alive(&a, 0), Some(want), "旗 {v:#x}");
        }
        assert_eq!(bullet.alive(&a, 0x1F), None, "欄がコピーの外");
        assert_eq!(bit.alive(&a, usize::MAX), None, "溢れる位置");
    }

    #[test]
    fn live_slots_are_never_skipped() {
        let (a1, mut a2) = pair([1, 1, 0, 0], [1, 0, 0, 0]);
        dirty_draw(&mut a2, 0, 0x4);
        dirty_draw(&mut a2, 1, 0x2A8);
        let (none, row_none) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::None);
        let (dead, row_dead) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        let want = 16 + 16 + 1;
        assert_eq!(none.diff.bytes, want);
        assert_eq!(dead.diff.bytes, want, "生きている枠の欄を外した");
        assert_eq!(dead.skipped - none.skipped, 2 * 2 * VM_SKIP);
        assert_eq!(row_dead.total, row_none.total);
        assert_eq!(
            row_dead.bytes[3][CAT_ROT] + row_dead.bytes[3][CAT_POS],
            16,
            "生・生"
        );
        assert_eq!(
            row_dead.bytes[2][CAT_ROT] + row_dead.bytes[2][CAT_POS],
            16,
            "D1 生・D2 死"
        );
        assert_eq!(row_dead.live(), want);
        assert_eq!(row_dead.draw_skipped, 2 * 2 * VM_SKIP);
    }

    #[test]
    fn dead_slots_lose_exactly_their_draw_fields() {
        let (a1, mut a2) = pair([0; COUNT], [0; COUNT]);
        dirty_draw(&mut a2, 2, 0x4);
        dirty_draw(&mut a2, 2, 0x2A8);
        let outside = [
            0x4 + 0x7,
            0x4 + 0xC,
            0x4 + 0x207,
            0x4 + 0x214,
            0x2A8 + 0x7,
            0x2A8 + 0x214,
        ];
        for &o in &outside {
            a2[slot(2) + o] = 1;
        }
        dirty_draw(&mut a2, 2, 0x1A0); // 診断だけの候補 VM
        let (none, row_none) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::None);
        let (dead, row_dead) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        let all = 2 * 16 + outside.len() as u32 + 16;
        assert_eq!(none.diff.bytes, all);
        assert_eq!(
            dead.diff.bytes,
            outside.len() as u32 + 16,
            "外すのは除外の VM 2 本の 16 バイトずつだけ（外の 1 バイトと候補 VM は残る）"
        );
        assert_eq!(dead.skipped - none.skipped, 4 * 2 * VM_SKIP);
        assert_eq!(row_dead.draw_skipped, 4 * 2 * VM_SKIP);
        assert_eq!(row_none.total, all);
        assert_eq!(row_dead.total, all);
        assert_eq!(row_dead.bytes[0][CAT_ROT], 8);
        assert_eq!(row_dead.bytes[0][CAT_POS], 24);
        assert_eq!(
            row_dead.bytes[0][CAT_CAND_ROT] + row_dead.bytes[0][CAT_CAND_POS],
            16
        );
        assert_eq!(row_dead.bytes[0][CAT_OTHER], outside.len() as u32);
        assert_eq!(row_dead.non_draw(), outside.len() as u32 + 16);
    }

    #[test]
    fn diffs_outside_the_draw_fields_are_kept() {
        let (a1, mut a2) = pair([0; COUNT], [0; COUNT]);
        a2[3] = 9; // 枠の前
        a2[BASE + STRIDE * COUNT + 4] = 9; // 枠の後
        a2[slot(1) + 0x100] = 9; // 死んだ枠の中の別の欄
        set16(&mut a2, slot(3) + FLAG, 6); // 死んだ枠の旗が 0 → 6（同じ「死」の値でも、バイトが違えば食い違い）
        let (dead, row) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        assert_eq!(dead.diff.bytes, 4);
        assert_eq!(row.outside(), 2);
        assert_eq!(row.bytes[0][CAT_OTHER], 2);
        assert_eq!(row.non_draw(), 4);
    }

    #[test]
    fn alive_in_d1_and_d2_are_counted_apart() {
        let (a1, mut a2) = pair([0, 1, 6, 2], [1, 0, 6, 2]);
        for i in 0..COUNT {
            dirty_draw(&mut a2, i, 0x4);
        }
        dirty_draw(&mut a2, 0, 0x2A8);
        let (dead, row) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        let draw = |c: usize| row.bytes[c][CAT_ROT] + row.bytes[c][CAT_POS];
        assert_eq!((draw(0), draw(1), draw(2), draw(3)), (16, 32, 16, 16));
        assert_eq!(row.slots, [1, 1, 1, 1], "違いのあった枠の数（類ごと）");
        assert_eq!(row.first_slot, [2, 0, 1, 3], "最初の枠の番号");
        assert_eq!(row.bytes[1][CAT_OTHER], 1);
        assert_eq!(row.bytes[2][CAT_OTHER], 1);
        assert_eq!(dead.diff.bytes, 1 + 17 + 16);
        assert_eq!(row.live(), 33 + 17 + 16, "D1 か D2 で生きている枠の違い");
    }

    #[test]
    fn bit0_alive_rule_drives_the_skip() {
        let (mut a1, mut a2) = (vec![0u8; LEN], vec![0u8; LEN]);
        a1[slot(0) + FLAG] = 1;
        a2[slot(0) + FLAG] = 1;
        a1[slot(1) + FLAG + 3] = 0x80;
        a2[slot(1) + FLAG + 3] = 0x80;
        for i in 0..2 {
            for o in (0x10..0x14).chain(0x210..0x21C) {
                a2[slot(i) + o] = 1;
            }
        }
        let (dead, row) = run(&SPEC_BIT, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        assert_eq!(dead.diff.bytes, 16, "枠 0（生）の分だけ残る");
        assert_eq!(row.slots[3], 1);
        assert_eq!(row.slots[0], 1);
    }

    #[test]
    fn default_none_matches_the_plain_comparison_and_the_diagnosis_adds_up() {
        assert_eq!(SkipDraw::default(), SkipDraw::None);
        assert_eq!(
            skip_draw_from_text("[rollback_probe]\nenable = 1\n"),
            Ok(SkipDraw::None)
        );
        assert_eq!(
            skip_draw_from_text("[rollback_probe]\nenable = 1\nskip_draw_fields = dead\n"),
            Ok(SkipDraw::Dead)
        );
        assert_eq!(
            skip_draw_from_text("[rollback_probe]\nenable = 1\nskip_draw_fields = all\n"),
            Ok(SkipDraw::All)
        );
        for bad in ["both", "All", "", "alive"] {
            assert!(
                crate::rollback_probe::settings_from_text(&format!(
                    "[rollback_probe]\nenable = 1\nskip_draw_fields = {bad}\n"
                ))
                .is_err(),
                "{bad:?} を通した"
            );
        }
        for ok in ["none", "dead", "all"] {
            assert!(
                crate::rollback_probe::settings_from_text(&format!(
                    "[rollback_probe]\nenable = 1\nskip_draw_fields = {ok}\n"
                ))
                .is_ok(),
                "{ok:?} を断った"
            );
            assert_eq!(SkipDraw::parse(ok).map(SkipDraw::name), Ok(ok));
        }
        let (a1, mut a2) = pair([0, 1, 0, 1], [0, 1, 0, 1]);
        for i in 0..COUNT {
            dirty_draw(&mut a2, i, 0x2A8);
            a2[slot(i) + 0x300] = 7;
        }
        a2[5] = 3;
        a2[LEN - 1] = 4;
        let mut skips = Skips::EMPTY;
        skips.push((slot(1) + 0x300) as u32, (slot(1) + 0x304) as u32);
        let plain = RegionResult::of(&region(LEN), &a1, &a2, &skips);
        let (none, row) = run(&SPEC, &a1, &a2, &skips, SkipDraw::None);
        assert_eq!(none, plain, "既定 none の結果が今までと違う");
        assert_eq!(row.total, plain.diff.bytes, "診断の合計と違いのバイト数");
        assert_eq!(row.draw_skipped, 0);
        assert!(row.n_samples > 0);
        let (dead, _) = run(&SPEC, &a1, &a2, &skips, SkipDraw::Dead);
        assert_eq!(compare_bytes(&a1, &a2, &skips).diff.bytes, plain.diff.bytes);
        assert_eq!(plain.diff.bytes - dead.diff.bytes, 32);
    }

    #[test]
    fn length_gap_is_counted_like_compare_bytes() {
        let (a1, mut a2) = pair([0; COUNT], [0; COUNT]);
        a2.extend_from_slice(&[1; 10]);
        let mut skips = Skips::EMPTY;
        skips.push(LEN as u32 + 2, LEN as u32 + 5);
        let plain = compare_bytes(&a1, &a2, &skips);
        let (res, row) = run(&SPEC, &a1, &a2, &skips, SkipDraw::None);
        assert_eq!(res.diff.bytes, plain.diff.bytes);
        assert_eq!(row.total, plain.diff.bytes);
        assert_eq!(plain.diff.bytes, 7);
    }

    #[test]
    fn merged_skips_match_a_byte_by_byte_reference() {
        let mut x: u32 = 0x1234_5678;
        let mut rnd = |m: u32| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 8) % m
        };
        for _ in 0..200 {
            let len = 40 + rnd(60) as usize;
            let a1: Vec<u8> = (0..len).map(|_| rnd(3) as u8).collect();
            let a2: Vec<u8> = a1
                .iter()
                .map(|&b| if rnd(4) == 0 { b ^ 1 } else { b })
                .collect();
            let mut skips = Skips::EMPTY;
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            for _ in 0..rnd(4) {
                let s = rnd(len as u32);
                let e = s + rnd(8);
                skips.push(s, e);
                ranges.push((s, e));
            }
            let mut dynamic: Vec<(u32, u32)> = Vec::new();
            let mut at = 0u32;
            while at < len as u32 {
                at += rnd(12);
                let e = at + 1 + rnd(5);
                if at < len as u32 {
                    dynamic.push((at, e));
                }
                at = e;
            }
            let walk = |emit: &mut dyn FnMut(u32, u32)| {
                for &(s, e) in &dynamic {
                    emit(s, e);
                }
            };
            let got = crate::rollback_probe::compare_bytes_with(&a1, &a2, &skips, &walk);
            let skipped = |p: usize| {
                ranges
                    .iter()
                    .chain(&dynamic)
                    .any(|&(s, e)| (s as usize) <= p && p < e as usize)
            };
            let want_diff = (0..len).filter(|&p| !skipped(p) && a1[p] != a2[p]).count() as u32;
            let want_skipped = (0..len).filter(|&p| skipped(p)).count() as u32;
            assert_eq!(got.diff.bytes, want_diff, "{ranges:?} {dynamic:?}");
            assert_eq!(got.skipped, want_skipped, "{ranges:?} {dynamic:?}");
        }
        let (a1, a2) = (vec![1u8; 30], vec![2u8; 30]);
        assert_eq!(
            crate::rollback_probe::compare_bytes_with(&a1, &a2, &Skips::EMPTY, &no_extra_skips),
            compare_bytes(&a1, &a2, &Skips::EMPTY)
        );
    }

    struct VecMem(Vec<u8>);
    impl ArenaMem for VecMem {
        fn len(&self) -> u32 {
            self.0.len() as u32
        }
        fn read(&self, off: u32) -> u32 {
            word(&self.0, off as usize)
        }
        fn write(&mut self, off: u32, v: u32) {
            self.0[off as usize..off as usize + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    const TAG: u32 = 0x0040_CB17;

    fn arena() -> (VecMem, ArenaState, [u32; 4]) {
        let mut m = VecMem(vec![0; 1 << 14]);
        let mut s = ArenaState::new();
        let a1 = s.alloc_tagged(&mut m, 676, TAG).unwrap();
        let b = s.alloc_tagged(&mut m, 676, TAG).unwrap();
        let c = s.alloc_tagged(&mut m, 100, 0x1111).unwrap();
        let d = s.alloc_tagged(&mut m, 100, 0x2222).unwrap();
        s.free(&mut m, b).unwrap();
        s.free(&mut m, c).unwrap();
        m.0.truncate(s.top as usize);
        (m, s, [a1, b, c, d])
    }

    fn run_arena(
        a1: &[u8],
        a2: &[u8],
        st1: &ArenaState,
        st2: &ArenaState,
        mem2: &VecMem,
        mode: SkipDraw,
    ) -> (RegionResult, DiagRow, ArenaWalk) {
        let mut row = DiagRow::new(RowKind::Arena);
        let mut walk = ArenaWalk::NotRun;
        let res = compare_arena(
            &Region {
                kind: RegionKind::Arena,
                addr: 0x8000,
                len: a1.len() as u32,
                cap: 1 << 14,
                buf_off: 0,
            },
            a::ROLLBACK_ARENA_VM_TAGS,
            (a1, a2),
            &Skips::EMPTY,
            mode,
            (st1, st2, mem2),
            (&mut row, &mut walk),
        );
        (res, row, walk)
    }

    #[test]
    fn arena_skips_only_draw_fields_of_free_vm_sized_blocks() {
        let (m1, st1, [pa, pb, pc, pd]) = arena();
        let a1 = m1.0.clone();
        let mut a2 = a1.clone();
        for o in (pb + 0x8..pb + 0xC).chain(pb + 0x208..pb + 0x214) {
            a2[o as usize] = 0x55;
        }
        a2[(pb + 0x7) as usize] = 0x55;
        a2[(pb + 0x214) as usize] = 0x55;
        a2[(pb - 1) as usize] = 0x55; // 先頭の最後のバイト（列の次）
        a2[(pa + 0x8) as usize] = 0x66;
        a2[(pc + 8) as usize] = 0x77;
        a2[(pd + 8) as usize] = 0x88;
        let mem2 = VecMem(a2.clone());
        let (none, row_none, walk) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::None);
        assert_eq!(walk, ArenaWalk::Ok);
        let (dead, row, _) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::Dead);
        assert_eq!(none.diff.bytes, 16 + 2 + 1 + 1 + 1 + 1);
        assert_eq!(
            dead.diff.bytes,
            none.diff.bytes - 16,
            "外すのは空きの VM 大の塊の欄 16 バイトだけ"
        );
        assert_eq!(row.draw_skipped, VM_SKIP);
        assert_eq!(row_none.total, none.diff.bytes);
        let sum = |k: usize| (0..4).map(|c| row.bytes[c][k]).sum::<u32>();
        assert_eq!(sum(ACAT_FREE_VM_DRAW), 16);
        assert_eq!(sum(ACAT_FREE_VM_OTHER), 2);
        assert_eq!(sum(ACAT_HEADER), 1);
        assert_eq!(sum(ACAT_USED_VM_DRAW), 1);
        assert_eq!(sum(ACAT_FREE_OTHER), 1);
        assert_eq!(sum(ACAT_USED_OTHER), 1);
        assert_eq!(row.class_bytes(0), 16 + 2 + 1 + 1);
        assert_eq!(row.class_bytes(3), 1 + 1);
        assert_eq!(row.live(), 2, "生きている塊の違い");
        assert_eq!(row.draw(), 16 + 1);
    }

    #[test]
    fn broken_arena_headers_disable_the_skip_and_are_reported() {
        let (m1, st1, [_, pb, ..]) = arena();
        let mut a1 = m1.0.clone();
        let mut a2 = a1.clone();
        for o in pb + 0x8..pb + 0xC {
            a2[o as usize] = 0x55;
        }
        let d_head = a1.len() - 16 - 128;
        a1[d_head + 4] = 0xFF;
        a2[d_head + 4] = 0xFF;
        let mem2 = VecMem(a2.clone());
        let (dead, row, walk) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::Dead);
        assert_eq!(walk, ArenaWalk::Broken);
        assert_eq!(dead.diff.bytes, 4, "歩けなければ外さない");
        assert_eq!(row.draw_skipped, 0);
        let mut d = DrawDiag::EMPTY;
        d.valid = true;
        d.mode = SkipDraw::Dead;
        d.arena_walk = walk;
        let lines = trial_lines(&d);
        assert!(
            lines.iter().any(|l| l.contains("歩けなかった")),
            "{lines:?}"
        );
    }

    #[test]
    fn arena_blocks_alive_in_only_one_side_are_counted_apart() {
        let (m1, st1, [_, pb, ..]) = arena();
        let a1 = m1.0.clone();
        let mut a2 = a1.clone();
        let mut st2 = st1;
        let mut mem2 = VecMem(a2.clone());
        let again = st2.alloc_tagged(&mut mem2, 676, TAG).unwrap();
        assert_eq!(again, pb, "列から同じ塊を取る（前提）");
        a2.copy_from_slice(&mem2.0);
        a2[(pb + 0x8) as usize] = 0x55;
        let (dead, row, _) = run_arena(&a1, &a2, &st1, &st2, &mem2, SkipDraw::Dead);
        assert_eq!(row.bytes[1][ACAT_FREE_VM_DRAW], 1, "D1 死・D2 生");
        assert_eq!(row.slots[1], 1);
        assert_eq!(row.draw_skipped, VM_SKIP);
        assert!(
            dead.diff.bytes >= 2 && row.bytes[1][ACAT_HEADER] >= 2,
            "{row:?}"
        );
        assert_eq!(
            dead.diff.bytes,
            row.non_draw(),
            "外れたのは描画欄の 1 バイトだけ"
        );
    }

    #[test]
    fn all_skips_every_slots_draw_fields_but_not_world_fields() {
        let (a1, mut a2) = pair([1, 1, 0, 0], [1, 0, 1, 0]);
        for i in 0..COUNT {
            for vm in [0x4, 0x2A8] {
                dirty_draw(&mut a2, i, vm);
            }
        }
        a2[slot(0) + 0x300] = 9;
        a2[slot(2) + 0x300] = 9;
        a2[3] = 9;
        let draw_all = 4 * 2 * 16;
        let skip_all = 4 * 2 * VM_SKIP;
        let other = 2 + 1 + 2;
        let (none, row_none) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::None);
        let (dead, row_dead) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        let (all, row_all) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::All);
        assert_eq!(none.diff.bytes, draw_all + other);
        assert_eq!(
            dead.diff.bytes,
            draw_all + other - 2 * 16,
            "dead は D1 で死んでいる枠 2・3 の除外の VM だけ（候補 VM は外さない）"
        );
        assert_eq!(
            all.diff.bytes, other,
            "all で残るのは世界の欄・旗・枠の外だけ"
        );
        assert_eq!(all.skipped - none.skipped, skip_all);
        assert_eq!(row_all.draw_skipped, skip_all);
        assert_eq!(
            (row_none.draw_skipped, row_dead.draw_skipped),
            (0, 2 * VM_SKIP)
        );
        assert_eq!(row_none.total, draw_all + other);
        assert_eq!(row_dead.total, row_none.total);
        assert_eq!(row_all.total, row_none.total);
        assert_eq!(row_all.mode, SkipDraw::All);
        assert_eq!(row_all.draw(), draw_all);
        assert_eq!(row_all.non_draw(), other);
        assert_eq!(row_none.draw(), 4 * 16);
        assert_eq!(row_none.non_draw(), other + 4 * 16);
        assert_eq!(row_dead.non_draw(), row_none.non_draw());
        assert_eq!(row_all.live(), 3 * 32 + 2 + 2);
        assert_eq!(row_all.live_non_draw(), 4);
        assert_eq!(row_none.live_non_draw(), 4 + 3 * 16);
        assert_eq!(row_all.outside(), 1);
        let mut rec = TrialRecord::EMPTY;
        rec.draw.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::All);
        rec.draw.rows[0] = DiagRow {
            kind: RowKind::Slots(&SPEC_A),
            ..row_all
        };
        let joined = summary_lines(&[rec], SkipDraw::All).join("\n");
        assert!(joined.contains("skip_draw_fields = all"), "{joined}");
        assert!(
            joined.contains(&format!(
                "生きている枠の違い {} バイト / 描画用の欄の違い {draw_all} バイト / 描画用の欄以外の違い {other} バイト",
                3 * 32 + 4
            )),
            "{joined}"
        );
        assert!(
            joined.contains("生きている枠の描画欄以外の違い 4 バイト"),
            "{joined}"
        );
        assert!(
            joined.contains(&format!("描画用の欄を比べから外したバイト {skip_all}")),
            "{joined}"
        );
        assert!(
            joined.contains(&format!(
                "内訳: 枠の配列の分 {skip_all} バイト / arena の分 0 バイト（比べた回 1 回）"
            )),
            "{joined}"
        );
    }

    #[test]
    fn all_walk_merges_candidates_in_order() {
        let (a1, _) = pair([1, 0, 1, 0], [1, 0, 1, 0]);
        let collect = |spec: &'static RbSlotArray, mode| {
            let mut v = Vec::new();
            walk_slot_skips(spec, &a1, mode, &mut |s, e| v.push((s, e)));
            v
        };
        let s0 = slot(0) as u32;
        let f = |vm: u32| {
            [
                (s0 + vm + 0x8, s0 + vm + 0xC),
                (s0 + vm + 0x208, s0 + vm + 0x214),
                (s0 + vm + 0x238, s0 + vm + 0x244),
            ]
        };
        let want: Vec<(u32, u32)> = [0x4, 0x2A8].iter().flat_map(|&vm| f(vm)).collect();
        for spec in [&SPEC_A, &SPEC_B] {
            let all = collect(spec, SkipDraw::All);
            assert_eq!(all.len(), COUNT * 2 * 3);
            assert!(all.windows(2).all(|w| w[0].1 <= w[1].0), "{all:?}");
            assert_eq!(&all[..6], &want[..], "枠 0 の 2 本（0x4・0x2A8 の順）");
        }
        for (spec, vm) in [(&SPEC_A, 0x4u32), (&SPEC_B, 0x2A8)] {
            let dead = collect(spec, SkipDraw::Dead);
            assert_eq!(dead.len(), 2 * 3);
            assert!(dead.iter().all(|&(s, _)| {
                let i = (s as usize - BASE) / STRIDE;
                (i == 1 || i == 3) && (s as usize - BASE) % STRIDE >= vm as usize
            }));
            assert_eq!(dead[0].0, slot(1) as u32 + vm + 0x8);
            assert!(collect(spec, SkipDraw::None).is_empty());
        }
        let mut bit = Vec::new();
        walk_slot_skips(&SPEC_BIT, &a1, SkipDraw::All, &mut |s, e| bit.push((s, e)));
        assert_eq!(bit.len(), COUNT * 3);
    }

    #[test]
    fn interp_start_is_skipped_like_pos_but_its_neighbours_are_not() {
        let (a1, mut a2) = pair([1, 0, 1, 1], [1, 0, 1, 1]);
        for i in [0, 1] {
            for o in 0x4 + 0x238..0x4 + 0x244 {
                a2[slot(i) + o] = 1;
            }
        }
        for o in 0x2A8 + 0x238..0x2A8 + 0x244 {
            a2[slot(2) + o] = 1;
        }
        a2[slot(3) + 0x4 + 0x237] = 1;
        a2[slot(3) + 0x4 + 0x244] = 1;
        let (none, row_none) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::None);
        let (dead, row_dead) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::Dead);
        let (all, row_all) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::All);
        assert_eq!(none.diff.bytes, 12 * 3 + 2);
        assert_eq!(
            dead.diff.bytes,
            12 * 2 + 2,
            "dead は D1 で死んでいる枠 1 の始点 12 B だけ外す（生きている枠 0 と候補 VM は残る）"
        );
        assert_eq!(
            all.diff.bytes, 2,
            "all は除外の VM の 24 B と候補 VM の 12 B を外し、外の 2 B だけ残る"
        );
        for row in [&row_none, &row_dead, &row_all] {
            assert_eq!(row.total, 12 * 3 + 2);
            assert_eq!(row.bytes[3][CAT_INTERP], 12, "枠 0（生・生）");
            assert_eq!(row.bytes[0][CAT_INTERP], 12, "枠 1（死・死）");
            assert_eq!(row.bytes[3][CAT_CAND_INTERP], 12, "枠 2 の候補 VM");
            assert_eq!(
                row.bytes[3][CAT_OTHER], 2,
                "始点のすぐ外の 2 B は枠の中のほか"
            );
        }
        assert_eq!(row_none.draw(), 24);
        assert_eq!(row_all.draw(), 36);
        assert_eq!(row_all.non_draw(), 2);
        assert_eq!((row_none.draw_skipped, row_dead.draw_skipped), (0, VM_SKIP));
        assert_eq!(row_all.draw_skipped, 4 * 2 * VM_SKIP);
        let mut rec = TrialRecord::EMPTY;
        rec.draw.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::All);
        rec.draw.rows[0] = DiagRow {
            kind: RowKind::Slots(&SPEC_A),
            ..row_all
        };
        let lines = trial_lines(&rec.draw).join("\n");
        assert!(lines.contains("補間の始点 24 B"), "{lines}");
        assert!(lines.contains("候補 VM の補間の始点 12 B"), "{lines}");
    }

    #[test]
    fn interp_neighbours_stay_compared_under_all() {
        let (a1, mut a2) = pair([1, 1, 1, 1], [1, 1, 1, 1]);
        let near = [
            0x228u32, 0x234, 0x237, 0x244, 0x24B, 0x24C, 0x288, 0x28C, 0x1F8,
        ];
        for (i, &o) in near.iter().enumerate() {
            a2[slot(i % COUNT) + 0x4 + o as usize] ^= 1u8 << (i / COUNT);
        }
        let (all, row) = run(&SPEC_A, &a1, &a2, &Skips::EMPTY, SkipDraw::All);
        assert_eq!(all.diff.bytes, near.len() as u32, "始点の外側は外れない");
        assert_eq!(row.bytes[3][CAT_INTERP], 0);
        assert_eq!(row.draw(), 0);
    }

    #[test]
    fn all_skips_the_interp_start_of_used_vm_arrays_only() {
        let (m1, st1, [pa, pb, ..]) = arena();
        let a1 = m1.0.clone();
        let mut a2 = a1.clone();
        for o in pa + 0x238..pa + 0x244 {
            a2[o as usize] = 0x55; // 使用中の VM の配列 A の始点 12 B
        }
        a2[(pa + 0x237) as usize] = 0x55;
        a2[(pa + 0x244) as usize] = 0x55;
        a2[(pb + 0x238) as usize] = 0x66; // 空きの VM 大の塊 B の始点 1 B
        let mem2 = VecMem(a2.clone());
        let (none, _, _) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::None);
        let (dead, row_dead, _) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::Dead);
        let (all, row, _) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::All);
        assert_eq!(none.diff.bytes, 12 + 2 + 1);
        assert_eq!(dead.diff.bytes, 12 + 2, "dead は空きの塊 B の 1 B だけ外す");
        assert_eq!(
            all.diff.bytes,
            2 + 1,
            "all は A の始点 12 B だけ外し、B の 1 B と外の 2 B は残る"
        );
        let sum = |k: usize| (0..4).map(|c| row.bytes[c][k]).sum::<u32>();
        assert_eq!(sum(ACAT_USED_VM_DRAW), 12);
        assert_eq!(sum(ACAT_USED_VM_OTHER), 2);
        assert_eq!(sum(ACAT_FREE_VM_DRAW), 1);
        assert_eq!(row_dead.draw_skipped, VM_SKIP);
        assert_eq!(row.draw_skipped, VM_SKIP);
    }

    #[test]
    fn all_skips_only_draw_fields_of_used_vm_arrays() {
        let (m1, st1, [pa, pb, pc, pd]) = arena();
        let a1 = m1.0.clone();
        let mut a2 = a1.clone();
        for o in (pa + 0x8..pa + 0xC).chain(pa + 0x208..pa + 0x214) {
            a2[o as usize] = 0x55;
        }
        a2[(pa + 0x7) as usize] = 0x55;
        a2[(pa + 0x214) as usize] = 0x55;
        for o in pb + 0x8..pb + 0xC {
            a2[o as usize] = 0x66;
        }
        a2[(pc + 8) as usize] = 0x77;
        a2[(pd + 8) as usize] = 0x88;
        let mem2 = VecMem(a2.clone());
        let (none, row_none, _) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::None);
        let (dead, row_dead, _) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::Dead);
        let (all, row, walk) = run_arena(&a1, &a2, &st1, &st1, &mem2, SkipDraw::All);
        assert_eq!(walk, ArenaWalk::Ok);
        assert_eq!(none.diff.bytes, 16 + 2 + 4 + 1 + 1);
        assert_eq!(
            dead.diff.bytes,
            none.diff.bytes - 4,
            "dead は空きの VM 大の塊 B の 4 B だけ外す"
        );
        assert_eq!(
            all.diff.bytes,
            none.diff.bytes - 16,
            "all は使用中の VM 配列 A の描画欄 16 B だけ外す"
        );
        assert_eq!(
            (
                row_none.draw_skipped,
                row_dead.draw_skipped,
                row.draw_skipped
            ),
            (0, VM_SKIP, VM_SKIP)
        );
        let sum = |k: usize| (0..4).map(|c| row.bytes[c][k]).sum::<u32>();
        assert_eq!(sum(ACAT_USED_VM_DRAW), 16);
        assert_eq!(sum(ACAT_USED_VM_OTHER), 2);
        assert_eq!(sum(ACAT_FREE_VM_DRAW), 4);
        assert_eq!(sum(ACAT_FREE_OTHER), 1);
        assert_eq!(sum(ACAT_USED_OTHER), 1);
        assert_eq!(row.draw(), 16);
        assert_eq!(row.non_draw(), 8);
        assert_eq!(row_dead.draw(), 16 + 4);
        assert_eq!(row.live(), 16 + 2 + 1);
        assert_eq!(row.live_non_draw(), 3);
    }

    #[test]
    fn all_covers_every_vm_of_an_ex_array_and_needs_a_walkable_arena() {
        let mut m = VecMem(vec![0; 1 << 14]);
        let mut s = ArenaState::new();
        let ex = s.alloc_tagged(&mut m, 0xEC4, 0x0044_0DB9).unwrap();
        let plain = s.alloc_tagged(&mut m, a::VM_BYTES, 0x1234).unwrap();
        m.0.truncate(s.top as usize);
        let a1 = m.0.clone();
        let mut a2 = a1.clone();
        for vm in 0..5 {
            a2[(ex + vm * a::VM_BYTES + 0x8) as usize] = 1;
            a2[(ex + vm * a::VM_BYTES + 0x208) as usize] = 1;
        }
        a2[(ex + 5 * a::VM_BYTES + 0x8) as usize] = 1; // 追加（状態ブロック）の欄（6 本目の位置ではあるが、本数に入らない）
        a2[(plain + 0x8) as usize] = 1;
        let mem2 = VecMem(a2.clone());
        let (all, row, walk) = run_arena(&a1, &a2, &s, &s, &mem2, SkipDraw::All);
        assert_eq!(walk, ArenaWalk::Ok);
        assert_eq!(all.diff.bytes, 2, "残るのは追加の欄と札の違う塊");
        assert_eq!(row.draw_skipped, 5 * VM_SKIP);
        let mut b1 = a1.clone();
        let mut b2 = a2.clone();
        let plain_head = (plain - HEADER) as usize;
        b1[plain_head + 4] = 0xFF;
        b2[plain_head + 4] = 0xFF;
        let mem2b = VecMem(b2.clone());
        let (broken, row_b, walk_b) = run_arena(&b1, &b2, &s, &s, &mem2b, SkipDraw::All);
        assert_eq!(walk_b, ArenaWalk::Broken);
        assert_eq!(row_b.draw_skipped, 0);
        assert!(broken.diff.bytes >= 4, "歩けなければ外さない");
    }

    #[test]
    fn ex_vm_count_comes_from_the_table_not_from_floor() {
        let ex_tag = a::ROLLBACK_ARENA_EX_TAG;
        let run_one = |size: u32, vm: u32, within: u32| {
            let mut m = VecMem(vec![0; 1 << 14]);
            let mut s = ArenaState::new();
            let ex = s.alloc_tagged(&mut m, size, ex_tag).unwrap();
            m.0.truncate(s.top as usize);
            let a1 = m.0.clone();
            let mut a2 = a1.clone();
            a2[(ex + vm * a::VM_BYTES + within) as usize] = 1;
            let mem2 = VecMem(a2.clone());
            let (all, row, walk) = run_arena(&a1, &a2, &s, &s, &mem2, SkipDraw::All);
            assert_eq!(walk, ArenaWalk::Ok);
            (row.draw_skipped, all.diff.bytes, row.draw(), row.non_draw())
        };
        assert_eq!(run_one(0x11EC, 1, 0x208), (VM_SKIP, 1, 0, 1));
        assert_eq!(run_one(0x11EC, 0, 0x208), (VM_SKIP, 0, 1, 0));
        assert_eq!(run_one(0xEC4, 4, 0x8), (5 * VM_SKIP, 0, 1, 0));
        assert_eq!(run_one(0xEC4, 5, 0x8), (5 * VM_SKIP, 1, 0, 1));
        assert_eq!(run_one(0xEC4, 4, 0x238), (5 * VM_SKIP, 0, 1, 0));
        assert_eq!(run_one(0xEC4, 5, 0x238), (5 * VM_SKIP, 1, 0, 1));
        assert_eq!(run_one(0x11EC, 0, 0x238), (VM_SKIP, 0, 1, 0));
        assert_eq!(run_one(0x11EC, 1, 0x238), (VM_SKIP, 1, 0, 1));
        assert_eq!(run_one(0xBE8, 3, 0x8), (4 * VM_SKIP, 0, 1, 0));
        assert_eq!(run_one(0xBE8, 4, 0x8), (4 * VM_SKIP, 1, 0, 1));
        assert_eq!(run_one(2 * a::VM_BYTES + 100, 1, 0x208), (0, 1, 0, 1));
        assert_eq!(a::ex_vm_count(2 * a::VM_BYTES + 100), 0);
    }

    #[test]
    fn ex_vm_table_matches_the_call_sites() {
        for &(size, n) in a::ROLLBACK_EX_VM_COUNTS {
            assert!(n >= 1 && size > n * a::VM_BYTES, "{size:#x}");
            assert_eq!(a::ex_vm_count(size), n);
        }
        let sizes: Vec<u32> = a::ROLLBACK_EX_VM_COUNTS.iter().map(|x| x.0).collect();
        for (i, s) in sizes.iter().enumerate() {
            assert!(!sizes[..i].contains(s), "{s:#x} が二重");
        }
        for (extra, n) in [
            (0x4C, 1),
            (0x50, 1),
            (0x58, 1),
            (0x5C, 1),
            (0x190, 5),
            (0x158, 4),
            (0xF48, 1),
            (0x688, 1),
            (0x68C, 1),
        ] {
            assert_eq!(
                a::ex_vm_count(n * a::VM_BYTES + extra),
                n,
                "追加 {extra:#x}"
            );
        }
        assert_eq!(a::ROLLBACK_EX_VM_COUNTS.len(), 9);
    }

    /// `p?_bullets` の塊の大きさ（`ROLLBACK_BLOCKS`。`0x004150EB push`）。
    const P_LEN: usize = 0x25E1C0;
    const L_BASE: usize = 0x24D424;
    const L_STRIDE: usize = 0x59C;
    /// レーザー 1 本の中で、描画だけが書く欄（VM の回転 z・pos と、ほかの描画だけの欄）の（位置, 長さ）。計 71 バイト。
    const L_DRAW: [(usize, usize); 10] = [
        (0x8, 4),    // VM 0 の回転 z
        (0x208, 12), // VM 0 の pos
        (0x238, 12), // VM 0 の補間の始点
        (0x2AC, 4),  // VM 1 の回転 z（0x2A4 + 0x8）
        (0x2BC, 8),  // VM 1 の拡大
        (0x494, 4),  // VM 1 の色と α
        (0x49C, 1),  // VM 1 の旗の低位 1 B（描画 0x57 / 進め直し 0x17。th09_addr.rs の注）
        (0x4AC, 12), // VM 1 の pos（0x2A4 + 0x208）
        (0x4DC, 12), // VM 1 の補間の始点（0x2A4 + 0x238）
        (0x596, 2),  // u16
    ];

    fn laser(i: usize) -> usize {
        L_BASE + i * L_STRIDE
    }

    fn set_laser_alive(a: &mut [u8], i: usize, v: u32) {
        a[laser(i) + 0x584..laser(i) + 0x588].copy_from_slice(&v.to_le_bytes());
    }

    fn dirty_laser_draw(a2: &mut [u8], i: usize) {
        for &(o, l) in &L_DRAW {
            for k in o..o + l {
                a2[laser(i) + k] = 1;
            }
        }
    }

    fn p_pair() -> (Vec<u8>, Vec<u8>) {
        (vec![0u8; P_LEN], vec![0u8; P_LEN])
    }

    fn region_run(
        region_name: &str,
        (a1, a2): (&[u8], &[u8]),
        mode: SkipDraw,
    ) -> (RegionResult, DrawDiag) {
        let mut diag = DrawDiag::EMPTY;
        diag.begin(a::ROLLBACK_SLOT_ARRAYS, mode);
        let res = compare_region_slots(
            &region(a1.len()),
            (region_name, a::ROLLBACK_SLOT_ARRAYS),
            (a1, a2),
            &Skips::EMPTY,
            mode,
            &mut diag,
        )
        .expect("範囲を名指す枠の配列が表にある");
        (res, diag)
    }

    fn row_of(diag: &DrawDiag, label: &str) -> DiagRow {
        *diag
            .rows()
            .iter()
            .find(|r| matches!(r.kind, RowKind::Slots(s) if s.label == label))
            .unwrap_or_else(|| panic!("{label} の行が無い"))
    }

    fn excluded_in_laser(a1: &[u8], mode: SkipDraw, i: usize, label: &str) -> Vec<usize> {
        let spec = a::ROLLBACK_SLOT_ARRAYS
            .iter()
            .find(|s| s.label == label)
            .expect(label);
        let mut v = Vec::new();
        walk_slot_skips(spec, a1, mode, &mut |s, e| {
            for p in s as usize..e as usize {
                if (laser(i)..laser(i) + L_STRIDE).contains(&p) {
                    v.push(p - laser(i));
                }
            }
        });
        v
    }

    #[test]
    fn laser_all_excludes_exactly_the_draw_only_bytes() {
        let (mut a1, _) = p_pair();
        set_laser_alive(&mut a1, 1, 1); // レーザー 1 は生きている、0 は死んでいる
        let want: Vec<usize> = L_DRAW.iter().flat_map(|&(o, l)| o..o + l).collect();
        assert_eq!(want.len(), 71);
        assert_eq!(want.len() as u32, L_SKIP);
        for label in ["p1_lasers", "p2_lasers"] {
            for i in [0, 1] {
                assert_eq!(
                    excluded_in_laser(&a1, SkipDraw::All, i, label),
                    want,
                    "{label} {i} 番"
                );
            }
            assert!(excluded_in_laser(&a1, SkipDraw::None, 0, label).is_empty());
        }
        let kept: Vec<usize> = (0x548..0x596) // 世界の欄（生死 +0x584 を含む）
            .chain([0x598, 0x599, 0x59A, 0x59B]) // 世界の欄の残りと枠の末尾
            .chain([0x49D, 0x49E, 0x49F, 0x1F0, 0x0, 0x100, 0x2A4, 0x54]) // VM 1 の旗の上位 3 B・VM 0 の更新の欄・VM の先頭
            .chain([0x237, 0x244, 0x4DB, 0x4E8]) // 補間の始点のすぐ外（割り込みの控えの末尾・終点の先頭）。VM 0 と VM 1
            .collect();
        for o in kept {
            assert!(!want.contains(&o), "{o:#x} は外してはいけない欄");
        }
    }

    #[test]
    fn laser_dead_mode_skips_only_dead_lasers() {
        let (mut a1, _) = p_pair();
        set_laser_alive(&mut a1, 1, 1);
        set_laser_alive(&mut a1, 2, 0x100); // bit0 が無い = 死
        set_laser_alive(&mut a1, 3, 0x101); // bit0 がある = 生
        let want: Vec<usize> = L_DRAW.iter().flat_map(|&(o, l)| o..o + l).collect();
        for (i, dead) in [(0, true), (1, false), (2, true), (3, false)] {
            let got = excluded_in_laser(&a1, SkipDraw::Dead, i, "p1_lasers");
            if dead {
                assert_eq!(got, want, "死んでいるレーザー {i}");
            } else {
                assert!(got.is_empty(), "生きているレーザー {i} を外した: {got:?}");
            }
        }
        let (mut b1, mut b2) = p_pair();
        set_laser_alive(&mut b1, 1, 1);
        set_laser_alive(&mut b2, 1, 1);
        dirty_laser_draw(&mut b2, 1);
        dirty_laser_draw(&mut b2, 2);
        let (none, _) = region_run("p1_bullets", (&b1, &b2), SkipDraw::None);
        let (dead, dd) = region_run("p1_bullets", (&b1, &b2), SkipDraw::Dead);
        let (all, _) = region_run("p1_bullets", (&b1, &b2), SkipDraw::All);
        assert_eq!(
            (none.diff.bytes, dead.diff.bytes, all.diff.bytes),
            (2 * 71, 71, 0)
        );
        assert_eq!(row_of(&dd, "p1_lasers").draw_skipped, 47 * L_SKIP);
        assert_eq!(none.skipped, 0);
    }

    #[test]
    fn laser_flag_low_byte_is_draw_only_but_upper_bytes_stay() {
        let (mut a1, mut a2) = p_pair();
        set_laser_alive(&mut a1, 4, 1);
        set_laser_alive(&mut a2, 4, 1);
        a1[laser(4) + 0x49C] = 0x57;
        a2[laser(4) + 0x49C] = 0x17;
        let (none, _) = region_run("p1_bullets", (&a1, &a2), SkipDraw::None);
        let (all, da) = region_run("p1_bullets", (&a1, &a2), SkipDraw::All);
        assert_eq!(none.diff.bytes, 1);
        assert_eq!(
            all.diff.bytes, 0,
            "旗の低位 1 B だけの違いは all で一致する"
        );
        assert_eq!(row_of(&da, "p1_lasers").non_draw(), 0);
        a2[laser(4) + 0x49D] = 1;
        let (all, da) = region_run("p1_bullets", (&a1, &a2), SkipDraw::All);
        assert_eq!(all.diff.bytes, 1);
        let row = row_of(&da, "p1_lasers");
        assert_eq!((row.draw(), row.non_draw()), (1, 1));
        assert_eq!(row.bytes[3][CAT_OTHER], 1);
    }

    #[test]
    fn laser_diagnosis_splits_the_fields_and_all_keeps_the_world() {
        for (region_name, lasers) in [("p1_bullets", "p1_lasers"), ("p2_bullets", "p2_lasers")] {
            let (mut a1, mut a2) = p_pair();
            set_laser_alive(&mut a1, 3, 1);
            set_laser_alive(&mut a2, 3, 1);
            dirty_laser_draw(&mut a2, 3); // 71 B
            for o in [0x548, 0x554, 0x598, 0x599] {
                a2[laser(3) + o] = 9; // 世界の欄 4 B
            }
            a2[laser(3) + 0x584] = 3; // 生死の欄も世界の欄（bit0 は立ったまま、値は違う）+1 B
            for o in [0x49D, 0x1F0, 0x59A] {
                a2[laser(3) + o] = 9; // 旗の上位 3 B の 1 つ・VM 0 の更新の欄・末尾 = 枠の中のほか 3 B
            }
            let (none, dn) = region_run(region_name, (&a1, &a2), SkipDraw::None);
            let (dead, dd) = region_run(region_name, (&a1, &a2), SkipDraw::Dead);
            let (all, da) = region_run(region_name, (&a1, &a2), SkipDraw::All);
            assert_eq!(none.diff.bytes, 71 + 8);
            assert_eq!(
                dead.diff.bytes,
                71 + 8,
                "生きているレーザーは dead で外れない"
            );
            assert_eq!(all.diff.bytes, 8, "all で残るのは世界の欄・旗・生死・ほか");
            let row = row_of(&da, lasers);
            let by = |cat: usize| row.bytes[3][cat]; // 類 3 = D1 生・D2 生
            assert_eq!(by(CAT_ROT), 8, "回転 z（2 本の VM）");
            assert_eq!(by(CAT_POS), 12, "VM 0 の pos");
            assert_eq!(by(CAT_POS_VM1), 12, "VM 1 の pos");
            assert_eq!(
                by(CAT_INTERP),
                24,
                "補間の始点（2 本の VM。VM 0 と VM 1 を分けない）"
            );
            assert_eq!(
                by(CAT_DRAW_ONLY),
                15,
                "ほかの描画だけの欄（+0x596・色 α・拡大・旗の低位 1 B）"
            );
            assert_eq!(
                by(CAT_WORLD),
                5,
                "世界の欄（+0x548・+0x554・+0x584・+0x598・+0x599）"
            );
            assert_eq!(by(CAT_OTHER), 3, "枠の中のほか（+0x49D・+0x1F0・+0x59A）");
            assert_eq!(row.total, 79);
            assert_eq!(
                (row.draw(), row.non_draw(), row.live_non_draw()),
                (71, 8, 8)
            );
            assert_eq!(row.outside(), 0, "レーザーの行は枠の外を数えない");
            assert_eq!(row.draw_skipped, 48 * L_SKIP);
            for d in [&dn, &dd] {
                assert_eq!(row_of(d, lasers).total, 79);
            }
            assert_eq!(row_of(&da, region_name).total, 0);
            assert_eq!(row_of(&da, region_name).draw_skipped, 536 * 5 * VM_SKIP);
            assert_eq!(row_of(&dd, lasers).draw(), 71);
        }
    }

    #[test]
    fn diffs_outside_both_arrays_stay_and_are_counted_once() {
        let (a1, mut a2) = p_pair();
        a2[0x10] = 1; // 弾の枠の前
        a2[0x24C360 + 5] = 1; // 537 枠目の弾（弾の枠 0〜535 の外）
        a2[0x24D423] = 1; // レーザーの直前の 1 バイト
        a2[0x25E164] = 1; // レーザーの後ろの最初の 1 バイト
        a2[0x25E1BF] = 1; // 塊の最後の 1 バイト
        a2[0x24C360 - 1] = 1; // 弾の枠 535 の最後のバイト = 枠の中のほか
        a2[laser(47) + L_STRIDE - 1] = 1; // レーザー 47 の最後のバイト = 枠の中のほか
        a2[laser(0) + 0x100] = 1; // レーザー 0 の中
        for region_name in ["p1_bullets", "p2_bullets"] {
            let (bul, lsr) = if region_name == "p1_bullets" {
                ("p1_bullets", "p1_lasers")
            } else {
                ("p2_bullets", "p2_lasers")
            };
            for mode in [SkipDraw::None, SkipDraw::Dead, SkipDraw::All] {
                let (res, d) = region_run(region_name, (&a1, &a2), mode);
                assert_eq!(res.diff.bytes, 8, "{mode:?}: 外す欄に入らない違いが消えた");
                let (b, l) = (row_of(&d, bul), row_of(&d, lsr));
                assert_eq!(
                    b.outside(),
                    5,
                    "{mode:?}: 弾の行の枠の外（レーザーの分は抜ける）"
                );
                assert_eq!(b.total, 6, "{mode:?}: 枠の外 5 ＋ 弾の枠 535 の中 1");
                assert_eq!(l.outside(), 0);
                assert_eq!(
                    l.total, 2,
                    "{mode:?}: レーザー 47 の末尾 ＋ レーザー 0 の +0x100"
                );
                assert_eq!(
                    b.total + l.total,
                    res.diff.bytes,
                    "{mode:?}: 二重・取りこぼし"
                );
                assert_eq!((b.draw(), b.non_draw()), (0, 6));
                assert_eq!((l.draw(), l.non_draw()), (0, 2));
            }
        }
        let mut long = a2.clone();
        long.extend_from_slice(&[1; 7]);
        let mut d = DrawDiag::EMPTY;
        d.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::None);
        let res = compare_region_slots(
            &region(a1.len()),
            ("p1_bullets", a::ROLLBACK_SLOT_ARRAYS),
            (&a1, &long),
            &Skips::EMPTY,
            SkipDraw::None,
            &mut d,
        )
        .expect("表にある");
        assert_eq!(
            row_of(&d, "p1_bullets").total + row_of(&d, "p1_lasers").total,
            res.diff.bytes
        );
        assert_eq!(row_of(&d, "p1_lasers").outside(), 0);
    }

    #[test]
    fn laser_table_values_are_pinned() {
        let bullets = a::ROLLBACK_SLOT_ARRAYS
            .iter()
            .find(|s| s.label == "p1_bullets")
            .expect("弾");
        let bullets_end = bullets.base + bullets.stride * bullets.count;
        assert_eq!(bullets_end, 0x24C360, "弾の枠 0〜535 の終わり");
        for (label, region_name) in [("p1_lasers", "p1_bullets"), ("p2_lasers", "p2_bullets")] {
            let s = a::ROLLBACK_SLOT_ARRAYS
                .iter()
                .find(|s| s.label == label)
                .expect(label);
            assert_eq!(s.region, region_name);
            assert_eq!((s.base, s.stride, s.count), (0x24D424, 0x59C, 48));
            assert_eq!(s.alive, AliveRule::U32Bit0 { off: 0x584 });
            assert_eq!(s.vms, &[0x0, 0x2A4]);
            assert!(s.probe_vms.is_empty());
            assert_eq!(
                s.draw_only,
                &[(0x2BC, 8), (0x494, 4), (0x49C, 1), (0x596, 2)]
            );
            assert_eq!(s.world, &[(0x548, 0x52)]);
            assert!(s.pos_by_vm);
            let end = s.base + s.stride * s.count;
            assert_eq!(end, 0x25E164);
            assert!(bullets_end <= s.base, "弾の枠と重なる");
            assert_eq!(s.base - bullets_end, 0x10C4, "間は弾 1 枠分（537 枠目）");
            let blk = a::ROLLBACK_BLOCKS
                .iter()
                .find(|b| b.name == region_name)
                .expect("塊");
            assert_eq!(blk.size, 0x25E1C0);
            assert_eq!(blk.size - end, 0x5C, "レーザーの後ろの尻尾");
            for off in [0x49Du32, 0x49E, 0x49F, 0x584, 0x548, 0x554, 0x598, 0x599] {
                assert!(
                    !s.draw_only.iter().any(|&(o, l)| off >= o && off < o + l),
                    "{off:#x} が描画だけの欄に入っている"
                );
                assert_eq!(
                    slot_cat(s, off),
                    if (0x49D..=0x49F).contains(&off) {
                        CAT_OTHER
                    } else {
                        CAT_WORLD
                    }
                );
            }
            assert_eq!(slot_cat(s, 0x49C), CAT_DRAW_ONLY);
            assert_eq!(slot_cat(s, 0x208), CAT_POS);
            assert_eq!(slot_cat(s, 0x4AC), CAT_POS_VM1);
            assert_eq!((slot_cat(s, 0x8), slot_cat(s, 0x2AC)), (CAT_ROT, CAT_ROT));
            for off in [0x238u32, 0x243, 0x4DC, 0x4E7] {
                assert_eq!(slot_cat(s, off), CAT_INTERP, "{off:#x}");
            }
            for off in [0x237u32, 0x244, 0x4DB, 0x4E8] {
                assert_ne!(slot_cat(s, off), CAT_INTERP, "{off:#x}");
            }
            for off in [0x2BCu32, 0x2C3, 0x494, 0x497, 0x596, 0x597] {
                assert_eq!(slot_cat(s, off), CAT_DRAW_ONLY, "{off:#x}");
            }
            for off in [0x2C4u32, 0x498, 0x595, 0x598, 0x59A] {
                assert_ne!(slot_cat(s, off), CAT_DRAW_ONLY, "{off:#x}");
            }
        }
        for s in a::ROLLBACK_SLOT_ARRAYS {
            if !s.label.ends_with("_lasers") {
                assert!(
                    s.draw_only.is_empty() && s.world.is_empty() && !s.pos_by_vm,
                    "{}",
                    s.label
                );
            }
        }
        let order: Vec<&str> = a::ROLLBACK_SLOT_ARRAYS.iter().map(|s| s.label).collect();
        let at = |l: &str| order.iter().position(|&x| x == l).expect(l);
        assert!(at("p1_bullets") < at("p1_lasers") && at("p2_bullets") < at("p2_lasers"));
    }

    #[test]
    fn all_skip_bytes_per_compare_counts_the_lasers() {
        let table = a::ROLLBACK_SLOT_ARRAYS;
        let per = |pred: &dyn Fn(&RbSlotArray) -> bool| -> u32 {
            table
                .iter()
                .filter(|s| pred(s))
                .map(|s| {
                    let mut one = [(0u32, 0u32); MAX_SLOT_RANGES];
                    let n = slot_ranges(s, SkipDraw::All, &mut one);
                    one[..n].iter().map(|&(x, y)| y - x).sum::<u32>() * s.count
                })
                .sum()
        };
        assert_eq!(536 * 5 * 2 + 128 * 2 * 2 + 128 * 2, 6_128);
        assert_eq!(per(&|s| !s.label.ends_with("_lasers")), 6_128 * VM_SKIP);
        assert_eq!(per(&|s| !s.label.ends_with("_lasers")), 171_584);
        assert_eq!(per(&|s| s.label.ends_with("_lasers")), 2 * 48 * L_SKIP);
        assert_eq!(per(&|s| s.label.ends_with("_lasers")), 6_816);
        assert_eq!(all_skip_bytes_per_compare(table), 178_400);
        let (a1, a2) = p_pair();
        let mut total = 0u32;
        for region_name in ["p1_bullets", "p2_bullets"] {
            let (res, d) = region_run(region_name, (&a1, &a2), SkipDraw::All);
            total += res.skipped;
            assert_eq!(res.diff.bytes, 0);
            let lasers_row = d
                .rows()
                .iter()
                .find(|r| matches!(r.kind, RowKind::Slots(s) if s.region == region_name && s.label.ends_with("_lasers")))
                .expect("レーザーの行");
            assert_eq!(lasers_row.draw_skipped, 48 * L_SKIP);
        }
        assert_eq!(total, 2 * (536 * 5 * VM_SKIP + 48 * L_SKIP));
        let mut rec = TrialRecord::EMPTY;
        rec.draw.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::All);
        let all = summary_lines(&[rec], SkipDraw::All).join("\n");
        assert!(
            all.contains("表から数えた期待 178400 バイト × 1 回 = 178400 バイト"),
            "{all}"
        );
        let mut rec2 = TrialRecord::EMPTY;
        rec2.draw.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::Dead);
        let dead = summary_lines(&[rec2], SkipDraw::Dead).join("\n");
        assert!(!dead.contains("表から数えた期待"), "{dead}");
    }

    #[test]
    fn region_compare_does_not_allocate() {
        let (mut a1, mut a2) = p_pair();
        set_laser_alive(&mut a1, 2, 1);
        dirty_laser_draw(&mut a2, 2);
        dirty_laser_draw(&mut a2, 5);
        a2[0x24D423] = 1;
        a2[laser(7) + 0x548] = 1;
        let mut diag = DrawDiag::EMPTY;
        let r = region(P_LEN);
        for mode in [SkipDraw::None, SkipDraw::Dead, SkipDraw::All] {
            diag.begin(a::ROLLBACK_SLOT_ARRAYS, mode);
            let before = crate::test_alloc::count();
            let res = compare_region_slots(
                &r,
                ("p1_bullets", a::ROLLBACK_SLOT_ARRAYS),
                (&a1, &a2),
                &Skips::EMPTY,
                mode,
                &mut diag,
            );
            let per = all_skip_bytes_per_compare(a::ROLLBACK_SLOT_ARRAYS);
            assert_eq!(
                crate::test_alloc::count(),
                before,
                "{mode:?}: 比べの中で確保した"
            );
            assert!(res.is_some() && per == 178_400);
        }
    }

    #[test]
    fn laser_trial_line_lists_bytes_per_field_beyond_the_sample_limit() {
        let (mut a1, mut a2) = p_pair();
        set_laser_alive(&mut a1, 5, 1);
        set_laser_alive(&mut a2, 5, 1);
        dirty_laser_draw(&mut a2, 5);
        for o in [0x548, 0x554, 0x598, 0x599, 0x584] {
            a2[laser(5) + o] = 9;
        }
        for o in [0x49D, 0x1F0, 0x59A] {
            a2[laser(5) + o] = 9;
        }
        let (_, d) = region_run("p1_bullets", (&a1, &a2), SkipDraw::All);
        let lines = trial_lines(&d);
        let l = lines
            .iter()
            .find(|l| l.contains("診断 p1_lasers:"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        for want in [
            "違い 79 B",
            "VM 0 の pos 12 B",
            "VM 1 の pos 12 B",
            "回転 z 8 B",
            "ほかの描画だけの欄 15 B",
            "世界の欄 5 B",
            "枠の中の描画欄の外 3 B",
            "描画用の欄の違い 71 B・描画用の欄以外の違い 8 B",
            "補間の始点 24 B",
            "描画欄を比べから外した 3408 B",
        ] {
            assert!(l.contains(want), "{want:?} が無い: {l}");
        }
        assert!(
            !lines.iter().any(|l| l.contains("診断 p1_bullets:")),
            "違いの無い行は出さない: {lines:?}"
        );
        assert!(d.rows().iter().all(|r| r.n_samples as usize <= MAX_SAMPLES));
    }

    #[test]
    fn diag_rows_follow_the_table() {
        let mut d = DrawDiag::EMPTY;
        d.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::Dead);
        assert!(d.valid && d.mode == SkipDraw::Dead);
        assert_eq!(d.rows().len(), a::ROLLBACK_SLOT_ARRAYS.len() + 1);
        assert!(a::ROLLBACK_SLOT_ARRAYS.len() < MAX_ROWS);
        for (i, spec) in a::ROLLBACK_SLOT_ARRAYS.iter().enumerate() {
            assert!(
                !a::ROLLBACK_SLOT_ARRAYS[..i]
                    .iter()
                    .any(|s| s.label == spec.label),
                "{} が二重",
                spec.label
            );
            let row = d.row_of_label(spec.label).expect(spec.label);
            assert_eq!(row.kind, RowKind::Slots(spec));
        }
        for label in [
            "p1_bullets",
            "p2_bullets",
            "p1_lasers",
            "p2_lasers",
            "p1_enemies",
            "p2_enemies",
            "p1_player_shots",
            "p2_player_shots",
        ] {
            assert!(d.row_of_label(label).is_some(), "{label} の行が無い");
        }
        assert!(d.row_of_label("globals").is_none());
        assert!(arena_row(&mut d).is_some());
    }

    #[test]
    fn slot_table_is_consistent() {
        for s in a::ROLLBACK_SLOT_ARRAYS {
            let blk = a::ROLLBACK_BLOCKS
                .iter()
                .find(|b| b.name == s.region)
                .unwrap_or_else(|| panic!("{} の範囲が無い", s.region));
            assert!(s.base + s.stride * s.count <= blk.size, "{}", s.label);
            for vms in [s.vms, s.probe_vms] {
                assert!(
                    vms.windows(2).all(|w| w[0] + a::VM_BYTES <= w[1]),
                    "{} の VM の順",
                    s.label
                );
                for &vm in vms {
                    assert!(
                        vm + a::VM_INTERP_START_OFF + a::VM_INTERP_START_LEN <= s.stride,
                        "{}",
                        s.label
                    );
                }
            }
            let mut all: Vec<u32> = s.vms.iter().chain(s.probe_vms).copied().collect();
            for (i, x) in all.iter().enumerate() {
                assert!(!all[..i].contains(x), "{} の VM が二重", s.label);
            }
            all.sort_unstable();
            assert!(
                all.windows(2).all(|w| w[0] + a::VM_BYTES <= w[1]),
                "{} の VM（除外と候補）が重なる",
                s.label
            );
            for list in [s.draw_only, s.world] {
                for &(o, l) in list {
                    assert!(l > 0 && o + l <= s.stride, "{} の欄 {o:#x}+{l}", s.label);
                }
            }
            assert!(
                s.draw_only.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0),
                "{} の描画だけの欄の順",
                s.label
            );
            let mut rel = [(0u32, 0u32); MAX_SLOT_RANGES];
            let n = slot_ranges(s, SkipDraw::All, &mut rel);
            assert_eq!(
                n,
                (s.vms.len() + s.probe_vms.len()) * FIELDS.len() + s.draw_only.len(),
                "{} の欄が置き場に入りきらない / 数が合わない",
                s.label
            );
            assert!(
                rel[..n].iter().all(|&(st, e)| st < e && e <= s.stride),
                "{}",
                s.label
            );
            assert!(
                rel[..n].windows(2).all(|w| w[0].1 <= w[1].0),
                "{} の除外の欄が重なる・小さい順でない: {:?}",
                s.label,
                &rel[..n]
            );
        }
        for (i, s) in a::ROLLBACK_SLOT_ARRAYS.iter().enumerate() {
            for t in &a::ROLLBACK_SLOT_ARRAYS[i + 1..] {
                if t.region == s.region {
                    assert!(
                        s.base + s.stride * s.count <= t.base,
                        "{} と {} の順・重なり",
                        s.label,
                        t.label
                    );
                }
            }
        }
        let mut buf = vec![0u8; 0x25E1C0];
        for i in 0..536 {
            let p = 0x1A900 + i * 0x10C4 + 0xDBE;
            buf[p] = if i % 3 == 0 { 1 } else { 0 };
        }
        for spec in a::ROLLBACK_SLOT_ARRAYS {
            for mode in [SkipDraw::Dead, SkipDraw::All] {
                let mut last = 0u32;
                let mut n = 0u32;
                walk_slot_skips(spec, &buf, mode, &mut |s, e| {
                    assert!(s >= last && e > s, "{} {mode:?}", spec.label);
                    last = e;
                    n += 1;
                });
                assert!(n > 0, "{} {mode:?} が何も渡さない", spec.label);
            }
        }
    }

    #[test]
    fn summary_reports_counts_with_n_and_says_unmeasured_for_zero() {
        let none = summary_lines(&[], SkipDraw::None);
        assert_eq!(none.len(), 1);
        assert!(
            none[0].contains("比べた回が 0 回") && none[0].contains("測れていない"),
            "{none:?}"
        );
        let (a1, mut a2) = pair([0, 1, 0, 0], [0, 1, 0, 0]);
        dirty_draw(&mut a2, 0, 0x4); // 死んだ枠の描画欄 16 B
        dirty_draw(&mut a2, 1, 0x4); // 生きた枠の描画欄 16 B
        a2[slot(2) + 0x100] = 1; // 死んだ枠の欄の外 1 B
        let (_, row) = run(&SPEC, &a1, &a2, &Skips::EMPTY, SkipDraw::None);
        let mut rec = TrialRecord::EMPTY;
        rec.draw.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::None);
        rec.draw.rows[0] = DiagRow {
            kind: RowKind::Slots(&SPEC),
            ..row
        };
        let mut clean = TrialRecord::EMPTY;
        clean.draw.begin(a::ROLLBACK_SLOT_ARRAYS, SkipDraw::None);
        let lines = summary_lines(&[rec, clean, TrialRecord::EMPTY], SkipDraw::None);
        let joined = lines.join("\n");
        assert!(
            joined.contains("比べた回 2 回のうち違いのあった回 1 回"),
            "{joined}"
        );
        assert!(
            joined.contains("生きている枠の違い 16 バイト / 描画用の欄の違い 32 バイト / 描画用の欄以外の違い 1 バイト"),
            "{joined}"
        );
        assert!(
            joined.contains(
                "生きている枠の描画欄以外の違い 0 バイト（上の「生きている枠の違い」16 バイトのうち"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("生きている枠の違い 16 B（うち描画欄以外 0 B）"),
            "{joined}"
        );
        assert!(joined.contains("t_bullets: 違い 33 B"), "{joined}");
        let per_trial = trial_lines(&rec.draw);
        assert!(
            per_trial[0].contains("t_bullets: 違い 33 B"),
            "{per_trial:?}"
        );
        assert!(
            trial_lines(&TrialRecord::EMPTY.draw).is_empty(),
            "走っていない回は何も出さない"
        );
    }

    #[test]
    fn slot_values_appear_in_the_note() {
        let start = NOTE.find("## 18. ").expect("ノートに §18 が無い");
        let sec = &NOTE[start..];
        let row = |name: &str| {
            sec.lines()
                .find(|l| l.starts_with(&format!("| `{name}` ")))
                .unwrap_or_else(|| panic!("ノート §18 の表に {name} の行が無い"))
        };
        for s in a::ROLLBACK_SLOT_ARRAYS {
            if s.label.ends_with("_lasers") {
                continue;
            }
            let l = row(s.label);
            let hexo = |v: u32| format!("`0x{v:X}`");
            assert!(l.contains(&hexo(s.base)), "{}: {l}", s.label);
            assert!(l.contains(&hexo(s.stride)), "{}: {l}", s.label);
            assert!(l.contains(&format!("`{}`", s.count)), "{}: {l}", s.label);
            assert!(l.contains(&format!("`{}`", s.region)), "{}: {l}", s.label);
            match s.alive {
                AliveRule::U16NotIn { off, dead } => {
                    assert!(l.contains(&format!("u16 `+0x{off:X}`")), "{}: {l}", s.label);
                    let d = dead
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("・");
                    assert!(l.contains(&format!("死 {d}")), "{}: {l}", s.label);
                }
                AliveRule::U32Bit0 { off } => {
                    assert!(
                        l.contains(&format!("u32 `+0x{off:X}` の bit0")),
                        "{}: {l}",
                        s.label
                    );
                }
            }
            let (excluded, probe_only) = l.split_once("診断だけ").unwrap_or((l, ""));
            assert!(excluded.contains("除外"), "{}: {l}", s.label);
            for &vm in s.vms {
                let after = &excluded[excluded.find("除外").unwrap_or(0)..];
                assert!(after.contains(&format!("`+0x{vm:X}`")), "{}: {l}", s.label);
            }
            assert_eq!(
                probe_only.matches("`+0x").count(),
                s.probe_vms.len(),
                "{}: {l}",
                s.label
            );
            for &vm in s.probe_vms {
                assert!(
                    l.contains(&format!("診断だけ `+0x{vm:X}`")),
                    "{}: {l}",
                    s.label
                );
            }
        }
        for (name, v) in [
            ("VM_BYTES", a::VM_BYTES),
            ("VM_ROT_Z_OFF", a::VM_ROT_Z_OFF),
            ("VM_ROT_Z_LEN", a::VM_ROT_Z_LEN),
            ("VM_POS_OFF", a::VM_POS_OFF),
            ("VM_POS_LEN", a::VM_POS_LEN),
            ("VM_INTERP_START_OFF", a::VM_INTERP_START_OFF),
            ("VM_INTERP_START_LEN", a::VM_INTERP_START_LEN),
        ] {
            let l = row(name);
            assert!(
                l.contains(&format!("`0x{v:X}`")) || l.contains(&format!("`{v}`")),
                "{name}: {l}"
            );
        }
        for t in a::ROLLBACK_ARENA_VM_TAGS {
            assert!(sec.contains(&format!("`0x{t:08X}`")), "札 {t:08X}");
        }
    }
}
