//! 受信スレッドがその場で返すPONGの応答役。
//!
//! ゲームのスレッドがリングを空にするのはtickの始まりだけ。
//! そのためPINGへのPONGをゲームのスレッド側のセッション（`link.rs` → `liveness.rs`）が返すと、マッチの外ではPINGが最大1 tick（16.7 ms）待たされ、相手の測るRTTがその分大きく出た。
//! PONGはPINGのsend_timeを返すだけでゲームの状態も自分の時計も要らない。そのため受信スレッドが受けたその場で返す。
//! マッチの入力への遅延の増分は0。PINGでない1通はtypeの1バイトを見て素通りさせる。
//! 応答役が持つのは接続した後にセッションが作る値のコピー（session_id・鍵・相手の候補・採った経路。`Link::pong_responder`）だけ。生存の状態は持たない。
//! **返してよいかどうかは呼び手が生存の状態から判断し、「生きている」の間だけ呼ぶ**（dllは`NetPlay`がtickごとに見て、受信スレッドへアトミック変数で渡す）。
//! 受信のガードはセッション（`Link::on_datagram`）と同じ順と条件: typeがPING → 候補（接続する前に学んだ候補を含む）のどれかから来た → session_idが同じ → タグが合う → 解ける。外れたら何も返さない。
//! 返した1通も受信スレッドはリングへ積む（生存の「届いた」フラグを失わないため）。
//! このとき「PONGを返した」フラグを添えて、セッションがPONGを重ねないようにする（`Link::on_datagram_marked`）。
//! 受信スレッドで1通ごとに呼ぶのでヒープを使わない。パニックしうる添字とunwrapも使わない（パニックすると以後何も受けない）。
//! 返すバイト列は呼び手のスタックの`[u8; MAX_DATAGRAM]`に作る。

use std::net::SocketAddr;

use crate::auth::AuthKey;
use crate::candidate::{Candidates, Learned};
use crate::seal::{Opener, Sealer, Seq};
use crate::wire::{self, ByeReason, MAX_DATAGRAM, Message};

/// 受信スレッドのPONGの応答役（接続した後のセッションの値のコピー）。接続ごとに1つ。
///
/// PINGもPONGも封をしたパケットなので、相手からの向きの`Opener`と、送信元の識別子`Sender::Pong`の`Sealer`を持つ。
/// 通し番号は応答役に持たせず、返すたびに呼び手が渡す（[`PongResponder::answer`]）。
/// dllでは、ゲームのスレッドから受信スレッドへ応答役を渡すコンテナが`Seq`を1つだけ持ち、0に戻さない。
/// 2本の受信スレッドと差し替えた応答役が全部これを使うので、同じ番号を2回使わない。
/// ゲームのスレッド側のセッションとは送信元の識別子が違うので重ならない。
#[derive(Debug, Clone)]
pub struct PongResponder {
    session_id: u32,
    /// バージョン違いの見分けにだけ使う鍵（`wire::decode`のガード）。
    key: AuthKey,
    open: Opener,
    seal: Sealer,
    /// 受ける送信元（セッションの`Paths::accepts`と同じ: 候補のどれか）。
    peers: Candidates,
    /// 接続する前に学んだ候補（`Paths::learn`）。接続した経路が学んだアドレスのことがあるので受ける送信元にも含める（セッションと同じガード）。
    learned: Learned,
    /// 返す先（セッションが接続した後の送信に使う経路と同じ）。
    path: SocketAddr,
}

impl PongResponder {
    pub fn new(
        session_id: u32,
        key: AuthKey,
        open: Opener,
        seal: Sealer,
        peers: Candidates,
        path: SocketAddr,
    ) -> Self {
        Self {
            session_id,
            key,
            open,
            seal,
            peers,
            learned: Learned::EMPTY,
            path,
        }
    }

