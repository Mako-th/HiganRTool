//! 花映塚の確保・解放を、呼び出し元ごと・スレッドの別ごとに数える観測の表と集計（pure。入口のアセンブリと Win32 は `alloc_win.rs`）。
//! 確保を呼び出し元ごとに数える考えは giuroll を参考にした（コードはコピーしていない）。
//!
//! - 入口は 5 つ（`th09_addr::ALLOC_SITES`）。どれも関数の先頭に inline detour を書き（`detour::install_detour` を、トランポリンの置き場と
//!   入口のアドレスだけ差し替えた `SiteDetourEnv` で呼ぶ）、入口のアセンブリは `[esp]`（呼び出し元の戻りアドレス）を数える。
//! - 表は「入口 × スレッドの別（ゲームのスレッド / それ以外）」の 10 本。1 本は開アドレス法の 1024 枠（鍵 = 戻りアドレス、値 = 回数）。
//!   入口はロックを取らず、空きの枠を `lock cmpxchg` で取り、回数を `lock inc` で足す。64 枠たどっても入れなければ溢れを数える。
//!   鍵は一度置いたら変えないので、読み手は鍵 → 回数の順に読めばよい。
//! - 書き出しのスレッドが 1 秒ごとに表を読み、前の秒との差を `_alloc_sec.csv`・`_alloc_calls.csv` へ書く。終わりに
//!   `_alloc_callers.csv` と `_alloc_table.csv`（場面ごとの 1 秒あたりの中央値・最大・総数・母数の秒数）を書く。
//! - 「0 件」を静かに出さない: 書けなかった入口は `installed` の列が 0、場面の秒が 0 なら `seconds` が 0 の行を出す。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;

use higanr_core::ini;
use higanr_core::patch::AddrRange;

use crate::detour::{self, DetourEnv};
use crate::hook::{HookEnv, ThreadRef, WriteDone};
use crate::log_mode::detail_out;
use crate::sites::{SiteDef, check_table, hex_bytes};
use crate::ticks::CopyRow;

// ---- 表の形（入口のアセンブリが `const` で使う）----

/// 入口の数（`th09_addr::ALLOC_SITES` の行の数と同じ）。
pub const SITE_COUNT: usize = 5;
/// スレッドの別（0 = ゲームのスレッド、1 = それ以外）。
pub const CLASS_COUNT: usize = 2;
/// 表の本数（入口 × スレッドの別）。表の番号は `site * 2 + class`。
pub const TABLE_COUNT: usize = SITE_COUNT * CLASS_COUNT;
/// 1 本の枠の数の 2 の冪。呼び出し元は exe 全体の `call` の即値で free が 119 + 180 か所なので、1024 枠なら埋まりは 3 割ほど（推定）。
pub const SLOT_BITS: u32 = 10;
pub const SLOTS: usize = 1 << SLOT_BITS;
/// 枠の番号 → 次の枠の `and` の値。
pub const SLOT_MASK: u32 = (SLOTS as u32) - 1;
/// 1 枠の語の数（鍵・回数）。入口は `[表 + 枠 * 8]` を鍵、`+ 4` を回数として触る。
pub const SLOT_WORDS: usize = 2;
/// 1 本の表のバイト数の `shl` の値（1024 枠 × 8 バイト = 8192 = 1 << 13）。
pub const TABLE_SHIFT: u32 = SLOT_BITS + 3;
/// 空きを探してたどる枠の上限。越えたら溢れに数える（入口の 1 回の手間に上限を置くため）。
pub const PROBES: u32 = 64;
/// 鍵（戻りアドレス）→ 最初に見る枠の掛け算（黄金比の 2^32 倍を奇数にした値。Knuth の乗算ハッシュ）。戻りアドレスは近いので、
/// 下位ビットをそのまま使うと固まる。入口の `imul` は符号つきの即値なので `i32`。
pub const HASH_MUL: i32 = 0x9E37_79B1_u32 as i32;
/// 掛けた後に上位 `SLOT_BITS` ビットを取る `shr` の値。
pub const HASH_SHIFT: u32 = 32 - SLOT_BITS;

const _: () = assert!(1usize << TABLE_SHIFT == SLOTS * SLOT_WORDS * 4);
const _: () = assert!(TABLE_COUNT == 10);

/// 表 10 本（80 KiB）。DLL の .bss に置かれ、入口のアセンブリが `[ALLOC_TABLES + 表の番号 << TABLE_SHIFT + 枠 * 8]` で触る。
#[repr(C, align(64))]
pub struct AllocTables {
    pub words: [AtomicU32; TABLE_COUNT * SLOTS * SLOT_WORDS],
}

pub static ALLOC_TABLES: AllocTables = AllocTables {
    words: [const { AtomicU32::new(0) }; TABLE_COUNT * SLOTS * SLOT_WORDS],
};
/// 表ごとの総数（入口が表を引く前に `lock inc`）。
pub static ALLOC_TOTALS: [AtomicU32; TABLE_COUNT] = [const { AtomicU32::new(0) }; TABLE_COUNT];
/// 表ごとの溢れ（`PROBES` 枠たどっても入れなかった回数）。
pub static ALLOC_OVERFLOW: [AtomicU32; TABLE_COUNT] = [const { AtomicU32::new(0) }; TABLE_COUNT];
/// ゲームのスレッドの ID。入口は `fs:[0x24]` とこれを比べる。**inline detour を書く前に置く**（0 のままなら全部が「それ以外」に入る）。
pub static ALLOC_MAIN_TID: AtomicU32 = AtomicU32::new(0);
/// 入口ごとのトランポリンのアドレス（`SiteDetourEnv::set_trampoline` が JMP を書く前に置く）。
pub static ALLOC_TRAMPS: [AtomicU32; SITE_COUNT] = [const { AtomicU32::new(0) }; SITE_COUNT];

pub const CLASS_NAMES: [&str; CLASS_COUNT] = ["main", "other"];

pub fn table_index(site: usize, class: usize) -> usize {
    site * CLASS_COUNT + class
}

/// 鍵が最初に見る枠（入口のアセンブリの `imul` と `shr` と同じ式。検査用）。
#[cfg(test)]
pub fn home_slot(caller: u32) -> u32 {
    caller.wrapping_mul(HASH_MUL as u32) >> HASH_SHIFT
}

/// 表と数を全部 0 に戻す（detour を書く前だけ。書いた後は入口と競る）。
pub fn reset_counters() {
    for w in ALLOC_TABLES.words.iter() {
        w.store(0, Ordering::SeqCst);
    }
    for a in ALLOC_TOTALS.iter().chain(ALLOC_OVERFLOW.iter()) {
        a.store(0, Ordering::SeqCst);
    }
}

// ---- 読み取り ----

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub totals: [u32; TABLE_COUNT],
    pub overflow: [u32; TABLE_COUNT],
    /// (表の番号, 呼び出し元, 回数)。表の番号・枠の順。
    pub slots: Vec<(u8, u32, u32)>,
}

impl Snapshot {
    pub fn slot_sums(&self) -> [u64; TABLE_COUNT] {
        let mut s = [0u64; TABLE_COUNT];
        for &(t, _, c) in &self.slots {
            s[t as usize] += c as u64;
        }
        s
    }
}

/// 表を読む。鍵を先に読み（`Acquire`）、その後で回数を読む。総数と溢れは枠の後に読むので、「総数 ≧ 枠の和 ＋ 溢れ」になる向きにずれる。
pub fn snapshot() -> Snapshot {
    let mut slots = Vec::new();
    for t in 0..TABLE_COUNT {
        for i in 0..SLOTS {
            let base = (t * SLOTS + i) * SLOT_WORDS;
            let key = ALLOC_TABLES.words[base].load(Ordering::Acquire);
            if key != 0 {
                let count = ALLOC_TABLES.words[base + 1].load(Ordering::Acquire);
                slots.push((t as u8, key, count));
            }
        }
    }
    let totals = core::array::from_fn(|t| ALLOC_TOTALS[t].load(Ordering::Acquire));
    let overflow = core::array::from_fn(|t| ALLOC_OVERFLOW[t].load(Ordering::Acquire));
    Snapshot {
        totals,
        overflow,
        slots,
    }
}

