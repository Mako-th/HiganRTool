//! 2 台の模型（`lockstep` の検査の中だけで使う）。
//!
//! - 1 ms 単位の時計。フレームは 16 ms ごとの枠で、2 台の枠の始まりはずらしてある。
//! - 枠の始まりに、そのフレームを未取得なら `capture` → INPUT を送る。枠の中は 1 ms ごとに `advance` を試し、
//!   揃わない間は 4 ms ごとに `build_input()` を送り直す。枠の終わりまで揃わなければ描画だけのフレーム（stall）として数え、
//!   次の枠で同じフレームをやり直す（取り直さない）。
//! - 送ったものは必ず encode → decode を通す。方向ごとに、送るたびに `fate` が落とす・遅らせる（入れ替え）・重ねるを決める。
//! - seg の最後のフレームを使ったら `begin_seg` して読み込みに入る（台ごとに長さが違う）。最後の seg を終えた台も
//!   `begin_seg` して、前の seg のバックアップの返信だけを続ける。
//!
//! 台 0 = ホスト（席 2P）、台 1 = ゲスト（席 1P）。席と台の番号をわざと入れ違いにしてある（揃えると by_seat の入れ違いが見えない）。

use super::*;
use crate::wire::{self, MAX_DATAGRAM, Message};

fn net_key() -> &'static crate::auth::AuthKey {
    static K: std::sync::OnceLock<crate::auth::AuthKey> = std::sync::OnceLock::new();
    K.get_or_init(|| crate::auth::AuthKey::derive(&crate::auth::Secret([0x7E; 16]), 0x5EED))
}

/// 模型の向きの鍵（封じた形だけを通す）。鍵の交換は接続のセッションの仕事なので、両方向に同じ 1 本を使う。
fn net_dir() -> crate::seal::DirKeys {
    crate::seal::DirKeys::from_raw([0x7F; 32], [0x7F; 32])
}

fn net_seq() -> &'static crate::seal::Seq {
    static S: std::sync::OnceLock<crate::seal::Seq> = std::sync::OnceLock::new();
    S.get_or_init(crate::seal::Seq::new)
}

pub const FRAME_MS: u64 = 16;
pub const RESEND_MS: u64 = 4;

pub const SEATS: [Seat; 2] = [Seat::P2, Seat::P1];
pub const ROLES: [Role; 2] = [Role::Host, Role::Guest];

pub fn word(node: usize, seg: u16, c: u32) -> u16 {
    let mut z = (node as u64) << 48 ^ (u64::from(seg) << 32) ^ u64::from(c);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 32;
    (z as u16) | 0x8000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Capture,
    Resend,
    PrevReply,
}

#[derive(Debug, Clone, Copy)]
pub struct SendInfo {
    pub from: usize,
    pub seg_idx: u32,
    pub frame: u32,
    pub kind: Kind,
    pub has_lat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    Deliver,
    Drop,
    Delay(u64),
    Dup(u64),
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub delay: u8,
    pub segs: u32,
    pub frames_per_seg: u32,
    pub one_way_ms: u64,
    pub start_ms: [u64; 2],
    pub load_ms: [u64; 2],
    /// ホストが (seg の番号, frame, 値) の取得の前に `request_delay` する。
    pub requests: Vec<(u32, u32, u8)>,
    pub retry_busy: bool,
    pub reply_prev: bool,
    /// Ctrl＋数字の押下: (台, seg の番号, frame, 値)。その台がその取得の前に `press_delay` する（ホストは次の取得で切替、ゲストは要求）。
    pub presses: Vec<(usize, u32, u32, u8)>,
}

