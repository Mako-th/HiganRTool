//! バージョン 5 の使い捨ての鍵と、便の暗号化（AEAD）。暗号は自作しない: `x25519-dalek`（BSD-3-Clause）・`chacha20poly1305`・`hkdf`
//! （RustCrypto。MIT OR Apache-2.0）を呼ぶ形を 1 か所にまとめる。
//!
//! - **使い捨ての鍵**: X25519。秘密鍵は呼び手（dll）が OS の乱数で引いた 32 B（[`KeySeed`]）から作る（core は乱数を持たない）。
//!   対戦では HELLO / HELLO_ACK、観戦では観戦用の接続コードと SPECTATE_JOIN で公開鍵 32 B を渡し合う。
//! - **向きごとの鍵**: 共有の値 = X25519(自分の秘密鍵, 相手の公開鍵)。`HKDF-SHA256(ikm = 共有の値, salt = K, info = "HGRT v5 keys" ‖
//!   session_id（LE 4 B）‖ 前の公開鍵 ‖ 後ろの公開鍵)` から 64 B を引き、前の 32 B を「前 → 後ろ」、後ろの 32 B を「後ろ → 前」の鍵にする。
//!   「前」は、対戦では role_nonce の大きい側、観戦ではホスト。弱い点（共有の値が 0 になる）を送ってきた相手には鍵を作らない。
//! - **便の形**: ヘッダ 12 B（暗号化しない。追加の認証データ）‖ nonce の欄 8 B（送り手の印 1 B ‖ 通し番号 7 B（LE）。平文）‖
//!   暗号化した本文 ‖ タグ 16 B。AEAD の nonce は `0 の 4 B ‖ nonce の欄`。詰め方・読み方は `wire::seal` / `wire::decode`。
//!
//! **nonce を二度使わない決まり**: 同じ鍵で同じ nonce を 2 回使うと、ChaCha20-Poly1305 は 2 通の平文の XOR と認証の鍵が漏れる
//! （RFC 8439 §4）。そこで、**送り手の印**（[`Sender`]）を送るスレッドごとに固定し、**通し番号**（[`Seq`]）は送り手ごとに 0 から 1 ずつ
//! 増やして、鍵と同じかそれより長い寿命の物に持たせ 0 に戻さない。封じる側（[`Sealer`]）と通し番号を分けて持ち、封じるたびに両方を渡す。
//! `Seq` は `Clone` が無いので数え直すコピーは作れず、同じ印で複数のスレッドが送るとき（PONG の応答役）は同じ `Seq` を参照で共有する。
//! 2^56（[`SEQ_LIMIT`]）に着いたら送らない。対戦の秘密鍵は接続のセッション（`link::Link`）と同じ寿命で、セッションの中の作り直しでは秘密鍵も通し番号も
//! 変えない。
//!
//! **確保しない**: 封じる・開くはゲームのスレッドと受信スレッドで 1 通ごとに呼ぶ。`chacha20poly1305` はその場で暗号化するインターフェース
//! （`AeadInOut`）だけを使い、既定の機能（alloc）は切ってある。`Seq` も原子変数をそのまま持つだけ（`Arc` で共有すると、セッションの作り直しの
//! たびにゲームのスレッドで確保と解放が出る）。`tests/alloc_check.rs` が見る。

use std::sync::atomic::{AtomicU64, Ordering};

use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, Key, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::auth::AuthKey;

pub const SEED_LEN: usize = 32;
pub const PUBLIC_LEN: usize = 32;
/// nonce の欄の長さ（B。送り手の印 1 ‖ 通し番号 7）。
pub const NONCE_FIELD_LEN: usize = 8;
pub const SEAL_TAG_LEN: usize = 16;
/// 封じた便が本文の外に足す長さ（nonce の欄 ＋ タグ）。HMAC のタグ（16 B）より 8 B 長い。
pub const SEAL_OVERHEAD: usize = NONCE_FIELD_LEN + SEAL_TAG_LEN;
/// 通し番号の上限（7 B に収まる数の個数）。
pub const SEQ_LIMIT: u64 = 1 << 56;
/// HKDF の info の先頭。鍵の作り方のバージョンなので、プロトコルのバージョンを上げても変えない。
pub const KEYS_LABEL: &[u8] = b"HGRT v5 keys";