// ---- ini ----

pub const SECTION: &str = "alloc_count";
/// `seconds` を書かなかったときの長さ（9 分）。
pub const DEFAULT_SECONDS: u32 = 540;
/// `seconds` の上限（`observe::SCRIPT_MAX_SECONDS` と同じ）。
pub const MAX_SECONDS: u32 = crate::observe::SCRIPT_MAX_SECONDS;
const KEYS: [&str; 2] = ["enable", "seconds"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocSettings {
    pub seconds: u32,
}

/// ini の本文から読む。`[alloc_count]` が無い・`enable = 0` なら `Ok(None)`。知らないキーは断る（綴りの誤りで既定のまま走らないように）。
pub fn settings_from_text(text: &str) -> Result<Option<AllocSettings>, String> {
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
    let seconds = match ini.get(SECTION, "seconds") {
        None => DEFAULT_SECONDS,
        Some(v) => v
            .parse::<u32>()
            .map_err(|_| format!("seconds の値 {v:?} が数として読めない"))?,
    };
    if seconds == 0 || seconds > MAX_SECONDS {
        return Err(format!("seconds の値 {seconds} は 1〜{MAX_SECONDS}"));
    }
    Ok(Some(AllocSettings { seconds }))
}

// ---- 場面 ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SceneObs {
    /// 今の場面 `[0x4B3100 + 0x58C]`（1 = メニュー、2 = マッチ）。
    pub scene: Option<u32>,
    /// `th09_addr::LOADING`（0 でなければ読み込み中）。
    pub loading: Option<u32>,
    /// `th09_addr::GLOBAL_STATE`（bit1 = タイトルデモ）。
    pub global: Option<u32>,
    /// `th09_addr::MATCH_END_MENU`（1 = マッチの終わりのメニュー）。
    pub end_menu: Option<u8>,
    /// `th09_addr::PAUSE` の下位 1 バイト（0 でなければ一時停止）。
    pub pause: Option<u8>,
    pub wins: [Option<u32>; 2],
    /// 入力の入口を通った tick の数（`stub::SEQ`。秒ごとの差がその秒の tick の数）。
    pub tick_seq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scene {
    /// 場面 2 で、読み込み・マッチの終わりのメニュー・一時停止のどれでもない。
    Match,
    EndMenu,
    Loading,
    Pause,
    Demo,
    /// 場面 1。
    Menu,
    /// ほかの場面の値。
    Other(u32),
    /// 場面が読めない。
    Unknown,
}

impl Scene {
    pub fn label(&self) -> String {
        match self {
            Scene::Match => "match".to_string(),
            Scene::EndMenu => "end_menu".to_string(),
            Scene::Loading => "loading".to_string(),
            Scene::Pause => "pause".to_string(),
            Scene::Demo => "demo".to_string(),
            Scene::Menu => "menu".to_string(),
            Scene::Other(v) => format!("scene{v}"),
            Scene::Unknown => "unknown".to_string(),
        }
    }
}

/// 場面を決める。デモを先に見る（デモの間も場面は 2 になる）。場面 2 の中は、読み込み → マッチの終わりのメニュー → 一時停止 の順
/// （メニューの間も一時停止の下位 1 バイトは 0 のまま）。補助の値が読めなければ `Unknown`。
pub fn classify(o: &SceneObs) -> Scene {
    let Some(scene) = o.scene else {
        return Scene::Unknown;
    };
    if o.global
        .is_some_and(|g| g & crate::th09_addr::GLOBAL_STATE_DEMO != 0)
    {
        return Scene::Demo;
    }
    match scene {
        1 => Scene::Menu,
        2 => match (o.loading, o.end_menu, o.pause) {
            (Some(l), _, _) if l != 0 => Scene::Loading,
            (Some(_), Some(1), _) => Scene::EndMenu,
            (Some(_), Some(_), Some(p)) if p != 0 => Scene::Pause,
            (Some(_), Some(_), Some(_)) => Scene::Match,
            _ => Scene::Unknown,
        },
        v => Scene::Other(v),
    }
}

// ---- 秒ごとの集計 ----

/// 1 秒の期間の長さの許し（ミリ秒）。この外の期間は表の中央値・最大に入れない（`full` が 0）。
pub const FULL_MIN_MS: u64 = 900;
pub const FULL_MAX_MS: u64 = 1100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecRow {
    pub sec: u32,
    /// 数え始めからの、この期間の終わり（ミリ秒）。
    pub end_ms: u64,
    pub dur_ms: u64,
    pub start: Scene,
    pub end: Scene,
    pub obs: SceneObs,
    pub ticks: u32,
    pub counts: [u32; TABLE_COUNT],
    pub overflow: [u32; TABLE_COUNT],
    /// (表の番号, 呼び出し元, 回数)。回数が 0 でないものだけ。
    pub callers: Vec<(u8, u32, u32)>,
}

impl SecRow {
    pub fn full(&self) -> bool {
        (FULL_MIN_MS..=FULL_MAX_MS).contains(&self.dur_ms)
    }
    /// 期間の場面の名前（始めと終わりが違えば `mixed`）。
    pub fn label(&self) -> String {
        if self.start == self.end {
            self.end.label()
        } else {
            "mixed".to_string()
        }
    }
}

/// 秒ごとの差を取り、行を貯める（書き出しのスレッドの中身。Win32 に触らない）。
#[derive(Debug, Clone)]
pub struct Collector {
    prev: Snapshot,
    prev_counts: BTreeMap<(u8, u32), u32>,
    prev_ms: u64,
    prev_scene: Scene,
    prev_tick: u32,
    pub rows: Vec<SecRow>,
    pub installed: [bool; SITE_COUNT],
}

fn counts_of(s: &Snapshot) -> BTreeMap<(u8, u32), u32> {
    s.slots.iter().map(|&(t, k, c)| ((t, k), c)).collect()
}

impl Collector {
    /// 数え始め（`now_ms` は数え始めからのミリ秒。ふつうは 0）。
    pub fn new(now_ms: u64, obs: &SceneObs, snap: Snapshot, installed: [bool; SITE_COUNT]) -> Self {
        Collector {
            prev_counts: counts_of(&snap),
            prev: snap,
            prev_ms: now_ms,
            prev_scene: classify(obs),
            prev_tick: obs.tick_seq,
            rows: Vec::new(),
            installed,
        }
    }

    /// 1 つの期間を閉じる。回数は前の読みとの差（`wrapping_sub`。u32 の回数が 1 周しても差は合う）。
    pub fn sample(&mut self, now_ms: u64, obs: &SceneObs, snap: Snapshot) -> &SecRow {
        let counts = core::array::from_fn(|t| snap.totals[t].wrapping_sub(self.prev.totals[t]));
        let overflow =
            core::array::from_fn(|t| snap.overflow[t].wrapping_sub(self.prev.overflow[t]));
        let now_counts = counts_of(&snap);
        let callers = now_counts
            .iter()
            .filter_map(|(&(t, k), &c)| {
                let d = c.wrapping_sub(self.prev_counts.get(&(t, k)).copied().unwrap_or(0));
                (d != 0).then_some((t, k, d))
            })
            .collect();
        let end = classify(obs);
        let row = SecRow {
            sec: self.rows.len() as u32 + 1,
            end_ms: now_ms,
            dur_ms: now_ms.saturating_sub(self.prev_ms),
            start: self.prev_scene,
            end,
            obs: *obs,
            ticks: obs.tick_seq.wrapping_sub(self.prev_tick),
            counts,
            overflow,
            callers,
        };
        self.prev = snap;
        self.prev_counts = now_counts;
        self.prev_ms = now_ms;
        self.prev_scene = end;
        self.prev_tick = obs.tick_seq;
        self.rows.push(row);
        &self.rows[self.rows.len() - 1]
    }

    pub fn caller_totals(&self) -> BTreeMap<(u8, u32), u64> {
        let mut m = BTreeMap::new();
        for r in &self.rows {
            for &(t, k, c) in &r.callers {
                *m.entry((t, k)).or_insert(0u64) += c as u64;
            }
        }
        m
    }

    /// 場面ごとの表の行。場面は `match` / `end_menu` / `loading` / `pause` / `menu` を必ず出し（0 秒でも）、
    /// ほかは現れたものだけ。`mixed` と長さが 1 秒に近くない期間は場面の行に入れない（`all` の行にだけ入る）。
    pub fn table(&self) -> Vec<TableRow> {
        let mut labels: Vec<String> = ["match", "end_menu", "loading", "pause", "menu"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for r in &self.rows {
            let l = r.label();
            if r.full() && l != "mixed" && !labels.contains(&l) {
                labels.push(l);
            }
        }
        let mut out = Vec::new();
        for label in &labels {
            let rows: Vec<&SecRow> = self
                .rows
                .iter()
                .filter(|r| r.full() && &r.label() == label)
                .collect();
            self.table_for(label, &rows, &mut out);
        }
        let all: Vec<&SecRow> = self.rows.iter().collect();
        self.table_for("all", &all, &mut out);
        out
    }

    fn table_for(&self, label: &str, rows: &[&SecRow], out: &mut Vec<TableRow>) {
        for site in 0..SITE_COUNT {
            for class in 0..CLASS_COUNT {
                let t = table_index(site, class);
                let per_sec: Vec<u64> = rows.iter().map(|r| r.counts[t] as u64).collect();
                out.push(TableRow::new(
                    label,
                    site,
                    class,
                    CallerCol::All,
                    &per_sec,
                    self.installed[site],
                ));
                let ovf: Vec<u64> = rows.iter().map(|r| r.overflow[t] as u64).collect();
                out.push(TableRow::new(
                    label,
                    site,
                    class,
                    CallerCol::Overflow,
                    &ovf,
                    self.installed[site],
                ));
                // この場面の秒に 1 回でも出た呼び出し元（出なかった秒は 0 として中央値に入れる）。
                let mut keys: Vec<u32> = rows
                    .iter()
                    .flat_map(|r| r.callers.iter())
                    .filter(|&&(tt, _, _)| tt as usize == t)
                    .map(|&(_, k, _)| k)
                    .collect();
                keys.sort_unstable();
                keys.dedup();
                for k in keys {
                    let v: Vec<u64> = rows
                        .iter()
                        .map(|r| {
                            r.callers
                                .iter()
                                .find(|&&(tt, kk, _)| tt as usize == t && kk == k)
                                .map_or(0, |&(_, _, c)| c as u64)
                        })
                        .collect();
                    out.push(TableRow::new(
                        label,
                        site,
                        class,
                        CallerCol::Caller(k),
                        &v,
                        self.installed[site],
                    ));
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerCol {
    /// 呼び出し元を分けない合計（総数の差）。
    All,
    Overflow,
    Caller(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub scene: String,
    pub site: usize,
    pub class: usize,
    pub caller: CallerCol,
    pub seconds: usize,
    pub total: u64,
    /// 1 秒あたりの中央値（秒の数が偶数なら真ん中の 2 つの平均）。秒が 0 なら `None`。
    pub median: Option<f64>,
    pub max: Option<u64>,
    pub installed: bool,
}

pub fn median(values: &[u64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] as f64 + v[n / 2] as f64) / 2.0
    })
}

impl TableRow {
    fn new(
        scene: &str,
        site: usize,
        class: usize,
        caller: CallerCol,
        per_sec: &[u64],
        installed: bool,
    ) -> Self {
        TableRow {
            scene: scene.to_string(),
            site,
            class,
            caller,
            seconds: per_sec.len(),
            total: per_sec.iter().sum(),
            median: median(per_sec),
            max: per_sec.iter().copied().max(),
            installed,
        }
    }
}

// ---- CSV ----

pub fn site_name(sites: &[SiteDef], site: usize) -> &'static str {
    sites.get(site).map_or("?", |d| d.name)
}

pub fn sec_header(sites: &[SiteDef]) -> String {
    let mut s = String::from(
        "sec,end_ms,dur_ms,full,label,scene_start,scene_end,scene,loading,demo,end_menu,pause,wins_1p,wins_2p,ticks",
    );
    for site in 0..SITE_COUNT {
        for class in CLASS_NAMES {
            let _ = write!(s, ",{}_{class}", site_name(sites, site));
        }
    }
    s.push_str(",overflow");
    s
}

fn opt<T: core::fmt::Display>(v: Option<T>) -> String {
    v.map_or(String::new(), |x| x.to_string())
}

pub fn sec_line(r: &SecRow) -> String {
    let o = &r.obs;
    let mut s = format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        r.sec,
        r.end_ms,
        r.dur_ms,
        u8::from(r.full()),
        r.label(),
        r.start.label(),
        r.end.label(),
        opt(o.scene),
        opt(o.loading),
        opt(o
            .global
            .map(|g| u8::from(g & crate::th09_addr::GLOBAL_STATE_DEMO != 0))),
        opt(o.end_menu),
        opt(o.pause),
        opt(o.wins[0]),
        opt(o.wins[1]),
        r.ticks
    );
    for c in r.counts {
        let _ = write!(s, ",{c}");
    }
    let _ = write!(s, ",{}", r.overflow.iter().map(|&x| x as u64).sum::<u64>());
    s
}

pub const CALLS_HEADER: &str = "sec,label,site,thread,caller,count";

pub fn calls_lines(r: &SecRow, sites: &[SiteDef]) -> Vec<String> {
    r.callers
        .iter()
        .map(|&(t, k, c)| {
            let (site, class) = (t as usize / CLASS_COUNT, t as usize % CLASS_COUNT);
            format!(
                "{},{},{},{},0x{k:08X},{c}",
                r.sec,
                r.label(),
                site_name(sites, site),
                CLASS_NAMES[class]
            )
        })
        .collect()
}

pub const TABLE_HEADER: &str =
    "scene,site,thread,caller,installed,seconds,total,median_per_s,max_per_s";

pub fn table_line(r: &TableRow, sites: &[SiteDef]) -> String {
    let caller = match r.caller {
        CallerCol::All => "all".to_string(),
        CallerCol::Overflow => "overflow".to_string(),
        CallerCol::Caller(k) => format!("0x{k:08X}"),
    };
    format!(
        "{},{},{},{caller},{},{},{},{},{}",
        r.scene,
        site_name(sites, r.site),
        CLASS_NAMES[r.class],
        u8::from(r.installed),
        r.seconds,
        r.total,
        r.median.map_or(String::new(), |m| format!("{m:.1}")),
        opt(r.max)
    )
}

// ---- 呼んだ命令の読み ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// `E8 rel32`（戻りアドレスの 5 バイト前）。値は飛び先。
    Rel32 { at: u32, target: u32 },
    /// `FF 15 abs32`（6 バイト前。`call [abs]`）。値はアドレスを読む所。
    Abs { at: u32, slot: u32 },
    /// `FF /2` のレジスタ（2 バイト前 `FF D0+r`）。
    Reg { at: u32, reg: u8 },
    /// `FF /2` の `[reg + disp8]`（3 バイト前 `FF 50+r xx`。r = 4 は SIB なので除く）。
    RegDisp8 { at: u32, reg: u8, disp: i8 },
    /// どれでもない（読めなかったときも）。
    Unknown,
}

const REG_NAMES: [&str; 8] = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"];

/// 戻りアドレス `ret` と、その手前 6 バイト（`[ret - 6, ret)`。読めなければ `None`）から、呼んだ命令を読む。
pub fn decode_call(ret: u32, before: Option<[u8; 6]>) -> CallKind {
    let Some(b) = before else {
        return CallKind::Unknown;
    };
    if b[1] == 0xE8 {
        let rel = i32::from_le_bytes([b[2], b[3], b[4], b[5]]);
        return CallKind::Rel32 {
            at: ret.wrapping_sub(5),
            target: ret.wrapping_add(rel as u32),
        };
    }
    if b[0] == 0xFF && b[1] == 0x15 {
        return CallKind::Abs {
            at: ret.wrapping_sub(6),
            slot: u32::from_le_bytes([b[2], b[3], b[4], b[5]]),
        };
    }
    if b[4] == 0xFF && (0xD0..=0xD7).contains(&b[5]) {
        return CallKind::Reg {
            at: ret.wrapping_sub(2),
            reg: b[5] - 0xD0,
        };
    }
    if b[3] == 0xFF && (0x50..=0x57).contains(&b[4]) && b[4] != 0x54 {
        return CallKind::RegDisp8 {
            at: ret.wrapping_sub(3),
            reg: b[4] - 0x50,
            disp: b[5] as i8,
        };
    }
    CallKind::Unknown
}

pub const CALLERS_HEADER: &str = "site,thread,caller,call_at,kind,target,bytes_before,total";

pub fn callers_line(
    sites: &[SiteDef],
    t: u8,
    caller: u32,
    before: Option<[u8; 6]>,
    total: u64,
) -> String {
    let (site, class) = (t as usize / CLASS_COUNT, t as usize % CLASS_COUNT);
    let (at, kind, target) = match decode_call(caller, before) {
        CallKind::Rel32 { at, target } => (
            format!("0x{at:08X}"),
            "call_rel32",
            format!("0x{target:08X}"),
        ),
        CallKind::Abs { at, slot } => {
            (format!("0x{at:08X}"), "call_abs", format!("[0x{slot:08X}]"))
        }
        CallKind::Reg { at, reg } => (
            format!("0x{at:08X}"),
            "call_reg",
            REG_NAMES[reg as usize].to_string(),
        ),
        CallKind::RegDisp8 { at, reg, disp } => (
            format!("0x{at:08X}"),
            "call_mem",
            format!("[{}{disp:+}]", REG_NAMES[reg as usize]),
        ),
        CallKind::Unknown => (String::new(), "unknown", String::new()),
    };
    format!(
        "{},{},0x{caller:08X},{at},{kind},{target},{},{total}",
        site_name(sites, site),
        CLASS_NAMES[class],
        before.map_or("unreadable".to_string(), |b| hex_bytes(&b))
    )
}

// ---- 設置 ----

/// 入口ごとのアドレスとトランポリンの置き場（本物は `alloc_win`、検査では偽物）と、数えの始めと終わり。
pub trait AllocHost {
    /// `HiganRTool.ini` の本文（無ければ `Ok(None)`）。
    fn read_ini(&mut self) -> Result<Option<String>, String>;
    /// ゲームのスレッドの ID（入力の入口の tick の記録から）。上限まで待っても記録が無ければ `None`。
    fn game_thread_id(&mut self) -> Option<u32>;
    /// 書き出しのファイルを開く（回ごとに別名。既存を上書きしない）。開けなければ数えない。
    fn open_outputs(&mut self) -> Result<String, String>;
    /// 表を 0 に戻し、ゲームのスレッドの ID を置く（detour を書く前）。
    fn prepare_counters(&mut self, main_tid: u32);
    /// 入口 `site` のアセンブリのアドレス。
    fn entry_address(&self, site: usize) -> u32;
    /// 入口 `site` のトランポリンのアドレスを置く（JMP を書く前）。
    fn set_trampoline(&mut self, site: usize, addr: u32);
    /// 書き出しのスレッドを起こす（detour を書いた後）。
    fn start_writer(
        &mut self,
        s: &AllocSettings,
        installed: [bool; SITE_COUNT],
    ) -> Result<(), String>;
    /// 書き出しのスレッドを止めて待ち、終わりの集計の文を返す（起こしていなければ `None`）。
    fn finish(&mut self) -> Option<Vec<String>>;
}

/// `detour::install_detour` に渡す、入口 1 つぶんの `DetourEnv`。スレッドの停止・読み書き・トランポリンの頁は包んだ本物へ渡し、
/// トランポリンの置き場と入口のアドレスだけを差し替える。入力の入口の静的変数を置く口は何もしない（入力のフックを書き換えないように）。
pub struct SiteDetourEnv<'a> {
    pub inner: &'a mut dyn DetourEnv,
    pub host: &'a mut dyn AllocHost,
    pub site: usize,
}

impl HookEnv for SiteDetourEnv<'_> {
    fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
        self.inner.open_other_threads()
    }
    fn suspend(&mut self, t: ThreadRef) -> bool {
        self.inner.suspend(t)
    }
    fn resume(&mut self, t: ThreadRef) -> bool {
        self.inner.resume(t)
    }
    fn close(&mut self, t: ThreadRef) {
        self.inner.close(t)
    }
    fn eip(&mut self, t: ThreadRef) -> Option<u32> {
        self.inner.eip(t)
    }
    fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
        self.inner.read_code(addr, buf)
    }
    fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
        self.inner.write_code(addr, bytes)
    }
    fn set_original_target(&mut self, _target: u32) {}
    fn set_window(&mut self, _addr: u32) {}
    fn set_copy_table(&mut self, _rows: &[CopyRow]) {}
    fn set_render_only_exit(&mut self, _addr: u32) {}
    fn entry_address(&self) -> u32 {
        self.host.entry_address(self.site)
    }
    fn match_entry_address(&self) -> u32 {
        self.host.entry_address(self.site)
    }
    fn set_match_original(&mut self, _target: u32) {}
    fn sleep_ms(&mut self, ms: u32) {
        self.inner.sleep_ms(ms)
    }
    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }
    fn counter(&self) -> u32 {
        self.inner.counter()
    }
}

