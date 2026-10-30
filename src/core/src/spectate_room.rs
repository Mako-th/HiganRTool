//! 観戦用のルーム（インターネット越しの観戦）の**入り直しの判断**のステートマシン2つ: 配信側[`Caster`]（マッチのホストのdll）と観戦者[`Joiner`]（観戦者のdll）。
//!
//! ルームサーバーの手順は対戦と同じ（2人・先に入った方が`host`・後からが`guest`。ルーム名は`RoomName::spectate_of`）。
//! 配信側が先に入って観戦用のコード（種別3）をofferする。観戦者が後から入って`peer`で受け取り、返答（種別4）をanswerで返す。
//! 1回のルームのやり取りは`room::RoomClient`とdllの`net_room::RoomConn`が持つ。
//! **ここはその外側**: いつ接続し直すか・何回まで接続するか・いつ諦めるかだけを決める。
//! ソケットも時計も持たない（`now`は単調なmsを呼び手が渡す）。確保しない。監視スレッドで回す（ゲームのスレッドでは呼ばない）。
//! `poll`は[`Caster::next_wake`] / [`Joiner::next_wake`]の時刻以前に呼ぶ。
//! **入る回数の制限**: サーバーは1 IP（IPv6は /64）あたり60秒に30回までで429を返す。対戦のルームの入室や同じ家の別のPCも数える。
//! そこでどの60秒の期間でも、このステートマシンが入る回数は[`ENTRY_WINDOW_MAX`]（8）回を越えないことを保証する（`window_limited`）。
//! 観戦者は別に全体で30分（[`JOINER_GIVE_UP_MS`]）待っても会えなければ諦める（`JoinerFailure::GaveUp`）。
//! 配信側が観戦を断っていれば（4403）すぐ終わる。

use crate::code::CodeError;
use crate::room::{DEADLINE_GRACE_MS, Failure, Role, WATCH_ANSWER_TTL_MS, Why};

// ---------------- 定数 ----------------

/// 入る回数を数える期間の長さ（ms）。サーバーの回数制限の期間と同じ60秒。
pub const ENTRY_WINDOW_MS: u64 = 60_000;
/// どの[`ENTRY_WINDOW_MS`]の期間でも越えない入る回数。
/// サーバーの上限に対して同じIPの別の接続（対戦のルームの入室など）の分の余裕を残した値。
pub const ENTRY_WINDOW_MAX: usize = 8;

/// 配信側: 自分の外側のアドレスが変わって観戦用のコードを作り直すときに入り直すまで（ms）。サーバーは同じcidの前の入室を閉じるので待たない。
pub const CASTER_REMAP_MS: u64 = 0;
/// 配信側: 1000（手順を終えた）の後に入り直すまでの最短（ms）。
/// 観戦者が答えを登録してから穴あけを始めるまでの間を空けず、連続した入室で期間内の回数を使い切らない間隔。
pub const CASTER_REJOIN_MS: u64 = 2_000;
/// 配信側: 役`guest`（観戦者が一瞬いた）を受けて閉じた後に入り直すまで（ms）。
pub const CASTER_WRONG_ROLE_MS: u64 = 5_000;
/// 配信側: 4408（ルームの期限）の後に入り直すまで（ms）。ルームの期限切れは正常な繰り返しなのですぐ入り直す。
pub const CASTER_EXPIRED_MS: u64 = 0;
/// 配信側: 4410（相手が抜けた）の後に入り直すまで（ms）。
pub const CASTER_PEER_LEFT_MS: u64 = 2_000;
/// 配信側: 席を持っている間に来た4410（より新しいチケットの配信側に席を渡した）の後と、その後の入室も役`guest`だったときに入り直すまで（ms。5分）。
/// 同じルーム名で後から対戦を成立させた人が席を持つ間は、5秒ごとに入り直さない（その人が抜ければ次の入室で席が戻る）。
pub const CASTER_EVICTED_MS: u64 = 5 * 60 * 1000;
/// 配信側: 観戦者がコードを受け取った（`matched`）後に外側のアドレスが変わったとき、閉じて入り直すまでの最短（ms。`matched`から）。
/// サーバーはこの観戦者を`WATCH_ANSWER_TTL_MS`で閉じる。
/// それを過ぎても席にいれば、後から来る観戦者へ古い候補のコードを渡し続けていることになる。
pub const CASTER_REMAP_AFTER_MATCH_MS: u64 = WATCH_ANSWER_TTL_MS + DEADLINE_GRACE_MS;
/// 配信側: 409（ルームが満員）の後に入り直すまで（ms）。
pub const CASTER_ROOM_FULL_MS: u64 = 10_000;
/// 配信側: サーバーが応答しないときの最初の待ち（ms）。続けるたびに倍にする。
pub const CASTER_UNRESPONSIVE_FIRST_MS: u64 = 30_000;
/// 配信側: サーバーが応答しない間の待ちの上限（ms。5分）。
pub const CASTER_UNRESPONSIVE_MAX_MS: u64 = 5 * 60 * 1000;

