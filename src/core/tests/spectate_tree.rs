//! 観戦の中継の木の模型: ホスト（`SpectateHost`）・中継役（`Watcher` ＋ 中継の配る側 `SpectateHost::relay`）・子（`Watcher`）を、
//! 落とす・重ねる・順を入れ替えるネットワークで接続し、紹介・受け入れの依頼・付け替え・満杯の取り置きを見る。
//! ネットワークは 1 ms ごとに進む固定 seed の模型。中継役は dll の受信スレッドと同じ順（受ける → 組み立てから積む → `tend_relay` →
//! `poll`）で回す。紹介・依頼・断りのガードは、封じた便を直に組み立てて見る。上りの測り（MEASURE・PAIR）の模型
//! （`Sim::measured`）は、観戦者の上りを絞り、測りの組の到着を µs でホストの `on_pair` へ渡す。

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::bits512::Bits512;
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::rtt_probe::DelaySnapshot;
use higanr_core::seal::{DirKeys, Ephemeral, KeySeed, Sender, Seq, read_nonce_field};
use higanr_core::spectate::{
    ADMIT_TTL_MS, HEARTBEAT_MS, HostConfig, HostEvent, MAX_SPECTATORS, MEASURE_CLOSE_MS,
    MEASURE_PAIRS, MEASURE_RESEND_MS, MEASURE_RETRY_MS, MEASURE_SENDS, PAIR_CAP, PAIR_GAP_MS,
    PREPAID_MS, PUNCH_ROUNDS, REJOIN_MS, RELAY_CHILDREN, RELAY_HEARTBEAT_MS, RELAY_QUIET_MS,
    RESERVE_MS, RelayKey, SILENT_MS, SpectateHost, Watcher, WatcherConfig, leave_tree, tend_relay,
};
use higanr_core::spectate_rate::RELAY_MIN_BPS;
use higanr_core::wire::{
    self, ByeReason, HEADER_LEN, JoinTree, MAX_DATAGRAM, Message, PeekedType, Record, RelayState,
    Seat, SpectateAdmit, SpectateIntro, SpectateJoin, SpectatePair, SpectateStream,
};
use std::net::SocketAddr;

const HOST: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    48_000,
);
const SID: u32 = 0x7EE0_0001;
/// 中継役に向く上りの太さ。
const THICK: u32 = 2 * RELAY_MIN_BPS;
const REACHABLE: NatMark = NatMark {
    symmetry: Symmetry::No,
    upnp: false,
};
const PERIOD: u32 = 16;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 48_100 + n))
}

fn secret() -> Secret {
    Secret([0x3C; 16])
}

fn key() -> AuthKey {
    AuthKey::derive(&secret(), SID)
}

fn host_cfg() -> HostConfig {
    HostConfig {
        session_id: SID,
        secret: secret(),
        key_seed: KeySeed([0x48; 32]),
    }
}

fn seed(n: u16, salt: u8) -> KeySeed {
    let mut s = [0u8; 32];
    for (i, b) in s.iter_mut().enumerate() {
        *b = (n as u8).wrapping_mul(31) ^ (i as u8).wrapping_mul(7) ^ salt;
    }
    KeySeed(s)
}

/// 観戦者 `n` の使い捨ての鍵の種。
fn wseed(n: u16) -> KeySeed {
    seed(n, 0xA7)
}

/// 観戦者 `n` の中継用の鍵の種。
fn rseed(n: u16) -> KeySeed {
    seed(n, 0x5B)
}

fn nonce_of(n: u16) -> u64 {
    0xB000 + u64::from(n)
}

fn watcher(n: u16) -> Watcher {
    Watcher::new(WatcherConfig {
        session_id: SID,
        secret: secret(),
        nonce: nonce_of(n),
        name: Name::new(&format!("t{n}")),
        host: Candidates::from_addrs(&[HOST]).expect("loopback"),
        host_public: host_cfg().public(),
        key_seed: wseed(n),
    })
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct InFlight {
    at: u32,
    /// 着く時刻（µs。上りを絞った送り元の便だけ ms より細かい。測りの組の差に使う）。
    at_us: u64,
    seq: u64,
    from: SocketAddr,
    to: SocketAddr,
    bytes: Vec<u8>,
}

/// PAIR の通し番号から、落とすかを決める。
type PairDrop = fn(u32) -> bool;

struct Net {
    q: Vec<InFlight>,
    rng: Rng,
    seq: u64,
    base: u32,
    jitter: u32,
    drop_pct: u64,
    dup_pct: u64,
    /// 落ちた観戦者（この送り元・宛先の通信は全部落とす）。
    dead: Vec<SocketAddr>,
    /// 片方向だけ切った経路（送り元, 宛先）。
    cut: Vec<(SocketAddr, SocketAddr)>,
    /// ホストが中継役へ出す受け入れの依頼を、この数だけ先頭から落とす。
    drop_admits: u32,
    /// ホストの紹介を、この数だけ先頭から落とす。
    drop_intros: u32,
    /// 上りを絞った送り元（送り元, B/s, 上りが空く µs）。この送り元の便は上りに 1 通ずつ並び、ばらつき無しで送った順に着く。
    paced: Vec<(SocketAddr, u32, u64)>,
    /// この送り元の PAIR のうち、通し番号（0 から。送り直しの番も続けて数える）で真になるものを落とす。
    pair_drop: Option<(SocketAddr, PairDrop)>,
    pairs_seen: u32,
    dropped: u64,
    /// 「送った相手からだけ受ける」NAT の内側にいるアドレス。
    nat: Vec<SocketAddr>,
    /// NAT の穴（持ち主, 相手, 持ち主が最後に相手へ送った時刻）。NAT の内側かに依らず、送るたびに記録する。
    holes: Vec<(SocketAddr, SocketAddr, u32)>,
    /// 穴が最後の送りから開いている長さ（ms）。
    nat_ttl: u32,
    nat_dropped: u64,
}

impl Net {
    /// `owner` の NAT が `peer` からの便を通すか。
    fn nat_passes(&self, now: u32, owner: SocketAddr, peer: SocketAddr) -> bool {
        !self.nat.contains(&owner)
            || self
                .holes
                .iter()
                .any(|h| h.0 == owner && h.1 == peer && now.wrapping_sub(h.2) <= self.nat_ttl)
    }

    fn send(&mut self, now: u32, from: SocketAddr, to: SocketAddr, bytes: &[u8]) {
        match self.holes.iter_mut().find(|h| h.0 == from && h.1 == to) {
            Some(h) => h.2 = now,
            None => self.holes.push((from, to, now)),
        }
        if !self.nat_passes(now, to, from) {
            self.nat_dropped += 1;
            self.dropped += 1;
            return;
        }
        if self.dead.contains(&from) || self.dead.contains(&to) || self.cut.contains(&(from, to)) {
            self.dropped += 1;
            return;
        }
        if let Some((a, drop)) = self.pair_drop
            && a == from
            && wire::peek_type(bytes) == PeekedType::SpectatePair
        {
            let k = self.pairs_seen;
            self.pairs_seen += 1;
            if drop(k) {
                self.dropped += 1;
                return;
            }
        }
        if from == HOST
            && self.drop_admits > 0
            && wire::peek_type(bytes) == PeekedType::SpectateAdmit
        {
            self.drop_admits -= 1;
            self.dropped += 1;
            return;
        }
        if from == HOST
            && self.drop_intros > 0
            && wire::peek_type(bytes) == PeekedType::SpectateIntro
        {
            self.drop_intros -= 1;
            self.dropped += 1;
            return;
        }
        if self.rng.below(100) < self.drop_pct {
            self.dropped += 1;
            return;
        }
        let copies = if self.rng.below(100) < self.dup_pct {
            2
        } else {
            1
        };
        let paced = self.paced.iter_mut().find(|p| p.0 == from).map(|p| {
            let depart =
                (u64::from(now) * 1_000).max(p.2) + bytes.len() as u64 * 1_000_000 / u64::from(p.1);
            p.2 = depart;
            depart + u64::from(self.base) * 1_000
        });
        for _ in 0..copies {
            let at_us = paced.unwrap_or_else(|| {
                u64::from(now + self.base + self.rng.below(u64::from(self.jitter) + 1) as u32)
                    * 1_000
            });
            let at = at_us.div_ceil(1_000) as u32;
            self.seq += 1;
            self.q.push(InFlight {
                at,
                at_us,
                seq: self.seq,
                from,
                to,
                bytes: bytes.to_vec(),
            });
        }
    }
}

struct Tx<'a> {
    net: &'a mut Net,
    from: SocketAddr,
    now: u32,
}

impl Outlet for Tx<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.net.send(self.now, self.from, to, bytes);
    }
}

struct Node {
    n: u16,
    addr: SocketAddr,
    w: Watcher,
    relay: Option<SpectateHost>,
    uplink: u32,
    start_at: u32,
    /// 動いている（抜けた・落ちたら false）。
    live: bool,
    /// 抜けた・落ちた時刻。
    gone_at: Option<u32>,
    got: Vec<(u32, Record)>,
    /// 親を替えた時刻と、替えた後の親が元のホストか。
    moves: Vec<(u32, bool)>,
    epoch: u16,
}

struct Sim {
    now: u32,
    net: Net,
    host: SpectateHost,
    nodes: Vec<Node>,
    /// `PERIOD` ごとにホストが積む記録（None はその回は何も積まない。選択の画面・読み込みの間）。
    script: Vec<Option<Record>>,
    next_script: usize,
    log: Vec<Record>,
    events: Vec<(u32, SocketAddr, HostEvent)>,
    /// ホストの数える人数が、動き始めた観戦者（落ちてから `SILENT_MS` の内の人を含む）の数を越えた最初の時刻と（人数, 観戦者の数）。
    over: Option<(u32, usize, usize)>,
    /// 接続している生きた観戦者が `MAX_SPECTATORS` を越えた最初の時刻と数。
    cap: Option<(u32, usize)>,
    /// ホストが埋まっていると見る席（人数 ＋ 取り置き）が、接続している生きた観戦者より少なかった最初の時刻と（席, 観戦者の数）。
    under: Option<(u32, usize, usize)>,
    /// `under` で数えない観戦者（ホストが黙ったと見て外した間も生きている人）。
    excused: Vec<u16>,
    /// 中継役の下の子の親が、元のホストの直接の観戦者でなかった（木が 3 段になった）最初の時刻と子。
    deep: Option<(u32, u16)>,
    /// 上りを測りで決める（`note_uplink` を呼ばず、模型が観戦者の上りを絞って、PAIR をホストの `on_pair` へ渡す）。
    measured: bool,
    /// ホストが測りの番を渡した時刻と相手。
    turns: Vec<(u32, SocketAddr)>,
}

/// 台本: `Seg(0)`・名前 2 つ・マッチの語 `frames` 個・`End`。
fn script(rng: &mut Rng, frames: u32) -> Vec<Option<Record>> {
    let mut v = seg_head(0);
    push_frames(&mut v, rng, frames);
    v.push(Some(Record::End(ByeReason::Finished)));
    v
}

fn seg_head(seg: u16) -> Vec<Option<Record>> {
    vec![
        Some(Record::Seg { seg }),
        Some(Record::name(Seat::P1, b"p1")),
        Some(Record::name(Seat::P2, b"p2")),
    ]
}

fn push_frames(v: &mut Vec<Option<Record>>, rng: &mut Rng, frames: u32) {
    v.extend((0..frames).map(|f| {
        Some(Record::Frame {
            frame: f,
            p1: rng.next() as u16,
            p2: rng.next() as u16,
        })
    }));
}

/// 2 マッチの台本: マッチ 0 の語 `frames0` 個・`Seg(1)` と名前・`idle_ms` の間は何も積まない（マッチの切れ目）・マッチ 1 の語
/// `frames1` 個・`End`。
fn two_matches(seed: u64, frames0: u32, idle_ms: u32, frames1: u32) -> Vec<Option<Record>> {
    let mut rng = Rng(seed ^ 0x7EE);
    let mut v = seg_head(0);
    push_frames(&mut v, &mut rng, frames0);
    v.extend(seg_head(1));
    v.extend((0..idle_ms / PERIOD).map(|_| None));
    push_frames(&mut v, &mut rng, frames1);
    v.push(Some(Record::End(ByeReason::Finished)));
    v
}

/// 台本 `v` の `Seg(1)` を積む時刻と、その後の何も積まない間が終わる時刻。
fn idle_window(v: &[Option<Record>]) -> (u32, u32) {
    let seg = v
        .iter()
        .position(|r| matches!(r, Some(Record::Seg { seg: 1 })))
        .expect("Seg(1)");
    let end = (seg..v.len())
        .find(|&k| matches!(v[k], Some(Record::Frame { .. })))
        .expect("マッチ 1");
    ((seg as u32 + 1) * PERIOD, (end as u32 + 1) * PERIOD)
}

/// 乱れ: 片道の最小・ばらつき・落とす %・重ねる %。
#[derive(Clone, Copy)]
struct Chaos {
    base: u32,
    jitter: u32,
    drop_pct: u64,
    dup_pct: u64,
}

const LOSSY: Chaos = Chaos {
    base: 5,
    jitter: 25,
    drop_pct: 10,
    dup_pct: 5,
};

/// 落とさない回線（BYE・REHOME は 1 通きりで、落ちたら子が `REJOIN_MS` の待ちに戻る。その速さを見る検査に使う）。
const CALM: Chaos = Chaos {
    base: 5,
    jitter: 25,
    drop_pct: 0,
    dup_pct: 5,
};

impl Sim {
    fn new(seed: u64, c: Chaos, frames: u32) -> Sim {
        let mut rng = Rng(seed ^ 0x7EE);
        Sim {
            now: 0,
            net: Net {
                q: Vec::new(),
                rng: Rng(seed),
                seq: 0,
                base: c.base,
                jitter: c.jitter,
                drop_pct: c.drop_pct,
                dup_pct: c.dup_pct,
                dead: Vec::new(),
                cut: Vec::new(),
                drop_admits: 0,
                drop_intros: 0,
                paced: Vec::new(),
                pair_drop: None,
                pairs_seen: 0,
                dropped: 0,
                nat: Vec::new(),
                holes: Vec::new(),
                nat_ttl: u32::MAX,
                nat_dropped: 0,
            },
            host: SpectateHost::with_capacity(host_cfg(), 16_384),
            nodes: Vec::new(),
            script: script(&mut rng, frames),
            next_script: 0,
            log: Vec::new(),
            events: Vec::new(),
            over: None,
            cap: None,
            under: None,
            excused: Vec::new(),
            deep: None,
            measured: false,
            turns: Vec::new(),
        }
    }

    /// 台本 `script` の模型。
    fn with_script(seed: u64, c: Chaos, script: Vec<Option<Record>>) -> Sim {
        Sim {
            script,
            ..Sim::new(seed, c, 0)
        }
    }

    /// 上りを測りで決める模型。`add` の `uplink` は、その観戦者の上りの太さ（絞る）になる。
    fn measured(seed: u64, c: Chaos, frames: u32) -> Sim {
        Sim {
            measured: true,
            ..Sim::new(seed, c, frames)
        }
    }

    /// 観戦者 `n` を `start_at` ms から動かす。`relay` なら中継でき、上り `uplink` を持つ。
    fn add(&mut self, n: u16, start_at: u32, relay: bool, uplink: u32) {
        let mut w = watcher(n);
        w.set_nat(REACHABLE);
        let relay = relay.then(|| {
            let k = RelayKey::new(&rseed(n));
            w.set_relay_public(Some(k.public()));
            SpectateHost::relay_with_capacity(SID, secret(), k, 16_384)
        });
        if self.measured && uplink > 0 {
            self.net.paced.push((addr(n), uplink, 0));
        }
        self.nodes.push(Node {
            n,
            addr: addr(n),
            w,
            relay,
            uplink,
            start_at,
            live: true,
            gone_at: None,
            got: Vec::new(),
            moves: Vec::new(),
            epoch: 0,
        });
    }

    /// 中継できる太い観戦者 1〜`count` を、`gap` ms おきに動かす。
    fn add_tree(&mut self, count: u16, gap: u32) {
        for n in 1..=count {
            self.add(n, u32::from(n) * gap, true, THICK);
        }
    }

    fn node(&self, n: u16) -> &Node {
        self.nodes.iter().find(|x| x.n == n).expect("観戦者")
    }

    fn node_mut(&mut self, n: u16) -> &mut Node {
        self.nodes.iter_mut().find(|x| x.n == n).expect("観戦者")
    }

    /// 観戦者 `n` を落とす（強制終了。以後の通信は届かない）。
    fn crash(&mut self, n: u16) {
        let (a, now) = (addr(n), self.now);
        self.net.dead.push(a);
        let x = self.node_mut(n);
        x.live = false;
        x.gone_at = Some(now);
    }

    /// 観戦者 `n` が観戦を終える（ホストへ BYE・子へ REHOME。送った便は届く）。
    fn leave(&mut self, n: u16) {
        let now = self.now;
        let Sim { net, nodes, .. } = self;
        let node = nodes.iter_mut().find(|x| x.n == n).expect("観戦者");
        let mut tx = Tx {
            net,
            from: node.addr,
            now,
        };
        leave_tree(
            &mut node.w,
            node.relay.as_mut(),
            ByeReason::Cancelled,
            &mut tx,
        );
        node.live = false;
        node.gone_at = Some(now);
    }

