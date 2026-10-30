//! 候補に無い送り元を学ぶ経路（NAT越えの対策。ルーターが外側のポートを付け替えた相手に接続する）の検査。
//! 公開のAPI（`Link`と`PongResponder`）だけを使う。
//!
//! 前半は1つの`Link`（A。ゲスト側のnonce 1）に相手Bのつもりの1通ずつを手で渡し、学ぶ・学ばないのゲートを1つずつ見る。
//! 後半は2つの`Link`を偽のネットワーク（1 msごとに進む固定の模型。coreはpureなので実際のUDP・時計は使わない）で接続する。
//! 組み上がりで接続できることを見る。
//!
//! アドレス: `B_CODE`はAが持つ接続コードの中のBのアドレス（候補）。`B_REAL`はBのルーターが付け替えた実際の送り元（コードに無い）。
//! `A_ADDR`はAのアドレス（Bのコードの中のAの候補と実際が同じ）。

use std::net::SocketAddr;

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::candidate::MAX_LEARNED;
use higanr_core::handshake::{self, FailReason, Name, State};
use higanr_core::link::{Config, Link, Outlet, Recv};
use higanr_core::pong::PongResponder;
use higanr_core::seal::{DirKeys, Ephemeral, KeySeed, PUBLIC_LEN, Sender, Seq};
use higanr_core::wire::{
    self, ByeReason, HEADER_LEN, Hello, MAX_DATAGRAM, MAX_WORDS, Message, Mode, Seat,
};

const SID: u32 = 0x1A2B_3C4D;
const SECRET: Secret = Secret([0x5E; 16]);
const A_ADDR: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_001,
);
const B_CODE: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_002,
);
const B_REAL: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_102,
);

fn sa(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn hs_cfg(nonce: u64) -> handshake::Config {
    handshake::Config {
        session_id: SID,
        role_nonce: nonce,
        latency: 2,
        name: Name::new("t"),
        host_seat: Seat::P1,
    }
}

/// 相手の候補が`peer`だけのセッション。使い捨ての鍵の種はnonceから決める（本物はdllがOSの乱数で生成する）。
fn link(peer: SocketAddr, nonce: u64) -> Link {
    Link::new(
        Config::single(hs_cfg(nonce), SECRET, peer, seed_of(nonce)),
        0,
    )
}

fn seed_of(nonce: u64) -> KeySeed {
    KeySeed([(nonce as u8).wrapping_mul(13) ^ 0x6D; 32])
}

fn public_of(nonce: u64) -> [u8; PUBLIC_LEN] {
    Ephemeral::from_seed(&seed_of(nonce)).public()
}

/// 相手B（nonce 2）の側から見た向きごとの鍵（送る = B → A、受ける = A → B）。Aはnonce 1（Bの方が大きいので並びはBが前）。
fn b_keys() -> DirKeys {
    b_keys_for(1)
}

fn b_keys_for(a: u64) -> DirKeys {
    Ephemeral::from_seed(&seed_of(2))
        .dir_keys(&public_of(a), 2 > a, SID, &key())
        .expect("鍵が作れる")
}

fn key() -> AuthKey {
    AuthKey::derive(&SECRET, SID)
}

fn enc_with(key: &AuthKey, sid: u32, msg: &Message) -> Vec<u8> {
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(key, sid, msg, &mut buf);
    buf[..n].to_vec()
}

/// B（nonce 2）がAへ送る1通（HELLO・HELLO_ACKはHMACのタグ、ほかはBの鍵で封じる）。
fn enc(msg: &Message) -> Vec<u8> {
    if wire::is_signed(msg) {
        return enc_with(&key(), SID, msg);
    }
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::seal(
        &b_keys().sealer(Sender::Link),
        &Seq::new(),
        SID,
        msg,
        &mut buf,
    )
    .expect("封じられる");
    buf[..n].to_vec()
}

/// AがBへ送った1通を読む（Bの受ける向きの鍵で開く。Aのnonceは1・3・9のどれか）。
fn dec(b: &[u8]) -> Result<wire::Packet, wire::DecodeError> {
    let mut last = wire::decode(&key(), None, b);
    if last != Err(wire::DecodeError::NotKeyed) {
        return last;
    }
    for a in [1, 3, 9] {
        last = wire::decode(&key(), Some(&b_keys_for(a).opener()), b);
        if last.is_ok() {
            return last;
        }
    }
    last
}

fn hello(nonce: u64) -> Message {
    let mut name = [0u8; wire::NAME_MAX];
    name[0] = b'x';
    Message::Hello(Hello {
        proto_ver: wire::PROTO_VER,
        latency: 2,
        name_len: 1,
        mode: Mode::Delay,
        can_rollback: false,
        listable: false,
        role_nonce: nonce,
        name,
        seat_wish: wire::SeatWish::P1,
        arrival: wire::Arrival::Unknown,
        build: higanr_core::update::BUILD_NUMBER,
        public: public_of(nonce),
        host_pick: false,
        nat: higanr_core::nat::NatMark::UNKNOWN,
        rollback_delay: 0,
    })
}

/// `role`の相手がnonce `echo`のHELLO（公開鍵は`public_of(echo)`）へ返すHELLO_ACK。
fn ack(echo: u64, role: u64) -> Message {
    ack_to_key(echo, role, public_of(echo))
}

/// `echo_public`を指定する形（前のセッションの公開鍵への返答を作る）。
fn ack_to_key(echo: u64, role: u64, echo_public: [u8; PUBLIC_LEN]) -> Message {
    Message::HelloAck {
        echo_nonce: echo,
        role_nonce: role,
        public: public_of(role),
        echo_public,
    }
}

fn agree() -> Message {
    Message::Agree {
        host_seat: Seat::P1,
        delay: 2,
        mode: Mode::Delay,
        pick: higanr_core::host_pick::Pick::NONE,
    }
}

#[derive(Default)]
struct Q(Vec<(SocketAddr, Vec<u8>)>);

impl Outlet for Q {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.0.push((to, bytes.to_vec()));
    }
}

