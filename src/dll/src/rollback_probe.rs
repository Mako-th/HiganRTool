//! rollback の可否実験の判断（pure。unsafe 無し。Win32・ゲームのメモリ・入口のアセンブリは `rollback_win.rs`）。
//!
//! 問い: マッチの中のある tick T の始まりで状態をコピー、k tick 進めてからコピーを書き戻し、同じ入力で k tick 進め直すと、同じ状態に着くか。
//! 手順（1 回ぶん。どれも入力の地点 = tick の始まりで、ゲームのスレッドで）:
//! 1. マッチに入って `start_seconds` 秒（その後は `gap_ticks`）経った tick T の始まり: 範囲の表（決まった塊・EffectSys の要素の配列・.data の範囲・
//!    DLL の領域と割り当て器の状態）を作り、コピー S を取る。台本はこの tick から普通に feed し、そのワード（1P・2P）を k 個記録する。
//! 2. T + k の始まり: 期間の中で「書き戻すと壊れること」が起きていないかを見る。起きていれば書き戻さずに捨てる。起きていなければ今の状態を
//!    D1 としてコピー、S を書き戻し、記録した 1 個目のワードを feed する（台本は進めない）。続く k − 1 tick は記録したワードを順に feed する。
//! 3. 次の始まり（D2）: D1 のコピーと今の状態を、範囲ごとにダイジェストで比べ、違った範囲だけバイトで比べる。
//!
//! 捨てる条件（書き戻した状態が「もう無い物」を指すか、状態の外と食い違う）: 期間の中でゲームのスレッドが DLL の領域の外の塊（CRT の塊）を
//! 解放した / 別のスレッドが DLL の領域の塊を解放しようとした・領域の塊の解放が壊れていた・領域が足りず CRT へ回した確保があった /
//! 更新の列・描画の列の並びが変わった（ScreenEffect の登録と解除。列のつなぎはレジストリにありコピーしていない）/ 決まった塊のポインタ・
//! EffectSys の要素の数が変わった / マッチを出た / 台本が feed しなかった tick がある。
//!
//! この段はゲームの経路の判断だけを持つ。確保・ファイル・ロック待ち・パニックを置かない（`Probe` の中身は固定長）。
#![forbid(unsafe_code)]

use higanr_core::arena::{ArenaMem, ArenaState, STATE_WORDS};
use higanr_core::digest::{DIGEST_INIT, Diff, PAGE, diff, digest_continue};
use higanr_core::ini;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::rollback_draw::{self, DrawDiag, SkipDraw};
use crate::rollback_resim::{self, ResimMode, ResimRec};

// ---- 表の型（値は `th09_addr` の E1-2 の段階）----

/// 決まった塊（ポインタの置き場と大きさ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbBlock {
    pub name: &'static str,
    pub ptr_at: u32,
    pub size: u32,
    pub source: &'static str,
}

/// EffectSys の要素の配列（EffectSys のポインタの置き場と、要素の数・足し分の想定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbEffect {
    pub name: &'static str,
    pub sys_ptr_at: u32,
    pub count: u32,
    pub extra: u32,
    pub source: &'static str,
}

/// 塊の中のポインタと本数で決まる VM の配列（陣の背景の `bgscroll`）。読み込みのスレッドが CRT から取るので塊のコピーに入らず、
/// 本数は読み込みの後に決まる。コピーの置き場は `max` 本ぶんを取り、回ごとに `[[owner]+ptr_off]` から `[[owner]+count_off]` 本
/// （i32）だけをコピーする。本数が 0 以下・`max` を越える・ポインタが 0 ならコピーしない（長さ 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbVmArray {
    pub name: &'static str,
    pub owner: &'static str,
    pub ptr_off: u32,
    pub count_off: u32,
    pub elem: u32,
    /// コピーする本数の上限（置き場の大きさ）。
    pub max: u32,
    pub source: &'static str,
}

/// VM の配列の今の形（`vm_array_shape`）。`len` はコピーするバイト数（コピーしないときは 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmShape {
    /// 塊の欄のポインタ（読めた値。コピーしないときも残す —— 測る入口が上限を越えた陣も読む）。
    pub addr: u32,
    /// 塊の欄の本数（読めた値。負もありうる）。
    pub count: i32,
    pub len: u32,
}

impl VmShape {
    pub fn over(&self, v: &RbVmArray) -> bool {
        self.count > 0 && self.count as u32 > v.max
    }

    pub fn place(&self) -> (u32, u32) {
        if self.len == 0 {
            (0, 0)
        } else {
            (self.addr, self.len)
        }
    }
}

/// VM の配列 `v` の今のアドレス・本数・コピーする長さを読む。持ち主の塊のポインタが読めない・0 なら断る。配列のポインタ・本数が読めない・
/// 0・上限を越えるときは長さ 0（コピーしない）を返す: 背景の VM は見た目だけ（乱数の変数を使う ANM は `etama.anm` だけで、
/// `world*.anm` は 0 件）なので、コピーできない陣は直す前と同じ形に残し、巻き戻しは止めない。
pub fn vm_array_shape(
    t: &Tables,
    v: &RbVmArray,
    io: &mut dyn LayoutIo,
    idx: u8,
) -> Result<VmShape, LayoutError> {
    let owner_at = t
        .blocks
        .iter()
        .find(|b| b.name == v.owner)
        .map(|b| b.ptr_at)
        .ok_or(LayoutError::Unreadable(idx))?;
    let owner = io.read_u32(owner_at).ok_or(LayoutError::Unreadable(idx))?;
    if owner == 0 {
        return Err(LayoutError::Null(idx));
    }
    let rd = |io: &mut dyn LayoutIo, off: u32| owner.checked_add(off).and_then(|a| io.read_u32(a));
    let (Some(addr), Some(count)) = (rd(io, v.ptr_off), rd(io, v.count_off)) else {
        return Ok(VmShape {
            addr: 0,
            count: 0,
            len: 0,
        });
    };
    let count = count as i32;
    let len = if addr == 0 || count <= 0 || count as u32 > v.max {
        0
    } else {
        // count ≤ max なので、置き場の大きさ（max × elem。`Layout::plan` が溢れを断った）を越えない。
        count as u32 * v.elem
    };
    Ok(VmShape { addr, count, len })
}

/// コピーする .data の範囲 `[start, end)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbData {
    pub name: &'static str,
    pub start: u32,
    pub end: u32,
    pub source: &'static str,
}

/// DLL の領域へ向けない呼び出し元の範囲 `[start, end)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbCallers {
    pub start: u32,
    pub end: u32,
    pub why: &'static str,
}

/// 比べない所の 1 行（コピーと書き戻しはする。D1 と D2 の比べからだけ外す）。見分け方は `kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RbSkip {
    pub name: &'static str,
    pub kind: RbSkipKind,
    pub source: &'static str,
}

/// 比べない所の見分け方。どれも「外すのは fps の値が入る所だけ」の粒度で書く（範囲を丸ごと外すと本物のずれを隠す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RbSkipKind {
    /// ゲームのアドレスの範囲 `[start, end)`（中の語が全部その値の欄のとき）。
    Range { start: u32, end: u32 },
    /// 文字の列 `list` の `count` 本（1 本 `stride` バイト）のうち、D1 と D2 の両方で `+pos_off` の 3 語が `pos` と同じ
    /// 枠の、先頭から `text` バイト（文字の欄）。何番目の枠に入るかが回ごとに変わる文字を、位置の欄で見分ける。
    TextAt {
        list: u32,
        stride: u32,
        count: u32,
        text: u32,
        pos_off: u32,
        pos: [u32; 3],
    },
    /// DLL の領域の中で、札（確保の呼び出し元）が `callers` のどれかの塊のうち、書き位置 `[[ptr_at] + cursor_off]` が
    /// S の始まりから D1・D2 の始まりまでに進んだバイト（`cursor_span`）。書き位置は `Probe` が 3 つの始まりで読む（`Marks`）。
    Cursor {
        callers: &'static [u32],
        ptr_at: u32,
        cursor_off: u32,
    },
}

/// コピーする範囲の表（本物は `Tables::th09()`、検査では小さな偽物）。
#[derive(Debug, Clone, Copy)]
pub struct Tables {
    pub blocks: &'static [RbBlock],
    pub effects: &'static [RbEffect],
    /// 塊の中のポインタと本数で決まる VM の配列（製品の経路だけ。E1 は空 —— E1 の実験の入口の挙動を変えない）。
    pub vm_arrays: &'static [RbVmArray],
    pub data: &'static [RbData],
    /// 比べない所（`MAX_SKIP_ROWS` 行まで）。
    pub skips: &'static [RbSkip],
    /// EffectSys の中の要素の配列のポインタ・数・足し分の位置と、要素の大きさ。
    pub effect_ptr: u32,
    pub effect_count: u32,
    pub effect_extra: u32,
    pub effect_elem: u32,
}

impl Tables {
    pub fn th09() -> Self {
        Self::th09_with(crate::th09_addr::ROLLBACK_DATA, &[])
    }

    /// 製品の経路（`rollback_play`）の表。E1 の表との違いは、.data に更新の列のつなぎ（レジストリの先頭の 2 つの節 0x40 B）を足し、
    /// fps の計測の 6 語を外し（`th09_addr::ROLLBACK_PLAY_DATA`）、陣の背景の VM の配列（`th09_addr::ROLLBACK_VM_ARRAYS`）を足すこと。
    pub fn th09_play() -> Self {
        Self::th09_with(
            crate::th09_addr::ROLLBACK_PLAY_DATA,
            crate::th09_addr::ROLLBACK_VM_ARRAYS,
        )
    }

    fn th09_with(data: &'static [RbData], vm_arrays: &'static [RbVmArray]) -> Self {
        use crate::th09_addr as a;
        Tables {
            blocks: a::ROLLBACK_BLOCKS,
            effects: a::ROLLBACK_EFFECT_ARRAYS,
            vm_arrays,
            data,
            skips: a::ROLLBACK_COMPARE_SKIPS,
            effect_ptr: a::EFFECT_ARRAY_PTR,
            effect_count: a::EFFECT_ARRAY_COUNT,
            effect_extra: a::EFFECT_ARRAY_EXTRA,
            effect_elem: a::EFFECT_ELEM_BYTES,
        }
    }

    fn effect_bytes(&self, e: &RbEffect) -> Option<u32> {
        e.count
            .checked_add(e.extra)?
            .checked_add(1)?
            .checked_mul(self.effect_elem)
    }
}

// ---- ini ----

/// 節の名前（`HiganRTool.ini`。`[alloc_count]` と同じファイル）。
pub const SECTION: &str = "rollback_probe";
const KEYS: [&str; 8] = [
    "enable",
    "start_seconds",
    "ticks",
    "trials",
    "gap_ticks",
    "arena_mib",
    "resim",
    rollback_draw::INI_KEY,
];
/// k の値の数の上限と、k の上限（記録する語の置き場の大きさ。600 tick = 10 秒）。
pub const MAX_KS: usize = 8;
pub const MAX_K: u32 = 600;
pub const MAX_RECORDS: usize = 120;
/// 1 秒の tick の数（マッチの中は 60 tick/秒。実測で 59〜61）。
pub const TICKS_PER_SECOND: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub start_ticks: u32,
    pub ks: [u32; MAX_KS],
    pub nk: usize,
    pub trials_per_k: u32,
    pub gap_ticks: u32,
    pub arena_bytes: u32,
    /// 進め直しの形（既定は E1-2 の実時間の k フレーム。`frame` は E1-3: 1 回の描画の中で走査を k 回 —— `rollback_resim.rs`）。
    pub resim: ResimMode,
}

impl Settings {
    pub fn ks(&self) -> &[u32] {
        &self.ks[..self.nk]
    }
}

fn parse_u32(key: &str, v: &str) -> Result<u32, String> {
    v.parse::<u32>()
        .map_err(|_| format!("{key} の値 {v:?} が数として読めない"))
}

fn in_range(key: &str, v: u32, lo: u32, hi: u32) -> Result<u32, String> {
    if (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(format!("{key} の値 {v} は {lo}〜{hi}"))
    }
}

/// ini の本文から読む。`[rollback_probe]` が無い・`enable = 0` なら `Ok(None)`。知らないキーは断る。
pub fn settings_from_text(text: &str) -> Result<Option<Settings>, String> {
    let ini = ini::parse(text).map_err(|e| e.to_string())?;
    if !ini.has_section(SECTION) {
        return Ok(None);
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) {
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} を知らない（読めるのは {}）",
                KEYS.join(" / ")
            ));
        }
    }
    match ini.get(SECTION, "enable") {
        Some("1") => {}
        Some("0") | None => return Ok(None),
        Some(v) => return Err(format!("enable の値 {v:?} は 0 か 1")),
    }
    let get = |key: &str, default: u32, lo: u32, hi: u32| -> Result<u32, String> {
        match ini.get(SECTION, key) {
            None => Ok(default),
            Some(v) => in_range(key, parse_u32(key, v)?, lo, hi),
        }
    };
    let start_seconds = get("start_seconds", 20, 0, 600)?;
    let trials_per_k = get("trials", 10, 1, MAX_RECORDS as u32)?;
    let gap_ticks = get("gap_ticks", 120, 1, 3600)?;
    let arena_mib = get("arena_mib", 16, 1, 64)?;
    let mut ks = [0u32; MAX_KS];
    let mut nk = 0usize;
    for part in ini.get(SECTION, "ticks").unwrap_or("1,8,60").split(',') {
        let v = in_range("ticks", parse_u32("ticks", part.trim())?, 1, MAX_K)?;
        if nk == MAX_KS {
            return Err(format!("ticks の値は {MAX_KS} 個まで"));
        }
        if ks[..nk].contains(&v) {
            return Err(format!("ticks の値 {v} が 2 回ある"));
        }
        ks[nk] = v;
        nk += 1;
    }
    if (nk as u32) * trials_per_k > MAX_RECORDS as u32 {
        return Err(format!(
            "ticks の数 {nk} × trials {trials_per_k} が記録の上限 {MAX_RECORDS} を越える"
        ));
    }
    let resim = match ini.get(SECTION, "resim") {
        None => ResimMode::Realtime,
        Some(v) => ResimMode::parse(v)?,
    };
    // 値の綴りの誤りはここで断る（本物の読み出しは `skip_draw_from_text`）。
    skip_draw_from_ini(&ini)?;
    Ok(Some(Settings {
        start_ticks: start_seconds * TICKS_PER_SECOND,
        ks,
        nk,
        trials_per_k,
        gap_ticks,
        arena_bytes: arena_mib << 20,
        resim,
    }))
}

fn skip_draw_from_ini(ini: &ini::Ini) -> Result<SkipDraw, String> {
    match ini.get(SECTION, rollback_draw::INI_KEY) {
        None => Ok(SkipDraw::default()),
        Some(v) => SkipDraw::parse(v),
    }
}

/// 描画用の欄の除外（E1-3b。`skip_draw_fields`。既定は `none` = 数えるだけで比べの結果は変えない）。`Settings` には入れず別に読む
/// （検査が多く組み立てるので、除外は `Probe::set_skip_draw` で渡す）。綴りは `settings_from_text` が先に断っている。
pub fn skip_draw_from_text(text: &str) -> Result<SkipDraw, String> {
    skip_draw_from_ini(&ini::parse(text).map_err(|e| e.to_string())?)
}

// ---- 範囲の表 ----

/// 範囲の数の上限（本物は塊 20・要素の配列 3・.data 7・領域・割り当て器の状態 = 32。製品の経路は .data に更新の列のレジストリを
/// 足して 33、陣の背景の VM の配列 2 本を足して 35 でちょうど埋まる）。越えると `Layout::plan` が断り、`arm` が「範囲の表を
/// 作れない」と書いて入れない（黙って範囲を落とさない）。
pub const MAX_REGIONS: usize = 35;
pub const STATE_BYTES: u32 = STATE_WORDS as u32 * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    Block(u8),
    Effect(u8),
    /// 塊の中のポインタと本数で決まる VM の配列（`Tables::vm_arrays` の番号）。長さは回ごとに変わり、0 ならコピーしない。
    VmArray(u8),
    Data(u8),
    Arena,
    ArenaState,
}

/// 1 つの範囲。`buf_off` はコピーの置き場の中の位置（`cap` ずつ並ぶ。範囲の表の形で決まり、回ごとに変わらない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub kind: RegionKind,
    pub addr: u32,
    pub len: u32,
    pub cap: u32,
    pub buf_off: u32,
}

const EMPTY_REGION: Region = Region {
    kind: RegionKind::Arena,
    addr: 0,
    len: 0,
    cap: 0,
    buf_off: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub n: usize,
    pub regions: [Region; MAX_REGIONS],
}

/// 範囲の表を作れなかった理由（`u8` は範囲の番号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// ポインタが 0。
    Null(u8),
    /// ポインタ・EffectSys の欄が読めない。
    Unreadable(u8),
    /// EffectSys の要素の数・足し分が想定と違う。
    EffectShape { region: u8, count: u32, extra: u32 },
    /// 範囲がコミット済みで書ける頁でない。
    NotWritable(u8),
}

impl Layout {
    /// 範囲の表の形（置き場の中の位置と大きさ）を決める。.data の範囲はアドレスも決まる。塊と要素の配列のアドレスは
    /// `fill` が回ごとに読む。
    pub fn plan(t: &Tables, arena_base: u32, arena_bytes: u32) -> Result<Layout, String> {
        // 書き位置の記録（`Marks`）は行の番号で引くので、置き場を越える行があると黙って外し損ねる。先に断る。
        if t.skips.len() > MAX_SKIP_ROWS {
            return Err(format!(
                "比べない所の表が {} 行（{MAX_SKIP_ROWS} 行まで）",
                t.skips.len()
            ));
        }
        let mut l = Layout {
            n: 0,
            regions: [EMPTY_REGION; MAX_REGIONS],
        };
        let mut off = 0u32;
        let mut push = |l: &mut Layout, kind, addr, cap: u32| -> Result<(), String> {
            if l.n == MAX_REGIONS {
                return Err(format!("範囲が {MAX_REGIONS} を越える"));
            }
            l.regions[l.n] = Region {
                kind,
                addr,
                len: cap,
                cap,
                buf_off: off,
            };
            l.n += 1;
            off = off
                .checked_add(cap)
                .ok_or("コピーの置き場が 4 GiB を越える")?;
            Ok(())
        };
        for (i, b) in t.blocks.iter().enumerate() {
            push(&mut l, RegionKind::Block(i as u8), 0, b.size)?;
        }
        for (i, e) in t.effects.iter().enumerate() {
            let cap = t
                .effect_bytes(e)
                .ok_or_else(|| format!("{} の大きさが溢れる", e.name))?;
            push(&mut l, RegionKind::Effect(i as u8), 0, cap)?;
        }
        for (i, v) in t.vm_arrays.iter().enumerate() {
            if !t.blocks.iter().any(|b| b.name == v.owner) {
                return Err(format!("{} の持ち主の塊 {} が表に無い", v.name, v.owner));
            }
            let cap = v
                .max
                .checked_mul(v.elem)
                .ok_or_else(|| format!("{} の大きさが溢れる", v.name))?;
            push(&mut l, RegionKind::VmArray(i as u8), 0, cap)?;
        }
        for (i, d) in t.data.iter().enumerate() {
            if d.end <= d.start {
                return Err(format!("{} の範囲が空か逆", d.name));
            }
            push(&mut l, RegionKind::Data(i as u8), d.start, d.end - d.start)?;
        }
        push(&mut l, RegionKind::Arena, arena_base, arena_bytes)?;
        push(&mut l, RegionKind::ArenaState, 0, STATE_BYTES)?;
        Ok(l)
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions[..self.n]
    }

