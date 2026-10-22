//! 試合中の観戦者 → ホストの上りの太さの測り（中継役を選ぶ値 `note_uplink` を作る）。ホストは中継役が足りない間だけ、直接の観戦者に
//! 1 人ずつ番（SPECTATE_MEASURE）を渡し、観戦者は 2 通の組（SPECTATE_PAIR。1,200 B ちょうど）を `PAIR_GAP_MS` ごとに
//! `MEASURE_PAIRS` 組送る。ホストは組の 2 通の到着の差からその組の太さを出し（packet pair。差は経路のいちばん細い所の太さで決まる）、
//! 使える組の中央値を見積もりにする。どれも受信スレッドで走り、確保しない。ホストの時刻は `poll` が ms、`on_pair` が µs
//! （別の時計でよい。µs は組の 2 通の差にだけ使う）。

use super::tree::before;
use super::*;
use crate::host_pick::MIN_WIDTH_US;
use crate::spectate_rate::{FULL_PPM, RELAY_MIN_BPS};
use crate::wire::SpectatePair;

/// 1 回の番で観戦者が送る組の数。
pub const MEASURE_PAIRS: u8 = 10;
/// PAIR 1 通の長さ（B。封じた全体）。経路 MTU を割らない上限いっぱいにして、組の 2 通の差を太さに比べて大きくする。
pub const PAIR_LEN: usize = MAX_DATAGRAM;
/// 観戦者が組を送る間隔（ms）。組どうしを離し、前の組の後ろに次の組が詰まらないようにする（細い回線で詰まっても、組の 2 通は続けて
/// 並ぶので差は変わらない）。
pub const PAIR_GAP_MS: u32 = 100;
/// ホストが MEASURE を送り直す間隔（ms。最初の PAIR が来るまで）と、送る回数の上限（最初の 1 通を含む）。
pub const MEASURE_RESEND_MS: u32 = 300;
pub const MEASURE_SENDS: u8 = 3;
/// 番を渡してからこの時間で締める（ms）。
pub const MEASURE_CLOSE_MS: u32 = 2_000;
// 最後の MEASURE で始めた観戦者の組も、片道の分を残して締めの前に送り終える。
const _: () = assert!(
    (MEASURE_SENDS as u32 - 1) * MEASURE_RESEND_MS + (MEASURE_PAIRS as u32 - 1) * PAIR_GAP_MS
        < MEASURE_CLOSE_MS
);
/// 見積もりに要る、使える組の数の下限。
pub const MEASURE_MIN_PAIRS: usize = 3;
/// 測れなかった観戦者に番をもう 1 回回すまでの時間（ms）と、1 人（観戦者の枠）の番の回数の上限。
pub const MEASURE_RETRY_MS: u32 = 30_000;
pub const MEASURE_ROUNDS: u8 = 2;
/// 1 組の見積もりの上限（B/s）: 組の 2 通が `MIN_WIDTH_US` で届いた太さ（1.2 MB/s、約 9.6 Mbps）。これより太い同士の順は付かないが、
/// 中継に要る太さを大きく越える。
pub const PAIR_CAP: u32 = (PAIR_LEN as u64 * 1_000_000 / MIN_WIDTH_US) as u32;
const _: () = assert!(PAIR_CAP > RELAY_MIN_BPS);

/// 1 組の 2 通の到着（µs）。
#[derive(Debug, Clone, Copy, Default)]
struct Arrival {
    first: Option<u64>,
    second: Option<u64>,
    /// 2 通目が 1 通目より先に着いた（順の入れ替わった組は使わない）。
    reversed: bool,
}

impl Arrival {
    /// 使える組の太さ（B/s）。片方だけ・順の逆の組は None。
    fn bps(&self) -> Option<u32> {
        let (Some(f), Some(s)) = (self.first, self.second) else {
            return None;
        };
        if self.reversed {
            return None;
        }
        let width = s.checked_sub(f)?.max(MIN_WIDTH_US);
        Some((PAIR_LEN as u64 * 1_000_000 / width).clamp(1, u64::from(PAIR_CAP)) as u32)
    }
}

