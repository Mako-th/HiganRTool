//! 検査用の書き込み側: 合成のリプレイ（`T9RP`）を組む。`replay_decode` の往復と `replay_inputs` の検査が使う。
//! 書くのは配列 A / B / C（と D）とゲームが検査する見出しだけ。実在の値は入れない（名前は既定の `NO NAME`）。
//! 読み側と同じ思い違いをしていると往復は通るので、読み側には仕様から手で組んだ標本の検査もある。

use crate::replay_decode::{
    BLOCKS_START, BODY_START, CHECKSUM_SEED, CHUNK_FRAMES, ENC_START, FPS_EVERY,
    LZSS_INITIAL_WRITE, SLOTS, STAGE_HEADER_LEN, encrypt_in_place, expected_fps_len,
};

/// 履歴のリングバッファ。`replay_decode` と同じ値だが、片方だけの直しに気づけるよう別に持つ。
const WINDOW: usize = 8192;
const WINDOW_MASK: usize = WINDOW - 1;
const MAX_MATCH: usize = 18;
const MIN_MATCH: usize = 3;

/// 上位ビットから書くビット列。バイトにそろった分だけ出し、端数は捨てる（`0x0043421B`〜）。
struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, bit: bool) {
        self.cur = (self.cur << 1) | u8::from(bit);
        self.n += 1;
        if self.n == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.n = 0;
        }
    }

    fn put_bits(&mut self, value: u32, width: u32) {
        for i in (0..width).rev() {
            self.put((value >> i) & 1 == 1);
        }
    }
}

