//! 繋がった後の生存（PING / PONG と、沈黙による切断）と、終わりの BYE。
//!
//! PING / PONG を 1 秒に 1 回送り合い、同じ session_id のものが 10 秒何も届かなければ切れたとして BYE(切断) を出す。
//! ソケットも時計も持たない。dll と peer_sim は、繋がったときに `Liveness::new(session_id, いまの ms)` を作り、
//! 以後に受けた全部のパケットを `on_packet` へ渡し、1〜4 ms ごとに `poll_send` を呼ぶ。
//!
//! ## 終わりの BYE
//!
//! BYE は送り直しの決まりが無く、落ちると相手は 10 秒の沈黙を待つ。そこで自分から終える（`close`）ときは:
//!
//! - 直後の `poll_send` で BYE を 1 通出す。
//! - その後も PING は 1 秒に 1 回続け、相手の PING には PONG の代わりに BYE を返す（最初の BYE が落ちても次で届く）。
//! - 受けた BYE には返さない（両者が閉じているとき、返しの返しが往復し続けるため）。
//!
//! 受信スレッドかゲームスレッドから呼ばれうるので、ヒープを使わず、パニックしうる添字と unwrap を置かない。
//! 時刻は u32 の ms で、差は `wrapping_sub` で取る（49 日で折り返しても差は正しい）。

use crate::wire::{ByeReason, Message, Packet};

pub const PING_INTERVAL_MS: u32 = 1_000;

/// 相手から何も届かなければ切れたと見るまでの時間（ms）。ちょうど 10000 ms で切れたとする。
pub const SILENCE_LIMIT_MS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Alive,
    /// 自分から終えた（`close`）。相手の PING に BYE を返す。
    Closing(ByeReason),
    /// 相手から 10 秒何も届かなかった。以後は何も送らない。
    Dead,
}

/// `poll_send` が返す、いま送るもの。PING と BYE は同じ呼び出しで両方出ることがあるので欄を分けた。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outbox {
    pub ping: Option<Message>,
    pub bye: Option<Message>,
}

/// 数（ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// session_id の違うもの。「届いた」に数えない。
    pub wrong_session: u32,
    /// 送った覚えの無い send_time の PONG（最初の PING より前、または未来）。RTT に使わない。
    pub bad_pong: u32,
    pub after_dead: u32,
    pub pings_sent: u32,
    pub pongs_received: u32,
}

/// 往復の中央値に使う直近の PONG の数（1 秒に 1 回なので約 8 秒ぶん）。
pub const RTT_WINDOW: usize = 8;
/// 中央値を出すのに要る PONG の数。1〜2 個では外れ値を除けないので、それまでは出さない。
pub const RTT_MEDIAN_MIN: usize = 3;

/// 往復（ms）の中央値（偶数個なら真ん中の 2 つの平均を切り捨て）。`RTT_MEDIAN_MIN` 個に満たなければ None。
pub fn median_ms(samples: &[u32]) -> Option<u32> {
    if samples.len() < RTT_MEDIAN_MIN {
        return None;
    }
    let mut v = [0u32; RTT_WINDOW];
    let n = samples.len().min(RTT_WINDOW);
    v.get_mut(..n)?
        .copy_from_slice(samples.get(samples.len() - n..)?);
    let v = v.get_mut(..n)?;
    v.sort_unstable();
    let (lo, hi) = (*v.get((n - 1) / 2)?, *v.get(n / 2)?);
    Some(((u64::from(lo) + u64::from(hi)) / 2) as u32)
}

#[derive(Debug, Clone)]
pub struct Liveness {
    session_id: u32,
    state: State,
    /// 同じ session_id のものが最後に届いた時刻。
    last_heard: u32,
    ping_sent: Option<u32>,
    /// 最初に PING を送った時刻（PONG の send_time がこれより前なら、送った覚えが無い）。
    first_ping: Option<u32>,
    bye_due: Option<ByeReason>,
    rtt: Option<u32>,
    /// 直近の PONG の往復（ms）のリングバッファと、今までに測った数。
    rtt_hist: [u32; RTT_WINDOW],
    rtt_n: u32,
    peer_bye: Option<ByeReason>,
    counters: Counters,
}

