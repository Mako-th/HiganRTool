//! リプレイ（`.rpy`）の暗号化・圧縮の部分を復号・展開し、マッチ（ステージの枠）ごと・フレームごとの
//! 1P / 2P（と 3 本目）の入力語を取り出す。
//!
//! `replay.rs` は平文の部分（`T9RP`・`comp_size`・USER チャンク）だけを読む。こちらは `+0x18` から `comp_size` の手前を読む。
//!
//! 形の根拠（ゲーム本体の読み込みの逆アセンブル。コードはコピーしていない）:
//! - `0x00420604` `[+6] == 0`、`0x0042060B`〜`0x00420631` 復号（鍵は `+0x15` の 1 バイト、`[0x18, comp_size)` を
//!   1 バイトごとに引いて鍵に 7 を足す。u8 で回る）、`0x0042063B`〜`0x0042064E` チェックサム
//!   （`0x3F000318` に、復号した後の `[0x15, comp_size)` のバイトを u32 で足し、`+0x10` と比べる）、
//!   `0x00420694`〜`0x004206A8` 展開（入力は `0xC0` から `[0x18]` バイト、出力は `[0x1C]` バイト）。
//! - 圧縮の書き方（`0x00434020`。展開の逆）: 履歴は 8192 バイトのリングバッファで書き込み位置は 1 から、フラグ 1 ＋ 8 bit が
//!   リテラル、フラグ 0 ＋ 13 bit の位置 ＋ 4 bit の（長さ − 3）が一致。終わりは、フラグ 0 と 13 bit の 0 を書き、
//!   8 bit にそろわなかった端数のビットは書かずに捨てる（`0x0043421B`〜`0x00434248`）。だから展開側は、
//!   終わりの印の位置の 13 bit が足りなくても 0 として読む。
//!
//! 3 本目の配列 C は `InputManager[2] + 0x2C`（1P と 2P を合わせた語）で、実データでは `A | B` と一致する。
//! 読み出して `p3` として持つだけで、一致を強制しない。
//!
//! どんなバイト列を渡されてもパニックしない（添字は `get`、足し算は `checked_*`、出力の大きさに上限）。
//! ヒープを使う（ゲームスレッドの外で呼ぶ）。

use crate::replay::{MAGIC, MIN_COMP_SIZE};

/// 復号の始まり（`+0x18`。`0x0042061B lea 0x18(%esi),%eax`）。
pub const ENC_START: usize = 0x18;
/// 鍵の位置（`+0x15`。`0x0042060E mov 0x15(%esi),%cl`）。
pub const KEY_OFFSET: usize = 0x15;
/// 鍵に足す数（`0x00420625 add $0x7,%cl`）。
pub const KEY_STEP: u8 = 7;
/// チェックサムの位置（`+0x10`。`0x0042064E cmp 0x10(%esi),%edi`）。
pub const CHECKSUM_OFFSET: usize = 0x10;
/// チェックサムの初期値（`0x0042063B mov $0x3f000318,%edi`）。
pub const CHECKSUM_SEED: u32 = 0x3F00_0318;
/// LZSS の入力の始まりで、展開後の本体（`image` の `0xC0` 以降）の始まり（`0x0042069B lea 0xc0(%ebx)`）。
pub const BODY_START: usize = 0xC0;
/// `comp_size`（`+0x0C`）の位置。
const COMP_SIZE_OFFSET: usize = 0x0C;
/// LZSS の入力の長さの位置（`+0x18`。`0x00420697 mov 0x18(%edi),%edx`）。
const LZSS_LEN_OFFSET: usize = 0x18;
/// 展開後の長さの位置（`+0x1C`。`0x00420694 mov 0x1c(%edi),%eax`）。
const DECOMP_SIZE_OFFSET: usize = 0x1C;
/// バージョン（`+0x04`。`0x004205FD cmpw $0x2,0x4(%esi)`）。
const VERSION_OFFSET: usize = 0x04;
/// `+0x06` は 0 でなければならない（`0x00420604`〜`0x00420609`）。
const RESERVED_OFFSET: usize = 0x06;

/// 展開後の長さの上限。実物は 1 本で高々数百 KB（1,455 本の平均で約 170 KB）なので、桁違いに大きいヘッダの値は
/// 壊れているとして展開しない（確保の爆発を避ける安全側の値）。
pub const MAX_DECOMP_SIZE: u32 = 64 << 20;

/// 配列 A・B・C・D のポインタ表の位置（各 10 本の u32）。
const ARRAY_TABLE: [usize; 4] = [0x20, 0x48, 0x70, 0x98];
/// ポインタの本数（枠の数）。
pub const SLOTS: usize = 10;
/// 配列の名前（`ARRAY_TABLE` と同じ順。A = 1P、B = 2P、C = `InputManager[2]`、D = FPS 表）。
const ARRAY_NAMES: [char; 4] = ['A', 'B', 'C', 'D'];
/// 最初のブロックが始まる位置（実物の 1,455 本すべてでここ）。
pub const BLOCKS_START: usize = 0x1EC;
/// A / B / C のブロックの先頭の構造体の長さ。その後ろに 1 フレームにつき u16。
pub const STAGE_HEADER_LEN: usize = 0x20;
/// 1 つの塊のフレーム数（`cmpl $0xe0e,0x10(%ecx)` = 3598）。D は 30 フレームに 1 バイトで、塊ごとに切り捨てる。
pub const CHUNK_FRAMES: usize = 3598;
/// D（FPS 表）は何フレームに 1 バイトか（`0x0042033A mov $0x1e,%ecx`）。
pub const FPS_EVERY: usize = 30;

/// ヘッダ `+0x0D7`: 難易度。
const DIFFICULTY_OFFSET: usize = 0xD7;
/// ヘッダ `+0x1E4`: game_mode（0 = Story、1 = Extra、2 = Match）。
const GAME_MODE_OFFSET: usize = 0x1E4;

