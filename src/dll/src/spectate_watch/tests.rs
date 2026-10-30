//! 観戦者の側の部品（`spectate_watch.rs`）の検査。ゲームのウィンドウを伴う組み上がりは`netplay/watch_tests.rs`。
use std::sync::Arc;

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::handshake::Name;

use super::*;

fn cfg(session: u32) -> WatcherConfig {
    WatcherConfig {
        session_id: session,
        secret: Secret([session as u8; 16]),
        nonce: 1,
        name: Name::new("w"),
        host: Candidates::from_addrs(&[std::net::SocketAddr::from(([127, 0, 0, 1], 47_001))])
            .unwrap(),
        host_public: [0x41; 32],
        key_seed: higanr_core::seal::KeySeed([session as u8; 32]),
    }
}

/// `cfg(session)`の観戦者の公開鍵（鍵の種はsessionの値で埋めてある）。
fn public_of(session: u32) -> [u8; 32] {
    higanr_core::seal::Ephemeral::from_seed(&higanr_core::seal::KeySeed([session as u8; 32]))
        .public()
}

#[test]
fn the_slot_hands_over_only_the_newest_config_once_and_never_waits() {
    let slot = WatchSlot::new();
    let mut seen = 0u32;
    assert_eq!(slot.generation(), 0);
    assert!(slot.take_newer(&mut seen).is_none());
    slot.offer(cfg(1));
    slot.offer(cfg(2));
    assert_eq!(slot.generation(), 2);
    // 監視の処理がロックを持っている間は待たない（取れなければ次のtick）。世代は進めない。
    {
        let _held = slot.cfg.lock().unwrap();
        assert!(slot.take_newer(&mut seen).is_none());
        assert_eq!(seen, 0, "取れなかったのに世代を進めた");
    }
    let got = slot.take_newer(&mut seen).expect("新しい設定");
    assert_eq!(got.watcher.public_key(), public_of(2), "古い設定を取った");
    assert_eq!(seen, 2);
    assert!(
        slot.take_newer(&mut seen).is_none(),
        "同じ設定を 2 回取った"
    );
    slot.offer(cfg(3));
    assert_eq!(
        slot.take_newer(&mut seen).map(|o| o.watcher.public_key()),
        Some(public_of(3))
    );
    assert!(!slot.is_joined() && !slot.is_done() && !slot.gave_up());
    slot.mark_joined();
    assert!(slot.is_joined() && !slot.is_done() && !slot.gave_up());
    slot.mark_done();
    slot.give_up();
    assert!(slot.is_done() && slot.gave_up());
}

