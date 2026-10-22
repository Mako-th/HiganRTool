//! 対戦の往復を INPUT ごとに測る（pure。確保しない）。INPUT の時刻の欄（`wire::Stamp`）に自分の時計と、最後に受けた相手の時計・受けてから
//! 送るまでの ms を載せ（`EchoClock`）、相手が持った時間を引いた往復を標本にする。標本は直近 `RECENT_SAMPLES` 個の最小と、標本のあった
//! 直近 `BASE_BUCKETS` 秒の最小に縮める（`DelayProbe`）。相手 → 自分の向きの延び（受けた時刻 − 相手の送った時刻の、同じ縮め方）を往復の延びから
//! 引き、自分の上りで待った分の見積もりにする（観戦の便はホストの上りだけを通る）。それを観戦者への送信（`spectate_rate::Throttle`）が使う。
//! ゲームスレッドで呼ぶ。受信スレッドへは `DelaySnapshot::pack` の 64 ビット 1 つで渡す。

use crate::wire::{STAMP_HELD_MAX, Stamp};

/// 往復の標本の上限（ms）。これより長い値は時計の食い違い・古い返しとして捨てる。
pub const RTT_SAMPLE_MAX_MS: u32 = 10_000;
/// 「今の往復」に使う標本の数。60 Hz の INPUT で約 67 ms。1 通だけの揺れ（相手のスレッドの遅れなど）で動かない最小の数。
pub const RECENT_SAMPLES: usize = 4;
/// 「詰まっていないときの往復」を見る秒の数（1 秒ごとの最小の輪）。
pub const BASE_BUCKETS: usize = 10;
pub const BASE_BUCKET_MS: u32 = 1_000;
/// 相手 → 自分の向きの要約がこれより古ければ、往復の延びをそのまま使う（ms）。PONG の往復には向きの内訳が無く、INPUT を流していない間
/// （メニュー）は相手の上りも INPUT で埋まっていない（推定）。
pub const ONE_WAY_STALE_MS: u32 = 1_500;
/// 相手 → 自分の向きの値（時計のずれを含む u16 の差）の基準からの離れがこれを越えたら、基準を取り直す（ms）。時計のずれの変化と
/// 16 ビットの折り返しを、延びと読み違えない。
const ONE_WAY_REANCHOR_MS: i32 = 16_000;

/// `now` が `then` から何後か（単位は呼び手の時計。折り返しを越えて前なら 0）。観戦者への送信を絞る部品と dll の観戦者が共有する。
pub fn elapsed(now: u32, then: u32) -> u32 {
    let d = now.wrapping_sub(then);
    if d > u32::MAX / 2 { 0 } else { d }
}

/// 受けた INPUT 1 通から取れた値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Echo {
    /// 相手が持った時間を引いた往復（ms）。返しが無い・食い違いなら None。
    pub rtt: Option<u32>,
    /// 受けた時刻 − 相手の送った時刻（ms の下位 16 ビットの差。時計のずれを含むので、差の動きだけに意味がある）。
    pub one_way: u16,
}

/// INPUT の時刻の返し（受けた相手の時刻を相手へ返す）: 送るときに `stamp`、受けたときに `on_receive`。
#[derive(Debug, Clone, Copy, Default)]
pub struct EchoClock {
    /// 最後に受けた相手の `sent` と、それを受けた時刻（受信スレッドが受けた時刻）。
    last: Option<(u16, u32)>,
}

impl EchoClock {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// 時刻 `now` に送る INPUT の時刻の欄。持った時間が `STAMP_HELD_MAX` を越えた返しは載せない。
    pub fn stamp(&self, now: u32) -> Stamp {
        let echo = self.last.and_then(|(sent, rx)| {
            let held = elapsed(now, rx);
            (held <= u32::from(STAMP_HELD_MAX)).then_some((sent, held as u8))
        });
        Stamp {
            sent: now as u16,
            echo,
        }
    }

