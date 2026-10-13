//! D3D8 のバックバッファの画素（B,G,R,X などの並び）→ RGB8 への変換と、PNG への符号化。
//!
//! 32bit は B,G,R,X の並び、16bit は R5G6B5 / X1R5G5B5 / A1R5G5B5 の並びとビットの広げ方で変換する。
//! ゲームのメモリには触らない（渡されたバイト列を読むだけ）。ゲームスレッドの外の読み取りのスレッドで呼ぶので
//! ヒープを使ってよい。どんなバイト列・大きさを渡されてもパニックしない（添字は `get`、掛け算は `checked_mul`）。

use std::fmt;

/// D3DFORMAT の値（D3D8 の定数）。バックバッファとして読める形式だけを挙げる。ほかは撮らず、形式の番号を記録する。
pub const D3DFMT_A8R8G8B8: u32 = 21;
pub const D3DFMT_X8R8G8B8: u32 = 22;
pub const D3DFMT_R5G6B5: u32 = 23;
pub const D3DFMT_X1R5G5B5: u32 = 24;
pub const D3DFMT_A1R5G5B5: u32 = 25;

/// 変換・符号化ができなかった理由。ゲームのメモリの壊れ方や D3D8 の想定外の返り値をそのまま反映し、直さずに返す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShotError {
    /// 読める形式（上の 5 つ）のどれでもない。
    UnknownFormat { format: u32 },
    /// 幅か高さが 0。
    ZeroSize { width: u32, height: u32 },
    /// `pitch` が 1 行ぶん（`width * bytes_per_pixel`）に満たない。
    PitchTooSmall { pitch: u32, needed: u32 },
    /// `data` が `(height-1)*pitch + 1行ぶん` に満たない。
    DataTooShort { needed: usize, got: usize },
    /// 大きさの計算（`width * height * 3` など）が `usize`（32bit）の範囲を超える。
    Overflow,
    /// `encode_png` に渡した `rgb` の長さが `width * height * 3` と合わない。
    RgbLenMismatch { expected: usize, got: usize },
    /// `png` クレートの符号化そのものが失敗した（`EncodingError` の文言をそのまま包む）。
    PngEncode(String),
}

impl fmt::Display for ShotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShotError::UnknownFormat { format } => {
                write!(f, "知らない画素の形式です（D3DFORMAT={format}）")
            }
            ShotError::ZeroSize { width, height } => {
                write!(f, "幅か高さが 0 です（{width}x{height}）")
            }
            ShotError::PitchTooSmall { pitch, needed } => {
                write!(
                    f,
                    "pitch が 1 行ぶんに満たない（pitch={pitch}、必要={needed}）"
                )
            }
            ShotError::DataTooShort { needed, got } => {
                write!(
                    f,
                    "data が足りない（必要={needed} バイト、実際={got} バイト）"
                )
            }
            ShotError::Overflow => write!(f, "大きさの計算が桁あふれした"),
            ShotError::RgbLenMismatch { expected, got } => {
                write!(
                    f,
                    "rgb の長さが width*height*3 と合わない（期待={expected}、実際={got}）"
                )
            }
            ShotError::PngEncode(msg) => write!(f, "PNG の符号化に失敗した: {msg}"),
        }
    }
}

/// 形式ごとの 1 画素のバイト数。読めない形式は `None`。
pub fn bytes_per_pixel(format: u32) -> Option<u32> {
    match format {
        D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8 => Some(4),
        D3DFMT_R5G6B5 | D3DFMT_X1R5G5B5 | D3DFMT_A1R5G5B5 => Some(2),
        _ => None,
    }
}

/// 5 ビットの値を 8 ビットへ広げる（`(v<<3)|(v>>2)`）。
fn expand5(v: u16) -> u8 {
    ((v << 3) | (v >> 2)) as u8
}

/// 6 ビットの値を 8 ビットへ広げる（`(v<<2)|(v>>4)`）。
fn expand6(v: u16) -> u8 {
    ((v << 2) | (v >> 4)) as u8
}

/// 1 画素（形式ごとの生バイト列）を RGB8 の 3 バイトへ変換する。`px` は `bytes_per_pixel(format)` の長さぴったり。
fn pixel_to_rgb(format: u32, px: &[u8]) -> Option<[u8; 3]> {
    match format {
        // メモリ上は B,G,R,X（小さい端の 0xXXRRGGBB）の並び。
        D3DFMT_A8R8G8B8 | D3DFMT_X8R8G8B8 => {
            let b = *px.first()?;
            let g = *px.get(1)?;
            let r = *px.get(2)?;
            Some([r, g, b])
        }
        D3DFMT_R5G6B5 => {
            let v = u16::from_le_bytes([*px.first()?, *px.get(1)?]);
            let r5 = (v >> 11) & 0x1F;
            let g6 = (v >> 5) & 0x3F;
            let b5 = v & 0x1F;
            Some([expand5(r5), expand6(g6), expand5(b5)])
        }
        D3DFMT_X1R5G5B5 | D3DFMT_A1R5G5B5 => {
            let v = u16::from_le_bytes([*px.first()?, *px.get(1)?]);
            let r5 = (v >> 10) & 0x1F;
            let g5 = (v >> 5) & 0x1F;
            let b5 = v & 0x1F;
            Some([expand5(r5), expand5(g5), expand5(b5)])
        }
        _ => None,
    }
}

