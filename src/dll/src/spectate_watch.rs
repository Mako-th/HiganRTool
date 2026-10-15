//! 観戦者の側: ホストが流す記録（`wire::Record`）を組み立て、観戦者のゲームへ「両席とも相手の入力」として入れる入力の地点の判断
//! （pure）。対戦の `netplay::NetPlay` と並ぶもの（観戦者は対戦のセッション `Link` を作らず、ホストへ JOIN するだけ）。
//!
//! 受信スレッドは生のバイト列をリングへ書くだけ（`net_udp`）。状態機械は**ゲームのスレッド**が持つ: 入力の地点の始まりでリングを空に
//! して core の `Watcher`（`higanr_core::spectate`）へ渡し（JOIN の送り直し・ACK・組み立て）、組み立てが渡す記録を 1 tick に 1 つ
//! ずつ使う。受信スレッドで組み立てないのは、記録のキューをゲームのスレッドへ渡す 2 本目のリングバッファが要り、ロス・並べ替えの吸収
//! （`Assembler`）を 2 スレッドで共有することになるため。ゲームのスレッドの確保は、コードを取って `Watcher` を作る 1 回（繋がる前）
//! だけ。
//!
//! ## 1 tick の流れ（`WatchPlay::on_input`）
//!
//! 1. リングを空にして `Watcher` へ渡し、`poll`（JOIN・ACK）。ホストの最初の STREAM を受けたら Selecting へ。
//! 2. **観戦をやめる（Esc）**: 繋がっている間（Unconnected と Ended 以外）は、自分の Esc が「押されていない → 押された」に変わった
//!    tick で、BYE をその tick にホストへ送り、段階を Ended（`WatchEnd::Quit`）にして `Local` を返す（その tick の Esc もゲームへ
//!    届く）。押しっぱなしで繋がったときは、離すまで数えない。BYE は 1 通で、届かなくてもホストは `spectate::SILENT_MS`（10 秒）で外す。
//! 3. `phase` ごと:
//!    - **Unconnected**: 選択の画面（人対人の Match の 7・8・16）で決定のビットだけ落として自分の入力を通す（繋がる前に決定が進むと
//!      画面が食い違う）。ほかは `Local`。
//!    - **Selecting**: 選択の画面なら**ホストの記録の列を両席の入力として**入れる（`menu_tick`）。`Menu` はそのまま両席の語
//!      （画面の合わせが届く前の語は、ゲストと同じく方向・Ctrl・Shift を落とす）、`Step` は続く `Menu` の語と一緒に、ゲームがその
//!      押下を読む tick に入れる（読む状態でない間は持ち越す。次が `Menu` でない、か `PAIR_WAIT_TICKS` 経っても来ないときは語 0 で
//!      入れる）。選択の外は観戦者のゲーム自身の入力で進む（流れの終わり `End` だけは外でも見る）。「今の場面 1・次の要求 2」
//!      （マップの決定が入った）を見たら Holding へ。
//!    - **Holding**（止める地点）: 3 本とも 0 を feed。`Select` が両席揃うまで描画だけのフレーム。揃ったら両席とシードをゲームへ書き
//!      （`Game::apply_watched`）、LOADING へ移って 0 を feed。
//!    - **Loading**: 0 を feed。開始のフック（`on_match_start`）でコピーのチャージ型を書いて Lockstep へ。
//!    - **Lockstep**: `Frame` の語を 3 本とも feed。語が届いていなければ予算まで待ち、過ぎたら描画だけのフレーム。再戦（今の場面 2・
//!      次の要求 10 ほか）は 0 を feed して LOADING へ（止めない・シードは置き直さない）。マッチを出るなら Selecting へ。
//!    - **Ended**: `End` を受けた・ホストが黙った・続けられなくなった・自分で抜けた。以後は `Local`。ただし観戦者が自分の入力でマッチの
//!      終わりのメニューの「リプレイを保存して終了」を決めた tick は、保存の画面を飛ばす印を頼む（`ended_tick`）。
//!
//! ## 観戦者の遅れと余裕
//!
//! 観戦者の遅れは、ホストの送り（`spectate_host.rs`）＋ 片道 ＋ **余裕**。余裕は `WatchConfig::margin_frames`（既定
//! `WATCH_MARGIN_FRAMES` = 3 フレーム ≈ 50 ms）で、**マッチの 1 フレーム目を使う前に**、記録が余裕の数まで溜まる（か
//! `START_HOLD_TICKS` = 30 tick 経つ）のを待つ。以後は 1 tick に 1 フレームずつ使うので、遅れは溜まった数のまま変わらない。3 は、
//! ホストの送りの最大 19.4 ms を吸収する 2 フレームに、tick と受信の周回の位相のずれの分の 1 フレームを足した値。実測（loopback。
//! 485 フレーム）は、ホストが使ってから観戦者が使うまで p50 約 49 ms・最大 51.4 ms。遅れは観戦者だけに掛かり、対戦者には掛からない。
//! マッチの途中で語が届かず止まったら、次に使う前に溜まりが戻るのを待つ（`spectate_pace::Pace::may_play`。最長 1 秒）。戻す数は、
//! 追いついている途中（往復ごとにまとまって届く）なら次のまとまりまで尽きない数（往復が長いほど多い）、追いついて 1 フレームに 1 つ
//! ずつ届いているなら余裕だけ。
//!
//! ## 遅れて JOIN した観戦者・流れが切れたとき
//!
//! 遅れて JOIN すると、ホストは最後に選択があった seg の始まり（`Seg(k)`。その後ろにその seg の選択の押下・SELECT が続く）から流す。
//! 再戦が続いている間に入っても、その再戦の列を最初から追う（`SpectateHost::join_start`）。**選択の画面は
//! 遅れを詰める**（`catch_up_menus`）: `Menu` は 1 tick に 1 つ積まれるので、同じ速さで使うとホストが選択の画面に居た時間（実測で
//! 62 秒）がそのまま遅れになる。使っていない記録が `MENU_CATCHUP_ABOVE` を越えているあいだ、押下のない `Menu` を 1 tick を使わずに
//! 捨てる（押下は全部、順に入れる）。マッチの語は詰めず、溜まりが `FF_ENTER_FRAMES` を越えたらゲームの更新を余分に回して
//! **早送り**する（`spectate_watch/ff.rs`。速さは届く速さに合わせる —— `spectate_pace`）。選択を先最初から辿れないなら（マッチから戻った選択は 8 から入る・再戦の seg は選択が無い）、
//! 先頭の記録がゲームの画面と合わず `MENU_HOLD_LIMIT` tick 持ち越した後にやめる（`Stuck`）。選択の記録が無いマッチの語（`Seg` の直後の
//! `Frame`）は使えないので、選択の画面にいる間に先頭に来た `Frame` は捨てて（`orphan_frames`）、次の `Seg` を待つ。ホストが選択の seg まで
//! 遡れなかった（リングバッファから落ちた・遡りの上限を越えた）ときがこの形で、待つ間は `awaiting_select` が真（画面の知らせの元）。
//!
//! ## ゲーム内のメニューの経路（観戦の「ルームマッチ」）
//!
//! 観戦者のセッションは、ゲーム内のメニュー（`code_side = game`）の観戦の「ルームマッチ」を決めた後に、ゲームのスレッドで**後から置く**
//! （`script_win::install_placed`）。`on_input` は `tend`（ネットワークの手入れ）→ `esc_edge`（Esc の読み）→ `dispatch`（判断）に分けてあり、
//! メニューが画面 6 で入力を握る tick（`net_ui::watch_input`）も `tend` を毎 tick 呼ぶ（止めると JOIN の送り直しと ACK が止まる）。
//! ホストの流れを受け始めたら、メニューが対戦者と同じ自動の前進で画面 6 → 画面 7 へ進める。
//!
//! 遅れて入った観戦者が 1 マッチ目の後の seg（選択が画面 8 から始まる）に入ると、タイトルから自分で進んだゲームは画面 7 にいて先頭の
//! 押下の画面（8）と合わない。この形だけ、観戦者の側で決定を押して画面 8 まで進める（`walk_to_chara`）。見張りが諦めたとき
//! （`WatchSlot::give_up`）は、繋がる前なら、コードを取った後でも終える（`watch_link`）。
//!
//! 観戦者が送るのは JOIN・ACK・BYE だけ（`Watcher` が作る）。マッチの語も自分の入力も送らない。
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use higanr_core::handshake::Name;
use higanr_core::input_model::keys::{ESC, Z};
use higanr_core::lockstep::RESEND_MS;
use higanr_core::menu_model::{
    APPLY_CLEAR_BITS, DECIDE_BITS, Outcome, SCREEN_CHARA, SCREEN_DIFFICULTY, TRANSITION_BITS,
    outcome_of, uses_align,
};
use higanr_core::net_menu::PeerStats;
use higanr_core::spectate::{AsmCounts, Watcher, WatcherConfig, WatcherCounts};
use higanr_core::spectate_pace::Pace;
use higanr_core::wire::{ByeReason, MAX_DATAGRAM, Record, Seat, Select, Step, StepKind};

use crate::net_portrait::Followed;
use crate::net_select::{self, MenuView};
use crate::net_ui::{HudNames, WatchHudFacts};
use crate::net_watch::phase_code;
use crate::netplay::peer_wait;
use crate::netplay::{
    Action, BadValue, CursorWrite, FrameRec, FrameTiming, Game, MENU_HOLD_LIMIT, Net, Phase,
    REQ_LEAVE_MATCH, REQ_REBUILD_MATCH, REQ_SAVE_AND_QUIT, SCENE_MATCH, StepLog, StepRec,
    write_plan_cursors,
};
use crate::scripted::SCENE_MENU;

