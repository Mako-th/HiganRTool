//! LAN 対戦の申し込みの運び手（std のソケットだけ。unsafe 無し）。
//!
//! 接続コードの受け渡しをクリップボードやルームサーバーでなく LAN の UDP でやる（判断と手順は core の `lan_proto`）。ここは
//! ソケットを開き、状態機械に受けたバイト列を渡し、出てきた「送る」「開く」「読ませる」を行うだけ。コードの中身も接続の手順も、
//! インターネットの経路と同じ（ホストが `Swapper::offer` で作ったコードをクライアントへ返し、クライアントが返すコードを返す）。
//!
//! - **別のスレッド・別のソケット**: 申し込みの受け口はゲームの受信ソケット（`net_udp` の受信ループ）とは別。ゲームのスレッドには何も足さない。
//!   ホストの受け口は、LAN の自分のアドレスごとに 1 つずつ（`bind(アドレス:ポート)`）。全部のアドレスで受ける未指定アドレスの待ち受けにしない:
//!   WAN 側のアドレスで待たず、どのアドレスに申し込みが届いたか（＝その相手に届く自分のアドレス）がそのまま分かる。
//! - **ゲームのネットワークは参加を受けてから開く**: ホストは申し込みが届いたアドレスで、クライアントは申し込み先へ届く自分のアドレスで
//!   ゲームのネットワークを開き（`LanEnv::open_net`。`scope = lan`・STUN・UPnP・IPv6 なし）、見張り（`net_code_game::GameWatcher`）が
//!   `LanBridge` を通してコードを受け渡す。見張りは「作る」「貼る」と同じ手順を踏むが、コードの運び先がクリップボードでなく `LanBridge`。
//! - **見張りとの受け渡し**: 運び手と見張りの間は `LanBridge`（錠で守った小さな入れ物）。見張りは指示のリングバッファの読み手のまま
//!   （運び手は、ネットワークを開く前の取りやめの指示だけを自分で読み、開く頼みを出したら読まない）。
//!
//! 申し込みの文脈（ホスト・クライアント・観戦の 3 つ）ごとに 1 本のスレッドで、終われば（取りやめ・失敗・成立して務めを終えた）スレッドも終わる。

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use higanr_core::guard::{self, Scope};
use higanr_core::lan_addr::LanAddr;
use higanr_core::lan_proto::{
    Client, ClientAct, ClientStage, Host, HostAct, Ver, Watcher, WatcherAct, Why,
};
use higanr_core::net_menu::{CodeState, Event, LAN_IPS_MAX, NAME_UNITS, Note};
use higanr_core::wire::MAX_DATAGRAM;

use crate::clipboard::Clipboard;
use crate::net_ui::{Order, UiLink};

/// ホストの受け口を回す間隔。申し込みは人が待つ程度の頻度なので、短くしない（待つだけで処理の重さは無い）。
const POLL: Duration = Duration::from_millis(20);
/// 申し込みを受けてからゲームのネットワークを開き、見張りが動き出すまで待つ上限。
const ATTACH_WAIT: Duration = Duration::from_secs(20);
/// 前のネットワークの片づけが済んで、新しいネットワークを開ける（ポートが空く）のを待つ上限。
const NET_FREE_WAIT: Duration = Duration::from_secs(8);

/// ゲームのネットワークを開くときの役。ホストは役割の乱数を大きくして、いつも試合のホスト（観戦コードを出す側）になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanRole {
    Host,
    Client,
}

/// 運び手が見張りにさせたいこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeCmd {
    /// 始める側のコードを作って `LanBridge` へ書く。
    Create,
    /// `LanBridge` にあるコードを読む（ホストは返すコード、クライアントは始める側のコード）。
    Paste,
    /// 観戦コードを作って答える（`LanBridge::post_watch_answer`）。
    WatchCode { src: SocketAddrV4, id: u32 },
}

#[derive(Default)]
struct Inner {
    attached: bool,
    closed: bool,
    connected: bool,
    aborted: bool,
    /// 見張りが書いたコード（運び手が取って相手へ送る）。
    outgoing: Option<String>,
    /// 運び手が置いたコード（見張りが `read` で読む。読んでも消さない）。
    incoming: Option<String>,
    cmds: VecDeque<BridgeCmd>,
    watch_answers: VecDeque<(SocketAddrV4, u32, Result<String, Why>)>,
    failure: Option<Event>,
}

/// 運び手と見張りの受け渡し。
#[derive(Default)]
pub struct LanBridge {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl LanBridge {
    pub fn new() -> Arc<LanBridge> {
        Arc::new(LanBridge::default())
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut g);
        self.changed.notify_all();
        r
    }

    // ---- 見張りの側 ----

    /// 見張りが動き出した。
    pub fn attach(&self) {
        self.with(|i| i.attached = true);
    }

    /// 見張りが終わった（以後、運び手は務めを終える）。
    pub fn close(&self) {
        self.with(|i| i.closed = true);
    }

    /// 対戦が成立した（観戦の申し込みを受け始める）。
    pub fn set_connected(&self) {
        self.with(|i| i.connected = true);
    }

    /// 見張りが待ちをやめた（メニューの取りやめ・失敗）。運び手は務めを終える。
    pub fn abort(&self) {
        self.with(|i| i.aborted = true);
    }

    pub fn take_cmd(&self) -> Option<BridgeCmd> {
        self.with(|i| i.cmds.pop_front())
    }

    pub fn take_failure(&self) -> Option<Event> {
        self.with(|i| i.failure.take())
    }

    pub fn post_outgoing(&self, text: &str) {
        self.with(|i| i.outgoing = Some(text.to_string()));
    }

    pub fn read_incoming(&self) -> Option<String> {
        self.with(|i| i.incoming.clone())
    }

    pub fn post_watch_answer(&self, src: SocketAddrV4, id: u32, answer: Result<String, Why>) {
        self.with(|i| i.watch_answers.push_back((src, id, answer)));
    }

    // ---- 運び手の側 ----

    pub fn is_closed(&self) -> bool {
        self.with(|i| i.closed)
    }

    pub fn is_aborted(&self) -> bool {
        self.with(|i| i.aborted)
    }

    pub fn is_connected(&self) -> bool {
        self.with(|i| i.connected)
    }

    pub fn post_cmd(&self, c: BridgeCmd) {
        self.with(|i| i.cmds.push_back(c));
    }

    pub fn set_incoming(&self, text: &str) {
        self.with(|i| i.incoming = Some(text.to_string()));
    }

    pub fn take_outgoing(&self) -> Option<String> {
        self.with(|i| i.outgoing.take())
    }

