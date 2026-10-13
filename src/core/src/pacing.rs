//! フレームのペース（取り込みの形 B「フレームの始まりで待つだけ」）の期限の計算と、1 フレームの記録の行・集計（pure）。
//!
//! Win32 に触らない: 時刻は QueryPerformanceCounter の値（整数のカウント）として受け取り、待つかどうか・どこまで待つかを返すだけ。待つのは `src/dll/src/pacing_win.rs`。
//!
//! - 期限の式: 基点 `base`（最初にフレームの始まりへ来た時刻）から `G_j = base + floor(j × QPF ÷ fps)`（fps は既定 60）。64 ビット（掛け算は 128 ビット）の整数で毎回ここから作り、
//!   前の期限に 1 周期を足して作らない。足して作ると、QPF が 60 で割り切れない PC（例 3,579,545 Hz）で毎フレーム切り捨てた端数が積もってずれる。
//!   花映塚自身は QPC の下位 32 ビットを単精度で割った時計に 1/60 を足すので、周期が 0.0166626 秒などに丸められ、429.5 秒ごとに折り返す。
//! - 遅れたとき: ゲーム自身と同じく「取り戻さない」。遅れて始めたフレームの次の期限は、始めた時刻より後の最初の格子点（`first_after`）。格子（位相）はそのままで、
//!   過ぎた格子点は飛ばす（数を返す）。花映塚の `0x0042DC74`〜`0x0042DC8B` も「期限 < 今 の間 1/60 ずつ足す」ので同じ形。vpatch の `AllowShortDelay=0`（基点を今に置き直す）とは違う（測定専用の `mode = vpatch` は `pacing_vpatch` で置き直す側を再現する）。
//! - ペース（fps）: vpatch の `GameFPS` と同じく ini（`[pacing] fps`）で変えられる（仕組みだけ参考）。検査の回を速く回すため（例 240 で長いマッチを 4 倍の速さで作る）。
//!   ゲームの 1 フレームの中身はペースに依らない見込み: ゲーム自身の時計 `0x0042CC90` を呼ぶのは do_frame の始まり `0x0042DC44` と尻 `0x0042DD65` の 2 か所だけで、B は始まりを置き換える。
//!   既定の 60 なら製品の遅延の増分は 0。
#![forbid(unsafe_code)]

use crate::ini::Ini;
use crate::pacing_hybrid::{HYBRID_KEYS, HybridParams};

/// 1 秒あたりのフレーム数の既定（花映塚の `[0x490278]` = 1/60）。ini に `fps` が無ければこれ。
pub const DEFAULT_FPS: u64 = 60;
pub const MIN_FPS: u64 = 1;
/// ini の `fps` の上限。検査の用途（240 前後）に足りる値。B の始まりは残りが 1 ms（dll の `pacing::SPIN_US`）以下なら寝ずに QPC を読み続けるので、1000 では 1 フレームずっと回し読みになる。
/// QPF より大きい値は `Grid::new` / `Pace::new` が断る。
pub const MAX_FPS: u64 = 1_000;

/// `qpf` と `fps` で格子が刻めるか（fps が 0 でなく、1 周期が 1 カウント以上。満たないと格子点が重なる）。
pub fn can_tick(qpf: u64, fps: u64) -> bool {
    fps >= MIN_FPS && qpf >= fps
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    base: u64,
    qpf: u64,
    fps: u64,
}

impl Grid {
    pub fn new(base: u64, qpf: u64, fps: u64) -> Option<Grid> {
        can_tick(qpf, fps).then_some(Grid { base, qpf, fps })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn at(&self, j: u64) -> u64 {
        let off = (j as u128 * self.qpf as u128) / self.fps as u128;
        u64::try_from(off)
            .ok()
            .and_then(|o| self.base.checked_add(o))
            .unwrap_or(u64::MAX)
    }

    pub fn shifted(&self, delta: i64) -> Grid {
        let b = (self.base as i128 + delta as i128).clamp(0, u64::MAX as i128) as u64;
        Grid { base: b, ..*self }
    }

    pub fn first_after(&self, t: u64) -> u64 {
        if t < self.base {
            return 0;
        }
        // j0 = floor((t - base) × fps ÷ qpf) は at(j0) <= t を満たす。at(j0 + 1) は切り捨てで t ちょうどに落ちることがあるので、越えるまで進める（qpf >= fps なので進むのは多くても 2 回）。
        let j0 = ((t - self.base) as u128 * self.fps as u128 / self.qpf as u128) as u64;
        let mut j = j0.saturating_add(1);
        while self.at(j) <= t && j < u64::MAX {
            j += 1;
        }
        j
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Run { deadline: u64, late: bool },
    Spin { deadline: u64 },
    Sleep { deadline: u64, us: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pace {
    qpf: u64,
    fps: u64,
    grid: Option<Grid>,
    next: u64,
    /// 期限が今より 2 周期より先にあったので基点を置き直した回数（QPC が戻ったときの守り）。
    rebased: u64,
    nudge_rem: i32,
}

impl Pace {
    pub fn new(qpf: u64, fps: u64) -> Option<Pace> {
        can_tick(qpf, fps).then_some(Pace {
            qpf,
            fps,
            grid: None,
            next: 0,
            rebased: 0,
            nudge_rem: 0,
        })
    }

    pub fn qpf(&self) -> u64 {
        self.qpf
    }

    pub fn fps(&self) -> u64 {
        self.fps
    }

    pub fn grid(&self) -> Option<Grid> {
        self.grid
    }

    pub fn rebased(&self) -> u64 {
        self.rebased
    }

    /// 次のフレームから、1 周期の長さを `ppm`（100 万分率。正 = 伸ばす = 遅くなる）だけ変える: 基点を `周期 × ppm / 10^6` カウントだけ動かす（以後の期限が全部その分ずれる）。
    /// 割り切れない端数は次の呼び出しへ持ち越す（QPF が小さい環境で、小さい ppm が 0 カウントに丸められ続けないため）。動かしたカウント（符号つき）を返す。格子がまだ無ければ 0。
    pub fn nudge(&mut self, ppm: i32) -> i64 {
        let Some(g) = self.grid else {
            return 0;
        };
        let total = self.period_ceil() as i128 * i128::from(ppm) + i128::from(self.nudge_rem);
        let delta = (total / 1_000_000) as i64;
        self.nudge_rem = (total % 1_000_000) as i32;
        self.grid = Some(g.shifted(delta));
        delta
    }

    fn period_ceil(&self) -> u64 {
        self.qpf.div_ceil(self.fps)
    }

    /// いま待っているフレームの期限。最初の呼び出しで基点を `now` に置く。期限が `now` より 2 周期より先なら（QPC は戻らないので作りの誤りか時計の異常）、基点を `now` に置き直す（待ち続けて止まらないための守り）。
    pub fn deadline(&mut self, now: u64) -> u64 {
        let grid = match self.grid {
            Some(g) => g,
            None => {
                let g = Grid {
                    base: now,
                    qpf: self.qpf,
                    fps: self.fps,
                };
                self.grid = Some(g);
                self.next = 0;
                g
            }
        };
        let d = grid.at(self.next);
        if d > now.saturating_add(2 * self.period_ceil()) {
            self.grid = Some(Grid {
                base: now,
                qpf: self.qpf,
                fps: self.fps,
            });
            self.next = 0;
            self.rebased += 1;
            return now;
        }
        d
    }

    /// フレームの始まりで呼ぶ。`spin` は最後に QPC を読み続ける幅（カウント）、`chunk_us` は 1 回に寝る長さの上限（µs）。寝る長さは「残り − spin」を µs に切り捨て、`chunk_us` で抑え、1 µs 以上にする。
    pub fn decide(&mut self, now: u64, spin: u64, chunk_us: u32) -> Head {
        let deadline = self.deadline(now);
        if now >= deadline {
            return Head::Run {
                deadline,
                late: now > deadline,
            };
        }
        let rest = deadline - now;
        if rest <= spin {
            return Head::Spin { deadline };
        }
        let us = ticks_to_us_floor(rest - spin, self.qpf)
            .min(chunk_us as u64)
            .max(1) as u32;
        Head::Sleep { deadline, us }
    }

    /// フレームを `start` に始めた。次の期限を「`start` より後の最初の格子点」にする（取り戻さない）。飛ばした格子点の数を返す。
    pub fn started(&mut self, start: u64) -> u64 {
        let Some(grid) = self.grid else {
            return 0;
        };
        let j = grid.first_after(start);
        let skipped = j.saturating_sub(self.next.saturating_add(1));
        self.next = j.max(self.next.saturating_add(1));
        skipped
    }
}

pub fn ticks_to_us_floor(ticks: u64, qpf: u64) -> u64 {
    if qpf == 0 {
        return 0;
    }
    (ticks as u128 * 1_000_000 / qpf as u128).min(u64::MAX as u128) as u64
}

pub fn us_to_ticks_ceil(us: u64, qpf: u64) -> u64 {
    (us as u128 * qpf as u128)
        .div_ceil(1_000_000)
        .min(u64::MAX as u128) as u64
}

pub const SECTION: &str = "pacing";
/// 節の中で読むキー（これと `pacing_hybrid::HYBRID_KEYS` の `hybrid_*` 以外は断る。綴りの誤りで既定のまま走らないように）。
const KEYS: [&str; 5] = ["mode", "fps", "cycle_secs", "cycle_phases", "auto_window"];

/// `mode = cycle` の 1 区間の長さ（秒）の既定。6 分のマッチなら 1 方式あたり 4 区間になる。
pub const DEFAULT_CYCLE_SECS: u64 = 30;
/// `cycle_secs` の下限・上限。下限 1 秒（1 区間が数十フレームより短いと、入れ替えの直後のフレームばかりになる）。上限は dll の計測の上限（`observe::SCRIPT_MAX_SECONDS` = 1200 秒）の 3 倍。
pub const MIN_CYCLE_SECS: u64 = 1;
pub const MAX_CYCLE_SECS: u64 = 3_600;

/// `[pacing]` の節が無いとき（ini が無いときも）・`mode` を書かないときの形。ウィンドウなら b-spin、全画面なら game の `Mode::Auto`。
/// 切り分けの実測（同じ実行で 30 秒ごとに入れ替え）で、入力 → Present の前が game p50 271.4 / p99 541.9 µs（10,813 フレーム）、b p50 308.5 / p99 1,052.9 µs（10,238）、
/// b-spin p50 272.8 / p99 539.3 µs（9,004）だった。b の遅れの原因は寝ることで、b-spin は game と同じ。b-spin は論理コア 1 つを回し続ける（素の花映塚と同じ）。
/// ただし全画面の素の花映塚は Present が垂直同期を待つので、全画面で 60.000 fps の格子を刻むと走査線と干渉する見込みがあり、全画面は game に任せる。
pub const DEFAULT_MODE: Mode = Mode::Auto;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Game,
    B,
    /// B と同じ格子だが、期限の前に寝ずに `Sleep(0)` で回る（ゲーム自身の待ち方 `0x0042DD7B` と同じ形）。
    BSpin,
    /// game → b → b-spin を `cycle_secs` ごとに入れ替える（切り分け用）。並びは `cycle_phases` で変えられる。
    Cycle,
    /// B と同じ格子で、期限の「回し読みの幅 + 余裕」前までは高分解能の待機タイマーで寝て、その後は b-spin と同じに `Sleep(0)` で回る（CPU を下げるため。`crate::pacing_hybrid`）。既定ではない。
    BHybrid,
    /// vpatch の通常のペースの形の再現（`crate::pacing_vpatch`）。遅延の表の「vpatch 相当」を測るための測定専用で、既定でも `auto_window` / `cycle_phases` の候補でもない。
    Vpatch,
    /// フレームごとに画面の形（`Screen`）を読み、ウィンドウなら b-spin、全画面・読めないなら game の経路（始まりの入口がゲーム自身の時計へ任せる）。既定（`DEFAULT_MODE`）。
    /// 設置の時でなくフレームごとに読むのは、設置が花映塚の設定の読み込み（WinMain の `0x0042E4D0`）より前に走りうり、読んだ値が起動前の 0 = 全画面に見えるため。
    Auto,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Game => "game",
            Mode::B => "b",
            Mode::BSpin => "b-spin",
            Mode::Cycle => "cycle",
            Mode::Auto => "auto",
            Mode::BHybrid => "b-hybrid",
            Mode::Vpatch => "vpatch",
        }
    }

    pub fn from_name(s: &str) -> Option<Mode> {
        [
            Mode::Game,
            Mode::B,
            Mode::BSpin,
            Mode::Cycle,
            Mode::Auto,
            Mode::BHybrid,
            Mode::Vpatch,
        ]
        .into_iter()
        .find(|m| m.name().eq_ignore_ascii_case(s))
    }

    /// 始まりの call（`0x0042DC44`）を書くか。game だけが書かない（auto は全画面のフレームでも書いたまま、始まりの入口がゲーム自身の経路へ任せる）。
    pub fn uses_head(self) -> bool {
        self != Mode::Game
    }

    pub fn fixed_phase(self) -> Option<Phase> {
        match self {
            Mode::Game => Some(Phase::Game),
            Mode::B => Some(Phase::B),
            Mode::BSpin => Some(Phase::BSpin),
            Mode::BHybrid => Some(Phase::BHybrid),
            Mode::Vpatch => Some(Phase::Vpatch),
            Mode::Cycle | Mode::Auto => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Windowed,
    Fullscreen,
    Unknown,
}

/// 設定のブロック `0x004B3488` の `+0xB1`（= `0x004B3539`）の 1 バイトを読み分ける（原本の逆アセンブルで確認）:
/// - デバイスを作る関数 `0x0042DD90` の `0x0042DDC2` `cmp %bl,0x4b3539`（bl = 0）→ 0 でなければ `0x0042DE94` へ飛び、`0x0042DEA1` で Windowed = 1・SwapEffect 3（COPY）。
///   0 なら全画面の経路（Windowed は `rep stos` の 0 のまま。`[0x4B30CC]` / `[0x4B36AC]` が立たなければリフレッシュ 60・PresentationInterval 1）。
/// - 既定値は 0（`0x0041A8EE` `mov %al,0xb1(%edx)`、al = 0）= 全画面。読み込みの照合 `0x0042F84B` が 2 以上を断る。
/// - オプションの画面は 0 と 1 を行き来する（`0x00427A0D`〜`0x00427A1F` / `0x00427C5D`〜`0x00427C6E`）。
///
/// 2 以上は花映塚の作りでは起きない値なので、ゲームの `jne`（0 以外はウィンドウ）に合わせず「読めない」として扱う（全画面と同じ game の経路に倒す）。
pub fn screen_from_flag(b: u8) -> Screen {
    match b {
        0 => Screen::Fullscreen,
        1 => Screen::Windowed,
        _ => Screen::Unknown,
    }
}

/// `mode = auto` のフレームの方式。ウィンドウは b-spin、全画面と読めないときは game（ゲーム自身の待ちと垂直同期のまま）。
pub fn auto_phase(s: Screen) -> Phase {
    auto_phase_with(s, DEFAULT_AUTO_WINDOW)
}

pub fn auto_phase_with(s: Screen, window: Phase) -> Phase {
    match s {
        Screen::Windowed => window,
        Screen::Fullscreen | Screen::Unknown => Phase::Game,
    }
}

/// `mode = auto` のウィンドウの方式の既定（ini の `auto_window` を書かないとき）。b-hybrid は CPU が軽いが入力 → Present の前の p50 が長いので既定にしない。
pub const DEFAULT_AUTO_WINDOW: Phase = Phase::BSpin;
/// `auto_window` に書ける方式（ウィンドウで格子を刻む方式のうち、寝ずに回る 2 つ。b は寝ることで遅れた方式なので入れない）。
pub const AUTO_WINDOW_PHASES: [Phase; 2] = [Phase::BSpin, Phase::BHybrid];

/// ゲーム自身の時計の経路（全画面・cycle の game の区間）で、始まりのこの 1 回がフレームを始める見込みか。始まりの中の待ち（delay 方式の「相手の語を始まりで待つ」）を、
/// 始める 1 回の直前でだけ呼ぶために使う。整数だけで比べる（ゲームのスレッドの始まりから呼ぶので x87 に触らない）。
///
/// ゲームの判断（原本の逆アセンブルで確認）: 時計 `0x0042CC90` は QPC の下位 32 ビット `qpc_low` を `[0x4B30C4]`（`divisor`。QPF の下位 32 ビット）で割り、
/// 制御語 `0x007F`（単精度）の下で丸める。do_frame の `0x0042DC59` `fcoml 0x3c(%esi)` → `0x0042DC64` `jne 0x42dd77` で、今が期限 `+0x3C`（double。`deadline_bits` はそのビット列）
/// 以下なら `Sleep(0)` して戻り、越えていれば始める。今が前の今 `+0x34` より小さい（時計の折り返し）ときは `0x0042DC56` で期限を今に置き直すので始めない。
///
/// ここでは丸めの前の値 `qpc_low / divisor` が「期限 − `early_ticks`」を越えたら真にする。`early_ticks` を単精度の丸め（256〜512 秒で半カウント約 15 µs）と、
/// こちらが QPC を読んでからゲームが読むまでの間より大きく取れば、「ゲームが始める 1 回」は必ず真に入る（真が早すぎる側にだけ外れる。早すぎた 1 回で待っても同じ tick で 2 度は待たない ——
/// `NetPlay::head_wait`）。折り返しの直後は期限が大きいまま今が小さいので偽になり、ゲームが期限を今に置き直した次の回で真になる。
/// `divisor` が 0（timeGetTime の経路）・期限が負・数でない（NaN・無限）なら偽（待たない）。
/// `GAME_DUE_EARLY_US` は `game_frame_due` の早め（µs）。丸め（最大で半カウント約 15 µs ＋ 2^31 を越えた QPC の丸め約 13 µs）と、こちらの QPC の読みからゲームの読みまでの間を足しても余る幅。
/// 早めた分は「始まりで待ち始めるのが期限の最大この幅だけ前」になるだけで、入力の読みはゲームの期限より前にならない。
pub const GAME_DUE_EARLY_US: u32 = 200;

pub fn game_due_early_ticks(divisor: u32) -> u32 {
    (u64::from(divisor) * u64::from(GAME_DUE_EARLY_US) / 1_000_000) as u32
}

pub fn game_frame_due(qpc_low: u32, divisor: u32, deadline_bits: u64, early_ticks: u32) -> bool {
    if divisor == 0 {
        return false;
    }
    let exp = ((deadline_bits >> 52) & 0x7FF) as i32;
    let frac = deadline_bits & ((1u64 << 52) - 1);
    if deadline_bits >> 63 != 0 {
        return false;
    }
    if exp == 0x7FF {
        return false;
    }
    let (mant, e) = if exp == 0 {
        (frac, 1 - 1075)
    } else {
        (frac | (1u64 << 52), exp - 1075)
    };
    let lhs = u128::from(qpc_low) + u128::from(early_ticks);
    let rhs = u128::from(mant) * u128::from(divisor);
    if e >= 0 {
        return e < 40 && lhs > rhs << e;
    }
    let s = -e;
    if s >= 94 {
        return lhs > 0;
    }
    (lhs << s) > rhs
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    Game,
    B,
    BSpin,
    BHybrid,
    /// `Mode::Vpatch` のフレーム（`cycle_phases` には書けない）。
    Vpatch,
}

pub const CYCLE_ORDER: [Phase; 3] = [Phase::Game, Phase::B, Phase::BSpin];
pub const ALL_PHASES: [Phase; 5] = [
    Phase::Game,
    Phase::B,
    Phase::BSpin,
    Phase::BHybrid,
    Phase::Vpatch,
];

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Game => "game",
            Phase::B => "b",
            Phase::BSpin => "b-spin",
            Phase::BHybrid => "b-hybrid",
            Phase::Vpatch => "vpatch",
        }
    }

