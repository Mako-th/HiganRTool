//! 配布側（インターネット越しの観戦）: マッチのホストのdllが観戦用のルーム（`RoomName::spectate_of`）に役hostで入って待つ。
//! 観戦者の返答（種別4）を受け取り、受信スレッドに穴あけ（`spectate::PunchPlan`）を頼む。
//!
//! - 監視のスレッド（`net_code_game::GameWatcher`）が`spawn`する。条件は次が全部揃ったとき: 接続済み・マッチのホスト・ルームで会った相手・観戦の用意あり。
//!   配布側のスレッド（`higanr-offer`。ここ）はcoreの`Caster`を50 msごとに回し、次を受け持つ。
//!   観戦用のルームへの接続（`RoomConn::start_as_host`。対戦の成立で受け取った席のチケットでhostの席を取る）。
//!   観戦用のコード（同じsessionと秘密の乱数・新しい期限。候補はSTUNの聞き直しで外側のアドレスが変わったら作り直す）のoffer。
//!   返答（`read_spectate_reply`）の検証と候補の絞り込み。`Shared::request_punch`。
//!   受信スレッド（`net_udp::recv_loop` → `SpectateSender::tick`）は穴あけの依頼を見て`PunchPlan`を持ち替え、候補へ4 Bの印を送る。
//! - ゲームのスレッドは何も呼ばない。確保・ブロッキングI/Oは全部このスレッドと`RoomConn`の受信スレッドの側。
//! - ルーム名・席のチケット・秘密の乱数・コードは本ログに書かず、`Debug`にも出さない。
//!   数は`Shared`のアトミック変数に積み、本ログの観戦の集計の行（`spectate_host::Snapshot::text`）に出す。
//!   止めるのは`spectate_host::Shared::stop_offer`（接続の後始末）。止まったらルームの接続を手放す。閉じ始めるだけで待たない。
//!
//! 観戦コード（ルームを使わない観戦。`OnewayCode`）: 同じ観戦のホストのsession・秘密の乱数・鍵で、ホストが頼まれるたびに新しい期限のコードを書く（書くのは`net_code_game`の監視の処理）。
//! ルームサーバーが無い設定でも用意する。観戦者は返答を出さず、ホストは穴をあけない。
//! 観戦用のコードのsessionと対戦のsessionは別に生成する（`spectate_host::pick_session`）。
//! ゲーム内のメニューの経路では対戦のsessionをルームで相手と会ってから生成するので、両者が等しくならない保証は無い。
//! 偶然に同じになる確率は2^-32。同じになると対戦の相手のパケットが観戦のホストへ回る。
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
use higanr_core::room::{Event, Failure, Role, RoomName, SeatTicket, Why};
use higanr_core::spectate::HostConfig;
use higanr_core::spectate_room::{Caster, CasterAction};
use higanr_core::watch_list::{HostEvent, Posting};

use crate::net_config::{NetSettings, OwnCandidates};
use crate::net_room::{HostListConn, RoomConn, RoomOptions, SubmitRefused};
use crate::netplay::LinkSlot;
use crate::spectate_host::{self, Shared, SpectateSender};
use crate::spectate_ring::SpectateRing;

/// 配布側のスレッドが`Caster`を回す間隔（ms）。監視の処理（`net_code_game::GAME_POLL`）と同じ50 ms。
/// 観戦者のJOINは250 msごとなので、これより細かくしても早くならない。
const POLL: Duration = Duration::from_millis(50);

// ---------------- 数 ----------------

/// 数えるイベント（`OfferStats`の添字）。
#[derive(Debug, Clone, Copy)]
enum Stat {
    Entered,
    /// 役guestを受けた（観戦者が先に入っていた）ので閉じた。
    WrongRole,
    Offered,
    OfferFailed,
    /// STUNの聞き直しで外側のアドレスが変わり、観戦用のコードの候補を作り直した。
    Remapped,
    Replies,
    /// 返答のタグが合わない（観戦用の秘密の乱数を知らない者が作った・書き換えた）。
    BadTag,
    WrongSession,
    /// 返答の期限が切れている。
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
    /// より新しいチケットの配布側に観戦用のルームの席を渡した。
    Evicted,
    /// サーバーがチケットを受けなかった（400 `bad_ticket`。以後は入らない）。
    FailBadTicket,
    ConnectErrors,
    /// ルームの通信の記録（WinHTTPの失敗・サーバーのバイナリ）。
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
    pub evicted: u32,
    pub fail_bad_ticket: u32,
    pub connect_errors: u32,
    pub transport: u32,
    pub panicked: u32,
    pub listed: u32,
    pub list_failed: u32,
}

