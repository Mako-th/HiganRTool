//! 観戦の中継の木。ホストは中継できる直接の観戦者のうち太い `RELAYS` 人を中継役にし、新しい観戦者を紹介（INTRO）と受け入れの依頼
//! （ADMIT）で中継役の下へ回す。中継役は受けた記録を番号を変えずに自分のリングへ積んで子へ配り（`SpectateHost` の中継の形）、子は
//! 親が黙るか REHOME を受けたら元のホストへ続きの番号で戻る。満杯のときは、落ちた中継役から戻る子の席を取り置く。マッチの切れ目には、
//! 直接の葉を中継役の下へ移す（入れ替え。ホストの上りを減らす）。
//! どれも受信スレッドで走る。確保は作るときだけで、1 回の処理は表（数件）を見るだけ。

use super::*;
use crate::candidate::{CandKind, Candidate};
use crate::spectate_rate::RELAY_MIN_BPS;
use crate::wire::{SpectateAdmit, SpectateIntro};

/// ホストが中継役にする直接の観戦者の数。
pub const RELAYS: usize = 2;
/// 中継役 1 人の子の上限。
pub const RELAY_CHILDREN: u8 = 4;
/// 中継役の ACK がこの間来なければ、黙ったと見て紹介しない（子孫は外すまで数え続ける）。
pub const RELAY_QUIET_MS: u32 = 2_500;
// 生きている中継役でも、合図の間隔と送り直しの上限の間は ACK が来ないことがある。
const _: () = assert!(RELAY_QUIET_MS > HEARTBEAT_MS + RESEND_MAX_MS);
/// 記録の無い間の生きている合図の間隔（ms）。ホストから中継役へと、中継役から子へ。直接の葉は `HEARTBEAT_MS`。
pub const RELAY_HEARTBEAT_MS: u32 = 500;
// 子と中継役は親の沈黙を `REJOIN_MS` で見る。10% 落ちる回線でも、合図が 5 通続けて落ちなければ戻らない。
const _: () = assert!(REJOIN_MS >= 6 * RELAY_HEARTBEAT_MS);
/// 受け入れの依頼の寿命（ホストの返事待ちの紹介と、中継役が子の JOIN を待つ分）。
pub const ADMIT_TTL_MS: u32 = 10_000;
/// 外した中継役の子孫のための取り置きの寿命。子は `REJOIN_MS` か REHOME ですぐ戻る。
pub const RESERVE_MS: u32 = REJOIN_MS + 2_000;
/// 中継役が子を外した ACK のあと、その子の JOIN を待つ長さ（前払い）。REHOME は中継役 → 子 → ホストの 2 区間、ACK は中継役 →
/// ホストの 1 区間なので、子の JOIN は ACK より遅れて着きうる。JOIN の送り直し（`JOIN_RESEND_MS`）を数回見込む。
pub const PREPAID_MS: u32 = 1_000;
/// 観戦者が中継の配る側へ渡す前に控える受け入れの依頼の数。
pub const ADMIT_QUEUE: usize = RELAY_CHILDREN as usize;
/// ホストの紹介（と置き場所）の表。中継役 2 人 × 子 4 人の倍（中継役が受け取った後も、子が戻るか中継役を外すまで持つ。満ちたら
/// 最も古い置き場所を空ける）。
const INTRO_CAP: usize = 2 * RELAYS * RELAY_CHILDREN as usize;
/// 取り置きの表（外した中継役ごと）。中継役の入れ替わりを見込んで、同時の中継役の倍。
const RESERVE_CAP: usize = 2 * RELAYS;