impl Q {
    /// 溜まったものを（宛先・解いた中身）にして取り出す。解けないものがあれば落ちる。
    fn take(&mut self) -> Vec<(SocketAddr, Message)> {
        std::mem::take(&mut self.0)
            .into_iter()
            .map(|(to, b)| (to, dec(&b).expect("復号できないものを送った").msg))
            .collect()
    }
}

fn dests(sent: &[(SocketAddr, Message)], pred: impl Fn(&Message) -> bool) -> Vec<SocketAddr> {
    sent.iter()
        .filter(|(_, m)| pred(m))
        .map(|(to, _)| *to)
        .collect()
}

fn is_hello(m: &Message) -> bool {
    matches!(m, Message::Hello(_))
}

#[test]
fn a_hello_from_an_unlisted_source_is_answered_once_and_learned_only_from_a_current_ack() {
    let mut a = link(B_CODE, 1);
    let mut q = Q::default();
    a.poll(1, &mut q);
    let first = q.take();
    assert_eq!(dests(&first, is_hello), [B_CODE], "学ぶ前は候補へだけ送る");

    let r = a.on_datagram(B_REAL, &enc(&hello(2)), 2, &mut q);
    assert_eq!(r, Recv::Nothing);
    let c = a.recv_counts();
    assert_eq!((c.learned, c.learn_refused, c.foreign), (0, 0, 0), "{c:?}");
    let reply = q.take();
    assert_eq!(
        dests(&reply, is_hello)
            .iter()
            .filter(|d| **d == B_REAL)
            .count(),
        1,
        "{reply:?}"
    );
    assert!(
        dests(&reply, |m| matches!(m, Message::HelloAck { .. })).is_empty(),
        "学んでいない送り元へ HELLO_ACK を返した: {reply:?}"
    );
    assert_eq!(a.hello_sent_unlisted(), 1);

    for t in 3..=60 {
        a.poll(t, &mut q);
    }
    let to = dests(&q.take(), is_hello);
    assert!(
        !to.is_empty() && to.iter().all(|d| *d == B_CODE),
        "学んでいない送り元へ送り続けた: {to:?}"
    );
    assert_eq!(a.hello_sent_learned(), 0);

    a.on_datagram(B_REAL, &enc(&ack(1, 2)), 61, &mut q);
    let c = a.recv_counts();
    assert_eq!((c.learned, c.foreign), (1, 0), "{c:?}");
    assert_eq!(a.path(), Some(B_REAL));
    q.take();
    a.on_datagram(B_REAL, &enc(&hello(2)), 62, &mut q);
    assert_eq!(
        dests(&q.take(), |m| matches!(
            m,
            Message::HelloAck { echo_nonce: 2, .. }
        )),
        [B_REAL]
    );
}

/// 今のセッションの自分のHELLOへの返答でないHELLO_ACK（echoが自分のnonceでない・自分の今の公開鍵でない）は、経路を決めず学ばない。
/// そのときHELLOも止めない。
#[test]
fn only_a_current_hello_ack_from_an_unlisted_source_decides_the_path_there() {
    let mut a = link(B_CODE, 1);
    let mut q = Q::default();
    a.poll(1, &mut q);
    q.take();
    a.on_datagram(sa(41_103), &enc(&ack(77, 2)), 2, &mut q);
    a.on_datagram(sa(41_104), &enc(&ack_to_key(1, 2, public_of(5))), 2, &mut q);
    a.on_datagram(B_CODE, &enc(&ack_to_key(1, 2, public_of(5))), 2, &mut q);
    assert_eq!(a.path(), None);
    assert!(!a.path_is_learned());
    let c = a.recv_counts();
    assert_eq!((c.learned, c.foreign), (0, 2), "{c:?}");
    assert_eq!(a.handshake_counters().wrong_echo, 1, "候補からの古い返し");
    a.poll(25, &mut q);
    assert_eq!(
        dests(&q.take(), is_hello),
        [B_CODE],
        "古い返しで HELLO を止めた"
    );
    a.on_datagram(B_REAL, &enc(&ack(1, 2)), 30, &mut q);
    assert_eq!(a.path(), Some(B_REAL));
    assert!(a.path_is_learned());
    assert_eq!(a.recv_counts().learned, 1);
    // 経路が決まった後のHELLOは経路へだけ（空でもよい。コードのアドレス・別の学んだアドレスへ送り続けないことを見る）。
    q.take();
    for t in 31..=100 {
        a.poll(t, &mut q);
    }
    let sent = q.take();
    let to = dests(&sent, is_hello);
    assert!(to.iter().all(|d| *d == B_REAL), "{to:?}");
}

#[test]
fn an_agree_from_an_unlisted_source_decides_the_path_there() {
    let mut a = link(B_CODE, 1);
    let mut q = Q::default();
    a.poll(1, &mut q);
    // HELLO_ACKは渡さない。渡すと経路が決まってしまう。
    // HELLOもコードの候補から届くと学んだ送り元に経路を決めない。
    // そこでルーターがポートを付け替えた相手と同じく、AGREEと同じ送り元から渡す。
    a.on_datagram(B_REAL, &enc(&hello(2)), 2, &mut q);
    assert_eq!(a.path(), None);
    a.on_datagram(B_REAL, &enc(&agree()), 3, &mut q);
    assert_eq!(a.recv_counts().learned, 1, "{:?}", a.recv_counts());
    assert_eq!(a.path(), Some(B_REAL));
    assert!(a.path_is_learned());
    assert!(a.connected().is_some(), "{:?}", a.handshake_state());
    assert_eq!(
        a.connected().map(|c| c.role()),
        Some(higanr_core::lockstep::Role::Guest)
    );
}

