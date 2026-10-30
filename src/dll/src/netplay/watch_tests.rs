//! 観戦者の側（`spectate_watch.rs`）の組み上がりの検査（pure。仮の時計と仮のネットワーク）。
//! 選択の画面を持つ偽のゲーム2つ（A = ホスト1P、B = ゲスト2P。`menu_tests::MenuGame`）を、本物の入力の地点の判断（`NetPlay`）で動かす。
//! ホストAの観戦のリングバッファ（`SpectateRing`）から記録を取り出し、coreの`SpectateHost`に積む。
//! それを仮のネットワークで観戦者（`WatchPlay` ＋ 3つ目の`MenuGame` W）へ送る。
//! Wの偽のゲームは、タイトルから7の画面へ入るところだけ自分の台本で進み、あとは記録の列だけで動く。
//!
//! 合格は「Wが使った試合のワード・入った押下・書いたSELECTが、AとBのゲームが使った値・決定した値と一致する」こと。
//! 期待はWの作りと別の所から取る。
//! マッチのワードはA・Bのゲームが使った列（`MenuGame::played`）。押下はAの`step_log`。ゲームが決定した値はA・Bのゲーム自身の状態。
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::input_model::keys::{ESC, SHIFT, X, Z};
use higanr_core::spectate::{HostConfig, HostCounts, SpectateHost, WatcherConfig};
use higanr_core::wire::{self, ByeReason, MAX_DATAGRAM, Message, Record, Seat};

use super::fake::{FRAME_US, FakeNet};
use super::menu_tests::{Act, Driver, MenuGame, Press, act, addr, link_cfg, link_cfg_named, plans};
use super::{Action, CheckSource, Config, FrameTiming, Game, Net, NetPlay, Phase, WAIT_BUDGET_US};
use crate::net_ui::{HudNames, WatchHudFacts};
use crate::spectate_ring::SpectateRing;
use crate::spectate_testkit::{Folded, fold};
use crate::spectate_watch::{
    FF_BUDGET_US, FF_ENTER_FRAMES, FF_EXIT_FRAMES, FfHost, HOST_SILENT_END_MS, WATCH_MARGIN_FRAMES,
    WatchConfig, WatchEnd, WatchPlay, WatchSlot, run_display,
};
use higanr_core::spectate_pace::FF_UPDATES_DEFAULT;

impl Driver for WatchPlay {
    fn on_input<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        WatchPlay::on_input(self, game, net)
    }
    fn on_match_start<G: Game>(&mut self, game: &mut G) {
        WatchPlay::on_match_start(self, game)
    }
    fn phase(&self) -> Phase {
        WatchPlay::phase(self)
    }
    fn last_timing(&self) -> FrameTiming {
        WatchPlay::last_timing(self)
    }
}

/// 検査の対戦する側の名前（合成。Aはホスト・1P、Bはゲスト・2P）。
const NAME_A: &str = "host-a";
const NAME_B: &str = "guest-b";

thread_local! {
    /// 真の間に作った`Rig`は、ホストのリングバッファから取り出した`Name`の記録を観戦の流れへ積まない（名前を受けていない観戦者の形を作る）。検査のスレッドごと。
    static DROP_NAMES: Cell<bool> = const { Cell::new(false) };
    /// `Some`の間に作った`Rig`のホストは、遅れたJOINの遡りの上限をこの値にする（0 = 選択のsegまで遡れない形）。
    static HOST_REWIND: Cell<Option<u32>> = const { Cell::new(None) };
    /// `Some((n, k))`の間に作った`Rig`は、観戦者がマッチのフレームをn使ったところで、観戦者のゲームのスレッドをkフレーム止める。
    /// 受信スレッドの代わりは回り続ける（ウィンドウのドラッグなどの形）。
    static WATCHER_FREEZE: Cell<Option<(u64, u32)>> = const { Cell::new(None) };
    /// `Some`の間に作った対戦する2人のCHECK（と観戦の照合）の作り方。`None`は入力のダイジェスト。
    static HOST_CHECK: Cell<Option<CheckSource>> = const { Cell::new(None) };
}

/// 観戦用のコードのsessionと秘密（対戦のsession 9・`DEFAULT_SECRET`とは別の値）。
pub(super) const SPEC_SESSION: u32 = 0x5EC7_0033;
pub(super) const SPEC_SECRET: Secret = Secret(*b"watch-tests-0001");
/// 観戦のホストの使い捨ての鍵のシードと観戦者のシード（検査では固定値にする。本物はdllがOSの乱数で生成する）。
const SPEC_HOST_SEED: higanr_core::seal::KeySeed = higanr_core::seal::KeySeed([0x48; 32]);
const SPEC_WATCHER_SEED: higanr_core::seal::KeySeed = higanr_core::seal::KeySeed([0x57; 32]);

/// 観戦のホストの公開鍵（`SPEC_HOST_SEED`の公開鍵）。観戦者の設定に書き込む。本物は観戦用のコードから得る。
fn spec_host_public() -> [u8; 32] {
    higanr_core::seal::Ephemeral::from_seed(&SPEC_HOST_SEED).public()
}

/// 観戦者（`SPEC_WATCHER_SEED`）が送ったパケットを開く関数（観戦者の送る向き）。検査が観戦者のACK・BYEを外から読むため。
fn watcher_sent_opener() -> higanr_core::seal::Opener {
    higanr_core::seal::Ephemeral::from_seed(&SPEC_WATCHER_SEED)
        .dir_keys(
            &spec_host_public(),
            false,
            SPEC_SESSION,
            &AuthKey::derive(&SPEC_SECRET, SPEC_SESSION),
        )
        .expect("鍵が作れる")
        .swapped()
        .opener()
}

/// 観戦者の偽のゲームの台本。6で待って7へ入る。
/// ホストの最初の押下は難易度のキャンセルなので6へ戻り、もう一度7へ入る。
/// 以後の画面（8・16）へは記録の押下で進む。
fn watcher_plan() -> Vec<Act> {
    vec![act(0, 6, 30, Z, 1), act(0, 6, 15, Z, 1)]
}

pub(super) fn watcher_game(match_frames: u32, max_matches: u32, rematches: u32) -> MenuGame {
    let mut w = MenuGame::new(
        0,
        watcher_plan(),
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
    );
    w.match_frames = match_frames;
    w.max_matches = max_matches;
    w.rematches = rematches;
    w
}

fn spec_addrs() -> (std::net::SocketAddr, std::net::SocketAddr) {
    (addr(10), addr(11))
}

/// 観戦の用意: ホスト（`SpectateHost`）・観戦者との仮のネットワーク・観戦者。`hook`を1フレームに1回呼ぶ。
struct Rig {
    host: SpectateHost,
    hnet: FakeNet,
    wnet: FakeNet,
    sclock: Rc<Cell<u64>>,
    ring: Arc<SpectateRing>,
    key: AuthKey,
    recs: Vec<Record>,
    sent: Vec<Message>,
    /// `sent`（観戦者が送った各パケット）を観戦のホスト（`SpectateHost`）が受けた時点の`wticks`（観戦者のtickの数。BYEが観戦者の何tick目に出たかを見る）。
    sent_at: Vec<u32>,
    /// 観戦者が終えた後のtickの数と、そのうち`Action::Local`でなかった数（終えた後は自分の入力だけ）。
    post_end_ticks: u32,
    post_end_not_local: u32,
    wp: WatchPlay,
    wg: MenuGame,
    started: bool,
    wticks: u32,
    /// 動かし始めた時点の観戦者の`local_input`の回数（接続した後は増えないことが、自分の入力を読まない証拠になる）。
    local_at_join: Option<u32>,
    /// 観戦者のtickごとの「ネットワークが`Local`を返したとき、ゲームが選択の画面か試合の中だった」回数（自分の入力が届く経路）。
    leaks: u32,
    /// 観戦者の`on_input`の確保の回数（接続した後のtick）。
    allocs_joined: u64,
    paused: bool,
    /// 終わりの記録（`End`）を観戦の経路へ積むのをこのtickの数だけ遅らせる（`Seg`は先に積む）。0なら遅らせない。
    end_delay_ticks: u32,
    /// 遅らせている`End`と、残りのtick。
    late_end: Option<(Record, u32)>,
    drop_names: bool,
    /// 最後のsegを出た後の`Menu`・`Step`を積まない（peer_simのホストのように、出た直後に閉じる並び）。
    bare_tail: bool,
    left_last_seg: bool,
    /// 観戦者を本物の走査のラッパーと同じ呼ばれ方（早送りつき）で回す。既定はfalse（今までの検査は1表示 = 1更新のまま）。
    ff_on: bool,
    /// 観戦者を動かした表示（`hook`の呼び出し）の数と、最初に接続した表示・最初に追いつき終えた表示の番号。
    displays: u32,
    joined_display: Option<u32>,
    caught_up_display: Option<u32>,
    /// 走査1回ごとのフラグ（false = 表示の最初の走査、true = 余分な走査）と、余分な走査の前にSE・文字の数を整えた回数。
    ff_scans: Vec<bool>,
    ff_drops: u32,
    /// 偽のゲームの走査1回の所要時間（µs）。表示の中の時計（`ff_skew`）に走査のたびに足す（時間の上限の検査）。
    scan_cost_us: u64,
    ff_skew: u64,
    /// 余分な走査のうちマッチのフレームを使わなかったもの（0のはず）。
    extra_not_played: u32,
    /// 最初に追いつき終えた後に使ったマッチのフレームの余裕（記録が届いてから使うまで。µs）。
    after_margins: Vec<u32>,
    hud: Vec<Option<WatchHudFacts>>,
    /// tickごとの（`awaiting_select`、それまでに使ったマッチのフレームの数）。
    awaiting: Vec<(bool, u64)>,
    /// 接続した後の早送りの呼び出し（`ff_begin`・`ff_more`・`ff_end`）の確保の回数。
    display_allocs: u64,
    /// 観戦者のゲームのスレッドを止める予定（`WATCHER_FREEZE`）・残りのフレーム・止めた時と動き出した時のホストの数。
    freeze_at: Option<(u64, u32)>,
    freeze_left: u32,
    freeze_from: Option<HostCounts>,
    freeze_to: Option<HostCounts>,
}

impl Rig {
    fn new(w: MenuGame, delay_us: u64) -> Rig {
        let (a, b) = spec_addrs();
        let (hnet, wnet, sclock) = FakeNet::pair(a, b, delay_us);
        let slot = Arc::new(WatchSlot::new());
        slot.offer(WatcherConfig {
            session_id: SPEC_SESSION,
            secret: SPEC_SECRET,
            nonce: 0xAB,
            name: Name::new("w"),
            host: Candidates::from_addrs(&[a]).expect("loopback"),
            host_public: spec_host_public(),
            key_seed: SPEC_WATCHER_SEED,
        });
        let wp = WatchPlay::new(
            slot,
            WatchConfig {
                wait_budget_us: WAIT_BUDGET_US,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: true,
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
            },
        );
        Rig {
            host: SpectateHost::with_capacity(
                HostConfig {
                    session_id: SPEC_SESSION,
                    secret: SPEC_SECRET,
                    key_seed: SPEC_HOST_SEED,
                },
                1 << 15,
            ),
            hnet,
            wnet,
            sclock,
            ring: Arc::new(SpectateRing::with_len(1 << 15)),
            key: AuthKey::derive(&SPEC_SECRET, SPEC_SESSION),
            recs: Vec::new(),
            sent: Vec::new(),
            sent_at: Vec::new(),
            post_end_ticks: 0,
            post_end_not_local: 0,
            wp,
            wg: w,
            started: false,
            wticks: 0,
            local_at_join: None,
            leaks: 0,
            allocs_joined: 0,
            paused: false,
            end_delay_ticks: 0,
            late_end: None,
            drop_names: false,
            bare_tail: false,
            left_last_seg: false,
            ff_on: false,
            displays: 0,
            joined_display: None,
            caught_up_display: None,
            ff_scans: Vec::new(),
            ff_drops: 0,
            scan_cost_us: 0,
            ff_skew: 0,
            extra_not_played: 0,
            after_margins: Vec::new(),
            hud: Vec::new(),
            awaiting: Vec::new(),
            display_allocs: 0,
            freeze_at: WATCHER_FREEZE.with(Cell::get),
            freeze_left: 0,
            freeze_from: None,
            freeze_to: None,
        }
    }

