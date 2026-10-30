//! 接続の手順（HELLO・HELLO_ACK・AGREE）のステートマシン。
//!
//! バイト列の形は`wire.rs`にある（検査は`decode`が済ませている）。
//! ここが持つのは「いつ何を返し、いつ何を送り、いつ接続したとするか」だけ。ソケットも時計も乱数も持たない。
//! 経路の選び方と自分のHELLOの跳ね返りを捨てるゲートは接続のセッション（`link.rs`）の側にある。ここが扱うのは相手が1つに決まっている形だけ。
//!
//! dllの呼び方: 接続ごとに`Handshake::new`を呼ぶ（rollbackを望むなら最初の`poll_send`の前に`set_wish`を呼ぶ）。
//! 受信したHELLO・HELLO_ACK・AGREEは`on_packet`へ渡し、返ったものは受けた相手へすぐ送る。
//! `poll_send`は1〜4 msごとと`on_packet`の直後に呼び、返ったものを送る。
//! `state()`が`Established`になったら選択（`selection.rs`）とlockstepを作る。接続後も`on_packet`・`poll_send`は続ける（相手の確認が落ちると相手は送り続けているので、返答が要る）。
//! 時間切れの後も待つなら`restart`で作り直す（いつ・何回までかは`link.rs`）。
//! AGREEを送るのは取りまとめ役（role_nonceの大きい方）。ホストの決め方（`set_host_pick`。`host_pick.rs`）を両者が使うならPROBEの列とゲストのPROBE_RESULTで決まった側がホストになる。
//!
//! 受信スレッドかゲームスレッドから呼ばれうるので、ヒープを使わず、パニックしうる添字とunwrapを置かない。名前は32 Bの固定の配列と長さで持つ。

use crate::frame::MAX_DELAY;
use crate::host_pick::{self, Facts, Pick, PickNote, Train};
use crate::nat::NatMark;
use crate::seal::PUBLIC_LEN;
use crate::seat_pick::{self, SeatAsk};
use crate::wire::{Hello, Message, Mode, NAME_MAX, PROTO_VER, Packet, Seat};

pub use crate::lockstep::Role;

/// HELLOを送り直す間隔（ms）。選択の4 msより間を空けたのは、候補アドレスが複数あり、最長10秒続く穴あけだから。
/// 例外: 自分のHELLOがまだackされていない間に相手のHELLOを受けたら間隔を待たずに次の`poll_send`で出す（`on_hello`の即送り）。
pub const HELLO_INTERVAL_MS: u32 = 20;

/// ホストがAGREEを送り直す間隔（ms）。選択の`selection::RESEND_MS`と同じ値だが、由来が別なので別の名前で持つ。
pub const AGREE_INTERVAL_MS: u32 = 4;

/// 始めてから接続できずにやめるまでの時間（ms）。1回の手順の長さ。
/// 接続のセッションに作り直せる長さが設定されていれば、時間切れの後に`restart`で作り直して待ち続ける。
pub const TIMEOUT_MS: u32 = 10_000;

// サイドは`Config::host_seat`で持つ（adonisと同じくホストが決められる）。
// 各自がHELLOでサイドの希望と先にいたかを名乗る（`set_seat_ask`。設定しなければ `SeatAsk::plain(host_seat)` = バージョン3までと同じ答え）。
// 取りまとめ役は決まったホストの側から`seat_pick::host_seat`で決めてAGREEのhost_seatで送る。
// ゲストは同じ式で検証する（`Counters::agree_bad_seat`）。

/// 名前（UTF-8で最大32 B）。ヒープを使わずにHELLOのnameのフィールドと同じ形で持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; NAME_MAX],
    len: u8,
}

impl Name {
    pub const EMPTY: Name = Name {
        bytes: [0; NAME_MAX],
        len: 0,
    };

    /// 32 Bに収まるよう文字の境目で切って詰める。
    /// バイトの途中で切るとUTF-8として読めなくなり、相手の`decode`がHELLOごと捨てる。
    pub fn new(s: &str) -> Name {
        let mut end = 0;
        for (i, c) in s.char_indices() {
            let e = i + c.len_utf8();
            if e > NAME_MAX {
                break;
            }
            end = e;
        }
        let mut bytes = [0u8; NAME_MAX];
        for (d, &b) in bytes.iter_mut().zip(s.as_bytes().iter().take(end)) {
            *d = b;
        }
        Name {
            bytes,
            len: end as u8,
        }
    }

    fn from_hello(h: &Hello) -> Name {
        Name {
            bytes: h.name,
            len: h.name_len.min(NAME_MAX as u8),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub session_id: u32,
    pub role_nonce: u64,
    /// 希望latency（0〜9）。範囲の外は9に詰める（相手の`decode`がHELLOごと捨てるのを避ける）。
    pub latency: u8,
    pub name: Name,
    /// 自分がホストになったときに座るサイド。ゲストになったら使わない。`set_seat_ask`を呼ばなければこのサイドを希望として名乗る（`SeatAsk::plain`）。
    pub host_seat: Seat,
}

/// 望むモード（HELLOの +3）とrollbackに決まったときのD。
/// 両者がrollbackを望んだときだけrollbackになる。片方でもdelay方式ならdelay方式になる（古いバージョンのHELLOは +3が0なのでdelay方式）。
/// rollbackのときのDはホストの`rollback_delay`を使う（AGREEのdelayのフィールドで送る）。
/// Dは開始直後の穴埋めの長さを決めるので両者で同じでないとずれる。
/// また遅い回線でも待ちを生まない（遅れは予測が埋める）ので「希望の大きい方」にはしない。
/// R（予測の上限）は確定の列が本物のワードだけで作られRに依らないので、ネットワークに載せない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeWish {
    pub mode: Mode,
    /// rollbackに決まったときのD（0〜9。範囲の外は9に詰める）。自分がゲストなら使わない（ホストの値に従う）。
    pub rollback_delay: u8,
}

impl ModeWish {
    pub const DELAY: ModeWish = ModeWish {
        mode: Mode::Delay,
        rollback_delay: crate::rollback::DEFAULT_DELAY,
    };

