//! 入力の地点の判断の検査（pure。仮の時計と仮のネットワークで、偽のゲーム 2 つを繋ぐ）。
//!
//! 両者の (seg, frame, 1P, 2P) の列が揃うこと（席・seg・読み込みの長さ・再戦の切替の tick に依らない）、列が偽のゲームが読んだ語の順から
//! 手で組んだ参照と合うこと、描画だけのフレームの後にローカル入力を読み直さないこと、止める地点で相手の SELECT を待つこと、CHECK の
//! ズレを見逃さないこと、`on_input` が確保しないことを見る。
use std::net::SocketAddr;

use higanr_core::handshake::{self, Name};
use higanr_core::link;
use higanr_core::wire::{ByeReason, Seat};

use super::fake::{FRAME_US, FakeGame, FakeNet, local_word};
use super::*;

const SID: u32 = 0x5EED_2001;
const SEED_A: u64 = 0xA;
const SEED_B: u64 = 0xB;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

struct Side {
    np: NetPlay,
    g: FakeGame,
    net: FakeNet,
    closed: bool,
}

/// A（ホスト。席は 2P —— 役割と席をわざと入れ違いにする。希望 latency 2）と B（ゲスト、1P、希望 3）。
/// 始めの delay は 3。
fn pair(
    check: CheckSource,
    games: (FakeGame, FakeGame),
) -> (Side, Side, std::rc::Rc<std::cell::Cell<u64>>) {
    pair_lat(check, games, 2, 3)
}

/// `pair` の希望 latency を選べる形（A は `lat_a`、B は `lat_b`。始めの delay は大きい方）。
fn pair_lat(
    check: CheckSource,
    games: (FakeGame, FakeGame),
    lat_a: u8,
    lat_b: u8,
) -> (Side, Side, std::rc::Rc<std::cell::Cell<u64>>) {
    let (mut na, mut nb, clock) = FakeNet::pair(addr(1), addr(2), 2_000);
    let cfg = Config {
        check,
        wait_budget_us: WAIT_BUDGET_US,
        ..Config::default()
    };
    let lc = |peer: u16, nonce: u64, latency: u8, name: &str| link::Config {
        handshake: handshake::Config {
            session_id: SID,
            role_nonce: nonce,
            latency,
            name: Name::new(name),
            host_seat: Seat::P2,
        },
        secret: peer_sim::cli::DEFAULT_SECRET,
        peers: higanr_core::candidate::Candidates::from_addrs(&[addr(peer)]).unwrap(),
        key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
    };
    let npa = NetPlay::new(lc(2, 0xF000, lat_a, "a"), cfg, &mut na);
    let npb = NetPlay::new(lc(1, 0x0001, lat_b, "b"), cfg, &mut nb);
    let (ga, gb) = games;
    (
        Side {
            np: npa,
            g: ga,
            net: na,
            closed: false,
        },
        Side {
            np: npb,
            g: gb,
            net: nb,
            closed: false,
        },
        clock,
    )
}

fn run(a: &mut Side, b: &mut Side, clock: &std::cell::Cell<u64>, max_ticks: u32) {
    let mut slot = clock.get();
    let mut after_end = 0;
    for _ in 0..max_ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        for s in [&mut *a, &mut *b] {
            s.g.do_frame(&mut s.np, &mut s.net);
            if s.g.done && !s.closed {
                s.np.close(ByeReason::Finished);
                s.closed = true;
            }
        }
        slot += FRAME_US;
        if a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            after_end += 1;
            if after_end > 30 {
                return;
            }
        }
    }
    panic!(
        "{max_ticks} tick で終わらない: A {:?} {:?} / B {:?} {:?}",
        a.np.phase(),
        a.np.end(),
        b.np.phase(),
        b.np.end()
    );
}

/// 手で組み立てた参照: 席 `seat` の人（seed、希望の順に数えた語）の列。seg ごとに `frames` フレームを使い、
/// 各 seg の始まり `delay` フレームは穴埋めの 0、それ以後のフレーム f は、その seg の取得 f − delay の語。
/// 取得は 1 フレームに 1 回で、seg の終わりの `delay` 回分（使われない先の語）も読んでいる。
fn expected(seed: u64, segs: u32, frames: u32, delay: u32) -> Vec<u16> {
    let mut v = Vec::new();
    for s in 0..segs {
        for f in 0..frames {
            v.push(if f < delay {
                0
            } else {
                local_word(seed, u64::from(s * frames + f - delay + 1))
            });
        }
    }
    v
}

fn games(frames: u32, segs: u32, sessions: u32) -> (FakeGame, FakeGame) {
    let mut ga = FakeGame::new(SEED_A, frames, segs, sessions);
    let mut gb = FakeGame::new(SEED_B, frames, segs, sessions);
    ga.menu_before_decide = 20;
    gb.menu_before_decide = 200;
    ga.load_first = 58;
    gb.load_first = 3;
    (ga, gb)
}

#[test]
fn the_log_line_carries_the_input_counts_in_delay_mode() {
    const F: u32 = 60;
    let (mut a, mut b, clock) = pair(CheckSource::Game, games(F, 1, 1));
    run(&mut a, &mut b, &clock, 20_000);
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
fn a_pair_plays_the_same_frames_through_rematch_and_back_to_menu() {
    const F: u32 = 90;
    let (mut a, mut b, clock) = pair(CheckSource::Game, games(F, 2, 2));
    b.net.delay_us = 30_000;
    run(&mut a, &mut b, &clock, 20_000);

    assert_eq!(a.g.recs, b.g.recs, "両者の列が違う");
    let segs: Vec<(u16, u32)> = a.g.recs.iter().map(|r| (r.seg, r.frame)).collect();
    let want: Vec<(u16, u32)> = (0..4u16)
        .flat_map(|s| (0..F).map(move |f| (s, f)))
        .collect();
    assert_eq!(segs, want, "seg とフレームの並びが違う");
    let d = a.np.connected().map(|c| c.lockstep().delay()).unwrap();
    assert_eq!(d, 3);
    // 決まったモードは delay 方式（この対は `Config::default()` の望むモード = delay 方式で作る。rollback の対は `netplay/rollback.rs` の検査）。
    for s in [&a, &b] {
        assert_eq!(s.np.agreed_mode(), Some((Mode::Delay, 3)));
    }
    let p1: Vec<u16> = a.g.recs.iter().map(|r| r.p1).collect();
    let p2: Vec<u16> = a.g.recs.iter().map(|r| r.p2).collect();
    assert_eq!(p1, expected(SEED_B, 4, F, 3), "1P（B）の列が参照と違う");
    assert_eq!(p2, expected(SEED_A, 4, F, 3), "2P（A）の列が参照と違う");
    for s in [&a, &b] {
        assert_eq!(s.g.local_reads, s.np.stats().local_reads);
        assert_eq!(s.g.local_reads, u64::from(4 * F), "読んだ回数");
        assert_eq!(s.g.match_starts, 4);
        assert_eq!(s.np.stats().segs_started, 4);
        assert_eq!(s.g.agreed.len(), 2);
        assert_eq!(s.g.charges.len(), 4);
        let c = s.np.connected().unwrap();
        assert!(
            c.counts().check_matches >= 4 * 3,
            "CHECK の一致 {}",
            c.counts().check_matches
        );
    }
    assert_eq!((a.g.seats_seen, b.g.seats_seen), (0b10, 0b01));
    let (sa, sb) = (a.g.select_chara, b.g.select_chara);
    assert_eq!(a.g.agreed[0].0, Seat::P1);
    assert_eq!(a.g.agreed[0].1.peer.chara, sb);
    assert_eq!(b.g.agreed[0].1.peer.chara, sa);
    assert_eq!(
        a.g.charges[0],
        (sb % 3, sa % 3),
        "A（2P）のチャージ型の並び"
    );
    assert_eq!(
        b.g.charges[0],
        (sb % 3, sa % 3),
        "B（1P）のチャージ型の並び"
    );
    assert!(a.np.stats().hold_frames > 100, "{:?}", a.np.stats());
    assert!(b.np.stats().stalls > 20, "{:?}", b.np.stats());
    // リプレイの印の通信の要約: delay 方式は止まった数（描画だけにしたフレーム）をそのまま、進め直しは 0 で渡す。
    let v = crate::replay_save::NetView::of(&b.np).expect("つながっていた接続");
    assert_eq!(v.totals.stalls, b.np.stats().stalls);
    assert_eq!((v.totals.rewinds, v.totals.resim_ticks), (0, 0));
    assert_eq!((v.match_no, v.rollback, v.rb_ready), (4, false, false));
    let ends = [a.np.end(), b.np.end()];
    assert!(
        ends.iter().all(|e| matches!(
            e,
            Some(End::Closed(ByeReason::Finished)) | Some(End::PeerBye(ByeReason::Finished))
        )),
        "{ends:?}"
    );
}

#[test]
fn losses_do_not_change_the_frames() {
    const F: u32 = 60;
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 2, 1));
    run(&mut a, &mut b, &clock, 20_000);
    let clean = a.g.recs.clone();
    assert_eq!(clean.len(), (2 * F) as usize);

    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 2, 1));
    a.net.drop_every = 5;
    b.net.drop_every = 7;
    run(&mut a, &mut b, &clock, 40_000);
    assert!(
        a.net.dropped > 20 && b.net.dropped > 20,
        "落としが効いていない"
    );
    assert_eq!(a.g.recs, b.g.recs);
    assert_eq!(a.g.recs, clean, "ロスで列が変わった");
    for s in [&a, &b] {
        assert_eq!(s.g.local_reads, u64::from(2 * F));
    }
}

/// 使ったフレームごとに、seg の始まりの穴埋め（その seg に入ったときの delay の数）かどうか。`timings` の delay は使った
/// 後の値なので、seg に入ったときの delay は前の seg の最後のフレームの値（最初の seg は `first`）。
fn fill_frames(recs: &[FrameRec], timings: &[FrameTiming], first: u8) -> Vec<bool> {
    let mut d0 = first;
    let mut prev: Option<(u16, u8)> = None;
    recs.iter()
        .zip(timings)
        .map(|(r, t)| {
            if let Some((seg, d)) = prev
                && seg != r.seg
            {
                d0 = d;
            }
            prev = Some((r.seg, t.delay));
            r.frame < u32::from(d0)
        })
        .collect()
}

fn delay_changes(timings: &[FrameTiming], first: u8) -> Vec<(usize, u8)> {
    let mut d = first;
    let mut out = Vec::new();
    for (i, t) in timings.iter().enumerate() {
        if t.delay != d {
            out.push((i, t.delay));
            d = t.delay;
        }
    }
    out
}

