//! rollback のモードのマッチの経路（`netplay/rollback.rs`）の検査（pure）。`NetPlay` を 2 つ、仮のネットワーク（片道の遅れ）と偽のゲーム
//! （`netplay::fake::FakeGame` は場面と自分の語、`rollback_testkit::FakeGame` は世界の状態とコピー）で繋ぎ、走査の包みのフックと Present の後のフックを交互に回す。
//! 世界の状態は feed された語の並びだけで決まるので、両者の最後の状態が「確定した行」から手で組んだ参照と一致すれば、予測の入力ワードが確定に混ざっておらず進め直しが正しい。
#![forbid(unsafe_code)]

use super::fake::{FRAME_US, FakeGame as ScriptGame, FakeNet, local_word};
use super::rollback::RbScan;
use super::*;
use crate::rollback_play::{Fault, PlayState, SUP_FIELDS};
use crate::rollback_testkit::{FakeGame as Mem, STATE, play_state};
use crate::th09_addr::ROLLBACK_BLOCKS;
use higanr_core::handshake::{self, ModeWish, Name};
use higanr_core::link;
use higanr_core::lockstep::LatError;
use higanr_core::wire::Seat;
use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;

const SID: u32 = 0x5EED_3001;
const SEED_A: u64 = 0xA1;
const SEED_B: u64 = 0xB1;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

struct Side {
    np: NetPlay,
    g: ScriptGame,
    net: FakeNet,
    st: PlayState,
    mem: Mem,
    rows: Vec<FrameRec>,
    /// 場面の境目が確定したときに偽のゲームが立てる次の要求（既定 10 = 再戦。6 = リプレイを保存して終了）。
    boundary_req: u32,
    quits: Vec<usize>,
    /// `Some` なら、毎フレームの走査の後に `hud_facts` を呼び（本物の呼び手 `script_win::ui_hud_tick` と同じ）、戻りを順に溜める。
    hud_log: Option<Vec<Option<crate::net_ui::HudFacts>>>,
}

fn pair(
    wish_a: ModeWish,
    wish_b: ModeWish,
    check: CheckSource,
    ahead: u8,
    one_way_frames: u64,
    lat_plan: LatPlan,
) -> (Side, Side, Rc<Cell<u64>>) {
    pair_with_ring(wish_a, wish_b, check, ahead, one_way_frames, lat_plan, None)
}

/// `pair` に、ホスト（A）へ観戦のリングバッファを付ける関数（繋がる前に置く。リングバッファの最初の記録は繋がった tick の `Seg { 0 }`）。
fn pair_with_ring(
    wish_a: ModeWish,
    wish_b: ModeWish,
    check: CheckSource,
    ahead: u8,
    one_way_frames: u64,
    lat_plan: LatPlan,
    ring: Option<Arc<crate::spectate_ring::SpectateRing>>,
) -> (Side, Side, Rc<Cell<u64>>) {
    let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), one_way_frames * FRAME_US);
    let cfg = |w: ModeWish, lat_plan: LatPlan| Config {
        check,
        wait_budget_us: WAIT_BUDGET_US,
        lat_plan,
        mode_wish: w,
        rollback_ahead: ahead,
        ..Config::default()
    };
    let lc = |peer: u16, nonce: u64, name: &str| link::Config {
        handshake: handshake::Config {
            session_id: SID,
            role_nonce: nonce,
            latency: 2,
            name: Name::new(name),
            host_seat: Seat::P2,
        },
        secret: peer_sim::cli::DEFAULT_SECRET,
        peers: higanr_core::candidate::Candidates::from_addrs(&[addr(peer)]).unwrap(),
        key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
    };
    let mut npa = NetPlay::new(lc(2, 0xF000, "a"), cfg(wish_a, lat_plan), &mut na);
    if let Some(r) = ring {
        npa.set_spectate(r);
    }
    let npb = NetPlay::new(lc(1, 0x0001, "b"), cfg(wish_b, LatPlan::default()), &mut nb);
    let mk = |np, seed, net| {
        let mut g = ScriptGame::new(seed, 100_000, 1, 1);
        g.menu_before_decide = u32::MAX;
        Side {
            np,
            g,
            net,
            st: play_state(),
            mem: Mem::new(),
            rows: Vec::new(),
            boundary_req: 10,
            quits: Vec::new(),
            hud_log: None,
        }
    };
    let mut a = mk(npa, SEED_A, na);
    let mut b = mk(npb, SEED_B, nb);
    let mut t = clock.get();
    for _ in 0..400 {
        for s in [&mut a, &mut b] {
            s.g.do_frame(&mut s.np, &mut s.net);
        }
        t += FRAME_US;
        clock.set(clock.get().max(t));
        if a.np.connected().is_some() && b.np.connected().is_some() {
            break;
        }
    }
    assert!(a.np.connected().is_some() && b.np.connected().is_some());
    for s in [&mut a, &mut b] {
        s.np.phase = Phase::Lockstep;
        s.np.seg_started = true;
        s.g.cur = SCENE_MATCH;
        s.g.next = 0;
    }
    (a, b, clock)
}

/// 1 フレーム（走査の包みのフック → do_frame の鳴らしと描画 → Present の後のフック）。フックが走査を呼ばなかったら走査が普通に走る（`on_input`）。
/// 場面の境目が確定していたら偽のゲームに次の要求（`boundary_req`）を立てる。`take_save_quit` は本物の呼び手と同じく入力の地点の後に取る。
fn frame(s: &mut Side) -> RbScan {
    let Side {
        np,
        g,
        net,
        st,
        mem,
        rows,
        boundary_req,
        quits,
        hud_log,
    } = s;
    g.begin();
    let r = np.rollback_scan(g, net, st, mem, &mut |rec, _| rows.push(*rec));
    match r {
        RbScan::Ran { .. } => mem.play_and_draw(),
        RbScan::Pass => {
            if np
                .link
                .connected()
                .and_then(|c| c.rollback())
                .is_some_and(|r| r.scene_ended())
            {
                g.cur = SCENE_MATCH;
                g.next = *boundary_req;
                mem.mem.insert(SUP_FIELDS[0], 2);
                mem.mem.insert(SUP_FIELDS[1], 2);
                mem.scene_on_p2 = None;
            }
            let a = np.on_input(g, net);
            g.after(a, np);
            if np.take_save_quit() {
                quits.push(rows.len());
            }
        }
    }
    if let Some(log) = hud_log {
        log.push(np.hud_facts());
    }
    np.rollback_post_present(net, st, mem, &mut |rec, _| rows.push(*rec));
    r
}

fn run(a: &mut Side, b: &mut Side, clock: &Cell<u64>, frames: u32) {
    let mut slot = clock.get();
    for _ in 0..frames {
        if clock.get() < slot {
            clock.set(slot);
        }
        frame(a);
        frame(b);
        slot += FRAME_US;
    }
}

fn settle(a: &mut Side, b: &mut Side, clock: &Cell<u64>, ticks: u32) {
    for _ in 0..ticks {
        clock.set(clock.get() + FRAME_US);
        for s in [&mut *a, &mut *b] {
            let Side {
                np,
                net,
                st,
                mem,
                rows,
                ..
            } = s;
            np.rollback_post_present(net, st, mem, &mut |rec, _| rows.push(*rec));
        }
    }
}

fn drain_ring(ring: &crate::spectate_ring::SpectateRing) -> Vec<higanr_core::wire::Record> {
    let mut v = Vec::new();
    while let Some(p) = ring.pop() {
        assert!(!p.after_gap, "リングバッファが溢れた");
        v.push(p.rec);
    }
    v
}