    pub fn buffer_bytes(&self) -> u32 {
        self.regions().iter().map(|r| r.cap).sum()
    }

    /// 塊・要素の配列のアドレスと長さが同じか（領域の長さは比べない）。
    pub fn same_places(&self, o: &Layout) -> bool {
        self.n == o.n
            && self
                .regions()
                .iter()
                .zip(o.regions())
                .all(|(a, b)| a.kind == RegionKind::Arena || (a.addr == b.addr && a.len == b.len))
    }
}

/// 範囲の表を埋めるのに要るインターフェース（`fill_layout`）。E1 は `ProbeIo` から（`fill`）、M8-2b の製品の経路は `rollback_play` の
/// インターフェースから渡す（同じ手順を 2 つ書かないため）。
pub trait LayoutIo {
    /// 落ちない読み方で 4 バイト。
    fn read_u32(&mut self, addr: u32) -> Option<u32>;
    /// `[addr, addr + len)` がコミット済みで書ける頁か。
    fn check_rw(&mut self, addr: u32, len: u32) -> bool;
}

struct ViaProbe<'a>(&'a mut dyn ProbeIo);

impl LayoutIo for ViaProbe<'_> {
    fn read_u32(&mut self, addr: u32) -> Option<u32> {
        self.0.read_u32(addr)
    }
    fn check_rw(&mut self, addr: u32, len: u32) -> bool {
        self.0.check_rw(addr, len)
    }
}

/// 回ごとに、塊・要素の配列のアドレスを読み、全部の範囲が書けるかを見る。領域の長さは `arena_top`。
pub fn fill(
    l: &mut Layout,
    t: &Tables,
    io: &mut dyn ProbeIo,
    arena_top: u32,
) -> Result<(), LayoutError> {
    fill_layout(l, t, &mut ViaProbe(io), arena_top)
}

pub fn fill_layout(
    l: &mut Layout,
    t: &Tables,
    io: &mut dyn LayoutIo,
    arena_top: u32,
) -> Result<(), LayoutError> {
    for i in 0..l.n {
        let idx = i as u8;
        let r = &mut l.regions[i];
        match r.kind {
            RegionKind::Block(b) => {
                let p = io
                    .read_u32(t.blocks[b as usize].ptr_at)
                    .ok_or(LayoutError::Unreadable(idx))?;
                if p == 0 {
                    return Err(LayoutError::Null(idx));
                }
                r.addr = p;
                r.len = r.cap;
            }
            RegionKind::Effect(e) => {
                let e = &t.effects[e as usize];
                let sys = io
                    .read_u32(e.sys_ptr_at)
                    .ok_or(LayoutError::Unreadable(idx))?;
                if sys == 0 {
                    return Err(LayoutError::Null(idx));
                }
                let mut rd = |off: u32| {
                    sys.checked_add(off)
                        .and_then(|a| io.read_u32(a))
                        .ok_or(LayoutError::Unreadable(idx))
                };
                let (p, count, extra) =
                    (rd(t.effect_ptr)?, rd(t.effect_count)?, rd(t.effect_extra)?);
                if count != e.count || extra != e.extra {
                    return Err(LayoutError::EffectShape {
                        region: idx,
                        count,
                        extra,
                    });
                }
                if p == 0 {
                    return Err(LayoutError::Null(idx));
                }
                r.addr = p;
                r.len = r.cap;
            }
            RegionKind::VmArray(v) => {
                let s = vm_array_shape(t, &t.vm_arrays[v as usize], io, idx)?;
                // コピーしない（長さ 0）ときはアドレスも 0 に（範囲の中の判定・記録の表に古い配列のアドレスを残さない）。
                r.addr = s.place().0;
                r.len = s.len;
            }
            RegionKind::Data(_) => {}
            RegionKind::Arena => r.len = arena_top.min(r.cap),
            RegionKind::ArenaState => continue,
        }
        if !matches!(r.kind, RegionKind::Arena) && !io.check_rw(r.addr, r.len) {
            return Err(LayoutError::NotWritable(idx));
        }
    }
    Ok(())
}

// ---- 比べない所 ----
//
// 実機で食い違った所のうち、同期と関係ない表示・実時間の値（fps の計測・「60.00fps」の文字・記録係の fps のバイト）を、
// コピーと書き戻しはそのままに、D1 と D2 の比べからだけ外す。外すのは表（`th09_addr::ROLLBACK_COMPARE_SKIPS`）に書いた所だけで、
// その外は 1 バイトでも違えば食い違い。見分けに要る値が読めない・形が合わないときは外さない（比べる所が増えるだけ）。

/// 1 つの範囲の中の比べない所の数の上限（本物は fps_timing 1・fps の文字の枠 1〜2・記録係の塊の書き位置 1〜4）。
/// 越えた分は外さずに比べる（`Skips::dropped`）。
pub const MAX_SKIPS: usize = 32;
/// 比べない所の表の行の上限（`Marks` の置き場の大きさ）。越えると `Layout::plan` が断る。
pub const MAX_SKIP_ROWS: usize = 8;

/// `Cursor` の行ごとの書き位置（`[0]` = S の始まり・`[1]` = D1 の始まり・`[2]` = D2 の始まり。読めなければ `None`）。添字は表の行の番号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marks {
    pub at: [[Option<u32>; 3]; MAX_SKIP_ROWS],
}

impl Marks {
    pub const EMPTY: Marks = Marks {
        at: [[None; 3]; MAX_SKIP_ROWS],
    };

    /// `Cursor` の行の書き位置を今の値で読み、`[which]` へ置く（ゲームのスレッド。読むだけ）。
    fn read(&mut self, t: &Tables, io: &mut dyn ProbeIo, which: usize) {
        for (i, s) in t.skips.iter().enumerate().take(MAX_SKIP_ROWS) {
            if let RbSkipKind::Cursor {
                ptr_at, cursor_off, ..
            } = s.kind
            {
                self.at[i][which] = io
                    .read_u32(ptr_at)
                    .filter(|&p| p != 0)
                    .and_then(|p| p.checked_add(cursor_off))
                    .and_then(|a| io.read_u32(a));
            }
        }
    }
}

/// 塊 `[p, p + n)` のうち、書き位置が `s` から `d` まで 1 バイトずつ進んで書いた所（領域の先頭からの座標）。書き位置が塊の中に
/// あるとは `p ≤ c ≤ p + n`。期間の中の取り直しは 1 回までと見る（記録係は 3,598 tick に 1 回、k は `MAX_K` = 600 まで）。
/// 2 回跨ぐと間の塊を外し損ねるが、それは食い違いが出る向きで、隠す向きではない。
/// - `s` も `d` もこの塊: `[s, d)`（戻る向き `d < s` なら無し）。
/// - `s` だけ: 期間の中で別の塊へ取り直した。`[s, p + n)`。
/// - `d` だけ: 期間の中で取り直した先の塊。`[p, d)`。
/// - どちらでもない: 無し（期間の中で書いていない塊）。
pub fn cursor_span(p: u32, n: u32, s: u32, d: u32) -> Option<(u32, u32)> {
    let end = p.saturating_add(n);
    let has = |c: u32| p <= c && c <= end;
    let (a, b) = match (has(s), has(d)) {
        (true, true) => (s, d),
        (true, false) => (s, end),
        (false, true) => (p, d),
        (false, false) => return None,
    };
    (a < b).then_some((a, b))
}

/// 1 つの範囲の中の比べない所（範囲の先頭からの `[start, end)`。始まりの順）。固定長（ゲームのスレッドで作る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skips {
    n: usize,
    r: [(u32, u32); MAX_SKIPS],
    /// 置き場が足りずに落とした数。落とした所は比べる（比べる所が増えるだけで、食い違いを隠す向きには働かない）。
    pub dropped: u32,
}

impl Skips {
    pub const EMPTY: Skips = Skips {
        n: 0,
        r: [(0, 0); MAX_SKIPS],
        dropped: 0,
    };