#[test]
fn the_host_plan_switches_latency_and_both_sides_agree() {
    const F: u32 = 200;
    for drop in [0u32, 7] {
        let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 2, 1));
        a.np.cfg.lat_plan = LatPlan::new(&[(50, 6), (120, 1), (260, 4), (330, 2)]).unwrap();
        b.np.cfg.lat_plan = LatPlan::new(&[(10, 7)]).unwrap();
        a.net.drop_every = drop;
        b.net.drop_every = drop;
        run(&mut a, &mut b, &clock, 40_000);
        let ctx = format!("drop_every {drop}");
        assert_eq!(a.g.recs, b.g.recs, "{ctx}: 両者の列が違う");
        let want: Vec<(u16, u32)> = (0..2u16)
            .flat_map(|s| (0..F).map(move |f| (s, f)))
            .collect();
        let got: Vec<(u16, u32)> = a.g.recs.iter().map(|r| (r.seg, r.frame)).collect();
        assert_eq!(got, want, "{ctx}: seg とフレームの並び");
        let da: Vec<u8> = a.g.timings.iter().map(|t| t.delay).collect();
        let db: Vec<u8> = b.g.timings.iter().map(|t| t.delay).collect();
        assert_eq!(da, db, "{ctx}: 両者の delay の列が違う");
        let ch = delay_changes(&a.g.timings, 3);
        assert_eq!(
            ch.iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![6, 1, 4, 2],
            "{ctx}: {ch:?}"
        );
        assert_eq!(ch[0].0, 53, "{ctx}: {ch:?}");
        let (sa, sb) = (a.np.stats(), b.np.stats());
        assert_eq!((sa.lat_requested, sa.lat_refused), (4, 0), "{ctx}: {sa:?}");
        assert_eq!((sb.lat_requested, sb.lat_refused), (0, 1), "{ctx}: {sb:?}");
        assert_eq!((sa.switches, sb.switches), (4, 4), "{ctx}");
        for (name, s) in [("A", &a), ("B", &b)] {
            let fill = fill_frames(&s.g.recs, &s.g.timings, 3);
            for ((r, t), f) in s.g.recs.iter().zip(&s.g.timings).zip(&fill) {
                assert_eq!(
                    t.margin_us.is_none(),
                    *f,
                    "{ctx} {name}: seg {} frame {} の余裕 {:?}（穴埋めか {f}）",
                    r.seg,
                    r.frame,
                    t.margin_us
                );
            }
            let fills = fill.iter().filter(|x| **x).count() as u64;
            assert_eq!(s.np.stats().margin_unknown, fills, "{ctx} {name}");
            assert!(
                s.g.timings
                    .iter()
                    .any(|t| t.margin_us.is_some_and(|m| m > 0)),
                "{ctx} {name}"
            );
            let st = s.np.stats();
            assert_eq!(
                st.wait_over_hist[0], st.wait_timeouts,
                "{ctx} {name}: {st:?}"
            );
            assert_eq!(st.wait_over_max_us, 0);
        }
        assert!(b.np.stats().wait_timeouts > 0, "{ctx}: {:?}", b.np.stats());
        assert!(b.g.timings.iter().any(|t| t.wait_us > 0), "{ctx}");
    }
}

#[test]
fn a_busy_switch_is_retried_on_later_frames() {
    const F: u32 = 150;
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 1, 1));
    a.np.cfg.lat_plan = LatPlan::new(&[(50, 6), (51, 1)]).unwrap();
    run(&mut a, &mut b, &clock, 40_000);
    assert_eq!(a.g.recs, b.g.recs);
    let ch = delay_changes(&a.g.timings, 3);
    assert_eq!(
        ch.iter().map(|x| x.1).collect::<Vec<_>>(),
        vec![6, 1],
        "{ch:?}"
    );
    let s = a.np.stats();
    assert_eq!((s.lat_requested, s.lat_refused), (2, 0), "{s:?}");
    assert!(s.lat_busy > 0, "{s:?}");
    let db: Vec<u8> = b.g.timings.iter().map(|t| t.delay).collect();
    let da: Vec<u8> = a.g.timings.iter().map(|t| t.delay).collect();
    assert_eq!(da, db);
}

#[test]
fn wait_overshoot_goes_to_the_right_bucket() {
    let (mut a, _b, _clock) = pair(CheckSource::InputDigest, games(10, 1, 1));
    for over in [0u64, 249, 250, 499, 500, 999, 1_000, 1_999, 2_000, 40_000] {
        a.np.count_wait_over(over);
    }
    let s = a.np.stats();
    assert_eq!(s.wait_timeouts, 10);
    assert_eq!(s.wait_over_hist, [2, 2, 2, 2, 2]);
    assert_eq!(s.wait_over_max_us, 40_000);
}

#[test]
fn a_game_check_mismatch_ends_lockstep_on_both_sides() {
    const F: u32 = 120;
    let (ga, gb) = games(F, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::Game, (ga, gb));
    a.g.corrupt_at = Some(40);
    run(&mut a, &mut b, &clock, 20_000);
    for (s, who) in [(&a, "A"), (&b, "B")] {
        match s.np.end() {
            Some(End::Desync { local, remote }) => {
                assert_eq!((local.seg, local.frame, remote.frame), (0, 60, 60), "{who}");
                assert_ne!(local.rng_state, remote.rng_state, "{who}");
            }
            e => panic!("{who}: {e:?}"),
        }
    }
    for s in [&a, &b] {
        let last = s.g.recs.last().map(|r| r.frame).unwrap_or(0);
        assert!((60..60 + 10).contains(&last), "最後に使ったフレーム {last}");
        assert!(!s.g.done);
    }
}

#[test]
fn a_desync_leaves_both_checks_in_the_status_and_the_summary() {
    use crate::net_watch::NetStatus;
    use crate::trial_summary::{self as ts, Facts, HostFacts, Mark, Stage};

    const F: u32 = 120;
    let (ga, gb) = games(F, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::Game, (ga, gb));
    a.g.corrupt_at = Some(40);
    run(&mut a, &mut b, &clock, 20_000);
    for (who, s, other) in [("A", &a, &b), ("B", &b, &a)] {
        let st = NetStatus::new();
        st.publish(&s.np);
        let snap = st.snapshot();
        let (local, remote) = snap
            .desync
            .unwrap_or_else(|| panic!("{who}: ズレの値が無い"));
        let Some(End::Desync {
            local: l0,
            remote: r0,
        }) = s.np.end()
        else {
            panic!("{who}: {:?}", s.np.end());
        };
        assert_eq!((local, remote), (l0, r0), "{who}");
        assert_eq!((local.seg, local.frame), (0, 60), "{who}");
        assert_ne!(local.rng_state, remote.rng_state, "{who}");
        let Some(End::Desync { local: o_local, .. }) = other.np.end() else {
            panic!("{who}: 相手がズレていない");
        };
        assert_eq!(
            remote, o_local,
            "{who}: 相手の CHECK が、相手側の自分の CHECK と違う"
        );
        let line = snap.text();
        for want in [
            "CHECK のズレ:",
            &format!("seg {} frame {}", local.seg, local.frame),
            &format!("乱数の状態 0x{:04X}", local.rng_state),
            &format!("乱数の状態 0x{:04X}", remote.rng_state),
            &format!("抽選回数 {}", local.rng_count),
            &format!("1P ライフ {}", local.life_1p),
            &format!("2P ライフ {}", local.life_2p),
            "食い違った欄: 乱数の状態",
        ] {
            assert!(line.contains(want), "{who}: {want} が無い: {line}");
        }
        let facts = Facts {
            game: ts::installed_game_facts(),
            net: Some(snap),
            host: HostFacts::default(),
            pace_end: None,
            stage: Stage::Final,
        };
        let items = ts::judge(&facts);
        let sync = ts::item_named(&items, ts::N_SYNC);
        assert_eq!(sync.mark, Mark::Bad, "{who}: {}", sync.text);
        assert!(
            sync.text.starts_with("ズレ（試合を止めた）"),
            "{who}: まとめの同期が「止めた」と言わない: {}",
            sync.text
        );
        assert!(
            sync.text.contains("2P ライフ") && sync.text.contains("抽選回数"),
            "{who}: {}",
            sync.text
        );
        assert_eq!(ts::item_named(&items, ts::N_END).mark, Mark::Bad, "{who}");
        assert_eq!(ts::item_named(&items, ts::N_LINK).mark, Mark::Ok, "{who}");
        assert_eq!(ts::item_named(&items, ts::N_MATCH).mark, Mark::Ok, "{who}");
        assert!(
            snap.connected && snap.path_kind == 1,
            "{who}: 経路は同じ PC の中（loopback = 1）: {snap:?}"
        );
        assert!(snap.connect_wait_ms < 5_000, "{who}: {snap:?}");
        eprintln!("ズレのまとめ {who}:\n{}", ts::render(&facts));
    }
    let (ga, gb) = games(F, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::Game, (ga, gb));
    run(&mut a, &mut b, &clock, 20_000);
    let st = NetStatus::new();
    st.publish(&a.np);
    assert_eq!(st.snapshot().desync, None);
    assert!(st.snapshot().desync_line().is_none());
}

#[test]
fn link_up_is_told_once_per_side_when_the_handshake_completes_and_never_before() {
    let (ga, gb) = games(30, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
    let mut connected_ticks = 0;
    for _ in 0..200 {
        clock.set(clock.get() + FRAME_US);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        for s in [&a, &b] {
            let want = u32::from(s.np.connected().is_some());
            assert_eq!(s.net.link_ups, want, "繋がった印と知らせが合わない");
        }
        if a.np.connected().is_some() && b.np.connected().is_some() {
            connected_ticks += 1;
        }
    }
    assert!(connected_ticks > 100, "繋がっていない: {connected_ticks}");
    assert_eq!((a.net.link_ups, b.net.link_ups), (1, 1));
}

#[test]
fn nothing_goes_out_before_the_stop_point_and_the_hold_feeds_zero() {
    let (mut ga, gb) = games(30, 1, 1);
    ga.menu_before_decide = 5;
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
    let mut saw_hold = 0;
    for _ in 0..150 {
        clock.set(clock.get() + FRAME_US);
        let act = a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if a.np.phase() == Phase::Holding {
            assert_eq!(act, Action::RenderOnly);
            assert_eq!(a.g.cur, 1, "止める地点で場面が切り替わった");
            saw_hold += 1;
        }
    }
    assert!(saw_hold > 100, "止める地点で待っていない: {saw_hold}");
    assert!(a.g.local > 0 && a.g.zero == 0 && a.g.recs.is_empty());
    assert_eq!(a.g.local_reads, 0, "lockstep の外でローカル入力を読んだ");
}

#[test]
fn a_remote_word_outside_the_send_mask_is_not_fed() {
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(60, 1, 1));
    for _ in 0..600 {
        clock.set(clock.get() + FRAME_US);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if a.np.phase() == Phase::Lockstep && a.g.recs.len() > 5 {
            break;
        }
    }
    assert_eq!(
        a.np.phase(),
        Phase::Lockstep,
        "試合に入らない（検査の前提）"
    );
    a.g.begin();
    let applied = Applied {
        frame: 999,
        local: 0x0002,
        remote: 0x1800 | 0x0041,
        switched_to: None,
    };
    let act = a.np.play(applied, &mut a.g, &mut a.net);
    match act {
        Action::Played(r) => assert_eq!((r.p1, r.p2), (0x0041, 0x0002)),
        other => panic!("{other:?}"),
    }
    a.g.after(act, &mut a.np);
}

