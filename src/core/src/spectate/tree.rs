//! 観戦の中継の木。
//! ホストは中継できる直接の観戦者のうち太い`RELAYS`人を中継役にする。
//! 新しい観戦者は紹介（INTRO）と受け入れの依頼（ADMIT）で中継役の下へ回す。
//! 中継役は受けた記録を番号を変えずに自分のリングへ積み、子へ配信する（`SpectateHost`の中継の形）。
//! 子は親が黙るかREHOMEを受けたら続きの番号で元のホストへ戻る。
//! 満杯のときは落ちた中継役から戻る子の受け入れ枠を取り置く。
//! マッチの切れ目には直接の葉を中継役の下へ移す（入れ替え。ホストの上りを減らす）。
//! どれも受信スレッドで走る。
//! 確保は作るときだけ。1回の処理は表（数件）を見るだけ。

use super::*;
use crate::candidate::Candidate;
use crate::guard::{Scope, reach};
use crate::spectate_rate::RELAY_MIN_BPS;
use crate::wire::{SpectateAdmit, SpectateIntro};

/// ホストが中継役にする直接の観戦者の数。
pub const RELAYS: usize = 2;
/// 中継役1人の子の上限。
pub const RELAY_CHILDREN: u8 = 4;
/// 中継役のACKがこの間来なければ黙ったと見て紹介しない（子孫は外すまで数え続ける）。
pub const RELAY_QUIET_MS: u32 = 2_500;
// 生きている中継役でも生存確認（ping）の間隔と再送の上限の間はACKが来ないことがある。
const _: () = assert!(RELAY_QUIET_MS > HEARTBEAT_MS + RESEND_MAX_MS);
/// 記録の無い間の生存確認（ping）の間隔（ms）。ホストから中継役へと中継役から子への間隔。直接の葉は`HEARTBEAT_MS`。
pub const RELAY_HEARTBEAT_MS: u32 = 500;
// 子と中継役は親の沈黙を`REJOIN_MS`で見る。10%落ちる回線でも生存確認（ping）が5通続けて落ちなければ戻らない。
const _: () = assert!(REJOIN_MS >= 6 * RELAY_HEARTBEAT_MS);
/// 受け入れの依頼の寿命（ホストの返事待ちの紹介と、中継役が子のJOINを待つ分）。
pub const ADMIT_TTL_MS: u32 = 10_000;
/// 紹介した子の送信元からのJOINへ紹介を再送する間隔の下限。
/// 子の送信元は最初のJOINの検証していない送信元。
/// そのためJOINの再送を偽った送信元へ紹介を1対1で反射しないようにする。
/// 正直な子は`JOIN_RESEND_MS`ごとに再送するので半分なら毎回通る。
pub const CHILD_RESEND_GAP_MS: u32 = JOIN_RESEND_MS / 2;
/// 外した中継役の子孫のための取り置きの寿命。子は`REJOIN_MS`かREHOMEですぐ戻る。
pub const RESERVE_MS: u32 = REJOIN_MS + 2_000;
/// 中継役が子を外したACKの後にその子のJOINを待つ長さ（前払い）。
/// REHOMEは中継役 → 子 → ホストの2区間。ACKは中継役 → ホストの1区間。
/// そのため子のJOINはACKより遅れて着きうる。
/// JOINの再送（`JOIN_RESEND_MS`）を数回見込む。
pub const PREPAID_MS: u32 = 1_000;
/// 観戦者が`INTRO_WINDOW_MS`の内に受ける紹介の数の上限。
/// 紹介のたびに観戦者は中継役の候補（最大`MAX_CANDIDATES`）へ`REJOIN_MS`の間JOINを送る。
/// そのため敵対的なホストに紹介を繰り返させない。
/// 越えたら「直接だけ」で元のホストに残る（観戦は続く）。
/// 正直なホストが紹介するのは、観戦者が入った時と中継役が抜けた時とマッチの切れ目で入れ替えた時だけ。
/// 同じ観戦者への次の紹介は、中継役の下から元のホストへ戻った後（REHOMEか中継役の`REJOIN_MS`の沈黙）にしか来ない。
pub const MAX_INTROS: u64 = 8;
/// `MAX_INTROS`を数えるウィンドウ（ms）。正直な観戦で1分に8回の紹介を受けるのは中継役が数秒ごとに抜け続けるときだけ（推定）。
pub const INTRO_WINDOW_MS: u32 = 60_000;
/// 観戦者が中継の配信側へ渡す前に控える受け入れの依頼の数。
pub const ADMIT_QUEUE: usize = RELAY_CHILDREN as usize;
/// ホストの紹介（と置き場所）の表。中継役2人 × 子4人の倍。
/// 中継役が受け取った後も子が戻るか中継役を外すまで持つ。
/// 満ちたら最も古い置き場所を空ける。
const INTRO_CAP: usize = 2 * RELAYS * RELAY_CHILDREN as usize;
/// 取り置きの表（外した中継役ごと）。中継役の入れ替わりを見込んで同時の中継役の倍。
const RESERVE_CAP: usize = 2 * RELAYS;
/// 中継役へ回した子の名前（中継役のnonce・子のnonce・公開鍵）を覚える数。
/// 紹介の表（`INTRO_CAP`）から空けた子も戻ったときに受け入れ枠を返せるよう倍を持つ。
/// 満ちたら最も古いものを忘れる。
const KIDS_CAP: usize = 2 * INTRO_CAP;

/// `a`が`b`より後のepochか（u16の折り返しを越えて比べる）。
pub(super) fn epoch_after(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

/// 中継役の使い捨ての鍵と、中継役が子へ送る記録の送り手`Sender::SpectateHost`の通し番号。
/// 複製できない。中継の配信側（[`SpectateHost::relay`]）へ1回だけ渡す。
/// 同じ子が同じ中継役へ入り直すと鍵は同じになる。通し番号は続きから取るのでnonceは重ならない。
pub struct RelayKey {
    eph: Ephemeral,
    seq: Seq,
    cookie_key: [u8; 32],
}

impl RelayKey {
    /// シード（呼び出し側がOSの乱数で生成する）から作る。X25519を1回計算する。
    pub fn new(seed: &KeySeed) -> Self {
        Self {
            eph: Ephemeral::from_seed(seed),
            seq: Seq::new(),
            cookie_key: cookie_key(seed),
        }
    }

    /// JOINに載せる中継用の公開鍵。
    pub fn public(&self) -> [u8; PUBLIC_LEN] {
        self.eph.public()
    }
}

/// ホストが中継役へ回した子1人分（紹介とその子の置き場所）。
/// 中継役が依頼の番号を受け取った後も、子が戻ってくるか中継役を外して`RESERVE_MS`経つまで持つ。
/// 戻った子を名前（nonce・公開鍵）で見分けて、中継役の子孫の受け入れ枠と入れ替える。
#[derive(Debug, Clone, Copy)]
pub(super) struct Intro {
    child: Joiner,
    /// 子のJOINのepoch（紹介に載せる。子は受けたら1進めて中継役へJOINする）。
    epoch: u16,
    relay_at: usize,
    relay_nonce: u64,
    no: u16,
    start: u32,
    cands: Candidates,
    at: u32,
    admit_at: u32,
    /// 返事待ちとして人数に数える（中継役が番号を受け取ると中継役の子孫の数に移る）。
    counted: bool,
    /// 中継役が番号を受け取った（依頼の再送をやめる）。受け取るまでは数えなくなっても再送する（番号を飛ばさない）。
    admitted: bool,
    /// 子が別のepochでJOINした（紹介を再送しない。戻った子）。
    child_moved: bool,
    /// 中継役を外した後の受け入れ枠の取り置きが切れる時刻。
    gone_until: Option<u32>,
    /// 入れ替え（直接の葉を中継役へ回した）。
    /// 葉は元の枠で数える。中継役が依頼を受け取ったら元の枠を空ける。
    swap: bool,
    /// 子の送信元（`child.addr`）でないアドレスへ紹介を再送した最後の時刻（`JOIN_RESEND_MS`に1回まで）。
    other_at: Option<u32>,
    /// 子の送信元へ紹介を再送した最後の時刻（`CHILD_RESEND_GAP_MS`に1回まで）。
    child_at: Option<u32>,
}

/// 中継役がまだ受け取っていない`ADMIT_TTL_MS`の内の入れ替えの紹介かどうか（中継役の空きに数えて次の入れ替えを待たせる）。
fn swap_pending(x: &Intro, now: u32) -> bool {
    x.swap
        && !x.admitted
        && !x.child_moved
        && x.gone_until.is_none()
        && elapsed(now, x.at) < ADMIT_TTL_MS
}

/// ホストが中継役へ回した子（戻りの受け入れ枠を渡してよい相手）。
#[derive(Debug, Clone, Copy)]
pub(super) struct Kid {
    relay_nonce: u64,
    nonce: u64,
    public: [u8; PUBLIC_LEN],
    at: u32,
}

/// 外した中継役の子孫のための取り置き。
#[derive(Debug, Clone, Copy)]
pub(super) struct Reserve {
    relay_nonce: u64,
    count: u8,
    until: u32,
}

/// `until`がまだ来ていないかどうか（`now` < `until`。u32の折り返しを越えて比べる）。
pub(super) fn before(now: u32, until: u32) -> bool {
    let d = until.wrapping_sub(now);
    d != 0 && d <= u32::MAX / 2
}

/// 中継役の配信側が持つ状態。
pub(super) struct RelaySide {
    /// 受けた依頼と受けた時刻（子のJOINを待つ）。
    admits: [Option<(SpectateAdmit, u32)>; ADMIT_QUEUE],
    /// 最初の記録を積んだ。
    started: bool,
    /// 上（ホスト）から聞こえている（`tend_relay`が渡す）。
    upstream_ok: bool,
    /// 配信をやめた（観戦を終えた。作り直さない）。
    stopped: bool,
    /// `admits`の同じ添字の子の候補への穴あけ。
    punch: [PunchPlan; ADMIT_QUEUE],
}

pub(super) struct Tree {
    relay: Option<RelaySide>,
    /// ホストの紹介と置き場所（`INTRO_CAP`枠。中継役の形では空）。
    intros: Vec<Option<Intro>>,
    reserves: [Option<Reserve>; RESERVE_CAP],
    /// ホストが外した中継役の子孫へ送る穴あけ（`INTRO_CAP`枠。中継役の形では空）。
    punch: Vec<PunchPlan>,
    /// 中継役へ回した子（`KIDS_CAP`枠。中継役の形では空）。
    kids: Vec<Option<Kid>>,
}

impl Tree {
    pub(super) fn host() -> Tree {
        Tree {
            relay: None,
            intros: vec![None; INTRO_CAP],
            reserves: [None; RESERVE_CAP],
            punch: vec![PunchPlan::none(); INTRO_CAP],
            kids: vec![None; KIDS_CAP],
        }
    }

    fn relay() -> Tree {
        Tree {
            relay: Some(RelaySide {
                admits: [None; ADMIT_QUEUE],
                started: false,
                upstream_ok: true,
                stopped: false,
                punch: [PunchPlan::none(); ADMIT_QUEUE],
            }),
            intros: Vec::new(),
            reserves: [None; RESERVE_CAP],
            punch: Vec::new(),
            kids: Vec::new(),
        }
    }

    /// 中継役`relay_nonce`へ子（nonce・公開鍵）を回したことを覚える（同じペアなら時刻だけ新しくする）。
    fn note_kid(&mut self, relay_nonce: u64, nonce: u64, public: &[u8; PUBLIC_LEN], now: u32) {
        let same =
            |k: &Kid| k.relay_nonce == relay_nonce && k.nonce == nonce && k.public == *public;
        let at = self
            .kids
            .iter()
            .position(|k| k.as_ref().is_some_and(same))
            .or_else(|| self.kids.iter().position(Option::is_none))
            .or_else(|| {
                (0..self.kids.len()).max_by_key(|&i| {
                    self.kids
                        .get(i)
                        .copied()
                        .flatten()
                        .map_or(0, |k| elapsed(now, k.at))
                })
            });
        if let Some(slot) = at.and_then(|i| self.kids.get_mut(i)) {
            *slot = Some(Kid {
                relay_nonce,
                nonce,
                public: *public,
                at: now,
            });
        }
    }

    /// 中継役`relay_nonce`へ回した子かどうか。
    fn is_kid(&self, relay_nonce: u64, j: &SpectateJoin) -> bool {
        self.kids
            .iter()
            .flatten()
            .any(|k| k.relay_nonce == relay_nonce && k.nonce == j.nonce && k.public == j.public)
    }

    /// 戻りの受け入れ枠を使った子を忘れる（同じ枠をもう一度取らせない）。
    fn forget_kid(&mut self, relay_nonce: u64, nonce: u64, public: &[u8; PUBLIC_LEN]) {
        for k in self.kids.iter_mut() {
            if k.is_some_and(|k| {
                k.relay_nonce == relay_nonce && k.nonce == nonce && k.public == *public
            }) {
                *k = None;
            }
        }
    }

    /// `from`からJOINが来た: そこへの穴あけを止める（穴は開いている）。
    pub(super) fn stop_punch(&mut self, from: SocketAddr) {
        let relay = self.relay.iter_mut().flat_map(|r| r.punch.iter_mut());
        for p in relay.chain(self.punch.iter_mut()) {
            p.stop_for(from);
        }
    }

    pub(super) fn is_relay(&self) -> bool {
        self.relay.is_some()
    }
}

/// 新しい観戦者を受け入れる余地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Room {
    Free,
    /// 中継役の子が戻ってきた。受け入れ枠`Seat`と入れ替える（満杯でも受け入れる）。
    Back(Seat),
    Full,
}

