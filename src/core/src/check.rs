//! CHECK（ズレの検出）の突き合わせ。
//!
//! frame 0と以後30フレームごとに自分の値（乱数・抽選回数・両者のライフ）を直近16回分（480フレーム）持つ。
//! 相手の値と両方揃ったら比べる（送り直さない）。相手が先に進んでいることがあるので相手の値も16回分持つ。
//! 1つ先のマッチの相手の値も持ち、そのマッチに入ったら比べる（読み込みの短い側が先にframe 0のCHECKを送るため）。
//! それ以外のマッチのものは捨てる。ゲームスレッドで呼ぶので固定長の表（16枠）だけを使う。

use crate::wire::{Check, CheckKind, WatchCheck};

/// CHECKを送る間隔（フレーム）。
pub const CHECK_INTERVAL: u32 = 30;
/// 持っておく回数（480フレーム分）。
pub const CHECK_KEEP: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckRule {
    /// 4つのフィールドを全部比べる（ダイジェストも抽選回数のフィールドに詰めるのでframe 0でも外さない）。
    #[default]
    All,
    /// frame 0だけ抽選回数を比べない（ゲームの値を書き込むときの形）。
    /// 抽選回数`0x004ACE10`は優先度7のon_tick `0x00420190`が毎tick 0に戻す。ただし読み込みの間はon_tickが走らないので溜まる。
    /// frame 0の値は読み込みの前後の場面での抽選を含みうる（未確認）。frame 30からは比べる。
    SkipCountAtFrame0,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Match {
        frame: u32,
    },
    /// 合わなかった。両者の値をログへ書く。BYE(ズレ検出)を出す。
    Mismatch {
        local: Check,
        remote: Check,
    },
}

/// 1枠。`done`は比べ終えたかどうかのフラグ（重複して届いたCHECKで2回報告しない）。
#[derive(Debug, Clone, Copy)]
struct Slot {
    check: Check,
    done: bool,
}

/// 両者のCHECKの記録。マッチごとに`begin_seg`で空にする。
#[derive(Debug, Clone)]
pub struct CheckLog {
    seg: u16,
    local: [Option<Slot>; CHECK_KEEP],
    remote: [Option<Check>; CHECK_KEEP],
    /// 次のマッチ（seg + 1）の相手の値。そのマッチに入ったら`remote`へ移す。
    next: [Option<Check>; CHECK_KEEP],
    latest_local: Option<u32>,
    dropped: u32,
    rule: CheckRule,
}

pub fn is_check_frame(frame: u32) -> bool {
    frame.is_multiple_of(CHECK_INTERVAL)
}

/// 観戦者へ送る流れにホストの照合の記録（`wire::Record::Check`）を積む間隔（フレーム。約1秒）。対戦のCHECKの2回に1回。値はCHECKと同じ作り方。
pub const WATCH_CHECK_INTERVAL: u32 = 2 * CHECK_INTERVAL;

pub fn is_watch_check_frame(frame: u32) -> bool {
    frame.is_multiple_of(WATCH_CHECK_INTERVAL)
}

// ---- 入力の列のダイジェスト（検査用）----
// ゲームの乱数とライフを持たない相手（peer_sim）とは、CHECKのrngのフィールドにそれまでに使った (1P, 2P)の列のダイジェストを入れて比べる。

/// ダイジェストの初期値（FNV-1a 64 bit）。マッチをまたいで続ける。
pub const INPUT_DIGEST_INIT: u64 = 0xCBF2_9CE4_8422_2325;