#[test]
fn closing_every_tick_closes_once_and_the_peer_ends() {
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(600, 1, 1));
    for _ in 0..600 {
        clock.set(clock.get() + FRAME_US);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if a.np.phase() == Phase::Lockstep && a.g.recs.len() > 40 {
            break;
        }
    }
    assert_eq!(
        a.np.phase(),
        Phase::Lockstep,
        "試合に入らない（検査の前提）"
    );
    a.np.close(ByeReason::Finished);
    assert_eq!(a.np.end(), Some(End::Closed(ByeReason::Finished)));
    for _ in 0..300 {
        clock.set(clock.get() + FRAME_US);
        a.np.close(ByeReason::Cancelled);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if b.np.phase() == Phase::Ended {
            break;
        }
    }
    assert_eq!(
        a.np.end(),
        Some(End::Closed(ByeReason::Finished)),
        "2 回目で書き換えた"
    );
    assert_eq!(b.np.end(), Some(End::PeerBye(ByeReason::Finished)));
    // 相手（B）は、届いた A の語を使い切ってからやめる。A は閉じる前に delay（3）フレーム先まで語を送っているので、B は A の使った
    // フレームを全部使い、その先を最大 3 フレーム使う。
    let n = a.g.recs.len();
    assert!(n > 40);
    assert_eq!(
        b.g.recs.get(..n),
        Some(&a.g.recs[..]),
        "A の列が B の先頭と違う"
    );
    assert!(
        b.g.recs.len() - n <= 3,
        "B が {} フレーム先まで使った",
        b.g.recs.len() - n
    );
    let (mut c, _d, _clock) = pair(CheckSource::InputDigest, games(60, 1, 1));
    c.np.close(ByeReason::Cancelled);
    assert_eq!(c.np.end(), Some(End::Closed(ByeReason::Cancelled)));
    assert_eq!(c.np.phase(), Phase::Ended);
}

/// 1 フレームずつ交互に回し、各 tick の `take_save_quit` を数える（`run` と違い、全部を終えても閉じない）。返すのは、両者の「保存して終了」を
/// 取った tick の、その時点で使ったフレームの数（取った順）。
fn run_until_menu(
    a: &mut Side,
    b: &mut Side,
    clock: &std::cell::Cell<u64>,
    max_ticks: u32,
) -> [Vec<usize>; 2] {
    let mut slot = clock.get();
    let mut taken: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    let mut after_done = 0;
    for _ in 0..max_ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        for (i, s) in [&mut *a, &mut *b].into_iter().enumerate() {
            s.g.do_frame(&mut s.np, &mut s.net);
            if s.np.take_save_quit() {
                taken[i].push(s.g.recs.len());
            }
        }
        slot += FRAME_US;
        if a.g.done && b.g.done && a.g.cur == SCENE_MENU && b.g.cur == SCENE_MENU {
            after_done += 1;
            if after_done > 120 {
                return taken;
            }
        }
    }
    panic!(
        "{max_ticks} tick で終わらない: A {:?} {:?} / B {:?} {:?}",
        a.np.phase(),
        a.np.end(),
        b.np.phase(),
        b.np.end()
    );
}

#[test]
fn save_and_quit_closes_both_and_asks_for_the_mark_once() {
    const F: u32 = 70;
    for slow_b in [false, true] {
        let (mut ga, mut gb) = games(F, 1, 1);
        ga.leave_req = REQ_SAVE_AND_QUIT;
        gb.leave_req = REQ_SAVE_AND_QUIT;
        let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
        if slow_b {
            b.net.delay_us = 30_000;
        }
        let taken = run_until_menu(&mut a, &mut b, &clock, 20_000);
        assert_eq!(
            taken,
            [vec![F as usize], vec![F as usize]],
            "slow_b {slow_b}: 両者が試合の全フレームを使った tick に 1 回ずつ"
        );
        assert_eq!(a.g.recs, b.g.recs, "slow_b {slow_b}");
        assert_eq!(a.g.recs.len(), F as usize);
        for s in [&mut a, &mut b] {
            assert_eq!(s.np.phase(), Phase::Ended, "slow_b {slow_b}");
            assert!(
                matches!(
                    s.np.end(),
                    Some(End::Closed(ByeReason::Finished) | End::PeerBye(ByeReason::Finished))
                ),
                "slow_b {slow_b}: {:?}",
                s.np.end()
            );
            assert_eq!(s.g.cur, SCENE_MENU);
            assert!(!s.np.take_save_quit(), "取った後にもう一度真");
        }
        for (s, o) in [(&a, &b), (&b, &a)] {
            let peer = s.np.connected().and_then(|c| c.liveness().peer_bye());
            assert_eq!(
                peer,
                Some(ByeReason::Finished),
                "slow_b {slow_b}: 相手 {:?} の BYE が届いていない",
                o.np.end()
            );
        }
    }
}

#[test]
fn save_and_quit_after_the_net_ended_does_not_ask() {
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(600, 1, 1));
    for _ in 0..2_000 {
        clock.set(clock.get() + FRAME_US);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if a.g.recs.len() > 40 {
            break;
        }
    }
    assert_eq!(
        a.np.phase(),
        Phase::Lockstep,
        "試合に入らない（検査の前提）"
    );
    a.np.close(ByeReason::Cancelled);
    for _ in 0..300 {
        clock.set(clock.get() + FRAME_US);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if b.np.phase() == Phase::Ended {
            break;
        }
    }
    for s in [&mut a, &mut b] {
        assert_eq!(s.np.phase(), Phase::Ended, "検査の前提");
        // ローカルのマッチの終わりのメニューで「保存して終了」（偽のゲームはローカルの入力ではフレームを数えないので、要求を直に置く）。
        s.g.cur = SCENE_MATCH;
        s.g.next = REQ_SAVE_AND_QUIT;
        s.g.do_frame(&mut s.np, &mut s.net);
        assert!(!s.np.take_save_quit(), "ネットワークが終わった後に頼んだ");
        assert_eq!(s.g.cur, SCENE_MENU, "試合を出ていない（検査の前提）");
    }
}

#[test]
fn rematch_and_chara_change_neither_close_nor_ask() {
    const F: u32 = 50;
    let (ga, gb) = games(F, 2, 2);
    assert_eq!((ga.leave_req, gb.leave_req), (1, 1), "検査の前提");
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
    let taken = run_until_menu(&mut a, &mut b, &clock, 30_000);
    assert_eq!(taken, [Vec::<usize>::new(), Vec::new()]);
    assert_eq!(a.g.recs, b.g.recs);
    assert_eq!(a.g.recs.len(), (4 * F) as usize, "再戦と選び直しを通った");
    for s in [&a, &b] {
        assert_eq!(s.np.end(), None, "閉じた");
        assert_ne!(s.np.phase(), Phase::Ended);
    }
}

#[test]
fn a_select_out_of_range_ends_without_playing() {
    let (mut ga, gb) = games(30, 1, 1);
    let bad = BadValue {
        field: "キャラ",
        value: 99,
    };
    ga.reject_agreed = Some(bad);
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
    for _ in 0..600 {
        clock.set(clock.get() + FRAME_US);
        a.g.do_frame(&mut a.np, &mut a.net);
        b.g.do_frame(&mut b.np, &mut b.net);
        if a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            break;
        }
    }
    assert_eq!(a.np.end(), Some(End::BadSelect(bad)));
    assert_eq!(b.np.end(), Some(End::PeerBye(ByeReason::Cancelled)));
    assert!(a.g.recs.is_empty() && a.g.agreed.is_empty() && a.g.charges.is_empty());
    assert!(b.g.recs.is_empty(), "相手だけ試合を進めた");
}

#[test]
fn on_input_never_allocates() {
    const F: u32 = 60;
    let (mut a, mut b, clock) = pair(CheckSource::Game, games(F, 2, 1));
    let mut used = 0u64;
    let mut slot = clock.get();
    for _ in 0..2_000 {
        if clock.get() < slot {
            clock.set(slot);
        }
        for s in [&mut a, &mut b] {
            s.g.begin();
            let before = crate::test_alloc::count();
            let act = s.np.on_input(&mut s.g, &mut s.net);
            used += crate::test_alloc::count() - before;
            s.g.after(act, &mut s.np);
        }
        slot += FRAME_US;
        if a.g.done && b.g.done {
            break;
        }
    }
    assert!(a.g.done && b.g.done, "終わらない");
    assert_eq!(
        a.g.recs.len(),
        (2 * F) as usize,
        "何も起きずに 0 回ではない"
    );
    assert_eq!(used, 0, "on_input が確保した");
}

// ---- 遅れの量をホストが決める ----

/// Ctrl+数字を押し続ける期間（ホストの使ったフレームの数で決める期間。範囲と、その間に押す数字と、押す台 —— 0 = A（ホスト）/ 1 = B（ゲスト）。
/// 期間 1 はホスト、期間 2 はゲストが押す）。
const PICK_WINDOWS: [(usize, usize, u8, usize); 2] = [(100, 112, 4, 0), (460, 468, 2, 1)];