    fn tick(&mut self) {
        self.now += 1;
        let now = self.now;
        if self.next_script < self.script.len() && now.is_multiple_of(PERIOD) {
            if let Some(rec) = self.script[self.next_script] {
                let idx = self.host.push(rec).expect("積める");
                assert_eq!(idx as usize, self.log.len());
                self.log.push(rec);
            }
            self.next_script += 1;
        }
        let mut q = std::mem::take(&mut self.net.q);
        q.sort_by_key(|p| (p.at, p.seq));
        let (due, later): (Vec<_>, Vec<_>) = q.into_iter().partition(|p| p.at <= now);
        self.net.q = later;
        for p in due {
            if p.to == HOST
                && self.measured
                && wire::peek_type(&p.bytes) == PeekedType::SpectatePair
            {
                self.host.on_pair(p.from, &p.bytes, p.at_us);
                continue;
            }
            if p.to == HOST {
                let mut tx = Tx {
                    net: &mut self.net,
                    from: HOST,
                    now,
                };
                let ev = self.host.on_datagram(p.from, &p.bytes, now, &mut tx);
                self.events.push((now, p.from, ev));
                // 測りの模型でなければ、接続した直後に上りを渡す（木の検査を測りから切り離す）。
                if ev == HostEvent::Joined
                    && !self.measured
                    && let Some(n) = self.nodes.iter().find(|x| x.addr == p.from)
                {
                    self.host.note_uplink(p.from, n.uplink);
                }
                continue;
            }
            let Some(node) = self
                .nodes
                .iter_mut()
                .find(|x| x.addr == p.to && x.live && now >= x.start_at)
            else {
                continue;
            };
            let mut tx = Tx {
                net: &mut self.net,
                from: node.addr,
                now,
            };
            // 受信の振り分け: JOIN・ACK は配る側、STREAM・INTRO・ADMIT・REHOME は観戦者の側、BYE は送り元で分ける。
            let to_relay = node.relay.is_some()
                && match wire::peek_type(&p.bytes) {
                    PeekedType::SpectateJoin | PeekedType::SpectateAck => true,
                    PeekedType::Bye => p.from != HOST && Some(p.from) != node.w.host_addr(),
                    _ => false,
                };
            match node.relay.as_mut() {
                Some(r) if to_relay => {
                    r.on_datagram(p.from, &p.bytes, now, &mut tx);
                }
                _ => node.w.on_datagram(p.from, &p.bytes, now, &mut tx),
            }
        }
        {
            let mut tx = Tx {
                net: &mut self.net,
                from: HOST,
                now,
            };
            let before = self.host.measuring();
            self.host.poll(now, &mut tx);
            if let Some(a) = self.host.measuring()
                && before != Some(a)
            {
                self.turns.push((now, a));
            }
        }
        for node in self.nodes.iter_mut() {
            if !node.live || now < node.start_at {
                continue;
            }
            let mut tx = Tx {
                net: &mut self.net,
                from: node.addr,
                now,
            };
            node.w.poll(now, &mut tx);
            while let Some((idx, rec)) = node.w.pop_indexed() {
                node.got.push((idx, rec));
                if let Some(r) = node.relay.as_mut() {
                    r.push_at(idx, rec);
                }
            }
            if let Some(r) = node.relay.as_mut() {
                tend_relay(&mut node.w, r, now, &mut tx);
                r.poll(now, &mut tx);
            }
            if node.w.epoch() != node.epoch {
                node.epoch = node.w.epoch();
                node.moves.push((now, node.w.parent_is_home()));
            }
        }
        let started = self
            .nodes
            .iter()
            .filter(|x| {
                now >= x.start_at
                    && (x.live || x.gone_at.is_some_and(|g| now - g <= SILENT_MS + 500))
            })
            .count();
        let count = self.host.spectator_count();
        if count > started && self.over.is_none() {
            self.over = Some((now, count, started));
        }
        let joined = self
            .nodes
            .iter()
            .filter(|x| x.live && x.w.is_joined())
            .count();
        if joined > MAX_SPECTATORS && self.cap.is_none() {
            self.cap = Some((now, joined));
        }
        let joined = joined - self.excused.iter().filter(|&&n| self.node(n).live).count();
        let held = count + self.host.reserved(now);
        if held < joined && self.under.is_none() {
            self.under = Some((now, held, joined));
        }
        if now.is_multiple_of(50) && self.deep.is_none() {
            self.deep = self.find_deep().map(|n| (now, n));
        }
    }

    /// 親が中継役で、その中継役の親も元のホストでない観戦者（3 段目）。
    fn find_deep(&self) -> Option<u16> {
        self.nodes.iter().filter(|x| x.live).find_map(|x| {
            let parent = x.w.host_addr().filter(|_| !x.w.parent_is_home())?;
            let p = self.nodes.iter().find(|y| y.addr == parent)?;
            (!p.w.parent_is_home()).then_some(x.n)
        })
    }

    fn run_to(&mut self, t: u32) {
        while self.now < t {
            self.tick();
        }
    }

    fn caught_up(&self) -> bool {
        self.next_script == self.script.len()
            && self
                .nodes
                .iter()
                .filter(|x| x.live && x.w.is_joined())
                .all(|x| x.got.last().map(|g| g.0 + 1) == Some(self.host.tail()))
    }

    fn run_until_caught_up(&mut self, limit: u32) {
        while !self.caught_up() {
            assert!(
                self.now < limit,
                "{limit} ms で追いつかなかった: {:?}",
                self.nodes
                    .iter()
                    .map(|x| (x.n, x.got.len(), x.live))
                    .collect::<Vec<_>>()
            );
            self.tick();
        }
    }

    /// 観戦者 `n` の列が、ホストの列の `from` から積んだ終わりまで、飛ばずに一致する。
    fn assert_column(&self, n: u16, from: u32) {
        let x = self.node(n);
        let tail = self.host.tail() as usize;
        assert_eq!(
            x.got.len(),
            tail - from as usize,
            "観戦者 {n}: 受け取った数（{from} から {tail} の手前まで）"
        );
        for (k, (idx, rec)) in x.got.iter().enumerate() {
            assert_eq!(*idx as usize, from as usize + k, "観戦者 {n}: 番号が飛んだ");
            assert_eq!(
                *rec, self.log[*idx as usize],
                "観戦者 {n}: 番号 {idx} が違う"
            );
        }
        assert_eq!(x.w.assembler_counts().jumps, 0, "観戦者 {n}: 飛んだ");
    }

    /// 走った間ずっと、接続している観戦者が上限以下で、ホストが席をその数以上と見ていた。
    fn assert_counts(&self) {
        assert_eq!(self.cap, None, "接続している観戦者が上限を越えた");
        assert_eq!(
            self.under, None,
            "ホストが接続している観戦者より少なく数えた"
        );
    }

    /// 中継役 `n` の子（親が `n` の観戦者）。
    fn children_of(&self, n: u16) -> Vec<u16> {
        let a = addr(n);
        self.nodes
            .iter()
            .filter(|x| x.live && !x.w.parent_is_home() && x.w.host_addr() == Some(a))
            .map(|x| x.n)
            .collect()
    }

    fn events_of(&self, n: u16) -> Vec<(u32, HostEvent)> {
        let a = addr(n);
        self.events
            .iter()
            .filter(|e| e.1 == a)
            .map(|e| (e.0, e.2))
            .collect()
    }
}

/// 10 人の木（中継役 2 人・子 4 人ずつ）ができた模型（観戦者 `k` は `k × 200` ms に入る。落としで入る順が入れ替わるので、
/// 中継役は形で見る）。
fn ten_tree(seed: u64, c: Chaos, frames: u32) -> Sim {
    let mut sim = Sim::new(seed, c, frames);
    sim.add_tree(10, 200);
    sim.run_to(4_500);
    let shape: Vec<usize> = sim.relays().iter().map(|r| r.1.len()).collect();
    assert_eq!(shape, vec![4, 4], "seed {seed:#x}: 木の形");
    sim
}

impl Sim {
    /// 子を持つ中継役（子の多い順）と、その子。
    fn relays(&self) -> Vec<(u16, Vec<u16>)> {
        let mut v: Vec<(u16, Vec<u16>)> = self
            .nodes
            .iter()
            .filter(|x| x.live && x.w.parent_is_home())
            .map(|x| (x.n, self.children_of(x.n)))
            .filter(|r| !r.1.is_empty())
            .collect();
        v.sort_by_key(|r| (std::cmp::Reverse(r.1.len()), r.0));
        v
    }
}

#[test]
fn ten_spectators_form_two_relays_with_four_children_each_and_get_the_hosts_column() {
    for seed in [0x51, 0x6262] {
        let mut sim = ten_tree(seed, LOSSY, 1_200);
        let relays: Vec<u16> = sim.relays().iter().map(|r| r.0).collect();
        for &r in &relays {
            assert!(sim.host.is_relay_of(addr(r)), "seed {seed:#x}: {r}");
        }
        assert_eq!(sim.host.spectator_count(), MAX_SPECTATORS);
        assert_eq!(sim.host.pending_intros(), 0);
        // ホストが直接送るのは中継役 2 人だけ。
        let direct: Vec<SocketAddr> = sim.host.spectators().map(|s| s.0).collect();
        let mut want: Vec<SocketAddr> = relays.iter().map(|&r| addr(r)).collect();
        want.sort();
        let mut direct_sorted = direct.clone();
        direct_sorted.sort();
        assert_eq!(direct_sorted, want, "seed {seed:#x}");
        // 11 人目は断られる。
        sim.add(11, sim.now + 1, false, 0);
        sim.run_until_caught_up(60_000);
        sim.assert_counts();
        assert!(!sim.node(11).w.is_joined());
        assert!(
            sim.events_of(11).iter().any(|e| e.1 == HostEvent::Refused),
            "11 人目が断られていない"
        );
        assert_eq!(sim.node(11).w.host_bye(), Some(ByeReason::Cancelled));
        for n in 1..=10 {
            sim.assert_column(n, 0);
        }
        assert_eq!(sim.over, None, "人数が動いている観戦者より多かった");
        assert_eq!(sim.deep, None);
        let c = sim.host.counts();
        assert!(sim.net.dropped > 0 && c.resends > 0, "落としが効いていない");
        assert!(c.intros >= 8, "紹介の数 {}", c.intros);
    }
}

#[test]
fn the_nat_model_passes_only_a_peer_the_owner_sent_to_and_forgets_it_after_the_ttl() {
    let mut sim = Sim::new(1, CALM, 10);
    let (a, b) = (addr(1), addr(2));
    sim.net.nat = vec![a];
    sim.net.nat_ttl = 100;
    sim.net.send(10, b, a, b"x");
    assert_eq!(sim.net.nat_dropped, 1, "送っていない相手の便を通した");
    sim.net.send(20, a, b, b"x");
    sim.net.send(30, b, a, b"x");
    assert_eq!(sim.net.nat_dropped, 1, "送った相手の便を通さなかった");
    sim.net.send(200, b, a, b"x");
    assert_eq!(sim.net.nat_dropped, 2, "古い穴を通した");
}

/// 中継役が「送った相手からだけ受ける」NAT の内側にいると、子の JOIN は中継役が子の候補へ穴あけを送るまで捨てられる。
#[test]
fn behind_a_restricted_nat_the_relay_punches_to_its_children_so_the_tree_forms() {
    for seed in [0x51, 0x6262] {
        let mut sim = Sim::new(seed, LOSSY, 1_200);
        sim.net.nat = (1..=10).map(addr).collect();
        sim.add_tree(10, 200);
        sim.run_to(4_500);
        let shape: Vec<usize> = sim.relays().iter().map(|r| r.1.len()).collect();
        assert_eq!(shape, vec![4, 4], "seed {seed:#x}: 木の形");
        for n in 1..=10 {
            assert_eq!(
                sim.node(n).w.tree_counts().unreached,
                0,
                "seed {seed:#x}: 観戦者 {n} が中継役へ届かず戻った"
            );
        }
        sim.run_until_caught_up(60_000);
        sim.assert_counts();
        for n in 1..=10 {
            sim.assert_column(n, 0);
        }
        assert!(sim.host.counts().intros >= 8);
        let sent: u64 = sim
            .nodes
            .iter()
            .filter_map(|x| x.relay.as_ref())
            .map(|r| r.counts().punches_sent)
            .sum();
        // 子の JOIN が通ったら止める（止めなければ 8 人 × `PUNCH_ROUNDS` 通）。
        assert!(
            (8..8 * u64::from(PUNCH_ROUNDS) / 2).contains(&sent),
            "seed {seed:#x}: 中継役が送った穴あけ {sent} 通"
        );
    }
}

/// 木ができた後にホストが NAT の内側になった形（子へはもう何も送っていない）で、中継役が子を残して消えたとき、戻る子の JOIN が
/// 通る。穴は最後の送りから `HOLE_TTL` だけ開く。
const HOLE_TTL: u32 = 2_000;

fn seven_tree_behind_host_nat(seed: u64, frames: u32) -> (Sim, u16, Vec<u16>) {
    let mut sim = seven_tree(seed, CALM, frames);
    let (a, kids) = sim.relays()[0].clone();
    sim.net.nat_ttl = HOLE_TTL;
    sim.net.nat.push(HOST);
    sim.run_to(sim.now + 2 * HOLE_TTL);
    (sim, a, kids)
}

/// 子 `kids` の JOIN が `t` から `within` ms の内にホストへ届き（その後ほかの中継役へ回ってもよい）、`gone` の下には誰もいない。
fn assert_kids_reach_the_host(sim: &Sim, kids: &[u16], t: u32, within: u32, gone: u16) {
    for &n in kids {
        let first = sim
            .events_of(n)
            .into_iter()
            .find(|e| {
                e.0 > t
                    && matches!(
                        e.1,
                        HostEvent::Joined | HostEvent::Introduced | HostEvent::Refreshed
                    )
            })
            .map(|e| e.0 - t);
        assert!(
            first.is_some_and(|d| d <= within),
            "子 {n} の JOIN がホストへ届くまで {first:?} ms（上限 {within}。NAT で捨てられた {} 通）",
            sim.net.nat_dropped
        );
    }
    assert_eq!(sim.children_of(gone), Vec::<u16>::new());
}

#[test]
fn a_leaving_relays_children_get_through_the_hosts_nat_because_the_host_punches_to_them() {
    let (mut sim, a, kids) = seven_tree_behind_host_nat(0x31, 900);
    let t = sim.now;
    sim.leave(a);
    sim.run_to(t + REJOIN_MS);
    assert_kids_reach_the_host(&sim, &kids, t, 1_000, a);
    // 子の JOIN が通ったら止める（止めなければ 4 人 × `PUNCH_ROUNDS` 通）。
    let sent = sim.host.counts().punches_sent;
    assert!(
        (kids.len() as u64..2 * kids.len() as u64 + 1).contains(&sent),
        "ホストが送った穴あけ {sent} 通"
    );
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in (1..=7).filter(|&n| n != a) {
        sim.assert_column(n, 0);
    }
}

#[test]
fn a_crashed_relays_children_get_through_the_hosts_nat_because_the_host_punches_to_them_when_it_goes_quiet()
 {
    let (mut sim, a, kids) = seven_tree_behind_host_nat(0x11, 1_400);
    let t = sim.now;
    sim.crash(a);
    sim.run_to(t + REJOIN_MS + 1_500);
    assert_kids_reach_the_host(&sim, &kids, t, REJOIN_MS + 1_000, a);
    assert!(sim.host.counts().punches_sent > 0);
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in (1..=7).filter(|&n| n != a) {
        sim.assert_column(n, 0);
    }
}

#[test]
fn the_count_follows_joins_leaves_and_returns() {
    let mut sim = seven_tree(0x77, CALM, 1_500);
    assert_eq!(sim.host.spectator_count(), 7, "入った後");
    let (big, small) = (sim.relays()[0].clone(), sim.relays()[1].clone());
    sim.leave(small.1[0]);
    sim.run_to(5_000);
    assert_eq!(sim.host.spectator_count(), 6, "子が抜けた後");
    sim.leave(big.0);
    sim.run_to(8_000);
    assert_eq!(sim.host.spectator_count(), 5, "中継役が抜けて子が戻った後");
    assert_eq!(sim.host.reserved(sim.now), 0);
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for &n in big.1.iter().chain([small.0].iter()) {
        sim.assert_column(n, 0);
    }
    assert_eq!(sim.over, None);
}

/// 中継役 2 人（子 4 人と 1 人）の木ができた模型。返すのは（子 4 人の中継役, その子）。
fn seven_tree(seed: u64, c: Chaos, frames: u32) -> Sim {
    let mut sim = Sim::new(seed, c, frames);
    sim.add_tree(7, 200);
    sim.run_to(3_000);
    let shape: Vec<usize> = sim.relays().iter().map(|r| r.1.len()).collect();
    assert_eq!(shape, vec![4, 1], "seed {seed:#x}: 木の形");
    sim
}

/// 子 `kids` が元のホストへ戻った時刻（`at` からの ms）。
fn return_delays(sim: &Sim, kids: &[u16], at: u32) -> Vec<u32> {
    kids.iter()
        .map(|&n| {
            let m = sim.node(n).moves.iter().find(|m| m.0 > at && m.1);
            m.map_or(u32::MAX, |m| m.0 - at)
        })
        .collect()
}

#[test]
fn a_crashed_relays_children_return_after_rejoin_ms_and_their_column_does_not_skip() {
    for seed in [0x11, 0x2323] {
        let mut sim = seven_tree(seed, LOSSY, 1_400);
        let (a, kids) = sim.relays()[0].clone();
        let t = sim.now;
        sim.crash(a);
        sim.run_to(t + SILENT_MS + 1_000);
        for d in return_delays(&sim, &kids, t) {
            assert!(
                (REJOIN_MS - 300..=REJOIN_MS + 100).contains(&d),
                "seed {seed:#x}: 戻るまで {d} ms"
            );
        }
        for &n in &kids {
            assert_eq!(sim.node(n).w.tree_counts().parent_silent, 1);
        }
        assert_eq!(sim.host.spectator_count(), 6, "落ちた中継役を外した後");
        sim.run_until_caught_up(60_000);
        sim.assert_counts();
        for n in (1..=7).filter(|&n| n != a) {
            sim.assert_column(n, 0);
        }
        assert_eq!(sim.over, None);
        assert_eq!(sim.deep, None);
    }
}

#[test]
fn a_leaving_relays_rehome_brings_its_children_back_sooner_than_rejoin_ms() {
    let mut sim = seven_tree(0x31, CALM, 900);
    let (a, kids) = sim.relays()[0].clone();
    let t = sim.now;
    sim.leave(a);
    sim.run_to(t + 2_000);
    for d in return_delays(&sim, &kids, t) {
        assert!(d < REJOIN_MS / 3, "戻るまで {d} ms");
    }
    for &n in &kids {
        assert_eq!(sim.node(n).w.tree_counts().rehomes, 1);
    }
    assert_eq!(
        sim.host.counts().reserved_used,
        4,
        "戻る子が取り置きを使っていない"
    );
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in (1..=7).filter(|&n| n != a) {
        sim.assert_column(n, 0);
    }
}

#[test]
fn cutting_only_host_to_relay_makes_the_relay_rehome_its_children() {
    let mut sim = seven_tree(0x41, CALM, 1_000);
    let (a, kids) = sim.relays()[0].clone();
    let t = sim.now;
    sim.net.cut.push((HOST, addr(a)));
    sim.run_to(t + REJOIN_MS + 1_000);
    for &n in &kids {
        let c = sim.node(n).w.tree_counts();
        assert_eq!((c.rehomes, c.parent_silent), (1, 0), "子 {n}");
    }
    for d in return_delays(&sim, &kids, t) {
        assert!(
            (REJOIN_MS - 300..=REJOIN_MS + 300).contains(&d),
            "戻るまで {d} ms"
        );
    }
    sim.net.cut.clear();
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in 1..=7 {
        sim.assert_column(n, 0);
    }
    assert_eq!(sim.deep, None);
}

