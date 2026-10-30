//! 観戦者の側: ホストが送る記録（`wire::Record`）を、観戦者のゲームへ「両サイドとも相手の入力」として渡す、入力の地点の判断（pure）。
//! 対戦の`netplay::NetPlay`と並ぶもの。観戦者は対戦のセッション`Link`を作らず、ホストへJOINするだけ。
//!
//! 受け取り（復号・組み立て・ACK・JOINの再送・BYE）は受信スレッドの`recv::WatchRecv`がcoreの`Watcher`ごと持つ。
//! 連続になった記録は約1時間ぶんのリング（`feed::WatchFeed`）でゲームのスレッドへ渡す。
//! ゲームのスレッドが止まっている間も受け続けるので、ホストの再送と沈黙による切り離しが増えない。
//! ゲームのスレッドは記録を1 tickに1つずつ使うだけ。確保はしない。
//! リングは`WatchPlay::try_new`が、`Watcher`は`WatchSlot::offer`が、観戦者のセッションを作るスレッドで確保する。
//! 段階（`phase`）: Unconnected → Selecting（選択の画面でホストの記録の列を両サイドの入力として渡す）→ Holding（止める地点で両サイドの`Select`が揃うまで描画だけのフレーム）→ Loading → Lockstep（`Frame`のワードを3本ともfeed）→ Ended。
//! `on_input`は`tend`（受け取りの状態を見る）→ `esc_edge`（Escの読み取り）→ `dispatch`（判断）に分けてある。
//! Esc: マッチの中では手元のゲームのポーズを開く。開けない間は重ね描きの一時停止にする（`spectate_watch/pause.rs`）。
//! マッチの外では3秒以内に2回押すと観戦をやめる。
//! 観戦者の遅れは、ホストの送信（`spectate_host.rs`）＋ 片道 ＋ **余裕**。
//! 余裕は`WatchConfig::margin_frames`で、マッチの1フレーム目を使う前に溜める記録の数（既定 `WATCH_MARGIN_FRAMES` = 3）。
//! 遅れてJOINした観戦者は選択の画面で遅れを詰める（`catch_up_menus`）。マッチのワードは早送りで追いつく（`spectate_watch/ff.rs`）。
//! 遅れは観戦者だけに掛かる。相手には掛からない。
//! ゲーム内のメニューの経路（観戦の「ルームマッチ」）では観戦者のセッションを後から組み込む（`script_win::install_placed`）。
//! メニューが画面6で入力を占有するtick（`net_ui::watch_input`）も`tend`を毎tick呼ぶ（最初の`tend`が受け取りを受信スレッドへ渡す）。
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

use higanr_core::candidate::Candidates;
use higanr_core::check::{
    INPUT_DIGEST_INIT, OwnCheck, WatchChecks, WatchVerdict, check_from_input_digest,
    input_digest_push, is_watch_check_frame,
};
use higanr_core::handshake::Name;
use higanr_core::input_model::MATCH_SEND_MASK;
use higanr_core::input_model::keys::{ESC, Z};
use higanr_core::lockstep::RESEND_MS;
use higanr_core::menu_model::{
    APPLY_CLEAR_BITS, DECIDE_BITS, Outcome, SCREEN_CHARA, SCREEN_DIFFICULTY, TRANSITION_BITS,
    outcome_of, uses_align,
};
use higanr_core::net_menu::PeerStats;
use higanr_core::rtt_probe::elapsed;
use higanr_core::seal::{KeySeed, PUBLIC_LEN};
use higanr_core::spectate::{
    AsmCounts, RelayKey, SpectateHost, Watcher, WatcherConfig, WatcherCounts,
};
use higanr_core::spectate_pace::{FF_UPDATES_MAX, Pace};
use higanr_core::wire::{ByeReason, CheckKind, Record, Seat, Select, Step, StepKind, WatchCheck};

use crate::net_portrait::Followed;
use crate::net_select::{self, MenuView};
use crate::net_ui::{HudNames, WatchHudFacts};
use crate::net_watch::phase_code;
use crate::netplay::peer_wait::{self, ESC_HOLD_MS};
use crate::netplay::{
    Action, BadValue, CursorWrite, EndPickRun, FrameRec, FrameTiming, Game, MENU_HOLD_LIMIT, Net,
    Phase, REQ_LEAVE_MATCH, REQ_REBUILD_MATCH, REQ_SAVE_AND_QUIT, SCENE_MATCH, StepLog, StepRec,
    write_plan_cursors,
};
use crate::scripted::SCENE_MENU;

mod feed;
mod ff;
mod pause;
mod recv;
pub use feed::{RelayCounts, WatchFeed, WatchUpnp};
use ff::FfState;
pub use ff::{FF_BUDGET_US, FF_ENTER_FRAMES, FF_EXIT_FRAMES, FfHost, FfStats, run_display};
use pause::LocalPause;
pub use pause::{ESC_HINT_KEEP_MS, PauseStats, WatchNotice};
pub use recv::{UpnpNews, WatchRecv};

/// マッチの1フレーム目を使う前に溜めておく記録の数（余裕）。
pub const WATCH_MARGIN_FRAMES: u32 = 3;
pub const START_HOLD_TICKS: u32 = 30;
/// 接続した後にホストから何も聞こえない時間（ms）がこれを越え、かつ溜めた記録を使い切ったか、記録をこの時間使わなかったときに見るのをやめる。
/// ホストに黙って外される時間（`spectate::SILENT_MS` = 10秒）の2倍。
/// 越えたらホストが終えた（か、外された後のJOINにも答えない）と読む。
pub const HOST_SILENT_END_MS: u32 = 20_000;
/// 止める地点で`Select`が両サイド揃うのを待ってよいtickの数。相手の待ち（`peer_wait::LIMIT_MS`）に10秒の余白を足した長さ（60 fps）。
/// 相手が最小化などで止まっても先に観戦者が見るのをやめない。
pub const HOLD_LIMIT_TICKS: u32 = (peer_wait::LIMIT_MS + 10_000) / 1000 * 60;
/// 先頭の`Step`の次の記録（そのtickの`Menu`）が届くのを待ってよいtickの数。来なければワード0を渡す。
pub const PAIR_WAIT_TICKS: u32 = 6;
/// 選択の画面で使っていない記録がこの数を**越えたら**遅れて入った観戦者とみなし、押下のない`Menu`を捨てて追いつく（`catch_up_menus`）。
/// 初めから観戦している観戦者の溜まりは余裕の3前後で、画面に入る演出が長いときでも6つ程度なので、この処理には入らない。
/// この処理に入るのは遅れて入った観戦者（実機で数千）だけ。
pub const MENU_CATCHUP_ABOVE: u32 = 30;
/// 追いつくときに残す記録の数（捨てた後の溜まり。余裕と同じ）。
pub const MENU_CATCHUP_KEEP: u32 = WATCH_MARGIN_FRAMES;
pub const HUD_DELAY_WINDOW: u32 = 30;
pub const MARGIN_BOUNDS_US: [u32; 5] = [8_000, 16_667, 33_333, 50_000, 100_000];
/// 検査で作る`WatchPlay`のリングの枠の数（`FEED_LEN`の8.8 MBを検査ごとに確保しない）。
#[cfg(test)]
const NEW_FEED_LEN: usize = 1 << 15;
#[cfg(not(test))]
const NEW_FEED_LEN: usize = feed::FEED_LEN;

/// 中継の配布する側のリングの枠の数（1枠36 Bで約9.4 MB。検査では小さくする）。
#[cfg(test)]
const RELAY_RING: usize = 1 << 12;
#[cfg(not(test))]
const RELAY_RING: usize = higanr_core::spectate::RING_CAP;

/// 受信スレッドへ渡す受け取りの中身: 観戦者と、中継できるなら中継の配布する側（`SpectateHost::relay`）。
pub(crate) struct Offered {
    pub(crate) watcher: Box<Watcher>,
    pub(crate) relay: Option<Box<SpectateHost>>,
}

