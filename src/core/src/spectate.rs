//! 観戦の core の部分（SPECTATE_JOIN・SPECTATE_STREAM・SPECTATE_ACK。中継の木は `tree`、中継役を選ぶ上りの測りは `uplink`）。
//!
//! 観戦者はホストか、ホストが紹介した中継役（同じ `SpectateHost` の中継の形）に接続する。ホストは「使うと決まったもの」を**記録**（`wire::Record`）として 1 本の通し番号（u32）に並べ、観戦者へ流す。観戦者は連続で受け取れた所までを ack で返す。
//! **seg**: dll の並びは「`Seg(0)` → 名前（`Name` × 2）→ 選択の記録 → マッチの語 → `Seg(1)` → … → `End`」で、`Seg(k)` は「この後の記録は seg k のもの」の宣言（[`seg_head`]）。遅れて JOIN した観戦者は、最後に選択の記録（`Menu`・`Step`・`Select`）があった seg の始まりから流す（再戦の seg には選択が無く、そこから始めると観戦者のゲームが次の選択まで何も映せない）。その seg がリングバッファから落ちたか `REWIND_MAX` より前なら、いまの seg の始まりから。リングバッファが埋まったときだけ、いまの seg の始まりより前を捨て、捨てられた所にいた観戦者はいまの seg の始まりへ**飛ぶ**（`SpectateStream::reset`）。1 つの seg だけで埋まると、その seg は諦める（観戦者を全員外し、次の `Seg` まで記録を受けない）。
//! **送り**: 観戦者ごとに `ack`・`sent_to`・`sack` を持ち、ウィンドウは `ack` から `SEND_WINDOW` 記録。新しい分はすぐ送り、ホストは**届いていない記録だけ**を送り直す（記録ごとの時計と、後から送った記録が届いたのに届かない穴の速い送り直し。RFC 8985 の RACK・往復の見積もりは RFC 6298 と Karn）。追いついていない観戦者（積んだ終わりから `BEHIND_RECORDS` より後ろ）への送りは通数と量の上限（`CATCHUP_PACKETS_PER_POLL`・`CATCHUP_PACKETS_PER_WATCHER`・`spectate_rate` の観戦者ごとと全体の上限）で止め、残りは次の `poll` へ回す。全体の上限は対戦の往復の延び（`note_delay`）で下げる。未確認が無いまま `HEARTBEAT_MS` 経ったら空の STREAM を送り、何も聞かない時間が `SILENT_MS` を越えた観戦者と BYE を送った観戦者は外す。
//! **受け入れと穴あけ**: 観戦者は、最初の STREAM を受けるまでは観戦用のコードの候補から来たものを受け、候補に無い送り元からは「タグ・session の合う STREAM」だけを受けて以後そのアドレスをホストとする（`learned_host`。ホストの NAT が宛先ごとに外側のポートを変える型への備え）。**穴あけ**（[`PunchPlan`]）: ホストが観戦者の候補のアドレスへ 4 B のパケット（[`PUNCH_PACKET`]）を送り、ホスト側の NAT に経路を作って観戦者の JOIN を通す。ACK を返した送り元が観戦者の選んだ経路で、同じ観戦者のほかの枠は外す（`drop_twins`・`twin_is_live`）。
//! **ゲームスレッド**: 1 つのスレッドから呼ぶ前提（`&mut self`）。`push`・`on_datagram`・`poll`・`Watcher` の各関数はヒープを確保せず、パニックしうる添字を使わない（`tests/alloc_check.rs`）。確保するのは `new` だけ。時刻 `now` は ms（折り返しは `wrapping_sub`）。
//! **鍵**: 鍵は対戦と同じ式（`seal::Ephemeral::dir_keys`。「ホスト → 観戦者」が前）で観戦者ごとに違い、JOIN の後の STREAM・ACK・BYE は全部封じる。通し番号はホストに 1 本（`Sender::SpectateHost`）。ビルド番号が違う JOIN は捨てて数える（`DropReason::OtherBuild`）。

use std::net::SocketAddr;

use crate::auth::{AuthKey, Secret};
use crate::bits512::{self, Bits512};
use crate::candidate::{Candidates, MAX_CANDIDATES, same_endpoint};
use crate::handshake::{Established, Name};
use crate::link::Outlet;
use crate::nat::NatMark;
use crate::rtt_probe::{DelaySnapshot, elapsed};
use crate::seal::{DirKeys, Ephemeral, KeySeed, PUBLIC_LEN, Sender, Seq};
use crate::spectate_pace::{FF_UPDATES_DEFAULT, FF_UPDATES_MAX, FF_UPDATES_MIN};
use crate::spectate_rate::{self, Throttle, ThrottleCounts};
use crate::wire::{
    self, ByeReason, Datagram, DecodeError, JoinTree, MAX_DATAGRAM, MAX_STREAM_RECORDS, Message,
    NAME_MAX, PeekedType, Record, RelayState, SACK_BITS, Seat, SpectateJoin, SpectateStream,
};

/// 中継の木（紹介・受け入れの依頼・取り置き・中継役が子へ配る側・観戦者の親の付け替え）。
mod tree;
/// 試合中の観戦者 → ホストの上りの太さの測り（MEASURE・PAIR）。
mod uplink;

pub use tree::{
    ADMIT_QUEUE, ADMIT_TTL_MS, PREPAID_MS, RELAY_CHILDREN, RELAY_HEARTBEAT_MS, RELAY_QUIET_MS,
    RELAYS, RESERVE_MS, RelayKey, TreeCounts, leave_tree, tend_relay,
};
pub use uplink::{
    MEASURE_CLOSE_MS, MEASURE_MIN_PAIRS, MEASURE_PAIRS, MEASURE_RESEND_MS, MEASURE_RETRY_MS,
    MEASURE_ROUNDS, MEASURE_SENDS, PAIR_CAP, PAIR_GAP_MS, PAIR_LEN,
};