    /// 時刻 `rx`（受信スレッドが受けた時刻）に受けた INPUT の時刻の欄。順が入れ替わって届いた古い INPUT と、同じ `sent` の重複は、返しに
    /// 使わない。前に受けてから `STAMP_HELD_MAX` を越えて空いたら（試合の間など）、前の `sent` はもう返せないので順を比べずに置き換える
    /// （16 ビットの時計は 32.8 秒を越えて空くと前後を取り違える）。
    pub fn on_receive(&mut self, st: &Stamp, rx: u32) -> Echo {
        let newer = self.last.is_none_or(|(s, at)| {
            let d = st.sent.wrapping_sub(s);
            elapsed(rx, at) > u32::from(STAMP_HELD_MAX) || (d != 0 && d < 0x8000)
        });
        if newer {
            self.last = Some((st.sent, rx));
        }
        Echo {
            rtt: st.echo.and_then(|(echo, held)| {
                let total = (rx as u16).wrapping_sub(echo);
                // ms に切り捨てた時刻どうしなので、往復が 0 に近いと数 ms 負になる。それより大きく負なのは食い違い。
                match i32::from(total) - i32::from(held) {
                    r if r < -4 => None,
                    r => {
                        let r = r.max(0) as u32;
                        (r <= RTT_SAMPLE_MAX_MS).then_some(r)
                    }
                }
            }),
            one_way: (rx as u16).wrapping_sub(st.sent),
        }
    }
}

/// 往復の標本の要約。`at` は最後の往復の標本の時刻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelaySnapshot {
    pub at: u32,
    /// 自分の上りで待った分の見積もり（ms）: 往復の延び − 相手 → 自分の向きの延び（0 で下を切る）。延びは直近 `RECENT_SAMPLES` 個の最小と、
    /// 標本のあった直近 `BASE_BUCKETS` 秒の最小の差。
    pub queue_ms: u16,
    /// 標本のあった直近 `BASE_BUCKETS` 秒の往復の最小（ms）。
    pub base_ms: u16,
}

/// `DelaySnapshot::pack` の「標本なし」。`queue_ms` と `base_ms` は `u16::MAX - 1` までなので、本物の値と重ならない。
pub const NO_SNAPSHOT: u64 = u64::MAX;

impl DelaySnapshot {
    pub fn queuing_ms(&self) -> u32 {
        u32::from(self.queue_ms)
    }

    pub fn pack(snap: Option<DelaySnapshot>) -> u64 {
        match snap {
            Some(s) => {
                (u64::from(s.at) << 32) | (u64::from(s.queue_ms) << 16) | u64::from(s.base_ms)
            }
            None => NO_SNAPSHOT,
        }
    }

    pub fn unpack(v: u64) -> Option<DelaySnapshot> {
        (v != NO_SNAPSHOT).then_some(DelaySnapshot {
            at: (v >> 32) as u32,
            queue_ms: (v >> 16) as u16,
            base_ms: v as u16,
        })
    }
}

const EMPTY: i32 = i32::MAX;

/// 直近の数個の最小と、標本のあった直近の秒の最小。秒の輪は標本が来たときだけ進めるので、標本の無い間（メニュー・ポーズ）の後も、前の秒の
/// 最小を捨てない。
#[derive(Debug, Clone, Copy)]
struct MinFilter {
    recent: [i32; RECENT_SAMPLES],
    recent_n: usize,
    recent_pos: usize,
    base: [i32; BASE_BUCKETS],
    base_pos: usize,
    bucket_at: u32,
    last_at: Option<u32>,
}

impl MinFilter {
    const NEW: MinFilter = MinFilter {
        recent: [EMPTY; RECENT_SAMPLES],
        recent_n: 0,
        recent_pos: 0,
        base: [EMPTY; BASE_BUCKETS],
        base_pos: 0,
        bucket_at: 0,
        last_at: None,
    };

    fn push(&mut self, at: u32, v: i32) {
        if let Some(slot) = self.recent.get_mut(self.recent_pos) {
            *slot = v;
        }
        self.recent_pos = (self.recent_pos + 1) % RECENT_SAMPLES;
        self.recent_n = (self.recent_n + 1).min(RECENT_SAMPLES);
        match self.last_at {
            None => self.bucket_at = at,
            Some(_) if elapsed(at, self.bucket_at) >= BASE_BUCKET_MS => {
                self.base_pos = (self.base_pos + 1) % BASE_BUCKETS;
                if let Some(b) = self.base.get_mut(self.base_pos) {
                    *b = EMPTY;
                }
                self.bucket_at = at;
            }
            Some(_) => {}
        }
        if let Some(b) = self.base.get_mut(self.base_pos) {
            *b = (*b).min(v);
        }
        self.last_at = Some(at);
    }

