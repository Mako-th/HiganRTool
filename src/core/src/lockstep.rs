//! マッチの中の lockstep の状態機械。ソケットも時計も持たない。dll（と peer_sim）が 1 フレームに次の順で呼ぶ:
//!
//! ```text
//! 読み込みに入るとき / 試合の場面を出るとき: ls.begin_seg(次の seg)
//! if !ls.captured() { ls.capture(local)?; send(ls.build_input()) }   // 取得は 1 フレームに 1 回
//! 待つ間: 届いた INPUT を ls.on_input() に渡し、4 ms ごとに send(ls.build_input())
//! if let Some(a) = ls.advance() { feed(by_seat(my_seat, a.local, a.remote)) }
//! else { 描画だけのフレーム（次の do_frame で同じフレームをやり直す。capture はし直さない）}
//! いつでも: on_input が PrevSeg { reply: true } なら send(ls.build_prev_input())
//! ```
//!
//! ゲームスレッドで呼ぶので、ヒープを使わず、パニックしうる添字を置かない。入力ワードは固定長のリング（`word_ring`）に置き、
//! 印が合わない枠は「無い」と読む。

use crate::delay_pick::{AUTO, AutoHold, Recommend};
use crate::frame::{MAX_DELAY, apply_frame, is_prefill};
use crate::wire::{Input, LatChange, MAX_WORDS, Seat};
use crate::word_ring::{RING, Ring, in_window};

/// 相手の入力を待つ間に `build_input()` を送り直す間隔（ms）。この状態機械は時計を持たないので、呼び手（dll と peer_sim）が数える。
pub const RESEND_MS: u32 = 4;

/// ホストかゲストか（role_nonce の大きい方がホスト）。latency の切替を出せるのはホストだけ（ゲストは `press_delay` で要求する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Guest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatError {
    /// ゲストは切替を出せない（ゲストは `press_delay` で要求する。frame 0 の lat はホストが INPUT ごと捨てる）。
    NotHost,
    /// 0〜9 の外（`frame::MAX_DELAY`）。
    OutOfRange,
    /// いまの delay と同じ（待っている切替があれば取り下げる）。
    Unchanged,
    /// 前の切替を両者が使い終えていない。INPUT に載る切替はいつも 1 つ。条件は `request_delay` の説明。
    Busy,
    /// 取り決めで rollback のモードに決まった接続（切替は「a のワードが届かなければ a に進めない」に頼るが、予測は a を越えるので出さない）。
    RollbackMode,
}

/// 遅れの量の押下（`press_delay`）がどう扱われたか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayPress {
    /// ホスト（rollback）: 予定を出した。取得のフレーム `frame` から新しい D。
    Scheduled { frame: u32 },
    /// ホスト（rollback）: ゲストがこの seg の D を知ったと分かるまで持っておく（ゲート）。
    Held,
    /// ホスト（delay 方式）: 次の取得で切替に結び付ける。前の切替を両者が使い終えていなければ、使い終えた後の取得で。
    Queued,
    /// ゲスト: ホストへ要求した（INPUT の lat に `seq` で載せる）。
    Asked { seq: u32 },
    /// 今の値（決まっている値か、要求している値）と同じ。何もしない。
    Unchanged,
    /// 0〜9 の外。または、この状態機械では変えない（rollback の状態機械の delay 方式）。
    Refused,
}

/// ゲストの要求を INPUT に載せ続ける上限（取得の数。60 fps で 2 秒）。押した値が通らなかったとき（同じ瞬間にホストが別の値を押して
/// 後勝ち等）に諦める。
pub const WISH_FRAMES: u8 = 120;

/// ゲストの要求（`press_delay`）。INPUT の lat に {frame: seq, delay: x} として載せる（seq は 1 から。0 と `u32::MAX` は使わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wish {
    pub(crate) seq: u32,
    pub(crate) x: u8,
    /// 押した時点の「予定・切替を受けた回数」。これより後に受けた予定・切替で値が `x` なら、通ったと見てやめる。
    pub(crate) mark: u32,
    /// 残りの取得の数（`WISH_FRAMES` から減らす）。
    pub(crate) left: u8,
}

impl Wish {
    /// 次の seq（1 から。`u32::MAX` に届いたら 1 へ戻さず止める）。
    pub(crate) fn next_seq(last: u32) -> u32 {
        last.saturating_add(1).clamp(1, u32::MAX - 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    /// このフレームはもう取った（描画だけのフレームの後でも取り直さない）。
    AlreadyCaptured,
    Full,
    NoDelay,
}

/// `on_input` の結果。`Accepted` 以外は、中身を 1 つも使わずに捨てた。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvOutcome {
    /// 受け入れた。`new_words` は新しく入ったワードの数、`conflicts` はもう持っている apply_frame に違うワードが来た数（最初のワードを守る）。
    Accepted { new_words: u8, conflicts: u8 },
    /// 自分より 1 つ前の seg の INPUT。`reply` なら相手はまだ前の seg のワードを待っているので `build_prev_input()` を送り返す。
    PrevSeg { reply: bool },
    /// 自分と違う seg。
    OtherSeg,
    /// ホストが受けた、frame 0 の lat を載せた INPUT（丸ごと捨てる。ゲストの lat は要求で frame ≥ 1 なので、frame 0 は取り決めの違反）。
    GuestLat,
    /// base が受理期間の外。
    OutOfWindow,
    /// ack が、自分がまだ送っていない apply_frame を指す（正しい相手なら起きない）。
    AckAhead,
    /// 切替の delay が 0〜9 の外。rollback のゲストでは、lat が無い・frame が 0 でない・D と違う値も。
    BadLat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub frame: u32,
    pub local: u16,
    pub remote: u16,
    /// このフレームで latency の切替を使ったら、その新しい delay（capture_frame frame+1 から効く）。
    pub switched_to: Option<u8>,
}

/// 目に見えない食い違いの数（ログに出す）。どれも正しい相手なら 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub conflicts: u32,
    /// 使い終えたフレームを指す、使ったものと違う切替が届いた数（両者が食い違う兆し）。
    pub lat_late: u32,
    /// 使う前の切替を持っている間に、別の切替が届いた数（最初の方を守った）。
    pub lat_conflicts: u32,
    /// delay を減らしたときに捨てた自分の取得の数（k 回。食い違いではない）。
    pub discarded_captures: u32,
    /// ゲスト: Ctrl＋数字で要求した数（`DelayPress::Asked`）/ 届いた印を見ずに `WISH_FRAMES` で諦めた数。
    pub wish_sent: u32,
    pub wish_expired: u32,
    /// ホスト: ゲストの要求を取った数 / 前に受けた seq 以下なので lat だけ無視した数（同じ要求の重ね送り）。
    pub wish_taken: u32,
    pub wish_dup: u32,
    /// ホスト: 押下・要求の値を切替に結び付けた数 / 前の切替を使い終えていないので次の取得へ回した数（`Busy`）。
    pub want_bound: u32,
    pub want_busy: u32,
    /// ホスト: seg の始まりの見直しで、素の推奨値なら遅れが替わるのに、変わりにくくして据え置いた数。
    pub auto_held: u32,
}

/// マッチの中の lockstep（1 接続に 1 つ）。
///
/// ## ワードの列の持ち方
///
/// - 自分の列: apply_frame `seg_start_delay` から `local_next` の手前まで、途切れずに並ぶ。取得のたびに 1 ワード（latency の切替の境目では
///   k ワード埋める / 取得を捨てる）。
/// - 相手の列: `remote_next`（= 送る ack）の手前まで全部届いている。先のワードが先に届いたらリングに置いておき、穴が埋まったら ack を進める。
/// - 相手の ack（`peer_ack`）より前の自分のワードは送らない。送る INPUT は `peer_ack` から最大 32 ワード。
///
/// ## latency の切替
///
/// ホストの `request_delay(new)` は、次に列へ入る自分の取得（apply_frame a）に結び付き、`LatChange { frame: a, delay: new }` として
/// 相手の ack が a を越えるまで毎回の INPUT に載る。両者ともフレーム a を使うとき（`advance`）に delay を替え、capture_frame a+1 から
/// 新しい delay で取る。増えた分は `capture` が直前のワードで埋め、減った分は `capture` が取得を捨てる。
///
/// ## 両者が変える
///
/// `press_delay(x)` は役で分かれる。**ホスト**は値を `want` に置き（後勝ち）、次の `capture` の先頭で `request_delay` へ渡す（`Busy` なら
/// 使い終えた後の取得まで持つ）。**ゲスト**は要求（`Wish`）を毎回の INPUT の lat に {frame: seq, delay: x} で載せる。ホストは seq が
/// 前に受けたものより大きければ 1 回だけ取り、範囲（`delay_pick::bounds`）に詰めて `want` に置く（同時に押したらホストに着いた順の
/// 後勝ち）。ゲストは、押した後に受けた切替で値が x のものを見るか、`WISH_FRAMES` 回の取得の後に載せるのをやめる。frame 0 のゲストの
/// lat は取り決めの違反として INPUT ごと捨てる（`GuestLat`）。遅延の増分: マッチ中の入力は 0。
///
/// ## seg の境目
///
/// `begin_seg` は、その時点で送るはずだった INPUT（前の seg の未確認のワード。32 ワードまで）を 1 つだけ取っておく。自分が前の seg の最後の
/// フレームを使って先に抜けても、相手はまだ前の seg で自分の最後のワードを待っていることがある（最後の INPUT が落ちた）。相手はそれを
/// 4 ms ごとに送り直してくるので、`on_input` が `PrevSeg { reply: true }` を返したらバックアップを送り返す。これが無いと、先に抜けた側は
/// 前の seg のワードを送らず、相手は前の seg で止まったままになる。マッチの場面を出るときも同じなので、dll は場面を出るときにも
/// `begin_seg` してバックアップを返し続ける。
#[derive(Debug, Clone)]
pub struct Lockstep {
    role: Role,
    seg: u16,
    /// いま有効な delay（0〜9）。seg をまたいで引き継ぐ。
    delay: u8,
    /// この seg に入ったときの delay。apply_frame がこれより小さいフレームは両者とも 0。
    seg_start_delay: u8,
    frame: u32,
    captured: bool,
    local: Ring,
    local_next: u32,
    remote: Ring,
    /// 相手から次に要る apply_frame（送る ack）。
    remote_next: u32,
    /// 相手から届いた ack のうち最大のもの（それより前の自分のワードは相手が持っている）。
    peer_ack: u32,
    /// ホスト: まだ取得に結び付いていない切替の要求（新しい delay）。
    request: Option<u8>,
    /// 両者がフレーム a で使う切替（ホスト: 結び付けたもの / ゲスト: 受け取ったもの）。
    pending: Option<LatChange>,
    /// ホスト: INPUT に載せ続ける切替（相手の ack が a を越えるまで）。
    carry: Option<LatChange>,
    /// ホスト: 最後に結び付けた切替と、そのときの旧 delay（Busy の判定用）。
    last_bound: Option<(LatChange, u8)>,
    /// 最後に使った切替（重複して届いた同じ切替を見分ける）。
    last_applied: Option<LatChange>,
    /// 前の seg のバックアップ（`begin_seg` の時点の `build_input()`）。
    prev: Option<Input>,
    /// 前の seg で自分が使ったフレームの数。相手が要るのは apply_frame < prev_end だけ。
    prev_end: u32,
    stats: Stats,
    /// ホスト: 次の取得で `request_delay` に渡す値（自分の押下とゲストの要求。後勝ち）。seg をまたいで持つ。
    want: Option<u8>,
    /// ホスト: 最後に取ったゲストの要求の seq（接続の間。seg で戻さない）。
    last_wish_seq: u32,
    /// ゲスト: 載せている要求と、最後に使った seq。
    wish: Option<Wish>,
    wish_seq: u32,
    /// ゲスト: 受けた切替の数（要求が通ったかを見る印）。
    switch_gen: u32,
    /// ホスト: auto の旗と seg の始まりの見直しの規則（`delay_pick::AutoHold`。接続ごとに 1 つ）と、呼び手が最後に渡した推奨値。
    auto: AutoHold,
    rec: Option<Recommend>,
}

