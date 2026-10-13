//! 席（1P / 2P）の希望から、ホストの席（AGREE の host_seat）を決める決まり（pure。確保しない）。
//!
//! - 各自が「妖怪 対 妖怪」の画面（ゲーム内のメニュー）で、あらかじめ希望（1P / 2P / おまかせ）を置いておく。
//! - 希望がぶつかったら（両方 1P・両方 2P）、**ルームに先にいた人**の希望を優先する。
//! - 片方がおまかせなら、相手の希望を優先する。両方おまかせなら、ランダムに決める。
//!
//! 「先にいた人」は、ルームサーバーの経路では先に入った人（始める側 `CodeSide::Offer`）、クリップボードの経路では
//! 「接続コードを作る」を選んだ人。各自は自分がどちらかを知っていて、HELLO の `arrival` で名乗る（`wire::Arrival`）。
//! どちらも名乗らない経路（ini の `peer`・ローダーのウィンドウ・peer_sim）や名乗りが食い違うとき（手順の上では起きない）は、
//! **ホストを先にいた人と見る**。名乗らない経路の希望は ini の `host_seat`（おまかせは無い）なので、ホストの希望が必ず通り、
//! 従来の「ホストが席を決める」（`Config::host_seat`）と同じ答えになる。
//!
//! 両方おまかせのとき（`coin`）は、両者の role_nonce（HELLO で交換した u64 の乱数）の排他的論理和の立っているビットの
//! 数の偶奇で決める。繋がる前に両方の nonce を持っているので追加の通信は要らず（遅延の増分 0）、ゲストは届いた AGREE の
//! host_seat を同じ式で確かめられる（`handshake.rs` の `Counters::agree_bad_seat`）。席は接続の間ずっと同じ。
#![forbid(unsafe_code)]

use crate::wire::{Arrival, Seat, SeatWish};

/// 片方の希望と、ルームに先にいたかの名乗り（HELLO の +44・+45）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatAsk {
    pub wish: SeatWish,
    pub arrival: Arrival,
}

impl SeatAsk {
    /// 名乗らない経路の形: ini の `host_seat` を希望にし、先かどうかは名乗らない。
    pub const fn plain(host_seat: Seat) -> SeatAsk {
        SeatAsk {
            wish: match host_seat {
                Seat::P1 => SeatWish::P1,
                Seat::P2 => SeatWish::P2,
            },
            arrival: Arrival::Unknown,
        }
    }
}

pub const fn other(s: Seat) -> Seat {
    match s {
        Seat::P1 => Seat::P2,
        Seat::P2 => Seat::P1,
    }
}

/// 両方おまかせのときに、先にいた人が座る席。引数の順に依らない（排他的論理和）。
pub const fn coin(nonce_a: u64, nonce_b: u64) -> Seat {
    if (nonce_a ^ nonce_b).count_ones().is_multiple_of(2) {
        Seat::P1
    } else {
        Seat::P2
    }
}

/// 先にいた人の席。`first` が先にいた人の希望、`second` が後から来た人の希望。`both_any` は両方おまかせのときの答え。
///
/// - 先にいた人が 1P / 2P を望めば、それが通る。
/// - 先にいた人がおまかせなら、後から来た人の希望の逆。後から来た人もおまかせなら `both_any`。
pub const fn first_seat(first: SeatWish, second: SeatWish, both_any: Seat) -> Seat {
    match (first, second) {
        (SeatWish::P1, _) => Seat::P1,
        (SeatWish::P2, _) => Seat::P2,
        (SeatWish::Any, SeatWish::P1) => Seat::P2,
        (SeatWish::Any, SeatWish::P2) => Seat::P1,
        (SeatWish::Any, SeatWish::Any) => both_any,
    }
}

/// ホストが先にいた人か。名乗りが揃ってゲストが先のときだけ偽。名乗らない・食い違うときは真（ホストを先と見る）。
pub const fn host_is_first(host: Arrival, guest: Arrival) -> bool {
    !matches!((host, guest), (Arrival::Second, Arrival::First))
}

