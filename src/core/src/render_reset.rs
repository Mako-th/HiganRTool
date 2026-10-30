//! コピーの中の「描画が消すフィールド」を描画の後の値に置き換える（カットインの赤い点滅の修正）。
//! 花映塚には「更新が毎tick立て、描画が使ってから消す」フィールドがある（Stageの背景の色`+0x642C`。アドレスはdllの`th09_addr::RENDER_RESETS`）。
//! 描画を通らない進め直しで取ったコピーにはその値が残る。
//! 書き戻すたびにその値が戻ると描画が色を掛け続ける。
//! コピーを取るたびにコピーの中のそのフィールドだけを描画の後の値へ置き換える。ゲームのメモリには書かない。
//! ブロックのバイト列とフィールドの表を受け取るだけの純粋な関数（確保・パニックしない。ゲームスレッドから呼ぶ）。
//! はみ出すフィールドがあれば1バイトも書かずに断る。

/// 置き換えるフィールド1つ。`off`はブロックの先頭からのバイト。`after`は描画の後の値（長さがフィールドの長さ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field<'a> {
    pub off: u32,
    pub after: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// フィールドの長さが0（何も置き換えないのに「直した」と数えないため断る）。
    Empty { index: usize },
    /// フィールドがブロックの外へはみ出す（`off + after.len() > block_len`）か、アドレスの計算が溢れる。
    OutOfBlock {
        index: usize,
        off: u32,
        len: usize,
        block_len: usize,
    },
}

/// 表が長さ`block_len`のブロックに収まるかどうか（置き換える前と表を組むとき）。
pub fn check(block_len: usize, fields: &[Field<'_>]) -> Result<(), Refused> {
    for (index, f) in fields.iter().enumerate() {
        if f.after.is_empty() {
            return Err(Refused::Empty { index });
        }
        let end = usize::try_from(f.off)
            .ok()
            .and_then(|o| o.checked_add(f.after.len()));
        if end.is_none_or(|e| e > block_len) {
            return Err(Refused::OutOfBlock {
                index,
                off: f.off,
                len: f.after.len(),
                block_len,
            });
        }
    }
    Ok(())
}

/// ブロックの中の表のフィールドを描画の後の値に置き換え、元の値が違っていたフィールドの数を返す（Presentの後のコピーでは0）。
/// 1つでもブロックに収まらなければ何も書かずに断る。
pub fn apply(block: &mut [u8], fields: &[Field<'_>]) -> Result<u32, Refused> {
    check(block.len(), fields)?;
    let mut changed = 0u32;
    for f in fields {
        // `check`で範囲を見たのでブロックの中（`get_mut`はパニックしないため）。
        let start = f.off as usize;
        let Some(dst) = block.get_mut(start..start + f.after.len()) else {
            continue;
        };
        if dst != f.after {
            changed = changed.saturating_add(1);
            dst.copy_from_slice(f.after);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// どのバイトも位置で決まる別の値（0埋めだと「元から0」と「書いた0」が見分けられない）。
    fn block(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(1))
            .collect()
    }

    #[test]
    fn only_the_fields_change_and_a_second_pass_changes_nothing() {
        let mut b = block(64);
        let before = b.clone();
        let after_a = [0x80, 0x80, 0x80, 0x00];
        let after_b = [0x11, 0x22];
        let t = [
            Field {
                off: 0x10,
                after: &after_a,
            },
            Field {
                off: 0x31,
                after: &after_b,
            },
        ];
        assert_eq!(apply(&mut b, &t), Ok(2));
        assert_eq!(&b[0x10..0x14], &after_a);
        assert_eq!(&b[0x31..0x33], &after_b);
        for (i, (x, y)) in b.iter().zip(before.iter()).enumerate() {
            if !(0x10..0x14).contains(&i) && !(0x31..0x33).contains(&i) {
                assert_eq!(x, y, "欄の外の {i:#X} が変わった");
            }
        }
        let once = b.clone();
        assert_eq!(apply(&mut b, &t), Ok(0));
        assert_eq!(b, once);
    }

    #[test]
    fn a_field_crossing_the_block_end_is_refused_without_writing() {
        let after = [0xAA, 0xBB, 0xCC, 0xDD];
        let ok = Field {
            off: 0,
            after: &after,
        };
        let edge = Field {
            off: 60,
            after: &after,
        };
        let mut b = block(64);
        assert_eq!(apply(&mut b, &[ok, edge]), Ok(2));
        // 1バイトはみ出す欄が後ろにあると、前の欄も書かない。
        let cross = Field {
            off: 61,
            after: &after,
        };
        let mut b = block(64);
        let before = b.clone();
        assert_eq!(
            apply(&mut b, &[ok, cross]),
            Err(Refused::OutOfBlock {
                index: 1,
                off: 61,
                len: 4,
                block_len: 64
            })
        );
        assert_eq!(b, before, "断ったのに書いた");
        // アドレスの溢れ（u32::MAX + 4）もパニックせず断る。
        let huge = Field {
            off: u32::MAX,
            after: &after,
        };
        assert!(matches!(
            apply(&mut b, &[huge]),
            Err(Refused::OutOfBlock { index: 0, .. })
        ));
        let empty = Field { off: 0, after: &[] };
        assert_eq!(apply(&mut b, &[empty]), Err(Refused::Empty { index: 0 }));
        assert_eq!(b, before);
    }

    #[test]
    fn an_empty_table_touches_nothing() {
        let mut b = block(16);
        let before = b.clone();
        assert_eq!(apply(&mut b, &[]), Ok(0));
        assert_eq!(b, before);
        assert_eq!(check(0, &[]), Ok(()));
    }
}