    /// 接続する前に学んだ候補も受ける送信元に加える（`Link::pong_responder`が`Paths::learned`を渡す）。
    #[must_use]
    pub fn with_learned(mut self, learned: Learned) -> Self {
        self.learned = learned;
        self
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    pub fn path(&self) -> SocketAddr {
        self.path
    }

    /// 受けた1データグラム`bytes`（送信元`from`）が本物のPINGなら、同じsend_timeのPONGを`out`に作る。
    /// 返り値は（返す先, 長さ）。
    /// それ以外（PINGでない・候補にない送信元・別のsession・タグ違い・解けない・短すぎる）はNone。
    /// `seq`はPONGの通し番号。**この応答役とそのコピーの全部が同じ1つを渡す**（同じ鍵の別の`Seq`だとnonceが重なる）。
    pub fn answer(
        &self,
        seq: &Seq,
        from: SocketAddr,
        bytes: &[u8],
        out: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize)> {
        let send_time = self.real_ping(from, bytes)?;
        self.seal_to_path(seq, &Message::Pong { send_time }, out)
    }

    /// `answer`と同じガードで、本物のPINGへの返答をPONGの代わりにBYEにする。
    /// 自分のゲームスレッドが止まった後に、閉じたことを相手のPINGのたびに知らせ直すため。セッションの`Liveness::close`の後と同じ形。
    pub fn answer_bye(
        &self,
        seq: &Seq,
        from: SocketAddr,
        bytes: &[u8],
        reason: ByeReason,
        out: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize)> {
        self.real_ping(from, bytes)?;
        self.seal_to_path(seq, &Message::Bye(reason), out)
    }

    /// 頼まれずに出すBYE（返す先は経路）。
    pub fn bye(
        &self,
        seq: &Seq,
        reason: ByeReason,
        out: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize)> {
        self.seal_to_path(seq, &Message::Bye(reason), out)
    }

    /// 本物のPINGならsend_time。
    fn real_ping(&self, from: SocketAddr, bytes: &[u8]) -> Option<u32> {
        // 先にtypeだけを見る（INPUTなどにタグの計算を足さない）。
        if !wire::peek_is_ping(bytes) {
            return None;
        }
        if !self.peers.contains(from) && !self.learned.contains(from) {
            return None;
        }
        if wire::peek_session(bytes) != Some(self.session_id) {
            return None;
        }
        let pkt = wire::decode(&self.key, Some(&self.open), bytes).ok()?;
        match pkt.msg {
            Message::Ping { send_time } => Some(send_time),
            _ => None,
        }
    }

    fn seal_to_path(
        &self,
        seq: &Seq,
        msg: &Message,
        out: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize)> {
        // 通し番号を使い切っていたら返さない（手順の上では起きない）。
        let n = wire::seal(&self.seal, seq, self.session_id, msg, out)?;
        Some((self.path, n))
    }
}

#[cfg(test)]
mod tests {
    //! 期待値は仕様（PONGはPINGのsend_timeを返す）から作る。PONGの読み戻しは相手の向きの`Opener`を使う`wire::decode`。
    use super::*;
    use crate::auth::Secret;
    use crate::seal::{DirKeys, NONCE_FIELD_LEN, Sender, Seq, read_nonce_field};
    use crate::wire::{ByeReason, HEADER_LEN, Input, MAX_WORDS, Packet};

    const SID: u32 = 0x0BAD_F00D;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn secret(b: u8) -> Secret {
        Secret([b; 16])
    }

    /// 応答役の側から見た向きの鍵（送る向きA1…、受ける向きB2…）。相手は入れ替えた鍵で送る。
    fn mine() -> DirKeys {
        DirKeys::from_raw([0xA1; 32], [0xB2; 32])
    }

    fn peer() -> DirKeys {
        mine().swapped()
    }

