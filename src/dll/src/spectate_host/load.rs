//! 観戦者の数と受信スレッドの負荷の実測（実時間。`#[ignore]`）。観戦者を 0・4・10 人にして、同じ受信スレッドへ対戦の相手の便
//! （100 Hz）が届く遅れ、受信スレッドが観戦に使った時間、ホストの送りの量、観戦者の遅れを比べる。
use super::*;
use crate::net_udp;
use crate::netplay::Net;
use crate::spectate_testkit::{Behavior, Observer};
use higanr_core::wire::MAX_DATAGRAM;
use std::time::{Duration, Instant};

use super::tests::{MATCH_SESSION, match_ping};
const FRAME: Duration = Duration::from_nanos(16_666_667);
const PEER_GAP: Duration = Duration::from_millis(10);

fn lo() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn sleep_until(t: Instant) {
    loop {
        let now = Instant::now();
        let Some(rem) = t.checked_duration_since(now) else {
            return;
        };
        if rem > Duration::from_micros(1_500) {
            std::thread::sleep(rem - Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn pct(v: &mut [u64], p: f64) -> u64 {
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v.get(i).copied().unwrap_or(0)
}

/// µs の列の「n / 平均 / p50 / p95 / p99 / 最大」。
fn summary(v: &[u64]) -> String {
    let mut s = v.to_vec();
    let mean = s.iter().sum::<u64>() as f64 / s.len().max(1) as f64;
    format!(
        "n={} 平均 {:.1} / p50 {} / p95 {} / p99 {} / 最大 {}",
        s.len(),
        mean,
        pct(&mut s, 0.5),
        pct(&mut s, 0.95),
        pct(&mut s, 0.99),
        pct(&mut s, 1.0),
    )
}

struct Report {
    /// 相手の便が、受信スレッドの `recv_from` を抜けてリングバッファの印を付けられるまでの遅れ（µs。送ってから）。
    stamp_us: Vec<u64>,
    /// 相手の便を、ゲームのスレッドがリングバッファから取り出すまでの遅れ（µs。送ってから）。
    game_us: Vec<u64>,
    /// 観戦の仕事の 1 周の長さ（µs）と、そのうち観戦者の便の処理の分（µs）。
    round_us: Vec<u64>,
    dgram_us: Vec<u64>,
    /// 全観戦者が受けた量の合計（バイト）と、その秒（観戦者が終わりまで受けた時まで）。
    bytes: u64,
    secs: f64,
    /// 観戦者が記録を使えるまでの遅れ（µs。ホストのリングバッファへ積んでから。Frame の記録だけ）。
    watch_us: Vec<u64>,
    /// 受信スレッドの観戦の仕事の割合（観戦の周の合計 ÷ 実時間）。
    duty: f64,
    /// ゲームのスレッドがリングバッファへ 1 記録書く所要（ns）。
    push_ns: Vec<u64>,
    /// 一様に着く便が観戦の仕事に当たって待つ時間の期待値（µs）= Σ(周の長さ²) ÷ 2 ÷ 実時間。
    expected_wait_us: f64,
    sent: usize,
    received: usize,
    snap: Snapshot,
}

/// `late` が 0 でなければ、観戦者が入る前に試合の頭からの記録を `late` 個積んでおく（遅れて入った観戦者が、全員そろって追いつきを
/// 始める形）。
fn run(n: usize, secs: u64, late: u32) -> Report {
    let peer = UdpSocket::bind(lo()).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (mut net, rx) = net_udp::open(lo(), &[peer_addr]).unwrap();
    let me = net.local_addr().unwrap();
    let ini = format!(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = {peer_addr}\nsession = {MATCH_SESSION}\n[spectate]\nenable = 1\n"
    );
    let s = crate::net_config::from_text(&ini).unwrap().unwrap();
    let p = prepare(&s, me).unwrap();
    let ring = Arc::clone(&p.ring);
    let shared = Arc::clone(p.sender.shared());
    assert!(rx.install_spectate(p.sender));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rx.spectate_active() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let frame_rec = |k: u32| Record::Frame {
        frame: k,
        p1: (k as u16) & 0xFF,
        p2: (k as u16).wrapping_mul(3) & 0xFF,
    };
    assert!(ring.push(Record::Seg { seg: 0 }));
    if late > 0 {
        // ゲームのリングバッファ（4,096。溢れると次の `Seg` まで受けない）を溢れさせないよう、2,000 ずつ積んで、受信スレッドが
        // ホストへ移し終えるのを待つ。
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut k = 0;
        while k < late {
            let end = (k + 2_000).min(late);
            for f in k..end {
                assert!(ring.push(frame_rec(f)), "ゲームのリングバッファが溢れた");
            }
            k = end;
            while rx.spectate_snapshot().map_or(0, |s| s.host.pushed) < u64::from(k) + 1 {
                assert!(Instant::now() < deadline, "積んだ記録がホストに入らない");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let spawn = || -> Vec<Observer> {
        (0..n)
            .map(|i| Observer::spawn(&p.code_text, Behavior::Normal, 0x200 + i as u64))
            .collect()
    };
    let mut observers = Vec::new();
    if late == 0 {
        let deadline = Instant::now() + Duration::from_secs(5);
        observers = spawn();
        while rx.spectate_snapshot().map_or(0, |s| s.host.joined) < n as u64 {
            assert!(
                Instant::now() < deadline,
                "観戦者が {n} 人入らない: {:?}",
                rx.spectate_snapshot()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let total_sends = (secs * 1000 / PEER_GAP.as_millis() as u64) as usize;
    let packets: Vec<Vec<u8>> = (0..total_sends as u32).map(match_ping).collect();
    // 時刻の対応: 受信スレッドの印（`now_us`）と、この検査の `Instant`。
    let base = Instant::now();
    let base_us = net.now_us();
    let start = base + Duration::from_millis(50);
    let sender = std::thread::spawn(move || {
        let mut sent = Vec::with_capacity(packets.len());
        for (i, pkt) in packets.iter().enumerate() {
            sleep_until(start + PEER_GAP * i as u32);
            sent.push(Instant::now());
            peer.send_to(pkt, me).unwrap();
        }
        sent
    });

    let frames = (secs * 60) as u32;
    let mut pushed_at: Vec<Instant> = Vec::with_capacity(frames as usize);
    let mut push_ns: Vec<u64> = Vec::with_capacity(frames as usize);
    let mut stamp_raw: Vec<u64> = Vec::new();
    let mut game_at: Vec<Instant> = Vec::new();
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut next_frame = start;
    let t_end = start + Duration::from_secs(secs) + Duration::from_millis(500);
    // 遅れて入る観戦者は、測りの 1 秒目に全員そろって入る（対戦の相手の便が届いている間に、追いつきの送りが重なる）。
    let join_at = start + Duration::from_secs(1);
    let mut k = 0u32;
    while Instant::now() < t_end {
        let now = Instant::now();
        if late > 0 && observers.is_empty() && now >= join_at {
            observers = spawn();
        }
        if k < frames && now >= next_frame {
            pushed_at.push(now);
            let began = Instant::now();
            let ok = ring.push(frame_rec(late + k));
            push_ns.push(began.elapsed().as_nanos() as u64);
            assert!(ok);
            k += 1;
            next_frame += FRAME;
        }
        while let Some((_, _, t_us)) = net.recv(&mut buf) {
            game_at.push(Instant::now());
            stamp_raw.push(t_us);
        }
        net.wait(500);
    }
    let sent = sender.join().unwrap();
    assert!(ring.push(Record::End(ByeReason::Finished)));
    let elapsed = base.elapsed();
    for o in &observers {
        assert!(
            o.wait_end(Duration::from_secs(10)),
            "観戦者が終わりまで受けない"
        );
    }
    // 送った量の秒は、観戦者が終わりまで受けた時まで（遅れて入った観戦者は、測りの終わりの後も追いつきを受けている）。
    let xfer = base.elapsed();
    let seen: Vec<_> = observers.into_iter().map(Observer::finish).collect();
    let snap = rx.spectate_snapshot().expect("集計");

    let off = |t: Instant| t.duration_since(base).as_micros() as u64;
    let k_recv = stamp_raw.len().min(sent.len());
    let stamp_us = (0..k_recv)
        .map(|i| (stamp_raw[i] - base_us).saturating_sub(off(sent[i])))
        .collect();
    let game_us = (0..k_recv)
        .map(|i| game_at[i].duration_since(sent[i]).as_micros() as u64)
        .collect();

    // 観戦者が受けた列が全員同じで、書いた列と一致する（同期）。
    let want_len = 2 + late as usize + frames as usize;
    let mut watch_us = Vec::new();
    for sn in &seen {
        assert_eq!(sn.recs.len(), want_len, "観戦者が受けた記録の数");
        for (j, (idx, r)) in sn.recs.iter().enumerate() {
            assert_eq!(*idx, j as u32);
            if let Record::Frame { frame, p1, p2 } = r {
                assert_eq!(
                    (*p1, *p2),
                    (
                        (*frame as u16) & 0xFF,
                        (*frame as u16).wrapping_mul(3) & 0xFF
                    ),
                    "観戦者の列が書いた列と違う"
                );
                // 先に積んでおいた記録（追いつきの分）は数えない。
                if let Some(at) = frame
                    .checked_sub(late)
                    .and_then(|f| pushed_at.get(f as usize))
                {
                    watch_us.push(sn.popped[j].duration_since(*at).as_micros() as u64);
                }
            }
        }
    }
    let log = |m: &Mutex<Vec<u32>>| -> Vec<u64> {
        m.lock()
            .unwrap()
            .iter()
            .map(|&x| u64::from(x) / 1000)
            .collect()
    };
    let round_us = log(&shared.busy.round);
    let dgram_us = log(&shared.busy.dgram);
    if n > 0 {
        // 観戦者の便のうち重いのは JOIN（鍵の計算）。入った人数ぶんの上位がそれ。
        let mut top = dgram_us.clone();
        top.sort_unstable_by(|a, b| b.cmp(a));
        top.truncate(n);
        eprintln!("  JOIN の処理(µs。{n} 人ぶん): {top:?}");
    }
    let rounds_ns = shared.busy.round.lock().unwrap().clone();
    let sum_ns: u64 = rounds_ns.iter().map(|&x| u64::from(x)).sum();
    let sq_ns: f64 = rounds_ns.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    let _ = rx.stop();
    Report {
        stamp_us,
        game_us,
        round_us,
        dgram_us,
        bytes: seen.iter().map(|s| s.bytes).sum(),
        secs: xfer.as_secs_f64(),
        watch_us,
        duty: sum_ns as f64 / 1e9 / elapsed.as_secs_f64(),
        push_ns,
        expected_wait_us: sq_ns / 2.0 / (elapsed.as_secs_f64() * 1e9) / 1e3,
        sent: sent.len(),
        received: game_at.len(),
        snap,
    }
}

fn show(n: usize, round: usize, r: &Report) {
    eprintln!(
        "[観戦者 {n:2} 人 / 回 {round}] 相手の便 送った {} 受けた {}\n  受信スレッドの印までの遅れ(µs): {}\n  ゲームが取るまでの遅れ(µs): {}\n  観戦の 1 周(µs): {}\n  うち観戦者の便の処理(µs): {}\n  観戦の仕事の割合 {:.3}% / 送った量 全体 {:.1} KB/s（1 人あたり {:.2} KB/s） / 観戦者が使えるまでの遅れ(µs): {}\n  ゲームのスレッドがリングバッファへ書く(ns): {}\n  集計 通 {} 記録 {} 送り直し {} 受けた ACK {}",
        r.sent,
        r.received,
        summary(&r.stamp_us),
        summary(&r.game_us),
        summary(&r.round_us),
        summary(&r.dgram_us),
        r.duty * 100.0,
        r.bytes as f64 / r.secs / 1000.0,
        if n == 0 {
            0.0
        } else {
            r.bytes as f64 / r.secs / 1000.0 / n as f64
        },
        summary(&r.watch_us),
        summary(&r.push_ns),
        r.snap.host.packets_sent,
        r.snap.host.records_sent,
        r.snap.host.resends,
        r.snap.host.acks,
    );
}

/// 1 回の実行の要約: 平均・p50・p95・p99・最大（µs）。
fn five(v: &[u64]) -> [u64; 5] {
    let mut s = v.to_vec();
    let mean = s.iter().sum::<u64>() / s.len().max(1) as u64;
    [
        mean,
        pct(&mut s, 0.5),
        pct(&mut s, 0.95),
        pct(&mut s, 0.99),
        pct(&mut s, 1.0),
    ]
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn env(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(d)
}

/// 条件（観戦者の数, 先に積む記録の数）を交互に `passes` 回ずつ回し、実行ごとの要約の中央値を標準エラーに出す。戻りは条件ごとの
/// （相手の便が受信スレッドの印を受けるまでの遅れの平均の中央値, 観戦の 1 周の最大の中央値）（µs）。
fn compare(cases: &[(usize, u32)], secs: u64, passes: u64) -> Vec<(f64, f64)> {
    // 条件ごとの実行の要約: 印までの遅れ・ゲームが取るまでの遅れ・観戦の 1 周・観戦者が使えるまでの遅れ・リングバッファへ書く所要。
    let mut runs: Vec<Vec<[[u64; 5]; 5]>> = vec![vec![]; cases.len()];
    let mut duty: Vec<Vec<f64>> = vec![vec![]; cases.len()];
    let mut expect: Vec<Vec<f64>> = vec![vec![]; cases.len()];
    let mut sends: Vec<Vec<String>> = vec![vec![]; cases.len()];
    for pass in 0..passes {
        for (slot, &(n, late)) in cases.iter().enumerate() {
            let r = run(n, secs, late);
            assert_eq!(r.sent, r.received, "対戦の相手の便が欠けた");
            show(n, pass as usize, &r);
            runs[slot].push([
                five(&r.stamp_us),
                five(&r.game_us),
                five(&r.round_us),
                five(&r.watch_us),
                five(&r.push_ns),
            ]);
            duty[slot].push(r.duty * 100.0);
            expect[slot].push(r.expected_wait_us);
            sends[slot].push(format!(
                "{:.1} KB/s（送り直した記録 {}）",
                r.bytes as f64 / r.secs / 1000.0,
                r.snap.host.records_resent
            ));
        }
    }
    eprintln!("=== 実行 {passes} 回ぶんの要約の中央値（平均 / p50 / p95 / p99 / 最大、µs） ===");
    let names = [
        "受信スレッドの印までの遅れ",
        "ゲームが取るまでの遅れ",
        "観戦の 1 周",
        "観戦者が使えるまでの遅れ",
        "ゲームのスレッドがリングバッファへ書く（ns）",
    ];
    let mut out = Vec::new();
    for (slot, &(n, late)) in cases.iter().enumerate() {
        eprintln!(
            "[観戦者 {n:2} 人・先に積んだ記録 {late}] 実行 {} 回・送り {:?}",
            runs[slot].len(),
            sends[slot]
        );
        for (k, name) in names.iter().enumerate() {
            let col: Vec<String> = (0..5)
                .map(|c| {
                    format!(
                        "{:.0}",
                        median(runs[slot].iter().map(|r| r[k][c] as f64).collect())
                    )
                })
                .collect();
            eprintln!("  {name}: {}", col.join(" / "));
        }
        out.push((
            median(runs[slot].iter().map(|r| r[0][0] as f64).collect()),
            median(runs[slot].iter().map(|r| r[2][4] as f64).collect()),
        ));
        eprintln!(
            "  観戦の仕事の割合（中央値）{:.3}% / 相手の便が観戦の仕事に当たって待つ時間の期待値（Σ周² / 2 / 実時間。µs）{:.2}",
            median(duty[slot].clone()),
            median(expect[slot].clone())
        );
    }
    out
}

/// 観戦者の数が 0・4・10 のとき、同じ受信スレッドで対戦の相手の便が届く遅れがどう変わるかを実測する。値は標準エラーに出す
/// （条件を交互に何回も回し、実行ごとの要約の中央値で比べる。この PC は他の仕事で揺れるので、標本を合わせず実行ごとに見る）。
/// 落ちる条件: 相手の便が欠ける・観戦者の列が書いた列と違う・観戦者 10 人で相手の便の遅れ（受信スレッドの印まで）の平均の
/// 中央値が 0 人より 0.5 ms を越えて増える。
#[test]
#[ignore = "手で走らせる測り（既定は 1 回 10 秒 × 9 回）。値を標準エラーに出す。HIGANR_LOAD_SECS / HIGANR_LOAD_PASSES で変える"]
fn measure_receive_thread_load_with_0_4_and_10_spectators() {
    let (secs, passes) = (env("HIGANR_LOAD_SECS", 10), env("HIGANR_LOAD_PASSES", 3));
    let r = compare(&[(0, 0), (4, 0), (10, 0)], secs, passes);
    assert!(
        r[2].0 <= r[0].0 + 500.0,
        "観戦者 10 人で相手の便の遅れの平均が 0.5 ms を越えて増えた"
    );
}

/// 試合の頭から 135 秒ぶん（8,100 記録）遅れて入った観戦者 10 人が、測りの 1 秒目にそろって追いつきを始めるとき、同じ受信スレッドで
/// 対戦の相手の便が届く遅れと、観戦の 1 周の最悪を、観戦者 0 人と比べる。値は標準エラーに出す。落ちる条件: 相手の便が欠ける・
/// 観戦者の列が書いた列と違う（全員が先に積んだ記録から最後まで受ける）。
#[test]
#[ignore = "手で走らせる測り（既定は 1 回 10 秒 × 6 回）。値を標準エラーに出す。HIGANR_LOAD_SECS / HIGANR_LOAD_PASSES で変える"]
fn measure_receive_thread_load_with_ten_late_spectators() {
    let (secs, passes) = (env("HIGANR_LOAD_SECS", 10), env("HIGANR_LOAD_PASSES", 3));
    let r = compare(&[(0, 0), (10, 135 * 60)], secs, passes);
    eprintln!(
        "相手の便の遅れの平均: 0 人 {:.0} µs・遅れて入った 10 人 {:.0} µs / 観戦の 1 周の最大: {:.0} µs",
        r[0].0, r[1].0, r[1].1
    );
}