/// `run` と同じ回し方で、各 tick の前に期間を置く（両者とも、ホストの使ったフレームの数で決める）。期間の中でマッチの phase にいれば、マッチ中の
/// 行の材料（`hud_facts`）の印を見る。`on_input` の確保も数える。戻りは確保の数。
fn run_pick(a: &mut Side, b: &mut Side, clock: &std::cell::Cell<u64>, max_ticks: u32) -> u64 {
    let mut slot = clock.get();
    let mut after_end = 0;
    let mut used = 0u64;
    let mut marks_seen = 0u32;
    for _ in 0..max_ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        let played = a.g.recs.len();
        let win = PICK_WINDOWS
            .iter()
            .find(|(s, e, _, _)| (*s..*e).contains(&played));
        for (k, s) in [&mut *a, &mut *b].into_iter().enumerate() {
            s.g.digit_held = Some(
                win.filter(|w| w.3 == k)
                    .map_or(0, |w| (1u16 << w.2) | crate::digit_keys::CTRL_BIT),
            );
            s.g.begin();
            let before = crate::test_alloc::count();
            let act = s.np.on_input(&mut s.g, &mut s.net);
            used += crate::test_alloc::count() - before;
            s.g.after(act, &mut s.np);
            if s.g.done && !s.closed {
                s.np.close(ByeReason::Finished);
                s.closed = true;
            }
        }
        if a.np.cfg.delay_pick && a.np.phase() == Phase::Lockstep && b.np.phase() == Phase::Lockstep
        {
            let (ha, hb) = (a.np.hud_facts().unwrap(), b.np.hud_facts().unwrap());
            // 対戦中の名前（HELLO で交換した値。A は "a"、B は "b"）: それぞれの画面が、自分の席に自分の名前、相手の席に相手の名前を置き、
            // 両者の画面で同じ席に同じ名前が出る（席を取り違えると左右が入れ替わる）。
            {
                use crate::net_ui::HudNames;
                let (sa, sb) = (
                    a.np.connected().unwrap().seat(),
                    b.np.connected().unwrap().seat(),
                );
                assert_ne!(sa, sb);
                let n = |s: &str| Name::new(s);
                assert_eq!(ha.names, Some(HudNames::of(sa, n("a"), n("b"))));
                assert_eq!(hb.names, Some(HudNames::of(sb, n("b"), n("a"))));
                assert_eq!(ha.names, hb.names, "両者の画面で同じ席に同じ名前");
                let seat_of_a = ha
                    .names
                    .map(|x| if x.p1 == n("a") { Seat::P1 } else { Seat::P2 });
                assert_eq!(seat_of_a, Some(sa), "A の名前が A の席に出ない");
            }
            // 推奨はそれぞれが測った RTT から（マッチの間は測れている）。画面の遅れはネットワークの側の目標（まだネットワークの側へ渡していない値 —— マッチの始まりの
            // 推奨値は最初の取得で渡す —— があればそれ）。印は付けない。
            for (h, s) in [(ha, &*a), (hb, &*b)] {
                let c = s.np.connected().unwrap();
                let rtt = c.liveness().rtt_ms();
                assert!(h.recommend.is_some() && h.rtt_ms == rtt);
                assert_eq!(
                    h.recommend,
                    delay_pick::recommend(higanr_core::wire::Mode::Delay, rtt)
                );
                let want =
                    s.np.delay_pick()
                        .and_then(|p| p.pending())
                        .unwrap_or_else(|| c.shown_delay());
                assert_eq!(h.latency, Some(want), "played {played}");
            }
            marks_seen += u32::from(win.is_some());
        }
        slot += FRAME_US;
        if a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            after_end += 1;
            if after_end > 30 {
                if a.np.cfg.delay_pick {
                    assert!(marks_seen > 10, "期間の中の印を見ていない（{marks_seen}）");
                }
                return used;
            }
        }
    }
    panic!("{max_ticks} tick で終わらない");
}

#[test]
fn the_digit_keys_of_both_sides_switch_both_sides_at_the_same_frame() {
    const F: u32 = 400;
    for pick_on in [true, false] {
        let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 2, 1));
        for s in [&mut a, &mut b] {
            s.np.cfg.delay_pick = pick_on;
        }
        let used = run_pick(&mut a, &mut b, &clock, 40_000);
        let ctx = format!("pick_on {pick_on}");
        assert_eq!(used, 0, "{ctx}: on_input が確保した");
        assert_eq!(a.g.recs, b.g.recs, "{ctx}: 両者の列が違う");
        let want: Vec<(u16, u32)> = (0..2u16)
            .flat_map(|s| (0..F).map(move |f| (s, f)))
            .collect();
        let got: Vec<(u16, u32)> = a.g.recs.iter().map(|r| (r.seg, r.frame)).collect();
        assert_eq!(got, want, "{ctx}: seg とフレームの並び");
        let da: Vec<u8> = a.g.timings.iter().map(|t| t.delay).collect();
        let db: Vec<u8> = b.g.timings.iter().map(|t| t.delay).collect();
        assert_eq!(
            da, db,
            "{ctx}: 両者の delay の列が違う（違うフレームで切り替えた）"
        );
        let ch = delay_changes(&a.g.timings, 3);
        if pick_on {
            assert_eq!(
                ch.iter().map(|x| x.1).collect::<Vec<_>>(),
                vec![1, 4, 2],
                "{ch:?}"
            );
            assert_eq!(ch[0].0, 3, "{ch:?}");
            let (pa, pb) = (a.np.delay_pick().unwrap(), b.np.delay_pick().unwrap());
            assert!(pa.is_host() && !pb.is_host());
            assert_eq!((pa.chosen(), pa.digits()), (Some(4), 1), "{pa:?}");
            assert_eq!((pb.chosen(), pb.digits()), (Some(2), 1), "{pb:?}");
            let (sa, sb) = (a.np.stats(), b.np.stats());
            assert_eq!(
                (sa.delay_presses, sb.delay_presses),
                (1, 0),
                "{sa:?} {sb:?}"
            );
            assert_eq!(
                (sa.lat_requested, sb.lat_requested),
                (0, 0),
                "予定の経路を使った"
            );
            let (la, lb) = (
                a.np.connected().unwrap().lockstep().stats(),
                b.np.connected().unwrap().lockstep().stats(),
            );
            assert_eq!(la.want_bound, 3, "{la:?}");
            assert_eq!((lb.wish_sent, la.wish_taken), (1, 1), "{la:?} {lb:?}");
            assert_eq!((sa.switches, sb.switches), (3, 3));
        } else {
            assert!(ch.is_empty(), "{ch:?}");
            assert_eq!(
                (a.np.stats().digit_presses, b.np.stats().digit_presses),
                (0, 0),
                "選びを持たない実行で数字キーを読んだ"
            );
            assert!(a.np.delay_pick().is_none() && b.np.delay_pick().is_none());
        }
    }
}

/// ← → はゲームの入力のまま両者に届き、遅れの量は変えない（偽のゲームの語は ← → を含む疑似乱数）。
#[test]
fn arrow_keys_reach_the_game_as_they_are_and_never_change_the_delay() {
    use higanr_core::input_model::keys;
    const F: u32 = 300;
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 1, 1));
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
        s.g.digit_held = Some(0);
    }
    run(&mut a, &mut b, &clock, 20_000);
    let arrows = keys::LEFT | keys::RIGHT;
    let seen = |s: &Side| {
        s.g.recs
            .iter()
            .filter(|r| (r.p1 | r.p2) & arrows != 0)
            .count()
    };
    assert!(
        seen(&a) > 100 && seen(&b) > 100,
        "← → が語に残っていない（{} / {}）",
        seen(&a),
        seen(&b)
    );
    assert_eq!(a.g.recs, b.g.recs, "両者の列が違う");
    let ch = delay_changes(&a.g.timings, 3);
    assert!(
        ch.len() <= 1,
        "試合の始まりの推奨値のほかに遅れが替わった: {ch:?}"
    );
    assert_eq!(
        (a.np.stats().digit_presses, b.np.stats().digit_presses),
        (0, 0)
    );
}

#[test]
fn digit_keys_set_the_delay_in_the_match_and_in_the_selection_for_both_sides() {
    const F: u32 = 300;
    const K: fn(u8) -> u16 = |d| (1 << d) | crate::digit_keys::CTRL_BIT;
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 1, 3));
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
    }
    let mut sel_ticks = [0u32; 3];
    let mut rec_at_press = None;
    let mut seg_last: Vec<(u8, u8)> = Vec::new();
    let mut mid_checks: Vec<(&str, u8, u8)> = Vec::new();
    let mut slot = clock.get();
    for _ in 0..60_000u32 {
        if clock.get() < slot {
            clock.set(slot);
        }
        let played = a.g.recs.len() as u32;
        let seg_idx = (played / F).min(2) as usize;
        let in_sel = a.np.phase() == Phase::Selecting;
        if in_sel {
            sel_ticks[seg_idx] += 1;
        }
        let in_seg = a.np.phase() == Phase::Lockstep && b.np.phase() == Phase::Lockstep;
        let at = |lo: u32| (lo..lo + 3).contains(&played);
        let (mut ha, mut hb) = (Some(0u16), Some(0u16));
        if in_sel && (5..8).contains(&sel_ticks[0]) && seg_idx == 0 {
            ha = Some(K(5)); // 最初の選択の間: ホストが 5
        }
        if in_seg && seg_idx == 0 && at(100) {
            hb = Some(K(2)); // マッチの途中: ゲストが 2（要求）
        }
        if in_seg && seg_idx == 0 && at(200) {
            ha = Some(K(9)); // マッチの途中: ホストが auto
            rec_at_press.get_or_insert_with(|| a.np.connected().unwrap().recommend_now());
        }
        if in_sel && seg_idx == 1 && (5..8).contains(&sel_ticks[1]) && played == F {
            hb = Some(K(7)); // 2 回目の選択の間: ゲストが 7（要求。auto の旗を降ろす）
        }
        if in_sel && seg_idx == 2 && (5..8).contains(&sel_ticks[2]) && played == 2 * F {
            ha = Some(K(4)); // 3 回目の選択の間: ホストが 4
        }
        // 3 つ目の seg: 読めない間をまたぐ押しっぱなしは効かない。
        if in_seg && seg_idx == 2 {
            let p = played - 2 * F;
            if (50..55).contains(&p) {
                hb = Some(0);
            } else if (55..60).contains(&p) {
                hb = None; // 前面でない間に 1 が押された
            } else if (60..75).contains(&p) {
                hb = Some(K(1)); // 戻っても押しっぱなし → 押下ではない
            } else if (80..85).contains(&p) {
                hb = Some(1 << 6); // Ctrl 無しの 6 → 効かない
            }
            if (150..153).contains(&p) {
                hb = Some(K(1)); // 離して押し直した → 効く
            }
        }
        a.g.digit_held = ha;
        b.g.digit_held = hb;
        for s in [&mut a, &mut b] {
            s.g.do_frame(&mut s.np, &mut s.net);
            if s.g.done && !s.closed {
                s.np.close(ByeReason::Finished);
                s.closed = true;
            }
        }
        let now = a.g.timings.len() as u32;
        let lock = |s: &Side| s.np.connected().map(|c| c.lockstep().delay()).unwrap_or(0);
        if in_seg && seg_idx == 0 && now == 60 && mid_checks.is_empty() {
            mid_checks.push(("選択の間のホストの 5", lock(&a), lock(&b)));
        }
        if in_seg && seg_idx == 2 && now == 2 * F + 100 && mid_checks.len() == 1 {
            mid_checks.push(("読めない間をまたぐ押しっぱなし", lock(&a), lock(&b)));
        }
        if now > 0 && now.is_multiple_of(F) && seg_last.len() < (now / F) as usize {
            let last = a.g.timings.last().map(|t| t.delay).unwrap_or(0);
            let lastb = b.g.timings.last().map(|t| t.delay).unwrap_or(0);
            seg_last.push((last, lastb));
        }
        slot += FRAME_US;
        if a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            break;
        }
    }
    let rec = rec_at_press
        .flatten()
        .expect("推奨値が測れている（検査の前提）");
    assert!(
        rec != 2 && rec != 5 && rec < 8,
        "検査の前提: 推奨値が他の押下の値と区別できる（{rec}）"
    );
    assert_eq!(
        seg_last,
        vec![(rec, rec), (7, 7), (1, 1)],
        "seg の最後の D: seg 0 は auto の推奨値（5 → 2 → auto）、seg 1 は選択の間のゲストの 7（auto の旗が降りる）、seg 2 は選択の間のホストの 4 → \
         読めない間をまたぐ押しっぱなしは無視 → 押し直しの 1"
    );
    assert_eq!(
        mid_checks,
        vec![
            ("選択の間のホストの 5", 5, 5),
            ("読めない間をまたぐ押しっぱなし", 4, 4)
        ],
        "選択の間のホストの 5 が最初の試合の始まりの推奨値（`begin_match`）で上書きされた、選択の間のホストの 4 が seg 2 の先頭に効かない、\
         または読めない間をまたぐ押しっぱなしが押下になった"
    );
    let (reads, presses) = (a.g.digit_reads, a.np.stats().digit_presses);
    a.g.digit_held = Some(K(3));
    a.np.digit_tick(&mut a.g);
    a.g.digit_held = Some(0);
    a.np.digit_tick(&mut a.g);
    a.g.digit_held = Some(K(3));
    a.np.digit_tick(&mut a.g);
    assert_eq!(
        (a.g.digit_reads, a.np.stats().digit_presses),
        (reads, presses),
        "ネットワークが終わった後に数字キーを読んだ"
    );
    let (sa, sb) = (a.np.stats(), b.np.stats());
    assert_eq!(
        (
            sa.digit_presses,
            sa.digit_unchanged,
            sb.digit_presses,
            sb.digit_unchanged
        ),
        (3, 0, 3, 0),
        "押下の数（ホスト 5・auto・4、ゲスト 2・7・1）"
    );
}