/// 中継役の上りだけを切ると、中継役の流れが止まり、子には `RELAY_HEARTBEAT_MS` ごとの生きている合図しか届かない（続けて落ちると子は
/// `REJOIN_MS` で戻る）。子が付いたままの形を見るので、落とさない回線で回す。ホストが外した間の中継役は生きているが、ホストは数えない（入り直しで
/// 満杯なら断られる）。
#[test]
fn a_relay_the_host_dropped_comes_back_directly_while_it_still_has_children() {
    let mut sim = seven_tree(0x61, CALM, 1_700);
    let (a, kids) = sim.relays()[0].clone();
    let t = sim.now;
    sim.excused.push(a);
    sim.net.cut.push((addr(a), HOST));
    sim.run_to(t + SILENT_MS + 1_000);
    assert_eq!(
        sim.host.ack_of(addr(a)),
        None,
        "ホストが中継役を外していない"
    );
    assert_eq!(sim.children_of(a), kids, "外される前に子が離れた");
    sim.net.cut.clear();
    sim.run_to(t + SILENT_MS + 5_000);
    let back: Vec<HostEvent> = sim
        .events_of(a)
        .into_iter()
        .filter(|e| e.0 > t + SILENT_MS)
        .map(|e| e.1)
        .filter(|e| matches!(e, HostEvent::Joined | HostEvent::Introduced))
        .collect();
    assert_eq!(back, vec![HostEvent::Joined], "子がいる中継役は直接受ける");
    let w = &sim.node(a).w;
    assert!(w.parent_is_home() && w.tree_counts().intros_taken == 0);
    assert_eq!(sim.deep, None, "木が 3 段になった");
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in 1..=7 {
        sim.assert_column(n, 0);
    }
}

#[test]
fn a_child_that_cannot_reach_its_relay_returns_direct_only_and_is_taken_directly() {
    let mut sim = Sim::new(0x71, LOSSY, 700);
    sim.add(1, 100, true, THICK);
    sim.add(2, 600, true, THICK);
    sim.net.cut.push((addr(2), addr(1)));
    sim.net.cut.push((addr(1), addr(2)));
    sim.run_to(600 + REJOIN_MS + 1_500);
    let x = sim.node(2);
    assert!(x.w.parent_is_home() && x.w.direct_only());
    assert_eq!(x.w.tree_counts().unreached, 1);
    assert_eq!(
        x.moves.iter().map(|m| m.1).collect::<Vec<_>>(),
        vec![false, true]
    );
    assert!(sim.host.ack_of(addr(2)).is_some(), "直接受けていない");
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    sim.assert_column(1, 0);
    sim.assert_column(2, 0);
}

#[test]
fn a_lost_admit_is_resent_until_the_relay_takes_it() {
    let mut sim = Sim::new(0x81, LOSSY, 500);
    sim.add(1, 100, true, THICK);
    sim.add(2, 600, true, THICK);
    sim.net.drop_admits = 2;
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    assert_eq!(sim.net.drop_admits, 0, "依頼を落とせていない");
    assert!(sim.host.counts().admit_resends >= 2);
    assert_eq!(sim.children_of(1), vec![2]);
    sim.assert_column(2, 0);
}

#[test]
fn when_full_a_crashed_relays_children_get_back_in_before_a_newcomer() {
    let mut sim = ten_tree(0x91, LOSSY, 1_500);
    let (a, kids) = sim.relays()[0].clone();
    let t = sim.now;
    sim.crash(a);
    sim.add(11, t + 1, false, 0);
    sim.run_to(t + REJOIN_MS + 2_000);
    for &n in &kids {
        assert!(sim.node(n).w.parent_is_home(), "子 {n} が戻っていない");
        assert!(sim.host.ack_of(addr(n)).is_some(), "子 {n} が入れていない");
    }
    assert!(
        !sim.node(11).w.is_joined(),
        "新しい人が戻る子より先に入った"
    );
    assert!(sim.events_of(11).iter().any(|e| e.1 == HostEvent::Refused));
    // 落ちた中継役を外せば、新しい人も入れる。
    sim.run_to(t + SILENT_MS + 2_000);
    assert!(sim.node(11).w.is_joined());
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in (1..=11).filter(|&n| n != a) {
        sim.assert_column(n, 0);
    }
}

/// 中継役の子が中継役へ届かなくなってホストへ戻った（中継役はまだその子の枠を持つ）後に、新しい人が続けて来る。
#[test]
fn newcomers_after_a_child_left_a_relay_that_still_holds_it_never_pass_ten() {
    let mut sim = seven_tree(0xC1, CALM, 1_700);
    let (a, kids) = sim.relays()[0].clone();
    let k = kids[0];
    let t = sim.now;
    sim.net.cut.push((addr(a), addr(k)));
    sim.net.cut.push((addr(k), addr(a)));
    sim.run_to(t + REJOIN_MS + 500);
    let w = &sim.node(k).w;
    assert!(
        w.epoch() >= 2 && w.host_addr() != Some(addr(a)),
        "子が戻っていない"
    );
    assert_eq!(sim.host.spectator_count(), 7);
    for (i, n) in (8..=12u16).enumerate() {
        sim.add(n, sim.now + 1 + 300 * i as u32, false, 0);
    }
    sim.run_to(t + SILENT_MS + 3_000);
    let newcomers = (8..=12).filter(|&n| sim.node(n).w.is_joined()).count();
    assert_eq!(newcomers, 3, "新しい人が上限まで入っていない・越えた");
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in (1..=12).filter(|&n| sim.node(n).w.is_joined()) {
        sim.assert_column(n, 0);
    }
}

/// 中継役の BYE が子の JOIN より後に着く順（REHOME を先に送り、BYE を 150 ms 遅らせる）でも、満杯の木で戻る子は断られず、元の中継役へは
/// 回されない。新しい人は中継役の BYE で空いた 1 席に入る。
#[test]
fn when_full_children_whose_join_beats_the_relays_bye_still_get_back_in() {
    let mut sim = ten_tree(0xB1, CALM, 1_100);
    let (a, kids) = sim.relays()[0].clone();
    let t = sim.now;
    {
        let Sim { net, nodes, .. } = &mut sim;
        let node = nodes.iter_mut().find(|x| x.n == a).expect("中継役");
        let mut tx = Tx {
            net,
            from: node.addr,
            now: t,
        };
        node.relay.as_mut().expect("配る側").stop_relay(&mut tx);
    }
    sim.add(11, t + 1, false, 0);
    sim.run_to(t + 150);
    assert!(sim.host.ack_of(addr(a)).is_some(), "中継役がもう外れた");
    let early = kids
        .iter()
        .filter(|&&n| {
            sim.events_of(n)
                .iter()
                .any(|e| e.0 > t && matches!(e.1, HostEvent::Joined | HostEvent::Introduced))
        })
        .count();
    assert_eq!(
        early,
        kids.len(),
        "子の JOIN が中継役の BYE より先に着いていない"
    );
    sim.leave(a);
    sim.run_to(t + 1_500);
    for &n in &kids {
        let w = &sim.node(n).w;
        assert!(!w.refused_after_join(), "子 {n} が断られた");
        assert_ne!(
            w.host_addr(),
            Some(addr(a)),
            "子 {n} が元の中継役へ回された"
        );
        assert_eq!(w.tree_counts().unreached, 0, "子 {n}");
    }
    assert!(sim.node(11).w.is_joined());
    assert_eq!(sim.host.spectator_count(), MAX_SPECTATORS);
    assert_eq!(sim.host.counts().reserved_used, 4);
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in (1..=11).filter(|&n| n != a) {
        sim.assert_column(n, 0);
    }
}

#[test]
fn a_reservation_expires_after_reserve_ms_and_lets_a_newcomer_in() {
    let mut sim = ten_tree(0xA1, CALM, 1_100);
    let (a, kids) = sim.relays()[0].clone();
    let t = sim.now;
    // 中継役は抜け、その子は落ちる（誰も戻らない）。取り置きの 4 席で、新しい人は 1 人しか入れない。
    for &n in &kids {
        sim.crash(n);
    }
    sim.leave(a);
    sim.add(11, t + 1, false, 0);
    sim.add(12, t + 1, false, 0);
    sim.run_to(t + 1_000);
    assert_eq!(sim.host.reserved(sim.now), 4);
    let joined_at = |sim: &Sim, n: u16| {
        sim.events_of(n)
            .iter()
            .find(|e| e.1 == HostEvent::Joined)
            .map_or(u32::MAX, |e| e.0 - t)
    };
    sim.run_to(t + RESERVE_MS + 2_000);
    let (x, y) = (joined_at(&sim, 11), joined_at(&sim, 12));
    let (first, second) = (x.min(y), x.max(y));
    assert!(first < 1_000, "1 人目が入れない: {x} {y}");
    assert!(
        (RESERVE_MS..RESERVE_MS + 1_000).contains(&second),
        "2 人目は取り置きが切れてから: {x} {y}"
    );
    assert_eq!(sim.host.spectator_count(), 7);
    sim.assert_counts();
}

// ============================ 入れ替え・照合のずれで戻る・中継の合図 ============================

impl Sim {
    /// 観戦者 `n` の照合がずれた（`Watcher::return_on_mismatch`）。
    fn mismatch(&mut self, n: u16) -> bool {
        let now = self.now;
        let Sim { net, nodes, .. } = self;
        let node = nodes.iter_mut().find(|x| x.n == n).expect("観戦者");
        let mut tx = Tx {
            net,
            from: node.addr,
            now,
        };
        node.w.return_on_mismatch(now, &mut tx)
    }

    /// 観戦者 `n` が木の欄の変化を元のホストへ知らせる（`Watcher::announce`）。
    fn announce(&mut self, n: u16) -> bool {
        let now = self.now;
        let Sim { net, nodes, .. } = self;
        let node = nodes.iter_mut().find(|x| x.n == n).expect("観戦者");
        let mut tx = Tx {
            net,
            from: node.addr,
            now,
        };
        node.w.announce(&mut tx)
    }

    /// 子が親を離れた回数（親の沈黙・REHOME）の合計。
    fn returns(&self) -> u64 {
        self.nodes
            .iter()
            .map(|x| {
                let c = x.w.tree_counts();
                c.parent_silent + c.rehomes
            })
            .sum()
    }
}

/// 中継しない葉 1〜3 が先に直接入り、中継できる 4 が後から来る（4 は誰の下にも回らない）。マッチ 0 の後に `idle_ms` の切れ目。
fn leaves_then_relay(seed: u64, c: Chaos, idle_ms: u32) -> Sim {
    let mut sim = Sim::with_script(seed, c, two_matches(seed, 300, idle_ms, 300));
    for n in 1..=3 {
        sim.add(n, u32::from(n) * 1_000 - 800, false, 0);
    }
    sim.add(4, 3_200, true, THICK);
    sim
}

#[test]
fn at_the_match_break_direct_leaves_move_under_the_relay_and_their_column_does_not_skip() {
    for seed in [0x5A, 0x6B6B] {
        let mut sim = leaves_then_relay(seed, LOSSY, 6_000);
        let (_, end) = idle_window(&sim.script);
        sim.run_to(end);
        assert_eq!(sim.children_of(4), vec![1, 2], "seed {seed:#x}");
        let mut direct: Vec<SocketAddr> = sim.host.spectators().map(|s| s.0).collect();
        direct.sort();
        assert_eq!(
            direct,
            vec![addr(3), addr(4)],
            "seed {seed:#x}: 直接は 2 人"
        );
        let c = sim.host.counts();
        assert_eq!(
            (c.swaps, c.swap_freed, c.silenced),
            (2, 2, 0),
            "seed {seed:#x}"
        );
        sim.run_until_caught_up(60_000);
        sim.assert_counts();
        for n in 1..=4 {
            sim.assert_column(n, 0);
        }
        assert_eq!(sim.over, None, "seed {seed:#x}: 人数が二重になった");
        assert_eq!(sim.deep, None);
    }
}

#[test]
fn during_a_match_no_leaf_is_moved() {
    let mut sim = leaves_then_relay(0x5A, LOSSY, 6_000);
    let (seg_at, end) = idle_window(&sim.script);
    sim.run_to(seg_at - 1);
    assert!(matches!(sim.log.last(), Some(Record::Frame { .. })));
    // 中継役に向く 4 はもう ACK を返していて、上りも分かっている。
    assert_eq!(sim.host.uplink_of(addr(4)), Some(THICK));
    assert!(sim.host.tree_of(addr(4)).is_some_and(|v| !v.relay.no_intro));
    assert_eq!(sim.host.counts().swaps, 0, "試合中に入れ替えた");
    for n in 1..=3 {
        assert_eq!(sim.node(n).w.tree_counts().intros_taken, 0, "葉 {n}");
    }
    // 切れ目に入ると、ホストが葉を移す（この模型で入れ替えが起きる証拠）。
    sim.run_to(end);
    assert_eq!(sim.host.counts().swaps, 2);
}

#[test]
fn a_leaf_whose_intro_was_lost_joins_the_host_after_rejoin_ms_and_is_introduced_again() {
    let mut sim = leaves_then_relay(0x7C, CALM, 6_000);
    let (seg_at, end) = idle_window(&sim.script);
    sim.run_to(seg_at - 1);
    sim.net.drop_intros = 1;
    sim.run_to(end);
    assert_eq!(sim.net.drop_intros, 0, "紹介を落とせていない");
    assert_eq!(sim.children_of(4), vec![1, 2]);
    let c = sim.host.counts();
    assert!(c.intro_resends >= 1, "紹介を送り直していない");
    assert_eq!((c.swaps, c.swap_freed, c.silenced), (2, 2, 0));
    let moved = sim.node(1).moves.first().map_or(u32::MAX, |m| m.0);
    assert!(
        (seg_at + REJOIN_MS..seg_at + REJOIN_MS + 2_000).contains(&moved),
        "葉 1 が移った時刻 {moved}（切れ目 {seg_at}）"
    );
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in 1..=4 {
        sim.assert_column(n, 0);
    }
    assert_eq!(sim.over, None);
}

/// 中継役が依頼を受け取らない間に、ホストの紹介を受けた葉 1 が中継役へ届かず「直接だけ」で戻る（元の枠はまだある）。
#[test]
fn a_leaf_back_while_its_direct_slot_remains_is_counted_once_and_keeps_its_slot() {
    let mut sim = leaves_then_relay(0x8D, CALM, 12_000);
    let (seg_at, end) = idle_window(&sim.script);
    sim.run_to(seg_at - 1);
    sim.net.drop_admits = u32::MAX;
    sim.run_to(seg_at + REJOIN_MS + 500);
    let w = &sim.node(1).w;
    assert!(w.parent_is_home() && w.direct_only(), "葉 1 が戻っていない");
    assert_eq!(w.tree_counts().unreached, 1);
    sim.net.drop_admits = 0;
    sim.run_to(end);
    let after: Vec<HostEvent> = sim
        .events_of(1)
        .into_iter()
        .filter(|e| e.0 >= seg_at)
        .map(|e| e.1)
        .filter(|e| matches!(e, HostEvent::Refreshed | HostEvent::Joined))
        .collect();
    assert!(
        after.first() == Some(&HostEvent::Refreshed) && !after.contains(&HostEvent::Joined),
        "葉 1 が元の枠のまま戻っていない: {after:?}"
    );
    // 葉 1 は直接のまま、2・3 は中継役 4 の下へ。空けた枠は 2・3 の分だけ。
    assert_eq!(sim.children_of(4), vec![2, 3]);
    let c = sim.host.counts();
    assert_eq!((c.swaps, c.swap_freed, c.silenced), (3, 2, 0));
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in 1..=4 {
        sim.assert_column(n, 0);
    }
    assert_eq!(sim.over, None, "人数が二重になった");
}

/// 中継役 1（子 2〜5）と中継役 6（子 7〜9）。1 が落ち、戻った子は 6 が満杯なので直接に残る。後から来た中継できる 10 の下へ、
/// 次の切れ目で移る。
#[test]
fn children_back_from_a_crashed_relay_move_under_another_relay_at_the_next_break() {
    for seed in [0x9E, 0xAFAF] {
        let mut sim = Sim::with_script(seed, LOSSY, two_matches(seed, 1_200, 6_000, 200));
        sim.add(1, 100, true, THICK);
        for n in 2..=5 {
            sim.add(n, 100 + 200 * u32::from(n), false, 0);
        }
        sim.add(6, 1_500, true, THICK);
        for n in 7..=9 {
            sim.add(n, 1_500 + 200 * u32::from(n - 6), false, 0);
        }
        sim.run_to(3_500);
        assert_eq!(
            sim.relays(),
            vec![(1, vec![2, 3, 4, 5]), (6, vec![7, 8, 9])],
            "seed {seed:#x}"
        );
        sim.crash(1);
        sim.run_to(3_500 + REJOIN_MS + 1_500);
        let home: Vec<u16> = (2..=5)
            .filter(|&n| sim.node(n).w.parent_is_home())
            .collect();
        assert_eq!(home.len(), 3, "seed {seed:#x}: 6 の空き 1 つ以外は直接");
        sim.add(10, sim.now, true, THICK);
        let (seg_at, end) = idle_window(&sim.script);
        assert!(seg_at > 3_500 + SILENT_MS, "1 を外す前に切れ目が来る");
        sim.run_to(end);
        let mut kids = sim.children_of(10);
        kids.sort_unstable();
        assert_eq!(kids, home, "seed {seed:#x}");
        assert_eq!(sim.host.counts().swaps, 3, "seed {seed:#x}");
        sim.run_until_caught_up(60_000);
        sim.assert_counts();
        for n in 2..=10 {
            sim.assert_column(n, 0);
        }
        assert_eq!(sim.over, None);
        assert_eq!(sim.deep, None);
    }
}

