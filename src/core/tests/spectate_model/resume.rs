//! 続きの番号から記録を受け取り直すためのJOIN（観戦者がホストへ送る。`JoinTree::resume_from`）とホストが観戦者ごとに持つ中継の木のフィールド。

use super::*;
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::spectate::TreeView;
use higanr_core::wire::{JoinTree, RelayState};

/// `from`〜`to` msの間はホストと観戦者`w`（送り元`wa`）の間で便を落とさずすぐ渡し合う。使った記録を`got`へ。
fn pump(
    host: &mut SpectateHost,
    w: &mut Watcher,
    wa: SocketAddr,
    got: &mut Vec<(u32, Record)>,
    from: u32,
    to: u32,
) {
    for t in from..to {
        let mut up = Sink::default();
        w.poll(t, &mut up);
        let mut down = Sink::default();
        for (_, b) in up.0.drain(..) {
            host.on_datagram(wa, &b, t, &mut down);
        }
        host.poll(t, &mut down);
        let mut acks = Sink::default();
        for (_, b) in down.0.drain(..) {
            w.on_datagram(HOST, &b, t, &mut acks);
        }
        for (_, b) in acks.0.drain(..) {
            host.on_datagram(wa, &b, t, &mut Sink::default());
        }
        while let Some(r) = w.pop_indexed() {
            got.push(r);
        }
    }
}

fn join_with(nonce: u64, tree: JoinTree) -> Message {
    let Message::SpectateJoin(j) = join_msg(nonce) else {
        unreachable!()
    };
    Message::SpectateJoin(SpectateJoin { tree, ..j })
}

fn resume_join(nonce: u64, resume_from: Option<u32>) -> Message {
    join_with(
        nonce,
        JoinTree {
            resume_from,
            ..JoinTree::NONE
        },
    )
}

struct Rejoin {
    /// 観戦者が使った列（番号つき）。
    got: Vec<(u32, Record)>,
    jumps: u64,
    /// 入り直した流れの始まり（ホストのack）。
    start: Option<u32>,
    /// ホストが積んだ列。
    log: Vec<Record>,
}

/// 観戦者が10記録を受けた後でホストに外され、その間に次のseg（15〜）が始まる。`resume`なら続きの番号でJOINし直す。
fn rejoin_after_a_new_seg(resume: bool) -> Rejoin {
    let mut host = SpectateHost::with_capacity(host_cfg(), 256);
    let mut log = vec![Record::Seg { seg: 0 }];
    log.extend((0..9).map(frame));
    for r in &log {
        host.push(*r).unwrap();
    }
    let wa = addr(1);
    let mut w = watcher(1, &[HOST]);
    let mut got = Vec::new();
    pump(&mut host, &mut w, wa, &mut got, 0, 50);
    assert_eq!(w.ack(), 10, "最初の 10 記録を受けていない");
    let mut sink = Sink::default();
    w.bye(ByeReason::Cancelled, &mut sink);
    for (_, b) in sink.0.drain(..) {
        host.on_datagram(wa, &b, 50, &mut Sink::default());
    }
    assert_eq!(host.spectator_count(), 0);
    let mut more: Vec<Record> = (9..14).map(frame).collect();
    more.push(Record::Seg { seg: 1 });
    more.extend((0..4).map(frame));
    for r in &more {
        host.push(*r).unwrap();
    }
    log.extend(more);
    assert_eq!((host.join_start(), host.tail()), (15, 20));
    let resume_from = resume.then(|| w.ack());
    let ev = host.on_datagram(
        wa,
        &to_host(&resume_join(0xA001, resume_from)),
        100,
        &mut sink,
    );
    assert_eq!(ev, HostEvent::Joined);
    let start = host.ack_of(wa);
    pump(&mut host, &mut w, wa, &mut got, 100, 200);
    Rejoin {
        got,
        jumps: w.assembler_counts().jumps,
        start,
        log,
    }
}

#[test]
fn a_rejoin_with_resume_from_continues_the_column_without_a_jump() {
    let Rejoin {
        got,
        jumps,
        start,
        log,
    } = rejoin_after_a_new_seg(true);
    assert_eq!(start, Some(10), "続きの番号から流れていない");
    assert_eq!(jumps, 0);
    let want: Vec<(u32, Record)> = log
        .iter()
        .enumerate()
        .map(|(i, r)| (i as u32, *r))
        .collect();
    assert_eq!(got, want, "観戦者の列がホストの列と一致しない");
}

/// 比べ: 続きの番号が無いといまのsegの始まりへ飛んで10〜14を受けない（上の検査が`resume_from`を読んでいる証拠）。
#[test]
fn a_rejoin_without_resume_from_jumps_to_the_join_start() {
    let Rejoin {
        got, jumps, start, ..
    } = rejoin_after_a_new_seg(false);
    assert_eq!(start, Some(15));
    assert_eq!(jumps, 1);
    let idx: Vec<u32> = got.iter().map(|g| g.0).collect();
    assert_eq!(idx, (0..10).chain(15..20).collect::<Vec<_>>());
}