/// 観戦者の数の上限。観戦者への送信は受信スレッドが観戦者ごとに 1 通ずつ行い、ホストの上りは
/// 1 人あたり約 3 KB/s（本文。IPv4 のヘッダ込みで約 4.6 KB/s）。10 人でも相手の便の受け取りの遅れに差が出なかった
/// （`dll` の `spectate_host::load` の測り）。
pub const MAX_SPECTATORS: usize = 10;
/// 観戦者の枠の数。同じ観戦者が IPv4 と IPv6 の両方から JOIN すると送り元ごとに 1 枠ずつ使う（ACK を返した方が決まるまで）ので、人数の上限の 2 倍を持つ。
/// 人数は枠でなく別人の数で `MAX_SPECTATORS` に止める（`SpectateHost::spectator_count`）。
const SLOT_CAP: usize = 2 * MAX_SPECTATORS;
/// リングバッファの大きさ（記録の数）。1 seg 60 分のマッチの語（216,000 個）＋ 選択の画面の MENU の語と余白で 2^18。1 記録 36 B
/// （名前の記録が最大。`wire::tests::a_record_stays_within_36_bytes` が見る）なので約 9.4 MB（起動時に 1 回確保）。
pub const RING_CAP: usize = 262_144;
/// 遅れて JOIN した観戦者を、いまの seg の始まりより前の選択の seg まで遡らせてよい記録の数（60 fps で 20 分）。観戦者の早送りは
/// 既定 8 倍（`ff_max`）なので、遡った分を追うのに 3 分近く（16 倍で 80 秒ほど）と再戦ごとの読み込みがかかる（外挿）。越えたら
/// 遡らず、観戦者は次の選択を待つ。
pub const REWIND_MAX: u32 = 72_000;
/// ホストが、観戦者の ack より先へ送ってよい記録の数（ウィンドウ）。往復 300 ms の回線で、早送りの最大の速さ（1 秒に約 1,000 記録）を
/// 切らさず運べる数（ウィンドウ ÷ 往復 ≧ 速さ）。送り直しの表と ACK の受け取れた範囲のビット数もこれ。
pub const SEND_WINDOW: u32 = 512;
// ウィンドウは ack の番号そのものを含むので、ack の後ろで受け取っていてよいのは `SEND_WINDOW - 1` 個まで（ACK の受け取れた範囲の幅）。
const _: () = assert!(SEND_WINDOW - 1 == SACK_BITS);
const _: () = assert!(SEND_WINDOW == bits512::BITS);
/// ack が積んだ終わりからこの数より後ろの観戦者を、追いついていないとみなす（送りの上限 `CATCHUP_PACKETS_PER_*` を掛け、追いついた
/// 観戦者を先に送る）。ウィンドウとは別: ふだんの流れ（1 往復で数記録）と、遅れて入ったときの溜まりを分ける境目。
pub const BEHIND_RECORDS: u32 = 64;
// 往復の延びで止めた送りを戻すまでに、追いついていた観戦者はこの境目より遅れる（戻した後は追いつきの上限の下で送る）。
const _: () = assert!(
    spectate_rate::RESUME_HOLD_MS * spectate_rate::RECORDS_PER_SEC / 1_000 > BEHIND_RECORDS
);
/// 観戦者の ACK が返るペース（ms）。dll の観戦者は入力の地点（ゲームのスレッド）で 1 フレームに 1 回だけ ACK を返す
/// （60 Hz = 16.7 ms を切り上げ）。
pub const ACK_TICK_MS: u32 = 17;
/// 送り直しの間隔の下限（ms）。ACK のペース 2 回ぶん（1 フレームの遅れがあっても、次の ACK までに送り直さない）。
pub const RESEND_MIN_MS: u32 = 2 * ACK_TICK_MS;
/// 往復の見積もりがまだ無いときの送り直しの間隔（ms）。ACK のペース ＋ インターネットの往復（〜80 ms）を見込んだ値。
pub const RESEND_INIT_MS: u32 = 100;
/// 送り直しの間隔の上限（ms）。2 回目からの送り直しの上限は観戦者ごとに往復の見積もり（`Watching::resend_cap`）で、この値で頭打ちに
/// する（往復が 1 秒近い回線でも、届く途中の記録を送り直さない）。往復の標本もこの値で切る（観戦者のゲームの読み込みなどで ACK が
/// 長く止まった 1 回で、見積もりを引き上げすぎない）。
pub const RESEND_MAX_MS: u32 = 1_000;
/// 2 回目からの送り直しの間隔の上限の、下の値（ms）。往復が短い回線ではこれが上限になる（落ちた送り直しを、倍々でもこれより待たせ
/// ない）。
pub const RESEND_CAP_MIN_MS: u32 = 250;
/// 先頭の穴から 1 通ぶん（`MAX_STREAM_RECORDS` 個）を指すビット（`Watching::resend_due` の `due` と同じ並び）。
const FIRST_PACKET: Bits512 = Bits512::low(MAX_STREAM_RECORDS as u32);
/// `Watching::rto_scale` の上限。下限の間隔 × これ が `RESEND_MAX_MS` に届く値（見積もりが短すぎるまま往復が延びても、倍々で往復を越えて
/// 標本が取れるようにする —— Karn）。
const RTO_SCALE_MAX: u32 = 32;
const _: () = assert!(RESEND_MIN_MS * RTO_SCALE_MAX >= RESEND_MAX_MS);
/// 追いついていない観戦者（ack が積んだ終わりから `BEHIND_RECORDS` より後ろ）への送りの、`poll` 1 回あたりの通数の上限（全員で）。
/// `poll` は受信スレッドが対戦の相手の便を受けた後に回すので、1 回の仕事（封じて送る通の数に比例する）とネットワークへ一度に出す量を
/// 抑える。追いついた観戦者への送り（1 記録ずつの新しい分と、その送り直し）は止めない。
/// 8 通は、落としのある長い回線でも既定の早送り（8 倍）を切らさず、受信スレッドの 1 周を短く保つ数（`dll` の `spectate_host::load`
/// が見る）。
pub const CATCHUP_PACKETS_PER_POLL: u32 = 8;
/// 同じく観戦者 1 人あたり（1 人が 1 回の `poll` で全部使える。ウィンドウ 512 記録 = 32 通の 1/4）。
pub const CATCHUP_PACKETS_PER_WATCHER: u32 = 8;
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
/// 同じ枠の観戦者の鍵の作り直し（nonce か公開鍵だけを変えた JOIN）の間隔の下限。受信スレッドでの X25519 を、
/// HMAC を通った JOIN の連打で増やさない。
pub const REKEY_GAP_MS: u32 = 1_000;
/// ホスト全体の鍵の計算（X25519）の上限: `AGREE_WINDOW_MS` の間に `AGREE_BURST` 回まで。送り元を変えながらの JOIN でも
/// 計算の量が変わらない。観戦者は `JOIN_RESEND_MS` ごとに JOIN を送り直すので、次の機会に入れる。
pub const AGREE_BURST: usize = 8;
pub const AGREE_WINDOW_MS: u32 = 1_000;
/// 流れが途切れて（何も聞かないまま）この時間を越えたら、観戦者が JOIN を送り直す（ホストが外した場合に戻る）。
pub const REJOIN_MS: u32 = 3_000;

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

/// 封じる便を作って送り、その長さを返す。通し番号を使い切っていたら送らない（0）。
fn emit_sealed<O: Outlet>(
    keys: &DirKeys,
    sender: Sender,
    seq: &Seq,
    session_id: u32,
    to: SocketAddr,
    msg: &Message,
    out: &mut O,
) -> usize {
    let mut buf = [0u8; MAX_DATAGRAM];
    match wire::seal(&keys.sealer(sender), seq, session_id, msg, &mut buf) {
        Some(n) => {
            out.send(to, buf.get(..n).unwrap_or(&[]));
            n
        }
        None => 0,
    }
}

// ============================ ホスト ============================

/// 新しい seg の始まりとして観戦の流れへ積む 3 つの記録: `Seg(seg)` と、その直後の席ごとの名前（1P → 2P）。名前は接続した 2 人の
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
    /// 中継役が、受け入れの依頼で名指しされていない JOIN を受けた（子へ記録を流すのをやめた・上が黙っている間も）。
    NotAdmitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    Dropped(DropReason),
    /// 新しい観戦者を入れた（同じ送り元が違う nonce で JOIN し直したときも）。
    Joined,
    /// 鍵の計算の上限（`REKEY_GAP_MS`・`AGREE_BURST`）に当たって、計算せずに断った。
    Throttled,
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
    /// 中継役を観戦者へ紹介した（返事待ちの紹介の JOIN の再送には、その観戦者へ紹介を送り直した）。
    Introduced,
    /// 中継役が、子の続きを持っていないので REHOME を返した。
    Rehomed,
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
    /// 鍵の計算の上限で計算せずに断った JOIN の数（同じ枠の作り直しの間隔・ホスト全体の回数）。
    pub join_throttled: u64,
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
    /// 送り直した記録のうち、時計を待たずに送ったもの（後から送った記録が届いたのに届かない穴。`records_resent` の内数）。
    pub records_fast: u64,
    /// 追いついていない観戦者へ送るものがあったのに、送りの上限（通数・量）で次の `poll` へ回した回数（観戦者ごと・`poll` ごとに 1）。
    pub deferred: u64,
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
    /// 同じ観戦者が IPv4 と IPv6 の両方から JOIN して 2 枠になったとき、返事の無い方を外した数（ACK を返した経路が決まった後に届いた、もう片方の
    /// 経路の JOIN を入れなかった数も含む）。
    pub twins_dropped: u64,
    /// 中継役を紹介した数（紹介の送り直しは `intro_resends`）。
    pub intros: u64,
    pub intro_resends: u64,
    /// ホストが中継役へ送る受け入れの依頼の送り直しの数。
    pub admit_resends: u64,
    /// 返事待ちの紹介のうち、`ADMIT_TTL_MS` の内に中継役が受け取らず数えなくなったもの。
    pub intros_expired: u64,
    pub not_admitted: u64,
    /// 中継役が子へ送った REHOME の数。
    pub rehomes_sent: u64,
    /// 戻る観戦者の取り置きを使って入れた数。
    pub reserved_used: u64,
    /// 子孫の数が `RELAY_CHILDREN` を越えた ACK（嘘。詰めて数えた）。
    pub descendants_clamped: u64,
    /// 中継役が、自分の受けた番号の飛びでリングを積み直した回数。
    pub relay_jumps: u64,
    /// 直接の観戦者へ上りの測りの番を渡した数（MEASURE の送り直しは `measure_resends`）。
    pub measures: u64,
    pub measure_resends: u64,
    /// 番を締めて上りの見積もりが決まった数と、決まらなかった数（使える組が足りない・番の途中で抜けた）。
    pub uplinks_measured: u64,
    pub uplinks_unmeasured: u64,
    /// 番の組として受けた PAIR の通数。
    pub pairs: u64,
    /// 捨てた PAIR（番を持つ観戦者の送り元でない・開かない・番号が違う・重なった）。
    pub pairs_dropped: u64,
    /// 木の穴あけの印を送った数。中継役は受け入れる子の候補へ、ホストは外した中継役の子孫へ送る。
    pub punches_sent: u64,
    /// マッチの切れ目に、直接の葉を中継役へ紹介した数（入れ替え。`intros` には数えない）。
    pub swaps: u64,
    /// 入れ替えで、中継役が依頼を受け取ったので空けた直接の枠の数（`silenced` には数えない）。
    pub swap_freed: u64,
}