/// `a` が `b` より後の epoch か（u16 の折り返しを越えて比べる）。
pub(super) fn epoch_after(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

/// 中継役の使い捨ての鍵と、中継役が子へ送る記録の送り手 `Sender::SpectateHost` の通し番号。複製できず、中継の配る側（[`SpectateHost::relay`]）へ 1 回だけ渡す。
/// 同じ子が同じ中継役へ入り直すと鍵は同じになるが、通し番号は続きから取るので nonce は重ならない。
pub struct RelayKey {
    eph: Ephemeral,
    seq: Seq,
}

impl RelayKey {
    /// 種（呼び手が OS の乱数で引く）から作る。X25519 を 1 回計算する。
    pub fn new(seed: &KeySeed) -> Self {
        Self {
            eph: Ephemeral::from_seed(seed),
            seq: Seq::new(),
        }
    }

    /// JOIN に載せる中継用の公開鍵。
    pub fn public(&self) -> [u8; PUBLIC_LEN] {
        self.eph.public()
    }
}

/// ホストが中継役へ回した子 1 人（紹介と、その子の置き場所）。中継役が依頼の番号を受け取った後も、子が戻ってくるか中継役を外して
/// `RESERVE_MS` 経つまで持つ（戻った子を名前（nonce・公開鍵）で見分け、中継役の子孫の席と入れ替える）。
#[derive(Debug, Clone, Copy)]
pub(super) struct Intro {
    child: Joiner,
    /// 子の JOIN の epoch（紹介に載せる。子は受けたら 1 進めて中継役へ JOIN する）。
    epoch: u16,
    relay_at: usize,
    relay_nonce: u64,
    no: u16,
    start: u32,
    cands: Candidates,
    at: u32,
    admit_at: u32,
    /// 返事待ちとして人数に数える（中継役が番号を受け取ると、中継役の子孫の数に移る）。
    counted: bool,
    /// 中継役が番号を受け取った（依頼の送り直しをやめる）。受け取るまでは、数えなくなっても送り直す（番号を飛ばさない）。
    admitted: bool,
    /// 子が別の epoch で JOIN した（紹介を送り直さない。戻った子）。
    child_moved: bool,
    /// 中継役を外した後の、席の取り置きが切れる時刻。
    gone_until: Option<u32>,
    /// 入れ替え（直接の葉を中継役へ回した）。葉は元の枠で数え、中継役が依頼を受け取ったら元の枠を空ける。
    swap: bool,
}

/// 中継役がまだ受け取っていない、`ADMIT_TTL_MS` の内の入れ替えの紹介（中継役の空きに数え、次の入れ替えを待たせる）。
fn swap_pending(x: &Intro, now: u32) -> bool {
    x.swap
        && !x.admitted
        && !x.child_moved
        && x.gone_until.is_none()
        && elapsed(now, x.at) < ADMIT_TTL_MS
}

/// 外した中継役の子孫のための取り置き。
#[derive(Debug, Clone, Copy)]
pub(super) struct Reserve {
    relay_nonce: u64,
    count: u8,
    until: u32,
}

/// `until` がまだ来ていない（`now` < `until`。u32 の折り返しを越えて比べる）。
pub(super) fn before(now: u32, until: u32) -> bool {
    let d = until.wrapping_sub(now);
    d != 0 && d <= u32::MAX / 2
}

/// 中継役の配る側の持ち物。
pub(super) struct RelaySide {
    /// 受けた依頼と受けた時刻（子の JOIN を待つ）。
    admits: [Option<(SpectateAdmit, u32)>; ADMIT_QUEUE],
    /// 最初の記録を積んだ。
    started: bool,
    /// 上（ホスト）から聞こえている（`tend_relay` が渡す）。
    upstream_ok: bool,
    /// 配るのをやめた（観戦を終えた。作り直さない）。
    stopped: bool,
    /// `admits` の同じ添字の子の候補への穴あけ。
    punch: [PunchPlan; ADMIT_QUEUE],
}

pub(super) struct Tree {
    relay: Option<RelaySide>,
    /// ホストの紹介と置き場所（`INTRO_CAP` 枠。中継役の形では空）。
    intros: Vec<Option<Intro>>,
    reserves: [Option<Reserve>; RESERVE_CAP],
    /// ホストが外した中継役の子孫へ送る穴あけ（`INTRO_CAP` 枠。中継役の形では空）。
    punch: Vec<PunchPlan>,
}

impl Tree {
    pub(super) fn host() -> Tree {
        Tree {
            relay: None,
            intros: vec![None; INTRO_CAP],
            reserves: [None; RESERVE_CAP],
            punch: vec![PunchPlan::none(); INTRO_CAP],
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
        }
    }

    /// `from` から JOIN が来た: そこへの穴あけを止める（穴は開いている）。
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

/// 新しい観戦者を入れる余地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Room {
    Free,
    /// 中継役の子が戻ってきた。席 `Seat` と入れ替える（満杯でも入れる）。
    Back(Seat),
    Full,
}

/// 戻ってきた子と入れ替える席。
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

/// `addr` への穴あけを始める。送り終えた枠が無ければ、いちばん先まで送った枠を使う。
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

/// 中継役の ACK の中継の状態を持つ。子孫の減りは、まず戻って入り直した数（`owed`）を減らし、残りはまだ戻っていない子の分として
/// `RESERVE_MS` だけ前払いに持つ（子の JOIN より中継役の ACK が先に着いたとき、二重に引かないため）。
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
        // 順の入れ替わった古い ACK で減って見えた分は、次の ACK で戻る。
        w.prepaid = prepaid(w, now).saturating_sub(new - old);
    }
    w.relay = relay;
    w.admit_ack = admit_ack;
    // ホストが外した中継役が入り直すと、番号は観戦者の側の続きから。
    w.admit_next = w.admit_next.max(admit_ack);
}

