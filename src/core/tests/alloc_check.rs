//! `Engine::step`・接続のセッション `link::Link` などがヒープを確保しないこと（ゲームのスレッドで走る）を、割り当て・解放の回数を
//! 数える割り当て器で見る検査。
//!
//! `src/core/src/` でなく `tests/` に置くのは、割り当てを数えるには `unsafe impl GlobalAlloc` が要るが、`higanr_core` は
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
        if w.sack() != 0 {
            ranged += 1;
        }
        while w.pop().is_some() {
            got += 1;
        }
    }
    let after = count();

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
