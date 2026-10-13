//! 署名の照合と detour の組み立て（pure）。
//!
//! フック地点は「アドレス・期待バイト列・命令の境目」を持ち、逆アセンブラを持たずに「長さの和が期待バイト列と同じ」
//! 「5 バイト以上」「どの命令も相対の分岐・呼び出しでない」「禁止の範囲と重ならない」を確かめる。
//! 実物の地点の表はここに置かない（アドレスの定数は `src/dll/th09_addr.rs`）。ゲームのメモリへ書く処理は `src/dll/` の
//! unsafe な層で、ここは書くべきバイト列を組み立てるだけ。

/// 地点 1 つぶんの記述。`expected` の長さと `instr_lens` の和は一致しなければならない（`check_site` が確かめる）。
pub struct Site {
    pub name: &'static str,
    pub addr: u32,
    pub expected: &'static [u8],
    /// `expected` を先頭から命令ごとに割った長さの列（バイト単位）。
    pub instr_lens: &'static [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrRange {
    pub start: u32,
    pub end: u32,
}

impl AddrRange {
    /// 1 バイトでも重なるか（半開区間なので隣接は重ならない）。
    pub fn overlaps(&self, other: &AddrRange) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteError {
    /// `instr_lens` の和が `expected` の長さと違う。
    LenSumMismatch { sum: usize, expected_len: usize },
    /// `expected` が 5 バイト未満（`E9 rel32` が入らない）。
    TooShort { len: usize },
    /// `instr_index` 番目の命令が相対（call/jmp/jcc rel）。
    RelativeInstruction { instr_index: usize, addr: u32 },
    /// 禁止の範囲と重なる。
    Forbidden { range: AddrRange },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisjointError {
    pub a: &'static str,
    pub b: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    LengthMismatch {
        actual_len: usize,
        expected_len: usize,
    },
    ByteMismatch {
        offset: usize,
        expected: u8,
        actual: u8,
    },
}

/// x86 の legacy 接頭辞。命令の相対／非相対はオペコードで決まるので、判定の前に飛ばす。
const PREFIXES: [u8; 11] = [
    0x66, 0x67, 0xF2, 0xF3, 0x2E, 0x36, 0x3E, 0x26, 0x64, 0x65, 0xF0,
];

/// 相対分岐・相対 call か。対象: `E8` / `E9` / `EB` / `70..=7F`（jcc rel8）/ `E0..=E3`（loop・jcxz rel8）/ `0F 80..=8F`（jcc rel32）。
fn is_relative_instr(instr: &[u8]) -> bool {
    let mut i = 0usize;
    while i < instr.len() && PREFIXES.contains(&instr[i]) {
        i += 1;
    }
    let Some(&opcode) = instr.get(i) else {
        return false; // 接頭辞だけで中身が無い（壊れた命令長。相対とは判定しない）
    };
    match opcode {
        0xE8 | 0xE9 | 0xEB => true,
        0x70..=0x7F => true,
        0xE0..=0xE3 => true,
        0x0F => matches!(instr.get(i + 1), Some(0x80..=0x8F)),
        _ => false,
    }
}

/// 地点 1 つの照合。`forbidden` は重なってはいけない範囲（vpatch のパッチ地点・他のフック地点など）。
pub fn check_site(site: &Site, forbidden: &[AddrRange]) -> Result<(), SiteError> {
    let sum: usize = site.instr_lens.iter().map(|&l| l as usize).sum();
    if sum != site.expected.len() {
        return Err(SiteError::LenSumMismatch {
            sum,
            expected_len: site.expected.len(),
        });
    }
    if site.expected.len() < 5 {
        return Err(SiteError::TooShort {
            len: site.expected.len(),
        });
    }
    let mut offset = 0usize;
    for (idx, &len) in site.instr_lens.iter().enumerate() {
        let len = len as usize;
        let instr = &site.expected[offset..offset + len];
        if is_relative_instr(instr) {
            return Err(SiteError::RelativeInstruction {
                instr_index: idx,
                addr: site.addr.wrapping_add(offset as u32),
            });
        }
        offset += len;
    }
    let site_range = AddrRange {
        start: site.addr,
        end: site.addr.wrapping_add(site.expected.len() as u32),
    };
    for r in forbidden {
        if site_range.overlaps(r) {
            return Err(SiteError::Forbidden { range: *r });
        }
    }
    Ok(())
}

/// 地点どうしが重ならないか（`O(n^2)` だが地点は数十）。
pub fn check_disjoint(sites: &[&Site]) -> Result<(), DisjointError> {
    for i in 0..sites.len() {
        let a_range = AddrRange {
            start: sites[i].addr,
            end: sites[i].addr.wrapping_add(sites[i].expected.len() as u32),
        };
        for site_b in &sites[i + 1..] {
            let b_range = AddrRange {
                start: site_b.addr,
                end: site_b.addr.wrapping_add(site_b.expected.len() as u32),
            };
            if a_range.overlaps(&b_range) {
                return Err(DisjointError {
                    a: sites[i].name,
                    b: site_b.name,
                });
            }
        }
    }
    Ok(())
}

/// 実測のバイト列と期待バイト列を比べる。長さ違いも `Err`。
pub fn verify(actual: &[u8], expected: &[u8]) -> Result<(), VerifyError> {
    if actual.len() != expected.len() {
        return Err(VerifyError::LengthMismatch {
            actual_len: actual.len(),
            expected_len: expected.len(),
        });
    }
    for (offset, (&a, &e)) in actual.iter().zip(expected.iter()).enumerate() {
        if a != e {
            return Err(VerifyError::ByteMismatch {
                offset,
                expected: e,
                actual: a,
            });
        }
    }
    Ok(())
}

/// `at` に置く `E9 rel32`（jmp）。rel32 は `to - (at + 5)`（wrapping）。
pub fn jmp_rel32(at: u32, to: u32) -> [u8; 5] {
    encode_rel32(0xE9, at, to)
}

pub fn call_rel32(at: u32, to: u32) -> [u8; 5] {
    encode_rel32(0xE8, at, to)
}

fn encode_rel32(opcode: u8, at: u32, to: u32) -> [u8; 5] {
    let next = at.wrapping_add(5);
    let rel = to.wrapping_sub(next);
    let b = rel.to_le_bytes();
    [opcode, b[0], b[1], b[2], b[3]]
}

/// `at` に置かれた `E8 rel32` の飛び先。先頭が `E8` でなければ `None`。
pub fn call_target(at: u32, bytes: &[u8; 5]) -> Option<u32> {
    if bytes[0] != 0xE8 {
        return None;
    }
    let rel = i32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    // rel を u32 で wrapping 加算すれば、符号付きの加算と同じ結果になる。
    Some(at.wrapping_add(5).wrapping_add(rel as u32))
}

/// `at` に置かれた `E9 rel32` の飛び先（`call_target` の JMP バージョン）。先頭が `E9` でなければ `None`。
pub fn jmp_target(at: u32, bytes: &[u8; 5]) -> Option<u32> {
    if bytes[0] != 0xE9 {
        return None;
    }
    let rel = i32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    Some(at.wrapping_add(5).wrapping_add(rel as u32))
}

/// 5 バイト JMP を置くときの最大の書き込み長。
pub const MAX_PATCH_LEN: usize = 16;
/// トランポリン（盗んだバイト ＋ 戻りの JMP）の最大長。
pub const MAX_TRAMPOLINE_LEN: usize = 32;

/// 固定長のバッファと有効長。ヒープを使わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchBuf {
    pub bytes: [u8; MAX_PATCH_LEN],
    pub len: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TrampolineBuf {
    pub bytes: [u8; MAX_TRAMPOLINE_LEN],
    pub len: usize,
}

/// `site.addr` に書くバイト列: `E9 rel32`（`hook` へ）＋残りを `0x90` で埋める。長さが `5..=MAX_PATCH_LEN` でなければ `None`。
pub fn patch_bytes(site: &Site, hook: u32) -> Option<PatchBuf> {
    let total = site.expected.len();
    if !(5..=MAX_PATCH_LEN).contains(&total) {
        return None;
    }
    let mut bytes = [0x90u8; MAX_PATCH_LEN];
    let jmp = jmp_rel32(site.addr, hook);
    bytes[0..5].copy_from_slice(&jmp);
    Some(PatchBuf { bytes, len: total })
}

/// `tramp_at` に置くトランポリン: 盗んだバイト（`site.expected`）＋戻りの `E9 rel32`（`site.addr + 盗んだ長さ` へ）。
/// 合計が `MAX_TRAMPOLINE_LEN` を超えるなら `None`。
pub fn trampoline_bytes(site: &Site, tramp_at: u32) -> Option<TrampolineBuf> {
    let stolen_len = site.expected.len();
    let total = stolen_len + 5;
    if total > MAX_TRAMPOLINE_LEN {
        return None;
    }
    let mut bytes = [0u8; MAX_TRAMPOLINE_LEN];
    bytes[0..stolen_len].copy_from_slice(site.expected);
    let back_at = tramp_at.wrapping_add(stolen_len as u32);
    let back_to = site.addr.wrapping_add(stolen_len as u32);
    let back = jmp_rel32(back_at, back_to);
    bytes[stolen_len..stolen_len + 5].copy_from_slice(&back);
    Some(TrampolineBuf { bytes, len: total })
}

#[cfg(test)]
mod tests {
    //! 期待値の rel32 は手で計算した値（`jmp_rel32` / `call_rel32` から作らない）。実物の `0x0043112D` の
    //! `E8 8E D8 FF FF` → `0x0042E9C0` も使う。
    use super::*;

    #[test]
    fn verify_matches_identical_bytes() {
        assert_eq!(verify(&[1, 2, 3], &[1, 2, 3]), Ok(()));
    }

    #[test]
    fn verify_rejects_length_mismatch() {
        assert_eq!(
            verify(&[1, 2, 3], &[1, 2, 3, 4]),
            Err(VerifyError::LengthMismatch {
                actual_len: 3,
                expected_len: 4
            })
        );
    }

    #[test]
    fn verify_rejects_single_byte_mismatch() {
        assert_eq!(
            verify(&[1, 2, 9], &[1, 2, 3]),
            Err(VerifyError::ByteMismatch {
                offset: 2,
                expected: 3,
                actual: 9
            })
        );
    }

    #[test]
    fn call_target_matches_known_real_site() {
        let bytes = [0xE8, 0x8E, 0xD8, 0xFF, 0xFF];
        assert_eq!(call_target(0x0043112D, &bytes), Some(0x0042E9C0));
    }

    #[test]
    fn call_rel32_reproduces_known_real_bytes() {
        assert_eq!(
            call_rel32(0x0043112D, 0x0042E9C0),
            [0xE8, 0x8E, 0xD8, 0xFF, 0xFF]
        );
    }

    #[test]
    fn call_target_none_when_not_e8() {
        let bytes = [0xE9, 0x8E, 0xD8, 0xFF, 0xFF];
        assert_eq!(call_target(0x0043112D, &bytes), None);
    }

    #[test]
    fn jmp_rel32_forward_hand_computed() {
        assert_eq!(
            jmp_rel32(0x00401000, 0x00401010),
            [0xE9, 0x0B, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn jmp_rel32_backward_hand_computed() {
        assert_eq!(
            jmp_rel32(0x00401020, 0x00401000),
            [0xE9, 0xDB, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn jmp_rel32_crossing_0x10000000_hand_computed() {
        assert_eq!(
            jmp_rel32(0x000FFFF0, 0x10000010),
            [0xE9, 0x1B, 0x00, 0xF0, 0x0F]
        );
        // call_target は E8 用だが rel32 の復元は同じなので、先頭バイトだけ E8 に差し替えて使う。
        let mut b = jmp_rel32(0x000FFFF0, 0x10000010);
        b[0] = 0xE8;
        assert_eq!(call_target(0x000FFFF0, &b), Some(0x10000010));
    }

    // 合成の非相対命令列（5 バイト）。
    const BENIGN_5: &[u8] = &[0x55, 0x8B, 0xEC, 0x51, 0x90]; // push ebp; mov ebp,esp; push ecx; nop
    const BENIGN_5_LENS: &[u8] = &[1, 2, 1, 1];

    fn benign_site() -> Site {
        Site {
            name: "synthetic.benign",
            addr: 0x0060_1000,
            expected: BENIGN_5,
            instr_lens: BENIGN_5_LENS,
        }
    }

    #[test]
    fn check_site_accepts_benign_non_relative_instructions() {
        assert_eq!(check_site(&benign_site(), &[]), Ok(()));
    }

    #[test]
    fn check_site_accepts_adjacent_forbidden_range() {
        let site = benign_site(); // [0x00601000, 0x00601005)
        let forbidden = [AddrRange {
            start: 0x0060_1005,
            end: 0x0060_1010,
        }];
        assert_eq!(check_site(&site, &forbidden), Ok(()));
    }

    #[test]
    fn check_site_rejects_len_sum_mismatch() {
        let site = Site {
            name: "synthetic.len_mismatch",
            addr: 0x0060_1000,
            expected: BENIGN_5,
            instr_lens: &[1, 2, 1], // 合計4 != 5
        };
        assert_eq!(
            check_site(&site, &[]),
            Err(SiteError::LenSumMismatch {
                sum: 4,
                expected_len: 5
            })
        );
    }

    #[test]
    fn check_site_rejects_too_short() {
        let site = Site {
            name: "synthetic.too_short",
            addr: 0x0060_1000,
            expected: &[0x90, 0x90, 0x90, 0x90], // 4 バイト
            instr_lens: &[1, 1, 1, 1],
        };
        assert_eq!(check_site(&site, &[]), Err(SiteError::TooShort { len: 4 }));
    }

    #[test]
    fn check_site_rejects_call_rel32() {
        let site = Site {
            name: "synthetic.call",
            addr: 0x0060_1000,
            expected: &[0xE8, 0x00, 0x00, 0x00, 0x00],
            instr_lens: &[5],
        };
        assert_eq!(
            check_site(&site, &[]),
            Err(SiteError::RelativeInstruction {
                instr_index: 0,
                addr: 0x0060_1000
            })
        );
    }

    #[test]
    fn check_site_rejects_jmp_rel8() {
        let site = Site {
            name: "synthetic.jmp_short",
            addr: 0x0060_1000,
            expected: &[0x90, 0x90, 0x90, 0xEB, 0x00, 0x90],
            instr_lens: &[1, 1, 1, 2, 1],
        };
        assert_eq!(
            check_site(&site, &[]),
            Err(SiteError::RelativeInstruction {
                instr_index: 3,
                addr: 0x0060_1003
            })
        );
    }

    #[test]
    fn check_site_rejects_jcc_near_0f8x() {
        let site = Site {
            name: "synthetic.jcc_near",
            addr: 0x0060_1000,
            expected: &[0x90, 0x0F, 0x84, 0x00, 0x00, 0x00, 0x00],
            instr_lens: &[1, 6],
        };
        assert_eq!(
            check_site(&site, &[]),
            Err(SiteError::RelativeInstruction {
                instr_index: 1,
                addr: 0x0060_1001
            })
        );
    }

    #[test]
    fn check_site_rejects_prefixed_jmp_rel32() {
        let site = Site {
            name: "synthetic.prefixed_jmp",
            addr: 0x0060_1000,
            expected: &[0x90, 0x66, 0xE9, 0x00, 0x00, 0x00, 0x00],
            instr_lens: &[1, 6],
        };
        assert_eq!(
            check_site(&site, &[]),
            Err(SiteError::RelativeInstruction {
                instr_index: 1,
                addr: 0x0060_1001
            })
        );
    }

    #[test]
    fn check_site_rejects_forbidden_overlap_by_one_byte() {
        let site = benign_site(); // [0x00601000, 0x00601005)
        // 禁止の範囲が地点の最後の1バイトとだけ重なる: [0x00601004, 0x00601010)
        let forbidden = [AddrRange {
            start: 0x0060_1004,
            end: 0x0060_1010,
        }];
        assert_eq!(
            check_site(&site, &forbidden),
            Err(SiteError::Forbidden {
                range: forbidden[0]
            })
        );
    }

    #[test]
    fn check_disjoint_accepts_non_overlapping_sites() {
        let a = benign_site(); // [0x00601000, 0x00601005)
        let b = Site {
            name: "synthetic.b",
            addr: 0x0060_1005,
            expected: BENIGN_5,
            instr_lens: BENIGN_5_LENS,
        };
        assert_eq!(check_disjoint(&[&a, &b]), Ok(()));
    }

    #[test]
    fn check_disjoint_rejects_overlapping_sites() {
        let a = benign_site(); // [0x00601000, 0x00601005)
        let b = Site {
            name: "synthetic.b_overlap",
            addr: 0x0060_1004, // 1バイトだけ重なる
            expected: BENIGN_5,
            instr_lens: BENIGN_5_LENS,
        };
        assert_eq!(
            check_disjoint(&[&a, &b]),
            Err(DisjointError {
                a: "synthetic.benign",
                b: "synthetic.b_overlap"
            })
        );
    }

    #[test]
    fn patch_bytes_writes_jmp_and_pads_with_nop() {
        let site = benign_site(); // 5 バイト
        let buf = patch_bytes(&site, 0x0070_0000).expect("5 bytes is patchable");
        assert_eq!(buf.len, 5);
        let expected_jmp = jmp_rel32(0x0060_1000, 0x0070_0000);
        assert_eq!(&buf.bytes[0..5], &expected_jmp);
    }

    #[test]
    fn patch_bytes_pads_remaining_bytes_with_nop_when_site_longer_than_5() {
        let site = Site {
            name: "synthetic.pad",
            addr: 0x0060_1000,
            expected: &[0x90; 8],
            instr_lens: &[1, 1, 1, 1, 1, 1, 1, 1],
        };
        let buf = patch_bytes(&site, 0x0070_0000).expect("8 bytes is patchable");
        assert_eq!(buf.len, 8);
        assert_eq!(buf.bytes[5], 0x90);
        assert_eq!(buf.bytes[6], 0x90);
        assert_eq!(buf.bytes[7], 0x90);
    }

    #[test]
    fn patch_bytes_none_when_site_too_short() {
        let site = Site {
            name: "synthetic.too_short_for_patch",
            addr: 0x0060_1000,
            expected: &[0x90, 0x90, 0x90, 0x90],
            instr_lens: &[1, 1, 1, 1],
        };
        assert_eq!(patch_bytes(&site, 0x0070_0000), None);
    }

    #[test]
    fn jmp_target_reads_back_hand_computed_jumps() {
        // 前向き: at=0x00401000、rel=0x0000000B → 0x00401010。
        assert_eq!(
            jmp_target(0x0040_1000, &[0xE9, 0x0B, 0x00, 0x00, 0x00]),
            Some(0x0040_1010)
        );
        // 後ろ向き: at=0x00401020、rel=0xFFFFFFDB（-0x25）→ 0x00401000。
        assert_eq!(
            jmp_target(0x0040_1020, &[0xE9, 0xDB, 0xFF, 0xFF, 0xFF]),
            Some(0x0040_1000)
        );
        assert_eq!(jmp_target(0x0040_1000, &[0xE8, 0x0B, 0, 0, 0]), None);
        assert_eq!(jmp_target(0x0040_1000, &[0xEB, 0x0B, 0, 0, 0]), None);
    }

    #[test]
    fn trampoline_bytes_copies_stolen_bytes_and_jumps_back() {
        let site = benign_site(); // expected = BENIGN_5, addr = 0x00601000
        let buf = trampoline_bytes(&site, 0x0080_0000).expect("fits in MAX_TRAMPOLINE_LEN");
        assert_eq!(buf.len, 5 + 5);
        assert_eq!(&buf.bytes[0..5], BENIGN_5);
        let expected_back = jmp_rel32(0x0080_0005, 0x0060_1005);
        assert_eq!(&buf.bytes[5..10], &expected_back);
    }
}
