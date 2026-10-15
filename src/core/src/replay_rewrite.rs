//! リプレイ（`.rpy`）の見出しの 1P / 2P のチャージ型を書き換えた写しを作る（再生のずれを直す）。
//!
//! 見出しのオプションの塊（`+0xDC` から 0xCC バイト。`th09.cfg` と同じ並び）の `+0xB4` / `+0xB5` が 1P / 2P のチャージ型で、
//! 展開した本体の座標では `+0x190` / `+0x191`。再生はこの塊から試合の設定を作る。
//! 書き換えたら本体を圧縮し直し、`comp_size`・LZSS の長さ・チェックサムを付け直して暗号化し直す。`comp_size` より後ろ
//! （USER チャンクの列。印 `HGRT` を含む）はそのまま付ける。
//!
//! ファイルには触らない（バイト列を受けてバイト列を返す）。
#![forbid(unsafe_code)]

use crate::replay::{
    BodyRead, MarkError, MarkRead, MarkRole, ReplayError, Side, mark_chunk, parse, parse_mark_body,
    read_mark,
};
use crate::replay_build::lzss_compress;
use crate::replay_decode::{
    BODY_START, CHECKSUM_OFFSET, DecodeError, ENC_START, KEY_OFFSET, checksum, decode_image,
    encrypt_in_place,
};

/// 見出しのオプションの塊の先頭（展開した本体の座標）。
pub const OPTIONS_OFFSET: usize = 0xDC;
/// オプションの塊の中の 1P のチャージ型（2P は次の 1 バイト）。
pub const OPT_CHARGE: usize = 0xB4;
/// 1P のチャージ型の位置（展開した本体の座標）。
pub const CHARGE_OFFSET: usize = OPTIONS_OFFSET + OPT_CHARGE;
const _: () = assert!(CHARGE_OFFSET == 0x190);

const COMP_SIZE_OFFSET: usize = 0x0C;
const LZSS_LEN_OFFSET: usize = 0x18;
const DECOMP_SIZE_OFFSET: usize = 0x1C;

/// 見出しの (1P, 2P) のチャージ型。
pub fn charge(image: &[u8]) -> Option<(u8, u8)> {
    Some((*image.get(CHARGE_OFFSET)?, *image.get(CHARGE_OFFSET + 1)?))
}

/// `file` のチャージ型を (`p1`, `p2`) に書き換えた写し。読めないリプレイは `decode_image` の理由で断る。
pub fn with_charge(file: &[u8], p1: u8, p2: u8) -> Result<Vec<u8>, DecodeError> {
    let mut image = decode_image(file)?;
    image[CHARGE_OFFSET] = p1;
    image[CHARGE_OFFSET + 1] = p2;
    Ok(reencode(file, &image))
}

/// `decode_image` の形の `image` から、`file` と同じ鍵・同じ末尾のファイルを組み直す。
/// `image` は `decode_image(file)` が通ったものを書き換えたものであること（見出しと長さは呼ぶ側が保つ）。
fn reencode(file: &[u8], image: &[u8]) -> Vec<u8> {
    let old_comp = u32_at(file, COMP_SIZE_OFFSET) as usize;
    let body = &image[BODY_START..];
    let lzss = lzss_compress(body);
    let comp_size = BODY_START + lzss.len();
    let mut out = image[..BODY_START].to_vec();
    put_u32(&mut out, COMP_SIZE_OFFSET, comp_size as u32);
    put_u32(&mut out, LZSS_LEN_OFFSET, lzss.len() as u32);
    put_u32(&mut out, DECOMP_SIZE_OFFSET, body.len() as u32);
    out.extend_from_slice(&lzss);
    let sum = checksum(&out, comp_size).unwrap_or(0);
    put_u32(&mut out, CHECKSUM_OFFSET, sum);
    let key = out[KEY_OFFSET];
    encrypt_in_place(&mut out[ENC_START..comp_size], key);
    out.extend_from_slice(file.get(old_comp..).unwrap_or(&[]));
    out
}

// ---- 古いリプレイの直し ----
//
// 見出しは、保存した PC の th09.cfg の (1P の枠, 2P の枠) のまま。生きた試合の値は、保存した人の PC からは分からない
// （相手の値が要る）ので、直す値は呼び出し側（人）が決める。

