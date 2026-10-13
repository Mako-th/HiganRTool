//! 遅延とロスを揺らす UDP の中継（`peer_sim --shape`）。同じ PC の本物の花映塚 2 台（dll 同士）の間に挟み、
//! 向きごとに「落とす・遅らせる・重ねる・一定の遅れ」を掛ける。ロールバックと観戦を本物 2 台の間でローカルに確かめる土台。
//!
//! peer_sim 自身の乱れ（`fault`）は自分が送る入力ワードにしか掛からず、巻き戻しの正しさは本物どうしの間でしか見えないので、
//! 中継は運ぶだけに徹する。
//!
//! ```text
//!   ゲーム A ── peer = via_a ──▶ [via_a  中継  via_b] ◀── peer = via_b ── ゲーム B
//!   （A の bind = a）                                       （B の bind = b）
//! ```
//!
//! - ポートを 2 つ開く。`via_a` で a から受けたものを `via_b` から b へ（A→B）、`via_b` で b から受けたものを
//!   `via_a` から a へ（B→A）送る。dll の `peer` を中継のポートに向ければ、dll には相手が中継に見える。
//! - 中身は読まずにそのまま運ぶ（書き換えれば認証タグで捨てられる）。プロトコルの決まりはここに書かない。
//! - 送り元が受けたポートの向きの相手（a / b）と違うものは捨てて数える（`route`）。知らない送り元を流すと別人の入力ワードが見える。
//! - 乱れは向きごとに別の `Faults`（別の乱数の列）で決める。決め方は `sim.rs` の `Tx::send` と同じ。
//!   遅らせたものは時刻の来た順に出す。
//! - ガードは loopback だけ（4 つのアドレスとも 127.0.0.0/8 か ::1）。開く前（`Args::check`）と送る直前に見る。

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use higanr_core::guard::{self, Scope};

use crate::fault::{Fate, FaultConfig, Faults};

/// `Args::max_sec` の既定（30 分）。
pub const DEFAULT_MAX_SEC: u32 = 1_800;

/// `--max-sec` の上限（2 時間。落とし忘れて受信ソケットが残らないための上限）。
pub const MAX_MAX_SEC: u32 = 7_200;

pub const REPORT_EVERY_SEC: u64 = 10;

/// 運ぶ向き。`AtoB` は a から受けて b へ（受けるポートは `via_a`、出すポートは `via_b`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    AtoB,
    BtoA,
}

impl Dir {
    pub const ALL: [Dir; 2] = [Dir::AtoB, Dir::BtoA];

    pub fn rev(self) -> Dir {
        match self {
            Dir::AtoB => Dir::BtoA,
            Dir::BtoA => Dir::AtoB,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dir::AtoB => "A→B",
            Dir::BtoA => "B→A",
        }
    }

    fn idx(self) -> usize {
        match self {
            Dir::AtoB => 0,
            Dir::BtoA => 1,
        }
    }
}

/// 本物の 2 台の受信ソケット（それぞれのゲームが送り出すアドレス。dll の ini の `bind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ends {
    pub a: SocketAddr,
    pub b: SocketAddr,
}

impl Ends {
    pub fn source(&self, dir: Dir) -> SocketAddr {
        match dir {
            Dir::AtoB => self.a,
            Dir::BtoA => self.b,
        }
    }

    pub fn dest(&self, dir: Dir) -> SocketAddr {
        self.source(dir.rev())
    }
}

