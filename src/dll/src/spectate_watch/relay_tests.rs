//! 中継の木の観戦者の側の配線の検査。対象は`recv.rs`の振り分け・中継の配布する側・`leave_tree`・`WatchSlot`の中継の設定・断りの読み手。
//! 仮のネットワークの模型は受け取り（`WatchRecv`）を中継役と子にする。ホストはcoreの`SpectateHost`を使う（便は失われない・片道2 ms）。
//! 本物の通信層（loopback）の検査はファイルの終わりにある。
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::seal::{Ephemeral, KeySeed, Sender, Seq};
use higanr_core::spectate::{HostConfig, JOIN_RESEND_MS, SpectateHost, Watcher, WatcherConfig};
use higanr_core::spectate_pace::FF_UPDATES_DEFAULT;
use higanr_core::spectate_rate::RELAY_MIN_BPS;
use higanr_core::wire::{
    self, ByeReason, JoinTree, MAX_DATAGRAM, Message, PeekedType, Record, SpectateIntro,
    SpectateStream,
};

use super::*;
use crate::net_udp::{self, GameNet, RecvThread, SecondStun};
use crate::netplay::fake::FakeNet;

const SESSION: u32 = 0x5EC7_0B01;
const SECRET: Secret = Secret(*b"relay-wiring-01!");
const HOST_SEED: KeySeed = KeySeed([0x71; 32]);
const HOST: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    47_301,
);
const RELAY: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_301,
);
const CHILD: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_302,
);
const LEAF: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_303,
);
const REACHABLE: NatMark = NatMark {
    symmetry: Symmetry::No,
    upnp: false,
};
/// 中継役に向く上りの太さ。
const THICK: u32 = 2 * RELAY_MIN_BPS;
const ONE_WAY_US: u64 = 2_000;
/// 仮のネットワークに置いておける便の数（送信時に確保しないよう前もって確保しておく）。
const QUEUE_CAP: usize = 4_096;
/// 子が動き始める時刻（ms。中継役がホストに接続した後）。
const CHILD_START_MS: u64 = 400;

fn host_cfg() -> HostConfig {
    HostConfig {
        session_id: SESSION,
        secret: SECRET,
        key_seed: HOST_SEED,
    }
}

fn host_public() -> [u8; 32] {
    Ephemeral::from_seed(&HOST_SEED).public()
}

fn watcher_cfg(host: SocketAddr, nonce: u64, seed: u8) -> WatcherConfig {
    WatcherConfig {
        session_id: SESSION,
        secret: SECRET,
        nonce,
        name: Name::new("w"),
        host: Candidates::from_addrs(&[host]).unwrap(),
        host_public: host_public(),
        key_seed: KeySeed([seed; 32]),
    }
}

fn watch_cfg() -> WatchConfig {
    WatchConfig {
        wait_budget_us: 16_000,
        margin_frames: WATCH_MARGIN_FRAMES,
        require_net_selection: false,
        ff_max: FF_UPDATES_DEFAULT,
    }
}

fn frame(n: u32) -> Record {
    Record::Frame {
        frame: n,
        p1: n as u16,
        p2: !(n as u16),
    }
}

/// ホストが観戦者`public`へ封じた1通（BYEの断りなどホストの決まりを通さずに作る）。
fn from_host(public: &[u8; 32], msg: &Message) -> Vec<u8> {
    sealed_by(&HOST_SEED, public, &Seq::new(), msg)
}

/// 鍵の種`seed`の送り手（元のホストか中継役）が観戦者`public`へ封じた1通。
fn sealed_by(seed: &KeySeed, public: &[u8; 32], seq: &Seq, msg: &Message) -> Vec<u8> {
    let keys = Ephemeral::from_seed(seed)
        .dir_keys(public, true, SESSION, &AuthKey::derive(&SECRET, SESSION))
        .expect("鍵");
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::seal(
        &keys.sealer(Sender::SpectateHost),
        seq,
        SESSION,
        msg,
        &mut buf,
    )
    .expect("封じる");
    buf[..n].to_vec()
}

fn join_tree_of(bytes: &[u8]) -> JoinTree {
    match wire::decode(&AuthKey::derive(&SECRET, SESSION), None, bytes)
        .expect("JOIN を開ける")
        .msg
    {
        Message::SpectateJoin(j) => j.tree,
        m => panic!("JOIN でない: {m:?}"),
    }
}

#[derive(Default)]
struct Sink(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Sink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

struct Pkt {
    at_us: u64,
    from: SocketAddr,
    to: SocketAddr,
    len: usize,
    bytes: [u8; MAX_DATAGRAM],
}

/// 仮のネットワーク。置き場は前もって確保してある。送信時には確保しない。
struct Wire {
    q: Vec<Pkt>,
    /// 中継役が出したBYEとREHOME（宛先と種類）の順。
    order: Vec<(SocketAddr, PeekedType)>,
    /// 送り元ごとの最初のJOIN。
    joins: Vec<(SocketAddr, usize, [u8; MAX_DATAGRAM])>,
    /// ホストへ送ったJOIN（送り元, 送った時刻µs）。
    join_log: Vec<(SocketAddr, u64)>,
    dropped: u64,
}

struct Tx<'a> {
    w: &'a mut Wire,
    from: SocketAddr,
    now_us: u64,
}

impl Outlet for Tx<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let len = bytes.len().min(MAX_DATAGRAM);
        let mut buf = [0u8; MAX_DATAGRAM];
        buf[..len].copy_from_slice(&bytes[..len]);
        let w = &mut *self.w;
        let kind = wire::peek_type(bytes);
        if self.from == RELAY
            && matches!(kind, PeekedType::Bye | PeekedType::SpectateRehome)
            && w.order.len() < w.order.capacity()
        {
            w.order.push((to, kind));
        }
        if kind == PeekedType::SpectateJoin
            && to == HOST
            && !w.joins.iter().any(|j| j.0 == self.from)
            && w.joins.len() < w.joins.capacity()
        {
            w.joins.push((self.from, len, buf));
        }
        if kind == PeekedType::SpectateJoin
            && to == HOST
            && w.join_log.len() < w.join_log.capacity()
        {
            w.join_log.push((self.from, self.now_us));
        }
        if w.q.len() >= w.q.capacity() {
            w.dropped += 1;
            return;
        }
        w.q.push(Pkt {
            at_us: self.now_us + ONE_WAY_US,
            from: self.from,
            to,
            len,
            bytes: buf,
        });
    }
}

struct Node {
    addr: SocketAddr,
    slot: Arc<WatchSlot>,
    feed: Arc<WatchFeed>,
    recv: WatchRecv,
    start_us: u64,
    got: Vec<Record>,
}

fn node(addr: SocketAddr, relay: bool, nonce: u64, seed: u8, start_us: u64) -> Node {
    let slot = Arc::new(WatchSlot::new());
    if relay {
        slot.enable_relay(Candidates::from_addrs(&[addr]).unwrap());
    }
    slot.offer_with(
        watcher_cfg(HOST, nonce, seed),
        relay.then_some(Ok(KeySeed([seed ^ 0x5A; 32]))),
    );
    let feed = Arc::new(WatchFeed::with_len(4_096));
    let mut recv = WatchRecv::new(Arc::clone(&slot), Arc::clone(&feed), FF_UPDATES_DEFAULT);
    recv.note_nat(REACHABLE);
    Node {
        addr,
        slot,
        feed,
        recv,
        start_us,
        got: Vec::with_capacity(4_096),
    }
}

