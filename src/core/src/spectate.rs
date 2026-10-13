//! 観戦の core の部分（SPECTATE_JOIN・SPECTATE_STREAM・SPECTATE_ACK）。
//!
//! 観戦者はホストだけに繋ぐ。ホストは「使うと決まったもの」を**記録**（`wire::Record`）として 1 本の通し番号（u32）に並べ、観戦者へ流す。
//! 観戦者は連続で受け取れた所までを ack で返す。
//!
//! ```text
//! ホスト（ゲームのスレッド）→ push(記録) → [SpectateHost のリングバッファ] ← poll / on_datagram（受信スレッド）
//!                                                 │ SPECTATE_STREAM（ウィンドウで詰める・送り直す）
//!                                                 ▼                    ▲ SPECTATE_ACK / JOIN
//!                                            [Watcher: Assembler] → pop() → 観戦者のゲーム
//! ```
//!
//! **seg**: dll の並びは「`Seg(0)` → 名前（`Name` × 2）→ 選択の記録 → マッチの語 → `Seg(1)` → … → `End`」で、`Seg(k)` は「この後の記録は
//! seg k のもの」の宣言（[`seg_head`]）。**seg の始まり**は「最後の `Frame`・`End`・`Seg` の次」で、遅れて JOIN した観戦者はここから流す。
//! リングバッファが埋まったときだけ、いまの seg の始まりより前を捨て、捨てられた所にいた観戦者はいまの seg の始まりへ**飛ぶ**（`SpectateStream::reset`）。
//! 1 つの seg だけでリングバッファが埋まると、その seg は諦める（観戦者を全員外し、次の `Seg` まで記録を受けない）。
//!
//! **送り**: 観戦者ごとに `ack`（次に要る番号）・`sent_to`（どこまで送ったか）・`sack`（`ack` の後ろで受け取れたと分かっている記録）を持ち、
//! ウィンドウは `ack` から `SEND_WINDOW` 記録。新しい分はすぐ送る。ACK は `next` に加えて、その後ろで受け取れた記録をビットで載せ
//! （`wire::SACK_BITS` = 63 個）、ホストは**届いていない記録だけ**を送り直す。送り直しは記録ごとに `rto` × 2^(送った回数 − 1)
//! （上限 `RESEND_MAX_MS`）の間届かなければ行い、**黙った観戦者**（ACK も JOIN も `DARK_MS` 聞こえない）へは先頭の穴の時計だけで
//! 先最初から 1 通ぶんを送る（記録ごとだとウィンドウの 64 記録が連打になる）。`rto` は ACK から取った往復の見積もり（RFC 6298）に ACK のペースを
//! 足したもので、送り直した記録は標本にしない（Karn）。未確認が無いまま `HEARTBEAT_MS` 経ったら空の STREAM を送り、何も聞かない時間が
//! `SILENT_MS` を越えた観戦者と BYE を送った観戦者は外す。
//!
//! **受け入れと穴あけ**: 観戦者は、最初の STREAM を受けるまでは観戦用のコードの候補から来たものを受け、候補に無い送り元からは
//! 「タグ・session の合う STREAM」だけを受けて以後そのアドレスをホストとする（`learned_host`。ホストの NAT が宛先ごとに外側のポートを
//! 変える型への備え。代償: コードの秘密を知る者は偽の記録を最初に送れるが、対戦者には届かない）。**穴あけ**（[`PunchPlan`]）:
//! ホストが観戦者の候補のアドレスへ 4 B のパケット（[`PUNCH_PACKET`]）を送り、ホスト側の NAT に経路を作って観戦者の JOIN を通す。
//!
//! **ゲームスレッド**: 1 つのスレッドから呼ぶ前提（`&mut self`）。`push`・`on_datagram`・`poll`・`Watcher` の各関数はヒープを確保せず、
//! パニックしうる添字を置かない（`tests/alloc_check.rs`）。確保するのは `new` だけ。時刻 `now` は ms（折り返しは `wrapping_sub`）。
//!
//! **鍵**: ホストは観戦を開くときに使い捨ての鍵を引いて公開鍵を観戦用の接続コードに載せ、観戦者は JOIN ごとに引いて JOIN に載せる。
//! 鍵は対戦と同じ式（`seal::Ephemeral::dir_keys`。「ホスト → 観戦者」が前）で観戦者ごとに違い、JOIN の後の STREAM・ACK・BYE は全部封じる。
//! **通し番号はホストに 1 本**（`Sender::SpectateHost`）: 観戦者ごとに数えると、同じ公開鍵の観戦者が入り直したとき nonce が重なる。
//! ビルド番号が違う JOIN は捨てて数える（`DropReason::OtherBuild`）。

use std::net::SocketAddr;

use crate::auth::{AuthKey, Secret};
use crate::candidate::{Candidates, MAX_CANDIDATES, same_endpoint};
use crate::handshake::{Established, Name};
use crate::link::Outlet;
use crate::seal::{DirKeys, Ephemeral, KeySeed, PUBLIC_LEN, Sender, Seq};
use crate::wire::{
    self, ByeReason, Datagram, DecodeError, MAX_DATAGRAM, MAX_STREAM_RECORDS, Message, NAME_MAX,
    Record, SACK_BITS, Seat, SpectateJoin, SpectateStream,
};

/// 観戦者の数の上限。観戦の送りは受信スレッドが観戦者ごとに 1 通ずつ行い（loopback の実測で 1 周 p95 約 0.2 ms）、ホストの上りは
/// 1 人あたり約 3 KB/s（本文。IPv4 のヘッダ込みで約 4.6 KB/s）。10 人でも対戦の相手の便の受け取りの遅れに差が出なかった
/// （`dll` の `spectate_host::load` の測り）。
pub const MAX_SPECTATORS: usize = 10;
/// リングバッファの大きさ（記録の数）。1 seg 60 分のマッチの語（216,000 個）＋ 選択の画面の MENU の語と余白で 2^18。1 記録 36 B
/// （名前の記録が最大。`wire::tests::a_record_stays_within_36_bytes` が見る）なので約 9.4 MB（起動時に 1 回確保）。
pub const RING_CAP: usize = 262_144;
/// ホストが、観戦者の ack より先へ送ってよい記録の数（ウィンドウ）。
pub const SEND_WINDOW: u32 = 64;
// ウィンドウは ack の番号そのものを含むので、ack の後ろで受け取っていてよいのは `SEND_WINDOW - 1` 個まで（ACK の受け取れた範囲の幅）。
const _: () = assert!(SEND_WINDOW - 1 == SACK_BITS);
/// 観戦者の ACK が返るペース（ms）。dll の観戦者は入力の地点（ゲームのスレッド）で 1 フレームに 1 回だけ ACK を返す
/// （60 Hz = 16.7 ms を切り上げ）。
pub const ACK_TICK_MS: u32 = 17;
/// 送り直しの間隔の下限（ms）。ACK のペース 2 回ぶん（1 フレームの遅れがあっても、次の ACK までに送り直さない）。
pub const RESEND_MIN_MS: u32 = 2 * ACK_TICK_MS;
/// 往復の見積もりがまだ無いときの送り直しの間隔（ms）。ACK のペース ＋ インターネットの往復（〜80 ms）を見込んだ値。
pub const RESEND_INIT_MS: u32 = 100;
/// 送り直しの間隔の上限（ms）。
pub const RESEND_MAX_MS: u32 = 250;
/// これだけ ACK も JOIN も聞こえない観戦者は、記録ごとの送り直しの時計をやめて先頭の穴だけを送り直す（`Watching::resend_due`）。
const DARK_MS: u32 = RESEND_MAX_MS;
/// 先頭の穴から 1 通ぶん（`MAX_STREAM_RECORDS` 個）を指すビット（`Watching::resend_due` の `due` と同じ並び）。
const FIRST_PACKET: u64 = (1 << MAX_STREAM_RECORDS) - 1;
/// `Watching::rto_scale` の上限（`RESEND_MAX_MS` を越えた倍率は効かないので、そこまで）。
const RTO_SCALE_MAX: u32 = 8;
/// 未確認が無いときに、生きている合図（空の STREAM）を送る間隔（ms）。
pub const HEARTBEAT_MS: u32 = 1_000;
/// この間、何も聞かない観戦者を外す（ms）。合図（`HEARTBEAT_MS`）10 回ぶん。
pub const SILENT_MS: u32 = 10_000;
/// 観戦者が消費を待たずに持つ連続の記録の数（`Assembler`）。観戦者の遅れがこれを越えると ack を止めてホストのウィンドウを止める。
pub const ASM_CAP: usize = 1_024;
pub const JOIN_RESEND_MS: u32 = 250;
/// 満杯の断り（BYE）を封じる間隔の下限（ホスト全体で 1 回）。断るたびに X25519 を計算するので、受信スレッドの仕事を
/// 1 秒に 1 回へ抑える。
pub const REFUSE_GAP_MS: u32 = 1_000;
/// 流れが途切れて（何も聞かないまま）この時間を越えたら、観戦者が JOIN を送り直す（ホストが外した場合に戻る）。
pub const REJOIN_MS: u32 = 3_000;

