//! 観戦者のルームを監視する処理（インターネット越しの観戦）。
//! ゲーム内のメニューで観戦の「ルームマッチ」を選択した人のゲームが、観戦用のルーム（`RoomName::spectate_of`）に役guestで入る。
//! 配布側（マッチのホストのdll）が登録した観戦用のコード（種別3）を受け取り、観戦者の返答（種別4）をanswerで返して、ホストへJOINを送り始めさせる。
//!
//! - 開くスレッド（`script_win::open_watch_net`）が受信ソケット・STUN・受信スレッド・`WatchPlay`を作り、ここの`Runner`を作る。
//!   観戦者のルームのスレッド（`higanr-spectate`。ここ）はcoreの`Joiner`を50 msごとに回し、次を受け持つ。
//!   観戦用のルームへの接続と、観戦用のコードの読み（`spectate_watch_code::Reader`のガード）。
//!   返答（自分の受信ソケット ＋ STUNのアドレス）のanswer。
//!   `WatchSlot`への設定の書き込み（`WatchPlay`が次のtickで取り、ホストへJOINを送り始める）。
//!   ホストの最初のSTREAM（`WatchSlot::is_joined`）と観戦の終わり（`WatchSlot::is_done`）の待ち。
//! - **ゲームのスレッドは何も呼ばない。** 確保・ブロッキングI/Oは全部このスレッドと`RoomConn`の受信スレッドの側で行う（相手のマッチへの遅延の増分は0）。
//!   ゲームのスレッドとの受け渡しは、指示のリングバッファ・状態のバッファ（`net_ui::UiLink`）・`WatchSlot`のアトミック変数・`RUN`（動いているかどうか）だけ。
//! - **状態を`UiLink`へ書く**（左上の行と項目の元）。
//!   終わった後の状態は、観戦者のセッションを手放すゲームのスレッドが消さない（`NetUi::after_watch_teardown`）。
//!   指示のリングバッファの読み手はこのスレッド。`Order::SpectateCancel`なら取りやめ、それ以外は読み捨てる。
//! - **ルーム名は本ログに書かない。** `Debug` はルーム名・秘密の乱数・コード・アドレスを出さない。
//!   返答を出してから30秒（`LINK_TIMEOUT_MS`）の間にホストの最初のSTREAMが来なければ「接続できなかった」で終える。
//!   この長さは、穴あけ5回 × 200 msとJOINの送り直し250 msで足りる長さに、NATが厳しい相手を待ち続けない上限を足したもの。実回線では検証していない。
//!
//! **観戦コードの経路**（`JoinSource::Code`。メニューの「観戦コード入力」）: ルームへは入らない。
//! 開くスレッドがクリップボードから読んだ観戦コード（種別3）を同じ`Reader`で読み、`WatchSlot`へ設定を書いてホストへJOINを送らせる。
//! 返答は出さず、ホストも穴をあけない。そのためホストのポートへ外から届く場合（同じLAN・UPnPで開けたポート）だけ接続できる。
//! `CODE_LINK_TIMEOUT_MS`で答えが無ければ終える。
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

/// 観戦者のルームのスレッドが`Joiner`を回す間隔。状態の更新と取りやめに気づくまでの最大の待ち時間。
/// 監視の処理（`net_code_game::GAME_POLL`）と同じ50 ms。
const POLL: Duration = Duration::from_millis(50);

/// 返答を出してからホストの最初のSTREAMが来るまで待ってよい長さ（ms）。越えたら「接続できなかった」で終える。
pub const LINK_TIMEOUT_MS: u64 = 30_000;

/// 観戦コードの経路でJOINを送り始めてからホストの最初のSTREAMを待つ長さ（ms）。
/// 穴あけを待たないので、届く形（LAN・開いたポート）ならJOINの送り直し（250 ms）の数回で答えが来る。
/// 届かない形を長く待たせない。
pub const CODE_LINK_TIMEOUT_MS: u64 = 10_000;

// ---------------- 数 ----------------

