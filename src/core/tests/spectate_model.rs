//! 観戦の 2 つの模型（ホストの送りウィンドウ `SpectateHost` ↔ 観戦者の組み立て `Watcher`）を、落とす・重ねる・順を入れ替える・
//! 遅れて JOIN する・観戦者が黙る の乱れのネットワークで繋ぎ、観戦者が受け取った記録の列がホストの積んだ列と一致することを見る。
//! あわせて、ウィンドウ・リングバッファ・観戦者の数の境目と、JOIN のガードを直に見る。
//!
//! ネットワークは 1 ms ごとに進む固定 seed の模型（実際の UDP・時計は使わない）。ホストは 60 fps くらい（16 ms に 1 記録）で記録を積み、
//! 両側が毎 ms `poll` を呼ぶ。

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;
use higanr_core::link::Outlet;
use higanr_core::seal::{DirKeys, Ephemeral, KeySeed, Sender, Seq};
use higanr_core::spectate::{
    ASM_CAP, Assembler, DropReason, HEARTBEAT_MS, HostConfig, HostEvent, JOIN_RESEND_MS,
    MAX_SPECTATORS, PushError, REFUSE_GAP_MS, REJOIN_MS, RESEND_INIT_MS, RESEND_MAX_MS,
    RESEND_MIN_MS, SEND_WINDOW, SILENT_MS, SpectateHost, Watcher, WatcherConfig,
};
use higanr_core::wire::{
    self, ByeReason, MAX_DATAGRAM, MAX_STREAM_RECORDS, Message, Record, Seat, Select, SpectateJoin,
    SpectateStream, Step, StepKind,
};
use std::net::SocketAddr;

const HOST: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    47_000,
);
const SID: u32 = 0x5EC7_0001;
/// 上限の次の観戦者の番号（addr・nonce に使う）。
const OVER: u16 = MAX_SPECTATORS as u16 + 1;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 47_100 + n))
}

