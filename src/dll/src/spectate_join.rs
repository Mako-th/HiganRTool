//! 観戦者のルームの見張り（インターネット越しの観戦）: ゲーム内のメニューの観戦の「ルームマッチ」を決めた人のゲームが、観戦用のルーム
//! （`RoomName::spectate_of`）に役 guest で入り、配る側（マッチのホストの dll）が預けた観戦用のコード（種別 3）を受けて、
//! 観戦者の返し（種別 4）を answer で返し、ホストへ JOIN を送り始めさせる。
//!
//! ```text
//! 開くスレッド（script_win::open_watch_net）: 受信ソケット・STUN・受信スレッド・WatchPlay を作り、ここの Runner を起こす
//! 観戦者のルームのスレッド（higanr-spectate。ここ）: core の Joiner を 50 ms ごとに回す
//!    Connect      → RoomConn::start（観戦用のルームへ）
//!    Close        → 接続を手放す（役が逆 = 配る側がまだいない・失敗・終わり。次の入り直しは Joiner が決めてある）
//!    SpectateCode → 観戦用のコードを読み（spectate_watch_code::Reader のガード）、返し（自分の受信ソケット ＋ STUN のアドレス）を answer し、
//!                   WatchSlot へ設定を置く（WatchPlay が次の tick で取り、ホストへ JOIN を送り始める）
//!    以後         → ホストの最初の STREAM（WatchSlot::is_joined）を待つ → 観戦の終わり（WatchSlot::is_done）を待つ
//! ```
//!
//! - **ゲームのスレッドは何も呼ばない。** 確保・ブロッキング I/O は全部このスレッドと `RoomConn` の受信スレッドの側
//!   （対戦者のマッチへの遅延の増分 0）。ゲームのスレッドとの受け渡しは、指示のリングバッファと状態の入れ物（`net_ui::UiLink`）・`WatchSlot` の
//!   原子変数・`RUN`（動いているか）だけ。
//! - **状態を `UiLink` へ書く**（左上の行と項目の元。`net_menu::spectate_state_of` / `spectate_failure_event`）: 入る・配る側を
//!   待つ・つなぎに行く・つながった（`SpectateIdle` ＋ 出来事なし）・終わった（`Idle` ＋ 理由）。終わった後の状態は、
//!   観戦者のセッションを手放すゲームのスレッドが消さない（`NetUi::after_watch_teardown`）。
//! - **指示のリングバッファの読み手はこのスレッド**（観戦者のセッションがある間）: `Order::SpectateCancel` で取りやめ、それ以外は読み捨てる。
//!   対戦者の見張り（`net_code_game::GameWatcher`）は観戦者のセッションがある間は居ない（`script_win::request_spectate` が観戦者のセッションを
//!   置く前に対戦者のネットワークの組を片づける）。
//! - **ルーム名は本ログに書かない。** 観戦用のルームの名は元のルーム名のハッシュだが、書かない。`Debug` はルーム名・秘密・コード・アドレスを出さない。
//! - 返しを出してから 30 秒（`LINK_TIMEOUT_MS`）ホストの最初の STREAM が来なければ「つながらなかった」で終える
//!   （穴あけ 5 回 × 200 ms と JOIN の送り直し 250 ms で足りる長さに、NAT が厳しい相手を待ち続けない上限を足したもので、実回線では確かめていない）。
//!
//! **観戦コードの経路**（`JoinSource::Code`。メニューの「観戦コード入力」）: ルームへは入らない。開くスレッドが読んだクリップボードの観戦コード
//! （種別 3）を同じ `Reader` で読み、`WatchSlot` へ設定を置いてホストへ JOIN を送らせる。返しは出さず、ホストは穴をあけないので、ホストのポートへ
//! 外から届く場合（同じ LAN・UPnP で開けたポート）だけ繋がる。`CODE_LINK_TIMEOUT_MS` で答えが無ければ終える。
#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::candidate::Candidates;
use higanr_core::code;
use higanr_core::handshake::Name;
use higanr_core::net_menu::{
    CodeState, Event, NAME_UNITS, Note, ROOM_UNITS, sanitize_name, spectate_failure_event,
    spectate_state_of,
};
use higanr_core::room::{Event as RoomEvent, Failure, RoomName, Why};
use higanr_core::spectate_room::{Joiner, JoinerAction, JoinerFailure, JoinerPhase};