impl Lockstep {
    pub fn new(role: Role, delay: u8) -> Self {
        let delay = delay.min(MAX_DELAY);
        let d = u32::from(delay);
        Lockstep {
            role,
            seg: 0,
            delay,
            seg_start_delay: delay,
            frame: 0,
            captured: false,
            local: Ring::new(),
            local_next: d,
            remote: Ring::new(),
            remote_next: d,
            peer_ack: d,
            request: None,
            pending: None,
            carry: None,
            last_bound: None,
            last_applied: None,
            prev: None,
            prev_end: 0,
            stats: Stats::default(),
            want: None,
            last_wish_seq: 0,
            wish: None,
            wish_seq: 0,
            switch_gen: 0,
            auto: AutoHold::default(),
            rec: None,
        }
    }

    pub fn begin_seg(&mut self, seg: u16) {
        self.prev = Some(self.build_input());
        self.prev_end = self.frame;
        let d = u32::from(self.delay);
        self.seg = seg;
        self.seg_start_delay = self.delay;
        self.frame = 0;
        self.captured = false;
        self.local.clear();
        self.local_next = d;
        self.remote.clear();
        self.remote_next = d;
        self.peer_ack = d;
        self.request = None;
        self.pending = None;
        self.carry = None;
        self.last_bound = None;
        self.last_applied = None;
    }

