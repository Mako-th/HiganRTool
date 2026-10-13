//! UPnP（IGD）でソケットの UDP ポートの割り当てを家のルーターに頼む（dll の側。文面の読み書きは `higanr_core::upnp`）。
//!
//! ネットワークを開くとき別のスレッド（`Handle::spawn`）で、SSDP の探索 → 機器の記述 → AddPortMapping の順に問い合わせ、割り当てた
//! 外側のポートを自分の候補に載せる（`NetSettings::candidates_for_upnp`）。開く側は上限の時間（`OPEN_BUDGET`）だけ待ち、
//! 過ぎたら「使用不可」として進む（遅れて割り当てられても、誰も使わないので自分で消す）。割り当ては期限つき
//! （`LEASE_SECS`）で、スレッドは期限の半分ごとに頼み直し、止められたら DeletePortMapping で消す。プロセスごと落ちても期限で消える。
//! 期限つきを 725（期限なししか受けない）で断られたら、期限なし（リース 0）で 1 回だけ頼み直す。期限なしは頼み直さず、落ちると
//! 残るので、説明（`MAPPING_DESCRIPTION`）を印にして次の起動が引き継ぐ。
//! 足す前に GetSpecificPortMappingEntry で同じ外側のポートを引く。この PC・このソケット向けで期限なしかつ説明が印でない
//! （手で開けた形）なら足さず、頼み直しも削除もしない。期限つき、または期限なしで説明が印のものは前の起動の消し残りと見て
//! 自分のものにする（期限つきは足し直して期限を延ばす。どちらも閉じるとき消す）。
//! 足した後はルーターの外側のアドレスを引き、STUN で見えたアドレスと突き合わせるのは
//! 開く側（`net_udp::nat_settle`）。
//!
//! 通信は LAN の中だけ（SSDP はマルチキャスト TTL 2、HTTP の宛先は LAN のアドレスに限る。`higanr_core::upnp::Url`）。ゲームのスレッドでは
//! 呼ばない（ブロッキング I/O）。本ログに出すのは結果とポートだけで、ルーターのアドレスは書かない。
#![forbid(unsafe_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use higanr_core::guard;
use higanr_core::upnp::{
    self as proto, FAULT_CONFLICT, FAULT_NO_SUCH_ENTRY, FAULT_ONLY_PERMANENT, HttpResponse,
    MAPPING_DESCRIPTION, MAX_RESPONSE, Service, UpnpError, Url,
};

use crate::net_win;

/// 割り当ての期限（秒）。消し忘れ（プロセスの異常終了）でもこの時間で消える。
pub const LEASE_SECS: u32 = 3600;
/// 頼み直す間隔。期限の半分。
pub const RENEW_EVERY: Duration = Duration::from_secs(1800);
/// 頼み直しに失敗したとき、次に頼み直すまでの間隔。通常の間隔のままだと、次は期限ちょうどになる。
pub const RETRY_AFTER_FAIL: Duration = Duration::from_secs(60);
/// ネットワークを開く側が割り当てを待つ上限。
pub const OPEN_BUDGET: Duration = Duration::from_secs(2);
/// SSDP の返事を待つ上限。
const DISCOVER_BUDGET: Duration = Duration::from_millis(1200);
/// HTTP 1 往復の上限（LAN の機器なので短い）。
const HTTP_TIMEOUT: Duration = Duration::from_secs(1);
/// 外側のポートが他の割り当てと重なったとき、頼み直す別のポートの数。
pub const ALT_PORTS: usize = 3;

/// 通信の道具。本物は std の UDP / TCP（`NetWire`）、検査は偽のルーター。
pub trait Wire: Send {
    /// M-SEARCH を `local` のソケットから出し、返事のあった機器の記述の URL を返す（最初の返事で返してよい）。返事が無ければ空。
    fn discover(
        &mut self,
        local: std::net::Ipv4Addr,
        budget: Duration,
    ) -> Result<Vec<Url>, UpnpError>;
    /// HTTP の 1 往復。`request` を `to` へ送り、返事を読む。
    fn exchange(
        &mut self,
        to: &Url,
        request: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, UpnpError>;
}

fn io_err(e: std::io::Error) -> UpnpError {
    UpnpError::Io(e.to_string())
}

pub struct NetWire;

/// SSDP が答えなかったソケットの記録（0 は無し）。答えない機器に、繋ぎ直すたびに探索の上限まで待たないための、プロセスの中の記憶。
/// ソケットのアドレスごとに覚えるので、別のネットワークへ移れば探し直す。誤りと断りは覚えない。
struct SsdpMemo(AtomicU32);

static SSDP_SILENT: SsdpMemo = SsdpMemo(AtomicU32::new(0));

impl SsdpMemo {
    fn run(
        &self,
        local: Ipv4Addr,
        search: impl FnOnce() -> Result<Vec<Url>, UpnpError>,
    ) -> Result<Vec<Url>, UpnpError> {
        let key = u32::from(local);
        if key != 0 && self.0.load(Ordering::Relaxed) == key {
            return Ok(Vec::new());
        }
        let r = search();
        if key != 0 && matches!(&r, Ok(v) if v.is_empty()) {
            self.0.store(key, Ordering::Relaxed);
        }
        r
    }
}

impl NetWire {
    /// `dest` へ M-SEARCH を出し、返事を待つ。本番の `dest` は SSDP のマルチキャスト（検査は loopback の偽の機器）。
    fn search(
        local: Ipv4Addr,
        budget: Duration,
        dest: SocketAddrV4,
    ) -> Result<Vec<Url>, UpnpError> {
        let sock = UdpSocket::bind(SocketAddrV4::new(local, 0)).map_err(io_err)?;
        net_win::set_multicast_if(&sock, local).map_err(io_err)?;
        sock.set_multicast_ttl_v4(2).map_err(io_err)?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(io_err)?;
        let start = Instant::now();
        let mut sent = 0u32;
        let mut buf = [0u8; 2048];
        while start.elapsed() < budget {
            // 取りこぼしに備え、0 ms と 500 ms の 2 回送る。
            if sent < 2 && start.elapsed() >= Duration::from_millis(500) * sent {
                for t in proto::SEARCH_TARGETS {
                    let _ = sock.send_to(&proto::search_request(t), dest);
                }
                sent += 1;
            }
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    // 送り手と LOCATION のアドレスが違う返事は、別の機器へ誘導する形なので使わない。
                    if let Ok(url) =
                        proto::parse_search_reply_from(buf.get(..n).unwrap_or(&[]), from.ip())
                    {
                        return Ok(vec![url]);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset
                    ) => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(Vec::new())
    }
}

impl Wire for NetWire {
    fn discover(
        &mut self,
        local: std::net::Ipv4Addr,
        budget: Duration,
    ) -> Result<Vec<Url>, UpnpError> {
        let dest = SocketAddrV4::new(proto::SSDP_ADDR, proto::SSDP_PORT);
        SSDP_SILENT.run(local, || NetWire::search(local, budget, dest))
    }

    fn exchange(
        &mut self,
        to: &Url,
        request: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, UpnpError> {
        let addr = SocketAddr::V4(SocketAddrV4::new(to.host, to.port));
        let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(io_err)?;
        s.set_read_timeout(Some(timeout)).map_err(io_err)?;
        s.set_write_timeout(Some(timeout)).map_err(io_err)?;
        s.write_all(request).map_err(io_err)?;
        let deadline = Instant::now() + timeout * 2;
        let cap = MAX_RESPONSE + 4096;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                    if buf.len() > cap {
                        return Err(UpnpError::Malformed("返事が大きすぎる"));
                    }
                }
                // 閉じない機器でも、読めたところまでで読む（Content-Length が足りていなければ読みで断る）。
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if buf.is_empty() {
                        return Err(io_err(e));
                    }
                    break;
                }
                Err(e) => return Err(io_err(e)),
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        proto::parse_http_response(&buf)
    }
}

/// 一連の問い合わせの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// SSDP に答える機器が無い（UPnP がオフ・対応していない・経路が違う）。
    NoAnswer,
    /// 答えたが、WAN の接続のサービスが読めない。
    NoService(UpnpError),
    /// 割り当てを断られた（errorCode つき）。
    Refused(UpnpError),
    /// 通信の誤り。
    Failed(UpnpError),
    /// 待つ上限を過ぎた（開く側が諦めた）。
    TimedOut,
    Mapped {
        external: u16,
        /// ルーターが答えた外側のアドレス。引けなければ None（STUN のアドレスとの突き合わせができない）。
        outer_ip: Option<Ipv4Addr>,
    },
}

/// 持っている割り当ての期限の形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lease {
    /// 期限つき。期限の半分ごとに頼み直す。
    #[default]
    Timed,
    /// 期限つきを 725 で断られ、期限なしで頼み直した。頼み直しは要らない。
    Permanent,
    /// 前の起動が作った期限なしの割り当て（説明が印）を引き継いだ。頼み直しは要らない。
    Inherited,
}

