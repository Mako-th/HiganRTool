//! ネットワークアダプターの一覧を OS から引く（Win32 の層。unsafe はここにもある。1 つずつ `// SAFETY:`）。選び方は core の `adapter_pick`。
//!
//! `GetAdaptersAddresses` で IPv4 のアドレスを持つアダプターを引き、アドレスごとに `Adapter` を作る。開くスレッド（`bind = auto` の解決・LAN の
//! ホストの待ち）でだけ呼ぶ。ゲームのスレッドでは呼ばない（確保とカーネルの呼び出しがある）。

use std::net::Ipv4Addr;

use higanr_core::adapter_pick::Adapter;
use windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, IpDadStatePreferred, SOCKADDR_IN, SOCKET_ADDRESS,
};

/// 最初に渡す書き込み先の大きさ（バイト）。足りなければ OS が必要な大きさを返すので引き直す。
const START_BYTES: u32 = 16 * 1024;
/// 引き直す回数の上限（アダプターが増えた直後の競合に備える）。
const RETRIES: usize = 4;
/// つながりをたどる数の上限（壊れた一覧で終わらなくならないように）。
const WALK_MAX: usize = 512;
/// `Description` を読む長さの上限（UTF-16 の単位）。
const DESCRIPTION_MAX: usize = 256;

/// IPv4 のアドレスを持つアダプターを、アドレスごとに返す。引けなければ理由の文。
pub fn list() -> Result<Vec<Adapter>, String> {
    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_INCLUDE_GATEWAYS;
    let mut bytes = START_BYTES;
    for _ in 0..RETRIES {
        // 8 バイト境界に置く（`IP_ADAPTER_ADDRESSES_LH` は u64 を含む）。
        let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
        let mut size = bytes;
        // SAFETY: `buf` は `size` バイト以上の書き込み先で、8 バイト境界。AF_INET = IPv4 だけ・予約は NULL。戻りの `size` は OS が必要な大きさを書く。
        let r = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                flags,
                core::ptr::null(),
                buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut size,
            )
        };
        if r == ERROR_BUFFER_OVERFLOW {
            bytes = size.max(bytes + 1024);
            continue;
        }
        if r != 0 {
            return Err(format!(
                "アダプターの一覧を引けない（GetAdaptersAddresses が {r}）"
            ));
        }
        // SAFETY: 成功したので、`buf` の先頭から `Next` でたどる連結リストが `buf` の中に作られている（OS が書いた。`buf` はこの関数の間生きる）。
        return Ok(unsafe { collect(buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()) });
    }
    Err("アダプターの一覧を引けない（書き込み先の大きさが決まらない）".to_string())
}