#[test]
fn a_resume_from_outside_the_ring_falls_back_to_the_join_start() {
    // リングバッファ12に13記録: 先頭は9（seg 1の始まり）、積んだ終わりは13（`a_rematch_join_falls_back_…`と同じ形）。
    let recs = rematch_order();
    for (resume, want) in [
        (Some(8), 9),
        (Some(10), 10),
        (Some(13), 13),
        (Some(14), 9),
        (Some(u32::MAX), 9),
        (None, 9),
    ] {
        let mut host = host_after(12, &recs, 13);
        assert_eq!((host.head(), host.join_start(), host.tail()), (9, 9, 13));
        let mut sink = Sink::default();
        let ev = host.on_datagram(addr(1), &to_host(&resume_join(1, resume)), 0, &mut sink);
        assert_eq!(ev, HostEvent::Joined, "{resume:?}");
        assert_eq!(host.ack_of(addr(1)), Some(want), "{resume:?}");
        host.poll(0, &mut sink);
        let s = sink.streams();
        if want < 13 {
            assert_eq!((s[0].first, s[0].reset), (want, true), "{resume:?}");
        }
    }
}

#[test]
fn a_restarted_watcher_in_the_same_slot_also_resumes() {
    let mut host = host_after(64, &rematch_order(), 19);
    let mut sink = Sink::default();
    assert_eq!(join(&mut host, addr(1), 1, 0, &mut sink), HostEvent::Joined);
    assert_eq!(host.ack_of(addr(1)), Some(0));
    let ev = host.on_datagram(
        addr(1),
        &to_host(&resume_join(2, Some(12))),
        REKEY_GAP_MS,
        &mut sink,
    );
    assert_eq!(ev, HostEvent::Joined);
    assert_eq!(host.ack_of(addr(1)), Some(12));
}

fn ack_relay(host: &mut SpectateHost, next: u32, relay: RelayState, admit_ack: u16, now: u32) {
    let m = Message::SpectateAck {
        nonce: 1,
        next,
        relay,
        admit_ack,
        cookie: host.cookie_of(addr(1)),
        sack: Bits512::EMPTY,
    };
    host.on_datagram(addr(1), &to_host(&m), now, &mut Sink::default());
}

#[test]
fn the_host_keeps_the_tree_fields_of_the_join_and_the_last_ack() {
    let mut host = host_after(64, &rematch_order(), 19);
    let tree = JoinTree {
        relay_public: Some([7; 32]),
        direct_only: true,
        has_children: false,
        resume_from: None,
        nat: NatMark {
            symmetry: Symmetry::No,
            upnp: true,
        },
        epoch: 3,
        candidates: Candidates::from_addrs(&["8.8.4.4:1000".parse().unwrap()]).unwrap(),
    };
    let mut sink = Sink::default();
    host.on_datagram(addr(1), &to_host(&join_with(1, tree)), 0, &mut sink);
    let view = |relay, admit_ack| {
        Some(TreeView {
            join: tree,
            relay,
            admit_ack,
        })
    };
    assert_eq!(host.tree_of(addr(1)), view(RelayState::NO_INTRO, 0));
    assert_eq!(host.tree_of(addr(2)), None);
    let two = RelayState {
        descendants: 2,
        no_intro: false,
    };
    ack_relay(&mut host, 5, two, 4, 1);
    assert_eq!(host.tree_of(addr(1)), view(two, 4));
    // 遅れて着いた古いACK（ackが戻る）の値は持たない。
    ack_relay(&mut host, 3, RelayState::NO_INTRO, 9, 2);
    assert_eq!(host.tree_of(addr(1)), view(two, 4));
    let one = RelayState {
        descendants: 1,
        no_intro: false,
    };
    ack_relay(&mut host, 5, one, 5, 3);
    assert_eq!(host.tree_of(addr(1)), view(one, 5));
}

#[test]
fn the_watcher_joins_without_tree_fields_but_its_nat_mark() {
    let mut w = watcher(1, &[HOST]);
    let mut sink = Sink::default();
    w.poll(0, &mut sink);
    let joined = |sink: &mut Sink| match sink.take().as_slice() {
        [(_, Message::SpectateJoin(j))] => j.tree,
        other => panic!("JOIN 1 通でない: {other:?}"),
    };
    assert_eq!(joined(&mut sink), JoinTree::NONE);
    let nat = NatMark {
        symmetry: Symmetry::Yes,
        upnp: false,
    };
    w.set_nat(nat);
    w.poll(JOIN_RESEND_MS, &mut sink);
    assert_eq!(
        joined(&mut sink),
        JoinTree {
            nat,
            ..JoinTree::NONE
        }
    );
}