mod ff;
use ff::FfState;
pub use ff::{
    FF_BUDGET_US, FF_ENTER_FRAMES, FF_EXIT_FRAMES, FF_MAX_EXTRA, FfHost, FfStats, run_display,
};

/// マッチの 1 フレーム目を使う前に溜めておく記録の数（余裕）。
pub const WATCH_MARGIN_FRAMES: u32 = 3;
pub const START_HOLD_TICKS: u32 = 30;
/// 繋がった後、ホストから何も聞こえない時間（ms）がこれを越えたら見るのをやめる。観戦者が黙って外される時間
/// （`spectate::SILENT_MS` = 10 秒）の 2 倍で、越えたらホストが終えた（か、外された後の JOIN にも答えない）と読む。
pub const HOST_SILENT_END_MS: u32 = 20_000;
/// 止める地点で `Select` が両席揃うのを待ってよい tick の数。対戦者の待ち（`peer_wait::LIMIT_MS`）に 10 秒の余白を足した長さ（60 fps）。
/// 対戦者が最小化などで止まっても、先に観戦者が見るのをやめない。
pub const HOLD_LIMIT_TICKS: u32 = (peer_wait::LIMIT_MS + 10_000) / 1000 * 60;
/// 先頭の `Step` の次の記録（その tick の `Menu`）が届くのを待ってよい tick の数。来なければ語 0 で入れる。
pub const PAIR_WAIT_TICKS: u32 = 6;
/// 選択の画面で、使っていない記録がこの数を**越えたら**、遅れて入った観戦者として押下のない `Menu` を捨てて追いつく
/// （`catch_up_menus`）。初めから観戦している観戦者の溜まり（余裕 3 前後、入る演出が長いときは 6 つ程度）では入らず、遅れて入った
/// 観戦者（実機で数千）だけが入る値。
pub const MENU_CATCHUP_ABOVE: u32 = 30;
/// 追いつくときに残す記録の数（捨てた後の溜まり。余裕と同じ）。
pub const MENU_CATCHUP_KEEP: u32 = WATCH_MARGIN_FRAMES;
pub const HUD_DELAY_WINDOW: u32 = 30;
pub const MARGIN_BOUNDS_US: [u32; 5] = [8_000, 16_667, 33_333, 50_000, 100_000];
/// 記録が届いた時刻を覚えておく数（`ack` の進みで付ける。余裕の起点）。溜まりがこれを越えた分は余裕が分からない。
const ARRIVAL_SLOTS: usize = 512;

#[derive(Debug, Default)]
pub struct WatchSlot {
    /// 置いた設定の世代（0 = まだ無い）。`cfg` の中の世代と同じ値を、置いた後に Release で書く。
    stamp: AtomicU32,
    cfg: Mutex<(u32, Option<WatcherConfig>)>,
    /// ホストの最初の STREAM を受けた（JOIN が通った。見張りは終える）。
    joined: AtomicBool,
    /// 観戦が終わった（見張りは終える）。
    done: AtomicBool,
    /// 見張りが期間内に使えるコードを読めずに諦めた。
    gave_up: AtomicBool,
}

impl WatchSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn offer(&self, c: WatcherConfig) {
        let mut g = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        let next = g.0.wrapping_add(1).max(1);
        *g = (next, Some(c));
        self.stamp.store(next, Ordering::Release);
    }

    fn take_newer(&self, seen: &mut u32) -> Option<WatcherConfig> {
        if self.stamp.load(Ordering::Acquire) == *seen {
            return None;
        }
        let g = match self.cfg.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let (stamp, c) = *g;
        *seen = stamp;
        c
    }

    pub fn generation(&self) -> u32 {
        self.stamp.load(Ordering::Acquire)
    }

    pub fn mark_joined(&self) {
        self.joined.store(true, Ordering::Release);
    }

    pub fn is_joined(&self) -> bool {
        self.joined.load(Ordering::Acquire)
    }

    pub fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn give_up(&self) {
        self.gave_up.store(true, Ordering::Release);
    }

    pub fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
    /// 語が届いていないフレームで待つ予算（µs。ini の `wait_budget_us`）。
    pub wait_budget_us: u32,
    /// マッチの 1 フレーム目を使う前に溜める記録の数（`WATCH_MARGIN_FRAMES`）。
    pub margin_frames: u32,
    /// 選択の画面を持つゲーム（本物）か。false は偽のゲーム。マップの決定が記録で入っていない場面 2（タイトルのデモなど）では止める地点に
    /// 入らず `Local`、繋がる前の選択の画面で決定を落とす。
    pub require_net_selection: bool,
}

/// 観戦をやめた理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEnd {
    /// ホストの `End`（マッチの終わり・ホストの終了・ズレ検出・切断など）。
    HostEnded(ByeReason),
    /// 繋がった後、ホストから `HOST_SILENT_END_MS` 聞こえなかった。
    HostSilent,
    /// 使えるコードが期間内に貼られなかった（見張りが諦めた）。
    GaveUp,
    /// 記録の SELECT の値が範囲の外だった（1 つも書かずにやめる）。
    BadSelect(BadValue),
    /// 持っている押下（先頭の `Step`）を `MENU_HOLD_LIMIT` tick 入れられなかった（画面がホストと合わない。辿れない seg など）。
    Stuck(Step),
    /// 止める地点で `Select` が `HOLD_LIMIT_TICKS` tick 揃わなかった。
    HoldTimeout,
    /// マッチの途中でホストの流れが seg の始まりへ飛んだ（外されて JOIN し直した）か、途中に `Seg` が来た。続けられない。
    Lost,
    /// 観戦者が自分で抜けた（自分の Esc が「押されていない → 押された」に変わった）。BYE をその tick に出す。
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WatchStats {
    pub ticks: u64,
    pub played: u64,
    /// 0 を feed した tick（止める地点・読み込み・再戦の切替・マッチを出る tick）。
    pub zero_ticks: u64,
    /// 語が届いていなくて描画だけのフレームにした回と、その最長の連続。
    pub stalls: u64,
    pub max_stall_run: u32,
    /// マッチの途中で止まった後、溜まりが戻るのを待った描画だけのフレーム（`spectate_pace::Pace::may_play`）。
    pub refill_ticks: u64,
    /// マッチの 1 フレーム目の前に、余裕が溜まるのを待った描画だけのフレーム。
    pub start_hold_ticks: u64,
    /// 止める地点で `Select` を待った描画だけのフレーム。
    pub hold_frames: u64,
    /// 選択の画面で記録の列から feed したフレーム（`Menu` と、押下を入れたフレーム）。
    pub menu_frames: u64,
    pub menu_applied: u32,
    /// 先頭の押下を、ゲームが読まないので持ち越したフレーム（入る演出の間）。
    pub menu_held: u64,
    /// 遅れて入った観戦者が、画面 7 から画面 8 まで自分で進めるために押した決定の数（`walk_to_chara`）。
    pub menu_walked: u32,
    /// 遅れて入って、選択の画面で追いつくために捨てた `Menu` の記録（`catch_up_menus`）。
    pub menu_skipped: u64,
    pub menu_align_miss: u32,
    pub menu_portrait_called: u32,
    pub menu_portrait_refused: u32,
    /// 繋がる前に選択の画面で落とした決定の押下（押した瞬間）。
    pub pre_join_decides: u32,
    pub segs: u32,
    pub matches_started: u32,
    pub selects: u32,
    /// 使えなかった記録: 選択の画面の先頭に来た `Frame`（選択の記録の無い seg）/ 止める地点やマッチの中に来た種類違いの記録。
    pub orphan_frames: u64,
    pub names: u32,
    /// 受けた対戦の状態の記録（`Record::Stats`。ホストがマッチの最中に約 1 秒ごとに積む）。
    pub peer_stats: u32,
    pub stray_records: u64,
    /// 使った `Frame` の番号が直前の続きでなかった回（0 のはず）。
    pub frame_gaps: u64,
    pub charge_write_failures: u32,
    /// ネットワークの選択の合意なしに場面 2 へ移った tick（タイトルのデモなど）。
    pub local_scene2: u32,
    pub wakes: u64,
    pub margin_hist: [u64; 6],
    pub margin_max_us: u32,
    pub margin_unknown: u64,
    pub margin_sum_us: u64,
    /// 使う直前に溜まっていた記録の最大（追いつくまでの遅れの目安）。
    pub backlog_max: u32,
    /// 消費のあと、ホストの流れが飛んだ回（`Assembler` の reset。最初の 1 回は数えない）。
    pub jumps: u32,
    /// 繋がるまでの ms（最初の STREAM を受けるまで。コードを取ってから）。
    pub join_wait_ms: Option<u32>,
    pub ff: FfStats,
}

#[derive(Debug, Clone, Copy)]
struct Arrival {
    idx: u32,
    t_us: u64,
}

const NO_ARRIVAL: Arrival = Arrival {
    idx: u32::MAX,
    t_us: 0,
};

fn ms(us: u64) -> u32 {
    (us / 1_000) as u32
}