/// 検査の道具: 記録の列を、ホストの `SpectateHost` → 観戦者の `Watcher` へメモリの上で順に渡して、観戦者が受けた記録の列を返す。
/// 観戦者は先に入っておく（遅れて入ると今の seg の始まりから流れる）。時刻は 1 記録ごとに 1 ms 進める。
fn through_core(recs: &[higanr_core::wire::Record]) -> Vec<higanr_core::wire::Record> {
    use higanr_core::auth::Secret;
    use higanr_core::link::Outlet;
    use higanr_core::spectate::{HostConfig, SpectateHost, Watcher, WatcherConfig};

    struct Queue<'a>(&'a mut Vec<Vec<u8>>);
    impl Outlet for Queue<'_> {
        fn send(&mut self, _to: SocketAddr, bytes: &[u8]) {
            self.0.push(bytes.to_vec());
        }
    }
    let (host_addr, watcher_addr) = (addr(7_001), addr(7_002));
    let (session, secret) = (0x005E_ED77, Secret([5; 16]));
    let host_cfg = HostConfig {
        session_id: session,
        secret,
        key_seed: higanr_core::seal::KeySeed([0x48; 32]),
    };
    let mut host = SpectateHost::with_capacity(host_cfg, 4_096);
    let mut w = Watcher::new(WatcherConfig {
        session_id: session,
        secret,
        nonce: 1,
        name: Name::new("viewer"),
        host: higanr_core::candidate::Candidates::from_addrs(&[host_addr]).unwrap(),
        host_public: host_cfg.public(),
        key_seed: higanr_core::seal::KeySeed([0x57; 32]),
    });
    let (mut to_host, mut to_watcher): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
    let mut got = Vec::new();
    let mut now = 0u32;
    for next in std::iter::once(None).chain(recs.iter().map(Some)) {
        if let Some(r) = next {
            host.push(*r).expect("ホストのリングバッファは溢れない");
        }
        now += 1;
        for _ in 0..50 {
            host.poll(now, &mut Queue(&mut to_watcher));
            w.poll(now, &mut Queue(&mut to_host));
            let (hs, ws) = (
                std::mem::take(&mut to_host),
                std::mem::take(&mut to_watcher),
            );
            if hs.is_empty() && ws.is_empty() {
                break;
            }
            for b in hs {
                host.on_datagram(watcher_addr, &b, now, &mut Queue(&mut to_watcher));
            }
            for b in ws {
                w.on_datagram(host_addr, &b, now, &mut Queue(&mut to_host));
            }
        }
        while let Some(r) = w.pop() {
            got.push(r);
        }
    }
    assert!(w.is_joined(), "観戦者が入っていない");
    got
}

/// rollback と観戦の組み合わせ: ホストがリングバッファへ積む列が対戦者の確定した列と一致し（予測の入力ワードが混ざらない）、seg の境目の並び
/// （`Frame(seg k の最後)` の後に `Seg(k + 1)`）が崩れず、観戦者の組み立てを通しても変わらない。片道 1・3・6 フレーム × 境目の frame 2 通りで再戦を 1 回跨ぐ。
#[test]
fn a_rollback_hosts_spectate_ring_holds_exactly_the_confirmed_rows_across_a_rematch() {
    use crate::spectate_testkit::fold;
    use higanr_core::wire::Record;
    let boundaries: Vec<u32> = (30..120u32)
        .filter(|&f| (0..f).all(|g| ref_row(1, g).1 != ref_row(1, f).1))
        .step_by(25)
        .take(2)
        .collect();
    assert_eq!(boundaries.len(), 2, "標本の境目が 2 つ取れない");
    let mut cells = 0;
    for one_way in [1u64, 3, 6] {
        for &t in &boundaries {
            let ctx = format!("片道 {one_way} フレーム・境目 {t}");
            let ring = Arc::new(crate::spectate_ring::SpectateRing::with_len(1 << 16));
            let (mut a, mut b, clock) = pair_with_ring(
                ModeWish::rollback(1),
                ModeWish::rollback(1),
                CheckSource::InputDigest,
                12,
                one_way,
                LatPlan::default(),
                Some(Arc::clone(&ring)),
            );
            let x = ref_row(1, t).1;
            a.mem.scene_on_p2 = Some(x);
            b.mem.scene_on_p2 = Some(x);
            run(&mut a, &mut b, &clock, t + 200);
            settle(&mut a, &mut b, &clock, 60);
            assert_eq!(a.np.phase(), Phase::Lockstep, "{ctx}: {:?}", a.np.end());
            assert_eq!(b.np.phase(), Phase::Lockstep, "{ctx}: {:?}", b.np.end());
            let recs = drain_ring(&ring);
            assert_eq!(recs.first(), Some(&Record::Seg { seg: 0 }), "{ctx}");
            let f = fold(&recs);
            assert_eq!(f.segs, vec![0, 1], "{ctx}: 再戦を 1 回跨ぐ（seg 0 と 1）");
            let want_names: Vec<(Seat, String)> = [Seat::P1, Seat::P2, Seat::P1, Seat::P2]
                .into_iter()
                .zip(["b", "a", "b", "a"])
                .map(|(s, n)| (s, n.to_string()))
                .collect();
            assert_eq!(f.names, want_names, "{ctx}: 各 seg の先頭の名前");
            let rows = |s: &Side| -> Vec<(u16, u32, u16, u16)> {
                s.rows
                    .iter()
                    .map(|r| (r.seg, r.frame, r.p1, r.p2))
                    .collect()
            };
            assert_eq!(
                f.frames,
                rows(&a),
                "{ctx}: ホストのリングバッファの列が確定の行と違う"
            );
            let at = recs
                .iter()
                .position(|r| matches!(r, Record::Seg { seg: 1 }))
                .unwrap();
            assert!(
                matches!(recs[at - 1], Record::Frame { frame, .. } if frame == t),
                "{ctx}: Seg(1) の直前が境目の frame {t} でない: {:?}",
                recs[at - 1]
            );
            assert!(
                matches!(recs[at + 1], Record::Name { seat: Seat::P1, .. })
                    && matches!(recs[at + 2], Record::Name { seat: Seat::P2, .. }),
                "{ctx}: Seg(1) の直後が名前（1P → 2P）でない: {:?} {:?}",
                recs[at + 1],
                recs[at + 2]
            );
            assert!(
                matches!(recs[at + 3], Record::Frame { frame: 0, .. }),
                "{ctx}: 名前の次が frame 0 でない: {:?}",
                recs[at + 3]
            );
            assert!(
                recs[at + 1..]
                    .iter()
                    .all(|r| !matches!(r, Record::Seg { .. })),
                "{ctx}"
            );
            let watched = through_core(&recs);
            assert_eq!(watched, recs, "{ctx}: 組み立てを通すと列が変わった");
            let fw = fold(&watched);
            let n = fw.frames.len().min(rows(&b).len());
            assert!(n > 100, "{ctx}: 比べた行が少ない {n}");
            assert_eq!(
                &fw.frames[..n],
                &rows(&b)[..n],
                "{ctx}: 観戦者の列がゲストの列と違う"
            );
            assert!(
                fw.frames.iter().filter(|r| r.0 == 1).count() > 50,
                "{ctx}: seg 1 の行が少ない"
            );
            cells += 1;
        }
    }
    assert_eq!(cells, 6, "走らせた組の数");
}

#[test]
fn save_and_quit_at_a_rollback_boundary_closes_both_and_asks_once() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    let t = (30..120u32)
        .find(|&f| (0..f).all(|g| ref_row(1, g).1 != ref_row(1, f).1))
        .unwrap();
    let x = ref_row(1, t).1;
    for s in [&mut a, &mut b] {
        s.mem.scene_on_p2 = Some(x);
        s.boundary_req = REQ_SAVE_AND_QUIT;
    }
    run(&mut a, &mut b, &clock, t + 200);
    settle(&mut a, &mut b, &clock, 60);
    for s in [&a, &b] {
        assert_eq!(
            s.quits,
            vec![(t + 1) as usize],
            "境目の frame まで確定した後に 1 回: {:?} {:?}",
            s.np.end(),
            s.np.stats()
        );
        assert_eq!(s.np.phase(), Phase::Ended);
        assert!(
            matches!(
                s.np.end(),
                Some(End::Closed(ByeReason::Finished) | End::PeerBye(ByeReason::Finished))
            ),
            "{:?}",
            s.np.end()
        );
        assert_eq!(s.np.rb_aborts(), 0, "{:?}", s.np.rb_abort_first());
        assert_eq!(s.g.cur, SCENE_MENU, "試合を出た");
        assert_eq!(s.rows.len() as u32, t + 1, "閉じた後に確定の行が流れた");
        for (i, r) in s.rows.iter().enumerate() {
            let (p1, p2) = ref_row(1, i as u32);
            assert_eq!((r.seg, r.frame, r.p1, r.p2), (0, i as u32, p1, p2));
        }
    }
    assert_eq!(a.rows, b.rows);
}

/// 参照: seg 0 の frame f の (1P, 2P)。1P は B（席 P1）、2P は A（席 P2）の語。D = `d`。
fn ref_row(d: u32, f: u32) -> (u16, u16) {
    let w = |seed: u64| {
        if f < d {
            0
        } else {
            local_word(seed, u64::from(f - d + 1))
        }
    };
    (w(SEED_B), w(SEED_A))
}