/// 展開（LZSS）で読めなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LzssError {
    /// フラグ・リテラル・長さ・（0 でない）位置の途中でデータが尽きた。
    Truncated,
    /// 終わりの印（フラグ 0 と位置 0）が無いまま尽きた。
    MissingTerminator,
    /// 終わりの印の後に、読まれないビットが残っている（ゲームの書き方は端数を捨てるので、残らない）。
    TrailingBits { bits: usize },
    /// 出力が上限（ヘッダの展開後の長さ）を超える。
    Overflow { limit: usize },
}

/// 復号・展開・配列の読み出しができなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// 長さが `0x18` 未満（見出しすら無い）。
    TooShort { len: usize },
    /// 先頭 4 バイトが `T9RP` でない。
    BadMagic,
    /// `+0x04` が 2 でない（ゲームの読み込みが断る形）。
    BadVersion { version: u16 },
    /// `+0x06` が 0 でない（同上）。
    BadReserved { value: u8 },
    /// `comp_size` が `0xC0` 未満か、ファイルの長さを超える。
    CompSizeOutOfRange { comp_size: u32, len: usize },
    /// チェックサムが合わない（鍵・復号の範囲・壊れた保存のどれか）。
    ChecksumMismatch { stored: u32, computed: u32 },
    /// `+0x18`（LZSS の長さ）が `comp_size - 0xC0` と違う（書き手は同じ値から両方を作る）。
    LzssLenField { field: u32, expected: u32 },
    /// `+0x1C`（展開後の長さ）が上限 `MAX_DECOMP_SIZE` を超える。
    DecompSizeTooLarge { size: u32 },
    /// 展開できなかった。
    Lzss(LzssError),
    /// 展開した長さが `+0x1C` と違う。
    DecompSizeMismatch { field: u32, actual: usize },
    /// 展開後の本体が `0x1EC`（最初のブロックの位置）より短い。
    ImageTooShort { len: usize },
    /// ポインタが `0x1EC` 未満か、本体の長さを超える。
    BadPointer {
        array: char,
        slot: u8,
        ptr: u32,
        len: usize,
    },
    /// 2 つのポインタが同じ位置を指す（ブロックの境目が決まらない）。
    SharedPointer { ptr: u32 },
    /// A・B・C のどれかだけが 0 の枠（3 本そろって書かれるはず）。
    MissingArray { slot: u8 },
    /// A / B / C のブロックの長さが `0x20 + 2n` の形でない。
    BadBlockLen { array: char, slot: u8, len: usize },
    /// 同じ枠の A・B・C のフレーム数が違う。
    FrameCountMismatch { slot: u8 },
}

/// 席（1P / 2P）の面ごとの情報（ブロックの先頭の構造体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideInfo {
    /// `+0x00`（u32。開始時の累計。内部値）。
    pub score: u32,
    /// `+0x06`（キャラ ID）。
    pub character: u8,
    /// `+0x07`（0 = 人間、1 = CPU。CPU の側は入力列が全部 0）。
    pub human_cpu: u8,
    /// `+0x0A`（u16。CPU レベル）。
    pub cpu_level: u16,
}

/// 1 つの枠（ステージ）の入力。Match は枠 9 だけを使う（実測 1,358 本）。ラウンドの区切りはファイルに無いので区切らない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageInputs {
    /// 枠の番号（0〜9）。
    pub slot: u8,
    /// 開始時の乱数の状態（A の `+0x04`。ゲームが書き戻すのは A のもの）。
    pub rng_seed: u16,
    pub p1: SideInfo,
    pub p2: SideInfo,
    /// A: 1P の語（`InputManager[0] + 0x2C`）。1 フレームに 1 つ。
    pub p1_inputs: Vec<u16>,
    /// B: 2P の語（`InputManager[1] + 0x2C`）。CPU の席は全部 0。
    pub p2_inputs: Vec<u16>,
    /// C: `InputManager[2] + 0x2C`（実データでは `A | B`）。
    pub p3_inputs: Vec<u16>,
    /// D: FPS の記録（30 フレームに 1 バイト）。無ければ空。
    pub fps: Vec<u8>,
    /// D の長さが、フレーム数から決まる長さ（`expected_fps_len`）と同じか。
    pub fps_len_ok: bool,
}

impl StageInputs {
    pub fn frames(&self) -> usize {
        self.p1_inputs.len()
    }

