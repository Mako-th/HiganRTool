//! 観戦のホストの検査の道具（検査のときだけ）: 観戦者を別のスレッドで走らせる（core の `Watcher` を直接回す）ことと、観戦者が
//! 受けた記録の列をマッチの語・押下・SELECT に畳むこと。`spectate_host.rs` の検査、`netplay/loopback.rs`、`netplay/spectate_tests.rs` が使う。
#![forbid(unsafe_code)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::code;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::selection::Agreed;
use higanr_core::spectate::{Watcher, WatcherConfig, WatcherCounts};
use higanr_core::wire::{ByeReason, Record, Seat, Select, Step};

use crate::net_udp::RecvThread;
use crate::netplay::NetPlay;

/// ホストのウィンドウ（`core::spectate::SEND_WINDOW`）。ACK を返さない観戦者が受けられる記録の数の上限。
pub const NOACK_WINDOW: usize = higanr_core::spectate::SEND_WINDOW as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    Normal,
    /// JOIN は送るが、届いた STREAM に ACK を返さない（ウィンドウの 64 記録で止まる）。
    NoAck,
    /// JOIN を 1 回送った後は何も聞かず何も送らない（ソケットは開いたまま）。
    Silent,
    /// 記録を `n` 個受けたところで消える（ソケットを閉じる。以後ホストが送ると ICMP の port unreachable が返る）。
    Vanish(usize),
}

#[derive(Debug)]
pub struct Seen {
    pub recs: Vec<(u32, Record)>,
    pub counts: WatcherCounts,
    pub joined: bool,
    pub datagrams: u32,
    /// 受けたデータグラムの長さの合計（ホストが送った量。loopback は落とさない）。
    pub bytes: u64,
    /// `recs` の各記録を取り出した時刻（`recs` と同じ並び。ホストが積んでから観戦者が使えるまでの遅れを測る）。
    pub popped: Vec<Instant>,
}

struct UdpOut<'a>(&'a UdpSocket);

impl Outlet for UdpOut<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let _ = self.0.send_to(bytes, to);
    }
}

struct NullOut;

impl Outlet for NullOut {
    fn send(&mut self, _to: SocketAddr, _bytes: &[u8]) {}
}

pub struct Observer {
    pub behavior: Behavior,
    stop: Arc<AtomicBool>,
    end: Arc<AtomicBool>,
    join: Option<JoinHandle<Seen>>,
}

impl Observer {
    /// `code_text` は `spectate_code.txt` の中身（`Prepared::code_text`）。`nonce` は観戦者ごとに変える。
    pub fn spawn(code_text: &str, behavior: Behavior, nonce: u64) -> Observer {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let c = code::read_spectate(code_text, now).expect("観戦用のコードが読める");
        let stop = Arc::new(AtomicBool::new(false));
        let end = Arc::new(AtomicBool::new(false));
        let join = {
            let (stop, end) = (Arc::clone(&stop), Arc::clone(&end));
            std::thread::spawn(move || observe(c, behavior, nonce, &stop, &end))
        };
        Observer {
            behavior,
            stop,
            end,
            join: Some(join),
        }
    }

    pub fn saw_end(&self) -> bool {
        self.end.load(Ordering::Acquire)
    }