fn elapsed(now: u32, then: u32) -> u32 {
    let d = now.wrapping_sub(then);
    if d > u32::MAX / 2 { 0 } else { d }
}

/// 鍵を交換する便（JOIN）を作って（HMAC のタグを付けて）送る。
fn emit_signed<O: Outlet>(
    key: &AuthKey,
    session_id: u32,
    to: SocketAddr,
    msg: &Message,
    out: &mut O,
) {
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = wire::encode(key, session_id, msg, &mut buf);
    out.send(to, buf.get(..n).unwrap_or(&[]));
}

/// 封じる便を作って送る。通し番号を使い切っていたら送らない（false）。
fn emit_sealed<O: Outlet>(
    keys: &DirKeys,
    sender: Sender,
    seq: &Seq,
    session_id: u32,
    to: SocketAddr,
    msg: &Message,
    out: &mut O,
) -> bool {
    let mut buf = [0u8; MAX_DATAGRAM];
    match wire::seal(&keys.sealer(sender), seq, session_id, msg, &mut buf) {
        Some(n) => {
            out.send(to, buf.get(..n).unwrap_or(&[]));
            true
        }
        None => false,
    }
}

// ============================ ホスト ============================

/// 新しい seg の始まりとして観戦の流れへ積む 3 つの記録: `Seg(seg)` と、その直後の席ごとの名前（1P → 2P）。名前は繋がった 2 人の
/// HELLO の名前（自分の席で 1P・2P に並べる）で、観戦者が手元のリプレイのフォルダ（`<1P> vs <2P>`）と印に使う。
/// 遅れて入った観戦者も seg の始まりから受けるので毎 seg 積む。dll のホストと peer_sim のホスト役が同じ関数を呼ぶ。
pub fn seg_head(seg: u16, est: &Established) -> [Record; 3] {
    let (p1, p2) = match est.seat {
        Seat::P1 => (est.own_name, est.peer_name),
        Seat::P2 => (est.peer_name, est.own_name),
    };
    [
        Record::Seg { seg },
        Record::name(Seat::P1, p1.as_bytes()),
        Record::name(Seat::P2, p2.as_bytes()),
    ]
}

/// `SpectateHost::new` に渡す値。観戦用の接続コード（`code::Side::Spectate`）の中身。**対戦の秘密・session とは別に引く**こと
/// （同じ値だと、観戦者が対戦の鍵を作れてしまう）。
#[derive(Debug, Clone, Copy)]
pub struct HostConfig {
    pub session_id: u32,
    pub secret: Secret,
    /// 観戦用の使い捨ての鍵の種（観戦を開くたびに呼び手が OS の乱数で引く）。公開鍵は `public` で観戦用のコードに載せる。
    pub key_seed: KeySeed,
}