/// 前に送った時刻から `PING_INTERVAL_MS` 経ったか。まだ 1 度も送っていなければすぐ送る。
fn due(last: Option<u32>, now: u32) -> bool {
    match last {
        None => true,
        Some(t) => now.wrapping_sub(t) >= PING_INTERVAL_MS,
    }
}

impl Liveness {
    pub fn new(session_id: u32, now: u32) -> Self {
        Self {
            session_id,
            state: State::Alive,
            last_heard: now,
            ping_sent: None,
            first_ping: None,
            bye_due: None,
            rtt: None,
            rtt_hist: [0; RTT_WINDOW],
            rtt_n: 0,
            peer_bye: None,
            counters: Counters::default(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn rtt_ms(&self) -> Option<u32> {
        self.rtt
    }

    /// 直近の PONG の往復の中央値（µs）。格子を寄せる判定の入力で、最後の 1 回だと揺れるネットワークで判定が毎秒開閉するため。
    /// 平均でなく中央値なのは、相手が読み込みで 1 度止まった外れ値で判定が 8 秒閉じるのを避けるため。
    pub fn rtt_median_us(&self) -> Option<u32> {
        let n = (self.rtt_n as usize).min(RTT_WINDOW);
        let hist = self.rtt_hist.get(..n)?;
        median_ms(hist).map(|ms| ms.saturating_mul(1_000))
    }

    pub fn peer_bye(&self) -> Option<ByeReason> {
        self.peer_bye
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn silence_ms(&self, now: u32) -> u32 {
        now.wrapping_sub(self.last_heard)
    }

    /// 10 秒（ちょうどを含む）何も届かなければ切れたとする。`on_packet` と `poll_send` の両方の入口で見る。
    /// 生きていたときだけ BYE(切断) を出す。
    fn check_silence(&mut self, now: u32) {
        if self.state == State::Dead || self.silence_ms(now) < SILENCE_LIMIT_MS {
            return;
        }
        self.bye_due = match self.state {
            State::Alive => Some(ByeReason::Disconnected),
            _ => None,
        };
        self.state = State::Dead;
    }

    /// 受けたパケット（種類を問わず全部）。返ったもの（PONG か BYE）は受けた相手へすぐ送る。受けた時刻は `now` とみなす。
    pub fn on_packet(&mut self, pkt: &Packet, now: u32) -> Option<Message> {
        self.on_packet_at(pkt, now, now)
    }

    /// `on_packet` と同じ。`rx` はこの 1 通を受信スレッドが受けた時刻（`now` 以前）で、PONG の往復だけをこれで測る。
    /// `now` で測ると、受けてから渡すまで（最大 1 フレーム）が RTT に足される（実測）。
    pub fn on_packet_at(&mut self, pkt: &Packet, now: u32, rx: u32) -> Option<Message> {
        self.check_silence(now);
        if self.state == State::Dead {
            self.counters.after_dead = self.counters.after_dead.saturating_add(1);
            return None;
        }
        if pkt.session_id != self.session_id {
            self.counters.wrong_session = self.counters.wrong_session.saturating_add(1);
            return None;
        }
        self.last_heard = now;
        match pkt.msg {
            Message::Bye(r) => {
                // 受けた BYE には返さない（返しの返しが往復し続けるため）。
                if self.peer_bye.is_none() {
                    self.peer_bye = Some(r);
                }
                None
            }
            Message::Ping { send_time } => match self.state {
                // 閉じた後は PONG の代わりに BYE を返す（最初の BYE が落ちたときの届け直し）。
                State::Closing(r) => Some(Message::Bye(r)),
                _ => Some(Message::Pong { send_time }),
            },
            Message::Pong { send_time } => {
                self.on_pong(send_time, rx);
                None
            }
            _ => None,
        }
    }

    fn on_pong(&mut self, send_time: u32, rx: u32) {
        // 最初の PING から受けた時刻までの間に無い send_time は、送った覚えが無いので RTT に使わない。
        let valid = self
            .first_ping
            .is_some_and(|f| rx.wrapping_sub(send_time) <= rx.wrapping_sub(f));
        if !valid {
            self.counters.bad_pong = self.counters.bad_pong.saturating_add(1);
            return;
        }
        self.counters.pongs_received = self.counters.pongs_received.saturating_add(1);
        let rtt = rx.wrapping_sub(send_time);
        self.rtt = Some(rtt);
        if let Some(slot) = self.rtt_hist.get_mut(self.rtt_n as usize % RTT_WINDOW) {
            *slot = rtt;
        }
        self.rtt_n = self.rtt_n.saturating_add(1);
    }

    /// 自分から終える。次の `poll_send` で BYE を出す。生きているときだけ効く。
    pub fn close(&mut self, reason: ByeReason) {
        if self.state == State::Alive {
            self.state = State::Closing(reason);
            self.bye_due = Some(reason);
        }
    }

    /// いま送るもの。切れた後は、切れたときの BYE(切断) の 1 通だけ。
    pub fn poll_send(&mut self, now: u32) -> Outbox {
        self.check_silence(now);
        let mut out = Outbox {
            ping: None,
            bye: self.bye_due.take().map(Message::Bye),
        };
        if self.state != State::Dead && due(self.ping_sent, now) {
            out.ping = Some(Message::Ping { send_time: now });
            self.ping_sent = Some(now);
            if self.first_ping.is_none() {
                self.first_ping = Some(now);
            }
            self.counters.pings_sent = self.counters.pings_sent.saturating_add(1);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: u32 = 0x1234_5678;

    fn pkt(session_id: u32, msg: Message) -> Packet {
        Packet { session_id, msg }
    }

    fn ping(t: u32) -> Packet {
        pkt(SID, Message::Ping { send_time: t })
    }

    fn ping_times(l: &mut Liveness, from: u32, until: u32) -> Vec<u32> {
        (from..=until)
            .filter(|&t| l.poll_send(t).ping.is_some())
            .collect()
    }

    #[test]
    fn pings_once_per_second_from_the_start() {
        let mut l = Liveness::new(SID, 100);
        let mut got = ping_times(&mut l, 100, 2_099);
        l.on_packet(&ping(0), 2_100);
        got.extend(ping_times(&mut l, 2_100, 3_100));
        assert_eq!(got, vec![100, 1_100, 2_100, 3_100]);
        assert_eq!(l.counters().pings_sent, 4);
    }

    #[test]
    fn ping_is_answered_with_pong_carrying_the_same_time() {
        let mut l = Liveness::new(SID, 0);
        assert_eq!(
            l.on_packet(&ping(777_777), 5),
            Some(Message::Pong { send_time: 777_777 })
        );
    }

    #[test]
    fn pong_gives_rtt() {
        let mut l = Liveness::new(SID, 1_000);
        assert_eq!(l.rtt_ms(), None);
        let p = l.poll_send(1_000).ping;
        assert_eq!(p, Some(Message::Ping { send_time: 1_000 }));
        l.on_packet(&pkt(SID, Message::Pong { send_time: 1_000 }), 1_037);
        assert_eq!(l.rtt_ms(), Some(37));
        assert!(l.poll_send(2_000).ping.is_some());
        l.on_packet(&pkt(SID, Message::Pong { send_time: 2_000 }), 2_005);
        assert_eq!(l.rtt_ms(), Some(5));
        assert_eq!(l.counters().pongs_received, 2);
    }

    #[test]
    fn rtt_median_ignores_outliers_and_needs_three_pongs() {
        assert_eq!(median_ms(&[]), None);
        assert_eq!(median_ms(&[20, 20]), None, "2 個では出さない");
        assert_eq!(median_ms(&[30, 10, 20]), Some(20));
        assert_eq!(
            median_ms(&[10, 30, 20, 40]),
            Some(25),
            "偶数個は真ん中 2 つの平均"
        );
        assert_eq!(median_ms(&[24, 24, 300, 24, 24]), Some(24), "外れ値");
        // 同じ 5 個の平均は 79 ms（外れ値で判定が閉じる値）。
        assert_eq!((24 * 4 + 300) / 5, 79);
        let mut l = Liveness::new(SID, 1_000);
        assert_eq!(l.rtt_median_us(), None);
        let mut t = 1_000;
        let pong = |l: &mut Liveness, t: &mut u32, rtt: u32| {
            assert!(l.poll_send(*t).ping.is_some());
            l.on_packet(&pkt(SID, Message::Pong { send_time: *t }), *t + rtt);
            *t += 1_000;
        };
        pong(&mut l, &mut t, 24);
        pong(&mut l, &mut t, 24);
        assert_eq!(l.rtt_median_us(), None, "2 個では出さない");
        pong(&mut l, &mut t, 300);
        assert_eq!(
            l.rtt_median_us(),
            Some(24_000),
            "3 個目の外れ値で動かない・µs"
        );
        for _ in 0..8 {
            pong(&mut l, &mut t, 40);
        }
        assert_eq!(l.rtt_median_us(), Some(40_000), "直近 8 個だけで決まる");
        assert_eq!(l.rtt_ms(), Some(40), "最後の 1 回は今までどおり");
    }

    #[test]
    fn pong_with_unknown_time_is_not_rtt() {
        let mut l = Liveness::new(SID, 1_000);
        l.on_packet(&pkt(SID, Message::Pong { send_time: 1_000 }), 1_000);
        assert_eq!(l.rtt_ms(), None);
        assert!(l.poll_send(1_000).ping.is_some());
        l.on_packet(&pkt(SID, Message::Pong { send_time: 999 }), 1_010);
        l.on_packet(&pkt(SID, Message::Pong { send_time: 1_500 }), 1_010);
        assert_eq!(l.rtt_ms(), None);
        assert_eq!(l.counters().bad_pong, 3);
        l.on_packet(&pkt(SID, Message::Pong { send_time: 1_000 }), 1_010);
        assert_eq!(l.rtt_ms(), Some(10));
    }

    #[test]
    fn pong_rtt_uses_the_receive_time_not_the_hand_over_time() {
        let mut l = Liveness::new(SID, 1_000);
        assert!(l.poll_send(1_000).ping.is_some());
        l.on_packet_at(&pkt(SID, Message::Pong { send_time: 1_000 }), 1_019, 1_003);
        assert_eq!(l.rtt_ms(), Some(3));
        assert!(l.poll_send(2_000).ping.is_some());
        l.on_packet(&pkt(SID, Message::Pong { send_time: 2_000 }), 2_017);
        assert_eq!(l.rtt_ms(), Some(17));
        for (now, rx) in [(2_020, 2_003), (2_050, 2_003)] {
            l.on_packet_at(&pkt(SID, Message::Pong { send_time: 2_000 }), now, rx);
            assert_eq!(l.rtt_ms(), Some(3), "渡した時刻 {now}");
        }
        // 沈黙は渡した時刻から数える: 最後に渡したのが 2050（受けたのは 2003）なので、12049 ではまだ生きていて、12050 で切れる。
        l.poll_send(12_049);
        assert_eq!(l.state(), State::Alive);
        l.poll_send(12_050);
        assert_eq!(l.state(), State::Dead);
        // 送った覚えの無い PONG（受けた時刻より後に送ったことになる）は、渡した時刻が後でも RTT に使わない。
        let mut l = Liveness::new(SID, 1_000);
        assert!(l.poll_send(1_000).ping.is_some());
        l.on_packet_at(&pkt(SID, Message::Pong { send_time: 1_010 }), 1_020, 1_005);
        assert_eq!((l.rtt_ms(), l.counters().bad_pong), (None, 1));
    }

    #[test]
    fn silence_of_exactly_ten_seconds_is_a_disconnect() {
        let mut l = Liveness::new(SID, 0);
        l.poll_send(0);
        l.on_packet(&ping(0), 500);
        let o = l.poll_send(10_499);
        assert_eq!(o.bye, None);
        assert_eq!(l.state(), State::Alive);
        let o = l.poll_send(10_500);
        assert_eq!(o.bye, Some(Message::Bye(ByeReason::Disconnected)));
        assert_eq!(o.ping, None, "切れた後は PING も出さない");
        assert_eq!(l.state(), State::Dead);
        for t in [10_501, 11_500, 30_000] {
            assert_eq!(l.poll_send(t), Outbox::default());
        }
    }

    #[test]
    fn any_same_session_message_counts_as_heard() {
        let mut l = Liveness::new(SID, 0);
        l.on_packet(
            &pkt(
                SID,
                Message::Agree {
                    host_seat: crate::wire::Seat::P1,
                    delay: 2,
                    mode: crate::wire::Mode::Delay,
                },
            ),
            9_000,
        );
        assert_eq!(l.poll_send(18_999).bye, None);
        assert_eq!(
            l.poll_send(19_000).bye,
            Some(Message::Bye(ByeReason::Disconnected))
        );
    }

    #[test]
    fn packet_at_the_boundary_does_not_revive() {
        // 10000 ms ちょうどに届いたものは、先に切れたと見る（handshake の時間切れと同じ順）。
        let mut l = Liveness::new(SID, 0);
        assert_eq!(l.on_packet(&ping(1), 10_000), None);
        assert_eq!(l.state(), State::Dead);
        assert_eq!(l.counters().after_dead, 1);
        assert_eq!(
            l.poll_send(10_000).bye,
            Some(Message::Bye(ByeReason::Disconnected))
        );
    }

    #[test]
    fn wrong_session_is_not_heard() {
        let mut l = Liveness::new(SID, 0);
        for t in (1_000..10_000).step_by(1_000) {
            assert_eq!(
                l.on_packet(&pkt(SID ^ 1, Message::Ping { send_time: t }), t),
                None
            );
        }
        assert_eq!(l.counters().wrong_session, 9);
        assert_eq!(l.poll_send(9_999).bye, None);
        assert_eq!(
            l.poll_send(10_000).bye,
            Some(Message::Bye(ByeReason::Disconnected))
        );
        let mut m = Liveness::new(SID, 0);
        for t in (1_000..10_000).step_by(1_000) {
            m.on_packet(&ping(t), t);
        }
        assert_eq!(m.poll_send(10_000).bye, None);
        assert_eq!(m.state(), State::Alive);
    }

    #[test]
    fn close_sends_one_bye_then_answers_pings_with_bye() {
        let mut l = Liveness::new(SID, 0);
        assert!(l.poll_send(0).ping.is_some());
        l.close(ByeReason::Finished);
        assert_eq!(l.state(), State::Closing(ByeReason::Finished));
        let o = l.poll_send(10);
        assert_eq!(o.bye, Some(Message::Bye(ByeReason::Finished)));
        assert_eq!(l.poll_send(11).bye, None, "BYE は close ごとに 1 通");
        assert_eq!(
            l.on_packet(&ping(5), 20),
            Some(Message::Bye(ByeReason::Finished))
        );
        assert_eq!(
            l.on_packet(&pkt(SID, Message::Bye(ByeReason::Finished)), 30),
            None
        );
        assert_eq!(l.peer_bye(), Some(ByeReason::Finished));
        assert!(l.poll_send(1_000).ping.is_some());
        l.close(ByeReason::Desync);
        assert_eq!(l.state(), State::Closing(ByeReason::Finished));
        assert_eq!(l.poll_send(1_001).bye, None);
    }

    #[test]
    fn closed_side_goes_dead_without_a_second_bye() {
        let mut l = Liveness::new(SID, 0);
        l.close(ByeReason::Finished);
        assert_eq!(l.poll_send(0).bye, Some(Message::Bye(ByeReason::Finished)));
        assert_eq!(l.poll_send(10_000).bye, None);
        assert_eq!(l.state(), State::Dead);
        l.close(ByeReason::Cancelled);
        assert_eq!(l.poll_send(10_001), Outbox::default());
    }

    #[test]
    fn peer_bye_keeps_the_first_reason_and_stays_alive() {
        let mut l = Liveness::new(SID, 0);
        l.on_packet(&pkt(SID, Message::Bye(ByeReason::Finished)), 10);
        l.on_packet(&pkt(SID, Message::Bye(ByeReason::Desync)), 20);
        assert_eq!(l.peer_bye(), Some(ByeReason::Finished));
        assert_eq!(l.state(), State::Alive);
        assert_eq!(
            l.on_packet(&ping(3), 30),
            Some(Message::Pong { send_time: 3 })
        );
    }

    #[test]
    fn clock_wraparound_does_not_disconnect() {
        let start = u32::MAX - 3_000;
        let mut l = Liveness::new(SID, start);
        l.on_packet(&ping(0), start.wrapping_add(5_000));
        assert_eq!(l.poll_send(start.wrapping_add(14_999)).bye, None);
        assert_eq!(l.state(), State::Alive);
        assert_eq!(
            l.poll_send(start.wrapping_add(15_000)).bye,
            Some(Message::Bye(ByeReason::Disconnected))
        );
    }
}