/// ホスト・中継役（`nodes[0]`）・子（`nodes[1]`）の模型。1歩1 ms。
struct Model {
    now_us: u64,
    host: SpectateHost,
    nodes: Vec<Node>,
    wire: Wire,
    log: Vec<Record>,
    /// ホストが記録を積むのをやめる時刻（ms）。
    push_until_ms: u64,
    /// 受け取り（`WatchRecv`）の呼び出しの中で確保・解放した回数。
    recv_allocs: u64,
    /// ホストが中継役の上りを知る時刻（ms。それまでは中継役に向かない）。
    uplink_from_ms: u64,
    /// ホストが便を受け始める時刻（ms。それまでに着いた便は捨てる）。
    host_from_ms: u64,
}

impl Model {
    /// `relay`: 中継役の候補（`nodes[0]`）が中継する設定かどうか。子（`nodes[1]`）は中継しない設定。
    fn new(relay: bool) -> Model {
        let mut host = SpectateHost::with_capacity(host_cfg(), 1 << 14);
        host.push(Record::Seg { seg: 0 }).unwrap();
        Model {
            now_us: 0,
            host,
            nodes: vec![
                node(RELAY, relay, 0xA1, 0x21, 0),
                node(CHILD, false, 0xA2, 0x22, CHILD_START_MS * 1_000),
            ],
            wire: Wire {
                q: Vec::with_capacity(QUEUE_CAP),
                order: Vec::with_capacity(64),
                joins: Vec::with_capacity(16),
                join_log: Vec::with_capacity(4_096),
                dropped: 0,
            },
            log: vec![Record::Seg { seg: 0 }],
            push_until_ms: u64::MAX,
            recv_allocs: 0,
            uplink_from_ms: 0,
            host_from_ms: 0,
        }
    }

    fn step(&mut self) {
        self.now_us += 1_000;
        let now_us = self.now_us;
        let now = (now_us / 1_000) as u32;
        let push_until = self.push_until_ms;
        let uplink_known = now_us >= self.uplink_from_ms * 1_000;
        let host_up = now_us >= self.host_from_ms * 1_000;
        let Model {
            host,
            nodes,
            wire,
            log,
            recv_allocs,
            ..
        } = self;
        if now.is_multiple_of(16) && u64::from(now) < push_until {
            let f = frame(log.len() as u32);
            host.push(f).unwrap();
            log.push(f);
        }
        // 中継役の上りの見積もり（測定はcoreの検査が見る）。
        if uplink_known {
            host.note_uplink(RELAY, THICK);
        }
        let mut i = 0;
        while i < wire.q.len() {
            if wire.q[i].at_us > now_us {
                i += 1;
                continue;
            }
            let p = wire.q.remove(i);
            let bytes = &p.bytes[..p.len];
            let mut tx = Tx {
                w: &mut *wire,
                from: p.to,
                now_us,
            };
            if p.to == HOST {
                if host_up {
                    host.on_datagram(p.from, bytes, now, &mut tx);
                }
                continue;
            }
            if let Some(n) = nodes
                .iter_mut()
                .find(|n| n.addr == p.to && now_us >= n.start_us)
            {
                let before = crate::test_alloc::count();
                n.recv.on_datagram(p.from, bytes, now_us, &mut tx);
                *recv_allocs += crate::test_alloc::count() - before;
            }
        }
        host.poll(
            now,
            &mut Tx {
                w: &mut *wire,
                from: HOST,
                now_us,
            },
        );
        for n in nodes.iter_mut().filter(|n| now_us >= n.start_us) {
            let mut tx = Tx {
                w: &mut *wire,
                from: n.addr,
                now_us,
            };
            let before = crate::test_alloc::count();
            n.recv.service(now_us, &mut tx);
            *recv_allocs += crate::test_alloc::count() - before;
            while let Some(e) = n.feed.peek() {
                if n.got.len() < n.got.capacity() {
                    n.got.push(e.rec);
                }
                n.feed.advance();
            }
        }
    }

    fn run_until(&mut self, ms: u64) {
        while self.now_us < ms * 1_000 {
            self.step();
        }
    }

    fn run_for(&mut self, ms: u64) {
        self.run_until(self.now_us / 1_000 + ms);
    }

    /// 送り元`from`の最初のJOINの木のフィールド。
    fn first_join(&self, from: SocketAddr) -> JoinTree {
        let (_, len, bytes) = self
            .wire
            .joins
            .iter()
            .find(|j| j.0 == from)
            .expect("JOIN を送っていない");
        join_tree_of(&bytes[..*len])
    }

    fn watcher(&self, i: usize) -> &Watcher {
        self.nodes[i].recv.watcher_for_test().expect("観戦者")
    }

    /// 中継役`nodes[0]`の受け取りをパニックの後や受信スレッドの停止のときと同じように手放させる。
    fn abandon_relay(&mut self) {
        let mut tx = Tx {
            w: &mut self.wire,
            from: RELAY,
            now_us: self.now_us,
        };
        self.nodes[0].recv.abandon(&mut tx);
    }

    /// ホストから`to`へ1通届ける（次の歩で着く）。
    fn inject_from_host(&mut self, to: SocketAddr, bytes: &[u8]) {
        let mut buf = [0u8; MAX_DATAGRAM];
        buf[..bytes.len()].copy_from_slice(bytes);
        self.wire.q.push(Pkt {
            at_us: self.now_us + 1_000,
            from: HOST,
            to,
            len: bytes.len(),
            bytes: buf,
        });
    }

    /// 中継役の数を本ログの行を作る`WatchStatus`に通した文字列。
    fn relay_line(&self) -> String {
        self.line_of(0)
    }

    /// 観戦者`nodes[i]`の中継の数を本ログの行を作る`WatchStatus`に通した文字列。
    fn line_of(&self, i: usize) -> String {
        let rc = self.nodes[i].feed.relay_counts().expect("中継の数");
        relay_text(&self.nodes[i].slot, &rc)
    }
}

/// 中継の数`rc`を本ログの行を作る`WatchStatus`に通した文字列。
fn relay_text(slot: &Arc<WatchSlot>, rc: &RelayCounts) -> String {
    let wp = WatchPlay::with_feed_len(Arc::clone(slot), watch_cfg(), 8);
    wp.feed().set_code_at(0);
    wp.feed().publish_relay(rc);
    let status = WatchStatus::new();
    status.publish(&wp);
    status.text()
}

/// 中継役の下に子が接続した模型。
fn with_child_under_relay() -> Model {
    let mut m = Model::new(true);
    m.run_until(CHILD_START_MS + 1_500);
    assert!(m.host.is_relay_of(RELAY), "中継役に選ばれていない");
    assert!(
        !m.watcher(1).parent_is_home(),
        "子が中継役の下にいない: {:?}",
        m.watcher(1).tree_counts()
    );
    m
}