/// 学ぶゲートを1つずつ見る。各行は「この1つだけが違う」サンプル。
/// 違いは次のとおり: タグの誤り・別の秘密の乱数・別のsession・バージョン違い（タグは合う）・手順の外の型・自分の跳ね返り・今の公開鍵への返答でないHELLO_ACK・STUN・でたらめ。
/// これらは学ばず「候補に無い送り元」として数えて捨てる。
/// 隣に、同じ送り元からの今のセッションのHELLO_ACKは学ぶことを見る対照を並べる。
#[test]
fn only_a_well_formed_handshake_packet_with_a_good_tag_is_learned() {
    let from = B_REAL;
    let good = enc(&hello(2));
    let good_ack = enc(&ack(1, 2));
    let mut flip_body = good.clone();
    flip_body[HEADER_LEN + 4] ^= 0x10;
    let mut flip_tag = good.clone();
    let last = flip_tag.len() - 1;
    flip_tag[last] ^= 0x01;
    let truncated = good[..good.len() - 1].to_vec();
    let other_secret = enc_with(&AuthKey::derive(&Secret([0x5F; 16]), SID), SID, &hello(2));
    let other_sid = enc_with(
        &AuthKey::derive(&SECRET, SID ^ 0x100),
        SID ^ 0x100,
        &hello(2),
    );
    let mut v = good[..good.len() - wire::TAG_LEN].to_vec();
    v[4] = wire::PROTO_VER ^ 1;
    let tag = key().tag(&v);
    v.extend_from_slice(&tag);
    let other_version = v;
    let mut words = [0u16; MAX_WORDS];
    words[0] = 0x0011;
    let not_handshake = [
        Message::Ping { send_time: 5 },
        Message::Pong { send_time: 5 },
        Message::Bye(ByeReason::Finished),
        Message::Input(wire::Input {
            seg: 0,
            ack: 2,
            base: 2,
            lat: None,
            stamp: higanr_core::wire::Stamp::NONE,
            count: 1,
            words,
        }),
    ];
    let mut poisons: Vec<(&str, Vec<u8>)> = vec![
        ("本文の 1 ビット", flip_body),
        ("タグの 1 ビット", flip_tag),
        ("切り詰め", truncated),
        ("別の秘密", other_secret),
        ("別の session", other_sid),
        ("バージョン違い（タグは合う）", other_version),
        ("自分の HELLO の跳ね返り", enc(&hello(1))),
        ("自分の HELLO_ACK の跳ね返り", enc(&ack(2, 1))),
        (
            "自分の今の公開鍵への返しでない HELLO_ACK",
            enc(&ack_to_key(1, 2, public_of(5))),
        ),
        (
            "STUN",
            higanr_core::stun::encode_binding_request([1; 12]).to_vec(),
        ),
        ("でたらめ", vec![0xAB; 40]),
        ("HGRT だけ", b"HGRT".to_vec()),
    ];
    for m in &not_handshake {
        poisons.push(("手順の外の型", enc(m)));
    }

    let mut a = link(B_CODE, 1);
    let mut q = Q::default();
    a.poll(1, &mut q);
    q.take();
    let mut foreign_expected = 0u64;
    for (what, bytes) in &poisons {
        let r = a.on_datagram(from, bytes, 2, &mut q);
        assert_eq!(r, Recv::Nothing, "{what}");
        let c = a.recv_counts();
        assert_eq!(c.learned, 0, "{what}: 学んだ（{c:?}）");
        // STUN・でたらめ（HGRTで始まらない）は受信側の手前のゲートで別に数える（`stun` / `decode_errors`）。
        if !matches!(*what, "STUN" | "でたらめ") {
            foreign_expected += 1;
            assert_eq!(c.foreign, foreign_expected, "{what}: {c:?}");
        }
        assert!(q.0.is_empty(), "{what}: 捨てたものに返した {:?}", q.take());
    }
    // タグの合うバージョン違いは止めずに数える。ほかの不正な入力はバージョン違いに数えない。
    assert_eq!(a.recv_counts().other_version, 1, "{:?}", a.recv_counts());
    assert_eq!(a.handshake_state(), State::Connecting);
    assert!(
        !matches!(
            a.handshake_state(),
            State::Failed(FailReason::OtherVersion { .. })
        ),
        "候補の外のバージョン違いで手順をやめた"
    );
    for t in 3..=60 {
        a.poll(t, &mut q);
    }
    let sent = q.take();
    assert!(
        dests(&sent, is_hello).iter().all(|d| *d == B_CODE),
        "{sent:?}"
    );
    assert_eq!(a.hello_sent_learned(), 0);

    a.on_datagram(from, &good, 61, &mut q);
    assert_eq!(a.recv_counts().learned, 0, "HELLO で学んだ");
    a.on_datagram(from, &good_ack, 62, &mut q);
    assert_eq!(a.recv_counts().learned, 1);
}

#[test]
fn nothing_new_is_learned_after_connecting() {
    let mut net = Net::new(1, 2, Route::Straight);
    net.run_until_connected(400);
    let stranger = sa(41_150);
    let (a, qa) = (&mut net.a, &mut net.qa);
    qa.0.clear();
    let before = a.recv_counts();
    for m in [hello(2), ack(1, 2), agree()] {
        let r = a.on_datagram(stranger, &enc(&m), 500, qa);
        assert_eq!(r, Recv::Nothing, "{m:?}");
    }
    let after = a.recv_counts();
    assert_eq!(after.learned, before.learned, "繋がった後に学んだ");
    assert_eq!(after.foreign, before.foreign + 3, "{after:?}");
    assert!(qa.0.is_empty(), "繋がった後の候補外の 1 通に返した");
    a.on_datagram(B_CODE, &enc(&hello(2)), 501, qa);
    assert!(!qa.0.is_empty(), "候補からの HELLO に返さない");
}

