//! ゲームのプロセスのANSIコードページ（日本語のWindowsはCP932・中国語のWindowsはCP936ほか）の読み書き。
//! 保存はゲーム自身の`CreateFileA`を通る。パスのバイト列はOSがそのコードページで読む。
//! コードページの決め打ちを避けるため変換は`Ansi`で受け取る。本物（`GetACP`の値に従う）はdll、検査は`EncodingAnsi`（Shift_JIS・GBK）。
#![forbid(unsafe_code)]

use encoding_rs::{Encoding, GBK, SHIFT_JIS};

/// ANSIのバイト列とUTF-8の変換。どちらの向きも変換が元に戻らないときや誤りがあるときは`None`（代替文字へ黙って落とさない）。
pub trait Ansi {
    /// `s`をANSIのバイト列にする。表せない文字や別の文字に落ちる文字（CP932の`¥` → `\`など）があれば`None`。
    fn encode(&self, s: &str) -> Option<Vec<u8>>;
    /// ANSIのバイト列を読む。不正なバイト列があれば`None`。
    fn decode(&self, b: &[u8]) -> Option<String>;
}

/// `encoding_rs`の符号化で`Ansi`を満たす。検査用とdllの検査の代役。
#[derive(Debug, Clone, Copy)]
pub struct EncodingAnsi(&'static Encoding);

impl EncodingAnsi {
    /// 日本語のWindowsのCP932。
    pub fn shift_jis() -> Self {
        EncodingAnsi(SHIFT_JIS)
    }

    /// 中国語（簡体字）のWindowsのCP936の代わり。
    pub fn gbk() -> Self {
        EncodingAnsi(GBK)
    }
}

impl Ansi for EncodingAnsi {
    fn encode(&self, s: &str) -> Option<Vec<u8>> {
        let (bytes, _, had_errors) = self.0.encode(s);
        if had_errors {
            return None;
        }
        let (back, decode_errors) = self.0.decode_without_bom_handling(&bytes);
        (!decode_errors && back == s).then(|| bytes.into_owned())
    }

    fn decode(&self, b: &[u8]) -> Option<String> {
        let (s, had_errors) = self.0.decode_without_bom_handling(b);
        (!had_errors).then(|| s.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_jis_and_gbk_read_the_same_bytes_differently() {
        let sj = EncodingAnsi::shift_jis();
        let gb = EncodingAnsi::gbk();
        assert_eq!(sj.decode(&[0xB6, 0xAB]).as_deref(), Some("ｶｫ"));
        assert_eq!(gb.decode(&[0xB6, 0xAB]).as_deref(), Some("东"));
        assert_eq!(gb.encode("东方"), Some(vec![0xB6, 0xAB, 0xB7, 0xBD]));
        assert_eq!(sj.encode("东"), None);
    }

    #[test]
    fn unrepresentable_or_falling_chars_are_refused() {
        for a in [EncodingAnsi::shift_jis(), EncodingAnsi::gbk()] {
            assert_eq!(a.encode("a😀"), None);
            assert_eq!(a.encode("¥"), None);
            assert_eq!(a.encode(""), Some(vec![]));
            assert_eq!(a.decode(&[0x81]), None);
        }
    }
}
