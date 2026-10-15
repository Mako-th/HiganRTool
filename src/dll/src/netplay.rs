//! 入力の地点（`0x0043112D` の入口）の判断（pure。unsafe 無し）: 繋がりのセッション（`higanr_core::link`）を持ち、tick ごとに
//! 「lockstep の外か / 3 本とも 0 を feed するか / lockstep のフレームを feed するか / 描画だけのフレームにするか」を決める。
//! ゲームのメモリとゲームの関数は `Game` の向こう、ソケット・時計・待ちは `Net` の向こうに置く（本物は `net_udp::GameNet`、検査では偽物）。
//!
//! 1 tick の流れ（`NetPlay::on_input`）:
//! 1. リングを空にしてセッションへ渡し、セッションの `poll` を呼ぶ。終わりの印（接続の失敗・CHECK のズレ・相手の BYE・相手の沈黙）を見る。相手の BYE(終了) だけは、
//!    マッチの中なら届いた入力ワードを使い切り、待ちの予算が切れたときかマッチの場面を出るときにやめる（相手の最後の入力ワードは BYE より先に届く）。
//! 2. `phase` ごと:
//!    - Unconnected / Ended: `Local`（いつもの poll_all か台本）。
//!    - Selecting: 「今の場面 1 かつ次の要求 2」（マップが決まった）を見たら Holding へ。選択の画面（人対人の Match の 7・8・16）なら `menu_tick`
//!      （`net_select.rs`）: 自分の入力ワードを自分の席に、相手の入力ワードを相手の席に、合成（3 本目）へは OR で feed し、画面を移る押下はホストが順番を決めてから
//!      両者のゲームへ入れる。それ以外は `Local`。
//!      `Config::require_net_selection` でネットワークの選択を通っていない場面 2 はデモ・リプレイの再生などなので `Local`。
//!    - Holding（止める地点）: 3 本とも 0 を feed。自分の SELECT を置き、両者の SELECT が揃うまで `RenderOnly`（待ちが `peer_wait::LIMIT_MS`
//!      続いたら BYE(切断) で閉じる）。揃ったら値をゲームへ書き、LOADING へ。
//!    - Loading: 3 本とも 0 を feed して `Zero`。開始のフック（`on_match_start`）で Lockstep へ。
//!    - Lockstep: 次の要求が 3 / 10 / 11 / 12 / 13（再戦ほか）なら 0 を feed して LOADING へ。1 / 6 / 7 / 9（マッチを出る）なら Selecting へ。ただし 6
//!      （「リプレイを保存して終了」）は接続を終え（`close(Finished)`）、保存の画面を飛ばす印を頼む（`take_save_quit`）。それ以外は、取っていなければ
//!      ローカル入力を 1 回取ってすぐ送り、相手の入力ワードを予算まで待つ。揃えば 3 本を feed して `Played`、揃わなければ `RenderOnly`（次の tick で同じフレームをやり直す）。
//! 3. 相手を待つ tick（描画だけのフレーム・止める地点・選択の画面で相手が黙っている間）が続いたら画面に知らせ、Esc か 60 秒で閉じる（`peer_wait`）。
//!
//! latency の切替: ホストは `Config::lat_plan` の予定どおり、使ったフレームの数が `at` に達した後の最初の取得の前に `request_delay` する
//! （`Busy` なら次のフレームで出し直す）。`FrameTiming` の余裕は、相手の入力ワードがそれより前の入力ワードと途切れなく揃った時刻（受信スレッドが受けた時刻）から、
//! そのフレームを使うまで。seg は、最初のマッチは繋がったときの seg 0 を使い、以後、マッチが始まった seg を出るとき 1 回だけ `begin_seg(seg + 1)`。
//!
//! `on_input`・`tend`・`dispatch`・`head_wait`（`pacing_win` から毎フレーム）・`on_match_start` はゲームスレッドで走るので、
//! 確保・ブロッキング I/O・ロック待ち・パニックしうる添字と unwrap を置かない。
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::check::{
    CheckResult, CheckRule, INPUT_DIGEST_INIT, check_from_input_digest, input_digest_push,
    is_check_frame,
};
use higanr_core::code::DEFAULT_TTL_SECS;
use higanr_core::delay_pick::{self, DelayChoice, DelayPick};
use higanr_core::handshake::{self, FailReason, ModeWish, Restart};
use higanr_core::input_model::keys::ESC;
use higanr_core::input_model::{InputState, MATCH_SEND_MASK};
use higanr_core::link::{self, Connected, FormatSwitch, Link, Outlet, Recv, SelExit};
use higanr_core::liveness;
use higanr_core::lockstep::{self, Applied, CaptureError, LatError, Role, by_seat};
use higanr_core::match_gate::NoMatch;
use higanr_core::menu_model::{
    APPLY_CLEAR_BITS, DECIDE_BITS, KEY_CTRL, MAP_ITEM_RANDOM_ANY, MAP_LIST_EMPTY, Outcome,
    SCREEN_CHARA, SCREEN_DIFFICULTY, SCREEN_MAP, TRANSITION_BITS, map_select_byte, outcome_of,
};
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::net_menu::{Conn, RewindWindow};
use higanr_core::pong::PongResponder;
use higanr_core::return_flow::{PlayView, Why};
use higanr_core::seat_pick::SeatAsk;
use higanr_core::selection::{Agreed, Press};
use higanr_core::wire::{
    ByeReason, Check, FormatValue, MAX_DATAGRAM, Mode, Record, Seat, Select, Step, StepKind,
};

use crate::digit_keys::{self, DigitEdge};
use crate::net_portrait::{Followed, Locked};
use crate::net_select::{self, MenuView};
use crate::net_ui::{HudFacts, HudNames};
use crate::scripted::SCENE_MENU;
use crate::spectate_ring::SpectateRing;

pub const SCENE_MATCH: u32 = 2;
/// 場面 2 を作り直す次の要求（表 `0x004314B8`。「再戦」は 10）。
pub const REQ_REBUILD_MATCH: [u32; 5] = [3, 10, 11, 12, 13];
pub const REQ_LEAVE_MATCH: [u32; 4] = [1, 6, 7, 9];
/// マッチの終わりのメニューの「リプレイを保存して終了」が置く次の要求（`0x00435A40 movl $0x6,0x4b3690`。「キャラを変更」は `0x00435A08` の 1、
/// 「再戦」は `0x00435A24` の 10）。
pub const REQ_SAVE_AND_QUIT: u32 = 6;
const _: () = assert!(
    REQ_LEAVE_MATCH[1] == REQ_SAVE_AND_QUIT,
    "保存して終了は場面を出る要求のひとつ"
);
/// 相手の入力ワードを待つ予算（µs）。1 フレーム（16 ms）。
pub const WAIT_BUDGET_US: u32 = 16_000;
/// フレームの始まり（自分の入力を読む前）で相手の入力ワードを待つ上限（µs。ini の `[netplay] head_wait = 1` の既定。`NetPlay::head_wait`）。
/// 2 台の 60 Hz の格子の位相の差（最大 16.7 ms）の分だけ、先に進む側が毎フレーム入力の地点の後で相手の入力ワードを待っていた（実測: ゲストの
/// 「入力 → Present の前」の p50 が 8,058 / 14,104 / 7,649 µs）。始まりで待てば入力の読みがその分後ろへ寄る。入力の地点の予算と同じにしたのは、
/// 位相の差が約 13.4 ms の実機で上限 10 ms だと残りの約 3.1 ms を入力の後で待ったから。始まりで待った分は入力の地点の予算から引くので、待ちの合計の
/// 最大は `WAIT_BUDGET_US` のまま。遅延の増分: 待った tick の Present は約 0.2 ms 後ろへずれる。待たない側は 0。ウィンドウ（b-spin / b）の格子の後と、
/// 全画面でゲームがその回にフレームを始める見込みのときだけ待つ（`pacing_win::pace_head`）。
pub const HEAD_WAIT_CAP_US: u32 = WAIT_BUDGET_US;
/// 届いた押下を持ち越し続けてよいフレームの数（10 秒）。入る演出は 8〜9 tick なので、これを越えて持ち越すのは両者の画面が
/// 分かれたとき（手順の外）。越えたら BYE(中止) でやめる。
pub const MENU_HOLD_LIMIT: u32 = 600;
pub const STEP_LOG_LEN: usize = 32;
/// 自分の押下（画面を移る押下）が入らずに捨てられたとき、押し直してよい tick の数（1 秒）。キャラの画面で両者がほぼ同時に決定を押すと、
/// ホストの決定が step 1 を取り、ゲストの要求は step の食い違いで捨てられる（`selection.rs`）。ゲームも同じ tick の両席の決定を両方読む
/// （`0x00428D40` / `0x00428E0B`）ので、捨てられた押下は同じ画面のまま・ゲームがその押下を読む状態のときだけ押し直す。
/// 1 秒は、人が「入らなかった」と見て押し直すまでの間より短く、ネットワークの 1 往復より長い値として置いた（測っていない）。
pub const MENU_RETRY_TICKS: u32 = 60;
/// 接続の手順を作り直して待ち続ける長さ（ms。`Link::set_retry_window`）。接続コードの期限 `code::DEFAULT_TTL_SECS`（30 分）と同じ:
/// dll は自分のコードをネットワークを開くときに作るので、コードが使える間は待つ。
pub const HANDSHAKE_RETRY_MS: u32 = (DEFAULT_TTL_SECS * 1000) as u32;
const _: () = assert!(DEFAULT_TTL_SECS * 1000 <= u32::MAX as u64);
/// ネットワークが終わってからネットワークの組を片づけてよくなるまで（ms。`NetPlay::relink_due`）。片づけるとセッションが消えて、落ちた BYE を相手の PING（1 秒ごと）への
/// 返しで届け直せなくなるので、PING 2 回ぶんはセッションを残す（残さないと、BYE が落ちた相手は 60 秒の沈黙を待って終わる）。待つのはマッチの外だけ。
pub const RELINK_GRACE_MS: u32 = 2_000;

/// `LinkSlot` に置く 1 組: セッションの設定と、名乗る形の上書き（None なら `Config::mode_wish` のまま）と、席の希望と先にいたかの名乗り
/// （None ならセッションの設定の `host_seat` を希望として名乗り、先かどうかは名乗らない —— `SeatAsk::plain`）。
type Offered = (link::Config, Option<ModeWish>, Option<SeatAsk>);

/// 接続コードの受け渡し（ini の `code_side`。`net_code`）で、見張りのスレッドが作った繋がりのセッションの設定をゲームのスレッドへ渡す経路。
/// 繋がりのセッションには作った後に相手の候補を足す手段が無いので、始める側（offer）は返す側のコードが読めるまでセッションを作らず、受信ソケットだけを開いておく
/// （届いたものは捨てる）。返す側（answer）は始める側のコードが読めてからセッションを作る。ゲームのスレッドはファイルに触らず、ここの原子変数と
/// `Mutex::try_lock`（取れなければ次の tick）しか見ない。見張りが新しいコードを読むたびに `offer` で設定を置き換え（世代を 1 進める）、
/// ゲームのスレッドは繋がる前なら新しい世代でセッションを作り直す。繋がった後・終えた後は置き換えない。
#[derive(Debug, Default)]
pub struct LinkSlot {
    stamp: AtomicU32,
    cfg: Mutex<(u32, Option<Offered>)>,
    /// ゲームのスレッドが繋がったのを見た（見張りは `my_code.txt` を消して終える）。
    connected: AtomicBool,
    seat: AtomicU8,
    /// 繋がったときに AGREE で決まった形（0 = 未定。それ以外は `(モード + 1) << 8 | D`）。`connected` より先に書く。
    shape: AtomicU32,
    /// 繋がったときのマッチの役（0 = 未定、1 = ホスト、2 = ゲスト）。`connected` より先に書く。観戦の配る側（`spectate_offer`）を
    /// 起こすかを見張りが決めるのに使う。
    host_role: AtomicU8,
    /// HELLO で「一覧に載せてよい」を名乗るか（自分の `[spectate] enable = 1`）。見張りがセッションの設定を置く前に書き、ゲームのスレッドが
    /// セッションを作るときに読む。
    list_wish: AtomicBool,
    /// 繋がったときの `Established::may_list`（0 = 未定、1 = 両者が載せてよい、2 = どちらかが断った）。`connected` より先に書く。
    may_list: AtomicU8,
    done: AtomicBool,
    gave_up: AtomicBool,
    /// つなぐ前に決めた遅延（メニューの「対戦の遅延」か ini の `auto`。0 = 決めていない、それ以外は 1 + `DelayChoice::raw`）。
    /// 見張りがセッションの設定を置くときに書き、ゲームのスレッドが繋がった tick に `DelayPick::set_start` へ渡す（ホストの初めの値になる）。
    start: AtomicU32,
    /// 相手のコードの回線の型の印（0 = まだ読んでいない、それ以外は 0x80 | 印の 1 B）。見張りが相手のコードを読んだときに書き、
    /// ゲームのスレッドが「つながらない」の文を選ぶのに読む。
    peer_nat: AtomicU8,
}

