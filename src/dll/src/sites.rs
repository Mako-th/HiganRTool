//! 地点の表の型と、表そのものの検査・実物のバイト列の判定（pure。unsafe 無し）。
//!
//! 実物の値（アドレス・期待バイト列）は `th09_addr.rs` にだけ置く。ここは「表をどう確かめるか」だけで、照合の芯は
//! `higanr_core::patch`（`check_site` / `check_disjoint` / `verify`）を呼ぶ。
//!
//! `check_site` は「相対の分岐を含む地点」を断るので、`E8 rel32` そのものを照合する call 先の差し替えの地点や、命令の途中で
//! 切れる関数の入口の 8 バイトに当てると必ず落ちる。そこで地点を使い方で 3 つに分け、`check_site` を当てるのは inline detour
//! で書く地点だけにする。他は「call 先の差し替え」の規則（`E8` と元の飛び先）と、禁止の範囲・地点どうしの重なりで確かめる。
#![forbid(unsafe_code)]

use higanr_core::patch::{self, AddrRange, DisjointError, Site, SiteError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// inline detour（5 バイト JMP ＋ トランポリン）で書く予定の地点。`check_site`
    /// （命令の境目の和・5 バイト以上・相対の分岐なし・禁止の範囲）を通す。
    Detour { instr_lens: &'static [u8] },
    /// call 先の差し替え。`addr` は **オペコード `E8` の位置**で、期待バイト列は `E8 rel32` の 5 バイト。書き換えるのは
    /// rel32（`addr + 1` から 4 バイト）だけ。`original_target` は rel32 から計算した元の飛び先がこれと一致することを確かめる値。
    CallRel32 { original_target: u32 },
    /// 書かない。照合だけ（観測・戻りアドレス・呼ぶ前の関数の入口）。`instr_lens` は期待バイト列の末尾が命令の境目で終わるときだけ
    /// 持つ（関数の入口の先頭 8 バイトは命令の途中で切れるので `None`）。
    VerifyOnly { instr_lens: Option<&'static [u8]> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiteDef {
    pub name: &'static str,
    /// image base `0x400000` を前提にした VA。
    pub addr: u32,
    pub expected: &'static [u8],
    pub kind: Use,
    /// 期待バイト列の出典（逆アセンブルの行）。
    pub source: &'static str,
}

impl SiteDef {
    pub fn range(&self) -> AddrRange {
        AddrRange {
            start: self.addr,
            end: self.addr.wrapping_add(self.expected.len() as u32),
        }
    }

    /// `check_disjoint` に渡す core の `Site`。アドレスと長さしか見ないので、境目を持たない行は空の列にしておく
    /// （`check_site` には渡さない）。
    fn as_core_site(&self) -> Site {
        let instr_lens: &'static [u8] = match self.kind {
            Use::Detour { instr_lens } => instr_lens,
            Use::CallRel32 { .. } => &[5],
            Use::VerifyOnly { instr_lens } => instr_lens.unwrap_or(&[]),
        };
        Site {
            name: self.name,
            addr: self.addr,
            expected: self.expected,
            instr_lens,
        }
    }

    pub fn use_label(&self) -> &'static str {
        match self.kind {
            Use::Detour { .. } => "inline detour の予定",
            Use::CallRel32 { .. } => "call 先の差し替えの予定",
            Use::VerifyOnly { .. } => "照合だけ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    /// inline detour の行が `check_site` に落ちた（境目の和・短すぎ・相対の分岐・禁止の範囲）。
    Detour { name: &'static str, err: SiteError },
    /// 照合だけの行で、命令の境目の和が期待バイト列の長さと食い違う。
    LenSumMismatch {
        name: &'static str,
        sum: usize,
        expected_len: usize,
    },
    /// call 先の差し替えの行が `E8 rel32` の 5 バイトの形をしていない。
    CallShape { name: &'static str },
    /// rel32 から計算した飛び先が、表に書いた元の飛び先と食い違う。
    CallTarget {
        name: &'static str,
        computed: u32,
        original_target: u32,
    },
    /// inline detour 以外の行が禁止の範囲と重なった（inline detour の行は `Detour` の中で返る）。
    Forbidden {
        name: &'static str,
        range: AddrRange,
    },
    /// 2 つの行が重なった。
    Overlap(DisjointError),
}

/// 表そのものの検査。1 つ目に見つかった誤りで止める。
pub fn check_table(sites: &[SiteDef], forbidden: &[AddrRange]) -> Result<(), TableError> {
    for def in sites {
        match def.kind {
            Use::Detour { instr_lens } => {
                let site = Site {
                    name: def.name,
                    addr: def.addr,
                    expected: def.expected,
                    instr_lens,
                };
                patch::check_site(&site, forbidden).map_err(|err| TableError::Detour {
                    name: def.name,
                    err,
                })?;
            }
            Use::CallRel32 { original_target } => {
                let bytes: [u8; 5] = def
                    .expected
                    .try_into()
                    .map_err(|_| TableError::CallShape { name: def.name })?;
                let computed = patch::call_target(def.addr, &bytes)
                    .ok_or(TableError::CallShape { name: def.name })?;
                if computed != original_target {
                    return Err(TableError::CallTarget {
                        name: def.name,
                        computed,
                        original_target,
                    });
                }
                check_forbidden(def, forbidden)?;
            }
            Use::VerifyOnly { instr_lens } => {
                if let Some(lens) = instr_lens {
                    let sum: usize = lens.iter().map(|&l| l as usize).sum();
                    if sum != def.expected.len() {
                        return Err(TableError::LenSumMismatch {
                            name: def.name,
                            sum,
                            expected_len: def.expected.len(),
                        });
                    }
                }
                check_forbidden(def, forbidden)?;
            }
        }
    }
    // 照合だけの行も重なりの検査に入れる: 書く地点と重なっていたら、書いた後の照合が必ず落ちる。
    let core_sites: Vec<Site> = sites.iter().map(SiteDef::as_core_site).collect();
    let refs: Vec<&Site> = core_sites.iter().collect();
    patch::check_disjoint(&refs).map_err(TableError::Overlap)
}

fn check_forbidden(def: &SiteDef, forbidden: &[AddrRange]) -> Result<(), TableError> {
    let r = def.range();
    match forbidden.iter().find(|f| r.overlaps(f)) {
        Some(f) => Err(TableError::Forbidden {
            name: def.name,
            range: *f,
        }),
        None => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteOutcome {
    Match,
    /// 実際に読めたバイト列（期待バイト列と同じ長さ）。
    Mismatch {
        actual: Vec<u8>,
    },
    /// その範囲がコミット済みで読める頁ではなかった。
    Unreadable,
}

pub fn judge(def: &SiteDef, actual: Option<&[u8]>) -> SiteOutcome {
    match actual {
        None => SiteOutcome::Unreadable,
        Some(bytes) => match patch::verify(bytes, def.expected) {
            Ok(()) => SiteOutcome::Match,
            Err(_) => SiteOutcome::Mismatch {
                actual: bytes.to_vec(),
            },
        },
    }
}

/// `A1 08 31 4B 00` の形（ログと検査で使う）。
pub fn hex_bytes(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

#[cfg(test)]
mod tests {
    //! 表の型の検査（合成の値）。実物の表の検査は `th09_addr.rs` の側にある。
    use super::*;

    #[test]
    fn judge_distinguishes_match_mismatch_and_unreadable() {
        let def = SiteDef {
            name: "synthetic",
            addr: 0x0060_1000,
            expected: &[0x55, 0x8B, 0xEC, 0x51, 0x90],
            kind: Use::VerifyOnly {
                instr_lens: Some(&[1, 2, 1, 1]),
            },
            source: "合成",
        };
        assert_eq!(
            judge(&def, Some(&[0x55, 0x8B, 0xEC, 0x51, 0x90])),
            SiteOutcome::Match
        );
        assert_eq!(
            judge(&def, Some(&[0x55, 0x8B, 0xEC, 0x51, 0x91])),
            SiteOutcome::Mismatch {
                actual: vec![0x55, 0x8B, 0xEC, 0x51, 0x91]
            }
        );
        assert_eq!(judge(&def, None), SiteOutcome::Unreadable);
    }

    #[test]
    fn hex_bytes_uses_the_same_form_as_the_notes() {
        assert_eq!(hex_bytes(&[0xA1, 0x08, 0x31, 0x4B, 0x00]), "A1 08 31 4B 00");
        assert_eq!(hex_bytes(&[]), "");
    }
}