#[test]
fn a_relay_capable_watcher_becomes_the_relay_and_its_child_gets_every_record_through_it() {
    let mut m = with_child_under_relay();
    m.push_until_ms = 2_500;
    m.run_until(3_000);
    assert_eq!(m.wire.dropped, 0);
    let t = m.first_join(RELAY);
    assert!(t.relay_public.is_some(), "中継用の公開鍵を載せていない");
    assert_eq!(t.nat, REACHABLE, "回線の型を載せていない");
    assert_eq!(t.candidates.as_slice().len(), 1);
    assert_eq!(t.candidates.as_slice()[0].addr, RELAY);
    assert_eq!(
        m.nodes[1].feed.host_addr(),
        Some(RELAY),
        "子の最初の流れが中継役から来ていない"
    );
    assert_eq!(m.watcher(1).tree_counts().intros_taken, 1);
    let rc = m.nodes[0].feed.relay_counts().expect("中継の数");
    assert!(rc.enabled && rc.failed == 0);
    let side = rc.side.expect("配る側");
    assert_eq!((side.children, side.joined), (1, 1), "{side:?}");
    assert!(side.records_sent > 0, "{side:?}");
    assert_eq!(side.pushed as usize, m.log.len(), "{side:?}");
    assert_eq!(m.nodes[0].got, m.log, "中継役の列とホストの列");
    assert_eq!(m.nodes[1].got, m.log, "子の列とホストの列");
    assert_eq!(m.recv_allocs, 0, "受け取りの周回で確保した");
    let text = m.relay_line();
    for want in [
        "観戦者の中継: 有効（作成不能 0）",
        "配信側: いまの子 1・受け入れた子 1",
    ] {
        assert!(text.contains(want), "{want:?} が無い: {text}");
    }
}

#[test]
fn with_relay_off_the_watcher_is_never_a_relay_and_sends_the_same_join_as_before() {
    let mut m = Model::new(false);
    m.push_until_ms = 2_500;
    m.run_until(3_000);
    assert!(!m.host.is_relay_of(RELAY), "中継しない設定で中継役になった");
    let t = m.first_join(RELAY);
    assert_eq!(t.relay_public, None);
    assert_eq!(
        t.nat,
        NatMark::UNKNOWN,
        "中継しない観戦者が回線の型を載せた"
    );
    assert!(t.candidates.as_slice().is_empty());
    assert_eq!(t.resume_from, None);
    assert_eq!(
        m.nodes[1].feed.host_addr(),
        Some(HOST),
        "子が直接受けていない"
    );
    assert_eq!(m.watcher(1).tree_counts().intros_taken, 0);
    let rc = m.nodes[0].feed.relay_counts().expect("中継の数");
    assert!(!rc.enabled && rc.side.is_none(), "{rc:?}");
    assert_eq!(m.nodes[0].got, m.log);
    assert_eq!(m.nodes[1].got, m.log);
    let text = m.relay_line();
    assert!(
        text.contains("観戦者の中継: 無効（ini の relay = 0）"),
        "{text}"
    );
}

/// ゲームのスレッドが観戦を終えた理由ごとのBYE（`WatchFeed::end`）と、受け取りを手放したとき。
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// 自分で抜けた・続けられない（BYE(取消)）。
    Cancelled,
    /// ホストの終わりの記録（BYE(終わり)）。
    Finished,
    /// ホストが黙った・接続コードの監視の処理が諦めた・受信スレッドの処理が止まった（BYEを頼まない）。
    NoBye,
    /// 受信スレッドの観戦者の処理がパニックした・受信スレッドが止まる。
    Abandoned,
}

fn end_relay(m: &mut Model, e: Ending) {
    match e {
        Ending::Cancelled => m.nodes[0].feed.end(Some(ByeReason::Cancelled)),
        Ending::Finished => m.nodes[0].feed.end(Some(ByeReason::Finished)),
        Ending::NoBye => m.nodes[0].feed.end(None),
        Ending::Abandoned => m.abandon_relay(),
    }
}

#[test]
fn every_way_the_relay_stops_says_bye_to_the_host_first_and_sends_its_child_home() {
    for e in [
        Ending::Cancelled,
        Ending::Finished,
        Ending::NoBye,
        Ending::Abandoned,
    ] {
        let mut m = with_child_under_relay();
        let left = m.host.counts().left;
        let mark = m.wire.order.len();
        end_relay(&mut m, e);
        m.run_for(50);
        assert_eq!(
            m.host.counts().left,
            left + 1,
            "{e:?}: BYE がホストへ届いていない"
        );
        let c = m.watcher(1);
        assert_eq!(
            c.tree_counts().rehomes,
            1,
            "{e:?}: 子が REHOME を受けていない"
        );
        assert!(c.parent_is_home(), "{e:?}: 子が元のホストへ戻っていない");
        assert_eq!(
            m.wire.order[mark..],
            [(HOST, PeekedType::Bye), (CHILD, PeekedType::SpectateRehome)],
            "{e:?}: BYE が先・REHOME が後でない"
        );
        // 子は元のホストから続きを受ける。中継役はもう配布しない。
        let had = m.nodes[1].got.len();
        m.run_for(500);
        assert_eq!(m.watcher(1).host_addr(), Some(HOST), "{e:?}");
        let got = &m.nodes[1].got;
        assert!(got.len() > had + 20, "{e:?}: 子の流れが止まった");
        assert_eq!(got[..], m.log[..got.len()], "{e:?}");
        let side = m.nodes[0].feed.relay_counts().unwrap().side.unwrap();
        assert_eq!(side.rehomes_sent, 1, "{e:?}");
    }
}

#[test]
fn a_child_that_leaves_frees_its_place_on_the_relay_and_the_relay_keeps_watching() {
    let mut m = with_child_under_relay();
    let host_left = m.host.counts().left;
    m.nodes[1].feed.end(Some(ByeReason::Cancelled));
    m.run_for(50);
    let side = m.nodes[0].feed.relay_counts().unwrap().side.unwrap();
    assert_eq!(
        (side.left, side.children),
        (1, 0),
        "子の BYE を配る側が受けていない: {side:?}"
    );
    assert_eq!(m.host.counts().left, host_left, "子の BYE がホストへ行った");
    let w = m.watcher(0);
    assert!(w.parent_is_home() && w.host_addr() == Some(HOST));
    assert_eq!(w.counts().foreign, 0, "子の BYE を観戦者の側が受けた");
    let had = m.nodes[0].got.len();
    m.run_for(200);
    assert!(m.nodes[0].got.len() > had + 5, "中継役の観戦が止まった");
}

#[test]
fn without_relay_only_a_requested_bye_goes_out_as_before() {
    for (e, bye) in [
        (Ending::Cancelled, true),
        (Ending::Finished, true),
        (Ending::NoBye, false),
        (Ending::Abandoned, false),
    ] {
        let mut m = Model::new(false);
        m.run_until(CHILD_START_MS + 500);
        assert_eq!(m.nodes[0].feed.host_addr(), Some(HOST));
        let left = m.host.counts().left;
        end_relay(&mut m, e);
        m.run_for(50);
        assert_eq!(
            m.host.counts().left,
            left + u64::from(bye),
            "{e:?}: 中継しない観戦者の BYE が今までと違う"
        );
        let byes = m
            .wire
            .order
            .iter()
            .filter(|o| o.1 == PeekedType::Bye)
            .count();
        assert_eq!(byes, usize::from(bye), "{e:?}: {:?}", m.wire.order);
        assert!(
            !m.wire
                .order
                .iter()
                .any(|o| o.1 == PeekedType::SpectateRehome),
            "{e:?}: 中継しない観戦者が REHOME を送った"
        );
    }
}

