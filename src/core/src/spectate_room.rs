//! 観戦用のルーム（インターネット越しの観戦）の**入り直しの判断**の状態機械 2 つ: 配る側 [`Caster`]（マッチのホストの dll）と
//! 観戦者 [`Joiner`]（観戦者の dll）。
//!
//! ルームサーバーの手順は対戦と同じ（2 人・先に入った方が `host`、後からが `guest`）。ルーム名は `RoomName::spectate_of`。
//! 配る側が先に入って観戦用のコード（種別 3）を offer し、観戦者が後から入って `peer` で受け、返し（種別 4）を answer で返す。
//! 1 回のルームのやり取りは `room::RoomClient` と dll の `net_room::RoomConn` が持ち、**ここはその外側**:
//! いつ繋ぎ直すか・何回まで繋ぐか・いつ諦めるかだけを決める。
//!
//! ソケットも時計も持たない（`now` は単調な ms を呼び手が渡す）。確保しない。見張りのスレッドで回す（ゲームのスレッドでは呼ばない）。
//!
//! ## 使い方
//!
//! 両方とも「`poll` が `Connect` を返したら、新しい `RoomConn` を開く」で始まる。接続の出来事（`room::Event`）を該当の
//! `on_*` へ渡し、返った指示（`*Action`）を実行する。`Close` を返された接続と、`on_done` / `on_failed` を渡した接続は、
//! dll が閉じて手放す。
//!
//! | 出来事（`room::Event`） | 配る側 | 観戦者 |
//! |---|---|---|
//! | `Joined { role: Offer }`（サーバーの `host`） | `MakeOffer`: 観戦用のコードを**その場で新しい期限で**作って offer する | 配る側がまだいない: `Close`、12 秒後に入り直す |
//! | `Joined { role: Answer }`（サーバーの `guest`） | 観戦者が一瞬いた: `Close`、5 秒後に入り直す | ルームに入れた（`peer` を待つ） |
//! | `PeerCode(text)` | `Punch(text)`: 観戦者の返しを読み、`PunchPlan` を作れ | `SpectateCode(text)`: 観戦用のコードを読み、返しを作って answer せよ |
//! | `Done`（サーバーの 1000） | 2 秒後に入り直す | 成功（終わり） |
//! | `Failed(f)` | 下の表 | 下の表 |
//!
//! 配る側が入るたびに新しい期限のコードを作らせる（`MakeOffer`）のは、`code::DEFAULT_TTL_SECS`（30 分）より長くルームに
//! いても期限切れのコードを預けたままにしないため。
//!
//! ## 失敗の扱い（`room::Failure`）
//!
//! | 失敗 | 配る側（止められるまで続く） | 観戦者 |
//! |---|---|---|
//! | `Expired`（4408 ルームの期限 10 分 / answer の期限） | **すぐ**入り直す（0 ms） | 12 秒後 |
//! | `PeerLeft`（4410） | 2 秒後 | 12 秒後 |
//! | `RoomFull`（409） | 10 秒後 | 12 秒後 |
//! | `RateLimited { retry_after_secs }`（429） | `retry_after`（下限 1 秒・上限 5 分） | 同じ |
//! | `Unresponsive(_)`（応答しない） | 30 秒後。続けるたびに倍（60・120・240・上限 300 秒）。ほかの失敗か 1000 を見ると 30 秒へ戻る | **失敗で終わる**（理由 `Why` つき） |
//! | `BadRoomName`（400。導いた名は決まりの内なので来ないはず） | 応答しない扱い（ゆっくり繰り返す） | 失敗で終わる |
//!
//! 観戦者は別に、**全体で 30 分**（[`JOINER_GIVE_UP_MS`]）待っても会えなければ諦める（`JoinerFailure::GaveUp`）。
//! 観戦用のコードが読めない・返しを作れない・answer を出せない（`Joiner::abort`）も失敗で終わる。
//!
//! ## 入る回数の歯止め
//!
//! サーバーは 1 IP（IPv6 は /64）あたり 60 秒に 30 回までで 429 を返し、対戦のルームの入室や同じ家の別の PC も数える。
//! そこで**どの 60 秒の期間でも、この機械が入る回数は [`ENTRY_WINDOW_MAX`]（8）回を越えない**ことを保証する: 直前の 8 回の
//! うち一番古いものから 60 秒経つまで、次の入室を出さない。表の待ちより歯止めが長ければ、歯止めに合わせて待つ（`window_limited`）。
//!
//! `now` は単調（戻らない）。`poll` は [`Caster::next_wake`] / [`Joiner::next_wake`] の時刻か、それより前に呼ぶ。

use crate::code::CodeError;
use crate::room::{Failure, Role, Why};

// ---------------- 決めた値 ----------------

/// 入る回数を数える期間の長さ（ms）。サーバーの回数制限の期間と同じ 60 秒。
pub const ENTRY_WINDOW_MS: u64 = 60_000;
/// どの [`ENTRY_WINDOW_MS`] の期間でも越えない入る回数。サーバーの上限に対し、同じ IP の別の接続（対戦のルームの入室など）の
/// 分の余裕を残した値。
pub const ENTRY_WINDOW_MAX: usize = 8;

/// 配る側: 1000（手順を終えた）の後、入り直すまでの最短（ms）。観戦者が答えを預けてから穴あけを始めるまでの間を空けず、
/// 連続した入室で期間内の回数を使い切らない間隔。
pub const CASTER_REJOIN_MS: u64 = 2_000;
/// 配る側: 役 `guest`（観戦者が一瞬いた）を受けて閉じた後、入り直すまで（ms）。
pub const CASTER_WRONG_ROLE_MS: u64 = 5_000;
/// 配る側: 4408（ルームの期限）の後、入り直すまで（ms）。ルームの期限切れは正常な繰り返しなのですぐ。
pub const CASTER_EXPIRED_MS: u64 = 0;
/// 配る側: 4410（相手が抜けた）の後、入り直すまで（ms）。
pub const CASTER_PEER_LEFT_MS: u64 = 2_000;
/// 配る側: 409（ルームが満員）の後、入り直すまで（ms）。
pub const CASTER_ROOM_FULL_MS: u64 = 10_000;
/// 配る側: サーバーが応答しないときの最初の待ち（ms）。続けるたびに倍にする。
pub const CASTER_UNRESPONSIVE_FIRST_MS: u64 = 30_000;
/// 配る側: サーバーが応答しない間の待ちの上限（ms。5 分）。
pub const CASTER_UNRESPONSIVE_MAX_MS: u64 = 5 * 60 * 1000;

