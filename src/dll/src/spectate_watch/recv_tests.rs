//! 受信スレッドの受け取り（`recv.rs`・`feed.rs`）の組み上がりの検査: 仮の時計の模型（core の `SpectateHost` ＋ 仮のネットワーク ＋
//! `WatchPlay`。受信スレッドの代わりは `FakeNet::run_watch`）と、本物の通信層（`net_udp`。loopback）。
use std::cell::Cell;
use std::net::{SocketAddr, UdpSocket};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::seal::KeySeed;
use higanr_core::spectate::{HostConfig, SpectateHost, WatcherConfig};
use higanr_core::spectate_pace::FF_UPDATES_DEFAULT;
use higanr_core::wire::{MAX_DATAGRAM, Record, Seat};

use super::*;
use crate::net_udp::{self, GameNet, RecvThread};
use crate::netplay::fake::FakeNet;
use crate::netplay::{Action, BadValue, Game, Net, Phase, SCENE_MATCH};

const SESSION: u32 = 0x5EC7_0A3A;
const SECRET: Secret = Secret(*b"feed-model-0001!");
const HOST_SEED: KeySeed = KeySeed([0x61; 32]);
const WATCHER_SEED: KeySeed = KeySeed([0x62; 32]);
/// 模型の 1 歩（µs）。仮のネットワークの片道は 1 ms なので、ACK は 2 歩でホストへ戻る。
const STEP_US: u64 = 4_000;
const FRAME_US: u64 = 16_667;

fn watch_cfg(wait_budget_us: u32) -> WatchConfig {
    WatchConfig {
        wait_budget_us,
        margin_frames: WATCH_MARGIN_FRAMES,
        require_net_selection: false,
        ff_max: FF_UPDATES_DEFAULT,
    }
}

fn watcher_cfg(host: SocketAddr, host_public: [u8; 32]) -> WatcherConfig {
    WatcherConfig {
        session_id: SESSION,
        secret: SECRET,
        nonce: 7,
        name: Name::new("w"),
        host: Candidates::from_addrs(&[host]).unwrap(),
        host_public,
        key_seed: WATCHER_SEED,
    }
}

fn host_cfg() -> HostConfig {
    HostConfig {
        session_id: SESSION,
        secret: SECRET,
        key_seed: HOST_SEED,
    }
}

fn frame(n: u32) -> Record {
    Record::Frame {
        frame: n,
        p1: n as u16,
        p2: !(n as u16),
    }
}

/// マッチの中の偽のゲーム（場面 2・次の要求 0）。
struct InMatch;

impl Game for InMatch {
    fn scenes(&mut self) -> (u32, u32) {
        (SCENE_MATCH, 0)
    }
    fn local_input(&mut self, _s: Seat) -> u16 {
        0
    }
    fn feed(&mut self, _i: u32, _w: u16) {}
    fn own_select(
        &mut self,
        _s: Seat,
        _r: higanr_core::lockstep::Role,
    ) -> higanr_core::wire::Select {
        unreachable!()
    }
    fn apply_agreed(
        &mut self,
        _p: Seat,
        _a: &higanr_core::selection::Agreed,
    ) -> Result<(), BadValue> {
        unreachable!()
    }
    fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
        true
    }
    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        (0, 0, 0, 0)
    }
}

/// 仮の時計の模型。観戦者のゲームのスレッド（`WatchPlay` の tick）は検査が回したときだけ動き、受信スレッドの代わりは歩ごとに回る。
struct Model {
    host: SpectateHost,
    hnet: FakeNet,
    wnet: FakeNet,
    clock: Rc<Cell<u64>>,
    slot: Arc<WatchSlot>,
    wp: WatchPlay,
    pushed: Vec<Record>,
}

