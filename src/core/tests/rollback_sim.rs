//! rollback の状態機械（`src/core/src/rollback.rs`）の検査。
//!
//! - **偽のゲーム**（`FakeGame`）: 入力の履歴で決まる状態（ハッシュの鎖・ライフ・一時停止）、コピー 1 枠と書き戻し、
//!   「ライフが 0 になったら次の場面の要求が立ち、更新の列が変わる」場面の境目、一時停止（走査は進まないが入力は読む tick）、
//!   状態の外の欄（つなぎ・場面の要求。書き戻しで戻す）、戻せない副作用、進め直しの間の出力（SE の数）。
//! - **偽のネットワーク**（`run`）: 1 ms 刻みの時計、片道の遅れ、決め打ちの列と固定 seed の乱れ（落とす・遅らせる・重ねる）。
//!   送るものは wire の encode → decode を通す。両端に rollback の状態機械 ＋ 偽のゲーム（片端を delay 方式にした形も）。
//! - **参照**（`reference`）: 同じ入力を本物の `Lockstep`（delay 方式）で回した実行。合格は、rollback の両端が流した確定の
//!   (seg, frame, p1, p2) の列と、30 フレームごとの確定の始まりの状態（CHECK の値）が参照と一致すること。
//!
//! 期待値は参照の実行から取る（rollback の式から作らない）。式の検査（⑤）は、ゲームの側が数えた更新・コピー・書き戻しの回数と、
//! 状態機械の `stats()` と、参照の seg の長さを突き合わせる。

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::check::is_check_frame;
use higanr_core::delay_pick::Recommend;
use higanr_core::liveness::{RTT_WINDOW, median_ms};
use higanr_core::lockstep::DelayPress;
use higanr_core::lockstep::{CaptureError, Lockstep, RecvOutcome, Role, by_seat};
use higanr_core::rollback::{
    After, CheckValues, Config, D_SWITCH_LOG, GlideParams, Mode, Rollback, SEG_D_LOG, SWITCH_LEAD,
    SYNC_GAP, SYNC_UNIT, Stats, Step, Tick, TickKind, TickReport, Wait, Window,
};
use higanr_core::wire::{self, Check, Input, LatChange, MAX_DATAGRAM, Message, Seat};

// ---------------------------------------------------------------- 偽のゲーム

const LIFE: u8 = 6;
/// 一時停止の押下（どちらの語のこのビットでも。押した瞬間に切り替わる）。
const PAUSE_BIT: u16 = 0x4000;

fn mix(h: u64, x: u64) -> u64 {
    let mut z = h ^ x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct World {
    chain: u64,
    life: [u8; 2],
    ticks: u32,
    paused: bool,
    pause_held: bool,
    paused_ticks: u32,
}

/// コピーの範囲の外の欄（実物のつなぎ `0x4ACFC8`〜と Supervisor の `+0x58C` / `+0x590` に当たる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outside {
    /// 更新の列の種類（要求が立つと変わる）。
    link: u32,
    /// 次の場面の要求。
    req: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sabotage {
    None,
    SkipRestore,
    /// つなぎと場面の要求を戻さない（範囲の中だけ戻す）。
    SkipOutside,
}

struct FakeGame {
    /// 場面の始まりのライフ（既定は `LIFE`）。
    life: u8,
    world: World,
    outside: Outside,
    snap: Option<(u32, World, Outside)>,
    seg: u16,
    sabotage: Sabotage,
    se_count: u32,
    se_saved: u32,
    /// 場面の要求が立ったまま更新が走った数（実物では場面の切替が走る。戻せない副作用）。
    stray_transitions: u32,
    tick_counts: [u32; 3],
    saves: u32,
    restores: u32,
    bad_restore: u32,
    confirmed: Vec<(u16, u32, u16, u16)>,
    checks: Vec<Check>,
}

impl FakeGame {
    fn with_life(sabotage: Sabotage, life: u8) -> Self {
        let mut g = FakeGame {
            life,
            world: fresh_world(0, life),
            outside: Outside {
                link: 0,
                req: false,
            },
            snap: None,
            seg: 0,
            sabotage,
            se_count: 0,
            se_saved: 0,
            stray_transitions: 0,
            tick_counts: [0; 3],
            saves: 0,
            restores: 0,
            bad_restore: 0,
            confirmed: Vec::new(),
            checks: Vec::new(),
        };
        g.new_scene(0);
        g
    }

    fn new_scene(&mut self, seg: u16) {
        self.seg = seg;
        self.world = fresh_world(seg, self.life);
        self.outside = Outside {
            link: 0,
            req: false,
        };
        self.snap = None;
    }

    fn digest(&self) -> u64 {
        let w = &self.world;
        let mut h = mix(w.chain, u64::from(w.ticks));
        h = mix(h, u64::from(w.life[0]) | u64::from(w.life[1]) << 8);
        h = mix(h, u64::from(w.paused) | u64::from(w.pause_held) << 1);
        h = mix(h, u64::from(w.paused_ticks));
        h = mix(
            h,
            u64::from(self.outside.link) | u64::from(self.outside.req) << 32,
        );
        h
    }
}

fn fresh_world(seg: u16, life: u8) -> World {
    World {
        chain: mix(0x5EED_0000, u64::from(seg)),
        life: [life; 2],
        ticks: 0,
        paused: false,
        pause_held: false,
        paused_ticks: 0,
    }
}

impl Window for FakeGame {
    fn save(&mut self, frame: u32) {
        self.saves += 1;
        self.snap = Some((frame, self.world, self.outside));
    }

    fn restore(&mut self, frame: u32) {
        self.restores += 1;
        let Some((f, w, o)) = self.snap else {
            self.bad_restore += 1;
            return;
        };
        if f != frame {
            self.bad_restore += 1;
        }
        match self.sabotage {
            Sabotage::None => {
                self.world = w;
                self.outside = o;
            }
            Sabotage::SkipRestore => {}
            Sabotage::SkipOutside => self.world = w,
        }
    }

    fn tick(&mut self, t: Tick) -> TickReport {
        let k = match t.kind {
            TickKind::Live => 0,
            TickKind::Confirmed => 1,
            TickKind::Predicted => 2,
        };
        self.tick_counts[k] += 1;
        if self.outside.req {
            // 場面の要求が立ったまま更新が走った: 実物ではここで場面が切り替わる（戻せない）。
            self.stray_transitions += 1;
            self.outside.req = false;
            self.world.chain = mix(self.world.chain, 0xDEAD);
        }
        // 一時停止: 押した瞬間に切り替わる。止まっている間は世界を進めない（走査が 1 を返す）が、入力は読む。
        let held = (t.p1 | t.p2) & PAUSE_BIT != 0;
        if held && !self.world.pause_held {
            self.world.paused = !self.world.paused;
        }
        self.world.pause_held = held;
        if self.world.paused {
            self.world.paused_ticks += 1;
            return TickReport::default();
        }
        let w = &mut self.world;
        w.ticks += 1;
        w.chain = mix(
            mix(mix(w.chain, u64::from(t.p1)), u64::from(t.p2)),
            u64::from(self.outside.link),
        );
        if (t.p1 ^ t.p2) & 1 != 0 {
            self.se_count += 1;
        }
        if t.p1 & 1 != 0 && w.chain & 7 == 0 {
            w.life[1] = w.life[1].saturating_sub(1);
        }
        if t.p2 & 2 != 0 && (w.chain >> 3) & 7 == 0 {
            w.life[0] = w.life[0].saturating_sub(1);
        }
        if self.outside.link == 0 && (w.life[0] == 0 || w.life[1] == 0) {
            self.outside.req = true;
            self.outside.link = 1;
            return TickReport { boundary: true };
        }
        TickReport::default()
    }

    fn check_values(&mut self) -> CheckValues {
        let d = self.digest();
        CheckValues {
            rng_state: d as u16,
            rng_count: (d >> 16) as u32,
            life_1p: self.world.life[0],
            life_2p: self.world.life[1],
        }
    }

    fn resim_begin(&mut self) {
        self.se_saved = self.se_count;
    }

    fn resim_end(&mut self) {
        self.se_count = self.se_saved;
    }

    fn confirmed(&mut self, frame: u32, p1: u16, p2: u16) {
        self.confirmed.push((self.seg, frame, p1, p2));
    }

    fn check(&mut self, c: &Check) {
        self.checks.push(*c);
    }
}

/// 台 `node` が seg `seg` の capture_frame `c` に取る語（台本）。5 フレームほど同じ語が続く（人の入力に近い形）。
fn word(node: usize, seg: u16, c: u32) -> u16 {
    let block = u64::from(c + 2 * node as u32) / 5;
    let z = mix(mix(node as u64 + 1, u64::from(seg)), block);
    let mut w = (z & 0x00FF) as u16;
    if (z >> 20).is_multiple_of(8) {
        w |= PAUSE_BIT;
    }
    w
}

/// 台 0 = 席 2P、台 1 = 席 1P（席と台の番号をわざと入れ違いにする）。
const SEATS: [Seat; 2] = [Seat::P2, Seat::P1];
const ROLES: [Role; 2] = [Role::Host, Role::Guest];

// ---------------------------------------------------------------- 参照（delay 方式）

struct RefRun {
    confirmed: Vec<(u16, u32, u16, u16)>,
    checks: Vec<Check>,
    seg_len: Vec<u32>,
    paused_ticks: u32,
}

/// 本物の `Lockstep` を 2 台、ネットワークなし（即時に届く）で回す。同じ入力・同じ偽のゲーム。D は全部の seg で `delay`。
fn reference(delay: u8, segs: u32) -> RefRun {
    reference_seq(&vec![delay; segs as usize])
}

fn reference_life(delay: u8, segs: u32, life: u8) -> RefRun {
    reference_seq_life(&vec![delay; segs as usize], life)
}

/// seg ごとの D（`delays[k]` が seg k の D）で回す参照。D は seg の始まりで決まり途中では変わらないので、seg ごとに `Lockstep` を作り直す。
fn reference_seq(delays: &[u8]) -> RefRun {
    reference_seq_life(delays, LIFE)
}

fn reference_seq_life(delays: &[u8], life: u8) -> RefRun {
    let segs = delays.len() as u32;
    let mut g = [
        FakeGame::with_life(Sabotage::None, life),
        FakeGame::with_life(Sabotage::None, life),
    ];
    let mut seg_len = Vec::new();
    let mut paused_ticks = 0;
    for seg_idx in 0..segs {
        let seg = seg_idx as u16;
        let d = delays[seg_idx as usize];
        let mut ls = [Lockstep::new(ROLES[0], d), Lockstep::new(ROLES[1], d)];
        for l in &mut ls {
            l.begin_seg(seg);
        }
        for gm in &mut g {
            gm.new_scene(seg);
        }
        for _guard in 0..100_000 {
            for (i, l) in ls.iter_mut().enumerate() {
                let f = l.frame();
                l.capture(word(i, seg, f)).expect("参照は取得を断られない");
            }
            let a = ls[0].build_input();
            let b = ls[1].build_input();
            assert!(matches!(ls[1].on_input(&a), RecvOutcome::Accepted { .. }));
            assert!(matches!(ls[0].on_input(&b), RecvOutcome::Accepted { .. }));
            let mut boundary = [false; 2];
            let mut frame = 0;
            for i in 0..2 {
                let ap = ls[i].advance().expect("参照は毎フレーム揃う");
                let (p1, p2) = by_seat(SEATS[i], ap.local, ap.remote);
                frame = ap.frame;
                if is_check_frame(frame) {
                    let v = g[i].check_values();
                    g[i].check(&Check {
                        seg,
                        frame,
                        rng_state: v.rng_state,
                        rng_count: v.rng_count,
                        life_1p: v.life_1p,
                        life_2p: v.life_2p,
                    });
                }
                let rep = g[i].tick(Tick {
                    frame,
                    p1,
                    p2,
                    kind: TickKind::Live,
                });
                g[i].confirmed(frame, p1, p2);
                boundary[i] = rep.boundary;
            }
            assert_eq!(boundary[0], boundary[1], "参照の 2 台の境目が食い違った");
            if boundary[0] {
                seg_len.push(frame + 1);
                break;
            }
        }
        assert_eq!(seg_len.len() as u32, seg_idx + 1, "参照の seg が終わらない");
        paused_ticks += g[0].world.paused_ticks;
    }
    assert_eq!(
        g[0].confirmed, g[1].confirmed,
        "参照の 2 台の列が食い違った"
    );
    assert_eq!(g[0].checks, g[1].checks, "参照の 2 台の CHECK が食い違った");
    assert_eq!(g[0].stray_transitions, 0);
    RefRun {
        confirmed: std::mem::take(&mut g[0].confirmed),
        checks: std::mem::take(&mut g[0].checks),
        seg_len,
        paused_ticks,
    }
}

// ---------------------------------------------------------------- 偽のネットワーク

fn net_key() -> AuthKey {
    AuthKey::derive(&Secret([0x7E; 16]), 0x5EED)
}

/// 偽のネットワークの向きの鍵（封じた形だけを通す。偽のネットワークは両方向に同じ 1 本を使う）。
fn net_dir() -> higanr_core::seal::DirKeys {
    higanr_core::seal::DirKeys::from_raw([0x7F; 32], [0x7F; 32])
}

const FRAME_MS: u64 = 16;
const RESEND_MS: u64 = 4;

#[derive(Debug, Clone, Copy)]
enum Kind {
    Capture,
    Resend,
    PrevReply,
}

#[derive(Debug, Clone, Copy)]
struct SendInfo {
    from: usize,
    #[allow(dead_code)] // 乱れの決め方に使いたいとき用（いまは使う検査が無い）
    kind: Kind,
    /// その送り手がその seg の INPUT を送った通し番号（0 から。前の seg のバックアップの返信は数えない）。乱れの決め方に使う。
    nth: u32,
    /// 送る INPUT の seg（前の seg のバックアップの返信ではその前の seg）。
    seg: u16,
    /// 載せた lat（ホストの D の予定・ゲストの要求）。
    lat: Option<LatChange>,
}

/// マッチの途中の Ctrl＋数字の押下（台 `node` が、seg `seg` の取得のフレーム `frame` の取得の前に `press_delay(x)` する）。
/// ホスト（台 0）なら予定、ゲスト（台 1）なら要求。
#[derive(Debug, Clone, Copy)]
struct PressAt {
    node: usize,
    seg: u32,
    frame: u32,
    x: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Deliver,
    Drop,
    /// 片道の遅れに足す（後に送ったものに追い越される）。
    Delay(u64),
    /// 1 通はふつうに、もう 1 通を足した遅れで。
    Dup(u64),
}

struct Scenario {
    cfg: [Config; 2],
    segs: u32,
    /// seg ごとの D（`delays[k]` が seg k の D）。rollback ではホスト = 台 0 がマッチの終わりに `press_delay` で押す。
    /// seg 0 は `cfg` の D（AGREE の値）と同じにしておく。
    delays: Vec<u8>,
    one_way_ms: u64,
    start_ms: [u64; 2],
    load_ms: [u64; 2],
    sabotage: Sabotage,
    /// 台ごとの 1 フレームの長さ（µs）。既定は両方 `FRAME_MS`。違えると速い側が少しずつ先へずれる（時刻合わせの検査用）。
    /// 1 ms 刻みの時計の上では、枠の始まりを µs で数え、その µs を越えた最初の ms に走らせる。
    period_us: [u64; 2],
    life: u8,
    max_ms: u64,
    /// 台ごとに、格子を寄せられるか（b-spin のウィンドウ = true、ゲーム自身の時計の全画面 = false）。寄せられる台は、枠の始まりで
    /// `Rollback::glide_ppm_with(&glide_params)` が返した量だけ次の枠までの長さを変える。
    glide: [bool; 2],
    glide_params: GlideParams,
    /// 寄せる判定に渡す往復（µs）。`None` は片道の 2 倍。
    glide_rtt_us: Option<u32>,
    /// `Some(j)` なら寄せる判定の往復を本物と同じ作りにする: 1 秒に 1 回の PONG（往復 = 片道の 2 倍 + 両向きの揺れ 0〜j ms）の
    /// 直近 8 個の中央値（`liveness::median_ms`。3 個に満たない間は渡さない）。
    pong_jitter_ms: Option<u64>,
    /// `Some(off)` なら台 0 を「ゲーム自身の時計」にする: 時計が `off` µs から数え始め、周期が時計の大きさで 59.987〜60.015 fps に
    /// 変わり、429.5 秒で 0 に戻って直後の枠が 7.46 ms で来る（`game_period_us`）。`period_us[0]` は使わない。
    game_clock_offset_us: Option<u64>,
    /// マッチの途中の Ctrl＋数字の押下。空でなければ、`delays` は seg 0 の D だけを使い、seg の始まりの D は前の seg の最後の予定の値になる
    /// （`judge` もそれで見る）。
    presses: Vec<PressAt>,
    /// seg ごとの推奨値の組（`Rollback::set_recommend`。`flat_recs` / `rtt_recs` で作る）。
    /// seg `k` に入った直後・押下より前に両台へ渡す。空なら渡さない。
    rec_seq: Vec<Recommend>,
    /// マッチの途中で替える実行の、seg ごとの始まりの D の手書きの期待値。`None` なら前の seg の最後の予定の値。
    /// 読み込みの間の押下や auto が始まりの D を直に替える実行では手で書く。
    expect_heads: Option<Vec<u8>>,
}

/// `PressAt::frame` がこの値なら、その seg の読み込みに入った直後（`begin_seg` の後・最初の取得の前）に押す。
const LOAD_PRESS: u32 = u32::MAX;

/// rollback のモードの台 `i` の設定（台 0 = ホスト・席 2P、台 1 = ゲスト・席 1P）。
fn rb_cfg(i: usize, delay: u8, ahead: u8) -> Config {
    Config::rollback(SEATS[i], delay, ahead).with_role(ROLES[i])
}

impl Scenario {
    fn new(cfg: [Config; 2], segs: u32) -> Self {
        Scenario {
            cfg,
            segs,
            delays: vec![cfg[0].delay(); segs as usize],
            one_way_ms: 5,
            start_ms: [0, 7],
            load_ms: [40, 300],
            sabotage: Sabotage::None,
            period_us: [FRAME_MS * 1000; 2],
            life: LIFE,
            max_ms: MAX_MS,
            glide: [false; 2],
            glide_params: GlideParams::DEFAULT,
            glide_rtt_us: None,
            pong_jitter_ms: None,
            game_clock_offset_us: None,
            presses: Vec::new(),
            rec_seq: Vec::new(),
            expect_heads: None,
        }
    }

    fn rollback(delay: u8, ahead: u8, segs: u32) -> Self {
        Self::new([rb_cfg(0, delay, ahead), rb_cfg(1, delay, ahead)], segs)
    }

    fn rollback_seq(delays: &[u8], ahead: u8) -> Self {
        let mut sc = Self::rollback(delays[0], ahead, delays.len() as u32);
        sc.delays = delays.to_vec();
        sc
    }
}

enum Phase {
    Play,
    Load(u64),
    Done,
}

struct Node {
    rb: Rollback,
    game: FakeGame,
    seg_idx: u32,
    phase: Phase,
    next_slot_us: u64,
    ran: bool,
    /// 時刻合わせの待ちを返された枠。この枠はもう試さない。
    sync_skip: bool,
    /// 時刻合わせの待ちを返された数と、最後の待ちから走らせた更新の数。
    sync_steps: u32,
    /// そのうち、seg の始まりの 300 フレームを過ぎてからの数。
    sync_steps_steady: u32,
    since_sync: u32,
    /// 取り合いの最悪: 直近 `RECENT_UPDATES` 更新の中の時刻合わせの待ちの数の、seg をまたいだ最大。数を出すだけで合否は付けない。
    updates_run: u32,
    recent_waits: Vec<u32>,
    worst_recent_waits: u32,
    /// 格子を寄せた枠の数・1 フレームの長さの変化の最大（ppm の絶対値）・寄せた量の絶対値の合計 / 正味の合計（µs）。
    glide_frames: u32,
    glide_max_ppm: i32,
    /// そのうち seg の始まりの 300 フレームを過ぎてからの最大・前の枠との差の最大・前の枠の値。
    glide_max_ppm_steady: i32,
    glide_max_step_ppm: i32,
    glide_max_step_ppm_steady: i32,
    glide_prev_ppm: i32,
    glide_abs_us: u64,
    glide_net_us: i64,
    /// 寄せる判定の往復の作り: 直近の PONG の往復（ms）・次の PONG の時刻（ms）・揺れの乱数。
    rtt_hist: Vec<u32>,
    next_pong_ms: u64,
    pong_rng: Rng,
    /// 寄せる判定を呼んだ数と、そのとき「寄せてよい」だった数。
    glide_calls: u32,
    gate_open: u32,
    /// ゲーム自身の時計の台: 前の枠の時計（折り返しの周期の中の µs）・折り返した時刻（sim の µs）。
    game_prev_m: u64,
    wrap_times_us: Vec<u64>,
    wait_times_us: Vec<u64>,
    /// 前の枠の始まりの `sync_steps`（待ちの直後の枠かを見る）と、寄せ幅が 1 枠に 5,000 ppm を越えて変わった数・そのうち前の枠で待ちが
    /// あった数。
    steps_at_prev_slot: u32,
    big_steps: u32,
    big_steps_after_wait: u32,
    big_steps_other: u32,
    last_send: u64,
    stalls: u32,
    /// ゲストが D 未定で取得を断られた回（`CaptureError::NoDelay`）。ホストでは 0 のはず。
    no_delay: u32,
    /// 更新を走らせた枠ごとの (seg の番号, 更新したフレーム, 更新後の N − F)。時刻合わせの効きを見る。
    ahead_log: Vec<(u32, u32, u32)>,
    prev_front: u32,
    resim_reports: u32,
    violations: Vec<String>,
}

struct Outcome {
    finished: bool,
    nodes: Vec<Node>,
    sent: u32,
    dropped: u32,
    /// ネットワークへ出た INPUT の lat が取り決めと違った例。
    wire: Vec<String>,
    prev_replies: u32,
    other_seg: [u32; 2],
}

struct Net<'f> {
    in_flight: Vec<(u64, u64, usize, Vec<u8>)>,
    order: u64,
    one_way: u64,
    fate: &'f mut dyn FnMut(&SendInfo) -> Fate,
    key: AuthKey,
    seq: higanr_core::seal::Seq,
    sent: u32,
    dropped: u32,
    /// rollback の両端の実行なら true。ネットワークへ出る INPUT の lat を取り決めと比べる（ホストは全部の INPUT に載せ、frame 0 の値は
    /// seg ごとに 1 つ、同じ F には同じ値。ゲストは載せないか要求 {seq ≥ 1, x}）。
    check_lat: bool,
    /// マッチの途中の押下の無い実行なら seg ごとの D（frame 0 の値をこれと比べる）。
    delays: Option<&'f [u8]>,
    seen_sched: std::collections::HashMap<(u16, u32), u8>,
    wire: Vec<String>,
    prev_replies: u32,
    seg_sends: [(u16, u32); 2],
}

