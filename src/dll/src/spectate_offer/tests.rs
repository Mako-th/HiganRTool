//! 配る側（`spectate_offer.rs`）の検査。部品は毎回の回、観戦用のルームとの往復（loopback の偽サーバーと本物のスレッド。実時間で数秒）は
//! `#[ignore]`（全件の回 `build.bat` が `--ignored` で走らせる）。外へは接続しない（`127.0.0.1` と `Proxy::Direct`）。ルーム名・アドレス・コードは合成の値だけ。
use super::*;

use std::time::{Duration, Instant};

use crate::net_room::Base;
use crate::net_room::fake::FakeServer;
use crate::room_win::{Proxy, Timeouts};

fn lo(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// ゲーム内のメニューの経路の設定（`code_side = game`、`[spectate] enable = 1`）。`extra` は `[netplay]` に足す行。
fn settings(extra: &str) -> NetSettings {
    crate::net_config::from_text(&format!(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\nname = host\n{extra}[spectate]\nenable = 1\n"
    ))
    .unwrap()
    .unwrap()
}

fn room_opts(base: &str) -> RoomOptions {
    RoomOptions {
        base: Base::parse(base).unwrap(),
        proxy: Proxy::Direct,
        timeouts: Timeouts {
            resolve_ms: 2_000,
            connect_ms: 2_000,
            send_ms: 2_000,
            response_ms: 2_000,
            close_wait_ms: 500,
        },
    }
}

const SESSION: u32 = 0x5EED_0B01;
const SECRET: Secret = Secret([0x21; 16]);

fn armed(s: &NetSettings, mapped: Option<SocketAddr>) -> Armed {
    armed_v6(s, mapped, None)
}

fn armed_v6(s: &NetSettings, mapped: Option<SocketAddr>, v6: Option<SocketAddr>) -> Armed {
    arm_with(
        s,
        lo(47_001),
        mapped,
        v6,
        None,
        Some(room_opts("ws://127.0.0.1:9")),
        || Ok(SESSION),
        || Ok(SECRET),
    )
    .unwrap()
}

/// 期限 `expires` の観戦用のコードへの返し（返しの期限はコードの期限）。
fn reply_text(session: u32, secret: Secret, cands: &[SocketAddr], expires: u64) -> String {
    let spec = Code {
        side: code::Side::Spectate,
        session_id: session,
        secret: Some(secret),
        name: Name::new("host"),
        nat: higanr_core::nat::NatMark::UNKNOWN,
        candidates: Candidates::from_addrs(&[lo(47_001)]).unwrap(),
        expires,
    };
    code::write_spectate_reply_to(
        &spec,
        Name::new("viewer"),
        Candidates::from_addrs(cands).unwrap(),
    )
    .unwrap()
}

fn runner(s: &NetSettings) -> (Runner, Arc<Shared>) {
    let a = armed(s, None);
    let shared = Arc::clone(a.kit.shared());
    let room = RoomName::parse("synth-room").unwrap().spectate_of();
    (Runner::new(a.kit, room, None), shared)
}

const V4_ONLY: Families = Families {
    v4: true,
    v6: false,
};
const V6_ONLY: Families = Families {
    v4: false,
    v6: true,
};
const BOTH: Families = Families { v4: true, v6: true };

#[test]
fn punch_candidates_keep_only_the_in_scope_addresses_of_the_sockets_families() {
    let far: SocketAddr = "203.0.113.5:2000".parse().unwrap();
    let v6: SocketAddr = "[2606:4700::1]:3000".parse().unwrap();
    let all = Candidates::from_addrs(&[lo(1_000), far, v6]).unwrap();
    let addrs = |c: Candidates| c.as_slice().iter().map(|x| x.addr).collect::<Vec<_>>();
    let has = |c: &[SocketAddr], a: SocketAddr| c.contains(&a);
    let r = addrs(punch_candidates(&all, Scope::Loopback, BOTH));
    assert_eq!(
        r,
        vec![lo(1_000)],
        "範囲 loopback は同じ PC のアドレスだけ（範囲の外の IPv6 へは穴あけを送らない）"
    );
    let r = addrs(punch_candidates(&all, Scope::Internet, V4_ONLY));
    assert!(has(&r, lo(1_000)) && has(&r, far) && !has(&r, v6), "{r:?}");
    assert_eq!(
        r.len(),
        2,
        "IPv6 のソケットが無ければ IPv6 の候補を残さない"
    );
    let r = addrs(punch_candidates(&all, Scope::Internet, V6_ONLY));
    assert_eq!(r, vec![v6], "IPv6 のソケットだけなら IPv6 だけ");
    let r = addrs(punch_candidates(&all, Scope::Internet, BOTH));
    assert!(
        has(&r, lo(1_000)) && has(&r, far) && has(&r, v6) && r.len() == 3,
        "両方のソケットがあれば全部: {r:?}"
    );
    let only_far = Candidates::from_addrs(&[far]).unwrap();
    assert!(punch_candidates(&only_far, Scope::Loopback, BOTH).is_empty());
    let lan_v6: SocketAddr = "[fd00::5]:3000".parse().unwrap();
    let only_v6 = Candidates::from_addrs(&[v6, lan_v6]).unwrap();
    let r = addrs(punch_candidates(&only_v6, Scope::Lan, BOTH));
    assert_eq!(r, vec![lan_v6], "範囲 lan は世界に通じる IPv6 を残さない");
}

#[test]
fn the_offer_and_watch_codes_carry_an_ipv6_candidate_only_when_a_v6_socket_exists() {
    let on = settings("");
    let v6: SocketAddr = "[2606:4700::10]:47001".parse().unwrap();
    let addrs = |c: &Candidates| c.as_slice().iter().map(|x| x.addr).collect::<Vec<_>>();
    let a = armed_v6(&on, None, Some(v6));
    assert!(
        addrs(&a.kit.candidates).contains(&v6),
        "観戦用のルームへ預けるコードに IPv6 が無い: {:?}",
        addrs(&a.kit.candidates)
    );
    assert!(
        addrs(&a.oneway.candidates).contains(&v6),
        "観戦コードに IPv6 が無い"
    );
    assert_eq!(a.kit.families, BOTH);
    // 読み戻した観戦用のコードにも載っている（コードの形式が観戦でも IPv6 を運べる）。
    let text = a.oneway.write(5_000_000).unwrap();
    let read = code::read_spectate(&text, 5_000_000).unwrap();
    assert!(addrs(&read.code.candidates).contains(&v6));
    // IPv6 のソケットが無ければ載せない。
    let a = armed_v6(&on, None, None);
    assert!(addrs(&a.kit.candidates).iter().all(SocketAddr::is_ipv4));
    assert!(addrs(&a.oneway.candidates).iter().all(SocketAddr::is_ipv4));
    assert_eq!(a.kit.families, V4_ONLY);
    // 世界に通じない IPv6（文書用・リンクローカル）は、ソケットがあっても載せない（対戦のコードと同じ判定）。
    for bad in ["[2001:db8::1]:47001", "[fe80::1]:47001"] {
        let a = armed_v6(&on, None, Some(bad.parse().unwrap()));
        assert!(
            addrs(&a.kit.candidates).iter().all(SocketAddr::is_ipv4),
            "{bad}"
        );
    }
}

#[test]
fn arming_follows_the_link_so_a_host_both_allow_keeps_the_mapping() {
    use higanr_core::lockstep::Role as Seat;
    let link = Arc::new(LinkSlot::new());
    let (a, note) = arm(&settings(""), lo(47_001), None, None, None, None, &link);
    let a = a.expect(&note);
    let offer = &a.kit.shared().offer;
    assert!(!offer.keeps_mapping(), "接続の前");
    link.mark_may_list(true);
    link.mark_role(Seat::Guest);
    assert!(!offer.keeps_mapping(), "ゲスト");
    link.mark_role(Seat::Host);
    assert!(offer.keeps_mapping(), "2 人とも許可したホスト");
    link.mark_may_list(false);
    assert!(!offer.keeps_mapping(), "片方が禁止したホスト");
}

/// 候補が空になる設定（STUN の答えが無い `candidates = mapped`）で用意に失敗したとき、句が簡潔にも出る形になる。
#[test]
fn an_arm_failure_note_is_seen_as_a_problem() {
    let s = settings("scope = internet\ncandidates = mapped\nstun = stun.example.invalid:3478\n");
    let link = Arc::new(LinkSlot::new());
    let (a, note) = arm(&s, lo(47_001), None, None, None, None, &link);
    assert!(a.is_none(), "候補が空なのに用意できた");
    assert!(
        note.starts_with(" / 観戦は使いません: ") && spectate_host::note_has_problem(&note),
        "{note}"
    );
}

#[test]
fn arming_needs_the_spectate_section_and_draws_fresh_values() {
    let off = crate::net_config::from_text(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\n",
    )
    .unwrap()
    .unwrap();
    let link = Arc::new(LinkSlot::new());
    let (a, note) = arm(
        &off,
        lo(47_001),
        None,
        None,
        None,
        Some(room_opts("ws://127.0.0.1:9")),
        &link,
    );
    assert!(a.is_none() && note.is_empty(), "{note}");
    let on = settings("");
    // ルームサーバーが無くても用意する（観戦コードの経路だけ）。配る側のスレッドは作れない。
    let (a, note) = arm(&on, lo(47_001), None, None, None, None, &link);
    let a = a.expect(&note);
    assert!(note.contains("観戦用のルームへは入りません"), "{note}");
    let room = RoomName::parse("synth-room").unwrap();
    let err = spawn(a.kit, &room, None).unwrap_err();
    assert!(err.contains("room_server"), "{err}");
    let (a, note) = arm(
        &on,
        lo(47_001),
        None,
        None,
        None,
        Some(room_opts("ws://127.0.0.1:9")),
        &link,
    );
    assert!(a.is_some(), "{note}");
    assert!(
        note.contains("配信側を用意しました") && !note.contains("127.0.0.1"),
        "{note}"
    );
    let mapped: SocketAddr = "203.0.113.7:40000".parse().unwrap();
    let a = armed(&on, Some(mapped));
    let got: Vec<SocketAddr> = a.kit.candidates.as_slice().iter().map(|c| c.addr).collect();
    let punch = SocketAddr::from(([203, 0, 113, 7], 47_001));
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(
        got.contains(&lo(47_001)) && got.contains(&mapped) && got.contains(&punch),
        "{got:?}"
    );
    assert_eq!((a.kit.session_id, a.kit.secret), (SESSION, SECRET));
    assert_eq!(a.parts.sender.session_id(), SESSION);
    assert!(a.kit.shared().offer.counts().armed);
    let mut n = 0u32;
    let mut draw = || {
        n += 1;
        Ok(n)
    };
    let a1 = arm_with(
        &on,
        lo(47_001),
        None,
        None,
        None,
        Some(room_opts("ws://127.0.0.1:9")),
        &mut draw,
        || Ok(Secret([1; 16])),
    )
    .unwrap();
    let a2 = arm_with(
        &on,
        lo(47_001),
        None,
        None,
        None,
        Some(room_opts("ws://127.0.0.1:9")),
        &mut draw,
        || Ok(Secret([2; 16])),
    )
    .unwrap();
    assert_ne!(a1.kit.session_id, a2.kit.session_id);
    assert_ne!(a1.kit.secret, a2.kit.secret);
    assert!(!Arc::ptr_eq(&a1.parts.ring, &a2.parts.ring));
}

#[test]
fn only_a_reply_with_the_right_tag_session_and_expiry_asks_for_a_punch() {
    let (mut r, shared) = runner(&settings(""));
    let now = 5_000_000u64;
    let far: SocketAddr = "203.0.113.5:2000".parse().unwrap();
    let gen0 = shared.punch_generation();
    r.punch(
        &reply_text(SESSION, SECRET, &[lo(41_000), far], now + 600),
        now,
    );
    assert_eq!(shared.punch_generation(), gen0 + 1, "正しい返しで頼まない");
    let asked = shared.peek_punch().expect("頼みが置かれている");
    assert_eq!(
        asked.as_slice().iter().map(|c| c.addr).collect::<Vec<_>>(),
        vec![lo(41_000)],
        "頼みは範囲で絞った候補"
    );
    let c = shared.get().offer;
    assert_eq!((c.replies, c.punch_requested), (1, 1));
    let bad = [
        reply_text(SESSION, Secret([0x77; 16]), &[lo(41_001)], now + 600),
        reply_text(SESSION ^ 1, SECRET, &[lo(41_002)], now + 600),
        reply_text(SESSION, SECRET, &[lo(41_003)], now - 1),
        "not-a-code!".to_string(),
        reply_text(SESSION, SECRET, &[far], now + 600),
    ];
    for t in &bad {
        r.punch(t, now);
    }
    assert_eq!(
        shared.punch_generation(),
        gen0 + 1,
        "悪い返しで頼みが立った"
    );
    let c = shared.get().offer;
    assert_eq!(
        (
            c.replies,
            c.bad_tag,
            c.wrong_session,
            c.expired,
            c.unreadable,
            c.no_route,
            c.punch_requested
        ),
        (6, 1, 1, 1, 1, 1, 1),
        "{c:?}"
    );
}

/// 観戦者の PC の時計がずれていても、すぐ出した返しはホストの時計で通る（返しの期限はホストのコードの期限）。
#[test]
fn a_reply_from_a_viewer_with_a_skewed_clock_still_asks_for_a_punch() {
    let host_now = 5_000_000u64;
    let a = armed(&settings(""), None);
    let shared = Arc::clone(a.kit.shared());
    let host_code = a.oneway.write(host_now).unwrap();
    let mut r = Runner::new(
        a.kit,
        RoomName::parse("synth-room").unwrap().spectate_of(),
        None,
    );
    for skew in [-600i64, 0, 600] {
        let viewer_now = host_now.checked_add_signed(skew).unwrap();
        // 観戦者の側の手順（`spectate_join` と同じ: 自分の時計でコードを読み、返しを書く）。
        let spec = code::read_spectate(&host_code, viewer_now).unwrap();
        let reply = code::write_spectate_reply_to(
            &spec.code,
            Name::new("viewer"),
            Candidates::from_addrs(&[lo(41_200)]).unwrap(),
        )
        .unwrap();
        let before = shared.punch_generation();
        r.punch(&reply, host_now + 1);
        assert_eq!(
            shared.punch_generation(),
            before + 1,
            "時計が {skew} 秒ずれた観戦者の返しで穴あけを頼まない: {:?}",
            shared.get().offer
        );
        // ホストのコードの期限が本当に過ぎた後の返しは捨てる。
        let expired = shared.get().offer.expired;
        r.punch(&reply, host_now + code::DEFAULT_TTL_SECS + 1);
        assert_eq!(shared.punch_generation(), before + 1, "期限切れで頼んだ");
        assert_eq!(shared.get().offer.expired, expired + 1);
    }
}

/// `follow_mapping` を直に回す（ルームへは接続しない）。
fn remap_runner(mapped: SocketAddr, upnp: Option<u16>) -> (Runner, OnewayCode, Arc<Shared>) {
    let a = arm_with(
        &settings(""),
        lo(47_001),
        Some(mapped),
        None,
        upnp,
        Some(room_opts("ws://127.0.0.1:9")),
        || Ok(SESSION),
        || Ok(SECRET),
    )
    .unwrap();
    let shared = Arc::clone(a.kit.shared());
    let room = RoomName::parse("synth-room").unwrap().spectate_of();
    (Runner::new(a.kit, room, None), a.oneway, shared)
}

fn addrs_of(c: &Candidates) -> Vec<SocketAddr> {
    c.as_slice().iter().map(|x| x.addr).collect()
}

#[test]
fn a_moved_mapping_rebuilds_both_codes_and_the_same_mapping_does_not() {
    let first: SocketAddr = "203.0.113.7:40000".parse().unwrap();
    let moved: SocketAddr = "203.0.113.7:40031".parse().unwrap();
    let other_ip: SocketAddr = "198.51.100.4:40031".parse().unwrap();
    let (mut r, oneway, shared) = remap_runner(first, Some(50_000));
    let before = addrs_of(&r.kit.candidates);
    assert!(before.contains(&first), "{before:?}");
    let oneway_addrs = |o: &OnewayCode| {
        addrs_of(
            &code::read_spectate(&o.write(5_000_000).unwrap(), 5_000_000)
                .unwrap()
                .code
                .candidates,
        )
    };
    // 聞き直しの答えが最初と同じなら作り直さない。
    shared.offer.note_mapped(first);
    r.follow_mapping(0);
    assert_eq!(shared.get().offer.remapped, 0);
    assert_eq!(addrs_of(&r.kit.candidates), before);
    // ポートが動いた: 預ける候補と観戦コードの候補が新しいポートになる。
    shared.offer.note_mapped(moved);
    r.follow_mapping(10);
    let now = addrs_of(&r.kit.candidates);
    assert!(now.contains(&moved) && !now.contains(&first), "{now:?}");
    assert_eq!(shared.get().offer.remapped, 1);
    let w = oneway_addrs(&oneway);
    assert!(w.contains(&moved) && !w.contains(&first), "{w:?}");
    assert!(
        w.contains(&"203.0.113.7:50000".parse().unwrap()),
        "観戦コードの UPnP の候補が落ちた: {w:?}"
    );
    assert_eq!(oneway.candidates_len(), w.len());
    // 同じ答えが続いても作り直さない。
    for t in [20, 30, 40] {
        r.follow_mapping(t);
    }
    assert_eq!(shared.get().offer.remapped, 1);
    // アドレスが変わると UPnP の候補もそのアドレスへ付く。
    shared.offer.note_mapped(other_ip);
    r.follow_mapping(50);
    assert_eq!(shared.get().offer.remapped, 2);
    let now = addrs_of(&r.kit.candidates);
    assert!(now.contains(&other_ip), "{now:?}");
    assert!(now.iter().all(|a| a.ip() != first.ip()), "{now:?}");
    let w = oneway_addrs(&oneway);
    assert!(
        w.contains(&"198.51.100.4:50000".parse().unwrap())
            && w.iter().all(|a| a.ip() != first.ip()),
        "{w:?}"
    );
    // 最初のアドレスへ戻れば、候補も最初と同じ並びへ戻る。
    shared.offer.note_mapped(first);
    r.follow_mapping(60);
    assert_eq!(shared.get().offer.remapped, 3);
    assert_eq!(addrs_of(&r.kit.candidates), before);
    // IPv6 の答えは受けない（NAT の付け替えは IPv4 の話）。
    shared
        .offer
        .note_mapped("[2606:4700::1]:4000".parse().unwrap());
    assert_eq!(shared.offer.mapped(), Some(first));
}

#[test]
fn the_snapshot_line_shows_the_offer_part_only_when_armed_and_never_an_address_or_room() {
    let plain = spectate_host::Snapshot::default();
    assert!(!plain.text().contains("配信側"), "{}", plain.text());
    let (r, shared) = runner(&settings(""));
    r.stats().bump(Stat::Entered);
    r.stats().bump(Stat::Offered);
    r.stats().bump(Stat::FailRate);
    r.stats().bump(Stat::Remapped);
    r.stats().bump(Stat::Remapped);
    let text = shared.get().text();
    assert!(
        text.contains("配信側")
            && text.contains("入った 1")
            && text.contains("預けた 1")
            && text.contains("外側のアドレスの変化で作り直し 2")
            && text.contains("混雑 1"),
        "{text}"
    );
    assert!(text.contains("未作成"), "{text}");
    for secret_like in ["127.0.0.1", "synth-room", "w."] {
        assert!(
            !text.contains(secret_like),
            "{secret_like:?} が行に出た: {text}"
        );
    }
}

#[test]
fn stopping_without_a_thread_returns_at_once_and_marks_stopped() {
    let shared = Shared::default();
    assert!(!shared.offer.stopped());
    let t = Instant::now();
    shared.stop_offer();
    assert!(t.elapsed() < Duration::from_millis(500));
    assert!(shared.offer.stopped());
    assert!(!shared.offer.counts().running);
}

fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < limit, "{limit:?} の内に {what} にならない");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 観戦者の役（ルームの guest）で入り、配る側の観戦用のコードを受けて、`make` で作った返しを answer する。サーバーが 1000 で閉じるまで回す。
fn viewer_answers(base: &str, room: &RoomName, make: impl FnOnce(&Code) -> String) -> Code {
    let mut c = RoomConn::start(&room_opts(base), room).unwrap();
    let t0 = Instant::now();
    let mut spec = None;
    let mut make = Some(make);
    let mut done = false;
    let mut joined_as_answer = false;
    while !done {
        for e in c.poll() {
            match e {
                Event::Joined { role, .. } => joined_as_answer = role == Role::Answer,
                Event::PeerCode(text) => {
                    let code = code::read_spectate(&text, crate::net_udp::now_unix())
                        .expect("配る側の観戦用のコードが読める")
                        .code;
                    let reply = make.take().expect("1 回だけ")(&code);
                    c.submit_code(&reply).expect("返しを預けられる");
                    spec = Some(code);
                }
                Event::Done => done = true,
                Event::Failed(f) => panic!("観戦者のルームの手順が失敗した: {f:?}"),
                Event::Matched { .. } => {}
            }
        }
        assert!(
            t0.elapsed() < Duration::from_secs(8),
            "観戦者の手順が終わらない"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        joined_as_answer,
        "観戦者はルームの後から入った側（guest）のはず"
    );
    spec.expect("観戦用のコードを受けた")
}

/// 配る側のスレッドの組み上がり（本物のスレッド ＋ 偽のルームサーバー ＋ 本物の WinHTTP）。観戦用のルーム（元のルーム名のハッシュ）だけへ入り、サーバーの 1000 の後に入り直し、止めたら終わる。
#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（入り直しの 2 秒を 2 回含む。十数秒）。全件の回（build.bat）で --ignored で走る"]
fn the_offer_thread_serves_one_viewer_per_entry_and_rejoins_after_each() {
    let srv = FakeServer::start();
    let base = srv.base();
    let s = settings(&format!("room_server = {base}\n"));
    let a = arm_with(
        &s,
        lo(47_002),
        None,
        None,
        None,
        Some(room_opts(&base)),
        || Ok(SESSION),
        || Ok(SECRET),
    )
    .unwrap();
    let shared = Arc::clone(a.kit.shared());
    let match_room = RoomName::parse("synth-spec-room").unwrap();
    let spec_room = match_room.spectate_of();
    spawn(a.kit, &match_room, None).unwrap();
    assert!(shared.get().offer.running);
    wait_until(
        "観戦用のコードの預け",
        Duration::from_secs(5),
        || shared.get().offer.offered >= 1,
    );
    let gen0 = shared.punch_generation();
    let spec1 = viewer_answers(&base, &spec_room, |spec| {
        code::write_spectate_reply_to(
            spec,
            Name::new("viewer"),
            Candidates::from_addrs(&[lo(41_100)]).unwrap(),
        )
        .unwrap()
    });
    assert_eq!(
        (spec1.session_id, spec1.secret),
        (SESSION, Some(SECRET)),
        "預けたのは観戦用の session・秘密のコード"
    );
    assert_eq!(
        spec1.candidates.as_slice()[0].addr,
        lo(47_002),
        "候補は自分の受信ソケット"
    );
    wait_until("穴あけの頼み", Duration::from_secs(5), || {
        shared.punch_generation() > gen0
    });
    assert_eq!(
        shared.peek_punch().unwrap().as_slice()[0].addr,
        lo(41_100),
        "頼みは観戦者の候補"
    );
    wait_until("入り直し", Duration::from_secs(8), || {
        shared.get().offer.entered >= 2 && shared.get().offer.offered >= 2
    });
    let gen1 = shared.punch_generation();
    viewer_answers(&base, &spec_room, |spec| {
        let forged = Code {
            secret: Some(Secret([0x66; 16])),
            ..*spec
        };
        code::write_spectate_reply_to(
            &forged,
            Name::new("forger"),
            Candidates::from_addrs(&[lo(41_101)]).unwrap(),
        )
        .unwrap()
    });
    wait_until("タグの数え", Duration::from_secs(5), || {
        shared.get().offer.bad_tag >= 1
    });
    assert_eq!(shared.punch_generation(), gen1, "偽の返しで穴あけを頼んだ");
    wait_until("入り直し 2", Duration::from_secs(8), || {
        shared.get().offer.entered >= 3 && shared.get().offer.offered >= 3
    });
    let far: SocketAddr = "203.0.113.9:4000".parse().unwrap();
    viewer_answers(&base, &spec_room, |spec| {
        code::write_spectate_reply_to(
            spec,
            Name::new("far"),
            Candidates::from_addrs(&[far]).unwrap(),
        )
        .unwrap()
    });
    wait_until("範囲外の数え", Duration::from_secs(5), || {
        shared.get().offer.no_route >= 1
    });
    assert_eq!(
        shared.punch_generation(),
        gen1,
        "範囲の外だけの返しで頼んだ"
    );
    let c = shared.get().offer;
    assert_eq!(
        (c.replies, c.punch_requested, c.bad_tag, c.no_route),
        (3, 1, 1, 1),
        "{c:?}"
    );
    assert!(c.done >= 3, "観戦者 3 人ぶん 1000 を受けた: {c:?}");
    let want = format!("/v0/rooms/{}", spec_room.as_str());
    let paths = srv.seen.paths.lock().unwrap().clone();
    assert!(
        !paths.is_empty() && paths.iter().all(|p| p.starts_with(&want)),
        "{paths:?}"
    );
    assert!(
        paths.iter().all(|p| !p.contains("synth-spec-room")),
        "元のルーム名で入った: {paths:?}"
    );
    let text = shared.get().text();
    assert!(
        !text.contains("synth-spec-room") && !text.contains(spec_room.as_str()),
        "{text}"
    );
    let t = Instant::now();
    shared.stop_offer();
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "止めるのに {:?}",
        t.elapsed()
    );
    assert!(!shared.get().offer.running);
    let ups = srv.seen.upgrades.load(Ordering::Acquire);
    std::thread::sleep(Duration::from_millis(2_600));
    assert_eq!(
        srv.seen.upgrades.load(Ordering::Acquire),
        ups,
        "止めた後に入り直した"
    );
}