/// ダイジェストに1フレーム（1P, 2Pの順）を足す。
pub fn input_digest_push(h: u64, p1: u16, p2: u16) -> u64 {
    let mut h = h;
    for b in p1.to_le_bytes().into_iter().chain(p2.to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// ダイジェストをCHECKのフィールドに詰める（下16 bitをrng_state、次の32 bitをrng_count、ライフは0）。
pub fn check_from_input_digest(seg: u16, frame: u32, digest: u64) -> Check {
    Check {
        seg,
        frame,
        rng_state: digest as u16,
        rng_count: (digest >> 16) as u32,
        life_1p: 0,
        life_2p: 0,
    }
}

fn slot_of(frame: u32) -> usize {
    // 添字は常に0..CHECK_KEEP（パニックしない）。
    ((frame / CHECK_INTERVAL) as usize) % CHECK_KEEP
}

fn same_values(a: &Check, b: &Check, rule: CheckRule) -> bool {
    let count_ok = match rule {
        CheckRule::All => a.rng_count == b.rng_count,
        CheckRule::SkipCountAtFrame0 => a.frame == 0 || a.rng_count == b.rng_count,
    };
    a.rng_state == b.rng_state && count_ok && a.life_1p == b.life_1p && a.life_2p == b.life_2p
}

impl CheckLog {
    pub fn new(seg: u16) -> Self {
        Self::with_rule(seg, CheckRule::All)
    }

    pub fn with_rule(seg: u16, rule: CheckRule) -> Self {
        CheckLog {
            seg,
            local: [None; CHECK_KEEP],
            remote: [None; CHECK_KEEP],
            next: [None; CHECK_KEEP],
            latest_local: None,
            dropped: 0,
            rule,
        }
    }

    pub fn rule(&self) -> CheckRule {
        self.rule
    }

    /// 新しいマッチに入る。前のマッチの値は比べない。
    /// 1つ先なら届いていた相手の値を引き継ぐ。飛ばしたなら捨てた数に足す。
    pub fn begin_seg(&mut self, seg: u16) {
        let early = self.next;
        let carried = seg == self.seg.wrapping_add(1);
        let mut dropped = self.dropped;
        if !carried {
            dropped += early.iter().flatten().count() as u32;
        }
        *self = CheckLog {
            dropped,
            ..CheckLog::with_rule(seg, self.rule)
        };
        if carried {
            self.remote = early;
        }
    }

    pub fn seg(&self) -> u16 {
        self.seg
    }

    /// 比べずに捨てた相手のCHECKの数。
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// 自分の値を記録する。相手の値が届いていれば比べる。
    pub fn record_local(&mut self, c: Check) -> Option<CheckResult> {
        if c.seg != self.seg || !is_check_frame(c.frame) {
            return None;
        }
        let s = slot_of(c.frame);
        self.local[s] = Some(Slot {
            check: c,
            done: false,
        });
        self.latest_local = Some(self.latest_local.map_or(c.frame, |l| l.max(c.frame)));
        let r = self.remote[s].filter(|r| r.frame == c.frame)?;
        self.remote[s] = None;
        self.compare(s, r)
    }

    /// 相手の値を受ける。自分の値があれば比べ、まだなら持っておく。
    pub fn on_remote(&mut self, r: &Check) -> Option<CheckResult> {
        if !is_check_frame(r.frame) {
            self.dropped += 1;
            return None;
        }
        if r.seg == self.seg.wrapping_add(1) {
            // 相手が先に次のマッチに入った。自分が入るまで持っておく（重複は上書き）。
            self.next[slot_of(r.frame)] = Some(*r);
            return None;
        }
        if r.seg != self.seg {
            self.dropped += 1;
            return None;
        }
        // 16回分より古いものは比べない（枠は新しい値で上書き済み）。
        if let Some(l) = self.latest_local
            && r.frame + CHECK_INTERVAL * CHECK_KEEP as u32 <= l
        {
            self.dropped += 1;
            return None;
        }
        let s = slot_of(r.frame);
        match self.local[s] {
            Some(slot) if slot.check.frame == r.frame => self.compare(s, *r),
            _ => {
                self.remote[s] = Some(*r);
                None
            }
        }
    }

    fn compare(&mut self, s: usize, r: Check) -> Option<CheckResult> {
        let slot = self.local[s].as_mut()?;
        if slot.done {
            return None;
        }
        slot.done = true;
        let local = slot.check;
        if same_values(&local, &r, self.rule) {
            Some(CheckResult::Match { frame: local.frame })
        } else {
            Some(CheckResult::Mismatch { local, remote: r })
        }
    }
}

/// 観戦者が自分のゲームで作った照合の値（同じframeの2通り。ホストの記録がどちらの作り方かは記録が持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnCheck {
    pub game: WatchCheck,
    pub digest: WatchCheck,
}

/// 観戦者の照合の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchVerdict {
    Match {
        frame: u32,
    },
    Mismatch {
        kind: CheckKind,
        host: WatchCheck,
        own: WatchCheck,
    },
}

/// 観戦者の照合: ホストの`Record::Check`と同じframeの自分の値をどちらが先に来てもペアにして比べる。
/// delay方式のホストはそのframeの`Frame`の前に積む。rollbackのホストは確定した後（`Frame`の後）に積む。そのため両方の順を持つ。
/// 1秒に1回なので枠は1つずつ。
/// マッチの始まりで`clear`する（frameの番号はマッチごとに0から）。
#[derive(Debug, Clone, Copy, Default)]
pub struct WatchChecks {
    host: Option<(CheckKind, WatchCheck)>,
    own: Option<OwnCheck>,
}

impl WatchChecks {
    pub fn clear(&mut self) {
        *self = WatchChecks::default();
    }

