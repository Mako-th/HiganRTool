//! 受信のイベントと、ゲームスレッドが相手を待つ待ち（Win32 の層。unsafe はここにもある。1 つずつ `// SAFETY:`）。
//!
//! 受信スレッドは 1 通書くたびにイベント（自動リセット）を立てる。ゲームスレッドは、イベントと高分解能の待機タイマー
//! （`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`。Windows 10 1803 から）を `WaitForMultipleObjects` で待つ。届けばすぐ起き、
//! 届かなければタイマーの時刻に起きる。`WaitForSingleObject` の待ち時間だけで待つと OS の時計の分解能（既定 15.6 ms）に
//! 丸められ、4 ms の送り直しと 1 フレームの予算が崩れる。タイマーが作れなければ、イベントだけを ms で待つ
//! （`high_resolution()` が false。ログに出す）。
//!
//! ここは待ち方だけを持つ。何を待つか・いつまで待つかは `netplay.rs`。OS の乱数（`os_random`）もここに置く: 接続の秘密と
//! STUN の transaction id に使う。暗号は自作しないので、Windows の `BCryptGenRandom`
//! （`BCRYPT_USE_SYSTEM_PREFERRED_RNG` なのでアルゴリズムのハンドルは要らない）を呼ぶだけ。

use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
};

/// タイマーが鳴らないときの歯止め（ms）。待ちたい長さにこれを足した所で必ず戻る。
const WAIT_GUARD_MS: u32 = 50;

/// 受信のイベント（自動リセット）。受信スレッドが立て、ゲームスレッドが待つ。
pub struct Event(HANDLE);

// SAFETY: 中身はカーネルのイベントのハンドルで、どのスレッドから `SetEvent` / 待ちをしてもよい
// （Win32 の決まり）。閉じるのは `Drop` の 1 回だけ。
unsafe impl Send for Event {}
// SAFETY: 同上（`&Event` から呼ぶのは `SetEvent` と待ちだけで、どちらもスレッドをまたいでよい）。
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> Option<Event> {
        // SAFETY: 属性 NULL・自動リセット（manual = 0）・最初は立っていない・名前 NULL を渡すだけ。
        let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        (!h.is_null()).then_some(Event(h))
    }

    pub fn set(&self) {
        // SAFETY: `self.0` は `new` で作ってまだ閉じていないイベント。
        unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: `new` で作ったハンドルを 1 回だけ閉じる。
        unsafe { CloseHandle(self.0) };
    }
}

struct Timer(HANDLE);

// SAFETY: カーネルのタイマーのハンドル。作ったスレッドと別のスレッド（ゲームスレッド）で使ってよい。
// `Waiter` ごと 1 本のスレッドへ移して使い、同時に 2 本からは使わない（`Waiter` は `Sync` でない）。
unsafe impl Send for Timer {}