#[test]
fn learning_stops_at_the_cap_and_counts_the_refusals() {
    let mut a = link(B_CODE, 1);
    a.set_retry_window(60_000);
    let mut q = Q::default();
    a.poll(1, &mut q);
    // 相手のHELLOはコードの候補でない送り元から（コードの候補から届くと、学んだ送り元には経路を決めない）。
    a.on_datagram(B_REAL, &enc(&hello(2)), 2, &mut q);
    q.take();
    let extra = 2usize;
    let ports: Vec<u16> = (0..(MAX_LEARNED + extra) as u16)
        .map(|i| 41_200 + i)
        .collect();
    for (i, p) in ports.iter().enumerate() {
        a.on_datagram(sa(*p), &enc(&ack(1, 2)), 3 + i as u32, &mut q);
    }
    let c = a.recv_counts();
    assert_eq!(c.learned, MAX_LEARNED as u64, "{c:?}");
    assert_eq!(c.learn_refused, extra as u64, "{c:?}");
    assert_eq!(
        c.foreign, extra as u64,
        "断ったものは候補に無い送り元として数える: {c:?}"
    );
    assert_eq!(c.last_foreign, ports.last().map(|p| sa(*p)));
    let want: Vec<SocketAddr> = ports[..MAX_LEARNED].iter().map(|p| sa(*p)).collect();
    assert_eq!(a.path(), Some(want[0]));
    a.on_datagram(sa(ports[0]), &enc(&ack(1, 2)), 30, &mut q);
    let c2 = a.recv_counts();
    assert_eq!((c2.learned, c2.foreign), (c.learned, c.foreign));
    // 作り直し（引き継ぐ形）で経路の選択を戻すとHELLOはコードの候補と学んだ送り元の全部へ出る。
    for t in (31..=10_100).step_by(4) {
        a.poll(t, &mut q);
    }
    assert_eq!(
        a.restarts().last.map(|(_, k)| k),
        Some(handshake::Restart::Resume),
        "引き継ぐ作り直しになっていない（この検査の前提）"
    );
    let sent = q.take();
    let to = dests(&sent, is_hello);
    for p in &ports[MAX_LEARNED..] {
        assert!(!to.contains(&sa(*p)), "断った送り元へ送った: {p}");
    }
    for d in want.iter().chain([&B_CODE]) {
        assert!(to.contains(d), "{d} へ送っていない");
    }
}

#[test]
fn a_fresh_restart_forgets_what_was_learned_and_a_resume_keeps_it() {
    let mut a = link(B_CODE, 1);
    a.set_retry_window(60_000);
    let mut q = Q::default();
    a.poll(1, &mut q);
    a.on_datagram(B_REAL, &enc(&ack(1, 2)), 2, &mut q);
    assert_eq!(a.path(), Some(B_REAL));
    q.take();
    for t in (3..=11_000).step_by(4) {
        a.poll(t, &mut q);
    }
    let r = a.restarts();
    assert!(
        r.count >= 1,
        "作り直していない: {:?} {:?}",
        r,
        a.handshake_state()
    );
    assert_eq!(
        r.last.map(|(_, k)| k),
        Some(handshake::Restart::Fresh),
        "最初からの作り直しになっていない"
    );
    assert_eq!(a.path(), None, "経路の選びを忘れていない");
    q.take();
    for t in (11_001..=11_100).step_by(4) {
        a.poll(t, &mut q);
    }
    let sent = q.take();
    let to = dests(&sent, is_hello);
    assert!(!to.is_empty(), "作り直した後に HELLO を送っていない");
    assert!(
        to.iter().all(|d| *d == B_CODE),
        "作り直しの後も学んだアドレスへ送った: {to:?}"
    );
    let l0 = a.recv_counts().learned;
    a.on_datagram(sa(41_160), &enc(&ack(1, 2)), 11_101, &mut q);
    assert_eq!(a.recv_counts().learned, l0 + 1);

    let mut b = link(B_CODE, 1);
    b.set_retry_window(60_000);
    b.poll(1, &mut q);
    b.on_datagram(B_CODE, &enc(&hello(2)), 2, &mut q);
    b.on_datagram(B_REAL, &enc(&ack(1, 2)), 2, &mut q);
    q.take();
    for t in (3..=11_000).step_by(4) {
        b.poll(t, &mut q);
    }
    assert_eq!(
        b.restarts().last.map(|(_, k)| k),
        Some(handshake::Restart::Resume)
    );
    q.take();
    for t in (11_001..=11_100).step_by(4) {
        b.poll(t, &mut q);
    }
    let sent = q.take();
    assert!(
        dests(&sent, is_hello).contains(&B_REAL),
        "引き継ぎで学んだアドレスを忘れた"
    );
}

#[test]
fn a_resume_forgets_a_learned_path_but_keeps_the_learned_list() {
    let mut a = link(B_CODE, 1);
    a.set_retry_window(60_000);
    let mut q = Q::default();
    a.poll(1, &mut q);
    a.on_datagram(B_REAL, &enc(&hello(2)), 2, &mut q);
    a.on_datagram(B_REAL, &enc(&ack(1, 2)), 3, &mut q);
    assert_eq!(
        a.path(),
        Some(B_REAL),
        "学んだアドレスに経路が決まっていない"
    );
    assert!(a.path_is_learned());
    q.take();
    for t in (4..=11_000).step_by(4) {
        a.poll(t, &mut q);
    }
    assert_eq!(
        a.restarts().last.map(|(_, k)| k),
        Some(handshake::Restart::Resume),
        "引き継ぐ作り直しになっていない（この検査の前提）"
    );
    assert_eq!(a.path(), None, "引き継ぐ作り直しで学んだ経路を持ち越した");
    q.take();
    for t in (11_001..=11_100).step_by(4) {
        a.poll(t, &mut q);
    }
    let to = dests(&q.take(), is_hello);
    assert!(
        to.contains(&B_CODE),
        "コードのアドレスへ送っていない: {to:?}"
    );
    assert!(to.contains(&B_REAL), "学んだ列まで捨てた: {to:?}");
    let moved = sa(41_202);
    a.on_datagram(moved, &enc(&hello(2)), 11_101, &mut q);
    a.on_datagram(moved, &enc(&ack(1, 2)), 11_102, &mut q);
    assert_eq!(
        a.path(),
        Some(moved),
        "新しいアドレスで経路を決め直していない"
    );
}

