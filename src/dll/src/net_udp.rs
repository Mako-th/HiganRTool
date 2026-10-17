//! dll の通信層: UDP のソケット・受信スレッド・ガード・ゲームスレッドのインターフェース（`GameNet`）。
//!
//! - **受信スレッド**は `recv_from` で受けた生のバイト列を `net_ring::RecvRing` へ書き、受信のイベントを立てるだけ。解析と状態機械は
//!   ゲームスレッド（`netplay.rs`）。例外は PING への PONG で、受信スレッドが受けたその場で返す（`answer_ping`。ゲームのスレッドの
//!   tick を待つと PONG が最大 1 tick 遅れ、相手の測る RTT がその分大きく出るため）。ゲームスレッドの心拍が古ければ PONG を返さず、ゲームスレッドが
//!   パニックで止まった後（`RecvThread::say_bye`）は PONG の代わりに BYE を返す（相手が止まったこちらを生きていると見続けないため）。Windows の UDP は、前に送った先のポートが
//!   閉じていると次の受信が `WSAECONNRESET` になる（相手が先に終えた）ので読み飛ばす（相手の生死は liveness が決める）。
//! - **ゲームスレッドのインターフェース** `GameNet` は `netplay::Net` の本物: 時計（`Instant`。QPC）・リングの取り出し・イベントか高分解能の
//!   待機タイマーでの待ち（`net_win.rs`）・送る（std の `UdpSocket::send_to`。アドレスはスタックで詰め、確保しない）。
//! - **ガード**: 開くときにソケットと相手の候補の全部がネットワークの範囲（ini の `scope`。既定 loopback。`higanr_core::guard`）の中かを見て、
//!   違えば 1 通も送らずに断る。送る直前にも宛先が範囲の中かを見る（`Outlet::send` の `to`。セッションは候補と送り元にしか送らない）。
//! - **STUN**: 開いた後、ネットワークを動かす前に、読み取りのスレッドで STUN の Binding を問い合わせる（`GameNet::stun_query`。ini の `stun` が
//!   あるときだけ）。ソケットに届いたものは `wire::classify` で STUN と HGRT に分ける。秘密を OS の乱数で引き、自分の接続コードを作る
//!   のも開く側（`prepare_link`）。
//! - **自動のソケットと名前の STUN**: ini の `bind = auto:ポート` は開くときに経路表から自分のアドレスを選び（`resolve_bind`。UDP の
//!   `connect` は何も送らない）、`stun = 名前:ポート` は開くときに別のスレッドで 2 秒まで名前を引く（`resolve_stun`）。どちらも
//!   読み取りのスレッドで 1 回だけ。ゲームのスレッドでは呼ばない（DNS はブロッキング）。
//! - **外側のアドレスの保ち（STUN の聞き直し）**: ネットワークを開いて STUN の答えを得た後、相手との接続の手順が成り立つまでの間、受信スレッドが
//!   同じソケットから同じ STUN へ 15 秒ごとに Binding を 1 通送る（`GameNet::keep_stun_mapping`。core の `stun::Keep` が間隔と答え合わせを
//!   持つ）。コードを作ってから相手が使うまでの無通信（実測 174 秒）のあいだに、家庭用ルーターが外側のポートの対応づけを忘れ、
//!   コードに載せたアドレスが使えなくなるのを防ぐ。答えは受信スレッドが抜き取り（セッションへは渡さない）、ゲームのスレッドには何も足さない。
//!   繋がったら（`Net::link_up`）止める。外側のアドレスが変わったかの数と、繋がらなかった理由を読むための診断の数（`NetDiag`）もここで持つ。
//!
//! 開く・閉じる（スレッドを起こす・止めて待つ）はゲームスレッドの外で行う（確保とブロッキングを含む）。
#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::adapter_pick::{self, Adapter};
use higanr_core::auth::{SECRET_LEN, Secret};
use higanr_core::candidate::{Candidate, is_global_unicast_v6, kind_of};
use higanr_core::guard::{self, Scope};
use higanr_core::link::{self, KIND_SLOTS, Outlet, kind_slot};
use higanr_core::liveness;
use higanr_core::nat::{NatMark, Symmetry};
use higanr_core::pong::PongResponder;
use higanr_core::seal::{KeySeed, SEED_LEN};
use higanr_core::stun;
use higanr_core::upnp::OuterCheck;
use higanr_core::wire::{self, ByeReason, Datagram, MAX_DATAGRAM};

use crate::adapters_win;
use crate::net_config::{AddrSource, NetSettings, StunTarget};
use crate::net_ring::{RecvRing, RingCounts};
use crate::net_win::{Event, Waiter, Woke, os_random};
use crate::netplay::{HANDSHAKE_RETRY_MS, Net};
use crate::spectate_host::{self, SpectateSender};
use crate::trial_summary::Stun;
use crate::upnp;

/// 受信スレッドが止める印を見に戻る間隔（`recv_from` の待ちの上限）。
///
/// **短くしない**: 観戦のホストを置いたときだけ上限を 1 ms にしたところ、対戦の相手のパケット 200 通のうち 1 通が受信スレッドに
/// 届かない実行が出た（観戦者付きの検査 330 走行のうち 22 走行。50 ms では 120 走行で 0、観戦のホストなし・50 ms で 80 走行で 0。
/// 実測）。ICMP の受信誤り（`WSAECONNRESET`）ではない（`SIO_UDP_CONNRESET` を切っても 60 走行のうち 3 走行で欠けた）。上限が短い
/// ほど打ち切りが多く、打ち切りと到着が重なった 1 通が落ちる形に見える（推測。Winsock の中は見ていない）。ゲームのスレッドの待ちは
/// 別で、この上限に依らない。
pub(crate) const RECV_POLL: Duration = Duration::from_millis(50);
/// 受信の作業場所。`MAX_DATAGRAM` より大きいものも 1 回で受けて、リングが「大きすぎる」と数えて捨てる
/// （小さい場所で受けると Windows は `WSAEMSGSIZE` の誤りを返す）。
const RECV_BUF: usize = 2048;

struct Shared {
    ring: RecvRing,
    /// 時計の起点（受信スレッドの受けた時刻と、ゲームスレッドの `now_us` は同じ起点の µs）。
    t0: Instant,
    event: Arc<Event>,
    stop: AtomicBool,
    conn_resets: AtomicU32,
    recv_errors: AtomicU32,
    pong: PongSlot,
    /// 観戦のホスト（ini の `[spectate] enable = 1` のときだけ置く）。
    spectate: SpectateSlot,
    /// ネットワークの範囲（開くときに決まり、受信スレッドを起こす前に置くので原子変数は要らない）。受信スレッドの PONG の送る直前のガードが使う。
    scope: Scope,
    /// 外側のアドレスの保ち（STUN の聞き直し）。置かなければ受信スレッドは今までどおり。
    keep: KeepSlot,
    /// 2 か所目の STUN（宛先ごとに外側のポートを変える型かの見分け）。置かなければ受信スレッドは今までどおり。
    second: SecondSlot,
    /// 繋がらなかった理由を読むための診断の数（人を特定する値は持たない）。
    diag: DiagSlot,
    /// IPv6 のソケット（`GameNet::open_ipv6` が開くときに 1 回だけ置く。置かなければ IPv4 だけ）。送る側（ゲームのスレッド）は
    /// 原子的に 1 回読むだけ（確保しない）。
    sock6: std::sync::OnceLock<Arc<UdpSocket>>,
    /// IPv6 のソケットの受信のリング。IPv4 のリング（`ring`）とは別の単一書き手（IPv6 の受信スレッド）・単一読み手（ゲームのスレッド）の組。
    ring6: RecvRing,
    /// IPv6 の受信スレッドの持ち手（`RecvThread::halt` が止めて待つ）。
    join6: Mutex<Option<JoinHandle<()>>>,
    /// UPnP の割り当ての持ち手（`GameNet::upnp_attach` が開くときに 1 回だけ置く。`RecvThread::halt` が止めて、割り当てを消す）。
    upnp: std::sync::OnceLock<upnp::Handle>,
}

/// 聞き直しを続ける上限（ms）。接続の手順を作り直して待つ長さ（`HANDSHAKE_RETRY_MS` = コードの期限と同じ 30 分）に、開いてからセッションを
/// 作るまでの余裕 1 分を足した値。繋がらないままネットワークが終わっても、受信スレッドが生きている間は送り続けないための上限。
const KEEP_MAX_MS: u32 = HANDSHAKE_RETRY_MS + 60_000;

#[derive(Debug, Clone, Copy)]
struct KeepSetup {
    server: SocketAddr,
    /// 自分のコードに載せた外側のアドレス（最初の STUN の答え）。
    mapped: SocketAddr,
    interval_ms: u32,
}

/// 聞き直しの受け渡し。設定は 1 回だけ置かれ（`OnceLock`）、受信スレッドが周回ごとに原子変数を 1 つ見て取る。数は受信スレッドが
/// 書き、読み取りのスレッドがログに読む。
#[derive(Default)]
struct KeepSlot {
    setup: std::sync::OnceLock<KeepSetup>,
    /// 送ってよいか。置くときに true、繋がったら（`Net::link_up`）ゲームのスレッドが false にする。
    run: AtomicBool,
    /// 受信スレッドが今も聞き直しているか（止めたら false。ログ用）。
    running: AtomicBool,
    sent: AtomicU32,
    answered: AtomicU32,
    ignored: AtomicU32,
    send_errors: AtomicU32,
    changed_port: AtomicU32,
    changed_addr: AtomicU32,
    differs: AtomicBool,
}

/// 2 か所目の STUN の受け渡し。読み取りのスレッドが起こす名前引きのスレッドが 1 回だけ宛先を置き（`OnceLock`）、受信スレッドが
/// 周回ごとに 1 つ見て取る。結果は `DiagSlot::second` に受信スレッドが書く。
#[derive(Default)]
struct SecondSlot {
    setup: std::sync::OnceLock<SecondSetup>,
}

#[derive(Debug, Clone, Copy)]
struct SecondSetup {
    server: SocketAddr,
    first: SocketAddr,
}

/// 診断の数（ゲームのスレッドは送りの誤りを足すだけ。ほかは読み取りのスレッドが置く）。
#[derive(Default)]
struct DiagSlot {
    own_cands: [AtomicU32; KIND_SLOTS],
    send_errors: [AtomicU32; KIND_SLOTS],
    /// 外側のポートがソケットのポートと同じか: 0 = 不明、1 = 同じ、2 = 違う。
    port_kept: AtomicU32,
    /// STUN で外側のアドレスを得た時刻（ネットワークを開いてからの ms + 1。0 = 得ていない）。
    stun_at_ms: AtomicU32,
    /// 2 か所目の STUN の結果（`SecondStun::code`）。
    second: AtomicU32,
    /// UPnP でソケットのポートを割り当てられた（自分のコードに載せた印。ゲームのスレッドが `Net::own_nat` で読む）。
    upnp_mapped: AtomicBool,
}

/// 2 か所目の STUN の結果（診断。1 か所目の答えとの比べ。アドレス・ポートの値は持たない）。画面の左上の文を作るとき
/// （繋がらなかった理由の行）も、この値を読む。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecondStun {
    /// 聞いていない（ini に `stun2` が無い・scope が internet でない・1 か所目の答えが無かったのどれか）。
    #[default]
    NotAsked,
    Pending,
    /// 宛先を決められず、聞かなかった（名前を引けない・範囲の外・1 か所目と同じアドレス）。
    Skipped,
    /// 8 回聞いても答えが来なかった。
    NoAnswer,
    /// 答えは来たが、族が 1 か所目と違い比べられなかった。
    Incomparable,
    /// 1 か所目の答えと比べられた。
    Compared(stun::Mapping),
}

impl SecondStun {
    fn code(self) -> u32 {
        match self {
            SecondStun::NotAsked => 0,
            SecondStun::Pending => 1,
            SecondStun::Skipped => 2,
            SecondStun::NoAnswer => 3,
            SecondStun::Incomparable => 4,
            SecondStun::Compared(stun::Mapping::Same) => 5,
            SecondStun::Compared(stun::Mapping::PortOnly) => 6,
            SecondStun::Compared(stun::Mapping::AddrOnly) => 7,
            SecondStun::Compared(stun::Mapping::Both) => 8,
        }
    }

    fn from_code(c: u32) -> SecondStun {
        match c {
            1 => SecondStun::Pending,
            2 => SecondStun::Skipped,
            3 => SecondStun::NoAnswer,
            4 => SecondStun::Incomparable,
            5 => SecondStun::Compared(stun::Mapping::Same),
            6 => SecondStun::Compared(stun::Mapping::PortOnly),
            7 => SecondStun::Compared(stun::Mapping::AddrOnly),
            8 => SecondStun::Compared(stun::Mapping::Both),
            _ => SecondStun::NotAsked,
        }
    }

    /// 宛先ごとに外側のアドレスが変わる型か（比べられたときだけ Some）。画面の左上の文（作り直しの後の 2 行目）が読む。
    pub fn depends_on_destination(self) -> Option<bool> {
        match self {
            SecondStun::Compared(m) => Some(m.depends_on_destination()),
            _ => None,
        }
    }

    /// 本ログ・試験のまとめの句（アドレス・ポートの値は書かない）。
    pub fn text(self) -> &'static str {
        match self {
            SecondStun::NotAsked => {
                "2 か所目の STUN: 聞いていない（ini に stun2 が無い・scope が internet でない・1 か所目の答えが無かったのどれか）"
            }
            SecondStun::Pending => {
                "2 か所目の STUN: 聞いている途中（名前を引いている・答えを待っている）"
            }
            SecondStun::Skipped => {
                "2 か所目の STUN: 宛先を決められず聞かなかった（名前を引けない・範囲の外・1 か所目と同じアドレスのどれか）"
            }
            SecondStun::NoAnswer => {
                "2 か所目の STUN: 答えが返らなかった（宛先ごとに外側のアドレスが変わる型かは分からない）"
            }
            SecondStun::Incomparable => {
                "2 か所目の STUN: 答えのアドレスの族が 1 か所目と違い、比べられなかった"
            }
            SecondStun::Compared(stun::Mapping::Same) => {
                "2 か所目の STUN: 外側のアドレスもポートも 1 か所目と同じ（宛先に依らない割り当て）"
            }
            SecondStun::Compared(stun::Mapping::PortOnly) => {
                "2 か所目の STUN: 外側のポートだけ 1 か所目と違う（宛先ごとに外側のポートを変える型。STUN で見えたアドレスは相手には使えない見込み）"
            }
            SecondStun::Compared(stun::Mapping::AddrOnly) => {
                "2 か所目の STUN: 外側のアドレスが 1 か所目と違う（ポートは同じ。宛先によって出口のアドレスが変わる回線）"
            }
            SecondStun::Compared(stun::Mapping::Both) => {
                "2 か所目の STUN: 外側のアドレスもポートも 1 か所目と違う（宛先ごとに外側のアドレスを変える型。STUN で見えたアドレスは相手には使えない見込み）"
            }
        }
    }
}

/// 繋がらなかった理由を読むための診断（アドレス・ポートの値は持たない。数と真偽だけ）。本ログと試験のまとめが読む。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetDiag {
    /// 自分のコードに載せた候補の種類ごとの数（`link::kind_slot` の並び。記録していなければ全部 0）。
    pub own_cands: [u32; KIND_SLOTS],
    /// 送りの誤り（候補の種類ごと）。
    pub send_errors: [u32; KIND_SLOTS],
    /// 外側のポートがソケットのポートと同じか（STUN の答えが無ければ None）。違えばルーターが付け替えている。
    pub port_kept: Option<bool>,
    /// STUN で外側のアドレスを得た時刻（ネットワークを開いてからの ms。得ていなければ None）。
    pub stun_at_ms: Option<u32>,
    pub keep: KeepDiag,
    /// 2 か所目の STUN の結果（宛先ごとに外側のポートを変える型かの見分け）。
    pub second: SecondStun,
    /// UPnP の割り当ての結果と数（頼んでいなければ `State::NotTried`）。
    pub upnp: upnp::Stats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepDiag {
    /// 聞き直しを置いたか（STUN を使わない・範囲が internet でない・答えが無いときは false）。
    pub installed: bool,
    /// 受信スレッドが今も聞き直しているか（繋がった・上限に来たら false）。
    pub running: bool,
    pub counts: stun::KeepCounts,
    /// 送りの誤り・乱数を引けなかったなどで送れなかった数。
    pub send_errors: u32,
}

impl NetDiag {
    pub fn port_text(&self) -> &'static str {
        match self.port_kept {
            Some(true) => "外側のポートは受信ソケットのポートと同じ",
            Some(false) => "外側のポートは受信ソケットのポートと違う（ルーターが付け替えている）",
            None => "外側のポートは不明（STUN の答えが無い）",
        }
    }

    pub fn keep_text(&self) -> String {
        let k = &self.keep;
        if !k.installed {
            return "STUN の聞き直し: していない（ini に stun が無い・scope が internet でない・STUN の答えが無かったのどれか）"
                .to_string();
        }
        let c = &k.counts;
        let moved = c.changed_port.saturating_add(c.changed_addr);
        let now = if c.answered == 0 {
            "答えがまだ無い"
        } else if c.differs_from_code {
            "いまの外側のアドレスは、コードに載せたものと違う"
        } else {
            "いまの外側のアドレスは、コードに載せたものと同じ"
        };
        format!(
            "STUN の聞き直し: {} / 送った {} 回（送れなかった {}） / 答え {} 回（答えにならなかったもの {}） / 外側のアドレスが変わった {} 回（ポートだけ {} / アドレスも {}）/ {}",
            if k.running {
                "続けている"
            } else {
                "止めた"
            },
            c.sent,
            k.send_errors,
            c.answered,
            c.ignored,
            moved,
            c.changed_port,
            c.changed_addr,
            now
        )
    }
}

enum KeepPhase {
    NotYet,
    Running(Box<KeepRun>),
    /// 止めた（繋がった・上限に来た・宛先がガードの外）。もう送らない。
    Done,
}

struct KeepRun {
    server: SocketAddr,
    keep: stun::Keep,
    started: u32,
}

fn now_ms(shared: &Shared) -> u32 {
    (shared.t0.elapsed().as_millis() & u128::from(u32::MAX)) as u32
}

fn publish_keep(shared: &Shared, k: &stun::Keep) {
    let c = k.counts();
    let s = &shared.keep;
    s.sent.store(c.sent, Ordering::Relaxed);
    s.answered.store(c.answered, Ordering::Relaxed);
    s.ignored.store(c.ignored, Ordering::Relaxed);
    s.changed_port.store(c.changed_port, Ordering::Relaxed);
    s.changed_addr.store(c.changed_addr, Ordering::Relaxed);
    s.differs.store(c.differs_from_code, Ordering::Relaxed);
}

/// 受信スレッドの 1 周ごとの聞き直し: 設定を取り、止める時を見て、送る時刻なら 1 通送る。置いていない実行は `OnceLock::get` 1 回だけ。
fn keep_step(phase: &mut KeepPhase, sock: &UdpSocket, shared: &Shared) {
    if matches!(phase, KeepPhase::NotYet) {
        let Some(setup) = shared.keep.setup.get() else {
            return;
        };
        let now = now_ms(shared);
        *phase = KeepPhase::Running(Box::new(KeepRun {
            server: setup.server,
            keep: stun::Keep::new(setup.mapped, now, setup.interval_ms),
            started: now,
        }));
        shared.keep.running.store(true, Ordering::Release);
    }
    let KeepPhase::Running(k) = phase else {
        return;
    };
    let now = now_ms(shared);
    let stop = !shared.keep.run.load(Ordering::Acquire)
        || now.wrapping_sub(k.started) >= KEEP_MAX_MS
        || !guard::allows(shared.scope, k.server);
    if stop {
        *phase = KeepPhase::Done;
        shared.keep.running.store(false, Ordering::Release);
        return;
    }
    if !k.keep.due(now) {
        return;
    }
    let mut txn = [0u8; stun::TRANSACTION_ID_LEN];
    if !os_random(&mut txn) {
        // 乱数を引けなくても聞き直しは続ける（transaction id は答えの取り違えを防ぐだけで、秘密ではない）。
        let nanos = shared.t0.elapsed().as_nanos().to_le_bytes();
        for (d, s) in txn.iter_mut().zip(nanos) {
            *d = s;
        }
    }
    let req = k.keep.send(now, txn);
    if sock.send_to(&req, k.server).is_err() {
        shared.keep.send_errors.fetch_add(1, Ordering::Relaxed);
    }
    publish_keep(shared, &k.keep);
}

/// 聞き直しの答え（STUN のサーバーから来た STUN）を受けたとき。セッションへは渡さない。
fn keep_answer(k: &mut KeepRun, shared: &Shared, bytes: &[u8]) {
    let _ = k.keep.on_datagram(bytes);
    publish_keep(shared, &k.keep);
}

enum SecondPhase {
    NotYet,
    Running(Box<SecondRun>),
    /// 結論を書いた（または宛先がガードの外）。もう送らない。
    Done,
}

struct SecondRun {
    server: SocketAddr,
    probe: stun::SecondProbe,
}

fn second_finish(phase: &mut SecondPhase, shared: &Shared, result: SecondStun) {
    shared.diag.second.store(result.code(), Ordering::Relaxed);
    *phase = SecondPhase::Done;
}

fn second_result(o: stun::SecondOutcome) -> SecondStun {
    match o {
        stun::SecondOutcome::Compared(m) => SecondStun::Compared(m),
        stun::SecondOutcome::Incomparable => SecondStun::Incomparable,
        stun::SecondOutcome::NoAnswer => SecondStun::NoAnswer,
    }
}

/// 受信スレッドの 1 周ごとの 2 か所目の STUN: 宛先を取り、問い合わせを送り直し（最大 8 通。250 ms ごと）、結論が出たら書く。
/// 置いていない実行は `OnceLock::get` 1 回だけ。
fn second_step(phase: &mut SecondPhase, sock: &UdpSocket, shared: &Shared) {
    if matches!(phase, SecondPhase::NotYet) {
        let Some(setup) = shared.second.setup.get() else {
            return;
        };
        let mut txn = [0u8; stun::TRANSACTION_ID_LEN];
        if !os_random(&mut txn) {
            // 乱数を引けなくても続ける（transaction id は答えの取り違えを防ぐだけで、秘密ではない。聞き直しと同じ）。
            let nanos = shared.t0.elapsed().as_nanos().to_le_bytes();
            for (d, s) in txn.iter_mut().zip(nanos) {
                *d = s;
            }
        }
        *phase = SecondPhase::Running(Box::new(SecondRun {
            server: setup.server,
            probe: stun::SecondProbe::new(txn, setup.first),
        }));
    }
    let SecondPhase::Running(r) = phase else {
        return;
    };
    if !guard::allows(shared.scope, r.server) {
        second_finish(phase, shared, SecondStun::Skipped);
        return;
    }
    if let Some(req) = r.probe.poll_send(now_ms(shared)) {
        // 送れなくても次の周回で送り直す（8 回の枠は `SecondProbe` が数える）。
        let _ = sock.send_to(&req, r.server);
    }
    if let Some(o) = r.probe.outcome() {
        second_finish(phase, shared, second_result(o));
    }
}

/// 2 か所目のサーバーから来た STUN を受けたとき（呼び手が送り元と STUN であることを確かめてある）。セッションへは渡さない。
fn second_answer(phase: &mut SecondPhase, shared: &Shared, bytes: &[u8]) {
    let SecondPhase::Running(r) = phase else {
        return;
    };
    r.probe.on_datagram(bytes);
    if let Some(o) = r.probe.outcome() {
        second_finish(phase, shared, second_result(o));
    }
}

