//! ホストの送りを直に見る: 送り直しの時計と上限、後ろが届いた穴の速い送り直し、流れの制御で止めた `ack` の記録、往復の標本の選び方、
//! 追いついていない観戦者への送りの上限（通数・量）。

use super::catchup::{Case, MEASURE_SEEDS, Policy, run};
use super::*;
use higanr_core::spectate::{
    CATCHUP_PACKETS_PER_POLL, CATCHUP_PACKETS_PER_WATCHER, RESEND_CAP_MIN_MS, SEND_BURST_BYTES,
    SEND_RATE_BPS,
};

/// 観戦者 1 を JOIN させ、16 記録を積んで送り `rtt` ms 後に全部届いた ACK を返すのを `rounds` 回くり返したホスト（往復の見積もりが
/// `rtt` に落ち着いた形）。戻りは（ホスト, いまの時刻 = 最後の ACK の時刻）。
fn host_with_rtt(rtt: u32, rounds: u32) -> (SpectateHost, u32) {
    let mut host = SpectateHost::with_capacity(host_cfg(), 4_096);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    let mut t = 0;
    let mut f = 0;
    for _ in 0..rounds {
        for _ in 0..16 {
            host.push(frame(f)).unwrap();
            f += 1;
        }
        host.poll(t, &mut sink);
        assert_eq!(sink.streams().len(), 1);
        t += rtt;
        let tail = host.tail();
        assert_eq!(
            ack(&mut host, addr(1), 1, tail, t),
            HostEvent::Acked { progress: true }
        );
    }
    (host, t)
}

/// 16 記録を積んで `now` に送らせ、その最初の番号を返す。
fn send_16(host: &mut SpectateHost, now: u32) -> u32 {
    let first = host.tail();
    for k in 0..16 {
        host.push(frame(first + k)).unwrap();
    }
    let mut sink = Sink::default();
    host.poll(now, &mut sink);
    assert_eq!(
        sink.streams()
            .iter()
            .map(|s| (s.first, s.count))
            .collect::<Vec<_>>(),
        vec![(first, 16)]
    );
    first
}

fn bits(from: u32, to: u32, ack: u32) -> u64 {
    sack_of_have(ack, &(from..to).collect::<Vec<_>>())
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
    // 最初の 10 は新しい分。以後の送り直しは 100, 200, 250, 250 ms 間隔（倍々は 400 になるところで上限に止まる。往復の見積もりが
    // 無い間の上限は `RESEND_CAP_MIN_MS`）。
    let mut want = vec![10u32];
    let mut at = 10;
    for gap in [
        RESEND_INIT_MS,
        2 * RESEND_INIT_MS,
        RESEND_CAP_MIN_MS,
        RESEND_CAP_MIN_MS,
    ] {
        at += gap;
        want.push(at);
    }
    want.retain(|&t| t < 1_000);
    assert_eq!(times, want);
    assert_eq!(RESEND_INIT_MS, 100, "この検査の期待は 100 ms 始まり");
    assert_eq!(host.resend_cap_of(addr(1)), Some(RESEND_CAP_MIN_MS));
}

/// 往復の見積もりがある観戦者では、送り直しの間隔は記録ごとに数える: 送るたびに倍で上限 `resend_cap`（見積もり 50 ms なら下の値
/// 250 ms）に止まり、ack が進んでも残りの記録の間隔は戻らない。新しく送った記録は自分の時計（見積もり × 倍率）で数え、`ack` の番号の
/// 記録は `ack` が届いた時からも数える。
#[test]
fn resend_clocks_are_per_record_once_the_round_trip_is_known() {
    // 往復 50 ms（送り直しの間隔 67 ms・上限 250 ms）。
    let (mut host, t) = host_with_rtt(50, 10);
    assert_eq!(host.rto_of(addr(1)), Some(67));
    assert_eq!(host.resend_cap_of(addr(1)), Some(RESEND_CAP_MIN_MS));
    let r0 = host.tail();
    host.push(frame(0)).unwrap();
    host.push(frame(1)).unwrap();
    let mut sink = Sink::default();
    let mut times = Vec::new();
    for now in t..t + 710 {
        // 黙った観戦者と見られないよう、JOIN の送り直しで生きている印を更新する（ACK は返さない）。
        if (now - t) % 100 == 0 {
            join(&mut host, addr(1), 1, now, &mut sink);
        }
        host.poll(now, &mut sink);
        if !sink.0.is_empty() {
            times.push(now - t);
            sink.0.clear();
        }
    }
    // 送った 0 の後、67（1 回目の時間切れ。倍率が 2 になる）・+134・+250・+250。
    assert_eq!(times, vec![0, 67, 201, 451, 701]);
    // r0 だけ届いた。新しい r2 を送る。
    ack(&mut host, addr(1), 1, r0 + 1, t + 710);
    host.push(frame(2)).unwrap();
    host.poll(t + 710, &mut sink);
    assert_eq!(indices(&sink.streams()), vec![r0 + 2]);
    let mut seen = Vec::new();
    for now in t + 711..t + 961 {
        if (now - t) % 100 == 0 {
            join(&mut host, addr(1), 1, now, &mut sink);
        }
        host.poll(now, &mut sink);
        for i in indices(&sink.streams()) {
            seen.push((now - t, i));
        }
    }
    // r2 は 67 × 倍率 2 = 134 ms（t + 844）。r1 は最後に送ってから 250 ms（t + 951）と、`ack` が来てから 250 ms（t + 960）の遅い方。
    assert_eq!(seen, vec![(844, r0 + 2), (960, r0 + 1)]);
}