impl HostConfig {
    /// 公開鍵（観戦用の接続コードに載せる）。X25519 を 1 回計算する。
    pub fn public(&self) -> [u8; PUBLIC_LEN] {
        Ephemeral::from_seed(&self.key_seed).public()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// リングバッファが 1 つの seg で埋まった（捨てられる古い seg が無い）。この記録は捨て、観戦者を全員外し、次の `Seg` まで記録を受けない。
    Full,
    /// 直前の溢れの後で、次の `Seg` を待っている。
    Broken,
    /// 通し番号が u32 を使い切った（60 fps で 2 年余り）。
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    NotHgrt,
    WrongSession,
    BadTag,
    Undecodable,
    /// ヘッダのバージョンが `wire::PROTO_VER` でない（中は相手のバージョン）。観戦用のコードのバージョンが先に断るので、ふつうは来ない。
    OtherVersion(u8),
    /// JOIN のビルド番号がホストと違う（中は観戦者の値）。
    OtherBuild(u32),
    /// JOIN の公開鍵が弱い点で、観戦者の鍵を作れない。
    WeakKey,
    /// ホストが受ける type（JOIN・ACK・BYE）でない。
    Unexpected,
    /// ACK が、JOIN していない送り元・違う nonce から来た。
    UnknownSpectator,
    /// リングバッファが溢れて次の `Seg` を待っている間の JOIN。
    Broken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    Dropped(DropReason),
    /// 新しい観戦者を入れた（同じ送り元が違う nonce で JOIN し直したときも）。
    Joined,
    /// 同じ送り元・同じ nonce の JOIN（黙っていない印だけ更新）。
    Refreshed,
    /// 観戦者が上限に達している。BYE(取消) を返した。
    Refused,
    /// ACK を受けた。`progress` は ack が進んだか。
    Acked {
        progress: bool,
    },
    /// 観戦者が BYE を送って抜けた。
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostCounts {
    pub pushed: u64,
    pub push_dropped: u64,
    pub overflows: u64,
    pub joined: u64,
    pub refused_full: u64,
    /// 満杯の断りのうち、`REFUSE_GAP_MS` の間に重なって BYE を省いたもの（`refused_full` の内数）。
    pub refused_quiet: u64,
    /// 観戦者の公開鍵との鍵の計算（X25519）の回数。受信スレッドで走る重い計算なので、再送の JOIN や満杯の断りで増えないことを検査が見る。
    pub key_agreements: u64,
    pub left: u64,
    pub silenced: u64,
    pub acks: u64,
    /// 積んだより先の番号の ack（嘘。積んだ所まで詰めて受けた）。
    pub acks_clamped: u64,
    /// 受け取れた範囲（`sack`）に、まだ送っていない記録を指すビットがあった ack の数（嘘。そのビットだけ捨てて受けた）。
    pub sacks_clamped: u64,
    pub packets_sent: u64,
    pub records_sent: u64,
    /// 送り直しの通数（`packets_sent` の内数）。
    pub resends: u64,
    /// 送り直しの通に載せた記録の数（`records_sent` の内数。穴の間に挟まった届き済みの記録も数える）。
    pub records_resent: u64,
    pub heartbeats: u64,
    /// リングバッファから捨てられた所にいた観戦者を、いまの seg の始まりへ飛ばした回数。
    pub jumps: u64,
    pub not_hgrt: u64,
    pub wrong_session: u64,
    pub bad_tag: u64,
    pub undecodable: u64,
    pub other_version: u64,
    pub other_build: u64,
    pub weak_key: u64,
    pub unexpected: u64,
    pub unknown_spectator: u64,
}

#[derive(Debug, Clone, Copy)]
struct Watching {
    addr: SocketAddr,
    nonce: u64,
    /// JOIN の公開鍵（同じ送り元・同じ nonce でも、公開鍵が違えば観戦者が起こし直したと見る）。
    public: [u8; PUBLIC_LEN],
    keys: DirKeys,
    /// 次に要る番号（観戦者が連続で受け取った所。`ack` 以降を送り直す）。
    ack: u32,
    /// この観戦者の流れの始まり（JOIN したときのいまの seg の始まりか、リングバッファから落ちて飛ばした先）。この番号から
    /// 始まる STREAM に `reset` を付ける。
    base: u32,
    /// どこまで送ったか（この番号の記録は未送信）。`ack` ≤ `sent_to` ≤ リングバッファの終わり。
    sent_to: u32,
    /// 最後に何かを送った時刻（生きている合図の間隔の起点）。
    last_send: u32,
    rtt: Rtt,
    /// 見積もりに掛ける倍率（1〜`RTO_SCALE_MAX`）。標本が取れないまま 1 回目の送り直しの時間切れが続くと倍にし、
    /// 標本が取れたら 1 へ戻す（Karn の後半）。
    rto_scale: u32,
    /// 観戦者が `ack` の後ろで受け取れた記録（ACK の `sack` を重ねたもの）。ビット i = 番号 `ack + 1 + i`。`ack` が進めば右へ
    /// 詰める。観戦者は受け取った記録を捨てない（`Assembler::on_stream`）ので、遅れて着いた古い ACK の方が少なく見えても
    /// 多い方を残す。
    sack: u64,
    /// ウィンドウの中の記録（番号 % `SEND_WINDOW`。ウィンドウは ack から 64 なので重ならない）を最後に送った時刻と送った回数。
    sent: [Sent; SEND_WINDOW as usize],
    last_heard: u32,
    name: Name,
}

#[derive(Debug, Clone, Copy)]
struct Sent {
    at: u32,
    times: u8,
}

impl Sent {
    const NEVER: Sent = Sent { at: 0, times: 0 };

    fn index(idx: u32) -> usize {
        (idx % SEND_WINDOW) as usize
    }
}

/// JOIN から作る、観戦者を見分ける値（送り元・nonce・公開鍵）と、その観戦者との鍵。
#[derive(Debug, Clone, Copy)]
struct Joiner {
    addr: SocketAddr,
    nonce: u64,
    public: [u8; PUBLIC_LEN],
    keys: DirKeys,
}

/// 往復の見積もり（RFC 6298 の SRTT / RTTVAR。整数の ms）。標本は ACK のペースの待ち（`ACK_TICK_MS`）を含む。
#[derive(Debug, Clone, Copy)]
struct Rtt {
    srtt: u32,
    var: u32,
    sampled: bool,
}

impl Rtt {
    const NEW: Rtt = Rtt {
        srtt: 0,
        var: 0,
        sampled: false,
    };

    fn sample(&mut self, ms: u32) {
        // 流れの制御（観戦者が消費を待って ack を止めた）で長く待った標本が、間隔を上限より大きく引き上げない。
        let ms = ms.min(RESEND_MAX_MS);
        if self.sampled {
            self.var = (3 * self.var + self.srtt.abs_diff(ms)) / 4;
            self.srtt = (7 * self.srtt + ms) / 8;
        } else {
            self.srtt = ms;
            self.var = ms / 2;
            self.sampled = true;
        }
    }

    /// 送り直しの間隔: 見積もり ＋ ばらつき（ACK のペース以上）を `RESEND_MIN_MS`〜`RESEND_MAX_MS` に収めたもの。
    fn rto(&self) -> u32 {
        if !self.sampled {
            return RESEND_INIT_MS;
        }
        self.srtt
            .saturating_add((4 * self.var).max(ACK_TICK_MS))
            .clamp(RESEND_MIN_MS, RESEND_MAX_MS)
    }
}

pub struct SpectateHost {
    session_id: u32,
    key: AuthKey,
    /// 観戦用の使い捨ての鍵（観戦を閉じたら`Ephemeral` ごと捨てる）。
    eph: Ephemeral,
    /// 送り手 `Sender::SpectateHost` の通し番号（全観戦者で 1 本）。
    seq: Seq,
    /// 自分のビルド番号（JOIN の値と比べる。既定は `update::BUILD_NUMBER`）。
    build: u32,
    ring: Vec<Record>,
    head: u32,
    /// いまの seg の始まり（最後に積んだ `Seg` の前置きの最初の記録。前置きが無ければその `Seg`）の番号。遅れて JOIN した観戦者は
    /// ここから。`head` ≤ これ。
    seg_start: u32,
    /// 次の前置きが始まる番号（最後に積んだ `Frame`・`End`・`Seg` の次）。次に `Seg` を積んだとき、これが新しい `seg_start` に
    /// なる。`Seg` の後ろにマッチの語が無いまま次の `Seg` が来たとき前の `Seg` を前置きに含めないよう、`Seg` の次も数える。
    /// まだ何も積んでいなければ 0（最初の seg はリングバッファの先最初から）。
    preamble_start: u32,
    next: u32,
    broken: bool,
    /// 最後に満杯の断りの BYE を封じた時刻（`REFUSE_GAP_MS` の間引き）。
    last_refusal: Option<u32>,
    slots: [Option<Watching>; MAX_SPECTATORS],
    counts: HostCounts,
}

impl SpectateHost {
    /// リングバッファの大きさ `RING_CAP` で作る（確保はここだけ）。
    pub fn new(cfg: HostConfig) -> Self {
        Self::with_capacity(cfg, RING_CAP)
    }

    /// 検査の小さいリングバッファ（0 は 1 に上げる）。
    pub fn with_capacity(cfg: HostConfig, cap: usize) -> Self {
        Self {
            session_id: cfg.session_id,
            key: AuthKey::derive(&cfg.secret, cfg.session_id),
            eph: Ephemeral::from_seed(&cfg.key_seed),
            seq: Seq::new(),
            build: crate::update::BUILD_NUMBER,
            ring: vec![Record::FILLER; cap.max(1)],
            head: 0,
            seg_start: 0,
            preamble_start: 0,
            next: 0,
            broken: false,
            last_refusal: None,
            slots: [None; MAX_SPECTATORS],
            counts: HostCounts::default(),
        }
    }

    pub fn tail(&self) -> u32 {
        self.next
    }

    pub fn public_key(&self) -> [u8; PUBLIC_LEN] {
        self.eph.public()
    }

    /// 検査が違うビルドのホストを作るため。既定は `update::BUILD_NUMBER`。
    pub fn set_build(&mut self, build: u32) {
        self.build = build;
    }

    /// 次に使う通し番号（本ログと検査）。
    pub fn next_seq(&self) -> u64 {
        self.seq.next()
    }

    /// 送り元 `addr` の観戦者との向きごとの鍵（**検査と診断のためだけ**。ホストが送った便を外から開く）。
    #[doc(hidden)]
    pub fn dir_keys_of(&self, addr: SocketAddr) -> Option<DirKeys> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(|w| w.keys)
    }

    pub fn head(&self) -> u32 {
        self.head
    }

    /// いまの seg の始まりの番号（遅れて JOIN した観戦者が流れ始める所）。最後に積んだ `Seg` の記録で、次の `Seg` が来るまで
    /// 動かない。最初の seg はリングバッファの先頭。`Seg` の前に選択の記録が積まれる並びなら、その最初の記録。
    pub fn seg_start(&self) -> u32 {
        self.seg_start
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub fn spectator_count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// 送り元 `addr` の観戦者の、次に要る番号。
    pub fn ack_of(&self, addr: SocketAddr) -> Option<u32> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(|w| w.ack)
    }

    /// 送り元 `addr` の観戦者が、`ack_of` の後ろで受け取れていると分かっている記録（ビット i = 番号 `ack + 1 + i`。ログと検査用）。
    pub fn sack_of(&self, addr: SocketAddr) -> Option<u64> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(|w| w.sack)
    }