    pub fn take_watch_answer(&self) -> Option<(SocketAddrV4, u32, Result<String, Why>)> {
        self.with(|i| i.watch_answers.pop_front())
    }

    /// 見張りに、画面へ出して待ちをやめさせる（時間切れなど）。
    pub fn fail(&self, ev: Event) {
        self.with(|i| i.failure = Some(ev));
    }

    /// 見張りが動き出すのを待つ。動き出したら true。
    pub fn wait_attached(&self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if g.attached {
                return true;
            }
            if g.closed || g.aborted {
                return false;
            }
            let Some(left) = end.checked_duration_since(Instant::now()) else {
                return false;
            };
            g = self
                .changed
                .wait_timeout(g, left.min(POLL))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// 見張りがコードを受け渡す「クリップボード」（`LanBridge` へ書く・から読む）。
pub struct LanClip(pub Arc<LanBridge>);

impl Clipboard for LanClip {
    fn read(&mut self) -> Result<Option<String>, String> {
        Ok(self.0.read_incoming())
    }

    fn write(&mut self, text: &str) -> Result<(), String> {
        self.0.post_outgoing(text);
        Ok(())
    }
}

/// ゲームのネットワークを開く頼み（`LanEnv::open_net`）。
type OpenNet = Box<dyn Fn(Ipv4Addr, LanRole, Arc<LanBridge>) -> Result<(), String> + Send + Sync>;

/// 運び手の材料。製品は `script_win` が、検査は偽物を渡す。
pub struct LanEnv {
    pub link: Arc<UiLink>,
    pub ver: Ver,
    /// 観戦の申し込みを受ける設定か（`[spectate] enable = 1`）。
    pub watch_ok: bool,
    /// ホストの受け口のポート。
    pub port: u16,
    /// ホストが返すコードを待つ上限（ms。製品は `lan_proto::HOST_WAIT_REPLY_MS`）。過ぎたら待機へ戻る。
    pub reply_wait_ms: u64,
    /// ホストの自分の LAN のアドレス（優先の高い順）。
    pub addresses: Box<dyn Fn() -> Result<Vec<Ipv4Addr>, String> + Send + Sync>,
    /// 前のネットワークの片づけが済んで、ホストの受け口のポートが空いたか。
    pub net_free: Box<dyn Fn() -> bool + Send + Sync>,
    /// ゲームのネットワークを `ip` で開く頼み（開くスレッドを起こして返る。見張りは動き出したら `LanBridge::attach` を呼ぶ）。
    pub open_net: OpenNet,
    /// 観戦コードを受けた。観戦者のネットワークを `ip` で開く頼みを出す（観戦者のセッションを置くのはゲームのスレッド）。
    pub start_watch: Box<dyn Fn(String, Ipv4Addr) -> Result<(), String> + Send + Sync>,
    /// 申し込みの番号（クライアント・観戦が引く乱数）。
    pub id: u32,
    /// 今の UNIX 秒（待機時間の起点）。
    pub unix: fn() -> u64,
    /// 外から止める（プロセスの終わり、など）。
    pub stop: Arc<AtomicBool>,
}

impl LanEnv {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// 状態の札の書き手（運び手が見張りに引き継ぐ前だけ使う）。
struct Say<'a> {
    link: &'a UiLink,
    note: Note,
}

impl<'a> Say<'a> {
    fn new(link: &'a UiLink) -> Say<'a> {
        Say {
            link,
            note: Note {
                seq: link.note().map_or(0, |n| n.seq),
                ..Note::default()
            },
        }
    }

    fn put(&mut self, state: CodeState, event: Event) {
        self.note.state = state;
        self.note.event = event;
        // ホストは見張りが動いた後も札を書く（待機へ戻ったとき）ので、見張りが書いた数の続きから数える（同じ数だとメニューが変化を見落とす）。
        let last = self.link.note().map_or(self.note.seq, |n| n.seq);
        self.note.seq = last.wrapping_add(1);
        self.link.publish(&self.note);
    }

    /// 待ちを終える（元のメニューへ戻し、理由を残す）。
    fn end(&mut self, event: Event) {
        self.put(CodeState::Idle, event);
    }
}

/// 申し込みの務めの終わり方（本ログと検査用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// メニューの取りやめ。
    Cancelled,
    /// 外から止められた。
    Stopped,
    /// 失敗して、理由を画面へ出した。
    Failed(Event),
    /// 務めを終えた（コードの受け渡しが済んだ・見張りが終わった）。
    Done,
}

fn v4(a: SocketAddr) -> Option<SocketAddrV4> {
    match a {
        SocketAddr::V4(v) => Some(v),
        SocketAddr::V6(_) => None,
    }
}

/// 取りやめの指示が来ているか（ネットワークを開く前だけ指示のリングバッファを読む。ほかの指示は読み捨てる）。
fn cancel_asked(link: &UiLink) -> bool {
    let mut cancel = false;
    while let Some(o) = link.take_order() {
        cancel |= o == Order::Cancel;
    }
    cancel
}

fn why_event(why: Why, say: &mut Say<'_>) -> Event {
    match why {
        Why::Busy => Event::LanBusy,
        Why::NotYet => Event::LanNoMatch,
        Why::WatchOff => Event::LanWatchOff,
        Why::OtherVersion { theirs } => {
            say.note.peer_ver = theirs;
            Event::OtherVersion
        }
        Why::OtherBuild { theirs } => {
            say.note.peer_build = theirs;
            Event::OtherBuild
        }
    }
}

fn wait_for(limit: Duration, mut ok: impl FnMut() -> bool, stop: &AtomicBool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if ok() {
            return true;
        }
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(POLL);
    }
    ok()
}

// ---- ホスト ----

/// 覚えておく送り元の数の上限（古いものから忘れる。LAN の中の申し込みの相手なので、数は人の数ほど）。
const ROUTES_MAX: usize = 64;

/// 送り元ごとの、申し込みを受けた受け口の番号。返事はその受け口から返す: 相手に届く自分のアドレスは相手ごとに違いうる（有線と無線の両方がある PC）し、
/// 申し込む側のソケットは `connect` 済みで、ほかのアドレスから来た返事を受け取らない。
#[derive(Default)]
struct Routes(VecDeque<(SocketAddrV4, usize)>);

impl Routes {
    fn note(&mut self, src: SocketAddrV4, at: usize) {
        self.0.retain(|(s, _)| *s != src);
        self.0.push_back((src, at));
        if self.0.len() > ROUTES_MAX {
            self.0.pop_front();
        }
    }