    pub fn rollback(d: u8) -> ModeWish {
        ModeWish {
            mode: Mode::Rollback,
            rollback_delay: d.min(MAX_DELAY),
        }
    }
}

impl Default for ModeWish {
    fn default() -> Self {
        ModeWish::DELAY
    }
}

/// 両者の望むモードから決まるモード（`ModeWish`の「決まり方」）。
/// ホストがAGREEを作るときとゲストが届いたAGREEを検証するときの両方が使う。
/// `able`は両者がrollbackができるかどうか（HELLOの`can_rollback`のAND）。両者が望んでも片方ができなければdelay方式になる（防御）。
fn agreed_mode(mine: Mode, peer: Mode, able: bool) -> Mode {
    match (mine, peer) {
        (Mode::Rollback, Mode::Rollback) if able => Mode::Rollback,
        _ => Mode::Delay,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Terms {
    host_seat: Seat,
    delay: u8,
    mode: Mode,
    pick: Pick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Established {
    pub role: Role,
    pub seat: Seat,
    pub host_seat: Seat,
    pub mode: Mode,
    /// delay方式: マッチを始めるときのdelay（ホストが決めた値。両者の希望の大きい方）。
    /// rollbackのモード: D（ホストの`ModeWish::rollback_delay`。マッチの間は変えない）。
    pub delay: u8,
    /// 自分がHELLOで名乗った名前。メニューで名前を変えた値と実際に名乗った値がずれても、名乗った値を試合中の画面と観戦者へ送る記録に出すために持つ。
    pub own_name: Name,
    pub peer_name: Name,
    pub peer_latency: u8,
    /// 相手の望むモード（HELLOの +3。古いバージョンはdelay方式）。「なぜ delay 方式になったか」を画面とログに出すため。
    pub peer_mode: Mode,
    /// 両者がrollbackができる（HELLOの +3のbit1のAND）。
    /// 真なら接続のセッションはrollbackのステートマシンを持つ。
    /// またキャラセレクトのTabで形式を替えられる（`link::Connected::switch_format`）。
    pub rollback_able: bool,
    /// 2人とも観戦を許可すると名乗った（HELLOの +3のbit2。決まりは`watch_list::may_list`）。
    /// 偽ならホストはどの入口でも観戦の受付を作らない（ランダムマッチの一覧にも載せない）。
    pub may_list: bool,
    /// どちらがホストに決まったかの材料（本ログに出す）。
    pub pick: PickNote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// 相手のHELLOのrole_nonceが自分と同じ（ほぼ自分のHELLOが自分に返った）。
    SameNonce,
    /// 10秒のあいだ同じsession_idのものが1通も届かない（相手に届かない・直結できない組み合わせの可能性）。
    NothingReceived,
    /// 同じsession_idのものは届いたが、10秒で接続できない（片方向だけ通るなど）。
    NoHandshake,
    /// 相手が別のプロトコルのバージョンで送ってきた（`theirs`は相手のバージョン）。
    /// 接続のセッションがタグの合ったパケットの`wire::DecodeError::BadVersion`を見て`Handshake::refuse_version`で設定する。
    /// 何度作り直しても同じ答えなので作り直さない。
    OtherVersion { theirs: u8 },
    /// 相手のHELLOのビルド番号（`theirs`）が自分（`update::BUILD_NUMBER`。`Handshake::set_build`）と違う（0と0は同じ）。
    /// プロトコルのバージョンが同じでも配布したzipが違う相手とは接続前に断る。作り直さない。
    OtherBuild { theirs: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connecting,
    Established(Established),
    Failed(FailReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// 最初から作り直す。相手のHELLOをまだ受けていないか、この手順の間に相手の値が食い違った場合に使う。
    /// 食い違いとは違うnonceのHELLOや値の違うAGREEのことで、相手がセッションを作り直した印になる。
    Fresh,
    /// 相手のHELLOで決めた役割と相手の値は引き継ぐ。時間切れの数え始めは今に更新し、自分のHELLOをackまで送り直す。
    /// 引き継ぐのは、相手がこちらのHELLO_ACKを受けた後はHELLOを送らず、値を捨てると相手が次に時間切れになるまで役割を決められないため。
    /// 送り直すのは、相手が同じnonceのままセッションを作り直していれば、相手はこちらの値を持っていないため。
    Resume,
}

/// `poll_send`が返す、いま送るもの。HELLOとAGREEは同時に送り直し中になりうるのでフィールドを分けた。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outbox {
    pub hello: Option<Message>,
    pub agree: Option<Message>,
    /// ゲストが測った取りまとめ役の上り（取りまとめ役が相手へ送る方向の回線の速さ。PROBE_RESULT）。
    pub report: Option<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub wrong_session: u32,
    /// 役割を決めた後に届いた、覚えた値と違うrole_nonceのHELLO / HELLO_ACK（相手が接続をやり直したなど）。
    pub stale_nonce: u32,
    pub wrong_echo: u32,
    /// 役割を決める前のAGREE（ホストが送り直すので捨ててよい）。
    pub agree_early: u32,
    pub agree_mismatch: u32,
    /// ゲストが受けた、両者の望むモードから決まるモードと違うAGREE。例: 自分はdelay方式を望んだのにrollbackと決めてきた。
    /// 返さずに捨てる（接続するとワードの使い方が食い違う）。相手がセッションを作り直した印ではないので`Counters::conflicts`には数えない。
    pub agree_bad_mode: u32,
    /// ゲストが受けた、サイドの希望から決まるサイド（`seat_pick::host_seat`）と違うhost_seatのAGREE。扱いは`agree_bad_mode`と同じ。
    pub agree_bad_seat: u32,
    /// ゲストが受けた、どちらがホストかの値が合わないAGREE（載っている値から決まるホストと違う・知っているネットワーク種別のフラグや自分が送った値と違う）。
    /// 扱いは`agree_bad_mode`と同じ。
    pub agree_bad_pick: u32,
    /// 列に使った相手のPROBE。
    pub probes: u32,
    pub unrelated: u32,
    pub after_fail: u32,
    /// 接続前に同じnonceで公開鍵だけ違うHELLOを受けて相手の公開鍵を取り替えた回数（相手がセッションを作り直した）。
    /// 取り替えた後の手順は新しい相手の値で続くので食い違い（`conflicts`）には数えない。
    pub rekeyed: u32,
    /// 手放した古い公開鍵のHELLO（遅れて届いた古いセッションのもの）を、取り替え直さずに捨てた回数。
    pub retired_hello: u32,
    /// 接続前に自分の今の公開鍵を返したHELLO_ACKを根拠に、役割を決めた相手を取り替えた回数。
    /// 先に届いた古いセッションのHELLOの相手は手放した。
    pub swapped: u32,
}

impl Counters {
    fn conflicts(&self) -> u64 {
        u64::from(self.stale_nonce) + u64::from(self.agree_mismatch)
    }
}

/// 前に送った時刻から`interval`経ったかどうか。まだ1度も送っていなければすぐ送る（遅延を足さないため）。
fn due(last: Option<u32>, now: u32, interval: u32) -> bool {
    match last {
        None => true,
        Some(t) => now.wrapping_sub(t) >= interval,
    }
}

fn other_seat(s: Seat) -> Seat {
    match s {
        Seat::P1 => Seat::P2,
        Seat::P2 => Seat::P1,
    }
}

#[derive(Debug, Clone, Copy)]
struct Peer {
    nonce: u64,
    name: Name,
    latency: u8,
    mode: Mode,
    can_rollback: bool,
    listable: bool,
    seat: SeatAsk,
    /// ホストの決め方を使う（HELLOの +3のbit3）。
    pick: bool,
    nat: NatMark,
    /// この人がホストに決まったときのrollbackのD（HELLOの +83）。
    rollback_delay: u8,
    /// 使い捨ての鍵の公開鍵（HELLOの +50）。
    /// 役割を決めた後に同じnonceで違う公開鍵のHELLO / HELLO_ACKが来たら、違うnonceと同じく「相手がセッションを作り直した」と見る（`on_hello`の注）。
    public: [u8; PUBLIC_LEN],
}

/// 相手の印（role_nonce, 使い捨ての鍵の公開鍵）。同じ相手でもセッションを作り直すと公開鍵が変わる。
type PeerKey = (u64, [u8; PUBLIC_LEN]);

impl Peer {
    fn of_hello(h: &Hello) -> Peer {
        Peer {
            nonce: h.role_nonce,
            name: Name::from_hello(h),
            latency: h.latency.min(MAX_DELAY),
            mode: h.mode,
            can_rollback: h.can_rollback,
            listable: h.listable,
            seat: SeatAsk {
                wish: h.seat_wish,
                arrival: h.arrival,
            },
            public: h.public,
            pick: h.host_pick,
            nat: h.nat,
            rollback_delay: h.rollback_delay.min(MAX_DELAY),
        }
    }

    fn key(&self) -> PeerKey {
        (self.nonce, self.public)
    }
}

#[derive(Debug, Clone)]
pub struct Handshake {
    cfg: Config,
    /// 望むモード（`set_wish`。既定はdelay方式）。
    wish: ModeWish,
    /// rollbackができるかどうか（`set_can_rollback`。既定はtrue）。HELLOの +3のbit1で名乗る。
    can_rollback: bool,
    /// 観戦を許可するかどうか（`set_listable`。既定はfalse）。HELLOの +3のbit2で名乗る。
    listable: bool,
    seat: SeatAsk,
    /// 自分の使い捨ての鍵の公開鍵（HELLO / HELLO_ACKに載せる。`set_public`。接続のセッションが`Link::new`で設定する）。
    /// 設定しなければ0の32 Bになる。これはセッションを通らない検査の形で、相手はこれから鍵を作れないので封じたパケットは交わせない。
    public: [u8; PUBLIC_LEN],
    /// 自分のビルド番号（既定は`update::BUILD_NUMBER`）。
    build: u32,
    start: u32,
    state: State,
    /// 取りまとめ役（role_nonceの大きい方。AGREEを送る）ならHost。決まったホストは接続したときの`Established::role`。
    role: Option<Role>,
    peer: Option<Peer>,
    got_ack: bool,
    received_any: bool,
    hello_sent: Option<u32>,
    /// ホスト: 送るAGREEの値。ゲスト: 最初に受けたAGREEの値（以後もゲストは同じ値のAGREEを取りまとめ役へ返す）。
    terms: Option<Terms>,
    agree_sent: Option<u32>,
    counters: Counters,
    conflicts_at_start: u64,
    /// 自分の今の公開鍵を返した最後のHELLO_ACKの送り手（`is_fresh_ack`）。前のセッションへの返答は自分の古い公開鍵を返すので入らない。
    /// 役割を決めた相手と同じなら、その相手は今のセッションの自分のHELLOを受けている（このとき`got_ack`）。
    /// 違えば、その相手のHELLOを待つ（その相手へはまだHELLO_ACKを返していないので、相手はHELLOを送り続けている）。
    acked_by: Option<PeerKey>,
    /// 役割を決めた後に取り替えて手放した相手（新しい順）。
    /// 遅れて届いた・盗み見て送り直された古いセッションのHELLOで取り替え直さないための記録。
    /// この中の相手から今の公開鍵への返答が届いたら、その相手へ戻す。
    /// 古いHELLOで取り替えられても、今の相手はHELLO_ACKを受けた後なのでHELLOを送り直さない。
    /// 作り直した（`Fresh`）ときは持たない。
    kept: [Option<Peer>; KEPT_PEERS],
    /// ホストの決め方の設定（`set_host_pick`）。NoneならPROBEを交わさず、取りまとめ役がそのままホスト。
    host_pick: Option<host_pick::Setup>,
    /// 相手のPROBEの列（相手の上りの見積もり）。
    train: Train,
    /// この相手へPROBEを送ったかどうか（接続のセッションが`take_probe`で1回だけ取る）。
    probe_taken: bool,
    /// 取りまとめ役: 相手のHELLOで役割を決めた時刻（ゲストの値を待つ上限の起点）。
    lead_at: Option<u32>,
    /// 取りまとめ役: ゲストが測った自分の上り（PROBE_RESULTの最初の値）。
    peer_report: Option<u32>,
    /// ゲスト: 自分が測った相手の上り（列を締めたら決まる）とそれを最後に送った時刻。
    my_report: Option<u32>,
    report_sent: Option<u32>,
}

/// 手放した相手の記録の数。遅れて届きうるのはふつう直前のセッションのHELLOだけ。
/// 同じsession・同じnonceの古いセッションのHELLOをこれより多く盗み見て送り直されると今の相手を忘れうる（その相手が時間切れで作り直してHELLOを送り直すまで接続しない（推定））。
const KEPT_PEERS: usize = 4;

impl Handshake {
    pub fn new(cfg: Config, now: u32) -> Self {
        let mut cfg = cfg;
        cfg.latency = cfg.latency.min(MAX_DELAY);
        Self {
            cfg,
            wish: ModeWish::DELAY,
            can_rollback: true,
            listable: false,
            seat: SeatAsk::plain(cfg.host_seat),
            public: [0; PUBLIC_LEN],
            build: crate::update::BUILD_NUMBER,
            start: now,
            state: State::Connecting,
            role: None,
            peer: None,
            got_ack: false,
            received_any: false,
            hello_sent: None,
            terms: None,
            agree_sent: None,
            counters: Counters::default(),
            conflicts_at_start: 0,
            acked_by: None,
            kept: [None; KEPT_PEERS],
            host_pick: None,
            train: Train::default(),
            probe_taken: false,
            lead_at: None,
            peer_report: None,
            my_report: None,
            report_sent: None,
        }
    }

    /// 相手を忘れたときにホストの決め方の途中の値も捨てる（相手がセッションを作り直した）。
    fn forget_pick(&mut self) {
        self.train = Train::default();
        self.probe_taken = false;
        self.lead_at = None;
        self.peer_report = None;
        self.my_report = None;
        self.report_sent = None;
    }

    /// 時間切れを`now`で見て今の状態を返す。
    /// 接続のセッションが、届いたものを`on_packet`で渡す前に作り直すかどうかを決めるのに使う。
    /// 渡してから時間切れになると、その1通を「やめた後」として捨てるため。
    pub fn expire(&mut self, now: u32) -> State {
        self.check_timeout(now);
        self.state
    }

    /// 時間切れでやめた手順を作り直す。形は`Restart`のdoc。カウンタ（`Counters`）は続けて足す。
    /// 時間切れでやめていない（接続した・手順の中・同じnonceでやめた）なら何もしない（`None`）。
    /// 同じnonceは作り直しても同じ答えになるので作り直さない。
    pub fn restart(&mut self, now: u32) -> Option<Restart> {
        match self.state {
            State::Failed(FailReason::NothingReceived | FailReason::NoHandshake) => {}
            State::Failed(
                FailReason::SameNonce
                | FailReason::OtherVersion { .. }
                | FailReason::OtherBuild { .. },
            )
            | State::Connecting
            | State::Established(_) => {
                return None;
            }
        }
        let conflicted = self.counters.conflicts() != self.conflicts_at_start;
        let kind = if self.peer.is_some() && !conflicted {
            self.state = State::Connecting;
            self.start = now;
            self.received_any = false;
            self.got_ack = false;
            // 返答は前回の手順のもの。相手が今回の手順の自分のHELLOを受けたかどうかは、新しい返答で知り直す。
            self.acked_by = None;
            self.hello_sent = None;
            // ホストはAGREEもすぐ出す（相手が待っているなら4 msを待たせない）。
            self.agree_sent = None;
            Restart::Resume
        } else {
            let counters = self.counters;
            let wish = self.wish;
            let can = self.can_rollback;
            let listable = self.listable;
            let seat = self.seat;
            let public = self.public;
            let build = self.build;
            let host_pick = self.host_pick;
            *self = Handshake::new(self.cfg, now);
            self.host_pick = host_pick;
            self.counters = counters;
            // 望むモードは接続の設定なので作り直しても変えない（相手に別の値を名乗ると決まるモードが手順ごとに揺れる）。
            self.wish = wish;
            self.can_rollback = can;
            self.listable = listable;
            self.seat = seat;
            // 公開鍵も変えない（role_nonceと同じ寿命）。
            // 最初からの作り直しでもrole_nonceは生成し直さないので、公開鍵だけ替えると自分の古い公開鍵を覚えている相手と鍵が食い違ったまま接続できない。
            self.public = public;
            self.build = build;
            Restart::Fresh
        };
        self.conflicts_at_start = self.counters.conflicts();
        Some(kind)
    }

    /// 相手が別のバージョンで送ってきたので接続前に手順をやめる（`FailReason::OtherVersion`）。接続後・やめた後は何もしない。
    pub fn refuse_version(&mut self, theirs: u8) {
        if self.state == State::Connecting {
            self.state = State::Failed(FailReason::OtherVersion { theirs });
        }
    }

    /// 自分の使い捨ての鍵の公開鍵を設定する（接続のセッションが`Link::new`で1回）。
    /// HELLOを1通でも出した後・役割を決めた後は何もしない（相手が先に覚えた公開鍵と食い違う）。
    pub fn set_public(&mut self, public: [u8; PUBLIC_LEN]) {
        if self.role.is_some() || self.hello_sent.is_some() {
            return;
        }
        self.public = public;
    }

    /// 自分のビルド番号を設定する（検査用）。最初の`poll_send`の前だけ効く。
    pub fn set_build(&mut self, build: u32) {
        if self.role.is_some() || self.hello_sent.is_some() {
            return;
        }
        self.build = build;
    }

    pub fn build(&self) -> u32 {
        self.build
    }

    /// 役割を決めた相手の（role_nonce, 公開鍵）。相手のHELLOを受ける前と最初から作り直した後はNone。
    /// 接続のセッションはこれが変わったときに向きごとの鍵を作り直す（`link.rs`の`refresh_keys`）。
    pub fn peer_key(&self) -> Option<(u64, [u8; PUBLIC_LEN])> {
        self.peer.map(|p| (p.nonce, p.public))
    }

    /// 望むモードとDを設定する。最初の`poll_send`の前に呼ぶ。
    /// HELLOを出した後に変えると先に名乗った値と後のAGREEの値が食い違いうるので、役割を決めた後は何もしない。
    pub fn set_wish(&mut self, wish: ModeWish) {
        if self.role.is_some() {
            return;
        }
        self.wish = ModeWish {
            mode: wish.mode,
            rollback_delay: wish.rollback_delay.min(MAX_DELAY),
        };
    }

    pub fn wish(&self) -> ModeWish {
        self.wish
    }

    /// rollbackが「できる」かを設定する（HELLOの +3のbit1）。`set_wish`と同じく最初の`poll_send`の前に呼ぶ。役割を決めた後は何もしない。
    pub fn set_can_rollback(&mut self, can: bool) {
        if self.role.is_some() {
            return;
        }
        self.can_rollback = can;
    }

    pub fn can_rollback(&self) -> bool {
        self.can_rollback
    }

    /// 観戦を許可するかどうか（`[spectate] enable = 1`）を設定する（HELLOの +3のbit2）。
    /// HELLOを1通でも出した後・役割を決めた後は何もしない（相手が最初に受けたHELLOの値で覚えるので後から変えると両者の`may_list`が食い違う）。
    pub fn set_listable(&mut self, on: bool) {
        if self.role.is_some() || self.hello_sent.is_some() {
            return;
        }
        self.listable = on;
    }

    pub fn listable(&self) -> bool {
        self.listable
    }

    /// ホストの決め方を使うかどうかを設定する（HELLOの +3のbit3とネットワーク種別のフラグ）。
    /// `set_listable`と同じくHELLOを1通でも出した後・役割を決めた後は何もしない。
    /// 両者が使うときだけPROBEを交わして決める。
    pub fn set_host_pick(&mut self, p: Option<host_pick::Setup>) {
        if self.role.is_some() || self.hello_sent.is_some() {
            return;
        }
        self.host_pick = p;
    }

    pub fn host_pick(&self) -> Option<host_pick::Setup> {
        self.host_pick
    }

    fn own_nat(&self) -> NatMark {
        self.host_pick.map_or(NatMark::UNKNOWN, |p| p.nat)
    }

    /// 両者がホストの決め方を使うかどうか（相手のHELLOを受ける前はfalse）。
    fn pick_active(&self) -> bool {
        self.host_pick.is_some() && self.peer.is_some_and(|p| p.pick)
    }

    /// 相手へPROBEの列を送る番かどうか。
    /// 相手のHELLOで両者が決め方を使うと分かった後から接続する前までに1回だけtrueを返す。
    /// 接続のセッションは、そのHELLOの送り元へだけ送る。
    pub fn take_probe(&mut self) -> bool {
        if !self.probe_due() {
            return false;
        }
        self.probe_taken = true;
        true
    }

    /// `take_probe`がtrueを返す状態かどうか（取らない）。接続のセッションが候補に無い送り元へ列を送るかどうかを決めるのに使う。
    pub fn probe_due(&self) -> bool {
        !self.probe_taken && self.state == State::Connecting && self.pick_active()
    }

    /// 相手のPROBE（`at_us`は受けた時刻のµs。`now`と同じ時計）。
    /// 接続する前で自分が決め方を使うときだけ列に足す（相手のHELLOより先に届いた便も使う）。
    pub fn on_probe(&mut self, seq: u8, at_us: u64, now: u32) {
        self.check_timeout(now);
        if self.state != State::Connecting || self.host_pick.is_none() {
            return;
        }
        if self.train.on_probe(seq, at_us) {
            self.counters.probes = self.counters.probes.saturating_add(1);
        }
    }

    /// サイドの希望と先にいたか（ルームに先に入った側・接続コードを作った側。`wire::Arrival`）の名乗りを設定する（`seat_pick.rs`）。
    /// 最初の`poll_send`の前に呼ぶ。
    /// HELLOを1通でも出した後・役割を決めた後は何もしない。
    /// 相手は最初に受けたHELLOの希望で覚えるので、後から変えるとホストのAGREEのサイドとゲストの検証が食い違い、`agree_bad_seat`で捨て続ける。
    pub fn set_seat_ask(&mut self, ask: SeatAsk) {
        if self.role.is_some() || self.hello_sent.is_some() {
            return;
        }
        self.seat = ask;
    }

    pub fn seat_ask(&self) -> SeatAsk {
        self.seat
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// 自分が取りまとめ役（role_nonceの大きい方。AGREEを送る）かどうか（相手のHELLOを受ける前はNone）。
    /// ホストの決め方を使わない接続ではそのままホストになる。決まったホストは`Established::role`。
    pub fn coordinator(&self) -> Option<bool> {
        self.role.map(|r| r == Role::Host)
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn session_id(&self) -> u32 {
        self.cfg.session_id
    }

    fn hello(&self) -> Message {
        Message::Hello(Hello {
            proto_ver: PROTO_VER,
            latency: self.cfg.latency,
            name_len: self.cfg.name.len,
            mode: self.wish.mode,
            can_rollback: self.can_rollback,
            listable: self.listable,
            role_nonce: self.cfg.role_nonce,
            name: self.cfg.name.bytes,
            seat_wish: self.seat.wish,
            arrival: self.seat.arrival,
            build: self.build,
            public: self.public,
            host_pick: self.host_pick.is_some(),
            nat: self.own_nat(),
            rollback_delay: self.wish.rollback_delay,
        })
    }

    fn agree(t: Terms) -> Message {
        Message::Agree {
            host_seat: t.host_seat,
            delay: t.delay,
            mode: t.mode,
            pick: t.pick,
        }
    }

    /// 決まったホストのサイド。サイドは両者の希望から決まる（`seat_pick.rs`）。名乗らない経路はバージョン3までと同じ「ホストの設定のサイド」。
    /// 取りまとめ役がAGREEを作るときもゲストが検証するときもこれを使う。
    /// `me_host`は自分がホストかどうか。
    fn host_seat_for(&self, peer: &Peer, me_host: bool) -> Seat {
        let (mine, theirs) = ((self.seat, self.cfg.role_nonce), (peer.seat, peer.nonce));
        let ((h_ask, h_nonce), (g_ask, g_nonce)) = if me_host {
            (mine, theirs)
        } else {
            (theirs, mine)
        };
        seat_pick::host_seat(h_ask, g_ask, h_nonce, g_nonce)
    }

    /// 取りまとめ役が決まったホスト（`pick.host_is_sender`なら自分）の側の値でAGREEの値を組み立てる。
    fn terms_for(&self, peer: &Peer, pick: Pick) -> Terms {
        let me_host = pick.host_is_sender;
        let host_seat = self.host_seat_for(peer, me_host);
        let able = self.can_rollback && peer.can_rollback;
        match agreed_mode(self.wish.mode, peer.mode, able) {
            // 希望の大きい方にする。小さい方にすると大きい値を望んだ側（回線が遅い見込み）のワードが毎フレーム間に合わず、両者が待つ。
            Mode::Delay => Terms {
                host_seat,
                delay: self.cfg.latency.max(peer.latency),
                mode: Mode::Delay,
                pick,
            },
            Mode::Rollback => Terms {
                host_seat,
                delay: if me_host {
                    self.wish.rollback_delay
                } else {
                    peer.rollback_delay
                },
                mode: Mode::Rollback,
                pick,
            },
        }
    }

    /// ゲストが受けたAGREEのどちらがホストかの値を検証する。
    /// 両者が決め方を使わない接続は`Pick::NONE`だけを通す。
    /// 使う接続は次が全部揃ったときだけ通す: ネットワーク種別のフラグがHELLOで知った値と同じ・取りまとめ役の上りが自分の送った値か0（届かなかった）・載っている値から決まるホストと同じ。
    /// 自分がまだ値を持っていなければ（作り直して測り直す前）、取りまとめ役の上りは照らし合わせない。
    /// 相手が作り直さずに前の値で決めたAGREEを断り続けると接続できないため。役の一致は、そのAGREEを採ることで保たれる。
    fn pick_ok(&self, peer: &Peer, p: &Pick) -> bool {
        if !self.pick_active() {
            return *p == Pick::NONE;
        }
        let up_ok = match self.my_report {
            None => true,
            Some(mine) => p.sender_up == 0 || p.sender_up == mine,
        };
        p.sender_nat == peer.nat && p.receiver_nat == self.own_nat() && up_ok && p.consistent()
    }

    /// 接続前に10秒経ったらやめる。理由は「同じsession_idのものが届いたか」で分ける。`on_packet`と`poll_send`の両方の入口で見る。
    fn check_timeout(&mut self, now: u32) {
        if self.state == State::Connecting && now.wrapping_sub(self.start) >= TIMEOUT_MS {
            self.state = State::Failed(if self.received_any {
                FailReason::NoHandshake
            } else {
                FailReason::NothingReceived
            });
        }
    }

    /// 接続した結果を記録する。ホストとゲストで同じ`Terms`から作るので、サイドとdelayは食い違わない。`role`は決まったホストから見た自分の役。
    fn establish(&mut self, role: Role, peer: Peer, t: Terms) {
        let i_sent = self.role == Some(Role::Host);
        let seat = match role {
            Role::Host => t.host_seat,
            Role::Guest => other_seat(t.host_seat),
        };
        self.state = State::Established(Established {
            role,
            seat,
            host_seat: t.host_seat,
            mode: t.mode,
            delay: t.delay,
            own_name: self.cfg.name,
            peer_name: peer.name,
            peer_latency: peer.latency,
            peer_mode: peer.mode,
            rollback_able: self.can_rollback && peer.can_rollback,
            may_list: crate::watch_list::may_list(self.listable, peer.listable),
            pick: t.pick.note(i_sent, self.pick_active()),
        });
    }

    /// 受けたパケット。返ったものがあれば受けた相手へすぐ送る（HELLO_ACKか、ゲストのAGREEの返答）。
    pub fn on_packet(&mut self, pkt: &Packet, now: u32) -> Option<Message> {
        self.check_timeout(now);
        if let State::Failed(_) = self.state {
            self.counters.after_fail = self.counters.after_fail.saturating_add(1);
            return None;
        }
        if pkt.session_id != self.cfg.session_id {
            self.counters.wrong_session = self.counters.wrong_session.saturating_add(1);
            return None;
        }
        self.received_any = true;
        match pkt.msg {
            Message::Hello(h) => self.on_hello(&h, now),
            Message::HelloAck {
                echo_nonce,
                role_nonce,
                public,
                echo_public,
            } => {
                self.on_hello_ack((echo_nonce, echo_public), (role_nonce, public), now);
                None
            }
            Message::Agree {
                host_seat,
                delay,
                mode,
                pick,
            } => self.on_agree(Terms {
                host_seat,
                delay,
                mode,
                pick,
            }),
            Message::ProbeResult { up } => {
                // 取りまとめ役が決める前の最初の値だけを使う（決めた後に変えない）。
                if self.role == Some(Role::Host)
                    && self.state == State::Connecting
                    && self.terms.is_none()
                    && self.pick_active()
                    && self.peer_report.is_none()
                {
                    self.peer_report = Some(up);
                }
                None
            }
            _ => {
                self.counters.unrelated = self.counters.unrelated.saturating_add(1);
                None
            }
        }
    }

    fn on_hello(&mut self, h: &Hello, now: u32) -> Option<Message> {
        let mine = self.cfg.role_nonce;
        if h.build != self.build {
            // ビルド番号が違う。HELLOはタグが合っているので、秘密の乱数を知る相手が別のビルドで動いている。
            // バージョン違いと同じく、接続前に手順をやめて理由を返す。
            // 相手への返答（HELLO_ACKなど）も出さない。相手は自分のHELLOを受けて同じ理由でやめる。まだ1通も出していなければ`poll_send`が1通だけ出す。
            if self.state == State::Connecting {
                self.state = State::Failed(FailReason::OtherBuild { theirs: h.build });
            }
            return None;
        }
        let key = (h.role_nonce, h.public);
        match self.peer {
            None => {
                if h.role_nonce == mine {
                    // 生成し直さずにやめる。生成し直すと、自分自身へ送っているときに止まらない。
                    // また生成し直す前のHELLOが遅れて届くと、両者が別の組み合わせで役割を決めて両方がホストになりうる。
                    self.state = State::Failed(FailReason::SameNonce);
                    return None;
                }
                if self.acked_by.is_some_and(|k| k != key) {
                    // 今の公開鍵への返答をくれた別の相手がいる。その相手のHELLOを待つ。
                    self.counters.stale_nonce = self.counters.stale_nonce.saturating_add(1);
                    return None;
                }
                self.adopt(Peer::of_hello(h), now);
            }
            Some(p) if p.key() == key => {}
            Some(p) => {
                if self.state != State::Connecting {
                    // 接続後は鍵を替えない（セッションの送信側がその鍵で封じている）ので食い違いとして捨てる。
                    self.counters.stale_nonce = self.counters.stale_nonce.saturating_add(1);
                    return None;
                }
                if self.acked_by == Some(key) {
                    // 今の公開鍵への返答をくれた相手のHELLO。先に届いたHELLO（古いセッションのものを送り直されたなど）の相手を手放す。
                    self.counters.swapped = self.counters.swapped.saturating_add(1);
                } else if self.kept.iter().flatten().any(|o| o.key() == key) {
                    // 遅れて届いた古いセッションのHELLO。手放した相手へはHELLOでは戻らない。
                    // 戻すと、相手が新しいセッションのHELLOを止めた後に古い鍵のまま残る。
                    // 食い違いの数にも加えない。
                    self.counters.retired_hello = self.counters.retired_hello.saturating_add(1);
                    return None;
                } else if p.nonce != h.role_nonce || self.acked_by.is_some_and(|k| k != p.key()) {
                    // 違うnonce（相手の再起動・自分のHELLOの跳ね返りなど）か、今の公開鍵への返答をくれた別の相手を待っている。
                    // 返答も出さない（出すと相手が別の組み合わせで役割を決めうる）。
                    // 違うnonceの相手が今のセッションの相手なら、自分のHELLOに返すHELLO_ACKでここを通れるようになる。
                    self.counters.stale_nonce = self.counters.stale_nonce.saturating_add(1);
                    return None;
                } else {
                    // 同じnonceで公開鍵だけ違う。相手がセッションを作り直した印として新しい公開鍵で覚え直す。
                    // ゲーム内のメニューで取りやめた後に同じ相手のコードを貼り直すとこうなる。
                    // 捨てると`TIMEOUT_MS`まで相手のパケットを開けないまま待つ。
                    // 古いセッションのHELLOを送り直されたのなら、今の相手が自分のHELLOに返すHELLO_ACKで戻る（`on_hello_ack`）。
                    self.counters.rekeyed = self.counters.rekeyed.saturating_add(1);
                }
                self.adopt(Peer::of_hello(h), now);
                if !self.got_ack {
                    // 新しい相手は自分のHELLOをまだ受けていない（前の相手が返したHELLO_ACKは新しい相手の印にならない）。
                    self.hello_sent = None;
                }
            }
        }
        if !self.got_ack {
            // 即送り（自分のHELLOを相手へ）: 相手のHELLOが届いたので、相手はいま受信できる。しかし自分のHELLOはまだ届いていないかもしれない。
            // 遅れて始めた相手にはそれまでのHELLOが全部落ちている。
            // そのため20 msの間隔を待つと、相手は役割を決められずその間のAGREEを捨て続ける（片道が20 msより短いLANほど効く）。
            // 増える送信は、ackが届くまで相手のHELLO 1通につき1通まで。
            // 同じmsにもうHELLOを出していれば出し直さない。
            // 受信リングに溜まった相手のHELLOを1度に渡されたとき、1通ごとに出すと相手の受信リング（64枠）を溢れさせた。
            if self.hello_sent != Some(now) {
                self.hello_sent = None;
            }
        }
        // 接続後も返す。相手は自分のHELLO_ACKが届くまでHELLOを送り続けている。
        Some(Message::HelloAck {
            echo_nonce: h.role_nonce,
            role_nonce: mine,
            public: self.public,
            echo_public: h.public,
        })
    }

    /// `peer`を役割を決めた相手にする（前の相手は`kept`へ移す）。役割・サイド・条件は新しい相手の値で決め直す。
    /// 向きごとの鍵は接続のセッションが`peer_key`の変化で作り直す。
    fn adopt(&mut self, peer: Peer, now: u32) {
        let key = peer.key();
        let mut kept = [None; KEPT_PEERS];
        let mut slots = kept.iter_mut();
        if let Some(old) = self.peer.replace(peer) {
            if self.acked_by == Some(old.key()) {
                self.acked_by = None;
            }
            if let Some(s) = slots.next() {
                *s = Some(old);
            }
        }
        for o in self.kept.iter().flatten().filter(|o| o.key() != key) {
            if let Some(s) = slots.next() {
                *s = Some(*o);
            }
        }
        self.kept = kept;
        self.role = None;
        self.terms = None;
        self.agree_sent = None;
        self.forget_pick();
        self.got_ack = self.acked_by == Some(key);
        if peer.nonce < self.cfg.role_nonce {
            self.role = Some(Role::Host);
            self.lead_at = Some(now);
            // 決め方を使わないなら自分がホストのままですぐ決める。使うならゲストの値を待ってから決める（`poll_send`）。
            if !self.pick_active() {
                self.terms = Some(self.terms_for(&peer, Pick::NONE));
            }
        } else {
            self.role = Some(Role::Guest);
        }
    }

    /// 自分のHELLOへの今のセッションの返答かどうか（返ったrole_nonceと公開鍵が自分の今の値）。
    /// 役割の乱数はメニューを開いている間は同じなので、公開鍵（セッションごとに生成する）まで比べないと前のセッションへの返答を見分けられない。
    pub fn is_fresh_ack(&self, echo_nonce: u64, echo_public: &[u8; PUBLIC_LEN]) -> bool {
        echo_nonce == self.cfg.role_nonce && *echo_public == self.public
    }

    fn on_hello_ack(
        &mut self,
        (echo, echo_public): (u64, [u8; PUBLIC_LEN]),
        key: PeerKey,
        now: u32,
    ) {
        if !self.is_fresh_ack(echo, &echo_public) {
            self.counters.wrong_echo = self.counters.wrong_echo.saturating_add(1);
            return;
        }
        if let Some(p) = self.peer
            && p.key() != key
        {
            // 自分の今のHELLOには返答が来ているが、役割を決めた相手とは別の相手。
            // 役割を決めた相手がもう返答している（`got_ack`）か接続後なら、先に決めた方を採る。
            // まだなら、返答してきた方が今のセッションの相手になる。手放した相手ならすぐ戻し、知らない相手ならそのHELLOを待つ。
            if self.state == State::Connecting && !self.got_ack {
                self.acked_by = Some(key);
                if let Some(o) = self.kept.iter().flatten().find(|o| o.key() == key).copied() {
                    self.counters.swapped = self.counters.swapped.saturating_add(1);
                    self.adopt(o, now);
                    return;
                }
            }
            self.counters.stale_nonce = self.counters.stale_nonce.saturating_add(1);
            return;
        }
        // 役割を決める前のHELLO_ACKではHELLOをやめない（`got_ack`は相手のHELLOで役割を決めたとき`adopt`で立つ）。
        // 返答した相手が取りやめて同じnonce・新しい鍵で始め直すと、そのHELLOは`acked_by`と食い違うので返答しない。
        // そのため自分のHELLOが届かないと、新しい相手は返答を作れない。
        self.acked_by = Some(key);
        self.got_ack = self.peer.is_some();
    }

    /// 候補にも学んだ送り元にも無いアドレスから来たHELLOに、そのアドレスへ1回だけ返すもの（自分のHELLOと、取りまとめ役ならAGREE）。
    /// 送った時刻は記録しない（候補へ送る間隔を動かさない）。
    /// そのアドレスを覚えるのは、相手が今のセッションのHELLO_ACKを返してから（接続のセッション`link.rs`）。
    pub fn answer_unlisted(&self) -> Outbox {
        let mut out = Outbox::default();
        if self.state != State::Connecting {
            return out;
        }
        if !self.got_ack {
            out.hello = Some(self.hello());
        }
        if self.role == Some(Role::Host)
            && let Some(t) = self.terms
        {
            out.agree = Some(Self::agree(t));
        }
        out
    }

    fn on_agree(&mut self, got: Terms) -> Option<Message> {
        let (Some(role), Some(peer)) = (self.role, self.peer) else {
            // ゲストは役割を決める前のAGREEを捨てる。ホストが送り直すのでHELLOが届いた後のAGREEで決まる。
            self.counters.agree_early = self.counters.agree_early.saturating_add(1);
            return None;
        };
        match role {
            Role::Guest => {
                if !self.pick_ok(&peer, &got.pick) {
                    // どちらがホストかが食い違うと両者がホスト（かゲスト）のまま接続する。モード・サイドより先に見る（サイドは決まったホストで変わる）。
                    self.counters.agree_bad_pick = self.counters.agree_bad_pick.saturating_add(1);
                    return None;
                }
                let able = self.can_rollback && peer.can_rollback;
                if got.mode != agreed_mode(self.wish.mode, peer.mode, able) {
                    // 両者の望むモードから決まるモードと違う。値の比較（下）より先に見る。
                    // 最初のAGREEでこれを受けて接続すると、ホストの側の決まりとワードの使い方が食い違うため。
                    self.counters.agree_bad_mode = self.counters.agree_bad_mode.saturating_add(1);
                    return None;
                }
                if got.host_seat != self.host_seat_for(&peer, !got.pick.host_is_sender) {
                    // 両者のサイドの希望から決まるサイドと違う。モードと同じく値の比較より先に見る。
                    self.counters.agree_bad_seat = self.counters.agree_bad_seat.saturating_add(1);
                    return None;
                }
                match self.terms {
                    None => {
                        self.terms = Some(got);
                        let me = if got.pick.host_is_sender {
                            Role::Guest
                        } else {
                            Role::Host
                        };
                        self.establish(me, peer, got);
                    }
                    Some(t) if t != got => {
                        // 最初と違う値に返答するとホストは違う値で接続しうる。捨てて数える。
                        self.counters.agree_mismatch =
                            self.counters.agree_mismatch.saturating_add(1);
                        return None;
                    }
                    Some(_) => {}
                }
                // 返答は最初の値で返す。接続後も続ける（ホストへの返答が落ちたとき）。
                Some(Self::agree(got))
            }
            Role::Host => {
                if self.state == State::Connecting {
                    if self.terms == Some(got) {
                        let me = if got.pick.host_is_sender {
                            Role::Host
                        } else {
                            Role::Guest
                        };
                        self.establish(me, peer, got);
                    } else {
                        // 送った値と違う返答で接続すると両者のサイドかdelayが食い違う。
                        self.counters.agree_mismatch =
                            self.counters.agree_mismatch.saturating_add(1);
                    }
                }
                None
            }
        }
    }

    pub fn poll_send(&mut self, now: u32) -> Outbox {
        self.check_timeout(now);
        let mut out = Outbox::default();
        if let State::Failed(r) = self.state {
            // バージョン違い・ビルド違いで断ったが自分のHELLOをまだ1通も出していない（相手のHELLOが最初の送信より先に届いた）なら、1通だけ出す。
            // 出さないと、相手は断った理由を知らないまま10秒の時間切れになる。
            // 相手のHELLOは相手の側で断られるので手順は進まない。
            if matches!(
                r,
                FailReason::OtherVersion { .. } | FailReason::OtherBuild { .. }
            ) && self.hello_sent.is_none()
            {
                out.hello = Some(self.hello());
                self.hello_sent = Some(now);
            }
            return out;
        }
        if !self.got_ack && due(self.hello_sent, now, HELLO_INTERVAL_MS) {
            out.hello = Some(self.hello());
            self.hello_sent = Some(now);
        }
        if self.state == State::Connecting && self.terms.is_none() {
            self.poll_pick(now, &mut out);
        }
        if self.role == Some(Role::Host)
            && self.state == State::Connecting
            && let Some(t) = self.terms
            && due(self.agree_sent, now, AGREE_INTERVAL_MS)
        {
            out.agree = Some(Self::agree(t));
            self.agree_sent = Some(now);
        }
        out
    }

    /// ホストの決め方の時計（接続する前・AGREEの値を決める前だけ）。取りまとめ役は、ゲストの値と自分の列が揃うか待つ上限が過ぎたら決める。
    /// ゲストは列を締めたら測った値をAGREEが届くまで送り直す。
    fn poll_pick(&mut self, now: u32, out: &mut Outbox) {
        let (Some(role), Some(peer)) = (self.role, self.peer) else {
            return;
        };
        if !self.pick_active() {
            return;
        }
        let now_us = u64::from(now) * 1_000;
        let fake = self.host_pick.and_then(|p| p.fake_up);
        match role {
            Role::Host => {
                let waited = now.wrapping_sub(self.lead_at.unwrap_or(now));
                let ready = self.peer_report.is_some() && self.train.closed(now_us);
                if ready || waited >= host_pick::REPORT_WAIT_MS {
                    let pick = Pick::decided(
                        Facts {
                            up: self.peer_report.unwrap_or(0),
                            nat: self.own_nat(),
                        },
                        Facts {
                            up: self.train.estimate_with(fake),
                            nat: peer.nat,
                        },
                    );
                    self.terms = Some(self.terms_for(&peer, pick));
                    self.agree_sent = None;
                }
            }
            Role::Guest => {
                if self.my_report.is_none() && self.train.closed(now_us) {
                    self.my_report = Some(self.train.estimate_with(fake));
                    self.report_sent = None;
                }
                if let Some(up) = self.my_report
                    && due(self.report_sent, now, host_pick::REPORT_INTERVAL_MS)
                {
                    out.report = Some(Message::ProbeResult { up });
                    self.report_sent = Some(now);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 部品の検査（1台に手でパケットを渡す）と2台の模型（1 ms単位の時計・方向ごとの送達の列）。
    //! 模型では送るものを必ず`encode` → `decode`に通す。
    //! 台0と台1の役割はnonceで決まるので、どちらがホストかを台の番号と揃えない筋も含める。

    use super::*;
    use crate::wire::{self, MAX_DATAGRAM, Message};

    const SID: u32 = 0x5EED_1234;

    fn tkey() -> crate::auth::AuthKey {
        crate::auth::AuthKey::derive(&crate::auth::Secret([0x11; 16]), SID)
    }

    fn tdir() -> crate::seal::DirKeys {
        crate::seal::DirKeys::from_raw([0x22; 32], [0x22; 32])
    }

    fn tenc(sid: u32, msg: &Message, buf: &mut [u8; MAX_DATAGRAM]) -> usize {
        if wire::is_signed(msg) {
            wire::encode(&tkey(), sid, msg, buf)
        } else {
            let s = tdir().sealer(crate::seal::Sender::Link);
            wire::seal(&s, &crate::seal::Seq::new(), sid, msg, buf).expect("封じられる")
        }
    }

    fn tdec(bytes: &[u8]) -> Result<Packet, wire::DecodeError> {
        wire::decode(&tkey(), Some(&tdir().opener()), bytes)
    }

    /// 検査用の決まった乱数（splitmix64。`selection.rs`の検査のモジュールの中のものは使えないので書き直した）。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n)) as u32
        }
    }

    fn cfg(nonce: u64, latency: u8, name: &str) -> Config {
        cfg_seat(nonce, latency, name, Seat::P1)
    }

    fn cfg_seat(nonce: u64, latency: u8, name: &str, host_seat: Seat) -> Config {
        Config {
            session_id: SID,
            role_nonce: nonce,
            latency,
            name: Name::new(name),
            host_seat,
        }
    }

    fn pkt(msg: Message) -> Packet {
        pkt_sid(SID, msg)
    }

    fn pkt_sid(sid: u32, msg: Message) -> Packet {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = tenc(sid, &msg, &mut buf);
        tdec(&buf[..n]).expect("自分が詰めたものは読める")
    }

    fn hello_of(nonce: u64, latency: u8, name: &str) -> Message {
        hello_mode(nonce, latency, name, Mode::Delay)
    }

    fn hello_mode(nonce: u64, latency: u8, name: &str, mode: Mode) -> Message {
        hello_ask(nonce, latency, name, mode, SeatAsk::plain(Seat::P1))
    }

    fn hello_ask(nonce: u64, latency: u8, name: &str, mode: Mode, ask: SeatAsk) -> Message {
        let n = Name::new(name);
        Message::Hello(Hello {
            proto_ver: PROTO_VER,
            latency,
            name_len: n.len,
            mode,
            can_rollback: true,
            listable: false,
            role_nonce: nonce,
            name: n.bytes,
            seat_wish: ask.wish,
            arrival: ask.arrival,
            build: crate::update::BUILD_NUMBER,
            public: [0; PUBLIC_LEN],
            host_pick: false,
            nat: crate::nat::NatMark::UNKNOWN,
            rollback_delay: 0,
        })
    }

    fn hello_unable(nonce: u64, latency: u8, name: &str, mode: Mode) -> Message {
        let Message::Hello(mut h) = hello_mode(nonce, latency, name, mode) else {
            unreachable!("HELLO を作った");
        };
        h.can_rollback = false;
        Message::Hello(h)
    }

    fn agree_of(seat: Seat, delay: u8) -> Message {
        agree_mode(seat, delay, Mode::Delay)
    }

    fn agree_mode(seat: Seat, delay: u8, mode: Mode) -> Message {
        Message::Agree {
            host_seat: seat,
            delay,
            mode,
            pick: crate::host_pick::Pick::NONE,
        }
    }

    fn with_wish(c: Config, wish: ModeWish) -> Handshake {
        let mut h = Handshake::new(c, 0);
        h.set_wish(wish);
        h
    }

    #[test]
    fn mode_is_rollback_only_when_both_wish_it() {
        use Mode::{Delay as Dl, Rollback as Rb};
        // (ホストの望み, ゲストの望み, 決まるモード, AGREEのdelayのフィールド)。ホスト: latency 2・D 3、ゲスト: latency 7・D 5。
        let table = [
            (Rb, Rb, Rb, 3u8),
            (Rb, Dl, Dl, 7),
            (Dl, Rb, Dl, 7),
            (Dl, Dl, Dl, 7),
        ];
        for (hw, gw, want_mode, want_delay) in table {
            let tag = format!("ホスト {hw:?} × ゲスト {gw:?}");
            let wish = |m: Mode, d: u8| ModeWish {
                mode: m,
                rollback_delay: d,
            };
            let mut h = with_wish(cfg(900, 2, "host"), wish(hw, 3));
            let mut g = with_wish(cfg(100, 7, "guest"), wish(gw, 5));
            let Some(Message::Hello(gh)) = g.poll_send(0).hello else {
                panic!("{tag}: ゲストの HELLO");
            };
            assert_eq!(gh.mode, gw, "{tag}");
            let Some(Message::Hello(hh)) = h.poll_send(0).hello else {
                panic!("{tag}: ホストの HELLO");
            };
            h.on_packet(&pkt(Message::Hello(gh)), 1);
            g.on_packet(&pkt(Message::Hello(hh)), 1);
            let agree = h.poll_send(1).agree.expect("ホストは AGREE を送る");
            assert_eq!(agree, agree_mode(Seat::P1, want_delay, want_mode), "{tag}");
            let echo = g.on_packet(&pkt(agree), 2).expect("ゲストは返す");
            assert_eq!(echo, agree, "{tag}");
            h.on_packet(&pkt(echo), 3);
            for (who, s, peer) in [("ホスト", h.state(), gw), ("ゲスト", g.state(), hw)] {
                let State::Established(e) = s else {
                    panic!("{tag} {who}: 繋がっていない {s:?}");
                };
                assert_eq!((e.mode, e.delay), (want_mode, want_delay), "{tag} {who}");
                assert_eq!(e.peer_mode, peer, "{tag} {who}: 相手の望み");
            }
        }
    }

    #[test]
    fn rollback_able_is_the_and_of_both_hellos_and_never_forces_rollback() {
        let wish = |m| ModeWish {
            mode: m,
            rollback_delay: 2,
        };
        let table = [
            (Mode::Delay, true, Mode::Delay, true, Mode::Delay, true),
            (
                Mode::Rollback,
                true,
                Mode::Rollback,
                true,
                Mode::Rollback,
                true,
            ),
            (
                Mode::Rollback,
                false,
                Mode::Rollback,
                true,
                Mode::Delay,
                false,
            ),
            (
                Mode::Rollback,
                true,
                Mode::Rollback,
                false,
                Mode::Delay,
                false,
            ),
            (Mode::Delay, true, Mode::Delay, false, Mode::Delay, false),
            (Mode::Delay, false, Mode::Delay, false, Mode::Delay, false),
        ];
        for (mw, mc, pw, pc, want_mode, want_able) in table {
            let tag = format!("{mw:?}/{mc} × {pw:?}/{pc}");
            let mut g = Handshake::new(cfg(100, 4, "guest"), 0);
            g.set_wish(wish(mw));
            g.set_can_rollback(mc);
            let Some(Message::Hello(gh)) = g.poll_send(0).hello else {
                panic!("{tag}: ゲストの HELLO");
            };
            assert_eq!((gh.mode, gh.can_rollback), (mw, mc), "{tag}: 名乗り");
            let peer = if pc {
                hello_mode(900, 4, "host", pw)
            } else {
                hello_unable(900, 4, "host", pw)
            };
            g.on_packet(&pkt(peer), 1);
            let agree = agree_mode(Seat::P1, 2, want_mode);
            assert!(g.on_packet(&pkt(agree), 2).is_some(), "{tag}");
            let State::Established(e) = g.state() else {
                panic!("{tag}: 繋がっていない");
            };
            assert_eq!(
                (e.mode, e.rollback_able),
                (want_mode, want_able),
                "{tag} ゲスト"
            );
            let other = if want_mode == Mode::Rollback {
                Mode::Delay
            } else {
                Mode::Rollback
            };
            let mut g2 = Handshake::new(cfg(100, 4, "guest"), 0);
            g2.set_wish(wish(mw));
            g2.set_can_rollback(mc);
            g2.on_packet(
                &pkt(if pc {
                    hello_mode(900, 4, "host", pw)
                } else {
                    hello_unable(900, 4, "host", pw)
                }),
                1,
            );
            assert_eq!(
                g2.on_packet(&pkt(agree_mode(Seat::P1, 2, other)), 2),
                None,
                "{tag}: 決まりと違うモードの AGREE"
            );
            let mut h = Handshake::new(cfg(900, 4, "host"), 0);
            h.set_wish(wish(pw));
            h.set_can_rollback(pc);
            h.on_packet(
                &pkt(if mc {
                    hello_mode(100, 4, "guest", mw)
                } else {
                    hello_unable(100, 4, "guest", mw)
                }),
                1,
            );
            let sent = h.poll_send(1).agree.expect("ホストは AGREE を送る");
            let Message::Agree { mode, .. } = sent else {
                panic!("{tag}: AGREE");
            };
            assert_eq!(mode, want_mode, "{tag}: ホストの決め");
            h.on_packet(&pkt(sent), 2);
            let State::Established(e) = h.state() else {
                panic!("{tag}: ホストが繋がっていない");
            };
            assert_eq!(
                (e.mode, e.rollback_able),
                (want_mode, want_able),
                "{tag} ホスト"
            );
        }
        let mut a = Handshake::new(cfg(900, 2, "A"), 0);
        a.set_can_rollback(false);
        a.on_packet(&pkt(hello_mode(100, 2, "B", Mode::Delay)), 1);
        a.set_can_rollback(true);
        assert!(!a.can_rollback());
    }

    /// 観戦の許可のフラグは、ホストとゲストの4通りのうち両者が立てたときだけ両方の`may_list`が真になる。
    /// 名乗りはHELLOのbit2で運ぶ。相手のフラグを自分のフラグで置き換えない（ホストだけ・ゲストだけでは決まらない）。
    #[test]
    fn may_list_needs_both_hellos_to_allow_listing() {
        for (host_on, guest_on) in [(true, true), (true, false), (false, true), (false, false)] {
            let tag = format!("ホスト {host_on} × ゲスト {guest_on}");
            let mut h = Handshake::new(cfg(900, 2, "host"), 0);
            let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
            assert!(!h.listable() && !g.listable(), "{tag}: 既定は載せない");
            h.set_listable(host_on);
            g.set_listable(guest_on);
            let Some(Message::Hello(hh)) = h.poll_send(0).hello else {
                panic!("{tag}: ホストの HELLO");
            };
            let Some(Message::Hello(gh)) = g.poll_send(0).hello else {
                panic!("{tag}: ゲストの HELLO");
            };
            assert_eq!(
                (hh.listable, gh.listable),
                (host_on, guest_on),
                "{tag}: 名乗り"
            );
            h.on_packet(&pkt(Message::Hello(gh)), 1);
            g.on_packet(&pkt(Message::Hello(hh)), 1);
            assert_eq!(h.coordinator(), Some(true), "{tag}");
            let agree = h.poll_send(1).agree.expect("ホストは AGREE を送る");
            let echo = g.on_packet(&pkt(agree), 2).expect("ゲストは返す");
            h.on_packet(&pkt(echo), 3);
            for (who, s) in [("ホスト", h.state()), ("ゲスト", g.state())] {
                let State::Established(e) = s else {
                    panic!("{tag} {who}: 繋がっていない {s:?}");
                };
                assert_eq!(e.may_list, host_on && guest_on, "{tag} {who}");
            }
        }
        // HELLOを出した後は変えない（相手が覚えた名乗りと食い違う）。
        let mut a = Handshake::new(cfg(900, 2, "A"), 0);
        a.set_listable(true);
        assert!(a.poll_send(0).hello.is_some());
        a.set_listable(false);
        assert!(a.listable());
        // 時間切れの後に最初から作り直しても名乗りは変えない（接続の設定なので）。
        assert_eq!(
            a.expire(TIMEOUT_MS),
            State::Failed(FailReason::NothingReceived)
        );
        assert_eq!(a.restart(TIMEOUT_MS), Some(Restart::Fresh));
        let Some(Message::Hello(again)) = a.poll_send(TIMEOUT_MS).hello else {
            panic!("作り直した後の HELLO");
        };
        assert!(again.listable);
    }

    #[test]
    fn a_peer_without_mode_bits_gets_a_delay_agree() {
        // +3が0のHELLO（delay方式を望み、rollbackができない）。goldenはwire.rs。
        let mut buf = [0u8; wire::MAX_DATAGRAM];
        let n = wire::encode(
            &tkey(),
            SID,
            &hello_unable(100, 2, "old", Mode::Delay),
            &mut buf,
        );
        assert_eq!(
            buf[wire::HEADER_LEN + 3],
            0,
            "古いバージョンの予約の位置が 0"
        );
        let mut h = with_wish(cfg(900, 2, "host"), ModeWish::rollback(1));
        h.on_packet(&tdec(&buf[..n]).unwrap(), 1);
        let agree = h.poll_send(1).agree.expect("AGREE");
        assert_eq!(agree, agree_of(Seat::P1, 2), "delay 方式・希望の大きい方");
        let len = tenc(SID, &agree, &mut buf);
        assert_eq!(
            len,
            wire::HEADER_LEN + 14 + crate::seal::SEAL_OVERHEAD,
            "AGREE の本文 14 B（封じて nonce の欄とタグが付く）"
        );
        h.on_packet(&pkt(agree), 2);
        assert!(matches!(
            h.state(),
            State::Established(e) if e.mode == Mode::Delay && e.peer_mode == Mode::Delay
        ));
    }

    #[test]
    fn guest_refuses_an_agree_with_the_wrong_mode() {
        let mut g = with_wish(cfg(100, 4, "guest"), ModeWish::DELAY);
        g.on_packet(&pkt(hello_mode(900, 4, "host", Mode::Rollback)), 1);
        assert_eq!(
            g.on_packet(&pkt(agree_mode(Seat::P1, 1, Mode::Rollback)), 2),
            None
        );
        assert_eq!(g.state(), State::Connecting);
        let mut g2 = with_wish(cfg(100, 4, "guest"), ModeWish::rollback(2));
        g2.on_packet(&pkt(hello_mode(900, 4, "host", Mode::Rollback)), 1);
        assert_eq!(g2.on_packet(&pkt(agree_of(Seat::P1, 4)), 2), None);
        assert_eq!(g2.state(), State::Connecting);
        for x in [&g, &g2] {
            assert_eq!(x.counters().agree_bad_mode, 1);
            assert_eq!(x.counters().agree_mismatch, 0);
            assert_eq!(x.counters().conflicts(), 0, "作り直しの印に混ぜない");
        }
        let ok = agree_mode(Seat::P1, 1, Mode::Rollback);
        assert_eq!(g2.on_packet(&pkt(ok), 3), Some(ok));
        assert!(matches!(
            g2.state(),
            State::Established(e) if e.mode == Mode::Rollback && e.delay == 1
        ));
    }

    #[test]
    fn wish_survives_a_fresh_restart_and_is_frozen_after_the_role() {
        let mut a = with_wish(cfg(900, 2, "A"), ModeWish::rollback(200));
        assert_eq!(a.wish().rollback_delay, MAX_DELAY);
        assert_eq!(
            a.expire(TIMEOUT_MS),
            State::Failed(FailReason::NothingReceived)
        );
        assert_eq!(a.restart(TIMEOUT_MS), Some(Restart::Fresh));
        assert_eq!(a.wish().mode, Mode::Rollback);
        let Some(Message::Hello(h)) = a.poll_send(TIMEOUT_MS).hello else {
            panic!("作り直した後の HELLO");
        };
        assert_eq!(h.mode, Mode::Rollback);
        a.on_packet(
            &pkt(hello_mode(100, 2, "B", Mode::Rollback)),
            TIMEOUT_MS + 1,
        );
        a.set_wish(ModeWish::DELAY);
        assert_eq!(a.wish().mode, Mode::Rollback);
        assert_eq!(
            a.poll_send(TIMEOUT_MS + 1).agree,
            Some(agree_mode(Seat::P1, MAX_DELAY, Mode::Rollback))
        );
    }

    fn seats_after(h_ask: SeatAsk, g_ask: SeatAsk) -> (Seat, Seat) {
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        h.set_seat_ask(h_ask);
        g.set_seat_ask(g_ask);
        let Some(Message::Hello(gh)) = g.poll_send(0).hello else {
            panic!("ゲストの HELLO");
        };
        let Some(Message::Hello(hh)) = h.poll_send(0).hello else {
            panic!("ホストの HELLO");
        };
        assert_eq!((gh.seat_wish, gh.arrival), (g_ask.wish, g_ask.arrival));
        assert_eq!((hh.seat_wish, hh.arrival), (h_ask.wish, h_ask.arrival));
        h.on_packet(&pkt(Message::Hello(gh)), 1);
        g.on_packet(&pkt(Message::Hello(hh)), 1);
        let agree = h.poll_send(1).agree.expect("ホストは AGREE を送る");
        let echo = g
            .on_packet(&pkt(agree), 2)
            .expect("ゲストは返す（席を確かめて通る）");
        h.on_packet(&pkt(echo), 3);
        let seat = |s: State| match s {
            State::Established(e) => e.seat,
            s => panic!("繋がっていない {s:?}"),
        };
        (seat(h.state()), seat(g.state()))
    }

    #[test]
    fn seats_follow_both_wishes_and_a_clash_goes_to_the_coin() {
        use crate::wire::{Arrival, SeatWish};
        use SeatWish::{Any, P1 as W1, P2 as W2};
        let coin = seat_pick::coin(900, 100);
        let table = [
            (W1, W1, coin),
            (W2, W2, coin),
            (W1, W2, Seat::P1),
            (W2, W1, Seat::P2),
            (W1, Any, Seat::P1),
            (W2, Any, Seat::P2),
            (Any, W1, Seat::P2),
            (Any, W2, Seat::P1),
            (Any, Any, coin),
        ];
        let ask = |wish, arrival| SeatAsk { wish, arrival };
        for (fw, sw, first) in table {
            let tag = format!("先 {fw:?} × 後 {sw:?}");
            let (hs, gs) = seats_after(ask(fw, Arrival::First), ask(sw, Arrival::Second));
            assert_eq!(
                (hs, gs),
                (first, seat_pick::other(first)),
                "{tag}（ホストが先）"
            );
            let (hs, gs) = seats_after(ask(sw, Arrival::Second), ask(fw, Arrival::First));
            assert_eq!(
                (gs, hs),
                (first, seat_pick::other(first)),
                "{tag}（ゲストが先）"
            );
        }
    }

    #[test]
    fn guest_refuses_an_agree_with_the_wrong_seat() {
        use crate::wire::{Arrival, SeatWish};
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        g.set_seat_ask(SeatAsk {
            wish: SeatWish::P2,
            arrival: Arrival::First,
        });
        let host = SeatAsk {
            wish: SeatWish::P2,
            arrival: Arrival::Second,
        };
        g.on_packet(&pkt(hello_ask(900, 2, "host", Mode::Delay, host)), 1);
        assert_eq!(g.on_packet(&pkt(agree_of(Seat::P2, 2)), 2), None);
        assert_eq!(g.counters().agree_bad_seat, 1);
        assert_eq!(g.state(), State::Connecting);
        assert_eq!(
            g.on_packet(&pkt(agree_of(Seat::P1, 2)), 3),
            Some(agree_of(Seat::P1, 2))
        );
        let State::Established(e) = g.state() else {
            panic!("繋がっていない");
        };
        assert_eq!(e.seat, Seat::P2);
        assert_eq!(g.counters().agree_bad_seat, 1);
    }

    fn nat_of(symmetry: crate::nat::Symmetry) -> NatMark {
        NatMark {
            symmetry,
            upnp: false,
        }
    }

    /// 決め方を使うHELLO（ネットワーク種別のフラグ`nat`）。
    fn hello_pick(nonce: u64, nat: NatMark) -> Message {
        let Message::Hello(mut h) = hello_of(nonce, 2, "p") else {
            unreachable!("HELLO を作った");
        };
        h.host_pick = true;
        h.nat = nat;
        Message::Hello(h)
    }

    fn agree_pick(seat: Seat, pick: Pick) -> Message {
        Message::Agree {
            host_seat: seat,
            delay: 2,
            mode: Mode::Delay,
            pick,
        }
    }

    /// ゲスト（nonce 100。ネットワーク種別はシンメトリックでない）が取りまとめ役（nonce 900。シンメトリック）の列を2 msごとに受けて測り終えた形。
    fn measured_guest() -> (Handshake, u32) {
        use crate::nat::Symmetry;
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        g.set_host_pick(Some(host_pick::Setup::new(nat_of(Symmetry::No))));
        g.on_packet(&pkt(hello_pick(900, nat_of(Symmetry::Yes))), 1);
        for s in 0..host_pick::PROBE_COUNT {
            g.on_probe(s, 10_000 + u64::from(s) * 2_000, 30);
        }
        let Some(Message::ProbeResult { up }) = g.poll_send(30).report else {
            panic!("列を締めたら値を送る");
        };
        assert_eq!(up, 600_000);
        (g, up)
    }

    #[test]
    fn guest_refuses_an_agree_whose_host_does_not_follow_from_its_values() {
        use crate::nat::Symmetry;
        let (_, up) = measured_guest();
        let (sym, open) = (nat_of(Symmetry::Yes), nat_of(Symmetry::No));
        // 正しい形: 取りまとめ役はシンメトリックなのでゲスト（受け手）がホスト。
        let good = Pick {
            host_is_sender: false,
            sender_up: up,
            receiver_up: 5_000_000,
            sender_nat: sym,
            receiver_nat: open,
        };
        assert!(good.consistent());
        // (筋, 壊した値)
        let bad = [
            (
                "ホストを入れ替えた",
                Pick {
                    host_is_sender: true,
                    ..good
                },
            ),
            (
                "取りまとめ役の回線の型が HELLO と違う",
                Pick {
                    sender_nat: open,
                    ..good
                },
            ),
            (
                "自分の回線の型が違う",
                Pick {
                    receiver_nat: sym,
                    ..good
                },
            ),
            (
                "自分が送っていない上りの値",
                Pick {
                    sender_up: up + 1,
                    ..good
                },
            ),
            ("決め方を使う接続なのに決めていない形", Pick::NONE),
        ];
        for (tag, p) in bad {
            let (mut g, _) = measured_guest();
            assert_eq!(
                g.on_packet(&pkt(agree_pick(Seat::P1, p)), 40),
                None,
                "{tag}"
            );
            assert_eq!(g.counters().agree_bad_pick, 1, "{tag}");
            assert_eq!(g.state(), State::Connecting, "{tag}");
        }
        // 効く期間: 正しい形は通り、ゲストがホストになる。サイドはホスト（自分）の希望の1P。
        let (mut g, _) = measured_guest();
        assert!(g.on_packet(&pkt(agree_pick(Seat::P1, good)), 40).is_some());
        let State::Established(e) = g.state() else {
            panic!("繋がっていない");
        };
        assert_eq!((e.role, e.seat), (Role::Host, Seat::P1));
        assert_eq!(g.counters().agree_bad_pick, 0);
        // 値を運ぶパケットが届かなかった（送り手の上り0）形でも、値から決まるホストなら通る（ここでもNATで受け手）。
        let (mut g, _) = measured_guest();
        let lost = Pick {
            sender_up: 0,
            ..good
        };
        assert!(g.on_packet(&pkt(agree_pick(Seat::P1, lost)), 40).is_some());
    }

    #[test]
    fn without_host_pick_only_the_empty_pick_passes() {
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        g.on_packet(&pkt(hello_pick(900, NatMark::UNKNOWN)), 1);
        let decided = Pick {
            host_is_sender: false,
            ..Pick::NONE
        };
        assert_eq!(g.on_packet(&pkt(agree_pick(Seat::P1, decided)), 2), None);
        assert_eq!(g.counters().agree_bad_pick, 1);
        assert!(
            g.on_packet(&pkt(agree_pick(Seat::P1, Pick::NONE)), 3)
                .is_some()
        );
        assert!(
            matches!(g.state(), State::Established(e) if e.role == Role::Guest && !e.pick.active)
        );
    }

    #[test]
    fn the_coordinator_waits_for_the_report_then_decides_once() {
        use crate::nat::Symmetry;
        let open = nat_of(Symmetry::No);
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        h.set_host_pick(Some(host_pick::Setup::new(open)));
        h.on_packet(&pkt(hello_pick(100, open)), 10);
        assert_eq!(h.poll_send(10).agree, None, "値が揃う前に AGREE を出した");
        // 相手の列（1,200 Bを10 msごと = 120,000 B/s）。
        for s in 0..host_pick::PROBE_COUNT {
            h.on_probe(s, 20_000 + u64::from(s) * 10_000, 20 + 10 * u32::from(s));
        }
        assert_eq!(h.poll_send(120).agree, None, "ゲストの値を待たずに決めた");
        h.on_packet(&pkt(Message::ProbeResult { up: 1_000_000 }), 130);
        let Some(Message::Agree {
            pick, host_seat, ..
        }) = h.poll_send(130).agree
        else {
            panic!("値が揃ったら AGREE を出す");
        };
        assert_eq!(
            pick,
            Pick {
                host_is_sender: true,
                sender_up: 1_000_000,
                receiver_up: 120_000,
                sender_nat: open,
                receiver_nat: open,
            }
        );
        assert_eq!(host_seat, Seat::P1);
        // 出した後に違う値が届いても変えない。
        h.on_packet(&pkt(Message::ProbeResult { up: 1 }), 140);
        let Some(Message::Agree { pick: again, .. }) = h.poll_send(200).agree else {
            panic!("AGREE の送り直し");
        };
        assert_eq!(again, pick);
        // 値が来なければ待つ上限で決める（受けた時刻からREPORT_WAIT_MS）。
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        h.set_host_pick(Some(host_pick::Setup::new(open)));
        h.on_packet(&pkt(hello_pick(100, open)), 10);
        assert_eq!(h.poll_send(10 + host_pick::REPORT_WAIT_MS - 1).agree, None);
        let Some(Message::Agree { pick, .. }) = h.poll_send(10 + host_pick::REPORT_WAIT_MS).agree
        else {
            panic!("待つ上限で決める");
        };
        assert_eq!(
            (pick.host_is_sender, pick.sender_up, pick.receiver_up),
            (true, 0, 0)
        );
    }

    /// 取りまとめ役が値を決めた後にAGREEが落ち、両者が時間切れで作り直した。
    /// ゲストは食い違い（別のnonceのHELLO）を見たので最初から作り直し（Fresh）、取りまとめ役は値を引き継ぐ（Resume）。
    /// ゲストは測り直す前でも、前の値で決めたAGREEを採って接続する。
    /// ゲストは作り直した後も決め方を名乗る。
    #[test]
    fn a_fresh_guest_accepts_the_values_a_resumed_coordinator_kept() {
        use crate::nat::Symmetry;
        let open = nat_of(Symmetry::No);
        let setup = Some(host_pick::Setup::new(open));
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        h.set_host_pick(setup);
        g.set_host_pick(setup);
        let hh = h.poll_send(0).hello.expect("HELLO");
        let gh = g.poll_send(0).hello.expect("HELLO");
        h.on_packet(&pkt(gh), 1);
        g.on_packet(&pkt(hh), 1);
        for s in 0..host_pick::PROBE_COUNT {
            h.on_probe(s, 2_000 + u64::from(s) * 1_000, 11);
            g.on_probe(s, 2_000 + u64::from(s) * 3_000, 29);
        }
        let report = g.poll_send(30).report.expect("値の便");
        h.on_packet(&pkt(report), 31);
        let agree = h.poll_send(31).agree.expect("AGREE");
        let Message::Agree { pick, .. } = agree else {
            unreachable!("AGREE");
        };
        assert!(
            pick.sender_up > 0,
            "効く期間: AGREE にゲストの値が載っている"
        );
        // AGREEは落ちる。ゲストは別のnonceのHELLOを受ける（食い違い）。
        g.on_packet(&pkt(hello_pick(555, open)), 40);
        let t = TIMEOUT_MS + 1;
        h.expire(t);
        g.expire(t);
        assert_eq!(h.restart(t), Some(Restart::Resume));
        assert_eq!(g.restart(t), Some(Restart::Fresh));
        let Some(Message::Hello(again)) = g.poll_send(t).hello else {
            panic!("作り直した後の HELLO");
        };
        assert!(again.host_pick, "作り直したら決め方を名乗らなくなった");
        let hh = h.poll_send(t).hello.expect("Resume の HELLO");
        g.on_packet(&pkt(hh), t + 1);
        let agree2 = h.poll_send(t + 4).agree.expect("AGREE の送り直し");
        assert_eq!(agree2, agree, "Resume で値が変わった");
        let echo = g
            .on_packet(&pkt(agree2), t + 5)
            .expect("前の値で決めた AGREE を断った");
        h.on_packet(&pkt(echo), t + 6);
        let (State::Established(eh), State::Established(eg)) = (h.state(), g.state()) else {
            panic!("接続しない: {:?} / {:?}", h.state(), g.state());
        };
        assert_ne!(eh.role, eg.role);
        assert!(eg.pick.active && eh.pick.active);
        assert_eq!(g.counters().agree_bad_pick, 0);
    }

    /// 同じnonceで公開鍵だけ違うHELLO（相手がセッションを作り直した）で、列・測った値・PROBEを送ったかどうかを捨てる。
    #[test]
    fn a_rekeyed_peer_starts_the_probe_train_over() {
        use crate::nat::Symmetry;
        let open = nat_of(Symmetry::No);
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        g.set_host_pick(Some(host_pick::Setup::new(open)));
        let first = hello_pick(900, open);
        g.on_packet(&pkt(first), 1);
        assert!(g.take_probe(), "効く期間: 最初の相手へ送る");
        assert!(!g.take_probe(), "2 回目は送らない");
        for s in 0..host_pick::PROBE_COUNT {
            g.on_probe(s, 2_000 + u64::from(s) * 1_000, 12);
        }
        assert!(g.poll_send(12).report.is_some(), "効く期間: 測り終えた");
        let Message::Hello(mut h2) = hello_pick(900, open) else {
            unreachable!("HELLO");
        };
        h2.public = [0x5A; PUBLIC_LEN];
        g.on_packet(&pkt(Message::Hello(h2)), 20);
        assert_eq!(g.counters().rekeyed, 1);
        assert!(g.take_probe(), "作り直した相手へ PROBE を送り直さない");
        assert_eq!(g.poll_send(100).report, None, "前の相手の値を送り続けた");
    }

    #[test]
    fn probes_count_only_while_connecting_and_using_host_pick() {
        use crate::nat::Symmetry;
        let open = nat_of(Symmetry::No);
        // 決め方を使わない: 数えない。
        let mut off = Handshake::new(cfg(100, 2, "guest"), 0);
        off.on_packet(&pkt(hello_pick(900, open)), 1);
        off.on_probe(0, 1_000, 1);
        off.on_probe(1, 2_000, 2);
        assert_eq!(off.counters().probes, 0);
        // 使う: 接続する前は数え、接続した後は数えない。
        let (mut g, _) = measured_guest();
        assert_eq!(g.counters().probes, u32::from(host_pick::PROBE_COUNT));
        let good = Pick {
            host_is_sender: false,
            sender_up: 0,
            receiver_up: 5_000_000,
            sender_nat: nat_of(Symmetry::Yes),
            receiver_nat: open,
        };
        assert!(g.on_packet(&pkt(agree_pick(Seat::P1, good)), 40).is_some());
        assert!(matches!(g.state(), State::Established(_)));
        let mut late = Handshake::new(cfg(100, 2, "guest"), 0);
        late.set_host_pick(Some(host_pick::Setup::new(open)));
        late.on_packet(&pkt(hello_pick(900, nat_of(Symmetry::Yes))), 1);
        late.on_packet(
            &pkt(agree_pick(
                Seat::P1,
                Pick {
                    sender_up: 0,
                    ..good
                },
            )),
            2,
        );
        assert!(
            matches!(late.state(), State::Established(_)),
            "効く期間: 接続した"
        );
        late.on_probe(0, 3_000, 3);
        late.on_probe(1, 4_000, 4);
        assert_eq!(late.counters().probes, 0, "接続した後の PROBE を数えた");
    }

    #[test]
    fn seat_ask_survives_a_fresh_restart_and_is_frozen_after_the_role() {
        use crate::wire::{Arrival, SeatWish};
        let plain = Handshake::new(cfg_seat(900, 2, "A", Seat::P2), 0);
        assert_eq!(plain.seat_ask(), SeatAsk::plain(Seat::P2));
        let any_first = SeatAsk {
            wish: SeatWish::Any,
            arrival: Arrival::First,
        };
        let mut a = Handshake::new(cfg_seat(900, 2, "A", Seat::P2), 0);
        a.set_seat_ask(any_first);
        assert_eq!(
            a.expire(TIMEOUT_MS),
            State::Failed(FailReason::NothingReceived)
        );
        assert_eq!(a.restart(TIMEOUT_MS), Some(Restart::Fresh));
        assert_eq!(a.seat_ask(), any_first);
        let Some(Message::Hello(h)) = a.poll_send(TIMEOUT_MS).hello else {
            panic!("作り直した後の HELLO");
        };
        assert_eq!((h.seat_wish, h.arrival), (SeatWish::Any, Arrival::First));
        let peer = SeatAsk {
            wish: SeatWish::P2,
            arrival: Arrival::Second,
        };
        a.on_packet(
            &pkt(hello_ask(100, 2, "B", Mode::Delay, peer)),
            TIMEOUT_MS + 1,
        );
        a.set_seat_ask(SeatAsk::plain(Seat::P2));
        assert_eq!(a.seat_ask(), any_first);
        assert_eq!(
            a.poll_send(TIMEOUT_MS + 1).agree,
            Some(agree_of(Seat::P1, 2))
        );
    }

    #[test]
    fn seat_ask_is_frozen_once_a_hello_is_out() {
        use crate::wire::{Arrival, SeatWish};
        let any = SeatAsk {
            wish: SeatWish::Any,
            arrival: Arrival::First,
        };
        let mut a = Handshake::new(cfg(900, 2, "A"), 0);
        a.set_seat_ask(any);
        assert_eq!(a.seat_ask(), any);
        assert!(a.poll_send(0).hello.is_some());
        a.set_seat_ask(SeatAsk::plain(Seat::P2));
        assert_eq!(a.seat_ask(), any, "HELLO を出した後は変えない");
    }

    #[test]
    fn name_truncates_at_char_boundary() {
        let s = "あああああああああああ";
        let n = Name::new(s);
        assert_eq!(n.as_bytes().len(), 30);
        assert_eq!(n.as_str(), "ああああああああああ");
        let s32 = "abcdefghijklmnopqrstuvwxyz012345";
        assert_eq!(Name::new(s32).as_str(), s32);
        assert_eq!(Name::new("").as_bytes(), b"");
        // 詰めた名前は相手のdecodeを通る（途中で切るとUTF-8でなくなりHELLOごと捨てられる）。
        let p = pkt(Handshake::new(cfg(1, 0, s), 0).hello());
        assert!(matches!(p.msg, Message::Hello(h) if h.name_len == 30));
    }

    #[test]
    fn hello_every_20ms_until_ack() {
        let mut a = Handshake::new(cfg(100, 3, "A"), 1000);
        let mut sent = Vec::new();
        for t in 1000..1070 {
            if t == 1045 {
                assert!(a.on_packet(&pkt(hello_of(7, 3, "B")), t).is_some());
                let r = a.on_packet(
                    &pkt(Message::HelloAck {
                        echo_nonce: 100,
                        role_nonce: 7,
                        public: [0; PUBLIC_LEN],
                        echo_public: [0; PUBLIC_LEN],
                    }),
                    t,
                );
                assert_eq!(r, None, "HELLO_ACK には返さない");
            }
            if let Some(m) = a.poll_send(t).hello {
                assert!(matches!(m, Message::Hello(h) if h.role_nonce == 100 && h.latency == 3));
                sent.push(t);
            }
        }
        assert_eq!(sent, vec![1000, 1020, 1040]);
    }

    #[test]
    fn hello_sent_at_once_on_peer_hello_while_unacked() {
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        assert!(a.poll_send(0).hello.is_some());
        assert!(a.on_packet(&pkt(hello_of(100, 2, "B")), 5).is_some());
        assert!(a.poll_send(5).hello.is_some(), "未 ack なら即送り");
        assert!(a.poll_send(6).hello.is_none());
        assert!(a.poll_send(24).hello.is_none());
        a.on_packet(&pkt(hello_of(100, 2, "B")), 8);
        assert!(a.poll_send(8).hello.is_some());
        assert_eq!(a.on_packet(&pkt(hello_of(9999, 2, "C")), 10), None);
        assert!(
            a.poll_send(10).hello.is_none(),
            "stale の HELLO で即送りした"
        );
        a.on_packet(
            &pkt(Message::HelloAck {
                echo_nonce: 500,
                role_nonce: 100,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            }),
            11,
        );
        assert!(a.on_packet(&pkt(hello_of(100, 2, "B")), 12).is_some());
        assert!(a.poll_send(12).hello.is_none(), "ack 済みで即送りした");
        assert!(a.poll_send(40).hello.is_none());
        let mut s = Handshake::new(cfg(7, 2, "S"), 0);
        assert!(s.poll_send(0).hello.is_some());
        s.on_packet(&pkt(hello_of(7, 2, "S")), 3);
        assert_eq!(s.poll_send(3), Outbox::default());
        let mut g = Handshake::new(cfg(1, 2, "G"), 0);
        assert!(g.poll_send(0).hello.is_some());
        g.on_packet(&pkt(hello_of(900, 2, "H")), 1);
        assert_eq!(g.coordinator(), Some(false));
        assert!(g.poll_send(1).hello.is_some());
    }

    #[test]
    fn host_seat_follows_hosts_config_not_guests() {
        let mut h = Handshake::new(cfg_seat(900, 2, "host", Seat::P2), 0);
        h.on_packet(&pkt(hello_of(100, 2, "guest")), 5);
        assert_eq!(h.coordinator(), Some(true));
        let agree = h.poll_send(5).agree.expect("ホストは AGREE を送る");
        assert_eq!(
            agree,
            agree_of(Seat::P2, 2),
            "ホストの host_seat の設定が送られていない"
        );
        assert_eq!(
            h.on_packet(&pkt(agree_of(Seat::P2, 2)), 6),
            None,
            "同じ値の返しで繋がる"
        );
        let State::Established(eh) = h.state() else {
            panic!("ホストが繋がっていない: {:?}", h.state());
        };
        assert_eq!(eh.host_seat, Seat::P2);
        assert_eq!(
            eh.seat,
            Seat::P2,
            "ホストの席は自分の host_seat の設定と同じ"
        );

        let mut g = Handshake::new(cfg_seat(100, 2, "guest", Seat::P2), 0);
        g.on_packet(&pkt(hello_of(900, 2, "host")), 5);
        assert_eq!(g.coordinator(), Some(false));
        let reply = g
            .on_packet(&pkt(agree_of(Seat::P1, 2)), 6)
            .expect("ゲストは AGREE に返す");
        assert_eq!(reply, agree_of(Seat::P1, 2));
        let State::Established(eg) = g.state() else {
            panic!("ゲストが繋がっていない: {:?}", g.state());
        };
        assert_eq!(
            eg.host_seat,
            Seat::P1,
            "ゲストは自分の host_seat の設定（P2）ではなく届いた値（P1）を使う"
        );
        assert_eq!(
            eg.seat,
            Seat::P2,
            "ゲストの席は届いた host_seat の逆（自分の設定は使わない）"
        );
    }

    #[test]
    fn host_decides_and_waits_for_matching_echo() {
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        let r = h.on_packet(&pkt(hello_of(100, 7, "guest")), 5);
        assert_eq!(
            r,
            Some(Message::HelloAck {
                echo_nonce: 100,
                role_nonce: 900,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            })
        );
        assert_eq!(h.coordinator(), Some(true));
        let mut agrees = Vec::new();
        for t in 5..20 {
            if let Some(m) = h.poll_send(t).agree {
                assert_eq!(m, agree_of(Seat::P1, 7), "delay は希望の大きい方（7 > 2）");
                agrees.push(t);
            }
        }
        assert_eq!(agrees, vec![5, 9, 13, 17]);
        assert_eq!(h.on_packet(&pkt(agree_of(Seat::P1, 2)), 20), None);
        assert_eq!(h.on_packet(&pkt(agree_of(Seat::P2, 7)), 20), None);
        assert_eq!(h.state(), State::Connecting);
        assert_eq!(h.counters().agree_mismatch, 2);
        assert_eq!(h.on_packet(&pkt(agree_of(Seat::P1, 7)), 21), None);
        let State::Established(e) = h.state() else {
            panic!("繋がっていない: {:?}", h.state());
        };
        assert_eq!(e.role, Role::Host);
        assert_eq!(e.seat, Seat::P1);
        assert_eq!(e.host_seat, Seat::P1);
        assert_eq!(e.delay, 7);
        assert_eq!(e.peer_name.as_str(), "guest");
        assert_eq!(e.own_name.as_str(), "host");
        assert_eq!(e.peer_latency, 7);
        for t in 21..60 {
            assert_eq!(h.poll_send(t).agree, None);
        }
    }

    #[test]
    fn guest_drops_agree_before_hello_then_accepts() {
        let mut g = Handshake::new(cfg(100, 4, "guest"), 0);
        assert_eq!(g.on_packet(&pkt(agree_of(Seat::P1, 6)), 3), None);
        assert_eq!(g.counters().agree_early, 1);
        assert_eq!(g.state(), State::Connecting);
        assert_eq!(g.coordinator(), None);
        assert!(g.on_packet(&pkt(hello_of(900, 6, "host")), 4).is_some());
        assert_eq!(g.coordinator(), Some(false));
        for t in 4..30 {
            assert_eq!(g.poll_send(t).agree, None);
        }
        assert_eq!(
            g.on_packet(&pkt(agree_of(Seat::P1, 6)), 30),
            Some(agree_of(Seat::P1, 6))
        );
        let State::Established(e) = g.state() else {
            panic!("繋がっていない: {:?}", g.state());
        };
        assert_eq!(e.role, Role::Guest);
        assert_eq!(e.seat, Seat::P2);
        assert_eq!(e.host_seat, Seat::P1);
        assert_eq!(e.delay, 6);
        assert_eq!(e.peer_name.as_str(), "host");
        assert_eq!(e.own_name.as_str(), "guest");
        assert_eq!(e.peer_latency, 6);
    }

    #[test]
    fn guest_drops_agree_with_other_values() {
        let mut g = Handshake::new(cfg(100, 4, "guest"), 0);
        g.on_packet(&pkt(hello_of(900, 6, "host")), 1);
        assert!(g.on_packet(&pkt(agree_of(Seat::P1, 6)), 2).is_some());
        let before = g.state();
        assert_eq!(g.on_packet(&pkt(agree_of(Seat::P1, 5)), 3), None);
        assert_eq!(g.on_packet(&pkt(agree_of(Seat::P2, 6)), 4), None);
        // サイドの違うAGREEは、最初の値と比べる前に「両者の希望から決まるサイドと違う」（`agree_bad_seat`）で落ちる。
        // どちらで落ちても返さず・値を変えないのは同じ。
        assert_eq!(g.counters().agree_mismatch, 1);
        assert_eq!(g.counters().agree_bad_seat, 1);
        assert_eq!(g.state(), before);
        assert_eq!(
            g.on_packet(&pkt(agree_of(Seat::P1, 6)), 5),
            Some(agree_of(Seat::P1, 6))
        );
    }

    #[test]
    fn replies_continue_after_established() {
        let mut g = Handshake::new(cfg(100, 1, "guest"), 0);
        g.on_packet(&pkt(hello_of(900, 1, "host")), 1);
        g.on_packet(&pkt(agree_of(Seat::P1, 1)), 2);
        assert!(matches!(g.state(), State::Established(_)));
        for t in [3, 500, 20_000] {
            assert_eq!(
                g.on_packet(&pkt(hello_of(900, 1, "host")), t),
                Some(Message::HelloAck {
                    echo_nonce: 900,
                    role_nonce: 100,
                    public: [0; PUBLIC_LEN],
                    echo_public: [0; PUBLIC_LEN],
                })
            );
            assert_eq!(
                g.on_packet(&pkt(agree_of(Seat::P1, 1)), t),
                Some(agree_of(Seat::P1, 1))
            );
        }
        let mut h = Handshake::new(cfg(900, 1, "host"), 0);
        h.on_packet(&pkt(hello_of(100, 1, "guest")), 1);
        h.on_packet(&pkt(agree_of(Seat::P1, 1)), 2);
        assert!(matches!(h.state(), State::Established(_)));
        assert!(
            h.on_packet(&pkt(hello_of(100, 1, "guest")), 20_000)
                .is_some()
        );
        assert!(
            matches!(h.state(), State::Established(_)),
            "繋がった後は時間切れにしない"
        );
    }

    #[test]
    fn drops_wrong_session_and_stale_nonce() {
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        assert_eq!(
            a.on_packet(&pkt_sid(SID ^ 1, hello_of(100, 2, "X")), 1),
            None
        );
        assert_eq!(
            a.on_packet(
                &pkt_sid(
                    SID ^ 1,
                    Message::HelloAck {
                        echo_nonce: 500,
                        role_nonce: 100,
                        public: [0; PUBLIC_LEN],
                        echo_public: [0; PUBLIC_LEN],
                    }
                ),
                1
            ),
            None
        );
        assert_eq!(
            a.on_packet(&pkt_sid(SID ^ 1, agree_of(Seat::P1, 2)), 1),
            None
        );
        assert_eq!(a.counters().wrong_session, 3);
        assert_eq!(a.coordinator(), None);
        assert!(a.on_packet(&pkt(hello_of(100, 2, "B")), 2).is_some());
        assert_eq!(a.coordinator(), Some(true));
        assert_eq!(a.on_packet(&pkt(hello_of(9999, 2, "C")), 3), None);
        assert_eq!(a.coordinator(), Some(true));
        a.on_packet(
            &pkt(Message::HelloAck {
                echo_nonce: 500,
                role_nonce: 9999,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            }),
            4,
        );
        a.on_packet(
            &pkt(Message::HelloAck {
                echo_nonce: 501,
                role_nonce: 100,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            }),
            4,
        );
        assert_eq!(a.counters().stale_nonce, 2);
        assert_eq!(a.counters().wrong_echo, 1);
        assert!(a.poll_send(40).hello.is_some(), "HELLO を続けている");
        a.on_packet(
            &pkt(Message::HelloAck {
                echo_nonce: 500,
                role_nonce: 100,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            }),
            41,
        );
        assert!(a.poll_send(100).hello.is_none());
        let mut b = Handshake::new(cfg(1, 2, "B"), 0);
        b.on_packet(&pkt_sid(SID ^ 1, hello_of(100, 2, "X")), 1);
        b.poll_send(TIMEOUT_MS);
        assert_eq!(b.state(), State::Failed(FailReason::NothingReceived));
    }

    fn hello_pub(nonce: u64, latency: u8, name: &str, public: [u8; PUBLIC_LEN]) -> Message {
        let Message::Hello(mut h) = hello_of(nonce, latency, name) else {
            unreachable!("HELLO を作った");
        };
        h.public = public;
        Message::Hello(h)
    }

    #[test]
    fn a_new_public_key_with_the_same_nonce_replaces_the_peer_before_connecting() {
        let (p1, p2, p3) = ([1u8; PUBLIC_LEN], [2u8; PUBLIC_LEN], [3u8; PUBLIC_LEN]);
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        a.set_public([0xA0; PUBLIC_LEN]);
        assert!(a.on_packet(&pkt(hello_pub(100, 2, "B1", p1)), 2).is_some());
        assert_eq!(a.peer_key(), Some((100, p1)));
        a.on_packet(
            &pkt(Message::HelloAck {
                echo_nonce: 500,
                role_nonce: 100,
                public: p1,
                echo_public: [0xA0; PUBLIC_LEN],
            }),
            3,
        );
        assert!(
            a.poll_send(40).hello.is_none(),
            "ack の後は HELLO を送らない"
        );
        let r = a.on_packet(&pkt(hello_pub(100, 5, "B2", p2)), 50);
        assert!(
            matches!(
                r,
                Some(Message::HelloAck {
                    echo_nonce: 100,
                    ..
                })
            ),
            "{r:?}"
        );
        assert_eq!(a.peer_key(), Some((100, p2)));
        let c = a.counters();
        assert_eq!((c.rekeyed, c.stale_nonce, c.retired_hello), (1, 0, 0));
        assert_eq!(a.coordinator(), Some(true));
        let o = a.poll_send(51);
        assert!(o.hello.is_some(), "新しいセッションへ HELLO を出し直さない");
        assert_eq!(
            o.agree,
            Some(agree_of(Seat::P1, 5)),
            "古い HELLO の遅延のまま"
        );
        assert_eq!(a.on_packet(&pkt(hello_pub(100, 2, "B1", p1)), 52), None);
        assert_eq!(a.peer_key(), Some((100, p2)));
        let c = a.counters();
        assert_eq!((c.rekeyed, c.stale_nonce, c.retired_hello), (1, 0, 1));
        assert!(a.on_packet(&pkt(hello_pub(100, 5, "B3", p3)), 60).is_some());
        assert_eq!(a.peer_key(), Some((100, p3)));
        for old in [p1, p2] {
            assert_eq!(a.on_packet(&pkt(hello_pub(100, 5, "B", old)), 61), None);
        }
        assert_eq!(a.peer_key(), Some((100, p3)));
        assert_eq!(a.counters().rekeyed, 2);
        assert_eq!(a.counters().retired_hello, 3);
        a.expire(TIMEOUT_MS);
        assert_eq!(a.restart(TIMEOUT_MS + 1), Some(Restart::Resume));
        assert_eq!(a.peer_key(), Some((100, p3)));
        a.on_packet(&pkt(agree_of(Seat::P1, 5)), TIMEOUT_MS + 2);
        assert!(
            matches!(a.state(), State::Established(_)),
            "{:?}",
            a.state()
        );
        assert_eq!(
            a.on_packet(
                &pkt(hello_pub(100, 5, "B4", [4; PUBLIC_LEN])),
                TIMEOUT_MS + 3
            ),
            None
        );
        assert_eq!(a.peer_key(), Some((100, p3)));
        assert_eq!(a.counters().stale_nonce, 1);
        assert_eq!(a.counters().rekeyed, 2);
    }

    #[test]
    fn only_a_new_key_under_the_same_nonce_replaces_the_peer() {
        let p1 = [1u8; PUBLIC_LEN];
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        assert!(a.on_packet(&pkt(hello_pub(100, 2, "B", p1)), 2).is_some());
        for t in 3..8 {
            assert!(a.on_packet(&pkt(hello_pub(100, 2, "B", p1)), t).is_some());
        }
        assert_eq!(
            a.on_packet(&pkt(hello_pub(101, 2, "C", [9; PUBLIC_LEN])), 9),
            None
        );
        assert_eq!(a.peer_key(), Some((100, p1)));
        let c = a.counters();
        assert_eq!((c.rekeyed, c.stale_nonce, c.retired_hello), (0, 1, 0));
    }

    /// 今のセッションの自分のHELLOへの返答（自分のnonceと公開鍵`mine`を返す）。
    fn fresh_ack(
        echo: u64,
        mine: [u8; PUBLIC_LEN],
        their: u64,
        public: [u8; PUBLIC_LEN],
    ) -> Message {
        Message::HelloAck {
            echo_nonce: echo,
            role_nonce: their,
            public,
            echo_public: mine,
        }
    }

    #[test]
    fn an_earlier_sessions_hello_that_arrived_first_gives_way_to_the_peer_that_answers_the_current_hello()
     {
        let me = [0xA0u8; PUBLIC_LEN];
        let (old, live) = ([1u8; PUBLIC_LEN], [2u8; PUBLIC_LEN]);
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        a.set_public(me);
        assert!(
            a.on_packet(&pkt(hello_pub(100, 2, "old", old)), 1)
                .is_some()
        );
        assert_eq!(a.peer_key(), Some((100, old)));
        assert_eq!(a.on_packet(&pkt(hello_pub(101, 4, "live", live)), 2), None);
        assert!(
            a.poll_send(25).hello.is_some(),
            "古い相手の返しが無いのに HELLO を止めた"
        );
        a.on_packet(&pkt(fresh_ack(500, me, 101, live)), 26);
        assert_eq!(
            a.peer_key(),
            Some((100, old)),
            "HELLO を受ける前に取り替えた"
        );
        let r = a.on_packet(&pkt(hello_pub(101, 4, "live", live)), 27);
        assert!(
            matches!(
                r,
                Some(Message::HelloAck {
                    echo_nonce: 101,
                    ..
                })
            ),
            "{r:?}"
        );
        assert_eq!(a.peer_key(), Some((101, live)));
        assert_eq!(a.counters().swapped, 1);
        assert!(
            a.poll_send(60).hello.is_none(),
            "返しを受けた相手なのに HELLO を続けた"
        );
        assert_eq!(a.on_packet(&pkt(hello_pub(100, 2, "old", old)), 61), None);
        assert_eq!(a.peer_key(), Some((101, live)));
        assert_eq!(a.counters().retired_hello, 1);
        a.on_packet(&pkt(agree_of(Seat::P1, 4)), 62);
        assert!(matches!(a.state(), State::Established(e) if e.peer_name.as_str() == "live"));
    }

    #[test]
    fn an_old_key_replayed_after_the_live_one_is_undone_by_the_live_peers_answer() {
        let me = [0xA0u8; PUBLIC_LEN];
        let (old, live) = ([1u8; PUBLIC_LEN], [2u8; PUBLIC_LEN]);
        let mut g = Handshake::new(cfg(100, 2, "G"), 0);
        g.set_public(me);
        g.on_packet(&pkt(hello_pub(900, 2, "live", live)), 1);
        g.on_packet(&pkt(fresh_ack(100, me, 900, live)), 2);
        assert!(
            g.on_packet(&pkt(hello_pub(900, 2, "old", old)), 3)
                .is_some()
        );
        assert_eq!(
            g.peer_key(),
            Some((900, old)),
            "同じ nonce の新しい鍵は今までどおり覚え直す"
        );
        assert!(
            g.poll_send(3).hello.is_some(),
            "覚え直した相手へ HELLO を出し直さない"
        );
        g.on_packet(&pkt(fresh_ack(100, me, 900, live)), 4);
        assert_eq!(g.peer_key(), Some((900, live)), "今の相手の返しで戻さない");
        assert_eq!(g.counters().swapped, 1);
        for t in 5..9 {
            assert_eq!(g.on_packet(&pkt(hello_pub(900, 2, "old", old)), t), None);
        }
        assert_eq!(g.peer_key(), Some((900, live)));
        assert_eq!(
            g.on_packet(&pkt(agree_of(Seat::P1, 2)), 10),
            Some(agree_of(Seat::P1, 2))
        );
        assert!(matches!(g.state(), State::Established(e) if e.peer_name.as_str() == "live"));
    }

    #[test]
    fn an_answer_to_an_earlier_session_key_neither_stops_hello_nor_picks_a_peer() {
        let me = [0xA0u8; PUBLIC_LEN];
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        a.set_public(me);
        a.on_packet(
            &pkt(fresh_ack(500, [0xA1; PUBLIC_LEN], 100, [1; PUBLIC_LEN])),
            1,
        );
        assert_eq!(a.counters().wrong_echo, 1);
        assert!(a.poll_send(1).hello.is_some());
        assert!(
            a.on_packet(&pkt(hello_pub(101, 2, "C", [3; PUBLIC_LEN])), 2)
                .is_some()
        );
        assert_eq!(a.peer_key(), Some((101, [3; PUBLIC_LEN])));
    }

    #[test]
    fn an_answer_that_arrives_before_the_peers_hello_does_not_stop_hello() {
        let me = [0xA0u8; PUBLIC_LEN];
        let (k1, k2) = ([1u8; PUBLIC_LEN], [2u8; PUBLIC_LEN]);
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        a.set_public(me);
        assert!(a.poll_send(0).hello.is_some());
        // B1の返答だけが届き、B1のHELLOは落ちた。その後Bが取りやめて同じnonce・新しい鍵で始め直した。
        a.on_packet(&pkt(fresh_ack(500, me, 100, k1)), 5);
        assert!(
            a.poll_send(20).hello.is_some(),
            "相手が決まる前に HELLO を止めた"
        );
        assert_eq!(a.on_packet(&pkt(hello_pub(100, 2, "B2", k2)), 21), None);
        assert_eq!(a.peer_key(), None);
        assert!(a.poll_send(40).hello.is_some());
        a.on_packet(&pkt(fresh_ack(500, me, 100, k2)), 41);
        assert!(a.on_packet(&pkt(hello_pub(100, 2, "B2", k2)), 42).is_some());
        assert_eq!(a.peer_key(), Some((100, k2)));
        let o = a.poll_send(60);
        assert!(o.hello.is_none(), "返した相手が決まった後も HELLO を続けた");
        let agree = o.agree.expect("取りまとめ役は AGREE を送る");
        a.on_packet(&pkt(agree), 61);
        assert!(matches!(a.state(), State::Established(e) if e.peer_name.as_str() == "B2"));
    }

    #[test]
    fn while_waiting_for_the_peer_that_answered_no_other_hello_is_taken() {
        let me = [0u8; PUBLIC_LEN];
        let (k, other) = ([5u8; PUBLIC_LEN], [6u8; PUBLIC_LEN]);
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        a.on_packet(&pkt(fresh_ack(500, me, 100, k)), 1);
        assert_eq!(a.on_packet(&pkt(hello_pub(101, 2, "X", other)), 2), None);
        assert_eq!(a.on_packet(&pkt(hello_pub(100, 2, "X", other)), 3), None);
        assert_eq!(a.peer_key(), None);
        assert!(a.on_packet(&pkt(hello_pub(100, 2, "K", k)), 4).is_some());
        assert_eq!(a.peer_key(), Some((100, k)));
        let mut b = Handshake::new(cfg(500, 2, "B"), 0);
        b.on_packet(&pkt(hello_pub(100, 2, "old", [1; PUBLIC_LEN])), 1);
        b.on_packet(&pkt(fresh_ack(500, me, 101, k)), 2);
        assert_eq!(
            b.on_packet(&pkt(hello_pub(100, 2, "old2", other)), 3),
            None,
            "返しをくれた相手を待つ間に、同じ nonce の別の鍵へ覚え直した"
        );
        assert_eq!(b.peer_key(), Some((100, [1; PUBLIC_LEN])));
    }

    #[test]
    fn timeout_at_10s_and_silent_after() {
        let mut a = Handshake::new(cfg(500, 2, "A"), 50);
        a.poll_send(50 + TIMEOUT_MS - 1);
        assert_eq!(a.state(), State::Connecting);
        let o = a.poll_send(50 + TIMEOUT_MS);
        assert_eq!(o, Outbox::default());
        assert_eq!(a.state(), State::Failed(FailReason::NothingReceived));
        assert_eq!(
            a.on_packet(&pkt(hello_of(100, 2, "B")), 50 + TIMEOUT_MS),
            None
        );
        assert_eq!(a.counters().after_fail, 1);
        let mut b = Handshake::new(cfg(500, 2, "A"), u32::MAX - 100); // 時計の折り返しをまたぐ
        b.on_packet(&pkt(hello_of(100, 2, "B")), u32::MAX - 50);
        assert_eq!(
            b.on_packet(
                &pkt(hello_of(100, 2, "B")),
                (u32::MAX - 101).wrapping_add(TIMEOUT_MS)
            ),
            Some(Message::HelloAck {
                echo_nonce: 100,
                role_nonce: 500,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            })
        );
        assert_eq!(
            b.on_packet(
                &pkt(hello_of(100, 2, "B")),
                (u32::MAX - 100).wrapping_add(TIMEOUT_MS)
            ),
            None
        );
        assert_eq!(b.state(), State::Failed(FailReason::NoHandshake));
        assert_eq!(b.poll_send(u32::MAX), Outbox::default());
    }

    #[test]
    fn restart_keeps_the_peer_unless_it_changed() {
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        assert_eq!(a.restart(5), None, "手順の中で作り直した");
        assert_eq!(
            a.expire(TIMEOUT_MS),
            State::Failed(FailReason::NothingReceived)
        );
        assert_eq!(a.restart(TIMEOUT_MS), Some(Restart::Fresh));
        assert_eq!(a.state(), State::Connecting);
        assert!(a.poll_send(TIMEOUT_MS).hello.is_some(), "すぐ HELLO");
        assert_eq!(
            a.expire(2 * TIMEOUT_MS - 1),
            State::Connecting,
            "数え始めが今でない"
        );
        assert_eq!(
            a.expire(2 * TIMEOUT_MS),
            State::Failed(FailReason::NothingReceived)
        );
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        h.on_packet(&pkt(hello_of(100, 7, "guest")), 5);
        h.on_packet(
            &pkt(Message::HelloAck {
                echo_nonce: 900,
                role_nonce: 100,
                public: [0; PUBLIC_LEN],
                echo_public: [0; PUBLIC_LEN],
            }),
            6,
        );
        assert!(
            h.poll_send(30).hello.is_none(),
            "ack の後は HELLO を送らない"
        );
        h.expire(TIMEOUT_MS);
        assert_eq!(h.state(), State::Failed(FailReason::NoHandshake));
        let before = h.counters();
        assert_eq!(h.restart(TIMEOUT_MS + 1), Some(Restart::Resume));
        assert_eq!(h.coordinator(), Some(true));
        assert_eq!(h.counters(), before, "数えを 0 に戻した");
        let o = h.poll_send(TIMEOUT_MS + 1);
        assert!(o.hello.is_some(), "引き継いだのに HELLO を送り直さない");
        assert_eq!(
            o.agree,
            Some(agree_of(Seat::P1, 7)),
            "ホストの AGREE（前の回と同じ値）"
        );
        h.on_packet(&pkt(agree_of(Seat::P1, 7)), TIMEOUT_MS + 2);
        assert!(matches!(h.state(), State::Established(e) if e.peer_name.as_str() == "guest"));
        assert_eq!(h.restart(TIMEOUT_MS + 3), None, "繋がった後に作り直した");
        let mut g = Handshake::new(cfg(100, 2, "guest"), 0);
        g.on_packet(&pkt(hello_of(900, 2, "old")), 5);
        g.on_packet(&pkt(hello_of(901, 2, "new")), 6);
        assert_eq!(g.counters().stale_nonce, 1);
        g.expire(TIMEOUT_MS);
        assert_eq!(g.restart(TIMEOUT_MS), Some(Restart::Fresh));
        assert_eq!(g.coordinator(), None, "食い違いがあったのに引き継いだ");
        assert!(
            g.on_packet(&pkt(hello_of(901, 2, "new")), TIMEOUT_MS + 1)
                .is_some()
        );
        assert_eq!(g.coordinator(), Some(false));
        g.expire(2 * TIMEOUT_MS);
        assert_eq!(
            g.restart(2 * TIMEOUT_MS),
            Some(Restart::Resume),
            "前の回の食い違いで引き継がない"
        );
        let mut h = Handshake::new(cfg(900, 2, "host"), 0);
        h.on_packet(&pkt(hello_of(100, 2, "guest")), 5);
        h.on_packet(&pkt(agree_of(Seat::P2, 2)), 6);
        assert_eq!(h.counters().agree_mismatch, 1);
        h.expire(TIMEOUT_MS);
        assert_eq!(h.restart(TIMEOUT_MS), Some(Restart::Fresh));
        assert_eq!(h.coordinator(), None);
        let mut s = Handshake::new(cfg(7, 2, "S"), 0);
        s.on_packet(&pkt(hello_of(7, 2, "S")), 1);
        assert_eq!(s.state(), State::Failed(FailReason::SameNonce));
        assert_eq!(s.restart(TIMEOUT_MS), None);
    }

    #[test]
    fn self_send_stops_with_same_nonce() {
        let mut a = Handshake::new(cfg(0xABCD_EF01_2345_6789, 3, "me"), 0);
        let mut n_sent = 0;
        for t in 0..200 {
            let o = a.poll_send(t);
            for m in [o.hello, o.agree].into_iter().flatten() {
                n_sent += 1;
                if let Some(r) = a.on_packet(&pkt(m), t + 1) {
                    a.on_packet(&pkt(r), t + 1);
                }
            }
        }
        assert_eq!(a.state(), State::Failed(FailReason::SameNonce));
        assert_eq!(n_sent, 1, "最初の HELLO だけで止まり、以後は何も送らない");
        assert_eq!(a.coordinator(), None);
    }

    #[test]
    fn latency_is_clamped_to_wire_range() {
        let mut a = Handshake::new(cfg(5, 200, "A"), 0);
        let m = a.poll_send(0).hello.expect("最初の HELLO");
        assert!(matches!(pkt(m).msg, Message::Hello(h) if h.latency == MAX_DELAY));
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Kind {
        Hello,
        Ack,
        Agree,
    }

    fn kind_of(m: &Message) -> Kind {
        match m {
            Message::Hello(_) => Kind::Hello,
            Message::HelloAck { .. } => Kind::Ack,
            Message::Agree { .. } => Kind::Agree,
            _ => unreachable!("handshake が送るのはこの 3 つだけ"),
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct Ctx {
        now: u32,
        from: usize,
        kind: Kind,
        nth: u32,
    }

    /// 筋書きが返す遅れの列の数だけ届く（空 = 落とす、2つ = 重ねる、遅れの違い = 入れ替える）。
    type Fate<'a> = dyn FnMut(Ctx) -> Vec<u32> + 'a;

    struct NodeCfg {
        cfg: Config,
        start_at: u32,
    }

    #[derive(Debug)]
    struct NodeOut {
        state: State,
        role: Option<Role>,
        established: Option<(u32, Established)>,
        sent: Vec<(u32, Kind)>,
        counters: Counters,
        dropped_before_start: u32,
    }

    struct Node {
        c: NodeCfg,
        hs: Option<Handshake>,
        established: Option<(u32, Established)>,
        sent: Vec<(u32, Kind)>,
        dropped_before_start: u32,
    }

    struct Net {
        q: Vec<(u32, u64, usize, Vec<u8>)>,
        seq: u64,
        nth: [[u32; 3]; 2],
    }

    impl Net {
        fn send(
            &mut self,
            now: u32,
            from: usize,
            m: &Message,
            fate: &mut Fate<'_>,
            log: &mut Vec<(u32, Kind)>,
        ) {
            let kind = kind_of(m);
            log.push((now, kind));
            let n = &mut self.nth[from][kind as usize];
            let ctx = Ctx {
                now,
                from,
                kind,
                nth: *n,
            };
            *n += 1;
            let mut buf = [0u8; MAX_DATAGRAM];
            let len = tenc(SID, m, &mut buf);
            for d in fate(ctx) {
                self.q
                    .push((now + d.max(1), self.seq, 1 - from, buf[..len].to_vec()));
                self.seq += 1;
            }
        }

        fn take_due(&mut self, now: u32) -> Vec<(usize, Vec<u8>)> {
            let mut due = Vec::new();
            let mut i = 0;
            while i < self.q.len() {
                if self.q[i].0 <= now {
                    due.push(self.q.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            due.sort_by_key(|x| (x.0, x.1));
            due.into_iter().map(|x| (x.2, x.3)).collect()
        }
    }

    fn note(n: &mut Node, t: u32) {
        if let Some(hs) = &n.hs {
            match (hs.state(), n.established) {
                (State::Established(e), None) => n.established = Some((t, e)),
                (State::Established(e), Some((_, e0))) => {
                    assert_eq!(e, e0, "繋がった後に結果が変わった")
                }
                (_, Some(_)) => panic!("繋がった後に状態が戻った: {:?}", hs.state()),
                _ => {}
            }
        }
    }

    /// 1 msずつ: 届いたものを台へ渡す（台の返答はすぐ相手へ送る）→ 始める時刻の台を作る → 両台が`poll_send`。
    /// `stop_after`は、両台が接続してからこのmsだけ回して止める（返答が続くことも見るため）。
    fn run(
        cfgs: [NodeCfg; 2],
        end: u32,
        stop_after: Option<u32>,
        fate: &mut Fate<'_>,
    ) -> [NodeOut; 2] {
        let mut nodes = cfgs.map(|c| Node {
            c,
            hs: None,
            established: None,
            sent: Vec::new(),
            dropped_before_start: 0,
        });
        let mut net = Net {
            q: Vec::new(),
            seq: 0,
            nth: [[0; 3]; 2],
        };
        let mut both_at = None;
        for t in 0..end {
            for (to, bytes) in net.take_due(t) {
                let p = tdec(&bytes).expect("模型のパケットは読める");
                let node = &mut nodes[to];
                let Some(hs) = node.hs.as_mut() else {
                    node.dropped_before_start += 1;
                    continue;
                };
                if let Some(r) = hs.on_packet(&p, t) {
                    net.send(t, to, &r, fate, &mut node.sent);
                }
                note(node, t);
            }
            for (i, node) in nodes.iter_mut().enumerate() {
                if node.hs.is_none() && t >= node.c.start_at {
                    node.hs = Some(Handshake::new(node.c.cfg, t));
                }
                let Some(hs) = node.hs.as_mut() else { continue };
                let o = hs.poll_send(t);
                for m in [o.hello, o.agree].into_iter().flatten() {
                    net.send(t, i, &m, fate, &mut node.sent);
                }
                note(node, t);
            }
            if both_at.is_none() && nodes.iter().all(|n| n.established.is_some()) {
                both_at = Some(t);
            }
            if let (Some(b), Some(s)) = (both_at, stop_after)
                && t >= b + s
            {
                break;
            }
        }
        nodes.map(|n| {
            let hs = n.hs.expect("模型の終わりには両台とも始めている");
            NodeOut {
                state: hs.state(),
                role: hs
                    .coordinator()
                    .map(|c| if c { Role::Host } else { Role::Guest }),
                established: n.established,
                sent: n.sent,
                counters: hs.counters(),
                dropped_before_start: n.dropped_before_start,
            }
        })
    }

    /// 合格の不変量は次の通り: 両者が接続している・役割が逆・サイドが食い違わない・delayが同じで希望の大きい方・名前と希望latencyが入れ違いで届く。
    /// サイドが食い違わないとは、host_seatはホストが設定した値が両者に同じく届き、ゲストが設定したhost_seatは使われないこと。
    fn check_pair(cfgs: &[Config; 2], out: &[NodeOut; 2]) -> Result<(), String> {
        let mut es = [None, None];
        for i in 0..2 {
            match out[i].state {
                State::Established(e) => es[i] = Some(e),
                s => return Err(format!("台 {i} が繋がっていない: {s:?}")),
            }
        }
        let (a, b) = (es[0].unwrap(), es[1].unwrap());
        let host_idx = if cfgs[0].role_nonce > cfgs[1].role_nonce {
            0
        } else {
            1
        };
        let host = if host_idx == 0 { a } else { b };
        let guest = if host_idx == 0 { b } else { a };
        if host.role != Role::Host || guest.role != Role::Guest {
            return Err(format!("役割が違う: {a:?} / {b:?}"));
        }
        let want_host_seat = cfgs[host_idx].host_seat;
        if host.host_seat != want_host_seat || guest.host_seat != want_host_seat {
            return Err(format!(
                "host_seat がホストの設定（{want_host_seat:?}）でない: {a:?} / {b:?}"
            ));
        }
        if host.seat != want_host_seat || guest.seat != other_seat(want_host_seat) {
            return Err(format!("席が食い違う: {a:?} / {b:?}"));
        }
        let want = cfgs[0]
            .latency
            .min(MAX_DELAY)
            .max(cfgs[1].latency.min(MAX_DELAY));
        if a.delay != want || b.delay != want {
            return Err(format!("delay が {want} でない: {} / {}", a.delay, b.delay));
        }
        for (me, other) in [(0, 1), (1, 0)] {
            let e = es[me].unwrap();
            if e.peer_name != cfgs[other].name
                || e.peer_latency != cfgs[other].latency.min(MAX_DELAY)
            {
                return Err(format!("台 {me} に届いた相手の値が違う: {e:?}"));
            }
        }
        Ok(())
    }

    fn lossless_d(d: u32) -> impl FnMut(Ctx) -> Vec<u32> {
        move |_| vec![d]
    }

    fn pair_cfgs() -> [Config; 2] {
        [
            cfg(0xF000_0000_0000_0001, 2, "Aさん"),
            cfg(0x0000_0000_0000_0002, 5, "B"),
        ]
    }

    fn sorted(mut v: Vec<(u32, Kind)>) -> Vec<(u32, Kind)> {
        v.sort();
        v
    }

    #[test]
    fn table_simultaneous_start() {
        // 片道7 msで両者t=0に始める（手で追った期待値）。
        // 7: 両者が相手のHELLOを受けてHELLO_ACKを返し、未ackなので即送りでHELLOを出す。台0（nonceが大きい）はホストに決めて、同じpoll_sendでAGREEを出す（以後4 msごとに送り直し）。
        // 14: 台1が接続して返答する（1往復）。
        // 21: 台0が返答(14) で接続する（1.5往復）。
        // 両者ともack済みなので27のHELLOは出さない。
        let cfgs = pair_cfgs();
        let out = run(
            [
                NodeCfg {
                    cfg: cfgs[0],
                    start_at: 0,
                },
                NodeCfg {
                    cfg: cfgs[1],
                    start_at: 0,
                },
            ],
            200,
            None,
            &mut lossless_d(7),
        );
        use Kind::*;
        assert_eq!(
            sorted(out[0].sent.clone()),
            vec![
                (0, Hello),
                (7, Hello),
                (7, Ack),
                (7, Agree),
                (11, Agree),
                (14, Ack),
                (15, Agree),
                (19, Agree)
            ]
        );
        assert_eq!(
            sorted(out[1].sent.clone()),
            vec![
                (0, Hello),
                (7, Hello),
                (7, Ack),
                (14, Ack),
                (14, Agree),
                (18, Agree),
                (22, Agree),
                (26, Agree)
            ]
        );
        assert_eq!(out[0].established.map(|x| x.0), Some(21));
        assert_eq!(out[1].established.map(|x| x.0), Some(14));
        check_pair(&cfgs, &out).unwrap();
        assert_eq!(out[0].counters, Counters::default());
        assert_eq!(out[1].counters, Counters::default());
    }

    #[test]
    fn table_late_start() {
        // 片道7 ms。台0（ホスト）がt=0、台1（ゲスト）がt=50に始める（手で追った期待値）。
        // 台0のHELLO 0・20・40は台1が始める前に届いて落ちる。
        // 57: 台0が受けてホストに決め、即送りでHELLOとAGREEを出す。
        // 64: 台1がHELLO_ACK → HELLO → AGREEの順に受けて接続し、返答する（1往復）。
        // 71: 台0が返答で接続する。
        // 即送りが無いと、64のAGREEはゲストがホストのHELLOを受ける前に届いて捨てられ、68 / 75に接続する。
        let cfgs = pair_cfgs();
        let out = run(
            [
                NodeCfg {
                    cfg: cfgs[0],
                    start_at: 0,
                },
                NodeCfg {
                    cfg: cfgs[1],
                    start_at: 50,
                },
            ],
            300,
            None,
            &mut lossless_d(7),
        );
        use Kind::*;
        assert_eq!(
            sorted(out[0].sent.clone()),
            vec![
                (0, Hello),
                (20, Hello),
                (40, Hello),
                (57, Hello),
                (57, Ack),
                (57, Agree),
                (61, Agree),
                (65, Agree),
                (69, Agree)
            ]
        );
        assert_eq!(
            sorted(out[1].sent.clone()),
            vec![
                (50, Hello),
                (64, Ack),
                (64, Agree),
                (68, Agree),
                (72, Agree),
                (76, Agree)
            ]
        );
        assert_eq!(out[0].established.map(|x| x.0), Some(71));
        assert_eq!(out[1].established.map(|x| x.0), Some(64));
        assert_eq!(out[1].dropped_before_start, 3);
        assert_eq!(out[1].counters.agree_early, 0);
        check_pair(&cfgs, &out).unwrap();
    }

    type Script = Box<dyn Fn(Ctx, usize) -> Vec<u32>>;

    fn scripted() -> Vec<(&'static str, Script)> {
        vec![
            (
                "hello_lost_3_each",
                Box::new(|c: Ctx, _| {
                    if c.kind == Kind::Hello && c.nth < 3 {
                        vec![]
                    } else {
                        vec![9]
                    }
                }),
            ),
            (
                "ack_lost_5_each",
                Box::new(|c: Ctx, _| {
                    if c.kind == Kind::Ack && c.nth < 5 {
                        vec![]
                    } else {
                        vec![9]
                    }
                }),
            ),
            (
                "agree_and_echo_lost_4",
                Box::new(|c: Ctx, _| {
                    if c.kind == Kind::Agree && c.nth < 4 {
                        vec![]
                    } else {
                        vec![9]
                    }
                }),
            ),
            // ゲストの返答を全部落とす間にホストのAGREEだけは届く → ホストは送り続け、500 ms後から返答が届く。
            (
                "echo_lost_500ms",
                Box::new(|c: Ctx, h| {
                    if c.kind == Kind::Agree && c.from != h && c.now < 500 {
                        vec![]
                    } else {
                        vec![9]
                    }
                }),
            ),
            // ホストのHELLOだけ遅らせ、AGREEが先に届く（ゲストはHELLOの前のAGREEを捨てる）。
            (
                "agree_before_hello",
                Box::new(|c: Ctx, h| {
                    if c.kind == Kind::Hello && c.from == h {
                        vec![60]
                    } else {
                        vec![5]
                    }
                }),
            ),
            (
                "dup_reorder_all",
                Box::new(|c: Ctx, _| vec![3 + (c.nth * 7) % 13, 25]),
            ),
            (
                "host_to_guest_blackout_300",
                Box::new(|c: Ctx, h| {
                    if c.from == h && c.now < 300 {
                        vec![]
                    } else {
                        vec![9]
                    }
                }),
            ),
            (
                "guest_to_host_blackout_300",
                Box::new(|c: Ctx, h| {
                    if c.from != h && c.now < 300 {
                        vec![]
                    } else {
                        vec![9]
                    }
                }),
            ),
        ]
    }

    #[test]
    fn model_scripted_losses() {
        let mut base = pair_cfgs();
        // 両者の設定のサイドをばらばらにする。base[0]がnonceの大きい方（常にホスト）なので host_seat = 2P で通る。
        // base[1]（ゲスト）の1Pは使われないはず。
        base[0].host_seat = Seat::P2;
        base[1].host_seat = Seat::P1;
        let mut agree_early_seen = 0;
        for (name, f) in scripted() {
            for swap in [false, true] {
                let cfgs = if swap { [base[1], base[0]] } else { base };
                let host = if cfgs[0].role_nonce > cfgs[1].role_nonce {
                    0
                } else {
                    1
                };
                for (s0, s1) in [(0, 0), (0, 37), (111, 0)] {
                    let out = run(
                        [
                            NodeCfg {
                                cfg: cfgs[0],
                                start_at: s0,
                            },
                            NodeCfg {
                                cfg: cfgs[1],
                                start_at: s1,
                            },
                        ],
                        TIMEOUT_MS,
                        Some(300),
                        &mut |c| f(c, host),
                    );
                    check_pair(&cfgs, &out)
                        .unwrap_or_else(|e| panic!("{name} swap={swap} start=({s0},{s1}): {e}"));
                    agree_early_seen += out[1 - host].counters.agree_early;
                }
            }
        }
        // 「HELLOの前のAGREE」の筋が本当に効いている（ゲストが捨てた数が0なら筋が働いていない）。
        assert!(agree_early_seen > 0);
    }

    fn random_fate(seed: u64, base: u32) -> impl FnMut(Ctx) -> Vec<u32> {
        let mut rng = Rng(seed);
        move |_| {
            let r = rng.below(100);
            let jitter = rng.below(3);
            if r < 20 {
                vec![]
            } else if r < 30 {
                vec![base + 1 + rng.below(40)]
            } else if r < 35 {
                vec![base + jitter, base + 1 + rng.below(30)]
            } else {
                vec![base + jitter]
            }
        }
    }

    fn random_cfgs(rng: &mut Rng) -> [Config; 2] {
        let names = [
            "霊夢",
            "魔理沙",
            "咲夜",
            "妖夢",
            "鈴仙",
            "チルノ",
            "リリカ",
            "ミスティア",
        ];
        let i = rng.below(names.len() as u32) as usize;
        let j = (i + 1 + rng.below(names.len() as u32 - 1) as usize) % names.len();
        let mut n0 = rng.next();
        let mut n1 = rng.next();
        if n0 == n1 {
            n1 ^= 1;
        }
        if rng.below(2) == 0 {
            std::mem::swap(&mut n0, &mut n1);
        }
        // 両者の設定のサイドを互いに逆にして生成し、接続した結果がホストの設定だけで決まる（ゲストの設定は使われない）ことをcheck_pairに検証させる。
        let seat0 = if rng.below(2) == 0 {
            Seat::P1
        } else {
            Seat::P2
        };
        [
            cfg_seat(n0, rng.below(10) as u8, names[i], seat0),
            cfg_seat(n1, rng.below(10) as u8, names[j], other_seat(seat0)),
        ]
    }

    #[test]
    fn model_random_losses() {
        let mut rng = Rng(0x00C0_FFEE);
        for seed in 0..400u64 {
            let cfgs = random_cfgs(&mut rng);
            let base = 1 + rng.below(80);
            let s0 = rng.below(300);
            let s1 = rng.below(300);
            let out = run(
                [
                    NodeCfg {
                        cfg: cfgs[0],
                        start_at: s0,
                    },
                    NodeCfg {
                        cfg: cfgs[1],
                        start_at: s1,
                    },
                ],
                s0.max(s1) + TIMEOUT_MS,
                Some(200),
                &mut random_fate(seed, base),
            );
            check_pair(&cfgs, &out).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        }
    }

    #[test]
    fn model_all_lost_times_out_nothing_received() {
        let cfgs = pair_cfgs();
        let out = run(
            [
                NodeCfg {
                    cfg: cfgs[0],
                    start_at: 0,
                },
                NodeCfg {
                    cfg: cfgs[1],
                    start_at: 30,
                },
            ],
            TIMEOUT_MS + 500,
            None,
            &mut |_| vec![],
        );
        for (i, o) in out.iter().enumerate() {
            assert_eq!(
                o.state,
                State::Failed(FailReason::NothingReceived),
                "台 {i}"
            );
            let start = [0, 30][i];
            let last = o.sent.iter().map(|x| x.0).max().unwrap();
            assert!(
                last < start + TIMEOUT_MS,
                "台 {i} がやめた後に送った: {last}"
            );
            assert_eq!(o.sent.len(), 500, "台 {i}");
        }
    }

    #[test]
    fn model_one_way_loss_splits_reasons() {
        let cfgs = pair_cfgs();
        // lost_fromの台が送ったものだけを全部落とす。lost_from側は相手のHELLOを受けるが接続できない。相手側は何も受けない（1通も届かない）。
        for lost_from in [0usize, 1] {
            let out = run(
                [
                    NodeCfg {
                        cfg: cfgs[0],
                        start_at: 0,
                    },
                    NodeCfg {
                        cfg: cfgs[1],
                        start_at: 0,
                    },
                ],
                TIMEOUT_MS + 100,
                None,
                &mut |c| if c.from == lost_from { vec![] } else { vec![9] },
            );
            let deaf = 1 - lost_from;
            assert_eq!(
                out[deaf].state,
                State::Failed(FailReason::NothingReceived),
                "受けなかった台 {deaf}"
            );
            assert_eq!(
                out[lost_from].state,
                State::Failed(FailReason::NoHandshake),
                "受けた台 {lost_from}"
            );
            let want = if lost_from == 0 {
                Role::Host
            } else {
                Role::Guest
            };
            assert_eq!(out[lost_from].role, Some(want));
        }
    }

    fn connect_times(n: u64, d: u32, lossy: bool) -> Vec<(u32, u32)> {
        let mut rng = Rng(0xD15_7121B);
        let mut v = Vec::new();
        for seed in 0..n {
            let cfgs = random_cfgs(&mut rng);
            let host = if cfgs[0].role_nonce > cfgs[1].role_nonce {
                0
            } else {
                1
            };
            let mut jitter = Rng(seed ^ 0x55);
            let mut lossy_f = random_fate(seed, d);
            let mut fate = |c: Ctx| {
                if lossy {
                    lossy_f(c)
                } else {
                    vec![d + jitter.below(3)]
                }
            };
            let out = run(
                [
                    NodeCfg {
                        cfg: cfgs[0],
                        start_at: 0,
                    },
                    NodeCfg {
                        cfg: cfgs[1],
                        start_at: 0,
                    },
                ],
                TIMEOUT_MS,
                Some(0),
                &mut fate,
            );
            check_pair(&cfgs, &out).unwrap();
            v.push((
                out[1 - host].established.unwrap().0,
                out[host].established.unwrap().0,
            ));
        }
        v
    }

    fn histogram(label: &str, xs: &[u32], edges: &[u32]) {
        let mut line = format!("{label} n={}:", xs.len());
        let mut lo = 0;
        for &e in edges {
            let c = xs.iter().filter(|&&x| x >= lo && x < e).count();
            line += &format!(" [{lo},{e})={c}");
            lo = e;
        }
        let c = xs.iter().filter(|&&x| x >= lo).count();
        line += &format!(" [{lo},)={c} max={}", xs.iter().max().unwrap());
        eprintln!("{line}");
    }

    #[test]
    fn connect_time_distribution() {
        let d = 20;
        let clean = connect_times(300, d, false);
        for &(g, h) in &clean {
            assert!((2 * d..=2 * d + 4).contains(&g), "ゲスト {g}");
            assert!((3 * d..=3 * d + 6).contains(&h), "ホスト {h}");
        }
        let edges = [40, 45, 60, 67, 80, 100, 140, 200];
        histogram(
            "clean guest",
            &clean.iter().map(|x| x.0).collect::<Vec<_>>(),
            &edges,
        );
        histogram(
            "clean host ",
            &clean.iter().map(|x| x.1).collect::<Vec<_>>(),
            &edges,
        );
        let lossy = connect_times(1000, d, true);
        histogram(
            "lossy guest",
            &lossy.iter().map(|x| x.0).collect::<Vec<_>>(),
            &edges,
        );
        histogram(
            "lossy host ",
            &lossy.iter().map(|x| x.1).collect::<Vec<_>>(),
            &edges,
        );
    }

    /// 両者の始める時刻を0〜499 msでばらし、遅く始めた側の開始から接続するまでの時間（片道`d` ms ＋ 揺らぎ0〜2 ms、落とさない）を測る。
    /// 結果は`(ゲスト, ホスト)`の列。
    fn late_connect_times(n: u64, d: u32) -> Vec<(u32, u32)> {
        let mut rng = Rng(0x1A7E);
        let mut v = Vec::new();
        for seed in 0..n {
            let cfgs = random_cfgs(&mut rng);
            let host = if cfgs[0].role_nonce > cfgs[1].role_nonce {
                0
            } else {
                1
            };
            let s0 = rng.below(500);
            let s1 = rng.below(500);
            let mut jitter = Rng(seed ^ 0x77);
            let out = run(
                [
                    NodeCfg {
                        cfg: cfgs[0],
                        start_at: s0,
                    },
                    NodeCfg {
                        cfg: cfgs[1],
                        start_at: s1,
                    },
                ],
                s0.max(s1) + TIMEOUT_MS,
                Some(0),
                &mut |_| vec![d + jitter.below(3)],
            );
            check_pair(&cfgs, &out).unwrap();
            let later = s0.max(s1);
            v.push((
                out[1 - host].established.unwrap().0 - later,
                out[host].established.unwrap().0 - later,
            ));
        }
        v
    }

    #[test]
    fn refusing_before_any_hello_sends_exactly_one_hello() {
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        a.refuse_version(0);
        let first = a.poll_send(5);
        assert!(first.hello.is_some(), "まだ出していない HELLO を 1 通出す");
        assert!(first.agree.is_none());
        assert_eq!(a.poll_send(5 + HELLO_INTERVAL_MS * 3), Outbox::default());
        let mut b = Handshake::new(cfg(500, 2, "B"), 0);
        assert!(b.poll_send(0).hello.is_some());
        b.refuse_version(0);
        assert_eq!(b.poll_send(HELLO_INTERVAL_MS * 3), Outbox::default());
        let mut t = Handshake::new(cfg(500, 2, "T"), 0);
        t.expire(TIMEOUT_MS);
        assert_eq!(t.poll_send(TIMEOUT_MS), Outbox::default());
    }

    #[test]
    fn refusing_the_version_stops_the_handshake_for_good() {
        let mut a = Handshake::new(cfg(500, 2, "A"), 0);
        assert!(
            a.poll_send(0).hello.is_some(),
            "効く期間: やめる前は HELLO を送る"
        );
        a.refuse_version(0);
        let gone = State::Failed(FailReason::OtherVersion { theirs: 0 });
        assert_eq!(a.state(), gone);
        assert_eq!(a.poll_send(HELLO_INTERVAL_MS * 3), Outbox::default());
        assert_eq!(a.on_packet(&pkt(hello_of(100, 7, "B")), 30), None);
        assert_eq!(a.coordinator(), None);
        assert_eq!(
            a.expire(TIMEOUT_MS * 2),
            gone,
            "時間切れで理由が上書きされた"
        );
        assert_eq!(
            a.restart(TIMEOUT_MS * 2),
            None,
            "バージョン違いを作り直した"
        );
        a.refuse_version(7);
        assert_eq!(a.state(), gone, "後のバージョンで理由を上書きした");
        let mut t = Handshake::new(cfg(500, 2, "A"), 0);
        t.expire(TIMEOUT_MS);
        t.refuse_version(0);
        assert_eq!(t.state(), State::Failed(FailReason::NothingReceived));
        let mut host = Handshake::new(cfg(900, 2, "host"), 0);
        let mut guest = Handshake::new(cfg(100, 2, "guest"), 0);
        for now in 0..200 {
            let ob = host.poll_send(now);
            for m in [ob.hello, ob.agree].into_iter().flatten() {
                if let Some(r) = guest.on_packet(&pkt(m), now) {
                    host.on_packet(&pkt(r), now);
                }
            }
            let og = guest.poll_send(now);
            for m in [og.hello, og.agree].into_iter().flatten() {
                if let Some(r) = host.on_packet(&pkt(m), now) {
                    guest.on_packet(&pkt(r), now);
                }
            }
            if matches!(host.state(), State::Established(_))
                && matches!(guest.state(), State::Established(_))
            {
                break;
            }
        }
        assert!(
            matches!(host.state(), State::Established(_)),
            "前提: 繋がる"
        );
        host.refuse_version(0);
        assert!(matches!(host.state(), State::Established(_)));
    }

    #[test]
    fn late_start_connect_time_lan() {
        let d = 3;
        let v = late_connect_times(1000, d);
        // 上限は手で追った最悪の筋で、ゲスト3片道・ホスト4片道に、片道ごとの揺らぎ2 msとAGREEがHELLOを追い越したときのAGREEの送り直し1回分（4 + 2 ms）を足した。
        let (gmax, hmax) = (3 * d + 3 * 2 + 6, 4 * d + 4 * 2 + 6);
        for &(g, h) in &v {
            assert!(g <= gmax, "ゲスト {g} > {gmax}");
            assert!(h <= hmax, "ホスト {h} > {hmax}");
        }
        let edges = [6, 9, 10, 12, 15, 20, 25, 30];
        let gs: Vec<u32> = v.iter().map(|x| x.0).collect();
        let hs: Vec<u32> = v.iter().map(|x| x.1).collect();
        histogram("late(d=3) guest", &gs, &edges);
        histogram("late(d=3) host ", &hs, &edges);
        let mean = |xs: &[u32]| xs.iter().map(|&x| f64::from(x)).sum::<f64>() / xs.len() as f64;
        eprintln!(
            "late(d=3) n={} mean guest={:.1} host={:.1}",
            v.len(),
            mean(&gs),
            mean(&hs)
        );
    }
}
