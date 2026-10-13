//! 観戦のリングバッファへの積みの組み上がりの検査（pure）: 選択の画面を持つ偽のゲーム 2 つを、本物の入力の地点の判断（`NetPlay`）と仮のネットワーク
//! （`FakeNet`）で繋ぎ、ホスト（A）に観戦のリングバッファを付けて、リングバッファへ書かれた記録が、ゲームが使った語・入った押下・SELECT の結果と食い違わないかを
//! 見る（`menu_tests::both_sides_walk_the_same_screens` と同じ実行）。
use std::sync::Arc;

use higanr_core::wire::{Record, Seat};

use super::menu_tests::{MenuGame, Run, plans, run_with_ring, run_with_rings};
use crate::spectate_ring::{Popped, SpectateRing};
use crate::spectate_testkit::{expected_selects, fold};

/// `both_sides_walk_the_same_screens` の組（A: ホスト 1P、B: ゲスト 2P。マップの解放が違う）。
fn games() -> (MenuGame, MenuGame) {
    let (pa, pb) = plans();
    let mut a = MenuGame::new(0, pa, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    let mut b = MenuGame::new(1, pb, &[0, 2, 5, 9]);
    b.saved_diff = 3;
    b.chara_cursor = [5, 10];
    a.max_matches = 2;
    b.max_matches = 2;
    // マッチの中の語を 0 にしない（1P と 2P の語が同じ 0 だと、席の入れ違いが見えない）。両者で違う種にする。
    a.match_words = Some(0xA1);
    b.match_words = Some(0xB2);
    (a, b)
}

fn go(delay_us: u64, ring: Option<Arc<SpectateRing>>) -> Run {
    let (a, b) = games();
    run_with_ring(a, b, delay_us, 0, 0, 20_000, ring)
}

fn drain(ring: &SpectateRing) -> Vec<Popped> {
    let mut v = Vec::new();
    while let Some(p) = ring.pop() {
        v.push(p);
    }
    v
}

#[test]
fn the_ring_holds_exactly_what_the_host_game_used() {
    for delay_ms in [1u64, 16, 40] {
        let ring = Arc::new(SpectateRing::with_len(1 << 16));
        let r = go(delay_ms * 1_000, Some(Arc::clone(&ring)));
        let ctx = format!("片道 {delay_ms} ms");
        let popped = drain(&ring);
        assert!(popped.iter().all(|p| !p.after_gap), "{ctx}: 溢れていない");
        let recs: Vec<Record> = popped.iter().map(|p| p.rec).collect();
        let f = fold(&recs);
        let c = ring.counts();
        assert_eq!((c.gaps, c.dropped_full, c.dropped_gap, c.bad), (0, 0, 0, 0));
        assert_eq!(c.pushed as usize, recs.len());

        assert_eq!(recs.first(), Some(&Record::Seg { seg: 0 }), "{ctx}");
        assert_eq!(f.segs, vec![0, 1, 2], "{ctx}: seg の先頭");

        assert_eq!(f.frames, r.a.played, "{ctx}: 試合の列");
        assert_eq!(f.frames.len(), 2 * r.a.match_frames as usize, "{ctx}");

        let log = r.npa.step_log();
        assert!(
            (log.count as usize) < 32,
            "{ctx}: 押下の記録がリングバッファを越える"
        );
        let logged: Vec<_> = log.iter().map(|s| s.step).collect();
        assert_eq!(f.steps, logged, "{ctx}: 押下の列");
        assert!(!f.steps.is_empty(), "{ctx}");

        assert_eq!(f.menus.len() as u64, r.npa.stats().menu_frames, "{ctx}");
        for (i, rec) in recs.iter().enumerate() {
            if matches!(rec, Record::Step(_)) {
                assert!(
                    matches!(recs.get(i + 1), Some(Record::Menu { .. })),
                    "{ctx}: Step の次が Menu でない: {:?}",
                    recs.get(i + 1)
                );
            }
        }

        // SELECT: 止める地点ごとに 1P → 2P の 2 つ。直後が frame 0。共通の欄はホストの値、席ごとの欄は各席の値。
        assert_eq!(f.selects.len(), 2 * r.a.agreed.len(), "{ctx}");
        for (i, rec) in recs.iter().enumerate() {
            if let Record::Select { seat, .. } = rec {
                if *seat == Seat::P1 {
                    assert!(
                        matches!(recs.get(i + 1), Some(Record::Select { seat: Seat::P2, .. })),
                        "{ctx}: 1P の次が 2P でない"
                    );
                } else {
                    assert!(
                        matches!(recs.get(i + 1), Some(Record::Frame { frame: 0, .. })),
                        "{ctx}: SELECT の後が frame 0 でない: {:?}",
                        recs.get(i + 1)
                    );
                }
            }
        }
        let (own, agreed) = r.npa.selects();
        let (own, agreed) = (own.unwrap(), agreed.unwrap());
        let n = f.selects.len();
        let last: Vec<Record> = f.selects[n - 2..]
            .iter()
            .map(|&(seat, sel)| Record::Select { seat, sel })
            .collect();
        assert_eq!(
            last,
            expected_selects(Seat::P1, own, &agreed),
            "{ctx}: 最後の SELECT"
        );
        for (pair, a) in f.selects.chunks(2).zip(&r.a.agreed) {
            assert_eq!(
                (pair[0].0, pair[1].0),
                (Seat::P1, Seat::P2),
                "{ctx}: 席の並び"
            );
            for (_, s) in pair {
                assert_eq!(
                    (
                        s.map,
                        s.difficulty,
                        s.cpu_1p,
                        s.cpu_2p,
                        s.background,
                        s.rng_seed
                    ),
                    (
                        a.map,
                        a.difficulty,
                        a.cpu_1p,
                        a.cpu_2p,
                        a.background,
                        a.rng_seed
                    ),
                    "{ctx}: 共通の欄がホストの値でない"
                );
            }
            assert_eq!(pair[1].1.chara, a.peer.chara, "{ctx}: 2P のキャラ");
            assert_eq!(pair[1].1.color, a.peer.color, "{ctx}: 2P の色");
            assert_eq!(pair[1].1.life, a.peer.life, "{ctx}: 2P の初期体力");
        }
        assert_eq!(r.a.on_input_allocs, 0, "{ctx}: on_input が確保した");
    }
}

#[test]
fn a_ring_that_overflows_changes_nothing_about_the_match() {
    let base = go(8_000, None);
    let tiny = Arc::new(SpectateRing::with_len(8));
    let r = go(8_000, Some(Arc::clone(&tiny)));
    let c = tiny.counts();
    assert!(
        c.gaps >= 1 && c.dropped_full > 0 && c.dropped_gap > 0,
        "溢れていない: {c:?}"
    );
    assert_eq!(r.a.played, base.a.played, "A の試合の列");
    assert_eq!(r.b.played, base.b.played, "B の試合の列");
    assert_eq!(r.npa.stats(), base.npa.stats(), "A の Stats");
    assert_eq!(r.npb.stats(), base.npb.stats(), "B の Stats");
    let steps = |np: &super::NetPlay| np.step_log().iter().map(|s| s.step).collect::<Vec<_>>();
    assert_eq!(steps(&r.npa), steps(&base.npa));
    assert_eq!(r.a.on_input_allocs, 0);
    // 溢れた後、リングバッファに残ったものは seg の始まりから始まる列だけ（溢れた後の最初の Seg には印が付く）。
    let popped = drain(&tiny);
    let marked: Vec<_> = popped.iter().filter(|p| p.after_gap).collect();
    assert!(
        marked.iter().all(|p| matches!(p.rec, Record::Seg { .. })),
        "溢れの印が Seg 以外に付いた"
    );
}

#[test]
fn a_host_only_ring_is_dropped_by_the_guest_at_connect_and_kept_by_the_host() {
    let base = go(8_000, None);
    let (ra, rb) = (
        Arc::new(SpectateRing::with_len(1 << 16)),
        Arc::new(SpectateRing::with_len(1 << 16)),
    );
    let (a, b) = games();
    let r = run_with_rings(
        a,
        b,
        8_000,
        0,
        0,
        20_000,
        (Some(Arc::clone(&ra)), Some(Arc::clone(&rb))),
        true,
    );
    assert_eq!(
        (r.a.on_input_allocs, r.b.on_input_allocs),
        (0, 0),
        "on_input が確保した（ホスト・ゲスト）"
    );
    assert!(
        ra.counts().pushed > 100,
        "ホストのリングバッファに積まれていない"
    );
    assert_eq!(
        rb.counts().pushed,
        0,
        "ゲストのリングバッファに記録が積まれた"
    );
    assert!(r.npa.has_spectate(), "ホストがリングバッファを外した");
    assert!(
        !r.npb.has_spectate(),
        "ゲストがリングバッファを外していない"
    );
    assert_eq!(r.a.played, base.a.played);
    assert_eq!(r.b.played, base.b.played);
    assert_eq!(r.npa.stats(), base.npa.stats());
    assert_eq!(r.npb.stats(), base.npb.stats());
    let recs: Vec<Record> = drain(&ra).iter().map(|p| p.rec).collect();
    assert_eq!(
        fold(&recs).frames,
        r.a.played,
        "ホストのリングバッファの試合の列"
    );
    let (rc, rd) = (
        Arc::new(SpectateRing::with_len(1 << 16)),
        Arc::new(SpectateRing::with_len(1 << 16)),
    );
    let (a, b) = games();
    let v0 = run_with_rings(
        a,
        b,
        8_000,
        0,
        0,
        20_000,
        (Some(Arc::clone(&rc)), Some(Arc::clone(&rd))),
        false,
    );
    assert!(
        rd.counts().pushed > 100,
        "v0 のゲストのリングバッファに積まれない"
    );
    assert!(v0.npa.has_spectate() && v0.npb.has_spectate());
}

#[test]
fn the_host_pushes_stats_once_a_second_only_inside_a_match() {
    use higanr_core::wire::{STATS_PING_UNKNOWN, STATS_ROLLBACK_NONE};
    for delay_ms in [1u64, 16, 40] {
        let ctx = format!("片道 {delay_ms} ms");
        let long_games = || {
            let (mut a, mut b) = games();
            a.match_frames = 150;
            b.match_frames = 150;
            (a, b)
        };
        let (a0, b0) = long_games();
        let base = run_with_ring(a0, b0, delay_ms * 1_000, 0, 0, 20_000, None);
        let ring = Arc::new(SpectateRing::with_len(1 << 16));
        let (mut a, b) = long_games();
        a.hud_log = Some(Vec::new());
        let r = run_with_ring(
            a,
            b,
            delay_ms * 1_000,
            0,
            0,
            20_000,
            Some(Arc::clone(&ring)),
        );
        let recs: Vec<Record> = drain(&ring).iter().map(|p| p.rec).collect();
        let f = fold(&recs);
        assert_eq!(f.frames, r.a.played, "{ctx}: 試合の列");
        assert_eq!(
            r.a.played, base.a.played,
            "{ctx}: リングバッファと呼びを足しても対戦の列が変わった"
        );
        assert_eq!(r.npa.stats(), base.npa.stats(), "{ctx}");
        assert_eq!(r.a.on_input_allocs, 0, "{ctx}: on_input が確保した");

        let log = r.a.hud_log.as_ref().unwrap();
        let mut want: Vec<(u16, u8, u8)> = Vec::new();
        let mut run_pos = 0usize;
        let mut inside = 0usize;
        for h in log {
            match h {
                Some(h) if h.format.is_none() => {
                    if run_pos.is_multiple_of(60) {
                        let ping = h
                            .rtt_ms
                            .map_or(STATS_PING_UNKNOWN, |p| p.min(65_534) as u16);
                        assert!(h.rewound.is_none(), "{ctx}: この実行は delay 方式のはず");
                        want.push((ping, h.latency.unwrap(), STATS_ROLLBACK_NONE));
                    }
                    run_pos += 1;
                    inside += 1;
                }
                Some(_) | None => run_pos = 0,
            }
        }
        assert!(inside > 120, "{ctx}: 試合の中の呼びが少ない {inside}");
        assert!(
            want.len() >= 2,
            "{ctx}: 期待が少なすぎる（空振り）{}",
            want.len()
        );
        assert_eq!(
            f.stats, want,
            "{ctx}: 積んだ Stats が hud_facts の値・間隔と違う"
        );
        assert!(
            f.stats.iter().any(|s| s.0 != STATS_PING_UNKNOWN),
            "{ctx}: ping が 1 度も測れていない（値の欄が空振り）"
        );

        // マッチの外には積まない: 各 Stats は、その seg の SELECT（1P → 2P）の後ろ・次の Seg の前（マッチの語の並びの中）にだけある。
        let mut selects_in_seg = 0;
        let mut in_stats = 0;
        for rec in &recs {
            match rec {
                Record::Seg { .. } => selects_in_seg = 0,
                Record::Select { .. } => selects_in_seg += 1,
                Record::Stats { .. } => {
                    assert_eq!(selects_in_seg, 2, "{ctx}: 試合の外（選択の最中）に Stats");
                    in_stats += 1;
                }
                _ => {}
            }
        }
        assert_eq!(in_stats, f.stats.len());

        let ring0 = Arc::new(SpectateRing::with_len(1 << 16));
        let _ = go(delay_ms * 1_000, Some(Arc::clone(&ring0)));
        let recs0: Vec<Record> = drain(&ring0).iter().map(|p| p.rec).collect();
        assert!(fold(&recs0).stats.is_empty(), "{ctx}: 呼ばないのに Stats");
    }
}