/// 観戦者: 役 `host`（配る側がいない）・409・4408・4410 の後、入り直すまで（ms）。12 秒なら 60 秒に 5 回で期間の 8 回に収まる。
pub const JOINER_RETRY_MS: u64 = 12_000;
/// 観戦者: 始めてから、配る側に会えないまま諦めるまでの全体の長さ（ms。30 分）。
pub const JOINER_GIVE_UP_MS: u64 = 30 * 60 * 1000;

/// 429 の `retry_after` を待つ長さの下限（ms）。0 秒と言われても、休まずに繋ぎ続けない。
pub const RETRY_AFTER_MIN_MS: u64 = 1_000;
/// 429 の `retry_after` を待つ長さの上限（ms）。サーバーの値が壊れていても何年も待たない。
pub const RETRY_AFTER_MAX_MS: u64 = 5 * 60 * 1000;

fn retry_after_ms(secs: u32) -> u64 {
    (u64::from(secs).saturating_mul(1000)).clamp(RETRY_AFTER_MIN_MS, RETRY_AFTER_MAX_MS)
}

// ---------------- 入る回数の期間 ----------------

/// 入った時刻の直近 [`ENTRY_WINDOW_MAX`] 回を持つリングバッファ。新しい入室は、その最も古い入室から [`ENTRY_WINDOW_MS`] 以上
/// 空いたときだけ許す。
#[derive(Debug, Clone, Copy)]
struct EntryWindow {
    times: [u64; ENTRY_WINDOW_MAX],
    total: u64,
}

impl EntryWindow {
    const NEW: EntryWindow = EntryWindow {
        times: [0; ENTRY_WINDOW_MAX],
        total: 0,
    };

    /// 次に入ってよい最初の時刻。8 回に満たなければ 0。
    fn allowed_at(&self) -> u64 {
        if self.total < ENTRY_WINDOW_MAX as u64 {
            return 0;
        }
        // 次に上書きされる枠 = 8 回前の入室。
        let oldest = self
            .times
            .get((self.total % ENTRY_WINDOW_MAX as u64) as usize)
            .copied()
            .unwrap_or(0);
        oldest.saturating_add(ENTRY_WINDOW_MS)
    }

    fn record(&mut self, now: u64) {
        let i = (self.total % ENTRY_WINDOW_MAX as u64) as usize;
        if let Some(slot) = self.times.get_mut(i) {
            *slot = now;
        }
        self.total += 1;
    }

    fn recent(&self, now: u64) -> usize {
        let n = usize::try_from(self.total).map_or(ENTRY_WINDOW_MAX, |t| t.min(ENTRY_WINDOW_MAX));
        self.times
            .iter()
            .take(n)
            .filter(|t| now.saturating_sub(**t) < ENTRY_WINDOW_MS)
            .count()
    }
}

// ---------------- 共通の語 ----------------

/// 入り直しを待っている理由（状態の表示と記録用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    /// 始めたばかり（最初の入室。待ちは 0）。
    Start,
    /// 配る側: 1000 で 1 人ぶんの手順を終えた。
    Done,
    /// 4408（ルーム・answer の期限）。
    Expired,
    /// 4410（相手が抜けた）。
    PeerLeft,
    /// 役が合わなかった（配る側: 役 `guest`。観戦者: 役 `host` = 配る側がまだいない）。
    WrongRole,
    /// 409（ルームが満員）。
    RoomFull,
    /// 429。
    RateLimited { retry_after_secs: u32 },
    /// サーバーが応答しない（`consecutive` は続けて何回目か）。配る側だけ。
    Unresponsive { consecutive: u32 },
}