    fn recent_min(&self) -> Option<i32> {
        self.recent.iter().take(self.recent_n).copied().min()
    }

    fn base_min(&self) -> Option<i32> {
        self.base.iter().copied().min().filter(|&v| v != EMPTY)
    }

    /// 直近の最小から、秒の最小までの延び（0 で下を切る）。
    fn rise(&self) -> Option<i32> {
        Some((self.recent_min()? - self.base_min()?).max(0))
    }
}

/// 往復と、相手 → 自分の向きの標本を縮める。
#[derive(Debug, Clone, Copy)]
pub struct DelayProbe {
    rtt: MinFilter,
    /// 相手 → 自分の向き（基準 `down_ref` からの離れ）。
    down: MinFilter,
    down_ref: Option<u16>,
    input_samples: u64,
    pong_samples: u64,
}

impl Default for DelayProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayProbe {
    pub const fn new() -> Self {
        Self {
            rtt: MinFilter::NEW,
            down: MinFilter::NEW,
            down_ref: None,
            input_samples: 0,
            pong_samples: 0,
        }
    }

    /// 時刻 `at` に受けた INPUT の値（`EchoClock::on_receive`）。
    pub fn sample_input(&mut self, at: u32, e: Echo) {
        let r = *self.down_ref.get_or_insert(e.one_way);
        let mut d = i32::from(e.one_way.wrapping_sub(r) as i16);
        if d.abs() > ONE_WAY_REANCHOR_MS {
            self.down = MinFilter::NEW;
            self.down_ref = Some(e.one_way);
            d = 0;
        }
        self.down.push(at, d);
        if let Some(rtt) = e.rtt {
            self.rtt.push(at, rtt.min(u32::from(u16::MAX) - 1) as i32);
            self.input_samples = self.input_samples.saturating_add(1);
        }
    }

    /// 時刻 `at` に測った往復（PONG。向きの内訳は無い）。
    pub fn sample_rtt(&mut self, at: u32, rtt_ms: u32) {
        self.rtt
            .push(at, rtt_ms.min(u32::from(u16::MAX) - 1) as i32);
        self.pong_samples = self.pong_samples.saturating_add(1);
    }

    /// 往復の標本の数（INPUT の返し, PONG）。
    pub fn samples(&self) -> (u64, u64) {
        (self.input_samples, self.pong_samples)
    }