impl Net<'_> {
    fn send(&mut self, t: u64, from: usize, inp: &Input, kind: Kind) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let sealer = net_dir().sealer(higanr_core::seal::Sender::Link);
        let len = wire::seal(&sealer, &self.seq, 0x5EED, &Message::Input(*inp), &mut buf)
            .expect("封じられる");
        self.sent += 1;
        let to = 1 - from;
        let at = t + self.one_way;
        if self.check_lat {
            let bad = match (from, inp.lat) {
                (0, None) => Some("ホストが lat を載せない".to_string()),
                (0, Some(l)) => {
                    let first = *self.seen_sched.entry((inp.seg, l.frame)).or_insert(l.delay);
                    let want0 = self
                        .delays
                        .and_then(|ds| ds.get(usize::from(inp.seg)).copied());
                    if first != l.delay {
                        Some(format!("同じ F {} に別の値 {first} / {}", l.frame, l.delay))
                    } else if l.frame == 0 && want0.is_some_and(|d| d != l.delay) {
                        Some(format!("seg の D は {want0:?}"))
                    } else {
                        None
                    }
                }
                (_, Some(l)) if l.frame == 0 => Some("ゲストの要求の seq が 0".to_string()),
                _ => None,
            };
            if let Some(why) = bad {
                self.wire.push(format!(
                    "台 {from} の INPUT（seg {}・{kind:?}）の lat {:?}: {why}",
                    inp.seg, inp.lat
                ));
            }
        }
        let nth = match kind {
            Kind::PrevReply => {
                self.prev_replies += 1;
                0
            }
            _ => {
                let s = &mut self.seg_sends[from];
                if s.0 != inp.seg {
                    *s = (inp.seg, 0);
                }
                s.1 += 1;
                s.1 - 1
            }
        };
        let seg = inp.seg;
        match (self.fate)(&SendInfo {
            from,
            kind,
            nth,
            seg,
            lat: inp.lat,
        }) {
            Fate::Deliver => self.push(at, to, &buf[..len]),
            Fate::Drop => self.dropped += 1,
            Fate::Delay(x) => self.push(at + x, to, &buf[..len]),
            Fate::Dup(x) => {
                self.push(at, to, &buf[..len]);
                self.push(at + x, to, &buf[..len]);
            }
        }
    }

    fn push(&mut self, at: u64, to: usize, bytes: &[u8]) {
        self.in_flight.push((at, self.order, to, bytes.to_vec()));
        self.order += 1;
    }
}

fn monitor(n: &mut Node, what: &str) {
    let (nn, ff) = (n.rb.frame(), n.rb.front());
    let r = u32::from(n.rb.config().max_ahead());
    if ff < n.prev_front {
        n.violations
            .push(format!("{what}: F が戻った {} -> {ff}", n.prev_front));
    }
    if ff > nn {
        n.violations.push(format!("{what}: F {ff} > N {nn}"));
    }
    if nn - ff.min(nn) > r {
        n.violations
            .push(format!("{what}: N − F = {} > R = {r}", nn - ff.min(nn)));
    }
    n.prev_front = ff;
}

const MAX_MS: u64 = 120_000;

/// ゲーム自身の時計が 0 に戻る周期（µs）= 2^32 カウント ÷ 10 MHz（実測で 429.506 秒）。
const GAME_CLOCK_WRAP_US: u64 = 429_496_730;
/// 折り返した直後の枠が来るまでの長さ（µs。実測 7.46 ms。ふつうの 16.67 ms より 9.2 ms 短い）。
const GAME_WRAP_SLOT_US: u64 = 7_460;

/// ゲーム自身の時計が `m` µs（折り返しの周期の中）のときの 1 周期（µs）。実測の表: 32 秒まで 60.000 fps（表に無いのでウィンドウと同じとする）、
/// 32〜64 秒 0.0166664、64〜128 秒 0.0166702、128 秒から折り返しまで 0.0166626。
fn game_period_us(m: u64) -> u64 {
    match m / 1_000_000 {
        0..=31 => 16_667,
        32..=63 => 16_666,
        64..=127 => 16_670,
        _ => 16_663,
    }
}

/// 取り合いの最悪を数える期間（更新の数。60 更新 = 約 1 秒）。
const RECENT_UPDATES: u32 = 60;

fn run(sc: &Scenario, fate: &mut dyn FnMut(&SendInfo) -> Fate) -> Outcome {
    // rollback のモードは台 0 がホスト・台 1 がゲスト（取り違えると、ホストの lat をゲストが載せるなどで INPUT が全部捨てられる）。
    for (i, c) in sc.cfg.iter().enumerate() {
        if c.mode() == Mode::Rollback {
            assert_eq!(c.role(), ROLES[i], "台 {i} の役");
        }
    }
    let mut nodes: Vec<Node> = (0..2)
        .map(|i| Node {
            rb: Rollback::new(sc.cfg[i]),
            game: FakeGame::with_life(sc.sabotage, sc.life),
            seg_idx: 0,
            phase: Phase::Load(sc.start_ms[i]),
            next_slot_us: 0,
            ran: false,
            sync_skip: false,
            sync_steps: 0,
            sync_steps_steady: 0,
            since_sync: u32::MAX,
            updates_run: 0,
            recent_waits: Vec::new(),
            worst_recent_waits: 0,
            glide_frames: 0,
            glide_max_ppm: 0,
            glide_max_ppm_steady: 0,
            glide_max_step_ppm: 0,
            glide_max_step_ppm_steady: 0,
            glide_prev_ppm: 0,
            glide_abs_us: 0,
            glide_net_us: 0,
            rtt_hist: Vec::new(),
            next_pong_ms: 1_000,
            pong_rng: Rng::new(0xC0DE + i as u64),
            glide_calls: 0,
            gate_open: 0,
            game_prev_m: 0,
            wrap_times_us: Vec::new(),
            wait_times_us: Vec::new(),
            steps_at_prev_slot: 0,
            big_steps: 0,
            big_steps_after_wait: 0,
            big_steps_other: 0,
            last_send: 0,
            stalls: 0,
            no_delay: 0,
            ahead_log: Vec::new(),
            prev_front: 0,
            resim_reports: 0,
            violations: Vec::new(),
        })
        .collect();
    let mut net = Net {
        in_flight: Vec::new(),
        order: 0,
        one_way: sc.one_way_ms,
        fate,
        key: net_key(),
        seq: higanr_core::seal::Seq::new(),
        sent: 0,
        dropped: 0,
        check_lat: sc.cfg.iter().all(|c| c.mode() == Mode::Rollback),
        delays: (sc.cfg.iter().all(|c| c.mode() == Mode::Rollback) && sc.presses.is_empty())
            .then_some(sc.delays.as_slice()),
        seen_sched: std::collections::HashMap::new(),
        wire: Vec::new(),
        prev_replies: 0,
        seg_sends: [(u16::MAX, 0); 2],
    };
    let mut finished = false;
    let mut other_seg = [0u32; 2];
    let mut pressed = vec![false; sc.presses.len()];
    if let Some(&r) = sc.rec_seq.first() {
        for n in &mut nodes {
            n.rb.set_recommend(Some(r));
        }
    }
    let mut t = 0u64;
    while t < sc.max_ms {
        let mut due: Vec<(u64, usize, Vec<u8>)> = Vec::new();
        net.in_flight.retain(|(at, ord, to, b)| {
            if *at == t {
                due.push((*ord, *to, b.clone()));
                false
            } else {
                true
            }
        });
        due.sort_by_key(|d| d.0);
        for (_, to, bytes) in due {
            let pkt = wire::decode(&net.key, Some(&net_dir().opener()), &bytes)
                .expect("模型が送ったものは必ず読める");
            let Message::Input(inp) = pkt.msg else {
                panic!("INPUT 以外は送っていない")
            };
            let outcome = nodes[to].rb.on_input(&inp);
            if outcome == RecvOutcome::OtherSeg {
                other_seg[to] += 1;
            }
            if let RecvOutcome::PrevSeg { reply: true } = outcome
                && let Some(p) = nodes[to].rb.build_prev_input()
            {
                net.send(t, to, &p, Kind::PrevReply);
            }
            // 正しい相手の INPUT は、lat の取り決めで捨てられない（捨てられたら模型の送り手が取り決めを破っている）。
            if matches!(outcome, RecvOutcome::GuestLat | RecvOutcome::BadLat) {
                nodes[to]
                    .violations
                    .push(format!("正しい INPUT を {outcome:?} で捨てた"));
            }
        }

        #[allow(clippy::needless_range_loop)]
        // nodes[i] を借りたまま net へ送るので、添字のまま書く。
        for i in 0..2 {
            let n = &mut nodes[i];
            let mut new_slot = false;
            match n.phase {
                Phase::Done => continue,
                Phase::Load(until) => {
                    if t < until {
                        continue;
                    }
                    n.phase = Phase::Play;
                    n.next_slot_us = t * 1000 + sc.period_us[i];
                    n.game.new_scene(n.rb.seg());
                    new_slot = true;
                }
                Phase::Play => {
                    if t * 1000 >= n.next_slot_us {
                        if !n.ran {
                            n.stalls += 1;
                            let before = (n.rb.front(), n.rb.frame());
                            let after = n.rb.post_present(&mut n.game);
                            check_after(n, before, after);
                            monitor(n, "post_present(stall)");
                        }
                        let mut len = match sc.game_clock_offset_us {
                            Some(off) if i == 0 => {
                                let m = (off + n.next_slot_us) % GAME_CLOCK_WRAP_US;
                                let wrapped = m < n.game_prev_m;
                                n.game_prev_m = m;
                                if wrapped {
                                    n.wrap_times_us.push(n.next_slot_us);
                                    GAME_WRAP_SLOT_US as i64
                                } else {
                                    game_period_us(m) as i64
                                }
                            }
                            _ => sc.period_us[i] as i64,
                        };
                        let waited_before = n.sync_steps != n.steps_at_prev_slot;
                        n.steps_at_prev_slot = n.sync_steps;
                        if sc.glide[i] {
                            // 枠の始まりで 1 回: 本物の dll も、始まりに 1 回だけ要求を読んで格子を動かす。
                            let rtt_us = match sc.pong_jitter_ms {
                                Some(j) => {
                                    if t >= n.next_pong_ms {
                                        n.next_pong_ms = t + 1_000;
                                        let r = 2 * sc.one_way_ms
                                            + n.pong_rng.below(j + 1)
                                            + n.pong_rng.below(j + 1);
                                        n.rtt_hist.push(r as u32);
                                        if n.rtt_hist.len() > RTT_WINDOW {
                                            n.rtt_hist.remove(0);
                                        }
                                    }
                                    median_ms(&n.rtt_hist).map(|ms| ms * 1_000)
                                }
                                None => {
                                    Some(sc.glide_rtt_us.unwrap_or(2000 * sc.one_way_ms as u32))
                                }
                            };
                            let ppm = n.rb.glide_ppm_with(&sc.glide_params, rtt_us, 16_667);
                            n.glide_calls += 1;
                            if n.rb.glide_on() {
                                n.gate_open += 1;
                            }
                            let adj = len * i64::from(ppm) / 1_000_000;
                            len += adj;
                            if n.rb.frame() >= 300 {
                                n.glide_max_ppm_steady = n.glide_max_ppm_steady.max(ppm.abs());
                            }
                            let step = (ppm - n.glide_prev_ppm).abs();
                            n.glide_max_step_ppm = n.glide_max_step_ppm.max(step);
                            if n.rb.frame() >= 300 {
                                n.glide_max_step_ppm_steady = n.glide_max_step_ppm_steady.max(step);
                            }
                            if step > 5_000 {
                                n.big_steps += 1;
                                if waited_before {
                                    n.big_steps_after_wait += 1;
                                } else if n.rb.frame() >= 3 {
                                    n.big_steps_other += 1;
                                }
                            }
                            n.glide_prev_ppm = ppm;
                            if ppm != 0 {
                                n.glide_frames += 1;
                                n.glide_max_ppm = n.glide_max_ppm.max(ppm.abs());
                                n.glide_abs_us += adj.unsigned_abs();
                                n.glide_net_us += adj;
                            }
                        }
                        n.next_slot_us = (n.next_slot_us as i64 + len) as u64;
                        new_slot = true;
                    }
                }
            }
            if new_slot {
                n.ran = false;
                n.sync_skip = false;
            }
            // 取得は枠の始まりに 1 回。ゲストは seg の D を知るまで断られるので、知るまでは毎 ms 試す（本物の dll は D 未定の間、入力を
            // 読まずに待つ）。D 未定の間も 4 ms ごとに語 0 個の INPUT を送り直す。
            if matches!(n.phase, Phase::Play)
                && !n.rb.captured()
                && (new_slot || !n.rb.delay_known())
            {
                for (k, p) in sc.presses.iter().enumerate() {
                    if !pressed[k] && p.node == i && p.seg == n.seg_idx && p.frame == n.rb.frame() {
                        pressed[k] = true;
                        n.rb.press_delay(p.x);
                    }
                }
                let w = word(i, n.rb.seg(), n.rb.frame());
                match n.rb.capture(w) {
                    Ok(()) => {
                        let inp = n.rb.build_input();
                        net.send(t, i, &inp, Kind::Capture);
                        nodes[i].last_send = t;
                    }
                    Err(CaptureError::NoDelay) => {
                        n.no_delay += 1;
                        if t >= n.last_send + RESEND_MS {
                            let inp = n.rb.build_input();
                            net.send(t, i, &inp, Kind::Resend);
                            nodes[i].last_send = t;
                        }
                    }
                    Err(e) => panic!("模型の中で取得は断られない: {e:?}"),
                }
            }
            let n = &mut nodes[i];
            if !n.ran && !n.sync_skip && !n.rb.scene_ended() {
                let step = n.rb.input_point(&mut n.game);
                monitor(n, "input_point");
                match step {
                    Step::Ran { frame, .. } => {
                        n.ran = true;
                        let before = (n.rb.front(), n.rb.frame());
                        let after = n.rb.post_present(&mut n.game);
                        check_after(n, before, after);
                        monitor(n, "post_present");
                        let (nn, ff) = (n.rb.frame(), n.rb.front());
                        n.ahead_log.push((n.seg_idx, frame, nn.saturating_sub(ff)));
                        n.since_sync = n.since_sync.saturating_add(1);
                        n.updates_run += 1;
                    }
                    Step::Wait(why) => {
                        if why == Wait::Fault {
                            n.violations.push("Wait::Fault".to_string());
                        }
                        if why == Wait::Sync {
                            n.sync_skip = true;
                            if n.since_sync < u32::from(SYNC_GAP) {
                                n.violations.push(format!(
                                    "時刻合わせの待ちの間隔が {} 更新（{SYNC_GAP} 未満）",
                                    n.since_sync
                                ));
                            }
                            n.sync_steps += 1;
                            n.wait_times_us.push(t * 1000);
                            if n.rb.frame() >= 300 {
                                n.sync_steps_steady += 1;
                            }
                            n.since_sync = 0;
                            let now = n.updates_run;
                            n.recent_waits.push(now);
                            n.recent_waits.retain(|&u| u + RECENT_UPDATES > now);
                            n.worst_recent_waits =
                                n.worst_recent_waits.max(n.recent_waits.len() as u32);
                        }
                        if t >= n.last_send + RESEND_MS {
                            let inp = n.rb.build_input();
                            net.send(t, i, &inp, Kind::Resend);
                            nodes[i].last_send = t;
                        }
                    }
                    Step::SceneEnd => {}
                }
            }
            let n = &mut nodes[i];
            if n.rb.scene_ended() {
                n.seg_idx += 1;
                let next = n.rb.seg().wrapping_add(1);
                if i == 0
                    && sc.presses.is_empty()
                    && let Some(&d) = sc.delays.get(n.seg_idx as usize)
                {
                    // ホストはマッチの終わり（`begin_seg` の前）に D を押す。今の seg の残りでは届かない予定になり、次の seg の始まりの D になる。
                    n.rb.press_delay(d);
                }
                n.rb.begin_seg(next);
                if n.seg_idx < sc.segs {
                    // 選択の間（読み込みの間）: 推奨値が替わり、人が押す（`LOAD_PRESS`）。ホストの押下は次の seg の始まりの D になる。
                    if let Some(&r) = sc.rec_seq.get(n.seg_idx as usize) {
                        n.rb.set_recommend(Some(r));
                    }
                    for (k, p) in sc.presses.iter().enumerate() {
                        if !pressed[k] && p.node == i && p.seg == n.seg_idx && p.frame == LOAD_PRESS
                        {
                            pressed[k] = true;
                            n.rb.press_delay(p.x);
                        }
                    }
                }
                n.prev_front = 0;
                n.since_sync = u32::MAX;
                n.phase = if n.seg_idx == sc.segs {
                    Phase::Done
                } else {
                    Phase::Load(t + sc.load_ms[i])
                };
            }
        }
        if nodes.iter().all(|n| matches!(n.phase, Phase::Done)) {
            finished = true;
            break;
        }
        t += 1;
    }
    Outcome {
        finished,
        nodes,
        sent: net.sent,
        dropped: net.dropped,
        wire: net.wire,
        prev_replies: net.prev_replies,
        other_seg,
    }
}