/// 連結リスト全体をたどって `Adapter` にする。
///
/// # Safety
/// `first` は `GetAdaptersAddresses` が成功して書いた先頭（または NULL）で、それが指す領域全体が呼び出しの間有効。
unsafe fn collect(first: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Adapter> {
    let mut out = Vec::new();
    let mut p = first;
    for _ in 0..WALK_MAX {
        if p.is_null() {
            break;
        }
        // SAFETY: 上の約束（`p` は先頭か `Next`。OS が書いた有効な要素）。
        let a = unsafe { &*p };
        // SAFETY: 同上。要素の中のポインター（Description・アドレスの連結リスト）は同じ領域の中を指す。
        unsafe { push_adapter(a, &mut out) };
        p = a.Next;
    }
    out
}

/// 1 つのアダプターの IPv4 アドレスごとに `Adapter` を足す。
///
/// # Safety
/// `a` の `Description`・`FirstUnicastAddress`・`FirstGatewayAddress` は、NULL か OS が書いた有効な領域を指す。
unsafe fn push_adapter(a: &IP_ADAPTER_ADDRESSES_LH, out: &mut Vec<Adapter>) {
    // SAFETY: 約束どおり `Description` は NULL か終端つきの UTF-16。
    let description = unsafe { read_wide(a.Description) };
    let mut oui = [0u8; 3];
    if a.PhysicalAddressLength >= 3 {
        oui.copy_from_slice(&a.PhysicalAddress[..3]);
    }
    // SAFETY: 約束どおり `FirstGatewayAddress` は NULL か有効な連結リスト。
    let gateway = unsafe { has_ipv4_gateway(a) };
    let mut u = a.FirstUnicastAddress;
    for _ in 0..WALK_MAX {
        if u.is_null() {
            break;
        }
        // SAFETY: NULL でなければ OS が書いた有効な要素。
        let e = unsafe { &*u };
        // SAFETY: `Address` の `lpSockaddr` は NULL か `iSockaddrLength` バイトの有効な領域。
        if let Some(ip) = unsafe { ipv4_of(&e.Address) } {
            out.push(Adapter {
                description: description.clone(),
                if_type: a.IfType,
                oui,
                up: a.OperStatus == IfOperStatusUp,
                preferred: e.DadState == IpDadStatePreferred,
                ip,
                gateway,
                metric: a.Ipv4Metric,
            });
        }
        u = e.Next;
    }
}

/// IPv4 のゲートウェイを持つか（IPv6 のゲートウェイだけなら偽）。
///
/// # Safety
/// `a.FirstGatewayAddress` は NULL か有効な連結リスト。
unsafe fn has_ipv4_gateway(a: &IP_ADAPTER_ADDRESSES_LH) -> bool {
    let mut g = a.FirstGatewayAddress;
    for _ in 0..WALK_MAX {
        if g.is_null() {
            return false;
        }
        // SAFETY: NULL でなければ OS が書いた有効な要素。
        let e = unsafe { &*g };
        // SAFETY: `Address` は NULL か有効な領域を指す。
        if unsafe { ipv4_of(&e.Address) }.is_some() {
            return true;
        }
        g = e.Next;
    }
    false
}

/// `SOCKET_ADDRESS` が IPv4 のアドレスならそれを返す。
///
/// # Safety
/// `s.lpSockaddr` は NULL か、`s.iSockaddrLength` バイトの有効な領域を指す。
unsafe fn ipv4_of(s: &SOCKET_ADDRESS) -> Option<Ipv4Addr> {
    if s.lpSockaddr.is_null() || (s.iSockaddrLength as usize) < core::mem::size_of::<SOCKADDR_IN>()
    {
        return None;
    }
    // SAFETY: NULL でなく、`SOCKADDR_IN` 以上の長さがある（上で確かめた）。ファミリーは先頭の 2 バイトで、`SOCKADDR` から読める。
    let family = unsafe { (*s.lpSockaddr.cast_const()).sa_family };
    if family != AF_INET {
        return None;
    }
    // SAFETY: ファミリーが AF_INET で、長さは `SOCKADDR_IN` 以上。
    let sin = unsafe { &*s.lpSockaddr.cast_const().cast::<SOCKADDR_IN>() };
    // SAFETY: `IN_ADDR` の共用体は 4 バイトの `S_addr`（ネットワークの並び）。どの見方で読んでもよい。
    let raw = unsafe { sin.sin_addr.S_un.S_addr };
    Some(Ipv4Addr::from(raw.to_ne_bytes()))
}

/// 終端つきの UTF-16 を文字列にする（NULL なら空。長すぎれば `DESCRIPTION_MAX` 単位で切る）。
///
/// # Safety
/// `p` は NULL か、0 で終わるか `DESCRIPTION_MAX` 単位以上続く有効な領域を指す。
unsafe fn read_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut units = Vec::new();
    for i in 0..DESCRIPTION_MAX {
        // SAFETY: 約束どおり、0 に当たるまで（または上限まで）読める。
        let u = unsafe { *p.add(i) };
        if u == 0 {
            break;
        }
        units.push(u);
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 壊れたら落ちる: 本物の OS の一覧を引けない・空で返る（どの Windows にも loopback の 127.0.0.1 がある）・引いた項目が IPv4 の
    /// ユニキャストでない・loopback が仮想と判定されない。値は PC ごとに違うので、形（引けること・loopback の扱い）だけ見る。
    #[test]
    fn the_os_list_comes_back_with_ipv4_entries_and_loopback_counts_as_virtual() {
        let list = list().expect("OS の一覧が引ける");
        let loopback: Vec<&Adapter> = list.iter().filter(|a| a.ip.is_loopback()).collect();
        assert!(!loopback.is_empty(), "loopback の項目が無い: {list:?}");
        for a in &list {
            assert!(
                !a.ip.is_multicast() && !a.ip.is_unspecified() && !a.ip.is_broadcast(),
                "{a:?}"
            );
        }
        for a in loopback {
            assert!(a.is_virtual(), "loopback は仮想: {a:?}");
            assert!(!a.is_lan_candidate(), "{a:?}");
        }
        println!("アダプターの IPv4 アドレス: {} 件", list.len());
    }
}
