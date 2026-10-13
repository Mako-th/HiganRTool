//! 受信のリング: 受信スレッド（書き手 1 本）が受けた生のデータグラムを書き、ゲームスレッド（読み手 1 本）が tick の始まりと相手を待つ間に取り出す。
//!
//! - 中身の解析はしない（生のバイト列と送り元のアドレスだけ）。例外は PING だけで、受信スレッドがその場で PONG を返し（`net_udp` の
//!   `answer_ping`）、返したことを枠の印（`push_marked` / `pop_marked`）でセッションへ知らせる。
//! - 書き手だけが「書いた数」を `Release` で進め、読み手は `Acquire` で読んでから枠を読む。読み手だけが「読んだ数」を `Release` で進め、
//!   書き手は `Acquire` で読んでから枠を上書きする。枠は原子変数の配列（4 バイトずつ `Relaxed`）なので unsafe が要らない。
//! - 満杯なら新しい方を捨てて数える（lockstep の語は送り直しと冗長送信で埋まる）。`MAX_DATAGRAM` を超えたものも捨てて数える。
//!
//! 読み手の側（`pop`）はゲームスレッドで走るので、確保・ロック待ち・パニックしうる添字を置かない。
#![forbid(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicU32, Ordering};

use higanr_core::wire::MAX_DATAGRAM;

/// 枠の数。ゲームスレッドは 1 フレームに少なくとも 1 回は空にし、相手が送るのは 4 ms ごとの送り直しと PING などなので、
/// 1 フレームに届くのは多くて十数通の見込み。64 あれば溢れない。
pub const RING_SLOTS: usize = 64;
const WORDS: usize = MAX_DATAGRAM.div_ceil(4);
/// 送り元のアドレスを詰める語の数（種類・ポート・アドレス 4 語・flowinfo・scope_id）。
const ADDR_WORDS: usize = 8;

struct Slot {
    len: AtomicU32,
    /// 受けた時刻（µs。下位・上位の 32 bit。書き手が `recv_from` から戻った直後に読んだ時計）。
    t_lo: AtomicU32,
    t_hi: AtomicU32,
    /// 受信スレッドがこの 1 通（PING）にその場で PONG を返したか（0 / 1。`higanr_core::pong`）。
    pong_sent: AtomicU32,
    addr: [AtomicU32; ADDR_WORDS],
    data: [AtomicU32; WORDS],
}

impl Slot {
    const fn new() -> Self {
        Slot {
            len: AtomicU32::new(0),
            t_lo: AtomicU32::new(0),
            t_hi: AtomicU32::new(0),
            pong_sent: AtomicU32::new(0),
            addr: [const { AtomicU32::new(0) }; ADDR_WORDS],
            data: [const { AtomicU32::new(0) }; WORDS],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RingCounts {
    /// 書いた数（捨てたものは含まない）。
    pub pushed: u32,
    pub dropped_full: u32,
    pub dropped_big: u32,
}

pub struct RecvRing {
    slots: [Slot; RING_SLOTS],
    written: AtomicU32,
    read: AtomicU32,
    dropped_full: AtomicU32,
    dropped_big: AtomicU32,
}

impl Default for RecvRing {
    fn default() -> Self {
        Self::new()
    }
}

fn encode_addr(a: SocketAddr) -> [u32; ADDR_WORDS] {
    match a {
        SocketAddr::V4(v4) => {
            let ip = u32::from_be_bytes(v4.ip().octets());
            [4, u32::from(v4.port()), ip, 0, 0, 0, 0, 0]
        }
        SocketAddr::V6(v6) => {
            let ip = u128::from(*v6.ip());
            [
                6,
                u32::from(v6.port()),
                (ip >> 96) as u32,
                (ip >> 64) as u32,
                (ip >> 32) as u32,
                ip as u32,
                v6.flowinfo(),
                v6.scope_id(),
            ]
        }
    }
}

fn decode_addr(w: &[u32; ADDR_WORDS]) -> SocketAddr {
    let [kind, port, a, b, c, d, flow, scope] = *w;
    let port = port as u16;
    if kind == 6 {
        let ip =
            (u128::from(a) << 96) | (u128::from(b) << 64) | (u128::from(c) << 32) | u128::from(d);
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, flow, scope))
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(a.to_be_bytes())), port)
    }
}

impl RecvRing {
    pub const fn new() -> Self {
        RecvRing {
            slots: [const { Slot::new() }; RING_SLOTS],
            written: AtomicU32::new(0),
            read: AtomicU32::new(0),
            dropped_full: AtomicU32::new(0),
            dropped_big: AtomicU32::new(0),
        }
    }