pub struct WatchPlay {
    cfg: WatchConfig,
    slot: Arc<WatchSlot>,
    seen_gen: u32,
    watcher: Option<Watcher>,
    phase: Phase,
    end: Option<WatchEnd>,
    bye: Option<ByeReason>,
    /// 繋がってから、自分の Esc が押されていないのを 1 度見たか（押しっぱなしで繋がったとき、離すまでは抜けない）。
    esc_armed: bool,
    /// コードを取った時刻（`Net::now_us` の ms）と、繋がったのを見た時刻。
    code_ms: Option<u32>,
    // ---- 記録の先読み（先頭の 2 つ。`Step` の次の `Menu` を見るため）----
    look: [Option<(u32, Record)>; 2],
    popped_any: bool,
    seen_jumps: u64,
    /// 最後に消費した `Seg` の seg（`Frame` の seg）と、次に来るはずの frame。
    seg: Option<u16>,
    /// 席ごとの名前（観戦の流れの `Name` の記録。1P・2P。接続の間変わらないので seg をまたいで持つ）。
    names: [Option<Name>; 2],
    /// 対戦者 2 人の間の状態の最新の値（観戦の流れの `Stats` の記録。新しい seg の始まりで捨てる。受けるまでは `None`）。
    peer_stats: Option<PeerStats>,
    expect_frame: u32,
    // ---- 選択 ----
    menu_active: bool,
    aligned: Option<u8>,
    map_decided: bool,
    hold_run: u32,
    /// 先頭の `Step` の次の記録（その tick の `Menu`）を待った tick の数。
    pair_wait: u32,
    /// 今の seg で入れた押下の数。0 のまま先頭の押下が画面 8 のときは、画面 7 にいる観戦者が遅れて入った seg（`walk_to_chara`）。
    seg_steps: u32,
    /// 選択の記録の無い seg のマッチの語を捨てて、次の選択を待っている（選択の記録を 1 つ使えば戻す）。
    awaiting_select: bool,
    pre_join_decide_held: bool,
    steps: StepLog,
    sel: [Option<Select>; 2],
    written: Option<[Select; 2]>,
    charge: [u8; 2],
    // ---- マッチ ----
    /// 今の seg のマッチの 1 フレーム目を使ったか（余裕を待ち終えたか）。
    started: bool,
    start_hold: u32,
    stall_run: u32,
    timing: FrameTiming,
    arrivals: [Arrival; ARRIVAL_SLOTS],
    stats: WatchStats,
    ff: FfState,
    /// 早送りの速さと、止まった後の余裕の作り直し（1 表示ごとに届いた記録の数を入れる）。
    pace: Pace,
    /// 前の表示の始まりから、使えるようになった記録の数（組み立ての連続の終わりが進んだ数。`reset` で飛んだ便は、飛んだ先から数える）。
    pace_arrived: u32,
    /// 画面の通信の状態の行に出す遅れ（`hud_facts`）: 使う直前の溜まりの直近 `HUD_DELAY_WINDOW` フレームの最大（そのまま出すと数字が
    /// ちらつく）。上がるときはすぐ、下がるときは期間が閉じるときにコピーする。
    hud_delay: u32,
    /// いまの期間の最大と、期間で使ったフレームの数。
    hud_win_max: u32,
    hud_win_n: u32,
    /// マッチの終わりのメニューの「リプレイを保存して終了」（今 2・次 6）を置いた tick に立つ。`take_save_quit` で 1 回だけ取る。
    save_quit: bool,
    /// 観戦が終わった後（`Ended`）の前の tick に「今 2・次 6」を見たか（立ち上がりだけで頼むため）。
    ended_save_seen: bool,
    /// 検査だけ: 使える記録の数を固定する（溜まりが多い形を、通信を通さずに作る）。
    #[cfg(test)]
    test_backlog: Option<u32>,
}

impl WatchPlay {
    pub fn new(slot: Arc<WatchSlot>, cfg: WatchConfig) -> Self {
        WatchPlay {
            cfg,
            slot,
            seen_gen: 0,
            watcher: None,
            phase: Phase::Unconnected,
            end: None,
            bye: None,
            esc_armed: false,
            code_ms: None,
            look: [None; 2],
            popped_any: false,
            seen_jumps: 0,
            seg: None,
            names: [None; 2],
            peer_stats: None,
            expect_frame: 0,
            menu_active: false,
            aligned: None,
            map_decided: false,
            hold_run: 0,
            pair_wait: 0,
            seg_steps: 0,
            awaiting_select: false,
            pre_join_decide_held: false,
            steps: StepLog::new(),
            sel: [None; 2],
            written: None,
            charge: [0; 2],
            started: false,
            start_hold: 0,
            stall_run: 0,
            timing: FrameTiming::default(),
            arrivals: [NO_ARRIVAL; ARRIVAL_SLOTS],
            stats: WatchStats::default(),
            ff: FfState::default(),
            pace: Pace::new(),
            pace_arrived: 0,
            hud_delay: 0,
            hud_win_max: 0,
            hud_win_n: 0,
            save_quit: false,
            ended_save_seen: false,
            #[cfg(test)]
            test_backlog: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn end(&self) -> Option<WatchEnd> {
        self.end
    }

    pub fn menu_screen(&self) -> Option<u8> {
        self.aligned
    }

    /// 次のキャラ選択まで映らない（ホストが選択の seg まで遡れなかった）。画面に `T_SPECTATE_NEXT_SELECT` を出す間。
    pub fn awaiting_select(&self) -> bool {
        self.phase == Phase::Selecting && self.awaiting_select
    }

    pub fn stats(&self) -> WatchStats {
        self.stats
    }

    pub fn step_log(&self) -> &StepLog {
        &self.steps
    }

    /// 最後にゲームへ書いた両席の SELECT（検査が見る）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn written(&self) -> Option<[Select; 2]> {
        self.written
    }

    pub fn last_timing(&self) -> FrameTiming {
        self.timing
    }

    pub fn watcher_counts(&self) -> Option<WatcherCounts> {
        self.watcher.as_ref().map(Watcher::counts)
    }

    pub fn assembler_counts(&self) -> Option<AsmCounts> {
        self.watcher.as_ref().map(Watcher::assembler_counts)
    }

    pub fn is_joined(&self) -> bool {
        self.watcher.as_ref().is_some_and(Watcher::is_joined)
    }

    pub fn host_addr(&self) -> Option<std::net::SocketAddr> {
        self.watcher.as_ref().and_then(Watcher::host_addr)
    }

    // ---- 記録の先読み ----

    /// 組み立てから記録を 1 つ取る。名前の記録と対戦の状態の記録は時間を使わないので、ここで食って `names` / `peer_stats` に溜め、
    /// 先読みにも画面ごとの判断にも渡さない。
    fn watcher_pop(&mut self) -> Option<(u32, Record)> {
        loop {
            let r = self.watcher.as_mut()?.pop_indexed();
            if r.is_some() {
                self.popped_any = true;
            }
            match r {
                Some((_, Record::Name { seat, len, bytes })) => self.take_name(seat, len, &bytes),
                Some((
                    _,
                    Record::Stats {
                        ping_ms,
                        latency,
                        rollback,
                    },
                )) => {
                    self.peer_stats = Some(PeerStats::from_record(ping_ms, latency, rollback));
                    self.stats.peer_stats = self.stats.peer_stats.saturating_add(1);
                }
                other => return other,
            }
        }
    }

    /// 名前の記録を 1 つ受ける（`decode` が UTF-8・長さを確かめてある。読めなければ空の名前として置く）。
    fn take_name(&mut self, seat: Seat, len: u8, bytes: &[u8; higanr_core::wire::NAME_MAX]) {
        let text = bytes
            .get(..usize::from(len))
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("");
        let i = match seat {
            Seat::P1 => 0,
            Seat::P2 => 1,
        };
        if let Some(slot) = self.names.get_mut(i) {
            *slot = Some(Name::new(text));
        }
        self.stats.names = self.stats.names.saturating_add(1);
    }

    #[cfg(test)]
    pub(crate) fn set_names_for_test(&mut self, p1: &str, p2: &str) {
        self.names = [Some(Name::new(p1)), Some(Name::new(p2))];
    }

    #[cfg(test)]
    pub(crate) fn set_peer_stats_for_test(&mut self, s: Option<PeerStats>) {
        self.peer_stats = s;
    }

    #[cfg(test)]
    pub(crate) fn peer_stats_for_test(&self) -> Option<PeerStats> {
        self.peer_stats
    }

    #[cfg(test)]
    pub(crate) fn begin_seg_for_test(&mut self, seg: u16) {
        self.begin_seg(seg);
    }

    pub fn names(&self) -> Option<[Name; 2]> {
        match self.names {
            [Some(a), Some(b)] => Some([a, b]),
            _ => None,
        }
    }

    /// マッチ中の画面の表示の材料。マッチの中（`Phase::Lockstep`）でなければ `None`。名前は両席の記録を受けるまで `None`。ゲームのスレッドから
    /// 毎 tick 呼ぶ。確保しない。
    pub(crate) fn hud_facts(&self) -> Option<WatchHudFacts> {
        if self.phase != Phase::Lockstep {
            return None;
        }
        Some(WatchHudFacts {
            names: self.names().map(|[p1, p2]| HudNames { p1, p2 }),
            stats: self.peer_stats,
            delay_frames: self.hud_delay,
            fast_forward: self.stats.ff.active,
        })
    }

    fn refill(&mut self) {
        if self.look[0].is_none() {
            self.look[0] = self.watcher_pop();
        }
        if self.look[0].is_some() && self.look[1].is_none() {
            self.look[1] = self.watcher_pop();
        }
    }

    fn head(&self) -> Option<Record> {
        self.look[0].map(|(_, r)| r)
    }

    fn second(&self) -> Option<Record> {
        self.look[1].map(|(_, r)| r)
    }

    fn pop(&mut self) -> Option<(u32, Record)> {
        let r = self.look[0].take();
        self.look[0] = self.look[1].take();
        r
    }

    fn available(&self) -> u32 {
        #[cfg(test)]
        if let Some(n) = self.test_backlog {
            return n;
        }
        let held = self.look.iter().flatten().count();
        let pending = self.watcher.as_ref().map_or(0, Watcher::pending);
        (held + pending).min(u32::MAX as usize) as u32
    }

    fn finish(&mut self, end: WatchEnd) {
        if self.phase == Phase::Ended {
            return;
        }
        self.phase = Phase::Ended;
        self.end = Some(end);
        self.bye = match end {
            WatchEnd::HostEnded(_) => Some(ByeReason::Finished),
            WatchEnd::BadSelect(_)
            | WatchEnd::Stuck(_)
            | WatchEnd::HoldTimeout
            | WatchEnd::Lost
            | WatchEnd::Quit => Some(ByeReason::Cancelled),
            WatchEnd::HostSilent | WatchEnd::GaveUp => None,
        };
        self.slot.mark_done();
    }

    // ---- ネットワーク ----

    /// 終えたときに積んだ BYE を、あれば 1 通だけ送る（ホストのアドレスが分かっていれば。観戦者の枠を返す）。
    fn send_bye<N: Net>(&mut self, net: &mut N) {
        if let Some(r) = self.bye.take()
            && let Some(w) = self.watcher.as_mut()
        {
            w.bye(r, net);
        }
    }

    /// 繋がっている間に、自分の Esc が「押されていない → 押された」に変わった tick なら true。押しっぱなしで繋がったときに即抜けない
    /// よう、押されていないのを 1 度見てからの押下だけ数える（`esc_armed`）。読むのは `Game::peek_local_input`（台本をこの tick の分だけ
    /// 進める。1 tick に 1 回）。
    fn esc_went_down<G: Game>(&mut self, game: &mut G) -> bool {
        if matches!(self.phase, Phase::Unconnected | Phase::Ended) {
            return false;
        }
        if game.peek_local_input() & ESC == 0 {
            self.esc_armed = true;
            return false;
        }
        self.esc_armed
    }

    fn take_code<N: Net>(&mut self, net: &mut N) {
        if self.watcher.as_ref().is_some_and(Watcher::is_joined) {
            return;
        }
        if let Some(c) = self.slot.take_newer(&mut self.seen_gen) {
            self.watcher = Some(Watcher::new(c));
            self.code_ms = Some(ms(net.now_us()));
        }
    }

    /// リングを空にして `Watcher` へ渡し、`poll`（JOIN の送り直しと ACK）。
    fn pump<N: Net>(&mut self, net: &mut N) {
        let mut buf = [0u8; MAX_DATAGRAM];
        if self.phase == Phase::Ended {
            self.send_bye(net);
            while net.recv_marked(&mut buf).is_some() {}
            return;
        }
        self.take_code(net);
        let Some(w) = self.watcher.as_mut() else {
            while net.recv_marked(&mut buf).is_some() {}
            return;
        };
        while let Some((from, n, t_us, _)) = net.recv_marked(&mut buf) {
            let now = ms(net.now_us());
            let before = w.ack();
            let jumps = w.assembler_counts().jumps;
            w.on_datagram(from, buf.get(..n).unwrap_or(&[]), now, net);
            stamp_arrivals(&mut self.arrivals, before, w.ack(), t_us);
            // `reset` で飛んだ便は、飛んだ先（組み立ての先頭）から数える（飛んだ距離を届いた数にしない）。
            let arrived = if w.assembler_counts().jumps != jumps {
                w.pending() as u32
            } else {
                w.ack().saturating_sub(before)
            };
            self.pace_arrived = self.pace_arrived.saturating_add(arrived);
        }
        let now = ms(net.now_us());
        w.poll(now, net);
    }

    fn watch_link<N: Net>(&mut self, net: &mut N) {
        if self.phase == Phase::Ended {
            return;
        }
        let now = ms(net.now_us());
        // 見張りが諦めた。繋がる前なら、コードを取った後でも終える。繋がった後は `gave_up` を見ない。
        if self.phase == Phase::Unconnected
            && self.slot.gave_up()
            && !self.watcher.as_ref().is_some_and(Watcher::is_joined)
        {
            self.finish(WatchEnd::GaveUp);
            return;
        }
        let Some((joined, silent, jumps)) = self
            .watcher
            .as_ref()
            .map(|w| (w.is_joined(), w.silent_ms(now), w.assembler_counts().jumps))
        else {
            return;
        };
        if self.phase == Phase::Unconnected {
            if joined {
                self.phase = Phase::Selecting;
                self.stats.join_wait_ms = self.code_ms.map(|c| now.wrapping_sub(c));
                self.slot.mark_joined();
            }
        } else if silent.is_some_and(|s| s >= HOST_SILENT_END_MS) {
            self.finish(WatchEnd::HostSilent);
            return;
        }
        if jumps != self.seen_jumps {
            self.seen_jumps = jumps;
            // 記録を 1 つも取る前の飛びは、遅れて JOIN したときの最初の reset（普通）。取った後の飛びは流れが切れた。
            if self.popped_any {
                self.stats.jumps = self.stats.jumps.saturating_add(1);
                self.look = [None; 2];
                self.finish(WatchEnd::Lost);
            } else {
                self.look = [None; 2];
            }
        }
    }

    // ---- 入力の地点 ----

    pub fn on_input<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        self.tend(net);
        let esc = self.esc_edge(game);
        self.dispatch(game, net, esc)
    }

