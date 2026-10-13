//! 認証タグ（HMAC-SHA256 の先頭 16 バイト。暗号化はしない）。
//!
//! - 秘密: 接続を始める側が引く 16 バイトの乱数（dll が OS の乱数を引いて渡す）。
//! - 鍵: `K = HMAC-SHA256(key = 秘密, msg = "HGRT v0 auth" ‖ session_id（LE 4 B）)`。接続ごとに 1 回（`AuthKey::derive`）。
//! - タグ: `HMAC-SHA256(K, ヘッダ ‖ 本文)` の先頭 16 バイト。比べは crate の定数時間の検証（`Mac::verify_truncated_left`）。
//!
//! 暗号は自作しない: RustCrypto の `hmac` 0.13 と `sha2` 0.11（どちらも MIT OR Apache-2.0）を呼ぶ形をここにまとめる。
//!
//! ヒープを使わない: タグはゲームスレッドで 1 通ごとに作る・比べる。鍵を入れ終えた HMAC の状態（数百バイト）を
//! `AuthKey` に持ち、1 通ごとにそれをスタックの上でコピーしてから本文を足す。鍵から作り直すと圧縮が 2 回余計にかかる。
//!
//! 鍵の長さ: HMAC は 64 バイト（SHA-256 のブロック長）より短い鍵を 0 で足して使う（RFC 2104）。ここで入れる鍵は
//! 16 B と 32 B だけなので、自分で 64 B の配列にして `KeyInit::new` に渡す（`new_from_slice` を通さず、失敗の枝も unwrap も要らない）。

use hmac::digest::Key;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const SECRET_LEN: usize = 16;
pub const TAG_LEN: usize = 16;
/// 鍵を作るときに秘密の後ろに置く文字。プロトコルのバージョン（`wire::PROTO_VER`）を上げても変えない。
/// 文字をバージョンごとに変えると、違うバージョンの相手のパケットはタグ違いで落ち、「コードの取り違え」と「相手のバージョンが違う」を見分けられない。
/// 保てば、タグの合ったパケットのヘッダのバージョンを読んで繋がる前に「相手のバージョンが違う」と言える（`wire::decode` の `BadVersion`）。
/// 文字の中の "v0" は鍵の作り方のバージョンで、プロトコルのバージョンではない。
pub const KEY_LABEL: &[u8] = b"HGRT v0 auth";

/// SHA-256 のブロック長（B）。この長さ以下の鍵は後ろに 0 を足して使う。
const BLOCK_LEN: usize = 64;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Secret(pub [u8; SECRET_LEN]);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Secret {
    /// 16 進 32 桁（大文字・小文字どちらも。`0x` は付けない）から読む。
    pub fn from_hex(s: &str) -> Result<Secret, String> {
        let b = s.as_bytes();
        if b.len() != SECRET_LEN * 2 {
            return Err(format!(
                "秘密は 16 進 {} 桁（いま {} 文字）",
                SECRET_LEN * 2,
                b.len()
            ));
        }
        let mut out = [0u8; SECRET_LEN];
        for (i, o) in out.iter_mut().enumerate() {
            let hi = hex_val(b[2 * i]);
            let lo = hex_val(b[2 * i + 1]);
            match (hi, lo) {
                (Some(h), Some(l)) => *o = (h << 4) | l,
                _ => return Err(format!("秘密の {} 文字目の辺りが 16 進でない", 2 * i + 1)),
            }
        }
        Ok(Secret(out))
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(SECRET_LEN * 2);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 64 B 以下の鍵を 0 で 64 B にし、鍵を入れ終えた HMAC を作る。64 B を越える鍵は渡さない
/// （越えた分は切り捨てるので RFC 2104 と違う値になる。`hmac_matches_rfc4231_samples` が 64 B 以下だけを見るのはそのため）。
fn keyed(key: &[u8]) -> HmacSha256 {
    let mut k = [0u8; BLOCK_LEN];
    for (d, s) in k.iter_mut().zip(key.iter()) {
        *d = *s;
    }
    HmacSha256::new(&Key::<HmacSha256>::from(k))
}

/// 1 回の HMAC-SHA256（32 B）。接続コードの返す側のタグ（`code.rs`）と検査が使う。
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = keyed(key);
    for p in parts {
        m.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&m.finalize().into_bytes());
    out
}

#[derive(Clone)]
pub struct AuthKey {
    mac: HmacSha256,
    /// K そのもの（接続コードの返す側のタグにも使う）。
    k: [u8; 32],
}

impl std::fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthKey(..)")
    }
}