    /// 1フレームに1回呼ぶ。ホストのリングバッファを空にして観戦者へ送り、観戦者から届いたパケットをhostに渡し、観戦者を1 tick動かす（`start`が真になってから）。
    fn hook(&mut self, now_us: u64, start: bool) {
        if self.sclock.get() < now_us {
            self.sclock.set(now_us);
        }
        let now_ms = (self.sclock.get() / 1_000) as u32;
        if !self.paused {
            if let Some((rec, left)) = self.late_end {
                if left == 0 {
                    let _ = self.host.push(rec);
                    self.late_end = None;
                } else {
                    self.late_end = Some((rec, left - 1));
                }
            }
            while let Some(p) = self.ring.pop() {
                if p.after_gap {
                    let _ = self.host.push(Record::End(ByeReason::Cancelled));
                }
                if self.bare_tail {
                    // 最後のsegを出た後の選択の画面の記録は積まない（`Seg(次)`の直後が`End`の並び）。
                    if matches!(p.rec, Record::Seg { seg } if seg > 0) {
                        self.left_last_seg = true;
                    } else if self.left_last_seg
                        && matches!(p.rec, Record::Menu { .. } | Record::Step(_))
                    {
                        continue;
                    }
                }
                if self.drop_names && matches!(p.rec, Record::Name { .. }) {
                    continue;
                }
                self.recs.push(p.rec);
                if self.end_delay_ticks > 0 && matches!(p.rec, Record::End(_)) {
                    // `Seg`は先に積んだ。`End`は遅れて積む（記録の並びは変えない。届く時刻だけ遅らせる）。
                    self.late_end = Some((p.rec, self.end_delay_ticks));
                } else {
                    let _ = self.host.push(p.rec);
                }
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            while let Some((from, n, _)) = self.hnet.recv(&mut buf) {
                let bytes = buf.get(..n).unwrap_or(&[]);
                if let Ok(pkt) = wire::decode(&self.key, Some(&watcher_sent_opener()), bytes) {
                    self.sent.push(pkt.msg);
                    self.sent_at.push(self.wticks);
                }
                self.host.on_datagram(from, bytes, now_ms, &mut self.hnet);
            }
            self.host.poll(now_ms, &mut self.hnet);
        }
        // 観戦者の受信スレッドの代わり（ゲームのスレッドのtickと別に、フレームごとに回る）。
        self.wnet.run_watch();
        if start && !self.started {
            self.started = true;
            // 受け取りを受信スレッド（の代わり）へ渡す1 tick（接続する前）。タイトルの偽のゲームで空回りさせる。
            let mut scratch = MenuGame::new(0, Vec::new(), &[0]);
            let _ = self.wp.on_input(&mut scratch, &mut self.wnet);
        }
        if self.started {
            if let Some((at, frames)) = self.freeze_at
                && self.wg.played.len() as u64 >= at
            {
                self.freeze_at = None;
                self.freeze_left = frames;
                self.freeze_from = Some(self.host.counts());
            }
            if self.freeze_left > 0 {
                self.freeze_left -= 1;
                if self.freeze_left == 0 {
                    self.freeze_to = Some(self.host.counts());
                }
                return;
            }
            self.displays += 1;
            if self.ff_on {
                let _ = run_display(self);
            } else {
                let _ = self.tick_once();
            }
            if self.joined_display.is_none() && self.wp.is_joined() {
                self.joined_display = Some(self.displays);
            }
            if self.caught_up_display.is_none() && self.wp.stats().ff.finished > 0 {
                self.caught_up_display = Some(self.displays);
            }
        }
    }

    fn tick_once(&mut self) -> Action {
        let joined_before = self.wp.is_joined();
        let calls_before = self.wg.local_input_calls;
        let alloc_before = self.wg.on_input_allocs;
        let in_selection = self.wg.cur == 1 && matches!(self.wg.screen, 7 | 8 | 16);
        let in_match_scene = self.wg.cur == 2;
        let stop_point = self.wg.cur == 1 && self.wg.next == 2;
        let ended_before = self.wp.end().is_some();
        let a = self.wg.tick(&mut self.wp, &mut self.wnet);
        self.hud.push(self.wp.hud_facts());
        self.awaiting
            .push((self.wp.awaiting_select(), self.wp.stats().played));
        self.wticks += 1;
        if ended_before {
            self.post_end_ticks += 1;
            if a != Action::Local {
                self.post_end_not_local += 1;
            }
        }
        if joined_before {
            if self.local_at_join.is_none() {
                self.local_at_join = Some(calls_before);
            }
            // 自分の入力が届く経路は`Action::Local`（いつものpoll_all）だけ。選択の画面とマッチの中では返さない。
            // 終わりを受けたtick（そのtickから自分の入力へ戻る）は数えない。
            if a == Action::Local
                && !ended_before
                && self.wp.end().is_none()
                && (in_selection || in_match_scene || stop_point)
            {
                self.leaks += 1;
            }
            self.allocs_joined += self.wg.on_input_allocs - alloc_before;
        }
        a
    }

    fn finished(&self) -> bool {
        self.started && self.wp.phase() == Phase::Ended
    }

    fn folded(&self) -> Folded {
        fold(&self.recs)
    }
}

/// 観戦者の偽のゲームを、本物の走査のラッパーと同じ呼ばれ方（`spectate_watch::run_display`）で回すための関数。
/// 走査1回 = 偽のゲームの更新1回（`tick_once`）。
/// SE・文字の数を整える関数は呼ばれた回数だけ数える（偽のゲームはSEを鳴らさない）。
impl FfHost for Rig {
    fn scan(&mut self, extra: bool) -> i32 {
        self.ff_scans.push(extra);
        if !extra {
            self.ff_skew = 0;
        }
        let a = self.tick_once();
        self.ff_skew += self.scan_cost_us;
        if extra && !matches!(a, Action::Played(_)) {
            self.extra_not_played += 1;
        }
        if matches!(a, Action::Played(_)) && self.caught_up_display.is_some() {
            self.after_margins.extend(self.wp.last_timing().margin_us);
        }
        // 本物の走査は、描画だけのフレーム（入力の地点の出口）では1を返す。普通の更新ではノード数（2以上）を返す。
        match a {
            Action::RenderOnly => 1,
            _ => 2,
        }
    }

    fn with_wp<R>(&mut self, f: impl FnOnce(&mut WatchPlay, u64) -> R) -> Option<R> {
        let joined = self.wp.is_joined();
        let before = crate::test_alloc::count();
        let r = f(&mut self.wp, self.sclock.get() + self.ff_skew);
        if joined {
            self.display_allocs += crate::test_alloc::count() - before;
        }
        Some(r)
    }

    fn begin(&mut self) {}

    fn drop_outputs(&mut self) -> bool {
        self.ff_drops += 1;
        true
    }

    fn boundary(&mut self) -> bool {
        // 偽のゲームは、マッチの中は場面2・次の要求0。
        self.wg.cur != 2 || self.wg.next != 0
    }
}

struct Out {
    a: MenuGame,
    b: MenuGame,
    npa: NetPlay,
    npb: NetPlay,
    rig: Rig,
}

/// A・Bを回し、Wを付ける。`start_when`が真になったtick（Aを見て）にWを開始する。
/// AとBが全部のマッチを終えてメニューへ戻ったら、AがBYE(終了)で閉じる。Wが終わる（か6000 tick）まで回す。
/// `with_watcher`が偽ならWは開始せず、リングバッファも付けない。
fn run_rig(
    a: MenuGame,
    b: MenuGame,
    w: MenuGame,
    with_watcher: bool,
    start_when: &dyn Fn(&MenuGame) -> bool,
    spec_delay_us: u64,
    ticks: u32,
) -> Out {
    run_rig_with(
        a,
        b,
        w,
        with_watcher,
        start_when,
        &|_| false,
        spec_delay_us,
        ticks,
    )
}

/// `run_rig`に、ホストの観戦者への送信を止めるtickを足したもの。
/// 止めるのは`pause_when`がAを見て真の間で、ホストの受信スレッドが止まった形・ネットワークが詰まった形にあたる。
/// AとBの対戦は止まらない。
#[allow(clippy::too_many_arguments)] // 検査のツール。引数は全部別の意味
fn run_rig_with(
    a: MenuGame,
    b: MenuGame,
    w: MenuGame,
    with_watcher: bool,
    start_when: &dyn Fn(&MenuGame) -> bool,
    pause_when: &dyn Fn(&MenuGame) -> bool,
    spec_delay_us: u64,
    ticks: u32,
) -> Out {
    run_rig_full(
        a,
        b,
        w,
        with_watcher,
        start_when,
        pause_when,
        (false, 0),
        spec_delay_us,
        ticks,
    )
}

/// `run_rig_with`に次のオプションを足したもの。
/// - 最後のsegを出た後の選択の記録を積まない（`bare_tail`）。
/// - 終わりの記録だけを`end_delay_ticks` tick遅らせて観戦の経路へ積む。
#[allow(clippy::too_many_arguments)] // 検査のツール。引数は全部別の意味
fn run_rig_full(
    a: MenuGame,
    b: MenuGame,
    w: MenuGame,
    with_watcher: bool,
    start_when: &dyn Fn(&MenuGame) -> bool,
    pause_when: &dyn Fn(&MenuGame) -> bool,
    (bare_tail, end_delay_ticks): (bool, u32),
    spec_delay_us: u64,
    ticks: u32,
) -> Out {
    run_rig_inner(
        a,
        b,
        w,
        with_watcher,
        start_when,
        pause_when,
        (bare_tail, end_delay_ticks, None),
        spec_delay_us,
        ticks,
    )
}

/// `run_rig_full`に、観戦者を本物の走査のラッパーと同じ呼ばれ方（早送りつき。`Rig::ff_on`）で回すオプションを足したもの。
#[allow(clippy::too_many_arguments)] // 検査のツール。引数は全部別の意味
fn run_rig_inner(
    mut a: MenuGame,
    mut b: MenuGame,
    w: MenuGame,
    with_watcher: bool,
    start_when: &dyn Fn(&MenuGame) -> bool,
    pause_when: &dyn Fn(&MenuGame) -> bool,
    (bare_tail, end_delay_ticks, ff): (bool, u32, Option<u64>),
    spec_delay_us: u64,
    ticks: u32,
) -> Out {
    let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), 8_000);
    let cfg = Config {
        check: HOST_CHECK
            .with(Cell::get)
            .unwrap_or(CheckSource::InputDigest),
        wait_budget_us: WAIT_BUDGET_US,
        require_net_selection: true,
        ..Config::default()
    };
    // 2人の名前は別々にする（観戦の`Name`の記録がサイドを取り違えても気づけるように）。
    let mut npa = NetPlay::new(link_cfg_named(2, 0xF000, NAME_A), cfg, &mut na);
    let mut rig = Rig::new(w, spec_delay_us);
    rig.drop_names = DROP_NAMES.with(Cell::get);
    if let Some(n) = HOST_REWIND.with(Cell::get) {
        rig.host.set_rewind_max(n);
    }
    rig.end_delay_ticks = end_delay_ticks;
    rig.bare_tail = bare_tail;
    rig.ff_on = ff.is_some();
    rig.scan_cost_us = ff.unwrap_or(0);
    if with_watcher {
        npa.set_spectate(Arc::clone(&rig.ring));
    }
    let mut npb = NetPlay::new(link_cfg_named(1, 1, NAME_B), cfg, &mut nb);
    let mut slot = clock.get();
    let mut closed = false;
    let mut after_close = 0u32;
    let mut after_finish = 0u32;
    for _ in 0..ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        a.tick(&mut npa, &mut na);
        clock.set(clock.get().max(slot + FRAME_US / 2));
        b.tick(&mut npb, &mut nb);
        if with_watcher {
            let start = start_when(&a);
            rig.paused = pause_when(&a);
            rig.hook(clock.get(), start);
        }
        slot += FRAME_US;
        let back = |g: &MenuGame| {
            g.matches >= g.max_matches
                && g.cur == 1
                && g.screen == 8
                && !g.pending_auto8
                && g.plan_done()
        };
        if !closed && back(&a) && back(&b) {
            npa.close(ByeReason::Finished);
            closed = true;
        }
        if closed {
            after_close += 1;
            if rig.finished() {
                after_finish += 1;
            }
            if !with_watcher || after_finish > 5 || after_close > 6_000 {
                break;
            }
        }
    }
    Out {
        a,
        b,
        npa,
        npb,
        rig,
    }
}

fn games() -> (MenuGame, MenuGame) {
    let (pa, pb) = plans();
    let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    a.max_matches = 2;
    b.max_matches = 2;
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    (a, b)
}

fn from_start(_: &MenuGame) -> bool {
    true
}

/// 観戦者が使ったマッチのワード・入った押下・書いたSELECTが、AとBのゲームが使った値・決定した値と一致するかどうか。
/// 初めから観戦している観戦者は、選択の画面のワードを1つも捨てない（`menu_skipped`が0。捨てるのは遅れて入って追いつくときだけ）。
fn assert_follows(o: &Out, tag: &str) {
    assert_eq!(
        o.rig.wp.stats().menu_skipped,
        0,
        "{tag}: 初めから観戦しているのに選択の語を捨てた"
    );
    assert_follows_with(o, tag, false);
}

/// `assert_follows`の、遅れて入った観戦者の形。
/// 選択の画面のワードは、追いつくために捨てた分だけホストより少ない（0でないワードの列がホストの列の部分列）。
/// 押下・マッチのワード・決定した値は全行一致のまま。
/// 捨てていること（追いつきの経路を通ったこと）も検証する。
fn assert_follows_late(o: &Out, tag: &str) {
    assert!(
        o.rig.wp.stats().menu_skipped > 0,
        "{tag}: 選択の語を捨てていない（遅れて入った形になっていない）"
    );
    assert_follows_with(o, tag, true);
}