#[test]
fn a_cancel_bye_after_joining_marks_the_feed_refused_and_before_joining_marks_the_slot_full() {
    let mut m = with_child_under_relay();
    let relay_public = m.watcher(0).public_key();
    m.inject_from_host(
        RELAY,
        &from_host(&relay_public, &Message::Bye(ByeReason::Finished)),
    );
    m.run_for(2);
    assert!(
        !m.nodes[0].feed.is_refused(),
        "BYE(終わり) で断りの印を立てた"
    );
    m.inject_from_host(
        RELAY,
        &from_host(&relay_public, &Message::Bye(ByeReason::Cancelled)),
    );
    m.run_for(2);
    assert!(
        m.nodes[0].feed.is_refused(),
        "接続した後の断りを読んでいない"
    );
    assert!(!m.nodes[0].slot.full_before_join());
    assert!(m.nodes[0].feed.relay_counts().unwrap().refused);
    // 接続する前にホストから満員の断りが来たら満員の待ちになる。観戦は続け、時間切れのときに満員と数える。
    let mut n = node(CHILD, false, 0xA3, 0x23, 0);
    let mut sink = Sink::default();
    n.recv.service(1_000, &mut sink);
    let public = n.recv.watcher_for_test().unwrap().public_key();
    n.recv.on_datagram(
        HOST,
        &from_host(&public, &Message::Bye(ByeReason::Finished)),
        2_000,
        &mut sink,
    );
    assert!(!n.slot.full_before_join(), "BYE(終わり) を満員と読んだ");
    n.recv.on_datagram(
        HOST,
        &from_host(&public, &Message::Bye(ByeReason::Cancelled)),
        3_000,
        &mut sink,
    );
    assert!(
        n.slot.full_before_join(),
        "接続する前の満員の断りを読んでいない"
    );
    assert!(
        !n.feed.is_refused(),
        "接続する前の断りで観戦を終える印を立てた"
    );
}

#[test]
fn the_feeds_refused_mark_goes_down_when_the_host_streams_again() {
    let mut m = with_child_under_relay();
    let public = m.watcher(0).public_key();
    m.inject_from_host(
        RELAY,
        &from_host(&public, &Message::Bye(ByeReason::Cancelled)),
    );
    m.run_for(2);
    assert!(m.nodes[0].feed.is_refused(), "断りの印が立たない");
    m.run_for(100);
    assert!(
        !m.nodes[0].feed.is_refused(),
        "ホストの流れが戻ったのに断りの印が残る"
    );
}

#[test]
fn a_new_offer_drops_the_full_before_join_mark_of_the_one_it_replaces() {
    let slot = WatchSlot::new();
    slot.mark_full_before_join();
    slot.offer(watcher_cfg(HOST, 1, 0x31));
    assert!(!slot.full_before_join(), "offer が印を下ろさない");
    // 受信スレッドが古い受け取りのままフラグを立てた後に新しい受け取りへ替えても、フラグは残らない。
    let mut n = node(CHILD, false, 0xA3, 0x23, 0);
    let mut sink = Sink::default();
    n.recv.service(1_000, &mut sink);
    n.slot.offer_with(watcher_cfg(HOST, 0xA4, 0x24), None);
    n.slot.mark_full_before_join();
    n.recv.service(2_000, &mut sink);
    assert!(!n.slot.full_before_join(), "受け取りを替えても印が残る");
}

#[test]
fn a_refused_watch_ends_only_after_the_buffered_records_are_used() {
    let (mut net, _peer, _clock) = FakeNet::pair(HOST, CHILD, 1_000);
    for refused in [false, true] {
        let slot = Arc::new(WatchSlot::new());
        let mut wp = WatchPlay::with_feed_len(Arc::clone(&slot), watch_cfg(), 64);
        wp.feed().set_joined(HOST, 0);
        wp.tend(&mut net);
        assert_eq!(wp.phase(), Phase::Selecting);
        wp.feed().set_refused(refused);
        wp.test_backlog = Some(3);
        wp.tend(&mut net);
        assert_eq!(wp.end(), None, "溜めた記録を使う前に終えた");
        assert!(!slot.refused_full(), "終える前に満員の印を立てた");
        wp.test_backlog = Some(0);
        wp.tend(&mut net);
        assert_eq!(
            slot.refused_full(),
            refused,
            "満員の印（断られた {refused}）"
        );
        if refused {
            assert_eq!(wp.end(), Some(WatchEnd::HostEnded(ByeReason::Cancelled)));
            assert_eq!(
                wp.feed().end_request(),
                Some(Some(ByeReason::Finished)),
                "受信スレッドに終わりを頼んでいない"
            );
        } else {
            assert_eq!(wp.end(), None, "断られていないのに終えた");
        }
    }
}

#[test]
fn the_slot_makes_a_relay_side_only_when_relay_is_enabled_and_a_key_seed_was_drawn() {
    let cfg = watcher_cfg(HOST, 1, 0x31);
    let join_of = |o: &mut Offered| {
        let mut sink = Sink::default();
        o.watcher.poll(0, &mut sink);
        join_tree_of(&sink.0.first().expect("JOIN").1)
    };
    // 中継しない設定。
    let slot = WatchSlot::new();
    slot.offer(cfg);
    let mut o = slot.take_newer(&mut 0).unwrap();
    assert!(o.relay.is_none() && !slot.relay_enabled());
    assert_eq!(join_of(&mut o).relay_public, None);
    // 中継する設定のときOSの乱数で種を生成する。
    let slot = WatchSlot::new();
    slot.enable_relay(Candidates::from_addrs(&[RELAY]).unwrap());
    slot.offer(cfg);
    let mut o = slot.take_newer(&mut 0).unwrap();
    assert!(o.relay.as_deref().expect("配る側").is_relay());
    let t = join_of(&mut o);
    assert!(t.relay_public.is_some());
    assert_eq!(t.candidates.as_slice()[0].addr, RELAY);
    assert_eq!(slot.relay_failed(), 0);
    // 種を生成できなければ中継しない（観戦は続ける）。
    slot.offer_with(cfg, Some(Err("乱数を引けない".to_string())));
    let mut o = slot.take_newer(&mut 1).unwrap();
    assert!(o.relay.is_none());
    assert_eq!(join_of(&mut o).relay_public, None);
    assert_eq!(slot.relay_failed(), 1);
}

// ---------------- 照合のずれで戻る・UPnPの知らせ ----------------

/// 中継役の上りが分かる前に子（`nodes[1]`）と葉（`nodes[2]`）が直接に入り、マッチの切れ目で子だけが中継役の下へ移った模型。
/// 戻り値の2つ目は移った時点の子のack（前の親から連続で受けた数）。
fn with_child_swapped() -> (Model, u32) {
    let mut m = Model::new(true);
    m.uplink_from_ms = 1_500;
    m.nodes.push(node(LEAF, false, 0xA3, 0x23, 600_000));
    m.push_until_ms = 2_000;
    m.run_until(2_000);
    assert_eq!(
        m.watcher(1).epoch(),
        0,
        "子が切れ目の前に中継役の下へ回った"
    );
    let seg = Record::Seg { seg: 1 };
    m.host.push(seg).unwrap();
    m.log.push(seg);
    while m.watcher(1).epoch() == 0 {
        assert!(
            m.now_us < 4_000_000,
            "子が中継役の下へ移らない: {:?}",
            m.host.counts()
        );
        m.step();
    }
    let ack = m.watcher(1).ack();
    m.run_until(4_000);
    assert_eq!(m.host.counts().swaps, 1, "{:?}", m.host.counts());
    assert!(
        !m.watcher(1).parent_is_home(),
        "子が中継役の下にいない: {:?}",
        m.watcher(1).tree_counts()
    );
    assert!(m.watcher(2).parent_is_home(), "葉まで移った");
    m.push_until_ms = u64::MAX;
    m.run_for(500);
    assert!(
        m.nodes[1].got.len() > m.log.len() - 20,
        "中継役から続きが来ない"
    );
    (m, ack)
}