fn secret() -> Secret {
    Secret([0x5A; 16])
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

/// 観戦者の使い捨ての鍵の種。JOIN の nonce から決めるので、模型の側で、その観戦者とホストの間の鍵を作れる（`wkeys`）。
fn wseed(nonce: u64) -> KeySeed {
    let mut s = [0u8; 32];
    for (i, b) in s.iter_mut().enumerate() {
        *b = (nonce.rotate_left(i as u32 * 3) as u8) ^ (i as u8) ^ 0xA7;
    }
    KeySeed(s)
}

/// nonce `nonce` の観戦者の側の向きごとの鍵（送る向き = 観戦者 → ホスト、受ける向き = ホスト → 観戦者）。
fn wkeys(nonce: u64) -> DirKeys {
    remember(nonce);
    Ephemeral::from_seed(&wseed(nonce))
        .dir_keys(&host_cfg().public(), false, SID, &key())
        .expect("鍵が作れる")
}

thread_local! {
    /// この検査のスレッドで使った観戦者の nonce（`Sink::take` が、ホストと観戦者の送った封じた便を外から開くため）。
    static NONCES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn remember(nonce: u64) {
    NONCES.with(|v| {
        let mut v = v.borrow_mut();
        if !v.contains(&nonce) {
            v.push(nonce);
        }
    });
}

fn watcher_cfg(n: u16) -> WatcherConfig {
    let nonce = 0xA000 + u64::from(n);
    remember(nonce);
    WatcherConfig {
        session_id: SID,
        secret: secret(),
        nonce,
        name: Name::new(&format!("w{n}")),
        host: Candidates::from_addrs(&[HOST]).expect("loopback"),
        host_public: host_cfg().public(),
        key_seed: wseed(nonce),
    }
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
    seq: u64,
    from: SocketAddr,
    to: SocketAddr,
    bytes: Vec<u8>,
}

struct Net {
    q: Vec<InFlight>,
    rng: Rng,
    seq: u64,
    /// 片道の最小（ms。1 以上）と、それに足すばらつきの最大。ばらつきが順の入れ替わりを作る。
    base: u32,
    jitter: u32,
    drop_pct: u64,
    dup_pct: u64,
    /// 黙った観戦者（この送り元・宛先の通信は全部落とす）。
    dead: Vec<SocketAddr>,
    sent: u64,
    host_bytes: u64,
    watcher_pkts: u64,
    watcher_bytes: u64,
    dropped: u64,
    /// 宛先ごとに、それまでに届けた最大の送出番号（`seq`）と、それより古い通が後から届いた回数（順の入れ替わり）。
    max_seq: std::collections::HashMap<SocketAddr, u64>,
    reordered: u64,
}

impl Net {
    fn new(seed: u64, base: u32, jitter: u32, drop_pct: u64, dup_pct: u64) -> Net {
        Net {
            q: Vec::new(),
            rng: Rng(seed),
            seq: 0,
            base,
            jitter,
            drop_pct,
            dup_pct,
            dead: Vec::new(),
            sent: 0,
            host_bytes: 0,
            watcher_pkts: 0,
            watcher_bytes: 0,
            dropped: 0,
            max_seq: std::collections::HashMap::new(),
            reordered: 0,
        }
    }

    fn send(&mut self, now: u32, from: SocketAddr, to: SocketAddr, bytes: &[u8]) {
        self.sent += 1;
        if from == HOST {
            self.host_bytes += bytes.len() as u64;
        } else {
            self.watcher_pkts += 1;
            self.watcher_bytes += bytes.len() as u64;
        }
        if self.dead.contains(&from)
            || self.dead.contains(&to)
            || self.rng.below(100) < self.drop_pct
        {
            self.dropped += 1;
            return;
        }
        let copies = if self.rng.below(100) < self.dup_pct {
            2
        } else {
            1
        };
        for _ in 0..copies {
            let at = now + self.base + self.rng.below(u64::from(self.jitter) + 1) as u32;
            self.seq += 1;
            self.q.push(InFlight {
                at,
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

struct W {
    addr: SocketAddr,
    w: Watcher,
    got: Vec<(u32, Record)>,
    got_at: Vec<u32>,
    start_at: u32,
    join_start: Option<u32>,
    consume_period: u32,
    next_consume: u32,
    max_pending: usize,
    /// 0 でなければ、観戦者は届いた通を溜め、この周期（ms）の `tick_phase` の時刻にまとめて受ける（ゲームのスレッドが
    /// 1 フレームに 1 回だけ受け・ACK を返す dll の観戦者の形）。0 は届いたその ms に受ける。
    tick_period: u32,
    tick_phase: u32,
    inbox: Vec<(SocketAddr, Vec<u8>)>,
}

struct Sim {
    now: u32,
    net: Net,
    host: SpectateHost,
    watchers: Vec<W>,
    script: Vec<Record>,
    next_script: usize,
    next_push: u32,
    period: u32,
    log: Vec<Record>,
    push_at: Vec<u32>,
}

impl Sim {
    fn new(net: Net, host: SpectateHost, script: Vec<Record>) -> Sim {
        Sim {
            now: 0,
            net,
            host,
            watchers: Vec::new(),
            script,
            next_script: 0,
            next_push: 1,
            period: 16,
            log: Vec::new(),
            push_at: Vec::new(),
        }
    }

    fn add_watcher(&mut self, n: u16, start_at: u32, consume_period: u32, asm_cap: usize) {
        self.watchers.push(W {
            addr: addr(n),
            w: Watcher::with_assembler(watcher_cfg(n), Assembler::with_capacity(asm_cap)),
            got: Vec::new(),
            got_at: Vec::new(),
            start_at,
            join_start: None,
            consume_period,
            next_consume: 0,
            max_pending: 0,
            tick_period: 0,
            tick_phase: 0,
            inbox: Vec::new(),
        });
    }

    fn set_tick(&mut self, idx: usize, period: u32, phase: u32) {
        self.watchers[idx].tick_period = period;
        self.watchers[idx].tick_phase = phase;
    }

    fn tick(&mut self) {
        self.now += 1;
        let now = self.now;
        if self.next_script < self.script.len() && now >= self.next_push {
            let rec = self.script[self.next_script];
            match self.host.push(rec) {
                Ok(idx) => {
                    assert_eq!(idx as usize, self.log.len(), "番号が積んだ数と合わない");
                    self.log.push(rec);
                    self.push_at.push(now);
                }
                Err(e) => {
                    panic!("積めなかった: {e:?}（この検査のリングバッファは溢れない大きさのはず）")
                }
            }
            self.next_script += 1;
            self.next_push = now + self.period;
        }
        let mut q = std::mem::take(&mut self.net.q);
        q.sort_by_key(|p| (p.at, p.seq));
        let (due, later): (Vec<_>, Vec<_>) = q.into_iter().partition(|p| p.at <= now);
        self.net.q = later;
        for p in due {
            let m = self.net.max_seq.entry(p.to).or_insert(0);
            if p.seq < *m {
                self.net.reordered += 1;
            } else {
                *m = p.seq;
            }
            if p.to == HOST {
                let mut tx = Tx {
                    net: &mut self.net,
                    from: HOST,
                    now,
                };
                let ev = self.host.on_datagram(p.from, &p.bytes, now, &mut tx);
                if ev == HostEvent::Joined
                    && let Some(w) = self.watchers.iter_mut().find(|w| w.addr == p.from)
                    && w.join_start.is_none()
                {
                    w.join_start = Some(self.host.seg_start().max(self.host.head()));
                }
            } else if let Some(w) = self.watchers.iter_mut().find(|w| w.addr == p.to)
                && now >= w.start_at
            {
                if w.tick_period > 0 {
                    w.inbox.push((p.from, p.bytes));
                    continue;
                }
                let mut tx = Tx {
                    net: &mut self.net,
                    from: w.addr,
                    now,
                };
                w.w.on_datagram(p.from, &p.bytes, now, &mut tx);
            }
        }
        {
            let mut tx = Tx {
                net: &mut self.net,
                from: HOST,
                now,
            };
            self.host.poll(now, &mut tx);
        }
        for w in &mut self.watchers {
            if now < w.start_at {
                continue;
            }
            let mut tx = Tx {
                net: &mut self.net,
                from: w.addr,
                now,
            };
            if w.tick_period > 0 && now % w.tick_period == w.tick_phase {
                for (from, bytes) in std::mem::take(&mut w.inbox) {
                    w.w.on_datagram(from, &bytes, now, &mut tx);
                }
            }
            w.w.poll(now, &mut tx);
            w.max_pending = w.max_pending.max(w.w.pending());
            if w.consume_period == 0 {
                while let Some(r) = w.w.pop_indexed() {
                    w.got.push(r);
                    w.got_at.push(now);
                }
            } else if now >= w.next_consume
                && let Some(r) = w.w.pop_indexed()
            {
                w.got.push(r);
                w.got_at.push(now);
                w.next_consume = now + w.consume_period;
            }
        }
    }

    fn caught_up(&self) -> bool {
        self.next_script == self.script.len()
            && self.watchers.iter().all(|w| {
                self.net.dead.contains(&w.addr)
                    || w.got.last().map(|g| g.0 + 1) == Some(self.host.tail())
            })
    }

    fn run_until_caught_up(&mut self, limit_ms: u32) {
        while !self.caught_up() {
            assert!(
                self.now < limit_ms,
                "{limit_ms} ms で追いつかなかった: 積んだ {}/{}・観戦者の受け取り {:?}・ネットワーク {}/{} 落とした",
                self.next_script,
                self.script.len(),
                self.watchers
                    .iter()
                    .map(|w| w.got.len())
                    .collect::<Vec<_>>(),
                self.net.dropped,
                self.net.sent
            );
            self.tick();
        }
    }

    fn run_for(&mut self, ms: u32) {
        let end = self.now + ms;
        while self.now < end {
            self.tick();
        }
    }
}

fn select_of(rng: &mut Rng) -> Select {
    Select {
        match_no: rng.next() as u16,
        chara: rng.below(16) as u8,
        color: rng.below(2) as u8,
        life: rng.below(6) as u8,
        charge: rng.below(2) as u8,
        map: rng.below(16) as u8,
        difficulty: rng.below(4) as u8,
        cpu_1p: 0,
        cpu_2p: 0,
        background: 0,
        rng_seed: rng.next() as u16,
        option_digest: rng.next() as u32,
    }
}

/// 記録の列の台本: 各 seg は `Seg` → 選択の画面（MENU の入力ワード・STEP・両席の SELECT）→ マッチの入力ワード。最後に `End`。
fn make_script(rng: &mut Rng, segs: u16, frames: u64) -> Vec<Record> {
    let mut v = Vec::new();
    for seg in 0..segs {
        v.push(Record::Seg { seg });
        // dll のホストと同じく、各 seg の `Seg` の直後に席ごとの名前を積む（遅れて入った観戦者も受け取る）。
        v.push(Record::name(Seat::P1, "p1-name".as_bytes()));
        v.push(Record::name(Seat::P2, "p2-name".as_bytes()));
        for k in 0..(20 + rng.below(20)) {
            v.push(Record::Menu {
                p1: rng.next() as u16,
                p2: rng.next() as u16,
            });
            if k % 9 == 4 {
                v.push(Record::Step(Step {
                    step: v.len() as u16,
                    kind: StepKind::Decide,
                    value: rng.below(32) as u8,
                    screen: 7 + rng.below(3) as u8,
                }));
            }
        }
        let sel = select_of(rng);
        v.push(Record::Select {
            seat: Seat::P1,
            sel,
        });
        v.push(Record::Select {
            seat: Seat::P2,
            sel: Select {
                chara: rng.below(16) as u8,
                ..sel
            },
        });
        for f in 0..(frames + rng.below(frames / 4 + 1)) {
            v.push(Record::Frame {
                frame: f as u32,
                p1: rng.next() as u16,
                p2: rng.next() as u16,
            });
        }
    }
    v.push(Record::End(ByeReason::Finished));
    v
}

fn assert_matches_host(sim: &Sim, w: &W, from: u32, label: &str) {
    let tail = sim.host.tail() as usize;
    assert_eq!(
        w.got.len(),
        tail - from as usize,
        "{label}: 受け取った数（番号 {from} から {tail} の手前まで）"
    );
    for (k, (idx, rec)) in w.got.iter().enumerate() {
        assert_eq!(
            *idx as usize,
            from as usize + k,
            "{label}: 番号が連続でない"
        );
        assert_eq!(
            *rec, sim.log[*idx as usize],
            "{label}: 番号 {idx} の記録が違う"
        );
    }
}

struct Chaos {
    name: &'static str,
    base: u32,
    jitter: u32,
    drop_pct: u64,
    dup_pct: u64,
}

const CHAOS: [Chaos; 4] = [
    Chaos {
        name: "乱れなし",
        base: 1,
        jitter: 0,
        drop_pct: 0,
        dup_pct: 0,
    },
    Chaos {
        name: "落とす 20% ＋ 重ねる 10% ＋ 順の入れ替わり（ばらつき 40 ms）",
        base: 5,
        jitter: 40,
        drop_pct: 20,
        dup_pct: 10,
    },
    Chaos {
        name: "重い（落とす 40% ＋ 重ねる 20% ＋ ばらつき 90 ms）",
        base: 10,
        jitter: 90,
        drop_pct: 40,
        dup_pct: 20,
    },
    Chaos {
        name: "順の入れ替わりだけ（落とさない。ばらつき 60 ms）",
        base: 2,
        jitter: 60,
        drop_pct: 0,
        dup_pct: 0,
    },
];

const SEEDS: [u64; 4] = [0x11, 0x2222, 0x33_3333, 0xDEAD_BEEF];

#[test]
fn a_spectator_from_the_start_gets_exactly_the_hosts_records() {
    let mut runs = 0;
    for c in &CHAOS {
        for &seed in &SEEDS {
            for consume_period in [0, 16] {
                let mut rng = Rng(seed ^ 0xF00D);
                let script = make_script(&mut rng, 3, 120);
                let mut sim = Sim::new(
                    Net::new(seed, c.base, c.jitter, c.drop_pct, c.dup_pct),
                    SpectateHost::with_capacity(host_cfg(), 4_096),
                    script,
                );
                sim.add_watcher(1, 0, consume_period, ASM_CAP);
                sim.run_until_caught_up(400_000);
                assert_matches_host(
                    &sim,
                    &sim.watchers[0],
                    0,
                    &format!("{} seed {seed:#x} 消費 {consume_period} ms", c.name),
                );
                let a = sim.watchers[0].w.assembler_counts();
                assert_eq!(a.jumps, 0, "リングバッファが溢れない実行で飛んだ");
                runs += 1;
                if c.drop_pct > 0 {
                    assert!(sim.net.dropped > 0, "{}: 落とせていない", c.name);
                    assert!(sim.host.counts().resends > 0, "{}: 送り直しが 0", c.name);
                }
                // 重ねるネットワーク・落とすネットワークでは重複が来る。ばらつきのあるネットワークでは順の入れ替わりが起きている（落とさず重ねもしないネットワークに重複は来ない）。
                if c.drop_pct > 0 || c.dup_pct > 0 {
                    assert!(
                        a.dup > 0,
                        "{}: 重複が 1 つも来ていない（乱れが効いていない）",
                        c.name
                    );
                }
                if c.jitter >= 40 {
                    assert!(
                        sim.net.reordered > 0,
                        "{}: 順の入れ替わりが 1 つも起きていない（乱れが効いていない）",
                        c.name
                    );
                }
            }
        }
    }
    assert_eq!(runs, 32);
}

#[test]
fn a_late_joiner_starts_from_the_current_seg_head() {
    for &seed in &SEEDS {
        let mut rng = Rng(seed ^ 0xBEEF);
        let script = make_script(&mut rng, 3, 150);
        let join_at = (script.len() as u32 * 45 / 100) * 16;
        let mut sim = Sim::new(
            Net::new(seed, 4, 30, 15, 10),
            SpectateHost::with_capacity(host_cfg(), 4_096),
            script,
        );
        sim.add_watcher(1, join_at, 0, ASM_CAP);
        sim.run_until_caught_up(400_000);
        let w = &sim.watchers[0];
        let start = w.join_start.expect("ホストが JOIN を受けていない");
        assert!(
            start > 0,
            "seg 0 の間に入ってしまった（この検査の狙いは遅れた JOIN）"
        );
        assert!(
            matches!(sim.log[start as usize], Record::Seg { .. }),
            "流れの始まりが Seg でない"
        );
        assert_matches_host(&sim, w, start, &format!("seed {seed:#x}"));
        assert_eq!(
            w.w.assembler_counts().jumps,
            1,
            "遅れた JOIN は頭へ 1 回飛ぶ"
        );
    }
}

/// 実際の記録の並びの台本: 各 seg は [前置き（MENU・STEP・両席の SELECT）→] `Seg` → マッチの入力ワード。前置きがあるのは seg 0 と偶数の seg
/// （奇数の seg は再戦で、マッチの入力ワードの後にすぐ `Seg`）。最後に `End`。戻り値は（記録の列、各 seg の始まりの番号＝前置きがあれば前置きの最初、
/// 無ければ `Seg`、各 `Seg` の番号）。
fn make_real_script(rng: &mut Rng, segs: u16, frames: u64) -> (Vec<Record>, Vec<u32>, Vec<u32>) {
    let mut v = Vec::new();
    let mut heads = Vec::new();
    let mut seg_at = Vec::new();
    for seg in 0..segs {
        heads.push(v.len() as u32);
        if seg % 2 == 0 {
            for k in 0..(6 + rng.below(6)) {
                v.push(Record::Menu {
                    p1: rng.next() as u16,
                    p2: rng.next() as u16,
                });
                if k % 4 == 1 {
                    v.push(Record::Step(Step {
                        step: v.len() as u16,
                        kind: StepKind::Decide,
                        value: rng.below(32) as u8,
                        screen: 7 + rng.below(3) as u8,
                    }));
                }
            }
            let sel = select_of(rng);
            v.push(Record::Select {
                seat: Seat::P1,
                sel,
            });
            v.push(Record::Select {
                seat: Seat::P2,
                sel: Select {
                    chara: rng.below(16) as u8,
                    ..sel
                },
            });
        }
        seg_at.push(v.len() as u32);
        v.push(Record::Seg { seg });
        v.push(Record::name(Seat::P1, "p1-name".as_bytes()));
        v.push(Record::name(Seat::P2, "p2-name".as_bytes()));
        for f in 0..(frames + rng.below(frames / 4 + 1)) {
            v.push(Record::Frame {
                frame: f as u32,
                p1: rng.next() as u16,
                p2: rng.next() as u16,
            });
        }
    }
    v.push(Record::End(ByeReason::Finished));
    (v, heads, seg_at)
}

#[test]
fn a_late_joiner_gets_the_preamble_of_the_current_seg() {
    for &seed in &SEEDS {
        for join_seg in [0usize, 2, 3] {
            let mut rng = Rng(seed ^ 0xC0DE);
            let (script, heads, seg_at) = make_real_script(&mut rng, 4, 120);
            // その seg の `Seg` から 40 記録（約 640 ms）入った所で JOIN を送り始める。送り直し（250 ms）が何回か落ちても seg の中にいる。
            let join_idx = seg_at[join_seg] + 40;
            let mut sim = Sim::new(
                Net::new(seed, 4, 30, 15, 10),
                SpectateHost::with_capacity(host_cfg(), 4_096),
                script,
            );
            sim.add_watcher(1, join_idx * 16, 0, ASM_CAP);
            sim.run_until_caught_up(400_000);
            let w = &sim.watchers[0];
            let start = w.join_start.expect("ホストが JOIN を受けていない");
            let label = format!("seed {seed:#x} seg {join_seg}");
            assert_eq!(
                start, heads[join_seg],
                "{label}: 流れの始まりが seg の始まりでない"
            );
            match join_seg {
                3 => assert!(matches!(sim.log[start as usize], Record::Seg { seg: 3 })),
                _ => assert!(matches!(sim.log[start as usize], Record::Menu { .. })),
            }
            if join_seg == 2 {
                let selects = w
                    .got
                    .iter()
                    .filter(|(_, r)| matches!(r, Record::Select { .. }))
                    .count();
                assert_eq!(selects, 2, "{label}: SELECT を受け取れていない");
            }
            assert_matches_host(&sim, w, start, &label);
        }
    }
}

#[test]
fn a_slow_consumer_holds_the_window_without_overrun() {
    for &seed in &SEEDS {
        let mut rng = Rng(seed ^ 0x5107);
        let script = make_script(&mut rng, 2, 100);
        let mut sim = Sim::new(
            Net::new(seed, 3, 10, 10, 5),
            SpectateHost::with_capacity(host_cfg(), 4_096),
            script,
        );
        sim.add_watcher(1, 0, 32, 8);
        sim.run_until_caught_up(600_000);
        let w = &sim.watchers[0];
        assert_matches_host(&sim, w, 0, &format!("seed {seed:#x}"));
        assert!(
            w.max_pending <= 8,
            "連続の分が置き場を越えた: {}",
            w.max_pending
        );
        assert_eq!(
            w.w.assembler_counts().beyond,
            0,
            "ウィンドウの中の記録が置き場の外に出た（ホストがウィンドウを越えて流した）"
        );
        assert!(sim.now > sim.script.len() as u32 * 16 + 1_000);
    }
}

#[test]
fn a_spectator_behind_the_ring_jumps_to_a_seg_head() {
    for &seed in &SEEDS {
        let mut rng = Rng(seed ^ 0x7217);
        let script = make_script(&mut rng, 6, 70); // 1 seg ≒ 120 記録 < リングバッファ 300
        let mut sim = Sim::new(
            Net::new(seed, 2, 10, 5, 5),
            SpectateHost::with_capacity(host_cfg(), 300),
            script,
        );
        sim.add_watcher(1, 0, 0, ASM_CAP);
        sim.run_for(2_000);
        sim.net.dead.push(addr(1)); // 6 秒黙る
        sim.run_for(6_000);
        sim.net.dead.clear();
        sim.run_until_caught_up(600_000);
        let w = &sim.watchers[0];
        let mut jumps = 0;
        let mut prev: Option<u32> = None;
        for (idx, rec) in &w.got {
            assert_eq!(*rec, sim.log[*idx as usize], "番号 {idx}");
            if let Some(p) = prev {
                assert!(*idx > p, "番号が戻った");
                if *idx != p + 1 {
                    assert!(
                        matches!(rec, Record::Seg { .. }),
                        "飛んだ先（番号 {idx}）が Seg の記録でない"
                    );
                    jumps += 1;
                }
            }
            prev = Some(*idx);
        }
        assert_eq!(prev, Some(sim.host.tail() - 1), "最後の記録まで届いた");
        assert!(
            jumps >= 1,
            "seed {seed:#x}: リングバッファから落ちたのに飛んでいない（検査の狙いが外れた）"
        );
        assert!(sim.host.counts().jumps >= 1);
        assert_eq!(sim.host.spectator_count(), 1, "黙っている間に外された");
    }
}

#[test]
fn a_spectator_over_the_limit_is_refused_and_a_silent_one_is_replaced() {
    let seed = 0x5EED_0005;
    let mut rng = Rng(seed);
    let script = make_script(&mut rng, 3, 200);
    let mut sim = Sim::new(
        Net::new(seed, 3, 25, 10, 5),
        SpectateHost::with_capacity(host_cfg(), 4_096),
        script,
    );
    for n in 1..=MAX_SPECTATORS as u16 {
        sim.add_watcher(n, 0, 0, ASM_CAP);
    }
    sim.add_watcher(OVER, 3_000, 0, ASM_CAP); // 上限の次の 1 人（断られる）
    let over = MAX_SPECTATORS;
    sim.run_for(9_000);
    assert_eq!(sim.host.spectator_count(), MAX_SPECTATORS);
    assert!(sim.host.counts().refused_full > 0);
    assert_eq!(sim.watchers[over].w.host_bye(), Some(ByeReason::Cancelled));
    assert!(
        !sim.watchers[over].w.is_joined(),
        "上限なのに {OVER} 人目が入った"
    );
    // 最後に入った 1 人が黙る。
    sim.net.dead.push(addr(MAX_SPECTATORS as u16));
    sim.run_for(SILENT_MS + 3_000);
    assert_eq!(sim.host.counts().silenced, 1);
    assert!(
        sim.watchers[over].w.is_joined(),
        "枠が空いたのに {OVER} 人目が入れない"
    );
    assert!(sim.watchers[over].join_start.is_some());
    sim.run_until_caught_up(600_000);
    for i in 0..MAX_SPECTATORS - 1 {
        assert_matches_host(&sim, &sim.watchers[i], 0, &format!("観戦者 {}", i + 1));
    }
    let last = &sim.watchers[over];
    assert_matches_host(
        &sim,
        last,
        last.join_start.unwrap(),
        &format!("{OVER} 人目"),
    );
}

/// 決めた上限は 10 人: 10 人目までは入れ、11 人目は BYE(取消) で断られる。
#[test]
fn the_limit_is_ten_spectators_and_the_eleventh_is_refused() {
    assert_eq!(MAX_SPECTATORS, 10);
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    let mut sink = Sink::default();
    for n in 1..=10u16 {
        assert_eq!(
            join(&mut host, addr(n), u64::from(n), 0, &mut sink),
            HostEvent::Joined,
            "{n} 人目"
        );
    }
    assert_eq!(host.spectator_count(), 10);
    sink.take();
    assert_eq!(
        join(&mut host, addr(11), 11, 0, &mut sink),
        HostEvent::Refused
    );
    assert_eq!(
        sink.take(),
        vec![(addr(11), Message::Bye(ByeReason::Cancelled))]
    );
    assert_eq!(host.spectator_count(), 10);
}

/// 上限ちょうどの人数が満杯でも、全員が同じ列を受ける（上限の観戦者それぞれに送りの時計・ウィンドウ・受け取れた範囲が独立している）。
#[test]
fn every_spectator_up_to_the_limit_gets_the_hosts_exact_column_on_a_lossy_line() {
    let seed = 0x5EED_000A;
    let mut rng = Rng(seed);
    let script = make_script(&mut rng, 3, 200);
    let mut sim = Sim::new(
        Net::new(seed, 3, 25, 10, 5),
        SpectateHost::with_capacity(host_cfg(), 4_096),
        script,
    );
    for n in 1..=MAX_SPECTATORS as u16 {
        sim.add_watcher(n, 0, 0, ASM_CAP);
    }
    sim.run_until_caught_up(600_000);
    assert_eq!(sim.host.spectator_count(), MAX_SPECTATORS);
    assert_eq!(sim.host.counts().refused_full, 0);
    for (i, w) in sim.watchers.iter().enumerate() {
        assert_matches_host(&sim, w, 0, &format!("観戦者 {}", i + 1));
    }
}

#[derive(Default)]
struct Sink(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Sink {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

impl Sink {
    fn take(&mut self) -> Vec<(SocketAddr, Message)> {
        std::mem::take(&mut self.0)
            .into_iter()
            .map(|(to, b)| (to, open_any(&b).expect("送ったものが解けない").msg))
            .collect()
    }

    fn streams(&mut self) -> Vec<SpectateStream> {
        self.take()
            .into_iter()
            .filter_map(|(_, m)| match m {
                Message::SpectateStream(s) => Some(s),
                _ => None,
            })
            .collect()
    }
}

fn frame(f: u32) -> Record {
    Record::Frame {
        frame: f,
        p1: f as u16,
        p2: 0x100 | f as u16,
    }
}

/// HMAC の便（JOIN）か、このスレッドで使った観戦者のどれかの鍵（どちらの向きでも）で開ける封じた便を読む（模型の送り口が溜めた
/// ものを外から見る）。
fn open_any(b: &[u8]) -> Result<wire::Packet, wire::DecodeError> {
    let first = wire::decode(&key(), None, b);
    if first != Err(wire::DecodeError::NotKeyed) {
        return first;
    }
    let nonces = NONCES.with(|v| v.borrow().clone());
    for n in nonces {
        let k = wkeys(n);
        for o in [k.opener(), k.swapped().opener()] {
            if let Ok(p) = wire::decode(&key(), Some(&o), b) {
                return Ok(p);
            }
        }
    }
    Err(wire::DecodeError::BadTag)
}

fn seal_with(keys: DirKeys, sender: Sender, msg: &Message) -> Vec<u8> {
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::seal(&keys.sealer(sender), &Seq::new(), SID, msg, &mut buf).expect("封じられる");
    buf[..n].to_vec()
}

fn to_host_as(nonce: u64, msg: &Message) -> Vec<u8> {
    if wire::is_signed(msg) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = wire::encode(&key(), SID, msg, &mut buf);
        return buf[..n].to_vec();
    }
    seal_with(wkeys(nonce), Sender::Spectator, msg)
}

fn to_host(msg: &Message) -> Vec<u8> {
    match msg {
        Message::SpectateJoin(j) => to_host_as(j.nonce, msg),
        Message::SpectateAck { nonce, .. } => to_host_as(*nonce, msg),
        other => panic!("観戦者の nonce が分からない便は to_host_as で作る: {other:?}"),
    }
}

fn join_msg(nonce: u64) -> Message {
    remember(nonce);
    let mut name = [0u8; 32];
    name[0] = b'x';
    Message::SpectateJoin(SpectateJoin {
        name_len: 1,
        nonce,
        name,
        build: higanr_core::update::BUILD_NUMBER,
        public: Ephemeral::from_seed(&wseed(nonce)).public(),
    })
}

fn join(
    host: &mut SpectateHost,
    from: SocketAddr,
    nonce: u64,
    now: u32,
    sink: &mut Sink,
) -> HostEvent {
    host.on_datagram(from, &to_host(&join_msg(nonce)), now, sink)
}

fn ack(host: &mut SpectateHost, from: SocketAddr, nonce: u64, next: u32, now: u32) -> HostEvent {
    ack_sack(host, from, nonce, next, 0, now)
}

fn ack_sack(
    host: &mut SpectateHost,
    from: SocketAddr,
    nonce: u64,
    next: u32,
    sack: u64,
    now: u32,
) -> HostEvent {
    let mut sink = Sink::default();
    host.on_datagram(
        from,
        &to_host(&Message::SpectateAck { nonce, next, sack }),
        now,
        &mut sink,
    )
}

/// 番号 `next` が連続の終わりで、`have` の記録を受け取れている観戦者の ACK の範囲（`wire` の配置どおり: ビット i = `next + 1 + i`）。
fn sack_of_have(next: u32, have: &[u32]) -> u64 {
    have.iter().fold(0u64, |acc, &r| {
        assert!(r > next && r - next - 1 < wire::SACK_BITS, "範囲の外: {r}");
        acc | 1 << (r - next - 1)
    })
}

fn indices(streams: &[SpectateStream]) -> Vec<u32> {
    streams
        .iter()
        .flat_map(|s| (0..u32::from(s.count)).map(move |k| s.first + k))
        .collect()
}

#[test]
fn the_send_window_stops_at_ack_plus_window() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 1_000);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..199 {
        host.push(frame(f)).unwrap();
    }
    let mut sink = Sink::default();
    assert_eq!(
        join(&mut host, addr(1), 7, 100, &mut sink),
        HostEvent::Joined
    );
    host.poll(100, &mut sink);
    let streams = sink.streams();
    let sent: Vec<u32> = streams
        .iter()
        .flat_map(|s| (0..u32::from(s.count)).map(move |k| s.first + k))
        .collect();
    assert_eq!(
        sent,
        (0..SEND_WINDOW).collect::<Vec<_>>(),
        "最初のウィンドウ"
    );
    assert_eq!(
        streams.len(),
        (SEND_WINDOW as usize).div_ceil(MAX_STREAM_RECORDS)
    );
    assert!(streams[0].reset && streams[0].first == 0);
    assert_eq!(
        ack(&mut host, addr(1), 7, 10, 101),
        HostEvent::Acked { progress: true }
    );
    host.poll(101, &mut sink);
    let sent: Vec<u32> = sink
        .streams()
        .iter()
        .flat_map(|s| (0..u32::from(s.count)).map(move |k| s.first + k))
        .collect();
    assert_eq!(sent, (SEND_WINDOW..10 + SEND_WINDOW).collect::<Vec<_>>());
}

#[test]
fn a_late_join_is_served_from_the_current_seg_head_with_reset() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    for r in [
        Record::Seg { seg: 0 },
        frame(0),
        frame(1),
        Record::Seg { seg: 1 },
        frame(0),
    ] {
        host.push(r).unwrap();
    }
    assert_eq!((host.head(), host.seg_start(), host.tail()), (0, 3, 5));
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    host.poll(0, &mut sink);
    let s = sink.streams();
    assert_eq!(s.len(), 1);
    assert_eq!((s[0].first, s[0].reset, s[0].count), (3, true, 2));
    assert_eq!(s[0].records[0], Record::Seg { seg: 1 });
    assert_eq!(s[0].records[1], frame(0));
}

fn menu_rec(n: u16) -> Record {
    Record::Menu {
        p1: n,
        p2: n | 0x100,
    }
}

fn select_rec(seat: Seat, chara: u8) -> Record {
    Record::Select {
        seat,
        sel: Select {
            match_no: 1,
            chara,
            color: 0,
            life: 10,
            charge: 0,
            map: 2,
            difficulty: 1,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: 0x1234,
            option_digest: 0,
        },
    }
}

/// 実際の並び（前置き → `Seg` → マッチの入力ワード）の 19 記録。番号:
/// 0 MENU・1 STEP・2 3 SELECT（seg 0 の前置き）・4 `Seg` 0・5〜7 マッチの入力ワード・8 MENU・9 STEP・10 MENU・11 12 SELECT
/// （seg 1 の前置き）・13 `Seg` 1・14 15 マッチの入力ワード・16 `Seg` 2（再戦。前置きなし）・17 マッチの入力ワード・18 `End`。
fn real_order() -> Vec<Record> {
    let step = |n: u16| {
        Record::Step(Step {
            step: n,
            kind: StepKind::Decide,
            value: 3,
            screen: 8,
        })
    };
    vec![
        menu_rec(0),
        step(0),
        select_rec(Seat::P1, 3),
        select_rec(Seat::P2, 4),
        Record::Seg { seg: 0 },
        frame(0),
        frame(1),
        frame(2),
        menu_rec(1),
        step(1),
        menu_rec(2),
        select_rec(Seat::P1, 5),
        select_rec(Seat::P2, 6),
        Record::Seg { seg: 1 },
        frame(0),
        frame(1),
        Record::Seg { seg: 2 },
        frame(0),
        Record::End(ByeReason::Finished),
    ]
}

fn host_after(cap: usize, recs: &[Record], n: usize) -> SpectateHost {
    let mut host = SpectateHost::with_capacity(host_cfg(), cap);
    for r in recs.iter().take(n) {
        host.push(*r).unwrap();
    }
    host
}

/// `host` に新しい観戦者（addr(9)）が JOIN して最初に受ける記録（番号つき）と、最初の 1 通の `reset`。
fn first_flow(host: &mut SpectateHost) -> (Vec<(u32, Record)>, bool) {
    let mut sink = Sink::default();
    assert_eq!(join(host, addr(9), 9, 0, &mut sink), HostEvent::Joined);
    host.poll(0, &mut sink);
    let streams: Vec<SpectateStream> = sink
        .take()
        .into_iter()
        .filter_map(|(to, m)| match m {
            Message::SpectateStream(s) if to == addr(9) => Some(s),
            _ => None,
        })
        .collect();
    let reset = streams[0].reset;
    let flow = streams
        .iter()
        .flat_map(|s| {
            s.records
                .iter()
                .take(usize::from(s.count))
                .enumerate()
                .map(|(k, r)| (s.first + k as u32, *r))
        })
        .collect();
    (flow, reset)
}

#[test]
fn the_seg_head_is_the_preamble_start_and_a_late_join_gets_it() {
    let recs = real_order();
    let mut host = SpectateHost::with_capacity(host_cfg(), 64);
    let want_head = [
        0, 0, 0, 0, 0, 0, 0,
        0, // 0〜7: 最初の seg（始まりはリングバッファの先頭）
        0, 0, 0, 0,
        0, // 8〜12: 次の seg の前置きを積む間は、まだ seg 0 の始まり
        8, 8, 8, // 13 `Seg` 1 で前置きの先頭（8）へ。14・15 は動かない
        16, 16, 16, // 16 `Seg` 2 は再戦: 前置きが無いので `Seg` 自身
    ];
    assert_eq!(want_head.len(), recs.len());
    for (i, r) in recs.iter().enumerate() {
        host.push(*r).unwrap();
        assert_eq!(
            host.seg_start(),
            want_head[i],
            "{i} 番を積んだ後の seg の始まり"
        );
    }
    let cases: [(usize, u32, Record); 4] = [
        (7, 0, recs[0]),    // seg 0 のマッチの途中 → 0 から（SELECT・押下も）
        (11, 0, recs[0]),   // seg 1 の前置きの途中 → まだ seg 0 の始まり
        (15, 8, recs[8]),   // 前置きのある seg 1 の途中 → 前置きの最初の MENU から
        (18, 16, recs[16]), // 再戦の seg 2 の途中 → `Seg` から
    ];
    for (n, first, rec0) in cases {
        let mut host = host_after(64, &recs, n);
        let (flow, reset) = first_flow(&mut host);
        assert!(
            reset,
            "{n} 個積んだ後の JOIN: 頭への流れに reset が付かない"
        );
        assert_eq!(flow.first().copied(), Some((first, rec0)), "{n} 個積んだ後");
        let want: Vec<(u32, Record)> = recs
            .iter()
            .enumerate()
            .skip(first as usize)
            .take(n - first as usize)
            .map(|(i, r)| (i as u32, *r))
            .collect();
        assert_eq!(flow, want, "{n} 個積んだ後");
    }
    let mut host = host_after(64, &recs, 15);
    let (flow, _) = first_flow(&mut host);
    assert_eq!(
        flow.iter()
            .filter(|(_, r)| matches!(r, Record::Select { .. }))
            .count(),
        2
    );
    assert_eq!(
        flow.iter()
            .filter(|(_, r)| matches!(r, Record::Step(_)))
            .count(),
        1
    );
}

#[test]
fn a_preamble_stays_in_the_ring_when_it_can_and_falls_back_to_the_seg_when_not() {
    // (1) 前置きの前に捨てられる古い seg がある: [Seg0, F, F, F, MENU, SEL, SEL, MENU] で埋まったリングバッファに `Seg` 1 を積むと、
    // 前置き（4〜7）は残り、それより前（seg 0）が落ちる。前置きの先頭 4 がリングバッファの先頭で、`Seg` 1（8）は前置きの続き。
    let mut host = SpectateHost::with_capacity(host_cfg(), 8);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink); // 空のリングバッファから見ている観戦者（ack 0）
    for r in [
        Record::Seg { seg: 0 },
        frame(0),
        frame(1),
        frame(2),
        menu_rec(1),
        select_rec(Seat::P1, 1),
        select_rec(Seat::P2, 2),
        menu_rec(2),
    ] {
        host.push(r).unwrap();
    }
    assert_eq!((host.head(), host.seg_start(), host.tail()), (0, 0, 8));
    assert_eq!(host.push(Record::Seg { seg: 1 }), Ok(8));
    assert_eq!((host.head(), host.seg_start(), host.tail()), (4, 4, 9));
    assert_eq!(
        host.ack_of(addr(1)),
        Some(4),
        "落ちた所の観戦者は前置きの先頭へ飛ぶ"
    );
    assert_eq!(host.counts().jumps, 1);
    let (flow, reset) = first_flow(&mut host);
    assert!(reset);
    assert_eq!(flow.first().map(|f| f.0), Some(4));
    assert_eq!(flow.len(), 5, "前置き 4 つ + Seg 1");
    assert!(matches!(flow[0].1, Record::Menu { .. }));
    assert_eq!(flow[4].1, Record::Seg { seg: 1 });

    // (2) 前置きだけでリングバッファが埋まる（前置きがリングバッファの先頭から始まる）: 空きを作れないので前置きを諦め、`Seg` が先頭。
    let mut host = SpectateHost::with_capacity(host_cfg(), 6);
    for n in 0..6 {
        host.push(menu_rec(n)).unwrap();
    }
    assert_eq!(host.push(Record::Seg { seg: 0 }), Ok(6));
    assert_eq!((host.head(), host.seg_start(), host.tail()), (6, 6, 7));
    let (flow, _) = first_flow(&mut host);
    assert_eq!(flow, vec![(6, Record::Seg { seg: 0 })]);

    // (3) 溢れたリングバッファ（1 つの seg で埋まった）の後の `Seg`: 溢れの間に捨てた前置きは残さず、`Seg` が先頭。
    let mut host = SpectateHost::with_capacity(host_cfg(), 4);
    for r in [Record::Seg { seg: 0 }, frame(0), frame(1), frame(2)] {
        host.push(r).unwrap();
    }
    assert_eq!(host.push(frame(3)), Err(PushError::Full));
    assert_eq!(host.push(menu_rec(1)), Err(PushError::Broken));
    assert_eq!(host.push(Record::Seg { seg: 1 }), Ok(4));
    assert_eq!((host.head(), host.seg_start()), (4, 4));
}

#[test]
fn resends_wait_for_the_rto_double_and_cap_at_250ms() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    for r in [Record::Seg { seg: 0 }, frame(0), frame(1)] {
        host.push(r).unwrap();
    }
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 10, &mut sink);
    let mut times = Vec::new();
    for t in 10..1_000u32 {
        // 観戦者は黙っていない扱い（ack は返さないが、外される前に JOIN を送り直して生きている印を更新する）。
        if t % 3_000 == 0 {
            join(&mut host, addr(1), 1, t, &mut sink);
        }
        host.poll(t, &mut sink);
        if !sink.0.is_empty() {
            times.push(t);
            sink.0.clear();
        }
    }
    // 最初の 10 は新しい分。以後の送り直しは 100, 200, 250, 250 ms 間隔（倍々は 400 になるところで上限に止まる）。
    let mut want = vec![10u32];
    let mut at = 10;
    for gap in [
        RESEND_INIT_MS,
        2 * RESEND_INIT_MS,
        RESEND_MAX_MS,
        RESEND_MAX_MS,
    ] {
        at += gap;
        want.push(at);
    }
    want.retain(|&t| t < 1_000);
    assert_eq!(times, want);
    assert_eq!(RESEND_INIT_MS, 100, "この検査の期待は 100 ms 始まり");
    // 間隔は記録ごとに数える: ack が 1 つ進んでも、残りの記録（もう 4 回送った）の間隔は戻らない（250 ms）。新しく積んで送った記録は
    // 自分の時計（見積もりが無いので `RESEND_INIT_MS`。標本が取れないまま 1 回目の時間切れがあったので倍率 2 =200 ms）で数える。
    let last = *times.last().unwrap();
    assert_eq!(
        ack(&mut host, addr(1), 1, 1, last + 1),
        HostEvent::Acked { progress: true }
    );
    host.push(frame(2)).unwrap();
    host.poll(last + 2, &mut sink);
    assert_eq!(sink.streams().len(), 1, "新しい記録 1 つを送る");
    host.poll(last + 2 + 2 * RESEND_INIT_MS - 1, &mut sink);
    assert!(sink.0.is_empty(), "新しい記録は間隔の手前では送り直さない");
    host.poll(last + 2 + 2 * RESEND_INIT_MS, &mut sink);
    let ss = sink.streams();
    assert_eq!(
        ss.iter().map(|s| (s.first, s.count)).collect::<Vec<_>>(),
        vec![(3, 1)],
        "新しい記録だけ `rto` で送り直す（古い記録 1・2 は間隔 250 ms の手前）"
    );
    host.poll(last + RESEND_MAX_MS - 1, &mut sink);
    assert!(sink.0.is_empty());
    host.poll(last + RESEND_MAX_MS, &mut sink);
    assert_eq!(
        sink.streams()
            .iter()
            .map(|s| (s.first, s.count))
            .collect::<Vec<_>>(),
        vec![(1, 2)],
        "残りの記録 1・2 は、最後に送ってから 250 ms で 1 通に詰めて送り直す"
    );
}

#[test]
fn the_rto_follows_the_ack_round_trip_within_its_bounds() {
    for (d, want) in [
        (5u32, RESEND_MIN_MS),
        (60, 60 + 4 * (60 / 2)),
        (1_000, RESEND_MAX_MS),
    ] {
        let mut host = SpectateHost::with_capacity(host_cfg(), 100);
        for r in [Record::Seg { seg: 0 }, frame(0), frame(1)] {
            host.push(r).unwrap();
        }
        let mut sink = Sink::default();
        join(&mut host, addr(1), 1, 0, &mut sink);
        assert_eq!(
            host.rto_of(addr(1)),
            Some(RESEND_INIT_MS),
            "標本が無い間は初めの値"
        );
        host.poll(0, &mut sink);
        assert_eq!(sink.streams().len(), 1);
        ack(&mut host, addr(1), 1, 3, d);
        assert_eq!(host.rto_of(addr(1)), Some(want), "往復 {d} ms の見積もり");
        if d == 1_000 {
            // 長い待ち（観戦者が消費を待って ack を止めた）の標本は 250 ms で切ってあるので、その後の 5 ms の往復 30 回で
            // 下限まで戻る（切らずに 1,000 を入れると、30 回では 139 ms の間隔が残って下限に届かない。計算は整数の式を手で回した値）。
            let mut t = d + 20;
            for k in 0..30u32 {
                host.push(frame(k)).unwrap();
                host.poll(t, &mut sink);
                assert_eq!(sink.streams().len(), 1, "{k} 回目の送り");
                let tail = host.tail();
                ack(&mut host, addr(1), 1, tail, t + 5);
                t += 20;
            }
            assert_eq!(
                host.rto_of(addr(1)),
                Some(RESEND_MIN_MS),
                "長い待ちの標本が見積もりに残っている"
            );
        }
    }
    // 送り直した通への ack は標本にしない（Karn）: 100 ms 黙って送り直し、その 5 ms 後の ack（送り直しへの返事とも、最初の通への
    // 遅い返事とも読める）で見積もりが動かない。
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    for r in [Record::Seg { seg: 0 }, frame(0), frame(1)] {
        host.push(r).unwrap();
    }
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    for t in 0..=RESEND_INIT_MS {
        host.poll(t, &mut sink);
    }
    assert_eq!(host.counts().resends, 1);
    ack(&mut host, addr(1), 1, 3, RESEND_INIT_MS + 5);
    assert_eq!(
        host.rto_of(addr(1)),
        Some(RESEND_INIT_MS),
        "送り直した後の ack を標本にした"
    );
}

#[test]
fn a_lost_packet_is_resent_while_new_records_keep_flowing() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 1_000);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    let mut have = std::collections::BTreeSet::new();
    let mut acked = 0u32;
    let mut lost_at = None;
    let mut got_20_at = None;
    let mut pushed = 0u32;
    for t in 1..4_000u32 {
        if t % 16 == 0 && pushed < 200 {
            let rec = if pushed == 0 {
                Record::Seg { seg: 0 }
            } else {
                frame(pushed)
            };
            host.push(rec).unwrap();
            pushed += 1;
        }
        host.poll(t, &mut sink);
        for st in sink.streams() {
            let hit = (st.first..st.first + u32::from(st.count)).contains(&20);
            if hit && lost_at.is_none() {
                lost_at = Some(t);
                continue;
            }
            for k in 0..u32::from(st.count) {
                have.insert(st.first + k);
                if st.first + k == 20 && got_20_at.is_none() {
                    got_20_at = Some(t);
                }
            }
        }
        if t % 16 == 8 {
            while have.contains(&acked) {
                acked += 1;
            }
            ack(&mut host, addr(1), 1, acked, t);
        }
    }
    let lost = lost_at.expect("記録 20 の通を落とせていない");
    let got = got_20_at.expect("落とした記録 20 が届かないまま（送り直しが止まった）");
    eprintln!(
        "落ちた通 → 送り直しが届くまで {} ms（60 fps の 1 記録ずつ・n = 1 通）",
        got - lost
    );
    assert!(
        got - lost <= 120,
        "落ちた通が {} ms 後に届いた（ウィンドウが埋まるまで送り直さない）",
        got - lost
    );
    assert!(
        host.counts().resends >= 1 && host.counts().resends <= 3,
        "送り直し {}",
        host.counts().resends
    );
    assert_eq!(acked, 200, "全部受け取った");
}