fn check_after(n: &mut Node, before: (u32, u32), after: After) {
    if let After::Resim {
        from,
        confirmed,
        predicted,
    } = after
    {
        let (f0, n0) = before;
        let (f1, n1) = (n.rb.front(), n.rb.frame());
        let mut bad = from != f0 || f1 < f0;
        if !n.rb.scene_ended() {
            // 通常の進め直し: 確定の分だけ F が進み、残りの N − F が予測の回数。N が戻ることもある（境目）。
            bad |= confirmed != f1 - f0 || predicted != n1 - f1;
        }
        bad |= n1 > n0;
        if bad {
            n.resim_reports += 1;
            n.violations.push(format!(
                "進め直しの報告が合わない: {after:?} 前 (F {f0}, N {n0}) 後 (F {f1}, N {n1})"
            ));
        }
    }
}

// ---------------------------------------------------------------- 乱れ

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(mix(0xA5A5, seed) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn clean(_: &SendInfo) -> Fate {
    Fate::Deliver
}

fn jitter(seed: u64, max: u64) -> impl FnMut(&SendInfo) -> Fate {
    let mut r = Rng::new(seed);
    move |_| Fate::Delay(r.below(max + 1))
}

/// ロス 20%・入れ替え 10%・重複 5%・小さな揺らぎ（`lockstep.rs` の模型と同じ割合）。
fn lossy(seed: u64) -> impl FnMut(&SendInfo) -> Fate {
    let mut r = Rng::new(seed);
    move |_| {
        let x = r.below(100);
        if x < 20 {
            Fate::Drop
        } else if x < 30 {
            Fate::Delay(1 + r.below(40))
        } else if x < 35 {
            Fate::Dup(1 + r.below(30))
        } else {
            Fate::Delay(r.below(3))
        }
    }
}

/// 決め打ち: 60 通に 1 回、続けて 12 通を落とす（バースト）。台 0 → 台 1 の向きだけ 2 通おきに 15 ms 遅らせる。
fn burst_loss() -> impl FnMut(&SendInfo) -> Fate {
    let mut count = 0u32;
    move |info| {
        count += 1;
        if count % 60 < 12 {
            Fate::Drop
        } else if info.from == 0 && count.is_multiple_of(2) {
            Fate::Delay(15)
        } else {
            Fate::Deliver
        }
    }
}

// ---------------------------------------------------------------- 合格の判定

#[derive(Default, Debug)]
struct Totals {
    runs: u32,
    stats: Stats,
    stalls: u32,
    paused_ticks: u32,
    stray: u32,
    sent: u32,
    dropped: u32,
    no_delay: u32,
}

impl Totals {
    fn add(&mut self, s: &Stats) {
        let t = &mut self.stats;
        t.live_confirmed += s.live_confirmed;
        t.live_unconfirmed_real += s.live_unconfirmed_real;
        t.live_predicted += s.live_predicted;
        t.resims += s.resims;
        t.stall_rewinds += s.stall_rewinds;
        t.resim_confirmed_ticks += s.resim_confirmed_ticks;
        t.resim_predicted_ticks += s.resim_predicted_ticks;
        t.holds_set += s.holds_set;
        t.holds_ended += s.holds_ended;
        t.holds_released += s.holds_released;
        t.holds_rearmed += s.holds_rearmed;
        t.saves += s.saves;
        t.restores += s.restores;
        t.scenes_ended += s.scenes_ended;
        t.discarded_ticks += s.discarded_ticks;
        t.recaptured += s.recaptured;
        t.waits_ahead += s.waits_ahead;
        t.waits_boundary += s.waits_boundary;
        t.waits_sync += s.waits_sync;
        t.max_ahead_seen = t.max_ahead_seen.max(s.max_ahead_seen);
    }
}

/// 出口の ①〜⑤。落ちたら `Err` に理由を返す（否定の検査が「落ちること」を確かめるため、assert にしない）。
fn judge(label: &str, sc: &Scenario, out: &Outcome, rf: &RefRun) -> Result<(), String> {
    let e = |m: String| Err(format!("[{label}] {m}"));
    if !out.finished {
        return e("時間切れ（seg が終わらない）".into());
    }
    if let Some(v) = out.wire.first() {
        return e(format!("⑥ ネットワークへ出た INPUT が取り決めと違う: {v}"));
    }
    for (i, n) in out.nodes.iter().enumerate() {
        if let Some(v) = n.violations.first() {
            return e(format!("台 {i}: ③ 不変条件 {v}"));
        }
        // ①
        if n.game.confirmed != rf.confirmed {
            let k = n
                .game
                .confirmed
                .iter()
                .zip(&rf.confirmed)
                .position(|(a, b)| a != b);
            return e(format!(
                "台 {i}: ① 確定の列が参照と違う（長さ {} / {}、最初の違い {:?}）",
                n.game.confirmed.len(),
                rf.confirmed.len(),
                k.map(|k| (n.game.confirmed[k], rf.confirmed[k]))
            ));
        }
        // ②
        if n.game.checks != rf.checks {
            let k = n
                .game
                .checks
                .iter()
                .zip(&rf.checks)
                .position(|(a, b)| a != b);
            return e(format!(
                "台 {i}: ② 確定の頭の状態が参照と違う（{} / {} 個、最初の違い {:?}）",
                n.game.checks.len(),
                rf.checks.len(),
                k.map(|k| (n.game.checks[k], rf.checks[k]))
            ));
        }
        if n.game.stray_transitions != 0 {
            return e(format!(
                "台 {i}: 場面の要求が立ったまま更新が走った {} 回（予測で境目を跨いだ）",
                n.game.stray_transitions
            ));
        }
        if n.game.bad_restore != 0 {
            return e(format!("台 {i}: 書き戻しのフレームがコピーと違う"));
        }
        // ⑤ 式
        let s = n.rb.stats();
        if s.faults != 0 {
            return e(format!("台 {i}: 内部の食い違い {}", s.faults));
        }
        if s.max_ahead_seen > u32::from(sc.cfg[i].max_ahead()) {
            return e(format!(
                "台 {i}: ③ N − F の最大 {} > R {}",
                s.max_ahead_seen,
                sc.cfg[i].max_ahead()
            ));
        }
        let l: u32 = rf.seg_len.iter().sum();
        // (F1) 全フレームは確定として 1 回ずつ（入力の地点か進め直しの中で）。
        if s.live_confirmed + s.resim_confirmed_ticks != l {
            return e(format!(
                "台 {i}: ⑤ 確定の更新 {} + {} != 参照の確定フレーム数 {l}",
                s.live_confirmed, s.resim_confirmed_ticks
            ));
        }
        // (F2) ゲームの側が数えた更新の数と、状態機械の数え。
        let live = s.live_confirmed + s.live_unconfirmed_real + s.live_predicted;
        if n.game.tick_counts != [live, s.resim_confirmed_ticks, s.resim_predicted_ticks] {
            return e(format!(
                "台 {i}: ⑤ ゲームの更新の数 {:?} != 状態機械の数 {:?}",
                n.game.tick_counts,
                [live, s.resim_confirmed_ticks, s.resim_predicted_ticks]
            ));
        }
        // (F3) 確定していない状態の上の更新は、進め直しで確定するか、境目より先で捨てたかのどちらか。
        let unconf = s.live_unconfirmed_real + s.live_predicted;
        if unconf != s.resim_confirmed_ticks + s.discarded_ticks {
            return e(format!(
                "台 {i}: ⑤ 未確定の更新 {unconf} != 進め直しで確定 {} + 捨てた {}",
                s.resim_confirmed_ticks, s.discarded_ticks
            ));
        }
        // (F3b) 予測の tick が立てた待ちは、必ず 3 つのどれかで解ける。
        if s.holds_set != s.holds_ended + s.holds_released + s.holds_rearmed {
            return e(format!(
                "台 {i}: ⑤ 待ちの数 {} != 場面の終わり {} + 消えた {} + 立て直し {}",
                s.holds_set, s.holds_ended, s.holds_released, s.holds_rearmed
            ));
        }
        // (F4) コピーと書き戻し。
        if n.game.restores != s.resims || s.restores != s.resims {
            return e(format!(
                "台 {i}: ⑤ 書き戻し {} (状態機械 {}) != 進め直し {}",
                n.game.restores, s.restores, s.resims
            ));
        }
        let expect_saves = match sc.cfg[i].mode() {
            Mode::Rollback => s.live_confirmed + s.resims,
            Mode::Delay => 0,
        };
        if n.game.saves != expect_saves || s.saves != expect_saves {
            return e(format!(
                "台 {i}: ⑤ コピー {} (状態機械 {}) != 式 {expect_saves}（確定の本物の更新 {} + 進め直し {}）",
                n.game.saves, s.saves, s.live_confirmed, s.resims
            ));
        }
        // (F5) 時刻合わせの待ち: 状態機械の数と、呼び手が `Wait::Sync` を受けた数が同じ。使わない設定では 0。
        if s.waits_sync != n.sync_steps {
            return e(format!(
                "台 {i}: ⑤ 時刻合わせの待ち {} != 呼び手が受けた数 {}",
                s.waits_sync, n.sync_steps
            ));
        }
        if !sc.cfg[i].time_sync() && s.waits_sync != 0 {
            return e(format!(
                "台 {i}: 時刻合わせを使わない設定なのに待った {}",
                s.waits_sync
            ));
        }
        if s.scenes_ended != sc.segs {
            return e(format!(
                "台 {i}: 場面の終わり {} != seg の数 {}",
                s.scenes_ended, sc.segs
            ));
        }
        // (F6) seg ごとの D: 両者とも seg k の D が `delays[k]` に決まった。D 未定で待つのはゲストだけ（ホストは自分で決める）。
        if sc.cfg[i].mode() == Mode::Rollback {
            assert!(
                (sc.segs as usize) < SEG_D_LOG,
                "記録のリングバッファより長い実行は seg の並びで比べられない"
            );
            let got: Vec<(u16, u8)> = s
                .seg_d_log
                .iter()
                .take(s.seg_d_count as usize)
                .copied()
                .filter(|&(k, _)| u32::from(k) < sc.segs)
                .collect();
            let want: Vec<(u16, u8)> = if sc.presses.is_empty() {
                (0..sc.segs as usize)
                    .map(|k| (k as u16, sc.delays[k]))
                    .collect()
            } else {
                // マッチの途中で替えた実行: seg の始まりの D は、ホストの前の seg の最後の予定の値（無ければ前の seg の始まりの D）。
                match &sc.expect_heads {
                    Some(v) => {
                        assert_eq!(v.len(), sc.segs as usize, "手書きの始まりの D の数");
                        v.iter().enumerate().map(|(k, &d)| (k as u16, d)).collect()
                    }
                    None => expected_seg_starts(&out.nodes[0].rb.stats(), sc.delays[0], sc.segs),
                }
            };
            if got != want {
                return e(format!(
                    "台 {i}: ⑥ seg ごとの D {got:?} != 決めた並び {want:?}"
                ));
            }
            if i == 0 && (n.no_delay != 0 || s.delay_waits != 0) {
                return e(format!("台 {i}: ⑥ ホストが D 未定で待った"));
            }
            if s.delay_waits != n.no_delay {
                return e(format!(
                    "台 {i}: ⑥ D 未定の待ち {} != 呼び手が断られた数 {}",
                    s.delay_waits, n.no_delay
                ));
            }
        }
        if sc.cfg[i].mode() == Mode::Delay
            && (s.resims != 0 || s.live_predicted != 0 || s.live_unconfirmed_real != 0)
        {
            return e(format!(
                "台 {i}: delay 方式なのに予測・進め直しがある {s:?}"
            ));
        }
    }
    Ok(())
}

/// マッチの途中で D を替えた実行の、seg ごとの始まりの D（seg k + 1 の始まりは、ホストの記録した seg k の最後の予定の値）。
fn expected_seg_starts(host: &Stats, d0: u8, segs: u32) -> Vec<(u16, u8)> {
    assert!(
        (host.d_switches as usize) <= D_SWITCH_LOG,
        "予定の記録のリングバッファより多く替えた実行は seg の並びで比べられない"
    );
    let log = &host.d_switch_log[..host.d_switches as usize];
    let mut d = d0;
    (0..segs as u16)
        .map(|k| {
            if k > 0
                && let Some(last) = log.iter().rev().find(|s| s.seg == k - 1)
            {
                d = last.to;
            }
            (k, d)
        })
        .collect()
}

type SegSched = (u8, Vec<(u32, u8)>);

type SimFate = Box<dyn FnMut(&SendInfo) -> Fate>;

fn schedules_of(s: &Stats, segs: u32) -> Vec<SegSched> {
    assert!((s.d_switches as usize) <= D_SWITCH_LOG);
    assert!((s.seg_d_count as usize) < SEG_D_LOG);
    (0..segs as u16)
        .map(|k| {
            let d0 = s.seg_d_log[..s.seg_d_count as usize]
                .iter()
                .find(|&&(seg, _)| seg == k)
                .map(|&(_, d)| d)
                .expect("seg の始まりの D の記録が無い");
            let sw = s.d_switch_log[..s.d_switches as usize]
                .iter()
                .filter(|x| x.seg == k)
                .map(|x| (x.frame, x.to))
                .collect();
            (d0, sw)
        })
        .collect()
}

/// 予定から、台 `node` の seg `seg` の apply_frame `a` の語を直に組む（状態機械の取得の作りとは別の式）: a が seg の始まりの D より
/// 小さければ穴埋めの 0。そうでなければ、apply_frame（c + D(c)）が初めて a 以上になる取得のフレーム c を探し、ちょうど a なら
/// その取得の語、越えていれば（D を上げた境目の隙間）1 つ手前の apply_frame の語。D を下げた後の取得は、その apply_frame が
/// もっと前の取得で先に届いているので選ばれない。
fn word_at(node: usize, seg: u16, a: u32, sched: &SegSched) -> u16 {
    let (d0, sw) = sched;
    let d_of = |c: u32| {
        sw.iter()
            .rev()
            .find(|&&(f, _)| f <= c)
            .map_or(*d0, |&(_, x)| x)
    };
    let mut a = a;
    loop {
        if a < u32::from(*d0) {
            return 0;
        }
        let c = (0..=a)
            .find(|&c| c + u32::from(d_of(c)) >= a)
            .expect("c = a で必ず届く");
        if c + u32::from(d_of(c)) == a {
            return word(node, seg, c);
        }
        a -= 1;
    }
}

/// マッチの途中で D を替えた実行の参照: 両台の記録した予定から各台の語の列を `word_at` で組み、偽のゲームをネットワークなしで回す。
/// 期待値の式は状態機械の取得の作りと別に書いた。
fn reference_from_schedules(scheds: &[Vec<SegSched>; 2], segs: u32, life: u8) -> RefRun {
    let mut g = [
        FakeGame::with_life(Sabotage::None, life),
        FakeGame::with_life(Sabotage::None, life),
    ];
    let mut seg_len = Vec::new();
    let mut paused_ticks = 0;
    for (seg_idx, (s0, s1)) in scheds[0]
        .iter()
        .zip(&scheds[1])
        .enumerate()
        .take(segs as usize)
    {
        let seg = seg_idx as u16;
        for gm in &mut g {
            gm.new_scene(seg);
        }
        let mut f = 0u32;
        loop {
            let w = [word_at(0, seg, f, s0), word_at(1, seg, f, s1)];
            let mut boundary = [false; 2];
            for i in 0..2 {
                let (p1, p2) = by_seat(SEATS[i], w[i], w[1 - i]);
                if is_check_frame(f) {
                    let v = g[i].check_values();
                    g[i].check(&Check {
                        seg,
                        frame: f,
                        rng_state: v.rng_state,
                        rng_count: v.rng_count,
                        life_1p: v.life_1p,
                        life_2p: v.life_2p,
                    });
                }
                let rep = g[i].tick(Tick {
                    frame: f,
                    p1,
                    p2,
                    kind: TickKind::Live,
                });
                g[i].confirmed(f, p1, p2);
                boundary[i] = rep.boundary;
            }
            assert_eq!(boundary[0], boundary[1]);
            if boundary[0] {
                seg_len.push(f + 1);
                break;
            }
            f += 1;
            assert!(f < 100_000, "参照の seg が終わらない");
        }
        paused_ticks += g[0].world.paused_ticks;
    }
    assert_eq!(g[0].confirmed, g[1].confirmed);
    RefRun {
        confirmed: std::mem::take(&mut g[0].confirmed),
        checks: std::mem::take(&mut g[0].checks),
        seg_len,
        paused_ticks,
    }
}

fn expect_ok(label: &str, sc: &Scenario, out: &Outcome, rf: &RefRun, tot: &mut Totals) {
    if let Err(m) = judge(label, sc, out, rf) {
        panic!("{m}");
    }
    tot.runs += 1;
    for n in &out.nodes {
        tot.add(&n.rb.stats());
        tot.stalls += n.stalls;
        tot.no_delay += n.no_delay;
        tot.stray += n.game.stray_transitions;
    }
    tot.paused_ticks += rf.paused_ticks;
    tot.sent += out.sent;
    tot.dropped += out.dropped;
}

// ---------------------------------------------------------------- 検査

#[test]
fn clean_network_matches_delay_reference() {
    let mut tot = Totals::default();
    for (d, r) in [(0u8, 8u8), (1, 8), (2, 4), (1, 1), (3, 16), (9, 8)] {
        let rf = reference(d, 5);
        let sc = Scenario::rollback(d, r, 5);
        let out = run(&sc, &mut clean);
        expect_ok(&format!("clean D={d} R={r}"), &sc, &out, &rf, &mut tot);
    }
    eprintln!("clean: {tot:?}");
    // 綺麗なネットワークでも D < 片道 + 揺れ のときは予測が起きる（式の検査が空振りにならない）。
    assert!(
        tot.stats.resims > 0,
        "進め直しが 1 回も起きていない: {tot:?}"
    );
    assert!(
        tot.paused_ticks > 0,
        "参照に一時停止の tick が無い: {tot:?}"
    );
}

#[test]
fn jitter_loss_reorder_match_delay_reference() {
    let mut tot = Totals::default();
    for (d, r) in [(1u8, 8u8), (0, 8), (2, 4), (1, 1), (3, 16)] {
        let rf = reference(d, 4);
        for seed in 0..12u64 {
            let sc = Scenario::rollback(d, r, 4);
            let out = run(&sc, &mut jitter(seed, 40));
            expect_ok(
                &format!("jitter D={d} R={r} seed={seed}"),
                &sc,
                &out,
                &rf,
                &mut tot,
            );
            let out = run(&sc, &mut lossy(seed));
            expect_ok(
                &format!("lossy D={d} R={r} seed={seed}"),
                &sc,
                &out,
                &rf,
                &mut tot,
            );
        }
        let sc = Scenario::rollback(d, r, 4);
        let out = run(&sc, &mut burst_loss());
        expect_ok(&format!("burst D={d} R={r}"), &sc, &out, &rf, &mut tot);
        let mut sc = Scenario::rollback(d, r, 4);
        sc.start_ms = [0, 500];
        sc.load_ms = [700, 20];
        let out = run(&sc, &mut lossy(99));
        expect_ok(
            &format!("uneven load D={d} R={r}"),
            &sc,
            &out,
            &rf,
            &mut tot,
        );
    }
    eprintln!("jitter/loss: {tot:?}");
    // 網羅: この乱れの下で機構の全部の経路を通っている（通っていなければ検査が空振り）。
    let s = &tot.stats;
    assert!(
        s.resims > 0 && s.stall_rewinds > 0,
        "止まっている間の進め直しが無い: {tot:?}"
    );
    assert!(s.holds_set > 0, "予測で境目を立てた例が無い: {tot:?}");
    assert!(
        s.holds_released > 0,
        "予測の境目が進め直しで消えた例が無い: {tot:?}"
    );
    assert!(
        s.holds_ended > 0,
        "予測の境目が確定の語でも立った例が無い: {tot:?}"
    );
    assert!(
        s.discarded_ticks > 0,
        "境目より先の更新を捨てた例が無い: {tot:?}"
    );
    assert!(
        s.waits_boundary > 0 && s.waits_ahead > 0,
        "待ちの種類が足りない: {tot:?}"
    );
    assert!(s.live_predicted > 0 && s.live_confirmed > 0, "{tot:?}");
    // 読み込みの長さの違う実行が含まれるので、時刻合わせの待ちも通っている。
    assert!(s.waits_sync > 0, "時刻合わせの待ちが 1 度も無い: {tot:?}");
    assert!(tot.paused_ticks > 0, "{tot:?}");
    assert!(
        tot.dropped > 0 && tot.dropped < tot.sent,
        "落とした数 {} / 送った数 {}",
        tot.dropped,
        tot.sent
    );
    assert_eq!(tot.stray, 0);
}

#[test]
fn slow_link_stalls_but_matches_reference() {
    let mut tot = Totals::default();
    for (d, r) in [(1u8, 8u8), (1, 2), (2, 16)] {
        let rf = reference(d, 3);
        let mut sc = Scenario::rollback(d, r, 3);
        sc.one_way_ms = 60;
        let out = run(&sc, &mut jitter(3, 25));
        expect_ok(&format!("slow D={d} R={r}"), &sc, &out, &rf, &mut tot);
    }
    eprintln!("slow: {tot:?}");
    assert!(
        tot.stalls > 0,
        "止まっていない（R を使い切っていない）: {tot:?}"
    );
    assert!(tot.stats.stall_rewinds > 0, "{tot:?}");
}

#[test]
fn rollback_and_delay_mode_peers_refuse_each_others_inputs() {
    for swap in [false, true] {
        let mut rbs = Rollback::new(rb_cfg(if swap { 1 } else { 0 }, 2, 8));
        let mut dl = Rollback::new(Config::delay_mode(SEATS[if swap { 0 } else { 1 }], 2));
        rbs.capture(0x11).unwrap();
        dl.capture(0x22).unwrap();
        let from_rb = rbs.build_input();
        let from_dl = dl.build_input();
        assert_eq!(from_dl.lat, None, "delay 方式の端は lat を載せない");
        assert_eq!(
            from_rb.lat.is_some(),
            rbs.config().role() == Role::Host,
            "swap={swap}: rollback のホストだけが lat を載せる"
        );
        // delay 方式の端: lat 付き（ホストの rollback）は BadLat。lat 無し（ゲストの rollback）は普通に受かる（その向きは下で押さえる）。
        if from_rb.lat.is_some() {
            assert_eq!(dl.on_input(&from_rb), RecvOutcome::BadLat, "swap={swap}");
        }
        // rollback の端: delay 方式の INPUT は lat が無い。ゲストなら BadLat、ホストなら普通に受かる。
        let got = rbs.on_input(&from_dl);
        if rbs.config().role() == Role::Guest {
            assert_eq!(got, RecvOutcome::BadLat, "swap={swap}");
        } else {
            assert!(matches!(got, RecvOutcome::Accepted { .. }), "{got:?}");
        }
    }
}

#[test]
fn delay_mode_machine_equals_lockstep() {
    let mut tot = Totals::default();
    for d in [0u8, 1, 3] {
        let rf = reference(d, 4);
        for seed in 0..6u64 {
            let sc = Scenario::new(
                [
                    Config::delay_mode(SEATS[0], d),
                    Config::delay_mode(SEATS[1], d),
                ],
                4,
            );
            let out = run(&sc, &mut lossy(seed));
            expect_ok(
                &format!("delay-mode D={d} seed={seed}"),
                &sc,
                &out,
                &rf,
                &mut tot,
            );
        }
    }
    assert_eq!(tot.stats.resims, 0);
    assert_eq!(tot.stats.live_predicted, 0);
}

// ---------------------------------------------------------------- seg ごとの D

/// ホストの seg の最初の `k` 通（前の seg のバックアップの返信は数えない）を落とす。ほかは普通に届く。
fn drop_first_host_inputs(k: u32) -> impl FnMut(&SendInfo) -> Fate {
    move |i| {
        if i.from == 0 && i.nth < k && !matches!(i.kind, Kind::PrevReply) {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    }
}

/// ホストの seg の最初の 1 通だけを、`delay` ms 遅らせる（後の INPUT に追い越される）か、`dup` ms 後にもう 1 通足す。
fn disturb_first_host_input(delay: Option<u64>, dup: Option<u64>) -> impl FnMut(&SendInfo) -> Fate {
    move |i| {
        if i.from == 0 && i.nth == 0 && !matches!(i.kind, Kind::PrevReply) {
            if let Some(d) = delay {
                return Fate::Delay(d);
            }
            if let Some(d) = dup {
                return Fate::Dup(d);
            }
        }
        Fate::Deliver
    }
}

#[test]
fn d_changes_between_matches_and_both_sides_match_the_reference() {
    let mut tot = Totals::default();
    let mut prev_replies = 0;
    let seqs: [&[u8]; 3] = [&[1, 2, 0, 1], &[0, 2, 1, 2], &[2, 0, 2, 0]];
    for delays in seqs {
        let rf = reference_seq(delays);
        for ahead in [8u8, 2] {
            for load in [[40u64, 300], [700, 20]] {
                let mk = || {
                    let mut sc = Scenario::rollback_seq(delays, ahead);
                    sc.load_ms = load;
                    sc
                };
                let tag = format!("D={delays:?} R={ahead} load={load:?}");
                let mut go = |what: String, fate: &mut dyn FnMut(&SendInfo) -> Fate| {
                    let sc = mk();
                    let out = run(&sc, fate);
                    prev_replies += out.prev_replies;
                    expect_ok(&format!("{tag} {what}"), &sc, &out, &rf, &mut tot);
                };
                go("clean".into(), &mut clean);
                go("burst".into(), &mut burst_loss());
                for k in [1, 3] {
                    go(
                        format!("first {k} host inputs lost"),
                        &mut drop_first_host_inputs(k),
                    );
                }
                go(
                    "first host input overtaken".into(),
                    &mut disturb_first_host_input(Some(80), None),
                );
                go(
                    "first host input duplicated".into(),
                    &mut disturb_first_host_input(None, Some(30)),
                );
                for seed in 0..3u64 {
                    go(format!("jitter seed={seed}"), &mut jitter(seed, 40));
                    go(format!("lossy seed={seed}"), &mut lossy(seed));
                }
            }
        }
    }
    eprintln!("seg ごとの D: {tot:?} 前の seg のバックアップの返信 {prev_replies}");
    // 網羅: ゲストが D 未定で待った・予測と進め直しが起きた・前の seg のバックアップの返信に lat が載った。
    assert!(tot.no_delay > 0, "ゲストが D を待った例が無い: {tot:?}");
    assert!(tot.stats.resims > 0, "{tot:?}");
    assert!(
        prev_replies > 0,
        "前の seg のバックアップの返信が 1 通も出ていない（lat の比べが空振り）"
    );
}

#[test]
fn judge_catches_a_wrong_d_in_one_seg() {
    let delays = [1u8, 2, 0, 1];
    let sc = Scenario::rollback_seq(&delays, 8);
    let out = run(&sc, &mut clean);
    judge("same", &sc, &out, &reference_seq(&delays)).expect("同じ決めなら通る");
    for k in 1..delays.len() {
        let mut other = delays;
        other[k] = (other[k] + 1) % 3;
        let mut sc2 = Scenario::rollback_seq(&other, 8);
        sc2.load_ms = sc.load_ms;
        let err = judge(
            &format!("seg {k} だけ違う"),
            &sc2,
            &out,
            &reference_seq(&other),
        );
        assert!(err.is_err(), "seg {k} の D が違うのに通った");
    }
}

#[test]
fn the_guest_waits_for_the_hosts_d_and_the_gates_drop_what_breaks_the_agreement() {
    let mut host = Rollback::new(rb_cfg(0, 1, 8));
    let mut guest = Rollback::new(rb_cfg(1, 1, 8));
    assert!(host.delay_known() && guest.delay_known());
    host.capture(0x20).unwrap();
    assert_eq!(
        host.build_input().lat,
        Some(LatChange { frame: 0, delay: 1 }),
        "seg 0 の INPUT にも lat"
    );
    // ホストは D を 2 に押す。ゲストの語をまだ受けていない（ガードが閉じている）ので予定は出さず、次の seg の始まりの D として持つ。
    assert_eq!(host.press_delay(200), DelayPress::Refused);
    assert_eq!(host.press_delay(wire_max_delay() + 1), DelayPress::Refused);
    assert_eq!(host.press_delay(2), DelayPress::Held);
    assert_eq!(
        (host.seg_delay(), host.cur_delay(), host.shown_delay()),
        (1, 1, 2),
        "ゲートの前の押下は今の D を変えず、画面には押した値"
    );
    assert_eq!(
        host.build_input().lat,
        Some(LatChange { frame: 0, delay: 1 }),
        "ゲートの前に F > 0 の予定を出した"
    );
    host.begin_seg(1);
    guest.begin_seg(1);
    assert_eq!((host.seg_delay(), host.delay_known()), (2, true));
    let prev = host.build_prev_input().expect("前の seg のバックアップ");
    assert_eq!(
        (prev.seg, prev.lat),
        (0, Some(LatChange { frame: 0, delay: 1 })),
        "前の seg のバックアップには前の seg の D"
    );
    assert_eq!(
        host.build_input().lat,
        Some(LatChange { frame: 0, delay: 2 }),
        "新しい seg の INPUT には新しい D"
    );
    assert!(guest.build_prev_input().is_some_and(|p| p.lat.is_none()));
    assert!(!guest.delay_known());
    assert_eq!(guest.capture(0x31), Err(CaptureError::NoDelay));
    assert!(!guest.captured());
    let g0 = guest.build_input();
    assert_eq!((g0.count, g0.base, g0.ack, g0.lat), (0, 0, 0, None));
    assert_eq!(guest.stats().delay_waits, 1);
    host.capture(0x21).unwrap();
    let h = host.build_input();
    assert_eq!(
        (h.seg, h.base, h.count, h.words[0]),
        (1, 2, 1, 0x21),
        "最初の語は apply_frame D"
    );
    assert_eq!(h.lat, Some(LatChange { frame: 0, delay: 2 }));
    // 取り決めを破る INPUT は、ゲストが D を知らないままでも捨てて、D を採らない。
    let with_lat = |lat: Option<LatChange>| Input { lat, ..h };
    for (what, inp, want) in [
        ("lat なし", with_lat(None), RecvOutcome::BadLat),
        (
            "frame が 0 でない",
            with_lat(Some(LatChange { frame: 1, delay: 2 })),
            RecvOutcome::BadLat,
        ),
        (
            "D が 0〜9 の外",
            with_lat(Some(LatChange {
                frame: 0,
                delay: 10,
            })),
            RecvOutcome::BadLat,
        ),
        (
            "ack が D より先（まだ送っていない語を指す）",
            Input { ack: 99, ..h },
            RecvOutcome::AckAhead,
        ),
        (
            "base が窓の外",
            Input { base: 1000, ..h },
            RecvOutcome::OutOfWindow,
        ),
    ] {
        assert_eq!(guest.on_input(&inp), want, "{what}");
        assert!(!guest.delay_known(), "{what}: 捨てた INPUT で D を採った");
        assert_eq!(guest.remote_next(), 1, "{what}: 捨てた INPUT の語を使った");
    }
    let n0 = host.remote_next();
    let g_bad = Input {
        lat: Some(LatChange { frame: 0, delay: 2 }),
        ..g0
    };
    assert_eq!(host.on_input(&g_bad), RecvOutcome::GuestLat);
    assert_eq!(host.remote_next(), n0);
    assert_eq!(host.stats().wish_taken, 0);
    assert!(matches!(guest.on_input(&h), RecvOutcome::Accepted { .. }));
    assert_eq!((guest.delay_known(), guest.seg_delay()), (true, 2));
    guest.capture(0x31).unwrap();
    let g1 = guest.build_input();
    assert_eq!((g1.base, g1.count, g1.words[0], g1.lat), (2, 1, 0x31, None));
    assert_eq!(
        guest.on_input(&with_lat(Some(LatChange { frame: 0, delay: 1 }))),
        RecvOutcome::BadLat
    );
    assert!(matches!(guest.on_input(&h), RecvOutcome::Accepted { .. }));
    assert_eq!(guest.seg_delay(), 2);
    // D を知った後の seg の途中の予定: 知っている予定より大きい F は置き換え、同じ F で違う値は捨て、
    // 小さい F は古い便なので lat だけ無視して受ける。
    assert!(matches!(
        guest.on_input(&with_lat(Some(LatChange {
            frame: 50,
            delay: 1
        }))),
        RecvOutcome::Accepted { .. }
    ));
    assert_eq!((guest.delay_at(49), guest.delay_at(50)), (2, 1));
    assert_eq!(
        guest.on_input(&with_lat(Some(LatChange {
            frame: 50,
            delay: 0
        }))),
        RecvOutcome::BadLat,
        "同じ F で違う値を受けた"
    );
    assert!(matches!(
        guest.on_input(&with_lat(Some(LatChange {
            frame: 40,
            delay: 0
        }))),
        RecvOutcome::Accepted { .. }
    ));
    assert_eq!(
        (guest.delay_at(49), guest.delay_at(50), guest.seg_delay()),
        (2, 1, 2),
        "古い F の便で予定が戻った"
    );
    assert!(matches!(host.on_input(&g1), RecvOutcome::Accepted { .. }));
}

#[test]
fn a_new_seg_input_arriving_before_the_guests_begin_seg_is_dropped_then_adopted() {
    let delays = [1u8, 2, 0];
    let rf = reference_seq(&delays);
    let mut tot = Totals::default();
    // 裏: 乱れ無しなら、ゲストの OtherSeg は起きない。
    let sc = Scenario::rollback_seq(&delays, 8);
    let out = run(&sc, &mut clean);
    assert_eq!(out.other_seg, [0, 0], "乱れ無しで OtherSeg が起きている");
    expect_ok("clean", &sc, &out, &rf, &mut tot);
    // 本番: ホストの seg 0 の INPUT を遅らせ、ホストは読み込みを待たずに seg 1 へ入る。
    for ahead in [8u8, 2] {
        let mut sc = Scenario::rollback_seq(&delays, ahead);
        sc.load_ms = [0, 300];
        let mut late = |i: &SendInfo| {
            if i.from == 0 && i.seg == 0 {
                Fate::Delay(400)
            } else {
                Fate::Deliver
            }
        };
        let out = run(&sc, &mut late);
        assert!(
            out.other_seg[1] >= 1,
            "R={ahead}: ゲストが OtherSeg で捨てた数が 0（検査が素通り）: {:?}",
            out.other_seg
        );
        expect_ok(&format!("R={ahead} late seg 0"), &sc, &out, &rf, &mut tot);
    }
}

#[test]
fn a_guest_leaving_a_seg_with_d_unknown_keeps_an_empty_prev_and_adopts_the_next_seg() {
    let mut host = Rollback::new(rb_cfg(0, 1, 8));
    let mut guest = Rollback::new(rb_cfg(1, 1, 8));
    assert_eq!(host.press_delay(2), DelayPress::Held);
    host.begin_seg(1);
    guest.begin_seg(1);
    assert!(!guest.delay_known());
    // seg 1 の最初の取得の前（選択の間と同じ）の押下は、その seg の始まりの D をこの場で替える。
    assert_eq!(host.press_delay(0), DelayPress::Scheduled { frame: 0 });
    assert_eq!(host.seg_delay(), 0);
    host.begin_seg(2);
    guest.begin_seg(2);
    let prev = guest.build_prev_input().expect("前の seg のバックアップ");
    assert_eq!(
        (prev.seg, prev.count, prev.base, prev.ack, prev.lat),
        (1, 0, 0, 0, None),
        "D 未定のまま抜けた seg のバックアップは入力ワード 0 個・lat なし"
    );
    assert!(!guest.delay_known(), "D はまだ未定");
    assert_eq!(guest.capture(0x31), Err(CaptureError::NoDelay));
    let mut hg = Script::new(|_, _| false);
    let mut gg = Script::new(|_, _| false);
    for f in 0..20u16 {
        host.capture(0x100 + f).unwrap();
        let h = host.build_input();
        assert!(matches!(guest.on_input(&h), RecvOutcome::Accepted { .. }));
        assert_eq!((guest.delay_known(), guest.seg_delay()), (true, 0));
        guest.capture(0x200 + f).unwrap();
        let g = guest.build_input();
        assert!(matches!(host.on_input(&g), RecvOutcome::Accepted { .. }));
        assert!(matches!(host.input_point(&mut hg), Step::Ran { .. }));
        host.post_present(&mut hg);
        assert!(matches!(guest.input_point(&mut gg), Step::Ran { .. }));
        guest.post_present(&mut gg);
    }
    assert_eq!(guest.seg_delay(), 0);
    assert_eq!(hg.confirmed.len(), 20, "確定の行が足りない");
    assert_eq!(hg.confirmed, gg.confirmed, "両者の確定の列が違う");
    assert_eq!(hg.confirmed[3], (3, 0x203, 0x103));
}

fn wire_max_delay() -> u8 {
    higanr_core::frame::MAX_DELAY
}

/// ホストの語 `0x40 + f`（取得のフレーム f）。
fn hw(f: u32) -> u16 {
    0x4000 | f as u16
}

#[test]
fn the_host_press_switches_lead_frames_ahead_and_fills_or_drops_its_own_words() {
    let mut host = Rollback::new(rb_cfg(0, 1, 8));
    let mut g = Script::new(|_, _| false);
    let mut step = |host: &mut Rollback, f: u32| {
        feed(
            host,
            f + 1,
            f.saturating_sub(10).max(1),
            &[0x2000 | f as u16],
        );
        host.capture(hw(f)).unwrap();
        assert!(matches!(host.input_point(&mut g), Step::Ran { .. }), "f{f}");
        host.post_present(&mut g);
    };
    for f in 0..4 {
        step(&mut host, f);
    }
    let f1 = 4 + SWITCH_LEAD;
    assert_eq!(host.press_delay(3), DelayPress::Scheduled { frame: f1 });
    assert_eq!(
        host.build_input().lat,
        Some(LatChange {
            frame: f1,
            delay: 3
        })
    );
    assert_eq!((host.cur_delay(), host.shown_delay()), (1, 3));
    for f in 4..=f1 + 2 {
        step(&mut host, f);
    }
    // apply_frame の表（手で数えた）: f < F1 は f + 1、F1 は F1 + 3。その間の F1 + 1・F1 + 2 は直前の入力ワードで埋める。
    let inp = host.build_input();
    let at = |a: u32| inp.words[(a - inp.base) as usize];
    assert_eq!(inp.base, f1 + 2 - 10, "ack は 10 フレーム遅れ");
    for f in inp.base - 1..f1 {
        assert_eq!(at(f + 1), hw(f), "f{f}: F の前から apply_frame が動いた");
    }
    assert_eq!(at(f1 + 1), hw(f1 - 1), "隙間を直前の語で埋めていない");
    assert_eq!(at(f1 + 2), hw(f1 - 1));
    assert_eq!(at(f1 + 3), hw(f1));
    assert_eq!(at(f1 + 5), hw(f1 + 2));
    assert_eq!(host.stats().d_filled, 2);
    assert_eq!(host.cur_delay(), 3);
    // フレーム F1 + 3 の取得の前に ← ← で 1 へ（2 回目の押下は 1 回目の予定が届く前。届いていない予定は効かずに消え、
    // F2 + 1 まで今の D 3 のまま）。
    let next = f1 + 3;
    let f2 = next + SWITCH_LEAD;
    assert_eq!(host.press_delay(2), DelayPress::Scheduled { frame: f2 });
    assert_eq!(
        host.press_delay(1),
        DelayPress::Scheduled { frame: f2 + 1 },
        "2 つ目の予定の F は前の予定の F より先"
    );
    assert_eq!(host.cur_delay(), 3, "届いていない予定で今の D が動いた");
    for f in next..=f2 + 5 {
        step(&mut host, f);
    }
    let s = host.stats();
    assert_eq!(
        (s.d_dropped, s.d_switches, s.d_cancelled),
        (2, 2, 1),
        "{s:?}"
    );
    let inp = host.build_input();
    let at = |a: u32| inp.words[(a - inp.base) as usize];
    // 取得 F2 − 1・F2 は D 3。F2 + 1・F2 + 2 は捨てる。
    assert_eq!(at(f2 + 2), hw(f2 - 1), "F2 の手前で D が動いた");
    assert_eq!(
        at(f2 + 3),
        hw(f2),
        "F2 の取得が D 3 でない（消えた予定の 2 で取った）"
    );
    assert_eq!(
        at(f2 + 4),
        hw(f2 + 3),
        "D 1 に下がった後、apply F2 + 4 は取得 F2 + 3 の語（F2 + 1・F2 + 2 は捨てる）"
    );
    let log: Vec<(u32, u8, u8)> = s.d_switch_log[..2]
        .iter()
        .map(|x| (x.frame, x.from, x.to))
        .collect();
    assert_eq!(log, vec![(f1, 1, 3), (f2 + 1, 3, 1)]);
    host.begin_seg(1);
    assert_eq!(host.seg_delay(), 1);
    host.begin_seg(2);
    assert_eq!(host.seg_delay(), 1, "押さなければ前の seg の D のまま");
    // seg 2 の最初の取得の前の押下は、その seg の始まりの D をこの場で替える。
    assert_eq!(host.press_delay(0), DelayPress::Scheduled { frame: 0 });
    assert_eq!(host.seg_delay(), 0);
    host.begin_seg(3);
    assert_eq!(host.seg_delay(), 0);
    // ゲストの押下は要求で、ゲスト自身の D は替わらない（D はホストの予定で決まる）。
    let mut guest = Rollback::new(rb_cfg(1, 1, 8));
    assert_eq!(guest.press_delay(2), DelayPress::Asked { seq: 1 });
    assert_eq!(
        guest.press_delay(2),
        DelayPress::Unchanged,
        "同じ値を重ねて要求した"
    );
    assert_eq!((guest.cur_delay(), guest.shown_delay()), (1, 2));
    guest.capture(0x11).unwrap();
    assert_eq!(
        guest.build_input().lat,
        Some(LatChange { frame: 1, delay: 2 }),
        "ゲストの要求は {{seq, x}}"
    );
    assert_eq!(guest.cur_delay(), 1);
}

// ---------------------------------------------------------------- マッチの途中の D

/// マッチの途中の押下: seg 0 で 1 → 2（ホスト）→ 0（ゲストの要求）→ 2（ホスト）、seg 1 で → 1（ゲスト）→ 0（ホスト）。
fn mid_match_presses() -> Vec<PressAt> {
    let p = |node, seg, frame, x| PressAt {
        node,
        seg,
        frame,
        x,
    };
    vec![
        p(0, 0, 40, 2),
        p(1, 0, 110, 0),
        p(0, 0, 180, 2),
        p(1, 1, 60, 1),
        p(0, 1, 140, 0),
    ]
}

/// マッチの途中で替える実行（場面の始まりのライフを上げて、seg を 400 フレーム前後にする）。
fn mid_match_scenario(ahead: u8, presses: Vec<PressAt>) -> Scenario {
    let mut sc = Scenario::rollback(1, ahead, 3);
    sc.life = 30;
    sc.presses = presses;
    sc
}

fn run_mid(
    label: &str,
    sc: &Scenario,
    fate: &mut dyn FnMut(&SendInfo) -> Fate,
    tot: &mut Totals,
) -> Outcome {
    let out = run(sc, fate);
    assert!(out.finished, "[{label}] 時間切れ");
    let scheds = [
        schedules_of(&out.nodes[0].rb.stats(), sc.segs),
        schedules_of(&out.nodes[1].rb.stats(), sc.segs),
    ];
    let rf = reference_from_schedules(&scheds, sc.segs, sc.life);
    expect_ok(label, sc, &out, &rf, tot);
    out
}

/// 予定の便（ホストの lat が F > 0）だけを、最初の `k` 通落とす。
fn drop_first_schedules(k: u32) -> impl FnMut(&SendInfo) -> Fate {
    let mut n = 0;
    move |i| {
        if i.from == 0 && i.lat.is_some_and(|l| l.frame > 0) {
            n += 1;
            if n <= k {
                return Fate::Drop;
            }
        }
        Fate::Deliver
    }
}

/// 予定の便を 1 通おきに `ms` 遅らせる（後の便に追い越され、古い F の便が後から着く）か、全部重ねる。
fn disturb_schedules(delay_ms: Option<u64>, dup_ms: Option<u64>) -> impl FnMut(&SendInfo) -> Fate {
    let mut n = 0u32;
    move |i| {
        if i.from == 0 && i.lat.is_some_and(|l| l.frame > 0) {
            n += 1;
            if let Some(d) = delay_ms
                && n % 2 == 1
            {
                return Fate::Delay(d);
            }
            if let Some(d) = dup_ms {
                return Fate::Dup(d);
            }
        }
        Fate::Deliver
    }
}

/// ゲストの要求の便（ゲストの lat）だけを、最初の `k` 通落とす（`u32::MAX` なら全部）。
fn drop_first_wishes(k: u32) -> impl FnMut(&SendInfo) -> Fate {
    let mut n = 0;
    move |i| {
        if i.from == 1 && i.lat.is_some() {
            n += 1;
            if n <= k {
                return Fate::Drop;
            }
        }
        Fate::Deliver
    }
}

#[test]
fn d_changes_mid_match_by_both_sides_match_the_schedule_reference() {
    let mut tot = Totals::default();
    let (mut late, mut filled, mut dropped, mut by_guest, mut dups) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut same_frame_runs = 0;
    for ahead in [8u8, 2] {
        let sc = mid_match_scenario(ahead, mid_match_presses());
        let mut fates: Vec<(String, SimFate)> = vec![
            ("clean".into(), Box::new(clean)),
            ("burst".into(), Box::new(burst_loss())),
            (
                "schedules lost x30".into(),
                Box::new(drop_first_schedules(30)),
            ),
            (
                "schedules reordered".into(),
                Box::new(disturb_schedules(Some(60), None)),
            ),
            (
                "schedules duplicated".into(),
                Box::new(disturb_schedules(None, Some(30))),
            ),
            ("wishes lost x10".into(), Box::new(drop_first_wishes(10))),
        ];
        for seed in 0..3u64 {
            fates.push((format!("jitter {seed}"), Box::new(jitter(seed, 40))));
            fates.push((format!("lossy {seed}"), Box::new(lossy(seed))));
        }
        for (name, fate) in &mut fates {
            let label = format!("R={ahead} {name}");
            let out = run_mid(&label, &sc, &mut **fate, &mut tot);
            let (h, g) = (out.nodes[0].rb.stats(), out.nodes[1].rb.stats());
            let hl = &h.d_switch_log[..h.d_switches as usize];
            let gl = &g.d_switch_log[..g.d_switches as usize];
            for x in gl {
                assert!(x.frame >= x.wire_frame, "[{label}] {x:?}");
            }
            if name == "clean" {
                let key = |l: &[higanr_core::rollback::DSwitch]| -> Vec<(u16, u32, u8, u8)> {
                    l.iter().map(|x| (x.seg, x.frame, x.from, x.to)).collect()
                };
                assert_eq!(key(hl), key(gl), "[{label}] 両台の予定が違う");
                assert_eq!(g.d_late, 0, "[{label}]");
                same_frame_runs += 1;
            }
            assert!(h.d_switches >= 4, "[{label}] 押下が予定にならない: {hl:?}");
            late += g.d_late;
            filled += h.d_filled + g.d_filled;
            dropped += h.d_dropped + g.d_dropped;
            by_guest += hl.iter().filter(|x| x.by_peer).count() as u32;
            dups += h.wish_dup;
        }
    }
    // ゲストが R いっぱい先にいるネットワーク（時刻合わせを切り、ゲストが 400 ms 先に始める。R 16・片道 50 ms）: 予定が届くときゲストは
    // F を過ぎて取っているので、受けた後の取得から替える。
    for seed in [None, Some(3u64)] {
        let mut sc = mid_match_scenario(16, mid_match_presses());
        sc.cfg = [
            rb_cfg(0, 1, 16).with_time_sync(false),
            rb_cfg(1, 1, 16).with_time_sync(false),
        ];
        sc.start_ms = [400, 0];
        sc.load_ms = [40, 40];
        sc.one_way_ms = 50;
        let label = format!("guest far ahead {seed:?}");
        let out = match seed {
            None => run_mid(&label, &sc, &mut clean, &mut tot),
            Some(s) => run_mid(&label, &sc, &mut lossy(s), &mut tot),
        };
        let g = out.nodes[1].rb.stats();
        assert!(
            g.d_late > 0,
            "[{label}] 遅れて替えていない（検査の前提）: {g:?}"
        );
        late += g.d_late;
    }
    eprintln!(
        "試合の途中の D: {tot:?} 遅れて替えた {late} 埋めた {filled} 捨てた {dropped} ゲストの要求からの予定 {by_guest} 要求の重ね {dups}"
    );
    // 網羅: 上げた・下げた・遅れて替えた・ゲストの要求が予定になった・同じ要求の重ねを捨てた。
    assert_eq!(same_frame_runs, 2);
    assert!(late > 0, "ゲストが遅れて替えた例が無い");
    assert!(
        filled > 0 && dropped > 0,
        "埋めた {filled} 捨てた {dropped}"
    );
    assert!(by_guest > 0, "ゲストの要求が予定になった例が無い");
    assert!(dups > 0, "同じ要求の重ねを見た例が無い");
    assert!(tot.stats.resims > 0);
}

#[test]
fn the_host_holds_a_press_until_the_guest_knows_the_seg_d() {
    let mut tot = Totals::default();
    let presses = vec![PressAt {
        node: 0,
        seg: 1,
        frame: 2,
        x: 2,
    }];
    let mut sc = mid_match_scenario(8, presses);
    sc.load_ms = [40, 440];
    for (name, fate) in [
        ("clean", &mut clean as &mut dyn FnMut(&SendInfo) -> Fate),
        ("lossy", &mut lossy(7)),
    ] {
        let out = run_mid(name, &sc, fate, &mut tot);
        let h = out.nodes[0].rb.stats();
        assert!(
            h.d_held >= 1,
            "[{name}] ゲートの前の押下が無い（検査の前提）: {h:?}"
        );
        let sw = &h.d_switch_log[..h.d_switches as usize];
        assert!(
            sw.iter().any(|x| x.seg == 1 && x.to == 2),
            "[{name}] ゲートが開いた後に予定を出さない: {sw:?}"
        );
    }
}

#[test]
fn a_guest_wish_is_clamped_by_the_host_and_expires_without_its_value() {
    let mut tot = Totals::default();
    let presses = vec![PressAt {
        node: 1,
        seg: 0,
        frame: 30,
        x: higanr_core::delay_pick::AUTO,
    }];
    let sc = mid_match_scenario(8, presses);
    let mut with_lat = 0u32;
    let mut fate = |i: &SendInfo| {
        if i.from == 1 && i.lat.is_some() {
            with_lat += 1;
        }
        Fate::Deliver
    };
    let out = run_mid("wish clamped", &sc, &mut fate, &mut tot);
    let (h, g) = (out.nodes[0].rb.stats(), out.nodes[1].rb.stats());
    assert_eq!((g.wish_sent, g.wish_expired), (1, 1), "{g:?}");
    assert_eq!(h.wish_taken, 1, "{h:?}");
    assert!(h.wish_dup > 0, "{h:?}");
    assert!(
        out.nodes[0].rb.auto_on(),
        "ゲストの auto でホストの旗が立たない"
    );
    // 要求を 1 回取った分 + 旗が立った後の seg の始まり（seg 1・2）の最初の取得ごと（推奨値が無いので何も出さない）。
    assert_eq!(h.d_auto, 1 + (sc.segs - 1), "{h:?}");
    assert_eq!(
        h.d_switches,
        0,
        "推奨値が無いのに予定を出した（9 を D にした）: {:?}",
        &h.d_switch_log[..h.d_switches as usize]
    );
    assert_eq!(
        out.nodes[1].rb.cur_delay(),
        out.nodes[0].rb.cur_delay(),
        "D が食い違う"
    );
    // 取得 1 回に 1 通以上送るので、諦めるまでに WISH_FRAMES − 1 通以上。諦めた後は載せない（数が上限の数倍で止まる）。
    let lim = u32::from(higanr_core::lockstep::WISH_FRAMES);
    assert!(with_lat >= lim - 1 && with_lat < 4 * lim, "{with_lat}");
}

#[test]
fn d_beyond_two_up_to_eight_matches_the_schedule_reference() {
    let p = |node, seg, frame, x| PressAt {
        node,
        seg,
        frame,
        x,
    };
    let presses = vec![
        p(0, 0, 40, 5),
        p(1, 0, 110, 8),
        p(0, 0, 180, 3),
        p(1, 1, 60, 0),
        p(0, 1, 140, 8),
    ];
    let mut tot = Totals::default();
    let mut reached = std::collections::BTreeSet::new();
    for ahead in [8u8, 2, 16] {
        let mut sc = Scenario::rollback(0, ahead, 3);
        sc.life = 30;
        sc.presses = presses.clone();
        let mut fates: Vec<(String, SimFate)> = vec![
            ("clean".into(), Box::new(clean)),
            ("burst".into(), Box::new(burst_loss())),
            (
                "schedules lost x20".into(),
                Box::new(drop_first_schedules(20)),
            ),
            (
                "schedules reordered".into(),
                Box::new(disturb_schedules(Some(60), None)),
            ),
        ];
        for seed in 0..2u64 {
            fates.push((format!("jitter {seed}"), Box::new(jitter(seed, 40))));
            fates.push((format!("lossy {seed}"), Box::new(lossy(seed))));
        }
        for (name, fate) in &mut fates {
            let label = format!("R={ahead} {name}");
            let out = run_mid(&label, &sc, &mut **fate, &mut tot);
            for n in &out.nodes {
                let s = n.rb.stats();
                reached.extend(s.d_switch_log[..s.d_switches as usize].iter().map(|x| x.to));
            }
            assert_eq!(out.nodes[0].rb.cur_delay(), 8, "[{label}] 最後の D");
            assert_eq!(out.nodes[1].rb.cur_delay(), 8, "[{label}] ゲストの最後の D");
        }
    }
    assert_eq!(
        reached.into_iter().collect::<Vec<_>>(),
        vec![0, 3, 5, 8],
        "予定の値が 3 以上へ行っていない"
    );
    assert!(tot.stats.resims > 0);
}

#[test]
fn a_guest_auto_wish_is_not_a_value_and_a_huge_net_schedule_does_not_overflow() {
    use higanr_core::delay_pick::AUTO;
    let mut guest = Rollback::new(rb_cfg(1, 1, 8));
    assert_eq!(guest.press_delay(AUTO), DelayPress::Asked { seq: 1 });
    assert_eq!(guest.shown_delay(), 1, "auto の要求を値（9）として出した");
    assert_eq!(guest.press_delay(AUTO), DelayPress::Unchanged);
    guest.capture(0x11).unwrap();
    assert_eq!(
        guest.build_input().lat,
        Some(LatChange {
            frame: 1,
            delay: AUTO
        }),
        "auto の要求は lat の delay の欄に AUTO"
    );
    // ネットワークから来る F が上限の手前: 足し算であふれず、予定は受け入れる。
    let mut host = Rollback::new(rb_cfg(0, 1, 8));
    host.capture(0x21).unwrap();
    let mut inp = host.build_input();
    inp.lat = Some(LatChange {
        frame: u32::MAX - 1,
        delay: 5,
    });
    let mut g2 = Rollback::new(rb_cfg(1, 1, 8));
    assert!(matches!(g2.on_input(&inp), RecvOutcome::Accepted { .. }));
    let mut host = Rollback::new(rb_cfg(0, 1, 8));
    let mut guest = Rollback::new(rb_cfg(1, 1, 8));
    host.set_recommend(Some(Recommend::flat(3)));
    host.capture(0x21).unwrap();
    assert!(matches!(
        guest.on_input(&host.build_input()),
        RecvOutcome::Accepted { .. }
    ));
    assert_eq!(guest.press_delay(AUTO), DelayPress::Asked { seq: 1 });
    guest.capture(0x31).unwrap();
    assert!(matches!(
        host.on_input(&guest.build_input()),
        RecvOutcome::Accepted { .. }
    ));
    assert!(host.auto_on(), "ゲストの auto でホストの旗が立たない");
    assert_eq!(host.shown_delay(), 3, "旗が立ったとき推奨値にする");
    assert_eq!(guest.press_delay(6), DelayPress::Asked { seq: 2 });
    assert!(matches!(
        host.on_input(&guest.build_input()),
        RecvOutcome::Accepted { .. }
    ));
    assert!(!host.auto_on(), "ゲストの数字でホストの旗が降りない");
    assert_eq!(host.shown_delay(), 6);
}

#[test]
fn auto_and_presses_during_the_load_set_the_next_seg_head_d() {
    let p = |node, seg, frame, x| PressAt {
        node,
        seg,
        frame,
        x,
    };
    let auto = higanr_core::delay_pick::AUTO;
    let mut sc = Scenario::rollback(1, 8, 5);
    sc.life = 30;
    sc.rec_seq = flat_recs(&[4, 5, 6, 6, 3]);
    sc.presses = vec![
        p(1, 0, 20, auto),
        p(0, 1, 50, 7),
        p(0, 3, LOAD_PRESS, 1),
        p(1, 3, LOAD_PRESS, 5),
        p(0, 4, LOAD_PRESS, auto),
    ];
    sc.expect_heads = Some(vec![1, 5, 7, 1, 3]);
    let mut tot = Totals::default();
    let mut fates: Vec<(String, SimFate)> = vec![
        ("clean".into(), Box::new(clean)),
        ("burst".into(), Box::new(burst_loss())),
        (
            "schedules lost x10".into(),
            Box::new(drop_first_schedules(10)),
        ),
    ];
    for seed in 0..3u64 {
        fates.push((format!("jitter {seed}"), Box::new(jitter(seed, 40))));
        fates.push((format!("lossy {seed}"), Box::new(lossy(seed))));
    }
    for (name, fate) in &mut fates {
        let out = run_mid(name, &sc, &mut **fate, &mut tot);
        let (h, g) = (out.nodes[0].rb.stats(), out.nodes[1].rb.stats());
        let log = &h.d_switch_log[..h.d_switches as usize];
        let seen: Vec<(u16, u8, bool)> = log.iter().map(|x| (x.seg, x.to, x.by_peer)).collect();
        assert_eq!(
            seen,
            vec![(0, 4, true), (1, 7, false), (3, 5, true)],
            "[{name}] 途中の予定: seg 0 はゲストの auto（推奨値 4）、seg 1 はホストの 7、seg 3 は読み込みの間に出したゲストの要求の 5"
        );
        assert_eq!(
            h.d_head_sets, 3,
            "[{name}] 始まりの D を直に替えた数: {h:?}"
        );
        assert_eq!(h.d_auto, 4, "[{name}] auto を使った数: {h:?}");
        assert!(
            out.nodes[0].rb.auto_on(),
            "[{name}] 旗は seg 4 で立ったまま"
        );
        assert_eq!(
            (out.nodes[0].rb.cur_delay(), out.nodes[1].rb.cur_delay()),
            (3, 3),
            "[{name}]"
        );
        assert_eq!(g.d_head_sets, 0, "ゲストは始まりの D を替えない");
    }
}

fn flat_recs(v: &[u8]) -> Vec<Recommend> {
    v.iter().map(|&r| Recommend::flat(r)).collect()
}

/// seg ごとの往復（ms）から rollback の推奨値の組を作る（本物の `link::Connected::recommend_pair_now` と同じ式）。
fn rtt_recs(v: &[u32]) -> Vec<Recommend> {
    v.iter()
        .map(|&ms| Recommend::from_rtt(Mode::Rollback, Some(ms)).expect("往復は測れている"))
        .collect()
}

/// ホスト 1 台だけで seg の始まりを順に通し、各 seg の始まりの D を返す。`presses` は `(seg, x)`: その seg の読み込みの間に押す
/// （seg 0 は最初の取得の前）。ネットワークは要らない。
fn host_heads(d0: u8, recs: &[Recommend], presses: &[(usize, u8)]) -> (Vec<u8>, Rollback) {
    let mut host = Rollback::new(rb_cfg(0, d0, 8));
    let mut heads = Vec::new();
    for (k, &r) in recs.iter().enumerate() {
        if k > 0 {
            host.begin_seg(k as u16);
        }
        host.set_recommend(Some(r));
        for &(_, x) in presses.iter().filter(|p| p.0 == k) {
            host.press_delay(x);
        }
        host.capture(0x21).unwrap();
        heads.push(host.seg_delay());
    }
    (heads, host)
}

#[test]
fn the_auto_head_d_does_not_flicker_when_the_rtt_wanders_around_a_boundary() {
    // rollback の推奨値の 1 と 2 の境目（100 ms で 1・101 ms で 2。「−5 ms」の側は 105 ms で 1・106 ms で 2）の上で揺れる往復。
    let rtts = [100u32, 101, 105, 106, 100, 101, 100, 95];
    let mut sc = Scenario::rollback(1, 8, rtts.len() as u32);
    sc.life = 30;
    sc.rec_seq = rtt_recs(&rtts);
    sc.presses = vec![PressAt {
        node: 0,
        seg: 0,
        frame: 10,
        x: higanr_core::delay_pick::AUTO,
    }];
    let heads = vec![1u8, 1, 1, 2, 2, 2, 2, 1];
    sc.expect_heads = Some(heads.clone());
    let mut tot = Totals::default();
    let mut fates: Vec<(String, SimFate)> = vec![("clean".into(), Box::new(clean))];
    fates.push(("jitter 0".into(), Box::new(jitter(0, 40))));
    fates.push(("lossy 0".into(), Box::new(lossy(0))));
    for (name, fate) in &mut fates {
        let out = run_mid(name, &sc, &mut **fate, &mut tot);
        let (h, g) = (out.nodes[0].rb.stats(), out.nodes[1].rb.stats());
        assert_eq!(h.d_auto, 8, "[{name}] 9 の押下 1 + seg の始まり 7: {h:?}");
        assert_eq!(h.d_auto_held, 4, "[{name}] 据え置いた数: {h:?}");
        assert_eq!(
            h.d_head_sets, 2,
            "[{name}] 始まりの D を替えた数（1→2 と 2→1）: {h:?}"
        );
        assert_eq!(g.d_auto_held, 0, "[{name}] ゲストは見直さない");
        assert!(out.nodes[0].rb.auto_on(), "[{name}] 旗は立ったまま");
    }
    // 対照（規則なし）: 同じ往復を素の推奨値だけで引くと、境目の往復で D がマッチごとに揺れる。
    let raw_heads: Vec<u8> = rtt_recs(&rtts).iter().map(|r| r.raw).collect();
    let flips = raw_heads.windows(2).filter(|w| w[0] != w[1]).count();
    let held_flips = heads.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        flips >= 4 && held_flips == 2,
        "素の推奨値は {raw_heads:?} で {flips} 回揺れる。変わりにくくした列 {heads:?} は {held_flips} 回"
    );
}

#[test]
fn the_host_head_uses_the_press_at_once_then_holds_and_a_number_lowers_the_flag() {
    // 往復 101 ms: 素 2・「−5 ms」の側 1。今 1 から 9 で 2（すぐ）、次の始まりは 100 ms（素 1）が 1 回目なので 2 のまま、2 回目で 1。
    let rtt = [101u32, 100, 100, 101, 100, 100];
    let (heads, host) = host_heads(1, &rtt_recs(&rtt), &[(0, higanr_core::delay_pick::AUTO)]);
    assert_eq!(
        heads,
        vec![1, 2, 1, 1, 1, 1],
        "seg 0 の始まりは 1（9 の 2 は 12 フレーム先の予定ではなくゲートの前に持つ）、seg 1 の始まりは 9 の 2、seg 2 で下がる、101 ms は上げない"
    );
    let s = host.stats();
    assert_eq!(
        s.d_auto_held, 2,
        "据え置いたのは seg 1（素 1 が低い 1 回目）と seg 3（素 2 を上げの余裕で）: {s:?}"
    );
    // 数字で旗が降りる: D 3 から 9（往復 167 ms: 素 3）、seg 1 の始まりは 100 ms の低い 1 回目で 3 のまま。seg 2 の読み込みの間にホストが 3 を押して
    // 旗を降ろすと、その後は 100 ms が何回続いても 3 のまま。seg 4 の読み込みの間の 9 は素の推奨値（100 ms で 1）をすぐ使う。
    let rtt = [167u32, 100, 100, 100, 100];
    let (heads, host) = host_heads(
        3,
        &rtt_recs(&rtt),
        &[
            (0, higanr_core::delay_pick::AUTO),
            (2, 3),
            (4, higanr_core::delay_pick::AUTO),
        ],
    );
    assert_eq!(heads, vec![3, 3, 3, 3, 1]);
    assert!(host.auto_on(), "最後の 9 で旗が立つ");
}

#[test]
fn prefill_and_prediction_read_the_seg_head_d_after_a_mid_seg_switch() {
    let r = |a: u32| 0x3000 | a as u16;
    let mut host = Rollback::new(Config::rollback(Seat::P1, 1, 16));
    let mut g = Script::new(|_, _| false);
    feed(&mut host, 1, 1, &[r(1), r(2), r(3), r(4)]);
    assert_eq!(
        host.press_delay(8),
        DelayPress::Scheduled { frame: SWITCH_LEAD }
    );
    for f in 0..=20u32 {
        host.capture(hw(f)).unwrap();
        assert!(matches!(host.input_point(&mut g), Step::Ran { .. }), "f{f}");
        host.post_present(&mut g);
    }
    assert_eq!((host.front(), host.frame(), host.cur_delay()), (5, 21, 8));
    assert_eq!(host.stats().d_filled, 7, "13〜19 を埋める");
    let predicted: Vec<u16> = g
        .log
        .iter()
        .filter(|(k, f, _, _)| *k == TickKind::Live && *f >= SWITCH_LEAD)
        .map(|&(_, _, _, p2)| p2)
        .collect();
    assert_eq!(predicted.len(), 9);
    assert!(
        predicted.iter().all(|&p| p == r(4)),
        "予測が直前の確定の語でない: {predicted:x?}"
    );
    let words: Vec<u16> = (5..=21).map(r).collect();
    feed(&mut host, 5, 1, &words);
    host.post_present(&mut g);
    assert_eq!(host.front(), 21);
    // 確定の行（席 1P: p1 = 自分）: frame 0 は穴埋め、1〜12 は取得 f − 1 の入力ワード（D 1）、13〜19 は埋めた入力ワード（取得 11）、20 は取得 12（D 8 の最初の入力ワード）、p2 は相手の入力ワード。
    let want: Vec<(u32, u16, u16)> = (0..=20u32)
        .map(|f| match f {
            0 => (0, 0, 0),
            1..=12 => (f, hw(f - 1), r(f)),
            13..=19 => (f, hw(11), r(f)),
            _ => (f, hw(12), r(f)),
        })
        .collect();
    assert_eq!(g.confirmed, want);
}

// ---------------------------------------------------------------- 否定の検査（壊した相手を必ず落とす）

#[test]
fn sabotage_skipping_restore_is_caught() {
    let (d, r) = (1u8, 8u8);
    let rf = reference(d, 4);
    let mut caught = 0;
    for seed in 0..6u64 {
        let mut sc = Scenario::rollback(d, r, 4);
        sc.sabotage = Sabotage::SkipRestore;
        let out = run(&sc, &mut lossy(seed));
        if judge("skip-restore", &sc, &out, &rf).is_err() {
            caught += 1;
        }
    }
    assert_eq!(caught, 6, "書き戻しを飛ばしても落ちない実行がある");
}

#[test]
fn sabotage_skipping_outside_restore_is_caught() {
    let (d, r) = (1u8, 8u8);
    let rf = reference(d, 4);
    let mut caught = 0;
    let mut released = 0;
    for seed in 0..40u64 {
        let mut sc = Scenario::rollback(d, r, 4);
        let ok = run(&sc, &mut lossy(seed));
        judge("outside-ok", &sc, &ok, &rf).expect("壊していない実行は通る");
        released += ok
            .nodes
            .iter()
            .map(|n| n.rb.stats().holds_released)
            .sum::<u32>();
        sc.sabotage = Sabotage::SkipOutside;
        let bad = run(&sc, &mut lossy(seed));
        if judge("outside-skip", &sc, &bad, &rf).is_err() {
            caught += 1;
        }
    }
    assert!(
        released > 0,
        "予測の境目が消える実行が 1 つも無い（検査が空振り）"
    );
    assert!(
        caught > 0,
        "つなぎを戻さなくても落ちない（released {released}）"
    );
}

#[test]
fn judge_catches_a_single_wrong_word() {
    let rf = reference(1, 2);
    let sc = Scenario::rollback(1, 8, 2);
    let mut out = run(&sc, &mut clean);
    judge("base", &sc, &out, &rf).expect("元の実行は通る");
    let mid = out.nodes[0].game.confirmed.len() / 2;
    out.nodes[0].game.confirmed[mid].2 ^= 0x0001;
    assert!(judge("wrong-word", &sc, &out, &rf).is_err());
    let mut out = run(&sc, &mut clean);
    out.nodes[1].game.checks[1].rng_count ^= 1;
    assert!(judge("wrong-check", &sc, &out, &rf).is_err());
}

// ---------------------------------------------------------------- 台本で決めた小さな実行（状態機械の単体）

/// 入力ワードで境目を決めるウィンドウ。`gate(frame, p2)` が真のとき要求が立つ。更新の種類と (frame, p1, p2) を記録する。
struct Script<G: Fn(u32, u16) -> bool> {
    gate: G,
    log: Vec<(TickKind, u32, u16, u16)>,
    confirmed: Vec<(u32, u16, u16)>,
}

impl<G: Fn(u32, u16) -> bool> Script<G> {
    fn new(gate: G) -> Self {
        Script {
            gate,
            log: Vec::new(),
            confirmed: Vec::new(),
        }
    }
}

impl<G: Fn(u32, u16) -> bool> Window for Script<G> {
    fn save(&mut self, _: u32) {}
    fn restore(&mut self, _: u32) {}
    fn tick(&mut self, t: Tick) -> TickReport {
        self.log.push((t.kind, t.frame, t.p1, t.p2));
        TickReport {
            boundary: (self.gate)(t.frame, t.p2),
        }
    }
    fn check_values(&mut self) -> CheckValues {
        CheckValues::default()
    }
    fn confirmed(&mut self, f: u32, p1: u16, p2: u16) {
        self.confirmed.push((f, p1, p2));
    }
}

fn feed(rb: &mut Rollback, base: u32, ack: u32, words: &[u16]) {
    let mut w = [0u16; wire::MAX_WORDS];
    w[..words.len()].copy_from_slice(words);
    let inp = Input {
        seg: rb.seg(),
        ack,
        base,
        lat: None,
        count: words.len() as u8,
        words: w,
    };
    assert!(matches!(rb.on_input(&inp), RecvOutcome::Accepted { .. }));
}

#[test]
fn confirmed_boundary_ends_the_scene() {
    let trigger = 0x1000;
    let mut rb = Rollback::new(Config::rollback(Seat::P1, 0, 8));
    let mut g = Script::new(move |_, p2| p2 == trigger);
    feed(&mut rb, 0, 0, &[0x0500, trigger]);
    rb.capture(0xA0).unwrap();
    assert!(matches!(
        rb.input_point(&mut g),
        Step::Ran {
            frame: 0,
            real: true,
            confirmed: true
        }
    ));
    assert_eq!(rb.post_present(&mut g), After::Saved);
    rb.capture(0xA1).unwrap();
    assert!(matches!(
        rb.input_point(&mut g),
        Step::Ran {
            frame: 1,
            real: true,
            confirmed: true
        }
    ));
    assert!(rb.scene_ended());
    assert_eq!(rb.input_point(&mut g), Step::SceneEnd);
    assert_eq!(rb.post_present(&mut g), After::Nothing);
    assert_eq!(g.confirmed, vec![(0, 0xA0, 0x0500), (1, 0xA1, trigger)]);
}

#[test]
fn spurious_prediction_boundary_waits_and_recovers() {
    let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8));
    let mut g = Script::new(|frame, p2| frame >= 2 && p2 == 0);
    for f in 0..3u32 {
        rb.capture(0xB0 + f as u16).unwrap();
        let step = rb.input_point(&mut g);
        if f < 2 {
            assert!(
                matches!(step, Step::Ran { frame, .. } if frame == f),
                "{step:?}"
            );
        } else {
            assert!(
                matches!(
                    step,
                    Step::Ran {
                        frame: 2,
                        real: false,
                        confirmed: false
                    }
                ),
                "{step:?}"
            );
        }
        rb.post_present(&mut g);
    }
    assert!(rb.holding());
    assert_eq!((rb.frame(), rb.front()), (3, 1));
    rb.capture(0xB3).unwrap();
    assert_eq!(rb.input_point(&mut g), Step::Wait(Wait::Boundary));
    assert_eq!(rb.input_point(&mut g), Step::Wait(Wait::Boundary));
    let ticks_before = g.log.len();
    assert_eq!(rb.post_present(&mut g), After::Nothing);
    assert_eq!(g.log.len(), ticks_before, "待っている間は更新しない");
    feed(&mut rb, 1, 0, &[0x0777, 0x0888]);
    let step = rb.input_point(&mut g);
    assert!(
        matches!(
            step,
            Step::Ran {
                frame: 3,
                real: false,
                confirmed: false
            }
        ),
        "要求が消えたので frame 3 に進む: {step:?}"
    );
    assert!(!rb.holding() && !rb.scene_ended());
    let s = rb.stats();
    assert_eq!((s.holds_set, s.holds_released, s.stall_rewinds), (1, 1, 1));
    let conf: Vec<_> = g
        .log
        .iter()
        .filter(|(k, ..)| *k == TickKind::Confirmed)
        .map(|&(_, f, _, p2)| (f, p2))
        .collect();
    assert_eq!(conf, vec![(1, 0x0777), (2, 0x0888)]);
    assert_eq!(
        g.log.last().map(|l| (l.0, l.1, l.3)),
        Some((TickKind::Live, 3, 0x0888))
    );
}