impl Model {
    fn new(feed_len: usize, host_cap: usize, wait_budget_us: u32) -> Model {
        let ha = SocketAddr::from(([127, 0, 0, 1], 47_101));
        let wa = SocketAddr::from(([127, 0, 0, 1], 41_101));
        let (hnet, mut wnet, clock) = FakeNet::pair(ha, wa, 1_000);
        let host = SpectateHost::with_capacity(host_cfg(), host_cap);
        let slot = Arc::new(WatchSlot::new());
        slot.offer(watcher_cfg(ha, host.public_key()));
        let mut wp =
            WatchPlay::with_feed_len(Arc::clone(&slot), watch_cfg(wait_budget_us), feed_len);
        // 最初の手入れが受け取りを受信スレッド（の代わり）へ渡す。
        wp.tend(&mut wnet);
        Model {
            host,
            hnet,
            wnet,
            clock,
            slot,
            wp,
            pushed: Vec::new(),
        }
    }

    fn push(&mut self, r: Record) {
        self.host.push(r).expect("ホストが積める");
        self.pushed.push(r);
    }

    /// 1 歩: 時計を進め、ホストが受けて送り（`host_on` の間）、受信スレッドの代わりが回る。
    fn step(&mut self, host_on: bool) {
        self.clock.set(self.clock.get() + STEP_US);
        if host_on {
            let now = (self.clock.get() / 1_000) as u32;
            let mut buf = [0u8; MAX_DATAGRAM];
            while let Some((from, n, _)) = self.hnet.recv(&mut buf) {
                self.host
                    .on_datagram(from, buf.get(..n).unwrap_or(&[]), now, &mut self.hnet);
            }
            self.host.poll(now, &mut self.hnet);
        }
        self.wnet.run_watch();
    }

    /// 1 フレームぶん（60 Hz）歩く。
    fn frame_time(&mut self, host_on: bool) {
        let until = self.clock.get() + FRAME_US;
        while self.clock.get() < until {
            self.step(host_on);
        }
    }
}

fn drain(feed: &WatchFeed) -> Vec<Record> {
    let mut v = Vec::new();
    while let Some(e) = feed.peek() {
        v.push(e.rec);
        feed.advance();
    }
    v
}

#[test]
fn ten_minutes_without_a_game_tick_keep_the_host_from_resending_and_the_feed_keeps_every_record() {
    const FRAMES: u32 = 10 * 60 * 60;
    let mut m = Model::new(40_000, 1 << 16, 16_000);
    m.push(Record::Seg { seg: 0 });
    for f in 0..FRAMES {
        m.push(frame(f));
        m.frame_time(true);
    }
    for _ in 0..30 {
        m.frame_time(true);
    }
    let c = m.host.counts();
    assert_eq!(
        (c.resends, c.silenced, c.left),
        (0, 0, 0),
        "ゲームのスレッドが止まっている間にホストが送り直した・外した: {c:?}"
    );
    assert!(m.wp.is_joined());
    let feed = Arc::clone(m.wp.feed());
    assert_eq!(feed.len() as usize, m.pushed.len());
    assert_eq!(drain(&feed), m.pushed, "受けた列とホストの列");
    m.wp.tend(&mut m.wnet);
    assert_eq!(m.wp.end(), None, "ホストは送り続けているのに終えた");
}

