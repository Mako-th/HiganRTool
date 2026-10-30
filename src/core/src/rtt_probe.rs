//! 対戦の往復をINPUTごとに測る（pure。確保しない）。
//! INPUTの時刻のフィールド（`wire::Stamp`）に自分の時計と、最後に受けた相手の時計・受けてから送るまでのmsを載せる（`EchoClock`）。
//! 相手が持った時間を引いた往復をサンプルにする。
//! サンプルは直近`RECENT_MS`の最小（少なくとも直近`RECENT_SAMPLES`個）と、サンプルのあった直近`BASE_BUCKETS`秒の最小に縮める（`DelayProbe`）。
//! 相手 → 自分の向きの延びは、受けた時刻 − 相手の送った時刻を同じ縮め方で縮めたもの。これを往復の延びから引き、自分の上りで待った分の見積もりにする（観戦のパケットはホストの上りだけを通る）。
//! この見積もりを観戦者への送信（`spectate_rate::Throttle`）が使う。
//! ゲームスレッドで呼ぶ。受信スレッドへは`DelaySnapshot::pack`の64ビット1つで渡す。

use crate::wire::{STAMP_HELD_MAX, Stamp};

/// 往復のサンプルの上限（ms）。これより長い値は時計の食い違い・古い返答として捨てる。
pub const RTT_SAMPLE_MAX_MS: u32 = 10_000;
/// 「今の往復」を見る長さ（ms）。この間に1通でも待たずに届いたサンプルがあれば延びと見なさない。
/// 1通ごとの揺れ（Wi-Fiなど）は最小を取ると消える。上りの列は全部のサンプルを押し上げる。
/// 1通ごとに0〜80 ms揺れる回線でも、揺れだけでは目安（`spectate_rate::TARGET_MS`）を越えない長さにしてある。
pub const RECENT_MS: u32 = 2_000;
/// 「今の往復」のバケットの数と1つの長さ（バケット全体は`RECENT_MS`。見る長さは1.75〜2秒）。
const RECENT_BUCKETS: usize = 8;
const RECENT_BUCKET_MS: u32 = RECENT_MS / RECENT_BUCKETS as u32;
/// 「今の往復」に少なくとも含めるサンプルの数。サンプルがPONGだけ（1秒に1つ）の間も直近の4つを見る。
pub const RECENT_SAMPLES: usize = 4;
/// 「詰まっていないときの往復」を見る秒の数（1秒ごとの最小を持つバケット）。
pub const BASE_BUCKETS: usize = 10;
pub const BASE_BUCKET_MS: u32 = 1_000;
/// 相手 → 自分の向きの要約がこれより古ければ往復の延びをそのまま使う（ms）。
/// PONGの往復には向きの内訳が無い。
/// INPUTを送っていない間（メニュー）は相手の上りもINPUTで埋まっていない（推定）。
pub const ONE_WAY_STALE_MS: u32 = 1_500;
/// 相手 → 自分の向きの値（時計のずれを含むu16の差）の基準からの離れがこれを越えたら基準を取り直す（ms）。
/// 時計のずれの変化と16ビットの折り返しを延びと読み違えないための処理。
const ONE_WAY_REANCHOR_MS: i32 = 16_000;

/// `then`から`now`までの経過（単位は呼び出し元の時計。折り返しを越えて前なら0）。観戦者への送信を絞る部品とdllの観戦者が共有する。
pub fn elapsed(now: u32, then: u32) -> u32 {
    let d = now.wrapping_sub(then);
    if d > u32::MAX / 2 { 0 } else { d }
}

/// 受けたINPUT 1通から取れた値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Echo {
    /// 相手が持った時間を引いた往復（ms）。返答が無い・食い違いならNone。
    pub rtt: Option<u32>,
    /// 受けた時刻 − 相手の送った時刻（msの下位16ビットの差。時計のずれを含むので差の動きだけに意味がある）。
    pub one_way: u16,
}

/// INPUTの時刻の返答（受けた相手の時刻を相手へ返す）。送るときに`stamp`、受けたときに`on_receive`を呼ぶ。
#[derive(Debug, Clone, Copy, Default)]
pub struct EchoClock {
    /// 最後に受けた相手の`sent`と、それを受けた時刻（受信スレッドが受けた時刻）。
    last: Option<(u16, u32)>,
}

