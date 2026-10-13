//! 偽の STUN の応答役（検査の道具）。本物の STUN サーバーへの問い合わせは外へ出る通信なので、本人の許可の後。それまでは loopback に立てた
//! これで、dll と peer_sim の「STUN の Binding を送って受ける口」を見る。受けた Binding Request に、送り元のアドレスを
//! XOR-MAPPED-ADDRESS に載せた Success Response を返す（本物と同じ答え方。loopback なので答えは 127.0.0.1:ポート）。組み立ては
//! `higanr_core::stun::encode_binding_success`。既定は受信ソケットも相手も loopback だけ。
//!
//! ネットワークの範囲を広げる形（`spawn_in`）: 問い合わせの送り元のガードを `guard::allows(scope, 送り元)` にする。受信ソケットは `127.0.0.1` のままにできる ——
//! `bind = auto` の dll は LAN のアドレスから送るので送り元は LAN のアドレスになるが、宛先は同じ PC の 127.0.0.1 なので、Windows のファイアウォールの
//! 確かめは出ない。受信ソケット自体も範囲のガード（`guard::check_bind`）を通せば LAN のアドレスに開ける（範囲を広げた引数のときだけ）。

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use higanr_core::guard::{self, Scope};
use higanr_core::stun::{self, MAX_SUCCESS_LEN};

/// 単体で起こす形（`peer_sim --stun-fake ADDR`。LAN の回で手で起こして落とす）の引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub bind: SocketAddr,
    pub scope: Scope,
    /// 自分で終わる秒数（落とし忘れて受信ソケットが残らないための上限）。
    pub max_sec: u32,
}

/// `Args::max_sec` の既定（30 分。実機の回の `limit_min` は最長 60 分で、STUN は接続の始めに 1 回聞かれるだけなので足りる）。
pub const DEFAULT_MAX_SEC: u32 = 1_800;

pub const MAX_MAX_SEC: u32 = 7_200;

/// 走っている間に検査が回せるつまみ（聞き直しの検査が「答えのポートを途中で変える」「答えを止める」のに使う）。
struct Dials {
    shift: AtomicU16,
    /// true の間は、問い合わせを数えるだけで答えない（答えの返らない STUN）。
    silent: AtomicBool,
    received: AtomicU32,
}

pub struct StunFake {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    dials: Arc<Dials>,
    answered: Arc<AtomicU32>,
    last_from: Arc<Mutex<Option<SocketAddr>>>,
    join: Option<JoinHandle<()>>,
}

impl StunFake {
    /// `127.0.0.1` の空いたポートに立てる。見えたアドレスは送り元のまま（宛先に依らない割り当てのルーターの形）。
    pub fn spawn() -> Result<StunFake, String> {
        Self::spawn_shifted(0)
    }

    /// `spawn` と同じで、答えに載せるポートを送り元のポート + `shift`（65535 を越えたら回り、0 になれば 1）にする。宛先に依って外側のポートが
    /// 変わるルーター（RFC 4787 の endpoint-dependent mapping）を模す検査用（`--stun-probe` の「ポートが違う」の形）。
    pub fn spawn_shifted(shift: u16) -> Result<StunFake, String> {
        Self::spawn_in(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            Scope::Loopback,
            shift,
        )
    }