/// 戻ってきた子と入れ替える受け入れ枠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seat {
    /// 置き場所の表の添字（名前で見分けた）。
    Placement(usize),
    /// 中継役の枠（添字）の前払い（中継役がもう数えていない子）。
    Prepaid(usize),
    /// 外した中継役の取り置き（添字）。
    Reserve(usize),
    /// 黙っている中継役の枠（添字）の子孫。
    Quiet(usize),
}

/// `addr`への穴あけを始める。送り終えた枠が無ければいちばん先まで送った枠を使う。
fn start_punch(plans: &mut [PunchPlan], addr: SocketAddr, now: u32) {
    let mut c = Candidates::EMPTY;
    if let Ok(x) = Candidate::guess(addr) {
        let _ = c.push(x);
    }
    let at = plans.iter().position(PunchPlan::is_done).or_else(|| {
        (0..plans.len()).max_by_key(|&i| plans.get(i).map_or(0, PunchPlan::rounds_sent))
    });
    if let Some(p) = at.and_then(|i| plans.get_mut(i)) {
        *p = PunchPlan::new(&c, now);
    }
}

/// 穴あけの予定を進める。送った通数。
fn poll_punch<O: Outlet>(plans: &mut [PunchPlan], now: u32, out: &mut O) -> u64 {
    plans.iter_mut().map(|p| p.poll(now, out) as u64).sum()
}

/// 中継役のACKに載った中継の状態を反映する。
/// 子孫が減った分は、まず戻って入り直した子の数（`owed`）から引く。
/// 残りはまだ戻っていない子の分として`RESERVE_MS`だけ前払いに持つ。
/// 子のJOINより中継役のACKが先に着いたときに二重に引かないため。
pub(super) fn settle_relay(
    w: &mut Watching,
    relay: RelayState,
    admit_ack: u16,
    now: u32,
    counts: &mut HostCounts,
) {
    if relay.descendants > RELAY_CHILDREN {
        counts.descendants_clamped += 1;
    }
    let (old, new) = (counted(&w.relay), counted(&relay));
    if new < old {
        let drop = old - new;
        let paid = drop.min(w.owed);
        w.owed -= paid;
        if drop > paid {
            w.prepaid = (prepaid(w, now) + drop - paid).min(RELAY_CHILDREN);
            w.prepaid_until = now.wrapping_add(PREPAID_MS);
        }
    } else if new > old {
        // 届く順が入れ替わった古いACKで減って見えた分は次のACKで戻る。
        w.prepaid = prepaid(w, now).saturating_sub(new - old);
    }
    w.relay = relay;
    w.admit_ack = admit_ack;
    // ホストが外した中継役が入り直すと番号は観戦者の側の続きから。
    w.admit_next = w.admit_next.max(admit_ack);
}

/// 数える子孫の数（偽りの値は`RELAY_CHILDREN`に切り詰める）。
fn counted(r: &RelayState) -> u8 {
    r.descendants.min(RELAY_CHILDREN)
}

/// 二重に数えない子孫の数。
fn descendants(w: &Watching) -> u8 {
    counted(&w.relay).saturating_sub(w.owed)
}

/// 切れていない前払い。
fn prepaid(w: &Watching, now: u32) -> u8 {
    if before(now, w.prepaid_until) {
        w.prepaid
    } else {
        0
    }
}

/// 中継役`w`の子が1人ホストへ戻って入り直した。
/// 前払いがあればそれを使い、無ければ子孫の数から1引く（`owed`）。
/// 中継役が数えている数より多くは引かない。
fn owe(w: &mut Watching, now: u32) {
    if prepaid(w, now) > 0 {
        w.prepaid -= 1;
    } else {
        w.owed = w.owed.saturating_add(1).min(counted(&w.relay));
    }
}

/// 紹介・依頼に載せる相手の候補: 相手のJOINの候補とホストに見えた送信元。
/// loopback・LANのアドレスは受け手と公開アドレス（ホストに見えた送信元のIP）が同じときだけ載せる。
/// 区分は申告の種類でなくアドレスで見る（種類はJOINの送り手が書ける）。
fn tree_cands(own: &Candidates, seen: SocketAddr, other_seen: SocketAddr) -> Candidates {
    let same_public = seen.ip() == other_seen.ip();
    let mut c = Candidates::EMPTY;
    if let Ok(x) = Candidate::guess(seen) {
        let _ = c.push(x);
    }
    for x in own.as_slice() {
        if same_public || reach(x.addr.ip()) == Some(Scope::Internet) {
            let _ = c.push(*x);
        }
    }
    c
}

/// 候補のうちアドレスの区分（`guard::reach`）が`floor`以上のもの。
fn reach_at_least(c: &Candidates, floor: Scope) -> Candidates {
    let mut out = Candidates::EMPTY;
    for x in c.as_slice() {
        if reach(x.addr.ip()).is_some_and(|r| r >= floor) {
            let _ = out.push(*x);
        }
    }
    out
}

/// 候補の区分のうちいちばん広いもの（候補が無ければloopback）。
fn widest(c: &Candidates) -> Scope {
    c.as_slice()
        .iter()
        .filter_map(|x| reach(x.addr.ip()))
        .max()
        .unwrap_or(Scope::Loopback)
}

impl SpectateHost {
    /// 中継役の配信側（リングバッファ`RING_CAP`。確保はここだけ）。
    /// sessionと秘密の乱数は観戦用のコードのもの（子のJOINのタグを検証する）。
    pub fn relay(session_id: u32, secret: Secret, key: RelayKey) -> Self {
        Self::relay_with_capacity(session_id, secret, key, RING_CAP)
    }