/// 使い捨ての鍵の種（呼び手が OS の乱数で引いた 32 B）。`Debug` では中身を出さない。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KeySeed(pub [u8; SEED_LEN]);

impl std::fmt::Debug for KeySeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeySeed(..)")
    }
}

/// 送り手の印（送るスレッドごとに固定）。同じ鍵で送る送り手どうしが違う印を持てば、通し番号が同じでも nonce は重ならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Sender {
    /// ゲームのスレッドの対戦の接続のセッション（`link::Link` と `link::Connected`。同じ通し番号を共有する）。
    Link = 0,
    /// 受信スレッドの PONG の応答役（IPv4・IPv6 の受信スレッドが同じ通し番号を共有する）。
    Pong = 1,
    /// 観戦の配る側（観戦者の全員へ同じ通し番号で送る）。
    SpectateHost = 2,
    /// 観戦者（`spectate::Watcher`）。
    Spectator = 3,
}

impl Sender {
    pub fn from_u8(v: u8) -> Option<Sender> {
        match v {
            0 => Some(Sender::Link),
            1 => Some(Sender::Pong),
            2 => Some(Sender::SpectateHost),
            3 => Some(Sender::Spectator),
            _ => None,
        }
    }
}

/// 使い捨ての鍵（秘密鍵と公開鍵）。`Debug` では中身を出さない。捨てると秘密鍵はメモリから消える（`zeroize`）。
#[derive(Clone)]
pub struct Ephemeral {
    secret: StaticSecret,
    public: [u8; PUBLIC_LEN],
}

impl std::fmt::Debug for Ephemeral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ephemeral(..)")
    }
}

impl Ephemeral {
    /// 種から作る（X25519 の秘密鍵は種そのもの。クランプは crate が計算のときにする）。公開鍵の計算は接続ごとに 1 回
    /// （所要は `measure_seal_cost` で測る）。
    pub fn from_seed(seed: &KeySeed) -> Ephemeral {
        let secret = StaticSecret::from(seed.0);
        let public = PublicKey::from(&secret).to_bytes();
        Ephemeral { secret, public }
    }

    /// 公開鍵（HELLO / HELLO_ACK・観戦用のコード・JOIN に載せる）。
    pub fn public(&self) -> [u8; PUBLIC_LEN] {
        self.public
    }

    /// 相手の公開鍵 `peer` との向きごとの鍵。`i_am_first` は自分が「前」か。`k` は接続コードの秘密と session から作る K
    /// （HKDF の salt）。相手の公開鍵が弱い点（共有の値が 0）なら None（来るのは壊れた相手だけ。その相手とは暗号の便を交わさない）。
    pub fn dir_keys(
        &self,
        peer: &[u8; PUBLIC_LEN],
        i_am_first: bool,
        session_id: u32,
        k: &AuthKey,
    ) -> Option<DirKeys> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer));
        if !shared.was_contributory() {
            return None;
        }
        let (first, second) = if i_am_first {
            (&self.public, peer)
        } else {
            (peer, &self.public)
        };
        let okm = expand_keys(shared.as_bytes(), k.k(), session_id, first, second);
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        a.copy_from_slice(okm.get(..32)?);
        b.copy_from_slice(okm.get(32..64)?);
        Some(if i_am_first {
            DirKeys { tx: a, rx: b }
        } else {
            DirKeys { tx: b, rx: a }
        })
    }
}

