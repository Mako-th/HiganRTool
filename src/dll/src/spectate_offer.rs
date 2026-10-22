//! 配る側（インターネット越しの観戦）: マッチのホストの dll が、観戦用のルーム（`RoomName::spectate_of`）に役 host で入って待ち、観戦者の返し（種別 4）を受けて、受信スレッドに穴あけ（`spectate::PunchPlan`）を頼む。
//!
//! - 見張りのスレッド（`net_code_game::GameWatcher`）が、接続してマッチのホスト・ルームで会った相手・観戦の用意ありのとき `spawn` する。配る側のスレッド（`higanr-offer`。ここ）が core の `Caster` を 50 ms ごとに回し、観戦用のルームへの接続（`RoomConn::start`）・観戦用のコード（同じ session・秘密、新しい期限。候補は STUN の聞き直しで外側のアドレスが変わったら作り直す）の offer・返し（`read_spectate_reply`）の検証と候補の絞り込み・`Shared::request_punch` を受け持つ。受信スレッド（`net_udp::recv_loop` → `SpectateSender::tick`）は頼みを見て `PunchPlan` を持ち替え、候補へ 4 B の印を送る。
//! - ゲームのスレッドは何も呼ばない。確保・ブロッキング I/O は全部このスレッドと `RoomConn` の受信スレッドの側。
//! - ルーム名・秘密・コードは本ログに書かず、`Debug` にも出さない。数は `Shared` の原子変数に積み、本ログの観戦の集計の行（`spectate_host::Snapshot::text`）に出す。止めるのは `spectate_host::Shared::stop_offer`（接続の片づけ。止まったらルームの接続を手放す。閉じ始めるだけで待たない）。
//!
//! 観戦コード（ルームを使わない観戦。`OnewayCode`）: 同じ観戦のホストの session・秘密・鍵で、ホストが頼まれるたびに新しい期限のコードを書く（書くのは `net_code_game` の見張り）。ルームサーバーが無い設定でも用意する。観戦者は返しを出さず、ホストは穴をあけない。
//! 観戦用のコードの session と対戦の session は別に引く（`spectate_host::pick_session`）。ゲーム内のメニューの経路では対戦の session はルームで相手と会ってから引くので、等しくならない保証は無い（偶然に同じになる確率は 2^-32。同じになると対戦の相手のパケットが観戦のホストへ回る）。
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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

use crate::net_config::{NetSettings, OwnCandidates};
use crate::net_room::{HostListConn, RoomConn, RoomOptions, SubmitRefused};
use crate::netplay::LinkSlot;
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
    /// STUN の聞き直しで外側のアドレスが変わり、観戦用のコードの候補を作り直した。
    Remapped,
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
    pub remapped: u32,
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
            " / 配信側（ルームで観戦者を待つ。{}）: 入った {}（役が逆で閉じた {}）・観戦コードを預けた {}（預け不能 {}・外側のアドレスの変化で作り直し {}）・観戦者の返答コード {}（読み取り不能: タグ {}・session {}・期限切れ {}・その他 {}）・穴あけを頼んだ {}（候補が範囲外で拒否 {}）・手順を終えた {} / ルームの失敗（期限 {}・相手が抜けた {}・満員 {}・混雑 {}・応答なし {}・ルーム名 {}）・接続できなかった {}・運びの記録 {}{} / 一覧（載った {}・載せ不能 {}）",
            if self.running {
                "作成済み"
            } else {
                "未作成"
            },
            self.entered,
            self.wrong_role,
            self.offered,
            self.offer_failed,
            self.remapped,
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
                "・パニックで停止（対戦は続く）"
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
    /// 配る側のスレッドが動いている（作る前に立て、終わるときに倒す）。受信スレッドは、これが立っている間は接続の後も STUN の聞き直しを続ける。
    /// いまは `link` の条件と同時にしか立たないが、中継役（`link` の条件の外で配る）のために別に持つ。
    casting: AtomicBool,
    /// STUN の聞き直しで直近に見えた IPv4 の外側のアドレス。書き手は受信スレッド、読み手は配る側のスレッドと見張り。
    mapped: MappedSlot,
    /// この接続の役と観戦の許可（観戦コードだけで配る試合の印。`start_game` が 1 回だけ設定する）。
    link: std::sync::OnceLock<Arc<LinkSlot>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stats: [AtomicU32; STAT_COUNT],
}