    /// 検査の小さいリングバッファ。
    pub fn relay_with_capacity(session_id: u32, secret: Secret, key: RelayKey, cap: usize) -> Self {
        Self::build(
            session_id,
            &secret,
            key.eph,
            key.seq,
            key.cookie_key,
            cap,
            Tree::relay(),
        )
    }

    /// 中継役の配信側かどうか。
    pub fn is_relay(&self) -> bool {
        self.tree.is_relay()
    }

    /// 送信元`addr`の観戦者の上りの太さの見積もり（B/s）。中継役を選ぶのに使う。
    pub fn note_uplink(&mut self, addr: SocketAddr, bps: u32) {
        for w in self.slots.iter_mut().flatten().filter(|w| w.addr == addr) {
            w.uplink = bps;
        }
    }

    /// 送信元`addr`の観戦者を中継役に選んだかどうか。
    pub fn is_relay_of(&self, addr: SocketAddr) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|w| w.addr == addr && w.relay_role)
    }

    /// 返事待ちの紹介の数（人数に数えているもの）。
    pub fn pending_intros(&self) -> usize {
        self.tree
            .intros
            .iter()
            .flatten()
            .filter(|i| i.counted)
            .count()
    }

    /// 戻る観戦者のために取り置いている受け入れ枠の数（外した中継役の取り置きと中継役の前払い）。
    pub fn reserved(&self, now: u32) -> usize {
        let removed: usize = self
            .tree
            .reserves
            .iter()
            .flatten()
            .filter(|r| before(now, r.until))
            .map(|r| usize::from(r.count))
            .sum();
        let prepaid: usize = self.reps().map(|(_, w)| usize::from(prepaid(w, now))).sum();
        removed + prepaid
    }

    /// 中継役の配信側の中継の状態（観戦者のACKに載せる）。ホストの形では「紹介しないで」。
    pub fn relay_state(&self) -> RelayState {
        match &self.tree.relay {
            Some(side) => RelayState {
                descendants: (self.direct_count() + side.admits.iter().flatten().count())
                    .min(usize::from(RelayState::MAX_DESCENDANTS))
                    as u8,
                no_intro: side.stopped || !side.upstream_ok,
            },
            None => RelayState::NO_INTRO,
        }
    }

    /// 中継役: 受けた記録を番号を変えずに積む。
    /// `idx`が次の番号より先なら自分が飛んでいる。そのため持っているものを捨ててそこから積み直す（続きを持たなくなった子は次の`poll`がREHOMEで戻す）。
    /// 前の番号（もう持っている番号）の記録は積まずに捨てる。
    /// 積んだらtrue。
    pub fn push_at(&mut self, idx: u32, rec: Record) -> bool {
        let Some(side) = self.tree.relay.as_mut() else {
            return false;
        };
        if side.stopped || idx == u32::MAX {
            return false;
        }
        if !side.started || idx > self.next {
            if side.started {
                self.counts.relay_jumps += 1;
            }
            side.started = true;
            self.head = idx;
            self.next = idx;
        }
        if idx < self.next {
            return false;
        }
        let cap = self.ring.len();
        if (self.next - self.head) as usize >= cap {
            self.head += 1;
        }
        if let Some(slot) = self.ring.get_mut(idx as usize % cap) {
            *slot = rec;
        }
        self.next = idx + 1;
        self.counts.pushed += 1;
        true
    }

    /// 中継役: 受け入れの依頼を控える（子は`RELAY_CHILDREN`人まで。同じ子の依頼の重複は受けたことにする）。受けたらtrue。
    pub fn admit(&mut self, a: SpectateAdmit, now: u32) -> bool {
        let children = self.direct_count();
        let Some(side) = self.tree.relay.as_mut() else {
            return false;
        };
        if side.stopped || !side.upstream_ok {
            return false;
        }
        let same =
            |x: &SpectateAdmit| x.nonce == a.nonce && x.epoch == a.epoch && x.public == a.public;
        if side.admits.iter().flatten().any(|(x, _)| same(x)) {
            return true;
        }
        if children + side.admits.iter().flatten().count() >= usize::from(RELAY_CHILDREN) {
            return false;
        }
        let at = side.admits.iter().position(Option::is_none);
        match at.and_then(|i| Some((side.admits.get_mut(i)?, side.punch.get_mut(i)?))) {
            Some((free, plan)) => {
                *free = Some((a, now));
                *plan = PunchPlan::new(&a.candidates, now);
                true
            }
            None => false,
        }
    }

    /// 中継役: 子の全員へREHOMEを送って外す。控えた依頼を捨てる（配信側は残る）。
    pub fn rehome_children<O: Outlet>(&mut self, out: &mut O) {
        for slot in self.slots.iter_mut() {
            if let Some(w) = slot.as_ref() {
                emit_sealed(
                    &w.keys,
                    Sender::SpectateHost,
                    &self.seq,
                    self.session_id,
                    w.addr,
                    &Message::SpectateRehome,
                    out,
                );
                self.counts.rehomes_sent += 1;
            }
            *slot = None;
        }
        if let Some(side) = self.tree.relay.as_mut() {
            side.admits = [None; ADMIT_QUEUE];
            side.punch = [PunchPlan::none(); ADMIT_QUEUE];
        }
    }

    /// 中継役: 配信をやめる（観戦を終えるとき。子へREHOMEを送る。作り直さない）。
    pub fn stop_relay<O: Outlet>(&mut self, out: &mut O) {
        self.rehome_children(out);
        if let Some(side) = self.tree.relay.as_mut() {
            side.stopped = true;
        }
    }

    /// 上（ホスト）が聞こえているかどうかを渡す。聞こえなくなったら子をREHOMEで戻す（空の生存確認（ping）が続いて子が沈黙に気づかないのを防ぐ）。
    fn set_upstream<O: Outlet>(&mut self, ok: bool, out: &mut O) {
        let Some(side) = self.tree.relay.as_mut() else {
            return;
        };
        let was = side.upstream_ok;
        side.upstream_ok = ok;
        if was && !ok {
            self.rehome_children(out);
        }
    }

    /// 中継役の配信側が子へ記録を渡し始めてよい番号かどうか。
    /// 積んだ範囲が対象。受けたまま積んでいない分を見込んで、積んだ終わりの先1ウィンドウまで。
    /// まだ1つも積んでいなければ受ける。
    /// ホストは中継役のackまでの番号しか頼まない。
    /// 最初に積んだ番号が子の始まりより先なら、`poll`がREHOMEで戻す。
    fn relay_has(&self, idx: u32) -> bool {
        let Some(side) = self.tree.relay.as_ref() else {
            return false;
        };
        !side.started || (self.head <= idx && idx <= self.next.saturating_add(SEND_WINDOW))
    }

    /// 中継役の配信側のJOIN: 依頼で名指しされた子（nonce・epoch・公開鍵）だけを受ける。
    /// 記録は子の続きの番号から渡す。無ければ依頼の始める番号から渡す。
    /// どちらも持っていなければREHOMEを返す（子の記録に飛びを作らない）。
    /// 同じ子の別の経路の2つ目の枠は、1つ目と同じ所から渡す。
    pub(super) fn relay_join<O: Outlet>(
        &mut self,
        from: SocketAddr,
        j: &SpectateJoin,
        known: Option<usize>,
        now: u32,
        out: &mut O,
    ) -> Result<HostEvent, DropReason> {
        let Some(side) = self.tree.relay.as_ref() else {
            return Err(DropReason::Unexpected);
        };
        if side.stopped || !side.upstream_ok {
            return Err(DropReason::NotAdmitted);
        }
        let admit = side.admits.iter().position(|a| {
            a.as_ref().is_some_and(|(a, _)| {
                a.nonce == j.nonce && a.epoch == j.tree.epoch && a.public == j.public
            })
        });
        let admitted = admit
            .and_then(|i| side.admits.get(i).copied().flatten())
            .map(|(a, _)| a);
        let same = |w: &&Watching| w.addr != from && w.nonce == j.nonce && w.public == j.public;
        // 別の経路の枠がACKを返していて聞こえている（`REJOIN_MS`の内にACKを受けた）なら、観戦者はそちらを選択済み。
        // そのため双子の枠は追加しない（ホストの`twin_is_live`と同じ向き）。
        // JOINは誰でも再送できるので、聞こえているかはACKの受信で見る。
        let twin_live = self
            .slots
            .iter()
            .flatten()
            .any(|w| same(&w) && acked_recently(w, now));
        let twin_ack = self.slots.iter().flatten().find(|w| same(w)).map(|w| w.ack);
        let start = match (admitted, twin_ack) {
            (Some(a), _) => j
                .tree
                .resume_from
                .filter(|&r| self.relay_has(r))
                .or(Some(a.start).filter(|&s| self.relay_has(s))),
            (None, Some(t)) if known.is_none() && !twin_live => Some(t),
            _ => {
                if known.is_none() && twin_ack.is_some() {
                    self.counts.twin_refused += 1;
                }
                return Err(DropReason::NotAdmitted);
            }
        };
        // この送信元の枠（あれば作り直す）を除いた同じ観戦者の枠。これが上限なら1つ増やすと越える。
        if self.slots.iter().flatten().filter(|w| same(w)).count() >= SLOTS_PER_IDENTITY {
            self.counts.twin_refused += 1;
            return Err(DropReason::NotAdmitted);
        }
        let held = known
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref);
        if held.is_some_and(|w| acked_recently(w, now)) {
            self.counts.rekey_held += 1;
            return Ok(HostEvent::Throttled);
        }
        if held.is_some_and(|w| elapsed(now, w.keyed_at) < REKEY_GAP_MS) {
            self.counts.join_throttled += 1;
            return Ok(HostEvent::Throttled);
        }
        if !self.take_agree_slot(now) {
            self.counts.join_throttled += 1;
            return Ok(HostEvent::Throttled);
        }
        let Some(keys) = self.agree(&j.public) else {
            return Err(DropReason::WeakKey);
        };
        if let Some(side) = self.tree.relay.as_mut()
            && let Some(i) = admit
            && let (Some(a), Some(plan)) = (side.admits.get_mut(i), side.punch.get_mut(i))
        {
            *a = None;
            *plan = PunchPlan::none();
        }
        let Some(start) = start else {
            emit_sealed(
                &keys,
                Sender::SpectateHost,
                &self.seq,
                self.session_id,
                from,
                &Message::SpectateRehome,
                out,
            );
            self.counts.rehomes_sent += 1;
            return Ok(HostEvent::Rehomed);
        };
        let name = Name::new(
            std::str::from_utf8(j.name.get(..usize::from(j.name_len)).unwrap_or(&[])).unwrap_or(""),
        );
        let id = Joiner {
            addr: from,
            nonce: j.nonce,
            public: j.public,
            keys,
        };
        let cookie = self.next_cookie();
        let fresh = Self::fresh(id, name, j.ff_max, j.tree, start, cookie, now);
        let slot = match known {
            Some(i) => self.slots.get_mut(i),
            None => self.slots.iter_mut().find(|w| w.is_none()),
        };
        let Some(slot) = slot else {
            return Err(DropReason::NotAdmitted);
        };
        *slot = Some(fresh);
        if admitted.is_some() {
            self.counts.joined += 1;
        }
        Ok(HostEvent::Joined)
    }

    /// 中継役の子孫と返事待ちの紹介の数（`spectator_count`の木の分）。
    pub(super) fn tree_count(&self) -> usize {
        let descendants: usize = self.reps().map(|(_, w)| usize::from(descendants(w))).sum();
        descendants + self.pending_intros()
    }

    /// 代表の枠（添字と観戦者）。
    fn reps(&self) -> impl Iterator<Item = (usize, &Watching)> + '_ {
        (0..SLOT_CAP)
            .filter(|&i| self.is_representative(i))
            .filter_map(|i| self.slots.get(i).and_then(Option::as_ref).map(|w| (i, w)))
    }

    /// JOINの観戦者を受け入れる余地。
    /// 中継役の子が戻ってきたなら、その受け入れ枠と入れ替える（満杯でも受け入れる）。
    /// 戻った子は名前（置き場所の表）で見分ける。
    /// 表から空けられて表に無くても、続きの番号があって親を替えたことのあるJOINなら、前払い・外した中継役の取り置き・黙っている中継役の子孫の順に使う。
    /// 新しい観戦者が満杯かどうかの判定には、取り置きと前払いも数える。
    pub(super) fn room(&self, j: &SpectateJoin, now: u32) -> Room {
        let placed = self
            .tree
            .intros
            .iter()
            .enumerate()
            .filter_map(|(i, x)| x.as_ref().map(|x| (i, x)))
            .filter(|(_, x)| {
                x.child_moved && x.child.nonce == j.nonce && x.child.public == j.public
            })
            .max_by_key(|(_, x)| x.at);
        if let Some((i, _)) = placed {
            return Room::Back(Seat::Placement(i));
        }
        // 表から空けられた子の受け入れ枠は、その中継役へ回したと覚えている子（`kids`）にだけ渡す（JOINの木のフィールドは送り手が書ける）。
        let t = &j.tree;
        if t.resume_from.is_some() && t.epoch != 0 && !t.has_children {
            let kid = |relay_nonce: u64| self.tree.is_kid(relay_nonce, j);
            if let Some((i, _)) = self
                .reps()
                .find(|(_, w)| prepaid(w, now) > 0 && kid(w.nonce))
            {
                return Room::Back(Seat::Prepaid(i));
            }
            if let Some(i) = self.tree.reserves.iter().position(|r| {
                r.is_some_and(|r| r.count > 0 && before(now, r.until) && kid(r.relay_nonce))
            }) {
                return Room::Back(Seat::Reserve(i));
            }
            if let Some((i, _)) = self.reps().find(|(_, w)| {
                elapsed(now, w.last_ack_at) > RELAY_QUIET_MS && descendants(w) > 0 && kid(w.nonce)
            }) {
                return Room::Back(Seat::Quiet(i));
            }
        }
        if self.spectator_count() + self.reserved(now) < MAX_SPECTATORS {
            Room::Free
        } else {
            Room::Full
        }
    }

    /// 戻った子`room`が元いた中継役のnonce（分かれば）。
    pub(super) fn came_from(&self, room: Room) -> Option<u64> {
        let Room::Back(seat) = room else {
            return None;
        };
        match seat {
            Seat::Placement(i) => self
                .tree
                .intros
                .get(i)
                .copied()
                .flatten()
                .map(|x| x.relay_nonce),
            Seat::Prepaid(i) | Seat::Quiet(i) => {
                self.slots.get(i).and_then(Option::as_ref).map(|w| w.nonce)
            }
            Seat::Reserve(i) => self
                .tree
                .reserves
                .get(i)
                .copied()
                .flatten()
                .map(|r| r.relay_nonce),
        }
    }

    /// 戻った子`room`（nonce・公開鍵）を受け入れた。入れ替えた受け入れ枠を1つ使う。
    pub(super) fn take_room(
        &mut self,
        room: Room,
        nonce: u64,
        public: &[u8; PUBLIC_LEN],
        now: u32,
    ) {
        let Room::Back(seat) = room else {
            return;
        };
        if let Some(r) = self.came_from(room) {
            self.tree.forget_kid(r, nonce, public);
        }
        self.counts.reserved_used += 1;
        match seat {
            Seat::Placement(i) => self.take_placement(i, now),
            Seat::Prepaid(i) => {
                if let Some(w) = self.slots.get_mut(i).and_then(Option::as_mut) {
                    w.prepaid = w.prepaid.saturating_sub(1);
                }
            }
            Seat::Reserve(i) => self.take_reserve(None, Some(i), now),
            Seat::Quiet(i) => {
                if let Some(w) = self.slots.get_mut(i).and_then(Option::as_mut) {
                    owe(w, now);
                }
            }
        }
    }

    /// 置き場所`i`の子が戻った。
    /// 中継役を外した後なら取り置き、中継役が依頼を受け取っていればその子孫の数を、それぞれ1つ使う。
    /// 中継役がまだ受け取っていなければ、受け取ったときに引く（`drop_admitted`）。
    /// 同じ名前の古い置き場所で中継役が受け取ったものは消す（その中継役はやがて子を外して前払いで数え直す）。
    fn take_placement(&mut self, i: usize, now: u32) {
        let Some(x) = self.tree.intros.get(i).copied().flatten() else {
            return;
        };
        for (k, y) in self.tree.intros.iter_mut().enumerate() {
            if k != i
                && y.is_some_and(|y| {
                    y.admitted && y.child.nonce == x.child.nonce && y.child.public == x.child.public
                })
            {
                *y = None;
            }
        }
        if x.gone_until.is_some() {
            if let Some(slot) = self.tree.intros.get_mut(i) {
                *slot = None;
            }
            self.take_reserve(Some(x.relay_nonce), None, now);
        } else if x.admitted {
            if let Some(slot) = self.tree.intros.get_mut(i) {
                *slot = None;
            }
            if let Some(w) = self
                .slots
                .get_mut(x.relay_at)
                .and_then(Option::as_mut)
                .filter(|w| w.nonce == x.relay_nonce)
            {
                owe(w, now);
            }
        }
    }

    /// 取り置きを1つ使う。添字`at`か中継役`nonce`の分を使い、無ければ切れていないどれかを使う。
    fn take_reserve(&mut self, nonce: Option<u64>, at: Option<usize>, now: u32) {
        let pick = at
            .filter(|&i| {
                self.tree
                    .reserves
                    .get(i)
                    .is_some_and(|r| r.is_some_and(|r| before(now, r.until)))
            })
            .or_else(|| {
                self.tree.reserves.iter().position(|r| {
                    r.is_some_and(|r| Some(r.relay_nonce) == nonce && before(now, r.until))
                })
            })
            .or_else(|| {
                self.tree
                    .reserves
                    .iter()
                    .position(|r| r.is_some_and(|r| r.count > 0 && before(now, r.until)))
            });
        if let Some(r) = pick.and_then(|i| self.tree.reserves.get_mut(i)) {
            if let Some(v) = r.as_mut() {
                v.count = v.count.saturating_sub(1);
            }
            if r.is_some_and(|v| v.count == 0) {
                *r = None;
            }
        }
    }

    /// 枠`i`を空ける（BYE・黙った）。
    /// 中継役だったなら、その子孫・前払い・返事待ちの紹介の数だけ受け入れ枠を`RESERVE_MS`取り置く。
    /// 取り置きの表が埋まっていればいちばん新しい取り置きに足す（黙って捨てない）。
    pub(super) fn release(&mut self, i: usize, now: u32) {
        let Some(slot) = self.slots.get_mut(i) else {
            return;
        };
        let Some((nonce, held)) = slot
            .as_ref()
            .map(|w| (w.nonce, descendants(w) + prepaid(w, now)))
        else {
            return;
        };
        *slot = None;
        if self.tree.is_relay() {
            return;
        }
        let until = now.wrapping_add(RESERVE_MS);
        let mut held = usize::from(held);
        let Tree { intros, punch, .. } = &mut self.tree;
        for x in intros.iter_mut().flatten() {
            if x.relay_at == i && x.relay_nonce == nonce && x.gone_until.is_none() {
                if x.counted {
                    held += 1;
                }
                if !x.child_moved {
                    start_punch(punch, x.child.addr, now);
                }
                x.counted = false;
                x.gone_until = Some(until);
            }
        }
        if held == 0 {
            return;
        }
        let held = held.min(MAX_SPECTATORS) as u8;
        let free = self
            .tree
            .reserves
            .iter()
            .position(|r| r.is_none_or(|x| !before(now, x.until)));
        match free {
            Some(k) => {
                if let Some(r) = self.tree.reserves.get_mut(k) {
                    *r = Some(Reserve {
                        relay_nonce: nonce,
                        count: held,
                        until,
                    });
                }
            }
            None => {
                let newest = self
                    .tree
                    .reserves
                    .iter_mut()
                    .flatten()
                    .max_by_key(|r| r.until.wrapping_sub(now));
                if let Some(r) = newest {
                    r.count = r.count.saturating_add(held).min(MAX_SPECTATORS as u8);
                    r.until = until;
                }
            }
        }
    }

    /// 紹介した子からJOINが再送されてきた（紹介が落ちた）場合の処理。
    /// 同じepochで`ADMIT_TTL_MS`の内なら、計算せずに紹介を再送する。
    /// 中継役が依頼を受け取った後も再送する。受け取りで紹介を忘れると、子は改めて直接受けられ、中継役は来ない子を寿命まで数えてしまう。
    /// 違うepochか寿命の後なら、子は戻ってきている。数えるのをやめ、JOINの受け入れは改めて判定する（受け入れ枠は`room`が入れ替える）。
    /// 平文のJOINは送信元を偽って再送できる。
    /// そのため紹介した子の送信元でないJOIN（子のもう1つの経路など）へは`JOIN_RESEND_MS`に1回だけ返す。
    /// 子の送信元（最初のJOINの検証していない送信元）へも`CHILD_RESEND_GAP_MS`に1回だけ返す。
    pub(super) fn intro_rejoin<O: Outlet>(
        &mut self,
        from: SocketAddr,
        j: &SpectateJoin,
        now: u32,
        out: &mut O,
    ) -> Option<HostEvent> {
        let at = self.tree.intros.iter().position(|x| {
            x.is_some_and(|x| {
                !x.child_moved && x.child.nonce == j.nonce && x.child.public == j.public
            })
        })?;
        let intro = self.tree.intros.get_mut(at).and_then(Option::as_mut)?;
        if intro.gone_until.is_some()
            || intro.epoch != j.tree.epoch
            || elapsed(now, intro.at) >= ADMIT_TTL_MS
        {
            intro.child_moved = true;
            intro.counted = false;
            return None;
        }
        let (last, gap) = if from == intro.child.addr {
            (&mut intro.child_at, CHILD_RESEND_GAP_MS)
        } else {
            (&mut intro.other_at, JOIN_RESEND_MS)
        };
        if last.is_some_and(|t| elapsed(now, t) < gap) {
            self.counts.join_throttled += 1;
            return Some(HostEvent::Throttled);
        }
        *last = Some(now);
        let intro = *intro;
        let relay = self
            .slots
            .get(intro.relay_at)
            .and_then(Option::as_ref)
            .filter(|w| w.nonce == intro.relay_nonce)?;
        let msg = Message::SpectateIntro(SpectateIntro {
            nonce: intro.child.nonce,
            epoch: intro.epoch,
            relay_public: relay.tree.relay_public?,
            relay: tree_cands(&relay.tree.candidates, relay.addr, from),
        });
        emit_sealed(
            &intro.child.keys,
            Sender::SpectateHost,
            &self.seq,
            self.session_id,
            from,
            &msg,
            out,
        );
        self.counts.intro_resends += 1;
        Some(HostEvent::Introduced)
    }

    /// 中継役に向く直接の観戦者かどうか。次が全部揃っているとき: 中継を許可している・ネットワーク種別のフラグが届く側・上りが`RELAY_MIN_BPS`以上・「紹介しないで」でない。
    pub(super) fn fit_for_relay(w: &Watching) -> bool {
        w.acked
            && w.tree.relay_public.is_some()
            && w.tree.nat.reachable() == Some(true)
            && w.uplink >= RELAY_MIN_BPS
            && !w.relay.no_intro
    }

    /// 中継役が`RELAYS`人に満たなければ向く直接の観戦者を太い順に足す。
    fn promote_relays(&mut self) {
        for _ in 0..RELAYS {
            let active = self.slots.iter().flatten().filter(|w| w.relay_role).count();
            if active >= RELAYS {
                return;
            }
            let mut best: Option<(usize, u32)> = None;
            for i in 0..SLOT_CAP {
                if !self.is_representative(i) {
                    continue;
                }
                let Some(w) = self.slots.get(i).and_then(Option::as_ref) else {
                    continue;
                };
                if !w.relay_role && Self::fit_for_relay(w) && best.is_none_or(|(_, u)| w.uplink > u)
                {
                    best = Some((i, w.uplink));
                }
            }
            match best.and_then(|(i, _)| self.slots.get_mut(i).and_then(Option::as_mut)) {
                Some(w) => w.relay_role = true,
                None => return,
            }
        }
    }

    /// 中継役`i`（nonce `nonce`）への返事待ちの紹介の数（人数に数えているものと受け取り待ちの入れ替え）。
    fn pending_for(&self, i: usize, nonce: u64, now: u32) -> u8 {
        self.tree
            .intros
            .iter()
            .flatten()
            .filter(|x| {
                (x.counted || swap_pending(x, now)) && x.relay_at == i && x.relay_nonce == nonce
            })
            .count() as u8
    }

    /// 新しい観戦者（記録を渡し始める番号`start`）を回す中継役を選ぶ。
    /// 「直接だけ」と「子がいる」の人は回さない。
    /// 中継役は次が全部揃っている人のうち上りの太い人: ACKが`RELAY_QUIET_MS`以内・子孫 ＋ 返事待ちが`RELAY_CHILDREN`未満・「紹介しないで」でない・`start`を持っている（その中継役の`base` ≤ `start` ≤ ホストが知るその中継役のack）。
    /// 戻ってきた子は、元の中継役（nonce `avoid`）へは回さない。
    /// その中継役のBYEより子のJOINが先に着くことがある。
    fn pick_relay(
        &mut self,
        tree: &JoinTree,
        start: u32,
        avoid: Option<u64>,
        now: u32,
    ) -> Option<usize> {
        if tree.direct_only || tree.has_children {
            return None;
        }
        self.promote_relays();
        let mut best: Option<(usize, u32)> = None;
        for i in 0..SLOT_CAP {
            if !self.is_representative(i) {
                continue;
            }
            let Some(w) = self.slots.get(i).and_then(Option::as_ref) else {
                continue;
            };
            let ok = w.relay_role
                && avoid != Some(w.nonce)
                && !w.relay.no_intro
                && w.tree.relay_public.is_some()
                && elapsed(now, w.last_ack_at) <= RELAY_QUIET_MS
                && counted(&w.relay) + self.pending_for(i, w.nonce, now) < RELAY_CHILDREN
                && w.base <= start
                && start <= w.ack;
            if ok && best.is_none_or(|(_, u)| w.uplink > u) {
                best = Some((i, w.uplink));
            }
        }
        best.map(|(i, _)| i)
    }

    /// 紹介の表の空き。無ければ中継役が受け取って子が移った置き場所のうち最も古いものを空ける。
    /// その子が戻ったときは新しい観戦者と同じに扱う。
    fn intro_slot(&mut self) -> Option<usize> {
        if let Some(i) = self.tree.intros.iter().position(Option::is_none) {
            return Some(i);
        }
        let (i, _) = self
            .tree
            .intros
            .iter()
            .enumerate()
            .filter_map(|(i, x)| x.as_ref().map(|x| (i, x)))
            .filter(|(_, x)| x.admitted && x.gone_until.is_none() && !x.child_moved)
            .min_by_key(|(_, x)| x.at)?;
        if let Some(x) = self.tree.intros.get_mut(i) {
            *x = None;
        }
        Some(i)
    }

    /// 中継役を選べたら子へ紹介を、中継役へ受け入れの依頼を送り、返事待ちの紹介を持つ。選べなければfalse（直接受ける）。
    pub(super) fn introduce<O: Outlet>(
        &mut self,
        child: Joiner,
        j: &SpectateJoin,
        start: u32,
        avoid: Option<u64>,
        now: u32,
        out: &mut O,
    ) -> bool {
        if self.tree.is_relay() {
            return false;
        }
        let Some(at) = self.pick_relay(&j.tree, start, avoid, now) else {
            return false;
        };
        if !self.send_intro(child, &j.tree, at, start, false, now, out) {
            return false;
        }
        self.counts.intros += 1;
        true
    }

    /// 子`child`（JOINの木のフィールド`tree`）へ中継役`at`の紹介を、中継役へ受け入れの依頼を送り、紹介の表に持つ。
    /// 入れ替え（`swap`）の紹介は人数に数えない（葉は元の枠で数えている）。
    #[allow(clippy::too_many_arguments)] // 紹介の表の1行を作る値をそのまま受ける。
    fn send_intro<O: Outlet>(
        &mut self,
        child: Joiner,
        tree: &JoinTree,
        at: usize,
        start: u32,
        swap: bool,
        now: u32,
        out: &mut O,
    ) -> bool {
        let Some(free) = self.intro_slot() else {
            return false;
        };
        let Some(r) = self.slots.get_mut(at).and_then(Option::as_mut) else {
            return false;
        };
        let (Some(relay_public), Some(no)) = (r.tree.relay_public, r.admit_next.checked_add(1))
        else {
            return false;
        };
        r.admit_next = no;
        let intro = Intro {
            child,
            epoch: tree.epoch,
            relay_at: at,
            relay_nonce: r.nonce,
            no,
            start,
            cands: tree_cands(&tree.candidates, child.addr, r.addr),
            at: now,
            admit_at: now,
            counted: !swap,
            admitted: false,
            child_moved: false,
            gone_until: None,
            swap,
            other_at: None,
            child_at: None,
        };
        let to_child = Message::SpectateIntro(SpectateIntro {
            nonce: child.nonce,
            epoch: tree.epoch,
            relay_public,
            relay: tree_cands(&r.tree.candidates, r.addr, child.addr),
        });
        let (rkeys, raddr) = (r.keys, r.addr);
        emit_sealed(
            &child.keys,
            Sender::SpectateHost,
            &self.seq,
            self.session_id,
            child.addr,
            &to_child,
            out,
        );
        emit_sealed(
            &rkeys,
            Sender::SpectateHost,
            &self.seq,
            self.session_id,
            raddr,
            &admit_msg(&intro),
            out,
        );
        if let Some(slot) = self.tree.intros.get_mut(free) {
            *slot = Some(intro);
        }
        self.tree
            .note_kid(intro.relay_nonce, child.nonce, &child.public, now);
        true
    }

    /// マッチの切れ目（最後に積んだ`Seg`・`Frame`・`End`が`Seg`）なら、その`Seg`の番号。
    fn cut_point(&self) -> Option<u32> {
        let seg = self.preamble_start.checked_sub(1)?;
        if seg < self.head || seg >= self.next {
            return None;
        }
        let rec = self.ring.get(seg as usize % self.ring.len())?;
        matches!(rec, Record::Seg { .. }).then_some(seg)
    }

    /// `nonce`・公開鍵`public`の観戦者への紹介が表にあるかどうか（戻った子の置き場所は数えない）。
    fn has_intro(&self, nonce: u64, public: &[u8; PUBLIC_LEN]) -> bool {
        self.tree
            .intros
            .iter()
            .flatten()
            .any(|x| !x.child_moved && x.child.nonce == nonce && x.child.public == *public)
    }

    /// 入れ替え: マッチの切れ目で直接の観戦者が`RELAYS`人を越えていれば、紹介の条件を満たす中継役へ直接の葉を1人回す。
    /// 中継役が受け取るか`ADMIT_TTL_MS`経つまで、次の葉は回さない。
    /// 回す葉は枠の番号の小さい方から選ぶ。
    /// 次の人を除き、ackがその`Seg`の番号以上で黙っていない葉に限る: 中継役・中継役に向く予備・子がいる人・測りの番を持つ人・紹介が表にある人。
    /// 「直接だけ」とJOINの「子がいる」の人は`pick_relay`が断る。
    fn swap_leaves<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let Some(seg) = self.cut_point() else {
            return;
        };
        if self.direct_count() <= RELAYS
            || self
                .tree
                .intros
                .iter()
                .flatten()
                .any(|x| swap_pending(x, now))
        {
            return;
        }
        let measuring = self.measuring();
        for i in 0..SLOT_CAP {
            if !self.is_representative(i) {
                continue;
            }
            let Some(w) = self.slots.get(i).and_then(Option::as_ref) else {
                continue;
            };
            let leaf = w.acked
                && !w.relay_role
                && !Self::fit_for_relay(w)
                && w.relay.descendants == 0
                && measuring != Some(w.addr)
                && w.ack >= seg
                && elapsed(now, w.last_ack_at) <= RELAY_QUIET_MS;
            if !leaf || self.has_intro(w.nonce, &w.public) {
                continue;
            }
            let child = Joiner {
                addr: w.addr,
                nonce: w.nonce,
                public: w.public,
                keys: w.keys,
            };
            let (tree, start, avoid) = (w.tree, w.ack, w.avoid_relay);
            let Some(at) = self.pick_relay(&tree, start, avoid, now) else {
                continue;
            };
            if self.send_intro(child, &tree, at, start, true, now, out) {
                self.counts.swaps += 1;
            }
            return;
        }
    }

    /// 中継役`i`（nonce `nonce`）が番号`upto`までの依頼を受け取った。
    /// その紹介は中継役の子孫の数に移った。
    /// 子がもう戻っていれば、中継役の数える子孫から1引いて表から消す。
    /// 入れ替えの葉なら元の直接の枠を空ける（人数は中継役の子孫へ移った）。
    pub(super) fn drop_admitted(&mut self, i: usize, nonce: u64, upto: u16, now: u32) {
        let Some(w) = self
            .slots
            .get_mut(i)
            .and_then(Option::as_mut)
            .filter(|w| w.nonce == nonce)
        else {
            return;
        };
        let mut moved = [None; INTRO_CAP];
        for (x, m) in self.tree.intros.iter_mut().zip(moved.iter_mut()) {
            if let Some(intro) = x.as_mut()
                && intro.relay_at == i
                && intro.relay_nonce == nonce
                && intro.gone_until.is_none()
                && !intro.admitted
                && intro.no <= upto
            {
                intro.admitted = true;
                intro.counted = false;
                if intro.child_moved {
                    owe(w, now);
                    *x = None;
                } else if intro.swap {
                    *m = Some((intro.child.nonce, intro.child.public));
                }
            }
        }
        for (nonce, public) in moved.into_iter().flatten() {
            for k in 0..SLOT_CAP {
                if self
                    .slots
                    .get(k)
                    .and_then(Option::as_ref)
                    .is_some_and(|v| v.nonce == nonce && v.public == public)
                {
                    self.release(k, now);
                    self.counts.swap_freed += 1;
                }
            }
        }
    }

    /// 直接の枠が残っている観戦者が親を替えて戻った（epochの進んだJOIN）。
    /// その観戦者の紹介を、戻った子として付ける（`intro_rejoin`と同じ）。
    /// 中継役がもう依頼を受け取っていれば、その受け入れ枠を使う（`take_placement`）。
    pub(super) fn back_while_direct(&mut self, nonce: u64, public: &[u8; PUBLIC_LEN], now: u32) {
        for k in 0..INTRO_CAP {
            let Some(x) = self.tree.intros.get_mut(k).and_then(Option::as_mut) else {
                continue;
            };
            if x.child_moved || x.child.nonce != nonce || x.child.public != *public {
                continue;
            }
            x.child_moved = true;
            x.counted = false;
            if x.admitted {
                self.take_placement(k, now);
            }
        }
    }

    /// ACKが`RELAY_QUIET_MS`来ない中継役の子孫へ穴あけを始める（中継役が落ちると子はホストのNATの穴が閉じた後にJOINを送る）。
    /// 次のACKが来るまで1回だけ。
    fn punch_quiet_relays(&mut self, now: u32) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let Some(w) = slot.as_mut() else {
                continue;
            };
            if !w.relay_role || w.quiet_punched || elapsed(now, w.last_ack_at) <= RELAY_QUIET_MS {
                continue;
            }
            w.quiet_punched = true;
            let Tree { intros, punch, .. } = &mut self.tree;
            for x in intros.iter().flatten() {
                if x.relay_at == i
                    && x.relay_nonce == w.nonce
                    && x.gone_until.is_none()
                    && !x.child_moved
                {
                    start_punch(punch, x.child.addr, now);
                }
            }
        }
    }

    /// `poll`の頭の処理。中継役の形なら子を外す処理と依頼の寿命切れの処理。ホストの形なら取り置きの寿命切れの処理と依頼の再送。配信側をやめていればfalse。
    pub(super) fn tree_poll<O: Outlet>(&mut self, now: u32, out: &mut O) -> bool {
        if let Some(side) = self.tree.relay.as_mut() {
            if side.stopped {
                return false;
            }
            for (a, plan) in side.admits.iter_mut().zip(side.punch.iter_mut()) {
                if a.is_some_and(|(_, t)| elapsed(now, t) >= ADMIT_TTL_MS) {
                    *a = None;
                    *plan = PunchPlan::none();
                }
            }
            self.counts.punches_sent += poll_punch(&mut side.punch, now, out);
            // 自分が飛んだかリングから落ちて続きを持たなくなった子は元のホストへ戻す。
            for slot in self.slots.iter_mut() {
                if let Some(w) = slot.as_ref()
                    && w.ack < self.head
                {
                    emit_sealed(
                        &w.keys,
                        Sender::SpectateHost,
                        &self.seq,
                        self.session_id,
                        w.addr,
                        &Message::SpectateRehome,
                        out,
                    );
                    self.counts.rehomes_sent += 1;
                    *slot = None;
                }
            }
            return true;
        }
        for r in self.tree.reserves.iter_mut() {
            if r.is_some_and(|x| !before(now, x.until)) {
                *r = None;
            }
        }
        self.punch_quiet_relays(now);
        self.counts.punches_sent += poll_punch(&mut self.tree.punch, now, out);
        let mut admitted = [None; INTRO_CAP];
        for (k, x) in self.tree.intros.iter_mut().enumerate() {
            let Some(intro) = x.as_mut() else {
                continue;
            };
            if let Some(g) = intro.gone_until {
                if !before(now, g) {
                    *x = None;
                }
                continue;
            }
            let Some(relay) = self
                .slots
                .get(intro.relay_at)
                .and_then(Option::as_ref)
                .filter(|w| w.nonce == intro.relay_nonce)
            else {
                *x = None;
                continue;
            };
            if !intro.admitted && relay.admit_ack >= intro.no {
                // ACKの後に届いた、中継役が依頼を受け取ったという情報（`drop_admitted`が見ていない）。
                if let Some(a) = admitted.get_mut(k) {
                    *a = Some((intro.relay_at, intro.relay_nonce, intro.no));
                }
                continue;
            }
            if intro.counted && elapsed(now, intro.at) >= ADMIT_TTL_MS {
                intro.counted = false;
                self.counts.intros_expired += 1;
            }
            if !intro.admitted && elapsed(now, intro.admit_at) >= relay.rtt.rto() {
                emit_sealed(
                    &relay.keys,
                    Sender::SpectateHost,
                    &self.seq,
                    self.session_id,
                    relay.addr,
                    &admit_msg(intro),
                    out,
                );
                intro.admit_at = now;
                self.counts.admit_resends += 1;
            }
        }
        for (i, nonce, no) in admitted.into_iter().flatten() {
            self.drop_admitted(i, nonce, no, now);
        }
        self.swap_leaves(now, out);
        true
    }
}