fn chain(rows: &[(u16, u16)]) -> u32 {
    rows.iter().fold(1u32, |s, &(p1, p2)| {
        s.wrapping_mul(31)
            .wrapping_add(u32::from(p1) * 7 + u32::from(p2) + 1)
    })
}

fn rollback_pair(
    check: CheckSource,
    ahead: u8,
    one_way_frames: u64,
) -> (Side, Side, Rc<Cell<u64>>) {
    pair(
        ModeWish::rollback(1),
        ModeWish::rollback(1),
        check,
        ahead,
        one_way_frames,
        LatPlan::default(),
    )
}

#[test]
fn two_rollback_sides_confirm_the_same_rows_and_reach_the_same_state() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 240);
    settle(&mut a, &mut b, &clock, 60);
    for s in [&a, &b] {
        let rb = s.np.connected().unwrap().rollback().unwrap();
        assert_eq!(s.np.phase(), Phase::Lockstep, "{:?}", s.np.end());
        assert_eq!(
            (rb.frame(), rb.front()),
            (240, 240),
            "前線が N に追いついていない"
        );
        assert_eq!(s.rows.len(), 240, "確定した行の数");
        for (i, r) in s.rows.iter().enumerate() {
            let (p1, p2) = ref_row(1, i as u32);
            assert_eq!(
                (r.seg, r.frame, r.p1, r.p2),
                (0, i as u32, p1, p2),
                "行 {i} が参照と違う"
            );
        }
        let want: Vec<(u16, u16)> = (0..240).map(|f| ref_row(1, f)).collect();
        assert_eq!(
            s.mem.get(STATE),
            chain(&want),
            "世界の状態が確定した行の鎖と違う"
        );
        assert_eq!(s.g.local_reads, s.np.stats().local_reads);
        assert_eq!(s.g.local_reads, 240, "自分の語を読んだ回数");
        assert_eq!(s.np.stats().played, 240);
        assert!(
            s.np.stats().margin_unknown < 100,
            "余裕が分からない行 {}",
            s.np.stats().margin_unknown
        );
        let ps = s.st.stats();
        assert!(ps.resim_confirmed_ticks > 50, "{ps:?}");
        assert!(ps.unconfirmed_ticks > 50, "{ps:?}");
        let resim_scans = s.mem.scans.iter().filter(|(_, resim)| *resim).count();
        assert!(resim_scans > 50, "進め直しの回の旗つきの走査 {resim_scans}");
    }
    assert_eq!(a.rows, b.rows, "両者の確定の列が違う");
}

/// rollback でも試合の語の Ctrl（`0x0100`）は両席・両側の確定の行に残り、Enter（`0x1000`）は落ちる。
#[test]
fn ctrl_in_the_match_word_reaches_both_seats_in_the_confirmed_rows_of_both_sides() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    for s in [&mut a, &mut b] {
        s.g.local_or = 0x1100;
    }
    run(&mut a, &mut b, &clock, 120);
    settle(&mut a, &mut b, &clock, 60);
    for (who, s) in [("A", &a), ("B", &b)] {
        assert_eq!(s.rows.len(), 120, "{who}: 確定した行の数");
        for (i, r) in s.rows.iter().enumerate() {
            let (p1, p2) = ref_row(1, i as u32);
            let ctrl = if i == 0 { 0 } else { 0x0100 };
            assert_eq!(
                (r.p1, r.p2),
                (p1 | ctrl, p2 | ctrl),
                "{who}: 行 {i}（Ctrl は両席に残り Enter は落ちる）"
            );
        }
        assert_eq!(s.np.phase(), Phase::Lockstep, "{who}: {:?}", s.np.end());
    }
    assert_eq!(a.rows, b.rows, "両者の確定の列が違う");
}

/// リプレイの印の通信の要約: rollback のモードの接続は、形式・何マッチ目・止まった数（入力の地点が待った数）・進め直しの数を渡す。
#[test]
fn the_replay_mark_view_reports_the_rollback_state_and_totals() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 240);
    for s in [&a, &b] {
        let rb = s.np.rollback_core_stats().expect("rollback の接続");
        let v = crate::replay_save::NetView::of(&s.np).expect("つながっていた接続");
        assert_eq!((v.rollback, v.rb_ready), (true, s.np.rollback_ready()));
        assert!(v.rb_ready, "試合の最中");
        // この部品は開始のフックを通さず Lockstep に置く（本物は `on_match_start` が数える）ので、数は入れ物の値のまま。
        assert_eq!(v.match_no, s.np.stats().segs_started);
        assert_eq!(
            v.totals.stalls,
            u64::from(rb.waits_ahead) + u64::from(rb.waits_boundary)
        );
        assert_eq!(v.totals.rewinds, u64::from(rb.resims));
        assert_eq!(
            v.totals.resim_ticks,
            u64::from(rb.resim_confirmed_ticks) + u64::from(rb.resim_predicted_ticks)
        );
        assert!(v.totals.rewinds > 0 && v.totals.resim_ticks > 50, "{rb:?}");
    }
}

#[test]
fn the_input_digest_check_is_built_from_confirmed_rows_like_peer_sim() {
    use higanr_core::check::{check_from_input_digest, input_digest_push};
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 200);
    settle(&mut a, &mut b, &clock, 60);
    let mut d = INPUT_DIGEST_INIT;
    let mut want = Vec::new();
    for f in 0..200u32 {
        let (p1, p2) = ref_row(1, f);
        d = input_digest_push(d, p1, p2);
        if is_check_frame(f) {
            want.push(check_from_input_digest(0, f, d));
        }
    }
    assert!(want.len() >= 6);
    for s in [&a, &b] {
        assert_eq!(
            s.np.sent_checks, want,
            "送った CHECK が確定した行のダイジェストと違う"
        );
        assert_eq!(
            s.np.phase(),
            Phase::Lockstep,
            "CHECK のズレで終わった: {:?}",
            s.np.end()
        );
        let m = s.np.connected().unwrap().counts().check_matches;
        assert!(m >= want.len() as u32, "CHECK が相手と合った数 {m}");
    }
}

#[test]
fn the_game_check_reads_the_confirmed_state_at_the_head_of_the_tick() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::Game, 12, 3);
    run(&mut a, &mut b, &clock, 200);
    settle(&mut a, &mut b, &clock, 60);
    let mut want = Vec::new();
    for f in (0..200u32).filter(|&f| is_check_frame(f)) {
        let rows: Vec<(u16, u16)> = (0..f).map(|g| ref_row(1, g)).collect();
        let state = if f == 0 {
            0x1234
        } else {
            chain(&rows) & 0xFFFF
        };
        want.push(Check {
            seg: 0,
            frame: f,
            rng_state: state as u16,
            rng_count: f,
            life_1p: 0,
            life_2p: 0,
        });
    }
    for s in [&a, &b] {
        assert_eq!(s.np.sent_checks, want);
        assert_eq!(s.np.phase(), Phase::Lockstep, "{:?}", s.np.end());
    }
}

#[test]
fn latency_switches_are_refused_in_the_rollback_mode() {
    let plan = LatPlan::new(&[(5, 4)]).unwrap();
    let (mut a, mut b, clock) = pair(
        ModeWish::rollback(1),
        ModeWish::rollback(1),
        CheckSource::InputDigest,
        12,
        2,
        plan,
    );
    run(&mut a, &mut b, &clock, 60);
    let s = a.np.stats();
    assert_eq!((s.lat_requested, s.lat_refused), (0, 1), "{s:?}");
    assert_eq!(
        a.np.link.connected_mut().unwrap().request_delay(3),
        Err(LatError::RollbackMode)
    );
    settle(&mut a, &mut b, &clock, 30);
    assert_eq!(a.rows.len(), 60, "切替の予定があっても列が進む");
    assert_eq!(a.rows, b.rows);
}