    /// `C != (A | B)` のフレームの数（実データでは 0。0 でなくても読み出しは通す）。
    pub fn p3_not_or_count(&self) -> usize {
        self.p1_inputs
            .iter()
            .zip(&self.p2_inputs)
            .zip(&self.p3_inputs)
            .filter(|((a, b), c)| (**a | **b) != **c)
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayInputs {
    pub difficulty: u8,
    pub game_mode: u8,
    /// 使われている枠（A・B・C がそろっているもの）を番号順に。
    pub stages: Vec<StageInputs>,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// 復号（その場で）。鍵は 1 バイトごとに `KEY_STEP` を足し、鍵もバイトも u8 で回る（`0x00420620`〜`0x00420625`）。
pub fn decrypt_in_place(buf: &mut [u8], key: u8) {
    let mut k = key;
    for b in buf.iter_mut() {
        *b = b.wrapping_sub(k);
        k = k.wrapping_add(KEY_STEP);
    }
}

/// 暗号化（`decrypt_in_place` の逆）。
pub fn encrypt_in_place(buf: &mut [u8], key: u8) {
    let mut k = key;
    for b in buf.iter_mut() {
        *b = b.wrapping_add(k);
        k = k.wrapping_add(KEY_STEP);
    }
}

/// チェックサム: `CHECKSUM_SEED` に、復号した後の `[0x15, comp_size)` のバイトを u32 で足す（あふれは回る）。
/// `decrypted` は先頭から `comp_size` バイト以上あること（足りなければ `None`）。
pub fn checksum(decrypted: &[u8], comp_size: usize) -> Option<u32> {
    let span = decrypted.get(KEY_OFFSET..comp_size)?;
    Some(
        span.iter()
            .fold(CHECKSUM_SEED, |acc, &b| acc.wrapping_add(u32::from(b))),
    )
}

/// LZSS の履歴の大きさ（`1 << 13`。`0x004341B6 and $0x1fff`）。
const HISTORY_SIZE: usize = 1 << 13;
const HISTORY_MASK: usize = HISTORY_SIZE - 1;
/// 書き込み位置の初期値（`0x0043406D movl $0x1,-0x14(%ebp)`。位置 0 は終わりの印に使うので空ける）。
pub const LZSS_INITIAL_WRITE: usize = 1;
/// 一致の長さのビット幅と下駄（4 bit ＋ 3。長さは 3〜18）。
const LEN_BITS: u32 = 4;
const LEN_MIN: usize = 3;
const INDEX_BITS: u32 = 13;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// `n`（≤ 16）ビットを読む。足りなければ `None`（位置は動かさない）。
    fn take(&mut self, n: u32) -> Option<u32> {
        if (n as usize) > self.remaining() {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(v)
    }
}

/// LZSS を展開する。出力が `limit` バイトを超えたら断る。終わりの印の 13 bit は（ゲームの書き方が端数を捨てるので）
/// 足りなくてもよい。印が無い・印の後にビットが残っていれば断る。
pub fn unlzss(data: &[u8], limit: usize) -> Result<Vec<u8>, LzssError> {
    let mut bits = Bits { data, pos: 0 };
    let mut history = vec![0u8; HISTORY_SIZE];
    let mut write = LZSS_INITIAL_WRITE;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let Some(flag) = bits.take(1) else {
            return Err(LzssError::MissingTerminator);
        };
        if flag == 1 {
            let b = bits.take(8).ok_or(LzssError::Truncated)? as u8;
            if out.len() >= limit {
                return Err(LzssError::Overflow { limit });
            }
            out.push(b);
            if let Some(h) = history.get_mut(write) {
                *h = b;
            }
            write = (write + 1) & HISTORY_MASK;
        } else {
            // 位置の 13 bit。足りない分は 0（左詰め）で読む。0 になれば終わりの印。
            let avail = bits.remaining().min(INDEX_BITS as usize) as u32;
            let head = bits.take(avail).ok_or(LzssError::Truncated)?;
            let index = (head << (INDEX_BITS - avail)) as usize;
            if index == 0 {
                let left = bits.remaining();
                if left != 0 {
                    return Err(LzssError::TrailingBits { bits: left });
                }
                return Ok(out);
            }
            if avail < INDEX_BITS {
                return Err(LzssError::Truncated);
            }
            let len = bits.take(LEN_BITS).ok_or(LzssError::Truncated)? as usize + LEN_MIN;
            let mut from = index;
            for _ in 0..len {
                if out.len() >= limit {
                    return Err(LzssError::Overflow { limit });
                }
                let b = history.get(from).copied().unwrap_or(0);
                out.push(b);
                if let Some(h) = history.get_mut(write) {
                    *h = b;
                }
                write = (write + 1) & HISTORY_MASK;
                from = (from + 1) & HISTORY_MASK;
            }
        }
    }
}

/// 復号・展開した本体（`image`）。`image[..0x18]` は生の見出し、`image[0x18..0xC0]` は復号した見出し、
/// `image[0xC0..]` は展開した本体。USER チャンクは含まない。
pub fn decode_image(file: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let len = file.len();
    if len < ENC_START {
        return Err(DecodeError::TooShort { len });
    }
    if file.get(0..4) != Some(&MAGIC[..]) {
        return Err(DecodeError::BadMagic);
    }
    let version = u16_at(file, VERSION_OFFSET).ok_or(DecodeError::TooShort { len })?;
    if version != 2 {
        return Err(DecodeError::BadVersion { version });
    }
    let reserved = *file
        .get(RESERVED_OFFSET)
        .ok_or(DecodeError::TooShort { len })?;
    if reserved != 0 {
        return Err(DecodeError::BadReserved { value: reserved });
    }
    let comp_size = u32_at(file, COMP_SIZE_OFFSET).ok_or(DecodeError::TooShort { len })?;
    let comp = comp_size as usize;
    if comp_size < MIN_COMP_SIZE || comp > len {
        return Err(DecodeError::CompSizeOutOfRange { comp_size, len });
    }
    let key = *file.get(KEY_OFFSET).ok_or(DecodeError::TooShort { len })?;

    let mut buf: Vec<u8> = file
        .get(..comp)
        .ok_or(DecodeError::CompSizeOutOfRange { comp_size, len })?
        .to_vec();
    if let Some(enc) = buf.get_mut(ENC_START..) {
        decrypt_in_place(enc, key);
    }
    let computed = checksum(&buf, comp).ok_or(DecodeError::TooShort { len })?;
    let stored = u32_at(&buf, CHECKSUM_OFFSET).ok_or(DecodeError::TooShort { len })?;
    if computed != stored {
        return Err(DecodeError::ChecksumMismatch { stored, computed });
    }

    let lzss_len = u32_at(&buf, LZSS_LEN_OFFSET).ok_or(DecodeError::TooShort { len })?;
    let expected_lzss = comp_size - MIN_COMP_SIZE;
    if lzss_len != expected_lzss {
        return Err(DecodeError::LzssLenField {
            field: lzss_len,
            expected: expected_lzss,
        });
    }
    let decomp = u32_at(&buf, DECOMP_SIZE_OFFSET).ok_or(DecodeError::TooShort { len })?;
    if decomp > MAX_DECOMP_SIZE {
        return Err(DecodeError::DecompSizeTooLarge { size: decomp });
    }
    let src = buf
        .get(BODY_START..)
        .ok_or(DecodeError::CompSizeOutOfRange { comp_size, len })?;
    let body = unlzss(src, decomp as usize).map_err(DecodeError::Lzss)?;
    if body.len() != decomp as usize {
        return Err(DecodeError::DecompSizeMismatch {
            field: decomp,
            actual: body.len(),
        });
    }
    let mut image = buf;
    image.truncate(BODY_START);
    image.extend_from_slice(&body);
    Ok(image)
}

/// 30 フレームに 1 バイトの D の長さ（塊ごとに切り捨て）。
pub fn expected_fps_len(frames: usize) -> usize {
    (frames / CHUNK_FRAMES) * (CHUNK_FRAMES / FPS_EVERY) + (frames % CHUNK_FRAMES) / FPS_EVERY
}

fn side_info(block: &[u8]) -> Option<SideInfo> {
    Some(SideInfo {
        score: u32_at(block, 0x00)?,
        character: *block.get(0x06)?,
        human_cpu: *block.get(0x07)?,
        cpu_level: u16_at(block, 0x0A)?,
    })
}

fn words(block: &[u8]) -> Option<Vec<u16>> {
    let body = block.get(STAGE_HEADER_LEN..)?;
    Some(
        body.as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect(),
    )
}

/// 展開した本体から、枠ごとの入力を読む。ブロックは隙間なく並ぶので、各ブロックの終わりは「次に大きいポインタ」（最後は本体の終わり）。
pub fn read_inputs(image: &[u8]) -> Result<ReplayInputs, DecodeError> {
    if image.len() < BLOCKS_START {
        return Err(DecodeError::ImageTooShort { len: image.len() });
    }
    // ポインタを全部集める。A・B・C のブロックは必ず 0x20 バイト以上あるので本体の中を指す。D は 30 フレーム
    // 未満だと空になりうるので本体の終わりを指してもよく、空の D どうしは同じ位置を指しうる。
    let mut ptrs = [[0u32; SLOTS]; 4];
    let mut starts: Vec<(u32, usize)> = Vec::new();
    for (ai, base) in ARRAY_TABLE.iter().enumerate() {
        for (slot, cell) in ptrs[ai].iter_mut().enumerate() {
            let p = u32_at(image, base + 4 * slot)
                .ok_or(DecodeError::ImageTooShort { len: image.len() })?;
            if p == 0 {
                continue;
            }
            let limit = if ai == 3 {
                image.len()
            } else {
                image.len() - 1
            };
            if (p as usize) < BLOCKS_START || (p as usize) > limit {
                return Err(DecodeError::BadPointer {
                    array: ARRAY_NAMES[ai],
                    slot: slot as u8,
                    ptr: p,
                    len: image.len(),
                });
            }
            *cell = p;
            starts.push((p, ai));
        }
    }
    starts.sort_unstable();
    for w in starts.windows(2) {
        if w[0].0 == w[1].0 && (w[0].1 != 3 || w[1].1 != 3) {
            return Err(DecodeError::SharedPointer { ptr: w[0].0 });
        }
    }
    let block = |ai: usize, slot: usize| -> Option<&[u8]> {
        let p = ptrs[ai][slot];
        if p == 0 {
            return None;
        }
        // 同じ位置を指す D が後ろの枠にあれば、この D は空（ファイルの中の並びは枠の順）。
        if ai == 3 && ((slot + 1)..SLOTS).any(|s2| ptrs[3][s2] == p) {
            return image.get(p as usize..p as usize);
        }
        let end = starts
            .iter()
            .map(|&(s, _)| s)
            .find(|&s| s > p)
            .map_or(image.len(), |s| s as usize);
        image.get(p as usize..end)
    };

    let mut stages = Vec::new();
    for slot in 0..SLOTS {
        let (a, b, c) = (block(0, slot), block(1, slot), block(2, slot));
        if a.is_none() && b.is_none() && c.is_none() {
            continue;
        }
        let (Some(a), Some(b), Some(c)) = (a, b, c) else {
            return Err(DecodeError::MissingArray { slot: slot as u8 });
        };
        for (ai, blk) in [(0usize, a), (1, b), (2, c)] {
            if blk.len() < STAGE_HEADER_LEN || !(blk.len() - STAGE_HEADER_LEN).is_multiple_of(2) {
                return Err(DecodeError::BadBlockLen {
                    array: ARRAY_NAMES[ai],
                    slot: slot as u8,
                    len: blk.len(),
                });
            }
        }
        if a.len() != b.len() || a.len() != c.len() {
            return Err(DecodeError::FrameCountMismatch { slot: slot as u8 });
        }
        let bad = DecodeError::BadBlockLen {
            array: 'A',
            slot: slot as u8,
            len: a.len(),
        };
        let p1_inputs = words(a).ok_or(bad)?;
        let p2_inputs = words(b).ok_or(bad)?;
        let p3_inputs = words(c).ok_or(bad)?;
        let fps = block(3, slot).map(<[u8]>::to_vec).unwrap_or_default();
        let frames = p1_inputs.len();
        stages.push(StageInputs {
            slot: slot as u8,
            rng_seed: u16_at(a, 0x04).ok_or(bad)?,
            p1: side_info(a).ok_or(bad)?,
            p2: side_info(b).ok_or(bad)?,
            fps_len_ok: fps.len() == expected_fps_len(frames),
            p1_inputs,
            p2_inputs,
            p3_inputs,
            fps,
        });
    }
    Ok(ReplayInputs {
        difficulty: *image
            .get(DIFFICULTY_OFFSET)
            .ok_or(DecodeError::ImageTooShort { len: image.len() })?,
        game_mode: *image
            .get(GAME_MODE_OFFSET)
            .ok_or(DecodeError::ImageTooShort { len: image.len() })?,
        stages,
    })
}

pub fn decode_inputs(file: &[u8]) -> Result<ReplayInputs, DecodeError> {
    let image = decode_image(file)?;
    read_inputs(&image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_build::{BuildSpec, build_replay};

    struct XorShift(u32);
    impl XorShift {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    /// 「0」と「1」の文字列から MSB 先頭のバイト列を作る（端数は捨てる —— ゲームの書き方）。書き込み側とは共有しない。
    fn bytes_from_bits(s: &str) -> Vec<u8> {
        let bits: Vec<u8> = s
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| u8::from(c == '1'))
            .collect();
        bits.as_chunks::<8>()
            .0
            .iter()
            .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b))
            .collect()
    }

    #[test]
    fn decrypt_matches_hand_computed_vectors() {
        // 鍵 0x40: 平文 10 20 30 のとき、暗号は 10+40=50、20+47=67、30+4E=7E。
        let mut b = [0x50u8, 0x67, 0x7E];
        decrypt_in_place(&mut b, 0x40);
        assert_eq!(b, [0x10, 0x20, 0x30]);
        // 鍵が u8 で回る: 鍵 0xFC は 0xFC → 0x03（0xFC+7=0x103）→ 0x0A。平文 00 00 00 の暗号は FC 03 0A。
        let mut w = [0xFCu8, 0x03, 0x0A];
        decrypt_in_place(&mut w, 0xFC);
        assert_eq!(w, [0, 0, 0]);
        let mut u = [0x01u8];
        decrypt_in_place(&mut u, 0x40);
        assert_eq!(u, [0xC1]);
        let mut r: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        let orig = r.clone();
        encrypt_in_place(&mut r, 0x9B);
        assert_ne!(r, orig);
        decrypt_in_place(&mut r, 0x9B);
        assert_eq!(r, orig);
    }

    #[test]
    fn checksum_matches_hand_computed_vector() {
        let mut d = vec![0u8; 0x20];
        d[0x14] = 0xEE; // 範囲の外（0x15 の手前）
        d[0x15] = 0x03;
        d[0x16] = 0x00;
        d[0x17] = 0x00;
        d[0x18] = 0x10;
        d[0x1F] = 0x20; // comp_size = 0x20 なら範囲の外（0x20 の手前まで）
        d[0x10] = 0xFF; // 範囲の外（チェックサム自身）
        assert_eq!(checksum(&d, 0x1F), Some(0x3F00_0318 + 0x03 + 0x10));
        assert_eq!(checksum(&d, 0x20), Some(0x3F00_0318 + 0x03 + 0x10 + 0x20));
        assert_eq!(checksum(&d, 0x21), None);
        assert_eq!(checksum(&d, 0x14), None);
        // u32 で回る: 0xFF を 1,700 万個（総和 4.3e9 は u32 に収まらない）。期待値は u64 で計算して 2^32 で割った余り。
        let big = vec![0xFFu8; 0x15 + 17_000_000];
        let want = (u64::from(CHECKSUM_SEED) + 255 * 17_000_000) % (1u64 << 32);
        assert_eq!(checksum(&big, big.len()), Some(want as u32));
    }

    #[test]
    fn unlzss_hand_written_stream() {
        // 'A' → 1 01000001 / 'B' → 1 01000010（履歴の位置 1 = 'A'、2 = 'B'）
        // 一致: 0 ＋ 位置 1（0000000000001）＋ 長さ 7 − 3 = 4（0100）→ 位置 1 から 7 バイト = A B A B A B A（重なる一致）
        // 終わり: 0 ＋ 13 bit の 0。計 50 bit → 6 バイト（48 bit）で、最後の 2 bit は捨てる。
        let s = "101000001 101000010 0 0000000000001 0100 0 0000000000000";
        let data = bytes_from_bits(s);
        assert_eq!(data.len(), 6);
        assert_eq!(unlzss(&data, 100).unwrap(), b"ABABABABA".to_vec());
        assert!(unlzss(&data, 9).is_ok());
        assert_eq!(unlzss(&data, 8), Err(LzssError::Overflow { limit: 8 }));
    }

    #[test]
    fn unlzss_terminator_forms() {
        // 「空」: 印だけ（14 bit → 8 bit にそろう分は 1 バイト）。
        assert_eq!(unlzss(&[0x00], 10).unwrap(), Vec::<u8>::new());
        // 印（フラグ 1 bit ＋ 位置 13 bit）の後に 2 bit 余る（16 bit のうち 14 bit が印）→ 断る。
        assert_eq!(
            unlzss(&[0x00, 0x00], 10),
            Err(LzssError::TrailingBits { bits: 2 })
        );
        assert_eq!(
            unlzss(&[0x00, 0x00, 0x00], 10),
            Err(LzssError::TrailingBits { bits: 10 })
        );
        assert_eq!(unlzss(&[], 10), Err(LzssError::MissingTerminator));
        assert_eq!(
            unlzss(&bytes_from_bits("1 0100 0001 1 0101"), 10),
            Err(LzssError::Truncated)
        );
        // 位置が途中で切れていて 0 でない: フラグ 0 の後の 7 bit が 0000001。
        assert_eq!(unlzss(&[0b0000_0001], 10), Err(LzssError::Truncated));
    }

    #[test]
    fn lzss_round_trips_including_window_wrap() {
        use crate::replay_build::lzss_compress;
        let mut rng = XorShift(0xC0FF_EE11);
        let mut cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![7],
            vec![1, 2, 3],
            vec![0u8; 5000],
            (0..20_000u32).map(|i| (i % 251) as u8).collect(),
            (0..12_000u32).map(|_| rng.next() as u8).collect(),
        ];
        let noise: Vec<u8> = (0..8000u32).map(|_| rng.next() as u8).collect();
        let mut twice = noise.clone();
        twice.extend_from_slice(&noise);
        twice.extend_from_slice(&noise[..500]);
        cases.push(twice);
        let mut words = Vec::new();
        for _ in 0..3000 {
            let w = (rng.below(4) as u16) << 4 | u16::from(rng.below(3) == 0);
            for _ in 0..rng.below(40) {
                words.extend_from_slice(&w.to_le_bytes());
            }
        }
        cases.push(words);
        for (i, data) in cases.iter().enumerate() {
            let packed = lzss_compress(data);
            let back = unlzss(&packed, data.len()).unwrap_or_else(|e| panic!("{i}: {e:?}"));
            assert_eq!(&back, data, "{i}");
        }
    }

