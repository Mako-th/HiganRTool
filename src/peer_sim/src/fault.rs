//! わざと壊す: 送るものを、固定の seed で落とす・遅らせる・重ねる。割合と遅らせる幅は起動の引数で。受ける側では何もしない
//! （受けた後の扱いは本物と同じにするため）。
//!
//! 決め打ちの落とし方（`drop_every` と `drop_run`）も持つ: 送った n 通目が `drop_every` の倍数なら、そこから `drop_run` 通を続けて
//! 落とす。乱数を使わないので、何通目が落ちるかは送った順だけで決まる。
//!
//! **一定の遅れ**（`one_way_ms`）: 全部の送りを同じ ms だけ遅らせる（相手から見た片道が長くなる。RTT を模す）。乱数の遅れ
//! （`delay_pct`）はパケットごとに違う遅れを引くので、同じ語を 4 ms ごとに送り直す INPUT では一番早く着いたコピーが使われ、遅れが
//! 打ち消される（実測: `--delay 100 --delay-max 60` で dll の stall 0）。一定の遅れは送り直しのコピーにも同じだけ乗るので打ち消されない。
//! 遅らせる・重ねるの運命の遅れには、この遅れを足す（`sim.rs` の `Tx::send`）。
//!
//! **揺らぐ遅れ**（`jitter_ms`）: 送るコピー 1 通ごとに 0〜N ms の乱数の遅れを、一定の遅れに足す。送り直しのコピーも別々に引くので、
//! 一番早いコピーでも打ち消されず、送った順と届く順が入れ替わる（実回線のジッタを模す）。運命（落とす・遅らせる・重ねる）の乱数とは
//! 別の乱数で引くので、ジッタの有無で運命の列は変わらない。
//!
//! **別のバージョンを名乗る**（`claim_ver`。`--proto-ver`）: 自分のバージョン `wire::PROTO_VER` と名乗るバージョン `claim_ver` を、送るものと**受けるもの**の
//! 両方で入れ替えて（ヘッダのバージョンと HELLO の本文のバージョン）、タグを付け直す（`Relabel`）。セッション（`Link`）は今のバージョンのまま動き、ネットワークの上では
//! `claim_ver` のバージョンの HiganRTool に見える。相手が今のバージョンなら、相手は「相手のバージョンが `claim_ver`」で、こちらのセッションは（入れ替えた後の）
//! 「相手のバージョンが `claim_ver`」で手順をやめる。バージョン違いで断る経路を、古いバージョンの物を建てずに loopback で確かめるためのもので、壊し方の中で
//! ここだけは受ける側にも掛かる。

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::wire::{HEADER_LEN, MAGIC, PROTO_VER, TAG_LEN};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JitterShape {
    #[default]
    Uniform,
    /// 大半（90%）は 0〜N/10、ときどき（10%）は N/2〜N の大きい遅れ（実回線の、たまに来る遅延の山を模す）。
    Spiky,
}

impl JitterShape {
    pub const NAMES: &'static str = "uniform か spiky";

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "uniform" => Some(Self::Uniform),
            "spiky" => Some(Self::Spiky),
            _ => None,
        }
    }
}

/// 起動の引数から作る、壊し方の設定。全部 0 なら何も壊さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultConfig {
    pub seed: u64,
    pub drop_pct: u8,
    /// 遅らせる割合（%）。遅れは 1〜`delay_max_ms` ms（後に送ったものに追い越される）。
    pub delay_pct: u8,
    /// 重ねる割合（%）。1 通はすぐ、もう 1 通を 1〜`delay_max_ms` ms 遅らせて。
    pub dup_pct: u8,
    pub delay_max_ms: u32,
    /// 決め打ち: 送った通番（1 から）がこの倍数なら落とし始める（0 なら使わない）。
    pub drop_every: u32,
    /// 決め打ち: 落とし始めたら続けて落とす通数（0 は 1 と同じ）。
    pub drop_run: u32,
    /// 決め打ち: 送った通番がこれを越えたら全部落とす（0 なら使わない）。「繋がった後に相手が黙る」形を作る。
    pub drop_after: u64,
    /// 全部の送りに足す一定の遅れ（ms。0 なら足さない。上限 `MAX_ONE_WAY_MS`）。
    pub one_way_ms: u32,
    /// コピー 1 通ごとに足す揺らぎの上限（ms。0 なら足さない。上限 `MAX_JITTER_MS`）。
    pub jitter_ms: u32,
    pub jitter_shape: JitterShape,
    /// ネットワークの上で名乗るプロトコルのバージョン（None なら今のバージョン `wire::PROTO_VER`）。
    pub claim_ver: Option<u8>,
}