/// 遅れの量を両者が変える: Ctrl+数字を両者が押すと D がマッチの途中で替わり、両者の確定の列が一致する。押下があった証拠（両者の `digit_presses`）を隣に置く。
#[test]
fn both_sides_pick_d_with_digit_keys_and_it_switches_in_the_same_match() {
    use higanr_core::delay_pick;
    use higanr_core::net_menu::{HudView, LineBuf, hud_line};
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
        s.np.ensure_pick();
    }
    // (押し始めの tick, 数字)。各 3 tick 押し続ける。
    let host_plan = [(60u32, 4u8), (140, 2), (220, 5)];
    let guest_plan = [(100u32, 3u8), (180, 1)];
    let held = |plan: &[(u32, u8)], t: u32| {
        plan.iter()
            .find(|(s, _)| (*s..*s + 3).contains(&t))
            .map_or(0, |(_, d)| (1u16 << d) | crate::digit_keys::CTRL_BIT)
    };
    let host_is_a = a.np.connected().unwrap().role() == higanr_core::lockstep::Role::Host;
    let mut slot = clock.get();
    for t in 0..400u32 {
        let (host, guest) = if host_is_a {
            (&mut a, &mut b)
        } else {
            (&mut b, &mut a)
        };
        host.g.digit_held = Some(held(&host_plan, t));
        guest.g.digit_held = Some(held(&guest_plan, t));
        if clock.get() < slot {
            clock.set(slot);
        }
        frame(&mut a);
        frame(&mut b);
        slot += FRAME_US;
    }
    settle(&mut a, &mut b, &clock, 60);
    for (who, s, presses) in [
        ("A", &mut a, if host_is_a { 3 } else { 2 }),
        ("B", &mut b, if host_is_a { 2 } else { 3 }),
    ] {
        assert_eq!(s.np.phase(), Phase::Lockstep, "{who}: {:?}", s.np.end());
        assert_eq!(s.np.rb_aborts(), 0, "{who}: {:?}", s.np.rb_abort_first());
        let st = s.np.stats();
        assert_eq!(st.digit_presses, presses, "{who}: 押下の数 {st:?}");
        assert_eq!(
            (st.lat_requested, st.switches),
            (0, 0),
            "{who}: delay 方式の切替を出した"
        );
        let c = s.np.connected().unwrap();
        let want = c.shown_delay();
        let live = c.liveness();
        let median = live.rtt_median_us().map(|us| us / 1_000).or(live.rtt_ms());
        let max = live.rtt_max_ms();
        let h = s.np.hud_facts().unwrap();
        assert_eq!(
            h.latency,
            Some(want),
            "{who}: 画面の遅れがネットワークの側の目標でない"
        );
        assert_eq!(
            h.recommend,
            delay_pick::recommend_with_max(higanr_core::wire::Mode::Rollback, median, max),
            "{who}: 画面の推奨値が往復の中央値と最大から引いた値でない"
        );
        let mut l = LineBuf::new();
        hud_line(
            &mut l,
            higanr_core::lang::Lang::Ja,
            &HudView {
                latency: h.latency,
                rtt_ms: h.rtt_ms,
                recommend: h.recommend,
                rewind: h.rewound,
                auto: h.auto,
                format: h.format,
                delay_only: h.delay_only,
            },
        );
        assert!(!l.as_str().contains('*'), "{who}: {}", l.as_str());
    }
    let (host, guest) = if host_is_a { (&a, &b) } else { (&b, &a) };
    let ha = host.np.connected().unwrap().rollback().unwrap().stats();
    let gb = guest.np.connected().unwrap().rollback().unwrap().stats();
    assert!(ha.d_switches > 0, "試合の途中で D が替わらない: {ha:?}");
    assert!(
        gb.wish_sent > 0 && ha.wish_taken > 0,
        "ゲストの要求をホストが取らない: {gb:?} {ha:?}"
    );
    let n = a.rows.len().min(b.rows.len());
    assert!(n > 300, "確定の行 {n}");
    assert_eq!(a.rows[..n], b.rows[..n]);
}

#[test]
fn the_log_line_carries_the_input_counts_in_rollback_mode() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 60);
    for s in [&a, &b] {
        let st = crate::net_watch::NetStatus::new();
        st.publish(&s.np);
        let snap = st.snapshot();
        let c = s.np.connected().unwrap().counts();
        assert!(c.input_accepted > 10, "検査が素通り: {c:?}");
        assert_eq!(u64::from(snap.input_accepted), c.input_accepted);
        assert_eq!(u64::from(snap.input_prev_seg), c.input_prev_seg);
        assert_eq!(u64::from(snap.input_other_seg), c.input_other_seg);
        assert_eq!(u64::from(snap.input_rejected), c.input_rejected);
        let want = format!("INPUT の受信ソケット: 受理 {} /", c.input_accepted);
        assert!(snap.text().contains(&want), "{}", snap.text());
    }
}

#[test]
fn the_input_count_phrase_names_every_reason() {
    use crate::net_watch::NetSnapshot;
    let s = NetSnapshot {
        input_accepted: 11,
        input_prev_seg: 22,
        input_prev_replies: 33,
        input_other_seg: 44,
        input_rejected: 55,
        input_bad_lat: 66,
        input_guest_lat: 77,
        ..NetSnapshot::default()
    };
    assert_eq!(
        s.input_text(),
        "INPUT の受信ソケット: 受理 11 / 前の seg 22（控えを返した 33） / 別の seg で捨てた 44 / ほかの理由で捨てた 55（うち lat の取り決め違い BadLat 66 ・ホストが切替つきを受けた GuestLat 77）"
    );
    assert!(
        NetSnapshot::default()
            .input_text()
            .contains("捨てた 0（うち lat の取り決め違い BadLat 0 ・")
    );
}