    pub fn push(&mut self, start: u32, end: u32) {
        if end <= start {
            return;
        }
        if self.n == MAX_SKIPS {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        // 始まりの順に差し込む（比べるときに先頭から 1 回なめるだけで済むように）。
        let mut i = self.n;
        while i > 0 && self.r[i - 1].0 > start {
            self.r[i] = self.r[i - 1];
            i -= 1;
        }
        self.r[i] = (start, end);
        self.n += 1;
    }

    pub fn ranges(&self) -> &[(u32, u32)] {
        &self.r[..self.n]
    }
}

/// 範囲 `r`（D1 のコピー `a1`・今 `a2`）の中の比べない所（範囲の先頭からの位置）。表の行ごとに:
/// - `Range`: ゲームのアドレスの範囲のうち `r` に掛かる所（DLL の領域と割り当て器の状態には掛けない）。
/// - `TextAt`: 列の枠のうち `r` に丸ごと入り、D1 と D2 の両方で位置の欄が `pos` の枠の文字の欄。
/// - `Cursor`: `r` が DLL の領域のとき、今の割り当て器 `st`（D2）を歩いて札の合う塊ごとに `cursor_span`（S → D1 と
///   S → D2 の両方）。級の余り・塊の先頭・札の違う塊は比べる。先頭が壊れていて歩けなければ、歩けた所まで。
///
/// 確保しない・パニックしない（ゲームのスレッドの比べの中で呼ぶ）。
pub fn region_skips<M: ArenaMem>(
    t: &Tables,
    r: &Region,
    a1: &[u8],
    a2: &[u8],
    arena: (&ArenaState, &M),
    marks: &Marks,
) -> Skips {
    let mut out = Skips::EMPTY;
    let game = !matches!(r.kind, RegionKind::Arena | RegionKind::ArenaState);
    let lo = r.addr;
    let hi = r.addr.saturating_add(r.len);
    for (i, row) in t.skips.iter().enumerate() {
        match row.kind {
            RbSkipKind::Range { start, end } if game => {
                let (a, b) = (start.max(lo), end.min(hi));
                if a < b {
                    out.push(a - lo, b - lo);
                }
            }
            RbSkipKind::TextAt {
                list,
                stride,
                count,
                text,
                pos_off,
                pos,
            } if game => {
                for k in 0..count {
                    let Some(at) = k
                        .checked_mul(stride)
                        .and_then(|o| list.checked_add(o))
                        .filter(|&at| at >= lo)
                    else {
                        continue;
                    };
                    let off = at - lo;
                    let end = off.checked_add(stride.max(pos_off.saturating_add(12)));
                    if end.is_none_or(|e| e > r.len) {
                        continue;
                    }
                    if pos_is(a1, off + pos_off, &pos) && pos_is(a2, off + pos_off, &pos) {
                        out.push(off, off + text.min(stride));
                    }
                }
            }
            RbSkipKind::Cursor { callers, .. } if r.kind == RegionKind::Arena => {
                let Some(m) = marks.at.get(i) else { continue };
                let rel = |c: Option<u32>| c.and_then(|c| c.checked_sub(lo));
                let (Some(s), d1, d2) = (rel(m[0]), rel(m[1]), rel(m[2])) else {
                    continue;
                };
                let (st, mem) = arena;
                let _ = st.for_each_used(mem, |p, n, tag| {
                    if !callers.contains(&tag) {
                        return;
                    }
                    for d in [d1, d2].into_iter().flatten() {
                        if let Some((a, b)) = cursor_span(p, n, s, d) {
                            out.push(a, b);
                        }
                    }
                });
            }
            _ => {}
        }
    }
    out
}

fn pos_is(a: &[u8], off: u32, pos: &[u32; 3]) -> bool {
    let o = off as usize;
    let Some(b) = a.get(o..o + 12) else {
        return false;
    };
    b.as_chunks::<4>()
        .0
        .iter()
        .zip(pos)
        .all(|(w, p)| *w == p.to_le_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compared {
    pub d1: u64,
    pub d2: u64,
    pub diff: Diff,
    pub offs: [u32; DIFF_OFFS],
    pub n_offs: u8,
    pub skipped: u32,
    /// 最初に違った位置（`diff.first`）から 4 バイトを、リトルエンディアンの u32 として読んだ値。D1 / D2 のどちらかで 4 バイトに
    /// 届かなければ両方 `None`（診断用）。
    pub first4_d1: Option<u32>,
    pub first4_d2: Option<u32>,
}

/// 表の外から足す比べない所（比べる範囲の先頭からの `[start, end)` を、始まりの小さい順に `emit` へ渡す関数）。E1-3b の描画用の欄の
/// 除外: 枠ごと・塊ごとに数が変わり `Skips` の固定長に収まらないので、歩きながら渡す。渡す順が乱れると、比べる区間を取りこぼす・
/// 二重に数える。
pub type ExtraSkips<'a> = &'a dyn Fn(&mut dyn FnMut(u32, u32));

pub fn no_extra_skips(_emit: &mut dyn FnMut(u32, u32)) {}

/// `[0, total)` から比べない所（`skips` と `extra`。どちらも始まりの順）を除いた区間を先頭から順に `f(始まり, 終わり)` で渡す。
/// 最後の区間は空でも 1 回渡す（比べない所が無いとき、ダイジェストが `digest(a)` と同じ値になるように）。
fn for_segments(total: u32, skips: &Skips, extra: ExtraSkips, mut f: impl FnMut(u32, u32)) {
    let mut cur = 0u32;
    let mut si = 0usize;
    let st = skips.ranges();
    let mut skip = |s: u32, e: u32, f: &mut dyn FnMut(u32, u32)| {
        let (s, e) = (s.min(total), e.min(total));
        if s > cur {
            f(cur, s);
        }
        cur = cur.max(e);
    };
    // 表の行（`skips`）と `extra` を、始まりの順に混ぜて渡す。
    extra(&mut |s, e| {
        while si < st.len() && st[si].0 <= s {
            skip(st[si].0, st[si].1, &mut f);
            si += 1;
        }
        skip(s, e, &mut f);
    });
    while si < st.len() {
        skip(st[si].0, st[si].1, &mut f);
        si += 1;
    }
    f(cur, total);
}

fn part(a: &[u8], s: u32, e: u32) -> &[u8] {
    let n = a.len();
    &a[(s as usize).min(n)..(e as usize).min(n)]
}

/// 表の行の比べない所だけで比べる（検査が、E1-3b の前からの比べ方の基準に使う。本物は `compare_bytes_with`）。
#[cfg(test)]
pub fn compare_bytes(a1: &[u8], a2: &[u8], skips: &Skips) -> Compared {
    compare_bytes_with(a1, a2, skips, &no_extra_skips)
}

/// D1 のコピー `a1` と今 `a2` を、比べない所（表の行 `skips` と表の外から足す `extra`）を除いて比べる。ダイジェストは比べる区間を
/// 先頭から順に `digest_continue` でつないだ値。バイトの比べはダイジェストか長さが違ったときだけ（ゲームのスレッドで約 12 MB を
/// 通すため）。`skipped` には `extra` の分も数える。違いの位置は範囲の先頭から。長さが違えば、長い方のはみ出した分も
/// （比べない所の外なら）違うと数える。確保しない・パニックしない。
pub fn compare_bytes_with(a1: &[u8], a2: &[u8], skips: &Skips, extra: ExtraSkips) -> Compared {
    let total = a1.len().max(a2.len()) as u32;
    let (mut h1, mut h2) = (DIGEST_INIT, DIGEST_INIT);
    let mut compared = 0u32;
    for_segments(total, skips, extra, |s, e| {
        h1 = digest_continue(h1, part(a1, s, e));
        h2 = digest_continue(h2, part(a2, s, e));
        compared += e - s;
    });
    let mut out = Compared {
        d1: h1,
        d2: h2,
        diff: Diff::default(),
        offs: [0; DIFF_OFFS],
        n_offs: 0,
        skipped: total - compared,
        first4_d1: None,
        first4_d2: None,
    };
    if h1 == h2 && a1.len() == a2.len() {
        return out;
    }
    let mut last_page: Option<u32> = None;
    let page = PAGE as u32;
    for_segments(total, skips, extra, |s, e| {
        let mut tmp = [0u32; DIFF_OFFS];
        let (d, n) = diff(part(a1, s, e), part(a2, s, e), &mut tmp);
        let (Some(f), Some(l)) = (d.first, d.last) else {
            return;
        };
        let (f, l) = (s + f, s + l);
        let o = &mut out.diff;
        o.bytes = o.bytes.saturating_add(d.bytes);
        o.first = o.first.or(Some(f));
        o.last = Some(l);
        // 区間どうしが同じ頁を分けるときは、その頁を 2 回数えない（区間の中の頁は先頭から順なので、重なりうるのは
        // この区間の最初の頁と、前の区間の最後の頁だけ）。
        let shared = u32::from(last_page == Some(f / page));
        o.pages = o.pages.saturating_add(d.pages - shared);
        last_page = Some(l / page);
        for &x in &tmp[..n] {
            if (out.n_offs as usize) < DIFF_OFFS {
                out.offs[out.n_offs as usize] = s + x;
                out.n_offs += 1;
            }
        }
    });
    // 診断用に、最初に違った位置から 4 バイトを両方読む（読めなければ両方 `None`）。
    if let Some(o) = out.diff.first {
        let o = o as usize;
        let read4 = |a: &[u8]| {
            a.get(o..o + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        out.first4_d1 = read4(a1);
        out.first4_d2 = read4(a2);
    }
    out
}

// ---- ゲームとのインターフェース ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buf {
    Snapshot,
    D1,
}

/// 入口と割り当て器の数（どれも増えるだけ。期間の中の回数は差で取る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// ゲームのスレッドの確保で、DLL の領域へ向けなかったもの（呼び出し元が状態の外・領域が足りない）。見張りの間だけ。
    pub out_allocs: u32,
    /// ゲームのスレッドが DLL の領域の外の塊（NULL でない）を解放した回数のうち、同じ期間の中で生まれた塊（`out_frees_born`）
    /// でないもの（期間の前からある塊）。見張りの間だけ。
    pub out_frees: u32,
    /// 領域の外の塊の解放のうち、同じ見張りの間に CRT へ回した確保で生まれた塊（アドレスを `BornSet` で引けた）。書き戻した状態は
    /// この塊を指さない（コピーした時点に無かった）ので、期間を捨てない。
    pub out_frees_born: u32,
    /// 見張りの間に CRT へ回した確保のうち、`BornSet` が満ちてアドレスを記録できなかったもの。その塊の解放を生まれ・前からの
    /// どちらとも言えないので、1 回でもあれば期間を捨てる（`Skip::OutsideFree`）。
    pub out_untracked: u32,
    /// 別のスレッドが DLL の領域の塊を解放しようとした回数（解放せずに数えるだけ）。
    pub foreign_frees: u32,
    /// DLL の領域の塊の解放を割り当て器が断った回数（二重・途中・壊れた先頭）。
    pub bad_frees: u32,
    /// 領域が足りず CRT へ回した確保。
    pub arena_full: u32,
    /// 割り当て器の確保・解放の回数（割り当て器の状態の欄。書き戻すと戻る）。
    pub arena_allocs: u32,
    pub arena_frees: u32,
}

impl Counters {
    pub fn since(&self, before: &Counters) -> Counters {
        Counters {
            out_allocs: self.out_allocs.wrapping_sub(before.out_allocs),
            out_frees: self.out_frees.wrapping_sub(before.out_frees),
            out_frees_born: self.out_frees_born.wrapping_sub(before.out_frees_born),
            out_untracked: self.out_untracked.wrapping_sub(before.out_untracked),
            foreign_frees: self.foreign_frees.wrapping_sub(before.foreign_frees),
            bad_frees: self.bad_frees.wrapping_sub(before.bad_frees),
            arena_full: self.arena_full.wrapping_sub(before.arena_full),
            arena_allocs: self.arena_allocs.wrapping_sub(before.arena_allocs),
            arena_frees: self.arena_frees.wrapping_sub(before.arena_frees),
        }
    }
}

/// `BornSet` の枠の数。外の確保があった期間は 1 期間 10 回（実測）なので、25 倍の余裕。
pub const BORN_CAP: usize = 256;

/// 見張りの間に CRT へ回した確保のアドレス（期間の中で生まれた、DLL の領域の外の塊）。見張りを立てるたびに空にする。入口
/// （ゲームのスレッド）から呼ぶので、ヒープを使わず、パニックしない。`static` に置くため中身は原子の語にする（`Relaxed`）。
pub struct BornSet {
    v: [AtomicU32; BORN_CAP],
    len: AtomicU32,
}

impl BornSet {
    pub const fn new() -> Self {
        BornSet {
            v: [const { AtomicU32::new(0) }; BORN_CAP],
            len: AtomicU32::new(0),
        }
    }

    pub fn clear(&self) {
        self.len.store(0, Ordering::Relaxed);
    }

    /// アドレスを記録する。満ちていて記録できなければ false（NULL は記録せず true —— CRT が確保に失敗した塊は解放の入口が数えない）。
    /// 同じアドレスが 2 回来たら 2 枠使う（operator new `0x47B24E` は malloc の入口 `0x47B151` を通らず同じ `0x47B125` を呼ぶので、
    /// 1 回の確保で記録するのは 1 回。重複は CRT の free を通らない解放の後だけ）。
    pub fn add(&self, p: u32) -> bool {
        if p == 0 {
            return true;
        }
        let n = self.len.load(Ordering::Relaxed) as usize;
        if n >= BORN_CAP {
            return false;
        }
        self.v[n].store(p, Ordering::Relaxed);
        self.len.store(n as u32 + 1, Ordering::Relaxed);
        true
    }

    /// アドレスが記録してあれば、同じアドレスの枠を全部取り除いて true（2 回記録したアドレスも 1 回の解放で消える）。無ければ false。
    pub fn take(&self, p: u32) -> bool {
        let mut n = self.len.load(Ordering::Relaxed) as usize;
        let mut found = false;
        let mut i = 0;
        while i < n {
            if self.v[i].load(Ordering::Relaxed) == p {
                found = true;
                n -= 1;
                self.v[i].store(self.v[n].load(Ordering::Relaxed), Ordering::Relaxed);
            } else {
                i += 1;
            }
        }
        self.len.store(n as u32, Ordering::Relaxed);
        found
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed) as usize
    }

    pub fn contains(&self, p: u32) -> bool {
        let n = self.len().min(BORN_CAP);
        self.v[..n].iter().any(|v| v.load(Ordering::Relaxed) == p)
    }
}

impl Default for BornSet {
    fn default() -> Self {
        Self::new()
    }
}

pub const DIFF_OFFS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionResult {
    pub kind: RegionKind,
    pub addr: u32,
    /// D1 と D2 の長さ（領域だけ違いうる）。
    pub len1: u32,
    pub len2: u32,
    pub d1: u64,
    pub d2: u64,
    pub diff: Diff,
    pub offs: [u32; DIFF_OFFS],
    pub n_offs: u8,
    /// 比べなかったバイトの数（比べない所。ダイジェストもこの所を除いた値）。
    pub skipped: u32,
    /// 最初に違った位置から 4 バイト（`Compared::first4_d1` / `first4_d2` と同じ。診断用）。
    pub first4_d1: Option<u32>,
    pub first4_d2: Option<u32>,
}

impl RegionResult {
    pub const EMPTY: RegionResult = RegionResult {
        kind: RegionKind::Arena,
        addr: 0,
        len1: 0,
        len2: 0,
        d1: 0,
        d2: 0,
        diff: Diff {
            bytes: 0,
            first: None,
            last: None,
            pages: 0,
        },
        offs: [0; DIFF_OFFS],
        n_offs: 0,
        skipped: 0,
        first4_d1: None,
        first4_d2: None,
    };

    pub fn of(r: &Region, a1: &[u8], a2: &[u8], skips: &Skips) -> RegionResult {
        Self::of_with(r, a1, a2, skips, &no_extra_skips)
    }

    /// `of` に、表の外の比べない所 `extra`（E1-3b の描画用の欄）を足したもの。`skipped` に数える。
    pub fn of_with(
        r: &Region,
        a1: &[u8],
        a2: &[u8],
        skips: &Skips,
        extra: ExtraSkips,
    ) -> RegionResult {
        let c = compare_bytes_with(a1, a2, skips, extra);
        RegionResult {
            kind: r.kind,
            addr: r.addr,
            len1: a1.len() as u32,
            len2: a2.len() as u32,
            d1: c.d1,
            d2: c.d2,
            diff: c.diff,
            offs: c.offs,
            n_offs: c.n_offs,
            skipped: c.skipped,
            first4_d1: c.first4_d1,
            first4_d2: c.first4_d2,
        }
    }

    pub fn same(&self) -> bool {
        self.d1 == self.d2 && self.len1 == self.len2 && self.diff.bytes == 0
    }
}

/// ゲームのスレッドが外から得るもの。本物は `rollback_win::RealProbeIo`（入力の地点の中）。
pub trait ProbeIo {
    /// マッチの中か（場面 2・読み込み中でない・マッチの終わりのメニューでない・デモでない）。
    fn in_match(&mut self) -> bool;
    fn read_u32(&mut self, addr: u32) -> Option<u32>;
    fn check_rw(&mut self, addr: u32, len: u32) -> bool;
    /// 更新の列と描画の列の並び（節のアドレス・関数・this）のダイジェスト。
    fn list_signature(&mut self) -> u64;
    fn arena_top(&mut self) -> u32;
    /// ゲームのスレッドの確保を DLL の領域へ向けるか。
    fn set_redirect(&mut self, on: bool);
    /// 領域の外の確保・解放を数えるか（期間中だけ）。
    fn set_watch(&mut self, on: bool);
    fn counters(&mut self) -> Counters;
    /// 範囲を置き場へコピーする（割り当て器の状態を含む）。
    fn save(&mut self, l: &Layout, to: Buf);
    /// S を書き戻す（領域は `[0, S の top)` を戻し、`[S の top, 今の top)` を 0 にし、割り当て器の状態を戻す）。
    fn restore(&mut self, l: &Layout);
    /// D1 の置き場と今の状態を範囲ごとに比べる（`out` は `l.n` 個）。比べない所は `region_skips(t, …, marks)`、
    /// バイトの比べは `RegionResult::of`。
    fn compare(&mut self, t: &Tables, l: &Layout, marks: &Marks, out: &mut [RegionResult]);
    /// `compare` に、E1-3b の診断（違ったバイトの内訳 `diag`）と描画用の欄の除外 `skip` を足したもの。既定は `compare` のまま。
    /// 本物（`rollback_win`）だけが上書きする。
    fn compare_draw(
        &mut self,
        t: &Tables,
        l: &Layout,
        marks: &Marks,
        out: &mut [RegionResult],
        _skip: SkipDraw,
        _diag: &mut DrawDiag,
    ) {
        self.compare(t, l, marks, out);
    }
    /// ローカル入力を 1 回読み、feed を 3 回（0 に p1・1 に p2・2 に p1|p2）呼ぶ（台本の経路と同じ順）。
    fn feed(&mut self, p1: u16, p2: u16);
    fn now_us(&mut self) -> u64;
    fn stop_requested(&mut self) -> bool;
    fn publish(&mut self, rec: &TrialRecord);
    /// 次の場面の要求が今の場面と違うか（読めなければ true）。フレームの形の進め直しの前にだけ見る（場面の切替をまたがない）。
    fn scene_request_pending(&mut self) -> bool {
        false
    }
    /// フレームの形の進め直しの前（S を書き戻す前）に、BGM の待ち行列と文字の数を取っておく（`rollback_resim::capture_outputs`）。
    /// E1-2 の形では呼ばない。
    fn capture_outputs(&mut self) {}
}

// ---- 結果 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Layout(LayoutError),
    /// 期間の中に台本が feed しなかった tick があった（台本が終わった・止まった）。
    ScriptGap,
    LeftMatch,
    OutsideFree,
    /// 期間の中で生まれた領域の外の塊が、D1 でまだ生きている（外の確保の数 ≠ 期間の中で生まれた塊の解放の数）。
    OutsideAlive,
    ForeignFree,
    BadFree,
    ArenaFull,
    ListChanged,
    LayoutChanged,
    /// フレームの形の進め直しの前に、次の場面の要求が立っていた（進め直しが場面の切替をまたぐ）。
    SceneRequest,
    /// 終わりの合図を受けた（進め直しの途中なら、比べはしたが確保の向け先が変わっていたかもしれない）。
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Match,
    Mismatch,
    Skipped(Skip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialRecord {
    pub index: u32,
    pub k: u32,
    /// マッチに入ってから何 tick 目の始まりでコピーしたか（DLL が数えたマッチの tick）。
    pub match_tick: u32,
    pub outcome: Outcome,
    /// コピー S・D1 のコピー・書き戻し・比べにかかった時間（マイクロ秒）。
    pub save_us: u32,
    pub d1_us: u32,
    pub restore_us: u32,
    pub compare_us: u32,
    /// 進めた期間と進め直した期間の数の差。
    pub fwd: Counters,
    pub rep: Counters,
    pub top_t: u32,
    pub top_d1: u32,
    pub top_d2: u32,
    pub n: u8,
    pub regions: [RegionResult; MAX_REGIONS],
    /// 比べない所の書き位置（S・D1・D2 の始まりで読んだ値）。
    pub marks: Marks,
    /// フレームの形（E1-3）の進め直しの記録。E1-2 の形では全部 0 / なし。
    pub resim: ResimRec,
    /// 違ったバイトの内訳（E1-3b。比べの後に本物の実装 `compare_draw` が埋める。埋めなければ `valid = false`）。
    pub draw: DrawDiag,
}

impl TrialRecord {
    pub const EMPTY: TrialRecord = TrialRecord {
        index: 0,
        k: 0,
        match_tick: 0,
        outcome: Outcome::Skipped(Skip::Stopped),
        save_us: 0,
        d1_us: 0,
        restore_us: 0,
        compare_us: 0,
        fwd: Counters {
            out_allocs: 0,
            out_frees: 0,
            out_frees_born: 0,
            out_untracked: 0,
            foreign_frees: 0,
            bad_frees: 0,
            arena_full: 0,
            arena_allocs: 0,
            arena_frees: 0,
        },
        rep: Counters {
            out_allocs: 0,
            out_frees: 0,
            out_frees_born: 0,
            out_untracked: 0,
            foreign_frees: 0,
            bad_frees: 0,
            arena_full: 0,
            arena_allocs: 0,
            arena_frees: 0,
        },
        top_t: 0,
        top_d1: 0,
        top_d2: 0,
        n: 0,
        regions: [RegionResult::EMPTY; MAX_REGIONS],
        marks: Marks::EMPTY,
        resim: ResimRec {
            frame: false,
            calls: 0,
            total_us: 0,
            max_us: 0,
            first_us: 0,
            abort: None,
            scene_req: 0,
            outputs_restored: None,
        },
        draw: DrawDiag::EMPTY,
    };

    pub fn regions(&self) -> &[RegionResult] {
        &self.regions[..self.n as usize]
    }
}

// ---- 手順 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// マッチの tick を `need` 数えたら次の回を始める。
    Wait,
    /// コピーした後、台本のワードを記録しながら進めている。
    Forward,
    /// 書き戻した後、記録した語の `j` 個目を feed する。
    Replay {
        j: u32,
    },
    Done,
}

pub struct Probe {
    s: Settings,
    t: Tables,
    plan: Layout,
    phase: Phase,
    need: u32,
    match_ticks: u32,
    /// 次の回の番号（記録の数）と、k を選ぶ回し手。
    records: u32,
    cursor: u32,
    done_per_k: [u32; MAX_KS],
    k: u32,
    words: [(u16, u16); MAX_K as usize],
    n_words: u32,
    fed: bool,
    snap: Layout,
    d1: Layout,
    sig: u64,
    c0: Counters,
    rec: TrialRecord,
    /// フレームの形で、今のフレームが書き戻しのフレーム（D1 の点から `take_owed` までの間）か。
    resim_armed: bool,
    skip_draw: SkipDraw,
}

impl Probe {
    pub fn new(s: Settings, t: Tables, arena_base: u32) -> Result<Probe, String> {
        let plan = Layout::plan(&t, arena_base, s.arena_bytes)?;
        Ok(Probe {
            s,
            t,
            plan,
            phase: Phase::Wait,
            need: s.start_ticks,
            match_ticks: 0,
            records: 0,
            cursor: 0,
            done_per_k: [0; MAX_KS],
            k: 0,
            words: [(0, 0); MAX_K as usize],
            n_words: 0,
            fed: false,
            snap: plan,
            d1: plan,
            sig: 0,
            c0: Counters::default(),
            rec: TrialRecord::EMPTY,
            resim_armed: false,
            skip_draw: SkipDraw::None,
        })
    }

    /// 描画用の欄の除外（`ARMED` を立てる前にだけ呼ぶ）。
    pub fn set_skip_draw(&mut self, m: SkipDraw) {
        self.skip_draw = m;
    }

    /// 進め直しの形（走査の call を書けなかった場合に realtime へ戻すために変える。`ARMED` を立てる前にだけ呼ぶ）。
    pub fn set_resim(&mut self, m: ResimMode) {
        self.s.resim = m;
    }

    pub fn skip_draw(&self) -> SkipDraw {
        self.skip_draw
    }

    pub fn resim_frame(&self) -> bool {
        self.s.resim == ResimMode::Frame
    }

    /// 走査の 1 回目の後に、このフレームであと何回走査を呼ぶか（書き戻しのフレームだけ `Some(k − 1)`、ほかは `None`）。
    /// 1 回聞いたら空になる（打ち切りの後の残りは次のフレームから実時間で続く）。
    pub fn take_owed(&mut self) -> Option<u32> {
        if !self.resim_armed {
            return None;
        }
        self.resim_armed = false;
        Some(match self.phase {
            Phase::Replay { j } if j < self.k => self.k - j,
            _ => 0,
        })
    }

    /// 進め直しの 1 回の走査の後の記録（`index` 0 = 最初の回。時間はマイクロ秒）。
    pub fn note_scan(&mut self, index: u32, us: u32, scene_req: bool) {
        let r = &mut self.rec.resim;
        if index == 0 {
            r.first_us = us;
        } else {
            r.calls = r.calls.saturating_add(1);
            r.total_us = r.total_us.saturating_add(us);
            r.max_us = r.max_us.max(us);
        }
        if scene_req {
            r.scene_req = r.scene_req.saturating_add(1);
        }
    }

    pub fn note_abort(&mut self, why: rollback_resim::ScanAbort) {
        self.rec.resim.abort = Some(why);
    }

    /// 最後の回の前に SE・BGM・文字の数を戻せたか。
    pub fn note_outputs(&mut self, ok: bool) {
        self.rec.resim.outputs_restored = Some(ok);
    }

    pub fn plan(&self) -> &Layout {
        &self.plan
    }

    pub fn settings(&self) -> &Settings {
        &self.s
    }

    pub fn finished(&self) -> bool {
        self.phase == Phase::Done
    }

    /// まだ回が足りない k のうち、回し手の次のもの。無ければ `None`。
    fn next_k(&mut self) -> Option<u32> {
        let nk = self.s.nk as u32;
        for _ in 0..nk {
            let i = (self.cursor % nk) as usize;
            self.cursor = self.cursor.wrapping_add(1);
            if self.done_per_k[i] < self.s.trials_per_k {
                return Some(self.s.ks[i]);
            }
        }
        None
    }

    fn all_done(&self) -> bool {
        self.done_per_k[..self.s.nk]
            .iter()
            .all(|&d| d >= self.s.trials_per_k)
            || self.records as usize >= MAX_RECORDS
    }

    /// 入力の地点（tick の始まり）で呼ぶ。`Some(1)` なら記録した入力ワードを feed した（入口は poll_all も台本も呼ばずに戻る）。
    /// `None` なら台本の経路へ（台本が feed したら `after_feed` を呼ぶ）。
    pub fn on_tick(&mut self, io: &mut dyn ProbeIo) -> Option<u32> {
        let stop = io.stop_requested();
        let m = io.in_match();
        self.match_ticks = if m {
            self.match_ticks.saturating_add(1)
        } else {
            0
        };
        match self.phase {
            Phase::Done => {
                io.set_redirect(false);
                None
            }
            Phase::Wait => {
                io.set_redirect(m && !stop);
                if stop {
                    self.phase = Phase::Done;
                    return None;
                }
                if !m {
                    self.need = self.s.start_ticks;
                    return None;
                }
                if self.need > 0 {
                    self.need -= 1;
                    return None;
                }
                self.begin(io)
            }
            Phase::Forward => self.forward(io, m, stop),
            // 進め直しの途中でマッチを出たら、記録した語をマッチの外（マッチの終わりのメニュー）へ入れずに捨てる（前向きの期間は `forward` が
            // 同じ理由で捨てている）。
            Phase::Replay { .. } if !m => self.skip(io, Skip::LeftMatch),
            Phase::Replay { j } => self.replay(io, j, stop),
        }
    }

    /// 台本が feed した語（ゲームのスレッド。`on_tick` が `None` を返した tick だけ）。
    pub fn after_feed(&mut self, p1: u16, p2: u16) {
        if self.phase != Phase::Forward {
            return;
        }
        self.fed = true;
        if self.n_words < self.k && (self.n_words as usize) < self.words.len() {
            self.words[self.n_words as usize] = (p1, p2);
            self.n_words += 1;
        }
    }

    fn begin(&mut self, io: &mut dyn ProbeIo) -> Option<u32> {
        let Some(k) = self.next_k() else {
            self.phase = Phase::Done;
            io.set_redirect(false);
            return None;
        };
        self.rec = TrialRecord::EMPTY;
        self.rec.index = self.records;
        self.rec.k = k;
        // フレームの形の回は、捨てた回も印を付ける（終わりの集計の母数を、書き戻しまで進んだ回だけにしないため）。
        self.rec.resim.frame = self.resim_frame();
        self.rec.match_tick = self.match_ticks;
        self.k = k;
        let mut l = self.plan;
        let top = io.arena_top();
        if let Err(e) = fill(&mut l, &self.t, io, top) {
            return self.skip(io, Skip::Layout(e));
        }
        self.sig = io.list_signature();
        self.rec.marks.read(&self.t, io, 0);
        let t0 = io.now_us();
        io.save(&l, Buf::Snapshot);
        self.rec.save_us = elapsed(io, t0);
        self.rec.top_t = top;
        self.snap = l;
        self.c0 = io.counters();
        io.set_watch(true);
        self.n_words = 0;
        self.fed = false;
        self.phase = Phase::Forward;
        None
    }

    fn forward(&mut self, io: &mut dyn ProbeIo, m: bool, stop: bool) -> Option<u32> {
        if !self.fed {
            return self.skip(io, Skip::ScriptGap);
        }
        self.fed = false;
        if stop {
            return self.skip(io, Skip::Stopped);
        }
        if !m {
            return self.skip(io, Skip::LeftMatch);
        }
        if self.n_words < self.k {
            return None;
        }
        let d = io.counters().since(&self.c0);
        io.set_watch(false);
        self.rec.fwd = d;
        let why = if d.out_frees > 0 || d.out_untracked > 0 {
            // 期間の中で生まれて同じ期間で解放された外の塊だけなら捨てない。
            Some(Skip::OutsideFree)
        } else if d.out_allocs != d.out_frees_born {
            // 期間の中で生まれて D1 でまだ生きている外の塊がある。書き戻しは安全だが、進め直した側は CRT から別のアドレスを受け取りうるので、
            // 状態がそのポインタを持っていれば D1 と D2 が必ず違いうる。
            Some(Skip::OutsideAlive)
        } else if d.foreign_frees > 0 {
            Some(Skip::ForeignFree)
        } else if d.bad_frees > 0 {
            Some(Skip::BadFree)
        } else if d.arena_full > 0 {
            Some(Skip::ArenaFull)
        } else if io.list_signature() != self.sig {
            Some(Skip::ListChanged)
        } else if self.resim_frame() && io.scene_request_pending() {
            // フレームの形は、進め直しの前に次の場面の要求が立っていれば進め直さない（場面の切替をまたがない）。
            Some(Skip::SceneRequest)
        } else {
            None
        };
        if let Some(w) = why {
            return self.skip(io, w);
        }
        let mut l = self.plan;
        let top = io.arena_top();
        if fill(&mut l, &self.t, io, top).is_err() || !l.same_places(&self.snap) {
            return self.skip(io, Skip::LayoutChanged);
        }
        self.rec.marks.read(&self.t, io, 1);
        let t0 = io.now_us();
        io.save(&l, Buf::D1);
        self.rec.d1_us = elapsed(io, t0);
        self.rec.top_d1 = top;
        self.d1 = l;
        if self.resim_frame() {
            // 途中の回が積む SE・BGM・文字の数を最後の回の前に戻すため、S を書き戻す前の値を取っておく（k = 1 は途中の回が無いので要らない）。
            if self.k > 1 {
                io.capture_outputs();
            }
            self.resim_armed = true;
        }
        let t0 = io.now_us();
        io.restore(&self.snap);
        self.rec.restore_us = elapsed(io, t0);
        self.c0 = io.counters();
        io.set_watch(true);
        self.replay(io, 0, stop)
    }

    fn replay(&mut self, io: &mut dyn ProbeIo, j: u32, stop: bool) -> Option<u32> {
        if j < self.k {
            let (p1, p2) = self.words[j as usize];
            io.feed(p1, p2);
            self.phase = Phase::Replay { j: j + 1 };
            return Some(1);
        }
        self.rec.rep = io.counters().since(&self.c0);
        io.set_watch(false);
        let n = self.d1.n;
        self.rec.marks.read(&self.t, io, 2);
        let t0 = io.now_us();
        io.compare_draw(
            &self.t,
            &self.d1,
            &self.rec.marks,
            &mut self.rec.regions[..n],
            self.skip_draw,
            &mut self.rec.draw,
        );
        self.rec.compare_us = elapsed(io, t0);
        self.rec.n = n as u8;
        self.rec.top_d2 = io.arena_top();
        let same = self.rec.regions().iter().all(RegionResult::same);
        self.rec.outcome = if stop {
            Outcome::Skipped(Skip::Stopped)
        } else if same {
            Outcome::Match
        } else {
            Outcome::Mismatch
        };
        if !stop && let Some(i) = self.s.ks().iter().position(|&x| x == self.k) {
            self.done_per_k[i] += 1;
        }
        self.finish_trial(io);
        None
    }

    fn skip(&mut self, io: &mut dyn ProbeIo, why: Skip) -> Option<u32> {
        io.set_watch(false);
        self.rec.outcome = Outcome::Skipped(why);
        self.finish_trial(io);
        None
    }

    fn finish_trial(&mut self, io: &mut dyn ProbeIo) {
        io.publish(&self.rec);
        self.records += 1;
        self.need = self.s.gap_ticks;
        if self.all_done() {
            self.phase = Phase::Done;
            io.set_redirect(false);
        } else {
            self.phase = Phase::Wait;
        }
    }
}

fn elapsed(io: &mut dyn ProbeIo, t0: u64) -> u32 {
    io.now_us().saturating_sub(t0).min(u64::from(u32::MAX)) as u32
}

// ---- 文（読み取りのスレッド）----

pub fn region_name(t: &Tables, k: RegionKind) -> &'static str {
    match k {
        RegionKind::Block(i) => t.blocks.get(i as usize).map_or("?", |b| b.name),
        RegionKind::Effect(i) => t.effects.get(i as usize).map_or("?", |e| e.name),
        RegionKind::VmArray(i) => t.vm_arrays.get(i as usize).map_or("?", |v| v.name),
        RegionKind::Data(i) => t.data.get(i as usize).map_or("?", |d| d.name),
        RegionKind::Arena => "arena",
        RegionKind::ArenaState => "arena_state",
    }
}

pub fn layout_error_text(t: &Tables, l: &Layout, e: &LayoutError) -> String {
    let name = |i: u8| {
        l.regions()
            .get(i as usize)
            .map_or("?", |r| region_name(t, r.kind))
    };
    match e {
        LayoutError::Null(i) => format!("{} のポインタが 0", name(*i)),
        LayoutError::Unreadable(i) => format!("{} のポインタか欄が読めない", name(*i)),
        LayoutError::EffectShape {
            region,
            count,
            extra,
        } => format!(
            "{} の要素の数 {count} ＋ {extra} が想定と違う",
            name(*region)
        ),
        LayoutError::NotWritable(i) => format!("{} がコミット済みで書ける頁でない", name(*i)),
    }
}

pub fn skip_text(t: &Tables, l: &Layout, s: &Skip) -> String {
    match s {
        Skip::Layout(e) => format!("範囲の表を作れない（{}）", layout_error_text(t, l, e)),
        Skip::ScriptGap => "窓の中に台本が feed しなかった tick がある".to_string(),
        Skip::LeftMatch => "窓の中で試合を出た".to_string(),
        Skip::OutsideFree => {
            "窓の中でゲームのスレッドが、窓の前からある DLL の領域の外の塊を解放した（書き戻すと解放済みを指す）か、窓の中の外の確保のアドレスを控えきれなかった"
                .to_string()
        }
        Skip::OutsideAlive => {
            "窓の中で生まれた DLL の領域の外の塊が、窓の終わりにまだ生きている（進め直した側は別のアドレスを受け取りうる）"
                .to_string()
        }
        Skip::ForeignFree => "窓の中で別のスレッドが DLL の領域の塊を解放しようとした".to_string(),
        Skip::BadFree => "窓の中で DLL の領域の解放を割り当て器が断った".to_string(),
        Skip::ArenaFull => "窓の中で DLL の領域が足りず CRT へ回した確保があった".to_string(),
        Skip::ListChanged => "窓の中で更新の列か描画の列の並びが変わった".to_string(),
        Skip::LayoutChanged => "窓の中で決まった塊のポインタか要素の数が変わった".to_string(),
        Skip::SceneRequest => {
            "進め直しの前に次の場面の要求 [0x4B3690] が立っていた（場面の切替をまたぐので進め直さない）"
                .to_string()
        }
        Skip::Stopped => "終わりの合図を受けた".to_string(),
    }
}

fn skip_key(s: &Skip) -> &'static str {
    match s {
        Skip::Layout(_) => "layout",
        Skip::ScriptGap => "script_gap",
        Skip::LeftMatch => "left_match",
        Skip::OutsideFree => "outside_free",
        Skip::OutsideAlive => "outside_alive",
        Skip::ForeignFree => "foreign_free",
        Skip::BadFree => "bad_free",
        Skip::ArenaFull => "arena_full",
        Skip::ListChanged => "list_changed",
        Skip::LayoutChanged => "layout_changed",
        Skip::SceneRequest => "scene_request",
        Skip::Stopped => "stopped",
    }
}