    /// 送り元 `addr` の観戦者への、いまの送り直しの間隔の見積もり（ms。倍々になる前の値。ログと検査用）。
    pub fn rto_of(&self, addr: SocketAddr) -> Option<u32> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(|w| w.rtt.rto())
    }

    pub fn spectators(&self) -> impl Iterator<Item = (SocketAddr, Name, u32)> + '_ {
        self.slots.iter().flatten().map(|w| (w.addr, w.name, w.ack))
    }

    pub fn counts(&self) -> HostCounts {
        self.counts
    }

    /// `head` を進め（古い記録を捨て）、捨てた所にいた観戦者を新しい `head` へ飛ばす。
    fn advance_head(&mut self, new_head: u32) {
        self.head = new_head;
        for w in self.slots.iter_mut().flatten() {
            if w.ack < new_head {
                w.ack = new_head;
                w.base = new_head;
                // 送ったつもりの分（リングバッファから落ちた古い記録）は捨てて、先最初から送り直す（`reset` 付きの 1 通目）。
                w.sent_to = new_head;
                w.sack = 0;
                w.rto_scale = 1;
                self.counts.jumps += 1;
            }
        }
    }

    /// 記録を 1 つ積み、その番号を返す。リングバッファが埋まっていたら、いまの seg の始まりより前を捨てる（`Seg` を積むときは、次の seg の
    /// 前置きより前。前置きがリングバッファの先最初から始まっていて捨てる古い seg が無ければ、前置きも含めて全部）。捨てる古い seg が無ければ
    /// `Full`。
    pub fn push(&mut self, rec: Record) -> Result<u32, PushError> {
        let is_seg = matches!(rec, Record::Seg { .. });
        if self.next == u32::MAX {
            return Err(PushError::Exhausted);
        }
        if self.broken && !is_seg {
            self.counts.push_dropped += 1;
            return Err(PushError::Broken);
        }
        let full = (self.next - self.head) as usize >= self.ring.len();
        if is_seg && (full || self.broken) {
            // 溢れの後は前置きも途中から欠けているので全部捨てる。埋まっただけなら次の seg の前置きは残す。
            // 前置きがリングバッファの先最初から始まっていて空きが作れないときは、前置きを諦める。
            let keep = if self.broken {
                self.next
            } else {
                self.preamble_start.max(self.head)
            };
            let keep = if keep > self.head { keep } else { self.next };
            self.broken = false;
            self.advance_head(keep);
        } else if full {
            if self.seg_start > self.head {
                self.advance_head(self.seg_start);
            } else {
                self.broken = true;
                self.counts.overflows += 1;
                self.counts.push_dropped += 1;
                self.slots = [None; MAX_SPECTATORS];
                return Err(PushError::Full);
            }
        }
        let idx = self.next;
        let cap = self.ring.len();
        if let Some(slot) = self.ring.get_mut(idx as usize % cap) {
            *slot = rec;
        }
        if is_seg {
            // 前置きの先頭。リングバッファから落ちていたら（上で全部捨てた）この `Seg` が先頭になる。
            self.seg_start = self.preamble_start.max(self.head);
        }
        if matches!(
            rec,
            Record::Seg { .. } | Record::Frame { .. } | Record::End(_)
        ) {
            self.preamble_start = idx + 1;
        }
        self.next = idx + 1;
        self.counts.pushed += 1;
        Ok(idx)
    }

    /// 受信ソケットに来た 1 データグラムを処理する。JOIN・ACK・BYE だけを受ける（正しいタグで、観戦用のコードの
    /// session のもの）。返事（上限のときの BYE）は `out` へ。記録は `poll` が送る。
    pub fn on_datagram<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        out: &mut O,
    ) -> HostEvent {
        let refuse = |c: &mut HostCounts, r: DropReason| {
            match r {
                DropReason::NotHgrt => c.not_hgrt += 1,
                DropReason::WrongSession => c.wrong_session += 1,
                DropReason::BadTag => c.bad_tag += 1,
                DropReason::Undecodable => c.undecodable += 1,
                DropReason::OtherVersion(_) => c.other_version += 1,
                DropReason::OtherBuild(_) => c.other_build += 1,
                DropReason::WeakKey => c.weak_key += 1,
                DropReason::Unexpected => c.unexpected += 1,
                DropReason::UnknownSpectator => c.unknown_spectator += 1,
                DropReason::Broken => {}
            }
            HostEvent::Dropped(r)
        };
        if wire::classify(bytes) != Datagram::Hgrt {
            return refuse(&mut self.counts, DropReason::NotHgrt);
        }
        if wire::peek_session(bytes).is_some_and(|s| s != self.session_id) {
            return refuse(&mut self.counts, DropReason::WrongSession);
        }
        let slot_of = |slots: &[Option<Watching>; MAX_SPECTATORS]| {
            // 観戦者の枠は 600 B ほどある。コピーせず参照で見る。
            slots
                .iter()
                .position(|w| w.as_ref().is_some_and(|w| w.addr == from))
        };
        // 封じた便（ACK・BYE）は、JOIN した観戦者の送り元から来たものだけ、その観戦者の鍵で開く。
        let open = slot_of(&self.slots)
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .map(|w| w.keys.opener());
        let pkt = match wire::decode(&self.key, open.as_ref(), bytes) {
            Ok(p) => p,
            Err(DecodeError::BadTag) => return refuse(&mut self.counts, DropReason::BadTag),
            Err(DecodeError::BadVersion(v)) => {
                return refuse(&mut self.counts, DropReason::OtherVersion(v));
            }
            // JOIN していない送り元の封じた便（鍵が無い）。
            Err(DecodeError::NotKeyed) => {
                return refuse(&mut self.counts, DropReason::UnknownSpectator);
            }
            Err(_) => return refuse(&mut self.counts, DropReason::Undecodable),
        };
        match pkt.msg {
            Message::SpectateJoin(j) => {
                if j.build != self.build {
                    return refuse(&mut self.counts, DropReason::OtherBuild(j.build));
                }
                let known = slot_of(&self.slots);
                // 同じ観戦者の JOIN の再送は、鍵の計算（X25519）の前に返す。ここは対戦相手の便も渡す受信スレッドの上なので、
                // HMAC を通った JOIN を何度送られても重い計算を繰り返さない。
                if let Some(w) = known
                    .and_then(|i| self.slots.get_mut(i))
                    .and_then(Option::as_mut)
                    && w.nonce == j.nonce
                    && w.public == j.public
                {
                    w.last_heard = now;
                    return HostEvent::Refreshed;
                }
                if known.is_none() {
                    if self.broken {
                        return refuse(&mut self.counts, DropReason::Broken);
                    }
                    if self.slots.iter().all(Option::is_some) {
                        // 満杯の断り。BYE を封じる鍵の計算が要るので、ホスト全体で `REFUSE_GAP_MS` に 1 回だけ答える
                        // （観戦者は 250 ms ごとに JOIN を送り直すので次の機会に BYE を受ける）。送り元ごとにせず全体に
                        // したのは、送り元を変えながら送られても計算の上限が変わらないため。
                        self.counts.refused_full += 1;
                        if self
                            .last_refusal
                            .is_some_and(|t| elapsed(now, t) < REFUSE_GAP_MS)
                        {
                            self.counts.refused_quiet += 1;
                            return HostEvent::Refused;
                        }
                        self.last_refusal = Some(now);
                        let Some(keys) = self.agree(&j.public) else {
                            return refuse(&mut self.counts, DropReason::WeakKey);
                        };
                        // 上限の断りも、この JOIN の公開鍵から作った鍵で封じる。
                        emit_sealed(
                            &keys,
                            Sender::SpectateHost,
                            &self.seq,
                            self.session_id,
                            from,
                            &Message::Bye(ByeReason::Cancelled),
                            out,
                        );
                        return HostEvent::Refused;
                    }
                }
                // 観戦者との向きごとの鍵（ホストが「前」）。弱い点なら入れない。
                let Some(keys) = self.agree(&j.public) else {
                    return refuse(&mut self.counts, DropReason::WeakKey);
                };
                let name = Name::new(
                    std::str::from_utf8(j.name.get(..usize::from(j.name_len)).unwrap_or(&[]))
                        .unwrap_or(""),
                );
                let id = Joiner {
                    addr: from,
                    nonce: j.nonce,
                    public: j.public,
                    keys,
                };
                let start = self.seg_start.max(self.head);
                if let Some(w) = known
                    .and_then(|i| self.slots.get_mut(i))
                    .and_then(Option::as_mut)
                {
                    // 観戦者が起こし直した: 流れをいまの seg の始まりからやり直す（鍵も新しい公開鍵で作り直した）。
                    *w = Self::fresh(id, name, start, now);
                    self.counts.joined += 1;
                    return HostEvent::Joined;
                }
                match self.slots.iter_mut().find(|w| w.is_none()) {
                    Some(free) => {
                        *free = Some(Self::fresh(id, name, start, now));
                        self.counts.joined += 1;
                        HostEvent::Joined
                    }
                    // 満杯は上で返してある（ここへは来ない）。
                    None => HostEvent::Refused,
                }
            }
            Message::SpectateAck { nonce, next, sack } => {
                let tail = self.next;
                let Some(w) = slot_of(&self.slots)
                    .and_then(|i| self.slots.get_mut(i))
                    .and_then(Option::as_mut)
                    .filter(|w| w.nonce == nonce)
                else {
                    return refuse(&mut self.counts, DropReason::UnknownSpectator);
                };
                w.last_heard = now;
                self.counts.acks += 1;
                // 積んだより先は嘘（受け取れるはずが無い）。積んだ所へ詰める。範囲（`sack`）も当てにならないので読まない。
                let lied = next > tail;
                let next = if lied {
                    self.counts.acks_clamped += 1;
                    tail
                } else {
                    next
                };
                let stale = next < w.ack;
                let progress = next > w.ack;
                let (old_ack, old_sack) = (w.ack, w.sack);
                if progress {
                    w.sack = w.sack.checked_shr(next - w.ack).unwrap_or(0);
                    w.ack = next;
                    w.sent_to = w.sent_to.max(next);
                }
                // 戻った ack（順が入れ替わって遅れて着いた古い ACK）の範囲は、今の `ack` の後ろの番号と合わない。読まない。
                if !stale && !lied {
                    // 送っていない記録を観戦者が受け取れているはずは無い。そのビットは捨てる（嘘か、壊れた実装）。
                    let sent = w
                        .sent_to
                        .saturating_sub(w.ack)
                        .saturating_sub(1)
                        .min(SACK_BITS);
                    let valid = (1u64 << sent) - 1;
                    if sack & !valid != 0 {
                        self.counts.sacks_clamped += 1;
                    }
                    w.sack |= sack & valid;
                }
                w.sample_new(old_ack, old_sack, now);
                HostEvent::Acked { progress }
            }
            Message::Bye(_) => match slot_of(&self.slots) {
                Some(i) => {
                    if let Some(w) = self.slots.get_mut(i) {
                        *w = None;
                    }
                    self.counts.left += 1;
                    HostEvent::Left
                }
                None => refuse(&mut self.counts, DropReason::UnknownSpectator),
            },
            _ => refuse(&mut self.counts, DropReason::Unexpected),
        }
    }

    /// 観戦者の公開鍵との向きごとの鍵（ホストが「前」）。計算（X25519）の回数を数える。
    fn agree(&mut self, public: &[u8; PUBLIC_LEN]) -> Option<DirKeys> {
        self.counts.key_agreements += 1;
        self.eph.dir_keys(public, true, self.session_id, &self.key)
    }

    /// 入ったばかりの観戦者。すぐ 1 通目（記録か空の合図）が出るよう、`last_send` を合図の間隔だけ前にする。
    fn fresh(id: Joiner, name: Name, start: u32, now: u32) -> Watching {
        Watching {
            addr: id.addr,
            nonce: id.nonce,
            public: id.public,
            keys: id.keys,
            ack: start,
            base: start,
            sent_to: start,
            last_send: now.wrapping_sub(HEARTBEAT_MS),
            rtt: Rtt::NEW,
            rto_scale: 1,
            sack: 0,
            sent: [Sent::NEVER; SEND_WINDOW as usize],
            last_heard: now,
            name,
        }
    }

    /// 送るものがあれば送る（外す観戦者も決める）。受信スレッドの周回で 1 ms ごとくらいに呼ぶ。
    pub fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        // 観戦者の枠を書き換えながら記録を読めるよう、ホストの側の読み取りだけの部品を枠から分けて借りる。
        let feed = Feed {
            ring: &self.ring,
            head: self.head,
            next: self.next,
            seq: &self.seq,
            session_id: self.session_id,
        };
        let counts = &mut self.counts;
        for slot in self.slots.iter_mut() {
            let Some(w) = slot.as_mut() else {
                continue;
            };
            if elapsed(now, w.last_heard) >= SILENT_MS {
                *slot = None;
                counts.silenced += 1;
                continue;
            }
            let end = feed.next.min(w.ack.saturating_add(SEND_WINDOW));
            if w.sent_to < end {
                let mut at = w.sent_to;
                while at < end {
                    let n = (end - at).min(MAX_STREAM_RECORDS as u32);
                    feed.send(w, at, n, out);
                    w.mark_sent(at, n, now);
                    counts.packets_sent += 1;
                    counts.records_sent += u64::from(n);
                    at += n;
                }
                w.sent_to = end;
                w.last_send = now;
            }
            if w.ack < w.sent_to {
                w.resend_due(&feed, now, out, counts);
            } else if elapsed(now, w.last_send) >= HEARTBEAT_MS {
                // 何も無いまま静か: 生きている合図（空の STREAM）。
                feed.send(w, w.ack, 0, out);
                counts.packets_sent += 1;
                counts.heartbeats += 1;
                w.last_send = now;
            }
        }
    }
}

