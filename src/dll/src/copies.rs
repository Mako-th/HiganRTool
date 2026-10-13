//! コピーの表の確かめと、コピーした範囲の読み方（pure。unsafe 無し）。入口は枠の先頭（64 バイト）の後ろへ、コピーの表の範囲を
//! そのままコピーする。ここは
//! - 表の確かめ（枠に収まる・先頭と重ならない・互いに重ならない・元が読める）。入口が枠の外へ書かないための守りなので、
//!   `stub::set_copy_table` も置く前に枠の側（`check_layout`）を通す。
//! - `_ticks.csv` のコピーの列（乱数・InputManager 3 本・選んだグローバル）、`_changes.csv`（2 つのグローバルの範囲を
//!   4 バイトずつ前の件と比べた差分）、`_snap.bin` の先頭。
//!
//! コピーした値は「入口が実際に使っている表」から引く。表に無いアドレスの列は空にする（0 と書くと「測れていない」が「0 だった」に見える）。
#![forbid(unsafe_code)]

use crate::th09_addr::{
    BACKGROUND, DIFFICULTY, GAME_MODE, IM_CHARGE, IM_CUR, IM_DOWN, IM_GAME, IM_PREV, IM_REPEAT,
    IM_UP, INPUT_MANAGERS, LOAD_DONE, LOADING, MAP, MATCH_LEN, MATCH_RANGE, MATCH_REGISTERED,
    P1_CHAR, P1_CONTROL, P2_CHAR, P2_CONTROL, PAUSE, RNG_ADDR, RNG_COUNT, SEATS_LEN, SEATS_RANGE,
    SELECT_KIND,
};
use crate::ticks::{COPY_MAX_ROWS, CopyRow, HEAD_BYTES, SLOT_BYTES, Snap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    /// 行が `COPY_MAX_ROWS` より多い。
    TooMany { rows: usize },
    /// 長さが 0（表の書き誤りの印なので断る）。
    Empty { row: usize },
    /// 枠の外（1024 バイトを越える・アドレスの計算が 32 ビットを越える）。
    OutOfSlot { row: usize },
    /// 先頭（`+0`〜`+63`。札を含む）と重なる。
    OverlapsHead { row: usize },
    /// 2 つの行の置き場が重なる。
    Overlap { a: usize, b: usize },
    /// 元の範囲が読めない（コミット済みで読める保護でない）。
    Unreadable { row: usize },
}

impl CopyError {
    pub fn text(&self) -> String {
        match *self {
            CopyError::TooMany { rows } => {
                format!("行が {rows} 行で、上限 {COPY_MAX_ROWS} 行を越える")
            }
            CopyError::Empty { row } => format!("{} 行目の長さが 0", row + 1),
            CopyError::OutOfSlot { row } => {
                format!("{} 行目が枠（{SLOT_BYTES} バイト）からはみ出す", row + 1)
            }
            CopyError::OverlapsHead { row } => {
                format!("{} 行目が枠の先頭（{HEAD_BYTES} バイト）と重なる", row + 1)
            }
            CopyError::Overlap { a, b } => {
                format!("{} 行目と {} 行目の置き場が重なる", a + 1, b + 1)
            }
            CopyError::Unreadable { row } => format!("{} 行目の元の範囲が読めない", row + 1),
        }
    }
}

/// 枠の中の置き場 `[dst, dst + len)`（はみ出しの計算で回り込まない）。
fn dst_range(r: &CopyRow) -> Option<(u32, u32)> {
    Some((r.dst, r.dst.checked_add(r.len)?))
}

/// 枠の側の確かめ（入口が枠の外・先頭・他の行の置き場に書かないこと）。元を読まずに決まる。
pub fn check_layout(rows: &[CopyRow]) -> Result<(), CopyError> {
    if rows.len() > COPY_MAX_ROWS {
        return Err(CopyError::TooMany { rows: rows.len() });
    }
    for (i, r) in rows.iter().enumerate() {
        if r.len == 0 {
            return Err(CopyError::Empty { row: i });
        }
        let Some((start, end)) = dst_range(r) else {
            return Err(CopyError::OutOfSlot { row: i });
        };
        if end > SLOT_BYTES || r.src.checked_add(r.len).is_none() {
            return Err(CopyError::OutOfSlot { row: i });
        }
        if start < HEAD_BYTES {
            return Err(CopyError::OverlapsHead { row: i });
        }
    }
    for (i, a) in rows.iter().enumerate() {
        for (j, b) in rows.iter().enumerate().skip(i + 1) {
            // 上で回り込みは断ってあるので、ここでは足してよい。
            if a.dst < b.dst + b.len && b.dst < a.dst + a.len {
                return Err(CopyError::Overlap { a: i, b: j });
            }
        }
    }
    Ok(())
}