fn ms(us: u32) -> String {
    format!("{:.1}", f64::from(us) / 1000.0)
}

fn counters_text(c: &Counters) -> String {
    format!(
        "領域の確保 {}・解放 {}・外の確保 {}（アドレスを控えられなかった {}）・窓の中で生まれた外の塊の解放 {}・前からある外の塊の解放 {}・別スレッドの解放 {}・断った解放 {}・領域が足りない {}",
        c.arena_allocs,
        c.arena_frees,
        c.out_allocs,
        c.out_untracked,
        c.out_frees_born,
        c.out_frees,
        c.foreign_frees,
        c.bad_frees,
        c.arena_full
    )
}

/// 1 回ぶんの文（本ログと `_rollback.txt`）。1 行目が結果、違った範囲は 1 行ずつ。
pub fn trial_lines(t: &Tables, plan: &Layout, r: &TrialRecord) -> Vec<String> {
    let head = format!(
        "巻き戻し: 回 {}（k = {}、試合の {} tick 目で写した）: ",
        r.index, r.k, r.match_tick
    );
    let mut v = Vec::new();
    match r.outcome {
        Outcome::Skipped(s) if r.n == 0 => {
            v.push(format!(
                "{head}捨てた（{}）。書き戻していない。コピー {} ms。進めたウィンドウ: {}",
                skip_text(t, plan, &s),
                ms(r.save_us),
                counters_text(&r.fwd)
            ));
            return v;
        }
        _ => {}
    }
    let word = match r.outcome {
        Outcome::Match => "一致".to_string(),
        Outcome::Mismatch => {
            let n = r.regions().iter().filter(|x| !x.same()).count();
            format!("違い（{n} 範囲）")
        }
        Outcome::Skipped(s) => {
            format!("捨てた（{}。書き戻しと比べはした）", skip_text(t, plan, &s))
        }
    };
    // 比べなかったバイト（表に書いた所だけ）。DLL の領域の分を分けて出すのは、記録係の fps のバイトを札と書き位置で見分けられたかを
    // ログで確かめるため。
    let skipped: u64 = r.regions().iter().map(|x| u64::from(x.skipped)).sum();
    let skipped_arena: u32 = r
        .regions()
        .iter()
        .filter(|x| x.kind == RegionKind::Arena)
        .map(|x| x.skipped)
        .sum();
    v.push(format!(
        "{head}{word}。コピー {} ms / D1 のコピー {} ms / 書き戻し {} ms / 比べ {} ms。比べなかった {skipped} バイト（うち DLL の領域 {skipped_arena}）。領域の top {} → {} → {}。進めたウィンドウ: {}。進め直したウィンドウ: {}",
        ms(r.save_us),
        ms(r.d1_us),
        ms(r.restore_us),
        ms(r.compare_us),
        r.top_t,
        r.top_d1,
        r.top_d2,
        counters_text(&r.fwd),
        counters_text(&r.rep)
    ));
    let marks = marks_text(t, &r.marks);
    if !marks.is_empty() {
        v.push(format!("巻き戻し:   書き位置 {marks}"));
    }
    if let Some(l) = rollback_resim::trial_line(&r.resim) {
        v.push(l);
    }
    for x in r.regions().iter().filter(|x| !x.same()) {
        let offs = x.offs[..x.n_offs as usize]
            .iter()
            .map(|o| format!("+0x{o:X}"))
            .collect::<Vec<_>>()
            .join(" ");
        v.push(format!(
            "巻き戻し:   違う範囲 {}（0x{:08X}、D1 {} B / D2 {} B、比べない {} B）: {} バイト・{} 頁、最初 {} 最後 {}、位置 {offs}。ダイジェスト D1 {:016X} / D2 {:016X}",
            region_name(t, x.kind),
            x.addr,
            x.len1,
            x.len2,
            x.skipped,
            x.diff.bytes,
            x.diff.pages,
            x.diff.first.map_or("-".to_string(), |o| format!("+0x{o:X}")),
            x.diff.last.map_or("-".to_string(), |o| format!("+0x{o:X}")),
            x.d1,
            x.d2
        ));
        // 診断用: 最初に違った位置から 4 バイトを f32 と u32 の 16 進で（両方読めたときだけ）。
        if let (Some(w1), Some(w2)) = (x.first4_d1, x.first4_d2) {
            v.push(format!(
                "巻き戻し:   最初の4バイト {}（+0x{:X}）: D1 f32={} u32=0x{:08X} / D2 f32={} u32=0x{:08X}",
                region_name(t, x.kind),
                x.diff.first.unwrap_or(0),
                f32::from_bits(w1),
                w1,
                f32::from_bits(w2),
                w2
            ));
        }
    }
    v
}

/// `Cursor` の行の書き位置（`名前 S 0x… → D1 0x… / D2 0x…`。読めなかった所は `-`）。`Cursor` の行が無ければ空。
fn marks_text(t: &Tables, m: &Marks) -> String {
    let hex = |c: Option<u32>| c.map_or("-".to_string(), |c| format!("0x{c:08X}"));
    t.skips
        .iter()
        .enumerate()
        .take(MAX_SKIP_ROWS)
        .filter(|(_, s)| matches!(s.kind, RbSkipKind::Cursor { .. }))
        .map(|(i, s)| {
            let a = m.at[i];
            format!(
                "{} S {} → D1 {} / D2 {}",
                s.name,
                hex(a[0]),
                hex(a[1]),
                hex(a[2])
            )
        })
        .collect::<Vec<_>>()
        .join("、")
}