#[test]
fn a_silent_host_does_not_end_the_watch_until_the_buffered_records_are_used() {
    // 25 秒ぶん溜める（`HOST_SILENT_END_MS` の 20 秒より長い）。
    const N: u32 = 25 * 60;
    let mut m = Model::new(1 << 12, 1 << 12, 0);
    m.push(Record::Seg { seg: 0 });
    for f in 0..N {
        m.push(frame(f));
        m.frame_time(true);
    }
    for _ in 0..10 {
        m.frame_time(true);
    }
    assert_eq!(m.wp.feed().len(), N + 1);
    // ここからホストは黙る。観戦者のゲームのスレッドが動き出し、1 tick に 1 フレームずつ使う。
    let mut g = InMatch;
    let _ = m.wp.on_input(&mut g, &mut m.wnet);
    assert_eq!(m.wp.phase(), Phase::Selecting, "接続を見ていない");
    m.wp.set_phase_for_test(Phase::Lockstep);
    let mut played = Vec::new();
    let mut kept_while_silent = 0u32;
    for _ in 0..N + 600 {
        m.frame_time(false);
        if let Action::Played(r) = m.wp.on_input(&mut g, &mut m.wnet) {
            played.push(r.frame);
        }
        let silent =
            m.wp.feed()
                .silent_ms((m.clock.get() / 1_000) as u32)
                .unwrap_or(0);
        if m.wp.end().is_some() {
            break;
        }
        if silent >= HOST_SILENT_END_MS {
            kept_while_silent += 1;
        }
    }
    assert!(
        kept_while_silent > 60,
        "ホストが黙ってから {HOST_SILENT_END_MS} ms を越えた後も使い続けた tick が少ない: {kept_while_silent}"
    );
    assert_eq!(
        played,
        (0..N).collect::<Vec<_>>(),
        "溜めた記録を使い切る前に終えた"
    );
    assert_eq!(
        m.wp.end(),
        Some(WatchEnd::HostSilent),
        "使い切った後に終えない"
    );
}

#[test]
fn a_full_feed_ends_the_watch_with_its_own_reason_and_says_bye() {
    let mut m = Model::new(64, 1 << 12, 16_000);
    m.push(Record::Seg { seg: 0 });
    for f in 0..63 {
        m.push(frame(f));
    }
    for _ in 0..30 {
        m.frame_time(true);
    }
    assert_eq!(m.wp.feed().len(), 64);
    assert!(!m.wp.feed().is_full(), "枠ちょうどで満杯にした");
    m.wp.tend(&mut m.wnet);
    assert_eq!(m.wp.end(), None);
    m.push(frame(63));
    for _ in 0..30 {
        m.frame_time(true);
    }
    assert!(m.wp.feed().is_full());
    m.wp.tend(&mut m.wnet);
    assert_eq!(m.wp.end(), Some(WatchEnd::BufferFull));
    assert!(m.slot.buffer_full() && m.slot.is_done());
    let left = m.host.counts().left;
    for _ in 0..30 {
        m.frame_time(true);
    }
    assert_eq!(m.host.counts().left, left + 1, "BYE がホストへ届いていない");
    assert!(
        m.wp.watcher_counts().is_some_and(|c| c.streams > 0),
        "STREAM を受けていない"
    );
}

#[test]
fn a_jump_after_records_were_used_ends_lost_but_the_first_jump_does_not() {
    let mut m = Model::new(64, 1 << 12, 16_000);
    let f = Arc::clone(m.wp.feed());
    // 受信スレッドの代わりにリングへ直に書く（最初の記録は遅れた JOIN の飛びの後）。
    assert!(f.push(&Record::Seg { seg: 0 }, true, 0));
    for n in 0..3 {
        assert!(f.push(&frame(n), false, 0));
    }
    let mut g = InMatch;
    let _ = m.wp.on_input(&mut g, &mut m.wnet);
    m.wp.set_phase_for_test(Phase::Lockstep);
    let mut played = Vec::new();
    for t in 0..8 {
        if t == 1 {
            // 記録を使った後に、流れが飛んだ（ホストのリングバッファから落ちた）。
            assert!(f.push(&Record::Seg { seg: 0 }, true, 0));
            assert!(f.push(&frame(3), false, 0));
        }
        if let Action::Played(r) = m.wp.on_input(&mut g, &mut m.wnet) {
            played.push(r.frame);
        }
        if m.wp.end().is_some() {
            break;
        }
    }
    assert_eq!(
        played,
        vec![0, 1, 2],
        "飛ぶ前の記録を使い切っていない・飛んだ先を使った"
    );
    assert_eq!(m.wp.end(), Some(WatchEnd::Lost));
    assert_eq!(m.wp.stats().jumps, 1);
}