/// `pack_v4` の値（0 は未設定）。`Debug` に値を出さない（アドレスを本ログへ持ち出さない）。
#[derive(Default)]
struct MappedSlot(AtomicU64);

impl std::fmt::Debug for MappedSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MappedSlot(..)")
    }
}

/// IPv4 のアドレスとポートを 1 語に詰める（受信スレッドが錠を取らずに書くため）。IPv6 は詰めない（NAT の付け替えは IPv4 の話）。
fn pack_v4(a: SocketAddr) -> Option<u64> {
    match a {
        SocketAddr::V4(v) => {
            Some(1 << 48 | u64::from(u32::from(*v.ip())) << 16 | u64::from(v.port()))
        }
        SocketAddr::V6(_) => None,
    }
}

fn unpack_v4(w: u64) -> Option<SocketAddr> {
    if w >> 48 != 1 {
        return None;
    }
    let ip = std::net::Ipv4Addr::from((w >> 16) as u32);
    Some(SocketAddr::from((ip, (w & 0xFFFF) as u16)))
}

impl OfferShared {
    /// 配る側のスレッドが動いていて、止められていない。
    pub(crate) fn casting(&self) -> bool {
        self.casting.load(Ordering::Acquire) && !self.stopped()
    }

    /// 接続の後も外側のアドレスを保つ間: 配る側のスレッドが動いているか、2 人とも観戦を許可した試合のホスト（観戦コードを配れる）。
    /// 受信スレッドが周回ごとに読む（原子変数だけ）。
    pub(crate) fn keeps_mapping(&self) -> bool {
        self.casting()
            || self
                .link
                .get()
                .is_some_and(|s| s.is_host() == Some(true) && s.may_list() == Some(true))
    }

    /// 受信スレッド: 聞き直しの答えの外側のアドレス（答えのたびに書く。IPv4 だけ）。
    pub(crate) fn note_mapped(&self, a: SocketAddr) {
        if let Some(w) = pack_v4(a) {
            self.mapped.0.store(w, Ordering::Release);
        }
    }

    /// 直近の聞き直しの答えの外側のアドレス（まだ無ければ None）。
    pub(crate) fn mapped(&self) -> Option<SocketAddr> {
        unpack_v4(self.mapped.0.load(Ordering::Acquire))
    }

    /// 検査だけ: 配る側のスレッドを作らずに「動いている」印を立てる。
    #[cfg(test)]
    pub(crate) fn mark_casting_for_test(&self) {
        self.casting.store(true, Ordering::Release);
    }

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
            remapped: g(Stat::Remapped),
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

