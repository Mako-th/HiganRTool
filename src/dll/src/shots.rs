//! 撮影の、置き場の先頭の並び・コピーしてよいかの判断・読み取りのスレッドでの PNG と `_shots.csv` と集計（pure）。
//! ゲームのスレッド（`shot_win::shot_on_draw`。描画の入口 `0x0042DD51` の中）がバックバッファを置き場（640×480×4 バイトを 4 枚）へコピー、
//! 置き場の先頭に各 HRESULT・desc の 8 語・present parameters の 3 語などを書く。形式の変換・PNG への符号化・書き込みは読み取りのスレッド（ここ）。
//! 画素の変換と PNG は `higanr_core::shot`（`to_rgb`・`encode_png`）。
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Write;

use higanr_core::script::{ABORT, END, POST, PRE, SETTLE, START, TRANSITION, WAIT};
use higanr_core::shot::{bytes_per_pixel, encode_png, to_rgb};

use crate::ticks::tsc_to_us;

pub const SHOT_SLOTS: usize = 4;
pub const SHOT_CAP: usize = 640 * 480 * 4;
pub const SHOT_FILE_LIMIT: u32 = 400;

// ---- 置き場の先頭（u32 の語。ゲームのスレッドが書き、読み取りのスレッドが読む）----
pub const HEAD_WORDS: usize = 32;
/// 撮った順の番号（1 始まり。ゲームのスレッドが振る）。
pub const H_SERIAL: usize = 0;
/// 撮影の理由のビット（`higanr_core::script` の `START`〜`WAIT`）。
pub const H_REASON: usize = 1;
pub const H_STEP: usize = 2;
/// 要求を置いた tick の番号 k。
pub const H_K: usize = 3;
/// 描画のときの `stub::SEQ`（tick k の後の描画なら k + 1。`render_only::draw_tag_after_tick`）。
pub const H_DRAW_SEQ: usize = 4;
/// 描画の前に要求がまとめられた回数（同じ描画の前に 2 つの tick が要求を置いたら 1）。
pub const H_MERGED: usize = 5;
/// 失敗の理由（`Fail::code`。0 はコピーできた）。
pub const H_FAIL: usize = 6;
/// 呼んだ関数のビット（`C_*`）。
pub const H_CALLS: usize = 7;
pub const H_HR_GETBACKBUFFER: usize = 8;
pub const H_HR_GETDESC: usize = 9;
pub const H_HR_LOCKRECT: usize = 10;
pub const H_HR_UNLOCKRECT: usize = 11;
/// Release が返した参照の数。
pub const H_RELEASE: usize = 12;
/// D3DSURFACE_DESC の 8 語（Format・Type・Usage・Pool・Size・MultiSampleType・Width・Height）。
pub const H_DESC: usize = 13;
/// present parameters の 3 語（幅・高さ・形式）。
pub const H_PP: usize = 21;
/// LockRect が返した pitch。
pub const H_PITCH: usize = 24;
/// コピーした幅・高さ・形式・バイト数（コピーできたときだけ。置き場の画素は幅 × 1 画素のバイト数で詰めてある）。
pub const H_WIDTH: usize = 25;
pub const H_HEIGHT: usize = 26;
pub const H_FORMAT: usize = 27;
pub const H_BYTES: usize = 28;
/// GetBackBuffer から Release までの TSC と、LockRect の後のコピーだけの TSC（どちらも差。32 ビットで飽和）。
pub const H_TSC_TOTAL: usize = 29;
pub const H_TSC_COPY: usize = 30;
/// デバイスのアドレス（`[D3D8_DEVICE_PTR]` の値）。
pub const H_DEVICE: usize = 31;
const _: () = assert!(H_DESC + 8 == H_PP && H_PP + 3 == H_PITCH && H_DEVICE < HEAD_WORDS);

/// 呼んだ関数のビット（`H_CALLS`）。「GetBackBuffer が通ったら、どの経路でも Release は 1 回だけ」を後から見るため。
pub const C_GETBACKBUFFER: u32 = 1;
pub const C_GETDESC: u32 = 2;
pub const C_LOCKRECT: u32 = 4;
pub const C_UNLOCKRECT: u32 = 8;
pub const C_RELEASE: u32 = 16;