/// 受信スレッドの PONG の口（`higanr_core::pong`）。ゲームのスレッドは繋がった tick に口を置き（`GameNet::pong_install`。錠は
/// `try_lock` だけで待たない）、生存の状態が変わった tick に `on` を書く（`NetPlay::sync_pong`）。受信スレッドは `on` の間だけ PING に
/// PONG を返し、口のコピーを自分で持つ（世代が変わったときだけ錠を取ってコピーする）。
#[derive(Default)]
struct PongSlot {
    /// 置いた口の世代（0 = まだ無い）。口を書いた後に Release で進める。
    generation: AtomicU32,
    /// 返してよいか（生存が「生きている」の間だけ true）。
    on: AtomicBool,
    /// ゲームスレッドの心拍（`Net::heartbeat` が tick ごとに書く `now_ms`。0 はまだ書いていない）。古ければ PONG を返さない
    /// （`liveness::heartbeat_fresh`）。
    beat: AtomicU32,
    /// 0 でなければ、ゲームスレッドが止まったので受信スレッドが BYE を出す（値は `bye_code` の理由）。立ったら PING には PONG の代わりに BYE を返す。
    bye: AtomicU32,
    responder: Mutex<Option<PongResponder>>,
    /// PONG の通し番号。IPv4・IPv6 の 2 本の受信スレッドと、置き直したどの口も、この 1 つを進める。ネットワークと同じ寿命で 0 に戻さない
    /// （鍵はセッションごとに替わるので、鍵ごとに 0 からでなくても重ならないことだけが要る）。
    seq: higanr_core::seal::Seq,
    /// 返した数 / ガード（宛先がネットワークの範囲 `Shared::scope` の中か）で止めた数 / 送りの誤り（ログと検査用）。
    sent: AtomicU32,
    refused: AtomicU32,
    send_errors: AtomicU32,
    /// 心拍が古いので返さなかった PING の数と、受信スレッドが出した BYE の数（頼まれずに出した 1 通と PING への返し）。
    stale: AtomicU32,
    byes_sent: AtomicU32,
}

fn bye_code(r: ByeReason) -> u32 {
    r as u32
}

fn bye_of(code: u32) -> Option<ByeReason> {
    [
        ByeReason::Finished,
        ByeReason::Disconnected,
        ByeReason::Desync,
        ByeReason::Cancelled,
    ]
    .into_iter()
    .find(|&r| bye_code(r) == code)
}