    /// `bind` に立て、`scope` の範囲の相手だけに答える（`spawn` は `127.0.0.1:0` と `Scope::Loopback`）。
    /// 受信ソケットは開く前に範囲のガード（`guard::check_bind`。0.0.0.0 など未指定アドレスも断る）を通す。
    pub fn spawn_in(bind: SocketAddr, scope: Scope, shift: u16) -> Result<StunFake, String> {
        guard::check_bind(bind, scope, "受信ソケット")?;
        let sock = UdpSocket::bind(bind).map_err(|e| format!("{bind} で待てない: {e}"))?;
        let addr = sock
            .local_addr()
            .map_err(|e| format!("受信ソケットが分からない: {e}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(|e| format!("受信の待ちの上限を置けない: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicU32::new(0));
        let last_from = Arc::new(Mutex::new(None));
        let dials = Arc::new(Dials {
            shift: AtomicU16::new(shift),
            silent: AtomicBool::new(false),
            received: AtomicU32::new(0),
        });
        let join = {
            let stop = Arc::clone(&stop);
            let answered = Arc::clone(&answered);
            let last_from = Arc::clone(&last_from);
            let dials = Arc::clone(&dials);
            std::thread::Builder::new()
                .name("stun-fake".into())
                .spawn(move || serve(&sock, &stop, &answered, &last_from, scope, &dials))
                .map_err(|e| format!("スレッドを起こせない: {e}"))?
        };
        Ok(StunFake {
            addr,
            stop,
            dials,
            answered,
            last_from,
            join: Some(join),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn answered(&self) -> u32 {
        self.answered.load(Ordering::Relaxed)
    }

    pub fn last_from(&self) -> Option<SocketAddr> {
        self.last_from.lock().ok().and_then(|g| *g)
    }

    /// 以後の答えに載せるポートのずれを変える（走っている間に外側のポートが付け替わる形を作る。検査用）。
    pub fn set_shift(&self, shift: u16) {
        self.dials.shift.store(shift, Ordering::Relaxed);
    }

    /// true の間は答えない（問い合わせは `received` に数える。検査用）。
    pub fn set_silent(&self, silent: bool) {
        self.dials.silent.store(silent, Ordering::Relaxed);
    }

    /// 受けた Binding Request の数（答えなかったものを含む）。`answered` は答えた数。
    pub fn received(&self) -> u32 {
        self.dials.received.load(Ordering::Relaxed)
    }
}

fn serve(
    sock: &UdpSocket,
    stop: &AtomicBool,
    answered: &AtomicU32,
    last_from: &Mutex<Option<SocketAddr>>,
    scope: Scope,
    dials: &Dials,
) {
    let mut buf = [0u8; 1500];
    let mut out = [0u8; MAX_SUCCESS_LEN];
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let shift = dials.shift.load(Ordering::Relaxed);
        let Some(len) = reply_for(scope, from, buf.get(..n).unwrap_or(&[]), shift, &mut out) else {
            continue;
        };
        dials.received.fetch_add(1, Ordering::Relaxed);
        if dials.silent.load(Ordering::Relaxed) {
            continue;
        }
        // 送る前に数える（送った後に数えると、クライアントが答えを受けて `answered()` を見る方が先になり「答えを受けたのに 0」と読める）。
        // 送りに失敗したら数えと送り元を戻す。
        let prev = last_from.lock().ok().and_then(|g| *g);
        if let Ok(mut g) = last_from.lock() {
            *g = Some(from);
        }
        answered.fetch_add(1, Ordering::SeqCst);
        if sock.send_to(&out[..len], from).is_err() {
            answered.fetch_sub(1, Ordering::SeqCst);
            if let Ok(mut g) = last_from.lock() {
                *g = prev;
            }
        }
    }
}

/// 受けた 1 通への答え（`out` に書いた長さ）。範囲の外の送り元と、Binding Request でないものには答えない（None）。送り元のアドレスを持たない
/// 検査でもガードを見られるよう、socket から切り離してある。
fn reply_for(
    scope: Scope,
    from: SocketAddr,
    request: &[u8],
    shift: u16,
    out: &mut [u8; MAX_SUCCESS_LEN],
) -> Option<usize> {
    if !guard::allows(scope, from) {
        return None;
    }
    let txn = stun::binding_request_txn(request)?;
    let seen = SocketAddr::new(from.ip(), shifted_port(from.port(), shift));
    Some(stun::encode_binding_success(txn, seen, out))
}

/// 答えに載せるポート（0 はアドレスとして使えないので 1 に）。検査が「見えるはずのアドレス」を作るのに使う。
pub fn shifted_port(port: u16, shift: u16) -> u16 {
    match port.wrapping_add(shift) {
        0 => 1,
        p => p,
    }
}

impl Drop for StunFake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_a_binding_request_with_the_senders_address() {
        let f = StunFake::spawn().unwrap();
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let me = c.local_addr().unwrap();
        c.send_to(b"HGRT-not-stun-at-all", f.addr()).unwrap();
        let txn = [9u8; 12];
        c.send_to(&stun::encode_binding_request(txn), f.addr())
            .unwrap();
        let mut buf = [0u8; 256];
        let (n, from) = c.recv_from(&mut buf).unwrap();
        assert_eq!(from, f.addr());
        assert_eq!(stun::decode_binding_response(&buf[..n], &txn), Ok(me));
        // 応答役は送ってから数を足すので、受けた直後はまだ 0 のことがある。少し待って見る。
        let t0 = std::time::Instant::now();
        while f.answered() == 0 && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(f.answered(), 1);
    }

    #[test]
    fn shifted_answers_carry_the_moved_port_and_zero_shift_is_plain() {
        assert_eq!(
            [
                shifted_port(40_000, 0),
                shifted_port(40_000, 7),
                shifted_port(65_535, 1),
                shifted_port(65_535, 2)
            ],
            [40_000, 40_007, 1, 1]
        );
        let ask = |f: &StunFake| {
            let c = UdpSocket::bind("127.0.0.1:0").unwrap();
            c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let txn = [5u8; 12];
            c.send_to(&stun::encode_binding_request(txn), f.addr())
                .unwrap();
            let mut buf = [0u8; 256];
            let (n, _) = c.recv_from(&mut buf).unwrap();
            (
                c.local_addr().unwrap(),
                stun::decode_binding_response(&buf[..n], &txn).unwrap(),
            )
        };
        let plain = StunFake::spawn_shifted(0).unwrap();
        let (me, seen) = ask(&plain);
        assert_eq!(seen, me);
        let moved = StunFake::spawn_shifted(3).unwrap();
        let (me, seen) = ask(&moved);
        assert_eq!(seen.ip(), me.ip());
        assert_eq!(seen.port(), shifted_port(me.port(), 3));
        assert_ne!(seen, me);
    }

    #[test]
    fn dials_change_the_answer_while_running_and_silence_still_counts_requests() {
        let f = StunFake::spawn().unwrap();
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        c.set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let me = c.local_addr().unwrap();
        let ask = |n: u8| {
            let txn = [n; 12];
            c.send_to(&stun::encode_binding_request(txn), f.addr())
                .unwrap();
            let mut buf = [0u8; 256];
            c.recv_from(&mut buf)
                .ok()
                .map(|(len, _)| stun::decode_binding_response(&buf[..len], &txn).unwrap())
        };
        assert_eq!(ask(1), Some(me), "つまみを触らないのに送り元と違う");
        f.set_shift(4);
        assert_eq!(
            ask(2).map(|a| a.port()),
            Some(shifted_port(me.port(), 4)),
            "走っている間のずれが効かない"
        );
        f.set_silent(true);
        assert_eq!(ask(3), None, "silent なのに答えた");
        c.send_to(b"HGRT-not-stun-at-all", f.addr()).unwrap();
        f.set_silent(false);
        assert_eq!(ask(4).map(|a| a.port()), Some(shifted_port(me.port(), 4)));
        assert_eq!(f.received(), 4);
    }

    #[test]
    fn the_scope_gates_the_senders_and_the_bind() {
        let a = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let from = [
            a("127.0.0.1:40000"),
            a("192.168.11.6:47001"),
            a("10.1.2.3:47001"),
            a("203.0.113.9:47001"),
            a("[::ffff:192.168.11.6]:47001"),
            a("[fd00::6]:47001"),
            a("[2001:db8::6]:47001"),
        ];
        let req = stun::encode_binding_request([7u8; 12]);
        let row = |scope: Scope| {
            from.map(|f| {
                let mut out = [0u8; MAX_SUCCESS_LEN];
                let r = reply_for(scope, f, &req, 0, &mut out);
                if let Some(n) = r {
                    assert_eq!(stun::decode_binding_response(&out[..n], &[7u8; 12]), Ok(f));
                }
                r.is_some()
            })
        };
        let mut out = [0u8; MAX_SUCCESS_LEN];
        assert_eq!(
            reply_for(Scope::Lan, from[1], b"HGRT-not-stun-at-all", 0, &mut out),
            None
        );
        assert_eq!(
            row(Scope::Loopback),
            [true, false, false, false, false, false, false]
        );
        assert_eq!(
            row(Scope::Lan),
            [true, true, true, false, false, true, false]
        );
        for bind in ["192.168.11.6:0", "10.1.2.3:0"] {
            let e = StunFake::spawn_in(a(bind), Scope::Loopback, 0)
                .err()
                .unwrap();
            assert!(
                e.contains("scope = loopback") && e.contains("lan"),
                "{bind}: {e}"
            );
        }
        for bind in ["203.0.113.9:0", "0.0.0.0:0", "[::]:0"] {
            assert!(
                StunFake::spawn_in(a(bind), Scope::Lan, 0).is_err(),
                "{bind}"
            );
        }
        assert!(StunFake::spawn().is_ok());
        assert!(StunFake::spawn_in(a("127.0.0.1:0"), Scope::Lan, 0).is_ok());
    }
}