    pub fn wait_end(&self, limit: Duration) -> bool {
        let t0 = Instant::now();
        while !self.saw_end() && t0.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(2));
        }
        self.saw_end()
    }

    pub fn finish(mut self) -> Seen {
        self.stop.store(true, Ordering::Release);
        self.join
            .take()
            .expect("まだ join していない")
            .join()
            .expect("観戦者のスレッドがパニックしない")
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn observe(
    c: code::SpectateCode,
    behavior: Behavior,
    nonce: u64,
    stop: &AtomicBool,
    end: &AtomicBool,
) -> Seen {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("loopback に bind できる");
    sock.set_read_timeout(Some(Duration::from_millis(1)))
        .expect("待ちの上限を置ける");
    let mut w = Watcher::new(WatcherConfig {
        session_id: c.code.session_id,
        secret: c.code.secret.expect("観戦用のコードは秘密を持つ"),
        nonce,
        name: Name::new("watcher"),
        host: c.code.candidates,
        // ホストの公開鍵は観戦用のコードから、観戦者の種は観戦者ごとに OS の乱数で（本物の観戦者と同じ）。
        host_public: c.host_public,
        key_seed: crate::net_udp::draw_key_seed().expect("OS の乱数が引ける"),
    });
    let t0 = Instant::now();
    let mut buf = [0u8; 2048];
    let mut seen = Seen {
        recs: Vec::new(),
        counts: WatcherCounts::default(),
        joined: false,
        datagrams: 0,
        bytes: 0,
        popped: Vec::new(),
    };
    let mut join_sent = false;
    while !stop.load(Ordering::Acquire) {
        let now = t0.elapsed().as_millis() as u32;
        if behavior == Behavior::Silent {
            if !join_sent {
                w.poll(now, &mut UdpOut(&sock));
                join_sent = true;
            }
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            seen.datagrams += 1;
            seen.bytes += n as u64;
            let bytes = buf.get(..n).unwrap_or(&[]);
            if behavior == Behavior::NoAck {
                w.on_datagram(from, bytes, now, &mut NullOut);
            } else {
                w.on_datagram(from, bytes, now, &mut UdpOut(&sock));
            }
        }
        if behavior == Behavior::NoAck && w.is_joined() {
            w.poll(now, &mut NullOut);
        } else {
            w.poll(now, &mut UdpOut(&sock));
        }
        while let Some((i, r)) = w.pop_indexed() {
            seen.recs.push((i, r));
            seen.popped.push(Instant::now());
            if matches!(r, Record::End(_)) {
                end.store(true, Ordering::Release);
            }
        }
        if let Behavior::Vanish(n) = behavior
            && seen.recs.len() >= n
        {
            break;
        }
    }
    seen.counts = w.counts();
    seen.joined = w.is_joined();
    seen
}

/// 本物の経路（`spectate_host::arm` と `commit` と同じ部品）で観戦のホストを用意し、リングバッファをゲームのセッション `np` に付け、送り手を受信スレッド `rx` に置き、
/// `behaviors` の観戦者を起こして全員が入る（ホストが JOIN を受ける）まで待つ。`local` はゲームの受信ソケット、`session` は対戦の session
/// （観戦用の session はそれと別に引かれる）。
pub fn attach(
    np: &mut NetPlay,
    rx: &RecvThread,
    local: SocketAddr,
    session: u32,
    behaviors: &[Behavior],
) -> Vec<Observer> {
    attach_with_code(np, rx, local, session, behaviors).0
}

/// `attach` に、観戦用のコードの文字（`spectate_code.txt` の中身）も返す関数（dll の観戦者を別に起こす検査が使う）。
pub fn attach_with_code(
    np: &mut NetPlay,
    rx: &RecvThread,
    local: SocketAddr,
    session: u32,
    behaviors: &[Behavior],
) -> (Vec<Observer>, String) {
    let ini = format!(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:9\nsession = {session}\n[spectate]\nenable = 1\n"
    );
    let s = crate::net_config::from_text(&ini).unwrap().unwrap();
    let p = crate::spectate_host::prepare(&s, local).expect("観戦のホストを用意できる");
    np.set_spectate(Arc::clone(&p.ring));
    assert!(rx.install_spectate(p.sender));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rx.spectate_active() {
        assert!(
            Instant::now() < deadline,
            "受信スレッドが観戦のホストを取らない"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let observers: Vec<Observer> = behaviors
        .iter()
        .enumerate()
        .map(|(i, &b)| Observer::spawn(&p.code_text, b, 0x100 + i as u64))
        .collect();
    while rx.spectate_snapshot().map_or(0, |s| s.host.joined) < behaviors.len() as u64 {
        assert!(
            Instant::now() < deadline,
            "観戦者が全員入らない: {:?}",
            rx.spectate_snapshot()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    (observers, p.code_text)
}

/// 揃った値から、席ごとの SELECT の記録の期待。`netplay.rs` の `spec_selects` とは別に書いた（同じ間違いを 2 か所でコピーしないため）。
pub fn expected_selects(own_seat: Seat, own: Select, a: &Agreed) -> [Record; 2] {
    let common = |s: Select| Select {
        map: a.map,
        difficulty: a.difficulty,
        cpu_1p: a.cpu_1p,
        cpu_2p: a.cpu_2p,
        background: a.background,
        rng_seed: a.rng_seed,
        ..s
    };
    let p1 = if own_seat == Seat::P1 { own } else { a.peer };
    let p2 = if own_seat == Seat::P2 { own } else { a.peer };
    [
        Record::Select {
            seat: Seat::P1,
            sel: common(p1),
        },
        Record::Select {
            seat: Seat::P2,
            sel: common(p2),
        },
    ]
}

/// 記録の列を、マッチの語・押下・SELECT に畳んだもの（比べる側が使う）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Folded {
    pub frames: Vec<(u16, u32, u16, u16)>,
    pub segs: Vec<u16>,
    pub steps: Vec<Step>,
    pub menus: Vec<(u16, u16)>,
    pub selects: Vec<(Seat, Select)>,
    pub ends: Vec<ByeReason>,
    pub names: Vec<(Seat, String)>,
    pub stats: Vec<(u16, u8, u8)>,
}

/// 畳む。最初の記録が `Seg` でなければ panic（流れは seg の始まりから始まる）。
pub fn fold(recs: &[Record]) -> Folded {
    let mut f = Folded::default();
    let mut seg: Option<u16> = None;
    for r in recs {
        match *r {
            Record::Seg { seg: s } => {
                seg = Some(s);
                f.segs.push(s);
            }
            Record::Frame { frame, p1, p2 } => {
                f.frames
                    .push((seg.expect("Frame の前に Seg が無い"), frame, p1, p2))
            }
            Record::Step(s) => f.steps.push(s),
            Record::Menu { p1, p2 } => f.menus.push((p1, p2)),
            Record::Select { seat, sel } => f.selects.push((seat, sel)),
            Record::End(reason) => f.ends.push(reason),
            Record::Name { seat, len, bytes } => {
                let text = bytes
                    .get(..usize::from(len))
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("");
                f.names.push((seat, text.to_string()));
            }
            Record::Stats {
                ping_ms,
                latency,
                rollback,
            } => f.stats.push((ping_ms, latency, rollback)),
        }
    }
    f
}