/// 上の検査はゲストの側でAGREEを送らないのでAGREEの宛先は見ていない。ここはホストの側。
#[test]
fn a_resume_on_the_host_side_sends_agree_to_the_new_port_not_the_dead_learned_one() {
    let mut a = link(B_CODE, 3);
    a.set_retry_window(60_000);
    let mut q = Q::default();
    a.poll(1, &mut q);
    a.on_datagram(B_REAL, &enc(&hello(2)), 2, &mut q);
    a.on_datagram(B_REAL, &enc(&ack(3, 2)), 3, &mut q);
    for t in (4..=11_000).step_by(4) {
        a.poll(t, &mut q);
    }
    assert_eq!(
        a.restarts().last.map(|(_, k)| k),
        Some(handshake::Restart::Resume),
        "引き継ぐ作り直しになっていない（この検査の前提）"
    );
    q.take();
    let moved = sa(41_302);
    a.on_datagram(moved, &enc(&hello(2)), 11_001, &mut q);
    for t in (11_002..=11_100).step_by(4) {
        a.poll(t, &mut q);
    }
    let sent = q.take();
    let agree_to = dests(&sent, |m| matches!(m, Message::Agree { .. }));
    assert!(
        !agree_to.contains(&B_REAL),
        "引き継ぐ作り直しの後も死んだ学んだアドレスへ AGREE を送った: {agree_to:?}"
    );
    assert!(
        agree_to.contains(&moved),
        "新しいアドレスへ AGREE を送っていない（この検査が AGREE を見ている証拠）: {agree_to:?} / {sent:?}"
    );
}

#[test]
fn the_pong_responder_answers_pings_from_a_learned_address() {
    let mut net = Net::new(1, 2, Route::PeerBehindMovedPort);
    net.run_until_connected(2_000);
    let r: PongResponder = net.a.pong_responder().expect("繋がった");
    let ping = enc(&Message::Ping { send_time: 9 });
    let mut out = [0u8; MAX_DATAGRAM];
    let seq = higanr_core::seal::Seq::new();
    let got = r.answer(&seq, B_REAL, &ping, &mut out);
    assert_eq!(
        got.map(|(to, _)| to),
        Some(B_REAL),
        "学んだアドレスからの PING に返さない"
    );
    let (_, n) = got.expect("返す");
    assert_eq!(
        dec(&out[..n]).map(|p| p.msg),
        Ok(Message::Pong { send_time: 9 })
    );
    let mut out2 = [0u8; MAX_DATAGRAM];
    assert!(
        r.answer(&seq, sa(41_999), &ping, &mut out2).is_none(),
        "学んでいないアドレスにも返した"
    );
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    Straight,
    /// Bのルーターが外側のポートを付け替えた（B → Aの送り元は`B_REAL`。コードの`B_CODE`へ送ると捨てられる）。
    PeerBehindMovedPort,
}

struct Net {
    a: Link,
    b: Link,
    qa: Q,
    qb: Q,
    now: u32,
    route: Route,
    /// ネットワークが捨てた通数（コードのアドレス`B_CODE`宛て）。対照の実験で「捨てられていた」ことを見せる。
    dropped_to_stale: u32,
    /// B → Aを全部捨てる（片方向の途絶）。捨てた便は`cut_log`に残す（盗み見た人が持つ写し）。
    cut_b_to_a: bool,
    cut_log: Vec<Vec<u8>>,
}

impl Net {
    fn new(a_nonce: u64, b_nonce: u64, route: Route) -> Net {
        Net {
            a: link(B_CODE, a_nonce),
            b: link(A_ADDR, b_nonce),
            qa: Q::default(),
            qb: Q::default(),
            now: 0,
            route,
            dropped_to_stale: 0,
            cut_b_to_a: false,
            cut_log: Vec::new(),
        }
    }

    fn step(&mut self) {
        self.now += 1;
        let t = self.now;
        self.a.poll(t, &mut self.qa);
        self.b.poll(t, &mut self.qb);
        for _ in 0..4 {
            let from_a = std::mem::take(&mut self.qa.0);
            let from_b = std::mem::take(&mut self.qb.0);
            if from_a.is_empty() && from_b.is_empty() {
                break;
            }
            for (to, bytes) in from_a {
                let moved = self.route != Route::Straight;
                if to == B_CODE && moved {
                    self.dropped_to_stale += 1;
                } else if to == B_CODE || to == B_REAL {
                    self.b.on_datagram(A_ADDR, &bytes, t, &mut self.qb);
                }
            }
            for (to, bytes) in from_b {
                if to == A_ADDR && self.cut_b_to_a {
                    self.cut_log.push(bytes);
                } else if to == A_ADDR {
                    let src = if self.route == Route::Straight {
                        B_CODE
                    } else {
                        B_REAL
                    };
                    self.a.on_datagram(src, &bytes, t, &mut self.qa);
                }
            }
        }
    }

    fn run_until_connected(&mut self, max_ms: u32) {
        for _ in 0..max_ms {
            self.step();
            if self.a.connected().is_some() && self.b.connected().is_some() {
                break;
            }
        }
        for _ in 0..max_ms.min(1_500) {
            self.step();
        }
    }
}

/// 組み上がり: Aの候補が全部外れても（コードのアドレスへ送っても届かない）どちらの役割でも接続できる。
/// そのときHELLO・HELLO_ACK・AGREEの全部が学んだアドレスから来る。
/// 経路は学んだアドレスになり、PONGでRTTが測れる。
/// 対照: 「候補に無い送り元を捨てる」ゲートをネットワーク側に追加すると接続できない（セッションの側の変異は変異検査で見る）。
#[test]
fn two_links_connect_when_one_side_candidates_are_all_wrong() {
    for (a_nonce, b_nonce) in [(1u64, 2u64), (9, 2)] {
        let tag = format!("A {a_nonce} / B {b_nonce}");
        let mut net = Net::new(a_nonce, b_nonce, Route::PeerBehindMovedPort);
        net.run_until_connected(3_000);
        assert!(
            net.a.connected().is_some() && net.b.connected().is_some(),
            "{tag}: 繋がらない（A {:?} / B {:?}）",
            net.a.handshake_state(),
            net.b.handshake_state()
        );
        assert_eq!(net.a.path(), Some(B_REAL), "{tag}");
        assert!(net.a.path_is_learned(), "{tag}");
        assert_eq!(net.b.path(), Some(A_ADDR), "{tag}");
        assert!(!net.b.path_is_learned(), "{tag}: B の経路はコードの候補");
        let (ca, cb) = (net.a.recv_counts(), net.b.recv_counts());
        assert_eq!(ca.learned, 1, "{tag}: {ca:?}");
        assert_eq!((cb.learned, cb.foreign), (0, 0), "{tag}: {cb:?}");
        assert_eq!(
            ca.foreign, 0,
            "{tag}: 学んだ送り元からのものを捨てた {ca:?}"
        );
        assert!(
            net.dropped_to_stale > 0,
            "{tag}: 古いアドレスへ送っていない（模擬のネットワークが働いていない）"
        );
        let (ra, rb) = (
            net.a.connected().and_then(|c| c.liveness().rtt_ms()),
            net.b.connected().and_then(|c| c.liveness().rtt_ms()),
        );
        assert!(
            ra.is_some(),
            "{tag}: A に RTT が無い（学んだアドレスからの PONG を受けていない）"
        );
        assert!(rb.is_some(), "{tag}: B に RTT が無い");
    }

    let mut old = OldGate::new(1, 2);
    for _ in 0..3_000 {
        old.step();
    }
    assert!(
        old.a.connected().is_none() && old.b.connected().is_none(),
        "対照が繋がった（ネットワークの再現が効いていない）"
    );
    assert!(old.refused_by_gate > 0, "A へ 1 通も着いていない");
}