/// 数える子孫（嘘は `RELAY_CHILDREN` に詰める）。
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

/// 中継役 `w` の子が 1 人ホストへ戻って入り直した: 前払いがあればそれを使い、無ければ子孫の数から 1 引く（`owed`。中継役が数えて
/// いる数より多くは引かない）。
fn owe(w: &mut Watching, now: u32) {
    if prepaid(w, now) > 0 {
        w.prepaid -= 1;
    } else {
        w.owed = w.owed.saturating_add(1).min(counted(&w.relay));
    }
}

/// 紹介・依頼に載せる相手の候補: 相手の JOIN の候補と、ホストに見えた送り元。LAN の候補は、受け手と公開アドレス（ホストに見えた
/// 送り元の IP）が同じときだけ載せる。
fn tree_cands(own: &Candidates, seen: SocketAddr, other_seen: SocketAddr) -> Candidates {
    let same_public = seen.ip() == other_seen.ip();
    let mut c = Candidates::EMPTY;
    if let Ok(x) = Candidate::guess(seen) {
        let _ = c.push(x);
    }
    for x in own.as_slice() {
        if x.kind != CandKind::Host || same_public {
            let _ = c.push(*x);
        }
    }
    c
}

impl SpectateHost {
    /// 中継役の配る側（リングバッファ `RING_CAP`。確保はここだけ）。session と秘密は観戦用のコードのもの（子の JOIN のタグを検証する）。
    pub fn relay(session_id: u32, secret: Secret, key: RelayKey) -> Self {
        Self::relay_with_capacity(session_id, secret, key, RING_CAP)
    }

    /// 検査の小さいリングバッファ。
    pub fn relay_with_capacity(session_id: u32, secret: Secret, key: RelayKey, cap: usize) -> Self {
        Self::build(session_id, &secret, key.eph, key.seq, cap, Tree::relay())
    }

    /// 中継役の配る側か。
    pub fn is_relay(&self) -> bool {
        self.tree.is_relay()
    }

    /// 送り元 `addr` の観戦者の上りの太さの見積もり（B/s）。中継役を選ぶのに使う。
    pub fn note_uplink(&mut self, addr: SocketAddr, bps: u32) {
        for w in self.slots.iter_mut().flatten().filter(|w| w.addr == addr) {
            w.uplink = bps;
        }
    }

    /// 送り元 `addr` の観戦者を中継役に選んだか。
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

    /// 戻る観戦者のために取り置いている席の数（外した中継役の取り置きと、中継役の前払い）。
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

    /// 中継役の配る側の中継の状態（観戦者の ACK に載せる）。ホストの形では「紹介しないで」。
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

    /// 中継役: 受けた記録を番号を変えずに積む。`idx` が次の番号より先なら、自分が飛んだので持っているものを捨ててそこから積み直す
    /// （続きを持たなくなった子は次の `poll` が REHOME で戻す）。前の番号（持っている）は捨てる。積んだら true。
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

    /// 中継役: 受け入れの依頼を控える（子は `RELAY_CHILDREN` 人まで。同じ子の依頼の重複は受けたことにする）。受けたら true。
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

    /// 中継役: 子の全員へ REHOME を送って外し、控えた依頼を捨てる（配る側は残る）。
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

