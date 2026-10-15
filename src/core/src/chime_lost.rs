//! 相手との接続が切れた知らせの音（短く下がる 2 音）を、その場で合成して WAV のバイト列にする。
//!
//! 接続した知らせ（`chime`。上がる 2 音の鐘で約 0.9 秒）と聞き分けられるよう、向きを逆にして短く切った柔らかい音にした。
//! 確保するのでゲームのスレッドでは呼ばない。

use std::f64::consts::TAU;

use crate::chime::{PEAK, RATE, wav};

pub const SECONDS: f64 = 0.3;

/// 1 音: 始まる時刻（秒）・基音（Hz）・長さ（秒）。シ（B5）→ ミ（E5）と、接続の音（ミ → シ）の逆向き。
const NOTES: [(f64, f64, f64); 2] = [(0.0, 987.767, 0.13), (0.14, 659.255, 0.16)];
/// 倍音（基音の倍数と大きさ）。鐘のように長く響かせないので、減衰は 1 音の長さの中で閉じる。
const PARTIALS: [(f64, f64); 2] = [(1.0, 1.0), (2.0, 0.25)];
/// 立ち上がりと 1 音の終わりの丸め（秒）。急に鳴らす・止めるとプツッと聞こえる。
const EDGE: f64 = 0.006;
/// 1 音の中の減衰の時定数（秒）。
const DECAY: f64 = 0.09;

fn ramp(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    0.5 - 0.5 * (x * TAU / 2.0).cos()
}

pub fn lost_samples() -> Vec<i16> {
    let n = (f64::from(RATE) * SECONDS) as usize;
    let rate = f64::from(RATE);
    let raw: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let mut v = 0.0;
            for &(start, f0, len) in &NOTES {
                let u = t - start;
                if !(0.0..len).contains(&u) {
                    continue;
                }
                let env = ramp(u / EDGE) * ramp((len - u) / EDGE) * (-u / DECAY).exp();
                for &(k, amp) in &PARTIALS {
                    v += env * amp * (TAU * f0 * k * u).sin();
                }
            }
            v
        })
        .collect();
    let max = raw.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let scale = if max > 0.0 {
        f64::from(PEAK) / max
    } else {
        0.0
    };
    raw.iter().map(|v| (v * scale).round() as i16).collect()
}

pub fn lost_wav() -> Vec<u8> {
    wav(RATE, &lost_samples())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `f` Hz の成分の強さ（Goertzel）。
    fn power_at(s: &[i16], from: usize, to: usize, f: f64) -> f64 {
        let w = TAU * f / f64::from(RATE);
        let c = 2.0 * w.cos();
        let (mut p1, mut p2) = (0.0, 0.0);
        for &x in &s[from..to] {
            let p0 = f64::from(x) + c * p1 - p2;
            p2 = p1;
            p1 = p0;
        }
        p1 * p1 + p2 * p2 - c * p1 * p2
    }

    #[test]
    fn it_is_short_full_level_and_starts_and_ends_silent() {
        let s = lost_samples();
        assert_eq!(s.len(), (f64::from(RATE) * SECONDS) as usize);
        assert!(
            s.len() * 2 < crate::chime::connected_samples().len(),
            "接続の音の半分より短く"
        );
        let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(
            (i32::from(peak) - i32::from(PEAK)).abs() <= 1,
            "いちばん大きい標本 {peak}"
        );
        assert!(s[0].unsigned_abs() < 200 && s[s.len() - 1].unsigned_abs() < 200);
        // 2 音の間は無音（区切りが聞こえる）。
        let gap = (0.135 * f64::from(RATE)) as usize..(0.139 * f64::from(RATE)) as usize;
        assert!(s[gap].iter().all(|v| v.unsigned_abs() < 200));
    }

    #[test]
    fn the_pitch_falls_from_b_to_e() {
        let s = lost_samples();
        let r = f64::from(RATE);
        let first = (0.01 * r) as usize..(0.10 * r) as usize;
        let second = (0.16 * r) as usize..(0.26 * r) as usize;
        let (b, e) = (987.767, 659.255);
        assert!(
            power_at(&s, first.start, first.end, b)
                > 10.0 * power_at(&s, first.start, first.end, e)
        );
        assert!(
            power_at(&s, second.start, second.end, e)
                > 10.0 * power_at(&s, second.start, second.end, b)
        );
    }

    #[test]
    fn the_wav_wraps_the_samples() {
        let b = lost_wav();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(b.len(), 44 + lost_samples().len() * 2);
        assert_ne!(b, crate::chime::connected_wav());
    }
}