#[test]
fn a_mismatch_returns_home_only_for_a_record_from_the_current_relay_and_an_unbroken_pause() {
    let (mut m, ack) = with_child_swapped();
    let from = m.nodes[1].recv.parent_from_for_test();
    assert_eq!(
        from, ack,
        "いまの親の記録の始まりが、前の親から連続で受けた数と違う"
    );
    let left = m.nodes[0].feed.relay_counts().unwrap().side.unwrap().left;
    let ask = |m: &mut Model, pos: u32, broken: bool| {
        m.nodes[1].feed.request_mismatch(pos, broken);
        m.run_for(5);
    };
    // 前の親（元のホスト）が運んだ記録のずれ。
    ask(&mut m, from - 1, false);
    // そのマッチでポーズが崩れていた。
    ask(&mut m, from, true);
    assert!(!m.watcher(1).parent_is_home(), "戻らないはずのずれで戻った");
    let rc = m.nodes[1].feed.relay_counts().unwrap();
    assert_eq!((rc.tree.mismatch_returns, rc.mismatch_stays), (0, 2));
    // いまの親が運んだ記録のずれ。
    ask(&mut m, from, false);
    let w = m.watcher(1);
    assert!(
        w.parent_is_home() && w.direct_only(),
        "{:?}",
        w.tree_counts()
    );
    let rc = m.nodes[1].feed.relay_counts().unwrap();
    assert_eq!((rc.tree.mismatch_returns, rc.mismatch_stays), (1, 2));
    let side = m.nodes[0].feed.relay_counts().unwrap().side.unwrap();
    assert_eq!(side.left, left + 1, "中継役へ BYE を送っていない");
    // 親が元のホストになった後のずれは数えない。
    ask(&mut m, from + 50, false);
    assert_eq!(m.nodes[1].feed.relay_counts().unwrap().mismatch_stays, 2);
    let had = m.nodes[1].got.len();
    m.run_for(1_000);
    assert_eq!(m.watcher(1).host_addr(), Some(HOST));
    let got = &m.nodes[1].got;
    assert!(got.len() > had + 20, "戻った後の流れが止まった");
    assert_eq!(got[..], m.log[..got.len()], "戻った後の列に抜けか重なり");
    assert_eq!(m.recv_allocs, 0, "受け取りの周回で確保した");
    let text = m.line_of(1);
    assert!(
        text.contains(" / 照合のずれで中継役の下から戻った 1・戻らなかった 2"),
        "{text}"
    );
}

fn stream_msg(first: u32, n: u32) -> Message {
    let mut records = [Record::FILLER; wire::MAX_STREAM_RECORDS];
    for (k, r) in records.iter_mut().take(n as usize).enumerate() {
        *r = frame(first + k as u32);
    }
    Message::SpectateStream(SpectateStream {
        first,
        reset: first == 0,
        cookie: None,
        count: n as u8,
        records,
    })
}

/// ホストが中継役を紹介した時点で組み立てがackより先に持っていた飛び飛びの記録は、前の親の分に数える。
/// 後でいまの親が抜けを埋めて連続になっても同じ（その間の抜けも前の親の分）。
/// 戻るのはいまの親がその先に運んだ記録にずれがあったときだけ。
#[test]
fn records_held_beyond_the_ack_at_the_intro_count_as_the_previous_parents() {
    let slot = Arc::new(WatchSlot::new());
    slot.offer_with(watcher_cfg(HOST, 0xC1, 0x51), None);
    let feed = Arc::new(WatchFeed::with_len(4_096));
    let mut recv = WatchRecv::new(Arc::clone(&slot), Arc::clone(&feed), FF_UPDATES_DEFAULT);
    let mut sink = Sink::default();
    let mut t_us = 1_000;
    recv.service(t_us, &mut sink);
    let public = recv.watcher_for_test().expect("観戦者").public_key();
    let relay_seed = KeySeed([0x5C; 32]);
    let (host_seq, relay_seq) = (Seq::new(), Seq::new());
    let mut deliver = |recv: &mut WatchRecv, from: SocketAddr, msg: &Message, sink: &mut Sink| {
        let (seed, seq) = if from == HOST {
            (&HOST_SEED, &host_seq)
        } else {
            (&relay_seed, &relay_seq)
        };
        t_us += 1_000;
        recv.on_datagram(from, &sealed_by(seed, &public, seq, msg), t_us, sink);
        recv.service(t_us, sink);
    };
    deliver(&mut recv, HOST, &stream_msg(0, 16), &mut sink);
    deliver(&mut recv, HOST, &stream_msg(16, 4), &mut sink);
    // 20〜23が抜けて、24〜29が先に着いた。
    deliver(&mut recv, HOST, &stream_msg(24, 6), &mut sink);
    let w = recv.watcher_for_test().unwrap();
    assert_eq!((w.ack(), w.stored_end(), feed.written_pos()), (20, 30, 20));
    let intro = Message::SpectateIntro(SpectateIntro {
        nonce: 0xC1,
        epoch: 0,
        relay_public: Ephemeral::from_seed(&relay_seed).public(),
        relay: Candidates::from_addrs(&[RELAY]).unwrap(),
    });
    deliver(&mut recv, HOST, &intro, &mut sink);
    assert_eq!(
        recv.watcher_for_test().unwrap().epoch(),
        1,
        "紹介を受けていない"
    );
    let from = recv.parent_from_for_test();
    deliver(&mut recv, RELAY, &stream_msg(20, 4), &mut sink);
    deliver(&mut recv, RELAY, &stream_msg(30, 4), &mut sink);
    assert_eq!(feed.written_pos(), 34, "中継役からの記録が積まれていない");
    let mut ask = |recv: &mut WatchRecv, pos: u32| {
        feed.request_mismatch(pos, false);
        t_us += 1_000;
        recv.service(t_us, &mut sink);
    };
    // 前の親が先に運んだ記録・いまの親が埋めた抜け。
    for pos in [27, 21] {
        ask(&mut recv, pos);
        assert!(
            !recv.watcher_for_test().unwrap().parent_is_home(),
            "位置 {pos} のずれで戻った"
        );
    }
    ask(&mut recv, 31);
    let w = recv.watcher_for_test().unwrap();
    assert!(
        w.parent_is_home() && w.direct_only(),
        "{:?}",
        w.tree_counts()
    );
    let rc = feed.relay_counts().unwrap();
    assert_eq!((rc.tree.mismatch_returns, rc.mismatch_stays), (1, 2));
    assert!(
        sink.0
            .iter()
            .any(|(to, b)| *to == RELAY && wire::peek_type(b) == PeekedType::Bye),
        "中継役へ BYE を送っていない"
    );
    assert_eq!(from, 30, "いまの親の記録の始まり");
}

#[test]
fn the_mismatch_line_is_left_out_while_there_is_no_mismatch() {
    let (m, _) = with_child_swapped();
    let text = m.line_of(1);
    assert!(!text.contains("照合のずれ"), "{text}");
    assert!(!text.contains("UPnP"), "{text}");
}

