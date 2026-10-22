//! ホストの送りを直に見る: 送り直しの時計と上限、後ろが届いた穴の速い送り直し、流れの制御で止めた `ack` の記録、往復の標本の選び方、
//! 追いついていない観戦者への送りの上限（通数・量）。

use super::catchup::{Case, MEASURE_SEEDS, Policy, run};
use super::*;
use higanr_core::spectate::{
    BEHIND_RECORDS, CATCHUP_PACKETS_PER_POLL, CATCHUP_PACKETS_PER_WATCHER, RESEND_CAP_MIN_MS,
};
use higanr_core::spectate_pace::{FF_UPDATES_DEFAULT, FF_UPDATES_MAX};
use higanr_core::spectate_rate::{burst_bytes, total_cap_bps, watcher_cap_bps};

/// 観戦者 1 を JOIN させ、16 記録を積んで送り `rtt` ms 後に全部届いた ACK を返すのを `rounds` 回くり返したホスト（往復の見積もりが
/// `rtt` に落ち着いた形）。戻りは（ホスト, いまの時刻 = 最後の ACK の時刻）。
fn host_with_rtt(rtt: u32, rounds: u32) -> (SpectateHost, u32) {
    rtt_rounds(SpectateHost::with_capacity(host_cfg(), 4_096), rtt, rounds)
}