    /// 中継役: 配るのをやめる（観戦を終えるとき。子へ REHOME を送る。作り直さない）。
    pub fn stop_relay<O: Outlet>(&mut self, out: &mut O) {
        self.rehome_children(out);
        if let Some(side) = self.tree.relay.as_mut() {
            side.stopped = true;
        }
    }

    /// 上（ホスト）が聞こえているかを渡す。聞こえなくなったら子を REHOME で戻す（子が空の合図で黙りを見逃さないため）。
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

    /// 中継役の配る側が子の流れを始めてよい番号か（積んだ範囲。受けたまま積んでいない分を見込み、積んだ終わりの先 1 ウィンドウまで）。
    /// まだ 1 つも積んでいなければ受ける（ホストは中継役の ack までの番号しか頼まない。最初に積んだ番号が子の始まりより先なら、`poll` が
    /// REHOME で戻す）。
    fn relay_has(&self, idx: u32) -> bool {
        let Some(side) = self.tree.relay.as_ref() else {
            return false;
        };
        !side.started || (self.head <= idx && idx <= self.next.saturating_add(SEND_WINDOW))
    }

    /// 中継役の配る側の JOIN: 依頼で名指しされた子（nonce・epoch・公開鍵）だけを受ける。流れは子の続きの番号から、無ければ依頼の
    /// 始める番号から。どちらも持っていなければ REHOME を返す（子を飛びで終わらせない）。同じ子の別の経路の 2 つ目の枠は、
    /// 1 つ目と同じ所から流す。
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
        let twin_ack = self
            .slots
            .iter()
            .flatten()
            .find(|w| w.addr != from && w.nonce == j.nonce && w.public == j.public)
            .map(|w| w.ack);
        let start = match (admitted, twin_ack) {
            (Some(a), _) => j
                .tree
                .resume_from
                .filter(|&r| self.relay_has(r))
                .or(Some(a.start).filter(|&s| self.relay_has(s))),
            (None, Some(t)) if known.is_none() => Some(t),
            _ => return Err(DropReason::NotAdmitted),
        };
        if known
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .is_some_and(|w| elapsed(now, w.keyed_at) < REKEY_GAP_MS)
        {
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
        let fresh = Self::fresh(id, name, j.ff_max, j.tree, start, now);
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

    /// 中継役の子孫と返事待ちの紹介の数（`spectator_count` の木の分）。
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

    /// JOIN の観戦者を入れる余地。中継役の子が戻ってきたなら、その席と入れ替える（満杯でも入れる）。戻った子は名前（置き場所の表）で
    /// 見分け、表に無くても（表から空けられた）続きの番号があって親を替えたことのある JOIN なら、前払い・外した中継役の取り置き・
    /// 黙っている中継役の子孫の順に使う。新しい観戦者の満杯の判定には、取り置きと前払いも数える。
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
        let t = &j.tree;
        if t.resume_from.is_some() && t.epoch != 0 && !t.has_children {
            if let Some((i, _)) = self.reps().find(|(_, w)| prepaid(w, now) > 0) {
                return Room::Back(Seat::Prepaid(i));
            }
            if let Some(i) = self
                .tree
                .reserves
                .iter()
                .position(|r| r.is_some_and(|r| r.count > 0 && before(now, r.until)))
            {
                return Room::Back(Seat::Reserve(i));
            }
            if let Some((i, _)) = self
                .reps()
                .find(|(_, w)| elapsed(now, w.last_ack_at) > RELAY_QUIET_MS && descendants(w) > 0)
            {
                return Room::Back(Seat::Quiet(i));
            }
        }
        if self.spectator_count() + self.reserved(now) < MAX_SPECTATORS {
            Room::Free
        } else {
            Room::Full
        }
    }

    /// 戻った子 `room` が元いた中継役の nonce（分かれば）。
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