// ---------------- 配る側 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasterAction {
    Connect,
    /// 観戦用のコードを**いまの時刻から新しい期限で**作り、`RoomClient::submit_code` で offer せよ。
    MakeOffer,
    /// 観戦者の返し（`peer` の文字）。`code::read_spectate_reply` で読み、読めたら候補から `spectate::PunchPlan` を作って
    /// 受信スレッドへ渡せ（読めなければ捨てる）。
    Punch(String),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasterPhase {
    /// 繋いでいない。次の入室を待っている（`CasterStatus::next_entry_at`）。
    Waiting,
    Joining,
    /// 役 `host` でルームにいる。観戦者を待っている（offer を預けた後）。
    Casting,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CasterStatus {
    pub phase: CasterPhase,
    /// `Waiting` のとき、次に入る時刻（ms。待ちと入る回数の歯止めの遅い方）。
    pub next_entry_at: Option<u64>,
    /// いちばん最近の待ちの理由（入った後も次の待ちまで残る）。
    pub last_wait: Option<WaitReason>,
    /// 次の入室が、入る回数の歯止めで予定より遅れているか。
    pub window_limited: bool,
    pub entries_in_window: usize,
    pub entries_total: u64,
    /// サーバーが応答しない失敗が続いている回数（ほかの失敗か 1000 で 0 に戻る）。
    pub unresponsive_streak: u32,
}

/// 配る側（マッチのホストの dll）の入り直しの判断。止められるまで、ルームに入り直し続ける。
#[derive(Debug, Clone, Copy)]
pub struct Caster {
    phase: CasterPhase,
    /// `Waiting` のとき、表の待ちだけで決まる入室の時刻（歯止めは別に見る）。
    wait_until: u64,
    last_wait: Option<WaitReason>,
    window: EntryWindow,
    streak: u32,
}

impl Caster {
    pub fn new(now: u64) -> Caster {
        Caster {
            phase: CasterPhase::Waiting,
            wait_until: now,
            last_wait: Some(WaitReason::Start),
            window: EntryWindow::NEW,
            streak: 0,
        }
    }

    fn next_entry(&self) -> u64 {
        self.wait_until.max(self.window.allowed_at())
    }

    fn wait(&mut self, now: u64, delay_ms: u64, why: WaitReason) {
        self.phase = CasterPhase::Waiting;
        self.wait_until = now.saturating_add(delay_ms);
        self.last_wait = Some(why);
    }

    /// 入る時刻になっていれば `Connect`（このとき入る回数を 1 つ数える）。
    pub fn poll(&mut self, now: u64) -> Option<CasterAction> {
        if self.phase == CasterPhase::Waiting && now >= self.next_entry() {
            self.window.record(now);
            self.phase = CasterPhase::Joining;
            return Some(CasterAction::Connect);
        }
        None
    }

    /// 次に `poll` を呼ぶべき時刻（ms）。待っていなければ `None`（出来事が来るまで）。
    pub fn next_wake(&self) -> Option<u64> {
        (self.phase == CasterPhase::Waiting).then(|| self.next_entry())
    }

    pub fn on_joined(&mut self, now: u64, role: Role) -> Option<CasterAction> {
        if self.phase != CasterPhase::Joining {
            return None;
        }
        match role {
            Role::Offer => {
                self.phase = CasterPhase::Casting;
                Some(CasterAction::MakeOffer)
            }
            Role::Answer => {
                self.wait(now, CASTER_WRONG_ROLE_MS, WaitReason::WrongRole);
                Some(CasterAction::Close)
            }
        }
    }

    pub fn on_peer_code(&mut self, text: String) -> Option<CasterAction> {
        (self.phase == CasterPhase::Casting).then_some(CasterAction::Punch(text))
    }

    pub fn on_done(&mut self, now: u64) {
        if matches!(self.phase, CasterPhase::Joining | CasterPhase::Casting) {
            self.streak = 0;
            self.wait(now, CASTER_REJOIN_MS, WaitReason::Done);
        }
    }

    pub fn on_failed(&mut self, now: u64, f: Failure) {
        if !matches!(self.phase, CasterPhase::Joining | CasterPhase::Casting) {
            return;
        }
        if matches!(f, Failure::Unresponsive(_) | Failure::BadRoomName) {
            self.streak = self.streak.saturating_add(1);
            // 30 秒 → 60 → 120 → 240 → 300（上限）。倍にする回数は 10 で頭打ちにして溢れさせない。
            let shift = self.streak.saturating_sub(1).min(10);
            let delay = CASTER_UNRESPONSIVE_FIRST_MS
                .saturating_mul(1u64 << shift)
                .min(CASTER_UNRESPONSIVE_MAX_MS);
            self.wait(
                now,
                delay,
                WaitReason::Unresponsive {
                    consecutive: self.streak,
                },
            );
            return;
        }
        self.streak = 0;
        match f {
            Failure::Expired => self.wait(now, CASTER_EXPIRED_MS, WaitReason::Expired),
            Failure::PeerLeft => self.wait(now, CASTER_PEER_LEFT_MS, WaitReason::PeerLeft),
            Failure::RoomFull => self.wait(now, CASTER_ROOM_FULL_MS, WaitReason::RoomFull),
            Failure::RateLimited { retry_after_secs } => self.wait(
                now,
                retry_after_ms(retry_after_secs),
                WaitReason::RateLimited { retry_after_secs },
            ),
            Failure::Unresponsive(_) | Failure::BadRoomName => {}
        }
    }

    /// 止める（つながりの片づけ）。以後 `poll` は何も返さず、出来事は無視する。
    pub fn stop(&mut self) {
        self.phase = CasterPhase::Stopped;
    }

    pub fn status(&self, now: u64) -> CasterStatus {
        let waiting = self.phase == CasterPhase::Waiting;
        CasterStatus {
            phase: self.phase,
            next_entry_at: waiting.then(|| self.next_entry()),
            last_wait: self.last_wait,
            window_limited: waiting && self.window.allowed_at() > self.wait_until,
            entries_in_window: self.window.recent(now),
            entries_total: self.window.total,
            unresponsive_streak: self.streak,
        }
    }
}

// ---------------- 観戦者 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinerAction {
    Connect,
    Close,
    /// 観戦用のコード（`peer` の文字）。`code::read_spectate` で読み、返しを作って answer せよ。出せたら
    /// [`Joiner::on_answer_sent`]、読めない・作れない・出せないなら [`Joiner::abort`]。
    SpectateCode(String),
}