/// マッチの途中の切替: ホストが Ctrl+数字を押しても、ゲストが押しても、D がそのマッチのうちに替わり、両台が同じフレームで替える。
/// 窓の側へ渡す穴埋めの D（`PlayState` の hint）は seg の始まりの D、確定した行に添える D は今の D、次の seg の始まりの D は前の seg の最後の値。
/// ホストが 2 を押して 1 → 2、ゲストが 1 を押して 2 → 1。どちらも 4 tick 押し続け、ちょうど 1 回の押下。
#[test]
fn a_host_press_and_a_guest_wish_switch_d_mid_match_on_both_sides() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
        s.np.ensure_pick();
        s.mem.scene_every_ticks = Some(400);
    }
    let key = |d: u8| Some((1u16 << d) | crate::digit_keys::CTRL_BIT);
    let plan = |t: u32| match t {
        60..=63 => (key(2), None),
        160..=163 => (None, key(1)),
        _ => (None, None),
    };
    let cur = |s: &Side| s.np.connected().unwrap().cur_delay();
    let mut mid = None;
    let mut slot = clock.get();
    for t in 0..900u32 {
        let (ha, hb) = plan(t);
        let host_is_a = a.np.connected().unwrap().role() == higanr_core::lockstep::Role::Host;
        let (hh, gh) = (ha.or(Some(0)), hb.or(Some(0)));
        (a.g.digit_held, b.g.digit_held) = if host_is_a { (hh, gh) } else { (gh, hh) };
        if clock.get() < slot {
            clock.set(slot);
        }
        let seg_before = (
            a.np.connected().map(|c| c.seg()),
            b.np.connected().map(|c| c.seg()),
        );
        let rows_before = (a.rows.len(), b.rows.len());
        frame(&mut a);
        frame(&mut b);
        for (s, seg0) in [(&a, seg_before.0), (&b, seg_before.1)] {
            let c = s.np.connected().unwrap();
            if Some(c.seg()) == seg0 && c.delay_known() {
                assert_eq!(
                    s.st.hint_delay(),
                    c.seg_delay(),
                    "t {t}: hint が seg の先頭の D でない"
                );
            }
        }
        for (s, n0) in [(&a, rows_before.0), (&b, rows_before.1)] {
            if s.rows.len() > n0 {
                assert_eq!(s.np.last_timing().delay, cur(s), "t {t}");
            }
        }
        if t == 130 {
            mid = Some((cur(&a), cur(&b), a.np.connected().unwrap().seg_delay()));
        }
        slot += FRAME_US;
    }
    settle(&mut a, &mut b, &clock, 60);
    assert_eq!(mid, Some((2, 2, 1)), "試合の途中で D が替わらない");
    for (who, s) in [("ホスト", &mut a), ("ゲスト", &mut b)] {
        assert_eq!(s.np.phase(), Phase::Lockstep, "{who}: {:?}", s.np.end());
        assert_eq!(s.np.rb_aborts(), 0, "{who}: {:?}", s.np.rb_abort_first());
        assert_eq!(s.np.stats().digit_presses, 1, "{who}: 押下が 1 回でない");
        assert_eq!(cur(s), 1);
        assert_eq!(
            s.np.hud_facts().unwrap().latency,
            Some(1),
            "{who}: 画面の遅れ"
        );
        let c = s.np.connected().unwrap();
        assert!(
            c.seg() >= 2,
            "{who}: 場面を 2 回終えていない（seg {}）",
            c.seg()
        );
    }
    let log = |s: &Side| -> Vec<(u16, u32, u8, u8, bool)> {
        let r = s.np.connected().unwrap().rollback().unwrap().stats();
        r.d_switch_log[..r.d_switches as usize]
            .iter()
            .map(|x| (x.seg, x.frame, x.from, x.to, x.by_peer))
            .collect()
    };
    let (la, lb) = (log(&a), log(&b));
    assert_eq!(
        la.iter().map(|x| (x.0, x.2, x.3, x.4)).collect::<Vec<_>>(),
        vec![(0, 1, 2, false), (0, 2, 1, true)],
        "ホストの予定の記録: {la:?}"
    );
    assert_eq!(
        lb.iter()
            .map(|x| (x.0, x.1, x.2, x.3, !x.4))
            .collect::<Vec<_>>(),
        la.iter()
            .map(|x| (x.0, x.1, x.2, x.3, x.4))
            .collect::<Vec<_>>(),
        "両者の予定のフレームが違う: {lb:?}"
    );
    assert!(la[0].1 < 130 && la[1].1 > 160, "{la:?}");
    let st = crate::net_watch::NetStatus::new();
    st.publish(&a.np);
    let text = st.timing_text();
    for want in [
        format!(
            "seg 0 の取得のフレーム {} から D 1 → 2（自分の押下。埋めた 1）",
            la[0].1
        ),
        format!(
            "seg 0 の取得のフレーム {} から D 2 → 1（相手の押下。捨てる取得 1）",
            la[1].1
        ),
        "数字キーで渡した押下 1（今の値・旗と同じ 0）".to_string(),
        "相手の要求を取った 1".to_string(),
    ] {
        assert!(text.contains(&want), "本ログに「{want}」が無い:\n{text}");
    }
    let st_b = crate::net_watch::NetStatus::new();
    st_b.publish(&b.np);
    let wire_rows = |t: &str| -> Vec<(String, String)> {
        t.lines()
            .filter(|l| l.contains("から D "))
            .map(|l| {
                let (head, f) = l
                    .rsplit_once("ネットワークの F ")
                    .expect("ネットワークの F の欄");
                let seg_d = format!(
                    "{} {}",
                    head.split(" の取得のフレーム").next().unwrap_or(""),
                    head.split("から D ")
                        .nth(1)
                        .and_then(|s| s.split('（').next())
                        .unwrap_or("")
                );
                (seg_d, f.trim().to_string())
            })
            .collect()
    };
    let (wa, wb) = (wire_rows(&text), wire_rows(&st_b.timing_text()));
    assert_eq!(wa.len(), 2, "ホストの予定の行: {wa:?}");
    assert_eq!(wa, wb, "両台の (seg, 前後の D, ネットワークの F) が違う");
    for s in [&a, &b] {
        let r = s.np.connected().unwrap().rollback().unwrap().stats();
        assert_eq!(&r.seg_d_log[..2], &[(0, 1), (1, 1)]);
    }
    let n = a.rows.len().min(b.rows.len());
    assert!(n > 600, "確定の行 {n}");
    assert_eq!(a.rows[..n], b.rows[..n], "両者の確定の列が違う");
    for seg in [0u16, 1] {
        let rows: Vec<&FrameRec> = a.rows.iter().filter(|r| r.seg == seg).collect();
        assert!(rows.len() > 100, "seg {seg} の行 {}", rows.len());
        assert_eq!(
            (rows[0].p1, rows[0].p2),
            (0, 0),
            "seg {seg}: 先頭が穴埋めでない"
        );
        assert!(
            (rows[1].p1, rows[1].p2) != (0, 0),
            "seg {seg}: 2 行目が穴埋めのまま（seg の先頭の D が 1 でない）"
        );
    }
}

#[test]
fn a_scene_boundary_leaves_the_seg_and_the_next_seg_starts_clean() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    let t = (30..120u32)
        .find(|&f| (0..f).all(|g| ref_row(1, g).1 != ref_row(1, f).1))
        .unwrap();
    let x = ref_row(1, t).1;
    a.mem.scene_on_p2 = Some(x);
    b.mem.scene_on_p2 = Some(x);
    run(&mut a, &mut b, &clock, t + 200);
    settle(&mut a, &mut b, &clock, 60);
    for s in [&a, &b] {
        assert_eq!(
            s.np.phase(),
            Phase::Lockstep,
            "{:?} fault {:?} stats {:?} rows {} rb {:?}",
            s.np.end(),
            s.st.fault(),
            s.np.stats(),
            s.rows.len(),
            s.np.connected().unwrap().rollback().map(|r| r.stats())
        );
        let seg0: Vec<&FrameRec> = s.rows.iter().filter(|r| r.seg == 0).collect();
        assert_eq!(
            seg0.len() as u32,
            t + 1,
            "seg 0 の行の数（境目の frame まで）"
        );
        for (i, r) in seg0.iter().enumerate() {
            let (p1, p2) = ref_row(1, i as u32);
            assert_eq!((r.frame, r.p1, r.p2), (i as u32, p1, p2));
        }
        let seg1: Vec<&FrameRec> = s.rows.iter().filter(|r| r.seg == 1).collect();
        assert!(seg1.len() > 50, "seg 1 の行 {}", seg1.len());
        for (i, r) in seg1.iter().enumerate() {
            assert_eq!(r.frame, i as u32, "seg 1 が frame 0 から始まらない");
        }
        assert!(
            s.st.stats().layout_fills >= 2,
            "seg を出たのにコピーの表を埋め直していない: {:?}",
            s.st.stats()
        );
        assert_eq!(s.st.fault(), None);
    }
    let s1 =
        |s: &Side| -> Vec<FrameRec> { s.rows.iter().filter(|r| r.seg == 1).copied().collect() };
    let n = s1(&a).len().min(s1(&b).len());
    assert_eq!(s1(&a)[..n], s1(&b)[..n], "seg 1 の両者の列が違う");
    assert_eq!(a.np.stats().segs_started, b.np.stats().segs_started);
}

#[test]
fn a_fault_ends_the_match_and_stops_touching_the_game() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 30);
    a.mem.mem.insert(ROLLBACK_BLOCKS[0].ptr_at, 0x0F00_0000);
    let mut slot = clock.get();
    for _ in 0..60 {
        if clock.get() < slot {
            clock.set(slot);
        }
        frame(&mut a);
        if a.st.fault().is_some() {
            break;
        }
        frame(&mut b);
        slot += FRAME_US;
    }
    assert_eq!(a.st.fault(), Some(Fault::PlacesMoved));
    assert_eq!(
        a.np.phase(),
        Phase::Ended,
        "見つけた同じフレームでやめていない"
    );
    run(&mut a, &mut b, &clock, 30);
    assert_eq!(a.np.end(), Some(End::Closed(ByeReason::Cancelled)));
    assert_eq!(a.np.stats().rb_aborts, 1);
    assert!(!a.mem.redirect, "やめた後も確保の向け先が開いている");
    let scans = a.mem.scans.len();
    for _ in 0..5 {
        frame(&mut a);
    }
    assert_eq!(a.mem.scans.len(), scans, "やめた後にフックが走査を呼んだ");
    run(&mut a, &mut b, &clock, 30);
    assert_eq!(b.np.phase(), Phase::Ended);
    assert!(
        matches!(b.np.end(), Some(End::PeerBye(ByeReason::Cancelled))),
        "{:?}",
        b.np.end()
    );
}

