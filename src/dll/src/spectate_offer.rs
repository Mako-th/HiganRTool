//! 配る側（インターネット越しの観戦 v1）: マッチのホストの dll が、観戦用のルーム（`RoomName::spectate_of`）に役 host で入って待ち、
//! 観戦者の返し（種別 4）を受けて、受信スレッドに穴あけ（`spectate::PunchPlan`）を頼む。
//!
//! ```text
//! 見張りのスレッド（net_code_game::GameWatcher）: つながった → マッチのホスト・ルームで会った相手・観戦の用意あり → spawn
//! 配る側のスレッド（higanr-offer。ここ）: core の Caster を 50 ms ごとに回す
//!    Connect  → RoomConn::start（観戦用のルームへ）
//!    MakeOffer → 観戦用のコード（同じ session・秘密・候補、新しい期限）を作って offer
//!    Punch(文字) → read_spectate_reply（タグ・session・期限）→ 候補をネットワークの範囲で絞る → Shared::request_punch
//! 受信スレッド（net_udp::recv_loop → SpectateSender::tick）: 頼みを見て PunchPlan を持ち替え、候補へ 4 B の印を送る
//! ```
//!
//! - ゲームのスレッドは何も呼ばない。確保・ブロッキング I/O は全部このスレッドと `RoomConn` の受信スレッドの側。
//! - ルーム名・秘密・コードは本ログに書かず、`Debug` にも出さない（観戦用のルームの名は元のルーム名のハッシュだが、書かない）。
//! - 数は `Shared` の原子変数に積み、本ログの観戦の集計の行（`spectate_host::Snapshot::text`）に出す。
//! - 止めるのは `spectate_host::Shared::stop_offer`（つながりの片づけ）。止まったらルームの接続を手放す（閉じ始めるだけで待たない）。
//!
//! 観戦用のコードの session と対戦の session は別に引く（`spectate_host::pick_session`）。ゲーム内のメニューの経路では対戦の session は
//! ルームで相手と会ってから引くので、等しくならない保証は無い（偶然に同じになる確率は 2^-32。同じになると相手のパケットが観戦のホストへ回る）。
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::code::{self, Code, CodeError};
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::Name;
use higanr_core::room::{Event, Failure, Role, RoomName, Why};
use higanr_core::spectate::HostConfig;
use higanr_core::spectate_room::{Caster, CasterAction};
use higanr_core::watch_list::{HostEvent, Posting};

use crate::net_config::NetSettings;
use crate::net_room::{HostListConn, RoomConn, RoomOptions, SubmitRefused};
use crate::spectate_host::{self, Shared, SpectateSender};
use crate::spectate_ring::SpectateRing;

/// 配る側のスレッドが `Caster` を回す間隔（ms）。見張り（`net_code_game::GAME_POLL`）と同じ 50 ms。観戦者の JOIN は 250 ms ごとなので、
/// これより細かくしても早くならない。
const POLL: Duration = Duration::from_millis(50);

// ---------------- 数 ----------------

/// 数える出来事（`OfferStats` の添字）。
#[derive(Debug, Clone, Copy)]
enum Stat {
    Entered,
    /// 役 guest を受けた（観戦者が先に入っていた）ので閉じた。
    WrongRole,
    Offered,
    OfferFailed,
    Replies,
    /// 返しのタグが合わない（観戦用の秘密を知らない者が作った・書き換えた）。
    BadTag,
    WrongSession,
    /// 返しの期限が切れている。
    Expired,
    Unreadable,
    NoRoute,
    PunchRequested,
    Done,
    FailExpired,
    FailPeerLeft,
    FailFull,
    FailRate,
    FailUnresponsive,
    FailBadName,
    ConnectErrors,
    /// ルームの運びの記録（WinHTTP の失敗・サーバーのバイナリ）。
    Transport,
    Panicked,
    /// 進行中のランダム対戦の一覧に載った回数・載せられなかった（切れた・断られた）回数。
    Listed,
    ListFailed,
}