impl Timer {
    fn new() -> Option<Timer> {
        // SAFETY: 属性 NULL・名前 NULL（名前なし）・旗と権利を渡すだけ。古い Windows で旗が分からなければ NULL。
        let h = unsafe {
            CreateWaitableTimerExW(
                core::ptr::null(),
                core::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        (!h.is_null()).then_some(Timer(h))
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: `new` で作ったハンドルを 1 回だけ閉じる。
        unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woke {
    /// イベントが立った（何か届いた）。
    Signaled,
    /// 時刻が来た（か、歯止めで戻った）。
    TimedOut,
}

/// ゲームスレッドの待ち。1 本のスレッドだけが使う。
pub struct Waiter {
    event: Arc<Event>,
    timer: Option<Timer>,
}

impl Waiter {
    pub fn new(event: Arc<Event>) -> Waiter {
        Waiter {
            event,
            timer: Timer::new(),
        }
    }

    pub fn high_resolution(&self) -> bool {
        self.timer.is_some()
    }

    /// 最大 `us` µs 待つ。イベントが立てば早く戻る。確保しない。
    pub fn wait(&self, us: u32) -> Woke {
        if us == 0 {
            return Woke::TimedOut;
        }
        let guard_ms = us / 1000 + WAIT_GUARD_MS;
        if let Some(t) = &self.timer {
            // 100 ns 単位。負は「今から」の相対。
            let due: i64 = -(i64::from(us) * 10);
            // SAFETY: `t.0` は作ってまだ閉じていないタイマー。`due` はこの呼び出しの間生きている。周期 0（1 回だけ）、
            // 完了ルーチンなし、電源の復帰なし。前の待ちで残っていた時刻は、ここで置き直される。
            let ok = unsafe { SetWaitableTimer(t.0, &due, 0, None, core::ptr::null(), 0) };
            if ok != 0 {
                let hs = [self.event.0, t.0];
                // SAFETY: `hs` はこの呼び出しの間生きている 2 つの開いたハンドルの配列。どれか 1 つで戻る（all = 0）。
                let r = unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 0, guard_ms) };
                return if r == WAIT_OBJECT_0 {
                    Woke::Signaled
                } else {
                    Woke::TimedOut
                };
            }
        }
        let ms = us.div_ceil(1000);
        // SAFETY: 開いたイベントを ms で待つだけ（`INFINITE` にはしない）。
        let r = unsafe { WaitForSingleObject(self.event.0, ms.min(INFINITE - 1)) };
        if r == WAIT_OBJECT_0 {
            Woke::Signaled
        } else {
            Woke::TimedOut
        }
    }
}

/// `buf` を OS の乱数で埋める（`BCryptGenRandom`）。失敗したら false（`buf` の中身は使わない）。
/// 接続を開くとき（読み取りのスレッド）にだけ呼ぶ。ゲームスレッドでは呼ばない。
pub fn os_random(buf: &mut [u8]) -> bool {
    let Ok(len) = u32::try_from(buf.len()) else {
        return false;
    };
    // SAFETY: `buf` はこの呼び出しの間生きている、長さ `len` の書ける領域。`BCRYPT_USE_SYSTEM_PREFERRED_RNG` の
    // ときアルゴリズムのハンドルは null でよい（Win32 の決まり）。戻り値の NTSTATUS は 0 以上が成功。
    let st = unsafe {
        BCryptGenRandom(
            core::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    st >= 0
}

/// マルチキャストを出すインターフェースを `ip` の持ち主のネットワークインターフェースに決める（`IP_MULTICAST_IF`）。std にこの設定が無い。
/// この PC のようにネットワークインターフェースが複数あると、指定しない送り出しは既定の経路のインターフェースから出てルーターへ届かず、SSDP の返事が来ない。
/// UPnP の問い合わせ（`upnp.rs`）だけが呼ぶ。
pub fn set_multicast_if(sock: &std::net::UdpSocket, ip: std::net::Ipv4Addr) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        IP_MULTICAST_IF, IPPROTO_IP, SOCKET, SOCKET_ERROR, setsockopt,
    };
    // in_addr はネットワークの順の 4 バイト（`octets` の並びと同じ）。
    let addr = ip.octets();
    // SAFETY: `sock` はこの呼び出しの間生きている UDP ソケットで、`as_raw_socket` はその有効なハンドル。`addr` は
    // 長さ 4 の読める領域で、`optlen` に同じ 4 を渡す。
    let r = unsafe {
        setsockopt(
            sock.as_raw_socket() as SOCKET,
            IPPROTO_IP,
            IP_MULTICAST_IF,
            addr.as_ptr(),
            addr.len() as i32,
        )
    };
    if r == SOCKET_ERROR {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn waiter() -> (Arc<Event>, Waiter) {
        let e = Arc::new(Event::new().expect("イベントを作れない"));
        (Arc::clone(&e), Waiter::new(e))
    }

    #[test]
    fn a_set_event_wakes_once_then_it_times_out() {
        let (e, w) = waiter();
        e.set();
        assert_eq!(w.wait(200_000), Woke::Signaled);
        assert_eq!(w.wait(2_000), Woke::TimedOut);
        assert_eq!(w.wait(0), Woke::TimedOut);
    }

    #[test]
    fn another_thread_wakes_the_waiter_early() {
        let (e, w) = waiter();
        let t0 = Instant::now();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            e.set();
        });
        let r = w.wait(2_000_000);
        let took = t0.elapsed();
        h.join().unwrap();
        assert_eq!(r, Woke::Signaled);
        assert!(took < Duration::from_millis(500), "{took:?}");
    }

    #[test]
    #[ignore = "OS の待機タイマーの精度を実時間で測る。全件の回（build.bat）で --ignored で走る"]
    fn high_resolution_waits_are_not_rounded_to_the_os_tick() {
        let (_e, w) = waiter();
        assert!(
            w.high_resolution(),
            "高分解能の待機タイマーを作れない（Windows 10 1803 より前？）"
        );
        // 4 ms を 20 回待ち、中央の値が 4〜8 ms に収まること（丸められれば 15.6 ms 前後）。
        let mut took: Vec<Duration> = (0..20)
            .map(|_| {
                let t0 = Instant::now();
                assert_eq!(w.wait(4_000), Woke::TimedOut);
                t0.elapsed()
            })
            .collect();
        took.sort();
        let mid = took[took.len() / 2];
        assert!(
            mid >= Duration::from_micros(3_900) && mid < Duration::from_millis(8),
            "4 ms の待ちの中央 {mid:?}（全部 {took:?}）"
        );
    }

    #[test]
    fn os_random_fills_the_buffer() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        assert!(os_random(&mut a) && os_random(&mut b));
        assert_ne!(a, [0u8; 32]);
        assert_ne!(a, b);
        // 後ろの半分も埋まっている（長さを取り違えて前だけ埋める形を落とす）。
        assert_ne!(&a[16..], &[0u8; 16][..]);
        assert!(os_random(&mut []));
    }

    #[test]
    fn waiting_never_allocates() {
        let (e, w) = waiter();
        e.set();
        let before = crate::test_alloc::count();
        let a = w.wait(1_000);
        let b = w.wait(1_000);
        let used = crate::test_alloc::count() - before;
        assert_eq!(used, 0, "待ちで確保した");
        assert_eq!((a, b), (Woke::Signaled, Woke::TimedOut));
    }
}