/// 観戦者のセッションを作るスレッドから受信スレッド（`WatchRecv`）への`Watcher`の受け渡しと、接続・終わりのフラグ（監視の処理が読む）。
#[derive(Default)]
pub struct WatchSlot {
    /// 設定の世代（0 = まだ無い）。`cfg`の中の世代と同じ値を、`cfg`を書いた後にReleaseで書く。
    stamp: AtomicU32,
    cfg: Mutex<(u32, Option<Offered>)>,
    /// 中継する設定のときの自分の候補（JOINに載せる）。設定されていなければ中継しない。
    relay_own: OnceLock<Candidates>,
    /// UPnPの候補を足した自分の候補（受信スレッドが割り当てを突き合わせた後に1回。受け取りを替えても渡し直す）。
    upnp_own: OnceLock<Candidates>,
    /// 中継する設定なのに鍵の種を生成できない・リングを確保できないので中継しなかった回数。
    relay_failed: AtomicU32,
    /// 接続する前に元のホストからBYE(取消)を受けた（満員）。
    full_before_join: AtomicBool,
    /// ホストの最初のSTREAMを受けた（JOINが通った。監視の処理は終える）。
    joined: AtomicBool,
    /// 接続した経路がIPv6かどうか（`joined`を立てる前に書く）。
    via_v6: AtomicBool,
    /// 観戦が終わった（監視の処理は終える）。
    done: AtomicBool,
    /// 監視の処理が期間内に使えるコードを読めずに諦めた。
    gave_up: AtomicBool,
    /// 溜めた記録が上限に達して観戦をやめた（`done`の前に立てる）。
    buffer_full: AtomicBool,
    /// 接続した後に元のホストに満員で断られて観戦をやめた（`done`の前に立てる）。
    refused_full: AtomicBool,
}

