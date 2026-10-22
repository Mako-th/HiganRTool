//! `Engine::step`・接続のセッション `link::Link` などがヒープを確保しないこと（ゲームのスレッドで走る）を、割り当て・解放の回数を
//! 数える割り当て器で見る検査。
//!
//! `src/core/src/` でなく `tests/` にあるのは、割り当てを数えるには `unsafe impl GlobalAlloc` が要るが、`higanr_core` は
//! `#![forbid(unsafe_code)]` で、integration test は lib とは別クレートなのでここでだけ `unsafe` を書けるため。

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::check::is_check_frame;
use higanr_core::delay_pick::Recommend;
use higanr_core::handshake::{self, Name};
use higanr_core::link::{self, Link, Outlet, SelExit};
use higanr_core::script::{Engine, Obs, parse};
use higanr_core::seal::KeySeed;
use higanr_core::spectate::{Assembler, HostConfig, SpectateHost, Watcher, WatcherConfig};
use higanr_core::spectate_pace::Pace;
use higanr_core::wire::{ByeReason, Check, MAX_DATAGRAM, Record, Seat, Select};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::net::SocketAddr;

thread_local! {
    // `const` の初期化で、スレッドの局所記憶に直に置かれ、初めて触るときにも割り当てが起きない。
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn bump() {
    // スレッドの終わりぎわ（局所記憶が片付いた後）には数えない。
    let _ = COUNT.try_with(|c| c.set(c.get() + 1));
}

fn count() -> u64 {
    COUNT.try_with(Cell::get).unwrap_or(0)
}

struct Counting;

// SAFETY: どの関数も、受け取った引数をそのまま `System` の同じ関数へ渡して結果を返すだけで、割り当ても解放もしない。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: 呼び出し元が `GlobalAlloc::alloc` の約束（大きさ 0 でない `layout`）を守っている。
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: 同上。
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump();
        // SAFETY: `ptr` はこの割り当て器が `layout` で割り当てたもの（呼び出し元の約束）。
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: 同上。`new_size` の約束も呼び出し元が守っている。
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn obs_scene(scene: u32) -> Obs {
    Obs {
        scene,
        screen: None,
        cursor: None,
        sub: None,
        trans: None,
        pause: false,
        loading: false,
        demo: false,
        end_menu: false,
        menu_flag: false,
        talk: None,
    }
}

#[test]
fn engine_step_never_allocates() {
    let script = parse("scene=1 p1=Z wait=2 hold=3\nscene=2 wait=1\n").expect("parse");
    let mut eng = Engine::new(script);
    // 1,1,1,1,1 は段階0の WaitCond→Wait→Hold の全部を、2,2,2 は段階1(Wait のみ)→Done を通す。
    let scenes = [1u32, 1, 1, 1, 1, 1, 2, 2, 2];

    let before = count();
    for s in scenes {
        eng.step(&obs_scene(s));
    }
    assert_eq!(count(), before, "Engine::step がヒープを確保した");
}

/// 予測の方法（`pred_eval`）は、作った後の `observe` / `predict` / `reset_stream` で確保しない。
#[test]
fn predictors_never_allocate() {
    use higanr_core::pred_eval::{
        Context, FarRelease, HazardRelease, MAX_M, Majority3, Predictor, Repeat, RunHazard,
    };
    let mut p0 = Repeat::default();
    let mut p1 = Majority3::default();
    let mut p2 = Box::<RunHazard>::default();
    let mut p3 = Box::<Context>::default();
    let mut p4 = FarRelease::new(3);
    let mut p5 = Box::new(HazardRelease::new(4));
    let mut out = [0u16; MAX_M];
    // 数え方が働いている証拠: 確保すれば増える。
    let probe = count();
    drop(std::hint::black_box(vec![0u8; 64]));
    assert!(count() > probe, "割り当ての数え方が動いていない");
    let before = count();
    for i in 0..3000u32 {
        // 押す・離すを不規則に混ぜた語。
        let w = ((i.wrapping_mul(2654435761) >> 13) & 0xFF) as u16;
        for p in [
            &mut p0 as &mut dyn Predictor,
            &mut p1,
            &mut *p2,
            &mut *p3,
            &mut p4,
            &mut *p5,
        ] {
            p.observe(w);
            p.predict(1 + (i as usize % MAX_M), &mut out);
            if i % 500 == 499 {
                p.reset_stream();
            }
        }
    }
    assert_eq!(count(), before, "予測の方法がヒープを確保した");
}

/// 送られたデータグラムを固定の枠に溜める `Outlet`（検査の側も確保しない）。溢れた分は捨てる。
#[derive(Clone, Copy)]
struct Mailbox {
    bufs: [[u8; MAX_DATAGRAM]; 16],
    lens: [usize; 16],
    n: usize,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            bufs: [[0; MAX_DATAGRAM]; 16],
            lens: [0; 16],
            n: 0,
        }
    }
}

impl Outlet for Mailbox {
    fn send(&mut self, _to: SocketAddr, bytes: &[u8]) {
        if let (Some(buf), Some(len)) = (self.bufs.get_mut(self.n), self.lens.get_mut(self.n)) {
            buf[..bytes.len()].copy_from_slice(bytes);
            *len = bytes.len();
            self.n += 1;
        }
    }
}

fn deliver(from: &mut Mailbox, from_addr: SocketAddr, to: &mut Link, now: u32, back: &mut Mailbox) {
    let m = std::mem::replace(from, Mailbox::new());
    for i in 0..m.n {
        let _ = to.on_datagram(from_addr, &m.bufs[i][..m.lens[i]], now, back);
    }
}

fn link_cfg(peer: SocketAddr, nonce: u64) -> link::Config {
    link::Config {
        handshake: handshake::Config {
            session_id: 0xABCD_0001,
            role_nonce: nonce,
            latency: 1,
            name: Name::new("alloc"),
            host_seat: Seat::P1,
        },
        secret: Secret([0xA5; 16]),
        peers: Candidates::from_addrs(&[peer]).expect("loopback の候補"),
        key_seed: KeySeed([nonce as u8; 32]),
    }
}