/// 中継役 1（子 2〜5）と、中継役 6（子 7）。子 2 の照合がずれる。
#[test]
fn a_mismatch_under_a_relay_returns_direct_only_and_is_never_introduced_again() {
    let mut sim = Sim::with_script(0xB2, LOSSY, two_matches(0xB2, 400, 6_000, 200));
    sim.add(1, 100, true, THICK);
    for n in 2..=5 {
        sim.add(n, 100 + 200 * u32::from(n), false, 0);
    }
    sim.add(6, 1_500, true, THICK);
    sim.add(7, 2_500, false, 0);
    sim.run_to(3_500);
    assert_eq!(sim.relays(), vec![(1, vec![2, 3, 4, 5]), (6, vec![7])]);
    let t = sim.now;
    // 親が元のホストなら何もしない。
    assert!(!sim.mismatch(6));
    let w6 = &sim.node(6).w;
    assert_eq!(
        (
            w6.epoch(),
            w6.direct_only(),
            w6.parent_since(),
            w6.tree_counts().mismatch_returns
        ),
        (0, false, None, 0)
    );
    let since = sim.node(2).w.parent_since();
    assert!(sim.mismatch(2));
    let w = &sim.node(2).w;
    assert!(w.parent_is_home() && w.direct_only());
    assert_eq!(
        (
            w.epoch(),
            w.parent_since(),
            w.tree_counts().mismatch_returns
        ),
        (2, Some(t), 1)
    );
    assert!(since.is_some_and(|s| s < t), "紹介を受けた時刻が無い");
    sim.run_to(t + 1_000);
    assert!(
        sim.host.ack_of(addr(2)).is_some(),
        "ホストが直接受けていない"
    );
    // 中継役 1 は BYE で子 2 を外した。
    assert_eq!(sim.children_of(1), vec![3, 4, 5]);
    let relay1 = sim.node(1).relay.as_ref().expect("配る側");
    assert!(relay1.counts().left >= 1, "中継役へ BYE が届いていない");
    // 切れ目でも、直接の 3 人（1・6・2）のうち 2 は移さない。
    let (seg_at, end) = idle_window(&sim.script);
    assert!(seg_at > t + 1_000);
    sim.run_to(end);
    assert_eq!(
        sim.node(2).w.tree_counts().intros_taken,
        1,
        "また紹介された"
    );
    assert_eq!(sim.host.counts().swaps, 0);
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in 1..=7 {
        sim.assert_column(n, 0);
    }
}

/// 接続した後に足した候補と回線の型を、ホストが知らせの JOIN で読み直す（流れは飛ばない）。元のホストから中継役の下へ移った子と、
/// まだ接続していない観戦者は送らない。
#[test]
fn an_announce_updates_what_the_host_knows_and_only_a_joined_direct_watcher_sends_it() {
    let mut sim = leaves_then_relay(0xA7, CALM, 6_000);
    sim.add(5, 600_000, false, 0);
    let (_, end) = idle_window(&sim.script);
    sim.run_to(end);
    let moved = sim.children_of(4);
    assert_eq!(moved.len(), 2, "{moved:?}");
    let leaf = (1..=3).find(|n| !moved.contains(n)).expect("直接の葉");
    let t = sim.now;
    let before = sim.host.tree_of(addr(leaf)).expect("ホストの枠").join;
    let extra = SocketAddr::from(([198, 51, 100, 7], 47_001));
    assert!(!before.candidates.contains(extra) && !before.nat.upnp);
    let mapped = NatMark {
        upnp: true,
        ..REACHABLE
    };
    let x = sim.node_mut(leaf);
    x.w.set_candidates(Candidates::from_addrs(&[addr(leaf), extra]).unwrap());
    x.w.set_nat(mapped);
    let (joins, joined) = (
        sim.node(leaf).w.counts().joins_sent,
        sim.host.counts().joined,
    );
    assert!(sim.announce(leaf));
    assert_eq!(sim.node(leaf).w.counts().joins_sent, joins + 1);
    assert!(
        !sim.announce(moved[0]),
        "元のホストから中継役の下へ移った子が送った"
    );
    assert!(!sim.announce(5), "接続していない観戦者が送った");
    // CALM は落とさないので 1 通で届く。
    sim.run_to(t + 200);
    let after = sim.host.tree_of(addr(leaf)).expect("ホストの枠").join;
    assert!(after.candidates.contains(extra), "候補を読み直していない");
    assert_eq!(after.nat, mapped, "回線の型を読み直していない");
    assert_eq!(after.epoch, before.epoch);
    assert_eq!(sim.host.counts().joined, joined, "知らせを入り直しと数えた");
    sim.run_until_caught_up(60_000);
    sim.assert_counts();
    for n in 1..=4 {
        sim.assert_column(n, 0);
    }
}

/// 選択の画面（記録が積まれず合図だけが流れる間）が 60 秒続き、10% 落ちる回線で、子が親の沈黙と見て戻らない。
#[test]
fn on_a_lossy_line_children_do_not_return_while_only_heartbeats_flow() {
    let mut wrong = Vec::new();
    for seed in [0x13, 0x2424] {
        let script = two_matches(seed, 300, 60_000, 100);
        let (from, to) = idle_window(&script);
        let mut sim = Sim::with_script(seed, LOSSY, script);
        sim.add_tree(10, 200);
        sim.run_to(4_500);
        let shape: Vec<usize> = sim.relays().iter().map(|r| r.1.len()).collect();
        assert_eq!(shape, vec![4, 4], "seed {seed:#x}: 木の形");
        sim.run_to(from);
        let before = sim.returns();
        sim.run_to(to);
        wrong.push(sim.returns() - before);
        assert!(sim.net.dropped > 0);
        sim.run_until_caught_up(to + 30_000);
        sim.assert_counts();
        for n in 1..=10 {
            sim.assert_column(n, 0);
        }
    }
    assert_eq!(wrong, vec![0, 0], "合図だけの間に子が戻った");
}

// ============================ 便を直に組み立てる検査 ============================

/// 封じた便・HMAC の便の受け口（`(宛先, バイト列)` を溜める）。
#[derive(Default)]
struct Sink(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Sink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

impl Sink {
    /// 宛先 `to` の便を `keys` で開いて取り出す（開けない便は飛ばす）。
    fn opened(&self, to: SocketAddr, keys: &DirKeys) -> Vec<Message> {
        self.0
            .iter()
            .filter(|(a, _)| *a == to)
            .filter_map(|(_, b)| wire::decode(&key(), Some(&keys.opener()), b).ok())
            .map(|p| p.msg)
            .collect()
    }
}

/// 観戦者 `n` を演じる側: ホストとの鍵（観戦者が「後ろ」）で ACK を封じ、ホストの便を開く。
struct Fake {
    n: u16,
    keys: DirKeys,
    seq: Seq,
}

impl Fake {
    fn new(n: u16) -> Fake {
        Fake {
            n,
            keys: Ephemeral::from_seed(&wseed(n))
                .dir_keys(&host_cfg().public(), false, SID, &key())
                .expect("鍵"),
            seq: Seq::new(),
        }
    }

    fn public(&self) -> [u8; 32] {
        Ephemeral::from_seed(&wseed(self.n)).public()
    }

    fn join(&self, tree: JoinTree) -> Vec<u8> {
        let mut name = [0u8; 32];
        name[0] = b'f';
        let msg = Message::SpectateJoin(SpectateJoin {
            name_len: 1,
            ff_max: 8,
            nonce: nonce_of(self.n),
            name,
            build: higanr_core::update::BUILD_NUMBER,
            public: self.public(),
            tree,
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        let k = wire::encode(&key(), SID, &msg, &mut buf);
        buf[..k].to_vec()
    }

    fn sealed(&self, msg: &Message) -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let k = wire::seal(
            &self.keys.sealer(Sender::Spectator),
            &self.seq,
            SID,
            msg,
            &mut buf,
        )
        .expect("封じる");
        buf[..k].to_vec()
    }

    fn pair(&self, turn: u16, pair: u8, second: bool) -> Vec<u8> {
        self.sealed(&Message::SpectatePair(SpectatePair { turn, pair, second }))
    }

    fn ack(&self, next: u32, relay: RelayState, admit_ack: u16) -> Vec<u8> {
        self.sealed(&Message::SpectateAck {
            nonce: nonce_of(self.n),
            next,
            relay,
            admit_ack,
            sack: Bits512::EMPTY,
        })
    }
}

/// 中継できる観戦者の JOIN の木の欄。
fn relay_tree(n: u16) -> JoinTree {
    JoinTree {
        relay_public: Some(RelayKey::new(&rseed(n)).public()),
        nat: REACHABLE,
        ..JoinTree::NONE
    }
}

const ON: RelayState = RelayState {
    descendants: 0,
    no_intro: false,
};

/// 記録 0〜100 を積んだホストに、中継役を演じる観戦者 1 が入り、番号 `ack` まで受け取った。
fn host_with_relay(
    tree: JoinTree,
    uplink: u32,
    relay: RelayState,
    ack: u32,
) -> (SpectateHost, Fake) {
    let mut host = SpectateHost::with_capacity(host_cfg(), 1_024);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..100 {
        host.push(Record::Frame {
            frame: f,
            p1: f as u16,
            p2: 0,
        })
        .unwrap();
    }
    let r = Fake::new(1);
    let mut sink = Sink::default();
    assert_eq!(
        host.on_datagram(addr(1), &r.join(tree), 0, &mut sink),
        HostEvent::Joined
    );
    host.note_uplink(addr(1), uplink);
    host.poll(1, &mut sink);
    host.on_datagram(addr(1), &r.ack(ack, relay, 0), 2, &mut sink);
    (host, r)
}

fn child_tree(resume_from: Option<u32>, epoch: u16) -> JoinTree {
    JoinTree {
        resume_from,
        epoch,
        ..JoinTree::NONE
    }
}

#[test]
fn a_child_ahead_of_the_relay_is_taken_directly() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut sink = Sink::default();
    for (n, resume, want) in [
        (2, Some(80), HostEvent::Joined),
        (3, Some(40), HostEvent::Introduced),
        (4, None, HostEvent::Introduced),
    ] {
        let ev = host.on_datagram(
            addr(n),
            &Fake::new(n).join(child_tree(resume, 1)),
            3,
            &mut sink,
        );
        assert_eq!(ev, want, "子 {n}（続き {resume:?}）");
    }
    // ホストが子 3 へ送った紹介は子 3 との鍵で開け、中継役の中継用の公開鍵と、子の JOIN の nonce・epoch が載っている。
    let c3 = Fake::new(3);
    let got = sink.opened(addr(3), &c3.keys);
    let want_public = RelayKey::new(&rseed(1)).public();
    assert!(
        got.iter().any(|m| matches!(m, Message::SpectateIntro(i)
            if i.nonce == nonce_of(3) && i.epoch == 1 && i.relay_public == want_public
                && i.relay.contains(addr(1)))),
        "紹介が無い: {got:?}"
    );
    let admits: Vec<SpectateAdmit> = sink
        .opened(addr(1), &r.keys)
        .into_iter()
        .filter_map(|m| match m {
            Message::SpectateAdmit(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(admits.len(), 2);
    assert_eq!(
        (
            admits[0].no,
            admits[0].start,
            admits[0].nonce,
            admits[0].epoch,
            admits[0].public
        ),
        (1, 40, nonce_of(3), 2, c3.public())
    );
    assert!(
        admits[0].candidates.contains(addr(3)),
        "子の送り元が候補に無い"
    );
    assert_eq!((admits[1].no, admits[1].start), (2, 0));
}

#[test]
fn only_a_fit_relay_gets_children() {
    let unknown = NatMark {
        symmetry: Symmetry::Unknown,
        upnp: false,
    };
    let symmetric = NatMark {
        symmetry: Symmetry::Yes,
        upnp: false,
    };
    let cases: [(&str, JoinTree, u32, RelayState, u32, HostEvent); 7] = [
        ("向く", relay_tree(1), THICK, ON, 3, HostEvent::Introduced),
        (
            "中継しない",
            JoinTree {
                relay_public: None,
                ..relay_tree(1)
            },
            THICK,
            ON,
            3,
            HostEvent::Joined,
        ),
        (
            "シンメトリック",
            JoinTree {
                nat: symmetric,
                ..relay_tree(1)
            },
            THICK,
            ON,
            3,
            HostEvent::Joined,
        ),
        (
            "種類不明",
            JoinTree {
                nat: unknown,
                ..relay_tree(1)
            },
            THICK,
            ON,
            3,
            HostEvent::Joined,
        ),
        (
            "細い",
            relay_tree(1),
            RELAY_MIN_BPS - 1,
            ON,
            3,
            HostEvent::Joined,
        ),
        (
            "紹介しないで",
            relay_tree(1),
            THICK,
            RelayState::NO_INTRO,
            3,
            HostEvent::Joined,
        ),
        (
            "黙っている",
            relay_tree(1),
            THICK,
            ON,
            3 + RELAY_QUIET_MS,
            HostEvent::Joined,
        ),
    ];
    for (label, tree, uplink, relay, at, want) in cases {
        let (mut host, _) = host_with_relay(tree, uplink, relay, 50);
        let ev = host.on_datagram(
            addr(2),
            &Fake::new(2).join(child_tree(None, 0)),
            at,
            &mut Sink::default(),
        );
        assert_eq!(ev, want, "{label}");
    }
}

#[test]
fn a_descendant_count_over_four_is_counted_as_four_and_the_ack_still_moves() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    assert_eq!(host.spectator_count(), 1);
    let lie = RelayState {
        descendants: 7,
        no_intro: false,
    };
    let ev = host.on_datagram(addr(1), &r.ack(60, lie, 0), 5, &mut Sink::default());
    assert_eq!(ev, HostEvent::Acked { progress: true });
    assert_eq!(host.ack_of(addr(1)), Some(60));
    assert_eq!(host.spectator_count(), 1 + usize::from(RELAY_CHILDREN));
    assert_eq!(host.counts().descendants_clamped, 1);
}

/// 中継役 1 と、紹介した子 2 が 1 人いるホスト。
fn host_with_intro() -> (SpectateHost, Fake, Fake) {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let c = Fake::new(2);
    let ev = host.on_datagram(
        addr(2),
        &c.join(child_tree(None, 0)),
        3,
        &mut Sink::default(),
    );
    assert_eq!(ev, HostEvent::Introduced);
    assert_eq!((host.pending_intros(), host.spectator_count()), (1, 2));
    (host, r, c)
}

#[test]
fn a_pending_intro_moves_to_the_relays_descendants_when_its_number_comes_back() {
    let (mut host, r, _) = host_with_intro();
    let one = RelayState {
        descendants: 1,
        no_intro: false,
    };
    host.on_datagram(addr(1), &r.ack(60, one, 1), 10, &mut Sink::default());
    assert_eq!((host.pending_intros(), host.spectator_count()), (0, 2));
}

#[test]
fn a_join_resend_gets_the_intro_again_without_a_key_agreement() {
    let (mut host, _, c) = host_with_intro();
    let before = host.counts().key_agreements;
    let mut sink = Sink::default();
    let ev = host.on_datagram(addr(2), &c.join(child_tree(None, 0)), 20, &mut sink);
    assert_eq!(ev, HostEvent::Introduced);
    assert_eq!(host.counts().key_agreements, before);
    assert_eq!(host.counts().intro_resends, 1);
    assert!(
        sink.opened(addr(2), &c.keys)
            .iter()
            .any(|m| matches!(m, Message::SpectateIntro(_)))
    );
}

#[test]
fn an_unanswered_admit_is_resent_and_the_intro_stops_counting_after_admit_ttl_ms() {
    let (mut host, r, _) = host_with_intro();
    let mut sink = Sink::default();
    let mut t = 3;
    // 中継役は流れには ACK を返し続けるが、依頼の番号は返さない。
    while t < 3 + ADMIT_TTL_MS + 2_000 {
        t += 50;
        host.poll(t, &mut sink);
        if t % 500 == 3 {
            host.on_datagram(addr(1), &r.ack(60, ON, 0), t, &mut Sink::default());
        }
        if t == 3 + ADMIT_TTL_MS - 50 {
            assert_eq!(host.pending_intros(), 1, "寿命の前に消えた");
        }
    }
    assert_eq!((host.pending_intros(), host.spectator_count()), (0, 1));
    assert_eq!(host.counts().intros_expired, 1);
    let admits = |s: &Sink| {
        s.opened(addr(1), &r.keys)
            .iter()
            .filter(|m| matches!(m, Message::SpectateAdmit(_)))
            .count()
    };
    assert!(admits(&sink) >= 2, "依頼を送り直していない");
    // 数えなくなった後も、番号を飛ばさないよう依頼は送り直す。
    let mut later = Sink::default();
    host.poll(t + 2_000, &mut later);
    assert_eq!(admits(&later), 1);
}

#[test]
fn the_same_child_with_a_new_epoch_drops_the_pending_intro() {
    let (mut host, _, c) = host_with_intro();
    let back = JoinTree {
        direct_only: true,
        ..child_tree(None, 2)
    };
    let ev = host.on_datagram(addr(2), &c.join(back), 400, &mut Sink::default());
    assert_eq!(ev, HostEvent::Joined, "直接だけの子は直接受ける");
    assert_eq!((host.pending_intros(), host.spectator_count()), (0, 2));
}

#[test]
fn a_child_back_after_its_relay_already_dropped_it_is_not_subtracted_again() {
    let (mut host, r, c) = host_with_intro();
    let one = RelayState {
        descendants: 1,
        no_intro: false,
    };
    host.on_datagram(addr(1), &r.ack(60, one, 1), 10, &mut Sink::default());
    // 中継役が子を外した ACK が、子の戻りより先に着き、子は前払いが切れた後に戻る。
    host.on_datagram(addr(1), &r.ack(60, ON, 1), 20, &mut Sink::default());
    let back = c.join(child_tree(Some(30), 2));
    assert_eq!(
        host.on_datagram(addr(2), &back, 30 + PREPAID_MS, &mut Sink::default()),
        HostEvent::Joined
    );
    assert_eq!(host.spectator_count(), 2);
    // 中継役に次の子が入ると、子孫の数がそのまま人数に乗る。
    host.on_datagram(
        addr(1),
        &r.ack(60, one, 1),
        40 + PREPAID_MS,
        &mut Sink::default(),
    );
    assert_eq!(host.spectator_count(), 3);
}

fn relaying(descendants: u8) -> RelayState {
    RelayState {
        descendants,
        no_intro: false,
    }
}

#[test]
fn a_child_back_after_its_relay_reported_dropping_it_is_not_subtracted_twice() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    for (n, t) in [(2, 3), (3, 4)] {
        let ev = host.on_datagram(addr(n), &Fake::new(n).join(child_tree(None, 0)), t, &mut s);
        assert_eq!(ev, HostEvent::Introduced);
    }
    host.on_datagram(addr(1), &r.ack(60, relaying(2), 2), 10, &mut s);
    assert_eq!(host.spectator_count(), 3);
    // 中継役が子 3 を外した ACK が、子 3 の戻りより先に着く（子 2 は中継役の下に残る）。
    host.on_datagram(addr(1), &r.ack(60, relaying(1), 2), 20, &mut s);
    let back = Fake::new(3).join(child_tree(Some(30), 2));
    assert_eq!(
        host.on_datagram(addr(3), &back, 30, &mut s),
        HostEvent::Joined
    );
    assert_eq!(host.spectator_count(), 3, "中継役・子 2・戻った子 3");
}

#[test]
fn a_reordered_relay_ack_leaves_no_prepaid_seat() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    host.on_datagram(addr(1), &r.ack(60, relaying(2), 0), 10, &mut s);
    // 順の入れ替わった古い ACK（子孫 1）の後に、新しい ACK（子孫 2）が着く。
    host.on_datagram(addr(1), &r.ack(60, relaying(1), 0), 11, &mut s);
    assert_eq!(host.reserved(11), 1);
    host.on_datagram(addr(1), &r.ack(60, relaying(2), 0), 12, &mut s);
    assert_eq!(host.reserved(12), 0);
    assert_eq!(host.spectator_count(), 3);
}

#[test]
fn a_leaving_relays_prepaid_seats_become_its_reservation() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    host.on_datagram(addr(1), &r.ack(60, relaying(2), 0), 10, &mut s);
    // 中継役が子 2 人を REHOME した ACK の後、中継役の BYE が子の JOIN より先に着く。
    host.on_datagram(addr(1), &r.ack(60, ON, 0), 11, &mut s);
    let bye = r.sealed(&Message::Bye(ByeReason::Cancelled));
    assert_eq!(host.on_datagram(addr(1), &bye, 12, &mut s), HostEvent::Left);
    assert_eq!(host.reserved(13), 2);
    assert!(host.reserved(12 + PREPAID_MS + 1) == 2 && host.reserved(12 + RESERVE_MS) == 0);
}