/// 中継役（`nodes[0]`）に足したUPnPの候補。
fn upnp_extra() -> SocketAddr {
    SocketAddr::from(([198, 51, 100, 7], 47_301))
}

fn upnp_own() -> Candidates {
    Candidates::from_addrs(&[RELAY, upnp_extra()]).unwrap()
}

/// 中継役の候補（`nodes[0]`）だけが動く模型（子は動かさない）。
fn relay_alone() -> Model {
    let mut m = Model::new(true);
    m.nodes[1].start_us = u64::MAX;
    m.run_until(500);
    assert!(m.watcher(0).is_joined());
    m
}

/// `mark`の後に中継役がホストへ送ったJOINの時刻（µs）。
fn joins_after(m: &Model, mark: usize) -> Vec<u64> {
    m.wire.join_log[mark..]
        .iter()
        .filter(|j| j.0 == RELAY)
        .map(|j| j.1)
        .collect()
}

#[test]
fn a_upnp_mapping_after_joining_is_announced_join_resend_ms_apart_and_the_host_reads_it() {
    let mut m = relay_alone();
    let mark = m.wire.join_log.len();
    m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
    m.run_for(2_000);
    let sent = joins_after(&m, mark);
    assert_eq!(
        sent.len(),
        usize::from(super::recv::ANNOUNCE_ROUNDS),
        "{sent:?}"
    );
    for w in sent.windows(2) {
        assert_eq!(w[1] - w[0], u64::from(JOIN_RESEND_MS) * 1_000, "{sent:?}");
    }
    let t = m.host.tree_of(RELAY).expect("ホストの枠").join;
    assert!(
        t.candidates.contains(upnp_extra()),
        "ホストが候補を読み直していない"
    );
    let rc = m.nodes[0].feed.relay_counts().unwrap();
    assert_eq!(
        (rc.upnp, rc.announces_sent, rc.announces_skipped),
        (WatchUpnp::Mapped, 4, 0)
    );
    assert_eq!(
        m.nodes[0].got,
        m.log[..m.nodes[0].got.len()],
        "知らせで流れが飛んだ"
    );
    assert_eq!(m.recv_allocs, 0, "受け取りの周回で確保した");
    let text = m.relay_line();
    assert!(
        text.contains(
            " / UPnP: 割り当て済み（候補に追加） / ホストへ知らせの JOIN 送った 4・送らなかった 0"
        ),
        "{text}"
    );
}

#[test]
fn a_nat_type_learned_after_joining_is_announced_too() {
    let mut m = relay_alone();
    let mark = m.wire.join_log.len();
    let later = NatMark {
        symmetry: Symmetry::Yes,
        upnp: false,
    };
    m.nodes[0].recv.note_nat(later);
    m.run_for(2_000);
    assert_eq!(joins_after(&m, mark).len(), 4);
    assert_eq!(m.host.tree_of(RELAY).expect("ホストの枠").join.nat, later);
    // 同じ型を毎周渡しても送らない。
    let mark = m.wire.join_log.len();
    m.nodes[0].recv.note_nat(later);
    m.run_for(1_000);
    assert!(joins_after(&m, mark).is_empty(), "変わらない型で知らせた");
}

#[test]
fn a_relay_with_a_child_or_one_that_had_children_does_not_announce() {
    let mut m = with_child_under_relay();
    let mark = m.wire.join_log.len();
    m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
    m.run_for(1_000);
    assert!(joins_after(&m, mark).is_empty(), "子がいるのに知らせた");
    let rc = m.nodes[0].feed.relay_counts().unwrap();
    assert_eq!((rc.announces_sent, rc.announces_skipped), (0, 1));
    // 中継役になった後は子が抜けても知らせない。
    m.nodes[1].feed.end(Some(ByeReason::Cancelled));
    m.run_for(100);
    assert_eq!(
        m.nodes[0]
            .feed
            .relay_counts()
            .unwrap()
            .side
            .unwrap()
            .children,
        0
    );
    m.nodes[0].recv.note_nat(NatMark {
        symmetry: Symmetry::No,
        upnp: true,
    });
    m.run_for(1_000);
    assert!(
        joins_after(&m, mark).is_empty(),
        "中継役になった後に知らせた"
    );
    assert_eq!(m.nodes[0].feed.relay_counts().unwrap().announces_skipped, 2);
}

/// 受け取りの最初のJOINが割り当てを載せる（受信スレッドが取る前に割り当てが出た・接続する前に受け取りを替えた）なら知らせない。
#[test]
fn a_upnp_mapping_that_a_watchers_first_join_carries_is_not_announced() {
    // 受け取りを受信スレッドが取る前。
    let mut m = Model::new(true);
    m.nodes[1].start_us = u64::MAX;
    m.nodes[0].recv.note_upnp(UpnpNews::Asked);
    m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
    m.run_until(1_500);
    assert!(m.first_join(RELAY).candidates.contains(upnp_extra()));
    let rc = m.nodes[0].feed.relay_counts().unwrap();
    assert_eq!((rc.announces_sent, rc.announces_skipped), (0, 0));
    assert_eq!(rc.upnp, WatchUpnp::Mapped);
    // 最初の受け取りのJOINの後、接続する前に受け取りを替えた。
    let mut m = Model::new(true);
    m.nodes[1].start_us = u64::MAX;
    m.host_from_ms = 300;
    m.run_until(1);
    m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
    m.nodes[0].slot.offer_with(
        watcher_cfg(HOST, 0xA9, 0x29),
        Some(Ok(KeySeed([0x29 ^ 0x5A; 32]))),
    );
    m.run_until(2_000);
    assert!(m.watcher(0).is_joined());
    assert!(!m.first_join(RELAY).candidates.contains(upnp_extra()));
    let t = m.host.tree_of(RELAY).expect("ホストの枠").join;
    assert!(
        t.candidates.contains(upnp_extra()),
        "替えた受け取りの JOIN が載せていない"
    );
    let rc = m.nodes[0].feed.relay_counts().unwrap();
    assert_eq!(
        (rc.announces_sent, rc.announces_skipped),
        (0, 0),
        "受け取りの最初の JOIN が載せたのに知らせた"
    );
}

/// 最初のJOINの後に割り当てが出たら接続した後に知らせる。
/// 接続する前に再送したJOINが割り当てを載せていても同じ。
/// そのJOINが失われてホストが前のJOINで受け入れていることがあるため。
#[test]
fn a_upnp_mapping_after_the_first_join_is_announced_after_joining_even_if_a_resent_join_carried_it()
{
    for host_from_ms in [0, 300] {
        let mut m = Model::new(true);
        m.nodes[1].start_us = u64::MAX;
        m.host_from_ms = host_from_ms;
        m.run_until(1);
        assert!(!m.watcher(0).is_joined());
        m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
        // 接続した歩より前に送ったJOINの数（接続した歩では送り直さない）。
        let mut before = 0;
        while !m.watcher(0).is_joined() {
            assert!(m.now_us < 2_000_000, "接続しない");
            before = m.watcher(0).counts().joins_sent;
            m.step();
        }
        if host_from_ms == 0 {
            assert_eq!(before, 1, "送り直しの JOIN が出た");
        } else {
            assert!(before > 1, "送り直しの JOIN が出ていない");
        }
        m.run_until(2_000);
        assert!(!m.first_join(RELAY).candidates.contains(upnp_extra()));
        assert_eq!(m.watcher(0).counts().joins_sent, before + 4);
        let t = m.host.tree_of(RELAY).expect("ホストの枠").join;
        assert!(
            t.candidates.contains(upnp_extra()),
            "ホストが候補を知らない"
        );
        let rc = m.nodes[0].feed.relay_counts().unwrap();
        assert_eq!(
            (rc.announces_sent, rc.announces_skipped),
            (4, 0),
            "ホストが受け始める時刻 {host_from_ms} ms"
        );
    }
}

