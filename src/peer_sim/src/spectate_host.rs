//! 観戦のホストの側のポート（`--spectate-host`）。
//!
//! peer_sim が対戦のホストのとき、core の `SpectateHost`（記録のリングバッファ・観戦者ごとの送りウィンドウ）を持ち、使うと決まった
//! 記録（SELECT・選択の押下・MENU の入力ワード・マッチの入力ワード・終わり）を積んで観戦者へ流す。dll 無しに「peer_sim 対 peer_sim ＋
//! 観戦者」を loopback で回すための道具。
//!
//! - 別の受信ソケットを持つ（対戦の受信ソケットとは別のソケット）。観戦用のコードの session・秘密は対戦のものと別に引く
//!   （同じ値だと観戦者が対戦の鍵を作れてしまう —— `higanr_core::spectate` の `HostConfig`）。
//! - 1 本のスレッドで回す（対戦の 1 ms の周回の中で、受信・送り・積みを 1 回ずつ）。増えるのは 1 ms あたり受信 1 回と
//!   送りの判断だけで、観戦者が遅れても黙っても対戦者は待たない（検査 `silent_spectator_*`）。
//! - わざと壊す: 観戦者への送り（STREAM・BYE）に `FaultConfig` を掛けられる。CLI の手段は無い（中継 `--shape` を挟めばよい）。
//! - ホストの役のときだけ流す。ゲストになった走行では何も積まず、生きている合図（空の STREAM）だけが出る（報告の `active` が false）。

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::guard::{self, Scope};
use higanr_core::seal::KeySeed;
use higanr_core::spectate::{HostConfig, HostCounts, PushError, SpectateHost};
use higanr_core::wire::{ByeReason, Record};

use crate::fault::{FaultConfig, FaultCounts};
use crate::sim::{At, SimError, Tx};

/// 終わりの記録の後、観戦者が受け取り終えるのを待つ長さ（ms）。黙った観戦者に走行の終わりを引き延ばされないための上限。
pub const DRAIN_MS: u32 = 1_000;

#[derive(Debug)]
pub struct SpectateHostOpts {
    /// 観戦者を受けるソケット（ネットワークの範囲の中のアドレスに bind したもの。`run_opts` が複製して使う）。
    pub sock: UdpSocket,
    /// 観戦用のコードの session と秘密（対戦のものと別の値）。
    pub session_id: u32,
    pub secret: Secret,
    /// 配る側の使い捨ての鍵の種。通常は走るたびに新しい種（`keys::fresh_seed`）。固定の種（`keys::spectate_host_seed`）は
    /// 同じ秘密・session で起こし直すと同じ鍵で通し番号が 0 から数え直しになるので、観戦者が式で公開鍵を作る検査の 1 回きりの実行だけに使う。
    pub key_seed: KeySeed,
    pub fault: FaultConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpectateHostReport {
    /// ホストの役で、記録を積んだか（false = ゲストになったか、繋がらなかった）。
    pub active: bool,
    pub counts: HostCounts,
    pub faults: FaultCounts,
    pub tail: u32,
    /// 終わりに残っていた観戦者の（送り元, 次に要る番号）。`tail` に届いていれば全部受け取り終えている。
    pub spectators: Vec<(SocketAddr, u32)>,
    /// 受け取り終えるのを待った長さ（ms。`DRAIN_MS` が上限）。
    pub drain_ms: u32,
    pub sends: u64,
    pub send_errors: u64,
    pub send_refused: u64,
}

pub(crate) struct SpectateSide {
    host: SpectateHost,
    tx: Tx,
    ended: bool,
    active: bool,
    drain_ms: u32,
}

/// 走行の終わり方に対応する BYE の理由（観戦の `Record::End`）。繋がる前の失敗は記録が無いので None。
pub(crate) fn end_reason(end: &crate::sim::End) -> Option<ByeReason> {
    use crate::sim::End;
    match end {
        End::Finished { .. } | End::Deadline => Some(ByeReason::Finished),
        End::Desync { .. } => Some(ByeReason::Desync),
        End::MenuDiverged { .. } => Some(ByeReason::Cancelled),
        End::PeerBye(_) | End::Dead => Some(ByeReason::Disconnected),
        End::HandshakeFailed(_) => None,
    }
}

impl SpectateSide {
    /// ソケットを開く。受信ソケットがネットワークの範囲の外なら断る（ガード）。
    pub(crate) fn open(o: &SpectateHostOpts, scope: Scope) -> Result<SpectateSide, SimError> {
        let local = o.sock.local_addr()?;
        guard::check_bind(local, scope, "観戦の受信ソケット").map_err(SimError::Refused)?;
        o.fault.validate().map_err(SimError::Refused)?;
        let sock = o.sock.try_clone()?;
        sock.set_nonblocking(true)?;
        Ok(SpectateSide {
            host: SpectateHost::new(HostConfig {
                session_id: o.session_id,
                secret: o.secret,
                // 配る側の種は呼び手が決める。
                key_seed: o.key_seed,
            }),
            tx: Tx::new(sock, o.fault, scope),
            ended: false,
            active: false,
            drain_ms: 0,
        })
    }

    pub(crate) fn push(&mut self, rec: Record) {
        self.active = true;
        if matches!(rec, Record::End(_)) {
            if self.ended {
                return;
            }
            self.ended = true;
        }
        let _: Result<u32, PushError> = self.host.push(rec);
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    pub(crate) fn pump(&mut self, now: u32) -> Result<(), SimError> {
        let mut buf = [0u8; 2048];
        loop {
            match self.tx.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let bytes = buf.get(..n).unwrap_or(&[]);
                    let _ = self.host.on_datagram(
                        from,
                        bytes,
                        now,
                        &mut At {
                            tx: &mut self.tx,
                            now,
                        },
                    );
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // 観戦者が先に閉じたポートへの送りの後の受信が WSAECONNRESET になる（対戦のポートと同じ）。読み飛ばす。
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(SimError::Io(e)),
            }
        }
        self.host.poll(
            now,
            &mut At {
                tx: &mut self.tx,
                now,
            },
        );
        self.tx.flush(now);
        Ok(())
    }