/// `Route::PeerBehindMovedPort`のネットワークでAの受信側の手前に「候補に無い送り元は捨てる」ゲートを追加した対照。セッションは学べない。
struct OldGate {
    a: Link,
    b: Link,
    qa: Q,
    qb: Q,
    now: u32,
    refused_by_gate: u32,
}

impl OldGate {
    fn new(a_nonce: u64, b_nonce: u64) -> OldGate {
        OldGate {
            a: link(B_CODE, a_nonce),
            b: link(A_ADDR, b_nonce),
            qa: Q::default(),
            qb: Q::default(),
            now: 0,
            refused_by_gate: 0,
        }
    }

    fn step(&mut self) {
        self.now += 1;
        let t = self.now;
        self.a.poll(t, &mut self.qa);
        self.b.poll(t, &mut self.qb);
        for _ in 0..4 {
            let from_a = std::mem::take(&mut self.qa.0);
            let from_b = std::mem::take(&mut self.qb.0);
            for (to, bytes) in from_a {
                if to == B_REAL {
                    self.b.on_datagram(A_ADDR, &bytes, t, &mut self.qb);
                }
            }
            for (to, _) in from_b {
                if to == A_ADDR {
                    self.refused_by_gate += 1;
                }
            }
        }
    }
}

fn hello_keyed(nonce: u64, public: [u8; PUBLIC_LEN]) -> Message {
    let Message::Hello(mut h) = hello(nonce) else {
        unreachable!("HELLO を作った");
    };
    h.public = public;
    Message::Hello(h)
}

/// 盗み見た前のセッションのHELLO（Bがメニューを開き直す前のnonce 7か同じnonceの古い公開鍵）を送り元を偽って何度再送しても、Aは今のBと接続する。
/// Aの時間切れの作り直しは待たない。
#[test]
fn replayed_hellos_of_an_earlier_session_do_not_keep_the_live_peer_out() {
    let other_nonce = enc(&hello(7));
    let old_key = enc(&hello_keyed(2, public_of(9)));
    let elsewhere = sa(41_777);
    for (what, stale, from, first) in [
        (
            "別の nonce・候補のアドレス・先に",
            &other_nonce,
            B_CODE,
            true,
        ),
        (
            "別の nonce・候補に無いアドレス・先に",
            &other_nonce,
            elsewhere,
            true,
        ),
        (
            "同じ nonce の古い鍵・候補のアドレス・後に",
            &old_key,
            B_CODE,
            false,
        ),
        (
            "同じ nonce の古い鍵・候補に無いアドレス・後に",
            &old_key,
            elsewhere,
            false,
        ),
    ] {
        let mut net = Net::new(1, 2, Route::Straight);
        if first {
            net.a.on_datagram(from, stale, 0, &mut net.qa);
        }
        let mut at = None;
        for t in 1..2_000u32 {
            if t % 3 == 0 {
                net.a.on_datagram(from, stale, net.now, &mut net.qa);
            }
            net.step();
            if net.a.connected().is_some() && net.b.connected().is_some() {
                at = Some(t);
                break;
            }
        }
        assert!(
            at.is_some_and(|ms| ms < 500),
            "{what}: 接続しない（{at:?} ms・A {:?}・{:?}）",
            net.a.handshake_state(),
            net.a.handshake_counters()
        );
        let c = net.a.recv_counts();
        assert_eq!(c.learned, 0, "{what}: 偽の送り元を学んだ {c:?}");
    }
}

/// 次の場合でもAは時間切れを待たずに接続する。
/// Bの今のセッションのHELLO_ACKだけがAへ届き（BのHELLOは落ちた）、その直後にBが取りやめて同じコードを貼り直した（同じnonce・新しい公開鍵）。
#[test]
fn a_peer_that_restarts_right_after_only_its_ack_arrived_still_connects_at_once() {
    let mut net = Net::new(1, 2, Route::Straight);
    net.a.poll(1, &mut net.qa);
    for (to, bytes) in std::mem::take(&mut net.qa.0) {
        if to == B_CODE {
            net.b.on_datagram(A_ADDR, &bytes, 1, &mut net.qb);
        }
    }
    let mut acks = 0;
    for (to, bytes) in std::mem::take(&mut net.qb.0) {
        let msg = wire::decode(&key(), None, &bytes).map(|p| p.msg);
        if to == A_ADDR && matches!(msg, Ok(Message::HelloAck { .. })) {
            net.a.on_datagram(B_CODE, &bytes, 2, &mut net.qa);
            acks += 1;
        }
    }
    assert_eq!(acks, 1, "効く期間: B1 の返しが A へ届いた");
    assert_eq!(net.a.path(), Some(B_CODE));
    net.qa.0.clear();
    let b2_seed = KeySeed([0x55; 32]);
    assert_ne!(
        Ephemeral::from_seed(&b2_seed).public(),
        public_of(2),
        "効く期間: 貼り直した B の公開鍵が前と違う"
    );
    net.b = Link::new(Config::single(hs_cfg(2), SECRET, A_ADDR, b2_seed), 3);
    net.now = 3;
    let mut at = None;
    for _ in 0..3_000 {
        net.step();
        if net.a.connected().is_some() && net.b.connected().is_some() {
            at = Some(net.now);
            break;
        }
    }
    assert!(
        at.is_some_and(|ms| ms < 500),
        "接続しない（{at:?} ms・A {:?}・{:?}）",
        net.a.handshake_state(),
        net.a.handshake_counters()
    );
}