/// 受けた 1 通の行き先の判断。`arrived` は着いたポートが受ける向き。送り元がその向きの送り元と同じときだけその向きで運び、
/// 食い違うもの（a が `via_b` へ送った・知らない相手）は `None`（捨てて数える）。
pub fn route(ends: &Ends, arrived: Dir, from: SocketAddr) -> Option<Dir> {
    (from == ends.source(arrived)).then_some(arrived)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaneCounts {
    /// 受けて運ぶことにした通数（送り元違いは含まない）。
    pub received: u64,
    pub dropped: u64,
    /// 運命が「遅らせる」だった通数（一定の遅れだけの通は数えない）。
    pub delayed: u64,
    pub dupped: u64,
    /// 実際に送り出したコピーの数（重ねた分を含む）。
    pub emitted: u64,
    pub foreign: u64,
    pub send_errors: u64,
    pub jittered: u64,
}

#[derive(Debug, Clone)]
pub struct Lane {
    faults: Faults,
    pending: Vec<(u32, Vec<u8>)>,
    emitted: u64,
    foreign: u64,
    send_errors: u64,
}

impl Lane {
    pub fn new(cfg: FaultConfig) -> Lane {
        Lane {
            faults: Faults::new(cfg),
            pending: Vec::new(),
            emitted: 0,
            foreign: 0,
            send_errors: 0,
        }
    }

    /// 受けた 1 通に運命を決める。いますぐ出すコピーを `out` に足す。一定の遅れは運命の後に、届けるコピーの全部へ足す。
    pub fn accept(&mut self, now: u32, bytes: &[u8], out: &mut Vec<Vec<u8>>) {
        let lag = self.faults.one_way_ms();
        match self.faults.decide() {
            Fate::Deliver => self.after(now, lag, bytes, out),
            Fate::Drop => {}
            Fate::Delay(ms) => self.after(now, lag.saturating_add(ms), bytes, out),
            Fate::Dup(ms) => {
                self.after(now, lag, bytes, out);
                self.after(now, lag.saturating_add(ms), bytes, out);
            }
        }
    }

    fn after(&mut self, now: u32, after_ms: u32, bytes: &[u8], out: &mut Vec<Vec<u8>>) {
        // 揺らぎはコピー 1 通ごとに別に引く。
        let after_ms = after_ms.saturating_add(self.faults.jitter());
        if after_ms == 0 {
            self.emitted += 1;
            out.push(bytes.to_vec());
        } else {
            self.pending
                .push((now.wrapping_add(after_ms), bytes.to_vec()));
        }
    }

    pub fn due(&mut self, now: u32, out: &mut Vec<Vec<u8>>) {
        let emitted = &mut self.emitted;
        self.pending.retain(|(at, b)| {
            // 差で比べる（u32 の ms の折り返しでも正しい）。
            if (now.wrapping_sub(*at) as i32) >= 0 {
                *emitted += 1;
                out.push(b.clone());
                false
            } else {
                true
            }
        });
    }

    pub fn note_foreign(&mut self) {
        self.foreign += 1;
    }

    pub fn note_send_error(&mut self) {
        self.send_errors += 1;
    }

    pub fn counts(&self) -> LaneCounts {
        let f = self.faults.counts();
        LaneCounts {
            received: f.decided,
            dropped: f.dropped,
            delayed: f.delayed,
            dupped: f.dupped,
            emitted: self.emitted,
            foreign: self.foreign,
            send_errors: self.send_errors,
            jittered: f.jittered,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub a: SocketAddr,
    pub b: SocketAddr,
    /// A 向けのポート（A の ini の `peer` に書くアドレス）。
    pub via_a: SocketAddr,
    pub via_b: SocketAddr,
    pub a2b: FaultConfig,
    pub b2a: FaultConfig,
    pub max_sec: u32,
}

impl Args {
    pub fn ends(&self) -> Ends {
        Ends {
            a: self.a,
            b: self.b,
        }
    }

    pub fn fault(&self, dir: Dir) -> FaultConfig {
        match dir {
            Dir::AtoB => self.a2b,
            Dir::BtoA => self.b2a,
        }
    }

    /// 開く前の検査: 4 つのアドレスが全部 loopback で、互いに違い（同じアドレスだと自分へ送り返すリングバッファができる）、
    /// 乱れの設定が読めて、秒数が範囲の中。
    pub fn check(&self) -> Result<(), String> {
        guard::check_peer(self.a, Scope::Loopback).map_err(|e| format!("--a: {e}"))?;
        guard::check_peer(self.b, Scope::Loopback).map_err(|e| format!("--b: {e}"))?;
        guard::check_bind(self.via_a, Scope::Loopback, "A 向けの口（--via-a）")?;
        guard::check_bind(self.via_b, Scope::Loopback, "B 向けの口（--via-b）")?;
        let all = [self.a, self.b, self.via_a, self.via_b];
        for (i, x) in all.iter().enumerate() {
            if all[..i].contains(x) {
                return Err(format!(
                    "アドレス {x} が 2 つの引数で同じ（--a --b --via-a --via-b は全部違うアドレス。同じだと自分へ送り返すループができる）"
                ));
            }
        }
        self.a2b
            .validate()
            .map_err(|e| format!("A→B の乱れ: {e}"))?;
        self.b2a
            .validate()
            .map_err(|e| format!("B→A の乱れ: {e}"))?;
        if !(1..=MAX_MAX_SEC).contains(&self.max_sec) {
            return Err(format!("--max-sec は 1〜{MAX_MAX_SEC}（秒）"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    MaxSec,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub a2b: LaneCounts,
    pub b2a: LaneCounts,
    pub elapsed_ms: u64,
    pub ended: Ended,
}

pub fn describe(cfg: &FaultConfig) -> String {
    let items = [
        ("落とす%", u64::from(cfg.drop_pct)),
        ("遅らせる%", u64::from(cfg.delay_pct)),
        ("重ねる%", u64::from(cfg.dup_pct)),
        ("遅らせる幅ms", u64::from(cfg.delay_max_ms)),
        ("一定の遅れms", u64::from(cfg.one_way_ms)),
        ("揺らぎの幅ms", u64::from(cfg.jitter_ms)),
        ("決め打ちの落とし 倍数", u64::from(cfg.drop_every)),
        ("決め打ちの落とし 続ける通数", u64::from(cfg.drop_run)),
        ("これを越えたら全部落とす通番", cfg.drop_after),
    ];
    let set: Vec<String> = items
        .iter()
        .filter(|(_, v)| *v != 0)
        .map(|(k, v)| format!("{k} {v}"))
        .collect();
    if set.is_empty() {
        format!("なし（seed {}）", cfg.seed)
    } else {
        format!("{}（seed {}）", set.join("・"), cfg.seed)
    }
}

pub fn line(elapsed_ms: u64, a2b: &LaneCounts, b2a: &LaneCounts) -> String {
    let one = |dir: Dir, c: &LaneCounts| {
        format!(
            "{} 受けた {} 落とした {} 遅らせた {} 重ねた {} 出した {} 捨てた（送り元違い）{} 送信失敗 {} 揺らいだ {}",
            dir.name(),
            c.received,
            c.dropped,
            c.delayed,
            c.dupped,
            c.emitted,
            c.foreign,
            c.send_errors,
            c.jittered
        )
    };
    format!(
        "peer_sim(shape) {}.{} 秒 | {} | {}",
        elapsed_ms / 1_000,
        (elapsed_ms % 1_000) / 100,
        one(Dir::AtoB, a2b),
        one(Dir::BtoA, b2a)
    )
}

pub fn run(
    args: &Args,
    stop: &AtomicBool,
    on_line: &mut dyn FnMut(&str),
) -> Result<Report, String> {
    args.check()?;
    let sock_a =
        UdpSocket::bind(args.via_a).map_err(|e| format!("{} で待てない: {e}", args.via_a))?;
    let sock_b =
        UdpSocket::bind(args.via_b).map_err(|e| format!("{} で待てない: {e}", args.via_b))?;
    run_with_sockets(args, sock_a, sock_b, stop, on_line)
}

/// 開いたポートで走らせる（検査用）。`sock_a` は A 向けのポート、`sock_b` は B 向けのポート。`stop` が立つか `max_sec` で終わる。
pub fn run_with_sockets(
    args: &Args,
    sock_a: UdpSocket,
    sock_b: UdpSocket,
    stop: &AtomicBool,
    on_line: &mut dyn FnMut(&str),
) -> Result<Report, String> {
    args.check()?;
    for s in [&sock_a, &sock_b] {
        s.set_nonblocking(true)
            .map_err(|e| format!("ソケットを非同期にできない: {e}"))?;
    }
    let ends = args.ends();
    let socks = [(&sock_a, &sock_b), (&sock_b, &sock_a)];
    let mut lanes = [Lane::new(args.a2b), Lane::new(args.b2a)];
    let mut buf = vec![0u8; 65_536];
    let mut out: Vec<Vec<u8>> = Vec::new();
    let t0 = Instant::now();
    let mut next_report = REPORT_EVERY_SEC * 1_000;
    let ended = loop {
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        if stop.load(Ordering::Relaxed) {
            break Ended::Stopped;
        }
        if elapsed_ms >= u64::from(args.max_sec) * 1_000 {
            break Ended::MaxSec;
        }
        let now = elapsed_ms as u32;
        for dir in Dir::ALL {
            let (rx, tx) = socks[dir.idx()];
            // 受け続ける相手がいても、遅らせたコピーを出す番が回るように。
            for _ in 0..256 {
                match rx.recv_from(&mut buf) {
                    Ok((n, from)) => match route(&ends, dir, from) {
                        Some(d) => {
                            out.clear();
                            lanes[d.idx()].accept(now, &buf[..n], &mut out);
                            send_all(&mut lanes[d.idx()], tx, ends.dest(d), &out);
                        }
                        None => lanes[dir.idx()].note_foreign(),
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    // 相手のポートが閉じていたときの ICMP の跳ね返り（Windows の 10054）や大きすぎる 1 通は読み捨てる。
                    Err(_) => {}
                }
            }
            out.clear();
            lanes[dir.idx()].due(now, &mut out);
            send_all(&mut lanes[dir.idx()], tx, ends.dest(dir), &out);
        }
        if elapsed_ms >= next_report {
            next_report += REPORT_EVERY_SEC * 1_000;
            on_line(&line(elapsed_ms, &lanes[0].counts(), &lanes[1].counts()));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    let rep = Report {
        a2b: lanes[0].counts(),
        b2a: lanes[1].counts(),
        elapsed_ms,
        ended,
    };
    on_line(&line(elapsed_ms, &rep.a2b, &rep.b2a));
    Ok(rep)
}

fn send_all(lane: &mut Lane, sock: &UdpSocket, to: SocketAddr, copies: &[Vec<u8>]) {
    for c in copies {
        if !guard::allows(Scope::Loopback, to) || sock.send_to(c, to).is_err() {
            lane.note_send_error();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sa(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn ends() -> Ends {
        Ends {
            a: sa(47001),
            b: sa(47002),
        }
    }

    #[test]
    fn route_decides_the_direction_by_source_and_socket() {
        let e = ends();
        assert_eq!(route(&e, Dir::AtoB, e.a), Some(Dir::AtoB));
        assert_eq!(route(&e, Dir::BtoA, e.b), Some(Dir::BtoA));
        assert_eq!(route(&e, Dir::BtoA, e.a), None);
        assert_eq!(route(&e, Dir::AtoB, e.b), None);
        for d in Dir::ALL {
            assert_eq!(route(&e, d, sa(47003)), None);
            assert_eq!(
                route(&e, d, SocketAddr::from(([127, 0, 0, 2], 47001))),
                None
            );
        }
        assert_eq!((e.source(Dir::AtoB), e.dest(Dir::AtoB)), (e.a, e.b));
        assert_eq!((e.source(Dir::BtoA), e.dest(Dir::BtoA)), (e.b, e.a));
    }

    #[test]
    fn lane_applies_fate_and_constant_lag_to_every_copy() {
        let mut l = Lane::new(FaultConfig {
            one_way_ms: 30,
            ..FaultConfig::default()
        });
        let mut out = Vec::new();
        l.accept(1_000, b"x", &mut out);
        assert!(out.is_empty(), "遅れの前に出た");
        l.due(1_029, &mut out);
        assert!(out.is_empty(), "29 ms で出た");
        l.due(1_030, &mut out);
        assert_eq!(out, vec![b"x".to_vec()]);
        l.due(2_000, &mut out);
        assert_eq!(out.len(), 1, "二重に出た");
        assert_eq!(l.counts().emitted, 1);

        let mut l = Lane::new(FaultConfig {
            drop_pct: 100,
            ..FaultConfig::default()
        });
        let mut out = Vec::new();
        for _ in 0..50 {
            l.accept(0, b"y", &mut out);
        }
        l.due(10_000, &mut out);
        assert!(out.is_empty());
        let c = l.counts();
        assert_eq!((c.received, c.dropped, c.emitted), (50, 50, 0));

        let mut l = Lane::new(FaultConfig {
            dup_pct: 100,
            delay_max_ms: 5,
            ..FaultConfig::default()
        });
        let mut out = Vec::new();
        l.accept(0, b"z", &mut out);
        assert_eq!(out.len(), 1);
        l.due(5, &mut out);
        assert_eq!(out.len(), 2);
        let c = l.counts();
        assert_eq!((c.received, c.dupped, c.emitted), (1, 1, 2));
    }

    #[test]
    fn lane_is_deterministic_per_seed_and_lanes_do_not_share_streams() {
        let cfg = FaultConfig {
            seed: 7,
            drop_pct: 30,
            delay_pct: 20,
            delay_max_ms: 40,
            ..FaultConfig::default()
        };
        let run = |c: FaultConfig| {
            let mut l = Lane::new(c);
            let mut out = Vec::new();
            for i in 0..500u32 {
                l.accept(i, &i.to_le_bytes(), &mut out);
                l.due(i, &mut out);
            }
            (l.counts(), out)
        };
        assert_eq!(run(cfg), run(cfg), "同じ seed で違う列");
        assert_ne!(
            run(cfg).0,
            run(FaultConfig { seed: 8, ..cfg }).0,
            "seed が効かない"
        );
    }

    fn base_args() -> Args {
        Args {
            a: sa(47001),
            b: sa(47002),
            via_a: sa(47011),
            via_b: sa(47012),
            a2b: FaultConfig::default(),
            b2a: FaultConfig::default(),
            max_sec: 60,
        }
    }

    #[test]
    fn args_check_refuses_overlaps_out_of_loopback_and_bad_ranges() {
        assert!(base_args().check().is_ok());
        let cases: Vec<(&str, Args)> = vec![
            (
                "a と via_a が同じ",
                Args {
                    via_a: sa(47001),
                    ..base_args()
                },
            ),
            (
                "via が同じ",
                Args {
                    via_b: sa(47011),
                    ..base_args()
                },
            ),
            (
                "a と b が同じ",
                Args {
                    b: sa(47001),
                    ..base_args()
                },
            ),
            (
                "LAN のアドレス",
                Args {
                    via_a: "192.168.1.5:47011".parse().unwrap(),
                    ..base_args()
                },
            ),
            (
                "外のアドレスの相手",
                Args {
                    b: "8.8.8.8:47002".parse().unwrap(),
                    ..base_args()
                },
            ),
            (
                "全部の口",
                Args {
                    via_b: "0.0.0.0:47012".parse().unwrap(),
                    ..base_args()
                },
            ),
            (
                "相手のポート 0",
                Args {
                    a: sa(0),
                    ..base_args()
                },
            ),
            (
                "割合の和が 100 超",
                Args {
                    a2b: FaultConfig {
                        drop_pct: 60,
                        delay_pct: 50,
                        delay_max_ms: 5,
                        ..FaultConfig::default()
                    },
                    ..base_args()
                },
            ),
            (
                "秒数 0",
                Args {
                    max_sec: 0,
                    ..base_args()
                },
            ),
            (
                "秒数が上限超",
                Args {
                    max_sec: MAX_MAX_SEC + 1,
                    ..base_args()
                },
            ),
        ];
        for (what, a) in cases {
            assert!(a.check().is_err(), "{what} を通した");
        }
        assert!(
            Args {
                via_a: "[::1]:47011".parse().unwrap(),
                ..base_args()
            }
            .check()
            .is_ok()
        );
    }

    #[test]
    fn describe_lists_only_the_faults_that_are_set() {
        let s = describe(&FaultConfig {
            seed: 9,
            drop_pct: 2,
            one_way_ms: 30,
            ..FaultConfig::default()
        });
        assert_eq!(s, "落とす% 2・一定の遅れms 30（seed 9）");
        assert_eq!(describe(&FaultConfig::default()), "なし（seed 0）");
    }

    #[test]
    fn line_names_every_count_per_direction() {
        let a = LaneCounts {
            received: 11,
            dropped: 2,
            delayed: 3,
            dupped: 4,
            emitted: 15,
            foreign: 6,
            send_errors: 7,
            jittered: 8,
        };
        let b = LaneCounts::default();
        let s = line(12_345, &a, &b);
        assert!(s.contains("12.3 秒"), "{s}");
        assert!(
            s.contains(
                "A→B 受けた 11 落とした 2 遅らせた 3 重ねた 4 出した 15 捨てた（送り元違い）6 送信失敗 7 揺らいだ 8"
            ),
            "{s}"
        );
        assert!(s.contains("B→A 受けた 0 落とした 0"), "{s}");
    }

    struct Rig {
        a: UdpSocket,
        b: UdpSocket,
        via_a: SocketAddr,
        via_b: SocketAddr,
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<Report>>,
    }

    impl Rig {
        fn new(a2b: FaultConfig, b2a: FaultConfig) -> Rig {
            let a = UdpSocket::bind("127.0.0.1:0").unwrap();
            let b = UdpSocket::bind("127.0.0.1:0").unwrap();
            for s in [&a, &b] {
                s.set_read_timeout(Some(Duration::from_millis(400)))
                    .unwrap();
            }
            let va = UdpSocket::bind("127.0.0.1:0").unwrap();
            let vb = UdpSocket::bind("127.0.0.1:0").unwrap();
            let (via_a, via_b) = (va.local_addr().unwrap(), vb.local_addr().unwrap());
            let args = Args {
                a: a.local_addr().unwrap(),
                b: b.local_addr().unwrap(),
                via_a,
                via_b,
                a2b,
                b2a,
                max_sec: 30,
            };
            let stop = Arc::new(AtomicBool::new(false));
            let st = Arc::clone(&stop);
            let join = std::thread::spawn(move || {
                run_with_sockets(&args, va, vb, &st, &mut |_| {}).expect("中継が走る")
            });
            Rig {
                a,
                b,
                via_a,
                via_b,
                stop,
                join: Some(join),
            }
        }

        fn finish(mut self) -> Report {
            self.stop.store(true, Ordering::Relaxed);
            self.join
                .take()
                .unwrap()
                .join()
                .expect("中継がパニックしない")
        }
    }

    fn recv(s: &UdpSocket) -> Option<(Vec<u8>, SocketAddr)> {
        let mut buf = [0u8; 2048];
        s.recv_from(&mut buf)
            .ok()
            .map(|(n, f)| (buf[..n].to_vec(), f))
    }

    #[test]
    fn relay_carries_each_direction_with_its_own_faults_and_drops_strangers() {
        let rig = Rig::new(
            FaultConfig {
                drop_pct: 100,
                ..FaultConfig::default()
            },
            FaultConfig {
                one_way_ms: 60,
                ..FaultConfig::default()
            },
        );
        rig.a.send_to(b"a-to-b", rig.via_a).unwrap();
        assert!(recv(&rig.b).is_none(), "A→B の全部落としが効いていない");
        // 送り元は A 向けのポート（dll の peer と同じアドレス）。
        let t = Instant::now();
        rig.b.send_to(b"b-to-a\x00\xff", rig.via_b).unwrap();
        let (bytes, from) = recv(&rig.a).expect("B→A が着かない");
        assert!(
            t.elapsed() >= Duration::from_millis(60),
            "一定の遅れが効かない"
        );
        assert_eq!(bytes, b"b-to-a\x00\xff");
        assert_eq!(from, rig.via_a, "出す口が A 向けの口でない");
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        stranger.send_to(b"who", rig.via_a).unwrap();
        stranger.send_to(b"who", rig.via_b).unwrap();
        rig.a.send_to(b"wrong-socket", rig.via_b).unwrap();
        assert!(recv(&rig.b).is_none());
        assert!(recv(&rig.a).is_none());
        let rep = rig.finish();
        assert_eq!(
            (rep.a2b.received, rep.a2b.dropped, rep.a2b.emitted),
            (1, 1, 0)
        );
        assert_eq!((rep.b2a.received, rep.b2a.emitted), (1, 1));
        // 捨てた通数は受けたポートの向きに数える。
        assert_eq!((rep.a2b.foreign, rep.b2a.foreign), (1, 2));
        assert_eq!(rep.ended, Ended::Stopped);
    }
}