    fn sample_spec(frames: usize) -> BuildSpec {
        let mut rng = XorShift(0x1234_5678);
        let mut p1 = Vec::new();
        let mut p2 = Vec::new();
        for _ in 0..frames {
            p1.push(if rng.below(3) == 0 {
                rng.next() as u16 & 0x01F5
            } else {
                0x0001
            });
            p2.push(if rng.below(5) == 0 {
                rng.next() as u16 & 0x00F5
            } else {
                0
            });
        }
        BuildSpec::match_replay(0xA7, p1, p2)
    }

    #[test]
    fn container_round_trip_reads_back_the_inputs() {
        for frames in [0usize, 1, 29, 30, 3597, 3598, 3599, 8000] {
            let spec = sample_spec(frames);
            let file = build_replay(&spec);
            let got = decode_inputs(&file).unwrap_or_else(|e| panic!("{frames}: {e:?}"));
            assert_eq!(got.game_mode, 2);
            assert_eq!(got.stages.len(), 1);
            let st = &got.stages[0];
            assert_eq!(st.slot, 9);
            assert_eq!(st.p1_inputs, spec.stages[0].p1_inputs);
            assert_eq!(st.p2_inputs, spec.stages[0].p2_inputs);
            assert_eq!(st.frames(), frames);
            assert_eq!(st.p3_not_or_count(), 0);
            assert_eq!(st.rng_seed, spec.stages[0].rng_seed);
            assert_eq!(st.fps.len(), expected_fps_len(frames));
            assert!(st.fps_len_ok);
        }
        // 1 と 2 の取り違えを見つけるため、席ごとの情報も別の値で往復する。
        let spec = sample_spec(50);
        let got = decode_inputs(&build_replay(&spec)).unwrap();
        let (s, g) = (&spec.stages[0], &got.stages[0]);
        assert_eq!(g.p1.character, s.p1_character);
        assert_eq!(g.p2.character, s.p2_character);
        assert_ne!(g.p1.character, g.p2.character);
        assert_eq!(g.p2.human_cpu, s.p2_human_cpu);
        assert_eq!(g.p2.cpu_level, s.p2_cpu_level);
    }

