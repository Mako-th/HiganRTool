//! インターネット越しの観戦の穴あけ: 配る側（`SpectateHost`）が観戦者の候補へ `PunchPlan` で穴あけのパケットを送り、観戦者
//! （`Watcher`）の JOIN が NAT を通って流れが始まるところまでを、偽の NAT（宛先のアドレスで絞る型）を両側に置いて組み上げて見る。
//! あわせて `PunchPlan` の予定（200 ms おき・5 回・JOIN が来たら止める）と、観戦者の「候補に無い送り元の STREAM」の受け入れ
//! （`learned_host`）を直に見る。
//!
//! ネットワークは 1 ms ごとに進む固定の模型（実際の UDP・時計は使わない）。片道 20 ms、落とさない（落ちるのは NAT のゲートだけ）。
//! アドレスは文書用の範囲（RFC 5737 の 203.0.113.0/24・198.51.100.0/24、RFC 1918 の LAN）。

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::seal::{Ephemeral, KeySeed, Sender, Seq};
use higanr_core::spectate::{
    DropReason, HostConfig, HostEvent, PUNCH_INTERVAL_MS, PUNCH_PACKET, PUNCH_ROUNDS, PunchPlan,
    SpectateHost, Watcher, WatcherConfig,
};
use higanr_core::wire::{self, ByeReason, Datagram, MAX_DATAGRAM, Message, Record, SpectateJoin};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn sa(a: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::from(a)), port)
}

const SID: u32 = 0x5EC7_0002;

fn secret() -> Secret {
    Secret([0x6B; 16])
}

fn key() -> AuthKey {
    AuthKey::derive(&secret(), SID)
}

fn host_cfg() -> HostConfig {
    HostConfig {
        session_id: SID,
        secret: secret(),
        key_seed: KeySeed([0x5B; 32]),
    }
}

/// 観戦者の使い捨ての鍵の種（模型では nonce から決める）。
fn wseed(nonce: u64) -> KeySeed {
    KeySeed([(nonce as u8) ^ 0x3C; 32])
}

fn watcher_cfg(host_ext: SocketAddr) -> WatcherConfig {
    WatcherConfig {
        session_id: SID,
        secret: secret(),
        nonce: 0xB0B0,
        name: Name::new("viewer"),
        host: Candidates::from_addrs(&[host_ext]).expect("host candidate"),
        host_public: host_cfg().public(),
        key_seed: wseed(0xB0B0),
    }
}

fn join_of(nonce: u64) -> Message {
    let mut name = [0u8; 32];
    name[0] = b'v';
    Message::SpectateJoin(SpectateJoin {
        name_len: 1,
        nonce,
        name,
        build: higanr_core::update::BUILD_NUMBER,
        public: Ephemeral::from_seed(&wseed(nonce)).public(),
    })
}

fn frame(f: u32) -> Record {
    Record::Frame {
        frame: f,
        p1: f as u16,
        p2: 0x100 | f as u16,
    }
}