#[test]
fn real_boundary_found_in_resim_ends_the_scene_and_drops_the_rest() {
    let mut rb = Rollback::new(Config::rollback(Seat::P1, 0, 8));
    let mut g = Script::new(|_, p2| p2 == 0x0300);
    for f in 0..4u32 {
        rb.capture(f as u16).unwrap();
        assert!(matches!(
            rb.input_point(&mut g),
            Step::Ran { real: false, .. }
        ));
        rb.post_present(&mut g);
    }
    assert_eq!((rb.frame(), rb.front()), (4, 0));
    feed(&mut rb, 0, 0, &[0x0100, 0x0300, 0x0200, 0x0200]);
    let a = rb.post_present(&mut g);
    assert!(rb.scene_ended());
    assert_eq!(g.confirmed, vec![(0, 0, 0x0100), (1, 1, 0x0300)]);
    assert_eq!((rb.front(), rb.frame()), (2, 2), "境目より先の N は捨てる");
    assert!(matches!(
        a,
        After::Resim {
            from: 0,
            confirmed: 2,
            predicted: 0
        }
    ));
    assert_eq!(rb.stats().discarded_ticks, 2);
    rb.begin_seg(1);
    assert_eq!(rb.frame(), 0);
    assert!(rb.build_prev_input().is_some());
}

