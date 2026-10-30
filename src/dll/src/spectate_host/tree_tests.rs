//! 中継の木のホストの側の配線の検査（loopback）。
//! 構成は次のとおり。ホストは本物の受信スレッドに置いた`SpectateSender`。中継役の候補は本物の受信スレッドの観戦者（`WatchPlay` ＋ 配信する側）。その子は検査のスレッドで回すcoreの`Watcher`。
//! 上りの測り（MEASURE → PAIRのµsの到着 → 見積もり）から中継役の選び・紹介・子への記録・中継役が抜けた後のREHOMEまでを、1つの実行で通す。
use super::tests::{MATCH_SESSION, lo, wait_active};
use super::*;
use std::time::{Duration, Instant};

use higanr_core::code;
use higanr_core::spectate::{Watcher, WatcherConfig};
use higanr_core::spectate_pace::FF_UPDATES_DEFAULT;
use higanr_core::spectate_rate::RELAY_MIN_BPS;

use crate::net_udp::{self, GameNet, RecvThread, SecondStun};
use crate::spectate_watch::{WATCH_MARGIN_FRAMES, WatchConfig, WatchPlay, WatchSlot};

/// 中継役に向く上りの太さ。
const THICK: u32 = 2 * RELAY_MIN_BPS;

struct UdpOut<'a>(&'a UdpSocket);

impl Outlet for UdpOut<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let _ = self.0.send_to(bytes, to);
    }
}

fn frame(n: u32) -> Record {
    Record::Frame {
        frame: n,
        p1: n as u16,
        p2: !(n as u16),
    }
}

/// 観戦用のコード（ホストが書いたもの）から作る観戦者の設定。
fn watcher_cfg(code_text: &str, nonce: u64) -> WatcherConfig {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let c = code::read_spectate(code_text, now).expect("観戦用のコードが読める");
    WatcherConfig {
        session_id: c.code.session_id,
        secret: c.code.secret.expect("観戦用のコードは秘密を持つ"),
        nonce,
        name: Name::new("w"),
        host: c.code.candidates,
        host_public: c.host_public,
        key_seed: net_udp::draw_key_seed().expect("OS の乱数が引ける"),
    }
}

struct Cluster {
    _peer: UdpSocket,
    _host_net: GameNet,
    host_rx: RecvThread,
    ring: Arc<SpectateRing>,
    haddr: SocketAddr,
    code_text: String,
    relay_net: GameNet,
    _relay_rx: RecvThread,
    relay_wp: WatchPlay,
    raddr: SocketAddr,
    csock: UdpSocket,
    caddr: SocketAddr,
    child: Option<Watcher>,
    child_got: Vec<(u32, Record)>,
    /// ホストのゲームのスレッドが書いた列（通し番号 = 位置）。
    log: Vec<Record>,
    t0: Instant,
}