/// 割り当てが済んだ状態（頼み直しと削除に要る）。
#[derive(Debug, Clone)]
pub struct Established {
    pub service: Service,
    pub external: u16,
    pub outer_ip: Option<Ipv4Addr>,
    pub lease: Lease,
    /// 足す前から、この PC・このソケット向けの期限なしの割り当てがあった（手で開けた形）。足していないので、頼み直しも削除もしない。
    pub already_open: bool,
}

fn add_mapping(
    w: &mut dyn Wire,
    svc: &Service,
    external: u16,
    internal: SocketAddrV4,
    lease: u32,
) -> Result<(), UpnpError> {
    let args = proto::add_mapping_args(external, internal.port(), *internal.ip(), lease);
    let req = proto::soap_request(svc, "AddPortMapping", &args);
    let resp = w.exchange(&svc.control, &req, HTTP_TIMEOUT)?;
    proto::soap_result(&resp).map(|_| ())
}

/// `lease` で頼み、725 なら期限なしで 1 回だけ頼み直す。
fn add_mapping_or_permanent(
    w: &mut dyn Wire,
    svc: &Service,
    external: u16,
    internal: SocketAddrV4,
    lease: u32,
) -> Result<Lease, UpnpError> {
    match add_mapping(w, svc, external, internal, lease) {
        Ok(()) if lease == 0 => Ok(Lease::Permanent),
        Ok(()) => Ok(Lease::Timed),
        Err(e) if lease != 0 && e.fault_code() == Some(FAULT_ONLY_PERMANENT) => {
            add_mapping(w, svc, external, internal, 0).map(|()| Lease::Permanent)
        }
        Err(e) => Err(e),
    }
}

pub fn get_mapping(
    w: &mut dyn Wire,
    svc: &Service,
    external: u16,
) -> Result<proto::Entry, UpnpError> {
    let req = proto::soap_request(
        svc,
        "GetSpecificPortMappingEntry",
        &proto::mapping_key_args(external),
    );
    let resp = w.exchange(&svc.control, &req, HTTP_TIMEOUT)?;
    proto::parse_entry(proto::soap_result(&resp)?)
}

/// ルーターの外側のアドレス（GetExternalIPAddress）。
pub fn get_external_ip(w: &mut dyn Wire, svc: &Service) -> Result<Ipv4Addr, UpnpError> {
    let req = proto::soap_request(svc, "GetExternalIPAddress", &[]);
    let resp = w.exchange(&svc.control, &req, HTTP_TIMEOUT)?;
    proto::parse_external_ip(proto::soap_result(&resp)?)
}

/// `external` に既にある割り当ての見立て。
enum Existing {
    /// 無い（足してよい）。
    Free,
    /// この PC・このソケット向けの有効なもので、期限なしで説明が印でない（手で開けた形）。
    Ours,
    /// この PC・このソケット向けの有効なもので、期限なしで説明が印（前の起動の消し残り）。
    Leftover,
    /// ほかの機器・ほかのポート向け（読めない答えも安全側でこちら）。
    Other,
}

fn existing_mapping(
    w: &mut dyn Wire,
    svc: &Service,
    external: u16,
    local: SocketAddrV4,
) -> Result<Existing, UpnpError> {
    match get_mapping(w, svc, external) {
        Ok(e) => {
            let mine = e.internal_port == local.port()
                && e.internal_client.trim() == local.ip().to_string();
            Ok(match (mine, e.enabled) {
                // 期限つきは前の起動が消し損ねたもの。足し直して延ばす。
                // 期限が読めない答えは 0 になり、触らない側に倒れる。
                (true, true)
                    if e.lease_secs == 0 && e.description.trim() == MAPPING_DESCRIPTION =>
                {
                    Existing::Leftover
                }
                (true, true) if e.lease_secs == 0 => Existing::Ours,
                // 自分向けの無効な割り当ては、足して上書きしてよい。
                (true, _) => Existing::Free,
                (false, _) => Existing::Other,
            })
        }
        // 無いときの答え（714 が普通。機器により違う）。足す側が重なりを断れば、そこで分かる。
        Err(UpnpError::Fault { .. }) => Ok(Existing::Free),
        Err(e @ UpnpError::Io(_)) => Err(e),
        Err(_) => Ok(Existing::Other),
    }
}

pub fn delete_mapping(w: &mut dyn Wire, svc: &Service, external: u16) -> Result<(), UpnpError> {
    let req = proto::soap_request(svc, "DeletePortMapping", &proto::mapping_key_args(external));
    let resp = w.exchange(&svc.control, &req, HTTP_TIMEOUT)?;
    proto::soap_result(&resp).map(|_| ())
}

/// 機器を探し、機器の記述から WAN のサービスを選ぶ。
pub fn find_service(
    w: &mut dyn Wire,
    local: std::net::Ipv4Addr,
    budget: Duration,
) -> Result<Service, Outcome> {
    let locations = w.discover(local, budget).map_err(Outcome::Failed)?;
    if locations.is_empty() {
        return Err(Outcome::NoAnswer);
    }
    let mut why = UpnpError::Malformed("機器の記述を取れない");
    for loc in locations.iter().take(2) {
        let resp = match w.exchange(loc, &proto::get_request(loc), HTTP_TIMEOUT) {
            Ok(r) => r,
            Err(e) => {
                why = e;
                continue;
            }
        };
        if resp.status != 200 {
            why = UpnpError::Fault {
                status: resp.status,
                code: None,
            };
            continue;
        }
        match proto::find_wan_service(&resp.body, loc) {
            Ok(svc) => return Ok(svc),
            Err(e) => why = e,
        }
    }
    Err(Outcome::NoService(why))
}

/// 探して、`ports` の順に割り当てる（重なり 718 か、ほかの機器向けの割り当てがあるときだけ次のポート）。`ports` の先頭は
/// 内側と同じポート。足した後にルーターの外側のアドレスを引く（引けなくても割り当ては成り立つ。判定は呼び手）。
pub fn establish(
    w: &mut dyn Wire,
    local: SocketAddrV4,
    ports: &[u16],
    lease: u32,
) -> Result<Established, Outcome> {
    let service = find_service(w, *local.ip(), DISCOVER_BUDGET)?;
    let mut last = Outcome::Refused(UpnpError::Malformed("試すポートが無い"));
    for &external in ports {
        let (already_open, adopted) = match existing_mapping(w, &service, external, local) {
            Ok(Existing::Free) => (false, false),
            Ok(Existing::Ours) => (true, false),
            Ok(Existing::Leftover) => (false, true),
            Ok(Existing::Other) => {
                // 重なりと同じ扱い（機器は 718 で断るはずのもの）。
                last = Outcome::Refused(UpnpError::Fault {
                    status: 500,
                    code: Some(FAULT_CONFLICT),
                });
                continue;
            }
            Err(e) => return Err(Outcome::Failed(e)),
        };
        let added = if already_open {
            Ok(Lease::Timed)
        } else if adopted {
            Ok(Lease::Inherited)
        } else {
            add_mapping_or_permanent(w, &service, external, local, lease)
        };
        match added {
            Ok(lease) => {
                let outer_ip = get_external_ip(w, &service).ok();
                return Ok(Established {
                    service,
                    external,
                    outer_ip,
                    lease,
                    already_open,
                });
            }
            Err(e @ UpnpError::Fault { .. }) => {
                let conflict = e.fault_code() == Some(FAULT_CONFLICT);
                last = Outcome::Refused(e);
                if !conflict {
                    break;
                }
            }
            Err(e) => return Err(Outcome::Failed(e)),
        }
    }
    Err(last)
}

/// 割り当ての後始末の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Release {
    #[default]
    NotYet,
    /// DeletePortMapping が通った。
    Done,
    /// 機器に割り当てが無かった（714。期限で消えたか、ほかが消した）。
    Gone,
    Failed,
    /// 足したものでない（もとからあった割り当て）ので消さなかった。
    Kept,
}

impl Release {
    fn text(self) -> &'static str {
        match self {
            Release::NotYet => "まだ",
            Release::Done => "割り当てを消した",
            Release::Gone => "割り当てはもう無かった",
            Release::Failed => "消せなかった（期限で消える）",
            Release::Kept => "もとからあった割り当てなので消さない",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// 頼んでいない（ini の `upnp = off`・scope が internet でない・ソケットが LAN のアドレスでない）。
    #[default]
    NotTried,
    Working,
    NoAnswer,
    NoService,
    Refused,
    Failed,
    TimedOut,
    Mapped,
    /// 割り当てたが、ルーターの外側のアドレスが STUN で見えたアドレスと違う（または世界に通じない）ので使わない。
    OuterBehind,
    /// 割り当てたが、外側のアドレスを突き合わせられない（ルーターが答えない・STUN の IPv4 のアドレスが無い）ので使わない。
    OuterUnchecked,
}

/// 本ログ・診断に出す数（アドレスは持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub state: State,
    pub external_port: u16,
    /// 断られたときの HTTP の状態と errorCode（無ければ 0）。
    pub http_status: u16,
    pub fault_code: u32,
    /// 頼み始めてから結果が出るまで（ms）。
    pub took_ms: u32,
    pub renewed: u32,
    pub renew_failed: u32,
    pub release: Release,
    /// 足す前から同じ割り当てがあった（足していない）。
    pub already_open: bool,
    pub lease: Lease,
}