    /// 書き手（受信スレッド）: 1 通を、受けた時刻 `t_us` と一緒に書く。捨てたら false。印なし（リングの検査が使う短い形）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn push(&self, from: SocketAddr, bytes: &[u8], t_us: u64) -> bool {
        self.push_marked(from, bytes, t_us, false)
    }

    /// `push` と同じ。`pong_sent` は、受信スレッドがこの 1 通にその場で PONG を返したか（読み手のセッションが PONG を重ねないための印）。
    pub fn push_marked(&self, from: SocketAddr, bytes: &[u8], t_us: u64, pong_sent: bool) -> bool {
        if bytes.len() > MAX_DATAGRAM {
            self.dropped_big.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let w = self.written.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        if w.wrapping_sub(r) as usize >= RING_SLOTS {
            self.dropped_full.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let Some(slot) = self.slots.get(w as usize % RING_SLOTS) else {
            return false;
        };
        for (dst, chunk) in slot.data.iter().zip(bytes.chunks(4)) {
            let mut b = [0u8; 4];
            for (x, y) in b.iter_mut().zip(chunk) {
                *x = *y;
            }
            dst.store(u32::from_le_bytes(b), Ordering::Relaxed);
        }
        for (dst, v) in slot.addr.iter().zip(encode_addr(from)) {
            dst.store(v, Ordering::Relaxed);
        }
        slot.len.store(bytes.len() as u32, Ordering::Relaxed);
        slot.t_lo.store(t_us as u32, Ordering::Relaxed);
        slot.t_hi.store((t_us >> 32) as u32, Ordering::Relaxed);
        slot.pong_sent
            .store(u32::from(pong_sent), Ordering::Relaxed);
        self.written.store(w.wrapping_add(1), Ordering::Release);
        true
    }

    /// 読み手（ゲームスレッド）: 1 通を `buf` へコピーして取り出す（送り元・長さ・受けた時刻）。無ければ None。確保しない。
    pub fn pop(&self, buf: &mut [u8; MAX_DATAGRAM]) -> Option<(SocketAddr, usize, u64)> {
        self.pop_marked(buf).map(|(a, n, t, _)| (a, n, t))
    }

    /// `pop` と同じ。4 つ目は書き手が添えた「PONG を返した」の印（`push_marked`）。確保しない。
    pub fn pop_marked(
        &self,
        buf: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize, u64, bool)> {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.written.load(Ordering::Acquire);
        if r == w {
            return None;
        }
        let slot = self.slots.get(r as usize % RING_SLOTS)?;
        let len = (slot.len.load(Ordering::Relaxed) as usize).min(MAX_DATAGRAM);
        let t_us = u64::from(slot.t_hi.load(Ordering::Relaxed)) << 32
            | u64::from(slot.t_lo.load(Ordering::Relaxed));
        let pong_sent = slot.pong_sent.load(Ordering::Relaxed) != 0;
        for (chunk, src) in buf.chunks_mut(4).zip(slot.data.iter()) {
            let b = src.load(Ordering::Relaxed).to_le_bytes();
            for (x, y) in chunk.iter_mut().zip(b) {
                *x = y;
            }
        }
        let mut aw = [0u32; ADDR_WORDS];
        for (dst, src) in aw.iter_mut().zip(slot.addr.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        // 枠を読み終えてから「読んだ数」を進める（書き手はそれを見てから上書きする）。
        self.read.store(r.wrapping_add(1), Ordering::Release);
        Some((decode_addr(&aw), len, t_us, pong_sent))
    }

    pub fn counts(&self) -> RingCounts {
        RingCounts {
            pushed: self.written.load(Ordering::Relaxed),
            dropped_full: self.dropped_full.load(Ordering::Relaxed),
            dropped_big: self.dropped_big.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    //! 2 本のスレッドの間で書き終える前の枠を読まないこと（`Release` / `Acquire` の抜け）と、取り出しで確保しないことも見る。
    use super::*;
    use std::sync::Arc;

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn pop_vec(r: &RecvRing) -> Option<(SocketAddr, Vec<u8>)> {
        let mut buf = [0u8; MAX_DATAGRAM];
        r.pop(&mut buf).map(|(a, n, _)| (a, buf[..n].to_vec()))
    }

    #[test]
    fn bytes_lengths_and_addresses_round_trip_in_order() {
        let r = RecvRing::new();
        let v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 5000, 7, 3));
        let odd: Vec<u8> = (0..7u8).collect();
        let full: Vec<u8> = (0..MAX_DATAGRAM).map(|i| (i * 31) as u8).collect();
        let big_t = (5u64 << 32) | 0x8000_0001;
        assert!(r.push(v4(4000), &odd, 17));
        assert!(r.push(v6, &full, big_t));
        assert!(r.push(v4(4001), &[], 0));
        let mut buf = [0u8; MAX_DATAGRAM];
        assert_eq!(r.pop(&mut buf).map(|x| x.2), Some(17));
        assert_eq!(buf[..odd.len()], odd[..]);
        assert_eq!(r.pop(&mut buf), Some((v6, MAX_DATAGRAM, big_t)));
        assert_eq!(buf[..], full[..]);
        assert_eq!(r.pop(&mut buf), Some((v4(4001), 0, 0)));
        assert_eq!(pop_vec(&r), None);
        assert_eq!(r.counts().pushed, 3);
    }

    #[test]
    fn pong_mark_travels_with_each_datagram() {
        let r = RecvRing::new();
        let mut buf = [0u8; MAX_DATAGRAM];
        for round in 0..2u32 {
            for i in 0..RING_SLOTS as u32 {
                let mark = (i + round) % 2 == 0;
                assert!(r.push_marked(v4(1), &[i as u8], u64::from(i), mark));
            }
            for i in 0..RING_SLOTS as u32 {
                let want = (i + round) % 2 == 0;
                assert_eq!(
                    r.pop_marked(&mut buf).map(|x| (x.1, x.3)),
                    Some((1, want)),
                    "{round} 周目の枠 {i}"
                );
            }
        }
        assert!(r.push_marked(v4(1), &[1], 0, true));
        assert!(r.pop(&mut buf).is_some());
        assert!(r.push(v4(1), &[2], 0));
        assert_eq!(r.pop_marked(&mut buf).map(|x| x.3), Some(false));
    }

    #[test]
    fn full_ring_drops_the_newest_and_keeps_unread_slots() {
        let r = RecvRing::new();
        for i in 0..RING_SLOTS {
            assert!(r.push(v4(1), &[i as u8], i as u64));
        }
        assert!(!r.push(v4(1), &[0xEE], 0), "満杯なのに書いた");
        assert!(
            !r.push(v4(1), &[0u8; MAX_DATAGRAM + 1], 0),
            "大きすぎるものを書いた"
        );
        let c = r.counts();
        assert_eq!((c.pushed, c.dropped_full, c.dropped_big), (64, 1, 1));
        for i in 0..RING_SLOTS {
            assert_eq!(pop_vec(&r), Some((v4(1), vec![i as u8])), "枠 {i}");
        }
        assert_eq!(pop_vec(&r), None);
        assert!(r.push(v4(2), &[9], 0));
        assert_eq!(pop_vec(&r), Some((v4(2), vec![9])));
    }

    #[test]
    fn two_threads_see_every_datagram_whole_and_in_order() {
        // 書き手は通し番号を全部のバイトに書く。読み手は、どの枠も全部のバイトが同じ番号で、番号が
        const N: u32 = 20_000;
        // 時刻は上位と下位の 32 bit がどちらも番号で変わる値（片方だけコピーすると食い違う）。
        fn stamp(n: u32) -> u64 {
            (u64::from(n) << 32) | u64::from(n.wrapping_mul(0x9E37_79B9))
        }
        // 時間切れ（どちらのループも 30 秒で落とす。止まった検査が全体を止めない）。読み手が落ちたら
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
        struct Abort(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _abort = Abort(Arc::clone(&aborted));
        let started = std::time::Instant::now();
        let r = Arc::new(RecvRing::new());
        let w = Arc::clone(&r);
        let aborted_w = Arc::clone(&aborted);
        let writer = std::thread::spawn(move || {
            let mut n = 0u32;
            while n < N {
                if aborted_w.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                assert!(
                    started.elapsed() < LIMIT,
                    "書き手が 30 秒で終わらない: n={n}"
                );
                let len = 4 + (n as usize * 7) % (MAX_DATAGRAM - 4);
                let mut buf = vec![0u8; len];
                for c in buf.chunks_mut(4) {
                    for (x, y) in c.iter_mut().zip(n.to_le_bytes()) {
                        *x = y;
                    }
                }
                if w.push(v4((n % 60_000) as u16 + 1), &buf, stamp(n)) {
                    n += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut want = 0u32;
        while want < N {
            let Some((from, len, t)) = r.pop(&mut buf) else {
                assert!(
                    started.elapsed() < LIMIT,
                    "読み手が 30 秒で終わらない: want={want}"
                );
                std::thread::yield_now();
                continue;
            };
            assert_eq!(len, 4 + (want as usize * 7) % (MAX_DATAGRAM - 4));
            assert_eq!(t, stamp(want), "枠 {want} の時刻");
            assert_eq!(from, v4((want % 60_000) as u16 + 1));
            for c in buf[..len].chunks(4) {
                let mut b = [0u8; 4];
                b[..c.len()].copy_from_slice(c);
                let got = u32::from_le_bytes(b) & (u32::MAX >> (8 * (4 - c.len())));
                assert_eq!(got, want & (u32::MAX >> (8 * (4 - c.len()))), "枠 {want}");
            }
            want += 1;
        }
        writer.join().unwrap();
        assert_eq!(r.counts().pushed, N);
    }

    #[test]
    fn pop_never_allocates() {
        let r = RecvRing::new();
        r.push(v4(1), &[1, 2, 3], 0);
        let mut buf = [0u8; MAX_DATAGRAM];
        let before = crate::test_alloc::count();
        let got = r.pop(&mut buf);
        let none = r.pop(&mut buf);
        let used = crate::test_alloc::count() - before;
        assert_eq!(used, 0, "取り出しで確保した");
        assert_eq!(got.map(|(_, n, _)| n), Some(3));
        assert!(none.is_none());
    }
}