/// 観戦者: 役`host`（配信側がいない）・409・4408・4410の後に入り直すまで（ms）。12秒なら60秒に5回で期間の8回に収まる。
pub const JOINER_RETRY_MS: u64 = 12_000;
/// 観戦者: 始めてから配信側に会えないまま諦めるまでの全体の長さ（ms。30分）。
pub const JOINER_GIVE_UP_MS: u64 = 30 * 60 * 1000;

/// 429の`retry_after`を待つ長さの下限（ms）。0秒と言われても休まずに接続し続けない。
pub const RETRY_AFTER_MIN_MS: u64 = 1_000;
/// 429の`retry_after`を待つ長さの上限（ms）。サーバーの値が壊れていても何年も待たない。
pub const RETRY_AFTER_MAX_MS: u64 = 5 * 60 * 1000;

fn retry_after_ms(secs: u32) -> u64 {
    (u64::from(secs).saturating_mul(1000)).clamp(RETRY_AFTER_MIN_MS, RETRY_AFTER_MAX_MS)
}

// ---------------- 入る回数の期間 ----------------

/// 入った時刻の直近[`ENTRY_WINDOW_MAX`]回を持つリングバッファ。
/// 新しい入室はその最も古い入室から[`ENTRY_WINDOW_MS`]以上空いたときだけ許す。
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

    /// 次に入ってよい最初の時刻。8回に満たなければ0。
    fn allowed_at(&self) -> u64 {
        if self.total < ENTRY_WINDOW_MAX as u64 {
            return 0;
        }
        // 次に上書きされる枠 = 8回前の入室。
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
    /// 始めたばかり（最初の入室。待ちは0）。
    Start,
    /// 配信側: 1000で1人ぶんの手順を終えた。
    Done,
    /// 4408（ルーム・answerの期限）。
    Expired,
    /// 4410（相手が抜けた）。
    PeerLeft,
    /// 役が合わなかった（配信側: 役`guest`。観戦者: 役 `host` = 配信側がまだいない）。
    WrongRole,
    /// 409（ルームが満員）。
    RoomFull,
    /// 429。
    RateLimited { retry_after_secs: u32 },
    /// サーバーが応答しない（`consecutive`は続けて何回目か）。配信側だけ。
    Unresponsive { consecutive: u32 },
    /// 配信側: 外側のアドレスが変わり、登録したコードを作り直す。
    Remapped,
    /// 配信側: より新しいチケットの配信側が席を持っている（4410で席を渡した・入り直しても役`guest`）。
    Evicted,
}