    /// ネットワークの手入れ（tick の始まり。リングを空にして `Watcher` へ渡し、繋がった・黙った・流れが飛んだを見る）。ゲーム内のメニューが画面 6 で
    /// 入力を握る tick も毎 tick これを先に呼ぶ（止めると JOIN の送り直しと ACK が止まる）。
    pub fn tend<N: Net>(&mut self, net: &mut N) {
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        self.pump(net);
        self.watch_link(net);
    }

    /// 自分の Esc が「押されていない → 押された」に変わった tick か（繋がっている間だけ）。`peek_local_input` を 1 tick に 1 回呼ぶ
    /// ので台本がその tick の分だけ進む。メニューが先に握る実行は、`menu_words` で台本の語を読む前にこれを呼ぶ（二重に進まない）。
    pub fn esc_edge<G: Game>(&mut self, game: &mut G) -> bool {
        self.esc_went_down(game)
    }

    /// 自分で抜ける（Esc）。BYE はこの tick に出し（次の tick を待たない）、段階を `Ended` にする。
    pub fn quit_by_esc<N: Net>(&mut self, net: &mut N) {
        self.finish(WatchEnd::Quit);
        self.send_bye(net);
    }

    pub fn is_ended(&self) -> bool {
        self.phase == Phase::Ended
    }

    /// 観戦者のゲームが「リプレイを保存して終了」でマッチを出た tick か（1 回だけ真を返す）。呼び手はこれを見た tick に、ゲームの保存の画面を
    /// 飛ばす印を立てる。観戦が先に終わった後に観戦者が自分の入力で決めたときも真。再戦・キャラを変更・マッチの外では真にならない。
    pub fn take_save_quit(&mut self) -> bool {
        core::mem::take(&mut self.save_quit)
    }

    /// 検査の入口: 「保存して終了」でマッチを出た tick と同じ印だけを置く。呼び手の配線を、本物の `take_save_quit` 経由で見るため。
    #[cfg(test)]
    pub(crate) fn set_save_quit_for_test(&mut self) {
        self.save_quit = true;
    }

    /// 検査だけ: 段階を直に置く（繋がった後の段階を、ホストの流れを作らずに作る）。
    #[cfg(test)]
    pub(crate) fn set_phase_for_test(&mut self, phase: Phase) {
        self.phase = phase;
    }