#[test]
fn frame_paced_acks_cause_almost_no_resends() {
    let mut worst = 0.0f64;
    let mut n_runs = 0;
    for base in [1u32, 5, 25, 60] {
        for phase in [0u32, 4, 8, 12] {
            let (sent, resends, dup) = frame_paced_run(base, phase);
            assert!(sent >= 4_200, "流した数が少ない（実行が短い）: {sent}");
            let ratio = resends as f64 / sent as f64;
            worst = worst.max(ratio);
            eprintln!(
                "片道 {base} ms・位相 {phase} ms: 送った {sent} 通・送り直し {resends}・観戦者が捨てた重複 {dup}"
            );
            assert!(
                ratio < 0.01,
                "片道 {base} ms・位相 {phase} ms: 送った {sent} 通のうち送り直し {resends}"
            );
            n_runs += 1;
        }
    }
    assert_eq!(n_runs, 16);
    eprintln!("最悪の 送り直し / 送った通数 = {:.4}（16 通り）", worst);
}

#[test]
fn frame_paced_spectator_still_gets_every_record_with_loss() {
    for drop_pct in [10u64, 20] {
        let (sent, resends, _) = frame_paced_run_with(5, 20, 8, drop_pct);
        assert!(resends > 0, "落とせていないか送り直しが無い: {sent}");
        eprintln!("落とす {drop_pct}%: 送った {sent} 通・送り直し {resends}");
    }
}