use crate::net_room::{RoomConn, RoomOptions, TransportNote};
use crate::net_ui::{Order, UiLink};
use crate::spectate_watch::WatchSlot;
use crate::spectate_watch_code::{Reader, Step};
use crate::trial_summary::Refusal;

/// 観戦者のルームのスレッドが `Joiner` を回す間隔。状態の更新と取りやめに気づくまでの最大の待ちで、
/// 見張り（`net_code_game::GAME_POLL`）と同じ 50 ms。
const POLL: Duration = Duration::from_millis(50);

/// 返しを出してから、ホストの最初の STREAM が来るまで待ってよい長さ（ms）。越えたら「つながらなかった」で終える。
pub const LINK_TIMEOUT_MS: u64 = 30_000;

/// 観戦コードの経路で、JOIN を送り始めてからホストの最初の STREAM を待つ長さ（ms）。穴あけを待たないので、届く形（LAN・開いたポート）なら
/// JOIN の送り直し（250 ms）の数回で答えが来る。届かない形を長く待たせない。
pub const CODE_LINK_TIMEOUT_MS: u64 = 10_000;

// ---------------- 数 ----------------

#[derive(Debug, Clone, Copy)]
enum Stat {
    Entered,
    /// 役 host を受けた（配る側がまだいない）ので閉じて、あとで入り直す。
    WrongRole,
    CodesReceived,
    RepliesSent,
    CodeUnreadable,
    /// 観戦用のコードは読めたが、候補がネットワークの範囲の外などで返せなかった（ガード・返しの組み立て・answer を断られた）。
    ReplyFailed,
    /// ルームの失敗: 応答しない（観戦者はこれで終わる）・混雑・満員・期限・相手が抜けた（入り直す）・ルーム名（終わる）。
    FailUnresponsive,
    FailRate,
    FailFull,
    FailExpired,
    FailPeerLeft,
    FailBadName,
    /// 30 分待っても配る側に会えなかった。
    GaveUp,
    /// 返しを出した後、ホストの最初の STREAM が来ないまま `LINK_TIMEOUT_MS` 経った。
    LinkTimeout,
    Cancelled,
    Linked,
    /// 観戦が終わった（`WatchPlay` が `Ended`。対戦者の BYE・自分の Esc・流れが切れた）。
    Ended,
    ConnectErrors,
    /// ルームの運びの記録（WinHTTP の失敗・サーバーのバイナリ）。
    Transport,
    /// 観戦者のセッションがある間に読み捨てた指示（観戦中に選ばれた観戦の「ルームマッチ」など）。
    IgnoredOrders,
    /// スレッドの中でパニックして、スレッドを捨てた。
    Panicked,
    /// 観戦コードの経路で始めた（ルームへは入らない）。
    ByCode,
    /// 観戦コードの期限は切れていたが、時計のずれを見込んで繋ぎに行った。
    ExpiredButTried,
}

const STAT_COUNT: usize = Stat::ExpiredButTried as usize + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JoinCounts {
    pub entered: u32,
    pub wrong_role: u32,
    pub codes_received: u32,
    pub replies_sent: u32,
    pub code_unreadable: u32,
    pub reply_failed: u32,
    pub fail_unresponsive: u32,
    pub fail_rate: u32,
    pub fail_full: u32,
    pub fail_expired: u32,
    pub fail_peer_left: u32,
    pub fail_bad_name: u32,
    pub gave_up: u32,
    pub link_timeout: u32,
    pub cancelled: u32,
    pub linked: u32,
    pub ended: u32,
    pub connect_errors: u32,
    pub transport: u32,
    pub ignored_orders: u32,
    pub panicked: u32,
    pub by_code: u32,
    pub expired_but_tried: u32,
}