/// いま番を持っている観戦者の測り。
#[derive(Debug, Clone, Copy)]
struct Turn {
    slot: usize,
    addr: SocketAddr,
    nonce: u64,
    public: [u8; PUBLIC_LEN],
    turn: u16,
    started: u32,
    sends: u8,
    last_send: u32,
    /// PAIR を 1 通でも受けた（MEASURE の送り直しをやめる）。
    heard: bool,
    arrivals: [Arrival; MEASURE_PAIRS as usize],
}

/// ホストの測りの持ち物（ホストに 1 つ。観戦者の枠には回数と時刻だけ）。
pub(super) struct Meter {
    turn: Option<Turn>,
    last_turn: u16,
    fake: Option<u32>,
}

impl Meter {
    pub(super) fn new() -> Meter {
        Meter {
            turn: None,
            last_turn: 0,
            fake: None,
        }
    }
}

/// 使える組の太さの中央値（偶数個なら小さい方。中継役を太く見誤らない）。
fn median(vals: &mut [u32]) -> Option<u32> {
    vals.sort_unstable();
    let mid = vals.len().checked_sub(1)? / 2;
    vals.get(mid).copied()
}

impl SpectateHost {
    /// **検査のためだけ**: 使える組が 2 組以上そろった番の見積もりを、この値（B/s）にする。loopback では皆の太さが同じなので、
    /// 実機のテストで片方を太く見せる。
    #[doc(hidden)]
    pub fn set_fake_uplink(&mut self, bps: Option<u32>) {
        self.meter.fake = bps;
    }

    /// `set_fake_uplink` で置いた値。
    #[doc(hidden)]
    pub fn fake_uplink(&self) -> Option<u32> {
        self.meter.fake
    }