#[test]
fn link_never_allocates() {
    let addr_a = SocketAddr::from(([127, 0, 0, 1], 40_101));
    let addr_b = SocketAddr::from(([127, 0, 0, 1], 40_102));
    let before = count();
    let mut a = Link::new(link_cfg(addr_b, 1), 0);
    let mut b = Link::new(link_cfg(addr_a, 2), 0);
    let made = count();
    assert_eq!(made, before, "セッションを作るときにヒープを確保した");
    let (mut ma, mut mb) = (Mailbox::new(), Mailbox::new());
    let select = Select {
        match_no: 0,
        chara: 1,
        color: 0,
        life: 10,
        charge: 0,
        map: 0,
        difficulty: 0,
        cpu_1p: 0,
        cpu_2p: 0,
        background: 0,
        rng_seed: 9,
        option_digest: 0,
    };
    let mut applied = [0u32; 2];

    let made = count();
    for t in 1..=600u32 {
        a.poll(t, &mut ma);
        b.poll(t, &mut mb);
        deliver(&mut ma, addr_a, &mut b, t, &mut mb);
        deliver(&mut mb, addr_b, &mut a, t, &mut ma);
        for (i, (l, m)) in [(&mut a, &mut ma), (&mut b, &mut mb)]
            .into_iter()
            .enumerate()
        {
            let Some(c) = l.connected_mut() else {
                continue;
            };
            match t {
                100 => c.enter_selection(),
                101..=110 => {
                    c.selection_frame(0x10, None, true, m, |_| SelExit::Stay);
                }
                111 => {
                    c.selection_frame(0, None, true, m, |_| SelExit::Decided(select));
                }
                150 => c.begin_seg(1),
                151..=500 => {
                    if !c.lockstep().captured() {
                        let _ = c.capture(t as u16 | 1, t, m);
                    }
                    if let Some(x) = c.advance(t, m) {
                        applied[i] += 1;
                        if is_check_frame(x.frame) {
                            let chk = Check {
                                seg: c.lockstep().seg(),
                                frame: x.frame,
                                rng_state: 1,
                                rng_count: 2,
                                life_1p: 10,
                                life_2p: 10,
                            };
                            let _ = c.submit_check(chk, m);
                        }
                    }
                }
                510 => c.close(ByeReason::Finished),
                511 => c.flush_bye(t, m),
                _ => {}
            }
        }
    }
    let after = count();

    let ca = a.connected().expect("繋がらなかった");
    let cb = b.connected().expect("繋がらなかった");
    assert!(
        applied[0] >= 100 && applied[1] >= 100,
        "使ったフレーム {applied:?}"
    );
    assert!(
        ca.counts().check_matches >= 3 && cb.counts().check_matches >= 3,
        "CHECK の一致 {} / {}",
        ca.counts().check_matches,
        cb.counts().check_matches
    );
    assert!(ca.selection().selects_ready() && cb.selection().selects_ready());
    assert_eq!(after, made, "繋がりのセッションがヒープを確保した");

    // PONG の応答側（受信スレッドへ渡すコピー）を作って、相手の封じた PING に返す。
    let pong_seq = higanr_core::seal::Seq::new();
    let mut ping = Mailbox::new();
    let before = count();
    let r = a
        .pong_responder()
        .expect("繋がった後は PONG の応答役がある");
    let mut out = [0u8; MAX_DATAGRAM];
    b.poll(3_000, &mut ping);
    let mut answered = 0;
    for i in 0..ping.n {
        if r.answer(&pong_seq, addr_b, &ping.bufs[i][..ping.lens[i]], &mut out)
            .is_some()
        {
            answered += 1;
        }
    }
    drop(r);
    drop(a);
    drop(b);
    let after = count();
    assert!(answered >= 1, "応答役が PING に返さない（{} 通）", ping.n);
    assert_eq!(
        after, before,
        "PONG の応答役・セッションを捨てるときにヒープを確保・解放した"
    );
}

/// 観戦の送信側（`Outlet`）。送られたデータグラムを固定の枠に溜める（検査の側も確保しない）。
#[derive(Clone, Copy)]
struct SpectateMail {
    bufs: [[u8; 512]; 16],
    lens: [usize; 16],
    n: usize,
}

impl SpectateMail {
    fn new() -> Self {
        Self {
            bufs: [[0; 512]; 16],
            lens: [0; 16],
            n: 0,
        }
    }
}

impl Outlet for SpectateMail {
    fn send(&mut self, _to: SocketAddr, bytes: &[u8]) {
        if let (Some(buf), Some(len)) = (self.bufs.get_mut(self.n), self.lens.get_mut(self.n)) {
            buf[..bytes.len()].copy_from_slice(bytes);
            *len = bytes.len();
            self.n += 1;
        }
    }
}