    /// 戻った子 `room` を入れた: 入れ替えた席を 1 つ使う。
    pub(super) fn take_room(&mut self, room: Room, now: u32) {
        let Room::Back(seat) = room else {
            return;
        };
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

    /// 置き場所 `i` の子が戻った。中継役を外した後なら取り置きを、中継役が依頼を受け取っていればその子孫の数を 1 つ使う。中継役が
    /// まだ受け取っていなければ、受け取ったときに引く（`drop_admitted`）。同じ名前の古い置き場所で中継役が受け取ったものは消す
    /// （その中継役は、やがて子を外して前払いで数え直す）。
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

    /// 取り置きを 1 つ使う: 添字 `at` か、中継役 `nonce` の分か、無ければ切れていないどれか。
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

    /// 枠 `i` を空ける（BYE・黙った）。中継役だったなら、その子孫・前払い・返事待ちの紹介の数だけ席を `RESERVE_MS` 取り置く。
    /// 取り置きの表が埋まっていれば、いちばん新しい取り置きに足す（黙って捨てない）。
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

    /// 紹介した子の JOIN の再送（紹介が落ちた）: 同じ epoch で `ADMIT_TTL_MS` の内なら、計算せずに紹介を送り直す。中継役が依頼を
    /// 受け取った後も送り直す（受け取りで紹介を忘れると、子は改めて直接受けられ、中継役は来ない子を寿命まで数える）。違う epoch か
    /// 寿命の後なら、子は戻ってきた: 数えるのをやめ、JOIN は改めて決める（席は `room` が入れ替える）。
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

    /// 中継役に向く直接の観戦者か: 中継を許可し、回線の型が届く側で、上りが `RELAY_MIN_BPS` 以上で、「紹介しないで」でない。
    pub(super) fn fit_for_relay(w: &Watching) -> bool {
        w.acked
            && w.tree.relay_public.is_some()
            && w.tree.nat.reachable() == Some(true)
            && w.uplink >= RELAY_MIN_BPS
            && !w.relay.no_intro
    }

    /// 中継役が `RELAYS` 人に満たなければ、向く直接の観戦者を太い順に足す。
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

    /// 中継役 `i`（nonce `nonce`）への返事待ちの紹介の数（人数に数えているものと、受け取り待ちの入れ替え）。
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

    /// 新しい観戦者（流れ始める番号 `start`）を回す中継役。「直接だけ」と「子がいる」の人は回さない。中継役は、ACK が `RELAY_QUIET_MS`
    /// 以内・子孫 ＋ 返事待ちが `RELAY_CHILDREN` 未満・「紹介しないで」でない・`start` を持っている（その中継役の `base` ≤ `start` ≤
    /// ホストが知るその中継役の ack）の全部を満たす人のうち、上りの太い人。戻ってきた子は、元の中継役（nonce `avoid`）へは回さない
    /// （その中継役の BYE より子の JOIN が先に着くことがある）。
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

    /// 紹介の表の空き。無ければ、中継役が受け取って子が移った置き場所のうち最も古いものを空ける（その子が戻ったときは新しい観戦者と
    /// 同じに扱う）。
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

    /// 中継役を選べたら、子へ紹介を、中継役へ受け入れの依頼を送り、返事待ちの紹介を持つ。選べなければ false（直接受ける）。
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

    /// 子 `child`（JOIN の木の欄 `tree`）へ中継役 `at` の紹介を、中継役へ受け入れの依頼を送り、紹介の表に持つ。入れ替え（`swap`）の
    /// 紹介は人数に数えない（葉は元の枠で数えている）。
    #[allow(clippy::too_many_arguments)] // 紹介の表の 1 行を作る値をそのまま受ける。
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
        true
    }

    /// マッチの切れ目（最後に積んだ `Seg`・`Frame`・`End` が `Seg`）なら、その `Seg` の番号。
    fn cut_point(&self) -> Option<u32> {
        let seg = self.preamble_start.checked_sub(1)?;
        if seg < self.head || seg >= self.next {
            return None;
        }
        let rec = self.ring.get(seg as usize % self.ring.len())?;
        matches!(rec, Record::Seg { .. }).then_some(seg)
    }

    /// `nonce`・公開鍵 `public` の観戦者への紹介が表にある（戻った子の置き場所は数えない）。
    fn has_intro(&self, nonce: u64, public: &[u8; PUBLIC_LEN]) -> bool {
        self.tree
            .intros
            .iter()
            .flatten()
            .any(|x| !x.child_moved && x.child.nonce == nonce && x.child.public == *public)
    }