    /// 送り元 `addr` の観戦者の上りの見積もり（B/s。0 はまだ分からない）。
    pub fn uplink_of(&self, addr: SocketAddr) -> Option<u32> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(|w| w.uplink)
    }

    /// いま測りの番を持っている観戦者。
    pub fn measuring(&self) -> Option<SocketAddr> {
        self.meter.turn.as_ref().map(|t| t.addr)
    }

    /// 測りの組 PAIR の 1 通（`wire::peek_type` が `SpectatePair` のもの）。`now_us` は受信スレッドが読んだ時刻（µs）。番を持つ観戦者の
    /// 送り元から来たものだけを、その観戦者の鍵で開く（ほかは開かずに捨てる）。番の組として受けたら true。
    pub fn on_pair(&mut self, from: SocketAddr, bytes: &[u8], now_us: u64) -> bool {
        let Some((slot, nonce, public, turn)) = self
            .meter
            .turn
            .as_ref()
            .filter(|t| t.addr == from)
            .map(|t| (t.slot, t.nonce, t.public, t.turn))
        else {
            self.counts.pairs_dropped += 1;
            return false;
        };
        let keys = self
            .slots
            .get(slot)
            .and_then(Option::as_ref)
            .filter(|w| w.nonce == nonce && w.public == public)
            .map(|w| w.keys);
        let pair = keys.and_then(|k| {
            if wire::peek_session(bytes) != Some(self.session_id) {
                return None;
            }
            match wire::decode(&self.key, Some(&k.opener()), bytes) {
                Ok(p) => match p.msg {
                    Message::SpectatePair(x) if x.turn == turn => Some(x),
                    _ => None,
                },
                Err(_) => None,
            }
        });
        let arrival = pair.and_then(|p| {
            let t = self.meter.turn.as_mut()?;
            t.arrivals.get_mut(usize::from(p.pair)).map(|a| (a, p))
        });
        let Some((a, p)) = arrival else {
            self.counts.pairs_dropped += 1;
            return false;
        };
        // 重なった便は最初の 1 通だけ使う。
        let fresh = if p.second {
            a.second.is_none()
        } else {
            a.first.is_none()
        };
        if !fresh {
            self.counts.pairs_dropped += 1;
            return false;
        }
        if p.second {
            a.second = Some(now_us);
        } else {
            a.reversed = a.second.is_some();
            a.first = Some(now_us);
        }
        if let Some(t) = self.meter.turn.as_mut() {
            t.heard = true;
        }
        self.counts.pairs += 1;
        true
    }

    /// 測ってよいか: 元のホストで、中継役（と中継役に向くと分かった直接の観戦者）が `RELAYS` 人に満たず、観戦者への送信を絞っていない
    /// （対戦の往復が延びている間は、組の 1,200 B を相手の便の前に並べない）。
    fn may_measure(&self) -> bool {
        if self.tree.is_relay() || self.throttle.level_ppm() < FULL_PPM || self.throttle.paused() {
            return false;
        }
        let relays = (0..SLOT_CAP)
            .filter(|&i| self.is_representative(i))
            .filter_map(|i| self.slots.get(i).and_then(Option::as_ref))
            .filter(|w| w.relay_role || Self::fit_for_relay(w))
            .count();
        relays < RELAYS
    }

    /// 番を渡す観戦者（枠の順で最初の人）: 中継を許可し、回線の型が届く側で、ACK を返していて、中継役でなく、ACK が「紹介しないで」でなく
    /// （組を送らないので番が空振りする）、上りがまだ分からず、番の回数が上限の内（2 回目は測れなかった番から `MEASURE_RETRY_MS` の後）。
    fn measure_candidate(&self, now: u32) -> Option<usize> {
        (0..SLOT_CAP).find(|&i| {
            self.is_representative(i)
                && self.slots.get(i).and_then(Option::as_ref).is_some_and(|w| {
                    w.acked
                        && w.tree.relay_public.is_some()
                        && w.tree.nat.reachable() == Some(true)
                        && !w.relay_role
                        && !w.relay.no_intro
                        && w.uplink == 0
                        && w.measure_rounds < MEASURE_ROUNDS
                        && (w.measure_rounds == 0 || !before(now, w.measure_after))
                })
        })
    }

    /// `poll` の測り: 番を持つ観戦者がいれば MEASURE の送り直しと締め、いなければ測ってよいときに次の 1 人へ番を渡す。
    pub(super) fn measure_poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if self.meter.turn.is_some() {
            self.tend_turn(now, out);
            return;
        }
        if !self.may_measure() {
            return;
        }
        let Some(i) = self.measure_candidate(now) else {
            return;
        };
        let Some(w) = self.slots.get_mut(i).and_then(Option::as_mut) else {
            return;
        };
        let turn = match self.meter.last_turn.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        self.meter.last_turn = turn;
        w.measure_rounds += 1;
        emit_sealed(
            &w.keys,
            Sender::SpectateHost,
            &self.seq,
            self.session_id,
            w.addr,
            &Message::SpectateMeasure { turn },
            out,
        );
        self.meter.turn = Some(Turn {
            slot: i,
            addr: w.addr,
            nonce: w.nonce,
            public: w.public,
            turn,
            started: now,
            sends: 1,
            last_send: now,
            heard: false,
            arrivals: [Arrival::default(); MEASURE_PAIRS as usize],
        });
        self.counts.measures += 1;
    }

    /// 番を持つ観戦者の測り: 抜けた（枠が別の人になった）ら捨て、全部の組がそろったか `MEASURE_CLOSE_MS` 経ったら締め、まだ PAIR が
    /// 来なければ MEASURE を送り直す。
    fn tend_turn<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let Some(t) = self.meter.turn else {
            return;
        };
        let Some(keys) = self
            .slots
            .get(t.slot)
            .and_then(Option::as_ref)
            .filter(|w| w.addr == t.addr && w.nonce == t.nonce && w.public == t.public)
            .map(|w| w.keys)
        else {
            self.meter.turn = None;
            self.counts.uplinks_unmeasured += 1;
            return;
        };
        let complete = t
            .arrivals
            .iter()
            .all(|a| a.first.is_some() && a.second.is_some());
        if complete || elapsed(now, t.started) >= MEASURE_CLOSE_MS {
            let mut vals = [0u32; MEASURE_PAIRS as usize];
            let mut n = 0;
            for v in t.arrivals.iter().filter_map(Arrival::bps) {
                if let Some(slot) = vals.get_mut(n) {
                    *slot = v;
                    n += 1;
                }
            }
            let est = match self.meter.fake {
                Some(v) if n >= 2 => Some(v),
                _ if n >= MEASURE_MIN_PAIRS => vals.get_mut(..n).and_then(median),
                _ => None,
            };
            self.meter.turn = None;
            match est {
                Some(v) => {
                    self.note_uplink(t.addr, v.max(1));
                    self.counts.uplinks_measured += 1;
                }
                None => {
                    if let Some(w) = self.slots.get_mut(t.slot).and_then(Option::as_mut) {
                        w.measure_after = now.wrapping_add(MEASURE_RETRY_MS);
                    }
                    self.counts.uplinks_unmeasured += 1;
                }
            }
            return;
        }
        if !t.heard && t.sends < MEASURE_SENDS && elapsed(now, t.last_send) >= MEASURE_RESEND_MS {
            emit_sealed(
                &keys,
                Sender::SpectateHost,
                &self.seq,
                self.session_id,
                t.addr,
                &Message::SpectateMeasure { turn: t.turn },
                out,
            );
            if let Some(t) = self.meter.turn.as_mut() {
                t.sends += 1;
                t.last_send = now;
            }
            self.counts.measure_resends += 1;
        }
    }
}