impl JoinCounts {
    pub fn text(&self) -> String {
        let head = if self.by_code > 0 {
            format!(
                "観戦者（観戦コードでホストへ直に繋ぐ。ルームへは入らない。期限切れでも繋ぎに行った {}）",
                self.expired_but_tried
            )
        } else {
            "観戦者のルーム（観戦用のルームへ入り、対戦者の観戦用のコードを受けて返しを出す）"
                .to_string()
        };
        format!(
            "{head}: 入った {}（役が逆で閉じて入り直した {}）・観戦用のコードを受けた {}・返しを出した {}（コードを読めず {}・返せず {}）・ルームの失敗（応答なし {}・混雑 {}・満員 {}・期限 {}・相手が抜けた {}・ルーム名 {}）・30 分待って諦めた {}・つながらず終えた {}・取りやめた {}・つながった {}・観戦が終わった {}・繋げなかった {}・運びの記録 {}・読み捨てた指示 {}{}",
            self.entered,
            self.wrong_role,
            self.codes_received,
            self.replies_sent,
            self.code_unreadable,
            self.reply_failed,
            self.fail_unresponsive,
            self.fail_rate,
            self.fail_full,
            self.fail_expired,
            self.fail_peer_left,
            self.fail_bad_name,
            self.gave_up,
            self.link_timeout,
            self.cancelled,
            self.linked,
            self.ended,
            self.connect_errors,
            self.transport,
            self.ignored_orders,
            if self.panicked > 0 {
                "・パニックでこのスレッドを止めた"
            } else {
                ""
            },
        )
    }
}

/// 観戦者のルームのスレッドと、読み取りのスレッド・観戦者のセッションの片づけが共有するもの。
#[derive(Debug, Default)]
pub struct JoinShared {
    stop: AtomicBool,
    /// スレッドが最後の状態を書き終えて終わった。
    finished: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    stats: [AtomicU32; STAT_COUNT],
    /// ルームの運びの記録の文（`TransportNote` の `Debug`。WinHTTP の段階と番号・バイナリ・送り失敗だけで、ルーム名・アドレスは含まない）。
    /// 数だけでは「なぜルームへ入れなかったか」が読めないので、先頭 `NOTES_MAX` 件を本ログへ残す。
    notes: Mutex<Vec<String>>,
}

/// `JoinShared::notes` に残す文の数の上限（入り直しが続いても行を膨らませない）。
const NOTES_MAX: usize = 8;