impl OfferCounts {
    pub fn text(&self) -> String {
        format!(
            " / 配信側（ルームで観戦者を待つ。{}）: 入った {}（役が逆で閉じた {}）・観戦コードを預けた {}（預け不能 {}・外側のアドレスの変化で作り直し {}）・観戦者の返答コード {}（読み取り不能: タグ {}・session {}・期限切れ {}・その他 {}）・穴あけを頼んだ {}（候補が範囲外で拒否 {}）・手順を終えた {} / ルームの失敗（期限 {}・相手が抜けた {}・満員 {}・混雑 {}・応答なし {}・ルーム名 {}・入室許可の拒否 {}）・後から成立した対戦に席を譲った {}・接続できなかった {}・運びの記録 {}{} / 一覧（載った {}・載せ不能 {}）",
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
            self.fail_bad_ticket,
            self.evicted,
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
    /// 配布側のスレッドが動いている（作る前に立て、終わるときに倒す）。
    /// 受信スレッドはこれが立っている間、接続の後もSTUNの聞き直しを続ける。
    /// いまは`link`の条件と同時にしか立たない。中継役（`link`の条件の外で配布する）のために別に持つ。
    casting: AtomicBool,
    /// STUNの聞き直しで直近に見えたIPv4の外側のアドレス。書き手は受信スレッド。読み手は配布側のスレッドと監視の処理。
    mapped: MappedSlot,
    /// この接続の役と観戦の許可（観戦コードだけで配布する試合のフラグ。`start_game`が1回だけ設定する）。
    link: std::sync::OnceLock<Arc<LinkSlot>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stats: [AtomicU32; STAT_COUNT],
}

/// `pack_v4`の値（0は未設定）。`Debug`に値を出さない（アドレスを本ログへ持ち出さない）。
#[derive(Default)]
struct MappedSlot(AtomicU64);

impl std::fmt::Debug for MappedSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MappedSlot(..)")
    }
}