    pub fn from_name(s: &str) -> Option<Phase> {
        ALL_PHASES.into_iter().find(|p| p.name() == s)
    }

    pub fn code(self) -> u32 {
        match self {
            Phase::Game => 1,
            Phase::B => 2,
            Phase::BSpin => 3,
            Phase::BHybrid => 4,
            Phase::Vpatch => 5,
        }
    }

    pub fn from_code(c: u32) -> Option<Phase> {
        ALL_PHASES.into_iter().find(|p| p.code() == c)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CyclePhases {
    list: [Phase; 4],
    len: usize,
}

impl CyclePhases {
    pub const DEFAULT: CyclePhases = CyclePhases {
        list: [Phase::Game, Phase::B, Phase::BSpin, Phase::BSpin],
        len: 3,
    };

    pub fn new(phases: &[Phase]) -> Option<CyclePhases> {
        if !(2..=4).contains(&phases.len()) {
            return None;
        }
        for (i, p) in phases.iter().enumerate() {
            if phases[..i].contains(p) {
                return None;
            }
        }
        let mut list = [phases[0]; 4];
        list[..phases.len()].copy_from_slice(phases);
        Some(CyclePhases {
            list,
            len: phases.len(),
        })
    }

    pub fn as_slice(&self) -> &[Phase] {
        &self.list[..self.len]
    }

    pub fn text(&self) -> String {
        self.as_slice()
            .iter()
            .map(|p| p.name())
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

/// `cycle_phases` の値を読む（`game, b, b-spin` のように、方式の名前をカンマで区切る。大文字・小文字は区別する。CSV の札と同じ文字）。
pub fn parse_cycle_phases(v: &str) -> Result<CyclePhases, String> {
    let names: Vec<&str> = v.split(',').map(str::trim).collect();
    let mut phases = Vec::with_capacity(names.len());
    for n in &names {
        // vpatch は測定専用の mode = vpatch だけで走らせる（区間の入れ替えに載せると、Present の待ちの状態が区間をまたぐ）。
        let p = Phase::from_name(n).filter(|p| *p != Phase::Vpatch);
        phases.push(p.ok_or(format!(
            "cycle_phases の {n:?} は game / b / b-spin / b-hybrid のどれでもない"
        ))?);
    }
    CyclePhases::new(&phases).ok_or(format!(
        "cycle_phases の値 {v:?} は 2〜4 個の重ならない方式（例 b-spin, b-hybrid）"
    ))
}

/// `mode = cycle` の区間の時計（ゲームのスレッドの始まりだけが触る。確保しない）。区間を決めるのはフレームの始まりだけなので、1 フレームの途中で方式が変わることはない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cycle {
    len: u64,
    phases: CyclePhases,
    idx: usize,
    since: Option<u64>,
    switches: u64,
}

impl Cycle {
    pub fn new(qpf: u64, secs: u64) -> Option<Cycle> {
        Cycle::with_phases(qpf, secs, CyclePhases::DEFAULT)
    }

    pub fn with_phases(qpf: u64, secs: u64, phases: CyclePhases) -> Option<Cycle> {
        let len = qpf.checked_mul(secs).filter(|&l| l > 0)?;
        Some(Cycle {
            len,
            phases,
            idx: 0,
            since: None,
            switches: 0,
        })
    }

    pub fn phase(&self) -> Phase {
        self.phases.as_slice()[self.idx]
    }

    pub fn switches(&self) -> u64 {
        self.switches
    }

    /// フレームの最初の始まりで呼ぶ。いまの区間が `len` カウント以上続いていれば次の方式へ進め、区間の始まりを `now` に置く（遅れても区間を取り戻さない）。返すのは（このフレームの方式、入れ替えたか）。
    pub fn at_frame(&mut self, now: u64) -> (Phase, bool) {
        let Some(since) = self.since else {
            self.since = Some(now);
            return (self.phase(), false);
        };
        if now.saturating_sub(since) >= self.len {
            self.idx = (self.idx + 1) % self.phases.as_slice().len();
            self.since = Some(now);
            self.switches += 1;
            return (self.phase(), true);
        }
        (self.phase(), false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacingIni {
    pub mode: Mode,
    /// ペース（`fps`。書かなければ `DEFAULT_FPS`）。game と cycle では必ず `DEFAULT_FPS`（`mode_from_ini` が断る）。
    pub fps: u64,
    pub cycle_secs: u64,
    pub cycle_phases: CyclePhases,
    pub auto_window: Phase,
    pub hybrid: HybridParams,
}

impl PacingIni {
    /// 節が無い（ini が無い）ときの形。`mode_from_ini` と dll の「ini が無い」の経路が同じ値を使う。
    pub const DEFAULT: PacingIni = PacingIni {
        mode: DEFAULT_MODE,
        fps: DEFAULT_FPS,
        cycle_secs: DEFAULT_CYCLE_SECS,
        cycle_phases: CyclePhases::DEFAULT,
        auto_window: DEFAULT_AUTO_WINDOW,
        hybrid: HybridParams::DEFAULT,
    };

    pub fn uses_hybrid(&self) -> bool {
        match self.mode {
            Mode::BHybrid => true,
            Mode::Auto => self.auto_window == Phase::BHybrid,
            Mode::Cycle => self.cycle_phases.as_slice().contains(&Phase::BHybrid),
            Mode::Game | Mode::B | Mode::BSpin | Mode::Vpatch => false,
        }
    }
}

/// `fps` の値を読む（ini と `pacing_sum --fps` が同じ読み方をする）。10 進の整数で `MIN_FPS`〜`MAX_FPS`。
pub fn parse_fps(v: &str) -> Result<u64, String> {
    let t = v.trim();
    // `u64::from_str` は先頭の `+` を通すので、数字だけかを先に見る。
    let n = (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        .then(|| t.parse::<u64>().ok())
        .flatten();
    match n {
        Some(n) if (MIN_FPS..=MAX_FPS).contains(&n) => Ok(n),
        _ => Err(format!(
            "fps の値 {v:?} は {MIN_FPS}〜{MAX_FPS} の整数（既定 {DEFAULT_FPS}）"
        )),
    }
}

pub fn parse_cycle_secs(v: &str) -> Result<u64, String> {
    let t = v.trim();
    let n = (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        .then(|| t.parse::<u64>().ok())
        .flatten();
    match n {
        Some(n) if (MIN_CYCLE_SECS..=MAX_CYCLE_SECS).contains(&n) => Ok(n),
        _ => Err(format!(
            "cycle_secs の値 {v:?} は {MIN_CYCLE_SECS}〜{MAX_CYCLE_SECS} の整数（既定 {DEFAULT_CYCLE_SECS}）"
        )),
    }
}

/// `[pacing]` の節を読む（形とペース）。節が無ければ `PacingIni::DEFAULT`（auto・60 fps）、`mode` を書かなければ `DEFAULT_MODE`。
/// `fps` は b・b-spin・b-hybrid・vpatch でだけ効く。game・cycle・auto（`mode` を書かないときも）で 60 以外を書いたら誤りとして断る
/// （ゲーム自身のペースはこの道具が変えないので、書いたのに効かない実行・区間ごとに速さの違う実行を作らない）。
/// `cycle_secs` と `cycle_phases` は cycle でだけ、`auto_window` は auto でだけ、`hybrid_*` は b-hybrid のフレームがありうる形（`PacingIni::uses_hybrid`）でだけ書ける。
pub fn mode_from_ini(ini: &Ini) -> Result<PacingIni, String> {
    if !ini.has_section(SECTION) {
        return Ok(PacingIni::DEFAULT);
    }
    for (k, line) in ini.keys(SECTION) {
        if !KEYS.contains(&k) && !HYBRID_KEYS.iter().any(|h| h.name == k) {
            let all: Vec<&str> = KEYS
                .iter()
                .copied()
                .chain(HYBRID_KEYS.iter().map(|h| h.name))
                .collect();
            return Err(format!(
                "[{SECTION}] の {line} 行目のキー {k:?} を知らない（読めるのは {}）",
                all.join(" / ")
            ));
        }
    }
    let mode = match ini.get(SECTION, "mode") {
        None => DEFAULT_MODE,
        Some(v) => Mode::from_name(v).ok_or(format!(
            "[{SECTION}] の mode の値 {v:?} は game / b / b-spin / b-hybrid / vpatch / cycle / auto のどれか"
        ))?,
    };
    let fps = match ini.get(SECTION, "fps") {
        None => DEFAULT_FPS,
        Some(v) => parse_fps(v).map_err(|e| format!("[{SECTION}] の {e}"))?,
    };
    if matches!(mode, Mode::Game | Mode::Cycle | Mode::Auto) && fps != DEFAULT_FPS {
        return Err(format!(
            "[{SECTION}] の fps = {fps} は mode = b / b-spin / b-hybrid / vpatch のときだけ効く（mode = {} はゲーム自身の {DEFAULT_FPS} fps の区間を含む）",
            mode.name()
        ));
    }
    let only_for = |key: &str, ok: bool, want: &str| -> Result<(), String> {
        if ini.get(SECTION, key).is_some() && !ok {
            return Err(format!(
                "[{SECTION}] の {key} は {want} のときだけ書ける（mode = {}）",
                mode.name()
            ));
        }
        Ok(())
    };
    only_for("cycle_secs", mode == Mode::Cycle, "mode = cycle")?;
    only_for("cycle_phases", mode == Mode::Cycle, "mode = cycle")?;
    only_for("auto_window", mode == Mode::Auto, "mode = auto")?;
    let cycle_secs = match ini.get(SECTION, "cycle_secs") {
        None => DEFAULT_CYCLE_SECS,
        Some(v) => parse_cycle_secs(v).map_err(|e| format!("[{SECTION}] の {e}"))?,
    };
    let cycle_phases = match ini.get(SECTION, "cycle_phases") {
        None => CyclePhases::DEFAULT,
        Some(v) => parse_cycle_phases(v).map_err(|e| format!("[{SECTION}] の {e}"))?,
    };
    let auto_window = match ini.get(SECTION, "auto_window") {
        None => DEFAULT_AUTO_WINDOW,
        Some(v) => AUTO_WINDOW_PHASES
            .into_iter()
            .find(|p| p.name() == v.trim())
            .ok_or(format!(
                "[{SECTION}] の auto_window の値 {v:?} は b-spin / b-hybrid のどちらか"
            ))?,
    };
    let mut cfg = PacingIni {
        mode,
        fps,
        cycle_secs,
        cycle_phases,
        auto_window,
        hybrid: HybridParams::DEFAULT,
    };
    for k in &HYBRID_KEYS {
        only_for(
            k.name,
            cfg.uses_hybrid(),
            "b-hybrid のフレームがある形（mode = b-hybrid、mode = auto で auto_window = b-hybrid、mode = cycle で cycle_phases に b-hybrid）",
        )?;
        if let Some(v) = ini.get(SECTION, k.name) {
            cfg.hybrid
                .set(k.name, v)
                .map_err(|e| format!("[{SECTION}] の {e}"))?;
        }
    }
    cfg.hybrid
        .check()
        .map_err(|e| format!("[{SECTION}] の {e}"))?;
    Ok(cfg)
}

/// 論理 CPU の平らな番号（`group × 64 + 群の中の番号`）。GetCurrentProcessorNumberEx の PROCESSOR_NUMBER と GetSystemCpuSetInformation の `Group` / `LogicalProcessorIndex` を同じ式で平らにして突き合わせる
/// （1 群は最大 64 個。Windows のプロセッサ群の決まり）。
pub fn flat_cpu(group: u16, number: u8) -> u32 {
    group as u32 * 64 + number as u32
}

/// 論理 CPU ごとの効率クラス（`SYSTEM_CPU_SET_INFORMATION.EfficiencyClass`）の表。値が大きいほど速く、電力の効率は悪い。性能コアと省電力コアの混成（例 i5-12600K）なら 2 種類、そうでなければ 1 種類になる見込み。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuClasses {
    cpus: Vec<(u32, u8)>,
}

impl CpuClasses {
    pub fn from_pairs(mut v: Vec<(u32, u8)>) -> CpuClasses {
        v.sort_unstable();
        v.dedup_by_key(|p| p.0);
        CpuClasses { cpus: v }
    }

    pub fn len(&self) -> usize {
        self.cpus.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cpus.is_empty()
    }

    pub fn class_of(&self, cpu: u32) -> Option<u8> {
        self.cpus
            .binary_search_by_key(&cpu, |p| p.0)
            .ok()
            .map(|i| self.cpus[i].1)
    }

    pub fn distinct(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self.cpus.iter().map(|p| p.1).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn describe(&self) -> String {
        let parts: Vec<String> = self
            .distinct()
            .into_iter()
            .map(|c| {
                let n = self.cpus.iter().filter(|p| p.1 == c).count();
                format!("class {c} が {n} 個")
            })
            .collect();
        format!("論理 CPU {} 個: {}", self.cpus.len(), parts.join(" / "))
    }
}

/// `SYSTEM_CPU_SET_INFORMATION` の中の位置（winnt.h の並び。dll の `pacing_win` の検査が windows-sys の `offset_of!` と突き合わせる）。1 件の大きさは先頭の `Size` に書いてあり、それで次の件へ進む。
pub const CPU_SET_OFF_SIZE: usize = 0;
pub const CPU_SET_OFF_TYPE: usize = 4;
pub const CPU_SET_OFF_GROUP: usize = 12;
pub const CPU_SET_OFF_LOGICAL_INDEX: usize = 14;
pub const CPU_SET_OFF_EFFICIENCY: usize = 18;
pub const CPU_SET_TYPE_CPU_SET: u32 = 0;

/// GetSystemCpuSetInformation が埋めたバイト列を読む（Win32 は呼ばない）。大きさが 8 未満・残りをはみ出す件があれば誤り。
pub fn parse_cpu_sets(bytes: &[u8]) -> Result<CpuClasses, String> {
    let u32_at = |o: usize| -> u32 {
        u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };
    let mut v = Vec::new();
    let mut off = 0usize;
    while off < bytes.len() {
        if off + 8 > bytes.len() {
            return Err(format!("{off} バイト目の件の先頭が切れている"));
        }
        let size = u32_at(off + CPU_SET_OFF_SIZE) as usize;
        if size < 8 || off + size > bytes.len() {
            return Err(format!(
                "{off} バイト目の件の大きさ {size} が合わない（残り {}）",
                bytes.len() - off
            ));
        }
        if u32_at(off + CPU_SET_OFF_TYPE) == CPU_SET_TYPE_CPU_SET && size > CPU_SET_OFF_EFFICIENCY {
            let g = u16::from_le_bytes([
                bytes[off + CPU_SET_OFF_GROUP],
                bytes[off + CPU_SET_OFF_GROUP + 1],
            ]);
            let n = bytes[off + CPU_SET_OFF_LOGICAL_INDEX];
            v.push((flat_cpu(g, n), bytes[off + CPU_SET_OFF_EFFICIENCY]));
        }
        off += size;
    }
    Ok(CpuClasses::from_pairs(v))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoreKind {
    Perf,
    Eff,
    Mixed,
    Uniform,
    Unknown,
}

pub const CORE_KINDS: [CoreKind; 5] = [
    CoreKind::Perf,
    CoreKind::Eff,
    CoreKind::Mixed,
    CoreKind::Uniform,
    CoreKind::Unknown,
];

impl CoreKind {
    pub fn name(self) -> &'static str {
        match self {
            CoreKind::Perf => "性能コア",
            CoreKind::Eff => "省電力コア",
            CoreKind::Mixed => "途中で入れ替わり",
            CoreKind::Uniform => "区別なし",
            CoreKind::Unknown => "不明",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

pub fn core_kind(a: Option<u8>, b: Option<u8>, classes: &[u8]) -> CoreKind {
    let (Some(&lo), Some(&hi)) = (classes.iter().min(), classes.iter().max()) else {
        return CoreKind::Unknown;
    };
    let (Some(a), Some(b)) = (a, b) else {
        return CoreKind::Unknown;
    };
    if lo == hi {
        return CoreKind::Uniform;
    }
    let kind = |c: u8| {
        if c >= hi {
            CoreKind::Perf
        } else {
            CoreKind::Eff
        }
    };
    if kind(a) == kind(b) {
        kind(a)
    } else {
        CoreKind::Mixed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameRow {
    pub frame: u32,
    pub paced: bool,
    pub deadline: u64,
    pub first_head: u64,
    pub head_calls: u32,
    pub sleeps: u32,
    pub start: u64,
    pub late: bool,
    pub rebased: bool,
    pub skipped: u32,
    pub inputs: u32,
    pub sup_frame: u32,
    pub input: u64,
    pub present_begin: u64,
    pub present_end: u64,
    pub phase: Option<Phase>,
    pub head_cpu: Option<u32>,
    pub input_cpu: Option<u32>,
    pub present_cpu: Option<u32>,
    pub head_eff: Option<u8>,
    pub input_eff: Option<u8>,
    pub present_eff: Option<u8>,
    /// 最初に入力の地点を通ったとき・Present の前の QueryThreadCycleTime（ゲームのスレッドが CPU で走った周期の累計。0 は「無い」）。入力では QPC の後、Present の前では QPC の前に取るので、周期の期間は QPC の期間の内側に入る。
    pub input_cycles: u64,
    pub present_cycles: u64,
    /// この枠の始まりが読んだ格子の寄せの要求（ppm。正 = 伸ばす。要求が無かった・0 なら 0）。ゲーム自身の区間で捨てた要求も値は残す（捨てたことは `glide_moved` が 0 であることで分かる）。
    pub glide_req_ppm: i32,
    /// 実際に格子の基点を動かした量（QPC のカウント。符号つき。動かさなかった・格子が無かった・ゲーム自身の区間は 0）。CSV では µs（小数 1 桁）で書く。
    pub glide_moved: i64,
    /// この枠で b-hybrid の始まりが「寝るか譲るか」を決めたときの余裕の最大（µs。`pacing_hybrid::Doze::margin_us`）。判断が無かった枠は 0。
    pub hybrid_margin_us: u32,
    /// この枠で b-hybrid の始まりが待機タイマーで寝た眠りの寝過ごしの最大（µs。実際に寝た長さ − 頼んだ長さ。早く起きたら 0）。寝なかった枠・測っていない古い CSV は `None`（CSV では空）。
    pub oversleep_us: Option<u32>,
}

/// CSV の先頭の行。前の 15 列が記録そのもので、次の 4 列は QPF から出した µs（読み戻すときは使わない）。その後ろの 9 列（`phase`〜`present_cycles`）が足した記録、次の 2 列がそこから出した値。
/// 最後の 2 列（`glide_req_ppm` `glide_moved_us`）が格子の寄せの記録（µs の方は `glide_moved` から出す）、その後ろの 3 列（`hybrid_margin_us` `oversleep_us` `frame_cycles`）が b-hybrid の記録
/// （`frame_cycles` は 1 つ前の行の `present_cycles` との差から出す。読み戻すときは使わない）。前の 19 列の並びと意味は変えていない（足す前の CSV も `parse_csv` が読む）。
pub const CSV_HEADER: &str = "frame,paced,deadline_qpc,first_head_qpc,head_calls,sleeps,start_qpc,late,rebased,skipped,inputs,sup_frame,input_qpc,present_begin_qpc,present_end_qpc,wake_late_us,input_to_present_us,present_us,period_us,phase,head_cpu,head_eff,input_cpu,input_eff,present_cpu,present_eff,input_cycles,present_cycles,input_to_present_cycles,cycles_per_us,glide_req_ppm,glide_moved_us,hybrid_margin_us,oversleep_us,frame_cycles";
const CSV_RAW_COLUMNS: usize = 15;
/// 足す前の CSV の列の数（記録 15 ＋ µs 4）。
const CSV_OLD_COLUMNS: usize = 19;
const CSV_NEW_RAW_COLUMNS: usize = 9;
const CSV_NEW_COLUMNS: usize = CSV_OLD_COLUMNS + CSV_NEW_RAW_COLUMNS;
const CSV_NEW_DERIVED_COLUMNS: usize = 2;
/// 格子の寄せの 2 列が始まる位置（0 始まり）と、それを含めた列の数。古い CSV（28 / 30 列）には無い。
const CSV_GLIDE_AT: usize = CSV_NEW_COLUMNS + CSV_NEW_DERIVED_COLUMNS;
const CSV_GLIDE_COLUMNS: usize = CSV_GLIDE_AT + 2;
/// b-hybrid の 3 列（記録 2 ＋ 出した値 1）が始まる位置と、それを含めた列の数。古い CSV（32 列まで）には無い。
const CSV_HYBRID_AT: usize = CSV_GLIDE_COLUMNS;
const CSV_HYBRID_COLUMNS: usize = CSV_HYBRID_AT + 3;

/// 寄せた枠として数える最小の寄せ幅（ppm。0.1% = 60 fps で約 17 µs）。それより小さい寄せは毎フレーム出るので、数に入れると枠の数がほぼ全部になって意味が無い。本ログの寄せの行（dll）と `pacing_sum` が同じ値で数える。
pub const GLIDE_COUNT_PPM: u32 = 1_000;

pub fn us_text(ticks: i64, qpf: u64) -> String {
    if qpf == 0 {
        return String::new();
    }
    let tenths = (ticks as i128 * 10_000_000 / qpf as i128) as i64;
    let sign = if tenths < 0 { "-" } else { "" };
    let a = tenths.unsigned_abs();
    format!("{sign}{}.{}", a / 10, a % 10)
}

/// `us_text` の逆（符号つき・小数 1 桁の µs の文字を、最も近いカウントへ。絶対値で丸め、ちょうど 0.5 カウントは大きい側）。`qpf` が 0・文字が読めなければ `None`。
fn us_text_to_ticks(text: &str, qpf: u64) -> Option<i64> {
    if qpf == 0 {
        return None;
    }
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text),
    };
    let (int, frac) = body.split_once('.')?;
    if int.is_empty()
        || frac.len() != 1
        || !int.bytes().all(|c| c.is_ascii_digit())
        || !frac.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let tenths = int.parse::<i128>().ok()? * 10 + i128::from(frac.parse::<u8>().ok()?);
    let ticks = (tenths * qpf as i128 + 5_000_000) / 10_000_000;
    let v = i64::try_from(ticks).ok()?;
    Some(if neg { -v } else { v })
}

fn diff(a: u64, b: u64) -> i64 {
    (a as i128 - b as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

impl FrameRow {
    pub fn wake_late(&self) -> Option<i64> {
        (self.paced && self.deadline != 0 && self.start != 0)
            .then(|| diff(self.start, self.deadline))
    }

    pub fn input_to_present(&self) -> Option<i64> {
        (self.inputs > 0 && self.input != 0 && self.present_begin != 0)
            .then(|| diff(self.present_begin, self.input))
    }

    pub fn input_to_present_end(&self) -> Option<i64> {
        (self.inputs > 0 && self.input != 0 && self.present_end != 0)
            .then(|| diff(self.present_end, self.input))
    }

    pub fn present_len(&self) -> Option<i64> {
        (self.present_begin != 0 && self.present_end != 0)
            .then(|| diff(self.present_end, self.present_begin))
    }

    pub fn input_to_present_cycles(&self) -> Option<i64> {
        (self.inputs > 0 && self.input_cycles != 0 && self.present_cycles != 0)
            .then(|| diff(self.present_cycles, self.input_cycles))
    }

    /// 入力 → Present の前の「周期 ÷ µs」（整数に切り捨て）。QueryThreadCycleTime はスレッドが CPU に載っている間だけ数えるので、その間ずっと走っていれば周期を数える速さ（TSC の MHz のはず。推定）に近く、寝ていた・追い出されていた時間があればその割合だけ小さくなる。コアの周波数が下がっても、TSC で数えているなら値は下がらない（推定）。
    pub fn cycles_per_us(&self, qpf: u64) -> Option<i64> {
        let c = self.input_to_present_cycles()?;
        let t = self.input_to_present()?;
        if t <= 0 || qpf == 0 {
            return None;
        }
        let v = c as i128 * qpf as i128 / (t as i128 * 1_000_000);
        Some(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }

    pub fn csv_line(&self, prev: Option<&FrameRow>, qpf: u64) -> String {
        let opt = |v: Option<i64>| v.map(|t| us_text(t, qpf)).unwrap_or_default();
        let num = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_default();
        let nz = |v: u64| (v != 0).then_some(v);
        let period = prev
            .filter(|p| p.frame.wrapping_add(1) == self.frame && p.present_begin != 0)
            .filter(|_| self.present_begin != 0)
            .map(|p| diff(self.present_begin, p.present_begin));
        let old = format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.frame,
            self.paced as u8,
            self.deadline,
            self.first_head,
            self.head_calls,
            self.sleeps,
            self.start,
            self.late as u8,
            self.rebased as u8,
            self.skipped,
            self.inputs,
            self.sup_frame,
            self.input,
            self.present_begin,
            self.present_end,
            opt(self.wake_late()),
            opt(self.input_to_present()),
            opt(self.present_len()),
            opt(period),
        );
        let new = [
            self.phase.map(|p| p.name().to_string()).unwrap_or_default(),
            num(self.head_cpu.map(u64::from)),
            num(self.head_eff.map(u64::from)),
            num(self.input_cpu.map(u64::from)),
            num(self.input_eff.map(u64::from)),
            num(self.present_cpu.map(u64::from)),
            num(self.present_eff.map(u64::from)),
            num(nz(self.input_cycles)),
            num(nz(self.present_cycles)),
            self.input_to_present_cycles()
                .map(|c| c.to_string())
                .unwrap_or_default(),
            self.cycles_per_us(qpf)
                .map(|c| c.to_string())
                .unwrap_or_default(),
        ];
        // 要求が無い・動かさなかった枠も 0 / 0.0 と書く（空にすると足す前の CSV と見分けが付かない）。b-hybrid の余裕は判断の無かった枠も 0 と書き、寝過ごしは寝なかった枠を空にする（0 は「寝て、寝過ごさなかった」）。
        format!(
            "{old},{},{},{},{},{},{}",
            new.join(","),
            self.glide_req_ppm,
            us_text(self.glide_moved, qpf),
            self.hybrid_margin_us,
            num(self.oversleep_us.map(u64::from)),
            self.frame_cycles(prev)
                .map(|c| c.to_string())
                .unwrap_or_default()
        )
    }

    /// 1 フレームにゲームのスレッドが CPU で走った周期（この行と 1 つ前の行の、Present の前の QueryThreadCycleTime の差）。番号が続いていて、どちらにも周期があるときだけ。
    /// 寝ていた時間は数えないので、b-spin（回り続ける）と b-hybrid（寝る）で比べれば CPU を使った量の違いが出る。
    pub fn frame_cycles(&self, prev: Option<&FrameRow>) -> Option<i64> {
        let p = prev.filter(|p| p.frame.wrapping_add(1) == self.frame)?;
        (p.present_cycles != 0 && self.present_cycles != 0)
            .then(|| diff(self.present_cycles, p.present_cycles))
    }

    /// CSV の 1 行を読む（先頭の行・`#` の行・空の行は `None`）。µs の列と、足した列の後ろ 2 列（出した値）は読まない（`glide_moved` は 0 のまま。µs からカウントへ戻すには QPF が要るので `parse_csv_at`）。
    /// 足す前の CSV（19 列まで）も読む（足した欄は「無い」）。20〜27 列は途中で切れた行として断る。
    pub fn parse_csv(line: &str) -> Result<Option<FrameRow>, String> {
        Self::parse_csv_at(line, 0)
    }

    /// `parse_csv` に加えて、`glide_moved_us`（µs・小数 1 桁）を `qpf` でカウントへ戻して `glide_moved` に入れる。最も近いカウントへ丸めるので、QPF が 10 MHz を越えて 1 カウントが 0.1 µs より短いときは元のカウントと 1 つ違いうる。`qpf` が 0 なら戻さない。
    pub fn parse_csv_at(line: &str, qpf: u64) -> Result<Option<FrameRow>, String> {
        let t = line.trim();
        if t.is_empty() || t.starts_with("frame,") || t.starts_with('#') {
            return Ok(None);
        }
        let cols: Vec<&str> = t.split(',').collect();
        if cols.len() < CSV_RAW_COLUMNS {
            return Err(format!(
                "列が {} 個（{CSV_RAW_COLUMNS} 個以上のはず）",
                cols.len()
            ));
        }
        if cols.len() > CSV_OLD_COLUMNS && cols.len() < CSV_NEW_COLUMNS {
            return Err(format!(
                "列が {} 個（足す前の {CSV_OLD_COLUMNS} 個までか、足した後の {CSV_NEW_COLUMNS} 個以上のはず）",
                cols.len()
            ));
        }
        if cols.len() == CSV_GLIDE_COLUMNS - 1 {
            return Err(format!(
                "列が {} 個（寄せの列の前の {CSV_GLIDE_AT} 個か、後の {CSV_GLIDE_COLUMNS} 個のはず）",
                cols.len()
            ));
        }
        if cols.len() > CSV_GLIDE_COLUMNS && cols.len() < CSV_HYBRID_COLUMNS {
            return Err(format!(
                "列が {} 個（b-hybrid の列の前の {CSV_GLIDE_COLUMNS} 個か、後の {CSV_HYBRID_COLUMNS} 個のはず）",
                cols.len()
            ));
        }
        let u = |i: usize| -> Result<u64, String> {
            cols[i]
                .parse::<u64>()
                .map_err(|_| format!("{} 列目 {:?} が数でない", i + 1, cols[i]))
        };
        let u32c = |i: usize| -> Result<u32, String> {
            u32::try_from(u(i)?).map_err(|_| format!("{} 列目が大きすぎる", i + 1))
        };
        let b = |i: usize| -> Result<bool, String> {
            match cols[i] {
                "0" => Ok(false),
                "1" => Ok(true),
                v => Err(format!("{} 列目 {v:?} は 0 か 1", i + 1)),
            }
        };
        let mut r = FrameRow {
            frame: u32c(0)?,
            paced: b(1)?,
            deadline: u(2)?,
            first_head: u(3)?,
            head_calls: u32c(4)?,
            sleeps: u32c(5)?,
            start: u(6)?,
            late: b(7)?,
            rebased: b(8)?,
            skipped: u32c(9)?,
            inputs: u32c(10)?,
            sup_frame: u32c(11)?,
            input: u(12)?,
            present_begin: u(13)?,
            present_end: u(14)?,
            ..Default::default()
        };
        if cols.len() >= CSV_NEW_COLUMNS {
            let opt_u32 = |i: usize| -> Result<Option<u32>, String> {
                if cols[i].is_empty() {
                    Ok(None)
                } else {
                    u32c(i).map(Some)
                }
            };
            let opt_u8 = |i: usize| -> Result<Option<u8>, String> {
                match opt_u32(i)? {
                    None => Ok(None),
                    Some(v) => u8::try_from(v)
                        .map(Some)
                        .map_err(|_| format!("{} 列目が大きすぎる", i + 1)),
                }
            };
            let opt_u64 =
                |i: usize| -> Result<u64, String> { if cols[i].is_empty() { Ok(0) } else { u(i) } };
            let o = CSV_OLD_COLUMNS;
            r.phase = match cols[o] {
                "" => None,
                v => Some(Phase::from_name(v).ok_or(format!(
                    "{} 列目 {v:?} は game / b / b-spin / b-hybrid",
                    o + 1
                ))?),
            };
            r.head_cpu = opt_u32(o + 1)?;
            r.head_eff = opt_u8(o + 2)?;
            r.input_cpu = opt_u32(o + 3)?;
            r.input_eff = opt_u8(o + 4)?;
            r.present_cpu = opt_u32(o + 5)?;
            r.present_eff = opt_u8(o + 6)?;
            r.input_cycles = opt_u64(o + 7)?;
            r.present_cycles = opt_u64(o + 8)?;
        }
        if cols.len() >= CSV_GLIDE_COLUMNS {
            let c = cols[CSV_GLIDE_AT];
            r.glide_req_ppm = c
                .parse::<i32>()
                .map_err(|_| format!("{} 列目 {c:?} が整数でない", CSV_GLIDE_AT + 1))?;
            if qpf != 0 {
                let c = cols[CSV_GLIDE_AT + 1];
                r.glide_moved = us_text_to_ticks(c, qpf).ok_or(format!(
                    "{} 列目 {c:?} が µs（小数 1 桁）でない",
                    CSV_GLIDE_AT + 2
                ))?;
            }
        }
        if cols.len() >= CSV_HYBRID_COLUMNS {
            r.hybrid_margin_us = u32c(CSV_HYBRID_AT)?;
            r.oversleep_us = if cols[CSV_HYBRID_AT + 1].is_empty() {
                None
            } else {
                Some(u32c(CSV_HYBRID_AT + 1)?)
            };
        }
        Ok(Some(r))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preamble {
    pub qpf: u64,
    pub mode: Mode,
    pub eff_classes: Vec<u8>,
    pub cycle_secs: Option<u64>,
}

impl Preamble {
    pub fn new(qpf: u64, mode: Mode) -> Preamble {
        Preamble {
            qpf,
            mode,
            eff_classes: Vec::new(),
            cycle_secs: None,
        }
    }
}

pub fn csv_preamble(p: &Preamble) -> String {
    let mut s = format!("# qpf={} mode={}", p.qpf, p.mode.name());
    if !p.eff_classes.is_empty() {
        let v: Vec<String> = p.eff_classes.iter().map(|c| c.to_string()).collect();
        s.push_str(&format!(" eff_classes={}", v.join(",")));
    }
    if let Some(c) = p.cycle_secs {
        s.push_str(&format!(" cycle_secs={c}"));
    }
    s
}

pub fn parse_preamble(line: &str) -> Option<Preamble> {
    let rest = line.trim().strip_prefix("# ")?;
    let mut qpf = None;
    let mut mode = None;
    let mut eff_classes = Vec::new();
    let mut cycle_secs = None;
    for kv in rest.split_whitespace() {
        match kv.split_once('=') {
            Some(("qpf", v)) => qpf = v.parse::<u64>().ok(),
            Some(("mode", v)) => mode = Mode::from_name(v).filter(|m| m.name() == v),
            Some(("eff_classes", v)) => {
                eff_classes = v
                    .split(',')
                    .map(|c| c.parse::<u8>().ok())
                    .collect::<Option<Vec<u8>>>()?;
            }
            Some(("cycle_secs", v)) => cycle_secs = Some(v.parse::<u64>().ok()?),
            _ => {}
        }
    }
    Some(Preamble {
        qpf: qpf?,
        mode: mode?,
        eff_classes,
        cycle_secs,
    })
}

/// `_pacing.csv` の本文を集計する（あとから集計する道具 `pacing_sum` が使う）。最初の `#` の行が無い・読めない行があれば誤り（行の番号つき）。`fps` はその回のペース（CSV には書かない）。
/// `frames` を渡せば、その番号の範囲（両端を含む）の行だけを集計する（マッチの中だけを見るとき）。
pub fn summarize_csv(
    text: &str,
    fps: u64,
    frames: Option<(u32, u32)>,
) -> Result<(Preamble, Report), String> {
    let mut lines = text.lines();
    let first = lines.next().ok_or("空のファイル")?;
    let pre =
        parse_preamble(first).ok_or(format!("1 行目が「# qpf=… mode=…」でない: {first:?}"))?;
    let mut s = Splitter::new(pre.qpf, fps, &pre.eff_classes, pre.mode.fixed_phase());
    for (i, l) in lines.enumerate() {
        if let Some(r) =
            FrameRow::parse_csv_at(l, pre.qpf).map_err(|e| format!("{} 行目: {e}", i + 2))?
            && frames.is_none_or(|(a, b)| (a..=b).contains(&r.frame))
        {
            s.push(&r);
        }
    }
    Ok((pre, s.finish()))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dist {
    pub n: usize,
    pub min: i64,
    pub p50: i64,
    pub p95: i64,
    pub p99: i64,
    pub max: i64,
    pub mean: f64,
}

pub fn percentile(sorted: &[i64], p: u32) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let rank = (p as usize * n).div_ceil(100).clamp(1, n);
    Some(sorted[rank - 1])
}

impl Dist {
    pub fn of(mut v: Vec<i64>) -> Option<Dist> {
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let sum: i128 = v.iter().map(|&x| x as i128).sum();
        Some(Dist {
            n: v.len(),
            min: v[0],
            p50: percentile(&v, 50)?,
            p95: percentile(&v, 95)?,
            p99: percentile(&v, 99)?,
            max: v[v.len() - 1],
            mean: sum as f64 / v.len() as f64,
        })
    }

    pub fn text(&self, qpf: u64) -> String {
        let mean_us = self.mean * 1_000_000.0 / qpf as f64;
        format!(
            "n = {}、p50 {} / p95 {} / p99 {} / max {} µs（最小 {} µs、平均 {:.1} µs）",
            self.n,
            us_text(self.p50, qpf),
            us_text(self.p95, qpf),
            us_text(self.p99, qpf),
            us_text(self.max, qpf),
            us_text(self.min, qpf),
            mean_us
        )
    }

    pub fn text_plain(&self, unit: &str) -> String {
        format!(
            "n = {}、p50 {} / p95 {} / p99 {} / max {} {unit}（最小 {} {unit}、平均 {:.1} {unit}）",
            self.n, self.p50, self.p95, self.p99, self.max, self.min, self.mean
        )
    }
}

/// 1 つの列の標本の上限（件）。2 の冪にするのは `Vec` の伸び方（倍々）で容量が上限ちょうどに収まり、倍の容量を確保しないため。
/// 60 fps の 1260 秒（`observe::SCRIPT_MAX_SECONDS` = 75,600 フレーム）の 1.7 倍で、60 fps なら約 36 分まで間引かない（間引かない間は従来の `Dist::of` と同じ答え）。
/// 1 列 8 B × 131,072 = 1 MiB、列は 9 本なので 1 つの集計で最大 9 MiB（集計は全体 ＋ 方式ごと ＋（方式、コア）ごと）。上限の無かった頃は 12 時間（約 2.6 M フレーム）で 1 つの集計に約 190 MB（推定）。
/// 32bit のゲームのプロセスに常駐する量を半分にするため、1 << 18 でなく 1 << 17 を採った。
pub const SERIES_CAP: usize = 1 << 17;

/// 分位数を出すための値の並び。件数・最小・最大・合計は全件を数えて正確に持ち、並びだけを上限（`SERIES_CAP`）の中に収める。
/// 収め方は決まった間引き（乱数なし）: 上限に着いたら 1 つおきに捨てて半分にし、以後は 2 倍の間隔で残す。残るのは「番号が間隔の倍数のもの」だけなので、実行の全体にわたって等間隔の標本になり、同じ入力なら同じ答え。
/// 上限に着くまでは全件が残り、`dist` は従来（全件を並べ替える `Dist::of`）と同じ値を返す。
#[derive(Debug, Clone)]
struct Series {
    samples: Vec<i64>,
    cap: usize,
    stride: u64,
    n: u64,
    min: i64,
    max: i64,
    sum: i128,
}

impl Default for Series {
    fn default() -> Series {
        Series::with_cap(SERIES_CAP)
    }
}

impl Series {
    fn with_cap(cap: usize) -> Series {
        Series {
            samples: Vec::new(),
            cap: (cap & !1).max(2),
            stride: 1,
            n: 0,
            min: 0,
            max: 0,
            sum: 0,
        }
    }

    fn push(&mut self, v: i64) {
        if self.n == 0 {
            (self.min, self.max) = (v, v);
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.sum += i128::from(v);
        if self.n.is_multiple_of(self.stride) {
            if self.samples.len() >= self.cap {
                // 全件を残していた間（標本が cap 件）は n = cap × stride で、cap が偶数なので n は 2 × stride の倍数。半分にした後もこの値は新しい間隔の倍数に当たる。
                let mut i = 0usize;
                self.samples.retain(|_| {
                    i += 1;
                    i % 2 == 1
                });
                self.stride *= 2;
            }
            self.samples.push(v);
        }
        self.n += 1;
    }

    fn is_thinned(&self) -> bool {
        self.stride > 1
    }

    fn dist(&self) -> Option<Dist> {
        if self.n == 0 {
            return None;
        }
        let mut v = self.samples.clone();
        v.sort_unstable();
        Some(Dist {
            n: self.n as usize,
            min: self.min,
            p50: percentile(&v, 50)?,
            p95: percentile(&v, 95)?,
            p99: percentile(&v, 99)?,
            max: self.max,
            mean: self.sum as f64 / self.n as f64,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Summarizer {
    qpf: u64,
    fps: u64,
    classes: Vec<u8>,
    title: Option<String>,
    rows: usize,
    paced: usize,
    periods: Series,
    wake: Series,
    in_to_begin: Series,
    in_to_end: Series,
    present: Series,
    cycles_per_us: Series,
    kinds: [usize; 5],
    cpu_pairs: usize,
    cpu_moved: usize,
    late: usize,
    rebased: usize,
    skipped: u64,
    no_input: usize,
    sleeps: u64,
    head_calls: u64,
    gaps: usize,
    long_periods: usize,
    first_begin: Option<(u32, u64)>,
    last_begin: Option<(u32, u64)>,
    prev: Option<FrameRow>,
    glide: GlideTally,
    frame_cycles: Series,
    fc_sum: i128,
    fc_ticks: i128,
    margins: Series,
    oversleeps: Series,
    late_after_sleep: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GlideTally {
    pub nonzero: usize,
    pub counted: usize,
    pub at_cap: usize,
    pub max_abs_ppm: u32,
    pub unapplied: usize,
    pub moved_abs: u64,
    pub moved_net: i64,
}

impl GlideTally {
    fn add(&mut self, r: &FrameRow) {
        self.moved_abs = self.moved_abs.saturating_add(r.glide_moved.unsigned_abs());
        self.moved_net = self.moved_net.saturating_add(r.glide_moved);
        if r.glide_req_ppm == 0 {
            return;
        }
        let a = r.glide_req_ppm.unsigned_abs();
        self.nonzero += 1;
        if a >= GLIDE_COUNT_PPM {
            self.counted += 1;
        }
        if a >= crate::rollback::GlideParams::DEFAULT.max_ppm.unsigned_abs() {
            self.at_cap += 1;
        }
        self.max_abs_ppm = self.max_abs_ppm.max(a);
        if r.glide_moved == 0 {
            self.unapplied += 1;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HybridTally {
    pub margin: Option<Dist>,
    pub oversleep: Option<Dist>,
    pub late_after_sleep: usize,
    pub frame_cycles: Option<Dist>,
    /// スレッドの使用率の見込み（論理コア 1 つを 1.0）。周期の和 ÷（その行の周期の時間 × 入力 → Present の前の「周期 ÷ µs」の中央値）。入力 → Present の前はスレッドがほぼ CPU に載り続けている区間なので、その「周期 ÷ µs」を数える速さとみなす。
    pub busy: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub qpf: u64,
    pub target_fps: u64,
    pub title: Option<String>,
    pub rows: usize,
    pub paced: usize,
    pub period: Option<Dist>,
    pub long_periods: usize,
    pub gaps: usize,
    pub wake_late: Option<Dist>,
    pub late: usize,
    pub rebased: usize,
    pub skipped: u64,
    pub sleeps: u64,
    pub head_calls: u64,
    pub input_to_present: Option<Dist>,
    pub input_to_present_end: Option<Dist>,
    pub present_len: Option<Dist>,
    pub cycles_per_us: Option<Dist>,
    pub kinds: [usize; 5],
    pub cpu_pairs: usize,
    pub cpu_moved: usize,
    pub no_input: usize,
    pub fps: Option<f64>,
    /// 格子の寄せの要求と動かした量（足す前の CSV・寄せの無い実行は全部 0）。`lines` は要求が 1 件も無ければ書かない。
    pub glide: GlideTally,
    pub hybrid: HybridTally,
    /// 間引いた列（見出し、残した間隔、標本の件数、全件の数）。間引いた列の p50 / p95 / p99 は近似で、空なら全部正確（`Series`）。
    pub thinned: Vec<(&'static str, u64, usize, u64)>,
}

impl Summarizer {
    pub fn new(qpf: u64, fps: u64) -> Summarizer {
        Summarizer {
            qpf,
            fps: if fps == 0 { DEFAULT_FPS } else { fps },
            ..Default::default()
        }
    }

    pub fn with_classes(mut self, classes: &[u8]) -> Summarizer {
        self.classes = classes.to_vec();
        self
    }

    fn titled(mut self, title: String) -> Summarizer {
        self.title = Some(title);
        self
    }

    pub fn push(&mut self, r: &FrameRow) {
        let (period, gap) = period_from(self.prev.as_ref(), r);
        let fc = r.frame_cycles(self.prev.as_ref());
        self.add(r, period, gap, fc);
    }

    fn add(&mut self, r: &FrameRow, period: Option<i64>, gap: bool, fc: Option<i64>) {
        self.rows += 1;
        if r.paced {
            self.paced += 1;
            self.head_calls += r.head_calls as u64;
            self.sleeps += r.sleeps as u64;
        }
        if r.late {
            self.late += 1;
        }
        if r.rebased {
            self.rebased += 1;
        }
        self.skipped += r.skipped as u64;
        if let Some(w) = r.wake_late() {
            self.wake.push(w);
        }
        match r.input_to_present() {
            Some(t) => self.in_to_begin.push(t),
            None => self.no_input += 1,
        }
        if let Some(t) = r.input_to_present_end() {
            self.in_to_end.push(t);
        }
        if let Some(t) = r.present_len() {
            self.present.push(t);
        }
        if let Some(c) = r.cycles_per_us(self.qpf) {
            self.cycles_per_us.push(c);
        }
        self.kinds[core_kind(r.input_eff, r.present_eff, &self.classes).index()] += 1;
        if let (Some(a), Some(b)) = (r.input_cpu, r.present_cpu) {
            self.cpu_pairs += 1;
            if a != b {
                self.cpu_moved += 1;
            }
        }
        if gap {
            self.gaps += 1;
        }
        if let Some(p) = period {
            self.periods.push(p);
            if p > self.long_limit() {
                self.long_periods += 1;
            }
        }
        if r.present_begin != 0 {
            if self.first_begin.is_none() {
                self.first_begin = Some((r.frame, r.present_begin));
            }
            self.last_begin = Some((r.frame, r.present_begin));
        }
        self.glide.add(r);
        self.hybrid_add(r, period, fc);
        self.prev = Some(*r);
    }

    fn hybrid_add(&mut self, r: &FrameRow, period: Option<i64>, fc: Option<i64>) {
        if r.hybrid_margin_us > 0 {
            self.margins.push(i64::from(r.hybrid_margin_us));
        }
        if let Some(o) = r.oversleep_us {
            self.oversleeps.push(i64::from(o));
            if r.late {
                self.late_after_sleep += 1;
            }
        }
        if let Some(c) = fc {
            self.frame_cycles.push(c);
            if let Some(p) = period {
                self.fc_sum += i128::from(c);
                self.fc_ticks += i128::from(p);
            }
        }
    }

    fn long_limit(&self) -> i64 {
        (self.qpf as i128 * 3 / (2 * self.fps.max(MIN_FPS) as i128)) as i64
    }

    fn thinned(&self) -> Vec<(&'static str, u64, usize, u64)> {
        [
            ("周期", &self.periods),
            ("起床の遅れ", &self.wake),
            ("入力 → Present の前", &self.in_to_begin),
            ("入力 → Present の後", &self.in_to_end),
            ("Present の長さ", &self.present),
            ("周期 ÷ µs", &self.cycles_per_us),
            ("1 フレームのスレッドの周期", &self.frame_cycles),
            ("b-hybrid の余裕", &self.margins),
            ("寝過ごし", &self.oversleeps),
        ]
        .into_iter()
        .filter(|(_, s)| s.is_thinned())
        .map(|(name, s)| (name, s.stride, s.samples.len(), s.n))
        .collect()
    }

    #[cfg(test)]
    fn with_series_cap(mut self, cap: usize) -> Summarizer {
        for s in [
            &mut self.periods,
            &mut self.wake,
            &mut self.in_to_begin,
            &mut self.in_to_end,
            &mut self.present,
            &mut self.cycles_per_us,
            &mut self.frame_cycles,
            &mut self.margins,
            &mut self.oversleeps,
        ] {
            *s = Series::with_cap(cap);
        }
        self
    }

    fn hybrid_tally(&self) -> HybridTally {
        let rate = self.cycles_per_us.dist().map(|d| d.p50);
        let busy = match rate {
            Some(rate) if rate > 0 && self.fc_ticks > 0 && self.qpf > 0 => Some(
                self.fc_sum as f64 * self.qpf as f64
                    / (self.fc_ticks as f64 * 1_000_000.0 * rate as f64),
            ),
            _ => None,
        };
        HybridTally {
            margin: self.margins.dist(),
            oversleep: self.oversleeps.dist(),
            late_after_sleep: self.late_after_sleep,
            frame_cycles: self.frame_cycles.dist(),
            busy,
        }
    }

    pub fn finish(&self) -> Summary {
        let fps = match (self.first_begin, self.last_begin) {
            (Some((f0, t0)), Some((f1, t1))) if t1 > t0 && f1 > f0 && self.title.is_none() => {
                Some((f1 - f0) as f64 * self.qpf as f64 / (t1 - t0) as f64)
            }
            _ => None,
        };
        Summary {
            qpf: self.qpf,
            target_fps: self.fps,
            title: self.title.clone(),
            rows: self.rows,
            paced: self.paced,
            period: self.periods.dist(),
            long_periods: self.long_periods,
            gaps: self.gaps,
            wake_late: self.wake.dist(),
            late: self.late,
            rebased: self.rebased,
            skipped: self.skipped,
            sleeps: self.sleeps,
            head_calls: self.head_calls,
            input_to_present: self.in_to_begin.dist(),
            input_to_present_end: self.in_to_end.dist(),
            present_len: self.present.dist(),
            cycles_per_us: self.cycles_per_us.dist(),
            kinds: self.kinds,
            cpu_pairs: self.cpu_pairs,
            cpu_moved: self.cpu_moved,
            no_input: self.no_input,
            fps,
            glide: self.glide,
            hybrid: self.hybrid_tally(),
            thinned: self.thinned(),
        }
    }
}

fn period_from(prev: Option<&FrameRow>, r: &FrameRow) -> (Option<i64>, bool) {
    match prev {
        Some(p) if p.frame.wrapping_add(1) != r.frame => (None, true),
        Some(p) if p.present_begin != 0 && r.present_begin != 0 => {
            (Some(diff(r.present_begin, p.present_begin)), false)
        }
        _ => (None, false),
    }
}

impl Summary {
    pub fn lines(&self) -> Vec<String> {
        let q = self.qpf;
        let dist = |d: &Option<Dist>| match d {
            Some(d) => d.text(q),
            None => "n = 0（測れていない）".to_string(),
        };
        let head = match &self.title {
            None => "ペースの集計".to_string(),
            Some(t) => format!("ペースの集計 [{t}]"),
        };
        let mut v = vec![format!(
            "{head}: フレーム {} 件（うち B で先頭を決めた {} 件）、QPF {} Hz、ペース {} fps、番号の飛び {} 所",
            self.rows, self.paced, q, self.target_fps, self.gaps
        )];
        v.push(format!(
            "  周期（Present の前の間隔）: {}",
            dist(&self.period)
        ));
        v.push(format!(
            "  1.5 周期より長い周期: {} 件（周期 {} 件のうち）",
            self.long_periods,
            self.period.map_or(0, |d| d.n)
        ));
        if self.title.is_none() {
            v.push(match self.fps {
                Some(f) => format!("  最初と最後の Present から出した fps: {f:.4}"),
                None => "  fps: 出せない（Present が 2 件未満）".to_string(),
            });
        }
        if self.paced > 0 {
            v.push(format!(
                "  起床の遅れ（始めた時刻 − 期限）: {}",
                dist(&self.wake_late)
            ));
            v.push(format!(
                "  遅れて先頭へ来た: {} 件（B の {} 件のうち）、飛ばした格子点 {}、基点の置き直し {} 件、寝た回数 {}、先頭へ来た回数 {}",
                self.late, self.paced, self.skipped, self.rebased, self.sleeps, self.head_calls
            ));
        }
        v.push(format!(
            "  入力 → Present の前: {}",
            dist(&self.input_to_present)
        ));
        v.push(format!(
            "  入力 → Present の後: {}",
            dist(&self.input_to_present_end)
        ));
        v.push(format!(
            "  入力の地点を通らなかったフレーム: {} 件（{} 件のうち）",
            self.no_input, self.rows
        ));
        v.push(format!("  Present の長さ: {}", dist(&self.present_len)));
        v.push(format!(
            "  入力 → Present の前の周期 ÷ µs（QueryThreadCycleTime。ずっと CPU に載っていれば数える速さに近く、載っていない時間の割合だけ小さい）: {}",
            match &self.cycles_per_us {
                Some(d) => d.text_plain("周期/µs"),
                None => "n = 0（測れていない）".to_string(),
            }
        ));
        // 古い CSV・寝ない実行では出さない（0 件と書かない）。
        let h = &self.hybrid;
        if let Some(d) = &h.frame_cycles {
            v.push(format!(
                "  1 フレームのスレッドの周期（Present の前どうしの QueryThreadCycleTime の差。寝ていた間は数えない）: {}、使用率の見込み {}",
                d.text_plain("周期"),
                match h.busy {
                    Some(b) => format!(
                        "{:.1}%（論理コア 1 つを 100%。周期の和 ÷（その周期の時間 × 入力 → Present の前の周期 ÷ µs の中央値。見積もり））",
                        b * 100.0
                    ),
                    None => "出せない（周期 ÷ µs が無い）".to_string(),
                }
            ));
        }
        if let Some(d) = &h.oversleep {
            v.push(format!(
                "  寝過ごし（b-hybrid の先頭が待機タイマーで寝た枠の、1 枠の中の最大）: {}。寝た枠のうち遅れて先頭へ来た {} 件（寝た枠 {} 件のうち）",
                d.text_plain("µs"),
                h.late_after_sleep,
                d.n
            ));
        }
        if let Some(d) = &h.margin {
            v.push(format!(
                "  b-hybrid の余裕（1 枠の中の最大）: {}",
                d.text_plain("µs")
            ));
        }
        let kinds: Vec<String> = CORE_KINDS
            .iter()
            .map(|k| format!("{} {}", k.name(), self.kinds[k.index()]))
            .collect();
        v.push(format!(
            "  コアの種類（入力と Present の前の論理 CPU の効率クラス）: {}（{} 件のうち）",
            kinds.join(" / "),
            self.rows
        ));
        v.push(format!(
            "  入力と Present の前で論理 CPU が違った: {} 件（両方が取れた {} 件のうち）",
            self.cpu_moved, self.cpu_pairs
        ));
        let g = &self.glide;
        if g.nonzero > 0 {
            let ms = |ticks: i128| ticks as f64 * 1_000.0 / q.max(1) as f64;
            v.push(format!(
                "  格子の寄せ（枠の先頭が読んだ要求。正 = 伸ばす）: 要求が 0 でなかった枠 {} 件（{} 件のうち）・|要求| {GLIDE_COUNT_PPM} ppm 以上 {} 件・既定の上限 {} ppm 以上 {} 件・|要求| の最大 {} ppm・動かさなかった要求 {} 件・動かした量の絶対値の合計 {:.1} ms・正味 {:+.1} ms",
                g.nonzero,
                self.rows,
                g.counted,
                crate::rollback::GlideParams::DEFAULT.max_ppm,
                g.at_cap,
                g.max_abs_ppm,
                g.unapplied,
                ms(i128::from(g.moved_abs)),
                ms(i128::from(g.moved_net)),
            ));
        }
        if !self.thinned.is_empty() {
            let cols: Vec<String> = self
                .thinned
                .iter()
                .map(|(name, stride, kept, n)| {
                    format!("{name} {stride} 件に 1 件（標本 {kept} 件 / 全 {n} 件）")
                })
                .collect();
            v.push(format!(
                "  分位数は近似: 列が上限 {SERIES_CAP} 件を越えたので、走りの全体から等間隔に残した標本で p50 / p95 / p99 を出した（{}）。件数・最小・最大・平均・長い周期の数は全件で正確",
                cols.join("、")
            ));
        }
        v
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub all: Summary,
    pub by_phase: Vec<(Phase, Summary)>,
    pub by_core: Vec<(Phase, CoreKind, Summary)>,
}

impl Report {
    /// 本ログと `pacing_sum` に書く行。方式の集計は 2 つ以上の方式が出たときだけ、コアの種類の集計はどれか 1 件でも種類が分かったときだけ。
    pub fn lines(&self) -> Vec<String> {
        let mut v = self.all.lines();
        if self.by_phase.len() > 1 {
            for (_, s) in &self.by_phase {
                v.extend(s.lines());
            }
        }
        if self.by_core.iter().any(|(_, k, _)| *k != CoreKind::Unknown) {
            for (_, _, s) in &self.by_core {
                v.extend(s.lines());
            }
        }
        v
    }

    pub fn phase(&self, p: Phase) -> Option<&Summary> {
        self.by_phase.iter().find(|x| x.0 == p).map(|x| &x.1)
    }

    pub fn core(&self, p: Phase, k: CoreKind) -> Option<&Summary> {
        self.by_core
            .iter()
            .find(|x| x.0 == p && x.1 == k)
            .map(|x| &x.2)
    }
}

/// 行を受けて、全体・方式ごと・（方式、コアの種類）ごとに集計する（書き出しのスレッドと `pacing_sum`）。分けた集計の周期は「その行と 1 つ前の行（方式を問わない）」の差で、区間の境目をまたいで数十秒の周期を作らない。
#[derive(Debug, Clone)]
pub struct Splitter {
    qpf: u64,
    fps: u64,
    classes: Vec<u8>,
    default_phase: Option<Phase>,
    all: Summarizer,
    phases: Vec<(Phase, Summarizer)>,
    cores: Vec<(Phase, CoreKind, Summarizer)>,
    prev: Option<FrameRow>,
}

impl Splitter {
    pub fn new(qpf: u64, fps: u64, classes: &[u8], default_phase: Option<Phase>) -> Splitter {
        Splitter {
            qpf,
            fps,
            classes: classes.to_vec(),
            default_phase,
            all: Summarizer::new(qpf, fps).with_classes(classes),
            phases: Vec::new(),
            cores: Vec::new(),
            prev: None,
        }
    }

    pub fn push(&mut self, r: &FrameRow) {
        self.all.push(r);
        let (period, gap) = period_from(self.prev.as_ref(), r);
        let fc = r.frame_cycles(self.prev.as_ref());
        self.prev = Some(*r);
        let Some(phase) = r.phase.or(self.default_phase) else {
            return;
        };
        let kind = core_kind(r.input_eff, r.present_eff, &self.classes);
        let (qpf, fps, classes) = (self.qpf, self.fps, &self.classes);
        let i = match self.phases.iter().position(|x| x.0 == phase) {
            Some(i) => i,
            None => {
                self.phases.push((
                    phase,
                    Summarizer::new(qpf, fps)
                        .with_classes(classes)
                        .titled(phase.name().to_string()),
                ));
                self.phases.len() - 1
            }
        };
        self.phases[i].1.add(r, period, gap, fc);
        let j = match self.cores.iter().position(|x| x.0 == phase && x.1 == kind) {
            Some(j) => j,
            None => {
                self.cores.push((
                    phase,
                    kind,
                    Summarizer::new(qpf, fps)
                        .with_classes(classes)
                        .titled(format!("{} / {}", phase.name(), kind.name())),
                ));
                self.cores.len() - 1
            }
        };
        self.cores[j].2.add(r, period, gap, fc);
    }

    pub fn finish(&self) -> Report {
        let mut by_phase: Vec<(Phase, Summary)> =
            self.phases.iter().map(|(p, s)| (*p, s.finish())).collect();
        by_phase.sort_by_key(|x| x.0);
        let mut by_core: Vec<(Phase, CoreKind, Summary)> = self
            .cores
            .iter()
            .map(|(p, k, s)| (*p, *k, s.finish()))
            .collect();
        by_core.sort_by_key(|x| (x.0, x.1));
        Report {
            all: self.all.finish(),
            by_phase,
            by_core,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QPFS: [u64; 6] = [
        10_000_000,
        3_579_545,
        14_318_180,
        2_148_437,
        24_000_000,
        2_999_999_987,
    ];

    const FPSES: [u64; 4] = [MIN_FPS, DEFAULT_FPS, 240, MAX_FPS];

    /// 花映塚の時計と期限の比べを、単精度の丸めごと真似る。QPC の下位 32 ビットは `fildl` で符号つきに読み、負なら 2^32 を足す（ここで単精度に丸まる）。割り算も単精度に丸まる。
    fn game_runs(qpc_low: u32, divisor: u32, deadline: f64) -> bool {
        let l = if qpc_low >= 1 << 31 {
            f64::from(qpc_low as f32)
        } else {
            f64::from(qpc_low)
        };
        let now = f64::from((l / f64::from(divisor)) as f32);
        now > deadline
    }

    fn next_deadline(d: f64) -> f64 {
        f64::from((d + 1.0 / 60.0) as f32)
    }

    #[test]
    fn game_frame_due_covers_every_frame_the_game_starts_and_is_not_early() {
        for divisor in [10_000_000u32, 3_579_545, 14_318_180, 24_000_000] {
            let early = game_due_early_ticks(divisor);
            let mut misses_without_early = 0u32;
            for start_s in [1.0f64, 100.0, 200.0, 215.0, 240.0, 255.0, 300.0, 428.0] {
                let mut d = f64::from(start_s as f32);
                for _ in 0..40 {
                    d = next_deadline(d);
                    let center = d * f64::from(divisor);
                    if center + f64::from(early) * 2.0 >= 4_294_967_295.0 {
                        break;
                    }
                    let lo = (center as i64 - i64::from(early) * 2).max(0) as u32;
                    let hi = (center as i64 + i64::from(early) * 2) as u32;
                    let mut first_run = None;
                    let mut first_due = None;
                    for l in lo..=hi {
                        let runs = game_runs(l, divisor, d);
                        let due = game_frame_due(l, divisor, d.to_bits(), early);
                        assert!(
                            !runs || due,
                            "取りこぼし: divisor {divisor} 期限 {d} qpc {l}"
                        );
                        if runs && !game_frame_due(l, divisor, d.to_bits(), 0) {
                            misses_without_early += 1;
                        }
                        if runs && first_run.is_none() {
                            first_run = Some(l);
                        }
                        if due && first_due.is_none() {
                            first_due = Some(l);
                        }
                    }
                    let (run, due) = (first_run.unwrap(), first_due.unwrap());
                    assert!(due <= run, "divisor {divisor} 期限 {d}");
                    assert!(
                        run - due <= early + early / 2,
                        "早すぎる: divisor {divisor} 期限 {d} 始める {run} 真 {due}"
                    );
                }
            }
            if divisor == 10_000_000 {
                assert!(
                    misses_without_early > 0,
                    "早めが 0 でも取りこぼさない —— 検査が 2^31 を越えた QPC を通っていない"
                );
            }
        }
    }

    #[test]
    fn game_frame_due_edges() {
        let f = 10_000_000u32;
        let d = 300.0f64.to_bits();
        assert!(!game_frame_due(3_000_000_000, f, d, 0));
        assert!(game_frame_due(3_000_000_001, f, d, 0));
        assert!(!game_frame_due(3_000_000_000 - 2_000, f, d, 2_000));
        assert!(game_frame_due(3_000_000_000 - 1_999, f, d, 2_000));
        assert_eq!(game_due_early_ticks(f), 2_000);
        assert!(!game_frame_due(3_000_000_001, 0, d, 0));
        assert!(!game_frame_due(u32::MAX, f, (-1.0f64).to_bits(), 0));
        assert!(!game_frame_due(u32::MAX, f, (-0.0f64).to_bits(), 0));
        assert!(!game_frame_due(u32::MAX, f, f64::NAN.to_bits(), 0));
        assert!(!game_frame_due(u32::MAX, f, f64::INFINITY.to_bits(), 0));
        assert!(game_frame_due(1, f, 0.0f64.to_bits(), 0));
        assert!(!game_frame_due(0, f, 0.0f64.to_bits(), 0));
        assert!(game_frame_due(1, f, f64::from_bits(1).to_bits(), 0));
        assert!(!game_frame_due(100_000, f, 429.0f64.to_bits(), 2_000));
        assert!(!game_frame_due(
            u32::MAX,
            u32::MAX,
            (2.0f64).powi(60).to_bits(),
            u32::MAX
        ));
    }

    #[test]
    fn grid_is_exact_over_a_day_for_any_frequency() {
        for fps in FPSES {
            for qpf in QPFS {
                let g = Grid::new(123_456_789, qpf, fps).unwrap();
                for k in [1u64, 60, 3_600, 86_400, 86_400 * 30] {
                    assert_eq!(
                        g.at(fps * k) - g.base(),
                        k * qpf,
                        "fps {fps} qpf {qpf} k {k}"
                    );
                }
                let lo = qpf / fps;
                let hi = qpf.div_ceil(fps);
                for j in (0..10_000u64).chain(5_184_000..5_184_100) {
                    let d = g.at(j + 1) - g.at(j);
                    assert!(d == lo || d == hi, "fps {fps} qpf {qpf} j {j}: {d}");
                }
            }
        }
        for fps in FPSES {
            assert!(Grid::new(0, fps - 1, fps).is_none(), "fps {fps}");
            assert!(Pace::new(fps - 1, fps).is_none(), "fps {fps}");
            assert!(Grid::new(0, fps, fps).is_some(), "fps {fps}");
        }
        assert!(Grid::new(0, 10_000_000, 0).is_none());
        assert!(Pace::new(10_000_000, 0).is_none());
    }

    #[test]
    fn one_second_holds_exactly_fps_grid_points() {
        for fps in FPSES {
            for qpf in QPFS {
                let g = Grid::new(5_000, qpf, fps).unwrap();
                let end = g.base() + qpf;
                assert!(g.at(fps - 1) < end, "fps {fps} qpf {qpf}");
                assert_eq!(g.at(fps), end, "fps {fps} qpf {qpf}");
                assert_eq!(g.first_after(end - 1), fps, "fps {fps} qpf {qpf}");
                let p = Pace::new(qpf, fps).unwrap();
                assert_eq!(p.fps(), fps);
            }
        }
    }

    #[test]
    fn first_after_is_strictly_after() {
        for fps in FPSES {
            for qpf in QPFS {
                let g = Grid::new(1_000, qpf, fps).unwrap();
                assert_eq!(g.first_after(0), 0);
                assert_eq!(g.first_after(999), 0);
                assert_eq!(g.first_after(1_000), 1);
                for j in [1u64, 2, 59, 60, 61, 239, 240, 241, 1_000_003] {
                    let p = g.at(j);
                    assert_eq!(g.first_after(p), j + 1, "fps {fps} qpf {qpf} j {j}");
                    assert_eq!(g.first_after(p - 1), j, "fps {fps} qpf {qpf} j {j}");
                    assert!(g.at(g.first_after(p + 1)) > p + 1);
                }
            }
        }
    }

    #[test]
    fn nudge_shifts_the_whole_grid_by_period_times_ppm() {
        let qpf = 10_000_000u64;
        let mut p = Pace::new(qpf, 60).unwrap();
        assert_eq!(p.nudge(5_000), 0, "格子が無い間は動かさない");
        p.deadline(1_000_000);
        let g0 = p.grid().unwrap();
        assert_eq!(p.nudge(1_000), 166);
        let g1 = p.grid().unwrap();
        for j in [0u64, 1, 59, 60, 1_000_003] {
            assert_eq!(
                g1.at(j),
                g0.at(j) + 166,
                "j {j}: 全部の格子点が同じ量だけ遅れる"
            );
        }
        assert_eq!(g1.at(60) - g1.at(0), qpf, "1 秒 60 点の間隔は変わらない");
        assert_eq!(p.nudge(1_000), 167);
        let before = p.grid().unwrap().at(5);
        assert_eq!(p.nudge(-10_000), -1_666);
        assert_eq!(p.grid().unwrap().at(5), before - 1_666);
        let mut q = Pace::new(qpf, 60).unwrap();
        q.deadline(1_000_000);
        let b0 = q.grid().unwrap().base();
        let sum: i64 = (0..6_000).map(|_| q.nudge(1)).sum();
        assert_eq!(sum, 1_000);
        assert_eq!(q.grid().unwrap().base(), b0 + 1_000);
        let mut z = Pace::new(qpf, 60).unwrap();
        z.deadline(100);
        z.nudge(-1_000_000);
        assert_eq!(z.grid().unwrap().base(), 0);
    }

    fn run_on_time(qpf: u64, fps: u64, frames: u64, jitter: impl Fn(u64) -> u64) -> Pace {
        let mut p = Pace::new(qpf, fps).unwrap();
        let mut now = 5_000_000_000u64;
        for n in 0..frames {
            let d = p.deadline(now);
            if n > 0 {
                assert_eq!(
                    d,
                    p.grid().unwrap().at(n),
                    "qpf {qpf}: {n} 番目の期限が格子点でない"
                );
            }
            now = now.max(d) + jitter(n);
            assert_eq!(p.started(now), 0, "qpf {qpf}: {n} 番目で格子点を飛ばした");
            now += 1;
        }
        p
    }

    #[test]
    fn an_hour_on_time_stays_on_the_grid() {
        for (fps, secs) in [(DEFAULT_FPS, 3_600u64), (240, 600)] {
            for qpf in QPFS {
                let jit = qpf / fps / 4;
                let mut p = run_on_time(qpf, fps, fps * secs, |n| (n * 7_919) % jit.max(1));
                let base = p.grid().unwrap().base();
                let d = p.deadline(base + secs * qpf - 1);
                assert_eq!(d - base, secs * qpf, "fps {fps} qpf {qpf}");
                assert_eq!(p.rebased(), 0);
            }
        }
    }

    #[test]
    fn late_frames_keep_the_grid_and_do_not_catch_up() {
        for qpf in QPFS {
            let mut p = Pace::new(qpf, DEFAULT_FPS).unwrap();
            let base = 77_000u64;
            assert_eq!(
                p.decide(base, 0, 1_000),
                Head::Run {
                    deadline: base,
                    late: false
                }
            );
            assert_eq!(p.started(base), 0);
            let g = p.grid().unwrap();
            let half = (g.at(2) - g.at(1)) / 2;
            let now = g.at(1) + half;
            assert_eq!(
                p.decide(now, 0, 1_000),
                Head::Run {
                    deadline: g.at(1),
                    late: true
                }
            );
            assert_eq!(p.started(now), 0);
            assert_eq!(p.deadline(now), g.at(2));
            let now = g.at(2) + (g.at(5) - g.at(2)) + (g.at(6) - g.at(5)) / 5;
            assert!(matches!(
                p.decide(now, 0, 1_000),
                Head::Run { late: true, .. }
            ));
            assert_eq!(p.started(now), 3);
            let next = p.deadline(now);
            assert_eq!(next, g.at(6), "qpf {qpf}");
            assert!(next > now, "取り戻す（過ぎた期限を返す）");
            assert_eq!(p.grid().unwrap().base(), base, "基点を置き直した");
        }
    }

    #[test]
    fn decide_sleeps_in_chunks_then_spins_then_runs() {
        let qpf = 10_000_000; // 1 µs = 10 カウント
        let mut p = Pace::new(qpf, DEFAULT_FPS).unwrap();
        p.decide(0, 0, 1_000);
        p.started(0);
        let d = p.deadline(1); // 166,666 カウント
        assert_eq!(d, 166_666);
        let spin = 10_000; // 1 ms
        assert_eq!(
            p.decide(1, spin, 1_000),
            Head::Sleep {
                deadline: d,
                us: 1_000
            }
        );
        assert_eq!(
            p.decide(d - 15_000, spin, 1_000),
            Head::Sleep {
                deadline: d,
                us: 500
            }
        );
        assert_eq!(
            p.decide(d - spin - 5, spin, 1_000),
            Head::Sleep { deadline: d, us: 1 }
        );
        assert_eq!(p.decide(d - spin, spin, 1_000), Head::Spin { deadline: d });
        assert_eq!(p.decide(d - 1, spin, 1_000), Head::Spin { deadline: d });
        assert_eq!(
            p.decide(d, spin, 1_000),
            Head::Run {
                deadline: d,
                late: false
            }
        );
        assert_eq!(
            p.decide(1, spin, 100_000),
            Head::Sleep {
                deadline: d,
                us: 15_666
            }
        );
        let mut p = Pace::new(qpf, 240).unwrap();
        p.decide(0, 0, 1_000);
        p.started(0);
        assert_eq!(p.deadline(1), 41_666);
        assert_eq!(
            p.decide(1, spin, 100_000),
            Head::Sleep {
                deadline: 41_666,
                us: 3_166
            }
        );
    }

    #[test]
    fn a_far_deadline_is_rebased_instead_of_waited() {
        let qpf = 10_000_000;
        let mut p = Pace::new(qpf, DEFAULT_FPS).unwrap();
        p.decide(1_000_000_000, 0, 1_000);
        p.started(1_000_000_000);
        let now = 1_000_000_000 - qpf;
        assert_eq!(
            p.decide(now, 0, 1_000),
            Head::Run {
                deadline: now,
                late: false
            }
        );
        assert_eq!(p.rebased(), 1);
        assert_eq!(p.grid().unwrap().base(), now);
        let mut p = Pace::new(qpf, DEFAULT_FPS).unwrap();
        p.decide(0, 0, 1_000);
        p.started(0);
        assert_eq!(
            p.started(333_333),
            1,
            "2 番目の格子点ちょうどに始めた → 次は 3 番目"
        );
        assert_eq!(p.deadline(166_666), 500_000);
        assert_eq!(p.rebased(), 0);
        assert_eq!(p.deadline(166_665), 166_665);
        assert_eq!(p.rebased(), 1);
        let mut p = Pace::new(qpf, 240).unwrap();
        p.decide(0, 0, 1_000);
        p.started(0);
        assert_eq!(p.started(83_333), 1);
        assert_eq!(p.deadline(41_666), 125_000);
        assert_eq!(p.rebased(), 0);
        assert_eq!(p.deadline(41_665), 41_665);
        assert_eq!(p.rebased(), 1);
    }

    #[test]
    fn mode_from_ini_reads_only_the_pacing_section() {
        let m = |t: &str| mode_from_ini(&crate::ini::parse(t).unwrap());
        let game = PacingIni {
            mode: Mode::Game,
            fps: DEFAULT_FPS,
            cycle_secs: DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        let b = |fps| PacingIni {
            mode: Mode::B,
            fps,
            cycle_secs: DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        assert_eq!(m("[PACING]\nmode = B\n"), Ok(b(DEFAULT_FPS)));
        assert_eq!(m("[pacing]\nmode = game\n"), Ok(game));
        assert_eq!(m("[pacing]\nmode = GAME\n"), Ok(game));
        assert!(
            m("[pacing]\nmode = vpatc\n")
                .unwrap_err()
                .contains("game / b / b-spin / b-hybrid / vpatch / cycle / auto のどれか")
        );
        assert!(m("[pacing]\nmdoe = b\n").unwrap_err().contains("知らない"));
        assert_eq!(m("[pacing]\nmode = b\nfps = 240\n"), Ok(b(240)));
        assert_eq!(m("[pacing]\nFPS = 240\nmode = b\n"), Ok(b(240)));
        assert_eq!(m("[pacing]\nmode = b\nfps = 1\n"), Ok(b(1)));
        assert_eq!(m("[pacing]\nmode = b\nfps = 1000\n"), Ok(b(1_000)));
        assert_eq!(m("[pacing]\nmode = b\nfps = 60\n"), Ok(b(60)));
        for bad in [
            "0",
            "1001",
            "-1",
            "+240",
            "240.0",
            "abc",
            "",
            "99999999999999999999",
        ] {
            let e = m(&format!("[pacing]\nmode = b\nfps = {bad}\n")).unwrap_err();
            assert!(e.contains("1〜1000 の整数"), "{bad:?}: {e}");
        }
        for text in [
            "[pacing]\nmode = game\nfps = 240\n",
            "[pacing]\nmode = game\nfps = 1\n",
            "[pacing]\nfps = 1000\nmode = game\n",
        ] {
            let e = m(text).unwrap_err();
            assert!(
                e.contains("mode = b / b-spin / b-hybrid / vpatch のときだけ効く"),
                "{text:?}: {e}"
            );
        }
        assert_eq!(m("[pacing]\nmode = game\nfps = 60\n"), Ok(game));
        assert_eq!(parse_fps(" 240 "), Ok(240));
    }

    #[test]
    fn absent_section_or_mode_is_auto_at_60() {
        let m = |t: &str| mode_from_ini(&crate::ini::parse(t).unwrap());
        let with = |mode, fps| PacingIni {
            mode,
            fps,
            cycle_secs: DEFAULT_CYCLE_SECS,
            ..PacingIni::DEFAULT
        };
        assert_eq!(DEFAULT_MODE, Mode::Auto);
        assert_eq!(PacingIni::DEFAULT, with(Mode::Auto, 60));
        for text in [
            "",
            "; 注だけ\n",
            "[netplay]\nenable = 1\n",
            "[pacing]\n",
            "[pacing]\nfps = 60\n",
            "[pacing]\nmode = auto\n",
            "[pacing]\nmode = AUTO\nfps = 60\n",
        ] {
            assert_eq!(m(text), Ok(PacingIni::DEFAULT), "{text:?}");
        }
        for (text, want) in [
            ("[pacing]\nmode = game\n", with(Mode::Game, 60)),
            ("[pacing]\nmode = b\n", with(Mode::B, 60)),
            (
                "[pacing]\nmode = b-spin\nfps = 240\n",
                with(Mode::BSpin, 240),
            ),
        ] {
            assert_eq!(m(text), Ok(want), "{text:?}");
        }
        for text in ["[pacing]\nfps = 240\n", "[pacing]\nmode = auto\nfps = 1\n"] {
            let e = m(text).unwrap_err();
            assert!(
                e.contains("mode = b / b-spin / b-hybrid / vpatch のときだけ効く"),
                "{text:?}: {e}"
            );
        }
        let e = m("[pacing]\ncycle_secs = 30\n").unwrap_err();
        assert!(e.contains("mode = cycle のときだけ書ける"), "{e}");
        assert!(
            m("[pacing]\nmdoe = game\n")
                .unwrap_err()
                .contains("知らない")
        );
        assert!(
            m("[pacing]\nfps = 0\n")
                .unwrap_err()
                .contains("1〜1000 の整数")
        );
    }

    fn sample(frame: u32) -> FrameRow {
        FrameRow {
            frame,
            paced: true,
            deadline: 1_000_000,
            first_head: 990_000,
            head_calls: 4,
            sleeps: 3,
            start: 1_000_025,
            late: false,
            rebased: false,
            skipped: 0,
            inputs: 2,
            sup_frame: 812,
            input: 1_000_400,
            present_begin: 1_030_400,
            present_end: 1_031_000,
            ..Default::default()
        }
    }

    fn old_part(line: &str) -> String {
        line.split(',').take(19).collect::<Vec<_>>().join(",")
    }

    #[test]
    fn csv_roundtrip_and_microseconds() {
        let r = sample(7);
        let prev = FrameRow {
            frame: 6,
            present_begin: 1_030_400 - 166_667,
            ..sample(6)
        };
        let line = r.csv_line(Some(&prev), 10_000_000);
        assert_eq!(
            line.split(',').count(),
            CSV_HEADER.split(',').count(),
            "先頭の行と列の数"
        );
        assert!(
            old_part(&line).ends_with(",2.5,3000.0,60.0,16666.7"),
            "10 MHz の µs: {line}"
        );
        assert_eq!(FrameRow::parse_csv(&line), Ok(Some(r)));
        assert_eq!(FrameRow::parse_csv(CSV_HEADER), Ok(None));
        let line = r.csv_line(None, 3_579_545);
        assert!(old_part(&line).ends_with(",6.9,8380.9,167.6,"), "{line}");
        let far = FrameRow { frame: 5, ..prev };
        assert!(old_part(&r.csv_line(Some(&far), 10_000_000)).ends_with(",60.0,"));
        assert!(FrameRow::parse_csv("1,2,3").is_err());
        assert!(FrameRow::parse_csv(&line.replacen(",1,", ",2,", 1)).is_err());
    }

    #[test]
    fn summarize_csv_reads_the_preamble_and_every_row() {
        let qpf = 3_579_545;
        let rows: Vec<FrameRow> = (0..4).map(sample).collect();
        let mut text = format!(
            "{}\n{CSV_HEADER}\n",
            csv_preamble(&Preamble::new(qpf, Mode::B))
        );
        let mut prev = None;
        for r in &rows {
            text.push_str(&r.csv_line(prev.as_ref(), qpf));
            text.push('\n');
            prev = Some(*r);
        }
        let (pre, rep) = summarize_csv(&text, DEFAULT_FPS, None).unwrap();
        let s = rep.all;
        assert_eq!(pre.mode, Mode::B);
        assert_eq!((s.qpf, s.rows, s.paced), (qpf, 4, 4));
        assert_eq!(s.target_fps, DEFAULT_FPS);
        assert_eq!(
            summarize_csv(&text, 240, None).unwrap().1.all.target_fps,
            240
        );
        assert_eq!(
            parse_preamble("# qpf=10000000 mode=game"),
            Some(Preamble::new(10_000_000, Mode::Game))
        );
        assert_eq!(parse_preamble("# mode=game"), None);
        assert_eq!(parse_preamble("frame,paced"), None);
        assert!(
            summarize_csv(&text[text.find('\n').unwrap() + 1..], DEFAULT_FPS, None).is_err(),
            "最初の行が無い"
        );
        let bad = text.replacen("\n1,1,", "\n1,x,", 1);
        assert!(
            summarize_csv(&bad, DEFAULT_FPS, None)
                .unwrap_err()
                .starts_with("4 行目"),
            "{bad}"
        );
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50), Some(50));
        assert_eq!(percentile(&v, 95), Some(95));
        assert_eq!(percentile(&v, 99), Some(99));
        assert_eq!(percentile(&v, 100), Some(100));
        let w: Vec<i64> = (1..=10).collect();
        assert_eq!(percentile(&w, 50), Some(5));
        assert_eq!(percentile(&w, 95), Some(10));
        assert_eq!(percentile(&[], 50), None);
        let d = Dist::of(vec![3, 1, 2]).unwrap();
        assert_eq!((d.n, d.min, d.p50, d.max), (3, 1, 2, 3));
    }

    #[test]
    fn summary_counts_what_it_measured() {
        let qpf = 10_000_000;
        let mut s = Summarizer::new(qpf, DEFAULT_FPS);
        let p = 166_667u64;
        let mut rows = Vec::new();
        for f in 0..10u32 {
            let mut r = sample(f);
            r.present_begin = 2_000_000 + f as u64 * p;
            r.present_end = r.present_begin + 500;
            r.input = r.present_begin - 30_000;
            rows.push(r);
        }
        rows[3].inputs = 0; // 入力なし
        rows[5].paced = false; // ゲーム自身のペース
        rows[8].present_begin += p; // 1 フレーム分遅れた Present（長い周期 1 件）
        rows[8].present_end = rows[8].present_begin + 500;
        rows[8].input = rows[8].present_begin - 30_000;
        rows.remove(6); // 番号の飛び（5 → 7）
        for r in &rows {
            s.push(r);
        }
        let sum = s.finish();
        assert_eq!(sum.rows, 9);
        assert_eq!(sum.paced, 8);
        assert_eq!(sum.gaps, 1);
        assert_eq!(sum.no_input, 1);
        let io = sum.input_to_present.unwrap();
        assert_eq!((io.n, io.p50, io.max), (8, 30_000, 30_000));
        assert_eq!(sum.wake_late.unwrap().n, 8);
        let per = sum.period.unwrap();
        assert_eq!(per.n, 7);
        assert_eq!(sum.long_periods, 1);
        assert_eq!(per.max, 2 * p as i64);
        assert_eq!(per.min, 0);
        let fps = sum.fps.unwrap();
        assert!((fps - 60.0).abs() < 0.01, "{fps}");
        let text = sum.lines().join("\n");
        assert!(
            text.contains("フレーム 9 件（うち B で先頭を決めた 8 件）"),
            "{text}"
        );
        assert!(
            text.contains("1.5 周期より長い周期: 1 件（周期 7 件のうち）"),
            "{text}"
        );
        assert!(
            text.contains("入力の地点を通らなかったフレーム: 1 件（9 件のうち）"),
            "{text}"
        );
        assert!(text.contains("QPF 10000000 Hz、ペース 60 fps、"), "{text}");
    }

    #[test]
    fn summary_long_periods_follow_the_fps() {
        let qpf = 10_000_000;
        let fps = 240;
        let p = qpf / fps; // 41,666 カウント
        let limit = qpf * 3 / (2 * fps); // 62,500 カウント
        let mut s = Summarizer::new(qpf, fps);
        let mut t = 1_000_000u64;
        for (f, step) in [0, p, p, p, 2 * p, limit, limit + 1, p]
            .into_iter()
            .enumerate()
        {
            t += step;
            s.push(&FrameRow {
                frame: f as u32,
                present_begin: t,
                present_end: t + 10,
                ..Default::default()
            });
        }
        let sum = s.finish();
        assert_eq!(sum.target_fps, 240);
        assert_eq!(sum.period.unwrap().n, 7);
        assert_eq!(sum.long_periods, 2);
        let text = sum.lines().join("\n");
        assert!(text.contains("ペース 240 fps"), "{text}");
        assert!(
            text.contains("1.5 周期より長い周期: 2 件（周期 7 件のうち）"),
            "{text}"
        );
        assert_eq!(Summarizer::new(qpf, 0).finish().target_fps, DEFAULT_FPS);
    }

    #[test]
    fn mode_from_ini_reads_b_spin_and_cycle() {
        let m = |t: &str| mode_from_ini(&crate::ini::parse(t).unwrap());
        let ini = |mode, fps, cycle_secs| {
            Ok(PacingIni {
                mode,
                fps,
                cycle_secs,
                ..PacingIni::DEFAULT
            })
        };
        assert_eq!(m("[pacing]\nmode = b-spin\n"), ini(Mode::BSpin, 60, 30));
        assert_eq!(
            m("[pacing]\nmode = B-SPIN\nfps = 240\n"),
            ini(Mode::BSpin, 240, 30)
        );
        assert_eq!(m("[pacing]\nmode = cycle\n"), ini(Mode::Cycle, 60, 30));
        assert_eq!(DEFAULT_CYCLE_SECS, 30);
        for (v, want) in [("1", 1u64), ("3600", 3_600), (" 45 ", 45)] {
            assert_eq!(
                m(&format!("[pacing]\nmode = cycle\ncycle_secs = {v}\n")),
                ini(Mode::Cycle, 60, want),
                "{v:?}"
            );
        }
        for bad in ["0", "3601", "+30", "30.0", "", "-1"] {
            let e = m(&format!("[pacing]\nmode = cycle\ncycle_secs = {bad}\n")).unwrap_err();
            assert!(e.contains("1〜3600 の整数"), "{bad:?}: {e}");
        }
        for text in [
            "[pacing]\nmode = b\ncycle_secs = 30\n",
            "[pacing]\nmode = b-spin\ncycle_secs = 30\n",
            "[pacing]\ncycle_secs = 30\n",
        ] {
            let e = m(text).unwrap_err();
            assert!(e.contains("mode = cycle のときだけ書ける"), "{text:?}: {e}");
        }
        let e = m("[pacing]\nmode = cycle\nfps = 240\n").unwrap_err();
        assert!(e.contains("mode = cycle はゲーム自身の 60 fps"), "{e}");
        assert_eq!(
            m("[pacing]\nmode = cycle\nfps = 60\n"),
            ini(Mode::Cycle, 60, 30)
        );
        for mode in [Mode::Game, Mode::B, Mode::BSpin, Mode::Cycle, Mode::Auto] {
            assert_eq!(Mode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(Mode::from_name("bspin"), None);
        assert!(!Mode::Game.uses_head());
        assert!(Mode::B.uses_head() && Mode::BSpin.uses_head() && Mode::Cycle.uses_head());
        assert!(Mode::Auto.uses_head());
        assert_eq!(Mode::Cycle.fixed_phase(), None);
        assert_eq!(Mode::Auto.fixed_phase(), None);
        assert_eq!(Mode::BSpin.fixed_phase(), Some(Phase::BSpin));
        for p in CYCLE_ORDER {
            assert_eq!(Phase::from_code(p.code()), Some(p));
            assert_eq!(Phase::from_name(p.name()), Some(p));
        }
        assert_eq!(Phase::from_code(0), None);
    }

    #[test]
    fn auto_reads_the_screen_flag() {
        assert_eq!(screen_from_flag(0), Screen::Fullscreen);
        assert_eq!(screen_from_flag(1), Screen::Windowed);
        for b in [2u8, 3, 0x80, 0xFF] {
            assert_eq!(screen_from_flag(b), Screen::Unknown, "{b}");
        }
        assert_eq!(auto_phase(Screen::Windowed), Phase::BSpin);
        assert_eq!(auto_phase(Screen::Fullscreen), Phase::Game);
        assert_eq!(auto_phase(Screen::Unknown), Phase::Game);
        let spin: Vec<u8> = (0..=255u8)
            .filter(|&b| auto_phase(screen_from_flag(b)) == Phase::BSpin)
            .collect();
        assert_eq!(spin, vec![1]);
        let pre = Preamble::new(10_000_000, Mode::Auto);
        assert_eq!(csv_preamble(&pre), "# qpf=10000000 mode=auto");
        assert_eq!(parse_preamble(&csv_preamble(&pre)), Some(pre));
    }

    #[test]
    fn cycle_switches_in_order_after_the_length() {
        let qpf = 10_000_000u64;
        let len = qpf * 30;
        let mut c = Cycle::new(qpf, 30).unwrap();
        let t0 = 123_456_789u64;
        assert_eq!(c.at_frame(t0), (Phase::Game, false));
        assert_eq!(c.at_frame(t0 + len - 1), (Phase::Game, false));
        assert_eq!(c.at_frame(t0 + len), (Phase::B, true));
        let t1 = t0 + len;
        assert_eq!(c.at_frame(t1 + len - 1), (Phase::B, false));
        let t2 = t1 + 3 * len;
        assert_eq!(c.at_frame(t2), (Phase::BSpin, true));
        assert_eq!(c.at_frame(t2 + len), (Phase::Game, true));
        assert_eq!(c.switches(), 3);
        assert_eq!(c.phase(), Phase::Game);
        assert!(Cycle::new(qpf, 0).is_none());
        assert!(Cycle::new(0, 30).is_none());
        assert!(Cycle::new(u64::MAX, 2).is_none());
    }

    fn cpu_set(size: usize, ty: u32, group: u16, lpi: u8, eff: u8) -> Vec<u8> {
        let mut b = vec![0u8; size];
        b[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        b[4..8].copy_from_slice(&ty.to_le_bytes());
        if size >= 32 {
            b[8..12].copy_from_slice(&(0x100u32 + lpi as u32).to_le_bytes()); // Id
            b[12..14].copy_from_slice(&group.to_le_bytes());
            b[14] = lpi;
            b[15] = lpi / 2; // CoreIndex（効率クラスの隣。取り違えを見る）
            b[16] = 7; // LastLevelCacheIndex
            b[17] = 9; // NumaNodeIndex
            b[18] = eff;
        }
        b
    }

    #[test]
    fn cpu_sets_are_read_by_size_and_flattened() {
        let mut bytes = Vec::new();
        bytes.extend(cpu_set(32, 0, 0, 5, 0));
        bytes.extend(cpu_set(40, 1, 0, 99, 3));
        bytes.extend(cpu_set(32, 0, 0, 0, 1));
        bytes.extend(cpu_set(48, 0, 1, 0, 1)); // 大きい件（後ろに知らない欄）
        bytes.extend(cpu_set(32, 0, 0, 1, 1));
        let c = parse_cpu_sets(&bytes).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c.class_of(0), Some(1));
        assert_eq!(c.class_of(1), Some(1));
        assert_eq!(c.class_of(5), Some(0));
        assert_eq!(c.class_of(64), Some(1), "群 1 の 0 番");
        assert_eq!(c.class_of(99), None, "知らない種類の件を読んだ");
        assert_eq!(c.class_of(2), None);
        assert_eq!(c.distinct(), vec![0, 1]);
        assert_eq!(flat_cpu(1, 3), 67);
        assert_eq!(
            c.describe(),
            "論理 CPU 4 個: class 0 が 1 個 / class 1 が 3 個"
        );
        assert_eq!(parse_cpu_sets(&[]), Ok(CpuClasses::default()));
        assert!(parse_cpu_sets(&bytes[..bytes.len() - 1]).is_err());
        assert!(parse_cpu_sets(&cpu_set(32, 0, 0, 0, 0)[..6]).is_err());
        let mut small = cpu_set(32, 0, 0, 0, 0);
        small[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(parse_cpu_sets(&small).is_err());
    }

    #[test]
    fn core_kind_follows_the_largest_class() {
        let hybrid = [0u8, 1];
        assert_eq!(core_kind(Some(1), Some(1), &hybrid), CoreKind::Perf);
        assert_eq!(core_kind(Some(0), Some(0), &hybrid), CoreKind::Eff);
        assert_eq!(core_kind(Some(1), Some(0), &hybrid), CoreKind::Mixed);
        assert_eq!(core_kind(Some(0), Some(1), &hybrid), CoreKind::Mixed);
        assert_eq!(core_kind(Some(0), None, &hybrid), CoreKind::Unknown);
        assert_eq!(core_kind(Some(1), Some(1), &[]), CoreKind::Unknown);
        assert_eq!(core_kind(Some(0), Some(0), &[0]), CoreKind::Uniform);
        assert_eq!(core_kind(Some(1), Some(0), &[0, 1, 2]), CoreKind::Eff);
        assert_eq!(core_kind(Some(2), Some(2), &[0, 1, 2]), CoreKind::Perf);
    }

    fn full_row(frame: u32) -> FrameRow {
        FrameRow {
            phase: Some(Phase::BSpin),
            head_cpu: Some(66),
            head_eff: Some(1),
            input_cpu: Some(3),
            input_eff: Some(0),
            present_cpu: Some(12),
            present_eff: Some(1),
            input_cycles: 900_000_000,
            present_cycles: 911_100_000,
            ..sample(frame)
        }
    }

    #[test]
    fn csv_new_columns_roundtrip_and_old_rows_still_read() {
        let r = full_row(7);
        let line = r.csv_line(None, 10_000_000);
        assert_eq!(line.split(',').count(), CSV_HEADER.split(',').count());
        assert_eq!(
            CSV_HEADER.split(',').nth(19),
            Some("phase"),
            "足した列の先頭"
        );
        let old = FrameRow {
            phase: None,
            head_cpu: None,
            head_eff: None,
            input_cpu: None,
            input_eff: None,
            present_cpu: None,
            present_eff: None,
            input_cycles: 0,
            present_cycles: 0,
            ..r
        };
        assert_eq!(old_part(&line), old_part(&old.csv_line(None, 10_000_000)));
        assert!(
            line.ends_with(",b-spin,66,1,3,0,12,1,900000000,911100000,11100000,3700,0,0.0,0,,"),
            "{line}"
        );
        assert_eq!(FrameRow::parse_csv(&line), Ok(Some(r)));
        assert!(
            r.csv_line(None, 3_579_545)
                .ends_with(",11100000,1324,0,0.0,0,,")
        );
        let old_line = old_part(&line);
        assert_eq!(FrameRow::parse_csv(&old_line), Ok(Some(old)));
        let cut: Vec<&str> = line.split(',').take(24).collect();
        assert!(
            FrameRow::parse_csv(&cut.join(","))
                .unwrap_err()
                .contains("列が 24 個")
        );
        assert!(FrameRow::parse_csv(&line.replace(",b-spin,", ",c,")).is_err());
        assert!(FrameRow::parse_csv(&line.replace(",66,1,3,", ",66,256,3,")).is_err());
        let empty = FrameRow {
            phase: Some(Phase::Game),
            input_cycles: 0,
            ..full_row(8)
        };
        let l = empty.csv_line(None, 10_000_000);
        assert!(
            l.ends_with(",game,66,1,3,0,12,1,,911100000,,,0,0.0,0,,"),
            "{l}"
        );
        assert_eq!(FrameRow::parse_csv(&l), Ok(Some(empty)));
    }

    #[test]
    fn csv_glide_columns_roundtrip_and_old_rows_still_read() {
        let hdr: Vec<&str> = CSV_HEADER.split(',').collect();
        assert_eq!(hdr.len(), 35);
        assert_eq!((hdr[30], hdr[31]), ("glide_req_ppm", "glide_moved_us"));
        let mk = |req: i32, moved: i64| FrameRow {
            glide_req_ppm: req,
            glide_moved: moved,
            ..full_row(9)
        };
        for (req, moved, tail) in [
            (10_000, 1_666, ",10000,166.6"),
            (-4_000, -665, ",-4000,-66.5"),
            (0, 0, ",0,0.0"),
            (8_000, 0, ",8000,0.0"),
            (-1, -3, ",-1,-0.3"),
        ] {
            let r = mk(req, moved);
            let line = r.csv_line(None, 10_000_000);
            assert!(line.ends_with(&format!("{tail},0,,")), "{line}");
            assert_eq!(line.split(',').count(), hdr.len(), "見出しと列の数");
            assert_eq!(FrameRow::parse_csv_at(&line, 10_000_000), Ok(Some(r)));
            assert_eq!(FrameRow::parse_csv(&line), Ok(Some(mk(req, 0))), "{line}");
        }
        let r = mk(10_000, 1_666);
        let line = r.csv_line(None, 3_579_545);
        assert!(line.ends_with(",10000,465.4,0,,"), "{line}");
        assert_eq!(FrameRow::parse_csv_at(&line, 3_579_545), Ok(Some(r)));
        let full = mk(10_000, 1_666).csv_line(None, 10_000_000);
        let none = mk(0, 0);
        for n in [19usize, 28, 30] {
            let cut: Vec<&str> = full.split(',').take(n).collect();
            let got = FrameRow::parse_csv_at(&cut.join(","), 10_000_000)
                .unwrap()
                .unwrap();
            assert_eq!((got.glide_req_ppm, got.glide_moved), (0, 0), "{n} 列");
            let want = if n == 19 {
                FrameRow {
                    phase: None,
                    head_cpu: None,
                    head_eff: None,
                    input_cpu: None,
                    input_eff: None,
                    present_cpu: None,
                    present_eff: None,
                    input_cycles: 0,
                    present_cycles: 0,
                    ..none
                }
            } else {
                none
            };
            assert_eq!(got, want, "{n} 列");
        }
        let cut: Vec<&str> = full.split(',').take(31).collect();
        assert!(
            FrameRow::parse_csv(&cut.join(","))
                .unwrap_err()
                .contains("列が 31 個")
        );
        assert!(FrameRow::parse_csv(&full.replace(",10000,", ",1e4,")).is_err());
        assert!(FrameRow::parse_csv_at(&full.replace(",166.6", ",166"), 10_000_000).is_err());
        assert!(FrameRow::parse_csv_at(&full.replace(",166.6", ",x.y"), 10_000_000).is_err());
    }

    #[test]
    fn summary_counts_glide_requests_and_moved_amount() {
        let qpf = 10_000_000;
        let plan = [
            (0, 0),
            (500, 83),
            (1_000, 166),
            (-10_000, -1_666),
            (10_000, 0),
        ];
        let rows: Vec<FrameRow> = plan
            .iter()
            .enumerate()
            .map(|(i, &(req, moved))| FrameRow {
                glide_req_ppm: req,
                glide_moved: moved,
                ..sample(i as u32)
            })
            .collect();
        let mut s = Summarizer::new(qpf, DEFAULT_FPS);
        for r in &rows {
            s.push(r);
        }
        let g = s.finish().glide;
        assert_eq!(
            g,
            GlideTally {
                nonzero: 4,
                counted: 3,
                at_cap: 2,
                max_abs_ppm: 10_000,
                unapplied: 1,
                moved_abs: 83 + 166 + 1_666,
                moved_net: 83 + 166 - 1_666,
            }
        );
        let text = s.finish().lines().join("\n");
        assert!(
            text.contains("要求が 0 でなかった枠 4 件（5 件のうち）・|要求| 1000 ppm 以上 3 件・既定の上限 10000 ppm 以上 2 件・|要求| の最大 10000 ppm・動かさなかった要求 1 件・動かした量の絶対値の合計 0.2 ms・正味 -0.1 ms"),
            "{text}"
        );
        let mut z = Summarizer::new(qpf, DEFAULT_FPS);
        z.push(&sample(0));
        assert!(!z.finish().lines().join("\n").contains("格子の寄せ"));
        let mut text = format!(
            "{}\n{CSV_HEADER}\n",
            csv_preamble(&Preamble::new(qpf, Mode::B))
        );
        let mut prev = None;
        for r in &rows {
            text.push_str(&r.csv_line(prev.as_ref(), qpf));
            text.push('\n');
            prev = Some(*r);
        }
        let (_, rep) = summarize_csv(&text, DEFAULT_FPS, None).unwrap();
        assert_eq!(rep.all.glide, g);
    }

    #[test]
    fn preamble_roundtrip_with_classes_and_cycle() {
        let p = Preamble {
            qpf: 10_000_000,
            mode: Mode::Cycle,
            eff_classes: vec![0, 1],
            cycle_secs: Some(30),
        };
        let line = csv_preamble(&p);
        assert_eq!(
            line,
            "# qpf=10000000 mode=cycle eff_classes=0,1 cycle_secs=30"
        );
        assert_eq!(parse_preamble(&line), Some(p));
        let b = Preamble::new(3_579_545, Mode::BSpin);
        assert_eq!(csv_preamble(&b), "# qpf=3579545 mode=b-spin");
        assert_eq!(parse_preamble(&csv_preamble(&b)), Some(b));
        assert_eq!(
            parse_preamble("# qpf=10000000 mode=b"),
            Some(Preamble::new(10_000_000, Mode::B))
        );
        assert_eq!(parse_preamble("# qpf=1 mode=b eff_classes=0,x"), None);
        assert_eq!(
            parse_preamble("# qpf=1 mode=B"),
            None,
            "CSV の札は小文字だけ"
        );
    }

    fn phased(frame: u32, phase: Option<Phase>, eff: (u8, u8), work: u64) -> FrameRow {
        let begin = 5_000_000 + frame as u64 * 166_667;
        FrameRow {
            frame,
            inputs: 1,
            input: begin - work,
            present_begin: begin,
            present_end: begin + 2_000,
            phase,
            input_cpu: Some(eff.0 as u32),
            present_cpu: Some(eff.1 as u32),
            input_eff: Some(eff.0),
            present_eff: Some(eff.1),
            ..Default::default()
        }
    }

    #[test]
    fn splitter_splits_by_phase_and_core_kind() {
        let qpf = 10_000_000;
        let classes = [0u8, 1];
        let mut s = Splitter::new(qpf, DEFAULT_FPS, &classes, None);
        let plan = [
            (Phase::Game, (1, 1), 3_000),
            (Phase::Game, (1, 1), 3_000),
            (Phase::Game, (1, 1), 3_000),
            (Phase::B, (1, 1), 5_000),
            (Phase::B, (0, 0), 5_000),
            (Phase::B, (1, 1), 5_000),
            (Phase::BSpin, (1, 0), 4_000),
            (Phase::BSpin, (1, 1), 4_000),
            (Phase::Game, (1, 1), 3_000),
            (Phase::Game, (1, 1), 3_000),
        ];
        for (f, (p, e, w)) in plan.iter().enumerate() {
            s.push(&phased(f as u32, Some(*p), *e, *w));
        }
        s.push(&phased(10, None, (1, 1), 9_000));
        let rep = s.finish();
        assert_eq!(rep.all.rows, 11);
        assert_eq!(
            rep.by_phase.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![Phase::Game, Phase::B, Phase::BSpin]
        );
        let g = rep.phase(Phase::Game).unwrap();
        let b = rep.phase(Phase::B).unwrap();
        let bs = rep.phase(Phase::BSpin).unwrap();
        assert_eq!((g.rows, b.rows, bs.rows), (5, 3, 2));
        let io = |s: &Summary| {
            let d = s.input_to_present.unwrap();
            (d.min, d.max)
        };
        assert_eq!(io(g), (3_000, 3_000));
        assert_eq!(io(b), (5_000, 5_000));
        assert_eq!(io(bs), (4_000, 4_000));
        assert_eq!(g.period.unwrap().n, 4);
        assert_eq!(
            g.period.unwrap().max,
            166_667,
            "区間をまたいで長い周期を作った"
        );
        assert_eq!(b.period.unwrap().n, 3);
        assert_eq!(g.fps, None, "分けた集計に fps");
        assert_eq!(rep.core(Phase::B, CoreKind::Perf).unwrap().rows, 2);
        assert_eq!(rep.core(Phase::B, CoreKind::Eff).unwrap().rows, 1);
        assert_eq!(rep.core(Phase::BSpin, CoreKind::Mixed).unwrap().rows, 1);
        assert_eq!(rep.core(Phase::Game, CoreKind::Perf).unwrap().rows, 5);
        assert!(rep.core(Phase::Game, CoreKind::Eff).is_none());
        assert_eq!(rep.all.kinds, [9, 1, 1, 0, 0]);
        assert_eq!((rep.all.cpu_pairs, rep.all.cpu_moved), (11, 1));
        let text = rep.lines().join("\n");
        assert!(
            text.contains("ペースの集計 [b-spin]: フレーム 2 件"),
            "{text}"
        );
        assert!(
            text.contains("ペースの集計 [b / 省電力コア]: フレーム 1 件"),
            "{text}"
        );
        assert!(
            text.contains("コアの種類（入力と Present の前の論理 CPU の効率クラス）: 性能コア 9 / 省電力コア 1 / 途中で入れ替わり 1 / 区別なし 0 / 不明 0（11 件のうち）"),
            "{text}"
        );
        let mut s = Splitter::new(qpf, DEFAULT_FPS, &[], Some(Phase::B));
        for f in 0..3 {
            s.push(&FrameRow {
                phase: None,
                input_cpu: None,
                present_cpu: None,
                input_eff: None,
                present_eff: None,
                ..phased(f, None, (0, 0), 1_000)
            });
        }
        let rep = s.finish();
        assert_eq!(
            rep.phase(Phase::B).unwrap().rows,
            3,
            "札の無い行を固定の方式へ"
        );
        let lines = rep.lines();
        assert_eq!(
            lines,
            rep.all.lines(),
            "1 つの方式・種類の不明だけで分けて書いた"
        );
    }

    #[test]
    fn summarize_csv_keeps_only_the_frame_range() {
        let qpf = 10_000_000;
        let pre = Preamble {
            qpf,
            mode: Mode::Cycle,
            eff_classes: vec![0, 1],
            cycle_secs: Some(30),
        };
        let mut text = format!("{}\n{CSV_HEADER}\n", csv_preamble(&pre));
        let mut prev = None;
        for f in 0..10u32 {
            let r = phased(
                f,
                Some(CYCLE_ORDER[(f / 4) as usize % 3]),
                (1, 1),
                1_000 + f as u64,
            );
            text.push_str(&r.csv_line(prev.as_ref(), qpf));
            text.push('\n');
            prev = Some(r);
        }
        let (p, all) = summarize_csv(&text, DEFAULT_FPS, None).unwrap();
        assert_eq!(p, pre);
        assert_eq!(all.all.rows, 10);
        assert_eq!(all.by_phase.len(), 3);
        let (_, part) = summarize_csv(&text, DEFAULT_FPS, Some((3, 6))).unwrap();
        assert_eq!(part.all.rows, 4);
        let io = part.all.input_to_present.unwrap();
        assert_eq!((io.min, io.max), (1_003, 1_006));
        assert_eq!(
            part.all.period.unwrap().n,
            3,
            "範囲の外の行から周期を付けた"
        );
        assert_eq!(part.phase(Phase::Game).unwrap().rows, 1);
        assert_eq!(part.phase(Phase::B).unwrap().rows, 3);
    }

    #[test]
    fn b_hybrid_is_a_phase_but_not_the_default() {
        assert_eq!(DEFAULT_MODE, Mode::Auto);
        assert_eq!(PacingIni::DEFAULT.auto_window, Phase::BSpin);
        assert_eq!(auto_phase(Screen::Windowed), Phase::BSpin);
        assert_eq!(
            auto_phase_with(Screen::Windowed, Phase::BHybrid),
            Phase::BHybrid
        );
        for s in [Screen::Fullscreen, Screen::Unknown] {
            assert_eq!(auto_phase_with(s, Phase::BHybrid), Phase::Game);
        }
        assert!(!PacingIni::DEFAULT.uses_hybrid());
        assert_eq!(
            ALL_PHASES.map(Phase::code),
            [1, 2, 3, 4, 5],
            "既存の方式の値は変えない（vpatch は 5 を足しただけ）"
        );
        for p in ALL_PHASES {
            assert_eq!(Phase::from_name(p.name()), Some(p));
            assert_eq!(Phase::from_code(p.code()), Some(p));
        }
        assert_eq!(Phase::from_code(0), None);
        assert_eq!(Mode::from_name("B-Hybrid"), Some(Mode::BHybrid));
        assert_eq!(Mode::BHybrid.fixed_phase(), Some(Phase::BHybrid));
        assert!(Mode::BHybrid.uses_head());
        assert_eq!(CyclePhases::DEFAULT.as_slice(), &CYCLE_ORDER[..]);
    }

    // 何が壊れたら落ちるか: `mode = vpatch` が読めること（名前の表から外すと落ちる）、読めても既定は auto のままで vpatch が候補に紛れ込まないこと、
    // fps は効くこと、cycle / auto_window には書けないこと、CSV の札として読み戻せること。
    #[test]
    fn mode_vpatch_is_read_but_stays_out_of_defaults_and_candidates() {
        let m = |t: &str| mode_from_ini(&crate::ini::parse(t).unwrap());
        let c = m("[pacing]\nmode = vpatch\n").unwrap();
        assert_eq!((c.mode, c.fps), (Mode::Vpatch, 60));
        assert_eq!(Mode::from_name("VPatch"), Some(Mode::Vpatch));
        assert_eq!(m("[pacing]\nmode = vpatch\nfps = 240\n").unwrap().fps, 240);
        assert!(Mode::Vpatch.uses_head());
        assert_eq!(Mode::Vpatch.fixed_phase(), Some(Phase::Vpatch));
        assert!(!c.uses_hybrid());
        assert_eq!(PacingIni::DEFAULT.mode, Mode::Auto);
        assert_eq!(DEFAULT_MODE, Mode::Auto);
        assert!(!AUTO_WINDOW_PHASES.contains(&Phase::Vpatch));
        assert!(!CYCLE_ORDER.contains(&Phase::Vpatch));
        let e = m("[pacing]\nmode = cycle\ncycle_phases = b-spin, vpatch\n").unwrap_err();
        assert!(e.contains("cycle_phases の \"vpatch\""), "{e}");
        let e = m("[pacing]\nauto_window = vpatch\n").unwrap_err();
        assert!(e.contains("auto_window の値"), "{e}");
        let e = m("[pacing]\nmode = vpatch\ncycle_secs = 30\n").unwrap_err();
        assert!(e.contains("cycle_secs は mode = cycle"), "{e}");
        assert_eq!(Phase::from_name("vpatch"), Some(Phase::Vpatch));
        assert_eq!(Phase::from_code(5), Some(Phase::Vpatch));
    }

    #[test]
    fn b_hybrid_keys_are_read_only_where_they_work() {
        use crate::pacing_hybrid::{DEFAULT_MARGIN_US, HYBRID_KEYS, HybridParams};
        let m = |t: &str| mode_from_ini(&crate::ini::parse(t).unwrap());
        let ok = |t: &str| m(t).unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let err = |t: &str, want: &str| {
            let e = m(t).unwrap_err();
            assert!(e.contains(want), "{t:?}: {e}");
        };
        let c = ok("[pacing]\nmode = b-hybrid\n");
        assert_eq!((c.mode, c.fps), (Mode::BHybrid, 60));
        assert_eq!(c.hybrid, HybridParams::DEFAULT);
        assert_eq!(c.hybrid.margin_us, DEFAULT_MARGIN_US);
        assert!(c.uses_hybrid());
        let c = ok("[pacing]\nmode = b-hybrid\nfps = 240\nhybrid_margin_us = 3000\n");
        assert_eq!((c.fps, c.hybrid.margin_us), (240, 3_000));
        let c = ok(
            "[pacing]\nmode = b-hybrid\nhybrid_margin_us = 1500\nhybrid_margin_max_us = 6000\nhybrid_guard_us = 400\nhybrid_decay_shift = 8\nhybrid_chunk_us = 500\nhybrid_spin_us = 1200\nhybrid_fallback_count = 3\nhybrid_fallback_frames = 120\n",
        );
        assert_eq!(
            c.hybrid,
            HybridParams {
                margin_us: 1_500,
                margin_max_us: 6_000,
                guard_us: 400,
                decay_shift: 8,
                chunk_us: 500,
                spin_us: 1_200,
                fallback_count: 3,
                fallback_frames: 120,
            }
        );
        assert_eq!(
            c.hybrid.text(),
            "★hybrid_margin_us = 1500、★hybrid_margin_max_us = 6000、★hybrid_guard_us = 400、★hybrid_decay_shift = 8、★hybrid_chunk_us = 500、★hybrid_spin_us = 1200、★hybrid_fallback_count = 3、★hybrid_fallback_frames = 120"
        );
        assert!(!HybridParams::DEFAULT.text().contains('★'));
        for k in &HYBRID_KEYS {
            let other = match k.name {
                "hybrid_margin_us" => "hybrid_margin_max_us = 16000\n",
                "hybrid_margin_max_us" => "hybrid_margin_us = 0\n",
                _ => "",
            };
            for v in [k.min, k.max] {
                let c = ok(&format!(
                    "[pacing]\nmode = b-hybrid\n{other}{} = {v}\n",
                    k.name
                ));
                let mut want = c.hybrid;
                assert_eq!(want.set(k.name, &v.to_string()), Ok(true));
                assert_eq!(c.hybrid, want, "{} = {v}", k.name);
            }
            let mut bad = vec![
                "".to_string(),
                "+1".to_string(),
                "1.0".to_string(),
                "x".to_string(),
            ];
            if k.min > 0 {
                bad.push((k.min - 1).to_string());
            }
            bad.push((u64::from(k.max) + 1).to_string());
            for v in bad {
                err(
                    &format!("[pacing]\nmode = b-hybrid\n{} = {v}\n", k.name),
                    &format!("{} の値", k.name),
                );
            }
        }
        err(
            "[pacing]\nmode = b-hybrid\nhybrid_margin_us = 9000\n",
            "hybrid_margin_us = 9000 が hybrid_margin_max_us = 8000 より大きい",
        );
        let c = ok("[pacing]\nauto_window = b-hybrid\n");
        assert_eq!((c.mode, c.auto_window), (Mode::Auto, Phase::BHybrid));
        assert!(c.uses_hybrid());
        let c = ok("[pacing]\nmode = auto\nauto_window = b-hybrid\nhybrid_margin_us = 4000\n");
        assert_eq!(c.hybrid.margin_us, 4_000);
        assert_eq!(
            ok("[pacing]\nauto_window = b-spin\n").auto_window,
            Phase::BSpin
        );
        for v in ["b", "game", "B-HYBRID", "hybrid"] {
            err(
                &format!("[pacing]\nauto_window = {v}\n"),
                "auto_window の値",
            );
        }
        let c = ok("[pacing]\nmode = cycle\ncycle_phases = b-spin, b-hybrid\n");
        assert_eq!(c.cycle_phases.as_slice(), &[Phase::BSpin, Phase::BHybrid]);
        assert!(c.uses_hybrid());
        let c = ok(
            "[pacing]\nmode = cycle\ncycle_secs = 20\ncycle_phases = game,b-hybrid,b-spin\nhybrid_margin_us = 1500\n",
        );
        assert_eq!(c.cycle_phases.text(), "game → b-hybrid → b-spin");
        assert_eq!((c.cycle_secs, c.hybrid.margin_us), (20, 1_500));
        for v in [
            "b-spin",
            "b-spin,b-spin",
            "b-spin,x",
            "",
            "game,b,b-spin,b-hybrid,game",
        ] {
            err(
                &format!("[pacing]\nmode = cycle\ncycle_phases = {v}\n"),
                "cycle_phases の",
            );
        }
        for k in &HYBRID_KEYS {
            for head in [
                "[pacing]\nmode = b-spin\n",
                "[pacing]\n",
                "[pacing]\nmode = cycle\n",
            ] {
                err(
                    &format!("{head}{} = {}\n", k.name, k.default),
                    &format!("{} は b-hybrid のフレームがある形", k.name),
                );
            }
        }
        for (t, want) in [
            (
                "[pacing]\nmode = b-hybrid\nauto_window = b-hybrid\n",
                "auto_window は mode = auto のときだけ書ける",
            ),
            (
                "[pacing]\nmode = b\ncycle_phases = b, b-spin\n",
                "cycle_phases は mode = cycle のときだけ書ける",
            ),
            (
                "[pacing]\nmode = b-hybrid\ncycle_secs = 30\n",
                "cycle_secs は mode = cycle のときだけ書ける",
            ),
            (
                "[pacing]\nauto_window = b-hybrid\nfps = 240\n",
                "mode = b / b-spin / b-hybrid / vpatch のときだけ効く",
            ),
        ] {
            err(t, want);
        }
        err(
            "[pacing]\nmode = b-hybrid\nhybrid_margin = 2000\n",
            "を知らない（読めるのは mode / fps / cycle_secs / cycle_phases / auto_window / hybrid_margin_us /",
        );
    }

    #[test]
    fn cycle_follows_the_given_phases() {
        let qpf = 1_000;
        let two = CyclePhases::new(&[Phase::BSpin, Phase::BHybrid]).unwrap();
        let mut c = Cycle::with_phases(qpf, 2, two).unwrap();
        let mut seen = Vec::new();
        for t in (0..12_000).step_by(500) {
            let (p, switched) = c.at_frame(t);
            if switched || seen.is_empty() {
                seen.push(p);
            }
        }
        assert_eq!(
            seen,
            [
                Phase::BSpin,
                Phase::BHybrid,
                Phase::BSpin,
                Phase::BHybrid,
                Phase::BSpin,
                Phase::BHybrid
            ]
        );
        let mut d = Cycle::new(qpf, 1).unwrap();
        let mut seen = vec![d.at_frame(0).0];
        for t in (1_000..4_000).step_by(1_000) {
            seen.push(d.at_frame(t).0);
        }
        assert_eq!(seen, [Phase::Game, Phase::B, Phase::BSpin, Phase::Game]);
        assert!(CyclePhases::new(&[Phase::BSpin]).is_none());
        assert!(CyclePhases::new(&[Phase::B, Phase::B]).is_none());
    }

    #[test]
    fn csv_hybrid_columns_roundtrip_and_old_rows_still_read() {
        let hdr: Vec<&str> = CSV_HEADER.split(',').collect();
        assert_eq!(hdr.len(), 35);
        assert_eq!(
            (hdr[32], hdr[33], hdr[34]),
            ("hybrid_margin_us", "oversleep_us", "frame_cycles")
        );
        let mk = |frame: u32, margin: u32, over: Option<u32>, cycles: u64| FrameRow {
            phase: Some(Phase::BHybrid),
            hybrid_margin_us: margin,
            oversleep_us: over,
            present_cycles: cycles,
            ..full_row(frame)
        };
        let prev = mk(8, 0, None, 900_000_000);
        for (r, tail) in [
            (
                mk(9, 2_250, Some(1_234), 950_000_000),
                ",2250,1234,50000000",
            ),
            (mk(9, 2_000, Some(0), 950_000_000), ",2000,0,50000000"),
            (mk(9, 0, None, 950_000_000), ",0,,50000000"),
            (mk(10, 2_000, None, 950_000_000), ",2000,,"),
        ] {
            let line = r.csv_line(Some(&prev), 10_000_000);
            assert!(line.ends_with(tail), "{line}");
            assert_eq!(line.split(',').count(), hdr.len(), "見出しと列の数");
            assert_eq!(FrameRow::parse_csv_at(&line, 10_000_000), Ok(Some(r)));
        }
        let no_cycles = mk(9, 0, None, 0).csv_line(Some(&prev), 10_000_000);
        assert!(no_cycles.ends_with(",0,,"), "{no_cycles}");
        let full = mk(9, 2_250, Some(1_234), 950_000_000).csv_line(Some(&prev), 10_000_000);
        let cut: Vec<&str> = full.split(',').take(32).collect();
        let got = FrameRow::parse_csv_at(&cut.join(","), 10_000_000)
            .unwrap()
            .unwrap();
        assert_eq!((got.hybrid_margin_us, got.oversleep_us), (0, None));
        assert_eq!(got, mk(9, 0, None, 950_000_000));
        for n in [33usize, 34] {
            let cut: Vec<&str> = full.split(',').take(n).collect();
            let e = FrameRow::parse_csv(&cut.join(",")).unwrap_err();
            assert!(e.contains(&format!("列が {n} 個")), "{e}");
        }
        assert!(FrameRow::parse_csv(&full.replace(",2250,1234,", ",2250,-1,")).is_err());
        assert!(FrameRow::parse_csv(&full.replace(",2250,1234,", ",x,1234,")).is_err());
    }

    #[test]
    fn summary_reports_oversleep_late_and_cpu_by_phase() {
        let qpf = 10_000_000u64;
        let period = 166_667u64;
        let mut rows = Vec::new();
        let mut cycles = 1_000_000_000u64;
        for i in 0..40u32 {
            let hybrid = i >= 20;
            let used = if hybrid { 10_000_020 } else { 50_000_100 };
            cycles += used;
            let begin = 10_000_000 + u64::from(i) * period;
            rows.push(FrameRow {
                frame: i,
                paced: true,
                deadline: begin - 30_000,
                start: begin - 30_000,
                head_calls: 5,
                sleeps: if hybrid { 4 } else { 0 },
                late: hybrid && i % 10 == 0,
                inputs: 1,
                input: begin - 30_000,
                present_begin: begin,
                present_end: begin + 2_000,
                phase: Some(if hybrid { Phase::BHybrid } else { Phase::BSpin }),
                input_cycles: cycles - 9_000_000,
                present_cycles: cycles,
                hybrid_margin_us: if hybrid { 2_000 } else { 0 },
                oversleep_us: hybrid.then_some(100 + i),
                ..Default::default()
            });
        }
        let mut s = Splitter::new(qpf, 60, &[], None);
        for r in &rows {
            s.push(r);
        }
        let rep = s.finish();
        let spin = rep.phase(Phase::BSpin).unwrap();
        let hyb = rep.phase(Phase::BHybrid).unwrap();
        let busy = |x: &Summary| x.hybrid.busy.unwrap();
        assert!((busy(spin) - 1.0).abs() < 0.01, "{}", busy(spin));
        assert!((busy(hyb) - 0.2).abs() < 0.01, "{}", busy(hyb));
        let o = hyb.hybrid.oversleep.unwrap();
        assert_eq!((o.n, o.max), (20, 139));
        assert_eq!(hyb.hybrid.late_after_sleep, 2);
        assert_eq!(hyb.hybrid.margin.unwrap().n, 20);
        assert!(spin.hybrid.oversleep.is_none() && spin.hybrid.margin.is_none());
        let text = rep.lines().join("\n");
        assert!(text.contains("使用率の見込み 20.0%"), "{text}");
        assert!(
            text.contains("寝た枠のうち遅れて先頭へ来た 2 件（寝た枠 20 件のうち）"),
            "{text}"
        );
        assert!(
            text.contains("b-hybrid の余裕（1 枠の中の最大）: n = 20"),
            "{text}"
        );
        let mut old = Summarizer::new(qpf, 60);
        for r in &rows {
            old.push(&FrameRow {
                hybrid_margin_us: 0,
                oversleep_us: None,
                present_cycles: 0,
                ..*r
            });
        }
        let text = old.finish().lines().join("\n");
        assert!(
            !text.contains("寝過ごし") && !text.contains("使用率"),
            "{text}"
        );
    }
    /// 検査用の並び: 0..n の並べ替え（k = 1,000,003 は素数で、n が k の倍数でなければ i ↦ i·k mod n が全単射）を、負の値も混ぜるため n/2 だけずらしたもの。
    fn shuffled(n: u64) -> Vec<i64> {
        (0..n)
            .map(|i| (i * 1_000_003 % n) as i64 - (n / 2) as i64)
            .collect()
    }

    #[test]
    fn series_below_the_cap_is_identical_to_dist_of() {
        const { assert!(SERIES_CAP >= 75_600, "1260 秒 × 60 fps を間引かない") };
        for n in [0u64, 1, 2, 3, 63, 64] {
            let v = shuffled(n.max(1))[..n as usize].to_vec();
            let mut s = Series::with_cap(64);
            v.iter().for_each(|&x| s.push(x));
            assert_eq!(s.dist(), Dist::of(v.clone()), "n = {n}");
            assert!(!s.is_thinned(), "n = {n}");
            assert_eq!(s.samples.len() as u64, n);
        }
        let v = shuffled(75_600);
        let mut s = Series::default();
        v.iter().for_each(|&x| s.push(x));
        assert!(!s.is_thinned());
        assert_eq!(s.dist(), Dist::of(v));
        let mut s = Series::with_cap(64);
        (0..65).for_each(|x| s.push(x));
        assert_eq!((s.stride, s.samples.len(), s.n), (2, 33, 65));
    }

    #[test]
    fn series_over_the_cap_is_bounded_exact_in_counts_and_close_in_quantiles() {
        const CAP: usize = 4096;
        let n = (CAP * 10 + 7) as u64;
        let mut shapes: Vec<(&str, Vec<i64>)> = vec![
            ("並べ替え", shuffled(n)),
            ("単調", (0..n as i64).collect()),
            ("鋸歯", (0..n as i64).map(|i| (i % 1_000) * 40).collect()),
        ];
        let mut spiky = shuffled(n);
        spiky[3] = 1_000_000_000;
        spiky[n as usize - 2] = -1_000_000_000;
        shapes.push(("とがり", spiky));
        for (name, v) in shapes {
            let mut s = Series::with_cap(CAP);
            v.iter().for_each(|&x| s.push(x));
            let mut again = Series::with_cap(CAP);
            v.iter().for_each(|&x| again.push(x));
            assert_eq!(s.samples, again.samples, "{name}: 同じ入力なら同じ標本");
            assert!(s.samples.len() <= CAP, "{name}: {}", s.samples.len());
            assert!(s.stride.is_power_of_two() && s.stride > 1, "{name}");
            assert_eq!(s.samples.len() as u64, n.div_ceil(s.stride), "{name}");
            let want = Dist::of(v.clone()).unwrap();
            let got = s.dist().unwrap();
            assert_eq!(
                (got.n, got.min, got.max, got.mean),
                (want.n, want.min, want.max, want.mean),
                "{name}: 件数・最小・最大・平均は全件で正確"
            );
            if name == "とがり" {
                assert_eq!((got.min, got.max), (-1_000_000_000, 1_000_000_000));
                continue;
            }
            let width = (want.max - want.min) / 100;
            for (g, w, p) in [
                (got.p50, want.p50, "p50"),
                (got.p95, want.p95, "p95"),
                (got.p99, want.p99, "p99"),
            ] {
                assert!(
                    (g - w).abs() <= width,
                    "{name} {p}: {g} と {w}（幅 {width}）"
                );
            }
        }
    }

    #[test]
    fn summarizer_thinning_keeps_counts_and_long_periods_exact_and_says_so() {
        let qpf = 10_000_000;
        let (p, long) = (166_667u64, 300_000u64); // 1.5 周期 = 250,000 カウントより長いのは 300,000 の方
        let run = |rows: u32| {
            let mut s = Summarizer::new(qpf, DEFAULT_FPS).with_series_cap(256);
            let mut t = 2_000_000u64;
            let mut longs = 0usize;
            for f in 0..rows {
                if f > 0 {
                    let is_long = f % 7 == 0;
                    longs += usize::from(is_long);
                    t += if is_long { long } else { p };
                }
                let mut r = sample(f);
                r.present_begin = t;
                r.present_end = t + 500;
                r.input = t - 30_000;
                s.push(&r);
            }
            (s.finish(), longs)
        };
        let (sum, longs) = run(3_000);
        assert_eq!(sum.rows, 3_000);
        let per = sum.period.unwrap();
        assert_eq!((per.n, per.min, per.max), (2_999, p as i64, long as i64));
        assert_eq!(sum.long_periods, longs, "長い周期は全件で数える");
        assert!(
            longs > 256,
            "標本の上限より多く、標本から数え直すと食い違う"
        );
        assert_eq!((per.p50, per.p95), (p as i64, long as i64));
        assert_eq!(sum.input_to_present.unwrap().n, 3_000);
        assert_eq!(sum.thinned[0], ("周期", 16, 188, 2_999));
        let text = sum.lines().join("\n");
        assert!(text.contains("分位数は近似"), "{text}");
        assert!(
            text.contains("周期 16 件に 1 件（標本 188 件 / 全 2999 件）"),
            "{text}"
        );
        assert!(
            text.contains(&format!("列が上限 {SERIES_CAP} 件を越えた")),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "1.5 周期より長い周期: {longs} 件（周期 2999 件のうち）"
            )),
            "{text}"
        );
        let (sum, _) = run(100);
        assert!(sum.thinned.is_empty());
        assert!(!sum.lines().join("\n").contains("近似"));
    }

    #[test]
    fn default_cap_leaves_a_script_run_alone_and_bounds_a_long_run() {
        let qpf = 10_000_000;
        let run = |rows: u32| {
            let mut s = Summarizer::new(qpf, DEFAULT_FPS);
            for f in 0..rows {
                let mut r = sample(f);
                r.present_begin = 2_000_000 + f as u64 * 166_667;
                r.present_end = r.present_begin + 500;
                r.input = r.present_begin - 30_000;
                s.push(&r);
            }
            let sum = s.finish();
            let samples = (s.periods.samples.len(), s.in_to_begin.samples.len());
            (sum, samples)
        };
        let (sum, samples) = run(75_600);
        assert!(sum.thinned.is_empty(), "{:?}", sum.thinned);
        assert_eq!(samples, (75_599, 75_600));
        assert_eq!(sum.period.unwrap().n, 75_599);
        let (sum, samples) = run(SERIES_CAP as u32 + 1_000);
        assert_eq!(sum.rows, SERIES_CAP + 1_000);
        assert_eq!(sum.input_to_present.unwrap().n, SERIES_CAP + 1_000);
        assert!(
            samples.0 <= SERIES_CAP && samples.1 <= SERIES_CAP,
            "{samples:?}"
        );
        let names: Vec<&str> = sum.thinned.iter().map(|t| t.0).collect();
        assert_eq!(
            names,
            [
                "周期",
                "起床の遅れ",
                "入力 → Present の前",
                "入力 → Present の後",
                "Present の長さ"
            ]
        );
    }
}