const STAT_COUNT: usize = Stat::ListFailed as usize + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OfferCounts {
    pub armed: bool,
    pub running: bool,
    pub entered: u32,
    pub wrong_role: u32,
    pub offered: u32,
    pub offer_failed: u32,
    pub replies: u32,
    pub bad_tag: u32,
    pub wrong_session: u32,
    pub expired: u32,
    pub unreadable: u32,
    pub no_route: u32,
    pub punch_requested: u32,
    pub done: u32,
    pub fail_expired: u32,
    pub fail_peer_left: u32,
    pub fail_full: u32,
    pub fail_rate: u32,
    pub fail_unresponsive: u32,
    pub fail_bad_name: u32,
    pub connect_errors: u32,
    pub transport: u32,
    pub panicked: u32,
    pub listed: u32,
    pub list_failed: u32,
}

impl OfferCounts {
    pub fn text(&self) -> String {
        format!(
            " / 配る側（ルームで観戦者を待つ。{}）: 入った {}（役が逆で閉じた {}）・観戦用のコードを預けた {}（預けられず {}）・観戦者の返し {}（読めず: タグ {}・session {}・期限切れ {}・その他 {}）・穴あけを頼んだ {}（候補が範囲の外で断った {}）・手順を終えた {} / ルームの失敗（期限 {}・相手が抜けた {}・満員 {}・混雑 {}・応答なし {}・ルーム名 {}）・繋げなかった {}・運びの記録 {}{} / 一覧（載った {}・載せられず {}）",
            if self.running {
                "起こした"
            } else {
                "まだ起こしていない"
            },
            self.entered,
            self.wrong_role,
            self.offered,
            self.offer_failed,
            self.replies,
            self.bad_tag,
            self.wrong_session,
            self.expired,
            self.unreadable,
            self.punch_requested,
            self.no_route,
            self.done,
            self.fail_expired,
            self.fail_peer_left,
            self.fail_full,
            self.fail_rate,
            self.fail_unresponsive,
            self.fail_bad_name,
            self.connect_errors,
            self.transport,
            if self.panicked > 0 {
                "・パニックで配る側を止めた（対戦は続く）"
            } else {
                ""
            },
            self.listed,
            self.list_failed,
        )
    }
}

#[derive(Debug, Default)]
pub struct OfferShared {
    armed: AtomicBool,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    stats: [AtomicU32; STAT_COUNT],
}