    /// 形式の切替で、この機械がマッチを動かす側に替わるとき（最初の取得の前）。遅れを新しい値 `d` にして seg の始まりを作り直す（`begin_seg` と
    /// 同じ形。バックアップ `prev` は触らない）。押下・要求は切替の前の値なので捨てる。
    pub fn restart_head(&mut self, d: u8, auto: bool) {
        let d = d.min(MAX_DELAY);
        self.delay = d;
        self.want = None;
        self.wish = None;
        if self.role == Role::Host {
            if auto {
                self.auto.raise();
            } else {
                self.auto.lower();
            }
        }
        let n = u32::from(d);
        self.seg_start_delay = d;
        self.frame = 0;
        self.captured = false;
        self.local.clear();
        self.local_next = n;
        self.remote.clear();
        self.remote_next = n;
        self.peer_ack = n;
        self.request = None;
        self.pending = None;
        self.carry = None;
        self.last_bound = None;
        self.last_applied = None;
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn seg(&self) -> u16 {
        self.seg
    }

    pub fn frame(&self) -> u32 {
        self.frame
    }

    /// 相手から次に要る apply_frame（送る ack）。peer_sim の「付いていく」形が、相手がどのフレームまで取ったかを見るのに使う。
    pub fn remote_next(&self) -> u32 {
        self.remote_next
    }

    pub fn delay(&self) -> u8 {
        self.delay
    }

    /// 両者がこの先で使う切替（画面に「次のフレーム a から n」と出すため）。
    pub fn pending_switch(&self) -> Option<LatChange> {
        self.pending
    }

    pub fn shown_delay(&self) -> u8 {
        let mine = match self.role {
            Role::Host => self.want.or(self.request),
            // auto の要求は値ではない（9 を画面に出さない）。
            Role::Guest => self.wish.map(|w| w.x).filter(|&x| x != AUTO),
        };
        mine.or(self.pending.map(|sw| sw.delay))
            .unwrap_or(self.delay)
    }

    /// 遅れの量の押下（両者。上の「両者が変える」）。ホストは次の取得で切替に結び付け、ゲストはホストへ要求する。
    pub fn press_delay(&mut self, x: u8) -> DelayPress {
        self.press_delay_forced(x, false)
    }

    /// `press_delay` の、同じ値でも要求するバージョン（ゲストだけが見る `force`）。
    pub fn press_delay_forced(&mut self, x: u8, force: bool) -> DelayPress {
        if x > MAX_DELAY {
            return DelayPress::Refused;
        }
        match self.role {
            Role::Host if x == AUTO => {
                self.auto.raise();
                self.auto_now()
            }
            Role::Host => {
                self.auto.lower();
                if self.shown_delay() == x {
                    return DelayPress::Unchanged;
                }
                self.want = Some(x);
                DelayPress::Queued
            }
            Role::Guest => {
                let latest = self.pending.map_or(self.delay, |sw| sw.delay);
                let same = !force
                    && match self.wish {
                        Some(w) => w.x == x,
                        None => latest == x,
                    };
                if same {
                    return DelayPress::Unchanged;
                }
                self.wish_seq = Wish::next_seq(self.wish_seq);
                self.wish = Some(Wish {
                    seq: self.wish_seq,
                    x,
                    mark: self.switch_gen,
                    left: WISH_FRAMES,
                });
                self.stats.wish_sent = self.stats.wish_sent.saturating_add(1);
                DelayPress::Asked { seq: self.wish_seq }
            }
        }
    }

    /// ホスト: 押した瞬間・ゲストの auto の要求で、素の推奨値を `want` に置く。推奨値が無ければ何もしない（旗は立ったまま）。
    fn auto_now(&mut self) -> DelayPress {
        match self.auto.pick_now(self.rec) {
            Some(x) => self.want_auto(x),
            None => DelayPress::Unchanged,
        }
    }

    /// ホスト: seg の始まりの最初の取得で、旗が立っていれば遅れを見直して `want` に置く（今の目標は上げにくく・下げにくく）。マッチの途中では
    /// 呼ばない。
    fn auto_head(&mut self) {
        if let Some(p) = self.auto.pick_head(self.shown_delay(), self.rec) {
            if p.held {
                self.stats.auto_held = self.stats.auto_held.saturating_add(1);
            }
            self.want_auto(p.d);
        }
    }

    /// auto が決めた値 `x` を `want` に置く。今の目標と同じ値なら何も出さない。
    fn want_auto(&mut self, x: u8) -> DelayPress {
        if self.shown_delay() == x {
            return DelayPress::Unchanged;
        }
        self.want = Some(x);
        DelayPress::Queued
    }

    pub fn own_wish(&self) -> Option<u8> {
        self.wish.map(|w| w.x)
    }

    /// ホスト: auto の旗が立っているか。ゲストは常に false（ゲストの画面に出す旗は `HOST_STATE` で知る）。
    pub fn auto_on(&self) -> bool {
        self.auto.is_on()
    }

    /// ホスト: 推奨値を渡す。
    pub fn set_recommend(&mut self, r: Option<Recommend>) {
        self.rec = r;
    }

    pub fn recommend(&self) -> Option<Recommend> {
        self.rec
    }

    /// ホスト: 持っている値（押下・要求）を切替に結び付ける（`capture` の先頭）。`Busy` なら持ったまま、ほかは捨てる。
    fn bind_want(&mut self) {
        let Some(w) = self.want else {
            return;
        };
        match self.request_delay(w) {
            Ok(()) => {
                self.want = None;
                self.stats.want_bound = self.stats.want_bound.saturating_add(1);
            }
            Err(LatError::Busy) => self.stats.want_busy = self.stats.want_busy.saturating_add(1),
            Err(_) => self.want = None,
        }
    }

    /// ゲスト: 新しい切替を受けた・取得を 1 つ進めた後に、要求をやめるかを見る（押した後の切替で値が同じ / 回数の上限）。
    fn settle_wish(&mut self, captured: bool) {
        let Some(mut w) = self.wish else {
            return;
        };
        let seen =
            self.switch_gen != w.mark && self.pending.map_or(self.delay, |sw| sw.delay) == w.x;
        if seen {
            self.wish = None;
            return;
        }
        if captured {
            w.left = w.left.saturating_sub(1);
            if w.left == 0 {
                self.wish = None;
                self.stats.wish_expired = self.stats.wish_expired.saturating_add(1);
                return;
            }
        }
        self.wish = Some(w);
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    /// latency の切替を要求する（ホストだけ）。次に列へ入る自分の取得に結び付く。まだ結び付いていない要求があれば置き換える。
    /// 前の切替（フレーム a、旧 delay o）が次のどれかに当たる間は `Busy`:
    /// - まだ自分が a を使っていない
    /// - 相手の ack が a を越えていない（INPUT に載る切替はいつも 1 つ）
    /// - 相手のワード a+o+1 がまだ届いていない。このワードは相手が capture_frame a+1 以降に作るので、届いていれば相手も a を使い終えている。
    ///   これが無いと、ゲストは使う前の切替を 2 つ持ちうる。
    pub fn request_delay(&mut self, new: u8) -> Result<(), LatError> {
        if self.role != Role::Host {
            return Err(LatError::NotHost);
        }
        if new > MAX_DELAY {
            return Err(LatError::OutOfRange);
        }
        if self.pending.is_some() {
            return Err(LatError::Busy);
        }
        if let Some((sw, old)) = self.last_bound {
            let guest_used = self.remote_next > sw.frame + u32::from(old) + 1;
            if self.frame <= sw.frame || self.peer_ack <= sw.frame || !guest_used {
                return Err(LatError::Busy);
            }
        }
        if new == self.delay {
            self.request = None;
            return Err(LatError::Unchanged);
        }
        self.request = Some(new);
        Ok(())
    }

    /// フレーム `frame()` の自分の入力を取る（1 フレームに 1 回）。切替の境目では:
    /// - apply_frame が自分の列の次より先（delay が k 増えた）なら、隙間の k 個を直前のワードで埋めてから置く（0 で埋めると押し続けている
    ///   射撃が k フレーム離れたことになる）。
    /// - 送り済みの位置（delay が k 減った）なら、この取得を捨てる。捨てた取得に切替の要求は結び付けない（相手がもう持っているワードに
    ///   切替を後から載せても、相手はそれを見ずに a を越えうる）。
    pub fn capture(&mut self, local: u16) -> Result<(), CaptureError> {
        if self.captured {
            return Err(CaptureError::AlreadyCaptured);
        }
        if self.role == Role::Host {
            // auto の旗が立っていれば、seg の最初の取得でその時点の推奨値で見直す（マッチの途中では追わない）。
            if self.auto.is_on() && self.frame == 0 {
                self.auto_head();
            }
            self.bind_want();
        }
        let apply = apply_frame(self.frame, self.delay);
        if apply < self.local_next {
            self.stats.discarded_captures = self.stats.discarded_captures.saturating_add(1);
            self.captured = true;
            self.settle_wish(true);
            return Ok(());
        }
        // 相手がまだ持っていない自分のワードと、まだ使っていない自分のワードを上書きしない。
        let oldest_needed = self.frame.min(self.peer_ack);
        if apply - oldest_needed >= RING as u32 {
            return Err(CaptureError::Full);
        }
        let prev_word = if self.local_next > u32::from(self.seg_start_delay) {
            self.local.get(self.local_next - 1).unwrap_or(0)
        } else {
            0
        };
        for x in self.local_next..apply {
            self.local.put(x, prev_word);
        }
        self.local.put(apply, local);
        self.local_next = apply + 1;

        if let Some(new) = self.request.take() {
            let sw = LatChange {
                frame: apply,
                delay: new,
            };
            self.pending = Some(sw);
            self.carry = Some(sw);
            self.last_bound = Some((sw, self.delay));
        }
        self.captured = true;
        self.settle_wish(true);
        Ok(())
    }

    pub fn build_input(&self) -> Input {
        let base = self.peer_ack;
        let n = (self.local_next.saturating_sub(base) as usize).min(MAX_WORDS);
        let mut words = [0u16; MAX_WORDS];
        for (k, w) in words.iter_mut().enumerate().take(n) {
            *w = self.local.get(base + k as u32).unwrap_or(0);
        }
        Input {
            seg: self.seg,
            ack: self.remote_next,
            base,
            // ホスト: 相手の ack が a を越えるまで載せる。ゲスト: 要求を、やめるまで毎回載せる。
            lat: match self.role {
                Role::Host => self.carry.filter(|sw| sw.frame >= self.peer_ack),
                Role::Guest => self.wish.map(|w| LatChange {
                    frame: w.seq,
                    delay: w.x,
                }),
            },
            count: n as u8,
            words,
        }
    }

    /// 前の seg のバックアップ（`on_input` が `PrevSeg { reply: true }` を返したときに送る）。
    pub fn build_prev_input(&self) -> Option<Input> {
        self.prev
    }

    pub fn on_input(&mut self, inp: &Input) -> RecvOutcome {
        if inp.seg != self.seg {
            if self.prev.is_some_and(|p| p.seg == inp.seg) {
                // 相手の ack が自分が使い終えたフレームに届いていなければ、相手はまだ前の seg の自分のワードを待っている。バックアップの終わりで
                // 比べると、使われない先のワードを互いに待ち合い、バックアップどうしが返信し合って止まらない。
                return RecvOutcome::PrevSeg {
                    reply: inp.ack < self.prev_end,
                };
            }
            return RecvOutcome::OtherSeg;
        }
        // ゲストの lat は要求（frame = seq ≥ 1）。frame 0 は取り決めの違反として丸ごと捨てる。
        if self.role == Role::Host && inp.lat.is_some_and(|l| l.frame == 0) {
            return RecvOutcome::GuestLat;
        }
        let e = self.remote_next;
        if !in_window(inp.base, e) {
            return RecvOutcome::OutOfWindow;
        }
        if inp.ack > self.local_next {
            return RecvOutcome::AckAhead;
        }
        if let Some(sw) = inp.lat
            && sw.delay > MAX_DELAY
        {
            return RecvOutcome::BadLat;
        }

        self.peer_ack = self.peer_ack.max(inp.ack);
        if let Some(sw) = inp.lat {
            match self.role {
                Role::Host => self.take_wish(sw),
                Role::Guest => self.take_remote_switch(sw),
            }
        }

        let n = (inp.count as usize).min(MAX_WORDS);
        // 先のワードを置けるのは、まだ使っていないワードの枠を上書きしない範囲だけ（frame + RING の手前）。
        let limit = self.frame.saturating_add(RING as u32);
        let mut new_words = 0u8;
        let mut conflicts = 0u8;
        for (k, &w) in inp.words.iter().enumerate().take(n) {
            let Some(x) = inp.base.checked_add(k as u32) else {
                break;
            };
            if is_prefill(x, self.seg_start_delay) {
                // 開始直後の穴埋めは送られない。正しい相手なら来ない。
                continue;
            }
            if x >= limit {
                break;
            }
            match self.remote.get(x) {
                Some(w0) => {
                    if w0 != w {
                        conflicts = conflicts.saturating_add(1);
                    }
                }
                None => {
                    if x >= e {
                        self.remote.put(x, w);
                        new_words += 1;
                    }
                }
            }
        }
        self.stats.conflicts = self.stats.conflicts.saturating_add(u32::from(conflicts));
        for _ in 0..RING {
            if self.remote.get(self.remote_next).is_some() {
                self.remote_next += 1;
            } else {
                break;
            }
        }
        RecvOutcome::Accepted {
            new_words,
            conflicts,
        }
    }

    fn take_remote_switch(&mut self, sw: LatChange) {
        if sw.frame < self.frame {
            // もう使ったフレームの切替のコピーが遅れて届くのは正常。最後に使ったもの以降のフレームを指す知らない切替だけが食い違いの兆し。
            let stale = self
                .last_applied
                .is_some_and(|l| l == sw || sw.frame < l.frame);
            if !stale {
                self.stats.lat_late = self.stats.lat_late.saturating_add(1);
            }
            return;
        }
        match self.pending {
            None => {
                self.pending = Some(sw);
                self.switch_gen = self.switch_gen.wrapping_add(1);
                self.settle_wish(false);
            }
            Some(p) if p == sw => {}
            Some(_) => self.stats.lat_conflicts = self.stats.lat_conflicts.saturating_add(1),
        }
    }

    fn take_wish(&mut self, l: LatChange) {
        if l.frame <= self.last_wish_seq {
            self.stats.wish_dup = self.stats.wish_dup.saturating_add(1);
            return;
        }
        self.last_wish_seq = l.frame;
        self.stats.wish_taken = self.stats.wish_taken.saturating_add(1);
        if l.delay == AUTO {
            // ゲストの auto: 旗を立てて、素の推奨値をすぐ使う（変わりにくくするのは seg の始まりの見直しだけ）。
            self.auto.raise();
            self.auto_now();
        } else {
            self.auto.lower();
            self.want = Some(crate::delay_pick::clamp(crate::wire::Mode::Delay, l.delay));
        }
    }

    fn local_word(&self, f: u32) -> Option<u16> {
        if is_prefill(f, self.seg_start_delay) {
            Some(0)
        } else {
            self.local.get(f)
        }
    }

    fn remote_word(&self, f: u32) -> Option<u16> {
        if is_prefill(f, self.seg_start_delay) {
            Some(0)
        } else {
            self.remote.get(f)
        }
    }

    /// `frame()` で要る相手のワードが、もう届いているか。delay 方式の dll が、フレームの始まりで相手のワードを待つかを決めるのに使う。
    pub fn remote_ready(&self) -> bool {
        self.remote_word(self.frame).is_some()
    }

    pub fn ready(&self) -> bool {
        self.captured
            && self.local_word(self.frame).is_some()
            && self.remote_word(self.frame).is_some()
    }

    pub fn advance(&mut self) -> Option<Applied> {
        if !self.ready() {
            return None;
        }
        let f = self.frame;
        let local = self.local_word(f)?;
        let remote = self.remote_word(f)?;
        let mut switched_to = None;
        if let Some(sw) = self.pending
            && sw.frame == f
        {
            self.delay = sw.delay;
            self.pending = None;
            self.last_applied = Some(sw);
            switched_to = Some(sw.delay);
        }
        self.frame = f + 1;
        self.captured = false;
        Some(Applied {
            frame: f,
            local,
            remote,
            switched_to,
        })
    }
}

pub fn by_seat(my_seat: Seat, local: u16, remote: u16) -> (u16, u16) {
    match my_seat {
        Seat::P1 => (local, remote),
        Seat::P2 => (remote, local),
    }
}

// 2 台の模型（検査の中だけで使う）。
#[cfg(test)]
#[path = "testnet.rs"]
mod testnet;

#[cfg(test)]
mod tests {
    //! golden trace は手で書いた表と同期で回した 2 台の出力を比べる。2 台の模型（`testnet.rs`）は決め打ちのロス列と固定 seed の乱数列で、
    //! 両台の記録・ロスの無い走行・台本から組んだ参照の列が一致することを見る。
    use super::testnet::{self, Fate, Kind, Outcome, Rec, Scenario, SendInfo};
    use super::*;

    type FateBox = Box<dyn FnMut(&SendInfo) -> Fate>;
    /// 切替の表の 1 行: (ホストの local, ゲストの local, switched_to, ホストの INPUT に lat があったか)。
    type SwitchRow = (u16, u16, Option<u8>, bool);

    // ---------- 同期の 2 台（表の検査用） ----------

    struct SyncFrame {
        ia: Input,
        ib: Input,
        ra: RecvOutcome,
        rb: RecvOutcome,
        aa: Applied,
        ab: Applied,
    }

    fn sync_frame(a: &mut Lockstep, b: &mut Lockstep, wa: u16, wb: u16) -> SyncFrame {
        a.capture(wa).expect("capture a");
        b.capture(wb).expect("capture b");
        let ia = a.build_input();
        let ib = b.build_input();
        let ra = a.on_input(&ib);
        let rb = b.on_input(&ia);
        let aa = a.advance().expect("a が進めない");
        let ab = b.advance().expect("b が進めない");
        SyncFrame {
            ia,
            ib,
            ra,
            rb,
            aa,
            ab,
        }
    }

    // ---------- golden trace（latency 0 と 3） ----------
    // 台 A = ホスト・席 2P、台 B = ゲスト・席 1P。表の列: フレーム / 送った INPUT / 受信 / 適用（A の local, remote）/ リプレイ（1P, 2P）。
    const A_SCRIPT: [u16; 10] = [
        0x0001, 0x0003, 0x0002, 0x0010, 0x0011, 0x0040, 0x0041, 0x0080, 0x0081, 0x0004,
    ];
    const B_SCRIPT: [u16; 10] = [
        0x0200, 0x0201, 0x0202, 0x0204, 0x0208, 0x0210, 0x0220, 0x0240, 0x0280, 0x0300,
    ];

    struct Row {
        frame: u32,
        base: u32,
        count: u8,
        a_words: [u16; 2],
        b_words: [u16; 2],
        ack: u32,
        recv_new: u8,
        a_local: u16,
        a_remote: u16,
        p1: u16,
        p2: u16,
    }

    #[rustfmt::skip]
    const GOLDEN_L0: [Row; 10] = [
        //   f  base cnt  A のワード           B のワード          ack 受信  A local  A remote  1P      2P
        Row { frame: 0, base: 0, count: 1, a_words: [0x0001, 0], b_words: [0x0200, 0], ack: 0, recv_new: 1, a_local: 0x0001, a_remote: 0x0200, p1: 0x0200, p2: 0x0001 },
        Row { frame: 1, base: 0, count: 2, a_words: [0x0001, 0x0003], b_words: [0x0200, 0x0201], ack: 1, recv_new: 1, a_local: 0x0003, a_remote: 0x0201, p1: 0x0201, p2: 0x0003 },
        Row { frame: 2, base: 1, count: 2, a_words: [0x0003, 0x0002], b_words: [0x0201, 0x0202], ack: 2, recv_new: 1, a_local: 0x0002, a_remote: 0x0202, p1: 0x0202, p2: 0x0002 },
        Row { frame: 3, base: 2, count: 2, a_words: [0x0002, 0x0010], b_words: [0x0202, 0x0204], ack: 3, recv_new: 1, a_local: 0x0010, a_remote: 0x0204, p1: 0x0204, p2: 0x0010 },
        Row { frame: 4, base: 3, count: 2, a_words: [0x0010, 0x0011], b_words: [0x0204, 0x0208], ack: 4, recv_new: 1, a_local: 0x0011, a_remote: 0x0208, p1: 0x0208, p2: 0x0011 },
        Row { frame: 5, base: 4, count: 2, a_words: [0x0011, 0x0040], b_words: [0x0208, 0x0210], ack: 5, recv_new: 1, a_local: 0x0040, a_remote: 0x0210, p1: 0x0210, p2: 0x0040 },
        Row { frame: 6, base: 5, count: 2, a_words: [0x0040, 0x0041], b_words: [0x0210, 0x0220], ack: 6, recv_new: 1, a_local: 0x0041, a_remote: 0x0220, p1: 0x0220, p2: 0x0041 },
        Row { frame: 7, base: 6, count: 2, a_words: [0x0041, 0x0080], b_words: [0x0220, 0x0240], ack: 7, recv_new: 1, a_local: 0x0080, a_remote: 0x0240, p1: 0x0240, p2: 0x0080 },
        Row { frame: 8, base: 7, count: 2, a_words: [0x0080, 0x0081], b_words: [0x0240, 0x0280], ack: 8, recv_new: 1, a_local: 0x0081, a_remote: 0x0280, p1: 0x0280, p2: 0x0081 },
        Row { frame: 9, base: 8, count: 2, a_words: [0x0081, 0x0004], b_words: [0x0280, 0x0300], ack: 9, recv_new: 1, a_local: 0x0004, a_remote: 0x0300, p1: 0x0300, p2: 0x0004 },
    ];

    // latency 3: apply_frame 0〜2 は両者 0（送らない。ack は 3 から）。capture_frame f のワードはフレーム f+3 で使う。
    #[rustfmt::skip]
    const GOLDEN_L3: [Row; 10] = [
        Row { frame: 0, base: 3, count: 1, a_words: [0x0001, 0], b_words: [0x0200, 0], ack: 3, recv_new: 1, a_local: 0, a_remote: 0, p1: 0, p2: 0 },
        Row { frame: 1, base: 3, count: 2, a_words: [0x0001, 0x0003], b_words: [0x0200, 0x0201], ack: 4, recv_new: 1, a_local: 0, a_remote: 0, p1: 0, p2: 0 },
        Row { frame: 2, base: 4, count: 2, a_words: [0x0003, 0x0002], b_words: [0x0201, 0x0202], ack: 5, recv_new: 1, a_local: 0, a_remote: 0, p1: 0, p2: 0 },
        Row { frame: 3, base: 5, count: 2, a_words: [0x0002, 0x0010], b_words: [0x0202, 0x0204], ack: 6, recv_new: 1, a_local: 0x0001, a_remote: 0x0200, p1: 0x0200, p2: 0x0001 },
        Row { frame: 4, base: 6, count: 2, a_words: [0x0010, 0x0011], b_words: [0x0204, 0x0208], ack: 7, recv_new: 1, a_local: 0x0003, a_remote: 0x0201, p1: 0x0201, p2: 0x0003 },
        Row { frame: 5, base: 7, count: 2, a_words: [0x0011, 0x0040], b_words: [0x0208, 0x0210], ack: 8, recv_new: 1, a_local: 0x0002, a_remote: 0x0202, p1: 0x0202, p2: 0x0002 },
        Row { frame: 6, base: 8, count: 2, a_words: [0x0040, 0x0041], b_words: [0x0210, 0x0220], ack: 9, recv_new: 1, a_local: 0x0010, a_remote: 0x0204, p1: 0x0204, p2: 0x0010 },
        Row { frame: 7, base: 9, count: 2, a_words: [0x0041, 0x0080], b_words: [0x0220, 0x0240], ack: 10, recv_new: 1, a_local: 0x0011, a_remote: 0x0208, p1: 0x0208, p2: 0x0011 },
        Row { frame: 8, base: 10, count: 2, a_words: [0x0080, 0x0081], b_words: [0x0240, 0x0280], ack: 11, recv_new: 1, a_local: 0x0040, a_remote: 0x0210, p1: 0x0210, p2: 0x0040 },
        Row { frame: 9, base: 11, count: 2, a_words: [0x0081, 0x0004], b_words: [0x0280, 0x0300], ack: 12, recv_new: 1, a_local: 0x0041, a_remote: 0x0220, p1: 0x0220, p2: 0x0041 },
    ];

    fn check_golden(delay: u8, table: &[Row]) {
        let mut a = Lockstep::new(Role::Host, delay);
        let mut b = Lockstep::new(Role::Guest, delay);
        for (i, r) in table.iter().enumerate() {
            assert_eq!(a.frame(), r.frame);
            let s = sync_frame(&mut a, &mut b, A_SCRIPT[i], B_SCRIPT[i]);
            let n = r.count as usize;
            for (inp, words, who) in [(&s.ia, &r.a_words, "A"), (&s.ib, &r.b_words, "B")] {
                assert_eq!(
                    (inp.seg, inp.base, inp.count, inp.ack, inp.lat),
                    (0, r.base, r.count, r.ack, None),
                    "latency {delay} フレーム {} の {who} の INPUT",
                    r.frame
                );
                assert_eq!(&inp.words[..n], &words[..n], "{who} のワード f{}", r.frame);
            }
            let acc = RecvOutcome::Accepted {
                new_words: r.recv_new,
                conflicts: 0,
            };
            assert_eq!((s.ra, s.rb), (acc, acc), "受信 f{}", r.frame);
            assert_eq!(
                s.aa,
                Applied {
                    frame: r.frame,
                    local: r.a_local,
                    remote: r.a_remote,
                    switched_to: None
                },
                "A の適用 f{}",
                r.frame
            );
            assert_eq!(
                (s.ab.frame, s.ab.local, s.ab.remote),
                (r.frame, r.a_remote, r.a_local),
                "B の適用 f{}",
                r.frame
            );
            assert_eq!(by_seat(Seat::P2, s.aa.local, s.aa.remote), (r.p1, r.p2));
            assert_eq!(by_seat(Seat::P1, s.ab.local, s.ab.remote), (r.p1, r.p2));
        }
    }

    #[test]
    fn golden_trace_latency_0() {
        check_golden(0, &GOLDEN_L0);
    }

    #[test]
    fn golden_trace_latency_3() {
        check_golden(3, &GOLDEN_L3);
    }

    #[test]
    fn by_seat_orders_p1_p2() {
        assert_eq!(by_seat(Seat::P1, 0x11, 0x22), (0x11, 0x22));
        assert_eq!(by_seat(Seat::P2, 0x11, 0x22), (0x22, 0x11));
    }

    #[test]
    fn remote_ready_tracks_the_peer_word_not_our_capture() {
        let mut a = Lockstep::new(Role::Host, 1);
        let mut b = Lockstep::new(Role::Guest, 1);
        assert!(!a.captured());
        assert!(a.remote_ready(), "穴埋めの frame 0 は届く必要が無い");
        a.capture(0x11).unwrap();
        b.capture(0x22).unwrap();
        let (ia, ib) = (a.build_input(), b.build_input());
        a.on_input(&ib);
        b.on_input(&ia);
        a.advance().unwrap();
        b.advance().unwrap();
        assert_eq!(a.frame(), 1);
        assert!(a.remote_ready(), "B の frame 0 のワードは届いている");
        // A だけが frame 1 を取って先へ進む。frame 2 で要るワードは B が frame 1 で取るワード（apply 2）で、B が取る前は届いていない。
        a.capture(0x33).unwrap();
        a.advance().unwrap();
        assert_eq!(a.frame(), 2);
        assert!(!a.remote_ready(), "B の frame 1 のワードはまだ無い");
        b.capture(0x44).unwrap();
        a.on_input(&b.build_input());
        assert!(a.remote_ready(), "B が取って届いた後は真");
        let mut c = Lockstep::new(Role::Host, 0);
        assert!(!c.remote_ready(), "delay 0 の frame 0 は相手の取得を待つ");
        let mut d = Lockstep::new(Role::Guest, 0);
        d.capture(5).unwrap();
        c.on_input(&d.build_input());
        assert!(c.remote_ready(), "届いたら真（自分の取得は要らない）");
        assert!(!c.captured(), "問うだけで取得は進めない");
    }

    #[test]
    fn capture_once_and_no_advance_without_capture() {
        let mut a = Lockstep::new(Role::Host, 0);
        let mut b = Lockstep::new(Role::Guest, 0);
        assert_eq!(a.advance(), None, "取得前は進めない");
        a.capture(1).unwrap();
        assert_eq!(a.capture(2), Err(CaptureError::AlreadyCaptured));
        assert!(!a.ready(), "相手のワードがまだ無い");
        assert_eq!(a.advance(), None);
        b.capture(9).unwrap();
        a.on_input(&b.build_input());
        let got = a.advance().unwrap();
        assert_eq!((got.local, got.remote), (1, 9), "最初の取得が残る");
    }

    // ---------- latency の切替の境目（手で書いた表） ----------

    fn h(c: u32) -> u16 {
        0x1000 + c as u16
    }
    fn g(c: u32) -> u16 {
        0x2000 + c as u16
    }

    /// 同期で回し、ホストが `req_at` の取得の前に `request_delay(new)` する。各フレームの `SwitchRow` を返す。
    fn sync_switch_run(
        delay: u8,
        frames: u32,
        req_at: u32,
        new: u8,
    ) -> (Vec<SwitchRow>, Lockstep, Lockstep) {
        let mut a = Lockstep::new(Role::Host, delay);
        let mut b = Lockstep::new(Role::Guest, delay);
        let mut out = Vec::new();
        for f in 0..frames {
            if f == req_at {
                a.request_delay(new).unwrap();
            }
            let s = sync_frame(&mut a, &mut b, h(f), g(f));
            assert_eq!((s.ab.local, s.ab.remote), (s.aa.remote, s.aa.local));
            assert_eq!(
                s.aa.switched_to, s.ab.switched_to,
                "両者が同じフレームで切り替える"
            );
            assert!(s.ib.lat.is_none(), "ゲストは lat を載せない");
            out.push((s.aa.local, s.ab.local, s.aa.switched_to, s.ia.lat.is_some()));
        }
        (out, a, b)
    }

    #[test]
    fn switch_table_2_to_4() {
        // delay 2、フレーム 5 の取得に切替（a = 5+2 = 7、新 delay 4）。captures 0〜7 は旧 2 で
        // apply 2〜9、capture 8 は新 4 で apply 12。隙間の 10・11 は apply 9 のワード（capture 7）で埋まる。
        let (rows, _, _) = sync_switch_run(2, 16, 5, 4);
        #[rustfmt::skip]
        let want: [SwitchRow; 16] = [
            (0,      0,      None,    false), // f0  穴埋め
            (0,      0,      None,    false), // f1  穴埋め
            (0x1000, 0x2000, None,    false), // f2  = capture 0
            (0x1001, 0x2001, None,    false), // f3
            (0x1002, 0x2002, None,    false), // f4
            (0x1003, 0x2003, None,    true),  // f5  この取得（capture 5, apply 7）に切替が載る
            (0x1004, 0x2004, None,    true),  // f6  ゲストの ack が 7 を越えるまで載せる
            (0x1005, 0x2005, Some(4), false), // f7  = a。ここで切り替える
            (0x1006, 0x2006, None,    false), // f8
            (0x1007, 0x2007, None,    false), // f9  = capture 7（旧 delay の最後）
            (0x1007, 0x2007, None,    false), // f10 埋め（直前のワード）
            (0x1007, 0x2007, None,    false), // f11 埋め（直前のワード）
            (0x1008, 0x2008, None,    false), // f12 = capture 8（新 delay 4）
            (0x1009, 0x2009, None,    false), // f13
            (0x100A, 0x200A, None,    false), // f14
            (0x100B, 0x200B, None,    false), // f15
        ];
        assert_eq!(rows, want);
    }

    #[test]
    fn switch_table_4_to_1() {
        // delay 4、フレーム 3 の取得に切替（a = 7、新 delay 1）。captures 0〜7 は apply 4〜11、
        // capture 8・9・10 は apply 9・10・11 で送り済みと重なるので捨てる（k = 3）。capture 11 → 12。
        let (rows, a, b) = sync_switch_run(4, 16, 3, 1);
        #[rustfmt::skip]
        let want: [SwitchRow; 16] = [
            (0,      0,      None,    false), // f0 穴埋め
            (0,      0,      None,    false), // f1
            (0,      0,      None,    false), // f2
            (0,      0,      None,    true),  // f3 この取得（capture 3, apply 7）に切替が載る
            (0x1000, 0x2000, None,    true),  // f4 = capture 0。ゲストの ack が 7 を越えるまで載せる
            (0x1001, 0x2001, None,    false), // f5（ゲストの ack はフレーム 4 の交換で 8）
            (0x1002, 0x2002, None,    false), // f6
            (0x1003, 0x2003, Some(1), false), // f7 = a
            (0x1004, 0x2004, None,    false), // f8
            (0x1005, 0x2005, None,    false), // f9
            (0x1006, 0x2006, None,    false), // f10
            (0x1007, 0x2007, None,    false), // f11 = capture 7（旧 delay の最後）
            (0x100B, 0x200B, None,    false), // f12 = capture 11（8・9・10 は捨てた）
            (0x100C, 0x200C, None,    false), // f13
            (0x100D, 0x200D, None,    false), // f14
            (0x100E, 0x200E, None,    false), // f15
        ];
        assert_eq!(rows, want);
        assert_eq!(a.stats().discarded_captures, 3);
        assert_eq!(b.stats().discarded_captures, 3);
        for (hl, gl, _, _) in &rows {
            for c in 8..=10 {
                assert_ne!(*hl, h(c), "捨てた取得 capture {c} が使われた");
                assert_ne!(*gl, g(c), "捨てた取得 capture {c} が使われた");
            }
        }
    }

    #[test]
    fn request_delay_rules() {
        let mut b = Lockstep::new(Role::Guest, 2);
        assert_eq!(b.request_delay(4), Err(LatError::NotHost));
        let mut a = Lockstep::new(Role::Host, 2);
        assert_eq!(a.request_delay(10), Err(LatError::OutOfRange));
        assert_eq!(a.request_delay(2), Err(LatError::Unchanged));

        let mut b = Lockstep::new(Role::Guest, 2);
        for f in 0..16u32 {
            if f == 5 {
                a.request_delay(4).unwrap();
            }
            if (6..=8).contains(&f) {
                assert_eq!(a.request_delay(3), Err(LatError::Busy), "f{f}");
            }
            if f == 9 {
                assert_eq!(a.request_delay(3), Ok(()), "f{f}");
            }
            sync_frame(&mut a, &mut b, h(f), g(f));
        }
        assert_eq!(a.delay(), 3);
        assert_eq!(b.delay(), 3);
    }

    #[test]
    fn unbound_request_is_replaced_and_skips_discarded_capture() {
        let mut a = Lockstep::new(Role::Host, 4);
        let mut b = Lockstep::new(Role::Guest, 4);
        for f in 0..20u32 {
            if f == 3 {
                a.request_delay(1).unwrap();
            }
            if f == 12 {
                a.request_delay(5).unwrap();
                a.request_delay(6).unwrap(); // まだ結び付いていないので置き換わる
            }
            sync_frame(&mut a, &mut b, h(f), g(f));
            if f == 12 {
                assert_eq!(
                    a.pending_switch(),
                    Some(LatChange {
                        frame: 13,
                        delay: 6
                    })
                );
            }
        }
        assert_eq!((a.delay(), b.delay()), (6, 6));

        // 捨てる取得の最中に要求が通る場合（ゲストが先に進んでいて、Busy が先に解ける）: 次の捨てられない取得まで待つ。
        let mut a = Lockstep::new(Role::Host, 4);
        let mut b = Lockstep::new(Role::Guest, 4);
        for f in 0..10u32 {
            if f == 3 {
                a.request_delay(1).unwrap();
            }
            sync_frame(&mut a, &mut b, h(f), g(f));
        }
        b.capture(g(10)).unwrap(); // apply 11 は送り済み ⇒ 捨てる
        assert!(b.advance().is_some());
        b.capture(g(11)).unwrap(); // apply 12
        assert!(matches!(
            a.on_input(&b.build_input()),
            RecvOutcome::Accepted { new_words: 1, .. }
        ));
        assert_eq!(a.request_delay(2), Ok(()));
        a.capture(h(10)).unwrap(); // apply 11 ⇒ 捨てる。要求は結び付かない
        assert_eq!(a.pending_switch(), None);
        b.on_input(&a.build_input());
        assert!(a.advance().is_some()); // フレーム 10
        a.capture(h(11)).unwrap(); // apply 12 ⇒ ここに結び付く
        assert_eq!(
            a.pending_switch(),
            Some(LatChange {
                frame: 12,
                delay: 2
            })
        );
        b.on_input(&a.build_input());
        a.on_input(&b.build_input());
        let (x, y) = (a.advance().unwrap(), b.advance().unwrap()); // フレーム 11
        assert_eq!((x.local, x.remote), (y.remote, y.local));
        assert_eq!(
            b.pending_switch(),
            Some(LatChange {
                frame: 12,
                delay: 2
            })
        );
        for f in 12..24u32 {
            let s = sync_frame(&mut a, &mut b, h(f), g(f));
            assert_eq!((s.ab.local, s.ab.remote), (s.aa.remote, s.aa.local), "f{f}");
            assert_eq!(s.aa.switched_to, s.ab.switched_to, "f{f}");
        }
        assert_eq!((a.delay(), b.delay()), (2, 2));
        assert_eq!(a.stats().discarded_captures, 3);
    }

    #[test]
    fn consecutive_switches_stay_in_step() {
        let seq = [3u8, 7, 1, 9, 0, 5, 2, 8];
        let mut a = Lockstep::new(Role::Host, 0);
        let mut b = Lockstep::new(Role::Guest, 0);
        let mut next = 0usize;
        let mut busy_seen = 0u32;
        let mut switches = Vec::new();
        let mut rec = Vec::new();
        for f in 0..200u32 {
            if next < seq.len() {
                match a.request_delay(seq[next]) {
                    Ok(()) => next += 1,
                    Err(LatError::Busy) => busy_seen += 1,
                    Err(e) => panic!("{e:?}"),
                }
            }
            let s = sync_frame(
                &mut a,
                &mut b,
                testnet::word(0, 0, f),
                testnet::word(1, 0, f),
            );
            assert_eq!(s.aa.switched_to, s.ab.switched_to);
            if let Some(d) = s.aa.switched_to {
                switches.push((f, d));
            }
            rec.push((s.aa.local, s.aa.remote));
            assert_eq!((s.ab.local, s.ab.remote), (s.aa.remote, s.aa.local));
        }
        assert_eq!(next, seq.len(), "全部の切替が出た");
        assert_eq!(switches.len(), seq.len(), "全部の切替が使われた");
        assert!(busy_seen > 0, "Busy の期間を通った（効く期間）");
        let la = reference_list(0, 0, 0, &switches, 200);
        let lb = reference_list(1, 0, 0, &switches, 200);
        for (f, &(l, r)) in rec.iter().enumerate() {
            assert_eq!((l, r), (la[f], lb[f]), "f{f}");
        }
    }

    // ---------- 参照の列（送り手の規則を素朴に書いたもの） ----------

    /// 台 `node` の seg `seg` の apply_frame 0..len のワード。`switches` は (使ったフレーム a, 新 delay)。
    fn reference_list(
        node: usize,
        seg: u16,
        start_delay: u8,
        switches: &[(u32, u8)],
        len: u32,
    ) -> Vec<u16> {
        let mut list: Vec<u16> = vec![0; start_delay as usize];
        let mut d = u32::from(start_delay);
        let mut c = 0u32;
        while (list.len() as u32) < len {
            // capture_frame c の delay: a < c の切替は使い終えている（a+1 から新しい delay）。
            for &(a, nd) in switches {
                if a + 1 == c {
                    d = u32::from(nd);
                }
            }
            let apply = c + d;
            if (apply as usize) >= list.len() {
                let last = list.last().copied().unwrap_or(0);
                while (list.len() as u32) < apply {
                    list.push(last);
                }
                list.push(testnet::word(node, seg, c));
            }
            c += 1;
        }
        list.truncate(len as usize);
        list
    }

    /// 模型の結果を確かめる: 走り切った / 両台の記録が同じ / 参照の列と同じ / 食い違いの数が 0。
    fn assert_run(sc: &Scenario, out: &Outcome, label: &str) {
        assert!(
            out.finished,
            "{label}: 走り切れない（{} ms、記録 {} / {}）",
            out.end_ms,
            out.rec[0].len(),
            out.rec[1].len()
        );
        assert_eq!(out.rec[0], out.rec[1], "{label}: 両台の記録が違う");
        let per = sc.frames_per_seg as usize;
        assert_eq!(out.rec[0].len(), per * sc.segs as usize, "{label}");
        let mut start_delay = sc.delay;
        for s in 0..sc.segs as usize {
            let seg_rec: &[Rec] = &out.rec[0][s * per..(s + 1) * per];
            let switches: Vec<(u32, u8)> = seg_rec
                .iter()
                .filter_map(|r| r.switched_to.map(|d| (r.frame, d)))
                .collect();
            let la = reference_list(0, s as u16, start_delay, &switches, sc.frames_per_seg);
            let lb = reference_list(1, s as u16, start_delay, &switches, sc.frames_per_seg);
            for (f, r) in seg_rec.iter().enumerate() {
                assert_eq!((r.seg, r.frame), (s as u16, f as u32), "{label}");
                assert_eq!((r.p1, r.p2), (lb[f], la[f]), "{label}: seg {s} f{f}");
            }
            start_delay = seg_rec[per - 1].delay;
        }
        for (i, st) in out.stats.iter().enumerate() {
            assert_eq!(
                (st.conflicts, st.lat_late, st.lat_conflicts),
                (0, 0, 0),
                "{label}: 台 {i} の食い違い"
            );
        }
    }

    fn assert_constant_delay(sc: &Scenario, out: &Outcome, label: &str) {
        let d = u32::from(sc.delay);
        for r in &out.rec[0] {
            assert_eq!(r.delay, sc.delay, "{label}");
            let w = |node| {
                if r.frame < d {
                    0
                } else {
                    testnet::word(node, r.seg, r.frame - d)
                }
            };
            assert_eq!(
                (r.p1, r.p2),
                (w(1), w(0)),
                "{label}: seg {} f{}",
                r.seg,
                r.frame
            );
        }
    }

    fn lossless(sc: &Scenario) -> Outcome {
        testnet::run(sc, &mut |_| Fate::Deliver)
    }

    // ---------- 2 台の模型: 決め打ちのロス列 ----------

    /// 決め打ちのロス列（名前・何発いじるはずか・fate）。F = フレーム 20 の取得の直後の送り。
    fn scripted_fates() -> Vec<(&'static str, u32, FateBox)> {
        const F: u32 = 20;
        let first_at = |i: &SendInfo, from: usize, f: u32| {
            i.from == from && i.kind == Kind::Capture && i.frame == f && i.seg_idx == 0
        };
        let mut v: Vec<(&'static str, u32, FateBox)> = vec![(
            "両方向の同じフレームを 1 発ずつ落とす",
            2,
            Box::new(move |i: &SendInfo| {
                if first_at(i, 0, F) || first_at(i, 1, F) {
                    Fate::Drop
                } else {
                    Fate::Deliver
                }
            }),
        )];
        for (name, from, k) in [
            ("ホスト → ゲストを 2 発続けて落とす", 0usize, 2u32),
            ("ホスト → ゲストを 3 発続けて落とす", 0, 3),
            ("ゲスト → ホストを 4 発続けて落とす", 1, 4),
        ] {
            let mut left = 0u32;
            v.push((
                name,
                k,
                Box::new(move |i: &SendInfo| {
                    if first_at(i, from, F) {
                        left = k;
                    }
                    if i.from == from && left > 0 {
                        left -= 1;
                        Fate::Drop
                    } else {
                        Fate::Deliver
                    }
                }),
            ));
        }
        v.push((
            "順序の入れ替え（両方向）",
            2,
            Box::new(move |i: &SendInfo| {
                if first_at(i, 0, F) {
                    Fate::Delay(40)
                } else if first_at(i, 1, F + 1) {
                    Fate::Delay(25)
                } else {
                    Fate::Deliver
                }
            }),
        ));
        v.push((
            "重複（両方向）",
            2,
            Box::new(move |i: &SendInfo| {
                if first_at(i, 0, F) {
                    Fate::Dup(20)
                } else if first_at(i, 1, F) {
                    Fate::Dup(3)
                } else {
                    Fate::Deliver
                }
            }),
        ));
        v
    }

    #[test]
    fn model_scripted_losses_latency_0_and_3() {
        for delay in [0u8, 3] {
            let mut sc = Scenario::new(delay, 1, 60);
            sc.one_way_ms = 5;
            let base = lossless(&sc);
            assert_run(&sc, &base, "ロス無し");
            assert_constant_delay(&sc, &base, "ロス無し");
            for (name, touched, mut fate) in scripted_fates() {
                let out = testnet::run(&sc, &mut *fate);
                let label = format!("latency {delay}: {name}");
                let c = &out.counts;
                assert_eq!(
                    c.dropped + c.delayed + c.dupped,
                    touched,
                    "{label}: 決め打ちの列が当たっていない"
                );
                assert_run(&sc, &out, &label);
                assert_constant_delay(&sc, &out, &label);
                assert_eq!(out.rec[0], base.rec[0], "{label}: ロス無しと違う");
                println!(
                    "{label}: stall {:?} 走行 {} ms（ロス無し {} ms） {:?}",
                    out.stalls, out.end_ms, base.end_ms, out.counts
                );
            }
        }
    }

    // ---------- 2 台の模型: 固定 seed の乱数列 ----------

    #[test]
    fn model_random_losses_3000_frames() {
        let mut seen = Vec::new();
        for (seed, delay) in [
            (0x0A11_CE01u64, 3u8),
            (0x0B0B_0002, 3),
            (0x0C0D_E003, 3),
            (0x0D0D_0004, 0),
            (0x0E0E_0005, 9),
        ] {
            let mut sc = Scenario::new(delay, 1, 3000);
            sc.one_way_ms = 12;
            let base = lossless(&sc);
            let mut fate = testnet::random_fate(seed);
            let out = testnet::run(&sc, &mut fate);
            let label = format!("seed {seed:#x} latency {delay}");
            let sig = (out.counts.sent, out.counts.dropped, out.counts.delayed);
            assert!(
                !seen.contains(&sig),
                "{label}: 別の seed と同じ列（seed が効いていない）"
            );
            seen.push(sig);
            assert!(
                out.counts.dropped > 1000 && out.counts.delayed > 0 && out.counts.dupped > 0,
                "{label}: 乱数の列が効いていない {:?}",
                out.counts
            );
            assert_run(&sc, &out, &label);
            assert_constant_delay(&sc, &out, &label);
            assert_eq!(out.rec[0], base.rec[0], "{label}: ロス無しと違う");
            println!(
                "{label}: stall {:?} 走行 {} ms（ロス無し {} ms） {:?}",
                out.stalls, out.end_ms, base.end_ms, out.counts
            );
        }
    }

    #[test]
    fn model_random_losses_with_switches_and_segs() {
        let mut sc = Scenario::new(2, 3, 1000);
        sc.one_way_ms = 9;
        sc.requests = vec![
            (0, 100, 5),
            (0, 300, 1),
            (0, 500, 9),
            (0, 700, 0),
            (1, 50, 4),
            (1, 997, 8), // a = 997 + 4 = 1001。seg 1 は 999 まで ⇒ 使われない
            (2, 10, 2),
            (2, 400, 6),
        ];
        let base = lossless(&sc);
        assert_run(&sc, &base, "ロス無し");
        let mut seen = Vec::new();
        for seed in [0x5E6_0001u64, 0x5E6_0002, 0x5E6_0003] {
            let mut fate = testnet::random_fate(seed);
            let out = testnet::run(&sc, &mut fate);
            let label = format!("seed {seed:#x}");
            let sig = (out.counts.sent, out.counts.dropped, out.counts.delayed);
            assert!(
                !seen.contains(&sig),
                "{label}: 別の seed と同じ列（seed が効いていない）"
            );
            seen.push(sig);
            assert!(
                out.requests.iter().all(|r| r.3.is_ok()),
                "{label}: 切替が Busy で断られた（ロス無しと比べる前提が崩れる）: {:?}",
                out.requests
            );
            assert_run(&sc, &out, &label);
            assert_eq!(out.rec[0], base.rec[0], "{label}: ロス無しと違う");
            println!(
                "{label}: stall {:?} 走行 {} ms（ロス無し {} ms） {:?}",
                out.stalls, out.end_ms, base.end_ms, out.counts
            );
            let used: Vec<(u16, u32, u8)> = out.rec[0]
                .iter()
                .filter_map(|r| r.switched_to.map(|d| (r.seg, r.frame, d)))
                .collect();
            assert_eq!(
                used,
                vec![
                    (0, 102, 5),
                    (0, 305, 1),
                    (0, 501, 9),
                    (0, 709, 0),
                    (1, 50, 4),
                    (2, 14, 2),
                    (2, 402, 6)
                ],
                "{label}"
            );
            assert_eq!(out.rec[0][2000].delay, 4, "{label}");
        }
    }

    // ---------- 2 台の模型: latency の切替 ----------

    #[test]
    fn model_switches_with_lat_packet_loss_dup_reorder() {
        for (from, to) in [(0u8, 3u8), (2, 4), (3, 0), (4, 1), (0, 9), (9, 0)] {
            let mut sc = Scenario::new(from, 1, 90);
            sc.requests = vec![(0, 20, to)];
            let base = lossless(&sc);
            assert_run(&sc, &base, "ロス無し");
            let a = 20 + u32::from(from);
            let sw: Vec<_> = base.rec[0]
                .iter()
                .filter(|r| r.switched_to.is_some())
                .collect();
            assert_eq!(sw.len(), 1);
            assert_eq!(
                (sw[0].frame, sw[0].switched_to),
                (a, Some(to)),
                "{from}→{to}"
            );

            let mut lat_seen = 0u32;
            let fates: Vec<(&str, FateBox)> = vec![
                (
                    "lat を載せた最初の 3 発を落とす",
                    Box::new(move |i: &SendInfo| {
                        if i.has_lat {
                            lat_seen += 1;
                            if lat_seen <= 3 {
                                return Fate::Drop;
                            }
                        }
                        Fate::Deliver
                    }),
                ),
                (
                    "lat を載せたものを全部重ねる",
                    Box::new(|i: &SendInfo| {
                        if i.has_lat {
                            Fate::Dup(30)
                        } else {
                            Fate::Deliver
                        }
                    }),
                ),
                (
                    "lat を載せたものを全部 40 ms 遅らせる（後の INPUT に追い越される）",
                    Box::new(|i: &SendInfo| {
                        if i.has_lat {
                            Fate::Delay(40)
                        } else {
                            Fate::Deliver
                        }
                    }),
                ),
            ];
            for (name, mut fate) in fates {
                let out = testnet::run(&sc, &mut *fate);
                let label = format!("{from}→{to}: {name}");
                assert!(
                    out.counts.dropped + out.counts.delayed + out.counts.dupped > 0,
                    "{label}: lat を載せた INPUT が 1 つも無い"
                );
                assert_run(&sc, &out, &label);
                assert_eq!(out.rec[0], base.rec[0], "{label}: ロス無しと違う");
            }
        }
    }

    #[test]
    fn guest_behind_does_not_end_with_other_delay() {
        let mut a = Lockstep::new(Role::Host, 3);
        let mut b = Lockstep::new(Role::Guest, 3);
        for f in 0..10 {
            sync_frame(&mut a, &mut b, h(f), g(f));
        }
        a.request_delay(5).unwrap(); // capture 10 ⇒ a1 = 13
        sync_frame(&mut a, &mut b, h(10), g(10));
        a.on_input(&b.build_input()); // ゲストの ack 14（a1 を越えた）がホストに届く
        // ゲストは 11 を取ったところで遅れる。ホストだけ 11〜13 を使い、13 で切り替える。
        b.capture(g(11)).unwrap();
        for f in 11..14 {
            a.capture(h(f)).unwrap();
            assert!(a.advance().is_some(), "ホストが f{f} を使えない");
        }
        let lat1 = LatChange {
            frame: 13,
            delay: 5,
        };
        assert_eq!(
            (a.delay(), b.pending_switch()),
            (5, Some(lat1)),
            "効く期間に入った"
        );
        let second = a.request_delay(2);
        a.capture(h(14)).unwrap();
        b.on_input(&a.build_input()); // ゲストが a1 を使う前に届く
        a.on_input(&b.build_input()); // ゲストの ack が進み、ホストは切替を載せるのをやめる
        let mut ra = Vec::new();
        let mut rb = Vec::new();
        for _ in 0..200 {
            if a.frame() < 40 && !a.captured() {
                a.capture(h(a.frame())).unwrap();
            }
            if b.frame() < 40 && !b.captured() {
                b.capture(g(b.frame())).unwrap();
            }
            let (ia, ib) = (a.build_input(), b.build_input());
            a.on_input(&ib);
            b.on_input(&ia);
            if a.frame() < 40
                && let Some(x) = a.advance()
            {
                ra.push(x);
            }
            if b.frame() < 40
                && let Some(x) = b.advance()
            {
                rb.push(x);
            }
        }
        assert_eq!((a.frame(), b.frame()), (40, 40));
        for x in &ra {
            let y = rb.iter().find(|y| y.frame == x.frame).unwrap();
            assert_eq!((x.local, x.remote), (y.remote, y.local), "f{}", x.frame);
        }
        assert_eq!(
            a.delay(),
            b.delay(),
            "両者の delay が食い違った（2 つ目の要求: {second:?}）"
        );
        assert_eq!(b.stats().lat_conflicts, 0);
    }

    #[test]
    fn model_back_to_back_switch_requests() {
        // ホストが先に進んでいる（ロスと遅れで delay フレームまで先行しうる）ときに起きるので、同期の検査では出ない。
        let pairs = [
            (7u8, 0u8),
            (9, 2),
            (1, 8),
            (5, 3),
            (0, 9),
            (6, 1),
            (2, 4),
            (8, 0),
        ];
        for seed in [0xBAC_0001u64, 0xBAC_0002, 0xBAC_0003] {
            let mut sc = Scenario::new(3, 1, 700);
            sc.one_way_ms = 9;
            sc.retry_busy = true;
            for (k, &(x, y)) in pairs.iter().enumerate() {
                let f = 40 + 80 * k as u32;
                sc.requests.push((0, f, x));
                sc.requests.push((0, f + 1, y));
            }
            let mut fate = testnet::random_fate(seed);
            let out = testnet::run(&sc, &mut fate);
            let label = format!("seed {seed:#x}");
            assert!(
                out.requests.iter().any(|r| r.3 == Err(LatError::Busy)),
                "{label}: Busy の期間を通っていない"
            );
            assert_run(&sc, &out, &label);
            let used = out.rec[0]
                .iter()
                .filter(|r| r.switched_to.is_some())
                .count();
            assert_eq!(used, pairs.len() * 2, "{label}: 使われなかった切替がある");
        }
    }

    // ---------- seg ----------

    #[test]
    fn seg_switch_not_used_before_seg_change_is_dropped_by_both() {
        let mut sc = Scenario::new(3, 2, 60);
        sc.requests = vec![(0, 58, 7)]; // a = 61。seg 0 は 59 まで
        for (name, delay_lat) in [("ふつう", 0u64), ("lat が seg の後に届く", 400)] {
            let mut fate = move |i: &SendInfo| {
                if i.has_lat && delay_lat > 0 {
                    Fate::Delay(delay_lat)
                } else {
                    Fate::Deliver
                }
            };
            let out = testnet::run(&sc, &mut fate);
            assert_run(&sc, &out, name);
            assert!(
                out.rec[0]
                    .iter()
                    .all(|r| r.switched_to.is_none() && r.delay == 3),
                "{name}"
            );
            assert_eq!(out.requests.len(), 1);
            assert_eq!(out.requests[0].3, Ok(()));
        }
    }

    #[test]
    fn seg_boundary_needs_prev_reply() {
        // バックアップを返さないバージョン（「違う seg は捨てる」だけ）では止まることを隣に置く。
        for delay in [0u8, 2] {
            let per = 60u32;
            let mut sc = Scenario::new(delay, 2, per);
            // ホストは読み込みをすぐ終えて新しい seg の INPUT を先に送る（ゲストはまだ前の seg）。
            sc.load_ms = [1, 300];
            // ホストの seg 0 の最後のワードを全部落とす。バックアップの返信は 30 ms 遅らせ、その間に新しい seg の INPUT がゲストに着くようにする。
            let cut = per - 1 - u32::from(delay);
            let mk = move || {
                move |i: &SendInfo| {
                    if i.kind == Kind::PrevReply {
                        Fate::Delay(30)
                    } else if i.from == 0 && i.seg_idx == 0 && i.frame >= cut {
                        Fate::Drop
                    } else {
                        Fate::Deliver
                    }
                }
            };
            let out = testnet::run(&sc, &mut mk());
            assert_run(&sc, &out, &format!("delay {delay} バックアップを返す"));
            assert!(
                out.counts.prev_replies > 0,
                "delay {delay}: バックアップの返信の期間を通った"
            );
            assert!(
                out.counts.other_seg > 0,
                "delay {delay}: 新しい seg の INPUT が捨てられる期間を通った"
            );
            assert_constant_delay(&sc, &out, "バックアップを返す");

            sc.reply_prev = false;
            let stuck = testnet::run(&sc, &mut mk());
            assert!(
                !stuck.finished,
                "delay {delay}: バックアップを返さなくても走り切れた（期間が効いていない）"
            );
            let host_seg0 = stuck.rec[0].iter().filter(|r| r.seg == 0).count();
            assert_eq!(host_seg0, per as usize, "ホストは seg 0 を終えている");
            assert!(stuck.rec[0].len() <= per as usize + usize::from(delay));
            assert_eq!(
                stuck.rec[1].len(),
                per as usize - 1,
                "ゲストは seg 0 の最後で止まる"
            );
        }
    }

    #[test]
    fn prev_seg_input_is_not_mixed() {
        let mut a = Lockstep::new(Role::Host, 1);
        let mut b = Lockstep::new(Role::Guest, 1);
        for f in 0..5 {
            sync_frame(&mut a, &mut b, h(f), g(f));
        }
        let old = b.build_input(); // seg 0、apply 1〜
        a.begin_seg(1);
        b.begin_seg(1);
        let e_before = a.build_input().ack;
        assert_eq!(a.on_input(&old), RecvOutcome::PrevSeg { reply: false });
        assert_eq!(a.build_input().ack, e_before, "前の seg のワードが入った");
        let mut older = old;
        older.seg = 7;
        assert_eq!(a.on_input(&older), RecvOutcome::OtherSeg);
        b.capture(0x77).unwrap();
        assert_eq!(
            a.on_input(&b.build_input()),
            RecvOutcome::Accepted {
                new_words: 1,
                conflicts: 0
            }
        );
    }

    // ---------- 毒標本 ----------

    fn warmed() -> (Lockstep, Lockstep) {
        let mut a = Lockstep::new(Role::Host, 0);
        let mut b = Lockstep::new(Role::Guest, 0);
        for f in 0..70 {
            sync_frame(&mut a, &mut b, h(f), g(f));
        }
        (a, b)
    }

    fn inp(seg: u16, ack: u32, base: u32, words: &[u16]) -> Input {
        let mut w = [0u16; MAX_WORDS];
        w[..words.len()].copy_from_slice(words);
        Input {
            seg,
            ack,
            base,
            lat: None,
            count: words.len() as u8,
            words: w,
        }
    }

    #[test]
    fn poison_window_edges() {
        let (mut a, _) = warmed();
        let e = a.build_input().ack;
        assert_eq!(e, 70);
        let ack = a.build_input().base; // 相手の ack として正しい値
        assert_eq!(
            a.on_input(&inp(0, ack, e + 65, &[1])),
            RecvOutcome::OutOfWindow
        );
        assert_eq!(
            a.on_input(&inp(0, ack, e - 65, &[1])),
            RecvOutcome::OutOfWindow
        );
        assert_eq!(a.build_input().ack, e);
        // 期間のちょうど端は受ける（効く期間）。e−64 は昔のワードのコピー（同じワードなら食い違いでない）。
        assert!(matches!(
            a.on_input(&inp(0, ack, e + 64, &[1])),
            RecvOutcome::Accepted { new_words: 1, .. }
        ));
        assert_eq!(
            a.on_input(&inp(0, ack, e - 64, &[g(e - 64)])),
            RecvOutcome::Accepted {
                new_words: 0,
                conflicts: 0
            }
        );
        assert_eq!(a.build_input().ack, e);
    }

    #[test]
    fn poison_other_seg_and_guest_lat() {
        let (mut a, mut b) = warmed();
        b.capture(0x55).unwrap();
        let good = b.build_input();
        let mut other = good;
        other.seg = 1;
        assert_eq!(a.on_input(&other), RecvOutcome::OtherSeg);
        let mut with_lat = good;
        with_lat.lat = Some(LatChange { frame: 0, delay: 2 });
        assert_eq!(a.on_input(&with_lat), RecvOutcome::GuestLat);
        assert_eq!(a.build_input().ack, 70, "捨てた INPUT のワードが入った");
        assert_eq!(a.pending_switch(), None);
        assert_eq!(a.stats().wish_taken, 0);
        assert_eq!(
            a.on_input(&good),
            RecvOutcome::Accepted {
                new_words: 1,
                conflicts: 0
            }
        );
        assert_eq!(a.build_input().ack, 71);
        a.capture(0x66).unwrap();
        let mut from_host = a.build_input();
        from_host.lat = Some(LatChange {
            frame: 75,
            delay: 2,
        });
        assert!(matches!(
            b.on_input(&from_host),
            RecvOutcome::Accepted { .. }
        ));
        assert_eq!(
            b.pending_switch(),
            Some(LatChange {
                frame: 75,
                delay: 2
            })
        );
    }

    #[test]
    fn a_guest_wish_is_taken_once_per_seq_and_stops_when_the_switch_arrives() {
        let (mut a, mut b) = warmed();
        assert_eq!(b.press_delay(0), DelayPress::Unchanged, "今の値と同じ");
        assert_eq!(b.press_delay(10), DelayPress::Refused);
        assert_eq!(b.press_delay(3), DelayPress::Asked { seq: 1 });
        assert_eq!(b.shown_delay(), 3);
        b.capture(g(70)).unwrap();
        let w1 = b.build_input();
        assert_eq!(w1.lat, Some(LatChange { frame: 1, delay: 3 }));
        assert_eq!(
            a.on_input(&w1),
            RecvOutcome::Accepted {
                new_words: 1,
                conflicts: 0
            },
            "要求の INPUT のワードを使わない"
        );
        assert_eq!((a.stats().wish_taken, a.shown_delay()), (1, 3));
        assert!(matches!(a.on_input(&w1), RecvOutcome::Accepted { .. }));
        assert_eq!((a.stats().wish_taken, a.stats().wish_dup), (1, 1));
        a.capture(h(70)).unwrap();
        let sw = LatChange {
            frame: 70,
            delay: 3,
        };
        assert_eq!(a.pending_switch(), Some(sw));
        assert_eq!(a.stats().want_bound, 1);
        assert_eq!(b.press_delay(0), DelayPress::Asked { seq: 2 });
        let w2 = b.build_input();
        assert!(matches!(a.on_input(&w2), RecvOutcome::Accepted { .. }));
        assert!(matches!(a.on_input(&w1), RecvOutcome::Accepted { .. }));
        assert_eq!(
            (a.stats().wish_taken, a.stats().wish_dup, a.shown_delay()),
            (2, 2, 0),
            "古い seq で値が戻った・0 を下限 1 に詰めた"
        );
        let mut b2 = Lockstep::new(Role::Guest, 2);
        assert_eq!(b2.press_delay(4), DelayPress::Asked { seq: 1 });
        b2.capture(g(0)).unwrap();
        assert!(b2.build_input().lat.is_some());
        let mut from_host = inp(0, 0, 2, &[h(0)]);
        from_host.lat = Some(LatChange { frame: 2, delay: 4 });
        assert!(matches!(
            b2.on_input(&from_host),
            RecvOutcome::Accepted { .. }
        ));
        assert_eq!(b2.build_input().lat, None, "通った要求を載せ続ける");
        assert_eq!(b2.stats().wish_expired, 0);
    }

    #[test]
    fn auto_follows_the_recommendation_at_the_press_and_the_seg_head_and_a_number_lowers_it() {
        let (mut a, mut b) = warmed();
        assert_eq!(a.press_delay(AUTO), DelayPress::Unchanged);
        assert!(a.auto_on());
        assert_eq!(a.shown_delay(), 0);
        a.set_recommend(Some(Recommend::flat(3)));
        assert_eq!(a.press_delay(2), DelayPress::Queued);
        assert!(!a.auto_on());
        assert_eq!(a.press_delay(AUTO), DelayPress::Queued);
        assert!(a.auto_on());
        assert_eq!(a.shown_delay(), 3, "9 そのものを D にしない");
        a.capture(h(70)).unwrap();
        assert_eq!(a.pending_switch().map(|s| s.delay), Some(3));
        a.set_recommend(Some(Recommend::flat(6)));
        let used = a.build_input();
        assert!(matches!(b.on_input(&used), RecvOutcome::Accepted { .. }));
        b.capture(g(70)).unwrap();
        assert!(matches!(
            a.on_input(&b.build_input()),
            RecvOutcome::Accepted { .. }
        ));
        a.advance().unwrap();
        b.advance().unwrap();
        a.capture(h(71)).unwrap();
        assert_eq!(a.pending_switch(), None, "切替は使った後");
        assert_eq!(a.shown_delay(), 3, "試合の途中の推奨値の変化を追った");
        // ゲストの画面の目標に 9 は出ない。要求は INPUT の lat の delay の欄に AUTO で載る。
        assert_eq!(b.press_delay(AUTO), DelayPress::Asked { seq: 1 });
        assert_eq!(
            b.shown_delay(),
            3,
            "9 を画面の目標にしない（使った切替の値 3 のまま）"
        );
        b.capture(g(71)).unwrap();
        let w = b.build_input();
        assert_eq!(
            w.lat,
            Some(LatChange {
                frame: 1,
                delay: AUTO
            })
        );
        let (mut a2, mut b2) = warmed();
        a2.set_recommend(Some(Recommend::flat(6)));
        assert_eq!(b2.press_delay(AUTO), DelayPress::Asked { seq: 1 });
        b2.capture(g(70)).unwrap();
        assert!(matches!(
            a2.on_input(&b2.build_input()),
            RecvOutcome::Accepted { .. }
        ));
        assert!(a2.auto_on());
        assert_eq!(a2.shown_delay(), 6);
        assert_eq!(b2.press_delay(1), DelayPress::Asked { seq: 2 });
        assert!(matches!(
            a2.on_input(&b2.build_input()),
            RecvOutcome::Accepted { .. }
        ));
        assert!(!a2.auto_on());
        assert_eq!(a2.shown_delay(), 1);
        let (mut a3, _) = warmed();
        a3.set_recommend(Some(Recommend::flat(4)));
        a3.press_delay(AUTO);
        a3.capture(h(70)).unwrap();
        a3.begin_seg(1);
        a3.set_recommend(Some(Recommend::flat(5)));
        a3.capture(h(0)).unwrap();
        assert_eq!(a3.shown_delay(), 5, "seg の最初の取得で推奨値を取り直す");
        let (mut a4, _) = warmed();
        a4.set_recommend(Some(Recommend::flat(4)));
        a4.press_delay(AUTO);
        a4.press_delay(0);
        a4.capture(h(70)).unwrap();
        a4.begin_seg(1);
        a4.capture(h(0)).unwrap();
        assert_eq!(a4.shown_delay(), 0, "旗が降りているのに推奨値へ動いた");
    }

    /// ホスト a・ゲスト b を順に回して（seg ごとに 40 フレーム）、各 seg の最後の遅れとホストの統計を返す。`recs` は seg ごとの推奨値、
    /// `*_presses` は `(seg, x)`（その seg の最初の取得の前）。
    fn auto_run(
        d0: u8,
        recs: &[crate::delay_pick::Recommend],
        host_presses: &[(usize, u8)],
        guest_presses: &[(usize, u8)],
    ) -> (Vec<u8>, Stats) {
        let mut a = Lockstep::new(Role::Host, d0);
        let mut b = Lockstep::new(Role::Guest, d0);
        let mut out = Vec::new();
        for (k, &r) in recs.iter().enumerate() {
            if k > 0 {
                a.begin_seg(k as u16);
                b.begin_seg(k as u16);
            }
            a.set_recommend(Some(r));
            for &(_, x) in host_presses.iter().filter(|p| p.0 == k) {
                a.press_delay(x);
            }
            for &(_, x) in guest_presses.iter().filter(|p| p.0 == k) {
                b.press_delay(x);
            }
            for f in 0..40 {
                sync_frame(&mut a, &mut b, h(f), g(f));
            }
            assert_eq!(a.delay(), b.delay(), "seg {k}: 両台の遅れが違う");
            out.push(a.delay());
        }
        (out, a.stats())
    }

    fn rtt_recs(v: &[u32]) -> Vec<crate::delay_pick::Recommend> {
        v.iter()
            .map(|&ms| {
                crate::delay_pick::Recommend::from_rtt(crate::wire::Mode::Delay, Some(ms)).unwrap()
            })
            .collect()
    }

    #[test]
    fn the_delay_head_does_not_flicker_when_the_rtt_wanders_around_a_boundary() {
        let rtts = [33u32, 34, 38, 39, 33, 34, 33, 30];
        let (ds, st) = auto_run(1, &rtt_recs(&rtts), &[(0, AUTO)], &[]);
        assert_eq!(ds, vec![1, 1, 1, 2, 2, 2, 2, 1]);
        assert_eq!(st.auto_held, 4, "{st:?}");
    }

    #[test]
    fn the_delay_press_is_at_once_the_head_holds_and_a_number_lowers_the_flag() {
        // 往復 34 ms（素 2・−5 ms の側 1）で今 1: ホストの 9 で 2（すぐ）。次の seg の始まりは 33 ms（素 1）の 1 回目なので 2 のまま、2 回目で 1、34 ms は上げない。
        let (ds, st) = auto_run(1, &rtt_recs(&[34, 33, 33, 34, 33, 33]), &[(0, AUTO)], &[]);
        assert_eq!(ds, vec![2, 2, 1, 1, 1, 1]);
        assert_eq!(
            st.auto_held, 2,
            "seg 1（素 1 の低い 1 回目）と seg 3（素 2 を上げの余裕で）: {st:?}"
        );
        // ゲストの 9（要求）もすぐ素の推奨値: seg 1 で往復 34 ms・今 1 → 2。
        let (ds, _) = auto_run(1, &rtt_recs(&[34, 34]), &[], &[(1, AUTO)]);
        assert_eq!(ds, vec![1, 2], "ゲストの要求が余裕つきで 1 のまま");
        // 数字で旗が降りる: D 3 の 9（70 ms で素 3）、seg 2 の初めにホストが 3 を押すと旗が降り、33 ms が続いても 3。
        // seg 4 の初めの 9 は素の推奨値（33 ms で 1）をすぐ使う。
        let (ds, _) = auto_run(
            3,
            &rtt_recs(&[70, 33, 33, 33, 33]),
            &[(0, AUTO), (2, 3), (4, AUTO)],
            &[],
        );
        assert_eq!(ds, vec![3, 3, 3, 3, 1]);
        // ゲストの数字（要求）でもホストの旗が降りる: seg 1 でゲストが 2 を要求する。旗が降りていれば seg 2 以降は 33 ms が続いても 2、
        // 降りていなければ seg 2 の始まりで 1 に下がる。
        let (ds, _) = auto_run(3, &rtt_recs(&[70, 33, 33, 33]), &[(0, AUTO)], &[(1, 2)]);
        assert_eq!(ds, vec![3, 2, 2, 2]);
    }

    // ---------- 2 台の模型: ゲストの要求 ----------

    #[test]
    fn model_guest_wishes_switch_both_sides_with_loss_dup_reorder() {
        let mut sc = Scenario::new(3, 2, 300);
        sc.presses = vec![(1, 0, 20, 5), (0, 0, 120, 2), (1, 0, 200, 4), (1, 1, 30, 1)];
        let base = lossless(&sc);
        assert_run(&sc, &base, "ロス無し");
        let values =
            |o: &Outcome| -> Vec<u8> { o.rec[0].iter().filter_map(|r| r.switched_to).collect() };
        assert_eq!(values(&base), vec![5, 2, 4, 1]);
        let mut fates: Vec<(String, FateBox)> = vec![
            (
                "要求の便の最初の 5 発を落とす".into(),
                Box::new({
                    let mut n = 0u32;
                    move |i: &SendInfo| {
                        if i.from == 1 && i.has_lat {
                            n += 1;
                            if n <= 5 {
                                return Fate::Drop;
                            }
                        }
                        Fate::Deliver
                    }
                }),
            ),
            (
                "要求の便を全部重ねる".into(),
                Box::new(|i: &SendInfo| {
                    if i.from == 1 && i.has_lat {
                        Fate::Dup(30)
                    } else {
                        Fate::Deliver
                    }
                }),
            ),
            (
                "要求の便を 1 発おきに 40 ms 遅らせる".into(),
                Box::new({
                    let mut n = 0u32;
                    move |i: &SendInfo| {
                        if i.from == 1 && i.has_lat {
                            n += 1;
                            if n % 2 == 1 {
                                return Fate::Delay(40);
                            }
                        }
                        Fate::Deliver
                    }
                }),
            ),
        ];
        for seed in [0x5EED_0001u64, 0x5EED_0002] {
            fates.push((
                format!("乱れ {seed:#x}"),
                Box::new(testnet::random_fate(seed)),
            ));
        }
        let mut dups = 0;
        for (name, mut fate) in fates {
            let out = testnet::run(&sc, &mut *fate);
            assert_run(&sc, &out, &name);
            assert_eq!(values(&out), vec![5, 2, 4, 1], "{name}");
            assert_eq!(
                (out.stats[0].wish_taken, out.stats[1].wish_sent),
                (3, 3),
                "{name}: 要求の数と取った数"
            );
            dups += out.stats[0].wish_dup;
        }
        assert!(dups > 0, "重ねて届いた要求が 1 つも無い（検査が素通り）");
    }

    #[test]
    fn poison_count0_moves_only_ack() {
        let mut a = Lockstep::new(Role::Host, 0);
        for f in 0..3 {
            a.capture(h(f)).unwrap();
            a.on_input(&inp(0, 0, f, &[g(f)]));
            a.advance().unwrap();
        }
        a.capture(h(3)).unwrap();
        let before = a.build_input();
        assert_eq!((before.base, before.count), (0, 4));
        assert_eq!(
            a.on_input(&inp(0, 3, 3, &[])),
            RecvOutcome::Accepted {
                new_words: 0,
                conflicts: 0
            }
        );
        let after = a.build_input();
        assert_eq!((after.base, after.count, after.words[0]), (3, 1, h(3)));
        assert_eq!(after.ack, before.ack, "ワードが無いのに ack が進んだ");
        a.on_input(&inp(0, 1, 3, &[]));
        assert_eq!(a.build_input().base, 3);
        assert_eq!(a.on_input(&inp(0, 9, 3, &[])), RecvOutcome::AckAhead);
    }

    #[test]
    fn poison_conflicting_word_keeps_first() {
        let mut a = Lockstep::new(Role::Host, 0);
        a.capture(1).unwrap();
        assert_eq!(
            a.on_input(&inp(0, 0, 0, &[0x0A0A])),
            RecvOutcome::Accepted {
                new_words: 1,
                conflicts: 0
            }
        );
        assert_eq!(
            a.on_input(&inp(0, 0, 0, &[0x0B0B, 0x0C0C])),
            RecvOutcome::Accepted {
                new_words: 1,
                conflicts: 1
            }
        );
        assert_eq!(a.stats().conflicts, 1);
        let got = a.advance().unwrap();
        assert_eq!(got.remote, 0x0A0A, "最初のワードを守る");
    }
}