#[test]
fn on_input_alone_in_a_rollback_connection_ends_the_match() {
    let (mut a, _b, _clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    a.g.begin();
    let act = a.np.on_input(&mut a.g, &mut a.net);
    assert_eq!(act, Action::Local);
    assert_eq!(a.np.phase(), Phase::Ended);
    assert_eq!(a.np.end(), Some(End::Closed(ByeReason::Cancelled)));
    assert_eq!(a.np.stats().rb_aborts, 1);
    assert_eq!(a.g.local_reads, 0, "delay 方式の取得を始めた");
}

#[test]
fn a_fault_found_at_the_scan_point_ends_the_match_before_scanning() {
    use higanr_core::rollback::Window;
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 10);
    crate::rollback_play::GameWindow {
        mem: &mut a.mem,
        st: &mut a.st,
    }
    .restore(999);
    assert_eq!(a.st.fault(), Some(Fault::NoSnapshot { want: 999 }));
    let scans = a.mem.scans.len();
    let r = frame(&mut a);
    assert_eq!(r, RbScan::Pass, "食い違いの入口で走査を呼んだ");
    assert_eq!(a.mem.scans.len(), scans);
    assert_eq!(a.np.end(), Some(End::Closed(ByeReason::Cancelled)));
    assert_eq!(a.np.stats().rb_aborts, 1);
    assert!(!a.mem.redirect);
}

#[test]
fn a_new_connection_clears_the_previous_connections_fault() {
    use higanr_core::rollback::Window;
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    crate::rollback_play::GameWindow {
        mem: &mut a.mem,
        st: &mut a.st,
    }
    .restore(999);
    assert_eq!(a.st.fault(), Some(Fault::NoSnapshot { want: 999 }));
    run(&mut a, &mut b, &clock, 30);
    assert_eq!(a.st.fault(), None, "前の接続の印が残った");
    assert_eq!(a.np.phase(), Phase::Lockstep, "{:?}", a.np.end());
    assert_eq!(a.np.stats().rb_aborts, 0);
    assert!(a.rows.len() > 10, "試合が進んでいない {}", a.rows.len());
    crate::rollback_play::GameWindow {
        mem: &mut a.mem,
        st: &mut a.st,
    }
    .restore(999);
    let r = frame(&mut a);
    assert_eq!(r, RbScan::Pass);
    assert_eq!(a.np.stats().rb_aborts, 1, "途中の食い違いを消した");
}

#[test]
fn delay_mode_connections_never_touch_the_game_through_the_rollback_hooks() {
    let (mut a, mut b, _clock) = pair(
        ModeWish::rollback(1),
        ModeWish::DELAY,
        CheckSource::InputDigest,
        12,
        2,
        LatPlan::default(),
    );
    for s in [&mut a, &mut b] {
        assert_eq!(
            s.np.agreed_mode().map(|m| m.0),
            Some(higanr_core::wire::Mode::Delay)
        );
        assert!(!s.np.rollback_ready());
        for _ in 0..20 {
            s.g.begin();
            let r =
                s.np.rollback_scan(&mut s.g, &mut s.net, &mut s.st, &mut s.mem, &mut |_, _| {
                    panic!("delay 方式で記録を流した")
                });
            assert_eq!(r, RbScan::Pass);
            assert!(
                s.np.rollback_post_present(&mut s.net, &mut s.st, &mut s.mem, &mut |_, _| {
                    panic!("delay 方式で記録を流した")
                })
                .is_none()
            );
        }
        assert_eq!(
            (
                s.mem.scans.len(),
                s.mem.redirect_on_calls,
                s.mem.watch_on_calls,
                s.mem.copy_outs,
                s.mem.copy_ins
            ),
            (0, 0, 0, 0, 0),
            "delay 方式でフックがゲームに触った"
        );
    }
}

#[test]
fn a_stalled_side_waits_for_words_within_the_budget_and_still_agrees() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 2, 8);
    run(&mut a, &mut b, &clock, 120);
    settle(&mut a, &mut b, &clock, 120);
    for s in [&a, &b] {
        assert!(s.net.waits > 0, "止まっている間に待っていない");
        assert!(s.np.stats().stalls > 0, "{:?}", s.np.stats());
        assert!(s.np.stats().wakes > 0);
        assert_eq!(s.np.phase(), Phase::Lockstep, "{:?}", s.np.end());
    }
    let n = a.rows.len().min(b.rows.len());
    assert!(n > 30, "進んだ行 {n}");
    assert_eq!(a.rows[..n], b.rows[..n]);
    for (i, r) in a.rows.iter().enumerate() {
        let (p1, p2) = ref_row(1, i as u32);
        assert_eq!((r.frame, r.p1, r.p2), (i as u32, p1, p2));
    }
}

/// つなぎ直し: 実行の間ずっと生きるコピーの担当（`PlayState`）とゲームのメモリを、終わった 1 本目の接続から新しい 2 本目へそのまま渡したとき、
/// 1 本目の後に確保の向け先が閉じ、2 本目が frame 0 から確定して、コピーの担当の数えが 2 本目で 0 に戻らない。
#[test]
fn a_second_connection_reuses_the_play_state_left_by_the_first() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 120);
    settle(&mut a, &mut b, &clock, 60);
    assert_eq!(
        (a.rows.len(), b.rows.len()),
        (120, 120),
        "検査の前提: 1 本目"
    );
    a.np.close(ByeReason::Cancelled);
    run(&mut a, &mut b, &clock, 30);
    for s in [&a, &b] {
        assert_eq!(s.np.phase(), Phase::Ended, "{:?}", s.np.end());
        assert!(
            !s.mem.redirect,
            "接続が終わった後も確保の向け先が開いたまま"
        );
    }
    let resim1 = a.st.stats().resim_confirmed_ticks;
    // 2 本目: ネットワークとセッションは新しく、コピーの担当とゲームのメモリは 1 本目のものを続けて使う（本物では静的）。
    let (mut a2, mut b2, clock2) = rollback_pair(CheckSource::InputDigest, 12, 3);
    core::mem::swap(&mut a2.st, &mut a.st);
    core::mem::swap(&mut a2.mem, &mut a.mem);
    core::mem::swap(&mut b2.st, &mut b.st);
    core::mem::swap(&mut b2.mem, &mut b.mem);
    let start = (a2.mem.get(STATE), b2.mem.get(STATE));
    run(&mut a2, &mut b2, &clock2, 120);
    settle(&mut a2, &mut b2, &clock2, 60);
    let want: Vec<(u16, u16)> = (0..120).map(|f| ref_row(1, f)).collect();
    for (s, from) in [(&a2, start.0), (&b2, start.1)] {
        assert_eq!(s.np.phase(), Phase::Lockstep, "{:?}", s.np.end());
        assert_eq!(s.st.fault(), None);
        assert_eq!(s.rows.len(), 120, "2 本目の確定した行の数");
        for (i, r) in s.rows.iter().enumerate() {
            assert_eq!(
                (r.seg, r.frame, (r.p1, r.p2)),
                (0, i as u32, want[i]),
                "2 本目の行 {i} が参照と違う"
            );
        }
        let chained = want.iter().fold(from, |st, &(p1, p2)| {
            st.wrapping_mul(31)
                .wrapping_add(u32::from(p1) * 7 + u32::from(p2) + 1)
        });
        assert_eq!(
            s.mem.get(STATE),
            chained,
            "2 本目の世界の状態が、2 本目の確定の行の鎖と違う"
        );
    }
    assert_eq!(a2.rows, b2.rows);
    assert!(
        a2.st.stats().resim_confirmed_ticks > resim1,
        "検査の前提: 2 本目でも進め直した（数えは実行の合計）"
    );
}

#[test]
fn head_wait_never_waits_in_the_rollback_mode() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    for s in [&mut a, &mut b] {
        s.np.cfg.head_wait_cap_us = HEAD_WAIT_CAP_US;
    }
    let mut slot = clock.get();
    let mut calls = 0u32;
    for _ in 0..120 {
        if clock.get() < slot {
            clock.set(slot);
        }
        for s in [&mut a, &mut b] {
            let t0 = clock.get();
            assert_eq!(s.np.head_wait(&mut s.net), HeadWait::Off);
            assert_eq!(clock.get(), t0, "rollback の先頭で時計が進んだ");
            calls += 1;
            frame(s);
        }
        slot += FRAME_US;
    }
    settle(&mut a, &mut b, &clock, 60);
    for s in [&a, &b] {
        assert_eq!(s.np.phase(), Phase::Lockstep);
        assert!(s.np.connected().unwrap().rollback().is_some());
        assert_eq!(s.np.stats().head_waits, 0);
        assert_eq!(s.rows.len(), 120, "確定した行の数");
    }
    assert_eq!(calls, 240);
    assert_eq!(a.rows, b.rows);
}