#[test]
fn the_log_line_appears_only_after_a_watcher_published() {
    let s = WatchStatus::new();
    assert!(!s.published());
    assert_eq!(s.text(), "");
    let wp = WatchPlay::new(
        Arc::new(WatchSlot::new()),
        WatchConfig {
            wait_budget_us: 16_000,
            margin_frames: WATCH_MARGIN_FRAMES,
            require_net_selection: true,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    s.publish(&wp);
    assert!(s.published());
    let t = s.text();
    for want in [
        "観戦者: 段階 コードか返事を待つ",
        "終わり 未終了",
        "観戦者の遅れ",
        "観戦者の受け",
    ] {
        assert!(t.contains(want), "{want:?} が無い: {t}");
    }
}

#[test]
fn every_end_has_its_own_code_and_a_word() {
    let ends = [
        WatchEnd::HostEnded(ByeReason::Finished),
        WatchEnd::HostSilent,
        WatchEnd::GaveUp,
        WatchEnd::BadSelect(BadValue {
            field: "x",
            value: 1,
        }),
        WatchEnd::Stuck(Step {
            step: 3,
            kind: StepKind::Decide,
            value: 0,
            screen: 8,
        }),
        WatchEnd::HoldTimeout,
        WatchEnd::Lost,
        WatchEnd::Quit,
        WatchEnd::BufferFull,
        WatchEnd::Broken,
    ];
    let codes: Vec<u32> = ends.iter().map(|e| end_code(Some(*e)).0).collect();
    assert_eq!(codes, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(end_code(None), (0, 0));
    assert_eq!(ENDS.len(), 11);
    assert!(
        ENDS[end_code(Some(WatchEnd::Quit)).0 as usize].contains("自分で抜けた"),
        "本ログの終わりの理由に「自分で抜けた」が出る"
    );
    assert_eq!(end_code(Some(ends[4])).1, 3, "持ち越した押下の step");
}

#[test]
fn the_watchers_stop_point_wait_outlasts_the_players_wait_by_a_margin() {
    let player_ticks = u64::from(peer_wait::LIMIT_MS) * 60 / 1_000;
    let watcher_ticks = u64::from(HOLD_LIMIT_TICKS);
    assert!(watcher_ticks > player_ticks, "対戦者より先に観戦者が切れる");
    assert_eq!(watcher_ticks - player_ticks, 600, "余白は 10 秒（60 fps）");
}

#[test]
fn shot_pacer_asks_on_phase_change_screen_change_and_every_600_ticks_in_match() {
    use higanr_core::script::{SETTLE, TRANSITION};
    let mut p = ShotPacer::default();
    assert_eq!(
        p.tick(Phase::Unconnected, None),
        0,
        "最初は基準を覚えるだけ"
    );
    assert_eq!(p.tick(Phase::Unconnected, None), 0);
    assert_eq!(p.tick(Phase::Selecting, None), TRANSITION, "段階が変わった");
    assert_eq!(p.tick(Phase::Selecting, None), 0);
    assert_eq!(p.tick(Phase::Selecting, Some(7)), TRANSITION | SETTLE);
    assert_eq!(p.tick(Phase::Selecting, Some(7)), 0);
    assert_eq!(p.tick(Phase::Selecting, Some(8)), TRANSITION | SETTLE);
    assert_eq!(p.tick(Phase::Selecting, Some(16)), TRANSITION | SETTLE);
    assert_eq!(p.tick(Phase::Holding, Some(16)), TRANSITION);
    assert_eq!(
        p.tick(Phase::Holding, Some(8)),
        0,
        "選択の外の画面の変化は頼まない"
    );
    assert_eq!(p.tick(Phase::Loading, Some(8)), TRANSITION);
    assert_eq!(p.tick(Phase::Lockstep, Some(8)), TRANSITION);
    let mut asked = Vec::new();
    for n in 1..=1300u32 {
        if p.tick(Phase::Lockstep, Some(8)) != 0 {
            asked.push(n);
        }
    }
    assert_eq!(asked, vec![600, 1200], "試合の 600 tick ごと");
    assert_eq!(p.tick(Phase::Ended, Some(8)), TRANSITION);
    for _ in 0..1300 {
        assert_eq!(p.tick(Phase::Ended, Some(8)), 0, "終わった後は頼まない");
    }
    let mut q = ShotPacer::default();
    q.tick(Phase::Loading, None);
    q.tick(Phase::Lockstep, None);
    for _ in 0..599 {
        q.tick(Phase::Lockstep, None);
    }
    q.tick(Phase::Loading, None);
    q.tick(Phase::Lockstep, None);
    assert_eq!(q.tick(Phase::Lockstep, None), 0, "数え直している");
}

#[test]
fn shot_step_code_carries_phase_and_screen_above_script_steps() {
    assert_eq!(shot_step_code(Phase::Selecting, Some(16)), 0xF110);
    assert_eq!(shot_step_code(Phase::Lockstep, None), 0xF400);
}

#[test]
fn the_join_carries_the_ff_max_of_the_watch_config() {
    use crate::netplay::fake::FakeNet;
    use crate::netplay::{Game, Net};
    use higanr_core::wire::{self, MAX_DATAGRAM, Message};
    struct Idle;
    impl Game for Idle {
        fn scenes(&mut self) -> (u32, u32) {
            (1, 0)
        }
        fn local_input(&mut self, _s: higanr_core::wire::Seat) -> u16 {
            0
        }
        fn feed(&mut self, _i: u32, _w: u16) {}
        fn own_select(
            &mut self,
            _s: higanr_core::wire::Seat,
            _r: higanr_core::lockstep::Role,
        ) -> higanr_core::wire::Select {
            unreachable!()
        }
        fn apply_agreed(
            &mut self,
            _p: higanr_core::wire::Seat,
            _a: &higanr_core::selection::Agreed,
        ) -> Result<(), crate::netplay::BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            true
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            (0, 0, 0, 0)
        }
    }
    // 既定（8）と違う値を使い、範囲の両端も含める。
    for ff_max in [3u32, 1, 16] {
        let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_302));
        let h = std::net::SocketAddr::from(([127, 0, 0, 1], 47_001));
        let (mut net, mut host, _clock) = FakeNet::pair(a, h, 0);
        let slot = Arc::new(WatchSlot::new());
        let c = cfg(6);
        slot.offer(c);
        let mut wp = WatchPlay::new(
            Arc::clone(&slot),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: 3,
                require_net_selection: false,
                ff_max,
            },
        );
        wp.on_input(&mut Idle, &mut net);
        let mut buf = [0u8; MAX_DATAGRAM];
        let (_, n, _) = host.recv(&mut buf).expect("JOIN が届いていない");
        let key = higanr_core::auth::AuthKey::derive(&c.secret, c.session_id);
        match wire::decode(&key, None, &buf[..n]).map(|p| p.msg) {
            Ok(Message::SpectateJoin(j)) => assert_eq!(u32::from(j.ff_max), ff_max),
            other => panic!("JOIN でない: {other:?}"),
        }
    }
}