#[test]
fn heartbeats_keep_a_quiet_spectator_and_silence_removes_one() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    join(&mut host, addr(2), 2, 0, &mut sink);
    let mut beats = Vec::new();
    for t in 0..30_000u32 {
        host.poll(t, &mut sink);
        for s in sink.take() {
            if let (a, Message::SpectateStream(st)) = s {
                assert_eq!(st.count, 0, "積んだ記録が無いのに記録が出た");
                if a == addr(1) {
                    beats.push(t);
                }
                if a == addr(1) {
                    ack(&mut host, addr(1), 1, 0, t);
                }
            }
        }
    }
    assert_eq!(&beats[..3], &[0, HEARTBEAT_MS, 2 * HEARTBEAT_MS]);
    assert_eq!(host.spectator_count(), 1, "黙った観戦者だけ外れた");
    assert_eq!(host.counts().silenced, 1);
    assert_eq!(host.ack_of(addr(2)), None);
    assert_eq!(host.ack_of(addr(1)), Some(0));
}

#[test]
fn a_silent_spectator_is_not_hammered_with_one_record_packets() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 8_192);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    let (mut packets, mut records) = (0u64, 0u64);
    let mut f = 0;
    for t in 0..7_000u32 {
        if t % 16 == 0 {
            host.push(frame(f)).unwrap();
            f += 1;
        }
        host.poll(t, &mut sink);
        for s in sink.streams() {
            packets += 1;
            records += u64::from(s.count);
        }
    }
    let c = host.counts();
    eprintln!(
        "黙った観戦者へ 7 秒: 送った {packets} 通・{records} 記録（送り直し {} 通・{} 記録）",
        c.resends, c.records_resent
    );
    assert_eq!(host.spectator_count(), 1, "7 秒ではまだ外れない");
    assert!(c.resends > 0, "黙った観戦者へ送り直しが無い（効く期間）");
    assert!(packets < 150, "黙った観戦者へ 7 秒で {packets} 通");
    assert!(c.resends < 60, "うち送り直し {} 通", c.resends);
}