/// 表の全部の確かめ: 枠の側に加えて、各行の元が読めるか（`readable`。本物は `Env::read`: コミット済みで読める保護の頁だけ）。
pub fn check_copy_table(
    rows: &[CopyRow],
    mut readable: impl FnMut(u32, u32) -> bool,
) -> Result<(), CopyError> {
    check_layout(rows)?;
    for (i, r) in rows.iter().enumerate() {
        if !readable(r.src, r.len) {
            return Err(CopyError::Unreadable { row: i });
        }
    }
    Ok(())
}

/// 表の行の説明（ログ用）: `0x004A7D90 から 320 バイト → 枠 +64`。
pub fn row_text(r: &CopyRow) -> String {
    format!("0x{:08X} から {} バイト → 枠 +{}", r.src, r.len, r.dst)
}

/// ゲームのアドレス `addr` の `width` バイトを、入口が使った表 `rows` を通して枠から読む。
/// 表のどの行も `[addr, addr + width)` を丸ごと含まなければ `None`（コピーしていない）。
pub fn value_at(rows: &[CopyRow], snap: &Snap, addr: u32, width: u32) -> Option<u32> {
    let r = rows.iter().find(|r| {
        addr >= r.src
            && addr
                .checked_add(width)
                .is_some_and(|end| end <= r.src.wrapping_add(r.len))
    })?;
    snap.read(r.dst + (addr - r.src), width)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Hex16,
    Dec16,
    Signed32,
    Dec32,
}

fn columns() -> Vec<(String, u32, u32, Form)> {
    let mut c = vec![
        (
            format!("rng_state_{RNG_ADDR:06X}"),
            RNG_ADDR,
            2,
            Form::Hex16,
        ),
        (
            format!("rng_count_{RNG_COUNT:06X}"),
            RNG_COUNT,
            4,
            Form::Dec32,
        ),
    ];
    for (k, base) in INPUT_MANAGERS.iter().enumerate() {
        for (name, off, form) in [
            ("cur", IM_CUR, Form::Hex16),
            ("prev", IM_PREV, Form::Hex16),
            ("rep", IM_REPEAT, Form::Hex16),
            ("down", IM_DOWN, Form::Hex16),
            ("up", IM_UP, Form::Hex16),
            ("2a", IM_CHARGE, Form::Dec16),
            ("2c", IM_GAME, Form::Hex16),
        ] {
            c.push((format!("im{k}_{name}"), base + off, 2, form));
        }
    }
    for (name, addr) in [
        ("game_mode", GAME_MODE),
        ("sel", SELECT_KIND),
        ("p1_ctrl", P1_CONTROL),
        ("p2_ctrl", P2_CONTROL),
        ("loading", LOADING),
        ("load_done", LOAD_DONE),
        ("pause", PAUSE),
        ("match_reg", MATCH_REGISTERED),
        ("difficulty", DIFFICULTY),
        ("map", MAP),
        ("bg", BACKGROUND),
        ("p1_char", P1_CHAR),
        ("p2_char", P2_CHAR),
    ] {
        c.push((format!("{name}_{addr:06X}"), addr, 4, Form::Signed32));
    }
    c
}

pub fn csv_header() -> String {
    columns()
        .into_iter()
        .map(|(h, ..)| h)
        .collect::<Vec<_>>()
        .join(",")
}

/// 1 件のコピーの列（先頭のカンマなし）。表に無いアドレスは空。
pub fn csv_columns(rows: &[CopyRow], snap: &Snap) -> String {
    columns()
        .into_iter()
        .map(
            |(_, addr, width, form)| match value_at(rows, snap, addr, width) {
                None => String::new(),
                Some(v) => match form {
                    Form::Hex16 => format!("0x{v:04X}"),
                    Form::Dec16 | Form::Dec32 => v.to_string(),
                    Form::Signed32 => (v as i32).to_string(),
                },
            },
        )
        .collect::<Vec<_>>()
        .join(",")
}