/// 観戦者へ記録を送るのに要る、ホストの側の読み取りだけの部品（`poll` が観戦者の枠を書き換えながら使えるよう、枠から分けた借り物）。
struct Feed<'a> {
    ring: &'a [Record],
    head: u32,
    next: u32,
    seq: &'a Seq,
    session_id: u32,
}

impl Feed<'_> {
    fn record_at(&self, idx: u32) -> Option<Record> {
        if idx < self.head || idx >= self.next {
            return None;
        }
        self.ring.get(idx as usize % self.ring.len()).copied()
    }

    /// 番号 `first` から `count` 個（`MAX_STREAM_RECORDS` 以下）の STREAM を 1 通送る。
    fn send<O: Outlet>(&self, w: &Watching, first: u32, count: u32, out: &mut O) {
        let mut st = SpectateStream {
            first,
            reset: first == w.base,
            count: count.min(MAX_STREAM_RECORDS as u32) as u8,
            records: [Record::FILLER; MAX_STREAM_RECORDS],
        };
        for (k, slot) in st
            .records
            .iter_mut()
            .take(usize::from(st.count))
            .enumerate()
        {
            *slot = self
                .record_at(first.saturating_add(k as u32))
                .unwrap_or(Record::FILLER);
        }
        emit_sealed(
            &w.keys,
            Sender::SpectateHost,
            self.seq,
            self.session_id,
            w.addr,
            &Message::SpectateStream(st),
            out,
        );
    }

    /// 送り直す記録 `due`（ビット k = 番号 `w.ack + k`）を送る。続く番号は 1 通に詰める（`MAX_STREAM_RECORDS` まで）。
    fn resend<O: Outlet>(
        &self,
        w: &mut Watching,
        due: u64,
        now: u32,
        out: &mut O,
        counts: &mut HostCounts,
    ) {
        let mut k = 0u32;
        while k < SEND_WINDOW {
            if (due >> k) & 1 == 0 {
                k += 1;
                continue;
            }
            let mut n = 1;
            while n < MAX_STREAM_RECORDS as u32 && k + n < SEND_WINDOW && (due >> (k + n)) & 1 == 1
            {
                n += 1;
            }
            self.send(w, w.ack + k, n, out);
            w.mark_resent(k, n, now);
            counts.packets_sent += 1;
            counts.records_sent += u64::from(n);
            counts.records_resent += u64::from(n);
            counts.resends += 1;
            k += n;
        }
    }
}