/// バックバッファの生バイト列（`data`。行 `i` は `data[i*pitch ..][.. width*bpp]`）を
/// RGB8（上の行から `width*height*3` バイト）へ変換する。
pub fn to_rgb(
    format: u32,
    width: u32,
    height: u32,
    pitch: u32,
    data: &[u8],
) -> Result<Vec<u8>, ShotError> {
    let bpp = bytes_per_pixel(format).ok_or(ShotError::UnknownFormat { format })?;
    if width == 0 || height == 0 {
        return Err(ShotError::ZeroSize { width, height });
    }
    let row_needed = width.checked_mul(bpp).ok_or(ShotError::Overflow)?;
    if pitch < row_needed {
        return Err(ShotError::PitchTooSmall {
            pitch,
            needed: row_needed,
        });
    }
    // 最後の行だけは pitch の詰め物まで要らない（1 行ぶんで足りる）ので、
    // 必要な長さは `(height-1)*pitch + row_needed`。
    let full_rows = height - 1; // height != 0 を上で確かめ済み
    let required = (full_rows as usize)
        .checked_mul(pitch as usize)
        .and_then(|v| v.checked_add(row_needed as usize))
        .ok_or(ShotError::Overflow)?;
    if data.len() < required {
        return Err(ShotError::DataTooShort {
            needed: required,
            got: data.len(),
        });
    }

    let out_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|v| v.checked_mul(3))
        .ok_or(ShotError::Overflow)?;
    let mut rgb = Vec::with_capacity(out_len);

    for y in 0..height {
        // 理屈のうえではあふれないが、添字と同じく確かめてから使うため `checked_mul` を通す。
        let row_start = (y as usize)
            .checked_mul(pitch as usize)
            .ok_or(ShotError::Overflow)?;
        let row_end = row_start
            .checked_add(row_needed as usize)
            .ok_or(ShotError::Overflow)?;
        let row = data
            .get(row_start..row_end)
            .ok_or(ShotError::DataTooShort {
                needed: row_end,
                got: data.len(),
            })?;
        for x in 0..width {
            let off = (x as usize)
                .checked_mul(bpp as usize)
                .ok_or(ShotError::Overflow)?;
            let off_end = off.checked_add(bpp as usize).ok_or(ShotError::Overflow)?;
            let px = row.get(off..off_end).ok_or(ShotError::DataTooShort {
                needed: row_start + off_end,
                got: data.len(),
            })?;
            let [r, g, b] = pixel_to_rgb(format, px).ok_or(ShotError::UnknownFormat { format })?;
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }
    }
    Ok(rgb)
}

