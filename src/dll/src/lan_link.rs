//! LAN対戦の申し込みの運び手（stdのソケットだけ。unsafe無し）。
//! 接続コードの受け渡しをクリップボードやルームサーバーでなくLANのUDPでやる（判断と手順はcoreの`lan_proto`）。
//! ここはソケットを開いてステートマシンに受けたバイト列を渡し、出てきた「送る」「開く」「読ませる」を行うだけ。
//! コードの中身も接続の手順もインターネットの経路と同じ（ホストが`Swapper::offer`で作ったコードをクライアントへ返し、クライアントが返すコードをホストへ返す）。
//! - **別のスレッド・別のソケット**: 申し込みの受付用受信ソケットは対戦用受信ソケット（`net_udp`の受信ループ）とは別。ゲームのスレッドには何も足さない。
//!   ホストの受付用受信ソケットはLANの自分のアドレスごとに1つずつ（`bind(アドレス:ポート)`）。
//!   未指定アドレスで全部のアドレスを受ける待ち受けにはしない。理由はWAN側のアドレスで待たないことと、どのアドレスに申し込みが届いたか（＝その相手に届く自分のアドレス）をそのまま分かるようにすること。
//! - **ゲームのネットワークは参加を受けてから開く**: ホストは申し込みが届いたアドレスで開く。クライアントは申し込み先へ届く自分のアドレスで開く。
//!   開くときは`scope = lan`（VPNのアドレスではそのネットワークを足した範囲）・STUN・UPnP・IPv6なし（`LanEnv::open_net`）。
//!   監視の処理（`net_code_game::GameWatcher`）が`LanBridge`を通してコードを受け渡す。
//!   監視の処理は「作る」「貼る」と同じ手順を踏むが、コードの運び先がクリップボードでなく`LanBridge`。
//! - **監視の処理との受け渡し**: 運び手と監視の処理の間は`LanBridge`（ロックで守った小さなバッファ）。
//!   指示のリングバッファの読み手は監視の処理のまま。運び手はネットワークを開く前の取りやめの指示だけを自分で読み、開く頼みを出したら読まない。
//!   スレッドは申し込みの文脈（ホスト・クライアント・観戦の3つ）ごとに1本。取りやめ・失敗・成立して務めを終えたときにスレッドも終わる。

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use higanr_core::adapter_pick::{HostAddr, VpnLink};
use higanr_core::guard::{self, Scope};
use higanr_core::lan_addr::LanAddr;
use higanr_core::lan_proto::{
    Client, ClientAct, ClientStage, Host, HostAct, Ver, Watcher, WatcherAct, Why,
};
use higanr_core::net_menu::{CodeState, Event, LAN_IPS_MAX, NAME_UNITS, Note};
use higanr_core::vpn_net::{Net4, VpnTool};
use higanr_core::wire::MAX_DATAGRAM;

use crate::clipboard::Clipboard;
use crate::net_ui::{Order, UiLink};

/// ホストの受付用受信ソケットをポーリングする間隔。申し込みは人が待つ程度の頻度なので短くしない（待つだけで処理の重さは無い）。
const POLL: Duration = Duration::from_millis(20);
/// 申し込みを受けてからゲームのネットワークを開き、監視の処理が動き出すまで待つ上限。
const ATTACH_WAIT: Duration = Duration::from_secs(20);
/// 前のネットワークの後始末が済んで新しいネットワークを開ける（ポートが空く）のを待つ上限。
const NET_FREE_WAIT: Duration = Duration::from_secs(8);

/// ゲームのネットワークを開くときの役。ホストは役割の乱数を大きくしていつも試合のホスト（観戦コードを出す側）になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanRole {
    Host,
    Client,
}

/// 運び手が監視の処理にさせたいこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeCmd {
    /// 始める側のコードを作って`LanBridge`へ書く。
    Create,
    /// `LanBridge`にあるコードを読む（ホストは返すコードを読み、クライアントは始める側のコードを読む）。
    Paste,
    /// 観戦コードを作って答える（`LanBridge::post_watch_answer`。`via`は申し込みを受けた受付用受信ソケットの番号。答えにそのまま付けて返す）。
    WatchCode {
        src: SocketAddrV4,
        id: u32,
        via: usize,
    },
}

/// 観戦の申し込みへの答え（送信元・番号・受けた受付用受信ソケットの番号・観戦コードか断る理由）。
type WatchAnswer = (SocketAddrV4, u32, usize, Result<String, Why>);

#[derive(Default)]
struct Inner {
    attached: bool,
    closed: bool,
    connected: bool,
    aborted: bool,
    /// 監視の処理が書いたコード（運び手が取って相手へ送る）。
    outgoing: Option<String>,
    /// 運び手が書いたコード（監視の処理が`read`で読む。読んでも消さない）。
    incoming: Option<String>,
    cmds: VecDeque<BridgeCmd>,
    watch_answers: VecDeque<WatchAnswer>,
    failure: Option<Event>,
}