/// HKDF-SHA256 で 64 B を引く。検査が crate の外で計算した値と比べられるよう、式だけを切り出した。
fn expand_keys(
    shared: &[u8; 32],
    salt: &[u8; 32],
    session_id: u32,
    first: &[u8; PUBLIC_LEN],
    second: &[u8; PUBLIC_LEN],
) -> [u8; 64] {
    let hk = Hkdf::<Sha256>::new(Some(salt), shared);
    let mut okm = [0u8; 64];
    // 64 B は HKDF の上限（255 × 32 B）より短いので誤りは返らない。返っても全部 0 のままにして相手と食い違わせる。
    let _ = hk.expand_multi_info(
        &[KEYS_LABEL, &session_id.to_le_bytes(), first, second],
        &mut okm,
    );
    okm
}

/// 向きごとの鍵 2 本（送る向き `tx`・受ける向き `rx`）。`Debug` では中身を出さない。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DirKeys {
    tx: [u8; 32],
    rx: [u8; 32],
}

impl std::fmt::Debug for DirKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DirKeys(..)")
    }
}

impl DirKeys {
    /// 鍵の交換を通さず向きの鍵だけを決めて使う検査のため（2 台の模型 `testnet.rs`）。
    pub fn from_raw(tx: [u8; 32], rx: [u8; 32]) -> DirKeys {
        DirKeys { tx, rx }
    }

    /// 相手から見た鍵（送る向きと受ける向きを入れ替える）。模型が片側の鍵から相手の鍵を作るため。
    pub fn swapped(&self) -> DirKeys {
        DirKeys {
            tx: self.rx,
            rx: self.tx,
        }
    }

    pub fn opener(&self) -> Opener {
        Opener { key: self.rx }
    }

    /// 送る向きの封じる側。通し番号は封じるたびに渡す。
    pub fn sealer(&self, sender: Sender) -> Sealer {
        Sealer {
            key: self.tx,
            sender,
        }
    }
}

/// 通し番号（送り手ごと）。`Clone` が無く、共有は参照で。
#[derive(Default)]
pub struct Seq(AtomicU64);

impl std::fmt::Debug for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Seq({})", self.next())
    }
}

impl Seq {
    /// 0 から数える新しい通し番号（確保しない。`static` にも置ける）。
    pub const fn new() -> Seq {
        Seq(AtomicU64::new(0))
    }

    /// 検査が上限の近くの番号を作るため。
    #[cfg(test)]
    pub(crate) fn at(v: u64) -> Seq {
        Seq(AtomicU64::new(v))
    }

    /// 次に取る番号（取らない）。
    pub fn next(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// 番号を 1 つ取る。[`SEQ_LIMIT`] に着いていたら None。番号が重ならないことだけが要るので Relaxed。
    fn take(&self) -> Option<u64> {
        let v = self.0.fetch_add(1, Ordering::Relaxed);
        (v < SEQ_LIMIT).then_some(v)
    }
}

/// 送る向きの封じる側（鍵 ＋ 送り手の印）。通し番号は持たない。`Debug` では鍵を出さない。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sealer {
    key: [u8; 32],
    sender: Sender,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sealer({:?})", self.sender)
    }
}

impl Sealer {
    pub fn sender(&self) -> Sender {
        self.sender
    }

    /// `body` をその場で暗号化し、（nonce の欄, タグ）を返す。`aad` はヘッダ 12 B。番号を使い切っていたら None（`body` は変えない）。
    pub(crate) fn seal(
        &self,
        seq: &Seq,
        aad: &[u8],
        body: &mut [u8],
    ) -> Option<([u8; NONCE_FIELD_LEN], [u8; SEAL_TAG_LEN])> {
        let seq = seq.take()?;
        let field = nonce_field(self.sender as u8, seq);
        let tag = aead_seal(&self.key, &nonce_of(&field), aad, body)?;
        Some((field, tag))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Opener {
    key: [u8; 32],
}

impl std::fmt::Debug for Opener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Opener(..)")
    }
}

impl Opener {
    /// `body`（暗号文）をその場で開く。タグが合わなければ false（`body` の中身は使わない）。
    pub(crate) fn open(
        &self,
        aad: &[u8],
        field: &[u8; NONCE_FIELD_LEN],
        body: &mut [u8],
        tag: &[u8; SEAL_TAG_LEN],
    ) -> bool {
        aead_open(&self.key, &nonce_of(field), aad, body, tag)
    }
}