pub const CHANGES_HEADER: &str = "seq,addr,before,after,sup454,scene_cur,scene_req";

/// 差分を見る 2 つの範囲（2 つのグローバルを 4 バイトずつ）。
pub const CHANGE_RANGES: [(u32, u32); 2] = [(SEATS_RANGE, SEATS_LEN), (MATCH_RANGE, MATCH_LEN)];

/// 2 つの範囲の 4 バイトずつの値（アドレスの順）。表に無い範囲の語は `None`。
pub fn change_words(rows: &[CopyRow], snap: &Snap) -> Vec<(u32, Option<u32>)> {
    let mut v = Vec::new();
    for (start, len) in CHANGE_RANGES {
        for k in 0..len / 4 {
            let a = start + 4 * k;
            v.push((a, value_at(rows, snap, a, 4)));
        }
    }
    v
}

/// 差分の行を作る（前の件から変わった語だけ。前が無ければ全部の語。値が取れない語は書かない）。
/// 呼び出し側は、書いた後に `words` を次の件の `prev` として持つ。
pub fn change_rows(
    prev: Option<&[(u32, Option<u32>)]>,
    words: &[(u32, Option<u32>)],
    snap: &Snap,
) -> Vec<String> {
    let r = &snap.rec;
    let tail = format!(
        "{},{},{}",
        r.sup_frame, r.scene_cur as i32, r.scene_req as i32
    );
    let mut out = Vec::new();
    for (i, &(addr, now)) in words.iter().enumerate() {
        let Some(now) = now else { continue };
        let before = prev.and_then(|p| p.get(i)).and_then(|&(a, v)| {
            debug_assert_eq!(a, addr);
            v
        });
        if prev.is_some() && before == Some(now) {
            continue;
        }
        let before = before.map(|b| format!("0x{b:08X}")).unwrap_or_default();
        out.push(format!(
            "{},0x{addr:08X},{before},0x{now:08X},{tail}",
            r.seq
        ));
    }
    out
}

/// `_snap.bin` の先頭の大きさ（この後ろに 1024 バイトの枠が取れた順に並ぶ）。
pub const SNAP_HEADER_BYTES: usize = 128;
pub const SNAP_MAGIC: &[u8; 8] = b"HRTSNAP\0";
pub const SNAP_VERSION: u32 = 1;

/// `_snap.bin` の先頭: 印 8 / バージョン 4 / 枠の大きさ 4 / 先頭の大きさ 4 / 表の行の数 4 / 表の 8 行（元・長さ・置き場の
/// 4 バイトずつ。使わない行は 0）96 / 残り 0。数はリトルエンディアン。枠の札（`+48`）が番号 + 1 なので、
/// 件ごとの先頭は置かない。
pub fn snap_header(rows: &[CopyRow]) -> Vec<u8> {
    let mut h = Vec::with_capacity(SNAP_HEADER_BYTES);
    h.extend_from_slice(SNAP_MAGIC);
    for v in [SNAP_VERSION, SLOT_BYTES, HEAD_BYTES, rows.len() as u32] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    for i in 0..COPY_MAX_ROWS {
        let r = rows.get(i).copied().unwrap_or(CopyRow {
            src: 0,
            len: 0,
            dst: 0,
        });
        for v in [r.src, r.len, r.dst] {
            h.extend_from_slice(&v.to_le_bytes());
        }
    }
    h.resize(SNAP_HEADER_BYTES, 0);
    h
}

#[cfg(test)]
pub(crate) mod tests {
    //! 合成の枠と表だけで見る（ゲームのメモリは読まない）。
    use super::*;
    use crate::ticks::tests::rec;
    use crate::ticks::{COPY_TABLE, OFF_COPY_INPUT, OFF_COPY_MATCH, OFF_COPY_RNG, SLOT_WORDS};