/// 紹介`intro`の受け入れの依頼。子のepochは子が紹介を受けて1進めた後の値（中継役へのJOINの値）。
fn admit_msg(intro: &Intro) -> Message {
    Message::SpectateAdmit(SpectateAdmit {
        no: intro.no,
        start: intro.start,
        nonce: intro.child.nonce,
        epoch: intro.epoch.wrapping_add(1),
        public: intro.child.public,
        candidates: intro.cands,
    })
}

// ============================ 観戦者 ============================

/// 紹介された中継役（観戦者のいまの親）。
#[derive(Debug, Clone, Copy)]
pub(super) struct RelayParent {
    pub(super) cands: Candidates,
    pub(super) keys: DirKeys,
    /// 最初のSTREAMを送ってきた送信元。
    pub(super) addr: Option<SocketAddr>,
    /// 最後にSTREAMを受けた時刻。
    pub(super) heard: Option<u32>,
    /// 紹介を受けた時刻。
    pub(super) since: u32,
}

/// 観戦者の木のカウンタ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeCounts {
    /// 元のホストから受けた紹介（親を中継役に替えた）。
    pub intros_taken: u64,
    /// 受けなかった紹介（古いepoch・親が中継役・子がいる・鍵を作れない）。
    pub intros_ignored: u64,
    pub admits_taken: u64,
    /// 元のホストから受けなかった依頼（番号が前 ＋ 1でない・中継しない・控えが満杯）。
    pub admits_ignored: u64,
    /// 元のホストの鍵で開けなかった紹介・依頼（偽の紹介など）。
    pub control_dropped: u64,
    /// 中継役から受けたREHOME。
    pub rehomes: u64,
    /// 紹介から`REJOIN_MS`の内に中継役からSTREAMが来ずに「直接だけ」で戻った。
    pub unreached: u64,
    /// 中継役の流れが`REJOIN_MS`途切れて戻った。
    pub parent_silent: u64,
    /// 始めた測りの番（元のホストのMEASURE。同じ番号の再送は数えない）。
    pub measures_taken: u64,
    /// 元のホストから受けなかった測りの番（中継しない・親が中継役・子がいる）。
    pub measures_ignored: u64,
    /// 元のホストへ送った測りのペアの数（1ペア2通）。
    pub pairs_sent: u64,
    /// 照合のずれで中継役の下から「直接だけ」で戻った（[`Watcher::return_on_mismatch`]）。
    pub mismatch_returns: u64,
}

