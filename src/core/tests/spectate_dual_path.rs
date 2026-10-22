//! 観戦者が IPv4 と IPv6 の両方の候補へ JOIN を送ったときの、ホストと観戦者の振る舞い。
//!
//! ホストは送り元ごとに別の枠を作る（同じ観戦者が 2 枠になる）。観戦者が選んだ経路（最初の STREAM が届いた送り元）から ACK が来たら、
//! もう片方の枠は外す。観戦者の側は最初の STREAM の送り元に決め、以後そこだけへ送る。ネットワークは持たず、`Outlet` の記録を手で渡す。
//! アドレスは文書用の範囲（RFC 5737 / RFC 3849）。

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::seal::{Ephemeral, KeySeed};
use higanr_core::spectate::{
    HostConfig, HostEvent, MAX_SPECTATORS, PUNCH_PACKET, REJOIN_MS, SpectateHost, Watcher,
    WatcherConfig,
};
use higanr_core::wire::{self, MAX_DATAGRAM, Message, Record, SpectateJoin};
use std::net::SocketAddr;

const SID: u32 = 0x5EC7_0D01;

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn secret() -> Secret {
    Secret([0x6C; 16])
}

fn host_cfg() -> HostConfig {
    HostConfig {
        session_id: SID,
        secret: secret(),
        key_seed: KeySeed([0x5C; 32]),
    }
}

fn wseed(nonce: u64) -> KeySeed {
    KeySeed([(nonce as u8) ^ 0x3D; 32])
}

fn watcher_cfg(host: &[SocketAddr], nonce: u64) -> WatcherConfig {
    WatcherConfig {
        session_id: SID,
        secret: secret(),
        nonce,
        name: Name::new("viewer"),
        host: Candidates::from_addrs(host).unwrap(),
        host_public: host_cfg().public(),
        key_seed: wseed(nonce),
    }
}

fn join_bytes(nonce: u64) -> Vec<u8> {
    let mut name = [0u8; 32];
    name[0] = b'v';
    let msg = Message::SpectateJoin(SpectateJoin {
        name_len: 1,
        ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT as u8,
        nonce,
        name,
        build: higanr_core::update::BUILD_NUMBER,
        public: Ephemeral::from_seed(&wseed(nonce)).public(),
        tree: wire::JoinTree::NONE,
    });
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(&AuthKey::derive(&secret(), SID), SID, &msg, &mut buf);
    buf[..n].to_vec()
}