impl EchoClock {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// 時刻`now`に送るINPUTの時刻のフィールド。持った時間が`STAMP_HELD_MAX`を越えた返答は載せない。
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

    /// 時刻`rx`（受信スレッドが受けた時刻）に受けたINPUTの時刻のフィールド。
    /// 順が入れ替わって届いた古いINPUTと、同じ`sent`の重複は返答に使わない。
    /// 前に受けてから`STAMP_HELD_MAX`を越えて空いたら（試合の間など）前の`sent`はもう返せない。
    /// そのため順を比べずに置き換える。
    /// 16ビットの時計は32.8秒を越えて空くと前後を取り違える。
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
                // msに切り捨てた時刻どうしなので往復が0に近いと数ms負になる。それより大きく負なのは食い違い。
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

/// 往復のサンプルの要約。`at`は最後の往復のサンプルの時刻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelaySnapshot {
    pub at: u32,
    /// 自分の上りで待った分の見積もり（ms。`QUEUE_MAX_MS`まで）。
    /// 往復の延びから相手 → 自分の向きの延びを引いた値（0で下を切る）。
    /// 延びは「今の往復」（`RECENT_MS`）の最小と、サンプルのあった直近`BASE_BUCKETS`秒の最小の差。
    /// サンプルが`RECENT_MS`続くまでは0。
    pub queue_ms: u16,
    /// 同じ見積もりを直近`RECENT_SAMPLES`個の最小で出したもの（ms。`QUEUE_MAX_MS`まで）。揺れでも動く。列のでき始めにはすぐ出る。
    /// 空きの後に`RECENT_SAMPLES`個そろうまでは0（1つだけのサンプルの揺れで動かない）。
    pub fresh_ms: u16,
    /// サンプルのあった直近`BASE_BUCKETS`秒の往復の最小（ms。`BASE_MAX_MS`まで）。
    pub base_ms: u16,
}

/// `DelaySnapshot::pack`のフィールドの幅（ビット）: `at` 32・延び2つ・往復。
const QUEUE_BITS: u32 = 10;
const BASE_BITS: u32 = 12;
const _: () = assert!(32 + 2 * QUEUE_BITS + BASE_BITS == 64);
/// `DelaySnapshot`の延びと往復の上限（ms）。フィールドの全ビットが1にならない値（`NO_SNAPSHOT`と重ならない）。
pub const QUEUE_MAX_MS: u16 = (1 << QUEUE_BITS) - 2;
pub const BASE_MAX_MS: u16 = (1 << BASE_BITS) - 2;
/// `DelaySnapshot::pack`の「サンプルなし」。延びと往復は上限で止めるので本物の値と重ならない。
pub const NO_SNAPSHOT: u64 = u64::MAX;

impl DelaySnapshot {
    pub fn queuing_ms(&self) -> u32 {
        u32::from(self.queue_ms)
    }

    pub fn pack(snap: Option<DelaySnapshot>) -> u64 {
        match snap {
            Some(s) => {
                (u64::from(s.at) << 32)
                    | (u64::from(s.queue_ms.min(QUEUE_MAX_MS)) << (QUEUE_BITS + BASE_BITS))
                    | (u64::from(s.fresh_ms.min(QUEUE_MAX_MS)) << BASE_BITS)
                    | u64::from(s.base_ms.min(BASE_MAX_MS))
            }
            None => NO_SNAPSHOT,
        }
    }

    pub fn unpack(v: u64) -> Option<DelaySnapshot> {
        let field = |shift: u32, bits: u32| ((v >> shift) & ((1 << bits) - 1)) as u16;
        (v != NO_SNAPSHOT).then_some(DelaySnapshot {
            at: (v >> 32) as u32,
            queue_ms: field(QUEUE_BITS + BASE_BITS, QUEUE_BITS),
            fresh_ms: field(BASE_BITS, QUEUE_BITS),
            base_ms: field(0, BASE_BITS),
        })
    }
}

const EMPTY: i32 = i32::MAX;