#[test]
fn a_reset_from_the_host_after_records_were_used_ends_lost_after_the_records_before_it() {
    // ホストのリングバッファを小さくし、観戦者に届く前に古い seg を捨てさせる（ホストが `reset` 付きで送り直す）。
    let mut m = Model::new(1 << 12, 64, 16_000);
    m.push(Record::Seg { seg: 0 });
    for f in 0..5 {
        m.push(frame(f));
    }
    for _ in 0..10 {
        m.frame_time(true);
    }
    assert_eq!(m.wp.feed().len(), 6, "最初の 6 記録が届いていない");
    // ここからは届ける前に積む（ネットワークが詰まった形）。seg 1 で埋まり、seg 0 の残りが捨てられる。
    for f in 5..10 {
        m.push(frame(f));
    }
    m.push(Record::Seg { seg: 1 });
    for f in 10..63 {
        m.push(frame(f));
    }
    assert!(
        m.host.counts().jumps >= 1,
        "ホストが観戦者を飛ばしていない: {:?}",
        m.host.counts()
    );
    for _ in 0..30 {
        m.frame_time(true);
    }
    let mut g = InMatch;
    let _ = m.wp.on_input(&mut g, &mut m.wnet);
    m.wp.set_phase_for_test(Phase::Lockstep);
    let mut played = Vec::new();
    for _ in 0..80 {
        m.frame_time(true);
        if let Action::Played(r) = m.wp.on_input(&mut g, &mut m.wnet) {
            played.push(r.frame);
        }
        if m.wp.end().is_some() {
            break;
        }
    }
    assert_eq!(
        played,
        vec![0, 1, 2, 3, 4],
        "飛ぶ前の記録を使い切っていない・飛んだ先を使った"
    );
    assert_eq!(m.wp.end(), Some(WatchEnd::Lost));
    assert_eq!(m.wp.stats().jumps, 1, "飛びの印で終えていない");
}

#[test]
fn an_unreadable_cell_is_counted_on_the_log_line() {
    let m = Model::new(64, 1 << 12, 16_000);
    m.wp.feed().push_unreadable_for_test();
    assert!(m.wp.feed().peek().is_none());
    let status = WatchStatus::new();
    status.publish(&m.wp);
    let text = status.text();
    assert!(
        text.contains("受け渡しのリングバッファの読み取り不能枠 1"),
        "{text}"
    );
}

/// タイトルの偽のゲーム（場面 1・選択の画面の外。記録を使わない）。
struct AtTitle;

impl Game for AtTitle {
    fn scenes(&mut self) -> (u32, u32) {
        (crate::scripted::SCENE_MENU, 0)
    }
    fn local_input(&mut self, _s: Seat) -> u16 {
        0
    }
    fn feed(&mut self, _i: u32, _w: u16) {}
    fn own_select(
        &mut self,
        _s: Seat,
        _r: higanr_core::lockstep::Role,
    ) -> higanr_core::wire::Select {
        unreachable!()
    }
    fn apply_agreed(
        &mut self,
        _p: Seat,
        _a: &higanr_core::selection::Agreed,
    ) -> Result<(), BadValue> {
        unreachable!()
    }
    fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
        true
    }
    fn check_values(&mut self) -> (u16, u32, u8, u8) {
        (0, 0, 0, 0)
    }
}