#[derive(Default)]
struct Sink(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Sink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

fn cands(addrs: &[SocketAddr]) -> Candidates {
    Candidates::from_addrs(addrs).expect("candidates")
}

fn three() -> [SocketAddr; 3] {
    [
        sa([198, 51, 100, 7], 40_000),
        sa([192, 168, 1, 20], 50_000),
        sa([127, 0, 0, 1], 47_002),
    ]
}

#[test]
fn the_punch_packet_is_four_bytes_and_not_an_hgrt_packet() {
    assert_eq!(PUNCH_PACKET, *b"HGRp");
    assert_eq!(wire::classify(&PUNCH_PACKET), Datagram::Other);
    assert!(!PUNCH_PACKET.starts_with(b"HGRT"));
    let mut host = SpectateHost::with_capacity(host_cfg(), 16);
    let mut sink = Sink::default();
    assert_eq!(
        host.on_datagram(sa([198, 51, 100, 7], 40_000), &PUNCH_PACKET, 0, &mut sink),
        HostEvent::Dropped(DropReason::NotHgrt)
    );
    assert!(sink.0.is_empty());
    let mut w = Watcher::new(watcher_cfg(sa([203, 0, 113, 5], 47_000)));
    w.on_datagram(sa([203, 0, 113, 5], 47_000), &PUNCH_PACKET, 0, &mut sink);
    assert_eq!(w.counts().not_hgrt, 1);
    assert!(!w.is_joined());
    assert!(sink.0.is_empty());
}

#[test]
fn punches_go_to_every_candidate_every_200ms_five_times() {
    assert_eq!((PUNCH_INTERVAL_MS, PUNCH_ROUNDS), (200, 5));
    let addrs = three();
    let mut plan = PunchPlan::new(&cands(&addrs), 1_000);
    assert_eq!(plan.active(), 3);
    let mut sink = Sink::default();
    let mut at = Vec::new();
    for now in 1_000..3_000u32 {
        let n = plan.poll(now, &mut sink);
        if n > 0 {
            at.push((now, n));
        }
    }
    assert_eq!(
        at,
        [(1_000, 3), (1_200, 3), (1_400, 3), (1_600, 3), (1_800, 3)]
    );
    assert_eq!(sink.0.len(), 15);
    for a in addrs {
        let c = sink.0.iter().filter(|(to, _)| *to == a).count();
        assert_eq!(c, 5, "{a}");
    }
    assert!(sink.0.iter().all(|(_, b)| b == &PUNCH_PACKET));
    assert!(plan.is_done());
    assert_eq!(plan.rounds_sent(), 5);
    assert_eq!(plan.active(), 0);
    let mut plan = PunchPlan::new(&cands(&addrs), 1_000);
    let mut sink = Sink::default();
    assert_eq!(plan.poll(1_000, &mut sink), 3);
    assert_eq!(plan.poll(1_199, &mut sink), 0);
    assert_eq!(plan.poll(1_200, &mut sink), 3);
}

#[test]
fn a_late_poll_sends_one_round_and_the_clock_may_wrap() {
    let addrs = three();
    let mut plan = PunchPlan::new(&cands(&addrs), 0);
    let mut sink = Sink::default();
    assert_eq!(plan.poll(0, &mut sink), 3);
    // 5 秒呼ばれなかった: 1 周だけ。次はそこから 200 ms 後。
    assert_eq!(plan.poll(5_000, &mut sink), 3);
    assert_eq!(plan.poll(5_001, &mut sink), 0);
    assert_eq!(plan.poll(5_199, &mut sink), 0);
    assert_eq!(plan.poll(5_200, &mut sink), 3);
    assert_eq!(plan.rounds_sent(), 3);
    let start = u32::MAX - 100;
    let mut plan = PunchPlan::new(&cands(&addrs[..1]), start);
    let mut sink = Sink::default();
    let mut times = Vec::new();
    for k in 0..1_500u32 {
        let now = start.wrapping_add(k);
        if plan.poll(now, &mut sink) > 0 {
            times.push(k);
        }
    }
    assert_eq!(times, [0, 200, 400, 600, 800]);
}

#[test]
fn a_join_from_a_candidate_stops_only_that_candidates_punches() {
    let addrs = three();
    let mut plan = PunchPlan::new(&cands(&addrs), 0);
    let mut sink = Sink::default();
    assert_eq!(plan.poll(0, &mut sink), 3);
    // 予定にないアドレス（同じ IP の別ポート・別 IP の同じポート）は止めない。
    assert!(!plan.stop_for(sa([198, 51, 100, 7], 40_001)));
    assert!(!plan.stop_for(sa([198, 51, 100, 8], 40_000)));
    assert_eq!(plan.active(), 3);
    assert!(plan.stop_for(addrs[0]));
    assert!(!plan.stop_for(addrs[0]), "2 回目は止めるものが無い");
    assert_eq!(plan.active(), 2);
    let mut sink = Sink::default();
    for now in 1..2_000u32 {
        plan.poll(now, &mut sink);
    }
    assert!(
        sink.0.iter().all(|(to, _)| *to != addrs[0]),
        "止めたアドレスへ送った"
    );
    for a in &addrs[1..] {
        let c = sink.0.iter().filter(|(to, _)| to == a).count();
        assert_eq!(c, 4, "残りの 4 周 {a}");
    }
    assert!(plan.stop_for(addrs[1]));
    assert!(plan.stop_for(addrs[2]));
    assert!(plan.is_done());
}

#[test]
fn an_empty_plan_sends_nothing_and_eight_candidates_are_all_served() {
    let mut sink = Sink::default();
    let mut none = PunchPlan::none();
    assert!(none.is_done());
    assert_eq!(none.poll(0, &mut sink), 0);
    let mut empty = PunchPlan::new(&Candidates::EMPTY, 0);
    assert!(empty.is_done());
    assert_eq!(empty.poll(0, &mut sink), 0);
    assert!(sink.0.is_empty());
    let eight: Vec<SocketAddr> = (0..8u8)
        .map(|i| sa([198, 51, 100, 10 + i], 40_000 + u16::from(i)))
        .collect();
    let mut plan = PunchPlan::new(&cands(&eight), 0);
    assert_eq!(plan.active(), 8);
    assert_eq!(plan.poll(0, &mut sink), 8);
    assert_eq!(sink.0.len(), 8);
}

/// ホストを作り、`viewer` からの JOIN を受けさせ、`start` の記録を積んで、観戦者へ出る STREAM のバイト列を返す。
fn host_stream_to(viewer: SocketAddr, nonce: u64) -> (SpectateHost, Vec<Vec<u8>>) {
    let mut host = SpectateHost::with_capacity(host_cfg(), 64);
    host.push(Record::Seg { seg: 0 }).unwrap();
    host.push(frame(1)).unwrap();
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(&key(), SID, &join_of(nonce), &mut buf);
    let mut sink = Sink::default();
    assert_eq!(
        host.on_datagram(viewer, &buf[..n], 0, &mut sink),
        HostEvent::Joined
    );
    host.poll(0, &mut sink);
    let out: Vec<Vec<u8>> = sink
        .0
        .into_iter()
        .filter(|(to, _)| *to == viewer)
        .map(|(_, b)| b)
        .collect();
    assert!(!out.is_empty(), "ホストが観戦者へ STREAM を出す");
    (host, out)
}

#[test]
fn a_valid_stream_from_outside_the_candidates_is_accepted_and_fixes_the_host() {
    let cand = sa([203, 0, 113, 5], 47_000);
    let actual = sa([203, 0, 113, 5], 47_001);
    let viewer = sa([198, 51, 100, 7], 40_000);
    let (_host, streams) = host_stream_to(viewer, 0xB0B0);
    let mut w = Watcher::new(watcher_cfg(cand));
    let mut out = Sink::default();
    w.on_datagram(actual, &streams[0], 10, &mut out);
    assert!(w.is_joined());
    assert_eq!(w.host_addr(), Some(actual));
    assert_eq!(w.counts().learned_host, 1);
    assert_eq!(w.counts().streams, 1);
    assert_eq!(w.counts().foreign, 0);
    assert_eq!(out.0.len(), 1, "ACK を出した");
    assert_eq!(out.0[0].0, actual, "ACK は採ったアドレスへ");
    assert_eq!(w.pop(), Some(Record::Seg { seg: 0 }));
    // 以後はそのアドレスだけ。元の候補のアドレスからの通は別の送り元（`foreign`）。
    w.on_datagram(cand, &streams[0], 20, &mut out);
    assert_eq!(w.counts().foreign, 1);
    assert_eq!(w.counts().streams, 1);
    w.on_datagram(actual, &streams[0], 30, &mut out);
    assert_eq!(w.counts().streams, 2);
    assert_eq!(w.counts().learned_host, 1, "2 通目以降は採り直さない");
    let mut w2 = Watcher::new(watcher_cfg(cand));
    w2.on_datagram(cand, &streams[0], 10, &mut Sink::default());
    assert!(w2.is_joined());
    assert_eq!(w2.counts().learned_host, 0);
    let mut out = Sink::default();
    w.poll(10_000, &mut out);
    assert!(
        out.0.iter().all(|(to, _)| *to == actual),
        "{:?}",
        out.0.iter().map(|x| x.0).collect::<Vec<_>>()
    );
}

#[test]
fn everything_else_from_outside_the_candidates_is_dropped_as_foreign() {
    let cand = sa([203, 0, 113, 5], 47_000);
    let stranger = sa([192, 0, 2, 99], 9_999);
    let viewer = sa([198, 51, 100, 7], 40_000);
    let (_host, good) = host_stream_to(viewer, 0xB0B0);
    let mut w = Watcher::new(watcher_cfg(cand));
    let mut out = Sink::default();
    let mut other_secret = secret();
    other_secret.0[0] ^= 1;
    let mut other_host = SpectateHost::with_capacity(
        HostConfig {
            session_id: SID,
            secret: other_secret,
            key_seed: KeySeed([0x5B; 32]),
        },
        64,
    );
    other_host.push(Record::Seg { seg: 0 }).unwrap();
    let other_key = AuthKey::derive(&other_secret, SID);
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(&other_key, SID, &join_of(0xB0B0), &mut buf);
    let mut sink = Sink::default();
    other_host.on_datagram(viewer, &buf[..n], 0, &mut sink);
    other_host.poll(0, &mut sink);
    let forged = sink
        .0
        .iter()
        .find(|(to, _)| *to == viewer)
        .unwrap()
        .1
        .clone();
    w.on_datagram(stranger, &forged, 1, &mut out);
    let mut wrong_session = good[0].clone();
    wrong_session[8] ^= 0xFF;
    w.on_datagram(stranger, &wrong_session, 2, &mut out);
    let mut bad_tag = good[0].clone();
    let last = bad_tag.len() - 1;
    bad_tag[last] ^= 1;
    w.on_datagram(stranger, &bad_tag, 3, &mut out);
    // タグの合う BYE（観戦者の鍵の、ホストの送る向きで封じる）。ホストのアドレスにしない・host_bye を立てない。
    let to_w = w.dir_keys().expect("鍵がある").swapped();
    let n = wire::seal(
        &to_w.sealer(Sender::SpectateHost),
        &Seq::new(),
        SID,
        &Message::Bye(ByeReason::Cancelled),
        &mut buf,
    )
    .unwrap();
    w.on_datagram(stranger, &buf[..n], 4, &mut out);
    w.on_datagram(stranger, b"HGRT", 5, &mut out);
    w.on_datagram(stranger, &[0u8; 40], 6, &mut out);
    let c = w.counts();
    assert!(!w.is_joined());
    assert_eq!(w.host_bye(), None);
    assert_eq!(c.learned_host, 0);
    assert_eq!(c.streams, 0);
    assert_eq!(c.foreign, 5, "HGRT で始まる 5 通は全部 foreign: {c:?}");
    assert_eq!(c.not_hgrt, 1, "HGRT で始まらない 1 通: {c:?}");
    assert_eq!(
        (c.bad_tag, c.wrong_session, c.undecodable, c.byes),
        (0, 0, 0, 0)
    );
    assert!(out.0.is_empty(), "何も返さない");
    w.on_datagram(stranger, &good[0], 7, &mut out);
    assert!(w.is_joined());
    assert_eq!(w.host_addr(), Some(stranger));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inbound {
    Open,
    /// 内から送った相手の（IP・ポート）からだけ通す（宛先のアドレスで絞る型）。
    AddrPort,
    AddrOnly,
}

/// 内側の 1 台の前に置く NAT。外側のアドレスは `ext_ip`、外から受けるポートは `ext_port`（外へ出すとき使うポート `out_port` が違えば、
/// 外へ出すときだけ外側のポートが変わる型）。外から来た通は、宛先のポートが `ext_port` か `out_port` で、`inbound` の絞りを通ったものだけが内側へ入る。
struct Nat {
    ext_ip: IpAddr,
    ext_port: u16,
    out_port: u16,
    inbound: Inbound,
    sent_to: Vec<SocketAddr>,
    passed: u64,
    dropped: u64,
}

impl Nat {
    fn new(ext: SocketAddr, out_port: u16, inbound: Inbound) -> Nat {
        Nat {
            ext_ip: ext.ip(),
            ext_port: ext.port(),
            out_port,
            inbound,
            sent_to: Vec::new(),
            passed: 0,
            dropped: 0,
        }
    }

    fn outbound(&mut self, dst: SocketAddr) -> SocketAddr {
        if !self.sent_to.contains(&dst) {
            self.sent_to.push(dst);
        }
        SocketAddr::new(self.ext_ip, self.out_port)
    }

    fn admits(&mut self, from: SocketAddr, to: SocketAddr) -> bool {
        let port_ok =
            to.ip() == self.ext_ip && (to.port() == self.ext_port || to.port() == self.out_port);
        let filter_ok = match self.inbound {
            Inbound::Open => true,
            Inbound::AddrPort => self.sent_to.contains(&from),
            Inbound::AddrOnly => self.sent_to.iter().any(|a| a.ip() == from.ip()),
        };
        let ok = port_ok && filter_ok;
        if ok {
            self.passed += 1;
        } else {
            self.dropped += 1;
        }
        ok
    }
}

struct Packet {
    at: u32,
    from: SocketAddr,
    to: SocketAddr,
    bytes: Vec<u8>,
}

struct NatTx<'a> {
    nat: &'a mut Nat,
    wire: &'a mut Vec<Packet>,
    now: u32,
    punches: &'a mut u32,
}

const LATENCY: u32 = 20;

impl Outlet for NatTx<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        if bytes == PUNCH_PACKET {
            *self.punches += 1;
        }
        let from = self.nat.outbound(to);
        self.wire.push(Packet {
            at: self.now + LATENCY,
            from,
            to,
            bytes: bytes.to_vec(),
        });
    }
}