/// 今のBのHELLOを盗み見て送り元を偽って再送しても、Aは偽の送り元を学ばない。
/// 1通につき1回だけ返し、その後は送らない。
#[test]
fn a_replayed_hello_from_spoofed_sources_gets_one_answer_each_and_is_never_learned() {
    for (a_nonce, per_replay) in [(1u64, 1usize), (3, 2)] {
        let mut a = link(B_CODE, a_nonce);
        let mut q = Q::default();
        a.poll(1, &mut q);
        q.take();
        let spoofed: Vec<SocketAddr> = (0..(MAX_LEARNED as u16 + 2))
            .map(|i| sa(41_400 + i))
            .collect();
        for (i, s) in spoofed.iter().enumerate() {
            a.on_datagram(*s, &enc(&hello(2)), 2 + i as u32, &mut q);
        }
        let first = q.take();
        for s in &spoofed {
            let to_s: Vec<&Message> = first
                .iter()
                .filter(|(to, _)| to == s)
                .map(|(_, m)| m)
                .collect();
            assert_eq!(to_s.len(), per_replay, "A {a_nonce}・{s}: {to_s:?}");
            assert!(
                to_s.iter().all(|m| !matches!(m, Message::HelloAck { .. })),
                "A {a_nonce}・{s}: HELLO_ACK を返した"
            );
        }
        let c = a.recv_counts();
        assert_eq!((c.learned, c.learn_refused), (0, 0), "A {a_nonce}: {c:?}");
        for t in 10..=500 {
            a.poll(t, &mut q);
        }
        let later = q.take();
        assert!(
            later.iter().all(|(to, _)| !spoofed.contains(to)),
            "A {a_nonce}: 偽の送り元へ送り続けた"
        );
        assert!(
            !dests(&later, is_hello).is_empty(),
            "A {a_nonce}: HELLO を止めた（この検査が送りを見ている証拠）"
        );
    }
}

/// 盗み見たBの便を送り直す人のアドレス（候補にもBの実際の送り元にも無い）。
const X_ADDR: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    41_900,
);

fn is_ack_bytes(b: &[u8]) -> bool {
    matches!(
        wire::decode(&key(), None, b).map(|p| p.msg),
        Ok(Message::HelloAck { .. })
    )
}

/// 封じた便（ホストの決め方を使わない接続の前ではAGREEだけ）。
fn is_sealed_bytes(b: &[u8]) -> bool {
    wire::decode(&key(), None, b) == Err(wire::DecodeError::NotKeyed)
}

/// Aの送った便のうちBのコードのアドレス宛てをBへ渡す（X宛ては捨てる）。
fn deliver_a(net: &mut Net, t: u32) {
    for (to, b) in std::mem::take(&mut net.qa.0) {
        if to == B_CODE {
            net.b.on_datagram(A_ADDR, &b, t, &mut net.qb);
        }
    }
}

/// Bの送った便をBの送った順にAへ渡す。`relay`に当たる便は本物の直前にXからも渡す。`lose`に当たる本物は落とす。
/// Xから渡した通数を返す。
fn deliver_b(net: &mut Net, t: u32, relay: fn(&[u8]) -> bool, lose: fn(&[u8]) -> bool) -> usize {
    let mut relayed = 0;
    for (to, b) in std::mem::take(&mut net.qb.0) {
        if to != A_ADDR {
            continue;
        }
        if relay(&b) {
            net.a.on_datagram(X_ADDR, &b, t, &mut net.qa);
            relayed += 1;
        }
        if !lose(&b) {
            net.a.on_datagram(B_CODE, &b, t, &mut net.qa);
        }
    }
    relayed
}

/// 両者が接続し、Aの経路・受信スレッドのPONGの宛先がBのコードのアドレスで、その後のAの送信がXへ向かない。
fn assert_on_code_path(net: &mut Net, what: &str) {
    assert!(
        net.a.connected().is_some() && net.b.connected().is_some(),
        "{what}: 接続しない（A {:?} / B {:?}）",
        net.a.handshake_state(),
        net.b.handshake_state()
    );
    assert_eq!(net.a.path(), Some(B_CODE), "{what}");
    assert_eq!(
        net.a.pong_responder().map(|r| r.path()),
        Some(B_CODE),
        "{what}: 接続の経路"
    );
    net.qa.0.clear();
    for _ in 0..1_500 {
        net.now += 1;
        net.a.poll(net.now, &mut net.qa);
    }
    let sent = std::mem::take(&mut net.qa.0);
    assert!(!sent.is_empty(), "{what}: 接続の後に何も送っていない");
    assert!(
        sent.iter().all(|(to, _)| *to == B_CODE),
        "{what}: X へ送った"
    );
}

#[test]
fn a_current_ack_relayed_from_another_address_before_the_real_one_does_not_take_the_path() {
    for (a_nonce, b_nonce) in [(1u64, 2u64), (3, 2)] {
        let what = format!("A {a_nonce} / B {b_nonce}");
        let mut net = Net::new(a_nonce, b_nonce, Route::Straight);
        net.now = 1;
        net.a.poll(1, &mut net.qa);
        deliver_a(&mut net, 1);
        let relayed = deliver_b(&mut net, 2, is_ack_bytes, |_| false);
        assert_eq!(
            relayed, 1,
            "{what}: 効く期間: B の HELLO_ACK を X から送り直した"
        );
        assert_eq!(
            net.a.recv_counts().learned,
            1,
            "{what}: 効く期間: X を学んだ"
        );
        net.now = 2;
        net.run_until_connected(2_000);
        assert_on_code_path(&mut net, &what);
    }
}