/// 送り直しも落ちた穴は、その後に送った記録が届いたら、倍にした時計（往復の見積もり × 2）を待たずに送り直す。
#[test]
fn a_lost_resend_is_resent_once_a_later_record_arrives_without_its_doubled_timer() {
    // 往復 50 ms に落ち着いた（見積もり 50・ばらつき 0 → 送り直しの間隔 50 + 17 = 67 ms・入れ替わりの幅 34 ms）。
    let (mut host, t) = host_with_rtt(50, 10);
    assert_eq!(host.rto_of(addr(1)), Some(67));
    let a = send_16(&mut host, t);
    let b = send_16(&mut host, t + 10);
    // A は落ち、B だけ届いた。
    ack_sack(&mut host, addr(1), 1, a, bits(b, b + 16, a), t + 60);
    let mut sink = Sink::default();
    for now in t + 61..t + 67 {
        host.poll(now, &mut sink);
    }
    assert!(sink.0.is_empty(), "時計の手前で送った");
    host.poll(t + 67, &mut sink);
    assert_eq!(
        indices(&sink.streams()),
        (a..a + 16).collect::<Vec<_>>(),
        "A の 1 回目の送り直し（時計）"
    );
    // A の送り直しも落ちた。その後に送った C は届いた。
    let c = send_16(&mut host, t + 80);
    ack_sack(&mut host, addr(1), 1, a, bits(b, c + 16, a), t + 130);
    let fast_before = host.counts().records_fast;
    // C が届いたと分かった往復 50 ＋ 入れ替わりの幅 34 = 送り直しから 84 ms（t + 151）。時計なら 67 × 2 = 134 ms（t + 201）。
    for now in t + 131..t + 151 {
        host.poll(now, &mut sink);
    }
    assert!(sink.0.is_empty(), "入れ替わりの幅の手前で送った");
    host.poll(t + 151, &mut sink);
    assert_eq!(
        indices(&sink.streams()),
        (a..a + 16).collect::<Vec<_>>(),
        "後ろが届いた穴 A だけを、時計を待たずに送り直す"
    );
    assert_eq!(host.counts().records_fast - fast_before, 16);
}

/// 観戦者が消費を待って `ack` を止めている（流れの制御。`ack` の記録は届いている）間、消費で `ack` が 1 表示ごとに進むなら、
/// `ack` の記録を送り直さない（送ってから往復の見積もりを越えていても）。
#[test]
fn the_record_at_a_flow_controlled_ack_is_not_resent_while_the_ack_keeps_moving() {
    let (mut host, t) = host_with_rtt(30, 10);
    let rto = host.rto_of(addr(1)).unwrap();
    assert_eq!(rto, 47);
    let start = host.tail();
    for k in 0..300 {
        host.push(frame(k)).unwrap();
    }
    let mut sink = Sink::default();
    host.poll(t, &mut sink);
    assert_eq!(indices(&sink.streams()).len(), SEND_WINDOW as usize);
    let resends = host.counts().resends;
    // 30 ms 後から、17 ms ごとに ack が 1 つ進む。`ack` の記録は送ってから 64 × 17 ms 近く経っているが、届いていて ack に入って
    // いないだけ（その後ろの、送った記録は全部届いている）。
    let mut now = t + 30;
    for k in 0..60u32 {
        let next = start + k;
        let sent_end = start + SEND_WINDOW + k;
        ack_sack(
            &mut host,
            addr(1),
            1,
            next,
            bits(next + 1, sent_end, next),
            now,
        );
        for _ in 0..17 {
            host.poll(now, &mut sink);
            now += 1;
        }
    }
    let ss = sink.streams();
    assert_eq!(
        ss.len(),
        59,
        "ack が 1 つ進むごとに新しい記録を 1 つ送る: {:?}",
        indices(&ss)
    );
    assert_eq!(
        host.counts().resends,
        resends,
        "届いている `ack` の記録を送り直した"
    );
    // 消費が止まったら（`ack` が見積もりより長く進まない）、`ack` の記録を送り直す（最後の ACK の後に送った 1 つも、届いたと分からない
    // ので送り直す）。
    for _ in 0..rto {
        host.poll(now, &mut sink);
        now += 1;
    }
    assert_eq!(
        indices(&sink.streams()),
        vec![start + 59, start + 59 + SEND_WINDOW - 1],
        "止まった `ack` の記録と、最後に送ってまだ ACK の無い記録"
    );
}