impl OfferShared {
    fn bump(&self, s: Stat) {
        if let Some(a) = self.stats.get(s as usize) {
            a.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn get(&self, s: Stat) -> u32 {
        self.stats
            .get(s as usize)
            .map_or(0, |a| a.load(Ordering::Relaxed))
    }

    fn lock_thread(&self) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
        self.thread.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn counts(&self) -> OfferCounts {
        let g = |s| self.get(s);
        OfferCounts {
            armed: self.armed.load(Ordering::Acquire),
            running: self.lock_thread().is_some(),
            entered: g(Stat::Entered),
            wrong_role: g(Stat::WrongRole),
            offered: g(Stat::Offered),
            offer_failed: g(Stat::OfferFailed),
            replies: g(Stat::Replies),
            bad_tag: g(Stat::BadTag),
            wrong_session: g(Stat::WrongSession),
            expired: g(Stat::Expired),
            unreadable: g(Stat::Unreadable),
            no_route: g(Stat::NoRoute),
            punch_requested: g(Stat::PunchRequested),
            done: g(Stat::Done),
            fail_expired: g(Stat::FailExpired),
            fail_peer_left: g(Stat::FailPeerLeft),
            fail_full: g(Stat::FailFull),
            fail_rate: g(Stat::FailRate),
            fail_unresponsive: g(Stat::FailUnresponsive),
            fail_bad_name: g(Stat::FailBadName),
            connect_errors: g(Stat::ConnectErrors),
            transport: g(Stat::Transport),
            panicked: g(Stat::Panicked),
            listed: g(Stat::Listed),
            list_failed: g(Stat::ListFailed),
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// 止めて、配る側のスレッドが終わるのを待つ（つながりの片づけ。最大で 1 回の見回り —— 50 ms とルームへの送りの待ち）。起こしていなければ何もしない。
    pub fn stop_and_join(&self) {
        self.stop();
        let h = self.lock_thread().take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

// ---------------- 用意 ----------------

/// 受信スレッドと見張りが持つ、観戦のホストの本体（リングバッファと送り手）。
pub struct Parts {
    pub ring: Arc<SpectateRing>,
    pub sender: SpectateSender,
}

/// 配る側のスレッドの材料（見張りが持ち、つながったとき `spawn` へ渡す）。`Debug` は書かない（秘密を含む）。
pub struct OfferKit {
    session_id: u32,
    secret: Secret,
    /// 観戦用の使い捨ての鍵の公開鍵（入るたびに作り直す観戦用のコードへ載せる。配る側のセッションの秘密鍵と対）。
    host_public: [u8; higanr_core::seal::PUBLIC_LEN],
    candidates: Candidates,
    name: Name,
    scope: Scope,
    /// 受信ソケットの族（観戦者の候補のうち、受信スレッドの受信ソケットから送れる族だけに穴をあける）。
    v4: bool,
    room: RoomOptions,
    shared: Arc<Shared>,
}

pub struct Armed {
    pub parts: Parts,
    pub kit: OfferKit,
}

/// 観戦を使う設定（`[spectate] enable = 1`）のとき、ゲーム内のメニューの経路の観戦のホストを用意する（読み取りのスレッド）。
/// `local` は受信ソケット、`mapped` は STUN で見えたアドレスで、観戦用のコードの候補はこの 2 つ（IPv6 は載せない。受信スレッドの観戦は IPv4 の受信ソケットだけ）。
/// 秘密と session はつながりごとに引く。`room` が無ければ観戦用のルームへ入れないので用意しない（理由を返す）。
pub fn arm_with(
    s: &NetSettings,
    local: SocketAddr,
    mapped: Option<SocketAddr>,
    room: Option<RoomOptions>,
    mut draw_session: impl FnMut() -> Result<u32, String>,
    draw_secret: impl FnOnce() -> Result<Secret, String>,
) -> Result<Armed, String> {
    let room = room.ok_or("ルームサーバー（room_server）が無い（観戦用のルームへ入れない）")?;
    let session_id = spectate_host::pick_session(&mut draw_session, s.session_id)?;
    let secret = draw_secret()?;
    // 観戦用の使い捨ての鍵: つながりごとに OS の乱数で引く。公開鍵は観戦用のコード（ルームサーバーへ預ける）に載せる。
    let host = HostConfig {
        session_id,
        secret,
        key_seed: crate::net_udp::draw_key_seed()?,
    };
    let host_public = host.public();
    let candidates = s.candidates_for(local, mapped, None)?;
    let ring = Arc::new(SpectateRing::new());
    let shared = Arc::new(Shared::default());
    shared.offer.armed.store(true, Ordering::Release);
    let sender = SpectateSender::new(host, Arc::clone(&ring), s.scope, Arc::clone(&shared));
    Ok(Armed {
        parts: Parts { ring, sender },
        kit: OfferKit {
            session_id,
            secret,
            host_public,
            candidates,
            name: Name::new(&s.name),
            scope: s.scope,
            v4: local.is_ipv4(),
            room,
            shared,
        },
    })
}

/// OS の乱数で用意する（本物の経路）。戻りの文は本ログの「ネットワーク: 開いた」の行に足す（観戦を使わない設定なら空）。
pub fn arm(
    s: &NetSettings,
    local: SocketAddr,
    mapped: Option<SocketAddr>,
    room: Option<RoomOptions>,
) -> (Option<Armed>, String) {
    if s.spectate.is_none() {
        return (None, String::new());
    }
    match arm_with(
        s,
        local,
        mapped,
        room,
        || {
            let mut b = [0u8; 4];
            if !crate::net_win::os_random(&mut b) {
                return Err("観戦用の session を OS の乱数で引けない".to_string());
            }
            Ok(u32::from_le_bytes(b))
        },
        crate::net_udp::draw_secret,
    ) {
        Ok(a) => {
            let note = format!(
                " / 観戦の配る側を用意した（観戦用の session 0x{:08X}・候補 {} 件。ルームで会った相手と繋がった試合のホストだけが、観戦用のルームで観戦者を待つ）",
                a.kit.session_id,
                a.kit.candidates.len()
            );
            (Some(a), note)
        }
        Err(e) => (None, format!(" / 観戦は使わない: {e}")),
    }
}

impl OfferKit {
    #[cfg(test)]
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

/// 進行中のランダム対戦の一覧に載せる材料（ランダム対戦のホストだけ）。割り当てのルーム名は `posting.room`（サーバーが観戦用のルーム名を導く元）。
#[derive(Debug, Clone, Copy)]
pub struct ListingInfo {
    pub build: u32,
    pub posting: Posting,
}

/// 配る側のスレッドを起こす（見張りのスレッド。つながって、マッチのホストで、ルームで会った相手か、ランダム対戦の相手のとき）。`room` は元のルーム名
/// （ランダム対戦は割り当てのルーム名）。`listing` があれば、観戦者の数を載せつつ一覧に載せる。
/// 起こせなければ理由を返す（観戦だけを使わない。対戦は続く）。
pub fn spawn(kit: OfferKit, room: &RoomName, listing: Option<ListingInfo>) -> Result<(), String> {
    let shared = Arc::clone(&kit.shared);
    let mut runner = Runner::new(kit, room.spectate_of(), listing);
    let sh = Arc::clone(&shared);
    let h = std::thread::Builder::new()
        .name("higanr-offer".into())
        .spawn(move || {
            // パニックは受けて外へ出さない（スレッドの入口）。受けたら配る側だけを止める。
            let r = catch_unwind(AssertUnwindSafe(|| runner.run()));
            if r.is_err() {
                sh.offer.bump(Stat::Panicked);
            }
        })
        .map_err(|e| format!("観戦の配る側のスレッドを起こせない: {e}"))?;
    *shared.offer.lock_thread() = Some(h);
    Ok(())
}

// ---------------- 回す ----------------

pub(crate) struct Runner {
    kit: OfferKit,
    /// 観戦用のルームの名（元のルーム名のハッシュ。`Debug` に出さない）。
    room: RoomName,
    caster: Caster,
    conn: Option<RoomConn>,
    listing: Option<Listing>,
    t0: Instant,
}

/// 一覧に載せる接続（ランダム対戦のホストだけ）。切れたら、決まった間隔で入り直す。
struct Listing {
    info: ListingInfo,
    conn: Option<HostListConn>,
    /// 次に繋ぎ直してよい時刻（単調な時計の ms。繋いでいる間は使わない）。
    retry_at_ms: u64,
    /// 続けて載せられなかった回数（入り直しの間隔を倍々にする）。
    fails: u32,
    /// 載った後に送った観戦者の数（変わったときだけ送る）。
    watchers: u8,
    /// 件数の上限で断られた（入り直さない）。
    gave_up: bool,
}

/// 一覧へ入り直す最初の間隔と上限。サーバーの回数の数え（1 IP 30 回 / 分）に触れない長さ。
const RELIST_FIRST_MS: u64 = 30_000;
const RELIST_MAX_MS: u64 = 5 * 60_000;

impl Runner {
    pub(crate) fn new(kit: OfferKit, room: RoomName, listing: Option<ListingInfo>) -> Runner {
        Runner {
            kit,
            room,
            caster: Caster::new(0),
            conn: None,
            listing: listing.map(|info| Listing {
                info,
                conn: None,
                retry_at_ms: 0,
                fails: 0,
                watchers: 0,
                gave_up: false,
            }),
            t0: Instant::now(),
        }
    }

    /// スレッドの本体: 止められるまで 50 ms ごとに見回る。止められたらルームの接続を手放す。
    fn run(&mut self) {
        while !self.kit.shared.offer.stopped() {
            let now = u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.step(now, crate::net_udp::now_unix());
            std::thread::sleep(POLL);
        }
        self.close();
    }

    pub(crate) fn close(&mut self) {
        self.caster.stop();
        if let Some(mut c) = self.conn.take() {
            c.cancel();
        }
        if let Some(l) = self.listing.as_mut()
            && let Some(mut c) = l.conn.take()
        {
            c.cancel();
        }
    }

    fn stats(&self) -> &OfferShared {
        &self.kit.shared.offer
    }

    pub(crate) fn step(&mut self, now_ms: u64, unix: u64) {
        self.step_listing(now_ms);
        if let Some(CasterAction::Connect) = self.caster.poll(now_ms) {
            self.connect(now_ms);
        }
        let (evs, notes) = {
            let Some(conn) = self.conn.as_mut() else {
                return;
            };
            (conn.poll(), conn.take_notes().len())
        };
        for _ in 0..notes {
            self.stats().bump(Stat::Transport);
        }
        for e in evs {
            if self.conn.is_none() {
                // 手放した接続の残りの出来事は捨てる（`spectate_room` の使い方）。
                break;
            }
            self.on_event(e, now_ms, unix);
        }
        if self.conn.as_ref().is_some_and(RoomConn::is_finished) {
            self.conn = None;
        }
    }

    /// 一覧に載せる接続を見回る: 切れていれば間隔を置いて繋ぎ直し、観戦者の数が変わったら送り、届いた出来事で載った・載せられなかったを数える。
    fn step_listing(&mut self, now_ms: u64) {
        let Some(l) = self.listing.as_mut() else {
            return;
        };
        if l.gave_up {
            return;
        }
        let spectators = self.kit.shared.get().spectators;
        let watchers = u8::try_from(spectators)
            .unwrap_or(u8::MAX)
            .min(l.info.posting.cap);
        if l.conn.is_none() && now_ms >= l.retry_at_ms {
            match HostListConn::start_post(&self.kit.room, l.info.build, l.info.posting, watchers) {
                Ok(c) => {
                    l.watchers = watchers;
                    l.conn = Some(c);
                }
                Err(_) => {
                    self.kit.shared.offer.bump(Stat::ConnectErrors);
                    l.fails = l.fails.saturating_add(1);
                    l.retry_at_ms = now_ms + relist_delay(l.fails);
                }
            }
        }
        let Some(conn) = l.conn.as_mut() else {
            return;
        };
        if watchers != l.watchers {
            l.watchers = watchers;
            conn.set_watchers(watchers);
        }
        let evs = conn.poll();
        let notes = conn.take_notes().len();
        for _ in 0..notes {
            self.kit.shared.offer.bump(Stat::Transport);
        }
        for e in evs {
            match e {
                HostEvent::Listed => {
                    self.kit.shared.offer.bump(Stat::Listed);
                    l.fails = 0;
                }
                HostEvent::Failed(f) => {
                    self.kit.shared.offer.bump(Stat::ListFailed);
                    l.fails = l.fails.saturating_add(1);
                    // 件数の上限（全体・1 IP あたり）は入り直しても変わらない。429 は言われた秒数の後。
                    match f {
                        Failure::RoomFull => l.gave_up = true,
                        Failure::RateLimited { retry_after_secs } => {
                            l.retry_at_ms = now_ms + u64::from(retry_after_secs) * 1000;
                        }
                        _ => l.retry_at_ms = now_ms + relist_delay(l.fails),
                    }
                    l.conn = None;
                    break;
                }
            }
        }
        if l.conn.as_ref().is_some_and(HostListConn::is_finished) {
            l.conn = None;
        }
    }

    fn connect(&mut self, now_ms: u64) {
        match RoomConn::start(&self.kit.room, &self.room) {
            Ok(c) => self.conn = Some(c),
            Err(_) => {
                // スレッドを起こせない。`Caster` に「繋げなかった」の入口が無いので、応答しない扱い（30 秒から倍々で待つ）にする。
                self.stats().bump(Stat::ConnectErrors);
                self.caster
                    .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
            }
        }
    }

    fn drop_conn(&mut self) {
        self.conn = None;
    }

    fn on_event(&mut self, e: Event, now_ms: u64, unix: u64) {
        match e {
            Event::Joined { role, .. } => {
                if role == Role::Offer {
                    self.stats().bump(Stat::Entered);
                }
                match self.caster.on_joined(now_ms, role) {
                    Some(CasterAction::MakeOffer) => self.make_offer(now_ms, unix),
                    Some(CasterAction::Close) => {
                        self.stats().bump(Stat::WrongRole);
                        self.drop_conn();
                    }
                    _ => {}
                }
            }
            Event::Matched { .. } => {}
            Event::PeerCode(text) => {
                if let Some(CasterAction::Punch(text)) = self.caster.on_peer_code(text) {
                    self.punch(&text, unix);
                }
            }
            Event::Done => {
                self.stats().bump(Stat::Done);
                self.caster.on_done(now_ms);
                self.drop_conn();
            }
            Event::Failed(f) => {
                self.stats().bump(match f {
                    Failure::Expired => Stat::FailExpired,
                    Failure::PeerLeft => Stat::FailPeerLeft,
                    Failure::RoomFull => Stat::FailFull,
                    Failure::RateLimited { .. } => Stat::FailRate,
                    Failure::Unresponsive(_) => Stat::FailUnresponsive,
                    Failure::BadRoomName => Stat::FailBadName,
                });
                self.caster.on_failed(now_ms, f);
                self.drop_conn();
            }
        }
    }

    /// 観戦用のコード（同じ session・秘密・候補、いまから新しい期限）を作って offer する。
    fn make_offer(&mut self, now_ms: u64, unix: u64) {
        let text = match code::write_spectate(
            &Code {
                side: code::Side::Spectate,
                session_id: self.kit.session_id,
                secret: Some(self.kit.secret),
                name: self.kit.name,
                nat: higanr_core::nat::NatMark::UNKNOWN,
                candidates: self.kit.candidates,
                expires: unix.saturating_add(code::DEFAULT_TTL_SECS),
            },
            &self.kit.host_public,
        ) {
            Ok(t) => t,
            Err(_) => return self.offer_failed(now_ms),
        };
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        match conn.submit_code(&text) {
            Ok(()) => self.stats().bump(Stat::Offered),
            // 送れなかった: 失敗の出来事は次の見回りで届く（そこで入り直しの待ちに入る）。
            Err(SubmitRefused::NotSent) => self.stats().bump(Stat::OfferFailed),
            Err(SubmitRefused::Core(_)) => self.offer_failed(now_ms),
        }
    }

    /// 観戦用のコードを作れない・ルームが受けない: この接続を閉じ、ゆっくり入り直す（応答しない扱い。30 秒から倍々で、5 分が上限）。
    fn offer_failed(&mut self, now_ms: u64) {
        self.stats().bump(Stat::OfferFailed);
        self.caster
            .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
        self.drop_conn();
    }

    /// 観戦者の返し（種別 4）を観戦用の秘密・session で確かめ、通れば候補をネットワークの範囲で絞って穴あけを頼む。
    fn punch(&mut self, text: &str, unix: u64) {
        let st = self.stats();
        st.bump(Stat::Replies);
        match code::read_spectate_reply(text, &self.kit.secret, self.kit.session_id, unix) {
            Ok(reply) => {
                let c = punch_candidates(&reply.candidates, self.kit.scope, self.kit.v4);
                if c.is_empty() {
                    st.bump(Stat::NoRoute);
                } else {
                    self.kit.shared.request_punch(c);
                    st.bump(Stat::PunchRequested);
                }
            }
            Err(CodeError::BadTag) => st.bump(Stat::BadTag),
            Err(CodeError::WrongSession { .. }) => st.bump(Stat::WrongSession),
            Err(CodeError::Expired { .. }) => st.bump(Stat::Expired),
            Err(_) => st.bump(Stat::Unreadable),
        }
    }
}

/// 一覧へ入り直すまでの間隔（失敗が続くほど倍々。上限 5 分）。
fn relist_delay(fails: u32) -> u64 {
    RELIST_FIRST_MS
        .saturating_mul(1u64 << fails.saturating_sub(1).min(4))
        .min(RELIST_MAX_MS)
}

/// 観戦者の候補から、穴あけを送ってよいものだけを残す（ネットワークの範囲 `scope` の中で、受信ソケットと同じ族のもの）。送る直前のガードでも
/// 同じ範囲を見る。ここで除くのは、頼みの数と本ログを実際に送る分と合わせるため。
pub fn punch_candidates(reply: &Candidates, scope: Scope, v4: bool) -> Candidates {
    let mut out = Candidates::EMPTY;
    for c in reply.as_slice() {
        if guard::allows(scope, c.addr) && c.addr.is_ipv4() == v4 {
            // 同じ候補は元の列で重ならない。入らなくても（満杯）残りを送れれば足りる。
            let _ = out.push(*c);
        }
    }
    out
}

#[cfg(test)]
mod tests;