#[derive(Debug, Clone, Copy)]
enum Stat {
    Entered,
    /// 役hostを受けた（配布側がまだいない）ので閉じて、あとで入り直す。
    WrongRole,
    CodesReceived,
    RepliesSent,
    CodeUnreadable,
    /// 観戦用のコードは読めたが、候補がネットワークの範囲の外などで返答できなかった（ガード・返答の組み立て・answerを断られた）。
    ReplyFailed,
    /// ルームの失敗: 応答しない（観戦者はこれで終わる）・混雑・満員・期限・相手が抜けた（入り直す）・ルーム名（終わる）。
    FailUnresponsive,
    FailRate,
    FailFull,
    FailExpired,
    FailPeerLeft,
    FailBadName,
    /// 配布側が観戦を断っていた（4403）。観戦者はこれで終わる。
    FailRefused,
    /// 30分待っても配布側に会えなかった。
    GaveUp,
    /// 返答を出した後ホストの最初のSTREAMが来ないまま`LINK_TIMEOUT_MS`経った。
    LinkTimeout,
    Cancelled,
    Linked,
    /// 観戦が終わった（`WatchPlay`が`Ended`。相手のBYE・自分のEsc・ストリームが切れた）。
    Ended,
    ConnectErrors,
    /// ルームの通信の記録（WinHTTPの失敗・サーバーのバイナリ）。
    Transport,
    /// 観戦者のセッションがある間に読み捨てた指示（観戦中に選ばれた観戦の「ルームマッチ」など）。
    IgnoredOrders,
    /// スレッドの中でパニックしてスレッドを捨てた。
    Panicked,
    /// 観戦コードの経路で始めた（ルームへは入らない）。
    ByCode,
    /// 観戦コードの期限は切れていた。時計のずれを見込んで接続しに行った。
    ExpiredButTried,
    /// `Linked`のうち最初のSTREAMがIPv6の送信元から来たもの。
    LinkedV6,
    /// `LinkTimeout`のうちホストから何の返事も無かったもの（ホストに届かず観戦できなかった）。
    Unreached,
    /// `LinkTimeout`のうちホストに満員で断られ続けたもの。
    FullBeforeJoin,
}

const STAT_COUNT: usize = Stat::FullBeforeJoin as usize + 1;

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
    pub fail_refused: u32,
    pub gave_up: u32,
    pub link_timeout: u32,
    /// `link_timeout`のうちホストから何の返事も無かった数と満員で断られ続けた数。
    pub unreached: u32,
    pub full_before_join: u32,
    pub cancelled: u32,
    pub linked: u32,
    /// `linked`のうちIPv6の経路で接続した数（残りはIPv4）。
    pub linked_v6: u32,
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
                "観戦者（観戦コードでホストへ直に接続する。ルームへは入らない。期限切れでも接続しに行った {}）",
                self.expired_but_tried
            )
        } else {
            "観戦者のルーム（観戦用のルームへ入り、相手の観戦コードを受けて返答コードを出す）"
                .to_string()
        };
        let mut t = format!(
            "{head}: 入った {}（役が逆で閉じて入り直した {}）・観戦コードを受けた {}・返答コードを出した {}（コードを読めず {}・返せず {}）・ルームの失敗（応答なし {}・混雑 {}・満員 {}・期限 {}・相手が抜けた {}・ルーム名 {}・観戦拒否 {}）・30 分待って諦めた {}・接続できずに終えた {}・取りやめた {}・接続した {}（うち IPv6 のアドレスで接続 {}）・観戦が終わった {}・接続できなかった {}・運びの記録 {}・読み捨てた指示 {}{}",
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
            self.fail_refused,
            self.gave_up,
            self.link_timeout,
            self.cancelled,
            self.linked,
            self.linked_v6,
            self.ended,
            self.connect_errors,
            self.transport,
            self.ignored_orders,
            if self.panicked > 0 {
                "・パニックでこの処理を停止"
            } else {
                ""
            },
        );
        // サーバーの中継を作るかを決める材料（ホストに直接届かない観戦者がどれだけいるか）。
        if self.unreached > 0 {
            t.push_str(&format!(
                "\n観戦者: ホストに届かず観戦できなかった {} 回（JOIN にホストの返事なし）",
                self.unreached
            ));
        }
        if self.full_before_join > 0 {
            t.push_str(&format!(
                "\n観戦者: ホストの観戦者が満員で入れなかった {} 回（JOIN に満員の BYE）",
                self.full_before_join
            ));
        }
        t
    }
}

/// 観戦者のルームのスレッド・読み取りのスレッド・観戦者のセッションの後始末が共有するもの。
#[derive(Debug, Default)]
pub struct JoinShared {
    stop: AtomicBool,
    /// スレッドが最後の状態を書き終えて終わった。
    finished: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    stats: [AtomicU32; STAT_COUNT],
    /// ルームの通信の記録の文（`TransportNote`の`Debug`。WinHTTPの段階と番号・バイナリ・送信失敗だけで、ルーム名・アドレスは含まない）。
    /// 数だけでは「なぜルームへ入れなかったか」が読めないので先頭`NOTES_MAX`件を本ログへ残す。
    notes: Mutex<Vec<String>>,
}