impl Watching {
    /// この ACK で初めて受け取れたと分かった記録のうち、1 回しか送っていないものの最も古いものから、往復の標本を 1 つ取る
    /// （`old_ack` / `old_sack` は ACK を重ねる前の値）。送り直した記録は取らない（どの送りへの返事か分からない —— Karn）。
    fn sample_new(&mut self, old_ack: u32, old_sack: u64, now: u32) {
        let top = self.sent_to.min(old_ack.saturating_add(SEND_WINDOW));
        let mut oldest: Option<u32> = None;
        let mut idx = old_ack;
        while idx < top {
            let was = idx > old_ack && (old_sack >> (idx - old_ack - 1)) & 1 == 1;
            let is =
                idx < self.ack || (idx > self.ack && (self.sack >> (idx - self.ack - 1)) & 1 == 1);
            if is
                && !was
                && let Some(sent) = self.sent.get(Sent::index(idx))
                && sent.times == 1
            {
                let age = elapsed(now, sent.at);
                oldest = Some(oldest.map_or(age, |o| o.max(age)));
            }
            idx += 1;
        }
        if let Some(age) = oldest {
            self.rtt.sample(age);
            self.rto_scale = 1;
        }
    }

    fn mark_sent(&mut self, first: u32, n: u32, now: u32) {
        for q in first..first + n {
            if let Some(sent) = self.sent.get_mut(Sent::index(q)) {
                *sent = Sent { at: now, times: 1 };
            }
        }
    }

    fn mark_resent(&mut self, k: u32, n: u32, now: u32) {
        for q in k..k + n {
            if let Some(sent) = self.sent.get_mut(Sent::index(self.ack + q)) {
                sent.at = now;
                sent.times = sent.times.saturating_add(1);
            }
        }
    }

    /// 時間切れの記録（観戦者がまだ受け取れていないもので、最後に送ってから `rto` × 2^(送った回数 − 1) 経ったもの）を送り直す。
    /// 受け取れた範囲（`sack`）の中の記録は送らない。
    fn resend_due<O: Outlet>(
        &mut self,
        feed: &Feed<'_>,
        now: u32,
        out: &mut O,
        counts: &mut HostCounts,
    ) {
        let span = self.sent_to.saturating_sub(self.ack).min(SEND_WINDOW);
        let received = self.sack << 1; // ビット k = 番号 ack + k（k = 0 は連続の終わり = 受け取れていない）
        let rto = self.rtt.rto();
        let (mut missing, mut due, mut fresh) = (0u64, 0u64, 0u64);
        for k in 0..span {
            if (received >> k) & 1 == 1 {
                continue;
            }
            missing |= 1 << k;
            let sent = self
                .sent
                .get(Sent::index(self.ack + k))
                .copied()
                .unwrap_or(Sent::NEVER);
            let tries = u32::from(sent.times).max(1);
            // 送るたびに 2 倍（黙った観戦者へ連打しない）。倍率 `rto_scale` の方が大きければそちらに合わせる。
            let timeout = rto
                .saturating_mul((1u32 << (tries - 1).min(8)).max(self.rto_scale))
                .min(RESEND_MAX_MS);
            if elapsed(now, sent.at) >= timeout {
                due |= 1 << k;
                if tries == 1 {
                    fresh |= 1 << k;
                }
            }
        }
        // 返事（ACK）が `DARK_MS` 聞こえない間は、記録ごとの時計をやめ、先頭の穴の時計だけで先最初から 1 通ぶん送る
        // （連打を避ける。モジュール doc）。返事が戻れば記録ごとの時計に戻る。
        if elapsed(now, self.last_heard) >= DARK_MS {
            due = if due & 1 == 1 {
                missing & FIRST_PACKET
            } else {
                0
            };
            fresh &= due;
        }
        if due == 0 {
            return;
        }
        feed.resend(self, due, now, out, counts);
        self.last_send = now;
        if fresh != 0 {
            self.rto_scale = self.rto_scale.saturating_mul(2).min(RTO_SCALE_MAX);
        }
    }
}

// ============================ 観戦者 ============================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Received {
    pub stored: u8,
    /// 持っている（重なり・重複）か、消費済みの記録の数。
    pub dup: u8,
    /// 置き場の外（先すぎる）で捨てた記録の数。
    pub beyond: u8,
    /// `reset` に従って、いまの seg の始まりへ飛んだ。
    pub jumped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AsmCounts {
    pub stored: u64,
    pub dup: u64,
    pub beyond: u64,
    pub jumps: u64,
}

/// 観戦者の組み立て: 届いた STREAM から記録を番号順に並べ、使う側へ 1 つずつ渡す。持つ範囲は `head`（次に渡す番号）から
/// `head + CAP + SEND_WINDOW` まで。`run_end`（連続で受け取れた終わり）は `head + CAP` を越えない: 使う側が遅れて CAP 個
/// たまったら ack を止める。ホストは ack から `SEND_WINDOW` までしか送らないので、その分の置き場は常にある。
pub struct Assembler {
    slots: Vec<Record>,
    present: Vec<bool>,
    cap: u32,
    head: u32,
    /// 連続で受け取った終わり（`head` ≤ これ ≤ `head + cap`）。次に要る番号（ack）。
    run_end: u32,
    counts: AsmCounts,
}

impl Assembler {
    pub fn new() -> Self {
        Self::with_capacity(ASM_CAP)
    }

    /// 検査の小さい置き場（0 は 1 に上げる）。
    pub fn with_capacity(cap: usize) -> Self {
        let cap = cap.max(1);
        let store = cap + SEND_WINDOW as usize;
        Self {
            slots: vec![Record::FILLER; store],
            present: vec![false; store],
            cap: cap as u32,
            head: 0,
            run_end: 0,
            counts: AsmCounts::default(),
        }
    }

    pub fn ack(&self) -> u32 {
        self.run_end
    }

    /// 受け取れた範囲（ACK の `sack`）: `ack()` の後ろで受け取った記録。ビット i = 番号 `ack() + 1 + i`。確保しない。
    pub fn sack(&self) -> u64 {
        let store = self.store_len();
        // 置き場が持てるのは head から store 個。それより先は置いていない（`on_stream` が捨てる）。
        let limit = self.head.saturating_add(store);
        let mut bits = 0u64;
        for i in 0..SACK_BITS {
            let idx = self.run_end.saturating_add(1 + i);
            if idx >= limit {
                break;
            }
            if self
                .present
                .get((idx % store) as usize)
                .copied()
                .unwrap_or(false)
            {
                bits |= 1 << i;
            }
        }
        bits
    }

    pub fn pending(&self) -> usize {
        (self.run_end - self.head) as usize
    }

    pub fn counts(&self) -> AsmCounts {
        self.counts
    }

    fn store_len(&self) -> u32 {
        self.slots.len() as u32
    }

    fn advance(&mut self) {
        let limit = self.head.saturating_add(self.cap);
        while self.run_end < limit {
            let i = (self.run_end % self.store_len()) as usize;
            if self.present.get(i).copied().unwrap_or(false) {
                self.run_end += 1;
            } else {
                break;
            }
        }
    }

    /// 届いた STREAM を入れる。
    ///
    /// - `reset` が付いていて `first` が連続の終わりより先なら、そこへ飛ぶ（持っているものを捨てる）。付いていない
    ///   先の番号は、順の入れ替わりで先に着いただけかもしれないので、飛ばずに置き場へ（範囲の外なら捨てる）。
    /// - 連続の終わりより前の番号は重複（捨てる。渡した分も含む）。
    pub fn on_stream(&mut self, s: &SpectateStream) -> Received {
        let mut r = Received::default();
        if s.reset && s.first > self.run_end {
            // 飛び先より前の記録は捨てる。飛び先より先に順が入れ替わって着いていた記録は残す: `sack` でホストへ知らせてあるので、
            // 捨てるとホストが送り直さない。
            let upto = s.first.min(self.head.saturating_add(self.store_len()));
            for idx in self.head..upto {
                let i = (idx % self.store_len()) as usize;
                if let Some(p) = self.present.get_mut(i) {
                    *p = false;
                }
            }
            self.head = s.first;
            self.run_end = s.first;
            self.counts.jumps += 1;
            r.jumped = true;
        }
        let limit = self.head.saturating_add(self.store_len());
        for (k, rec) in s
            .records
            .iter()
            .take(usize::from(s.count).min(MAX_STREAM_RECORDS))
            .enumerate()
        {
            let Some(idx) = s.first.checked_add(k as u32) else {
                r.beyond = r.beyond.saturating_add(1);
                self.counts.beyond += 1;
                continue;
            };
            if idx < self.run_end {
                r.dup = r.dup.saturating_add(1);
                self.counts.dup += 1;
                continue;
            }
            if idx >= limit {
                r.beyond = r.beyond.saturating_add(1);
                self.counts.beyond += 1;
                continue;
            }
            let i = (idx % self.store_len()) as usize;
            if self.present.get(i).copied().unwrap_or(false) {
                r.dup = r.dup.saturating_add(1);
                self.counts.dup += 1;
                continue;
            }
            if let (Some(slot), Some(p)) = (self.slots.get_mut(i), self.present.get_mut(i)) {
                *slot = *rec;
                *p = true;
                r.stored = r.stored.saturating_add(1);
                self.counts.stored += 1;
            }
        }
        self.advance();
        r
    }