/// 運び手と監視の処理の受け渡し。
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

    // ---- 監視の処理の側 ----

    /// 監視の処理が動き出した。
    pub fn attach(&self) {
        self.with(|i| i.attached = true);
    }

    /// 監視の処理が終わった（以後は運び手が務めを終える）。
    pub fn close(&self) {
        self.with(|i| i.closed = true);
    }

    /// 対戦が成立した（ホストが観戦者からの申し込みを受け始める）。
    pub fn set_connected(&self) {
        self.with(|i| i.connected = true);
    }

    /// 監視の処理が待ちをやめた（メニューの取りやめ・失敗）。運び手は務めを終える。
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

    pub fn post_watch_answer(
        &self,
        src: SocketAddrV4,
        id: u32,
        via: usize,
        answer: Result<String, Why>,
    ) {
        self.with(|i| i.watch_answers.push_back((src, id, via, answer)));
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

    pub fn take_watch_answer(&self) -> Option<WatchAnswer> {
        self.with(|i| i.watch_answers.pop_front())
    }

    /// 監視の処理に、イベントを画面へ出させて待ちをやめさせる（時間切れなど）。
    pub fn fail(&self, ev: Event) {
        self.with(|i| i.failure = Some(ev));
    }

    /// 監視の処理が動き出すのを待つ。動き出したらtrue。
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

/// 監視の処理がコードを受け渡す「クリップボード」（`LanBridge`への書き込みと読み取り）。
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

/// ゲームのネットワークを開く頼み（`LanEnv::open_net`）。引数は自分のアドレス・開く範囲（`Scope::lan_via`）・役・監視の処理との受け渡し。
type OpenNet =
    Box<dyn Fn(Ipv4Addr, Scope, LanRole, Arc<LanBridge>) -> Result<(), String> + Send + Sync>;

/// 運び手の材料。製品は`script_win`が、検査は偽物を渡す。
pub struct LanEnv {
    pub link: Arc<UiLink>,
    pub ver: Ver,
    /// 観戦の申し込みを受ける設定かどうか（`[spectate] enable = 1`）。
    pub watch_ok: bool,
    /// ホストの受付用受信ソケットのポート。
    pub port: u16,
    /// ホストが返すコードを待つ上限（ms。製品は`lan_proto::HOST_WAIT_REPLY_MS`）。過ぎたら待機へ戻る。
    pub reply_wait_ms: u64,
    /// ホストの自分のLANのアドレス（本物のLAN → VPN → ほかの仮想の順）。
    pub addresses: Box<dyn Fn() -> Result<Vec<HostAddr>, String> + Send + Sync>,
    /// クライアント・観戦の自分のPCのVPNのアドレス（利用者が打ったアドレスがLANの外のとき、この中のネットワークなら受ける）。
    pub vpn_links: Box<dyn Fn() -> Vec<VpnLink> + Send + Sync>,
    /// 前のネットワークの後始末が済んでホストの受付用受信ソケットのポートが空いたかどうか。
    pub net_free: Box<dyn Fn() -> bool + Send + Sync>,
    /// ゲームのネットワークを`ip`で開く頼み（開くスレッドを作って返る。監視の処理は動き出したら`LanBridge::attach`を呼ぶ）。
    pub open_net: OpenNet,
    /// 観戦コードを受けた。観戦者のネットワークを`ip`・範囲`scope`で開く頼みを出す（観戦者のセッションを作るのはゲームのスレッド）。
    pub start_watch: Box<dyn Fn(String, Ipv4Addr, Scope) -> Result<(), String> + Send + Sync>,
    /// 本ログへ1行頼む（製品は`script_win`が本ログと「ネットワーク」の詳細へ書く。検査は記録する）。
    pub note: Box<dyn Fn(String) + Send + Sync>,
    /// 申し込みの番号（クライアント・観戦が生成する乱数）。
    pub id: u32,
    /// 今のUNIX秒（待機時間の起点）。
    pub unix: fn() -> u64,
    /// 外から止める（プロセスの終了時など）。
    pub stop: Arc<AtomicBool>,
}

impl LanEnv {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// ゲームのネットワークをVPNの範囲で開く頼みの前に本ログへ1行書く（範囲lanの接続では何も書かない）。
/// ツールの名前は`vpn_links`の`ip`で取得する（取得できなければ名前を省く）。アドレスは書かない。
fn note_vpn_open(env: &LanEnv, ip: Ipv4Addr, scope: Scope) {
    if let Scope::LanVpn(net) = scope {
        let tool = (env.vpn_links)()
            .iter()
            .find(|l| l.ip == ip)
            .map(|l| l.tool);
        (env.note)(vpn_open_line(tool, net.prefix()));
    }
}

/// 本ログのVPNの行（ツールの名前とネットマスクの長さだけ）。
fn vpn_open_line(tool: Option<VpnTool>, prefix: u8) -> String {
    match tool {
        Some(t) => format!(
            "LAN: {} のネットワーク（/{prefix}）を足した範囲で開きます。",
            t.name()
        ),
        None => format!("LAN: VPN のネットワーク（/{prefix}）を足した範囲で開きます。"),
    }
}

/// 状態のタグの書き手（運び手が監視の処理に引き継ぐ前だけ使う）。
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
        // ホストは監視の処理が動いた後もタグを書く（待機へ戻ったとき）。そのため監視の処理が書いた数の続きから数える（同じ数だとメニューが変化を見落とす）。
        let last = self.link.note().map_or(self.note.seq, |n| n.seq);
        self.note.seq = last.wrapping_add(1);
        self.link.publish(&self.note);
    }

    /// 待ちを終える（元のメニューへ戻して理由を残す）。
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
    /// 失敗して理由を画面へ出した。
    Failed(Event),
    /// 務めを終えた（コードの受け渡しが済んだ・監視の処理が終わった）。
    Done,
}

fn v4(a: SocketAddr) -> Option<SocketAddrV4> {
    match a {
        SocketAddr::V4(v) => Some(v),
        SocketAddr::V6(_) => None,
    }
}

/// 取りやめの指示が来ているかどうか（ネットワークを開く前だけ指示のリングバッファを読む。ほかの指示は読み捨てる）。
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
        Why::WatchOff => Event::WatchRefused,
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

/// 1回の巡回で1つの受付用受信ソケットから読む数の上限。
/// 読み取りより速く送られ続けても巡回を終え、外からの停止・取りやめ・コードの受け渡しを待たせない（読み残しは次の巡回で読む）。
/// 正直な申し込みは500 msごとに1通なので届くことはない。
const RECV_PER_POLL: usize = 256;

/// 開いた受付用受信ソケット1つ。
struct Listener {
    ip: Ipv4Addr,
    sock: UdpSocket,
    /// 私設でないVPNのアドレスの受付用受信ソケットなら、そのネットワーク。同じネットワークから来た申し込みだけを読み、ゲームのネットワークはこれを足した範囲で開く。
    net: Option<Net4>,
}

/// 受付用受信ソケット`at`（読む範囲は`vpn`。`Listener::net`）で受けた1通をステートマシンへ渡す。
/// 送信元が受付用受信ソケットの読んでよい範囲（`source_ok`）の外なら相手にしない（WANから届いたものを読まない）。
fn host_receive(
    host: &mut Host,
    (at, vpn): (usize, Option<Net4>),
    from: SocketAddr,
    bytes: &[u8],
    now: u64,
) -> Vec<HostAct> {
    let Some(src) = v4(from).filter(|s| source_ok(*s, vpn)) else {
        return Vec::new();
    };
    host.on_datagram(src, at, bytes, now)
}