#[test]
fn a_give_up_ends_a_watch_that_has_a_watcher_but_is_not_joined() {
    use crate::netplay::fake::FakeNet;
    use crate::netplay::{Game, Phase};
    let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_301));
    let h = std::net::SocketAddr::from(([127, 0, 0, 1], 47_001));
    let (mut net, _other, _clock) = FakeNet::pair(a, h, 0);
    let slot = Arc::new(WatchSlot::new());
    slot.offer(cfg(5));
    let mut wp = WatchPlay::new(
        Arc::clone(&slot),
        WatchConfig {
            wait_budget_us: 16_000,
            margin_frames: 3,
            require_net_selection: false,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    struct Idle;
    impl Game for Idle {
        fn scenes(&mut self) -> (u32, u32) {
            (1, 0)
        }
        fn local_input(&mut self, _s: higanr_core::wire::Seat) -> u16 {
            0
        }
        fn feed(&mut self, _i: u32, _w: u16) {}
        fn own_select(
            &mut self,
            _s: higanr_core::wire::Seat,
            _r: higanr_core::lockstep::Role,
        ) -> higanr_core::wire::Select {
            unreachable!()
        }
        fn apply_agreed(
            &mut self,
            _p: higanr_core::wire::Seat,
            _a: &higanr_core::selection::Agreed,
        ) -> Result<(), crate::netplay::BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            true
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            (0, 0, 0, 0)
        }
    }
    wp.on_input(&mut Idle, &mut net);
    assert!(wp.watcher_counts().is_some(), "Watcher を作っていない");
    assert_eq!(wp.phase(), Phase::Unconnected);
    slot.give_up();
    wp.on_input(&mut Idle, &mut net);
    assert_eq!(wp.end(), Some(WatchEnd::GaveUp));
    assert_eq!(wp.phase(), Phase::Ended);
    assert!(slot.is_done());
    assert!(wp.bye.is_none(), "繋がっていないのに BYE を積んだ");
    // 接続した後の`give_up`は観戦を切らない（別のslotと別のWatchPlayを使う。`Watcher`が接続した状態を作れないので、段階で見る）。
    let slot2 = Arc::new(WatchSlot::new());
    let mut wp2 = WatchPlay::new(
        Arc::clone(&slot2),
        WatchConfig {
            wait_budget_us: 16_000,
            margin_frames: 3,
            require_net_selection: false,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    wp2.set_phase_for_test(Phase::Selecting);
    slot2.give_up();
    wp2.on_input(&mut Idle, &mut net);
    assert_eq!(wp2.end(), None, "繋がった後の give_up で切った");
}

#[test]
fn a_watcher_asks_to_skip_the_save_screen_only_when_its_game_leaves_by_save_and_quit() {
    use crate::netplay::fake::FakeNet;
    use crate::netplay::{Action, Game, Phase};
    use crate::scripted::SCENE_MENU;
    struct AtEnd {
        cur: u32,
        next: u32,
        fed: Vec<(u32, u16)>,
    }
    impl Game for AtEnd {
        fn scenes(&mut self) -> (u32, u32) {
            (self.cur, self.next)
        }
        fn local_input(&mut self, _s: higanr_core::wire::Seat) -> u16 {
            0
        }
        fn feed(&mut self, i: u32, w: u16) {
            self.fed.push((i, w));
        }
        fn own_select(
            &mut self,
            _s: higanr_core::wire::Seat,
            _r: higanr_core::lockstep::Role,
        ) -> higanr_core::wire::Select {
            unreachable!()
        }
        fn apply_agreed(
            &mut self,
            _p: higanr_core::wire::Seat,
            _a: &higanr_core::selection::Agreed,
        ) -> Result<(), crate::netplay::BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            true
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            (0, 0, 0, 0)
        }
    }
    let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_302));
    let h = std::net::SocketAddr::from(([127, 0, 0, 1], 47_002));
    let (mut net, _other, _clock) = FakeNet::pair(a, h, 0);
    let wp_new = || {
        WatchPlay::new(
            Arc::new(WatchSlot::new()),
            WatchConfig {
                wait_budget_us: 16_000,
                margin_frames: 3,
                require_net_selection: true,
                ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
            },
        )
    };
    let mut wp = wp_new();
    wp.set_phase_for_test(Phase::Lockstep);
    let mut g = AtEnd {
        cur: SCENE_MATCH,
        next: REQ_SAVE_AND_QUIT,
        fed: Vec::new(),
    };
    assert_eq!(wp.dispatch(&mut g, &mut net, false), Action::Zero);
    assert_eq!(wp.phase(), Phase::Selecting);
    assert_eq!(g.fed, vec![(0, 0), (1, 0), (2, 0)], "0 を feed していない");
    assert!(wp.take_save_quit(), "保存して終了で頼まない");
    assert!(!wp.take_save_quit(), "1 回の要求で 2 回頼んだ");
    for (next, phase) in [
        (1, Phase::Selecting),
        (7, Phase::Selecting),
        (10, Phase::Loading),
    ] {
        let mut wp = wp_new();
        wp.set_phase_for_test(Phase::Lockstep);
        let mut g = AtEnd {
            cur: SCENE_MATCH,
            next,
            fed: Vec::new(),
        };
        assert_eq!(
            wp.dispatch(&mut g, &mut net, false),
            Action::Zero,
            "次 {next}"
        );
        assert_eq!(wp.phase(), phase, "次 {next}");
        assert!(!wp.take_save_quit(), "次 {next} で頼んだ");
    }
    let mut wp = wp_new();
    wp.set_phase_for_test(Phase::Ended);
    let mut g = AtEnd {
        cur: SCENE_MATCH,
        next: 0,
        fed: Vec::new(),
    };
    let mut step = |wp: &mut WatchPlay, cur: u32, next: u32| {
        g.cur = cur;
        g.next = next;
        assert_eq!(
            wp.dispatch(&mut g, &mut net, false),
            Action::Local,
            "{cur}・{next}"
        );
        wp.take_save_quit()
    };
    assert!(!step(&mut wp, SCENE_MATCH, 0), "試合の中で頼んだ");
    assert!(!step(&mut wp, SCENE_MATCH, 1), "キャラを変更で頼んだ");
    assert!(
        step(&mut wp, SCENE_MATCH, REQ_SAVE_AND_QUIT),
        "終わった後の保存して終了で頼まない"
    );
    assert!(
        !step(&mut wp, SCENE_MATCH, REQ_SAVE_AND_QUIT),
        "同じ要求が続いた tick にもう一度頼んだ"
    );
    assert!(!step(&mut wp, SCENE_MENU, 0), "試合の外で頼んだ");
    assert!(
        step(&mut wp, SCENE_MATCH, REQ_SAVE_AND_QUIT),
        "次の試合の保存して終了で頼まない"
    );
    assert!(g.fed.is_empty(), "終わった後に feed した");
}