impl LinkSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 相手のコードの印を置く（`None` は読んでいない状態に戻す）。
    pub fn set_peer_nat(&self, m: Option<NatMark>) {
        self.peer_nat
            .store(m.map_or(0, |m| 0x80 | m.to_byte()), Ordering::Release);
    }

    pub fn peer_nat(&self) -> Option<NatMark> {
        match self.peer_nat.load(Ordering::Acquire) {
            0 => None,
            v => NatMark::from_byte(v & 0x7F),
        }
    }

    pub fn set_start(&self, c: Option<DelayChoice>) {
        self.start
            .store(c.map_or(0, |c| 1 + u32::from(c.raw())), Ordering::Release);
    }

    pub fn start(&self) -> Option<DelayChoice> {
        match self.start.load(Ordering::Acquire) {
            0 => None,
            v => u8::try_from(v - 1).ok().and_then(DelayChoice::from_raw),
        }
    }

    #[cfg(test)]
    pub fn offer(&self, c: link::Config) {
        self.offer_with_mode(c, None);
    }

    pub fn offer_with_mode(&self, c: link::Config, wish: Option<ModeWish>) {
        self.offer_with_seat(c, wish, None);
    }

    /// `offer_with_mode` に、席の希望と先にいたかの名乗り `seat` を添えて置く（`net_code_game`）。
    pub fn offer_with_seat(&self, c: link::Config, wish: Option<ModeWish>, seat: Option<SeatAsk>) {
        let mut g = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        let next = g.0.wrapping_add(1).max(1);
        *g = (next, Some((c, wish, seat)));
        self.stamp.store(next, Ordering::Release);
    }

    fn take_newer(&self, seen: &mut u32) -> Option<Offered> {
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

    #[cfg(test)]
    pub fn offered_wish(&self) -> Option<Option<ModeWish>> {
        let g = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        g.1.map(|(_, w, _)| w)
    }

    #[cfg(test)]
    pub fn offered_seat(&self) -> Option<Option<SeatAsk>> {
        let g = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        g.1.map(|(_, _, s)| s)
    }

    pub fn generation(&self) -> u32 {
        self.stamp.load(Ordering::Acquire)
    }

    /// 繋がったことと自分の席と決まった形（`mode`・`delay`）を見張りへ知らせる（繋がった tick に 1 回）。先に書いてから `connected` を
    /// Release で書くので、見張りは `is_connected` を見た後の値を必ず読める。
    pub fn mark_connected(&self, seat: Seat, mode: Mode, delay: u8) {
        self.seat.store(
            match seat {
                Seat::P1 => 1,
                Seat::P2 => 2,
            },
            Ordering::Relaxed,
        );
        let m = match mode {
            Mode::Delay => 1,
            Mode::Rollback => 2,
        };
        self.shape
            .store((m << 8) | u32::from(delay), Ordering::Relaxed);
        self.connected.store(true, Ordering::Release);
    }

    pub fn mark_role(&self, role: Role) {
        self.host_role.store(
            match role {
                Role::Host => 1,
                Role::Guest => 2,
            },
            Ordering::Relaxed,
        );
    }

    /// 自分の「一覧に載せてよい」を置く（見張りが `offer_with_seat` の前に）。`offer_with_seat` の Release の後にゲームのスレッドが読む。
    pub fn set_list_wish(&self, on: bool) {
        self.list_wish.store(on, Ordering::Relaxed);
    }

    pub fn list_wish(&self) -> bool {
        self.list_wish.load(Ordering::Relaxed)
    }

    /// 両者が一覧に載せてよいと名乗ったか（繋がった tick に 1 回。`mark_connected` より前に呼ぶ）。
    pub fn mark_may_list(&self, on: bool) {
        self.may_list
            .store(if on { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// 繋がった後の `Established::may_list`。繋がる前は None。
    pub fn may_list(&self) -> Option<bool> {
        match self.may_list.load(Ordering::Relaxed) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn is_host(&self) -> Option<bool> {
        match self.host_role.load(Ordering::Relaxed) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    pub fn seat(&self) -> Option<Seat> {
        match self.seat.load(Ordering::Relaxed) {
            1 => Some(Seat::P1),
            2 => Some(Seat::P2),
            _ => None,
        }
    }

    pub fn shape(&self) -> Option<(Mode, u8)> {
        let v = self.shape.load(Ordering::Relaxed);
        let mode = match v >> 8 {
            1 => Mode::Delay,
            2 => Mode::Rollback,
            _ => return None,
        };
        Some((mode, (v & 0xFF) as u8))
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

/// ゲームスレッドのネットワークのインターフェース。本物は `net_udp::GameNet`。どれも確保しない・パニックしない。
pub trait Net: Outlet {
    fn now_us(&mut self) -> u64;
    /// 届いている 1 通を `buf` へコピーして取り出す（送り元・長さ・受けた時刻）。受けた時刻は受信スレッドが受けた時の `now_us` と同じ時計の µs（余裕の起点）。
    fn recv(&mut self, buf: &mut [u8; MAX_DATAGRAM]) -> Option<(SocketAddr, usize, u64)>;
    fn wait(&mut self, us: u32) -> bool;
    /// `recv` と同じ。4 つ目は、受信スレッドがこの 1 通（PING）にその場で PONG を返したか。偽物はいつも false。
    fn recv_marked(
        &mut self,
        buf: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize, u64, bool)> {
        self.recv(buf).map(|(a, n, t)| (a, n, t, false))
    }
    /// 受信スレッドに PONG の口を置く。置けたら（か、置く先が無ければ）true。待たない（錠が取れなければ false）。
    fn pong_install(&mut self, _r: &PongResponder) -> bool {
        true
    }
    fn pong_enable(&mut self, _on: bool) {}
    /// ゲームスレッドの心拍（`now_us` と同じ時計の ms。tick ごとに 1 回）。本物は原子変数へ 1 回書くだけで、受信スレッドはこれが古いと PONG を返さない。
    fn heartbeat(&mut self, _now_ms: u32) {}
    /// 接続の手順が成り立った（繋がったのを最初に見た tick に 1 回）。本物は、待っている間だけ続ける STUN の聞き直しを止める。
    fn link_up(&mut self) {}
    /// 自分の回線が宛先ごとに外側のポートを変える型か（2 か所目の STUN の比べ）。比べられていなければ `None`。左上の「つながらない」の
    /// 2 行目の選び（`net_ui::net_input`）が、作り直しが始まった後にだけ読む。
    fn own_port_varies(&mut self) -> Option<bool> {
        None
    }
    /// 自分の回線の型の印（宛先ごとに外側のポートが変わる型かは 2 か所目の STUN の今の結果、UPnP は割り当てられているか）。
    /// 「つながらない」の文の選び（`net_ui::net_input`）が、作り直しが始まった後か相手のコードを読んだ後にだけ読む。
    fn own_nat(&mut self) -> NatMark {
        NatMark {
            symmetry: Symmetry::from_depends(self.own_port_varies()),
            upnp: false,
        }
    }
}

pub trait Game {
    /// （今の場面 `+0x58C`, 次の要求 `+0x590`）。
    fn scenes(&mut self) -> (u32, u32);
    /// このフレームの自分の入力ワード。本物は、台本があれば台本の入力ワード、無ければ素の poll `0x0042B850` を席 0 で呼んだ入力ワードの下位 16 ビット
    /// （InputManager[0] は呼ぶ前後でコピーして戻す）。poll の入口の照合が合わなかった実行は `0x0042BE40`。
    fn local_input(&mut self, seat: Seat) -> u16;
    /// 画面 6（ゲームが `[2]` だけを読む —— `0x0042A81D` の決定・`0x0042457A` の上下）でメニューが握るときの 3 本の入力ワード（index 0・1・2）。
    /// 本物は素の poll を席 0・1・2 で呼んだ入力ワード。台本が動いている・poll を呼べない実行は `local_input` の 1 ワードを 1P と合成に（2P は 0）。1 tick に 1 回だけ呼ぶ。
    fn menu_words(&mut self, seat: Seat) -> [u16; 3] {
        let w = self.local_input(seat);
        [w, 0, w]
    }
    /// 観戦者（Esc で観戦をやめる）: 自分の Esc の読み。1 tick に 1 回だけ呼ぶ。本物は `local_input` と同じ読みに台本の入力ワードの ESC ビットだけを足して返し、
    /// 台本を 1 tick 進める。`Local` を返した tick の feed は、ここで進めた結果を使う（台本が二重に進まない。`net_game::NetGame::peeked`）。
    fn peek_local_input(&mut self) -> u16 {
        self.local_input(Seat::P1)
    }
    /// feed `0x0042ACE0`（index 0 = 1P、1 = 2P、2 = 合成）。
    fn feed(&mut self, index: u32, word: u16);
    /// InputManager `index` の今の状態（feed の模型 `InputState` の形）。`ui_entry::leaks` が使う。偽物は空の状態。
    fn input_state(&mut self, _index: usize) -> InputState {
        InputState::new()
    }
    /// ゲーム内のメニューが読んだ入力ワードを、同じ tick の後の `local_input` へ預ける（1 tick に台本・デバイスを 2 回読まないため）。
    fn stash_local(&mut self, _word: u16) {}
    /// ルーム名の入力に入った tick の終わりに `true`、出た tick の終わりに `false` で呼ぶ: ウィンドウから打たれた文字を貯める口の開け閉め。
    /// 開けている間はウィンドウの手続きが `WM_CHAR` をゲームへ渡さず、IME もウィンドウから切り離す。開けられなければ false。
    fn text_gate(&mut self, _open: bool) -> bool {
        true
    }
    /// Ctrl+数字・Ctrl+Tab のキーの今の押下の集合（`digit_keys` の bit の並び: 数字 `d` = bit `d`・Tab・Alt/Win・Ctrl）。
    /// 読んではいけない間（ウィンドウが前面にない・文字の入力中・読めない）は `None`。1 tick に 1 回だけ呼ぶ。
    fn digit_keys(&mut self) -> Option<u16> {
        None
    }
    /// 台本の段階（`digit=x` / `tab`）が、この tick の「押した瞬間」の集合を差し替える（`DigitEdge` を通さない）。段階が効き始めた次の tick に 1 回だけ `Some`。
    fn digit_script(&mut self) -> Option<u16> {
        None
    }
    /// 台本の段階（`copy=1`）が、この tick に Ctrl+C を押したことにするか。段階が効き始めた次の tick に 1 回だけ `true`（台本の実行は本物のキーを読まない）。
    /// `digit_script` と同じ段階から取り出すので、1 つの段に `digit=` と `copy=` を一緒に書かない。
    fn copy_script(&mut self) -> bool {
        false
    }
    /// この実行に台本が付いているか。付いている実行は、段階の無い tick に本物の数字キー・Tab を読まない。台本が終わった後も `true`。
    fn scripted_run(&self) -> bool {
        false
    }
    fn typed(&mut self, _out: &mut [u16]) -> usize {
        0
    }
    /// 止める地点で自分の SELECT を作る（自分の席のキャラ・色・初期ライフ・チャージ型、マップ・難易度。ホストならシードと CPU レベル。
    /// マップ −2 の置き換えもここ）。
    fn own_select(&mut self, seat: Seat, role: Role) -> Select;
    /// 揃った値を書く（相手の席のキャラ・色・初期ライフ、合意したマップ・難易度、シード）。場面の切替の前。
    /// 書く前に値の範囲を見て、範囲の外があれば 1 つも書かずに `Err`（どの欄か）。
    fn apply_agreed(&mut self, peer_seat: Seat, agreed: &Agreed) -> Result<(), BadValue>;
    /// 観戦者: 止める地点で、両席の SELECT を自分のゲームへ書く（`apply_agreed` は相手の席だけ）。書く前に範囲を見て、外があれば 1 つも書かずに `Err`。
    fn apply_watched(&mut self, p1: &Select, p2: &Select) -> Result<(), BadValue> {
        self.apply_agreed(Seat::P1, &watched_agreed(p1))?;
        self.apply_agreed(Seat::P2, &watched_agreed(p2))
    }
    /// コピー `[0x4A7E78]` の `+0xB4` / `+0xB5` へ両者のチャージ型（開始のフック `0x0041AB2B` の中）。書けなければ false。
    fn write_charge(&mut self, p1: u8, p2: u8) -> bool;
    /// CHECK の値（乱数の状態の下 16 bit・抽選回数・1P のライフ・2P のライフ）。
    fn check_values(&mut self) -> (u16, u32, u8, u8);
    /// 選択の画面の読み。今の場面が 1 で次の要求が 2 でない tick にだけ呼ぶ（マップの決定の後は MainMenu が片付けられうる）。読めなければ None。
    fn menu_view(&mut self) -> Option<MenuView> {
        None
    }
    /// MainMenu の欄（`[0x4AC8C0]` からのオフセット）へ u32 を書く（落ちない書き方。書けなければ false）。
    fn write_menu_u32(&mut self, _offset: u32, _value: u32) -> bool {
        false
    }
    /// マップの一覧 `0x4AC8C4`（止める地点で、ホストがマップ −2 を置き換えるときに読む）。
    fn map_list(&mut self) -> [u8; 16] {
        [MAP_LIST_EMPTY; 16]
    }
    /// キャラ選択で席 `seat` のカーソルを `cursor` に書いた直後に、立ち絵と席の目印を追わせる（ゲームの移動の経路が呼ぶ 3 本の関数。
    /// `net_portrait`）。`v` は書く前に読んだ画面。関数を持たない偽物はいつも `Refused`。
    fn follow_portrait(
        &mut self,
        _v: &MenuView,
        _seat: usize,
        _cursor: u32,
        _locked: Locked,
    ) -> Followed {
        Followed::Refused
    }
    /// 選択の画面のカーソルの音を 1 回積む（`0x43E2F0(0x4DC698, 0xC, 0)`。ゲームのカーソルの移動 `0x0042474A` と同じ呼び出し）。
    /// 積めなければ（関数の頭のバイト列が合わない・偽物）false。
    fn cursor_se(&mut self) -> bool {
        false
    }
    /// 試合の場面の読み込み・一時停止・終わりのメニューの状態（対戦が終わった後の戻り道だけが読む）。読めなければ None。
    fn play_view(&mut self) -> Option<PlayView> {
        None
    }
}

/// 観戦の記録の SELECT 1 席ぶんを、`Game::apply_agreed` が読む形にする。観戦者はホストの値をそのまま書くので、食い違いの印は空。
pub(crate) fn watched_agreed(sel: &Select) -> Agreed {
    Agreed {
        peer: *sel,
        map: sel.map,
        difficulty: sel.difficulty,
        cpu_1p: sel.cpu_1p,
        cpu_2p: sel.cpu_2p,
        background: sel.background,
        rng_seed: sel.rng_seed,
        mismatch: higanr_core::selection::Mismatch::default(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorWrite {
    Miss,
    /// 書いた。キャラ選択なら、続けて立ち絵を追わせた結果（`Game::follow_portrait`）。
    Wrote(Option<Followed>),
}

/// 押下を入れる tick の計画（`net_select::apply_plan` / `align_plan`）のカーソルをゲームへ書き、キャラ選択なら同じ tick のうちに立ち絵を追わせる。
/// 計画にカーソルが無ければ `None`。対戦の `NetPlay` と観戦者（`spectate_watch.rs`）が同じ関数を使う。
pub(crate) fn write_plan_cursors<G: Game>(
    game: &mut G,
    v: &MenuView,
    plan: &net_select::ApplyPlan,
) -> Option<CursorWrite> {
    let (off, val) = plan.cursor?;
    Some(write_cursor(
        game,
        v,
        off,
        val,
        plan.portrait,
        Locked::Refuse,
    ))
}

/// MainMenu の欄 `off` へカーソル `val` を書き、`portrait` があれば同じ tick のうちに立ち絵を追わせる。
fn write_cursor<G: Game>(
    game: &mut G,
    v: &MenuView,
    off: u32,
    val: u32,
    portrait: Option<(usize, u32)>,
    locked: Locked,
) -> CursorWrite {
    if !game.write_menu_u32(off, val) {
        return CursorWrite::Miss;
    }
    CursorWrite::Wrote(portrait.map(|(seat, cur)| game.follow_portrait(v, seat, cur, locked)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadValue {
    pub field: &'static str,
    pub value: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSource {
    Game,
    /// 使った (1P, 2P) の列のダイジェスト（peer_sim と。検査用）。
    InputDigest,
}

pub const LAT_PLAN_MAX: usize = 16;

/// 観戦の流れへ `Record::Stats`（観戦者の左下の行の前半）を積む間隔（`hud_facts` の呼びの数。約 1 秒）。「戻し」の期間（`net_menu::REWIND_WINDOW`）と同じ長さ。
const STATS_EVERY: u32 = higanr_core::net_menu::REWIND_WINDOW as u32;

/// latency の切替の予定（ホストのとき。検査用）: 「この接続で使った lockstep のフレームの数が `at` に達したら、次の取得の前に delay を
/// `delay` に」を `at` の小さい順に。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatPlan {
    items: [(u64, u8); LAT_PLAN_MAX],
    len: usize,
}

impl LatPlan {
    /// `at` が狭義に増えていて、delay が 0〜9、長さが `LAT_PLAN_MAX` 以下のときだけ作る。
    pub fn new(items: &[(u64, u8)]) -> Result<Self, String> {
        if items.len() > LAT_PLAN_MAX {
            return Err(format!(
                "latency の切替の予定は {LAT_PLAN_MAX} 個まで（{} 個）",
                items.len()
            ));
        }
        let mut plan = LatPlan::default();
        let mut prev: Option<u64> = None;
        for (i, &(at, delay)) in items.iter().enumerate() {
            if delay > higanr_core::frame::MAX_DELAY {
                return Err(format!(
                    "latency の切替の delay {delay} は 0〜{}",
                    higanr_core::frame::MAX_DELAY
                ));
            }
            if prev.is_some_and(|p| at <= p) {
                return Err(format!(
                    "latency の切替のフレームの数 {at} が前の値以下（小さい順に書く）"
                ));
            }
            prev = Some(at);
            if let Some(slot) = plan.items.get_mut(i) {
                *slot = (at, delay);
            }
        }
        plan.len = items.len();
        Ok(plan)
    }

    pub fn get(&self, i: usize) -> Option<(u64, u8)> {
        if i < self.len {
            self.items.get(i).copied()
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub check: CheckSource,
    pub wait_budget_us: u32,
    pub lat_plan: LatPlan,
    /// 止める地点へは、ネットワークの選択でマップの決定が両者に入ったときだけ入る。選択を通らずに場面 2 へ移るのはネットワークのマッチではない（デモ・リプレイの再生・
    /// ストーリーなど）ので、ネットワークは何もせず `Local` のまま流す。本物のゲームは true（台本なしの実機で、タイトルで約 27 秒待つとデモが始まり、
    /// ネットワークが相手の SELECT を待ち続けてゲームが止まったことがある）。
    /// false は v0 の形（選択の画面を持たない偽物と peer_sim の v0 の選択）: 選択を通らずに来ても止める地点で選択の状態機械を動かす。
    /// 既定が false なのは、`..Config::default()` で作る検査が v0 の形でしかマッチに入れないから。
    /// 繋がる前の選択の画面で決定を落とす経路（`unconnected_tick`）も、この欄が true のときだけ動く（false は、ゲームのメモリの無いプロセスで場面のアドレスを読むと落ちるため、
    /// 繋がる前にゲームの場面を読まない）。
    pub require_net_selection: bool,
    /// HELLO で名乗る望むモードと D（`net_config::NetSettings::mode_wish`）と、rollback に決まったときの R。セッションを作るたびに `Link::set_mode` へ渡す。
    /// 既定は delay 方式。本物の実行で rollback を名乗るのは、マッチの経路の入口（`rollback_play_win::arm`）が全部書けたときだけ（`NetSettings::mode_wish_if`）。
    pub mode_wish: ModeWish,
    pub rollback_ahead: u8,
    /// rollback が「できる」か（HELLO の +3 の bit1 で名乗る）。望み（`mode_wish`）とは別: 望まなくても、確保の向け先などの入口を全部書けていれば
    /// （`rollback_play_win::armed`）できる。両者ができる組だけ、キャラセレクトの Tab で形式を替えられる。本物の設定は `script_win::play_config` が置く。
    pub rollback_able: bool,
    /// 繋がった後、両者の READY（type 20）が揃うまで対戦を成立させない（`higanr_core::match_gate`。`code_side = game` の実行だけ true）。
    /// 成立の前は `Unconnected` のまま、ネットワークは入力に触らない（`Local`）。false は、繋がったときに `Selecting` へ移る。
    pub match_gate: bool,
    /// 遅れの量の選び（`higanr_core::delay_pick`）: 繋がったら `DelayPick` を持ち、delay 方式では最初のマッチの始まりでホストが推奨値へ切り替える。
    /// rollback のモードでは推奨値で置き換えない。数字キーで選んだ値は両者ともマッチのうちに効く（`NetPlay::digit_tick`）。
    /// `code_side = game` の実行で `lat_plan` が空のときだけ true。
    pub delay_pick: bool,
    /// フレームの始まりで相手の入力ワードを待つ上限（µs。0 なら待たない。ini の `head_wait`。`HEAD_WAIT_CAP_US`）。delay 方式の lockstep のときだけ効く。
    pub head_wait_cap_us: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            check: CheckSource::Game,
            wait_budget_us: WAIT_BUDGET_US,
            lat_plan: LatPlan::default(),
            require_net_selection: false,
            mode_wish: ModeWish::DELAY,
            rollback_ahead: higanr_core::rollback::DEFAULT_AHEAD,
            rollback_able: true,
            match_gate: false,
            delay_pick: false,
            head_wait_cap_us: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRec {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
}

/// 使ったフレーム 1 つの時間（ログ用）。`FrameRec` と別にするのは、`FrameRec` を peer_sim の記録とそのまま比べるため。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTiming {
    pub delay: u8,
    /// 余裕（µs）: 相手の入力ワードが前の入力ワードと途切れなく揃ってから、このフレームを使うまで。開始直後の穴埋めと、揃ったのを見ていない入力ワードは None。
    pub margin_us: Option<u32>,
    pub wait_us: u32,
    /// このフレームを使った tick の始まりで、相手の入力ワードを待った時間（µs。`NetPlay::head_wait`）。始まりで待った分は `wait_us` に入らない。
    pub head_wait_us: u32,
}

/// 待ちが時刻で戻ったときの遅れの目盛り（µs の上限。最後の枠はそれ以上）。
pub const WAIT_OVER_BOUNDS_US: [u32; 4] = [250, 500, 1_000, 2_000];

/// 相手の入力ワードの揃った時刻を覚えておく数。lockstep の先読みは delay（最大 9）と 1 通の入力ワードの数（最大 32）なので、64 あれば使う前に上書きされない。
const ARRIVAL_SLOTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arrival {
    seg: u16,
    frame: u32,
    t_us: u64,
}

const NO_ARRIVAL: Arrival = Arrival {
    seg: 0,
    frame: u32::MAX,
    t_us: 0,
};

/// 入力の地点の判断。`Local` = 元の飛び先へ、`Zero` / `Played` = poll_all を呼ばずに戻る、`RenderOnly` = 戻りアドレスを `0x00431259` へ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Local,
    Zero,
    Played(FrameRec),
    RenderOnly,
    Menu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadWait {
    /// 待たない形（始まりの待ちを切った・delay 方式の lockstep の外・rollback・観戦者・遅れ 0・直前の tick が描画だけのフレーム・
    /// 相手がマッチを終えた・この tick で待った後）。時計にもセッションにも触らない。
    Off,
    Ready,
    Arrived(u32),
    /// 上限に当たって戻った（待った時間 µs）。入力ワードはまだ無く、入力の地点の待ちに任せる。
    Capped(u32),
    /// 待つ間にネットワークが終わった・相手がマッチを終えた（待った時間 µs）。後の処理は入力の地点（`tend`）がする。
    Ended(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Unconnected,
    Selecting,
    Holding,
    Loading,
    Lockstep,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    HandshakeFailed(handshake::FailReason),
    PeerBye(ByeReason),
    Dead,
    /// CHECK が合わなかった（BYE(ズレ検出) を出す）。
    Desync {
        local: Check,
        remote: Check,
    },
    Closed(ByeReason),
    /// 相手の SELECT の値が範囲の外だった（1 つも書かずに BYE(中止) を出す）。
    BadSelect(BadValue),
    /// 届いた押下を `MENU_HOLD_LIMIT` フレームより長く入れられなかった（両者の画面が分かれた。BYE(中止) を出す）。
    MenuStuck(Step),
    /// 繋がったが、対戦が成立しないまま閉じた（`Config::match_gate`。理由は `NoMatch`）。成立の後の BYE(取消) は `PeerBye`。
    Unmatched(NoMatch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub ticks: u64,
    pub played: u64,
    pub zero_ticks: u64,
    /// lockstep の中の描画だけのフレーム（相手の入力ワードが予算までに揃わなかった）。
    pub stalls: u64,
    pub max_stall_run: u32,
    pub hold_frames: u64,
    pub local_reads: u64,
    pub segs_started: u32,
    pub wakes: u64,
    pub charge_write_failures: u32,
    pub menu_frames: u64,
    pub menu_applied: u32,
    pub menu_held: u64,
    /// 決定を入れる tick にカーソルを合わせられなかった（MainMenu に書けない・マップの一覧に無い値）。
    pub menu_align_miss: u32,
    pub menu_lost: u32,
    pub menu_retries: u32,
    /// キャラ選択の決定でカーソルを書いたとき、立ち絵と席の目印の 3 本を呼んだ回数と、書いたカーソルが前と違うのに呼ばなかった回数
    /// （関数の署名が合わない・手元で未解放・MainMenu の部品が無いなど。`net_portrait`）。
    pub menu_portrait_called: u32,
    pub menu_portrait_refused: u32,
    /// キャラ選択で相手の席のカーソルを相手の位置へ書いた回数と、そのうちカーソルの音を積んだ・積めなかった（関数の頭が合わない）回数。
    pub menu_peer_writes: u32,
    pub menu_cursor_se: u32,
    pub menu_cursor_se_failed: u32,
    /// 相手の入力ワードに「ランダム」（Ctrl）が立っていた選択の tick の数（キャラ選択では相手の席へ入れずに落とす）。
    pub menu_peer_random: u32,
    /// 繋がる前に選択の画面（7・8・16）で、ゲームへ入れずに落とした決定の押下（押した瞬間）の回数（`unconnected_tick`）。
    pub menu_pre_link_decides: u32,
    pub lat_requested: u32,
    pub lat_busy: u32,
    pub lat_refused: u32,
    pub switches: u32,
    /// 初めの値（マッチの始まりの推奨値・つなぐ前に決めた遅延）をネットワークの側へ渡した回数と、そのうち今の値と同じで何もしなかった数。
    pub delay_presses: u32,
    pub delay_presses_unchanged: u32,
    /// 数字キーの押下をネットワークの側へ渡した回数と、そのうち今の値・旗と同じで何もしなかった数。
    pub digit_presses: u32,
    pub digit_unchanged: u32,
    /// Tab の押下（キャラセレクトでなくても数える）・そのうち効かなかった数（画面 8 でない・両者が rollback ができる組でない・ほかの押下と
    /// 同じ tick）・形式の切替を求めた数（ホストは自分の押下、ゲストは要求を作った回数。押し直しは数えない）。
    pub tab_presses: u32,
    pub tab_ignored: u32,
    pub format_asked: u32,
    /// 入れた形式の切替の STEP の数（両役。`Connected::switch_format` が通った回）・断った数（手順の外）・value が読めなかった数。
    pub format_steps: u32,
    pub format_refused: u32,
    pub format_bad: u32,
    pub margin_unknown: u64,
    /// 待ちが時刻で戻った回数と、その遅れの目盛り（`WAIT_OVER_BOUNDS_US` の枠ごと）・最大（µs）。
    pub wait_timeouts: u64,
    pub wait_over_hist: [u64; 5],
    pub wait_over_max_us: u32,
    /// 始まりで待った回数（待たずに済んだ tick は数えない）・待った時間の合計（µs）・上限 `Config::head_wait_cap_us` に当たって戻った回数。
    pub head_waits: u64,
    pub head_wait_us_total: u64,
    pub head_wait_caps: u64,
    pub hs_restarts: u32,
    pub hs_restart_last: Option<(FailReason, Restart)>,
    /// ネットワークの選択の合意なしに場面 2 へ移るのを見て、止める地点へ入らずに `Local` で流した tick（タイトルのデモ・リプレイの再生など）。
    pub local_scene2: u32,
    /// 食い違い（`PlayState` の止まり・状態機械の内部の食い違い・フックが外れた `lockstep_tick`）でマッチをやめた回数。
    pub rb_aborts: u32,
    pub stop_ticks: u64,
    pub stop_played: u64,
    /// 相手を待つ知らせの間に Esc で閉じた回数・待ちが `peer_wait::LIMIT_MS` 続いて閉じた回数（止める地点の待ちも含む）。
    pub peer_wait_esc: u32,
    pub peer_wait_cuts: u32,
}

/// マッチの最中の同期ズレ（CHECK の食い違い）で、そのマッチを止めている状態。ゲームは決定的なので食い違いは 1 回で本物のズレ。ネットワークを閉じても
/// マッチを動かし続けず、描画だけのフレームで画面を保ち、利用者に知らせて（`Conn::Desynced`）、Esc を待つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// 止めていない。
    None,
    /// 止めている。`esc_armed` は Esc が押されていないのを 1 度見たか（押したまま止まった場合に、止まった瞬間に戻らないため）。
    Held { esc_armed: bool },
    /// Esc で戻った（以後は `Local`）。
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepRec {
    pub step: Step,
    pub own: bool,
}

/// 入った押下の記録（固定のリングバッファ。ゲームのスレッドで確保しない）。
#[derive(Debug, Clone, Copy)]
pub struct StepLog {
    recs: [Option<StepRec>; STEP_LOG_LEN],
    pub count: u32,
}

impl StepLog {
    pub(crate) const fn new() -> Self {
        StepLog {
            recs: [None; STEP_LOG_LEN],
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, r: StepRec) {
        if let Some(slot) = self.recs.get_mut(self.count as usize % STEP_LOG_LEN) {
            *slot = Some(r);
        }
        self.count = self.count.saturating_add(1);
    }

    pub fn iter(&self) -> impl Iterator<Item = StepRec> + '_ {
        let n = self.count as usize;
        let first = n.saturating_sub(STEP_LOG_LEN);
        (first..n).filter_map(move |i| self.recs.get(i % STEP_LOG_LEN).copied().flatten())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Decisions {
    difficulty: Option<u8>,
    map_item: Option<u8>,
}

/// 入力の地点の判断の本体（接続ごとに 1 つ。ゲームスレッドだけが持つ）。
pub struct NetPlay {
    link: Link,
    /// 今のセッションを作った時刻と、繋がったのを最初に見た時刻（どちらも `Net::now_us` の ms）。セッションを作り直したら前者を置き直す。
    link_started_ms: u32,
    connected_ms: Option<u32>,
    /// ネットワークが終わったのを手入れ（`tend`）が最初に見た時刻（ms）。つなぎ直しの片づけの待ち（`RELINK_GRACE_MS`）はここから数える。
    ended_seen_ms: Option<u32>,
    cfg: Config,
    phase: Phase,
    end: Option<End>,
    seg_started: bool,
    sel_entered: bool,
    sel_left: bool,
    own_charge: u8,
    peer_charge: u8,
    held_local: Option<u16>,
    peer_finished: bool,
    /// lockstep の中で「保存して終了」（今 2・次 `REQ_SAVE_AND_QUIT`）を見て閉じた。呼び手が同じ tick に `take_save_quit` で 1 回だけ取る。
    save_quit: bool,
    /// 「保存して終了」で試合の場面を出た（`save_quit` と違い取られても残る。戻り道を始めないための印）。
    left_by_save_quit: bool,
    /// 成立した後に終わり、まだ `take_lost` が取っていない。
    lost_pending: bool,
    digest: u64,
    stall_run: u32,
    stats: Stats,
    menu_net: bool,
    menu_prev: u16,
    pre_link_decide_held: bool,
    decisions: Decisions,
    map_decided: bool,
    menu_hold_run: u32,
    menu_retry: Option<(Press, u32)>,
    steps: StepLog,
    own_select: Option<Select>,
    agreed: Option<Agreed>,
    lat_next: usize,
    arrivals: [Arrival; ARRIVAL_SLOTS],
    timing: FrameTiming,
    /// 最後に始まりで待った tick（`Stats::ticks`）。同じ tick の 2 回目以降の始まりでは待たない（ゲーム自身の時計の経路は、1 フレームの間に始まりを何度も通る）。
    head_wait_tick: u64,
    /// 始まりで待った時間（µs）。次の `dispatch` が `tick_head_wait_us` へ移す（始まりは `tend` の前に走るので、いまの tick のものとして渡す）。
    head_wait_pending_us: u32,
    tick_head_wait_us: u32,
    code: Option<Arc<LinkSlot>>,
    code_state: CodeState,
    pong_installed: bool,
    pong_enabled: bool,
    /// 観戦のリングバッファ（ini の `[spectate] enable = 1` のときだけ）。ゲームのスレッドは記録をリングバッファへ書くだけで、送りは受信スレッドが行う。
    spec: Option<Arc<SpectateRing>>,
    /// 観戦のリングバッファを、マッチのホストのときだけ使う形（ゲーム内のメニューの経路。`set_spectate_for_host`）。繋がった tick で役がゲストならリングバッファを
    /// `spec_parked` へ移して書かない（ゲストは配らない）。ini の `[spectate]` はどちらの役でも書く。
    spec_host_only: bool,
    /// 外したリングバッファの置き場（リングバッファの持ち主は受信スレッドの送り手と共有で、`NetPlay` の手放しまで解放しない。ゲームのスレッドで大きい配列を解放しないため）。
    spec_parked: Option<Arc<SpectateRing>>,
    /// seg を出た回数（`leave_seg` が rollback の状態機械の `begin_seg` と同時に数える）と、`PlayState` が見終えた回数。差があれば
    /// 次の入口がコピーとアドレスの表を捨てる。
    rb_epoch: u32,
    rb_seen: u32,
    /// この `NetPlay` で `PlayState::begin_conn` を済ませたか（`PlayState` は静的で接続を跨ぐのに `rb_epoch` は接続ごとに 0 から数えるので、
    /// epoch の差では新しい接続を見分けられない）。
    rb_bound: bool,
    /// 最後に `pacing::glide()` へ渡した格子の寄せ幅（ppm。検査用: 置き場がスレッドごとなので、偽のゲームのスレッドで書いた値は検査のスレッドから読めない）。
    rb_glide_last: i32,
    rb_abort_first: Option<(RbAbort, u64)>,
    stop: Stop,
    ui_resim_seen: u32,
    stats_tick: u32,
    stats_rewind: RewindWindow,
    pick: Option<DelayPick>,
    digits: DigitEdge,
    tab_edge: bool,
    /// 選択の局面で最後に見た選択の画面の番号（`menu_tick` が書く。選択の外の tick は 0）。キャラセレクト（画面 8）とマップ（画面 16）の間だけ
    /// 左下の行と名前の帯を出すための印（`hud_facts`。画面 6・7 は出さない）。
    hud_screen: u8,
    /// 止める地点で相手の SELECT を待ち続けているフレームの数。
    hold_run: u32,
    wait: PeerWait,
    /// 最後に `peer_wait_tick` が見た tick（`Stats::ticks`）。
    wait_tick: u64,
    #[cfg(test)]
    sent_checks: Vec<Check>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodeState {
    pub seen_gen: u32,
    pub links: u32,
    /// 今のセッションを作った設定の session。つながった後に片づけたら「使った」コードとして記録し、次のつながりで同じ session のコードを貼られても読まない。
    pub session: u32,
    pub dropped_before_link: u64,
    notified_connected: bool,
    notified_done: bool,
}

fn ms(us: u64) -> u32 {
    (us / 1_000) as u32
}

fn other_mode(m: Mode) -> Mode {
    match m {
        Mode::Delay => Mode::Rollback,
        Mode::Rollback => Mode::Delay,
    }
}

fn value_mode(v: u8) -> Mode {
    if v & 1 != 0 {
        Mode::Rollback
    } else {
        Mode::Delay
    }
}

impl NetPlay {
    /// 繋ぎ始める（`Link::new` を `net` の時計で作る）。CHECK をゲームの値で作るなら、frame 0 の抽選回数は比べない（`CheckRule`）。
    pub fn new<N: Net>(link_cfg: link::Config, cfg: Config, net: &mut N) -> Self {
        let now = ms(net.now_us());
        Self::with_link(Self::build_link(link_cfg, &cfg, None, None, now), cfg, now)
    }

    /// 接続コードの受け渡しの形（`LinkSlot`）: 見張りのスレッドが設定を置くまでセッションを作らず、届いたものは捨てる。
    /// 設定が置かれた次の tick の始まりでセッションを作り、すぐ HELLO を送り始める。
    pub fn with_code_slot<N: Net>(slot: Arc<LinkSlot>, cfg: Config, net: &mut N) -> Self {
        let now = ms(net.now_us());
        // セッションを作るまでの仮のセッション。候補が空なので何も送らず何も受けず、`pump` はこれを呼ばない（`code_tick`）。
        let placeholder = link::Config {
            handshake: handshake::Config {
                session_id: 0,
                role_nonce: 0,
                latency: 0,
                name: handshake::Name::new(""),
                host_seat: Seat::P1,
            },
            secret: Secret([0; 16]),
            peers: Candidates::EMPTY,
            // 仮のセッションは候補が空で何も送らないので、種は使われない（ゲームのスレッドで OS の乱数を引かない）。
            key_seed: higanr_core::seal::KeySeed([0; 32]),
        };
        let mut np = Self::with_link(Link::new(placeholder, now), cfg, now);
        np.code = Some(slot);
        np
    }

    fn build_link(
        link_cfg: link::Config,
        cfg: &Config,
        wish: Option<ModeWish>,
        seat: Option<SeatAsk>,
        now: u32,
    ) -> Link {
        let mut link = Link::new(link_cfg, now);
        link.set_mode(wish.unwrap_or(cfg.mode_wish), cfg.rollback_ahead);
        // rollback が「できる」か（望みとは別。両者ができる組だけ、キャラセレクトの Tab で形式を替えられる）。
        link.set_can_rollback(cfg.rollback_able);
        // 席の希望（ゲーム内のメニューの「サイド」と始める側か返す側か。バージョン 4 の案）。無ければ ini の `host_seat` を名乗る。
        if let Some(a) = seat {
            link.set_seat_ask(a);
        }
        link.set_retry_window(HANDSHAKE_RETRY_MS);
        link.set_match_gate(cfg.match_gate);
        link.set_check_rule(match cfg.check {
            CheckSource::Game => CheckRule::SkipCountAtFrame0,
            CheckSource::InputDigest => CheckRule::All,
        });
        link
    }

    fn with_link(link: Link, cfg: Config, now: u32) -> Self {
        NetPlay {
            link,
            link_started_ms: now,
            connected_ms: None,
            ended_seen_ms: None,
            cfg,
            phase: Phase::Unconnected,
            end: None,
            seg_started: false,
            sel_entered: false,
            sel_left: false,
            own_charge: 0,
            peer_charge: 0,
            held_local: None,
            peer_finished: false,
            save_quit: false,
            left_by_save_quit: false,
            lost_pending: false,
            digest: INPUT_DIGEST_INIT,
            stall_run: 0,
            stats: Stats::default(),
            menu_net: false,
            menu_prev: 0,
            pre_link_decide_held: false,
            decisions: Decisions::default(),
            map_decided: false,
            menu_hold_run: 0,
            menu_retry: None,
            steps: StepLog::new(),
            own_select: None,
            agreed: None,
            lat_next: 0,
            arrivals: [NO_ARRIVAL; ARRIVAL_SLOTS],
            timing: FrameTiming::default(),
            head_wait_tick: u64::MAX,
            head_wait_pending_us: 0,
            tick_head_wait_us: 0,
            code: None,
            code_state: CodeState::default(),
            pong_installed: false,
            pong_enabled: false,
            spec: None,
            spec_host_only: false,
            spec_parked: None,
            rb_epoch: 0,
            rb_seen: 0,
            rb_bound: false,
            rb_glide_last: 0,
            rb_abort_first: None,
            stop: Stop::None,
            ui_resim_seen: 0,
            stats_tick: 0,
            stats_rewind: RewindWindow::new(),
            pick: None,
            digits: DigitEdge::new(),
            tab_edge: false,
            hud_screen: 0,
            hold_run: 0,
            wait: PeerWait::default(),
            wait_tick: u64::MAX,
            #[cfg(test)]
            sent_checks: Vec::new(),
        }
    }

    pub fn set_spectate(&mut self, ring: Arc<SpectateRing>) {
        self.spec = Some(ring);
    }

    /// 観戦のリングバッファを、マッチのホストのときだけ書く形で置く（ゲーム内のメニューの経路）。つなぎ直しごとに開くので、リングバッファは 1 本の接続のものになる。
    /// 繋がった tick に役がゲストと分かればリングバッファを外す（`tend`。配る側のスレッドは見張りがマッチのホストのときだけ起こす）。
    pub fn set_spectate_for_host(&mut self, ring: Arc<SpectateRing>) {
        self.spec = Some(ring);
        self.spec_host_only = true;
    }

    #[cfg(test)]
    pub fn has_spectate(&self) -> bool {
        self.spec.is_some()
    }

    /// 観戦のリングバッファへ記録を 1 つ書く（ゲームのスレッド。リングバッファが無ければ何もしない。確保も待ちもしない。溢れた記録はリングバッファが捨てて数え、対戦は止めない）。
    /// `&self` でなく欄を渡すのは、`connected_mut` の借用の間にも呼べるようにするため。
    fn spec_to(spec: &Option<Arc<SpectateRing>>, rec: Record) {
        if let Some(r) = spec {
            r.push(rec);
        }
    }

    /// 観戦: 新しい seg の始まり（`Seg`）と、その直後に席ごとの名前（1P → 2P）を積む。遅れて入った観戦者も seg の始まりから受けるので毎 seg 積む。
    /// 遅延の増分: 1 seg に 3 記録で、マッチの中のフレームには何も足さない。
    fn spec_seg(spec: &Option<Arc<SpectateRing>>, seg: u16, est: &handshake::Established) {
        if spec.is_none() {
            return;
        }
        for rec in higanr_core::spectate::seg_head(seg, est) {
            Self::spec_to(spec, rec);
        }
    }

    /// 止める地点で揃った SELECT の結果を、席ごと（1P → 2P）に積む。席ごとに違う欄は各席の SELECT から、共通の欄（マップ・難易度・CPU レベル・背景・シード）は
    /// ホストの値（`Agreed`）を両方に入れる。
    fn spec_selects(
        spec: &Option<Arc<SpectateRing>>,
        own_seat: Seat,
        own: Option<Select>,
        a: &Agreed,
    ) {
        let Some(own) = own else {
            return;
        };
        let common = |s: Select| Select {
            map: a.map,
            difficulty: a.difficulty,
            cpu_1p: a.cpu_1p,
            cpu_2p: a.cpu_2p,
            background: a.background,
            rng_seed: a.rng_seed,
            ..s
        };
        let (p1, p2) = match own_seat {
            Seat::P1 => (own, a.peer),
            Seat::P2 => (a.peer, own),
        };
        Self::spec_to(
            spec,
            Record::Select {
                seat: Seat::P1,
                sel: common(p1),
            },
        );
        Self::spec_to(
            spec,
            Record::Select {
                seat: Seat::P2,
                sel: common(p2),
            },
        );
    }

    /// ネットワークが終わった理由を観戦の終わり（`Record::End`）の reason にする。接続が張られなかった失敗は流れが無いので None。
    fn spec_end_reason(end: &End) -> Option<ByeReason> {
        match end {
            End::HandshakeFailed(_) => None,
            End::PeerBye(r) | End::Closed(r) => Some(*r),
            End::Dead => Some(ByeReason::Disconnected),
            End::Desync { .. } => Some(ByeReason::Desync),
            End::BadSelect(_) | End::MenuStuck(_) | End::Unmatched(_) => Some(ByeReason::Cancelled),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn code_state(&self) -> CodeState {
        self.code_state
    }

    pub fn head_wait_cap_us(&self) -> u32 {
        self.cfg.head_wait_cap_us
    }

    pub fn last_timing(&self) -> FrameTiming {
        self.timing
    }

    pub fn step_log(&self) -> &StepLog {
        &self.steps
    }

    pub fn selects(&self) -> (Option<Select>, Option<Agreed>) {
        (self.own_select, self.agreed)
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn end(&self) -> Option<End> {
        self.end
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// 相手のコードの回線の型の印（コードを読んだ後。接続コードの経路でなければ `None`）。確保しない。
    pub fn peer_nat(&self) -> Option<NatMark> {
        self.code.as_deref().and_then(LinkSlot::peer_nat)
    }

    /// 接続の手順を作り直した回数と直前の 1 回（`Link::restarts`）。左上の「つながらない」の 2 行が毎 tick 読むので、`Stats` をコピーせずこれだけ返す。
    /// 今のセッションの相手の候補が空なら 0 回を返す（取りやめの後のセッションは候補が無く、10 秒ごとに作り直されるが、接続の手順の時間切れではない）。
    pub fn restarts(&self) -> higanr_core::link::Restarts {
        if self.link.peer_candidate_counts().iter().all(|&n| n == 0) {
            return higanr_core::link::Restarts::default();
        }
        self.link.restarts()
    }

    /// ネットワークの対戦のマッチの終わりのメニューで「リプレイを保存して終了」が決まり、この tick に接続を終えたか（1 回だけ真）。呼び手は自動保存が呼んで
    /// いればゲームの保存の画面を飛ばす印を立てる（`higanr_core::save_skip`）。観戦者の頼みは `WatchPlay::take_save_quit`。
    pub fn take_save_quit(&mut self) -> bool {
        core::mem::take(&mut self.save_quit)
    }

    /// 成立した後にネットワークが終わったとき 1 回だけ、「妖怪 対 妖怪」へ戻す理由を返す（`net_ui::NetUi::on_lost`）。戻さない終わり方は
    /// `None`: 同期ズレ（止めて Esc を待つ別の経路）・「保存して終了」（タイトルのまま）・自分から終えた BYE(終了)（台本の終わり）。
    /// 確かめの枠で抜けた BYE(取消) は呼び手が見分ける（ゲームが自分で画面 6 へ戻る）。
    pub fn take_lost(&mut self) -> Option<Why> {
        if !core::mem::take(&mut self.lost_pending)
            || self.left_by_save_quit
            || self.stop != Stop::None
        {
            return None;
        }
        match self.end? {
            End::PeerBye(ByeReason::Finished | ByeReason::Cancelled) => Some(Why::PeerLeft),
            End::Closed(ByeReason::Cancelled) if self.stats.peer_wait_esc > 0 => Some(Why::SelfCut),
            End::PeerBye(ByeReason::Disconnected)
            | End::Closed(ByeReason::Disconnected | ByeReason::Cancelled)
            | End::Dead
            | End::MenuStuck(_)
            | End::BadSelect(_) => Some(Why::Lost),
            End::PeerBye(ByeReason::Desync)
            | End::Desync { .. }
            | End::Closed(ByeReason::Finished | ByeReason::Desync)
            | End::HandshakeFailed(_)
            | End::Unmatched(_) => None,
        }
    }

    /// 検査用: 「保存して終了」で閉じた tick と同じ印だけを置く。
    #[cfg(test)]
    pub fn set_save_quit(&mut self) {
        self.save_quit = true;
    }

    /// 自分から終える（BYE は次の tick の `poll` から出る）。最初の 1 回だけが効く。終えた後、マッチの中ならローカルの入力で続く。
    pub fn close(&mut self, reason: ByeReason) {
        if let Some(c) = self.link.connected_mut() {
            c.close(reason);
        }
        self.finish(End::Closed(reason));
    }

    /// 入力の地点の 1 tick（手入れ `tend` → 判断 `dispatch`）。本物の経路は `net_ui::net_input`。検査用。
    #[cfg(test)]
    pub fn on_input<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        self.tend(net);
        self.dispatch(game, net)
    }

    /// ネットワークの手入れ（入力の地点の tick の始まり。ゲームの入力・場面には触れず、feed もしない）: 受信をセッションへ渡して `poll` する・終わりの印を見る・PONG を合わせる・
    /// 繋がったのを見たら `Unconnected → Selecting` にする。ゲーム内のメニューが入力を握って `dispatch` を呼ばない tick でも毎 tick 要る
    /// （止まるとセッションが作られずつながらない）。1 tick に 1 回だけ呼ぶこと。
    pub fn tend<N: Net>(&mut self, net: &mut N) {
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        self.pump(net);
        self.watch_end();
        self.sync_pong(net);
        if self.phase == Phase::Ended && self.ended_seen_ms.is_none() {
            self.ended_seen_ms = Some(ms(net.now_us()));
        }
        if self.phase == Phase::Unconnected
            && let Some(c) = self.link.connected()
        {
            // 繋がったのを最初に見た時刻（試験のまとめの「繋がるまで」と `Conn::Finished` の印）。成立を待つ実行でも、成立ではなく繋がった時刻。
            if self.connected_ms.is_none() {
                self.connected_ms = Some(ms(net.now_us()));
                net.link_up();
                // 観戦をマッチのホストだけが配る形で、役がゲストならリングバッファを外す（`Seg` を積む前に外すので、ゲストのリングバッファには何も積まれない）。外したリングバッファは解放せず置き場へ移すだけ。
                if self.spec_host_only && c.role() == Role::Guest {
                    self.spec_parked = self.spec.take();
                }
            }
            if c.matched() {
                // 成立した（ゲートの無いセッションは繋がった tick）。ここから選択の同期・lockstep・CHECK に入れる。
                self.phase = Phase::Selecting;
                // 観戦: 繋がった seg（最初は 0）の始まり。
                let seg = c.lockstep().seg();
                Self::spec_seg(&self.spec, seg, &c.established());
            } else if let Some(why) = c.match_gate().and_then(|g| g.closed()) {
                // 成立しないまま時間切れ・相手にゲートが無い（セッションが同じ poll で BYE(取消) を出した —— `link::Connected::poll_gate`）。
                self.finish(End::Unmatched(why));
            }
        }
        self.ensure_pick();
    }

    /// フレームの始まり（自分の入力を読む前）で、この tick の入力の地点で要る相手の入力ワードを待つ（delay 方式の lockstep だけ）。
    /// 2 台の 60 Hz の格子の位相の差の分だけ先に進んでいる側が入力を読んだ後で待っていた待ちを、フレームの始まりへ移す（`HEAD_WAIT_CAP_US` の由来）。
    /// 始まりで待った分は入力の地点の予算から引く。呼ぶのはウィンドウ（b-spin / b）の格子の後と、全画面でゲームがその回にフレームを始める見込みのときだけ
    /// （`pacing_win::pace_head`）。全画面の経路は 1 フレームに何度も呼ばれうるので、2 度目から `Off`。
    ///
    /// 次の全部が成り立つときだけ待つ（外れれば `Off`。時計にもセッションにも触らない）: `Config::head_wait_cap_us` が 0 でない・`phase` が Lockstep でセッションが繋がっていて
    /// rollback でない・今の遅れが 1 以上で使う前の切替も 0 への切替でない（遅れ 0 では相手の取得が相手の始まりの後なので、両者が始まりで待って上限まで固まる）・
    /// 直前の tick が描画だけのフレームでない（入力の地点が予算まで待って来た後なので、止まった相手に 1 tick ごと待ちを足さない）・
    /// 相手がマッチを終えていない・この tick でまだ待っていない。
    ///
    /// 待ち方は入力の地点の待ちと同じ（`Net::wait` を最大 `RESEND_MS` ずつ。受信で起き、起きるたびに受信をセッションへ渡し、送り直しの期限だけ見る）。
    /// 上限で届かなければ `Capped`、待つ間にネットワークが終わる・相手がマッチを終えたら `Ended` で戻る。確定する入力の列は変わらない。
    pub fn head_wait<N: Net>(&mut self, net: &mut N) -> HeadWait {
        let cap = self.cfg.head_wait_cap_us;
        if cap == 0
            || self.phase != Phase::Lockstep
            || self.stall_run != 0
            || self.peer_finished
            || self.head_wait_tick == self.stats.ticks
        {
            return HeadWait::Off;
        }
        match self.head_wait_state() {
            None => return HeadWait::Off,
            Some(true) => return HeadWait::Ready,
            Some(false) => {}
        }
        self.pump(net);
        self.watch_end();
        if self.phase != Phase::Lockstep || self.peer_finished {
            return HeadWait::Ended(0);
        }
        // 受信を渡した後で形が変わりうる（0 への切替が届いた）。待たない形になったなら `Off`、入力ワードが届いたなら `Ready`。
        match self.head_wait_state() {
            None => return HeadWait::Off,
            Some(true) => return HeadWait::Ready,
            Some(false) => {}
        }
        let start = net.now_us();
        let deadline = start.saturating_add(u64::from(cap));
        let mut now = start;
        let end = loop {
            if now >= deadline {
                break HeadWait::Capped(0);
            }
            let left = (deadline - now).min(u64::from(lockstep::RESEND_MS) * 1_000);
            net.wait(left as u32);
            self.pump(net);
            self.watch_end();
            now = net.now_us();
            if self.phase != Phase::Lockstep || self.peer_finished {
                break HeadWait::Ended(0);
            }
            match self.head_wait_state() {
                Some(true) => break HeadWait::Arrived(0),
                Some(false) => {}
                None => break HeadWait::Ended(0),
            }
            if let Some(c) = self.link.connected_mut() {
                c.resend_if_due(ms(now), net);
            }
        };
        let waited = now.saturating_sub(start).min(u64::from(u32::MAX)) as u32;
        self.head_wait_tick = self.stats.ticks;
        self.head_wait_pending_us = waited;
        self.stats.head_waits = self.stats.head_waits.saturating_add(1);
        self.stats.head_wait_us_total = self
            .stats
            .head_wait_us_total
            .saturating_add(u64::from(waited));
        match end {
            HeadWait::Capped(_) => {
                self.stats.head_wait_caps = self.stats.head_wait_caps.saturating_add(1);
                HeadWait::Capped(waited)
            }
            HeadWait::Arrived(_) => HeadWait::Arrived(waited),
            _ => HeadWait::Ended(waited),
        }
    }

    /// 始まりで待つ形か。`None` = 待つ形でない（繋がっていない・rollback・遅れ 0 か 0 への切替）、`Some(true)` = 待つ形で、相手の入力ワードは
    /// もう届いている、`Some(false)` = 待つ形で、まだ届いていない。
    fn head_wait_state(&self) -> Option<bool> {
        let c = self.link.connected()?;
        if c.rollback().is_some() {
            return None;
        }
        let ls = c.lockstep();
        if ls.delay() == 0 || ls.pending_switch().is_some_and(|sw| sw.delay == 0) {
            return None;
        }
        Some(ls.remote_ready())
    }

    /// 繋がっていて、遅れの量の選びをまだ持っていなければ作る（`Config::delay_pick` が true で `lat_plan` が空のときだけ）。
    /// 役とモードは AGREE で決まっているので、繋がりごとに 1 つ。
    fn ensure_pick(&mut self) {
        if self.pick.is_some() || !self.cfg.delay_pick || !self.cfg.lat_plan.is_empty() {
            return;
        }
        if let Some(c) = self.link.connected() {
            let mut p = DelayPick::new(c.role() == Role::Host, c.mode());
            p.set_start(self.code.as_deref().and_then(LinkSlot::start));
            self.pick = Some(p);
        }
    }

    pub fn dispatch<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        self.tick_head_wait_us = core::mem::take(&mut self.head_wait_pending_us);
        // 数字キー（0〜8 がその値・9 が auto）: マッチの外（選択・読み込みの間）はここで毎 tick 1 回読む。押した値は次の seg の始まりの D になる
        // （ホストは始まりの D を直に替える、ゲストの要求は取得の数で数えるので選択の間に捨てられない —— `Rollback::press_delay`）。
        // マッチの間は取得の中で読む。
        if matches!(
            self.phase,
            Phase::Selecting | Phase::Holding | Phase::Loading
        ) {
            self.digit_tick(game);
        }
        let action = match self.phase {
            Phase::Unconnected => self.unconnected_tick(game),
            // 終えた（相手が来ないまま期間が切れた・BYE・沈黙など）後はネットワークの外。人が画面から出られるようにローカルの入力のまま。
            // ただしマッチの最中の同期ズレで止めている間は、Esc まで描画だけのフレーム（`ended_tick`）。
            Phase::Ended => self.ended_tick(game),
            Phase::Selecting => {
                self.hud_screen = 0;
                let (cur, next) = game.scenes();
                if cur == SCENE_MENU && next == SCENE_MATCH {
                    if self.cfg.require_net_selection && !self.map_decided {
                        // ネットワークの選択でマップが決まっていない場面 2 はネットワークのマッチではない（デモ・リプレイの再生など）。止める地点に入ると
                        // 相手の SELECT を待って描画だけのフレームを返し続け、ゲームが止まる（`Config::require_net_selection`）。MainMenu は読まない。
                        self.stats.local_scene2 = self.stats.local_scene2.saturating_add(1);
                        Action::Local
                    } else {
                        self.phase = Phase::Holding;
                        self.hold_run = 0;
                        self.hold(game, net)
                    }
                } else if cur == SCENE_MENU {
                    self.menu_tick(game, net)
                } else {
                    Action::Local
                }
            }
            Phase::Holding => self.hold(game, net),
            Phase::Loading => self.zero(game),
            Phase::Lockstep => self.lockstep_tick(game, net),
        };
        self.peer_wait_tick(game, net);
        action
    }

    /// 相手を待ち続けている間の知らせと切断（`peer_wait`）。1 tick に 1 回: delay 方式は `dispatch` の終わり、rollback のモードのマッチは
    /// 走査の包みのフックの後（`script_win::rb_scan_handler`）。待っていたかは、マッチは描画だけのフレームが続いているか、止める地点は相手の SELECT を
    /// 待っているか、選択の画面は相手のゲームスレッドが黙っているか（PONG だけでは戻らない `Liveness::quiet_ms`）。切るときは自分から閉じる
    /// （Esc は BYE(取消)、時間切れは BYE(切断)）ので、その後は普段の「接続が終わった」の経路。Esc は知らせを出している間だけ読む。
    pub fn peer_wait_tick<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) {
        if self.wait_tick == self.stats.ticks {
            return;
        }
        self.wait_tick = self.stats.ticks;
        // 時計はマッチ・止める地点では待っている tick にだけ読む（ふだんのマッチの tick に足すのは比べだけ）。
        let stalled = match self.phase {
            Phase::Lockstep => self.stall_run > 0,
            Phase::Holding => self.hold_run > 0,
            _ => false,
        };
        let (blocked, now) = if stalled {
            let now = ms(net.now_us());
            (Some(now), now)
        } else if self.phase == Phase::Selecting && self.hud_screen != 0 {
            let now = ms(net.now_us());
            let quiet = self
                .link
                .connected()
                .map_or(0, |c| c.liveness().quiet_ms(now));
            (
                (quiet >= peer_wait::QUIET_MS).then(|| now.wrapping_sub(quiet)),
                now,
            )
        } else {
            (None, 0)
        };
        match self
            .wait
            .step(blocked, now, || game.peek_local_input() & ESC != 0)
        {
            Verdict::Keep => {}
            Verdict::Esc => {
                self.stats.peer_wait_esc = self.stats.peer_wait_esc.saturating_add(1);
                self.close(ByeReason::Cancelled);
            }
            Verdict::TimedOut => {
                self.stats.peer_wait_cuts = self.stats.peer_wait_cuts.saturating_add(1);
                self.close(ByeReason::Disconnected);
            }
        }
    }

    /// 相手を待つ知らせ（`net_menu::T_PEER_WAIT`）を出す間か。
    pub fn peer_wait_shown(&self) -> bool {
        self.phase != Phase::Ended && self.wait.shown()
    }

    /// 開始のフック（`0x0041AB2B`。読み込みの後の最初の更新の中）: コピーにチャージ型を書き、次の tick を frame 0 にする。LOADING のときだけ。
    pub fn on_match_start<G: Game>(&mut self, game: &mut G) {
        if self.phase != Phase::Loading {
            return;
        }
        let Some(c) = self.link.connected() else {
            return;
        };
        let (p1, p2) = by_seat(
            c.seat(),
            u16::from(self.own_charge),
            u16::from(self.peer_charge),
        );
        if !game.write_charge(p1 as u8, p2 as u8) {
            self.stats.charge_write_failures = self.stats.charge_write_failures.saturating_add(1);
        }
        if let Some(p) = self.pick.as_mut() {
            // delay 方式の初めの値は推奨値（ホストがまだ何も押していなければ。このときの RTT で）。作った値は最初の取得でネットワークの側へ渡す（`deliver_pick`）。
            p.begin_match(c.liveness().rtt_ms());
        }
        self.phase = Phase::Lockstep;
        self.seg_started = true;
        self.stall_run = 0;
        self.stats.segs_started = self.stats.segs_started.saturating_add(1);
    }

    /// 接続コードの受け渡しの 1 回（`pump` の始まり）。見張りへ「繋がった」「終えた」を知らせ、繋がる前なら新しい設定でセッションを作る（作り直す）。
    /// セッションをまだ一度も作っていなければ、届いたものをリングから出して捨て、false を返す（`pump` は仮のセッションを呼ばない）。ファイルには触らない。
    fn code_tick<N: Net>(&mut self, net: &mut N) -> bool {
        let Some(slot) = self.code.as_deref() else {
            return true;
        };
        let st = &mut self.code_state;
        if self.phase == Phase::Ended {
            if !st.notified_done {
                slot.mark_done();
                st.notified_done = true;
            }
        } else if let Some(c) = self.link.connected() {
            if !st.notified_connected {
                slot.mark_role(c.role());
                slot.mark_may_list(c.established().may_list);
                slot.mark_connected(c.seat(), c.mode(), c.established().delay);
                st.notified_connected = true;
            }
        } else if let Some((c, wish, seat)) = slot.take_newer(&mut st.seen_gen) {
            let now = ms(net.now_us());
            st.session = c.handshake.session_id;
            self.link = Self::build_link(c, &self.cfg, wish, seat, now);
            self.link.set_listable(slot.list_wish());
            self.link_started_ms = now;
            st.links = st.links.saturating_add(1);
        }
        if st.links > 0 {
            return true;
        }
        let gave_up = slot.gave_up();
        let mut buf = [0u8; MAX_DATAGRAM];
        while net.recv(&mut buf).is_some() {
            st.dropped_before_link = st.dropped_before_link.saturating_add(1);
        }
        if gave_up {
            self.finish(End::HandshakeFailed(FailReason::NothingReceived));
        }
        false
    }

    /// リングを空にしてセッションへ渡し、セッションの `poll` を呼ぶ。
    fn pump<N: Net>(&mut self, net: &mut N) {
        if self.code.is_some() && !self.code_tick(net) {
            return;
        }
        let mut buf = [0u8; MAX_DATAGRAM];
        while let Some((from, n, t_us, pong_sent)) = net.recv_marked(&mut buf) {
            let now = ms(net.now_us());
            let before = self.remote_mark();
            // 受信スレッドが PONG を返した PING には、セッションは PONG を重ねない（`sync_pong`）。PONG の往復は受信スレッドが受けた時刻 `t_us` で測る
            // （`now` で測ると最大 1 フレーム大きく出る）。
            let r = self.link.on_datagram_at(
                from,
                buf.get(..n).unwrap_or(&[]),
                now,
                ms(t_us),
                pong_sent,
                net,
            );
            self.stamp_arrivals(before, t_us);
            if let Recv::Desync { local, remote } = r {
                self.desync(local, remote);
            }
        }
        let now = ms(net.now_us());
        self.link.poll(now, net);
        net.heartbeat(now);
        let r = self.link.restarts();
        self.stats.hs_restarts = r.count;
        self.stats.hs_restart_last = r.last;
    }

    /// 受信スレッドの PONG（`higanr_core::pong`）を生存の状態に合わせる。tick に 1 回（`pump` の後）。受信スレッドは PING を受けたその場で PONG を返す
    /// （tick を待つと相手の RTT が大きく出る）。返してよいのは生存が「生きている」の間だけ: 閉じた後は PONG の代わりに BYE を返し、切れた後は何も返さないので、
    /// その 2 つはセッションが返す。状態が変わった tick にだけ受信スレッドへ知らせる。
    fn sync_pong<N: Net>(&mut self, net: &mut N) {
        let want = self
            .link
            .connected()
            .is_some_and(|c| c.liveness().state() == liveness::State::Alive);
        if want == self.pong_enabled {
            return;
        }
        if want && !self.pong_installed {
            // 繋がった tick に 1 回。錠が取れなければ次の tick にやり直す（その間はセッションが返す）。
            let Some(r) = self.link.pong_responder() else {
                return;
            };
            if !net.pong_install(&r) {
                return;
            }
            self.pong_installed = true;
        }
        net.pong_enable(want);
        self.pong_enabled = want;
    }

    fn remote_mark(&self) -> Option<(u16, u32)> {
        // rollback のモードでは INPUT を状態機械が受ける（`lockstep()` は seg だけを揃えて置いてある —— `link.rs`）。
        self.link.connected().map(|c| match c.rollback() {
            Some(rb) => (rb.seg(), rb.remote_next()),
            None => (c.lockstep().seg(), c.lockstep().remote_next()),
        })
    }

    fn stamp_arrivals(&mut self, before: Option<(u16, u32)>, t_us: u64) {
        let (Some((s0, n0)), Some((s1, n1))) = (before, self.remote_mark()) else {
            return;
        };
        if s0 != s1 || n1 <= n0 {
            return;
        }
        let from = n0.max(n1.saturating_sub(ARRIVAL_SLOTS as u32));
        for w in from..n1 {
            if let Some(slot) = self.arrivals.get_mut(w as usize % ARRIVAL_SLOTS) {
                *slot = Arrival {
                    seg: s1,
                    frame: w,
                    t_us,
                };
            }
        }
    }

    fn watch_end(&mut self) {
        if self.phase == Phase::Ended {
            return;
        }
        if let handshake::State::Failed(r) = self.link.handshake_state() {
            self.finish(End::HandshakeFailed(r));
            return;
        }
        let Some(c) = self.link.connected() else {
            return;
        };
        if c.liveness().state() == liveness::State::Dead {
            self.finish(End::Dead);
        } else if let Some(r) = c.liveness().peer_bye() {
            // 相手がマッチを終えた（BYE(終了)）なら、マッチの中ではすぐにはやめない。相手の最後の入力ワードは BYE より先に届いている（同じ受けで両方を渡す
            // ことがある）ので、届いた入力ワードを使い切り、待ちの予算が切れたときかマッチの場面を出るときにやめる（`lockstep_tick`）。
            if r == ByeReason::Finished && self.phase == Phase::Lockstep {
                self.peer_finished = true;
            } else if r == ByeReason::Cancelled && !c.matched() {
                // 成立の前に相手が閉じた（相手の 10 分が先に切れた・相手が確かめの枠で抜けた）。遊んだ後に終わったのと見分けられないので、成立しないまま
                // 閉じた理由に載せる。ゲートの無いセッションは繋がったときに成立しているので、ここへ来ない。
                self.finish(End::Unmatched(NoMatch::PeerLeft));
            } else {
                self.finish(End::PeerBye(r));
            }
        }
    }

    fn desync(&mut self, local: Check, remote: Check) {
        if self.phase == Phase::Ended {
            return;
        }
        if let Some(c) = self.link.connected_mut() {
            c.close(ByeReason::Desync);
        }
        self.finish(End::Desync { local, remote });
    }

    fn finish(&mut self, end: End) {
        if self.phase != Phase::Ended {
            // マッチの最中（lockstep）に同期ズレで終わった（自分が CHECK の食い違いを見つけた・相手の BYE(同期ズレ) を受けた）ら、そのマッチを止める。
            // マッチの外で終わったときは止めるものが無い。
            if self.phase == Phase::Lockstep
                && matches!(end, End::Desync { .. } | End::PeerBye(ByeReason::Desync))
            {
                self.stop = Stop::Held { esc_armed: false };
                self.stats.stop_played = self.stats.played;
            }
            // 成立していた（選択・止める地点・読み込み・試合のどこか）なら、「妖怪 対 妖怪」へ戻す候補（`take_lost`）。
            self.lost_pending = self.phase != Phase::Unconnected;
            self.phase = Phase::Ended;
            self.end = Some(end);
            // 観戦: 流れの終わり。繋がっていなかった（接続の失敗）なら積まない。
            if let Some(r) = Self::spec_end_reason(&end)
                && self.link.connected().is_some()
            {
                Self::spec_to(&self.spec, Record::End(r));
            }
        }
    }

    fn count_wait_over(&mut self, over_us: u64) {
        let over = over_us.min(u64::from(u32::MAX)) as u32;
        self.stats.wait_timeouts = self.stats.wait_timeouts.saturating_add(1);
        self.stats.wait_over_max_us = self.stats.wait_over_max_us.max(over);
        let i = WAIT_OVER_BOUNDS_US
            .iter()
            .position(|&b| over < b)
            .unwrap_or(WAIT_OVER_BOUNDS_US.len());
        if let Some(n) = self.stats.wait_over_hist.get_mut(i) {
            *n = n.saturating_add(1);
        }
    }

    fn feed3<G: Game>(game: &mut G, p1: u16, p2: u16) {
        game.feed(0, p1);
        game.feed(1, p2);
        game.feed(2, p1 | p2);
    }

    fn zero<G: Game>(&mut self, game: &mut G) -> Action {
        Self::feed3(game, 0, 0);
        self.stats.zero_ticks = self.stats.zero_ticks.saturating_add(1);
        Action::Zero
    }

    fn end_action(&mut self) -> Action {
        if matches!(self.stop, Stop::Held { .. }) {
            self.stats.stop_ticks = self.stats.stop_ticks.saturating_add(1);
            Action::RenderOnly
        } else {
            Action::Local
        }
    }

    /// ネットワークが終わった後の 1 tick。止めていなければ `Local`。止めている間は、Esc が「押されていない → 押された」に変わる tick まで描画だけのフレーム
    /// （ゲームの更新を走らせない —— 乱数もマッチも動かない）。その tick から `Local`（この tick の Esc もゲームへ届き、一時停止のメニューが開く）。
    fn ended_tick<G: Game>(&mut self, game: &mut G) -> Action {
        let Stop::Held { esc_armed } = self.stop else {
            return Action::Local;
        };
        if game.peek_local_input() & ESC == 0 {
            self.stop = Stop::Held { esc_armed: true };
        } else if esc_armed {
            self.stop = Stop::Released;
            return Action::Local;
        }
        self.end_action()
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.stop, Stop::Held { .. })
    }

    /// 同期ズレでマッチを止めたことの印（本ログの `S_STOP`）: bit0 = 止めた（今も止めている・Esc で戻った）、bit1 = Esc で戻った、
    /// bit2 = 相手の BYE(同期ズレ) で止めた（自分が CHECK の食い違いを見つけたのでなく）。止めていなければ 0。
    pub fn stop_flags(&self) -> u32 {
        match self.stop {
            Stop::None => 0,
            Stop::Held { .. } | Stop::Released => {
                1 | if self.stop == Stop::Released { 2 } else { 0 }
                    | if matches!(self.end, Some(End::PeerBye(ByeReason::Desync))) {
                        4
                    } else {
                        0
                    }
            }
        }
    }

    pub(crate) fn ui_conn(&self, now_ms: u32) -> (Conn, Seat) {
        if self.phase == Phase::Ended {
            let conn = if self.is_stopped() {
                Conn::Desynced
            } else if let Some(End::Unmatched(why)) = self.end {
                Conn::Unmatched(why)
            } else if let Some(End::HandshakeFailed(FailReason::OtherVersion { theirs })) = self.end
            {
                Conn::OtherVersion { theirs }
            } else if let Some(End::HandshakeFailed(FailReason::OtherBuild { theirs })) = self.end {
                Conn::OtherBuild { theirs }
            } else if self.connected_ms.is_some() {
                Conn::Finished
            } else {
                Conn::Ended
            };
            return (conn, Seat::P1);
        }
        match self.link.connected() {
            Some(c) if self.phase == Phase::Unconnected => (
                Conn::Found {
                    seat: c.seat(),
                    shape: Some((c.mode(), c.established().delay)),
                    ready: c.match_gate().is_some_and(|g| g.local()),
                    left_secs: c.match_gate().map_or(0, |g| g.left_ms(now_ms) / 1000),
                },
                c.seat(),
            ),
            Some(c) => (
                Conn::Connected {
                    seat: c.seat(),
                    host: c.role() == Role::Host,
                    shape: Some((c.mode(), self.shown_delay(c))),
                },
                c.seat(),
            ),
            None => (Conn::Waiting, Seat::P1),
        }
    }

    /// ネットワークの組を片づけてよいか（ゲーム内のメニューの実行のつなぎ直し）: ネットワークが終わっていて、終わりを手入れが見てから `grace_ms` 経ち、場面がマッチの外。
    /// マッチの中で切れたときは `Local` で続け、マッチの外へ戻ってから片づける。
    pub fn relink_due(&self, now_ms: u32, scenes: (u32, u32), grace_ms: u32) -> bool {
        let (cur, next) = scenes;
        self.phase == Phase::Ended
            && !self.is_stopped()
            && self
                .ended_seen_ms
                .is_some_and(|t| now_ms.wrapping_sub(t) >= grace_ms)
            && cur == SCENE_MENU
            && next != SCENE_MATCH
    }

    /// つながった（ことがある）接続の session。片づけるときに「使った」コードとして記録し（`net_code_game` の `GameUi::spent`）、次のつながりで
    /// 同じコードを貼られても読まない。つながらないまま終わった session は記録しない（相手がまだ待っていれば、同じコードで繋ぎ直せる）。
    pub fn spent_session(&self) -> Option<u32> {
        (self.code.is_some() && self.connected_ms.is_some() && self.code_state.links > 0)
            .then_some(self.code_state.session)
    }

    /// 自分が「妖怪 対 妖怪」を選んだ（`net_ui::net_input`）。繋がっていて成立の前なら、READY の 1 通目をすぐ送る（`link::Connected::set_ready`）。
    /// 成立を待たない実行・繋がる前・終えた後は何もしない。
    pub(crate) fn set_ready<N: Net>(&mut self, net: &mut N) {
        if self.phase != Phase::Unconnected {
            return;
        }
        let now = ms(net.now_us());
        if let Some(c) = self.link.connected_mut() {
            c.set_ready(now, net);
        }
    }

    /// マッチ中の行（「Latency N (推奨)  ping NNms  rollback N」。形は `net_menu::hud_line`）の材料。マッチの中でなければ `None`（行を消す）。
    /// 「戻し」は前に呼んでから今までに進め直した更新の数（rollback のときだけ）で、メニューの側が直近 60 フレームの最大にする。
    /// キャラセレクト（画面 8）とマップ（画面 16）の間も `Some`（左下の行の先頭に形式が付き、「戻し」は無い）。画面 6・7 と選択の外は `None`（`hud_screen`）。
    pub(crate) fn hud_facts(&mut self) -> Option<HudFacts> {
        let in_match = self.phase == Phase::Lockstep;
        let in_select = self.phase == Phase::Selecting
            && (self.hud_screen == SCREEN_CHARA || self.hud_screen == SCREEN_MAP);
        if !in_match && !in_select {
            self.stats_tick = 0;
            return None;
        }
        let (latency, rtt_ms, is_rollback, recommend, names, auto, mode, delay_only) =
            self.link.connected().map(|c| {
                let rtt = c.liveness().rtt_ms();
                let est = c.established();
                (
                    self.shown_delay(c),
                    rtt,
                    c.rollback().is_some(),
                    c.recommend_now(),
                    HudNames::of(est.seat, est.own_name, est.peer_name),
                    c.shown_auto(),
                    c.mode(),
                    !c.rollback_able(),
                )
            })?;
        if !in_match {
            return Some(HudFacts {
                latency: Some(latency),
                rtt_ms,
                rewound: None,
                recommend,
                auto,
                format: Some(mode),
                delay_only,
                names: Some(names),
            });
        }
        let rewound = if is_rollback {
            self.rollback_core_stats().map(|s| {
                let total = s
                    .resim_confirmed_ticks
                    .saturating_add(s.resim_predicted_ticks);
                let d = total.wrapping_sub(self.ui_resim_seen);
                self.ui_resim_seen = total;
                d
            })
        } else {
            None
        };
        self.spec_stats(latency, rtt_ms, rewound);
        Some(HudFacts {
            latency: Some(latency),
            rtt_ms,
            rewound,
            recommend,
            auto,
            format: None,
            delay_only: false,
            names: Some(names),
        })
    }

    /// 観戦: 対戦の状態（ping・Latency・Rollback）を、マッチの中で 60 回の呼びに 1 回（約 1 秒）観戦のリングバッファへ積む。値は `hud_facts` が作る対戦者の左下の行と同じ。
    /// リングバッファが無ければ何もしない。遅延の増分: 1 秒に 1 回のリングバッファへの書き（確保なし・待ちなし）と、毎フレームのウィンドウの更新。
    fn spec_stats(&mut self, latency: u8, rtt_ms: Option<u32>, rewound: Option<u32>) {
        if self.spec.is_none() {
            return;
        }
        if let Some(d) = rewound {
            self.stats_rewind.push(d);
        }
        let due = self.stats_tick == 0;
        self.stats_tick = (self.stats_tick + 1) % STATS_EVERY;
        if due {
            let rollback = rewound.map(|_| self.stats_rewind.max());
            Self::spec_to(&self.spec, Record::stats(rtt_ms, latency, rollback));
        }
    }

    /// 画面に出す遅れ: ネットワークの側の「目標」（`Connected::shown_delay`）。まだ渡していない初めの値があればそれ。
    fn shown_delay(&self, c: &Connected) -> u8 {
        self.pick
            .and_then(|p| p.pending())
            .filter(|&x| x != delay_pick::AUTO)
            .unwrap_or_else(|| c.shown_delay_for(self.phase == Phase::Lockstep))
    }

    fn deliver_pick(pick: &mut Option<DelayPick>, c: &mut Connected, stats: &mut Stats) {
        let Some(x) = pick.as_mut().and_then(DelayPick::take) else {
            return;
        };
        stats.delay_presses = stats.delay_presses.saturating_add(1);
        if c.press_delay(x) == lockstep::DelayPress::Unchanged {
            stats.delay_presses_unchanged = stats.delay_presses_unchanged.saturating_add(1);
        }
    }

    /// Ctrl+数字キー（上段の 0〜9。0〜8 がその値、9 が auto）と Ctrl+Tab の押した瞬間を読んで、ネットワークの側（`Connected::press_delay`）へ渡す。1 tick に 1 回。
    /// ネットワークが繋がっていて遅れの量の選び `pick` を持つ間だけ読む（ウィンドウが前面にない・文字の入力中は `Game::digit_keys` が `None`）。台本の付いている実行は、
    /// 台本の段階（`digit_script`）の押下だけを使い、本物のキーを読まない。押した値は、マッチの途中なら D の予定・切替、マッチの外ならホストは次の seg の始まりの D、
    /// ゲストはホストへの要求になる。auto（9）はホストが旗を持つ。遅延の増分: マッチ中の 1 tick に数字キーの読み 10 回と比べ数回。
    fn digit_tick<G: Game>(&mut self, game: &mut G) {
        let in_net = matches!(
            self.phase,
            Phase::Selecting | Phase::Holding | Phase::Loading | Phase::Lockstep
        );
        // 台本の段階（`digit=x` / `tab=1`）は、その tick の押した瞬間を直に差し替える（OS には何も注がない）。段階が効き始めた tick にネットワークの外だった分は捨てる。
        // 台本の押下は `pick` が無くても効く（`lat_plan` を書いた実行は `pick` を作らない）。
        let scripted = game.digit_script();
        let pressed = match scripted {
            Some(p) if in_net => p,
            Some(_) => 0,
            None => {
                // 台本の実行は本物のキーを読まない。読まない tick は `None` と同じく `feed` へ渡す（押し続けの状態は `DigitEdge` が持ったまま）。
                let held = if in_net && self.pick.is_some() && !game.scripted_run() {
                    game.digit_keys()
                } else {
                    None
                };
                self.digits.feed(held)
            }
        };
        // Tab は形式の切替（キャラセレクトの間だけ。`menu_tick` が取る）。毎回書き直すので、取られなかった押下は次の tick に残らない。
        if std::mem::take(&mut self.tab_edge) {
            self.stats.tab_ignored = self.stats.tab_ignored.saturating_add(1);
        }
        self.tab_edge = digit_keys::tab_pressed(pressed);
        if self.tab_edge {
            self.stats.tab_presses = self.stats.tab_presses.saturating_add(1);
        }
        let Some(x) = digit_keys::pick(pressed) else {
            return;
        };
        let Some(c) = self.link.connected_mut() else {
            return;
        };
        self.stats.digit_presses = self.stats.digit_presses.saturating_add(1);
        if c.press_delay(x) == lockstep::DelayPress::Unchanged {
            self.stats.digit_unchanged = self.stats.digit_unchanged.saturating_add(1);
        }
        if let Some(p) = self.pick.as_mut() {
            p.note_digit(x);
        }
    }

    pub fn delay_pick(&self) -> Option<DelayPick> {
        self.pick
    }

    /// マッチが始まった seg を出る。相手が先に終えていたら、ここでやめる。
    fn leave_seg(&mut self) {
        if self.peer_finished {
            self.finish(End::PeerBye(ByeReason::Finished));
        }
        if !self.seg_started {
            return;
        }
        if let Some(c) = self.link.connected_mut() {
            let next = c.seg().wrapping_add(1);
            // rollback のモードの次の seg の D は、前の seg の最後の予定の値（数字キーで替えた値）。`Rollback::begin_seg` が決め、INPUT の lat で相手へ渡る。
            c.begin_seg(next);
            self.rb_epoch = self.rb_epoch.wrapping_add(1);
            Self::spec_seg(&self.spec, next, &c.established());
        }
        self.seg_started = false;
        self.held_local = None;
    }

    /// まだ繋がっていない間の 1 tick。選択の画面（人対人の Match の 7・8・16）では、ローカルの入力から決定のビットだけを落としてゲームへ入れる
    /// （`Action::Menu`）。取消は通す（決定を止めれば 8 より先へ進めないので、取消で 6 へ戻るのは害が無い）。席は繋がる前は決まらないので 1P の入力ワードとして
    /// 入れる（キーボードの入力ワードは席に依らず、台本の入力ワードは 1P 側を読む）。2P の別の入力機器は読まない。それ以外は `Local`。繋がる前にホストの台本が難易度の画面で決定を押すと、
    /// ホストのゲームだけがキャラ選択へ進んで画面が食い違ったまま戻らなかったため、繋がるまでは決定を通さず、2 台が同じ所から始められるようにする。
    /// 諦めた後は止めない（`phase` が `Ended` になり、`Local`）。期間の中の作り直しはまだ繋がる前なので、その間は決定を落とし続ける。遅延の増分は 0。
    fn unconnected_tick<G: Game>(&mut self, game: &mut G) -> Action {
        // 成立を待つ実行（`code_side = game`。`Config::match_gate`）は、繋がる前も成立する前も入力に触らない: 試合へは両者が「妖怪 対 妖怪」を選び、
        // 成立してから自動の前進で入るので、決定を落とす理由が無く、落とすと待っている間にローカルの人間 対 人間を遊べなくなる。
        if self.cfg.match_gate {
            self.pre_link_decide_held = false;
            return Action::Local;
        }
        // 選択の画面を持たないゲームのウィンドウ（`require_net_selection` が false）では、繋がる前にゲームの場面を読まない。
        if !self.cfg.require_net_selection {
            return Action::Local;
        }
        let (cur, next) = game.scenes();
        // 止める地点（場面 2 への切替の tick）では MainMenu を読まない（`Game::menu_view`）。
        let in_selection = if cur != SCENE_MENU || next == SCENE_MATCH {
            false
        } else {
            game.menu_view().filter(net_select::in_selection).is_some()
        };
        if !in_selection {
            self.pre_link_decide_held = false;
            return Action::Local;
        }
        let raw = game.local_input(Seat::P1);
        let held = raw & DECIDE_BITS != 0;
        if held && !self.pre_link_decide_held {
            self.stats.menu_pre_link_decides = self.stats.menu_pre_link_decides.saturating_add(1);
        }
        self.pre_link_decide_held = held;
        Self::feed3(game, raw & !DECIDE_BITS, 0);
        Action::Menu
    }

    /// ネットの選択の 1 フレーム（今の場面 1 で、次の要求が 2 でない tick）。選択の画面でなければ `Local`。
    fn menu_tick<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        let view = game.menu_view().filter(net_select::in_selection);
        self.hud_screen = view.map_or(0, |v| u8::try_from(v.screen).unwrap_or(0));
        let Some(c) = self.link.connected_mut() else {
            return Action::Local;
        };
        let Some(v) = view else {
            self.menu_retry = None;
            if self.menu_net && c.selection().is_active() {
                c.leave_selection();
                self.stats.menu_lost = self.stats.menu_lost.saturating_add(1);
            }
            self.menu_net = false;
            return Action::Local;
        };
        let seat = c.seat();
        let role = c.role();
        let entering = !c.selection().is_active();
        if entering {
            c.enter_selection();
            self.menu_net = true;
            self.decisions = Decisions::default();
            self.map_decided = false;
            self.menu_hold_run = 0;
            self.menu_retry = None;
        }
        // 選択の画面の入力ワードも試合と同じマスク（`MATCH_SEND_MASK`。Ctrl を含む）で相手へ送る。
        // Ctrl（キャラ・マップの「ランダム」。Ctrl+数字・Ctrl+Tab を含む）は落とさず、自分のゲームと相手へ届く。
        let raw = game.local_input(seat) & MATCH_SEND_MASK;
        let pressed = if entering { 0 } else { raw & !self.menu_prev };
        self.menu_prev = raw;
        let my = net_select::seat_index(seat);
        // 合わせ（難易度・マップの画面。`selection.rs`）。ゲストは、この画面の合わせが届くまで動かず、押下も要求しない（相手が来る前の動きが
        // 捨てられて、カーソルの値だけ食い違う形を無くす）。ホストは止めない。キャラ選択は合わせない（相手の席は位置で書く。下の `peer_sync`）。
        let screen8 = u8::try_from(v.screen).unwrap_or(0);
        let mute = role == Role::Guest && !c.selection().is_aligned(screen8);
        let mut press = net_select::local_press(&v, raw, pressed, my);
        // 対戦の形式の切替（Tab）: キャラセレクトの間で、両者が rollback ができる組のときだけ、今の形式の反対へ替える押下にする。ゲームの決定・取消と同じ tick なら
        // そちらを先に通す。ゲストの押下は形式だけの要求で、D と auto の旗はホストが決める。
        if std::mem::take(&mut self.tab_edge) {
            if screen8 == SCREEN_CHARA && c.rollback_able() && press.is_none() {
                press = Some(Press {
                    kind: StepKind::Format,
                    value: FormatValue::request(other_mode(c.mode())),
                    screen: screen8,
                });
                self.stats.format_asked = self.stats.format_asked.saturating_add(1);
            } else {
                self.stats.tab_ignored = self.stats.tab_ignored.saturating_add(1);
            }
        }
        let mut retried: Option<u32> = None;
        match (press, self.menu_retry) {
            (Some(_), _) | (None, None) => self.menu_retry = None,
            (None, Some((p, left))) => {
                if left == 0
                    || u32::from(p.screen) != v.screen
                    || (p.kind == StepKind::Format && value_mode(p.value) == c.mode())
                {
                    self.menu_retry = None;
                } else if !mute && net_select::reads(&v, p.kind, p.value) {
                    press = Some(net_select::repress(&v, p, my));
                    retried = Some(left);
                    self.menu_retry = None;
                    self.stats.menu_retries = self.stats.menu_retries.saturating_add(1);
                } else {
                    self.menu_retry = Some((p, left - 1));
                }
            }
        }
        if mute && let Some(p) = press.take() {
            self.menu_retry = Some((p, MENU_RETRY_TICKS));
        }
        // ホストの value: 形式の切替の STEP の D と auto の旗はホストが決める。押した tick の（押し直しなら入る tick の）ホストの値を詰める
        // （押し直しの間に数字キーで替えた D に追従する）。
        if role == Role::Host
            && let Some(p) = press
            && p.kind == StepKind::Format
        {
            press = Some(Press {
                value: c.format_value_for(value_mode(p.value)).pack(),
                ..p
            });
        }
        // ホスト: 相手が来ていて、この画面の合わせがまだなら、自分の押下より先に合わせを出す（押下は捨てられて、
        // 押し直しになる）。
        let human = press;
        let local = if mute {
            raw & !TRANSITION_BITS & !APPLY_CLEAR_BITS
        } else {
            raw & !TRANSITION_BITS
        };
        let align = if role == Role::Host
            && c.selection().align_due(screen8)
            && net_select::can_align_now(&v, local)
        {
            net_select::align_press(&v)
        } else {
            None
        };
        let pending = c.selection().next_apply();
        // ホスト: ゲストの形式の要求（形式だけ）が入れる状態で待っているとき、ホストが D と旗を決めた完全な形の押下にして出す
        // （`Selection` は要求の value をそのまま STEP にするので、ここで置き換える。ホストの押下が先にあれば、要求は step の食い違いで
        // 捨てられ、ゲストは自分の STEP の到着で要求をやめる）。
        let guest_fmt = match (role, pending) {
            (Role::Host, Some(st)) if st.kind == StepKind::Format => Some(Press {
                kind: StepKind::Format,
                value: c.format_value_for(value_mode(st.value)).pack(),
                screen: st.screen,
            }),
            _ => None,
        };
        let fmt_from_peer = guest_fmt.is_some() && align.is_none() && human.is_none();
        let press = align.or(human).or(guest_fmt);
        let ready_ok = pending.is_some_and(|st| net_select::reads_step(&v, &st));
        if pending.is_some() && !ready_ok {
            self.menu_hold_run = self.menu_hold_run.saturating_add(1);
            self.stats.menu_held = self.stats.menu_held.saturating_add(1);
            if self.menu_hold_run > MENU_HOLD_LIMIT
                && let Some(st) = pending
            {
                c.close(ByeReason::Cancelled);
                self.finish(End::MenuStuck(st));
                return Action::Local;
            }
        } else {
            self.menu_hold_run = 0;
        }
        let my_request = c.selection().pending_press();
        let (own_screen, own_cursor) = net_select::own_cursor(&v, my);
        c.set_menu_cursor(own_screen, own_cursor);
        let fo = c.selection_frame(local, press, ready_ok, net, |fo| match fo.apply {
            Some(st) if outcome_of(st.screen, st.kind) != Outcome::Stay => SelExit::Leave,
            _ => SelExit::Stay,
        });
        // 観戦の記録には相手の語をそのまま積む（観戦者は両席を語で動かす）。自分のゲームの相手の席へは、キャラ選択では方向と
        // 「ランダム」を落として入れ、カーソルは相手の位置で書く（`peer_sync`）。
        let remote_rec = if mute {
            fo.remote & MATCH_SEND_MASK & !APPLY_CLEAR_BITS
        } else {
            fo.remote & MATCH_SEND_MASK
        };
        let remote = net_select::peer_word(v.screen, remote_rec);
        if remote_rec & KEY_CTRL != 0 {
            self.stats.menu_peer_random = self.stats.menu_peer_random.saturating_add(1);
        }
        let sync = if fo.apply.is_none() {
            net_select::peer_sync(&v, my, || c.take_peer_cursor())
        } else {
            None
        };
        // 自分の押下が捨てられたか（ホスト: 押したのに入らなかった —— 前の STEP がまだ届いていない・相手が来ていない・
        // 同じ tick に合わせを出した。ゲスト: 要求していた押下（この tick の押下を含む）の代わりに、ホストの別の押下が入った）。
        let same = |p: &Press, st: &Step| {
            p.kind == st.kind && p.value == st.value && p.screen == st.screen
        };
        let lost = match role {
            Role::Host => human.filter(|p| !fo.apply.is_some_and(|st| same(p, &st))),
            Role::Guest => match fo.apply {
                Some(st) => press
                    .or(my_request)
                    .filter(|p| !my_request.is_some_and(|m| same(&m, &st)) && !same(p, &st)),
                None => None,
            },
        };
        if let Some(p) = lost {
            let left = retried.map_or(MENU_RETRY_TICKS, |l| l.saturating_sub(1));
            self.menu_retry = (left > 0).then_some((p, left));
        }
        let words = match fo.apply {
            None => net_select::normal_words(seat, local, remote),
            Some(st) => {
                let is_align = st.kind == StepKind::Align;
                let plan = if is_align {
                    net_select::align_plan(&v, &st, seat, local, remote, role == Role::Guest)
                } else {
                    net_select::apply_plan(&v, &st, seat, local, remote)
                };
                let is_fmt = st.kind == StepKind::Format;
                let own = match role {
                    Role::Host => press.is_some() && !(is_fmt && fmt_from_peer),
                    Role::Guest if is_fmt => my_request.is_some_and(|p| {
                        p.kind == StepKind::Format && value_mode(p.value) == value_mode(st.value)
                    }),
                    Role::Guest => my_request.is_some_and(|p| {
                        p.kind == st.kind && p.value == st.value && p.screen == st.screen
                    }),
                };
                if is_fmt {
                    // 対戦の形式を替える（両者が同じ STEP を同じ順で入れる）。ゲームには何も入れない。
                    Self::apply_format(c, &mut self.pick, &mut self.stats, &st, !own);
                    // 形式の押し直しはもう要らない（望みの形式になった）。ほかの押し直し（Format の STEP に追い越された決定・取消）は残す。
                    if self
                        .menu_retry
                        .is_some_and(|(p, _)| p.kind == StepKind::Format)
                    {
                        self.menu_retry = None;
                    }
                }
                match plan.cursor {
                    Some(_) => self.write_plan(game, &v, &plan),
                    None if st.kind == StepKind::Decide || (is_align && role == Role::Guest) => {
                        self.stats.menu_align_miss = self.stats.menu_align_miss.saturating_add(1);
                    }
                    None => {}
                }
                self.steps.push(StepRec { step: st, own });
                if !is_fmt {
                    // 観戦: 入った押下。この tick の `Record::Menu`（下）より先に積む。形式の切替は積まない: 観戦者のマッチは対戦者の形式に依らず
                    // 入力ワードの列だけで動くので、積むと観戦者が押下として入れてしまう。
                    Self::spec_to(&self.spec, Record::Step(st));
                }
                self.stats.menu_applied = self.stats.menu_applied.saturating_add(1);
                if st.kind == StepKind::Decide {
                    match st.screen {
                        SCREEN_DIFFICULTY => self.decisions.difficulty = Some(st.value & 3),
                        SCREEN_MAP => self.decisions.map_item = Some(st.value & 0x0F),
                        _ => {}
                    }
                }
                match outcome_of(st.screen, st.kind) {
                    Outcome::MapDecided => self.map_decided = true,
                    Outcome::LeftToMode => self.menu_net = false,
                    Outcome::Stay => {}
                }
                plan.words
            }
        };
        if let Some(s) = sync {
            self.write_peer_cursor(game, &v, s);
        }
        let [p1, p2, p3] = words;
        game.feed(0, p1);
        game.feed(1, p2);
        game.feed(2, p3);
        let (m1, m2) = by_seat(seat, local, remote_rec);
        Self::spec_to(&self.spec, Record::Menu { p1: m1, p2: m2 });
        self.stats.menu_frames = self.stats.menu_frames.saturating_add(1);
        Action::Menu
    }

    /// キャラ選択で、相手の席のカーソルを相手の位置へ書き、立ち絵を追わせ（手元で未解放のキャラでも）、相手のカーソルが動いたならゲームと同じ
    /// カーソルの音を積む。feed の前（入力の地点）に呼ぶ。
    fn write_peer_cursor<G: Game>(&mut self, game: &mut G, v: &MenuView, s: net_select::PeerSync) {
        let (off, val) = s.cursor;
        let w = write_cursor(game, v, off, val, Some(s.portrait), Locked::Allow);
        self.count_cursor_write(w);
        if !matches!(w, CursorWrite::Wrote(_)) {
            return;
        }
        self.stats.menu_peer_writes = self.stats.menu_peer_writes.saturating_add(1);
        if s.sound {
            if game.cursor_se() {
                self.stats.menu_cursor_se = self.stats.menu_cursor_se.saturating_add(1);
            } else {
                self.stats.menu_cursor_se_failed =
                    self.stats.menu_cursor_se_failed.saturating_add(1);
            }
        }
    }

    /// 形式の切替の STEP を入れた tick に、ネットワークの側の形式（`Connected::switch_format`）と遅れの選び（`DelayPick::switch_mode`）を替える。
    /// 両者が同じ STEP を同じ順で入れるので、最初の INPUT より前に同じ値で揃う。value が読めない・この組で替えられない STEP は何も変えずに数える。
    /// `by_peer` は、この切替を求めたのが相手か。
    fn apply_format(
        c: &mut Connected,
        pick: &mut Option<DelayPick>,
        stats: &mut Stats,
        st: &Step,
        by_peer: bool,
    ) {
        let Some(v) = FormatValue::unpack(st.value) else {
            stats.format_bad = stats.format_bad.saturating_add(1);
            return;
        };
        match c.switch_format(v, by_peer) {
            FormatSwitch::Refused => {
                stats.format_refused = stats.format_refused.saturating_add(1);
            }
            FormatSwitch::Changed { .. } | FormatSwitch::Reset => {
                stats.format_steps = stats.format_steps.saturating_add(1);
                if let Some(p) = pick.as_mut() {
                    p.switch_mode(v.mode);
                }
            }
        }
    }

    fn write_plan<G: Game>(&mut self, game: &mut G, v: &MenuView, plan: &net_select::ApplyPlan) {
        if let Some(w) = write_plan_cursors(game, v, plan) {
            self.count_cursor_write(w);
        }
    }

    fn count_cursor_write(&mut self, w: CursorWrite) {
        match w {
            CursorWrite::Miss => {
                self.stats.menu_align_miss = self.stats.menu_align_miss.saturating_add(1);
            }
            CursorWrite::Wrote(Some(Followed::Called)) => {
                self.stats.menu_portrait_called = self.stats.menu_portrait_called.saturating_add(1);
            }
            CursorWrite::Wrote(Some(Followed::Refused)) => {
                self.stats.menu_portrait_refused =
                    self.stats.menu_portrait_refused.saturating_add(1);
            }
            CursorWrite::Wrote(Some(Followed::Unchanged) | None) => {}
        }
    }

    /// 止める地点（今の場面 1・次の要求 2）。選択は v0 の形。
    fn hold<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        Self::feed3(game, 0, 0);
        let Some(c) = self.link.connected_mut() else {
            return Action::RenderOnly;
        };
        if !self.sel_left && self.map_decided {
            let (seat, role) = (c.seat(), c.role());
            let mut s = game.own_select(seat, role);
            if let Some(d) = self.decisions.difficulty {
                s.difficulty = d;
            }
            if let Some(m) = self.decisions.map_item {
                s.map = if role == Role::Host && m == MAP_ITEM_RANDOM_ANY {
                    net_select::pick_any_map(&game.map_list(), s.rng_seed)
                } else {
                    map_select_byte(m)
                };
            }
            c.set_local_select(&s);
            self.sel_left = true;
            self.own_charge = s.charge;
            self.own_select = Some(s);
        }
        if !self.sel_left {
            // 選択の画面を通らずに止める地点に来た（v0 の形）。`require_net_selection` の本物はここへ来ない。
            if !self.sel_entered {
                c.enter_selection();
                self.sel_entered = true;
            }
            let (seat, role) = (c.seat(), c.role());
            let press = (role == Role::Host && c.selection().peer_present()).then_some(Press {
                kind: StepKind::Decide,
                value: 0,
                screen: 0,
            });
            let mut own: Option<Select> = None;
            c.selection_frame(0, press, true, net, |fo| {
                if fo.apply.is_some() {
                    let s = game.own_select(seat, role);
                    own = Some(s);
                    SelExit::Decided(s)
                } else {
                    SelExit::Stay
                }
            });
            if let Some(s) = own {
                self.sel_left = true;
                self.own_charge = s.charge;
                self.own_select = Some(s);
            }
        }
        let agreed = c
            .selection()
            .selects_ready()
            .then(|| c.selection().agreed())
            .flatten();
        let Some(a) = agreed else {
            self.stats.hold_frames = self.stats.hold_frames.saturating_add(1);
            self.hold_run = self.hold_run.saturating_add(1);
            return Action::RenderOnly;
        };
        self.hold_run = 0;
        let peer_seat = match c.seat() {
            Seat::P1 => Seat::P2,
            Seat::P2 => Seat::P1,
        };
        if let Err(bad) = game.apply_agreed(peer_seat, &a) {
            c.close(ByeReason::Cancelled);
            self.finish(End::BadSelect(bad));
            self.stats.zero_ticks = self.stats.zero_ticks.saturating_add(1);
            return Action::Zero;
        }
        Self::spec_selects(&self.spec, c.seat(), self.own_select, &a);
        self.peer_charge = a.peer.charge;
        self.agreed = Some(a);
        self.sel_entered = false;
        self.sel_left = false;
        self.map_decided = false;
        self.menu_net = false;
        self.leave_seg();
        self.phase = Phase::Loading;
        self.stats.zero_ticks = self.stats.zero_ticks.saturating_add(1);
        Action::Zero
    }

    fn lockstep_tick<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) -> Action {
        let (cur, next) = game.scenes();
        if cur == SCENE_MATCH && REQ_REBUILD_MATCH.contains(&next) {
            self.leave_seg();
            self.phase = Phase::Loading;
            return self.zero(game);
        }
        if cur == SCENE_MATCH && next == REQ_SAVE_AND_QUIT {
            // マッチの終わりのメニューの「リプレイを保存して終了」。要求はフレーム f − 1 の更新が書くので、両者とも同じフレーム f の入口でこれを見て、それぞれ閉じる
            // （先に相手の BYE を受けていれば `leave_seg` が `PeerBye` で終え、下の `close` は終わり方を書き換えない）。
            self.leave_seg();
            self.save_quit = true;
            self.left_by_save_quit = true;
            self.close(ByeReason::Finished);
            return self.zero(game);
        }
        if cur != SCENE_MATCH || REQ_LEAVE_MATCH.contains(&next) {
            self.leave_seg();
            self.phase = Phase::Selecting;
            if cur != SCENE_MATCH {
                return Action::Local;
            }
            return self.zero(game);
        }
        if self
            .link
            .connected()
            .is_some_and(|c| c.rollback().is_some())
        {
            // rollback のモードのマッチの tick は走査の包みのフック（`rollback_scan`）が動かす。ここへ来たのはフックが走査を呼ばなかった tick で、
            // delay 方式の取得と待ちを重ねると予測で進めた分と食い違うので、マッチをやめる。
            self.rb_abort(RbAbort::Stray);
            return Action::Local;
        }
        self.capture(game, net);
        let start = net.now_us();
        // 同じ tick の始まりで待った分（`head_wait`）は予算から引く（引かないと「始まりの待ち + 予算」になり、始まりの待ちを切った実行より Present が遅れる）。
        let budget_us = self
            .cfg
            .wait_budget_us
            .saturating_sub(self.tick_head_wait_us);
        let deadline = start.saturating_add(u64::from(budget_us));
        loop {
            if self.held_local.is_some() {
                self.capture(game, net);
            }
            let now = net.now_us();
            let Some(c) = self.link.connected_mut() else {
                return Action::Local;
            };
            if let Some(a) = c.advance(ms(now), net) {
                self.timing.wait_us = now.saturating_sub(start).min(u64::from(u32::MAX)) as u32;
                return self.play(a, game, net);
            }
            if now >= deadline {
                if self.peer_finished {
                    self.finish(End::PeerBye(ByeReason::Finished));
                    return Action::Local;
                }
                self.stats.stalls = self.stats.stalls.saturating_add(1);
                self.stall_run = self.stall_run.saturating_add(1);
                self.stats.max_stall_run = self.stats.max_stall_run.max(self.stall_run);
                return Action::RenderOnly;
            }
            let left = (deadline - now).min(u64::from(lockstep::RESEND_MS) * 1_000);
            if net.wait(left as u32) {
                let over = net.now_us().saturating_sub(now).saturating_sub(left);
                self.count_wait_over(over);
            }
            self.stats.wakes = self.stats.wakes.saturating_add(1);
            self.pump(net);
            self.watch_end();
            if self.phase == Phase::Ended {
                return self.end_action();
            }
        }
    }

    /// latency の切替の予定（ホスト）。取得の前に要求すると、この取得に結び付く。rollback のモードではセッションが断る（`LatError::RollbackMode`）ので、
    /// 予定は `lat_refused` に数えて飛ばす。
    fn lat_plan_step(c: &mut Connected, cfg: &Config, lat_next: &mut usize, stats: &mut Stats) {
        while let Some((at, delay)) = cfg.lat_plan.get(*lat_next) {
            if stats.played < at {
                break;
            }
            match c.request_delay(delay) {
                Ok(()) => {
                    stats.lat_requested = stats.lat_requested.saturating_add(1);
                    *lat_next += 1;
                }
                Err(LatError::Busy) => {
                    stats.lat_busy = stats.lat_busy.saturating_add(1);
                    break;
                }
                Err(_) => {
                    stats.lat_refused = stats.lat_refused.saturating_add(1);
                    *lat_next += 1;
                }
            }
        }
    }

    fn capture<G: Game, N: Net>(&mut self, game: &mut G, net: &mut N) {
        let now = ms(net.now_us());
        self.digit_tick(game);
        let Some(c) = self.link.connected_mut() else {
            return;
        };
        if c.lockstep().captured() {
            return;
        }
        Self::lat_plan_step(c, &self.cfg, &mut self.lat_next, &mut self.stats);
        let local = match self.held_local {
            Some(w) => w,
            None => {
                self.stats.local_reads = self.stats.local_reads.saturating_add(1);
                game.local_input(c.seat()) & MATCH_SEND_MASK
            }
        };
        // 作った値（と、最初のマッチの始まりの推奨値）をネットワークの側へ渡す。ホストは次の取得（この取得）で切替に結び付け、ゲストはホストへ要求する。
        Self::deliver_pick(&mut self.pick, c, &mut self.stats);
        match c.capture(local, now, net) {
            Ok(()) | Err(CaptureError::AlreadyCaptured) => self.held_local = None,
            Err(CaptureError::Full | CaptureError::NoDelay) => self.held_local = Some(local),
        }
    }

    /// 使えたフレーム: CHECK（feed の前にゲームから読む）→ 3 本を feed。
    fn play<G: Game, N: Net>(&mut self, a: Applied, game: &mut G, net: &mut N) -> Action {
        let Some(c) = self.link.connected_mut() else {
            return Action::Local;
        };
        let seg = c.lockstep().seg();
        // 相手の入力ワードも MATCH_SEND_MASK の外を落とす（外のビットは自分の側でも効く。P/Home はゲームのフォルダへスクリーンショットを書く）。
        // 自分の入力ワードは取るときに落としてある。
        let (p1, p2) = by_seat(c.seat(), a.local, a.remote & MATCH_SEND_MASK);
        if self.cfg.check == CheckSource::InputDigest {
            self.digest = input_digest_push(self.digest, p1, p2);
        }
        let mut mismatch = None;
        if is_check_frame(a.frame) {
            let chk = match self.cfg.check {
                CheckSource::Game => {
                    let (rng_state, rng_count, life_1p, life_2p) = game.check_values();
                    Check {
                        seg,
                        frame: a.frame,
                        rng_state,
                        rng_count,
                        life_1p,
                        life_2p,
                    }
                }
                CheckSource::InputDigest => check_from_input_digest(seg, a.frame, self.digest),
            };
            if let Some(CheckResult::Mismatch { local, remote }) = c.submit_check(chk, net) {
                mismatch = Some((local, remote));
            }
        }
        Self::feed3(game, p1, p2);
        // 観戦: 使った入力ワード（seg は直前の `Record::Seg`）。
        Self::spec_to(
            &self.spec,
            Record::Frame {
                frame: a.frame,
                p1,
                p2,
            },
        );
        self.stall_run = 0;
        self.stats.played = self.stats.played.saturating_add(1);
        if a.switched_to.is_some() {
            self.stats.switches = self.stats.switches.saturating_add(1);
        }
        let now = net.now_us();
        let slot = self
            .arrivals
            .get(a.frame as usize % ARRIVAL_SLOTS)
            .copied()
            .unwrap_or(NO_ARRIVAL);
        let margin_us = (slot.seg == seg && slot.frame == a.frame)
            .then(|| now.saturating_sub(slot.t_us).min(u64::from(u32::MAX)) as u32);
        if margin_us.is_none() {
            self.stats.margin_unknown = self.stats.margin_unknown.saturating_add(1);
        }
        self.timing.delay = c.lockstep().delay();
        self.timing.margin_us = margin_us;
        self.timing.head_wait_us = self.tick_head_wait_us;
        if let Some((local, remote)) = mismatch {
            self.desync(local, remote);
        }
        Action::Played(FrameRec {
            seg,
            frame: a.frame,
            p1,
            p2,
        })
    }

    pub fn connected(&self) -> Option<&Connected> {
        self.link.connected()
    }

    /// AGREE で決まったモードと delay（rollback のモードなら D）。繋がる前は None。ゲームのスレッドから読む（値のコピーで、確保しない）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn agreed_mode(&self) -> Option<(Mode, u8)> {
        self.link
            .connected()
            .map(|c| (c.mode(), c.established().delay))
    }

    pub fn recv_counts(&self) -> link::RecvCounts {
        self.link.recv_counts()
    }

    pub fn link_path(&self) -> Option<SocketAddr> {
        self.link.path()
    }

    pub fn connect_wait_ms(&self) -> Option<u32> {
        self.connected_ms
            .map(|c| c.wrapping_sub(self.link_started_ms))
    }

    /// 今のセッションを作った時刻（`Net::now_us` の ms）。コードの外側のアドレスを STUN で得てからの古さを測る起点。
    pub fn link_started_ms(&self) -> u32 {
        self.link_started_ms
    }

    pub fn link_hello_sent(&self) -> [u32; link::KIND_SLOTS] {
        self.link.hello_sent()
    }

    /// 今のセッションが、学んだ候補（接続コードに無かった送り元）へ送った HELLO の数。
    pub fn link_hello_sent_learned(&self) -> u32 {
        self.link.hello_sent_learned()
    }

    pub fn link_path_is_learned(&self) -> bool {
        self.link.path_is_learned()
    }

    pub fn link_peer_candidates(&self) -> [u32; link::KIND_SLOTS] {
        self.link.peer_candidate_counts()
    }
}

pub(crate) mod peer_wait;
use peer_wait::{PeerWait, Verdict};
mod rollback;
pub use rollback::{RbAbort, RbScan};

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod loopback;
#[cfg(test)]
mod menu_tests;
#[cfg(test)]
mod rollback_tests;
#[cfg(test)]
mod spectate_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod watch_loopback;
#[cfg(test)]
mod watch_tests;