#[derive(Default)]
struct Sink(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Sink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

impl Sink {
    fn to(&self, a: SocketAddr) -> Vec<&Vec<u8>> {
        self.0
            .iter()
            .filter(|(t, _)| *t == a)
            .map(|(_, b)| b)
            .collect()
    }
}

const HOST4: &str = "203.0.113.5:47000";
const HOST6: &str = "[2001:db8::5]:47000";
const VIEW4: &str = "198.51.100.7:40000";
const VIEW6: &str = "[2001:db8:1::7]:40000";

fn host_with_records() -> SpectateHost {
    let mut host = SpectateHost::with_capacity(host_cfg(), 64);
    host.push(Record::Seg { seg: 0 }).unwrap();
    host.push(Record::Frame {
        frame: 1,
        p1: 1,
        p2: 2,
    })
    .unwrap();
    host
}

/// 同じ観戦者（nonce が同じ）の JOIN を、IPv4 と IPv6 の送り元から入れる。
fn join_both(host: &mut SpectateHost, nonce: u64, now: u32) {
    let mut sink = Sink::default();
    for from in [VIEW4, VIEW6] {
        assert_eq!(
            host.on_datagram(sa(from), &join_bytes(nonce), now, &mut sink),
            HostEvent::Joined
        );
    }
}

/// 観戦者の ACK（`Watcher` が STREAM を受けて返すもの）を、その送り元の枠へ渡す。ホストの STREAM は `streams` から `to` 宛のものを使う。
fn ack_from(host: &mut SpectateHost, streams: &Sink, to: &str, now: u32) -> HostEvent {
    let mut w = Watcher::new(watcher_cfg(&[sa(HOST4), sa(HOST6)], 0xB0B0));
    let mut acks = Sink::default();
    // 観戦者は ACK を返す側（STREAM を受けた送り元 = `to` の枠に向けたホストの便）。
    let stream = streams
        .to(sa(to))
        .into_iter()
        .next()
        .expect("STREAM がある");
    let host_side = if to == VIEW4 { sa(HOST4) } else { sa(HOST6) };
    w.on_datagram(host_side, stream, now, &mut acks);
    assert!(w.is_joined());
    assert_eq!(w.host_addr(), Some(host_side));
    let ack = acks.0.pop().expect("ACK を返した").1;
    host.on_datagram(sa(to), &ack, now, &mut Sink::default())
}

/// 両方の経路から入った同じ観戦者は、ACK を返した経路だけが残る。
#[test]
fn the_path_that_acks_stays_and_the_silent_twin_is_dropped() {
    let mut host = host_with_records();
    join_both(&mut host, 0xB0B0, 0);
    assert_eq!(host.slot_count(), 2, "送り元ごとに 1 枠");
    assert_eq!(host.spectator_count(), 1, "同じ観戦者は 1 人");
    assert_eq!(host.counts().joined, 1, "入った人数は 1");
    let mut streams = Sink::default();
    host.poll(0, &mut streams);
    assert!(!streams.to(sa(VIEW4)).is_empty() && !streams.to(sa(VIEW6)).is_empty());
    assert!(matches!(
        ack_from(&mut host, &streams, VIEW6, 20),
        HostEvent::Acked { .. }
    ));
    let left: Vec<SocketAddr> = host.spectators().map(|(a, ..)| a).collect();
    assert_eq!(left, vec![sa(VIEW6)], "ACK を返した IPv6 の枠だけ");
    assert_eq!(host.counts().twins_dropped, 1);
    let mut later = Sink::default();
    for t in [100, 1_100, 2_100] {
        host.poll(t, &mut later);
    }
    assert!(later.to(sa(VIEW4)).is_empty(), "外した経路へまだ送っている");
    // 逆の経路が選ばれたときも対称に動く（片方の族を決め打ちにしていない）。
    let mut host = host_with_records();
    join_both(&mut host, 0xB0B0, 0);
    let mut streams = Sink::default();
    host.poll(0, &mut streams);
    ack_from(&mut host, &streams, VIEW4, 20);
    let left: Vec<SocketAddr> = host.spectators().map(|(a, ..)| a).collect();
    assert_eq!(left, vec![sa(VIEW4)]);
}

/// 別の観戦者（nonce も公開鍵も違う）の枠は、同じ族でも別の族でも外さない。
#[test]
fn another_watcher_on_the_other_family_is_not_a_twin() {
    let mut host = host_with_records();
    let mut sink = Sink::default();
    host.on_datagram(sa(VIEW4), &join_bytes(0xB0B0), 0, &mut sink);
    host.on_datagram(sa(VIEW6), &join_bytes(0xC0C0), 0, &mut sink);
    assert_eq!(host.spectator_count(), 2);
    let mut streams = Sink::default();
    host.poll(0, &mut streams);
    ack_from(&mut host, &streams, VIEW4, 20);
    assert_eq!(host.spectator_count(), 2, "別人の枠を外した");
    assert_eq!(host.counts().twins_dropped, 0);
}

/// 経路が決まった後に、もう片方の経路の JOIN が遅れて届いても枠を作らない。観戦者の NAT が付け替わって黙った後の入り直しは、ホストが受ける。
#[test]
fn a_late_join_on_the_other_path_is_ignored_but_a_rebind_after_silence_is_taken() {
    let mut host = host_with_records();
    let mut sink = Sink::default();
    host.on_datagram(sa(VIEW4), &join_bytes(0xB0B0), 0, &mut sink);
    let mut streams = Sink::default();
    host.poll(0, &mut streams);
    ack_from(&mut host, &streams, VIEW4, 20);
    assert_eq!(
        host.on_datagram(sa(VIEW6), &join_bytes(0xB0B0), 30, &mut sink),
        HostEvent::Refreshed
    );
    assert_eq!(host.spectator_count(), 1, "遅れた JOIN が枠を作った");
    assert_eq!(host.counts().twins_dropped, 1);
    // 古い経路が最後に聞こえた 20 ms から `REJOIN_MS` に満たないうちはまだ生きている。ちょうど `REJOIN_MS` で、新しい送り元は入れる。
    assert_eq!(
        host.on_datagram(
            sa(VIEW6),
            &join_bytes(0xB0B0),
            20 + REJOIN_MS - 1,
            &mut sink
        ),
        HostEvent::Refreshed,
        "REJOIN_MS の 1 ms 手前"
    );
    assert_eq!(host.slot_count(), 1);
    assert_eq!(
        host.on_datagram(sa(VIEW6), &join_bytes(0xB0B0), 20 + REJOIN_MS, &mut sink),
        HostEvent::Joined,
        "ちょうど REJOIN_MS"
    );
    assert_eq!(host.slot_count(), 2);
    assert_eq!(host.spectator_count(), 1, "同じ観戦者は 1 人");
    assert_eq!(
        host.counts().joined,
        1,
        "経路を替えた入り直しは入った人数に足さない"
    );
}

/// 人数の上限は別人の数で止め、同じ観戦者の 2 つ目の経路の枠は上限いっぱいでも入れる。別人の 11 人目は断る。
#[test]
fn the_limit_counts_people_and_a_second_path_takes_a_spare_slot() {
    let mut host = host_with_records();
    let mut sink = Sink::default();
    for n in 1..=MAX_SPECTATORS as u64 {
        let from = format!("198.51.100.{n}:40000");
        assert_eq!(
            host.on_datagram(sa(&from), &join_bytes(0x100 + n), 200 * n as u32, &mut sink),
            HostEvent::Joined
        );
    }
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    // 10 人目(nonce 0x10A)が IPv6 からも JOIN する: 枠は 11、人数は 10 のまま。
    assert_eq!(
        host.on_datagram(
            sa(VIEW6),
            &join_bytes(0x100 + MAX_SPECTATORS as u64),
            3_000,
            &mut sink
        ),
        HostEvent::Joined,
        "上限いっぱいでも同じ観戦者の別経路は入れる"
    );
    assert_eq!(host.slot_count(), MAX_SPECTATORS + 1);
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    assert_eq!(host.counts().joined, MAX_SPECTATORS as u64);
    // 別人の 11 人目は断る。
    assert_eq!(
        host.on_datagram(
            sa("198.51.100.99:40000"),
            &join_bytes(0x999),
            4_000,
            &mut sink
        ),
        HostEvent::Refused
    );
    assert_eq!(host.counts().refused_full, 1);
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
}

/// 観戦者は最初に STREAM が届いた経路に決め、以後の ACK・JOIN の送り直しはそこだけへ出す。
#[test]
fn the_watcher_sticks_to_the_first_family_that_answers() {
    let mut host = host_with_records();
    join_both(&mut host, 0xB0B0, 0);
    let mut streams = Sink::default();
    host.poll(0, &mut streams);
    let mut w = Watcher::new(watcher_cfg(&[sa(HOST4), sa(HOST6)], 0xB0B0));
    let mut first = Sink::default();
    w.poll(0, &mut first);
    assert!(
        !first.to(sa(HOST4)).is_empty() && !first.to(sa(HOST6)).is_empty(),
        "最初の JOIN は両方の族の候補へ一斉に"
    );
    let mut out = Sink::default();
    w.on_datagram(sa(HOST6), streams.to(sa(VIEW6))[0], 10, &mut out);
    assert_eq!(w.host_addr(), Some(sa(HOST6)));
    // もう片方の族から遅れて届いた STREAM は別の送り元として捨て、ACK も返さない。
    let acks_before = out.0.len();
    w.on_datagram(sa(HOST4), streams.to(sa(VIEW4))[0], 11, &mut out);
    assert_eq!(out.0.len(), acks_before, "選ばなかった経路へ ACK を返した");
    assert_eq!(w.counts().foreign, 1);
    let mut later = Sink::default();
    w.poll(10_000, &mut later);
    assert!(!later.0.is_empty(), "黙ったので JOIN を送り直す");
    assert!(
        later.0.iter().all(|(to, _)| *to == sa(HOST6)),
        "選ばなかった経路へ JOIN を送り続けている"
    );
}

/// ホストの穴あけの印（HGRT でない便）を受けた数を、IPv6 の分だけ別に数える。
#[test]
fn punch_packets_are_counted_per_family() {
    let mut w = Watcher::new(watcher_cfg(&[sa(HOST4), sa(HOST6)], 0xB0B0));
    let mut sink = Sink::default();
    w.on_datagram(sa(HOST4), &PUNCH_PACKET, 0, &mut sink);
    w.on_datagram(sa(HOST6), &PUNCH_PACKET, 1, &mut sink);
    w.on_datagram(sa(HOST6), &PUNCH_PACKET, 2, &mut sink);
    let c = w.counts();
    assert_eq!((c.not_hgrt, c.not_hgrt_v6), (3, 2));
    assert!(sink.0.is_empty());
}