struct World {
    now: u32,
    host_ext: SocketAddr,
    view_ext: SocketAddr,
    nat_a: Nat,
    nat_b: Nat,
    host: SpectateHost,
    watcher: Watcher,
    plan: Option<PunchPlan>,
    /// JOIN を受けた送り元へ穴あけを止める（本番の使い方）か。
    stop_on_join: bool,
    punch_at: u32,
    wire: Vec<Packet>,
    log: Vec<Record>,
    got: Vec<Record>,
    punches: u32,
    /// 観戦者がこの時刻から動く（それまでは何も送らず、何も受けない）。
    viewer_start: u32,
    next_frame: u32,
}

impl World {
    fn new(nat_a: Nat, nat_b: Nat, punch_at: Option<u32>) -> World {
        let host_ext = SocketAddr::new(nat_a.ext_ip, nat_a.ext_port);
        let view_ext = SocketAddr::new(nat_b.ext_ip, nat_b.ext_port);
        let mut host = SpectateHost::with_capacity(host_cfg(), 4_096);
        let mut log = Vec::new();
        host.push(Record::Seg { seg: 0 }).unwrap();
        log.push(Record::Seg { seg: 0 });
        World {
            now: 0,
            host_ext,
            view_ext,
            nat_a,
            nat_b,
            host,
            watcher: Watcher::new(watcher_cfg(host_ext)),
            plan: None,
            stop_on_join: true,
            punch_at: punch_at.unwrap_or(u32::MAX),
            wire: Vec::new(),
            log,
            got: Vec::new(),
            punches: 0,
            viewer_start: 0,
            next_frame: 1,
        }
    }