    #[test]
    fn expected_fps_len_matches_hand_values() {
        // 3598 / 30 = 119 余り 28、7196 は塊 2 つ、3599 は塊 1 つ ＋ 1 フレーム。
        assert_eq!(expected_fps_len(0), 0);
        assert_eq!(expected_fps_len(29), 0);
        assert_eq!(expected_fps_len(30), 1);
        assert_eq!(expected_fps_len(3597), 119);
        assert_eq!(expected_fps_len(3598), 119);
        assert_eq!(expected_fps_len(3599), 119);
        assert_eq!(expected_fps_len(3598 + 30), 120);
        assert_eq!(expected_fps_len(7196), 238);
    }

    #[test]
    fn hand_assembled_file_decodes() {
        // 本体（`0xC0` 以降）: 0x1EC − 0xC0 = 0x12C バイトの見出し部 ＋ A・B・C 各 0x20 + 6 ＋ D は無し。
        let a_ptr = 0x1ECu32;
        let blk = 0x20 + 6;
        let mut body = vec![0u8; 0x12C];
        body[0xD7 - 0xC0] = 3; // 難易度
        body[0x1E4 - 0xC0] = 2; // game_mode = Match
        let mut a = vec![0u8; blk];
        a[0x04] = 0x34;
        a[0x05] = 0x12; // rng_seed 0x1234
        a[0x06] = 5; // キャラ
        a[0x20..].copy_from_slice(&[0x01, 0x00, 0x11, 0x00, 0x21, 0x00]);
        let mut b = vec![0u8; blk];
        b[0x06] = 6;
        b[0x20..].copy_from_slice(&[0x00, 0x00, 0x10, 0x00, 0x04, 0x00]);
        let c: Vec<u8> = {
            let mut c = vec![0u8; blk];
            c[0x04] = 0x34;
            c[0x05] = 0x12;
            c[0x20..].copy_from_slice(&[0x01, 0x00, 0x11, 0x00, 0x25, 0x00]);
            c
        };
        body.extend_from_slice(&a);
        body.extend_from_slice(&b);
        body.extend_from_slice(&c);
        // LZSS: 全部リテラル（1 + 8 bit）＋ 終わり（0 + 13 bit の 0）。端数は捨てる。
        let mut bit_string = String::new();
        for byte in &body {
            bit_string.push('1');
            bit_string.push_str(&format!("{byte:08b}"));
        }
        bit_string.push_str("0 0000000000000");
        let lzss = bytes_from_bits(&bit_string);
        let comp_size = 0xC0 + lzss.len();
        let mut plain = vec![0u8; 0xC0];
        plain[0..4].copy_from_slice(b"T9RP");
        plain[4] = 2;
        plain[7] = 1;
        plain[0x0C..0x10].copy_from_slice(&(comp_size as u32).to_le_bytes());
        plain[0x14] = 0x99; // 乱数のゴミ
        plain[0x15] = 0x55; // 鍵
        plain[0x18..0x1C].copy_from_slice(&(lzss.len() as u32).to_le_bytes());
        plain[0x1C..0x20].copy_from_slice(&(body.len() as u32).to_le_bytes());
        // 配列 A の枠 9、B の枠 9、C の枠 9（`0x20 + 4 * 9`、`0x48 + 4 * 9`、`0x70 + 4 * 9`）。
        plain[0x20 + 36..0x20 + 40].copy_from_slice(&a_ptr.to_le_bytes());
        plain[0x48 + 36..0x48 + 40].copy_from_slice(&(a_ptr + blk as u32).to_le_bytes());
        plain[0x70 + 36..0x70 + 40].copy_from_slice(&(a_ptr + 2 * blk as u32).to_le_bytes());
        plain.extend_from_slice(&lzss);
        let sum: u32 = plain[0x15..comp_size]
            .iter()
            .fold(0x3F00_0318u32, |s, &x| s + u32::from(x));
        plain[0x10..0x14].copy_from_slice(&sum.to_le_bytes());
        // 暗号化: [0x18, comp_size) の i 番目に 鍵 + 7 * i を足す（手で書く）。
        let mut file = plain.clone();
        for (i, x) in file[0x18..comp_size].iter_mut().enumerate() {
            *x = x.wrapping_add(0x55u8.wrapping_add((7 * i) as u8));
        }
        file.extend_from_slice(b"USER");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&[0, 0, 0, 0]);
        file.extend_from_slice(&[b'x', 0, 0, 0]);