    pub fn snapshot(&self) -> Option<DelaySnapshot> {
        let at = self.rtt.last_at?;
        let rise = self.rtt.rise()?;
        let base = self.rtt.base_min()?;
        let down = match self.down.last_at {
            Some(t) if elapsed(at, t) <= ONE_WAY_STALE_MS => self.down.rise().unwrap_or(0),
            _ => 0,
        };
        let cap = i32::from(u16::MAX) - 1;
        Some(DelaySnapshot {
            at,
            queue_ms: (rise - down).clamp(0, cap) as u16,
            base_ms: base.clamp(0, cap) as u16,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(sent: u16, echo: Option<(u16, u8)>) -> Stamp {
        Stamp { sent, echo }
    }

    #[test]
    fn the_peers_hold_time_is_subtracted_from_the_round_trip() {
        // A が 1000 に送り、B が 1030 に受けて 12 ms 持ってから返し、A が 1080 に受けた: 往復 80 − 12 = 68。
        let mut a = EchoClock::new();
        let mut b = EchoClock::new();
        let to_b = a.stamp(1_000);
        assert_eq!(to_b.echo, None, "まだ何も受けていない");
        assert_eq!(b.on_receive(&to_b, 50_030).rtt, None);
        let to_a = b.stamp(50_042);
        assert_eq!(to_a.echo, Some((1_000u32 as u16, 12)));
        assert_eq!(a.on_receive(&to_a, 1_080).rtt, Some(68));
        // 持った時間の上限を越えたら返しを載せない。
        let held_max = u32::from(STAMP_HELD_MAX);
        assert_eq!(
            b.stamp(50_030 + held_max).echo.map(|e| e.1),
            Some(STAMP_HELD_MAX)
        );
        assert_eq!(b.stamp(50_030 + held_max + 1).echo, None);
    }

    #[test]
    fn an_older_reordered_input_or_a_duplicate_does_not_replace_the_echo_and_the_clock_wraps() {
        let mut b = EchoClock::new();
        b.on_receive(&st(0xFFF0, None), 10);
        // 16 ビットの時計の折り返しの後の方が新しい。
        b.on_receive(&st(0x0005, None), 20);
        b.on_receive(&st(0xFFF8, None), 25);
        assert_eq!(b.stamp(30).echo, Some((0x0005, 10)));
        // 同じ `sent` の重複は、受けた時刻を後ろへずらさない（持った時間を短く見せない）。
        b.on_receive(&st(0x0005, None), 28);
        assert_eq!(b.stamp(30).echo, Some((0x0005, 10)));
        // 返しの往復も折り返しをまたいで数える: 0xFFF0 に送り 0x0010（= 0x1_0010）に受けた、持った 2 ms。
        let mut a = EchoClock::new();
        assert_eq!(
            a.on_receive(&st(1, Some((0xFFF0, 2))), 0x1_0010).rtt,
            Some(0x20 - 2)
        );
    }

    #[test]
    fn after_a_long_gap_between_matches_the_next_input_is_echoed_again() {
        // 16 ビットの差の上位が 0x8000 以上になる空き（40 秒・50 秒・60 秒）でも、空いた後の INPUT を返しに使う。
        for gap in [40_000u32, 50_000, 60_000, 33_000] {
            let mut b = EchoClock::new();
            let t0 = 7_000u32;
            b.on_receive(&st(t0 as u16, None), 100);
            let sent = (t0 + gap) as u16;
            b.on_receive(&st(sent, None), 100 + gap);
            assert_eq!(
                b.stamp(100 + gap + 3).echo,
                Some((sent, 3)),
                "{gap} ms 空けた後"
            );
        }
        // 空きが短ければ、古い INPUT は今までどおり返しに使わない。
        let mut b = EchoClock::new();
        b.on_receive(&st(5_000, None), 100);
        b.on_receive(&st(4_990, None), 100 + u32::from(STAMP_HELD_MAX));
        assert_eq!(
            b.stamp(100 + u32::from(STAMP_HELD_MAX)).echo.map(|e| e.0),
            Some(5_000)
        );
    }

    #[test]
    fn a_sample_just_below_zero_is_zero_and_a_clock_mismatch_is_dropped() {
        let mut a = EchoClock::new();
        let s = |held| st(0, Some((100, held)));
        assert_eq!(a.on_receive(&s(3), 101).rtt, Some(0), "切り捨ての 2 ms");
        assert_eq!(
            a.on_receive(&s(30), 101).rtt,
            None,
            "持った時間が往復より長い"
        );
        assert_eq!(a.on_receive(&s(0), 100 + RTT_SAMPLE_MAX_MS + 1).rtt, None);
    }

    fn input(rtt: u32, one_way: u16) -> Echo {
        Echo {
            rtt: Some(rtt),
            one_way,
        }
    }

    #[test]
    fn recent_is_the_min_of_the_last_four_and_base_the_min_of_ten_sampled_seconds() {
        let mut p = DelayProbe::new();
        assert_eq!(p.snapshot(), None);
        let mut t = 0;
        for _ in 0..60 {
            p.sample_input(t, input(30, 500));
            t += 17;
        }
        for v in [80, 50, 90, 70] {
            p.sample_input(t, input(v, 500));
            t += 17;
        }
        let s = p.snapshot().unwrap();
        assert_eq!((s.queue_ms, s.base_ms), (20, 30));
        // 1 通だけ戻っても、4 通の最小は直近の値。
        p.sample_input(t, input(31, 500));
        assert_eq!(p.snapshot().unwrap().queue_ms, 1);
        // 標本の無い 1 分の後も、前の秒の最小を捨てない（秒の輪は標本で進む）。PONG だけの標本でも続く。
        t += 60_000;
        p.sample_rtt(t, 75);
        let s = p.snapshot().unwrap();
        assert_eq!((s.at, s.base_ms), (t, 30));
        // 標本のある 10 秒が過ぎたら、前の最小は輪から出る。
        for _ in 0..BASE_BUCKETS {
            t += BASE_BUCKET_MS;
            p.sample_rtt(t, 75);
        }
        assert_eq!(p.snapshot().unwrap().base_ms, 75);
        assert_eq!(p.samples(), (65, 11));
    }

    /// ホスト（A）と相手（B）が 60 Hz で INPUT を送り合う。時計は互いに 16 ビットの折り返しの近くでずれている。`up(t)`・`down(t)` は
    /// 時刻 t に送った便の片道（ms）。A の要約を返す。
    fn exchange(up: impl Fn(u32) -> u32, down: impl Fn(u32) -> u32, ms: u32) -> DelaySnapshot {
        let (mut a, mut b) = (EchoClock::new(), EchoClock::new());
        let mut p = DelayProbe::new();
        let b_off = 0xFFF0u32; // B の時計 = A の時計 + これ
        let mut to_b: Vec<(u32, Stamp)> = Vec::new();
        let mut to_a: Vec<(u32, Stamp)> = Vec::new();
        for t in 0..ms {
            to_b.retain(|&(at, s)| {
                if at == t {
                    b.on_receive(&s, t.wrapping_add(b_off));
                }
                at != t
            });
            to_a.retain(|&(at, s)| {
                if at == t {
                    let e = a.on_receive(&s, t);
                    p.sample_input(t, e);
                }
                at != t
            });
            if t % 17 == 0 {
                to_b.push((t + up(t), a.stamp(t)));
            }
            if t % 17 == 9 {
                to_a.push((t + down(t), b.stamp(t.wrapping_add(b_off))));
            }
        }
        p.snapshot().expect("要約")
    }

    #[test]
    fn only_a_delay_on_the_own_uplink_counts_as_queuing() {
        // 12 秒目から 50 ms 延びる。相手 → 自分の向きだけが延びたら見積もりは 0 前後、自分 → 相手の向きなら 50 前後。
        let flat = |_t: u32| 15;
        let rising = |t: u32| if t >= 12_000 { 65 } else { 15 };
        let down_only = exchange(flat, rising, 13_000);
        assert!(
            down_only.queue_ms <= 2,
            "相手の上りの延びを数えた: {down_only:?}"
        );
        let up_only = exchange(rising, flat, 13_000);
        assert!(
            (48..=52).contains(&up_only.queue_ms),
            "自分の上りの延び: {up_only:?}"
        );
        assert!((29..=31).contains(&up_only.base_ms), "{up_only:?}");
    }

    #[test]
    fn without_fresh_inputs_the_round_trip_rise_is_used_as_is() {
        let mut p = DelayProbe::new();
        let mut t = 0;
        for _ in 0..60 {
            p.sample_input(t, input(30, 100));
            t += 17;
        }
        // 相手 → 自分の向きも同じだけ延びた INPUT の後、INPUT が止まって PONG だけになる。
        for _ in 0..4 {
            p.sample_input(t, input(80, 150));
            t += 17;
        }
        assert_eq!(p.snapshot().unwrap().queue_ms, 0, "相手の向きの延びを引く");
        t += ONE_WAY_STALE_MS + 1;
        for _ in 0..4 {
            p.sample_rtt(t, 80);
            t += 1_000;
        }
        assert_eq!(
            p.snapshot().unwrap().queue_ms,
            50,
            "古い向きの要約は引かない"
        );
    }

    #[test]
    fn a_snapshot_survives_the_64_bit_handover() {
        let s = DelaySnapshot {
            at: 0xDEAD_BEEF,
            queue_ms: 0x1234,
            base_ms: 0x0FFE,
        };
        assert_eq!(DelaySnapshot::unpack(DelaySnapshot::pack(Some(s))), Some(s));
        assert_eq!(DelaySnapshot::unpack(DelaySnapshot::pack(None)), None);
        let mut p = DelayProbe::new();
        p.sample_rtt(u32::MAX, u32::MAX);
        let big = p.snapshot().unwrap();
        assert_ne!(
            DelaySnapshot::pack(Some(big)),
            NO_SNAPSHOT,
            "最大の値でも「標本なし」にならない"
        );
    }
}