impl AuthKey {
    pub fn derive(secret: &Secret, session_id: u32) -> AuthKey {
        let k = hmac_sha256(&secret.0, &[KEY_LABEL, &session_id.to_le_bytes()]);
        AuthKey { mac: keyed(&k), k }
    }

    pub(crate) fn k(&self) -> &[u8; 32] {
        &self.k
    }

    pub fn tag(&self, data: &[u8]) -> [u8; TAG_LEN] {
        let mut m = self.mac.clone();
        m.update(data);
        let full = m.finalize().into_bytes();
        let mut out = [0u8; TAG_LEN];
        for (d, s) in out.iter_mut().zip(full.iter()) {
            *d = *s;
        }
        out
    }

    /// `tag` が `data` のタグか。長さが `TAG_LEN` でなければ偽。
    pub fn verify(&self, data: &[u8], tag: &[u8]) -> bool {
        if tag.len() != TAG_LEN {
            return false;
        }
        let mut m = self.mac.clone();
        m.update(data);
        m.verify_truncated_left(tag).is_ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! 期待値はこの crate の外で作った: RFC 4231 §4 の標本と、Python 3 の標準の `hmac` / `hashlib` で計算した値。
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 4231 §4.2〜§4.6（Test Case 1〜5。鍵 4〜25 B）。Test Case 6・7 は鍵が 131 B で `keyed` の前提の外なので入れない。
    #[test]
    fn hmac_matches_rfc4231_samples() {
        let cases: [(Vec<u8>, &[u8], &str); 5] = [
            (
                vec![0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                &[0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                (1u8..=25).collect(),
                &[0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                vec![0x0c; 20],
                b"Test With Truncation",
                "a3b6167473100ee06e0c796c2955552b",
            ),
        ];
        for (i, (key, data, want)) in cases.iter().enumerate() {
            let got = hmac_sha256(key, &[data]);
            let want = hex(want);
            assert_eq!(&got[..want.len()], &want[..], "Test Case {}", i + 1);
        }
        assert_eq!(
            hmac_sha256(b"Jefe", &[b"what do ya ", b"want for nothing?"]),
            hmac_sha256(b"Jefe", &[b"what do ya want for nothing?"])
        );
    }

    /// 秘密 00 01 … 0f、session_id 0x11223344 を、Python の `hmac` で `b"HGRT v0 auth" + session_id(LE 4 B)` に掛けた値。
    const K_GOLDEN: &str = "45c520b62dbb40eebccfe58eb7f51993126eba6c3a93388569ce8549bb720acf";

    /// `wire.rs` の PING の golden の 16 B。
    #[rustfmt::skip]
    pub(crate) const PING_SIGNED: [u8; 16] = [
        0x48, 0x47, 0x52, 0x54, 0x04, 0x08, 0x00, 0x00, 0x44, 0x33, 0x22, 0x11, 0x60, 0xEA, 0x00, 0x00,
    ];
    /// 上の 16 B のタグ（Python の `hmac.new(K, PING_SIGNED, hashlib.sha256).digest()[:16]`）。
    pub(crate) const PING_TAG: &str = "000c227eb9310fbde04aba055ddd2b0a";

    pub(crate) fn golden_secret() -> Secret {
        let mut s = [0u8; SECRET_LEN];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        Secret(s)
    }

    #[test]
    fn key_and_tag_match_values_computed_outside() {
        let key = AuthKey::derive(&golden_secret(), 0x1122_3344);
        assert_eq!(key.k().to_vec(), hex(K_GOLDEN));
        assert_eq!(key.tag(&PING_SIGNED).to_vec(), hex(PING_TAG));
        assert!(key.verify(&PING_SIGNED, &hex(PING_TAG)));
    }

    #[test]
    fn verify_refuses_wrong_tags_keys_and_lengths() {
        let key = AuthKey::derive(&golden_secret(), 0x1122_3344);
        let tag = key.tag(&PING_SIGNED);
        assert!(key.verify(&PING_SIGNED, &tag));
        for bit in 0..(TAG_LEN * 8) {
            let mut t = tag;
            t[bit / 8] ^= 1 << (bit % 8);
            assert!(
                !key.verify(&PING_SIGNED, &t),
                "タグの {bit} bit 目を反転しても通った"
            );
        }
        for n in 0..TAG_LEN {
            assert!(
                !key.verify(&PING_SIGNED, &tag[..n]),
                "{n} B に切ったタグが通った"
            );
        }
        let mut long = tag.to_vec();
        long.push(0);
        assert!(!key.verify(&PING_SIGNED, &long));
        let mut data = PING_SIGNED;
        data[13] ^= 0x01;
        assert!(!key.verify(&data, &tag), "本文を変えても通った");
        let other_session = AuthKey::derive(&golden_secret(), 0x1122_3345);
        assert!(!other_session.verify(&PING_SIGNED, &tag));
        let mut s2 = golden_secret();
        s2.0[15] ^= 0x80;
        assert!(!AuthKey::derive(&s2, 0x1122_3344).verify(&PING_SIGNED, &tag));
    }

    /// 1 通あたりのタグの所要を測る道具なので `#[ignore]`。走らせ方: `cargo test -p higanr_core --release --lib -- --ignored --nocapture measure_tag`。
    /// 落ちるのは 1 通が 1 ms を越えたとき（建て方の取り違え）だけ。
    #[test]
    #[ignore = "所要を測るだけ。手で --release --ignored --nocapture で走らせる"]
    fn measure_tag_cost_per_datagram() {
        let key = AuthKey::derive(&golden_secret(), 0x1122_3344);
        for (what, len) in [("INPUT 32 語 92 B", 92usize), ("PING 16 B", 16)] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let n = 200_000u32;
            let t0 = std::time::Instant::now();
            let mut acc = 0u8;
            for _ in 0..n {
                acc ^= key.tag(std::hint::black_box(&data))[0];
            }
            let tag_ns = t0.elapsed().as_nanos() / u128::from(n);
            let tag = key.tag(&data);
            let t0 = std::time::Instant::now();
            let mut ok = 0u32;
            for _ in 0..n {
                ok += u32::from(key.verify(std::hint::black_box(&data), &tag));
            }
            let verify_ns = t0.elapsed().as_nanos() / u128::from(n);
            println!("{what}: タグ {tag_ns} ns / 比べ {verify_ns} ns（{n} 回の平均。{acc}）");
            assert_eq!(ok, n);
            assert!(tag_ns < 1_000_000 && verify_ns < 1_000_000);
        }
        let t0 = std::time::Instant::now();
        for s in 0..10_000u32 {
            std::hint::black_box(AuthKey::derive(&golden_secret(), s));
        }
        println!(
            "鍵を作る: {} ns（接続ごとに 1 回）",
            t0.elapsed().as_nanos() / 10_000
        );
    }

    #[test]
    fn secret_hex_round_trip_and_refusals() {
        let s = golden_secret();
        assert_eq!(s.to_hex(), "000102030405060708090a0b0c0d0e0f");
        assert_eq!(Secret::from_hex("000102030405060708090A0B0C0D0E0F"), Ok(s));
        for bad in [
            "",
            "000102030405060708090a0b0c0d0e0",
            "000102030405060708090a0b0c0d0e0f0",
            "0x0102030405060708090a0b0c0d0e0f",
            "000102030405060708090a0b0c0d0e0g",
            "00010203040506070809 a0b0c0d0e0f",
        ] {
            assert!(Secret::from_hex(bad).is_err(), "{bad:?} を通した");
        }
        assert_eq!(format!("{s:?}"), "Secret(..)", "秘密をログに出した");
        let k = AuthKey::derive(&s, 1);
        assert_eq!(format!("{k:?}"), "AuthKey(..)");
    }
}