pub fn summary_lines(t: &Tables, s: &Settings, rs: &[TrialRecord]) -> Vec<String> {
    let mut v = Vec::new();
    let count = |f: &dyn Fn(&TrialRecord) -> bool| rs.iter().filter(|r| f(r)).count();
    v.push(format!(
        "巻き戻し: 記録 {} 件のうち 一致 {} / 違い {} / 捨てた {}",
        rs.len(),
        count(&|r| r.outcome == Outcome::Match),
        count(&|r| r.outcome == Outcome::Mismatch),
        count(&|r| matches!(r.outcome, Outcome::Skipped(_)))
    ));
    for &k in s.ks() {
        let of_k = |f: &dyn Fn(&TrialRecord) -> bool| count(&|r| r.k == k && f(r));
        v.push(format!(
            "巻き戻し: k = {k}: 一致 {} / 違い {} / 捨てた {}（比べ終えたい回 {}）",
            of_k(&|r| r.outcome == Outcome::Match),
            of_k(&|r| r.outcome == Outcome::Mismatch),
            of_k(&|r| matches!(r.outcome, Outcome::Skipped(_))),
            s.trials_per_k
        ));
    }
    let mut skips: Vec<(&'static str, usize)> = Vec::new();
    for r in rs {
        if let Outcome::Skipped(s) = r.outcome {
            let key = skip_key(&s);
            match skips.iter_mut().find(|(k, _)| *k == key) {
                Some(e) => e.1 += 1,
                None => skips.push((key, 1)),
            }
        }
    }
    if !skips.is_empty() {
        v.push(format!(
            "巻き戻し: 捨てた理由: {}",
            skips
                .iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }
    let mut diffs: Vec<(&'static str, usize)> = Vec::new();
    for r in rs.iter().filter(|r| r.outcome == Outcome::Mismatch) {
        for x in r.regions().iter().filter(|x| !x.same()) {
            let name = region_name(t, x.kind);
            match diffs.iter_mut().find(|(k, _)| *k == name) {
                Some(e) => e.1 += 1,
                None => diffs.push((name, 1)),
            }
        }
    }
    if !diffs.is_empty() {
        v.push(format!(
            "巻き戻し: 違った範囲（違いの回の数）: {}",
            diffs
                .iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::arena::{ArenaMem, ArenaState};
    use higanr_core::digest::{diff, digest};

    const NOTE: &str = include_str!("../../../analysis/notes/th09_rollback_E1.md");

    #[test]
    fn rollback_values_appear_in_the_note() {
        use crate::th09_addr as a;
        let start = NOTE.find("## 11. ").expect("ノートに §11 が無い");
        let sec = &NOTE[start..];
        let row = |name: &str| {
            sec.lines()
                .find(|l| l.starts_with(&format!("| `{name}` ")))
                .unwrap_or_else(|| panic!("ノート §11 の表に {name} の行が無い"))
        };
        let hex = |v: u32| format!("`0x{v:08X}`");
        for b in a::ROLLBACK_BLOCKS {
            let l = row(b.name);
            assert!(l.contains(&hex(b.ptr_at)), "{}: {l}", b.name);
            assert!(l.contains(&format!("`0x{:X}`", b.size)), "{}: {l}", b.name);
        }
        for e in a::ROLLBACK_EFFECT_ARRAYS {
            let l = row(e.name);
            assert!(l.contains(&hex(e.sys_ptr_at)), "{}: {l}", e.name);
            assert!(
                l.contains(&format!("`0x{:X}` ＋ {}", e.count, e.extra)),
                "{}: {l}",
                e.name
            );
        }
        for d in a::ROLLBACK_DATA {
            let l = row(d.name);
            assert!(l.contains(&hex(d.start)), "{}: {l}", d.name);
            assert!(l.contains(&hex(d.end)), "{}: {l}", d.name);
        }
        for c in a::ROLLBACK_EXCLUDED_CALLERS {
            assert!(
                sec.contains(&format!("{}〜{}", hex(c.start), hex(c.end))),
                "除く呼び出し元 {:08X}",
                c.start
            );
        }
        for s in a::ROLLBACK_COMPARE_SKIPS {
            let l = row(s.name);
            let want: Vec<String> = match s.kind {
                RbSkipKind::Range { start, end } => vec![format!("{}〜{}", hex(start), hex(end))],
                RbSkipKind::TextAt {
                    list,
                    stride,
                    count,
                    text,
                    pos_off,
                    pos,
                } => vec![
                    hex(list),
                    format!("`0x{stride:X}` × `0x{count:X}`"),
                    format!("文字の欄 `0x{text:X}`"),
                    format!("`+0x{pos_off:X}`"),
                    format!("`0x{:08X}` `0x{:08X}` `0x{:08X}`", pos[0], pos[1], pos[2]),
                ],
                RbSkipKind::Cursor {
                    callers,
                    ptr_at,
                    cursor_off,
                } => callers
                    .iter()
                    .map(|&c| hex(c))
                    .chain([format!("[{}]+0x{cursor_off:X}", hex(ptr_at))])
                    .collect(),
            };
            for w in want {
                assert!(l.contains(&w), "{}: {w} が無い: {l}", s.name);
            }
        }
        for v in [
            a::GAME_CODE_START,
            a::GAME_CODE_END,
            a::ALLOC_WRAP_RET,
            a::UPDATE_REGISTRY,
        ] {
            assert!(sec.contains(&hex(v)), "{v:08X} が §11 に無い");
        }
        for (part, size) in [
            ("Stage", 0x65C4),
            ("Player", 0x30F70),
            ("BulletManager", 0x25E1C0),
            ("EnemyManager", 0x2AC450),
            ("Spellcard", 0x1368),
            ("GuiSide", 0xABE8),
            ("ExAttackManager", 0x4C80),
            ("Gui", 0x1293C),
            ("ReplayManager", 0x164),
        ] {
            let l = NOTE
                .lines()
                .find(|l| l.starts_with(&format!("| {part} |")))
                .unwrap_or_else(|| panic!("§1-3 に {part} が無い"));
            assert!(l.contains(&format!("0x{size:X}")), "{part}: {l}");
            assert!(
                a::ROLLBACK_BLOCKS.iter().any(|b| b.size == size),
                "{part} の大きさが表に無い"
            );
        }
    }

    #[test]
    fn data_ranges_avoid_what_must_not_be_restored() {
        use crate::th09_addr as a;
        let mut rs: Vec<(u32, u32)> = a::ROLLBACK_DATA.iter().map(|d| (d.start, d.end)).collect();
        rs.sort_unstable();
        for w in rs.windows(2) {
            assert!(w[0].1 <= w[1].0, "{w:?} が重なる");
        }
        let must_not = [
            (0x004A_0000, 0x004A_00A0, "CRT の初期化の表"),
            (0x004A_3000, 0x004A_7B14, "ライブラリの .data"),
            (0x004A_CFC8, 0x004A_D018, "レジストリ"),
            (0x004B_30B0, 0x004B_38B0, "ウィンドウと Supervisor"),
            (0x004B_41B8, 0x004B_42B8, "FPS の文字"),
            (0x004D_C698, 0x004E_5000, "SoundManager から後"),
        ];
        for (lo, hi, what) in must_not {
            for &(s, e) in &rs {
                assert!(e <= lo || s >= hi, "{what} をコピーする: {s:08X}〜{e:08X}");
            }
        }
        for x in [
            a::RNG_ADDR,
            a::RNG_COUNT,
            a::INPUT_MANAGERS[0],
            a::INPUT_MANAGERS[2] + 0x8D,
        ] {
            assert!(
                rs.iter().any(|&(s, e)| s <= x && x < e),
                "{x:08X} をコピーしない"
            );
        }
        for c in a::ROLLBACK_EXCLUDED_CALLERS {
            assert!(a::GAME_CODE_START <= c.start && c.end <= a::GAME_CODE_END);
        }
        let l = Layout::plan(&Tables::th09(), 0x1000_0000, 16 << 20).unwrap();
        assert_eq!(l.n, 32);
        let fixed: u32 = l
            .regions()
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::Block(_)))
            .map(|r| r.cap)
            .sum();
        assert_eq!(
            fixed,
            2 * 5_561_712 + 2 * 0xA0 + 0x2DC + 76_092 + 19_584 + 356
        );
    }

    #[test]
    fn stats_blocks_are_copied_for_both_sides() {
        use crate::th09_addr as a;
        let find = |n: &str| {
            a::ROLLBACK_BLOCKS
                .iter()
                .find(|b| b.name == n)
                .unwrap_or_else(|| panic!("{n} がコピーする範囲に無い"))
        };
        let (p1, p2) = (find("p1_stats"), find("p2_stats"));
        assert_eq!((p1.ptr_at, p1.size), (0x004A_7DAC, 0xA0));
        assert_eq!((p2.ptr_at, p2.size), (p1.ptr_at + 0x38, 0xA0));
        assert_eq!(p1.ptr_at, find("p1_gui_side").ptr_at + 4);
        assert_eq!(p2.ptr_at, find("p2_gui_side").ptr_at + 4);
    }

    #[test]
    fn compare_skips_sit_inside_copied_data_and_spare_the_sync_state() {
        use crate::th09_addr as a;
        let sync = [
            a::RNG_ADDR,
            a::RNG_COUNT,
            a::INPUT_MANAGERS[0],
            a::INPUT_MANAGERS[2] + 0x8D,
            0x004A_7D90,
            0x004A_7E1C,
            0x004B_42D0,
            0x004D_66BC,
        ];
        let inside_one = |name: &str, start: u32, end: u32| {
            assert!(start < end, "{name}");
            let n = a::ROLLBACK_DATA
                .iter()
                .filter(|d| d.start <= start && end <= d.end)
                .count();
            assert_eq!(n, 1, "{name} がコピーする範囲 1 つの中に無い");
            for x in sync {
                assert!(!(start <= x && x < end), "{name} が {x:08X} に掛かる");
            }
        };
        let mut names = Vec::new();
        for s in a::ROLLBACK_COMPARE_SKIPS {
            names.push(s.name);
            match s.kind {
                RbSkipKind::Range { start, end } => inside_one(s.name, start, end),
                RbSkipKind::TextAt {
                    list,
                    stride,
                    count,
                    text,
                    pos_off,
                    ..
                } => {
                    inside_one(s.name, list, list + stride * count);
                    assert!(text <= pos_off && pos_off + 12 <= stride, "{}", s.name);
                }
                RbSkipKind::Cursor {
                    callers,
                    ptr_at,
                    cursor_off,
                } => {
                    let b = a::ROLLBACK_BLOCKS.iter().find(|b| b.ptr_at == ptr_at);
                    assert!(
                        b.is_some_and(|b| cursor_off + 4 <= b.size),
                        "{} の書き位置がコピーする塊の中に無い",
                        s.name
                    );
                    for &c in callers {
                        assert!(
                            crate::rollback_win::redirectable(c, 0),
                            "{} の札 {c:08X} の塊は DLL の領域へ向かない",
                            s.name
                        );
                    }
                }
            }
        }
        assert_eq!(
            names,
            [
                "fps_timing",
                "fps_text",
                "replay_fps",
                "ascii_pen_x",
                "ascii_vm1",
                "sprite_temp"
            ]
        );
        assert!(a::ROLLBACK_COMPARE_SKIPS.len() <= MAX_SKIP_ROWS);
    }

    #[test]
    fn settings_are_read_strictly() {
        assert_eq!(settings_from_text(""), Ok(None));
        assert_eq!(settings_from_text("[alloc_count]\nenable = 1\n"), Ok(None));
        assert_eq!(
            settings_from_text("[rollback_probe]\nenable = 0\n"),
            Ok(None)
        );
        let s = settings_from_text("[rollback_probe]\nenable = 1\n")
            .unwrap()
            .unwrap();
        assert_eq!(s.start_ticks, 1200);
        assert_eq!(s.ks(), &[1, 8, 60]);
        assert_eq!(
            (s.trials_per_k, s.gap_ticks, s.arena_bytes),
            (10, 120, 16 << 20)
        );
        let s = settings_from_text(
            "[ROLLBACK_PROBE]\nenable=1\nstart_seconds=5\nticks= 2 , 30\ntrials=3\ngap_ticks=10\narena_mib=4\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                s.start_ticks,
                s.ks(),
                s.trials_per_k,
                s.gap_ticks,
                s.arena_bytes
            ),
            (300, &[2u32, 30][..], 3, 10, 4 << 20)
        );
        for bad in [
            "enable = 2",
            "enable = 1\nx = 1",
            "enable = 1\nticks = 0",
            "enable = 1\nticks = 601",
            "enable = 1\nticks = 1,1",
            "enable = 1\nticks = 1,2,3,4,5,6,7,8,9",
            "enable = 1\nticks = a",
            "enable = 1\ntrials = 0",
            "enable = 1\nticks = 1,2\ntrials = 61",
            "enable = 1\narena_mib = 65",
            "enable = 1\ngap_ticks = 0",
            "enable = 1\nstart_seconds = 601",
        ] {
            assert!(
                settings_from_text(&format!("[rollback_probe]\n{bad}\n")).is_err(),
                "{bad:?} を通した"
            );
        }
    }

    #[test]
    fn sample_ini_enables_only_the_probe() {
        const SAMPLE: &str = include_str!("../../../scripts/e1_02_rollback.ini");
        let s = settings_from_text(SAMPLE).unwrap().expect("有効");
        assert_eq!(s.ks(), &[1, 8, 60]);
        assert_eq!(crate::alloc_count::settings_from_text(SAMPLE), Ok(None));
        assert_eq!(crate::net_config::from_text(SAMPLE), Ok(None));
    }

    #[test]
    fn second_and_third_ini_change_one_thing_each() {
        use higanr_core::pacing::{Mode, mode_from_ini};
        const FIRST: &str = include_str!("../../../scripts/e1_02_rollback.ini");
        const SECOND: &str = include_str!("../../../scripts/e1_02_rollback_2.ini");
        const THIRD: &str = include_str!("../../../scripts/e1_02_rollback_3_240.ini");
        let probe = |text: &str| settings_from_text(text).unwrap().expect("有効");
        let pacing = |text: &str| mode_from_ini(&ini::parse(text).unwrap()).unwrap();
        assert_eq!(probe(SECOND), probe(FIRST));
        assert_eq!(probe(THIRD), probe(FIRST));
        let p1 = pacing(FIRST);
        assert_eq!((p1.mode, p1.fps), (Mode::Auto, 60));
        let p2 = pacing(SECOND);
        assert_eq!((p2.mode, p2.fps), (Mode::Game, 60));
        let p3 = pacing(THIRD);
        assert_eq!((p3.mode, p3.fps), (Mode::BSpin, 240));
        for text in [SECOND, THIRD] {
            assert_eq!(crate::alloc_count::settings_from_text(text), Ok(None));
            assert_eq!(crate::net_config::from_text(text), Ok(None));
        }
    }

    #[test]
    fn script_keeps_shooting_and_never_moves_the_end_menu_cursor() {
        use higanr_core::input_model::keys;
        const SCRIPT: &str = include_str!("../../../scripts/e1_02_rollback_vs_cpu.txt");
        let s = higanr_core::script::parse(SCRIPT).expect("台本が読める");
        let start = s
            .steps
            .iter()
            .position(|st| st.note == "match_start")
            .expect("match_start の段階");
        let after = &s.steps[start + 1..];
        let forbidden = keys::UP | keys::DOWN | keys::ESC;
        for st in after {
            assert_eq!(st.p1 & forbidden, 0, "{} 行目", st.line);
            assert_eq!(st.p2, 0, "{} 行目", st.line);
            assert_eq!(
                st.cond,
                higanr_core::script::Cond::default(),
                "{} 行目",
                st.line
            );
        }
        assert!(after.iter().filter(|st| st.p1 & keys::Z != 0).count() >= 40);
        let ticks: u32 = after
            .iter()
            .map(|st| st.wait + if st.p1 != 0 { st.hold } else { 0 })
            .sum();
        assert!(
            ticks >= 2 * (1200 + 10 * (3 * 120 + 2 * (1 + 8 + 60))),
            "{ticks} tick"
        );
    }

    const T_BLOCKS: &[RbBlock] = &[
        RbBlock {
            name: "blk_a",
            ptr_at: 0x100,
            size: 64,
            source: "",
        },
        RbBlock {
            name: "blk_b",
            ptr_at: 0x104,
            size: 32,
            source: "",
        },
    ];
    const T_EFFECTS: &[RbEffect] = &[RbEffect {
        name: "fx",
        sys_ptr_at: 0x108,
        count: 2,
        extra: 1,
        source: "",
    }];
    const T_DATA: &[RbData] = &[RbData {
        name: "globals",
        start: 0x100,
        end: 0x140,
        source: "",
    }];
    /// 記録係の塊の札（本物の `0x004203F2` と同じ値）と、書き位置の欄（blk_b の +0x1C = 0x31C）。
    const SKIP_TAG: u32 = 0x0042_03F2;
    const CURSOR_AT: u32 = 0x31C;
    /// 偽の比べない所: globals の中の範囲（乱数 0x120〜0x124 と塊のポインタの欄 0x100〜0x10C の外）と、記録係の書き位置。
    const T_SKIPS: &[RbSkip] = &[
        RbSkip {
            name: "noise",
            kind: RbSkipKind::Range {
                start: 0x130,
                end: 0x134,
            },
            source: "",
        },
        RbSkip {
            name: "rec",
            kind: RbSkipKind::Cursor {
                callers: &[SKIP_TAG],
                ptr_at: 0x104,
                cursor_off: CURSOR_AT - 0x300,
            },
            source: "",
        },
    ];
    const T: Tables = Tables {
        blocks: T_BLOCKS,
        effects: T_EFFECTS,
        vm_arrays: &[],
        data: T_DATA,
        skips: T_SKIPS,
        effect_ptr: 0x0,
        effect_count: 0x4,
        effect_extra: 0x8,
        effect_elem: 8,
    };
    const ARENA_BASE: u32 = 0x8000;
    const ARENA_BYTES: u32 = 0x10000;
    const RNG: u32 = 0x120;

    struct Mem<'a>(&'a mut [u8]);
    impl ArenaMem for Mem<'_> {
        fn len(&self) -> u32 {
            self.0.len() as u32
        }
        fn read(&self, off: u32) -> u32 {
            let o = off as usize;
            u32::from_le_bytes(self.0[o..o + 4].try_into().unwrap())
        }
        fn write(&mut self, off: u32, v: u32) {
            let o = off as usize;
            self.0[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    /// 偽のゲーム: 0x0〜0x1000 の平らなメモリ（ポインタの欄・塊・要素の配列）と領域・割り当て器。1 tick の更新は語と乱数で塊を書き、
    /// ときどき領域から確保・解放して、そのポインタを blk_a に置く（ポインタの値まで比べる）。
    struct Game {
        mem: Vec<u8>,
        arena: Vec<u8>,
        state: ArenaState,
        /// 生きている領域の塊（偽物ではコピーと書き戻しで一緒に扱う）。
        live: Vec<u32>,
        live_at_snap: Vec<u32>,
        in_match: bool,
        redirect: bool,
        watch: bool,
        c: Counters,
        snap: Vec<u8>,
        d1: Vec<u8>,
        /// コピーしない所に置いた、更新に効く値（true なら毎 tick 足す —— 巻き戻しても戻らない）。
        hidden: u32,
        hidden_on: bool,
        /// この tick に領域の外の解放を起こす。
        outside_free_at: Option<u32>,
        /// この tick に、期間の中で生まれた外の塊の確保と解放を起こす。
        born_free_at: Option<u32>,
        /// この tick に、アドレスを記録できなかった外の確保を起こす。
        untracked_at: Option<u32>,
        /// この tick に、期間の中で生まれて解放されない外の確保を起こす。
        alive_at: Option<u32>,
        list_change_at: Option<u32>,
        leave_at: Option<u32>,
        /// 1 回でも書き戻した後で、この tick にマッチを出る（進め直しの間にマッチを出る形。前向きの期間では出ない）。
        leave_after_restore_at: Option<u32>,
        list_sig: u64,
        stop: bool,
        ticks: u32,
        fed: Vec<(u16, u16)>,
        published: Vec<TrialRecord>,
        now: u64,
        restores: u32,
        /// 同期と関係ない値の真似（fps の数字）: このアドレスへ毎 tick、通算の tick の下位 1 バイトを書く（巻き戻しても戻らないので D1 と D2 で必ず違う）。
        noise_at: Option<u32>,
        /// 記録係の真似 `(札, 塊の大きさ)`: 確保を向けている tick に、書き位置 `[CURSOR_AT]` が今の塊の外（塊の終わりを含む）なら、
        /// 札つきで塊を領域から取って 0 で埋め（本物の `0x00420456`〜`0x00420460`）、書き位置を塊の先頭へ。そして書き位置へ通算の tick の
        /// 下位 1 バイトを書いて 1 進める（本物は 30 tick に 1 回。偽物は毎 tick）。
        recorder: Option<(u32, u32)>,
        /// 記録係の今の塊（領域のアドレス）。偽物では `live` と同じく、コピーした時点のバックアップへ書き戻しで戻す。
        rec_chunk: Option<u32>,
        rec_chunk_at_snap: Option<u32>,
        /// 外してはいけない所の真似: 毎 tick、今の塊の `+.0`（`Some(u32::MAX)` なら書き位置の 1 つ先）へも同じ値を書く（塊の中に収まるときだけ）。
        rec_stray: Option<u32>,
        scene_req: bool,
        /// インターフェースの呼ばれた順（フレームの形の順序の検査用）。
        log: Vec<&'static str>,
    }

    impl Game {
        fn new() -> Self {
            let mut g = Game {
                mem: vec![0; 0x1000],
                arena: vec![0; ARENA_BYTES as usize],
                state: ArenaState::new(),
                live: Vec::new(),
                live_at_snap: Vec::new(),
                in_match: true,
                redirect: false,
                watch: false,
                c: Counters::default(),
                snap: Vec::new(),
                d1: Vec::new(),
                hidden: 0,
                hidden_on: false,
                outside_free_at: None,
                born_free_at: None,
                untracked_at: None,
                alive_at: None,
                list_change_at: None,
                leave_at: None,
                leave_after_restore_at: None,
                list_sig: 7,
                stop: false,
                ticks: 0,
                fed: Vec::new(),
                published: Vec::new(),
                now: 0,
                restores: 0,
                noise_at: None,
                recorder: None,
                rec_chunk: None,
                rec_chunk_at_snap: None,
                rec_stray: None,
                scene_req: false,
                log: Vec::new(),
            };
            g.w32(0x100, 0x200); // blk_a
            g.w32(0x104, 0x300); // blk_b
            g.w32(0x108, 0x400); // fx の EffectSys
            g.w32(0x400, 0x500); // 要素の配列
            g.w32(0x404, 2);
            g.w32(0x408, 1);
            g.w32(RNG, 12345);
            g
        }
        fn r32(&self, a: u32) -> u32 {
            let o = a as usize;
            u32::from_le_bytes(self.mem[o..o + 4].try_into().unwrap())
        }
        fn w32(&mut self, a: u32, v: u32) {
            let o = a as usize;
            self.mem[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn step(&mut self, p1: u16, p2: u16) {
            self.ticks += 1;
            let mut rng = self.r32(RNG);
            rng = rng
                .wrapping_mul(1_103_515_245)
                .wrapping_add(12345 + u32::from(p1));
            if self.hidden_on {
                self.hidden += 1;
                rng = rng.wrapping_add(self.hidden);
            }
            self.w32(RNG, rng);
            let i = 0x200 + (rng >> 8) % 60;
            let v = self.r32(i & !3) ^ u32::from(p2) ^ rng;
            self.w32(i & !3, v);
            self.w32(0x300 + (rng % 7) * 4, rng);
            self.w32(0x500 + (rng % 6) * 4, u32::from(p1) << 16 | u32::from(p2));
            let noise = self.ticks as u8;
            if let Some(a) = self.noise_at {
                self.mem[a as usize] = noise;
            }
            if let (true, Some((tag, size))) = (self.redirect, self.recorder) {
                let cur = self.r32(CURSOR_AT);
                let inside = self.rec_chunk.is_some_and(|c| c <= cur && cur < c + size);
                if !inside
                    && let Some(off) = self
                        .state
                        .alloc_tagged(&mut Mem(&mut self.arena), size, tag)
                {
                    self.arena[off as usize..(off + size) as usize].fill(0);
                    self.rec_chunk = Some(ARENA_BASE + off);
                    self.w32(CURSOR_AT, ARENA_BASE + off);
                }
                if let Some(c) = self.rec_chunk {
                    let cur = self.r32(CURSOR_AT);
                    if (c..c + size).contains(&cur) {
                        self.arena[(cur - ARENA_BASE) as usize] = noise;
                        self.w32(CURSOR_AT, cur + 1);
                    }
                    let stray = match self.rec_stray {
                        Some(u32::MAX) => Some(cur + 1),
                        Some(o) => Some(c + o),
                        None => None,
                    };
                    if let Some(a) = stray.filter(|a| (c..c + size).contains(a)) {
                        self.arena[(a - ARENA_BASE) as usize] = noise;
                    }
                }
            }
            if self.redirect {
                if rng.is_multiple_of(3) && !self.live.is_empty() {
                    let p = self.live.remove((rng as usize / 3) % self.live.len());
                    self.state
                        .free(&mut Mem(&mut self.arena), p - ARENA_BASE)
                        .unwrap();
                } else if let Some(off) =
                    self.state.alloc(&mut Mem(&mut self.arena), 20 + rng % 200)
                {
                    let p = ARENA_BASE + off;
                    self.arena[off as usize..off as usize + 4].copy_from_slice(&rng.to_le_bytes());
                    self.live.push(p);
                    self.w32(0x238, p);
                }
            }
            if self.outside_free_at == Some(self.ticks) && self.watch {
                self.c.out_frees += 1;
            }
            if self.born_free_at == Some(self.ticks) && self.watch {
                self.c.out_allocs += 1;
                self.c.out_frees_born += 1;
            }
            if self.untracked_at == Some(self.ticks) && self.watch {
                self.c.out_allocs += 1;
                self.c.out_untracked += 1;
            }
            if self.alive_at == Some(self.ticks) && self.watch {
                self.c.out_allocs += 1;
            }
            if self.list_change_at == Some(self.ticks) {
                self.list_sig += 1;
            }
            if self.leave_at == Some(self.ticks) {
                self.in_match = false;
            }
            if self.restores > 0 && self.leave_after_restore_at == Some(self.ticks) {
                self.in_match = false;
            }
        }
        fn regions_bytes(&self, l: &Layout, arena_len: Option<u32>) -> Vec<Vec<u8>> {
            l.regions()
                .iter()
                .map(|r| match r.kind {
                    RegionKind::Arena => self.arena[..arena_len.unwrap_or(r.len) as usize].to_vec(),
                    RegionKind::ArenaState => self
                        .state
                        .to_words()
                        .iter()
                        .flat_map(|w| w.to_le_bytes())
                        .collect(),
                    _ => self.mem[r.addr as usize..(r.addr + r.len) as usize].to_vec(),
                })
                .collect()
        }
    }

    fn put(buf: &mut Vec<u8>, l: &Layout, parts: Vec<Vec<u8>>) {
        buf.clear();
        buf.resize(l.buffer_bytes() as usize, 0);
        for (r, p) in l.regions().iter().zip(parts) {
            let o = r.buf_off as usize;
            buf[o..o + p.len()].copy_from_slice(&p);
        }
    }

    impl ProbeIo for Game {
        fn in_match(&mut self) -> bool {
            self.in_match
        }
        fn read_u32(&mut self, addr: u32) -> Option<u32> {
            ((addr as usize) + 4 <= self.mem.len()).then(|| self.r32(addr))
        }
        fn check_rw(&mut self, addr: u32, len: u32) -> bool {
            (addr as usize + len as usize) <= self.mem.len()
        }
        fn list_signature(&mut self) -> u64 {
            self.list_sig
        }
        fn arena_top(&mut self) -> u32 {
            self.state.top
        }
        fn set_redirect(&mut self, on: bool) {
            self.redirect = on;
        }
        fn set_watch(&mut self, on: bool) {
            self.watch = on;
        }
        fn counters(&mut self) -> Counters {
            Counters {
                arena_allocs: self.state.allocs,
                arena_frees: self.state.frees,
                ..self.c
            }
        }
        fn save(&mut self, l: &Layout, to: Buf) {
            let parts = self.regions_bytes(l, None);
            let mut buf = std::mem::take(match to {
                Buf::Snapshot => &mut self.snap,
                Buf::D1 => &mut self.d1,
            });
            put(&mut buf, l, parts);
            match to {
                Buf::Snapshot => {
                    self.snap = buf;
                    self.live_at_snap = self.live.clone();
                    self.rec_chunk_at_snap = self.rec_chunk;
                }
                Buf::D1 => self.d1 = buf,
            }
        }
        fn restore(&mut self, l: &Layout) {
            self.restores += 1;
            self.log.push("restore");
            let now_top = self.state.top;
            for r in l.regions() {
                let o = r.buf_off as usize;
                let src = &self.snap[o..o + r.len as usize];
                match r.kind {
                    RegionKind::Arena => {
                        self.arena[..r.len as usize].copy_from_slice(src);
                        for b in &mut self.arena[r.len as usize..now_top as usize] {
                            *b = 0;
                        }
                    }
                    RegionKind::ArenaState => {
                        let mut w = [0u32; STATE_WORDS];
                        for (i, x) in w.iter_mut().enumerate() {
                            *x = u32::from_le_bytes(src[i * 4..i * 4 + 4].try_into().unwrap());
                        }
                        self.state = ArenaState::from_words(&w);
                    }
                    _ => {
                        let a = r.addr as usize;
                        let src = src.to_vec();
                        self.mem[a..a + r.len as usize].copy_from_slice(&src);
                    }
                }
            }
            self.live = self.live_at_snap.clone();
            self.rec_chunk = self.rec_chunk_at_snap;
        }
        fn compare(&mut self, t: &Tables, l: &Layout, marks: &Marks, out: &mut [RegionResult]) {
            let now = self.regions_bytes(l, Some(self.state.top));
            let state = self.state;
            for ((r, o), cur) in l.regions().iter().zip(out.iter_mut()).zip(now) {
                let b = r.buf_off as usize;
                let d1 = &self.d1[b..b + r.len as usize];
                let skips = region_skips(t, r, d1, &cur, (&state, &Mem(&mut self.arena)), marks);
                assert_eq!(skips.dropped, 0);
                *o = RegionResult::of(r, d1, &cur, &skips);
            }
        }
        fn feed(&mut self, p1: u16, p2: u16) {
            self.log.push("feed");
            self.step(p1, p2);
        }
        fn now_us(&mut self) -> u64 {
            self.now += 10;
            self.now
        }
        fn stop_requested(&mut self) -> bool {
            self.stop
        }
        fn publish(&mut self, rec: &TrialRecord) {
            self.published.push(*rec);
        }
        fn scene_request_pending(&mut self) -> bool {
            self.scene_req
        }
        fn capture_outputs(&mut self) {
            self.log.push("capture");
        }
    }

    fn settings() -> Settings {
        let mut ks = [0u32; MAX_KS];
        ks[0] = 1;
        ks[1] = 4;
        Settings {
            start_ticks: 5,
            ks,
            nk: 2,
            trials_per_k: 2,
            gap_ticks: 3,
            arena_bytes: ARENA_BYTES,
            resim: ResimMode::Realtime,
        }
    }

    fn frame_settings() -> Settings {
        Settings {
            resim: ResimMode::Frame,
            ..settings()
        }
    }

    /// 台本の真似のワード（tick ごとに違う。進め直しで台本のワードを使うと、記録したワードと違って状態がずれる）。
    fn script_word(t: u32) -> (u16, u16) {
        (
            ((t * 7) % 5) as u16 | 1,
            (t % 3) as u16 + (t / 5 % 2) as u16 * 8,
        )
    }

    fn run(g: &mut Game, p: &mut Probe, ticks: u32, gap_at: Option<u32>) {
        for t in 0..ticks {
            if p.on_tick(g).is_some() {
                continue;
            }
            if gap_at == Some(t) {
                g.step(0, 0); // poll_all の入力（台本のワードではない）
                continue;
            }
            let (a, b) = script_word(t);
            g.step(a, b);
            p.after_feed(a, b);
            g.fed.push((a, b));
        }
    }

    fn outcomes(g: &Game) -> Vec<Outcome> {
        g.published.iter().map(|r| r.outcome).collect()
    }

    #[test]
    fn a_deterministic_game_matches_every_trial() {
        let mut g = Game::new();
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 400, None);
        assert!(p.finished());
        assert!(!g.redirect, "終えた後も向けている");
        assert_eq!(outcomes(&g), vec![Outcome::Match; 4]);
        let ks: Vec<u32> = g.published.iter().map(|r| r.k).collect();
        assert_eq!(ks, vec![1, 4, 1, 4]);
        for r in &g.published {
            assert_eq!(r.n, 6, "範囲の数");
            assert_eq!(r.fwd, r.rep, "進めた期間と進め直した期間の数が違う: {r:?}");
            assert_eq!(r.top_d1, r.top_d2);
        }
        let allocs: u32 = g.published.iter().map(|r| r.fwd.arena_allocs).sum();
        let frees: u32 = g.published.iter().map(|r| r.fwd.arena_frees).sum();
        assert!(
            allocs > 0 && frees > 0,
            "領域の確保・解放を期間の中で通っていない（毒: 恒真）"
        );
        assert_eq!(g.published[0].match_tick, 6);
        let lines = trial_lines(&T, p.plan(), &g.published[1]);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("回 1（k = 4") && lines[0].contains("一致"),
            "{lines:?}"
        );
        assert_eq!(
            lines[1],
            "巻き戻し:   書き位置 rec S 0x00000000 → D1 0x00000000 / D2 0x00000000"
        );
    }

    #[test]
    fn hidden_state_shows_up_as_a_mismatch_with_the_range() {
        let mut g = Game::new();
        g.hidden_on = true;
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 400, None);
        assert_eq!(outcomes(&g), vec![Outcome::Mismatch; 4]);
        let r = &g.published[0];
        let bad: Vec<&str> = r
            .regions()
            .iter()
            .filter(|x| !x.same())
            .map(|x| region_name(&T, x.kind))
            .collect();
        assert!(bad.contains(&"globals"), "{bad:?}");
        let lines = trial_lines(&T, p.plan(), r);
        assert!(
            lines.iter().any(|l| l.contains("違う範囲 globals")),
            "{lines:?}"
        );
        let sum = summary_lines(&T, &settings(), &g.published);
        assert!(sum[0].contains("一致 0 / 違い 4 / 捨てた 0"), "{sum:?}");
        assert!(sum.iter().any(|l| l.contains("globals 4")), "{sum:?}");
    }

    /// `Game` を包み、`compare_draw` だけ上書きする偽（E1-3b のインターフェースの検査用。診断の中身は `rollback_draw` の検査で見る）。
    struct DrawGame {
        g: Game,
        seen: Vec<SkipDraw>,
    }

    impl ProbeIo for DrawGame {
        fn in_match(&mut self) -> bool {
            self.g.in_match()
        }
        fn read_u32(&mut self, addr: u32) -> Option<u32> {
            self.g.read_u32(addr)
        }
        fn check_rw(&mut self, addr: u32, len: u32) -> bool {
            self.g.check_rw(addr, len)
        }
        fn list_signature(&mut self) -> u64 {
            self.g.list_signature()
        }
        fn arena_top(&mut self) -> u32 {
            self.g.arena_top()
        }
        fn set_redirect(&mut self, on: bool) {
            self.g.set_redirect(on)
        }
        fn set_watch(&mut self, on: bool) {
            self.g.set_watch(on)
        }
        fn counters(&mut self) -> Counters {
            self.g.counters()
        }
        fn save(&mut self, l: &Layout, to: Buf) {
            self.g.save(l, to)
        }
        fn restore(&mut self, l: &Layout) {
            self.g.restore(l)
        }
        fn compare(&mut self, t: &Tables, l: &Layout, marks: &Marks, out: &mut [RegionResult]) {
            self.g.compare(t, l, marks, out)
        }
        fn compare_draw(
            &mut self,
            t: &Tables,
            l: &Layout,
            marks: &Marks,
            out: &mut [RegionResult],
            skip: SkipDraw,
            diag: &mut DrawDiag,
        ) {
            self.g.compare(t, l, marks, out);
            self.seen.push(skip);
            diag.valid = true;
            diag.mode = skip;
        }
        fn feed(&mut self, p1: u16, p2: u16) {
            self.g.feed(p1, p2)
        }
        fn now_us(&mut self) -> u64 {
            self.g.now_us()
        }
        fn stop_requested(&mut self) -> bool {
            self.g.stop_requested()
        }
        fn publish(&mut self, rec: &TrialRecord) {
            self.g.publish(rec)
        }
    }

    #[test]
    fn probe_passes_the_skip_mode_to_the_compare_hook() {
        for mode in [None, Some(SkipDraw::Dead), Some(SkipDraw::None)] {
            let mut d = DrawGame {
                g: Game::new(),
                seen: Vec::new(),
            };
            let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
            assert_eq!(p.skip_draw(), SkipDraw::None, "既定は none");
            if let Some(m) = mode {
                p.set_skip_draw(m);
            }
            for t in 0..400 {
                if p.on_tick(&mut d).is_some() {
                    continue;
                }
                let (a, b) = script_word(t);
                d.g.step(a, b);
                p.after_feed(a, b);
                d.g.fed.push((a, b));
            }
            let want = mode.unwrap_or(SkipDraw::None);
            assert_eq!(d.seen, vec![want; 4], "比べの回数と設定");
            assert_eq!(outcomes(&d.g), vec![Outcome::Match; 4]);
            for r in &d.g.published {
                assert!(r.draw.valid && r.draw.mode == want, "{:?}", r.draw.mode);
            }
        }
        let mut g = Game::new();
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        p.set_skip_draw(SkipDraw::Dead);
        run(&mut g, &mut p, 400, None);
        assert!(g.published.iter().all(|r| !r.draw.valid));
    }

    #[test]
    fn unsafe_windows_are_skipped_without_restoring() {
        type Case = (&'static str, fn(&mut Game), Option<u32>, Skip);
        let cases: [Case; 6] = [
            (
                "窓の終わりに生きている外の塊",
                |g| g.alive_at = Some(8),
                None,
                Skip::OutsideAlive,
            ),
            (
                "外の解放",
                |g| g.outside_free_at = Some(8),
                None,
                Skip::OutsideFree,
            ),
            (
                "アドレスを控えられなかった外の確保",
                |g| g.untracked_at = Some(8),
                None,
                Skip::OutsideFree,
            ),
            (
                "列の変化",
                |g| g.list_change_at = Some(8),
                None,
                Skip::ListChanged,
            ),
            (
                "試合を出た",
                |g| g.leave_at = Some(7),
                None,
                Skip::LeftMatch,
            ),
            ("台本の抜け", |_| {}, Some(7), Skip::ScriptGap),
        ];
        for (name, set, gap, want) in cases {
            let mut g = Game::new();
            set(&mut g);
            let mut s = settings();
            s.ks[0] = 4;
            s.nk = 1;
            let mut p = Probe::new(s, T, ARENA_BASE).unwrap();
            run(&mut g, &mut p, 12, gap);
            let first = g
                .published
                .first()
                .unwrap_or_else(|| panic!("{name}: 記録が無い"));
            assert_eq!(first.outcome, Outcome::Skipped(want), "{name}");
            assert_eq!(first.n, 0, "{name}: 捨てたのに比べた");
            assert_eq!(g.restores, 0, "{name}: 捨てたのに書き戻した");

            let lines = trial_lines(&T, p.plan(), first);
            assert!(lines[0].contains("捨てた"), "{name}: {lines:?}");
        }
        let mut g = Game::new();
        g.outside_free_at = Some(6);
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 400, None);
        assert_eq!(
            outcomes(&g),
            vec![
                Outcome::Skipped(Skip::OutsideFree),
                Outcome::Match,
                Outcome::Match,
                Outcome::Match,
                Outcome::Match
            ]
        );
    }

    #[test]
    fn a_window_whose_outside_frees_are_all_born_inside_it_is_compared() {
        let mut g = Game::new();
        g.born_free_at = Some(8);
        let mut s = settings();
        s.ks[0] = 4;
        s.nk = 1;
        let mut p = Probe::new(s, T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 40, None);
        let first = g.published.first().expect("記録が無い");
        assert_eq!(first.outcome, Outcome::Match);
        assert_eq!((first.fwd.out_allocs, first.fwd.out_frees_born), (1, 1));
        assert_eq!((first.fwd.out_frees, first.fwd.out_untracked), (0, 0));
        assert!(g.restores >= 1, "比べたのに書き戻していない");
        let lines = trial_lines(&T, p.plan(), first);
        assert!(
            lines[0].contains("窓の中で生まれた外の塊の解放 1"),
            "{lines:?}"
        );
    }

    #[test]
    fn born_set_takes_each_address_once_and_reports_when_full() {
        let b = BornSet::new();
        assert!(b.add(0x1000) && b.add(0x2000) && b.add(0x1000));
        assert!(b.add(0));
        assert_eq!(b.len(), 3);
        assert!(b.take(0x1000));
        assert_eq!(b.len(), 1, "同じアドレスの 2 枠が 1 回で消えていない");
        assert!(!b.take(0x1000));
        assert!(!b.take(0x3000));
        assert!(b.take(0x2000));
        assert_eq!(b.len(), 0);
        for i in 0..BORN_CAP as u32 {
            assert!(b.add(0x10_0000 + i * 16));
        }
        assert!(!b.add(0x9999_0000), "満ちたのに記録したと言った");
        assert!(b.take(0x10_0000 + 5 * 16));
        assert!(b.add(0x9999_0000));
        b.clear();
        assert_eq!(b.len(), 0);
        assert!(!b.take(0x9999_0000), "空にした後に前のアドレスが残った");
    }

    #[test]
    fn leaving_the_match_during_the_replay_stops_feeding_the_recorded_words() {
        let mut g = Game::new();
        g.leave_after_restore_at = Some(11);
        let mut s = settings();
        s.ks[0] = 4;
        s.nk = 1;
        let mut p = Probe::new(s, T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 30, None);
        assert!(
            g.restores >= 1,
            "書き戻しまで進んでいない（検査の期間が効いていない）"
        );
        let first = g.published.first().expect("記録が無い");
        assert_eq!(first.outcome, Outcome::Skipped(Skip::LeftMatch));
        assert_eq!(first.n, 0, "試合を出たのに比べた");
    }

    #[test]
    fn redirect_follows_the_match_and_the_stop() {
        let mut g = Game::new();
        g.in_match = false;
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 20, None);
        assert!(!g.redirect);
        assert!(g.published.is_empty());
        g.in_match = true;
        run(&mut g, &mut p, 3, None);
        assert!(g.redirect);
        g.in_match = false;
        run(&mut g, &mut p, 1, None);
        assert!(!g.redirect);
        g.in_match = true;
        run(&mut g, &mut p, 4, None);
        assert!(g.published.is_empty());
        g.stop = true;
        run(&mut g, &mut p, 50, None);
        assert!(!g.redirect, "止めた後も向けている");
        assert!(p.finished());
        assert!(g.published.is_empty(), "止めた後に回を始めた");
    }

    #[test]
    fn layout_problems_are_skipped() {
        let mut g = Game::new();
        g.w32(0x104, 0);
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 8, None);
        assert_eq!(
            g.published[0].outcome,
            Outcome::Skipped(Skip::Layout(LayoutError::Null(1)))
        );
        let lines = trial_lines(&T, p.plan(), &g.published[0]);
        assert!(lines[0].contains("blk_b のポインタが 0"), "{lines:?}");
        let mut g = Game::new();
        g.w32(0x404, 3);
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 8, None);
        assert_eq!(
            g.published[0].outcome,
            Outcome::Skipped(Skip::Layout(LayoutError::EffectShape {
                region: 2,
                count: 3,
                extra: 1
            }))
        );
    }

    #[test]
    fn plan_places_regions_back_to_back() {
        let l = Layout::plan(&T, ARENA_BASE, ARENA_BYTES).unwrap();
        let kinds: Vec<RegionKind> = l.regions().iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RegionKind::Block(0),
                RegionKind::Block(1),
                RegionKind::Effect(0),
                RegionKind::Data(0),
                RegionKind::Arena,
                RegionKind::ArenaState
            ]
        );
        let mut off = 0;
        for r in l.regions() {
            assert_eq!(r.buf_off, off);
            off += r.cap;
        }
        assert_eq!(
            l.buffer_bytes(),
            64 + 32 + 32 + 0x40 + ARENA_BYTES + STATE_BYTES
        );
    }

    #[test]
    fn compare_bytes_hides_only_the_skipped_bytes() {
        let a: Vec<u8> = (0..3 * PAGE as u32).map(|i| (i * 13) as u8).collect();
        let none = Skips::EMPTY;
        let c = compare_bytes(&a, &a, &none);
        assert_eq!((c.d1, c.d2, c.skipped), (digest(&a), digest(&a), 0));
        assert_eq!((c.diff, c.n_offs), (Diff::default(), 0));

        let mut b = a.clone();
        let far = 2 * PAGE + 5;
        for i in [7usize, 10, 16, far] {
            b[i] ^= 0xFF;
        }
        let (d, n) = diff(&a, &b, &mut [0; DIFF_OFFS]);
        let c = compare_bytes(&a, &b, &none);
        assert_eq!((c.d1, c.d2), (digest(&a), digest(&b)));
        assert_eq!(
            (c.diff, c.n_offs as usize),
            (d, n),
            "比べない所が無いときは diff と同じ"
        );

        let mut s = Skips::EMPTY;
        s.push(8, 16);
        let c = compare_bytes(&a, &b, &s);
        assert_ne!(c.d1, c.d2);
        assert_eq!(c.diff.bytes, 3);
        assert_eq!((c.diff.first, c.diff.last), (Some(7), Some(far as u32)));
        assert_eq!(&c.offs[..c.n_offs as usize], &[7, 16, far as u32]);
        assert_eq!(c.diff.pages, 2, "頁 0 を 2 つの区間で 2 回数えた");
        assert_eq!(c.skipped, 8);

        let mut s = Skips::EMPTY;
        s.push(far as u32, far as u32 + 1);
        s.push(16, 17);
        s.push(5, 12);
        s.push(9, 11);
        s.push(3, 3);
        let c = compare_bytes(&a, &b, &s);
        assert_eq!(c.d1, c.d2);
        assert_eq!((c.diff.bytes, c.skipped), (0, 7 + 1 + 1));

        let short = &a[..100];
        let long = &a[..104];
        let c = compare_bytes(short, long, &none);
        assert_eq!((c.diff.bytes, c.diff.first), (4, Some(100)));
        let mut s = Skips::EMPTY;
        s.push(100, 200);
        let c = compare_bytes(short, long, &s);
        assert_eq!((c.d1 == c.d2, c.diff.bytes, c.skipped), (true, 0, 4));
    }

    fn skips_of(
        t: &Tables,
        r: &Region,
        a1: &[u8],
        a2: &[u8],
        st: &ArenaState,
        arena: &mut [u8],
        m: &Marks,
    ) -> Vec<(u32, u32)> {
        region_skips(t, r, a1, a2, (st, &Mem(arena)), m)
            .ranges()
            .to_vec()
    }

    fn data_region(addr: u32, len: u32) -> Region {
        Region {
            kind: RegionKind::Data(0),
            addr,
            len,
            cap: len,
            buf_off: 0,
        }
    }

    #[test]
    fn range_skips_are_clipped_to_the_region() {
        const fn range(start: u32, end: u32) -> RbSkip {
            RbSkip {
                name: "r",
                kind: RbSkipKind::Range { start, end },
                source: "",
            }
        }
        const SK: &[RbSkip] = &[range(0xF0, 0x108), range(0x13C, 0x200), range(0x140, 0x300)];
        let t = Tables { skips: SK, ..T };
        let z = [0u8; 0x40];
        let st = ArenaState::new();
        let mut arena = vec![0u8; 64];
        let mut r = data_region(0x100, 0x40);
        let m = Marks::EMPTY;
        assert_eq!(
            skips_of(&t, &r, &z, &z, &st, &mut arena, &m),
            [(0, 8), (0x3C, 0x40)]
        );
        assert_eq!(
            skips_of(&T, &r, &z, &z, &st, &mut arena, &m),
            [(0x30, 0x34)]
        );
        for k in [RegionKind::Arena, RegionKind::ArenaState] {
            r.kind = k;
            assert!(skips_of(&t, &r, &z, &z, &st, &mut arena, &m).is_empty());
        }
        let mut s = Skips::EMPTY;
        for i in 0..MAX_SKIPS as u32 + 3 {
            s.push(i * 2, i * 2 + 1);
        }
        assert_eq!((s.ranges().len(), s.dropped), (MAX_SKIPS, 3));
        const ROW: RbSkip = range(0x130, 0x134);
        static FULL: [RbSkip; MAX_SKIP_ROWS] = [ROW; MAX_SKIP_ROWS];
        static OVER: [RbSkip; MAX_SKIP_ROWS + 1] = [ROW; MAX_SKIP_ROWS + 1];
        assert!(Layout::plan(&Tables { skips: &FULL, ..T }, ARENA_BASE, ARENA_BYTES).is_ok());
        assert!(Layout::plan(&Tables { skips: &OVER, ..T }, ARENA_BASE, ARENA_BYTES).is_err());
    }

    #[test]
    fn text_skips_follow_the_position_not_the_index() {
        const POS: [u32; 3] = [0x438C_0000, 0x43E9_0000, 0];
        const SK: &[RbSkip] = &[RbSkip {
            name: "fps_text",
            kind: RbSkipKind::TextAt {
                list: 0x1010,
                stride: 0x20,
                count: 8,
                text: 0x10,
                pos_off: 0x10,
                pos: POS,
            },
            source: "",
        }];
        let t = Tables { skips: SK, ..T };
        let st = ArenaState::new();
        let mut arena = vec![0u8; 64];
        let m = Marks::EMPTY;
        let r = data_region(0x1000, 0x10C);
        let put = |a: &mut [u8], k: u32, p: [u32; 3]| {
            let o = (0x10 + k * 0x20 + 0x10) as usize;
            for (i, w) in p.iter().enumerate() {
                a[o + i * 4..o + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
        };
        let mut a1 = vec![0u8; 0x10C];
        let mut a2 = vec![0u8; 0x10C];
        for a in [&mut a1, &mut a2] {
            put(a, 3, POS); // 両方で fps の位置（外す）
            put(a, 7, POS); // はみ出す枠（外さない）
        }
        put(&mut a1, 5, POS); // D1 だけ（外さない）
        put(&mut a1, 6, POS); // z だけ違う（外さない）
        put(&mut a2, 6, [POS[0], POS[1], 1]);
        assert_eq!(
            skips_of(&t, &r, &a1, &a2, &st, &mut arena, &m),
            [(0x70, 0x80)]
        );
        let r2 = data_region(0x1030, 0xDC);
        assert_eq!(
            skips_of(&t, &r2, &a1[0x30..], &a2[0x30..], &st, &mut arena, &m),
            [(0x40, 0x50)]
        );
        let mut ra = r;
        ra.kind = RegionKind::Arena;
        assert!(skips_of(&t, &ra, &a1, &a2, &st, &mut arena, &m).is_empty());
        let skips = region_skips(&t, &r, &a1, &a2, (&st, &Mem(&mut arena)), &m);
        let mut b = a2.clone();
        b[0x70] = b'5';
        b[0x71] = b'9';
        b[0x7F] = b'4';
        assert_eq!(compare_bytes(&a2, &b, &skips).diff.bytes, 0);
        for at in [0x6F, 0x80, 0x8C, 0x50] {
            let mut c = b.clone();
            c[at] ^= 0x5A;
            let d = compare_bytes(&a2, &c, &skips).diff;
            assert_eq!((d.bytes, d.first), (1, Some(at as u32)), "+0x{at:X}");
        }
    }

    #[test]
    fn cursor_span_covers_only_what_the_window_wrote() {
        let f = |s, d| cursor_span(100, 10, s, d);
        assert_eq!(f(102, 105), Some((102, 105)));
        assert_eq!(f(100, 110), Some((100, 110)));
        assert_eq!(f(102, 102), None);
        assert_eq!(f(105, 102), None);
        assert_eq!(
            f(102, 200),
            Some((102, 110)),
            "期間の中で別の塊へ取り直した（前の塊）"
        );
        assert_eq!(
            f(50, 104),
            Some((100, 104)),
            "期間の中で取り直した（後の塊）"
        );
        assert_eq!(
            f(50, 110),
            Some((100, 110)),
            "取り直した先を期間の中で書き終えた"
        );
        assert_eq!(f(50, 200), None);
        assert_eq!(f(99, 111), None);
        assert_eq!(f(110, 200), None, "S の時点で塊は書き終えていた");
    }

    #[test]
    fn ascii_pen_x_and_sprite_temp_skip_only_their_own_bytes() {
        use crate::th09_addr as a;
        let ascii = a::ROLLBACK_DATA
            .iter()
            .find(|d| d.name == "ascii")
            .expect("ascii の範囲がノートに無い");
        let r = data_region(ascii.start, ascii.end - ascii.start);
        let st = ArenaState::new();
        let m = Marks::EMPTY;
        let t = Tables::th09();
        let check = |global_addr: u32, want_same: bool, label: &str| {
            let len = r.len as usize;
            let a1 = vec![0u8; len];
            let mut a2 = a1.clone();
            let off = (global_addr - r.addr) as usize;
            a2[off] ^= 0xFF;
            let mut arena = vec![0u8; 4];
            let skips = region_skips(&t, &r, &a1, &a2, (&st, &Mem(&mut arena)), &m);
            let c = compare_bytes(&a1, &a2, &skips);
            assert_eq!(
                (c.d1 == c.d2, c.diff.bytes),
                (want_same, u32::from(!want_same)),
                "{label} @0x{global_addr:08X}"
            );
        };
        check(0x004C_E660, true, "ascii_pen_x 頭");
        check(0x004C_E663, true, "ascii_pen_x 尻");
        check(0x004C_E65F, false, "ascii_pen_x の 1 バイト前");
        check(
            0x004C_E664,
            false,
            "ascii_pen_x の 1 バイト後（半開区間の外）",
        );
        check(0x004D_C558, true, "sprite_temp 頭");
        check(0x004D_C5C7, true, "sprite_temp 尻");
        check(0x004D_C557, false, "sprite_temp の 1 バイト前");
        check(
            0x004D_C5C8,
            false,
            "sprite_temp の 1 バイト後（半開区間の外）",
        );
    }

    fn run_noise(
        t: Tables,
        noise_at: Option<u32>,
        recorder: Option<(u32, u32)>,
        stray: Option<u32>,
    ) -> Game {
        let mut g = Game::new();
        g.noise_at = noise_at;
        g.recorder = recorder;
        g.rec_stray = stray;
        let mut p = Probe::new(settings(), t, ARENA_BASE).unwrap();
        run(&mut g, &mut p, 400, None);
        assert!(p.finished());
        g
    }

    fn mismatches(r: &TrialRecord) -> Vec<(&'static str, Option<u32>)> {
        r.regions()
            .iter()
            .filter(|x| !x.same())
            .map(|x| (region_name(&T, x.kind), x.diff.first))
            .collect()
    }

    fn arena_skipped(r: &TrialRecord) -> Option<u32> {
        r.regions()
            .iter()
            .find(|x| x.kind == RegionKind::Arena)
            .map(|x| x.skipped)
    }

    #[test]
    fn skipped_bytes_only_hide_what_the_tables_name() {
        for at in [0x130, 0x133] {
            let g = run_noise(T, Some(at), None, None);
            assert_eq!(outcomes(&g), vec![Outcome::Match; 4], "0x{at:X}");
            for r in &g.published {
                let x = r.regions().iter().find(|x| x.kind == RegionKind::Data(0));
                assert_eq!(x.map(|x| x.skipped), Some(4));
                assert_eq!(arena_skipped(r), Some(0), "記録係がいないのに領域を外した");
            }
        }
        for at in [0x12F, 0x134] {
            let g = run_noise(T, Some(at), None, None);
            assert_eq!(outcomes(&g), vec![Outcome::Mismatch; 4], "0x{at:X}");
            for r in &g.published {
                assert_eq!(mismatches(r), vec![("globals", Some(at - 0x100))]);
            }
        }
    }

    #[test]
    fn recorder_bytes_are_skipped_only_where_the_cursor_moved() {
        let g = run_noise(T, None, Some((SKIP_TAG, 1000)), None);
        assert_eq!(outcomes(&g), vec![Outcome::Match; 4]);
        let chunk = g.rec_chunk.expect("記録係が塊を取った");
        let mut seen_k: Vec<u32> = g.published.iter().map(|r| r.k).collect();
        seen_k.sort_unstable();
        assert_eq!(seen_k, [1, 1, 4, 4]);
        for r in &g.published {
            assert_eq!(arena_skipped(r), Some(r.k), "回 {}", r.index);
            let [s, d1, d2] = r.marks.at[1];
            let s = s.expect("S の書き位置");
            assert_eq!((d1, d2), (Some(s + r.k), Some(s + r.k)));
            assert!(chunk < s, "S の時点で書き位置が塊の先頭より先");
            let lines = trial_lines(&T, &Layout::plan(&T, ARENA_BASE, ARENA_BYTES).unwrap(), r);
            let want = format!("比べなかった {} バイト（うち DLL の領域 {}）", 4 + r.k, r.k);
            assert!(lines[0].contains(&want), "{lines:?}");
            let want = format!(
                "巻き戻し:   書き位置 rec S 0x{s:08X} → D1 0x{:08X} / D2 0x{:08X}",
                s + r.k,
                s + r.k
            );
            assert!(lines.contains(&want), "{lines:?}");
        }
        let g = run_noise(T, None, Some((SKIP_TAG, 1000)), Some(0));
        assert_eq!(outcomes(&g), vec![Outcome::Mismatch; 4]);
        let chunk = g.rec_chunk.unwrap();
        for r in &g.published {
            assert_eq!(mismatches(r), vec![("arena", Some(chunk - ARENA_BASE))]);
        }
        let g = run_noise(T, None, Some((SKIP_TAG, 1000)), Some(u32::MAX));
        assert_eq!(outcomes(&g), vec![Outcome::Mismatch; 4]);
        for r in &g.published {
            let d = r.marks.at[1][2].unwrap();
            assert_eq!(mismatches(r), vec![("arena", Some(d - ARENA_BASE))]);
        }
        let g = run_noise(T, None, Some((SKIP_TAG + 1, 1000)), None);
        assert_eq!(outcomes(&g), vec![Outcome::Mismatch; 4]);
        for r in &g.published {
            let s = r.marks.at[1][0].unwrap();
            assert_eq!(mismatches(r), vec![("arena", Some(s - ARENA_BASE))]);
            assert_eq!(arena_skipped(r), Some(0));
        }
        const NULL_PTR: &[RbSkip] = &[RbSkip {
            name: "rec",
            kind: RbSkipKind::Cursor {
                callers: &[SKIP_TAG],
                ptr_at: 0x10C,
                cursor_off: CURSOR_AT - 0x300,
            },
            source: "",
        }];
        let t = Tables {
            skips: NULL_PTR,
            ..T
        };
        let g = run_noise(t, None, Some((SKIP_TAG, 1000)), None);
        assert_eq!(outcomes(&g), vec![Outcome::Mismatch; 4]);
        for r in &g.published {
            assert_eq!(r.marks.at[0], [None; 3]);
            assert_eq!(arena_skipped(r), Some(0));
        }
    }

    #[test]
    fn recorder_bytes_are_skipped_across_a_new_chunk() {
        let g = run_noise(T, None, Some((SKIP_TAG, 4)), None);
        assert_eq!(outcomes(&g), vec![Outcome::Match; 4]);
        let mut crossed = 0;
        for r in &g.published {
            assert_eq!(arena_skipped(r), Some(r.k), "回 {}", r.index);
            let [s, d, _] = r.marks.at[1];
            if d.unwrap() != s.unwrap() + r.k {
                crossed += 1;
            }
        }
        assert!(crossed >= 2, "k = 4 の 2 回は取り直しを跨ぐ（{crossed}）");
    }

    /// 本物の表で `fill` と `Probe` を回す偽のインターフェース（読むのはアドレスの表だけ。書き戻し・比べは呼ばれたら落ちる）。
    struct MapIo {
        words: std::collections::HashMap<u32, u32>,
        not_rw: Option<u32>,
        saves: u32,
        published: Vec<TrialRecord>,
    }

    impl MapIo {
        fn th09() -> MapIo {
            use crate::th09_addr as a;
            let mut words = std::collections::HashMap::new();
            for (i, b) in a::ROLLBACK_BLOCKS.iter().enumerate() {
                words.insert(b.ptr_at, 0x1000_0000 + i as u32 * 0x0100_0000);
            }
            for e in a::ROLLBACK_EFFECT_ARRAYS {
                let sys = words[&e.sys_ptr_at];
                words.insert(sys + a::EFFECT_ARRAY_PTR, sys + 0x0080_0000);
                words.insert(sys + a::EFFECT_ARRAY_COUNT, e.count);
                words.insert(sys + a::EFFECT_ARRAY_EXTRA, e.extra);
            }
            MapIo {
                words,
                not_rw: None,
                saves: 0,
                published: Vec::new(),
            }
        }
    }

    impl ProbeIo for MapIo {
        fn in_match(&mut self) -> bool {
            true
        }
        fn read_u32(&mut self, addr: u32) -> Option<u32> {
            self.words.get(&addr).copied()
        }
        fn check_rw(&mut self, addr: u32, _len: u32) -> bool {
            self.not_rw != Some(addr)
        }
        fn list_signature(&mut self) -> u64 {
            0
        }
        fn arena_top(&mut self) -> u32 {
            0
        }
        fn set_redirect(&mut self, _on: bool) {}
        fn set_watch(&mut self, _on: bool) {}
        fn counters(&mut self) -> Counters {
            Counters::default()
        }
        fn save(&mut self, _l: &Layout, _to: Buf) {
            self.saves += 1;
        }
        fn restore(&mut self, _l: &Layout) {
            panic!("書き戻した");
        }
        fn compare(&mut self, _t: &Tables, _l: &Layout, _m: &Marks, _out: &mut [RegionResult]) {
            panic!("比べた");
        }
        fn feed(&mut self, _p1: u16, _p2: u16) {}
        fn now_us(&mut self) -> u64 {
            0
        }
        fn stop_requested(&mut self) -> bool {
            false
        }
        fn publish(&mut self, rec: &TrialRecord) {
            self.published.push(*rec);
        }
    }

    #[test]
    fn a_bad_stats_pointer_skips_the_trial_without_copying() {
        let t = Tables::th09();
        let base = 0x5000_0000;
        let plan = Layout::plan(&t, base, 1 << 20).unwrap();
        let mut ks = [0u32; MAX_KS];
        ks[0] = 1;
        let s = Settings {
            start_ticks: 0,
            ks,
            nk: 1,
            trials_per_k: 1,
            gap_ticks: 1,
            arena_bytes: 1 << 20,
            resim: ResimMode::Realtime,
        };
        for name in ["p1_stats", "p2_stats"] {
            let i = t.blocks.iter().position(|b| b.name == name).unwrap();
            let ptr_at = t.blocks[i].ptr_at;
            let mut io = MapIo::th09();
            let mut l = plan;
            fill(&mut l, &t, &mut io, 0).unwrap();
            let r = l.regions[i];
            assert_eq!(r.kind, RegionKind::Block(i as u8));
            assert_eq!((r.addr, r.len), (io.words[&ptr_at], 0xA0));
            let null = {
                let mut io = MapIo::th09();
                io.words.insert(ptr_at, 0);
                (io, LayoutError::Null(i as u8), "のポインタが 0")
            };
            let unreadable = {
                let mut io = MapIo::th09();
                io.words.remove(&ptr_at);
                (
                    io,
                    LayoutError::Unreadable(i as u8),
                    "のポインタか欄が読めない",
                )
            };
            let not_rw = {
                let mut io = MapIo::th09();
                io.not_rw = Some(io.words[&ptr_at]);
                (
                    io,
                    LayoutError::NotWritable(i as u8),
                    "がコミット済みで書ける頁でない",
                )
            };
            for (mut io, want, text) in [null, unreadable, not_rw] {
                let mut p = Probe::new(s, t, base).unwrap();
                assert_eq!(p.on_tick(&mut io), None);
                assert_eq!(io.saves, 0, "{name}: 捨てたのにコピーした");
                let rec = io.published.first().expect("記録が無い");
                assert_eq!(rec.outcome, Outcome::Skipped(Skip::Layout(want)));
                assert_eq!(rec.n, 0);
                let lines = trial_lines(&t, p.plan(), rec);
                assert!(lines[0].contains(&format!("{name} {text}")), "{lines:?}");
            }
        }
    }

    /// 偽のゲームで、do_frame の走査 1 回ぶん（= 入力の地点 1 回 + 台本の tick 1 回）を回す `ScanIo`。本物の `RealScan`
    /// と同じ組み方: 1 回目の走査の中の入力の地点で `Probe` が D1 をコピーして書き戻し、走査の後に `take_owed` を聞き、
    /// 残りの回は入力の地点が記録した語を feed する。
    struct GameScan<'a> {
        g: &'a mut Game,
        p: &'a mut Probe,
        script_t: u32,
        calls: u32,
        /// この通算の回（0 始まり）に走査が 1 を返す（打ち切りの真似）。
        abort_at: Option<u32>,
        events: Vec<&'static str>,
    }

    impl ScanIo for GameScan<'_> {
        fn call_scan(&mut self) -> i32 {
            let n = self.calls;
            self.calls += 1;
            self.events.push("scan");
            if self.p.on_tick(self.g).is_none() {
                let (a, b) = script_word(self.script_t);
                self.script_t += 1;
                self.g.step(a, b);
                self.p.after_feed(a, b);
            }
            if self.abort_at == Some(n) { 1 } else { 9 }
        }
        fn take_owed(&mut self) -> Option<u32> {
            self.p.take_owed()
        }
        fn before_last_call(&mut self) {
            self.events.push("outputs");
            self.p.note_outputs(true);
        }
        fn after_call(&mut self, index: u32, _ret: i32) {
            self.p.note_scan(index, 10, false);
        }
        fn aborted(&mut self, _index: u32, why: rollback_resim::ScanAbort) {
            self.p.note_abort(why);
        }
    }

    fn run_frames(g: &mut Game, p: &mut Probe, abort_at: Option<u32>) -> (u32, Vec<&'static str>) {
        let mut io = GameScan {
            g,
            p,
            script_t: 0,
            calls: 0,
            abort_at,
            events: Vec::new(),
        };
        let mut frames = 0;
        while !io.p.finished() && frames < 1000 {
            run_scan(&mut io);
            frames += 1;
        }
        (frames, io.events)
    }

    fn run_realtime_frames(g: &mut Game, p: &mut Probe) -> u32 {
        let mut frames = 0;
        let mut t = 0;
        while !p.finished() && frames < 1000 {
            if p.on_tick(g).is_none() {
                let (a, b) = script_word(t);
                t += 1;
                g.step(a, b);
                p.after_feed(a, b);
            }
            frames += 1;
        }
        frames
    }

    use crate::rollback_resim::{ScanIo, run_scan};

    #[test]
    fn frame_form_matches_and_uses_fewer_frames() {
        let mut g = Game::new();
        let mut p = Probe::new(frame_settings(), T, ARENA_BASE).unwrap();
        let (frames, events) = run_frames(&mut g, &mut p, None);
        assert!(p.finished());
        assert!(!g.redirect, "終えた後も向けている");
        assert_eq!(outcomes(&g), vec![Outcome::Match; 4]);
        let ks: Vec<u32> = g.published.iter().map(|r| r.k).collect();
        assert_eq!(ks, vec![1, 4, 1, 4]);
        for r in &g.published {
            assert_eq!(r.fwd, r.rep, "進めた期間と進め直した期間の数が違う: {r:?}");
            assert!(r.resim.frame);
            assert_eq!(r.resim.calls, r.k - 1, "k = {} の追加の走査の数", r.k);
            assert_eq!(r.resim.total_us, 10 * (r.k - 1));
            assert_eq!(r.resim.first_us, 10);
            assert_eq!(r.resim.abort, None);
            assert_eq!(
                r.resim.outputs_restored,
                (r.k > 1).then_some(true),
                "k = {}: 出力を戻すのは途中の回がある k > 1 だけ",
                r.k
            );
        }
        let mut g2 = Game::new();
        let mut p2 = Probe::new(settings(), T, ARENA_BASE).unwrap();
        let realtime = run_realtime_frames(&mut g2, &mut p2);
        assert_eq!(outcomes(&g2), vec![Outcome::Match; 4]);
        assert_eq!(
            realtime - frames,
            6,
            "realtime {realtime} フレーム / frame {frames} フレーム"
        );
        assert_eq!(
            events.iter().filter(|e| **e == "scan").count() as u32,
            frames + 6
        );
        assert_eq!(events.iter().filter(|e| **e == "outputs").count(), 2);
        assert_eq!(g.r32(RNG), g2.r32(RNG));
        let recs = &g.published;
        let s = rollback_resim::summary_lines(recs, &[10, 20, 30], 0).join("\n");
        assert!(
            s.contains("書き戻しまで進んだ回 4（一致 4 / 違い 0）"),
            "{s}"
        );
        assert!(rollback_resim::summary_lines(&g2.published, &[], 0).is_empty());
    }

    #[test]
    fn frame_form_captures_outputs_before_the_restore_and_realtime_does_not() {
        let mut ks = [0u32; MAX_KS];
        ks[0] = 4;
        let s = Settings {
            ks,
            nk: 1,
            trials_per_k: 1,
            ..frame_settings()
        };
        let mut g = Game::new();
        let mut p = Probe::new(s, T, ARENA_BASE).unwrap();
        run_frames(&mut g, &mut p, None);
        let i = g
            .log
            .iter()
            .position(|e| *e == "capture")
            .expect("取り込んでいない");
        assert_eq!(&g.log[i..i + 3], ["capture", "restore", "feed"]);
        assert_eq!(g.log.iter().filter(|e| **e == "capture").count(), 1);
        assert_eq!(g.log.iter().filter(|e| **e == "feed").count(), 4);
        let mut g = Game::new();
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run_realtime_frames(&mut g, &mut p);
        assert!(!g.log.contains(&"capture"));
        let mut ks = [0u32; MAX_KS];
        ks[0] = 1;
        let one = Settings {
            ks,
            nk: 1,
            trials_per_k: 1,
            ..frame_settings()
        };
        let mut g = Game::new();
        let mut p = Probe::new(one, T, ARENA_BASE).unwrap();
        run_frames(&mut g, &mut p, None);
        assert!(!g.log.contains(&"capture"));
        assert_eq!(outcomes(&g), vec![Outcome::Match]);
    }

    #[test]
    fn a_pending_scene_request_skips_only_the_frame_form() {
        let mut g = Game::new();
        g.scene_req = true;
        let mut s = frame_settings();
        s.trials_per_k = 1;
        let mut p = Probe::new(s, T, ARENA_BASE).unwrap();
        run_frames(&mut g, &mut p, None);
        let first = g.published.first().expect("記録が無い");
        assert_eq!(first.outcome, Outcome::Skipped(Skip::SceneRequest));
        assert_eq!(first.n, 0, "捨てたのに比べた");
        assert_eq!(g.restores, 0, "捨てたのに書き戻した");
        assert!(!g.log.contains(&"capture"));
        assert!(
            g.published
                .iter()
                .all(|r| r.outcome == Outcome::Skipped(Skip::SceneRequest)),
            "要求が立っている間に進め直した"
        );
        let lines = trial_lines(&T, p.plan(), first);
        assert!(lines[0].contains("次の場面の要求"), "{lines:?}");
        let sum = summary_lines(&T, &frame_settings(), &g.published);
        assert!(
            sum.iter().any(|l| l.contains("scene_request")),
            "捨てた理由の集計に無い: {sum:?}"
        );
        let mut g = Game::new();
        g.scene_req = true;
        let mut p = Probe::new(settings(), T, ARENA_BASE).unwrap();
        run_realtime_frames(&mut g, &mut p);
        assert_eq!(outcomes(&g), vec![Outcome::Match; 4]);
    }

    #[test]
    fn an_aborted_resim_falls_back_to_realtime_and_still_compares() {
        let mut ks = [0u32; MAX_KS];
        ks[0] = 4;
        let s = Settings {
            ks,
            nk: 1,
            trials_per_k: 1,
            ..frame_settings()
        };
        let mut g = Game::new();
        let mut p = Probe::new(s, T, ARENA_BASE).unwrap();
        let (frames_abort, _) = run_frames(&mut g, &mut p, Some(9));
        let r = g.published.first().expect("記録が無い");
        assert_eq!(r.resim.abort, Some(rollback_resim::ScanAbort::One));
        assert_eq!(r.resim.calls, 0, "打ち切った後に追加の走査を呼んだ");
        assert_eq!(r.outcome, Outcome::Match, "打ち切った後の比べが違う: {r:?}");
        let mut g2 = Game::new();
        let mut p2 = Probe::new(s, T, ARENA_BASE).unwrap();
        let (frames_ok, _) = run_frames(&mut g2, &mut p2, None);
        assert_eq!(g2.published[0].resim.abort, None);
        assert_eq!(frames_abort - frames_ok, 3, "{frames_abort} / {frames_ok}");
    }

    #[test]
    fn resim_key_and_the_frame_sample_ini() {
        use higanr_core::pacing::{Mode, mode_from_ini};
        let base = "[rollback_probe]\nenable = 1\n";
        assert_eq!(
            settings_from_text(base).unwrap().unwrap().resim,
            ResimMode::Realtime,
            "resim を書かない ini は今までどおり"
        );
        for (v, want) in [
            ("realtime", ResimMode::Realtime),
            ("frame", ResimMode::Frame),
        ] {
            let s = settings_from_text(&format!("{base}resim = {v}\n"))
                .unwrap()
                .unwrap();
            assert_eq!(s.resim, want);
        }
        for bad in ["Frame", "", "frames", "1", "both"] {
            assert!(
                settings_from_text(&format!("{base}resim = {bad}\n")).is_err(),
                "{bad:?} を通した"
            );
        }
        const SECOND: &str = include_str!("../../../scripts/e1_02_rollback_2.ini");
        const FRAME: &str = include_str!("../../../scripts/e1_03_resim_frame.ini");
        let f = settings_from_text(FRAME).unwrap().expect("有効");
        let s2 = settings_from_text(SECOND).unwrap().expect("有効");
        assert_eq!(f.resim, ResimMode::Frame);
        assert_eq!(
            Settings {
                resim: ResimMode::Realtime,
                ..f
            },
            s2,
            "2 回目との差分は resim だけのはず"
        );
        let p = mode_from_ini(&ini::parse(FRAME).unwrap()).unwrap();
        assert_eq!((p.mode, p.fps), (Mode::Game, 60));
        assert_eq!(crate::alloc_count::settings_from_text(FRAME), Ok(None));
        assert_eq!(crate::net_config::from_text(FRAME), Ok(None));
    }

    #[test]
    fn skipdraw_ini_differs_from_the_frame_ini_by_one_line() {
        use higanr_core::pacing::{Mode, mode_from_ini};
        const FRAME: &str = include_str!("../../../scripts/e1_03_resim_frame.ini");
        const B: &str = include_str!("../../../scripts/e1_03b_resim_frame_skipdraw.ini");
        const RUN: &str = include_str!("../../../scripts/runs/e1_3b_frame.run");
        let body = |t: &str| -> Vec<String> {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        };
        let (mut fb, mut bb) = (body(FRAME), body(B));
        let extra: Vec<String> = bb.iter().filter(|l| !fb.contains(l)).cloned().collect();
        assert_eq!(extra, vec!["skip_draw_fields = dead".to_string()]);
        bb.retain(|l| l != "skip_draw_fields = dead");
        fb.sort();
        bb.sort();
        assert_eq!(fb, bb, "E1-3 の ini の行が減った");
        assert_eq!(skip_draw_from_text(B), Ok(SkipDraw::Dead));
        assert_eq!(skip_draw_from_text(FRAME), Ok(SkipDraw::None));
        assert_eq!(
            settings_from_text(B).unwrap().expect("有効"),
            settings_from_text(FRAME).unwrap().expect("有効")
        );
        let p = mode_from_ini(&ini::parse(B).unwrap()).unwrap();
        assert_eq!((p.mode, p.fps), (Mode::Game, 60));
        assert_eq!(crate::alloc_count::settings_from_text(B), Ok(None));
        assert_eq!(crate::net_config::from_text(B), Ok(None));
        assert!(
            RUN.lines()
                .any(|l| l == "ini = scripts/e1_03b_resim_frame_skipdraw.ini"),
            ".run が E1-3b の ini を指していない"
        );
    }

    #[test]
    fn all_ini_differs_from_the_skipdraw_ini_by_one_line() {
        use higanr_core::pacing::{Mode, mode_from_ini};
        const B: &str = include_str!("../../../scripts/e1_03b_resim_frame_skipdraw.ini");
        const C: &str = include_str!("../../../scripts/e1_03c_resim_frame_all.ini");
        const RUN_B: &str = include_str!("../../../scripts/runs/e1_3b_frame.run");
        const RUN_C: &str = include_str!("../../../scripts/runs/e1_3c_frame.run");
        let body = |t: &str| -> Vec<String> {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        };
        let (mut bb, mut cc) = (body(B), body(C));
        let extra: Vec<String> = cc.iter().filter(|l| !bb.contains(l)).cloned().collect();
        assert_eq!(extra, vec!["skip_draw_fields = all".to_string()]);
        let gone: Vec<String> = bb.iter().filter(|l| !cc.contains(l)).cloned().collect();
        assert_eq!(gone, vec!["skip_draw_fields = dead".to_string()]);
        bb.sort();
        cc.sort();
        assert_eq!(bb.len(), cc.len(), "行の数が違う");
        assert_eq!(skip_draw_from_text(C), Ok(SkipDraw::All));
        assert_eq!(skip_draw_from_text(B), Ok(SkipDraw::Dead));
        assert_eq!(
            settings_from_text(C).unwrap().expect("有効"),
            settings_from_text(B).unwrap().expect("有効"),
            "除外以外の設定（k・回数・進め直しの形）が動いた"
        );
        let p = mode_from_ini(&ini::parse(C).unwrap()).unwrap();
        assert_eq!((p.mode, p.fps), (Mode::Game, 60));
        assert_eq!(crate::alloc_count::settings_from_text(C), Ok(None));
        assert_eq!(crate::net_config::from_text(C), Ok(None));
        assert!(
            RUN_C
                .lines()
                .any(|l| l == "ini = scripts/e1_03c_resim_frame_all.ini"),
            ".run が E1-3c の ini を指していない"
        );
        let keys = |t: &str| -> Vec<String> {
            body(t)
                .into_iter()
                .filter(|l| !l.starts_with("ini =") && !l.starts_with("purpose ="))
                .collect()
        };
        assert_eq!(keys(RUN_B), keys(RUN_C));
        assert!(keys(RUN_C).len() >= 3, "比べる行が無い");
    }

    #[test]
    fn lasers_run_reuses_the_all_ini_and_states_the_expected_skip_bytes() {
        const RUN_C: &str = include_str!("../../../scripts/runs/e1_3c_frame.run");
        const RUN_D: &str = include_str!("../../../scripts/runs/e1_3d_frame.run");
        let keys = |t: &str| -> Vec<String> {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter(|l| !l.starts_with("purpose ="))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            keys(RUN_C),
            keys(RUN_D),
            "ini・台本・game・時間が E1-3c と違う"
        );
        assert!(
            RUN_D
                .lines()
                .any(|l| l == "ini = scripts/e1_03c_resim_frame_all.ini")
        );
        let per = rollback_draw::all_skip_bytes_per_compare(crate::th09_addr::ROLLBACK_SLOT_ARRAYS);
        assert_eq!(per, 178_400);
        let with_commas = format!("{},{:03}", per / 1000, per % 1000);
        assert!(
            RUN_D.contains(&format!("枠の配列の分 = {with_commas} × 比べた回")),
            "合格の数が表と違う: {with_commas}"
        );
    }
}