#[test]
fn spectate_never_allocates() {
    let host_addr = SocketAddr::from(([127, 0, 0, 1], 47_000));
    let w_addr = SocketAddr::from(([127, 0, 0, 1], 47_101));
    let secret = Secret([0x5A; 16]);
    let host_cfg = HostConfig {
        session_id: 0x5EC7_0001,
        secret,
        key_seed: KeySeed([0x11; 32]),
    };
    let mut host = SpectateHost::with_capacity(host_cfg, 40);
    let mut w = Watcher::with_assembler(
        WatcherConfig {
            session_id: 0x5EC7_0001,
            secret,
            nonce: 9,
            name: Name::new("alloc"),
            host: Candidates::from_addrs(&[host_addr]).expect("loopback の候補"),
            host_public: host_cfg.public(),
            key_seed: KeySeed([0x22; 32]),
        },
        Assembler::with_capacity(16),
    );
    let (mut to_host, mut to_w) = (SpectateMail::new(), SpectateMail::new());
    let (mut got, mut seen, mut ranged) = (0u32, 0u32, 0u32);
    // 観戦者の早送りの決め方（ゲームのスレッドで 1 表示ごとに呼ぶ）。
    let mut pace = Pace::new();
    let (mut last_ack, mut extras) = (0u32, 0u32);

    let before = count();
    for t in 1..=3_000u32 {
        if t % 16 == 0 {
            let k = t / 16;
            let rec = if k % 20 == 1 {
                Record::Seg { seg: k as u16 }
            } else {
                Record::Frame {
                    frame: k,
                    p1: k as u16,
                    p2: 0,
                }
            };
            let _ = host.push(rec);
        }
        let m = std::mem::replace(&mut to_host, SpectateMail::new());
        for i in 0..m.n {
            seen += 1;
            if seen % 7 != 0 {
                let _ = host.on_datagram(w_addr, &m.bufs[i][..m.lens[i]], t, &mut to_w);
            }
        }
        let m = std::mem::replace(&mut to_w, SpectateMail::new());
        for i in 0..m.n {
            seen += 1;
            if seen % 7 != 0 {
                w.on_datagram(host_addr, &m.bufs[i][..m.lens[i]], t, &mut to_host);
            }
        }
        host.poll(t, &mut to_w);
        w.poll(t, &mut to_host);
        if !w.sack().is_empty() {
            ranged += 1;
        }
        if t % 16 == 8 {
            pace.on_display(w.ack().saturating_sub(last_ack));
            last_ack = w.ack();
            // 溜まりは決め打ち（この検査はすぐ全部使うので溜まらない）。止まり・待ち・早送りの道を順に通す。
            let backlog = (t / 16) % 20;
            if pace.may_play(backlog, 3) {
                if backlog == 0 {
                    pace.on_stall();
                } else {
                    extras += pace.extras(backlog, 3);
                }
            }
        }
        while w.pop().is_some() {
            got += 1;
        }
    }
    let after = count();
    assert!(extras > 0, "早送りの決め方の道を通っていない");

    assert!(w.is_joined(), "JOIN が通らなかった");
    assert!(got >= 100, "届いた記録が {got}");
    assert!(host.counts().resends > 0, "送り直しが 0（落とせていない）");
    assert!(
        host.counts().records_resent > 0 && ranged > 0,
        "穴だけを送り直す道（受け取れた範囲を載せる ACK・範囲を使う送り直し）を通っていない: 送り直した記録 {}・範囲のある周回 {ranged}",
        host.counts().records_resent
    );
    assert_eq!(after, before, "観戦の送りか組み立てがヒープを確保した");
}

/// 中継の木の便（JOIN の木の欄・ACK の中継の状態・INTRO・ADMIT・REHOME・MEASURE・PAIR）の encode / decode と `peek_type` は
/// 受信スレッドで走る。
#[test]
fn spectate_tree_wire_never_allocates() {
    use higanr_core::auth::AuthKey;
    use higanr_core::nat::NatMark;
    use higanr_core::seal::{DirKeys, Sender, Seq};
    use higanr_core::wire::{
        self, JoinTree, Message, PeekedType, RelayState, SpectateAdmit, SpectateIntro,
        SpectateJoin, SpectatePair,
    };
    let key = AuthKey::derive(&Secret([0x5A; 16]), 7);
    let keys = DirKeys::from_raw([1; 32], [1; 32]);
    let seq = Seq::new();
    let cands = Candidates::from_addrs(&[
        SocketAddr::from(([8, 8, 4, 4], 1000)),
        "[2400::1]:7000".parse().expect("IPv6"),
    ])
    .expect("候補");
    let mut name = [0u8; 32];
    name[0] = b'x';
    let msgs = [
        Message::SpectateJoin(SpectateJoin {
            name_len: 1,
            ff_max: 8,
            nonce: 3,
            name,
            build: 1,
            public: [2; 32],
            tree: JoinTree {
                relay_public: Some([3; 32]),
                direct_only: true,
                has_children: true,
                resume_from: Some(9),
                nat: NatMark::UNKNOWN,
                epoch: 4,
                candidates: cands,
            },
        }),
        Message::SpectateAck {
            nonce: 3,
            next: 5,
            relay: RelayState {
                descendants: 2,
                no_intro: false,
            },
            admit_ack: 1,
            sack: higanr_core::bits512::Bits512::EMPTY,
        },
        Message::SpectateIntro(SpectateIntro {
            nonce: 3,
            epoch: 4,
            relay_public: [5; 32],
            relay: cands,
        }),
        Message::SpectateAdmit(SpectateAdmit {
            no: 1,
            start: 10,
            nonce: 3,
            epoch: 4,
            public: [6; 32],
            candidates: cands,
        }),
        Message::SpectateRehome,
        Message::SpectateMeasure { turn: 2 },
        Message::SpectatePair(SpectatePair {
            turn: 2,
            pair: 9,
            second: true,
        }),
    ];
    let mut buf = [0u8; MAX_DATAGRAM];
    let (mut decoded, mut peeked) = (0u32, 0u32);
    let before = count();
    for _ in 0..100 {
        for m in &msgs {
            let n = if wire::is_signed(m) {
                wire::encode(&key, 7, m, &mut buf)
            } else {
                wire::seal(&keys.sealer(Sender::SpectateHost), &seq, 7, m, &mut buf).unwrap_or(0)
            };
            let bytes = buf.get(..n).unwrap_or(&[]);
            if wire::peek_type(bytes) != PeekedType::Other {
                peeked += 1;
            }
            if wire::decode(&key, Some(&keys.opener()), bytes).is_ok_and(|p| p.msg == *m) {
                decoded += 1;
            }
        }
    }
    let after = count();
    assert_eq!((decoded, peeked), (700, 700), "往復か覗きが通っていない");
    assert_eq!(
        after, before,
        "中継の木の便の encode / decode か peek_type がヒープを確保した"
    );
}

/// 中継の木の便の行き来（送り元・宛先つき）。受信スレッドの周回の分だけ固定の枠に溜める。
struct TreeMail {
    bufs: [[u8; 512]; 64],
    lens: [usize; 64],
    ends: [(SocketAddr, SocketAddr); 64],
    n: usize,
}

impl TreeMail {
    fn new() -> Self {
        let a = SocketAddr::from(([127, 0, 0, 1], 1));
        Self {
            bufs: [[0; 512]; 64],
            lens: [0; 64],
            ends: [(a, a); 64],
            n: 0,
        }
    }
}