    fn tick(&mut self) {
        self.now += 1;
        let now = self.now;
        if now.is_multiple_of(16) {
            let r = frame(self.next_frame);
            self.next_frame += 1;
            self.host.push(r).expect("リングバッファは溢れない");
            self.log.push(r);
        }
        if now == self.punch_at {
            self.plan = Some(PunchPlan::new(
                &Candidates::from_addrs(&[self.view_ext]).unwrap(),
                now,
            ));
        }
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.wire)
            .into_iter()
            .partition(|p| p.at <= now);
        self.wire = later;
        for p in due {
            if p.to.ip() == self.host_ext.ip() {
                if !self.nat_a.admits(p.from, p.to) {
                    continue;
                }
                let mut tx = NatTx {
                    nat: &mut self.nat_a,
                    wire: &mut self.wire,
                    now,
                    punches: &mut self.punches,
                };
                let ev = self.host.on_datagram(p.from, &p.bytes, now, &mut tx);
                if self.stop_on_join
                    && matches!(ev, HostEvent::Joined | HostEvent::Refreshed)
                    && let Some(plan) = self.plan.as_mut()
                {
                    plan.stop_for(p.from);
                }
            } else if p.to.ip() == self.view_ext.ip() {
                if !self.nat_b.admits(p.from, p.to) {
                    continue;
                }
                let mut tx = NatTx {
                    nat: &mut self.nat_b,
                    wire: &mut self.wire,
                    now,
                    punches: &mut self.punches,
                };
                if now >= self.viewer_start {
                    self.watcher.on_datagram(p.from, &p.bytes, now, &mut tx);
                }
            }
        }
        {
            let mut tx = NatTx {
                nat: &mut self.nat_a,
                wire: &mut self.wire,
                now,
                punches: &mut self.punches,
            };
            self.host.poll(now, &mut tx);
            if let Some(plan) = self.plan.as_mut() {
                plan.poll(now, &mut tx);
            }
        }
        {
            let mut tx = NatTx {
                nat: &mut self.nat_b,
                wire: &mut self.wire,
                now,
                punches: &mut self.punches,
            };
            if now >= self.viewer_start {
                self.watcher.poll(now, &mut tx);
            }
        }
        while let Some(r) = self.watcher.pop() {
            self.got.push(r);
        }
    }

    fn run_to(&mut self, t: u32) {
        while self.now < t {
            self.tick();
        }
    }
}