#[test]
fn outputs_during_resim_are_rolled_back() {
    struct Se {
        se: u32,
        saved: u32,
        depth: i32,
        max_depth: i32,
    }
    impl Window for Se {
        fn save(&mut self, _: u32) {}
        fn restore(&mut self, _: u32) {}
        fn tick(&mut self, t: Tick) -> TickReport {
            if t.p2 & 1 != 0 {
                self.se += 1;
            }
            TickReport::default()
        }
        fn check_values(&mut self) -> CheckValues {
            CheckValues::default()
        }
        fn resim_begin(&mut self) {
            self.depth += 1;
            self.max_depth = self.max_depth.max(self.depth);
            self.saved = self.se;
        }
        fn resim_end(&mut self) {
            self.depth -= 1;
            self.se = self.saved;
        }
    }
    let mut rb = Rollback::new(Config::rollback(Seat::P1, 0, 8));
    let mut g = Se {
        se: 0,
        saved: 0,
        depth: 0,
        max_depth: 0,
    };
    for round in 0..5u32 {
        for _ in 0..3 {
            rb.capture(0).unwrap();
            assert!(matches!(rb.input_point(&mut g), Step::Ran { .. }));
            rb.post_present(&mut g);
        }
        feed(&mut rb, round * 3, 0, &[1, 1, 1]);
        rb.post_present(&mut g);
        assert_eq!(rb.front(), rb.frame());
    }
    assert_eq!((g.depth, g.max_depth), (0, 1));
    // 入力の地点で鳴った SE だけが残る（進め直しの中の 15 回は全部捨てられる）。
    assert_eq!(g.se, 12);
    assert_eq!(rb.stats().resims, 5);
}