/// 観戦者の中継の木の状態。
pub(super) struct WatchTree {
    /// いまの親が中継役ならSome（Noneは元のホスト）。
    pub(super) parent: Option<RelayParent>,
    /// いまの親に替えた時刻（一度も替えていなければNone）。
    parent_at: Option<u32>,
    pub(super) epoch: u16,
    relay_public: Option<[u8; PUBLIC_LEN]>,
    /// 中継の配信側の状態（dllが`set_relay_state`で渡す）。
    relay_state: RelayState,
    own: Candidates,
    direct_only: bool,
    /// 受けた紹介の時刻（直近`MAX_INTROS`回。`counts.intros_taken`を`MAX_INTROS`で割った余りの位置が最も古い）。
    intro_at: [u32; MAX_INTROS as usize],
    admits: [Option<SpectateAdmit>; ADMIT_QUEUE],
    /// 最後に受けた依頼の番号。
    pub(super) admit_no: u16,
    pub(super) joined_once: bool,
    /// 一度接続した後に元のホストからBYE(取消)を受けた。
    pub(super) refused: bool,
    pub(super) counts: TreeCounts,
}

impl WatchTree {
    pub(super) fn new() -> WatchTree {
        WatchTree {
            parent: None,
            parent_at: None,
            epoch: 0,
            relay_public: None,
            relay_state: RelayState::NO_INTRO,
            own: Candidates::EMPTY,
            direct_only: false,
            intro_at: [0; MAX_INTROS as usize],
            admits: [None; ADMIT_QUEUE],
            admit_no: 0,
            joined_once: false,
            refused: false,
            counts: TreeCounts::default(),
        }
    }