/// 揺らぎの上限（ms）。一定の遅れの上限と同じ理由（それより長いのは沈黙の検査）。
pub const MAX_JITTER_MS: u32 = 1_000;

/// 一定の遅れの上限（ms）。delay の上限 9 フレーム（60 fps で 150 ms）を片道で大きく越える値まで。これより長い片道は
/// 遅延の検査ではなく沈黙の検査（接続の手順と生存の 10 秒）になるので断る（道具の決め事）。
pub const MAX_ONE_WAY_MS: u32 = 1_000;

#[derive(Debug, Clone)]
pub struct Relabel {
    key: AuthKey,
    claim: u8,
}

impl Relabel {
    pub fn new(secret: &Secret, session_id: u32, claim: u8) -> Relabel {
        Relabel {
            key: AuthKey::derive(secret, session_id),
            claim,
        }
    }

    pub fn claim(&self) -> u8 {
        self.claim
    }

    /// 今のバージョン ⇔ 名乗るバージョン を入れ替える（どちらでもないバージョンはそのまま）。送る向きと受ける向きで同じ式（入れ替えは 2 回で戻る）。
    pub fn swap_ver(&self, v: u8) -> u8 {
        if v == PROTO_VER {
            self.claim
        } else if v == self.claim {
            PROTO_VER
        } else {
            v
        }
    }

    /// 1 データグラムのヘッダのバージョン（と HELLO なら本文のバージョン）を入れ替えてタグを付け直す。HGRT でない・短い（タグまで無い）・
    /// バージョンが入れ替わらないものは None（そのまま使う）。タグを付け直すので、タグの合わない物（別の鍵）を入れ替えると合う物に
    /// 化けるが、受ける向きでは入れ替える前にセッションと同じ鍵でタグを確かめ、合わない物は触らない（毒の検査を化けさせない）。
    pub fn apply(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.len() < HEADER_LEN + TAG_LEN || !bytes.starts_with(&MAGIC) {
            return None;
        }
        let (signed, tag) = bytes.split_at(bytes.len() - TAG_LEN);
        if !self.key.verify(signed, tag) {
            return None;
        }
        let v = signed[4];
        let to = self.swap_ver(v);
        if to == v {
            return None;
        }
        let mut out = signed.to_vec();
        out[4] = to;
        if out[5] == 1 && out.get(HEADER_LEN) == Some(&v) {
            out[HEADER_LEN] = to;
        }
        let t = self.key.tag(&out);
        out.extend_from_slice(&t);
        Some(out)
    }
}