    pub fn dispatch<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N, esc: bool) -> Action {
        if esc {
            self.quit_by_esc(net);
            return Action::Local;
        }
        match self.phase {
            Phase::Unconnected => self.pre_join_tick(game),
            Phase::Ended => self.ended_tick(game),
            Phase::Selecting => {
                let (cur, next) = game.scenes();
                if cur == SCENE_MENU && next == SCENE_MATCH {
                    if self.cfg.require_net_selection && !self.map_decided {
                        self.stats.local_scene2 = self.stats.local_scene2.saturating_add(1);
                        return Action::Local;
                    }
                    self.phase = Phase::Holding;
                    self.hold(game)
                } else if cur == SCENE_MENU {
                    self.menu_tick(game)
                } else {
                    Action::Local
                }
            }
            Phase::Holding => self.hold(game),
            Phase::Loading => self.zero(game),
            Phase::Lockstep => self.lockstep_tick(game, net),
        }
    }

    pub fn on_match_start<G: Game>(&mut self, game: &mut G) {
        if self.phase != Phase::Loading {
            return;
        }
        if !game.write_charge(self.charge[0], self.charge[1]) {
            self.stats.charge_write_failures = self.stats.charge_write_failures.saturating_add(1);
        }
        self.phase = Phase::Lockstep;
        self.started = false;
        self.start_hold = 0;
        self.stall_run = 0;
        self.pace.reset_seg();
        self.stats.matches_started = self.stats.matches_started.saturating_add(1);
    }

    /// 観戦が終わった後の 1 tick。観戦者が自分の入力で「リプレイを保存して終了」（今 2・次 6）を決めた tick にも、保存の画面を飛ばす印を
    /// 頼む。頼むのは立ち上がりの 1 tick だけ。
    fn ended_tick<G: Game>(&mut self, game: &mut G) -> Action {
        let (cur, next) = game.scenes();
        let hit = cur == SCENE_MATCH && next == REQ_SAVE_AND_QUIT;
        if hit && !self.ended_save_seen {
            self.save_quit = true;
        }
        self.ended_save_seen = hit;
        Action::Local
    }

    fn feed3<G: Game>(game: &mut G, p1: u16, p2: u16) {
        game.feed(0, p1);
        game.feed(1, p2);
        game.feed(2, p1 | p2);
    }

    fn zero<G: Game>(&mut self, game: &mut G) -> Action {
        Self::feed3(game, 0, 0);
        self.zero_fed()
    }

    fn zero_fed(&mut self) -> Action {
        self.stats.zero_ticks = self.stats.zero_ticks.saturating_add(1);
        Action::Zero
    }

    fn pre_join_tick<G: Game>(&mut self, game: &mut G) -> Action {
        if !self.cfg.require_net_selection {
            return Action::Local;
        }
        let (cur, next) = game.scenes();
        if cur != SCENE_MENU
            || next == SCENE_MATCH
            || game.menu_view().filter(net_select::in_selection).is_none()
        {
            self.pre_join_decide_held = false;
            return Action::Local;
        }
        let raw = game.local_input(Seat::P1);
        let held = raw & DECIDE_BITS != 0;
        if held && !self.pre_join_decide_held {
            self.stats.pre_join_decides = self.stats.pre_join_decides.saturating_add(1);
        }
        self.pre_join_decide_held = held;
        Self::feed3(game, raw & !DECIDE_BITS, 0);
        Action::Menu
    }

    fn begin_seg(&mut self, seg: u16) {
        self.seg = Some(seg);
        self.expect_frame = 0;
        self.seg_steps = 0;
        self.sel = [None; 2];
        self.peer_stats = None;
        self.stats.segs = self.stats.segs.saturating_add(1);
    }

    fn menu_tick<G: Game>(&mut self, game: &mut G) -> Action {
        let view = game.menu_view().filter(net_select::in_selection);
        let Some(v) = view else {
            self.menu_active = false;
            // 選択の画面の外（タイトルなど）では記録を使わないが、流れの終わりだけは見る（先頭まで使い切っていれば `End`）。
            self.refill();
            if let Some(Record::End(r)) = self.head() {
                self.pop();
                self.finish(WatchEnd::HostEnded(r));
            }
            return Action::Local;
        };
        if !self.menu_active {
            // 選択に入る（最初は難易度の画面、マッチから戻ると押下なしでキャラ選択まで進む）。画面ごとの合わせはやり直し。
            self.menu_active = true;
            self.aligned = None;
            self.map_decided = false;
            self.hold_run = 0;
        }
        loop {
            self.refill();
            match self.head() {
                Some(Record::Seg { seg }) => {
                    self.pop();
                    self.begin_seg(seg);
                }
                Some(Record::Frame { .. }) => {
                    self.pop();
                    self.stats.orphan_frames = self.stats.orphan_frames.saturating_add(1);
                    self.awaiting_select = true;
                }
                Some(Record::End(r)) => {
                    self.pop();
                    self.finish(WatchEnd::HostEnded(r));
                    return Action::Local;
                }
                _ => break,
            }
        }
        self.catch_up_menus();
        let screen8 = u8::try_from(v.screen).unwrap_or(0);
        // 合わせが届くまでの語は、カーソルの値が食い違ったまま動かさないよう、方向・Ctrl・Shift を落とす（ゲストと同じ）。キャラ選択には
        // 合わせが無いので、両席とも語のまま動かす（観戦者の画面の相手の席は語の相対で動く。対戦者の側の位置は記録に無い）。
        let clear: u16 = if self.aligned == Some(screen8) || !uses_align(screen8) {
            u16::MAX
        } else {
            !APPLY_CLEAR_BITS
        };
        match (self.head(), self.second()) {
            (Some(Record::Menu { p1, p2 }), _) => {
                self.pop();
                self.hold_run = 0;
                self.awaiting_select = false;
                let [w1, w2, w3] = net_select::normal_words(Seat::P1, p1 & clear, p2 & clear);
                game.feed(0, w1);
                game.feed(1, w2);
                game.feed(2, w3);
                self.stats.menu_frames = self.stats.menu_frames.saturating_add(1);
                Action::Menu
            }
            (Some(Record::Step(st)), second) => {
                let words = match second {
                    Some(Record::Menu { p1, p2 }) => Some((p1, p2, true)),
                    Some(_) => Some((0, 0, false)),
                    None if self.pair_wait >= PAIR_WAIT_TICKS => Some((0, 0, false)),
                    None => None,
                };
                match words {
                    None => {
                        self.pair_wait += 1;
                        Self::feed3(game, 0, 0);
                        Action::Menu
                    }
                    Some((m1, m2, with_menu)) => {
                        if net_select::reads_step(&v, &st) {
                            self.pop();
                            if with_menu {
                                self.pop();
                            }
                            self.hold_run = 0;
                            self.pair_wait = 0;
                            self.apply_step(game, &v, &st, m1 & clear, m2 & clear);
                            Action::Menu
                        } else if self.seg_steps == 0
                            && st.screen == SCREEN_CHARA
                            && v.screen == u32::from(SCREEN_DIFFICULTY)
                        {
                            self.walk_to_chara(game, &v, st)
                        } else {
                            self.hold_menu(game, st)
                        }
                    }
                }
            }
            (Some(Record::Select { .. }), _) => {
                self.hold_run = self.hold_run.saturating_add(1);
                self.stats.menu_held = self.stats.menu_held.saturating_add(1);
                if self.hold_run > MENU_HOLD_LIMIT {
                    self.finish(WatchEnd::HoldTimeout);
                    return Action::Local;
                }
                Self::feed3(game, 0, 0);
                Action::Menu
            }
            _ => {
                if self.head().is_some() {
                    self.pop();
                    self.stats.stray_records = self.stats.stray_records.saturating_add(1);
                }
                Self::feed3(game, 0, 0);
                Action::Menu
            }
        }
    }

    /// 遅れて入った観戦者の選択の追いつき: 使っていない記録が `MENU_CATCHUP_ABOVE` を越えているあいだ、先頭の押下のない `Menu` を 1 tick を
    /// 使わずに捨てて `MENU_CATCHUP_KEEP` まで詰める。押下（`Step`）は 1 つも捨てない。止める地点の `Select` が両席を上書きするので、
    /// 飛ばした語はゲームの状態に残らない。
    fn catch_up_menus(&mut self) {
        if self.available() <= MENU_CATCHUP_ABOVE {
            return;
        }
        while self.available() > MENU_CATCHUP_KEEP {
            self.refill();
            if !matches!(self.head(), Some(Record::Menu { .. })) {
                break;
            }
            self.pop();
            self.stats.menu_skipped = self.stats.menu_skipped.saturating_add(1);
        }
    }

    /// 先頭の押下をゲームが読まない（か、その tick の `Menu` がまだ来ない）ので持ち越す。3 本とも 0 を feed する。
    fn hold_menu<G: Game>(&mut self, game: &mut G, st: Step) -> Action {
        self.hold_run = self.hold_run.saturating_add(1);
        self.stats.menu_held = self.stats.menu_held.saturating_add(1);
        if self.hold_run > MENU_HOLD_LIMIT {
            self.finish(WatchEnd::Stuck(st));
            return Action::Local;
        }
        Self::feed3(game, 0, 0);
        Action::Menu
    }

    /// 遅れて入った観戦者の画面 7 → 画面 8。1 マッチ目の後の seg は選択が画面 8 から始まるが、タイトルから自分で進んで画面 7 にいる
    /// 観戦者のゲームは進まないので、観戦者の側で決定（Z）を 1 tick 押して 1 tick 離して画面 8 まで進める。難易度は止める地点の
    /// `Select` が上書きする。数えは持ち越しと同じ `hold_run`。
    fn walk_to_chara<G: Game>(&mut self, game: &mut G, v: &MenuView, st: Step) -> Action {
        self.hold_run = self.hold_run.saturating_add(1);
        self.stats.menu_held = self.stats.menu_held.saturating_add(1);
        if self.hold_run > MENU_HOLD_LIMIT {
            self.finish(WatchEnd::Stuck(st));
            return Action::Local;
        }
        // 画面 7 の入る演出の間（ゲームが押下を読まない）には押さない（読まれない決定を入れない）。
        if self.hold_run % 2 == 1 && net_select::accepting(v) {
            self.stats.menu_walked = self.stats.menu_walked.saturating_add(1);
            // 画面 7 は合成 `[2]` だけを読む（`net_select::apply_plan` が押下を `w[2]` にだけ入れるのと同じ）。
            game.feed(0, 0);
            game.feed(1, 0);
            game.feed(2, Z);
        } else {
            Self::feed3(game, 0, 0);
        }
        Action::Menu
    }

    fn apply_step<G: Game>(&mut self, game: &mut G, v: &MenuView, st: &Step, m1: u16, m2: u16) {
        // 観戦者の席は決まっていない。両席の語を席のまま並べる（`by_seat(P1, m1, m2)` = (m1, m2)）。
        let plan = if st.kind == StepKind::Align {
            net_select::align_plan(v, st, Seat::P1, m1, m2, true)
        } else {
            net_select::apply_plan(v, st, Seat::P1, m1, m2)
        };
        match plan.cursor {
            Some(_) => {
                if let Some(w) = write_plan_cursors(game, v, &plan) {
                    match w {
                        CursorWrite::Miss => {
                            self.stats.menu_align_miss =
                                self.stats.menu_align_miss.saturating_add(1);
                        }
                        CursorWrite::Wrote(Some(Followed::Called)) => {
                            self.stats.menu_portrait_called =
                                self.stats.menu_portrait_called.saturating_add(1);
                        }
                        CursorWrite::Wrote(Some(Followed::Refused)) => {
                            self.stats.menu_portrait_refused =
                                self.stats.menu_portrait_refused.saturating_add(1);
                        }
                        CursorWrite::Wrote(_) => {}
                    }
                }
            }
            // 決定・合わせは値を書けなかった（マップの値が自分の一覧に無い）。書かずに数える。
            None if st.kind != StepKind::Cancel => {
                self.stats.menu_align_miss = self.stats.menu_align_miss.saturating_add(1);
            }
            None => {}
        }
        self.steps.push(StepRec {
            step: *st,
            own: false,
        });
        self.seg_steps = self.seg_steps.saturating_add(1);
        self.stats.menu_applied = self.stats.menu_applied.saturating_add(1);
        if st.kind == StepKind::Align {
            self.aligned = Some(st.screen);
        }
        match outcome_of(st.screen, st.kind) {
            Outcome::MapDecided => self.map_decided = true,
            Outcome::LeftToMode => self.menu_active = false,
            Outcome::Stay => {}
        }
        let [p1, p2, p3] = plan.words;
        game.feed(0, p1);
        game.feed(1, p2);
        game.feed(2, p3);
        self.stats.menu_frames = self.stats.menu_frames.saturating_add(1);
        self.awaiting_select = false;
    }

    fn hold<G: Game>(&mut self, game: &mut G) -> Action {
        Self::feed3(game, 0, 0);
        loop {
            self.refill();
            match self.head() {
                Some(Record::Seg { seg }) => {
                    self.pop();
                    self.begin_seg(seg);
                }
                Some(Record::Select { seat, sel }) => {
                    self.pop();
                    self.stats.selects = self.stats.selects.saturating_add(1);
                    let i = match seat {
                        Seat::P1 => 0,
                        Seat::P2 => 1,
                    };
                    if let Some(s) = self.sel.get_mut(i) {
                        *s = Some(sel);
                    }
                }
                Some(Record::Frame { .. }) if self.sel.iter().all(Option::is_some) => break,
                Some(Record::Frame { .. }) => {
                    self.pop();
                    self.stats.orphan_frames = self.stats.orphan_frames.saturating_add(1);
                }
                Some(Record::End(r)) => {
                    self.pop();
                    self.finish(WatchEnd::HostEnded(r));
                    return self.zero_fed();
                }
                Some(
                    Record::Menu { .. }
                    | Record::Step(_)
                    | Record::Name { .. }
                    | Record::Stats { .. },
                ) => {
                    // 止める地点に着いたのに選択の記録が残っている（観戦者のゲームがホストより先に着いた）。捨てる。
                    self.pop();
                    self.stats.stray_records = self.stats.stray_records.saturating_add(1);
                }
                None => break,
            }
        }
        if let [Some(s1), Some(s2)] = self.sel {
            if let Err(bad) = game.apply_watched(&s1, &s2) {
                self.finish(WatchEnd::BadSelect(bad));
                return self.zero_fed();
            }
            self.written = Some([s1, s2]);
            self.charge = [s1.charge, s2.charge];
            self.sel = [None; 2];
            self.map_decided = false;
            self.menu_active = false;
            self.hold_run = 0;
            self.phase = Phase::Loading;
            return self.zero_fed();
        }
        self.stats.hold_frames = self.stats.hold_frames.saturating_add(1);
        self.hold_run = self.hold_run.saturating_add(1);
        if self.hold_run > HOLD_LIMIT_TICKS {
            self.finish(WatchEnd::HoldTimeout);
            return self.zero_fed();
        }
        Action::RenderOnly
    }

    fn leave_seg(&mut self) {
        self.started = false;
        self.start_hold = 0;
    }

    fn lockstep_tick<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        let (cur, next) = game.scenes();
        if cur == SCENE_MATCH && REQ_REBUILD_MATCH.contains(&next) {
            self.leave_seg();
            self.phase = Phase::Loading;
            return self.zero(game);
        }
        if cur == SCENE_MATCH && next == REQ_SAVE_AND_QUIT {
            // マッチの終わりのメニューの「リプレイを保存して終了」。観戦者のゲームはこの後リプレイの保存の画面へ進むので、保存の画面を
            // 飛ばす印を頼む（対戦者の `NetPlay` は同じ tick に閉じるが、観戦者は閉じない）。
            self.save_quit = true;
        }
        if cur != SCENE_MATCH || REQ_LEAVE_MATCH.contains(&next) {
            self.leave_seg();
            self.phase = Phase::Selecting;
            self.menu_active = false;
            if cur != SCENE_MATCH {
                return Action::Local;
            }
            return self.zero(game);
        }
        let start = net.now_us();
        let deadline = start.saturating_add(u64::from(self.cfg.wait_budget_us));
        loop {
            self.refill();
            match self.head() {
                Some(Record::Seg { seg }) => {
                    if self.started {
                        // マッチの途中に次の seg の始まりが来た。ホストは最後の seg を出るときも `Seg(次)` を積み、直後に `End` が来る。
                        // 直後が `End` ならホストが終えただけで、付いていけなかったのではない。
                        match self.second() {
                            Some(Record::End(r)) => {
                                self.pop();
                                self.pop();
                                self.finish(WatchEnd::HostEnded(r));
                                return Action::Local;
                            }
                            // 本当に次の seg へ進んだのに、観戦者のゲームがマッチを続けている。辿れない。
                            Some(_) => {
                                self.finish(WatchEnd::Lost);
                                return Action::Local;
                            }
                            None => {
                                if let Some(a) = self.wait_words(net, deadline) {
                                    return a;
                                }
                                continue;
                            }
                        }
                    }
                    self.pop();
                    self.begin_seg(seg);
                }
                Some(Record::End(r)) => {
                    self.pop();
                    self.finish(WatchEnd::HostEnded(r));
                    return Action::Local;
                }
                Some(Record::Frame { frame, p1, p2 }) => {
                    if !self.started {
                        if self.available() < self.cfg.margin_frames
                            && self.start_hold < START_HOLD_TICKS
                        {
                            self.start_hold += 1;
                            self.stats.start_hold_ticks =
                                self.stats.start_hold_ticks.saturating_add(1);
                            return Action::RenderOnly;
                        }
                        self.started = true;
                    } else if !self.pace.may_play(self.available(), self.cfg.margin_frames) {
                        // 止まった後: 溜まりが戻るまで待つ（1 記録ずつ使うとすぐまた止まる）。
                        self.stats.refill_ticks = self.stats.refill_ticks.saturating_add(1);
                        return Action::RenderOnly;
                    }
                    let backlog = self.available();
                    let idx = self.look[0].map_or(u32::MAX, |(i, _)| i);
                    self.pop();
                    let wait_us =
                        net.now_us().saturating_sub(start).min(u64::from(u32::MAX)) as u32;
                    return self.play(game, net, (frame, p1, p2), idx, backlog, wait_us);
                }
                Some(_) => {
                    self.pop();
                    self.stats.stray_records = self.stats.stray_records.saturating_add(1);
                }
                None => {
                    if let Some(a) = self.wait_words(net, deadline) {
                        return a;
                    }
                }
            }
        }
    }

    fn wait_words<N: Net>(&mut self, net: &mut N, deadline: u64) -> Option<Action> {
        let now = net.now_us();
        if now >= deadline {
            self.stats.stalls = self.stats.stalls.saturating_add(1);
            self.stall_run = self.stall_run.saturating_add(1);
            self.stats.max_stall_run = self.stats.max_stall_run.max(self.stall_run);
            if self.started {
                self.pace.on_stall();
            }
            return Some(Action::RenderOnly);
        }
        let left = (deadline - now).min(u64::from(RESEND_MS) * 1_000);
        if net.wait(left as u32) {
            self.stats.wakes = self.stats.wakes.saturating_add(1);
        }
        self.pump(net);
        self.watch_link(net);
        if self.phase == Phase::Ended {
            return Some(Action::Local);
        }
        None
    }

    fn play<G: Game, N: Net>(
        &mut self,
        game: &mut G,
        net: &mut N,
        (frame, p1, p2): (u32, u16, u16),
        idx: u32,
        backlog: u32,
        wait_us: u32,
    ) -> Action {
        if frame != self.expect_frame {
            self.stats.frame_gaps = self.stats.frame_gaps.saturating_add(1);
        }
        self.expect_frame = frame.wrapping_add(1);
        Self::feed3(game, p1, p2);
        self.stall_run = 0;
        self.stats.played = self.stats.played.saturating_add(1);
        self.stats.backlog_max = self.stats.backlog_max.max(backlog);
        self.hud_delay = self.hud_delay.max(backlog);
        self.hud_win_max = self.hud_win_max.max(backlog);
        self.hud_win_n += 1;
        if self.hud_win_n >= HUD_DELAY_WINDOW {
            self.hud_delay = self.hud_win_max;
            self.hud_win_max = 0;
            self.hud_win_n = 0;
        }
        let now = net.now_us();
        let slot = self
            .arrivals
            .get(idx as usize % ARRIVAL_SLOTS)
            .copied()
            .unwrap_or(NO_ARRIVAL);
        let margin_us = (slot.idx == idx)
            .then(|| now.saturating_sub(slot.t_us).min(u64::from(u32::MAX)) as u32);
        match margin_us {
            Some(m) => {
                let i = MARGIN_BOUNDS_US
                    .iter()
                    .position(|&b| m < b)
                    .unwrap_or(MARGIN_BOUNDS_US.len());
                if let Some(n) = self.stats.margin_hist.get_mut(i) {
                    *n = n.saturating_add(1);
                }
                self.stats.margin_max_us = self.stats.margin_max_us.max(m);
                self.stats.margin_sum_us = self.stats.margin_sum_us.saturating_add(u64::from(m));
            }
            None => {
                self.stats.margin_unknown = self.stats.margin_unknown.saturating_add(1);
            }
        }
        self.timing = FrameTiming {
            delay: 0,
            margin_us,
            wait_us,
            head_wait_us: 0,
        };
        Action::Played(FrameRec {
            seg: self.seg.unwrap_or(0),
            frame,
            p1,
            p2,
        })
    }
}