/// nonce の欄（送り手の印 ‖ 通し番号の下位 7 B。LE）。
pub(crate) fn nonce_field(sender: u8, seq: u64) -> [u8; NONCE_FIELD_LEN] {
    let s = seq.to_le_bytes();
    [sender, s[0], s[1], s[2], s[3], s[4], s[5], s[6]]
}

pub fn read_nonce_field(field: &[u8; NONCE_FIELD_LEN]) -> (u8, u64) {
    let mut s = [0u8; 8];
    for (d, b) in s.iter_mut().zip(field.iter().skip(1)) {
        *d = *b;
    }
    (field[0], u64::from_le_bytes(s))
}

/// AEAD の nonce（12 B）= 0 の 4 B ‖ nonce の欄。
fn nonce_of(field: &[u8; NONCE_FIELD_LEN]) -> [u8; 12] {
    let mut n = [0u8; 12];
    for (d, s) in n.iter_mut().skip(4).zip(field.iter()) {
        *d = *s;
    }
    n
}

/// ChaCha20-Poly1305 でその場で封じる（RFC 8439 の標本と比べるため、nonce の作り方と分けた）。
fn aead_seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    body: &mut [u8],
) -> Option<[u8; SEAL_TAG_LEN]> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let tag: Tag = cipher
        .encrypt_inout_detached(&Nonce::from(*nonce), aad, body.into())
        .ok()?;
    let mut out = [0u8; SEAL_TAG_LEN];
    for (d, s) in out.iter_mut().zip(tag.iter()) {
        *d = *s;
    }
    Some(out)
}

fn aead_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    body: &mut [u8],
    tag: &[u8; SEAL_TAG_LEN],
) -> bool {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    cipher
        .decrypt_inout_detached(&Nonce::from(*nonce), aad, body.into(), &Tag::from(*tag))
        .is_ok()
}

#[cfg(test)]
pub(crate) mod tests {
    //! 期待値は crate の外で作った: RFC 7748 §6.1・RFC 5869 の Test Case 1・RFC 8439 §2.8.2 の標本と、
    //! Python 3 の `cryptography` と標準の `hmac` で計算した HGRT の鍵。
    use super::*;
    use crate::auth::Secret;

    pub(crate) fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    pub(crate) fn arr32(s: &str) -> [u8; 32] {
        let v = hex(s);
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        a
    }

    /// RFC 7748 §6.1 の Alice と Bob の秘密鍵。
    pub(crate) const ALICE: &str =
        "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
    pub(crate) const BOB: &str = "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb";

    #[test]
    fn x25519_matches_rfc7748() {
        let a = Ephemeral::from_seed(&KeySeed(arr32(ALICE)));
        let b = Ephemeral::from_seed(&KeySeed(arr32(BOB)));
        assert_eq!(
            a.public().to_vec(),
            hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
        );
        assert_eq!(
            b.public().to_vec(),
            hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
        );
        let shared = a.secret.diffie_hellman(&PublicKey::from(b.public()));
        assert_eq!(
            shared.as_bytes().to_vec(),
            hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
        );
    }

    #[test]
    fn hkdf_matches_rfc5869() {
        let hk = Hkdf::<Sha256>::new(Some(&hex("000102030405060708090a0b0c")), &[0x0b; 22]);
        let mut okm = [0u8; 42];
        hk.expand_multi_info(&[&hex("f0f1f2f3f4"), &hex("f5f6f7f8f9")], &mut okm)
            .unwrap();
        assert_eq!(
            okm.to_vec(),
            hex(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
            )
        );
    }