/// 撮れなかった理由（`H_FAIL`）。どれでも置き場は「できた」にして出す（先頭だけが意味を持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fail {
    Ok,
    /// `[D3D8_DEVICE_PTR]` が 0。
    NoDevice,
    GetBackBuffer,
    /// GetBackBuffer は通ったが面が NULL。
    NullSurface,
    GetDesc,
    /// 幅・高さが present parameters と違う。
    SizeMismatch,
    /// 幅か高さが 0。
    ZeroSize,
    /// 読めない画素の形式。
    UnknownFormat,
    /// 置き場に収まらない。
    TooBig,
    /// 置き場が用意されていない（設置の前に確保していない）。
    NoBuffer,
    LockRect,
    /// LockRect は通ったが、pBits が NULL か pitch が 1 行ぶんに満たない。
    BadLock,
}

impl Fail {
    pub fn code(self) -> u32 {
        match self {
            Fail::Ok => 0,
            Fail::NoDevice => 1,
            Fail::GetBackBuffer => 2,
            Fail::NullSurface => 3,
            Fail::GetDesc => 4,
            Fail::SizeMismatch => 5,
            Fail::ZeroSize => 6,
            Fail::UnknownFormat => 7,
            Fail::TooBig => 8,
            Fail::NoBuffer => 9,
            Fail::LockRect => 10,
            Fail::BadLock => 11,
        }
    }

    pub fn from_code(c: u32) -> Option<Self> {
        let all = [
            Fail::Ok,
            Fail::NoDevice,
            Fail::GetBackBuffer,
            Fail::NullSurface,
            Fail::GetDesc,
            Fail::SizeMismatch,
            Fail::ZeroSize,
            Fail::UnknownFormat,
            Fail::TooBig,
            Fail::NoBuffer,
            Fail::LockRect,
            Fail::BadLock,
        ];
        all.into_iter().find(|f| f.code() == c)
    }

    pub fn name(self) -> &'static str {
        match self {
            Fail::Ok => "ok",
            Fail::NoDevice => "no_device",
            Fail::GetBackBuffer => "getbackbuffer",
            Fail::NullSurface => "null_surface",
            Fail::GetDesc => "getdesc",
            Fail::SizeMismatch => "size_mismatch",
            Fail::ZeroSize => "zero_size",
            Fail::UnknownFormat => "unknown_format",
            Fail::TooBig => "too_big",
            Fail::NoBuffer => "no_buffer",
            Fail::LockRect => "lockrect",
            Fail::BadLock => "bad_lock",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPlan {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /// 1 行のバイト数（幅 × 1 画素のバイト数。置き場ではこの幅で詰める）。
    pub row: u32,
    pub total: u32,
}

/// GetDesc の 8 語と present parameters の 3 語から、コピーしてよいかを決める（幅・高さが present parameters と同じで、形式が分かり、
/// 置き場に収まるときだけ）。
pub fn plan_copy(desc: &[u32; 8], pp: &[u32; 3], cap: usize) -> Result<CopyPlan, Fail> {
    let (format, width, height) = (desc[0], desc[6], desc[7]);
    if (width, height) != (pp[0], pp[1]) {
        return Err(Fail::SizeMismatch);
    }
    if width == 0 || height == 0 {
        return Err(Fail::ZeroSize);
    }
    let bpp = bytes_per_pixel(format).ok_or(Fail::UnknownFormat)?;
    let row = width.checked_mul(bpp).ok_or(Fail::TooBig)?;
    let total = row.checked_mul(height).ok_or(Fail::TooBig)?;
    if total as usize > cap {
        return Err(Fail::TooBig);
    }
    Ok(CopyPlan {
        width,
        height,
        format,
        row,
        total,
    })
}

/// 撮影の理由の名前（ファイル名と `_shots.csv`）。ビットの小さい順に `+` でつなぐ（`pre+post`）。
pub fn reason_name(bits: u32) -> String {
    let names = [
        (START as u32, "start"),
        (PRE as u32, "pre"),
        (POST as u32, "post"),
        (TRANSITION as u32, "trans"),
        (SETTLE as u32, "settle"),
        (ABORT as u32, "abort"),
        (END as u32, "end"),
        (WAIT as u32, "wait"),
    ];
    let parts: Vec<&str> = names
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| *n)
        .collect();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join("+")
    }
}