// ---------------- 配る側 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasterAction {
    Connect,
    /// 観戦用のコードを**いまの時刻から新しい期限で**作り、`RoomClient::submit_code`でofferせよ。
    MakeOffer,
    /// 観戦者の返答（`peer`の文字列）。
    /// `code::read_spectate_reply`で読み、読めたら候補から`spectate::PunchPlan`を作って受信スレッドへ渡せ（読めなければ捨てる）。
    Punch(String),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasterPhase {
    /// 接続していない。次の入室を待っている（`CasterStatus::next_entry_at`）。
    Waiting,
    Joining,
    /// 役`host`でルームにいる。観戦者を待っている（offerを登録した後）。
    Casting,
    /// サーバーがチケットを受け付けない（400 `bad_ticket`）。入り直しても変わらないのでもう入らない。
    GaveUp,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CasterStatus {
    pub phase: CasterPhase,
    /// `Waiting`のときに次に入る時刻（ms。待ちと入る回数の制限の遅い方）。
    pub next_entry_at: Option<u64>,
    /// いちばん最近の待ちの理由（入った後も次の待ちまで残る）。
    pub last_wait: Option<WaitReason>,
    /// 次の入室が入る回数の制限で予定より遅れているかどうか。
    pub window_limited: bool,
    pub entries_in_window: usize,
    pub entries_total: u64,
    /// サーバーが応答しない失敗が続いている回数（ほかの失敗か1000で0に戻る）。
    pub unresponsive_streak: u32,
}

/// 配信側（マッチのホストのdll）の入り直しの判断。止められるまでルームに入り直し続ける。
#[derive(Debug, Clone, Copy)]
pub struct Caster {
    phase: CasterPhase,
    /// `Waiting`のときに表の待ちだけで決まる入室の時刻（入る回数の制限は別に見る）。
    wait_until: u64,
    last_wait: Option<WaitReason>,
    window: EntryWindow,
    streak: u32,
    /// この入室で観戦者が来た（`matched`）時刻。来た後は登録したコードで手順を終えさせる。
    matched_at: Option<u64>,
    /// 観戦者が来た後に外側のアドレスが変わった。`CASTER_REMAP_AFTER_MATCH_MS`を過ぎても席にいれば閉じて入り直す。
    remap_pending: bool,
    /// より新しいチケットの配信側に席を渡した。役`host`で入れるまで入り直しの間隔を`CASTER_EVICTED_MS`にする。
    evicted: bool,
}

impl Caster {
    pub fn new(now: u64) -> Caster {
        Caster {
            phase: CasterPhase::Waiting,
            wait_until: now,
            last_wait: Some(WaitReason::Start),
            window: EntryWindow::NEW,
            streak: 0,
            matched_at: None,
            remap_pending: false,
            evicted: false,
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

    /// 観戦者が来た後の外側のアドレスの変化で閉じて入り直す時刻。
    fn remap_close_at(&self) -> Option<u64> {
        (self.phase == CasterPhase::Casting && self.remap_pending).then(|| {
            self.matched_at
                .map_or(0, |m| m.saturating_add(CASTER_REMAP_AFTER_MATCH_MS))
        })
    }

    /// 入る時刻になっていれば`Connect`（このとき入る回数を1つ数える）。
    /// 観戦者が来た後に外側のアドレスが変わっていて、その観戦者の答えの期限を過ぎても席にいれば`Close`。
    pub fn poll(&mut self, now: u64) -> Option<CasterAction> {
        if self.remap_close_at().is_some_and(|at| now >= at) {
            self.wait(now, CASTER_REMAP_MS, WaitReason::Remapped);
            return Some(CasterAction::Close);
        }
        if self.phase == CasterPhase::Waiting && now >= self.next_entry() {
            self.window.record(now);
            self.phase = CasterPhase::Joining;
            self.matched_at = None;
            self.remap_pending = false;
            return Some(CasterAction::Connect);
        }
        None
    }

    /// 次に`poll`を呼ぶべき時刻（ms）。待っていなければ`None`（イベントが来るまで）。
    pub fn next_wake(&self) -> Option<u64> {
        if self.phase == CasterPhase::Waiting {
            return Some(self.next_entry());
        }
        self.remap_close_at()
    }

    /// サーバーの`matched`（配信側が登録したコードを観戦者が受け取った）。観戦用のルームでは1回の入室で1度しか来ない。
    /// 答えずに抜けた観戦者の後に来る観戦者のことは、配信側には知らされない。
    pub fn on_matched(&mut self, now: u64) {
        if self.phase == CasterPhase::Casting && self.matched_at.is_none() {
            self.matched_at = Some(now);
        }
    }

    /// 自分の外側のアドレスが変わり、観戦用のコードの候補を作り直した。
    /// 登録したコードを誰も受け取っていなければ閉じて入り直す（`Close`。新しい入室で新しいコードを登録する）。
    /// 観戦者が受け取った後はその観戦者の答えの期限（`CASTER_REMAP_AFTER_MATCH_MS`）までは閉じずに手順を終えさせる。
    /// 期限を過ぎても席にいれば`poll`が閉じる。
    /// 入る前・入っている途中は何もしない（入ったときに作るコードがもう新しい）。
    pub fn on_remapped(&mut self, now: u64) -> Option<CasterAction> {
        if self.phase != CasterPhase::Casting {
            return None;
        }
        if let Some(m) = self.matched_at
            && now < m.saturating_add(CASTER_REMAP_AFTER_MATCH_MS)
        {
            self.remap_pending = true;
            return None;
        }
        self.wait(now, CASTER_REMAP_MS, WaitReason::Remapped);
        Some(CasterAction::Close)
    }

    /// より新しいチケットの配信側に席を渡した後かどうか（役`host`で入れると戻る）。
    pub fn evicted(&self) -> bool {
        self.evicted
    }

    pub fn on_joined(&mut self, now: u64, role: Role) -> Option<CasterAction> {
        if self.phase != CasterPhase::Joining {
            return None;
        }
        match role {
            Role::Offer => {
                self.phase = CasterPhase::Casting;
                self.evicted = false;
                Some(CasterAction::MakeOffer)
            }
            // チケットの無い観戦者は空のルームで席を取れない。
            // そのため役`guest`になるのは、自分の前の接続が残っていたか、より新しいチケットの配信側がいるかのどちらか。
            // 席を渡した後なら後者として長く待つ。
            Role::Answer if self.evicted => {
                self.wait(now, CASTER_EVICTED_MS, WaitReason::Evicted);
                Some(CasterAction::Close)
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
        if f == Failure::BadTicket {
            self.phase = CasterPhase::GaveUp;
            return;
        }
        // 観戦用のルームで席を持つ配信側へ4410が来るのは、より新しいチケットの配信側が席を取ったときだけ（観戦者が抜けても配信側は閉じられない）。
        if f == Failure::PeerLeft && self.phase == CasterPhase::Casting {
            self.streak = 0;
            self.evicted = true;
            self.wait(now, CASTER_EVICTED_MS, WaitReason::Evicted);
            return;
        }
        // 4403は観戦者にしか来ない。来たら応答しないときと同じにゆっくり入り直す。
        if matches!(
            f,
            Failure::Unresponsive(_) | Failure::BadRoomName | Failure::Refused
        ) {
            self.streak = self.streak.saturating_add(1);
            // 30秒 → 60 → 120 → 240 → 300（上限）。倍にする回数は10で頭打ちにして溢れさせない。
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
            // room_in_useは対戦のルームにだけ返る。観戦用のルームに来たら満員と同じに待つ。
            Failure::RoomFull | Failure::RoomInUse => {
                self.wait(now, CASTER_ROOM_FULL_MS, WaitReason::RoomFull)
            }
            Failure::RateLimited { retry_after_secs } => self.wait(
                now,
                retry_after_ms(retry_after_secs),
                WaitReason::RateLimited { retry_after_secs },
            ),
            Failure::Unresponsive(_)
            | Failure::BadRoomName
            | Failure::Refused
            | Failure::BadTicket => {}
        }
    }

    /// 止める（接続の後始末）。以後`poll`は何も返さず、イベントは無視する。
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
    /// 観戦用のコード（`peer`の文字列）。`code::read_spectate`で読み、返答を作ってanswerせよ。
    /// 出せたら[`Joiner::on_answer_sent`]を呼び、読めない・作れない・出せないなら[`Joiner::abort`]を呼べ。
    SpectateCode(String),
}

/// 観戦者が失敗で終わった理由（画面に出す理由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinerFailure {
    Unresponsive(Why),
    BadRoomName,
    /// 観戦用のコードを読めない・返答を作れない（中は理由。バージョン違い`BadVersion`を含む）。
    CodeUnreadable(CodeError),
    ReplyNotSent,
    GaveUp,
    /// 配信側が観戦を断っている（4403。2人のどちらかが観戦を許可していない）。
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinerPhase {
    /// 接続していない。次の入室を待っている（`last_wait`が`WrongRole`なら「配る側を待っている」）。
    Waiting,
    Joining,
    /// 役`guest`でルームにいる。観戦用のコードを待っている（または読んで返答を作っている）。
    InRoom,
    AnswerSent,
    /// 終わり（成功）。次は`Watcher`がJOINを送る。
    Succeeded,
    Failed(JoinerFailure),
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinerStatus {
    pub phase: JoinerPhase,
    /// `Waiting`のとき、次に入る時刻（ms）。
    pub next_entry_at: Option<u64>,
    /// いちばん最近の待ちの理由。`InRoom`に入ると消える。
    pub last_wait: Option<WaitReason>,
    pub window_limited: bool,
    pub entries_in_window: usize,
    pub entries_total: u64,
    pub give_up_at: u64,
}

impl JoinerStatus {
    /// 「相手（配る側）を待っています」と出せる状態かどうか: 配信側がまだルームにいなかったので入り直しを待っている間。
    /// 配信側がいない観戦用のルームはサーバーがhostの役ではなく409 `room_full`で断る（`WrongRole`は古いサーバーの断り方）。
    pub fn waiting_for_caster(&self) -> bool {
        matches!(
            self.last_wait,
            Some(WaitReason::WrongRole | WaitReason::RoomFull)
        ) && matches!(self.phase, JoinerPhase::Waiting | JoinerPhase::Joining)
    }
}

/// 観戦者（観戦者のdll）の入り直しの判断。配信側に会えるか、失敗するか、30分経つまで入り直す。
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

    /// 全体の時間切れなら諦め（接続があれば`Close`）、入る時刻になっていれば`Connect`。`AnswerSent`の後は諦めない
    /// （あと少しで1000が来る）。
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

    /// ルームに入れた。役`host`は配信側がまだいない。
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

    /// answerを出した。サーバーの1000を待つ。
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
            // チケットは配信側だけが付けるので観戦者に400 `bad_ticket`は来ないはず。来たらルーム名が不正なときと同じに終える。
            Failure::BadRoomName | Failure::BadTicket => {
                self.phase = JoinerPhase::Failed(JoinerFailure::BadRoomName)
            }
            Failure::Refused => self.phase = JoinerPhase::Failed(JoinerFailure::Refused),
            Failure::RateLimited { retry_after_secs } => self.wait(
                now,
                retry_after_ms(retry_after_secs),
                WaitReason::RateLimited { retry_after_secs },
            ),
            Failure::RoomFull | Failure::RoomInUse => {
                self.wait(now, JOINER_RETRY_MS, WaitReason::RoomFull)
            }
            Failure::Expired => self.wait(now, JOINER_RETRY_MS, WaitReason::Expired),
            Failure::PeerLeft => self.wait(now, JOINER_RETRY_MS, WaitReason::PeerLeft),
        }
    }

    /// dllが続けられなくなった（コードを読めない・返答を作れない・answerを出せない）。
    /// 失敗で終わり、接続を閉じさせる。
    /// 終わっている・接続していないステートマシンには何もしない。
    pub fn abort(&mut self, why: JoinerFailure) -> Option<JoinerAction> {
        if matches!(self.phase, JoinerPhase::InRoom | JoinerPhase::AnswerSent) {
            self.phase = JoinerPhase::Failed(why);
            return Some(JoinerAction::Close);
        }
        None
    }

    /// 利用者がやめた。接続していれば`Close`。終わっていたステートマシンの結果は書き換えない。
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

    /// 一番混む期間の入る回数を返す。`times`のどの60秒の期間（`[t, t + 60 秒)`）にも9回以上入っていないことの検査に使う。
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
        assert_eq!(CASTER_REMAP_MS, 0);
        assert_eq!(CASTER_REJOIN_MS, 2_000);
        assert_eq!(CASTER_WRONG_ROLE_MS, 5_000);
        assert_eq!(CASTER_EXPIRED_MS, 0);
        assert_eq!(CASTER_PEER_LEFT_MS, 2_000);
        assert_eq!(CASTER_EVICTED_MS, 300_000);
        assert_eq!(CASTER_REMAP_AFTER_MATCH_MS, 15_000);
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
    fn a_remap_rejoins_only_while_the_offer_waits_unclaimed() {
        // 待っている・入っている途中は閉じない（入ったときに作るコードがもう新しい）。
        let mut c = Caster::new(NOW);
        assert_eq!(c.on_remapped(NOW), None);
        assert_eq!(c.poll(NOW), Some(CasterAction::Connect));
        assert_eq!(c.on_remapped(NOW + 5), None);
        assert_eq!(c.status(NOW + 5).phase, CasterPhase::Joining);
        // 登録したコードを誰も受け取っていなければ閉じてすぐ入り直す。
        assert_eq!(
            c.on_joined(NOW + 10, Role::Offer),
            Some(CasterAction::MakeOffer)
        );
        assert_eq!(c.on_remapped(NOW + 20), Some(CasterAction::Close));
        let s = c.status(NOW + 20);
        assert_eq!(s.phase, CasterPhase::Waiting);
        assert_eq!(s.last_wait, Some(WaitReason::Remapped));
        assert_eq!(s.next_entry_at, Some(NOW + 20));
        assert_eq!(c.poll(NOW + 20), Some(CasterAction::Connect));
        assert_eq!(
            c.on_joined(NOW + 30, Role::Offer),
            Some(CasterAction::MakeOffer)
        );
        // 観戦者が受け取った後はその手順を壊さない。
        c.on_matched(NOW + 35);
        assert_eq!(c.on_remapped(NOW + 40), None);
        assert_eq!(c.status(NOW + 40).phase, CasterPhase::Casting);
        assert_eq!(
            c.on_peer_code("ReplyText".to_string()),
            Some(CasterAction::Punch("ReplyText".to_string()))
        );
        c.on_done(NOW + 50);
        // 次の入室ではまた閉じて入り直せる（前の入室のmatchedを持ち越さない）。
        assert_eq!(
            c.poll(NOW + 50 + CASTER_REJOIN_MS),
            Some(CasterAction::Connect)
        );
        c.on_joined(NOW + 2_060, Role::Offer);
        assert_eq!(c.on_remapped(NOW + 2_070), Some(CasterAction::Close));
        // 止めた後は何もしない。
        c.stop();
        assert_eq!(c.on_remapped(NOW + 2_080), None);
        // 入る回数の制限は入り直しにも効く。
        let mut c = Caster::new(NOW);
        let mut entries = Vec::new();
        let mut t = NOW;
        while t < NOW + 3 * ENTRY_WINDOW_MS {
            if c.poll(t) == Some(CasterAction::Connect) {
                entries.push(t);
                c.on_joined(t, Role::Offer);
            }
            let _ = c.on_remapped(t);
            t += 100;
        }
        assert_eq!(busiest_window(&entries), ENTRY_WINDOW_MAX, "{entries:?}");
    }

    #[test]
    fn a_remap_after_a_match_closes_once_the_viewers_answer_window_has_passed() {
        let mut c = caster_joining(NOW);
        c.on_joined(NOW, Role::Offer);
        c.on_matched(NOW + 100);
        // 2度目のmatchedは来ないはず。来ても最初の時刻のまま。
        c.on_matched(NOW + 9_000);
        assert_eq!(c.next_wake(), None, "席にいて変化が無ければ起こさない");
        assert_eq!(c.on_remapped(NOW + 1_000), None);
        let at = NOW + 100 + CASTER_REMAP_AFTER_MATCH_MS;
        assert_eq!(c.next_wake(), Some(at));
        assert_eq!(c.poll(at - 1), None);
        assert_eq!(c.status(at - 1).phase, CasterPhase::Casting);
        assert_eq!(c.poll(at), Some(CasterAction::Close));
        let s = c.status(at);
        assert_eq!(s.phase, CasterPhase::Waiting);
        assert_eq!(s.last_wait, Some(WaitReason::Remapped));
        assert_eq!(c.poll(at), Some(CasterAction::Connect));
        // 次の入室には持ち越さない。
        c.on_joined(at, Role::Offer);
        assert_eq!(c.poll(at + 10 * CASTER_REMAP_AFTER_MATCH_MS), None);

        // 期限を過ぎてからの変化はすぐ閉じる。
        let mut c = caster_joining(NOW);
        c.on_joined(NOW, Role::Offer);
        c.on_matched(NOW);
        assert_eq!(
            c.on_remapped(NOW + CASTER_REMAP_AFTER_MATCH_MS),
            Some(CasterAction::Close)
        );

        // 観戦者が答えて手順を終えたら変化のフラグは残らない。
        let mut c = caster_joining(NOW);
        c.on_joined(NOW, Role::Offer);
        c.on_matched(NOW);
        assert_eq!(c.on_remapped(NOW + 10), None);
        c.on_done(NOW + 20);
        assert_eq!(
            c.poll(NOW + CASTER_REMAP_AFTER_MATCH_MS),
            Some(CasterAction::Connect)
        );
    }

    #[test]
    fn a_caster_that_lost_its_seat_to_a_newer_ticket_returns_only_every_five_minutes() {
        let mut c = caster_joining(NOW);
        c.on_joined(NOW, Role::Offer);
        assert!(!c.evicted());
        c.on_failed(NOW + 100, Failure::PeerLeft);
        assert!(c.evicted());
        let s = c.status(NOW + 100);
        assert_eq!(s.phase, CasterPhase::Waiting);
        assert_eq!(s.last_wait, Some(WaitReason::Evicted));
        assert_eq!(s.next_entry_at, Some(NOW + 100 + CASTER_EVICTED_MS));
        let mut t = NOW + 100 + CASTER_EVICTED_MS;
        assert_eq!(c.poll(t - 1), None);
        // 新しいチケットの配信側がまだいる: 役guestを何度受けても5分ごと。
        for _ in 0..3 {
            assert_eq!(c.poll(t), Some(CasterAction::Connect));
            assert_eq!(c.on_joined(t + 10, Role::Answer), Some(CasterAction::Close));
            let s = c.status(t + 10);
            assert_eq!(s.last_wait, Some(WaitReason::Evicted));
            assert_eq!(s.next_entry_at, Some(t + 10 + CASTER_EVICTED_MS));
            assert_eq!(
                c.poll(t + 10 + CASTER_WRONG_ROLE_MS),
                None,
                "5 秒で入り直した"
            );
            t += 10 + CASTER_EVICTED_MS;
        }
        // 席が空けば戻り、以後の役guestは今までどおり5秒。
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        assert_eq!(c.on_joined(t, Role::Offer), Some(CasterAction::MakeOffer));
        assert!(!c.evicted());
        c.on_done(t);
        t += CASTER_REJOIN_MS;
        assert_eq!(c.poll(t), Some(CasterAction::Connect));
        c.on_joined(t, Role::Answer);
        assert_eq!(c.status(t).last_wait, Some(WaitReason::WrongRole));
        assert_eq!(c.status(t).next_entry_at, Some(t + CASTER_WRONG_ROLE_MS));
        // 席に着く前の4410は席を渡したことを示さない（2秒で入り直す）。
        let mut c = caster_joining(NOW);
        c.on_failed(NOW, Failure::PeerLeft);
        assert!(!c.evicted());
        assert_eq!(c.status(NOW).next_entry_at, Some(NOW + CASTER_PEER_LEFT_MS));
    }

    #[test]
    fn a_caster_whose_ticket_the_server_refuses_gives_up_for_good() {
        for in_room in [false, true] {
            let mut c = caster_joining(NOW);
            if in_room {
                c.on_joined(NOW, Role::Offer);
            }
            c.on_failed(NOW + 10, Failure::BadTicket);
            let s = c.status(NOW + 10);
            assert_eq!(s.phase, CasterPhase::GaveUp, "{in_room}");
            assert_eq!(s.next_entry_at, None);
            assert_eq!(c.next_wake(), None);
            for t in [NOW + 10, NOW + 3_600_000, u64::MAX] {
                assert_eq!(c.poll(t), None, "{in_room} {t}");
            }
            c.on_done(NOW + 20);
            c.on_failed(NOW + 30, Failure::Expired);
            assert_eq!(c.on_remapped(NOW + 40), None);
            assert_eq!(c.status(NOW + 40).phase, CasterPhase::GaveUp);
            c.stop();
            assert_eq!(c.status(NOW + 40).phase, CasterPhase::Stopped);
        }
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
                if f == Failure::PeerLeft && in_room {
                    // 席を持つ間の4410は席を渡したことを示す（上の検査）。
                    continue;
                }
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
    fn a_joiner_refused_with_room_full_shows_waiting_for_the_caster_but_other_waits_do_not() {
        for (f, waiting) in [
            (Failure::RoomFull, true),
            (Failure::Expired, false),
            (Failure::PeerLeft, false),
            (
                Failure::RateLimited {
                    retry_after_secs: 5,
                },
                false,
            ),
        ] {
            for in_room in [false, true] {
                let mut j = joiner_joining(NOW);
                if in_room {
                    j.on_joined(NOW, Role::Answer);
                }
                j.on_failed(NOW + 10, f);
                let s = j.status(NOW + 10);
                assert_eq!(s.phase, JoinerPhase::Waiting, "{f:?}");
                assert_eq!(s.waiting_for_caster(), waiting, "{f:?} in_room={in_room}");
            }
        }
        // 古いサーバーの断り方（hostの役が来る）も待機中のまま。
        let mut j = joiner_joining(NOW);
        j.on_joined(NOW, Role::Offer);
        assert!(j.status(NOW).waiting_for_caster());
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
    fn a_joiner_refused_by_the_caster_ends_at_once_and_never_reenters() {
        for in_room in [false, true] {
            let mut j = joiner_joining(NOW);
            if in_room {
                j.on_joined(NOW, Role::Answer);
            }
            j.on_failed(NOW + 10, Failure::Refused);
            assert_eq!(
                j.status(NOW + 10).phase,
                JoinerPhase::Failed(JoinerFailure::Refused),
                "{in_room}"
            );
            assert_eq!(j.next_wake(), None);
            assert_eq!(j.poll(NOW + JOINER_GIVE_UP_MS), None, "入り直した");
        }
        // 配信側には来ないはず。来ても止まらずゆっくり入り直す。
        let mut c = caster_joining(NOW);
        c.on_failed(NOW, Failure::Refused);
        assert_eq!(c.status(NOW).next_entry_at, Some(NOW + 30_000));
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

    // ---------------- 乱れたイベントの列 ----------------

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
            5 => Failure::Refused,
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
        // 母数: 配信側は200本で数千回入る（0件の列では保証を見ていない）。
        assert!(total_caster > 1_000, "配る側の入室 {total_caster}");
        assert!(total_joiner > 200, "観戦者の入室 {total_joiner}");
    }
}