// ---------------------------------------------------------------- 時刻合わせ

/// 更新を走らせた枠のうち、seg の始めの `warm` フレームと終わりの `tail` フレームを除いた N − F の (中央値, 最大値, 枠の数)。
/// 始めを除くのは、揃うまでの間を「揃った後」に混ぜないため。
fn ahead_stats(n: &Node, rf: &RefRun, warm: u32, tail: u32) -> (u32, u32, usize) {
    let mut v: Vec<u32> = n
        .ahead_log
        .iter()
        .filter(|&&(s, f, _)| f >= warm && f + tail < rf.seg_len[s as usize])
        .map(|&(_, _, a)| a)
        .collect();
    v.sort_unstable();
    (
        v.get(v.len() / 2).copied().unwrap_or(0),
        v.last().copied().unwrap_or(0),
        v.len(),
    )
}

/// 読み込みの長さの違う 2 台（台 0 が 30 ms・台 1 が 250 ms）で `segs` 個の seg を回す。台 0 が先に seg に入る。
/// ネットワークは綺麗で片道は `ow` ms。`sync` は両端の時刻合わせ。
fn uneven_load_run(sync: bool, d: u8, r: u8, ow: u64, segs: u32) -> (Scenario, Outcome, RefRun) {
    let rf = reference(d, segs);
    let cfg = |i| rb_cfg(i, d, r).with_time_sync(sync);
    let mut sc = Scenario::new([cfg(0), cfg(1)], segs);
    sc.start_ms = [30, 250];
    sc.load_ms = [30, 250];
    sc.one_way_ms = ow;
    let out = run(&sc, &mut clean);
    (sc, out, rf)
}

/// `uneven_load_run` の、両端が格子を寄せられるバージョン（b-spin 同士。往復の判定は片道の 2 倍）。
fn uneven_load_run_glide(d: u8, r: u8, ow: u64, segs: u32) -> (Scenario, Outcome, RefRun) {
    let rf = reference(d, segs);
    let mut sc = Scenario::new([rb_cfg(0, d, r), rb_cfg(1, d, r)], segs);
    sc.start_ms = [30, 250];
    sc.load_ms = [30, 250];
    sc.one_way_ms = ow;
    sc.glide = [true, true];
    let out = run(&sc, &mut clean);
    (sc, out, rf)
}

/// 揃った後（seg の始まりの 50 フレームを除く）の先の側の N − F の中央値の上限: 片道ぶんのフレーム数 + 時刻合わせの
/// 閾値の残り（L < 1.5 フレーム）+ 端数の 1。
fn shrunk_bound(ow: u64) -> u32 {
    ow.div_ceil(FRAME_MS) as u32 + 2
}

const WARM: u32 = 50;
const TAIL: u32 = 12;