    /// 相手の候補は2つ（経路は2つ目）。受信が1つ目から来ても返す先は経路（候補と経路をわざと違える）。
    fn responder() -> PongResponder {
        let peers = Candidates::from_addrs(&[sa("127.0.0.1:4001"), sa("127.0.0.1:4002")]).unwrap();
        PongResponder::new(
            SID,
            AuthKey::derive(&secret(7), SID),
            mine().opener(),
            mine().sealer(Sender::Pong),
            peers,
            sa("127.0.0.1:4002"),
        )
    }

    thread_local! {
        // 検査ごとのPONGの通し番号（検査は別スレッドで走るので各検査の最初のPONGは0番）。
        static SEQ: Seq = const { Seq::new() };
    }

    /// 相手が送る1通（HMACのパケットは秘密`secret_byte`の鍵・封をするパケットは`keys`の送る向き）。
    fn packet_with(secret_byte: u8, keys: DirKeys, sid: u32, msg: &Message) -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = if wire::is_signed(msg) {
            wire::encode(
                &AuthKey::derive(&secret(secret_byte), sid),
                sid,
                msg,
                &mut buf,
            )
        } else {
            wire::seal(&keys.sealer(Sender::Link), &Seq::new(), sid, msg, &mut buf).unwrap()
        };
        buf[..n].to_vec()
    }

    fn packet(sid: u32, msg: &Message) -> Vec<u8> {
        packet_with(7, peer(), sid, msg)
    }

    fn ping(t: u32) -> Vec<u8> {
        packet(SID, &Message::Ping { send_time: t })
    }

    fn ans(r: &PongResponder, from: &str, bytes: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        SEQ.with(|s| ans_with(r, s, from, bytes))
    }

    fn ans_with(
        r: &PongResponder,
        seq: &Seq,
        from: &str,
        bytes: &[u8],
    ) -> Option<(SocketAddr, Vec<u8>)> {
        let mut out = [0u8; MAX_DATAGRAM];
        r.answer(seq, sa(from), bytes, &mut out)
            .map(|(to, n)| (to, out[..n].to_vec()))
    }

    fn read_at_peer(bytes: &[u8]) -> Result<Packet, wire::DecodeError> {
        wire::decode(
            &AuthKey::derive(&secret(7), SID),
            Some(&peer().opener()),
            bytes,
        )
    }

    #[test]
    fn a_real_ping_gets_a_sealed_pong_with_the_same_time_to_the_path() {
        let r = responder();
        let (to, bytes) = ans(&r, "127.0.0.1:4001", &ping(0xDEAD_BEEF)).expect("PONG を返さない");
        assert_eq!(to, sa("127.0.0.1:4002"));
        assert_eq!(
            read_at_peer(&bytes),
            Ok(Packet {
                session_id: SID,
                msg: Message::Pong {
                    send_time: 0xDEAD_BEEF
                }
            })
        );
        // 向きごとの鍵: 応答役の受ける向きの鍵（相手の送信）では開かない。
        assert_eq!(
            wire::decode(
                &AuthKey::derive(&secret(7), SID),
                Some(&mine().opener()),
                &bytes
            ),
            Err(wire::DecodeError::BadTag)
        );
        let field: [u8; NONCE_FIELD_LEN] = bytes[HEADER_LEN..HEADER_LEN + NONCE_FIELD_LEN]
            .try_into()
            .unwrap();
        assert_eq!(read_nonce_field(&field), (Sender::Pong as u8, 0));
        assert!(ans(&r, "127.0.0.1:4002", &ping(1)).is_some());
    }

    #[test]
    fn copies_of_the_responder_never_reuse_a_sequence_number() {
        let seq = Seq::new();
        let a = responder();
        let b = a.clone();
        let c = responder();
        let mut seen = std::collections::HashSet::new();
        for i in 0..30u32 {
            let r = match i % 3 {
                0 => &a,
                1 => &b,
                _ => &c,
            };
            let (_, bytes) = ans_with(r, &seq, "127.0.0.1:4001", &ping(i)).unwrap();
            let field: [u8; NONCE_FIELD_LEN] = bytes[HEADER_LEN..HEADER_LEN + NONCE_FIELD_LEN]
                .try_into()
                .unwrap();
            assert!(seen.insert(field), "{i} 通目で nonce が重なった: {field:?}");
            assert_eq!(read_nonce_field(&field), (Sender::Pong as u8, u64::from(i)));
            assert_eq!(
                read_at_peer(&bytes).map(|p| p.msg),
                Ok(Message::Pong { send_time: i })
            );
        }
        assert_eq!(seq.next(), 30, "3 つの送り手が同じ番号の入れ物を進める");
    }