impl Scenario {
    pub fn new(delay: u8, segs: u32, frames_per_seg: u32) -> Self {
        Scenario {
            delay,
            segs,
            frames_per_seg,
            one_way_ms: 5,
            start_ms: [0, 7],
            load_ms: [40, 300],
            requests: Vec::new(),
            retry_busy: false,
            reply_prev: true,
            presses: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rec {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
    pub switched_to: Option<u8>,
    pub delay: u8,
}

#[derive(Debug, Default, Clone)]
pub struct Counts {
    pub sent: u32,
    pub dropped: u32,
    pub delayed: u32,
    pub dupped: u32,
    pub accepted: u32,
    pub prev_seg: u32,
    pub prev_replies: u32,
    pub other_seg: u32,
    pub rejected: u32,
}

#[derive(Debug)]
pub struct Outcome {
    pub finished: bool,
    pub end_ms: u64,
    pub rec: [Vec<Rec>; 2],
    pub stalls: [u32; 2],
    pub stats: [Stats; 2],
    pub counts: Counts,
    /// ホストの `request_delay` の結果: (seg の番号, frame, 新しい delay, 結果)。
    pub requests: Vec<(u32, u32, u8, Result<(), LatError>)>,
}

enum Phase {
    Play,
    Load(u64),
    Done,
}

struct Node {
    ls: Lockstep,
    seg_idx: u32,
    phase: Phase,
    slot_start: u64,
    advanced: bool,
    last_send: u64,
    stalls: u32,
    rec: Vec<Rec>,
    retry: Option<u8>,
}

struct Net<'f> {
    in_flight: Vec<(u64, u64, usize, Vec<u8>)>,
    order: u64,
    one_way: u64,
    fate: &'f mut dyn FnMut(&SendInfo) -> Fate,
    counts: Counts,
}

impl Net<'_> {
    fn send(&mut self, t: u64, from: usize, node: &Node, inp: &Input, kind: Kind) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let sealer = net_dir().sealer(crate::seal::Sender::Link);
        let len = wire::seal(&sealer, net_seq(), 0x5EED, &Message::Input(*inp), &mut buf)
            .expect("封じられる");
        let info = SendInfo {
            from,
            seg_idx: node.seg_idx,
            frame: node.ls.frame(),
            kind,
            has_lat: inp.lat.is_some(),
        };
        self.counts.sent += 1;
        let to = 1 - from;
        let at = t + self.one_way;
        match (self.fate)(&info) {
            Fate::Deliver => self.push(at, to, &buf[..len]),
            Fate::Drop => self.counts.dropped += 1,
            Fate::Delay(x) => {
                self.counts.delayed += 1;
                self.push(at + x, to, &buf[..len]);
            }
            Fate::Dup(x) => {
                self.counts.dupped += 1;
                self.push(at, to, &buf[..len]);
                self.push(at + x, to, &buf[..len]);
            }
        }
    }

    fn push(&mut self, at: u64, to: usize, bytes: &[u8]) {
        self.in_flight.push((at, self.order, to, bytes.to_vec()));
        self.order += 1;
    }
}

