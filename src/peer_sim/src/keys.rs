//! 使い捨ての鍵の種をpeer_simのツールとしてどう用意するか。鍵の式・封じ方は`higanr_core::seal`にあり、ここは種の出どころだけ。
//! 対戦のセッション・観戦者の種は走るたびに新しい値（`fresh_seed`）。peer_simはstdだけで書く（unsafeもWin32も使わない）のでstdの`RandomState`から32 Bを作る。検査の相手役なので本物（dllはOSの乱数）より弱くてよい。
//! 観戦者へ配信する側の種も既定は`fresh_seed`。同じ秘密の乱数・sessionで起動し直しても鍵が変わり、nonceが重ならない。
//! 固定の種（`spectate_host_seed`）は観戦用のコードを使わない経路（`--watch --peer`・loopbackの検査）のためだけ。**起動し直しに使わない**（同じ鍵で通し番号が0から数え直され、同じ観戦者との間でnonceが重なる）。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use higanr_core::auth::{AuthKey, Secret};
use higanr_core::seal::{Ephemeral, KeySeed, PUBLIC_LEN};

pub fn fresh_seed() -> KeySeed {
    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        for (d, s) in chunk.iter_mut().zip(h.finish().to_le_bytes()) {
            *d = s;
        }
    }
    KeySeed(out)
}

/// 観戦で配布する側の固定の種（観戦用の秘密の乱数とsessionから。**起動し直しに使わない**）。鍵で2つの固定の文字列に付けたタグ（16 Bずつ）を並べる。
pub fn spectate_host_seed(secret: &Secret, session_id: u32) -> KeySeed {
    let k = AuthKey::derive(secret, session_id);
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&k.tag(b"peer_sim spectate host seed 1"));
    out[16..].copy_from_slice(&k.tag(b"peer_sim spectate host seed 2"));
    KeySeed(out)
}

/// 固定の種の公開鍵（観戦者が式で作る経路だけ）。
pub fn spectate_host_public(secret: &Secret, session_id: u32) -> [u8; PUBLIC_LEN] {
    Ephemeral::from_seed(&spectate_host_seed(secret, session_id)).public()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_seeds_differ_and_the_fixed_spectate_host_seed_is_fixed_per_code() {
        assert_ne!(fresh_seed(), fresh_seed());
        let s = Secret([1; 16]);
        assert_eq!(spectate_host_seed(&s, 7), spectate_host_seed(&s, 7));
        assert_ne!(spectate_host_seed(&s, 7), spectate_host_seed(&s, 8));
        assert_ne!(
            spectate_host_seed(&s, 7),
            spectate_host_seed(&Secret([2; 16]), 7)
        );
        assert_eq!(
            spectate_host_public(&s, 7),
            Ephemeral::from_seed(&spectate_host_seed(&s, 7)).public()
        );
    }
}