impl FaultConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.claim_ver == Some(PROTO_VER) {
            return Err(format!(
                "--proto-ver の値 {PROTO_VER} は今のバージョンと同じ（今のバージョンを名乗るなら書かない）"
            ));
        }
        if self.one_way_ms > MAX_ONE_WAY_MS {
            return Err(format!(
                "一定の遅れ（--one-way-ms）は 0〜{MAX_ONE_WAY_MS} ms"
            ));
        }
        if self.jitter_ms > MAX_JITTER_MS {
            return Err(format!("揺らぐ遅れ（--jitter-ms）は 0〜{MAX_JITTER_MS} ms"));
        }
        let sum = u32::from(self.drop_pct) + u32::from(self.delay_pct) + u32::from(self.dup_pct);
        if sum > 100 {
            return Err(format!(
                "落とす・遅らせる・重ねるの割合の和が {sum}% で 100% を超える"
            ));
        }
        if (self.delay_pct > 0 || self.dup_pct > 0) && self.delay_max_ms == 0 {
            return Err("遅らせる・重ねるには遅れの幅（--delay-max）が 1 ms 以上要る".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    Deliver,
    Drop,
    Delay(u32),
    Dup(u32),
}

#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z ^= z >> 31;
        Rng(if z == 0 { 1 } else { z })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultCounts {
    pub decided: u64,
    pub dropped: u64,
    pub delayed: u64,
    pub dupped: u64,
    pub jittered: u64,
}

#[derive(Debug, Clone)]
pub struct Faults {
    cfg: FaultConfig,
    rng: Rng,
    /// 揺らぎ専用の乱数（運命の列を変えないため別に持つ）。
    jitter_rng: Rng,
    run_left: u32,
    counts: FaultCounts,
}

impl Faults {
    pub fn new(cfg: FaultConfig) -> Self {
        Faults {
            cfg,
            rng: Rng::new(cfg.seed),
            jitter_rng: Rng::new(cfg.seed ^ 0x6A17_7E52_D00D_F00D),
            run_left: 0,
            counts: FaultCounts::default(),
        }
    }

    pub fn counts(&self) -> FaultCounts {
        self.counts
    }

    /// 全部の送りに足す一定の遅れ（ms）。運命（`decide`）とは別に、送る側が足す。
    pub fn one_way_ms(&self) -> u32 {
        self.cfg.one_way_ms
    }

    /// 送るコピー 1 通に足す揺らぎ（ms。0〜`jitter_ms`）。0 設定なら乱数を引かず 0。
    pub fn jitter(&mut self) -> u32 {
        let n = u64::from(self.cfg.jitter_ms);
        if n == 0 {
            return 0;
        }
        let ms = self.draw_jitter(n);
        if ms > 0 {
            self.counts.jittered += 1;
        }
        ms
    }

    fn draw_jitter(&mut self, n: u64) -> u32 {
        match self.cfg.jitter_shape {
            JitterShape::Uniform => self.jitter_rng.below(n + 1) as u32,
            JitterShape::Spiky => {
                if self.jitter_rng.below(100) < 90 {
                    self.jitter_rng.below(n / 10 + 1) as u32
                } else {
                    (n / 2 + self.jitter_rng.below(n - n / 2 + 1)) as u32
                }
            }
        }
    }

    pub fn decide(&mut self) -> Fate {
        self.counts.decided += 1;
        let fate = self.pick();
        match fate {
            Fate::Deliver => {}
            Fate::Drop => self.counts.dropped += 1,
            Fate::Delay(_) => self.counts.delayed += 1,
            Fate::Dup(_) => self.counts.dupped += 1,
        }
        fate
    }

    fn pick(&mut self) -> Fate {
        if self.run_left > 0 {
            self.run_left -= 1;
            return Fate::Drop;
        }
        let n = self.counts.decided;
        if self.cfg.drop_after > 0 && n > self.cfg.drop_after {
            return Fate::Drop;
        }
        if self.cfg.drop_every > 0 && n.is_multiple_of(u64::from(self.cfg.drop_every)) {
            self.run_left = self.cfg.drop_run.saturating_sub(1);
            return Fate::Drop;
        }
        let (d, l, u) = (
            u64::from(self.cfg.drop_pct),
            u64::from(self.cfg.delay_pct),
            u64::from(self.cfg.dup_pct),
        );
        if d + l + u == 0 {
            return Fate::Deliver;
        }
        let r = self.rng.below(100);
        let width = u64::from(self.cfg.delay_max_ms);
        if r < d {
            Fate::Drop
        } else if r < d + l {
            Fate::Delay(1 + self.rng.below(width) as u32)
        } else if r < d + l + u {
            Fate::Dup(1 + self.rng.below(width) as u32)
        } else {
            Fate::Deliver
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fates(cfg: FaultConfig, n: usize) -> Vec<Fate> {
        let mut f = Faults::new(cfg);
        (0..n).map(|_| f.decide()).collect()
    }

    #[test]
    fn nothing_configured_delivers_everything() {
        assert!(
            fates(FaultConfig::default(), 1000)
                .iter()
                .all(|f| *f == Fate::Deliver)
        );
    }

    #[test]
    fn drop_every_with_run_drops_exactly_those_ordinals() {
        let cfg = FaultConfig {
            drop_every: 7,
            drop_run: 3,
            ..FaultConfig::default()
        };
        let dropped: Vec<usize> = fates(cfg, 25)
            .iter()
            .enumerate()
            .filter(|(_, f)| **f == Fate::Drop)
            .map(|(i, _)| i + 1)
            .collect();
        assert_eq!(dropped, vec![7, 8, 9, 14, 15, 16, 21, 22, 23]);
    }

    #[test]
    fn drop_after_drops_everything_past_the_ordinal() {
        let cfg = FaultConfig {
            drop_after: 5,
            ..FaultConfig::default()
        };
        let f = fates(cfg, 8);
        assert!(f[..5].iter().all(|x| *x == Fate::Deliver), "{f:?}");
        assert!(f[5..].iter().all(|x| *x == Fate::Drop), "{f:?}");
    }

    #[test]
    fn same_seed_same_fates_and_ratios_apply() {
        let cfg = FaultConfig {
            seed: 42,
            drop_pct: 20,
            delay_pct: 10,
            dup_pct: 5,
            delay_max_ms: 40,
            ..FaultConfig::default()
        };
        let a = fates(cfg, 10_000);
        assert_eq!(a, fates(cfg, 10_000));
        let other = fates(FaultConfig { seed: 43, ..cfg }, 10_000);
        assert_ne!(a, other, "seed が効いていない");
        let drops = a.iter().filter(|f| **f == Fate::Drop).count();
        let delays: Vec<u32> = a
            .iter()
            .filter_map(|f| match f {
                Fate::Delay(x) | Fate::Dup(x) => Some(*x),
                _ => None,
            })
            .collect();
        assert!((1_700..=2_300).contains(&drops), "{drops}");
        assert!((1_200..=1_800).contains(&delays.len()), "{}", delays.len());
        assert!(delays.iter().all(|&x| (1..=40).contains(&x)));
        assert!(
            delays.contains(&1) && delays.contains(&40),
            "幅の両端が出ない"
        );
    }

    #[test]
    fn validate_refuses_bad_mixes() {
        let over = FaultConfig {
            drop_pct: 60,
            delay_pct: 30,
            dup_pct: 11,
            delay_max_ms: 5,
            ..FaultConfig::default()
        };
        assert!(over.validate().is_err());
        assert!(
            FaultConfig {
                dup_pct: 10,
                ..over
            }
            .validate()
            .is_ok()
        );
        let no_width = FaultConfig {
            delay_pct: 5,
            ..FaultConfig::default()
        };
        assert!(no_width.validate().is_err());
        let lag = |ms| FaultConfig {
            one_way_ms: ms,
            ..FaultConfig::default()
        };
        assert!(lag(MAX_ONE_WAY_MS).validate().is_ok());
        assert!(lag(MAX_ONE_WAY_MS + 1).validate().is_err());
    }

    #[test]
    fn one_way_delay_does_not_change_the_fates() {
        let cfg = FaultConfig {
            seed: 9,
            drop_pct: 20,
            delay_pct: 10,
            delay_max_ms: 30,
            drop_every: 13,
            ..FaultConfig::default()
        };
        assert_eq!(
            fates(cfg, 2_000),
            fates(
                FaultConfig {
                    one_way_ms: 80,
                    ..cfg
                },
                2_000
            )
        );
    }

    #[test]
    fn jitter_stays_in_range_is_seeded_and_shaped() {
        let cfg = FaultConfig {
            seed: 5,
            jitter_ms: 40,
            ..FaultConfig::default()
        };
        let draw = |c: FaultConfig| {
            let mut f = Faults::new(c);
            (0..10_000).map(|_| f.jitter()).collect::<Vec<u32>>()
        };
        let a = draw(cfg);
        assert_eq!(a, draw(cfg), "同じ seed で列が違う");
        assert_ne!(a, draw(FaultConfig { seed: 6, ..cfg }), "seed が効かない");
        assert!(a.iter().all(|&x| x <= 40), "幅の外");
        assert!(a.contains(&0) && a.contains(&40), "両端が出ない");
        let low = a.iter().filter(|&&x| x < 20).count();
        assert!((4_500..=5_500).contains(&low), "{low}");
        assert!(a.windows(2).filter(|w| w[1] < w[0]).count() > 3_000);

        let spiky = draw(FaultConfig {
            jitter_shape: JitterShape::Spiky,
            ..cfg
        });
        assert!(spiky.iter().all(|&x| x <= 40));
        let big = spiky.iter().filter(|&&x| x >= 20).count();
        assert!((800..=1_200).contains(&big), "大きい遅れ {big}");
        assert!(spiky.iter().filter(|&&x| x <= 4).count() > 8_000);
        assert!(spiky.contains(&40) && spiky.contains(&20));

        let mut f = Faults::new(cfg);
        let nonzero = (0..1_000).filter(|_| f.jitter() > 0).count() as u64;
        assert_eq!(f.counts().jittered, nonzero, "数え違い");
        assert!(draw(FaultConfig::default()).iter().all(|&x| x == 0));
        let base = FaultConfig {
            seed: 9,
            drop_pct: 20,
            delay_pct: 10,
            delay_max_ms: 30,
            ..FaultConfig::default()
        };
        assert_eq!(
            fates(base, 2_000),
            fates(
                FaultConfig {
                    jitter_ms: 500,
                    ..base
                },
                2_000
            )
        );
        let j = |ms| FaultConfig {
            jitter_ms: ms,
            ..FaultConfig::default()
        };
        assert!(j(MAX_JITTER_MS).validate().is_ok());
        assert!(j(MAX_JITTER_MS + 1).validate().is_err());
    }

    #[test]
    fn relabel_swaps_the_version_both_ways_and_reseals() {
        use higanr_core::wire::{self, DecodeError, Hello, MAX_DATAGRAM, Message, Mode, NAME_MAX};
        let secret = Secret([0x42; 16]);
        let sid = 0x5EED_0007;
        let key = AuthKey::derive(&secret, sid);
        let rl = Relabel::new(&secret, sid, 0);
        assert_ne!(
            PROTO_VER, 0,
            "前提: 名乗るバージョン 0 は今のバージョンと違う"
        );
        let mut buf = [0u8; MAX_DATAGRAM];
        let hello = Message::Hello(Hello {
            proto_ver: PROTO_VER,
            latency: 1,
            name_len: 0,
            mode: Mode::Delay,
            can_rollback: false,
            listable: false,
            role_nonce: 9,
            name: [0; NAME_MAX],
            seat_wish: wire::SeatWish::P1,
            arrival: wire::Arrival::Unknown,
            build: higanr_core::update::BUILD_NUMBER,
            public: [0x09; 32],
        });
        let n = wire::encode(&key, sid, &hello, &mut buf);
        let now_ver = buf[..n].to_vec();
        let old = rl.apply(&now_ver).expect("今のバージョンの物は入れ替わる");
        assert_eq!(
            (old[4], old[HEADER_LEN]),
            (0, 0),
            "ヘッダと本文のバージョン"
        );
        assert_eq!(
            wire::decode(&key, None, &old),
            Err(DecodeError::BadVersion(0))
        );
        let back = rl.apply(&old).expect("名乗ったバージョンの物は戻る");
        assert_eq!(back, now_ver);
        assert_eq!(wire::decode(&key, None, &back).map(|p| p.msg), Ok(hello));
        assert_eq!(
            (rl.swap_ver(PROTO_VER), rl.swap_ver(0), rl.swap_ver(8)),
            (0, PROTO_VER, 8)
        );
        let mut bad = now_ver.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(rl.apply(&bad), None);
        assert_eq!(rl.apply(b"STUN-or-anything-else-not-hgrt-at-all"), None);
        assert_eq!(rl.apply(&now_ver[..HEADER_LEN]), None);
        let c = |v| FaultConfig {
            claim_ver: Some(v),
            ..FaultConfig::default()
        };
        assert!(c(PROTO_VER).validate().is_err());
        assert!(c(0).validate().is_ok());
    }
}