fn assert_follows_with(o: &Out, tag: &str, late: bool) {
    let w = &o.rig.wg;
    let f = o.rig.folded();
    assert!(!o.a.played.is_empty(), "{tag}: A が試合をしていない");
    assert_eq!(w.played, o.a.played, "{tag}: W の列と A の列");
    assert_eq!(w.played, o.b.played, "{tag}: W の列と B の列");
    assert_eq!(f.frames, o.a.played, "{tag}: ホストの記録の列と A の列");
    // 照合: ホストが積んだ記録を全部、観戦者の値と比べて合っている（ホストのダイジェストとWの列が同じなら合う）。
    let s = o.rig.wp.stats();
    assert!(!f.checks.is_empty(), "{tag}: ホストが照合を積んでいない");
    assert_eq!(
        (s.checks_matched as usize, s.checks_mismatched),
        (f.checks.len(), 0),
        "{tag}: 照合（一致, ずれ）: {:?}",
        s.first_mismatch
    );
    // 選択の画面でfeedした3本（1P・2P・合成）のうち0でないものの列は、ホストAがfeedした列と全部同じになる。
    // サイドの入れ違いや、押下をワードに載せ忘れたことがここで分かる。
    // 0のtickは待ちの長さが観戦者とAで違うので除く。
    let nonzero = |v: &[[u16; 3]]| {
        v.iter()
            .copied()
            .filter(|f| *f != [0; 3])
            .collect::<Vec<_>>()
    };
    // キャラ選択では、Aは相手（B・2P）のワードの方向と「ランダム」を取り除いてfeedし、カーソルは位置で書く。
    // 観戦者は両サイドをワードのままfeedする。
    // そのため観戦者の列の画面8の2Pのワードに同じ取り除き方を当ててから比べる。
    let as_host: Vec<[u16; 3]> = w
        .fed_menu
        .iter()
        .zip(&w.fed_menu_screen)
        .map(|(f, &s)| {
            let p2 = crate::net_select::peer_word(s, f[1]);
            if s == 8 { [f[0], p2, f[0] | p2] } else { *f }
        })
        .collect();
    let (fw, fa) = (nonzero(&as_host), nonzero(&o.a.fed_menu));
    if late {
        let mut rest = fa.iter();
        assert!(
            fw.iter().all(|x| rest.any(|y| y == x)),
            "{tag}: 選択の画面で feed した語がホストの列の部分列でない\n観戦者 {fw:?}\nホスト {fa:?}"
        );
        assert!(fw.len() >= 8, "{tag}: 選択の語が少ない {}", fw.len());
    } else {
        assert!(fw.len() >= 15, "{tag}: 選択の語が少ない {}", fw.len());
        assert_eq!(fw, fa, "{tag}: 選択の画面で feed した語（0 でないもの）");
    }
    assert_eq!(w.fed_played, o.a.fed_played, "{tag}: 試合で feed した 3 本");
    assert!(
        w.fed_played
            .iter()
            .any(|f| f[0] != 0 && f[1] != 0 && f[0] != f[1]),
        "{tag}: 1P と 2P の語が違う標本が無い（席の入れ違いが見えない）"
    );
    assert!(
        !w.charges.is_empty()
            && w.charges.iter().all(|c| *c == (0, 1))
            && w.charges.len() == o.rig.wp.stats().matches_started as usize,
        "{tag}: チャージ型 {:?}",
        w.charges
    );
    let a_steps: Vec<_> = o.npa.step_log().iter().map(|s| s.step).collect();
    let w_steps: Vec<_> = o.rig.wp.step_log().iter().map(|s| s.step).collect();
    assert_eq!(w_steps, a_steps, "{tag}: 入った押下の列");
    assert!(a_steps.len() >= 8, "{tag}: 押下が少ない {}", a_steps.len());
    assert!(
        o.rig.wp.step_log().iter().all(|s| !s.own),
        "{tag}: 観戦者の押下は自分の押下でない"
    );
    assert_eq!(w.decided_chars, o.a.decided_chars, "{tag}: 決めたキャラ");
    assert_eq!(
        w.decided_chars, o.b.decided_chars,
        "{tag}: 決めたキャラ（B）"
    );
    assert_eq!(w.decided_maps, o.a.decided_maps, "{tag}: 決めたマップ");
    assert_eq!(
        w.saved_diff, o.a.saved_diff,
        "{tag}: 難易度のカーソルの控え"
    );
    assert!(
        w.decided_chars.len() >= 2 && w.decided_maps.contains(&-2),
        "{tag}: 選択が 2 回通っていない"
    );
    assert_eq!(
        (w.chara, w.color, w.difficulty, w.map),
        (o.a.chara, o.a.color, o.a.difficulty, o.a.map),
        "{tag}: 試合に入った値"
    );
    let pairs: Vec<_> = f.selects.chunks(2).collect();
    assert_eq!(w.watched.len(), o.a.agreed.len(), "{tag}: 止める地点の数");
    assert_eq!(pairs.len(), w.watched.len(), "{tag}");
    for ((pair, got), agreed) in pairs.iter().zip(&w.watched).zip(&o.a.agreed) {
        assert_eq!((pair[0].0, pair[1].0), (Seat::P1, Seat::P2), "{tag}");
        assert_eq!([pair[0].1, pair[1].1], *got, "{tag}: 書いた SELECT と記録");
        assert_eq!(
            (got[1].chara, got[1].color, got[1].life),
            (agreed.peer.chara, agreed.peer.color, agreed.peer.life),
            "{tag}: 2P の値"
        );
        for s in got {
            assert_eq!(
                (s.map, s.difficulty, s.rng_seed),
                (agreed.map, agreed.difficulty, agreed.rng_seed),
                "{tag}: 共通の欄がホスト A の値でない"
            );
        }
        assert_ne!(got[0].chara, 0xFF, "{tag}");
    }
    assert_eq!(
        o.rig.wp.written(),
        w.watched.last().copied(),
        "{tag}: 最後に書いた SELECT"
    );
    // 自分（観戦者）の入力が届かない: 接続した後はローカルの入力を1回も読まず、選択の画面とマッチの中で`Local`を返さない。
    assert_eq!(
        Some(w.local_input_calls),
        o.rig.local_at_join,
        "{tag}: 繋がった後にローカルの入力を読んだ"
    );
    assert_eq!(
        o.rig.leaks, 0,
        "{tag}: 選択の画面か試合の中で Local を返した"
    );
    assert_eq!(w.ignored_presses, 0, "{tag}: ゲームが読まなかった押下");
    assert_eq!(o.rig.allocs_joined, 0, "{tag}: on_input が確保した");
    let (mut joins, mut acks, mut byes) = (0, 0, 0);
    for m in &o.rig.sent {
        match m {
            Message::SpectateJoin(_) => joins += 1,
            Message::SpectateAck { .. } => acks += 1,
            Message::Bye(_) => byes += 1,
            other => panic!("{tag}: 観戦者が JOIN・ACK・BYE 以外を送った: {other:?}"),
        }
    }
    assert!(
        joins >= 1 && acks > 10 && byes >= 1,
        "{tag}: {joins} {acks} {byes}"
    );
    assert_eq!(o.rig.recs.first(), Some(&Record::Seg { seg: 0 }), "{tag}");
    assert_eq!(f.ends, vec![ByeReason::Finished], "{tag}");
    assert_eq!(
        o.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{tag}"
    );
}

#[test]
fn a_watcher_walks_the_hosts_selection_and_plays_the_hosts_matches() {
    for spec_delay_ms in [5u64, 40] {
        let (a, b) = games();
        let w = watcher_game(a.match_frames, 2, 0);
        let o = run_rig(a, b, w, true, &from_start, spec_delay_ms * 1_000, 20_000);
        let tag = format!("観戦の片道 {spec_delay_ms} ms");
        assert_follows(&o, &tag);
        let s = o.rig.wp.stats();
        assert_eq!(s.frame_gaps, 0, "{tag}");
        assert_eq!(s.jumps, 0, "{tag}");
        assert_eq!(s.matches_started, 2, "{tag}");
        assert!(s.menu_applied >= 8, "{tag}: {s:?}");
        // 余裕: 1マッチの始まりで記録が溜まるのを待つ（`WATCH_MARGIN_FRAMES`）。使ったフレームは全部、届いてから33 ms以上の余裕を持って使った。
        assert!(
            (1..=60).contains(&s.start_hold_ticks),
            "{tag}: 余裕待ち {}",
            s.start_hold_ticks
        );
        assert_eq!(
            s.margin_hist[..3].iter().sum::<u64>(),
            0,
            "{tag}: 余裕が 2 フレーム未満で使ったフレームがある {:?}",
            s.margin_hist
        );
        eprintln!(
            "観戦者（片道 {spec_delay_ms} ms）: 使った {} フレーム、描画だけ {}（最長 {}）、余裕待ち {}、余裕の分布 {:?}、最大 {} µs、押下 {}、持ち越し {}",
            s.played,
            s.stalls,
            s.max_stall_run,
            s.start_hold_ticks,
            s.margin_hist,
            s.margin_max_us,
            s.menu_applied,
            s.menu_held
        );
    }
}

#[test]
fn a_watcher_with_a_longer_entry_animation_carries_the_steps_over() {
    let (a, b) = games();
    let mut w = watcher_game(a.match_frames, 2, 0);
    w.anim_len = 10;
    let o = run_rig(a, b, w, true, &from_start, 5_000, 20_000);
    assert_follows(&o, "演出の長い観戦者");
    let s = o.rig.wp.stats();
    assert!(
        s.menu_held > 0,
        "持ち越しが起きていない（演出の期間を踏めていない）: {s:?}"
    );
    assert_eq!(o.rig.wg.ignored_presses, 0);
}

#[test]
fn words_before_the_screens_align_are_muted_and_after_it_they_move_the_cursor() {
    use higanr_core::input_model::keys::{DOWN, LEFT, RIGHT};
    use higanr_core::wire::{Step, StepKind};
    let w = MenuGame::new(0, vec![act(0, 6, 12, Z, 1)], &[0, 1, 2, 3]);
    let mut rig = Rig::new(w, 1_000);
    let menu = |p1, p2| Record::Menu { p1, p2 };
    for r in [
        Record::Seg { seg: 0 },
        menu(RIGHT, LEFT | SHIFT | Z),
        menu(RIGHT, LEFT | SHIFT | Z),
        menu(RIGHT, LEFT | SHIFT | Z),
        Record::Step(Step {
            step: 0,
            kind: StepKind::Align,
            value: 2,
            screen: 7,
        }),
        menu(0, 0),
        menu(DOWN, 0),
        menu(0, 0),
    ] {
        rig.host.push(r).unwrap();
    }
    let mut now = 1_000_000u64;
    for _ in 0..200 {
        rig.hook(now, true);
        now += FRAME_US;
    }
    let nonzero: Vec<[u16; 3]> = rig
        .wg
        .fed_menu
        .iter()
        .copied()
        .filter(|f| *f != [0; 3])
        .collect();
    assert_eq!(nonzero, vec![[DOWN, 0, DOWN]], "合わせの前の語が入った");
    assert_eq!(rig.wg.cursor, 3, "合わせの値 2 から下へ 1 つ");
    assert_eq!(rig.wp.stats().menu_applied, 1);
    assert_eq!(rig.wp.stats().menu_align_miss, 0);
}

#[test]
fn selection_words_with_the_screenshot_bit_never_reach_the_game() {
    use higanr_core::input_model::MATCH_SEND_MASK;
    use higanr_core::input_model::keys::{DOWN, LEFT, RIGHT};
    use higanr_core::wire::{Step, StepKind};
    const SHOT: u16 = 0x0800;
    let w = MenuGame::new(0, vec![act(0, 6, 12, Z, 1)], &[0, 1, 2, 3]);
    let mut rig = Rig::new(w, 1_000);
    let menu = |p1, p2| Record::Menu {
        p1: p1 | SHOT,
        p2: p2 | SHOT,
    };
    for r in [
        Record::Seg { seg: 0 },
        menu(RIGHT, LEFT | SHIFT | Z),
        menu(RIGHT, LEFT | SHIFT | Z),
        Record::Step(Step {
            step: 0,
            kind: StepKind::Align,
            value: 2,
            screen: 7,
        }),
        menu(0, 0),
        menu(DOWN, 0),
        menu(0, 0),
    ] {
        rig.host.push(r).unwrap();
    }
    let mut now = 1_000_000u64;
    for _ in 0..200 {
        rig.hook(now, true);
        now += FRAME_US;
    }
    assert!(
        rig.wg
            .fed_menu
            .iter()
            .flatten()
            .all(|w| w & !MATCH_SEND_MASK == 0),
        "選択の feed に MATCH_SEND_MASK の外のビットが混ざった: {:?}",
        rig.wg.fed_menu
    );
    let nonzero: Vec<[u16; 3]> = rig
        .wg
        .fed_menu
        .iter()
        .copied()
        .filter(|f| *f != [0; 3])
        .collect();
    assert_eq!(
        nonzero,
        vec![[DOWN, 0, DOWN]],
        "正直な語と同じ再生にならない"
    );
    assert_eq!(rig.wg.cursor, 3);
}

#[test]
fn a_step_without_its_menu_is_applied_with_zero_words() {
    use higanr_core::wire::{Select, Step, StepKind};
    let step = Record::Step(Step {
        step: 0,
        kind: StepKind::Align,
        value: 3,
        screen: 7,
    });
    let sel = Select {
        match_no: 0,
        chara: 1,
        color: 0,
        life: 10,
        charge: 0,
        map: 0,
        difficulty: 3,
        cpu_1p: 0,
        cpu_2p: 0,
        background: 0,
        rng_seed: 1,
        option_digest: 0,
    };
    let cases = [
        (
            vec![
                Record::Select {
                    seat: Seat::P1,
                    sel,
                },
                Record::Select {
                    seat: Seat::P2,
                    sel,
                },
            ],
            30u32,
        ),
        (Vec::new(), 40),
    ];
    for (i, (tail, within)) in cases.into_iter().enumerate() {
        let w = MenuGame::new(0, vec![act(0, 6, 12, Z, 1)], &[0, 1, 2, 3]);
        let mut rig = Rig::new(w, 1_000);
        rig.host.push(Record::Seg { seg: 0 }).unwrap();
        rig.host.push(step).unwrap();
        for r in tail {
            rig.host.push(r).unwrap();
        }
        let mut now = 1_000_000u64;
        let mut applied_at = None;
        for t in 0..80u32 {
            rig.hook(now, true);
            now += FRAME_US;
            if applied_at.is_none() && rig.wp.stats().menu_applied == 1 {
                applied_at = Some(t);
            }
        }
        let at = applied_at.unwrap_or_else(|| panic!("ケース {i}: 押下が入らない"));
        assert_eq!(rig.wg.cursor, 3, "ケース {i}: 合わせの値");
        assert!(at <= within, "ケース {i}: {at} tick");
        assert!(rig.wg.fed_menu.iter().all(|f| *f == [0; 3]), "ケース {i}");
    }
}

#[test]
fn a_watcher_waits_for_a_host_that_stops_sending_instead_of_guessing() {
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let pause = |a: &MenuGame| a.matches == 0 && (8..20).contains(&a.played.len());
    let o = run_rig_with(a, b, w, true, &from_start, &pause, 5_000, 20_000);
    assert_follows(&o, "ホストが止まる");
    let s = o.rig.wp.stats();
    assert!(
        s.stalls > 0,
        "入力ワードを待たなかった（止めた期間を踏めていない）: {s:?}"
    );
    assert!(s.max_stall_run >= 2, "{s:?}");
}

#[test]
fn a_watcher_follows_rematches_without_reselecting() {
    let (pa, pb) = plans();
    let round0 = |p: Vec<Act>| p.into_iter().filter(|a| a.round == 0).collect::<Vec<_>>();
    let mut a = MenuGame::new(
        0,
        round0(pa),
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
    );
    let mut b = MenuGame::new(1, round0(pb), &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    for g in [&mut a, &mut b] {
        g.max_matches = 3;
        g.rematches = 2;
        g.match_frames = 50;
    }
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    let w = watcher_game(50, 3, 2);
    let o = run_rig(a, b, w, true, &from_start, 5_000, 20_000);
    let f = o.rig.folded();
    assert_eq!(
        f.segs,
        vec![0, 1, 2, 3],
        "seg の先頭（最後は試合を出るときの次の seg）"
    );
    assert_eq!(o.rig.wg.played, o.a.played, "W の列と A の列");
    assert_eq!(o.rig.wg.played, o.b.played, "W の列と B の列");
    assert_eq!(o.a.played.len(), 150);
    let segs: Vec<u16> = o.rig.wg.played.iter().map(|r| r.0).collect();
    assert_eq!((segs[0], segs[50], segs[100]), (0, 1, 2), "seg の切れ目");
    let frames: Vec<u32> = o.rig.wg.played.iter().map(|r| r.1).collect();
    assert!(
        frames.chunks(50).all(|c| c.iter().copied().eq(0..50)),
        "frame が seg ごとに 0 から続かない"
    );
    assert_eq!(o.rig.wg.watched.len(), 1);
    assert_eq!(o.rig.wp.stats().matches_started, 3);
    assert_eq!(o.rig.wp.stats().frame_gaps, 0);
    assert_eq!(
        o.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished))
    );
    assert_eq!(o.rig.wg.ignored_presses, 0);
    assert_eq!(o.rig.leaks, 0);
    assert_eq!(o.rig.allocs_joined, 0);
}