// ============================ 観戦者 ============================

/// 観戦者の測り（受けた番と、送った組）。
#[derive(Debug, Clone, Copy)]
pub(super) struct WatchMeter {
    /// 最後に受けた番の番号（0 はまだ無い。同じ番号の送り直しは無視する）。
    turn: u16,
    sent: u8,
    next_at: u32,
    active: bool,
}

impl WatchMeter {
    pub(super) fn new() -> WatchMeter {
        WatchMeter {
            turn: 0,
            sent: 0,
            next_at: 0,
            active: false,
        }
    }
}

impl Watcher {
    /// 元のホストの MEASURE: 測れる（`measurable`）ときだけ、新しい番号の番を始める（前の番は捨てる）。
    pub(super) fn on_measure(&mut self, turn: u16, now: u32) {
        if turn == self.meter.turn {
            return;
        }
        if !self.measurable() {
            self.tree.counts.measures_ignored += 1;
            return;
        }
        self.meter = WatchMeter {
            turn,
            sent: 0,
            next_at: now,
            active: true,
        };
        self.tree.counts.measures_taken += 1;
    }

    /// `poll` の測り: 番の間、`PAIR_GAP_MS` ごとに組（2 通を続けて）を元のホストへ送る。遅れて呼ばれても 1 回に 1 組だけ。測れなく
    /// なったら（子ができた・親を替えた）やめる。
    pub(super) fn send_pairs<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let m = self.meter;
        if !m.active || before(now, m.next_at) {
            return;
        }
        let (Some(to), Some(keys)) = (self.host_addr, self.keys) else {
            self.meter.active = false;
            return;
        };
        if !self.measurable() {
            self.meter.active = false;
            return;
        }
        for second in [false, true] {
            let p = SpectatePair {
                turn: m.turn,
                pair: m.sent,
                second,
            };
            emit_sealed(
                &keys,
                Sender::Spectator,
                &self.seq,
                self.session_id,
                to,
                &Message::SpectatePair(p),
                out,
            );
        }
        self.tree.counts.pairs_sent += 2;
        let sent = m.sent + 1;
        self.meter = WatchMeter {
            sent,
            next_at: now.wrapping_add(PAIR_GAP_MS),
            active: sent < MEASURE_PAIRS,
            ..m
        };
    }
}