    /// 枠を 1 つ作る。先頭は `rec` の値、コピーの所は `fill(枠のバイト位置)` のバイト。
    pub(crate) fn snap_with(seq: u32, sup: u32, fill: impl Fn(u32) -> u8) -> Snap {
        let r = rec(seq, 1000 + seq as u64, 1.0 + seq as f64, sup, 2, 2);
        let mut words = Box::new([0u32; SLOT_WORDS]);
        words[..crate::ticks::BODY_WORDS].copy_from_slice(&r.to_words());
        for (i, w) in words.iter_mut().enumerate().skip((HEAD_BYTES / 4) as usize) {
            let b = |k: u32| fill(i as u32 * 4 + k) as u32;
            *w = b(0) | b(1) << 8 | b(2) << 16 | b(3) << 24;
        }
        Snap::from_words(seq, words)
    }

    fn row(src: u32, len: u32, dst: u32) -> CopyRow {
        CopyRow { src, len, dst }
    }

    #[test]
    fn the_table_of_this_version_fits_the_slot() {
        assert_eq!(check_layout(&COPY_TABLE), Ok(()));
        assert_eq!(COPY_TABLE.iter().map(|r| r.len).sum::<u32>(), 548);
        assert_eq!(COPY_TABLE[0].dst, HEAD_BYTES);
        let end = COPY_TABLE.iter().map(|r| r.dst + r.len).max().unwrap();
        assert_eq!(end, HEAD_BYTES + 548);
        assert!(COPY_TABLE.len() <= COPY_MAX_ROWS - 2, "予備の 2 行が無い");
        assert_eq!(
            (OFF_COPY_MATCH, OFF_COPY_RNG, OFF_COPY_INPUT),
            (384, 432, [444, 500, 556])
        );
    }

    #[test]
    fn bad_tables_are_refused() {
        let ok = [row(0x1000, 8, 64), row(0x2002, 6, 72)];
        assert_eq!(check_copy_table(&ok, |_, _| true), Ok(()));
        let unreadable = |a: u32, _| a != 0x2002;
        assert_eq!(
            check_copy_table(&ok, unreadable),
            Err(CopyError::Unreadable { row: 1 })
        );
        assert_eq!(check_layout(&[row(0x1000, 24, 1000)]), Ok(()));
        assert_eq!(
            check_layout(&[row(0x1000, 25, 1000)]),
            Err(CopyError::OutOfSlot { row: 0 })
        );
        assert_eq!(
            check_layout(&[row(0x1000, 4, u32::MAX - 1)]),
            Err(CopyError::OutOfSlot { row: 0 })
        );
        assert_eq!(
            check_layout(&[row(u32::MAX - 1, 4, 64)]),
            Err(CopyError::OutOfSlot { row: 0 })
        );
        assert_eq!(
            check_layout(&[row(0x1000, 8, 64), row(0x3000, 4, 63)]),
            Err(CopyError::OverlapsHead { row: 1 })
        );
        assert_eq!(
            check_layout(&[row(0x1000, 8, 64), row(0x3000, 4, 71)]),
            Err(CopyError::Overlap { a: 0, b: 1 })
        );
        assert_eq!(
            check_layout(&[row(0x1000, 8, 64), row(0x3000, 4, 72)]),
            Ok(())
        );
        assert_eq!(
            check_layout(&[row(0x1000, 0, 64)]),
            Err(CopyError::Empty { row: 0 })
        );
        let nine: Vec<CopyRow> = (0..9).map(|i| row(0x1000, 4, 64 + 4 * i)).collect();
        assert_eq!(check_layout(&nine), Err(CopyError::TooMany { rows: 9 }));
        assert_eq!(check_layout(&nine[..8]), Ok(()));
        assert!(CopyError::Unreadable { row: 1 }.text().contains("2 行目"));
    }