/// 台本の実行の 1 回分。ホスト（A）だけに台本の印と、本物の数字 3 の押し（`real_keys`。マッチの 5〜14 tick 目だけ押す。読めない間から
/// 持ち越したキーは押下にならない —— `DigitEdge::feed` の規則）を置く。台本の段階（数字 5）は、台本の実行（`scripted`）で A がマッチに入った
/// 最初の tick に 1 回だけ置く。返す: A の tick ごとの D・A の押下の数（`digit_presses`）・A が `digit_keys` を読んだ回数。
fn run_with_real_digit_keys(scripted: bool, real_keys: bool) -> (Vec<u8>, u32, u64) {
    const F: u32 = 300;
    const K3: u16 = (1 << 3) | crate::digit_keys::CTRL_BIT;
    const K5: u16 = 1 << 5;
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 1, 2));
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
    }
    a.g.scripted = scripted;
    let mut slot = clock.get();
    let mut script_sent = false;
    let mut lock_ticks = 0u32;
    for _ in 0..60_000u32 {
        if clock.get() < slot {
            clock.set(slot);
        }
        a.g.digit_held = if !real_keys {
            None
        } else if (5..15).contains(&lock_ticks) {
            Some(K3)
        } else {
            Some(0)
        };
        if a.np.phase() == Phase::Lockstep {
            lock_ticks += 1;
        }
        if scripted && !script_sent && a.np.phase() == Phase::Lockstep {
            a.g.script_press = Some(K5);
            script_sent = true;
        }
        for s in [&mut a, &mut b] {
            s.g.do_frame(&mut s.np, &mut s.net);
            if s.g.done && !s.closed {
                s.np.close(ByeReason::Finished);
                s.closed = true;
            }
        }
        slot += FRAME_US;
        if a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            break;
        }
    }
    let delays = a.g.timings.iter().map(|t| t.delay).collect();
    (delays, a.np.stats().digit_presses, a.g.digit_reads)
}

#[test]
fn a_scripted_run_never_reads_the_real_digit_keys() {
    let (d_keys, presses_keys, reads_keys) = run_with_real_digit_keys(true, true);
    let (d_none, presses_none, reads_none) = run_with_real_digit_keys(true, false);
    assert_eq!(reads_keys, 0, "台本の実行で本物のキーを読んだ");
    assert_eq!(
        reads_none, 0,
        "台本の実行で本物のキーを読んだ（押していない形）"
    );
    assert_eq!(
        presses_none, 1,
        "検査の前提: 台本の段の押下（数字 5）が 1 回効く"
    );
    assert_eq!(
        presses_keys, presses_none,
        "台本の実行で本物のキーの押下が増えた"
    );
    assert_eq!(d_keys, d_none, "台本の実行で本物のキーが D を替えた");
    let (d_plain, presses_plain, reads_plain) = run_with_real_digit_keys(false, true);
    assert!(reads_plain > 0, "台本の無い実行で本物のキーを読まない");
    assert_eq!(
        presses_plain, 1,
        "台本の無い実行で、押した瞬間の本物の数字 3 が効かない"
    );
    assert_ne!(
        d_plain, d_none,
        "台本の無い実行で本物のキーが D を替えない（読まれていない）"
    );
}

#[test]
fn the_plan_wins_over_the_pick_and_the_connected_line_shows_the_role_and_the_live_value() {
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(200, 1, 1));
    a.np.cfg.lat_plan = LatPlan::new(&[(50, 6)]).unwrap();
    for s in [&mut a, &mut b] {
        s.np.cfg.delay_pick = true;
    }
    let mut seen_live = false;
    let mut slot = clock.get();
    for _ in 0..40_000 {
        if clock.get() < slot {
            clock.set(slot);
        }
        for s in [&mut a, &mut b] {
            s.g.do_frame(&mut s.np, &mut s.net);
            if s.g.done && !s.closed {
                s.np.close(ByeReason::Finished);
                s.closed = true;
            }
        }
        slot += FRAME_US;
        if a.np.phase() == Phase::Lockstep && b.np.phase() == Phase::Lockstep {
            let (ca, _) = a.np.ui_conn(0);
            let (cb, _) = b.np.ui_conn(0);
            let ls = a.np.connected().unwrap().lockstep();
            let live = ls.delay();
            // 出す値は今の遅れか、結び付けた後でまだ使っていない切替の値（画面はネットワークの側の目標 —— `Connected::shown_delay`。切替を使う前の
            // 数フレームは新しい値を先に出す）。AGREE の値のまま止まらないことは下の `seen_live` で見る。
            let shown = [Some(live), ls.pending_switch().map(|sw| sw.delay)];
            assert!(
                shown.iter().flatten().any(|&d| ca
                    == Conn::Connected {
                        seat: Seat::P2,
                        host: true,
                        shape: Some((higanr_core::wire::Mode::Delay, d)),
                    }),
                "{ca:?} / 今 {live} / 使う前の切替 {:?}",
                ls.pending_switch()
            );
            assert!(matches!(
                cb,
                Conn::Connected {
                    host: false,
                    seat: Seat::P1,
                    ..
                }
            ));
            seen_live |= live == 6;
        }
        if a.g.done && b.g.done && a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            break;
        }
    }
    assert!(seen_live, "切り替えた後の値をつながった行で見ていない");
    assert!(a.np.delay_pick().is_none(), "予定のある実行で選びを作った");
    let ch = delay_changes(&a.g.timings, 3);
    assert_eq!(
        ch.iter().map(|x| x.1).collect::<Vec<_>>(),
        vec![6],
        "{ch:?}"
    );
}

#[test]
fn the_ping_is_the_round_trip_even_when_the_pump_is_late() {
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(30, 1, 1));
    let rtt = |s: &Side| s.np.connected().and_then(|c| c.liveness().rtt_ms());
    let counters = |s: &Side| {
        s.np.connected()
            .map(|c| c.liveness().counters())
            .unwrap_or_default()
    };
    for _ in 0..3_000 {
        clock.set(clock.get() + 1_000);
        a.np.tend(&mut a.net);
        b.np.tend(&mut b.net);
        if rtt(&a).is_some() && rtt(&b).is_some() {
            break;
        }
    }
    for s in [&a, &b] {
        let r = rtt(s).expect("往復が測れていない（検査の前提）");
        assert!((4..=5).contains(&r), "1 ms ごとに手入れした往復 {r} ms");
    }
    let before = counters(&a).pings_sent;
    for _ in 0..2_000 {
        if counters(&a).pings_sent != before {
            break;
        }
        clock.set(clock.get() + 1_000);
        a.np.tend(&mut a.net);
        b.np.tend(&mut b.net);
    }
    assert_eq!(counters(&a).pings_sent, before + 1, "次の PING が出ない");
    let pongs = counters(&a).pongs_received;
    for _ in 0..20 {
        clock.set(clock.get() + 1_000);
        b.np.tend(&mut b.net);
    }
    a.np.tend(&mut a.net);
    assert_eq!(counters(&a).pongs_received, pongs + 1, "PONG を 1 通受けた");
    let r = rtt(&a).unwrap();
    assert!(
        (4..=5).contains(&r),
        "手入れが 20 ms 遅れた側の往復 {r} ms（遅れが足されている）"
    );
}

// ---- フレームの始まりの待ち（`NetPlay::head_wait`）----

/// 始まりの待ちの呼び出しの数え（1 側ぶん）。
#[derive(Debug, Default)]
struct HeadTally {
    off: u32,
    ready: u32,
    arrived: u32,
    capped: u32,
    ended: u32,
    /// 始まりの待ちの中で起きた割り当て・解放の回数（確保しない約束）。
    allocs: u64,
}

