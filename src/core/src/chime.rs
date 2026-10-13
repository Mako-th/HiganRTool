//! 繋がった知らせの音を、その場で合成して WAV のバイト列にする。
//!
//! 基音と倍音の正弦波を足し、高い倍音ほど早く減衰させた鐘の音を、2 音（ミ → シ）少しずらして重ねる。
//! 確保するのでゲームのスレッドでは呼ばない。

use std::f64::consts::TAU;

/// 標本化の周波数（Hz）。5 kHz より上は使わないので 22,050 で足りる。
pub const RATE: u32 = 22_050;
pub const SECONDS: f64 = 0.9;
/// いちばん大きい標本（16 ビットの満量のうち約 −6 dBFS）。
pub const PEAK: i16 = 16_000;

/// 1 音: 始まる時刻（秒）と基音（Hz）。
const NOTES: [(f64, f64); 2] = [
    (0.0, 659.255),  // ミ（E5）
    (0.14, 987.767), // シ（B5）
];
/// 倍音: 基音の倍数・大きさ・減衰の時定数（秒）。
const PARTIALS: [(f64, f64, f64); 4] = [
    (1.0, 1.0, 0.30),
    (2.0, 0.40, 0.16),
    (3.0, 0.18, 0.10),
    (4.2, 0.08, 0.06),
];
/// 立ち上がり（秒）。急に鳴らすとプツッと聞こえるので余弦で滑らかにする。
const ATTACK: f64 = 0.004;
const RELEASE: f64 = 0.03;

fn smooth(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    0.5 - 0.5 * (x * TAU / 2.0).cos()
}

pub fn connected_samples() -> Vec<i16> {
    let n = (f64::from(RATE) * SECONDS) as usize;
    let rate = f64::from(RATE);
    let raw: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let mut v = 0.0;
            for &(start, f0) in &NOTES {
                let u = t - start;
                if u < 0.0 {
                    continue;
                }
                let env = smooth(u / ATTACK);
                for &(k, amp, tau) in &PARTIALS {
                    v += env * amp * (-u / tau).exp() * (TAU * f0 * k * u).sin();
                }
            }
            v * smooth((SECONDS - t) / RELEASE)
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

pub fn wav(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + samples.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); // fmt の塊の長さ
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // チャンネル数
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes()); // 1 秒のバイト数
    b.extend_from_slice(&2u16.to_le_bytes()); // 1 標本のバイト数
    b.extend_from_slice(&16u16.to_le_bytes()); // ビット数
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

pub fn connected_wav() -> Vec<u8> {
    wav(RATE, &connected_samples())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

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
    fn wav_header_and_body() {
        let b = wav(8_000, &[1, -2, 0x1234]);
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32_at(&b, 4), 36 + 6);
        assert_eq!(&b[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(&b, 16), 16);
        assert_eq!(u16_at(&b, 20), 1);
        assert_eq!(u16_at(&b, 22), 1);
        assert_eq!(u32_at(&b, 24), 8_000);
        assert_eq!(u32_at(&b, 28), 16_000);
        assert_eq!(u16_at(&b, 32), 2);
        assert_eq!(u16_at(&b, 34), 16);
        assert_eq!(&b[36..40], b"data");
        assert_eq!(u32_at(&b, 40), 6);
        assert_eq!(&b[44..], &[1, 0, 0xFE, 0xFF, 0x34, 0x12]);
    }

    #[test]
    fn samples_are_the_right_length_level_and_end_quietly() {
        let s = connected_samples();
        assert_eq!(s.len(), (f64::from(RATE) * SECONDS) as usize);
        let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(
            (i32::from(peak) - i32::from(PEAK)).abs() <= 1,
            "いちばん大きい標本 {peak}"
        );
        assert_eq!(s[0], 0, "始まりが 0 でない");
        let tail_max = s[s.len() - 10..]
            .iter()
            .map(|v| v.unsigned_abs())
            .max()
            .unwrap();
        assert!(tail_max <= 200, "終わりの 10 標本の最大 {tail_max}");
        let head_max = s[..8].iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(
            head_max <= 2_000,
            "始まりの 8 標本の最大 {head_max}（立ち上がりが急）"
        );
    }

    #[test]
    fn two_notes_rise_in_order() {
        let s = connected_samples();
        let second = (0.14 * f64::from(RATE)) as usize;
        let (e, b) = (NOTES[0].1, NOTES[1].1);
        let early_e = power_at(&s, 0, second, e);
        let early_b = power_at(&s, 0, second, b);
        assert!(
            early_e > 100.0 * early_b,
            "始めの 0.14 秒: ミ {early_e:e} / シ {early_b:e}"
        );
        let late = second + (0.02 * f64::from(RATE)) as usize;
        let late_b = power_at(&s, late, late + 2_000, b);
        let late_off = power_at(&s, late, late + 2_000, 1_300.0);
        assert!(
            late_b > 100.0 * late_off,
            "2 音目: シ {late_b:e} / 1300 Hz {late_off:e}"
        );
    }
}