struct Post<'a> {
    from: SocketAddr,
    mail: &'a mut TreeMail,
}

impl Outlet for Post<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let m = &mut *self.mail;
        if let (Some(buf), Some(len), Some(end)) = (
            m.bufs.get_mut(m.n),
            m.lens.get_mut(m.n),
            m.ends.get_mut(m.n),
        ) {
            buf[..bytes.len()].copy_from_slice(bytes);
            *len = bytes.len();
            *end = (self.from, to);
            m.n += 1;
        }
    }
}

/// 中継の木（紹介・受け入れの依頼・中継役が子へ配る側・中継役が抜けて子へ REHOME・ホストが取り置きを作って使う・`resume_from` つきの JOIN）は受信スレッドで走る。
#[test]
fn spectate_tree_never_allocates() {
    use higanr_core::nat::{NatMark, Symmetry};
    use higanr_core::spectate::{RelayKey, leave_tree, tend_relay};
    use higanr_core::wire::PeekedType;
    let (h_addr, r_addr, c_addr) = (
        SocketAddr::from(([127, 0, 0, 1], 47_000)),
        SocketAddr::from(([127, 0, 0, 1], 47_101)),
        SocketAddr::from(([127, 0, 0, 1], 47_102)),
    );
    let (sid, secret) = (0x5EC7_0002, Secret([0x5A; 16]));
    let host_cfg = HostConfig {
        session_id: sid,
        secret,
        key_seed: KeySeed([0x11; 32]),
    };
    let cfg = |nonce: u64, seed: u8| WatcherConfig {
        session_id: sid,
        secret,
        nonce,
        name: Name::new("tree"),
        host: Candidates::from_addrs(&[h_addr]).expect("loopback の候補"),
        host_public: host_cfg.public(),
        key_seed: KeySeed([seed; 32]),
    };
    let mut host = SpectateHost::with_capacity(host_cfg, 1_024);
    let rkey = RelayKey::new(&KeySeed([0x44; 32]));
    let mut r = Watcher::with_assembler(cfg(1, 0x22), Assembler::with_capacity(16));
    r.set_relay_public(Some(rkey.public()));
    r.set_nat(NatMark {
        symmetry: Symmetry::No,
        upnp: false,
    });
    let mut relay = SpectateHost::relay_with_capacity(sid, secret, rkey, 1_024);
    let mut c = Watcher::with_assembler(cfg(2, 0x33), Assembler::with_capacity(16));
    let (mut now_mail, mut next_mail) = (TreeMail::new(), TreeMail::new());
    let (mut noted, mut via_relay) = (false, 0u32);
    // 子が中継役から受けている間に、中継役が観戦を終える（ホストへ BYE、子へ REHOME。以後は中継役への便を捨てる）。
    let leave_at = 3_500;
    let r_down = |t: u32| t > leave_at;

    let before = count();
    for t in 1..=8_000u32 {
        if t % 16 == 0 {
            let k = t / 16;
            let rec = if k == 1 {
                Record::Seg { seg: 0 }
            } else {
                Record::Frame {
                    frame: k,
                    p1: k as u16,
                    p2: 0,
                }
            };
            let _ = host.push(rec);
        }
        std::mem::swap(&mut now_mail, &mut next_mail);
        next_mail.n = 0;
        for i in 0..now_mail.n {
            let (from, to) = now_mail.ends[i];
            let bytes = &now_mail.bufs[i][..now_mail.lens[i]];
            if to == h_addr {
                let mut post = Post {
                    from: h_addr,
                    mail: &mut next_mail,
                };
                let _ = host.on_datagram(from, bytes, t, &mut post);
            } else if to == r_addr && !r_down(t) {
                let mut post = Post {
                    from: r_addr,
                    mail: &mut next_mail,
                };
                match higanr_core::wire::peek_type(bytes) {
                    PeekedType::SpectateJoin | PeekedType::SpectateAck => {
                        let _ = relay.on_datagram(from, bytes, t, &mut post);
                    }
                    PeekedType::Bye if from != h_addr => {
                        let _ = relay.on_datagram(from, bytes, t, &mut post);
                    }
                    _ => r.on_datagram(from, bytes, t, &mut post),
                }
            } else if to == c_addr {
                let mut post = Post {
                    from: c_addr,
                    mail: &mut next_mail,
                };
                c.on_datagram(from, bytes, t, &mut post);
            }
        }
        {
            let mut post = Post {
                from: h_addr,
                mail: &mut next_mail,
            };
            host.poll(t, &mut post);
        }
        if !noted && r.is_joined() {
            host.note_uplink(r_addr, 1_000_000);
            noted = true;
        }
        if !r_down(t) {
            let mut post = Post {
                from: r_addr,
                mail: &mut next_mail,
            };
            r.poll(t, &mut post);
            while let Some((idx, rec)) = r.pop_indexed() {
                relay.push_at(idx, rec);
            }
            tend_relay(&mut r, &mut relay, t, &mut post);
            relay.poll(t, &mut post);
            if t == leave_at {
                leave_tree(&mut r, Some(&mut relay), ByeReason::Cancelled, &mut post);
            }
        }
        if t >= 1_000 {
            let mut post = Post {
                from: c_addr,
                mail: &mut next_mail,
            };
            c.poll(t, &mut post);
            while c.pop().is_some() {
                if !c.parent_is_home() {
                    via_relay += 1;
                }
            }
        }
    }
    let after = count();

    assert!(
        c.tree_counts().intros_taken >= 1 && via_relay > 0,
        "子が中継役から受けていない: 紹介 {:?}・中継役から {via_relay}",
        c.tree_counts()
    );
    assert!(
        c.parent_is_home() && c.epoch() >= 2,
        "子が元のホストへ戻っていない"
    );
    assert!(
        host.ack_of(c_addr).is_some(),
        "ホストが続きの番号の JOIN を受けていない"
    );
    assert!(relay.counts().joined >= 1 && host.counts().intros >= 1);
    assert!(
        c.tree_counts().rehomes >= 1 && host.counts().left >= 1,
        "中継役の BYE と REHOME の道を通っていない"
    );
    assert!(
        host.counts().reserved_used >= 1,
        "戻った子が取り置きを使う道を通っていない"
    );
    assert_eq!(after, before, "中継の木の処理がヒープを確保した");
}