/// `run` と同じ回し方で、各 tick の最初（do_frame の先頭と同じ順）に始まりの待ちを呼ぶ。呼び出しごとに、時計の進みが結果と合うこと
/// （`Off` / `Ready` は 0、`Arrived(w)` は w、`Capped(w)` は上限ちょうど）を見る。両側の数えを返す。
fn head_run(
    a: &mut Side,
    b: &mut Side,
    clock: &std::cell::Cell<u64>,
    max_ticks: u32,
) -> [HeadTally; 2] {
    let mut tally = [HeadTally::default(), HeadTally::default()];
    let mut slot = clock.get();
    let mut after_end = 0;
    for _ in 0..max_ticks {
        if clock.get() < slot {
            clock.set(slot);
        }
        for (s, t) in [&mut *a, &mut *b].into_iter().zip(tally.iter_mut()) {
            let cap = s.np.cfg.head_wait_cap_us;
            let (c0, n0) = (clock.get(), crate::test_alloc::count());
            let r = s.np.head_wait(&mut s.net);
            t.allocs += crate::test_alloc::count() - n0;
            let dt = clock.get() - c0;
            match r {
                HeadWait::Off => {
                    t.off += 1;
                    assert_eq!(dt, 0, "待たない形で時計が進んだ");
                }
                HeadWait::Ready => {
                    t.ready += 1;
                    assert_eq!(dt, 0, "もう届いているのに時計が進んだ");
                }
                HeadWait::Arrived(w) => {
                    t.arrived += 1;
                    assert_eq!(u64::from(w), dt, "届くまでの時間と時計の進みが違う");
                    assert!(w > 0 && w < cap, "上限の中で届いたはず: {w} / {cap}");
                }
                HeadWait::Capped(w) => {
                    t.capped += 1;
                    assert_eq!((u64::from(w), dt), (u64::from(cap), u64::from(cap)));
                }
                HeadWait::Ended(_) => t.ended += 1,
            }
            s.g.do_frame(&mut s.np, &mut s.net);
            if s.g.done && !s.closed {
                s.np.close(ByeReason::Finished);
                s.closed = true;
            }
        }
        slot += FRAME_US;
        if a.np.phase() == Phase::Ended && b.np.phase() == Phase::Ended {
            after_end += 1;
            if after_end > 30 {
                return tally;
            }
        }
    }
    panic!("{max_ticks} tick で終わらない");
}

/// B → A の片道の遅れ。B を先に回し（同じ slot の中で B の tick → A の tick）、A → B は速いままにすると、A だけが毎フレーム 3 ms 待ち、
/// B は待たない形になる（2 台は 1 本のスレッドで順に回すので、先に回す側の送る時刻は、後に回す側の待ちに引きずられない。双方向に遅らせたり
/// A を先に回したりすると、待ちが毎フレーム積み上がって上限に当たる —— 仮のネットワークの作りの癖）。5 フレーム + 3 ms の根拠: delay 3 で A → B が
/// 速いと B は A の 2 フレーム先で釣り合い、A が使うフレーム f の語は B の tick f − 3（= B の slot では 5 slot 前）の送りなので、片道が
/// 5 フレームより 3 ms 長ければ、A の slot の 3 ms 後に着く。
const LATE_ONE_WAY_US: u64 = 5 * FRAME_US + 3_000;

#[test]
fn head_wait_moves_the_wait_before_the_input_and_never_changes_the_frames() {
    const F: u32 = 120;
    let run_with = |cap: u32| {
        let (mut a, mut b, clock) = pair(CheckSource::InputDigest, games(F, 1, 1));
        b.net.delay_us = LATE_ONE_WAY_US;
        for s in [&mut a, &mut b] {
            s.np.cfg.head_wait_cap_us = cap;
        }
        let [tb, ta] = head_run(&mut b, &mut a, &clock, 20_000);
        (a, b, [ta, tb])
    };
    let (a0, b0, t0) = run_with(0);
    let (a1, b1, t1) = run_with(HEAD_WAIT_CAP_US);
    assert_eq!(a0.g.recs.len(), F as usize);
    assert_eq!(a0.g.recs, b0.g.recs);
    assert_eq!(a1.g.recs, b1.g.recs);
    assert_eq!(
        a1.g.recs, a0.g.recs,
        "先頭の待ちで確定した入力の列が変わった"
    );
    let p1: Vec<u16> = a1.g.recs.iter().map(|r| r.p1).collect();
    let p2: Vec<u16> = a1.g.recs.iter().map(|r| r.p2).collect();
    assert_eq!(p1, expected(SEED_B, 1, F, 3));
    assert_eq!(p2, expected(SEED_A, 1, F, 3));
    let sum = |s: &Side, f: fn(&FrameTiming) -> u32| -> u64 {
        s.g.timings.iter().map(|t| u64::from(f(t))).sum()
    };
    let (wait_us, head_us) = (
        |t: &FrameTiming| t.wait_us,
        |t: &FrameTiming| t.head_wait_us,
    );
    for (s, t) in [(&a0, &t0[0]), (&b0, &t0[1])] {
        assert_eq!(
            (t.arrived, t.capped, t.ready, t.ended),
            (0, 0, 0, 0),
            "{t:?}"
        );
        assert_eq!(s.np.stats().head_waits, 0);
        assert_eq!(sum(s, head_us), 0);
    }
    assert!(
        sum(&a0, wait_us) > 40 * 1_000,
        "切った実行で A の入力の後の待ちが起きていない（検査が素通り）: {} µs",
        sum(&a0, wait_us)
    );
    let (st, t) = (a1.np.stats(), &t1[0]);
    assert!(t.arrived > 40, "先頭で待たない: {t:?}");
    assert_eq!((t.capped, t.ended), (0, 0), "{t:?}");
    assert_eq!(st.head_waits, u64::from(t.arrived), "{st:?}");
    assert_eq!(st.head_wait_caps, 0);
    assert_eq!(
        st.head_wait_us_total,
        sum(&a1, head_us),
        "数えと欄の合計が違う（待った tick を使わずに捨てていない前提）"
    );
    assert!(sum(&a1, head_us) > 40 * 1_000, "{} µs", sum(&a1, head_us));
    assert!(
        sum(&a1, wait_us) * 4 < sum(&a1, head_us),
        "待ちが先頭へ移っていない: 先頭 {} µs / 入力の後 {} µs",
        sum(&a1, head_us),
        sum(&a1, wait_us)
    );
    assert_eq!(t.allocs, 0, "先頭の待ちが確保した");
    let (before, after) = (sum(&a0, wait_us), sum(&a1, head_us) + sum(&a1, wait_us));
    eprintln!(
        "A（{F} フレーム）: 切った実行の入力の後の待ち {before} µs / 入れた実行の先頭の待ち {} µs + 入力の後の待ち {} µs（先頭で待った {} 回）",
        sum(&a1, head_us),
        sum(&a1, wait_us),
        t.arrived
    );
    assert!(
        after * 2 > before && after < before * 2,
        "待ちの合計が変わりすぎ: 切った実行 {before} µs / 入れた実行 {after} µs"
    );
    let (on_log, off_log) = (
        crate::net_watch::NetStatus::new(),
        crate::net_watch::NetStatus::new(),
    );
    on_log.publish(&a1.np);
    off_log.publish(&a0.np);
    let want = format!(
        "先頭で待つ: 有効（上限 {HEAD_WAIT_CAP_US} µs。ウィンドウ（b-spin / b）は格子の後、全画面（ゲーム自身の時計）はゲームがフレームを始める見込みの回だけ。rollback のモードでは待たない）/ 先頭で待った回 {} / 合計 {} µs / 上限に当たった回 0",
        st.head_waits, st.head_wait_us_total
    );
    assert!(
        on_log.timing_text().contains(&want),
        "{}",
        on_log.timing_text()
    );
    assert!(
        off_log.timing_text().contains("先頭で待つ: 無効"),
        "{}",
        off_log.timing_text()
    );
    // 待たなくてよい側（B）は、入れても待たない（遅延の増分 0）。使ったフレームの欄は 0。B が待った 1 回は、マッチの最初に A が読み込みで
    // 遅れている間（B は先に始まり、A の最初の語を待つ。上限で戻り、以後は描画だけのフレームなので重ねて待たない）。
    let tb = &t1[1];
    assert_eq!((tb.arrived, tb.ended), (0, 0), "{tb:?}");
    assert!(
        tb.capped <= 1,
        "B が先頭で何度も待った（上限に当たり続けた）: {tb:?}"
    );
    assert_eq!(
        sum(&b1, head_us),
        0,
        "B の使ったフレームに先頭の待ちが付いた"
    );
    assert!(
        tb.allocs == 0 && tb.ready > 40,
        "B は届いている形で走った: {tb:?}"
    );
}

/// 始まりの待ちの単位の検査のための、マッチの phase の途中の 2 台（遅れ `lat`・A の次の tick の始まりの手前）。戻りは (A, B, 時計, 次の slot)。
fn mid_match(lat: u8) -> (Side, Side, std::rc::Rc<std::cell::Cell<u64>>, u64) {
    let (mut a, mut b, clock) = pair_lat(CheckSource::InputDigest, games(1_000, 1, 1), lat, lat);
    let mut slot = clock.get();
    for _ in 0..5_000 {
        if clock.get() < slot {
            clock.set(slot);
        }
        for s in [&mut a, &mut b] {
            s.g.do_frame(&mut s.np, &mut s.net);
        }
        slot += FRAME_US;
        if a.g.recs.len() >= 20 && b.g.recs.len() >= 20 {
            break;
        }
    }
    assert!(a.g.recs.len() >= 20, "試合の中へ入れない");
    assert!(a.np.phase() == Phase::Lockstep && b.np.phase() == Phase::Lockstep);
    if clock.get() < slot {
        clock.set(slot);
    }
    (a, b, clock, slot)
}