    fn queued(&self) -> usize {
        self.admits.iter().flatten().count()
    }

    /// 直近`INTRO_WINDOW_MS`の内に`MAX_INTROS`回の紹介を受けているかどうか。
    fn intros_crowded(&self, now: u32) -> bool {
        let taken = self.counts.intros_taken;
        taken >= MAX_INTROS
            && self
                .intro_at
                .get((taken % MAX_INTROS) as usize)
                .is_some_and(|&at| elapsed(now, at) < INTRO_WINDOW_MS)
    }

    fn note_intro(&mut self, now: u32) {
        if let Some(at) = self
            .intro_at
            .get_mut((self.counts.intros_taken % MAX_INTROS) as usize)
        {
            *at = now;
        }
        self.counts.intros_taken += 1;
    }
}

impl Watcher {
    /// 中継できるなら中継用の公開鍵（[`RelayKey::public`]。JOINに載せる）。Noneは中継しない。
    pub fn set_relay_public(&mut self, public: Option<[u8; PUBLIC_LEN]>) {
        self.tree.relay_public = public;
    }

    /// 中継の配信側の状態（[`SpectateHost::relay_state`]）。ACKに載せる。子がいる間は紹介を受けない。
    pub fn set_relay_state(&mut self, state: RelayState) {
        self.tree.relay_state = state;
    }