/// 直近`RECENT_MS`（少なくとも直近の数個）の最小と、サンプルのあった直近の秒の最小を持つ。
/// 秒ごとのバケットはサンプルが来たときだけ進める。
/// そのためサンプルの無い間（メニュー・ポーズ）の後も、前の秒の最小を捨てない。
/// 「今の往復」は時刻で進め、サンプルが`RECENT_MS`続いてから使う。
/// 接続の直後と長い空きの後の数通だけの最小では、揺れを延びと読んでしまうため。
#[derive(Debug, Clone, Copy)]
struct MinFilter {
    recent: [i32; RECENT_SAMPLES],
    recent_n: usize,
    recent_pos: usize,
    span: [i32; RECENT_BUCKETS],
    span_pos: usize,
    span_at: u32,
    /// サンプルが`RECENT_MS`以上空かずに続いている区間の始まりの時刻。
    run_since: u32,
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
        span: [EMPTY; RECENT_BUCKETS],
        span_pos: 0,
        span_at: 0,
        run_since: 0,
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
            Some(last) if elapsed(at, last) < RECENT_MS => {
                let steps = elapsed(at, self.span_at) / RECENT_BUCKET_MS;
                if steps >= RECENT_BUCKETS as u32 {
                    self.span = [EMPTY; RECENT_BUCKETS];
                    self.span_at = at;
                } else {
                    for _ in 0..steps {
                        self.span_pos = (self.span_pos + 1) % RECENT_BUCKETS;
                        if let Some(b) = self.span.get_mut(self.span_pos) {
                            *b = EMPTY;
                        }
                    }
                    self.span_at = self.span_at.wrapping_add(steps * RECENT_BUCKET_MS);
                }
            }
            _ => {
                // 空きの前の数個も「今の往復」に残さない。
                self.recent = [EMPTY; RECENT_SAMPLES];
                if let Some(slot) = self.recent.first_mut() {
                    *slot = v;
                }
                self.recent_pos = 1 % RECENT_SAMPLES;
                self.recent_n = 1;
                self.span = [EMPTY; RECENT_BUCKETS];
                self.span_at = at;
                self.run_since = at;
            }
        }
        if let Some(b) = self.span.get_mut(self.span_pos) {
            *b = (*b).min(v);
        }
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
        let at = self.last_at?;
        if elapsed(at, self.run_since) < RECENT_MS {
            return None;
        }
        Some(self.span.iter().copied().fold(self.few_min()?, i32::min))
    }

    fn few_min(&self) -> Option<i32> {
        self.recent.iter().take(self.recent_n).copied().min()
    }

    fn base_min(&self) -> Option<i32> {
        self.base.iter().copied().min().filter(|&v| v != EMPTY)
    }

    /// 直近の最小から秒の最小までの延び（0で下を切る）。
    fn rise(&self) -> Option<i32> {
        Some((self.recent_min()? - self.base_min()?).max(0))
    }

    /// 直近の数個の最小から秒の最小までの延び（`RECENT_SAMPLES`個そろうまでは0）。
    fn fresh_rise(&self) -> Option<i32> {
        if self.recent_n < RECENT_SAMPLES {
            return self.base_min().map(|_| 0);
        }
        Some((self.few_min()? - self.base_min()?).max(0))
    }
}

/// 往復と、相手 → 自分の向きのサンプルを縮める。
#[derive(Debug, Clone, Copy)]
pub struct DelayProbe {
    rtt: MinFilter,
    /// 相手 → 自分の向き（基準`down_ref`からの離れ）。
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

    /// 時刻`at`に受けたINPUTの値（`EchoClock::on_receive`）。
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

    /// 時刻`at`に測った往復（PONG。向きの内訳は無い）。
    pub fn sample_rtt(&mut self, at: u32, rtt_ms: u32) {
        self.rtt
            .push(at, rtt_ms.min(u32::from(u16::MAX) - 1) as i32);
        self.pong_samples = self.pong_samples.saturating_add(1);
    }

    /// 往復のサンプルの数（INPUTの返答とPONG）。
    pub fn samples(&self) -> (u64, u64) {
        (self.input_samples, self.pong_samples)
    }