/// 1,200 B の便（測りの組）も載る行き来の枠。
struct BigMail {
    bufs: [[u8; MAX_DATAGRAM]; 64],
    lens: [usize; 64],
    ends: [(SocketAddr, SocketAddr); 64],
    n: usize,
}

impl BigMail {
    fn new() -> Self {
        let a = SocketAddr::from(([127, 0, 0, 1], 1));
        Self {
            bufs: [[0; MAX_DATAGRAM]; 64],
            lens: [0; 64],
            ends: [(a, a); 64],
            n: 0,
        }
    }
}

struct BigPost<'a> {
    from: SocketAddr,
    mail: &'a mut BigMail,
}

impl Outlet for BigPost<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        let m = &mut *self.mail;
        if let (Some(buf), Some(len), Some(end)) = (
            m.bufs.get_mut(m.n),
            m.lens.get_mut(m.n),
            m.ends.get_mut(m.n),
        ) {
            buf[..bytes.len()].copy_from_slice(bytes);
            *len = bytes.len();
            *end = (self.from, to);
            m.n += 1;
        }
    }
}

/// 上りの測り（ホストが観戦者へ番を渡す・ホストから観戦者への MEASURE の送り直し・観戦者が組を送る・ホストが組を受けて締める・PAIR を `on_datagram` が開かずに
/// 捨てる）は受信スレッドで走る。
#[test]
fn spectate_uplink_measure_never_allocates() {
    use higanr_core::nat::{NatMark, Symmetry};
    use higanr_core::spectate::RelayKey;
    use higanr_core::wire::{self, PeekedType, RelayState};
    let (h_addr, w_addr) = (
        SocketAddr::from(([127, 0, 0, 1], 47_200)),
        SocketAddr::from(([127, 0, 0, 1], 47_201)),
    );
    let (sid, secret) = (0x5EC7_0004, Secret([0x5A; 16]));
    let host_cfg = HostConfig {
        session_id: sid,
        secret,
        key_seed: KeySeed([0x11; 32]),
    };
    let mut host = SpectateHost::with_capacity(host_cfg, 1_024);
    let mut w = Watcher::with_assembler(
        WatcherConfig {
            session_id: sid,
            secret,
            nonce: 7,
            name: Name::new("up"),
            host: Candidates::from_addrs(&[h_addr]).expect("loopback の候補"),
            host_public: host_cfg.public(),
            key_seed: KeySeed([0x22; 32]),
        },
        Assembler::with_capacity(16),
    );
    w.set_relay_public(Some(RelayKey::new(&KeySeed([0x44; 32])).public()));
    w.set_nat(NatMark {
        symmetry: Symmetry::No,
        upnp: false,
    });
    // dll が中継の配る側の状態を毎周渡したときと同じ（既定の「紹介しないで」のままだと番が来ない）。
    w.set_relay_state(RelayState {
        descendants: 0,
        no_intro: false,
    });
    let (mut now_mail, mut next_mail) = (BigMail::new(), BigMail::new());
    let (mut lost_measure, mut via_datagram) = (false, 0u64);

    let before = count();
    for t in 1..=4_000u32 {
        if t % 16 == 0 {
            let k = t / 16;
            let rec = if k == 1 {
                Record::Seg { seg: 0 }
            } else {
                Record::Frame {
                    frame: k,
                    p1: k as u16,
                    p2: 0,
                }
            };
            let _ = host.push(rec);
        }
        std::mem::swap(&mut now_mail, &mut next_mail);
        next_mail.n = 0;
        for i in 0..now_mail.n {
            let (from, to) = now_mail.ends[i];
            let bytes = &now_mail.bufs[i][..now_mail.lens[i]];
            let kind = wire::peek_type(bytes);
            if to == h_addr {
                let mut post = BigPost {
                    from: h_addr,
                    mail: &mut next_mail,
                };
                if kind == PeekedType::SpectatePair {
                    let _ = host.on_pair(from, bytes, u64::from(t) * 1_000 + i as u64);
                    let _ = host.on_datagram(from, bytes, t, &mut post);
                    via_datagram += 1;
                } else {
                    let _ = host.on_datagram(from, bytes, t, &mut post);
                }
            } else if to == w_addr {
                // 最初の MEASURE を落として、送り直しの道を通す。
                if kind == PeekedType::SpectateMeasure && !lost_measure {
                    lost_measure = true;
                    continue;
                }
                let mut post = BigPost {
                    from: w_addr,
                    mail: &mut next_mail,
                };
                w.on_datagram(from, bytes, t, &mut post);
            }
        }
        {
            let mut post = BigPost {
                from: h_addr,
                mail: &mut next_mail,
            };
            host.poll(t, &mut post);
        }
        let mut post = BigPost {
            from: w_addr,
            mail: &mut next_mail,
        };
        w.poll(t, &mut post);
        while w.pop().is_some() {}
    }
    let after = count();

    let c = host.counts();
    assert!(
        lost_measure && c.measure_resends >= 1,
        "MEASURE の送り直しの道を通っていない"
    );
    assert_eq!(
        (c.measures, c.uplinks_measured, c.pairs),
        (1, 1, 20),
        "番を締めて見積もりを決める道を通っていない"
    );
    assert_eq!(w.tree_counts().pairs_sent, 20);
    assert!(host.uplink_of(w_addr).is_some_and(|u| u > 0));
    assert_eq!(c.unexpected, via_datagram, "PAIR を on_datagram が受けた");
    assert_eq!(after, before, "上りの測りがヒープを確保した");
}