impl JoinShared {
    fn bump(&self, s: Stat) {
        if let Some(a) = self.stats.get(s as usize) {
            a.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn get(&self, s: Stat) -> u32 {
        self.stats
            .get(s as usize)
            .map_or(0, |a| a.load(Ordering::Relaxed))
    }

    fn note_transport(&self, notes: Vec<TransportNote>) {
        let mut kept = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        for n in notes {
            self.bump(Stat::Transport);
            if kept.len() < NOTES_MAX {
                kept.push(format!("{n:?}"));
            }
        }
    }

    pub fn line(&self) -> String {
        let mut t = self.counts().text();
        let kept = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if !kept.is_empty() {
            t.push_str(&format!("・運びの記録の文 [{}]", kept.join(" / ")));
        }
        t
    }

    pub fn counts(&self) -> JoinCounts {
        let g = |s| self.get(s);
        JoinCounts {
            entered: g(Stat::Entered),
            wrong_role: g(Stat::WrongRole),
            codes_received: g(Stat::CodesReceived),
            replies_sent: g(Stat::RepliesSent),
            code_unreadable: g(Stat::CodeUnreadable),
            reply_failed: g(Stat::ReplyFailed),
            fail_unresponsive: g(Stat::FailUnresponsive),
            fail_rate: g(Stat::FailRate),
            fail_full: g(Stat::FailFull),
            fail_expired: g(Stat::FailExpired),
            fail_peer_left: g(Stat::FailPeerLeft),
            fail_bad_name: g(Stat::FailBadName),
            gave_up: g(Stat::GaveUp),
            link_timeout: g(Stat::LinkTimeout),
            cancelled: g(Stat::Cancelled),
            linked: g(Stat::Linked),
            ended: g(Stat::Ended),
            connect_errors: g(Stat::ConnectErrors),
            transport: g(Stat::Transport),
            ignored_orders: g(Stat::IgnoredOrders),
            panicked: g(Stat::Panicked),
            by_code: g(Stat::ByCode),
            expired_but_tried: g(Stat::ExpiredButTried),
        }
    }

    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// 止めて、スレッドが終わるのを待つ（最大で 1 回の見回り）。
    fn stop_and_join(&self) {
        self.stop();
        let h = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

// ---------------- 動いているスレッド（プロセスで 1 本）----------------

/// 動いているか: 0 = 起こしていない（か、片づけた）/ 1 = 動いている / 2 = 最後の状態を書き終えて終わった。
static RUN: AtomicU32 = AtomicU32::new(0);
/// 動いている（か、片づける前の）スレッドの共有。読み取りのスレッドとワーカーのスレッドが見る。ゲームのスレッドは触らない。
static ACTIVE: Mutex<Option<Arc<JoinShared>>> = Mutex::new(None);
/// 観戦者のルームのスレッドが最後の状態を書き終えたか（ゲームのスレッドが原子変数を 1 つ読むだけ）。
pub fn finished() -> bool {
    RUN.load(Ordering::Acquire) == 2
}

/// 検査だけ: 動いている印を直に置く（`tests::TEST_LOCK` の下で）。
#[cfg(test)]
pub(crate) fn set_run_for_test(v: u32) {
    RUN.store(v, Ordering::SeqCst);
}

/// 観戦者のルームのスレッドを止めて待ち、最後の数の行を返す。起こしていなければ `None`。返した行は呼び手
/// （`script_win::close_watch`）が節に入れる。ここでは控えない（控える場所が 2 つあると同じ行が 2 回並ぶ）。
pub fn stop_and_join() -> Option<String> {
    let shared = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    let line = shared.map(|sh| {
        sh.stop_and_join();
        sh.line()
    });
    RUN.store(0, Ordering::Release);
    line
}

pub fn active_line() -> Option<String> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|sh| sh.line())
}

/// 本ログの行。いま動いている（か、片づける前の）分だけ。片づけた分は `stop_and_join` が返す。起こしたことが無ければ空。
pub fn stats_text() -> String {
    active_line().map_or_else(String::new, |l| format!("\n{l}"))
}

// ---------------- 状態の書き手 ----------------

/// 見張りの状態（`Note`）をメニューとの受け渡し `UiLink` へ書く者。書き手は 1 人（開くスレッドが先に書き、続きを観戦者のルームの
/// スレッドへ渡す）。変わったときだけ書く。
pub struct Publisher {
    link: Arc<UiLink>,
    note: Note,
    last: Option<(CodeState, Event)>,
}

impl Publisher {
    /// `room` は画面に出すルーム名（`sanitize_room` を通したもの）、`since` は待ち始めた時刻（UNIX 秒の下位 32 ビット）。
    /// 状態の札はメニューとの受け渡しに残っている値の続きから数える。
    pub fn new(link: Arc<UiLink>, room: [u16; ROOM_UNITS], since: u32) -> Publisher {
        let note = Note {
            seq: link.note().map_or(0, |n| n.seq),
            room,
            since,
            ..Note::default()
        };
        Publisher {
            link,
            note,
            last: None,
        }
    }

    pub fn publish(&mut self, state: CodeState, event: Event) {
        if self.last == Some((state, event)) {
            return;
        }
        self.last = Some((state, event));
        self.note.state = state;
        self.note.event = event;
        self.note.seq = self.note.seq.wrapping_add(1);
        self.link.publish(&self.note);
    }
}

// ---------------- 回す ----------------

/// 観戦用のコードの受け取り方。
pub enum JoinSource {
    /// 観戦用のルームで、配る側が預けた観戦用のコードを受ける。`room` はルームサーバーの設定（製品は `RoomOptions::product`、
    /// 検査は loopback の偽サーバーと `Proxy::Direct`）。
    Room {
        room: RoomOptions,
        watch_room: RoomName,
    },
    /// 観戦コード: 開くスレッドがクリップボードを読んだ結果（`Clipboard::read`。文字・空・読めない）。
    Code(Result<Option<String>, String>),
}

/// 観戦者のルームのスレッドの材料。`Debug` は書かない（候補・名前を含む）。
pub struct JoinKit {
    pub source: JoinSource,
    /// 観戦用のコードを読んで `WatcherConfig` にするガード（ネットワークの範囲・観戦者の名前と nonce）。
    pub reader: Reader,
    pub reply_name: Name,
    /// 観戦者の返しに載せる候補（自分の受信ソケット ＋ STUN で見えたアドレス。IPv4 だけ —— 受信スレッドの観戦は IPv4 の受信ソケットだけ）。
    pub own: Candidates,
    pub slot: Arc<WatchSlot>,
    /// 状態を書く者（開くスレッドが先に書き始めたものの続き）。指示のリングバッファを読む側は `publisher.link`。
    pub publisher: Publisher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 観戦コードをまだ読んでいない（最初の見回りで読む）。
    Code,
    Room,
    /// ホストへ JOIN を送っている（最初の STREAM を待つ）。
    Linking,
    Watching,
    Done,
}

/// 観戦者のルームの 1 本ぶん。`step` は時刻を渡す形（検査は時計を進めて直に呼ぶ）。
pub(crate) struct Runner {
    kit: JoinKit,
    joiner: Joiner,
    conn: Option<RoomConn>,
    phase: Phase,
    /// 返しを出した時刻（`LINK_TIMEOUT_MS` の起点。単調な時計の ms）。
    replied_at: Option<u64>,
    shared: Arc<JoinShared>,
    t0: Instant,
    /// このスレッドを起こした観戦の「ルームマッチ」の指示を、もう 1 回読み捨てたか（起こした指示は数えない）。
    start_order_seen: bool,
    /// JOIN を送り始めてから最初の STREAM を待つ長さ（ms）と、来なかったときの出来事（ルームの経路と観戦コードの経路で違う）。
    link_limit_ms: u64,
    link_fail: Event,
}

impl Runner {
    pub(crate) fn new(kit: JoinKit, shared: Arc<JoinShared>) -> Runner {
        let phase = match kit.source {
            JoinSource::Room { .. } => Phase::Room,
            JoinSource::Code(_) => Phase::Code,
        };
        Runner {
            kit,
            joiner: Joiner::new(0),
            conn: None,
            phase,
            replied_at: None,
            shared,
            t0: Instant::now(),
            start_order_seen: false,
            link_limit_ms: LINK_TIMEOUT_MS,
            link_fail: spectate_failure_event(JoinerFailure::ReplyNotSent).0,
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    fn run(&mut self) {
        while !self.is_done() && !self.shared.stopped() {
            let now = u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.step(now, crate::net_udp::now_unix());
            std::thread::sleep(POLL);
        }
        self.close();
    }

    pub(crate) fn close(&mut self) {
        if let Some(mut c) = self.conn.take() {
            c.cancel();
        }
    }

    fn bump(&self, s: Stat) {
        self.shared.bump(s);
    }

    /// 1 回の見回り。`now_ms` は単調な時計（この機械の始まりから）、`unix` は UNIX 秒（コードの期限）。止まらない。
    pub(crate) fn step(&mut self, now_ms: u64, unix: u64) {
        if self.is_done() {
            return;
        }
        // 指示: 観戦の取りやめ。観戦が始まった後（`Watching`）の取りやめは受けない（メニューの項目は出さない）。
        while let Some(o) = self.kit.publisher.link.take_order() {
            if o == Order::SpectateCancel && self.phase != Phase::Watching {
                return self.cancel();
            }
            if o == Order::Spectate && !self.start_order_seen {
                self.start_order_seen = true;
                continue;
            }
            self.bump(Stat::IgnoredOrders);
        }
        // ホストの最初の STREAM は、ルームの手順の途中（1000 より前）に来ることもある。来たらルームの手順は要らない。
        if matches!(self.phase, Phase::Room | Phase::Linking) && self.kit.slot.is_joined() {
            return self.start_watching();
        }
        match self.phase {
            Phase::Code => self.step_code(now_ms, unix),
            Phase::Room => self.step_room(now_ms, unix),
            Phase::Linking => self.step_linking(now_ms),
            Phase::Watching => self.step_watching(),
            Phase::Done => {}
        }
    }

    fn step_room(&mut self, now_ms: u64, unix: u64) {
        match self.joiner.poll(now_ms) {
            Some(JoinerAction::Connect) => self.connect(now_ms),
            Some(JoinerAction::Close) => self.close(),
            _ => {}
        }
        let (evs, notes) = match self.conn.as_mut() {
            Some(c) => (c.poll(), c.take_notes()),
            None => (Vec::new(), Vec::new()),
        };
        self.shared.note_transport(notes);
        for e in evs {
            if self.conn.is_none() {
                // 手放した接続の残りの出来事は捨てる（`spectate_room` の使い方）。
                break;
            }
            self.on_event(e, now_ms, unix);
        }
        if self.conn.as_ref().is_some_and(RoomConn::is_finished) {
            self.conn = None;
        }
        if self.link_timed_out(now_ms) {
            return self.fail_link(Stat::LinkTimeout);
        }
        self.refresh(now_ms);
    }

    fn step_linking(&mut self, now_ms: u64) {
        if self.link_timed_out(now_ms) {
            return self.fail_link(Stat::LinkTimeout);
        }
        if self.kit.slot.is_done() {
            // 繋がらないまま `WatchPlay` が終わった（検査・想定外の終わり）。
            return self.fail_link(Stat::LinkTimeout);
        }
        self.kit
            .publisher
            .publish(CodeState::SpectateLinking, Event::SpectatePeer);
    }

    fn step_watching(&mut self) {
        if self.kit.slot.is_done() {
            self.bump(Stat::Ended);
            self.kit
                .publisher
                .publish(CodeState::Idle, Event::SpectateEnded);
            self.phase = Phase::Done;
        }
    }

    fn link_timed_out(&self, now_ms: u64) -> bool {
        self.replied_at
            .is_some_and(|t| now_ms.saturating_sub(t) >= self.link_limit_ms)
    }

    /// 返しを出した後ホストの最初の STREAM が来ないまま終える: `WatchPlay` を終わらせ（`give_up`）、理由を状態に書く。
    fn fail_link(&mut self, why: Stat) {
        self.bump(why);
        self.close();
        self.kit.slot.give_up();
        self.kit.publisher.publish(CodeState::Idle, self.link_fail);
        self.phase = Phase::Done;
    }

    /// 観戦コードを読み、読めたら `WatchSlot` へ設定を置いて JOIN を送らせる（返しは出さない）。読めなければ理由を状態に書いて終える。
    fn step_code(&mut self, now_ms: u64, unix: u64) {
        self.bump(Stat::ByCode);
        let read = match &self.kit.source {
            JoinSource::Code(r) => r.clone(),
            JoinSource::Room { .. } => Ok(None),
        };
        let text = match read {
            Ok(Some(t)) => t,
            Ok(None) => return self.code_refused(Event::SpectateCodeEmpty, Stat::CodeUnreadable),
            Err(_) => return self.code_refused(Event::Unreadable, Stat::CodeUnreadable),
        };
        self.bump(Stat::CodesReceived);
        let trimmed = text.trim().trim_start_matches('\u{FEFF}');
        let bytes = trimmed.as_bytes();
        // 期限は書いた側の時計で決まり、読む側の時計で比べる。LAN の会場の PC は時計がずれていることがあるので、期限の切れた観戦コードでも
        // 繋ぎに行く（ホストが同じ観戦のホストを動かしている間だけ繋がる。次のつながりのホストは session と秘密が違うので断る）。
        let (step, expired) = match self.kit.reader.clone().on_peer_file(Some(bytes), unix) {
            Step::Refused(_, _, Some(Refusal::Expired)) => {
                (self.kit.reader.clone().on_peer_file(Some(bytes), 0), true)
            }
            s => (s, false),
        };
        let cfg = match step {
            Step::Ready(cfg, _, _) => cfg,
            Step::Refused(_, _, why) => {
                let ev = match why {
                    None => Event::SpectateCodeEmpty,
                    Some(Refusal::Unreadable) => Event::Unreadable,
                    Some(Refusal::WrongKind) => Event::SpectateCodeWrongKind,
                    Some(Refusal::Expired) => Event::SpectateCodeExpired,
                    Some(Refusal::NoRoute) => Event::NoRoute,
                    Some(Refusal::Internal) => Event::Internal,
                    Some(Refusal::OtherVersion { theirs }) => {
                        self.kit.publisher.note.peer_ver = theirs;
                        Event::SpectateOtherVersion
                    }
                    Some(Refusal::OtherBuild { theirs }) => {
                        self.kit.publisher.note.peer_build = theirs;
                        Event::SpectateOtherBuild
                    }
                    Some(
                        Refusal::Broken
                        | Refusal::OwnCode
                        | Refusal::BothOffer
                        | Refusal::BothAnswer
                        | Refusal::WrongSession,
                    ) => Event::SpectateCodeBroken,
                };
                return self.code_refused(ev, Stat::CodeUnreadable);
            }
            Step::Same => return self.code_refused(Event::Internal, Stat::CodeUnreadable),
        };
        if expired {
            self.bump(Stat::ExpiredButTried);
        }
        let name = code::read_spectate(trimmed, 0).map_or(Name::EMPTY, |s| s.code.name);
        self.kit.slot.offer(cfg.0);
        self.kit.publisher.note.name = sanitize_name(name.as_bytes(), glyphs_has);
        self.link_limit_ms = CODE_LINK_TIMEOUT_MS;
        self.link_fail = if expired {
            Event::SpectateCodeExpired
        } else {
            Event::SpectateCodeNoLink
        };
        self.replied_at = Some(now_ms);
        self.phase = Phase::Linking;
        self.kit
            .publisher
            .publish(CodeState::SpectateLinking, Event::SpectatePeer);
    }

    fn code_refused(&mut self, ev: Event, stat: Stat) {
        self.bump(stat);
        self.kit.slot.give_up();
        self.kit.publisher.publish(CodeState::Idle, ev);
        self.phase = Phase::Done;
    }

    /// ホストの最初の STREAM を受けた: ルームの手順を閉じ、観戦の段階へ。メニューは「観戦中」（`SpectateIdle` ＋ 出来事なし）。
    fn start_watching(&mut self) {
        let _ = self.joiner.stop();
        self.close();
        self.bump(Stat::Linked);
        self.kit
            .publisher
            .publish(CodeState::SpectateIdle, Event::None);
        self.phase = Phase::Watching;
    }

    fn cancel(&mut self) {
        let _ = self.joiner.stop();
        self.close();
        self.bump(Stat::Cancelled);
        self.kit.slot.give_up();
        self.kit
            .publisher
            .publish(CodeState::Idle, Event::SpectateCancelled);
        self.phase = Phase::Done;
    }

    fn connect(&mut self, now_ms: u64) {
        let started = match &self.kit.source {
            JoinSource::Room { room, watch_room } => RoomConn::start(room, watch_room).ok(),
            JoinSource::Code(_) => None,
        };
        match started {
            Some(c) => self.conn = Some(c),
            None => {
                // スレッドを起こせない。`Joiner` に「繋げなかった」の入口が無いので、応答しない扱い（観戦者は失敗で終わる）。
                self.bump(Stat::ConnectErrors);
                self.joiner
                    .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
            }
        }
    }

    fn on_event(&mut self, e: RoomEvent, now_ms: u64, unix: u64) {
        match e {
            RoomEvent::Joined { role, .. } => {
                self.bump(Stat::Entered);
                if let Some(JoinerAction::Close) = self.joiner.on_joined(now_ms, role) {
                    // 役 host: 配る側がまだいない。閉じて、12 秒後に入り直す。
                    self.bump(Stat::WrongRole);
                    self.close();
                }
            }
            RoomEvent::Matched { .. } => {}
            RoomEvent::PeerCode(text) => {
                if let Some(JoinerAction::SpectateCode(text)) = self.joiner.on_peer_code(text) {
                    self.on_spectate_code(&text, unix);
                }
            }
            RoomEvent::Done => {
                self.joiner.on_done();
                self.close();
            }
            RoomEvent::Failed(f) => {
                self.bump(match f {
                    Failure::Unresponsive(_) => Stat::FailUnresponsive,
                    Failure::RateLimited { .. } => Stat::FailRate,
                    Failure::RoomFull => Stat::FailFull,
                    Failure::Expired => Stat::FailExpired,
                    Failure::PeerLeft => Stat::FailPeerLeft,
                    Failure::BadRoomName => Stat::FailBadName,
                });
                self.joiner.on_failed(now_ms, f);
                self.close();
            }
        }
    }

    /// 観戦用のコードを受けた: 読み、返しを answer で出し、`WatchSlot` へ設定を置く。読めない・返せないなら
    /// `Joiner::abort`（失敗で終わる）。
    fn on_spectate_code(&mut self, text: &str, unix: u64) {
        self.bump(Stat::CodesReceived);
        let spec = match code::read_spectate(text, unix) {
            Ok(c) => c,
            Err(e) => return self.abort(JoinerFailure::CodeUnreadable(e), Stat::CodeUnreadable),
        };
        // ガードと `WatcherConfig` の組み立ては、ファイルの経路の観戦者と同じ `Reader`。同じ文字がもう一度来ても `Same` で
        // 断らないよう、読むたびに複製で読む。
        let cfg = match self
            .kit
            .reader
            .clone()
            .on_peer_file(Some(text.as_bytes()), unix)
        {
            Step::Ready(cfg, _, _) => cfg,
            Step::Refused(..) | Step::Same => {
                return self.abort(JoinerFailure::ReplyNotSent, Stat::ReplyFailed);
            }
        };
        let reply = match code::write_spectate_reply_to(
            &spec.code,
            self.kit.reply_name,
            self.kit.own,
            unix,
        ) {
            Ok(r) => r,
            Err(_) => return self.abort(JoinerFailure::ReplyNotSent, Stat::ReplyFailed),
        };
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        if conn.submit_code(&reply).is_err() {
            return self.abort(JoinerFailure::ReplyNotSent, Stat::ReplyFailed);
        }
        self.joiner.on_answer_sent();
        self.bump(Stat::RepliesSent);
        // 返しを出したら、すぐ JOIN を始めさせる（JOIN は 250 ms ごとに送り直すので、ホストの穴あけの後の JOIN が届く。
        // 自分の NAT にもホストへの穴ができる）。
        self.kit.slot.offer(cfg.0);
        self.kit.publisher.note.name = sanitize_name(spec.code.name.as_bytes(), glyphs_has);
        self.replied_at = Some(u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX));
    }

    fn abort(&mut self, why: JoinerFailure, stat: Stat) {
        self.bump(stat);
        if let Some(JoinerAction::Close) = self.joiner.abort(why) {
            self.close();
        }
    }

    fn refresh(&mut self, now_ms: u64) {
        let st = self.joiner.status(now_ms);
        match st.phase {
            JoinerPhase::Failed(f) => self.fail(f),
            JoinerPhase::Stopped => {}
            JoinerPhase::Succeeded => {
                self.phase = Phase::Linking;
                self.kit
                    .publisher
                    .publish(CodeState::SpectateLinking, Event::SpectatePeer);
            }
            _ => {
                let state = spectate_state_of(&st);
                let event = match state {
                    CodeState::SpectateWaiting => Event::RoomWaiting,
                    CodeState::SpectateLinking => Event::SpectatePeer,
                    _ => Event::RoomJoining,
                };
                self.kit.publisher.publish(state, event);
            }
        }
    }

    /// 失敗で終える: 理由を状態に書き、`WatchPlay` を終わらせる。数は他で数えてあるので、ここで数えるのは 30 分の諦めだけ。
    fn fail(&mut self, why: JoinerFailure) {
        if why == JoinerFailure::GaveUp {
            self.bump(Stat::GaveUp);
        }
        let (ev, ver, build) = spectate_failure_event(why);
        self.close();
        self.kit.slot.give_up();
        self.kit.publisher.note.peer_ver = ver;
        self.kit.publisher.note.peer_build = build;
        self.kit.publisher.publish(CodeState::Idle, ev);
        self.phase = Phase::Done;
    }
}

/// 文字の表（相手の名前の文字を `?` に直す）。`ui_glyphs` は小さい順の表で、毎回引くと確保するので 1 度だけ作る。
fn glyphs_has(c: char) -> bool {
    use std::sync::OnceLock;
    static GLYPHS: OnceLock<Vec<char>> = OnceLock::new();
    GLYPHS
        .get_or_init(higanr_core::overlay::ui_glyphs)
        .binary_search(&c)
        .is_ok()
}

const _: () = assert!(NAME_UNITS >= 1);

pub fn spawn(kit: JoinKit) -> Result<Arc<JoinShared>, String> {
    let shared = Arc::new(JoinShared::default());
    let sh = Arc::clone(&shared);
    let mut runner = Runner::new(kit, Arc::clone(&shared));
    // 先に印を置く（スレッドが速く終わって `finished` を書いた後に、起こした側が 1 に戻さない）。
    RUN.store(1, Ordering::Release);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&shared));
    let spawned = std::thread::Builder::new()
        .name("higanr-spectate".into())
        .spawn(move || {
            // パニックは受けて外へ出さない。受けたら観戦者のセッションを手放せる形にする（`WatchPlay` を終わらせ、状態は「内部の不具合」）。
            let r = catch_unwind(AssertUnwindSafe(|| runner.run()));
            if r.is_err() {
                sh.bump(Stat::Panicked);
                runner.kit.slot.give_up();
                runner
                    .kit
                    .publisher
                    .publish(CodeState::Idle, Event::Internal);
            }
            sh.finished.store(true, Ordering::Release);
            RUN.store(2, Ordering::Release);
        });
    match spawned {
        Ok(h) => {
            *shared.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
            Ok(shared)
        }
        Err(e) => {
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            RUN.store(0, Ordering::Release);
            Err(format!("観戦者のルームのスレッドを起こせない: {e}"))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