#[test]
fn silence_boundary_is_exactly_silent_ms() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 500, &mut sink);
    host.poll(500 + SILENT_MS - 1, &mut sink);
    assert_eq!(host.spectator_count(), 1);
    host.poll(500 + SILENT_MS, &mut sink);
    assert_eq!(host.spectator_count(), 0);
}

#[test]
fn spectator_limit_refusal_and_rejoin() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    // いまの seg の始まり（3）がリングバッファの先頭（0）と違うリングバッファ（起こし直しの行き先が始まりと混ざらないように）。
    for r in [
        Record::Seg { seg: 0 },
        frame(0),
        frame(1),
        Record::Seg { seg: 1 },
        frame(0),
        frame(1),
    ] {
        host.push(r).unwrap();
    }
    assert_eq!((host.head(), host.seg_start()), (0, 3));
    let mut sink = Sink::default();
    for n in 1..=MAX_SPECTATORS as u16 {
        assert_eq!(
            join(&mut host, addr(n), u64::from(n), 0, &mut sink),
            HostEvent::Joined
        );
    }
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    sink.take();
    assert_eq!(
        join(&mut host, addr(OVER), u64::from(OVER), 0, &mut sink),
        HostEvent::Refused
    );
    assert_eq!(
        sink.take(),
        vec![(addr(OVER), Message::Bye(ByeReason::Cancelled))]
    );
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    assert_eq!(
        join(&mut host, addr(1), 1, 5, &mut sink),
        HostEvent::Refreshed
    );
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    ack(&mut host, addr(2), 2, 4, 6);
    assert_eq!(host.ack_of(addr(2)), Some(4));
    assert_eq!(
        join(&mut host, addr(2), 222, 7, &mut sink),
        HostEvent::Joined
    );
    assert_eq!(host.ack_of(addr(2)), Some(host.seg_start()));
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    // 起こし直す前の nonce の ack は聞かない。バージョン 5 では、起こし直す前の観戦者の鍵（JOIN し直した公開鍵とは別）で封じてあるので、
    // 開く前にタグで落ちる。
    assert_eq!(
        ack(&mut host, addr(2), 2, 5, 8),
        HostEvent::Dropped(DropReason::BadTag)
    );
    // 起こし直した後の鍵で封じても、中の nonce が前のものなら聞かない（nonce のガードは鍵のガードと別に残る）。
    assert_eq!(
        host.on_datagram(
            addr(2),
            &to_host_as(
                222,
                &Message::SpectateAck {
                    nonce: 2,
                    next: 5,
                    sack: 0
                }
            ),
            8,
            &mut Sink::default()
        ),
        HostEvent::Dropped(DropReason::UnknownSpectator)
    );
    let bye = to_host_as(3, &Message::Bye(ByeReason::Finished));
    assert_eq!(
        host.on_datagram(addr(3), &bye, 9, &mut sink),
        HostEvent::Left
    );
    assert_eq!(
        join(&mut host, addr(OVER), u64::from(OVER), 10, &mut sink),
        HostEvent::Joined
    );
}

#[test]
fn ring_limit_trims_old_segs_and_breaks_on_a_single_seg() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 8);
    for r in [
        Record::Seg { seg: 0 },
        frame(0),
        frame(1),
        Record::Seg { seg: 1 },
        frame(0),
        frame(1),
        frame(2),
        frame(3),
    ] {
        host.push(r).unwrap();
    }
    assert_eq!((host.head(), host.tail()), (0, 8));
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink); // ack = 3（いまの seg の始まり）
    assert_eq!(host.ack_of(addr(1)), Some(3));
    assert_eq!(host.push(frame(4)), Ok(8));
    assert_eq!((host.head(), host.seg_start()), (3, 3), "seg 0 が落ちた");
    for f in 5..7 {
        assert!(host.push(frame(f)).is_ok());
    }
    assert_eq!(host.tail() - host.head(), 8);
    assert_eq!(host.push(frame(8)), Err(PushError::Full));
    assert!(host.is_broken());
    assert_eq!(host.spectator_count(), 0, "溢れたら観戦者を全員外す");
    assert_eq!(host.push(frame(9)), Err(PushError::Broken));
    assert_eq!(
        join(&mut host, addr(2), 2, 1, &mut sink),
        HostEvent::Dropped(DropReason::Broken)
    );
    assert_eq!(host.push(Record::Seg { seg: 2 }), Ok(11));
    assert!(!host.is_broken());
    assert_eq!((host.head(), host.seg_start(), host.tail()), (11, 11, 12));
    assert_eq!(join(&mut host, addr(2), 2, 2, &mut sink), HostEvent::Joined);
    let mut host = SpectateHost::with_capacity(host_cfg(), 4);
    for r in [Record::Seg { seg: 0 }, frame(0), frame(1), frame(2)] {
        host.push(r).unwrap();
    }
    assert_eq!(host.push(Record::Seg { seg: 1 }), Ok(4));
    assert_eq!((host.head(), host.seg_start()), (4, 4));
}

#[test]
fn a_spectator_left_behind_is_moved_to_the_seg_head_with_reset() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 8);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink); // 空のリングバッファに入る（ack 0）
    for r in [
        Record::Seg { seg: 0 },
        frame(0),
        frame(1),
        Record::Seg { seg: 1 },
        frame(0),
        frame(1),
        frame(2),
        frame(3),
    ] {
        host.push(r).unwrap();
    }
    host.poll(0, &mut sink);
    sink.take();
    assert_eq!(host.ack_of(addr(1)), Some(0));
    host.push(frame(4)).unwrap();
    assert_eq!(host.ack_of(addr(1)), Some(3));
    assert_eq!(host.counts().jumps, 1);
    host.poll(20, &mut sink);
    let s = sink.streams();
    assert_eq!((s[0].first, s[0].reset), (3, true));
    assert_eq!(s[0].records[0], Record::Seg { seg: 1 });
}

#[test]
fn a_spectator_of_another_version_is_refused() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 10);
    let mut sink = Sink::default();
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(&key(), SID, &join_msg(1), &mut buf);
    let now_ver = buf[..n].to_vec();
    // バージョンを書き換えてタグを付け直す（同じ秘密を知る、別のバージョンの観戦者が送った形）。
    for v in [0u8, wire::PROTO_VER - 1, wire::PROTO_VER + 1] {
        let mut old = now_ver[..n - wire::TAG_LEN].to_vec();
        old[4] = v;
        let tag = key().tag(&old);
        old.extend_from_slice(&tag);
        assert_eq!(
            host.on_datagram(addr(1), &old, 0, &mut sink),
            HostEvent::Dropped(DropReason::OtherVersion(v)),
            "バージョン {v}"
        );
    }
    assert_eq!(host.spectator_count(), 0);
    let c = host.counts();
    assert_eq!((c.other_version, c.undecodable, c.joined), (3, 0, 0));
    assert!(sink.0.is_empty(), "バージョン違いに返事をした");
    assert_eq!(
        host.on_datagram(addr(1), &now_ver, 0, &mut sink),
        HostEvent::Joined
    );
}

#[test]
fn a_spectator_of_another_build_is_refused_and_counted() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 10);
    let mut sink = Sink::default();
    let mut w = watcher(1, &[HOST]);
    w.set_build(403_920);
    w.poll(0, &mut sink);
    let sent = std::mem::take(&mut sink.0);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        host.on_datagram(addr(1), &sent[0].1, 0, &mut sink),
        HostEvent::Dropped(DropReason::OtherBuild(403_920))
    );
    assert_eq!(host.spectator_count(), 0);
    let c = host.counts();
    assert_eq!((c.other_build, c.joined, c.undecodable), (1, 0, 0));
    assert!(sink.0.is_empty(), "ビルド違いに返事をした");
    let mut host2 = SpectateHost::with_capacity(host_cfg(), 10);
    host2.set_build(403_920);
    assert_eq!(
        host2.on_datagram(addr(1), &to_host(&join_msg(1)), 0, &mut sink),
        HostEvent::Dropped(DropReason::OtherBuild(higanr_core::update::BUILD_NUMBER))
    );
    let mut w = watcher(1, &[HOST]);
    w.poll(0, &mut sink);
    let sent = std::mem::take(&mut sink.0);
    assert_eq!(
        host.on_datagram(addr(1), &sent[0].1, 1, &mut sink),
        HostEvent::Joined
    );
}

#[test]
fn streams_are_sealed_per_spectator_and_the_host_never_reuses_a_nonce() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    for r in [Record::Seg { seg: 0 }, frame(0), frame(1)] {
        host.push(r).unwrap();
    }
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    join(&mut host, addr(2), 2, 0, &mut sink);
    host.poll(1, &mut sink);
    let sent = std::mem::take(&mut sink.0);
    assert!(sent.len() >= 2, "{}", sent.len());
    let mut fields = std::collections::HashSet::new();
    for (to, b) in &sent {
        let (mine, other) = if *to == addr(1) { (1, 2) } else { (2, 1) };
        // 宛先の観戦者の鍵（ホストの送る向き = 観戦者の受ける向き）で開く。別の観戦者の鍵・観戦者の送る向きでは開かない。
        assert!(wire::decode(&key(), Some(&wkeys(mine).opener()), b).is_ok());
        assert_eq!(
            wire::decode(&key(), Some(&wkeys(other).opener()), b),
            Err(wire::DecodeError::BadTag)
        );
        assert_eq!(
            wire::decode(&key(), Some(&wkeys(mine).swapped().opener()), b),
            Err(wire::DecodeError::BadTag)
        );
        assert_eq!(b[wire::HEADER_LEN], Sender::SpectateHost as u8);
        assert!(fields.insert(b[wire::HEADER_LEN..wire::HEADER_LEN + 8].to_vec()));
    }
    // 観戦者 2 の鍵で封じた ACK を観戦者 1 の送り元から送っても、観戦者 1 の鍵では開かない（タグ違い）。
    assert_eq!(
        host.on_datagram(
            addr(1),
            &to_host_as(
                2,
                &Message::SpectateAck {
                    nonce: 1,
                    next: 1,
                    sack: 0
                }
            ),
            2,
            &mut sink
        ),
        HostEvent::Dropped(DropReason::BadTag)
    );
    // 観戦者 1 が黙って外され（`SILENT_MS`）、同じ鍵（同じ nonce・同じ公開鍵）で入り直しても、ホストの番号は前の続き（重ならない）。
    host.poll(SILENT_MS + 10, &mut sink);
    assert_eq!(host.spectator_count(), 0);
    sink.0.clear();
    let before = host.next_seq();
    join(&mut host, addr(1), 1, SILENT_MS + 11, &mut sink);
    host.poll(SILENT_MS + 12, &mut sink);
    let again = std::mem::take(&mut sink.0);
    assert!(!again.is_empty());
    for (_, b) in &again {
        assert!(
            fields.insert(b[wire::HEADER_LEN..wire::HEADER_LEN + 8].to_vec()),
            "入り直した観戦者への便で nonce が重なった"
        );
    }
    assert!(host.next_seq() > before);
    // 同じ送り元・同じ nonce でも公開鍵が違う JOIN は、起こし直しとして入り直させる（古い鍵で送り続けない）。
    let Message::SpectateJoin(mut j) = join_msg(1) else {
        unreachable!("JOIN");
    };
    j.public = Ephemeral::from_seed(&wseed(77)).public();
    assert_eq!(
        host.on_datagram(
            addr(1),
            &to_host(&Message::SpectateJoin(j)),
            SILENT_MS + 13,
            &mut sink
        ),
        HostEvent::Joined
    );
    let new_keys = Ephemeral::from_seed(&wseed(77))
        .dir_keys(&host_cfg().public(), false, SID, &key())
        .unwrap();
    assert_eq!(host.dir_keys_of(addr(1)), Some(new_keys.swapped()));
    for n in 2..=MAX_SPECTATORS as u16 {
        join(&mut host, addr(n), u64::from(n), SILENT_MS + 14, &mut sink);
    }
    sink.0.clear();
    assert_eq!(
        join(
            &mut host,
            addr(OVER),
            u64::from(OVER),
            SILENT_MS + 15,
            &mut sink
        ),
        HostEvent::Refused
    );
    let bye = std::mem::take(&mut sink.0);
    assert_eq!(bye.len(), 1);
    assert_eq!(
        wire::decode(&key(), Some(&wkeys(u64::from(OVER)).opener()), &bye[0].1).map(|p| p.msg),
        Ok(Message::Bye(ByeReason::Cancelled))
    );
    assert_eq!(
        wire::decode(&key(), None, &bye[0].1),
        Err(wire::DecodeError::NotKeyed)
    );
}