#[test]
fn a_watcher_left_on_a_screen_that_uses_no_records_ends_20_seconds_after_the_host_goes_silent() {
    let mut m = Model::new(1 << 12, 1 << 12, 16_000);
    m.push(Record::Seg { seg: 0 });
    for _ in 0..120 {
        m.push(Record::Menu { p1: 0, p2: 0 });
    }
    let mut g = AtTitle;
    for _ in 0..60 {
        m.frame_time(true);
        let _ = m.wp.on_input(&mut g, &mut m.wnet);
    }
    assert_eq!(m.wp.phase(), Phase::Selecting);
    assert!(m.wp.feed().len() > 100, "記録が溜まっていない");
    // ここからホストは黙る。観戦者はタイトルに留まり、記録を使わない。
    let silent_at = m.clock.get();
    for _ in 0..(HOST_SILENT_END_MS as u64 * 1_000 / FRAME_US + 120) {
        m.frame_time(false);
        let _ = m.wp.on_input(&mut g, &mut m.wnet);
        if m.wp.end().is_some() {
            break;
        }
    }
    assert_eq!(
        m.wp.end(),
        Some(WatchEnd::HostSilent),
        "記録を使わない画面で終えない"
    );
    let after_ms = (m.clock.get() - silent_at) / 1_000;
    assert!(
        (u64::from(HOST_SILENT_END_MS) - 500..u64::from(HOST_SILENT_END_MS) + 1_000)
            .contains(&after_ms),
        "黙ってから {after_ms} ms で終えた"
    );
    assert!(
        m.wp.feed().len() > 100,
        "記録を使い切って終えた形になっている"
    );
}

#[test]
fn a_game_thread_that_resumes_after_30_seconds_with_a_silent_host_uses_the_buffer_before_ending() {
    let mut m = Model::new(1 << 12, 1 << 12, 0);
    m.push(Record::Seg { seg: 0 });
    let mut g = InMatch;
    let _ = m.wp.on_input(&mut g, &mut m.wnet);
    let mut played = Vec::new();
    let mut next = 0u32;
    // 5 秒は追いついて見る。
    for _ in 0..300 {
        m.push(frame(next));
        next += 1;
        m.frame_time(true);
        if m.wp.phase() == Phase::Selecting {
            m.wp.set_phase_for_test(Phase::Lockstep);
        }
        if let Action::Played(r) = m.wp.on_input(&mut g, &mut m.wnet) {
            played.push(r.frame);
        }
    }
    assert!(played.len() > 250, "追いついて見ていない: {}", played.len());
    // ゲームのスレッドが 30 秒止まる。最初の 5 秒はホストが送り続け、その後は黙る。
    for t in 0..30 * 60 {
        let host_on = t < 5 * 60;
        if host_on {
            m.push(frame(next));
            next += 1;
        }
        m.frame_time(host_on);
    }
    let silent =
        m.wp.feed()
            .silent_ms((m.clock.get() / 1_000) as u32)
            .unwrap_or(0);
    assert!(
        silent >= HOST_SILENT_END_MS,
        "ホストが黙っていない: {silent}"
    );
    let left = m.wp.feed().len();
    assert!(left > 250, "溜まっていない: {left}");
    for _ in 0..(left + 120) {
        m.frame_time(false);
        if let Action::Played(r) = m.wp.on_input(&mut g, &mut m.wnet) {
            played.push(r.frame);
        }
        if m.wp.end().is_some() {
            break;
        }
    }
    assert_eq!(
        played,
        (0..next).collect::<Vec<_>>(),
        "再開の後、溜めた記録を使い切る前に終えた"
    );
    assert_eq!(
        m.wp.end(),
        Some(WatchEnd::HostSilent),
        "使い切った後に終えない"
    );
}

// ---------------- 本物の通信層（loopback） ----------------

struct UdpOut<'a>(&'a UdpSocket);

impl Outlet for UdpOut<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let _ = self.0.send_to(bytes, to);
    }
}

/// 本物の通信層の観戦者と、検査のスレッドで回すホスト（core の `SpectateHost` ＋ loopback のソケット）。
struct Real {
    host: SpectateHost,
    sock: UdpSocket,
    net: GameNet,
    rx: RecvThread,
    wp: WatchPlay,
    t0: Instant,
}