fn stamp_arrivals(arrivals: &mut [Arrival; ARRIVAL_SLOTS], before: u32, after: u32, t_us: u64) {
    if after <= before {
        return;
    }
    let from = before.max(after.saturating_sub(ARRIVAL_SLOTS as u32));
    for idx in from..after {
        if let Some(a) = arrivals.get_mut(idx as usize % ARRIVAL_SLOTS) {
            *a = Arrival { idx, t_us };
        }
    }
}

/// 落とす決定のビットが `TRANSITION_BITS` に含まれることの確認用（`pre_join_tick` は `DECIDE_BITS` だけを落とす）。
const _: () = assert!(TRANSITION_BITS & DECIDE_BITS == DECIDE_BITS);

// ---------------------------------------------------------------- 読み取りのスレッドへのコピー

pub struct WatchStatus {
    w: [AtomicU32; WORDS],
}

const S_ON: usize = 0;
const S_PHASE: usize = 1;
const S_END: usize = 2;
const S_END_DETAIL: usize = 3;
const S_TICKS: usize = 4;
const S_PLAYED: usize = 5;
const S_ZERO: usize = 6;
const S_STALLS: usize = 7;
const S_MAX_STALL: usize = 8;
const S_START_HOLD: usize = 9;
const S_HOLD: usize = 10;
const S_MENU_FRAMES: usize = 11;
const S_MENU_APPLIED: usize = 12;
const S_MENU_HELD: usize = 13;
const S_ALIGN_MISS: usize = 14;
const S_SEGS: usize = 15;
const S_MATCHES: usize = 16;
const S_SELECTS: usize = 17;
const S_ORPHAN: usize = 18;
const S_STRAY: usize = 19;
const S_GAPS: usize = 20;
const S_CHARGE_FAIL: usize = 21;
const S_WAKES: usize = 22;
const S_MARGIN_HIST: usize = 23; // 6 語
const S_MARGIN_MAX: usize = 29;
const S_MARGIN_UNKNOWN: usize = 30;
const S_MARGIN_SUM_LO: usize = 31;
const S_MARGIN_SUM_HI: usize = 32;
const S_BACKLOG_MAX: usize = 33;
const S_JUMPS: usize = 34;
const S_JOIN_WAIT: usize = 35; // NO_VALUE = まだ
const S_STREAMS: usize = 36;
const S_ACKS: usize = 37;
const S_JOINS: usize = 38;
const S_FOREIGN: usize = 39;
const S_BAD_TAG: usize = 40;
const S_ASM_STORED: usize = 41;
const S_ASM_DUP: usize = 42;
const S_ASM_BEYOND: usize = 43;
const S_ASM_JUMPS: usize = 44;
const S_PRE_JOIN: usize = 45;
const S_LOCAL_SCENE2: usize = 46;
const S_PORTRAIT: usize = 47; // 2 語
const S_HOST_KIND: usize = 49;
const S_UNDECODABLE: usize = 50;
const S_WRONG_SESSION: usize = 51;
const S_MENU_WALKED: usize = 52;
// 早送り（`spectate_watch/ff.rs`）。
const S_FF_BEGUN: usize = 53;
const S_FF_DISPLAYS: usize = 54;
const S_FF_EXTRA: usize = 55;
const S_FF_BATCH_MAX: usize = 56;
const S_FF_EPISODES: usize = 57;
const S_FF_FINISHED: usize = 58;
const S_FF_ENTER_BACKLOG: usize = 59; // NO_VALUE = まだ
const S_FF_FIRST_MS: usize = 60; // NO_VALUE = まだ
const S_FF_MAX_MS: usize = 61;
const S_FF_TOTAL_MS: usize = 62;
const S_FF_CUTOFFS: usize = 63;
const S_FF_DROP_FAIL: usize = 64;
const S_FF_ACTIVE: usize = 65;
// 選択の画面で遅れを詰めるために捨てた `Menu`（`catch_up_menus`）。
const S_MENU_SKIPPED: usize = 66;
// マッチの途中で止まった後、溜まりが戻るのを待った描画だけのフレーム。
const S_REFILL: usize = 67;
const WORDS: usize = 68;
const NO_VALUE: u32 = u32::MAX;