impl Cluster {
    /// `fake`: iniの`[spectate] fake_uplink`（上りの見積もりを替える。Noneならloopbackの本物の測り）。
    fn new(fake: Option<u32>) -> Cluster {
        let peer = UdpSocket::bind(lo(0)).unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (host_net, host_rx) = net_udp::open(lo(0), &[peer_addr]).unwrap();
        let haddr = host_net.local_addr().unwrap();
        let spectate = match fake {
            Some(n) => format!("[spectate]\nenable = 1\nfake_uplink = {n}\n"),
            None => "[spectate]\nenable = 1\n".to_string(),
        };
        let s = crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = {peer_addr}\nsession = {MATCH_SESSION}\n{spectate}"
        ))
        .unwrap()
        .unwrap();
        let p = prepare(&s, haddr).unwrap();
        let ring = Arc::clone(&p.ring);
        let code_text = p.code_text.clone();
        assert!(host_rx.install_spectate(p.sender));
        wait_active(&host_rx);

        let (relay_net, relay_rx) =
            net_udp::open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        // loopbackでは2か所目のSTUNに問い合わせないので届く側のネットワーク種別のフラグを設定する。
        relay_net.set_second_stun_for_test(SecondStun::Compared(higanr_core::stun::Mapping::Same));
        let raddr = relay_net.local_addr().unwrap();
        let slot = Arc::new(WatchSlot::new());
        slot.enable_relay(Candidates::from_addrs(&[raddr]).unwrap());
        slot.offer(watcher_cfg(&code_text, 0xC1));
        let relay_wp = WatchPlay::with_feed_len(
            slot,
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: WATCH_MARGIN_FRAMES,
                require_net_selection: false,
                ff_max: FF_UPDATES_DEFAULT,
            },
            4_096,
        );
        let csock = UdpSocket::bind(lo(0)).unwrap();
        csock
            .set_read_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let caddr = csock.local_addr().unwrap();
        let mut c = Cluster {
            _peer: peer,
            _host_net: host_net,
            host_rx,
            ring,
            haddr,
            code_text,
            relay_net,
            _relay_rx: relay_rx,
            relay_wp,
            raddr,
            csock,
            caddr,
            child: None,
            child_got: Vec::new(),
            log: Vec::new(),
            t0: Instant::now(),
        };
        c.write(Record::Seg { seg: 0 });
        c
    }

    fn write(&mut self, rec: Record) {
        assert!(self.ring.push(rec));
        self.log.push(rec);
    }

    fn snap(&self) -> Snapshot {
        self.host_rx.spectate_snapshot().expect("ホストの集計")
    }

    fn host(&self) -> HostCounts {
        self.snap().host
    }

    fn start_child(&mut self) {
        self.child = Some(Watcher::new(watcher_cfg(&self.code_text, 0xC2)));
    }

    fn child(&self) -> &Watcher {
        self.child.as_ref().expect("子を始めていない")
    }

    /// 1周でやること: ホストのゲームのスレッドが16 msごとに1記録を書く・中継役のゲームのスレッドの手入れ・子が記録を受けて送信元へackを返し親へJOINを送り直す。
    fn round(&mut self) {
        let ms = self.t0.elapsed().as_millis() as u32;
        while self.log.len() as u32 <= ms / 16 {
            let n = self.log.len() as u32;
            self.write(frame(n));
        }
        self.relay_wp.tend(&mut self.relay_net);
        let Some(child) = self.child.as_mut() else {
            std::thread::sleep(Duration::from_millis(1));
            return;
        };
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = self.csock.recv_from(&mut buf) {
            child.on_datagram(from, &buf[..n], ms, &mut UdpOut(&self.csock));
        }
        child.poll(ms, &mut UdpOut(&self.csock));
        while let Some(x) = child.pop_indexed() {
            self.child_got.push(x);
        }
    }

    fn until(&mut self, what: &str, secs: u64, mut done: impl FnMut(&mut Cluster) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{what}: {:?} / 子 {:?}",
                self.host_rx.spectate_snapshot().map(|s| (s.host, s.punch)),
                self.child.as_ref().map(Watcher::tree_counts)
            );
            self.round();
        }
    }

    /// 子が受けた列が番号の抜け・重なりなしでホストの列と同じ。
    fn child_matches_log(&self) -> bool {
        let Some(&(first, _)) = self.child_got.first() else {
            return false;
        };
        self.child_got.iter().enumerate().all(|(k, (i, r))| {
            *i == first + k as u32 && self.log.get(*i as usize).is_some_and(|l| l == r)
        })
    }

    /// 中継役が接続して上りの測りが決まるまで回す。
    fn until_measured(&mut self) {
        self.until("中継役がホストに接続しない", 10, |c| {
            c.relay_wp.is_joined()
        });
        self.until("上りの測りが決まらない", 20, |c| {
            c.host().uplinks_measured >= 1
        });
    }
}

#[test]
fn a_measured_viewer_becomes_the_relay_its_child_follows_it_and_goes_home_when_it_leaves() {
    let mut c = Cluster::new(Some(THICK));
    c.until_measured();
    let h = c.host();
    assert_eq!(
        (h.measures, h.uplinks_measured, h.uplinks_unmeasured),
        (1, 1, 0),
        "{h:?}"
    );
    assert!(
        h.pairs >= 6,
        "使える組が 3 組そろう分の PAIR が届いていない: {h:?}"
    );
    assert_eq!(c.snap().relays, 0, "子が入る前に中継役の印が付いた");

    // 子への穴あけの予定を先に作る。子へはホストが中継役を紹介する（中継役の下へ回す）。子のJOINを中継役が受けられるので穴あけは止まる。
    let shared = c.host_rx.spectate_shared().expect("共有");
    shared.request_punch(Candidates::from_addrs(&[c.caddr]).unwrap());
    c.until("穴あけの予定を作らない", 5, |c| {
        c.snap().punch.started == 1
    });
    c.start_child();
    let raddr = c.raddr;
    c.until("子が中継役から受けない", 15, |c| {
        c.child().host_addr() == Some(raddr) && c.child_got.len() >= 20
    });
    let snap = c.snap();
    assert_eq!((snap.relays, snap.host.intros), (1, 1), "{snap:?}");
    // 止めた数は送っている途中の予定だけを数える（送り切った後のJOINは数えない）。
    assert_eq!(
        (snap.punch.started, snap.punch.stopped),
        (1, 1),
        "紹介した子への穴あけが止まっていない: {:?}",
        snap.punch
    );
    assert!(c.child_matches_log(), "子の列がホストの列と違う");
    let side = c
        .relay_wp
        .relay_counts()
        .and_then(|r| r.side)
        .expect("配る側");
    assert_eq!((side.children, side.joined), (1, 1), "{side:?}");
    assert!(
        side.punches_sent >= 1,
        "中継役が子の候補へ穴あけしていない: {side:?}"
    );
    let lines = snap.lines(false);
    for want in [
        "観戦者のツリー: 中継役 1 人 / 中継役への振り分け 1（",
        "上り回線速度の測定: 観戦者へ測定依頼を送った 1（",
        "速度の確定 1・測定不能 0",
    ] {
        assert!(lines.contains(want), "{want:?} が無い: {lines}");
    }

    // 中継役が抜ける: ホストがBYEを受け、子がREHOMEで元のホストへ戻り、列が続く。
    let (left, had) = (c.host().left, c.child_got.len());
    c.relay_wp.feed().end(Some(ByeReason::Cancelled));
    let haddr = c.haddr;
    c.until("子が元のホストへ戻らない", 10, |c| {
        c.host().left > left && c.child().tree_counts().rehomes == 1
    });
    assert!(c.child().parent_is_home());
    assert!(
        c.host().punches_sent >= 1,
        "ホストが外した中継役の子孫へ穴あけしていない: {:?}",
        c.host()
    );
    c.until("戻った子が続きを受けない", 10, |c| {
        c.child().host_addr() == Some(haddr) && c.child_got.len() >= had + 20
    });
    assert!(c.child_matches_log(), "戻った後の列に抜けか重なりがある");
}