impl Stats {
    pub fn text(&self) -> String {
        match self.state {
            State::NotTried => {
                "UPnP: 使っていない（ini の upnp = off・scope が internet でない・受信ソケットが家の中のアドレスでないのどれか）".to_string()
            }
            State::Working => "UPnP: 問い合わせの途中".to_string(),
            State::NoAnswer => "UPnP: 応じない（ルーターから SSDP の返事が無い。ルーターの UPnP がオフか、対応していない）".to_string(),
            State::NoService => format!(
                "UPnP: 応じたが使えない（機器の記述から WAN の接続のサービスを読めない・HTTP {}）",
                self.http_status
            ),
            State::Refused => format!(
                "UPnP: 断られた（HTTP {}・errorCode {}）",
                self.http_status, self.fault_code
            ),
            State::Failed => "UPnP: 通信の誤りで割り当てられなかった".to_string(),
            State::TimedOut => format!(
                "UPnP: {} 秒の上限内に終わらなかったので使わない",
                OPEN_BUDGET.as_secs()
            ),
            State::Mapped if self.already_open => format!(
                "UPnP: 外側のポート {} はもう開いている（{} ms。足していない・頼み直さない・後始末: {}）",
                self.external_port,
                self.took_ms,
                self.release.text()
            ),
            State::Mapped => format!(
                "UPnP: 外側のポート {} を割り当てた（{} ms。{}・後始末: {}）",
                self.external_port,
                self.took_ms,
                match self.lease {
                    Lease::Timed => format!(
                        "期限つき・頼み直し {} 回（失敗 {}）",
                        self.renewed, self.renew_failed
                    ),
                    Lease::Permanent => "期限なしで頼み直した".to_string(),
                    Lease::Inherited => "前の起動の期限なしの割り当てを引き継いだ".to_string(),
                },
                self.release.text()
            ),
            State::OuterBehind => format!(
                "UPnP: 使えない（ルーターの外側のアドレスが STUN で見えたアドレスと違うか、世界に通じないアドレス＝二重 NAT の見込み。割り当ては取り下げた・後始末: {}）",
                self.release.text()
            ),
            State::OuterUnchecked => format!(
                "UPnP: 使えない（ルーターの外側のアドレスを STUN のアドレスと突き合わせられない。割り当ては取り下げた・後始末: {}）",
                self.release.text()
            ),
        }
    }
}

fn stats_of(o: &Outcome, took: Duration) -> Stats {
    let took_ms = u32::try_from(took.as_millis()).unwrap_or(u32::MAX);
    let mut s = Stats {
        took_ms,
        ..Stats::default()
    };
    let fault = |e: &UpnpError, s: &mut Stats| {
        if let UpnpError::Fault { status, code } = e {
            s.http_status = *status;
            s.fault_code = code.unwrap_or(0);
        }
    };
    match o {
        Outcome::NoAnswer => s.state = State::NoAnswer,
        Outcome::NoService(e) => {
            s.state = State::NoService;
            fault(e, &mut s);
        }
        Outcome::Refused(e) => {
            s.state = State::Refused;
            fault(e, &mut s);
        }
        Outcome::Failed(_) => s.state = State::Failed,
        Outcome::TimedOut => s.state = State::TimedOut,
        Outcome::Mapped { external, .. } => {
            s.state = State::Mapped;
            s.external_port = *external;
        }
    }
    s
}

struct Inner {
    outcome: Option<Outcome>,
    stats: Stats,
    stop: bool,
    /// 開く側が待つのをやめた。後から割り当てられても使われないので、その場で消す。
    abandoned: bool,
}

struct Shared {
    m: Mutex<Inner>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 別のスレッドで割り当てを頼み、持ち続け、止められたら消す持ち手。
pub struct Handle {
    shared: Arc<Shared>,
    join: Mutex<Option<JoinHandle<()>>>,
    started: Instant,
}

impl Handle {
    /// 頼み始める。`ports` の先頭が内側と同じポート（重なったとき順に別のポート）。`renew` ごとに頼み直す
    /// （失敗したら `RETRY_AFTER_FAIL` 後に頼み直す）。
    pub fn spawn(
        wire: Box<dyn Wire>,
        local: SocketAddrV4,
        ports: Vec<u16>,
        lease_secs: u32,
        renew: Duration,
    ) -> std::io::Result<Handle> {
        Self::spawn_with_retry(wire, local, ports, lease_secs, renew, RETRY_AFTER_FAIL)
    }