#[test]
fn head_wait_waits_only_for_a_missing_word_up_to_the_cap_once_per_tick() {
    let (mut a, mut b, clock, slot) = mid_match(2);
    for s in [&mut a, &mut b] {
        s.np.cfg.head_wait_cap_us = HEAD_WAIT_CAP_US;
    }
    let t0 = clock.get();
    let reads0 = a.g.local_reads;
    assert_eq!(a.np.head_wait(&mut a.net), HeadWait::Ready);
    assert_eq!(clock.get(), t0, "届いているのに時計が進んだ");
    assert_eq!(a.np.stats().head_waits, 0);
    assert_eq!(a.g.local_reads, reads0, "先頭は自分の入力を取らない");
    let cap = u64::from(HEAD_WAIT_CAP_US);
    let mut stalled_at = None;
    let mut at = slot;
    for i in 0..10 {
        clock.set(clock.get().max(at));
        let before = clock.get();
        let r = a.np.head_wait(&mut a.net);
        let act = {
            a.g.begin();
            let act = a.np.on_input(&mut a.g, &mut a.net);
            a.g.after(act, &mut a.np);
            act
        };
        if matches!(r, HeadWait::Capped(_)) {
            assert_eq!(r, HeadWait::Capped(HEAD_WAIT_CAP_US));
            assert!(clock.get() - before >= cap, "上限より短い");
            assert_eq!(
                act,
                Action::RenderOnly,
                "上限の後は入力の地点の待ちに任せる"
            );
            assert_eq!(a.np.stats().head_wait_caps, 1);
            assert_eq!(a.np.stats().head_waits, 1);
            stalled_at = Some(i);
            break;
        }
        assert!(
            matches!(r, HeadWait::Ready),
            "B を止めている間、語が残っている tick は待たない: {r:?}（{i} tick 目）"
        );
        at += FRAME_US;
    }
    let stalled_at = stalled_at.expect("B を止めても A が待たなかった（検査が待つ形にならない）");
    assert!(
        stalled_at >= 1,
        "最初の tick から待った（先の語が残っていない）"
    );
    let t1 = clock.get();
    assert_eq!(a.np.head_wait(&mut a.net), HeadWait::Off);
    assert_eq!(clock.get(), t1, "描画だけのフレームの後の先頭で待った");
    assert_eq!(a.np.stall_run, 1, "A が止まっていない（検査の前提）");
    a.np.stall_run = 0;
    a.np.cfg.head_wait_cap_us = 0;
    let t2 = clock.get();
    assert_eq!(a.np.head_wait(&mut a.net), HeadWait::Off);
    assert_eq!(clock.get(), t2, "上限 0 なのに待った");
    a.np.cfg.head_wait_cap_us = HEAD_WAIT_CAP_US;
    assert_eq!(
        a.np.head_wait(&mut a.net),
        HeadWait::Capped(HEAD_WAIT_CAP_US)
    );
    assert_eq!(clock.get(), t2 + cap);
    assert_eq!(
        a.np.head_wait(&mut a.net),
        HeadWait::Off,
        "同じ tick で 2 回待った"
    );
}

/// A だけを回して（B は止める）、A が次に要る B の語が無い形（`remote_ready` が偽）まで進める。始まりの待ちは切って回す（上限 0）。最後の tick は
/// 入力の地点が予算まで待って描画だけのフレームにした後のことがあるので、`stall_run` は 0 に戻して返す。戻りは次の slot。
///
/// `switch` は `(何 tick 回した後に, 要求する遅れ)`。A（ホスト）が遅れの切替を要求するのを、B の入力ワードの残りが少なくなってからにする
/// （切替の先のフレームは要求した tick の frame + 旧 delay。そこで A が使う前に止まるには、B の入力ワードの残りがそれより少ないときに要求する）。
fn stall_a_alone(
    a: &mut Side,
    clock: &std::cell::Cell<u64>,
    mut at: u64,
    switch: Option<(u32, u8)>,
) -> u64 {
    let cap = a.np.cfg.head_wait_cap_us;
    a.np.cfg.head_wait_cap_us = 0;
    let mut missing = false;
    for i in 0..10 {
        if let Some((after, target)) = switch
            && i == after
        {
            a.np.link
                .connected_mut()
                .expect("繋がっている")
                .request_delay(target)
                .expect("A はホスト");
        }
        clock.set(clock.get().max(at));
        a.g.begin();
        let act = a.np.on_input(&mut a.g, &mut a.net);
        a.g.after(act, &mut a.np);
        at += FRAME_US;
        if a.np
            .link
            .connected()
            .is_some_and(|c| !c.lockstep().remote_ready())
        {
            missing = true;
            break;
        }
    }
    assert!(missing, "B を止めても A の要る語が尽きない（検査の前提）");
    a.np.stall_run = 0;
    a.np.cfg.head_wait_cap_us = cap;
    clock.set(clock.get().max(at));
    clock.get() + FRAME_US
}

#[test]
fn head_wait_is_subtracted_from_the_input_budget_so_the_render_only_frame_is_not_later() {
    let run = |cap: u32| {
        let (mut a, _b, clock, slot) = mid_match(2);
        stall_a_alone(&mut a, &clock, slot, None);
        a.np.cfg.head_wait_cap_us = cap;
        let t0 = clock.get();
        let r = a.np.head_wait(&mut a.net);
        a.g.begin();
        let act = a.np.on_input(&mut a.g, &mut a.net);
        a.g.after(act, &mut a.np);
        (r, act, clock.get() - t0, a.np.stats().head_wait_caps)
    };
    let (r0, act0, d0, caps0) = run(0);
    let (r1, act1, d1, caps1) = run(HEAD_WAIT_CAP_US);
    assert_eq!((r0, caps0), (HeadWait::Off, 0));
    assert_eq!(
        (r1, caps1),
        (HeadWait::Capped(HEAD_WAIT_CAP_US), 1),
        "先頭で上限まで待つ形になっていない（検査が素通り）"
    );
    assert_eq!((act0, act1), (Action::RenderOnly, Action::RenderOnly));
    assert_eq!(
        d0,
        u64::from(WAIT_BUDGET_US),
        "切った実行の待ちが予算と違う"
    );
    assert_eq!(
        d1, d0,
        "先頭で待った分を予算から引いていない（描画だけのフレームへ進むのが遅れる）"
    );
}

#[test]
fn head_wait_ends_when_the_peer_finishes_or_the_net_ends_during_the_wait() {
    for (reason, ends_phase) in [(ByeReason::Finished, false), (ByeReason::Cancelled, true)] {
        for in_wait in [false, true] {
            let (mut a, mut b, clock, slot) = mid_match(2);
            stall_a_alone(&mut a, &clock, slot, None);
            a.np.cfg.head_wait_cap_us = HEAD_WAIT_CAP_US;
            b.np.close(reason);
            b.np.tend(&mut b.net);
            if !in_wait {
                clock.set(clock.get() + 2_500);
            }
            let (t0, waits0) = (clock.get(), a.np.stats().head_waits);
            let r = a.np.head_wait(&mut a.net);
            let dt = clock.get() - t0;
            let ctx = format!("{reason:?} / 待つ間に届く = {in_wait}");
            if in_wait {
                assert_eq!(r, HeadWait::Ended(2_000), "{ctx}");
                assert_eq!(dt, 2_000, "{ctx}");
                assert_eq!(a.np.stats().head_waits, waits0 + 1, "{ctx}");
                assert_eq!(a.np.stats().head_wait_caps, 0, "{ctx}");
                assert_eq!(a.np.head_wait_pending_us, 2_000, "{ctx}");
            } else {
                assert_eq!(r, HeadWait::Ended(0), "{ctx}");
                assert_eq!(dt, 0, "{ctx}: 届いているのに待った");
                assert_eq!(a.np.stats().head_waits, waits0, "{ctx}");
                assert_eq!(a.np.head_wait_pending_us, 0, "{ctx}");
            }
            assert_eq!(
                a.np.phase() == Phase::Ended,
                ends_phase,
                "{ctx}: 終わりの種類（ネットワークが終わったか・相手の終了を覚えただけか）"
            );
            for _ in 0..3 {
                a.g.begin();
                let act = a.np.on_input(&mut a.g, &mut a.net);
                a.g.after(act, &mut a.np);
            }
            assert_eq!(a.np.phase(), Phase::Ended, "{ctx}: 終わらない");
            assert_eq!(a.np.end(), Some(End::PeerBye(reason)), "{ctx}");
        }
    }
}

#[test]
fn head_wait_is_off_while_a_switch_to_delay_zero_is_pending() {
    let run = |target: u8| {
        let (mut a, _b, clock, slot) = mid_match(2);
        stall_a_alone(&mut a, &clock, slot, Some((2, target)));
        a.np.cfg.head_wait_cap_us = HEAD_WAIT_CAP_US;
        let pending =
            a.np.link
                .connected()
                .and_then(|c| c.lockstep().pending_switch())
                .map(|sw| sw.delay);
        let t0 = clock.get();
        let r = a.np.head_wait(&mut a.net);
        (pending, r, clock.get() - t0)
    };
    let (p0, r0, d0) = run(0);
    assert_eq!(p0, Some(0), "切替が控えに無い（検査の前提）");
    assert_eq!(r0, HeadWait::Off, "0 への切替を控えているのに待った");
    assert_eq!(d0, 0);
    let (p3, r3, d3) = run(3);
    assert_eq!(p3, Some(3), "切替が控えに無い（検査の前提）");
    assert_eq!(
        r3,
        HeadWait::Capped(HEAD_WAIT_CAP_US),
        "対照: 先が 0 でない切替では待つ形のまま"
    );
    assert_eq!(d3, u64::from(HEAD_WAIT_CAP_US));
}

#[test]
fn head_wait_never_waits_at_delay_zero_or_outside_lockstep() {
    const F: u32 = 80;
    let run_with = |cap: u32| {
        let (mut a, mut b, clock) = pair_lat(CheckSource::InputDigest, games(F, 1, 1), 0, 0);
        a.net.delay_us = LATE_ONE_WAY_US;
        b.net.delay_us = LATE_ONE_WAY_US;
        for s in [&mut a, &mut b] {
            s.np.cfg.head_wait_cap_us = cap;
        }
        let t = head_run(&mut a, &mut b, &clock, 20_000);
        (a, b, t)
    };
    let (a0, _b0, _t0) = run_with(0);
    let (a1, b1, t1) = run_with(HEAD_WAIT_CAP_US);
    assert_eq!(
        a0.np.agreed_mode(),
        Some((Mode::Delay, 0)),
        "検査の前提: 遅れ 0"
    );
    assert_eq!(a1.g.recs.len(), F as usize);
    assert_eq!(a1.g.recs, a0.g.recs, "遅れ 0 で先頭の待ちが列を変えた");
    assert_eq!(a1.g.recs, b1.g.recs);
    for (s, t) in [(&a1, &t1[0]), (&b1, &t1[1])] {
        assert_eq!(
            (t.arrived, t.capped, t.ready, t.ended),
            (0, 0, 0, 0),
            "遅れ 0 で待った / 待つ形と見た: {t:?}"
        );
        assert!(t.off > F, "先頭の待ちを呼んでいない");
        assert_eq!(s.np.stats().head_waits, 0);
        assert!(s.g.timings.iter().all(|t| t.head_wait_us == 0));
    }
}

#[test]
fn head_wait_is_off_before_the_match() {
    let (mut a, b, clock) = pair(CheckSource::InputDigest, games(30, 1, 1));
    a.np.cfg.head_wait_cap_us = HEAD_WAIT_CAP_US;
    assert_eq!(a.np.phase(), Phase::Unconnected);
    let t0 = clock.get();
    assert_eq!(a.np.head_wait(&mut a.net), HeadWait::Off);
    assert_eq!(clock.get(), t0);
    assert_eq!(b.np.phase(), Phase::Unconnected);
}

// ---- 同期ズレでマッチを止める ----