impl DetourEnv for SiteDetourEnv<'_> {
    fn alloc_trampoline(&mut self, len: usize) -> Result<u32, u32> {
        self.inner.alloc_trampoline(len)
    }
    fn write_and_seal_trampoline(&mut self, addr: u32, bytes: &[u8]) -> Result<(), u32> {
        self.inner.write_and_seal_trampoline(addr, bytes)
    }
    fn free_trampoline(&mut self, addr: u32) {
        self.inner.free_trampoline(addr)
    }
    fn set_trampoline(&mut self, addr: u32) {
        self.host.set_trampoline(self.site, addr)
    }
    fn detour_entry_address(&self) -> u32 {
        self.host.entry_address(self.site)
    }
}

/// 5 つの入口に detour を書く。表の検査と 5 行ぶんのバイト列の照合が全部通ったときだけ書く（1 行でも合わなければ 1 バイトも書かない）。
/// 途中の 1 つが書けなくても、書けた入口はそのまま残る（外す手順は無い）。書けたかを入口ごとに返す。
pub fn install_all(
    hook_env: &mut dyn DetourEnv,
    host: &mut dyn AllocHost,
    sites: &[SiteDef],
    forbidden: &[AddrRange],
    out: &mut dyn Write,
) -> [bool; SITE_COUNT] {
    let mut written = [false; SITE_COUNT];
    if sites.len() != SITE_COUNT {
        let _ = writeln!(
            out,
            "確保の数え: 表の行が {} 行で、入口の数 {SITE_COUNT} と違うので書かない",
            sites.len()
        );
        return written;
    }
    if let Err(e) = check_table(sites, forbidden) {
        let _ = writeln!(out, "確保の数え: 表の検査に落ちた（{e:?}）ので書かない");
        return written;
    }
    let mut all_match = true;
    for def in sites {
        let mut now = vec![0u8; def.expected.len()];
        let line = if !hook_env.read_code(def.addr, &mut now) {
            all_match = false;
            "読めない".to_string()
        } else if now != def.expected {
            all_match = false;
            format!(
                "不一致（いま {} / 期待 {}）",
                hex_bytes(&now),
                hex_bytes(def.expected)
            )
        } else {
            "一致".to_string()
        };
        let _ = writeln!(
            out,
            "確保の数え: 照合 {} 0x{:08X}: {line}",
            def.name, def.addr
        );
    }
    if !all_match {
        let _ = writeln!(
            out,
            "確保の数え: 5 地点のうち一致しない地点があるので、どこにも書かない"
        );
        return written;
    }
    for (site, def) in sites.iter().enumerate() {
        let mut env = SiteDetourEnv {
            inner: &mut *hook_env,
            host: &mut *host,
            site,
        };
        let r = detour::install_detour(&mut env, def, forbidden);
        detour::write_report(out, def, &r);
        written[site] = r.written();
    }
    written
}