/// 中継する設定で配布する側を作れなかった受け取り: 接続するまでは割り当てを使うかどうか決めない。接続したら通信層へ割り当ての取り下げを1回だけ頼む。
#[test]
fn a_watcher_whose_relay_side_failed_asks_once_to_withdraw_the_mapping_after_joining() {
    let mut m = Model::new(true);
    m.nodes[1].start_us = u64::MAX;
    m.host_from_ms = 300;
    m.nodes[0].slot.offer_with(
        watcher_cfg(HOST, 0xA1, 0x21),
        Some(Err("乱数を引けない".to_string())),
    );
    m.run_until(1);
    m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
    m.run_for(10);
    assert!(!m.watcher(0).is_joined());
    assert_eq!(
        m.nodes[0].feed.relay_counts().unwrap().upnp,
        WatchUpnp::Waiting
    );
    assert!(!m.nodes[0].recv.take_upnp_drop(), "接続する前に取り下げた");
    m.run_until(1_000);
    assert!(m.watcher(0).is_joined());
    let rc = m.nodes[0].feed.relay_counts().unwrap();
    assert_eq!((rc.failed, rc.upnp), (1, WatchUpnp::NoRelay));
    assert!(m.nodes[0].recv.take_upnp_drop(), "取り下げを頼んでいない");
    m.run_for(100);
    assert!(!m.nodes[0].recv.take_upnp_drop(), "取り下げを 2 回頼んだ");
    let t = m.host.tree_of(RELAY).expect("ホストの枠").join;
    assert!(!t.candidates.contains(upnp_extra()));
    assert!(
        m.relay_line()
            .contains(" / UPnP: 中継の配信側なし（取り下げ）")
    );
}

#[test]
fn an_unused_upnp_result_or_a_watcher_without_relay_changes_nothing_but_the_log() {
    let mut m = relay_alone();
    m.nodes[0].recv.note_upnp(UpnpNews::Asked);
    m.run_for(10);
    assert!(
        m.relay_line()
            .contains(" / UPnP: 結果待ち / ホストへ知らせの JOIN 送った 0・送らなかった 0")
    );
    let mark = m.wire.join_log.len();
    m.nodes[0]
        .recv
        .note_upnp(UpnpNews::Unused(WatchUpnp::Behind));
    m.run_for(1_000);
    assert!(joins_after(&m, mark).is_empty());
    let t = m.host.tree_of(RELAY).expect("ホストの枠").join;
    assert!(!t.candidates.contains(upnp_extra()));
    assert!(
        m.relay_line()
            .contains(" / UPnP: 外側のアドレスが STUN と不一致（取り下げ） / ホストへ知らせの JOIN 送った 0・送らなかった 0")
    );
    // 中継しない観戦者は割り当てが来ても候補を載せず知らせない。
    let mut m = Model::new(false);
    m.nodes[1].start_us = u64::MAX;
    m.run_until(500);
    let mark = m.wire.join_log.len();
    m.nodes[0].recv.note_upnp(UpnpNews::Mapped(upnp_own()));
    m.run_for(1_000);
    assert!(
        joins_after(&m, mark).is_empty(),
        "中継しない観戦者が知らせた"
    );
    assert!(m.first_join(RELAY).candidates.as_slice().is_empty());
}

// ---------------- 本物の通信層（loopback） ----------------

struct UdpOut<'a>(&'a UdpSocket);

impl Outlet for UdpOut<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let _ = self.0.send_to(bytes, to);
    }
}

fn bind_lo() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    s.set_read_timeout(Some(Duration::from_millis(1)))
        .expect("待ちの上限を置ける");
    s
}

/// 本物の受信スレッドの中継役（`GameNet` ＋ `WatchPlay`）と、検査のスレッドで回すホスト（coreの`SpectateHost`）と子（coreの`Watcher`）。
struct Rig {
    hsock: UdpSocket,
    csock: UdpSocket,
    raddr: SocketAddr,
    host: SpectateHost,
    child: Watcher,
    wp: WatchPlay,
    net: GameNet,
    t0: Instant,
    pushed: u32,
    got: usize,
}

impl Rig {
    fn new() -> (Rig, RecvThread) {
        Self::new_with(true)
    }

    /// `relay_side`: 中継の配布する側を作れるかどうか（falseは鍵の種を生成できなかった状態）。
    fn new_with(relay_side: bool) -> (Rig, RecvThread) {
        let hsock = bind_lo();
        let csock = bind_lo();
        let haddr = hsock.local_addr().unwrap();
        let (net, rx) =
            net_udp::open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).expect("通信層を開ける");
        // loopbackでは2か所目のSTUNを聞かないので届く側のネットワーク種別を設定する。
        net.set_second_stun_for_test(SecondStun::Compared(higanr_core::stun::Mapping::Same));
        let raddr = net.local_addr().unwrap();
        let slot = Arc::new(WatchSlot::new());
        slot.enable_relay(Candidates::from_addrs(&[raddr]).unwrap());
        if relay_side {
            slot.offer(watcher_cfg(haddr, 0xB1, 0x41));
        } else {
            slot.offer_with(
                watcher_cfg(haddr, 0xB1, 0x41),
                Some(Err("乱数を引けない".to_string())),
            );
        }
        let wp = WatchPlay::with_feed_len(slot, watch_cfg(), 4_096);
        let mut host = SpectateHost::with_capacity(host_cfg(), 1 << 14);
        host.push(Record::Seg { seg: 0 }).unwrap();
        let rig = Rig {
            hsock,
            csock,
            raddr,
            host,
            child: Watcher::new(watcher_cfg(haddr, 0xB2, 0x42)),
            wp,
            net,
            t0: Instant::now(),
            pushed: 1,
            got: 0,
        };
        (rig, rx)
    }

    /// 1周: ゲームのスレッドの手入れ・ホストが受けて送る・（`with_child`なら）子が受けて送る。
    fn round(&mut self, with_child: bool) {
        let now = self.t0.elapsed().as_millis() as u32;
        self.wp.tend(&mut self.net);
        if now / 16 >= self.pushed {
            self.host.push(frame(self.pushed)).unwrap();
            self.pushed += 1;
        }
        self.host.note_uplink(self.raddr, THICK);
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = self.hsock.recv_from(&mut buf) {
            self.host
                .on_datagram(from, &buf[..n], now, &mut UdpOut(&self.hsock));
        }
        self.host.poll(now, &mut UdpOut(&self.hsock));
        if with_child {
            while let Ok((n, from)) = self.csock.recv_from(&mut buf) {
                self.child
                    .on_datagram(from, &buf[..n], now, &mut UdpOut(&self.csock));
            }
            self.child.poll(now, &mut UdpOut(&self.csock));
            while self.child.pop_indexed().is_some() {
                self.got += 1;
            }
        }
    }

    /// 中継役が接続して子が中継役から20記録受けるまで回す。
    fn attach_child(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.wp.is_joined() {
            assert!(Instant::now() < deadline, "中継役が接続しない");
            self.round(false);
        }
        while !(self.child.host_addr() == Some(self.raddr) && self.got >= 20) {
            assert!(
                Instant::now() < deadline,
                "子が中継役から受けない: {:?} / {:?}",
                self.child.tree_counts(),
                self.wp.relay_counts()
            );
            self.round(true);
        }
        assert!(self.host.is_relay_of(self.raddr));
    }

    /// 中継役がBYEを送り、子がREHOMEを受けて元のホストへ戻るまで回す。
    fn wait_child_home(&mut self, left: u64, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !(self.host.counts().left > left && self.child.tree_counts().rehomes == 1) {
            assert!(
                Instant::now() < deadline,
                "{what}: ホストが受けた BYE {left} → {}・子が受けた REHOME {}",
                self.host.counts().left,
                self.child.tree_counts().rehomes
            );
            self.round(true);
        }
        assert!(self.child.parent_is_home(), "{what}");
    }
}