/// 観戦のホストの受け渡し。読み取りのスレッドが開くときに 1 回だけ置き、受信スレッドが 1 回だけ取って自分で持つ（周回ごとには錠を
/// 取らない）。置いていなければ、受信スレッドは今までどおり。
#[derive(Default)]
struct SpectateSlot {
    installed: AtomicBool,
    /// 受信スレッドが取って持った（この後に届いた観戦のデータグラムは観戦のホストが受ける）。
    taken: AtomicBool,
    /// 観戦のホストの中でパニックして、受信スレッドが観戦のホストを捨てた（対戦の受信は続く）。
    stopped: AtomicBool,
    pending: Mutex<Option<SpectateSender>>,
    /// 本ログの集計（置いた後は読み取りのスレッドが読める）。
    shared: std::sync::OnceLock<Arc<spectate_host::Shared>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecvStats {
    pub ring: RingCounts,
    /// IPv6 のソケットのリング（IPv6 の経路を開いていなければ全部 0）。
    pub ring6: RingCounts,
    pub conn_resets: u32,
    pub recv_errors: u32,
    /// 受信スレッドがその場で返した PONG / 宛先のガードで止めた PONG / 送りの誤り（`PongSlot`）。
    pub pongs_sent: u32,
    pub pongs_refused: u32,
    pub pong_send_errors: u32,
    /// ゲームスレッドの心拍が古いので返さなかった PING / ゲームスレッドが止まった後に受信スレッドが出した BYE。
    pub pongs_stale: u32,
    pub byes_sent: u32,
    /// 繋がらなかった理由を読むための診断（STUN の聞き直し・送りの誤り・候補の数。`NetDiag`）。
    pub diag: NetDiag,
}

pub struct RecvThread {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

pub struct GameNet {
    sock: Arc<UdpSocket>,
    shared: Arc<Shared>,
    waiter: Waiter,
    sends: u64,
    send_errors: u64,
    /// ネットワークの範囲の外の宛先へ送ろうとして止めた数（セッションは候補と送り元にしか送らず、候補は開くときにガードを通してあるので 0 のはず）。
    send_refused: u64,
    /// ネットワークの範囲（開くときに受けた ini の `scope`）。送る直前のガードと STUN の宛先のガードが使う。
    scope: Scope,
}

/// 開く。`local` に bind し（ポート 0 なら OS が選ぶ）、受信スレッドを起こす。ソケットか相手の候補（`peers`）の
/// どれかがネットワークの範囲 `scope` の外なら、bind する前に断る。
pub fn open_scoped(
    scope: Scope,
    local: SocketAddr,
    peers: &[SocketAddr],
) -> Result<(GameNet, RecvThread), String> {
    guard::check_bind(local, scope, "受信ソケット")?;
    if peers.is_empty() {
        return Err("相手の候補が無い".to_string());
    }
    for &p in peers {
        guard::check_peer(p, scope)?;
    }
    open_socket(scope, local)
}

/// 相手の候補をまだ知らずに開く（接続コードの受け渡しの形。ini の `code_side`）。相手の候補は接続コードから来て、`net_code` がセッションへ
/// 渡す前にガード（`guard::check_peer`）を通す。
pub fn open_for_code_scoped(
    scope: Scope,
    local: SocketAddr,
) -> Result<(GameNet, RecvThread), String> {
    guard::check_bind(local, scope, "受信ソケット")?;
    open_socket(scope, local)
}

/// 検査の略記: 範囲 loopback で `open_scoped`。
#[cfg(test)]
pub fn open(local: SocketAddr, peers: &[SocketAddr]) -> Result<(GameNet, RecvThread), String> {
    open_scoped(Scope::Loopback, local, peers)
}

/// 検査の略記: 範囲 loopback で `open_for_code_scoped`。
#[cfg(test)]
pub fn open_for_code(local: SocketAddr) -> Result<(GameNet, RecvThread), String> {
    open_for_code_scoped(Scope::Loopback, local)
}

fn open_socket(scope: Scope, local: SocketAddr) -> Result<(GameNet, RecvThread), String> {
    let sock = UdpSocket::bind(local).map_err(|e| format!("bind に失敗: {e}"))?;
    // bind した後の実際のソケットももう一度見る（ガードは 2 か所 —— peer_sim と同じ）。
    let bound = sock
        .local_addr()
        .map_err(|e| format!("受信ソケットが分からない: {e}"))?;
    guard::check_bind(bound, scope, "受信ソケット")?;
    sock.set_read_timeout(Some(RECV_POLL))
        .map_err(|e| format!("受信の待ちの上限を置けない: {e}"))?;
    let event = Arc::new(Event::new().ok_or("受信のイベントを作れない")?);
    let shared = Arc::new(Shared {
        ring: RecvRing::new(),
        t0: Instant::now(),
        event: Arc::clone(&event),
        stop: AtomicBool::new(false),
        conn_resets: AtomicU32::new(0),
        recv_errors: AtomicU32::new(0),
        pong: PongSlot::default(),
        spectate: SpectateSlot::default(),
        scope,
        keep: KeepSlot::default(),
        second: SecondSlot::default(),
        diag: DiagSlot::default(),
        sock6: std::sync::OnceLock::new(),
        ring6: RecvRing::new(),
        join6: Mutex::new(None),
        upnp: std::sync::OnceLock::new(),
    });
    let sock = Arc::new(sock);
    let join = {
        let sock = Arc::clone(&sock);
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("higanr-recv".into())
            .spawn(move || {
                // パニックは受けて外へ出さない（スレッドの入口）。受けたら受信をやめる（相手の沈黙として liveness が扱う）。
                let _ = catch_unwind(AssertUnwindSafe(|| recv_loop(&sock, &shared)));
            })
            .map_err(|e| format!("受信スレッドを起こせない: {e}"))?
    };
    let net = GameNet {
        sock,
        shared: Arc::clone(&shared),
        waiter: Waiter::new(event),
        sends: 0,
        send_errors: 0,
        send_refused: 0,
        scope,
    };
    Ok((
        net,
        RecvThread {
            shared,
            join: Some(join),
        },
    ))
}

fn recv_loop(sock: &UdpSocket, shared: &Shared) {
    let mut buf = [0u8; RECV_BUF];
    let mut cache: (u32, Option<PongResponder>) = (0, None);
    let mut spectate: Option<SpectateSender> = None;
    let mut spectate_taken = false;
    let mut keep = KeepPhase::NotYet;
    let mut second = SecondPhase::NotYet;
    let mut bye_done = false;
    while !shared.stop.load(Ordering::Relaxed) {
        if !spectate_taken && shared.spectate.installed.load(Ordering::Acquire) {
            spectate_taken = true;
            spectate = shared
                .spectate
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if spectate.is_some() {
                // 受け待ちの上限（`RECV_POLL`）は変えない（短くすると対戦の相手のパケットを落とす）。リングバッファを空にして送る周回は、対戦の
                // 相手のパケットと観戦者の ACK が来るたびと、待ちが切れたときに回る。
                shared.spectate.taken.store(true, Ordering::Release);
            }
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let t_us = shared.t0.elapsed().as_micros() as u64;
                let bytes = buf.get(..n).unwrap_or(&[]);
                let ours = spectate
                    .as_ref()
                    .is_some_and(|sp| wire::peek_session(bytes) == Some(sp.session_id()));
                let keep_answer_for = match &mut keep {
                    KeepPhase::Running(k)
                        if from == k.server && wire::classify(bytes) == Datagram::Stun =>
                    {
                        Some(k)
                    }
                    _ => None,
                };
                let second_answer_for = matches!(
                    &second,
                    SecondPhase::Running(r)
                        if from == r.server && wire::classify(bytes) == Datagram::Stun
                );
                if let Some(k) = keep_answer_for {
                    keep_answer(k, shared, bytes);
                } else if second_answer_for {
                    second_answer(&mut second, shared, bytes);
                } else if ours {
                    let now = (t_us / 1_000) as u32;
                    run_spectate(&mut spectate, shared, |sp| {
                        sp.on_datagram(sock, from, bytes, now)
                    });
                } else {
                    let pong_sent = answer_ping(sock, shared, &mut cache, from, bytes);
                    shared.ring.push_marked(from, bytes, t_us, pong_sent);
                    shared.event.set();
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                shared.conn_resets.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                shared.recv_errors.fetch_add(1, Ordering::Relaxed);
                // 同じ誤りが続くときに空回りしない。
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        bye_once(sock, shared, &mut cache, &mut bye_done);
        keep_step(&mut keep, sock, shared);
        second_step(&mut second, sock, shared);
        if spectate.is_some() {
            let now = (shared.t0.elapsed().as_micros() / 1_000) as u32;
            run_spectate(&mut spectate, shared, |sp| sp.tick(sock, now));
        }
    }
}

/// IPv6 の受信スレッド。IPv4 の `recv_loop` の、対戦の相手の受けだけを残した形: 受けた 1 通を IPv6 のリング（`ring6`）へ積み、PING
/// ならその場で PONG を返す（返すソケットは `answer_ping` が経路の族で選ぶので、IPv4 のソケット `sock4` も渡す）。観戦・STUN の聞き直しは IPv4 の
/// ソケットの仕事。
fn recv_loop_v6(sock6: &UdpSocket, sock4: &UdpSocket, shared: &Shared) {
    let mut buf = [0u8; RECV_BUF];
    let mut cache: (u32, Option<PongResponder>) = (0, None);
    while !shared.stop.load(Ordering::Relaxed) {
        match sock6.recv_from(&mut buf) {
            Ok((n, from)) => {
                let t_us = shared.t0.elapsed().as_micros() as u64;
                let bytes = buf.get(..n).unwrap_or(&[]);
                let pong_sent = answer_ping(sock4, shared, &mut cache, from, bytes);
                shared.ring6.push_marked(from, bytes, t_us, pong_sent);
                shared.event.set();
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                shared.conn_resets.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                shared.recv_errors.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// 観戦のホストの呼び出し 1 回を包む（置いていなければ何もしない）。パニックしたら観戦のホストを捨てる: 受信スレッド全体が止まると
/// 対戦の相手のパケットも受けられなくなるので、観戦だけを止めて対戦の受信を続ける。捨てたら `stopped` を立てる。
fn run_spectate(
    spectate: &mut Option<SpectateSender>,
    shared: &Shared,
    f: impl FnOnce(&mut SpectateSender),
) {
    let Some(sp) = spectate.as_mut() else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| f(sp))).is_err() {
        *spectate = None;
        shared.spectate.stopped.store(true, Ordering::Release);
    }
}

/// 受信スレッド: `bytes` がこの接続の本物の PING で、返してよい間なら、その場で PONG を返す（返したら true）。PING でない 1 通は
/// type の 1 バイトを見て素通りする（INPUT の受けにタグの計算を足さない）。送れなかったら false（セッションが返す）。
fn answer_ping(
    sock: &UdpSocket,
    shared: &Shared,
    cache: &mut (u32, Option<PongResponder>),
    from: SocketAddr,
    bytes: &[u8],
) -> bool {
    let slot = &shared.pong;
    if !wire::peek_is_ping(bytes) {
        return false;
    }
    let bye = bye_of(slot.bye.load(Ordering::Acquire));
    if bye.is_none() {
        if !slot.on.load(Ordering::Acquire) {
            return false;
        }
        // 心拍を先に読み、時刻は後で取る（同じ時計なので、書き手が先に進んでいても差は小さい）。
        let beat = slot.beat.load(Ordering::Acquire);
        if !liveness::heartbeat_fresh(beat, now_ms(shared)) {
            slot.stale.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    }
    let Some(r) = cached_responder(slot, cache) else {
        return false;
    };
    let mut out = [0u8; MAX_DATAGRAM];
    let answered = match bye {
        Some(reason) => r.answer_bye(&slot.seq, from, bytes, reason, &mut out),
        None => r.answer(&slot.seq, from, bytes, &mut out),
    };
    let Some((to, n)) = answered else {
        return false;
    };
    if !send_to_path(sock, shared, to, out.get(..n).unwrap_or(&[])) {
        return false;
    }
    let count = if bye.is_some() {
        &slot.byes_sent
    } else {
        &slot.sent
    };
    count.fetch_add(1, Ordering::Relaxed);
    true
}

/// 置かれた口のコピー（世代が変わったときだけ錠を取ってコピーする。ゲームのスレッドは try_lock しかしないので、ここで待つのは受信スレッドだけ）。
fn cached_responder<'a>(
    slot: &PongSlot,
    cache: &'a mut (u32, Option<PongResponder>),
) -> Option<&'a PongResponder> {
    let generation = slot.generation.load(Ordering::Acquire);
    if generation != cache.0 {
        let g = slot.responder.lock().unwrap_or_else(|e| e.into_inner());
        cache.1 = g.clone();
        cache.0 = generation;
    }
    cache.1.as_ref()
}

/// 受信スレッドが経路へ 1 通送る。送る直前のガードは `Outlet::send`（`GameNet::may_send`）と同じ関数・同じ範囲。返す先（繋がった経路）の族の
/// ソケットから送る（PING が来たソケットと同じとは限らない。IPv6 のソケットを開いていなければ `sock` のまま）。
fn send_to_path(sock: &UdpSocket, shared: &Shared, to: SocketAddr, bytes: &[u8]) -> bool {
    let slot = &shared.pong;
    if !guard::allows(shared.scope, to) {
        slot.refused.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let out_sock: &UdpSocket = match (to.is_ipv6(), shared.sock6.get()) {
        (true, Some(s6)) => s6,
        _ => sock,
    };
    if out_sock.send_to(bytes, to).is_err() {
        slot.send_errors.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

/// ゲームスレッドが止まった（`RecvThread::say_bye`）後の最初の 1 回だけ、頼まれずに BYE を出す。口が無い（繋がる前）なら何も出さない。
/// 以後は相手の PING への返しが BYE になる（`answer_ping`）ので、この 1 通が落ちても次の PING で届く。
fn bye_once(
    sock: &UdpSocket,
    shared: &Shared,
    cache: &mut (u32, Option<PongResponder>),
    done: &mut bool,
) {
    if *done {
        return;
    }
    let slot = &shared.pong;
    let Some(reason) = bye_of(slot.bye.load(Ordering::Acquire)) else {
        return;
    };
    *done = true;
    let Some(r) = cached_responder(slot, cache) else {
        return;
    };
    let mut out = [0u8; MAX_DATAGRAM];
    if let Some((to, n)) = r.bye(&slot.seq, reason, &mut out)
        && send_to_path(sock, shared, to, out.get(..n).unwrap_or(&[]))
    {
        slot.byes_sent.fetch_add(1, Ordering::Relaxed);
    }
}

impl RecvThread {
    pub fn stats(&self) -> RecvStats {
        let p = &self.shared.pong;
        RecvStats {
            ring: self.shared.ring.counts(),
            ring6: self.shared.ring6.counts(),
            conn_resets: self.shared.conn_resets.load(Ordering::Relaxed),
            recv_errors: self.shared.recv_errors.load(Ordering::Relaxed),
            pongs_sent: p.sent.load(Ordering::Relaxed),
            pongs_refused: p.refused.load(Ordering::Relaxed),
            pong_send_errors: p.send_errors.load(Ordering::Relaxed),
            pongs_stale: p.stale.load(Ordering::Relaxed),
            byes_sent: p.byes_sent.load(Ordering::Relaxed),
            diag: self.diag(),
        }
    }

    pub fn diag(&self) -> NetDiag {
        let (k, d) = (&self.shared.keep, &self.shared.diag);
        let l = |a: &AtomicU32| a.load(Ordering::Relaxed);
        NetDiag {
            own_cands: std::array::from_fn(|i| d.own_cands.get(i).map_or(0, l)),
            send_errors: std::array::from_fn(|i| d.send_errors.get(i).map_or(0, l)),
            port_kept: match l(&d.port_kept) {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
            stun_at_ms: l(&d.stun_at_ms).checked_sub(1),
            keep: KeepDiag {
                installed: k.setup.get().is_some(),
                running: k.running.load(Ordering::Acquire),
                counts: stun::KeepCounts {
                    sent: l(&k.sent),
                    answered: l(&k.answered),
                    ignored: l(&k.ignored),
                    changed_port: l(&k.changed_port),
                    changed_addr: l(&k.changed_addr),
                    differs_from_code: k.differs.load(Ordering::Relaxed),
                },
                send_errors: l(&k.send_errors),
            },
            second: SecondStun::from_code(l(&d.second)),
            upnp: self
                .shared
                .upnp
                .get()
                .map(upnp::Handle::stats)
                .unwrap_or_default(),
        }
    }

    /// ゲームスレッドが止まった（パニックを受けた）ので、受信スレッドに BYE を出させる。PONG はもう返さない。原子変数を書くだけで待たない。
    /// 最初の理由だけが効く。
    pub fn say_bye(&self, reason: ByeReason) {
        let p = &self.shared.pong;
        p.on.store(false, Ordering::Release);
        let _ = p
            .bye
            .compare_exchange(0, bye_code(reason), Ordering::AcqRel, Ordering::Acquire);
    }

    /// 観戦のホストを置く（読み取りのスレッド。開くときに 1 回だけ）。2 回目は置かずに false。
    pub fn install_spectate(&self, sender: SpectateSender) -> bool {
        let slot = &self.shared.spectate;
        if slot.shared.set(Arc::clone(sender.shared())).is_err() {
            return false;
        }
        *slot.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
        slot.installed.store(true, Ordering::Release);
        true
    }

    /// 受信スレッドが観戦のホストを取って動かし始めたか（`install_spectate` の後、最大 `RECV_POLL` ほどで true になる。それより前に
    /// 届いた観戦者の JOIN は対戦のセッションへ回って捨てられる —— 観戦者は JOIN を送り直す）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spectate_active(&self) -> bool {
        self.shared.spectate.taken.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn spectate_shared(&self) -> Option<Arc<spectate_host::Shared>> {
        self.shared.spectate.shared.get().cloned()
    }

    pub fn spectate_snapshot(&self) -> Option<spectate_host::Snapshot> {
        let slot = &self.shared.spectate;
        slot.shared.get().map(|s| spectate_host::Snapshot {
            stopped: slot.stopped.load(Ordering::Acquire),
            ..s.get()
        })
    }

    /// 止めて待つ（最大で `RECV_POLL` ほど）。止めた後、ゲームのスレッドのインターフェース（`GameNet`）も落ちていればソケットが閉じる。本物の経路では
    /// ゲーム内のメニューのつなぎ直しの片づけ（`net_code_game::close_parts`）だけが止める。ほかの実行は `script_win::RECV` が
    /// プロセスの終わりまで持つ。
    pub fn stop(mut self) -> RecvStats {
        self.halt();
        self.stats()
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        let j6 = self
            .shared
            .join6
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(j) = j6 {
            let _ = j.join();
        }
        // 観戦の配る側のスレッド（`spectate_offer`）も止めて待つ。つながりの片づけは見張りを先に止めて待つので、ここより後に
        // 起こされることは無い。
        if let Some(s) = self.shared.spectate.shared.get() {
            s.stop_offer();
        }
        // UPnP の割り当てを消す（機器が答えなくても数秒で戻る）。
        if let Some(h) = self.shared.upnp.get() {
            h.stop();
        }
    }
}

impl Drop for RecvThread {
    fn drop(&mut self) {
        self.halt();
    }
}

impl GameNet {
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    /// VPN の範囲（`Scope::LanVpn`）で開いたか。外側の候補の欄を「VPN」と書くかの判断に使う。
    pub fn is_vpn(&self) -> bool {
        matches!(self.scope, Scope::LanVpn(_))
    }

    #[cfg(test)]
    pub fn recv_timeout(&self) -> Option<Duration> {
        self.sock.read_timeout().ok().flatten()
    }

    pub fn high_resolution_wait(&self) -> bool {
        self.waiter.high_resolution()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn send_counts(&self) -> (u64, u64) {
        (self.sends, self.send_errors)
    }

    /// 送る直前のガード（`Outlet::send` が使う）。理由の文を作らない `guard::allows`（確保しない）。検査はこれを直に見る。
    fn may_send(&self, to: SocketAddr) -> bool {
        guard::allows(self.scope, to)
    }
}

impl Outlet for GameNet {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        if !self.may_send(to) {
            self.send_refused = self.send_refused.saturating_add(1);
            return;
        }
        self.sends = self.sends.saturating_add(1);
        // 宛先の族のソケットから出す。IPv6 のソケットを開いていれば IPv6 の宛先はそちら、それ以外はソケットのまま。
        let sock: &UdpSocket = match (to.is_ipv6(), self.shared.sock6.get()) {
            (true, Some(s6)) => s6,
            _ => &self.sock,
        };
        if sock.send_to(bytes, to).is_err() {
            self.send_errors = self.send_errors.saturating_add(1);
            if let Some(a) = self.shared.diag.send_errors.get(kind_slot(kind_of(to))) {
                a.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// STUN の問い合わせの間、待ちを刻む長さ（µs）。答えが届けば受信のイベントで早く起きる。
const STUN_POLL_US: u32 = 5_000;

impl GameNet {
    /// STUN の Binding を `server` へ送り、見えた自分のアドレスを返す（`higanr_core::stun::Query`。250 ms ごとに 8 回、計 2 秒で
    /// 打ち切る）。**読み取りのスレッドで、ネットワークを動かす前（`start_net` の前）にだけ呼ぶ**（待つ・確保する）。その間に届いた STUN 以外の
    /// もの（相手の早い HELLO）は捨てる（相手は 20 ms ごとに送り直す）。transaction id は OS の乱数で引く。宛先はネットワークの範囲の中だけ。
    pub fn stun_query(&mut self, server: SocketAddr) -> Result<SocketAddr, String> {
        guard::check_peer(server, self.scope)?;
        let mut txn = [0u8; stun::TRANSACTION_ID_LEN];
        if !os_random(&mut txn) {
            return Err("STUN の transaction id を OS の乱数で引けない".to_string());
        }
        let mut q = stun::Query::new(txn);
        let mut buf = [0u8; MAX_DATAGRAM];
        loop {
            let now = (self.shared.t0.elapsed().as_millis() & u128::from(u32::MAX)) as u32;
            if let Some(req) = q.poll_send(now) {
                self.sock
                    .send_to(&req, server)
                    .map_err(|e| format!("STUN の問い合わせを送れない: {e}"))?;
            }
            while let Some((from, n, _)) = self.shared.ring.pop(&mut buf) {
                let bytes = buf.get(..n).unwrap_or(&[]);
                if from == server && wire::classify(bytes) == Datagram::Stun {
                    q.on_datagram(bytes);
                }
            }
            match q.state() {
                stun::QueryState::Done(a) => return Ok(a),
                stun::QueryState::TimedOut => {
                    return Err(format!(
                        "STUN の答えが {} 回の問い合わせで返らない（{server}）",
                        stun::QUERY_TRIES
                    ));
                }
                stun::QueryState::Waiting => {
                    self.waiter.wait(STUN_POLL_US);
                }
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn send_refused(&self) -> u64 {
        self.send_refused
    }

    /// 外側のアドレスの保ちを置く（読み取りのスレッド。開くときに 1 回だけ）: 受信スレッドが `interval` ごとに `server`（最初の STUN と
    /// 同じサーバー）へ Binding を送り、答えの外側のアドレスを `mapped`（自分のコードに載せたアドレス）と比べて数える。繋がったら止める。
    /// 2 回目は置かずに false。
    pub fn keep_stun_mapping(
        &self,
        server: SocketAddr,
        mapped: SocketAddr,
        interval: Duration,
    ) -> bool {
        let k = &self.shared.keep;
        k.run.store(true, Ordering::Release);
        let interval_ms = u32::try_from(interval.as_millis()).unwrap_or(u32::MAX);
        k.setup
            .set(KeepSetup {
                server,
                mapped,
                interval_ms,
            })
            .is_ok()
    }

    /// 最初の STUN の答え（外側のアドレス `mapped`）を診断に残す: 得た時刻と、外側のポートがソケットのポートと同じか。値そのものは残さない。
    pub fn note_stun_answer(&self, mapped: SocketAddr) {
        let d = &self.shared.diag;
        d.stun_at_ms
            .store(now_ms(&self.shared).saturating_add(1), Ordering::Relaxed);
        let same = self
            .sock
            .local_addr()
            .map(|l| l.port() == mapped.port())
            .ok();
        d.port_kept.store(
            match same {
                Some(true) => 1,
                Some(false) => 2,
                None => 0,
            },
            Ordering::Relaxed,
        );
    }

    /// 2 か所目の STUN へ並行に聞き始める（読み取りのスレッド。最初の STUN の答えを得た直後に 1 回だけ）。名前を引くのは別のスレッド
    /// （引き終えるまで接続コードの作成を待たせない。OS の名前引きは中断できないので、引き終えなくても放っておく）で、引けたら宛先を
    /// 置き、問い合わせ（最大 8 通）は受信スレッドが同じソケットから送る（同じソケットから出さないと、宛先ごとに外側のポートが変わるかを
    /// 見分けられない）。結論は診断（`NetDiag::second`）に出る。答えが来なくても、何も止めない・接続コードの候補にも入れない。
    ///
    /// 戻りは、聞き始めたか（スレッドを起こせなければ false）。`first_server` と宛先が同じアドレスなら聞かない（同じ宛先へ 2 回聞くと
    /// 比べる意味が無い）。
    pub fn ask_second_stun(
        &self,
        target: StunTarget,
        first_server: SocketAddr,
        first_mapped: SocketAddr,
        lookup: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String> + Send + 'static,
    ) -> bool {
        let shared = Arc::clone(&self.shared);
        let scope = self.scope;
        let want_v4 = first_server.is_ipv4();
        shared
            .diag
            .second
            .store(SecondStun::Pending.code(), Ordering::Relaxed);
        let job = {
            let shared = Arc::clone(&shared);
            move || {
                // 名前引きの中のパニックは受けて、聞かなかったことにする（スレッドの入口）。
                let found = catch_unwind(AssertUnwindSafe(|| {
                    resolve_stun(&target, scope, want_v4, lookup)
                }));
                match found {
                    Ok(Ok(server)) if server != first_server => {
                        let _ = shared.second.setup.set(SecondSetup {
                            server,
                            first: first_mapped,
                        });
                    }
                    _ => shared
                        .diag
                        .second
                        .store(SecondStun::Skipped.code(), Ordering::Relaxed),
                }
            }
        };
        let spawned = std::thread::Builder::new()
            .name("higanr-stun2".into())
            .spawn(job);
        if spawned.is_err() {
            shared
                .diag
                .second
                .store(SecondStun::Skipped.code(), Ordering::Relaxed);
        }
        spawned.is_ok()
    }

    /// 2 か所目の STUN の今の結果。
    pub fn second_stun(&self) -> SecondStun {
        SecondStun::from_code(self.shared.diag.second.load(Ordering::Relaxed))
    }

    /// UPnP の割り当ての持ち手を置く（読み取りのスレッド。開くときに 1 回だけ）。2 回目は置かずに false（受け取った持ち手は落ちて、
    /// 割り当てを消す）。
    pub fn upnp_attach(&self, h: upnp::Handle) -> bool {
        self.shared.upnp.set(h).is_ok()
    }

    /// UPnP の結果を、頼み始めてから `total` まで待つ。頼んでいなければ `None`。
    pub fn upnp_outcome(&self, total: Duration) -> Option<upnp::Outcome> {
        self.shared.upnp.get().map(|h| h.wait_until(total))
    }

    /// 割り当てられたが使わないと決めた UPnP の割り当てを、消すよう知らせる（待たない）。
    pub fn upnp_discard(&self, why: upnp::State) {
        if let Some(h) = self.shared.upnp.get() {
            h.discard(why);
        }
    }

    pub fn note_own_candidates(&self, own: &[Candidate]) {
        let mut n = [0u32; KIND_SLOTS];
        for c in own {
            if let Some(x) = n.get_mut(kind_slot(c.kind)) {
                *x = x.saturating_add(1);
            }
        }
        for (a, v) in self.shared.diag.own_cands.iter().zip(n) {
            a.store(v, Ordering::Relaxed);
        }
    }

    /// IPv6 のソケットを別に 1 本開き、受信スレッドとリングを 1 組足す（読み取りのスレッドで、ネットワークを動かす前に 1 回だけ）。IPv4 の経路は
    /// 何も変わらない。`bind` は IPv6 のアドレス（全部のインターフェース `::` は断る。ガードは IPv4 と同じ `guard::check_bind`）。戻りは bind した後の
    /// 実際のアドレス。2 回目は開かずに Err。
    ///
    /// IPv6 のソケットは IPv4 と別のソケット（IPV6_V6ONLY が既定で真の Windows では 1 本で両方は受けられない）。送る側は宛先の族で
    /// ソケットを選び（`Outlet::send`）、受ける側はゲームのスレッドが 2 つのリングを順に見る（`Net::recv`）。
    pub fn open_ipv6(&self, bind: SocketAddr) -> Result<SocketAddr, String> {
        if !bind.is_ipv6() {
            return Err(format!(
                "IPv6 の受信ソケットに IPv4 のアドレス {bind} は使えない"
            ));
        }
        guard::check_bind(bind, self.scope, "IPv6 の受信ソケット")?;
        if self.shared.sock6.get().is_some() {
            return Err("IPv6 の受信ソケットはもう開いている".to_string());
        }
        // ソケットそのものが IPv6 のとき（ini の `bind` に IPv6 のアドレスを書いた形）は、IPv6 の宛先をそのソケットが受け持つので足さない。
        if self.sock.local_addr().is_ok_and(|a| a.is_ipv6()) {
            return Err(
                "受信ソケットがもう IPv6（IPv6 の経路は IPv4 の受信ソケットに足す形）".to_string(),
            );
        }
        let sock = UdpSocket::bind(bind)
            .map_err(|e| format!("IPv6 の受信ソケット {bind} の bind に失敗: {e}"))?;
        let bound = sock
            .local_addr()
            .map_err(|e| format!("IPv6 の受信ソケットが分からない: {e}"))?;
        guard::check_bind(bound, self.scope, "IPv6 の受信ソケット")?;
        sock.set_read_timeout(Some(RECV_POLL))
            .map_err(|e| format!("IPv6 の受信の待ちの上限を置けない: {e}"))?;
        let sock = Arc::new(sock);
        let join = {
            let (sock6, sock4) = (Arc::clone(&sock), Arc::clone(&self.sock));
            let shared = Arc::clone(&self.shared);
            std::thread::Builder::new()
                .name("higanr-recv6".into())
                .spawn(move || {
                    // パニックは受けて外へ出さない（スレッドの入口）。受けたら IPv6 の受信だけをやめる（IPv4 の経路は続く）。
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        recv_loop_v6(&sock6, &sock4, &shared);
                    }));
                })
                .map_err(|e| format!("IPv6 の受信スレッドを起こせない: {e}"))?
        };
        *self.shared.join6.lock().unwrap_or_else(|e| e.into_inner()) = Some(join);
        self.shared
            .sock6
            .set(sock)
            .map_err(|_| "IPv6 の受信ソケットをもう置いてある".to_string())?;
        Ok(bound)
    }
}

/// 開いた後、ネットワークを動かす前の用意（読み取りのスレッド。`script_win::open_net` から）: 秘密（ini に無ければ OS の乱数で引く）・STUN の
/// 問い合わせ（ini の `stun` があるとき）・繋がりのセッションの設定。戻りの文はログの 1 行に足す（秘密を引いたときは、自分の始める側の接続
/// コード —— peer_sim の `--code` で読む —— を含む）。STUN が答えなくてもネットワークは開く。
#[cfg(test)]
pub fn prepare_link(
    s: &NetSettings,
    net: &mut GameNet,
    role_nonce: u64,
) -> Result<(link::Config, String), String> {
    prepare_link_stun(s, net, role_nonce).map(|(c, n, _)| (c, n))
}

/// `prepare_link` の、STUN の結果の種類も返す形（試験のまとめの「STUN の答え」の行）。
pub fn prepare_link_stun(
    s: &NetSettings,
    net: &mut GameNet,
    role_nonce: u64,
) -> Result<(link::Config, String, Stun), String> {
    let mut note = String::new();
    let (mapped, stun) = stun_state(s, net, &mut note);
    let v6 = open_ipv6(s, net, &mut note);
    let secret = match s.secret {
        Some(x) => x,
        None => {
            let b = draw_secret()?;
            let local = net.local_addr().ok_or("受信ソケットが分からない")?;
            // `peer` の形では、読むときに session を必ず Some にしてある（`net_config::from_ini`）。
            let session = s.session_id.ok_or("session が無い")?;
            let code = s.initiator_code(b, session, local, mapped, v6, now_unix())?;
            note.push_str(&format!(
                " / 秘密を OS の乱数で引いた。自分の接続コード（始める側。peer_sim --code で読む）: {code}"
            ));
            b
        }
    };
    let key_seed = draw_key_seed()?;
    Ok((s.link_config(role_nonce, secret, key_seed), note, stun))
}

/// 名前を引く待ちの上限。オフラインの DNS は何秒も返らないことがあり、ネットワークを開くのを止めないため。
const DNS_TIMEOUT: Duration = Duration::from_secs(2);

/// 経路表を引くための文書用の宛先（RFC 5737 の TEST-NET-1。実在の相手ではない）。UDP の `connect` は何も送らず、OS が「この宛先へ
/// 出るときの自分のアドレス」を決めるだけ。
const ROUTE_PROBE: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9);

/// 外へ出る経路の自分の IPv4 のアドレスを、OS の経路表から引く（`Bind::resolve` の `route`）。0.0.0.0:0 のソケットを作って
/// `ROUTE_PROBE` へ `connect` するだけで、1 通も送らない。経路が無ければ（例: オフライン）`connect` が誤りを返すので、その文を返す。
/// 読み取りのスレッドで開くときにだけ呼ぶ。
pub fn probe_route_ip() -> Result<IpAddr, String> {
    let sock = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .map_err(|e| format!("経路を引くソケットを作れない: {e}"))?;
    let connected = sock.connect(ROUTE_PROBE);
    judge_route(connected, sock.local_addr())
}

/// 経路を引いた OS の答え（`connect` の結果と、その後の自分のアドレス）から、使える IPv4 のアドレスか誤りの文を決める（pure。OS の状態に
/// 依らず、決まった入力で判断を検査するため切り出した）。「アドレスが無い」（0.0.0.0）と「マルチキャスト」は経路が無いのと同じ扱いで
/// 断る。loopback・IPv6・範囲のガードは、呼び手の `Bind::resolve` が決める。リンクローカル 169.254 は、ルーターの無い LAN 直結の相手には
/// 正しいアドレスなのでここでは断らない。
fn judge_route(
    connected: std::io::Result<()>,
    local: std::io::Result<SocketAddr>,
) -> Result<IpAddr, String> {
    connected.map_err(|e| format!("外へ出る経路が無い: {e}"))?;
    let ip = local
        .map(|a| a.ip())
        .map_err(|e| format!("経路の自分のアドレスが分からない: {e}"))?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err(format!(
            "外へ出る経路の自分のアドレス {ip} は使えない（経路が無い）"
        ));
    }
    Ok(ip)
}

/// ini の `bind` から、開くソケットのアドレスを決める（`auto:ポート` なら経路表を引く。戻りの文は `auto` から何に決めたか、手で書いた
/// アドレスなら空）。読み取りのスレッドで開くときにだけ。
pub fn resolve_bind(s: &NetSettings) -> Result<(SocketAddr, String), String> {
    s.bind.resolve_by(s.scope, || {
        route_or_adapters(probe_route_ip(), adapters_win::list)
    })
}

/// 経路が引けたらその送り元（今までの動き）。引けなかったときだけ、アダプターの一覧から LAN のアドレスを選ぶ（ルーターなしの直結など、
/// デフォルト経路の無い PC のため）。選べなければ、経路が引けなかった理由と一覧で選べなかった理由を並べて返す。
fn route_or_adapters(
    route: Result<IpAddr, String>,
    adapters: impl FnOnce() -> Result<Vec<Adapter>, String>,
) -> Result<(IpAddr, AddrSource), String> {
    let route_err = match route {
        Ok(ip) => return Ok((ip, AddrSource::Route)),
        Err(e) => e,
    };
    let list = adapters().map_err(|e| format!("{route_err}。{e}"))?;
    adapter_pick::choose(&list)
        .map(|ip| (IpAddr::V4(ip), AddrSource::Adapters))
        .map_err(|r| format!("{route_err}。{}", r.text()))
}

/// IPv6 の経路を引くための文書用の宛先（RFC 3849 の 2001:db8::/32。実在の相手ではない）。`ROUTE_PROBE` の IPv6 バージョン。
const ROUTE_PROBE_V6: SocketAddr = SocketAddr::new(
    IpAddr::V6(Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 1)),
    9,
);

/// 外へ出る経路の自分の IPv6 のアドレス（OS の既定の送り元）を、OS の経路表から引く。`[::]:0` のソケットを作って `ROUTE_PROBE_V6` へ
/// `connect` するだけで、1 通も送らない。OS が決めた送り元をそのまま使うので、Windows の既定どおり一時アドレス（RFC 4941。一定の
/// 時間で替わる）が選ばれる。固定のアドレス（EUI-64・安定プライバシー）を自分で選ばないのは、相手に見せるアドレスが長く変わらない値だと
/// 同じアドレスで別の機会にも辿れるため。接続コードの有効期限は 30 分で、一時アドレスの有効期間より十分短い（実測: 有効期間の最大値
/// 7d・優先する有効期間の最大値 1d）。接続のたびにソケットを開き直し、そのとき引き直すので、替わったアドレスを持ち越さない。
/// 経路が無ければ誤りの文を返す。読み取りのスレッドで開くときにだけ呼ぶ。
pub fn probe_route_ipv6() -> Result<IpAddr, String> {
    let sock = UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
        .map_err(|e| format!("IPv6 の経路を引くソケットを作れない: {e}"))?;
    let connected = sock.connect(ROUTE_PROBE_V6);
    judge_route(connected, sock.local_addr())
}

/// 経路を引いた結果から、IPv6 の経路のソケットに使うアドレスを決める（pure）。世界に通じる 2000::/3（`candidate::is_global_unicast_v6`）
/// だけを採り、IPv4・リンクローカル・ユニークローカル・Teredo・6to4・文書用は Err（呼び手は IPv6 の経路を開かずに IPv4 だけで続ける）。
fn choose_v6_source(route: Result<IpAddr, String>) -> Result<Ipv6Addr, String> {
    match route? {
        IpAddr::V6(ip) if is_global_unicast_v6(ip) => Ok(ip),
        IpAddr::V6(ip) => Err(format!(
            "OS が選んだ送り元 {ip} は世界に通じる IPv6（2000::/3。Teredo・6to4・文書用を除く）でない"
        )),
        IpAddr::V4(ip) => Err(format!(
            "外へ出る IPv6 の経路の送り元が IPv4 のアドレス {ip} になった（IPv6 の経路が無い）"
        )),
    }
}

/// IPv6 の経路を開く（読み取りのスレッドで、ネットワークを動かす前に 1 回）。開いたときのソケットのアドレスを返す（自分のコードの候補に載せる）。
/// None は開かなかった（IPv4 だけで続ける。理由は `note` に足す。`scope` が internet でないときは理由も書かない）。
///
/// 開くアドレスは OS の既定の送り元（`probe_route_ipv6`）。ポートは IPv4 のソケットと同じ番号を先に試し、使えなければ OS に選ばせる
/// （ファイアウォールの許可やルーターのポート開放が同じ番号で済む）。
pub fn open_ipv6(s: &NetSettings, net: &GameNet, note: &mut String) -> Option<SocketAddr> {
    open_ipv6_with(s, net, note, probe_route_ipv6)
}

/// `open_ipv6` の中身（経路を引く道具を渡す。検査は OS の経路表を引かずにアドレスを注入する）。
fn open_ipv6_with(
    s: &NetSettings,
    net: &GameNet,
    note: &mut String,
    route: impl FnOnce() -> Result<IpAddr, String>,
) -> Option<SocketAddr> {
    if s.scope != Scope::Internet {
        return None;
    }
    if !s.ipv6 {
        note.push_str(" / IPv6 の経路は使わない（ini の ipv6 = off）");
        return None;
    }
    let ip = match choose_v6_source(route()) {
        Ok(ip) => ip,
        Err(e) => {
            note.push_str(&format!(" / IPv6 の経路は使わない: {e}"));
            return None;
        }
    };
    open_ipv6_on(net, ip, note)
}

/// IPv6 のソケットをアドレス `ip` で開く（ポートの決め方は `open_ipv6` の doc。検査は `::1` を渡す）。
fn open_ipv6_on(net: &GameNet, ip: Ipv6Addr, note: &mut String) -> Option<SocketAddr> {
    let want_port = net.local_addr().map_or(0, |a| a.port());
    let ports: &[u16] = if want_port == 0 {
        &[0]
    } else {
        &[want_port, 0]
    };
    let mut why = String::new();
    for &port in ports {
        match net.open_ipv6(SocketAddr::new(IpAddr::V6(ip), port)) {
            Ok(bound) => {
                note.push_str(&format!(
                    " / IPv6 の経路: 受信ソケット {bound} を開いた（OS の既定の送り元。自分のコードに載せる）"
                ));
                return Some(bound);
            }
            Err(e) => why = e,
        }
    }
    note.push_str(&format!(" / IPv6 の経路は使わない: {why}"));
    None
}

#[derive(Debug, PartialEq, Eq)]
enum Stalled {
    /// スレッドを起こせなかった（仕事は始まってもいない。文は OS の返した理由）。
    NotStarted(String),
    /// 上限の時間内に終わらなかった。
    TimedOut,
    /// スレッドが結果を送る前に終わった（仕事の中の panic）。
    Died,
}

/// `f` を別のスレッドで走らせ、`timeout` 内に終われば結果を返す（終わらなければ `Stalled`。走っているスレッドは
/// 放っておく — OS の名前引きは中断できず、終われば結果を捨てて自分で終わる）。
fn run_with_timeout<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    timeout: Duration,
) -> Result<T, Stalled> {
    run_with_timeout_via(
        |job| {
            std::thread::Builder::new()
                .name("higanr-dns".into())
                .spawn(job)
                .map(drop)
        },
        f,
        timeout,
    )
}

/// `run_with_timeout` の中身（スレッドを起こす道具 `spawn` を渡す）。
fn run_with_timeout_via<T: Send + 'static>(
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    f: impl FnOnce() -> T + Send + 'static,
    timeout: Duration,
) -> Result<T, Stalled> {
    let (tx, rx) = std::sync::mpsc::channel();
    spawn(Box::new(move || {
        let _ = tx.send(f());
    }))
    .map_err(|e| Stalled::NotStarted(e.to_string()))?;
    rx.recv_timeout(timeout).map_err(|e| match e {
        std::sync::mpsc::RecvTimeoutError::Timeout => Stalled::TimedOut,
        std::sync::mpsc::RecvTimeoutError::Disconnected => Stalled::Died,
    })
}

/// 名前引きが結果を返さなかったときの誤りの文（pure）。打ち切ったのと、スレッドを起こせなかったのと、仕事が途中で
/// 終わったのを取り違えない（起こせなかったのに「打ち切った」と書くと、原因を DNS の遅さと読んで的を外す）。
fn lookup_stalled_text(name: &str, timeout: Duration, why: &Stalled) -> String {
    match why {
        Stalled::NotStarted(e) => {
            format!("名前 {name} を引くスレッドを起こせない（引いていない）: {e}")
        }
        Stalled::TimedOut => format!(
            "名前 {name} を {} 秒で引けない（打ち切った）",
            timeout.as_secs_f32()
        ),
        Stalled::Died => format!("名前 {name} を引くスレッドが答えを返さずに終わった"),
    }
}

/// 名前を引く（OS の名前引き。`ToSocketAddrs`）。別のスレッドで `timeout` まで待ち、打ち切ったら誤りの文を返す。
/// **読み取りのスレッドだけで呼ぶ**（ゲームのスレッドでは絶対に引かない）。
pub fn lookup_with_timeout(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<Vec<SocketAddr>, String> {
    let host = host.to_string();
    let name = host.clone();
    run_with_timeout(move || lookup_blocking(&host, port), timeout)
        .unwrap_or_else(|why| Err(lookup_stalled_text(&name, timeout, &why)))
}

/// 名前を引く（OS の名前引き。上限なしで待つ）。呼ぶスレッドが待たされてよいときだけ: `lookup_with_timeout` の別のスレッドと、
/// 2 か所目の STUN の名前引きのスレッド（引き終えなくても誰も待たない）。
fn lookup_blocking(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    (host, port)
        .to_socket_addrs()
        .map(|it| it.collect::<Vec<_>>())
        .map_err(|e| format!("名前 {host} を引けない: {e}"))
}

/// STUN の問い合わせ先をアドレスにする。名前は `lookup` で引き、引けたアドレスのうちソケットと同じ族（今は IPv4 だけ）で `guard::check_peer`
/// （ネットワークの範囲）を通る最初の 1 つを使う。Err は「STUN を使わない」理由（STUN なしで続ける）。範囲 loopback では `localhost` 以外の名前は
/// 引かない（DNS の問い合わせは外への通信）。
pub fn resolve_stun(
    target: &StunTarget,
    scope: Scope,
    want_v4: bool,
    lookup: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
) -> Result<SocketAddr, String> {
    let (host, port) = match target {
        StunTarget::Addr(a) => return Ok(*a),
        StunTarget::Name { host, port } => (host.as_str(), *port),
    };
    if scope == Scope::Loopback && !guard::is_localhost_name(host) {
        return Err(format!(
            "scope = loopback では localhost 以外の名前 {host} を引かない（DNS の問い合わせは外への通信。lan / internet で使う）"
        ));
    }
    let found = lookup(host, port)?;
    found
        .iter()
        .copied()
        .find(|a| a.is_ipv4() == want_v4 && guard::check_peer(*a, scope).is_ok())
        .ok_or_else(|| {
            format!(
                "名前 {host} を引いたアドレス {found:?} に、受信ソケットと同じ族（{}）で scope = {} の中のものが無い",
                if want_v4 { "IPv4" } else { "IPv6" },
                scope.name()
            )
        })
}

/// ini の `stun` があれば問い合わせ、見えた自分のアドレスを返す（答えの有無は `note` に足す。答えなくてもネットワークは開く。名前が引けなくても、
/// 引けたアドレスが範囲の外でも、STUN なしで続ける）。読み取りのスレッドで、ネットワークを動かす前だけ。
///
/// 戻りの 2 つ目は結果の種類（試験のまとめの「STUN の答え」の行）。
pub fn stun_state(
    s: &NetSettings,
    net: &mut GameNet,
    note: &mut String,
) -> (Option<SocketAddr>, Stun) {
    stun_state_inner(
        s,
        net,
        note,
        |h, p| lookup_with_timeout(h, p, DNS_TIMEOUT),
        Duration::from_millis(u64::from(stun::KEEP_INTERVAL_MS)),
        lookup_blocking,
    )
}

/// 待っている間に STUN へ聞き直す範囲か。外の相手と繋ぐ範囲（internet）だけ。家の中（lan）の相手とはルーターの対応づけを通らず、
/// 通っても STUN のサーバーが範囲の外で問い合わせない。同じ PC の中（loopback）は STUN を使うのが検査の偽のサーバーだけで、聞き直しを
/// 送ると検査の数が揺れる。
pub fn keeps_mapping_fresh(scope: Scope) -> bool {
    scope == Scope::Internet
}

/// 2 か所目の STUN（`stun2`）へ聞く範囲か。聞き直しと同じガード: internet だけ。lan では STUN のサーバーが範囲の外になり、loopback では
/// 外へ出ない約束なので、2 か所目へは名前も引かず 1 通も送らない。
pub fn asks_second_stun(scope: Scope) -> bool {
    scope == Scope::Internet
}

/// UPnP を頼む範囲か。外の相手と繋ぐ範囲（internet）だけ（lan / loopback の相手とはルーターの割り当てが要らない）。
pub fn asks_upnp(scope: Scope) -> bool {
    scope == Scope::Internet
}

/// 2 か所目の STUN の結果を待つ上限。出なければ型を「不明」にして先へ進む。
pub const SECOND_STUN_WAIT: Duration = Duration::from_secs(1);

/// 開いたソケットのポートの割り当てを、家のルーターへ UPnP で頼み始める（読み取りのスレッドで、ネットワークを動かす前に 1 回。STUN と並行に
/// 走らせるため、STUN の問い合わせより前に呼ぶ）。頼まない理由があれば `note` に書く（scope が internet でないときは書かない）。
/// 戻りは頼み始めたか。
pub fn upnp_begin(s: &NetSettings, net: &GameNet, note: &mut String) -> bool {
    if !asks_upnp(s.scope) {
        return false;
    }
    if !s.upnp {
        note.push_str(" / UPnP は使わない（ini の upnp = off）");
        return false;
    }
    let Some(local) = net.local_addr().and_then(upnp::lan_source) else {
        note.push_str(" / UPnP は使わない（受信ソケットが家の中の IPv4 のアドレスでない）");
        return false;
    };
    let mut seed = [0u8; 2 * upnp::ALT_PORTS];
    if !os_random(&mut seed) {
        note.push_str(" / UPnP は使わない（OS の乱数を引けない）");
        return false;
    }
    upnp_begin_with(
        net,
        Box::new(upnp::NetWire),
        local,
        upnp::port_order(local.port(), seed),
        upnp::RENEW_EVERY,
        note,
    )
}

/// `upnp_begin` の中身（通信の道具・順に試す外側のポート・頼み直す間隔を渡す。検査は偽のルーターを渡す）。
pub fn upnp_begin_with(
    net: &GameNet,
    wire: Box<dyn upnp::Wire>,
    local: std::net::SocketAddrV4,
    ports: Vec<u16>,
    renew: Duration,
    note: &mut String,
) -> bool {
    match upnp::Handle::spawn(wire, local, ports, upnp::LEASE_SECS, renew) {
        Ok(h) => {
            if !net.upnp_attach(h) {
                note.push_str(" / UPnP: もう頼んである");
                return false;
            }
            note.push_str(&format!(
                " / UPnP: ルーターへ受信ソケットのポートの割り当てを頼み始めた（期限 {} 秒。上限 {} 秒で諦める）",
                upnp::LEASE_SECS,
                upnp::OPEN_BUDGET.as_secs()
            ));
            true
        }
        Err(e) => {
            note.push_str(&format!(" / UPnP は使わない（スレッドを起こせない: {e}）"));
            false
        }
    }
}

/// 開くときに決まった自分の回線の情報（`nat_settle` の戻り）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OwnNat {
    /// 自分のコードに載せる型の印。
    pub mark: NatMark,
    /// UPnP で割り当てた外側のポート（自分の候補に載せる）。
    pub upnp_port: Option<u16>,
}

/// 2 か所目の STUN の結果と UPnP の結果が出るのを待ち、自分の型の印を決める（読み取りのスレッドで、STUN の問い合わせの後・
/// 自分のコードを作る前に 1 回）。2 か所目の STUN は `SECOND_STUN_WAIT`、UPnP は頼み始めてから `upnp::OPEN_BUDGET` までで、
/// 出なければ型は「不明」・UPnP は「使えない」にして先へ進む。待った時間は `note`（本ログ）に書く。
///
/// UPnP を使うのは、ルーターの外側のアドレスが `mapped`（1 か所目の STUN で見えた自分のアドレス）と同じ IPv4 で、世界に通じるときだけ。
/// 違えば（二重 NAT など）割り当てを消し、印も候補も付けない。`mapped` が IPv4 のこのときだけ候補が足されるので、印は候補と同じ条件。
pub fn nat_settle(
    s: &NetSettings,
    net: &GameNet,
    mapped: Option<SocketAddr>,
    note: &mut String,
) -> OwnNat {
    let t0 = Instant::now();
    let mut second = net.second_stun();
    if second == SecondStun::Pending {
        while second == SecondStun::Pending && t0.elapsed() < SECOND_STUN_WAIT {
            std::thread::sleep(Duration::from_millis(5));
            second = net.second_stun();
        }
        note.push_str(&format!(
            " / 2 か所目の STUN の結果を {} ms 待った（{}）",
            t0.elapsed().as_millis(),
            if second == SecondStun::Pending {
                "上限に来たので型は不明のまま進む"
            } else {
                "結果が出た"
            }
        ));
    }
    let t1 = Instant::now();
    let upnp_port = net.upnp_outcome(upnp::OPEN_BUDGET).and_then(|o| {
        note.push_str(&format!(
            " / UPnP の結果を {} ms 待った",
            t1.elapsed().as_millis()
        ));
        let upnp::Outcome::Mapped { external, outer_ip } = o else {
            return None;
        };
        match higanr_core::upnp::check_outer(mapped, outer_ip) {
            OuterCheck::Match => Some(external),
            OuterCheck::Behind => {
                net.upnp_discard(upnp::State::OuterBehind);
                None
            }
            OuterCheck::NoStun | OuterCheck::Unknown => {
                net.upnp_discard(upnp::State::OuterUnchecked);
                None
            }
        }
    });
    if let Some(p) = upnp_port {
        net.shared.diag.upnp_mapped.store(true, Ordering::Relaxed);
        let st = net
            .shared
            .upnp
            .get()
            .map(upnp::Handle::stats)
            .unwrap_or_default();
        let how = match st.lease {
            _ if st.already_open => "もう開いている割り当てがあった（足していない）",
            upnp::Lease::Timed => "割り当てた",
            upnp::Lease::Permanent => "割り当てた（期限なしで頼み直した）",
            upnp::Lease::Inherited => "割り当てた（前の起動の期限なしの割り当てを引き継いだ）",
        };
        note.push_str(&format!(
            " / UPnP: 外側のポート {p} を{how}。自分の候補に載せる"
        ));
    } else if asks_upnp(s.scope) && s.upnp {
        note.push_str(&format!(
            " / {}",
            net.shared
                .upnp
                .get()
                .map(upnp::Handle::stats)
                .unwrap_or_default()
                .text()
        ));
    }
    let mark = NatMark {
        symmetry: Symmetry::from_depends(second.depends_on_destination()),
        upnp: upnp_port.is_some(),
    };
    note.push_str(&format!(" / 自分の回線の型の印: {}", mark.text()));
    OwnNat { mark, upnp_port }
}

#[cfg(test)]
pub fn stun_mapped(s: &NetSettings, net: &mut GameNet, note: &mut String) -> Option<SocketAddr> {
    stun_state(s, net, note).0
}

#[cfg(test)]
fn stun_state_with(
    s: &NetSettings,
    net: &mut GameNet,
    note: &mut String,
    lookup: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
    keep_every: Duration,
) -> (Option<SocketAddr>, Stun) {
    stun_state_inner(s, net, note, lookup, keep_every, lookup_blocking)
}

#[cfg(test)]
fn stun_mapped_with(
    s: &NetSettings,
    net: &mut GameNet,
    note: &mut String,
    lookup: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
) -> Option<SocketAddr> {
    stun_state_with(s, net, note, lookup, KEEP_OFF).0
}

/// 検査の「聞き直しの間隔」: 十分長くして、検査の間に 1 通も送らせない。
#[cfg(test)]
const KEEP_OFF: Duration = Duration::from_secs(3_600);

/// `stun_state` の中身（1 か所目の名前を引く道具・聞き直しの間隔・2 か所目の名前を引く道具を渡す。検査は実在の名前を引かずに、
/// 引けたアドレスを注入し、間隔を短くする）。
fn stun_state_inner(
    s: &NetSettings,
    net: &mut GameNet,
    note: &mut String,
    lookup: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
    keep_every: Duration,
    lookup2: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String> + Send + 'static,
) -> (Option<SocketAddr>, Stun) {
    let Some(target) = s.stun.as_ref() else {
        return (None, Stun::NotUsed);
    };
    let want_v4 = net.local_addr().is_none_or(|a| a.is_ipv4());
    let server = match resolve_stun(target, s.scope, want_v4, lookup) {
        Ok(a) => a,
        Err(e) => {
            note.push_str(&format!(" / STUN は使わない: {e}"));
            return (None, Stun::Skipped(e));
        }
    };
    if let StunTarget::Name { host, port } = target {
        note.push_str(&format!(" / STUN {host}:{port} を引いて {server} を使う"));
    }
    match net.stun_query(server) {
        Ok(a) => {
            note.push_str(&format!(" / STUN {server} から見えた自分のアドレス {a}"));
            net.note_stun_answer(a);
            if keeps_mapping_fresh(s.scope) && net.keep_stun_mapping(server, a, keep_every) {
                note.push_str(&format!(
                    " / 繋がるまで {} 秒ごとに同じ STUN へ聞き直して、外側のアドレスを保つ",
                    keep_every.as_secs_f32()
                ));
            }
            if asks_second_stun(s.scope)
                && let Some(t2) = s.stun2.as_ref()
                && net.ask_second_stun(t2.clone(), server, a, lookup2)
            {
                note.push_str(&format!(
                    " / 2 か所目の STUN {} へも並行に聞く（外側のアドレスが宛先ごとに変わる型かを見分ける診断だけ。答えを待たない）",
                    t2.text()
                ));
            }
            (Some(a), Stun::Seen)
        }
        Err(e) => {
            note.push_str(&format!(" / STUN: {e}"));
            // 1 か所目が答えないときは 2 か所目に同じように聞き、答えをコードに載せる（1 か所目だけが答えない回線があった）。宛先ごとの
            // 型の比べは、比べる相手が無いのでしない（診断は「聞いていない」のまま）。待ちが延びるのは 1 か所目が答えないときだけ
            // （名前引き 2 秒 ＋ 問い合わせ 2 秒まで）。
            if asks_second_stun(s.scope)
                && let Some(t2) = s.stun2.as_ref()
            {
                let lookup2 = move |h: &str, p: u16| {
                    let host = h.to_string();
                    run_with_timeout(move || lookup2(&host, p), DNS_TIMEOUT)
                        .unwrap_or_else(|why| Err(lookup_stalled_text(h, DNS_TIMEOUT, &why)))
                };
                let got = resolve_stun(t2, s.scope, want_v4, lookup2)
                    .and_then(|server2| net.stun_query(server2).map(|a| (server2, a)));
                match got {
                    Ok((server2, a)) => {
                        note.push_str(&format!(
                            " / 1 か所目が答えないので 2 か所目の STUN {}（{server2}）に聞き、見えた自分のアドレス {a} を使う",
                            t2.text()
                        ));
                        net.note_stun_answer(a);
                        if keeps_mapping_fresh(s.scope)
                            && net.keep_stun_mapping(server2, a, keep_every)
                        {
                            note.push_str(&format!(
                                " / 繋がるまで {} 秒ごとに 2 か所目の STUN へ聞き直して、外側のアドレスを保つ",
                                keep_every.as_secs_f32()
                            ));
                        }
                        return (Some(a), Stun::Seen);
                    }
                    Err(e2) => note.push_str(&format!(" / 2 か所目の STUN も使えない: {e2}")),
                }
            }
            (None, Stun::NoAnswer(e))
        }
    }
}

/// 接続の秘密を OS の乱数で引く（core は乱数を持たない）。
pub fn draw_secret() -> Result<Secret, String> {
    let mut b = [0u8; SECRET_LEN];
    if !os_random(&mut b) {
        return Err("接続の秘密を OS の乱数で引けない".to_string());
    }
    Ok(Secret(b))
}

/// 使い捨ての鍵の種を OS の乱数で引く（core は乱数を持たない）。繋がりのセッションの設定・観戦の配る側・観戦者を作るたびに 1 回（同じ種を
/// 2 つのセッションに使い回さない）。ゲームのスレッドでは呼ばない。
pub fn draw_key_seed() -> Result<KeySeed, String> {
    let mut b = [0u8; SEED_LEN];
    if !os_random(&mut b) {
        return Err("使い捨ての鍵の種を OS の乱数で引けない".to_string());
    }
    Ok(KeySeed(b))
}

/// 今の UNIX 秒（接続コードの有効期限。時計が 1970 年より前なら 0）。
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Net for GameNet {
    fn now_us(&mut self) -> u64 {
        self.shared.t0.elapsed().as_micros() as u64
    }

    fn recv(&mut self, buf: &mut [u8; MAX_DATAGRAM]) -> Option<(SocketAddr, usize, u64)> {
        self.shared
            .ring
            .pop(buf)
            .or_else(|| self.shared.ring6.pop(buf))
    }

    fn wait(&mut self, us: u32) -> bool {
        self.waiter.wait(us) == Woke::TimedOut
    }

    fn recv_marked(
        &mut self,
        buf: &mut [u8; MAX_DATAGRAM],
    ) -> Option<(SocketAddr, usize, u64, bool)> {
        self.shared
            .ring
            .pop_marked(buf)
            .or_else(|| self.shared.ring6.pop_marked(buf))
    }

    fn pong_install(&mut self, r: &PongResponder) -> bool {
        let slot = &self.shared.pong;
        let mut g = match slot.responder.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        *g = Some(r.clone());
        let next = slot
            .generation
            .load(Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        slot.generation.store(next, Ordering::Release);
        true
    }

    fn pong_enable(&mut self, on: bool) {
        self.shared.pong.on.store(on, Ordering::Release);
    }

    fn heartbeat(&mut self, now_ms: u32) {
        self.shared
            .pong
            .beat
            .store(now_ms.max(1), Ordering::Release);
    }

    fn link_up(&mut self) {
        self.shared.keep.run.store(false, Ordering::Release);
    }

    fn own_port_varies(&mut self) -> Option<bool> {
        self.second_stun().depends_on_destination()
    }

    fn own_nat(&mut self) -> NatMark {
        NatMark {
            symmetry: Symmetry::from_depends(self.second_stun().depends_on_destination()),
            upnp: self.shared.diag.upnp_mapped.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn lo(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// ソケットを使う検査を 1 本ずつ走らせる錠。`connection_reset_is_skipped_*` は閉じたばかりのポートへわざと送るので、同じ時に走る
    /// 別の検査がそのポートを引き当てると、余計な 3 通を受ける。
    fn socket_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn gate_refuses_non_loopback_before_binding() {
        let any = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let lan = SocketAddr::from(([192, 168, 1, 2], 40_000));
        let v6_any = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 40_000);
        assert!(
            open(any, &[lo(40_000)]).is_err(),
            "受信ソケット 0.0.0.0 を通した"
        );
        assert!(open(lo(0), &[lan]).is_err(), "LAN の宛先を通した");
        assert!(open(lo(0), &[v6_any]).is_err(), "宛先 :: を通した");
        assert!(open(lo(0), &[lo(40_000)]).is_ok());
        assert!(
            open(lo(0), &[lo(40_000), lan]).is_err(),
            "候補の 2 つ目の LAN を通した"
        );
        assert!(open(lo(0), &[]).is_err(), "候補が空なのに開いた");
        assert!(open_for_code(any).is_err(), "受信ソケット 0.0.0.0 を通した");
        assert!(open_for_code(lo(0)).is_ok());
    }

    /// 壊れたら落ちる: VPN の範囲で開いた受信ソケットが VPN の印を持たない、範囲 lan の受信ソケットに VPN の印が付く。
    #[test]
    fn only_a_vpn_scope_socket_says_it_is_vpn() {
        let _g = socket_lock();
        let net = higanr_core::vpn_net::Net4::new(Ipv4Addr::new(127, 0, 0, 4), 30).unwrap();
        let peer = SocketAddr::from(([127, 0, 0, 2], 40_000));
        let (vpn, _t1) = open_scoped(Scope::LanVpn(net), lo(0), &[peer]).unwrap();
        assert!(vpn.is_vpn(), "VPN の範囲で開いたのに印が無い");
        let (plain, _t2) = open_scoped(Scope::Lan, lo(0), &[peer]).unwrap();
        assert!(!plain.is_vpn(), "範囲 lan に VPN の印が付いた");
    }

    #[test]
    fn gate_follows_the_scope_it_was_opened_with() {
        let _g = socket_lock();
        let lan = SocketAddr::from(([192, 168, 1, 2], 40_000));
        let lan6: SocketAddr = "[fd00::2]:40000".parse().unwrap();
        let wan = SocketAddr::from(([203, 0, 113, 2], 40_000));
        let wan6: SocketAddr = "[2001:db8::2]:40000".parse().unwrap();
        let mcast = SocketAddr::from(([239, 1, 2, 3], 40_000));
        let any = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        assert!(open_scoped(Scope::Loopback, lo(0), &[lan]).is_err());
        assert!(open_scoped(Scope::Lan, lo(0), &[lo(40_000), lan, lan6]).is_ok());
        assert!(open_scoped(Scope::Lan, lo(0), &[lan, wan]).is_err());
        assert!(open_scoped(Scope::Internet, lo(0), &[lan, wan, wan6]).is_ok());
        for scope in [Scope::Loopback, Scope::Lan, Scope::Internet] {
            assert!(open_scoped(scope, lo(0), &[mcast]).is_err(), "{scope:?}");
            assert!(open_scoped(scope, any, &[lo(40_000)]).is_err(), "{scope:?}");
            assert!(open_for_code_scoped(scope, any).is_err(), "{scope:?}");
        }
        for (scope, want) in [
            (Scope::Loopback, [true, false, false, false]),
            (Scope::Lan, [true, true, false, false]),
            (Scope::Internet, [true, true, true, false]),
        ] {
            let (mut net, _th) = open_for_code_scoped(scope, lo(0)).unwrap();
            let got = [lo(40_000), lan, wan, mcast].map(|a| net.may_send(a));
            assert_eq!(got, want, "{scope:?}");
            assert!(!net.may_send(lo(0)), "{scope:?}: ポート 0");
            let outside = match scope {
                Scope::Loopback => lan,
                Scope::Lan => wan,
                Scope::Internet => mcast,
                Scope::LanVpn(_) => unreachable!("この表に LanVpn は無い"),
            };
            let e = net.stun_query(outside).unwrap_err();
            let named = format!("scope = {}", scope.name());
            assert!(
                e.contains(&named)
                    || (scope == Scope::Internet && e.contains("どのネットワークの範囲でも")),
                "{scope:?}: {e}"
            );
            net.send(outside, b"x");
            assert_eq!((net.send_counts(), net.send_refused()), ((0, 0), 1));
        }
    }

    /// 壊れたら落ちる: VPN のネットワークを足した範囲で開いたソケットが、そのネットワークの中の相手を断る（候補の検査・送る直前のガードの両方）、
    /// ネットワークの外（隣の /8・ネットワークのアドレスとブロードキャスト・世界のアドレス）を通す、lan の分（私設・loopback）を落とす、
    /// 送る直前のガードの拒否が数えられず外へ出る。開く範囲が lan のソケットは VPN のアドレスを今までどおり断る。
    /// 通るアドレスへは送らない（`may_send` は判定だけ。26/8 は実在する世界のアドレスで、検査から外へ出さない）。
    #[test]
    fn a_lan_vpn_scope_passes_its_network_on_every_gate_and_a_lan_scope_does_not() {
        let _g = socket_lock();
        let radmin =
            Scope::LanVpn(higanr_core::vpn_net::Net4::new([26, 1, 2, 3].into(), 8).unwrap());
        let at = |a: [u8; 4]| SocketAddr::from((a, 40_000));
        let (vpn, vpn_lo, vpn_hi) = (
            at([26, 12, 34, 56]),
            at([26, 0, 0, 0]),
            at([26, 255, 255, 255]),
        );
        let (next, lan, wan, mcast) = (
            at([27, 0, 0, 1]),
            at([192, 168, 1, 2]),
            at([203, 0, 113, 2]),
            at([239, 1, 2, 3]),
        );
        // 開くときの宛先の検査。
        assert!(open_scoped(radmin, lo(0), &[lo(40_000), lan, vpn]).is_ok());
        for bad in [vpn_lo, vpn_hi, next, wan, mcast] {
            assert!(open_scoped(radmin, lo(0), &[vpn, bad]).is_err(), "{bad}");
        }
        assert!(
            open_scoped(Scope::Lan, lo(0), &[vpn]).is_err(),
            "lan が VPN の宛先を通した"
        );
        // 受信ソケットのアドレスの検査（この PC に 26.x は無いので OS の bind までは行かない。ガードだけ見る）。
        assert!(guard::check_bind(vpn, radmin, "受信ソケット").is_ok());
        assert!(guard::check_bind(vpn, Scope::Lan, "受信ソケット").is_err());
        assert!(guard::check_bind(vpn_lo, radmin, "受信ソケット").is_err());
        // 送る直前のガード。
        let (mut net, _th) = open_for_code_scoped(radmin, lo(0)).unwrap();
        let got = [lo(40_000), lan, vpn, vpn_lo, vpn_hi, next, wan, mcast].map(|a| net.may_send(a));
        assert_eq!(got, [true, true, true, false, false, false, false, false]);
        assert!(
            !net.may_send(SocketAddr::from(([26, 12, 34, 56], 0))),
            "ポート 0"
        );
        for refused in [vpn_lo, vpn_hi, next, wan] {
            net.send(refused, b"x");
        }
        assert_eq!((net.send_counts(), net.send_refused()), ((0, 0), 4));
        let (mut lan_net, _th2) = open_for_code_scoped(Scope::Lan, lo(0)).unwrap();
        assert!(
            !lan_net.may_send(vpn),
            "lan のソケットが VPN の宛先を通した"
        );
        lan_net.send(vpn, b"x");
        assert_eq!((lan_net.send_counts(), lan_net.send_refused()), ((0, 0), 1));
        // 受信スレッドの返信のガード（`answer` の検査は `pong_gate_tests`）が使う範囲も、開いた範囲そのもの。
        let (_net, th) = open_for_code_scoped(radmin, lo(0)).unwrap();
        assert_eq!(th.shared.scope, radmin);
    }

    #[test]
    fn datagrams_reach_the_ring_and_wake_the_waiter() {
        let _g = socket_lock();
        let peer = UdpSocket::bind(lo(0)).unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (mut net, th) = open(lo(0), &[peer_addr]).unwrap();
        let me = net.local_addr().unwrap();
        let t0 = Instant::now();
        peer.send_to(b"HGRT-test", me).unwrap();
        net.wait(1_000_000);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "イベントで起きない"
        );
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut got = None;
        for _ in 0..100 {
            if let Some(x) = net.recv(&mut buf) {
                got = Some(x);
                break;
            }
            net.wait(2_000);
        }
        let (from, n, t_us) = got.expect("リングに入らない");
        assert_eq!((from, &buf[..n]), (peer_addr, &b"HGRT-test"[..]));
        assert!(t_us <= net.now_us(), "受けた時刻が今より後: {t_us}");
        peer.send_to(&[0u8; 1500], me).unwrap();
        net.send(peer_addr, b"back");
        let mut rb = [0u8; 16];
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (n, from) = peer.recv_from(&mut rb).unwrap();
        assert_eq!((&rb[..n], from), (&b"back"[..], me));
        let deadline = Instant::now() + Duration::from_secs(2);
        while th.stats().ring.dropped_big == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let s = th.stop();
        assert_eq!((s.ring.pushed, s.ring.dropped_big), (1, 1));
        assert_eq!(net.send_counts(), (1, 0));
    }

    #[test]
    fn connection_reset_is_skipped_and_the_thread_stops() {
        let _g = socket_lock();
        let gone = UdpSocket::bind(lo(0)).unwrap();
        let gone_addr = gone.local_addr().unwrap();
        drop(gone);
        let (mut net, th) = open(lo(0), &[gone_addr]).unwrap();
        let me = net.local_addr().unwrap();
        for _ in 0..3 {
            net.send(gone_addr, b"x");
        }
        let other = UdpSocket::bind(lo(0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut got = false;
        while !got && Instant::now() < deadline {
            other.send_to(b"alive", me).unwrap();
            net.wait(5_000);
            got = net.recv(&mut buf).is_some();
        }
        assert!(got, "WSAECONNRESET の後に受けられない");
        let t0 = Instant::now();
        let s = th.stop();
        assert!(t0.elapsed() < Duration::from_secs(1), "止まらない");
        assert_eq!(s.recv_errors, 0);
    }

    #[test]
    fn game_side_calls_never_allocate() {
        let _g = socket_lock();
        let peer = UdpSocket::bind(lo(0)).unwrap();
        let pa = peer.local_addr().unwrap();
        let (mut net, _th) = open(lo(0), &[pa]).unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let dir = higanr_core::seal::DirKeys::from_raw([1; 32], [2; 32]);
        let r = PongResponder::new(
            1,
            higanr_core::auth::AuthKey::derive(&higanr_core::auth::Secret([1; 16]), 1),
            dir.opener(),
            dir.sealer(higanr_core::seal::Sender::Pong),
            higanr_core::candidate::Candidates::from_addrs(&[pa]).unwrap(),
            pa,
        );
        net.send(pa, b"warm");
        let _ = net.recv(&mut buf);
        let _ = net.recv_marked(&mut buf);
        let _ = net.now_us();
        assert!(net.pong_install(&r));
        let before = crate::test_alloc::count();
        net.send(pa, b"hello");
        let _ = net.recv(&mut buf);
        let _ = net.recv_marked(&mut buf);
        let _ = net.now_us();
        net.wait(100);
        assert!(net.pong_install(&r));
        net.pong_enable(true);
        net.pong_enable(false);
        let used = crate::test_alloc::count() - before;
        assert_eq!(used, 0, "ゲームスレッドの口で確保した");
    }

    #[test]
    fn send_refuses_non_loopback_destinations() {
        let _g = socket_lock();
        let peer = UdpSocket::bind(lo(0)).unwrap();
        let pa = peer.local_addr().unwrap();
        let (mut net, _th) = open(lo(0), &[pa]).unwrap();
        net.send(SocketAddr::from(([192, 0, 2, 1], 9)), b"x");
        net.send("[2001:db8::1]:9".parse().unwrap(), b"x");
        assert_eq!((net.send_counts(), net.send_refused()), ((0, 0), 2));
        net.send(pa, b"y");
        assert_eq!((net.send_counts(), net.send_refused()), ((1, 0), 2));
    }

    #[test]
    fn stun_query_gets_own_address_from_a_fake_server() {
        let _g = socket_lock();
        let fake = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let (mut net, _th) = open(lo(0), &[lo(40_000)]).unwrap();
        let me = net.local_addr().unwrap();
        UdpSocket::bind(lo(0))
            .unwrap()
            .send_to(b"HGRT-early", me)
            .unwrap();
        assert_eq!(net.stun_query(fake.addr()), Ok(me));
        assert!(fake.answered() >= 1);
        assert!(
            net.stun_query(SocketAddr::from(([192, 0, 2, 1], 3478)))
                .is_err()
        );
    }

    #[test]
    #[ignore = "本物の UDP と 2 本のスレッドで実時間（1 秒ほど）を待つ。全件の回（build.bat）で --ignored で走る"]
    fn pong_comes_from_the_receive_thread_without_waiting_for_a_tick() {
        use crate::netplay::fake::FakeGame;
        use crate::netplay::{self, NetPlay};
        use higanr_core::auth::{AuthKey, Secret};
        use higanr_core::candidate::Candidates;
        use higanr_core::handshake::{self, Name};
        use higanr_core::link::Link;
        use higanr_core::wire::{ByeReason, Message, Seat};

        const SID: u32 = 0x9090_0001;
        const SECRET: Secret = Secret([0x5A; 16]);
        const TICK: u32 = 0;
        const STALL: u32 = 1;
        const CLOSE: u32 = 2;
        const DONE: u32 = 3;

        let _g = socket_lock();
        let psock = UdpSocket::bind(lo(0)).unwrap();
        let paddr = psock.local_addr().unwrap();
        let (mut net, th) = open(lo(0), &[paddr]).unwrap();
        let daddr = net.local_addr().unwrap();
        let cfg = |nonce: u64, peer: SocketAddr| link::Config {
            handshake: handshake::Config {
                session_id: SID,
                role_nonce: nonce,
                latency: 2,
                name: Name::new("p"),
                host_seat: Seat::P1,
            },
            secret: SECRET,
            peers: Candidates::from_addrs(&[peer]).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        };
        let mut np = NetPlay::new(cfg(0xF000, paddr), netplay::Config::default(), &mut net);
        let mut game = FakeGame::new(1, 60, 1, 1);
        let mode = Arc::new(AtomicU32::new(TICK));
        let seen = Arc::new(AtomicU32::new(u32::MAX));
        let dll_connected = Arc::new(AtomicBool::new(false));

        let peer = {
            let (mode, seen, dll_connected) = (
                Arc::clone(&mode),
                Arc::clone(&seen),
                Arc::clone(&dll_connected),
            );
            std::thread::spawn(move || {
                struct Out<'a>(&'a UdpSocket);
                impl Outlet for Out<'_> {
                    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
                        let _ = self.0.send_to(bytes, to);
                    }
                }
                psock
                    .set_read_timeout(Some(Duration::from_millis(1)))
                    .unwrap();
                let t0 = Instant::now();
                let now = || t0.elapsed().as_millis() as u32;
                let mut buf = [0u8; RECV_BUF];
                let mut link = Link::new(cfg(0x0001, daddr), 0);
                while !(link.connected().is_some() && dll_connected.load(Ordering::Acquire)) {
                    assert!(t0.elapsed() < Duration::from_secs(5), "繋がらない");
                    if let Ok((n, from)) = psock.recv_from(&mut buf) {
                        link.on_datagram(from, &buf[..n], now(), &mut Out(&psock));
                    }
                    link.poll(now(), &mut Out(&psock));
                }
                // 以後はセッションを回さない（PING は自分で作り、返ってきたものは自分で解く）。手で作る PING はセッションの送り（`Sender::Link`）と
                // 番号が重ならないよう、検査だけ別の印（観戦者の印）で封じる。
                let key = AuthKey::derive(&SECRET, SID);
                let keys = link.dir_keys().expect("繋がった");
                let seq = higanr_core::seal::Seq::new();
                let set_mode = |m: u32| {
                    mode.store(m, Ordering::Release);
                    let t = Instant::now();
                    while seen.load(Ordering::Acquire) != m {
                        assert!(
                            t.elapsed() < Duration::from_secs(2),
                            "dll が回し方 {m} を見ない"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                };
                let mut rb = [0u8; RECV_BUF];
                let mut drain = || while psock.recv_from(&mut rb).is_ok() {};
                let send_ping = |t: u32| {
                    let mut out = [0u8; MAX_DATAGRAM];
                    let sealer = keys.sealer(higanr_core::seal::Sender::Spectator);
                    let n = wire::seal(
                        &sealer,
                        &seq,
                        SID,
                        &Message::Ping { send_time: t },
                        &mut out,
                    )
                    .expect("封じられる");
                    psock.send_to(&out[..n], daddr).unwrap();
                };
                let collect = |from: Instant, dur: Duration| {
                    let mut got = Vec::new();
                    let mut b = [0u8; RECV_BUF];
                    while from.elapsed() < dur {
                        if let Ok((n, _)) = psock.recv_from(&mut b)
                            && let Ok(p) = wire::decode(&key, Some(&keys.opener()), &b[..n])
                        {
                            got.push((from.elapsed(), p.msg));
                        }
                    }
                    got
                };
                let pongs = |got: &[(Duration, Message)], t: u32| -> Vec<Duration> {
                    got.iter()
                        .filter(|(_, m)| *m == Message::Pong { send_time: t })
                        .map(|(d, _)| *d)
                        .collect()
                };

                set_mode(STALL);
                drain();
                let t = Instant::now();
                send_ping(0xA1);
                let got_a = collect(t, Duration::from_millis(300));
                let a = pongs(&got_a, 0xA1);
                assert_eq!(a.len(), 1, "(A) tick を止めている間の PONG の数: {got_a:?}");
                assert!(
                    a[0] < Duration::from_millis(100),
                    "(A) PONG が遅い: {:?}",
                    a[0]
                );

                set_mode(TICK);
                let t = Instant::now();
                send_ping(0xA2);
                let got_b = collect(t, Duration::from_millis(300));
                assert_eq!(pongs(&got_b, 0xA2).len(), 1, "(B) PONG の数: {got_b:?}");
                assert_eq!(
                    pongs(&got_b, 0xA1).len(),
                    0,
                    "(B) 止めていた間の PING に箱が重ねた"
                );

                set_mode(CLOSE);
                std::thread::sleep(Duration::from_millis(50));
                set_mode(STALL);
                drain();
                let t = Instant::now();
                send_ping(0xA3);
                let got_c = collect(t, Duration::from_millis(200));
                assert_eq!(
                    pongs(&got_c, 0xA3).len(),
                    0,
                    "(C) 閉じた後に PONG を返した: {got_c:?}"
                );
                set_mode(TICK);
                let got_d = collect(t, Duration::from_millis(400));
                assert_eq!(
                    pongs(&got_d, 0xA3).len(),
                    0,
                    "(C) 閉じた後に PONG: {got_d:?}"
                );
                assert!(
                    got_d
                        .iter()
                        .any(|(_, m)| *m == Message::Bye(ByeReason::Finished)),
                    "(C) 閉じた後の PING に BYE が返らない: {got_d:?}"
                );
                mode.store(DONE, Ordering::Release);
                a[0]
            })
        };

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let m = mode.load(Ordering::Acquire);
            if m == DONE || peer.is_finished() || Instant::now() > deadline {
                break;
            }
            if m != STALL {
                if m == CLOSE {
                    np.close(ByeReason::Finished);
                }
                np.on_input(&mut game, &mut net);
                if np.connected().is_some() {
                    dll_connected.store(true, Ordering::Release);
                }
            }
            seen.store(m, Ordering::Release);
            std::thread::sleep(Duration::from_millis(2));
        }
        let rtt = peer.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
        let s = th.stats();
        let c = np.connected().expect("繋がっている").counts();
        assert!(s.pongs_sent >= 2, "{s:?}");
        assert_eq!((s.pongs_refused, s.pong_send_errors), (0, 0), "{s:?}");
        assert!(c.pong_by_receiver >= 2, "{c:?}");
        eprintln!(
            "tick を止めている間の PONG: {rtt:?} / 受信スレッドの PONG {} 通 / 箱が重ねなかった PING {} 通",
            s.pongs_sent, c.pong_by_receiver
        );
    }

    #[test]
    fn stun_by_name_gets_own_address_from_a_fake_server() {
        let _g = socket_lock();
        let fake = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let text = format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\nstun = localhost:{}\n",
            fake.addr().port()
        );
        let s = crate::net_config::from_text(&text).unwrap().unwrap();
        assert!(
            matches!(s.stun, Some(StunTarget::Name { .. })),
            "{:?}",
            s.stun
        );
        let (mut net, _th) = open(lo(0), &s.peer_addrs()).unwrap();
        let me = net.local_addr().unwrap();
        let mut note = String::new();
        assert_eq!(stun_mapped(&s, &mut net, &mut note), Some(me), "{note}");
        assert!(
            note.contains(&format!(
                "STUN localhost:{} を引いて {}",
                fake.addr().port(),
                fake.addr()
            )) && note.contains("見えた自分のアドレス"),
            "{note}"
        );
        assert!(fake.answered() >= 1);
        let (mapped, state) = stun_state(&s, &mut net, &mut String::new());
        assert_eq!((mapped, state), (Some(me), Stun::Seen));
        let no_stun = crate::net_config::from_text(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            stun_state(&no_stun, &mut net, &mut String::new()),
            (None, Stun::NotUsed)
        );
    }

    #[test]
    fn a_name_that_does_not_resolve_into_the_scope_continues_without_stun() {
        let _g = socket_lock();
        let wan = SocketAddr::from(([203, 0, 113, 1], 3478));
        let text = |scope: &str, name: &str| {
            format!(
                "[netplay]\nenable = 1\nscope = {scope}\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\nstun = {name}:3478\n"
            )
        };
        let s = crate::net_config::from_text(&text("lan", "stun.test.example"))
            .unwrap()
            .unwrap();
        let (mut net, _th) = open_scoped(Scope::Lan, lo(0), &s.peer_addrs()).unwrap();
        type Found = Result<Vec<SocketAddr>, String>;
        let cases: [(Found, &str); 4] = [
            (Ok(vec![wan]), "scope = lan"),
            (Ok(vec!["[fd00::1]:3478".parse().unwrap()]), "IPv4"),
            (
                Err("名前 stun.test.example を引けない: 合成".to_string()),
                "合成",
            ),
            (Ok(vec![]), "無い"),
        ];
        for (found, want) in cases {
            let mut note = String::new();
            assert_eq!(
                stun_mapped_with(&s, &mut net, &mut note, |_, _| found.clone()),
                None
            );
            assert!(
                note.contains("STUN は使わない") && note.contains(want),
                "{note}"
            );
            let (_, state) =
                stun_state_with(&s, &mut net, &mut String::new(), |_, _| found, KEEP_OFF);
            assert!(
                matches!(&state, Stun::Skipped(e) if e.contains(want)),
                "{state:?}"
            );
        }
        assert_eq!((net.send_counts(), net.send_refused()), ((0, 0), 0));
        let s = crate::net_config::from_text(&text("loopback", "stun.cloudflare.com"))
            .unwrap()
            .unwrap();
        let (mut net, _th) = open(lo(0), &s.peer_addrs()).unwrap();
        let mut note = String::new();
        let r = stun_mapped_with(&s, &mut net, &mut note, |_, _| panic!("名前を引いた"));
        assert_eq!(r, None);
        assert!(note.contains("localhost 以外の名前"), "{note}");
        let t0 = Instant::now();
        let (_lc, pnote) = prepare_link(&s, &mut net, 9).unwrap();
        assert!(
            pnote.contains("STUN は使わない") && pnote.contains("接続コード"),
            "{pnote}"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "名前を引いて待った"
        );
    }

    #[test]
    fn resolve_stun_picks_the_first_address_of_the_same_family_inside_the_scope() {
        let name = |h: &str| StunTarget::Name {
            host: h.to_string(),
            port: 3478,
        };
        let sa = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let never = |_: &str, _: u16| -> Result<Vec<SocketAddr>, String> { panic!("引いた") };
        let a = sa("198.51.100.1:3478");
        assert_eq!(
            resolve_stun(&StunTarget::Addr(a), Scope::Lan, true, never),
            Ok(a)
        );
        let list = vec![
            sa("203.0.113.1:3478"),
            sa("[fd00::1]:3478"),
            sa("0.0.0.0:3478"),
            sa("224.0.0.1:3478"),
            sa("192.168.1.9:3478"),
            sa("10.0.0.2:3478"),
        ];
        let got = |scope, v4, l: &[SocketAddr]| {
            let l = l.to_vec();
            resolve_stun(&name("stun.test.example"), scope, v4, move |h, p| {
                assert_eq!((h, p), ("stun.test.example", 3478));
                Ok(l)
            })
        };
        assert_eq!(got(Scope::Lan, true, &list), Ok(sa("192.168.1.9:3478")));
        assert_eq!(
            got(Scope::Internet, true, &list),
            Ok(sa("203.0.113.1:3478"))
        );
        assert_eq!(got(Scope::Lan, false, &list), Ok(sa("[fd00::1]:3478")));
        assert!(got(Scope::Lan, true, &[list[0], list[1], list[2], list[3]]).is_err());
        assert!(got(Scope::Internet, true, &[list[1], list[2], list[3]]).is_err());
        assert!(got(Scope::Lan, false, &[list[0], list[4]]).is_err());
        assert!(got(Scope::Lan, true, &[]).is_err());
        let e = resolve_stun(&name("stun.test.example"), Scope::Lan, true, |_, _| {
            Err("引く道具の誤り".to_string())
        })
        .unwrap_err();
        assert_eq!(e, "引く道具の誤り");
        let lo_addr = sa("127.0.0.1:3478");
        for h in ["localhost", "LOCALHOST"] {
            let r = resolve_stun(&name(h), Scope::Loopback, true, |_, _| Ok(vec![lo_addr]));
            assert_eq!(r, Ok(lo_addr), "{h}");
        }
        for h in ["stun.cloudflare.com", "localhost.example", "local"] {
            let e = resolve_stun(&name(h), Scope::Loopback, true, never).unwrap_err();
            assert!(e.contains("localhost 以外"), "{h}: {e}");
        }
        assert!(
            resolve_stun(&name("localhost"), Scope::Loopback, true, |_, _| Ok(vec![
                sa("192.168.1.9:3478")
            ]))
            .is_err()
        );
    }

    #[test]
    fn a_slow_lookup_is_given_up_and_a_fast_one_is_returned() {
        let t0 = Instant::now();
        let slow = run_with_timeout(
            || {
                std::thread::sleep(Duration::from_millis(400));
                7
            },
            Duration::from_millis(50),
        );
        assert_eq!(slow, Err(Stalled::TimedOut));
        assert!(
            t0.elapsed() < Duration::from_millis(300),
            "打ち切らずに待った"
        );
        assert_eq!(run_with_timeout(|| 7, Duration::from_secs(5)), Ok(7));
    }

    #[test]
    fn a_lookup_thread_that_could_not_start_is_not_reported_as_a_timeout() {
        let t0 = Instant::now();
        let r = run_with_timeout_via(
            |_job| Err(std::io::Error::other("スレッドの上限")),
            || 7,
            Duration::from_secs(5),
        );
        assert_eq!(r, Err(Stalled::NotStarted("スレッドの上限".to_string())));
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "起こせないのに待った"
        );
        let died = run_with_timeout(|| -> u32 { panic!("検査の panic") }, Duration::from_secs(5));
        assert_eq!(died, Err(Stalled::Died));

        let t = Duration::from_secs(2);
        let texts: Vec<String> = [
            Stalled::NotStarted("スレッドの上限".to_string()),
            Stalled::TimedOut,
            Stalled::Died,
        ]
        .iter()
        .map(|w| lookup_stalled_text("stun.test.example", t, w))
        .collect();
        assert!(
            texts[0].contains("起こせない") && texts[0].contains("スレッドの上限"),
            "{}",
            texts[0]
        );
        assert!(!texts[0].contains("打ち切"), "{}", texts[0]);
        assert!(
            texts[1].contains("2 秒で引けない") && texts[1].contains("打ち切った"),
            "{}",
            texts[1]
        );
        assert!(
            texts[2].contains("答えを返さずに終わった") && !texts[2].contains("打ち切"),
            "{}",
            texts[2]
        );
        assert!(texts.iter().all(|s| s.contains("stun.test.example")));
    }

    #[test]
    fn the_route_judgement_accepts_an_egress_address_and_names_every_failure() {
        let ok = |s: &str| judge_route(Ok(()), Ok(s.parse().unwrap()));
        for a in [
            "192.168.1.9:50000",
            "203.0.113.5:1",
            "169.254.7.7:9",
            "127.0.0.1:9",
        ] {
            let want: IpAddr = a.parse::<SocketAddr>().unwrap().ip();
            assert_eq!(ok(a), Ok(want), "{a}");
        }
        for a in ["0.0.0.0:0", "224.0.0.1:9", "[::]:0", "[ff02::1]:9"] {
            let e = ok(a).unwrap_err();
            assert!(e.contains("経路") && e.contains("使えない"), "{a}: {e}");
        }
        let e = judge_route(
            Err(std::io::Error::other("ネットワークに届かない")),
            Ok(lo(9)),
        )
        .unwrap_err();
        assert!(
            e.contains("経路が無い") && e.contains("ネットワークに届かない"),
            "{e}"
        );
        let e = judge_route(Ok(()), Err(std::io::Error::other("アドレスが取れない"))).unwrap_err();
        assert!(
            e.contains("経路") && e.contains("アドレスが取れない"),
            "{e}"
        );
    }

    fn nic(ip: &str, gateway: bool, metric: u32) -> Adapter {
        Adapter {
            description: "Realtek PCIe GbE Family Controller".to_string(),
            name: "イーサネット".to_string(),
            prefix: 24,
            if_type: 6,
            oui: [0x3C, 0x52, 0x82],
            up: true,
            preferred: true,
            ip: ip.parse().unwrap(),
            gateway,
            metric,
        }
    }

    /// 壊れたら落ちる: 経路が引けたのに一覧を引く（今の環境の動きが変わる）、経路が引けないときに一覧から選ばない、選べないときの理由が
    /// 経路の理由か一覧の理由のどちらかしか出ない、同点を選んでしまう。
    #[test]
    fn the_list_is_used_only_when_the_route_fails_and_a_tie_is_not_chosen() {
        let route_ok: IpAddr = "192.168.0.7".parse().unwrap();
        let never = || -> Result<Vec<Adapter>, String> {
            panic!("経路が引けたのに一覧を引いた")
        };
        assert_eq!(
            route_or_adapters(Ok(route_ok), never),
            Ok((route_ok, AddrSource::Route))
        );
        let no_route = || Err("外へ出る経路が無い: 到達できない".to_string());
        // 直結: 経路は無く、169.254 が 1 つだけ。
        let direct = vec![nic("169.254.20.5", false, 25)];
        let (ip, src) = route_or_adapters(no_route(), || Ok(direct)).unwrap();
        assert_eq!(
            (ip, src),
            ("169.254.20.5".parse().unwrap(), AddrSource::Adapters)
        );
        // 同点: 選ばず、両方の理由とアドレスを並べる。
        let tie = vec![nic("192.168.0.10", true, 30), nic("192.168.0.20", true, 30)];
        let e = route_or_adapters(no_route(), || Ok(tie)).unwrap_err();
        assert!(
            e.contains("外へ出る経路が無い")
                && e.contains("決められない")
                && e.contains("192.168.0.10・192.168.0.20"),
            "{e}"
        );
        // 仮想だけ。
        let mut vpn = nic("10.8.0.6", true, 5);
        vpn.if_type = 131;
        let e = route_or_adapters(no_route(), || Ok(vec![vpn])).unwrap_err();
        assert!(
            e.contains("外へ出る経路が無い") && e.contains("LAN で使える"),
            "{e}"
        );
        // 一覧が引けない。
        let e = route_or_adapters(no_route(), || Err("一覧が引けない".to_string())).unwrap_err();
        assert!(
            e.contains("外へ出る経路が無い") && e.contains("一覧が引けない"),
            "{e}"
        );
    }

    /// 壊れたら落ちる: `bind = auto` の経路が引けないとき、一覧から選んだことがログの文に出ない・scope = internet でも 169.254 を断る。
    #[test]
    fn bind_auto_names_the_adapter_list_when_it_picks_from_it() {
        use crate::net_config::Bind;
        let list = || Ok(vec![nic("169.254.20.5", false, 25)]);
        let (a, note) = Bind::Auto(17723)
            .resolve_by(Scope::Lan, || {
                route_or_adapters(Err("外へ出る経路が無い: x".to_string()), list)
            })
            .unwrap();
        assert_eq!(a, "169.254.20.5:17723".parse::<SocketAddr>().unwrap());
        assert!(note.contains("アダプターの一覧"), "{note}");
        // scope = internet でも、直結の 169.254 を選べる（ルーターなしの直結はインターネットの設定のままでも使える）。
        let (a, note) = Bind::Auto(17723)
            .resolve_by(Scope::Internet, || {
                route_or_adapters(Err("外へ出る経路が無い: x".to_string()), list)
            })
            .unwrap();
        assert_eq!(a, "169.254.20.5:17723".parse::<SocketAddr>().unwrap());
        assert!(note.contains("scope = internet"), "{note}");
        // 経路が引けたときの文は今までのまま。
        let (_, note) = Bind::Auto(17723)
            .resolve_by(Scope::Lan, || {
                route_or_adapters(Ok("192.168.0.7".parse().unwrap()), list)
            })
            .unwrap();
        assert!(
            note.contains("外へ出る経路の自分のアドレス") && !note.contains("アダプター"),
            "{note}"
        );
        // 範囲 loopback では一覧を引かない。
        let (a, _) = Bind::Auto(17723)
            .resolve_by(Scope::Loopback, || -> Result<_, String> {
                panic!("loopback で引いた")
            })
            .unwrap();
        assert!(a.ip().is_loopback());
    }

    #[test]
    fn the_route_probe_smoke_does_not_panic_on_this_os() {
        match probe_route_ip() {
            Ok(ip) => assert!(!ip.is_unspecified() && !ip.is_multicast(), "{ip}"),
            Err(e) => assert!(e.contains("経路"), "{e}"),
        }
    }

    #[test]
    fn the_ipv6_route_probe_smoke_does_not_panic_on_this_os() {
        match probe_route_ipv6() {
            Ok(ip) => {
                assert!(!ip.is_unspecified() && !ip.is_multicast(), "{ip}");
                let chosen = choose_v6_source(Ok(ip));
                eprintln!(
                    "IPV6-ROUTE: 引けた（族 {}・世界に通じるアドレスとして採れる {}）",
                    if ip.is_ipv6() { "IPv6" } else { "IPv4" },
                    chosen.is_ok()
                );
                if let Ok(v6) = chosen {
                    assert!(is_global_unicast_v6(v6));
                }
            }
            Err(e) => {
                assert!(e.contains("経路") || e.contains("IPv6"), "{e}");
                eprintln!("IPV6-ROUTE: 引けない（{e}）");
            }
        }
    }

    #[test]
    #[ignore = "STUN の打ち切りを実時間（2 秒）で見る。全件の回（build.bat）で --ignored で走る"]
    fn stun_query_times_out_on_a_silent_server() {
        let _g = socket_lock();
        let silent = UdpSocket::bind(lo(0)).unwrap();
        let (mut net, _th) = open(lo(0), &[lo(40_000)]).unwrap();
        let t0 = Instant::now();
        let r = net.stun_query(silent.local_addr().unwrap());
        let took = t0.elapsed();
        assert!(r.unwrap_err().contains("返らない"));
        assert!(
            took >= Duration::from_millis(1_900) && took < Duration::from_secs(4),
            "{took:?}"
        );
        let text = format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\nstun = {}\n",
            silent.local_addr().unwrap()
        );
        let s = crate::net_config::from_text(&text).unwrap().unwrap();
        let (mapped, state) = stun_state(&s, &mut net, &mut String::new());
        assert_eq!(mapped, None);
        assert!(
            matches!(&state, Stun::NoAnswer(e) if e.contains("返らない")),
            "{state:?}"
        );
    }

    #[test]
    fn prepare_link_draws_a_secret_and_writes_own_code() {
        let _g = socket_lock();
        let fake = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let text = format!(
            "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 0x1234\nstun = {}\n",
            fake.addr()
        );
        let s = crate::net_config::from_text(&text).unwrap().unwrap();
        let (mut net, _th) = open(lo(0), &s.peer_addrs()).unwrap();
        let me = net.local_addr().unwrap();
        let (lc, note) = prepare_link(&s, &mut net, 9).unwrap();
        let code_text = note
            .rsplit(": ")
            .next()
            .expect("接続コードが文に無い")
            .to_string();
        let c = higanr_core::code::read_initiator(&code_text, 0).unwrap();
        assert_eq!(c.secret, Some(lc.secret), "コードの秘密と箱の秘密が違う");
        assert_ne!(lc.secret, peer_sim::cli::DEFAULT_SECRET);
        assert_eq!(c.session_id, 0x1234);
        let addrs: Vec<SocketAddr> = c.candidates.as_slice().iter().map(|x| x.addr).collect();
        assert_eq!(addrs, vec![me]);
        assert!(note.contains("見えた自分のアドレス"), "{note}");
        let (lc2, _) = prepare_link(&s, &mut net, 9).unwrap();
        assert_ne!(lc.secret, lc2.secret);
        assert_ne!(lc.key_seed, lc2.key_seed, "2 つの箱が同じ鍵の種を使った");
        let s2 = crate::net_config::from_text(
            &(text.replace("stun = ", "; stun = ") + "secret = 00112233445566778899aabbccddeeff\n"),
        )
        .unwrap()
        .unwrap();
        let (lc3, note3) = prepare_link(&s2, &mut net, 9).unwrap();
        assert_eq!(lc3.secret.to_hex(), "00112233445566778899aabbccddeeff");
        assert!(note3.is_empty(), "{note3}");
        let (lc4, _) = prepare_link(&s2, &mut net, 9).unwrap();
        assert_eq!(lc4.secret, lc3.secret);
        assert_ne!(lc3.key_seed, lc4.key_seed, "秘密が同じ箱の鍵の種が同じ");
        assert_ne!(lc3.key_seed, lc.key_seed);
    }

    // ---- 外側のアドレスの保ち（STUN の聞き直し）----
    //
    // 本物の STUN へは送らない。偽の応答役（peer_sim の `stun_fake`。loopback）へ、短くした間隔で聞かせる。

    fn wait_for(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    fn keeping(
        interval: Duration,
    ) -> (
        peer_sim::stun_fake::StunFake,
        GameNet,
        RecvThread,
        SocketAddr,
    ) {
        let fake = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let mapped = net.stun_query(fake.addr()).unwrap();
        net.note_stun_answer(mapped);
        assert!(net.keep_stun_mapping(fake.addr(), mapped, interval));
        (fake, net, th, mapped)
    }

    #[test]
    fn the_keeper_asks_the_same_stun_on_the_interval_and_stops_when_the_link_is_up() {
        let _g = socket_lock();
        let every = Duration::from_millis(100);
        let (fake, mut net, th, mapped) = keeping(every);
        assert!(
            !net.keep_stun_mapping(fake.addr(), mapped, every),
            "2 回目の置きを受け付けた"
        );
        let base = fake.received();
        let t0 = Instant::now();
        assert!(wait_for(Duration::from_secs(5), || {
            th.diag().keep.counts.sent >= 4
        }));
        let took = t0.elapsed();
        let d = th.diag();
        assert!(took >= Duration::from_millis(280), "{took:?}");
        let upper = u32::try_from(took.as_millis() / 100).unwrap() + 1;
        assert!(d.keep.counts.sent <= upper, "{:?} {took:?}", d.keep);
        assert!(d.keep.installed && d.keep.running);
        assert!(wait_for(Duration::from_secs(2), || {
            th.diag().keep.counts.answered >= 3
        }));
        let d = th.diag();
        let got = fake.received() - base;
        assert!(
            got <= d.keep.counts.sent && got + 1 >= d.keep.counts.sent,
            "偽の STUN が受けた {got} / 送った {:?}",
            d.keep
        );
        assert_eq!(
            (d.keep.counts.changed_port, d.keep.counts.changed_addr),
            (0, 0),
            "同じアドレスの答えを変わったと数えた"
        );
        let other = UdpSocket::bind(lo(0)).unwrap();
        other
            .send_to(b"HGRT-while-keeping", net.local_addr().unwrap())
            .unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut from_others = Vec::new();
        assert!(wait_for(Duration::from_secs(2), || {
            while let Some((from, n, _)) = net.recv(&mut buf) {
                from_others.push((from, buf[..n].to_vec()));
            }
            from_others.iter().any(|(_, b)| b == b"HGRT-while-keeping")
        }));
        assert!(
            from_others.iter().all(|(f, _)| *f != fake.addr()),
            "聞き直しの答えがリングへ流れた: {from_others:?}"
        );
        net.link_up();
        assert!(wait_for(Duration::from_secs(2), || {
            !th.diag().keep.running
        }));
        let sent = th.diag().keep.counts.sent;
        let seen = fake.received();
        std::thread::sleep(Duration::from_millis(450));
        assert_eq!(th.diag().keep.counts.sent, sent, "繋がった後も送った");
        assert_eq!(fake.received(), seen, "繋がった後も STUN へ届いた");
        while net.recv(&mut buf).is_some() {}
        net.sock
            .send_to(&stun::encode_binding_request([7; 12]), fake.addr())
            .unwrap();
        assert!(wait_for(Duration::from_secs(2), || {
            matches!(net.recv(&mut buf), Some((f, _, _)) if f == fake.addr())
        }));
    }

    #[test]
    fn the_keeper_counts_a_moved_port_once_and_sees_it_move_back() {
        let _g = socket_lock();
        let (fake, _net, th, _) = keeping(Duration::from_millis(60));
        let answers = |n: u32| {
            wait_for(Duration::from_secs(5), || {
                th.diag().keep.counts.answered >= n
            })
        };
        assert!(answers(2));
        let c = th.diag().keep.counts;
        assert_eq!(
            (c.changed_port, c.changed_addr, c.differs_from_code),
            (0, 0, false)
        );
        fake.set_shift(7);
        assert!(wait_for(Duration::from_secs(5), || {
            th.diag().keep.counts.changed_port >= 1
        }));
        let at = th.diag().keep.counts.answered;
        assert!(answers(at + 3));
        let c = th.diag().keep.counts;
        assert_eq!(
            (c.changed_port, c.changed_addr, c.differs_from_code),
            (1, 0, true),
            "{c:?}"
        );
        fake.set_shift(0);
        assert!(wait_for(Duration::from_secs(5), || {
            th.diag().keep.counts.changed_port >= 2
        }));
        assert!(answers(th.diag().keep.counts.answered + 2));
        let d = th.diag();
        assert_eq!(
            (
                d.keep.counts.changed_port,
                d.keep.counts.changed_addr,
                d.keep.counts.differs_from_code
            ),
            (2, 0, false)
        );
        assert!(
            d.keep_text()
                .contains("変わった 2 回（ポートだけ 2 / アドレスも 0）"),
            "{}",
            d.keep_text()
        );
        fake.set_silent(true);
        let (sent, answered) = (d.keep.counts.sent, d.keep.counts.answered);
        std::thread::sleep(Duration::from_millis(150));
        let answered = answered.max(th.diag().keep.counts.answered);
        assert!(wait_for(Duration::from_secs(5), || {
            th.diag().keep.counts.sent >= sent + 4
        }));
        assert_eq!(
            th.diag().keep.counts.answered,
            answered,
            "答えが返らないのに答えを数えた"
        );
    }

    #[test]
    fn the_keeper_runs_only_for_internet_with_a_stun() {
        let _g = socket_lock();
        assert_eq!(
            [Scope::Loopback, Scope::Lan, Scope::Internet].map(keeps_mapping_fresh),
            [false, false, true]
        );
        let fake = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let every = Duration::from_millis(40);
        let text = |scope: &str, stun: &str| {
            format!(
                "[netplay]\nenable = 1\nscope = {scope}\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\n{stun}\n"
            )
        };
        // `keeping` が true の行は、聞き直しが 3 通届くまで（最大 5 秒）待つ。false の行は 400 ms 待って、1 通も来ないことを見る。
        let run = |scope: Scope, ini: &str, keeping: bool| {
            let s = crate::net_config::from_text(ini).unwrap().unwrap();
            let (mut net, th) = open_for_code_scoped(scope, lo(0)).unwrap();
            let mut note = String::new();
            let r = stun_state_with(
                &s,
                &mut net,
                &mut note,
                |_, _| Err("引かない".into()),
                every,
            );
            let after_first = fake.received();
            if keeping {
                wait_for(Duration::from_secs(5), || {
                    fake.received() - after_first >= 3
                });
            } else {
                std::thread::sleep(Duration::from_millis(400));
            }
            (r, note, th.diag(), fake.received() - after_first)
        };
        let stun = format!("stun = {}", fake.addr());
        let (r, note, d, extra) = run(Scope::Internet, &text("internet", &stun), true);
        assert_eq!(r.1, Stun::Seen);
        assert!(d.keep.installed && extra >= 3, "{d:?} {extra}");
        assert!(note.contains("聞き直して"), "{note}");
        assert!(d.port_kept == Some(true) && d.stun_at_ms.is_some(), "{d:?}");
        for scope in [Scope::Lan, Scope::Loopback] {
            let (r, note, d, extra) = run(scope, &text(scope.name(), &stun), false);
            assert_eq!(r.1, Stun::Seen, "{scope:?}: 最初の問い合わせは通る");
            assert!(!d.keep.installed, "{scope:?}: {d:?}");
            assert_eq!(extra, 0, "{scope:?}: 聞き直しを送った");
            assert!(!note.contains("聞き直して"), "{scope:?}: {note}");
        }
        let before = fake.received();
        let (r, _, d, _) = run(Scope::Internet, &text("internet", "stun ="), false);
        assert_eq!(r.1, Stun::NotUsed);
        assert!(!d.keep.installed && d.keep.counts == stun::KeepCounts::default());
        assert_eq!(fake.received(), before);
    }

    #[test]
    #[ignore = "答えない STUN の打ち切りを実時間（2 秒）で待つ。全件の回（build.bat）で --ignored で走る"]
    fn the_keeper_is_not_placed_when_the_first_stun_gets_no_answer() {
        let _g = socket_lock();
        let fake = peer_sim::stun_fake::StunFake::spawn().unwrap();
        fake.set_silent(true);
        let ini = format!(
            "[netplay]\nenable = 1\nscope = internet\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\nstun = {}\n",
            fake.addr()
        );
        let s = crate::net_config::from_text(&ini).unwrap().unwrap();
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let r = stun_state_with(
            &s,
            &mut net,
            &mut String::new(),
            |_, _| Err("引かない".into()),
            Duration::from_millis(40),
        );
        assert!(matches!(r.1, Stun::NoAnswer(_)), "{:?}", r.1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!th.diag().keep.installed, "答えが無いのに置いた");
        assert!(fake.received() <= stun::QUERY_TRIES);
    }

    #[test]
    #[ignore = "答えない STUN の打ち切りを実時間（2 秒）で待つ。全件の回（build.bat）で --ignored で走る"]
    fn when_the_first_stun_is_silent_the_second_answer_goes_into_the_code() {
        let _g = socket_lock();
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        f1.set_silent(true);
        let f2 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let addr2 = f2.addr();
        let s = second_settings("internet", f1.addr(), STUN2_NAME);
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let me = net.local_addr().unwrap();
        let asked = Arc::new(Mutex::new(None));
        let asked2 = Arc::clone(&asked);
        let mut note = String::new();
        let r = stun_state_inner(
            &s,
            &mut net,
            &mut note,
            |_, _| Err("引かない".into()),
            Duration::from_millis(40),
            move |h, p| {
                *asked2.lock().unwrap() = Some((h.to_string(), p));
                Ok(vec![addr2])
            },
        );
        assert_eq!(r, (Some(me), Stun::Seen), "{note}");
        assert_eq!(
            asked.lock().unwrap().clone(),
            Some(("stun2.test.example".to_string(), 3478))
        );
        assert!(
            note.contains("1 か所目が答えないので 2 か所目の STUN"),
            "{note}"
        );
        assert!(th.diag().keep.installed, "聞き直しを置いていない");
        let before = f2.received();
        assert!(wait_for(Duration::from_secs(2), || f2.received() > before));
        assert!(f1.received() <= stun::QUERY_TRIES);
        drop(th);

        let s = second_settings("lan", f1.addr(), STUN2_NAME);
        let (mut net, _th) = open_for_code_scoped(Scope::Lan, lo(0)).unwrap();
        let looked = Arc::new(Mutex::new(false));
        let looked2 = Arc::clone(&looked);
        let r = stun_state_inner(
            &s,
            &mut net,
            &mut String::new(),
            |_, _| Err("引かない".into()),
            KEEP_OFF,
            move |_, _| {
                *looked2.lock().unwrap() = true;
                Ok(vec![addr2])
            },
        );
        assert_eq!(r.0, None, "{:?}", r.1);
        assert!(!*looked.lock().unwrap(), "lan で 2 か所目の名前を引いた");
    }

    // ---- 2 か所目の STUN（宛先ごとに外側のポートを変える型かの見分け）----
    //
    // 本物の STUN へは送らない。偽の応答役（loopback）を 2 つ立て、1 か所目を `stun`、2 か所目を `stun2`（名前の形。名前引きは検査の
    // 道具を注入）に置く。

    fn second_settings(scope: &str, first: SocketAddr, stun2: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nscope = {scope}\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\nstun = {first}\n{stun2}\n"
        ))
        .unwrap()
        .unwrap()
    }

    const STUN2_NAME: &str = "stun2 = stun2.test.example:3478";

    #[test]
    fn the_second_stun_tells_a_same_mapping_from_a_port_that_moves_with_the_destination() {
        let _g = socket_lock();
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        for (shift, want) in [(0u16, stun::Mapping::Same), (9, stun::Mapping::PortOnly)] {
            let f2 = peer_sim::stun_fake::StunFake::spawn_shifted(shift).unwrap();
            let s = second_settings("internet", f1.addr(), STUN2_NAME);
            let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
            let me = net.local_addr().unwrap();
            let addr2 = f2.addr();
            let asked = Arc::new(Mutex::new(None));
            let asked2 = Arc::clone(&asked);
            let (mapped, state) = stun_state_inner(
                &s,
                &mut net,
                &mut String::new(),
                |_, _| Err("引かない".into()),
                KEEP_OFF,
                move |h, p| {
                    *asked2.lock().unwrap() = Some((h.to_string(), p));
                    Ok(vec![addr2])
                },
            );
            assert_eq!(
                (mapped, state),
                (Some(me), Stun::Seen),
                "1 か所目は今までどおり"
            );
            assert!(
                wait_for(Duration::from_secs(5), || th.diag().second
                    == SecondStun::Compared(want)),
                "shift {shift}: {:?}",
                th.diag().second
            );
            assert_eq!(
                asked.lock().unwrap().clone(),
                Some(("stun2.test.example".to_string(), 3478))
            );
            assert_eq!(th.diag().second.depends_on_destination(), Some(shift != 0));
            assert_eq!(net.own_port_varies(), Some(shift != 0));
            assert!(wait_for(Duration::from_secs(2), || f2
                .last_from()
                .is_some()));
            assert_eq!(f2.last_from(), Some(me));
            let mut b = [0u8; MAX_DATAGRAM];
            assert!(net.recv(&mut b).is_none(), "2 か所目の答えがリングへ届いた");
            assert!(!th.diag().second.text().contains("127.0.0.1"));
        }
    }

    #[test]
    fn the_second_stun_is_asked_only_for_internet_with_a_stun_and_a_stun2() {
        let _g = socket_lock();
        assert_eq!(
            [Scope::Loopback, Scope::Lan, Scope::Internet].map(asks_second_stun),
            [false, false, true]
        );
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let f2 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let addr2 = f2.addr();
        let run = |scope: Scope, stun2: &str, with_stun: bool, expect_ask: bool| {
            let mut s = second_settings(scope.name(), f1.addr(), stun2);
            if !with_stun {
                s.stun = None;
            }
            let (mut net, th) = open_for_code_scoped(scope, lo(0)).unwrap();
            let called = Arc::new(AtomicBool::new(false));
            let called2 = Arc::clone(&called);
            let before = f2.received();
            let _ = stun_state_inner(
                &s,
                &mut net,
                &mut String::new(),
                |_, _| Err("引かない".into()),
                KEEP_OFF,
                move |_, _| {
                    called2.store(true, Ordering::Relaxed);
                    Ok(vec![addr2])
                },
            );
            if expect_ask {
                // 聞く側は別スレッドなので、負荷の下では届くまでに 400 ms を超えることがある。
                wait_for(Duration::from_secs(3), || f2.received() > before);
            } else {
                std::thread::sleep(Duration::from_millis(400));
            }
            (
                called.load(Ordering::Relaxed),
                f2.received() - before,
                th.diag().second,
            )
        };
        for (scope, stun2, with_stun) in [
            (Scope::Loopback, STUN2_NAME, true),
            (Scope::Lan, STUN2_NAME, true),
            (Scope::Internet, "stun2 =", true),
            (Scope::Internet, "", true),
            (Scope::Internet, STUN2_NAME, false),
        ] {
            let (called, got, second) = run(scope, stun2, with_stun, false);
            assert!(
                !called && got == 0 && second == SecondStun::NotAsked,
                "{scope:?} {stun2:?} stun {with_stun}: 名前を引いた {called} / 2 か所目が受けた {got} / {second:?}"
            );
        }
        let (called, got, second) = run(Scope::Internet, STUN2_NAME, true, true);
        assert!(
            called && got >= 1 && second != SecondStun::NotAsked,
            "肯定の行で聞いていない: {called} {got} {second:?}"
        );
    }

    #[test]
    fn the_second_stun_does_not_hold_back_the_first_answer_while_the_name_is_looked_up() {
        let _g = socket_lock();
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let f2 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let addr2 = f2.addr();
        let s = second_settings("internet", f1.addr(), STUN2_NAME);
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let me = net.local_addr().unwrap();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let finished = Arc::new(AtomicBool::new(false));
        let finished2 = Arc::clone(&finished);
        let (mapped, state) = stun_state_inner(
            &s,
            &mut net,
            &mut String::new(),
            |_, _| Err("引かない".into()),
            KEEP_OFF,
            move |_, _| {
                let _ = gate.recv_timeout(Duration::from_secs(5));
                finished2.store(true, Ordering::Relaxed);
                Ok(vec![addr2])
            },
        );
        assert!(
            !finished.load(Ordering::Relaxed),
            "名前引きの終わりを待ってから戻った"
        );
        assert_eq!((mapped, state), (Some(me), Stun::Seen));
        assert_eq!(th.diag().second, SecondStun::Pending);
        assert_eq!(f2.received(), 0, "名前を引き終える前に送った");
        release.send(()).unwrap();
        assert!(
            wait_for(Duration::from_secs(5), || th.diag().second
                == SecondStun::Compared(stun::Mapping::Same)),
            "{:?}",
            th.diag().second
        );
    }

    #[test]
    fn the_second_stun_ignores_answers_from_other_sources_and_other_transactions() {
        let _g = socket_lock();
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let spoofer = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let addr2 = server.local_addr().unwrap();
        let s = second_settings("internet", f1.addr(), STUN2_NAME);
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let me = net.local_addr().unwrap();
        let _ = stun_state_inner(
            &s,
            &mut net,
            &mut String::new(),
            |_, _| Err("引かない".into()),
            KEEP_OFF,
            move |_, _| Ok(vec![addr2]),
        );
        let answer = |txn: stun::TransactionId, a: SocketAddr| {
            let mut out = [0u8; stun::MAX_SUCCESS_LEN];
            let n = stun::encode_binding_success(txn, a, &mut out);
            out[..n].to_vec()
        };
        let mut buf = [0u8; 1500];
        let (n, client) = server
            .recv_from(&mut buf)
            .expect("2 か所目へ問い合わせが来ない");
        assert_eq!(client, me, "同じ受信ソケットから出ていない");
        let txn = stun::binding_request_txn(&buf[..n]).expect("STUN の問い合わせでない");
        let moved = SocketAddr::new(me.ip(), me.port().wrapping_add(100));
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), me.port());
        spoofer.send_to(&answer(txn, moved), me).unwrap();
        server.send_to(&answer([9; 12], moved), me).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            th.diag().second,
            SecondStun::Pending,
            "偽の答えで結論が出た"
        );
        server.send_to(&answer(txn, me), me).unwrap();
        assert!(
            wait_for(Duration::from_secs(5), || th.diag().second
                == SecondStun::Compared(stun::Mapping::Same)),
            "{:?}",
            th.diag().second
        );
        server.send_to(&answer(txn, moved), me).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(th.diag().second, SecondStun::Compared(stun::Mapping::Same));

        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let addr2 = server.local_addr().unwrap();
        let s = second_settings("internet", f1.addr(), STUN2_NAME);
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let _ = stun_state_inner(
            &s,
            &mut net,
            &mut String::new(),
            |_, _| Err("引かない".into()),
            KEEP_OFF,
            move |_, _| Ok(vec![addr2]),
        );
        let (n, client) = server
            .recv_from(&mut buf)
            .expect("2 回目の問い合わせが来ない");
        let txn = stun::binding_request_txn(&buf[..n]).unwrap();
        server.send_to(&answer(txn, v6), client).unwrap();
        assert!(
            wait_for(Duration::from_secs(5), || th.diag().second
                == SecondStun::Incomparable),
            "{:?}",
            th.diag().second
        );
    }

    #[test]
    fn the_second_stun_is_skipped_for_the_same_server_an_unresolvable_name_or_a_panic() {
        let _g = socket_lock();
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let addr1 = f1.addr();
        let names = ["1 か所目と同じアドレス", "引けない", "範囲の外", "パニック"];
        for (i, name) in names.into_iter().enumerate() {
            let s = second_settings("internet", addr1, STUN2_NAME);
            let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
            let me = net.local_addr().unwrap();
            let (mapped, state) = stun_state_inner(
                &s,
                &mut net,
                &mut String::new(),
                |_, _| Err("引かない".into()),
                KEEP_OFF,
                move |_, _| match i {
                    0 => Ok(vec![addr1]),
                    1 => Err("合成".to_string()),
                    2 => Ok(vec![SocketAddr::from(([224, 0, 0, 1], 3478))]),
                    _ => panic!("名前引きの中のパニック（検査のための合成）"),
                },
            );
            assert_eq!((mapped, state), (Some(me), Stun::Seen), "{name}");
            assert!(
                wait_for(Duration::from_secs(5), || th.diag().second
                    == SecondStun::Skipped),
                "{name}: {:?}",
                th.diag().second
            );
        }
    }

    #[test]
    #[ignore = "答えない 2 か所目を実時間（2 秒余り）で待つ。全件の回（build.bat）で --ignored で走る"]
    fn the_second_stun_gives_up_quietly_when_it_gets_no_answer() {
        let _g = socket_lock();
        let f1 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        let f2 = peer_sim::stun_fake::StunFake::spawn().unwrap();
        f2.set_silent(true);
        let addr2 = f2.addr();
        let s = second_settings("internet", f1.addr(), STUN2_NAME);
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let t0 = Instant::now();
        let (mapped, state) = stun_state_inner(
            &s,
            &mut net,
            &mut String::new(),
            |_, _| Err("引かない".into()),
            KEEP_OFF,
            move |_, _| Ok(vec![addr2]),
        );
        let took = t0.elapsed();
        assert!(mapped.is_some() && state == Stun::Seen);
        assert!(took < Duration::from_millis(1_000), "{took:?}");
        assert!(
            wait_for(Duration::from_secs(8), || th.diag().second
                == SecondStun::NoAnswer),
            "{:?}",
            th.diag().second
        );
        // 上限は厳密に、下限は 2 通の取りこぼしを許す: 負荷の下で、送りは 8 回とも Ok なのに偽の STUN が 3 秒待っても 6〜7 通しか
        // 受けない実行が 48 走行のうち 4 回あった（loopback の UDP の取りこぼし。実測）。
        wait_for(Duration::from_secs(3), || {
            f2.received() >= stun::QUERY_TRIES
        });
        let got = f2.received();
        assert!(
            (stun::QUERY_TRIES - 2..=stun::QUERY_TRIES).contains(&got),
            "8 回聞いて諦める: {got}"
        );
    }

    #[test]
    fn diagnostics_count_send_errors_by_candidate_kind_and_remember_own_candidates() {
        use higanr_core::candidate::CandKind;
        let _g = socket_lock();
        let (mut net, th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let peer = UdpSocket::bind(lo(0)).unwrap();
        net.send(peer.local_addr().unwrap(), b"ok");
        assert_eq!(
            th.diag().send_errors,
            [0; KIND_SLOTS],
            "送れたのに誤りに数えた"
        );
        net.send("[2001:db8::2]:40000".parse().unwrap(), b"x");
        assert_eq!(net.send_counts(), (2, 1));
        assert_eq!(
            th.diag().send_errors,
            [0, 0, 0, 1],
            "IPv6 への送りの誤りが IPv6 の欄に数えられていない"
        );
        let c = |k, a: &str| Candidate::new(k, a.parse().unwrap()).unwrap();
        net.note_own_candidates(&[
            c(CandKind::External, "203.0.113.5:1000"),
            c(CandKind::Host, "192.168.1.5:47001"),
            c(CandKind::Host, "10.0.0.5:47001"),
            c(CandKind::Ipv6, "[2001:db8::5]:47001"),
            c(CandKind::Host, "172.16.0.5:47001"),
        ]);
        assert_eq!(th.diag().own_cands, [0, 3, 1, 1]);
        assert_eq!(th.diag().stun_at_ms, None);
        assert_eq!(th.diag().port_kept, None);
        let me = net.local_addr().unwrap();
        net.note_stun_answer(me);
        assert_eq!(th.diag().port_kept, Some(true));
        assert!(th.diag().stun_at_ms.is_some());
        net.note_stun_answer(SocketAddr::new(me.ip(), me.port().wrapping_add(1)));
        assert_eq!(th.diag().port_kept, Some(false));
    }

    // ---- IPv6 の経路 ----

    fn v6lo(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
    }

    /// この PC で IPv6 の loopback（`::1`）が使えるか。使えなければ標準エラーに「測れなかった」と書いて false（その検査は何も
    /// 確かめていない。通ったように見せない）。
    fn v6_loopback_or_unmeasured(what: &str) -> bool {
        match UdpSocket::bind(v6lo(0)) {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "IPV6-UNMEASURED: {what}: この PC では [::1] を bind できない（{e}）。この検査は測れていない"
                );
                false
            }
        }
    }

    fn unused_v6_port() -> u16 {
        UdpSocket::bind(v6lo(0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn ipv6_settings(scope: &str, extra: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nscope = {scope}\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 1\n{extra}\n"
        ))
        .unwrap()
        .unwrap()
    }

    #[test]
    fn the_v6_source_is_taken_only_when_it_is_global_unicast() {
        for ok in ["2400:cb00::7", "2a00:1450::1", "2001:200::1"] {
            let ip: Ipv6Addr = ok.parse().unwrap();
            assert_eq!(choose_v6_source(Ok(IpAddr::V6(ip))), Ok(ip), "{ok}");
        }
        for ng in [
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2002::1",
            "2001:db8::1",
            "fe80::1",
            "fd00::1",
            "::1",
            "::",
            "ff02::1",
            "192.168.0.2",
            "203.0.113.5",
        ] {
            let ip: IpAddr = ng.parse().unwrap();
            let e = choose_v6_source(Ok(ip)).expect_err(ng);
            assert!(e.contains(&ip.to_string()), "{ng}: {e}");
        }
        assert_eq!(
            choose_v6_source(Err("外へ出る経路が無い: x".to_string())),
            Err("外へ出る経路が無い: x".to_string())
        );
    }

    #[test]
    fn the_ipv6_path_opens_only_for_internet_with_a_global_source_and_failures_keep_ipv4() {
        let _g = socket_lock();
        let boom = || -> Result<IpAddr, String> { panic!("経路を引いた") };
        for scope in ["loopback", "lan"] {
            let s = ipv6_settings(scope, "");
            let (net, _th) = open_for_code_scoped(s.scope, lo(0)).unwrap();
            let mut note = String::new();
            assert_eq!(open_ipv6_with(&s, &net, &mut note, boom), None, "{scope}");
            assert!(note.is_empty(), "{scope}: 範囲の外では何も書かない: {note}");
        }
        let (net, _th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let v4 = net.local_addr();
        let off = ipv6_settings("internet", "ipv6 = off");
        let mut note = String::new();
        assert_eq!(open_ipv6_with(&off, &net, &mut note, boom), None);
        assert!(note.contains("ipv6 = off"), "{note}");
        let on = ipv6_settings("internet", "");
        let cases: [(Result<IpAddr, String>, &str); 4] = [
            (Err("外へ出る経路が無い: テスト".to_string()), "経路が無い"),
            (Ok(IpAddr::V6(Ipv6Addr::LOCALHOST)), "世界に通じる"),
            (Ok("fe80::1".parse().unwrap()), "世界に通じる"),
            (Ok("2400:cb00::7".parse().unwrap()), "bind に失敗"),
        ];
        for (route, want) in cases {
            let mut note = String::new();
            assert_eq!(open_ipv6_with(&on, &net, &mut note, || route), None);
            assert!(
                note.contains("IPv6 の経路は使わない") && note.contains(want),
                "{want}: {note}"
            );
        }
        assert_eq!(
            net.local_addr(),
            v4,
            "IPv6 の失敗が IPv4 の受信ソケットを変えた"
        );
    }

    #[test]
    fn the_ipv6_port_follows_the_ipv4_one_and_falls_back_to_any() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 のポートの決め方") {
            return;
        }
        let (net, _th) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let p4 = net.local_addr().unwrap().port();
        if UdpSocket::bind(v6lo(p4)).is_err() {
            eprintln!(
                "IPV6-UNMEASURED: 同じ番号 {p4} の [::1] が使用中。同じ番号を採る枝は測れていない"
            );
        } else {
            let mut note = String::new();
            let got = open_ipv6_on(&net, Ipv6Addr::LOCALHOST, &mut note).expect(&note);
            assert_eq!(got, v6lo(p4), "同じ番号を先に試さない: {note}");
            assert!(note.contains(&got.to_string()), "{note}");
        }
        let (net2, _th2) = open_for_code_scoped(Scope::Internet, lo(0)).unwrap();
        let p4b = net2.local_addr().unwrap().port();
        let Ok(_hold) = UdpSocket::bind(v6lo(p4b)) else {
            eprintln!("IPV6-UNMEASURED: 同じ番号 {p4b} を塞げない。逃げ道の枝は測れていない");
            return;
        };
        let mut note = String::new();
        let got = open_ipv6_on(&net2, Ipv6Addr::LOCALHOST, &mut note).expect(&note);
        assert_ne!(got.port(), p4b, "塞がった番号を開いた?: {note}");
        assert!(got.is_ipv6() && got.port() != 0, "{got}");
    }

    #[test]
    fn ipv6_datagrams_reach_their_own_ring_and_wake_the_waiter() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 のリング") {
            return;
        }
        let (mut a, tha) = open_for_code(lo(0)).unwrap();
        let (mut b, thb) = open_for_code(lo(0)).unwrap();
        let a6 = a.open_ipv6(v6lo(0)).unwrap();
        let b6 = b.open_ipv6(v6lo(0)).unwrap();
        assert!(a6.is_ipv6() && a6.port() != 0 && b6.port() != 0);
        assert!(a.open_ipv6(v6lo(0)).is_err(), "2 回目を開けた");
        assert!(
            a.open_ipv6(lo(0)).is_err(),
            "IPv4 のアドレスで IPv6 の受信ソケットを開けた"
        );
        let t0 = Instant::now();
        a.send(b6, b"HGRT-v6");
        b.wait(1_000_000);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "イベントで起きない"
        );
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut got = None;
        for _ in 0..100 {
            if let Some(x) = b.recv(&mut buf) {
                got = Some(x);
                break;
            }
            b.wait(2_000);
        }
        let (from, n, _) = got.expect("IPv6 のリングから取り出せない");
        assert!(
            higanr_core::candidate::same_endpoint(from, a6),
            "{from} / {a6}"
        );
        assert_eq!(&buf[..n], b"HGRT-v6");
        b.send(a6, b"back-v6");
        let mut back = None;
        for _ in 0..100 {
            if let Some(x) = a.recv_marked(&mut buf) {
                back = Some(x);
                break;
            }
            a.wait(2_000);
        }
        let (from, n, _, marked) = back.expect("返しが届かない");
        assert!(
            higanr_core::candidate::same_endpoint(from, b6),
            "{from} / {b6}"
        );
        assert_eq!((&buf[..n], marked), (&b"back-v6"[..], false));
        let (sa, sb) = (tha.stats(), thb.stats());
        assert_eq!((sb.ring.pushed, sb.ring6.pushed), (0, 1), "{sb:?}");
        assert_eq!((sa.ring.pushed, sa.ring6.pushed), (0, 1), "{sa:?}");
        assert_eq!((a.send_counts(), b.send_counts()), ((1, 0), (1, 0)));
    }

    #[test]
    fn stopping_the_receive_thread_also_stops_the_ipv6_thread_and_frees_the_port() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 の片づけ") {
            return;
        }
        let (net, mut th) = open_for_code(lo(0)).unwrap();
        let a6 = net.open_ipv6(v6lo(0)).unwrap();
        assert!(
            th.shared.join6.lock().unwrap().is_some(),
            "IPv6 の受信スレッドの持ち手を置いていない"
        );
        th.halt();
        assert!(
            th.shared.join6.lock().unwrap().is_none(),
            "止めたのに IPv6 の受信スレッドを待っていない"
        );
        let stats = th.stop();
        drop(net);
        assert_eq!(stats.ring6.pushed, 0);
        UdpSocket::bind(a6).expect("止めた直後に IPv6 の受信ソケットが塞がったまま");
    }

    #[test]
    fn an_ipv6_primary_socket_still_sends_to_ipv6_and_refuses_a_second_ipv6_path() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 の受信ソケット") {
            return;
        }
        let peer = UdpSocket::bind(v6lo(0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (mut net, th) = open_for_code(v6lo(0)).unwrap();
        let me = net.local_addr().unwrap();
        assert!(
            net.open_ipv6(v6lo(0)).is_err(),
            "受信ソケットが IPv6 なのに足せた"
        );
        net.send(peer.local_addr().unwrap(), b"from-v6-primary");
        let mut rb = [0u8; 64];
        let (n, from) = peer
            .recv_from(&mut rb)
            .expect("受信ソケットから IPv6 の宛先へ送れない");
        assert_eq!((&rb[..n], from), (&b"from-v6-primary"[..], me));
        assert_eq!(net.send_counts(), (1, 0));
        peer.send_to(b"HGRT-x", me).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while th.stats().ring.pushed < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let s = th.stats();
        assert_eq!((s.ring.pushed, s.ring6.pushed), (1, 0), "{s:?}");
    }

    #[test]
    fn ipv6_game_side_calls_never_allocate() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 の確保") {
            return;
        }
        let (mut a, tha) = open_for_code(lo(0)).unwrap();
        let a6 = a.open_ipv6(v6lo(0)).unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        a.send(a6, b"warm");
        let deadline = Instant::now() + Duration::from_secs(2);
        while tha.stats().ring6.pushed < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(a.recv(&mut buf).is_some(), "暖め: IPv6 のリングに届かない");
        a.send(a6, b"x");
        a.send(a6, b"y");
        while tha.stats().ring6.pushed < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let before = crate::test_alloc::count();
        let one = a.recv(&mut buf);
        let two = a.recv_marked(&mut buf);
        a.send(a6, b"z");
        let none = a.recv(&mut buf);
        let used = crate::test_alloc::count() - before;
        assert!(
            one.is_some() && two.is_some(),
            "取り出せていない（この検査の前提）"
        );
        let _ = none;
        assert_eq!(used, 0, "IPv6 の経路を開いたゲームスレッドの口で確保した");
    }

    #[test]
    fn the_receive_thread_answers_ipv6_pings_from_the_socket_of_the_path_family() {
        use higanr_core::auth::AuthKey;
        use higanr_core::candidate::Candidates;
        use higanr_core::wire::Message;
        const SID: u32 = 0x6060_0002;
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 の PONG") {
            return;
        }
        let secret = Secret([0x6C; 16]);
        let key = AuthKey::derive(&secret, SID);
        let (mut a, tha) = open_for_code(lo(0)).unwrap();
        let a4 = a.local_addr().unwrap();
        let a6 = a.open_ipv6(v6lo(0)).unwrap();
        let peer6 = UdpSocket::bind(v6lo(0)).unwrap();
        let peer4 = UdpSocket::bind(lo(0)).unwrap();
        let (p6, p4) = (peer6.local_addr().unwrap(), peer4.local_addr().unwrap());
        for s in [&peer6, &peer4] {
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        }
        // 経路は IPv6（p6）。受けてよい送り元は p6 と p4 の両方（候補）。口の向きの鍵は 0x61…（送る向き）と 0x62…（受ける向き）。
        // 相手役はその逆で封じて開く。
        let dir = higanr_core::seal::DirKeys::from_raw([0x61; 32], [0x62; 32]);
        let peer_dir = dir.swapped();
        let r = PongResponder::new(
            SID,
            key.clone(),
            dir.opener(),
            dir.sealer(higanr_core::seal::Sender::Pong),
            Candidates::from_addrs(&[p6, p4]).unwrap(),
            p6,
        );
        assert!(a.pong_install(&r));
        a.pong_enable(true);
        let now = (a.now_us() / 1_000) as u32;
        a.heartbeat(now);
        let peer_seq = higanr_core::seal::Seq::new();
        let ping = |t: u32| {
            let mut out = [0u8; MAX_DATAGRAM];
            let sealer = peer_dir.sealer(higanr_core::seal::Sender::Link);
            let n = wire::seal(
                &sealer,
                &peer_seq,
                SID,
                &Message::Ping { send_time: t },
                &mut out,
            )
            .expect("封じられる");
            out[..n].to_vec()
        };
        let pong_of = |s: &UdpSocket| {
            let mut rb = [0u8; RECV_BUF];
            let (n, from) = s.recv_from(&mut rb).expect("PONG が返らない");
            (
                from,
                wire::decode(&key, Some(&peer_dir.opener()), &rb[..n]).map(|p| p.msg),
            )
        };
        peer6.send_to(&ping(0x61), a6).unwrap();
        let (from, msg) = pong_of(&peer6);
        assert!(higanr_core::candidate::same_endpoint(from, a6), "{from}");
        assert_eq!(msg, Ok(Message::Pong { send_time: 0x61 }));
        peer4.send_to(&ping(0x62), a4).unwrap();
        let (from, msg) = pong_of(&peer6);
        assert!(higanr_core::candidate::same_endpoint(from, a6), "{from}");
        assert_eq!(msg, Ok(Message::Pong { send_time: 0x62 }));
        let stranger = UdpSocket::bind(v6lo(0)).unwrap();
        stranger
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        stranger.send_to(&ping(0x63), a6).unwrap();
        let mut rb = [0u8; RECV_BUF];
        assert!(
            stranger.recv_from(&mut rb).is_err(),
            "候補にない送り元に返した"
        );
        assert_eq!(tha.stats().pongs_sent, 2);
    }

    #[test]
    fn two_game_nets_connect_over_ipv6_or_ipv4_whichever_candidates_are_alive() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("IPv6 で繋がる") {
            return;
        }
        for (kill_v4, kill_v6) in [(true, false), (false, true), (false, false)] {
            let (mut an, atha) = open_for_code(lo(0)).unwrap();
            let (mut bn, bth) = open_for_code(lo(0)).unwrap();
            let (a4, b4) = (an.local_addr().unwrap(), bn.local_addr().unwrap());
            let a6 = an.open_ipv6(v6lo(0)).unwrap();
            let b6 = bn.open_ipv6(v6lo(0)).unwrap();
            let dead4 = || {
                let s = UdpSocket::bind(lo(0)).unwrap();
                s.local_addr().unwrap()
            };
            let dead6 = || v6lo(unused_v6_port());
            let a_sees_b = [
                if kill_v4 { dead4() } else { b4 },
                if kill_v6 { dead6() } else { b6 },
            ];
            let b_sees_a = [
                if kill_v4 { dead4() } else { a4 },
                if kill_v6 { dead6() } else { a6 },
            ];
            let cfg = |nonce: u64, peers: &[SocketAddr]| link::Config {
                handshake: higanr_core::handshake::Config {
                    session_id: 0x6060_0001,
                    role_nonce: nonce,
                    latency: 2,
                    name: higanr_core::handshake::Name::new("v6"),
                    host_seat: higanr_core::wire::Seat::P1,
                },
                secret: Secret([0x6B; 16]),
                peers: higanr_core::candidate::Candidates::from_addrs(peers).unwrap(),
                key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
            };
            let mut la = link::Link::new(cfg(1, &a_sees_b), 0);
            let mut lb = link::Link::new(cfg(2, &b_sees_a), 0);
            let t0 = Instant::now();
            let now = || t0.elapsed().as_millis() as u32;
            let mut buf = [0u8; MAX_DATAGRAM];
            while !(la.connected().is_some() && lb.connected().is_some()) {
                assert!(
                    t0.elapsed() < Duration::from_secs(5),
                    "v4 を外す {kill_v4} / v6 を外す {kill_v6}: 繋がらない（A {:?} / B {:?}）",
                    la.handshake_state(),
                    lb.handshake_state()
                );
                la.poll(now(), &mut an);
                lb.poll(now(), &mut bn);
                while let Some((from, n, _)) = an.recv(&mut buf) {
                    la.on_datagram(from, &buf[..n], now(), &mut an);
                }
                while let Some((from, n, _)) = bn.recv(&mut buf) {
                    lb.on_datagram(from, &buf[..n], now(), &mut bn);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let rtt_until = Instant::now() + Duration::from_secs(5);
            let rtt = |l: &link::Link| l.connected().and_then(|c| c.liveness().rtt_ms());
            while !(rtt(&la).is_some() && rtt(&lb).is_some()) && Instant::now() < rtt_until {
                la.poll(now(), &mut an);
                lb.poll(now(), &mut bn);
                while let Some((from, n, _)) = an.recv(&mut buf) {
                    la.on_datagram(from, &buf[..n], now(), &mut an);
                }
                while let Some((from, n, _)) = bn.recv(&mut buf) {
                    lb.on_datagram(from, &buf[..n], now(), &mut bn);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let tag = format!("v4 を外す {kill_v4} / v6 を外す {kill_v6}");
            assert!(
                rtt(&la).is_some() && rtt(&lb).is_some(),
                "{tag}: RTT が測れない"
            );
            let (sa, sb) = (atha.stats(), bth.stats());
            let same = higanr_core::candidate::same_endpoint;
            if kill_v4 {
                assert!(
                    la.path().is_some_and(|p| same(p, b6)),
                    "{tag}: {:?}",
                    la.path()
                );
                assert!(
                    lb.path().is_some_and(|p| same(p, a6)),
                    "{tag}: {:?}",
                    lb.path()
                );
                assert_eq!(
                    (sa.ring.pushed, sb.ring.pushed),
                    (0, 0),
                    "{tag}: IPv4 に届いた"
                );
                assert!(sa.ring6.pushed > 0 && sb.ring6.pushed > 0, "{tag}");
            } else if kill_v6 {
                assert!(
                    la.path().is_some_and(|p| same(p, b4)),
                    "{tag}: {:?}",
                    la.path()
                );
                assert!(
                    lb.path().is_some_and(|p| same(p, a4)),
                    "{tag}: {:?}",
                    lb.path()
                );
                assert_eq!(
                    (sa.ring6.pushed, sb.ring6.pushed),
                    (0, 0),
                    "{tag}: IPv6 に届いた"
                );
                assert!(sa.ring.pushed > 0 && sb.ring.pushed > 0, "{tag}");
            } else {
                assert!(la.path().is_some() && lb.path().is_some(), "{tag}");
            }
            assert_eq!(an.send_counts().1 + bn.send_counts().1, 0, "{tag}");
        }
    }

    #[test]
    fn one_sided_ipv6_peer_still_connects_over_ipv4_and_counts_the_v6_send_errors() {
        let _g = socket_lock();
        if !v6_loopback_or_unmeasured("片側だけ IPv6 でも IPv4 で繋がる") {
            return;
        }
        let (mut an, atha) = open_for_code(lo(0)).unwrap();
        let (mut bn, bth) = open_for_code(lo(0)).unwrap();
        let (a4, b4) = (an.local_addr().unwrap(), bn.local_addr().unwrap());
        let a6 = an.open_ipv6(v6lo(0)).unwrap();
        let a_sees_b = [b4];
        let b_sees_a = [a4, a6];
        let cfg = |nonce: u64, peers: &[SocketAddr]| link::Config {
            handshake: higanr_core::handshake::Config {
                session_id: 0x6060_0002,
                role_nonce: nonce,
                latency: 2,
                name: higanr_core::handshake::Name::new("v6"),
                host_seat: higanr_core::wire::Seat::P1,
            },
            secret: Secret([0x6C; 16]),
            peers: higanr_core::candidate::Candidates::from_addrs(peers).unwrap(),
            key_seed: higanr_core::seal::KeySeed([0x4B; 32]),
        };
        let mut la = link::Link::new(cfg(1, &a_sees_b), 0);
        let mut lb = link::Link::new(cfg(2, &b_sees_a), 0);
        let t0 = Instant::now();
        let now = || t0.elapsed().as_millis() as u32;
        let mut buf = [0u8; MAX_DATAGRAM];
        while !(la.connected().is_some() && lb.connected().is_some()) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "片側だけ IPv6: 繋がらない（A {:?} / B {:?}）",
                la.handshake_state(),
                lb.handshake_state()
            );
            la.poll(now(), &mut an);
            lb.poll(now(), &mut bn);
            while let Some((from, n, _)) = an.recv(&mut buf) {
                la.on_datagram(from, &buf[..n], now(), &mut an);
            }
            while let Some((from, n, _)) = bn.recv(&mut buf) {
                lb.on_datagram(from, &buf[..n], now(), &mut bn);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let same = higanr_core::candidate::same_endpoint;
        assert!(la.path().is_some_and(|p| same(p, b4)), "{:?}", la.path());
        assert!(lb.path().is_some_and(|p| same(p, a4)), "{:?}", lb.path());
        assert_eq!(atha.stats().ring6.pushed, 0, "A の IPv6 のソケットに届いた");
        assert!(atha.stats().ring.pushed > 0 && bth.stats().ring.pushed > 0);
        // B の v6 宛ての送りは誤りに数えられ、A 側に誤りは無い。宛先は `[::1]`（世界に通じるアドレスではない）なので `kind_of` は IPv6 の欄
        // ではなく別の欄（添字 0）へ入れる。欄の合計が送りの誤りの数と一致するかを見る。
        let errs = bn.send_counts().1;
        assert!(errs > 0, "B の送りの誤りが数えられていない");
        assert_eq!(
            bth.diag().send_errors.iter().sum::<u32>() as u64,
            errs,
            "欄の合計: {:?}",
            bth.diag().send_errors
        );
        assert_eq!(an.send_counts().1, 0, "A の送りに誤りがある");
    }
}

#[cfg(test)]
mod pong_gate_tests {
    use super::*;
    use higanr_core::auth::AuthKey;
    use higanr_core::candidate::Candidates;
    use higanr_core::wire::Message;

    const SID: u32 = 0x5C0F_E001;

    fn shared(scope: Scope) -> Shared {
        Shared {
            ring: RecvRing::new(),
            t0: Instant::now(),
            event: Arc::new(Event::new().expect("イベント")),
            stop: AtomicBool::new(false),
            conn_resets: AtomicU32::new(0),
            recv_errors: AtomicU32::new(0),
            pong: PongSlot::default(),
            spectate: SpectateSlot::default(),
            scope,
            keep: KeepSlot::default(),
            second: SecondSlot::default(),
            diag: DiagSlot::default(),
            sock6: std::sync::OnceLock::new(),
            ring6: RecvRing::new(),
            join6: Mutex::new(None),
            upnp: std::sync::OnceLock::new(),
        }
    }

    fn install(s: &Shared, path: SocketAddr) {
        let key = AuthKey::derive(&Secret([0x33; 16]), SID);
        let peers = Candidates::from_addrs(&[path]).expect("候補");
        let dir = test_dir();
        *s.pong.responder.lock().unwrap() = Some(PongResponder::new(
            SID,
            key,
            dir.opener(),
            dir.sealer(higanr_core::seal::Sender::Pong),
            peers,
            path,
        ));
        s.pong.generation.store(1, Ordering::Release);
        s.pong.on.store(true, Ordering::Release);
        s.pong.beat.store(now_ms(s).max(1), Ordering::Release);
    }

    /// 口の向きの鍵。相手役は入れ替えた鍵で封じる。
    fn test_dir() -> higanr_core::seal::DirKeys {
        higanr_core::seal::DirKeys::from_raw([0x33; 32], [0x34; 32])
    }

    fn ping() -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let sealer = test_dir().swapped().sealer(higanr_core::seal::Sender::Link);
        let n = wire::seal(
            &sealer,
            &higanr_core::seal::Seq::new(),
            SID,
            &Message::Ping { send_time: 7 },
            &mut buf,
        )
        .expect("封じられる");
        buf[..n].to_vec()
    }

    /// (sent, refused, send_errors) と `answer_ping` の戻り値。
    fn answer(scope: Scope, path: SocketAddr) -> (bool, u32, u32, u32) {
        let sock = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let s = shared(scope);
        install(&s, path);
        let mut cache = (0, None);
        let r = answer_ping(&sock, &s, &mut cache, path, &ping());
        let p = &s.pong;
        (
            r,
            p.sent.load(Ordering::Relaxed),
            p.refused.load(Ordering::Relaxed),
            p.send_errors.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn the_receive_thread_gate_uses_the_opened_scope() {
        let dest = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let local = dest.local_addr().unwrap();
        let lan = SocketAddr::from(([10, 255, 255, 254], 47_001));
        assert_eq!(answer(Scope::Loopback, local), (true, 1, 0, 0));
        let (r, sent, refused, _) = answer(Scope::Loopback, lan);
        assert_eq!((r, sent, refused), (false, 0, 1));
        for scope in [Scope::Lan, Scope::Internet] {
            let (_, _, refused, _) = answer(scope, lan);
            assert_eq!(refused, 0, "{scope:?} で LAN の経路をガードで止めた");
        }
        for scope in [Scope::Loopback, Scope::Lan, Scope::Internet] {
            let (_net, th) =
                open_for_code_scoped(scope, SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            assert_eq!(th.shared.scope, scope);
        }
    }

    /// 壊れたら落ちる: 受信スレッドの返信のガードが、VPN のネットワークを足した範囲の外（ネットワークのアドレス・ブロードキャスト・隣の /8・世界のアドレス）へ
    /// 送る、範囲 lan のままの接続が VPN の宛先へ送る。通る宛先（VPN のネットワークの中）へは送らない（外へ出るため）ので、断る側だけ数える。
    #[test]
    fn the_receive_thread_gate_refuses_outside_a_widened_lan_scope() {
        let radmin =
            Scope::LanVpn(higanr_core::vpn_net::Net4::new(Ipv4Addr::new(26, 1, 2, 3), 8).unwrap());
        let at = |a: [u8; 4]| SocketAddr::from((a, 47_001));
        for refused in [
            at([26, 0, 0, 0]),
            at([26, 255, 255, 255]),
            at([27, 0, 0, 1]),
            at([203, 0, 113, 2]),
        ] {
            let (r, sent, count, _) = answer(radmin, refused);
            assert_eq!((r, sent, count), (false, 0, 1), "{refused}");
        }
        let (r, sent, count, _) = answer(Scope::Lan, at([26, 12, 34, 56]));
        assert_eq!(
            (r, sent, count),
            (false, 0, 1),
            "lan の返信が VPN の宛先を通した"
        );
    }

    /// 相手役の受け（口の向きの逆の鍵で開く）。
    fn read_at_peer(dest: &UdpSocket) -> Option<Message> {
        let mut buf = [0u8; 2048];
        let (n, _) = dest.recv_from(&mut buf).ok()?;
        let key = AuthKey::derive(&Secret([0x33; 16]), SID);
        wire::decode(&key, Some(&test_dir().swapped().opener()), &buf[..n])
            .ok()
            .map(|p| p.msg)
    }

    #[test]
    fn a_stale_heartbeat_stops_the_pong_and_a_fresh_one_brings_it_back() {
        let sock = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let dest = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        dest.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let path = dest.local_addr().unwrap();
        let s = shared(Scope::Loopback);
        install(&s, path);
        let mut cache = (0, None);
        let p = &s.pong;
        let counts = || {
            (
                p.sent.load(Ordering::Relaxed),
                p.stale.load(Ordering::Relaxed),
            )
        };
        assert!(answer_ping(&sock, &s, &mut cache, path, &ping()));
        assert_eq!(read_at_peer(&dest), Some(Message::Pong { send_time: 7 }));
        assert_eq!(counts(), (1, 0));
        let old = now_ms(&s).wrapping_sub(liveness::HEARTBEAT_LIMIT_MS);
        for (beat, why) in [(old, "古い心拍"), (0, "まだ書いていない心拍")] {
            p.beat.store(beat, Ordering::Release);
            assert!(!answer_ping(&sock, &s, &mut cache, path, &ping()), "{why}");
        }
        assert_eq!(counts(), (1, 2));
        p.beat.store(now_ms(&s).max(1), Ordering::Release);
        assert!(answer_ping(&sock, &s, &mut cache, path, &ping()));
        assert_eq!(counts(), (2, 2));
        assert_eq!(read_at_peer(&dest), Some(Message::Pong { send_time: 7 }));
    }

    #[test]
    fn after_the_game_thread_stops_pings_get_a_bye_and_one_bye_goes_out_unasked() {
        let sock = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let dest = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        dest.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let path = dest.local_addr().unwrap();
        let s = shared(Scope::Loopback);
        install(&s, path);
        let mut cache = (0, None);
        let mut done = false;
        // 立てる前は何も出さない（下の BYE がいつも出る形でないことの隣）。
        bye_once(&sock, &s, &mut cache, &mut done);
        assert!(!done);
        assert_eq!(read_at_peer(&dest), None);
        s.pong.on.store(false, Ordering::Release);
        s.pong
            .bye
            .store(bye_code(ByeReason::Cancelled), Ordering::Release);
        // 心拍が古くても BYE は返す（止まった後こそ返す）。
        s.pong.beat.store(0, Ordering::Release);
        assert!(answer_ping(&sock, &s, &mut cache, path, &ping()));
        assert_eq!(
            read_at_peer(&dest),
            Some(Message::Bye(ByeReason::Cancelled))
        );
        bye_once(&sock, &s, &mut cache, &mut done);
        bye_once(&sock, &s, &mut cache, &mut done);
        assert_eq!(
            read_at_peer(&dest),
            Some(Message::Bye(ByeReason::Cancelled))
        );
        assert_eq!(read_at_peer(&dest), None, "頼まれない BYE が 2 通出た");
        let p = &s.pong;
        assert_eq!(
            (
                p.byes_sent.load(Ordering::Relaxed),
                p.sent.load(Ordering::Relaxed)
            ),
            (2, 0)
        );
        for r in [
            ByeReason::Finished,
            ByeReason::Disconnected,
            ByeReason::Desync,
            ByeReason::Cancelled,
        ] {
            assert_eq!(bye_of(bye_code(r)), Some(r));
        }
        assert_eq!(bye_of(0), None);
    }

    #[test]
    fn say_bye_makes_the_receive_thread_send_a_bye_without_any_ping() {
        let dest = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        dest.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let path = dest.local_addr().unwrap();
        let (_net, th) = open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        install(&th.shared, path);
        th.say_bye(ByeReason::Cancelled);
        th.say_bye(ByeReason::Desync);
        assert_eq!(
            read_at_peer(&dest),
            Some(Message::Bye(ByeReason::Cancelled)),
            "最初の理由で出ない"
        );
        assert!(!th.shared.pong.on.load(Ordering::Acquire));
        // 数えは送った後に足すので、相手が受けた直後はまだ 0 のことがある。
        let t = std::time::Instant::now();
        while th.stats().byes_sent == 0 && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(th.stats().byes_sent, 1);
    }
}

/// 開くときの UPnP と回線の型の印（`upnp_begin` / `nat_settle`）。偽のルーターは 2 つの返事（記述・SOAP）を作文で返すだけ。
#[cfg(test)]
mod nat_tests {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Mutex;

    use higanr_core::nat::{NatMark, Symmetry};
    use higanr_core::upnp::{HttpResponse, UpnpError, Url};

    use super::*;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Maps,
        Refuses,
        Silent,
    }

    /// 偽のルーター。数えるものは持ち手（`Arc`）で検査の側へ見せる。
    struct Quick {
        mode: Mode,
        /// GetExternalIPAddress の答え（None は断る）。
        outer: Option<Ipv4Addr>,
        adds: Arc<AtomicU32>,
        deletes: Arc<AtomicU32>,
        ports: Arc<Mutex<Vec<u16>>>,
    }

    impl upnp::Wire for Quick {
        fn discover(&mut self, _: Ipv4Addr, _: Duration) -> Result<Vec<Url>, UpnpError> {
            Ok(if self.mode == Mode::Silent {
                Vec::new()
            } else {
                vec![Url::parse("http://192.168.0.1:5000/d.xml").unwrap()]
            })
        }

        fn exchange(
            &mut self,
            _: &Url,
            request: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, UpnpError> {
            let req = String::from_utf8_lossy(request).into_owned();
            let ok = |body: &str| {
                Ok(HttpResponse {
                    status: 200,
                    body: body.to_string(),
                })
            };
            if req.starts_with("GET ") {
                return ok(
                    "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/c</controlURL></service></root>",
                );
            }
            if req.contains("#GetSpecificPortMappingEntry") {
                return Ok(HttpResponse {
                    status: 500,
                    body: "<errorCode>714</errorCode>".to_string(),
                });
            }
            if req.contains("#GetExternalIPAddress") {
                return match self.outer {
                    Some(ip) => ok(&format!(
                        "<NewExternalIPAddress>{ip}</NewExternalIPAddress>"
                    )),
                    None => Ok(HttpResponse {
                        status: 500,
                        body: "<errorCode>501</errorCode>".to_string(),
                    }),
                };
            }
            if req.contains("#AddPortMapping") {
                self.adds.fetch_add(1, Ordering::Relaxed);
                if self.mode == Mode::Refuses {
                    return Ok(HttpResponse {
                        status: 500,
                        body: "<errorCode>501</errorCode>".to_string(),
                    });
                }
                let a = req.find("<NewExternalPort>").unwrap() + "<NewExternalPort>".len();
                let b = a + req[a..].find('<').unwrap();
                self.ports.lock().unwrap().push(req[a..b].parse().unwrap());
                return ok("<u:AddPortMappingResponse/>");
            }
            if req.contains("#DeletePortMapping") {
                self.deletes.fetch_add(1, Ordering::Relaxed);
                return ok("<u:DeletePortMappingResponse/>");
            }
            panic!("知らない要求: {req}");
        }
    }

    struct Counts {
        adds: Arc<AtomicU32>,
        deletes: Arc<AtomicU32>,
        ports: Arc<Mutex<Vec<u16>>>,
    }

    /// 偽のルーターの外側のアドレス（STUN にも同じアドレスが見えている普通の家）。
    const PUBLIC: Ipv4Addr = Ipv4Addr::new(8, 8, 4, 4);

    /// STUN の 1 か所目で見えたアドレス（ポートはルーターが付け替えた値）。
    fn stun_seen(ip: &str) -> Option<SocketAddr> {
        Some(SocketAddr::new(ip.parse().unwrap(), 50_000))
    }

    fn quick(mode: Mode) -> (Box<Quick>, Counts) {
        quick_outer(mode, Some(PUBLIC))
    }

    fn quick_outer(mode: Mode, outer: Option<Ipv4Addr>) -> (Box<Quick>, Counts) {
        let c = Counts {
            adds: Arc::new(AtomicU32::new(0)),
            deletes: Arc::new(AtomicU32::new(0)),
            ports: Arc::new(Mutex::new(Vec::new())),
        };
        (
            Box::new(Quick {
                mode,
                outer,
                adds: Arc::clone(&c.adds),
                deletes: Arc::clone(&c.deletes),
                ports: Arc::clone(&c.ports),
            }),
            c,
        )
    }

    fn settings(extra: &str) -> NetSettings {
        crate::net_config::from_text(&format!(
            "[netplay]\nenable = 1\nbind = auto:47001\ncode_side = offer\n{extra}"
        ))
        .unwrap()
        .unwrap()
    }

    fn home(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 7), port)
    }

    fn net() -> (GameNet, RecvThread) {
        open_for_code(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap()
    }

    #[test]
    fn a_mapping_marks_upnp_gives_its_port_and_closing_the_net_removes_it() {
        let (net, th) = net();
        let (wire, c) = quick(Mode::Maps);
        let mut note = String::new();
        assert!(upnp_begin_with(
            &net,
            wire,
            home(47_001),
            vec![47_001, 50_001],
            upnp::RENEW_EVERY,
            &mut note
        ));
        assert!(note.contains("UPnP"), "{note}");
        let s = settings("scope = internet\n");
        let nat = nat_settle(&s, &net, stun_seen("8.8.4.4"), &mut note);
        assert_eq!(nat.upnp_port, Some(47_001));
        assert_eq!(
            nat.mark,
            NatMark {
                symmetry: Symmetry::Unknown,
                upnp: true
            }
        );
        assert!(
            note.contains("47001") && note.contains("割り当てた"),
            "{note}"
        );
        assert!(
            !note.contains("192.168"),
            "ルーターのアドレスを書いた: {note}"
        );
        let mut net = net;
        assert_eq!(
            net.own_nat(),
            nat.mark,
            "ゲームのスレッドの側も同じ印を読む"
        );
        let stats = th.stop();
        assert_eq!(
            c.deletes.load(Ordering::Relaxed),
            1,
            "ネットワークを閉じても消えない"
        );
        assert_eq!(stats.diag.upnp.state, upnp::State::Mapped);
        assert_eq!(stats.diag.upnp.release, upnp::Release::Done);
        assert_eq!(*c.ports.lock().unwrap(), [47_001]);
        assert!(stats.diag.upnp.text().contains("47001"));
    }

    #[test]
    fn a_refusing_or_silent_router_leaves_upnp_unmarked_and_the_net_still_opens() {
        for mode in [Mode::Refuses, Mode::Silent] {
            let (net, th) = net();
            let (wire, c) = quick(mode);
            let mut note = String::new();
            assert!(upnp_begin_with(
                &net,
                wire,
                home(47_001),
                vec![47_001],
                upnp::RENEW_EVERY,
                &mut note
            ));
            let s = settings("scope = internet\n");
            let t = Instant::now();
            let nat = nat_settle(&s, &net, stun_seen("8.8.4.4"), &mut note);
            assert!(t.elapsed() < upnp::OPEN_BUDGET + Duration::from_secs(1));
            assert_eq!(nat.upnp_port, None);
            assert!(!nat.mark.upnp);
            let mut net = net;
            assert!(!net.own_nat().upnp);
            let want = if mode == Mode::Refuses {
                "断られた"
            } else {
                "応じない"
            };
            assert!(note.contains(want), "{want}: {note}");
            let stats = th.stop();
            assert_eq!(
                c.deletes.load(Ordering::Relaxed),
                0,
                "割り当てていないものを消した"
            );
            assert_eq!(stats.diag.upnp.release, upnp::Release::NotYet);
        }
    }

    #[test]
    fn the_mark_and_the_candidate_come_from_the_same_outer_address_check() {
        use upnp::State::{Mapped, OuterBehind, OuterUnchecked};
        // (ルーターの外側のアドレス, STUN で見えたアドレス, 使うか, 最後の状態)
        let cases = [
            (Some("8.8.4.4"), Some("8.8.4.4:50000"), true, Mapped),
            (Some("8.8.8.8"), Some("8.8.4.4:50000"), false, OuterBehind),
            // 同じでも私設・事業者の中のアドレスは、さらに外側に NAT がある。
            (
                Some("192.168.1.2"),
                Some("192.168.1.2:50000"),
                false,
                OuterBehind,
            ),
            (
                Some("100.64.1.2"),
                Some("100.64.1.2:50000"),
                false,
                OuterBehind,
            ),
            (None, Some("8.8.4.4:50000"), false, OuterUnchecked),
            (Some("8.8.4.4"), None, false, OuterUnchecked),
            (
                Some("8.8.4.4"),
                Some("[2001:4860::1]:50000"),
                false,
                OuterUnchecked,
            ),
        ];
        for (outer, seen, used, state) in cases {
            let (net, th) = net();
            let (wire, c) = quick_outer(Mode::Maps, outer.map(|o| o.parse().unwrap()));
            let mut note = String::new();
            assert!(upnp_begin_with(
                &net,
                wire,
                home(47_001),
                vec![50_001],
                upnp::RENEW_EVERY,
                &mut note
            ));
            let mapped: Option<SocketAddr> = seen.map(|s| s.parse().unwrap());
            let s = settings("scope = internet\n");
            let nat = nat_settle(&s, &net, mapped, &mut note);
            let why = format!("{outer:?} {seen:?}: {note}");
            assert_eq!(nat.upnp_port, used.then_some(50_001), "{why}");
            assert_eq!(nat.mark.upnp, used, "{why}");
            let mut net = net;
            assert_eq!(net.own_nat().upnp, used, "{why}");
            // 印が真のときだけ、割り当てたポートの候補が自分のコードに載る。
            let local = SocketAddr::from(([192, 168, 0, 7], 47_001));
            let cands = s
                .candidates_for_upnp(local, mapped, None, nat.upnp_port)
                .unwrap();
            let has = cands
                .as_slice()
                .iter()
                .any(|c| c.addr == SocketAddr::new(PUBLIC.into(), 50_001));
            assert_eq!(has, used, "印と候補が食い違う: {why}");
            if !used {
                assert!(note.contains("使えない"), "{why}");
                assert!(!note.contains("8.8."), "アドレスを書いた: {why}");
            }
            let stats = th.stop();
            assert_eq!(stats.diag.upnp.state, state, "{why}");
            // 使わない割り当ても、足したものはネットワークを閉じるまでに消える。
            assert_eq!(c.deletes.load(Ordering::Relaxed), 1, "{why}");
            assert_eq!(stats.diag.upnp.release, upnp::Release::Done, "{why}");
        }
    }

    #[test]
    fn upnp_is_asked_only_for_the_internet_scope_with_a_home_ipv4_source() {
        // scope = loopback（既定）: 頼まない・何も書かない。
        let (net, th) = net();
        let mut note = String::new();
        assert!(!upnp_begin(&settings(""), &net, &mut note));
        assert_eq!(note, "");
        assert!(net.upnp_outcome(Duration::from_millis(1)).is_none());
        // scope = internet でも、ソケットが loopback（家の中のアドレスでない）なら頼まない。
        assert!(!upnp_begin(
            &settings("scope = internet\n"),
            &net,
            &mut note
        ));
        assert!(note.contains("家の中"), "{note}");
        // ini の upnp = off。
        let mut note = String::new();
        assert!(!upnp_begin(
            &settings("scope = internet\nupnp = off\n"),
            &net,
            &mut note
        ));
        assert!(note.contains("upnp = off"), "{note}");
        // 頼まなかった実行の印: UPnP なし・型は不明。
        let nat = nat_settle(&settings(""), &net, None, &mut String::new());
        assert_eq!(nat, OwnNat::default());
        assert_eq!(th.stop().diag.upnp.state, upnp::State::NotTried);
        assert!(asks_upnp(Scope::Internet));
        assert!(!asks_upnp(Scope::Lan) && !asks_upnp(Scope::Loopback));
    }

    #[test]
    fn a_pending_second_stun_is_waited_for_up_to_the_cap_and_then_the_type_is_unknown() {
        // 途中で結果が出る: 待つ。
        let (net, th) = net();
        net.shared
            .diag
            .second
            .store(SecondStun::Pending.code(), Ordering::Relaxed);
        let shared = Arc::clone(&net.shared);
        let flip = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            shared.diag.second.store(
                SecondStun::Compared(stun::Mapping::PortOnly).code(),
                Ordering::Relaxed,
            );
        });
        let mut note = String::new();
        let t = Instant::now();
        let nat = nat_settle(&settings(""), &net, None, &mut note);
        assert!(t.elapsed() >= Duration::from_millis(100), "待たずに進んだ");
        assert!(t.elapsed() < SECOND_STUN_WAIT, "上限まで待った");
        assert_eq!(nat.mark.symmetry, Symmetry::Yes);
        assert!(note.contains("結果が出た"), "{note}");
        flip.join().unwrap();
        // 出ないまま: 上限で進む。型は不明。待った時間が本ログに出る。
        net.shared
            .diag
            .second
            .store(SecondStun::Pending.code(), Ordering::Relaxed);
        let mut note = String::new();
        let t = Instant::now();
        let nat = nat_settle(&settings(""), &net, None, &mut note);
        assert!(t.elapsed() >= SECOND_STUN_WAIT);
        assert!(t.elapsed() < SECOND_STUN_WAIT + Duration::from_millis(900));
        assert_eq!(nat.mark.symmetry, Symmetry::Unknown);
        assert!(
            note.contains("上限") && note.contains(" ms 待った"),
            "{note}"
        );
        // 聞いていない（NotAsked）なら待たない。
        net.shared
            .diag
            .second
            .store(SecondStun::NotAsked.code(), Ordering::Relaxed);
        let t = Instant::now();
        nat_settle(&settings(""), &net, None, &mut String::new());
        assert!(t.elapsed() < Duration::from_millis(100));
        drop(th);
    }

    #[test]
    fn own_nat_reads_the_second_stun_live_for_each_comparison() {
        let (mut net, _th) = net();
        let want = [
            (SecondStun::NotAsked, Symmetry::Unknown),
            (SecondStun::NoAnswer, Symmetry::Unknown),
            (SecondStun::Compared(stun::Mapping::Same), Symmetry::No),
            (SecondStun::Compared(stun::Mapping::PortOnly), Symmetry::Yes),
            (SecondStun::Compared(stun::Mapping::AddrOnly), Symmetry::Yes),
            (SecondStun::Compared(stun::Mapping::Both), Symmetry::Yes),
        ];
        for (second, sym) in want {
            net.shared
                .diag
                .second
                .store(second.code(), Ordering::Relaxed);
            assert_eq!(net.own_nat().symmetry, sym, "{second:?}");
            assert!(!net.own_nat().upnp);
        }
    }
}