#[derive(Debug, Clone, Copy)]
struct Watching {
    addr: SocketAddr,
    nonce: u64,
    /// JOIN の公開鍵（同じ送り元・同じ nonce でも、公開鍵が違えば観戦者が起動し直したと見る）。
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
    sack: Bits512,
    /// ウィンドウの中の記録（番号 % `SEND_WINDOW`。ウィンドウは ack から `SEND_WINDOW` なので重ならない）を最後に送った時刻と送った回数。
    sent: [Sent; SEND_WINDOW as usize],
    /// 届いたと分かった記録のうち（1 回しか送っていないもの）、最も後に送った記録の（送った時刻, 番号）と、送ってから届いたと
    /// 分かるまでの ms。これより前に送ってまだ届かない穴は、この ms ＋ 入れ替わりの幅を過ぎたら送り直す（`resend_due`）。
    rack: Option<Rack>,
    /// `ack` が最後に進んだ時刻。観戦者は消費を待つ間、届いた記録があっても `ack` をそこで止める（流れの制御。`Assembler`）ので、
    /// `ack` の番号の記録が届いていないと決めるのは、`ack` がそこで止まってからの時間で数える（`resend_due`）。
    ack_moved_at: u32,
    /// 最後に往復の標本を取った時刻（`rto_scale` を倍にするのは、標本の取れない間の時間切れだけ）。
    sampled_at: Option<u32>,
    last_heard: u32,
    /// ACK を 1 通でも受けた（観戦者がこの送り元の経路を選んだ印。`drop_twins`・`twin_is_live`）。
    acked: bool,
    /// 鍵を作った時刻（`REKEY_GAP_MS` の起点）。
    keyed_at: u32,
    name: Name,
    /// JOIN の早送りの速さ（追いついていない観戦者への送りの上限 `spectate_rate::watcher_cap_bps`）。
    ff_max: u8,
    /// この観戦者への追いつきの送りの桶。
    cap: Bucket,
    /// JOIN の中継の木の欄。
    tree: JoinTree,
    /// 最後に受けた ACK の中継の状態（ACK を受けるまでは「紹介しないで」）。
    relay: RelayState,
    /// 最後に受けた ACK の、観戦者が受け取った受け入れの依頼の番号。
    admit_ack: u16,
    /// 上りの太さの見積もり（B/s。0 は測っていない。`SpectateHost::note_uplink`）。
    uplink: u32,
    /// 中継役に選んだ（`RELAYS` 人まで。外すまで続く）。
    relay_role: bool,
    /// 最後に ACK を受けた時刻（JOIN の再送では進めない。流れを受けている印）。
    last_ack_at: u32,
    /// この観戦者へ最後に送った受け入れの依頼の番号。
    admit_next: u16,
    /// 黙った中継役の子孫のうち、ホストへ戻って入り直した数（子孫の数から引いて二重に数えない）。
    owed: u8,
    /// 中継役の子孫の減りのうち、まだ戻っていない子の分（戻った子はこれを先に使い、`owed` を足さない）。`prepaid_until` まで。
    prepaid: u8,
    prepaid_until: u32,
    /// 上りの測りの番を渡した回数（`uplink::MEASURE_ROUNDS` まで）と、測れなかった後に次の番を渡してよい時刻。
    measure_rounds: u8,
    measure_after: u32,
    /// 黙った中継役の子孫へ穴あけを送った（次の ACK まで繰り返さない）。
    quiet_punched: bool,
    /// 中継役の下から戻ってきた観戦者の、元の中継役の nonce（入れ替えでその中継役へは回さない）。
    avoid_relay: Option<u64>,
}

// 受信スレッドのスタックにも作られる。i686 での大きさをチェック（cargo check）だけで見る。
const _: () = assert!(std::mem::size_of::<Watching>() <= 5_000);

/// ホストが観戦者ごとに持つ、中継の木の欄（[`SpectateHost::tree_of`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeView {
    pub join: JoinTree,
    pub relay: RelayState,
    pub admit_ack: u16,
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

#[derive(Debug, Clone, Copy)]
struct Rack {
    at: u32,
    idx: u32,
    rtt: u32,
}

/// 番号 `a_idx` を時刻 `a_at` に送ったのが、番号 `b_idx` を時刻 `b_at` に送ったより前か（同じ ms なら番号の順に送っている）。
fn sent_before(a_at: u32, a_idx: u32, b_at: u32, b_idx: u32) -> bool {
    let d = b_at.wrapping_sub(a_at);
    (d != 0 && d <= u32::MAX / 2) || (d == 0 && a_idx < b_idx)
}

/// 送りの量の桶（ms ごとに上限の速さで貯まり、送った分を引く。貯めは `burst` B まで、借りも `burst` B まで）。量は 1/1000 B で持つ
/// （1 ms に貯まる 1 B 未満の端数を捨てない）。
#[derive(Debug, Clone, Copy)]
struct Bucket {
    milli: i64,
    at: Option<u32>,
}

impl Bucket {
    /// 最初の `refill` で上限の貯めまで満ちる。
    const NEW: Bucket = Bucket {
        milli: i64::MAX / 2,
        at: None,
    };

    fn refill(&mut self, now: u32, bps: u32, burst: u32) {
        let dt = self.at.map_or(0, |t| elapsed(now, t));
        self.at = Some(now);
        self.milli = self
            .milli
            .saturating_add(i64::from(dt) * i64::from(bps))
            .min(i64::from(burst) * 1_000);
    }

    fn spend(&mut self, bytes: usize, burst: u32) {
        self.milli = (self.milli - bytes as i64 * 1_000).max(-i64::from(burst) * 1_000);
    }

    fn has_room(&self) -> bool {
        self.milli > 0
    }
}

/// 全体の上限の決め方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateMode {
    /// `spectate_rate` の観戦者ごとと全体の上限。全体は対戦の往復の延びで下げる。追いついた観戦者への送りは数えない。
    Adaptive,
    /// 固定の全体の上限（B/s）だけ。追いついた観戦者への送りも全体から引く（比べの測りのため）。
    Fixed(u32),
}

/// `poll` 1 回の送りの門。追いついていない観戦者（`capped`）への送りだけを、通数と桶（全体と、その観戦者の分）で止める（桶が正なら
/// 1 通送ってよい）。`paused` の間は誰にも送らない（生きている合図は門を通らない）。
struct Gate<'a> {
    capped: bool,
    paused: bool,
    poll_left: &'a mut u32,
    watcher_left: u32,
    bucket: &'a mut Bucket,
    burst: u32,
    /// この観戦者の桶のコピー（`poll` が送った後に枠へ戻す）。`None` は観戦者ごとの上限なし（`RateMode::Fixed`）。
    own: Option<(Bucket, u32)>,
    /// 追いついた観戦者への送りも全体の桶から引く（`RateMode::Fixed`）。
    live_spends: bool,
    held: bool,
    /// 門を通った送りがあった（生きている合図は門を通らないので数えない）。
    passed: bool,
}

impl Gate<'_> {
    /// 門が開いているか（見るだけ。止めた印 `held` を付けない）。
    fn is_open(&self) -> bool {
        if self.paused {
            return false;
        }
        !self.capped
            || (*self.poll_left > 0
                && self.watcher_left > 0
                && self.bucket.has_room()
                && self.own.is_none_or(|(b, _)| b.has_room()))
    }

    /// 1 通送ってよいか。よければ通った印を、だめなら止めた印を付ける。
    fn open(&mut self) -> bool {
        let ok = self.is_open();
        if ok {
            self.passed = true;
        } else {
            self.held = true;
        }
        ok
    }

    fn spend(&mut self, bytes: usize) {
        if self.capped {
            *self.poll_left = self.poll_left.saturating_sub(1);
            self.watcher_left = self.watcher_left.saturating_sub(1);
            if let Some((b, burst)) = self.own.as_mut() {
                b.spend(bytes, *burst);
            }
        }
        if self.capped || self.live_spends {
            self.bucket.spend(bytes, self.burst);
        }
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

/// 往復の見積もり（RFC 6298 の SRTT / RTTVAR。整数の ms）。標本は観戦者が ACK を返すペースの待ち（`ACK_TICK_MS`）を含む。
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
        // 観戦者のゲームが読み込みなどで ACK を長く止めた標本が、間隔を上限より大きく引き上げない。
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
    /// 最後に積んだ選択の記録（`Menu`・`Step`・`Select`）の番号。
    last_sel: Option<u32>,
    /// 最後に選択の記録があった seg の始まり（`seg_start` と同じ数え方）。遅れて JOIN した観戦者はここから（`join_start`）。
    sel_start: Option<u32>,
    rewind_max: u32,
    next: u32,
    broken: bool,
    /// 最後に満杯の断りの BYE を封じた時刻（`REFUSE_GAP_MS` の間引き）。
    last_refusal: Option<u32>,
    /// 直近の鍵の計算の時刻（古い順に上書きする輪。`AGREE_BURST` 個）。
    agree_log: [Option<u32>; AGREE_BURST],
    agree_next: usize,
    /// 観戦者の枠（`SLOT_CAP` 個）。1 枠が約 4.8 KB（大半は `sent` の表）なので、スタックに一時の配列を作らず、作るときだけ確保して以後は
    /// 1 枠ずつ書き換える。
    slots: Vec<Option<Watching>>,
    /// 観戦者への送信全体の量の桶。
    bucket: Bucket,
    rate: RateMode,
    /// 対戦の往復の延びで全体の上限を下げる決まりと、最後に受けた往復の要約（`note_delay`）。
    throttle: Throttle,
    delay: Option<DelaySnapshot>,
    /// 時刻の違う往復の要約を受けた数（ログ用）。
    delay_seen: u64,
    /// 最後に追いつきの送り（`capped` の門を通った送り）をした時刻。
    last_capped_send: Option<u32>,
    /// 追いついていない観戦者を回し始める枠（最後に追いつきの送りが届いた枠の次。上限で後ろの枠ばかり待たせない）。
    rr: usize,
    counts: HostCounts,
    /// 中継の木（ホストの紹介と取り置き、または中継役が子へ配る側）。
    tree: tree::Tree,
    /// 上りの測り（元のホストだけが使う）。
    meter: uplink::Meter,
}

impl SpectateHost {
    /// リングバッファの大きさ `RING_CAP` で作る（確保はここだけ）。
    pub fn new(cfg: HostConfig) -> Self {
        Self::with_capacity(cfg, RING_CAP)
    }

    /// 検査の小さいリングバッファ（0 は 1 に上げる）。
    pub fn with_capacity(cfg: HostConfig, cap: usize) -> Self {
        Self::build(
            cfg.session_id,
            &cfg.secret,
            Ephemeral::from_seed(&cfg.key_seed),
            Seq::new(),
            cap,
            tree::Tree::host(),
        )
    }