    #[test]
    fn chacha20poly1305_matches_rfc8439() {
        let key = arr32("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&hex("070000004041424344454647"));
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let mut body = plain.to_vec();
        let tag = aead_seal(&key, &nonce, &aad, &mut body).unwrap();
        assert_eq!(&body[..16], &hex("d31a8d34648e60db7b86afbc53ef7ec2")[..]);
        assert_eq!(tag.to_vec(), hex("1ae10b594f09e26a7e902ecbd0600691"));
        let mut back = body.clone();
        assert!(aead_open(&key, &nonce, &aad, &mut back, &tag));
        assert_eq!(&back[..], &plain[..]);
        let mut aad2 = aad.clone();
        aad2[0] ^= 1;
        let mut back = body.clone();
        assert!(!aead_open(&key, &nonce, &aad2, &mut back, &tag));
    }

    /// HGRT の鍵の golden（Python 3 の `cryptography` と標準の `hmac` で計算）:
    /// 秘密は 00 01 … 0f、session 0x11223344（K は `auth.rs` の `K_GOLDEN`）、Alice が「前」。
    pub(crate) const OKM_GOLDEN: &str = "a214f2218b0ed79e233e9d33ea6313cc0f1b9331319bab495e637b4bf968edb2\
        9f0d189db2c6386bb21403da6623ae2bd5ffc41a30ba575e4134243152c0ee9b";

    #[test]
    fn direction_keys_match_values_computed_outside_and_mesh() {
        let k = AuthKey::derive(&crate::auth::tests::golden_secret(), 0x1122_3344);
        let a = Ephemeral::from_seed(&KeySeed(arr32(ALICE)));
        let b = Ephemeral::from_seed(&KeySeed(arr32(BOB)));
        let ka = a.dir_keys(&b.public(), true, 0x1122_3344, &k).unwrap();
        let kb = b.dir_keys(&a.public(), false, 0x1122_3344, &k).unwrap();
        let okm = hex(&OKM_GOLDEN.replace(' ', ""));
        assert_eq!(ka.tx.to_vec(), okm[..32].to_vec(), "前 → 後ろ");
        assert_eq!(ka.rx.to_vec(), okm[32..].to_vec(), "後ろ → 前");
        assert_eq!(ka.tx, kb.rx);
        assert_eq!(ka.rx, kb.tx);
        assert_ne!(ka.tx, ka.rx, "向きの鍵が 1 本");
        let k2 = AuthKey::derive(&crate::auth::tests::golden_secret(), 0x1122_3345);
        assert_ne!(a.dir_keys(&b.public(), true, 0x1122_3345, &k2).unwrap(), ka);
        let k3 = AuthKey::derive(&Secret([9; 16]), 0x1122_3344);
        assert_ne!(a.dir_keys(&b.public(), true, 0x1122_3344, &k3).unwrap(), ka);
    }

    #[test]
    fn weak_public_keys_make_no_keys() {
        let k = AuthKey::derive(&Secret([1; 16]), 7);
        let a = Ephemeral::from_seed(&KeySeed(arr32(ALICE)));
        let mut one = [0u8; 32];
        one[0] = 1;
        for weak in [[0u8; 32], one] {
            assert_eq!(a.dir_keys(&weak, true, 7, &k), None);
        }
        let b = Ephemeral::from_seed(&KeySeed(arr32(BOB)));
        assert!(a.dir_keys(&b.public(), true, 7, &k).is_some());
    }

    #[test]
    fn seq_is_shared_by_reference_and_stops_at_the_limit() {
        static S: Seq = Seq::new();
        let got: Vec<u64> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..2)
                .map(|_| sc.spawn(|| (0..1_000).filter_map(|_| S.take()).collect::<Vec<_>>()))
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        let mut sorted = got.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 2_000, "2 本のスレッドが同じ番号を取った");
        assert_eq!(S.next(), 2_000);
        let s = Seq::at(SEQ_LIMIT - 1);
        assert_eq!(s.take(), Some(SEQ_LIMIT - 1));
        assert_eq!(s.take(), None);
        assert_eq!(s.take(), None);
        let f = nonce_field(2, 0x0011_2233_4455_6677);
        assert_eq!(f, [2, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        assert_eq!(read_nonce_field(&f), (2, 0x0011_2233_4455_6677));
        assert_eq!(format!("{:?}", KeySeed([7; 32])), "KeySeed(..)");
    }

    #[test]
    #[ignore = "所要を測るだけ。手で --release --ignored --nocapture で走らせる"]
    fn measure_seal_cost_per_datagram() {
        use crate::wire::{self, Input, MAX_DATAGRAM, MAX_WORDS, Message};
        const SID: u32 = 0x1122_3344;
        let k = AuthKey::derive(&Secret([0x42; 16]), SID);
        let a = Ephemeral::from_seed(&KeySeed(arr32(ALICE)));
        let b = Ephemeral::from_seed(&KeySeed(arr32(BOB)));
        let ka = a.dir_keys(&b.public(), true, SID, &k).unwrap();
        let kb = b.dir_keys(&a.public(), false, SID, &k).unwrap();
        let mut words = [0u16; MAX_WORDS];
        for (i, w) in words.iter_mut().enumerate() {
            *w = (i as u16).wrapping_mul(0x9E37);
        }
        let msgs = [
            (
                "INPUT 32 語",
                Message::Input(Input {
                    seg: 1,
                    ack: 2,
                    base: 3,
                    lat: None,
                    count: MAX_WORDS as u8,
                    words,
                }),
            ),
            ("PING", Message::Ping { send_time: 60_000 }),
        ];
        let seq = Seq::new();
        let sealer = ka.sealer(Sender::Link);
        let opener = kb.opener();
        let n = 200_000u32;
        for (what, m) in &msgs {
            let mut buf = [0u8; MAX_DATAGRAM];
            let t0 = std::time::Instant::now();
            let mut len = 0;
            for _ in 0..n {
                len = wire::seal(&sealer, &seq, SID, std::hint::black_box(m), &mut buf).unwrap();
            }
            let seal_ns = t0.elapsed().as_nanos() / u128::from(n);
            let bytes = buf[..len].to_vec();
            let t0 = std::time::Instant::now();
            let mut ok = 0u32;
            for _ in 0..n {
                ok += u32::from(
                    wire::decode(&k, Some(&opener), std::hint::black_box(&bytes)).is_ok(),
                );
            }
            let open_ns = t0.elapsed().as_nanos() / u128::from(n);
            let plain: Vec<u8> = bytes[..len - SEAL_OVERHEAD].to_vec();
            let t0 = std::time::Instant::now();
            let mut acc = 0u8;
            for _ in 0..n {
                acc ^= k.tag(std::hint::black_box(&plain))[0];
            }
            let tag_ns = t0.elapsed().as_nanos() / u128::from(n);
            let tag = k.tag(&plain);
            let t0 = std::time::Instant::now();
            let mut vok = 0u32;
            for _ in 0..n {
                vok += u32::from(k.verify(std::hint::black_box(&plain), &tag));
            }
            let verify_ns = t0.elapsed().as_nanos() / u128::from(n);
            println!(
                "{what}（封じて {len} B）: 封じる {seal_ns} ns / 開く {open_ns} ns ‖ バージョン 4 の HMAC（{} B に）: タグ {tag_ns} ns / 比べ {verify_ns} ns（各 {n} 回の平均。{acc}）",
                plain.len()
            );
            assert_eq!((ok, vok), (n, n));
            assert!(seal_ns < 1_000_000 && open_ns < 1_000_000);
        }
        let m = 2_000u32;
        let t0 = std::time::Instant::now();
        for i in 0..m {
            let mut seed = arr32(ALICE);
            seed[0] = i as u8;
            seed[1] = (i >> 8) as u8;
            let e = Ephemeral::from_seed(&KeySeed(seed));
            std::hint::black_box(e.dir_keys(&b.public(), true, SID, &k));
        }
        println!(
            "鍵の交換: {} ns（公開鍵の計算 ＋ 向きごとの鍵。{m} 回の平均。接続ごとに 1 回）",
            t0.elapsed().as_nanos() / u128::from(m)
        );
    }
}