/// 預けたコードを誰も受け取っていない間に外側のポートが動いたら、入り直して新しいポートのコードを預ける。同じアドレスなら入り直さない。
#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_moved_mapping_enters_again_and_the_viewer_gets_the_new_port() {
    let srv = FakeServer::start();
    let base = srv.base();
    let s = settings(&format!("room_server = {base}\n"));
    let first: SocketAddr = "203.0.113.7:40000".parse().unwrap();
    let moved: SocketAddr = "203.0.113.7:40031".parse().unwrap();
    let a = arm_with(
        &s,
        lo(47_004),
        Some(first),
        None,
        None,
        Some(room_opts(&base)),
        || Ok(SESSION),
        || Ok(SECRET),
    )
    .unwrap();
    let shared = Arc::clone(a.kit.shared());
    let spec_room = RoomName::parse("synth-remap-room").unwrap().spectate_of();
    let mut r = Runner::new(a.kit, spec_room, None);
    let t0 = Instant::now();
    let run_until = |r: &mut Runner, what: &str, limit: Duration, cond: &dyn Fn() -> bool| {
        let t1 = Instant::now();
        while !cond() {
            assert!(t1.elapsed() < limit, "{limit:?} の内に {what} にならない");
            r.step(
                u64::try_from(t0.elapsed().as_millis()).unwrap(),
                crate::net_udp::now_unix(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    run_until(&mut r, "1 回目の預け", Duration::from_secs(5), &|| {
        shared.get().offer.offered >= 1
    });
    let ups = srv.seen.upgrades.load(Ordering::Acquire);
    shared.offer.note_mapped(first);
    let quiet = Instant::now();
    run_until(
        &mut r,
        "同じアドレスの見回り",
        Duration::from_secs(2),
        &|| quiet.elapsed() > Duration::from_millis(400),
    );
    assert_eq!(
        srv.seen.upgrades.load(Ordering::Acquire),
        ups,
        "同じアドレスで入り直した"
    );
    assert_eq!(shared.get().offer.remapped, 0);
    shared.offer.note_mapped(moved);
    run_until(
        &mut r,
        "入り直しの預け",
        Duration::from_secs(10),
        &|| shared.get().offer.offered >= 2,
    );
    assert_eq!(shared.get().offer.remapped, 1);
    let spec = viewer_answers(&base, &spec_room, |spec| {
        code::write_spectate_reply_to(
            spec,
            Name::new("viewer"),
            Candidates::from_addrs(&[lo(41_300)]).unwrap(),
        )
        .unwrap()
    });
    let got = addrs_of(&spec.candidates);
    assert!(got.contains(&moved) && !got.contains(&first), "{got:?}");
    r.close();
}

/// 観戦者がコードを受け取った（`matched` が届いている）同じ見回りで外側のポートが動いても、閉じずにその手順を終えさせ、次の入室で新しいポートのコードを預ける。
#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（入り直しの 2 秒を含む。数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_moved_mapping_lets_a_viewer_who_got_the_code_finish() {
    let srv = FakeServer::start();
    let base = srv.base();
    let s = settings(&format!("room_server = {base}\n"));
    let first: SocketAddr = "203.0.113.7:40000".parse().unwrap();
    let moved: SocketAddr = "203.0.113.7:40031".parse().unwrap();
    let a = arm_with(
        &s,
        lo(47_005),
        Some(first),
        None,
        None,
        Some(room_opts(&base)),
        || Ok(SESSION),
        || Ok(SECRET),
    )
    .unwrap();
    let shared = Arc::clone(a.kit.shared());
    let spec_room = RoomName::parse("synth-remap-matched")
        .unwrap()
        .spectate_of();
    let mut r = Runner::new(a.kit, spec_room, None);
    let t0 = Instant::now();
    let ms = || u64::try_from(t0.elapsed().as_millis()).unwrap();
    while shared.get().offer.offered < 1 {
        assert!(t0.elapsed() < Duration::from_secs(5), "預けない");
        r.step(ms(), crate::net_udp::now_unix());
        std::thread::sleep(Duration::from_millis(20));
    }
    // 観戦者が入ってコードを受け取る。配る側は回さないので、`matched` は配る側の接続に届いたまま読まれていない。
    let mut v = RoomConn::start(&room_opts(&base), &spec_room).unwrap();
    let mut spec = None;
    let t1 = Instant::now();
    while spec.is_none() {
        for e in v.poll() {
            match e {
                Event::PeerCode(text) => {
                    spec = Some(
                        code::read_spectate(&text, crate::net_udp::now_unix())
                            .unwrap()
                            .code,
                    );
                }
                Event::Failed(f) => panic!("観戦者のルームの手順が失敗した: {f:?}"),
                _ => {}
            }
        }
        assert!(t1.elapsed() < Duration::from_secs(5), "コードが届かない");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(200));
    shared.offer.note_mapped(moved);
    r.step(ms(), crate::net_udp::now_unix());
    assert_eq!(shared.get().offer.remapped, 1);
    assert_eq!(
        r.caster.status(ms()).phase,
        higanr_core::spectate_room::CasterPhase::Casting,
        "観戦者が受け取った後に閉じた"
    );
    assert!(r.conn.is_some());
    let reply = code::write_spectate_reply_to(
        &spec.unwrap(),
        Name::new("viewer"),
        Candidates::from_addrs(&[lo(41_400)]).unwrap(),
    )
    .unwrap();
    v.submit_code(&reply).unwrap();
    let mut done = false;
    while !done {
        for e in v.poll() {
            match e {
                Event::Done => done = true,
                Event::Failed(f) => panic!("観戦者の手順が壊れた: {f:?}"),
                _ => {}
            }
        }
        r.step(ms(), crate::net_udp::now_unix());
        assert!(
            t1.elapsed() < Duration::from_secs(10),
            "観戦者の手順が終わらない"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // 配る側も同じ接続で返しを受け、穴あけを頼んで手順を終える。
    while shared.get().offer.done < 1 {
        assert!(
            t1.elapsed() < Duration::from_secs(10),
            "配る側が手順を終えない: {:?}",
            shared.get().offer
        );
        r.step(ms(), crate::net_udp::now_unix());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(shared.get().offer.punch_requested, 1);
    while shared.get().offer.offered < 2 {
        assert!(t1.elapsed() < Duration::from_secs(15), "入り直さない");
        r.step(ms(), crate::net_udp::now_unix());
        std::thread::sleep(Duration::from_millis(20));
    }
    let next = viewer_answers(&base, &spec_room, |spec| {
        code::write_spectate_reply_to(
            spec,
            Name::new("viewer2"),
            Candidates::from_addrs(&[lo(41_401)]).unwrap(),
        )
        .unwrap()
    });
    let got = addrs_of(&next.candidates);
    assert!(got.contains(&moved) && !got.contains(&first), "{got:?}");
    r.close();
}

// ---------------- 進行中のランダム対戦の一覧に載せる ----------------

const LIST_BUILD: u32 = 7;
const Q_NAME: &str = "q.0123456789abcdef0123456789abcd";

fn listing_info(cap: u8) -> ListingInfo {
    ListingInfo {
        build: LIST_BUILD,
        posting: Posting {
            room: higanr_core::room_queue::AssignedRoom::parse(Q_NAME).unwrap(),
            p1: higanr_core::watch_list::WatchName::parse("alice").unwrap(),
            p2: higanr_core::watch_list::WatchName::parse("bob").unwrap(),
            mode: higanr_core::wire::Mode::Rollback,
            delay: 1,
            cap,
        },
    }
}

/// 一覧に載せる配る側（観戦用のルームの名は割り当ての名から導く）。
fn listing_runner(srv: &FakeServer, listing: Option<ListingInfo>) -> (Runner, Arc<Shared>) {
    let base = srv.base();
    let s = settings(&format!("room_server = {base}\n"));
    let a = arm_with(
        &s,
        lo(47_003),
        None,
        None,
        None,
        Some(room_opts(&base)),
        || Ok(SESSION),
        || Ok(SECRET),
    )
    .unwrap();
    let shared = Arc::clone(a.kit.shared());
    let q = RoomName::parse_server_rules(Q_NAME).unwrap();
    (Runner::new(a.kit, q.spectate_of(), listing), shared)
}

/// 見回りを実時間で 20 ms ごとに回して、条件が整うまで待つ。
fn step_until(r: &mut Runner, what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < limit, "{limit:?} の内に {what} にならない");
        r.step(
            u64::try_from(t0.elapsed().as_millis()).unwrap(),
            crate::net_udp::now_unix(),
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_random_host_lists_the_match_follows_the_spectator_count_and_leaves_when_stopped() {
    let srv = FakeServer::start();
    let (mut r, shared) = listing_runner(&srv, Some(listing_info(10)));
    let want = RoomName::parse_server_rules(Q_NAME).unwrap().spectate_of();
    step_until(&mut r, "一覧に載る", Duration::from_secs(5), || {
        !srv.watch_items(LIST_BUILD).is_empty()
    });
    let items = srv.watch_items(LIST_BUILD);
    assert_eq!(
        items,
        vec![crate::net_room::fake::ListedItem {
            room: want.as_str().to_owned(),
            p1: "alice".into(),
            p2: "bob".into(),
            mode: "rollback".into(),
            delay: 1,
            watchers: 0,
            cap: 10,
        }]
    );
    assert_eq!(
        items[0].room, "w.604d502a3981065f",
        "割り当ての名から導いた観戦用のルーム名"
    );
    assert!(!items[0].room.contains("q."));
    let paths = srv.seen.paths.lock().unwrap().clone();
    assert!(
        paths.contains(&format!("/v0/watch/{LIST_BUILD}/post")),
        "載せるエンドポイントのパスにビルド番号: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p == &format!("/v0/rooms/{Q_NAME}")),
        "割り当てのルームへ入った: {paths:?}"
    );
    step_until(
        &mut r,
        "載ったと数える",
        Duration::from_secs(5),
        || shared.get().offer.listed == 1,
    );
    // 観戦者の数に付いていく（上限を越える数は上限に収める）。
    for (n, want) in [(3u32, 3i64), (0, 0), (99, 10), (1, 1)] {
        shared.set_spectators(n);
        step_until(
            &mut r,
            "観戦者の数が一覧に届く",
            Duration::from_secs(5),
            || {
                srv.watch_items(LIST_BUILD)
                    .first()
                    .is_some_and(|i| i.watchers == want)
            },
        );
    }
    assert_eq!(shared.get().offer.list_failed, 0);
    // 止めると一覧から消える。
    r.close();
    wait_until("一覧から消える", Duration::from_secs(5), || {
        srv.watch_items(LIST_BUILD).is_empty()
    });
}

#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_host_without_listing_info_never_touches_the_list() {
    let srv = FakeServer::start();
    let (mut r, shared) = listing_runner(&srv, None);
    // 観戦用のルームへは入る（配る側は動く）。一覧のエンドポイントには触らない。
    step_until(
        &mut r,
        "観戦用のルームへ入る",
        Duration::from_secs(5),
        || shared.get().offer.entered >= 1,
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(srv.seen.watch_post_upgrades.load(Ordering::Acquire), 0);
    let paths = srv.seen.paths.lock().unwrap().clone();
    assert!(
        !paths.iter().any(|p| p.starts_with("/v0/watch/")),
        "{paths:?}"
    );
    assert_eq!(shared.get().offer.listed, 0);
    r.close();
}

#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn a_refused_listing_retries_after_a_growing_delay_and_a_full_list_is_given_up() {
    let srv = FakeServer::start();
    let (mut r, shared) = listing_runner(&srv, Some(listing_info(10)));
    let posts = || srv.seen.watch_post_upgrades.load(Ordering::Acquire);
    // 件数の上限（list_full）で断られたら、入り直さない。
    srv.knobs.list_full.store(true, Ordering::Release);
    let t0 = Instant::now();
    r.step(0, 0);
    wait_until("1 回目の post", Duration::from_secs(5), || posts() == 1);
    while shared.get().offer.list_failed == 0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "断られたと数えない");
        r.step(10, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    for ms in [60_000u64, 600_000, 6_000_000] {
        r.step(ms, 0);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(posts(), 1, "件数の上限で断られたのに入り直した");
    assert!(srv.watch_items(LIST_BUILD).is_empty());
    r.close();

    // それ以外の失敗（bad_message）は、間隔を空けて入り直す（30 秒・60 秒・…）。時計は見回りに渡す値で進める。
    let srv = FakeServer::start();
    let (mut r, shared) = listing_runner(&srv, Some(listing_info(10)));
    let posts = || srv.seen.watch_post_upgrades.load(Ordering::Acquire);
    srv.knobs.list_bad.store(true, Ordering::Release);
    let mut now = 0u64;
    r.step(now, 0);
    // 失敗の出来事は次の見回りで届く。
    let t0 = Instant::now();
    while shared.get().offer.list_failed < 1 {
        assert!(t0.elapsed() < Duration::from_secs(5), "断られたと数えない");
        now += 10;
        r.step(now, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    let failed_at = now;
    for early in [failed_at + 1_000, failed_at + RELIST_FIRST_MS - 1] {
        r.step(early, 0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(posts(), 1, "間隔の前に入り直した（{early} ms）");
    }
    now = failed_at + RELIST_FIRST_MS;
    r.step(now, 0);
    wait_until("2 回目の post", Duration::from_secs(5), || posts() == 2);
    // 2 回目の失敗の後は間隔が倍になる。
    let t0 = Instant::now();
    while shared.get().offer.list_failed < 2 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "2 回目の失敗を数えない"
        );
        now += 10;
        r.step(now, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    let failed2_at = now;
    r.step(failed2_at + RELIST_FIRST_MS, 0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        posts(),
        2,
        "2 回目の失敗の後に 30 秒で入り直した（60 秒に倍になるはず）"
    );
    // 偽サーバーの一覧の断りを止めると、次の入り直しで載る。
    srv.knobs.list_bad.store(false, Ordering::Release);
    now = failed2_at + 2 * RELIST_FIRST_MS;
    r.step(now, 0);
    let t0 = Instant::now();
    while srv.watch_items(LIST_BUILD).is_empty() {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "断りを止めても載らない"
        );
        now += 10;
        r.step(now, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    // 載った知らせは次の見回りで届く。
    while shared.get().offer.listed < 1 {
        assert!(t0.elapsed() < Duration::from_secs(5), "載ったと数えない");
        now += 10;
        r.step(now, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(shared.get().offer.listed, 1);
    r.close();
}

#[test]
fn the_relist_delay_doubles_from_thirty_seconds_up_to_five_minutes() {
    let d: Vec<u64> = (1..=8).map(relist_delay).collect();
    assert_eq!(
        d,
        [
            30_000, 60_000, 120_000, 240_000, 300_000, 300_000, 300_000, 300_000
        ]
    );
    assert_eq!(relist_delay(0), RELIST_FIRST_MS);
    assert_eq!(relist_delay(u32::MAX), RELIST_MAX_MS);
}

#[test]
#[ignore = "偽サーバー・本物のスレッドと WinHTTP（loopback）で実時間に回す（数秒）。全件の回（build.bat）で --ignored で走る"]
fn an_offer_that_cannot_be_sent_ends_the_entry_and_the_caster_enters_again_later() {
    use higanr_core::spectate_room::CasterPhase;
    let srv = FakeServer::start();
    let (mut r, shared) = listing_runner(&srv, None);
    r.step(0, crate::net_udp::now_unix());
    r.conn.as_mut().expect("入っていない").fail_sends_for_test();
    let t0 = Instant::now();
    let mut now = 0;
    while shared.get().offer.fail_unresponsive == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "送りの失敗が届かない: {:?}",
            shared.get().offer
        );
        now = u64::try_from(t0.elapsed().as_millis()).unwrap();
        r.step(now, crate::net_udp::now_unix());
        std::thread::sleep(Duration::from_millis(10));
    }
    let c = shared.get().offer;
    assert_eq!((c.entered, c.offered, c.offer_failed), (1, 0, 1), "{c:?}");
    assert!(r.conn.is_none());
    let s = r.caster.status(now);
    assert_eq!(
        s.phase,
        CasterPhase::Waiting,
        "入り直しを待っていない: {s:?}"
    );
    r.close();
}

/// 開発用の `[spectate] fake_uplink` は、ゲーム内のメニューの経路の観戦のホストにも届く（書かなければ無し）。
#[test]
fn the_dev_fake_uplink_reaches_the_sender_of_the_in_game_path() {
    let with = crate::net_config::from_text(
        "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\n[spectate]\nenable = 1\nfake_uplink = 77000\n",
    )
    .unwrap()
    .unwrap();
    assert_eq!(armed(&with, None).parts.sender.fake_uplink(), Some(77_000));
    assert_eq!(armed(&settings(""), None).parts.sender.fake_uplink(), None);
}