#[test]
fn a_watcher_learns_the_names_each_seg_even_joining_late_and_saves_both_matches_under_1p_vs_2p() {
    use crate::replay_save::tests::{
        FakeGame, FakeOs, synthetic_replay, temp_dir, vault_files, worker_net,
    };
    use crate::replay_save::{Attempt, Driver as SaveDriver, Handoff, MENU_SETTLE_TICKS, Watching};
    use higanr_core::replay::{self, MarkRead, MarkRole};
    let want = [Name::new(NAME_A), Name::new(NAME_B)];
    for (tag, late) in [("最初から観戦", false), ("1 試合目の途中から", true)] {
        let (a, b) = games();
        let w = watcher_game(a.match_frames, 2, 0);
        let o = if late {
            run_rig(
                a,
                b,
                w,
                true,
                &|a: &MenuGame| a.played.len() >= 5,
                5_000,
                30_000,
            )
        } else {
            run_rig(a, b, w, true, &from_start, 5_000, 20_000)
        };
        assert!(o.rig.started, "{tag}");
        assert_eq!(o.rig.wp.names(), Some(want), "{tag}: 観戦者が受けた名前");
        let f = o.rig.folded();
        assert!(f.segs.len() >= 2, "{tag}: {:?}", f.segs);
        assert_eq!(f.names.len(), 2 * f.segs.len(), "{tag}: {:?}", f.names);
        for pair in f.names.chunks(2) {
            assert_eq!(
                pair,
                [
                    (Seat::P1, NAME_A.to_string()),
                    (Seat::P2, NAME_B.to_string())
                ],
                "{tag}"
            );
        }
        let dir = temp_dir(if late { "watch_e2e_late" } else { "watch_e2e" });
        let h = Handoff::new();
        let mut d = SaveDriver::new();
        let mut g = FakeGame::default();
        let watching = Watching::Names(o.rig.wp.names().unwrap());
        d.tick_watching(&h, &mut g, None, watching);
        let (mut worker, log) = worker_net(higanr_core::vault::Split::Peer);
        let mut os = FakeOs::at(&dir);
        worker.step(&h, &mut os);
        let folder = dir.join("Replay_HiganRTool").join("host-a vs guest-b");
        for n in 1..=2u32 {
            g.wins = if n == 1 { [2, 0, 2] } else { [0, 2, 2] };
            g.menu = 1;
            let a: Vec<_> = (0..MENU_SETTLE_TICKS)
                .filter_map(|_| d.tick_watching(&h, &mut g, None, watching))
                .collect();
            assert_eq!(
                a,
                vec![Attempt::Recorded],
                "{tag}: {n} 試合目 {}",
                log.text()
            );
            let file = folder.join(format!("th9_ud{n:04}.rpy"));
            std::fs::write(&file, synthetic_replay()).unwrap();
            worker.step(&h, &mut os);
            let MarkRead::Found(m) = replay::read_mark(&std::fs::read(&file).unwrap()).unwrap()
            else {
                panic!("{tag}: 印が読めない {}", log.text());
            };
            assert_eq!(
                m.role,
                MarkRole::Watch {
                    p1: NAME_A.to_string(),
                    p2: NAME_B.to_string()
                },
                "{tag}"
            );
            assert_eq!(
                m.result,
                if n == 1 {
                    replay::MatchResult::Win
                } else {
                    replay::MatchResult::Lose
                },
                "{tag}: 1P から見た結果"
            );
            g.menu = 0;
            g.req = 10;
            d.tick_watching(&h, &mut g, None, watching);
            g.req = crate::netplay::SCENE_MATCH;
            d.tick_watching(&h, &mut g, None, watching);
            worker.step(&h, &mut os);
        }
        assert_eq!(g.calls, 2, "{tag}: 保存関数を呼んだ回数");
        let files = vault_files(&dir);
        assert_eq!(
            files[..2],
            [
                "host-a vs guest-b/th9_ud0001.rpy",
                "host-a vs guest-b/th9_ud0002.rpy"
            ],
            "{tag}: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_watcher_without_names_in_the_stream_never_saves_and_says_why() {
    use crate::replay_save::tests::{FakeGame, FakeOs, temp_dir, vault_files, worker_net};
    use crate::replay_save::{
        Attempt, Driver as SaveDriver, Handoff, MENU_SETTLE_TICKS, Skip, Watching,
    };
    DROP_NAMES.with(|c| c.set(true));
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let o = run_rig(a, b, w, true, &from_start, 5_000, 20_000);
    DROP_NAMES.with(|c| c.set(false));
    assert!(o.rig.started);
    assert!(o.rig.wp.is_joined(), "流れは受けている（名前だけが無い）");
    assert_eq!(o.rig.wp.names(), None);
    assert_eq!(o.rig.wp.stats().names, 0);
    let dir = temp_dir("watch_e2e_nonames");
    let h = Handoff::new();
    let mut d = SaveDriver::new();
    let mut g = FakeGame::default();
    let watching = o
        .rig
        .wp
        .names()
        .map_or(Watching::WaitingForNames, Watching::Names);
    assert_eq!(watching, Watching::WaitingForNames);
    let (mut worker, log) = worker_net(higanr_core::vault::Split::Peer);
    let mut os = FakeOs::at(&dir);
    g.menu = 1;
    let a: Vec<_> = (0..MENU_SETTLE_TICKS)
        .filter_map(|_| d.tick_watching(&h, &mut g, None, watching))
        .collect();
    worker.step(&h, &mut os);
    assert_eq!(a, vec![Attempt::Retry(Skip::WatchNoNames)]);
    assert_eq!(g.calls, 0);
    assert!(vault_files(&dir).is_empty(), "{:?}", vault_files(&dir));
    assert!(
        log.text().contains("観戦の試合の名前をまだ受けていない"),
        "{}",
        log.text()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_late_joiner_replays_the_current_seg_from_its_head_and_follows_the_next() {
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let late = |a: &MenuGame| a.played.len() >= 5;
    let o = run_rig(a, b, w, true, &late, 5_000, 30_000);
    assert!(o.rig.started);
    assert_follows_late(&o, "遅れた JOIN（1 試合目の途中）");
    // 選択を始まりから辿り直した（A（対戦のホスト）がfeedしたtickの数 = W（観戦者）が記録からfeedしたtickの数 + 観戦者が追いつくために捨てた`Menu`）。
    // 1マッチ目のマッチのワードは同じ速さで追う（早送りは別の経路）。そのため追いつく間に溜まったマッチのフレームが1秒を越える。
    let s = o.rig.wp.stats();
    assert_eq!(
        s.menu_frames + s.menu_skipped,
        o.npa.stats().menu_frames,
        "{s:?}"
    );
    assert!(s.margin_max_us > 1_000_000, "遅れたまま追っていない: {s:?}");
    assert!(s.backlog_max > 20, "{s:?}");
    assert_eq!(s.jumps, 0);
}

/// 長いマッチ（A・B・Wとも900フレームのマッチを2回）。遅れて入った観戦者にフレームが溜まる長さ。
fn long_games() -> (MenuGame, MenuGame, MenuGame) {
    let (mut a, mut b) = games();
    a.match_frames = 900;
    b.match_frames = 900;
    let w = watcher_game(900, 2, 0);
    (a, b, w)
}

/// 早送りの数が、走査の呼ばれ方と合っているかどうか。
/// 余分に回した数 = 余分な走査 = SE・文字の数を整えた数。
/// 1表示の余分は上限以内。余分な走査はどれもマッチのフレームを使った。
fn assert_ff_accounting(o: &Out, tag: &str) {
    let ff = o.rig.wp.stats().ff;
    let extras = o.rig.ff_scans.iter().filter(|e| **e).count() as u64;
    assert_eq!(
        ff.extra_ticks, extras,
        "{tag}: 数えた余分と回した余分: {ff:?}"
    );
    assert_eq!(u64::from(o.rig.ff_drops), extras, "{tag}: SE を捨てた数");
    assert_eq!(
        o.rig.extra_not_played, 0,
        "{tag}: 試合のフレームを使わない余分な走査"
    );
    assert_eq!(
        ff.begun,
        u64::from(o.rig.displays),
        "{tag}: 走査の包みを通った表示"
    );
    let mut run = 0u32;
    let mut longest = 0u32;
    let mut displays_with_extra = 0u64;
    for &extra in &o.rig.ff_scans {
        if extra {
            run += 1;
            longest = longest.max(run);
            if run == 1 {
                displays_with_extra += 1;
            }
        } else {
            run = 0;
        }
    }
    assert!(
        longest < FF_UPDATES_DEFAULT,
        "{tag}: 1 表示の余分 {longest}"
    );
    assert_eq!(ff.displays, displays_with_extra, "{tag}: 早送りした表示");
}

#[test]
fn a_late_watcher_fast_forwards_to_a_small_lag_and_plays_the_same_columns() {
    let late = |a: &MenuGame| a.played.len() >= 300;
    let (a, b, w) = long_games();
    let base = run_rig_inner(
        a,
        b,
        w,
        true,
        &late,
        &|_| false,
        (false, 0, None),
        5_000,
        40_000,
    );
    let (a, b, w) = long_games();
    let o = run_rig_inner(
        a,
        b,
        w,
        true,
        &late,
        &|_| false,
        (false, 0, Some(0)),
        5_000,
        40_000,
    );
    assert_follows_late(&base, "早送りなし");
    assert_follows_late(&o, "早送りあり");
    assert_eq!(o.rig.wg.played, base.rig.wg.played);
    assert_eq!(o.rig.wg.fed_played, base.rig.wg.fed_played);
    let bs = base.rig.wp.stats();
    assert_eq!(bs.ff.displays, 0, "{:?}", bs.ff);
    assert!(
        bs.margin_max_us > 5_000_000,
        "早送りなしの比べが遅れていない: {bs:?}"
    );
    let s = o.rig.wp.stats();
    let ff = s.ff;
    assert_ff_accounting(&o, "早送りあり");
    assert!(
        ff.displays > 20 && ff.episodes >= 1 && ff.finished >= 1,
        "{ff:?}"
    );
    assert!(!ff.active, "試合が終わったのに早送り中: {ff:?}");
    let enter = ff.first_enter_backlog.expect("入ったときの溜まり");
    assert!(enter > 150, "入ったときの溜まりが小さい: {ff:?}");
    let first_ms = ff.first_ms.expect("追いつくまでの時間");
    let frames = f64::from(enter) / f64::from(FF_UPDATES_DEFAULT - 1);
    assert!(
        f64::from(first_ms) <= (frames + 10.0) * 16.7,
        "追いつくのが遅い: {first_ms} ms / 溜まり {enter}: {ff:?}"
    );
    assert!(
        f64::from(first_ms) >= frames * 16.0 * 0.8,
        "追いつくのが速すぎる（上限の倍率を越えた？）: {first_ms} ms / 溜まり {enter}: {ff:?}"
    );
    assert!(o.rig.caught_up_display.is_some());
    assert!(
        o.rig.after_margins.len() > 300,
        "{}",
        o.rig.after_margins.len()
    );
    let worst = o.rig.after_margins.iter().copied().max().unwrap_or(0);
    assert!(worst < 150_000, "追いついた後も遅れている: {worst} µs");
    assert!(
        s.stalls <= bs.stalls + 10,
        "早送りの後に語を待つ: {} / {}",
        s.stalls,
        bs.stalls
    );
    assert_eq!(s.frame_gaps, 0);
    assert_eq!(s.jumps, 0);
    let st = crate::spectate_watch::WatchStatus::new();
    st.publish(&o.rig.wp);
    let text = st.text();
    assert!(text.contains("観戦者の早送り:"), "{text}");
    assert!(
        text.contains(&format!(
            "早送りした表示 {}（余分に回した更新 {}",
            ff.displays, ff.extra_ticks
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("最初の 1 回 {first_ms} ms")),
        "追いつくまでの時間が出ない: {text}"
    );
    assert!(
        text.contains(&format!(
            "{FF_ENTER_FRAMES} フレームを越えたら入り {FF_EXIT_FRAMES} 以下で戻る"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("上限 {FF_BUDGET_US} µs")), "{text}");
}

#[test]
fn a_late_watcher_of_a_delay_match_catches_up_only_when_the_scan_call_is_placed_for_watchers() {
    use higanr_core::rollback::Mode;
    let late = |a: &MenuGame| a.played.len() >= 300;
    let mut runs = Vec::new();
    for watching in [true, false] {
        let (placed, writes) =
            crate::rollback_play::tests::scan_call_written_after_setup(Mode::Delay, watching);
        assert_eq!(
            placed, watching,
            "走査の call が書かれたか（観戦者 {watching}）"
        );
        assert_eq!(
            writes,
            usize::from(watching),
            "書いた地点の数（観戦者 {watching}）"
        );
        let (a, b, w) = long_games();
        let o = run_rig_inner(
            a,
            b,
            w,
            true,
            &late,
            &|_| false,
            (false, 0, placed.then_some(0)),
            5_000,
            40_000,
        );
        assert_follows_late(
            &o,
            if watching {
                "観戦者の実行"
            } else {
                "対戦者の実行"
            },
        );
        runs.push(o);
    }
    let (watched, unplaced) = (&runs[0], &runs[1]);
    assert_eq!(watched.rig.wg.played, unplaced.rig.wg.played);
    let s = watched.rig.wp.stats();
    assert!(
        s.ff.displays > 20 && s.ff.episodes >= 1 && s.ff.finished >= 1 && !s.ff.active,
        "{:?}",
        s.ff
    );
    assert_ff_accounting(watched, "delay の試合の観戦者");
    assert!(watched.rig.caught_up_display.is_some());
    let worst = watched.rig.after_margins.iter().copied().max().unwrap_or(0);
    assert!(
        watched.rig.after_margins.len() > 300 && worst < 150_000,
        "追いついた後も遅れている: {worst} µs（{} 件）",
        watched.rig.after_margins.len()
    );
    let u = unplaced.rig.wp.stats();
    assert_eq!(u.ff.begun, 0, "{:?}", u.ff);
    assert_eq!(u.ff.displays, 0, "{:?}", u.ff);
    assert!(u.margin_max_us > 5_000_000, "{u:?}");
}

#[test]
fn a_watcher_that_keeps_up_never_fast_forwards() {
    for spec_delay_ms in [5u64, 40] {
        let (a, b) = games();
        let w = watcher_game(a.match_frames, 2, 0);
        let o = run_rig_inner(
            a,
            b,
            w,
            true,
            &from_start,
            &|_| false,
            (false, 0, Some(0)),
            spec_delay_ms * 1_000,
            20_000,
        );
        let tag = format!("最初から付く・片道 {spec_delay_ms} ms");
        assert_follows(&o, &tag);
        let ff = o.rig.wp.stats().ff;
        assert_eq!(ff.displays, 0, "{tag}: {ff:?} / {:?}", o.rig.wp.stats());
        assert_eq!(ff.episodes, 0, "{tag}: {ff:?}");
        assert!(ff.begun > 100, "{tag}: 早送りの経路を通っていない {ff:?}");
        assert_ff_accounting(&o, &tag);
    }
}

#[test]
fn the_watchers_hud_has_names_and_delay_only_inside_a_match_and_marks_fast_forward() {
    let want = HudNames {
        p1: Name::new(NAME_A),
        p2: Name::new(NAME_B),
    };
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let o = run_rig_inner(
        a,
        b,
        w,
        true,
        &from_start,
        &|_| false,
        (false, 0, Some(0)),
        5_000,
        20_000,
    );
    assert_follows(&o, "最初から");
    let st = o.rig.wp.stats();
    let some: Vec<&WatchHudFacts> = o.rig.hud.iter().flatten().collect();
    assert!(
        some.len() as u64 >= st.played,
        "試合の中の更新 {} が使ったフレーム {} より少ない",
        some.len(),
        st.played
    );
    assert!(
        some.len() < o.rig.hud.len(),
        "試合の外でも材料が出ている: {} 件中 {} 件",
        o.rig.hud.len(),
        some.len()
    );
    assert!(o.rig.hud.first().is_some_and(Option::is_none));
    assert!(o.rig.hud.last().is_some_and(Option::is_none), "終わった後");
    assert!(some.iter().all(|h| h.names == Some(want)), "名前が違う");
    assert!(some.iter().all(|h| !h.fast_forward), "早送りの印が出た");
    let top = some.iter().map(|h| h.delay_frames).max().unwrap_or(0);
    assert_eq!(top, st.backlog_max, "遅れの最大と溜まりの最大が違う");
    for (np, own, peer) in [(&o.npa, NAME_A, NAME_B), (&o.npb, NAME_B, NAME_A)] {
        let v = crate::replay_save::NetView::of(np).expect("つながっていた接続");
        assert_eq!((v.own_name, v.peer_name), (Name::new(own), Name::new(peer)));
    }
    let (a, b, w) = long_games();
    let o = run_rig_inner(
        a,
        b,
        w,
        true,
        &|a: &MenuGame| a.played.len() >= 300,
        &|_| false,
        (false, 0, Some(0)),
        5_000,
        40_000,
    );
    assert_follows_late(&o, "遅れて入る");
    let some: Vec<&WatchHudFacts> = o.rig.hud.iter().flatten().collect();
    let ff_on: Vec<&&WatchHudFacts> = some.iter().filter(|h| h.fast_forward).collect();
    assert!(!ff_on.is_empty(), "早送りの間に印が出ない");
    let first_ff = ff_on[0];
    assert!(
        first_ff.delay_frames > FF_ENTER_FRAMES,
        "早送りの最初の遅れが小さい: {}",
        first_ff.delay_frames
    );
    let tail = some.last().expect("試合の中の更新");
    assert!(
        !tail.fast_forward && tail.delay_frames <= FF_ENTER_FRAMES,
        "追いついた後も遅れが大きい・印が残る: {tail:?}"
    );
    assert!(some.iter().all(|h| h.names == Some(want)), "名前が違う");
}

#[test]
fn a_burst_after_a_host_pause_is_caught_up_by_fast_forward() {
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let (mut a, mut b, mut w) = (a, b, w);
    for g in [&mut a, &mut b, &mut w] {
        g.match_frames = 600;
    }
    let pause = |a: &MenuGame| a.matches == 0 && (100..160).contains(&a.played.len());
    let o = run_rig_inner(
        a,
        b,
        w,
        true,
        &from_start,
        &pause,
        (false, 0, Some(0)),
        5_000,
        30_000,
    );
    assert_follows(&o, "ホストが止まった後の早送り");
    assert_ff_accounting(&o, "ホストが止まった後の早送り");
    let s = o.rig.wp.stats();
    assert!(s.stalls > 0, "止めた期間を踏めていない: {s:?}");
    assert!(
        s.ff.displays > 0 && s.ff.finished >= 1,
        "まとめて届いた分を早送りしなかった: {:?}",
        s.ff
    );
    assert!(!s.ff.active, "{:?}", s.ff);
}

#[test]
fn the_time_budget_cuts_off_the_extra_updates_of_one_display() {
    let late = |a: &MenuGame| a.played.len() >= 300;
    let run = |cost_us: u64| {
        let (a, b, w) = long_games();
        run_rig_inner(
            a,
            b,
            w,
            true,
            &late,
            &|_| false,
            (false, 0, Some(cost_us)),
            5_000,
            40_000,
        )
    };
    let fast = run(0);
    let slow = run(5_000);
    assert_follows_late(&fast, "更新が軽い");
    assert_follows_late(&slow, "更新が重い");
    assert_ff_accounting(&slow, "更新が重い");
    let (fs, ss) = (fast.rig.wp.stats().ff, slow.rig.wp.stats().ff);
    assert_eq!(fs.cutoffs, 0, "{fs:?}");
    assert!(ss.cutoffs > 20, "上限で打ち切らない: {ss:?}");
    let mut run = 0u32;
    let mut longest = 0u32;
    for &extra in &slow.rig.ff_scans {
        run = if extra { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    assert_eq!(longest, 2, "上限の時間で 2 回に収まらない: {ss:?}");
    assert!(
        u64::from(ss.batch_max_us) <= 2 * 5_000 + 1,
        "1 表示の余分の時間が上限を越えた: {ss:?}"
    );
    assert!(
        ss.first_ms.unwrap_or(0) > fs.first_ms.unwrap_or(u32::MAX),
        "上限で打ち切ったのに追いつくのが遅くならない: {ss:?} / {fs:?}"
    );
    assert_eq!(slow.rig.wg.played, fast.rig.wg.played);
}

#[test]
fn a_late_joiner_walks_from_screen_seven_to_the_chara_screen_of_a_seg_that_starts_there() {
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let late = |a: &MenuGame| a.matches >= 1 && a.cur == 2 && a.played.len() >= 25;
    let o = run_rig(a, b, w, true, &late, 5_000, 30_000);
    assert!(o.rig.started, "W を起こせていない");
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    let s = wp.stats();
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{s:?}"
    );
    assert!(s.menu_walked >= 1, "画面 8 まで進めていない: {s:?}");
    let first = o.a.played.iter().take_while(|r| r.0 == 0).count();
    assert!(
        first > 0 && first < o.a.played.len(),
        "A が 2 試合していない"
    );
    assert_eq!(wg.played, o.a.played[first..], "2 試合目の列");
    assert_eq!(wg.played, o.b.played[first..], "2 試合目の列（B）");
    assert!(wg.played.iter().all(|r| r.0 == 1), "seg 0 の語を使った");
    assert_eq!(wg.watched.len(), 1, "止める地点は 2 試合目の 1 回だけ");
    assert_eq!(s.matches_started, 1, "{s:?}");
    assert_eq!((s.frame_gaps, s.jumps, s.stalls), (0, 0, 0), "{s:?}");
    assert_eq!(wg.ignored_presses, 0, "読まれない押下を入れた");
    assert_eq!(o.rig.leaks, 0);
    assert_eq!(o.rig.allocs_joined, 0, "on_input が確保した");
}

#[test]
fn the_screen_eight_walk_does_not_fire_when_the_game_is_where_the_record_starts() {
    type When = Box<dyn Fn(&MenuGame) -> bool>;
    let cases: [(&str, When); 2] = [
        ("最初から", Box::new(from_start)),
        (
            "1 試合目の途中",
            Box::new(|a: &MenuGame| a.played.len() >= 5),
        ),
    ];
    for (name, late) in cases {
        let (a, b) = games();
        let w = watcher_game(a.match_frames, 2, 0);
        let o = run_rig(a, b, w, true, &*late, 5_000, 30_000);
        assert!(o.rig.started, "{name}: W を起こせていない");
        let s = o.rig.wp.stats();
        assert_eq!(s.menu_walked, 0, "{name}: 自分で決定を押した {s:?}");
        if name == "最初から" {
            assert_follows(&o, name);
        } else {
            assert_follows_late(&o, name);
        }
    }
}

/// 1回選んで2回再戦する3試合（各50フレーム）。観戦者は2試合目（再戦のseg 1）の途中で入る。
/// `bare_tail`の意味は`run_rig_full`と同じ（最後のsegを出た後の選択の記録を積まず、`End`が`Seg`の直後に来る）。
fn rematch_run(bare_tail: bool) -> Out {
    let (pa, pb) = plans();
    let round0 = |p: Vec<Act>| p.into_iter().filter(|a| a.round == 0).collect::<Vec<_>>();
    let mut a = MenuGame::new(
        0,
        round0(pa),
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
    );
    let mut b = MenuGame::new(1, round0(pb), &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    for g in [&mut a, &mut b] {
        g.max_matches = 3;
        g.rematches = 2;
        g.match_frames = 50;
    }
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    let w = watcher_game(50, 3, 2);
    let late = |a: &MenuGame| a.matches == 1 && a.played.len() >= 60;
    run_rig_full(
        a,
        b,
        w,
        true,
        &late,
        &|_| false,
        (bare_tail, 0),
        5_000,
        20_000,
    )
}

#[test]
fn a_late_joiner_in_a_rematch_seg_replays_the_rematches_from_the_last_selection() {
    let o = rematch_run(false);
    assert!(o.rig.started);
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    let s = wp.stats();
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{s:?}"
    );
    assert_eq!(o.a.played.len(), 150, "A が 3 試合していない");
    assert_eq!(wg.played, o.a.played, "1 試合目の選択から全部");
    assert_eq!(wg.played, o.b.played);
    assert_eq!(wg.watched.len(), 1, "止める地点は最初の選択の 1 回");
    assert_eq!(s.matches_started, 3, "{s:?}");
    assert_eq!(s.orphan_frames, 0, "{s:?}");
    assert!(
        o.rig.awaiting.iter().all(|(a, _)| !a),
        "遡れたのに次の選択を待つ印が立った"
    );
    assert_eq!((s.stray_records, s.stalls, s.jumps), (0, 0, 0), "{s:?}");
    assert_eq!(o.rig.leaks, 0);
    assert_eq!(wg.ignored_presses, 0);
}

#[test]
fn a_late_joiner_in_a_rematch_seg_discards_the_frames_it_cannot_replay() {
    HOST_REWIND.with(|c| c.set(Some(0)));
    // ホストは最後の試合の後の選択を積まない: 印が立ったまま`End`を受ける形（終われば下りる）。
    let o = rematch_run(true);
    HOST_REWIND.with(|c| c.set(None));
    assert!(o.rig.started);
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    assert_eq!(wp.end(), Some(WatchEnd::HostEnded(ByeReason::Finished)));
    assert!(wg.played.is_empty(), "選択の記録の無い試合の語を使った");
    assert!(wg.watched.is_empty());
    let s = wp.stats();
    assert!(s.orphan_frames >= 90, "{s:?}");
    assert_eq!((s.stray_records, s.stalls, s.jumps), (0, 0, 0), "{s:?}");
    assert_eq!(wg.screen, 7, "選択の画面のまま（自分の入力は届かない）");
    assert_eq!(o.rig.leaks, 0);
    assert_eq!(wg.ignored_presses, 0);
    // 最初の試合の語を捨てたtickから観戦が終わるまで「次のキャラ選択から映ります」の印が続き、終われば下りる。
    let marks: Vec<bool> = o.rig.awaiting.iter().map(|x| x.0).collect();
    let first = marks.iter().position(|a| *a).expect("印が立っていない");
    let last = marks.iter().rposition(|a| *a).unwrap_or(first);
    assert!(marks[first..=last].iter().all(|a| *a), "途中で印が下りた");
    assert_eq!(marks.last(), Some(&false), "終わった後も印が立っている");
}

#[test]
fn a_late_joiner_in_a_rematch_seg_waits_for_the_next_seg_and_walks_into_its_selection() {
    HOST_REWIND.with(|c| c.set(Some(0)));
    let (pa, pb) = plans();
    let remap = |p: Vec<Act>| {
        p.into_iter()
            .map(|a| Act {
                round: if a.round == 1 { 2 } else { a.round },
                ..a
            })
            .collect::<Vec<_>>()
    };
    let mut a = MenuGame::new(
        0,
        remap(pa),
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
    );
    let mut b = MenuGame::new(1, remap(pb), &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    for g in [&mut a, &mut b] {
        g.max_matches = 4;
        g.rematches = 1;
        g.match_frames = 40;
    }
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    let w = watcher_game(40, 4, 1);
    let late = |a: &MenuGame| a.matches == 1 && a.played.len() >= 50;
    let o = run_rig(a, b, w, true, &late, 5_000, 40_000);
    HOST_REWIND.with(|c| c.set(None));
    assert!(o.rig.started, "W を起こせていない");
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    let s = wp.stats();
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{s:?}"
    );
    assert_eq!(o.a.played.len(), 160, "A が 4 試合していない");
    let from_seg2 = o.a.played.iter().take_while(|r| r.0 < 2).count();
    assert_eq!(from_seg2, 80);
    assert_eq!(wg.played, o.a.played[from_seg2..], "seg 2・3 の列");
    assert_eq!(wg.played, o.b.played[from_seg2..], "seg 2・3 の列（B）");
    assert!(s.orphan_frames >= 40, "{s:?}");
    assert!(s.menu_walked >= 1, "画面 8 まで進めていない: {s:?}");
    assert_eq!(wg.watched.len(), 1, "止める地点は seg 2 の 1 回だけ");
    assert_eq!(s.matches_started, 2, "{s:?}");
    assert_eq!((s.stray_records, s.stalls, s.jumps), (0, 0, 0), "{s:?}");
    assert_eq!(wg.ignored_presses, 0);
    assert_eq!(o.rig.leaks, 0);
    // 待つ間だけ印が立ち、次の選択の記録を使った後（試合を映している間）は立たない。
    assert!(o.rig.awaiting.iter().any(|(a, _)| *a), "印が立っていない");
    assert!(
        !o.rig.awaiting.iter().any(|&(a, played)| a && played > 0),
        "試合を映し始めた後も印が立っている"
    );
}

#[test]
fn the_wait_mark_goes_down_with_the_first_selection_record_used_menu_or_step() {
    use higanr_core::wire::{Step, StepKind};
    for with_step in [false, true] {
        let w = MenuGame::new(0, vec![act(0, 6, 12, Z, 1)], &[0, 1, 2, 3]);
        let mut rig = Rig::new(w, 1_000);
        for r in [
            Record::Seg { seg: 0 },
            Record::Frame {
                frame: 0,
                p1: 1,
                p2: 2,
            },
            Record::Frame {
                frame: 1,
                p1: 1,
                p2: 2,
            },
        ] {
            rig.host.push(r).unwrap();
        }
        let mut now = 1_000_000u64;
        for _ in 0..300 {
            rig.hook(now, true);
            now += FRAME_US;
        }
        assert!(rig.wp.awaiting_select(), "{with_step}: 印が立っていない");
        rig.host.push(Record::Seg { seg: 1 }).unwrap();
        if with_step {
            // 押下とそのtickの`Menu`はStepの側で一緒に使う（`Menu`の枝を通らない）。
            rig.host
                .push(Record::Step(Step {
                    step: 0,
                    kind: StepKind::Align,
                    value: 2,
                    screen: 7,
                }))
                .unwrap();
        }
        rig.host.push(Record::Menu { p1: 0, p2: 0 }).unwrap();
        for _ in 0..60 {
            rig.hook(now, true);
            now += FRAME_US;
        }
        let s = rig.wp.stats();
        assert_eq!(s.menu_applied, u32::from(with_step), "{with_step}: {s:?}");
        assert!(!rig.wp.awaiting_select(), "{with_step}: 印が下りない {s:?}");
    }
}

#[test]
fn the_screen_eight_walk_is_only_for_the_head_of_a_seg() {
    use higanr_core::wire::{Step, StepKind};
    let menu = |p1, p2| Record::Menu { p1, p2 };
    let w = MenuGame::new(0, vec![act(0, 6, 12, Z, 1)], &[0, 1, 2, 3]);
    let mut rig = Rig::new(w, 1_000);
    for r in [
        Record::Seg { seg: 0 },
        Record::Step(Step {
            step: 0,
            kind: StepKind::Align,
            value: 2,
            screen: 7,
        }),
        menu(0, 0),
        Record::Step(Step {
            step: 1,
            kind: StepKind::Decide,
            value: 0,
            screen: 8,
        }),
        menu(0, 0),
    ] {
        rig.host.push(r).unwrap();
    }
    let mut now = 1_000_000u64;
    for _ in 0..800 {
        rig.hook(now, true);
        now += FRAME_US;
    }
    let s = rig.wp.stats();
    assert_eq!(s.menu_walked, 0, "seg の途中で決定を押して進めた: {s:?}");
    assert!(
        matches!(rig.wp.end(), Some(WatchEnd::Stuck(st)) if st.screen == 8),
        "{:?} {s:?}",
        rig.wp.end()
    );
    assert_eq!(rig.wg.screen, 7, "ゲームの画面が動いた");
}

#[test]
fn the_players_are_the_same_with_and_without_a_watcher() {
    let run = |with: bool| {
        let (a, b) = games();
        let w = watcher_game(a.match_frames, 2, 0);
        run_rig(a, b, w, with, &from_start, 5_000, 20_000)
    };
    let (with, without) = (run(true), run(false));
    assert!(with.rig.wp.end().is_some());
    assert_eq!(with.a.played, without.a.played);
    assert_eq!(with.b.played, without.b.played);
    let same = |s: super::Stats| super::Stats { ticks: 0, ..s };
    assert_eq!(same(with.npa.stats()), same(without.npa.stats()));
    assert_eq!(same(with.npb.stats()), same(without.npb.stats()));
    let steps = |np: &NetPlay| np.step_log().iter().map(|s| s.step).collect::<Vec<_>>();
    assert_eq!(steps(&with.npa), steps(&without.npa));
    assert_eq!(with.a.screens, without.a.screens);
}

/// 製品の既定の作り方（ゲームの値）のCHECKでも、ホストが積んだ照合を観戦者が全部自分のゲームの値と比べて合う（`assert_follows`）。
/// frame 0の抽選回数は両者で違う（読み込み中の乱数の生成回数が違う。偽のゲームの`load_draws`）。
/// そのため比べないフィールドが効いていることも見る。
#[test]
fn the_watch_checks_match_with_the_game_values_too() {
    HOST_CHECK.with(|c| c.set(Some(CheckSource::Game)));
    let (mut a, b) = games();
    a.load_draws = 7;
    let mut w = watcher_game(a.match_frames, 2, 0);
    w.load_draws = 3;
    let o = run_rig(a, b, w, true, &from_start, 5_000, 20_000);
    HOST_CHECK.with(|c| c.set(None));
    assert_follows(&o, "ゲームの値の照合");
    let f = o.rig.folded();
    assert!(
        f.checks
            .iter()
            .all(|(k, _)| *k == higanr_core::wire::CheckKind::Game),
        "作り方がゲームの値でない"
    );
    assert!(
        f.checks
            .iter()
            .any(|(_, c)| c.frame == 0 && c.rng_count == 7)
    );
}

#[test]
fn the_watchers_own_keys_never_reach_the_game_except_esc_which_leaves() {
    let (a, b) = games();
    let mut w = watcher_game(a.match_frames, 2, 0);
    // 選択の画面とマッチの中では、X・Z・Shiftを3 tickごとに叩く人の手（画面6の外の全部の画面で）。Escは含めない。
    w.spam = Some((7, X | Z | SHIFT, 3));
    let o = run_rig(a, b, w, true, &from_start, 5_000, 20_000);
    assert_follows(&o, "観戦者の手がうるさい");
    assert_eq!(o.rig.wg.screens, o.a.screens, "画面の列が A と同じ");
    assert!(
        u64::from(o.rig.wg.peek_calls) >= o.rig.wp.stats().ticks / 2,
        "Esc を読む経路が毎 tick 通っていない: {} 回 / {} tick",
        o.rig.wg.peek_calls,
        o.rig.wp.stats().ticks
    );
}

#[test]
fn before_joining_decides_are_dropped() {
    let slot = Arc::new(WatchSlot::new());
    let (ha, wa) = spec_addrs();
    slot.offer(WatcherConfig {
        session_id: SPEC_SESSION,
        secret: SPEC_SECRET,
        nonce: 1,
        name: Name::new("w"),
        host: Candidates::from_addrs(&[ha]).unwrap(),
        host_public: spec_host_public(),
        key_seed: SPEC_WATCHER_SEED,
    });
    let (_hnet, mut wnet, clock) = FakeNet::pair(ha, wa, 1_000);
    let mut wp = WatchPlay::new(
        slot,
        WatchConfig {
            wait_budget_us: WAIT_BUDGET_US,
            margin_frames: WATCH_MARGIN_FRAMES,
            require_net_selection: true,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    let mut g = MenuGame::new(
        0,
        vec![act(0, 6, 12, Z, 1), act(0, 7, 12, Z, 1)],
        &[0, 1, 2],
    );
    for _ in 0..200 {
        g.tick(&mut wp, &mut wnet);
        clock.set(clock.get() + FRAME_US);
    }
    assert_eq!(wp.phase(), Phase::Unconnected);
    assert_eq!(g.screen, 7, "繋がる前に決定が通った（8 へ進んだ）");
    assert!(wp.stats().pre_join_decides >= 1);
    // 対照: 決定を捨てない設定（選択の画面を持たないゲームのウィンドウ）なら、同じ台本で8へ進む。決定の押下が本当に7で押されていることのチェック。
    let slot2 = Arc::new(WatchSlot::new());
    let mut wp2 = WatchPlay::new(
        slot2,
        WatchConfig {
            wait_budget_us: WAIT_BUDGET_US,
            margin_frames: WATCH_MARGIN_FRAMES,
            require_net_selection: false,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    let mut g2 = MenuGame::new(
        0,
        vec![act(0, 6, 12, Z, 1), act(0, 7, 12, Z, 1)],
        &[0, 1, 2],
    );
    let (_h2, mut wnet2, clock2) = FakeNet::pair(ha, wa, 1_000);
    for _ in 0..200 {
        g2.tick(&mut wp2, &mut wnet2);
        clock2.set(clock2.get() + FRAME_US);
    }
    assert_eq!(g2.screen, 8, "対照: 決定を落とさなければ 8 へ進む台本");
}

#[test]
fn a_silent_host_hands_the_input_back_after_the_silent_time() {
    let (a, b) = games();
    let w = watcher_game(a.match_frames, 2, 0);
    let _ = b;
    let mut rig = Rig::new(w, 1_000);
    let mut now = 1_000_000u64;
    for _ in 0..600 {
        rig.hook(now, true);
        now += FRAME_US;
    }
    assert!(rig.wp.is_joined());
    assert_eq!(rig.wp.end(), None, "合図が届く間に終えた");
    let silent_ticks = u64::from(HOST_SILENT_END_MS) * 1_000 / FRAME_US + 120;
    for _ in 0..silent_ticks {
        rig.sclock.set(rig.sclock.get().max(now));
        rig.wg.tick(&mut rig.wp, &mut rig.wnet);
        now += FRAME_US;
    }
    assert_eq!(rig.wp.end(), Some(WatchEnd::HostSilent));
    assert_eq!(rig.wp.phase(), Phase::Ended);
    // ホストが黙ったのでBYEは送らない。再JOINは送り続けた（ホストが戻れば入り直せる）。
    let mut buf = [0u8; MAX_DATAGRAM];
    let (mut joins, mut byes) = (0, 0);
    while let Some((_, n, _)) = rig.hnet.recv(&mut buf) {
        let opener = watcher_sent_opener();
        match wire::decode(&rig.key, Some(&opener), buf.get(..n).unwrap_or(&[])).map(|p| p.msg) {
            Ok(Message::SpectateJoin(_)) => joins += 1,
            Ok(Message::Bye(_)) => byes += 1,
            _ => {}
        }
    }
    assert_eq!(byes, 0);
    assert!(joins >= 3, "再 JOIN {joins}");
}

/// 1マッチだけ通すA・B（選択は1回。マッチが終わるとホストは`Seg(次)`を積み、閉じると`End`が続く）と、マッチが終わらないW。
fn single_match_games() -> (MenuGame, MenuGame, MenuGame) {
    let (pa, pb) = plans();
    let round0 = |p: Vec<Act>| p.into_iter().filter(|a| a.round == 0).collect::<Vec<_>>();
    let mut a = MenuGame::new(
        0,
        round0(pa),
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
    );
    let mut b = MenuGame::new(1, round0(pb), &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    a.max_matches = 1;
    b.max_matches = 1;
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    let w = watcher_game(u32::MAX, 1, 0);
    (a, b, w)
}

#[test]
fn a_seg_then_end_inside_a_match_is_the_host_finishing_not_lost() {
    let (a, b, w) = single_match_games();
    let o = run_rig_full(
        a,
        b,
        w,
        true,
        &from_start,
        &|_| false,
        (true, 0),
        5_000,
        20_000,
    );
    assert!(!o.a.played.is_empty());
    assert_eq!(
        o.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{:?}",
        o.rig.wp.stats()
    );
    assert_eq!(o.rig.wg.played, o.a.played, "最後のフレームまで使い終える");
    assert_eq!(o.rig.wp.stats().frame_gaps, 0);
    assert_eq!(o.rig.leaks, 0);
}

#[test]
fn a_seg_whose_next_record_is_late_is_waited_for() {
    let (a, b, w) = single_match_games();
    let base = run_rig_full(
        a,
        b,
        w,
        true,
        &from_start,
        &|_| false,
        (true, 0),
        5_000,
        20_000,
    );
    let (a, b, w) = single_match_games();
    let o = run_rig_full(
        a,
        b,
        w,
        true,
        &from_start,
        &|_| false,
        (true, 90),
        5_000,
        20_000,
    );
    assert_eq!(
        o.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{:?}",
        o.rig.wp.stats()
    );
    assert_eq!(o.rig.wg.played, o.a.played);
    assert!(
        o.rig.wp.stats().stalls >= base.rig.wp.stats().stalls + 30,
        "待たなかった: {:?} / {:?}",
        o.rig.wp.stats(),
        base.rig.wp.stats()
    );
}

#[test]
fn a_seg_followed_by_more_records_still_ends_lost() {
    let (a, b) = games();
    let w = watcher_game(u32::MAX, 2, 0);
    let o = run_rig(a, b, w, true, &from_start, 5_000, 20_000);
    assert_eq!(o.rig.wp.end(), Some(WatchEnd::Lost));
}

// ---------------------------------------------------------------- 観戦者のEsc（観戦をやめる）

/// Escの検査の対戦する側（`games()`のマッチを80フレームに伸ばしたもの。マッチの20フレーム目がマッチの途中になる）。
const ESC_MATCH_FRAMES: u32 = 80;

fn esc_games() -> (MenuGame, MenuGame) {
    let (mut a, mut b) = games();
    a.match_frames = ESC_MATCH_FRAMES;
    b.match_frames = ESC_MATCH_FRAMES;
    (a, b)
}

/// Escの検査の観戦者（マッチの長さは`esc_games()`と同じ）。
fn esc_watcher() -> MenuGame {
    watcher_game(ESC_MATCH_FRAMES, 2, 0)
}

/// 2回の短いEsc。`when`が真になったtickと、その`gap` tick後に1 tickずつ押す（マッチの中の一時停止と、その解除）。
fn esc_taps(when: impl Fn(&MenuGame) -> bool + 'static, gap: u32) -> [Press; 2] {
    let at = Rc::new(Cell::new(None::<u32>));
    let first = Rc::clone(&at);
    [
        Press::new(
            move |g| {
                let hit = when(g);
                if hit {
                    first.set(Some(g.tick_no));
                }
                hit
            },
            ESC,
            1,
        ),
        Press::new(
            move |g| at.get().is_some_and(|t| g.tick_no >= t + gap),
            ESC,
            1,
        ),
    ]
}

/// 長押しの検査で押し続けるtick（3秒 ＝ 180 tickに10 tickの余り）。
const ESC_HOLD_TICKS: u32 = 190;

/// 観戦をやめるEscの長押し: `when`が真になったtickから`ESC_HOLD_TICKS` tick押し続ける。
fn esc_hold(when: impl Fn(&MenuGame) -> bool + 'static) -> Press {
    Press::new(when, ESC, ESC_HOLD_TICKS)
}

/// Escの検査の実行: 観戦の片道1 ms（BYEがホストへ次のtickの始まりまでに届く）。
fn esc_run(w: MenuGame) -> Out {
    let (a, b) = esc_games();
    run_rig(a, b, w, true, &from_start, 1_000, 20_000)
}

/// 観戦者が出したBYE（ホストが受けたもの）の（理由, 観戦者の何tick目の後に受けたか）。
fn byes_of(o: &Out) -> Vec<(ByeReason, u32)> {
    o.rig
        .sent
        .iter()
        .zip(&o.rig.sent_at)
        .filter_map(|(m, t)| match m {
            Message::Bye(r) => Some((*r, *t)),
            _ => None,
        })
        .collect()
}

/// Escで抜けた実行に共通のチェック。抜けたEscのtickの番号を返す。
/// - 終わりは`Quit`、段階は`Ended`。
/// - BYEは1通（理由は取消）で、抜けたEscの次のフレーム（受信スレッドの次の周回）に出ている。
/// - そのtickのEscはゲームへ届いている。
/// - 以後のtickは全部`Local`。
/// - ホストは観戦者を「自分で抜けた」で外している。
/// - 観戦者の`on_input`は接続した後に確保していない。
fn assert_left_by_esc(o: &Out, tag: &str) -> u32 {
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    assert_eq!(wp.end(), Some(WatchEnd::Quit), "{tag}: {:?}", wp.stats());
    assert_eq!(wp.phase(), Phase::Ended, "{tag}");
    let byes = byes_of(o);
    assert_eq!(
        byes.iter().map(|b| b.0).collect::<Vec<_>>(),
        vec![ByeReason::Cancelled],
        "{tag}: BYE の理由"
    );
    let t = byes[0].1 - 1;
    assert!(
        wg.esc_local.contains(&t),
        "{tag}: 抜けた tick {t} の Esc がゲームへ届いていない: {:?}",
        wg.esc_local
    );
    assert!(o.rig.post_end_ticks >= 5, "{tag}: {}", o.rig.post_end_ticks);
    assert_eq!(
        o.rig.post_end_not_local, 0,
        "{tag}: 終えた後に Local でない"
    );
    let h = o.rig.host.counts();
    assert_eq!(
        (o.rig.host.spectator_count(), h.left, h.silenced),
        (0, 1, 0),
        "{tag}: {h:?}"
    );
    assert_eq!(o.rig.allocs_joined, 0, "{tag}: on_input が確保した");
    t
}

#[test]
fn holding_esc_in_a_selection_screen_leaves_with_one_bye_and_short_escs_do_not() {
    let (a, _) = esc_games();
    let mut w = esc_watcher();
    // 短いEsc 2回では終えない（前の作りの「3秒以内の2回」も終えない）。
    let tapped = Rc::new(Cell::new(None::<u32>));
    let first = Rc::clone(&tapped);
    w.presses.extend(esc_taps(
        move |g| {
            let hit = g.cur == 1 && g.screen == 8 && g.tick_no > 30;
            if hit {
                first.set(Some(g.tick_no));
            }
            hit
        },
        20,
    ));
    let pressed = Rc::new(Cell::new(None::<u32>));
    let at = Rc::clone(&pressed);
    w.presses.push(esc_hold(move |g| {
        let hit = tapped.get().is_some_and(|t| g.tick_no > t + 40);
        if hit && at.get().is_none() {
            at.set(Some(g.tick_no));
        }
        hit
    }));
    w.presses.push(Press::new(
        |g| g.esc_local.len() >= 2 && g.tick_no > g.esc_local[0] + ESC_HOLD_TICKS,
        ESC,
        1,
    ));
    // ホストAがキャラ選択（画面8）で5秒待つ形（長押しの3秒の間に試合へ入らない）。
    let (mut pa, _) = plans();
    pa.insert(4, act(0, 8, 300, 0, 1));
    let mut slow = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    slow.max_matches = a.max_matches;
    slow.match_frames = a.match_frames;
    slow.match_words = a.match_words;
    let (_, b) = esc_games();
    let o = run_rig(slow, b, w, true, &from_start, 1_000, 20_000);
    let t = assert_left_by_esc(&o, "選択の画面 8");
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    let s = wp.stats();
    assert!(
        s.pause.esc_outside >= 3,
        "マッチの外の押下を数えていない: {s:?}"
    );
    let from = pressed.get().expect("長押しを始めていない");
    assert_eq!(t - from, 180, "押し始めから 3 秒（180 tick）で終える");
    assert!(
        s.menu_applied >= 1,
        "選択の押下を入れていない（8 の画面に来ていない）"
    );
    assert_eq!(
        wg.esc_local.first(),
        Some(&t),
        "抜ける前の Esc がゲームへ届いた"
    );
    assert!(
        wg.esc_local.iter().any(|&k| k > t + ESC_HOLD_TICKS),
        "抜けた後の Esc もゲームへ届く: {:?}",
        wg.esc_local
    );
    assert_eq!(byes_of(&o).len(), 1, "抜けた後の Esc で BYE が増えた");
    assert!(
        o.a.played.len() >= a.match_frames as usize,
        "観戦者が抜けた後に対戦者が試合をしていない"
    );
}

/// ポーズの状態を読み取れない偽のゲーム（`MenuGame`は`play_view`が無い）では、マッチの中のEscはゲームのポーズを開かない。
/// 代わりに重ね描きの一時停止（描画だけのフレーム）にする。
/// もう1回のEscで戻り、止めた間の記録を使って最後までAと同じ列になる。観戦は終えない。
#[test]
fn esc_in_a_match_without_a_readable_pause_holds_by_the_overlay_and_a_second_esc_resumes() {
    let mut w = esc_watcher();
    w.presses
        .extend(esc_taps(|g| g.cur == 2 && g.played.len() >= 20, 45));
    let o = esc_run(w);
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{:?}",
        wp.stats()
    );
    assert!(wg.esc_local.is_empty(), "マッチの中の Esc がゲームへ届いた");
    let p = wp.stats().pause;
    assert_eq!((p.opened, p.overlays), (0, 1), "{p:?}");
    assert_eq!(p.overlay_ticks, 45, "止めた tick の数: {p:?}");
    assert_eq!(wg.played, o.a.played, "止めた後も A と同じ列");
    assert_eq!(
        byes_of(&o).iter().map(|b| b.0).collect::<Vec<_>>(),
        vec![ByeReason::Finished],
        "一時停止で BYE を出した"
    );
    assert_eq!(o.rig.allocs_joined, 0, "on_input が確保した");
}

#[test]
fn esc_before_the_selection_screens_leaves_too() {
    let mut w = esc_watcher();
    w.presses.push(esc_hold(|g| {
        g.cur == 1 && g.screen == 6 && g.screens.contains(&7)
    }));
    let o = esc_run(w);
    assert_left_by_esc(&o, "画面 6（7 の手前）");
    let s = o.rig.wp.stats();
    assert_eq!((s.played, s.matches_started), (0, 0), "{s:?}");
    assert!(o.rig.wp.is_joined(), "繋がっている間の Esc のはず");
}

#[test]
fn a_held_esc_at_connect_does_not_leave_but_a_fresh_press_later_does() {
    let mut w = esc_watcher();
    w.presses.push(Press::new(|_| true, ESC, 150));
    let held = esc_run(w);
    let (wp, wg) = (&held.rig.wp, &held.rig.wg);
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "押しっぱなしで抜けた: {:?}",
        wp.stats()
    );
    assert_eq!(wg.played, held.a.played, "最後まで使い切った");
    assert_eq!(
        byes_of(&held).iter().map(|b| b.0).collect::<Vec<_>>(),
        vec![ByeReason::Finished],
        "抜けの BYE（取消）が出た"
    );
    assert_eq!(
        wg.esc_local.first(),
        Some(&1),
        "繋がる前の tick 1 の Esc はゲームへ届く"
    );
    let mut w = esc_watcher();
    w.presses.push(Press::new(|_| true, ESC, 150));
    w.presses.push(esc_hold(|g| g.tick_no >= 200));
    let again = esc_run(w);
    assert_eq!(again.rig.wp.end(), Some(WatchEnd::Quit));
    assert_eq!(
        byes_of(&again),
        vec![(ByeReason::Cancelled, 381)],
        "tick 200 から 3 秒の押し直しで抜ける"
    );
    assert_eq!(again.rig.wg.esc_local.first(), Some(&1));
}

#[test]
fn a_watcher_leaving_changes_nothing_for_the_players() {
    let without = {
        let (a, b) = esc_games();
        let w = esc_watcher();
        run_rig(a, b, w, false, &from_start, 1_000, 20_000)
    };
    let mut leaving_in_selection = esc_watcher();
    leaving_in_selection
        .presses
        .push(esc_hold(|g| g.cur == 1 && g.screen == 8 && g.tick_no > 60));
    let mut holding_in_match = esc_watcher();
    holding_in_match
        .presses
        .extend(esc_taps(|g| g.cur == 2 && g.played.len() >= 20, 30));
    let same = |s: super::Stats| super::Stats { ticks: 0, ..s };
    let steps = |np: &NetPlay| np.step_log().iter().map(|s| s.step).collect::<Vec<_>>();
    for (tag, w, end) in [
        ("選択の画面で抜ける", leaving_in_selection, WatchEnd::Quit),
        (
            "試合の中で止める",
            holding_in_match,
            WatchEnd::HostEnded(ByeReason::Finished),
        ),
    ] {
        let with = esc_run(w);
        assert_eq!(with.rig.wp.end(), Some(end), "{tag}");
        assert_eq!(with.rig.host.counts().left, 1, "{tag}");
        assert_eq!(with.a.played, without.a.played, "{tag}: A の試合の語");
        assert_eq!(with.b.played, without.b.played, "{tag}: B の試合の語");
        assert_eq!(
            same(with.npa.stats()),
            same(without.npa.stats()),
            "{tag}: A の Stats"
        );
        assert_eq!(
            same(with.npb.stats()),
            same(without.npb.stats()),
            "{tag}: B の Stats"
        );
        assert_eq!(steps(&with.npa), steps(&without.npa), "{tag}: A の押下");
        assert_eq!(with.a.screens, without.a.screens, "{tag}: A の画面の列");
        eprintln!(
            "{tag}: 対戦者の試合の語 A {}・B {}（観戦者なしと同じ）、stall A {}・B {}（観戦者なし A {}・B {}）",
            with.a.played.len(),
            with.b.played.len(),
            with.npa.stats().stalls,
            with.npb.stats().stalls,
            without.npa.stats().stalls,
            without.npb.stats().stalls
        );
    }
    assert!(!without.a.played.is_empty(), "対戦者が試合をしていない");
}

// ---------------------------------------------------------------- 台本から押した観戦者のEsc（実機で台本から検証する経路）

/// 台本のワードの検査に共通のチェック: 台本は毎tickちょうど1回進んだ（接続する前・接続している間・抜けた後のどのtickでも）。
/// つまり進めた回数がtickの数と同じ。読みが2回進めていれば多くなり、進めないtickがあれば少なくなる。
fn assert_script_stepped_once_per_tick(o: &Out, tag: &str) {
    let wg = &o.rig.wg;
    assert!(wg.tick_no > 100, "{tag}: 実行が短い {}", wg.tick_no);
    assert_eq!(
        u64::from(wg.script_steps),
        u64::from(wg.tick_no),
        "{tag}: 台本を進めた回数 / tick の数"
    );
}

#[test]
fn a_scripts_esc_in_a_match_holds_like_a_human_esc_and_the_script_steps_once_per_tick() {
    let mut w = esc_watcher();
    w.script_presses
        .extend(esc_taps(|g| g.cur == 2 && g.played.len() >= 20, 30));
    let o = esc_run(w);
    let (wp, wg) = (&o.rig.wp, &o.rig.wg);
    assert_eq!(
        wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "{:?}",
        wp.stats()
    );
    assert!(wg.esc_local.is_empty(), "マッチの中の Esc がゲームへ届いた");
    assert_eq!(wp.stats().pause.overlays, 1);
    assert_eq!(wg.played, o.a.played, "止めた後も A と同じ列");
    assert!(
        wg.presses.is_empty(),
        "人の語は使っていない（台本だけが押した）"
    );
    assert_script_stepped_once_per_tick(&o, "台本の Esc（試合の中）");
}

#[test]
fn a_scripts_esc_in_a_selection_screen_leaves_too() {
    let mut w = esc_watcher();
    w.script_presses
        .push(esc_hold(|g| g.cur == 1 && g.screen == 8 && g.tick_no > 60));
    let o = esc_run(w);
    assert_left_by_esc(&o, "台本の Esc（選択の画面 8）");
    let s = o.rig.wp.stats();
    assert_eq!((s.played, s.matches_started), (0, 0), "{s:?}");
    assert_script_stepped_once_per_tick(&o, "台本の Esc（選択の画面 8）");
}

#[test]
fn the_scripts_non_esc_words_never_reach_the_game_while_watching() {
    let base = esc_run(esc_watcher());
    assert_eq!(
        base.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished))
    );
    let mut w = esc_watcher();
    w.script_presses.push(Press::new(
        |g| g.cur == 2 && g.played.len() >= 5,
        X | Z | SHIFT,
        30,
    ));
    w.script_presses.push(Press::new(
        |g| g.cur == 1 && g.screen == 8 && g.tick_no > 60,
        X | Z | SHIFT,
        20,
    ));
    let o = esc_run(w);
    let (wg, bg) = (&o.rig.wg, &base.rig.wg);
    assert_eq!(
        o.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "Esc でない台本の語で抜けた"
    );
    assert_eq!(
        wg.fed_played, bg.fed_played,
        "試合の feed に台本の語が混ざった"
    );
    assert_eq!(wg.fed_menu, bg.fed_menu, "選択の feed に台本の語が混ざった");
    assert_eq!(wg.screens, bg.screens, "画面の列が変わった");
    assert_eq!(wg.played, bg.played);
    assert!(wg.esc_local.is_empty() && byes_of(&o).len() == 1);
    assert_eq!(
        (wg.script_nonzero_ticks, wg.script_nonzero_non_local),
        (50, 50),
        "台本の語が観戦中の tick に届いていない"
    );
    assert_script_stepped_once_per_tick(&o, "台本の Esc 以外の語");
}

#[test]
fn a_scripts_held_esc_at_connect_does_not_leave_but_a_fresh_press_later_does() {
    let mut w = esc_watcher();
    w.script_presses.push(Press::new(|_| true, ESC, 150));
    let held = esc_run(w);
    assert_eq!(
        held.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "押しっぱなしで抜けた"
    );
    assert_eq!(held.rig.wg.played, held.a.played, "最後まで使い切った");
    assert_eq!(held.rig.wg.esc_local.first(), Some(&1));
    assert_script_stepped_once_per_tick(&held, "台本の押しっぱなし");
    let mut w = esc_watcher();
    w.script_presses.push(Press::new(|_| true, ESC, 150));
    w.script_presses.push(esc_hold(|g| g.tick_no >= 200));
    let again = esc_run(w);
    assert_eq!(again.rig.wp.end(), Some(WatchEnd::Quit));
    assert_eq!(byes_of(&again), vec![(ByeReason::Cancelled, 381)]);
    assert_script_stepped_once_per_tick(&again, "台本の押し直し");
}

#[test]
fn a_watcher_leaving_by_a_scripts_esc_changes_nothing_for_the_players() {
    let without = {
        let (a, b) = esc_games();
        run_rig(a, b, esc_watcher(), false, &from_start, 1_000, 20_000)
    };
    let mut w = esc_watcher();
    w.script_presses
        .push(esc_hold(|g| g.cur == 1 && g.screen == 8 && g.tick_no > 60));
    let with = esc_run(w);
    assert_eq!(with.rig.wp.end(), Some(WatchEnd::Quit));
    assert_eq!(with.a.played, without.a.played);
    assert_eq!(with.b.played, without.b.played);
    let same = |s: super::Stats| super::Stats { ticks: 0, ..s };
    assert_eq!(same(with.npa.stats()), same(without.npa.stats()));
    assert_eq!(same(with.npb.stats()), same(without.npb.stats()));
    assert_eq!(with.a.screens, without.a.screens);
}

// 長く続く選択の画面に遅れて入った観戦者・2回目の選択（別のキャラ）を追う観戦者。
mod select_join;

#[test]
fn the_watchers_line_shows_the_hosts_ping_latency_and_rollback() {
    use higanr_core::net_menu::{LineBuf, PeerStats, WatchHudView, watch_hud_line};
    use higanr_core::wire::{STATS_PING_UNKNOWN, STATS_ROLLBACK_NONE};
    let (mut a, b, w) = long_games();
    a.hud_log = Some(Vec::new());
    let o = run_rig_inner(
        a,
        b,
        w,
        true,
        &from_start,
        &|_| false,
        (false, 0, Some(0)),
        5_000,
        60_000,
    );
    assert_follows(&o, "最初から");
    let pushed = o.rig.folded().stats;
    assert!(
        pushed.len() >= 20,
        "ホストが積んだ Stats が少ない {}",
        pushed.len()
    );
    assert!(
        pushed.iter().any(|s| s.0 != STATS_PING_UNKNOWN),
        "ping が 1 度も測れていない（値の欄が空振り）"
    );
    assert_eq!(
        o.rig.wp.stats().peer_stats as usize,
        pushed.len(),
        "受けた Stats の数がホストの積んだ数と違う"
    );
    let as_view = |&(ping, latency, rollback): &(u16, u8, u8)| {
        PeerStats::from_record(ping, latency, rollback)
    };
    let want: Vec<PeerStats> = pushed.iter().map(as_view).collect();
    let mut shown: Vec<PeerStats> = Vec::new();
    for h in o.rig.hud.iter().flatten() {
        if let Some(s) = h.stats
            && shown.last() != Some(&s)
        {
            shown.push(s);
        }
    }
    assert!(!shown.is_empty(), "観戦者の行に値が 1 度も出ない");
    let mut at = 0;
    for s in &shown {
        let Some(i) = want[at..].iter().position(|w| w == s) else {
            panic!("観戦者の行の値 {s:?} がホストの積んだ並びの後ろに無い（{at} 番目以降）");
        };
        at += i;
    }
    for h in o.rig.hud.iter().flatten() {
        let mut l = LineBuf::new();
        watch_hud_line(
            &mut l,
            higanr_core::lang::Lang::Ja,
            &WatchHudView {
                stats: h.stats,
                delay_frames: h.delay_frames,
                fast_forward: h.fast_forward,
            },
        );
        assert!(!l.as_str().contains("Rollback"), "{}", l.as_str());
        assert_eq!(
            l.as_str().starts_with("ping "),
            h.stats.is_some(),
            "{}",
            l.as_str()
        );
        if let Some(s) = h.stats {
            assert!(s.rollback.is_none() && pushed.iter().all(|p| p.2 == STATS_ROLLBACK_NONE));
        }
    }
    assert!(o.rig.hud.first().is_some_and(Option::is_none));
    assert!(o.rig.hud.last().is_some_and(Option::is_none));
}

#[test]
fn a_new_seg_forgets_the_previous_matchs_peer_stats() {
    use higanr_core::net_menu::PeerStats;
    let mut wp = WatchPlay::new(
        Arc::new(WatchSlot::new()),
        WatchConfig {
            wait_budget_us: WAIT_BUDGET_US,
            margin_frames: WATCH_MARGIN_FRAMES,
            require_net_selection: true,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    let s = PeerStats::from_record(14, 1, 2);
    wp.set_peer_stats_for_test(Some(s));
    assert_eq!(wp.peer_stats_for_test(), Some(s));
    wp.begin_seg_for_test(1);
    assert_eq!(wp.peer_stats_for_test(), None, "前の試合の値が残った");
}

#[test]
fn a_format_switch_is_not_in_the_stream_and_the_watcher_does_not_stall() {
    use higanr_core::wire::StepKind;
    let (mut a, mut b) = games();
    // 画面8に入ってから30 tick目にA、50 tick目にBがTabを押す（Bの決定は75 tick目）。
    // 要求を持っている間の押下は捨てる規則に決定が当たると、台本の1回きりの決定が消えてマッチに入らない。
    a.tab_at = vec![(8, 30)];
    b.tab_at = vec![(8, 50)];
    let w = watcher_game(a.match_frames, 2, 0);
    let o = run_rig(a, b, w, true, &from_start, 5_000, 30_000);
    let fmt_of = |np: &NetPlay| {
        np.step_log()
            .iter()
            .filter(|s| s.step.kind == StepKind::Format)
            .count()
    };
    assert_eq!(
        (fmt_of(&o.npa), fmt_of(&o.npb)),
        (2, 2),
        "形式の STEP が両者に 2 つ入っていない（Tab が空振りしている）: A {:?} B {:?}",
        o.npa.stats(),
        o.npb.stats()
    );
    assert!(
        o.rig
            .recs
            .iter()
            .all(|r| !matches!(r, Record::Step(s) if s.kind == StepKind::Format)),
        "観戦の流れに形式の STEP が積まれた"
    );
    // リングバッファの読み手（受信スレッド）が「読み取れない記録」として捨てたものも無い（ホストが形式のSTEPを積んでも読み手の守りが隠すので、リングバッファの異常の数で見る）。
    assert_eq!(
        o.rig.ring.counts().bad,
        0,
        "ホストが形式の STEP をリングバッファへ積んだ（リングバッファの読み手が捨てた）"
    );
    let host_steps: Vec<_> = o
        .npa
        .step_log()
        .iter()
        .map(|s| s.step)
        .filter(|s| s.kind != StepKind::Format)
        .collect();
    let watched: Vec<_> = o.rig.wp.step_log().iter().map(|s| s.step).collect();
    assert_eq!(watched, host_steps, "観戦者の押下の列");
    assert!(!o.a.played.is_empty());
    assert_eq!(o.rig.wg.played, o.a.played);
    assert_eq!(o.rig.wg.played, o.b.played);
    assert_eq!(
        o.rig.wp.end(),
        Some(WatchEnd::HostEnded(ByeReason::Finished)),
        "観戦者がホストの終了まで付いていけない"
    );
    assert_eq!(o.rig.wp.stats().matches_started, 2);
}

#[test]
fn the_slot_and_the_log_remember_the_family_the_first_stream_came_over() {
    struct Sink(Vec<(std::net::SocketAddr, Vec<u8>)>);
    impl higanr_core::link::Outlet for Sink {
        fn send(&mut self, to: std::net::SocketAddr, bytes: &[u8]) {
            self.0.push((to, bytes.to_vec()));
        }
    }
    for v6 in [false, true] {
        let host_addr: std::net::SocketAddr = if v6 {
            "[2001:db8::10]:47001"
        } else {
            "203.0.113.10:47001"
        }
        .parse()
        .unwrap();
        let viewer: std::net::SocketAddr = "198.51.100.7:40000".parse().unwrap();
        let slot = Arc::new(WatchSlot::new());
        slot.offer(WatcherConfig {
            session_id: SPEC_SESSION,
            secret: SPEC_SECRET,
            nonce: 0xAB,
            name: Name::new("w"),
            host: Candidates::from_addrs(&[host_addr]).unwrap(),
            host_public: spec_host_public(),
            key_seed: SPEC_WATCHER_SEED,
        });
        let (_hnet, mut wnet, _clock) = FakeNet::pair(addr(10), addr(11), 1_000);
        let mut wp = WatchPlay::new(
            Arc::clone(&slot),
            WatchConfig {
                wait_budget_us: WAIT_BUDGET_US,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: true,
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
            },
        );
        let mut host = SpectateHost::with_capacity(
            HostConfig {
                session_id: SPEC_SESSION,
                secret: SPEC_SECRET,
                key_seed: SPEC_HOST_SEED,
            },
            64,
        );
        host.push(Record::Seg { seg: 0 }).unwrap();
        let mut name = [0u8; 32];
        name[0] = b'w';
        let join = Message::SpectateJoin(wire::SpectateJoin {
            name_len: 1,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT as u8,
            nonce: 0xAB,
            name,
            build: higanr_core::update::BUILD_NUMBER,
            public: higanr_core::seal::Ephemeral::from_seed(&SPEC_WATCHER_SEED).public(),
            tree: wire::JoinTree::NONE,
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = wire::encode(
            &AuthKey::derive(&SPEC_SECRET, SPEC_SESSION),
            SPEC_SESSION,
            &join,
            &mut buf,
        );
        let mut sink = Sink(Vec::new());
        host.on_datagram(viewer, &buf[..n], 0, &mut sink);
        host.poll(0, &mut sink);
        let stream = sink
            .0
            .iter()
            .find(|(to, _)| *to == viewer)
            .expect("ホストが STREAM を出す")
            .1
            .clone();
        wnet.inject_from(host_addr, &stream);
        wp.tend(&mut wnet);
        assert!(slot.is_joined(), "v6 = {v6}: 繋がらない");
        assert_eq!(slot.via_v6(), v6, "繋がった経路の族");
        let status = crate::spectate_watch::WatchStatus::new();
        status.publish(&wp);
        let text = status.text();
        let want = if v6 { "経路 IPv6" } else { "経路 IPv4" };
        assert!(text.contains(want), "{want:?} が本ログに無い: {text}");
        assert!(text.contains("ホストの穴あけパケット"), "{text}");
    }
}

// ---------------------------------------------------------------- 受信スレッドの受け取り（ゲームのスレッドが止まっている間）

#[test]
fn a_watcher_whose_game_thread_stops_for_30_seconds_keeps_receiving_and_follows_after() {
    const MATCH_FRAMES: u32 = 2_400;
    const FROZEN: u32 = 30 * 60;
    let (mut a, mut b) = games();
    a.match_frames = MATCH_FRAMES;
    b.match_frames = MATCH_FRAMES;
    let w = watcher_game(MATCH_FRAMES, 2, 0);
    WATCHER_FREEZE.with(|c| c.set(Some((300, FROZEN))));
    let o = run_rig(a, b, w, true, &from_start, 5_000, 40_000);
    WATCHER_FREEZE.with(|c| c.set(None));
    let from = o.rig.freeze_from.expect("観戦者を止めていない");
    let to = o.rig.freeze_to.expect("観戦者が動き出していない");
    assert_eq!(
        (to.resends - from.resends, to.silenced, to.left),
        (0, 0, 0),
        "止まっている間にホストが送り直した・外した: {from:?} → {to:?}"
    );
    assert!(
        to.acks >= from.acks + u64::from(FROZEN) / 16,
        "止まっている間に ACK を返していない: {from:?} → {to:?}"
    );
    assert!(
        o.rig.wp.stats().backlog_max >= FROZEN - 60,
        "溜まっていない（止まっていない）: {:?}",
        o.rig.wp.stats()
    );
    // 動き出した後は30秒遅れのまま見るので、次の選択の画面では遅れて入った観戦者と同じく選択のワードを捨てて詰める。
    assert_follows_late(&o, "30 秒止まった観戦者");
    assert_eq!(o.rig.host.counts().resends, 0);
}

#[test]
fn the_game_thread_never_allocates_while_watching_even_when_fast_forwarding() {
    let late = |a: &MenuGame| a.played.len() >= 300;
    let (a, b, w) = long_games();
    let o = run_rig_inner(
        a,
        b,
        w,
        true,
        &late,
        &|_| false,
        (false, 0, Some(0)),
        5_000,
        40_000,
    );
    assert_follows_late(&o, "早送りあり");
    let ff = o.rig.wp.stats().ff;
    assert!(ff.displays > 20, "早送りの経路を通っていない: {ff:?}");
    assert!(o.rig.displays > 1_000);
    assert_eq!(
        (o.rig.allocs_joined, o.rig.display_allocs),
        (0, 0),
        "接続した後のゲームのスレッドの経路（tend・dispatch・早送り）で確保した"
    );
}