/// ホストの席。ホストが AGREE を作るときもゲストが確かめるときもこれを使う。`host_nonce` / `guest_nonce` は両者の role_nonce。
pub const fn host_seat(host: SeatAsk, guest: SeatAsk, host_nonce: u64, guest_nonce: u64) -> Seat {
    let both_any = coin(host_nonce, guest_nonce);
    if host_is_first(host.arrival, guest.arrival) {
        first_seat(host.wish, guest.wish, both_any)
    } else {
        other(first_seat(guest.wish, host.wish, both_any))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WISHES: [SeatWish; 3] = [SeatWish::P1, SeatWish::P2, SeatWish::Any];

    /// 仕様をそのまま書いた表（先にいた人の希望, 後から来た人の希望 → 先にいた人の席。おまかせ同士は None = 乱数）。
    const TABLE: [(SeatWish, SeatWish, Option<Seat>); 9] = [
        // ぶつかる → 先にいた人の希望。
        (SeatWish::P1, SeatWish::P1, Some(Seat::P1)),
        (SeatWish::P2, SeatWish::P2, Some(Seat::P2)),
        // ぶつからない → 両方の希望が通る。
        (SeatWish::P1, SeatWish::P2, Some(Seat::P1)),
        (SeatWish::P2, SeatWish::P1, Some(Seat::P2)),
        // 後から来た人がおまかせ → 先にいた人の希望。
        (SeatWish::P1, SeatWish::Any, Some(Seat::P1)),
        (SeatWish::P2, SeatWish::Any, Some(Seat::P2)),
        // 先にいた人がおまかせ → 後から来た人の希望（先にいた人はその逆）。
        (SeatWish::Any, SeatWish::P1, Some(Seat::P2)),
        (SeatWish::Any, SeatWish::P2, Some(Seat::P1)),
        // 両方おまかせ → 乱数。
        (SeatWish::Any, SeatWish::Any, None),
    ];

    #[test]
    fn the_table_covers_all_nine_pairs_once() {
        for a in WISHES {
            for b in WISHES {
                let n = TABLE.iter().filter(|r| r.0 == a && r.1 == b).count();
                assert_eq!(n, 1, "{a:?} × {b:?}");
            }
        }
    }

    #[test]
    fn first_seat_follows_the_owners_rules_for_all_nine_pairs() {
        for (first, second, want) in TABLE {
            for both_any in [Seat::P1, Seat::P2] {
                let got = first_seat(first, second, both_any);
                assert_eq!(
                    got,
                    want.unwrap_or(both_any),
                    "{first:?} × {second:?}（乱数 {both_any:?}）"
                );
            }
        }
    }

    #[test]
    fn a_specific_wish_is_never_lost_to_an_any() {
        for (first, second, _) in TABLE {
            let s1 = first_seat(first, second, Seat::P1);
            let s2 = other(s1);
            match (first, second) {
                (SeatWish::Any, SeatWish::Any) => {}
                (SeatWish::Any, w) => assert_eq!(Some(s2), wish_seat(w), "{first:?} × {second:?}"),
                (w, SeatWish::Any) => assert_eq!(Some(s1), wish_seat(w), "{first:?} × {second:?}"),
                (w1, w2) if w1 == w2 => assert_eq!(Some(s1), wish_seat(w1), "ぶつかったら先"),
                (w1, w2) => {
                    assert_eq!(Some(s1), wish_seat(w1));
                    assert_eq!(Some(s2), wish_seat(w2));
                }
            }
        }
    }

    fn wish_seat(w: SeatWish) -> Option<Seat> {
        match w {
            SeatWish::P1 => Some(Seat::P1),
            SeatWish::P2 => Some(Seat::P2),
            SeatWish::Any => None,
        }
    }

    #[test]
    fn the_host_counts_as_first_unless_the_guest_alone_says_first() {
        use Arrival::{First, Second, Unknown};
        let all = [Unknown, First, Second];
        for h in all {
            for g in all {
                let want = !(h == Second && g == First);
                assert_eq!(host_is_first(h, g), want, "ホスト {h:?} / ゲスト {g:?}");
            }
        }
    }

    #[test]
    fn host_seat_matches_the_table_whoever_came_first() {
        let (hn, gn) = (0xFFFF_0000_0000_0001, 0x1); // 排他的論理和のビットは 16 本（偶数）→ 乱数の答えは 1P。
        assert_eq!(coin(hn, gn), Seat::P1);
        for (first, second, want) in TABLE {
            let want_first = want.unwrap_or(Seat::P1);
            let h = SeatAsk {
                wish: first,
                arrival: Arrival::First,
            };
            let g = SeatAsk {
                wish: second,
                arrival: Arrival::Second,
            };
            assert_eq!(
                host_seat(h, g, hn, gn),
                want_first,
                "ホストが先 {first:?} × {second:?}"
            );
            let g = SeatAsk {
                wish: first,
                arrival: Arrival::First,
            };
            let h = SeatAsk {
                wish: second,
                arrival: Arrival::Second,
            };
            assert_eq!(
                host_seat(h, g, hn, gn),
                other(want_first),
                "ゲストが先 {first:?} × {second:?}"
            );
        }
    }

    #[test]
    fn the_plain_path_keeps_the_hosts_seat_as_before() {
        for hs in [Seat::P1, Seat::P2] {
            for gs in [Seat::P1, Seat::P2] {
                for (hn, gn) in [(5, 3), (u64::MAX, 0), (0x8000_0000_0000_0000, 0x7FFF)] {
                    assert_eq!(
                        host_seat(SeatAsk::plain(hs), SeatAsk::plain(gs), hn, gn),
                        hs
                    );
                }
            }
        }
    }

    #[test]
    fn the_coin_is_symmetric_and_roughly_fair() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let n = 10_000;
        let mut p1 = 0;
        for _ in 0..n {
            let (a, b) = (next(), next());
            assert_eq!(coin(a, b), coin(b, a));
            if coin(a, b) == Seat::P1 {
                p1 += 1;
            }
        }
        assert!((4_500..=5_500).contains(&p1), "1P の割合 {p1} / {n}");
    }

    #[test]
    fn both_any_uses_the_coin() {
        let any = |arrival| SeatAsk {
            wish: SeatWish::Any,
            arrival,
        };
        let (even, odd) = ((3, 0), (1, 0)); // 3 = 0b11（2 本・偶数）、1 = 0b1（1 本・奇数）
        assert_eq!(coin(even.0, even.1), Seat::P1);
        assert_eq!(coin(odd.0, odd.1), Seat::P2);
        assert_eq!(
            host_seat(any(Arrival::First), any(Arrival::Second), even.0, even.1),
            Seat::P1
        );
        assert_eq!(
            host_seat(any(Arrival::First), any(Arrival::Second), odd.0, odd.1),
            Seat::P2
        );
        assert_eq!(
            host_seat(any(Arrival::Second), any(Arrival::First), even.0, even.1),
            Seat::P2
        );
        assert_eq!(
            host_seat(any(Arrival::Second), any(Arrival::First), odd.0, odd.1),
            Seat::P1
        );
    }
}