/// 中継の木の端の道（中継役が飛んで子を REHOME する・中継役が持たない始まりの子へ REHOME を返す・ホストから観戦者への満杯の断り）。
#[test]
fn spectate_tree_edges_never_allocate() {
    use higanr_core::seal::Ephemeral;
    use higanr_core::spectate::{MAX_SPECTATORS, RelayKey};
    use higanr_core::wire::{JoinTree, Message, SpectateAdmit, SpectateJoin};
    let (sid, secret) = (0x5EC7_0003, Secret([0x5A; 16]));
    let key = higanr_core::auth::AuthKey::derive(&secret, sid);
    let host_cfg = HostConfig {
        session_id: sid,
        secret,
        key_seed: KeySeed([0x11; 32]),
    };
    let at = |n: u16| SocketAddr::from(([127, 0, 0, 1], 47_200 + n));
    // 観戦者 n の JOIN（先に作っておく。作るのは検査の外）。
    const N: usize = MAX_SPECTATORS + 1;
    let mut joins = [[0u8; MAX_DATAGRAM]; N + 2];
    let mut lens = [0usize; N + 2];
    let mut publics = [[0u8; 32]; N + 2];
    for (k, (buf, len)) in joins.iter_mut().zip(lens.iter_mut()).enumerate() {
        let public = Ephemeral::from_seed(&KeySeed([0x60 + k as u8; 32])).public();
        publics[k] = public;
        let msg = Message::SpectateJoin(SpectateJoin {
            name_len: 0,
            ff_max: 8,
            nonce: 0x900 + k as u64,
            name: [0; 32],
            build: higanr_core::update::BUILD_NUMBER,
            public,
            tree: JoinTree {
                epoch: 1,
                ..JoinTree::NONE
            },
        });
        *len = higanr_core::wire::encode(&key, sid, &msg, buf);
    }
    let mut host = SpectateHost::with_capacity(host_cfg, 64);
    let mut relay =
        SpectateHost::relay_with_capacity(sid, secret, RelayKey::new(&KeySeed([0x44; 32])), 64);
    let admit = |k: usize, start: u32| SpectateAdmit {
        no: k as u16,
        start,
        nonce: 0x900 + k as u64,
        epoch: 1,
        public: publics[k],
        candidates: Candidates::EMPTY,
    };
    let (admit_a, admit_b) = (admit(N, 0), admit(N + 1, 90_000));
    let mut mail = SpectateMail::new();
    let mut refused = 0u32;

    let before = count();
    let _ = host.push(Record::Seg { seg: 0 });
    for k in 0..N {
        mail.n = 0;
        let ev = host.on_datagram(
            at(k as u16),
            &joins[k][..lens[k]],
            k as u32 * 150,
            &mut mail,
        );
        if ev == higanr_core::spectate::HostEvent::Refused && mail.n > 0 {
            refused += 1;
        }
    }
    for f in 0..20u32 {
        relay.push_at(f, Record::FILLER);
    }
    relay.admit(admit_a, 2_000);
    relay.admit(admit_b, 2_000);
    mail.n = 0;
    let joined = relay.on_datagram(at(N as u16), &joins[N][..lens[N]], 2_001, &mut mail);
    let rehomed = relay.on_datagram(
        at(N as u16 + 1),
        &joins[N + 1][..lens[N + 1]],
        2_002,
        &mut mail,
    );
    relay.push_at(5_000, Record::FILLER);
    mail.n = 0;
    relay.poll(2_003, &mut mail);
    let after = count();

    use higanr_core::spectate::HostEvent;
    assert_eq!(refused, 1, "満杯の断りの BYE を通っていない");
    assert_eq!((joined, rehomed), (HostEvent::Joined, HostEvent::Rehomed));
    assert_eq!(
        (relay.counts().relay_jumps, relay.spectator_count()),
        (1, 0),
        "飛んで子を REHOME する道を通っていない"
    );
    assert_eq!(after, before, "中継の木の端の道がヒープを確保した");
}