#[test]
fn a_resent_join_and_a_full_refusal_do_not_repeat_the_key_exchange() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    let mut sink = Sink::default();
    assert_eq!(join(&mut host, addr(1), 1, 0, &mut sink), HostEvent::Joined);
    assert_eq!(host.counts().key_agreements, 1);
    for t in 1..=40 {
        assert_eq!(
            join(&mut host, addr(1), 1, t * JOIN_RESEND_MS, &mut sink),
            HostEvent::Refreshed
        );
    }
    assert_eq!(host.counts().key_agreements, 1, "再送のたびに計算した");
    for n in 2..=MAX_SPECTATORS as u16 {
        join(&mut host, addr(n), u64::from(n), 10_000, &mut sink);
    }
    assert_eq!(host.counts().key_agreements, MAX_SPECTATORS as u64);
    let Message::SpectateJoin(mut j) = join_msg(1) else {
        unreachable!("JOIN");
    };
    j.public = Ephemeral::from_seed(&wseed(77)).public();
    assert_eq!(
        host.on_datagram(
            addr(1),
            &to_host_as(1, &Message::SpectateJoin(j)),
            10_001,
            &mut sink
        ),
        HostEvent::Joined
    );
    assert_eq!(host.counts().key_agreements, MAX_SPECTATORS as u64 + 1);
    // 満杯: 最初の 1 人には BYE（計算 1 回）、1 秒の間は送り元を変えて 100 人来ても計算せず BYE も出さない。
    let base = host.counts().key_agreements;
    sink.0.clear();
    assert_eq!(
        join(&mut host, addr(200), 200, 20_000, &mut sink),
        HostEvent::Refused
    );
    assert_eq!(sink.0.len(), 1);
    assert_eq!(host.counts().key_agreements, base + 1);
    sink.0.clear();
    for i in 0..100u16 {
        let n = 300 + i;
        let t = 20_001 + u32::from(i) * (REFUSE_GAP_MS - 2) / 100;
        assert_eq!(
            join(&mut host, addr(n), u64::from(n), t, &mut sink),
            HostEvent::Refused
        );
    }
    assert!(sink.0.is_empty(), "間引く間に BYE を出した");
    assert_eq!(host.counts().key_agreements, base + 1, "満杯で計算した");
    assert_eq!(host.counts().refused_quiet, 100);
    assert_eq!(
        join(&mut host, addr(200), 200, 20_000 + REFUSE_GAP_MS, &mut sink),
        HostEvent::Refused
    );
    assert_eq!(sink.0.len(), 1, "間引いたまま二度と答えない");
    assert_eq!(host.counts().key_agreements, base + 2);
    sink.0.clear();
    host.poll(20_000 + REFUSE_GAP_MS + SILENT_MS + 1, &mut sink);
    assert_eq!(host.spectator_count(), 0);
    assert_eq!(
        join(&mut host, addr(200), 200, 31_100, &mut sink),
        HostEvent::Joined
    );
    assert_eq!(host.counts().key_agreements, base + 3);
}

#[test]
fn a_watcher_refuses_a_stream_of_the_previous_version_before_joining() {
    let mut w = watcher(1, &[HOST]);
    let mut sink = Sink::default();
    let now_ver = from_host(&stream_msg(0, true, 2));
    // バージョン 4 のホストが送った形を、平文の本文から組む: ヘッダ 12 B ‖ 本文（first 4 B ‖ reset 1 B ‖ 個数 1 B ‖ Frame の記録 9 B × 2）‖
    // HMAC のタグ 16 B。type 14・記録の種類 5 は `wire.rs` の `TYPE_SPECTATE_STREAM`・`REC_FRAME`（非公開なので値で書く）。
    let mut old = vec![0x48, 0x47, 0x52, 0x54, wire::PROTO_VER - 1, 14, 0, 0];
    old.extend_from_slice(&SID.to_le_bytes());
    old.extend_from_slice(&0u32.to_le_bytes());
    old.extend_from_slice(&[1, 2]);
    for f in 0u32..2 {
        old.push(5);
        old.extend_from_slice(&f.to_le_bytes());
        old.extend_from_slice(&(f as u16).to_le_bytes());
        old.extend_from_slice(&(0x100 | f as u16).to_le_bytes());
    }
    assert_eq!(
        old.len() - wire::HEADER_LEN,
        now_ver.len() - wire::HEADER_LEN - 8 - wire::TAG_LEN,
        "手で組んだバージョン 4 の本文が今のバージョンの本文と長さが違う（検査の標本が実物の形でない）"
    );
    let tag = key().tag(&old);
    old.extend_from_slice(&tag);
    w.on_datagram(HOST, &old, 0, &mut sink);
    let c = w.counts();
    assert_eq!((c.other_version, c.undecodable), (1, 0), "{c:?}");
    assert!(!w.is_joined(), "前のバージョンの流れで繋がった");
    assert_eq!(w.pending(), 0);
    assert!(sink.0.is_empty(), "バージョン違いに ACK を返した");
    w.on_datagram(HOST, &now_ver, 5, &mut sink);
    assert!(w.is_joined());
    assert_eq!(w.pending(), 2);
}

#[test]
fn the_join_gate_accepts_only_the_spectate_code_key_and_session() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 10);
    let mut sink = Sink::default();
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut enc = |k: &AuthKey, sid: u32, m: &Message| {
        let n = wire::encode(k, sid, m, &mut buf);
        buf[..n].to_vec()
    };
    let j = join_msg(1);
    let versus = AuthKey::derive(&Secret([0x11; 16]), SID);
    let bad_key = enc(&versus, SID, &j);
    assert_eq!(
        host.on_datagram(addr(1), &bad_key, 0, &mut sink),
        HostEvent::Dropped(DropReason::BadTag)
    );
    let other_sid = AuthKey::derive(&secret(), SID + 1);
    let bad_sid = enc(&other_sid, SID + 1, &j);
    assert_eq!(
        host.on_datagram(addr(1), &bad_sid, 0, &mut sink),
        HostEvent::Dropped(DropReason::WrongSession)
    );
    let mut flipped = enc(&key(), SID, &j);
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert_eq!(
        host.on_datagram(addr(1), &flipped, 0, &mut sink),
        HostEvent::Dropped(DropReason::BadTag)
    );
    for junk in [
        &b""[..],
        b"not a packet at all, really not.....",
        &[0u8; 64],
    ] {
        assert_eq!(
            host.on_datagram(addr(1), junk, 0, &mut sink),
            HostEvent::Dropped(DropReason::NotHgrt)
        );
    }
    assert_eq!(
        host.on_datagram(addr(1), b"HGRT", 0, &mut sink),
        HostEvent::Dropped(DropReason::Undecodable)
    );
    // JOIN していない送り元の封じた便（ACK・ホストが受けない type の STREAM・PING）: 開く鍵が無いので「知らない観戦者」（バージョン 5）。
    let stream = Message::SpectateStream(SpectateStream {
        first: 0,
        reset: false,
        count: 0,
        records: [Record::FILLER; MAX_STREAM_RECORDS],
    });
    for m in [
        Message::SpectateAck {
            nonce: 1,
            next: 0,
            sack: 0,
        },
        stream,
        Message::Ping { send_time: 1 },
    ] {
        assert_eq!(
            host.on_datagram(addr(1), &to_host_as(1, &m), 0, &mut sink),
            HostEvent::Dropped(DropReason::UnknownSpectator),
            "{m:?}"
        );
    }
    assert_eq!(
        host.spectator_count(),
        0,
        "ガードを通らないものは 1 人も入れない"
    );
    assert!(sink.0.is_empty(), "ガードで落としたものに返事をしない");
    assert_eq!(
        host.on_datagram(addr(1), &enc(&key(), SID, &j), 0, &mut sink),
        HostEvent::Joined
    );
    for m in [stream, Message::Ping { send_time: 1 }] {
        assert_eq!(
            host.on_datagram(addr(1), &to_host_as(1, &m), 0, &mut sink),
            HostEvent::Dropped(DropReason::Unexpected)
        );
    }
    let c = host.counts();
    assert_eq!(
        (
            c.bad_tag,
            c.wrong_session,
            c.unexpected,
            c.unknown_spectator
        ),
        (2, 1, 2, 3)
    );
    assert_eq!((c.not_hgrt, c.undecodable), (3, 1));
}

#[test]
fn acks_beyond_the_tail_are_clamped_and_backwards_acks_are_ignored() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    for r in [Record::Seg { seg: 0 }, frame(0), frame(1)] {
        host.push(r).unwrap();
    }
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    assert_eq!(
        ack(&mut host, addr(1), 1, 99, 1),
        HostEvent::Acked { progress: true }
    );
    assert_eq!(host.ack_of(addr(1)), Some(3), "積んだ所（3）へ詰める");
    assert_eq!(host.counts().acks_clamped, 1);
    assert_eq!(
        ack(&mut host, addr(1), 1, 1, 2),
        HostEvent::Acked { progress: false }
    );
    assert_eq!(host.ack_of(addr(1)), Some(3), "戻さない");
}

/// 40 記録（番号 0..40）を積んで観戦者 1（nonce 1）を JOIN させ、40 記録を 1 周回で全部送らせたホスト。送った STREAM は捨てる。
fn host_with_40_sent() -> (SpectateHost, Sink) {
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..39 {
        host.push(frame(f)).unwrap();
    }
    let mut sink = Sink::default();
    assert_eq!(join(&mut host, addr(1), 1, 0, &mut sink), HostEvent::Joined);
    host.poll(0, &mut sink);
    assert_eq!(indices(&sink.streams()), (0..40).collect::<Vec<_>>());
    (host, sink)
}

#[test]
fn a_resend_carries_only_the_records_the_spectator_lacks() {
    let (mut host, mut sink) = host_with_40_sent();
    let have: Vec<u32> = (6..=11).chain(14..=20).collect();
    assert_eq!(
        ack_sack(&mut host, addr(1), 1, 5, sack_of_have(5, &have), 1),
        HostEvent::Acked { progress: true }
    );
    assert_eq!(host.sack_of(addr(1)), Some(sack_of_have(5, &have)));
    let lacking: Vec<u32> = std::iter::once(5).chain(12..=13).chain(21..40).collect();
    for t in 2..RESEND_MIN_MS {
        host.poll(t, &mut sink);
    }
    assert!(sink.0.is_empty(), "時間切れの手前で送り直した");
    host.poll(RESEND_MIN_MS, &mut sink);
    let first_round = sink.streams();
    assert_eq!(
        first_round
            .iter()
            .map(|s| (s.first, s.count))
            .collect::<Vec<_>>(),
        vec![(5, 1), (12, 2), (21, 16), (37, 3)]
    );
    assert_eq!(indices(&first_round), lacking);
    let c = host.counts();
    assert_eq!((c.resends, c.records_resent), (4, lacking.len() as u64));
    let mut later = Vec::new();
    for t in RESEND_MIN_MS + 1..1_500 {
        host.poll(t, &mut sink);
        later.extend(indices(&sink.streams()));
    }
    assert!(
        later.len() > lacking.len(),
        "時間切れが続くはず: {}",
        later.len()
    );
    for r in &later {
        assert!(lacking.contains(r), "届き済みの記録 {r} を送り直した");
    }
    assert!(have.iter().all(|r| !later.contains(r)));
}

#[test]
fn the_sack_accumulates_shifts_with_ack_and_ignores_stale_acks() {
    let (mut host, _) = host_with_40_sent();
    let a = addr(1);
    ack_sack(&mut host, a, 1, 5, sack_of_have(5, &[6, 7]), 1);
    assert_eq!(host.sack_of(a), Some(sack_of_have(5, &[6, 7])));
    ack_sack(&mut host, a, 1, 5, sack_of_have(5, &[6]), 2);
    assert_eq!(host.sack_of(a), Some(sack_of_have(5, &[6, 7])));
    ack_sack(&mut host, a, 1, 5, sack_of_have(5, &[10]), 3);
    assert_eq!(host.sack_of(a), Some(sack_of_have(5, &[6, 7, 10])));
    assert_eq!(
        ack_sack(&mut host, a, 1, 3, u64::MAX >> 1, 4),
        HostEvent::Acked { progress: false }
    );
    assert_eq!(host.ack_of(a), Some(5));
    assert_eq!(host.sack_of(a), Some(sack_of_have(5, &[6, 7, 10])));
    ack_sack(&mut host, a, 1, 8, sack_of_have(8, &[12]), 5);
    assert_eq!(host.ack_of(a), Some(8));
    assert_eq!(host.sack_of(a), Some(sack_of_have(8, &[10, 12])));
    ack_sack(&mut host, a, 1, 39, 0, 6);
    assert_eq!(host.sack_of(a), Some(0));
    let mut host = SpectateHost::with_capacity(host_cfg(), 200);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..129 {
        host.push(frame(f)).unwrap();
    }
    let mut sink = Sink::default();
    join(&mut host, a, 1, 0, &mut sink);
    host.poll(0, &mut sink);
    ack_sack(&mut host, a, 1, 2, sack_of_have(2, &[5, 60]), 1);
    assert_eq!(host.sack_of(a), Some(sack_of_have(2, &[5, 60])));
    ack_sack(&mut host, a, 1, 2 + SEND_WINDOW, 0, 2);
    assert_eq!(host.ack_of(a), Some(2 + SEND_WINDOW));
    assert_eq!(host.sack_of(a), Some(0));
}