/// BのHELLOはまだ届かず、HELLO_ACKの写しがXから先に、本物がコードの候補から後に届く。
#[test]
fn a_current_ack_from_the_code_takes_the_path_back_from_a_learned_relay_before_connecting() {
    let mut a = link(B_CODE, 1);
    let mut q = Q::default();
    a.poll(1, &mut q);
    let stored = enc(&ack(1, 2));
    a.on_datagram(X_ADDR, &stored, 2, &mut q);
    assert_eq!(a.path(), Some(X_ADDR), "効く期間: X が先に経路を取った");
    a.on_datagram(B_CODE, &stored, 3, &mut q);
    assert_eq!(a.path(), Some(B_CODE));
    a.on_datagram(X_ADDR, &stored, 4, &mut q);
    assert_eq!(a.path(), Some(B_CODE), "X が経路を取り返した");
}

/// XがBのHELLO_ACKとHELLOを先に再送して経路とAGREEの宛先を取っても、BのHELLOがコードの候補から届いたらホストのAはAGREEを候補へ送る。
#[test]
fn a_hello_from_the_code_takes_the_agree_back_from_a_learned_relay() {
    let mut a = link(B_CODE, 3);
    let mut q = Q::default();
    a.poll(1, &mut q);
    a.on_datagram(X_ADDR, &enc(&ack(3, 2)), 2, &mut q);
    a.on_datagram(X_ADDR, &enc(&hello(2)), 3, &mut q);
    let early = dests(&q.take(), |m| matches!(m, Message::Agree { .. }));
    assert!(
        early.contains(&X_ADDR),
        "効く期間: AGREE が X へ出た: {early:?}"
    );
    a.on_datagram(B_CODE, &enc(&hello(2)), 4, &mut q);
    assert_eq!(a.path(), None, "学んだ経路を戻していない");
    q.take();
    for t in 5..=40 {
        a.poll(t, &mut q);
    }
    let later = dests(&q.take(), |m| matches!(m, Message::Agree { .. }));
    assert!(!later.is_empty(), "AGREE を送っていない");
    assert!(later.iter().all(|d| *d == B_CODE), "{later:?}");
}

/// BのHELLO_ACKは落ち、Bの封じたAGREE（ホストのAGREEかゲストの返答）の写しがXから本物より先に届く。
#[test]
fn a_sealed_agree_relayed_from_another_address_before_the_real_one_does_not_take_the_path() {
    for (a_nonce, b_nonce) in [(1u64, 2u64), (3, 2)] {
        let what = format!("A {a_nonce} / B {b_nonce}");
        let mut net = Net::new(a_nonce, b_nonce, Route::Straight);
        net.a.poll(1, &mut net.qa);
        let mut relayed = 0;
        for t in 1..=10 {
            deliver_a(&mut net, t);
            relayed += deliver_b(&mut net, t + 1, is_sealed_bytes, is_ack_bytes);
            if net.a.connected().is_some() {
                break;
            }
        }
        assert!(
            relayed >= 1,
            "{what}: 効く期間: B の AGREE を X から送り直した"
        );
        assert_eq!(
            net.a.recv_counts().learned,
            1,
            "{what}: 効く期間: X を学んだ"
        );
        assert!(
            net.a.connected().is_some(),
            "{what}: 効く期間: A が接続した"
        );
        net.now = 12;
        net.run_until_connected(2_000);
        assert_on_code_path(&mut net, &what);
    }
}

/// 最初の回はB → Aが途絶えてAが最初から作り直し（Fresh）、その直後からXが盗み見たBのHELLO_ACKを再送し続ける。
#[test]
fn a_stored_ack_resent_from_another_address_after_a_fresh_restart_does_not_keep_the_path() {
    for (a_nonce, b_nonce) in [(1u64, 2u64), (3, 2)] {
        let what = format!("A {a_nonce} / B {b_nonce}");
        let mut net = Net::new(a_nonce, b_nonce, Route::Straight);
        net.a.set_retry_window(60_000);
        net.b.set_retry_window(60_000);
        net.cut_b_to_a = true;
        while net.a.restarts().count == 0 && net.now < 12_000 {
            net.step();
        }
        assert_eq!(
            net.a.restarts().last.map(|(_, k)| k),
            Some(handshake::Restart::Fresh),
            "{what}: 効く期間: 最初からの作り直し"
        );
        let stored = net
            .cut_log
            .iter()
            .find(|b| is_ack_bytes(b))
            .cloned()
            .expect("効く期間: B の HELLO_ACK を盗み見た");
        net.a.on_datagram(X_ADDR, &stored, net.now, &mut net.qa);
        assert_eq!(
            net.a.path(),
            Some(X_ADDR),
            "{what}: 効く期間: 作り直しの直後に X が経路を取った"
        );
        net.cut_b_to_a = false;
        for _ in 0..3_000 {
            if net.now.is_multiple_of(3) {
                net.a.on_datagram(X_ADDR, &stored, net.now, &mut net.qa);
            }
            net.step();
            if net.a.connected().is_some() && net.b.connected().is_some() {
                break;
            }
        }
        assert_on_code_path(&mut net, &what);
    }
}

/// Xを学んだまま引き継ぐ作り直し（Resume）になっても、Xから再送し続ける今のセッションのHELLO_ACKで経路を取り直させない。
#[test]
fn a_learned_relay_address_does_not_retake_the_path_after_a_resume() {
    let mut a = link(B_CODE, 1);
    a.set_retry_window(60_000);
    let mut q = Q::default();
    a.poll(1, &mut q);
    let stored = enc(&ack(1, 2));
    a.on_datagram(X_ADDR, &stored, 2, &mut q);
    assert_eq!(a.path(), Some(X_ADDR), "効く期間: X が先に経路を取った");
    a.on_datagram(B_CODE, &enc(&hello(2)), 3, &mut q);
    for t in (4..=11_000).step_by(4) {
        a.poll(t, &mut q);
    }
    assert_eq!(
        a.restarts().last.map(|(_, k)| k),
        Some(handshake::Restart::Resume),
        "効く期間: 引き継ぐ作り直し"
    );
    assert_eq!(a.recv_counts().learned, 1, "効く期間: X を学んだまま");
    for t in 11_001..11_050 {
        a.on_datagram(X_ADDR, &stored, t, &mut q);
        assert_ne!(a.path(), Some(X_ADDR), "t={t}");
    }
    a.on_datagram(B_CODE, &stored, 11_050, &mut q);
    assert_eq!(a.path(), Some(B_CODE));
}