#[test]
fn names_are_fixed_per_seg_by_the_first_complete_pair() {
    let mut wp = WatchPlay::new(
        Arc::new(WatchSlot::new()),
        WatchConfig {
            wait_budget_us: 0,
            margin_frames: 3,
            require_net_selection: true,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    let name = |seat, s: &str| Record::name(seat, s.as_bytes());
    let names = |wp: &WatchPlay| {
        wp.names()
            .map(|[a, b]| (a.as_str().to_string(), b.as_str().to_string()))
    };
    let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
    let recs = [
        Record::Seg { seg: 0 },
        name(Seat::P1, "p1"),
        name(Seat::P1, "p1x"),
        name(Seat::P2, "p2"),
        name(Seat::P1, "p1y"),
        name(Seat::P2, "p2y"),
    ];
    for r in &recs {
        assert!(wp.push_for_test(r));
    }
    assert!(matches!(
        wp.watcher_pop(),
        Some((_, Record::Seg { seg: 0 }))
    ));
    assert!(wp.watcher_pop().is_none());
    assert_eq!(
        names(&wp),
        pair("p1x", "p2"),
        "最初にそろった組から変わった"
    );
    for r in [
        Record::Seg { seg: 1 },
        name(Seat::P1, "q1"),
        name(Seat::P2, "q2"),
        name(Seat::P1, "q1y"),
    ] {
        assert!(wp.push_for_test(&r));
    }
    assert!(matches!(
        wp.watcher_pop(),
        Some((_, Record::Seg { seg: 1 }))
    ));
    assert!(wp.watcher_pop().is_none());
    assert_eq!(names(&wp), pair("q1", "q2"), "次の seg で組が替わらない");
    // 1サイドぶんしか来ないマッチでは前のペアのまま。
    for r in [Record::Seg { seg: 2 }, name(Seat::P1, "r1")] {
        assert!(wp.push_for_test(&r));
    }
    assert!(wp.watcher_pop().is_some());
    assert!(wp.watcher_pop().is_none());
    assert_eq!(names(&wp), pair("q1", "q2"), "片方の席だけで組が替わった");
}

#[test]
fn match_frame_words_are_masked_before_the_game_and_the_pause_memory() {
    use crate::netplay::fake::FakeNet;
    use crate::netplay::{Action, Game, Phase};
    struct Rec(Vec<(u32, u16)>);
    impl Game for Rec {
        fn scenes(&mut self) -> (u32, u32) {
            (SCENE_MATCH, 0)
        }
        fn local_input(&mut self, _s: higanr_core::wire::Seat) -> u16 {
            0
        }
        fn feed(&mut self, i: u32, w: u16) {
            self.0.push((i, w));
        }
        fn own_select(
            &mut self,
            _s: higanr_core::wire::Seat,
            _r: higanr_core::lockstep::Role,
        ) -> higanr_core::wire::Select {
            unreachable!()
        }
        fn apply_agreed(
            &mut self,
            _p: higanr_core::wire::Seat,
            _a: &higanr_core::selection::Agreed,
        ) -> Result<(), crate::netplay::BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            true
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            (0, 0, 0, 0)
        }
    }
    let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_304));
    let h = std::net::SocketAddr::from(([127, 0, 0, 1], 47_004));
    let (mut net, _other, _clock) = FakeNet::pair(a, h, 0);
    let mut wp = WatchPlay::new(
        Arc::new(WatchSlot::new()),
        WatchConfig {
            wait_budget_us: 0,
            margin_frames: 3,
            require_net_selection: true,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    wp.set_phase_for_test(Phase::Lockstep);
    wp.started = true;
    let shot = 0x0800;
    wp.look[0] = Some((
        0,
        Record::Frame {
            frame: 0,
            p1: shot | Z | 0x0100,
            p2: shot | 0xA000,
        },
    ));
    wp.test_backlog = Some(3);
    let mut g = Rec(Vec::new());
    assert!(matches!(
        wp.dispatch(&mut g, &mut net, false),
        Action::Played(_)
    ));
    assert_eq!(
        g.0,
        vec![(0, Z | 0x0100), (1, 0), (2, Z | 0x0100)],
        "P/Home などの手元で働くビットがゲームへ渡った"
    );
    assert_eq!(
        wp.last_words,
        (Z | 0x0100, 0),
        "ポーズの再開へ持ち越す語に残った"
    );
}

/// マッチの途中でワードが届かず止まったら次に使う前に溜まりが余裕（3）に戻るのを待つ。止まっていなければ溜まりが1でも待たない。
#[test]
fn a_stall_in_the_match_waits_for_the_margin_before_playing_again() {
    use crate::netplay::fake::FakeNet;
    use crate::netplay::{Action, Game, Phase};
    struct InMatch;
    impl Game for InMatch {
        fn scenes(&mut self) -> (u32, u32) {
            (SCENE_MATCH, 0)
        }
        fn local_input(&mut self, _s: higanr_core::wire::Seat) -> u16 {
            0
        }
        fn feed(&mut self, _i: u32, _w: u16) {}
        fn own_select(
            &mut self,
            _s: higanr_core::wire::Seat,
            _r: higanr_core::lockstep::Role,
        ) -> higanr_core::wire::Select {
            unreachable!()
        }
        fn apply_agreed(
            &mut self,
            _p: higanr_core::wire::Seat,
            _a: &higanr_core::selection::Agreed,
        ) -> Result<(), crate::netplay::BadValue> {
            unreachable!()
        }
        fn write_charge(&mut self, _p1: u8, _p2: u8) -> bool {
            true
        }
        fn check_values(&mut self) -> (u16, u32, u8, u8) {
            (0, 0, 0, 0)
        }
    }
    let a = std::net::SocketAddr::from(([127, 0, 0, 1], 41_303));
    let h = std::net::SocketAddr::from(([127, 0, 0, 1], 47_003));
    let (mut net, _other, _clock) = FakeNet::pair(a, h, 0);
    let mut wp = WatchPlay::new(
        Arc::new(WatchSlot::new()),
        WatchConfig {
            // ワードを待たずに止まる（描画だけのフレームにする）。
            wait_budget_us: 0,
            margin_frames: 3,
            require_net_selection: true,
            ff_max: higanr_core::spectate_pace::FF_UPDATES_DEFAULT,
        },
    );
    wp.set_phase_for_test(Phase::Lockstep);
    wp.started = true;
    let frame = |f: u32| {
        Some((
            f,
            Record::Frame {
                frame: f,
                p1: 0,
                p2: 0,
            },
        ))
    };
    let mut g = InMatch;
    // 止まらずに続くなら溜まり1でも使う。
    wp.look[0] = frame(0);
    wp.test_backlog = Some(1);
    assert!(matches!(
        wp.dispatch(&mut g, &mut net, false),
        Action::Played(_)
    ));
    // ワードが無くて止まった。
    wp.test_backlog = None;
    assert_eq!(wp.dispatch(&mut g, &mut net, false), Action::RenderOnly);
    assert_eq!(wp.stats().stalls, 1);
    // 1つ届いても余裕まで溜まるのを待つ。
    wp.look[0] = frame(1);
    wp.test_backlog = Some(1);
    assert_eq!(wp.dispatch(&mut g, &mut net, false), Action::RenderOnly);
    wp.test_backlog = Some(2);
    assert_eq!(wp.dispatch(&mut g, &mut net, false), Action::RenderOnly);
    assert_eq!(wp.stats().refill_ticks, 2);
    wp.test_backlog = Some(3);
    assert!(matches!(
        wp.dispatch(&mut g, &mut net, false),
        Action::Played(_)
    ));
    // 戻った後はまた溜まり1でも使う。
    wp.look[0] = frame(2);
    wp.test_backlog = Some(1);
    assert!(matches!(
        wp.dispatch(&mut g, &mut net, false),
        Action::Played(_)
    ));
    assert_eq!(wp.stats().refill_ticks, 2);
    let st = WatchStatus::new();
    st.publish(&wp);
    assert!(
        st.text()
            .contains("止まった後に溜まりが戻るのを待った描画だけのフレーム 2"),
        "{}",
        st.text()
    );
}