pub fn shot_suffix(n: u32, k: u32, reason: u32) -> String {
    format!("_shot_{n:04}_k{k}_{}.png", reason_name(reason))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotHead(pub [u32; HEAD_WORDS]);

impl ShotHead {
    pub fn get(&self, i: usize) -> u32 {
        self.0.get(i).copied().unwrap_or(0)
    }
    pub fn fail(&self) -> Option<Fail> {
        Fail::from_code(self.get(H_FAIL))
    }
    pub fn desc(&self) -> [u32; 8] {
        core::array::from_fn(|i| self.get(H_DESC + i))
    }
    pub fn pp(&self) -> [u32; 3] {
        core::array::from_fn(|i| self.get(H_PP + i))
    }
    pub fn release_balanced(&self) -> bool {
        let c = self.get(H_CALLS);
        let gbb_ok = c & C_GETBACKBUFFER != 0 && (self.get(H_HR_GETBACKBUFFER) as i32) >= 0;
        let released = c & C_RELEASE != 0;
        let null_surface = self.fail() == Some(Fail::NullSurface);
        released == (gbb_ok && !null_surface)
    }

    /// 置き場の画素（`pixels`。幅 × 1 画素のバイト数で詰めたもの）を PNG へ。コピーできていない先頭なら `Err`。
    pub fn to_png(self, pixels: &[u8]) -> Result<Vec<u8>, String> {
        if self.fail() != Some(Fail::Ok) {
            return Err("コピーできていない".to_string());
        }
        let (w, h, f) = (self.get(H_WIDTH), self.get(H_HEIGHT), self.get(H_FORMAT));
        let bpp = bytes_per_pixel(f).ok_or_else(|| format!("知らない形式 {f}"))?;
        let rgb = to_rgb(f, w, h, w.saturating_mul(bpp), pixels).map_err(|e| e.to_string())?;
        encode_png(w, h, &rgb).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShotCounters {
    pub placed: u32,
    pub merged: u32,
    pub dropped_off: u32,
    pub taken: u32,
    pub no_free: u32,
    /// 描画の入口でパニックを受けた回数（1 回で撮影は止まる）。
    pub panicked: u32,
}

pub const SHOTS_CSV_HEADER: &str = "n,serial,k,draw_seq,reason,step,merged,result,width,height,format,pitch,hr_getbackbuffer,hr_getdesc,hr_lockrect,hr_unlockrect,release_ref,calls,device,desc_format,desc_type,desc_usage,desc_pool,desc_size,desc_multisample,desc_width,desc_height,pp_width,pp_height,pp_format,total_tsc,copy_tsc,total_us,copy_us,file";

fn hex(v: u32) -> String {
    format!("0x{v:08X}")
}

fn us_text(tsc: u32, freq: Option<f64>) -> String {
    freq.map_or(String::new(), |f| {
        format!("{:.1}", tsc_to_us(tsc as u64, f))
    })
}

#[derive(Debug, Clone, Default)]
pub struct ShotStats {
    /// 取り出した置き場（撮れた・撮れなかったの両方）の数。`_shots.csv` の通し番号。
    pub taken: u32,
    pub ok: u32,
    pub written: u32,
    /// 書けなかった（PNG にできない・ファイルを開けない / 書けない）。
    pub write_failed: u32,
    /// 上限（400 枚）を超えて書かなかった。
    pub over_limit: u32,
    /// 失敗の理由ごとの数（コピーできなかったもの）。
    pub fails: BTreeMap<&'static str, u32>,
    pub unlock_failed: u32,
    /// 「Release は 1 回だけ」が崩れた先頭の数。
    pub release_unbalanced: u32,
    pub total_tsc: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// 撮れたので PNG に書く（本ログの名前の先頭に付ける尻）。
    Write(String),
    /// 撮れたが上限（400 枚）を超えたので書かない。
    OverLimit,
    /// 撮れなかった（先頭の失敗の理由を数えた）。
    Failed,
}

impl ShotStats {
    /// 1 枚ぶんを数え、通し番号とどうするかを返す。書けたかは呼び出し側が `written` / `write_failed` に足す。
    pub fn take(&mut self, head: &ShotHead) -> (u32, Taken) {
        self.taken += 1;
        let n = self.taken;
        let fail = head.fail();
        if !head.release_balanced() {
            self.release_unbalanced += 1;
        }
        if (head.get(H_CALLS) & C_UNLOCKRECT != 0) && (head.get(H_HR_UNLOCKRECT) as i32) < 0 {
            self.unlock_failed += 1;
        }
        match fail {
            Some(Fail::Ok) => {
                self.ok += 1;
                self.total_tsc.push(head.get(H_TSC_TOTAL));
                if self.written + self.write_failed >= SHOT_FILE_LIMIT {
                    self.over_limit += 1;
                    return (n, Taken::OverLimit);
                }
                (
                    n,
                    Taken::Write(shot_suffix(n, head.get(H_K), head.get(H_REASON))),
                )
            }
            Some(f) => {
                *self.fails.entry(f.name()).or_default() += 1;
                (n, Taken::Failed)
            }
            None => {
                *self.fails.entry("unknown_code").or_default() += 1;
                (n, Taken::Failed)
            }
        }
    }

    /// `_shots.csv` の 1 行。`file` は書いた PNG の名前（書かなかったら空、上限なら `over_limit`）。
    pub fn row(n: u32, head: &ShotHead, freq: Option<f64>, file: &str) -> String {
        let result = head.fail().map_or("unknown_code", Fail::name);
        let d = head.desc();
        let pp = head.pp();
        format!(
            "{n},{},{},{},{},{},{},{result},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{file}",
            head.get(H_SERIAL),
            head.get(H_K),
            head.get(H_DRAW_SEQ),
            reason_name(head.get(H_REASON)),
            head.get(H_STEP),
            head.get(H_MERGED),
            head.get(H_WIDTH),
            head.get(H_HEIGHT),
            head.get(H_FORMAT),
            head.get(H_PITCH) as i32,
            hex(head.get(H_HR_GETBACKBUFFER)),
            hex(head.get(H_HR_GETDESC)),
            hex(head.get(H_HR_LOCKRECT)),
            hex(head.get(H_HR_UNLOCKRECT)),
            head.get(H_RELEASE),
            head.get(H_CALLS),
            hex(head.get(H_DEVICE)),
            d[0],
            d[1],
            d[2],
            d[3],
            d[4],
            d[5],
            d[6],
            d[7],
            pp[0],
            pp[1],
            pp[2],
            head.get(H_TSC_TOTAL),
            head.get(H_TSC_COPY),
            us_text(head.get(H_TSC_TOTAL), freq),
            us_text(head.get(H_TSC_COPY), freq),
        )
    }

    pub fn write_summary(&self, out: &mut dyn Write, c: &ShotCounters, freq: Option<f64>) {
        let _ = writeln!(
            out,
            "撮影: 要求 {}（まとめた {}・撮影が無効で捨てた {}）/ 描画の入口が取り出した {} / 空きなし {} / 撮影のパニック {}",
            c.placed, c.merged, c.dropped_off, c.taken, c.no_free, c.panicked
        );
        let fails = if self.fails.is_empty() {
            "なし".to_string()
        } else {
            self.fails
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" / ")
        };
        let _ = writeln!(
            out,
            "撮影: 読み取りのスレッドが受け取った {} / 撮れた {} / 書いた {} / 書けなかった {} / 上限（{SHOT_FILE_LIMIT} 枚）で書かなかった {} / 撮れなかった（理由ごと）{fails} / UnlockRect の失敗 {} / Release の数の崩れ {}",
            self.taken,
            self.ok,
            self.written,
            self.write_failed,
            self.over_limit,
            self.unlock_failed,
            self.release_unbalanced
        );
        let _ = match (freq, self.total_tsc.is_empty()) {
            (_, true) => writeln!(
                out,
                "撮影: コピーにかかった時間は測れていない（撮れた 0 枚）"
            ),
            (None, false) => writeln!(
                out,
                "撮影: コピーにかかった時間は TSC の周波数が求まらないので出さない（撮れた {} 枚）",
                self.total_tsc.len()
            ),
            (Some(f), false) => {
                let mut v = self.total_tsc.clone();
                v.sort_unstable();
                let us = |t: u32| tsc_to_us(t as u64, f);
                writeln!(
                    out,
                    "撮影: コピーにかかった時間（GetBackBuffer から Release まで。{} 枚）最小 {:.1} µs / 中央 {:.1} µs / 最大 {:.1} µs",
                    v.len(),
                    us(v[0]),
                    us(v[v.len() / 2]),
                    us(v[v.len() - 1])
                )
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::shot::{D3DFMT_R5G6B5, D3DFMT_X8R8G8B8};

    fn desc(format: u32, w: u32, h: u32) -> [u32; 8] {
        // Size は判断に使わない語。桁あふれの件（幅 0x8000_0000）でも作れるよう wrapping で。
        [format, 1, 1, 0, w.wrapping_mul(h).wrapping_mul(4), 0, w, h]
    }

    #[test]
    fn plan_copy_decides_like_the_design() {
        let pp = [640, 480, D3DFMT_X8R8G8B8];
        assert_eq!(
            plan_copy(&desc(D3DFMT_X8R8G8B8, 640, 480), &pp, SHOT_CAP),
            Ok(CopyPlan {
                width: 640,
                height: 480,
                format: D3DFMT_X8R8G8B8,
                row: 2560,
                total: SHOT_CAP as u32
            })
        );
        assert_eq!(
            plan_copy(&desc(D3DFMT_R5G6B5, 640, 480), &pp, SHOT_CAP).map(|p| p.row),
            Ok(1280)
        );
        assert_eq!(
            plan_copy(&desc(D3DFMT_X8R8G8B8, 641, 480), &pp, SHOT_CAP),
            Err(Fail::SizeMismatch)
        );
        assert_eq!(
            plan_copy(&desc(D3DFMT_X8R8G8B8, 640, 479), &pp, SHOT_CAP),
            Err(Fail::SizeMismatch)
        );
        assert_eq!(
            plan_copy(&desc(99, 640, 480), &pp, SHOT_CAP),
            Err(Fail::UnknownFormat)
        );
        assert_eq!(
            plan_copy(&desc(D3DFMT_X8R8G8B8, 0, 480), &[0, 480, 22], SHOT_CAP),
            Err(Fail::ZeroSize)
        );
        assert_eq!(
            plan_copy(&desc(D3DFMT_X8R8G8B8, 640, 480), &pp, SHOT_CAP - 1),
            Err(Fail::TooBig)
        );
        assert_eq!(
            plan_copy(
                &desc(D3DFMT_X8R8G8B8, 0x8000_0000, 2),
                &[0x8000_0000, 2, 22],
                SHOT_CAP
            ),
            Err(Fail::TooBig)
        );
    }

    #[test]
    fn reason_names_and_file_suffix() {
        assert_eq!(reason_name((PRE | POST) as u32), "pre+post");
        assert_eq!(reason_name((TRANSITION | START) as u32), "start+trans");
        assert_eq!(reason_name(WAIT as u32), "wait");
        assert_eq!(
            reason_name((SETTLE | ABORT | END) as u32),
            "settle+abort+end"
        );
        assert_eq!(reason_name(0), "none");
        assert_eq!(
            shot_suffix(7, 1234, POST as u32),
            "_shot_0007_k1234_post.png"
        );
        for f in [Fail::Ok, Fail::NoDevice, Fail::BadLock, Fail::NoBuffer] {
            assert_eq!(Fail::from_code(f.code()), Some(f));
        }
        assert_eq!(Fail::from_code(99), None);
    }

    fn ok_head() -> ShotHead {
        let mut h = [0u32; HEAD_WORDS];
        h[H_FAIL] = Fail::Ok.code();
        h[H_CALLS] = C_GETBACKBUFFER | C_GETDESC | C_LOCKRECT | C_UNLOCKRECT | C_RELEASE;
        h[H_K] = 50;
        h[H_REASON] = PRE as u32;
        h[H_WIDTH] = 2;
        h[H_HEIGHT] = 1;
        h[H_FORMAT] = D3DFMT_X8R8G8B8;
        h[H_TSC_TOTAL] = 3000;
        ShotHead(h)
    }

    #[test]
    fn release_balance_is_judged_from_the_calls() {
        assert!(ok_head().release_balanced());
        let mut h = ok_head();
        h.0[H_CALLS] &= !C_RELEASE;
        assert!(!h.release_balanced(), "Release を抜いた道");
        let mut g = ShotHead([0; HEAD_WORDS]);
        g.0[H_FAIL] = Fail::GetBackBuffer.code();
        g.0[H_CALLS] = C_GETBACKBUFFER;
        g.0[H_HR_GETBACKBUFFER] = 0x8876_086C; // D3DERR_INVALIDCALL
        assert!(g.release_balanced());
        g.0[H_CALLS] |= C_RELEASE;
        assert!(
            !g.release_balanced(),
            "失敗した GetBackBuffer の後に Release"
        );
        let mut n = ShotHead([0; HEAD_WORDS]);
        n.0[H_FAIL] = Fail::NullSurface.code();
        n.0[H_CALLS] = C_GETBACKBUFFER;
        assert!(n.release_balanced());
    }

    #[test]
    fn stats_limit_fails_and_rows() {
        let mut s = ShotStats::default();
        for i in 0..SHOT_FILE_LIMIT {
            let (n, what) = s.take(&ok_head());
            assert_eq!(n, i + 1);
            assert_eq!(what, Taken::Write(shot_suffix(n, 50, PRE as u32)));
            s.written += 1;
        }
        let (n, what) = s.take(&ok_head());
        assert_eq!((n, what), (SHOT_FILE_LIMIT + 1, Taken::OverLimit));
        assert_eq!(s.over_limit, 1);
        let mut f = ShotHead([0; HEAD_WORDS]);
        f.0[H_FAIL] = Fail::LockRect.code();
        f.0[H_CALLS] = C_GETBACKBUFFER | C_GETDESC | C_LOCKRECT | C_RELEASE;
        f.0[H_TSC_TOTAL] = 999_999;
        let (_, what) = s.take(&f);
        assert_eq!(what, Taken::Failed);
        assert_eq!(s.fails.get("lockrect"), Some(&1));
        assert_eq!(
            s.total_tsc.len(),
            SHOT_FILE_LIMIT as usize + 1,
            "撮れなかったものの時間を取った"
        );
        let row = ShotStats::row(3, &ok_head(), Some(1e9), "x.png");
        assert!(
            row.starts_with("3,0,50,0,pre,0,0,ok,2,1,22,0,0x00000000"),
            "{row}"
        );
        assert!(row.ends_with(",3.0,0.0,x.png"), "{row}");
        assert_eq!(row.split(',').count(), SHOTS_CSV_HEADER.split(',').count());
        let mut out = Vec::new();
        s.write_summary(&mut out, &ShotCounters::default(), Some(1e9));
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("撮れなかった（理由ごと）lockrect 1"), "{log}");
        assert!(
            log.contains("最小 3.0 µs / 中央 3.0 µs / 最大 3.0 µs"),
            "{log}"
        );
    }

    #[test]
    fn to_png_roundtrips_a_two_pixel_picture() {
        let h = ok_head();
        // B,G,R,X: 赤・青。
        let px = [0, 0, 0xFF, 0, 0xFF, 0, 0, 0];
        let png_bytes = h.to_png(&px).expect("PNG");
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("read_info");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..6], &[255, 0, 0, 0, 0, 255]);
        let mut bad = ok_head();
        bad.0[H_FAIL] = Fail::TooBig.code();
        assert!(bad.to_png(&px).is_err());
    }
}