/// 後ろの記録が先に届いていた記録（観戦者が消費を待って `ack` を止めていた）が後から `ack` に入っても、その長い待ちを往復の標本に
/// しない。
#[test]
fn a_record_freed_by_consumption_is_not_taken_as_a_round_trip_sample() {
    let (mut host, t) = host_with_rtt(30, 10);
    let rto = host.rto_of(addr(1));
    let a = host.tail();
    for k in 0..SEND_WINDOW {
        host.push(frame(k)).unwrap();
    }
    let mut sink = Sink::default();
    host.poll(t, &mut sink);
    // 往復 30 ms で全部届いたが、観戦者は消費を待って `ack` を `a` で止めた。
    ack_sack(
        &mut host,
        addr(1),
        1,
        a,
        bits(a + 1, a + SEND_WINDOW, a),
        t + 30,
    );
    assert_eq!(host.rto_of(addr(1)), rto, "後ろの記録の標本は 30 ms");
    // 500 ms 後に消費が進み、`a` が ack に入った。
    ack_sack(
        &mut host,
        addr(1),
        1,
        a + 1,
        bits(a + 2, a + SEND_WINDOW, a + 1),
        t + 530,
    );
    assert_eq!(host.rto_of(addr(1)), rto, "530 ms の待ちを標本にした");
}

/// 追いついていない観戦者への送りは、`poll` 1 回で全員 `CATCHUP_PACKETS_PER_POLL` 通・1 人 `CATCHUP_PACKETS_PER_WATCHER` 通まで。
/// 追いついた観戦者への新しい分は止めずに先に送り、追いついていない観戦者は `poll` ごとに順に回る。
#[test]
fn catchup_sends_are_capped_per_poll_and_per_spectator_and_a_live_one_is_not_held() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 4_096);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..2_000 {
        host.push(frame(f)).unwrap();
    }
    let mut sink = Sink::default();
    // 鍵の計算の上限（1 秒に 8 回）に当たらないよう、入る時刻をずらす。
    for n in 1..=MAX_SPECTATORS as u16 {
        assert_eq!(
            join(
                &mut host,
                addr(n),
                u64::from(n),
                u32::from(n) * 150,
                &mut sink
            ),
            HostEvent::Joined
        );
    }
    // 10 人目は追いついている（積んだ終わりの 5 つ前まで受け取った）。
    let live = addr(MAX_SPECTATORS as u16);
    let tail = host.tail();
    ack(&mut host, live, MAX_SPECTATORS as u64, tail - 5, 1_600);
    let mut got = vec![0u32; MAX_SPECTATORS];
    let mut first_poll = None;
    // 量の上限（`SEND_RATE_BPS`）でも止まるので、全員に回るまで 200 ms 回す。
    for now in 1_600..1_800u32 {
        host.poll(now, &mut sink);
        let mut this = vec![0u32; MAX_SPECTATORS];
        for (to, m) in sink.take() {
            // 記録を載せた通だけ（入ったばかりの観戦者への生きている合図は数えない）。
            if let Message::SpectateStream(s) = m
                && s.count > 0
            {
                let i = (1..=MAX_SPECTATORS as u16)
                    .position(|n| addr(n) == to)
                    .expect("観戦者の宛先");
                this[i] += 1;
                got[i] += 1;
            }
        }
        first_poll.get_or_insert(this);
    }
    let first = first_poll.unwrap();
    assert_eq!(
        first[MAX_SPECTATORS - 1],
        1,
        "追いついた観戦者の残り 5 記録（1 通）"
    );
    let behind: u32 = first[..MAX_SPECTATORS - 1].iter().sum();
    assert_eq!(
        behind, CATCHUP_PACKETS_PER_POLL,
        "追いついていない観戦者へ 1 回の poll で送った通数: {first:?}"
    );
    assert!(
        first.iter().all(|&n| n <= CATCHUP_PACKETS_PER_WATCHER),
        "1 人へ 1 回の poll で: {first:?}"
    );
    assert!(
        got.iter().all(|&n| n > 0),
        "回ってこない観戦者がいる: {got:?}"
    );
    assert!(host.counts().deferred > 0);
}