/// 1 本のリプレイの直しの下調べ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixPlan {
    /// 保存した人の側。観戦者のリプレイは `None`。
    pub side: Option<Side>,
    /// (1P, 2P) の名前（印から。対戦者のリプレイは自分と相手を `side` で並べる）。
    pub names: (String, String),
    /// 見出しの今の (1P, 2P) のチャージ型。
    pub current: (u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// 印 `HGRT` が無い（このツールが保存したものでない）か、読めない。
    NoMark,
    /// 印に `charge` がある（見出しを生きた試合の値に直して保存したバージョンか、直し済み）。直すものは無い。
    Recorded([u8; 2]),
    /// 印のビルド番号が `LAST_UNFIXED_BUILD` より新しい（見出しを直して保存するバージョン）。
    Newer(u32),
    Decode(DecodeError),
}

/// 見出しのチャージ型を直さずに保存した最後の配るビルド（v1.0.0）。これより新しい配るビルドのリプレイは直さない。
/// 開発版のビルド番号は 0 なので対象に残る。
pub const LAST_UNFIXED_BUILD: u32 = 411_272;

/// 印と見出しから、名前・側・今の値を出す。
pub fn plan(file: &[u8]) -> Result<FixPlan, PlanError> {
    let mark = match read_mark(file) {
        Ok(MarkRead::Found(m)) => m,
        _ => return Err(PlanError::NoMark),
    };
    if let Some(c) = mark.detail.as_ref().and_then(|d| d.charge) {
        return Err(PlanError::Recorded(c));
    }
    if let Some(b) = mark
        .detail
        .as_ref()
        .and_then(|d| d.build)
        .filter(|&b| b > LAST_UNFIXED_BUILD)
    {
        return Err(PlanError::Newer(b));
    }
    let image = decode_image(file).map_err(PlanError::Decode)?;
    let current = charge(&image).ok_or(PlanError::Decode(DecodeError::TooShort {
        len: image.len(),
    }))?;
    let (side, names) = match mark.role {
        MarkRole::Play => (
            Some(mark.side),
            match mark.side {
                Side::P1 => (mark.self_name, mark.peer),
                Side::P2 => (mark.peer, mark.self_name),
            },
        ),
        MarkRole::Watch { p1, p2 } => (None, (p1, p2)),
    };
    Ok(FixPlan {
        side,
        names,
        current,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkEditError {
    /// 印が無い・読めない・知らないバージョンだけ。
    NoMark,
    /// 平文の部分（USER チャンクの列）が読めない。
    Replay(ReplayError),
    /// 印を組み直せない（値が範囲の外）。
    Mark(MarkError),
}

/// 印の `charge` を (`p1`, `p2`) にした写し。印の他の項目と、他の USER チャンクはそのまま。見出しは触らない。
pub fn with_mark_charge(file: &[u8], p1: u8, p2: u8) -> Result<Vec<u8>, MarkEditError> {
    let layout = parse(file).map_err(MarkEditError::Replay)?;
    for chunk in layout.chunks.iter().filter(|c| c.is_mark()) {
        let Some(body) = chunk.body(file) else {
            continue;
        };
        let mut mark = match parse_mark_body(body) {
            Ok(BodyRead::Mark(m)) => m,
            Ok(BodyRead::UnknownVersion) => continue,
            Err(_) => return Err(MarkEditError::NoMark),
        };
        mark.detail.get_or_insert_with(Default::default).charge = Some([p1, p2]);
        let new = mark_chunk(&mark).map_err(MarkEditError::Mark)?;
        let mut out = Vec::with_capacity(file.len() + new.len());
        out.extend_from_slice(&file[..chunk.offset]);
        out.extend_from_slice(&new);
        out.extend_from_slice(file.get(chunk.offset + chunk.len..).unwrap_or(&[]));
        return Ok(out);
    }
    Err(MarkEditError::NoMark)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixError {
    Plan(PlanError),
    /// 渡した値が、見出しに入っている値と同じ（変えるものが無い）。
    Unchanged,
    Mark(MarkEditError),
}

/// 見出しのチャージ型を (`p1`, `p2`) にし、印にも `charge=<p1>.<p2>`（直し済みの印）を入れた写し。
/// 印に `charge` があるリプレイ・印が読めないリプレイ・値が今と同じリプレイは断る。
pub fn fixed_copy(file: &[u8], p1: u8, p2: u8) -> Result<Vec<u8>, FixError> {
    let plan = plan(file).map_err(FixError::Plan)?;
    if plan.current == (p1, p2) {
        return Err(FixError::Unchanged);
    }
    let body = with_charge(file, p1, p2).map_err(|e| FixError::Plan(PlanError::Decode(e)))?;
    with_mark_charge(&body, p1, p2).map_err(FixError::Mark)
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    let mut x = [0u8; 4];
    x.copy_from_slice(&b[off..off + 4]);
    u32::from_le_bytes(x)
}

fn put_u32(b: &mut [u8], off: usize, x: u32) {
    b[off..off + 4].copy_from_slice(&x.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{
        MarkRead, MarkRole, MatchResult, Side, Timestamp, chunk_to_append, read_mark,
    };
    use crate::replay_build::{BuildSpec, build_replay};
    use crate::replay_decode::decode_inputs;

    fn sample() -> Vec<u8> {
        build_replay(&BuildSpec::match_replay(
            0x60,
            (0..500u16).map(|i| i % 7).collect(),
            (0..500u16).map(|i| (i / 3) % 5).collect(),
        ))
    }

    #[test]
    fn only_the_two_charge_bytes_change_and_the_tail_is_kept() {
        let f = sample();
        let before = decode_image(&f).unwrap();
        // 合成の見出しの塊は (i % 251) なので、ここは 0xB4 / 0xB5。
        assert_eq!(charge(&before), Some((0xB4, 0xB5)));
        let g = with_charge(&f, 1, 0).unwrap();
        let after = decode_image(&g)
            .expect("組み直したファイルがゲームの検査（チェックサム・長さ）を通らない");
        assert_eq!(charge(&after), Some((1, 0)));
        assert_eq!(before.len(), after.len());
        let diff: Vec<usize> = (0..before.len())
            .filter(|&i| {
                before[i] != after[i] && !(CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4).contains(&i)
            })
            .collect();
        assert_eq!(
            diff,
            vec![CHARGE_OFFSET, CHARGE_OFFSET + 1],
            "チャージ型以外が変わった"
        );
        assert_eq!(
            decode_inputs(&f).unwrap(),
            decode_inputs(&g).unwrap(),
            "入力が変わった"
        );
        let tail = |x: &[u8]| x[u32_at(x, COMP_SIZE_OFFSET) as usize..].to_vec();
        assert_eq!(tail(&f), tail(&g), "USER チャンクの列が変わった");
        assert!(matches!(read_mark(&g), Ok(MarkRead::Absent)));
    }

    #[test]
    fn writing_the_same_values_gives_a_file_that_decodes_to_the_same_image() {
        let f = sample();
        let (p1, p2) = charge(&decode_image(&f).unwrap()).unwrap();
        let g = with_charge(&f, p1, p2).unwrap();
        assert_eq!(
            decode_image(&f).unwrap()[..CHECKSUM_OFFSET],
            decode_image(&g).unwrap()[..CHECKSUM_OFFSET]
        );
        assert_eq!(
            decode_image(&f).unwrap()[BODY_START..],
            decode_image(&g).unwrap()[BODY_START..]
        );
    }

    #[test]
    fn a_broken_file_is_refused() {
        let mut f = sample();
        f[0] = b'X';
        assert_eq!(with_charge(&f, 0, 0), Err(DecodeError::BadMagic));
    }

    /// 見出しを (`h1`, `h2`) にして、印を付けたリプレイ。
    fn marked(h1: u8, h2: u8, side: Side, role: MarkRole) -> Vec<u8> {
        marked_with(h1, h2, side, role, None)
    }

    fn marked_with(
        h1: u8,
        h2: u8,
        side: Side,
        role: MarkRole,
        detail: Option<crate::replay_detail::MatchDetail>,
    ) -> Vec<u8> {
        let mut f = with_charge(&sample(), h1, h2).unwrap();
        let mark = crate::replay::Mark {
            peer: if matches!(role, MarkRole::Play) {
                "Peer".to_string()
            } else {
                String::new()
            },
            self_name: "Me".to_string(),
            side,
            lat: 4,
            ping_ms: 277,
            result: MatchResult::Lose,
            at: Timestamp {
                year: 2026,
                month: 10,
                day: 14,
                hour: 19,
                minute: 59,
                second: 55,
            },
            role,
            detail,
        };
        let chunk = chunk_to_append(&f, &mark).unwrap();
        f.extend_from_slice(&chunk);
        f
    }

    fn watch_role() -> MarkRole {
        MarkRole::Watch {
            p1: "B".to_string(),
            p2: "A".to_string(),
        }
    }

    #[test]
    fn plan_lines_the_names_up_by_side() {
        // 保存した人 "Me"・相手 "Peer"。ゲスト（2P 側）なら 1P は相手、ホストなら 1P は自分。
        let guest = plan(&marked(0, 1, Side::P2, MarkRole::Play)).unwrap();
        assert_eq!(guest.side, Some(Side::P2));
        assert_eq!(guest.names, ("Peer".to_string(), "Me".to_string()));
        assert_eq!(guest.current, (0, 1));
        let host = plan(&marked(1, 0, Side::P1, MarkRole::Play)).unwrap();
        assert_eq!(host.side, Some(Side::P1));
        assert_eq!(host.names, ("Me".to_string(), "Peer".to_string()));
        assert_eq!(host.current, (1, 0));
        let w = plan(&marked(0, 0, Side::P1, watch_role())).unwrap();
        assert_eq!(w.side, None);
        assert_eq!(w.names, ("B".to_string(), "A".to_string()));
    }

    /// 印の他の項目と他の USER チャンクが、印の差し替えで変わらないことを見るための、チャンクを 2 つ足したリプレイ。
    fn marked_between_other_chunks(detail: Option<crate::replay_detail::MatchDetail>) -> Vec<u8> {
        let game_chunk = |text: &[u8]| {
            let total = (text.len() + 13 + 1) & !1;
            let mut c = b"USER".to_vec();
            c.extend_from_slice(&(total as u32).to_le_bytes());
            c.extend_from_slice(&0x7FFF_FF00u32.to_le_bytes());
            c.extend_from_slice(text);
            c.push(0);
            c.resize(total, 0);
            c
        };
        let plain = with_charge(&sample(), 0, 1).unwrap();
        let comp = u32_at(&plain, COMP_SIZE_OFFSET) as usize;
        let mut f = plain[..comp].to_vec();
        f.extend(game_chunk(b"first chunk"));
        let mark = crate::replay::Mark {
            peer: "Peer".to_string(),
            self_name: "Me".to_string(),
            side: Side::P2,
            lat: 4,
            ping_ms: 277,
            result: MatchResult::Win,
            at: Timestamp {
                year: 2026,
                month: 10,
                day: 14,
                hour: 19,
                minute: 59,
                second: 55,
            },
            role: MarkRole::Play,
            detail,
        };
        f.extend(crate::replay::mark_chunk(&mark).unwrap());
        f.extend(game_chunk(b"last chunk"));
        f
    }

    #[test]
    fn the_mark_gets_the_charge_and_everything_else_in_the_tail_is_kept() {
        let detail = crate::replay_detail::MatchDetail {
            build: Some(403_920),
            match_no: Some(2),
            ..Default::default()
        };
        let f = marked_between_other_chunks(Some(detail));
        let g = with_mark_charge(&f, 1, 0).unwrap();
        let comp = u32_at(&f, COMP_SIZE_OFFSET) as usize;
        assert_eq!(&f[..comp], &g[..comp], "見出しと本体は触らない");
        let (lf, lg) = (parse(&f).unwrap(), parse(&g).unwrap());
        assert_eq!(lf.chunks.len(), 3);
        assert_eq!(lg.chunks.len(), 3);
        for (i, (a, b)) in lf.chunks.iter().zip(&lg.chunks).enumerate() {
            if a.is_mark() {
                continue;
            }
            assert_eq!(a.body(&f), b.body(&g), "{i} 番目の USER チャンクが変わった");
        }
        let (MarkRead::Found(mf), MarkRead::Found(mg)) =
            (read_mark(&f).unwrap(), read_mark(&g).unwrap())
        else {
            panic!("印が読めない");
        };
        assert_eq!(mg.detail.as_ref().unwrap().charge, Some([1, 0]));
        let mut expect = mf;
        expect.detail.as_mut().unwrap().charge = Some([1, 0]);
        assert_eq!(mg, expect, "charge 以外の項目が変わった");
    }

    #[test]
    fn a_mark_without_detail_gets_only_the_charge_key() {
        let f = marked_between_other_chunks(None);
        let g = with_mark_charge(&f, 0, 1).unwrap();
        let MarkRead::Found(m) = read_mark(&g).unwrap() else {
            panic!("印が読めない");
        };
        let fields = m.detail.unwrap().fields();
        assert_eq!(fields, vec![("charge", "0.1".to_string())]);
    }

    #[test]
    fn without_a_readable_mark_nothing_is_written() {
        assert_eq!(
            with_mark_charge(&sample(), 0, 1),
            Err(MarkEditError::NoMark)
        );
        assert!(matches!(
            with_mark_charge(&sample()[..10], 0, 1),
            Err(MarkEditError::Replay(_))
        ));
    }

    #[test]
    fn a_fixed_copy_has_the_header_and_the_mark_with_the_chosen_values() {
        let f = marked(0, 0, Side::P2, MarkRole::Play);
        let g = fixed_copy(&f, 1, 0).unwrap();
        let image = decode_image(&g).expect("ゲームの検査（チェックサム・長さ）を通らない");
        assert_eq!(charge(&image), Some((1, 0)));
        assert_eq!(decode_inputs(&f).unwrap(), decode_inputs(&g).unwrap());
        let MarkRead::Found(m) = read_mark(&g).unwrap() else {
            panic!("印が読めない");
        };
        assert_eq!(m.detail.unwrap().charge, Some([1, 0]));
        assert_eq!(m.peer, "Peer");
        // 直した写しは「直し済み」なので、もう一度直そうとしても断る（反転が戻る事故を防ぐ）。
        assert_eq!(
            fixed_copy(&g, 0, 0),
            Err(FixError::Plan(PlanError::Recorded([1, 0])))
        );
    }

    #[test]
    fn a_fixed_copy_is_refused_when_nothing_would_change_or_the_mark_is_missing() {
        let f = marked(0, 1, Side::P1, MarkRole::Play);
        assert_eq!(fixed_copy(&f, 0, 1), Err(FixError::Unchanged));
        assert_eq!(
            fixed_copy(&sample(), 0, 1),
            Err(FixError::Plan(PlanError::NoMark))
        );
        let recorded = crate::replay_detail::MatchDetail {
            charge: Some([0, 1]),
            ..Default::default()
        };
        let r = marked_with(0, 1, Side::P1, MarkRole::Play, Some(recorded));
        assert_eq!(
            fixed_copy(&r, 1, 1),
            Err(FixError::Plan(PlanError::Recorded([0, 1])))
        );
    }

    #[test]
    fn a_replay_whose_mark_records_the_charge_is_not_planned() {
        let d = crate::replay_detail::MatchDetail {
            charge: Some([1, 0]),
            ..Default::default()
        };
        // 新しい版のゲストのリプレイ: 見出しは直してあり (1, 0)。1P の枠を自分の値と読むと 1 になり、壊す。
        let f = marked_with(1, 0, Side::P2, MarkRole::Play, Some(d));
        assert_eq!(plan(&f), Err(PlanError::Recorded([1, 0])));
        let without = marked_with(1, 0, Side::P2, MarkRole::Play, Some(Default::default()));
        assert!(plan(&without).is_ok(), "charge の無い印は直す対象");
    }

    /// 境目は v1.0.0 のビルド番号 411272（`847dc03` の配る zip）。
    #[test]
    fn a_replay_from_a_build_newer_than_v1_0_0_is_not_planned() {
        let with_build = |b| crate::replay_detail::MatchDetail {
            build: Some(b),
            ..Default::default()
        };
        for b in [0, 403_920, 411_272] {
            let f = marked_with(0, 0, Side::P2, MarkRole::Play, Some(with_build(b)));
            assert!(plan(&f).is_ok(), "ビルド {b} は直す対象");
        }
        let f = marked_with(0, 0, Side::P2, MarkRole::Play, Some(with_build(411_273)));
        assert_eq!(plan(&f), Err(PlanError::Newer(411_273)));
        assert_eq!(
            fixed_copy(&f, 1, 0),
            Err(FixError::Plan(PlanError::Newer(411_273)))
        );
    }

    #[test]
    fn a_replay_without_the_mark_is_not_planned() {
        assert_eq!(plan(&sample()), Err(PlanError::NoMark));
        assert!(matches!(read_mark(&sample()), Ok(MarkRead::Absent)));
    }
}