    /// 止めて、配る側のスレッドが終わるのを待つ（接続の片づけ。最大で 1 回の見回り —— 50 ms とルームへの送りの待ち）。作っていなければ何もしない。
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

/// 配る側のスレッドの材料（見張りが持ち、接続したとき `spawn` へ渡す）。`Debug` は書かない（秘密を含む）。
pub struct OfferKit {
    session_id: u32,
    secret: Secret,
    /// 観戦用の使い捨ての鍵の公開鍵（入るたびに作り直す観戦用のコードへ載せる。配る側のセッションの秘密鍵と対）。
    host_public: [u8; higanr_core::seal::PUBLIC_LEN],
    /// いま預ける候補（外側のアドレスが変わったら `sources` から作り直す）。
    candidates: Candidates,
    sources: Sources,
    name: Name,
    scope: Scope,
    /// 受信ソケットの族（観戦者の候補のうち、受信スレッドのソケットから送れる族だけに穴をあける）。
    families: Families,
    /// ルームサーバー。無ければ観戦用のルームへは入らない（観戦コードの経路だけ）。
    room: Option<RoomOptions>,
    shared: Arc<Shared>,
}

/// 観戦コード（ルームを使わない観戦）の材料。ホストが「観戦コードをコピー」のたびに、いまから新しい期限の観戦用のコード（種別 3）を書く。
/// session・秘密・鍵は配る側と同じ（受信スレッドの観戦のホストは 1 つ）。`Debug` は書かない（秘密を含む）。
#[derive(Clone)]
pub struct OnewayCode {
    session_id: u32,
    secret: Secret,
    host_public: [u8; higanr_core::seal::PUBLIC_LEN],
    /// LAN のアドレス・STUN で見えたアドレス・UPnP で開けたポート。観戦者は穴あけの返しを出さないので、ホストのポートへ外から届く候補だけが効く。
    candidates: Candidates,
    sources: Sources,
    /// 聞き直しで見えたいまの外側のアドレスを読む（配る側と同じ観戦のホストの共有）。
    shared: Arc<Shared>,
    name: Name,
}

impl OnewayCode {
    /// 観戦コードの文字（期限は `unix` から `code::DEFAULT_TTL_SECS`）。
    pub fn write(&self, unix: u64) -> Result<String, CodeError> {
        code::write_spectate(
            &Code {
                side: code::Side::Spectate,
                session_id: self.session_id,
                secret: Some(self.secret),
                name: self.name,
                nat: higanr_core::nat::NatMark::UNKNOWN,
                candidates: self.current_candidates(),
                expires: unix.saturating_add(code::DEFAULT_TTL_SECS),
            },
            &self.host_public,
        )
    }

    pub fn candidates_len(&self) -> usize {
        self.current_candidates().len()
    }

    /// この接続の `LinkSlot` を渡す（ホストで 2 人とも許可している間、受信スレッドが接続の後も聞き直しを続ける）。2 回目は無視。
    pub(crate) fn follow_link(&self, slot: Arc<LinkSlot>) {
        let _ = self.shared.offer.link.set(slot);
    }

    /// 聞き直しで外側のアドレスが変わっていれば、いまのアドレスで作り直した候補（作れなければ最初の候補）。
    fn current_candidates(&self) -> Candidates {
        match self.shared.offer.mapped() {
            Some(m) if Some(m) != self.sources.mapped => {
                self.sources.build(Some(m), true).unwrap_or(self.candidates)
            }
            _ => self.candidates,
        }
    }
}

/// 観戦用のコードの候補の材料（外側のアドレスが変わったとき、同じ並びで作り直すため）。アドレスを持つので `Debug` は書かない。
#[derive(Clone, Copy)]
struct Sources {
    mode: OwnCandidates,
    local: SocketAddr,
    /// 候補に載せた STUN の外側のアドレス。
    mapped: Option<SocketAddr>,
    v6: Option<SocketAddr>,
    upnp_port: Option<u16>,
}

impl Sources {
    /// `upnp` は UPnP で開けたポートの候補も足すか（観戦コードだけ）。
    fn build(&self, mapped: Option<SocketAddr>, upnp: bool) -> Result<Candidates, String> {
        NetSettings::candidates_with(
            self.mode,
            self.local,
            mapped,
            self.v6,
            self.upnp_port.filter(|_| upnp),
        )
    }
}

pub struct Armed {
    pub parts: Parts,
    pub kit: OfferKit,
    pub oneway: OnewayCode,
}

/// 観戦のホストが送れる族（受信スレッドが持つソケット）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Families {
    pub v4: bool,
    pub v6: bool,
}

impl Families {
    /// 主の受信ソケット `local` と、足した IPv6 のソケット `v6` から。
    fn of(local: SocketAddr, v6: Option<SocketAddr>) -> Self {
        Families {
            v4: local.is_ipv4(),
            v6: local.is_ipv6() || v6.is_some(),
        }
    }
}