/// 中継役（子 4 人）と直接の 4 人で 9 人。中継役の子 5 が戻っても中継役はまだ子 5 の枠を持っている。
fn host_with_a_relay_holding_a_returned_child() -> (SpectateHost, Fake, u32) {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    let mut t = 3;
    for n in 2..=5 {
        let ev = host.on_datagram(addr(n), &Fake::new(n).join(child_tree(None, 0)), t, &mut s);
        assert_eq!(ev, HostEvent::Introduced, "子 {n}");
        t += 150;
    }
    host.on_datagram(addr(1), &r.ack(60, relaying(4), 4), t, &mut s);
    let direct = JoinTree {
        direct_only: true,
        ..JoinTree::NONE
    };
    for n in 6..=9 {
        t += 150;
        let ev = host.on_datagram(addr(n), &Fake::new(n).join(direct), t, &mut s);
        assert_eq!(ev, HostEvent::Joined, "直接 {n}");
    }
    t += 150;
    let back = Fake::new(5).join(child_tree(Some(30), 2));
    assert_eq!(
        host.on_datagram(addr(5), &back, t, &mut s),
        HostEvent::Joined
    );
    host.on_datagram(addr(1), &r.ack(60, relaying(4), 4), t + 1, &mut s);
    assert_eq!(host.spectator_count(), 9);
    (host, r, t + 2)
}

#[test]
fn a_relay_still_holding_a_returned_child_gets_no_new_child() {
    let (mut host, _, t) = host_with_a_relay_holding_a_returned_child();
    let mut s = Sink::default();
    let ev = host.on_datagram(
        addr(11),
        &Fake::new(11).join(child_tree(None, 0)),
        t,
        &mut s,
    );
    assert_eq!(ev, HostEvent::Joined, "中継役は子 4 人の枠を持ったまま");
    let ev = host.on_datagram(
        addr(12),
        &Fake::new(12).join(child_tree(None, 0)),
        t + 1,
        &mut s,
    );
    assert_eq!(ev, HostEvent::Refused);
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
}

/// 中継役が受け取れない依頼（子が 4 人いる）は、番号を受け取らない（ACK に載せない）。
#[test]
fn a_relay_does_not_ack_an_admit_its_distributor_has_no_room_for() {
    let mut w = watcher(1);
    let k = RelayKey::new(&rseed(1));
    w.set_relay_public(Some(k.public()));
    let mut relay = SpectateHost::relay_with_capacity(SID, secret(), k, 256);
    let seq = Seq::new();
    let mut s = Sink::default();
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: true,
        count: 0,
        records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
    });
    w.on_datagram(HOST, &from_host(&w, &stream, None, &seq), 1, &mut s);
    tend_relay(&mut w, &mut relay, 1, &mut s);
    for no in 1..=5u16 {
        let a = Message::SpectateAdmit(SpectateAdmit {
            no,
            start: 0,
            nonce: 0xC000 + u64::from(no),
            epoch: 1,
            public: [no as u8; 32],
            candidates: Candidates::EMPTY,
        });
        let t = 2 + u32::from(no);
        w.on_datagram(HOST, &from_host(&w, &a, None, &seq), t, &mut s);
        tend_relay(&mut w, &mut relay, t, &mut s);
    }
    let mut acks = Sink::default();
    w.on_datagram(HOST, &from_host(&w, &stream, None, &seq), 20, &mut acks);
    let mine = Ephemeral::from_seed(&host_cfg().key_seed)
        .dir_keys(&w.public_key(), true, SID, &key())
        .unwrap();
    let got = acks.opened(HOST, &mine);
    assert!(
        matches!(
            got.as_slice(),
            [Message::SpectateAck {
                admit_ack: 4,
                relay: RelayState {
                    descendants: 4,
                    no_intro: false
                },
                ..
            }]
        ),
        "{got:?}"
    );
    assert_eq!(relay.relay_state().descendants, 4);
}

/// 紹介の表（16 枠）が、中継役から BYE で抜けた子の置き場所で埋まり、いちばん古い子の置き場所が空けられた。中継役が落ちて
/// その子が戻ると、表に名前が無くても満杯で断られない。
#[test]
fn a_child_evicted_from_the_placement_table_still_gets_back_in_when_full() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    let mut t = 3;
    let mut admitted: u16 = 0;
    for n in 2..=5u16 {
        host.on_datagram(
            addr(1),
            &r.ack(60, relaying(admitted as u8), admitted),
            t,
            &mut s,
        );
        let ev = host.on_datagram(addr(n), &Fake::new(n).join(child_tree(None, 0)), t, &mut s);
        assert_eq!(ev, HostEvent::Introduced);
        admitted += 1;
        t += 150;
    }
    host.on_datagram(addr(1), &r.ack(60, relaying(4), admitted), t, &mut s);
    for n in 20..33u16 {
        t += 150;
        host.on_datagram(addr(1), &r.ack(60, relaying(3), admitted), t, &mut s);
        let ev = host.on_datagram(addr(n), &Fake::new(n).join(child_tree(None, 0)), t, &mut s);
        assert_eq!(ev, HostEvent::Introduced, "入れ替わり {n}");
        admitted += 1;
        t += 20;
        host.on_datagram(addr(1), &r.ack(60, relaying(4), admitted), t, &mut s);
    }
    // 抜けた子の前払いが切れてから、直接の 5 人で満杯にする。
    t += PREPAID_MS;
    let direct = JoinTree {
        direct_only: true,
        ..JoinTree::NONE
    };
    for n in 40..45u16 {
        t += 150;
        let ev = host.on_datagram(addr(n), &Fake::new(n).join(direct), t, &mut s);
        assert_eq!(ev, HostEvent::Joined);
    }
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    // 中継役が落ち、子は `REJOIN_MS` の後に戻る。
    t += REJOIN_MS;
    for (n, at) in [(2, t), (32, t + 1)] {
        let back = Fake::new(n).join(child_tree(Some(30), 2));
        let ev = host.on_datagram(addr(n), &back, at, &mut s);
        assert_eq!(ev, HostEvent::Joined, "子 {n}");
    }
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    // 対照: 子でなかった新しい人（続きの番号なし）は断られる。
    let ev = host.on_datagram(
        addr(50),
        &Fake::new(50).join(child_tree(None, 0)),
        t + 2_000,
        &mut s,
    );
    assert_eq!(ev, HostEvent::Refused);
}

#[test]
fn a_known_spectator_with_a_newer_epoch_is_served_again_from_its_resume_point() {
    let (mut host, r) = host_with_relay(JoinTree::NONE, 0, RelayState::NO_INTRO, 50);
    let mut sink = Sink::default();
    let again = |epoch| {
        r.join(JoinTree {
            epoch,
            ..child_tree(Some(30), epoch)
        })
    };
    assert_eq!(
        host.on_datagram(addr(1), &again(0), 5, &mut sink),
        HostEvent::Refreshed
    );
    assert_eq!(
        host.ack_of(addr(1)),
        Some(50),
        "同じ epoch では流し直さない"
    );
    assert_eq!(
        host.on_datagram(addr(1), &again(2), 6, &mut sink),
        HostEvent::Refreshed
    );
    assert_eq!(host.ack_of(addr(1)), Some(30));
    assert_eq!(host.tree_of(addr(1)).map(|v| v.join.epoch), Some(2));
    // 遅れて着いた古い epoch の JOIN は、欄を読み直さない。
    host.on_datagram(addr(1), &again(0), 7, &mut sink);
    assert_eq!(host.tree_of(addr(1)).map(|v| v.join.epoch), Some(2));
}

/// 中継役 1・2 と、直接の観戦者 3（中継に向く）がいるホスト。ホストが子 4 へ紹介を 1 回送り、中継役 1・2 に印を付けた。
fn host_with_two_relays_and_a_spare() -> (SpectateHost, Fake, Fake, u32) {
    let (mut host, r1) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    // 中継役 2 は「直接だけ」で入る（中継役 1 の下へ回さない）。
    let r2 = Fake::new(2);
    let direct2 = JoinTree {
        direct_only: true,
        ..relay_tree(2)
    };
    assert_eq!(
        host.on_datagram(addr(2), &r2.join(direct2), 3, &mut s),
        HostEvent::Joined
    );
    host.note_uplink(addr(2), THICK);
    host.poll(4, &mut s);
    host.on_datagram(addr(2), &r2.ack(50, ON, 0), 5, &mut s);
    let spare = JoinTree {
        direct_only: true,
        ..relay_tree(3)
    };
    let r3 = Fake::new(3);
    assert_eq!(
        host.on_datagram(addr(3), &r3.join(spare), 6, &mut s),
        HostEvent::Joined
    );
    host.note_uplink(addr(3), THICK);
    host.poll(7, &mut s);
    host.on_datagram(addr(3), &r3.ack(50, ON, 0), 8, &mut s);
    let ev = host.on_datagram(addr(4), &Fake::new(4).join(child_tree(None, 0)), 9, &mut s);
    assert_eq!(ev, HostEvent::Introduced);
    assert!(host.is_relay_of(addr(1)) && host.is_relay_of(addr(2)));
    assert!(!host.is_relay_of(addr(3)));
    (host, r1, r2, 10)
}

#[test]
fn a_relay_that_briefly_says_no_intro_still_holds_one_of_the_two_relay_places() {
    let (mut host, r1, _, t) = host_with_two_relays_and_a_spare();
    let mut s = Sink::default();
    host.on_datagram(addr(1), &r1.ack(50, RelayState::NO_INTRO, 0), t, &mut s);
    host.on_datagram(
        addr(5),
        &Fake::new(5).join(child_tree(None, 0)),
        t + 1,
        &mut s,
    );
    assert!(!host.is_relay_of(addr(3)), "中継役が 3 人になった");
}

#[test]
fn a_child_back_twice_frees_its_newest_place() {
    let (mut host, _, r2, t) = host_with_two_relays_and_a_spare();
    let mut s = Sink::default();
    let c = Fake::new(4);
    // 子 4 は中継役 1 へ回った（同じ太さなら枠の番号の小さい方）。依頼を受け取る前に戻り、中継役 2 へ回る。
    let ev = host.on_datagram(addr(4), &c.join(child_tree(None, 2)), t, &mut s);
    assert_eq!(ev, HostEvent::Introduced);
    let to_r2 = s
        .opened(addr(2), &r2.keys)
        .into_iter()
        .any(|m| matches!(m, Message::SpectateAdmit(a) if a.nonce == nonce_of(4)));
    assert!(to_r2, "中継役 2 へ回っていない");
    host.on_datagram(addr(2), &r2.ack(50, relaying(1), 1), t + 1, &mut s);
    let before = host.spectator_count();
    // もう一度戻る（直接だけ）: 新しい置き場所の中継役の子孫から引く。
    let back = JoinTree {
        direct_only: true,
        ..child_tree(Some(30), 4)
    };
    assert_eq!(
        host.on_datagram(addr(4), &c.join(back), t + 2, &mut s),
        HostEvent::Joined
    );
    assert_eq!(
        host.spectator_count(),
        before,
        "席の入れ替えで人数は変わらない"
    );
}

#[test]
fn a_fifth_removed_relays_reservation_is_added_not_dropped() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 1_024);
    host.push(Record::Seg { seg: 0 }).unwrap();
    let mut s = Sink::default();
    let fakes: Vec<Fake> = (1..=5).map(Fake::new).collect();
    for (k, f) in fakes.iter().enumerate() {
        let t = k as u32 * 200;
        let n = f.n;
        assert_eq!(
            host.on_datagram(addr(n), &f.join(JoinTree::NONE), t, &mut s),
            HostEvent::Joined
        );
        host.poll(t + 1, &mut s);
        host.on_datagram(addr(n), &f.ack(1, relaying(1), 0), t + 2, &mut s);
    }
    assert_eq!(host.spectator_count(), 10);
    for (k, f) in fakes.iter().enumerate() {
        let bye = f.sealed(&Message::Bye(ByeReason::Cancelled));
        let ev = host.on_datagram(addr(f.n), &bye, 1_100 + k as u32, &mut s);
        assert_eq!(ev, HostEvent::Left);
    }
    assert_eq!(host.reserved(1_200), 5, "5 人目の取り置きが消えた");
}

/// 記録 0〜100 を積んだホストに、観戦者 `ns`（番号・JOIN の木の欄・上り・ACK の中継の状態）が直接入って全部を受け取り、1 回
/// `poll` した後に `Seg(1)` を積んで（マッチの切れ目）全員がそれも受け取った。返すのは次の時刻。
fn host_at_cut(ns: &[(u16, JoinTree, u32, RelayState)]) -> (SpectateHost, Vec<Fake>, Sink, u32) {
    let mut host = SpectateHost::with_capacity(host_cfg(), 1_024);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..100 {
        host.push(Record::Frame {
            frame: f,
            p1: f as u16,
            p2: 0,
        })
        .unwrap();
    }
    let mut sink = Sink::default();
    let fakes: Vec<Fake> = ns.iter().map(|x| Fake::new(x.0)).collect();
    for (f, &(n, tree, _, _)) in fakes.iter().zip(ns) {
        assert_eq!(
            host.on_datagram(addr(n), &f.join(tree), 0, &mut sink),
            HostEvent::Joined,
            "観戦者 {n}"
        );
    }
    for (f, &(n, _, up, st)) in fakes.iter().zip(ns) {
        host.note_uplink(addr(n), up);
        host.on_datagram(addr(n), &f.ack(host.tail(), st, 0), 1, &mut sink);
    }
    host.poll(2, &mut sink);
    host.push(Record::Seg { seg: 1 }).unwrap();
    for (f, &(n, _, _, st)) in fakes.iter().zip(ns) {
        host.on_datagram(addr(n), &f.ack(host.tail(), st, 0), 3, &mut sink);
    }
    (host, fakes, sink, 4)
}

/// 宛先 `f` へ送った紹介の数。
fn intros_to(sink: &Sink, f: &Fake) -> usize {
    sink.opened(addr(f.n), &f.keys)
        .iter()
        .filter(|m| matches!(m, Message::SpectateIntro(_)))
        .count()
}

/// 中継役 `r` へ送った受け入れの依頼の子の nonce（送り直しも並ぶ）。
fn admits_to(sink: &Sink, r: &Fake) -> Vec<u64> {
    sink.opened(addr(r.n), &r.keys)
        .into_iter()
        .filter_map(|m| match m {
            Message::SpectateAdmit(a) => Some(a.nonce),
            _ => None,
        })
        .collect()
}