/// `JoinShared::notes`に残す文の数の上限（入り直しが続いても行を膨らませない）。
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
            fail_refused: g(Stat::FailRefused),
            gave_up: g(Stat::GaveUp),
            link_timeout: g(Stat::LinkTimeout),
            unreached: g(Stat::Unreached),
            full_before_join: g(Stat::FullBeforeJoin),
            cancelled: g(Stat::Cancelled),
            linked: g(Stat::Linked),
            linked_v6: g(Stat::LinkedV6),
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

    /// 止めてスレッドが終わるのを待つ（最大でポーリング1回分）。
    fn stop_and_join(&self) {
        self.stop();
        let h = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

// ---------------- 動いているスレッド（プロセスで1本）----------------

/// 動作の状態: 0 = 作っていない（または後始末した）/ 1 = 動いている / 2 = 最後の状態を書き終えて終わった。
static RUN: AtomicU32 = AtomicU32::new(0);
/// 動いている（か後始末する前の）スレッドの共有。読み取りのスレッドとワーカーのスレッドが見る。ゲームのスレッドは触らない。
static ACTIVE: Mutex<Option<Arc<JoinShared>>> = Mutex::new(None);
/// 観戦者のルームのスレッドが最後の状態を書き終えたかどうか（ゲームのスレッドがアトミック変数を1つ読むだけ）。
pub fn finished() -> bool {
    RUN.load(Ordering::Acquire) == 2
}

/// 検査だけ: 動いているフラグを直接設定する（`tests::TEST_LOCK`の下で）。
#[cfg(test)]
pub(crate) fn set_run_for_test(v: u32) {
    RUN.store(v, Ordering::SeqCst);
}

/// 観戦者のルームのスレッドを止めて待ち、最後の集計の行を返す。作っていなければ`None`。
/// 返した行は呼び出し元（`script_win::close_watch`）がセクションへ追加する。ここでは保存しない（保存する場所が2つあると同じ行が2回並ぶ）。
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

/// 本ログの行。いま動いている（か後始末する前の）分だけ。後始末した分は`stop_and_join`が返す。作ったことが無ければ空。
pub fn stats_text() -> String {
    active_line().map_or_else(String::new, |l| format!("\n{l}"))
}

// ---------------- 状態の書き手 ----------------

/// 監視の処理の状態（`Note`）をメニューとの受け渡し`UiLink`へ書く者。
/// 書き手は1人（開くスレッドが先に書き始め、続きを観戦者のルームのスレッドへ渡す）。変わったときだけ書く。
pub struct Publisher {
    link: Arc<UiLink>,
    note: Note,
    last: Option<(CodeState, Event)>,
}

impl Publisher {
    /// `room`は画面に出すルーム名（`sanitize_room`を通したもの）。`since`は待ち始めた時刻（UNIX秒の下位32ビット）。
    /// 状態のタグはメニューとの受け渡しに残っている値の続きから数える。
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
    /// 観戦用のルームで、配布側が登録した観戦用のコードを受け取る。
    /// `room`はルームサーバーの設定。製品は`RoomOptions::product`、検査はloopbackの偽サーバーと`Proxy::Direct`。
    Room {
        room: RoomOptions,
        watch_room: RoomName,
    },
    /// 観戦コード: 開くスレッドがクリップボードを読んだ結果（`Clipboard::read`。文字・空・読めない）。
    Code(Result<Option<String>, String>),
}

impl JoinSource {
    /// 中継の配布側（約9.4 MB）を確保する経路かどうか。ルームの経路だけ。
    pub fn can_relay(&self) -> bool {
        matches!(self, JoinSource::Room { .. })
    }
}

/// 観戦者のルームのスレッドの材料。`Debug`は書かない（候補・名前を含む）。
pub struct JoinKit {
    pub source: JoinSource,
    /// 観戦用のコードを読んで`WatcherConfig`にするガード（ネットワークの範囲・観戦者の名前とnonce）。
    pub reader: Reader,
    pub reply_name: Name,
    /// 観戦者の返答に載せる候補（自分の受信ソケット ＋ STUNで見えたアドレス ＋ 開けたIPv6のソケット）。
    pub own: Candidates,
    pub slot: Arc<WatchSlot>,
    /// 状態を書く者（開くスレッドが先に書き始めたものの続き）。指示のリングバッファを読む側は`publisher.link`。
    pub publisher: Publisher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 観戦コードをまだ読んでいない（最初のポーリングで読む）。
    Code,
    Room,
    /// ホストへJOINを送っている（最初のSTREAMを待つ）。
    Linking,
    Watching,
    Done,
}

/// 観戦者のルーム1つ分の処理。`step`は時刻を引数で渡す形（検査は時計を進めて直接呼ぶ）。
pub(crate) struct Runner {
    kit: JoinKit,
    joiner: Joiner,
    conn: Option<RoomConn>,
    phase: Phase,
    /// 返答を出した時刻（`LINK_TIMEOUT_MS`の起点。単調な時計のms）。
    replied_at: Option<u64>,
    shared: Arc<JoinShared>,
    t0: Instant,
    /// このスレッドを作った観戦の「ルームマッチ」の指示をもう1回読み捨てたかどうか（作った指示は数えない）。
    start_order_seen: bool,
    /// JOINを送り始めてから最初のSTREAMを待つ長さ（ms）と来なかったときのイベント（ルームの経路と観戦コードの経路で違う）。
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

    /// ポーリング1回分の処理。`now_ms`は単調な時計（このステートマシンの開始から）。`unix`はUNIX秒（コードの期限）。止まらない。
    pub(crate) fn step(&mut self, now_ms: u64, unix: u64) {
        if self.is_done() {
            return;
        }
        // 指示は観戦の取りやめ。観戦が始まった後（`Watching`）の取りやめは受け付けない（メニューに項目を出さない）。
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
        // ホストの最初のSTREAMはルームの手順の途中（1000より前）に来ることもある。来たらルームの手順は要らない。
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
                // 手放した接続の残りのイベントは捨てる（`spectate_room`の使い方）。
                break;
            }
            self.on_event(e, now_ms, unix);
        }
        if self.conn.as_ref().is_some_and(RoomConn::is_finished) {
            self.conn = None;
        }
        if self.link_timed_out(now_ms) {
            return self.time_out();
        }
        self.refresh(now_ms);
    }

    fn step_linking(&mut self, now_ms: u64) {
        if self.link_timed_out(now_ms) {
            return self.time_out();
        }
        if self.kit.slot.is_done() {
            // 接続できないまま`WatchPlay`が終わった（検査・想定外の終わり）。
            return self.fail_link(Stat::LinkTimeout);
        }
        self.kit
            .publisher
            .publish(CodeState::SpectateLinking, Event::SpectatePeer);
    }

    fn step_watching(&mut self) {
        if self.kit.slot.is_done() {
            self.bump(Stat::Ended);
            let ev = if self.kit.slot.buffer_full() {
                Event::SpectateBufferFull
            } else if self.kit.slot.refused_full() {
                Event::SpectateFullEnded
            } else {
                Event::SpectateEnded
            };
            self.kit.publisher.publish(CodeState::Idle, ev);
            self.phase = Phase::Done;
        }
    }

    /// JOINを送り始めてからホストの最初のSTREAMが来ないまま待ちの上限が来た。満員の断りを受けていたかどうかで分けて数える。
    fn time_out(&mut self) {
        let full = self.kit.slot.full_before_join();
        self.bump(if full {
            Stat::FullBeforeJoin
        } else {
            Stat::Unreached
        });
        if full {
            self.link_fail = match self.kit.source {
                JoinSource::Code(_) => Event::SpectateCodeFull,
                JoinSource::Room { .. } => Event::SpectateFull,
            };
        }
        self.fail_link(Stat::LinkTimeout);
    }

    fn link_timed_out(&self, now_ms: u64) -> bool {
        self.replied_at
            .is_some_and(|t| now_ms.saturating_sub(t) >= self.link_limit_ms)
    }

    /// 返答を出した後ホストの最初のSTREAMが来ないまま終える: `WatchPlay`を終わらせ（`give_up`）、理由を状態に書く。
    fn fail_link(&mut self, why: Stat) {
        self.bump(why);
        self.close();
        self.kit.slot.give_up();
        self.kit.publisher.publish(CodeState::Idle, self.link_fail);
        self.phase = Phase::Done;
    }

    /// 観戦コードを読み、読めたら`WatchSlot`へ設定を書いてJOINを送らせる（返答は出さない）。読めなければ理由を状態に書いて終える。
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
        // 期限は書いた側の時計で決まり、読む側の時計で比べる。
        // LANの会場のPCは時計がずれていることがあるので、期限の切れた観戦コードでも接続しに行く。
        // 同じ観戦のホストが動いている間だけ接続できる（次の接続はsessionと秘密の乱数が違う別の観戦のホストなので断る）。
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

    /// ホストの最初のSTREAMを受けた: ルームの手順を閉じて観戦の段階へ移る。メニューは「観戦中」（`SpectateIdle` ＋ イベントなし）。
    fn start_watching(&mut self) {
        let _ = self.joiner.stop();
        self.close();
        self.bump(Stat::Linked);
        if self.kit.slot.via_v6() {
            self.bump(Stat::LinkedV6);
        }
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
                // スレッドを作れない。`Joiner`に「接続できなかった」の入口が無いので、応答しない扱い（観戦者は失敗で終わる）。
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
                    // 役host: 配布側がまだいない。閉じて、12秒後に入り直す。
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
            // Heldは対戦のルームだけ（観戦用のルームは成立でサーバーが閉じる）。来たら終わりと同じに扱う。
            RoomEvent::Done | RoomEvent::Held => {
                self.joiner.on_done();
                self.close();
            }
            RoomEvent::Failed(f) => {
                self.bump(match f {
                    Failure::Unresponsive(_) => Stat::FailUnresponsive,
                    Failure::RateLimited { .. } => Stat::FailRate,
                    Failure::RoomFull | Failure::RoomInUse => Stat::FailFull,
                    Failure::Expired => Stat::FailExpired,
                    Failure::PeerLeft => Stat::FailPeerLeft,
                    Failure::BadRoomName | Failure::BadTicket => Stat::FailBadName,
                    Failure::Refused => Stat::FailRefused,
                });
                self.joiner.on_failed(now_ms, f);
                self.close();
            }
        }
    }

    /// 観戦用のコードを受け取った: 読み、返答をanswerで出し、`WatchSlot`へ設定を書く。
    /// 読めない・返答できないなら`Joiner::abort`（失敗で終わる）。
    fn on_spectate_code(&mut self, text: &str, unix: u64) {
        self.bump(Stat::CodesReceived);
        let spec = match code::read_spectate(text, unix) {
            Ok(c) => c,
            Err(e) => return self.abort(JoinerFailure::CodeUnreadable(e), Stat::CodeUnreadable),
        };
        // ガードと`WatcherConfig`の組み立てはファイルの経路の観戦者と同じ`Reader`。
        // 同じ文字がもう一度来ても`Same`で断らないよう、読むたびに複製して読む。
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
        let reply =
            match code::write_spectate_reply_to(&spec.code, self.kit.reply_name, self.kit.own) {
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
        // 返答を出したらすぐJOINを始めさせる。
        // JOINは250 msごとに送り直すので、ホストの穴あけの後のJOINが届く。自分のNATにもホストへの穴ができる。
        self.kit.slot.offer(cfg.0);
        self.kit.publisher.note.name = sanitize_name(spec.code.name.as_bytes(), glyphs_has);
        self.note_replied(u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX));
    }

    /// 期限（`LINK_TIMEOUT_MS`）は最初の返答から数える。入り直しで観戦コードが届き直すたびに始め直すと、期限が来ないままJOINを送り続ける。
    fn note_replied(&mut self, now_ms: u64) {
        self.replied_at.get_or_insert(now_ms);
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

    /// 失敗で終える: 理由を状態に書き、`WatchPlay`を終わらせる。数は他で数えてあるのでここで数えるのは30分で諦めた場合だけ。
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

/// 文字の表（相手の名前の文字を`?`に直す）。`ui_glyphs`は小さい順の表で、毎回取得すると確保が起きるので1度だけ作る。
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
    // 先にフラグを立てる（スレッドが速く終わって`finished`を書いた後に作った側が1に戻さない）。
    RUN.store(1, Ordering::Release);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&shared));
    let spawned = std::thread::Builder::new()
        .name("higanr-spectate".into())
        .spawn(move || {
            // パニックは捕捉して外へ出さない。捕捉したら観戦者のセッションを手放せる状態にする（`WatchPlay`を終わらせ、状態は「内部の不具合」）。
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
            Err(format!("観戦者のルームのスレッドの作成不能: {e}"))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