#[test]
fn a_sack_naming_records_not_sent_yet_is_clamped() {
    let (mut host, mut sink) = host_with_40_sent();
    let a = addr(1);
    let lie = sack_of_have(5, &[10, 45]);
    assert_eq!(host.counts().sacks_clamped, 0);
    ack_sack(&mut host, a, 1, 5, lie, 1);
    assert_eq!(host.sack_of(a), Some(sack_of_have(5, &[10])));
    assert_eq!(host.counts().sacks_clamped, 1);
    // 嘘のビットの記録（45）を後で送るとき、届き済みとは扱わない: ウィンドウが 5 + 64 = 69 まで開き、新しい分として 40.. が出る。45 を落とす
    // ネットワークを想定して、ack が進まないまま時間切れまで待つと、45 も穴として送り直される。
    for r in 39..59 {
        host.push(frame(r)).unwrap();
    }
    host.poll(2, &mut sink);
    assert_eq!(indices(&sink.streams()), (40..60).collect::<Vec<_>>());
    for t in 3..300 {
        host.poll(t, &mut sink);
    }
    let resent = indices(&sink.streams());
    assert!(resent.contains(&45), "45 を送り直さなかった: {resent:?}");
    assert!(!resent.contains(&10), "範囲の中の 10 を送り直した");
    let (mut host, _) = host_with_40_sent();
    ack_sack(&mut host, a, 1, 999, u64::MAX >> 1, 1);
    assert_eq!(host.ack_of(a), Some(40));
    assert_eq!(host.sack_of(a), Some(0));
    assert_eq!(host.counts().acks_clamped, 1);
    assert_eq!(host.counts().sacks_clamped, 0, "範囲は読んでいない");
}

fn stream_of(first: u32, reset: bool, n: u32) -> SpectateStream {
    let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
    for (k, r) in records.iter_mut().take(n as usize).enumerate() {
        *r = frame(first + k as u32);
    }
    SpectateStream {
        first,
        reset,
        count: n as u8,
        records,
    }
}

fn drain(a: &mut Assembler) -> Vec<(u32, Record)> {
    std::iter::from_fn(|| a.pop_indexed()).collect()
}

#[test]
fn the_assembler_orders_dedups_and_acks_only_the_contiguous_run() {
    let mut a = Assembler::with_capacity(64);
    let r = a.on_stream(&stream_of(5, false, 5));
    assert_eq!((r.stored, r.dup, r.jumped), (5, 0, false));
    assert_eq!((a.ack(), a.pending()), (0, 0));
    assert_eq!(a.pop(), None);
    let r = a.on_stream(&stream_of(0, false, 5));
    assert_eq!(r.stored, 5);
    assert_eq!(a.ack(), 10);
    let got = drain(&mut a);
    assert_eq!(
        got.iter().map(|g| g.0).collect::<Vec<_>>(),
        (0..10).collect::<Vec<_>>()
    );
    assert!(got.iter().all(|(i, r)| *r == frame(*i)));
    let r = a.on_stream(&stream_of(8, false, 5));
    assert_eq!((r.stored, r.dup), (3, 2));
    assert_eq!(a.ack(), 13);
    let r = a.on_stream(&stream_of(8, false, 5));
    assert_eq!((r.stored, r.dup), (0, 5));
    assert_eq!(a.ack(), 13);
    a.on_stream(&stream_of(20, false, 3));
    assert_eq!(a.ack(), 13);
    a.on_stream(&stream_of(13, false, 7));
    assert_eq!(a.ack(), 23, "抜けが埋まったら、先に着いていた分まで連続");
    let got = drain(&mut a);
    assert_eq!(
        got.iter().map(|g| g.0).collect::<Vec<_>>(),
        (10..23).collect::<Vec<_>>()
    );
}

#[test]
fn reset_jumps_only_when_it_is_ahead_of_the_contiguous_run() {
    let mut a = Assembler::with_capacity(64);
    a.on_stream(&stream_of(0, false, 5));
    assert_eq!(a.pop_indexed().map(|x| x.0), Some(0));
    let r = a.on_stream(&stream_of(100, false, 3));
    assert!(!r.jumped);
    assert_eq!(a.ack(), 5);
    let r = a.on_stream(&stream_of(2, true, 3));
    assert!(!r.jumped);
    assert_eq!(a.ack(), 5);
    let r = a.on_stream(&stream_of(5, true, 2));
    assert!(!r.jumped);
    assert_eq!(a.ack(), 7);
    let r = a.on_stream(&stream_of(200, true, 2));
    assert!(r.jumped);
    assert_eq!(a.ack(), 202);
    assert_eq!(
        drain(&mut a).iter().map(|g| g.0).collect::<Vec<_>>(),
        vec![200, 201]
    );
    assert_eq!(a.counts().jumps, 1);
}

#[test]
fn the_assembler_bounds_its_store_and_stalls_ack_at_cap() {
    let cap = 4u32;
    let mut a = Assembler::with_capacity(cap as usize);
    let store = cap + SEND_WINDOW;
    let r = a.on_stream(&stream_of(0, false, 10));
    assert_eq!((r.stored, r.beyond), (10, 0));
    assert_eq!(a.ack(), cap);
    let r = a.on_stream(&stream_of(store - 1, false, 1));
    assert_eq!((r.stored, r.beyond), (1, 0));
    let r = a.on_stream(&stream_of(store, false, 1));
    assert_eq!((r.stored, r.beyond), (0, 1));
    assert_eq!(a.counts().beyond, 1);
    assert_eq!(a.pop_indexed().map(|x| x.0), Some(0));
    assert_eq!(a.ack(), cap + 1);
    let got = drain(&mut a);
    assert_eq!(got.last().map(|g| g.0), Some(9));
    assert_eq!(a.ack(), 10, "持っていた 0..10 の次の抜けの手前まで");
}

#[test]
fn the_assembler_reports_what_it_holds_beyond_the_run_in_the_ack_bit_layout() {
    let mut a = Assembler::with_capacity(8); // 置き場は 8 + 64 = 72
    assert_eq!(a.sack(), 0, "何も持っていない");
    a.on_stream(&stream_of(0, false, 3));
    a.on_stream(&stream_of(5, false, 2));
    assert_eq!((a.ack(), a.sack()), (3, sack_of_have(3, &[5, 6])));
    a.on_stream(&stream_of(66, false, 1));
    a.on_stream(&stream_of(67, false, 1));
    assert_eq!(a.sack(), sack_of_have(3, &[5, 6, 66]));
    assert_eq!(a.sack() >> wire::SACK_BITS, 0, "予約ビットは立てない");
    a.on_stream(&stream_of(3, false, 2));
    assert_eq!((a.ack(), a.sack()), (7, sack_of_have(7, &[66, 67])));
    assert!(a.pop().is_some());
    assert_eq!(a.sack(), sack_of_have(7, &[66, 67]));

    // 流れの制御: cap 4 で 10 個来ると、ack は 4 で止まり、置き場には 0..10 が残る。4 自身は連続の終わりで、範囲は 5 から。
    let cap = 4u32;
    let mut a = Assembler::with_capacity(cap as usize);
    a.on_stream(&stream_of(0, false, 10));
    a.on_stream(&stream_of(cap + SEND_WINDOW - 1, false, 1)); // 置き場の最後（head + 置き場の大きさ - 1 = 67）
    assert_eq!(a.ack(), cap);
    assert_eq!(
        a.sack(),
        sack_of_have(cap, &[5, 6, 7, 8, 9, cap + SEND_WINDOW - 1])
    );
}

#[test]
fn a_late_reset_keeps_what_arrived_ahead_of_it_and_drops_what_is_behind() {
    let mut a = Assembler::with_capacity(8);
    a.on_stream(&stream_of(20, false, 2));
    a.on_stream(&stream_of(5, false, 1));
    assert_eq!((a.ack(), a.sack()), (0, sack_of_have(0, &[5, 20, 21])));
    let r = a.on_stream(&stream_of(10, true, 2));
    assert!(r.jumped);
    assert_eq!(a.ack(), 12);
    assert_eq!(
        a.sack(),
        sack_of_have(12, &[20, 21]),
        "先に着いた 20・21 は残し、飛び先より前の 5 は捨てる"
    );
    a.on_stream(&stream_of(12, false, 8));
    let got = drain(&mut a);
    assert_eq!(
        got.iter().map(|g| g.0).collect::<Vec<_>>(),
        (10..22).collect::<Vec<_>>()
    );
    for (idx, rec) in got {
        assert_eq!(rec, frame(idx), "番号 {idx} の中身");
    }
}

fn watcher(n: u16, hosts: &[SocketAddr]) -> Watcher {
    let mut cfg = watcher_cfg(n);
    cfg.host = Candidates::from_addrs(hosts).expect("候補");
    Watcher::new(cfg)
}

fn from_host(msg: &Message) -> Vec<u8> {
    seal_with(wkeys(0xA001).swapped(), Sender::SpectateHost, msg)
}

fn stream_msg(first: u32, reset: bool, n: u32) -> Message {
    Message::SpectateStream(stream_of(first, reset, n))
}

#[test]
fn the_watcher_joins_to_all_candidates_then_only_to_the_host_and_rejoins() {
    let h2 = SocketAddr::from(([127, 0, 0, 1], 47_001));
    let mut w = watcher(1, &[HOST, h2]);
    let mut sink = Sink::default();
    w.poll(0, &mut sink);
    let first = sink.take();
    assert_eq!(
        first.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![HOST, h2]
    );
    assert!(
        first
            .iter()
            .all(|x| matches!(x.1, Message::SpectateJoin(j) if j.nonce == 0xA001))
    );
    for t in 1..JOIN_RESEND_MS {
        w.poll(t, &mut sink);
        assert!(sink.0.is_empty(), "t={t} で早すぎる JOIN");
    }
    w.poll(JOIN_RESEND_MS, &mut sink);
    assert_eq!(sink.take().len(), 2);
    w.on_datagram(h2, &from_host(&stream_msg(0, true, 3)), 300, &mut sink);
    assert!(w.is_joined());
    assert_eq!(w.host_addr(), Some(h2));
    assert_eq!(
        sink.take(),
        vec![(
            h2,
            Message::SpectateAck {
                nonce: 0xA001,
                next: 3,
                sack: 0
            }
        )]
    );
    w.on_datagram(HOST, &from_host(&stream_msg(3, false, 1)), 301, &mut sink);
    assert_eq!(w.counts().foreign, 1);
    assert!(sink.0.is_empty());
    w.poll(300 + REJOIN_MS - 1, &mut sink);
    assert!(sink.0.is_empty());
    w.poll(300 + REJOIN_MS, &mut sink);
    let re = sink.take();
    assert_eq!(re.len(), 1);
    assert_eq!(re[0].0, h2);
    assert!(matches!(re[0].1, Message::SpectateJoin(_)));
}

#[test]
fn the_watcher_drops_foreign_sources_bad_tags_and_wrong_types() {
    let mut w = watcher(1, &[HOST]);
    let mut sink = Sink::default();
    let good = from_host(&stream_msg(0, true, 2));
    // 候補にない送り元でも、タグの合う STREAM は受ける（`tests/spectate_punch.rs`）。タグの合わないものは捨てる。
    let mut bad_from_stranger = good.clone();
    *bad_from_stranger.last_mut().unwrap() ^= 1;
    w.on_datagram(addr(5), &bad_from_stranger, 0, &mut sink);
    let mut buf = [0u8; MAX_DATAGRAM];
    let to_w = wkeys(0xA001).swapped().sealer(Sender::SpectateHost);
    let n = wire::seal(
        &to_w,
        &Seq::new(),
        SID + 1,
        &stream_msg(0, true, 2),
        &mut buf,
    )
    .unwrap();
    w.on_datagram(HOST, &buf[..n], 0, &mut sink);
    let mut bad = good.clone();
    *bad.last_mut().unwrap() ^= 1;
    w.on_datagram(HOST, &bad, 0, &mut sink);
    w.on_datagram(HOST, b"junk", 0, &mut sink);
    w.on_datagram(
        HOST,
        &from_host(&Message::Ping { send_time: 1 }),
        0,
        &mut sink,
    );
    let c = w.counts();
    assert_eq!(
        (
            c.foreign,
            c.wrong_session,
            c.bad_tag,
            c.not_hgrt,
            c.unexpected
        ),
        (1, 1, 1, 1, 1)
    );
    assert!(!w.is_joined());
    assert_eq!(w.pending(), 0);
    assert!(sink.0.is_empty(), "捨てたものに ack を返さない");
    w.on_datagram(HOST, &good, 5, &mut sink);
    assert!(w.is_joined());
    assert_eq!(w.pending(), 2);
    assert_eq!(w.silent_ms(15), Some(10));
}