#[test]
fn at_the_break_relays_spares_parents_and_direct_only_leaves_are_not_moved() {
    let kids = JoinTree {
        has_children: true,
        ..JoinTree::NONE
    };
    let direct = JoinTree {
        direct_only: true,
        ..JoinTree::NONE
    };
    let off = RelayState::NO_INTRO;
    let (mut host, f, mut sink, mut t) = host_at_cut(&[
        (1, relay_tree(1), 2 * THICK, ON),
        (2, relay_tree(2), 2 * THICK, ON),
        // 中継役に向く予備（中継役はもう 2 人）。
        (3, relay_tree(3), THICK, ON),
        // JOIN の後に子ができた（ACK の子孫だけ）。
        (4, JoinTree::NONE, 0, relaying(1)),
        (5, direct, 0, off),
        (7, kids, 0, off),
        (6, JoinTree::NONE, 0, off),
    ]);
    host.poll(t, &mut sink);
    assert!(host.is_relay_of(addr(1)) && host.is_relay_of(addr(2)) && !host.is_relay_of(addr(3)));
    let counts = |s: &Sink| f.iter().map(|x| intros_to(s, x)).collect::<Vec<_>>();
    assert_eq!(counts(&sink), vec![0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(admits_to(&sink, &f[0]), vec![nonce_of(6)]);
    // 中継役 1 が受け取ると葉 6 の枠を空ける。残りの 6 人（直接の 2 人を越える）は動かさない。
    host.on_datagram(
        addr(1),
        &f[0].ack(host.tail(), relaying(1), 1),
        t,
        &mut sink,
    );
    assert_eq!(host.ack_of(addr(6)), None, "葉 6 の枠が残った");
    assert_eq!(host.counts().swap_freed, 1);
    let states = [relaying(1), ON, ON, relaying(1), off, off];
    while t < 4 + 3_000 {
        t += 1;
        if t % 250 == 0 {
            for (x, st) in f.iter().zip(states) {
                let admit = if x.n == 1 { 1 } else { 0 };
                host.on_datagram(addr(x.n), &x.ack(host.tail(), st, admit), t, &mut sink);
            }
        }
        host.poll(t, &mut sink);
    }
    assert_eq!(counts(&sink), vec![0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(host.counts().swaps, 1);
    assert_eq!(
        host.spectator_count(),
        8,
        "直接 6・中継役 1 の子 1・観戦者 4 の子 1"
    );
}

#[test]
fn at_the_break_a_leaf_measuring_its_uplink_is_not_moved() {
    let off = RelayState::NO_INTRO;
    let (mut host, f, mut sink, t) = host_at_cut(&[
        (1, relay_tree(1), THICK, ON),
        // 上りの分からない、中継できる観戦者（測りの番を持つ）。
        (2, relay_tree(2), 0, ON),
        (3, JoinTree::NONE, 0, off),
    ]);
    assert_eq!(host.measuring(), Some(addr(2)), "番が渡っていない");
    host.poll(t, &mut sink);
    let got: Vec<usize> = f.iter().map(|x| intros_to(&sink, x)).collect();
    assert_eq!(got, vec![0, 0, 1]);
    assert!(host.is_relay_of(addr(1)));
}

/// ホストが葉 2 へ送った紹介は届かず、中継役 1 も依頼を受け取らない（葉 2 は元の枠に残る）。
#[test]
fn a_leaf_whose_swap_intro_is_still_in_the_table_is_not_introduced_again() {
    let off = RelayState::NO_INTRO;
    let (mut host, f, mut sink, mut t) = host_at_cut(&[
        (1, relay_tree(1), THICK, ON),
        (2, JoinTree::NONE, 0, off),
        (3, JoinTree::NONE, 0, off),
    ]);
    let states = [ON, off, off];
    let mut seen = vec![(0, 0)];
    while t < 4 + ADMIT_TTL_MS + 1_000 {
        t += 1;
        if t % 250 == 0 {
            for (x, st) in f.iter().zip(states) {
                host.on_datagram(addr(x.n), &x.ack(host.tail(), st, 0), t, &mut sink);
            }
        }
        host.poll(t, &mut sink);
        let now = (intros_to(&sink, &f[1]), intros_to(&sink, &f[2]));
        if seen.last() != Some(&now) {
            seen.push(now);
        }
    }
    // ホストが、中継役 1 の依頼の受け取りを `ADMIT_TTL_MS` 待ってから、次の葉 3 へ。
    assert_eq!(seen, vec![(0, 0), (1, 0), (1, 1)]);
    assert_eq!(host.counts().swaps, 2);
    assert!(host.ack_of(addr(2)).is_some(), "葉 2 の枠を空けた");
}

/// 中継役 1（太い）・2 がいて、子 3 は中継役 1 の下から戻った。そのとき中継役 2 は「紹介しないで」だったので、子 3 は直接に残る。
#[test]
fn at_the_break_a_child_back_from_a_relay_moves_under_another_relay() {
    let (mut host, r1) = host_with_relay(relay_tree(1), 2 * THICK, ON, 101);
    let mut s = Sink::default();
    let r2 = Fake::new(2);
    let direct2 = JoinTree {
        direct_only: true,
        ..relay_tree(2)
    };
    assert_eq!(
        host.on_datagram(addr(2), &r2.join(direct2), 3, &mut s),
        HostEvent::Joined
    );
    host.note_uplink(addr(2), THICK);
    host.poll(4, &mut s);
    host.on_datagram(addr(2), &r2.ack(101, ON, 0), 5, &mut s);
    let c = Fake::new(3);
    assert_eq!(
        host.on_datagram(addr(3), &c.join(child_tree(None, 0)), 6, &mut s),
        HostEvent::Introduced
    );
    assert_eq!(
        admits_to(&s, &r1),
        vec![nonce_of(3)],
        "中継役 1 へ回っていない"
    );
    host.on_datagram(addr(1), &r1.ack(101, relaying(1), 1), 7, &mut s);
    host.on_datagram(addr(2), &r2.ack(101, RelayState::NO_INTRO, 0), 8, &mut s);
    assert_eq!(
        host.on_datagram(addr(3), &c.join(child_tree(Some(101), 2)), 9, &mut s),
        HostEvent::Joined,
        "子 3 は直接に残る"
    );
    host.push(Record::Seg { seg: 1 }).unwrap();
    let tail = host.tail();
    host.on_datagram(addr(1), &r1.ack(tail, ON, 1), 10, &mut s);
    host.on_datagram(addr(2), &r2.ack(tail, ON, 0), 10, &mut s);
    host.on_datagram(addr(3), &c.ack(tail, RelayState::NO_INTRO, 0), 10, &mut s);
    let mut cut = Sink::default();
    host.poll(11, &mut cut);
    assert_eq!(intros_to(&cut, &c), 1, "切れ目で移していない");
    // 太くて番号の小さい中継役 1 ではなく、元の中継役でない 2 へ。
    assert_eq!(admits_to(&cut, &r1), Vec::<u64>::new());
    assert_eq!(admits_to(&cut, &r2), vec![nonce_of(3)]);
}

#[test]
fn dropping_a_relays_twin_slot_keeps_its_childrens_reservation() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 50);
    let mut s = Sink::default();
    let ev = host.on_datagram(addr(2), &Fake::new(2).join(child_tree(None, 0)), 3, &mut s);
    assert_eq!(ev, HostEvent::Introduced);
    assert!(host.is_relay_of(addr(1)));
    host.on_datagram(addr(1), &r.ack(60, relaying(1), 1), 10, &mut s);
    assert_eq!((host.spectator_count(), host.reserved(10)), (2, 0));
    // 中継役 1 の NAT が付け替わり、黙った後に別の送り元から入り直して、そちらから ACK を返す。
    let moved = addr(30);
    let later = 10 + REJOIN_MS + 100;
    assert_eq!(
        host.on_datagram(moved, &r.join(relay_tree(1)), later, &mut s),
        HostEvent::Joined
    );
    host.on_datagram(moved, &r.ack(60, relaying(1), 1), later + 1, &mut s);
    assert_eq!(host.ack_of(addr(1)), None, "古い経路の枠が残った");
    assert_eq!(host.counts().twins_dropped, 1);
    assert_eq!(
        host.reserved(later + 1),
        1,
        "中継役の枠と一緒に子の取り置きが消えた"
    );
}

/// ホストから中継役へは `RELAY_HEARTBEAT_MS`、中継役でない直接の葉へは `HEARTBEAT_MS` ごとに合図する。
#[test]
fn the_host_heartbeats_a_relay_every_500_ms_and_a_plain_leaf_every_1000_ms() {
    let (mut host, r) = host_with_relay(relay_tree(1), THICK, ON, 101);
    let mut s = Sink::default();
    let leaf = Fake::new(2);
    let direct = JoinTree {
        direct_only: true,
        ..JoinTree::NONE
    };
    assert_eq!(
        host.on_datagram(addr(2), &leaf.join(direct), 3, &mut s),
        HostEvent::Joined
    );
    // 3 人目の紹介で中継役 1 に印が付く。
    let ev = host.on_datagram(addr(3), &Fake::new(3).join(child_tree(None, 0)), 4, &mut s);
    assert_eq!(ev, HostEvent::Introduced);
    let mut sink = Sink::default();
    for t in 5..=5_004 {
        if t % 100 == 0 {
            host.on_datagram(addr(1), &r.ack(101, ON, 0), t, &mut Sink::default());
            host.on_datagram(
                addr(2),
                &leaf.ack(101, RelayState::NO_INTRO, 0),
                t,
                &mut Sink::default(),
            );
        }
        let mut out = Sink::default();
        host.poll(t, &mut out);
        if t >= 1_005 {
            sink.0.extend(out.0);
        }
    }
    let beats = |f: &Fake| {
        sink.opened(addr(f.n), &f.keys)
            .iter()
            .filter(|m| matches!(m, Message::SpectateStream(st) if st.count == 0))
            .count()
    };
    assert_eq!(
        (beats(&r), beats(&leaf)),
        (
            (4_000 / RELAY_HEARTBEAT_MS) as usize,
            (4_000 / HEARTBEAT_MS) as usize
        )
    );
}

/// 中継役の配る側から子へも `RELAY_HEARTBEAT_MS` ごとに合図する。
#[test]
fn the_relay_heartbeats_its_children_every_500_ms() {
    let mut r = relay_side();
    let c = Fake::new(2);
    assert!(r.admit(admit_for(&c, 1, 1, 40), 1));
    assert_eq!(
        r.on_datagram(
            addr(2),
            &c.join(child_tree(None, 1)),
            2,
            &mut Sink::default()
        ),
        HostEvent::Joined
    );
    let keys = child_relay_keys(&c);
    let seq = Seq::new();
    let ack = || {
        let mut buf = [0u8; MAX_DATAGRAM];
        let msg = Message::SpectateAck {
            nonce: nonce_of(2),
            next: 40,
            relay: RelayState::NO_INTRO,
            admit_ack: 0,
            sack: Bits512::EMPTY,
        };
        let k = wire::seal(&keys.sealer(Sender::Spectator), &seq, SID, &msg, &mut buf).unwrap();
        buf[..k].to_vec()
    };
    let mut sink = Sink::default();
    for t in 3..=5_002 {
        if t % 100 == 0 {
            r.on_datagram(addr(2), &ack(), t, &mut Sink::default());
        }
        let mut out = Sink::default();
        r.poll(t, &mut out);
        if t >= 1_003 {
            sink.0.extend(out.0);
        }
    }
    let beats = sink
        .opened(addr(2), &keys)
        .iter()
        .filter(|m| matches!(m, Message::SpectateStream(st) if st.count == 0))
        .count();
    assert_eq!(beats, (4_000 / RELAY_HEARTBEAT_MS) as usize);
}

// ---------------------------- 観戦者の側 ----------------------------

/// 観戦者 `w` へ、元のホストの鍵で `msg` を封じる（`keys` を替えると偽の鍵）。
fn from_host(w: &Watcher, msg: &Message, keys: Option<DirKeys>, seq: &Seq) -> Vec<u8> {
    let keys = keys.unwrap_or_else(|| {
        Ephemeral::from_seed(&host_cfg().key_seed)
            .dir_keys(&w.public_key(), true, SID, &key())
            .expect("鍵")
    });
    let mut buf = [0u8; MAX_DATAGRAM];
    let k =
        wire::seal(&keys.sealer(Sender::SpectateHost), seq, SID, msg, &mut buf).expect("封じる");
    buf[..k].to_vec()
}

fn intro(n: u16, epoch: u16, relay: u16) -> Message {
    Message::SpectateIntro(SpectateIntro {
        nonce: nonce_of(n),
        epoch,
        relay_public: RelayKey::new(&rseed(relay)).public(),
        relay: Candidates::from_addrs(&[addr(relay)]).unwrap(),
    })
}

#[test]
fn old_wrong_or_forged_intros_do_not_move_the_watcher() {
    let mut w = watcher(2);
    let seq = Seq::new();
    let mut sink = Sink::default();
    let forged = DirKeys::from_raw([9; 32], [9; 32]);
    let one = RelayState {
        descendants: 1,
        no_intro: false,
    };
    w.set_relay_public(Some(RelayKey::new(&rseed(2)).public()));
    for (label, msg, keys, state) in [
        ("今の epoch でない", intro(2, 1, 1), None, ON),
        ("違う nonce", intro(3, 0, 1), None, ON),
        ("偽の鍵", intro(2, 0, 1), Some(forged), ON),
        ("子がいる", intro(2, 0, 1), None, one),
    ] {
        w.set_relay_state(state);
        w.on_datagram(HOST, &from_host(&w, &msg, keys, &seq), 1, &mut sink);
        assert!(w.parent_is_home() && w.epoch() == 0, "{label}");
    }
    let c = w.tree_counts();
    assert_eq!(
        (c.intros_taken, c.intros_ignored, c.control_dropped),
        (0, 3, 1)
    );
    // 対照: 同じ紹介でも、子がいなければ受ける。
    w.set_relay_state(ON);
    w.on_datagram(
        HOST,
        &from_host(&w, &intro(2, 0, 1), None, &seq),
        2,
        &mut sink,
    );
    assert!(!w.parent_is_home() && w.epoch() == 1);
    // 遅れて着いた同じ紹介・別の中継役の紹介（前の epoch）は親を替えない。
    for relay in [1, 6] {
        w.on_datagram(
            HOST,
            &from_host(&w, &intro(2, 0, relay), None, &seq),
            3,
            &mut sink,
        );
    }
    assert_eq!((w.epoch(), w.tree_counts().intros_taken), (1, 1));
}

#[test]
fn an_intro_from_outside_the_host_candidates_is_taken() {
    let mut w = watcher(2);
    let elsewhere = SocketAddr::from(([127, 0, 0, 1], 59_999));
    let seq = Seq::new();
    let mut sink = Sink::default();
    w.on_datagram(
        elsewhere,
        &from_host(&w, &intro(2, 0, 1), None, &seq),
        1,
        &mut sink,
    );
    assert!(!w.parent_is_home() && w.epoch() == 1);
    // 紹介された中継役へ、epoch 1 の JOIN を送る。
    w.poll(2, &mut sink);
    let joins: Vec<(SocketAddr, JoinTree)> = sink
        .0
        .iter()
        .filter_map(|(a, b)| match wire::decode(&key(), None, b).ok()?.msg {
            Message::SpectateJoin(j) => Some((*a, j.tree)),
            _ => None,
        })
        .collect();
    assert_eq!(joins.len(), 1);
    assert_eq!(
        (joins[0].0, joins[0].1.epoch, joins[0].1.resume_from),
        (addr(1), 1, None)
    );
    // 親（中継役）のアドレスは、中継役から STREAM が来るまで決まらない。
    assert_eq!(w.host_addr(), None);
}

fn admit(no: u16) -> Message {
    Message::SpectateAdmit(SpectateAdmit {
        no,
        start: 7,
        nonce: nonce_of(5),
        epoch: 1,
        public: [3; 32],
        candidates: Candidates::EMPTY,
    })
}

/// 番号 0 の依頼は封じる側（ホスト）が作れず（`debug_assert`）、開く側（観戦者）は decode で捨てる（`wire::tests::poison_spectate_tree_messages`）。
#[test]
fn admits_are_taken_in_order_and_a_skipped_number_is_not() {
    let mut w = watcher(1);
    let seq = Seq::new();
    let mut sink = Sink::default();
    // 中継しない観戦者は受けない。
    w.on_datagram(HOST, &from_host(&w, &admit(1), None, &seq), 1, &mut sink);
    assert_eq!(w.take_admit(), None);
    w.set_relay_public(Some(RelayKey::new(&rseed(1)).public()));
    w.set_relay_state(ON);
    for no in [2, 1, 1, 3, 2] {
        w.on_datagram(HOST, &from_host(&w, &admit(no), None, &seq), 2, &mut sink);
    }
    let taken: Vec<u16> = std::iter::from_fn(|| w.take_admit())
        .map(|a| a.no)
        .collect();
    assert_eq!(taken, vec![1, 2]);
    let c = w.tree_counts();
    assert_eq!(
        (c.admits_taken, c.admits_ignored, c.control_dropped),
        (2, 4, 0)
    );
    // 観戦者の ACK に、受け入れの依頼の受け取りの番号が載る。
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: true,
        count: 0,
        records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
    });
    let mut acks = Sink::default();
    w.on_datagram(HOST, &from_host(&w, &stream, None, &seq), 3, &mut acks);
    let mine = Ephemeral::from_seed(&host_cfg().key_seed)
        .dir_keys(&w.public_key(), true, SID, &key())
        .unwrap();
    let got = acks.opened(HOST, &mine);
    assert!(
        matches!(got.as_slice(), [Message::SpectateAck { admit_ack: 2, .. }]),
        "{got:?}"
    );
}

#[test]
fn a_watcher_stays_joined_while_it_moves_to_a_relay() {
    let mut w = watcher(2);
    let seq = Seq::new();
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: true,
        count: 0,
        records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
    });
    w.on_datagram(
        HOST,
        &from_host(&w, &stream, None, &seq),
        1,
        &mut Sink::default(),
    );
    assert_eq!(w.host_addr(), Some(HOST));
    // 元のホストの紹介は、JOIN の今の epoch のものだけを受ける。
    w.on_datagram(
        HOST,
        &from_host(&w, &intro(2, 0, 1), None, &seq),
        2,
        &mut Sink::default(),
    );
    assert!(!w.parent_is_home());
    assert!(w.is_joined(), "親を替える間に接続していない扱いになった");
    assert_eq!(
        w.host_addr(),
        None,
        "親（中継役）のアドレスはまだ決まらない"
    );
    // 中継役から何も来なければ、元のホストへ「直接だけ」で戻る。続きの番号を載せて JOIN する。
    let mut sink = Sink::default();
    w.poll(2 + REJOIN_MS, &mut sink);
    assert!(w.parent_is_home() && w.direct_only() && w.epoch() == 2);
    let joins: Vec<JoinTree> = sink
        .0
        .iter()
        .filter_map(|(a, b)| match wire::decode(&key(), None, b).ok()?.msg {
            Message::SpectateJoin(j) if *a == HOST => Some(j.tree),
            _ => None,
        })
        .collect();
    assert_eq!(joins.len(), 1);
    assert_eq!(
        (joins[0].epoch, joins[0].direct_only, joins[0].resume_from),
        (2, true, Some(0))
    );
}

#[test]
fn a_cancel_bye_after_joining_marks_the_end_but_not_before() {
    let seq = Seq::new();
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: true,
        count: 0,
        records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
    });
    let cancel = Message::Bye(ByeReason::Cancelled);
    let mut early = watcher(1);
    early.on_datagram(
        HOST,
        &from_host(&early, &cancel, None, &seq),
        1,
        &mut Sink::default(),
    );
    assert!(!early.refused_after_join(), "入る前の断りは満杯の待ち");
    for (reason, want) in [(ByeReason::Cancelled, true), (ByeReason::Finished, false)] {
        let mut w = watcher(1);
        w.on_datagram(
            HOST,
            &from_host(&w, &stream, None, &seq),
            1,
            &mut Sink::default(),
        );
        assert!(w.is_joined());
        let bye = Message::Bye(reason);
        w.on_datagram(
            HOST,
            &from_host(&w, &bye, None, &seq),
            2,
            &mut Sink::default(),
        );
        assert_eq!(w.refused_after_join(), want, "{reason:?}");
    }
}

#[test]
fn a_stream_after_a_refusal_lowers_the_refused_mark() {
    let seq = Seq::new();
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: true,
        count: 0,
        records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
    });
    let mut w = watcher(1);
    let feed = |w: &mut Watcher, msg: &Message, t: u32| {
        w.on_datagram(
            HOST,
            &from_host(w, msg, None, &seq),
            t,
            &mut Sink::default(),
        );
    };
    feed(&mut w, &stream, 1);
    feed(&mut w, &Message::Bye(ByeReason::Cancelled), 2);
    assert!(w.refused_after_join(), "断りの印が立たない");
    feed(&mut w, &stream, 3);
    assert!(!w.refused_after_join(), "入り直せたのに断りの印が残る");
    feed(&mut w, &Message::Bye(ByeReason::Cancelled), 4);
    assert!(w.refused_after_join(), "2 度目の断りで印が立たない");
}

// ---------------------------- 中継役の配る側 ----------------------------