/// 観戦を使う設定（`[spectate] enable = 1`）のとき、ゲーム内のメニューの経路の観戦のホストを用意する（読み取りのスレッド）。
/// `local` は受信ソケット、`mapped` は STUN で見えたアドレス、`v6` は開いた IPv6 の受信ソケット、`upnp_port` は UPnP で開けた外側のポート。
/// 観戦用のルームへ預けるコードの候補は `local`・`mapped`・世界に通じる `v6`、観戦コードの候補はそれに UPnP のポートを足したもの。観戦者は IPv4 と IPv6 の両方の候補へ JOIN を送れる（IPv6 の便も受信スレッドの観戦のホストが受ける。`net_udp::recv_loop_v6`）。
/// 秘密と session は接続ごとに引く。`room` が無くても用意する（観戦コードの経路だけになる）。
// 自分のアドレスの材料と、検査が差し替える乱数の口を引数で受ける入口。
#[allow(clippy::too_many_arguments)]
pub fn arm_with(
    s: &NetSettings,
    local: SocketAddr,
    mapped: Option<SocketAddr>,
    v6: Option<SocketAddr>,
    upnp_port: Option<u16>,
    room: Option<RoomOptions>,
    mut draw_session: impl FnMut() -> Result<u32, String>,
    draw_secret: impl FnOnce() -> Result<Secret, String>,
) -> Result<Armed, String> {
    let session_id = spectate_host::pick_session(&mut draw_session, s.session_id)?;
    let secret = draw_secret()?;
    // 観戦用の使い捨ての鍵: 接続ごとに OS の乱数で引く。公開鍵は観戦用のコード（ルームサーバーへ預ける）に載せる。
    let host = HostConfig {
        session_id,
        secret,
        key_seed: crate::net_udp::draw_key_seed()?,
    };
    let host_public = host.public();
    let sources = Sources {
        mode: s.candidates,
        local,
        mapped,
        v6,
        upnp_port,
    };
    let candidates = sources.build(mapped, false)?;
    let oneway_candidates = sources.build(mapped, true)?;
    let ring = Arc::new(SpectateRing::new());
    let shared = Arc::new(Shared::default());
    shared.offer.armed.store(true, Ordering::Release);
    let mut sender = SpectateSender::new(host, Arc::clone(&ring), s.scope, Arc::clone(&shared));
    sender.set_fake_uplink(s.spectate.as_ref().and_then(|x| x.fake_uplink));
    let name = Name::new(&s.name);
    Ok(Armed {
        parts: Parts { ring, sender },
        kit: OfferKit {
            session_id,
            secret,
            host_public,
            candidates,
            sources,
            name,
            scope: s.scope,
            families: Families::of(local, v6),
            room,
            shared: Arc::clone(&shared),
        },
        oneway: OnewayCode {
            session_id,
            secret,
            host_public,
            candidates: oneway_candidates,
            sources,
            shared,
            name,
        },
    })
}