impl Real {
    fn new(prep: impl FnOnce(&mut WatchPlay)) -> Real {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
        sock.set_read_timeout(Some(Duration::from_millis(1)))
            .expect("待ちの上限を置ける");
        let host = SpectateHost::new(host_cfg());
        let (net, rx) =
            net_udp::open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).expect("通信層を開ける");
        let slot = Arc::new(WatchSlot::new());
        slot.offer(watcher_cfg(sock.local_addr().unwrap(), host.public_key()));
        let mut wp = WatchPlay::with_feed_len(slot, watch_cfg(16_000), 1_024);
        prep(&mut wp);
        Real {
            host,
            sock,
            net,
            rx,
            wp,
            t0: Instant::now(),
        }
    }

    /// ホストの 1 周（受けた便を処理して送る）。
    fn host_round(&mut self) {
        let now = self.t0.elapsed().as_millis() as u32;
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            self.host.on_datagram(
                from,
                buf.get(..n).unwrap_or(&[]),
                now,
                &mut UdpOut(&self.sock),
            );
        }
        self.host.poll(now, &mut UdpOut(&self.sock));
    }

    /// 接続して、最初の記録（`Seg`）がリングに入るまで回す。
    fn join(&mut self) {
        self.host.push(Record::Seg { seg: 0 }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(self.wp.is_joined() && self.wp.feed().len() >= 1) {
            assert!(Instant::now() < deadline, "接続しない");
            self.wp.tend(&mut self.net);
            self.host_round();
        }
    }
}