impl WatchStatus {
    pub const fn new() -> Self {
        WatchStatus {
            w: [const { AtomicU32::new(0) }; WORDS],
        }
    }

    pub fn publish(&self, wp: &WatchPlay) {
        let s = wp.stats();
        let c = |v: u64| v.min(u64::from(u32::MAX)) as u32;
        let (end, detail) = end_code(wp.end());
        let set = |i: usize, v: u32| {
            if let Some(a) = self.w.get(i) {
                a.store(v, Ordering::Relaxed);
            }
        };
        set(S_ON, 1);
        set(S_PHASE, phase_code(wp.phase()));
        set(S_END, end);
        set(S_END_DETAIL, detail);
        set(S_TICKS, c(s.ticks));
        set(S_PLAYED, c(s.played));
        set(S_ZERO, c(s.zero_ticks));
        set(S_STALLS, c(s.stalls));
        set(S_MAX_STALL, s.max_stall_run);
        set(S_REFILL, c(s.refill_ticks));
        set(S_START_HOLD, c(s.start_hold_ticks));
        set(S_HOLD, c(s.hold_frames));
        set(S_MENU_FRAMES, c(s.menu_frames));
        set(S_MENU_APPLIED, s.menu_applied);
        set(S_MENU_HELD, c(s.menu_held));
        set(S_MENU_WALKED, s.menu_walked);
        set(S_MENU_SKIPPED, c(s.menu_skipped));
        set(S_ALIGN_MISS, s.menu_align_miss);
        set(S_SEGS, s.segs);
        set(S_MATCHES, s.matches_started);
        set(S_SELECTS, s.selects);
        set(S_ORPHAN, c(s.orphan_frames));
        set(S_STRAY, c(s.stray_records));
        set(S_GAPS, c(s.frame_gaps));
        set(S_CHARGE_FAIL, s.charge_write_failures);
        set(S_WAKES, c(s.wakes));
        for (i, h) in s.margin_hist.iter().enumerate() {
            set(S_MARGIN_HIST + i, c(*h));
        }
        set(S_MARGIN_MAX, s.margin_max_us);
        set(S_MARGIN_UNKNOWN, c(s.margin_unknown));
        set(S_MARGIN_SUM_LO, s.margin_sum_us as u32);
        set(S_MARGIN_SUM_HI, (s.margin_sum_us >> 32) as u32);
        set(S_BACKLOG_MAX, s.backlog_max);
        set(S_JUMPS, s.jumps);
        set(S_JOIN_WAIT, s.join_wait_ms.unwrap_or(NO_VALUE));
        set(S_FF_BEGUN, c(s.ff.begun));
        set(S_FF_DISPLAYS, c(s.ff.displays));
        set(S_FF_EXTRA, c(s.ff.extra_ticks));
        set(S_FF_BATCH_MAX, s.ff.batch_max_us);
        set(S_FF_EPISODES, s.ff.episodes);
        set(S_FF_FINISHED, s.ff.finished);
        set(
            S_FF_ENTER_BACKLOG,
            s.ff.first_enter_backlog.unwrap_or(NO_VALUE),
        );
        set(S_FF_FIRST_MS, s.ff.first_ms.unwrap_or(NO_VALUE));
        set(S_FF_MAX_MS, s.ff.max_ms);
        set(S_FF_TOTAL_MS, c(s.ff.total_ms));
        set(S_FF_CUTOFFS, s.ff.cutoffs);
        set(S_FF_DROP_FAIL, s.ff.drop_fail);
        set(S_FF_ACTIVE, u32::from(s.ff.active));
        set(S_PRE_JOIN, s.pre_join_decides);
        set(S_LOCAL_SCENE2, s.local_scene2);
        set(S_PORTRAIT, s.menu_portrait_called);
        set(S_PORTRAIT + 1, s.menu_portrait_refused);
        if let Some(wc) = wp.watcher_counts() {
            set(S_STREAMS, c(wc.streams));
            set(S_ACKS, c(wc.acks_sent));
            set(S_JOINS, c(wc.joins_sent));
            set(S_FOREIGN, c(wc.foreign));
            set(S_BAD_TAG, c(wc.bad_tag));
            set(S_UNDECODABLE, c(wc.undecodable));
            set(S_WRONG_SESSION, c(wc.wrong_session));
        }
        if let Some(ac) = wp.assembler_counts() {
            set(S_ASM_STORED, c(ac.stored));
            set(S_ASM_DUP, c(ac.dup));
            set(S_ASM_BEYOND, c(ac.beyond));
            set(S_ASM_JUMPS, c(ac.jumps));
        }
        set(
            S_HOST_KIND,
            wp.host_addr()
                .map_or(0, |a| u32::from(higanr_core::candidate::kind_of(a).to_u8())),
        );
    }

    pub fn published(&self) -> bool {
        self.get(S_ON) != 0
    }

    fn get(&self, i: usize) -> u32 {
        self.w.get(i).map_or(0, |a| a.load(Ordering::Relaxed))
    }