#[test]
fn the_scan_hook_hands_the_glide_request_to_the_grid_after_three_pongs() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 0);
    let mut slot = clock.get();
    let (mut early, mut a_max, mut b_min) = (0i32, 0i32, 0i32);
    for k in 0..600u32 {
        if clock.get() < slot {
            clock.set(slot);
        }
        frame(&mut a);
        if k % 4 != 3 {
            frame(&mut b);
        }
        if k < 120 {
            early = early
                .max(a.np.rb_glide_last().abs())
                .max(b.np.rb_glide_last().abs());
        }
        a_max = a_max.max(a.np.rb_glide_last());
        b_min = b_min.min(b.np.rb_glide_last());
        slot += FRAME_US;
    }
    assert_eq!(early, 0, "往復が 3 回測れる前に寄せ幅を出した");
    let ma = a.np.connected().unwrap().liveness().rtt_median_us();
    assert!(
        ma.is_some(),
        "往復の中央値が出ていない（検査がゲートを通っていない）"
    );
    assert!(a_max > 0, "先にいる A が伸ばす要求を出さない {a_max}");
    assert!(b_min < 0, "遅れる B が縮める要求を出さない {b_min}");
    let max = higanr_core::rollback::GlideParams::DEFAULT.max_ppm;
    assert!(a_max <= max && -b_min <= max);
}

#[test]
fn a_check_mismatch_in_a_rollback_match_stops_both_sides_and_esc_returns_to_local() {
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 20);
    a.np.digest ^= 1;
    for k in 0..400 {
        run(&mut a, &mut b, &clock, 1);
        if a.np.is_stopped() && b.np.is_stopped() {
            break;
        }
        assert!(k < 399, "止まらない: {:?} / {:?}", a.np.end(), b.np.end());
    }
    let found = [&a, &b]
        .iter()
        .filter(|s| matches!(s.np.end(), Some(End::Desync { .. })))
        .count();
    assert!(found >= 1, "{:?} / {:?}", a.np.end(), b.np.end());
    for s in [&a, &b] {
        assert_eq!(s.np.ui_conn(0).0, Conn::Desynced);
        assert!(!s.mem.redirect, "止めた後も確保の向け先が開いている");
    }
    let before = |s: &Side| (s.mem.scans.len(), s.mem.get(STATE), s.rows.len());
    let (ba, bb) = (before(&a), before(&b));
    let (ra, rb) = (a.g.render_only, b.g.render_only);
    run(&mut a, &mut b, &clock, 60);
    assert_eq!((before(&a), before(&b)), (ba, bb), "止めた後に試合が動いた");
    assert_eq!((a.g.render_only - ra, b.g.render_only - rb), (60, 60));
    a.g.esc_down = false;
    run(&mut a, &mut b, &clock, 2);
    assert!(a.np.is_stopped());
    a.g.esc_down = true;
    let local0 = a.g.local;
    run(&mut a, &mut b, &clock, 3);
    assert!(!a.np.is_stopped());
    assert_eq!(a.g.local - local0, 3, "戻った後の返しが Local でない");
    assert_eq!(a.mem.scans.len(), ba.0, "戻った後にフックが走査を呼んだ");
    assert!(b.np.is_stopped());
}

#[test]
fn a_rollback_host_pushes_the_same_rollback_number_as_its_status_line() {
    use crate::spectate_testkit::fold;
    use higanr_core::wire::{STATS_PING_UNKNOWN, STATS_ROLLBACK_NONE};
    let mut nonzero_runs = 0;
    for (one_way, drop_every) in [(1u64, 0u32), (3, 0), (6, 0), (2, 5), (3, 7)] {
        let ctx = format!("片道 {one_way} フレーム・{drop_every} 通に 1 通落とす");
        let ring = Arc::new(crate::spectate_ring::SpectateRing::with_len(1 << 16));
        let (mut a, mut b, clock) = pair_with_ring(
            ModeWish::rollback(1),
            ModeWish::rollback(1),
            CheckSource::InputDigest,
            12,
            one_way,
            LatPlan::default(),
            Some(Arc::clone(&ring)),
        );
        a.hud_log = Some(Vec::new());
        a.net.drop_every = drop_every;
        b.net.drop_every = drop_every;
        run(&mut a, &mut b, &clock, 400);
        let recs = drain_ring(&ring);
        let f = fold(&recs);
        let log = a.hud_log.as_ref().unwrap();
        assert_eq!(log.len(), 400, "{ctx}: 毎フレーム呼んでいる");

        let mut want: Vec<(u16, u8, u8)> = Vec::new();
        let mut inside: Vec<u32> = Vec::new();
        for h in log {
            match h {
                Some(h) => {
                    inside.push(h.rewound.expect("rollback の試合は戻しを持つ"));
                    if (inside.len() - 1).is_multiple_of(60) {
                        let from = inside.len().saturating_sub(60);
                        let max = inside[from..].iter().copied().max().unwrap().min(254) as u8;
                        let ping = h
                            .rtt_ms
                            .map_or(STATS_PING_UNKNOWN, |p| p.min(65_534) as u16);
                        want.push((ping, h.latency.unwrap(), max));
                    }
                }
                None => inside.clear(),
            }
        }
        assert!(want.len() >= 5, "{ctx}: 期待が少なすぎる {}", want.len());
        assert_eq!(
            f.stats, want,
            "{ctx}: Stats が hud_facts の値・窓の式と違う"
        );
        assert!(
            f.stats.iter().all(|s| s.2 != STATS_ROLLBACK_NONE),
            "{ctx}: rollback の試合に delay 方式の印"
        );
        assert!(
            f.stats.iter().any(|s| s.0 != STATS_PING_UNKNOWN),
            "{ctx}: ping"
        );
        assert!(
            f.stats.iter().all(|s| s.1 == 1),
            "{ctx}: D が 1 でない {:?}",
            f.stats
        );
        if f.stats.iter().any(|s| s.2 > 0) {
            nonzero_runs += 1;
        }
        let rows: Vec<(u16, u32, u16, u16)> = a
            .rows
            .iter()
            .map(|r| (r.seg, r.frame, r.p1, r.p2))
            .collect();
        assert_eq!(f.frames, rows, "{ctx}: 試合の列");
    }
    assert!(nonzero_runs >= 2, "戻しが起きた実行が {nonzero_runs}");
}

#[test]
fn digit_keys_set_d_in_a_rollback_match_for_both_sides() {
    const K: fn(u8) -> u16 = |d| (1 << d) | crate::digit_keys::CTRL_BIT;
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
        s.np.ensure_pick();
    }
    let cur = |s: &Side| s.np.connected().unwrap().cur_delay();
    let mut seen: Vec<(&str, u8, u8)> = Vec::new();
    let mut rec = None;
    let mut slot = clock.get();
    for t in 0..700u32 {
        let (mut ha, mut hb) = (Some(0u16), Some(0u16));
        match t {
            60..=62 => ha = Some(K(5)),
            160..=162 => hb = Some(K(8)),
            260..=262 => {
                ha = Some(K(9));
                rec.get_or_insert_with(|| a.np.connected().unwrap().recommend_now());
            }
            360..=362 => hb = Some(K(0)),
            450..=454 => ha = None,
            455..=470 => ha = Some(K(2)),
            560..=562 => ha = Some(K(2)),
            _ => {}
        }
        a.g.digit_held = ha;
        b.g.digit_held = hb;
        if clock.get() < slot {
            clock.set(slot);
        }
        frame(&mut a);
        frame(&mut b);
        match t {
            150 => seen.push(("ホストの 5", cur(&a), cur(&b))),
            250 => seen.push(("ゲストの 8（要求）", cur(&a), cur(&b))),
            350 => seen.push(("ホストの auto = 推奨値", cur(&a), cur(&b))),
            440 => seen.push(("ゲストの 0（旗が降りる）", cur(&a), cur(&b))),
            520 => seen.push(("読めない間をまたぐ押しっぱなし", cur(&a), cur(&b))),
            _ => {}
        }
        slot += FRAME_US;
    }
    settle(&mut a, &mut b, &clock, 60);
    let rec = rec.flatten().expect("推奨値が測れている（検査の前提）");
    assert!(
        ![0u8, 5, 8, 2].contains(&rec),
        "検査の前提: 推奨値が他の値と区別できる（{rec}）"
    );
    assert_eq!(
        seen,
        vec![
            ("ホストの 5", 5, 5),
            ("ゲストの 8（要求）", 8, 8),
            ("ホストの auto = 推奨値", rec, rec),
            ("ゲストの 0（旗が降りる）", 0, 0),
            ("読めない間をまたぐ押しっぱなし", 0, 0),
        ]
    );
    for (who, s) in [("ホスト", &mut a), ("ゲスト", &mut b)] {
        assert_eq!(s.np.phase(), Phase::Lockstep, "{who}: {:?}", s.np.end());
        assert_eq!(s.np.rb_aborts(), 0, "{who}: {:?}", s.np.rb_abort_first());
    }
    assert_eq!((cur(&a), cur(&b)), (2, 2), "最後の押し直しの 2");
    assert!(
        !a.np.connected().unwrap().auto_on(),
        "ゲストの 0 でホストの旗が降りていない"
    );
    let (sa, sb) = (a.np.stats(), b.np.stats());
    assert_eq!(
        (sa.digit_presses, sb.digit_presses),
        (3, 2),
        "押下の数（ホスト 5・auto・2、ゲスト 8・0）"
    );
    assert_eq!((sa.digit_unchanged, sb.digit_unchanged), (0, 0));
    let n = a.rows.len().min(b.rows.len());
    assert!(n > 600, "確定の行 {n}");
    assert_eq!(a.rows[..n], b.rows[..n], "両者の確定の列が違う");
}