    /// `spawn` の中身（頼み直しの失敗後の間隔を渡す。検査は短くする）。
    pub fn spawn_with_retry(
        wire: Box<dyn Wire>,
        local: SocketAddrV4,
        ports: Vec<u16>,
        lease_secs: u32,
        renew: Duration,
        retry: Duration,
    ) -> std::io::Result<Handle> {
        let shared = Arc::new(Shared {
            m: Mutex::new(Inner {
                outcome: None,
                stats: Stats {
                    state: State::Working,
                    ..Stats::default()
                },
                stop: false,
                abandoned: false,
            }),
            cv: Condvar::new(),
        });
        let started = Instant::now();
        let join = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("higanr-upnp".into())
                .spawn(move || {
                    // パニックは受けて外へ出さない（スレッドの入口）。受けたら失敗として待つ側を起こす。
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        let plan = Plan {
                            local,
                            lease: lease_secs,
                            renew,
                            retry,
                        };
                        worker(wire, &shared, &ports, &plan, started);
                    }));
                    if r.is_err() {
                        let mut g = shared.lock();
                        if g.outcome.is_none() {
                            let o = Outcome::Failed(UpnpError::Malformed("内部の誤り"));
                            g.stats = stats_of(&o, started.elapsed());
                            g.outcome = Some(o);
                        }
                        shared.cv.notify_all();
                    }
                })?
        };
        Ok(Handle {
            shared,
            join: Mutex::new(Some(join)),
            started,
        })
    }

    /// 頼み始めてから `total` 経つまで、結果を待つ。出なければ待つのをやめて `TimedOut` を返す（後から割り当てられたら自分で消す）。
    pub fn wait_until(&self, total: Duration) -> Outcome {
        let left = total.saturating_sub(self.started.elapsed());
        let g = self.shared.lock();
        let (mut g, _) = self
            .shared
            .cv
            .wait_timeout_while(g, left, |i| i.outcome.is_none())
            .unwrap_or_else(|e| e.into_inner());
        if let Some(o) = &g.outcome {
            return o.clone();
        }
        g.abandoned = true;
        g.stats = stats_of(&Outcome::TimedOut, self.started.elapsed());
        Outcome::TimedOut
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().stats
    }

    /// 割り当てられたが使わないと決めた（理由は `why`）。待たずに、スレッドへ消すよう知らせる（消すのはスレッドで、
    /// 済むのは `stop` で待ったとき）。
    pub fn discard(&self, why: State) {
        {
            let mut g = self.shared.lock();
            g.stop = true;
            g.stats.state = why;
        }
        self.shared.cv.notify_all();
    }

    /// 止めて待つ（割り当てを消す往復が終わるまで。機器の応答が無くても `HTTP_TIMEOUT` の数倍で戻る）。何度呼んでもよい。
    pub fn stop(&self) {
        {
            let mut g = self.shared.lock();
            g.stop = true;
        }
        self.shared.cv.notify_all();
        let j = self.join.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(j) = j {
            let _ = j.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn release_of(r: Result<(), UpnpError>) -> Release {
    match r {
        Ok(()) => Release::Done,
        Err(e) if e.fault_code() == Some(FAULT_NO_SUCH_ENTRY) => Release::Gone,
        Err(_) => Release::Failed,
    }
}

/// 頼み続けるときの取り決め。
struct Plan {
    local: SocketAddrV4,
    lease: u32,
    renew: Duration,
    /// 頼み直しに失敗した後の間隔の上限。
    retry: Duration,
}

/// 次に頼み直すまでの間隔。失敗の後は通常の間隔より短くする（通常の間隔だと、次は期限ちょうどになる）。
fn next_wait(renew: Duration, retry: Duration, last_ok: bool) -> Duration {
    if last_ok { renew } else { renew.min(retry) }
}

fn worker(mut w: Box<dyn Wire>, shared: &Shared, ports: &[u16], plan: &Plan, started: Instant) {
    let Plan {
        local,
        lease,
        renew,
        retry,
    } = *plan;
    let est = match establish(&mut *w, local, ports, lease) {
        Ok(e) => e,
        Err(o) => {
            let mut g = shared.lock();
            g.stats = stats_of(&o, started.elapsed());
            if !g.abandoned {
                g.outcome = Some(o);
            }
            shared.cv.notify_all();
            return;
        }
    };
    // 消すのは自分が足したものだけ。
    let finish = |w: &mut dyn Wire| {
        if est.already_open {
            Release::Kept
        } else {
            release_of(delete_mapping(w, &est.service, est.external))
        }
    };
    let mut g = shared.lock();
    if g.abandoned || g.stop {
        // 誰も待っていない。割り当てを使わないので、その場で消す。
        drop(g);
        let r = finish(&mut *w);
        let mut g = shared.lock();
        g.stats = stats_of(&Outcome::TimedOut, started.elapsed());
        g.stats.release = r;
        shared.cv.notify_all();
        return;
    }
    let o = Outcome::Mapped {
        external: est.external,
        outer_ip: est.outer_ip,
    };
    g.stats = stats_of(&o, started.elapsed());
    g.stats.already_open = est.already_open;
    g.stats.lease = est.lease;
    g.outcome = Some(o);
    shared.cv.notify_all();
    let mut wait = renew;
    loop {
        let (g2, timeout) = shared
            .cv
            .wait_timeout_while(g, wait, |i| !i.stop)
            .unwrap_or_else(|e| e.into_inner());
        g = g2;
        if g.stop {
            break;
        }
        if timeout.timed_out() && !est.already_open && est.lease == Lease::Timed {
            // 通信の間は錠を持たない。
            drop(g);
            let r = add_mapping(&mut *w, &est.service, est.external, local, lease);
            g = shared.lock();
            if r.is_ok() {
                g.stats.renewed = g.stats.renewed.saturating_add(1);
            } else {
                g.stats.renew_failed = g.stats.renew_failed.saturating_add(1);
            }
            wait = next_wait(renew, retry, r.is_ok());
        }
    }
    drop(g);
    let r = finish(&mut *w);
    shared.lock().stats.release = r;
}

/// ソケットのアドレスが UPnP を頼む対象か。家の中の IPv4 のアドレス（private / link-local）だけ。
pub fn lan_source(local: SocketAddr) -> Option<SocketAddrV4> {
    match local {
        SocketAddr::V4(a) if guard::is_lan_v4(*a.ip()) => Some(a),
        _ => None,
    }
}

/// 内側のポートと同じ外側のポートを先頭に、重なったときの別のポート（動的ポートの範囲）を `seed` から作る。
pub fn port_order(internal: u16, seed: [u8; 2 * ALT_PORTS]) -> Vec<u16> {
    let mut v = vec![internal];
    for pair in seed.as_chunks::<2>().0 {
        let raw = u16::from_le_bytes(*pair);
        // 49152〜65535（16384 個）。
        let p = 49_152 + raw % 16_384;
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    const LOCAL: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 7), 47001);
    const ROUTER: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 1);
    /// 偽のルーターの外側のアドレス。
    const PUBLIC: Ipv4Addr = Ipv4Addr::new(8, 8, 4, 4);

    #[derive(Default)]
    struct Router {
        reply: bool,
        io_error_on_discover: bool,
        description_status: u16,
        description: String,
        mappings: BTreeMap<u16, (String, u16, u32)>,
        /// 外側のポートごとの説明（入れていなければ空）。
        descriptions: BTreeMap<u16, String>,
        /// AddPortMapping で受けたリース（断ったものも含む）。
        add_leases: Vec<u32>,
        adds: u32,
        gets: u32,
        deletes: u32,
        /// GetExternalIPAddress の答え（None は断る）と、その回数。
        outer_ip: Option<Ipv4Addr>,
        outer_gets: u32,
        conflict: Vec<u16>,
        refuse: Option<(u16, u32)>,
        only_permanent: bool,
        delay: Duration,
    }

    fn description() -> String {
        "<root><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IPConn</controlURL></service></serviceList></device></root>".to_string()
    }

    fn router() -> Arc<Mutex<Router>> {
        Arc::new(Mutex::new(Router {
            reply: true,
            description_status: 200,
            description: description(),
            outer_ip: Some(PUBLIC),
            ..Router::default()
        }))
    }

    fn body_value(req: &str, tag: &str) -> String {
        let a = req
            .find(&format!("<{tag}>"))
            .unwrap_or_else(|| panic!("{tag} が要求に無い"))
            + tag.len()
            + 2;
        let b = a + req[a..].find('<').unwrap();
        req[a..b].to_string()
    }

    fn soap_fault(code: u32) -> HttpResponse {
        HttpResponse {
            status: 500,
            body: format!(
                "<s:Fault><UPnPError><errorCode>{code}</errorCode></UPnPError></s:Fault>"
            ),
        }
    }

    /// 偽のルーター。要求の文面を読み、状態を替えて返す（こちらが作った文面をそのまま読むので、書き方の誤りも落ちる）。
    struct FakeWire(Arc<Mutex<Router>>);

    impl Wire for FakeWire {
        fn discover(&mut self, local: Ipv4Addr, _budget: Duration) -> Result<Vec<Url>, UpnpError> {
            let r = self.0.lock().unwrap();
            assert_eq!(local, *LOCAL.ip(), "問い合わせは家の中のアドレスから出す");
            if r.io_error_on_discover {
                return Err(UpnpError::Io("bind".into()));
            }
            if !r.reply {
                return Ok(Vec::new());
            }
            Ok(vec![
                Url::parse(&format!("http://{ROUTER}:5000/rootDesc.xml")).unwrap(),
            ])
        }

        fn exchange(
            &mut self,
            to: &Url,
            request: &[u8],
            _timeout: Duration,
        ) -> Result<HttpResponse, UpnpError> {
            let delay = self.0.lock().unwrap().delay;
            std::thread::sleep(delay);
            let mut r = self.0.lock().unwrap();
            assert_eq!(to.host, ROUTER);
            let req = String::from_utf8(request.to_vec()).unwrap();
            if req.starts_with("GET /rootDesc.xml") {
                return Ok(HttpResponse {
                    status: r.description_status,
                    body: r.description.clone(),
                });
            }
            assert!(req.starts_with("POST /ctl/IPConn"), "{req}");
            let action = req
                .lines()
                .find_map(|l| l.strip_prefix("SOAPACTION: \""))
                .and_then(|l| l.split('#').nth(1))
                .and_then(|l| l.strip_suffix('"'))
                .unwrap()
                .to_string();
            if action == "GetExternalIPAddress" {
                r.outer_gets += 1;
                return Ok(match r.outer_ip {
                    Some(ip) => HttpResponse {
                        status: 200,
                        body: format!(
                            "<u:GetExternalIPAddressResponse><NewExternalIPAddress>{ip}</NewExternalIPAddress></u:GetExternalIPAddressResponse>"
                        ),
                    },
                    None => soap_fault(501),
                });
            }
            let ext: u16 = body_value(&req, "NewExternalPort").parse().unwrap();
            assert_eq!(body_value(&req, "NewProtocol"), "UDP");
            match action.as_str() {
                "AddPortMapping" => {
                    r.adds += 1;
                    let lease: u32 = body_value(&req, "NewLeaseDuration").parse().unwrap();
                    r.add_leases.push(lease);
                    if let Some((status, code)) = r.refuse {
                        return Ok(HttpResponse {
                            status,
                            body: soap_fault(code).body,
                        });
                    }
                    if r.only_permanent && lease != 0 {
                        return Ok(soap_fault(725));
                    }
                    if r.conflict.contains(&ext) {
                        return Ok(soap_fault(718));
                    }
                    let client = body_value(&req, "NewInternalClient");
                    let internal: u16 = body_value(&req, "NewInternalPort").parse().unwrap();
                    r.mappings.insert(ext, (client, internal, lease));
                    r.descriptions
                        .insert(ext, body_value(&req, "NewPortMappingDescription"));
                    Ok(HttpResponse {
                        status: 200,
                        body: "<u:AddPortMappingResponse/>".into(),
                    })
                }
                "GetSpecificPortMappingEntry" => {
                    r.gets += 1;
                    match r.mappings.get(&ext) {
                        Some((client, internal, lease)) => {
                            let desc = r.descriptions.get(&ext).cloned().unwrap_or_default();
                            Ok(HttpResponse {
                                status: 200,
                                body: format!(
                                    "<NewInternalPort>{internal}</NewInternalPort><NewInternalClient>{client}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>{desc}</NewPortMappingDescription><NewLeaseDuration>{lease}</NewLeaseDuration>"
                                ),
                            })
                        }
                        None => Ok(soap_fault(714)),
                    }
                }
                "DeletePortMapping" => {
                    r.deletes += 1;
                    r.descriptions.remove(&ext);
                    if r.mappings.remove(&ext).is_some() {
                        Ok(HttpResponse {
                            status: 200,
                            body: "<u:DeletePortMappingResponse/>".into(),
                        })
                    } else {
                        Ok(soap_fault(714))
                    }
                }
                other => panic!("知らない動作 {other}"),
            }
        }
    }

    fn ports() -> Vec<u16> {
        vec![47001, 50001, 50002]
    }

    #[test]
    fn the_internal_port_is_mapped_for_this_pc_with_a_finite_lease() {
        let r = router();
        let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
        assert_eq!(e.external, 47001);
        let g = r.lock().unwrap();
        assert_eq!(
            g.mappings.get(&47001),
            Some(&("192.168.0.7".to_string(), 47001, LEASE_SECS))
        );
        const { assert!(LEASE_SECS > 0, "期限なしで頼まない") };
    }

    #[test]
    fn an_overlapping_external_port_is_retried_on_another_port_and_the_internal_port_stays() {
        let r = router();
        r.lock().unwrap().conflict = vec![47001, 50001];
        let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
        assert_eq!(e.external, 50002);
        let g = r.lock().unwrap();
        assert_eq!(g.adds, 3);
        assert_eq!(g.mappings.get(&50002).map(|m| m.1), Some(47001));
        assert!(!g.mappings.contains_key(&47001));
    }

    #[test]
    fn every_port_overlapping_is_a_refusal_with_the_code() {
        let r = router();
        r.lock().unwrap().conflict = ports();
        let o = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap_err();
        let Outcome::Refused(e) = o else {
            panic!("{o:?}");
        };
        assert_eq!(e.fault_code(), Some(FAULT_CONFLICT));
        assert!(r.lock().unwrap().mappings.is_empty());
    }

    #[test]
    fn a_router_that_only_takes_permanent_mappings_is_asked_again_with_lease_zero() {
        let r = router();
        r.lock().unwrap().only_permanent = true;
        let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
        assert_eq!(
            (e.external, e.lease, e.already_open),
            (47001, Lease::Permanent, false)
        );
        let g = r.lock().unwrap();
        assert_eq!(
            g.add_leases,
            [LEASE_SECS, 0],
            "725 の後、期限なしで 1 回だけ"
        );
        assert_eq!(
            g.mappings.get(&47001),
            Some(&("192.168.0.7".to_string(), 47001, 0))
        );
        assert_eq!(
            g.descriptions.get(&47001).map(String::as_str),
            Some(MAPPING_DESCRIPTION),
            "期限なしは落ちると残るので印を付ける"
        );
    }

    #[test]
    fn a_permanent_refusal_after_725_ends_the_attempt_without_a_third_request() {
        let r = router();
        r.lock().unwrap().refuse = Some((500, 725));
        let o = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap_err();
        assert!(
            matches!(&o, Outcome::Refused(e) if e.fault_code() == Some(725)),
            "{o:?}"
        );
        let g = r.lock().unwrap();
        assert_eq!(g.add_leases, [LEASE_SECS, 0]);
        assert!(g.mappings.is_empty());
    }

    #[test]
    fn a_permanent_mapping_is_not_renewed_and_is_deleted_on_close() {
        let r = router();
        r.lock().unwrap().only_permanent = true;
        let h = handle(&r, Duration::from_millis(20));
        assert!(matches!(
            h.wait_until(Duration::from_secs(5)),
            Outcome::Mapped {
                external: 47001,
                ..
            }
        ));
        // 頼み直しの間隔（20 ms）を何度も越える間、足さない。
        std::thread::sleep(Duration::from_millis(120));
        let s = h.stats();
        assert_eq!(
            (s.lease, s.renewed, s.renew_failed),
            (Lease::Permanent, 0, 0)
        );
        assert!(s.text().contains("期限なしで頼み直した"), "{}", s.text());
        assert_eq!(r.lock().unwrap().add_leases, [LEASE_SECS, 0]);
        h.stop();
        let g = r.lock().unwrap();
        assert_eq!(g.deletes, 1);
        assert!(g.mappings.is_empty(), "閉じるとき消していない");
        assert_eq!(h.stats().release, Release::Done);
    }

    #[test]
    fn a_leftover_permanent_mapping_with_our_mark_is_taken_over_and_deleted() {
        for only_permanent in [true, false] {
            let r = router();
            {
                let mut g = r.lock().unwrap();
                g.only_permanent = only_permanent;
                g.mappings
                    .insert(47001, ("192.168.0.7".to_string(), 47001, 0));
                g.descriptions
                    .insert(47001, MAPPING_DESCRIPTION.to_string());
            }
            let h = handle(&r, Duration::from_millis(20));
            assert!(matches!(
                h.wait_until(Duration::from_secs(5)),
                Outcome::Mapped {
                    external: 47001,
                    ..
                }
            ));
            std::thread::sleep(Duration::from_millis(80));
            let s = h.stats();
            assert_eq!((s.lease, s.already_open), (Lease::Inherited, false));
            assert!(s.text().contains("引き継いだ"), "{}", s.text());
            h.stop();
            let g = r.lock().unwrap();
            assert_eq!(g.adds, 0, "引き継ぐだけで足さない");
            assert_eq!(g.deletes, 1);
            assert!(g.mappings.is_empty(), "前の起動の消し残りを消していない");
            assert_eq!(h.stats().release, Release::Done);
        }
    }

    #[test]
    fn a_permanent_mapping_without_our_mark_is_left_alone_even_on_a_permanent_only_router() {
        for desc in ["", "other tool", "higanrtool 2"] {
            let r = router();
            {
                let mut g = r.lock().unwrap();
                g.only_permanent = true;
                g.mappings
                    .insert(47001, ("192.168.0.7".to_string(), 47001, 0));
                g.descriptions.insert(47001, desc.to_string());
            }
            let h = handle(&r, Duration::from_millis(20));
            assert!(matches!(
                h.wait_until(Duration::from_secs(5)),
                Outcome::Mapped {
                    external: 47001,
                    ..
                }
            ));
            assert!(h.stats().already_open, "{desc:?}");
            h.stop();
            let g = r.lock().unwrap();
            assert_eq!(
                (g.adds, g.deletes),
                (0, 0),
                "{desc:?}: 他人の割り当てを触った"
            );
            assert!(g.mappings.contains_key(&47001), "{desc:?}");
            assert_eq!(h.stats().release, Release::Kept);
        }
    }

    #[test]
    fn our_mark_on_another_machines_mapping_is_not_taken_over() {
        let r = router();
        {
            let mut g = r.lock().unwrap();
            g.mappings
                .insert(47001, ("192.168.0.99".to_string(), 47001, 0));
            g.descriptions
                .insert(47001, MAPPING_DESCRIPTION.to_string());
        }
        let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
        assert_eq!((e.external, e.lease), (50001, Lease::Timed));
        let g = r.lock().unwrap();
        assert_eq!(
            g.mappings.get(&47001),
            Some(&("192.168.0.99".to_string(), 47001, 0)),
            "他の機器の割り当てを書き換えた"
        );
    }

    #[test]
    fn other_refusals_stop_at_the_first_port() {
        let r = router();
        r.lock().unwrap().refuse = Some((500, 606));
        let o = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap_err();
        assert!(matches!(&o, Outcome::Refused(e) if e.fault_code() == Some(606)));
        assert_eq!(r.lock().unwrap().adds, 1);
        let s = stats_of(&o, Duration::from_millis(12));
        assert_eq!(
            (s.state, s.http_status, s.fault_code),
            (State::Refused, 500, 606)
        );
        assert!(s.text().contains("606"), "{}", s.text());
    }

    #[test]
    fn no_reply_broken_descriptions_and_io_errors_are_told_apart() {
        let r = router();
        r.lock().unwrap().reply = false;
        assert_eq!(
            establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), 60).unwrap_err(),
            Outcome::NoAnswer
        );
        let r = router();
        r.lock().unwrap().io_error_on_discover = true;
        assert!(matches!(
            establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), 60).unwrap_err(),
            Outcome::Failed(UpnpError::Io(_))
        ));
        for (status, body) in [
            (404, ""),
            (200, "<html>not xml</html>"),
            (200, ""),
            (
                200,
                "<root><service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/c</controlURL></service></root>",
            ),
        ] {
            let r = router();
            {
                let mut g = r.lock().unwrap();
                g.description_status = status;
                g.description = body.to_string();
            }
            let o = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), 60).unwrap_err();
            assert!(matches!(o, Outcome::NoService(_)), "{status} {body}: {o:?}");
            assert_eq!(r.lock().unwrap().adds, 0, "サービスが読めないのに頼んだ");
        }
    }

    fn handle(r: &Arc<Mutex<Router>>, renew: Duration) -> Handle {
        Handle::spawn(
            Box::new(FakeWire(Arc::clone(r))),
            LOCAL,
            ports(),
            LEASE_SECS,
            renew,
        )
        .unwrap()
    }

    #[test]
    fn closing_the_net_deletes_the_mapping() {
        let r = router();
        let h = handle(&r, RENEW_EVERY);
        assert_eq!(
            h.wait_until(Duration::from_secs(5)),
            Outcome::Mapped {
                external: 47001,
                outer_ip: Some(PUBLIC)
            }
        );
        assert!(r.lock().unwrap().mappings.contains_key(&47001));
        assert_eq!(h.stats().release, Release::NotYet);
        h.stop();
        {
            let g = r.lock().unwrap();
            assert!(
                g.mappings.is_empty(),
                "ネットワークを閉じたのに割り当てが残った"
            );
            assert_eq!(g.deletes, 1);
        }
        let s = h.stats();
        assert_eq!(
            (s.state, s.external_port, s.release),
            (State::Mapped, 47001, Release::Done)
        );
        assert!(
            s.text().contains("47001") && s.text().contains("消した"),
            "{}",
            s.text()
        );
        h.stop();
        assert_eq!(r.lock().unwrap().deletes, 1, "2 回目の stop は何もしない");
    }

    #[test]
    fn dropping_the_handle_also_deletes_the_mapping() {
        let r = router();
        {
            let h = handle(&r, RENEW_EVERY);
            h.wait_until(Duration::from_secs(5));
        }
        assert!(r.lock().unwrap().mappings.is_empty());
    }

    #[test]
    fn the_mapping_is_renewed_while_the_net_is_open() {
        let r = router();
        let h = handle(&r, Duration::from_millis(20));
        h.wait_until(Duration::from_secs(5));
        let t0 = Instant::now();
        while h.stats().renewed < 2 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.stats().renewed >= 2, "{:?}", h.stats());
        assert!(r.lock().unwrap().adds >= 3);
        h.stop();
        assert!(r.lock().unwrap().mappings.is_empty());
    }

    #[test]
    fn a_failed_renewal_is_counted_and_the_mapping_is_still_deleted() {
        let r = router();
        let h = handle(&r, Duration::from_millis(20));
        h.wait_until(Duration::from_secs(5));
        r.lock().unwrap().refuse = Some((500, 501));
        let t0 = Instant::now();
        while h.stats().renew_failed < 1 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.stats().renew_failed >= 1);
        h.stop();
        assert!(r.lock().unwrap().mappings.is_empty());
        assert_eq!(h.stats().release, Release::Done);
    }

    /// `cond` が真になるまで最大 5 秒待つ。
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let t0 = Instant::now();
        while !cond() {
            if t0.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn a_late_mapping_after_the_opener_gave_up_is_removed_by_itself() {
        let r = router();
        r.lock().unwrap().delay = Duration::from_millis(80);
        let h = handle(&r, RENEW_EVERY);
        // 80 ms × 数往復（記述・確認・追加）より短く待つ。
        assert_eq!(h.wait_until(Duration::from_millis(30)), Outcome::TimedOut);
        assert_eq!(h.stats().state, State::TimedOut);
        // 止める前に、スレッドが自分で消すこと（止めたときの後始末と区別する）。
        assert!(
            wait_for(|| r.lock().unwrap().deletes == 1),
            "諦めた後の割り当てを自分で消さない"
        );
        assert!(r.lock().unwrap().mappings.is_empty());
        assert_eq!(
            h.stats().state,
            State::TimedOut,
            "使われない割り当ては Mapped にしない"
        );
        h.stop();
        let g = r.lock().unwrap();
        assert_eq!(g.adds, g.deletes, "追加と同じだけ消した");
        assert_eq!(g.deletes, 1, "止めても二重に消さない");
    }

    #[test]
    fn stopping_during_the_query_leaves_nothing_behind() {
        let r = router();
        r.lock().unwrap().delay = Duration::from_millis(40);
        let h = handle(&r, RENEW_EVERY);
        std::thread::sleep(Duration::from_millis(10));
        h.stop();
        {
            let g = r.lock().unwrap();
            assert!(g.mappings.is_empty());
            assert_eq!(g.adds, g.deletes, "足したものは消した");
            assert_eq!(g.deletes, 1);
        }
        // 止められた後に、使える割り当てとして知らせない。
        assert_eq!(h.stats().state, State::TimedOut);
        assert_eq!(h.stats().release, Release::Done);
        assert_eq!(h.wait_until(Duration::ZERO), Outcome::TimedOut);
    }

    #[test]
    fn a_mapping_already_open_for_this_pc_is_neither_added_renewed_nor_deleted() {
        let r = router();
        r.lock()
            .unwrap()
            .mappings
            .insert(47001, ("192.168.0.7".to_string(), 47001, 0));
        let h = Handle::spawn(
            Box::new(FakeWire(Arc::clone(&r))),
            LOCAL,
            ports(),
            LEASE_SECS,
            Duration::from_millis(20),
        )
        .unwrap();
        assert_eq!(
            h.wait_until(Duration::from_secs(5)),
            Outcome::Mapped {
                external: 47001,
                outer_ip: Some(PUBLIC)
            }
        );
        // 頼み直しの間隔（20 ms）を何度も越える間、何も足さない。
        std::thread::sleep(Duration::from_millis(120));
        let s = h.stats();
        assert!(s.already_open);
        assert_eq!((s.renewed, s.renew_failed), (0, 0));
        assert!(s.text().contains("もう開いている"), "{}", s.text());
        h.stop();
        let g = r.lock().unwrap();
        assert_eq!((g.adds, g.deletes), (0, 0), "足していないものを触った");
        assert!(
            g.mappings.contains_key(&47001),
            "もとからの割り当てを消した"
        );
        assert_eq!(h.stats().release, Release::Kept);
    }

    #[test]
    fn a_leftover_timed_mapping_for_this_pc_is_taken_over_renewed_and_deleted() {
        let r = router();
        // 前の起動が作って消し損ねた形（残り 90 秒）。
        r.lock()
            .unwrap()
            .mappings
            .insert(47001, ("192.168.0.7".to_string(), 47001, 90));
        let h = Handle::spawn(
            Box::new(FakeWire(Arc::clone(&r))),
            LOCAL,
            ports(),
            LEASE_SECS,
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(matches!(
            h.wait_until(Duration::from_secs(5)),
            Outcome::Mapped {
                external: 47001,
                ..
            }
        ));
        assert_eq!(
            r.lock().unwrap().mappings.get(&47001).map(|m| m.2),
            Some(LEASE_SECS),
            "期限を延ばしていない"
        );
        assert!(!h.stats().already_open);
        assert!(
            wait_for(|| h.stats().renewed >= 2),
            "以後の頼み直しをしない"
        );
        h.stop();
        let g = r.lock().unwrap();
        assert!(g.adds >= 3);
        assert_eq!(g.deletes, 1);
        assert!(g.mappings.is_empty(), "閉じるとき消していない");
        assert_eq!(h.stats().release, Release::Done);
    }

    #[test]
    fn a_mapping_for_another_machine_or_port_is_left_alone_and_the_next_port_is_used() {
        for (client, internal) in [("192.168.0.99", 47001), ("192.168.0.7", 40000)] {
            let r = router();
            r.lock()
                .unwrap()
                .mappings
                .insert(47001, (client.to_string(), internal, 0));
            let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
            assert_eq!((e.external, e.already_open), (50001, false), "{client}");
            let g = r.lock().unwrap();
            assert_eq!(g.adds, 1, "他人の割り当てへ足していない");
            assert_eq!(
                g.mappings.get(&47001),
                Some(&(client.to_string(), internal, 0)),
                "他人の割り当てを書き換えた"
            );
            assert_eq!(g.mappings.get(&50001).map(|m| m.1), Some(47001));
        }
        // 全部ほかの機器向けなら、足さずに重なりとして断る。
        let r = router();
        for p in ports() {
            r.lock()
                .unwrap()
                .mappings
                .insert(p, ("192.168.0.99".to_string(), p, 0));
        }
        let o = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap_err();
        assert!(matches!(&o, Outcome::Refused(e) if e.fault_code() == Some(FAULT_CONFLICT)));
        assert_eq!(r.lock().unwrap().adds, 0);
    }

    #[test]
    fn the_router_side_address_is_read_after_adding_and_may_be_missing() {
        let r = router();
        let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
        assert_eq!(e.outer_ip, Some(PUBLIC));
        assert_eq!(r.lock().unwrap().outer_gets, 1);
        // 答えない機器でも、割り当て自体は成り立つ（判定は呼び手）。
        let r = router();
        r.lock().unwrap().outer_ip = None;
        let e = establish(&mut FakeWire(Arc::clone(&r)), LOCAL, &ports(), LEASE_SECS).unwrap();
        assert_eq!(e.outer_ip, None);
        assert!(r.lock().unwrap().mappings.contains_key(&47001));
    }

    #[test]
    fn a_discarded_mapping_is_deleted_and_the_state_says_why() {
        let r = router();
        let h = handle(&r, RENEW_EVERY);
        assert!(matches!(
            h.wait_until(Duration::from_secs(5)),
            Outcome::Mapped { .. }
        ));
        h.discard(State::OuterBehind);
        assert!(
            wait_for(|| r.lock().unwrap().deletes == 1),
            "使わないと決めた割り当てを待たずに消さない"
        );
        h.stop();
        let s = h.stats();
        assert_eq!((s.state, s.release), (State::OuterBehind, Release::Done));
        assert!(s.text().contains("二重 NAT"), "{}", s.text());
        assert!(r.lock().unwrap().mappings.is_empty());
    }

    #[test]
    fn the_retry_after_a_failed_renewal_is_shorter_than_the_normal_interval() {
        let (renew, retry) = (RENEW_EVERY, RETRY_AFTER_FAIL);
        assert_eq!(next_wait(renew, retry, true), renew);
        let after_fail = next_wait(renew, retry, false);
        assert_eq!(after_fail, retry);
        assert!(after_fail >= Duration::from_secs(60) && after_fail <= Duration::from_secs(120));
        // 通常の間隔のほうが短い設定なら、それより長くしない。
        assert_eq!(
            next_wait(Duration::from_millis(20), retry, false),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn a_failed_renewal_is_asked_again_sooner_than_the_normal_interval() {
        let r = router();
        let h = Handle::spawn_with_retry(
            Box::new(FakeWire(Arc::clone(&r))),
            LOCAL,
            ports(),
            LEASE_SECS,
            Duration::from_millis(600),
            Duration::from_millis(20),
        )
        .unwrap();
        h.wait_until(Duration::from_secs(5));
        r.lock().unwrap().refuse = Some((500, 501));
        // 最初の頼み直し（約 600 ms 後）が失敗してから、次の 3 回は通常の間隔（600 ms）なら 1.8 秒かかる。
        assert!(wait_for(|| h.stats().renew_failed >= 1));
        let t = Instant::now();
        assert!(wait_for(|| h.stats().renew_failed >= 4));
        assert!(
            t.elapsed() < Duration::from_millis(400),
            "{:?}",
            t.elapsed()
        );
        h.stop();
    }

    #[test]
    fn ssdp_silence_is_remembered_per_interface_and_errors_are_not() {
        let memo = SsdpMemo(AtomicU32::new(0));
        let a = Ipv4Addr::new(192, 168, 0, 7);
        let b = Ipv4Addr::new(10, 0, 0, 7);
        let url = || Url::parse("http://192.168.0.1:5000/d.xml").unwrap();
        let mut searched = 0u32;
        let mut run = |ip: Ipv4Addr, r: Result<Vec<Url>, UpnpError>| {
            memo.run(ip, || {
                searched += 1;
                r
            })
        };
        // 誤りは覚えない。
        assert!(run(a, Err(UpnpError::Io("bind".into()))).is_err());
        assert!(run(a, Err(UpnpError::Io("bind".into()))).is_err());
        // 答えがあれば覚えない。
        assert_eq!(run(a, Ok(vec![url()])).unwrap().len(), 1);
        // 答えが無かったソケットは、次から探さない。
        assert!(run(a, Ok(Vec::new())).unwrap().is_empty());
        assert!(run(a, Ok(vec![url()])).unwrap().is_empty());
        // 別のソケット（別のネットワーク）では探す。
        assert_eq!(run(b, Ok(vec![url()])).unwrap().len(), 1);
        assert_eq!(searched, 5, "探した数（覚えた口では探さない）");
    }

    // ---- SSDP の返事のガード（本物の UDP。loopback の偽の機器へ送る）----

    /// 受けた M-SEARCH に `location` を載せた返事を 1 通返す偽の機器。
    fn ssdp_responder(location: &'static str) -> (SocketAddrV4, JoinHandle<()>) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let SocketAddr::V4(addr) = sock.local_addr().unwrap() else {
            unreachable!()
        };
        let join = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let (n, from) = sock.recv_from(&mut buf).unwrap();
            assert!(buf[..n].starts_with(b"M-SEARCH * HTTP/1.1"));
            let reply = format!("HTTP/1.1 200 OK\r\nLOCATION: {location}\r\n\r\n");
            sock.send_to(reply.as_bytes(), from).unwrap();
        });
        (addr, join)
    }

    #[test]
    fn discovery_uses_a_reply_only_when_the_sender_is_the_machine_it_names() {
        let local = Ipv4Addr::LOCALHOST;
        // 送り手（127.0.0.1）と LOCATION のアドレス（127.0.0.2）が違う返事は使わない。
        let (dest, j) = ssdp_responder("http://127.0.0.2:5000/d.xml");
        let found = NetWire::search(local, Duration::from_millis(400), dest).unwrap();
        j.join().unwrap();
        assert!(found.is_empty(), "別の機器を指す返事を使った: {found:?}");
        // 同じなら使う。
        let (dest, j) = ssdp_responder("http://127.0.0.1:5000/d.xml");
        let found = NetWire::search(local, Duration::from_millis(400), dest).unwrap();
        j.join().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].host, local);
    }

    // ---- 本物のルーターの検査のガード ----

    /// 呼ばれたら数える偽の道具。
    #[derive(Default)]
    struct CountingWire(u32);

    impl Wire for CountingWire {
        fn discover(&mut self, _: Ipv4Addr, _: Duration) -> Result<Vec<Url>, UpnpError> {
            self.0 += 1;
            Ok(Vec::new())
        }
        fn exchange(&mut self, _: &Url, _: &[u8], _: Duration) -> Result<HttpResponse, UpnpError> {
            self.0 += 1;
            Err(UpnpError::Io("検査".into()))
        }
    }

    #[test]
    fn the_real_router_check_does_nothing_without_the_opt_in() {
        for opt_in in [None, Some(""), Some("0"), Some("yes"), Some("true")] {
            let mut w = CountingWire::default();
            assert!(!real_router_run(opt_in, &mut w), "{opt_in:?}");
            assert_eq!(w.0, 0, "{opt_in:?}: 許可が無いのにネットワークへ触った");
        }
    }

    #[test]
    fn a_refused_start_is_reported_to_the_waiter() {
        let r = router();
        r.lock().unwrap().reply = false;
        let h = handle(&r, RENEW_EVERY);
        assert_eq!(h.wait_until(Duration::from_secs(5)), Outcome::NoAnswer);
        assert_eq!(h.stats().state, State::NoAnswer);
        assert!(h.stats().text().contains("応じない"));
        h.stop();
        assert_eq!(
            r.lock().unwrap().deletes,
            0,
            "割り当てていないものは消さない"
        );
    }

    #[test]
    fn every_state_has_its_own_text_without_an_address() {
        let states = [
            State::NotTried,
            State::Working,
            State::NoAnswer,
            State::NoService,
            State::Refused,
            State::Failed,
            State::TimedOut,
            State::Mapped,
            State::OuterBehind,
            State::OuterUnchecked,
        ];
        let mut seen = std::collections::HashSet::new();
        for state in states {
            let t = Stats {
                state,
                external_port: 47001,
                http_status: 500,
                fault_code: 718,
                ..Stats::default()
            }
            .text();
            assert!(t.starts_with("UPnP: "), "{t}");
            assert!(!t.contains("192.168") && !t.contains("127.0"), "{t}");
            assert!(seen.insert(t), "{state:?} の文が他と同じ");
        }
    }

    #[test]
    fn only_home_ipv4_sources_are_asked() {
        let v4 = |s: &str| lan_source(s.parse().unwrap());
        assert!(v4("192.168.0.7:47001").is_some());
        assert!(v4("10.1.2.3:47001").is_some());
        assert!(v4("172.16.0.9:47001").is_some());
        assert!(v4("169.254.3.4:47001").is_some());
        assert!(v4("127.0.0.1:47001").is_none(), "loopback は頼まない");
        assert!(
            v4("203.0.113.5:47001").is_none(),
            "公開のアドレスは頼まない"
        );
        assert!(v4("[fd00::2]:47001").is_none());
    }

    #[test]
    fn the_port_order_starts_with_the_internal_port_and_stays_in_the_dynamic_range() {
        let v = port_order(47001, [0, 0, 0xFF, 0xFF, 0x34, 0x12]);
        assert_eq!(v[0], 47001);
        assert!(v.len() >= 2 && v.len() <= 1 + ALT_PORTS);
        assert!(v[1..].iter().all(|&p| p >= 49_152));
        let mut u = v.clone();
        u.sort_unstable();
        u.dedup();
        assert_eq!(u.len(), v.len(), "重なり無し");
        // 全部同じ種なら重ねない。
        assert_eq!(port_order(47001, [7; 6]).len(), 2);
    }

    // ---- 本物の TCP で HTTP を話す偽の機器（SSDP だけは注入。マルチキャストは検査で使わない）----

    struct LoopWire {
        base: Url,
    }

    impl Wire for LoopWire {
        fn discover(&mut self, _: Ipv4Addr, _: Duration) -> Result<Vec<Url>, UpnpError> {
            Ok(vec![self.base.clone()])
        }
        fn exchange(
            &mut self,
            to: &Url,
            request: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, UpnpError> {
            NetWire.exchange(to, request, timeout)
        }
    }

    /// 127.0.0.1 の TCP で、機器の記述（chunked で返す）と SOAP（Content-Length で返す）を話し、接続を閉じる。
    fn serve_igd(stop_after: usize) -> (Url, JoinHandle<Vec<String>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let base = Url::parse(&format!("http://127.0.0.1:{port}/rootDesc.xml")).unwrap();
        let join = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut table: BTreeMap<String, String> = BTreeMap::new();
            for _ in 0..stop_after {
                let (mut s, _) = l.accept().unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut req = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let n = s.read(&mut chunk).unwrap();
                    req.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&req).into_owned();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        if req.len() >= h + 4 + len {
                            break;
                        }
                    }
                    assert!(n > 0, "要求が途中で切れた");
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let first = text.lines().next().unwrap().to_string();
                let reply = if first.starts_with("GET /rootDesc.xml") {
                    let body = description();
                    let (a, b) = body.split_at(40);
                    format!(
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                        a.len(),
                        b.len()
                    )
                } else {
                    let action = text
                        .lines()
                        .find_map(|l| l.strip_prefix("SOAPACTION: \""))
                        .and_then(|l| l.split('#').nth(1))
                        .and_then(|l| l.strip_suffix('"'))
                        .unwrap()
                        .to_string();
                    let ext = if action == "GetExternalIPAddress" {
                        String::new()
                    } else {
                        body_value(&text, "NewExternalPort")
                    };
                    let body = match action.as_str() {
                        "GetExternalIPAddress" => {
                            format!("<NewExternalIPAddress>{PUBLIC}</NewExternalIPAddress>")
                        }
                        "AddPortMapping" => {
                            table.insert(ext, body_value(&text, "NewInternalClient"));
                            "<u:AddPortMappingResponse/>".to_string()
                        }
                        "GetSpecificPortMappingEntry" => match table.get(&ext) {
                            Some(c) => format!(
                                "<NewInternalPort>{ext}</NewInternalPort><NewInternalClient>{c}</NewInternalClient>"
                            ),
                            None => {
                                let b = soap_fault(714).body;
                                s.write_all(
                                    format!(
                                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                                        b.len()
                                    )
                                    .as_bytes(),
                                )
                                .unwrap();
                                seen.push(action);
                                continue;
                            }
                        },
                        _ => {
                            table.remove(&ext);
                            "<u:DeletePortMappingResponse/>".to_string()
                        }
                    };
                    seen.push(action);
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                s.write_all(reply.as_bytes()).unwrap();
            }
            seen
        });
        (base, join)
    }

    #[test]
    fn a_real_tcp_round_trip_maps_checks_and_deletes() {
        // 往復: 記述 1・先に確認 1（無い）・追加 1・外側のアドレス 1・確認 1・削除 1・確認 1（消えている）。
        let (base, server) = serve_igd(7);
        let mut w = LoopWire { base };
        let local = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 47001);
        let e = establish(&mut w, local, &[47001], 60).unwrap();
        assert_eq!((e.external, e.outer_ip), (47001, Some(PUBLIC)));
        let entry = get_mapping(&mut w, &e.service, 47001).unwrap();
        assert_eq!(
            (entry.internal_port, entry.internal_client.as_str()),
            (47001, "127.0.0.1")
        );
        delete_mapping(&mut w, &e.service, 47001).unwrap();
        let gone = get_mapping(&mut w, &e.service, 47001).unwrap_err();
        assert_eq!(gone.fault_code(), Some(FAULT_NO_SUCH_ENTRY));
        assert_eq!(
            server.join().unwrap(),
            [
                "GetSpecificPortMappingEntry",
                "AddPortMapping",
                "GetExternalIPAddress",
                "GetSpecificPortMappingEntry",
                "DeletePortMapping",
                "GetSpecificPortMappingEntry"
            ]
        );
    }

    #[test]
    fn a_silent_server_ends_with_a_timeout_error_not_a_hang() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let to = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let t0 = Instant::now();
        let r = NetWire.exchange(&to, b"GET /x HTTP/1.1\r\n\r\n", Duration::from_millis(150));
        assert!(matches!(r, Err(UpnpError::Io(_))), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(2));
        drop(l);
        // 閉じたポートへ: つなげない。
        let r = NetWire.exchange(&to, b"GET /x HTTP/1.1\r\n\r\n", Duration::from_millis(150));
        assert!(matches!(r, Err(UpnpError::Io(_))), "{r:?}");
    }

    // ---- 本物のルーター（手で回す。ルーターの設定を一時的に変える）----

    /// 本物のルーターの検査を回す許可の環境変数。値が `1` のときだけ回す。
    const REAL_ROUTER_ENV: &str = "HIGANR_REAL_ROUTER";

    /// 本物のルーターで 追加 → 確認 → 削除 → 確認 を 1 回ずつ行う。外のインターネットへは何も送らない（SSDP は LAN、HTTP は
    /// ルーターの LAN 側）。割り当ては期限 60 秒で、消し損ねても 1 分で消える。
    /// 回し方: `HIGANR_REAL_ROUTER=1 cargo test -p higanrtool_dll real_router_round_trip -- --ignored --nocapture`
    /// 許可が無ければ何もせずに戻る（全件の `--ignored` に混ざっても、ルーターの設定を変えない）。
    #[test]
    #[ignore = "本物のルーターの設定を一時的に変える。手で回す"]
    fn real_router_round_trip() {
        let opt_in = std::env::var(REAL_ROUTER_ENV).ok();
        real_router_run(opt_in.as_deref(), &mut NetWire);
    }

    /// 許可があれば本物のルーターの検査を回して true、無ければネットワークへ触らず false。
    fn real_router_run(opt_in: Option<&str>, w: &mut dyn Wire) -> bool {
        const TEST_PORT: u16 = 47_999;
        const LEASE: u32 = 60;
        if opt_in != Some("1") {
            eprintln!("本物のルーターの検査を飛ばした（回すなら {REAL_ROUTER_ENV}=1）");
            return false;
        }
        let route = crate::net_udp::probe_route_ip().expect("外へ出る経路が引けない");
        let IpAddr::V4(src) = route else {
            panic!("経路のアドレスが IPv4 でない");
        };
        assert!(
            lan_source(SocketAddr::new(route, TEST_PORT)).is_some(),
            "家の中のアドレスでない"
        );
        let local = SocketAddrV4::new(src, TEST_PORT);
        let t = Instant::now();
        let svc = match find_service(w, src, DISCOVER_BUDGET) {
            Ok(s) => s,
            Err(o) => panic!("機器を見つけられない: {o:?}"),
        };
        println!(
            "探索と機器の記述: {} ms（サービス {}）",
            t.elapsed().as_millis(),
            svc.service_type
        );
        // 最後に消すので、もとからある割り当てには触らない。
        assert!(
            get_mapping(w, &svc, TEST_PORT).is_err(),
            "検査用のポートに割り当てが既にある。触らずに止める"
        );
        let outer = get_external_ip(w, &svc);
        println!(
            "GetExternalIPAddress: {}",
            match &outer {
                Ok(ip) => format!(
                    "読めた（世界に通じるアドレスか: {}）",
                    guard::is_global_v4(*ip)
                ),
                Err(e) => e.text(),
            }
        );
        let t = Instant::now();
        let added = add_mapping(w, &svc, TEST_PORT, local, LEASE);
        println!(
            "AddPortMapping: {} ms -> {:?}",
            t.elapsed().as_millis(),
            added
        );
        added.expect("割り当てを足せない");
        let t = Instant::now();
        let got = get_mapping(w, &svc, TEST_PORT);
        println!(
            "GetSpecificPortMappingEntry: {} ms -> 内側のポート・有効・残り期限 {:?}",
            t.elapsed().as_millis(),
            got.as_ref()
                .map(|e| (e.internal_port, e.enabled, e.lease_secs))
        );
        let entry = got.expect("足した割り当てが見えない");
        assert_eq!(entry.internal_port, TEST_PORT);
        assert_eq!(entry.internal_client, src.to_string());
        let t = Instant::now();
        let del = delete_mapping(w, &svc, TEST_PORT);
        println!(
            "DeletePortMapping: {} ms -> {:?}",
            t.elapsed().as_millis(),
            del
        );
        del.expect("割り当てを消せない");
        let t = Instant::now();
        let after = get_mapping(w, &svc, TEST_PORT);
        println!(
            "GetSpecificPortMappingEntry（消した後）: {} ms -> {:?}",
            t.elapsed().as_millis(),
            after
        );
        let err = after.expect_err("消したはずの割り当てがまだ見える");
        assert_eq!(err.fault_code(), Some(FAULT_NO_SUCH_ENTRY));
        true
    }
}