    /// JOINに載せる自分の候補（ホストが中継役の穴あけの宛先として渡す）。
    pub fn set_candidates(&mut self, own: Candidates) {
        self.tree.own = own;
    }

    /// 受けた受け入れの依頼を1つ渡す（中継の配信側の[`SpectateHost::admit`]へ）。
    pub fn take_admit(&mut self) -> Option<SpectateAdmit> {
        self.tree.admits.iter_mut().find_map(Option::take)
    }

    /// 親を替えた回数。
    pub fn epoch(&self) -> u16 {
        self.tree.epoch
    }

    /// いまの親が元のホストかどうか。
    pub fn parent_is_home(&self) -> bool {
        self.tree.parent.is_none()
    }

    /// 上りの太さを測ってよいかどうか。次が全部揃っているとき: 中継できる・親が元のホスト・子（控えた依頼を含む）がいない。
    pub(super) fn measurable(&self) -> bool {
        let t = &self.tree;
        t.relay_public.is_some()
            && t.parent.is_none()
            && t.relay_state.descendants == 0
            && t.queued() == 0
    }

    /// 紹介された中継役へ届かず、元のホストへ「直接だけ」で戻ったかどうか。
    pub fn direct_only(&self) -> bool {
        self.tree.direct_only
    }

    /// 一度接続した後に元のホストに断られた（満杯）かどうか。観戦を終える条件。
    pub fn refused_after_join(&self) -> bool {
        self.tree.refused
    }

    /// いまの親から最後にSTREAM（元のホストならBYEも）を聞いてからのms（何も聞いていなければNone）。
    pub fn parent_silent_ms(&self, now: u32) -> Option<u32> {
        self.parent_link().1.map(|t| elapsed(now, t))
    }

    pub fn tree_counts(&self) -> TreeCounts {
        self.tree.counts
    }

    /// ACKに載せる中継の状態（控えた依頼も子孫に数える）。中継しない観戦者は「紹介しないで」。
    pub(super) fn ack_relay_state(&self) -> RelayState {
        if self.tree.relay_public.is_none() {
            return RelayState::NO_INTRO;
        }
        let s = self.tree.relay_state;
        RelayState {
            descendants: (usize::from(s.descendants) + self.tree.queued())
                .min(usize::from(RelayState::MAX_DESCENDANTS)) as u8,
            no_intro: s.no_intro,
        }
    }

    /// JOINの木の欄。続きの番号は、木に加わった観戦者（中継できる・親を替えた）がSTREAMを1つ受けた後だけ載せる。
    pub(super) fn join_tree(&self) -> JoinTree {
        let t = &self.tree;
        let resume = t.joined_once && (t.epoch != 0 || t.relay_public.is_some());
        // 中継役へはその中継役の区分より狭い自分の候補（インターネット越しの中継役へLANのアドレス）を知らせない。
        let candidates = match &t.parent {
            Some(p) => reach_at_least(&t.own, widest(&p.cands)),
            None => t.own,
        };
        JoinTree {
            relay_public: t.relay_public,
            direct_only: t.parent.is_none() && t.direct_only,
            has_children: t.relay_state.descendants > 0 || t.queued() > 0,
            resume_from: resume.then(|| self.asm.ack()),
            nat: self.nat,
            epoch: t.epoch,
            candidates,
        }
    }