fn strict_pair() -> (Nat, Nat) {
    (
        Nat::new(sa([203, 0, 113, 5], 47_000), 47_000, Inbound::AddrPort),
        Nat::new(sa([198, 51, 100, 7], 40_000), 40_000, Inbound::AddrPort),
    )
}

#[test]
fn punching_opens_the_hosts_nat_and_the_viewers_join_starts_the_stream() {
    let (a, b) = strict_pair();
    let mut w = World::new(a, b, Some(1_600));
    // 穴あけの前（JOIN は 250 ms ごとに 7 回送っている）: ホストの NAT は 1 通も通さない。
    w.run_to(1_599);
    assert_eq!(w.nat_a.passed, 0, "穴あけの前に JOIN が通った");
    assert!(
        w.nat_a.dropped >= 6,
        "NAT が JOIN を落とした数 {}",
        w.nat_a.dropped
    );
    assert_eq!(w.host.spectator_count(), 0);
    assert!(!w.watcher.is_joined());
    assert!(w.watcher.counts().joins_sent >= 6);
    assert_eq!(w.punches, 0);
    // 穴あけの後: 次の JOIN（1750）が通り、流れが始まる。
    w.run_to(3_000);
    assert_eq!(w.host.spectator_count(), 1);
    assert!(w.watcher.is_joined());
    assert_eq!(w.watcher.host_addr(), Some(w.host_ext));
    assert_eq!(w.watcher.counts().learned_host, 0, "候補のアドレスのまま");
    assert!(
        w.watcher.counts().not_hgrt >= 1,
        "穴あけのパケットが観戦者まで届いた"
    );
    assert_eq!(w.punches, 1, "JOIN が来たら止まる（最初の 1 通だけ）");
    assert!(w.plan.unwrap().is_done());
    // 観戦者が受けた記録は、ホストが積んだ列の先頭からその時点の最後まで一致する。
    assert!(w.got.len() > 100, "受けた記録 {}", w.got.len());
    assert_eq!(&w.log[..w.got.len()], &w.got[..]);
    assert!(w.log.len() - w.got.len() < 5, "ほぼ追いついている");
}