/// RGB8（8bit・3チャンネル）を PNG のバイト列へ符号化する。`png` クレート（MIT OR Apache-2.0）。
pub fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, ShotError> {
    if width == 0 || height == 0 {
        return Err(ShotError::ZeroSize { width, height });
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|v| v.checked_mul(3))
        .ok_or(ShotError::Overflow)?;
    if rgb.len() != expected {
        return Err(ShotError::RgbLenMismatch {
            expected,
            got: rgb.len(),
        });
    }

    let mut buf: Vec<u8> = Vec::new();
    {
        // `&mut Vec<u8>` は `Write` なので、符号化が終われば `buf` の所有権が戻る。
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ShotError::PngEncode(e.to_string()))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| ShotError::PngEncode(e.to_string()))?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    //! 期待値は画素を手で決め打ちした小さな絵で作る（`to_rgb` 自身の式を逆に辿って作らない）。
    use super::*;

    #[test]
    fn x8r8g8b8_2x2_matches_hand_picked_pixels() {
        // 画素 (0,0)=赤 (1,0)=緑 (0,1)=青 (1,1)=白。メモリは B,G,R,X の順。
        // pitch=16（1 行 8 B の画素 + 8 B の詰め物）。最後の行は詰め物を含めなくてよい。
        #[rustfmt::skip]
        let data: [u8; 24] = [
            0x00, 0x00, 0xFF, 0x00,  0x00, 0xFF, 0x00, 0x00, // 行0: 赤, 緑
            0,0,0,0, 0,0,0,0,                                // 行0 の詰め物
            0xFF, 0x00, 0x00, 0x00,  0xFF, 0xFF, 0xFF, 0x00, // 行1: 青, 白（詰め物なし）
        ];
        let rgb = to_rgb(D3DFMT_X8R8G8B8, 2, 2, 16, &data).expect("convert");
        assert_eq!(
            rgb,
            vec![
                255, 0, 0, /**/ 0, 255, 0, /**/ 0, 0, 255, /**/ 255, 255, 255
            ]
        );
    }

    #[test]
    fn r5g6b5_pure_colors_expand_to_full_8bit() {
        // 純粋な赤 → 0xF800、緑 → 0x07E0、青 → 0x001F。5bit/6bit の最大値は 8bit の最大値(255)へ広がる。
        let red: u16 = 0xF800;
        let green: u16 = 0x07E0;
        let blue: u16 = 0x001F;
        let data: Vec<u8> = [red, green, blue, 0x0000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let rgb = to_rgb(D3DFMT_R5G6B5, 2, 2, 4, &data).expect("convert");
        assert_eq!(
            rgb,
            vec![
                255, 0, 0, /**/ 0, 255, 0, /**/ 0, 0, 255, /**/ 0, 0, 0
            ]
        );
    }

    #[test]
    fn x1r5g5b5_pure_colors_expand_to_full_8bit() {
        let red: u16 = 0b0_11111_00000_00000;
        let green: u16 = 0b0_00000_11111_00000;
        let blue: u16 = 0b0_00000_00000_11111;
        let data: Vec<u8> = [red, green, blue, 0x0000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let rgb = to_rgb(D3DFMT_X1R5G5B5, 2, 2, 4, &data).expect("convert");
        assert_eq!(
            rgb,
            vec![
                255, 0, 0, /**/ 0, 255, 0, /**/ 0, 0, 255, /**/ 0, 0, 0
            ]
        );
        let rgb_a = to_rgb(D3DFMT_A1R5G5B5, 2, 2, 4, &data).expect("convert");
        assert_eq!(rgb_a, rgb);
    }

    #[test]
    fn pitch_padding_is_skipped_not_read_as_pixels() {
        // width=1, bpp=4 だが pitch=8。詰め物に、間違えて読むと別の色になる値を置く。
        let data: [u8; 16] = [
            0x00, 0x00, 0xFF, 0x00, /*詰め物*/ 0xAA, 0xBB, 0xCC, 0xDD, // 行0: 赤
            0xFF, 0x00, 0x00, 0x00, /*詰め物*/ 0xAA, 0xBB, 0xCC, 0xDD, // 行1: 青
        ];
        let rgb = to_rgb(D3DFMT_X8R8G8B8, 1, 2, 8, &data).expect("convert");
        assert_eq!(rgb, vec![255, 0, 0, /**/ 0, 0, 255]);
    }

    #[test]
    fn unknown_format_is_rejected() {
        let err = to_rgb(999, 1, 1, 4, &[0, 0, 0, 0]).unwrap_err();
        assert_eq!(err, ShotError::UnknownFormat { format: 999 });
    }

    #[test]
    fn zero_size_is_rejected() {
        assert_eq!(
            to_rgb(D3DFMT_X8R8G8B8, 0, 1, 4, &[]).unwrap_err(),
            ShotError::ZeroSize {
                width: 0,
                height: 1
            }
        );
        assert_eq!(
            to_rgb(D3DFMT_X8R8G8B8, 1, 0, 4, &[]).unwrap_err(),
            ShotError::ZeroSize {
                width: 1,
                height: 0
            }
        );
    }

    #[test]
    fn pitch_smaller_than_one_row_is_rejected() {
        let err = to_rgb(D3DFMT_X8R8G8B8, 2, 1, 4, &[0; 8]).unwrap_err();
        assert_eq!(
            err,
            ShotError::PitchTooSmall {
                pitch: 4,
                needed: 8
            }
        );
    }

    #[test]
    fn data_too_short_is_rejected() {
        let data = vec![0u8; 11];
        let err = to_rgb(D3DFMT_X8R8G8B8, 1, 2, 8, &data).unwrap_err();
        assert_eq!(
            err,
            ShotError::DataTooShort {
                needed: 12,
                got: 11
            }
        );
    }

    #[test]
    fn huge_size_overflow_is_rejected_not_panicking() {
        let err = to_rgb(D3DFMT_X8R8G8B8, u32::MAX, u32::MAX, u32::MAX, &[]).unwrap_err();
        assert_eq!(err, ShotError::Overflow);
    }

    #[test]
    fn encode_png_rejects_wrong_length() {
        let err = encode_png(2, 2, &[0u8; 5]).unwrap_err();
        assert_eq!(
            err,
            ShotError::RgbLenMismatch {
                expected: 12,
                got: 5
            }
        );
    }

    #[test]
    fn encode_png_roundtrips_through_decoder() {
        let data: [u8; 16] = [
            0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF,
            0xFF, 0x00,
        ];
        let rgb = to_rgb(D3DFMT_X8R8G8B8, 2, 2, 8, &data).expect("convert");
        let png_bytes = encode_png(2, 2, &rgb).expect("encode");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("read_info");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("output_buffer_size")];
        let info = reader.next_frame(&mut buf).expect("next_frame");
        let decoded = &buf[..info.buffer_size()];

        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(decoded, rgb.as_slice());
    }

    #[test]
    fn encode_png_rejects_zero_size() {
        let err = encode_png(0, 0, &[]).unwrap_err();
        assert_eq!(
            err,
            ShotError::ZeroSize {
                width: 0,
                height: 0
            }
        );
    }
}