    /// 次の記録を 1 つ渡す（番号つき）。番号が飛ぶのは、`reset` に従って、いまの seg の始まりへ飛んだ直後だけ。
    pub fn pop_indexed(&mut self) -> Option<(u32, Record)> {
        if self.head >= self.run_end {
            return None;
        }
        let idx = self.head;
        let i = (idx % self.store_len()) as usize;
        let rec = self.slots.get(i).copied()?;
        if let Some(p) = self.present.get_mut(i) {
            *p = false;
        }
        self.head += 1;
        self.advance();
        Some((idx, rec))
    }

    pub fn pop(&mut self) -> Option<Record> {
        self.pop_indexed().map(|(_, r)| r)
    }
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    /// 観戦用のコード（`code::Side::Spectate`）の session と秘密。
    pub session_id: u32,
    pub secret: Secret,
    /// 起動ごとに引く乱数（core は乱数を持たない）。
    pub nonce: u64,
    pub name: Name,
    /// ホストの候補アドレス（観戦用のコードから）。JOIN はここへ送る。最初の STREAM を受けるまではここから来たものを受け、
    /// ここに無い送り元はタグの合う STREAM だけを受ける（そのアドレスをホストに採る）。最初の STREAM の後は、そのアドレスだけ。
    pub host: Candidates,
    /// ホストの観戦用の公開鍵（観戦用のコードから）。
    pub host_public: [u8; PUBLIC_LEN],
    /// 観戦者の使い捨ての鍵の種（`Watcher` を作る = JOIN するたびに呼び手が OS の乱数で引く）。
    pub key_seed: KeySeed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WatcherCounts {
    pub joins_sent: u64,
    pub acks_sent: u64,
    pub streams: u64,
    pub foreign: u64,
    pub not_hgrt: u64,
    pub wrong_session: u64,
    pub bad_tag: u64,
    pub undecodable: u64,
    /// タグは合うが、ヘッダのバージョンが違うホストから来たもの（ふつうは来ない）。
    pub other_version: u64,
    pub unexpected: u64,
    pub byes: u64,
    /// ホストの公開鍵が弱い点で鍵を作れず、封じた便を開かずに捨てた数（壊れたコードしか作れない）。
    pub no_keys: u64,
    /// 最初の STREAM が観戦用のコードの候補に無い送り元から来て、そのアドレスをホストに採った回数（0 か 1）。
    pub learned_host: u64,
}

pub struct Watcher {
    session_id: u32,
    key: AuthKey,
    public: [u8; PUBLIC_LEN],
    /// ホストとの向きごとの鍵（観戦者は「後ろ」。ホストの公開鍵が弱い点なら None）。
    keys: Option<DirKeys>,
    /// 送り手 `Sender::Spectator` の通し番号（秘密鍵と同じ寿命）。
    seq: Seq,
    /// 自分のビルド番号（JOIN に載せる。既定は `update::BUILD_NUMBER`）。
    build: u32,
    nonce: u64,
    name: Name,
    host: Candidates,
    /// 最初の STREAM を送ってきた送り元（以後はここだけ）。
    host_addr: Option<SocketAddr>,
    asm: Assembler,
    last_join: Option<u32>,
    last_heard: Option<u32>,
    /// 最後に送った ack（消費で `Assembler::ack` が進んだら送り直す）。
    last_ack_sent: u32,
    /// ホストの最後の BYE（上限のときの取消など）。次の STREAM が来たら消す。
    host_bye: Option<ByeReason>,
    counts: WatcherCounts,
}

impl Watcher {
    pub fn new(cfg: WatcherConfig) -> Self {
        Self::with_assembler(cfg, Assembler::new())
    }

    pub fn with_assembler(cfg: WatcherConfig, asm: Assembler) -> Self {
        let key = AuthKey::derive(&cfg.secret, cfg.session_id);
        // 鍵はホストの公開鍵（観戦用のコード）からここで作れる（X25519 と HKDF を 1 回）。
        let eph = Ephemeral::from_seed(&cfg.key_seed);
        let keys = eph.dir_keys(&cfg.host_public, false, cfg.session_id, &key);
        Self {
            session_id: cfg.session_id,
            key,
            public: eph.public(),
            keys,
            seq: Seq::new(),
            build: crate::update::BUILD_NUMBER,
            nonce: cfg.nonce,
            name: cfg.name,
            host: cfg.host,
            host_addr: None,
            asm,
            last_join: None,
            last_heard: None,
            last_ack_sent: 0,
            host_bye: None,
            counts: WatcherCounts::default(),
        }
    }

    pub fn is_joined(&self) -> bool {
        self.host_addr.is_some()
    }

    /// 検査が違うビルドの観戦者を作るため。既定は `update::BUILD_NUMBER`。
    pub fn set_build(&mut self, build: u32) {
        self.build = build;
    }

    pub fn public_key(&self) -> [u8; PUBLIC_LEN] {
        self.public
    }

    /// ホストとの向きごとの鍵（**検査と診断のためだけ**。観戦者が送った便を外から開く・ホスト役の検査が封じて渡す）。
    #[doc(hidden)]
    pub fn dir_keys(&self) -> Option<DirKeys> {
        self.keys
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.next()
    }

    pub fn host_addr(&self) -> Option<SocketAddr> {
        self.host_addr
    }

    /// ホストが最後に送った BYE（JOIN を断られたときなど）。次の STREAM を受けると消える。
    pub fn host_bye(&self) -> Option<ByeReason> {
        self.host_bye
    }

    /// 最後に何かを聞いてからの ms（何も聞いていなければ `None`）。使う側が「ホストが黙った」を決める。
    pub fn silent_ms(&self, now: u32) -> Option<u32> {
        self.last_heard.map(|t| elapsed(now, t))
    }

    pub fn ack(&self) -> u32 {
        self.asm.ack()
    }

    pub fn sack(&self) -> u64 {
        self.asm.sack()
    }

    pub fn pending(&self) -> usize {
        self.asm.pending()
    }

    pub fn counts(&self) -> WatcherCounts {
        self.counts
    }

    pub fn assembler_counts(&self) -> AsmCounts {
        self.asm.counts()
    }

    pub fn pop_indexed(&mut self) -> Option<(u32, Record)> {
        self.asm.pop_indexed()
    }

    pub fn pop(&mut self) -> Option<Record> {
        self.asm.pop()
    }

    /// 観戦を抜ける（BYE を 1 通。ホストは観戦者の枠を空ける）。dll の観戦者が、見終わった・続けられなくなったときに
    /// 枠を返す。最初の STREAM を受けるまで（ホストのアドレスが定まるまで）は何も送らない。
    pub fn bye<O: Outlet>(&mut self, reason: ByeReason, out: &mut O) {
        if let (Some(a), Some(k)) = (self.host_addr, self.keys) {
            emit_sealed(
                &k,
                Sender::Spectator,
                &self.seq,
                self.session_id,
                a,
                &Message::Bye(reason),
                out,
            );
        }
    }