impl WatchSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 中継する（iniの`[spectate] relay = 1`）。`own`はJOINに載せる自分の候補。最初の`offer`の前に呼ぶ。
    pub fn enable_relay(&self, own: Candidates) {
        let _ = self.relay_own.set(own);
    }

    /// 設定から`Watcher`を作って置く。中継する設定なら中継の配布する側も作る（確保と鍵の計算。呼んだスレッドで走る）。
    /// 受け取る前に次が来たら古い方を捨てる。
    pub fn offer(&self, c: WatcherConfig) {
        let seed = self
            .relay_own
            .get()
            .map(|_| crate::net_udp::draw_key_seed());
        self.offer_with(c, seed);
    }

    /// `offer`の中身。`seed`は中継用の鍵の種（中継しない設定ならNone）。
    pub(crate) fn offer_with(&self, c: WatcherConfig, seed: Option<Result<KeySeed, String>>) {
        let mut watcher = Box::new(Watcher::new(c));
        let mut relay = None;
        if let (Some(own), Some(seed)) = (self.relay_own.get(), seed) {
            match seed.ok().and_then(|s| relay_side(&c, &s)) {
                Some((side, public)) => {
                    watcher.set_relay_public(Some(public));
                    watcher.set_candidates(*own);
                    relay = Some(side);
                }
                None => {
                    self.relay_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.clear_full_before_join();
        let mut g = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        let next = g.0.wrapping_add(1).max(1);
        let old = std::mem::replace(&mut *g, (next, Some(Offered { watcher, relay })));
        self.stamp.store(next, Ordering::Release);
        drop(g);
        drop(old);
    }

    pub(crate) fn set_upnp_own(&self, own: Candidates) {
        let _ = self.upnp_own.set(own);
    }

    pub(crate) fn upnp_own(&self) -> Option<Candidates> {
        self.upnp_own.get().copied()
    }

    /// 中継する設定かどうか。
    pub fn relay_enabled(&self) -> bool {
        self.relay_own.get().is_some()
    }

    pub fn relay_failed(&self) -> u32 {
        self.relay_failed.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_full_before_join(&self) {
        self.full_before_join.store(true, Ordering::Release);
    }

    /// 新しい受け取りの断りのフラグは古い受け取りのものを引き継がない。
    pub(crate) fn clear_full_before_join(&self) {
        self.full_before_join.store(false, Ordering::Release);
    }

    /// 接続する前にホストに満員で断られたかどうか（JOINに返事が無かったのではない）。
    pub fn full_before_join(&self) -> bool {
        self.full_before_join.load(Ordering::Acquire)
    }

    /// 受信スレッド: `seen`より新しい受け取りがあれば取る。ロックを待たない（取れなければ次の周回）。
    fn take_newer(&self, seen: &mut u32) -> Option<Offered> {
        if self.stamp.load(Ordering::Acquire) == *seen {
            return None;
        }
        let mut g = match self.cfg.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        *seen = g.0;
        g.1.take()
    }

    pub fn mark_buffer_full(&self) {
        self.buffer_full.store(true, Ordering::Release);
    }

    pub fn buffer_full(&self) -> bool {
        self.buffer_full.load(Ordering::Acquire)
    }

    pub(crate) fn mark_refused_full(&self) {
        self.refused_full.store(true, Ordering::Release);
    }

    pub fn refused_full(&self) -> bool {
        self.refused_full.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u32 {
        self.stamp.load(Ordering::Acquire)
    }

    pub fn mark_joined(&self) {
        self.joined.store(true, Ordering::Release);
    }

    /// 接続した経路（最初のSTREAMの送り元の族）を設定する。`mark_joined`の前に呼ぶ。
    pub fn set_via_v6(&self, v6: bool) {
        self.via_v6.store(v6, Ordering::Relaxed);
    }

    pub fn via_v6(&self) -> bool {
        self.via_v6.load(Ordering::Relaxed)
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

/// 中継の配布する側とJOINに載せる中継用の公開鍵を作る。リングを確保できそうになければNone。
fn relay_side(c: &WatcherConfig, seed: &KeySeed) -> Option<(Box<SpectateHost>, [u8; PUBLIC_LEN])> {
    // `SpectateHost`のリングは確保に失敗するとプロセスごと落ちる。32 bitのアドレス空間では続けて確保できないことがあるので、同じ大きさを先に試す。
    let mut probe: Vec<Record> = Vec::new();
    probe.try_reserve_exact(RELAY_RING).ok()?;
    drop(probe);
    let key = RelayKey::new(seed);
    let public = key.public();
    let side = SpectateHost::relay_with_capacity(c.session_id, c.secret, key, RELAY_RING);
    Some((Box::new(side), public))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
    /// ワードが届いていないフレームで待つ予算（µs。iniの`wait_budget_us`）。
    pub wait_budget_us: u32,
    /// マッチの1フレーム目を使う前に溜める記録の数（`WATCH_MARGIN_FRAMES`）。
    pub margin_frames: u32,
    /// 選択の画面を持つゲーム（本物）かどうか。falseは偽のゲーム。
    /// trueのとき、マップの決定が記録で入っていない場面2（タイトルのデモなど）では止める地点に入らず`Local`になる。
    /// また、接続する前の選択の画面では決定を落とす。
    pub require_net_selection: bool,
    /// 早送りの速さ（1表示あたりの更新の数。iniの`[spectate] ff_max`。1〜`FF_UPDATES_MAX`）。上限は`Pace`がこれから決める。
    pub ff_max: u32,
}

/// 観戦をやめた理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEnd {
    /// ホストの`End`（マッチの終わり・ホストの終了・ズレ検出・切断など）。
    HostEnded(ByeReason),
    /// 接続した後にホストから`HOST_SILENT_END_MS`聞こえなかった。
    HostSilent,
    /// 使えるコードが期間内に貼られなかった（監視の処理が諦めた）。
    GaveUp,
    /// 記録のSELECTの値が範囲の外だった（1つも書かずにやめる）。
    BadSelect(BadValue),
    /// 持っている押下（先頭の`Step`）を`MENU_HOLD_LIMIT` tickゲームに渡せなかった（画面がホストと合わない。辿れないマッチなど）。
    Stuck(Step),
    /// 止める地点で`Select`が`HOLD_LIMIT_TICKS` tick揃わなかった。
    HoldTimeout,
    /// マッチの途中でホストの流れがマッチの始まりへ飛んだ（外されてJOINし直した）か、途中に`Seg`が来た。続けられない。
    Lost,
    /// 観戦者が自分で抜けた（マッチの外のEscを3秒以内に2回か、ゲームのポーズの「タイトルへ戻る」）。BYEは受信スレッドが次の周回で出す。
    Quit,
    /// 溜めた記録が受け渡しのリングの上限（`FEED_LEN`。約1時間）に達した。
    BufferFull,
    /// 受信スレッドの観戦者の処理がパニックして止まった（受信スレッドは生きている）。
    Broken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WatchStats {
    pub ticks: u64,
    pub played: u64,
    /// 0をfeedしたtick（止める地点・読み込み・再戦の切替・マッチを出るtick）。
    pub zero_ticks: u64,
    /// ワードが届いていなくて描画だけのフレームにした回数とその最長の連続。
    pub stalls: u64,
    pub max_stall_run: u32,
    /// マッチの途中で止まった後に溜まりが戻るのを待った描画だけのフレーム（`spectate_pace::Pace::may_play`）。
    pub refill_ticks: u64,
    /// マッチの1フレーム目の前に、余裕が溜まるのを待った描画だけのフレーム。
    pub start_hold_ticks: u64,
    /// 止める地点で`Select`を待った描画だけのフレーム。
    pub hold_frames: u64,
    /// 選択の画面で記録の列からfeedしたフレーム（`Menu`のフレームと、押下を渡したフレーム）。
    pub menu_frames: u64,
    pub menu_applied: u32,
    /// 先頭の押下をゲームが読まないので持ち越したフレーム（入る演出の間）。
    pub menu_held: u64,
    /// 遅れて入った観戦者が画面7から画面8まで自分で進めるために押した決定の数（`walk_to_chara`）。
    pub menu_walked: u32,
    /// 遅れて入って選択の画面で追いつくために捨てた`Menu`の記録（`catch_up_menus`）。
    pub menu_skipped: u64,
    pub menu_align_miss: u32,
    pub menu_portrait_called: u32,
    pub menu_portrait_refused: u32,
    /// 接続する前に選択の画面で落とした決定の押下（押した瞬間）。
    pub pre_join_decides: u32,
    pub segs: u32,
    pub matches_started: u32,
    pub selects: u32,
    /// 使えなかった記録: 選択の画面の先頭に来た`Frame`（選択の記録の無いマッチ）/ 止める地点やマッチの中に来た種類違いの記録。
    pub orphan_frames: u64,
    pub names: u32,
    /// 受けた対戦の状態の記録（`Record::Stats`。ホストがマッチの最中に約1秒ごとに積む）。
    pub peer_stats: u32,
    pub stray_records: u64,
    /// 使った`Frame`の番号が直前の続きでなかった回（0のはず）。
    pub frame_gaps: u64,
    pub charge_write_failures: u32,
    /// ネットワークの選択の合意なしに場面2へ移ったtick（タイトルのデモなど）。
    pub local_scene2: u32,
    pub wakes: u64,
    pub margin_hist: [u64; 6],
    pub margin_max_us: u32,
    pub margin_unknown: u64,
    pub margin_sum_us: u64,
    /// 使う直前に溜まっていた記録の最大（追いつくまでの遅れの目安）。
    pub backlog_max: u32,
    /// 消費のあとホストの流れが飛んだ回（`Assembler`のreset。最初の1回は数えない）。
    pub jumps: u32,
    /// 接続するまでのms（最初のSTREAMを受けるまで。コードを取ってから）。
    pub join_wait_ms: Option<u32>,
    pub ff: FfStats,
    pub pause: PauseStats,
    /// ホストの照合の記録（`Record::Check`）と自分の値が合った数・ずれた数と、最初のずれ（作り方・ホストの値・自分の値）。
    pub checks_matched: u32,
    pub checks_mismatched: u32,
    pub first_mismatch: Option<(CheckKind, WatchCheck, WatchCheck)>,
}

fn ms(us: u64) -> u32 {
    (us / 1_000) as u32
}

pub struct WatchPlay {
    cfg: WatchConfig,
    slot: Arc<WatchSlot>,
    feed: Arc<WatchFeed>,
    /// 受信スレッドへ渡す前の受け取り（最初の`tend`で`Net::watch_install`へ渡す）。
    recv: Option<WatchRecv>,
    phase: Phase,
    end: Option<WatchEnd>,
    bye: Option<ByeReason>,
    /// 接続してから自分のEscが押されていないのを1度見たかどうか（押しっぱなしで接続したときは離すまで抜けない）。
    esc_armed: bool,
    /// 前のtickに自分のEscが押されていたかどうか（押した瞬間を取る）。
    esc_held: bool,
    /// Escを押し始めた時刻（ms。離したら`None`）。`ESC_HOLD_MS`押し続けたら観戦をやめる。
    esc_since_ms: Option<u32>,
    /// Escを離した時刻（ms）。マッチの外の案内をその後`ESC_HINT_KEEP_MS`残す。
    esc_released_ms: Option<u32>,
    /// マッチの中の一時停止（手元のゲームのポーズか、重ね描きの一時停止）。
    pause: LocalPause,
    /// このマッチでゲームのポーズが崩れた（開かなかった・閉じるtickが外れた）。マッチの残りは重ね描きの一時停止だけを使う。
    pause_broken: bool,
    /// 今のマッチで最後に使った`Frame`のワード（1P, 2P）。
    last_words: (u16, u16),
    /// マッチの終わりの選択（対戦する2人と同じステートマシンを記録のワードで回す）。`end_live`はマッチに入ってから縁取りを消していない間。
    end_pick: EndPickRun,
    end_live: bool,
    /// マッチの中で観戦を終えた（ポーズの「タイトルへ戻る」・Escの長押し・ポーズの間の終わり）。場面がマッチを出るまで`[2]`だけをこちらが作る。
    /// 本物のキーボードでは試合が進まないようにする（`pause::leaving_tick`）。
    leaving: bool,
    /// `leaving`の間に押した瞬間を作るための交互に切り替わるフラグ。
    leave_pulse: bool,
    /// 照合: ホストの記録と自分の値のペアと、マッチのframe 0からの入力のダイジェスト。
    checks: WatchChecks,
    check_digest: u64,
    /// 最後に読んだホストのCheckの記録の位置（`WatchFeed::read_pos`。ずれを受信スレッドへ渡すときに記録を運んだ親を見分ける）。
    check_pos: u32,
    /// このマッチでずれを受信スレッドへ渡した（マッチの最初のずれだけ。後のずれは同じずれの続き）。
    seg_mismatched: bool,
    // ---- 記録の先読み（先頭の2つ。`Step`の次の`Menu`を見るため。数は記録が使えるようになった時刻µsの下位32 bit）----
    look: [Option<(u32, Record)>; 2],
    popped_any: bool,
    /// 記録を使った後に流れが飛んだ（リングの先頭が飛んだ後の最初の記録）。次の手入れで`Lost`にする。
    lost_pending: bool,
    /// いまのtickのゲームの時計（ms。`watch_link`が書く）と前に見た時刻。
    now_ms: u32,
    seen_tick_ms: Option<u32>,
    /// 最後に記録を使った時刻（ms。接続したときに最初の値を書き込む）。
    used_ms: Option<u32>,
    /// 最後に消費した`Seg`のseg（`Frame`のseg）と、次に来るはずのframe。
    seg: Option<u16>,
    /// サイドごとの名前（観戦の流れの`Name`の記録。1P・2P。接続の間変わらないのでマッチをまたいで持つ）。
    names: [Option<Name>; 2],
    /// 今のマッチで受けた名前（両サイドそろったら`names`へ書き込んで固定する。次の`Seg`で空に戻す）。
    /// 送り手が名前を変え続けても、保存先のフォルダ（`<1P> vs <2P>`）がマッチごとに1つに決まるように。
    seg_names: [Option<Name>; 2],
    names_locked: bool,
    /// 対戦する2人の間の状態の最新の値（観戦の流れの`Stats`の記録。新しいマッチの始まりで捨てる。受けるまでは`None`）。
    peer_stats: Option<PeerStats>,
    expect_frame: u32,
    // ---- 選択 ----
    menu_active: bool,
    aligned: Option<u8>,
    map_decided: bool,
    hold_run: u32,
    /// 先頭の`Step`の次の記録（そのtickの`Menu`）を待ったtickの数。
    pair_wait: u32,
    /// 今のマッチでゲームに渡した押下の数。0のまま先頭の押下が画面8のときは画面7にいる観戦者が遅れて入ったマッチ（`walk_to_chara`）。
    seg_steps: u32,
    /// 次の選択待ち。選択の記録の無いマッチのワードを捨てる（選択の記録を1つ使えば戻す）。
    awaiting_select: bool,
    pre_join_decide_held: bool,
    steps: StepLog,
    sel: [Option<Select>; 2],
    written: Option<[Select; 2]>,
    charge: [u8; 2],
    // ---- マッチ ----
    /// 今のマッチの1フレーム目を使ったかどうか（余裕を待ち終えたか）。
    started: bool,
    start_hold: u32,
    stall_run: u32,
    timing: FrameTiming,
    stats: WatchStats,
    ff: FfState,
    /// 早送りの速さと止まった後の余裕の作り直し（1表示ごとに届いた記録の数を渡す）。
    pace: Pace,
    /// 前の表示の始まりに見たリングに書かれた数（差がその表示の間に使えるようになった記録の数）。
    seen_written: u32,
    /// 画面の通信の状態の行に出す遅れ（`hud_facts`）: 使う直前の溜まりの、直近`HUD_DELAY_WINDOW`フレームでの最大値。
    /// そのまま出すと数字がちらつくので、上がるときはすぐ反映し、下がるときは期間が閉じるときにコピーする。
    hud_delay: u32,
    /// いまの期間の最大と期間で使ったフレームの数。
    hud_win_max: u32,
    hud_win_n: u32,
    /// マッチ終了時のメニューの「リプレイを保存して終了」（今2・次6）を決定したtickに立つ。`take_save_quit`で1回だけ取る。
    save_quit: bool,
    /// 観戦が終わった後（`Ended`）の前のtickに「今2・次6」を見たかどうか（立ち上がりだけで頼むため）。
    ended_save_seen: bool,
    /// 検査だけ: 使える記録の数を固定する（溜まりが多い状態を通信を通さずに作る）。
    #[cfg(test)]
    test_backlog: Option<u32>,
}

impl WatchPlay {
    /// 作る（観戦者のセッションを開くスレッド。受け渡しのリング`FEED_LEN`枠を確保する）。
    pub fn try_new(slot: Arc<WatchSlot>, cfg: WatchConfig) -> Result<Self, String> {
        Self::try_with_feed_len(slot, cfg, NEW_FEED_LEN)
    }

    #[cfg(test)]
    pub fn new(slot: Arc<WatchSlot>, cfg: WatchConfig) -> Self {
        Self::try_new(slot, cfg).expect("検査の大きさなら確保できる")
    }

    #[cfg(test)]
    pub fn with_feed_len(slot: Arc<WatchSlot>, cfg: WatchConfig, len: usize) -> Self {
        Self::try_with_feed_len(slot, cfg, len).expect("検査の大きさなら確保できる")
    }

    /// リングの枠の数を指定して作る。確保できなければ理由の文。
    pub(crate) fn try_with_feed_len(
        slot: Arc<WatchSlot>,
        cfg: WatchConfig,
        len: usize,
    ) -> Result<Self, String> {
        let feed =
            Arc::new(WatchFeed::try_with_len(len).ok_or_else(|| {
                format!("観戦者の受け渡しのリングバッファ（{len} 記録）の確保不能")
            })?);
        let recv = WatchRecv::new(Arc::clone(&slot), Arc::clone(&feed), cfg.ff_max);
        Ok(WatchPlay {
            cfg,
            slot,
            feed,
            recv: Some(recv),
            phase: Phase::Unconnected,
            end: None,
            bye: None,
            esc_armed: false,
            esc_held: false,
            esc_since_ms: None,
            esc_released_ms: None,
            pause: LocalPause::Off,
            pause_broken: false,
            last_words: (0, 0),
            end_pick: EndPickRun::default(),
            end_live: false,
            leaving: false,
            leave_pulse: false,
            checks: WatchChecks::default(),
            check_digest: INPUT_DIGEST_INIT,
            check_pos: 0,
            seg_mismatched: false,
            look: [None; 2],
            popped_any: false,
            lost_pending: false,
            now_ms: 0,
            seen_tick_ms: None,
            used_ms: None,
            seg: None,
            names: [None; 2],
            seg_names: [None; 2],
            names_locked: false,
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
            stats: WatchStats::default(),
            ff: FfState::default(),
            pace: Pace::with_updates(cfg.ff_max),
            seen_written: 0,
            hud_delay: 0,
            hud_win_max: 0,
            hud_win_n: 0,
            save_quit: false,
            ended_save_seen: false,
            #[cfg(test)]
            test_backlog: None,
        })
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

    /// 次のキャラ選択まで映らない（ホストが選択のマッチまで遡れなかった）。画面に`T_SPECTATE_NEXT_SELECT`を出す間。
    pub fn awaiting_select(&self) -> bool {
        self.phase == Phase::Selecting && self.awaiting_select
    }

    pub fn stats(&self) -> WatchStats {
        self.stats
    }

    pub fn step_log(&self) -> &StepLog {
        &self.steps
    }

    /// 最後にゲームへ書いた両サイドのSELECT（検査が見る）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn written(&self) -> Option<[Select; 2]> {
        self.written
    }

    pub fn last_timing(&self) -> FrameTiming {
        self.timing
    }

    pub fn watcher_counts(&self) -> Option<WatcherCounts> {
        self.feed.counts()
    }

    pub fn assembler_counts(&self) -> Option<AsmCounts> {
        self.feed.asm_counts()
    }

    pub fn relay_counts(&self) -> Option<RelayCounts> {
        self.feed.relay_counts()
    }

    pub fn is_joined(&self) -> bool {
        self.feed.is_joined()
    }

    pub fn host_addr(&self) -> Option<std::net::SocketAddr> {
        self.feed.host_addr()
    }

    pub fn feed(&self) -> &Arc<WatchFeed> {
        &self.feed
    }

    /// 検査だけ: まだ受信スレッドへ渡していない受け取り。
    #[cfg(test)]
    pub(crate) fn recv_for_test(&mut self) -> Option<&mut WatchRecv> {
        self.recv.as_mut()
    }

    // ---- 記録の先読み ----

    /// リングから記録を1つ取る。名前の記録と対戦の状態の記録は時間を使わないので、ここで処理して`names` / `peer_stats`に溜める。
    /// これらは先読みにも画面ごとの判断にも渡さない。
    /// 記録を使った後に流れが飛んだら飛んだ先の記録は取らず、`lost_pending`を立てる。
    fn watcher_pop(&mut self) -> Option<(u32, Record)> {
        loop {
            if self.lost_pending {
                return None;
            }
            let e = self.feed.peek()?;
            if e.jump && self.popped_any {
                self.lost_pending = true;
                return None;
            }
            let pos = self.feed.read_pos();
            self.feed.advance();
            self.popped_any = true;
            self.mark_used();
            match e.rec {
                Record::Name { seat, len, bytes } => self.take_name(seat, len, &bytes),
                Record::Stats {
                    ping_ms,
                    latency,
                    rollback,
                } => {
                    self.peer_stats = Some(PeerStats::from_record(ping_ms, latency, rollback));
                    self.stats.peer_stats = self.stats.peer_stats.saturating_add(1);
                }
                Record::Check { kind, check } => {
                    self.check_pos = pos;
                    if let Some(v) = self.checks.on_host(kind, check) {
                        self.count_check(v);
                    }
                }
                rec => {
                    if matches!(rec, Record::Seg { .. }) {
                        self.seg_names = [None; 2];
                        self.names_locked = false;
                    }
                    return Some((e.t_us, rec));
                }
            }
        }
    }

    fn mark_used(&mut self) {
        self.used_ms = Some(self.now_ms);
    }

    fn count_check(&mut self, v: WatchVerdict) {
        let s = &mut self.stats;
        match v {
            WatchVerdict::Match { .. } => s.checks_matched = s.checks_matched.saturating_add(1),
            WatchVerdict::Mismatch { kind, host, own } => {
                s.checks_mismatched = s.checks_mismatched.saturating_add(1);
                s.first_mismatch.get_or_insert((kind, host, own));
                // ペアになったホストの値はいつも最後に読んだCheckの記録（`WatchChecks`は1つしか持たない）。
                if !self.seg_mismatched {
                    self.seg_mismatched = true;
                    self.feed
                        .request_mismatch(self.check_pos, self.pause_broken);
                }
            }
        }
    }

    /// 照合: このフレームをfeedする前の自分の値（ゲームの値とマッチのframe 0からのダイジェスト）を作り、ホストの記録とペアにする。
    /// ゲームの値を読むのは約1秒に1回（`is_watch_check_frame`）。
    fn own_check<G: Game>(&mut self, game: &mut G, frame: u32, p1: u16, p2: u16) {
        if frame == 0 {
            self.check_digest = INPUT_DIGEST_INIT;
        }
        self.check_digest = input_digest_push(self.check_digest, p1, p2);
        if !is_watch_check_frame(frame) {
            return;
        }
        let (rng_state, rng_count, life_1p, life_2p) = game.check_values();
        let own = OwnCheck {
            game: WatchCheck {
                frame,
                rng_state,
                rng_count,
                life_1p,
                life_2p,
            },
            digest: WatchCheck::from(&check_from_input_digest(0, frame, self.check_digest)),
        };
        if let Some(v) = self.checks.on_own(own) {
            self.count_check(v);
        }
    }

    /// 最後に記録を使ってからのms（接続する前は0）。
    fn unused_ms(&self) -> u32 {
        self.used_ms.map_or(0, |t| elapsed(self.now_ms, t))
    }

    /// 名前の記録を1つ受ける（`decode`がUTF-8・長さを検証してある。読めなければ空の名前にする）。
    fn take_name(&mut self, seat: Seat, len: u8, bytes: &[u8; higanr_core::wire::NAME_MAX]) {
        let text = bytes
            .get(..usize::from(len))
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("");
        let i = match seat {
            Seat::P1 => 0,
            Seat::P2 => 1,
        };
        self.stats.names = self.stats.names.saturating_add(1);
        if self.names_locked {
            return;
        }
        if let Some(slot) = self.seg_names.get_mut(i) {
            *slot = Some(Name::new(text));
        }
        if self.seg_names.iter().all(Option::is_some) {
            self.names = self.seg_names;
            self.names_locked = true;
        }
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

    /// 検査だけ: 受信スレッドを通さずに受け渡しのリングへ記録を1つ書く。
    #[cfg(test)]
    pub(crate) fn push_for_test(&self, rec: &Record) -> bool {
        self.feed.push(rec, false, 0)
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

    /// マッチ中の画面の表示の材料。マッチの中（`Phase::Lockstep`）でなければ`None`。名前は両サイドの記録を受けるまで`None`。
    /// ゲームのスレッドから毎tick呼ぶ。確保しない。
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
        let held = self.look.iter().flatten().count() as u32;
        held.saturating_add(self.feed.timed_len())
    }

    /// 終える。BYEは積むだけ。受信スレッドが次の周回でホストへ送信する（ホストのアドレスが分かっていれば。観戦者の枠を返す）。
    fn finish(&mut self, end: WatchEnd) {
        if self.phase == Phase::Ended {
            return;
        }
        // ポーズ・一時停止の間に終わったらその後の本物のキーボードで試合を進めない。
        if self.phase == Phase::Lockstep && self.pause != LocalPause::Off {
            self.leaving = true;
        }
        self.phase = Phase::Ended;
        self.end = Some(end);
        self.bye = match end {
            WatchEnd::HostEnded(_) => Some(ByeReason::Finished),
            WatchEnd::BadSelect(_)
            | WatchEnd::Stuck(_)
            | WatchEnd::HoldTimeout
            | WatchEnd::Lost
            | WatchEnd::Quit
            | WatchEnd::BufferFull => Some(ByeReason::Cancelled),
            // 受け取りが止まったのでホストへBYEを送る処理がない（ホストは沈黙で外す）。
            WatchEnd::HostSilent | WatchEnd::GaveUp | WatchEnd::Broken => None,
        };
        self.feed.end(self.bye);
        if end == WatchEnd::BufferFull {
            self.slot.mark_buffer_full();
        }
        self.slot.mark_done();
    }

    /// 接続している間に自分のEscが「押されていない → 押された」に変わったtickならtrue。
    /// 押しっぱなしで接続したときは押されていないのを1度見るまで押下も長押しも数えない（`esc_armed`。`peer_wait`と同じ）。
    /// `ESC_HOLD_MS`押し続けたらどこでも観戦をやめる（`quit_by_hold`）。
    /// 読むのは`Game::peek_local_input`（台本をこのtickの分だけ進める。1 tickに1回）。
    /// 早送りの余分な更新の中では読み取りを使わない（次の表示の普段の更新で取得する）。
    fn esc_went_down<G: Game>(&mut self, game: &mut G) -> bool {
        if matches!(self.phase, Phase::Unconnected | Phase::Ended) {
            return false;
        }
        let down = game.peek_local_input() & ESC != 0;
        if self.ff_in_extra() {
            return false;
        }
        if !down {
            if self.esc_held {
                self.esc_released_ms = Some(self.now_ms);
            }
            self.esc_armed = true;
            self.esc_held = false;
            self.esc_since_ms = None;
            return false;
        }
        if !self.esc_armed {
            return false;
        }
        let edge = !self.esc_held;
        self.esc_held = true;
        self.esc_released_ms = None;
        if edge && self.phase != Phase::Lockstep {
            self.stats.pause.esc_outside = self.stats.pause.esc_outside.saturating_add(1);
        }
        let since = *self.esc_since_ms.get_or_insert(self.now_ms);
        if elapsed(self.now_ms, since) >= ESC_HOLD_MS {
            self.quit_by_hold();
            return false;
        }
        edge
    }

    /// Escの長押しで観戦をやめる。マッチの中なら場面がマッチを出るまで`leaving`の扱い（ポーズを開いて「タイトルへ戻る」へ送る）。
    fn quit_by_hold(&mut self) {
        if self.phase == Phase::Lockstep {
            self.leaving = true;
        }
        self.quit_by_esc();
    }

    /// 受信スレッドの状態を見る: 接続した・黙った・流れが飛んだ・リングが満杯・受信スレッドの観戦者の処理が止まった。
    fn watch_link<N: Net>(&mut self, net: &mut N) {
        if self.phase == Phase::Ended {
            return;
        }
        let now = ms(net.now_us());
        // ゲームの時計が跳んだ（ゲームのスレッドが止まっていた）tickは止まっていた間を「使わなかった」と数えない。
        if self
            .seen_tick_ms
            .is_some_and(|t| now.wrapping_sub(t) >= HOST_SILENT_END_MS)
            && self.used_ms.is_some()
        {
            self.used_ms = Some(now);
        }
        self.seen_tick_ms = Some(now);
        self.now_ms = now;
        if self.feed.is_broken() {
            self.finish(WatchEnd::Broken);
            return;
        }
        if self.feed.is_full() {
            self.finish(WatchEnd::BufferFull);
            return;
        }
        let joined = self.feed.is_joined();
        // 監視の処理が諦めた。接続する前ならコードを取った後でも終える。接続した後は`gave_up`を見ない。
        if self.phase == Phase::Unconnected && self.slot.gave_up() && !joined {
            self.finish(WatchEnd::GaveUp);
            return;
        }
        if self.phase == Phase::Unconnected {
            if joined {
                self.phase = Phase::Selecting;
                self.stats.join_wait_ms = self.feed.join_wait_ms();
                self.mark_used();
            }
        } else if self
            .feed
            .silent_ms(now)
            .is_some_and(|s| s >= HOST_SILENT_END_MS)
            && (self.available() == 0 || self.unused_ms() >= HOST_SILENT_END_MS)
        {
            // 溜めた記録を使っている間は、ホストが黙っても終えない。使い切ったか、記録を使わない画面（タイトルなど）に留まっているなら終える。
            self.finish(WatchEnd::HostSilent);
            return;
        } else if self.feed.is_refused()
            && (self.available() == 0 || self.unused_ms() >= HOST_SILENT_END_MS)
        {
            // 落ちた中継役から戻ったが満員で断られた（以後は何も届かない）。黙ったときと同じく溜めた記録を使い切ってから終える。
            self.slot.mark_refused_full();
            self.finish(WatchEnd::HostEnded(ByeReason::Cancelled));
            return;
        }
        if self.lost_pending {
            self.stats.jumps = self.stats.jumps.saturating_add(1);
            self.look = [None; 2];
            self.finish(WatchEnd::Lost);
        }
    }

    // ---- 入力の地点 ----

    pub fn on_input<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        self.tend(net);
        let esc = self.esc_edge(game);
        self.dispatch(game, net, esc)
    }

    /// tickの始まり: 受け取りをまだ受信スレッドへ渡していなければ渡す（`Net::watch_install`。ロックを取れなければ次のtick）。
    /// 続けて受信スレッドの状態を見る。
    /// ゲーム内のメニューが画面6で入力を占有するtickも、毎tickこれを先に呼ぶ。確保しない・待たない。
    pub fn tend<N: Net>(&mut self, net: &mut N) {
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        if let Some(r) = self.recv.take()
            && let Err(r) = net.watch_install(r)
        {
            self.recv = Some(r);
        }
        self.watch_link(net);
    }

    /// 自分のEscが「押されていない → 押された」に変わったtickかどうか（接続している間だけ）。
    /// `peek_local_input`を1 tickに1回呼ぶので、台本がそのtickの分だけ進む。
    /// メニューが先に入力を占有する実行では`menu_words`で台本のワードを読む前にこれを呼ぶ（二重に進まない）。
    pub fn esc_edge<G: Game>(&mut self, game: &mut G) -> bool {
        self.esc_went_down(game)
    }

    /// 自分で抜ける。段階を`Ended`にし、BYEを受信スレッドに頼む。
    pub fn quit_by_esc(&mut self) {
        self.finish(WatchEnd::Quit);
    }

    /// 左上の1行に出す知らせ。優先の順は重ね描きの一時停止・長押しの案内・次のキャラ選択から。
    /// 長押しの案内はマッチの外でEscを押している間と、離してから`ESC_HINT_KEEP_MS`の間に出す。
    pub fn notice(&self) -> Option<WatchNotice> {
        if self.phase == Phase::Ended {
            return None;
        }
        if self.phase == Phase::Lockstep {
            return (self.pause == LocalPause::Overlay).then_some(WatchNotice::Paused);
        }
        let released = self
            .esc_released_ms
            .is_some_and(|t| elapsed(self.now_ms, t) < ESC_HINT_KEEP_MS);
        if self.esc_held || released {
            return Some(WatchNotice::EscHold);
        }
        self.awaiting_select().then_some(WatchNotice::NextSelect)
    }

    pub fn is_ended(&self) -> bool {
        self.phase == Phase::Ended
    }

    /// 観戦者のゲームが「リプレイを保存して終了」でマッチを出たtickかどうか（1回だけ真を返す）。
    /// 呼び手はこれを見たtickに、ゲームの保存の画面を飛ばすフラグを立てる。
    /// 観戦が先に終わった後に観戦者が自分の入力で決定したときも真。
    /// 再戦・キャラを変更・マッチの外では真にならない。
    pub fn take_save_quit(&mut self) -> bool {
        core::mem::take(&mut self.save_quit)
    }

    /// 検査の入口: 「保存して終了」でマッチを出たtickと同じフラグだけを立てる。呼び手の配線を本物の`take_save_quit`経由で見るため。
    #[cfg(test)]
    pub(crate) fn set_save_quit_for_test(&mut self) {
        self.save_quit = true;
    }

    /// 検査だけ: 段階を直接設定する（接続した後の段階をホストの流れを作らずに作る）。
    #[cfg(test)]
    pub(crate) fn set_phase_for_test(&mut self, phase: Phase) {
        self.phase = phase;
    }

    /// マッチの外へ出ていたらマッチの終わりの選択の縁取りと色の指示を1回だけ消す（ゲーム内のメニューが入力を占有するtickにも呼び手が呼ぶ）。
    pub fn end_leave<G: Game>(&mut self, game: &mut G) {
        if self.end_live && self.phase != Phase::Lockstep {
            self.end_pick.clear(game);
            self.end_live = false;
        }
    }

    pub fn dispatch<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N, esc: bool) -> Action {
        self.end_leave(game);
        // マッチの外の短いEscは何もしない（案内だけ。長押しは`esc_went_down`）。マッチの中のEscは`lockstep_tick`が見る。
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
            Phase::Lockstep => self.lockstep_tick(game, net, esc),
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
        // 読み込みのtickは3本とも0をfeedしてあるので、最初のFrameの前のワードは0（ホストも同じ）。
        self.last_words = (0, 0);
        self.end_live = true;
        self.pause = LocalPause::Off;
        self.pace.reset_seg();
        self.stats.matches_started = self.stats.matches_started.saturating_add(1);
    }

    /// 観戦が終わった後の1 tick。観戦者が自分の入力で「リプレイを保存して終了」（今2・次6）を決定したtickにも、保存の画面を飛ばすフラグを頼む。
    /// 頼むのは立ち上がりの1 tickだけ。
    fn ended_tick<G: Game>(&mut self, game: &mut G) -> Action {
        let (cur, next) = game.scenes();
        if self.leaving {
            if cur == SCENE_MATCH {
                return self.leaving_tick(game);
            }
            self.leaving = false;
        }
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
        self.end_pick.begin_seg();
        self.expect_frame = 0;
        self.seg_steps = 0;
        self.sel = [None; 2];
        self.peer_stats = None;
        self.checks.clear();
        self.seg_mismatched = false;
        self.pause_broken = false;
        self.stats.segs = self.stats.segs.saturating_add(1);
    }

    fn menu_tick<G: Game>(&mut self, game: &mut G) -> Action {
        let view = game.menu_view().filter(net_select::in_selection);
        let Some(v) = view else {
            self.menu_active = false;
            // 選択の画面の外（タイトルなど）では記録を使わないが、流れの終わりだけは見る（先頭まで使い切っていれば`End`）。
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
        // 合わせが届くまでのワードは、カーソルの値が食い違ったまま動かさないよう方向・Ctrl・Shiftを落とす（ゲストと同じ）。
        // キャラ選択には合わせが無いので、両サイドともワードのまま動かす（観戦者の画面の相手のサイドはワードの相対で動く。対戦する2人の側の位置は記録に無い）。
        // 記録のワードは送り手が決める。手元で働くP/Home（スクリーンショット）などは`MATCH_SEND_MASK`で落とす。
        let clear: u16 = if self.aligned == Some(screen8) || !uses_align(screen8) {
            MATCH_SEND_MASK
        } else {
            MATCH_SEND_MASK & !APPLY_CLEAR_BITS
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

    /// 遅れて入った観戦者の選択の追いつき: 使っていない記録が`MENU_CATCHUP_ABOVE`を越えている間は、先頭の押下のない`Menu`は1 tickを使わずに捨てて`MENU_CATCHUP_KEEP`まで詰める。
    /// 押下（`Step`）は1つも捨てない。
    /// 止める地点の`Select`が両サイドを上書きするので、飛ばしたワードはゲームの状態に残らない。
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

    /// 先頭の押下をゲームが読まない（か、そのtickの`Menu`がまだ来ない）ので持ち越す。3本とも0をfeedする。
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

    /// 遅れて入った観戦者の画面7 → 画面8。
    /// 1マッチ目の後のマッチは選択が画面8から始まる。
    /// しかしタイトルから自分で進んで画面7にいる観戦者のゲームはそのままでは進まない。
    /// そこで観戦者の側で決定（Z）を1 tick押して1 tick離し、画面8まで進める。
    /// 難易度は止める地点の`Select`が上書きする。回数は持ち越しと同じ`hold_run`で数える。
    fn walk_to_chara<G: Game>(&mut self, game: &mut G, v: &MenuView, st: Step) -> Action {
        self.hold_run = self.hold_run.saturating_add(1);
        self.stats.menu_held = self.stats.menu_held.saturating_add(1);
        if self.hold_run > MENU_HOLD_LIMIT {
            self.finish(WatchEnd::Stuck(st));
            return Action::Local;
        }
        // 画面7に入る演出の間（ゲームが押下を読まない）は押さない（読まれない決定は渡さない）。
        if self.hold_run % 2 == 1 && net_select::accepting(v) {
            self.stats.menu_walked = self.stats.menu_walked.saturating_add(1);
            // 画面7は合成`[2]`だけを読む（`net_select::apply_plan`が押下を`w[2]`にだけ書き込むのと同じ）。
            game.feed(0, 0);
            game.feed(1, 0);
            game.feed(2, Z);
        } else {
            Self::feed3(game, 0, 0);
        }
        Action::Menu
    }

    fn apply_step<G: Game>(&mut self, game: &mut G, v: &MenuView, st: &Step, m1: u16, m2: u16) {
        // 観戦者のサイドは未決定。両サイドのワードをサイドのまま並べる（`by_seat(P1, m1, m2)` = (m1, m2)）。
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
            // 決定・合わせは値を書き込めなかった（マップの値が自分の一覧に無い）。書かずに数える。
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
                    | Record::Stats { .. }
                    | Record::Check { .. },
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
        self.pause = LocalPause::Off;
    }

    fn lockstep_tick<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N, esc: bool) -> Action {
        let (cur, next) = game.scenes();
        let in_match = cur == SCENE_MATCH
            && !REQ_REBUILD_MATCH.contains(&next)
            && !REQ_LEAVE_MATCH.contains(&next);
        if in_match {
            if let Some(a) = self.pause_tick(game, esc) {
                return a;
            }
            let a = self.match_frame(game, net);
            self.after_close(a);
            return a;
        }
        if cur == SCENE_MATCH && REQ_REBUILD_MATCH.contains(&next) {
            self.leave_seg();
            self.phase = Phase::Loading;
            return self.zero(game);
        }
        if cur == SCENE_MATCH && next == REQ_SAVE_AND_QUIT {
            // マッチ終了時のメニューの「リプレイを保存して終了」。
            // 観戦者のゲームはこの後リプレイの保存の画面へ進むので保存の画面を飛ばすフラグを頼む。
            // 対戦の`NetPlay`は同じtickに閉じるが、観戦者は閉じない。
            self.save_quit = true;
        }
        // マッチを出る（今がマッチの場面でない・次の要求がマッチを出るもの）。
        self.leave_seg();
        self.phase = Phase::Selecting;
        self.menu_active = false;
        if cur != SCENE_MATCH {
            return Action::Local;
        }
        self.zero(game)
    }

    /// マッチの1フレーム: 次の`Frame`を使う（届いていなければ予算まで待ち、それでも無ければ描画だけのフレーム）。
    fn match_frame<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        let start = net.now_us();
        let deadline = start.saturating_add(u64::from(self.cfg.wait_budget_us));
        loop {
            self.refill();
            match self.head() {
                Some(Record::Seg { seg }) => {
                    if self.started {
                        // マッチの途中に次のマッチの始まりが来た。ホストは最後のマッチを出るときも`Seg(次)`を積み、直後に`End`が来る。
                        // 直後が`End`ならホストが終えただけ。付いていけなかったのではない。
                        match self.second() {
                            Some(Record::End(r)) => {
                                self.pop();
                                self.pop();
                                self.finish(WatchEnd::HostEnded(r));
                                return Action::Local;
                            }
                            // 本当に次のマッチへ進んだのに観戦者のゲームがマッチを続けている。辿れない。
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
                        // 止まった後: 溜まりが戻るまで待つ（1記録ずつ使うとすぐまた止まる）。
                        self.stats.refill_ticks = self.stats.refill_ticks.saturating_add(1);
                        return Action::RenderOnly;
                    }
                    let backlog = self.available();
                    let arrived_us = self.look[0].map_or(0, |(t, _)| t);
                    self.pop();
                    let wait_us =
                        net.now_us().saturating_sub(start).min(u64::from(u32::MAX)) as u32;
                    return self.play(game, net, (frame, p1, p2), arrived_us, backlog, wait_us);
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
        arrived_us: u32,
        backlog: u32,
        wait_us: u32,
    ) -> Action {
        // 記録のワードは送り手が決める。手元で働くP/Home（スクリーンショット）などは対戦の相手のワードと同じく落とす。
        let (p1, p2) = (p1 & MATCH_SEND_MASK, p2 & MATCH_SEND_MASK);
        if frame != self.expect_frame {
            self.stats.frame_gaps = self.stats.frame_gaps.saturating_add(1);
        }
        self.expect_frame = frame.wrapping_add(1);
        self.own_check(game, frame, p1, p2);
        self.end_pick
            .feed(game, self.seg.unwrap_or(0), frame, p1, p2);
        self.last_words = (p1, p2);
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
        // 時刻はµsの下位32 bit（約71分で折り返す。リングは約1時間ぶんなので使うまでの差はその中）。
        let margin_us = Some(elapsed(net.now_us() as u32, arrived_us));
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

/// 落とす決定のビットが`TRANSITION_BITS`に含まれることの確認用（`pre_join_tick`は`DECIDE_BITS`だけを落とす）。
const _: () = assert!(TRANSITION_BITS & DECIDE_BITS == DECIDE_BITS);

// ---------------------------------------------------------------- 読み取りのスレッドへのコピー

pub struct WatchStatus {
    w: [AtomicU32; WORDS],
    /// 中継の木の数（`feed::RELAY_LEN`ワード。受け取りが無ければ全部0）。
    relay: [AtomicU64; feed::RELAY_LEN],
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
const S_MARGIN_HIST: usize = 23; // 6ワード
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
const S_PORTRAIT: usize = 47; // 2ワード
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
// 選択の画面で遅れを詰めるために捨てた`Menu`（`catch_up_menus`）。
const S_MENU_SKIPPED: usize = 66;
// マッチの途中で止まった後、溜まりが戻るのを待った描画だけのフレーム。
const S_REFILL: usize = 67;
// ホストの穴あけの印（HGRTで始まらない便）を受けた数と、そのうちIPv6から来た数。接続した経路（0 = まだ・1 = IPv4・2 = IPv6）。
const S_NOT_HGRT: usize = 68;
const S_NOT_HGRT_V6: usize = 69;
const S_VIA: usize = 70;
// 早送りの速さの設定（1表示あたりの更新の数。iniの`ff_max`）。
const S_FF_UPDATES: usize = 71;
// 受け渡しのリングで読み取れなかった枠（0のはず）。
const S_FEED_BAD: usize = 72;
// マッチの中の一時停止（`spectate_watch/pause.rs`）。
const S_PAUSE_OPENED: usize = 73;
const S_PAUSE_TICKS: usize = 74;
const S_PAUSE_RESUMED: usize = 75;
const S_PAUSE_REOPENED: usize = 76;
const S_PAUSE_OPEN_SLIPS: usize = 77;
const S_PAUSE_CLOSE_SLIPS: usize = 78;
const S_PAUSE_UNREADABLE: usize = 79;
const S_PAUSE_QUITS: usize = 80;
const S_OVERLAYS: usize = 81;
const S_OVERLAY_TICKS: usize = 82;
const S_ESC_OUTSIDE: usize = 83;
// 照合（`Record::Check`）。最初のずれは8ワード: 作り方 + 1（0 = まだ）・frame・ホストの乱数・抽選回数・ライフ（1P | 2P << 8）・自分の3つ。
const S_CHECK_OK: usize = 84;
const S_CHECK_BAD: usize = 85;
const S_CHECK_FIRST: usize = 86;
// 崩れたマッチでゲームのポーズの代わりに重ね描きの一時停止にした回数。
const S_PAUSE_FALLBACKS: usize = 94;
const WORDS: usize = 95;
const NO_VALUE: u32 = u32::MAX;

impl WatchStatus {
    pub const fn new() -> Self {
        WatchStatus {
            w: [const { AtomicU32::new(0) }; WORDS],
            relay: [const { AtomicU64::new(0) }; feed::RELAY_LEN],
        }
    }

    pub fn publish(&self, wp: &WatchPlay) {
        let s = wp.stats();
        let c = |v: u64| v.min(u64::from(u32::MAX)) as u32;
        let (end, mut detail) = end_code(wp.end());
        if wp.end() == Some(WatchEnd::Quit) && s.pause.quits > 0 {
            detail = QUIT_BY_PAUSE;
        }
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
        set(S_FF_UPDATES, wp.ff_updates());
        set(S_FEED_BAD, wp.feed().bad());
        let p = s.pause;
        set(S_PAUSE_OPENED, p.opened);
        set(S_PAUSE_TICKS, c(p.ticks));
        set(S_PAUSE_RESUMED, p.resumed);
        set(S_PAUSE_REOPENED, p.reopened);
        set(S_PAUSE_OPEN_SLIPS, p.open_slips);
        set(S_PAUSE_CLOSE_SLIPS, p.close_slips);
        set(S_PAUSE_UNREADABLE, p.unreadable);
        set(S_PAUSE_QUITS, p.quits);
        set(S_OVERLAYS, p.overlays);
        set(S_OVERLAY_TICKS, c(p.overlay_ticks));
        set(S_ESC_OUTSIDE, p.esc_outside);
        set(S_PAUSE_FALLBACKS, p.fallbacks);
        set(S_CHECK_OK, s.checks_matched);
        set(S_CHECK_BAD, s.checks_mismatched);
        if let Some((kind, h, o)) = s.first_mismatch {
            let lives = |w: &WatchCheck| u32::from(w.life_1p) | u32::from(w.life_2p) << 8;
            let words = [
                kind as u32 + 1,
                h.frame,
                u32::from(h.rng_state),
                h.rng_count,
                lives(&h),
                u32::from(o.rng_state),
                o.rng_count,
                lives(&o),
            ];
            for (i, v) in words.into_iter().enumerate() {
                set(S_CHECK_FIRST + i, v);
            }
        }
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
            set(S_NOT_HGRT, c(wc.not_hgrt));
            set(S_NOT_HGRT_V6, c(wc.not_hgrt_v6));
        }
        set(
            S_VIA,
            wp.host_addr()
                .map_or(0, |a| if a.is_ipv6() { 2 } else { 1 }),
        );
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
        if let Some(rc) = wp.relay_counts() {
            for (dst, v) in self.relay.iter().zip(feed::relay_words(&rc)) {
                dst.store(v, Ordering::Relaxed);
            }
        }
    }

    /// 中継の木の行（観戦者の一時停止と照合の行の次）。
    fn relay_text(&self) -> String {
        let mut w = [0u64; feed::RELAY_LEN];
        for (dst, src) in w.iter_mut().zip(self.relay.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        let c = feed::relay_counts_of(w);
        let t = c.tree;
        let setting = match (c.enabled, c.side.is_some()) {
            (false, _) => "無効（ini の relay = 0）",
            (true, true) => "有効",
            (true, false) => "有効・配信側の作成不能",
        };
        let side = match c.side {
            None => String::new(),
            Some(s) => format!(
                " / 配信側: いまの子 {}・受け入れた子 {}（BYE で抜けた {}・応答が無く外した {}・依頼のない JOIN {}）・積んだ記録 {}（自分の飛びで積み直した {}）・送った便 {}（記録 {}・送り直し {}）・子へ送った付け替え指示 {}・子の候補へ送った穴あけパケット {}",
                s.children,
                s.joined,
                s.left,
                s.silenced,
                s.not_admitted,
                s.pushed,
                s.relay_jumps,
                s.packets_sent,
                s.records_sent,
                s.resends,
                s.rehomes_sent,
                s.punches_sent,
            ),
        };
        // 照合のずれとUPnPは、数が1つでもあるときだけ足す。
        let mut more = String::new();
        if t.mismatch_returns + c.mismatch_stays > 0 {
            more.push_str(&format!(
                " / 照合のずれで中継役の下から戻った {}・戻らなかった {}",
                t.mismatch_returns, c.mismatch_stays
            ));
        }
        if c.upnp != WatchUpnp::NotAsked || c.announces_sent + c.announces_skipped > 0 {
            more.push_str(&format!(
                " / UPnP: {} / ホストへ知らせの JOIN 送った {}・送らなかった {}",
                c.upnp.text(),
                c.announces_sent,
                c.announces_skipped
            ));
        }
        format!(
            "観戦者の中継: {setting}（作成不能 {}） / 親の付け替え回数 {}・いまの親 {}・振り分けられた中継役に届かず直接接続だけに戻った {}・一度接続した後にホストに拒否された {} / ホストからの振り分け 受けた {}・受けなかった {} / ホストからの受け入れ依頼 受けた {}・受けなかった {} / 元のホストの鍵で開けなかった振り分け・依頼 {} / 中継役から受けた付け替え指示 {} / 振り分け後に中継役の流れが来ず戻った {}・中継役の流れが途切れて戻った {} / 上り回線速度の測定 ホストの依頼で始めた {}・受けなかった {}・ホストへ送った測定パケット {}{more}{side}",
            c.failed,
            c.epoch,
            if c.parent_is_home {
                "元のホスト"
            } else {
                "中継役"
            },
            if c.direct_only { "はい" } else { "いいえ" },
            if c.refused { "はい" } else { "いいえ" },
            t.intros_taken,
            t.intros_ignored,
            t.admits_taken,
            t.admits_ignored,
            t.control_dropped,
            t.rehomes,
            t.unreached,
            t.parent_silent,
            t.measures_taken,
            t.measures_ignored,
            t.pairs_sent,
        )
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
        let via = match g(S_VIA) {
            1 => "IPv4",
            2 => "IPv6",
            _ => "まだ",
        };
        let main = format!(
            "観戦者: 段階 {} / 終わり {}（{}） / tick {} / 使った試合のフレーム {} / 0 を feed {} / 入力ワードが届かず描画だけのフレーム {} 回（最長の連続 {}） / 止まった後に溜まりが戻るのを待った描画だけのフレーム {} / 試合の始めに余裕を待った描画だけのフレーム {} / 止める地点で待った {} / 選択のフレーム {}（入れた押下 {}・持ち越したフレーム {}・カーソルを書けなかった {}・立ち絵を追わせた {}・追わせなかった {}・遅れて入って画面 7 から 8 へ自分で押した決定 {}・遅れて入って追いつくために捨てた選択の語 {}） / 消費した seg の先頭 {}・受けた SELECT {}・始まった試合 {} / 使えなかった記録（選択の画面の先頭の試合の語 {}・種類違い {}）・番号が続かなかったフレーム {}・チャージ型を書けなかった {}\n\
             観戦者の受け: 接続するまで {join}（経路 {via}） / 受けた STREAM {}（送った JOIN {}・ACK {}） / 組み立て（入れた {}・重複 {}・バッファの範囲外 {}・飛び {}）・消費後の飛び {} / 捨てたもの（候補に無い送り元 {}・別の session {}・タグ {}・読めない {}） / ホストの穴あけパケット（HGRT で始まらない便）{}（うち IPv6 {}） / 受け渡しのリングバッファの読み取り不能枠 {}\n\
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
            g(S_NOT_HGRT),
            g(S_NOT_HGRT_V6),
            g(S_FEED_BAD),
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
        format!(
            "{main}\n{}\n{}\n{}",
            self.ff_text(),
            self.pause_text(),
            self.relay_text()
        )
    }

    /// 一時停止と照合の行（早送りの行の次）。
    fn pause_text(&self) -> String {
        let g = |i: usize| self.get(i);
        let first = match g(S_CHECK_FIRST) {
            0 => String::new(),
            k => {
                let f = |i: usize| g(S_CHECK_FIRST + i);
                let lives = |v: u32| format!("{}・{}", v & 0xFF, v >> 8 & 0xFF);
                format!(
                    "（最初のずれ: 作り方 {} / frame {} / ホスト 乱数 0x{:04X}・回数 {}・ライフ {} / 自分 乱数 0x{:04X}・回数 {}・ライフ {}）",
                    if k == CheckKind::Game as u32 + 1 {
                        "ゲームの値"
                    } else {
                        "入力のダイジェスト"
                    },
                    f(1),
                    f(2),
                    f(3),
                    lives(f(4)),
                    f(5),
                    f(6),
                    lives(f(7)),
                )
            }
        };
        format!(
            "観戦者の一時停止: ゲームのポーズ {} 回（{} tick・閉じる tick に次の frame を使った {}・閉じた直後にホストのポーズが開いた {}・タイトルへ戻るで終了 {}） / 異常（Esc の tick に開かなかった {}・閉じる tick の外れ {}・状態の取得不能 {} tick・異常の後にオーバーレイにした {}） / オーバーレイの一時停止 {} 回（{} tick） / マッチの外の Esc の押下 {}\n\
             観戦者の照合: 一致 {} 回 / ずれ {} 回{first}",
            g(S_PAUSE_OPENED),
            g(S_PAUSE_TICKS),
            g(S_PAUSE_RESUMED),
            g(S_PAUSE_REOPENED),
            g(S_PAUSE_QUITS),
            g(S_PAUSE_OPEN_SLIPS),
            g(S_PAUSE_CLOSE_SLIPS),
            g(S_PAUSE_UNREADABLE),
            g(S_PAUSE_FALLBACKS),
            g(S_OVERLAYS),
            g(S_OVERLAY_TICKS),
            g(S_ESC_OUTSIDE),
            g(S_CHECK_OK),
            g(S_CHECK_BAD),
        )
    }

    /// 早送りの行（観戦者の遅れの次の行）。閾値と上限も出す（どの値で走ったかが本ログだけで分かるように）。
    fn ff_text(&self) -> String {
        let g = |i: usize| self.get(i);
        let secs = |ms: u32| format!("{:.1} 秒", f64::from(ms) / 1_000.0);
        let first = match g(S_FF_FIRST_MS) {
            NO_VALUE => "まだ".to_string(),
            ms => format!("{ms} ms（{}）", secs(ms)),
        };
        let updates = g(S_FF_UPDATES);
        let enter = match g(S_FF_ENTER_BACKLOG) {
            NO_VALUE => "なし".to_string(),
            n => format!("{n} フレーム"),
        };
        format!(
            "観戦者の早送り: 設定（溜まりが {FF_ENTER_FRAMES} フレームを越えたら入り {FF_EXIT_FRAMES} 以下で戻る・1 表示の更新は最大 {updates} 回（ini の ff_max。1〜{FF_UPDATES_MAX}）・余分に使う時間の上限 {FF_BUDGET_US} µs） / 早送り用フックを通った表示 {}（0 のまま観戦者の tick が進むなら早送りの処理フローが無い） / 早送りした表示 {}（余分に回した更新 {}・1 表示の余分の時間の最大 {} µs） / 追いつくまで（入ってから戻るまで）最初の 1 回 {first}・入った回数 {}（戻った回数 {}・最初に入ったときの溜まり {enter}）・最長 {} ms・合計 {} ms / いま早送り中 {} / 時間の上限で打ち切った {} / SE・文字の数を整え不能で早送りをやめた {}",
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

/// 観戦者のセッションを組み込んだ（`script_win::arm_watch`）。読み取りのスレッドが`_watch_frames.csv`を開くかどうかのフラグ。
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

const ENDS: [&str; 11] = [
    "未終了",
    "ホストの終了（終わりの記録を受信）",
    "ホストの沈黙（溜めた記録を使い切った後）",
    "使えるコードの貼り付けなし",
    "記録の SELECT が範囲外",
    "押下の持ち越し超過（画面がホストと不一致）",
    "止める地点の SELECT 不揃い",
    "マッチの途中でホストの流れが切断",
    "自分で抜けた（詳細 0: Esc の 3 秒長押し／詳細 1: ポーズのタイトルへ戻る）",
    "手元に溜めた試合の記録が上限（約 1 時間分）に到達",
    "観戦者の受信処理が停止（内部の不具合）",
];

/// 終わり`Quit`の詳細: ゲームのポーズの「タイトルへ戻る」で終えた。
const QUIT_BY_PAUSE: u32 = 1;

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
        Some(WatchEnd::BufferFull) => (9, 0),
        Some(WatchEnd::Broken) => (10, 0),
    }
}

pub const SHOT_EVERY_TICKS: u32 = 600;

/// 観戦者の実行で撮影を頼むtickの判断（pure）。
/// 台本の段階は画面6で終わるので、台本の`PRE` / `POST`では画面7以降が撮れない。
/// 段階が変わったtick・選択の画面が変わったtick・マッチの中の`SHOT_EVERY_TICKS` tickごとに頼む。
/// 理由のビットは台本が8つ全部使っていて足せないので、`TRANSITION`・`TRANSITION | SETTLE`・`WAIT`を流用する。
/// 観戦者の撮影だと分かるのは`_shots.csv`のstep列（0xF000台）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ShotPacer {
    seen: Option<(Phase, Option<u8>)>,
    in_match: u32,
}

impl ShotPacer {
    /// 撮影の理由のビット（0なら頼まない）を返す。毎tick 1回、`on_input`の後に呼ぶ。最初のtickは基準を覚えるだけ。
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

#[cfg(test)]
mod recv_tests;

#[cfg(test)]
mod relay_tests;

#[cfg(test)]
mod pause_tests;
