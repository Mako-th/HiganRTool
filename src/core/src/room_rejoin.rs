//! ルームの期限（10 分。閉じる番号 4408）が切れたときに、自動で入り直すかの判断（pure。dll の見張りが呼ぶ）。
//!
//! 入り直すのは全部が当てはまるときだけ:
//! - 失敗が期限切れ（`Failure::Expired`）。サーバーの誤り・429・満員などは自動で繰り返さない。
//! - 先に入った側（`Role::Offer`）。返す側の 4408 は相手のコードを返す 60 秒の期限なので、知らせて終える。
//! - 相手がまだ来ていない。相手のコードを読んでセッションの設定を置いた後は、つなぐ側に任せる。
//! - 前の入り直しから `MIN_GAP_SECS` 秒以上空いている（すぐ期限切れになる誤りでサーバーへ繋ぎ続けない）。

use crate::room::Failure;

/// 前の入り直しから次の入り直しまでの最短（秒）。ルームの期限（10 分）の間隔なら常に満たす。
pub const MIN_GAP_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Situation {
    pub first_in: bool,
    /// 相手がルームに来たか（`room::Event::Matched` を見たか）。
    pub peer_came: bool,
    pub placed: bool,
    /// 前の入り直しから経った秒（入り直したことが無ければ `None`）。
    pub since_last_rejoin_secs: Option<u64>,
}

pub fn should_rejoin(f: Failure, s: &Situation) -> bool {
    matches!(f, Failure::Expired)
        && s.first_in
        && !s.peer_came
        && !s.placed
        && s.since_last_rejoin_secs.is_none_or(|t| t >= MIN_GAP_SECS)
}

/// ランダム対戦で、待ち列へ入り直す間隔の最短（秒）。相手が入っては抜けるのを繰り返しても、サーバーへ繋ぎ続けない
/// （1 回の入り直しで 4 回繋ぐので、10 秒に 1 回なら IP ごとの回数の制限〔1 分に 30 回〕に収まる）。
pub const REQUEUE_MIN_GAP_SECS: u64 = 10;
const _: () = assert!(60 / REQUEUE_MIN_GAP_SECS * 4 <= 30);

/// ランダム対戦で失敗したとき、待ち列へ入り直すか。待ち列の期限切れ、割り当てられたルームの期限切れ
/// （片方が来なかった）、相手がルームで抜けたときに入り直す。相手のコードを読んでセッションの設定を置いた後（`placed`）は、
/// つなぐ側に任せる。サーバーの誤り・429 などは自動で繰り返さない。
pub fn should_requeue(f: Failure, placed: bool, since_last_requeue_secs: Option<u64>) -> bool {
    matches!(f, Failure::Expired | Failure::PeerLeft)
        && !placed
        && since_last_requeue_secs.is_none_or(|t| t >= REQUEUE_MIN_GAP_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::Why;

    #[test]
    fn a_random_match_requeues_after_an_expiry_or_a_departure_but_not_after_placing() {
        assert!(should_requeue(Failure::Expired, false, None));
        assert!(should_requeue(Failure::PeerLeft, false, None));
        for f in [
            Failure::Unresponsive(Why::NoAnswer),
            Failure::Unresponsive(Why::Closed(1011)),
            Failure::RateLimited {
                retry_after_secs: 60,
            },
            Failure::RoomFull,
            Failure::BadRoomName,
        ] {
            assert!(!should_requeue(f, false, None), "{f:?}");
        }
        assert!(!should_requeue(Failure::Expired, true, None), "置いた後");
        assert!(!should_requeue(Failure::PeerLeft, true, None), "置いた後");
    }

    #[test]
    fn requeues_are_at_least_ten_seconds_apart() {
        for (gap, want) in [
            (Some(0), false),
            (Some(REQUEUE_MIN_GAP_SECS - 1), false),
            (Some(REQUEUE_MIN_GAP_SECS), true),
            (Some(600), true),
            (None, true),
        ] {
            assert_eq!(
                should_requeue(Failure::PeerLeft, false, gap),
                want,
                "{gap:?}"
            );
        }
    }

    fn waiting() -> Situation {
        Situation {
            first_in: true,
            peer_came: false,
            placed: false,
            since_last_rejoin_secs: None,
        }
    }

    #[test]
    fn only_an_expiry_of_a_waiting_host_rejoins() {
        assert!(should_rejoin(Failure::Expired, &waiting()));
        for f in [
            Failure::Unresponsive(Why::NoAnswer),
            Failure::Unresponsive(Why::Closed(1011)),
            Failure::RateLimited {
                retry_after_secs: 60,
            },
            Failure::RoomFull,
            Failure::BadRoomName,
            Failure::PeerLeft,
        ] {
            assert!(!should_rejoin(f, &waiting()), "{f:?}");
        }
    }

    #[test]
    fn a_guest_a_matched_room_and_a_placed_box_never_rejoin() {
        let mut s = waiting();
        s.first_in = false;
        assert!(!should_rejoin(Failure::Expired, &s), "返す側");
        let mut s = waiting();
        s.peer_came = true;
        assert!(!should_rejoin(Failure::Expired, &s), "相手が来た後");
        let mut s = waiting();
        s.placed = true;
        assert!(
            !should_rejoin(Failure::Expired, &s),
            "セッションの設定を置いた後"
        );
    }

    #[test]
    fn rejoins_are_at_least_a_minute_apart() {
        let mut s = waiting();
        for (gap, want) in [
            (Some(0), false),
            (Some(MIN_GAP_SECS - 1), false),
            (Some(MIN_GAP_SECS), true),
            (Some(600), true),
            (None, true),
        ] {
            s.since_last_rejoin_secs = gap;
            assert_eq!(should_rejoin(Failure::Expired, &s), want, "{gap:?}");
        }
        assert_eq!(MIN_GAP_SECS, 60, "決めた間隔は 60 秒");
    }
}