/// 観戦者が失敗で終わった理由（画面に出す理由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinerFailure {
    Unresponsive(Why),
    BadRoomName,
    /// 観戦用のコードを読めない・返しを作れない（中は理由。バージョン違い `BadVersion` を含む）。
    CodeUnreadable(CodeError),
    ReplyNotSent,
    GaveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinerPhase {
    /// 繋いでいない。次の入室を待っている（`last_wait` が `WrongRole` なら「配る側を待っている」）。
    Waiting,
    Joining,
    /// 役 `guest` でルームにいる。観戦用のコードを待っている（または読んで返しを作っている）。
    InRoom,
    AnswerSent,
    /// 終わり（成功）。次は `Watcher` が JOIN を送る。
    Succeeded,
    Failed(JoinerFailure),
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinerStatus {
    pub phase: JoinerPhase,
    /// `Waiting` のとき、次に入る時刻（ms）。
    pub next_entry_at: Option<u64>,
    /// いちばん最近の待ちの理由。`InRoom` に入ると消える。
    pub last_wait: Option<WaitReason>,
    pub window_limited: bool,
    pub entries_in_window: usize,
    pub entries_total: u64,
    pub give_up_at: u64,
}

impl JoinerStatus {
    /// 「対戦者（配る側）を待っています」と出せる状態か: 配る側がまだルームにいなかったので入り直しを待っている間。
    pub fn waiting_for_caster(&self) -> bool {
        self.last_wait == Some(WaitReason::WrongRole)
            && matches!(self.phase, JoinerPhase::Waiting | JoinerPhase::Joining)
    }
}

/// 観戦者（観戦者の dll）の入り直しの判断。配る側に会えるか、失敗するか、30 分経つまで入り直す。
#[derive(Debug, Clone, Copy)]
pub struct Joiner {
    phase: JoinerPhase,
    wait_until: u64,
    last_wait: Option<WaitReason>,
    window: EntryWindow,
    give_up_at: u64,
}

impl Joiner {
    pub fn new(now: u64) -> Joiner {
        Joiner {
            phase: JoinerPhase::Waiting,
            wait_until: now,
            last_wait: Some(WaitReason::Start),
            window: EntryWindow::NEW,
            give_up_at: now.saturating_add(JOINER_GIVE_UP_MS),
        }
    }

    fn next_entry(&self) -> u64 {
        self.wait_until.max(self.window.allowed_at())
    }

    fn wait(&mut self, now: u64, delay_ms: u64, why: WaitReason) {
        self.phase = JoinerPhase::Waiting;
        self.wait_until = now.saturating_add(delay_ms);
        self.last_wait = Some(why);
    }

    fn is_active(&self) -> bool {
        !matches!(
            self.phase,
            JoinerPhase::Succeeded | JoinerPhase::Failed(_) | JoinerPhase::Stopped
        )
    }

    /// 全体の時間切れなら諦め（接続があれば `Close`）、入る時刻になっていれば `Connect`。`AnswerSent` の後は諦めない
    /// （あと少しで 1000 が来る）。
    pub fn poll(&mut self, now: u64) -> Option<JoinerAction> {
        if self.is_active() && self.phase != JoinerPhase::AnswerSent && now >= self.give_up_at {
            let connected = matches!(self.phase, JoinerPhase::Joining | JoinerPhase::InRoom);
            self.phase = JoinerPhase::Failed(JoinerFailure::GaveUp);
            return connected.then_some(JoinerAction::Close);
        }
        if self.phase == JoinerPhase::Waiting && now >= self.next_entry() {
            self.window.record(now);
            self.phase = JoinerPhase::Joining;
            return Some(JoinerAction::Connect);
        }
        None
    }

    pub fn next_wake(&self) -> Option<u64> {
        match self.phase {
            JoinerPhase::Waiting => Some(self.next_entry().min(self.give_up_at)),
            JoinerPhase::Joining | JoinerPhase::InRoom => Some(self.give_up_at),
            JoinerPhase::AnswerSent
            | JoinerPhase::Succeeded
            | JoinerPhase::Failed(_)
            | JoinerPhase::Stopped => None,
        }
    }

    /// ルームに入れた。役 `host` は、配る側がまだいない。
    pub fn on_joined(&mut self, now: u64, role: Role) -> Option<JoinerAction> {
        if self.phase != JoinerPhase::Joining {
            return None;
        }
        match role {
            Role::Offer => {
                self.wait(now, JOINER_RETRY_MS, WaitReason::WrongRole);
                Some(JoinerAction::Close)
            }
            Role::Answer => {
                self.phase = JoinerPhase::InRoom;
                self.last_wait = None;
                None
            }
        }
    }

    pub fn on_peer_code(&mut self, text: String) -> Option<JoinerAction> {
        (self.phase == JoinerPhase::InRoom).then_some(JoinerAction::SpectateCode(text))
    }

    /// answer を出した。サーバーの 1000 を待つ。
    pub fn on_answer_sent(&mut self) {
        if self.phase == JoinerPhase::InRoom {
            self.phase = JoinerPhase::AnswerSent;
        }
    }

    pub fn on_done(&mut self) {
        if matches!(self.phase, JoinerPhase::InRoom | JoinerPhase::AnswerSent) {
            self.phase = JoinerPhase::Succeeded;
        }
    }

    pub fn on_failed(&mut self, now: u64, f: Failure) {
        if !matches!(
            self.phase,
            JoinerPhase::Joining | JoinerPhase::InRoom | JoinerPhase::AnswerSent
        ) {
            return;
        }
        match f {
            Failure::Unresponsive(why) => {
                self.phase = JoinerPhase::Failed(JoinerFailure::Unresponsive(why));
            }
            Failure::BadRoomName => self.phase = JoinerPhase::Failed(JoinerFailure::BadRoomName),
            Failure::RateLimited { retry_after_secs } => self.wait(
                now,
                retry_after_ms(retry_after_secs),
                WaitReason::RateLimited { retry_after_secs },
            ),
            Failure::RoomFull => self.wait(now, JOINER_RETRY_MS, WaitReason::RoomFull),
            Failure::Expired => self.wait(now, JOINER_RETRY_MS, WaitReason::Expired),
            Failure::PeerLeft => self.wait(now, JOINER_RETRY_MS, WaitReason::PeerLeft),
        }
    }

    /// dll が続けられなくなった（コードを読めない・返しを作れない・answer を出せない）。失敗で終わり、接続を閉じさせる。
    /// 終わっている・繋いでいない機械には何もしない。
    pub fn abort(&mut self, why: JoinerFailure) -> Option<JoinerAction> {
        if matches!(self.phase, JoinerPhase::InRoom | JoinerPhase::AnswerSent) {
            self.phase = JoinerPhase::Failed(why);
            return Some(JoinerAction::Close);
        }
        None
    }

    /// 利用者がやめた。繋いでいれば `Close`。終わっていた機械の結果は書き換えない。
    pub fn stop(&mut self) -> Option<JoinerAction> {
        let connected = matches!(
            self.phase,
            JoinerPhase::Joining | JoinerPhase::InRoom | JoinerPhase::AnswerSent
        );
        if self.is_active() {
            self.phase = JoinerPhase::Stopped;
        }
        connected.then_some(JoinerAction::Close)
    }

    pub fn status(&self, now: u64) -> JoinerStatus {
        let waiting = self.phase == JoinerPhase::Waiting;
        JoinerStatus {
            phase: self.phase,
            next_entry_at: waiting.then(|| self.next_entry()),
            last_wait: self.last_wait,
            window_limited: waiting && self.window.allowed_at() > self.wait_until,
            entries_in_window: self.window.recent(now),
            entries_total: self.window.total,
            give_up_at: self.give_up_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn no_answer() -> Failure {
        Failure::Unresponsive(Why::NoAnswer)
    }

    fn caster_joining(now: u64) -> Caster {
        let mut c = Caster::new(now);
        assert_eq!(c.poll(now), Some(CasterAction::Connect));
        c
    }

    fn joiner_joining(now: u64) -> Joiner {
        let mut j = Joiner::new(now);
        assert_eq!(j.poll(now), Some(JoinerAction::Connect));
        j
    }

    /// `times` のどの 60 秒の期間にも 9 回以上入っていない（期間 `[t, t + 60 秒)` の数え直し）こと。一番混む期間の入る回数を返す。
    fn busiest_window(times: &[u64]) -> usize {
        times
            .iter()
            .map(|&t| {
                times
                    .iter()
                    .filter(|&&u| u >= t && u < t + ENTRY_WINDOW_MS)
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    // ---------------- 値の固定 ----------------

    #[test]
    fn the_decided_values_are_pinned() {
        assert_eq!(ENTRY_WINDOW_MAX, 8);
        assert_eq!(ENTRY_WINDOW_MS, 60_000);
        assert_eq!(CASTER_REJOIN_MS, 2_000);
        assert_eq!(CASTER_WRONG_ROLE_MS, 5_000);
        assert_eq!(CASTER_EXPIRED_MS, 0);
        assert_eq!(CASTER_PEER_LEFT_MS, 2_000);
        assert_eq!(CASTER_ROOM_FULL_MS, 10_000);
        assert_eq!(CASTER_UNRESPONSIVE_FIRST_MS, 30_000);
        assert_eq!(CASTER_UNRESPONSIVE_MAX_MS, 300_000);
        assert_eq!(JOINER_RETRY_MS, 12_000);
        assert_eq!(JOINER_GIVE_UP_MS, 1_800_000);
    }

    // ---------------- 配る側 ----------------

    #[test]
    fn a_caster_offers_on_every_entry_punches_on_a_reply_and_returns_two_seconds_after_done() {
        let mut c = Caster::new(NOW);
        assert_eq!(c.status(NOW).phase, CasterPhase::Waiting);
        assert_eq!(c.status(NOW).last_wait, Some(WaitReason::Start));
        assert_eq!(c.next_wake(), Some(NOW));
        let mut t = NOW;
        for round in 0..3u64 {
            assert_eq!(c.poll(t), Some(CasterAction::Connect), "{round}");
            assert_eq!(c.status(t).phase, CasterPhase::Joining);
            assert_eq!(c.poll(t), None, "接続中は二重に入らない");
            assert_eq!(
                c.on_joined(t + 10, Role::Offer),
                Some(CasterAction::MakeOffer),
                "{round} 回目の入室でも新しいコードを作らせる"
            );
            assert_eq!(c.status(t + 10).phase, CasterPhase::Casting);
            assert_eq!(
                c.on_peer_code("ReplyText".to_string()),
                Some(CasterAction::Punch("ReplyText".to_string()))
            );
            t += 5_000;
            c.on_done(t);
            let s = c.status(t);
            assert_eq!(s.phase, CasterPhase::Waiting);
            assert_eq!(s.last_wait, Some(WaitReason::Done));
            assert_eq!(s.next_entry_at, Some(t + 2_000));
            assert_eq!(c.next_wake(), Some(t + 2_000));
            assert_eq!(c.poll(t + 1_999), None);
            t += 2_000;
        }
        assert_eq!(c.status(t).entries_total, 3);
    }

    #[test]
    fn a_caster_that_meets_a_guest_closes_and_returns_after_five_seconds() {
        let mut c = caster_joining(NOW);
        assert_eq!(
            c.on_joined(NOW + 100, Role::Answer),
            Some(CasterAction::Close)
        );
        let s = c.status(NOW + 100);
        assert_eq!(s.phase, CasterPhase::Waiting);
        assert_eq!(s.last_wait, Some(WaitReason::WrongRole));
        assert_eq!(s.next_entry_at, Some(NOW + 100 + 5_000));
        c.on_failed(NOW + 200, no_answer());
        c.on_done(NOW + 300);
        assert_eq!(c.on_peer_code("x".into()), None);
        assert_eq!(c.status(NOW + 400).next_entry_at, Some(NOW + 5_100));
        assert_eq!(c.poll(NOW + 5_099), None);
        assert_eq!(c.poll(NOW + 5_100), Some(CasterAction::Connect));
    }

    #[test]
    fn a_caster_waits_as_the_table_says_for_each_failure() {
        let cases = [
            (Failure::Expired, 0, WaitReason::Expired),
            (Failure::PeerLeft, 2_000, WaitReason::PeerLeft),
            (Failure::RoomFull, 10_000, WaitReason::RoomFull),
            (
                Failure::RateLimited {
                    retry_after_secs: 60,
                },
                60_000,
                WaitReason::RateLimited {
                    retry_after_secs: 60,
                },
            ),
            (
                Failure::RateLimited {
                    retry_after_secs: 0,
                },
                1_000,
                WaitReason::RateLimited {
                    retry_after_secs: 0,
                },
            ),
            (
                Failure::RateLimited {
                    retry_after_secs: u32::MAX,
                },
                300_000,
                WaitReason::RateLimited {
                    retry_after_secs: u32::MAX,
                },
            ),
        ];
        for (f, delay, why) in cases {
            for in_room in [false, true] {
                let mut c = caster_joining(NOW);
                if in_room {
                    c.on_joined(NOW, Role::Offer);
                }
                c.on_failed(NOW + 50, f);
                let s = c.status(NOW + 50);
                assert_eq!(s.phase, CasterPhase::Waiting, "{f:?}");
                assert_eq!(s.last_wait, Some(why), "{f:?}");
                assert_eq!(s.next_entry_at, Some(NOW + 50 + delay), "{f:?} {in_room}");
                if delay > 0 {
                    assert_eq!(c.poll(NOW + 50 + delay - 1), None, "{f:?}");
                }
                assert_eq!(
                    c.poll(NOW + 50 + delay),
                    Some(CasterAction::Connect),
                    "{f:?}"
                );
            }
        }
    }

    #[test]
    fn a_caster_backs_off_while_the_server_does_not_answer_and_resets_after_any_other_outcome() {
        let mut c = Caster::new(NOW);
        let mut t = NOW;
        let want = [30_000, 60_000, 120_000, 240_000, 300_000, 300_000, 300_000];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(c.poll(t), Some(CasterAction::Connect), "{i}");
            c.on_failed(t, no_answer());
            let s = c.status(t);
            assert_eq!(s.next_entry_at, Some(t + w), "{i} 回目");
            assert_eq!(
                s.last_wait,
                Some(WaitReason::Unresponsive {
                    consecutive: i as u32 + 1
                })
            );
            assert_eq!(s.unresponsive_streak, i as u32 + 1);
            t += w;
        }
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_failed(t, Failure::BadRoomName);
        assert_eq!(c.status(t).next_entry_at, Some(t + 300_000));
        t += 300_000;
        for _ in 0..300 {
            assert_eq!(c.poll(t), Some(CasterAction::Connect));
            c.on_failed(t, no_answer());
            t = c.status(t).next_entry_at.unwrap();
        }
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_joined(t, Role::Offer);
        c.on_done(t);
        assert_eq!(c.status(t).unresponsive_streak, 0);
        t += 2_000;
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_failed(t, no_answer());
        assert_eq!(
            c.status(t).next_entry_at,
            Some(t + 30_000),
            "1000 の後は 30 秒から"
        );
        t += 30_000;
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_failed(t, no_answer());
        t += 60_000;
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_failed(t, Failure::PeerLeft);
        assert_eq!(c.status(t).unresponsive_streak, 0);
        t += 2_000;
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_failed(t, no_answer());
        assert_eq!(
            c.status(t).next_entry_at,
            Some(t + 30_000),
            "別の失敗の後も 30 秒から"
        );
    }

    #[test]
    fn a_caster_never_enters_more_than_eight_times_in_any_minute() {
        let mut c = Caster::new(0);
        let mut times = Vec::new();
        let mut t = 0u64;
        while times.len() < 40 {
            match c.poll(t) {
                Some(CasterAction::Connect) => {
                    times.push(t);
                    c.on_failed(t, Failure::Expired);
                }
                _ => {
                    let s = c.status(t);
                    if !times.is_empty() && times.len().is_multiple_of(8) {
                        assert!(s.window_limited, "{t}: {s:?}");
                        assert_eq!(s.entries_in_window, 8, "{t}");
                    }
                    t = c.next_wake().unwrap();
                }
            }
        }
        for (i, &x) in times.iter().enumerate() {
            let want = 60_000 * (i as u64 / 8);
            assert_eq!(x, want, "{i} 回目の入室の時刻（8 回ごとに 60 秒）");
        }
        assert_eq!(busiest_window(&times), 8);
    }

    #[test]
    fn a_stopped_caster_stays_quiet_and_out_of_turn_events_are_ignored() {
        let mut c = Caster::new(NOW);
        assert_eq!(c.on_joined(NOW, Role::Offer), None);
        assert_eq!(c.on_peer_code("x".into()), None);
        c.on_done(NOW);
        c.on_failed(NOW, no_answer());
        assert_eq!(c.status(NOW).phase, CasterPhase::Waiting);
        assert_eq!(c.status(NOW).unresponsive_streak, 0);
        assert_eq!(c.poll(NOW), Some(CasterAction::Connect));
        assert_eq!(c.on_peer_code("x".into()), None);
        c.on_joined(NOW, Role::Offer);
        c.stop();
        assert_eq!(c.status(NOW).phase, CasterPhase::Stopped);
        assert_eq!(c.next_wake(), None);
        assert_eq!(c.on_peer_code("x".into()), None);
        c.on_done(NOW);
        c.on_failed(NOW, Failure::Expired);
        assert_eq!(c.poll(NOW + 10_000_000), None);
        assert_eq!(c.status(NOW).phase, CasterPhase::Stopped);
    }

    // ---------------- 観戦者 ----------------

    #[test]
    fn a_joiner_reads_the_code_answers_and_succeeds_on_done() {
        let mut j = Joiner::new(NOW);
        assert_eq!(j.status(NOW).give_up_at, NOW + 1_800_000);
        assert_eq!(j.next_wake(), Some(NOW));
        assert_eq!(j.poll(NOW), Some(JoinerAction::Connect));
        assert_eq!(
            j.on_peer_code("early".into()),
            None,
            "room の前の peer は読まない"
        );
        assert_eq!(j.on_joined(NOW + 50, Role::Answer), None);
        assert_eq!(j.status(NOW + 50).phase, JoinerPhase::InRoom);
        assert_eq!(j.status(NOW + 50).last_wait, None);
        assert_eq!(
            j.on_peer_code("SpecCode".into()),
            Some(JoinerAction::SpectateCode("SpecCode".to_string()))
        );
        j.on_answer_sent();
        assert_eq!(j.status(NOW + 60).phase, JoinerPhase::AnswerSent);
        assert_eq!(j.next_wake(), None);
        j.on_done();
        assert_eq!(j.status(NOW + 70).phase, JoinerPhase::Succeeded);
        assert_eq!(j.poll(NOW + 10_000_000), None);
        j.on_failed(NOW + 80, no_answer());
        assert_eq!(
            j.status(NOW + 90).phase,
            JoinerPhase::Succeeded,
            "終わった後の失敗で結果が変わらない"
        );
    }

    #[test]
    fn a_joiner_waits_for_a_caster_and_gives_up_after_thirty_minutes() {
        let mut j = Joiner::new(0);
        let mut t = 0u64;
        let mut times = Vec::new();
        loop {
            assert_eq!(j.poll(t), Some(JoinerAction::Connect), "{t}");
            times.push(t);
            assert!(t < 1_800_000);
            if times.len() > 1 {
                assert!(
                    j.status(t).waiting_for_caster(),
                    "入り直して room を待つ間も"
                );
            }
            assert_eq!(j.on_joined(t + 30, Role::Offer), Some(JoinerAction::Close));
            let s = j.status(t + 30);
            assert!(s.waiting_for_caster());
            assert_eq!(s.next_entry_at, Some(t + 30 + 12_000));
            assert_eq!(j.next_wake(), Some((t + 30 + 12_000).min(1_800_000)));
            t += 30 + 12_000;
            if t >= 1_800_000 {
                break;
            }
        }
        assert_eq!(j.poll(1_799_999), None);
        assert_eq!(j.status(1_799_999).phase, JoinerPhase::Waiting);
        assert_eq!(j.poll(1_800_000), None, "接続が無いので Close は要らない");
        assert_eq!(
            j.status(1_800_000).phase,
            JoinerPhase::Failed(JoinerFailure::GaveUp)
        );
        assert_eq!(
            times.len(),
            150,
            "12.03 秒ごと（待ち 12 秒 + ルームに入るまでの 30 ms）の入室が 30 分に 150 回"
        );
        assert!(busiest_window(&times) <= 8);
        let mut j = joiner_joining(NOW);
        j.on_joined(NOW, Role::Offer);
        let t2 = NOW + 12_000;
        assert_eq!(j.poll(t2), Some(JoinerAction::Connect));
        j.on_joined(t2, Role::Answer);
        assert!(!j.status(t2).waiting_for_caster());
    }

    #[test]
    fn a_joiner_retries_busy_rooms_and_fails_when_the_server_does_not_answer() {
        let cases = [
            (Failure::RoomFull, 12_000, WaitReason::RoomFull),
            (Failure::Expired, 12_000, WaitReason::Expired),
            (Failure::PeerLeft, 12_000, WaitReason::PeerLeft),
            (
                Failure::RateLimited {
                    retry_after_secs: 42,
                },
                42_000,
                WaitReason::RateLimited {
                    retry_after_secs: 42,
                },
            ),
            (
                Failure::RateLimited {
                    retry_after_secs: 0,
                },
                1_000,
                WaitReason::RateLimited {
                    retry_after_secs: 0,
                },
            ),
            (
                Failure::RateLimited {
                    retry_after_secs: 100_000,
                },
                300_000,
                WaitReason::RateLimited {
                    retry_after_secs: 100_000,
                },
            ),
        ];
        for (f, delay, why) in cases {
            for in_room in [false, true] {
                let mut j = joiner_joining(NOW);
                if in_room {
                    j.on_joined(NOW, Role::Answer);
                }
                j.on_failed(NOW + 10, f);
                let s = j.status(NOW + 10);
                assert_eq!(s.phase, JoinerPhase::Waiting, "{f:?}");
                assert_eq!(s.last_wait, Some(why), "{f:?}");
                assert_eq!(s.next_entry_at, Some(NOW + 10 + delay), "{f:?}");
                assert_eq!(j.poll(NOW + 10 + delay - 1), None, "{f:?}");
                assert_eq!(
                    j.poll(NOW + 10 + delay),
                    Some(JoinerAction::Connect),
                    "{f:?}"
                );
            }
        }
        for why in [Why::NoAnswer, Why::HttpStatus(503), Why::NoPong, Why::Lost] {
            for in_room in [false, true] {
                let mut j = joiner_joining(NOW);
                if in_room {
                    j.on_joined(NOW, Role::Answer);
                }
                j.on_failed(NOW + 10, Failure::Unresponsive(why));
                assert_eq!(
                    j.status(NOW + 10).phase,
                    JoinerPhase::Failed(JoinerFailure::Unresponsive(why))
                );
                assert_eq!(j.poll(NOW + 10_000_000), None, "終わった後は入り直さない");
                assert_eq!(j.next_wake(), None);
            }
        }
        let mut j = joiner_joining(NOW);
        j.on_failed(NOW, Failure::BadRoomName);
        assert_eq!(
            j.status(NOW).phase,
            JoinerPhase::Failed(JoinerFailure::BadRoomName)
        );
    }

    #[test]
    fn a_joiner_aborts_on_unusable_codes_and_the_deadline_closes_the_connection() {
        let mut j = joiner_joining(NOW);
        j.on_joined(NOW, Role::Answer);
        j.on_peer_code("x".into());
        assert_eq!(
            j.abort(JoinerFailure::CodeUnreadable(CodeError::BadVersion(1))),
            Some(JoinerAction::Close)
        );
        assert_eq!(
            j.status(NOW).phase,
            JoinerPhase::Failed(JoinerFailure::CodeUnreadable(CodeError::BadVersion(1)))
        );
        assert_eq!(
            j.abort(JoinerFailure::ReplyNotSent),
            None,
            "終わった後は変わらない"
        );
        let mut waiting = Joiner::new(NOW);
        assert_eq!(waiting.abort(JoinerFailure::ReplyNotSent), None);
        assert_eq!(waiting.status(NOW).phase, JoinerPhase::Waiting);

        for phase_in_room in [false, true] {
            let mut j = Joiner::new(0);
            assert_eq!(j.poll(0), Some(JoinerAction::Connect));
            if phase_in_room {
                j.on_joined(1, Role::Answer);
            }
            assert_eq!(j.next_wake(), Some(1_800_000));
            assert_eq!(j.poll(1_799_999), None);
            assert_eq!(j.poll(1_800_000), Some(JoinerAction::Close));
            assert_eq!(
                j.status(1_800_000).phase,
                JoinerPhase::Failed(JoinerFailure::GaveUp)
            );
        }
        let mut j = Joiner::new(0);
        j.poll(0);
        j.on_joined(1, Role::Answer);
        j.on_answer_sent();
        assert_eq!(j.poll(2_000_000), None);
        assert_eq!(j.status(2_000_000).phase, JoinerPhase::AnswerSent);
        j.on_done();
        assert_eq!(j.status(2_000_000).phase, JoinerPhase::Succeeded);
        let mut j = joiner_joining(NOW);
        assert_eq!(j.stop(), Some(JoinerAction::Close));
        assert_eq!(j.status(NOW).phase, JoinerPhase::Stopped);
        assert_eq!(j.poll(NOW + 10_000_000), None);
        assert_eq!(j.stop(), None);
        let mut done = Joiner::new(NOW);
        done.poll(NOW);
        done.on_joined(NOW, Role::Answer);
        done.on_done();
        assert_eq!(done.stop(), None);
        assert_eq!(done.status(NOW).phase, JoinerPhase::Succeeded);
    }

    #[test]
    fn a_joiner_never_enters_more_than_eight_times_in_any_minute() {
        let mut j = Joiner::new(0);
        let mut times = Vec::new();
        let mut t = 0u64;
        let mut guard = 0u32;
        let outcome = loop {
            // 諦めが壊れても止まるように。
            guard += 1;
            assert!(guard < 5_000, "終わらない: 入室 {} 回", times.len());
            match j.poll(t) {
                Some(JoinerAction::Connect) => {
                    times.push(t);
                    j.on_failed(
                        t,
                        Failure::RateLimited {
                            retry_after_secs: 0,
                        },
                    );
                }
                _ => {
                    if let JoinerPhase::Failed(f) = j.status(t).phase {
                        break (t, f);
                    }
                    t = j.next_wake().unwrap();
                }
            }
        };
        assert_eq!(outcome, (1_800_000, JoinerFailure::GaveUp));
        assert_eq!(busiest_window(&times), 8);
        assert_eq!(times.len(), 240);
        assert_eq!(
            &times[..9],
            &[0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 7_000, 60_000]
        );
        for w in times.windows(9) {
            assert!(w[8] - w[0] >= 60_000, "{w:?}");
        }
    }

    // ---------------- 乱れた出来事の列 ----------------

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_failure(r: &mut Rng) -> Failure {
        match r.below(7) {
            0 => Failure::Expired,
            1 => Failure::PeerLeft,
            2 => Failure::RoomFull,
            3 => Failure::RateLimited {
                retry_after_secs: r.below(4) as u32,
            },
            4 => Failure::BadRoomName,
            _ => Failure::Unresponsive(Why::NoAnswer),
        }
    }

    #[test]
    fn random_event_streams_keep_the_window_and_never_double_connect() {
        let mut total_caster = 0usize;
        let mut total_joiner = 0usize;
        for seed in 0..200u64 {
            let mut r = Rng(0x5EC7_0000 + seed);
            let mut caster = Caster::new(0);
            let mut joiner = Joiner::new(0);
            let (mut ct, mut jt) = (Vec::new(), Vec::new());
            let mut now = 0u64;
            for _ in 0..400 {
                now += match r.below(4) {
                    0 => 0,
                    1 => r.below(500),
                    2 => r.below(15_000),
                    _ => r.below(120_000),
                };
                if let Some(CasterAction::Connect) = caster.poll(now) {
                    ct.push(now);
                    assert_eq!(caster.poll(now), None, "接続中に二重に入った");
                }
                if let Some(JoinerAction::Connect) = joiner.poll(now) {
                    jt.push(now);
                    assert_eq!(joiner.poll(now), None, "接続中に二重に入った");
                }
                let role = if r.below(2) == 0 {
                    Role::Offer
                } else {
                    Role::Answer
                };
                match r.below(6) {
                    0 => {
                        caster.on_joined(now, role);
                    }
                    1 => {
                        caster.on_peer_code("c".into());
                    }
                    2 => caster.on_done(now),
                    3 => caster.on_failed(now, random_failure(&mut r)),
                    4 => {
                        joiner.on_joined(now, role);
                    }
                    _ => {
                        joiner.on_peer_code("c".into());
                        joiner.on_answer_sent();
                    }
                }
                match r.below(3) {
                    0 => joiner.on_failed(now, random_failure(&mut r)),
                    1 => joiner.on_done(),
                    _ => {}
                }
            }
            assert!(
                busiest_window(&ct) <= ENTRY_WINDOW_MAX,
                "seed {seed}: 配る側 {ct:?}"
            );
            assert!(
                busiest_window(&jt) <= ENTRY_WINDOW_MAX,
                "seed {seed}: 観戦者 {jt:?}"
            );
            total_caster += ct.len();
            total_joiner += jt.len();
        }
        // 母数: 配る側は 200 本で数千回入る（0 件の列では保証を見ていない）。
        assert!(total_caster > 1_000, "配る側の入室 {total_caster}");
        assert!(total_joiner > 200, "観戦者の入室 {total_joiner}");
    }
}