/// 模型を走らせる。全部の seg を両台が終えるか、時間切れ（`finished = false`）まで。
pub fn run(sc: &Scenario, fate: &mut dyn FnMut(&SendInfo) -> Fate) -> Outcome {
    let mut nodes: Vec<Node> = (0..2)
        .map(|i| Node {
            ls: Lockstep::new(ROLES[i], sc.delay),
            seg_idx: 0,
            phase: Phase::Load(sc.start_ms[i]),
            slot_start: 0,
            advanced: false,
            last_send: 0,
            stalls: 0,
            rec: Vec::new(),
            retry: None,
        })
        .collect();
    let mut net = Net {
        in_flight: Vec::new(),
        order: 0,
        one_way: sc.one_way_ms,
        fate,
        counts: Counts::default(),
    };
    let mut requests = Vec::new();
    let total = u64::from(sc.segs) * u64::from(sc.frames_per_seg);
    let max_ms = total * FRAME_MS * 6 + 20_000;
    let mut finished = false;
    let mut t = 0u64;
    while t < max_ms {
        let mut due: Vec<(u64, usize, Vec<u8>)> = Vec::new();
        net.in_flight.retain(|(at, ord, to, b)| {
            if *at == t {
                due.push((*ord, *to, b.clone()));
                false
            } else {
                true
            }
        });
        due.sort_by_key(|d| d.0);
        for (_, to, bytes) in due {
            let pkt = wire::decode(net_key(), Some(&net_dir().opener()), &bytes)
                .expect("模型が送ったものは必ず読める");
            let Message::Input(inp) = pkt.msg else {
                panic!("INPUT 以外は送っていない")
            };
            let out = nodes[to].ls.on_input(&inp);
            match out {
                RecvOutcome::Accepted { .. } => net.counts.accepted += 1,
                RecvOutcome::PrevSeg { reply } => {
                    net.counts.prev_seg += 1;
                    if reply
                        && sc.reply_prev
                        && let Some(p) = nodes[to].ls.build_prev_input()
                    {
                        net.counts.prev_replies += 1;
                        net.send(t, to, &nodes[to], &p, Kind::PrevReply);
                    }
                }
                RecvOutcome::OtherSeg => net.counts.other_seg += 1,
                _ => net.counts.rejected += 1,
            }
        }

        for i in 0..2 {
            let n = &mut nodes[i];
            let mut new_slot = false;
            match n.phase {
                Phase::Done => continue,
                Phase::Load(until) => {
                    if t < until {
                        continue;
                    }
                    n.phase = Phase::Play;
                    n.slot_start = t;
                    new_slot = true;
                }
                Phase::Play => {
                    if t >= n.slot_start + FRAME_MS {
                        if !n.advanced {
                            n.stalls += 1;
                        }
                        n.slot_start += FRAME_MS;
                        new_slot = true;
                    }
                }
            }
            if new_slot {
                n.advanced = false;
                if !n.ls.captured() {
                    for &(who, s, f, x) in &sc.presses {
                        if who == i && s == n.seg_idx && f == n.ls.frame() {
                            n.ls.press_delay(x);
                        }
                    }
                    if ROLES[i] == Role::Host {
                        let f = n.ls.frame();
                        let mut want = n.retry.take();
                        for &(s, rf, d) in &sc.requests {
                            if s == n.seg_idx && rf == f {
                                want = Some(d);
                            }
                        }
                        if let Some(d) = want {
                            let r = n.ls.request_delay(d);
                            requests.push((n.seg_idx, f, d, r));
                            if r == Err(LatError::Busy) && sc.retry_busy {
                                n.retry = Some(d);
                            }
                        }
                    }
                    let w = word(i, n.ls.seg(), n.ls.frame());
                    n.ls.capture(w).expect("模型の中で取得は断られない");
                    let inp = n.ls.build_input();
                    net.send(t, i, &nodes[i], &inp, Kind::Capture);
                    nodes[i].last_send = t;
                }
            }
            let n = &mut nodes[i];
            if !n.advanced {
                if let Some(a) = n.ls.advance() {
                    n.advanced = true;
                    let (p1, p2) = by_seat(SEATS[i], a.local, a.remote);
                    n.rec.push(Rec {
                        seg: n.ls.seg(),
                        frame: a.frame,
                        p1,
                        p2,
                        switched_to: a.switched_to,
                        delay: n.ls.delay(),
                    });
                    if n.ls.frame() == sc.frames_per_seg {
                        n.seg_idx += 1;
                        let next = n.ls.seg().wrapping_add(1);
                        n.ls.begin_seg(next);
                        n.retry = None;
                        n.phase = if n.seg_idx == sc.segs {
                            Phase::Done
                        } else {
                            Phase::Load(t + sc.load_ms[i])
                        };
                    }
                } else if t >= n.last_send + RESEND_MS {
                    let inp = n.ls.build_input();
                    net.send(t, i, &nodes[i], &inp, Kind::Resend);
                    nodes[i].last_send = t;
                }
            }
        }
        if nodes.iter().all(|n| matches!(n.phase, Phase::Done)) {
            finished = true;
            break;
        }
        t += 1;
    }
    let [a, b]: [Node; 2] = match nodes.try_into() {
        Ok(v) => v,
        Err(_) => unreachable!(),
    };
    Outcome {
        finished,
        end_ms: t,
        stalls: [a.stalls, b.stalls],
        stats: [a.ls.stats(), b.ls.stats()],
        rec: [a.rec, b.rec],
        counts: net.counts,
        requests,
    }
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // `seed | 1` だと 2 と 3 が同じ列になるので、一度かき混ぜる。
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z ^= z >> 31;
        Rng(if z == 0 { 1 } else { z })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// ロス 20%・入れ替え（遅らせる）10%・重複 5%・小さな揺らぎ。
pub fn random_fate(seed: u64) -> impl FnMut(&SendInfo) -> Fate {
    let mut rng = Rng::new(seed);
    move |_info| {
        let r = rng.below(100);
        if r < 20 {
            Fate::Drop
        } else if r < 30 {
            Fate::Delay(1 + rng.below(40))
        } else if r < 35 {
            Fate::Dup(1 + rng.below(30))
        } else {
            Fate::Delay(rng.below(3))
        }
    }
}