/// 中継役 1 の配る側（記録 0〜39 を積んだ）。
fn relay_side() -> SpectateHost {
    let mut r = SpectateHost::relay_with_capacity(SID, secret(), RelayKey::new(&rseed(1)), 256);
    for f in 0..40 {
        assert!(r.push_at(
            f,
            Record::Frame {
                frame: f,
                p1: 0,
                p2: 0
            }
        ));
    }
    r
}

fn admit_for(c: &Fake, no: u16, epoch: u16, start: u32) -> SpectateAdmit {
    SpectateAdmit {
        no,
        start,
        nonce: nonce_of(c.n),
        epoch,
        public: c.public(),
        candidates: Candidates::EMPTY,
    }
}

/// 子 `c` と中継役 1 の間の鍵（子の側）。
fn child_relay_keys(c: &Fake) -> DirKeys {
    Ephemeral::from_seed(&wseed(c.n))
        .dir_keys(&RelayKey::new(&rseed(1)).public(), false, SID, &key())
        .unwrap()
}

#[test]
fn the_relay_takes_only_named_children() {
    let mut r = relay_side();
    let c = Fake::new(2);
    let mut sink = Sink::default();
    let join = |epoch| c.join(child_tree(None, epoch));
    assert_eq!(
        r.on_datagram(addr(2), &join(1), 1, &mut sink),
        HostEvent::Dropped(higanr_core::spectate::DropReason::NotAdmitted)
    );
    assert!(r.admit(admit_for(&c, 1, 1, 10), 1));
    assert_eq!(
        r.on_datagram(addr(2), &join(2), 2, &mut sink),
        HostEvent::Dropped(higanr_core::spectate::DropReason::NotAdmitted),
        "違う epoch"
    );
    assert_eq!(
        r.on_datagram(addr(2), &join(1), 3, &mut sink),
        HostEvent::Joined
    );
    assert_eq!(r.ack_of(addr(2)), Some(10), "依頼の始める番号から流す");
    assert_eq!(r.relay_state().descendants, 1);
}

#[test]
fn the_relay_rehomes_a_child_whose_start_it_does_not_have() {
    let mut r = relay_side();
    let c = Fake::new(2);
    assert!(r.admit(admit_for(&c, 1, 1, 2_000), 1));
    let mut sink = Sink::default();
    let ev = r.on_datagram(addr(2), &c.join(child_tree(Some(3_000), 1)), 2, &mut sink);
    assert_eq!(ev, HostEvent::Rehomed);
    let got = sink.opened(addr(2), &child_relay_keys(&c));
    assert_eq!(got, vec![Message::SpectateRehome]);
}

#[test]
fn a_relay_that_skips_ahead_rehomes_children_it_can_no_longer_serve() {
    let mut r = relay_side();
    let c = Fake::new(2);
    assert!(r.admit(admit_for(&c, 1, 1, 10), 1));
    let mut sink = Sink::default();
    r.on_datagram(addr(2), &c.join(child_tree(None, 1)), 2, &mut sink);
    r.poll(3, &mut sink);
    let opened = |s: &Sink| s.opened(addr(2), &child_relay_keys(&c));
    assert!(!opened(&sink).contains(&Message::SpectateRehome));
    // 続けて積める番号は、子を残す。
    assert!(r.push_at(40, Record::FILLER));
    r.poll(4, &mut sink);
    assert_eq!(r.spectator_count(), 1);
    // 中継役自身が飛んだ（先の番号から積み直した）: 子の続きはもう無い。
    assert!(r.push_at(5_000, Record::FILLER));
    let mut after = Sink::default();
    r.poll(5, &mut after);
    assert_eq!(opened(&after), vec![Message::SpectateRehome]);
    assert_eq!(
        (r.relay_state().descendants, r.counts().relay_jumps),
        (0, 1)
    );
}

#[test]
fn the_same_child_admitted_twice_to_one_relay_never_reuses_a_nonce() {
    let mut r = relay_side();
    let c = Fake::new(2);
    let keys = child_relay_keys(&c);
    let bye_seq = Seq::new();
    let mut seen: Vec<(u8, u64)> = Vec::new();
    for (round, epoch) in [(0u32, 1u16), (1, 3)] {
        let t0 = 10 + round * 3_000;
        assert!(r.admit(admit_for(&c, round as u16 + 1, epoch, 0), t0));
        let mut sink = Sink::default();
        assert_eq!(
            r.on_datagram(addr(2), &c.join(child_tree(None, epoch)), t0, &mut sink),
            HostEvent::Joined
        );
        for t in t0..t0 + 200 {
            r.poll(t, &mut sink);
        }
        let mine: Vec<&Vec<u8>> = sink
            .0
            .iter()
            .filter(|(a, _)| *a == addr(2))
            .map(|(_, b)| b)
            .collect();
        assert!(mine.len() >= 3, "送りが少ない: {}", mine.len());
        for b in mine {
            assert!(
                wire::decode(&key(), Some(&keys.opener()), b).is_ok(),
                "子の鍵で開けない"
            );
            let mut f = [0u8; 8];
            f.copy_from_slice(&b[HEADER_LEN..HEADER_LEN + 8]);
            seen.push(read_nonce_field(&f));
        }
        // 子が抜ける（BYE は子と中継役の鍵で封じる）。
        let mut buf = [0u8; MAX_DATAGRAM];
        let bye = Message::Bye(ByeReason::Cancelled);
        let k = wire::seal(
            &keys.sealer(Sender::Spectator),
            &bye_seq,
            SID,
            &bye,
            &mut buf,
        )
        .unwrap();
        assert_eq!(
            r.on_datagram(addr(2), &buf[..k], t0 + 200, &mut Sink::default()),
            HostEvent::Left
        );
    }
    let mut uniq = seen.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), seen.len(), "同じ鍵で同じ nonce を 2 回使った");
    assert!(seen.iter().all(|s| s.0 == Sender::SpectateHost as u8));
    // 対照: 同じ種で作り直した配る側は通し番号を 0 から取るので、重なる（この検査が重なりを見分けられる証拠）。
    let mut again = relay_side();
    assert!(again.admit(admit_for(&c, 1, 1, 0), 0));
    let mut sink = Sink::default();
    again.on_datagram(addr(2), &c.join(child_tree(None, 1)), 0, &mut sink);
    again.poll(1, &mut sink);
    let first = sink.0.iter().find(|(a, _)| *a == addr(2)).expect("送り");
    let mut f = [0u8; 8];
    f.copy_from_slice(&first.1[HEADER_LEN..HEADER_LEN + 8]);
    assert!(seen.contains(&read_nonce_field(&f)));
}

// ============================ 上りの測り（MEASURE・PAIR） ============================

/// 1 組の太さ（B/s）: 1,200 B の 2 通が `delta_us` µs 離れて着いた。
fn pair_bps(delta_us: u64) -> u32 {
    (1_200 * 1_000_000 / delta_us) as u32
}

/// 記録 0〜100 を積んだホストに、観戦者 `ns`（JOIN の木の欄つき）が入り、全部を受け取った ACK を返した（`poll` はまだ）。
fn host_with_watchers(ns: &[(u16, JoinTree)]) -> (SpectateHost, Vec<Fake>) {
    let mut host = SpectateHost::with_capacity(host_cfg(), 1_024);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..100 {
        host.push(Record::Frame {
            frame: f,
            p1: f as u16,
            p2: 0,
        })
        .unwrap();
    }
    let mut sink = Sink::default();
    let mut fakes = Vec::new();
    for &(n, tree) in ns {
        let f = Fake::new(n);
        assert_eq!(
            host.on_datagram(addr(n), &f.join(tree), 0, &mut sink),
            HostEvent::Joined,
            "観戦者 {n}"
        );
        assert_eq!(
            host.on_datagram(addr(n), &f.ack(host.tail(), ON, 0), 0, &mut sink),
            HostEvent::Acked { progress: true }
        );
        fakes.push(f);
    }
    (host, fakes)
}

/// 宛先 `f` へ送った MEASURE の番号（送った順。送り直しも並ぶ）。
fn measures_to(sink: &Sink, f: &Fake) -> Vec<u16> {
    sink.opened(addr(f.n), &f.keys)
        .into_iter()
        .filter_map(|m| match m {
            Message::SpectateMeasure { turn } => Some(turn),
            _ => None,
        })
        .collect()
}

/// ホストを `from`〜`to` ms まで 1 ms ごとに回す。観戦者 `fakes` は 500 ms ごとに ACK を返して黙らない。
fn run_host(host: &mut SpectateHost, fakes: &[Fake], from: u32, to: u32, sink: &mut Sink) {
    for t in from..=to {
        if t.is_multiple_of(500) {
            for f in fakes {
                let next = host.ack_of(addr(f.n)).unwrap_or(0);
                host.on_datagram(addr(f.n), &f.ack(next, ON, 0), t, sink);
            }
        }
        host.poll(t, sink);
    }
}

/// 組 `pair` の 2 通を、`at` µs と `at + delta` µs に着いたとして渡す。
fn feed_pair(host: &mut SpectateHost, f: &Fake, turn: u16, pair: u8, at: u64, delta: u64) {
    assert!(
        host.on_pair(addr(f.n), &f.pair(turn, pair, false), at),
        "組 {pair} の 1 通目"
    );
    assert!(
        host.on_pair(addr(f.n), &f.pair(turn, pair, true), at + delta),
        "組 {pair} の 2 通目"
    );
}

/// 組 `k` の 1 通目が着く µs（組どうしを 100 ms 離す）。
fn pair_at(k: u8) -> u64 {
    1_000_000 + u64::from(k) * 100_000
}

#[test]
fn the_turn_goes_to_one_spectator_at_a_time_with_measure_sent_three_times_at_most() {
    let (mut host, f) =
        host_with_watchers(&[(1, relay_tree(1)), (2, relay_tree(2)), (3, relay_tree(3))]);
    let mut sink = Sink::default();
    // ホストが番を観戦者 1 に t = 1 で渡す。送り直しは最初の PAIR が来るまで 300 ms ごと、3 回まで。
    let mut t = 1;
    for (to, want) in [
        (MEASURE_RESEND_MS, 1),
        (MEASURE_RESEND_MS + 1, 2),
        (2 * MEASURE_RESEND_MS, 2),
        (2 * MEASURE_RESEND_MS + 1, 3),
        (MEASURE_CLOSE_MS, usize::from(MEASURE_SENDS)),
    ] {
        run_host(&mut host, &f, t, to, &mut sink);
        t = to + 1;
        assert_eq!(measures_to(&sink, &f[0]).len(), want, "{to} ms まで");
    }
    let first = measures_to(&sink, &f[0]);
    assert!(first.iter().all(|&x| x == first[0] && x != 0), "{first:?}");
    assert_eq!(host.measuring(), Some(addr(1)));
    assert!(
        measures_to(&sink, &f[1]).is_empty() && measures_to(&sink, &f[2]).is_empty(),
        "番を持つ人がいる間に別の人へ番を渡した"
    );
    // 2 秒で締め（PAIR が来ないので測れない）、次の poll でホストが番を次の 1 人へ渡す。
    run_host(&mut host, &f, t, t, &mut sink);
    assert_eq!(host.measuring(), None);
    assert_eq!(host.counts().uplinks_unmeasured, 1);
    run_host(&mut host, &f, t + 1, t + 1, &mut sink);
    assert_eq!(host.measuring(), Some(addr(2)));
    let second = measures_to(&sink, &f[1]);
    assert_eq!(second.len(), 1);
    assert_ne!(second[0], first[0], "番の番号が同じ");
    assert!(measures_to(&sink, &f[2]).is_empty());
    assert_eq!(host.counts().measures, 2);
}

#[test]
fn the_estimate_is_the_lower_median_and_ten_complete_pairs_close_the_turn_early() {
    let (mut host, f) = host_with_watchers(&[(1, relay_tree(1))]);
    let mut sink = Sink::default();
    host.poll(1, &mut sink);
    let turn = measures_to(&sink, &f[0])[0];
    // Δ（ms）を組の番号の順と揃えずに並べる。
    let deltas_ms = [60u64, 10, 90, 30, 100, 20, 80, 40, 70, 50];
    for (k, d) in deltas_ms.iter().enumerate() {
        feed_pair(&mut host, &f[0], turn, k as u8, pair_at(k as u8), d * 1_000);
    }
    host.poll(2, &mut sink);
    // 10 組の太さを小さい順に並べた 5 番目（Δ 60 ms）。平均（約 35,000）・上の中央値（Δ 50 ms の 24,000）とは違う。
    assert_eq!(host.uplink_of(addr(1)), Some(pair_bps(60_000)));
    assert_eq!(pair_bps(60_000), 20_000);
    assert_eq!(host.measuring(), None, "10 組そろったのに締めない");
    let c = host.counts();
    assert_eq!((c.uplinks_measured, c.pairs, c.pairs_dropped), (1, 20, 0));
}

#[test]
fn fewer_than_three_usable_pairs_measure_nothing_and_the_turn_comes_back_after_retry_ms() {
    let (mut host, f) = host_with_watchers(&[(1, relay_tree(1))]);
    let mut sink = Sink::default();
    host.poll(1, &mut sink);
    let first = measures_to(&sink, &f[0])[0];
    feed_pair(&mut host, &f[0], first, 0, pair_at(0), 10_000);
    feed_pair(&mut host, &f[0], first, 1, pair_at(1), 20_000);
    // 片方だけの組 2 つと、2 通目が先に着いた組（同じ µs。順の逆の組は使わない）。
    assert!(host.on_pair(addr(1), &f[0].pair(first, 2, false), pair_at(2)));
    assert!(host.on_pair(addr(1), &f[0].pair(first, 3, true), pair_at(3)));
    assert!(host.on_pair(addr(1), &f[0].pair(first, 4, true), pair_at(4)));
    assert!(host.on_pair(addr(1), &f[0].pair(first, 4, false), pair_at(4)));
    // 重なった便は数えない。
    assert!(!host.on_pair(addr(1), &f[0].pair(first, 0, true), pair_at(0) + 15_000));
    run_host(&mut host, &f, 2, MEASURE_CLOSE_MS, &mut sink);
    assert_eq!(host.measuring(), Some(addr(1)), "2 秒より前に締めた");
    let closed = 1 + MEASURE_CLOSE_MS;
    host.poll(closed, &mut sink);
    assert_eq!(host.measuring(), None);
    assert_eq!(host.uplink_of(addr(1)), Some(0));
    assert_eq!(host.counts().uplinks_unmeasured, 1);
    let mut sink = Sink::default();
    run_host(
        &mut host,
        &f,
        closed + 1,
        closed + MEASURE_RETRY_MS - 1,
        &mut sink,
    );
    assert!(
        measures_to(&sink, &f[0]).is_empty(),
        "{MEASURE_RETRY_MS} ms より前に番を回した"
    );
    host.poll(closed + MEASURE_RETRY_MS, &mut sink);
    let again = measures_to(&sink, &f[0]);
    assert_eq!(again.len(), 1, "{MEASURE_RETRY_MS} ms の後に番を回さない");
    assert_ne!(again[0], first, "新しい番号でない");
    // 前の番の組は数えない。
    assert!(!host.on_pair(addr(1), &f[0].pair(first, 7, false), pair_at(7)));
    // 2 回目: ちょうど 3 組で決まる（Δ 40・10・20 ms の真ん中 = 60,000 B/s）。
    for (k, d) in [(5u8, 40_000u64), (0, 10_000), (9, 20_000)] {
        feed_pair(&mut host, &f[0], again[0], k, pair_at(k) + 50_000_000, d);
    }
    host.poll(closed + MEASURE_RETRY_MS + MEASURE_CLOSE_MS, &mut sink);
    assert_eq!(host.uplink_of(addr(1)), Some(60_000));
    assert_eq!(host.counts().uplinks_measured, 1);
}

#[test]
fn a_spectator_gets_two_turns_at_most_until_it_joins_again() {
    let (mut host, f) = host_with_watchers(&[(1, relay_tree(1))]);
    let mut sink = Sink::default();
    let end = 3 * (MEASURE_CLOSE_MS + MEASURE_RETRY_MS);
    run_host(&mut host, &f, 1, end, &mut sink);
    let mut turns = measures_to(&sink, &f[0]);
    turns.dedup();
    assert_eq!(turns.len(), 2, "番の番号: {turns:?}");
    assert_eq!(
        (host.counts().measures, host.counts().uplinks_unmeasured),
        (2, 2)
    );
    // 抜けて入り直すと枠が新しいので、数え直す。
    let bye = f[0].sealed(&Message::Bye(ByeReason::Cancelled));
    assert_eq!(
        host.on_datagram(addr(1), &bye, end + 1, &mut sink),
        HostEvent::Left
    );
    let mut sink = Sink::default();
    assert_eq!(
        host.on_datagram(addr(1), &f[0].join(relay_tree(1)), end + 2, &mut sink),
        HostEvent::Joined
    );
    let next = host.tail();
    host.on_datagram(addr(1), &f[0].ack(next, ON, 0), end + 3, &mut sink);
    host.poll(end + 4, &mut sink);
    assert_eq!(measures_to(&sink, &f[0]).len(), 1);
    assert_eq!(host.counts().measures, 3);
}

#[test]
fn pairs_from_another_address_key_or_turn_are_not_counted() {
    let (mut host, f) = host_with_watchers(&[(1, relay_tree(1)), (2, relay_tree(2))]);
    let mut sink = Sink::default();
    host.poll(1, &mut sink);
    assert_eq!(host.measuring(), Some(addr(1)));
    let turn = measures_to(&sink, &f[0])[0];
    let forged = Fake {
        n: 1,
        keys: DirKeys::from_raw([9; 32], [9; 32]),
        seq: Seq::new(),
    };
    for k in 0..MEASURE_PAIRS {
        let at = pair_at(k);
        // 別の鍵（偽の組）。
        assert!(!host.on_pair(addr(1), &forged.pair(turn, k, false), at));
        assert!(!host.on_pair(addr(1), &forged.pair(turn, k, true), at + 10_000));
        // 番を持たない観戦者 2 の組。
        assert!(!host.on_pair(addr(2), &f[1].pair(turn, k, false), at));
        // 番を持つ観戦者の鍵でも、別の送り元から。
        assert!(!host.on_pair(addr(2), &f[0].pair(turn, k, true), at + 10_000));
        // 違う番の番号。
        assert!(!host.on_pair(addr(1), &f[0].pair(turn.wrapping_add(1), k, false), at));
    }
    let c = host.counts();
    assert_eq!(
        (c.pairs, c.pairs_dropped),
        (0, 5 * u64::from(MEASURE_PAIRS))
    );
    // 対照: 同じ番の本物の組は数える（1 組では決まらない）。
    feed_pair(&mut host, &f[0], turn, 3, pair_at(3), 30_000);
    assert_eq!(host.counts().pairs, 2);
    host.poll(1 + MEASURE_CLOSE_MS, &mut sink);
    assert_eq!(host.uplink_of(addr(1)), Some(0));
    assert_eq!(host.counts().uplinks_unmeasured, 1);
    // PAIR は `on_datagram` では開かずに捨てる（知らない送り元の封じた便として鍵を探しにいかない）。
    let stranger = SocketAddr::from(([127, 0, 0, 1], 48_999));
    assert_eq!(
        host.on_datagram(stranger, &f[0].pair(turn, 0, false), 2_100, &mut sink),
        HostEvent::Dropped(higanr_core::spectate::DropReason::Unexpected)
    );
    assert_eq!(host.counts().unknown_spectator, 0);
}