    fn build(
        session_id: u32,
        secret: &Secret,
        eph: Ephemeral,
        seq: Seq,
        cap: usize,
        tree: tree::Tree,
    ) -> Self {
        Self {
            session_id,
            key: AuthKey::derive(secret, session_id),
            eph,
            seq,
            build: crate::update::BUILD_NUMBER,
            ring: vec![Record::FILLER; cap.max(1)],
            head: 0,
            seg_start: 0,
            preamble_start: 0,
            last_sel: None,
            sel_start: None,
            rewind_max: REWIND_MAX,
            next: 0,
            broken: false,
            last_refusal: None,
            agree_log: [None; AGREE_BURST],
            agree_next: 0,
            slots: vec![None; SLOT_CAP],
            bucket: Bucket::NEW,
            rate: RateMode::Adaptive,
            throttle: Throttle::new(),
            delay: None,
            delay_seen: 0,
            last_capped_send: None,
            rr: 0,
            counts: HostCounts::default(),
            tree,
            meter: uplink::Meter::new(),
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

    /// 遅れて JOIN した観戦者が流れ始める番号: 最後に選択の記録があった seg の始まり。それがリングバッファから落ちたか、いまの seg の
    /// 始まりより `rewind_max` を越えて前なら、いまの seg の始まり。
    pub fn join_start(&self) -> u32 {
        let seg = self.seg_start.max(self.head);
        match self.sel_start {
            Some(s) if s >= self.head && seg.saturating_sub(s) <= self.rewind_max => s.min(seg),
            _ => seg,
        }
    }

    /// JOIN した観戦者が流れ始める番号: JOIN の続きの番号 `resume` がリングにある（`head` ≤ それ ≤ 積んだ終わり）ならそこ、
    /// 無ければ [`join_start`](Self::join_start)。続きから流すと、最初の STREAM の `first` は観戦者の連続の終わりと同じなので、
    /// `reset` が付いても観戦者は飛ばない。
    fn flow_start(&self, resume: Option<u32>) -> u32 {
        match resume {
            Some(r) if (self.head..=self.next).contains(&r) => r,
            _ => self.join_start(),
        }
    }

    /// 送り元 `addr` の観戦者の、中継の木の欄（JOIN と最後の ACK の値）。
    pub fn tree_of(&self, addr: SocketAddr) -> Option<TreeView> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(|w| TreeView {
                join: w.tree,
                relay: w.relay,
                admit_ack: w.admit_ack,
            })
    }

    /// 遡りの上限（記録の数。既定 `REWIND_MAX`）。検査が遡れない形を作るため。
    pub fn set_rewind_max(&mut self, n: u32) {
        self.rewind_max = n;
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// 観戦者の数: 直接の観戦者 ＋ 中継役の子孫 ＋ 返事待ちの紹介。同じ観戦者（nonce・公開鍵が同じ）が経路ごとに 2 枠あっても 1 人と数える。
    pub fn spectator_count(&self) -> usize {
        self.direct_count() + self.tree_count()
    }

    /// 直接の観戦者の数（代表の枠）。
    fn direct_count(&self) -> usize {
        (0..SLOT_CAP).filter(|&i| self.is_representative(i)).count()
    }

    /// 使っている枠の数(経路ごとの枠を別に数える。検査用)。
    pub fn slot_count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// 枠 `i` が、その観戦者を代表する枠か。同じ観戦者の枠が複数あれば、番号の小さい方 1 つだけが真。
    fn is_representative(&self, i: usize) -> bool {
        let Some(w) = self.slots.get(i).and_then(Option::as_ref) else {
            return false;
        };
        !self.slots.iter().enumerate().any(|(j, v)| {
            v.as_ref()
                .is_some_and(|v| j < i && v.nonce == w.nonce && v.public == w.public)
        })
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
    pub fn sack_of(&self, addr: SocketAddr) -> Option<Bits512> {
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

    /// 送り元 `addr` の観戦者への、送り直しの間隔の上限（ms。倍々もここで止まる。黙った観戦者と見る間もこれ。ログと検査用）。
    pub fn resend_cap_of(&self, addr: SocketAddr) -> Option<u32> {
        self.slots
            .iter()
            .flatten()
            .find(|w| w.addr == addr)
            .map(Watching::resend_cap)
    }

    /// 観戦者ごとに 1 つ(経路が 2 枠あれば代表の枠)。
    pub fn spectators(&self) -> impl Iterator<Item = (SocketAddr, Name, u32)> + '_ {
        (0..SLOT_CAP)
            .filter(|&i| self.is_representative(i))
            .filter_map(|i| self.slots.get(i).and_then(Option::as_ref))
            .map(|w| (w.addr, w.name, w.ack))
    }

    pub fn counts(&self) -> HostCounts {
        self.counts
    }

    /// 対戦の往復の要約（`rtt_probe::DelayProbe::snapshot`。ゲームスレッドが測って渡す）。次の `poll` から使う。
    pub fn note_delay(&mut self, snap: Option<DelaySnapshot>) {
        if let Some(s) = snap
            && self.delay.is_none_or(|d| d.at != s.at)
        {
            self.delay_seen = self.delay_seen.saturating_add(1);
        }
        self.delay = snap;
    }

    /// 時刻の違う往復の要約を受けた数（ログ用）。
    pub fn delay_seen(&self) -> u64 {
        self.delay_seen
    }

    /// 往復の延びで全体の上限を下げた・止めた数。
    pub fn throttle_counts(&self) -> ThrottleCounts {
        self.throttle.counts()
    }

    /// 全体の上限に今掛けている割合（ppm）と、追いついた観戦者への送りも止めているか（ログと検査用）。
    pub fn throttle_state(&self) -> (u32, bool) {
        (self.throttle.level_ppm(), self.throttle.paused())
    }

    /// **検査と比べの測りのためだけ**: 全体の上限を固定の `bps` にし、観戦者ごとの上限と往復の延びによる下げをやめる（追いついた観戦者への
    /// 送りも全体から引く）。ウィンドウ・送り直しの仕組みを、送りの量の上限に依らずに見るため。
    #[doc(hidden)]
    pub fn use_fixed_rate(&mut self, bps: u32) {
        self.rate = RateMode::Fixed(bps);
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
                w.sack = Bits512::EMPTY;
                w.rto_scale = 1;
                w.rack = None;
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
                self.slots.iter_mut().for_each(|s| *s = None);
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
            // 選択が `Seg` の前（前置き）に積まれる並びでは、選択はこの seg のもの。
            if self.last_sel.is_some_and(|i| i >= self.seg_start) {
                self.sel_start = Some(self.seg_start);
            }
        }
        if matches!(
            rec,
            Record::Menu { .. } | Record::Step(_) | Record::Select { .. }
        ) {
            self.last_sel = Some(idx);
            self.sel_start = Some(self.seg_start);
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
                DropReason::NotAdmitted => c.not_admitted += 1,
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
        // 測りの組は µs の時刻の口（`on_pair`）で受ける。ここでは 1,200 B を開かない。
        if wire::peek_type(bytes) == PeekedType::SpectatePair {
            return refuse(&mut self.counts, DropReason::Unexpected);
        }
        let slot_of = |slots: &[Option<Watching>]| {
            // 観戦者の枠は 4.8 KB ほどある。コピーせず参照で見る。
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
                self.tree.stop_punch(from);
                let known = slot_of(&self.slots);
                let mut twin = false;
                let start = self.flow_start(j.tree.resume_from);
                // 同じ観戦者の JOIN の再送は、鍵の計算（X25519）の前に返す。ここは相手の便も渡す受信スレッドの上なので、
                // HMAC を通った JOIN を何度送られても重い計算を繰り返さない。
                if let Some(w) = known
                    .and_then(|i| self.slots.get_mut(i))
                    .and_then(Option::as_mut)
                    && w.nonce == j.nonce
                    && w.public == j.public
                {
                    w.last_heard = now;
                    // 親を替えて戻った（epoch が進んだ）なら、観戦者の連続の終わりから流し直す（ack が観戦者の手元と食い違うと
                    // 穴が埋まらない）。木の欄は読み直すが、遅れて着いた古い epoch の JOIN は読まない。中継役は子の起点を依頼で
                    // 決めるので流し直さない。
                    let back =
                        tree::epoch_after(j.tree.epoch, w.tree.epoch) && !self.tree.is_relay();
                    if back {
                        w.restart(start, now);
                    }
                    if !tree::epoch_after(w.tree.epoch, j.tree.epoch) {
                        w.tree = j.tree;
                    }
                    if back {
                        // 入れ替えの紹介を受けた観戦者が、元の枠が残っている間にホストへ戻った。
                        self.back_while_direct(j.nonce, &j.public, now);
                    }
                    return HostEvent::Refreshed;
                }
                if self.tree.is_relay() {
                    return match self.relay_join(from, &j, known, now, out) {
                        Ok(ev) => ev,
                        Err(r) => refuse(&mut self.counts, r),
                    };
                }
                if let Some(ev) = self.intro_rejoin(from, &j, now, out) {
                    return ev;
                }
                let mut room = tree::Room::Free;
                // 起動し直しの連打は鍵の計算の前に断る。`last_heard` は延ばさない（重複の JOIN で枠を握り続けさせない）。
                if known
                    .and_then(|i| self.slots.get(i))
                    .and_then(Option::as_ref)
                    .is_some_and(|w| elapsed(now, w.keyed_at) < REKEY_GAP_MS)
                {
                    self.counts.join_throttled += 1;
                    return HostEvent::Throttled;
                }
                if known.is_none() {
                    // 同じ観戦者の別の経路（IPv4 と IPv6）が先に ACK を返している: こちらは観戦者が選ばなかった経路なので入れない。
                    if self.twin_is_live(from, j.nonce, &j.public, now) {
                        self.counts.twins_dropped += 1;
                        return HostEvent::Refreshed;
                    }
                    if self.broken {
                        return refuse(&mut self.counts, DropReason::Broken);
                    }
                    // 同じ観戦者の別の経路の枠は、人数の上限の手前でなくても入れる(ACK を返した方が決まれば片方は外れる)。
                    twin = self.has_twin(from, j.nonce, &j.public);
                    if !twin {
                        room = self.room(&j, now);
                    }
                    if room == tree::Room::Full {
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
                        if !self.take_agree_slot(now) {
                            self.counts.join_throttled += 1;
                            return HostEvent::Throttled;
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
                if !self.take_agree_slot(now) {
                    self.counts.join_throttled += 1;
                    return HostEvent::Throttled;
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
                let ff_max = j.ff_max;
                if let Some(w) = known
                    .and_then(|i| self.slots.get_mut(i))
                    .and_then(Option::as_mut)
                {
                    // 観戦者が起動し直した: 流れを JOIN の始まりからやり直す（鍵も新しい公開鍵で作り直した）。
                    *w = Self::fresh(id, name, ff_max, j.tree, start, now);
                    self.counts.joined += 1;
                    return HostEvent::Joined;
                }
                // 戻った子の席の入れ替えは、紹介の表を書き換える前に済ませる（紹介が表の空きを作り直すため）。
                let avoid = self.came_from(room);
                self.take_room(room, now);
                if !twin && self.introduce(id, &j, start, avoid, now, out) {
                    return HostEvent::Introduced;
                }
                match self.slots.iter_mut().find(|w| w.is_none()) {
                    Some(free) => {
                        let mut w = Self::fresh(id, name, ff_max, j.tree, start, now);
                        w.avoid_relay = avoid;
                        *free = Some(w);
                        // 同じ観戦者の 2 つ目の経路は、入った人数に足さない。
                        if !twin {
                            self.counts.joined += 1;
                        }
                        HostEvent::Joined
                    }
                    // 満杯は上で返してある（ここへは来ない）。
                    None => HostEvent::Refused,
                }
            }
            Message::SpectateAck {
                nonce,
                next,
                relay,
                admit_ack,
                sack,
            } => {
                let tail = self.next;
                let at = slot_of(&self.slots);
                let Some(w) = at
                    .and_then(|i| self.slots.get_mut(i))
                    .and_then(Option::as_mut)
                    .filter(|w| w.nonce == nonce)
                else {
                    return refuse(&mut self.counts, DropReason::UnknownSpectator);
                };
                w.last_heard = now;
                w.last_ack_at = now;
                w.quiet_punched = false;
                w.acked = true;
                let (twin_nonce, twin_public) = (w.nonce, w.public);
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
                    w.sack.shr(next - w.ack);
                    w.ack = next;
                    w.ack_moved_at = now;
                    w.sent_to = w.sent_to.max(next);
                }
                // 戻った ack（順が入れ替わって遅れて着いた古い ACK）の中継の状態は古いかもしれないので持たない。
                if !stale {
                    tree::settle_relay(w, relay, admit_ack, now, &mut self.counts);
                }
                // 戻った ack の範囲は、今の `ack` の後ろの番号と合わない。読まない。
                if !stale && !lied {
                    // 送っていない記録を観戦者が受け取れているはずは無い。そのビットは捨てる（嘘か、壊れた実装）。
                    let sent = w
                        .sent_to
                        .saturating_sub(w.ack)
                        .saturating_sub(1)
                        .min(SACK_BITS);
                    let valid = Bits512::low(sent);
                    if !sack.andn(&valid).is_empty() {
                        self.counts.sacks_clamped += 1;
                    }
                    w.sack = w.sack.or(&sack.and(&valid));
                }
                w.sample_new(old_ack, &old_sack, now);
                let moved = w.admit_ack;
                if let Some(i) = at {
                    self.drop_admitted(i, twin_nonce, moved, now);
                }
                self.drop_twins(from, twin_nonce, &twin_public, now);
                HostEvent::Acked { progress }
            }
            Message::Bye(_) => match slot_of(&self.slots) {
                Some(i) => {
                    self.release(i, now);
                    self.counts.left += 1;
                    HostEvent::Left
                }
                None => refuse(&mut self.counts, DropReason::UnknownSpectator),
            },
            _ => refuse(&mut self.counts, DropReason::Unexpected),
        }
    }

    /// 観戦者は候補の全部（IPv4 と IPv6）へ JOIN するので、同じ観戦者が送り元ごとに 2 枠になる。ACK を返した送り元 `keep` が観戦者の選んだ
    /// 経路なので、同じ nonce・公開鍵のほかの枠は外す（残すと枠と追いつきの送りの量を、返事のない経路に使い続ける）。中継役の枠は
    /// `release` で外す（子孫の席の取り置きを消さない）。
    fn drop_twins(&mut self, keep: SocketAddr, nonce: u64, public: &[u8; PUBLIC_LEN], now: u32) {
        for i in 0..SLOT_CAP {
            let Some(w) = self.slots.get(i).and_then(Option::as_ref) else {
                continue;
            };
            if w.addr == keep || w.nonce != nonce || w.public != *public {
                continue;
            }
            if w.relay_role {
                self.release(i, now);
            } else if let Some(slot) = self.slots.get_mut(i) {
                *slot = None;
            }
            self.counts.twins_dropped += 1;
        }
    }

    /// 別の送り元に、同じ観戦者(nonce・公開鍵が同じ)の枠があるか。
    fn has_twin(&self, from: SocketAddr, nonce: u64, public: &[u8; PUBLIC_LEN]) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|w| w.addr != from && w.nonce == nonce && w.public == *public)
    }

    /// 別の送り元の同じ観戦者（nonce・公開鍵が同じ）が ACK を返していて、`REJOIN_MS` の内に何か聞いているか。観戦者の NAT が付け替わって
    /// 同じ観戦者が新しい送り元から入り直すときは、古い送り元が黙ってから（観戦者は `REJOIN_MS` 黙った後に JOIN を送り直す）なので当たらない。
    fn twin_is_live(
        &self,
        from: SocketAddr,
        nonce: u64,
        public: &[u8; PUBLIC_LEN],
        now: u32,
    ) -> bool {
        self.slots.iter().flatten().any(|w| {
            w.addr != from
                && w.nonce == nonce
                && w.public == *public
                && w.acked
                && elapsed(now, w.last_heard) < REJOIN_MS
        })
    }

    /// 観戦者の公開鍵との向きごとの鍵（ホストが「前」）。計算（X25519）の回数を数える。
    fn agree(&mut self, public: &[u8; PUBLIC_LEN]) -> Option<DirKeys> {
        self.counts.key_agreements += 1;
        self.eph.dir_keys(public, true, self.session_id, &self.key)
    }

    /// 入ったばかりの観戦者。すぐ 1 通目（記録か空の合図）が出るよう、`last_send` を合図の間隔だけ前にする。
    fn fresh(id: Joiner, name: Name, ff_max: u8, tree: JoinTree, start: u32, now: u32) -> Watching {
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
            sack: Bits512::EMPTY,
            sent: [Sent::NEVER; SEND_WINDOW as usize],
            rack: None,
            ack_moved_at: now,
            sampled_at: None,
            last_heard: now,
            acked: false,
            keyed_at: now,
            name,
            ff_max,
            cap: Bucket::NEW,
            tree,
            relay: RelayState::NO_INTRO,
            admit_ack: 0,
            uplink: 0,
            relay_role: false,
            last_ack_at: now,
            admit_next: 0,
            owed: 0,
            prepaid: 0,
            prepaid_until: now,
            measure_rounds: 0,
            measure_after: now,
            quiet_punched: false,
            avoid_relay: None,
        }
    }

    /// 鍵の計算の枠が空いていれば使って true。ホスト全体で `AGREE_WINDOW_MS` に `AGREE_BURST` 回まで。
    fn take_agree_slot(&mut self, now: u32) -> bool {
        let Some(oldest) = self.agree_log.get_mut(self.agree_next) else {
            return false;
        };
        if oldest.is_some_and(|t| elapsed(now, t) < AGREE_WINDOW_MS) {
            return false;
        }
        *oldest = Some(now);
        self.agree_next = (self.agree_next + 1) % AGREE_BURST;
        true
    }

    /// 送るものがあれば送る（外す観戦者も決める）。受信スレッドの周回で 1 ms ごとくらいに呼ぶ。追いついていない観戦者への送りは
    /// 上限（モジュール doc の「送りの上限」）で止め、残りは次の呼び出しで送る。
    pub fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if !self.tree_poll(now, out) {
            return;
        }
        let (rate, live_spends) = match self.rate {
            RateMode::Adaptive => {
                let catching_up = self
                    .last_capped_send
                    .is_some_and(|t| elapsed(now, t) < spectate_rate::CATCHUP_RECENT_MS);
                self.throttle.update(now, self.delay, catching_up);
                // 同じ観戦者の 2 つ目の経路の枠は数えない（代表の枠だけ）。
                let cap = spectate_rate::total_cap_bps(
                    (0..SLOT_CAP)
                        .filter(|&i| self.is_representative(i))
                        .filter_map(|i| self.slots.get(i).and_then(Option::as_ref))
                        .map(|w| w.ff_max),
                );
                (self.throttle.scale(cap), false)
            }
            RateMode::Fixed(bps) => (bps, true),
        };
        let burst = spectate_rate::burst_bytes(rate);
        let paused = self.rate == RateMode::Adaptive && self.throttle.paused();
        self.bucket.refill(now, rate, burst);
        for i in 0..SLOT_CAP {
            if self
                .slots
                .get(i)
                .and_then(Option::as_ref)
                .is_some_and(|w| elapsed(now, w.last_heard) >= SILENT_MS)
            {
                self.release(i, now);
                self.counts.silenced += 1;
            }
        }
        self.measure_poll(now, out);
        // 観戦者の枠を書き換えながら記録を読めるよう、ホストの側の読み取りだけの部品を枠から分けて借りる。
        let feed = Feed {
            ring: &self.ring,
            head: self.head,
            next: self.next,
            seq: &self.seq,
            session_id: self.session_id,
        };
        let counts = &mut self.counts;
        let relay_form = self.tree.is_relay();
        let mut poll_left = CATCHUP_PACKETS_PER_POLL;
        let start = self.rr;
        let mut served = None;
        // 追いついた観戦者を先に回す（対戦に付いて見ている観戦者の遅れを、追いつきの送りで増やさない）。
        for behind_pass in [false, true] {
            for j in 0..SLOT_CAP {
                let at = (start + j) % SLOT_CAP;
                let Some(w) = self.slots.get_mut(at).and_then(Option::as_mut) else {
                    continue;
                };
                let behind = feed.next.saturating_sub(w.ack) > BEHIND_RECORDS;
                if behind != behind_pass {
                    continue;
                }
                let own = match self.rate {
                    RateMode::Adaptive => {
                        let bps = spectate_rate::watcher_cap_bps(w.ff_max);
                        let b = spectate_rate::burst_bytes(bps);
                        w.cap.refill(now, bps, b);
                        Some((w.cap, b))
                    }
                    RateMode::Fixed(_) => None,
                };
                let mut gate = Gate {
                    capped: behind,
                    paused,
                    poll_left: &mut poll_left,
                    watcher_left: CATCHUP_PACKETS_PER_WATCHER,
                    bucket: &mut self.bucket,
                    burst,
                    own,
                    live_spends,
                    held: false,
                    passed: false,
                };
                // 中継役とその子は親の沈黙を `REJOIN_MS` で見るので、記録の無い間も短い間隔で合図する。
                let heartbeat = if relay_form || w.relay_role {
                    RELAY_HEARTBEAT_MS
                } else {
                    HEARTBEAT_MS
                };
                w.serve(&feed, now, heartbeat, out, counts, &mut gate);
                if let Some((b, _)) = gate.own {
                    w.cap = b;
                }
                if gate.held {
                    counts.deferred += 1;
                }
                if behind && gate.passed {
                    served = Some(at);
                    self.last_capped_send = Some(now);
                }
            }
        }
        // 次は、追いつきの送りが最後に届いた観戦者の次から回す。量の桶が空く周期と枠の回りが重なって、同じ観戦者がいつまでも後回しに
        // ならない。
        if let Some(at) = served {
            self.rr = (at + 1) % SLOT_CAP;
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

    /// 番号 `first` から `count` 個（`MAX_STREAM_RECORDS` 以下）の STREAM を 1 通送り、その長さを返す。
    fn send<O: Outlet>(&self, w: &Watching, first: u32, count: u32, out: &mut O) -> usize {
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
        )
    }

    /// 送り直す記録 `due`（ビット k = 番号 `w.ack + k`）を送る。続く番号は 1 通に詰める（`MAX_STREAM_RECORDS` まで）。門が閉じたら
    /// そこでやめる。送った記録のビットを返す。
    fn resend<O: Outlet>(
        &self,
        w: &mut Watching,
        due: &Bits512,
        now: u32,
        out: &mut O,
        counts: &mut HostCounts,
        gate: &mut Gate<'_>,
    ) -> Bits512 {
        let mut sent = Bits512::EMPTY;
        let mut from = 0u32;
        while let Some(k) = due.next_set(from) {
            if !gate.open() {
                break;
            }
            let n = due.run_len(k, MAX_STREAM_RECORDS as u32);
            let bytes = self.send(w, w.ack + k, n, out);
            gate.spend(bytes);
            w.mark_resent(k, n, now);
            sent.set_range(k, n);
            counts.packets_sent += 1;
            counts.records_sent += u64::from(n);
            counts.records_resent += u64::from(n);
            counts.resends += 1;
            from = k + n;
        }
        sent
    }
}

impl Watching {
    /// 2 回目からの送り直しの間隔の上限（ms）: 往復の見積もりを `RESEND_CAP_MIN_MS`〜`RESEND_MAX_MS` に収めたもの（見積もりが無ければ
    /// 下の値）。往復が下の値より長い回線では往復についていき、届く途中の記録を送り直さない。倍々はここで止まる（落ちた送り直しを
    /// 長く待たせない）。
    fn resend_cap(&self) -> u32 {
        if self.rtt.sampled {
            self.rtt.srtt.clamp(RESEND_CAP_MIN_MS, RESEND_MAX_MS)
        } else {
            RESEND_CAP_MIN_MS
        }
    }

    /// 流れを `start` から始め直す（鍵・往復の見積もり・名前はそのまま。`reset` 付きの 1 通目をすぐ送る）。
    fn restart(&mut self, start: u32, now: u32) {
        self.ack = start;
        self.base = start;
        self.sent_to = start;
        self.last_send = now.wrapping_sub(HEARTBEAT_MS);
        self.rto_scale = 1;
        self.sack = Bits512::EMPTY;
        self.sent.fill(Sent::NEVER);
        self.rack = None;
        self.ack_moved_at = now;
    }

    /// 新しい分を送り、送り直しの時期の来た記録を送り直し、何も無ければ `heartbeat` ms ごとに生きている合図を送る。
    fn serve<O: Outlet>(
        &mut self,
        feed: &Feed<'_>,
        now: u32,
        heartbeat: u32,
        out: &mut O,
        counts: &mut HostCounts,
        gate: &mut Gate<'_>,
    ) {
        let end = feed.next.min(self.ack.saturating_add(SEND_WINDOW));
        if gate.paused {
            // 往復の延びで止めている間も、観戦者が「ホストが黙った」と見て抜けないよう、生きている合図だけは送る。
            if self.sent_to < end || self.ack < self.sent_to {
                gate.held = true;
            }
            if elapsed(now, self.last_send) >= heartbeat {
                let bytes = feed.send(self, self.ack, 0, out);
                gate.spend(bytes);
                counts.packets_sent += 1;
                counts.heartbeats += 1;
                self.last_send = now;
            }
            return;
        }
        let mut sent_new = false;
        while self.sent_to < end && gate.open() {
            let at = self.sent_to;
            let n = (end - at).min(MAX_STREAM_RECORDS as u32);
            let bytes = feed.send(self, at, n, out);
            gate.spend(bytes);
            self.mark_sent(at, n, now);
            counts.packets_sent += 1;
            counts.records_sent += u64::from(n);
            self.sent_to = at + n;
            sent_new = true;
        }
        if sent_new {
            self.last_send = now;
        }
        if self.ack < self.sent_to {
            self.resend_due(feed, now, out, counts, gate);
        } else if elapsed(now, self.last_send) >= heartbeat {
            // 何も無いまま静か: 生きている合図（空の STREAM）。
            let bytes = feed.send(self, self.ack, 0, out);
            gate.spend(bytes);
            counts.packets_sent += 1;
            counts.heartbeats += 1;
            self.last_send = now;
        }
    }

    /// この ACK で初めて受け取れたと分かった記録のうち、1 回しか送っていないものの最も古いものから、往復の標本を 1 つ取る
    /// （`old_ack` / `old_sack` は ACK を重ねる前の値）。送り直した記録は取らない（どの送りへの返事か分からない —— Karn）。
    /// 前の ACK で後ろの記録が届いていた記録も取らない（観戦者が消費を待って ack を止めた・順が入れ替わった。往復より長い）。
    /// あわせて、届いた記録のうち最も後に送ったもの（`rack`）を進める。
    fn sample_new(&mut self, old_ack: u32, old_sack: &Bits512, now: u32) {
        let top = self.sent_to.min(old_ack.saturating_add(SEND_WINDOW));
        // 前の ACK までに届いていたと分かる最も後の番号の次。
        let frontier = old_sack.highest().map_or(old_ack, |h| old_ack + 2 + h);
        // 以下のビット k は番号 `old_ack + k`。前の ACK で届いていたもの（k = 0 は `old_ack` で、届いていない）と、いま届いているもの
        // （`ack` より前は全部・後ろは `sack`）の差が、この ACK で初めて分かった記録。
        let mut was = *old_sack;
        was.shl(1);
        let moved = self.ack.saturating_sub(old_ack);
        let mut is = self.sack;
        is.shl(moved.saturating_add(1));
        let is = is.or(&Bits512::low(moved));
        let new = is
            .andn(&was)
            .and(&Bits512::low(top.saturating_sub(old_ack)));
        let mut oldest: Option<u32> = None;
        let mut from = 0u32;
        while let Some(k) = new.next_set(from) {
            from = k + 1;
            let idx = old_ack + k;
            if let Some(sent) = self.sent.get(Sent::index(idx)).copied()
                && sent.times == 1
            {
                let age = elapsed(now, sent.at);
                if idx >= frontier {
                    oldest = Some(oldest.map_or(age, |o| o.max(age)));
                }
                if self
                    .rack
                    .is_none_or(|r| sent_before(r.at, r.idx, sent.at, idx))
                {
                    self.rack = Some(Rack {
                        at: sent.at,
                        idx,
                        rtt: age,
                    });
                }
            }
        }
        if let Some(age) = oldest {
            self.rtt.sample(age);
            self.rto_scale = 1;
            self.sampled_at = Some(now);
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

    /// 送り直す記録（観戦者がまだ受け取れていないもの）を送り直す。受け取れた範囲（`sack`）の中の記録は送らない。送り直すのは:
    /// - 最後に送ってから `rto` × 2^(送った回数 − 1)（上限 `resend_cap`）経った記録（時計）
    /// - それより前でも、後から送った記録が届いて（`rack`）、その往復 ＋ 入れ替わりの幅（往復の 1/4。ACK のペース以上）を過ぎた記録
    fn resend_due<O: Outlet>(
        &mut self,
        feed: &Feed<'_>,
        now: u32,
        out: &mut O,
        counts: &mut HostCounts,
        gate: &mut Gate<'_>,
    ) {
        // 門が閉じていれば何も送れない。ウィンドウ（最大 512 ビット）を走査しない。
        if !gate.is_open() {
            return;
        }
        let span = self.sent_to.saturating_sub(self.ack).min(SEND_WINDOW);
        // ビット k = 番号 ack + k（k = 0 は連続の終わり = 受け取れていない）。送ったのに返事の無い記録だけを数える。
        let mut received = self.sack;
        received.shl(1);
        let missing = Bits512::low(span).andn(&received);
        let rto = self.rtt.rto();
        let cap = self.resend_cap();
        let reorder = (4 * self.rtt.var).max(2 * ACK_TICK_MS);
        // 返事（ACK）が送り直しの上限と送り直しの間隔の長い方の間聞こえない間は、記録ごとの時計をやめ、先頭の穴の時計だけで先最初から
        // 1 通ぶん送る（連打を避ける。モジュール doc）。往復の標本がまだ無い間も同じ（往復が `RESEND_INIT_MS` より長い回線で、入った
        // 直後のウィンドウ全部を送り直さない）。返事が戻れば記録ごとの時計に戻る。見る記録も先頭の 1 通ぶんだけ。
        let quiet = elapsed(now, self.last_heard) >= cap.max(rto) || !self.rtt.sampled;
        let scan = if quiet {
            missing.and(&FIRST_PACKET)
        } else {
            missing
        };
        let (mut due, mut fresh, mut fast) = (Bits512::EMPTY, Bits512::EMPTY, Bits512::EMPTY);
        let mut from = 0u32;
        while let Some(k) = scan.next_set(from) {
            from = k + 1;
            let idx = self.ack + k;
            let sent = self
                .sent
                .get(Sent::index(idx))
                .copied()
                .unwrap_or(Sent::NEVER);
            let tries = u32::from(sent.times).max(1);
            // 1 回目は見積もり × 倍率（見積もりが短すぎるときに往復を越えて待ち、標本を取る —— Karn）。送り直した後は送るたびに
            // 2 倍で、上限 `cap`（黙った観戦者へ連打しない）。
            let timeout = if tries == 1 {
                rto.saturating_mul(self.rto_scale).min(RESEND_MAX_MS)
            } else {
                rto.saturating_mul(1 << (tries - 1).min(8)).min(cap)
            };
            let waited = elapsed(now, sent.at);
            // `ack` の番号の記録は、`ack` がそこへ来てからの時間でも数える（流れの制御で止めた `ack` の記録は届いている。消費が
            // 続く間、`ack` は 1 表示ごとに進む）。
            let stuck = if k == 0 {
                elapsed(now, self.ack_moved_at)
            } else {
                u32::MAX
            };
            if waited.min(stuck) >= timeout {
                due.set(k);
                if tries == 1 {
                    fresh.set(k);
                }
            } else if stuck >= 2 * ACK_TICK_MS
                && self.rack.is_some_and(|r| {
                    sent_before(sent.at, idx, r.at, r.idx)
                        && waited >= r.rtt.saturating_add(reorder)
                })
            {
                due.set(k);
                fast.set(k);
            }
        }
        if quiet {
            // 先頭の穴の時計が来たときだけ、先頭の 1 通ぶんの穴を全部送る。
            due = if due.get(0) { scan } else { Bits512::EMPTY };
            fresh = fresh.and(&due);
            fast = fast.and(&due);
        }
        if due.is_empty() {
            return;
        }
        let sent = feed.resend(self, &due, now, out, counts, gate);
        if sent.is_empty() {
            return;
        }
        self.last_send = now;
        counts.records_fast += u64::from(sent.and(&fast).count_ones());
        // 見積もりが短すぎる印は、時計で送り直した（速い送り直しでない）1 回目の時間切れが、往復の標本の取れない間に起きたこと。標本が
        // 取れている間の時間切れはただの落とし（倍にすると、ウィンドウの最後の通が落ちたときの回復が遅れる）。
        if !fresh.and(&sent).is_empty() && self.sampled_at.is_none_or(|t| elapsed(now, t) >= rto) {
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
    /// 置き場に入れたことのある最も先の番号の次（`sack` が、何も持っていない先を数えないための上限）。
    top: u32,
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
            top: 0,
            counts: AsmCounts::default(),
        }
    }

    pub fn ack(&self) -> u32 {
        self.run_end
    }

    /// 受け取れた範囲（ACK の `sack`）: `ack()` の後ろで受け取った記録。ビット i = 番号 `ack() + 1 + i`。確保しない。
    pub fn sack(&self) -> Bits512 {
        let store = self.store_len();
        // 置き場が持てるのは head から store 個。それより先は持っていない（`on_stream` が捨てる）。`top` より先も何も無い。
        let limit = self.head.saturating_add(store).min(self.top);
        let mut bits = Bits512::EMPTY;
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
                bits.set(i);
            }
        }
        bits
    }

    pub fn pending(&self) -> usize {
        (self.run_end - self.head) as usize
    }

    /// 置き場にある最も先の記録の番号 ＋ 1（連続の後ろの飛び飛びの記録も数える。無ければ `ack()`）。`ack()` より先の記録は
    /// 捨てずに残るので、`top` が `ack()` より先なら、その手前の記録は置き場にある。
    pub fn stored_end(&self) -> u32 {
        self.top.max(self.run_end)
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
                self.top = self.top.max(idx + 1);
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
    /// `not_hgrt` のうち IPv6 から来たもの（ホストの穴あけの印が IPv6 で届いたかを見る）。
    pub not_hgrt_v6: u64,
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
    /// 使い捨ての鍵（紹介された中継役との鍵を作る）。
    eph: Ephemeral,
    /// 元のホストとの向きごとの鍵（観戦者は「後ろ」。ホストの公開鍵が弱い点なら None）。
    keys: Option<DirKeys>,
    /// 送り手 `Sender::Spectator` の通し番号（秘密鍵と同じ寿命）。
    seq: Seq,
    /// 自分のビルド番号（JOIN に載せる。既定は `update::BUILD_NUMBER`）。
    build: u32,
    /// 早送りの速さ（JOIN に載せる。既定 `FF_UPDATES_DEFAULT`）。
    ff_max: u8,
    /// 回線の型（JOIN に載せる。既定は不明）。
    nat: NatMark,
    nonce: u64,
    name: Name,
    host: Candidates,
    /// 元のホストのうち、最初の STREAM を送ってきた送り元（以後はここだけ）。
    host_addr: Option<SocketAddr>,
    /// 元のホストから最後に STREAM か BYE を聞いた時刻（親がホストの間の JOIN の送り直し）。
    home_heard: Option<u32>,
    asm: Assembler,
    last_join: Option<u32>,
    last_heard: Option<u32>,
    /// 最後に送った ack（消費で `Assembler::ack` が進んだら送り直す）。
    last_ack_sent: u32,
    /// ホストの最後の BYE（上限のときの取消など）。次の STREAM が来たら消す。
    host_bye: Option<ByeReason>,
    counts: WatcherCounts,
    /// 中継の木（いまの親・epoch・受け入れの依頼の控え）。
    tree: tree::WatchTree,
    /// 上りの測りの番。
    meter: uplink::WatchMeter,
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
            eph,
            keys,
            seq: Seq::new(),
            build: crate::update::BUILD_NUMBER,
            ff_max: FF_UPDATES_DEFAULT as u8,
            nat: NatMark::UNKNOWN,
            nonce: cfg.nonce,
            name: cfg.name,
            host: cfg.host,
            host_addr: None,
            home_heard: None,
            asm,
            last_join: None,
            last_heard: None,
            last_ack_sent: 0,
            host_bye: None,
            counts: WatcherCounts::default(),
            tree: tree::WatchTree::new(),
            meter: uplink::WatchMeter::new(),
        }
    }

    /// 一度でも親（ホストか中継役）から STREAM を受けた。親を替える間も偽に戻らない。
    pub fn is_joined(&self) -> bool {
        self.tree.joined_once
    }

    /// 検査が違うビルドの観戦者を作るため。既定は `update::BUILD_NUMBER`。
    pub fn set_build(&mut self, build: u32) {
        self.build = build;
    }

    /// JOIN に載せる早送りの速さ（ini の `[spectate] ff_max`。1〜16 に収める）。ホストはこれで追いつきの送りの上限を決める。
    pub fn set_ff_max(&mut self, ff_max: u32) {
        self.ff_max = ff_max.clamp(FF_UPDATES_MIN, FF_UPDATES_MAX) as u8;
    }

    pub fn ff_max(&self) -> u8 {
        self.ff_max
    }

    /// JOIN に載せる回線の型（ホストが中継役を選ぶのに読む）。
    pub fn set_nat(&mut self, nat: NatMark) {
        self.nat = nat;
    }

    pub fn public_key(&self) -> [u8; PUBLIC_LEN] {
        self.public
    }

    /// ホストとの向きごとの鍵（**検査と診断のためだけ**。観戦者が送った便を外から開く・ホスト役の検査が、観戦者へ渡すために封じる）。
    #[doc(hidden)]
    pub fn dir_keys(&self) -> Option<DirKeys> {
        self.keys
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.next()
    }

    /// いまの親（元のホストか、紹介された中継役）のうち、STREAM を送ってきた送り元。親を替えた直後は、新しい親から STREAM が
    /// 来るまで None。
    pub fn host_addr(&self) -> Option<SocketAddr> {
        self.parent_link().0
    }

    /// ホストが最後に送った BYE（JOIN を断られたときなど）。次の STREAM を受けると消える。
    pub fn host_bye(&self) -> Option<ByeReason> {
        self.host_bye
    }

    /// 最後に何かを聞いてからの ms（何も聞いていなければ `None`）。使う側が「ホストが黙った」を決める。
    pub fn silent_ms(&self, now: u32) -> Option<u32> {
        self.last_heard.map(|t| elapsed(now, t))
    }

    /// 最後に何かを聞いた時刻（ms。何も聞いていなければ `None`）。受信スレッドが、別のスレッドの時計で沈黙を測らせるために渡す。
    pub fn last_heard(&self) -> Option<u32> {
        self.last_heard
    }

    pub fn ack(&self) -> u32 {
        self.asm.ack()
    }

    pub fn sack(&self) -> Bits512 {
        self.asm.sack()
    }

    pub fn pending(&self) -> usize {
        self.asm.pending()
    }

    /// `Assembler::stored_end`。
    pub fn stored_end(&self) -> u32 {
        self.asm.stored_end()
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

    /// 観戦を抜ける（BYE を 1 通。親は観戦者の枠を空ける）。dll の観戦者が、見終わった・続けられなくなったときに
    /// 枠を返す。いまの親から最初の STREAM を受けるまで（親のアドレスが定まるまで）は何も送らない。
    pub fn bye<O: Outlet>(&mut self, reason: ByeReason, out: &mut O) {
        if let (Some(a), Some(k)) = self.parent_addr_keys() {
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

    /// いまの親の（アドレス, 鍵）。
    fn parent_addr_keys(&self) -> (Option<SocketAddr>, Option<DirKeys>) {
        match &self.tree.parent {
            Some(p) => (p.addr, Some(p.keys)),
            None => (self.host_addr, self.keys),
        }
    }

    /// いまの親の（アドレス, 最後に聞いた時刻）。
    fn parent_link(&self) -> (Option<SocketAddr>, Option<u32>) {
        match &self.tree.parent {
            Some(p) => (p.addr, p.heard),
            None => (self.host_addr, self.home_heard),
        }
    }

    fn send_ack<O: Outlet>(&mut self, to: SocketAddr, out: &mut O) {
        let next = self.asm.ack();
        let Some(k) = self.parent_addr_keys().1 else {
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
                relay: self.ack_relay_state(),
                admit_ack: self.tree.admit_no,
                sack: self.asm.sack(),
            },
            out,
        );
        self.last_ack_sent = next;
        self.counts.acks_sent += 1;
    }

    /// 受信ソケットに来た 1 データグラムを処理する。STREAM を組み立てに入れ、すぐ ack を返す。紹介（INTRO）・受け入れの依頼（ADMIT）・
    /// 測りの番（MEASURE）は元のホストの鍵で開けたものだけを、送り元に依らず受ける。
    pub fn on_datagram<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        out: &mut O,
    ) {
        if wire::classify(bytes) != Datagram::Hgrt {
            self.counts.not_hgrt += 1;
            if from.is_ipv6() {
                self.counts.not_hgrt_v6 += 1;
            }
            return;
        }
        if matches!(
            wire::peek_type(bytes),
            PeekedType::SpectateIntro | PeekedType::SpectateAdmit | PeekedType::SpectateMeasure
        ) {
            self.on_home_control(bytes, now);
            return;
        }
        let home = self.tree.parent.is_none();
        let (paddr, keys) = self.parent_addr_keys();
        let known = match &self.tree.parent {
            Some(p) => p.cands.contains(from),
            None => self.host.contains(from),
        };
        let c = &mut self.counts;
        // 親のアドレスが決まったあとは、そのアドレスだけ。決まる前は、親の候補から来たものを受け、候補に無い送り元（`probation`）は
        // タグの合う STREAM だけを受けてそのアドレスを親に採る。それ以外は `foreign` で捨てる。
        if paddr.is_some_and(|a| a != from) {
            c.foreign += 1;
            return;
        }
        let probation = paddr.is_none() && !known;
        if wire::peek_session(bytes).is_some_and(|s| s != self.session_id) {
            if probation {
                c.foreign += 1;
            } else {
                c.wrong_session += 1;
            }
            return;
        }
        let open = keys.map(|k| k.opener());
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
                if probation && home {
                    c.learned_host += 1;
                }
                match self.tree.parent.as_mut() {
                    Some(p) => {
                        p.addr = Some(from);
                        p.heard = Some(now);
                    }
                    None => {
                        self.host_addr = Some(from);
                        self.home_heard = Some(now);
                    }
                }
                self.tree.joined_once = true;
                self.last_heard = Some(now);
                self.host_bye = None;
                self.tree.refused = false;
                self.asm.on_stream(&s);
                self.send_ack(from, out);
            }
            _ if probation => c.foreign += 1,
            Message::Bye(r) if home => {
                c.byes += 1;
                self.last_heard = Some(now);
                self.home_heard = Some(now);
                self.host_bye = Some(r);
                // 一度接続した後の取消は、ホストが満杯で入れ直せなかった印（入る前の取消は満杯の待ちで、JOIN を送り続ける）。
                if r == ByeReason::Cancelled && self.tree.joined_once {
                    self.tree.refused = true;
                }
            }
            Message::SpectateRehome if !home => {
                self.tree.counts.rehomes += 1;
                self.go_home(false, now);
            }
            _ => c.unexpected += 1,
        }
    }