    fn all_caught_up(&self) -> bool {
        let tail = self.host.tail();
        self.host.spectators().all(|(_, _, ack)| ack >= tail)
    }

    /// 走行の終わりに、いる観戦者が受け取り終える（終わりの記録まで）のを `DRAIN_MS` まで待つ。
    pub(crate) fn drain(&mut self, t0: Instant) -> Result<(), SimError> {
        let start = Instant::now();
        while !self.all_caught_up() && start.elapsed().as_millis() < u128::from(DRAIN_MS) {
            let now = (t0.elapsed().as_micros() / 1_000) as u32;
            self.pump(now)?;
            std::thread::sleep(Duration::from_millis(1));
        }
        self.drain_ms = start.elapsed().as_millis() as u32;
        Ok(())
    }

    pub(crate) fn report(&self) -> SpectateHostReport {
        SpectateHostReport {
            active: self.active,
            counts: self.host.counts(),
            faults: self.tx.faults.counts(),
            tail: self.host.tail(),
            spectators: self.host.spectators().map(|(a, _, ack)| (a, ack)).collect(),
            drain_ms: self.drain_ms,
            sends: self.tx.sends,
            send_errors: self.tx.send_errors,
            send_refused: self.tx.send_refused,
        }
    }
}

#[cfg(test)]
mod tests {
    //! 実時間は 0.3 秒ほど（loopback の実 UDP）。
    use super::*;
    use higanr_core::auth::AuthKey;
    use higanr_core::wire::{self, MAX_DATAGRAM, Message, NAME_MAX, SpectateJoin};

    const SESSION: u32 = 0x5EC7_0001;
    const SECRET: Secret = Secret(*b"spectate-test-01");

    fn opts() -> SpectateHostOpts {
        SpectateHostOpts {
            sock: UdpSocket::bind("127.0.0.1:0").unwrap(),
            session_id: SESSION,
            secret: SECRET,
            key_seed: crate::keys::fresh_seed(),
            fault: FaultConfig::default(),
        }
    }

    fn join_bytes(secret: &Secret, session: u32, nonce: u64) -> Vec<u8> {
        let key = AuthKey::derive(secret, session);
        let mut name = [0u8; NAME_MAX];
        name[..3].copy_from_slice(b"spy");
        let msg = Message::SpectateJoin(SpectateJoin {
            name_len: 3,
            nonce,
            name,
            build: higanr_core::update::BUILD_NUMBER,
            public: higanr_core::seal::Ephemeral::from_seed(&crate::keys::fresh_seed()).public(),
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = wire::encode(&key, session, &msg, &mut buf);
        buf[..n].to_vec()
    }

    fn pump_and_count(side: &mut SpectateSide, sock: &UdpSocket, ms: u64) -> usize {
        sock.set_nonblocking(true).unwrap();
        let t0 = Instant::now();
        let mut got = 0;
        let mut buf = [0u8; 2048];
        while t0.elapsed() < Duration::from_millis(ms) {
            side.pump((t0.elapsed().as_millis()) as u32).unwrap();
            while sock.recv_from(&mut buf).is_ok() {
                got += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        got
    }

    #[test]
    fn join_with_a_wrong_secret_or_session_is_not_streamed() {
        let o = opts();
        let host_addr = o.sock.local_addr().unwrap();
        let mut side = SpectateSide::open(&o, Scope::Loopback).unwrap();
        for k in 0..10u16 {
            side.push(Record::Seg { seg: k });
        }
        let spy = UdpSocket::bind("127.0.0.1:0").unwrap();
        // 秘密が違う（タグ違い）・session が違う（ヘッダの session が観戦用のものでない）。
        spy.send_to(
            &join_bytes(&Secret(*b"another-secret-1"), SESSION, 7),
            host_addr,
        )
        .unwrap();
        spy.send_to(&join_bytes(&SECRET, SESSION ^ 1, 7), host_addr)
            .unwrap();
        let got = pump_and_count(&mut side, &spy, 150);
        let r = side.report();
        assert_eq!(got, 0, "観戦者でない者へ流した");
        assert_eq!((r.counts.bad_tag, r.counts.wrong_session), (1, 1), "{r:?}");
        assert_eq!(r.counts.joined, 0);
        // 対照: 正しい JOIN には流し始める（上の 0 が「口が壊れていた」ではないこと）。
        spy.send_to(&join_bytes(&SECRET, SESSION, 7), host_addr)
            .unwrap();
        let got = pump_and_count(&mut side, &spy, 150);
        assert!(got >= 1, "正しい JOIN に流さない");
        assert_eq!(side.report().counts.joined, 1);
    }

    #[test]
    fn end_reasons_follow_how_the_run_ended() {
        use crate::sim::End;
        assert_eq!(
            end_reason(&End::Finished { peer_bye: true }),
            Some(ByeReason::Finished)
        );
        assert_eq!(end_reason(&End::Deadline), Some(ByeReason::Finished));
        assert_eq!(end_reason(&End::Dead), Some(ByeReason::Disconnected));
        assert_eq!(
            end_reason(&End::PeerBye(ByeReason::Cancelled)),
            Some(ByeReason::Disconnected)
        );
        assert_eq!(
            end_reason(&End::HandshakeFailed(
                higanr_core::handshake::FailReason::NoHandshake
            )),
            None
        );
    }
}