/// 入れ替え（マッチの切れ目に直接の葉を中継役へ紹介し、中継役が受け取ったら元の枠を空ける）と、照合のずれで中継役の下から直接へ
/// 戻る口と、中継役がホストへ知らせる JOIN は受信スレッドで走る。
#[test]
fn spectate_swap_and_mismatch_return_never_allocate() {
    use higanr_core::nat::{NatMark, Symmetry};
    use higanr_core::spectate::{RelayKey, tend_relay};
    use higanr_core::wire::PeekedType;
    let h_addr = SocketAddr::from(([127, 0, 0, 1], 47_300));
    let r_addr = SocketAddr::from(([127, 0, 0, 1], 47_301));
    let leaf_addr = |k: u16| SocketAddr::from(([127, 0, 0, 1], 47_302 + k));
    let (sid, secret) = (0x5EC7_0005, Secret([0x5A; 16]));
    let host_cfg = HostConfig {
        session_id: sid,
        secret,
        key_seed: KeySeed([0x11; 32]),
    };
    let cfg = |nonce: u64, seed: u8| WatcherConfig {
        session_id: sid,
        secret,
        nonce,
        name: Name::new("swap"),
        host: Candidates::from_addrs(&[h_addr]).expect("loopback の候補"),
        host_public: host_cfg.public(),
        key_seed: KeySeed([seed; 32]),
    };
    let mut host = SpectateHost::with_capacity(host_cfg, 1_024);
    let rkey = RelayKey::new(&KeySeed([0x44; 32]));
    let mut r = Watcher::with_assembler(cfg(1, 0x22), Assembler::with_capacity(16));
    r.set_relay_public(Some(rkey.public()));
    r.set_nat(NatMark {
        symmetry: Symmetry::No,
        upnp: false,
    });
    let mut relay = SpectateHost::relay_with_capacity(sid, secret, rkey, 1_024);
    // 中継しない葉 2 人（中継役より先に直接入る）。
    let mut leaves = [
        Watcher::with_assembler(cfg(2, 0x33), Assembler::with_capacity(16)),
        Watcher::with_assembler(cfg(3, 0x34), Assembler::with_capacity(16)),
    ];
    let (mut now_mail, mut next_mail) = (TreeMail::new(), TreeMail::new());
    let (mut noted, mut returned, mut announced) = (false, false, false);
    // マッチ 0 の語は 2 秒まで。その後は `Seg(1)` を積んで何も積まない（マッチの切れ目）。
    let seg1_at = 2_000;
    let mismatch_at = 4_500;
    let announce_at = 5_000;

    let before = count();
    for t in 1..=6_000u32 {
        if t % 16 == 0 && t <= seg1_at {
            let k = t / 16;
            let rec = match k {
                1 => Record::Seg { seg: 0 },
                _ if t == seg1_at => Record::Seg { seg: 1 },
                _ => Record::Frame {
                    frame: k,
                    p1: k as u16,
                    p2: 0,
                },
            };
            let _ = host.push(rec);
        }
        std::mem::swap(&mut now_mail, &mut next_mail);
        next_mail.n = 0;
        for i in 0..now_mail.n {
            let (from, to) = now_mail.ends[i];
            let bytes = &now_mail.bufs[i][..now_mail.lens[i]];
            if to == h_addr {
                let mut post = Post {
                    from: h_addr,
                    mail: &mut next_mail,
                };
                let _ = host.on_datagram(from, bytes, t, &mut post);
            } else if to == r_addr {
                let mut post = Post {
                    from: r_addr,
                    mail: &mut next_mail,
                };
                match higanr_core::wire::peek_type(bytes) {
                    PeekedType::SpectateJoin | PeekedType::SpectateAck => {
                        let _ = relay.on_datagram(from, bytes, t, &mut post);
                    }
                    PeekedType::Bye if from != h_addr => {
                        let _ = relay.on_datagram(from, bytes, t, &mut post);
                    }
                    _ => r.on_datagram(from, bytes, t, &mut post),
                }
            } else if let Some(k) = (0..2u16).find(|&k| to == leaf_addr(k)) {
                let mut post = Post {
                    from: to,
                    mail: &mut next_mail,
                };
                leaves[usize::from(k)].on_datagram(from, bytes, t, &mut post);
            }
        }
        {
            let mut post = Post {
                from: h_addr,
                mail: &mut next_mail,
            };
            host.poll(t, &mut post);
        }
        if !noted && r.is_joined() {
            host.note_uplink(r_addr, 1_000_000);
            noted = true;
        }
        if t >= 500 {
            let mut post = Post {
                from: r_addr,
                mail: &mut next_mail,
            };
            r.poll(t, &mut post);
            while let Some((idx, rec)) = r.pop_indexed() {
                relay.push_at(idx, rec);
            }
            tend_relay(&mut r, &mut relay, t, &mut post);
            relay.poll(t, &mut post);
            if t == announce_at {
                announced = r.announce(&mut post);
            }
        }
        for (k, w) in leaves.iter_mut().enumerate() {
            let mut post = Post {
                from: leaf_addr(k as u16),
                mail: &mut next_mail,
            };
            if k == 0 && t == mismatch_at {
                returned = w.return_on_mismatch(t, &mut post);
            }
            w.poll(t, &mut post);
            while w.pop().is_some() {}
        }
    }
    let after = count();

    let c = host.counts();
    assert!(
        c.swaps >= 1 && c.swap_freed >= 1,
        "入れ替えの道を通っていない: {c:?}"
    );
    assert!(
        leaves[0].tree_counts().intros_taken >= 1 && returned,
        "葉が中継役の下へ移ってから照合の口で戻る道を通っていない"
    );
    assert!(leaves[0].parent_is_home() && leaves[0].direct_only());
    assert!(announced, "中継役の知らせの JOIN の道を通っていない");
    assert_eq!(after, before, "入れ替えか照合の口がヒープを確保した");
}

/// 観戦者の照合（ゲームのスレッドで 1 秒に 1 回）: 記録を作る・組にして比べる。
#[test]
fn watch_checks_never_allocate() {
    use higanr_core::check::{OwnCheck, WatchChecks, WatchVerdict, is_watch_check_frame};
    use higanr_core::wire::{CheckKind, WatchCheck};
    let mut w = WatchChecks::default();
    let (mut matched, mut mismatched) = (0u32, 0u32);
    let before = count();
    for f in (0..6_000u32).filter(|&f| is_watch_check_frame(f)) {
        let c = Check {
            seg: 0,
            frame: f,
            rng_state: f as u16,
            rng_count: f,
            life_1p: 1,
            life_2p: 2,
        };
        let Record::Check { kind, check } = Record::check(CheckKind::Game, &c) else {
            unreachable!()
        };
        let own = OwnCheck {
            game: WatchCheck {
                rng_state: if f % 600 == 0 { 0 } else { f as u16 },
                ..check
            },
            digest: check,
        };
        let v = if f % 120 == 0 {
            w.on_host(kind, check).or(w.on_own(own))
        } else {
            w.on_own(own).or(w.on_host(kind, check))
        };
        match v {
            Some(WatchVerdict::Match { .. }) => matched += 1,
            Some(WatchVerdict::Mismatch { .. }) => mismatched += 1,
            None => {}
        }
    }
    let after = count();
    assert!(matched > 50 && mismatched > 5, "{matched} / {mismatched}");
    assert_eq!(after, before, "照合がヒープを確保した");
}

/// 語の配列を領域にした `ArenaMem`。配列は検査の側が先に作る。
struct FixedMem(Box<[u32]>);

impl higanr_core::arena::ArenaMem for FixedMem {
    fn len(&self) -> u32 {
        self.0.len() as u32 * 4
    }
    fn read(&self, off: u32) -> u32 {
        self.0[(off / 4) as usize]
    }
    fn write(&mut self, off: u32, v: u32) {
        self.0[(off / 4) as usize] = v;
    }
}

#[test]
fn arena_alloc_and_free_never_allocate() {
    use higanr_core::arena::{ArenaMem, ArenaState};
    let mut mem = FixedMem(vec![0u32; 1 << 14].into_boxed_slice());
    let mut st = ArenaState::new();
    let sizes = [676u32, 40, 7224, 100, 3000, 16];
    let mut live = [0u32; 6];
    let before = count();
    for round in 0..300u32 {
        for (i, &n) in sizes.iter().enumerate() {
            let p = st.alloc_tagged(&mut mem, n, 0x0040_CB17).expect("入る");
            mem.write(p, 0xDEAD_0000 | round);
            live[i] = p;
        }
        for &p in &live {
            st.free(&mut mem, p).expect("解放できる");
            assert_eq!(mem.read(p), 0, "解放した中身が残っている");
        }
    }
    let after = count();
    assert_eq!((st.allocs, st.frees), (300 * 6, 300 * 6));
    assert_eq!(after, before, "割り当て器の確保・解放がヒープを確保した");
}