#[test]
fn time_sync_shrinks_the_lead_of_the_early_starter() {
    for (d, r) in [(1u8, 8u8), (0, 8), (2, 6)] {
        for ow in [5u64, 20, 40] {
            let (sc, out, rf) = uneven_load_run(true, d, r, ow, 3);
            judge(&format!("sync D={d} R={r} ow={ow}"), &sc, &out, &rf).expect("列と状態が一致");
            let lead = ahead_stats(&out.nodes[0], &rf, WARM, TAIL);
            let back = ahead_stats(&out.nodes[1], &rf, WARM, TAIL);
            let (s0, s1) = (out.nodes[0].rb.stats(), out.nodes[1].rb.stats());
            eprintln!(
                "D={d} R={r} ow={ow}: 先の側 N−F (中央, 最大, 枠数) {lead:?} waits_sync {} / 後ろの側 {back:?} waits_sync {}",
                s0.waits_sync, s1.waits_sync
            );
            assert!(lead.2 >= 100, "揃った後の枠が少ない {lead:?}");
            assert!(
                lead.0 <= shrunk_bound(ow),
                "D={d} R={r} ow={ow}: 先の側の N − F の中央 {} が縮んでいない（上限 {}）",
                lead.0,
                shrunk_bound(ow)
            );
            assert!(s0.waits_sync > 0, "先の側が待っていない");
            assert!(
                s1.waits_sync < s0.waits_sync,
                "後ろの側の待ち {} が先の側 {} より多い",
                s1.waits_sync,
                s0.waits_sync
            );
            // 後ろの側は先の側より 1 を超えて先へ進まない（片道が 1 フレームより長ければ揃っていても両者が予測し、
            // 先の側が 0 まで縮み後ろの側が 1 残ることがある）。
            assert!(
                back.0 <= lead.0 + 1,
                "後ろの側 {back:?} が先の側 {lead:?} より先"
            );
        }
    }
}

#[test]
#[ignore = "診断。値は assert しない"]
fn glide_uneven_load_table() {
    for (d, r) in [(1u8, 8u8), (2, 6)] {
        for ow in [3u64, 7, 12] {
            let (sc, out, rf) = uneven_load_run_glide(d, r, ow, 3);
            judge(&format!("glide D={d} ow={ow}"), &sc, &out, &rf).expect("列と状態が一致");
            let (_, base, rf0) = uneven_load_run(true, d, r, ow, 3);
            let lead = ahead_stats(&out.nodes[0], &rf, WARM, TAIL);
            let lead0 = ahead_stats(&base.nodes[0], &rf0, WARM, TAIL);
            let st = |o: &Outcome| {
                (
                    o.nodes[0].rb.stats().waits_sync,
                    o.nodes[1].rb.stats().waits_sync,
                )
            };
            eprintln!(
                "D={d} ow={ow}: 先の側 N−F (中央, 最大, 枠数) 寄せ {lead:?} 今 {lead0:?}・待ち 寄せ {:?} 今 {:?}・寄せ 最大 ppm {} / {}",
                st(&out),
                st(&base),
                out.nodes[0].glide_max_ppm,
                out.nodes[1].glide_max_ppm
            );
        }
    }
}

#[test]
fn without_time_sync_the_early_starter_stays_near_r() {
    for (d, r) in [(1u8, 8u8), (0, 8), (2, 6)] {
        for ow in [5u64, 20, 40] {
            let (sc, out, rf) = uneven_load_run(false, d, r, ow, 3);
            judge(&format!("nosync D={d} R={r} ow={ow}"), &sc, &out, &rf).expect("列と状態が一致");
            let lead = ahead_stats(&out.nodes[0], &rf, WARM, TAIL);
            assert!(lead.2 >= 100, "{lead:?}");
            let (sc_on, out_on, rf_on) = uneven_load_run(true, d, r, ow, 3);
            judge("sync", &sc_on, &out_on, &rf_on).expect("列と状態が一致");
            let on = ahead_stats(&out_on.nodes[0], &rf_on, WARM, TAIL);
            eprintln!(
                "D={d} R={r} ow={ow}: 先の側 N−F の中央 時刻合わせなし {} / あり {}",
                lead.0, on.0
            );
            assert!(
                lead.0 >= u32::from(r).saturating_sub(3) && lead.0 >= on.0 + 3,
                "D={d} R={r} ow={ow}: 切っても張り付かない（なし {lead:?} / あり {on:?}）"
            );
            for n in &out.nodes {
                assert_eq!(n.rb.stats().waits_sync, 0);
            }
        }
    }
}

#[test]
fn time_sync_does_not_wait_on_a_balanced_noisy_link() {
    let (d, r) = (1u8, 8u8);
    let rf = reference(d, 4);
    let mut waits = 0u32;
    let mut frames = 0usize;
    for seed in 0..12u64 {
        for lossy_net in [false, true] {
            let mut sc = Scenario::rollback(d, r, 4);
            sc.start_ms = [30, 37];
            sc.load_ms = [30, 37];
            let out = if lossy_net {
                run(&sc, &mut lossy(seed))
            } else {
                run(&sc, &mut jitter(seed, 40))
            };
            judge(
                &format!("balanced seed={seed} lossy={lossy_net}"),
                &sc,
                &out,
                &rf,
            )
            .expect("列と状態が一致");
            let off = sc.cfg.map(|c| c.with_time_sync(false));
            let mut sc_off = Scenario::new(off, 4);
            sc_off.start_ms = sc.start_ms;
            sc_off.load_ms = sc.load_ms;
            let out_off = if lossy_net {
                run(&sc_off, &mut lossy(seed))
            } else {
                run(&sc_off, &mut jitter(seed, 40))
            };
            for (n, m) in out.nodes.iter().zip(&out_off.nodes) {
                waits += n.rb.stats().waits_sync;
                frames += n.ahead_log.len();
                let (on_med, off_med) = (
                    ahead_stats(n, &rf, 30, TAIL).0,
                    ahead_stats(m, &rf, 30, TAIL).0,
                );
                assert!(
                    on_med <= off_med + 1,
                    "seed={seed} lossy={lossy_net}: 時刻合わせありの中央 {on_med} が、なしの {off_med} より増えた"
                );
            }
        }
    }
    eprintln!("釣り合った揺れるネットワーク: 時刻合わせの待ち {waits} / 更新した枠 {frames}");
    assert!(waits * 100 < frames as u32, "待ちすぎ {waits} / {frames}");
}

#[test]
fn time_sync_waits_one_frame_at_a_time_with_spacing() {
    let (d, r) = (1u8, 16u8);
    let rf = reference(d, 3);
    let mut sc = Scenario::rollback(d, r, 3);
    sc.start_ms = [30, 700];
    sc.load_ms = [30, 700];
    sc.one_way_ms = 20;
    let out = run(&sc, &mut clean);
    judge("spacing", &sc, &out, &rf).expect("間隔の違反が無く、列が一致");
    let s = out.nodes[0].rb.stats();
    let back = out.nodes[1].rb.stats().waits_sync;
    assert!(s.waits_sync >= 8, "大きい差なのに待ちが少ない {s:?}");
    // 片道が 1 フレームを大きく越えるネットワーク（RTT 40 ms）では、揃った後に後ろの側が 1 回待つことがある。
    // 先の側の待ちの 1 割に満たなければ符号の取り違えではない。
    assert!(
        back * 10 < s.waits_sync,
        "後ろの側の待ち {back} が先の側 {} に近い",
        s.waits_sync
    );
}

#[test]
fn time_sync_is_off_in_delay_mode() {
    let c = Config::delay_mode(SEATS[0], 2).with_time_sync(true);
    assert!(!c.time_sync());
    assert!(Config::rollback(SEATS[0], 2, 8).time_sync());
    assert!(
        !Config::rollback(SEATS[0], 2, 8)
            .with_time_sync(false)
            .time_sync()
    );
    // 両端が delay 方式（読み込みの長さが違っても待たない）。
    let rf = reference(2, 3);
    let mut sc = Scenario::new([c, Config::delay_mode(SEATS[1], 2).with_time_sync(true)], 3);
    sc.start_ms = [30, 250];
    sc.load_ms = [30, 250];
    let out = run(&sc, &mut clean);
    judge("delay-mode sync", &sc, &out, &rf).expect("列と状態が一致");
    assert_eq!(out.nodes[0].rb.stats().waits_sync, 0);
    assert_eq!(out.nodes[1].rb.stats().waits_sync, 0);
}

// ---------------------------------------------------------------- 時刻合わせ: 時計のずれ（片方の周期が速い）

/// 長い seg 用のライフ（seg が数千フレーム続く）。
const DRIFT_LIFE: u8 = 200;

/// 台 0 の周期が台 1 より短い（台 0 が先へずれる）実行。`start_ms[1]` が台 0 から見た最初のずれ（台 0 が先）。
fn drift_run(
    sync: bool,
    d: u8,
    ow: u64,
    period_us: [u64; 2],
    start_ms: [u64; 2],
    segs: u32,
) -> (Scenario, Outcome, RefRun) {
    let rf = reference_life(d, segs, DRIFT_LIFE);
    let cfg = |i| rb_cfg(i, d, 8).with_time_sync(sync);
    let mut sc = Scenario::new([cfg(0), cfg(1)], segs);
    sc.start_ms = start_ms;
    sc.load_ms = [30, 30];
    sc.one_way_ms = ow;
    sc.period_us = period_us;
    sc.life = DRIFT_LIFE;
    sc.max_ms = 4_000_000;
    let out = run(&sc, &mut clean);
    (sc, out, rf)
}

fn predicted_share(n: &Node) -> (u32, u32) {
    let s = n.rb.stats();
    (
        s.live_predicted,
        s.live_confirmed + s.live_unconfirmed_real + s.live_predicted,
    )
}

/// 周期（µs）の組: 台 0 が速い。`fast` は 1 分で約 98 ms ずれる誇張、`real` は花映塚の全画面（ゲーム自身の時計 60.0146 fps）と
/// ウィンドウ（60.000 fps）の差（実測、1 分に約 14.6 ms）に近い 4 µs の差、`same` は同じ速さ。
const PERIODS: [(&str, [u64; 2]); 3] = [
    ("fast", [16_640, 16_667]),
    ("real", [16_663, 16_667]),
    ("same", [16_667, 16_667]),
];