    /// 入れ替え: マッチの切れ目で、直接の観戦者が `RELAYS` 人を越えていれば、直接の葉を 1 人、紹介の条件を満たす中継役へ回す
    /// （中継役が受け取るか `ADMIT_TTL_MS` 経つまで次の葉は回さない）。回すのは枠の番号の小さい方から、中継役・中継役に向く予備・
    /// 子がいる人・測りの番を持つ人・紹介が表にある人を除いた、ack がその `Seg` の番号以上で黙っていない葉（「直接だけ」と JOIN の
    /// 「子がいる」の人は `pick_relay` が断る）。
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

    /// 中継役 `i`（nonce `nonce`）が番号 `upto` までの依頼を受け取った: その紹介は中継役の子孫の数に移った。子がもう戻っていれば、
    /// 中継役の数える子孫から 1 引いて表から消す。入れ替えの葉なら、元の直接の枠を空ける（人数は中継役の子孫へ移った）。
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

    /// 直接の枠が残っている観戦者が親を替えて戻った（epoch の進んだ JOIN）: その観戦者の紹介を、戻った子として付ける
    /// （`intro_rejoin` と同じ）。中継役がもう依頼を受け取っていれば、その席を使う（`take_placement`）。
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

    /// ACK が `RELAY_QUIET_MS` 来ない中継役の子孫へ穴あけを始める（中継役が落ちると、子はホストの NAT の穴が閉じた後に JOIN を送る）。
    /// 次の ACK が来るまで 1 回だけ。
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

    /// `poll` の頭: 中継役の形なら子の外しと依頼の寿命、ホストの形なら取り置きの寿命と依頼の送り直し。配る側をやめていれば false。
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
            // 自分が飛んだかリングから落ちて、続きを持たなくなった子は元のホストへ戻す。
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
                // ACK の後に届いた（`drop_admitted` が見ていない）、中継役が依頼を受け取った印。
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

/// 紹介 `intro` の受け入れの依頼。子の epoch は、子が紹介を受けて 1 進めた後の値（中継役への JOIN の値）。
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
    /// 最初の STREAM を送ってきた送り元。
    pub(super) addr: Option<SocketAddr>,
    /// 最後に STREAM を受けた時刻。
    pub(super) heard: Option<u32>,
    /// 紹介を受けた時刻。
    pub(super) since: u32,
}

/// 観戦者の木の数え。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeCounts {
    /// 元のホストから受けた紹介（親を中継役に替えた）。
    pub intros_taken: u64,
    /// 受けなかった紹介（古い epoch・親が中継役・子がいる・鍵を作れない）。
    pub intros_ignored: u64,
    pub admits_taken: u64,
    /// 元のホストから受けなかった依頼（番号が前 ＋ 1 でない・中継しない・控えが満杯）。
    pub admits_ignored: u64,
    /// 元のホストの鍵で開けなかった紹介・依頼（偽の紹介など）。
    pub control_dropped: u64,
    /// 中継役から受けた REHOME。
    pub rehomes: u64,
    /// 紹介から `REJOIN_MS` の内に中継役から STREAM が来ず、「直接だけ」で戻った。
    pub unreached: u64,
    /// 中継役の流れが `REJOIN_MS` 途切れて戻った。
    pub parent_silent: u64,
    /// 始めた測りの番（元のホストの MEASURE。同じ番号の送り直しは数えない）。
    pub measures_taken: u64,
    /// 元のホストから受けなかった測りの番（中継しない・親が中継役・子がいる）。
    pub measures_ignored: u64,
    /// 元のホストへ送った測りの組の便（1 組 2 通）。
    pub pairs_sent: u64,
    /// 照合のずれで、中継役の下から「直接だけ」で戻った（[`Watcher::return_on_mismatch`]）。
    pub mismatch_returns: u64,
}

/// 観戦者の中継の木の持ち物。
pub(super) struct WatchTree {
    /// いまの親が中継役なら Some（None は元のホスト）。
    pub(super) parent: Option<RelayParent>,
    /// いまの親に替えた時刻（一度も替えていなければ None）。
    parent_at: Option<u32>,
    pub(super) epoch: u16,
    relay_public: Option<[u8; PUBLIC_LEN]>,
    /// 中継の配る側の状態（dll が `set_relay_state` で渡す）。
    relay_state: RelayState,
    own: Candidates,
    direct_only: bool,
    admits: [Option<SpectateAdmit>; ADMIT_QUEUE],
    /// 最後に受けた依頼の番号。
    pub(super) admit_no: u16,
    pub(super) joined_once: bool,
    /// 一度接続した後に、元のホストから BYE(取消) を受けた。
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
}