/// `host_with_rtt` を、渡したホストで行う。
fn rtt_rounds(mut host: SpectateHost, rtt: u32, rounds: u32) -> (SpectateHost, u32) {
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

fn bits(from: u32, to: u32, ack: u32) -> Bits512 {
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
    // ウィンドウを 40 ms で出し切るよう、送りの量の上限に依らない形で回す。
    let (mut host, t) = rtt_rounds(window_test_host(4_096), 30, 10);
    let rto = host.rto_of(addr(1)).unwrap();
    assert_eq!(rto, 47);
    let first = host.tail();
    for k in 0..SEND_WINDOW + 100 {
        host.push(frame(k)).unwrap();
    }
    let mut sink = Sink::default();
    // 追いついていない観戦者への送りは 1 `poll` の通数と量に上限があるので、ウィンドウが出そろうまで回す（40 ms ほど）。
    assert_eq!(
        indices(&poll_for(&mut host, &mut sink, t, 40)).len(),
        SEND_WINDOW as usize
    );
    // 出し終えた時点で先頭が届き、ack が 1 つ進んだ（これ以降の ack の止まりは、消費を待つ流れの制御だけ）。
    let t = t + 40;
    let start = first + 1;
    ack_sack(
        &mut host,
        addr(1),
        1,
        start,
        bits(start + 1, first + SEND_WINDOW, start),
        t,
    );
    host.poll(t, &mut sink);
    assert_eq!(indices(&sink.streams()), vec![first + SEND_WINDOW]);
    let resends = host.counts().resends;
    // 30 ms 後から、17 ms ごとに ack が 1 つ進む。`ack` の記録は送ってから 40 ms 以上経っているが、届いていて ack に入って
    // いないだけ（その後ろの、送った記録は全部届いている）。
    let mut now = t + 30;
    for k in 1..=60u32 {
        let next = start + k;
        let sent_end = start + SEND_WINDOW + k - 1;
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
        60,
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
        vec![start + 60, start + 60 + SEND_WINDOW - 1],
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
/// 追いついた観戦者への新しい分は止めずに先に送り、追いついていない観戦者は `poll` ごとに順に回る。量の上限が通数より先に効かない
/// よう、全員が最大の早送り（上限が最も大きい）を名乗る。
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
            join_ff(
                &mut host,
                addr(n),
                u64::from(n),
                FF_UPDATES_MAX as u8,
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
    // 量の上限（観戦者ごとと全体）でも止まるので、全員に回るまで 200 ms 回す。
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

/// 早送りの速さ `ff` の観戦者たち（すぐ ACK を返し、ずっと追いついていない）へ、`ms` の間に送ったバイト数を観戦者ごと・1 秒ごとに数える。
fn bytes_per_second_to_catching_up_spectators(ffs: &[u8], secs: u32) -> Vec<Vec<u64>> {
    let mut host = SpectateHost::with_capacity(host_cfg(), 65_536);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..40_000 {
        host.push(frame(f)).unwrap();
    }
    let mut sink = Sink::default();
    for (i, &ff) in ffs.iter().enumerate() {
        let n = i as u16 + 1;
        join_ff(
            &mut host,
            addr(n),
            u64::from(n),
            ff,
            u32::from(n) * 150,
            &mut sink,
        );
    }
    assert_eq!(host.spectator_count(), ffs.len());
    sink.0.clear();
    // 鍵の計算（X25519）を便ごとにしないよう、観戦者ごとの鍵を先に作る。
    let keys: Vec<DirKeys> = (1..=ffs.len() as u64).map(wkeys).collect();
    let mut next = vec![0u32; ffs.len()];
    let mut per_sec = vec![vec![0u64; secs as usize]; ffs.len()];
    for now in 2_000..2_000 + secs * 1_000 {
        host.poll(now, &mut sink);
        let sent = std::mem::take(&mut sink.0);
        for (to, bytes) in &sent {
            let i = (1..=ffs.len() as u16)
                .position(|n| addr(n) == *to)
                .expect("観戦者の宛先");
            per_sec[i][((now - 2_000) / 1_000) as usize] += bytes.len() as u64;
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
                relay: wire::RelayState::NO_INTRO,
                admit_ack: 0,
                sack: Bits512::EMPTY,
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
    assert!(host.counts().deferred > 0);
    per_sec
}

/// 追いついていない観戦者が 10 人で、全員がすぐ ACK を返しても、ホストの送りは 1 秒あたり全体の上限（上限の大きい 2 人ぶん）＋ 貯めた分まで。
#[test]
fn the_total_cap_holds_with_ten_catching_up_spectators() {
    let ffs = [FF_UPDATES_DEFAULT as u8; MAX_SPECTATORS];
    let per_sec = bytes_per_second_to_catching_up_spectators(&ffs, 4);
    let cap = u64::from(total_cap_bps(ffs.iter().copied()));
    assert_eq!(
        cap,
        2 * u64::from(watcher_cap_bps(FF_UPDATES_DEFAULT as u8))
    );
    let burst = u64::from(burst_bytes(cap as u32));
    let total: Vec<u64> = (0..4).map(|s| per_sec.iter().map(|w| w[s]).sum()).collect();
    eprintln!("1 秒ごとの送り（B）: {total:?}（上限 {cap}）");
    for (s, &b) in total.iter().enumerate().skip(1) {
        assert!(b <= cap + burst, "{s} 秒目に {b} B");
        assert!(
            b >= cap * 9 / 10,
            "{s} 秒目に {b} B（上限まで使えていない —— この検査の狙いが外れた）"
        );
    }
}

/// 観戦者 1 人への追いつきの送りは、その観戦者が JOIN で名乗った早送りの速さの上限（`watcher_cap_bps`）まで。速さの違う 2 人で、それぞれが
/// 自分の上限に当たる（全体の上限は 2 人の和なので先に効かない）。
#[test]
fn each_spectator_is_capped_by_the_ff_max_of_its_join() {
    let ffs = [2u8, FF_UPDATES_MAX as u8];
    let per_sec = bytes_per_second_to_catching_up_spectators(&ffs, 4);
    for (w, &ff) in per_sec.iter().zip(&ffs) {
        let cap = u64::from(watcher_cap_bps(ff));
        let burst = u64::from(burst_bytes(cap as u32));
        eprintln!("ff_max {ff}: 1 秒ごとの送り（B）: {w:?}（上限 {cap}）");
        for (s, &b) in w.iter().enumerate().skip(1) {
            assert!(b <= cap + burst, "ff_max {ff}: {s} 秒目に {b} B");
            assert!(
                b >= cap * 9 / 10,
                "ff_max {ff}: {s} 秒目に {b} B（上限まで使えていない）"
            );
        }
    }
}

/// 10 人が同時に遅れて入っても、追いつきの送りは全体の上限 ＋ 貯めた分まで（追いついた観戦者への 1 記録ずつの流れは上限に数えないので、
/// その分を足した値で縛る）。観戦者 1 人あたりは上限の下で、10 人の合計は上限を越える回線。
#[test]
fn ten_late_spectators_stay_within_the_total_cap() {
    let one = Case {
        rtt: 30,
        loss: 0,
        late_s: 20,
        watchers: 1,
        policy: Policy::Pace,
        ff_max: FF_UPDATES_DEFAULT,
        seed: MEASURE_SEEDS[0],
        limit_s: 60,
    };
    let single = run(one);
    let ten = run(Case {
        watchers: 10,
        ..one
    });
    let cap = u64::from(total_cap_bps([FF_UPDATES_DEFAULT as u8; 10].into_iter()));
    let burst = u64::from(burst_bytes(cap as u32));
    // 追いついた観戦者 1 人への流れ: 1 秒に 60 通の 1 記録の STREAM。
    let live = 60 * wire::sealed_frame_stream_len(1) as u64;
    eprintln!(
        "1 人 {} B/s・10 人 {} B/s（全体の上限 {cap}）・10 人の追いつき {:?} ms",
        single.peak_bps, ten.peak_bps, ten.catchup_ms
    );
    assert!(
        single.peak_bps * 10 > cap * 2,
        "1 人の追いつきが軽すぎて、10 人でも上限に当たらない: {}",
        single.peak_bps
    );
    assert!(
        ten.peak_bps <= cap + burst + 10 * live,
        "10 人の送り {} B/s",
        ten.peak_bps
    );
    assert!(ten.catchup_ms.is_some(), "10 人が追いつかない");
}

/// ウィンドウ（512）のどの語にある穴も送り直しの走査から落ちず、届き済みの記録は送り直さない（穴は 1 つだけ、語ごと・語の境目・ウィンドウの最後の
/// 有効なビットまで）。
#[test]
fn a_hole_in_any_word_of_the_512_window_is_resent_and_nothing_else_is() {
    let mut cases: Vec<u32> = (0..8).map(|word| 2 + 64 * word + 40).collect();
    cases.extend([63 + 2, 64 + 2, 447 + 2, 448 + 2, SEND_WINDOW - 1]);
    for hole in cases {
        let mut host = window_test_host(1_000);
        let mut sink = Sink::default();
        join(&mut host, addr(1), 1, 0, &mut sink);
        for f in 0..SEND_WINDOW + 50 {
            host.push(frame(f)).unwrap();
        }
        let sent = indices(&poll_for(&mut host, &mut sink, 0, 100));
        assert_eq!(
            sent.len(),
            SEND_WINDOW as usize,
            "hole {hole}: ウィンドウを出し終えた"
        );
        // 0 は届いて ack は 1（1 は ack の記録で、届いていない扱い）。穴以外の 2..=511 は届いた。
        let have: Vec<u32> = (2..SEND_WINDOW).filter(|&r| r != hole).collect();
        ack_sack(&mut host, addr(1), 1, 1, sack_of_have(1, &have), 100);
        let mut resent = std::collections::BTreeSet::new();
        for t in 101..400 {
            host.poll(t, &mut sink);
            resent.extend(
                indices(&sink.streams())
                    .into_iter()
                    .filter(|&i| i < SEND_WINDOW),
            );
        }
        assert_eq!(
            resent.into_iter().collect::<Vec<_>>(),
            vec![1, hole],
            "hole {hole}: ack の記録と穴だけを送り直す"
        );
    }
}

/// 追いついていない観戦者（ack が積んだ終わりから `BEHIND_RECORDS` より後ろ）は、遅れがウィンドウ（512）の中でも追いつきの上限で送る。
#[test]
fn a_watcher_further_back_than_the_behind_margin_is_capped_even_inside_the_send_window() {
    let mut host = window_test_host(1_000);
    let mut sink = Sink::default();
    join(&mut host, addr(1), 1, 0, &mut sink);
    for f in 0..300 {
        host.push(frame(f)).unwrap();
    }
    // 積んだ終わり（300）の 200 手前まで受け取った。ウィンドウ（512）の中だが BEHIND_RECORDS（64）より後ろ。
    let lag = 200;
    ack(&mut host, addr(1), 1, 300 - lag, 1);
    host.poll(2, &mut sink);
    assert_eq!(
        sink.streams().len() as u32,
        CATCHUP_PACKETS_PER_WATCHER,
        "遅れ {lag} 記録（13 通ぶん）の送りを 1 回の poll で全部出した"
    );
    assert!(host.counts().deferred > 0);
    // 境目の両側: 3 人がちょうど BEHIND_RECORDS 遅れなら上限を掛けず全員の分を出し、1 つ多く遅れたら 1 回の poll の上限（全員で
    // CATCHUP_PACKETS_PER_POLL 通）で止める。1 人あたりの上限には当たらない遅れなので、全員の上限だけが差を作る。
    let watchers = 3u16;
    let per_packet = MAX_STREAM_RECORDS as u32;
    let per_watcher = (BEHIND_RECORDS + 1).div_ceil(per_packet);
    assert!(per_watcher < CATCHUP_PACKETS_PER_WATCHER);
    assert!(per_watcher * u32::from(watchers) > CATCHUP_PACKETS_PER_POLL);
    for (lag, capped) in [(BEHIND_RECORDS, false), (BEHIND_RECORDS + 1, true)] {
        let mut host = window_test_host(1_000);
        for n in 1..=watchers {
            join(&mut host, addr(n), u64::from(n), 0, &mut sink);
        }
        for f in 0..300 {
            host.push(frame(f)).unwrap();
        }
        for n in 1..=watchers {
            ack(&mut host, addr(n), u64::from(n), 300 - lag, 1);
        }
        sink.0.clear();
        host.poll(2, &mut sink);
        let records = indices(&sink.streams()).len() as u32;
        if capped {
            assert!(
                records <= CATCHUP_PACKETS_PER_POLL * per_packet,
                "遅れ {lag}: {records} 記録"
            );
            assert!(host.counts().deferred > 0, "遅れ {lag}");
        } else {
            assert_eq!(records, lag * u32::from(watchers), "遅れ {lag}");
            assert_eq!(host.counts().deferred, 0, "遅れ {lag}");
        }
    }
}

/// 往復の延びで下げきって止めている間、ホストは追いついている観戦者へも記録を送らず、生きている合図（空の STREAM）だけを
/// `HEARTBEAT_MS` ごとに送る。延びが収まって `RESUME_HOLD_MS` 続いたら、止めていた記録を送り始める。
#[test]
fn a_paused_host_sends_only_heartbeats_and_resumes_after_the_calm_hold() {
    use higanr_core::rtt_probe::DelaySnapshot;
    use higanr_core::spectate_rate::RESUME_HOLD_MS;
    let (mut host, mut t) = host_with_rtt(30, 10);
    let snap = |at: u32, recent: u16| {
        Some(DelaySnapshot {
            at,
            queue_ms: recent - 30,
            base_ms: 30,
        })
    };
    let mut sink = Sink::default();
    // 返事の無い記録を残して止める（送り直しも止まり、合図だけが出る）。
    let unacked = host.tail();
    for k in 0..5 {
        host.push(frame(unacked + k)).unwrap();
    }
    host.poll(t, &mut sink);
    assert_eq!(
        indices(&sink.streams()),
        (unacked..unacked + 5).collect::<Vec<_>>()
    );
    // 延びた往復を渡し続けて、下げきって止める（下げる間隔は 100 ms）。
    for _ in 0..6 {
        host.note_delay(snap(t, 80));
        host.poll(t, &mut sink);
        t += 100;
    }
    assert!(
        host.throttle_state().1,
        "止めていない: {:?}",
        host.throttle_state()
    );
    sink.0.clear();
    // 止めている 3 秒の間に積んだ記録は送らず、合図だけ。
    let first = host.tail();
    let mut heartbeats = Vec::new();
    for k in 0..3_000u32 {
        if k % 17 == 0 {
            host.push(frame(first + k / 17)).unwrap();
        }
        host.note_delay(snap(t, 80));
        host.poll(t, &mut sink);
        for s in sink.streams() {
            assert_eq!(s.count, 0, "止めている間に記録を送った（{k} ms 目）");
            heartbeats.push(k);
        }
        t += 1;
    }
    assert!(
        (2..=4).contains(&heartbeats.len()),
        "合図の数: {heartbeats:?}"
    );
    // 延びが収まっても、`RESUME_HOLD_MS` の手前では送らない。
    for _ in 0..RESUME_HOLD_MS {
        host.note_delay(snap(t, 30));
        host.poll(t, &mut sink);
        assert!(
            sink.streams().iter().all(|s| s.count == 0),
            "静かが続く前に記録を送った"
        );
        t += 1;
    }
    host.note_delay(snap(t, 30));
    host.poll(t, &mut sink);
    assert!(!host.throttle_state().1, "戻していない");
    assert!(
        sink.streams()
            .iter()
            .any(|s| s.count > 0 && s.first >= first),
        "戻した後に止めていた記録を送らない"
    );
}

/// 同じ観戦者が 2 つの経路（IPv4 と IPv6 など）から JOIN して 2 枠になっても、全体の上限は 1 人ぶん（代表の枠だけ数える）。
#[test]
fn the_second_path_of_the_same_spectator_does_not_raise_the_total_cap() {
    let mut host = SpectateHost::with_capacity(host_cfg(), 4_096);
    host.push(Record::Seg { seg: 0 }).unwrap();
    for f in 0..2_000 {
        host.push(frame(f)).unwrap();
    }
    let ff = FF_UPDATES_MAX as u8;
    let mut sink = Sink::default();
    // 同じ nonce（同じ公開鍵）で、送り元だけが違う。
    assert_eq!(
        join_ff(&mut host, addr(1), 1, ff, 0, &mut sink),
        HostEvent::Joined
    );
    assert_eq!(
        join_ff(&mut host, addr(2), 1, ff, 1, &mut sink),
        HostEvent::Joined
    );
    assert_eq!((host.slot_count(), host.spectator_count()), (2, 1));
    sink.0.clear();
    let mut bytes = 0u64;
    for now in 2..502u32 {
        host.poll(now, &mut sink);
        bytes += sink.0.drain(..).map(|(_, b)| b.len() as u64).sum::<u64>();
    }
    let cap = u64::from(watcher_cap_bps(ff));
    let burst = u64::from(burst_bytes(cap as u32));
    eprintln!("2 枠へ 0.5 秒に {bytes} B（1 人ぶんの上限 {cap} B/s）");
    assert!(
        bytes <= cap / 2 + burst + 200,
        "2 つ目の経路で上限が増えた: {bytes} B"
    );
    assert!(
        bytes >= cap * 2 / 5,
        "送れていない（この検査の狙いが外れた）: {bytes} B"
    );
}