fn tick_both(a: &mut Side, b: &mut Side, clock: &std::cell::Cell<u64>) -> (Action, Action) {
    clock.set(clock.get() + FRAME_US);
    let ra = a.g.do_frame(&mut a.np, &mut a.net);
    let rb = b.g.do_frame(&mut b.np, &mut b.net);
    (ra, rb)
}

fn stopped_pair() -> (Side, Side, std::rc::Rc<std::cell::Cell<u64>>) {
    let (ga, gb) = games(300, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::Game, (ga, gb));
    a.g.corrupt_at = Some(40);
    for k in 0..5_000 {
        tick_both(&mut a, &mut b, &clock);
        if a.np.is_stopped() && b.np.is_stopped() {
            return (a, b, clock);
        }
        assert!(k < 4_999, "止まらない: {:?} / {:?}", a.np.end(), b.np.end());
    }
    unreachable!()
}

#[test]
fn a_check_mismatch_stops_the_match_on_both_sides_and_the_game_stays_frozen() {
    let (mut a, mut b, clock) = stopped_pair();
    for (who, s) in [("A", &a), ("B", &b)] {
        assert_eq!(s.np.phase(), Phase::Ended, "{who}");
        assert_eq!(s.np.ui_conn(0).0, Conn::Desynced, "{who}");
        assert!(s.np.stop_flags() & 1 != 0, "{who}: 止めた印が無い");
    }
    let found = [&a, &b]
        .iter()
        .filter(|s| matches!(s.np.end(), Some(End::Desync { .. })))
        .count();
    assert!(found >= 1, "{:?} / {:?}", a.np.end(), b.np.end());
    let frozen = |s: &Side| (s.g.rng, s.g.recs.len(), s.g.local, s.g.zero);
    let (fa, fb) = (frozen(&a), frozen(&b));
    let (ra, rb) = (a.g.render_only, b.g.render_only);
    let (ta, tb) = (a.np.stats().stop_ticks, b.np.stats().stop_ticks);
    for _ in 0..120 {
        let (xa, xb) = tick_both(&mut a, &mut b, &clock);
        assert_eq!((xa, xb), (Action::RenderOnly, Action::RenderOnly));
    }
    assert_eq!(
        (frozen(&a), frozen(&b)),
        (fa, fb),
        "止めた後にゲームが動いた"
    );
    for (who, s) in [("A", &a), ("B", &b)] {
        assert!(
            !s.np.relink_due(10_000_000, (SCENE_MENU, 0), 0),
            "{who}: 止めている間に片づけた"
        );
    }
    assert_eq!(
        (a.g.render_only - ra, b.g.render_only - rb),
        (120, 120),
        "描画だけのフレームの数"
    );
    assert_eq!(
        (a.np.stats().stop_ticks - ta, b.np.stats().stop_ticks - tb),
        (120, 120),
        "本ログの「止めた間の描画だけのフレーム」が合わない"
    );
    assert_eq!(a.np.stats().stop_played, a.np.stats().played);
    let st = crate::net_watch::NetStatus::new();
    st.publish(&a.np);
    let snap = st.snapshot();
    let line = snap.stop_line().expect("止めた行が無い");
    assert!(
        line.contains("試合を止めた")
            && line.contains("delay 方式")
            && line.contains("まだ止めている"),
        "{line}"
    );
    assert!(snap.text().contains("試合を止めた"), "{}", snap.text());
}

#[test]
fn a_peer_desync_bye_stops_this_side_too_and_the_spectators_get_the_end() {
    let (ga, gb) = games(300, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
    let ring = std::sync::Arc::new(crate::spectate_ring::SpectateRing::with_len(1 << 16));
    a.np.set_spectate(std::sync::Arc::clone(&ring));
    for k in 0..2_000 {
        tick_both(&mut a, &mut b, &clock);
        if a.g.recs.len() >= 10 && b.g.recs.len() >= 10 {
            break;
        }
        assert!(k < 1_999, "試合に入らない");
    }
    assert_eq!(
        (a.np.phase(), b.np.phase()),
        (Phase::Lockstep, Phase::Lockstep)
    );
    let chk = |rng_state| Check {
        seg: 0,
        frame: 30,
        rng_state,
        rng_count: 7,
        life_1p: 10,
        life_2p: 10,
    };
    a.np.desync(chk(1), chk(2));
    for _ in 0..60 {
        tick_both(&mut a, &mut b, &clock);
    }
    assert_eq!(
        b.np.end(),
        Some(End::PeerBye(ByeReason::Desync)),
        "相手へ BYE(同期ズレ) が届いていない"
    );
    assert!(b.np.is_stopped() && a.np.is_stopped());
    assert_eq!(a.np.stop_flags(), 1, "見つけた側に bit2 が付いた");
    assert_eq!(b.np.stop_flags(), 1 | 4, "知らされた側に bit2 が付かない");
    assert_eq!(b.np.ui_conn(0).0, Conn::Desynced);
    let (ra, rb) = (a.g.render_only, b.g.render_only);
    for _ in 0..30 {
        let (xa, xb) = tick_both(&mut a, &mut b, &clock);
        assert_eq!((xa, xb), (Action::RenderOnly, Action::RenderOnly));
    }
    assert_eq!((a.g.render_only - ra, b.g.render_only - rb), (30, 30));
    let mut last = None;
    while let Some(p) = ring.pop() {
        last = Some(p.rec);
    }
    assert_eq!(last, Some(Record::End(ByeReason::Desync)));
    let st = crate::net_watch::NetStatus::new();
    st.publish(&b.np);
    let line = st.snapshot().stop_line().unwrap();
    assert!(line.contains("相手の BYE(同期ズレ) を受けた"), "{line}");
}

#[test]
fn esc_returns_a_stopped_match_to_local_only_after_the_key_was_seen_up() {
    let (ga, gb) = games(300, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::Game, (ga, gb));
    a.g.corrupt_at = Some(40);
    a.g.esc_down = true;
    for k in 0..5_000 {
        tick_both(&mut a, &mut b, &clock);
        if a.np.is_stopped() && b.np.is_stopped() {
            break;
        }
        assert!(k < 4_999);
    }
    let local0 = a.g.local;
    for _ in 0..10 {
        let (xa, _) = tick_both(&mut a, &mut b, &clock);
        assert_eq!(xa, Action::RenderOnly, "押したまま止まった瞬間に戻った");
    }
    assert_eq!(a.g.local, local0);
    a.g.esc_down = false;
    for _ in 0..3 {
        let (xa, _) = tick_both(&mut a, &mut b, &clock);
        assert_eq!(xa, Action::RenderOnly, "Esc を離しただけで戻った");
    }
    assert!(a.np.is_stopped());
    a.g.esc_down = true;
    let (xa, xb) = tick_both(&mut a, &mut b, &clock);
    assert_eq!((xa, xb), (Action::Local, Action::RenderOnly));
    assert!(!a.np.is_stopped());
    assert_eq!(a.np.stop_flags() & 3, 3, "戻った印");
    assert_eq!(a.np.ui_conn(0).0, Conn::Finished);
    assert!(b.np.is_stopped(), "相手の側まで戻った");
    for _ in 0..5 {
        let (xa, _) = tick_both(&mut a, &mut b, &clock);
        assert_eq!(xa, Action::Local, "戻った後に止め直した");
    }
    assert!(a.np.relink_due(10_000_000, (SCENE_MENU, 0), 0));
    assert!(!b.np.relink_due(10_000_000, (SCENE_MENU, 0), 0));
    let st = crate::net_watch::NetStatus::new();
    st.publish(&a.np);
    let line = st.snapshot().stop_line().unwrap();
    assert!(line.contains("Esc で戻った"), "{line}");
}

#[test]
fn a_desync_outside_the_match_does_not_stop_anything() {
    let (ga, gb) = games(300, 1, 1);
    let (mut a, mut b, clock) = pair(CheckSource::InputDigest, (ga, gb));
    for k in 0..2_000 {
        tick_both(&mut a, &mut b, &clock);
        if a.np.phase() == Phase::Selecting {
            break;
        }
        assert!(k < 1_999, "選択に入らない");
    }
    let chk = Check {
        seg: 0,
        frame: 0,
        rng_state: 1,
        rng_count: 0,
        life_1p: 10,
        life_2p: 10,
    };
    a.np.desync(chk, chk);
    assert!(!a.np.is_stopped());
    assert_eq!(a.np.stop_flags(), 0);
    assert_eq!(a.np.ui_conn(0).0, Conn::Finished);
    let (xa, _) = tick_both(&mut a, &mut b, &clock);
    assert_eq!(xa, Action::Local);
}

#[test]
fn a_stop_seen_inside_the_input_wait_returns_render_only_in_the_same_tick() {
    let (mut a, mut b, clock, slot) = mid_match(2);
    stall_a_alone(&mut a, &clock, slot, None);
    let chk = Check {
        seg: 0,
        frame: 30,
        rng_state: 1,
        rng_count: 0,
        life_1p: 10,
        life_2p: 10,
    };
    b.np.desync(chk, chk);
    b.np.tend(&mut b.net);
    let frames = a.g.recs.len();
    a.g.begin();
    let act = a.np.on_input(&mut a.g, &mut a.net);
    assert_eq!(act, Action::RenderOnly, "待ちの最中に止めた tick の返し");
    assert!(a.np.is_stopped());
    assert_eq!(a.np.end(), Some(End::PeerBye(ByeReason::Desync)));
    a.g.after(act, &mut a.np);
    assert_eq!(
        a.g.recs.len(),
        frames,
        "止めた tick にゲームが 1 フレーム進んだ"
    );
}

#[test]
fn the_offered_seat_reaches_the_link() {
    use higanr_core::seat_pick::SeatAsk;
    use higanr_core::wire::{Arrival, SeatWish};
    let lc = || link::Config {
        handshake: handshake::Config {
            session_id: SID,
            role_nonce: 0xF000,
            latency: 2,
            name: Name::new("a"),
            host_seat: Seat::P2,
        },
        secret: peer_sim::cli::DEFAULT_SECRET,
        peers: higanr_core::candidate::Candidates::from_addrs(&[addr(2)]).unwrap(),
        key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
    };
    let ask = SeatAsk {
        wish: SeatWish::Any,
        arrival: Arrival::Second,
    };
    let with = NetPlay::build_link(lc(), &Config::default(), None, Some(ask), 0);
    assert_eq!(with.seat_ask(), ask, "添えた希望がセッションへ渡る");
    let without = NetPlay::build_link(lc(), &Config::default(), None, None, 0);
    assert_eq!(
        without.seat_ask(),
        SeatAsk::plain(Seat::P2),
        "添えなければ ini の host_seat"
    );
}