#[test]
fn the_receive_thread_takes_the_stream_and_wakes_the_game_threads_wait_without_allocating() {
    let mut r = Real::new(|_| {});
    let before = crate::test_alloc::count();
    r.wp.tend(&mut r.net);
    assert_eq!(
        crate::test_alloc::count() - before,
        0,
        "受け取りを受信スレッドへ渡す手入れで確保した"
    );
    assert!(r.rx.stats().watch.installed);
    r.join();
    // 起きている印を捨てる（記録を移すたびに立つ。自動で戻るイベント）。
    while !r.net.wait(5_000) {}
    let had = r.wp.feed().len();
    r.host.push(frame(0)).unwrap();
    r.host_round();
    let t0 = Instant::now();
    let timed_out = r.net.wait(500_000);
    let took = t0.elapsed();
    assert!(
        !timed_out && took < Duration::from_millis(200),
        "記録が届いても待ちが起きない: {took:?}"
    );
    assert_eq!(r.wp.feed().len(), had + 1);
    // ゲームのスレッドの経路（手入れと判断）で確保しない。
    for n in 1..5 {
        r.host.push(frame(n)).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while r.wp.feed().len() < had + 5 {
        assert!(Instant::now() < deadline, "残りのフレームが届かない");
        r.host_round();
    }
    let mut g = InMatch;
    let _ = r.wp.on_input(&mut g, &mut r.net);
    r.wp.set_phase_for_test(Phase::Lockstep);
    let before = crate::test_alloc::count();
    let mut played = 0;
    for _ in 0..4 {
        if matches!(r.wp.on_input(&mut g, &mut r.net), Action::Played(_)) {
            played += 1;
        }
    }
    assert_eq!(
        crate::test_alloc::count() - before,
        0,
        "ゲームのスレッドの経路で確保した"
    );
    assert_eq!(played, 4, "届いたフレームを使っていない");
    assert!(r.rx.stats().watch.sent > 0, "受信スレッドから送っていない");
}

/// パニックは検査用のフック（`panic_on_service`）で受け取りの最初の周回に起こす。
#[test]
fn a_panic_in_the_watchers_receiving_stops_only_the_watching_and_the_receive_thread_keeps_going() {
    let mut r = Real::new(|wp| {
        wp.recv_for_test()
            .expect("渡す前の受け取り")
            .panic_on_service
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
    r.wp.tend(&mut r.net);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !r.rx.stats().watch.stopped {
        assert!(
            Instant::now() < deadline,
            "パニックが知らされない（受信スレッドが止まったか、包んでいない）"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    r.wp.tend(&mut r.net);
    assert_eq!(r.wp.end(), Some(WatchEnd::Broken));
    assert!(r.wp.bye.is_none(), "出す受け取りが無いのに BYE を積んだ");
    let status = WatchStatus::new();
    status.publish(&r.wp);
    let text = status.text();
    assert!(text.contains("観戦者の受信処理が停止"), "{text}");
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let me = r.net.local_addr().unwrap();
    for i in 0..20u8 {
        peer.send_to(&[b'H', b'G', b'R', b'T', i], me).unwrap();
    }
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut got = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while got < 20 && Instant::now() < deadline {
        while r.net.recv(&mut buf).is_some() {
            got += 1;
        }
        r.net.wait(2_000);
    }
    assert_eq!(got, 20, "パニックの後に受信スレッドが受けない");
}

/// ホストへ届いた BYE（自分で抜けた）の数を、来なくなるまで回して数える。
fn left_after(r: &mut Real, limit: Duration) -> u64 {
    let deadline = Instant::now() + limit;
    while r.host.counts().left == 0 && Instant::now() < deadline {
        r.host_round();
    }
    r.host.counts().left
}

#[test]
fn a_bye_after_a_game_thread_panic_also_ends_the_watch_and_stops_the_acks() {
    let mut r = Real::new(|_| {});
    r.join();
    r.rx.say_bye(higanr_core::wire::ByeReason::Cancelled);
    assert_eq!(
        left_after(&mut r, Duration::from_secs(3)),
        1,
        "BYE がホストへ届かない"
    );
    let acks = r.host.counts().acks;
    let had = r.wp.feed().len();
    for n in 0..5 {
        r.host.push(frame(n)).unwrap();
    }
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        r.host_round();
    }
    assert_eq!(r.host.counts().acks, acks, "BYE の後も ACK を返した");
    assert_eq!(r.wp.feed().len(), had, "BYE の後も受けた");
}

#[test]
fn a_bye_from_esc_goes_out_even_when_the_network_stops_right_after() {
    let mut r = Real::new(|_| {});
    r.join();
    r.wp.quit_by_esc();
    let Real {
        mut host,
        sock,
        net,
        rx,
        t0,
        ..
    } = r;
    let _ = rx.stop();
    drop(net);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 2048];
    while host.counts().left == 0 && Instant::now() < deadline {
        let now = t0.elapsed().as_millis() as u32;
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            host.on_datagram(from, buf.get(..n).unwrap_or(&[]), now, &mut UdpOut(&sock));
        }
    }
    assert_eq!(
        host.counts().left,
        1,
        "止めたネットワークから BYE が出ていない"
    );
}

#[test]
fn the_receive_thread_refuses_to_send_outside_the_scope() {
    let (mut net, rx) =
        net_udp::open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).expect("通信層を開ける");
    let slot = Arc::new(WatchSlot::new());
    // 文書用のアドレス（loopback の範囲の外）。
    let outside = SocketAddr::from(([192, 0, 2, 1], 47_001));
    slot.offer(watcher_cfg(
        outside,
        SpectateHost::new(host_cfg()).public_key(),
    ));
    let mut wp = WatchPlay::with_feed_len(slot, watch_cfg(16_000), 64);
    wp.tend(&mut net);
    let deadline = Instant::now() + Duration::from_secs(3);
    while rx.stats().watch.refused == 0 {
        assert!(
            Instant::now() < deadline,
            "範囲の外への JOIN を止めていない"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let c = rx.stats().watch;
    assert_eq!(
        (c.sent, c.send_errors),
        (0, 0),
        "範囲の外へ送ろうとした: {c:?}"
    );
}