        let got = decode_inputs(&file).unwrap();
        assert_eq!((got.difficulty, got.game_mode), (3, 2));
        let st = &got.stages[0];
        assert_eq!(st.slot, 9);
        assert_eq!(st.rng_seed, 0x1234);
        assert_eq!((st.p1.character, st.p2.character), (5, 6));
        assert_eq!(st.p1_inputs, vec![0x0001, 0x0011, 0x0021]);
        assert_eq!(st.p2_inputs, vec![0x0000, 0x0010, 0x0004]);
        assert_eq!(st.p3_inputs, vec![0x0001, 0x0011, 0x0025]);
        assert_eq!(st.p3_not_or_count(), 0);
        assert!(st.fps.is_empty() && st.fps_len_ok);
    }

    #[test]
    fn poisoned_files_are_refused_with_their_own_reason() {
        let good = build_replay(&sample_spec(100));
        assert!(decode_inputs(&good).is_ok());
        let comp = u32::from_le_bytes(good[12..16].try_into().unwrap()) as usize;

        assert_eq!(
            decode_inputs(&good[..0x17]),
            Err(DecodeError::TooShort { len: 0x17 })
        );
        assert_eq!(decode_inputs(&[]), Err(DecodeError::TooShort { len: 0 }));
        let mut f = good.clone();
        f[0] = b'X';
        assert_eq!(decode_inputs(&f), Err(DecodeError::BadMagic));
        let mut f = good.clone();
        f[4] = 3;
        assert_eq!(
            decode_inputs(&f),
            Err(DecodeError::BadVersion { version: 3 })
        );
        let mut f = good.clone();
        f[6] = 1;
        assert_eq!(
            decode_inputs(&f),
            Err(DecodeError::BadReserved { value: 1 })
        );
        let mut f = good.clone();
        f[12..16].copy_from_slice(&0xBFu32.to_le_bytes());
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::CompSizeOutOfRange { .. })
        ));
        f[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::CompSizeOutOfRange { .. })
        ));
        let mut f = good.clone();
        f[0x100] ^= 0x01;
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::ChecksumMismatch { .. })
        ));
        let mut f = good.clone();
        f[0x10] ^= 0x01;
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::ChecksumMismatch { .. })
        ));
        let mut f = good.clone();
        f[0x15] = f[0x15].wrapping_add(1);
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::ChecksumMismatch { .. })
        ));
        // USER チャンクを壊してもこの入口は通る（範囲の外）。
        let mut f = good.clone();
        let n = f.len();
        f[n - 3] ^= 0xFF;
        assert!(decode_inputs(&f).is_ok());
        assert!(matches!(
            decode_inputs(&good[..comp - 1]),
            Err(DecodeError::CompSizeOutOfRange { .. })
        ));

        // 中身を壊して、チェックサムを合わせ直す（暗号化 → 復号の内側を狙う毒）。
        let repack = |edit: &dyn Fn(&mut Vec<u8>)| -> Vec<u8> {
            let mut plain = good[..comp].to_vec();
            let key = plain[0x15];
            decrypt_in_place(&mut plain[0x18..], key);
            edit(&mut plain);
            let cs = checksum(&plain, plain.len()).unwrap();
            plain[0x10..0x14].copy_from_slice(&cs.to_le_bytes());
            encrypt_in_place(&mut plain[0x18..], key);
            plain
        };
        let f = repack(&|p| p[0x18] ^= 0x01);
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::LzssLenField { .. })
        ));
        let f = repack(&|p| p[0x1C..0x20].copy_from_slice(&u32::MAX.to_le_bytes()));
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::DecompSizeTooLarge { .. })
        ));
        let f = repack(&|p| p[0x1C..0x20].copy_from_slice(&10u32.to_le_bytes()));
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::Lzss(LzssError::Overflow { .. }))
        ));
        let f = repack(&|p| {
            let d = u32::from_le_bytes(p[0x1C..0x20].try_into().unwrap());
            p[0x1C..0x20].copy_from_slice(&(d + 1).to_le_bytes());
        });
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::DecompSizeMismatch { .. })
        ));
        let f = repack(&|p| {
            p.truncate(p.len() - 3);
            let n = (p.len() - 0xC0) as u32;
            p[0x18..0x1C].copy_from_slice(&n.to_le_bytes());
            let total = p.len() as u32;
            p[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
        });
        assert!(matches!(decode_inputs(&f), Err(DecodeError::Lzss(_))));
        let f = repack(&|p| {
            p.extend_from_slice(&[0, 0, 0]);
            let n = (p.len() - 0xC0) as u32;
            p[0x18..0x1C].copy_from_slice(&n.to_le_bytes());
            let total = p.len() as u32;
            p[0x0C..0x10].copy_from_slice(&total.to_le_bytes());
        });
        assert!(matches!(
            decode_inputs(&f),
            Err(DecodeError::Lzss(LzssError::TrailingBits { .. }))
        ));
    }

    #[test]
    fn poisoned_images_are_refused_in_the_array_layer() {
        let good = build_replay(&sample_spec(40));
        let image = decode_image(&good).unwrap();
        assert!(read_inputs(&image).is_ok());
        let ptr_a9 = 0x20 + 36;
        let ptr_b9 = 0x48 + 36;
        let ptr_c9 = 0x70 + 36;

        assert!(matches!(
            read_inputs(&image[..0x1EB]),
            Err(DecodeError::ImageTooShort { .. })
        ));
        let mut im = image.clone();
        im[ptr_a9..ptr_a9 + 4].copy_from_slice(&0x100u32.to_le_bytes());
        assert!(matches!(
            read_inputs(&im),
            Err(DecodeError::BadPointer {
                array: 'A',
                slot: 9,
                ..
            })
        ));
        im[ptr_a9..ptr_a9 + 4].copy_from_slice(&(image.len() as u32).to_le_bytes());
        assert!(matches!(
            read_inputs(&im),
            Err(DecodeError::BadPointer { .. })
        ));
        let mut im = image.clone();
        let a = &image[ptr_a9..ptr_a9 + 4].to_vec();
        im[ptr_b9..ptr_b9 + 4].copy_from_slice(a);
        assert!(matches!(
            read_inputs(&im),
            Err(DecodeError::SharedPointer { .. })
        ));
        let mut im = image.clone();
        im[ptr_c9..ptr_c9 + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(read_inputs(&im), Err(DecodeError::MissingArray { slot: 9 }));
        // D を丸ごと落とす: D の表はまだ残っているが、指す先が本体の終わりなので空の D として読める。
        let d9 = u32::from_le_bytes(image[0x98 + 36..0x98 + 40].try_into().unwrap()) as usize;
        let mut im = image.clone();
        im.truncate(d9);
        let r = read_inputs(&im).unwrap();
        assert!(r.stages[0].fps.is_empty());
        assert!(!r.stages[0].fps_len_ok);
        im.truncate(d9 - 1);
        assert!(matches!(
            read_inputs(&im),
            Err(DecodeError::BadPointer { array: 'D', .. })
        ));
        let mut im = image.clone();
        im[0x98 + 36..0x98 + 40].copy_from_slice(&(d9 as u32 + 1).to_le_bytes());
        assert!(matches!(
            read_inputs(&im),
            Err(DecodeError::BadBlockLen {
                array: 'C',
                slot: 9,
                ..
            })
        ));
    }

    #[test]
    fn random_bytes_never_panic() {
        // 半分は正しい形の合成リプレイを何バイトか壊したもので、奥まで届いた本数が 0 でないことも見る（全部が入口で断られると測れていない）。
        let mut rng = XorShift(0x9E37_79B9);
        let (mut ok, mut reached_lzss, mut reached_arrays) = (0u32, 0u32, 0u32);
        let base = build_replay(&sample_spec(200));
        for i in 0..3000u32 {
            let len = rng.below(600) as usize;
            let mut f: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if i % 2 == 0 {
                f = base.clone();
                for _ in 0..rng.below(4) {
                    let at = rng.below(f.len() as u32) as usize;
                    f[at] = rng.next() as u8;
                }
                if rng.below(4) == 0 {
                    let cut = rng.below(f.len() as u32 + 1) as usize;
                    f.truncate(cut);
                }
            }
            match decode_inputs(&f) {
                Ok(_) => ok += 1,
                Err(DecodeError::Lzss(_)) | Err(DecodeError::DecompSizeMismatch { .. }) => {
                    reached_lzss += 1;
                }
                Err(
                    DecodeError::BadPointer { .. }
                    | DecodeError::SharedPointer { .. }
                    | DecodeError::MissingArray { .. }
                    | DecodeError::BadBlockLen { .. }
                    | DecodeError::FrameCountMismatch { .. },
                ) => reached_arrays += 1,
                Err(_) => {}
            }
            // 壊した後にチェックサムだけ合わせ直すと奥まで届く。
            if i % 2 == 0 && f.len() > 0x30 {
                let comp = u32::from_le_bytes([f[12], f[13], f[14], f[15]]) as usize;
                if (0xC0..=f.len()).contains(&comp) {
                    let key = f[0x15];
                    let mut plain = f[..comp].to_vec();
                    decrypt_in_place(&mut plain[0x18..], key);
                    for _ in 0..1 + rng.below(3) {
                        let at = 0x18 + rng.below((comp - 0x18) as u32) as usize;
                        plain[at] = rng.next() as u8;
                    }
                    if let Some(cs) = checksum(&plain, comp) {
                        plain[0x10..0x14].copy_from_slice(&cs.to_le_bytes());
                        encrypt_in_place(&mut plain[0x18..], key);
                        match decode_inputs(&plain) {
                            Ok(_) => ok += 1,
                            Err(DecodeError::Lzss(_))
                            | Err(DecodeError::DecompSizeMismatch { .. }) => reached_lzss += 1,
                            Err(
                                DecodeError::BadPointer { .. }
                                | DecodeError::SharedPointer { .. }
                                | DecodeError::MissingArray { .. }
                                | DecodeError::BadBlockLen { .. }
                                | DecodeError::FrameCountMismatch { .. },
                            ) => reached_arrays += 1,
                            Err(_) => {}
                        }
                    }
                }
            }
        }
        assert!(ok > 10, "通った本数 {ok}");
        assert!(
            reached_lzss > 10,
            "展開まで届いて断られた本数 {reached_lzss}"
        );
        assert!(
            reached_arrays > 10,
            "配列まで届いて断られた本数 {reached_arrays}"
        );
    }
}