    pub fn text(&self) -> String {
        if !self.published() {
            return String::new();
        }
        let g = |i: usize| self.get(i);
        let n = g(S_PLAYED);
        let margin_n: u64 = (0..6).map(|i| u64::from(g(S_MARGIN_HIST + i))).sum();
        let sum = u64::from(g(S_MARGIN_SUM_LO)) | u64::from(g(S_MARGIN_SUM_HI)) << 32;
        let mean = sum.checked_div(margin_n).unwrap_or(0);
        let b = MARGIN_BOUNDS_US;
        let join = match g(S_JOIN_WAIT) {
            NO_VALUE => "まだ".to_string(),
            v => format!("{v} ms"),
        };
        let main = format!(
            "観戦者: 段階 {} / 終わり {}（{}） / tick {} / 使った試合のフレーム {} / 0 を feed {} / 入力ワードが届かず描画だけのフレーム {} 回（最長の連続 {}） / 止まった後に溜まりが戻るのを待った描画だけのフレーム {} / 試合の始めに余裕を待った描画だけのフレーム {} / 止める地点で待った {} / 選択のフレーム {}（入れた押下 {}・持ち越したフレーム {}・カーソルを書けなかった {}・立ち絵を追わせた {}・追わせなかった {}・遅れて入って画面 7 から 8 へ自分で押した決定 {}・遅れて入って追いつくために捨てた選択の語 {}） / 消費した seg の先頭 {}・受けた SELECT {}・始まった試合 {} / 使えなかった記録（選択の画面の先頭の試合の語 {}・種類違い {}）・番号が続かなかったフレーム {}・チャージ型を書けなかった {}\n\
             観戦者の受け: 繋がるまで {join} / 受けた STREAM {}（送った JOIN {}・ACK {}） / 組み立て（入れた {}・重複 {}・置き場の外 {}・飛び {}）・消費後の飛び {} / 捨てたもの（候補に無い送り元 {}・別の session {}・タグ {}・読めない {}）\n\
             観戦者の遅れ: 記録が届いてから使うまで（余裕）{} µs 未満 {} / {} 未満 {} / {} 未満 {} / {} 未満 {} / {} 未満 {} / それ以上 {}（平均 {mean} µs・最大 {} µs・分からなかった {}） / 使う直前に溜まっていた記録の最大 {}（使ったフレーム {n}）",
            PHASES.get(g(S_PHASE) as usize).copied().unwrap_or("?"),
            ENDS.get(g(S_END) as usize).copied().unwrap_or("?"),
            g(S_END_DETAIL),
            g(S_TICKS),
            n,
            g(S_ZERO),
            g(S_STALLS),
            g(S_MAX_STALL),
            g(S_REFILL),
            g(S_START_HOLD),
            g(S_HOLD),
            g(S_MENU_FRAMES),
            g(S_MENU_APPLIED),
            g(S_MENU_HELD),
            g(S_ALIGN_MISS),
            g(S_PORTRAIT),
            g(S_PORTRAIT + 1),
            g(S_MENU_WALKED),
            g(S_MENU_SKIPPED),
            g(S_SEGS),
            g(S_SELECTS),
            g(S_MATCHES),
            g(S_ORPHAN),
            g(S_STRAY),
            g(S_GAPS),
            g(S_CHARGE_FAIL),
            g(S_STREAMS),
            g(S_JOINS),
            g(S_ACKS),
            g(S_ASM_STORED),
            g(S_ASM_DUP),
            g(S_ASM_BEYOND),
            g(S_ASM_JUMPS),
            g(S_JUMPS),
            g(S_FOREIGN),
            g(S_WRONG_SESSION),
            g(S_BAD_TAG),
            g(S_UNDECODABLE),
            b[0],
            g(S_MARGIN_HIST),
            b[1],
            g(S_MARGIN_HIST + 1),
            b[2],
            g(S_MARGIN_HIST + 2),
            b[3],
            g(S_MARGIN_HIST + 3),
            b[4],
            g(S_MARGIN_HIST + 4),
            g(S_MARGIN_HIST + 5),
            g(S_MARGIN_MAX),
            g(S_MARGIN_UNKNOWN),
            g(S_BACKLOG_MAX),
        );
        format!("{main}\n{}", self.ff_text())
    }

    /// 早送りの行（観戦者の遅れの次の行）。閾値と上限も出す（どの値で走ったかが本ログだけで分かるように）。
    fn ff_text(&self) -> String {
        let g = |i: usize| self.get(i);
        let secs = |ms: u32| format!("{:.1} 秒", f64::from(ms) / 1_000.0);
        let first = match g(S_FF_FIRST_MS) {
            NO_VALUE => "まだ".to_string(),
            ms => format!("{ms} ms（{}）", secs(ms)),
        };
        let enter = match g(S_FF_ENTER_BACKLOG) {
            NO_VALUE => "なし".to_string(),
            n => format!("{n} フレーム"),
        };
        format!(
            "観戦者の早送り: 設定（溜まりが {FF_ENTER_FRAMES} フレームを越えたら入り {FF_EXIT_FRAMES} 以下で戻る・1 表示に余分に回す更新は最大 {FF_MAX_EXTRA} 回・余分に使う時間の上限 {FF_BUDGET_US} µs） / 走査の包みを通った表示 {}（0 のまま観戦者の tick が進むなら早送りの経路が無い） / 早送りした表示 {}（余分に回した更新 {}・1 表示の余分の時間の最大 {} µs） / 追いつくまで（入ってから戻るまで）最初の 1 回 {first}・入った回数 {}（戻った回数 {}・最初に入ったときの溜まり {enter}）・最長 {} ms・合計 {} ms / いま早送り中 {} / 時間の上限で打ち切った {} / SE・文字の数を整えられず早送りをやめた {}",
            g(S_FF_BEGUN),
            g(S_FF_DISPLAYS),
            g(S_FF_EXTRA),
            g(S_FF_BATCH_MAX),
            g(S_FF_EPISODES),
            g(S_FF_FINISHED),
            g(S_FF_MAX_MS),
            g(S_FF_TOTAL_MS),
            if g(S_FF_ACTIVE) != 0 {
                "はい"
            } else {
                "いいえ"
            },
            g(S_FF_CUTOFFS),
            g(S_FF_DROP_FAIL),
        )
    }
}

impl Default for WatchStatus {
    fn default() -> Self {
        Self::new()
    }
}

pub static WATCH_STATUS: WatchStatus = WatchStatus::new();

/// 観戦者のセッションを置いた（`script_win::arm_watch`）。読み取りのスレッドが `_watch_frames.csv` を開くかの印。
static ARMED: AtomicBool = AtomicBool::new(false);

pub fn armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

pub(crate) fn set_armed() {
    ARMED.store(true, Ordering::Release);
}

const PHASES: [&str; 6] = [
    "コードか返事を待つ",
    "選択",
    "止める地点",
    "読み込み",
    "試合",
    "終えた",
];

const ENDS: [&str; 9] = [
    "まだ",
    "ホストの終わりの記録",
    "ホストが黙った",
    "使えるコードが貼られなかった",
    "記録の SELECT が範囲の外",
    "押下を持ち越しすぎた（画面がホストと合わない）",
    "止める地点で SELECT が揃わなかった",
    "試合の途中で流れが切れた",
    "自分で抜けた（Esc）",
];

pub fn end_code(e: Option<WatchEnd>) -> (u32, u32) {
    match e {
        None => (0, 0),
        Some(WatchEnd::HostEnded(r)) => (1, r as u32),
        Some(WatchEnd::HostSilent) => (2, 0),
        Some(WatchEnd::GaveUp) => (3, 0),
        Some(WatchEnd::BadSelect(b)) => (4, b.value as u32),
        Some(WatchEnd::Stuck(st)) => (5, u32::from(st.step)),
        Some(WatchEnd::HoldTimeout) => (6, 0),
        Some(WatchEnd::Lost) => (7, 0),
        Some(WatchEnd::Quit) => (8, 0),
    }
}

pub const SHOT_EVERY_TICKS: u32 = 600;

/// 観戦者の実行で撮影を頼む tick の判断（pure）。台本の段階は画面 6 で終わるので、台本の `PRE` / `POST` では画面 7 以降が撮れない。
/// 段階が変わった tick・選択の画面が変わった tick・マッチの中の `SHOT_EVERY_TICKS` tick ごとに頼む。理由のビットは台本が 8 つ全部使って
/// いて足せないので、`TRANSITION`・`TRANSITION | SETTLE`・`WAIT` を流用する。観戦者の撮影だと分かるのは `_shots.csv` の step 列
/// （0xF000 台）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ShotPacer {
    seen: Option<(Phase, Option<u8>)>,
    in_match: u32,
}

impl ShotPacer {
    /// 撮影の理由のビット（0 なら頼まない）を返す。毎 tick 1 回、`on_input` の後に呼ぶ。最初の tick は基準を覚えるだけ。
    pub fn tick(&mut self, phase: Phase, screen: Option<u8>) -> u8 {
        use higanr_core::script::{SETTLE, TRANSITION, WAIT};
        let Some((p0, s0)) = self.seen.replace((phase, screen)) else {
            return 0;
        };
        if p0 != phase {
            self.in_match = 0;
            return TRANSITION;
        }
        if phase == Phase::Selecting && s0 != screen {
            return TRANSITION | SETTLE;
        }
        if phase == Phase::Lockstep {
            self.in_match += 1;
            if self.in_match.is_multiple_of(SHOT_EVERY_TICKS) {
                return WAIT;
            }
        }
        0
    }
}

pub fn shot_step_code(phase: Phase, screen: Option<u8>) -> u16 {
    0xF000 | ((phase_code(phase) as u16 & 0xF) << 8) | u16::from(screen.unwrap_or(0))
}

#[cfg(test)]
mod tests;