    fn send_ack<O: Outlet>(&mut self, to: SocketAddr, out: &mut O) {
        let next = self.asm.ack();
        let Some(k) = self.keys else {
            return;
        };
        emit_sealed(
            &k,
            Sender::Spectator,
            &self.seq,
            self.session_id,
            to,
            &Message::SpectateAck {
                nonce: self.nonce,
                next,
                sack: self.asm.sack(),
            },
            out,
        );
        self.last_ack_sent = next;
        self.counts.acks_sent += 1;
    }

    /// 受信ソケットに来た 1 データグラムを処理する。STREAM を組み立てに入れ、すぐ ack を返す。
    pub fn on_datagram<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        out: &mut O,
    ) {
        let c = &mut self.counts;
        if wire::classify(bytes) != Datagram::Hgrt {
            c.not_hgrt += 1;
            return;
        }
        // ホストのアドレスが決まったあとは、そのアドレスだけ。決まる前は、観戦用のコードの候補から来たものを受け、候補に無い送り元
        // （`probation`）はタグの合う STREAM だけを受けてそのアドレスをホストに採る。それ以外は `foreign` で捨てる。
        if self.host_addr.is_some_and(|a| a != from) {
            c.foreign += 1;
            return;
        }
        let probation = self.host_addr.is_none() && !self.host.contains(from);
        if wire::peek_session(bytes).is_some_and(|s| s != self.session_id) {
            if probation {
                c.foreign += 1;
            } else {
                c.wrong_session += 1;
            }
            return;
        }
        let open = self.keys.map(|k| k.opener());
        let pkt = match wire::decode(&self.key, open.as_ref(), bytes) {
            Ok(p) => p,
            Err(e) => {
                if probation {
                    c.foreign += 1;
                } else {
                    match e {
                        DecodeError::BadTag => c.bad_tag += 1,
                        DecodeError::BadVersion(_) => c.other_version += 1,
                        DecodeError::NotKeyed => c.no_keys += 1,
                        _ => c.undecodable += 1,
                    }
                }
                return;
            }
        };
        match pkt.msg {
            Message::SpectateStream(s) => {
                c.streams += 1;
                if probation {
                    c.learned_host += 1;
                }
                self.host_addr = Some(from);
                self.last_heard = Some(now);
                self.host_bye = None;
                self.asm.on_stream(&s);
                self.send_ack(from, out);
            }
            _ if probation => c.foreign += 1,
            Message::Bye(r) => {
                c.byes += 1;
                self.last_heard = Some(now);
                self.host_bye = Some(r);
            }
            _ => c.unexpected += 1,
        }
    }

    /// JOIN の送り直しと、消費で進んだ ack の通知。使う側の周回で 1 ms ごとくらいに呼ぶ。
    pub fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        // JOIN: まだ流れが来ない間と、来ていたのが途切れた間（ホストが外したかもしれない）。
        let wants_join = match (self.host_addr, self.last_heard) {
            (Some(_), Some(t)) => elapsed(now, t) >= REJOIN_MS,
            _ => true,
        };
        if wants_join
            && self
                .last_join
                .is_none_or(|t| elapsed(now, t) >= JOIN_RESEND_MS)
        {
            let mut name = [0u8; NAME_MAX];
            let bytes = self.name.as_bytes();
            for (d, s) in name.iter_mut().zip(bytes.iter()) {
                *d = *s;
            }
            let join = Message::SpectateJoin(SpectateJoin {
                name_len: bytes.len().min(NAME_MAX) as u8,
                nonce: self.nonce,
                name,
                build: self.build,
                public: self.public,
            });
            match self.host_addr {
                Some(a) => emit_signed(&self.key, self.session_id, a, &join, out),
                None => {
                    for c in self.host.as_slice() {
                        emit_signed(&self.key, self.session_id, c.addr, &join, out);
                    }
                }
            }
            self.last_join = Some(now);
            self.counts.joins_sent += 1;
        }
        // 消費で置き場が空き、ack が進んだ（流れの制御で止めていた分）。
        if let Some(a) = self.host_addr
            && self.asm.ack() != self.last_ack_sent
        {
            self.send_ack(a, out);
        }
    }
}

// ============================ 穴あけ ============================

/// 穴あけのパケット（4 B）。`HGRT`（`wire::MAGIC`）で始まらないので、受け手は `Datagram::Hgrt` でない 1 通として捨てる
/// （STUN の形でもない）。
pub const PUNCH_PACKET: [u8; 4] = *b"HGRp";
/// 穴あけを送る間隔（ms）。観戦者の JOIN の送り直し（250 ms）に近く、NAT の対応の保ち（UDP で普通は 30 秒以上）より十分短い値。
pub const PUNCH_INTERVAL_MS: u32 = 200;
/// 各候補へ穴あけを送る回数（最初の 1 通はすぐ。5 回で 200 ms × 4 = 0.8 秒）。観戦者の JOIN が 250 ms ごとなので、その間に
/// 少なくとも 3 回は JOIN が通る機会がある。
pub const PUNCH_ROUNDS: u8 = 5;

/// 穴あけの予定: 観戦者の候補の並び（`MAX_CANDIDATES` 以下）の各アドレスへ、[`PUNCH_INTERVAL_MS`] おきに [`PUNCH_ROUNDS`] 回、
/// [`PUNCH_PACKET`] を送る。配る側の受信スレッドが観戦者の返しを読んだとき作り、周回で [`PunchPlan::poll`] を呼ぶ。
/// そのアドレスから JOIN が来たら [`PunchPlan::stop_for`] でそのアドレスへの分を止める。確保しない。
#[derive(Debug, Clone, Copy)]
pub struct PunchPlan {
    /// まだ送るアドレス（止めたアドレス・空きは `None`）。
    targets: [Option<SocketAddr>; MAX_CANDIDATES],
    round: u8,
    next_due: u32,
}

impl PunchPlan {
    pub fn new(cands: &Candidates, now: u32) -> Self {
        let mut targets = [None; MAX_CANDIDATES];
        for (t, c) in targets.iter_mut().zip(cands.as_slice()) {
            *t = Some(c.addr);
        }
        Self {
            targets,
            round: 0,
            next_due: now,
        }
    }

    /// 穴あけをしない予定（検査・まだ観戦者の返しが来ていない間の置き場）。
    pub fn none() -> Self {
        Self {
            targets: [None; MAX_CANDIDATES],
            round: PUNCH_ROUNDS,
            next_due: 0,
        }
    }

    /// 時刻になっていれば、止めていない全候補へ 1 通ずつ送る。送った通数を返す。遅れて呼ばれても 1 回に 1 周だけ送り
    /// （追いつこうとして続けて送らない）、次はいまから [`PUNCH_INTERVAL_MS`] 後。
    pub fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) -> usize {
        let reached = now.wrapping_sub(self.next_due) <= u32::MAX / 2;
        if self.round >= PUNCH_ROUNDS || !reached {
            return 0;
        }
        let mut sent = 0;
        for a in self.targets.iter().flatten() {
            out.send(*a, &PUNCH_PACKET);
            sent += 1;
        }
        self.round += 1;
        self.next_due = now.wrapping_add(PUNCH_INTERVAL_MS);
        sent
    }

    /// アドレス `from` から JOIN が来た: そのアドレスへの穴あけを止める。止めた（予定にあった）なら true。
    pub fn stop_for(&mut self, from: SocketAddr) -> bool {
        let mut stopped = false;
        for t in self.targets.iter_mut() {
            if t.is_some_and(|a| same_endpoint(a, from)) {
                *t = None;
                stopped = true;
            }
        }
        stopped
    }

    pub fn active(&self) -> usize {
        if self.round >= PUNCH_ROUNDS {
            return 0;
        }
        self.targets.iter().flatten().count()
    }

    /// もう送るものが無い（回数を使い切った・全部止めた・候補が無い）。
    pub fn is_done(&self) -> bool {
        self.active() == 0
    }

    pub fn rounds_sent(&self) -> u8 {
        self.round
    }
}