/// 数える処理の入口（`session::ScriptHost::alloc_count`）の 2 つの呼び時。
pub enum AllocStep<'a> {
    /// 入力のフックを書いた後・台本の分岐を開く前。ini を読み、有効なら 5 つの入口に detour を書いて、
    /// 書き出しのスレッドを起こす。
    Arm {
        hook_env: &'a mut dyn DetourEnv,
        forbidden: &'a [AddrRange],
    },
    /// 読み取りのスレッドの計測が終わった後。書き出しのスレッドを止め、集計をログに書く。
    Finish,
}

/// ini を読み、有効なら数え始める。数え始めたら true。
pub fn arm(
    host: &mut dyn AllocHost,
    hook_env: &mut dyn DetourEnv,
    sites: &[SiteDef],
    forbidden: &[AddrRange],
    out: &mut dyn Write,
) -> bool {
    let settings = match host.read_ini() {
        Ok(None) => {
            let _ = writeln!(detail_out(out), "確保の数え: ini が無い（数えない）");
            return false;
        }
        Ok(Some(text)) => match settings_from_text(&text) {
            Ok(None) => {
                let _ = writeln!(
                    detail_out(out),
                    "確保の数え: [{SECTION}] が無いか enable = 1 でない（数えない）"
                );
                return false;
            }
            Ok(Some(s)) => s,
            Err(e) => {
                let _ = writeln!(out, "確保の数え: [{SECTION}] を読めない（{e}）。数えない");
                return false;
            }
        },
        Err(why) => {
            let _ = writeln!(out, "確保の数え: ini を読めない（{why}）。数えない");
            return false;
        }
    };
    let _ = writeln!(out, "確保の数え: 有効（seconds = {}）", settings.seconds);
    let Some(tid) = host.game_thread_id() else {
        let _ = writeln!(
            out,
            "確保の数え: ゲームのスレッドの ID が tick の記録から取れない（入力のフックを通っていない）。スレッドの別を分けられないので書かない"
        );
        return false;
    };
    match host.open_outputs() {
        Ok(desc) => {
            let _ = writeln!(out, "確保の数え: 書き出し先 {desc}");
        }
        Err(why) => {
            let _ = writeln!(out, "確保の数え: 書き出し先を開けない（{why}）。書かない");
            return false;
        }
    }
    host.prepare_counters(tid);
    let _ = writeln!(
        out,
        "確保の数え: 表を 0 にし、ゲームのスレッドの ID {tid} を置いた"
    );
    let written = install_all(hook_env, host, sites, forbidden, out);
    let n = written.iter().filter(|&&w| w).count();
    let _ = writeln!(
        out,
        "確保の数え: 5 地点のうち {n} 地点に書いた（{}）",
        sites
            .iter()
            .zip(written)
            .map(|(d, w)| format!("{}={}", d.name, u8::from(w)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if n == 0 {
        return false;
    }
    match host.start_writer(&settings, written) {
        Ok(()) => {
            let _ = writeln!(
                out,
                "確保の数え: 書き出しのスレッドを起こした（1 秒ごと、最大 {} 秒）",
                settings.seconds
            );
            true
        }
        Err(why) => {
            let _ = writeln!(
                out,
                "確保の数え: 書き出しのスレッドを起こせない（{why}）。入口は数え続けるが、ファイルには書かない"
            );
            false
        }
    }
}

pub fn finish(host: &mut dyn AllocHost, out: &mut dyn Write) {
    if let Some(lines) = host.finish() {
        for l in lines {
            let _ = writeln!(out, "確保の数え: {l}");
        }
    }
}

/// 終わりの集計の文: 入口 × スレッドの別ごとの総数・表の和・溢れと、「総数 − 表の和 − 溢れ」
/// （読みの途中で入口が進んだ分だけ 0 より大きくなりうる。負なら数え方の誤り）。
pub fn summary_lines(sites: &[SiteDef], c: &Collector, last: &Snapshot) -> Vec<String> {
    let mut v = Vec::new();
    let full = c.rows.iter().filter(|r| r.full()).count();
    let mut by_label: BTreeMap<String, usize> = BTreeMap::new();
    for r in c.rows.iter().filter(|r| r.full()) {
        *by_label.entry(r.label()).or_insert(0) += 1;
    }
    v.push(format!(
        "{} 秒の行のうち 1 秒に近い期間 {full}（場面ごと: {}）",
        c.rows.len(),
        by_label
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let sums = last.slot_sums();
    for site in 0..SITE_COUNT {
        for (class, class_name) in CLASS_NAMES.iter().enumerate() {
            let t = table_index(site, class);
            let keys = last.slots.iter().filter(|s| s.0 as usize == t).count();
            let gap = last.totals[t] as i64 - sums[t] as i64 - last.overflow[t] as i64;
            v.push(format!(
                "{} {class_name}: 書いた={} 総数 {} / 表の和 {} / 溢れ {} / 差 {gap} / 呼び出し元 {keys} 種（枠 {SLOTS} のうち）",
                site_name(sites, site),
                u8::from(c.installed[site]),
                last.totals[t],
                sums[t],
                last.overflow[t],
            ));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::th09_addr::{ALLOC_SITES, FORBIDDEN, SITES};

    const NOTE: &str = include_str!("../../../analysis/notes/th09_rollback_E1.md");

    // ---- 表（th09_addr::ALLOC_SITES）----

    #[test]
    fn alloc_sites_pass_the_table_checks_and_do_not_touch_sites() {
        assert_eq!(ALLOC_SITES.len(), SITE_COUNT);
        assert_eq!(check_table(ALLOC_SITES, FORBIDDEN), Ok(()));
        let mut both: Vec<SiteDef> = SITES.to_vec();
        both.extend_from_slice(ALLOC_SITES);
        assert_eq!(check_table(&both, FORBIDDEN), Ok(()));
        for d in ALLOC_SITES {
            assert!(
                matches!(d.kind, crate::sites::Use::Detour { .. }),
                "{} が detour の行でない",
                d.name
            );
        }
    }

    #[test]
    fn alloc_sites_appear_in_the_note() {
        let start = NOTE.find("## 9. ").expect("ノートに §9 が無い");
        let sec = &NOTE[start..];
        for d in ALLOC_SITES {
            let crate::sites::Use::Detour { instr_lens } = d.kind else {
                panic!("{} が detour の行でない", d.name);
            };
            let line = sec
                .lines()
                .find(|l| l.starts_with(&format!("| `{}` ", d.name)))
                .unwrap_or_else(|| panic!("ノート §9 の表に {} の行が無い", d.name));
            let addr = format!("`0x{:08X}`", d.addr);
            let hex = format!("`{}`", hex_bytes(d.expected));
            let lens = format!(
                "[{}]",
                instr_lens
                    .iter()
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            assert!(
                line.contains(&addr),
                "{}: 行に {addr} が無い: {line}",
                d.name
            );
            assert!(line.contains(&hex), "{}: 行に {hex} が無い: {line}", d.name);
            assert!(
                line.contains(&lens),
                "{}: 行に {lens} が無い: {line}",
                d.name
            );
        }
    }

    // ---- ini ----

    #[test]
    fn settings_are_read_strictly() {
        assert_eq!(settings_from_text(""), Ok(None));
        assert_eq!(settings_from_text("[netplay]\nenable = 1\n"), Ok(None));
        assert_eq!(settings_from_text("[alloc_count]\nenable = 0\n"), Ok(None));
        assert_eq!(settings_from_text("[alloc_count]\nseconds = 5\n"), Ok(None));
        assert_eq!(
            settings_from_text("[alloc_count]\nenable = 1\n"),
            Ok(Some(AllocSettings {
                seconds: DEFAULT_SECONDS
            }))
        );
        assert_eq!(
            settings_from_text("[ALLOC_COUNT]\nENABLE=1\nseconds=540\n"),
            Ok(Some(AllocSettings { seconds: 540 }))
        );
        assert!(settings_from_text("[alloc_count]\nenable = 1\nsecond = 5\n").is_err());
        assert!(settings_from_text("[alloc_count]\nenable = 2\n").is_err());
        assert!(settings_from_text("[alloc_count]\nenable = 1\nseconds = 0\n").is_err());
        assert!(settings_from_text("[alloc_count]\nenable = 1\nseconds = 1201\n").is_err());
        assert!(settings_from_text("[alloc_count]\nenable = 1\nseconds = 1200\n").is_ok());
        assert!(settings_from_text("[alloc_count]\nenable = 1\nseconds = x\n").is_err());
    }

    #[test]
    fn sample_ini_enables_counting_without_the_net() {
        const SAMPLE: &str = include_str!("../../../scripts/e1_01_alloc.ini");
        assert_eq!(
            settings_from_text(SAMPLE),
            Ok(Some(AllocSettings { seconds: 600 }))
        );
        assert_eq!(crate::net_config::from_text(SAMPLE), Ok(None));
    }

    #[test]
    fn script_never_moves_the_end_menu_cursor() {
        use higanr_core::input_model::keys;
        const SCRIPT: &str = include_str!("../../../scripts/e1_01_alloc_vs_cpu.txt");
        let s = higanr_core::script::parse(SCRIPT).expect("台本が読める");
        let start = s
            .steps
            .iter()
            .position(|st| st.note == "match_start")
            .expect("match_start の段");
        let after = &s.steps[start + 1..];
        assert!(
            after.len() >= 100,
            "試合の後の段が {} しかない",
            after.len()
        );
        let forbidden = keys::UP | keys::DOWN | keys::ESC;
        for st in after {
            assert_eq!(st.p1 & forbidden, 0, "{} 行目で上・下・ESC を押す", st.line);
            assert_eq!(st.p2, 0, "{} 行目で 2P を押す", st.line);
            assert_eq!(
                st.cond,
                higanr_core::script::Cond::default(),
                "{} 行目に条件",
                st.line
            );
        }
        // 押す段階が本当にある（無いと上の検査は恒真）。
        assert!(after.iter().filter(|st| st.p1 & keys::Z != 0).count() >= 80);
        let ticks: u32 = after
            .iter()
            .map(|st| st.wait + if st.p1 != 0 { st.hold } else { 0 })
            .sum();
        assert!(ticks >= 32_400, "試合の後の段が {ticks} tick");
    }

    // ---- 場面 ----

    fn obs(scene: u32, loading: u32, global: u32, end_menu: u8, pause: u8) -> SceneObs {
        SceneObs {
            scene: Some(scene),
            loading: Some(loading),
            global: Some(global),
            end_menu: Some(end_menu),
            pause: Some(pause),
            wins: [Some(0), Some(0)],
            tick_seq: 0,
        }
    }

    #[test]
    fn scenes_are_told_apart() {
        assert_eq!(classify(&obs(2, 0, 0, 0, 0)), Scene::Match);
        assert_eq!(classify(&obs(2, 0, 0, 1, 0)), Scene::EndMenu);
        assert_eq!(classify(&obs(2, 1, 0, 0, 0)), Scene::Loading);
        assert_eq!(classify(&obs(2, 1, 0, 1, 0)), Scene::Loading);
        assert_eq!(classify(&obs(2, 0, 0, 0, 1)), Scene::Pause);
        assert_eq!(classify(&obs(2, 0, 0x0E, 0, 0)), Scene::Demo);
        assert_eq!(classify(&obs(1, 0, 0x0E, 0, 0)), Scene::Demo);
        assert_eq!(classify(&obs(2, 0, 0x08, 0, 0)), Scene::Match);
        assert_eq!(classify(&obs(1, 0, 0, 0, 0)), Scene::Menu);
        assert_eq!(classify(&obs(10, 0, 0, 0, 0)), Scene::Other(10));
        assert_eq!(classify(&SceneObs::default()), Scene::Unknown);
        let mut o = obs(2, 0, 0, 0, 0);
        o.end_menu = None;
        assert_eq!(classify(&o), Scene::Unknown, "メニューが読めないのに試合");
        let mut o = obs(2, 0, 0, 0, 0);
        o.pause = None;
        assert_eq!(classify(&o), Scene::Unknown, "一時停止が読めないのに試合");
    }

    // ---- 集計 ----

    fn snap(totals: [u32; TABLE_COUNT], slots: &[(u8, u32, u32)]) -> Snapshot {
        Snapshot {
            totals,
            overflow: [0; TABLE_COUNT],
            slots: slots.to_vec(),
        }
    }

    fn t(site: usize, class: usize) -> usize {
        table_index(site, class)
    }

    #[test]
    fn collector_takes_differences_per_caller_and_table() {
        let mut tot = [0u32; TABLE_COUNT];
        tot[t(2, 0)] = 100;
        tot[t(2, 1)] = u32::MAX - 1;
        let s0 = snap(tot, &[(t(2, 0) as u8, 0x41_0000, 100)]);
        let mut o = obs(1, 0, 0, 0, 0);
        o.tick_seq = 1000;
        let mut c = Collector::new(0, &o, s0, [true; SITE_COUNT]);
        tot[t(2, 0)] = 130;
        tot[t(2, 1)] = 3;
        let s1 = snap(
            tot,
            &[
                (t(2, 0) as u8, 0x41_0000, 120),
                (t(2, 0) as u8, 0x42_0000, 10),
                (t(2, 1) as u8, 0x41_0000, 5),
            ],
        );
        let mut o1 = obs(2, 0, 0, 0, 0);
        o1.tick_seq = 1060;
        let r = c.sample(1000, &o1, s1).clone();
        assert_eq!(r.counts[t(2, 0)], 30);
        assert_eq!(r.counts[t(2, 1)], 5, "1 周の差");
        assert_eq!(r.ticks, 60);
        assert_eq!(r.label(), "mixed", "メニュー → 試合の期間");
        assert!(r.full());
        let mut callers = r.callers.clone();
        callers.sort();
        assert_eq!(
            callers,
            vec![
                (t(2, 0) as u8, 0x41_0000, 20),
                (t(2, 0) as u8, 0x42_0000, 10),
                (t(2, 1) as u8, 0x41_0000, 5),
            ]
        );
        let s2 = snap(
            tot,
            &[
                (t(2, 0) as u8, 0x41_0000, 120),
                (t(2, 0) as u8, 0x42_0000, 10),
                (t(2, 1) as u8, 0x41_0000, 5),
            ],
        );
        let r2 = c.sample(2000, &o1, s2).clone();
        assert!(r2.callers.is_empty());
        assert_eq!(r2.label(), "match");
        assert_eq!(r2.sec, 2);
    }

    #[test]
    fn table_uses_only_full_single_scene_seconds() {
        let installed = [true, true, false, true, true];
        let o_match = obs(2, 0, 0, 0, 0);
        let o_menu = obs(1, 0, 0, 0, 0);
        let mut c = Collector::new(0, &o_match, snap([0; TABLE_COUNT], &[]), installed);
        let k = t(4, 0);
        let mut tot = [0u32; TABLE_COUNT];
        let mut cnt = 0u32;
        let mut ms = 0u64;
        let steps: [(u64, u32, bool, &SceneObs); 6] = [
            (1000, 10, true, &o_match),
            (1000, 40, false, &o_match),
            (1000, 20, true, &o_match),
            (1000, 30, false, &o_match),
            (1500, 100, false, &o_match),
            (1000, 1000, false, &o_menu),
        ];
        let mut a = 0u32;
        for (dur, n, with_a, o) in steps {
            ms += dur;
            tot[k] += n;
            cnt += n;
            if with_a {
                a += 7;
            }
            let mut slots = vec![(k as u8, 0x0040_1000, cnt - a)];
            if a > 0 {
                slots.push((k as u8, 0x0040_2000, a));
            }
            c.sample(ms, o, snap(tot, &slots));
        }
        let table = c.table();
        let find = |scene: &str, site: usize, caller: CallerCol| {
            let hits: Vec<&TableRow> = table
                .iter()
                .filter(|r| {
                    r.scene == scene && r.site == site && r.class == 0 && r.caller == caller
                })
                .collect();
            assert_eq!(hits.len(), 1, "{scene} {site} {caller:?} の行の数");
            hits[0].clone()
        };
        let m = find("match", 4, CallerCol::All);
        assert_eq!(m.seconds, 4, "母数に 1.5 秒の期間か mixed の期間が入った");
        assert_eq!(m.total, 100);
        assert_eq!(m.median, Some(25.0));
        assert_eq!(m.max, Some(40));
        let ma = find("match", 4, CallerCol::Caller(0x0040_2000));
        assert_eq!(ma.seconds, 4);
        assert_eq!(ma.total, 14);
        assert_eq!(ma.median, Some(3.5), "出なかった秒を 0 として入れていない");
        assert_eq!(ma.max, Some(7));
        let all = find("all", 4, CallerCol::All);
        assert_eq!(all.seconds, 6);
        assert_eq!(all.total, 1200);
        let menu = find("menu", 4, CallerCol::All);
        assert_eq!(
            (menu.seconds, menu.total, menu.median, menu.max),
            (0, 0, None, None)
        );
        let em = find("end_menu", 0, CallerCol::All);
        assert_eq!(em.seconds, 0);
        assert!(!find("match", 2, CallerCol::All).installed);
        assert!(find("match", 3, CallerCol::All).installed);
        let line = table_line(&m, ALLOC_SITES);
        assert_eq!(line, "match,crt_free,main,all,1,4,100,25.0,40");
        let line = table_line(&menu, ALLOC_SITES);
        assert_eq!(line, "menu,crt_free,main,all,1,0,0,,");
    }

    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5.0));
        assert_eq!(median(&[9, 1, 5]), Some(5.0));
        assert_eq!(median(&[9, 1, 5, 2]), Some(3.5));
    }

    #[test]
    fn sec_line_matches_its_header() {
        let mut c = Collector::new(0, &obs(2, 0, 0, 0, 0), Snapshot::default(), [true; 5]);
        let mut s = Snapshot::default();
        s.overflow[3] = 4;
        s.overflow[7] = 1;
        let r = c.sample(1000, &obs(2, 0, 0, 0, 0), s).clone();
        let h = sec_header(ALLOC_SITES);
        let l = sec_line(&r);
        assert_eq!(h.split(',').count(), l.split(',').count());
        assert!(h.starts_with("sec,"));
        assert!(h.contains(",alloc_wrap_main,alloc_wrap_other,"));
        assert!(l.ends_with(",5"), "溢れの和が出ない: {l}");
    }

    // ---- 呼んだ命令の読み ----

    #[test]
    fn decode_the_call_before_the_return_address() {
        assert_eq!(
            decode_call(0x0040_134C, Some([0x50, 0xE8, 0x05, 0x9E, 0x07, 0x00])),
            CallKind::Rel32 {
                at: 0x0040_1347,
                target: 0x0047_B151
            }
        );
        assert_eq!(
            decode_call(0x0047_B160, Some([0x24, 0xE8, 0xC5, 0xFF, 0xFF, 0xFF])),
            CallKind::Rel32 {
                at: 0x0047_B15B,
                target: 0x0047_B125
            }
        );
        assert_eq!(
            decode_call(0x0047_B1CE, Some([0xFF, 0x15, 0x90, 0xE1, 0x48, 0x00])),
            CallKind::Abs {
                at: 0x0047_B1C8,
                slot: 0x0048_E190
            }
        );
        assert_eq!(
            decode_call(0x1000, Some([0, 0, 0, 0, 0xFF, 0xD6])),
            CallKind::Reg { at: 0xFFE, reg: 6 }
        );
        assert_eq!(
            decode_call(0x1000, Some([0, 0, 0, 0xFF, 0x50, 0xF8])),
            CallKind::RegDisp8 {
                at: 0xFFD,
                reg: 0,
                disp: -8
            }
        );
        assert_eq!(
            decode_call(0x1000, Some([0, 0, 0, 0xFF, 0x54, 0x24])),
            CallKind::Unknown
        );
        assert_eq!(decode_call(0x1000, Some([0; 6])), CallKind::Unknown);
        assert_eq!(decode_call(0x1000, None), CallKind::Unknown);
        let l = callers_line(
            ALLOC_SITES,
            t(2, 1) as u8,
            0x0040_134C,
            Some([0x50, 0xE8, 0x05, 0x9E, 0x07, 0x00]),
            12,
        );
        assert_eq!(
            l,
            "crt_malloc,other,0x0040134C,0x00401347,call_rel32,0x0047B151,50 E8 05 9E 07 00,12"
        );
        assert_eq!(l.split(',').count(), CALLERS_HEADER.split(',').count());
    }

    #[test]
    fn home_slot_is_the_top_bits_of_the_product() {
        assert_eq!(home_slot(1), 0x278);
        let p = (0x0040_1347u64 * 0x9E37_79B1u64) as u32;
        assert_eq!(home_slot(0x0040_1347), p >> 22);
        assert!(home_slot(u32::MAX) < SLOTS as u32);
    }

    // ---- 設置（偽物）----

    /// 偽の `DetourEnv` と `AllocHost`: アドレス → バイトの表を持ち、スレッドは 0 本。
    struct FakeAlloc {
        mem: BTreeMap<u32, u8>,
        writes: Vec<(u32, Vec<u8>)>,
        next_tramp: u32,
    }

    impl FakeAlloc {
        fn new() -> Self {
            let mut mem = BTreeMap::new();
            for d in ALLOC_SITES {
                for (i, &b) in d.expected.iter().enumerate() {
                    mem.insert(d.addr + i as u32, b);
                }
            }
            FakeAlloc {
                mem,
                writes: Vec::new(),
                next_tramp: 0x0090_0000,
            }
        }
    }

    impl HookEnv for FakeAlloc {
        fn open_other_threads(&mut self) -> Result<Vec<ThreadRef>, u32> {
            Ok(Vec::new())
        }
        fn suspend(&mut self, _t: ThreadRef) -> bool {
            true
        }
        fn resume(&mut self, _t: ThreadRef) -> bool {
            true
        }
        fn close(&mut self, _t: ThreadRef) {}
        fn eip(&mut self, _t: ThreadRef) -> Option<u32> {
            None
        }
        fn read_code(&mut self, addr: u32, buf: &mut [u8]) -> bool {
            for (i, b) in buf.iter_mut().enumerate() {
                match self.mem.get(&(addr + i as u32)) {
                    Some(&v) => *b = v,
                    None => return false,
                }
            }
            true
        }
        fn write_code(&mut self, addr: u32, bytes: &[u8]) -> Result<WriteDone, u32> {
            for (i, &b) in bytes.iter().enumerate() {
                self.mem.insert(addr + i as u32, b);
            }
            self.writes.push((addr, bytes.to_vec()));
            Ok(WriteDone {
                protect_restored: true,
                flushed: true,
            })
        }
        fn set_original_target(&mut self, _t: u32) {
            panic!("E1-1 が入力のフックの元の飛び先を触った");
        }
        fn set_window(&mut self, _a: u32) {
            panic!("E1-1 が窓を触った");
        }
        fn set_copy_table(&mut self, _r: &[CopyRow]) {
            panic!("E1-1 がコピーの表を触った");
        }
        fn set_render_only_exit(&mut self, _a: u32) {
            panic!("E1-1 が出口を触った");
        }
        fn entry_address(&self) -> u32 {
            0x1111_0000
        }
        fn match_entry_address(&self) -> u32 {
            0x1111_1000
        }
        fn set_match_original(&mut self, _t: u32) {
            panic!("E1-1 が開始のフックを触った");
        }
        fn sleep_ms(&mut self, _ms: u32) {}
        fn now_ms(&self) -> u64 {
            0
        }
        fn counter(&self) -> u32 {
            0
        }
    }

    impl DetourEnv for FakeAlloc {
        fn alloc_trampoline(&mut self, _len: usize) -> Result<u32, u32> {
            self.next_tramp += 0x1000;
            Ok(self.next_tramp)
        }
        fn write_and_seal_trampoline(&mut self, _addr: u32, _bytes: &[u8]) -> Result<(), u32> {
            Ok(())
        }
        fn free_trampoline(&mut self, _addr: u32) {}
        fn set_trampoline(&mut self, _addr: u32) {
            panic!("E1-1 が描画の入口のトランポリンを触った");
        }
        fn detour_entry_address(&self) -> u32 {
            0x2222_0000
        }
    }

    /// 偽の `AllocHost`（`FakeAlloc` とは別の入れ物。`install_all` は両方を同時に借りる）。
    struct FakeHost {
        ini: Result<Option<String>, String>,
        tid: Option<u32>,
        open_fail: bool,
        events: Vec<String>,
        tramps: [u32; SITE_COUNT],
        writer: Option<[bool; SITE_COUNT]>,
    }

    impl FakeHost {
        fn new() -> Self {
            FakeHost {
                ini: Ok(Some(
                    "[alloc_count]\nenable = 1\nseconds = 30\n".to_string(),
                )),
                tid: Some(4242),
                open_fail: false,
                events: Vec::new(),
                tramps: [0; SITE_COUNT],
                writer: None,
            }
        }
    }

    const FAKE_ENTRY_BASE: u32 = 0x3000_0000;

    impl AllocHost for FakeHost {
        fn read_ini(&mut self) -> Result<Option<String>, String> {
            self.events.push("ini".into());
            self.ini.clone()
        }
        fn game_thread_id(&mut self) -> Option<u32> {
            self.events.push("tid".into());
            self.tid
        }
        fn open_outputs(&mut self) -> Result<String, String> {
            self.events.push("open".into());
            if self.open_fail {
                Err("偽物".into())
            } else {
                Ok("偽物".into())
            }
        }
        fn prepare_counters(&mut self, main_tid: u32) {
            self.events.push(format!("prepare {main_tid}"));
        }
        fn entry_address(&self, site: usize) -> u32 {
            FAKE_ENTRY_BASE + site as u32 * 0x100
        }
        fn set_trampoline(&mut self, site: usize, addr: u32) {
            self.events.push(format!("tramp {site}"));
            self.tramps[site] = addr;
        }
        fn start_writer(
            &mut self,
            _s: &AllocSettings,
            installed: [bool; SITE_COUNT],
        ) -> Result<(), String> {
            self.events.push("writer".into());
            self.writer = Some(installed);
            Ok(())
        }
        fn finish(&mut self) -> Option<Vec<String>> {
            self.events.push("finish".into());
            self.writer.map(|_| vec!["集計".to_string()])
        }
    }

    fn run_arm(f: &mut FakeAlloc, h: &mut FakeHost) -> (bool, String) {
        let mut out = Vec::new();
        let ok = arm(h, f, ALLOC_SITES, FORBIDDEN, &mut out);
        (ok, String::from_utf8(out).unwrap())
    }

    #[test]
    fn arm_installs_five_detours_through_their_own_entries() {
        let mut f = FakeAlloc::new();
        let mut h = FakeHost::new();
        let (ok, log) = run_arm(&mut f, &mut h);
        assert!(ok, "{log}");
        assert_eq!(f.writes.len(), SITE_COUNT, "{log}");
        for (site, d) in ALLOC_SITES.iter().enumerate() {
            let (addr, bytes) = &f.writes[site];
            assert_eq!(*addr, d.addr);
            assert_eq!(bytes.len(), d.expected.len());
            let head: [u8; 5] = bytes[..5].try_into().unwrap();
            assert_eq!(
                higanr_core::patch::jmp_target(d.addr, &head),
                Some(FAKE_ENTRY_BASE + site as u32 * 0x100),
                "{} の E9 の飛び先",
                d.name
            );
            assert!(bytes[5..].iter().all(|&b| b == 0x90), "残りが NOP でない");
            assert_ne!(h.tramps[site], 0, "{} のトランポリンを置いていない", d.name);
        }
        assert_eq!(h.writer, Some([true; SITE_COUNT]));
        let order: Vec<&str> = h.events.iter().map(String::as_str).collect();
        assert_eq!(
            order,
            vec![
                "ini",
                "tid",
                "open",
                "prepare 4242",
                "tramp 0",
                "tramp 1",
                "tramp 2",
                "tramp 3",
                "tramp 4",
                "writer"
            ]
        );
    }

    #[test]
    fn one_mismatch_means_no_writes_at_all() {
        for (i, d) in ALLOC_SITES.iter().enumerate() {
            for unreadable in [false, true] {
                let mut f = FakeAlloc::new();
                let a = d.addr + d.expected.len() as u32 - 1;
                if unreadable {
                    f.mem.remove(&a);
                } else {
                    *f.mem.get_mut(&a).unwrap() ^= 0x01;
                }
                let mut h = FakeHost::new();
                let (ok, log) = run_arm(&mut f, &mut h);
                assert!(!ok, "{i} {unreadable}");
                assert!(
                    f.writes.is_empty(),
                    "{} が合わないのに書いた: {log}",
                    d.name
                );
                assert!(h.tramps.iter().all(|&t| t == 0));
                assert_eq!(h.writer, None);
                assert!(log.contains("どこにも書かない"), "{log}");
            }
        }
    }

    /// 検査の場合（名前と、偽の入れ物を崩す手）。ini が無い・無効・読めない、ゲームのスレッドの ID が取れない、書き出し先を開けない
    /// のどれでも、1 バイトも書かず表も 0 にしない。
    type Case = (&'static str, fn(&mut FakeHost));

    #[test]
    fn nothing_is_written_unless_every_precondition_holds() {
        let cases: [Case; 5] = [
            ("ini なし", |h| h.ini = Ok(None)),
            ("無効", |h| {
                h.ini = Ok(Some("[alloc_count]\nenable = 0\n".into()))
            }),
            ("誤り", |h| {
                h.ini = Ok(Some("[alloc_count]\nenable = 1\nx = 1\n".into()))
            }),
            ("tid なし", |h| h.tid = None),
            ("開けない", |h| h.open_fail = true),
        ];
        for (name, set) in cases {
            let mut f = FakeAlloc::new();
            let mut h = FakeHost::new();
            set(&mut h);
            let (ok, log) = run_arm(&mut f, &mut h);
            assert!(!ok, "{name}");
            assert!(f.writes.is_empty(), "{name}: 書いた: {log}");
            assert!(
                !h.events.iter().any(|e| e.starts_with("prepare")),
                "{name}: 表を 0 にした"
            );
            assert_eq!(h.writer, None, "{name}");
        }
    }

    #[test]
    fn bad_tables_are_refused() {
        let mut f = FakeAlloc::new();
        let mut h = FakeHost::new();
        let mut out = Vec::new();
        let w = install_all(&mut f, &mut h, &ALLOC_SITES[..4], FORBIDDEN, &mut out);
        assert_eq!(w, [false; SITE_COUNT]);
        let forbidden = [AddrRange {
            start: ALLOC_SITES[3].addr + 1,
            end: ALLOC_SITES[3].addr + 2,
        }];
        let w = install_all(&mut f, &mut h, ALLOC_SITES, &forbidden, &mut out);
        assert_eq!(w, [false; SITE_COUNT]);
        assert!(f.writes.is_empty());
    }

    #[test]
    fn finish_writes_only_when_started() {
        let mut h = FakeHost::new();
        let mut out = Vec::new();
        finish(&mut h, &mut out);
        assert!(out.is_empty());
        h.writer = Some([true; SITE_COUNT]);
        finish(&mut h, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "確保の数え: 集計\n");
    }

    #[test]
    fn summary_reports_the_gap() {
        let c = Collector::new(0, &obs(2, 0, 0, 0, 0), Snapshot::default(), [true; 5]);
        let mut s = Snapshot::default();
        s.totals[t(0, 1)] = 10;
        s.overflow[t(0, 1)] = 3;
        s.slots = vec![(t(0, 1) as u8, 0x1234, 6)];
        let lines = summary_lines(ALLOC_SITES, &c, &s);
        let l = lines
            .iter()
            .find(|l| l.starts_with("alloc_wrap other"))
            .unwrap();
        assert!(
            l.contains("総数 10 / 表の和 6 / 溢れ 3 / 差 1 / 呼び出し元 1 種"),
            "{l}"
        );
    }
}