#[test]
fn no_turn_is_given_while_two_relays_are_known() {
    let thin = RELAY_MIN_BPS - 1;
    for (label, known, want) in [
        ("2 人分かっている", [(1, THICK), (3, THICK)], None),
        ("1 人は細い", [(1, THICK), (3, thin)], Some(addr(2))),
        ("1 人だけ", [(1, THICK), (1, THICK)], Some(addr(2))),
    ] {
        let (mut host, _f) =
            host_with_watchers(&[(1, relay_tree(1)), (2, relay_tree(2)), (3, relay_tree(3))]);
        for (n, bps) in known {
            host.note_uplink(addr(n), bps);
        }
        let mut sink = Sink::default();
        for t in 1..=10 {
            host.poll(t, &mut sink);
        }
        assert_eq!(host.measuring(), want, "{label}");
    }
}

#[test]
fn no_turn_is_given_while_the_spectate_sending_is_cut_or_paused() {
    let late = DelaySnapshot {
        at: 0,
        queue_ms: 50,
        base_ms: 20,
    };
    // 追いついた観戦者だけ（追いつきの送りが無い）: 往復が延びたら、ホストが番と送りを止める。
    for (label, delay, want) in [("止めた", Some(late), None), ("対照", None, Some(addr(1)))] {
        let (mut host, _f) = host_with_watchers(&[(1, relay_tree(1))]);
        host.note_delay(delay.map(|d| DelaySnapshot { at: 1, ..d }));
        host.poll(1, &mut Sink::default());
        assert_eq!(host.measuring(), want, "{label}");
        assert_eq!(host.throttle_state().1, delay.is_some(), "{label}");
    }
    // 遅れた観戦者（追いつきの送りがある）: 往復が延びたら全体の上限を半分にする（止めない）。
    for (label, delay, want) in [("絞った", Some(late), None), ("対照", None, Some(addr(1)))] {
        let mut host = SpectateHost::with_capacity(host_cfg(), 1_024);
        host.push(Record::Seg { seg: 0 }).unwrap();
        for k in 0..100 {
            host.push(Record::Frame {
                frame: k,
                p1: 0,
                p2: 0,
            })
            .unwrap();
        }
        let f = Fake::new(1);
        let mut sink = Sink::default();
        host.on_datagram(addr(1), &f.join(relay_tree(1)), 0, &mut sink);
        // ACK の前は番を渡さない。追いつきの送りだけが出る。
        host.poll(1, &mut sink);
        assert_eq!(host.measuring(), None, "{label}: ACK の前に番を渡した");
        host.on_datagram(addr(1), &f.ack(0, ON, 0), 2, &mut sink);
        host.note_delay(delay.map(|d| DelaySnapshot { at: 2, ..d }));
        host.poll(2, &mut sink);
        assert_eq!(host.measuring(), want, "{label}");
        let (level, paused) = host.throttle_state();
        assert!(!paused, "{label}");
        assert_eq!(level < 1_000_000, delay.is_some(), "{label}: {level}");
    }
}

#[test]
fn only_a_relay_capable_reachable_acked_spectator_with_an_unknown_uplink_gets_a_turn() {
    let nat = |symmetry| NatMark {
        symmetry,
        upnp: false,
    };
    for (label, tree, want) in [
        ("向く", relay_tree(1), Some(addr(1))),
        (
            "中継しない",
            JoinTree {
                relay_public: None,
                ..relay_tree(1)
            },
            None,
        ),
        (
            "シンメトリック",
            JoinTree {
                nat: nat(Symmetry::Yes),
                ..relay_tree(1)
            },
            None,
        ),
        (
            "種類不明",
            JoinTree {
                nat: nat(Symmetry::Unknown),
                ..relay_tree(1)
            },
            None,
        ),
    ] {
        let (mut host, _f) = host_with_watchers(&[(1, tree)]);
        host.poll(1, &mut Sink::default());
        assert_eq!(host.measuring(), want, "{label}");
    }
    // ACK で「紹介しないで」と返している人には渡さない（組を送らない）。
    let (mut host, f) = host_with_watchers(&[(1, relay_tree(1))]);
    let stop = RelayState {
        descendants: 0,
        no_intro: true,
    };
    host.on_datagram(
        addr(1),
        &f[0].ack(host.tail(), stop, 0),
        1,
        &mut Sink::default(),
    );
    host.poll(2, &mut Sink::default());
    assert_eq!(host.measuring(), None, "紹介しないで");
    // 上りが分かっている人には渡さない（細くても）。
    let (mut host, _f) = host_with_watchers(&[(1, relay_tree(1))]);
    host.note_uplink(addr(1), RELAY_MIN_BPS - 1);
    host.poll(1, &mut Sink::default());
    assert_eq!(host.measuring(), None, "分かっている");
}

#[test]
fn the_relay_side_gives_no_turn_to_its_children() {
    let mut r = relay_side();
    let c = Fake::new(2);
    let tree = JoinTree {
        relay_public: Some(RelayKey::new(&rseed(2)).public()),
        nat: REACHABLE,
        epoch: 1,
        ..JoinTree::NONE
    };
    assert!(r.admit(admit_for(&c, 1, 1, 10), 1));
    let mut sink = Sink::default();
    assert_eq!(
        r.on_datagram(addr(2), &c.join(tree), 2, &mut sink),
        HostEvent::Joined
    );
    let to_relay = Fake {
        n: 2,
        keys: child_relay_keys(&c),
        seq: Seq::new(),
    };
    assert_eq!(
        r.on_datagram(addr(2), &to_relay.ack(40, ON, 0), 3, &mut sink),
        HostEvent::Acked { progress: true }
    );
    for t in 4..=3_000 {
        r.poll(t, &mut sink);
    }
    assert_eq!(r.measuring(), None);
    assert_eq!(r.counts().measures, 0);
    assert!(measures_to(&sink, &to_relay).is_empty());
    // 対照: 同じ木の欄の観戦者を、元のホストは測る。
    let (mut host, _f) = host_with_watchers(&[(2, tree)]);
    host.poll(1, &mut Sink::default());
    assert_eq!(host.measuring(), Some(addr(2)));
}

#[test]
fn a_fake_uplink_replaces_the_estimate_once_two_pairs_are_usable() {
    let fake = 7 * RELAY_MIN_BPS;
    for (pairs, want) in [(2u8, fake), (1, 0)] {
        let (mut host, f) = host_with_watchers(&[(1, relay_tree(1))]);
        host.set_fake_uplink(Some(fake));
        let mut sink = Sink::default();
        host.poll(1, &mut sink);
        let turn = measures_to(&sink, &f[0])[0];
        for k in 0..pairs {
            feed_pair(&mut host, &f[0], turn, k, pair_at(k), 50_000);
        }
        host.poll(1 + MEASURE_CLOSE_MS, &mut sink);
        assert_eq!(host.uplink_of(addr(1)), Some(want), "{pairs} 組");
    }
}

// ---------------------------- 観戦者の測り ----------------------------

/// 元のホストから STREAM を 1 通受けて接続した、中継できる観戦者 `n`。
fn joined_relay_watcher(n: u16, seq: &Seq) -> Watcher {
    let mut w = watcher(n);
    w.set_relay_public(Some(RelayKey::new(&rseed(n)).public()));
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: true,
        count: 0,
        records: [Record::FILLER; wire::MAX_STREAM_RECORDS],
    });
    w.on_datagram(
        HOST,
        &from_host(&w, &stream, None, seq),
        1,
        &mut Sink::default(),
    );
    assert_eq!(w.host_addr(), Some(HOST));
    w
}

/// 観戦者 `w` を `from`〜`to` ms まで回し、ホストへ送った PAIR を（送った時刻, 中身, 長さ）で返す（ホストの側の鍵で開く）。
fn pairs_sent(w: &mut Watcher, from: u32, to: u32) -> Vec<(u32, SpectatePair, usize)> {
    let keys = Ephemeral::from_seed(&host_cfg().key_seed)
        .dir_keys(&w.public_key(), true, SID, &key())
        .unwrap();
    let mut got = Vec::new();
    for t in from..=to {
        let mut sink = Sink::default();
        w.poll(t, &mut sink);
        for (a, b) in &sink.0 {
            if let Ok(p) = wire::decode(&key(), Some(&keys.opener()), b)
                && let Message::SpectatePair(x) = p.msg
            {
                assert_eq!(*a, HOST);
                got.push((t, x, b.len()));
            }
        }
    }
    got
}

fn measure(turn: u16) -> Message {
    Message::SpectateMeasure { turn }
}

#[test]
fn a_watcher_sends_ten_pairs_of_full_datagrams_every_100_ms_and_restarts_on_a_new_turn() {
    let seq = Seq::new();
    let mut w = joined_relay_watcher(1, &seq);
    let tell = |w: &mut Watcher, turn: u16, at: u32| {
        w.on_datagram(
            HOST,
            &from_host(w, &measure(turn), None, &seq),
            at,
            &mut Sink::default(),
        );
    };
    tell(&mut w, 5, 10);
    let mut got = pairs_sent(&mut w, 10, 300);
    // 同じ番号の送り直しは無視する（始め直さない）。
    tell(&mut w, 5, 300);
    got.extend(pairs_sent(&mut w, 301, 1_200));
    assert_eq!(got.len(), 2 * usize::from(MEASURE_PAIRS));
    for (i, (t, p, len)) in got.iter().enumerate() {
        let k = (i / 2) as u8;
        assert_eq!((p.turn, p.pair, p.second), (5, k, i % 2 == 1), "{i} 通目");
        assert_eq!(*t, 10 + u32::from(k) * PAIR_GAP_MS, "{i} 通目");
        assert_eq!(*len, 1_200, "{i} 通目");
    }
    // 新しい番号を受けたら、前の番をやめて組 0 から始め直す。
    tell(&mut w, 6, 1_300);
    let six = pairs_sent(&mut w, 1_300, 1_450);
    tell(&mut w, 7, 1_450);
    let seven = pairs_sent(&mut w, 1_451, 2_500);
    let seen = |v: &[(u32, SpectatePair, usize)]| -> Vec<(u32, u16, u8)> {
        v.iter()
            .filter(|g| g.1.second)
            .map(|g| (g.0, g.1.turn, g.1.pair))
            .collect()
    };
    assert_eq!(seen(&six), vec![(1_300, 6, 0), (1_400, 6, 1)]);
    let seven = seen(&seven);
    assert_eq!(seven.len(), usize::from(MEASURE_PAIRS));
    assert_eq!(seven[0], (1_451, 7, 0));
    assert!(seven.iter().all(|g| g.1 == 7));
    let c = w.tree_counts();
    assert_eq!((c.measures_taken, c.measures_ignored), (3, 0));
    assert_eq!(c.pairs_sent, 2 * (10 + 2 + 10));
}

#[test]
fn a_watcher_that_cannot_relay_follows_a_relay_or_has_children_sends_no_pairs() {
    let seq = Seq::new();
    let one = RelayState {
        descendants: 1,
        no_intro: false,
    };
    for label in ["中継しない", "親が中継役", "子がいる", "依頼を控えた"] {
        let mut w = joined_relay_watcher(1, &seq);
        match label {
            "中継しない" => w.set_relay_public(None),
            "親が中継役" => {
                w.on_datagram(
                    HOST,
                    &from_host(&w, &intro(1, 0, 2), None, &seq),
                    2,
                    &mut Sink::default(),
                );
                assert!(!w.parent_is_home());
            }
            "子がいる" => w.set_relay_state(one),
            _ => {
                w.set_relay_state(ON);
                w.on_datagram(
                    HOST,
                    &from_host(&w, &admit(1), None, &seq),
                    2,
                    &mut Sink::default(),
                );
                assert_eq!(w.tree_counts().admits_taken, 1);
            }
        }
        w.on_datagram(
            HOST,
            &from_host(&w, &measure(3), None, &seq),
            3,
            &mut Sink::default(),
        );
        assert!(pairs_sent(&mut w, 3, 1_500).is_empty(), "{label}");
        let c = w.tree_counts();
        assert_eq!((c.measures_taken, c.measures_ignored), (0, 1), "{label}");
    }
    // 対照と、番の途中で子ができたらそこでやめる。
    let mut w = joined_relay_watcher(1, &seq);
    w.on_datagram(
        HOST,
        &from_host(&w, &measure(3), None, &seq),
        3,
        &mut Sink::default(),
    );
    assert_eq!(pairs_sent(&mut w, 3, 150).len(), 4);
    w.set_relay_state(one);
    assert!(pairs_sent(&mut w, 151, 1_500).is_empty());
}

// ---------------------------- 上りを絞った模型 ----------------------------

/// 細い観戦者（目安の 2/3）と太い観戦者 2 人。
const THIN: u32 = RELAY_MIN_BPS * 2 / 3;
const THICKER: u32 = 4 * RELAY_MIN_BPS;

/// 見積もりが本当の太さの ±2% に入る。
fn assert_near(sim: &Sim, n: u16, bps: u32) {
    let got = sim.host.uplink_of(addr(n)).expect("観戦者");
    let err = got.abs_diff(bps);
    assert!(err * 50 <= bps, "観戦者 {n}: 見積もり {got}・本当 {bps}");
}

#[test]
fn measured_uplinks_put_a_thin_spectator_below_the_relay_line_and_thick_ones_above() {
    let mut sim = Sim::measured(0x5A1, CALM, 600);
    sim.add(1, 100, true, THIN);
    sim.add(2, 300, true, THICK);
    sim.add(3, 500, true, THICKER);
    // 2 人目と 3 人目で中継役に向く人が 2 人そろうので、4 人目は測らない。
    sim.add(4, 600, true, THICKER);
    sim.run_to(12_000);
    assert_near(&sim, 1, THIN);
    assert_near(&sim, 2, THICK);
    assert_near(&sim, 3, THICKER);
    let up = |n| sim.host.uplink_of(addr(n)).unwrap();
    assert!(
        up(1) < RELAY_MIN_BPS && RELAY_MIN_BPS <= up(2),
        "{} {}",
        up(1),
        up(2)
    );
    assert_eq!(up(4), 0);
    let who: Vec<SocketAddr> = sim.turns.iter().map(|x| x.1).collect();
    assert_eq!(who, vec![addr(1), addr(2), addr(3)]);
    let c = sim.host.counts();
    assert_eq!(
        (c.measures, c.uplinks_measured, c.uplinks_unmeasured),
        (3, 3, 0)
    );
    // 番は前の番を締めてから渡す（1 人ずつ）。番 1 回は 2 秒以内に締まる。
    for w in sim.turns.windows(2) {
        assert!(w[1].0 - w[0].0 <= MEASURE_CLOSE_MS + 1, "{:?}", sim.turns);
    }
}

#[test]
fn a_tree_forms_from_measured_uplinks() {
    let mut sim = Sim::measured(0x7A3, LOSSY, 1_200);
    sim.add(1, 100, true, THIN);
    sim.add(2, 300, true, THICK);
    sim.add(3, 500, true, THICKER);
    sim.run_to(8_000);
    assert_eq!(sim.host.counts().uplinks_measured, 3);
    for n in 4..=9 {
        sim.add(n, 8_000 + 200 * u32::from(n - 3), false, 0);
    }
    sim.run_until_caught_up(60_000);
    let relays = sim.relays();
    assert_eq!(
        relays.iter().map(|r| (r.0, r.1.len())).collect::<Vec<_>>(),
        vec![(3, 4), (2, 2)],
        "太い 2 人が、太い順に子を持つ"
    );
    assert!(!sim.host.is_relay_of(addr(1)));
    for n in 1..=9 {
        sim.assert_column(n, 0);
    }
    sim.assert_counts();
    assert_eq!(sim.deep, None);
    assert!(sim.net.dropped > 0, "落としが効いていない");
}

#[test]
fn lost_pairs_still_decide_with_three_and_otherwise_the_turn_comes_back_after_retry_ms() {
    // 組 0〜6 の 1 通目を落とす: 残るのは組 7〜9 の 3 組。
    let mut sim = Sim::measured(0x3B, CALM, 600);
    sim.net.pair_drop = Some((addr(1), |k| k % 2 == 0 && k / 2 < 7));
    sim.add(1, 100, true, THICK);
    sim.run_to(6_000);
    assert_near(&sim, 1, THICK);
    assert_eq!(sim.turns.len(), 1);
    // 1 回目は組 0・1 だけを残す（2 組では決まらない）。2 回目は全部届く。
    let mut sim = Sim::measured(0x3C, CALM, 600);
    sim.net.pair_drop = Some((addr(1), |k| (4..20).contains(&k)));
    sim.add(1, 100, true, THICK);
    sim.run_to(MEASURE_RETRY_MS);
    assert_eq!(sim.host.uplink_of(addr(1)), Some(0));
    assert_eq!(sim.host.counts().uplinks_unmeasured, 1);
    sim.run_to(MEASURE_RETRY_MS + 8_000);
    assert_near(&sim, 1, THICK);
    assert_eq!(sim.turns.len(), 2);
    assert_eq!(
        sim.turns[1].0 - sim.turns[0].0,
        MEASURE_CLOSE_MS + MEASURE_RETRY_MS,
        "締めてから {MEASURE_RETRY_MS} ms で番を回す"
    );
    assert_eq!(sim.host.counts().uplinks_measured, 1);
}

#[test]
fn a_pair_within_one_ms_counts_as_the_cap() {
    assert_eq!(PAIR_CAP, 1_200_000, "1,200 B ÷ 1 ms");
    let (mut host, f) = host_with_watchers(&[(1, relay_tree(1))]);
    let mut sink = Sink::default();
    host.poll(1, &mut sink);
    let turn = measures_to(&sink, &f[0])[0];
    for (k, d) in [(0u8, 0u64), (1, 500), (2, 999)] {
        feed_pair(&mut host, &f[0], turn, k, pair_at(k), d);
    }
    host.poll(1 + MEASURE_CLOSE_MS, &mut sink);
    assert_eq!(host.uplink_of(addr(1)), Some(PAIR_CAP));
}