    #[test]
    fn the_bye_answers_and_the_unasked_bye_reach_the_path_on_the_same_sequence() {
        let seq = Seq::new();
        let r = responder();
        let mut out = [0u8; MAX_DATAGRAM];
        let (to, n) = r.bye(&seq, ByeReason::Cancelled, &mut out).unwrap();
        assert_eq!(to, sa("127.0.0.1:4002"));
        assert_eq!(
            read_at_peer(&out[..n]).map(|p| p.msg),
            Ok(Message::Bye(ByeReason::Cancelled))
        );
        let (to, n) = r
            .answer_bye(
                &seq,
                sa("127.0.0.1:4001"),
                &ping(3),
                ByeReason::Cancelled,
                &mut out,
            )
            .expect("本物の PING に BYE を返さない");
        assert_eq!(to, sa("127.0.0.1:4002"));
        assert_eq!(
            read_at_peer(&out[..n]).map(|p| p.msg),
            Ok(Message::Bye(ByeReason::Cancelled))
        );
        let field: [u8; NONCE_FIELD_LEN] = out[HEADER_LEN..HEADER_LEN + NONCE_FIELD_LEN]
            .try_into()
            .unwrap();
        assert_eq!(read_nonce_field(&field), (Sender::Pong as u8, 1));
        // PINGでない・候補の外の送信元には返さない（ガードは`answer`と同じ）。
        let pong = packet(SID, &Message::Pong { send_time: 3 });
        for (from, bytes) in [("127.0.0.1:4001", &pong), ("127.0.0.1:4003", &ping(3))] {
            assert_eq!(
                r.answer_bye(&seq, sa(from), bytes, ByeReason::Cancelled, &mut out),
                None,
                "{from}"
            );
        }
        assert_eq!(seq.next(), 2, "返さなかった分は番号を使わない");
    }

    #[test]
    fn nothing_is_returned_for_anything_but_a_real_ping() {
        let r = responder();
        let good = ping(42);
        // 隣に「同じサンプルの正しい形なら返す」を並べる。下の否定のテストが、応答役が壊れて何も返さないだけで通ってしまうのを防ぐため。
        assert!(ans(&r, "127.0.0.1:4001", &good).is_some());

        for i in HEADER_LEN..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x01;
            assert_eq!(ans(&r, "127.0.0.1:4001", &bad), None, "{i} B 目");
        }
        for keys in [mine(), DirKeys::from_raw([8; 32], [8; 32])] {
            assert_eq!(
                ans(
                    &r,
                    "127.0.0.1:4001",
                    &packet_with(7, keys, SID, &Message::Ping { send_time: 42 })
                ),
                None
            );
        }
        assert_eq!(
            ans(
                &r,
                "127.0.0.1:4001",
                &packet(SID ^ 1, &Message::Ping { send_time: 42 })
            ),
            None
        );
        // ヘッダのsessionだけ書き換える。ヘッダは追加の認証データなので開かない。
        let mut bad = good.clone();
        bad[8] ^= 0x01;
        assert_eq!(ans(&r, "127.0.0.1:4001", &bad), None, "ヘッダの session");
        assert_eq!(ans(&r, "127.0.0.1:4003", &good), None);
        assert_eq!(ans(&r, "127.0.0.2:4001", &good), None);
        for n in [
            0,
            1,
            HEADER_LEN,
            HEADER_LEN + crate::seal::SEAL_OVERHEAD - 1,
            good.len() - 1,
        ] {
            assert_eq!(ans(&r, "127.0.0.1:4001", &good[..n]), None, "{n} B");
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(ans(&r, "127.0.0.1:4001", &long), None, "1 B 長い");
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(ans(&r, "127.0.0.1:4001", &bad), None, "magic");
    }