    /// JOIN（再送と通知で同じ中身）。
    pub(super) fn join_message(&self) -> Message {
        let mut name = [0u8; NAME_MAX];
        let bytes = self.name.as_bytes();
        for (d, s) in name.iter_mut().zip(bytes.iter()) {
            *d = *s;
        }
        Message::SpectateJoin(SpectateJoin {
            name_len: bytes.len().min(NAME_MAX) as u8,
            ff_max: self.ff_max,
            nonce: self.nonce,
            name,
            build: self.build,
            public: self.public,
            tree: self.join_tree(),
        })
    }

    /// 接続した後に、変わった木のフィールド（候補・ネットワーク種別のフラグ）を元のホストへJOINで通知する（ホストは同じ観戦者のJOINを読み直す）。
    /// 親が元のホストでそのアドレスが決まっているときだけ送り、送ったらtrue。
    pub fn announce<O: Outlet>(&mut self, out: &mut O) -> bool {
        let Some(a) = self.host_addr.filter(|_| self.tree.parent.is_none()) else {
            return false;
        };
        let join = self.join_message();
        emit_signed(&self.key, self.session_id, a, &join, out);
        self.counts.joins_sent += 1;
        true
    }

    /// いまの親に替えた時刻（ms。一度も替えていなければNone ＝ 最初から元のホスト）。
    /// 時計は`poll`・`on_datagram`に渡す`now`と同じ。
    /// 検査と、親を替えた時刻を読むための関数（dllはずれた記録の出どころを、時刻でなく受け渡しのリングの位置で見る）。
    pub fn parent_since(&self) -> Option<u32> {
        self.tree.parent_at
    }

    /// 照合がずれた。親が中継役ならその中継役へBYEを送り、「直接だけ」で元のホストへ戻る（以後は紹介も入れ替えもされない）。
    /// 戻ったらtrue。親が元のホストなら何もしない。
    pub fn return_on_mismatch<O: Outlet>(&mut self, now: u32, out: &mut O) -> bool {
        if self.tree.parent.is_none() {
            return false;
        }
        self.bye(ByeReason::Cancelled, out);
        self.tree.counts.mismatch_returns += 1;
        self.go_home(true, now);
        true
    }

    /// 元のホストへ戻る（epochを1進めてすぐJOINする）。
    pub(super) fn go_home(&mut self, direct_only: bool, now: u32) {
        self.tree.parent = None;
        self.tree.parent_at = Some(now);
        self.tree.epoch = self.tree.epoch.wrapping_add(1);
        self.tree.direct_only |= direct_only;
        self.home_heard = None;
        self.last_join = None;
        self.cookie = None;
    }

    /// 紹介をもう受けない（上限を越えた・使える候補が無い）。「直接だけ」にしてepochを進め、すぐ元のホストへJOINする。
    /// ホストはepochの違うJOINで返事待ちの紹介を捨てる。
    /// 「直接だけ」の観戦者は中継役へ回さないので、元のホストから流れを受け続ける。
    fn decline_intros(&mut self) {
        self.tree.epoch = self.tree.epoch.wrapping_add(1);
        self.tree.direct_only = true;
        self.home_heard = None;
        self.last_join = None;
    }

    /// 元のホストの区分。観戦用のコードの候補と、紹介・依頼の送信元`from`のうちいちばん広いもの。
    /// インターネット越しのホストなら中継役と子のloopback・LANの候補へは送らない（ホストが選んだ宛先へ自分の送信元で送らされないため）。
    fn home_floor(&self, from: SocketAddr) -> Scope {
        widest(&self.host).max(reach(from.ip()).unwrap_or(Scope::Loopback))
    }

    /// 親が中継役のときに元のホストへ戻る条件を見る。
    /// 紹介から`REJOIN_MS`の内にSTREAMが来なければ「直接だけ」で戻る。
    /// 流れが`REJOIN_MS`途切れたら「直接だけ」にせずに戻る。
    pub(super) fn check_parent(&mut self, now: u32) {
        let Some(p) = &self.tree.parent else {
            return;
        };
        match p.heard {
            None if elapsed(now, p.since) >= REJOIN_MS => {
                self.tree.counts.unreached += 1;
                self.go_home(true, now);
            }
            Some(t) if elapsed(now, t) >= REJOIN_MS => {
                self.tree.counts.parent_silent += 1;
                self.go_home(false, now);
            }
            _ => {}
        }
    }

    /// 元のホストからの紹介・依頼・測りの番。元のホストの鍵でだけ開き、送信元は問わない。
    /// ホストのアドレスが決まる前でもホストのNATが宛先ごとにポートを変える型でも受ける。
    /// 他人の中継役は鍵を持たないので子の行き先を偽れない。
    pub(super) fn on_home_control(&mut self, from: SocketAddr, bytes: &[u8], now: u32) {
        let Some(keys) = self.keys else {
            self.tree.counts.control_dropped += 1;
            return;
        };
        if wire::peek_session(bytes).is_some_and(|s| s != self.session_id) {
            self.tree.counts.control_dropped += 1;
            return;
        }
        let Ok(pkt) = wire::decode(&self.key, Some(&keys.opener()), bytes) else {
            self.tree.counts.control_dropped += 1;
            return;
        };
        let floor = self.home_floor(from);
        match pkt.msg {
            Message::SpectateIntro(i) => self.on_intro(&i, floor, now),
            Message::SpectateAdmit(a) => self.on_admit(a, floor),
            Message::SpectateMeasure { turn } => self.on_measure(turn, now),
            _ => self.tree.counts.control_dropped += 1,
        }
    }

    /// 紹介: 次が全部揃ったときだけ受ける。JOINのnonceとepochが今の値と合う・親が元のホスト・子がいない・「直接だけ」でない。
    /// 中継役の候補は元のホストの区分`floor`以上のものだけを使う。
    /// 直近`INTRO_WINDOW_MS`の内に`MAX_INTROS`回の紹介を受けているか使える候補が無ければ、紹介をもう受けない（`decline_intros`）。
    /// 中継役の公開鍵で鍵を作り直す。組み立てと通し番号はそのまま持ち越す。
    fn on_intro(&mut self, i: &SpectateIntro, floor: Scope, now: u32) {
        let t = &self.tree;
        let fits = i.nonce == self.nonce
            && i.epoch == t.epoch
            && t.parent.is_none()
            && !t.direct_only
            && t.relay_state.descendants == 0
            && t.queued() == 0;
        let cands = reach_at_least(&i.relay, floor);
        if fits && (t.intros_crowded(now) || cands.as_slice().is_empty()) {
            self.tree.counts.intros_ignored += 1;
            self.decline_intros();
            return;
        }
        let keys = if fits {
            self.eph
                .dir_keys(&i.relay_public, false, self.session_id, &self.key)
        } else {
            None
        };
        let Some(keys) = keys else {
            self.tree.counts.intros_ignored += 1;
            return;
        };
        self.tree.epoch = self.tree.epoch.wrapping_add(1);
        self.tree.parent_at = Some(now);
        self.tree.parent = Some(RelayParent {
            cands,
            keys,
            addr: None,
            heard: None,
            since: now,
        });
        self.last_heard = Some(now);
        self.last_join = None;
        self.cookie = None;
        self.tree.note_intro(now);
    }

    /// 受け入れの依頼: 次が全部揃ったときだけ控える。中継できる・親が元のホスト・配信側に空きがある（子 ＋ 控えが`RELAY_CHILDREN`未満）・番号が前 ＋ 1。
    /// 空きが無ければ番号を受け取らない（ホストは返事待ちのまま数え、寿命で数えなくなる）。
    /// 子の候補（穴あけの宛先）は元のホストの区分`floor`以上のものだけを控える。
    fn on_admit(&mut self, mut a: SpectateAdmit, floor: Scope) {
        a.candidates = reach_at_least(&a.candidates, floor);
        let t = &mut self.tree;
        let fits = t.relay_public.is_some()
            && t.parent.is_none()
            && !t.relay_state.no_intro
            && usize::from(t.relay_state.descendants) + t.queued() < usize::from(RELAY_CHILDREN)
            && a.no != 0
            && a.no == t.admit_no.wrapping_add(1);
        match t.admits.iter_mut().find(|x| x.is_none()) {
            Some(free) if fits => {
                *free = Some(a);
                t.admit_no = a.no;
                t.counts.admits_taken += 1;
            }
            _ => t.counts.admits_ignored += 1,
        }
    }
}

/// 中継役の受信スレッドの1周分の処理（観戦者`w`の組み立てから記録を`push_at`で積んだ後に呼ぶ）。
/// 受けた依頼を配信側へ渡す。
/// 上が`REJOIN_MS`黙っている間（または親が元のホストでない間）は子をREHOMEで戻して依頼を受けない。
/// 配信側の状態をACKに載せる。
pub fn tend_relay<O: Outlet>(w: &mut Watcher, relay: &mut SpectateHost, now: u32, out: &mut O) {
    let quiet = !w.parent_is_home() || w.parent_silent_ms(now).is_some_and(|ms| ms >= REJOIN_MS);
    relay.set_upstream(!quiet, out);
    while let Some(a) = w.take_admit() {
        relay.admit(a, now);
    }
    w.set_relay_state(relay.relay_state());
}

/// 観戦を終える（どの理由でも）。親へのBYEを先に、子へのREHOMEを後に送る（ホストが中継役を先に外し、戻る子の受け入れ枠を取り置くため）。
pub fn leave_tree<O: Outlet>(
    w: &mut Watcher,
    relay: Option<&mut SpectateHost>,
    reason: ByeReason,
    out: &mut O,
) {
    w.bye(reason, out);
    if let Some(r) = relay {
        r.stop_relay(out);
    }
}