impl Watcher {
    /// 中継できるなら中継用の公開鍵（[`RelayKey::public`]。JOIN に載せる）。None は中継しない。
    pub fn set_relay_public(&mut self, public: Option<[u8; PUBLIC_LEN]>) {
        self.tree.relay_public = public;
    }

    /// 中継の配る側の状態（[`SpectateHost::relay_state`]）。ACK に載せ、子がいる間は紹介を受けない。
    pub fn set_relay_state(&mut self, state: RelayState) {
        self.tree.relay_state = state;
    }

    /// JOIN に載せる自分の候補（ホストが中継役の穴あけの宛先として渡す）。
    pub fn set_candidates(&mut self, own: Candidates) {
        self.tree.own = own;
    }

    /// 受けた受け入れの依頼を 1 つ渡す（中継の配る側の [`SpectateHost::admit`] へ）。
    pub fn take_admit(&mut self) -> Option<SpectateAdmit> {
        self.tree.admits.iter_mut().find_map(Option::take)
    }

    /// 親を替えた回数。
    pub fn epoch(&self) -> u16 {
        self.tree.epoch
    }

    /// いまの親が元のホストか。
    pub fn parent_is_home(&self) -> bool {
        self.tree.parent.is_none()
    }

    /// 上りの太さを測ってよい: 中継でき、親が元のホストで、子（控えた依頼を含む）がいない。
    pub(super) fn measurable(&self) -> bool {
        let t = &self.tree;
        t.relay_public.is_some()
            && t.parent.is_none()
            && t.relay_state.descendants == 0
            && t.queued() == 0
    }

    /// 紹介された中継役へ届かず、元のホストへ「直接だけ」で戻った。
    pub fn direct_only(&self) -> bool {
        self.tree.direct_only
    }

    /// 一度接続した後に元のホストに断られた（満杯）。観戦を終える印。
    pub fn refused_after_join(&self) -> bool {
        self.tree.refused
    }

    /// いまの親から最後に STREAM（元のホストなら BYE も）を聞いてからの ms（何も聞いていなければ None）。
    pub fn parent_silent_ms(&self, now: u32) -> Option<u32> {
        self.parent_link().1.map(|t| elapsed(now, t))
    }

    pub fn tree_counts(&self) -> TreeCounts {
        self.tree.counts
    }

    /// ACK に載せる中継の状態（控えた依頼も子孫に数える）。中継しない観戦者は「紹介しないで」。
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

    /// JOIN の木の欄。続きの番号は、木に加わった観戦者（中継できる・親を替えた）が STREAM を 1 つ受けた後だけ載せる。
    pub(super) fn join_tree(&self) -> JoinTree {
        let t = &self.tree;
        let resume = t.joined_once && (t.epoch != 0 || t.relay_public.is_some());
        JoinTree {
            relay_public: t.relay_public,
            direct_only: t.parent.is_none() && t.direct_only,
            has_children: t.relay_state.descendants > 0 || t.queued() > 0,
            resume_from: resume.then(|| self.asm.ack()),
            nat: self.nat,
            epoch: t.epoch,
            candidates: t.own,
        }
    }

    /// JOIN（送り直しと知らせで同じ中身）。
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

    /// 接続した後に、変わった木の欄（候補・回線の型）を元のホストへ JOIN で知らせる（ホストは同じ観戦者の JOIN を読み直す）。
    /// 親が元のホストで、そのアドレスが決まっているときだけ送り、送ったら true。
    pub fn announce<O: Outlet>(&mut self, out: &mut O) -> bool {
        let Some(a) = self.host_addr.filter(|_| self.tree.parent.is_none()) else {
            return false;
        };
        let join = self.join_message();
        emit_signed(&self.key, self.session_id, a, &join, out);
        self.counts.joins_sent += 1;
        true
    }