    pub fn snapshot(&self) -> Option<DelaySnapshot> {
        let at = self.rtt.last_at?;
        let rise = self.rtt.rise().unwrap_or(0);
        let base = self.rtt.base_min()?;
        let fresh = self.rtt.fresh_rise()?;
        let (down, down_fresh) = match self.down.last_at {
            // 相手 → 自分の向きだけが空きの後で窓がそろっていない間（メニューの後のINPUTの頭）は直近の数個の延びを引く。
            Some(t) if elapsed(at, t) <= ONE_WAY_STALE_MS => (
                self.down
                    .rise()
                    .or_else(|| self.down.fresh_rise())
                    .unwrap_or(0),
                self.down.fresh_rise().unwrap_or(0),
            ),
            _ => (0, 0),
        };
        let q = i32::from(QUEUE_MAX_MS);
        Some(DelaySnapshot {
            at,
            queue_ms: (rise - down).clamp(0, q) as u16,
            fresh_ms: (fresh - down_fresh).clamp(0, q) as u16,
            base_ms: base.clamp(0, i32::from(BASE_MAX_MS)) as u16,
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
        // Aが1000に送り、Bが1030に受けて12 ms持ってから返し、Aが1080に受けた: 往復 80 − 12 = 68。
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
        // 16ビットの時計の折り返しの後の方が新しい。
        b.on_receive(&st(0x0005, None), 20);
        b.on_receive(&st(0xFFF8, None), 25);
        assert_eq!(b.stamp(30).echo, Some((0x0005, 10)));
        // 同じ`sent`の重複は、受けた時刻を後ろへずらさない（持った時間を短く見せない）。
        b.on_receive(&st(0x0005, None), 28);
        assert_eq!(b.stamp(30).echo, Some((0x0005, 10)));
        // 返答の往復も折り返しをまたいで数える: 0xFFF0に送り0x0010（= 0x1_0010）に受けた、持った2 ms。
        let mut a = EchoClock::new();
        assert_eq!(
            a.on_receive(&st(1, Some((0xFFF0, 2))), 0x1_0010).rtt,
            Some(0x20 - 2)
        );
    }

    #[test]
    fn after_a_long_gap_between_matches_the_next_input_is_echoed_again() {
        // 16ビットの差の上位が0x8000以上になる空き（40秒・50秒・60秒）でも空いた後のINPUTを返答に使う。
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
        // 空きが短ければ古いINPUTは今までどおり返答に使わない。
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
    fn recent_is_the_min_of_the_last_two_seconds_and_base_the_min_of_ten_sampled_seconds() {
        let mut p = DelayProbe::new();
        assert_eq!(p.snapshot(), None);
        let mut t = 0;
        let mut inputs = 0;
        // サンプルが`RECENT_MS`続くまでは窓の延びは0（直近の数個の延びは出る）。
        while t < RECENT_MS {
            p.sample_input(t, input(if t == 1_700 { 90 } else { 30 }, 500));
            inputs += 1;
            let s = p.snapshot().unwrap();
            assert_eq!(s.queue_ms, 0, "{t} ms");
            if (1_700..1_700 + 3 * 17).contains(&t) {
                assert_eq!(s.fresh_ms, 0, "{t} ms: 4 個の最小");
            }
            t += 17;
        }
        p.sample_input(t, input(30, 500));
        inputs += 1;
        let s = p.snapshot().unwrap();
        assert_eq!((s.queue_ms, s.base_ms), (0, 30));
        // 延びたサンプルが続いても窓の中に待たずに届いたサンプルが残る間は延びと見なさない。窓を過ぎたら延びと見なす。
        let start = t;
        for v in [80, 50, 90, 70].into_iter().cycle() {
            t += 17;
            p.sample_input(t, input(v, 500));
            inputs += 1;
            let q = p.snapshot().unwrap().queue_ms;
            if t - start < RECENT_MS - RECENT_BUCKET_MS {
                assert_eq!(q, 0, "{} ms 後", t - start);
            }
            if t - start > RECENT_MS {
                assert_eq!(q, 20, "{} ms 後", t - start);
                break;
            }
        }
        // 直近の数個の延びは窓を待たずに出る。
        assert_eq!(p.snapshot().unwrap().fresh_ms, 20);
        // 1通でも待たずに届けばすぐ延びではなくなる。
        p.sample_input(t, input(31, 500));
        inputs += 1;
        assert_eq!(p.snapshot().unwrap().queue_ms, 1);
        // `RECENT_MS`を越える空きの後はまた`RECENT_MS`続くまで窓の延びは0。`RECENT_SAMPLES`個そろうまで直近の延びも0。
        // 秒ごとのバケットはサンプルで進むので前の秒の最小は捨てない（PONGだけのサンプルでも続く）。
        t += 60_000;
        p.sample_rtt(t, 75);
        let s = p.snapshot().unwrap();
        assert_eq!((s.queue_ms, s.fresh_ms), (0, 0));
        t += 1_000;
        p.sample_rtt(t, 75);
        assert_eq!(p.snapshot().unwrap().queue_ms, 0);
        t += 1_000;
        p.sample_rtt(t, 75);
        let s = p.snapshot().unwrap();
        assert_eq!((s.at, s.base_ms, s.queue_ms, s.fresh_ms), (t, 30, 45, 0));
        t += BASE_BUCKET_MS;
        p.sample_rtt(t, 75);
        assert_eq!(p.snapshot().unwrap().fresh_ms, 45, "4 個そろったら出す");
        // サンプルのある10秒が過ぎたら前の最小はバケットから外れる。
        for _ in 1..BASE_BUCKETS {
            t += BASE_BUCKET_MS;
            p.sample_rtt(t, 75);
        }
        assert_eq!(p.snapshot().unwrap().base_ms, 75);
        assert_eq!(p.samples(), (inputs, 3 + BASE_BUCKETS as u64));
    }

    /// ホスト（A）と相手（B）が60 HzでINPUTを送り合う。時計は互いに16ビットの折り返しの近くでずれている。
    /// `up(t)`・`down(t)`は時刻tに送ったパケットの片道（ms）。`seen`は1 msごとにAの測定結果を受ける。
    fn exchange_with(
        up: impl Fn(u32) -> u32,
        down: impl Fn(u32) -> u32,
        ms: u32,
        mut seen: impl FnMut(u32, &DelayProbe),
    ) -> DelayProbe {
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
            seen(t, &p);
        }
        p
    }

    /// `exchange_with`の終わりのAの要約。
    fn exchange(up: impl Fn(u32) -> u32, down: impl Fn(u32) -> u32, ms: u32) -> DelaySnapshot {
        exchange_with(up, down, ms, |_, _| {})
            .snapshot()
            .expect("要約")
    }

    /// 1通ごとに0〜`hi` msの揺れ。並びは決まっていて走らせるたびに同じ。
    fn jitter(t: u32, salt: u32, hi: u32) -> u32 {
        (t ^ salt).wrapping_mul(0x9E37_79B9).rotate_right(16) % (hi + 1)
    }

    #[test]
    fn only_a_delay_on_the_own_uplink_counts_as_queuing() {
        // 12秒目から50 ms延びる。相手 → 自分の向きだけが延びたら見積もりは0前後、自分 → 相手の向きなら50前後。
        let flat = |_t: u32| 15;
        let rising = |t: u32| if t >= 12_000 { 65 } else { 15 };
        let down_only = exchange(flat, rising, 12_000 + RECENT_MS + 500);
        assert!(
            down_only.queue_ms <= 2,
            "相手の上りの延びを数えた: {down_only:?}"
        );
        let up_only = exchange(rising, flat, 12_000 + RECENT_MS + 500);
        assert!(
            (48..=52).contains(&up_only.queue_ms),
            "自分の上りの延び: {up_only:?}"
        );
        assert!((29..=31).contains(&up_only.base_ms), "{up_only:?}");
    }

    #[test]
    fn a_per_packet_jitter_is_not_read_as_queuing_but_a_standing_queue_is_within_the_window() {
        use crate::spectate_rate::{SURGE_MS, TARGET_MS};
        // 両向きとも片道100 msに1通ごと0〜40 ms・0〜80 msの揺れ。
        for hi in [40, 80] {
            let up = move |t: u32| 100 + jitter(t, 1, hi);
            let down = move |t: u32| 100 + jitter(t, 2, hi);
            let (mut worst, mut worst_fresh) = (0u16, 0u16);
            exchange_with(up, down, 60_000, |t, p| {
                if t >= 12_000
                    && let Some(s) = p.snapshot()
                {
                    worst = worst.max(s.queue_ms);
                    worst_fresh = worst_fresh.max(s.fresh_ms);
                }
            });
            assert!(
                u32::from(worst) <= TARGET_MS,
                "0〜{hi} ms: 揺れを延びと読んだ: {worst} ms"
            );
            assert!(
                u32::from(worst_fresh) <= SURGE_MS,
                "0〜{hi} ms: 揺れの直近の延びが窓を待たない目安を越えた: {worst_fresh} ms"
            );
            assert!(worst_fresh > 0, "0〜{hi} ms: 直近の延びを測れていない");
        }
        let up = |t: u32| 100 + jitter(t, 1, 40);
        let down = |t: u32| 100 + jitter(t, 2, 40);
        // 30秒目から自分の上りに50 msの列ができる。窓の長さのうちに目安を越える。
        let queued = |t: u32| up(t) + if t >= 30_000 { 50 } else { 0 };
        let mut first = None;
        exchange_with(queued, down, 30_000 + RECENT_MS + 1_000, |t, p| {
            if first.is_none()
                && p.snapshot()
                    .is_some_and(|s| u32::from(s.queue_ms) > TARGET_MS)
            {
                first = Some(t);
            }
        });
        let at = first.expect("上りの列を延びと読まない");
        assert!(
            (30_000..=30_000 + RECENT_MS + 300).contains(&at),
            "目安を越えた時刻: {at} ms"
        );
    }

    #[test]
    fn without_fresh_inputs_the_round_trip_rise_is_used_as_is() {
        let mut p = DelayProbe::new();
        let mut t = 0;
        while t <= RECENT_MS {
            p.sample_input(t, input(30, 100));
            t += 17;
        }
        // 相手 → 自分の向きも同じだけ延びたINPUTが窓いっぱい続いた後にINPUTが止まってPONGだけになる。
        let start = t;
        while t <= start + RECENT_MS + 17 {
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
    fn after_a_gap_only_in_inputs_the_peers_uplink_delay_is_still_subtracted() {
        let mut p = DelayProbe::new();
        let mut t = 0;
        while t <= 3_000 {
            p.sample_input(t, input(30, 100));
            t += 17;
        }
        // INPUTが止まり（メニュー）、PONGだけが1秒ごとに続く。その間に相手の上りに列ができて往復が50 ms延びる。
        for _ in 0..3 {
            t += 1_000;
            p.sample_rtt(t, 80);
        }
        // INPUTが戻る。相手 → 自分の向きも同じだけ延びているので自分の上りの延びは0前後になる（相手の向きの窓がそろう前でも差し引く）。
        for _ in 0..RECENT_SAMPLES {
            t += 17;
            p.sample_input(t, input(80, 150));
        }
        let s = p.snapshot().unwrap();
        assert!(
            s.queue_ms <= 2 && s.fresh_ms <= 2,
            "相手の上りの延びを自分のものと読んだ: {s:?}"
        );
    }

    #[test]
    fn a_snapshot_survives_the_64_bit_handover() {
        let s = DelaySnapshot {
            at: 0xDEAD_BEEF,
            queue_ms: 0x2A5,
            fresh_ms: 0x15A,
            base_ms: 0xF0E,
        };
        assert_eq!(DelaySnapshot::unpack(DelaySnapshot::pack(Some(s))), Some(s));
        // 上限を越えた値は上限で止まり、隣のフィールドへ漏れない。
        let over = DelaySnapshot {
            queue_ms: u16::MAX,
            fresh_ms: u16::MAX,
            base_ms: u16::MAX,
            ..s
        };
        assert_eq!(
            DelaySnapshot::unpack(DelaySnapshot::pack(Some(over))),
            Some(DelaySnapshot {
                queue_ms: QUEUE_MAX_MS,
                fresh_ms: QUEUE_MAX_MS,
                base_ms: BASE_MAX_MS,
                ..s
            })
        );
        assert_eq!(DelaySnapshot::unpack(DelaySnapshot::pack(None)), None);
        let mut p = DelayProbe::new();
        for back in [2_000, 1_000, 0] {
            p.sample_rtt(u32::MAX - back, u32::MAX);
        }
        let big = p.snapshot().unwrap();
        assert_ne!(
            DelaySnapshot::pack(Some(big)),
            NO_SNAPSHOT,
            "最大の値でも「標本なし」にならない"
        );
    }
}