#[test]
fn a_panic_in_the_receive_thread_sends_the_relays_child_home() {
    let (mut r, rx) = Rig::new();
    let switch = Arc::clone(
        &r.wp
            .recv_for_test()
            .expect("渡す前の受け取り")
            .panic_on_service,
    );
    r.attach_child();
    let left = r.host.counts().left;
    switch.store(true, Ordering::Relaxed);
    r.wait_child_home(left, "パニック");
    assert!(rx.stats().watch.stopped, "パニックの印が立っていない");
    r.wp.tend(&mut r.net);
    assert_eq!(r.wp.end(), Some(WatchEnd::Broken));
}

#[test]
fn stopping_the_receive_thread_sends_the_relays_child_home() {
    let (mut r, rx) = Rig::new();
    r.attach_child();
    let left = r.host.counts().left;
    let stats = rx.stop();
    assert!(!stats.watch.stopped, "止めただけなのにパニックの印");
    r.wait_child_home(left, "受信スレッドの止まり");
}

/// 本物の受信スレッドの中継役が開くときに頼んだUPnPの結果を接続した後に読み取り、STUNと突き合わせる。
/// `outer`はルーターが答える外側のアドレス。
/// `relay_side`は`Rig::new_with`と同じ。
/// 戻り値は（Rig, 受信スレッド, 偽のルーターの数）。
fn rig_with_upnp(
    outer: std::net::Ipv4Addr,
    relay_side: bool,
) -> (Rig, RecvThread, crate::net_udp::nat_tests::Counts) {
    use crate::net_udp::nat_tests::{Mode, held, home};
    let (mut r, rx) = Rig::new_with(relay_side);
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (wire, counts) = held(Mode::Maps, Some(outer), &hold);
    let mut note = String::new();
    assert!(net_udp::upnp_begin_with(
        &r.net,
        wire,
        home(47_301),
        vec![47_301],
        crate::upnp::RENEW_EVERY,
        &mut note
    ));
    assert!(r.net.plan_watch_upnp(net_udp::WatchUpnpPlan::new(
        crate::net_config::OwnCandidates::All,
        r.raddr,
        Some(SocketAddr::from(([8, 8, 4, 4], 50_000))),
        None,
    )));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !r.wp.is_joined() {
        assert!(Instant::now() < deadline, "中継役が接続しない");
        r.round(false);
    }
    r.round(false);
    assert_eq!(
        r.wp.relay_counts().map(|c| c.upnp),
        Some(WatchUpnp::Waiting),
        "接続を UPnP の結果で待った"
    );
    hold.store(false, Ordering::Relaxed);
    (r, rx, counts)
}

#[test]
fn a_matching_upnp_mapping_reaches_the_host_through_an_announce_after_joining() {
    let (mut r, rx, counts) = rig_with_upnp(crate::net_udp::nat_tests::PUBLIC, true);
    let extra = SocketAddr::from(([8, 8, 4, 4], 47_301));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !r
        .host
        .tree_of(r.raddr)
        .is_some_and(|v| v.join.candidates.contains(extra) && v.join.nat.upnp)
    {
        assert!(
            Instant::now() < deadline,
            "ホストが UPnP の候補を知らない: {:?}",
            r.wp.relay_counts()
        );
        r.round(false);
    }
    let rc = r.wp.relay_counts().unwrap();
    assert_eq!(rc.upnp, WatchUpnp::Mapped);
    assert!(
        rc.announces_sent >= 1 && rc.announces_skipped == 0,
        "{rc:?}"
    );
    assert!(crate::netplay::Net::own_nat(&mut r.net).upnp);
    let stats = rx.stop();
    assert_eq!(stats.diag.upnp.state, crate::upnp::State::Mapped);
    assert_eq!(
        counts.deletes.load(Ordering::Relaxed),
        1,
        "閉じても消えない"
    );
}

#[test]
fn a_upnp_mapping_behind_another_nat_is_withdrawn_and_never_announced() {
    let (mut r, rx, counts) = rig_with_upnp(std::net::Ipv4Addr::new(1, 1, 1, 1), true);
    let deadline = Instant::now() + Duration::from_secs(10);
    while r.wp.relay_counts().map(|c| c.upnp) != Some(WatchUpnp::Behind) {
        assert!(Instant::now() < deadline, "{:?}", r.wp.relay_counts());
        r.round(false);
    }
    while counts.deletes.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "合わない割り当てを消さない");
        r.round(false);
    }
    for _ in 0..200 {
        r.round(false);
    }
    let rc = r.wp.relay_counts().unwrap();
    assert_eq!((rc.announces_sent, rc.announces_skipped), (0, 0), "{rc:?}");
    let t = r.host.tree_of(r.raddr).expect("ホストの枠").join;
    assert!(
        !t.nat.upnp
            && !t
                .candidates
                .contains(SocketAddr::from(([8, 8, 4, 4], 47_301)))
    );
    assert!(!crate::netplay::Net::own_nat(&mut r.net).upnp);
    assert_eq!(rx.stop().diag.upnp.state, crate::upnp::State::OuterBehind);
}

#[test]
fn a_upnp_mapping_for_a_watcher_without_a_relay_side_is_withdrawn_once_it_joins() {
    let (mut r, rx, counts) = rig_with_upnp(crate::net_udp::nat_tests::PUBLIC, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while r.wp.relay_counts().map(|c| c.upnp) != Some(WatchUpnp::NoRelay) {
        assert!(Instant::now() < deadline, "{:?}", r.wp.relay_counts());
        r.round(false);
    }
    while counts.deletes.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "使わない割り当てを消さない");
        r.round(false);
    }
    for _ in 0..200 {
        r.round(false);
    }
    let rc = r.wp.relay_counts().unwrap();
    assert_eq!(
        (rc.failed, rc.announces_sent, rc.announces_skipped),
        (1, 0, 0),
        "{rc:?}"
    );
    assert!(!crate::netplay::Net::own_nat(&mut r.net).upnp);
    let line = relay_text(&Arc::new(WatchSlot::new()), &rc);
    assert!(
        line.contains(" / UPnP: 中継の配信側なし（取り下げ）"),
        "{line}"
    );
    assert!(!line.contains("割り当て済み（候補に追加）"), "{line}");
    assert_eq!(rx.stop().diag.upnp.state, crate::upnp::State::NoRelay);
    assert_eq!(counts.deletes.load(Ordering::Relaxed), 1, "二重に消した");
}