/// IPv4のアドレスとポートを1ワードに詰める（受信スレッドがロックを取らずに書くため）。IPv6は詰めない（NATの付け替えはIPv4の話）。
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
    /// 配布側のスレッドが動いていて止められていない。
    pub(crate) fn casting(&self) -> bool {
        self.casting.load(Ordering::Acquire) && !self.stopped()
    }

    /// 接続の後も外側のアドレスを保つ間: 配布側のスレッドが動いているか、2人とも観戦を許可した試合のホスト（観戦コードを配布できる）。
    /// 受信スレッドが周回ごとに読む（アトミック変数だけ）。
    pub(crate) fn keeps_mapping(&self) -> bool {
        self.casting()
            || self
                .link
                .get()
                .is_some_and(|s| s.is_host() == Some(true) && s.may_list() == Some(true))
    }

    /// 受信スレッド: 聞き直しの答えの外側のアドレス（答えのたびに書く。IPv4だけ）。
    pub(crate) fn note_mapped(&self, a: SocketAddr) {
        if let Some(w) = pack_v4(a) {
            self.mapped.0.store(w, Ordering::Release);
        }
    }

    /// 直近の聞き直しの答えの外側のアドレス（まだ無ければNone）。
    pub(crate) fn mapped(&self) -> Option<SocketAddr> {
        unpack_v4(self.mapped.0.load(Ordering::Acquire))
    }

    /// 検査だけ: 配布側のスレッドを作らずに「動いている」フラグを立てる。
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
            evicted: g(Stat::Evicted),
            fail_bad_ticket: g(Stat::FailBadTicket),
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

    /// 止めて配布側のスレッドが終わるのを待つ（接続の後始末。最大でポーリング1回分 —— 50 msとルームへの送信の待ち）。作っていなければ何もしない。
    pub fn stop_and_join(&self) {
        self.stop();
        let h = self.lock_thread().take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

// ---------------- 用意 ----------------

/// 受信スレッドと監視の処理が持つ、観戦のホストの本体（リングバッファと送信側）。
pub struct Parts {
    pub ring: Arc<SpectateRing>,
    pub sender: SpectateSender,
}

/// 配布側のスレッドの材料（監視の処理が持ち、接続したとき`spawn`へ渡す）。`Debug`は書かない（秘密の乱数を含む）。
pub struct OfferKit {
    session_id: u32,
    secret: Secret,
    /// 観戦用の使い捨ての鍵の公開鍵（入るたびに作り直す観戦用のコードへ載せる。配布側のセッションの秘密鍵とペア）。
    host_public: [u8; higanr_core::seal::PUBLIC_LEN],
    /// いま登録する候補（外側のアドレスが変わったら`sources`から作り直す）。
    candidates: Candidates,
    sources: Sources,
    name: Name,
    scope: Scope,
    /// 受信ソケットのアドレスファミリ（観戦者の候補のうち受信スレッドのソケットから送れるファミリだけに穴をあける）。
    families: Families,
    /// ルームサーバー。無ければ観戦用のルームへは入らない（観戦コードの経路だけ）。
    room: Option<RoomOptions>,
    shared: Arc<Shared>,
}

/// 観戦コード（ルームを使わない観戦）の材料。ホストが「観戦コードをコピー」するたびに、いまから新しい期限の観戦用のコード（種別3）を書く。
/// session・秘密の乱数・鍵は配布側と同じ（受信スレッドの観戦のホストは1つ）。`Debug`は書かない（秘密の乱数を含む）。
#[derive(Clone)]
pub struct OnewayCode {
    session_id: u32,
    secret: Secret,
    host_public: [u8; higanr_core::seal::PUBLIC_LEN],
    /// LANのアドレス・STUNで見えたアドレス・UPnPで開けたポート。観戦者は穴あけの返答を出さないので、ホストのポートへ外から届く候補だけが効く。
    candidates: Candidates,
    sources: Sources,
    /// 聞き直しで見えたいまの外側のアドレスを読む（配布側と同じ観戦のホストの共有）。
    shared: Arc<Shared>,
    name: Name,
}

impl OnewayCode {
    /// 観戦コードの文字（期限は`unix`から`code::DEFAULT_TTL_SECS`）。
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

    /// この接続の`LinkSlot`を渡す（ホストで2人とも許可している間、受信スレッドが接続の後も聞き直しを続ける）。2回目は無視。
    pub(crate) fn follow_link(&self, slot: Arc<LinkSlot>) {
        let _ = self.shared.offer.link.set(slot);
    }

    /// 聞き直しで外側のアドレスが変わっていればいまのアドレスで作り直した候補（作れなければ最初の候補）。
    fn current_candidates(&self) -> Candidates {
        match self.shared.offer.mapped() {
            Some(m) if Some(m) != self.sources.mapped => {
                self.sources.build(Some(m), true).unwrap_or(self.candidates)
            }
            _ => self.candidates,
        }
    }
}

/// 観戦用のコードの候補の材料（外側のアドレスが変わったとき、同じ並びで作り直すため）。アドレスを持つので`Debug`は書かない。
#[derive(Clone, Copy)]
struct Sources {
    mode: OwnCandidates,
    local: SocketAddr,
    /// 候補に載せたSTUNの外側のアドレス。
    mapped: Option<SocketAddr>,
    v6: Option<SocketAddr>,
    upnp_port: Option<u16>,
}

impl Sources {
    /// `upnp`はUPnPで開けたポートの候補も足すかどうか（観戦コードだけ）。
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

/// 観戦のホストが送れるアドレスファミリ（受信スレッドが持つソケット）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Families {
    pub v4: bool,
    pub v6: bool,
}

impl Families {
    /// 主の受信ソケット`local`と足したIPv6のソケット`v6`から。
    fn of(local: SocketAddr, v6: Option<SocketAddr>) -> Self {
        Families {
            v4: local.is_ipv4(),
            v6: local.is_ipv6() || v6.is_some(),
        }
    }
}

/// 観戦を使う設定（`[spectate] enable = 1`）のとき、ゲーム内のメニューの経路の観戦のホストを用意する（読み取りのスレッド）。
/// `local`は受信ソケット。`mapped`はSTUNで見えたアドレス。`v6`は開いたIPv6の受信ソケット。`upnp_port`はUPnPで開けた外側のポート。
/// 観戦用のルームへ登録するコードの候補は`local`・`mapped`・インターネットに通じる`v6`。観戦コードの候補はそれにUPnPのポートを足したもの。
/// 観戦者はIPv4とIPv6の両方の候補へJOINを送れる（IPv6のパケットも受信スレッドの観戦のホストが受ける。`net_udp::recv_loop_v6`）。
/// 秘密の乱数とsessionは接続ごとに生成する。`room`が無くても用意する（観戦コードの経路だけになる）。
// 自分のアドレスの材料と検査が差し替える乱数の取得元を引数で受ける入口。
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
    // 観戦用の使い捨ての鍵: 接続ごとにOSの乱数で生成する。公開鍵は観戦用のコード（ルームサーバーへ登録する）に載せる。
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

/// OSの乱数で用意する（本物の経路）。戻り値の文は本ログの「ネットワーク: 開いた」の行に足す（観戦を使わない設定なら空）。
/// `link`はこの接続の`LinkSlot`（観戦コードだけで配布する試合でも、ホストで2人とも許可している間は受信スレッドが聞き直しを続ける）。
// 自分のアドレスの材料と接続のslotを引数で受ける入口。
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

/// 進行中のランダム対戦の一覧に載せる材料（ランダム対戦のホストだけ）。割り当てのルーム名は`posting.room`（サーバーが観戦用のルーム名を導く元）。
#[derive(Debug, Clone, Copy)]
pub struct ListingInfo {
    pub build: u32,
    pub posting: Posting,
}

/// 配布側のスレッドを作る（監視のスレッドが呼ぶ。条件は接続済み・マッチのホスト・相手がルームで会った相手かランダム対戦の相手）。
/// `room`は元のルーム名（ランダム対戦は割り当てのルーム名）。観戦用のルームの名を導く元で、サーバーへは送らない。
/// `ticket`はその対戦の成立で受け取った席のチケット。
/// `listing`があれば、観戦者の数を載せつつ一覧に載せる。作れなければ理由を返す（観戦だけを使わない。対戦は続く）。
pub fn spawn(
    kit: OfferKit,
    room: &RoomName,
    ticket: SeatTicket,
    listing: Option<ListingInfo>,
) -> Result<(), String> {
    if kit.room.is_none() {
        return Err(
            "ルームサーバー（room_server）がありません（観戦用のルームへ入れません）".to_string(),
        );
    }
    let shared = Arc::clone(&kit.shared);
    let mut runner = Runner::new(kit, *room, ticket, listing);
    let sh = Arc::clone(&shared);
    // スレッドより先に立てる（スレッドが終わるときに倒すので、倒した後に立てない）。
    shared.offer.casting.store(true, Ordering::Release);
    let h = std::thread::Builder::new()
        .name("higanr-offer".into())
        .spawn(move || {
            // パニックは捕捉して外へ出さない（スレッドの入口）。捕捉したら配布側だけを止める。
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
    /// 観戦用のルームの名（元のルーム名のハッシュ。`Debug`に出さない）。
    room: RoomName,
    /// 観戦用のルームのhostの席を取るチケット（`Debug`は中身を書かない。ログに出さない）。
    ticket: SeatTicket,
    caster: Caster,
    conn: Option<RoomConn>,
    listing: Option<Listing>,
    t0: Instant,
    /// 最後に見た聞き直しの答え（同じ答えで作り直さない。作り直せなかった答えも繰り返さない）。
    seen_mapped: Option<SocketAddr>,
    /// 席を譲った・チケットを拒否されたことを本ログに書いた（1回だけ）。
    told_evicted: bool,
    told_bad_ticket: bool,
}

/// 一覧に載せる接続（ランダム対戦のホストだけ）。切れたら決まった間隔で入り直す。
struct Listing {
    info: ListingInfo,
    conn: Option<HostListConn>,
    /// 次に接続し直してよい時刻（単調な時計のms。接続している間は使わない）。
    retry_at_ms: u64,
    /// 続けて載せられなかった回数（入り直しの間隔を倍々にする）。
    fails: u32,
    /// 載った後に送った観戦者の数（変わったときだけ送る）。
    watchers: u8,
    /// 件数の上限で断られた（入り直さない）。
    gave_up: bool,
}

/// 本ログの1行（配布側ごとに1回だけ）。
const EVICTED_LINE: &str = "観戦の受付: 同じルーム名で後から成立した対戦に観戦用のルームを譲りました（入り直しは 5 分ごと）。";
const BAD_TICKET_LINE: &str =
    "観戦の受付: 停止（ルームサーバーが観戦用のルームの入室許可を拒否）。対戦は続きます。";

/// 一覧へ入り直す最初の間隔と上限。サーバーの回数制限（1 IP 30回 / 分）に触れない長さ。
const RELIST_FIRST_MS: u64 = 30_000;
const RELIST_MAX_MS: u64 = 5 * 60_000;

impl Runner {
    /// `origin`は元のルーム名（ランダム対戦は割り当てのルーム名）。
    pub(crate) fn new(
        kit: OfferKit,
        origin: RoomName,
        ticket: SeatTicket,
        listing: Option<ListingInfo>,
    ) -> Runner {
        Runner {
            seen_mapped: kit.sources.mapped,
            kit,
            room: origin.spectate_of(),
            ticket,
            told_evicted: false,
            told_bad_ticket: false,
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

    /// スレッドの本体: 止められるまで50 msごとにポーリングする。止められたらルームの接続を手放す。
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
        match self.caster.poll(now_ms) {
            Some(CasterAction::Connect) => self.connect(now_ms),
            // 観戦者が来た後に外側のアドレスが変わり、その観戦者の答えの期限を過ぎても席にいた。
            Some(CasterAction::Close) => self.drop_conn(),
            _ => {}
        }
        self.drain(now_ms, unix);
        // 届いていた`Matched`を読んだ後に見る（観戦者が受け取ったコードの手順を入り直しで壊さない）。
        self.follow_mapping(now_ms);
        // ルームサーバーへのofferの送信が失敗すると、その失敗のイベントは次の`poll`で返る。
        // 捨てる前に1回読まないと`Caster`が入室済みのままになり、二度と入り直さない。
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
                // 手放した接続の残りのイベントは捨てる（`spectate_room`の使い方）。
                break;
            }
            self.on_event(e, now_ms, unix);
        }
    }

    /// 一覧に載せる接続をポーリングする: 切れていれば間隔を空けて接続し直し、観戦者の数が変わったら送信し、届いたイベントで載った回数・載せられなかった回数を数える。
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
                    // 件数の上限（全体・1 IPあたり）は入り直しても変わらない。429はサーバーに指定された秒数の後。
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
            .and_then(|r| RoomConn::start_as_host(r, &self.room, &self.ticket).ok());
        match started {
            Some(c) => self.conn = Some(c),
            None => {
                // スレッドを作れない。`Caster`に「接続できなかった」の入口が無いので、応答しない扱い（30秒から倍々で待つ）にする。
                self.stats().bump(Stat::ConnectErrors);
                self.caster
                    .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
            }
        }
    }

    fn drop_conn(&mut self) {
        self.conn = None;
    }

    /// STUNの聞き直しで外側のアドレスが変わったら観戦用のコードの候補を作り直す。
    /// 登録したコードを誰も受け取っていなければルームを閉じて入り直し、新しいコードを登録する（サーバーは1回の入室でofferを1回しか受けない）。
    fn follow_mapping(&mut self, now_ms: u64) {
        let now = self.kit.shared.offer.mapped();
        if now.is_none() || now == self.seen_mapped {
            return;
        }
        self.seen_mapped = now;
        // 作れなければ前の候補のまま（`Candidate::guess`が断るアドレスは来ない筋）。
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
            Event::Matched { .. } => self.caster.on_matched(now_ms),
            Event::PeerCode(text) => {
                if let Some(CasterAction::Punch(text)) = self.caster.on_peer_code(text) {
                    self.punch(&text, unix);
                }
            }
            // Heldは対戦のルームだけ（観戦用のルームは成立でサーバーが閉じる）。来たら終わりと同じに扱う。
            Event::Done | Event::Held => {
                self.stats().bump(Stat::Done);
                self.caster.on_done(now_ms);
                self.drop_conn();
            }
            Event::Failed(f) => {
                self.stats().bump(match f {
                    Failure::Expired => Stat::FailExpired,
                    Failure::PeerLeft => Stat::FailPeerLeft,
                    Failure::RoomFull | Failure::RoomInUse => Stat::FailFull,
                    Failure::RateLimited { .. } => Stat::FailRate,
                    // 4403は観戦者にしか来ない筋。来たら応答しない扱い（`Caster`も同じ）。
                    Failure::Unresponsive(_) | Failure::Refused => Stat::FailUnresponsive,
                    Failure::BadRoomName => Stat::FailBadName,
                    Failure::BadTicket => Stat::FailBadTicket,
                });
                let was_evicted = self.caster.evicted();
                self.caster.on_failed(now_ms, f);
                self.drop_conn();
                if self.caster.evicted() && !was_evicted {
                    self.stats().bump(Stat::Evicted);
                    if !self.told_evicted {
                        self.told_evicted = true;
                        crate::main_log::push(EVICTED_LINE);
                    }
                }
                if f == Failure::BadTicket {
                    self.give_up_listing();
                    if !self.told_bad_ticket {
                        self.told_bad_ticket = true;
                        crate::main_log::push(BAD_TICKET_LINE);
                    }
                }
            }
        }
    }

    /// 一覧に載せるのをやめる（サーバーがチケットを受けないので一覧への投稿も通らない）。
    fn give_up_listing(&mut self) {
        if let Some(l) = self.listing.as_mut() {
            l.gave_up = true;
            if let Some(mut c) = l.conn.take() {
                c.cancel();
            }
        }
    }

    /// 観戦用のコード（同じsessionと秘密の乱数・いまの候補・いまから新しい期限）を作ってofferする。
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
            // 送れなかった: 失敗のイベントは`step`の末尾の`drain`で届く（そこで入り直しの待ちに入る）。
            Err(SubmitRefused::NotSent) => self.stats().bump(Stat::OfferFailed),
            Err(SubmitRefused::Core(_)) => self.offer_failed(now_ms),
        }
    }

    /// 観戦用のコードを作れない・ルームが受けない: この接続を閉じてゆっくり入り直す（応答しない扱い。30秒から倍々で上限は5分）。
    fn offer_failed(&mut self, now_ms: u64) {
        self.stats().bump(Stat::OfferFailed);
        self.caster
            .on_failed(now_ms, Failure::Unresponsive(Why::NoAnswer));
        self.drop_conn();
    }

    /// 観戦者の返答（種別4）を観戦用の秘密の乱数・sessionで検証し、通れば候補をネットワークの範囲で絞って穴あけを頼む。
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

/// 一覧へ入り直すまでの間隔（失敗が続くほど倍々。上限5分）。
fn relist_delay(fails: u32) -> u64 {
    RELIST_FIRST_MS
        .saturating_mul(1u64 << fails.saturating_sub(1).min(4))
        .min(RELIST_MAX_MS)
}

/// 観戦者の候補から穴あけを送ってよいものだけを残す（ネットワークの範囲`scope`の中で、持っているソケットのアドレスファミリのもの）。
/// 送る直前のガードでも同じ範囲を見る。ここで除くのは、穴あけの依頼の数と本ログを実際に送る分に合わせるため。
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