#[test]
fn the_watcher_acks_every_stream_and_after_consumption_frees_room() {
    let mut cfg = watcher_cfg(1);
    cfg.host = Candidates::from_addrs(&[HOST]).unwrap();
    let mut w = Watcher::with_assembler(cfg, Assembler::with_capacity(4));
    let mut sink = Sink::default();
    let ack_sack_of = |sink: &mut Sink| match sink.take().as_slice() {
        [(_, Message::SpectateAck { next, sack, .. })] => (*next, *sack),
        other => panic!("ack が 1 通でない: {other:?}"),
    };
    let ack_of = |sink: &mut Sink| ack_sack_of(sink).0;
    w.on_datagram(
        HOST,
        &from_host(&Message::Bye(ByeReason::Cancelled)),
        0,
        &mut sink,
    );
    assert_eq!(w.host_bye(), Some(ByeReason::Cancelled));
    assert!(!w.is_joined());
    w.on_datagram(HOST, &from_host(&stream_msg(0, true, 3)), 1, &mut sink);
    assert_eq!(ack_of(&mut sink), 3);
    assert_eq!(w.host_bye(), None, "流れが来たら BYE を消す");
    w.on_datagram(HOST, &from_host(&stream_msg(0, true, 3)), 2, &mut sink);
    assert_eq!(ack_of(&mut sink), 3);
    w.on_datagram(HOST, &from_host(&stream_msg(10, false, 2)), 3, &mut sink);
    assert_eq!(
        ack_sack_of(&mut sink),
        (3, sack_of_have(3, &[10, 11])),
        "ack は 3 のまま・範囲に 10・11"
    );
    w.on_datagram(HOST, &from_host(&stream_msg(3, false, 3)), 4, &mut sink);
    assert_eq!(ack_sack_of(&mut sink), (4, sack_of_have(4, &[5, 10, 11])));
    assert!(w.pop().is_some());
    w.poll(5, &mut sink);
    assert_eq!(ack_sack_of(&mut sink), (5, sack_of_have(5, &[10, 11])));
    w.poll(6, &mut sink);
    assert!(sink.0.is_empty(), "進んでいなければ送らない");
}

#[test]
fn a_watcher_says_bye_only_after_the_host_is_known_and_the_host_frees_the_slot() {
    let mut w = watcher(1, &[HOST]);
    let mut sink = Sink::default();
    w.bye(ByeReason::Finished, &mut sink);
    assert!(sink.0.is_empty(), "ホストが定まる前に BYE を送った");
    w.on_datagram(HOST, &from_host(&stream_msg(0, true, 3)), 0, &mut sink);
    sink.0.clear();
    w.bye(ByeReason::Cancelled, &mut sink);
    let sent = sink.take();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, HOST);
    assert_eq!(sent[0].1, Message::Bye(ByeReason::Cancelled));
    let mut host = SpectateHost::with_capacity(host_cfg(), 100);
    let mut hs = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut hs);
    assert_eq!(host.spectator_count(), 1);
    let ev = host.on_datagram(
        addr(1),
        &to_host_as(1, &Message::Bye(ByeReason::Cancelled)),
        1,
        &mut hs,
    );
    assert_eq!(ev, HostEvent::Left);
    assert_eq!(host.spectator_count(), 0);
}

/// 1 フレームに 1 回だけ受けて ACK を返す観戦者（dll の観戦者の形）へ、ホストが 1 フレーム 1 記録を流す 60 秒（3,600 フレーム）を、
/// ロスなしのネットワーク（片道 `base` ms）で走らせ、ホストの（送った通数, 送り直し, 観戦者が捨てた重複）を返す。観戦者の周期は 16 ms・位相 `phase`。
fn frame_paced_run_with(base: u32, jitter: u32, phase: u32, drop_pct: u64) -> (u64, u64, u64) {
    let mut rng = Rng(0xFACE);
    let script = make_script(&mut rng, 1, 3_600);
    let limit = (script.len() as u32 + 1) * 16 + 5_000;
    let mut sim = Sim::new(
        Net::new(0x77 + u64::from(phase), base, jitter, drop_pct, 0),
        SpectateHost::with_capacity(host_cfg(), 8_192),
        script,
    );
    sim.add_watcher(1, 0, 16, ASM_CAP);
    sim.set_tick(0, 16, phase);
    sim.run_until_caught_up(limit);
    assert_matches_host(&sim, &sim.watchers[0], 0, "1 フレーム 1 回の観戦者");
    let c = sim.host.counts();
    (
        c.packets_sent,
        c.resends,
        sim.watchers[0].w.assembler_counts().dup,
    )
}

fn frame_paced_run(base: u32, phase: u32) -> (u64, u64, u64) {
    frame_paced_run_with(base, 0, phase, 0)
}

/// 1 フレーム 1 回受ける観戦者へ 60 秒流したときの（ホストが送った記録 / 積んだ記録, ホストの送りの B/s, 観戦者が捨てた重複）。
/// 観戦者の列がホストの列と全行一致することも見る（`assert_matches_host`）。
fn lossy_cost(base: u32, jitter: u32, drop_pct: u64, seed: u64) -> (f64, f64, u64) {
    let r = lossy_run(base, jitter, drop_pct, seed);
    (r.ratio, r.bps, r.dup)
}

struct LossyRun {
    ratio: f64,
    bps: f64,
    up_bps: f64,
    dup: u64,
    /// 記録ごとの配達の遅れ（ms。観戦者が渡した時刻 − ホストが積んだ時刻）。
    delays: Vec<u32>,
    packets: u64,
    resent_records: u64,
    down_total: u64,
}

/// `lossy_cost` の本体: 1 フレーム 1 回受ける観戦者へ 60 秒（3,600 フレーム）流す。観戦者の列が全行一致することも見る。
fn lossy_run(base: u32, jitter: u32, drop_pct: u64, seed: u64) -> LossyRun {
    let mut rng = Rng(0xFACE);
    let script = make_script(&mut rng, 1, 3_600);
    let limit = (script.len() as u32 + 1) * 16 + 30_000;
    let mut sim = Sim::new(
        Net::new(seed, base, jitter, drop_pct, 0),
        SpectateHost::with_capacity(host_cfg(), 8_192),
        script,
    );
    sim.add_watcher(1, 0, 0, ASM_CAP);
    sim.set_tick(0, 16, 8);
    sim.run_until_caught_up(limit);
    assert_matches_host(&sim, &sim.watchers[0], 0, "落ちるネットワークの観戦者");
    let c = sim.host.counts();
    let w = &sim.watchers[0];
    let delays = w
        .got
        .iter()
        .zip(&w.got_at)
        .map(|((idx, _), at)| at.saturating_sub(sim.push_at[*idx as usize]))
        .collect();
    LossyRun {
        ratio: c.records_sent as f64 / c.pushed as f64,
        bps: sim.net.host_bytes as f64 * 1000.0 / f64::from(sim.now),
        up_bps: sim.net.watcher_bytes as f64 * 1000.0 / f64::from(sim.now),
        dup: w.w.assembler_counts().dup,
        delays,
        packets: c.packets_sent,
        resent_records: c.records_resent,
        down_total: sim.net.host_bytes,
    }
}

fn percentile(sorted: &[u32], pct: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() * pct).div_ceil(100)).clamp(1, sorted.len()) - 1]
}

const LOSSY_NETS: [(u32, u32, u64); 5] = [
    (30, 20, 1),
    (5, 20, 5),
    (5, 20, 12),
    (30, 100, 12),
    (60, 100, 20),
];
const LOSSY_SEEDS: [u64; 3] = [0x1234_5679, 0x2468_acf2, 0x369c_f06b];

fn lossy_table_row(
    base: u32,
    jitter: u32,
    drop_pct: u64,
) -> (u32, u32, f64, f64, f64, u64, usize, f64) {
    let mut all = Vec::new();
    let (mut ratio, mut bps, mut up, mut dup, mut total) = (0.0, 0.0, 0.0, 0, 0.0);
    for seed in LOSSY_SEEDS {
        let r = lossy_run(base, jitter, drop_pct, seed);
        ratio += r.ratio / LOSSY_SEEDS.len() as f64;
        bps += r.bps / LOSSY_SEEDS.len() as f64;
        up += r.up_bps / LOSSY_SEEDS.len() as f64;
        total += r.down_total as f64 / LOSSY_SEEDS.len() as f64;
        dup += r.dup;
        all.extend(r.delays);
    }
    all.sort_unstable();
    (
        percentile(&all, 50),
        percentile(&all, 99),
        ratio,
        bps,
        up,
        dup,
        all.len(),
        total,
    )
}

#[test]
fn sack_cuts_the_delivery_delay_on_lossy_lines() {
    // (片道, 揺れ, 落とす %, p50 の上限, p99 の上限)
    let cases: [(u32, u32, u64, u32, u32); 4] = [
        (30, 20, 1, 70, 190),
        (5, 20, 5, 35, 160),
        (30, 100, 12, 400, 1_000),
        (60, 100, 20, 600, 1_500),
    ];
    for (base, jitter, drop_pct, p50_max, p99_max) in cases {
        let (p50, p99, _, _, _, _, n, _) = lossy_table_row(base, jitter, drop_pct);
        eprintln!(
            "片道 {base}・揺れ {jitter}・落とす {drop_pct}%: 遅れ p50 {p50} ms・p99 {p99} ms（n = {n}）"
        );
        assert_eq!(n, 3 * 4_224, "標本の数（3 つの種の全記録）");
        assert!(
            p50 <= p50_max,
            "片道 {base}・揺れ {jitter}・落とす {drop_pct}%: p50 {p50} ms > {p50_max}"
        );
        assert!(
            p99 <= p99_max,
            "片道 {base}・揺れ {jitter}・落とす {drop_pct}%: p99 {p99} ms > {p99_max}"
        );
    }
}

#[test]
fn on_a_line_that_keeps_order_few_records_are_resent_twice() {
    for (base, drop_pct) in [(30u32, 5u64), (30, 12), (60, 12)] {
        let (mut dup, mut resent, mut pushed, mut pkts) = (0u64, 0u64, 0u64, 0u64);
        for seed in LOSSY_SEEDS {
            let r = lossy_run(base, 0, drop_pct, seed);
            dup += r.dup;
            resent += r.resent_records;
            pushed += 4_224;
            pkts += r.packets;
        }
        eprintln!(
            "片道 {base}・揺れ 0・落とす {drop_pct}%: 重複 {dup}・送り直した記録 {resent}・積んだ記録 {pushed}・通 {pkts}"
        );
        assert_eq!(pushed, 12_672, "積んだ記録の数（3 つの種）");
        assert!(
            resent > 0,
            "片道 {base}・落とす {drop_pct}%: 送り直しが 0（落とせていない）"
        );
        assert!(
            dup * 100 <= resent * 15,
            "片道 {base}・落とす {drop_pct}%: 送り直した {resent} 個のうち観戦者が捨てた重複が {dup} 個"
        );
        assert!(
            resent * 100 <= pushed * 20,
            "片道 {base}・落とす {drop_pct}%: 送り直した記録が積んだ {pushed} 個の {resent} 個"
        );
    }
}

#[test]
fn lossy_line_resends_keep_the_column_exact_and_the_cost_bounded() {
    let (_, free_bps, free_dup) = lossy_cost(5, 0, 0, 1);
    assert_eq!(free_dup, 0, "落ちないネットワークでは重複が無いはず");
    let mut n_dup_runs = 0;
    for (base, jitter, drop_pct) in LOSSY_NETS {
        for seed in LOSSY_SEEDS {
            let (ratio, bps, dup) = lossy_cost(base, jitter, drop_pct, seed);
            eprintln!(
                "片道 {base}・ばらつき {jitter}・落とす {drop_pct}%: 送った記録/積んだ記録 {ratio:.3}・{bps:.0} B/s・観戦者が捨てた重複 {dup}"
            );
            assert!(
                ratio < 1.6,
                "片道 {base}・ばらつき {jitter}・落とす {drop_pct}%: 送った記録が積んだ数の {ratio:.2} 倍"
            );
            assert!(
                bps < free_bps * 1.55,
                "片道 {base}・ばらつき {jitter}・落とす {drop_pct}%: 送りが {bps:.0} B/s（落ちないネットワーク {free_bps:.0}）"
            );
            if dup > 0 {
                n_dup_runs += 1;
            }
        }
    }
    // 重複が実在することの記録: 落とすネットワーク 15 実行のうち、観戦者が重複を捨てた実行がある（無くなれば、この検査の閾値の
    // 根拠が変わったので見直す）。
    assert!(n_dup_runs > 0, "落とすネットワークで重複が 1 つも出ない");
}

fn established(seat: Seat, own: &str, peer: &str) -> higanr_core::handshake::Established {
    higanr_core::handshake::Established {
        role: higanr_core::lockstep::Role::Host,
        seat,
        host_seat: seat,
        mode: wire::Mode::Delay,
        delay: 0,
        peer_name: Name::new(peer),
        own_name: Name::new(own),
        peer_latency: 0,
        peer_mode: wire::Mode::Delay,
        rollback_able: false,
        may_list: false,
    }
}

#[test]
fn seg_head_puts_the_seg_first_and_the_names_in_seat_order() {
    let h = higanr_core::spectate::seg_head(7, &established(Seat::P1, "ホスト", "guest"));
    assert_eq!(
        h,
        [
            Record::Seg { seg: 7 },
            Record::name(Seat::P1, "ホスト".as_bytes()),
            Record::name(Seat::P2, b"guest"),
        ]
    );
    let h = higanr_core::spectate::seg_head(0, &established(Seat::P2, "me", "相手"));
    assert_eq!(h[1], Record::name(Seat::P1, "相手".as_bytes()));
    assert_eq!(h[2], Record::name(Seat::P2, b"me"));
}