    /// いまの親に替えた時刻（ms。一度も替えていなければ None ＝ 最初から元のホスト）。時計は `poll`・`on_datagram` に渡す `now` と同じ。
    /// 検査と、親を替えた時刻を読む口（dll はずれた記録の出どころを、時刻でなく受け渡しのリングの位置で見る）。
    pub fn parent_since(&self) -> Option<u32> {
        self.tree.parent_at
    }

    /// 照合がずれた: 親が中継役なら、その中継役へ BYE を送り、「直接だけ」で元のホストへ戻る（以後は紹介も入れ替えもされない）。
    /// 戻ったら true。親が元のホストなら何もしない。
    pub fn return_on_mismatch<O: Outlet>(&mut self, now: u32, out: &mut O) -> bool {
        if self.tree.parent.is_none() {
            return false;
        }
        self.bye(ByeReason::Cancelled, out);
        self.tree.counts.mismatch_returns += 1;
        self.go_home(true, now);
        true
    }

    /// 元のホストへ戻る（epoch を 1 進め、すぐ JOIN する）。
    pub(super) fn go_home(&mut self, direct_only: bool, now: u32) {
        self.tree.parent = None;
        self.tree.parent_at = Some(now);
        self.tree.epoch = self.tree.epoch.wrapping_add(1);
        self.tree.direct_only |= direct_only;
        self.home_heard = None;
        self.last_join = None;
    }

    /// 親が中継役のとき、紹介から `REJOIN_MS` の内に STREAM が来なければ「直接だけ」で、流れが `REJOIN_MS` 途切れたら印なしで戻る。
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

    /// 元のホストからの紹介・依頼・測りの番。元のホストの鍵でだけ開き、送り元は問わない（ホストのアドレスが決まる前・ホストの NAT が宛先ごとに
    /// ポートを変える型でも受ける。他人の中継役は鍵を持たないので子の行き先を偽れない）。
    pub(super) fn on_home_control(&mut self, bytes: &[u8], now: u32) {
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
        match pkt.msg {
            Message::SpectateIntro(i) => self.on_intro(&i, now),
            Message::SpectateAdmit(a) => self.on_admit(a),
            Message::SpectateMeasure { turn } => self.on_measure(turn, now),
            _ => self.tree.counts.control_dropped += 1,
        }
    }

    /// 紹介: JOIN の nonce と epoch が今の値と合い、親が元のホストで、子がいないときだけ受ける。中継役の公開鍵で鍵を作り直し、
    /// 組み立てと通し番号はそのまま持ち越す。
    fn on_intro(&mut self, i: &SpectateIntro, now: u32) {
        let t = &self.tree;
        let fits = i.nonce == self.nonce
            && i.epoch == t.epoch
            && t.parent.is_none()
            && t.relay_state.descendants == 0
            && t.queued() == 0;
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
            cands: i.relay,
            keys,
            addr: None,
            heard: None,
            since: now,
        });
        self.last_heard = Some(now);
        self.last_join = None;
        self.tree.counts.intros_taken += 1;
    }

    /// 受け入れの依頼: 中継でき、親が元のホストで、配る側に空きがあり（子 ＋ 控えが `RELAY_CHILDREN` 未満）、番号が前 ＋ 1 のときだけ
    /// 控える。空きが無ければ番号を受け取らない（ホストは返事待ちのまま数え、寿命で数えなくなる）。
    fn on_admit(&mut self, a: SpectateAdmit) {
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

/// 中継役の受信スレッドの 1 周（観戦者 `w` の組み立てから記録を `push_at` で積んだ後に呼ぶ）: 受けた依頼を配る側へ渡し、上が
/// `REJOIN_MS` 黙っている（または親が元のホストでない）間は子を REHOME で戻して依頼を受けず、配る側の状態を ACK に載せる。
pub fn tend_relay<O: Outlet>(w: &mut Watcher, relay: &mut SpectateHost, now: u32, out: &mut O) {
    let quiet = !w.parent_is_home() || w.parent_silent_ms(now).is_some_and(|ms| ms >= REJOIN_MS);
    relay.set_upstream(!quiet, out);
    while let Some(a) = w.take_admit() {
        relay.admit(a, now);
    }
    w.set_relay_state(relay.relay_state());
}

/// 観戦を終える（どの理由でも）: 親への BYE を先に、子への REHOME を後に送る（ホストが中継役を先に外し、戻る子の席を取り置くため）。
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