/// 追いついていない観戦者が 10 人で、全員がすぐ ACK を返しても、ホストの送りは 1 秒あたり `SEND_RATE_BPS` ＋ 貯めた分まで。
#[test]
fn the_send_rate_cap_holds_with_ten_catching_up_spectators() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 65_536);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..40_000 {
        host.push(frame(f)).unwrap();
    }
    let mut sink = Sink::default();
    for n in 1..=MAX_SPECTATORS as u16 {
        join(
            &mut host,
            addr(n),
            u64::from(n),
            u32::from(n) * 150,
            &mut sink,
        );
    }
    assert_eq!(host.spectator_count(), MAX_SPECTATORS);
    sink.0.clear();
    // 鍵の計算（X25519）を便ごとにしないよう、観戦者ごとの鍵を先に作る。
    let keys: Vec<DirKeys> = (1..=MAX_SPECTATORS as u64).map(wkeys).collect();
    let mut next = [0u32; MAX_SPECTATORS];
    let mut per_sec = vec![0u64; 4];
    for now in 2_000..6_000u32 {
        host.poll(now, &mut sink);
        let sent = std::mem::take(&mut sink.0);
        for (to, bytes) in &sent {
            per_sec[((now - 2_000) / 1_000) as usize] += bytes.len() as u64;
            let i = (1..=MAX_SPECTATORS as u16)
                .position(|n| addr(n) == *to)
                .expect("観戦者の宛先");
            if let Ok(p) = wire::decode(&key(), Some(&keys[i].opener()), bytes)
                && let Message::SpectateStream(s) = p.msg
                && s.first == next[i]
            {
                next[i] += u32::from(s.count);
            }
        }
        for (i, &n) in next.iter().enumerate() {
            let nonce = i as u64 + 1;
            let msg = Message::SpectateAck {
                nonce,
                next: n,
                sack: 0,
            };
            let mut s = Sink::default();
            host.on_datagram(
                addr(i as u16 + 1),
                &seal_with(keys[i], Sender::Spectator, &msg),
                now,
                &mut s,
            );
        }
    }
    eprintln!("1 秒ごとの送り（B）: {per_sec:?}");
    for (s, &b) in per_sec.iter().enumerate().skip(1) {
        assert!(
            b <= u64::from(SEND_RATE_BPS + SEND_BURST_BYTES),
            "{s} 秒目に {b} B"
        );
        assert!(
            b >= u64::from(SEND_RATE_BPS) * 9 / 10,
            "{s} 秒目に {b} B（上限まで使えていない —— この検査の狙いが外れた）"
        );
    }
    assert!(host.counts().deferred > 0);
}

/// 10 人が同時に遅れて入っても、ホストの送りは 1 秒あたり `SEND_RATE_BPS` ＋ 貯めた分まで（観戦者 1 人あたりは上限の下で、10 人の
/// 合計は上限を越える回線）。
#[test]
fn ten_late_spectators_stay_within_the_send_rate() {
    use higanr_core::spectate::{SEND_BURST_BYTES, SEND_RATE_BPS};
    let one = Case {
        rtt: 30,
        loss: 0,
        late_s: 20,
        watchers: 1,
        policy: Policy::Pace,
        seed: MEASURE_SEEDS[0],
        limit_s: 60,
    };
    let single = run(one);
    let ten = run(Case {
        watchers: 10,
        ..one
    });
    eprintln!("1 人 {} B/s・10 人 {} B/s", single.peak_bps, ten.peak_bps);
    assert!(
        single.peak_bps * 10 > u64::from(SEND_RATE_BPS) * 2,
        "1 人の追いつきが軽すぎて、10 人でも上限に当たらない: {}",
        single.peak_bps
    );
    assert!(
        ten.peak_bps <= u64::from(SEND_RATE_BPS + SEND_BURST_BYTES),
        "10 人の送り {} B/s",
        ten.peak_bps
    );
    assert!(ten.catchup_ms.is_some(), "10 人が追いつかない");
}