    fn get(&self, src: SocketAddrV4) -> Option<usize> {
        self.0.iter().find(|(s, _)| *s == src).map(|(_, at)| *at)
    }
}

/// 受け口 `at` で受けた 1 通を状態機械へ渡す。範囲 lan の外の送り元は相手にしない（WAN から届いたものを読まない）。ポート 0 も断る。
fn host_receive(
    host: &mut Host,
    routes: &mut Routes,
    at: usize,
    from: SocketAddr,
    bytes: &[u8],
    now: u64,
) -> Vec<HostAct> {
    let Some(src) = v4(from).filter(|s| from_lan(*s)) else {
        return Vec::new();
    };
    routes.note(src, at);
    host.on_datagram(src, bytes, now)
}

/// ホストの務め。自分の LAN のアドレスの受け口を開いて参加者を待ち、先着 1 人を受けたらゲームのネットワークを開いてコードを受け渡し、
/// 対戦が成立した後は観戦の申し込みに観戦コードを答える。参加者が返すコードを返さないまま上限が過ぎたら、開いたネットワークのまま待機へ戻る。
pub fn run_host(env: &LanEnv) -> End {
    let mut say = Say::new(&env.link);
    if !wait_for(NET_FREE_WAIT, || (env.net_free)(), &env.stop) {
        say.end(Event::Internal);
        return End::Failed(Event::Internal);
    }
    let ips = match (env.addresses)() {
        Ok(v) if !v.is_empty() => v,
        _ => {
            say.end(Event::LanNoAddress);
            return End::Failed(Event::LanNoAddress);
        }
    };
    let mut socks: Vec<(Ipv4Addr, UdpSocket)> = Vec::new();
    for ip in ips {
        if let Ok(s) = UdpSocket::bind(SocketAddrV4::new(ip, env.port))
            && s.set_nonblocking(true).is_ok()
        {
            socks.push((ip, s));
        }
    }
    if socks.is_empty() {
        say.note.lan_port = env.port;
        say.end(Event::LanPortBusy);
        return End::Failed(Event::LanPortBusy);
    }
    // 画面に出すのは受け口を開けたアドレス（有線・無線の両方があれば両方）。
    say.note.lan_port = env.port;
    for (i, (ip, _)) in socks.iter().take(LAN_IPS_MAX).enumerate() {
        say.note.lan_ips[i] = ip.octets();
        say.note.lan_n = (i + 1) as u8;
    }
    say.note.since = (env.unix)() as u32;
    say.put(CodeState::LanWaiting, Event::None);

    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut host = Host::new(env.ver, env.watch_ok).with_reply_wait(env.reply_wait_ms);
    let mut bridge: Option<Arc<LanBridge>> = None;
    let mut routes = Routes::default();
    // ゲームのネットワークを開いたアドレス。待機へ戻った後の参加者は、このアドレスに届いた人だけ受けられる（ネットワークは開き直さない）。
    let mut net_ip: Option<Ipv4Addr> = None;
    let mut buf = [0u8; MAX_DATAGRAM + 64];
    loop {
        if env.stopped() {
            say.end(Event::LanCancelled);
            return End::Stopped;
        }
        // ネットワークを開く前だけ、取りやめの指示を自分で読む。開いた後は見張りが読み、取りやめは `LanBridge::abort` で届く。
        if bridge.is_none() && cancel_asked(&env.link) {
            say.end(Event::LanCancelled);
            return End::Cancelled;
        }
        if let Some(b) = &bridge {
            if b.is_closed() || b.is_aborted() {
                return End::Done;
            }
            if b.is_connected() {
                host.linked();
            }
            if let Some(code) = b.take_outgoing() {
                let acts = host.offer_ready(&code, now());
                for a in acts {
                    send_host_act(&socks, &routes, a);
                }
            }
            while let Some((src, id, answer)) = b.take_watch_answer() {
                let acts = match answer {
                    Ok(code) => host.watch_code_ready(src, id, &code),
                    Err(why) => host.watch_refused(src, id, why),
                };
                for a in acts {
                    send_host_act(&socks, &routes, a);
                }
            }
        }
        let mut acts = Vec::new();
        for (i, (_, s)) in socks.iter().enumerate() {
            while let Ok((n, from)) = s.recv_from(&mut buf) {
                acts.extend(host_receive(
                    &mut host,
                    &mut routes,
                    i,
                    from,
                    &buf[..n],
                    now(),
                ));
            }
        }
        acts.extend(host.tick(now()));
        for a in acts {
            match a {
                HostAct::Send { .. } => send_host_act(&socks, &routes, a),
                HostAct::Accepted { src } => {
                    let ip = routes
                        .get(src)
                        .and_then(|at| socks.get(at))
                        .map_or(Ipv4Addr::LOCALHOST, |(ip, _)| *ip);
                    match (&bridge, net_ip) {
                        // 待機へ戻った後: 開いたままのネットワークで、新しいコードを作らせる（前のコードは前の参加者に渡した）。
                        (Some(b), Some(on)) if on == ip => {
                            say.note.name = [0; NAME_UNITS];
                            say.put(CodeState::LanLinking, Event::LanPeer);
                            b.post_cmd(BridgeCmd::Create);
                        }
                        // ネットワークは別のアドレスで開いている。その相手に届くとは限らないので断る。
                        (Some(_), _) => {
                            for a in host.refuse_taken() {
                                send_host_act(&socks, &routes, a);
                            }
                        }
                        (None, _) => {
                            say.note.name = [0; NAME_UNITS];
                            say.put(CodeState::LanLinking, Event::LanPeer);
                            let b = LanBridge::new();
                            if (env.open_net)(ip, LanRole::Host, Arc::clone(&b)).is_err() {
                                say.end(Event::Internal);
                                return End::Failed(Event::Internal);
                            }
                            if !b.wait_attached(ATTACH_WAIT) {
                                say.end(Event::Internal);
                                return End::Failed(Event::Internal);
                            }
                            b.post_cmd(BridgeCmd::Create);
                            bridge = Some(b);
                            net_ip = Some(ip);
                        }
                    }
                }
                HostAct::DeliverReply { code } => {
                    if let Some(b) = &bridge {
                        b.set_incoming(&code);
                        b.post_cmd(BridgeCmd::Paste);
                    }
                }
                HostAct::WatchWanted { src, id } => {
                    if let Some(b) = &bridge {
                        b.post_cmd(BridgeCmd::WatchCode { src, id });
                    }
                }
                // 参加者が返すコードを返さなかった（取りやめた・打ち間違えた）。ネットワークは開いたまま、待機の画面へ戻す。
                HostAct::Rewait => {
                    say.note.name = [0; NAME_UNITS];
                    say.put(CodeState::LanWaiting, Event::None);
                }
                HostAct::TimedOut => {
                    if let Some(b) = &bridge {
                        b.fail(Event::Internal);
                    }
                    return End::Failed(Event::Internal);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// 範囲 lan の外の送り元は相手にしない（WAN から届いたものを読まない）。ポート 0 も断る。
fn from_lan(src: SocketAddrV4) -> bool {
    guard::allows(Scope::Lan, SocketAddr::V4(src))
}

fn send_host_act(socks: &[(Ipv4Addr, UdpSocket)], routes: &Routes, a: HostAct) {
    if let HostAct::Send { to, bytes } = a {
        // 宛先の申し込みを受けた受け口から返す（その相手に届く自分のアドレスから出す）。返事はどれも受けた 1 通への答えなので、覚えていない
        // 宛先は、上限を越えて忘れたときだけ。そのときは先頭の受け口から出す。
        let at = routes.get(to).unwrap_or(0);
        if let Some((_, s)) = socks.get(at) {
            let _ = s.send_to(&bytes, to);
        }
    }
}

// ---- 申し込む側の下ごしらえ ----

/// ホストのアドレス宛ての UDP ソケットと、そのホストに届く自分のアドレス（OS の経路で決まる）。
fn dial(addr: LanAddr) -> Result<(UdpSocket, Ipv4Addr), String> {
    let to = SocketAddr::V4(addr.socket());
    if !guard::allows(Scope::Lan, to) {
        return Err("LAN の外のアドレス".to_string());
    }
    let s = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|e| format!("ソケットを開けない: {e}"))?;
    s.connect(to)
        .map_err(|e| format!("ホストへの経路が無い: {e}"))?;
    s.set_nonblocking(true)
        .map_err(|e| format!("ソケットを設定できない: {e}"))?;
    let local = match s.local_addr().map_err(|e| e.to_string())? {
        SocketAddr::V4(v) => *v.ip(),
        SocketAddr::V6(_) => return Err("IPv4 でない".to_string()),
    };
    Ok((s, local))
}

// ---- クライアント ----

/// クライアントの務め。ホストのアドレスへ参加を申し込み、始める側のコードを受けたらゲームのネットワークを開いて見張りに読ませ、
/// 見張りが作った返すコードをホストへ返して、受け取りの確認を待つ。
pub fn run_client(env: &LanEnv, addr: LanAddr) -> End {
    let mut say = Say::new(&env.link);
    let (sock, local) = match dial(addr) {
        Ok(v) => v,
        Err(_) => {
            say.end(Event::LanNoHost);
            return End::Failed(Event::LanNoHost);
        }
    };
    say.note.name = [0; NAME_UNITS];
    say.put(CodeState::LanLinking, Event::LanPeer);
    // 前のネットワークの片づけ（待っているだけのセッションを `request_lan` が手放した）が済むまで申し込まない。済む前に始める側のコードが
    // 届くと、ゲームのネットワークを開けない。
    if !wait_for(NET_FREE_WAIT, || (env.net_free)(), &env.stop) {
        if env.stopped() {
            say.end(Event::LanCancelled);
            return End::Stopped;
        }
        say.end(Event::Internal);
        return End::Failed(Event::Internal);
    }
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut client = Client::new(env.ver, env.id, now());
    let mut bridge: Option<Arc<LanBridge>> = None;
    let mut buf = [0u8; MAX_DATAGRAM + 64];
    let mut acts = client.start();
    loop {
        if env.stopped() {
            say.end(Event::LanCancelled);
            return End::Stopped;
        }
        if bridge.is_none() && cancel_asked(&env.link) {
            say.end(Event::LanCancelled);
            return End::Cancelled;
        }
        if let Some(b) = &bridge {
            if b.is_closed() || b.is_aborted() {
                return End::Done;
            }
            if let Some(code) = b.take_outgoing() {
                acts.extend(client.reply_ready(&code, now()));
            }
        }
        while let Ok(n) = sock.recv(&mut buf) {
            acts.extend(client.on_datagram(&buf[..n]));
        }
        acts.extend(client.tick(now()));
        for a in std::mem::take(&mut acts) {
            match a {
                ClientAct::Send(bytes) => {
                    let _ = sock.send(&bytes);
                }
                ClientAct::DeliverOffer { code } => {
                    let b = LanBridge::new();
                    if (env.open_net)(local, LanRole::Client, Arc::clone(&b)).is_err()
                        || !b.wait_attached(ATTACH_WAIT)
                    {
                        say.end(Event::Internal);
                        return End::Failed(Event::Internal);
                    }
                    b.set_incoming(&code);
                    b.post_cmd(BridgeCmd::Paste);
                    bridge = Some(b);
                }
                ClientAct::Done => return End::Done,
                ClientAct::Refused(why) => {
                    let ev = why_event(why, &mut say);
                    say.end(ev);
                    return End::Failed(ev);
                }
                ClientAct::TimedOut(ClientStage::Offer) => {
                    say.end(Event::LanNoHost);
                    return End::Failed(Event::LanNoHost);
                }
                ClientAct::TimedOut(ClientStage::Ack) => {
                    if let Some(b) = &bridge {
                        b.fail(Event::LanNoLink);
                    }
                    return End::Failed(Event::LanNoLink);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

// ---- 観戦 ----

/// 観戦の務め。ホストのアドレスへ観戦コードを頼み、受けたら観戦者のネットワークを開く頼みを出す。前のネットワークの片づけは待たない
/// （観戦者のネットワークを開く頼みは、片づけの途中なら片づけ終えてから開く）。
pub fn run_watch(env: &LanEnv, addr: LanAddr) -> End {
    let mut say = Say::new(&env.link);
    let (sock, local) = match dial(addr) {
        Ok(v) => v,
        Err(_) => {
            say.end(Event::LanNoHost);
            return End::Failed(Event::LanNoHost);
        }
    };
    say.note.name = [0; NAME_UNITS];
    say.put(CodeState::LanLinking, Event::LanPeer);
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut w = Watcher::new(env.ver, env.id, now());
    let mut acts = w.start();
    let mut buf = [0u8; MAX_DATAGRAM + 64];
    loop {
        if env.stopped() {
            say.end(Event::LanCancelled);
            return End::Stopped;
        }
        if cancel_asked(&env.link) {
            say.end(Event::LanCancelled);
            return End::Cancelled;
        }
        while let Ok(n) = sock.recv(&mut buf) {
            acts.extend(w.on_datagram(&buf[..n]));
        }
        acts.extend(w.tick(now()));
        for a in std::mem::take(&mut acts) {
            match a {
                WatcherAct::Send(bytes) => {
                    let _ = sock.send(&bytes);
                }
                WatcherAct::DeliverCode { code } => {
                    return match (env.start_watch)(code, local) {
                        Ok(()) => End::Done,
                        Err(_) => {
                            say.end(Event::Internal);
                            End::Failed(Event::Internal)
                        }
                    };
                }
                WatcherAct::Refused(why) => {
                    let ev = why_event(why, &mut say);
                    say.end(ev);
                    return End::Failed(ev);
                }
                WatcherAct::TimedOut => {
                    say.end(Event::LanNoHost);
                    return End::Failed(Event::LanNoHost);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// 同時に動かす運び手は 1 本まで（動いている間に次の頼みが来たら断る）。
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 運び手のスレッドを起こす。動いている運び手があれば起こさず `false`（呼び手は読み捨てる）。終わりに `done` を呼ぶ。
pub fn spawn(
    name: &str,
    env: LanEnv,
    run: impl FnOnce(&LanEnv) -> End + Send + 'static,
    done: impl FnOnce(End) + Send + 'static,
) -> Result<bool, String> {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }
    let spawned = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // パニックは受けて外へ出さない（スレッドの入口）。
            let end = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&env)))
                .unwrap_or(End::Failed(Event::Internal));
            ACTIVE.store(false, Ordering::SeqCst);
            done(end);
        });
    match spawned {
        Ok(_) => Ok(true),
        Err(e) => {
            ACTIVE.store(false, Ordering::SeqCst);
            Err(format!("LAN の運び手のスレッドを起こせない: {e}"))
        }
    }
}

/// 運び手が動いているか。
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lan_proto::{Msg, decode, encode};
    use higanr_core::net_menu::Event as Ev;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::thread;

    const VER: Ver = Ver { proto: 7, build: 0 };

    fn port_free() -> u16 {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    }

    /// 偽の見張り: 実物の見張りが `LanBridge` に対してすること（動き出して、指示を取って、コードを書く・読む）だけを真似る。
    fn fake_watcher(bridge: Arc<LanBridge>, role: LanRole, log: Arc<Mutex<Vec<String>>>) {
        thread::spawn(move || {
            bridge.attach();
            let t = Instant::now();
            let mut linked = false;
            let mut made = 0;
            while t.elapsed() < Duration::from_secs(10) && !bridge.is_aborted() {
                match bridge.take_cmd() {
                    // 2 回目からは別のコード（作り直したことが分かるように）。
                    Some(BridgeCmd::Create) => {
                        made += 1;
                        log.lock().unwrap().push("create".into());
                        bridge.post_outgoing(&if made == 1 {
                            "OFFER-CODE-FROM-HOST".to_string()
                        } else {
                            format!("OFFER-CODE-FROM-HOST-{made}")
                        });
                    }
                    Some(BridgeCmd::Paste) => {
                        let read = bridge.read_incoming().unwrap_or_default();
                        log.lock().unwrap().push(format!("paste:{read}"));
                        match role {
                            LanRole::Client => bridge.post_outgoing("REPLY-CODE-FROM-CLIENT"),
                            LanRole::Host => {
                                bridge.set_connected();
                                linked = true;
                            }
                        }
                    }
                    Some(BridgeCmd::WatchCode { src, id }) => {
                        log.lock().unwrap().push("watchcode".into());
                        bridge.post_watch_answer(src, id, Ok("WATCH-CODE".into()));
                    }
                    None => {}
                }
                if role == LanRole::Client && linked {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if role == LanRole::Client {
                // クライアントの見張りはコードの受け渡しの後も居る。運び手が先に終わるのを待ってから閉じる。
                thread::sleep(Duration::from_millis(300));
                bridge.close();
            }
        });
    }

    struct Rig {
        env: LanEnv,
        opened: Arc<Mutex<Vec<(Ipv4Addr, LanRole)>>>,
        watch: Arc<Mutex<Vec<(String, Ipv4Addr)>>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    fn rig(port: u16, id: u32, watch_ok: bool) -> Rig {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let watch = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (o, l) = (Arc::clone(&opened), Arc::clone(&log));
        let w = Arc::clone(&watch);
        let env = LanEnv {
            link: Arc::new(UiLink::new()),
            ver: VER,
            watch_ok,
            port,
            reply_wait_ms: higanr_core::lan_proto::HOST_WAIT_REPLY_MS,
            addresses: Box::new(|| Ok(vec![Ipv4Addr::LOCALHOST])),
            net_free: Box::new(|| true),
            open_net: Box::new(move |ip, role, b| {
                o.lock().unwrap().push((ip, role));
                fake_watcher(b, role, Arc::clone(&l));
                Ok(())
            }),
            start_watch: Box::new(move |code, ip| {
                w.lock().unwrap().push((code, ip));
                Ok(())
            }),
            id,
            unix: || 1_000_000,
            stop: Arc::new(AtomicBool::new(false)),
        };
        Rig {
            env,
            opened,
            watch,
            log,
        }
    }

    fn last_note(link: &UiLink) -> Note {
        link.note().expect("札がある")
    }

    fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    /// 別のスレッドで回す運び手。戻りは上限つきで受ける（取りやめ・止めを読まない変異で検査が止まらないように）。
    struct Bg<T> {
        rx: mpsc::Receiver<T>,
        stop: Arc<AtomicBool>,
    }

    fn bg<T: Send + 'static>(
        stop: &Arc<AtomicBool>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Bg<T> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        Bg {
            rx,
            stop: Arc::clone(stop),
        }
    }

    impl<T> Bg<T> {
        /// `limit` のうちに戻らなければ、止めを立てて落ちる。
        fn wait(self, limit: Duration) -> T {
            match self.rx.recv_timeout(limit) {
                Ok(v) => v,
                Err(e) => {
                    self.stop.store(true, Ordering::SeqCst);
                    panic!("運び手が {limit:?} のうちに終わらない（{e:?}）");
                }
            }
        }

        /// 外から止めて、戻りを受ける。
        fn stop(self) -> T {
            self.stop.store(true, Ordering::SeqCst);
            self.wait(DONE)
        }
    }

    /// 運び手が終わるのを待つ上限（申し込みの待ちの上限 10 秒より長く）。
    const DONE: Duration = Duration::from_secs(15);

    fn host_bg(host: Rig) -> Bg<End> {
        let stop = Arc::clone(&host.env.stop);
        bg(&stop, move || run_host(&host.env))
    }

    fn client_bg(c: Rig, addr: LanAddr) -> Bg<End> {
        let stop = Arc::clone(&c.env.stop);
        bg(&stop, move || run_client(&c.env, addr))
    }

    fn waiting(link: &UiLink) -> bool {
        wait_until(Duration::from_secs(5), || {
            link.note()
                .is_some_and(|n| n.state == CodeState::LanWaiting)
        })
    }

    /// 壊れたら落ちる: ホストとクライアントの間で、始める側のコードと返すコードが渡らない、ネットワークを開く役・アドレスが違う、
    /// ホストの画面に待機のアドレスが出ない、成立した後の観戦の申し込みに観戦コードが返らない。
    #[test]
    fn host_client_and_watcher_exchange_the_codes_over_loopback() {
        let port = port_free();
        let host = rig(port, 0, true);
        let host_link = Arc::clone(&host.env.link);
        let host_opened = Arc::clone(&host.opened);
        let host_log = Arc::clone(&host.log);
        let h = host_bg(host);
        // 待機の札: LAN の待機・自分のアドレスとポート。
        assert!(waiting(&host_link));
        let n = last_note(&host_link);
        assert_eq!(
            (n.lan_n, n.lan_ips[0], n.lan_port),
            (1, [127, 0, 0, 1], port)
        );
        // クライアント。
        let cl = rig(0, 4242, true);
        let cl_link = Arc::clone(&cl.env.link);
        let (c_opened, c_log) = (Arc::clone(&cl.opened), Arc::clone(&cl.log));
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        assert_eq!(
            client_bg(cl, addr).wait(DONE),
            End::Done,
            "クライアントの務め"
        );
        assert_eq!(
            *host_opened.lock().unwrap(),
            vec![(Ipv4Addr::LOCALHOST, LanRole::Host)]
        );
        assert_eq!(
            *c_opened.lock().unwrap(),
            vec![(Ipv4Addr::LOCALHOST, LanRole::Client)]
        );
        // クライアントの見張りは始める側のコードを読み、ホストの見張りは返すコードを読む。
        assert_eq!(
            *c_log.lock().unwrap(),
            vec!["paste:OFFER-CODE-FROM-HOST".to_string()]
        );
        assert!(wait_until(Duration::from_secs(5), || host_log
            .lock()
            .unwrap()
            .contains(&"paste:REPLY-CODE-FROM-CLIENT".to_string())));
        assert_eq!(host_log.lock().unwrap()[0], "create");
        // 接続の途中の札（クライアントの画面）。見張りが引き継ぐ前の最後の札。
        let cn = last_note(&cl_link);
        assert_eq!((cn.state, cn.event), (CodeState::LanLinking, Ev::LanPeer));
        // 観戦: 成立の後なら観戦コードが返り、観戦者のネットワークを開く頼みが出る。
        let w = rig(0, 77, true);
        let w_watch = Arc::clone(&w.watch);
        let end = {
            let wa = LanAddr {
                ip: Ipv4Addr::LOCALHOST,
                port,
            };
            // 成立の印が運び手に届くのを待つ（偽の見張りが set_connected した後、運び手が見回る）。
            thread::sleep(Duration::from_millis(300));
            run_watch(&w.env, wa)
        };
        assert_eq!(end, End::Done);
        assert_eq!(
            *w_watch.lock().unwrap(),
            vec![("WATCH-CODE".to_string(), Ipv4Addr::LOCALHOST)]
        );
        // ホストの務めを外から止める（偽の見張りは閉じないので）。
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: 運び手の受け取り（`run_host` が受けた 1 通ごとに呼ぶ `host_receive`）が、LAN の外・ポート 0・マルチキャスト・IPv6 の
    /// 送り元の申し込みを状態機械へ渡す（WAN から届いた申し込みで先着が埋まる）、LAN の送り元を受けない・受け口を覚えない。
    #[test]
    fn only_lan_sources_are_answered() {
        let join = encode(&Msg::Join {
            id: 1,
            proto: VER.proto,
            build: VER.build,
        })
        .unwrap();
        for a in [
            "8.8.8.8:5000",
            "203.0.113.5:5000",
            "100.64.0.1:5000",
            "224.0.0.1:5000",
            "255.255.255.255:5000",
            "192.168.0.5:0",
            "[fe80::1]:5000",
        ] {
            let mut host = Host::new(VER, true);
            let mut routes = Routes::default();
            let from: SocketAddr = a.parse().unwrap();
            assert!(
                host_receive(&mut host, &mut routes, 1, from, &join, 0).is_empty(),
                "{a}"
            );
            assert!(!host.is_taken(), "{a} で先着が埋まった");
            assert!(routes.0.is_empty(), "{a}");
        }
        for a in [
            "192.168.0.5:5000",
            "10.1.2.3:1",
            "172.16.9.9:65535",
            "169.254.7.7:40000",
            "127.0.0.1:9",
        ] {
            let mut host = Host::new(VER, true);
            let mut routes = Routes::default();
            let src: SocketAddrV4 = a.parse().unwrap();
            assert_eq!(
                host_receive(&mut host, &mut routes, 1, SocketAddr::V4(src), &join, 0),
                vec![HostAct::Accepted { src }],
                "{a}"
            );
            assert_eq!(routes.get(src), Some(1), "{a}");
        }
    }

    /// 壊れたら落ちる: ホストの画面の待機のアドレスが受け口を開けた全部でない、申し込みが届いたアドレスと違うアドレスでゲームのネットワークを開く
    /// （返事が申し込みの届いたアドレスから出ない — 直結の相手に届かない）。127.0.0.2 は 127.0.0.1 と別のアドレス（有線と無線の両方がある PC の見立て）。
    #[test]
    fn the_network_opens_on_the_address_the_request_arrived_at() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let second = Ipv4Addr::new(127, 0, 0, 2);
        host.env.addresses = Box::new(move || Ok(vec![Ipv4Addr::LOCALHOST, second]));
        let link = Arc::clone(&host.env.link);
        let opened = Arc::clone(&host.opened);
        let h = host_bg(host);
        assert!(waiting(&link));
        let n = last_note(&link);
        assert_eq!(
            (n.lan_n, n.lan_ips[0], n.lan_ips[1]),
            (2, [127, 0, 0, 1], [127, 0, 0, 2]),
            "有線と無線の両方のアドレスを出す"
        );
        // 2 つ目のアドレスへ申し込む。返事がそのアドレスから出なければ、つないだソケットは受け取らず「返事がない」で終わる。
        let c = rig(0, 31, true);
        let end = client_bg(c, LanAddr { ip: second, port }).wait(DONE);
        assert_eq!(end, End::Done);
        assert_eq!(
            *opened.lock().unwrap(),
            vec![(second, LanRole::Host)],
            "申し込みが届いたアドレスで開く"
        );
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: 先着の参加者と別のアドレスに届いた申し込みへの返事（観戦コード・断り）が、先着の参加者を受けた受け口から出る
    /// （申し込んだ側の `connect` 済みのソケットが受け取らず、観戦できない・断りの理由が出ない）。
    #[test]
    fn each_answer_leaves_from_the_address_its_request_arrived_at() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let second = Ipv4Addr::new(127, 0, 0, 2);
        host.env.addresses = Box::new(move || Ok(vec![Ipv4Addr::LOCALHOST, second]));
        let link = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link));
        // 先着は 127.0.0.1 へ。
        let first = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        assert_eq!(client_bg(rig(0, 41, true), first).wait(DONE), End::Done);
        let at_second = LanAddr { ip: second, port };
        // 観戦は 127.0.0.2 へ（成立の印が運び手に届くまでの短い間は「まだ」と断られる）。
        let w = rig(0, 42, true);
        let w_watch = Arc::clone(&w.watch);
        let until = Instant::now() + Duration::from_secs(3);
        let end = loop {
            let end = run_watch(&w.env, at_second);
            if end != End::Failed(Ev::LanNoMatch) || Instant::now() > until {
                break end;
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(end, End::Done, "観戦コードが届かない");
        assert_eq!(w_watch.lock().unwrap().len(), 1);
        // 別のビルドの参加の申し込みも 127.0.0.2 へ。断りの理由が届く。
        let mut other = rig(0, 43, true);
        other.env.ver = Ver {
            proto: VER.proto,
            build: 999,
        };
        assert_eq!(
            client_bg(other, at_second).wait(DONE),
            End::Failed(Ev::OtherBuild)
        );
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: 返すコードを返さない参加者（打ち間違えて取りやめた人）のせいで待機が終わる、待機の画面へ戻らない、次の参加者のために
    /// ネットワークを開き直す（開いたままのネットワークがあるので開けない）、前と同じコードを渡す、ネットワークと別のアドレスに届いた参加者を受ける
    /// （その相手には届かないアドレスのコードを渡す）。
    #[test]
    fn a_silent_joiner_sends_the_host_back_to_waiting_and_the_next_one_gets_a_new_code() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let second = Ipv4Addr::new(127, 0, 0, 2);
        host.env.addresses = Box::new(move || Ok(vec![Ipv4Addr::LOCALHOST, second]));
        host.env.reply_wait_ms = 400;
        let link = Arc::clone(&host.env.link);
        let opened = Arc::clone(&host.opened);
        let h = host_bg(host);
        assert!(waiting(&link));
        // 黙る参加者: JOIN を 1 通送り、始める側のコードを受けたら何も返さない。
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        silent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let join = |id| {
            encode(&Msg::Join {
                id,
                proto: VER.proto,
                build: VER.build,
            })
            .unwrap()
        };
        silent
            .send_to(&join(1), (Ipv4Addr::LOCALHOST, port))
            .unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = silent.recv_from(&mut buf).unwrap();
        assert_eq!(
            decode(&buf[..n]),
            Ok(Msg::Offer {
                id: 1,
                code: "OFFER-CODE-FROM-HOST"
            })
        );
        // 上限が過ぎたら待機の画面へ戻る（運び手は終わらない）。
        assert!(
            wait_until(Duration::from_secs(5), || {
                link.note()
                    .is_some_and(|n| n.state == CodeState::LanWaiting)
            }),
            "待機へ戻らない: {:?}",
            link.note()
        );
        // ネットワークと別のアドレス（127.0.0.2）に届いた参加者は断る（返事は 127.0.0.2 から出る）。
        let other = UdpSocket::bind("127.0.0.1:0").unwrap();
        other
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        other.send_to(&join(2), (second, port)).unwrap();
        let (n, from) = other.recv_from(&mut buf).unwrap();
        assert_eq!(
            decode(&buf[..n]),
            Ok(Msg::Refuse {
                id: 2,
                why: Why::Busy
            })
        );
        assert_eq!(from.ip(), second);
        // 次の参加者は 127.0.0.1 へ: 開いたままのネットワークで、作り直したコードを受け取って接続する。
        let c = rig(0, 3, true);
        let c_log = Arc::clone(&c.log);
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        assert_eq!(client_bg(c, addr).wait(DONE), End::Done);
        assert_eq!(
            *c_log.lock().unwrap(),
            vec!["paste:OFFER-CODE-FROM-HOST-2".to_string()]
        );
        assert_eq!(
            *opened.lock().unwrap(),
            vec![(Ipv4Addr::LOCALHOST, LanRole::Host)],
            "ネットワークを開き直した"
        );
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: 見張りが待ちをやめた（取りやめ・失敗）のに、ホストの運び手が務めを続ける（次のホストを始められない）。
    #[test]
    fn the_host_carrier_ends_when_the_watcher_gives_up_or_closes() {
        for how in ["abort", "close"] {
            let port = port_free();
            let mut host = rig(port, 0, true);
            let keep: Arc<Mutex<Option<Arc<LanBridge>>>> = Arc::new(Mutex::new(None));
            let k = Arc::clone(&keep);
            host.env.open_net = Box::new(move |_, _, b| {
                b.attach();
                *k.lock().unwrap() = Some(b);
                Ok(())
            });
            let link = Arc::clone(&host.env.link);
            let h = host_bg(host);
            assert!(waiting(&link));
            let addr = LanAddr {
                ip: Ipv4Addr::LOCALHOST,
                port,
            };
            // 返すコードは来ないので、クライアントの側は待ち続ける。ホストの運び手だけを見る。
            let cl = client_bg(rig(0, 5, true), addr);
            assert!(wait_until(Duration::from_secs(5), || keep
                .lock()
                .unwrap()
                .is_some()));
            let b = keep.lock().unwrap().clone().unwrap();
            if how == "abort" {
                b.abort();
            } else {
                b.close();
            }
            assert_eq!(h.wait(DONE), End::Done, "{how}");
            cl.stop();
        }
    }

    /// 壊れたら落ちる: 2 人目の参加者が断られない、断られたクライアントが理由を画面へ出さない。
    #[test]
    fn the_second_client_is_told_the_host_is_busy() {
        let port = port_free();
        let host = rig(port, 0, true);
        let link_h = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link_h));
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        assert_eq!(client_bg(rig(0, 1, true), addr).wait(DONE), End::Done);
        let second = rig(0, 2, true);
        let link = Arc::clone(&second.env.link);
        assert_eq!(client_bg(second, addr).wait(DONE), End::Failed(Ev::LanBusy));
        let n = last_note(&link);
        assert_eq!((n.state, n.event), (CodeState::Idle, Ev::LanBusy));
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: 観戦の申し込みが対戦の成立の前に観戦コードを受け取る、観戦を受けない設定でも通る。
    #[test]
    fn a_watch_request_before_the_match_or_with_watching_off_is_refused() {
        let port = port_free();
        let host = rig(port, 0, true);
        let link_h = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link_h));
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        let w = rig(0, 5, true);
        let link = Arc::clone(&w.env.link);
        assert_eq!(run_watch(&w.env, addr), End::Failed(Ev::LanNoMatch));
        assert_eq!(last_note(&link).event, Ev::LanNoMatch);
        assert!(w.watch.lock().unwrap().is_empty());
        assert_eq!(h.stop(), End::Stopped);
        // 観戦を受けない設定。
        let port = port_free();
        let host = rig(port, 0, false);
        let link_h = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link_h));
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        let w = rig(0, 6, true);
        assert_eq!(run_watch(&w.env, addr), End::Failed(Ev::LanWatchOff));
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: バージョンやビルドの違うクライアントを、ネットワークを開く前に断らない。
    #[test]
    fn a_different_build_is_refused_before_any_network_is_opened() {
        let port = port_free();
        let host = rig(port, 0, true);
        let opened = Arc::clone(&host.opened);
        let link_h = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link_h));
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        let mut other = rig(0, 9, true);
        other.env.ver = Ver {
            proto: 7,
            build: 123,
        };
        let link = Arc::clone(&other.env.link);
        assert_eq!(
            client_bg(other, addr).wait(DONE),
            End::Failed(Ev::OtherBuild)
        );
        let n = last_note(&link);
        assert_eq!((n.event, n.peer_build), (Ev::OtherBuild, 0));
        assert!(
            opened.lock().unwrap().is_empty(),
            "断ったのにネットワークを開いた"
        );
        assert_eq!(h.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: LAN の外のアドレスへの申し込みをソケットの段で断らない（`dial` のガード。外したら最初の確かめで落ち、何も送らない —
    /// UDP の `connect` はパケットを出さない）、アドレスが無いホストが待ちを始める、使われているポートで待つ。
    #[test]
    fn an_outside_address_no_address_and_a_busy_port_end_with_their_own_events() {
        // 文書用のアドレス（TEST-NET-3）。LAN の外。
        let outside = LanAddr {
            ip: Ipv4Addr::new(203, 0, 113, 5),
            port: 17723,
        };
        assert_eq!(
            dial(outside).err().as_deref(),
            Some("LAN の外のアドレス"),
            "LAN の外へ申し込むソケットを作った"
        );
        let c = rig(0, 1, true);
        let link = Arc::clone(&c.env.link);
        assert_eq!(
            client_bg(c, outside).wait(Duration::from_secs(2)),
            End::Failed(Ev::LanNoHost)
        );
        assert_eq!(last_note(&link).event, Ev::LanNoHost);
        // 自分の LAN のアドレスが無い。
        let mut h = rig(port_free(), 0, true);
        h.env.addresses = Box::new(|| Ok(Vec::new()));
        assert_eq!(run_host(&h.env), End::Failed(Ev::LanNoAddress));
        assert_eq!(last_note(&h.env.link).event, Ev::LanNoAddress);
        // 待ち受けのポートが使われている。
        let busy = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let h = rig(port, 0, true);
        assert_eq!(run_host(&h.env), End::Failed(Ev::LanPortBusy));
        assert_eq!(last_note(&h.env.link).lan_port, port);
    }

    /// 壊れたら落ちる: クライアントが前のネットワークの片づけを待たずに申し込む（片づけの前に始める側のコードが届くと、ゲームのネットワークを
    /// 開けずに失敗する）、片づけが済んでも申し込まない。
    #[test]
    fn the_client_joins_only_after_the_previous_network_is_cleared() {
        let fake_host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = fake_host.local_addr().unwrap().port();
        let freed = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&freed);
        let mut c = rig(0, 8, true);
        c.env.net_free = Box::new(move || f.load(Ordering::SeqCst));
        let cl = client_bg(
            c,
            LanAddr {
                ip: Ipv4Addr::LOCALHOST,
                port,
            },
        );
        let mut buf = [0u8; MAX_DATAGRAM];
        // 申し込みの送り直しの間隔（500 ms）より長く待っても、何も届かない。
        fake_host
            .set_read_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        assert!(
            fake_host.recv_from(&mut buf).is_err(),
            "片づけの前に申し込んだ"
        );
        freed.store(true, Ordering::SeqCst);
        fake_host
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (n, _) = fake_host
            .recv_from(&mut buf)
            .expect("片づけの後に申し込まない");
        assert!(matches!(decode(&buf[..n]), Ok(Msg::Join { id: 8, .. })));
        assert_eq!(cl.stop(), End::Stopped);
    }

    /// 壊れたら落ちる: ネットワークを開く前の取りやめの指示を運び手が読まない（ホストの待ちが終わらない）。
    #[test]
    fn cancel_before_the_network_opens_ends_the_wait_and_clears_the_menu() {
        let port = port_free();
        let host = rig(port, 0, true);
        let link = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link));
        link.send(higanr_core::net_menu::Cmd::Cancel);
        assert_eq!(h.wait(Duration::from_secs(5)), End::Cancelled);
        let n = last_note(&link);
        assert_eq!((n.state, n.event), (CodeState::Idle, Ev::LanCancelled));
        // ポートは閉じて、もう一度待てる。
        assert!(UdpSocket::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn only_one_carrier_runs_at_a_time() {
        let count = Arc::new(AtomicU32::new(0));
        let go = Arc::new(AtomicBool::new(false));
        let (c1, g1) = (Arc::clone(&count), Arc::clone(&go));
        let r1 = spawn(
            "t1",
            rig(0, 0, true).env,
            move |_| {
                while !g1.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                End::Done
            },
            move |_| {
                c1.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert_eq!(r1, Ok(true));
        assert!(is_active());
        let r2 = spawn("t2", rig(0, 0, true).env, |_| End::Done, |_| {});
        assert_eq!(r2, Ok(false), "動いている間は 2 本目を起こさない");
        go.store(true, Ordering::SeqCst);
        assert!(wait_until(Duration::from_secs(5), || count
            .load(Ordering::SeqCst)
            == 1));
        assert!(wait_until(Duration::from_secs(5), || !is_active()));
        // パニックは受けて、次を起こせる。
        let r3 = spawn(
            "t3",
            rig(0, 0, true).env,
            |_| panic!("検査のパニック"),
            |_| {},
        );
        assert_eq!(r3, Ok(true));
        assert!(wait_until(Duration::from_secs(5), || !is_active()));
    }
}