    pub fn on_host(&mut self, kind: CheckKind, c: WatchCheck) -> Option<WatchVerdict> {
        if let Some(own) = self.own.filter(|o| o.game.frame == c.frame) {
            *self = WatchChecks::default();
            return Some(judge(kind, c, own));
        }
        self.host = Some((kind, c));
        None
    }

    pub fn on_own(&mut self, own: OwnCheck) -> Option<WatchVerdict> {
        if let Some((kind, c)) = self.host.filter(|(_, h)| h.frame == own.game.frame) {
            *self = WatchChecks::default();
            return Some(judge(kind, c, own));
        }
        self.own = Some(own);
        None
    }
}

fn judge(kind: CheckKind, host: WatchCheck, own: OwnCheck) -> WatchVerdict {
    let (mine, rule) = match kind {
        CheckKind::Game => (own.game, CheckRule::SkipCountAtFrame0),
        CheckKind::InputDigest => (own.digest, CheckRule::All),
    };
    let as_check = |c: &WatchCheck| Check {
        seg: 0,
        frame: c.frame,
        rng_state: c.rng_state,
        rng_count: c.rng_count,
        life_1p: c.life_1p,
        life_2p: c.life_2p,
    };
    if same_values(&as_check(&host), &as_check(&mine), rule) {
        WatchVerdict::Match { frame: host.frame }
    } else {
        WatchVerdict::Mismatch {
            kind,
            host,
            own: mine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chk(seg: u16, frame: u32, rng_state: u16) -> Check {
        Check {
            seg,
            frame,
            rng_state,
            rng_count: 1000 + frame,
            life_1p: 5,
            life_2p: 4,
        }
    }

    #[test]
    fn check_frames_are_0_and_every_30() {
        assert!(is_check_frame(0));
        assert!(is_check_frame(30));
        assert!(is_check_frame(450));
        assert!(!is_check_frame(1));
        assert!(!is_check_frame(29));
        assert!(!is_check_frame(31));
    }

    #[test]
    fn input_digest_sees_seat_swap_and_order() {
        let push = input_digest_push;
        let a = push(push(INPUT_DIGEST_INIT, 1, 2), 3, 4);
        assert_ne!(a, push(push(INPUT_DIGEST_INIT, 2, 1), 3, 4));
        assert_ne!(a, push(push(INPUT_DIGEST_INIT, 3, 4), 1, 2));
        assert_eq!(a, push(push(INPUT_DIGEST_INIT, 1, 2), 3, 4));
    }

    #[test]
    fn input_digest_goes_into_the_rng_fields() {
        let c = check_from_input_digest(3, 60, 0x0123_4567_89AB_CDEF);
        assert_eq!((c.seg, c.frame), (3, 60));
        assert_eq!(c.rng_state, 0xCDEF);
        assert_eq!(c.rng_count, 0x4567_89AB);
        assert_eq!((c.life_1p, c.life_2p), (0, 0));
    }

    #[test]
    fn match_when_equal_local_first() {
        let mut log = CheckLog::new(3);
        assert_eq!(log.record_local(chk(3, 60, 0xAB)), None);
        assert_eq!(
            log.on_remote(&chk(3, 60, 0xAB)),
            Some(CheckResult::Match { frame: 60 })
        );
        assert_eq!(log.on_remote(&chk(3, 60, 0xAB)), None);
    }

    #[test]
    fn match_when_remote_first() {
        let mut log = CheckLog::new(3);
        assert_eq!(log.on_remote(&chk(3, 0, 0x11)), None);
        assert_eq!(
            log.record_local(chk(3, 0, 0x11)),
            Some(CheckResult::Match { frame: 0 })
        );
    }

    #[test]
    fn mismatch_in_each_single_field() {
        let base = chk(1, 30, 0x1234);
        let variants: [fn(&mut Check); 4] = [
            |c| c.rng_state ^= 1,
            |c| c.rng_count += 1,
            |c| c.life_1p = 0,
            |c| c.life_2p = 9,
        ];
        for (i, v) in variants.iter().enumerate() {
            let mut log = CheckLog::new(1);
            log.record_local(base);
            let mut r = base;
            v(&mut r);
            assert_eq!(
                log.on_remote(&r),
                Some(CheckResult::Mismatch {
                    local: base,
                    remote: r
                }),
                "欄 {i}"
            );
        }
        let mut log = CheckLog::new(1);
        let mut r = base;
        r.life_2p = 1;
        assert_eq!(log.on_remote(&r), None);
        assert_eq!(
            log.record_local(base),
            Some(CheckResult::Mismatch {
                local: base,
                remote: r
            })
        );
    }

    #[test]
    fn keeps_16_and_drops_the_17th_back() {
        let mut log = CheckLog::new(0);
        for k in 0..=16u32 {
            log.record_local(chk(0, k * 30, k as u16));
        }
        assert_eq!(
            log.on_remote(&chk(0, 30, 1)),
            Some(CheckResult::Match { frame: 30 })
        );
        // 16回前（0）は枠が480に上書き済みなので捨てる。
        assert_eq!(log.on_remote(&chk(0, 0, 0)), None);
        assert_eq!(log.dropped(), 1);
        assert_eq!(
            log.on_remote(&chk(0, 480, 16)),
            Some(CheckResult::Match { frame: 480 })
        );
    }

    #[test]
    fn other_seg_is_dropped() {
        let mut log = CheckLog::new(2);
        log.record_local(chk(2, 30, 7));
        assert_eq!(log.on_remote(&chk(1, 30, 99)), None);
        assert_eq!(log.dropped(), 1);
        assert_eq!(
            log.on_remote(&chk(2, 30, 7)),
            Some(CheckResult::Match { frame: 30 })
        );
        log.begin_seg(3);
        assert_eq!(log.on_remote(&chk(3, 30, 7)), None);
        assert_eq!(log.dropped(), 1);
    }

    #[test]
    fn next_seg_frame0_is_kept_until_entered() {
        let mut log = CheckLog::new(1);
        assert_eq!(log.on_remote(&chk(2, 0, 0x55)), None);
        assert_eq!(log.dropped(), 0);
        assert_eq!(log.record_local(chk(1, 0, 0x55)), None);
        log.begin_seg(2);
        assert_eq!(
            log.record_local(chk(2, 0, 0x55)),
            Some(CheckResult::Match { frame: 0 })
        );
        assert_eq!(log.dropped(), 0);
        let mut log = CheckLog::new(1);
        let mut r = chk(2, 0, 0x55);
        r.rng_count += 1;
        log.on_remote(&r);
        log.begin_seg(2);
        assert_eq!(
            log.record_local(chk(2, 0, 0x55)),
            Some(CheckResult::Mismatch {
                local: chk(2, 0, 0x55),
                remote: r
            })
        );
    }

    #[test]
    fn two_ahead_and_skipped_next_are_dropped() {
        let mut log = CheckLog::new(1);
        assert_eq!(log.on_remote(&chk(3, 0, 9)), None);
        assert_eq!(log.dropped(), 1);
        assert_eq!(log.on_remote(&chk(2, 0, 9)), None);
        assert_eq!(log.dropped(), 1);
        log.begin_seg(3);
        assert_eq!(log.dropped(), 2);
        assert_eq!(log.record_local(chk(3, 0, 9)), None);
    }

    #[test]
    fn next_seg_wraps_at_u16_max() {
        let mut log = CheckLog::new(u16::MAX);
        assert_eq!(log.on_remote(&chk(0, 0, 3)), None);
        assert_eq!(log.dropped(), 0);
        log.begin_seg(0);
        assert_eq!(
            log.record_local(chk(0, 0, 3)),
            Some(CheckResult::Match { frame: 0 })
        );
    }

    #[test]
    fn frame0_count_is_skipped_only_under_the_game_rule() {
        let rule = CheckRule::SkipCountAtFrame0;
        for seg in [0u16, 1] {
            let local = Check {
                rng_count: 2,
                ..chk(seg, 0, 0x77)
            };
            let remote = Check {
                rng_count: 1030,
                ..local
            };
            let fresh = || {
                let mut log = CheckLog::with_rule(0, rule);
                if seg == 1 {
                    log.begin_seg(1);
                }
                log
            };
            let mut log = fresh();
            assert_eq!(log.rule(), rule);
            log.record_local(local);
            assert_eq!(
                log.on_remote(&remote),
                Some(CheckResult::Match { frame: 0 }),
                "seg {seg}: frame 0 の抽選回数の違いでズレと言った"
            );
            let bends: [fn(&mut Check); 3] =
                [|c| c.rng_state ^= 1, |c| c.life_1p ^= 1, |c| c.life_2p ^= 1];
            for (i, bend) in bends.iter().enumerate() {
                let mut log = fresh();
                log.record_local(local);
                let mut r = remote;
                bend(&mut r);
                assert!(
                    matches!(log.on_remote(&r), Some(CheckResult::Mismatch { .. })),
                    "seg {seg}: frame 0 の欄 {i} を比べていない"
                );
            }
            let mut log = fresh();
            log.record_local(Check { frame: 30, ..local });
            assert!(
                matches!(
                    log.on_remote(&Check {
                        frame: 30,
                        ..remote
                    }),
                    Some(CheckResult::Mismatch { .. })
                ),
                "seg {seg}: frame 30 の抽選回数を比べていない"
            );
        }
        let local = Check {
            rng_count: 2,
            ..chk(0, 0, 0x77)
        };
        let mut log = CheckLog::new(0);
        assert_eq!(log.rule(), CheckRule::All);
        log.record_local(local);
        assert!(matches!(
            log.on_remote(&Check {
                rng_count: 1030,
                ..local
            }),
            Some(CheckResult::Mismatch { .. })
        ));
    }

    #[test]
    fn non_check_frame_is_dropped() {
        let mut log = CheckLog::new(0);
        log.record_local(chk(0, 30, 5));
        assert_eq!(log.on_remote(&chk(0, 31, 6)), None);
        assert_eq!(log.dropped(), 1);
    }

    fn wc(frame: u32, rng_state: u16, rng_count: u32) -> WatchCheck {
        WatchCheck {
            frame,
            rng_state,
            rng_count,
            life_1p: 9,
            life_2p: 8,
        }
    }

    fn own(frame: u32, rng_state: u16, rng_count: u32) -> OwnCheck {
        OwnCheck {
            game: wc(frame, rng_state, rng_count),
            digest: wc(frame, !rng_state, !rng_count),
        }
    }

    #[test]
    fn watch_check_frames_are_every_other_check_frame() {
        let hits: Vec<u32> = (0..=180).filter(|&f| is_watch_check_frame(f)).collect();
        assert_eq!(hits, vec![0, 60, 120, 180]);
        assert!(hits.iter().all(|&f| is_check_frame(f)));
    }

    #[test]
    fn a_watch_check_pairs_in_either_order_and_judges_by_the_hosts_kind() {
        // ホストが先（delay方式）。
        let mut w = WatchChecks::default();
        assert_eq!(w.on_host(CheckKind::Game, wc(60, 1, 2)), None);
        assert_eq!(
            w.on_own(own(60, 1, 2)),
            Some(WatchVerdict::Match { frame: 60 })
        );
        // 自分が先（rollbackの確定の後に積まれる）。
        assert_eq!(w.on_own(own(120, 3, 4)), None);
        assert_eq!(
            w.on_host(CheckKind::Game, wc(120, 3, 5)),
            Some(WatchVerdict::Mismatch {
                kind: CheckKind::Game,
                host: wc(120, 3, 5),
                own: wc(120, 3, 4),
            })
        );
        // ダイジェストの記録は自分のダイジェストと比べる（ゲームの値とは比べない）。
        assert_eq!(w.on_own(own(180, 7, 8)), None);
        assert_eq!(
            w.on_host(CheckKind::InputDigest, wc(180, !7, !8)),
            Some(WatchVerdict::Match { frame: 180 })
        );
        assert_eq!(w.on_own(own(240, 7, 8)), None);
        assert!(matches!(
            w.on_host(CheckKind::InputDigest, wc(240, 7, 8)),
            Some(WatchVerdict::Mismatch {
                kind: CheckKind::InputDigest,
                ..
            })
        ));
    }

    #[test]
    fn a_watch_check_of_another_frame_waits_and_frame_0_skips_only_the_games_count() {
        let mut w = WatchChecks::default();
        assert_eq!(w.on_own(own(60, 1, 2)), None);
        assert_eq!(
            w.on_host(CheckKind::Game, wc(120, 1, 2)),
            None,
            "frame の違う組を比べた"
        );
        assert_eq!(
            w.on_own(own(120, 1, 2)),
            Some(WatchVerdict::Match { frame: 120 })
        );
        // frame 0の抽選回数は読み込み中の抽選で両者が違ってよい（対戦のCHECKと同じ）。ダイジェストは全部比べる。
        assert_eq!(w.on_host(CheckKind::Game, wc(0, 5, 999)), None);
        assert_eq!(
            w.on_own(own(0, 5, 1)),
            Some(WatchVerdict::Match { frame: 0 })
        );
        assert_eq!(w.on_host(CheckKind::InputDigest, wc(0, !5, 999)), None);
        assert!(matches!(
            w.on_own(own(0, 5, 1)),
            Some(WatchVerdict::Mismatch { .. })
        ));
        // マッチの始まりで捨てる。
        assert_eq!(w.on_host(CheckKind::Game, wc(60, 1, 2)), None);
        w.clear();
        assert_eq!(w.on_own(own(60, 1, 2)), None, "前の seg の記録と比べた");
    }
}