    #[test]
    fn csv_columns_read_through_the_table() {
        let s = snap_with(5, 10, |p| (p.wrapping_mul(7).wrapping_add(3)) as u8);
        let at = |addr: u32, width: u32| -> u32 {
            let r = COPY_TABLE
                .iter()
                .find(|r| addr >= r.src && addr + width <= r.src + r.len)
                .unwrap();
            let p = r.dst + addr - r.src;
            (0..width).fold(0, |v, i| v | (((p + i) * 7 + 3) as u8 as u32) << (8 * i))
        };
        let cols = csv_columns(&COPY_TABLE, &s);
        let vals: Vec<&str> = cols.split(',').collect();
        let heads = csv_header();
        let heads: Vec<&str> = heads.split(',').collect();
        assert_eq!(vals.len(), heads.len());
        assert_eq!(vals.len(), 2 + 21 + 13);
        let get = |h: &str| vals[heads.iter().position(|x| *x == h).unwrap()];
        assert_eq!(
            get("rng_state_4ACE0C"),
            format!("0x{:04X}", at(RNG_ADDR, 2))
        );
        assert_eq!(get("rng_count_4ACE10"), at(RNG_COUNT, 4).to_string());
        assert_eq!(
            get("im1_down"),
            format!("0x{:04X}", at(INPUT_MANAGERS[1] + IM_DOWN, 2))
        );
        assert_eq!(
            get("im1_2a"),
            at(INPUT_MANAGERS[1] + IM_CHARGE, 2).to_string()
        );
        assert_eq!(
            get("im2_2c"),
            format!("0x{:04X}", at(INPUT_MANAGERS[2] + IM_GAME, 2))
        );
        assert_eq!(
            get("game_mode_4A7EA8"),
            (at(GAME_MODE, 4) as i32).to_string()
        );
        assert_eq!(
            get("load_done_4A811C"),
            (at(LOAD_DONE, 4) as i32).to_string()
        );
        assert_eq!(get("p2_char_4A7DE8"), (at(P2_CHAR, 4) as i32).to_string());

        let empty = csv_columns(&[], &s);
        assert!(empty.split(',').all(str::is_empty), "{empty}");
        assert_eq!(empty.split(',').count(), heads.len());
        let partial: Vec<CopyRow> = COPY_TABLE[..3].to_vec();
        let cols = csv_columns(&partial, &s);
        let vals: Vec<&str> = cols.split(',').collect();
        let get = |h: &str| vals[heads.iter().position(|x| *x == h).unwrap()];
        assert_eq!(get("im0_cur"), "");
        assert_ne!(get("map_4A7EC8"), "");
    }

    #[test]
    fn changes_list_every_word_first_then_only_changes() {
        let a = snap_with(3, 100, |_| 0);
        let wa = change_words(&COPY_TABLE, &a);
        assert_eq!(wa.len(), (SEATS_LEN / 4 + MATCH_LEN / 4) as usize);
        let first = change_rows(None, &wa, &a);
        assert_eq!(first.len(), wa.len());
        assert_eq!(
            first[0],
            format!("3,0x{SEATS_RANGE:08X},,0x00000000,100,2,2")
        );
        let pos = |addr: u32| {
            let r = COPY_TABLE
                .iter()
                .find(|r| addr >= r.src && addr < r.src + r.len)
                .unwrap();
            r.dst + addr - r.src
        };
        let (p1, p2) = (pos(LOADING), pos(LOAD_DONE));
        let b = snap_with(4, 101, |p| {
            if p == p1 {
                1
            } else if p == p2 + 1 {
                2
            } else {
                0
            }
        });
        let wb = change_words(&COPY_TABLE, &b);
        let rows = change_rows(Some(&wa), &wb, &b);
        assert_eq!(
            rows,
            vec![
                format!("4,0x{LOADING:08X},0x00000000,0x00000001,101,2,2"),
                format!("4,0x{LOAD_DONE:08X},0x00000000,0x00000200,101,2,2"),
            ]
        );
        assert!(change_rows(Some(&wb), &wb, &b).is_empty());
        let none = change_words(&[], &a);
        assert!(change_rows(None, &none, &a).is_empty());
    }

    #[test]
    fn snap_header_layout() {
        let h = snap_header(&COPY_TABLE);
        assert_eq!(h.len(), SNAP_HEADER_BYTES);
        assert_eq!(&h[..8], SNAP_MAGIC);
        let u = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        assert_eq!((u(8), u(12), u(16), u(20)), (1, 1024, 64, 6));
        assert_eq!((u(24), u(28), u(32)), (SEATS_RANGE, SEATS_LEN, 64));
        assert_eq!(u(24 + 5 * 12), INPUT_MANAGERS[2]);
        assert_eq!((u(24 + 6 * 12), u(24 + 7 * 12 + 8)), (0, 0));
        assert!(h[120..].iter().all(|&b| b == 0));
    }
}