/// 受付用受信ソケット`at`からいま届いている分を`cap`通（製品は`RECV_PER_POLL`）まで読んでステートマシンへ渡し、返す内容を`acts`に追加する。読んだ数を返す。
fn read_listener(
    host: &mut Host,
    (at, l): (usize, &Listener),
    buf: &mut [u8],
    (now, cap): (u64, usize),
    acts: &mut Vec<HostAct>,
) -> usize {
    let mut n_read = 0;
    while n_read < cap {
        let Ok((n, from)) = l.sock.recv_from(buf) else {
            break;
        };
        n_read += 1;
        if let Some(b) = buf.get(..n) {
            acts.extend(host_receive(host, (at, l.net), from, b, now));
        }
    }
    n_read
}

/// ホストの務め。自分のLANのアドレスの受付用受信ソケットを開いて参加者を待つ。
/// 先着1人を受けたらゲームのネットワークを開き、その人とコードを受け渡す。
/// 対戦が成立した後は観戦の申し込みに観戦コードを答える。
/// 参加者が返すコードを返さないまま上限が過ぎたら開いたネットワークのまま待機へ戻る。
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
    let mut socks: Vec<Listener> = Vec::new();
    let mut tools = [None; LAN_IPS_MAX];
    // 画面に出せる数（`LAN_IPS_MAX`）までしか受付用受信ソケットを開かない。画面に無いアドレスで待っても相手はそのアドレスを知らない。
    for h in ips {
        if socks.len() >= LAN_IPS_MAX {
            break;
        }
        if let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(h.ip, env.port))
            && sock.set_nonblocking(true).is_ok()
        {
            if let Some(t) = tools.get_mut(socks.len()) {
                *t = h.tool;
            }
            socks.push(Listener {
                ip: h.ip,
                sock,
                net: h.net,
            });
        }
    }
    if socks.is_empty() {
        say.note.lan_port = env.port;
        say.end(Event::LanPortBusy);
        return End::Failed(Event::LanPortBusy);
    }
    // 画面に出すのは受付用受信ソケットを開けたアドレスの全部（VPNのツールのアドレスにはツールの名前を添える）。
    say.note.lan_port = env.port;
    say.note.lan_tools = tools;
    for (i, l) in socks.iter().take(LAN_IPS_MAX).enumerate() {
        say.note.lan_ips[i] = l.ip.octets();
        say.note.lan_n = (i + 1) as u8;
    }
    say.note.since = (env.unix)() as u32;
    say.put(CodeState::LanWaiting, Event::None);

    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut host = Host::new(env.ver, env.watch_ok).with_reply_wait(env.reply_wait_ms);
    let mut bridge: Option<Arc<LanBridge>> = None;
    // ゲームのネットワークを開いたアドレス。待機へ戻った後の参加者はこのアドレスに届いた人だけ受けられる（ネットワークは開き直さない）。
    let mut net_ip: Option<Ipv4Addr> = None;
    let mut buf = [0u8; MAX_DATAGRAM + 64];
    loop {
        if env.stopped() {
            say.end(Event::LanCancelled);
            return End::Stopped;
        }
        // 取りやめの指示を自分で読むのはネットワークを開く前だけ。開いた後は監視の処理が読み、取りやめは`LanBridge::abort`で届く。
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
                    send_host_act(&socks, a);
                }
            }
            while let Some((src, id, via, answer)) = b.take_watch_answer() {
                let acts = match answer {
                    Ok(code) => host.watch_code_ready(src, id, via, &code),
                    Err(why) => host.watch_refused(src, id, via, why),
                };
                for a in acts {
                    send_host_act(&socks, a);
                }
            }
        }
        let mut acts = Vec::new();
        for (i, l) in socks.iter().enumerate() {
            if env.stopped() {
                say.end(Event::LanCancelled);
                return End::Stopped;
            }
            read_listener(
                &mut host,
                (i, l),
                &mut buf,
                (now(), RECV_PER_POLL),
                &mut acts,
            );
        }
        acts.extend(host.tick(now()));
        for a in acts {
            match a {
                HostAct::Send { .. } => send_host_act(&socks, a),
                HostAct::Accepted { via, .. } => {
                    // 番号はステートマシンが申し込みを受けたときに覚えた受付用受信ソケットの番号。受付用受信ソケットは開いた後に減らないのでいつも見つかる。
                    let Some(on) = socks.get(via) else {
                        continue;
                    };
                    let ip = on.ip;
                    let scope = Scope::lan_via(ip, on.net);
                    match (&bridge, net_ip) {
                        // 待機へ戻った後は開いたままのネットワークで監視の処理に新しいコードを作らせる（前のコードは前の参加者に渡した）。
                        (Some(b), Some(on)) if on == ip => {
                            say.note.name = [0; NAME_UNITS];
                            say.put(CodeState::LanLinking, Event::LanPeer);
                            b.post_cmd(BridgeCmd::Create);
                        }
                        // ネットワークは別のアドレスで開いている。その相手に届くとは限らないので断る。
                        (Some(_), _) => {
                            for a in host.refuse_taken() {
                                send_host_act(&socks, a);
                            }
                        }
                        (None, _) => {
                            say.note.name = [0; NAME_UNITS];
                            say.put(CodeState::LanLinking, Event::LanPeer);
                            let b = LanBridge::new();
                            note_vpn_open(env, ip, scope);
                            if (env.open_net)(ip, scope, LanRole::Host, Arc::clone(&b)).is_err() {
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
                HostAct::WatchWanted { src, id, via } => {
                    if let Some(b) = &bridge {
                        b.post_cmd(BridgeCmd::WatchCode { src, id, via });
                    }
                }
                // 参加者が返すコードを返さなかった（取りやめた・打ち間違えた）。ネットワークは開いたまま待機の画面へ戻す。
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

/// 受付用受信ソケットが読んでよい送信元かどうか。
/// VPNのネットワークの受付用受信ソケット（`vpn`）はそのネットワークの中の1台だけを読む。
/// そうでない受付用受信ソケットは範囲lanの送信元なら誰でも読む（同じLANの誰でも先着1人。ルーターでつないだ別のサブネットの私設のアドレスも受ける）。
/// どちらもWANから届いたもの・ポート0・どの範囲でも使えないアドレス（マルチキャストなど）は読まない。
fn source_ok(src: SocketAddrV4, vpn: Option<Net4>) -> bool {
    let addr = SocketAddr::V4(src);
    match vpn {
        Some(net) => {
            src.port() != 0 && guard::reach(addr.ip()).is_some() && net.contains_host(*src.ip())
        }
        None => guard::allows(Scope::Lan, addr),
    }
}

/// 送る。使う受付用受信ソケットは宛先の申し込みを受けた受付用受信ソケット（ステートマシンが受けたときに覚えた番号）。その相手に届く自分のアドレスから出す。
fn send_host_act(socks: &[Listener], a: HostAct) {
    if let HostAct::Send { to, via, bytes } = a
        && let Some(l) = socks.get(via)
    {
        let _ = l.sock.send_to(&bytes, to);
    }
}

// ---- 申し込む側の下ごしらえ ----

/// 利用者が打ったアドレスに申し込んでよいかどうか。範囲lanの中ならよい（`Ok(None)`）。
/// 外なら自分のPCのVPNのネットワークの中の1台のときだけよい（`Ok(Some(その VPN のアドレス))`）。それ以外は断る。
fn admit_host(addr: LanAddr, links: &[VpnLink]) -> Result<Option<VpnLink>, String> {
    if guard::allows(Scope::Lan, SocketAddr::V4(addr.socket())) {
        return Ok(None);
    }
    links
        .iter()
        .find(|l| l.net.contains_host(addr.ip))
        .map(|l| Some(*l))
        .ok_or_else(|| "LAN の外のアドレス".to_string())
}

/// 申し込むソケットが選んだ自分のアドレス`local`から、その接続でゲームのネットワークを開く範囲を決める。
/// VPNのアドレスへの申し込み（`link`）は`local`がそのVPNのアダプターのアドレスのときだけ通す。
/// そうでなければ経路がVPNを通らず、世界のアドレスとして外へ出てしまう。
fn scope_via(link: Option<VpnLink>, local: Ipv4Addr) -> Result<Scope, String> {
    match link {
        Some(l) if l.ip != local => Err("VPN を経由しない経路".to_string()),
        _ => Ok(Scope::lan_via(local, link.map(|l| l.net))),
    }
}

/// ホストのアドレス宛てのUDPソケット・そのホストに届く自分のアドレス（OSの経路で決まる）・その接続でゲームのネットワークを開く範囲。
fn dial(addr: LanAddr, links: &[VpnLink]) -> Result<(UdpSocket, Ipv4Addr, Scope), String> {
    let to = SocketAddr::V4(addr.socket());
    let link = admit_host(addr, links)?;
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
    let scope = scope_via(link, local)?;
    Ok((s, local, scope))
}

// ---- クライアント ----

/// クライアントの務め。ホストのアドレスへ参加を申し込む。
/// 始める側のコードを受けたらゲームのネットワークを開いて監視の処理に読ませる。
/// 監視の処理が作った返すコードをホストへ返し、受け取りの確認を待つ。
pub fn run_client(env: &LanEnv, addr: LanAddr) -> End {
    let mut say = Say::new(&env.link);
    let (sock, local, scope) = match dial(addr, &(env.vpn_links)()) {
        Ok(v) => v,
        Err(_) => {
            say.end(Event::LanNoHost);
            return End::Failed(Event::LanNoHost);
        }
    };
    say.note.name = [0; NAME_UNITS];
    say.put(CodeState::LanLinking, Event::LanPeer);
    // 前のネットワークの後始末（待っているだけのセッションを`request_lan`が手放した）が済むまで申し込まない。
    // 済む前に始める側のコードが届くとゲームのネットワークを開けない。
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
                    note_vpn_open(env, local, scope);
                    if (env.open_net)(local, scope, LanRole::Client, Arc::clone(&b)).is_err()
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

/// 観戦の務め。ホストのアドレスへ観戦コードを頼み、受けたら観戦者のネットワークを開く頼みを出す。
/// 前のネットワークの後始末は待たない（観戦者のネットワークを開く頼みは後始末の途中なら終えてから開く）。
pub fn run_watch(env: &LanEnv, addr: LanAddr) -> End {
    let mut say = Say::new(&env.link);
    let (sock, local, scope) = match dial(addr, &(env.vpn_links)()) {
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
                    note_vpn_open(env, local, scope);
                    return match (env.start_watch)(code, local, scope) {
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

/// 同時に動かす運び手は1本まで（動いている間に次の頼みが来たら断る）。
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 運び手のスレッドを作る。動いている運び手があれば作らず`false`（呼び出し元は読み捨てる）。終了時に`done`を呼ぶ。
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
            Err(format!("LAN の運び手のスレッドの作成不能: {e}"))
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

    /// 偽の監視の処理。実物の監視の処理が`LanBridge`に対してすること（動き出す・指示を取る・コードを書く・読む）だけを真似る。
    fn fake_watcher(bridge: Arc<LanBridge>, role: LanRole, log: Arc<Mutex<Vec<String>>>) {
        thread::spawn(move || {
            bridge.attach();
            let t = Instant::now();
            let mut linked = false;
            let mut made = 0;
            while t.elapsed() < Duration::from_secs(10) && !bridge.is_aborted() {
                match bridge.take_cmd() {
                    // 2回目からは別のコード（作り直したことが分かるように）。
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
                    Some(BridgeCmd::WatchCode { src, id, via }) => {
                        log.lock().unwrap().push("watchcode".into());
                        bridge.post_watch_answer(src, id, via, Ok("WATCH-CODE".into()));
                    }
                    None => {}
                }
                if role == LanRole::Client && linked {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if role == LanRole::Client {
                // クライアントの監視の処理はコードの受け渡しの後も残る。運び手が先に終わるのを待ってから閉じる。
                thread::sleep(Duration::from_millis(300));
                bridge.close();
            }
        });
    }

    struct Rig {
        env: LanEnv,
        opened: Arc<Mutex<Vec<(Ipv4Addr, LanRole)>>>,
        /// `open_net`・`start_watch`が受けた範囲（呼ばれた順）。
        scopes: Arc<Mutex<Vec<Scope>>>,
        watch: Arc<Mutex<Vec<(String, Ipv4Addr)>>>,
        log: Arc<Mutex<Vec<String>>>,
        /// `note`が受けた本ログの行（呼ばれた順）。
        notes: Arc<Mutex<Vec<String>>>,
    }

    /// VPNでない受付用受信ソケットのアドレス。
    fn plain(ip: Ipv4Addr) -> HostAddr {
        HostAddr {
            ip,
            tool: None,
            net: None,
        }
    }

    fn rig(port: u16, id: u32, watch_ok: bool) -> Rig {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let scopes = Arc::new(Mutex::new(Vec::new()));
        let watch = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let (o, l, sc) = (Arc::clone(&opened), Arc::clone(&log), Arc::clone(&scopes));
        let (w, sw) = (Arc::clone(&watch), Arc::clone(&scopes));
        let n = Arc::clone(&notes);
        let env = LanEnv {
            link: Arc::new(UiLink::new()),
            ver: VER,
            watch_ok,
            port,
            reply_wait_ms: higanr_core::lan_proto::HOST_WAIT_REPLY_MS,
            addresses: Box::new(|| Ok(vec![plain(Ipv4Addr::LOCALHOST)])),
            vpn_links: Box::new(Vec::new),
            net_free: Box::new(|| true),
            open_net: Box::new(move |ip, scope, role, b| {
                o.lock().unwrap().push((ip, role));
                sc.lock().unwrap().push(scope);
                fake_watcher(b, role, Arc::clone(&l));
                Ok(())
            }),
            start_watch: Box::new(move |code, ip, scope| {
                w.lock().unwrap().push((code, ip));
                sw.lock().unwrap().push(scope);
                Ok(())
            }),
            note: Box::new(move |line| n.lock().unwrap().push(line)),
            id,
            unix: || 1_000_000,
            stop: Arc::new(AtomicBool::new(false)),
        };
        Rig {
            env,
            opened,
            scopes,
            watch,
            log,
            notes,
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
        /// `limit`のうちに戻らなければ停止フラグを立ててパニックする。
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

    /// 運び手が終わるのを待つ上限（申し込みの待ちの上限10秒より長く）。
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

    #[test]
    fn host_client_and_watcher_exchange_the_codes_over_loopback() {
        let port = port_free();
        let host = rig(port, 0, true);
        let host_link = Arc::clone(&host.env.link);
        let host_opened = Arc::clone(&host.opened);
        let host_log = Arc::clone(&host.log);
        let h = host_bg(host);
        // 待機のタグはLANの待機・自分のアドレスとポート。
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
        // クライアントの監視の処理は始める側のコードを読む。ホストの監視の処理は返すコードを読む。
        assert_eq!(
            *c_log.lock().unwrap(),
            vec!["paste:OFFER-CODE-FROM-HOST".to_string()]
        );
        assert!(wait_until(Duration::from_secs(5), || host_log
            .lock()
            .unwrap()
            .contains(&"paste:REPLY-CODE-FROM-CLIENT".to_string())));
        assert_eq!(host_log.lock().unwrap()[0], "create");
        // 接続の途中のタグ（クライアントの画面）。監視の処理が引き継ぐ前の最後のタグ。
        let cn = last_note(&cl_link);
        assert_eq!((cn.state, cn.event), (CodeState::LanLinking, Ev::LanPeer));
        // 観戦: 成立の後なら観戦コードが返り観戦者のネットワークを開く頼みが出る。
        let w = rig(0, 77, true);
        let w_watch = Arc::clone(&w.watch);
        let end = {
            let wa = LanAddr {
                ip: Ipv4Addr::LOCALHOST,
                port,
            };
            // 成立の通知が運び手に届くのを待つ（偽の監視の処理がset_connectedした後に運び手がポーリングする）。
            thread::sleep(Duration::from_millis(300));
            run_watch(&w.env, wa)
        };
        assert_eq!(end, End::Done);
        assert_eq!(
            *w_watch.lock().unwrap(),
            vec![("WATCH-CODE".to_string(), Ipv4Addr::LOCALHOST)]
        );
        // ホストの務めを外から止める（偽の監視の処理は閉じないので）。
        assert_eq!(h.stop(), End::Stopped);
    }

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
            let from: SocketAddr = a.parse().unwrap();
            assert!(
                host_receive(&mut host, (1, None), from, &join, 0).is_empty(),
                "{a}"
            );
            assert!(!host.is_taken(), "{a} で先着が埋まった");
        }
        for a in [
            "192.168.0.5:5000",
            "10.1.2.3:1",
            "172.16.9.9:65535",
            "169.254.7.7:40000",
            "127.0.0.1:9",
        ] {
            let mut host = Host::new(VER, true);
            let src: SocketAddrV4 = a.parse().unwrap();
            assert_eq!(
                host_receive(&mut host, (1, None), SocketAddr::V4(src), &join, 0),
                vec![HostAct::Accepted { src, via: 1 }],
                "{a}"
            );
        }
    }

    /// 普通の受付用受信ソケットはルーターでつないだ別のサブネットの私設の送信元からの申し込みも受ける（同じLANの誰でも先着1人）。
    /// 申し込みを受けた受付用受信ソケットの番号を覚える（返事はその受付用受信ソケットから出す）。WANの送信元は読まない。
    #[test]
    fn a_plain_listener_takes_private_senders_on_other_subnets_and_answers_from_the_listener_that_read_them()
     {
        let join = join_msg(1);
        // 受付用受信ソケットのアドレスが192.168.0.xでも送信元のサブネットでは断らない。
        for a in [
            "192.168.0.5:5000",
            "192.168.1.5:5000",
            "192.168.88.200:65535",
            "10.1.2.3:5000",
            "172.16.9.9:5000",
            "169.254.200.9:5000",
        ] {
            let mut host = Host::new(VER, true);
            let src: SocketAddrV4 = a.parse().unwrap();
            assert_eq!(
                host_receive(&mut host, (3, None), SocketAddr::V4(src), &join, 0),
                vec![HostAct::Accepted { src, via: 3 }],
                "{a}"
            );
            assert!(host.is_taken(), "{a}");
        }
        for a in ["8.8.8.8:5000", "203.0.113.9:5000", "192.168.1.5:0"] {
            let mut host = Host::new(VER, true);
            let from: SocketAddr = a.parse().unwrap();
            assert!(
                host_receive(&mut host, (3, None), from, &join, 0).is_empty(),
                "{a}"
            );
            assert!(!host.is_taken(), "{a}");
        }
    }

    /// 1回の巡回で1つの受付用受信ソケットから読む数は上限で止まる。読み残しは次の巡回で読む。
    #[test]
    fn one_round_reads_at_most_the_cap_from_a_listener() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let to = sock.local_addr().unwrap();
        let l = Listener {
            ip: Ipv4Addr::LOCALHOST,
            sock,
            net: None,
        };
        // 上限は小さな数で見る（OSの受信のバッファの大きさに左右されない数）。
        let (cap, extra) = (5usize, 3usize);
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0..cap + extra {
            sender.send_to(&join_msg(i as u32), to).unwrap();
        }
        let mut host = Host::new(VER, true);
        let mut buf = [0u8; MAX_DATAGRAM + 64];
        let mut acts = Vec::new();
        let mut first = 0;
        // 送った分が受付用受信ソケットに入るのを待つ。
        for _ in 0..100 {
            first += read_listener(
                &mut host,
                (0, &l),
                &mut buf,
                (0, cap.saturating_sub(first)),
                &mut acts,
            );
            if first == cap {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(first, cap);
        assert_eq!(
            acts,
            vec![HostAct::Accepted {
                src: v4(sender.local_addr().unwrap()).unwrap(),
                via: 0
            }]
        );
        let mut rest = 0;
        for _ in 0..100 {
            rest += read_listener(&mut host, (0, &l), &mut buf, (1, cap), &mut acts);
            if rest >= extra {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rest, extra, "読み残しを次の巡回で読まない");
        assert_eq!(
            read_listener(&mut host, (0, &l), &mut buf, (2, 0), &mut acts),
            0,
            "上限 0 で読んだ"
        );
    }

    /// 127.0.0.2は127.0.0.1と別のアドレス（有線と無線の両方があるPCの見立て）。
    #[test]
    fn the_network_opens_on_the_address_the_request_arrived_at() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let second = Ipv4Addr::new(127, 0, 0, 2);
        host.env.addresses = Box::new(move || Ok(vec![plain(Ipv4Addr::LOCALHOST), plain(second)]));
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
        // 2つ目のアドレスへ申し込む。返事がそのアドレスから出なければ接続したソケットは受け取らず、「返事がない」で終わる。
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

    #[test]
    fn the_addresses_on_screen_are_exactly_the_ones_listening() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let many: Vec<Ipv4Addr> = (1..=LAN_IPS_MAX as u8 + 2)
            .map(|i| Ipv4Addr::new(127, 0, 0, i))
            .collect();
        let shown: Vec<Ipv4Addr> = many[..LAN_IPS_MAX].to_vec();
        let list: Vec<HostAddr> = many.iter().copied().map(plain).collect();
        host.env.addresses = Box::new(move || Ok(list.clone()));
        let link = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link));
        let n = last_note(&link);
        assert_eq!(usize::from(n.lan_n), LAN_IPS_MAX);
        let on_screen: Vec<Ipv4Addr> = n.lan_ips[..LAN_IPS_MAX]
            .iter()
            .map(|o| Ipv4Addr::from(*o))
            .collect();
        assert_eq!(on_screen, shown);
        for ip in &many {
            let free = UdpSocket::bind(SocketAddrV4::new(*ip, port)).is_ok();
            assert_eq!(free, !shown.contains(ip), "{ip}");
        }
        // 画面の最後のアドレスへ申し込める。
        let last = LanAddr {
            ip: shown[LAN_IPS_MAX - 1],
            port,
        };
        assert_eq!(client_bg(rig(0, 51, true), last).wait(DONE), End::Done);
        assert_eq!(h.stop(), End::Stopped);
    }

    #[test]
    fn each_answer_leaves_from_the_address_its_request_arrived_at() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let second = Ipv4Addr::new(127, 0, 0, 2);
        host.env.addresses = Box::new(move || Ok(vec![plain(Ipv4Addr::LOCALHOST), plain(second)]));
        let link = Arc::clone(&host.env.link);
        let h = host_bg(host);
        assert!(waiting(&link));
        // 先着は127.0.0.1へ。
        let first = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        assert_eq!(client_bg(rig(0, 41, true), first).wait(DONE), End::Done);
        let at_second = LanAddr { ip: second, port };
        // 観戦は127.0.0.2へ（成立の通知が運び手に届くまでの短い間は「まだ」と断られる）。
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
        // 別のビルドの参加の申し込みも127.0.0.2へ。断りの理由が届く。
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

    /// ネットワークと別のアドレスに届いた参加者を受けないことも見る。
    #[test]
    fn a_silent_joiner_sends_the_host_back_to_waiting_and_the_next_one_gets_a_new_code() {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let second = Ipv4Addr::new(127, 0, 0, 2);
        host.env.addresses = Box::new(move || Ok(vec![plain(Ipv4Addr::LOCALHOST), plain(second)]));
        host.env.reply_wait_ms = 400;
        let link = Arc::clone(&host.env.link);
        let opened = Arc::clone(&host.opened);
        let h = host_bg(host);
        assert!(waiting(&link));
        // 黙る参加者。JOINを1通送り、始める側のコードを受けたら何も返さない。
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
        // ネットワークと別のアドレス（127.0.0.2）に届いた参加者は断る（返事は127.0.0.2から出る）。
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
        // 次の参加者は127.0.0.1へ申し込む。開いたままのネットワークで作り直したコードを受け取って接続する。
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

    #[test]
    fn the_host_carrier_ends_when_the_watcher_gives_up_or_closes() {
        for how in ["abort", "close"] {
            let port = port_free();
            let mut host = rig(port, 0, true);
            let keep: Arc<Mutex<Option<Arc<LanBridge>>>> = Arc::new(Mutex::new(None));
            let k = Arc::clone(&keep);
            host.env.open_net = Box::new(move |_, _, _, b| {
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
            // 返すコードは来ないのでクライアントの側は待ち続ける。ホストの運び手だけを見る。
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
        assert_eq!(run_watch(&w.env, addr), End::Failed(Ev::WatchRefused));
        assert_eq!(h.stop(), End::Stopped);
    }

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

    /// `dial`のガードを外すと最初のチェックで落ちる。何も送らない（UDPの`connect`はパケットを出さない）。
    #[test]
    fn an_outside_address_no_address_and_a_busy_port_end_with_their_own_events() {
        // 文書用のアドレス（TEST-NET-3）。LANの外。
        let outside = LanAddr {
            ip: Ipv4Addr::new(203, 0, 113, 5),
            port: 17723,
        };
        assert_eq!(
            dial(outside, &[]).err().as_deref(),
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
        // 自分のLANのアドレスが無い。
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
        // 申し込みの送り直しの間隔（500 ms）より長く待っても何も届かない。
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
        // ポートは閉じているのでもう一度待てる。
        assert!(UdpSocket::bind(("127.0.0.1", port)).is_ok());
    }

    fn net(a: &str, prefix: u8) -> Net4 {
        Net4::new(a.parse().unwrap(), prefix).unwrap()
    }

    fn join_msg(id: u32) -> Vec<u8> {
        encode(&Msg::Join {
            id,
            proto: VER.proto,
            build: VER.build,
        })
        .unwrap()
    }

    #[test]
    fn a_vpn_listener_reads_only_its_own_network_and_a_plain_one_never_reads_a_vpn_source() {
        let radmin = Some(net("26.0.0.1", 8));
        let join = join_msg(1);
        // (送信元, VPNの受付用受信ソケット(26/8)が読むか, 普通の受付用受信ソケットが読むか)
        for (a, vpn_ok, plain_ok) in [
            ("26.5.6.7:5000", true, false),
            ("26.0.0.1:1", true, false),
            ("26.255.255.254:65535", true, false),
            ("26.0.0.0:5000", false, false),
            ("26.255.255.255:5000", false, false),
            ("25.255.255.254:5000", false, false),
            ("27.0.0.1:5000", false, false),
            ("26.5.6.7:0", false, false),
            ("192.168.0.5:5000", false, true),
            ("10.1.2.3:5000", false, true),
            ("169.254.7.7:5000", false, true),
            ("127.0.0.1:9", false, true),
            ("8.8.8.8:5000", false, false),
            ("224.0.0.1:5000", false, false),
            ("[fe80::1]:5000", false, false),
            ("[::ffff:26.5.6.7]:5000", false, false),
        ] {
            let from: SocketAddr = a.parse().unwrap();
            for (vpn, want) in [(radmin, vpn_ok), (None, plain_ok)] {
                let mut host = Host::new(VER, true);
                let acts = host_receive(&mut host, (2, vpn), from, &join, 0);
                assert_eq!(!acts.is_empty(), want, "{a} / 受付用受信ソケット {vpn:?}");
                assert_eq!(host.is_taken(), want, "{a} / 受付用受信ソケット {vpn:?}");
                if want {
                    assert!(
                        matches!(acts[..], [HostAct::Accepted { via: 2, .. }]),
                        "{a} / 受付用受信ソケット {vpn:?}: {acts:?}"
                    );
                }
            }
        }
    }

    /// ネットワークが予約の範囲（240/4）に掛かっていても同じ。
    #[test]
    fn a_vpn_listener_never_reads_a_source_that_no_scope_can_use() {
        let join = join_msg(2);
        let odd = Some(net("240.0.0.1", 8));
        let mut host = Host::new(VER, true);
        let from: SocketAddr = "240.0.0.5:5000".parse().unwrap();
        assert!(host_receive(&mut host, (0, odd), from, &join, 0).is_empty());
        assert!(!host.is_taken());
        let ok: SocketAddr = "26.0.0.5:5000".parse().unwrap();
        let acts = host_receive(&mut host, (0, Some(net("26.0.0.1", 8))), ok, &join, 0);
        assert!(!acts.is_empty(), "同じ表で、使える送り元は読む");
    }

    #[test]
    fn the_client_dials_a_non_lan_address_only_inside_one_of_my_vpn_networks_and_over_that_adapter()
    {
        let link = |ip: &str, p: u8| VpnLink {
            ip: ip.parse().unwrap(),
            tool: higanr_core::vpn_net::VpnTool::Radmin,
            net: net(ip, p),
        };
        let radmin = link("26.12.34.56", 8);
        let zt = VpnLink {
            tool: higanr_core::vpn_net::VpnTool::ZeroTier,
            ..link("28.7.7.9", 24)
        };
        let at = |s: &str| LanAddr {
            ip: s.parse().unwrap(),
            port: 17723,
        };
        let all = [radmin, zt];
        assert_eq!(admit_host(at("26.200.0.1"), &all), Ok(Some(radmin)));
        assert_eq!(admit_host(at("28.7.7.1"), &all), Ok(Some(zt)));
        for out in [
            "26.0.0.0",
            "26.255.255.255",
            "25.0.0.1",
            "27.0.0.1",
            "28.7.6.1",
            "28.7.8.1",
            "8.8.8.8",
            "100.64.0.1",
            "224.0.0.1",
        ] {
            assert_eq!(
                admit_host(at(out), &all).unwrap_err(),
                "LAN の外のアドレス",
                "{out}"
            );
        }
        // VPNのネットワークが無ければ今までどおりLANの外。
        assert!(admit_host(at("26.200.0.1"), &[]).is_err());
        // 広いネットワーク（172/8など）が私設のアドレスを含んでいても私設の宛先はVPNの扱いにしない（LANのアダプターを通るのでVPNの経路のチェックで断らない）。
        let wide = link("172.99.1.1", 8);
        assert_eq!(admit_host(at("172.16.0.5"), &[wide]), Ok(None));
        assert_eq!(admit_host(at("172.200.0.5"), &[wide]), Ok(Some(wide)));
        // LANの中はVPNの指定があってもVPNの扱いにしない。
        for lan in [
            "192.168.0.2",
            "10.0.0.5",
            "172.16.0.1",
            "169.254.1.1",
            "127.0.0.1",
        ] {
            assert_eq!(admit_host(at(lan), &all), Ok(None), "{lan}");
            assert_eq!(admit_host(at(lan), &[]), Ok(None), "{lan}");
        }
        // 経路のチェックを通るのは申し込むソケットの自分のアドレスがVPNのアダプターのアドレスのときだけ。そのときはそのVPNのネットワークを足した範囲になる。
        assert_eq!(
            scope_via(Some(radmin), radmin.ip),
            Ok(Scope::LanVpn(radmin.net))
        );
        assert_eq!(scope_via(Some(zt), zt.ip), Ok(Scope::LanVpn(zt.net)));
        assert!(scope_via(Some(radmin), "192.168.0.7".parse().unwrap()).is_err());
        assert!(scope_via(Some(radmin), zt.ip).is_err());
        assert_eq!(
            scope_via(None, "192.168.0.7".parse().unwrap()),
            Ok(Scope::Lan)
        );
    }

    /// 待機のタグのツールの名前が別のアドレスに付かないことも見る。
    #[test]
    fn a_vpn_listener_answers_inside_its_network_only_and_opens_the_game_network_in_the_widened_scope()
     {
        let port = port_free();
        let mut host = rig(port, 0, true);
        let vpn_net = net("127.0.0.5", 30);
        let listener = Ipv4Addr::new(127, 0, 0, 5);
        host.env.addresses = Box::new(move || {
            Ok(vec![
                plain(Ipv4Addr::new(127, 0, 0, 2)),
                HostAddr {
                    ip: listener,
                    tool: Some(higanr_core::vpn_net::VpnTool::Radmin),
                    net: Some(vpn_net),
                },
            ])
        });
        host.env.vpn_links = Box::new(move || {
            vec![VpnLink {
                ip: listener,
                tool: VpnTool::Radmin,
                net: vpn_net,
            }]
        });
        let link = Arc::clone(&host.env.link);
        let (opened, scopes) = (Arc::clone(&host.opened), Arc::clone(&host.scopes));
        let notes = Arc::clone(&host.notes);
        let h = host_bg(host);
        assert!(waiting(&link));
        let n = last_note(&link);
        assert_eq!(
            (n.lan_n, n.lan_ips[1], n.lan_tools[0], n.lan_tools[1]),
            (
                2,
                [127, 0, 0, 5],
                None,
                Some(higanr_core::vpn_net::VpnTool::Radmin)
            ),
            "ツールの名前は VPN のアドレスの行だけ"
        );
        let ask = |from: &str, wait_ms: u64| {
            let s = UdpSocket::bind(format!("{from}:0")).unwrap();
            s.set_read_timeout(Some(Duration::from_millis(wait_ms)))
                .unwrap();
            s.send_to(&join_msg(9), (listener, port)).unwrap();
            let mut buf = [0u8; MAX_DATAGRAM];
            s.recv_from(&mut buf).map(|(n, _)| buf[..n].to_vec())
        };
        // 127.0.0.1は127.0.0.4/30の外なので何も返らずネットワークも開かない。
        assert!(
            ask("127.0.0.1", 700).is_err(),
            "ネットワークの外の申し込みに答えた"
        );
        assert!(opened.lock().unwrap().is_empty());
        // 127.0.0.6は中なので始める側のコードが返り、VPNのネットワークを足した範囲で開く。
        let reply = ask("127.0.0.6", 5000).expect("ネットワークの中の申し込みに答えない");
        assert!(matches!(decode(&reply), Ok(Msg::Offer { id: 9, .. })));
        assert_eq!(*opened.lock().unwrap(), vec![(listener, LanRole::Host)]);
        assert_eq!(*scopes.lock().unwrap(), vec![Scope::LanVpn(vpn_net)]);
        assert_eq!(
            *notes.lock().unwrap(),
            vec!["LAN: Radmin VPN のネットワーク（/30）を足した範囲で開きます。".to_string()],
            "VPN の範囲で開いた行（ツールの名前と長さだけ）"
        );
        assert_eq!(h.stop(), End::Stopped);
    }

    /// ツールが取得できないときは名前だけを省く。
    #[test]
    fn the_vpn_line_names_the_tool_and_the_prefix_only() {
        assert_eq!(
            vpn_open_line(Some(VpnTool::Radmin), 8),
            "LAN: Radmin VPN のネットワーク（/8）を足した範囲で開きます。"
        );
        assert_eq!(
            vpn_open_line(None, 30),
            "LAN: VPN のネットワーク（/30）を足した範囲で開きます。"
        );
        for t in [VpnTool::Hamachi, VpnTool::ZeroTier] {
            let line = vpn_open_line(Some(t), 24);
            assert!(
                line.contains(t.name()) && line.contains("（/24）"),
                "{line}"
            );
        }
        for t in [None, Some(VpnTool::Radmin)] {
            assert!(!vpn_open_line(t, 8).contains('.'), "アドレスの形が混ざった");
        }
    }

    #[test]
    fn only_a_vpn_scope_writes_the_vpn_line() {
        let mut r = rig(0, 0, true);
        let ip = Ipv4Addr::new(127, 0, 0, 5);
        let vpn_net = net("127.0.0.5", 30);
        note_vpn_open(&r.env, ip, Scope::Lan);
        assert!(r.notes.lock().unwrap().is_empty(), "範囲 lan に行を書いた");
        note_vpn_open(&r.env, ip, Scope::LanVpn(vpn_net));
        assert_eq!(
            *r.notes.lock().unwrap(),
            vec!["LAN: VPN のネットワーク（/30）を足した範囲で開きます。".to_string()],
            "ツールが引けないときは名前だけを省く"
        );
        r.notes.lock().unwrap().clear();
        r.env.vpn_links = Box::new(move || {
            vec![VpnLink {
                ip,
                tool: VpnTool::ZeroTier,
                net: vpn_net,
            }]
        });
        note_vpn_open(&r.env, ip, Scope::LanVpn(vpn_net));
        assert_eq!(
            *r.notes.lock().unwrap(),
            vec!["LAN: ZeroTier のネットワーク（/30）を足した範囲で開きます。".to_string()]
        );
    }

    #[test]
    fn a_plain_lan_connection_keeps_the_lan_scope_for_host_client_and_watcher() {
        let port = port_free();
        let host = rig(port, 0, true);
        let (link, host_scopes) = (Arc::clone(&host.env.link), Arc::clone(&host.scopes));
        let host_notes = Arc::clone(&host.notes);
        let h = host_bg(host);
        assert!(waiting(&link));
        let addr = LanAddr {
            ip: Ipv4Addr::LOCALHOST,
            port,
        };
        let c = rig(0, 61, true);
        let c_scopes = Arc::clone(&c.scopes);
        let c_notes = Arc::clone(&c.notes);
        assert_eq!(client_bg(c, addr).wait(DONE), End::Done);
        thread::sleep(Duration::from_millis(300));
        let w = rig(0, 62, true);
        let w_scopes = Arc::clone(&w.scopes);
        let w_notes = Arc::clone(&w.notes);
        assert_eq!(run_watch(&w.env, addr), End::Done);
        assert!(
            [&host_notes, &c_notes, &w_notes]
                .iter()
                .all(|n| n.lock().unwrap().is_empty()),
            "範囲 lan の接続が VPN の行を書いた"
        );
        for (who, s) in [
            ("ホスト", host_scopes),
            ("クライアント", c_scopes),
            ("観戦", w_scopes),
        ] {
            assert_eq!(*s.lock().unwrap(), vec![Scope::Lan], "{who}");
        }
        assert_eq!(h.stop(), End::Stopped);
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
        // パニックを受けた後も次を作れる。
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