/// 定数（`SYNC_ALPHA` / `SYNC_THRESHOLD` / `SYNC_BIAS` / `SYNC_UNIT`）を選び直すための表。
/// `cargo test --release -p higanr_core --test rollback_sim sweep_ -- --ignored --nocapture`
#[test]
#[ignore = "定数を選び直すときの表。値は assert しない"]
fn sweep_drift_table() {
    for d in [1u8, 2, 0] {
        for ow in [3u64, 7, 12] {
            for (name, p) in PERIODS {
                for sync in [false, true] {
                    let mut tot = [(0u32, 0u32); 2];
                    let mut waits = [0u32; 2];
                    let mut lead = [(0u64, 0i64, i32::MIN, i32::MAX); 2];
                    let mut ok = true;
                    for s in [0u64, 5, 10, 13] {
                        let (sc, out, rf) = drift_run(sync, d, ow, p, [0, s], 2);
                        if !out.finished {
                            ok = false;
                        }
                        if let Err(e) = judge("x", &sc, &out, &rf) {
                            panic!("{e}");
                        }
                        for i in 0..2 {
                            let (a, b) = predicted_share(&out.nodes[i]);
                            tot[i].0 += a;
                            tot[i].1 += b;
                            let st = out.nodes[i].rb.stats();
                            waits[i] += st.waits_sync;
                            lead[i].0 += u64::from(st.sync_samples);
                            lead[i].1 += st.sync_lead_sum;
                            lead[i].2 = lead[i].2.max(st.sync_lead_max);
                            lead[i].3 = lead[i].3.min(st.sync_lead_min);
                        }
                    }
                    let u = f64::from(SYNC_UNIT);
                    let lead_txt = lead.map(|(n, sum, mx, mn)| {
                        format!(
                            "標本 {n} 平均 {:.2} 最大 {:.2} 最小 {:.2}",
                            sum as f64 / n.max(1) as f64 / u,
                            f64::from(mx) / u,
                            f64::from(mn) / u
                        )
                    });
                    eprintln!(
                        "D={d} ow={ow} {name} sync={sync} fin={ok}: 台0 予測 {}/{} ({:.1}%) 台1 予測 {}/{} ({:.1}%) 待ち {:?} 見積もり 台0 [{}] 台1 [{}]",
                        tot[0].0,
                        tot[0].1,
                        100.0 * f64::from(tot[0].0) / f64::from(tot[0].1.max(1)),
                        tot[1].0,
                        tot[1].1,
                        100.0 * f64::from(tot[1].0) / f64::from(tot[1].1.max(1)),
                        waits,
                        lead_txt[0],
                        lead_txt[1]
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "定数を選び直すときの表。値は assert しない"]
fn sweep_jitter_table() {
    for d in [0u8, 1, 2] {
        sweep_jitter_for_d(d);
    }
}

fn sweep_jitter_for_d(d: u8) {
    let rf = reference_life(d, 2, DRIFT_LIFE);
    for ow in [3u64, 5, 8] {
        for jmax in [0u64, 4, 10, 20, 30] {
            for (name, p) in [PERIODS[1], PERIODS[2]] {
                let mut tot = [(0u32, 0u32); 2];
                let mut waits = [0u32; 2];
                let mut resims = [0u32; 2];
                let mut worst = [0u32; 2];
                let mut n = 0;
                for s in [0u64, 4, 8, 12, 16] {
                    for seed in 0..3u64 {
                        let cfg = |i| rb_cfg(i, d, 8);
                        let mut sc = Scenario::new([cfg(0), cfg(1)], 2);
                        sc.start_ms = [0, s];
                        sc.load_ms = [30, 30];
                        sc.one_way_ms = ow;
                        sc.period_us = p;
                        sc.life = DRIFT_LIFE;
                        sc.max_ms = 4_000_000;
                        let out = run(&sc, &mut jitter(seed, jmax));
                        judge("j", &sc, &out, &rf).expect("列と状態が一致");
                        n += 1;
                        for i in 0..2 {
                            let (a, b) = predicted_share(&out.nodes[i]);
                            tot[i].0 += a;
                            tot[i].1 += b;
                            waits[i] += out.nodes[i].rb.stats().waits_sync;
                            resims[i] += out.nodes[i].rb.stats().resims;
                            worst[i] = worst[i].max(out.nodes[i].worst_recent_waits);
                        }
                    }
                }
                eprintln!(
                    "J d={d} ow={ow} jit={jmax} {name} runs={n}: 台0 予測 {}/{} ({:.1}%) 台1 予測 {}/{} ({:.1}%) 待ち {:?} resims {:?} 60 更新中の待ちの最大 {:?}",
                    tot[0].0,
                    tot[0].1,
                    100.0 * f64::from(tot[0].0) / f64::from(tot[0].1.max(1)),
                    tot[1].0,
                    tot[1].1,
                    100.0 * f64::from(tot[1].0) / f64::from(tot[1].1.max(1)),
                    waits,
                    resims,
                    worst
                );
            }
        }
    }
}

// ---------------------------------------------------------------- 格子の寄せ

#[derive(Default, Debug, Clone)]
struct GlideAgg {
    runs: u32,
    pred: [(u32, u32); 2],
    /// 台ごとの時刻合わせの待ち（`Wait::Sync`）の数と、60 更新の中の最大。
    waits: [u32; 2],
    /// そのうち seg の始まりの 300 フレームを過ぎてからの待ち。
    waits_steady: [u32; 2],
    worst: [u32; 2],
    resims: [u32; 2],
    /// 台ごとの格子を寄せた枠の数・変化の最大（ppm）・寄せた量の絶対値の合計（µs）・正味（µs）。
    glide_frames: [u32; 2],
    glide_max_ppm: [i32; 2],
    glide_max_ppm_steady: [i32; 2],
    glide_max_step_ppm: [i32; 2],
    glide_max_step_ppm_steady: [i32; 2],
    glide_abs_us: [u64; 2],
    glide_net_us: [i64; 2],
    /// 台ごとの見積もり（`Stats::sync_lead_*`）の標本の数と合計（平均が 2L の偏りを見るため）。
    lead_n: [u64; 2],
    lead_sum: [i64; 2],
    updates: u32,
    unfinished: u32,
    /// 寄せる判定を呼んだ数・「寄せてよい」だった数・寄せ幅が 1 枠に 5,000 ppm を越えて変わった数・そのうち前の枠で待ちがあった数。
    glide_calls: [u32; 2],
    gate_open: [u32; 2],
    big_steps: [u32; 2],
    big_steps_after_wait: [u32; 2],
    big_steps_other: [u32; 2],
}

impl GlideAgg {
    fn pct(&self, i: usize) -> f64 {
        100.0 * f64::from(self.pred[i].0) / f64::from(self.pred[i].1.max(1))
    }

    fn waits_per_min(&self) -> f64 {
        let node_min = f64::from(self.updates) / 3600.0;
        f64::from(self.waits[0] + self.waits[1]) / node_min.max(1e-9)
    }

    /// 見積もりの平均（平均 = 「先にいるフレーム数の 2 倍」の値。標本ごとの平均の平均）。
    fn mean_avg(&self, i: usize) -> f64 {
        2.0 * self.lead_sum[i] as f64 / self.lead_n[i].max(1) as f64 / f64::from(SYNC_UNIT)
    }

    fn text(&self) -> String {
        format!(
            "先 予測 {}/{} ({:.2}%) 後 予測 {}/{} ({:.2}%) 待ち {:?} うち定常 {:?} ({:.2} 回/台・分) 1 秒の最大 {:?} resims {:?} 平均 [{:.2}, {:.2}] 寄せ 枠 {:?} 最大 ppm {:?} 定常の最大 ppm {:?} 1 枠の変化の最大 ppm {:?} 定常の 1 枠の変化の最大 ppm {:?} 絶対 µs {:?} 正味 µs {:?} 判定が開いていた {:?}/{:?} 大きい変化 {:?}（待ちの直後 {:?}・待ちも seg の始まりも無し {:?}）{}",
            self.pred[0].0,
            self.pred[0].1,
            self.pct(0),
            self.pred[1].0,
            self.pred[1].1,
            self.pct(1),
            self.waits,
            self.waits_steady,
            self.waits_per_min(),
            self.worst,
            self.resims,
            self.mean_avg(0),
            self.mean_avg(1),
            self.glide_frames,
            self.glide_max_ppm,
            self.glide_max_ppm_steady,
            self.glide_max_step_ppm,
            self.glide_max_step_ppm_steady,
            self.glide_abs_us,
            self.glide_net_us,
            self.gate_open,
            self.glide_calls,
            self.big_steps,
            self.big_steps_after_wait,
            self.big_steps_other,
            if self.unfinished > 0 {
                format!(" 未完 {}", self.unfinished)
            } else {
                String::new()
            }
        )
    }
}

/// D・片道・時計・揺れ・寄せられる台の組で、最初の位相 5 つ × seed（揺れが 0 なら 1 つ）の実行を `segs` seg ずつ回して集計する。
fn glide_sweep(
    d: u8,
    ow: u64,
    period_us: [u64; 2],
    jmax: u64,
    glide: [bool; 2],
    params: &GlideParams,
    segs: u32,
) -> GlideAgg {
    let rf = reference_life(d, segs, DRIFT_LIFE);
    let seeds = if jmax == 0 { 1 } else { 3 };
    let mut agg = GlideAgg::default();
    for s in [0u64, 4, 8, 12, 16] {
        for seed in 0..seeds {
            let cfg = |i| rb_cfg(i, d, 8);
            let mut sc = Scenario::new([cfg(0), cfg(1)], segs);
            sc.start_ms = [0, s];
            sc.load_ms = [30, 30];
            sc.one_way_ms = ow;
            sc.period_us = period_us;
            sc.life = DRIFT_LIFE;
            sc.max_ms = 4_000_000;
            sc.glide = glide;
            sc.glide_params = *params;
            sc.pong_jitter_ms = Some(jmax);
            let out = run(&sc, &mut jitter(seed, jmax));
            judge("glide", &sc, &out, &rf).expect("列と状態が一致");
            agg.runs += 1;
            if !out.finished {
                agg.unfinished += 1;
            }
            for (i, n) in out.nodes.iter().enumerate() {
                let (a, b) = predicted_share(n);
                agg.pred[i].0 += a;
                agg.pred[i].1 += b;
                agg.updates += b;
                let st = n.rb.stats();
                agg.waits[i] += st.waits_sync;
                agg.waits_steady[i] += n.sync_steps_steady;
                agg.resims[i] += st.resims;
                agg.worst[i] = agg.worst[i].max(n.worst_recent_waits);
                agg.glide_frames[i] += n.glide_frames;
                agg.glide_max_ppm[i] = agg.glide_max_ppm[i].max(n.glide_max_ppm);
                agg.glide_max_ppm_steady[i] =
                    agg.glide_max_ppm_steady[i].max(n.glide_max_ppm_steady);
                agg.glide_max_step_ppm[i] = agg.glide_max_step_ppm[i].max(n.glide_max_step_ppm);
                agg.glide_max_step_ppm_steady[i] =
                    agg.glide_max_step_ppm_steady[i].max(n.glide_max_step_ppm_steady);
                agg.glide_abs_us[i] += n.glide_abs_us;
                agg.glide_net_us[i] += n.glide_net_us;
                agg.glide_calls[i] += n.glide_calls;
                agg.gate_open[i] += n.gate_open;
                agg.big_steps[i] += n.big_steps;
                agg.big_steps_after_wait[i] += n.big_steps_after_wait;
                agg.big_steps_other[i] += n.big_steps_other;
                agg.lead_n[i] += u64::from(st.sync_samples);
                agg.lead_sum[i] += st.sync_lead_sum;
            }
        }
    }
    agg
}

/// 格子を寄せる形の表。環境変数 `GLIDE_SEGS`（既定 4）・`GLIDE_ONLY`（`d1` のように D を絞る）で変えられる。
/// `cargo test --release -p higanr_core --test rollback_sim glide_table -- --ignored --nocapture`
#[test]
#[ignore = "格子を寄せる形の表。値は assert しない"]
fn glide_table() {
    let segs: u32 = std::env::var("GLIDE_SEGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    // 定数を掃くための上書き（1/256 単位は SYNC_UNIT = 256 のまま）。
    let env_i32 = |k: &str, d: i32| -> i32 {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let params = GlideParams {
        gain_ppm: env_i32("GLIDE_GAIN", GlideParams::DEFAULT.gain_ppm),
        max_ppm: env_i32("GLIDE_MAX", GlideParams::DEFAULT.max_ppm),
        slew_ppm: env_i32("GLIDE_SLEW", GlideParams::DEFAULT.slew_ppm),
        dead: env_i32("GLIDE_DEAD", GlideParams::DEFAULT.dead),
        stale_frames: GlideParams::DEFAULT.stale_frames,
        rtt_on_pct: env_i32("GLIDE_RTT_ON", GlideParams::DEFAULT.rtt_on_pct as i32) as u32,
        rtt_off_pct: env_i32("GLIDE_RTT_OFF", GlideParams::DEFAULT.rtt_off_pct as i32) as u32,
    };
    eprintln!("GlideParams {params:?}");
    let env_list = |k: &str, d: &[u64]| -> Vec<u64> {
        match std::env::var(k) {
            Ok(v) => v.split(',').filter_map(|x| x.parse().ok()).collect(),
            Err(_) => d.to_vec(),
        }
    };
    let combos: [(&str, [bool; 2]); 4] = [
        ("game/game(今)", [false, false]),
        ("先=b-spin/後=game", [true, false]),
        ("先=game/後=b-spin", [false, true]),
        ("b-spin/b-spin", [true, true]),
    ];
    for d in [1u8, 0, 2] {
        if let Ok(only) = std::env::var("GLIDE_ONLY")
            && only != format!("d{d}")
        {
            continue;
        }
        for &ow in &env_list("GLIDE_OW", &[3, 7]) {
            for (pname, p) in [PERIODS[1], PERIODS[0]] {
                if std::env::var("GLIDE_P").is_ok_and(|v| v != pname) {
                    continue;
                }
                for &jmax in &env_list("GLIDE_JIT", &[0, 4, 10, 30]) {
                    for (cname, g) in combos {
                        if std::env::var("GLIDE_COMBO").is_ok_and(|v| !cname.contains(v.as_str())) {
                            continue;
                        }
                        let agg = glide_sweep(d, ow, p, jmax, g, &params, segs);
                        eprintln!(
                            "G d={d} ow={ow} {pname} jit={jmax} [{cname}] runs={}: {}",
                            agg.runs,
                            agg.text()
                        );
                    }
                }
            }
        }
    }
}

/// 台 0 をゲーム自身の時計（429.5 秒の折り返し・時計の大きさで変わる周期）、台 1 を b-spin（60.000 fps）にした実行。
/// 折り返しが実行の 120 秒目に来るよう時計を `GAME_CLOCK_WRAP_US − 120 秒` から数え始め、`segs` 個の seg（各 98 秒前後）を回す。
/// `glide` は寄せられる台（台 0 はゲーム自身の時計なので `[false, false]` か `[false, true]`）。
fn game_clock_run(
    d: u8,
    ow: u64,
    jmax: u64,
    glide: [bool; 2],
    start1_ms: u64,
    segs: u32,
    seed: u64,
) -> Outcome {
    let rf = reference_life(d, segs, DRIFT_LIFE);
    let mut sc = Scenario::new([rb_cfg(0, d, 8), rb_cfg(1, d, 8)], segs);
    sc.start_ms = [0, start1_ms];
    sc.load_ms = [30, 30];
    sc.one_way_ms = ow;
    sc.life = DRIFT_LIFE;
    sc.max_ms = 4_000_000;
    sc.glide = glide;
    sc.pong_jitter_ms = Some(jmax);
    sc.period_us = [16_667, 16_667];
    sc.game_clock_offset_us = Some(GAME_CLOCK_WRAP_US - 120_000_000);
    let out = run(&sc, &mut jitter(seed, jmax));
    judge("game clock", &sc, &out, &rf).expect("列と状態が一致");
    assert!(out.finished);
    out
}

#[test]
#[ignore = "診断。値は assert しない"]
fn glide_game_clock_table() {
    for d in [1u8, 2] {
        for ow in [3u64, 7] {
            for jmax in [0u64, 4, 10] {
                for g in [[false, false], [false, true]] {
                    let (mut pred, mut waits, mut late, mut near_wrap, mut wraps) =
                        ([(0u32, 0u32); 2], [0u32; 2], [0u32; 2], [0u32; 2], 0u32);
                    for (s, seed) in [(0u64, 1u64), (8, 2), (13, 3)] {
                        let out = game_clock_run(d, ow, jmax, g, s, 7, seed);
                        for (i, n) in out.nodes.iter().enumerate() {
                            let (a, b) = predicted_share(n);
                            pred[i].0 += a;
                            pred[i].1 += b;
                            waits[i] += n.rb.stats().waits_sync;
                            late[i] += n.rb.stats().waits_sync_late;
                            for &w in &n.wait_times_us {
                                for &t0 in &out.nodes[0].wrap_times_us {
                                    if w >= t0 && w < t0 + 30_000_000 {
                                        near_wrap[i] += 1;
                                    }
                                }
                            }
                        }
                        wraps += out.nodes[0].wrap_times_us.len() as u32;
                    }
                    let pct = |x: (u32, u32)| 100.0 * f64::from(x.0) / f64::from(x.1.max(1));
                    eprintln!(
                        "GC d={d} ow={ow} jit={jmax} 寄せ {g:?}: 折り返し {wraps} 回・予測 台0 {:.2}% 台1 {:.2}%・待ち {waits:?}（seg の始まりの 300 フレーム以降 {late:?}・折り返しから 30 秒の間 {near_wrap:?}）",
                        pct(pred[0]),
                        pct(pred[1])
                    );
                }
            }
        }
    }
}

#[test]
fn glide_follows_the_game_clock_across_period_reversals_and_the_wrap() {
    let (d, ow, jmax) = (1u8, 7u64, 4u64);
    let base = game_clock_run(d, ow, jmax, [false, false], 8, 7, 2);
    let on = game_clock_run(d, ow, jmax, [false, true], 8, 7, 2);
    let wraps = on.nodes[0].wrap_times_us.clone();
    assert_eq!(wraps.len(), 2, "折り返しが 2 回来る実行でない: {wraps:?}");
    let late = |o: &Outcome, i: usize| o.nodes[i].rb.stats().waits_sync_late;
    assert!(
        late(&base, 0) + late(&base, 1) >= 4,
        "今の形の定常の待ちが少ない（窓が効いていない）: {} / {}",
        late(&base, 0),
        late(&base, 1)
    );
    assert_eq!(late(&on, 1), 0, "寄せられる側が止まった");
    // ゲーム自身の側の待ちは全部、折り返しの 30 秒以内（seg の始まりを除く）。
    let near: Vec<u32> = wraps
        .iter()
        .map(|&t0| {
            on.nodes[0]
                .wait_times_us
                .iter()
                .filter(|&&w| w >= t0 && w < t0 + 30_000_000)
                .count() as u32
        })
        .collect();
    assert!(
        near.iter().all(|&n| n <= 1),
        "折り返し 1 回につき 1 回を越えた {near:?}"
    );
    assert!(
        late(&on, 0) <= near.iter().sum::<u32>(),
        "折り返しの外でゲーム自身の側が止まった {} 回（折り返しの近く {near:?}）",
        late(&on, 0)
    );
    for i in 0..2 {
        let (a, b) = predicted_share(&on.nodes[i]);
        assert!(
            f64::from(a) <= 0.01 * f64::from(b),
            "台 {i} の予測 {a} / {b}"
        );
    }
}

#[test]
fn glide_replaces_the_drift_stops_without_more_prediction() {
    let p = GlideParams::DEFAULT;
    for ow in [3u64, 7] {
        for jmax in [0u64, 4] {
            if (ow, jmax) == (3, 4) {
                continue; // 実行時間（debug の検査）のため、片道 3 ms は揺れなしだけ
            }
            let base = glide_sweep(1, ow, PERIODS[1].1, jmax, [false, false], &p, 1);
            assert!(
                base.waits_steady[0] + base.waits_steady[1] >= 5,
                "ow={ow} jit={jmax}: 今の形の定常の待ちが少ない（窓が効いていない）: {}",
                base.text()
            );
            for g in [[true, true], [true, false], [false, true]] {
                let a = glide_sweep(1, ow, PERIODS[1].1, jmax, g, &p, 1);
                let tag = format!("ow={ow} jit={jmax} 寄せる台 {g:?}");
                assert_eq!(a.waits_steady, [0, 0], "{tag}: {}", a.text());
                assert!(a.pct(0) <= 1.0, "{tag}: 先の側の予測 {:.2}%", a.pct(0));
                assert!(
                    a.pct(0) <= base.pct(0) + 0.05 && a.pct(1) <= base.pct(1) + 0.05,
                    "{tag}: 予測が今の形（{:.2}% / {:.2}%）より増えた {:.2}% / {:.2}%",
                    base.pct(0),
                    base.pct(1),
                    a.pct(0),
                    a.pct(1)
                );
                #[allow(clippy::needless_range_loop)] // 台の添字で 2 つの集計と g を引く
                for i in 0..2 {
                    assert!(
                        a.glide_max_step_ppm[i] <= p.slew_ppm,
                        "{tag}: 台 {i} の寄せ幅が 1 枠に {} ppm 変わった",
                        a.glide_max_step_ppm[i]
                    );
                    assert!(
                        a.glide_max_ppm[i] <= p.max_ppm,
                        "{tag}: 台 {i} の寄せ幅 {} ppm が上限を越えた",
                        a.glide_max_ppm[i]
                    );
                    if g[i] {
                        assert!(
                            a.glide_frames[i] * 2 > a.pred[i].1,
                            "{tag}: 台 {i} がほとんど寄せていない {} 枠 / {} 更新",
                            a.glide_frames[i],
                            a.pred[i].1
                        );
                    } else {
                        assert_eq!(a.glide_frames[i], 0, "{tag}: 寄せられない台 {i} が寄せた");
                    }
                }
            }
        }
    }
}

#[test]
fn glide_stays_off_on_a_long_link_and_at_d0_and_works_just_inside_the_gate() {
    let p = GlideParams::DEFAULT;
    let per = PERIODS[1].1;
    // 往復 36 ms（片道 18 ms・揺れなし）: 寄せられる台でも寄せず、寄せない形と全く同じ数。
    let off = glide_sweep(1, 18, per, 0, [false, false], &p, 1);
    let on = glide_sweep(1, 18, per, 0, [true, true], &p, 1);
    assert_eq!(on.glide_frames, [0, 0], "{}", on.text());
    assert_eq!(
        (on.pred, on.waits, on.resims),
        (off.pred, off.waits, off.resims),
        "往復が長いネットワークで数が変わった"
    );
    // 往復 32 ms（片道 16 ms）: 寄せて、先の側の予測が大きく減る。
    let off = glide_sweep(1, 16, per, 0, [false, false], &p, 1);
    let on = glide_sweep(1, 16, per, 0, [true, true], &p, 1);
    assert!(
        on.glide_frames[0] > 1_000 && on.glide_frames[1] > 1_000,
        "{}",
        on.text()
    );
    assert!(
        off.pct(0) > 50.0 && on.pct(0) < 10.0 && on.pct(1) < 10.0,
        "寄せなし {:.1}% / 寄せ {:.1}% / {:.1}%",
        off.pct(0),
        on.pct(0),
        on.pct(1)
    );
    // D = 0: 寄せない（今の形と同じ数）。
    let off = glide_sweep(0, 7, per, 4, [false, false], &p, 1);
    let on = glide_sweep(0, 7, per, 4, [true, true], &p, 1);
    assert_eq!(on.glide_frames, [0, 0], "{}", on.text());
    assert_eq!((on.pred, on.waits), (off.pred, off.waits));
}

/// 台 0 が `start` ms 先に始まる（最初の位相差）4 通りの実行の、各台の (予測した更新, 生きた更新, 待ち) の合計。
fn drift_totals(
    sync: bool,
    d: u8,
    ow: u64,
    period_us: [u64; 2],
) -> ([(u32, u32); 2], [u32; 2], u32) {
    let mut tot = [(0u32, 0u32); 2];
    let mut waits = [0u32; 2];
    let mut runs = 0;
    for s in [0u64, 5, 10, 13] {
        let (sc, out, rf) = drift_run(sync, d, ow, period_us, [0, s], 1);
        judge(
            &format!("drift sync={sync} D={d} ow={ow} s={s}"),
            &sc,
            &out,
            &rf,
        )
        .expect("列と状態が一致");
        assert!(out.finished);
        for (i, n) in out.nodes.iter().enumerate() {
            let (a, b) = predicted_share(n);
            tot[i].0 += a;
            tot[i].1 += b;
            waits[i] += n.rb.stats().waits_sync;
        }
        runs += 1;
    }
    (tot, waits, runs)
}

#[test]
fn time_sync_keeps_the_leader_from_predicting_under_clock_drift() {
    for ow in [3u64, 7] {
        for (name, p) in [PERIODS[0], PERIODS[1]] {
            let (on, w_on, runs) = drift_totals(true, 1, ow, p);
            let (off, w_off, _) = drift_totals(false, 1, ow, p);
            let pct = |x: (u32, u32)| 100.0 * f64::from(x.0) / f64::from(x.1.max(1));
            eprintln!(
                "ow={ow} {name} 実行 {runs} 本: 先の側の予測 時刻合わせなし {}/{} ({:.1}%) あり {}/{} ({:.1}%)・待ち なし {w_off:?} あり {w_on:?}・後ろの側の予測 あり {}/{}",
                off[0].0,
                off[0].1,
                pct(off[0]),
                on[0].0,
                on[0].1,
                pct(on[0]),
                on[1].0,
                on[1].1
            );
            assert!(on[0].1 > 20_000, "更新の数が少ない {on:?}");
            assert!(
                pct(on[0]) <= 3.0,
                "ow={ow} {name}: 先の側の予測が {:.1}%（あり。上限 3%）",
                pct(on[0])
            );
            assert!(
                pct(off[0]) >= 40.0,
                "ow={ow} {name}: 切っても予測が少ない {:.1}%。上の検査が何も見ていない",
                pct(off[0])
            );
            assert_eq!(on[1].0, 0, "後ろの側が予測した");
            assert_eq!(w_on[1], 0, "後ろの側が待った");
            assert!(w_on[0] > 0, "先の側が待っていない");
            assert_eq!(w_off, [0, 0]);
        }
    }
}

#[test]
fn time_sync_does_not_wait_when_the_early_one_is_inside_the_no_prediction_zone() {
    for d in [1u8, 2] {
        for s in [0u64, 4, 8] {
            let (sc, out, rf) = drift_run(true, d, 7, PERIODS[2].1, [0, s], 1);
            judge(&format!("zone D={d} s={s}"), &sc, &out, &rf).expect("列と状態が一致");
            for n in &out.nodes {
                let st = n.rb.stats();
                assert_eq!(
                    (st.waits_sync, st.live_predicted),
                    (0, 0),
                    "D={d} s={s}: 帯の外なのに待った・予測した {st:?}"
                );
            }
        }
    }
}

#[test]
fn time_sync_does_not_fight_on_a_mildly_jittery_link() {
    let d = 1u8;
    let rf = reference_life(d, 1, DRIFT_LIFE);
    let (mut waits, mut frames) = (0u32, 0u32);
    let (mut pred_on, mut pred_off) = (0u32, 0u32);
    for (_, p) in PERIODS {
        for s in [0u64, 4, 8, 12] {
            for seed in 0..2u64 {
                for sync in [true, false] {
                    let cfg = |i| rb_cfg(i, d, 8).with_time_sync(sync);
                    let mut sc = Scenario::new([cfg(0), cfg(1)], 1);
                    sc.start_ms = [0, s];
                    sc.load_ms = [30, 30];
                    sc.one_way_ms = 5;
                    sc.period_us = p;
                    sc.life = DRIFT_LIFE;
                    sc.max_ms = 4_000_000;
                    let out = run(&sc, &mut jitter(seed, 4));
                    judge("mild jitter", &sc, &out, &rf).expect("列と状態が一致");
                    for n in &out.nodes {
                        let (a, b) = predicted_share(n);
                        if sync {
                            pred_on += a;
                            waits += n.rb.stats().waits_sync;
                            frames += b;
                        } else {
                            pred_off += a;
                        }
                    }
                }
            }
        }
    }
    eprintln!(
        "小さい揺れ: 待ち {waits} / 更新 {frames}・予測の合計 時刻合わせなし {pred_off} あり {pred_on}"
    );
    assert!(frames > 100_000);
    assert!(waits * 1000 < frames, "待ちすぎ {waits} / {frames}");
    assert!(
        pred_on * 2 < pred_off,
        "予測が減っていない {pred_on} / {pred_off}"
    );
}

#[test]
fn time_sync_does_not_push_d0_into_the_both_predict_zone_and_keeps_d2_quiet() {
    let pct = |x: (u32, u32)| 100.0 * f64::from(x.0) / f64::from(x.1.max(1));
    for ow in [3u64, 7] {
        for (name, p) in [PERIODS[0], PERIODS[1]] {
            let (on, _, _) = drift_totals(true, 0, ow, p);
            let (off, _, _) = drift_totals(false, 0, ow, p);
            eprintln!(
                "D=0 ow={ow} {name}: 後ろの側の予測 時刻合わせなし {:.1}% あり {:.1}%",
                pct(off[1]),
                pct(on[1])
            );
            assert!(
                pct(on[1]) <= pct(off[1]) + 3.0,
                "D=0 ow={ow} {name}: 後ろの側の予測が {:.1}%（なし {:.1}%）",
                pct(on[1]),
                pct(off[1])
            );
            let (on2, w2, _) = drift_totals(true, 2, ow, p);
            assert!(
                pct(on2[0]) <= 1.0 && pct(on2[1]) <= 1.0,
                "D=2 ow={ow} {name}: 予測 {on2:?}"
            );
            assert_eq!(w2[1], 0, "D=2 の後ろの側が待った");
        }
    }
}
