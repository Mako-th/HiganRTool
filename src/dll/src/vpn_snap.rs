//! 自分のPCのVPNのネットワークのスナップショット。LANの「クライアント」「観戦」のアドレス入力で、打たれたアドレスを受け付けるかどうかの判定に使う。
//!
//! 入力はゲームのスレッドで動く。
//! アダプターの一覧の取得はOSの呼び出しとメモリの確保を伴うので、ここでは行わない。別のスレッドが一覧を取得してスナップショットを書く。
//! ゲームのスレッドはアトミック変数を読むだけ（確保しない）。

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use higanr_core::adapter_pick::{vpn_links, vpn_set};
use higanr_core::vpn_net::{VPN_SET_MAX, VpnSet};

use crate::adapters_win;

type Cells = [AtomicU64; VPN_SET_MAX];

static SNAP: Cells = [const { AtomicU64::new(0) }; VPN_SET_MAX];
/// 更新のスレッドが動いている間はtrue（重ねて作らない）。
static BUSY: AtomicBool = AtomicBool::new(false);

fn load(cells: &Cells) -> VpnSet {
    let mut w = [0u64; VPN_SET_MAX];
    for (d, s) in w.iter_mut().zip(cells) {
        *d = s.load(Ordering::Relaxed);
    }
    VpnSet::from_words(&w)
}

fn store(cells: &Cells, set: &VpnSet) {
    for (s, w) in cells.iter().zip(set.to_words()) {
        s.store(w, Ordering::Relaxed);
    }
}

/// スナップショットを読む（ゲームのスレッドで呼んでよい）。
pub fn read() -> VpnSet {
    load(&SNAP)
}

/// アダプターの一覧を取得してスナップショットを書き、書いた集合を返す。取得できなければスナップショットは更新しない。呼んだスレッドで待つので、ゲームのスレッドでは呼ばない。
pub fn scan() -> VpnSet {
    match adapters_win::list() {
        Ok(list) => {
            let set = vpn_set(&vpn_links(&list));
            store(&SNAP, &set);
            set
        }
        Err(_) => read(),
    }
}

/// 別のスレッドで`scan`する（すでに動いていれば何もしない）。スレッドを作れなくてもスナップショットが古いままになるだけ。
pub fn request_scan() {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("higanr-vpn".into())
        .spawn(|| {
            // パニックは受けて外へ出さない（スレッドの入口）。
            let _ = std::panic::catch_unwind(scan);
            BUSY.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        BUSY.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::vpn_net::Net4;

    /// 実物のスナップショット（`SNAP`）は他の検査が実機の一覧で書く。そのため同じ形の別のバッファで検査する。
    #[test]
    fn a_stored_set_is_read_back_and_replaced_whole() {
        let cells: Cells = [const { AtomicU64::new(0) }; VPN_SET_MAX];
        let a = Net4::new("26.0.0.1".parse().unwrap(), 8).unwrap();
        let b = Net4::new("25.0.0.1".parse().unwrap(), 8).unwrap();
        store(&cells, &VpnSet::from_nets([a, b]));
        let got = load(&cells);
        assert_eq!(got.find("26.1.1.1".parse().unwrap()), Some(a));
        assert_eq!(got.find("25.1.1.1".parse().unwrap()), Some(b));
        store(&cells, &VpnSet::from_nets([b]));
        assert_eq!(load(&cells).find("26.1.1.1".parse().unwrap()), None);
        assert_eq!(load(&cells).len(), 1);
        store(&cells, &VpnSet::NONE);
        assert!(load(&cells).is_empty());
    }
}