    /// JOIN の送り直しと、消費で進んだ ack の通知。使う側の周回で 1 ms ごとくらいに呼ぶ。親が中継役なら、紹介から `REJOIN_MS` の内に
    /// STREAM が来ないか、流れが `REJOIN_MS` 途切れたら元のホストへ戻る。測りの番の間は、上りの測りの組を送る。
    pub fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        self.check_parent(now);
        let (paddr, heard) = self.parent_link();
        // JOIN: まだ流れが来ない間と、来ていたのが途切れた間（親が外したかもしれない）。
        let wants_join = match (paddr, heard) {
            (Some(_), Some(t)) => elapsed(now, t) >= REJOIN_MS,
            _ => true,
        };
        if wants_join
            && self
                .last_join
                .is_none_or(|t| elapsed(now, t) >= JOIN_RESEND_MS)
        {
            let join = self.join_message();
            let cands = match &self.tree.parent {
                Some(p) => &p.cands,
                None => &self.host,
            };
            match paddr {
                Some(a) => emit_signed(&self.key, self.session_id, a, &join, out),
                None => {
                    for c in cands.as_slice() {
                        emit_signed(&self.key, self.session_id, c.addr, &join, out);
                    }
                }
            }
            self.last_join = Some(now);
            self.counts.joins_sent += 1;
        }
        // 消費で置き場が空き、ack が進んだ（流れの制御で止めていた分）。
        if let Some(a) = paddr
            && self.asm.ack() != self.last_ack_sent
        {
            self.send_ack(a, out);
        }
        self.send_pairs(now, out);
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

    /// アドレス `from` から JOIN が来た: そのアドレスへの穴あけを止める。送っている途中の予定にあって止めたなら true
    /// （回数を送り終えた後の JOIN は、止めたことにならない）。
    pub fn stop_for(&mut self, from: SocketAddr) -> bool {
        let running = self.round < PUNCH_ROUNDS;
        let mut stopped = false;
        for t in self.targets.iter_mut() {
            if t.is_some_and(|a| same_endpoint(a, from)) {
                *t = None;
                stopped |= running;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 観戦者の枠（`Copy`）は受信スレッドのスタックでも作られる。表を足して 5 KB を越えたら（20 枠で 100 KB）枠の持ち方を見直す。
    #[test]
    fn a_watching_slot_stays_under_5kb() {
        let n = std::mem::size_of::<Watching>();
        assert!(n > SEND_WINDOW as usize * 8, "送りの表より小さい: {n}");
        assert!(n <= 5_000, "観戦者の枠が {n} B");
    }
}