/// OS の乱数で用意する（本物の経路）。戻りの文は本ログの「ネットワーク: 開いた」の行に足す（観戦を使わない設定なら空）。
/// `link` はこの接続の `LinkSlot`（観戦コードだけで配る試合でも、ホストで 2 人とも許可している間は受信スレッドが聞き直しを続ける）。
// 自分のアドレスの材料と接続の slot を引数で受ける入口。
#[allow(clippy::too_many_arguments)]
pub fn arm(
    s: &NetSettings,
    local: SocketAddr,
    mapped: Option<SocketAddr>,
    v6: Option<SocketAddr>,
    upnp_port: Option<u16>,
    room: Option<RoomOptions>,
    link: &Arc<LinkSlot>,
) -> (Option<Armed>, String) {
    if s.spectate.is_none() {
        return (None, String::new());
    }
    match arm_with(
        s,
        local,
        mapped,
        v6,
        upnp_port,
        room,
        || {
            let mut b = [0u8; 4];
            if !crate::net_win::os_random(&mut b) {
                return Err("観戦用の session を OS の乱数で決められません".to_string());
            }
            Ok(u32::from_le_bytes(b))
        },
        crate::net_udp::draw_secret,
    ) {
        Ok(a) => {
            a.oneway.follow_link(Arc::clone(link));
            let note = format!(
                " / 観戦の配信側を用意しました（観戦用の session 0x{:08X}・候補 {} 件・観戦コードの候補 {} 件。{}）",
                a.kit.session_id,
                a.kit.candidates.len(),
                a.oneway.candidates_len(),
                if a.kit.room.is_some() {
                    "ルームで会った相手と接続したマッチのホストだけが、観戦用のルームで観戦者を待ちます。観戦コードはどの接続方法のマッチでもホストが配信します"
                } else {
                    "ルームサーバーがないので、観戦用のルームへは入りません。観戦コードはホストが配信します"
                }
            );
            (Some(a), note)
        }
        Err(e) => (None, format!("{}{e}", crate::spectate_host::NOT_USED)),
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

/// 配る側のスレッドを作る（見張りのスレッド。接続して、マッチのホストで、ルームで会った相手か、ランダム対戦の相手のとき）。`room` は元のルーム名
/// （ランダム対戦は割り当てのルーム名）。`listing` があれば、観戦者の数を載せつつ一覧に載せる。
/// 作れなければ理由を返す（観戦だけを使わない。対戦は続く）。
pub fn spawn(kit: OfferKit, room: &RoomName, listing: Option<ListingInfo>) -> Result<(), String> {
    if kit.room.is_none() {
        return Err(
            "ルームサーバー（room_server）がありません（観戦用のルームへ入れません）".to_string(),
        );
    }
    let shared = Arc::clone(&kit.shared);
    let mut runner = Runner::new(kit, room.spectate_of(), listing);
    let sh = Arc::clone(&shared);
    // スレッドより先に立てる（スレッドが終わるときに倒すので、倒した後に立てない）。
    shared.offer.casting.store(true, Ordering::Release);
    let h = std::thread::Builder::new()
        .name("higanr-offer".into())
        .spawn(move || {
            // パニックは受けて外へ出さない（スレッドの入口）。受けたら配る側だけを止める。
            let r = catch_unwind(AssertUnwindSafe(|| runner.run()));
            if r.is_err() {
                sh.offer.bump(Stat::Panicked);
            }
            sh.offer.casting.store(false, Ordering::Release);
        })
        .map_err(|e| {
            shared.offer.casting.store(false, Ordering::Release);
            format!("観戦の受付のスレッドの作成不能: {e}")
        })?;
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
    /// 最後に見た聞き直しの答え（同じ答えで作り直さない。作り直せなかった答えも繰り返さない）。
    seen_mapped: Option<SocketAddr>,
}

/// 一覧に載せる接続（ランダム対戦のホストだけ）。切れたら、決まった間隔で入り直す。
struct Listing {
    info: ListingInfo,
    conn: Option<HostListConn>,
    /// 次に接続し直してよい時刻（単調な時計の ms。接続している間は使わない）。
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
            seen_mapped: kit.sources.mapped,
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
        self.drain(now_ms, unix);
        // 届いていた `Matched` を読んだ後に見る（観戦者が受け取ったコードの手順を、入り直しで壊さない）。
        self.follow_mapping(now_ms);
        // ルームサーバーへの offer の送りが失敗すると、その失敗の出来事は次の `poll` で返る。捨てる前に 1 回読まないと `Caster` が
        // 入ったままになり、二度と入り直さない。
        if self.conn.as_ref().is_some_and(RoomConn::is_finished) {
            self.drain(now_ms, unix);
            self.conn = None;
        }
    }

    fn drain(&mut self, now_ms: u64, unix: u64) {
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
    }

    /// 一覧に載せる接続を見回る: 切れていれば間隔を空けて接続し直し、観戦者の数が変わったら送り、届いた出来事で載った・載せられなかったを数える。
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
            let started = self.kit.room.as_ref().and_then(|r| {
                HostListConn::start_post(r, l.info.build, l.info.posting, watchers).ok()
            });
            match started {
                Some(c) => {
                    l.watchers = watchers;
                    l.conn = Some(c);
                }
                None => {
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
        let started = self
            .kit
            .room
            .as_ref()
            .and_then(|r| RoomConn::start(r, &self.room).ok());
        match started {
            Some(c) => self.conn = Some(c),
            None => {
                // スレッドを作れない。`Caster` に「接続できなかった」の入口が無いので、応答しない扱い（30 秒から倍々で待つ）にする。
                self.stats().bump(Stat::ConnectErrors);
                self.caster
                    .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
            }
        }
    }

    fn drop_conn(&mut self) {
        self.conn = None;
    }

    /// STUN の聞き直しで外側のアドレスが変わったら、観戦用のコードの候補を作り直す。預けたコードを誰も受け取っていなければ、
    /// ルームを閉じて入り直し、新しいコードを預ける（サーバーは 1 回の入室で offer を 1 回しか受けない）。
    fn follow_mapping(&mut self, now_ms: u64) {
        let now = self.kit.shared.offer.mapped();
        if now.is_none() || now == self.seen_mapped {
            return;
        }
        self.seen_mapped = now;
        // 作れなければ前の候補のまま（`Candidate::guess` が断るアドレスは来ない筋）。
        let Ok(c) = self.kit.sources.build(now, false) else {
            return;
        };
        self.kit.candidates = c;
        self.stats().bump(Stat::Remapped);
        if let Some(CasterAction::Close) = self.caster.on_remapped(now_ms) {
            self.drop_conn();
        }
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
            Event::Matched { .. } => self.caster.on_matched(),
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
                    // 4403 は観戦者にしか来ない筋。来たら応答しない扱い（`Caster` も同じ）。
                    Failure::Unresponsive(_) | Failure::Refused => Stat::FailUnresponsive,
                    Failure::BadRoomName => Stat::FailBadName,
                });
                self.caster.on_failed(now_ms, f);
                self.drop_conn();
            }
        }
    }

    /// 観戦用のコード（同じ session・秘密、いまの候補、いまから新しい期限）を作って offer する。
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
            // 送れなかった: 失敗の出来事は `step` の末尾の `drain` で届く（そこで入り直しの待ちに入る）。
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

    /// 観戦者の返し（種別 4）を観戦用の秘密・session で検証し、通れば候補をネットワークの範囲で絞って穴あけを頼む。
    fn punch(&mut self, text: &str, unix: u64) {
        let st = self.stats();
        st.bump(Stat::Replies);
        match code::read_spectate_reply(text, &self.kit.secret, self.kit.session_id, unix) {
            Ok(reply) => {
                let c = punch_candidates(&reply.candidates, self.kit.scope, self.kit.families);
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

/// 観戦者の候補から、穴あけを送ってよいものだけを残す（ネットワークの範囲 `scope` の中で、持っているソケットの族のもの）。送る直前のガードでも
/// 同じ範囲を見る。ここで除くのは、頼みの数と本ログを実際に送る分と合わせるため。
pub fn punch_candidates(reply: &Candidates, scope: Scope, families: Families) -> Candidates {
    let mut out = Candidates::EMPTY;
    for c in reply.as_slice() {
        let family_ok = if c.addr.is_ipv4() {
            families.v4
        } else {
            families.v6
        };
        if guard::allows(scope, c.addr) && family_ok {
            // 同じ候補は元の列で重ならない。入らなくても（満杯）残りを送れれば足りる。
            let _ = out.push(*c);
        }
    }
    out
}

#[cfg(test)]
mod tests;