/// 確保しない偽のゲーム（状態機械の側の確保だけを見るため、`Window` の呼び出しの側は確保しない）。
struct NoAllocGame {
    chain: u64,
    snap: u64,
    ticks: u32,
    checks: u32,
    confirmed: u32,
}

impl higanr_core::rollback::Window for NoAllocGame {
    fn save(&mut self, _frame: u32) {
        self.snap = self.chain;
    }
    fn restore(&mut self, _frame: u32) {
        self.chain = self.snap;
    }
    fn tick(&mut self, t: higanr_core::rollback::Tick) -> higanr_core::rollback::TickReport {
        self.ticks += 1;
        self.chain = (self.chain ^ (u64::from(t.p1) << 16) ^ u64::from(t.p2))
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(17);
        higanr_core::rollback::TickReport {
            boundary: t.frame >= 20 && (self.chain >> 40) & 0x3F == 0x2A,
        }
    }
    fn check_values(&mut self) -> higanr_core::rollback::CheckValues {
        higanr_core::rollback::CheckValues::default()
    }
    fn confirmed(&mut self, _frame: u32, _p1: u16, _p2: u16) {
        self.confirmed += 1;
    }
    fn check(&mut self, _c: &Check) {
        self.checks += 1;
    }
}

#[test]
fn rollback_never_allocates() {
    use higanr_core::delay_pick::AUTO;
    use higanr_core::lockstep::{CaptureError, RecvOutcome, Role};
    use higanr_core::rollback::{Config, Rollback, Step, Wait};
    const PRESSES: [u8; 6] = [2, 5, AUTO, 8, 0, 3];
    let mut nodes = [
        Rollback::new(Config::rollback(Seat::P1, 1, 8).with_role(Role::Host)),
        Rollback::new(Config::rollback(Seat::P2, 1, 8).with_role(Role::Guest)),
    ];
    let mut games = [(); 2].map(|()| NoAllocGame {
        chain: 0x51ED,
        snap: 0,
        ticks: 0,
        checks: 0,
        confirmed: 0,
    });
    let mut lag = [None, None];
    let mut reply = [None, None];
    let mut sent = 0u32;
    let before = count();
    for it in 0..900u32 {
        for i in 0..2 {
            if let Some(inp) = lag[1 - i].take() {
                sent += 1;
                if !sent.is_multiple_of(5) {
                    let r = nodes[i].on_input(&inp);
                    assert!(!matches!(r, RecvOutcome::BadLat));
                    if let RecvOutcome::PrevSeg { reply: true } = r {
                        reply[i] = nodes[i].build_prev_input();
                    }
                }
            }
            // マッチの途中の Ctrl＋数字（確保の検査が seg の途中の D の切替の経路も通る）。
            if it % 37 == 11 {
                nodes[i].press_delay(PRESSES[((it / 37 + i as u32) % 6) as usize]);
            }
            if !nodes[i].captured() {
                let w = (it.wrapping_mul(2_654_435_761) >> (8 + 3 * i as u32)) as u16 & 0x00FF;
                match nodes[i].capture(w) {
                    Ok(()) => {}
                    Err(CaptureError::NoDelay) => assert!(!nodes[i].delay_known()),
                    Err(e) => panic!("取得は断られない: {e:?}"),
                }
            }
            lag[i] = reply[i].take().or_else(|| Some(nodes[i].build_input()));
            let step = if nodes[i].captured() {
                nodes[i].input_point(&mut games[i])
            } else {
                Step::Wait(Wait::NotCaptured)
            };
            if matches!(step, Step::Ran { .. }) {
                nodes[i].post_present(&mut games[i]);
            }
            if nodes[i].scene_ended() {
                let next = nodes[i].seg().wrapping_add(1);
                if i == 0 {
                    nodes[i].press_delay([2, 0, 1][(next % 3) as usize]);
                }
                nodes[i].begin_seg(next);
                // 選択の間（最初の取得の前）: 推奨値が替わり、ホストが押す（seg の始まりの D を直に替える経路と、auto の旗・seg の始まりの
                // 推奨値の経路を通す）。
                nodes[i].set_recommend(Some(Recommend::flat(1 + (next % 5) as u8)));
                if next.is_multiple_of(2) {
                    nodes[i].press_delay(PRESSES[(next % 6) as usize]);
                }
                games[i].chain = 0x51ED + u64::from(next);
                games[i].snap = 0;
            }
        }
    }
    let after = count();
    assert!(games[0].ticks > 0 && games[1].ticks > 0);
    // 通った経路の確認（何も起きていなければ、確保が無いのは当たり前）。
    let s = nodes[0].stats();
    assert!(
        s.resims > 10 && s.scenes_ended > 0 && s.live_predicted > 0,
        "{s:?}"
    );
    let mut ds = s.seg_d_log.iter().map(|&(_, d)| d).collect::<Vec<u8>>();
    ds.sort_unstable();
    ds.dedup();
    assert!(ds.len() >= 2, "{:?}", s.seg_d_log);
    assert!(nodes[1].stats().delay_waits > 0, "{:?}", nodes[1].stats());
    let g = nodes[1].stats();
    assert!(
        s.d_switches > 0 && s.d_filled + g.d_filled > 0 && s.d_dropped + g.d_dropped > 0,
        "{s:?}"
    );
    assert!(g.wish_sent > 0 && s.wish_taken > 0, "{g:?} {s:?}");
    assert!(s.d_auto > 0 && s.d_head_sets > 0, "{s:?}");
    assert!(
        s.d_switch_log[..s.d_switches.min(16) as usize]
            .iter()
            .any(|x| x.to >= 3),
        "{s:?}"
    );
    assert!(games[0].checks > 0 && games[0].confirmed > 0);
    assert_eq!(after, before, "rollback の状態機械がヒープを確保した");
}