/// 受信スレッドを止めた後でも止めた時点の木・上りの測りの数を読み取れる（止めた後は`RecvThread`のハンドルが無く、本ログの終わりの全文では読めない）。
#[test]
fn the_tree_and_measure_counts_survive_stopping_the_receive_thread() {
    let mut c = Cluster::new(Some(THICK));
    c.until_measured();
    c.start_child();
    let raddr = c.raddr;
    c.until("子が中継役から受けない", 15, |c| {
        c.child().host_addr() == Some(raddr) && c.child_got.len() >= 10
    });
    let live = c.snap();
    assert_eq!((live.relays, live.host.intros), (1, 1), "{live:?}");
    let Cluster { host_rx, .. } = c;
    let (_, stopped) = host_rx.stop_with_spectate();
    let stopped = stopped.expect("止めた後の観戦のホストの集計");
    assert_eq!(
        (stopped.relays, stopped.host.intros, stopped.host.measures),
        (1, 1, 1),
        "{stopped:?}"
    );
    assert!(stopped.host.uplinks_measured >= 1, "{stopped:?}");
    let lines = stopped.lines(false);
    for want in [
        "観戦者のツリー: 中継役 1 人 / 中継役への振り分け 1（",
        "速度の確定 1・測定不能 0",
    ] {
        assert!(lines.contains(want), "{want:?} が無い: {lines}");
    }
}

#[test]
fn with_no_fake_uplink_the_pairs_measured_on_loopback_are_thick_enough_to_pick_the_relay() {
    let mut c = Cluster::new(None);
    c.until_measured();
    c.start_child();
    let raddr = c.raddr;
    c.until("子が中継役から受けない", 15, |c| {
        c.child().host_addr() == Some(raddr) && c.child_got.len() >= 10
    });
    let snap = c.snap();
    assert_eq!((snap.relays, snap.host.intros), (1, 1), "{snap:?}");
    assert!(c.child_matches_log());
}

#[test]
fn a_thin_fake_uplink_keeps_the_viewer_from_being_the_relay_and_the_child_is_served_directly() {
    let thin = RELAY_MIN_BPS - 1;
    let mut c = Cluster::new(Some(thin));
    c.until_measured();
    c.start_child();
    let haddr = c.haddr;
    c.until("子がホストから受けない", 15, |c| {
        c.child().host_addr() == Some(haddr) && c.child_got.len() >= 20
    });
    let snap = c.snap();
    assert_eq!((snap.relays, snap.host.intros), (0, 0), "{snap:?}");
    assert_eq!(snap.host.uplinks_measured, 1);
    assert!(c.child_matches_log());
}

#[test]
fn a_relay_that_goes_silent_like_a_crash_sends_its_child_home_after_rejoin_ms_and_the_column_goes_on()
 {
    let mut c = Cluster::new(Some(THICK));
    c.until_measured();
    c.start_child();
    let raddr = c.raddr;
    c.until("子が中継役から受けない", 15, |c| {
        c.child().host_addr() == Some(raddr) && c.child_got.len() >= 20
    });
    let (left, had) = (c.host().left, c.child_got.len());
    // 中継役が子とホストへ送るパケットを全部捨てる（BYEもREHOMEも出ない。強制終了と同じ）。
    c.relay_net.mute_watch_for_test(true);
    let muted_at = Instant::now();
    c.until("子が元のホストへ戻らない", 10, |c| {
        c.child().parent_is_home()
    });
    let waited = muted_at.elapsed();
    let t = c.child().tree_counts();
    assert_eq!(
        (t.parent_silent, t.rehomes),
        (1, 0),
        "黙った親からでなく戻った: {t:?}"
    );
    // 最後の記録は黙らせる直前に届いている（記録は16 msごと）。
    assert!(
        waited + Duration::from_millis(200)
            >= Duration::from_millis(u64::from(higanr_core::spectate::REJOIN_MS)),
        "{waited:?} で戻った"
    );
    let haddr = c.haddr;
    c.until("戻った子が続きを受けない", 10, |c| {
        c.child().host_addr() == Some(haddr) && c.child_got.len() >= had + 200
    });
    assert!(c.child_matches_log(), "戻った後の列に抜けか重なりがある");
    assert_eq!(c.host().left, left, "黙らせた中継役の BYE がホストへ届いた");
}