#[test]
fn without_punching_the_viewers_join_never_gets_through() {
    let (a, b) = strict_pair();
    let mut w = World::new(a, b, None);
    w.run_to(8_000);
    assert_eq!(w.nat_a.passed, 0);
    assert!(w.nat_a.dropped >= 30, "{}", w.nat_a.dropped);
    assert_eq!(w.host.spectator_count(), 0);
    assert!(!w.watcher.is_joined());
    assert!(w.got.is_empty());
    assert_eq!(w.punches, 0);
}

#[test]
fn the_punches_stop_at_the_first_join_but_would_otherwise_run_all_five() {
    let (a, b) = strict_pair();
    let mut w = World::new(a, b, Some(1_600));
    w.stop_on_join = false;
    w.run_to(3_000);
    assert_eq!(w.punches, 5);
    assert_eq!(w.host.spectator_count(), 1);
    assert!(w.watcher.is_joined());
}

#[test]
fn the_stream_starts_even_when_the_punches_come_before_the_viewer_joins() {
    let (a, b) = strict_pair();
    let mut w = World::new(a, b, Some(1));
    w.viewer_start = 1_000;
    w.run_to(999);
    // 穴あけ 5 通（1・201・401・601・801 ms）は、観戦者の NAT が何も送っていない相手からなので全部落ちた。
    assert_eq!(w.punches, 5);
    assert_eq!((w.nat_b.passed, w.nat_b.dropped), (0, 5));
    assert_eq!(w.watcher.counts().not_hgrt, 0);
    assert!(!w.watcher.is_joined());
    w.run_to(3_000);
    assert!(w.watcher.is_joined(), "穴あけが先でも JOIN が通る");
    assert_eq!(w.host.spectator_count(), 1);
    assert_eq!(w.punches, 5, "JOIN が来たときにはもう送り終わっている");
    assert!(w.got.len() > 100);
    assert_eq!(&w.log[..w.got.len()], &w.got[..]);
}

#[test]
fn a_host_whose_replies_leave_from_another_port_still_streams() {
    let a = Nat::new(sa([203, 0, 113, 5], 47_000), 47_001, Inbound::Open);
    let b = Nat::new(sa([198, 51, 100, 7], 40_000), 40_000, Inbound::AddrOnly);
    let mut w = World::new(a, b, None);
    w.run_to(3_000);
    assert!(w.watcher.is_joined());
    assert_eq!(w.watcher.host_addr(), Some(sa([203, 0, 113, 5], 47_001)));
    assert_eq!(w.watcher.counts().learned_host, 1);
    assert_eq!(w.host.spectator_count(), 1);
    assert!(w.got.len() > 100, "{}", w.got.len());
    assert_eq!(&w.log[..w.got.len()], &w.got[..]);
    // ホストは ACK を受けて流れが進んでいる（観戦者の ack がホストの積んだ所に近い）。
    assert!(w.host.tail() - w.watcher.ack() < 5);
}