#[test]
fn the_guest_follows_the_hosts_auto_flag_in_a_rollback_match_without_flicker() {
    use higanr_core::delay_pick::AUTO;
    use higanr_core::lockstep::Role;
    let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, 3);
    run(&mut a, &mut b, &clock, 120);
    let host_is_a = a.np.connected().unwrap().role() == Role::Host;
    let frames = |a: &mut Side, b: &mut Side, clock: &Cell<u64>, n: u32| {
        let mut v = Vec::new();
        for _ in 0..n {
            run(a, b, clock, 1);
            let (ha, hb) = (a.np.hud_facts().unwrap(), b.np.hud_facts().unwrap());
            let (h, g) = if host_is_a { (ha, hb) } else { (hb, ha) };
            assert_eq!((h.format, g.format), (None, None), "試合中の行");
            v.push((h.auto, g.auto, h.latency.unwrap(), g.latency.unwrap()));
        }
        v
    };
    let press = |a: &mut Side, b: &mut Side, host: bool, x: u8| {
        let s = if host == host_is_a { a } else { b };
        s.np.link.connected_mut().unwrap().press_delay(x);
    };
    let first = frames(&mut a, &mut b, &clock, 1);
    assert_eq!(
        (first[0].0, first[0].1),
        (false, false),
        "最初は旗が降りている"
    );
    press(&mut a, &mut b, true, AUTO);
    let v = frames(&mut a, &mut b, &clock, 60);
    assert!(v[0].0, "ホストの旗が押した瞬間に立たない");
    let up = v.iter().position(|o| o.1).expect("ゲストの (A) が出ない");
    assert!(up <= 30, "ゲストの (A) が遅い: {up} フレーム");
    assert!(v[up..].iter().all(|o| o.0 && o.1), "旗がちらついた: {v:?}");
    press(&mut a, &mut b, true, 4);
    let v = frames(&mut a, &mut b, &clock, 90);
    let down = v.iter().position(|o| !o.1).expect("ゲストの旗が降りない");
    assert!(
        v[down..].iter().all(|o| !o.0 && !o.1),
        "旗がちらついた: {v:?}"
    );
    let end = v.last().unwrap();
    assert_eq!((end.2, end.3), (4, 4), "遅れが揃わない: {v:?}");
    press(&mut a, &mut b, false, AUTO);
    let v = frames(&mut a, &mut b, &clock, 120);
    assert!(
        v.iter().all(|o| o.1),
        "ゲスト自身の auto が途中で降りた: {v:?}"
    );
    let host_up = v.iter().position(|o| o.0).expect("ホストの旗が立たない");
    assert!(
        v[host_up..].iter().all(|o| o.0),
        "ホストの旗がちらついた: {v:?}"
    );
    let end = v.last().unwrap();
    assert_eq!(end.2, end.3, "auto の後に両者の遅れが揃わない: {v:?}");
    press(&mut a, &mut b, false, 1);
    let v = frames(&mut a, &mut b, &clock, 120);
    assert!(
        v.iter().all(|o| !o.1),
        "ゲスト自身の数字の後に旗が戻った: {v:?}"
    );
    let host_down = v.iter().position(|o| !o.0).expect("ホストの旗が降りない");
    assert!(
        v[host_down..].iter().all(|o| !o.0),
        "ホストの旗がちらついた: {v:?}"
    );
    let end = v.last().unwrap();
    assert_eq!(
        (end.2, end.3),
        (1, 1),
        "ゲストの数字が両者に揃わない: {v:?}"
    );
}

/// 往復がほぼ 0 の回線（片道 0 フレーム）で測れた窓の最大が小さいとき auto は D 0 を選び、片道 3 フレームの回線では 1 を選ぶ（両者へ揃う）。
#[test]
fn auto_picks_d_zero_only_on_a_fast_link() {
    use higanr_core::delay_pick::AUTO;
    use higanr_core::lockstep::Role;
    for (one_way, want) in [(0u64, 0u8), (3, 1)] {
        let (mut a, mut b, clock) = rollback_pair(CheckSource::InputDigest, 12, one_way);
        run(&mut a, &mut b, &clock, 300);
        let host_is_a = a.np.connected().unwrap().role() == Role::Host;
        let (h, g) = if host_is_a {
            (&mut a, &mut b)
        } else {
            (&mut b, &mut a)
        };
        let c = h.np.connected().unwrap();
        let max = c.liveness().rtt_max_ms();
        assert!(max.is_some(), "往復の窓が 3 個に満たない（検査の前提）");
        assert_eq!(c.recommend_now(), Some(want), "one_way {one_way}: {max:?}");
        h.np.link.connected_mut().unwrap().press_delay(AUTO);
        let (ha, hb) = (h, g);
        let mut slot = clock.get();
        for _ in 0..60 {
            if clock.get() < slot {
                clock.set(slot);
            }
            frame(ha);
            frame(hb);
            slot += FRAME_US;
        }
        for s in [&*ha, &*hb] {
            assert_eq!(
                s.np.connected().unwrap().cur_delay(),
                want,
                "one_way {one_way}: auto の D"
            );
        }
        assert_eq!(
            ha.np.hud_facts().unwrap().recommend,
            Some(want),
            "画面の推奨値も同じ"
        );
    }
}

#[test]
fn a_pair_that_connected_in_delay_runs_rollback_after_the_format_switch() {
    let (mut a, mut b, clock) = pair(
        ModeWish::DELAY,
        ModeWish::DELAY,
        CheckSource::InputDigest,
        12,
        3,
        LatPlan::default(),
    );
    for s in [&a, &b] {
        assert_eq!(s.np.connected().unwrap().mode(), Mode::Delay);
        assert!(s.np.connected().unwrap().rollback_able());
        assert!(!s.np.rollback_ready(), "替える前に rollback を動かす");
    }
    let v = higanr_core::wire::FormatValue {
        mode: Mode::Rollback,
        delay: 2,
        auto: false,
    };
    for s in [&mut a, &mut b] {
        let c = s.np.link.connected_mut().unwrap();
        assert!(matches!(
            c.switch_format(v, false),
            higanr_core::link::FormatSwitch::Changed { .. }
        ));
        assert!(s.np.rollback_ready(), "替えた後にフックが動かない");
    }
    run(&mut a, &mut b, &clock, 240);
    settle(&mut a, &mut b, &clock, 60);
    for s in [&a, &b] {
        assert_eq!(s.np.phase(), Phase::Lockstep, "{:?}", s.np.end());
        assert_eq!(s.rows.len(), 240, "確定した行の数");
        for (i, r) in s.rows.iter().enumerate() {
            let (p1, p2) = ref_row(2, i as u32);
            assert_eq!(
                (r.seg, r.frame, r.p1, r.p2),
                (0, i as u32, p1, p2),
                "行 {i} が D 2 の参照と違う"
            );
        }
        let ps = s.st.stats();
        assert!(
            ps.resim_confirmed_ticks > 50 && ps.unconfirmed_ticks > 50,
            "予測と進め直しが起きていない: {ps:?}"
        );
        assert_eq!(s.np.stats().rb_aborts, 0);
    }
    assert_eq!(a.rows, b.rows, "両者の確定の列が違う");
}