/// 展開の逆（ゲームの圧縮 `0x00434020` と同じ形式）。履歴は 8192 バイトのリングバッファで、書き込み位置は 1 から。
/// 位置 0 は終わりの印なので一致には使わない。まだ書いていない位置も使わない: ゲームの展開は履歴を 0 で埋めておらず、
/// ゲーム自身の圧縮もそこを指さない（指したリプレイは一覧で読めない枠になった）。
pub fn lzss_compress(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter {
        out: Vec::new(),
        cur: 0,
        n: 0,
    };
    let mut history = vec![0u8; WINDOW];
    let mut filled = vec![false; WINDOW];
    let mut write = LZSS_INITIAL_WRITE;
    let mut i = 0usize;
    while i < data.len() {
        // 一致の途中でリングバッファへ書かれる分（重なり）も見る。
        let mut best_len = 0usize;
        let mut best_pos = 0usize;
        let max = MAX_MATCH.min(data.len() - i);
        if max >= MIN_MATCH {
            for pos in 1..WINDOW {
                let mut len = 0usize;
                while len < max {
                    let q = (pos + len) & WINDOW_MASK;
                    // q が一致の中で既に書いた位置なら、その値はデータの側。
                    let back = (q + WINDOW - write) & WINDOW_MASK;
                    let src = if back < len {
                        data[i + back]
                    } else if filled[q] {
                        history[q]
                    } else {
                        break;
                    };
                    if src != data[i + len] {
                        break;
                    }
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_pos = pos;
                    if len == max {
                        break;
                    }
                }
            }
        }
        if best_len >= MIN_MATCH {
            w.put(false);
            w.put_bits(best_pos as u32, 13);
            w.put_bits((best_len - MIN_MATCH) as u32, 4);
            for k in 0..best_len {
                history[write] = data[i + k];
                filled[write] = true;
                write = (write + 1) & WINDOW_MASK;
            }
            i += best_len;
        } else {
            w.put(true);
            w.put_bits(u32::from(data[i]), 8);
            history[write] = data[i];
            filled[write] = true;
            write = (write + 1) & WINDOW_MASK;
            i += 1;
        }
    }
    // 終わりの印: フラグ 0 と 13 bit の 0。端数は書かない。
    w.put(false);
    w.put_bits(0, 13);
    w.out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSpec {
    pub slot: u8,
    pub rng_seed: u16,
    pub p1_character: u8,
    pub p2_character: u8,
    pub p1_human_cpu: u8,
    pub p2_human_cpu: u8,
    pub p2_cpu_level: u16,
    pub p1_inputs: Vec<u16>,
    pub p2_inputs: Vec<u16>,
    /// `None` なら `A | B`（実データの姿）。
    pub p3_inputs: Option<Vec<u16>>,
    /// `None` ならフレーム数から決まる長さで全部 60。
    pub fps: Option<Vec<u8>>,
}

impl StageSpec {
    pub fn new(slot: u8, p1_inputs: Vec<u16>, p2_inputs: Vec<u16>) -> Self {
        StageSpec {
            slot,
            rng_seed: 0xBEEF,
            p1_character: 3,
            p2_character: 11,
            p1_human_cpu: 0,
            p2_human_cpu: 0,
            p2_cpu_level: 15,
            p1_inputs,
            p2_inputs,
            p3_inputs: None,
            fps: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    /// 暗号の鍵（`+0x15`）。
    pub key: u8,
    /// `+0x14` の乱数のゴミ。
    pub junk: u8,
    pub game_mode: u8,
    pub difficulty: u8,
    pub stages: Vec<StageSpec>,
    /// USER チャンクの本文（Shift-JIS。末尾の NUL は足す）。
    pub user_text: Vec<u8>,
}

impl BuildSpec {
    /// Match（枠 9 だけ。2 席とも人間）。
    pub fn match_replay(key: u8, p1_inputs: Vec<u16>, p2_inputs: Vec<u16>) -> Self {
        BuildSpec {
            key,
            junk: 0x5A,
            game_mode: 2,
            difficulty: 1,
            stages: vec![StageSpec::new(9, p1_inputs, p2_inputs)],
            user_text: b"HIGANR TEST\r\n".to_vec(),
        }
    }
}

fn put_u32(v: &mut [u8], off: usize, x: u32) {
    v[off..off + 4].copy_from_slice(&x.to_le_bytes());
}

/// A / B / C のブロック（先頭の 0x20 バイトの構造体 ＋ 1 フレーム u16）。
fn stage_block(spec: &StageSpec, side: u8, words: &[u16]) -> Vec<u8> {
    let mut b = vec![0u8; STAGE_HEADER_LEN];
    if side < 2 {
        b[0x04..0x06].copy_from_slice(&spec.rng_seed.to_le_bytes());
        let (ch, hc) = if side == 0 {
            (spec.p1_character, spec.p1_human_cpu)
        } else {
            (spec.p2_character, spec.p2_human_cpu)
        };
        b[0x06] = ch;
        b[0x07] = hc;
        if side == 1 {
            b[0x0A..0x0C].copy_from_slice(&spec.p2_cpu_level.to_le_bytes());
        }
    } else {
        // C は `+0x04`（RNG）以外 0。
        b[0x04..0x06].copy_from_slice(&spec.rng_seed.to_le_bytes());
    }
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b
}

/// 合成のリプレイ（末尾に USER チャンク 1 つ）。`slot`（0〜9）が枠で重ならないのは呼ぶ側の責任。
pub fn build_replay(spec: &BuildSpec) -> Vec<u8> {
    // 本体: `0xC0` 以降。見出し部（`0xC0`〜`0x1EC`）→ 配列 A、B、C、D の順に、使う枠だけ。
    let mut body = vec![0u8; BLOCKS_START - BODY_START];
    body[0] = 0x77; // 0xC0: 乱数のゴミ
    body[1] = b'a'; // 0xC1: バージョンの枝番
    body[2..4].copy_from_slice(&0x0150u16.to_le_bytes()); // 0xC2
    body[0xC4 - BODY_START..0xC4 - BODY_START + 9].copy_from_slice(b"26/01/01\0"); // 日付（合成）
    body[0xCE - BODY_START..0xCE - BODY_START + 8].copy_from_slice(b"NO NAME\0"); // 既定の名前
    body[0xD7 - BODY_START] = spec.difficulty;
    for (i, x) in body[0xDC - BODY_START..0x1A8 - BODY_START]
        .iter_mut()
        .enumerate()
    {
        *x = (i % 251) as u8; // オプションの塊の代わり
    }
    body[0x1DC - BODY_START..0x1DC - BODY_START + 6].copy_from_slice(b"0150a\0");
    body[0x1E4 - BODY_START] = spec.game_mode;

    let mut table = [[0u32; SLOTS]; 4];
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    let mut pos = BLOCKS_START;
    let mut place = |ai: usize, slot: usize, blk: Vec<u8>| {
        table[ai][slot] = pos as u32;
        pos += blk.len();
        blocks.push(blk);
    };
    for ai in 0..3usize {
        for st in &spec.stages {
            let words: Vec<u16> = match ai {
                0 => st.p1_inputs.clone(),
                1 => st.p2_inputs.clone(),
                _ => st.p3_inputs.clone().unwrap_or_else(|| {
                    st.p1_inputs
                        .iter()
                        .zip(&st.p2_inputs)
                        .map(|(a, b)| a | b)
                        .collect()
                }),
            };
            place(ai, st.slot as usize, stage_block(st, ai as u8, &words));
        }
    }
    for st in &spec.stages {
        let frames = st.p1_inputs.len();
        let fps = st
            .fps
            .clone()
            .unwrap_or_else(|| vec![60u8; expected_fps_len(frames)]);
        place(3, st.slot as usize, fps);
    }
    for b in &blocks {
        body.extend_from_slice(b);
    }

    let lzss = lzss_compress(&body);
    let comp_size = BODY_START + lzss.len();
    let mut plain = vec![0u8; BODY_START];
    plain[0..4].copy_from_slice(b"T9RP");
    plain[4] = 2;
    plain[7] = 1;
    put_u32(&mut plain, 0x0C, comp_size as u32);
    plain[0x14] = spec.junk;
    plain[0x15] = spec.key;
    put_u32(&mut plain, 0x18, lzss.len() as u32);
    put_u32(&mut plain, 0x1C, body.len() as u32);
    for (ai, base) in [0x20usize, 0x48, 0x70, 0x98].iter().enumerate() {
        for (slot, &ptr) in table[ai].iter().enumerate() {
            put_u32(&mut plain, base + 4 * slot, ptr);
        }
    }
    plain.extend_from_slice(&lzss);
    let sum = plain[0x15..comp_size]
        .iter()
        .fold(CHECKSUM_SEED, |s, &b| s.wrapping_add(u32::from(b)));
    put_u32(&mut plain, 0x10, sum);
    let mut file = plain;
    encrypt_in_place(&mut file[ENC_START..comp_size], spec.key);

    // USER チャンク（ゲームと同じ: 全長 = 本文 + 13 を偶数へ、`+0x08` は残り物）。
    let total = (spec.user_text.len() + 13 + 1) & !1;
    file.extend_from_slice(b"USER");
    file.extend_from_slice(&(total as u32).to_le_bytes());
    file.extend_from_slice(&0x7FFF_FF00u32.to_le_bytes());
    file.extend_from_slice(&spec.user_text);
    file.push(0);
    file.resize(comp_size + total, 0);
    file
}

/// 読み側の `CHUNK_FRAMES` / `FPS_EVERY` をコピーした定数。検査が食い違いを見る場所。
pub fn chunk_constants() -> (usize, usize) {
    (CHUNK_FRAMES, FPS_EVERY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{MarkRead, read_mark};

    #[test]
    fn built_file_is_a_valid_container_for_replay_rs() {
        let f = build_replay(&BuildSpec::match_replay(0x60, vec![1, 2, 3], vec![0, 0, 4]));
        let layout = crate::replay::parse(&f).unwrap();
        assert_eq!(layout.chunks.len(), 1);
        assert_eq!(read_mark(&f).unwrap(), MarkRead::Absent);
        assert_eq!(chunk_constants(), (3598, 30));
    }

    /// 履歴を `fill` で埋めて展開する（ゲームの展開は履歴を 0 で埋めない）。終わりの印で止まる。
    fn unlzss_with_fill(data: &[u8], fill: u8) -> Vec<u8> {
        let bit = |p: usize| (data[p / 8] >> (7 - p % 8)) & 1;
        let take = |p: &mut usize, n: usize| {
            let mut v = 0usize;
            for _ in 0..n {
                v = (v << 1) | usize::from(bit(*p));
                *p += 1;
            }
            v
        };
        let mut history = vec![fill; WINDOW];
        let mut write = LZSS_INITIAL_WRITE;
        let mut out = Vec::new();
        let mut p = 0usize;
        while p + 14 <= data.len() * 8 {
            if take(&mut p, 1) == 1 {
                let b = take(&mut p, 8) as u8;
                out.push(b);
                history[write] = b;
                write = (write + 1) & WINDOW_MASK;
                continue;
            }
            let mut from = take(&mut p, 13);
            if from == 0 {
                break;
            }
            for _ in 0..take(&mut p, 4) + MIN_MATCH {
                let b = history[from];
                out.push(b);
                history[write] = b;
                write = (write + 1) & WINDOW_MASK;
                from = (from + 1) & WINDOW_MASK;
            }
        }
        out
    }

    #[test]
    fn compress_never_reads_unwritten_history() {
        // 先頭が 0 の並びだと、0 のまま書いていない履歴の位置に一致してしまう。
        let mut data = vec![0x77u8, 0x61, 0x50, 0x01];
        data.extend(std::iter::repeat_n(0u8, 40));
        data.extend((0..3000u32).map(|i| (i % 13) as u8));
        data.extend(std::iter::repeat_n(0u8, 100));
        let c = lzss_compress(&data);
        assert_eq!(unlzss_with_fill(&c, 0x00), data);
        assert_eq!(
            unlzss_with_fill(&c, 0xAA),
            data,
            "書いていない履歴の位置を指す一致がある"
        );
    }

    #[test]
    fn compress_ends_with_dropped_partial_byte() {
        // 空は 14 bit で 1 バイト、リテラル 1 つは 9 + 14 = 23 bit で 2 バイト。
        assert_eq!(lzss_compress(&[]), vec![0x00]);
        assert_eq!(lzss_compress(&[0xFF]), vec![0xFF, 0x80]);
    }
}