    #[test]
    fn a_learned_source_is_answered_like_a_candidate_and_nothing_else_is() {
        let peers = Candidates::from_addrs(&[sa("127.0.0.1:4001")]).unwrap();

        let mut paths = crate::candidate::Paths::new(peers);
        paths.learn(sa("127.0.0.1:4090")).unwrap();
        let learned: Learned = *paths.learned();
        assert_eq!(learned.len(), 1);
        let r = PongResponder::new(
            SID,
            AuthKey::derive(&secret(7), SID),
            mine().opener(),
            mine().sealer(Sender::Pong),
            peers,
            sa("127.0.0.1:4090"),
        );
        assert_eq!(ans(&r, "127.0.0.1:4090", &ping(1)), None);
        let r = r.with_learned(learned);
        let (to, bytes) =
            ans(&r, "127.0.0.1:4090", &ping(0xCAFE)).expect("学んだアドレスに返さない");
        assert_eq!(to, sa("127.0.0.1:4090"));
        assert_eq!(
            read_at_peer(&bytes).map(|p| p.msg),
            Ok(Message::Pong { send_time: 0xCAFE })
        );
        assert_eq!(
            ans(
                &r,
                "127.0.0.1:4090",
                &packet_with(7, mine(), SID, &Message::Ping { send_time: 1 })
            ),
            None
        );
        assert_eq!(
            ans(
                &r,
                "127.0.0.1:4090",
                &packet(SID ^ 1, &Message::Ping { send_time: 1 })
            ),
            None
        );
        assert_eq!(ans(&r, "127.0.0.1:4091", &ping(1)), None);
        assert!(ans(&r, "127.0.0.1:4001", &ping(1)).is_some());
    }

    #[test]
    fn other_messages_get_nothing() {
        let r = responder();
        let mut words = [0u16; MAX_WORDS];
        words[0] = 0x0011;
        let others = [
            Message::Pong { send_time: 42 },
            Message::Bye(ByeReason::Finished),
            Message::Input(Input {
                seg: 0,
                ack: 2,
                base: 2,
                lat: None,
                stamp: crate::wire::Stamp::NONE,
                count: 1,
                words,
            }),
            Message::HelloAck {
                echo_nonce: 1,
                role_nonce: 2,
                public: [3; 32],
                echo_public: [4; 32],
            },
        ];
        for m in others {
            assert_eq!(ans(&r, "127.0.0.1:4001", &packet(SID, &m)), None, "{m:?}");
        }
        // typeだけを別のtypeに書き換えた1通もtypeのガードで落ちる。
        let mut bad = ping(5);
        bad[5] = 9;
        assert_eq!(ans(&r, "127.0.0.1:4001", &bad), None);
    }

    #[test]
    fn poison_bytes_never_panic() {
        // でたらめのバイト列（PINGの先頭を持つものを含む）でもパニックしない。乱数はsplitmix64を数行で。
        let r = responder();
        let head = ping(9);
        let mut s: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut answered = 0;
        for _ in 0..20_000 {
            let len = (next() % 1300) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // 半分はPINGのヘッダ12 Bを先頭に付ける（typeのガードを越えて`Opener`のガードまで届かせる）。
            if next() % 2 == 0 {
                for (d, s) in b.iter_mut().zip(head.iter().take(HEADER_LEN)) {
                    *d = *s;
                }
            }
            if ans(&r, "127.0.0.1:4001", &b).is_some() {
                answered += 1;
            }
        }
        assert_eq!(answered, 0, "でたらめに PONG を返した");
    }
}
