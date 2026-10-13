//! 接続のセッション。受けたパケットをどの状態機械へ渡すか・どの順で呼ぶか・何を送り返すかを 1 か所に置く（peer_sim と dll が共有する）。
//! 各状態機械の中身は `handshake.rs`・`liveness.rs`・`selection.rs`・`lockstep.rs`・`check.rs` にあり、ここは「呼ぶ順」だけを持つ:
//!
//! - **受けたとき**（`Link::on_datagram`）: STUN の応答は数えて捨てる。相手の候補にない送り元は捨てる（繋がる前の手順の 1 通でタグまで
//!   通ったものは「学んだ候補」にする）。session_id が違うもの・タグが合わないもの・解けないもの・自分の送った HELLO が返ってきたもの
//!   （role_nonce が自分のもの）は、状態機械へ渡す前に捨てる。HELLO・HELLO_ACK・AGREE は繋がった後も `handshake` へ渡し、返しを送り、
//!   その直後に `poll_send` を 1 回呼ぶ。繋がる前のそれ以外は捨てる。繋がった後は、全部を `liveness` へ渡して返しを送り、種類ごとに
//!   選択・lockstep・CHECK へ渡す。ただし受信スレッドがその場で PONG を返した PING（`on_datagram_marked` の印。`pong.rs`）には PONG を
//!   重ねない。
//! - **INPUT**: 受け入れたら `Selection::on_peer_input`（SELECT の送り直しを止める）。前の seg の INPUT で相手がまだ前の seg の最後を
//!   待っていれば、前の seg のバックアップを送り返す。次の seg の INPUT は捨てて `Recv::NextSeg` を返し、選択の外で届いた今の step の MENU には
//!   `Recv::PeerSelecting` を返す（相手が今の seg を出た合図。peer_sim の「付いていく」形だけが使う）。
//! - **時計で**（`Link::poll`。1〜4 ms ごと、フレームの外でも）: 接続の手順の送り直し → 繋がったか → 生存の PING / BYE →
//!   選択の STEP / STEP_REQ / SELECT の送り直し。
//! - **マッチの 1 フレーム**（`Connected` のメソッド）: `capture` で取ってすぐ送る → `advance` が使えなければ 4 ms ごとに送り直す →
//!   CHECK のフレームなら `submit_check` で記録して送る → seg の境目で `begin_seg`。
//!
//! ## モード
//!
//! `set_mode` で望むモードと D・R を置く（置かなければ delay 方式）。繋がったとき、AGREE で決まったモードが rollback なら `Connected` は
//! rollback の状態機械（`rollback.rs`）も作り、INPUT の受け・取得・待つ間の送り直し・前の seg のバックアップをそちらへ回す（`lockstep()` は seg
//! だけ揃えて置く）。マッチの 1 フレームは、delay 方式なら `capture` → `advance`、rollback のモードなら `capture` →
//! `rollback_input_point` → （Present の後）`rollback_post_present`。rollback のモードでは latency の切替を出さない（`request_delay` が
//! `RollbackMode`）。D を替えるのは `press_delay`。
//!
//! ## 候補と経路
//!
//! 相手の候補アドレス（`Config::peers`）を `candidate::Paths` に持つ。経路を決める前、HELLO は全部の候補へ送る。AGREE（ホスト）は相手の
//! HELLO が届いた送り元へだけ送る（全部へ送ると、自分自身へ返る経路で自分の AGREE が返り、ホストが「ゲストの返し」と取り違えてひとりで
//! 繋がる）。HELLO_ACK とゲストの AGREE の返しは、受けたものの送り元へ返す。**最初に「相手が自分を聞いた印」（自分の HELLO への
//! HELLO_ACK か AGREE。どちらもタグが合い、候補のどれかから来たもの）が届いた経路を採る**。以後の送りは全部その経路へ。受けは、決めた
//! 後も候補のどれかからなら受ける（相手から見た自分の経路と、自分から見た相手の経路は同じとは限らない）。
//!
//! ## 学んだ候補
//!
//! 相手のルーターが宛先ごと・時間で外側のポートを付け替えると、相手の HELLO は接続コードのアドレスと別のポートから届く。そこで**繋がる前**の
//! HELLO・HELLO_ACK・AGREE に限り、候補に無い送り元でも、session_id が自分と同じ → タグが通る → 自分の送ったものの跳ね返りでない、の
//! 全部を満たしたら、送り元を**学んだ候補**として足す（`Paths::learn`。上限 `MAX_LEARNED`。ICE の peer reflexive と同じ考え）。以後
//! HELLO は学んだ候補へも送る。満たさないもの・繋がった後の 1 通は、学ばずに `foreign` として数えて捨てる（上限を越えたら
//! `learn_refused`）。学んだ候補は接続コードに載せず、最初からの作り直し（`Restart::Fresh`）で消える。経路の外の第三者は秘密を知らず
//! タグを作れないので学ばせられない（経路の上の者は本物の 1 通を別のポートから送り直せるが、もともと落とせる位置にいる）。
//!
//! ## 自分自身への跳ね返り
//!
//! 候補のどれかが自分自身に返る経路だと（同じ PC・NAT の折り返し・相手のコードに自分のアドレスが混じる）、自分の HELLO が自分に届き、
//! そのまま手順へ渡すと「同じ nonce」で接続を全部やめる。そこで**自分の送ったもの（HELLO と HELLO_ACK のうち role_nonce が自分の
//! もの）を、状態機械へ渡す前に捨てて数える**（`RecvCounts::self_bounce`）。アドレスで見ないのは、NAT の折り返しでは外側のアドレスで
//! 返ってくるため。中身の role_nonce なら、どの経路で返っても自分のものと分かる。
//!
//! ## 時間切れの後の作り直し
//!
//! `set_retry_window` で長さを置くと、手順が時間切れ（10 秒）でやめたとき、`Link::new` の時刻からその長さの間は手順を作り直して待ち続ける
//! （`Handshake::restart`。相手の HELLO を受けていれば相手の値は引き継ぐ）。最初から作り直すときは、経路の選びもやり直す（相手が
//! 起こし直して別のポートから来ることがある）。届いたものを手順へ渡す前にも時間切れを見て作り直す（渡してから時間切れになると、その 1 通を
//! 「やめた後」として捨てるため）。置かなければ作り直さない。
//!
//! ## 使い捨ての鍵と封じた便
//!
//! セッションは `Config::key_seed` から使い捨ての鍵を作り、公開鍵を HELLO / HELLO_ACK に載せる。相手の HELLO で役割を決めたら向きごとの鍵を
//! 作る（`refresh_keys`）。以後、HELLO・HELLO_ACK 以外は封じて送り（`Tx::send`）、開いて受ける。鍵が決まる前に届いた封じた便は捨てて
//! 数える（`before_keys`）。開かない便は `bad_tag` に数える。
//!
//! - **秘密鍵の寿命はセッションと同じ**: セッションの中の作り直しでは、Resume も Fresh も秘密鍵を変えない。Fresh で秘密鍵だけ替えると、相手が自分の
//!   古い公開鍵を覚えたまま鍵が食い違い、封じた便が開かずに繋がらない。
//! - **通し番号は鍵と同じ寿命**: 送り手 `Link` の番号は `Tx` が `Seq` で 1 つ持つ。繋がったら `Tx` ごと `Connected` へ移し、セッションの送りも
//!   `live_tx` でそちらを通す（2 つの送りが同じ番号を取らない）。共有の入れ物（`Arc`）にしないのは、dll がゲームのスレッドでセッションを
//!   作り直す・捨てるので確保も解放もさせないため。受信スレッドの PONG の応答側は別の印（`Sender::Pong`）で、番号は応答側の呼び手が持つ。
//! - 繋がる前に相手がセッションを作り直したとき（同じ nonce・新しい公開鍵の HELLO）は、手順が相手を覚え直し、`refresh_keys` が向きごとの鍵を
//!   作り直す。通し番号は続ける。繋がった後は替えない。
//!
//! ## セッションが持たないもの・呼び手が守ること
//!
//! ソケット・時計・スレッド・ゲームの画面は持たない。送るものは `Outlet` へ宛先とバイト列で渡す。いつ選択に入るか
//! （`enter_selection`）、いつ seg を進めるか、CHECK に何を入れるかは、呼び手がゲーム（か台本）を見て決める。
//!
//! - マッチの場面を出るときと読み込みに入るときに `begin_seg` を呼ぶ。マッチを出た後も seg を進めずにいると、前のマッチの INPUT が遅れて
//!   届いたときに SELECT の送り直しが止まり、両者が待ち続ける。
//! - 繋がったか（`Link::connected` が `Some` になったか）は、`on_datagram` と `poll` の**どちらの後にも**見る。
//!
//! ゲームスレッドで呼ぶので、ヒープを使わず、パニックしうる添字と unwrap を置かない。送るバイト列はスタックの `[u8; MAX_DATAGRAM]` に
//! 作る。

use std::net::SocketAddr;

use crate::auth::{AuthKey, Secret};
use crate::candidate::{
    CandKind, Candidates, LearnError, Paths, canonical_endpoint, kind_of, same_endpoint,
};
use crate::check::{CheckLog, CheckResult, CheckRule};
use crate::handshake::{self, Established, FailReason, Handshake, ModeWish, Restart};
use crate::liveness;
use crate::liveness::Liveness;
use crate::lockstep::{self, Applied, CaptureError, LatError, Lockstep, RecvOutcome, Role};
use crate::match_gate::Gate;
use crate::pong::PongResponder;
use crate::rollback::{self, After, Rollback, Window};
use crate::seal::{DirKeys, Ephemeral, KeySeed, Opener, PUBLIC_LEN, Sealer, Sender, Seq};
use crate::selection::{FrameOut, Press, Selection};
use crate::wire::{
    self, ByeReason, Check, Datagram, DecodeError, FormatValue, HostState, Input, MAX_DATAGRAM,
    MAX_WORDS, Message, Mode, Seat, Select,
};

/// 候補の種類ごとの数の並び: [同じ PC の中（`Loopback`）, 家の中（`Host`）, 外側の IPv4（`External`）, IPv6]。
/// 添字は `kind_slot` の 1 か所で決める。
pub const KIND_SLOTS: usize = 4;

pub fn kind_slot(k: CandKind) -> usize {
    match k {
        CandKind::Loopback => 0,
        CandKind::Host => 1,
        CandKind::External => 2,
        CandKind::Ipv6 => 3,
    }
}

pub trait Outlet {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]);
}

/// 送りの道具（HMAC の鍵と、`Sealer` と通し番号）。HELLO・HELLO_ACK は HMAC のタグ、ほかは封じる（`wire::is_signed`）。
/// コピーできない: コピーが通し番号を数え直すと nonce が重なる。
#[derive(Debug)]
struct Tx {
    session_id: u32,
    key: AuthKey,
    /// 向きごとの鍵が決まった後だけ Some（送り手の印は `Sender::Link`）。
    seal: Option<Sealer>,
    /// 送り手 `Link` の通し番号（セッションに 1 つ。繋がったら `Tx` ごと `Connected` へ移し、セッションの送りもそこを通す）。
    seq: Seq,
    /// 封じる便を、鍵が決まる前か通し番号を使い切ったので送らなかった数（手順の上では起きない）。
    unsent: u64,
}

fn live_tx<'a>(tx: &'a mut Tx, conn: &'a mut Option<Connected>) -> &'a mut Tx {
    match conn {
        Some(c) => &mut c.tx,
        None => tx,
    }
}

impl Tx {
    fn send<O: Outlet>(&mut self, to: SocketAddr, msg: &Message, out: &mut O) -> bool {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = if wire::is_signed(msg) {
            wire::encode(&self.key, self.session_id, msg, &mut buf)
        } else {
            match self
                .seal
                .as_ref()
                .and_then(|s| wire::seal(s, &self.seq, self.session_id, msg, &mut buf))
            {
                Some(n) => n,
                None => {
                    self.unsent = self.unsent.saturating_add(1);
                    return false;
                }
            }
        };
        out.send(to, buf.get(..n).unwrap_or(&[]));
        true
    }
}

/// 相手と決めた向きごとの鍵（役割を決めた相手の値から作った物）。
#[derive(Debug, Clone, Copy)]
struct Keys {
    /// 作ったときの相手の（role_nonce, 公開鍵）。`Handshake::peer_key` と違えば作り直す。
    peer: (u64, [u8; PUBLIC_LEN]),
    dir: DirKeys,
    open: Opener,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub handshake: handshake::Config,
    /// 接続の秘密（接続を始める側が引き、接続コードで渡す）。
    pub secret: Secret,
    /// 相手の候補アドレス（優先の順）。これ以外から来たものは捨てる（繋がる前の手順でタグまで通ったものは、学んだ候補にする）。
    pub peers: Candidates,
    /// 使い捨ての鍵の種。**セッションを作るたびに呼び手が OS の乱数で引く**（core は乱数を持たない）。同じ種を 2 つのセッションに使い回さない
    /// （通し番号はセッションごとに 0 から数えるので、同じ相手・同じ session で同じ種を使うと nonce が重なりうる）。
    pub key_seed: KeySeed,
}

impl Config {
    pub fn single(
        handshake: handshake::Config,
        secret: Secret,
        peer: SocketAddr,
        key_seed: KeySeed,
    ) -> Config {
        Config {
            handshake,
            secret,
            peers: Candidates::from_addrs(&[peer]).unwrap_or_default(),
            key_seed,
        }
    }
}

/// 繋がる前後の受信側で捨てたものの数（ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecvCounts {
    pub datagrams_in: u64,
    /// STUN の応答（受信側で分けた。STUN を聞くのは呼び手）。
    pub stun: u64,
    /// 相手の候補にない送り元から来て、捨てたもの（学ばなかったもの。上限で断った `learn_refused` もここに入る）。
    pub foreign: u64,
    /// 候補に無い送り元を、学んだ候補として足した数（繋がる前の HELLO・HELLO_ACK・AGREE でタグまで通ったものだけ）。
    pub learned: u64,
    /// 学ぶはずだったが、上限（`candidate::MAX_LEARNED`）を越えるので断った数（`foreign` の内数）。
    pub learn_refused: u64,
    pub last_foreign: Option<SocketAddr>,
    /// タグが合わないもの。封じた便が開かないものも同じ数に入れる。
    pub bad_tag: u64,
    /// 封じた便だが、向きごとの鍵がまだ無いので開かずに捨てたもの（相手の HELLO より先に届いた AGREE など）。
    pub before_keys: u64,
    /// 相手の公開鍵が弱い点（共有の値が 0）で、向きごとの鍵を作らなかった回数（秘密を知る相手の壊れた実装しか作れない。`seal.rs`）。
    pub weak_key: u64,
    /// タグは合ったが解けないもの（と、HGRT でも STUN でもないもの）。バージョン違い（`other_version`）は含まない。
    pub decode_errors: u64,
    /// タグは合ったが、ヘッダのバージョンが `wire::PROTO_VER` と違ったもの。繋がる前の 1 通目で接続の手順をやめる。
    pub other_version: u64,
    /// session_id の違うもの（別セッションの遅れたパケット）。
    pub wrong_session: u64,
    /// 自分の送った HELLO / HELLO_ACK が返ってきたもの（モジュール doc の「自分自身への跳ね返り」）。
    pub self_bounce: u64,
    /// 繋がる前に届いた、接続の手順以外のもの（ホストが繋がる前のゲストの MENU は捨ててよい）。
    pub before_established: u64,
    /// 同じ ms に同じ送り元へ同じ返し（HELLO_ACK・AGREE の返し）を出すところだったので重ねなかった数。
    pub reply_folded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnCounts {
    pub input_accepted: u64,
    pub input_prev_seg: u64,
    /// 前の seg の INPUT に、前の seg のバックアップを送り返した数。
    pub input_prev_replies: u64,
    pub input_other_seg: u64,
    /// そのほかの理由で捨てた INPUT（受理期間の外・ホストが受けた切替つき、など。`RecvOutcome`）。
    pub input_rejected: u64,
    /// `input_rejected` のうち、ホストが受けた切替つきの INPUT（`RecvOutcome::GuestLat`）の数。本ログで理由を見分けるため。
    pub input_guest_lat: u64,
    /// `input_rejected` のうち、lat の取り決めに合わないもの（`RecvOutcome::BadLat`）。
    pub input_bad_lat: u64,
    /// 取ろうとしてリングが溢れた回数（`CaptureError::Full`）。
    pub capture_full: u64,
    /// CHECK が一致した回数（自分の値を記録したときと、相手の値を受けたときの両方で数える）。
    pub check_matches: u32,
    /// 受信スレッドがその場で PONG を返していたので、セッションが PONG を重ねなかった PING の数（`pong.rs`）。
    pub pong_by_receiver: u64,
    /// ホストが送った HOST_STATE の数（値が変わった tick の分と 1 秒ごとの分）。
    pub host_state_sent: u64,
    /// ゲストが受け入れた HOST_STATE の数（古い便・重なりは含まない）。
    pub host_state_rx: u64,
    /// ゲストが古い seq（既に受けた最新より後でない）として捨てた HOST_STATE の数。ホストが受けた分（手順の外）も含む。
    pub host_state_stale: u64,
}

/// 接続の手順を作り直した回数と、直前の 1 回（やめた理由と作り直しの形）。モジュール doc の「時間切れの後の作り直し」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Restarts {
    pub count: u32,
    pub last: Option<(FailReason, Restart)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recv {
    /// 呼び手のすることは無い（状態機械が受けた、または捨てた）。
    Nothing,
    /// INPUT を受け入れた。`base` から `words` 入力ワードが届き、受けた後の lockstep は `frame` を待っている。
    Input {
        seg: u16,
        base: u32,
        words: u32,
        frame: u32,
    },
    /// 相手の CHECK が自分の値と合わなかった（呼び手が BYE(ズレ検出) で終える）。
    Desync { local: Check, remote: Check },
    /// 自分の次の seg の INPUT が届いた = 相手は今の seg を出て、次のマッチを始めた（再戦など）。中身は捨てた。dll は使わない。
    NextSeg { seg: u16 },
    /// 選択の外にいる間に、自分と同じ step の相手の MENU が届いた = 相手は次の選択に入った。使い方は `NextSeg` と同じ。
    PeerSelecting,
}

/// 選択の 1 フレームで、ゲームへ入れた後に呼び手が返す、選択を出るかどうか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelExit {
    Stay,
    /// 選択を出る（難易度の画面の取消でタイトルへ）。
    Leave,
    /// マップが決まって選択を出る。自分の SELECT を置く。
    Decided(Select),
}

/// 繋がった後の状態（接続ごとに 1 つ。繋がったときにセッションが作る）。コピーできない（コピーすると送りの通し番号が 2 つになり nonce が重なる）。
#[derive(Debug)]
pub struct Connected {
    /// 送りの道具（セッションの `Tx` のコピー。`Sealer` の通し番号はセッションと共有）。
    tx: Tx,
    /// 採った経路（繋がった後の送りは全部ここへ）。
    path: SocketAddr,
    est: Established,
    sel: Selection,
    ls: Lockstep,
    /// 両者が rollback ができるときだけ Some（取り決めが delay 方式でも作る。キャラセレクトの Tab で形式を替えられるように）。今の形式が
    /// rollback のとき、INPUT の受け・取得・送り直し・seg のバックアップはこちら。眠っている間も `begin_seg` は呼ぶ。
    rb: Option<Rollback>,
    /// 今の対戦の形式。繋がったときは `est.mode`（AGREE）で、`switch_format` で替わる。`est.mode` は繋がったときの値のまま。
    mode: Mode,
    /// 前の seg を動かした形式（`begin_seg` が呼ばれた時点の `mode`）。前の seg の INPUT へのバックアップの返事の持ち主は、いまの機械ではなく
    /// この機械（形式を替えた後に、前の seg の最後の入力ワードを待っている相手へ返せるように）。
    prev_owner: Mode,
    fmt: FormatStats,
    chk: CheckLog,
    live: Liveness,
    /// 最後に INPUT を送った時刻（ms）。待ち中の再送の間隔を数える。
    last_input_send: u32,
    counts: ConnCounts,
    /// 対戦の成立（READY）。`Link::set_match_gate` を置いたセッションだけ Some（`code_side = game`）。None なら繋がったときに成立したことになる。
    gate: Option<Gate>,
    /// ホスト: 最後に HOST_STATE で送った値と時刻（`poll_host_state`）。まだ送っていなければ None。
    hs_tx: Option<HostStateTx>,
    /// ゲスト: 受けた最新の HOST_STATE（古い便は捨てる。`on_host_state`）。まだ受けていなければ None。
    hs_rx: Option<HostState>,
}

/// 本ログに残す形式の切替の件数（新しい方から）。
pub const FORMAT_LOG: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatRec {
    /// 切替を入れた seg（次のマッチの seg。キャラセレクトの間はそのマッチの seg）。
    pub seg: u16,
    /// 切替の後の形式と D・auto の旗（STEP の value）。
    pub value: FormatValue,
    pub from: Mode,
    /// 切替を求めたのは相手か（ホスト: ゲストの要求を STEP にした / ゲスト: 自分は要求していないのに入った）。
    pub by_peer: bool,
}

impl FormatRec {
    const EMPTY: FormatRec = FormatRec {
        seg: 0,
        value: FormatValue {
            mode: Mode::Delay,
            delay: 0,
            auto: false,
        },
        from: Mode::Delay,
        by_peer: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatStats {
    /// 入れ替えた数（形式が替わらなかった STEP も含む —— D・旗の置き直しだけの切替）。
    pub applied: u32,
    /// 形式が実際に替わった数。
    pub changed: u32,
    /// 断った数（片方が rollback ができない組で Format の STEP が来た）。
    pub refused: u32,
    /// 最近の切替（`applied` が `FORMAT_LOG` を越えたら古い方から上書き。並びは `applied % FORMAT_LOG`）。
    pub log: [FormatRec; FORMAT_LOG],
}

impl FormatStats {
    const NEW: FormatStats = FormatStats {
        applied: 0,
        changed: 0,
        refused: 0,
        log: [FormatRec::EMPTY; FORMAT_LOG],
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatSwitch {
    /// 形式が `from` から `to` に替わった（D・旗は STEP の値で置き直した）。
    Changed { from: Mode, to: Mode },
    /// 形式は同じで、D・旗だけ STEP の値で置き直した。
    Reset,
    /// 断った（この組は rollback へ行けない）。何も変えていない。
    Refused,
}

/// ホストが最後に送った HOST_STATE（値が変わったかと、送り直しの時刻を見る）。
#[derive(Debug, Clone, Copy)]
struct HostStateTx {
    seq: u16,
    sent_ms: u32,
    mode: Mode,
    delay: u8,
    auto: bool,
}

/// HOST_STATE の送り直しの間隔（ms）。値が変わったときはすぐ送る。1 秒ごとの便は、落ちた便の穴埋めと、後から繋がった側への値の知らせを
/// 兼ねる（ack は要らない。最新の値だけ持つ）。
pub const HOST_STATE_EVERY_MS: u32 = 1_000;

/// 接続のセッション。接続ごとに 1 つ。コピーできない（`Connected` と同じ）。
#[derive(Debug)]
pub struct Link {
    session_id: u32,
    /// 自分の role_nonce（跳ね返りを見分ける）。
    own_nonce: u64,
    key: AuthKey,
    /// 使い捨ての鍵（セッションと同じ寿命）。
    eph: Ephemeral,
    /// 相手と決めた向きごとの鍵（相手の HELLO で役割を決めた後だけ Some）。
    keys: Option<Keys>,
    tx: Tx,
    paths: Paths,
    /// 相手の HELLO（自分のでない role_nonce）が最初に届いた送り元。経路を決める前の AGREE の宛先。
    hello_from: Option<SocketAddr>,
    hs: Handshake,
    conn: Option<Connected>,
    recv: RecvCounts,
    /// `Link::new` の時刻（作り直せる長さの起点）。
    started: u32,
    /// 作り直せる長さ（ms）。0 なら作り直さない。
    retry_ms: u32,
    restarts: Restarts,
    /// 繋がったときに作る CHECK の記録の比べ方。
    check_rule: CheckRule,
    /// 最後に出した手順の返し（ms・宛先・中身）。同じ ms の重ねを落とす。
    last_reply: Option<(u32, SocketAddr, Message)>,
    /// rollback のモードに決まったときの R（予測の上限。ネットワークに載せない各自の設定）。
    ahead: u8,
    /// rollback のモードに決まったときの時刻合わせ（既定は使う。ネットワークに載せない各自の設定）。
    time_sync: bool,
    /// 繋がった後、READY で対戦の成立を待つか（`set_match_gate`。ネットワークに載せない各自の設定）。
    match_gate: bool,
    /// 送った HELLO の数（宛先の候補の種類ごと。`kind_slot`）。「1 通も送っていない」か「送ったが届かない」かを見分けるため。
    hello_sent: [u32; KIND_SLOTS],
    /// 学んだ候補へ送った HELLO の数。`hello_sent` の種類の枠には入れない（コードの候補へ送った数と混ざると読めなくなる）。
    hello_sent_learned: u32,
}

fn cur_rb(rb: &Option<Rollback>, mode: Mode) -> Option<&Rollback> {
    rb.as_ref().filter(|_| mode == Mode::Rollback)
}

fn cur_rb_mut(rb: &mut Option<Rollback>, mode: Mode) -> Option<&mut Rollback> {
    rb.as_mut().filter(|_| mode == Mode::Rollback)
}

fn is_handshake(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Hello(_) | Message::HelloAck { .. } | Message::Agree { .. }
    )
}

impl Link {
    pub fn new(cfg: Config, now: u32) -> Self {
        let key = AuthKey::derive(&cfg.secret, cfg.handshake.session_id);
        let eph = Ephemeral::from_seed(&cfg.key_seed);
        let mut hs = Handshake::new(cfg.handshake, now);
        hs.set_public(eph.public());
        Self {
            session_id: cfg.handshake.session_id,
            own_nonce: cfg.handshake.role_nonce,
            tx: Tx {
                session_id: cfg.handshake.session_id,
                key: key.clone(),
                seal: None,
                seq: Seq::new(),
                unsent: 0,
            },
            key,
            eph,
            keys: None,
            paths: Paths::new(cfg.peers),
            hello_from: None,
            hs,
            conn: None,
            recv: RecvCounts::default(),
            started: now,
            retry_ms: 0,
            restarts: Restarts::default(),
            check_rule: CheckRule::All,
            last_reply: None,
            ahead: rollback::DEFAULT_AHEAD,
            time_sync: true,
            match_gate: false,
            hello_sent: [0; KIND_SLOTS],
            hello_sent_learned: 0,
        }
    }

    /// 送った HELLO の数（宛先の候補の種類ごと。学んだ候補へ送ったものは含まない）。
    pub fn hello_sent(&self) -> [u32; KIND_SLOTS] {
        self.hello_sent
    }

    pub fn hello_sent_learned(&self) -> u32 {
        self.hello_sent_learned
    }

    pub fn path_is_learned(&self) -> bool {
        self.paths.chosen_is_learned()
    }

    /// 相手の候補の数（種類ごと。`kind_slot` の並び。候補の無い仮のセッションは全部 0）。
    pub fn peer_candidate_counts(&self) -> [u32; KIND_SLOTS] {
        let mut n = [0u32; KIND_SLOTS];
        for c in self.paths.candidates().as_slice() {
            if let Some(x) = n.get_mut(kind_slot(c.kind)) {
                *x = x.saturating_add(1);
            }
        }
        n
    }

    fn count_hello(&mut self, kind: CandKind) {
        if let Some(x) = self.hello_sent.get_mut(kind_slot(kind)) {
            *x = x.saturating_add(1);
        }
    }

    /// 繋がった後、両者の READY が揃うまで対戦を成立させない（`code_side = game` の実行だけ）。**繋がる前に**呼ぶ。置かなければ繋がった
    /// ときに成立したことになり、READY を 1 通も送らない。
    pub fn set_match_gate(&mut self, on: bool) {
        self.match_gate = on;
    }

    pub fn set_time_sync(&mut self, on: bool) {
        self.time_sync = on;
    }

    pub fn set_mode(&mut self, wish: ModeWish, ahead: u8) {
        self.hs.set_wish(wish);
        self.ahead = ahead.clamp(rollback::MIN_AHEAD, rollback::MAX_AHEAD);
    }

    pub fn set_seat_ask(&mut self, ask: crate::seat_pick::SeatAsk) {
        self.hs.set_seat_ask(ask);
    }

    /// rollback が**できる**かを置く（HELLO の +3 の bit1）。**最初の `poll` の前に**呼ぶ。呼ばなければ「できる」。両者ができる組だけ、
    /// キャラセレクトの Tab で形式を替えられる。
    pub fn set_can_rollback(&mut self, can: bool) {
        self.hs.set_can_rollback(can);
    }

    /// 進行中のランダムマッチの一覧に載せてよいか（`[spectate] enable = 1`）を置く（HELLO の +3 の bit2）。**最初の `poll` の前に**呼ぶ。
    /// 呼ばなければ「載せない」。結果は繋がった後の `Established::may_list`。
    pub fn set_listable(&mut self, on: bool) {
        self.hs.set_listable(on);
    }

    pub fn seat_ask(&self) -> crate::seat_pick::SeatAsk {
        self.hs.seat_ask()
    }

    /// 時間切れの後も、`Link::new` の時刻から `window_ms` の間は接続の手順を作り直して待ち続ける。0 なら作り直さない（既定）。
    pub fn set_retry_window(&mut self, window_ms: u32) {
        self.retry_ms = window_ms;
    }

    pub fn restarts(&self) -> Restarts {
        self.restarts
    }

    /// 繋がったときに作る CHECK の記録の比べ方。繋がる前に置く（繋がった後に置いても効かない）。
    pub fn set_check_rule(&mut self, rule: CheckRule) {
        self.check_rule = rule;
    }

    fn maybe_restart(&mut self, now: u32) {
        if self.retry_ms == 0 || self.conn.is_some() {
            return;
        }
        let handshake::State::Failed(why) = self.hs.expire(now) else {
            return;
        };
        if now.wrapping_sub(self.started) >= self.retry_ms {
            return;
        }
        let Some(kind) = self.hs.restart(now) else {
            return;
        };
        if kind == Restart::Fresh {
            // 経路の選びもやり直す（相手が起こし直して別のポートから来ることがある）。
            self.paths = Paths::new(*self.paths.candidates());
            self.hello_from = None;
            // 向きごとの鍵も捨てる（手順が相手を忘れたので、次の HELLO の公開鍵で作り直す）。秘密鍵と通し番号はそのまま。
            self.refresh_keys();
        } else {
            self.paths.unchoose_learned();
            if self
                .hello_from
                .is_some_and(|h| self.paths.learned().contains(h))
            {
                self.hello_from = None;
            }
        }
        self.restarts.count = self.restarts.count.saturating_add(1);
        self.restarts.last = Some((why, kind));
    }

    /// 手順が覚えた相手に向きごとの鍵を合わせる。相手が変わったときだけ作り直し、相手を忘れた（最初から作り直した）なら捨てる。
    fn refresh_keys(&mut self) {
        // 繋がった後は鍵を替えない（`Connected` へ移した `Sealer` がその鍵で送っている）。
        if self.conn.is_some() {
            return;
        }
        let want = self.hs.peer_key();
        if self.keys.map(|k| k.peer) == want {
            return;
        }
        self.keys = want.and_then(|(nonce, public)| {
            // 並びの「前」は role_nonce の大きい側。同じ nonce は手順が先にやめている（`FailReason::SameNonce`）。
            let first = self.own_nonce > nonce;
            let made = self
                .eph
                .dir_keys(&public, first, self.session_id, &self.key);
            if made.is_none() {
                self.recv.weak_key = self.recv.weak_key.saturating_add(1);
            }
            made.map(|dir| Keys {
                peer: (nonce, public),
                dir,
                open: dir.opener(),
            })
        });
        self.tx.seal = self.keys.map(|k| k.dir.sealer(Sender::Link));
        #[cfg(test)]
        if let Some(k) = self.keys {
            tests::remember_keys(k.dir);
        }
    }

    pub fn public_key(&self) -> [u8; PUBLIC_LEN] {
        self.eph.public()
    }

    /// 相手と決めた向きごとの鍵（決まる前は None）。**検査と診断のためだけ**で、ゲームのスレッドの経路では使わない。
    #[doc(hidden)]
    pub fn dir_keys(&self) -> Option<DirKeys> {
        self.keys.map(|k| k.dir)
    }

    /// 送り手 `Link` の次に使う通し番号（本ログと検査。「0 に戻さない」を外から見る）。PONG の応答側の番号は応答側の呼び手が持つ。
    pub fn next_seq(&self) -> u64 {
        self.conn
            .as_ref()
            .map_or_else(|| self.tx.seq.next(), |c| c.tx.seq.next())
    }

    /// 封じる便を、鍵が決まる前か通し番号を使い切ったので送らなかった数（セッションと `Connected` の和）。
    pub fn unsent(&self) -> u64 {
        self.tx
            .unsent
            .saturating_add(self.conn.as_ref().map_or(0, |c| c.tx.unsent))
    }

    pub fn path(&self) -> Option<SocketAddr> {
        self.paths.chosen()
    }

    pub fn handshake_state(&self) -> handshake::State {
        self.hs.state()
    }

    pub fn handshake_counters(&self) -> handshake::Counters {
        self.hs.counters()
    }

    pub fn recv_counts(&self) -> RecvCounts {
        self.recv
    }

    pub fn connected(&self) -> Option<&Connected> {
        self.conn.as_ref()
    }

    pub fn connected_mut(&mut self) -> Option<&mut Connected> {
        self.conn.as_mut()
    }

    pub fn on_datagram<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        out: &mut O,
    ) -> Recv {
        self.on_datagram_marked(from, bytes, now, false, out)
    }

    /// `on_datagram` と同じ。`pong_sent` は、受信スレッドがこの 1 通（PING）にその場で PONG を返したか。返していれば、生存が返す PONG を
    /// 重ねない（閉じた後の BYE は返す）。生存の「届いた」の印は、返したかどうかに依らず付ける。
    pub fn on_datagram_marked<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        pong_sent: bool,
        out: &mut O,
    ) -> Recv {
        self.on_datagram_at(from, bytes, now, now, pong_sent, out)
    }

    /// `on_datagram_marked` と同じ。`rx` はこの 1 通を受信スレッドが受けた時刻で、生存の PONG の往復だけをこれで測る（受けてからセッションへ渡す
    /// までを RTT に足さないため）。
    pub fn on_datagram_at<O: Outlet>(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        now: u32,
        rx: u32,
        pong_sent: bool,
        out: &mut O,
    ) -> Recv {
        let from = canonical_endpoint(from);
        let r = &mut self.recv;
        r.datagrams_in = r.datagrams_in.saturating_add(1);
        match wire::classify(bytes) {
            Datagram::Hgrt => {}
            Datagram::Stun => {
                r.stun = r.stun.saturating_add(1);
                return Recv::Nothing;
            }
            Datagram::Other => {
                r.decode_errors = r.decode_errors.saturating_add(1);
                return Recv::Nothing;
            }
        }
        let probing = !self.paths.accepts(from);
        if probing && (self.conn.is_some() || !wire::peek_is_handshake(bytes)) {
            return self.drop_foreign(from);
        }
        let r = &mut self.recv;
        // タグを比べる前にヘッダの session_id だけを覗いて数え分ける。
        if wire::peek_session(bytes).is_some_and(|s| s != self.session_id) {
            if probing {
                return self.drop_foreign(from);
            }
            r.wrong_session = r.wrong_session.saturating_add(1);
            return Recv::Nothing;
        }
        let pkt = match wire::decode(&self.key, self.keys.as_ref().map(|k| &k.open), bytes) {
            Ok(p) => p,
            // 学ぶ前の 1 通は、タグが通らない・バージョンが違う・解けない、のどれでも学ばず「候補に無い送り元」として捨てる。
            Err(e) if probing => {
                if matches!(e, DecodeError::BadVersion(_)) {
                    r.other_version = r.other_version.saturating_add(1);
                }
                return self.drop_foreign(from);
            }
            Err(DecodeError::BadTag) => {
                r.bad_tag = r.bad_tag.saturating_add(1);
                return Recv::Nothing;
            }
            Err(DecodeError::NotKeyed) => {
                r.before_keys = r.before_keys.saturating_add(1);
                return Recv::Nothing;
            }
            Err(DecodeError::BadVersion(theirs)) => {
                r.other_version = r.other_version.saturating_add(1);
                // タグが合った（＝秘密を知る相手）のにバージョンが違う: 待っても同じ答えなので、10 秒の時間切れを待たずに手順をやめる。
                if self.conn.is_none() {
                    self.hs.refuse_version(theirs);
                }
                return Recv::Nothing;
            }
            Err(_) => {
                r.decode_errors = r.decode_errors.saturating_add(1);
                return Recv::Nothing;
            }
        };
        // 自分の送ったものの跳ね返り。状態機械へ渡す前に捨てる。
        let bounced = match pkt.msg {
            Message::Hello(h) => h.role_nonce == self.own_nonce,
            Message::HelloAck { role_nonce, .. } => role_nonce == self.own_nonce,
            _ => false,
        };
        if bounced {
            // 自分の送ったものは、候補の外から返ってきても学ばない（NAT の折り返しで、自分の外側のアドレスを相手の候補に足さない）。
            if probing {
                return self.drop_foreign(from);
            }
            r.self_bounce = r.self_bounce.saturating_add(1);
            return Recv::Nothing;
        }
        let hs_msg = is_handshake(&pkt.msg);
        if probing && !hs_msg {
            // peek_is_handshake と同じ type なので起きない。学ぶゲートを 1 か所に保つための念のため。
            return self.drop_foreign(from);
        }
        if hs_msg {
            // 渡す前に時間切れを見て作り直す。学ぶのはその後（作り直しは学んだものを消すので、先に学ぶと消える）。
            self.maybe_restart(now);
            if probing && !self.learn_source(from) {
                return self.drop_foreign(from);
            }
            match pkt.msg {
                Message::Hello(_) if self.hello_from.is_none() => self.hello_from = Some(from),
                // 相手が自分を聞いた印。最初の 1 回で経路を決める。
                Message::HelloAck { echo_nonce, .. } if echo_nonce == self.own_nonce => {
                    self.paths.heard_back(from);
                }
                Message::Agree { .. } => {
                    self.paths.heard_back(from);
                }
                _ => {}
            }
            // 繋がった後も渡し続ける（相手の確認が落ちると、相手は送り続けている）。同じ ms に同じ送り元へ同じ返しを出したら重ねない
            // （受けのリングに溜まった相手の HELLO を 1 度に渡されると、1 通ごとに返して相手の受けのリング（64 枠）を溢れさせた）。
            let reply = self.hs.on_packet(&pkt, now);
            // 相手の HELLO で役割を決めたら、返しと AGREE を送る前に向きごとの鍵を作る。
            self.refresh_keys();
            if let Some(r) = reply {
                let key = (now, from, r);
                if self.last_reply != Some(key) {
                    live_tx(&mut self.tx, &mut self.conn).send(from, &r, out);
                    self.last_reply = Some(key);
                } else {
                    self.recv.reply_folded = self.recv.reply_folded.saturating_add(1);
                }
            }
            // `handshake.rs` の呼び方: on_packet の直後にも poll_send を 1 回。
            self.poll_handshake(now, out);
            self.maybe_establish(now);
        }
        let Some(c) = self.conn.as_mut() else {
            if !hs_msg {
                self.recv.before_established = self.recv.before_established.saturating_add(1);
            }
            return Recv::Nothing;
        };
        c.on_packet(&pkt, now, rx, pong_sent, out)
    }

    /// 候補に無い送り元から来たものを、学ばずに捨てた（数える。最後の送り元を残す）。
    fn drop_foreign(&mut self, from: SocketAddr) -> Recv {
        self.recv.foreign = self.recv.foreign.saturating_add(1);
        self.recv.last_foreign = Some(from);
        Recv::Nothing
    }

    /// 送り元 `from` を、学んだ候補として足す。足せたら true。上限で断ったら数える（`learn_refused`）。
    fn learn_source(&mut self, from: SocketAddr) -> bool {
        match self.paths.learn(from) {
            Ok(()) => {
                self.recv.learned = self.recv.learned.saturating_add(1);
                true
            }
            Err(LearnError::Full) => {
                self.recv.learn_refused = self.recv.learn_refused.saturating_add(1);
                false
            }
            Err(LearnError::Unusable) => false,
        }
    }

    /// 受信スレッドの PONG の応答側に渡す値のコピー。繋がる前は None。応答側は相手からの向きの `Opener` と、送り手 `Sender::Pong` の `Sealer` を持つ。
    /// 通し番号は応答側の呼び手が 1 つだけ持って渡すので、何度作っても・何本コピーしても重ならない。
    pub fn pong_responder(&self) -> Option<PongResponder> {
        let c = self.conn.as_ref()?;
        let k = self.keys?;
        Some(
            PongResponder::new(
                self.session_id,
                self.key.clone(),
                k.open,
                k.dir.sealer(Sender::Pong),
                *self.paths.candidates(),
                c.path,
            )
            .with_learned(*self.paths.learned()),
        )
    }

    /// 時計で動くもの。1〜4 ms ごと（フレームの外でも）に呼ぶ。
    pub fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        self.maybe_restart(now);
        self.poll_handshake(now, out);
        self.maybe_establish(now);
        if let Some(c) = self.conn.as_mut() {
            c.poll(now, out);
        }
    }

    /// 接続の手順の送り: HELLO は経路を決めるまで全部の候補へ、AGREE は決めた経路か、相手の HELLO が届いた送り元へ。
    fn poll_handshake<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let ob = self.hs.poll_send(now);
        if let Some(m) = ob.hello {
            match self.paths.chosen() {
                Some(p) => {
                    live_tx(&mut self.tx, &mut self.conn).send(p, &m, out);
                    if self.paths.learned().contains(p) {
                        self.hello_sent_learned = self.hello_sent_learned.saturating_add(1);
                    } else {
                        // 採った経路は候補のどれかのはず。無ければアドレスから種類を推す（数えそこねない）。
                        let kind = self
                            .paths
                            .candidates()
                            .as_slice()
                            .iter()
                            .find(|c| same_endpoint(c.addr, p))
                            .map_or_else(|| kind_of(p), |c| c.kind);
                        self.count_hello(kind);
                    }
                }
                None => {
                    let cands = *self.paths.candidates();
                    for c in cands.as_slice() {
                        live_tx(&mut self.tx, &mut self.conn).send(c.addr, &m, out);
                        self.count_hello(c.kind);
                    }
                    // 学んだ候補へも送る（相手のルーターが付け替えたポートへ、こちらの HELLO を届かせる）。
                    let learned = *self.paths.learned();
                    for &a in learned.as_slice() {
                        live_tx(&mut self.tx, &mut self.conn).send(a, &m, out);
                        self.hello_sent_learned = self.hello_sent_learned.saturating_add(1);
                    }
                }
            }
        }
        if let Some(m) = ob.agree
            && let Some(to) = self.paths.chosen().or(self.hello_from)
        {
            // AGREE は封じる便（ホストは相手の HELLO で役割を決めたときに鍵も作っている）。
            live_tx(&mut self.tx, &mut self.conn).send(to, &m, out);
        }
    }

    fn maybe_establish(&mut self, now: u32) {
        if self.conn.is_some() {
            return;
        }
        let handshake::State::Established(e) = self.hs.state() else {
            return;
        };
        let Some(path) = self
            .paths
            .chosen()
            .or(self.hello_from)
            .or_else(|| self.paths.candidates().as_slice().first().map(|c| c.addr))
        else {
            return;
        };
        // 繋がるのは封じた AGREE を開けたときなので、向きごとの鍵はある。無ければ繋がったことにしない（手順の上では起きない）。
        if self.tx.seal.is_none() {
            return;
        }
        let idle = Tx {
            session_id: self.session_id,
            key: self.key.clone(),
            seal: None,
            seq: Seq::new(),
            unsent: 0,
        };
        let tx = std::mem::replace(&mut self.tx, idle);
        self.conn = Some(Connected {
            tx,
            path,
            est: e,
            sel: Selection::new(e.role),
            // 読み込み中と選択の間に届く相手の INPUT も受けられるよう、繋がったときに作っておく。
            ls: Lockstep::new(e.role, e.delay),
            // 両者が rollback ができるなら、取り決めが delay 方式でも作っておく（キャラセレクトの Tab で替える）。
            rb: e.rollback_able.then(|| {
                Rollback::new(
                    rollback::Config::rollback(e.seat, e.delay, self.ahead)
                        .with_role(e.role)
                        .with_time_sync(self.time_sync),
                )
            }),
            mode: e.mode,
            prev_owner: e.mode,
            fmt: FormatStats::NEW,
            chk: CheckLog::with_rule(0, self.check_rule),
            live: Liveness::new(self.session_id, now),
            last_input_send: 0,
            counts: ConnCounts::default(),
            gate: self.match_gate.then(|| Gate::new(now)),
            hs_tx: None,
            hs_rx: None,
        });
    }
}

impl Connected {
    pub fn established(&self) -> Established {
        self.est
    }

    pub fn role(&self) -> Role {
        self.est.role
    }

    pub fn seat(&self) -> Seat {
        self.est.seat
    }

    pub fn counts(&self) -> ConnCounts {
        self.counts
    }

    pub fn selection(&self) -> &Selection {
        &self.sel
    }

    /// delay 方式の lockstep。rollback のモードでは入力ワードの交換に使わない（seg だけは `begin_seg` で揃えてある）。
    pub fn lockstep(&self) -> &Lockstep {
        &self.ls
    }

    /// 今の対戦の形式（繋がったときは AGREE で決まった値。キャラセレクトの Tab で替わる）。繋がったときの値は `established().mode`。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 今動かしている rollback の状態機械（今の形式が rollback のときだけ Some。眠っている機械は返さない）。
    pub fn rollback(&self) -> Option<&Rollback> {
        cur_rb(&self.rb, self.mode)
    }

    pub fn seg(&self) -> u16 {
        match self.rollback() {
            Some(rb) => rb.seg(),
            None => self.ls.seg(),
        }
    }

    pub fn captured(&self) -> bool {
        match self.rollback() {
            Some(rb) => rb.captured(),
            None => self.ls.captured(),
        }
    }

    pub fn frame(&self) -> u32 {
        match self.rollback() {
            Some(rb) => rb.frame(),
            None => self.ls.frame(),
        }
    }

    fn build_input(&self) -> Input {
        match self.rollback() {
            Some(rb) => rb.build_input(),
            None => self.ls.build_input(),
        }
    }

    pub fn liveness(&self) -> &Liveness {
        &self.live
    }

    pub fn check_log(&self) -> &CheckLog {
        &self.chk
    }

    /// 繋がった後の受け。全部を生存へ渡し、種類ごとに振り分ける。
    fn on_packet<O: Outlet>(
        &mut self,
        pkt: &wire::Packet,
        now: u32,
        rx: u32,
        pong_sent: bool,
        out: &mut O,
    ) -> Recv {
        if let Some(r) = self.live.on_packet_at(pkt, now, rx) {
            if pong_sent && matches!(r, Message::Pong { .. }) {
                self.counts.pong_by_receiver = self.counts.pong_by_receiver.saturating_add(1);
            } else {
                self.tx.send(self.path, &r, out);
            }
        }
        if matches!(pkt.msg, Message::Pong { .. }) {
            // 往復を測り直した（約 1 秒ごと）。ホストが auto のとき使う推奨値を更新する。
            self.refresh_recommend();
        }
        if let Some(g) = self.gate.as_mut() {
            match pkt.msg {
                Message::Ready => g.on_ready(),
                // 成立後の通信（相手はもう成立している。READY の送り直しをやめる）。READY が来ないまま続けばゲートの無い相手。
                Message::Menu(_)
                | Message::StepReq(_)
                | Message::Step(_)
                | Message::Select(_)
                | Message::Input(_)
                | Message::Check(_) => g.on_post(now),
                _ => {}
            }
        }
        match pkt.msg {
            Message::Menu(m) => {
                let was_active = self.sel.is_active();
                self.sel.on_menu(&m);
                if !was_active && m.step == self.sel.step() {
                    return Recv::PeerSelecting;
                }
            }
            Message::HostState(h) => self.on_host_state(&h),
            Message::StepReq(s) => self.sel.on_step_req(&s),
            Message::Step(s) => self.sel.on_step(&s),
            Message::Select(s) => self.sel.on_select(&s),
            Message::Input(inp) => return self.on_input(&inp, out),
            Message::Check(r) => match self.chk.on_remote(&r) {
                Some(CheckResult::Match { .. }) => {
                    self.counts.check_matches = self.counts.check_matches.saturating_add(1);
                }
                Some(CheckResult::Mismatch { local, remote }) => {
                    return Recv::Desync { local, remote };
                }
                None => {}
            },
            // PING / PONG / BYE は liveness、HELLO / HELLO_ACK / AGREE は handshake、READY は成立のゲートが見た。
            _ => {}
        }
        Recv::Nothing
    }

    fn on_input<O: Outlet>(&mut self, inp: &Input, out: &mut O) -> Recv {
        let outcome = match cur_rb_mut(&mut self.rb, self.mode) {
            Some(rb) => rb.on_input(inp),
            None => self.ls.on_input(inp),
        };
        match outcome {
            RecvOutcome::Accepted { .. } => {
                self.counts.input_accepted = self.counts.input_accepted.saturating_add(1);
                self.sel.on_peer_input();
                Recv::Input {
                    seg: inp.seg,
                    base: inp.base,
                    words: u32::from(inp.count).min(MAX_WORDS as u32),
                    // rollback のモードでは確定の前線 F（本物の入力ワードで使うのはそこから）。
                    frame: match cur_rb(&self.rb, self.mode) {
                        Some(rb) => rb.front(),
                        None => self.ls.frame(),
                    },
                }
            }
            RecvOutcome::PrevSeg { reply } => {
                self.counts.input_prev_seg = self.counts.input_prev_seg.saturating_add(1);
                // バックアップの持ち主は前の seg を動かした機械（`prev_owner`）。形式を替えた直後は、いまの機械は前の seg を動かしていないので、
                // 持ち主に聞き直す。
                let (reply, prev) = if self.prev_owner == self.mode {
                    let prev = match cur_rb(&self.rb, self.mode) {
                        Some(rb) => rb.build_prev_input(),
                        None => self.ls.build_prev_input(),
                    };
                    (reply, prev)
                } else {
                    match (self.prev_owner, self.rb.as_mut()) {
                        (Mode::Rollback, Some(rb)) => (
                            matches!(rb.on_input(inp), RecvOutcome::PrevSeg { reply: true }),
                            rb.build_prev_input(),
                        ),
                        (Mode::Delay, _) => (
                            matches!(self.ls.on_input(inp), RecvOutcome::PrevSeg { reply: true }),
                            self.ls.build_prev_input(),
                        ),
                        (Mode::Rollback, None) => (false, None),
                    }
                };
                if reply && let Some(p) = prev {
                    self.counts.input_prev_replies =
                        self.counts.input_prev_replies.saturating_add(1);
                    self.tx.send(self.path, &Message::Input(p), out);
                }
                Recv::Nothing
            }
            RecvOutcome::OtherSeg => {
                self.counts.input_other_seg = self.counts.input_other_seg.saturating_add(1);
                if inp.seg == self.seg().wrapping_add(1) {
                    Recv::NextSeg { seg: inp.seg }
                } else {
                    Recv::Nothing
                }
            }
            other => {
                self.counts.input_rejected = self.counts.input_rejected.saturating_add(1);
                match other {
                    RecvOutcome::GuestLat => {
                        self.counts.input_guest_lat = self.counts.input_guest_lat.saturating_add(1);
                    }
                    RecvOutcome::BadLat => {
                        self.counts.input_bad_lat = self.counts.input_bad_lat.saturating_add(1);
                    }
                    _ => {}
                }
                Recv::Nothing
            }
        }
    }

    fn poll<O: Outlet>(&mut self, now: u32, out: &mut O) {
        // 成立のゲートは生存より先に見る: 時間切れで閉じた tick に、同じ `poll` で BYE を出すため。
        self.poll_gate(now, out);
        let lo = self.live.poll_send(now);
        for m in [lo.ping, lo.bye].into_iter().flatten() {
            self.tx.send(self.path, &m, out);
        }
        let so = self.sel.poll_send(now);
        for m in [so.step, so.select].into_iter().flatten() {
            self.tx.send(self.path, &m, out);
        }
        self.poll_host_state(now, out);
    }

    // ---- ホストの表示用の状態（HOST_STATE）----

    /// ホスト: 値（形式・目標の D・auto の旗）が変わった tick と、前に送ってから `HOST_STATE_EVERY_MS` 経った tick に、ゲストへ
    /// HOST_STATE を送る。表示用で、D の予定は INPUT の lat が決める。成立の前・生きていない間は送らない。ゲストは何もしない。
    fn poll_host_state<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if self.est.role != Role::Host || !self.matched() {
            return;
        }
        if self.live.state() != liveness::State::Alive || self.live.peer_bye().is_some() {
            return;
        }
        let mode = self.mode;
        let delay = self.shown_delay().min(crate::delay_pick::bounds(mode).1);
        let auto = self.auto_on();
        let seq = match self.hs_tx {
            Some(t)
                if t.mode == mode
                    && t.delay == delay
                    && t.auto == auto
                    && now.wrapping_sub(t.sent_ms) < HOST_STATE_EVERY_MS =>
            {
                return;
            }
            Some(t) => t.seq.wrapping_add(1),
            None => 0,
        };
        self.hs_tx = Some(HostStateTx {
            seq,
            sent_ms: now,
            mode,
            delay,
            auto,
        });
        self.counts.host_state_sent = self.counts.host_state_sent.saturating_add(1);
        self.tx.send(
            self.path,
            &Message::HostState(HostState {
                seq,
                mode,
                delay,
                auto,
            }),
            out,
        );
    }

    fn on_host_state(&mut self, h: &HostState) {
        if self.est.role != Role::Guest {
            self.counts.host_state_stale = self.counts.host_state_stale.saturating_add(1);
            return;
        }
        let newer = self
            .hs_rx
            .is_none_or(|old| (1..0x8000).contains(&h.seq.wrapping_sub(old.seq)));
        if newer {
            self.hs_rx = Some(*h);
            self.counts.host_state_rx = self.counts.host_state_rx.saturating_add(1);
        } else {
            self.counts.host_state_stale = self.counts.host_state_stale.saturating_add(1);
        }
    }

    /// ゲスト: 受けた最新の HOST_STATE（まだ受けていなければ `None`。ホストは常に `None`）。
    pub fn host_state(&self) -> Option<HostState> {
        self.hs_rx
    }

    /// 画面の「Latency N (A)」の auto の印: ホストは自分の旗、ゲストは自分の要求が出ている間はそれ（auto の要求なら立つ・
    /// 数字なら降りる）、無ければホストの旗（HOST_STATE の最新。受けていなければ降りている）。
    pub fn shown_auto(&self) -> bool {
        match self.est.role {
            Role::Host => self.auto_on(),
            Role::Guest => match self.own_wish() {
                Some(x) => x == crate::delay_pick::AUTO,
                None => self.hs_rx.is_some_and(|h| h.auto),
            },
        }
    }

    pub fn own_wish(&self) -> Option<u8> {
        match self.rollback() {
            Some(rb) => rb.own_wish(),
            None => self.ls.own_wish(),
        }
    }

    /// 画面に出す目標の D。`in_match` は INPUT が流れている局面か。マッチの外はゲストに INPUT の予定が届かないので、ホストの値は
    /// HOST_STATE の最新で出す（ゲスト自身の要求が出ている間はそちらを優先）。マッチの間は INPUT の予定の値。
    pub fn shown_delay_for(&self, in_match: bool) -> u8 {
        if self.est.role == Role::Guest
            && !in_match
            && self.own_wish().is_none_or(|x| x == crate::delay_pick::AUTO)
            && let Some(h) = self.hs_rx
        {
            return h.delay;
        }
        self.shown_delay()
    }

    // ---- 対戦の成立（READY）----

    fn poll_gate<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let alive = self.live.state() == liveness::State::Alive && self.live.peer_bye().is_none();
        let Some(g) = self.gate.as_mut() else {
            return;
        };
        if !alive {
            return;
        }
        if g.check_close(now).is_some() {
            self.live.close(ByeReason::Cancelled);
        } else if g.poll_send(now) {
            self.tx.send(self.path, &Message::Ready, out);
        }
    }

    pub fn match_gate(&self) -> Option<&Gate> {
        self.gate.as_ref()
    }

    /// 対戦が成立したか（ゲートの無いセッションは、繋がったときに成立している）。
    pub fn matched(&self) -> bool {
        self.gate.as_ref().is_none_or(Gate::matched)
    }

    pub fn set_ready<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if self.gate.as_mut().is_some_and(Gate::set_local) {
            self.poll_gate(now, out);
        }
    }

    // ---- 選択 ----

    pub fn enter_selection(&mut self) {
        self.sel.enter();
    }

    /// 選択を出る。押下を入れずに選択の画面の外へ出たとき（手順の外。dll は数える）。
    pub fn leave_selection(&mut self) {
        self.sel.leave();
    }

    pub fn selection_frame<O: Outlet, F: FnOnce(&FrameOut) -> SelExit>(
        &mut self,
        local: u16,
        press: Option<Press>,
        ready_ok: bool,
        out: &mut O,
        apply: F,
    ) -> FrameOut {
        let fo = self.sel.local_frame(local, press, ready_ok);
        match apply(&fo) {
            SelExit::Stay => {}
            SelExit::Leave => self.sel.leave(),
            SelExit::Decided(s) => {
                self.sel.leave();
                self.sel.set_local_select(&s);
            }
        }
        if let Some(m) = self.sel.build_menu() {
            self.tx.send(self.path, &Message::Menu(m), out);
        }
        fo
    }

    /// 自分の SELECT を置く。dll は、マップの決定を入れた tick には `SelExit::Leave` を返し、次の tick（止める地点）でゲームから読んだ値を
    /// ここで置く。
    pub fn set_local_select(&mut self, s: &Select) {
        self.sel.set_local_select(s);
    }

    // ---- マッチ ----

    /// seg を進める（マッチの場面を出るとき・読み込みに入るとき）。lockstep（と rollback の状態機械）と CHECK の全部。眠っている機械にも
    /// 呼び、前の seg を動かした形式（いまの形式）を覚える。
    pub fn begin_seg(&mut self, seg: u16) {
        self.prev_owner = self.mode;
        self.ls.begin_seg(seg);
        if let Some(rb) = self.rb.as_mut() {
            rb.begin_seg(seg);
        }
        self.chk.begin_seg(seg);
    }

    /// latency の切替を要求する（ホスト）。取得の前に呼ぶ。rollback のモードでは出さない（`LatError::RollbackMode`）。Ctrl＋数字の押下は
    /// `press_delay`。
    pub fn request_delay(&mut self, delay: u8) -> Result<(), LatError> {
        if self.rollback().is_some() {
            return Err(LatError::RollbackMode);
        }
        self.ls.request_delay(delay)
    }

    pub fn capture<O: Outlet>(
        &mut self,
        local: u16,
        now: u32,
        out: &mut O,
    ) -> Result<(), CaptureError> {
        let r = match cur_rb_mut(&mut self.rb, self.mode) {
            Some(rb) => rb.capture(local),
            None => self.ls.capture(local),
        };
        match r {
            Ok(()) => {
                self.tx
                    .send(self.path, &Message::Input(self.build_input()), out);
                self.last_input_send = now;
            }
            Err(CaptureError::Full) => {
                self.counts.capture_full = self.counts.capture_full.saturating_add(1);
            }
            // ゲストが seg の D を知るまで。ホストへは入力ワード 0 個の INPUT を 4 ms ごとに送り直す。
            Err(CaptureError::NoDelay) => self.resend_if_due(now, out),
            Err(CaptureError::AlreadyCaptured) => {}
        }
        r
    }

    /// この seg の始まりの D（rollback のモードは `Rollback::seg_delay`）、delay 方式は今使っている遅れ。ゲストが D 未定の間は前の seg の
    /// 最後の D。表示・記録の「今の D」は `cur_delay`。
    pub fn seg_delay(&self) -> u8 {
        match self.rollback() {
            Some(rb) => rb.seg_delay(),
            None => self.ls.delay(),
        }
    }

    /// 取得してよいか: rollback のモードで、この seg の D が決まっている（ゲストは `begin_seg` の後、ホストのその seg の INPUT を受ける
    /// まで false）。delay 方式はいつも true。false の間、呼び手は自分の入力を読まずに `wait_delay` で待つ。
    pub fn delay_known(&self) -> bool {
        self.rollback().is_none_or(Rollback::delay_known)
    }

    /// D 未定の間の待ち: 数えて、入力ワード 0 個の INPUT を 4 ms ごとに送り直す（ホストに生きていると知らせる）。
    pub fn wait_delay<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if let Some(rb) = cur_rb_mut(&mut self.rb, self.mode) {
            rb.note_delay_wait();
        }
        self.resend_if_due(now, out);
    }

    /// 遅れの量の押下（両者）。rollback のモードは `Rollback::press_delay`、delay 方式は `Lockstep::press_delay`。範囲は呼び手が詰める
    /// （`delay_pick::DelayPick`）。ホストはゲストの要求を同じ範囲に詰める。
    ///
    /// `x` が `delay_pick::AUTO`（数字キーの 9）なら auto: ホストは旗を立てて今の推奨値にし、ゲストはホストへ要求する。0〜8 の値は
    /// ホストの auto の旗を降ろす（後勝ち）。
    pub fn press_delay(&mut self, x: u8) -> lockstep::DelayPress {
        if x == crate::delay_pick::AUTO {
            self.refresh_recommend();
        }
        // ゲストの画面に auto の印が立っているときに 0〜8 を押したら、今の値と同じでも要求する（ゲストはホストの旗を知らない）。
        let force =
            self.est.role == Role::Guest && x != crate::delay_pick::AUTO && self.shown_auto();
        match cur_rb_mut(&mut self.rb, self.mode) {
            Some(rb) => rb.press_delay_forced(x, force),
            None => self.ls.press_delay_forced(x, force),
        }
    }

    // ---- 対戦の形式の切替（キャラセレクトの Tab）----

    /// 形式を替えられるか: 両者が rollback ができる（HELLO の印の AND）。偽の組では Tab は効かない（`switch_format` も断る）。
    pub fn rollback_able(&self) -> bool {
        self.est.rollback_able && self.rb.is_some()
    }

    pub fn format_stats(&self) -> FormatStats {
        self.fmt
    }

    /// ホスト: 形式を `target` へ替える STEP の value。D は、auto の旗が立っていれば新しい形式の推奨値（測れていなければ今の目標）、
    /// そうでなければ今の目標を、新しい形式の範囲に詰めた値。旗はホストの旗。
    pub fn format_value_for(&self, target: Mode) -> FormatValue {
        let auto = self.auto_on();
        let (lo, hi) = crate::delay_pick::bounds(target);
        let d = if auto {
            crate::delay_pick::recommend(target, self.rtt_for_recommend_ms())
                .unwrap_or_else(|| self.shown_delay())
        } else {
            self.shown_delay()
        };
        FormatValue {
            mode: target,
            delay: d.clamp(lo, hi),
            auto,
        }
    }

    /// 形式の切替の STEP を入れた tick に呼ぶ（両者）。新しい形式の機械の seg の始まりを STEP の D・旗で作り直し、前の機械は眠らせる。前の
    /// seg を動かした機械の覚え（`prev_owner`）は変えない。両者が rollback ができる組でなければ断って何も変えない。`by_peer` は、この
    /// 切替を求めたのが相手か（本ログ）。形式が替わらなくても D・旗は置き直す（`FormatSwitch::Reset`）。
    pub fn switch_format(&mut self, v: FormatValue, by_peer: bool) -> FormatSwitch {
        if !self.rollback_able() {
            self.fmt.refused = self.fmt.refused.saturating_add(1);
            return FormatSwitch::Refused;
        }
        let from = self.mode;
        self.mode = v.mode;
        match v.mode {
            Mode::Rollback => {
                if let Some(rb) = self.rb.as_mut() {
                    rb.restart_head(v.delay, v.auto);
                }
            }
            Mode::Delay => self.ls.restart_head(v.delay, v.auto),
        }
        // 推奨値は新しい形式の式で引き直して、新しい機械へ渡す（眠っている機械にも渡す。使うのは動かしている機械だけ）。
        self.refresh_recommend();
        let i = (self.fmt.applied as usize) % FORMAT_LOG;
        let rec = FormatRec {
            seg: self.seg(),
            value: v,
            from,
            by_peer,
        };
        if let Some(slot) = self.fmt.log.get_mut(i) {
            *slot = rec;
        }
        self.fmt.applied = self.fmt.applied.saturating_add(1);
        if from == v.mode {
            FormatSwitch::Reset
        } else {
            self.fmt.changed = self.fmt.changed.saturating_add(1);
            FormatSwitch::Changed { from, to: v.mode }
        }
    }

    /// 今の推奨値（`delay_pick::recommend`）: 直近の往復の中央値（3 回に満たなければ無い）、無ければ最後の往復。どちらも無ければ
    /// `None`。
    pub fn recommend_now(&self) -> Option<u8> {
        crate::delay_pick::recommend(self.mode, self.rtt_for_recommend_ms())
    }

    /// 推奨値の素と「上げる判断用」の組（同じ往復から引く。素は `recommend_now` と同じ値）。
    pub fn recommend_pair_now(&self) -> Option<crate::delay_pick::Recommend> {
        crate::delay_pick::Recommend::from_rtt(self.mode, self.rtt_for_recommend_ms())
    }

    fn rtt_for_recommend_ms(&self) -> Option<u32> {
        self.live
            .rtt_median_us()
            .map(|us| us / 1_000)
            .or_else(|| self.live.rtt_ms())
    }

    fn refresh_recommend(&mut self) {
        let r = self.recommend_pair_now();
        self.ls.set_recommend(r);
        if let Some(rb) = self.rb.as_mut() {
            rb.set_recommend(r);
        }
    }

    /// ホスト: auto の旗が立っているか（`delay_pick::AUTO` を押した後、数字を押すまで）。ゲストは旗を持たない。
    pub fn auto_on(&self) -> bool {
        match self.rollback() {
            Some(rb) => rb.auto_on(),
            None => self.ls.auto_on(),
        }
    }

    /// 画面に出す「目標」の遅れ（`Rollback::shown_delay` / `Lockstep::shown_delay`）。
    pub fn shown_delay(&self) -> u8 {
        match self.rollback() {
            Some(rb) => rb.shown_delay(),
            None => self.ls.shown_delay(),
        }
    }

    /// 今の遅れ（rollback のモードは次に新しく取るフレームの D）。記録・本ログの D。穴埋めの判定には使わない（`seg_delay`）。
    pub fn cur_delay(&self) -> u8 {
        match self.rollback() {
            Some(rb) => rb.cur_delay(),
            None => self.ls.delay(),
        }
    }

    /// 今のフレームを使えれば使う（delay 方式）。使えなければ、前に送ってから `RESEND_MS` 経っていれば送り直す。rollback のモードでは
    /// 使わない。
    pub fn advance<O: Outlet>(&mut self, now: u32, out: &mut O) -> Option<Applied> {
        if self.rollback().is_none()
            && let Some(a) = self.ls.advance()
        {
            return Some(a);
        }
        self.resend_if_due(now, out);
        None
    }

    /// 待つ間の送り直し（前に送ってから `RESEND_MS` 経っていれば送る）。使えるフレームをわざと使わずに待つ呼び手（peer_sim の
    /// 「付いていく」形）は、`advance` の代わりに呼ぶ。
    pub fn resend_if_due<O: Outlet>(&mut self, now: u32, out: &mut O) {
        if now.wrapping_sub(self.last_input_send) >= lockstep::RESEND_MS {
            self.tx
                .send(self.path, &Message::Input(self.build_input()), out);
            self.last_input_send = now;
        }
    }

    /// rollback のモードの入力の地点（`Rollback::input_point`。`capture` の後に呼ぶ）。待つなら 4 ms ごとに送り直す。delay 方式の接続
    /// では None。
    ///
    /// CHECK は `w` が受け取る。送るのは呼び手（この呼び出しの後で `submit_check` へ）。`Window` の中からセッションを触れないので、ここでは送らない。
    pub fn rollback_input_point<W: Window, O: Outlet>(
        &mut self,
        w: &mut W,
        now: u32,
        out: &mut O,
    ) -> Option<rollback::Step> {
        let step = cur_rb_mut(&mut self.rb, self.mode)?.input_point(w);
        if matches!(step, rollback::Step::Wait(_)) {
            self.resend_if_due(now, out);
        }
        Some(step)
    }

    /// rollback のモードの Present の後（`Rollback::post_present`）。delay 方式の接続では None。
    pub fn rollback_post_present<W: Window>(&mut self, w: &mut W) -> Option<After> {
        Some(cur_rb_mut(&mut self.rb, self.mode)?.post_present(w))
    }

    pub fn rollback_glide_ppm(&mut self, p: &rollback::GlideParams, frame_us: u32) -> i32 {
        let rtt_us = self.live.rtt_median_us();
        match cur_rb_mut(&mut self.rb, self.mode) {
            Some(rb) => rb.glide_ppm_with(p, rtt_us, frame_us),
            None => 0,
        }
    }

    /// CHECK のフレームを使った後の自分の値。記録して送る。
    pub fn submit_check<O: Outlet>(&mut self, c: Check, out: &mut O) -> Option<CheckResult> {
        let r = self.chk.record_local(c);
        if let Some(CheckResult::Match { .. }) = r {
            self.counts.check_matches = self.counts.check_matches.saturating_add(1);
        }
        self.tx.send(self.path, &Message::Check(c), out);
        r
    }

    // ---- 終わり ----

    /// 自分から終える（`Liveness::close`）。BYE は次の `poll` か `flush_bye` で出る。
    pub fn close(&mut self, reason: ByeReason) {
        self.live.close(reason);
    }

    /// 出すことになっている BYE だけを今すぐ出す（実行を終える直前。PING は出さない）。
    pub fn flush_bye<O: Outlet>(&mut self, now: u32, out: &mut O) {
        let lo = self.live.poll_send(now);
        if let Some(b) = lo.bye {
            self.tx.send(self.path, &b, out);
        }
    }
}

#[cfg(test)]
mod tests {
    //! 組み上がった形（本物の UDP の上で 2 つを繋ぐ）は peer_sim の `tests/loopback.rs`。
    use super::*;
    use crate::handshake::Name;
    use crate::match_gate::{MATCH_WAIT_MS, READY_RESEND_MS};

    const SID: u32 = 0x1234_5678;
    const PORT_A: u16 = 40_001;
    const PORT_B: u16 = 40_002;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    const SECRET: Secret = Secret([0x3C; 16]);

    fn hs_cfg(nonce: u64) -> handshake::Config {
        handshake::Config {
            session_id: SID,
            role_nonce: nonce,
            latency: 2,
            name: Name::new("t"),
            host_seat: Seat::P1,
        }
    }

    fn link(peer: u16, nonce: u64) -> Link {
        Link::new(
            Config::single(hs_cfg(nonce), SECRET, addr(peer), seed_of(nonce)),
            0,
        )
    }

    /// 検査の使い捨ての鍵の種（nonce から決める。本物は dll が OS の乱数で引く）。
    pub(super) fn seed_of(nonce: u64) -> KeySeed {
        let mut s = [0u8; 32];
        for (i, b) in s.iter_mut().enumerate() {
            *b = (nonce as u8).wrapping_mul(31).wrapping_add(i as u8) ^ 0x5C;
        }
        KeySeed(s)
    }

    fn public_of(nonce: u64) -> [u8; PUBLIC_LEN] {
        Ephemeral::from_seed(&seed_of(nonce)).public()
    }

    thread_local! {
        /// この検査のスレッドでセッションが作った向きごとの鍵（`dec` がセッションの送った封じた便を外から開くため。検査ごとにスレッドが別）。
        static KEYS: std::cell::RefCell<Vec<DirKeys>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// セッションが向きごとの鍵を作ったら覚える（`Link::refresh_keys` が検査の建て方でだけ呼ぶ）。
    pub(super) fn remember_keys(k: DirKeys) {
        KEYS.with(|v| {
            let mut v = v.borrow_mut();
            if !v.contains(&k) {
                v.push(k);
            }
        });
    }

    /// 秘密 `SECRET`・session `sid` の鍵で詰める。HMAC の便（HELLO・HELLO_ACK）だけ。封じる便は `enc_to`（どのセッションへ渡すかで鍵が決まる）。
    fn enc(sid: u32, msg: &Message) -> Vec<u8> {
        assert!(wire::is_signed(msg), "封じる便は enc_to で作る: {msg:?}");
        enc_with(&SECRET, sid, sid, msg)
    }

    fn enc_with(secret: &Secret, key_sid: u32, sid: u32, msg: &Message) -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = wire::encode(&AuthKey::derive(secret, key_sid), sid, msg, &mut buf);
        buf[..n].to_vec()
    }

    /// セッション `to` へ、その相手として封じた便を作る（`to` の受ける向きの鍵 = 相手の送る向き。ヘッダの session は `sid`）。
    fn enc_to(to: &Link, sid: u32, msg: &Message) -> Vec<u8> {
        let keys = to.dir_keys().expect("鍵の交換の後の箱").swapped();
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = wire::seal(
            &keys.sealer(crate::seal::Sender::Link),
            &Seq::new(),
            sid,
            msg,
            &mut buf,
        )
        .expect("封じられる");
        buf[..n].to_vec()
    }

    /// セッションが送った 1 通を読む（HMAC の便はそのまま、封じた便はこのスレッドのセッションが作った鍵のどれかで開く）。
    fn dec(b: &[u8]) -> Result<wire::Packet, DecodeError> {
        let key = AuthKey::derive(&SECRET, SID);
        let first = wire::decode(&key, None, b);
        if first != Err(DecodeError::NotKeyed) {
            return first;
        }
        KEYS.with(|v| {
            for k in v.borrow().iter() {
                for o in [k.opener(), k.swapped().opener()] {
                    if let Ok(p) = wire::decode(&key, Some(&o), b) {
                        return Ok(p);
                    }
                }
            }
            Err(DecodeError::BadTag)
        })
    }

    #[derive(Default)]
    struct Q(Vec<Vec<u8>>, Vec<SocketAddr>);

    impl Outlet for Q {
        fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
            self.0.push(bytes.to_vec());
            self.1.push(to);
        }
    }

    impl Q {
        fn take(&mut self) -> Vec<Message> {
            self.1.clear();
            self.0
                .drain(..)
                .map(|b| dec(&b).expect("箱が解けないものを送った").msg)
                .collect()
        }
    }

    fn select() -> Select {
        Select {
            match_no: 0,
            chara: 3,
            color: 0,
            life: 10,
            charge: 0,
            map: 0,
            difficulty: 0,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 0,
            rng_seed: 7,
            option_digest: 0,
        }
    }

    fn decide() -> Press {
        Press {
            kind: crate::wire::StepKind::Decide,
            value: 0,
            screen: 0,
        }
    }

    fn check(frame: u32, rng: u16) -> Check {
        Check {
            seg: 0,
            frame,
            rng_state: rng,
            rng_count: 1,
            life_1p: 10,
            life_2p: 10,
        }
    }

    fn input(seg: u16, ack: u32) -> Message {
        Message::Input(Input {
            seg,
            ack,
            base: 0,
            lat: None,
            count: 0,
            words: [0; MAX_WORDS],
        })
    }

    struct Pair {
        a: Link,
        b: Link,
        qa: Q,
        qb: Q,
        now: u32,
        /// B が最初に送った HELLO（繋がった後に A へもう一度渡す検査に使う）。
        b_hello: Option<Vec<u8>>,
        /// B が最初に送った AGREE（バージョン 5: 繋がった後に A へもう一度渡し、セッションの封じた返しを出させる検査に使う）。
        b_agree: Option<Vec<u8>>,
    }

    impl Pair {
        fn exchange(&mut self) -> (Vec<Recv>, Vec<Recv>) {
            let (mut ra, mut rb) = (Vec::new(), Vec::new());
            // 候補は 1 つずつなので、宛先はいつも相手（`fault_matrix_*` が宛先ごとに振り分ける形を見る）。
            self.qa.1.clear();
            self.qb.1.clear();
            for bytes in std::mem::take(&mut self.qa.0) {
                rb.push(
                    self.b
                        .on_datagram(addr(PORT_A), &bytes, self.now, &mut self.qb),
                );
            }
            for bytes in std::mem::take(&mut self.qb.0) {
                if self.b_hello.is_none()
                    && matches!(dec(&bytes).map(|p| p.msg), Ok(Message::Hello(_)))
                {
                    self.b_hello = Some(bytes.clone());
                }
                if self.b_agree.is_none()
                    && matches!(dec(&bytes).map(|p| p.msg), Ok(Message::Agree { .. }))
                {
                    self.b_agree = Some(bytes.clone());
                }
                ra.push(
                    self.a
                        .on_datagram(addr(PORT_B), &bytes, self.now, &mut self.qa),
                );
            }
            (ra, rb)
        }

        fn connect() -> Pair {
            Pair::connect_links(link(PORT_B, 1), link(PORT_A, 2))
        }

        /// 望むモードを置いてから繋ぐ（A はゲスト、B はホスト。R は A 3・B 12 —— R は各自の設定で、揃えない）。
        fn connect_modes(wa: ModeWish, wb: ModeWish) -> Pair {
            let (mut a, mut b) = (link(PORT_B, 1), link(PORT_A, 2));
            a.set_mode(wa, 3);
            b.set_mode(wb, 12);
            Pair::connect_links(a, b)
        }

        fn connect_links(a: Link, b: Link) -> Pair {
            let mut p = Pair {
                a,
                b,
                qa: Q::default(),
                qb: Q::default(),
                now: 0,
                b_hello: None,
                b_agree: None,
            };
            for _ in 0..200 {
                p.now += 1;
                p.a.poll(p.now, &mut p.qa);
                p.b.poll(p.now, &mut p.qb);
                p.exchange();
                if p.a.connected().is_some() && p.b.connected().is_some() {
                    break;
                }
            }
            assert!(
                p.a.connected().is_some() && p.b.connected().is_some(),
                "繋がらない"
            );
            for _ in 0..4 {
                p.exchange();
            }
            p.qa.0.clear();
            p.qb.0.clear();
            p
        }

        fn ca(&mut self) -> &mut Connected {
            self.a.connected_mut().expect("A は繋がっている")
        }
    }

    #[test]
    fn two_links_connect_with_opposite_roles_and_seats() {
        let p = Pair::connect();
        let (a, b) = (p.a.connected().unwrap(), p.b.connected().unwrap());
        assert_ne!(a.role(), b.role());
        assert_ne!(a.seat(), b.seat());
        assert_eq!(a.lockstep().delay(), b.lockstep().delay());
        assert_eq!(a.established().peer_name.as_str(), "t");
    }

    #[test]
    fn hello_counts_follow_what_was_sent_per_candidate_kind() {
        struct Count(Vec<(SocketAddr, bool)>);
        impl Outlet for Count {
            fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
                let hello = dec(bytes).is_ok_and(|p| matches!(p.msg, Message::Hello(_)));
                self.0.push((to, hello));
            }
        }
        let lan: SocketAddr = "192.168.9.5:40010".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:40011".parse().unwrap();
        let cfg = Config {
            handshake: hs_cfg(1),
            secret: SECRET,
            peers: Candidates::from_addrs(&[addr(PORT_B), lan, v6]).unwrap(),
            key_seed: seed_of(1),
        };
        let mut a = Link::new(cfg, 0);
        assert_eq!(a.hello_sent(), [0; KIND_SLOTS], "送る前に数えた");
        assert_eq!(a.peer_candidate_counts(), [1, 1, 0, 1]);
        let mut out = Count(Vec::new());
        for now in 0..200 {
            a.poll(now, &mut out);
        }
        let per = |ip: fn(&SocketAddr) -> bool| {
            u32::try_from(out.0.iter().filter(|(t, h)| *h && ip(t)).count()).unwrap()
        };
        let want = [
            per(|t| t.ip().is_loopback()),
            per(|t| t.ip().is_ipv4() && !t.ip().is_loopback()),
            0,
            per(|t| t.ip().is_ipv6()),
        ];
        assert!(
            want[0] > 1 && want[0] == want[1] && want[1] == want[3],
            "{want:?}"
        );
        assert_eq!(a.hello_sent(), want);
        // 経路を採った後の HELLO は、採った経路の種類の欄だけに数える。これが起きるのは手順が時間切れで引き継ぎ（`Restart::Resume`）に
        // なったときだけなので、それを起こす: A の送りのうち B のアドレスへ向いたものだけを B へ渡し、B の返事は AGREE だけ渡さない。
        a.set_retry_window(60_000);
        let mut b = link(PORT_A, 2);
        let mut at_restart: Option<([u32; KIND_SLOTS], u32)> = None;
        let mut hello_after = 0u32;
        for now in 300..30_000u32 {
            let mut qa = Q::default();
            let mut qb = Q::default();
            a.poll(now, &mut qa);
            b.poll(now, &mut qb);
            let to_b: Vec<Vec<u8>> =
                qa.0.iter()
                    .zip(&qa.1)
                    .filter(|(_, t)| **t == addr(PORT_B))
                    .map(|(x, _)| x.clone())
                    .collect();
            for x in to_b {
                b.on_datagram(addr(PORT_A), &x, now, &mut qb);
            }
            for x in std::mem::take(&mut qb.0) {
                if !matches!(dec(&x).map(|p| p.msg), Ok(Message::Agree { .. })) {
                    a.on_datagram(addr(PORT_B), &x, now, &mut Count(Vec::new()));
                }
            }
            if at_restart.is_some() {
                hello_after += u32::try_from(
                    qa.0.iter()
                        .filter(|x| matches!(dec(x).map(|p| p.msg), Ok(Message::Hello(_))))
                        .count(),
                )
                .unwrap();
            }
            if a.restarts().count >= 1 && at_restart.is_none() {
                at_restart = Some((a.hello_sent(), now));
            }
            if at_restart.is_some_and(|(_, t)| now >= t + 200) {
                break;
            }
        }
        let (at_restart, _) = at_restart.expect("作り直しが起きていない");
        assert_eq!(
            a.path(),
            Some(addr(PORT_B)),
            "経路を採ったまま作り直せていない"
        );
        assert_eq!(
            a.restarts().last.map(|l| l.1),
            Some(Restart::Resume),
            "引き継ぐ形の作り直しでない"
        );
        let end = a.hello_sent();
        assert!(
            hello_after > 0,
            "作り直しの後に HELLO を送っていない（この検査が何も見ていない）"
        );
        assert_eq!(
            (end[1], end[2], end[3]),
            (at_restart[1], at_restart[2], at_restart[3]),
            "採った後の HELLO を、採っていない経路の欄へ数えた: {at_restart:?} → {end:?}"
        );
        assert_eq!(
            end[0] - at_restart[0],
            hello_after,
            "採った後の HELLO の数が送った数と合わない"
        );
    }

    // ================= rollback のモード =================

    #[test]
    fn the_box_builds_the_rollback_machine_only_when_both_wish_it() {
        let (dl, rb) = (ModeWish::DELAY, ModeWish::rollback);
        for (wa, wb, want) in [
            (rb(5), rb(4), Mode::Rollback),
            (rb(5), dl, Mode::Delay),
            (dl, rb(4), Mode::Delay),
            (dl, dl, Mode::Delay),
        ] {
            let tag = format!("A {:?} × B {:?}", wa.mode, wb.mode);
            let p = Pair::connect_modes(wa, wb);
            let (a, b) = (p.a.connected().unwrap(), p.b.connected().unwrap());
            assert_eq!(b.role(), Role::Host, "{tag}");
            for (c, who, r) in [(a, "A", 3u8), (b, "B", 12)] {
                assert_eq!(c.mode(), want, "{tag} {who}");
                match want {
                    Mode::Rollback => {
                        let cfg = c.rollback().expect("rollback の状態機械").config();
                        assert_eq!(cfg.mode(), Mode::Rollback, "{tag} {who}");
                        assert_eq!(cfg.delay(), 4, "{tag} {who}: D はホスト（B）の値");
                        assert_eq!(cfg.max_ahead(), r, "{tag} {who}: R は各自の値");
                        assert_eq!(cfg.seat(), c.seat(), "{tag} {who}");
                    }
                    Mode::Delay => {
                        assert!(c.rollback().is_none(), "{tag} {who}");
                        assert_eq!(
                            c.lockstep().delay(),
                            2,
                            "{tag} {who}: 希望 latency 2 の大きい方"
                        );
                    }
                }
            }
        }
    }

    struct NoGame {
        last: u32,
        rows: Vec<(u32, u16, u16)>,
        checks: Vec<Check>,
        predicted: u32,
    }

    impl Window for NoGame {
        fn save(&mut self, _: u32) {}
        fn restore(&mut self, _: u32) {}
        fn tick(&mut self, t: rollback::Tick) -> rollback::TickReport {
            if t.kind == rollback::TickKind::Predicted {
                self.predicted += 1;
            }
            rollback::TickReport {
                boundary: t.frame == self.last,
            }
        }
        fn check_values(&mut self) -> rollback::CheckValues {
            rollback::CheckValues::default()
        }
        fn confirmed(&mut self, f: u32, p1: u16, p2: u16) {
            self.rows.push((f, p1, p2));
        }
        fn check(&mut self, c: &Check) {
            self.checks.push(*c);
        }
    }

    #[test]
    fn a_rollback_connection_confirms_the_same_rows_and_refuses_latency_switches() {
        const LAST: u32 = 90;
        const LAG: u32 = 3;
        let mut p = Pair::connect_modes(ModeWish::rollback(1), ModeWish::rollback(1));
        assert_eq!(
            p.ca().request_delay(4),
            Err(LatError::RollbackMode),
            "rollback のモードで切替を受け付けた"
        );
        let word = |who: usize, f: u32| 0x100 * (who as u16 + 1) + (f as u16 & 0xFF) + 1;
        let mut wins = [0, 1].map(|_| NoGame {
            last: LAST,
            rows: Vec::new(),
            checks: Vec::new(),
            predicted: 0,
        });
        let mut flight: Vec<(u32, usize, Vec<u8>)> = Vec::new();
        let mut ended = [false; 2];
        for _ in 0..2_000 {
            p.now += 1;
            let now = p.now;
            for (who, win) in wins.iter_mut().enumerate() {
                let (l, q) = if who == 0 {
                    (&mut p.a, &mut p.qa)
                } else {
                    (&mut p.b, &mut p.qb)
                };
                let c = l.connected_mut().unwrap();
                if ended[who] {
                    continue;
                }
                if !c.captured() {
                    let f = c.frame();
                    c.capture(word(who, f), now, q).unwrap();
                }
                if c.rollback_input_point(win, now, q) == Some(rollback::Step::SceneEnd) {
                    ended[who] = true;
                    continue;
                }
                c.rollback_post_present(win);
                ended[who] = c.rollback().unwrap().scene_ended();
            }
            for (who, q) in [(0usize, &mut p.qa), (1, &mut p.qb)] {
                for bytes in q.0.drain(..) {
                    flight.push((now + LAG, 1 - who, bytes));
                }
                q.1.clear();
            }
            let (due, keep): (Vec<_>, Vec<_>) = flight.into_iter().partition(|x| x.0 <= now);
            flight = keep;
            for (_, to, bytes) in due {
                let (l, q, from) = if to == 0 {
                    (&mut p.a, &mut p.qa, PORT_B)
                } else {
                    (&mut p.b, &mut p.qb, PORT_A)
                };
                l.on_datagram(addr(from), &bytes, now, q);
            }
            if ended == [true, true] {
                break;
            }
        }
        assert_eq!(ended, [true, true], "場面の終わりに届かない");
        // 参照: 台本の入力ワードから手で組み立てる（D = 1: フレーム 0 は穴埋めの 0、フレーム f は相手と自分の f − 1 の入力ワード）。
        let seat_a = p.a.connected().unwrap().seat();
        let want: Vec<(u32, u16, u16)> = (0..=LAST)
            .map(|f| {
                let (wa, wb) = if f == 0 {
                    (0, 0)
                } else {
                    (word(0, f - 1), word(1, f - 1))
                };
                match seat_a {
                    Seat::P1 => (f, wa, wb),
                    Seat::P2 => (f, wb, wa),
                }
            })
            .collect();
        assert_eq!(wins[0].rows, want, "A の確定の列");
        assert_eq!(wins[1].rows, want, "B の確定の列");
        for w in &wins {
            let frames: Vec<u32> = w.checks.iter().map(|c| c.frame).collect();
            assert_eq!(frames, vec![0, 30, 60, 90]);
        }
        // 予測と進め直しが本当に起きた（遅らせないと 0 のまま通る）。
        for (who, l) in [("A", &p.a), ("B", &p.b)] {
            let s = l.connected().unwrap().rollback().unwrap().stats();
            assert!(s.live_predicted > 0 && s.resims > 0, "{who}: {s:?}");
            assert_eq!(s.faults, 0, "{who}");
        }
        assert!(wins.iter().all(|w| w.predicted > 0));
        let c = p.ca();
        let before = c.counts().input_rejected;
        let mut words = [0u16; MAX_WORDS];
        words[0] = 0x7777;
        let bad = Message::Input(Input {
            seg: c.seg(),
            ack: 0,
            base: c.rollback().unwrap().remote_next(),
            lat: Some(wire::LatChange { frame: 0, delay: 3 }),
            count: 1,
            words,
        });
        let r =
            p.a.on_datagram(addr(PORT_B), &enc_to(&p.a, SID, &bad), p.now, &mut p.qa);
        assert_eq!(r, Recv::Nothing);
        assert_eq!(p.ca().counts().input_rejected, before + 1);
        let c = p.ca().counts();
        let (a_role, b_role) = (p.ca().role(), p.b.connected().unwrap().role());
        assert_ne!(a_role, b_role);
        let bad_to_b = Message::Input(Input {
            seg: p.b.connected().unwrap().seg(),
            ack: 0,
            base: p.b.connected().unwrap().rollback().unwrap().remote_next(),
            lat: Some(wire::LatChange { frame: 0, delay: 3 }),
            count: 1,
            words,
        });
        let r = p.b.on_datagram(
            addr(PORT_A),
            &enc_to(&p.b, SID, &bad_to_b),
            p.now,
            &mut p.qb,
        );
        assert_eq!(r, Recv::Nothing);
        let cb = p.b.connected().unwrap().counts();
        for (role, c) in [(a_role, c), (b_role, cb)] {
            match role {
                Role::Host => assert_eq!((c.input_guest_lat, c.input_bad_lat), (1, 0)),
                Role::Guest => assert_eq!((c.input_guest_lat, c.input_bad_lat), (0, 1)),
            }
        }
    }

    // ================= 形式の切替（キャラセレクトの Tab）=================

    fn word_of(who: usize, f: u32) -> u16 {
        0x100 * (who as u16 + 1) + (f as u16 & 0xFF) + 1
    }

    /// 参照: D の一定のマッチの確定の列。フレーム f は両者の f − D の入力ワード（f < D は穴埋めの 0）。台本の入力ワードから手で組む。
    fn ref_rows(seat_a: Seat, d: u32, last: u32) -> Vec<(u32, u16, u16)> {
        (0..=last)
            .map(|f| {
                let (wa, wb) = if f < d {
                    (0, 0)
                } else {
                    (word_of(0, f - d), word_of(1, f - d))
                };
                match seat_a {
                    Seat::P1 => (f, wa, wb),
                    Seat::P2 => (f, wb, wa),
                }
            })
            .collect()
    }

    fn play_match(p: &mut Pair, last: u32, lag: u32) -> [Vec<(u32, u16, u16)>; 2] {
        let rolled = p.a.connected().unwrap().rollback().is_some();
        assert_eq!(
            rolled,
            p.b.connected().unwrap().rollback().is_some(),
            "両者の形式が揃っていない"
        );
        let mut wins = [0, 1].map(|_| NoGame {
            last,
            rows: Vec::new(),
            checks: Vec::new(),
            predicted: 0,
        });
        let mut delay_rows: [Vec<(u32, u16, u16)>; 2] = [Vec::new(), Vec::new()];
        let seats = [
            p.a.connected().unwrap().seat(),
            p.b.connected().unwrap().seat(),
        ];
        let mut flight: Vec<(u32, usize, Vec<u8>)> = Vec::new();
        let mut ended = [false; 2];
        for _ in 0..4_000 {
            p.now += 1;
            let now = p.now;
            for (who, win) in wins.iter_mut().enumerate() {
                if ended[who] {
                    continue;
                }
                let (l, q) = if who == 0 {
                    (&mut p.a, &mut p.qa)
                } else {
                    (&mut p.b, &mut p.qb)
                };
                let c = l.connected_mut().unwrap();
                if !c.captured() {
                    let f = c.frame();
                    match c.capture(word_of(who, f), now, q) {
                        Ok(()) | Err(CaptureError::NoDelay | CaptureError::AlreadyCaptured) => {}
                        Err(e) => panic!("{e:?}"),
                    }
                }
                if rolled {
                    if c.rollback_input_point(win, now, q) == Some(rollback::Step::SceneEnd) {
                        ended[who] = true;
                        continue;
                    }
                    c.rollback_post_present(win);
                    ended[who] = c.rollback().is_some_and(Rollback::scene_ended);
                } else if let Some(a) = c.advance(now, q) {
                    let row = match seats[who] {
                        Seat::P1 => (a.frame, a.local, a.remote),
                        Seat::P2 => (a.frame, a.remote, a.local),
                    };
                    delay_rows[who].push(row);
                    ended[who] = a.frame == last;
                }
            }
            for (who, q) in [(0usize, &mut p.qa), (1, &mut p.qb)] {
                for bytes in q.0.drain(..) {
                    flight.push((now + lag, 1 - who, bytes));
                }
                q.1.clear();
            }
            let (due, keep): (Vec<_>, Vec<_>) = flight.into_iter().partition(|x| x.0 <= now);
            flight = keep;
            for (_, to, bytes) in due {
                let (l, q, from) = if to == 0 {
                    (&mut p.a, &mut p.qa, PORT_B)
                } else {
                    (&mut p.b, &mut p.qb, PORT_A)
                };
                l.on_datagram(addr(from), &bytes, now, q);
            }
            if ended == [true, true] {
                break;
            }
        }
        assert_eq!(ended, [true, true], "場面の終わりに届かない");
        if rolled {
            let [w0, w1] = wins;
            [w0.rows, w1.rows]
        } else {
            delay_rows
        }
    }

    fn next_seg(p: &mut Pair) -> u16 {
        let next = p.a.connected().unwrap().seg().wrapping_add(1);
        p.ca().begin_seg(next);
        p.b.connected_mut().unwrap().begin_seg(next);
        next
    }

    /// 両者へ同じ形式の切替を入れる（dll は STEP を入れた tick にそれぞれ呼ぶ）。ホストの要求（押した側）を `by_peer` に反映する。
    fn switch_both(p: &mut Pair, v: FormatValue) -> [FormatSwitch; 2] {
        let a = p.ca().switch_format(v, false);
        let b = p.b.connected_mut().unwrap().switch_format(v, true);
        [a, b]
    }

    #[test]
    fn a_format_switch_runs_the_next_match_in_the_new_format_on_both_sides() {
        const LAST: u32 = 40;
        let mut p = Pair::connect_modes(ModeWish::DELAY, ModeWish::DELAY);
        for (who, l) in [("A", &p.a), ("B", &p.b)] {
            let c = l.connected().unwrap();
            assert!(c.rollback_able(), "{who}: 両者ができるのに替えられない組");
            assert_eq!(c.mode(), Mode::Delay, "{who}");
        }
        let seat_a = p.a.connected().unwrap().seat();
        // マッチ 0（seg 0）: delay 方式・D 2（希望 latency 2）。
        let rows = play_match(&mut p, LAST, 3);
        assert_eq!(rows[0], ref_rows(seat_a, 2, LAST), "A 試合 0");
        assert_eq!(rows[1], ref_rows(seat_a, 2, LAST), "B 試合 0");
        let seg = next_seg(&mut p);
        assert_eq!(seg, 1);
        // マッチ 1（seg 1）: rollback・D 1。
        let to_rb = FormatValue {
            mode: Mode::Rollback,
            delay: 1,
            auto: false,
        };
        let r = switch_both(&mut p, to_rb);
        let changed = FormatSwitch::Changed {
            from: Mode::Delay,
            to: Mode::Rollback,
        };
        assert_eq!(r, [changed, changed]);
        for (who, l) in [("A", &p.a), ("B", &p.b)] {
            let c = l.connected().unwrap();
            assert_eq!(c.mode(), Mode::Rollback, "{who}");
            assert_eq!(c.seg(), 1, "{who}: 眠っていた機械の seg が揃っていない");
            assert_eq!(
                c.established().mode,
                Mode::Delay,
                "{who}: 繋がったときの値は残る"
            );
            assert_eq!(c.delay_known(), c.role() == Role::Host, "{who}");
        }
        let rows = play_match(&mut p, LAST, 3);
        assert_eq!(rows[0], ref_rows(seat_a, 1, LAST), "A 試合 1");
        assert_eq!(rows[1], ref_rows(seat_a, 1, LAST), "B 試合 1");
        for (who, l) in [("A", &p.a), ("B", &p.b)] {
            let s = l.connected().unwrap().rollback().unwrap().stats();
            assert!(
                s.live_predicted > 0 && s.resims > 0,
                "{who}: rollback で走っていない {s:?}"
            );
            assert_eq!(s.faults, 0, "{who}");
        }
        next_seg(&mut p);
        // マッチ 2（seg 2）: delay 方式・D 3。
        let to_dl = FormatValue {
            mode: Mode::Delay,
            delay: 3,
            auto: false,
        };
        let back = FormatSwitch::Changed {
            from: Mode::Rollback,
            to: Mode::Delay,
        };
        assert_eq!(switch_both(&mut p, to_dl), [back, back]);
        for l in [&p.a, &p.b] {
            assert_eq!(l.connected().unwrap().lockstep().delay(), 3);
        }
        let rows = play_match(&mut p, LAST, 3);
        assert_eq!(rows[0], ref_rows(seat_a, 3, LAST), "A 試合 2");
        assert_eq!(rows[1], ref_rows(seat_a, 3, LAST), "B 試合 2");
        next_seg(&mut p);
        // マッチ 3（seg 3）: また rollback・D 0。
        let to_rb0 = FormatValue {
            mode: Mode::Rollback,
            delay: 0,
            auto: false,
        };
        assert_eq!(switch_both(&mut p, to_rb0), [changed, changed]);
        let rows = play_match(&mut p, LAST, 3);
        assert_eq!(rows[0], ref_rows(seat_a, 0, LAST), "A 試合 3");
        assert_eq!(rows[1], ref_rows(seat_a, 0, LAST), "B 試合 3");
        let fs = p.b.connected().unwrap().format_stats();
        assert_eq!((fs.applied, fs.changed, fs.refused), (3, 3, 0));
        let last = fs.log[(fs.applied as usize - 1) % FORMAT_LOG];
        assert_eq!(
            (last.seg, last.value, last.from, last.by_peer),
            (3, to_rb0, Mode::Delay, true)
        );
    }

    #[test]
    fn a_format_switch_before_the_first_match_uses_the_step_d_in_seg_zero() {
        const LAST: u32 = 30;
        for (from, to, d) in [
            (Mode::Delay, Mode::Rollback, 3u8),
            (Mode::Rollback, Mode::Delay, 0),
        ] {
            let tag = format!("{from:?} → {to:?} D {d}");
            let wish = |m: Mode| match m {
                Mode::Delay => ModeWish::DELAY,
                Mode::Rollback => ModeWish::rollback(1),
            };
            let mut p = Pair::connect_modes(wish(from), wish(from));
            assert_eq!(p.ca().seg(), 0, "{tag}");
            let v = FormatValue {
                mode: to,
                delay: d,
                auto: false,
            };
            let r = switch_both(&mut p, v);
            assert!(
                r.iter().all(|x| matches!(x, FormatSwitch::Changed { .. })),
                "{tag}"
            );
            for l in [&p.a, &p.b] {
                let c = l.connected().unwrap();
                assert!(c.delay_known(), "{tag}: seg 0 のゲストが D を知らない");
                assert_eq!(c.seg_delay(), d, "{tag}: 頭の D が STEP の値でない");
            }
            let seat_a = p.a.connected().unwrap().seat();
            let rows = play_match(&mut p, LAST, 3);
            assert_eq!(rows[0], ref_rows(seat_a, u32::from(d), LAST), "{tag} A");
            assert_eq!(rows[1], ref_rows(seat_a, u32::from(d), LAST), "{tag} B");
        }
    }

    #[test]
    fn the_previous_seg_reply_comes_from_the_machine_that_ran_that_seg() {
        // (繋がったときの形式, 前の seg を動かした形式, 替える先): 3 つ目は 2 つが違う（繋がったときの形式を持ち主と取り違えるのを落とす）。
        for (initial, from, to) in [
            (Mode::Delay, Mode::Delay, Mode::Rollback),
            (Mode::Rollback, Mode::Rollback, Mode::Delay),
            (Mode::Delay, Mode::Rollback, Mode::Delay),
        ] {
            let tag = format!("{initial:?} 起動 {from:?} → {to:?}");
            let wish = |m: Mode| match m {
                Mode::Delay => ModeWish::DELAY,
                Mode::Rollback => ModeWish::rollback(2),
            };
            let mut p = Pair::connect_modes(wish(initial), wish(initial));
            if initial != from {
                let v = FormatValue {
                    mode: from,
                    delay: 2,
                    auto: false,
                };
                switch_both(&mut p, v);
            }
            assert_eq!(p.ca().mode(), from, "{tag}");
            play_match(&mut p, 12, 2);
            // A だけマッチを出る（B は前の seg の A の最後の入力ワードを待っていて、4 ms ごとに INPUT を送り直す、という場面）。
            p.ca().begin_seg(1);
            let v = FormatValue {
                mode: to,
                delay: 1,
                auto: false,
            };
            assert!(matches!(
                p.ca().switch_format(v, false),
                FormatSwitch::Changed { .. }
            ));
            let owner_prev = match from {
                Mode::Delay => p.ca().lockstep().build_prev_input(),
                Mode::Rollback => p.ca().rb.as_ref().unwrap().build_prev_input(),
            }
            .expect("前の seg のバックアップ");
            assert_eq!(owner_prev.seg, 0, "{tag}");
            let late = Message::Input(Input {
                seg: 0,
                ack: 3,
                base: 0,
                lat: None,
                count: 0,
                words: [0; MAX_WORDS],
            });
            p.qa.0.clear();
            let before = p.ca().counts();
            let r =
                p.a.on_datagram(addr(PORT_B), &enc_to(&p.a, SID, &late), p.now, &mut p.qa);
            assert_eq!(r, Recv::Nothing, "{tag}");
            let after = p.ca().counts();
            assert_eq!(after.input_prev_seg, before.input_prev_seg + 1, "{tag}");
            assert_eq!(
                after.input_prev_replies,
                before.input_prev_replies + 1,
                "{tag}: バックアップを返していない"
            );
            let sent = p.qa.take();
            assert_eq!(
                sent,
                vec![Message::Input(owner_prev)],
                "{tag}: 返したのが前の seg を動かした機械のバックアップでない"
            );
            let done = Message::Input(Input {
                seg: 0,
                ack: u32::from(u8::MAX),
                base: 0,
                lat: None,
                count: 0,
                words: [0; MAX_WORDS],
            });
            p.qa.0.clear();
            p.a.on_datagram(addr(PORT_B), &enc_to(&p.a, SID, &done), p.now, &mut p.qa);
            assert!(p.qa.take().is_empty(), "{tag}: 受け取り済みの相手へ返した");
        }
    }

    #[test]
    fn a_pair_where_one_side_cannot_roll_back_refuses_the_switch() {
        for (a_can, b_can) in [(false, true), (true, false), (false, false)] {
            let tag = format!("A {a_can} × B {b_can}");
            let (mut a, mut b) = (link(PORT_B, 1), link(PORT_A, 2));
            a.set_mode(ModeWish::rollback(2), 3);
            b.set_mode(ModeWish::rollback(2), 3);
            a.set_can_rollback(a_can);
            b.set_can_rollback(b_can);
            let mut p = Pair::connect_links(a, b);
            let v = FormatValue {
                mode: Mode::Rollback,
                delay: 1,
                auto: false,
            };
            for (who, l) in [("A", &mut p.a), ("B", &mut p.b)] {
                let c = l.connected_mut().unwrap();
                assert_eq!(
                    c.mode(),
                    Mode::Delay,
                    "{tag} {who}: 片方ができないのに rollback と決めた"
                );
                assert!(!c.rollback_able(), "{tag} {who}");
                assert!(c.rollback().is_none(), "{tag} {who}");
                assert_eq!(
                    c.switch_format(v, false),
                    FormatSwitch::Refused,
                    "{tag} {who}"
                );
                assert_eq!(c.mode(), Mode::Delay, "{tag} {who}: 断ったのに替えた");
                let fs = c.format_stats();
                assert_eq!(
                    (fs.applied, fs.changed, fs.refused),
                    (0, 0, 1),
                    "{tag} {who}"
                );
            }
        }
        let mut p = Pair::connect_modes(ModeWish::DELAY, ModeWish::DELAY);
        let v = FormatValue {
            mode: Mode::Rollback,
            delay: 1,
            auto: false,
        };
        assert!(matches!(
            p.ca().switch_format(v, false),
            FormatSwitch::Changed { .. }
        ));
    }

    #[test]
    fn a_guest_press_of_the_same_value_still_lowers_the_hosts_auto_flag() {
        use crate::delay_pick::AUTO;
        for (wish, name) in [
            (ModeWish::DELAY, "delay 方式"),
            (ModeWish::rollback(2), "rollback"),
        ] {
            for guest_presses in [true, false] {
                let tag = format!("{name} ゲストが押す {guest_presses}");
                let mut p = Pair::connect_modes(wish, wish);
                assert_eq!(p.b.connected().unwrap().role(), Role::Host, "{tag}");
                p.b.connected_mut().unwrap().press_delay(AUTO);
                assert!(p.b.connected().unwrap().auto_on(), "{tag}: 旗が立たない");
                for _ in 0..8 {
                    p.now += 5;
                    p.a.poll(p.now, &mut p.qa);
                    p.b.poll(p.now, &mut p.qb);
                    p.exchange();
                }
                assert!(
                    p.ca().shown_auto(),
                    "{tag}: ゲストの画面に auto の印が出ない"
                );
                let d = p.ca().shown_delay_for(false);
                let mut q = Pair::connect_modes(wish, wish);
                for _ in 0..8 {
                    q.now += 5;
                    q.a.poll(q.now, &mut q.qa);
                    q.b.poll(q.now, &mut q.qb);
                    q.exchange();
                }
                assert!(!q.ca().shown_auto(), "{tag}");
                let qd = q.ca().shown_delay_for(false);
                assert_eq!(
                    q.ca().press_delay(qd),
                    lockstep::DelayPress::Unchanged,
                    "{tag}: 旗が降りているのに同じ値で要求した"
                );
                if guest_presses {
                    let r = p.ca().press_delay(d);
                    assert!(
                        matches!(r, lockstep::DelayPress::Asked { .. }),
                        "{tag}: 同じ値でも要求するはずが {r:?}"
                    );
                }
                play_match(&mut p, 30, 2);
                assert_eq!(
                    p.b.connected().unwrap().auto_on(),
                    !guest_presses,
                    "{tag}: ホストの旗"
                );
                if guest_presses {
                    // 旗が降りた後は、ゲストが押した値（今と同じ値）のまま。auto の見直しが D を替えたままにしない。
                    assert_eq!(
                        p.b.connected().unwrap().shown_delay(),
                        d,
                        "{tag}: 同じ値の要求で D が動いた"
                    );
                }
            }
        }
    }

    #[test]
    fn host_state_follows_the_format_after_a_switch() {
        let mut p = Pair::connect_modes(ModeWish::DELAY, ModeWish::DELAY);
        for _ in 0..3 {
            p.now += 1;
            p.a.poll(p.now, &mut p.qa);
            p.b.poll(p.now, &mut p.qb);
        }
        p.qa.0.clear();
        p.qb.0.clear();
        let v = FormatValue {
            mode: Mode::Rollback,
            delay: 4,
            auto: false,
        };
        switch_both(&mut p, v);
        p.now += 1;
        p.b.poll(p.now, &mut p.qb);
        p.a.poll(p.now, &mut p.qa);
        let hs: Vec<HostState> =
            p.qb.take()
                .into_iter()
                .filter_map(|m| match m {
                    Message::HostState(h) => Some(h),
                    _ => None,
                })
                .collect();
        assert_eq!(hs.len(), 1, "替えた tick に 1 通出る");
        assert_eq!(
            (hs[0].mode, hs[0].delay, hs[0].auto),
            (Mode::Rollback, 4, false)
        );
        assert!(
            p.qa.take()
                .iter()
                .all(|m| !matches!(m, Message::HostState(_))),
            "ゲストが HOST_STATE を送った"
        );
    }

    #[test]
    fn receive_gate_drops_foreign_bad_wrong_session_and_early_packets() {
        let mut a = link(PORT_B, 1);
        let mut q = Q::default();
        let ping = Message::Ping { send_time: 5 };
        let keys = DirKeys::from_raw([0x3F; 32], [0x3F; 32]);
        let enc = |sid: u32, m: &Message| seal_with(keys, sid, m);
        let r = a.on_datagram(addr(PORT_B + 1), &enc(SID, &ping), 1, &mut q);
        assert_eq!(r, Recv::Nothing);
        let r = a.on_datagram(addr(PORT_B), b"HGxx", 1, &mut q);
        assert_eq!(r, Recv::Nothing);
        let r = a.on_datagram(addr(PORT_B), &enc(SID ^ 1, &ping), 1, &mut q);
        assert_eq!(r, Recv::Nothing);
        // 繋がる前の、接続の手順以外。封じた便は鍵の交換の前なので、開かずに `before_keys` で落ちる。
        let r = a.on_datagram(addr(PORT_B), &enc(SID, &ping), 1, &mut q);
        assert_eq!(r, Recv::Nothing);
        let c = a.recv_counts();
        let got = (
            c.datagrams_in,
            c.foreign,
            c.decode_errors,
            c.wrong_session,
            c.before_keys,
            c.before_established,
        );
        assert_eq!(got, (4, 1, 1, 1, 1, 0));
        assert!(q.0.is_empty(), "捨てたものに返した: {:?}", q.take());
        assert!(a.connected().is_none());
        assert_eq!(c.last_foreign, Some(addr(PORT_B + 1)));
    }

    #[test]
    fn last_foreign_is_the_latest_dropped_sender_only() {
        let mut a = link(PORT_B, 1);
        let mut q = Q::default();
        let ping = seal_with(
            DirKeys::from_raw([0x3F; 32], [0x3F; 32]),
            SID,
            &Message::Ping { send_time: 5 },
        );
        assert_eq!(a.recv_counts().last_foreign, None);
        a.on_datagram(addr(PORT_B + 1), &ping, 1, &mut q);
        a.on_datagram(addr(PORT_B + 2), &ping, 2, &mut q);
        a.on_datagram(addr(PORT_B), &ping, 3, &mut q);
        let c = a.recv_counts();
        assert_eq!((c.foreign, c.last_foreign), (2, Some(addr(PORT_B + 2))));
    }

    #[test]
    fn after_connecting_hello_goes_to_handshake_and_ping_to_liveness() {
        let mut p = Pair::connect();
        let hello = p.b_hello.clone().expect("B の HELLO を見た");
        let now = p.now;
        // 相手の確認が落ちて、相手が HELLO を送り直してきた形。繋がった後も返す。
        p.a.on_datagram(addr(PORT_B), &hello, now, &mut p.qa);
        let sent = p.qa.take();
        assert!(
            sent.iter().any(|m| matches!(m, Message::HelloAck { .. })),
            "繋がった後の HELLO に HELLO_ACK を返していない: {sent:?}"
        );
        assert_eq!(p.a.recv_counts().before_established, 0);
        let ping = enc_to(&p.a, SID, &Message::Ping { send_time: 77 });
        p.a.on_datagram(addr(PORT_B), &ping, now, &mut p.qa);
        assert_eq!(p.qa.take(), vec![Message::Pong { send_time: 77 }]);
    }

    #[test]
    fn a_ping_marked_as_answered_gets_no_second_pong_but_still_counts_as_heard() {
        let mut p = Pair::connect();
        let t0 = p.now;
        let to_a = p.a.dir_keys().expect("繋がった").swapped();
        let ping = |t: u32| seal_with(to_a, SID, &Message::Ping { send_time: t });
        p.a.on_datagram_marked(addr(PORT_B), &ping(11), t0, true, &mut p.qa);
        assert_eq!(p.qa.take(), Vec::<Message>::new(), "PONG を重ねた");
        assert_eq!(p.ca().counts().pong_by_receiver, 1);
        // 対照: 印なしなら今までどおり返す（上の「返さない」がセッションの壊れで出たのではない証拠）。
        p.a.on_datagram_marked(addr(PORT_B), &ping(12), t0, false, &mut p.qa);
        assert_eq!(p.qa.take(), vec![Message::Pong { send_time: 12 }]);
        assert_eq!(p.ca().counts().pong_by_receiver, 1);
        let mut t = t0;
        for k in 0..3 {
            t += 9_000;
            p.a.on_datagram_marked(addr(PORT_B), &ping(100 + k), t, true, &mut p.qa);
            p.a.poll(t, &mut p.qa);
        }
        assert_eq!(p.ca().liveness().state(), crate::liveness::State::Alive);
        p.qa.take();
        // 閉じた後は、印があっても PING に BYE を返す（受信スレッドが止める前の 1 tick に両方が出る形）。
        p.ca().close(ByeReason::Finished);
        p.a.poll(t, &mut p.qa);
        p.qa.take(); // close の直後の BYE
        p.a.on_datagram_marked(addr(PORT_B), &ping(13), t, true, &mut p.qa);
        assert_eq!(p.qa.take(), vec![Message::Bye(ByeReason::Finished)]);
    }

    #[test]
    fn the_pong_rtt_is_taken_at_the_receive_time() {
        let mut p = Pair::connect();
        let t = p.now + 5_000;
        p.a.poll(t, &mut p.qa);
        let sent = p.qa.take();
        assert!(sent.contains(&Message::Ping { send_time: t }), "{sent:?}");
        let pong = enc_to(&p.a, SID, &Message::Pong { send_time: t });
        // 受けたのは t + 2、セッションへ渡したのは t + 17（ゲームのスレッドの次のフレーム）。
        p.a.on_datagram_at(addr(PORT_B), &pong, t + 17, t + 2, false, &mut p.qa);
        assert_eq!(p.ca().liveness().rtt_ms(), Some(2));
        // 対照: 受けた時刻を持たない入口は渡した時刻で測る。
        p.a.on_datagram_marked(addr(PORT_B), &pong, t + 17, false, &mut p.qa);
        assert_eq!(p.ca().liveness().rtt_ms(), Some(17));
    }

    #[test]
    fn a_pong_refreshes_the_recommendation_and_the_auto_press_uses_it() {
        use crate::delay_pick::AUTO;
        use crate::lockstep::DelayPress;
        let mut p = Pair::connect();
        let first = p.ca().recommend_now();
        assert!(
            first.is_some(),
            "繋がった時の PING で測れている（検査の前提）"
        );
        let t = p.now + 5_000;
        p.a.poll(t, &mut p.qa);
        p.qa.take();
        let pong = enc_to(&p.a, SID, &Message::Pong { send_time: t });
        p.a.on_datagram_at(addr(PORT_B), &pong, t + 70, t + 70, false, &mut p.qa);
        assert_eq!(p.ca().liveness().rtt_ms(), Some(70));
        assert_eq!(p.ca().recommend_now(), Some(3), "70 ms → 2.1 フレーム → 3");
        // PONG を受けるだけで、両状態機械へ推奨値が渡る（押す前）。渡るのは素と「−5 ms」の組（70 ms の素は 3、65 ms の側は 2）。
        let want = Some(crate::delay_pick::Recommend { raw: 3, up: 2 });
        assert_eq!(p.ca().recommend_pair_now(), want);
        assert_eq!(p.ca().lockstep().recommend(), want);
        assert_eq!(p.ca().rollback().map_or(want, Rollback::recommend), want);
        // 往復が 3 回以上なら中央値（66 ms）。最後の往復（200 ms）や 1 ms ずれた値（67 ms）ではない。
        for (k, rtt) in [66u32, 66, 66, 200].into_iter().enumerate() {
            let tk = t + 5_000 * (k as u32 + 1);
            p.a.poll(tk, &mut p.qa);
            p.qa.take();
            let pong = enc_to(&p.a, SID, &Message::Pong { send_time: tk });
            p.a.on_datagram_at(addr(PORT_B), &pong, tk + rtt, tk + rtt, false, &mut p.qa);
        }
        assert_eq!(p.ca().liveness().rtt_ms(), Some(200));
        assert_eq!(p.ca().recommend_now(), Some(2), "中央値 66 ms → 2");
        assert_eq!(
            p.ca().lockstep().recommend(),
            Some(crate::delay_pick::Recommend { raw: 2, up: 2 }),
            "中央値 66 ms: 素 2、61 ms の側も 2"
        );
        let host = p.ca().role() == Role::Host;
        let r = p.ca().press_delay(AUTO);
        if host {
            assert!(
                matches!(r, DelayPress::Held | DelayPress::Queued),
                "ホストの 9 が推奨値の予定にならない: {r:?}"
            );
            assert_eq!(p.ca().shown_delay(), 2, "9 そのものを D にした");
        } else {
            assert!(matches!(r, DelayPress::Asked { .. }), "{r:?}");
            assert_ne!(p.ca().shown_delay(), AUTO, "ゲストの画面に 9 が出る");
        }
        assert_eq!(p.ca().auto_on(), host, "旗を持つのはホストだけ");
        p.ca().press_delay(2);
        assert!(!p.ca().auto_on(), "数字の押下で旗が降りない");
    }

    /// ホスト側・ゲスト側のセッションと `Outlet` を取り出す（`(host, guest, host の Outlet, guest の Outlet, host の送り元のアドレス)`）。
    fn host_guest(p: &mut Pair) -> (&mut Link, &mut Link, &mut Q, &mut Q, SocketAddr) {
        if p.a.connected().unwrap().role() == Role::Host {
            (&mut p.a, &mut p.b, &mut p.qa, &mut p.qb, addr(PORT_A))
        } else {
            (&mut p.b, &mut p.a, &mut p.qb, &mut p.qa, addr(PORT_B))
        }
    }

    fn take_host_states(q: &mut Q) -> Vec<(Vec<u8>, HostState)> {
        std::mem::take(&mut q.0)
            .into_iter()
            .filter_map(|b| match dec(&b).map(|p| p.msg) {
                Ok(Message::HostState(h)) => Some((b, h)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_host_sends_host_state_on_change_and_every_second_and_the_guest_never() {
        let mut p = Pair::connect();
        let t0 = p.now + 1;
        let (host, guest, qh, qg, _) = host_guest(&mut p);
        let d0 = host.connected().unwrap().shown_delay();
        host.poll(t0, qh);
        guest.poll(t0, qg);
        let first = take_host_states(qh);
        assert_eq!(first.len(), 1, "繋がって最初の poll で 1 通");
        assert_eq!(
            first[0].1,
            HostState {
                seq: 0,
                mode: Mode::Delay,
                delay: d0,
                auto: false
            }
        );
        host.poll(t0 + 1, qh);
        assert!(take_host_states(qh).is_empty(), "変わっていないのに送った");
        host.connected_mut().unwrap().press_delay(5);
        host.poll(t0 + 2, qh);
        let v = take_host_states(qh);
        assert_eq!(v.len(), 1, "値が変わった tick に 1 通");
        assert_eq!(
            (v[0].1.seq, v[0].1.delay, v[0].1.auto),
            (1, 5, false),
            "押した値が目標として出る"
        );
        host.connected_mut()
            .unwrap()
            .press_delay(crate::delay_pick::AUTO);
        host.poll(t0 + 3, qh);
        let v = take_host_states(qh);
        assert_eq!(v.len(), 1, "旗が立った tick に 1 通");
        assert_eq!((v[0].1.seq, v[0].1.auto), (2, true));
        let last = v[0].1;
        // 1 秒: 999 ms 後は送らない、1000 ms 後は同じ値で送り直す（seq は進む）。
        host.poll(t0 + 3 + HOST_STATE_EVERY_MS - 1, qh);
        assert!(take_host_states(qh).is_empty(), "1 秒より早く送った");
        host.poll(t0 + 3 + HOST_STATE_EVERY_MS, qh);
        let v = take_host_states(qh);
        assert_eq!(v.len(), 1, "1 秒ごとの送り直し");
        assert_eq!(v[0].1, HostState { seq: 3, ..last });
        host.connected_mut().unwrap().press_delay(2);
        host.poll(t0 + 3 + HOST_STATE_EVERY_MS + 1, qh);
        let v = take_host_states(qh);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].1.seq, v[0].1.delay, v[0].1.auto), (4, 2, false));
        for k in 0..3u32 {
            guest.poll(t0 + 2 + k * 600, qg);
        }
        assert!(take_host_states(qg).is_empty(), "ゲストが送った");
    }

    #[test]
    fn the_guest_follows_the_hosts_flag_and_ignores_stale_seqs() {
        use crate::delay_pick::AUTO;
        let mut p = Pair::connect();
        let t0 = p.now + 1;
        let (host, guest, qh, qg, from) = host_guest(&mut p);
        assert_eq!(guest.connected().unwrap().host_state(), None);
        assert!(!guest.connected().unwrap().shown_auto());
        host.poll(t0, qh);
        host.connected_mut().unwrap().press_delay(5);
        host.poll(t0 + 1, qh);
        host.connected_mut().unwrap().press_delay(AUTO);
        host.poll(t0 + 2, qh);
        let pk = take_host_states(qh);
        assert_eq!(pk.len(), 3);
        let host_delay_auto = pk[2].1.delay;
        let machine = guest.connected().unwrap().shown_delay();
        assert_ne!(
            pk[1].1.delay, machine,
            "検査の前提: ホストの押した D はゲストの機械の値と違う"
        );
        let deliver = |g: &mut Link, q: &mut Q, k: usize| {
            g.on_datagram(from, &pk[k].0, t0 + 10, q);
        };
        deliver(guest, qg, 0);
        assert_eq!(
            guest.connected().unwrap().host_state().map(|h| h.seq),
            Some(0)
        );
        assert!(!guest.connected().unwrap().shown_auto());
        deliver(guest, qg, 2);
        let c = guest.connected().unwrap();
        assert_eq!(
            c.host_state().map(|h| h.seq),
            Some(2),
            "落ちた便を飛ばして最新"
        );
        assert!(c.shown_auto(), "ホストの旗がゲストの画面に出る");
        assert_eq!(
            c.shown_delay_for(false),
            host_delay_auto,
            "選択の間はホストの値"
        );
        assert_eq!(
            c.shown_delay_for(true),
            machine,
            "試合の間は INPUT の予定の値"
        );
        let rx = c.counts().host_state_rx;
        deliver(guest, qg, 1);
        deliver(guest, qg, 2);
        let c = guest.connected().unwrap();
        assert_eq!(c.host_state().map(|h| (h.seq, h.auto)), Some((2, true)));
        assert_eq!(c.counts().host_state_stale, 2, "古い・重なりを数えた");
        assert_eq!(c.counts().host_state_rx, rx, "古い便を受け入れた数に入れた");
        let gc = guest.connected_mut().unwrap();
        gc.press_delay(3);
        assert!(!gc.shown_auto(), "数字の要求で旗が降りない");
        assert_eq!(gc.shown_delay_for(false), 3, "要求の値が出ない");
        gc.press_delay(AUTO);
        assert!(gc.shown_auto(), "auto の要求で旗が立たない");
        let before = host.connected().unwrap().counts().host_state_stale;
        let back = if from == addr(PORT_A) {
            addr(PORT_B)
        } else {
            addr(PORT_A)
        };
        // ホストへはホストの受ける向きの鍵で封じる（ホスト自身が送った便は、ホストの鍵では開かない）。
        let to_host = enc_to(host, SID, &Message::HostState(pk[2].1));
        host.on_datagram(back, &to_host, t0 + 11, qh);
        let hc = host.connected().unwrap();
        assert_eq!(hc.host_state(), None);
        assert_eq!(hc.counts().host_state_stale, before + 1);
    }

    #[test]
    fn host_state_seq_is_compared_with_wraparound() {
        let mut p = Pair::connect();
        let t = p.now + 1;
        let (_, guest, qh, _, from) = host_guest(&mut p);
        qh.0.clear();
        let to_guest = guest.dir_keys().expect("繋がった").swapped();
        let st = |seq: u16, delay: u8| {
            seal_with(
                to_guest,
                SID,
                &Message::HostState(HostState {
                    seq,
                    mode: Mode::Delay,
                    delay,
                    auto: false,
                }),
            )
        };
        let mut feed = |seq: u16, delay: u8| {
            guest.on_datagram(from, &st(seq, delay), t, &mut Q::default());
            guest
                .connected()
                .unwrap()
                .host_state()
                .map(|h| (h.seq, h.delay))
        };
        assert_eq!(feed(65_535, 1), Some((65_535, 1)), "最初の 1 通");
        assert_eq!(feed(0, 2), Some((0, 2)), "65535 の次の 0 は新しい");
        assert_eq!(feed(65_535, 3), Some((0, 2)), "回った後の古い便");
        assert_eq!(feed(0, 4), Some((0, 2)), "同じ seq");
        assert_eq!(
            feed(0x7FFF, 5),
            Some((0x7FFF, 5)),
            "半分より手前の先は新しい"
        );
        assert_eq!(
            feed(0xFFFF, 6),
            Some((0x7FFF, 5)),
            "半分以上先は古い扱い（0x8000 の差）"
        );
    }

    #[test]
    fn pong_responder_exists_only_after_connecting_and_answers_like_the_box() {
        let a = link(PORT_B, 1);
        assert!(a.pong_responder().is_none(), "繋がる前に口がある");
        let mut p = Pair::connect();
        let r = p.a.pong_responder().expect("繋がった後は口がある");
        assert_eq!((r.session_id(), r.path()), (SID, addr(PORT_B)));
        let ping = enc_to(&p.a, SID, &Message::Ping { send_time: 4242 });
        let mut out = [0u8; MAX_DATAGRAM];
        let (to, n) = r
            .answer(&Seq::new(), addr(PORT_B), &ping, &mut out)
            .expect("口が返さない");
        // セッションが返す PONG（印なし）と同じ中身・同じ session（封じ方は送り手の印だけ違う —— セッションは `Sender::Link`、応答側は `Sender::Pong`）。
        let now = p.now;
        p.a.on_datagram(addr(PORT_B), &ping, now, &mut p.qa);
        let from_box = p.qa.0.clone();
        assert_eq!(from_box.len(), 1);
        assert_eq!(dec(&from_box[0]), dec(&out[..n]));
        assert_eq!(
            dec(&out[..n]).map(|k| k.msg),
            Ok(Message::Pong { send_time: 4242 })
        );
        let h = wire::HEADER_LEN;
        assert_eq!(
            (from_box[0][h], out[h]),
            (
                crate::seal::Sender::Link as u8,
                crate::seal::Sender::Pong as u8
            )
        );
        assert_eq!(to, addr(PORT_B));
    }

    fn sends_select(p: &mut Pair, t: u32) -> bool {
        p.a.poll(t, &mut p.qa);
        p.qa.take().iter().any(|m| matches!(m, Message::Select(_)))
    }

    #[test]
    fn only_an_input_of_the_current_seg_stops_select_resend() {
        let mut p = Pair::connect();
        let now = p.now;
        // A は前のマッチを終えて seg 1 にいる。B はまだ seg 0 の INPUT を送ってくる（遅れて届いた前のマッチの分）。
        p.ca().begin_seg(1);
        p.ca().enter_selection();
        p.ca()
            .selection_frame(0, None, true, &mut Q::default(), |_| {
                SelExit::Decided(select())
            });
        assert!(sends_select(&mut p, now), "SELECT を置いたのに送らない");

        let b = p.b.connected_mut().unwrap();
        b.capture(0x11, now, &mut p.qb).unwrap();
        let (ra, _) = p.exchange();
        assert_eq!(ra, vec![Recv::Nothing], "seg 0 の INPUT を受け入れた");
        assert_eq!(p.ca().counts().input_prev_seg, 1);
        assert!(
            sends_select(&mut p, now + lockstep::RESEND_MS),
            "前の seg の INPUT で SELECT の送り直しを止めた"
        );

        let b = p.b.connected_mut().unwrap();
        b.begin_seg(1);
        b.capture(0x22, now, &mut p.qb).unwrap();
        let (ra, _) = p.exchange();
        assert!(
            matches!(ra.as_slice(), [Recv::Input { seg: 1, .. }]),
            "{ra:?}"
        );
        assert_eq!(p.ca().counts().input_accepted, 1);
        assert!(
            !sends_select(&mut p, now + 10 * lockstep::RESEND_MS),
            "今の seg の INPUT を受けても SELECT を送り直している"
        );
    }

    #[test]
    fn prev_seg_input_is_answered_only_while_the_peer_still_waits() {
        let mut p = Pair::connect();
        let now = p.now;
        p.ca().capture(0x11, now, &mut Q::default()).unwrap();
        let a = p.ca().advance(now, &mut Q::default());
        assert!(a.is_some(), "穴埋めのフレーム 0 を使えない");
        p.ca().begin_seg(1);

        let r = p.a.on_datagram(
            addr(PORT_B),
            &enc_to(&p.a, SID, &input(0, 0)),
            now,
            &mut p.qa,
        );
        assert_eq!(r, Recv::Nothing);
        let sent = p.qa.take();
        assert!(
            matches!(sent.as_slice(), [Message::Input(i)] if i.seg == 0),
            "バックアップを返していない: {sent:?}"
        );
        // 相手は使い終えている（ack 1）⇒ 返さない（返し合いが止まらなくなる）。
        p.a.on_datagram(
            addr(PORT_B),
            &enc_to(&p.a, SID, &input(0, 1)),
            now,
            &mut p.qa,
        );
        assert!(
            p.qa.take().is_empty(),
            "使い終えた相手にバックアップを返した"
        );
        let c = p.ca().counts();
        assert_eq!((c.input_prev_seg, c.input_prev_replies), (2, 1));
        p.a.on_datagram(
            addr(PORT_B),
            &enc_to(&p.a, SID, &input(5, 0)),
            now,
            &mut p.qa,
        );
        assert_eq!(p.ca().counts().input_other_seg, 1);
        assert!(p.qa.take().is_empty());
    }

    #[test]
    fn next_seg_input_is_reported_and_not_used() {
        let mut p = Pair::connect();
        let now = p.now;
        let before = p.ca().lockstep().remote_next();
        let b = p.b.connected_mut().unwrap();
        b.begin_seg(1);
        b.capture(0x33, now, &mut p.qb).unwrap();
        let (ra, _) = p.exchange();
        assert_eq!(ra, vec![Recv::NextSeg { seg: 1 }]);
        assert_eq!(p.ca().counts().input_other_seg, 1);
        assert_eq!(p.ca().counts().input_accepted, 0);
        assert_eq!(
            p.ca().lockstep().remote_next(),
            before,
            "次の seg の語を今の seg に入れた"
        );
        // 2 つ先の seg は合図にしない（相手が 1 マッチを丸ごと飛ばすことは無い）。
        let r = p.a.on_datagram(
            addr(PORT_B),
            &enc_to(&p.a, SID, &input(2, 0)),
            now,
            &mut p.qa,
        );
        assert_eq!(r, Recv::Nothing);
        p.ca().begin_seg(1);
        let again = Message::Input(p.b.connected().unwrap().lockstep().build_input());
        let r =
            p.a.on_datagram(addr(PORT_B), &enc_to(&p.a, SID, &again), now, &mut p.qa);
        assert!(matches!(r, Recv::Input { seg: 1, .. }), "{r:?}");
        let r = p.a.on_datagram(
            addr(PORT_B),
            &enc_to(&p.a, SID, &input(0, 0)),
            now,
            &mut p.qa,
        );
        assert_eq!(r, Recv::Nothing);
    }

    fn menu(to: &Link, step: u16) -> Vec<u8> {
        enc_to(
            to,
            SID,
            &Message::Menu(wire::Menu {
                step,
                ack: 0,
                base: 0,
                count: 0,
                words: [0; MAX_WORDS],
            }),
        )
    }

    #[test]
    fn menu_outside_selection_is_reported_only_at_the_current_step() {
        let mut p = Pair::connect();
        let now = p.now;
        let hb = p.b.connected_mut().unwrap();
        assert_eq!(hb.role(), Role::Host);
        hb.enter_selection();
        let r =
            p.b.on_datagram(addr(PORT_A), &menu(&p.b, 0), now, &mut p.qb);
        assert_eq!(r, Recv::Nothing, "選択の中の MENU で知らせた");
        let hb = p.b.connected_mut().unwrap();
        let fo = hb.selection_frame(0, Some(decide()), true, &mut Q::default(), |fo| {
            if fo.apply.is_some() {
                SelExit::Decided(select())
            } else {
                SelExit::Stay
            }
        });
        assert!(fo.apply.is_some(), "決定が入らない（検査の前提が崩れた）");
        assert_eq!(hb.selection().step(), 1);
        let r =
            p.b.on_datagram(addr(PORT_A), &menu(&p.b, 0), now, &mut p.qb);
        assert_eq!(r, Recv::Nothing, "前の選択の MENU で知らせた");
        let r =
            p.b.on_datagram(addr(PORT_A), &menu(&p.b, 1), now, &mut p.qb);
        assert_eq!(r, Recv::PeerSelecting);
        p.b.connected_mut().unwrap().enter_selection();
        let r =
            p.b.on_datagram(addr(PORT_A), &menu(&p.b, 1), now, &mut p.qb);
        assert_eq!(r, Recv::Nothing);
    }

    #[test]
    fn check_matches_both_ways_and_a_remote_mismatch_is_reported() {
        let mut p = Pair::connect();
        let now = p.now;
        let to_a = p.a.dir_keys().expect("繋がった").swapped();
        let remote = |c: Check| seal_with(to_a, SID, &Message::Check(c));
        let r =
            p.a.on_datagram(addr(PORT_B), &remote(check(0, 9)), now, &mut p.qa);
        assert_eq!(r, Recv::Nothing);
        let mut q = Q::default();
        let r = p.ca().submit_check(check(0, 9), &mut q);
        assert!(matches!(r, Some(CheckResult::Match { frame: 0 })), "{r:?}");
        assert_eq!(
            q.take(),
            vec![Message::Check(check(0, 9))],
            "記録した値を送っていない"
        );
        assert_eq!(p.ca().submit_check(check(30, 1), &mut q), None);
        let r =
            p.a.on_datagram(addr(PORT_B), &remote(check(30, 2)), now, &mut p.qa);
        let want = Recv::Desync {
            local: check(30, 1),
            remote: check(30, 2),
        };
        assert_eq!(r, want);
        assert_eq!(p.ca().submit_check(check(60, 4), &mut q), None);
        let r =
            p.a.on_datagram(addr(PORT_B), &remote(check(60, 4)), now, &mut p.qa);
        assert_eq!(r, Recv::Nothing);
        assert_eq!(p.ca().counts().check_matches, 2);
    }

    #[test]
    fn waiting_resends_the_input_every_resend_ms() {
        let mut p = Pair::connect();
        let t = p.now;
        let mut q = Q::default();
        for f in 0..2u16 {
            p.ca().capture(0x10 + f, t, &mut q).unwrap();
            assert!(p.ca().advance(t, &mut q).is_some(), "穴埋めのフレーム {f}");
        }
        q.0.clear();
        p.ca().capture(0x12, t, &mut q).unwrap();
        let sent = q.take();
        assert!(
            matches!(sent.as_slice(), [Message::Input(_)]),
            "取ってすぐ送っていない: {sent:?}"
        );
        let again = p.ca().capture(0x13, t, &mut q);
        assert_eq!(again, Err(CaptureError::AlreadyCaptured));
        assert!(q.0.is_empty(), "取り直しで送った");

        let rs = lockstep::RESEND_MS;
        assert!(p.ca().advance(t + rs - 1, &mut q).is_none());
        assert!(q.0.is_empty(), "4 ms より前に送り直した");
        assert!(p.ca().advance(t + rs, &mut q).is_none());
        let sent = q.take();
        assert!(
            matches!(sent.as_slice(), [Message::Input(_)]),
            "4 ms で送り直していない: {sent:?}"
        );
        assert!(p.ca().advance(t + rs + 1, &mut q).is_none());
        assert!(q.0.is_empty(), "送り直しの後、すぐにまた送った");
    }

    #[test]
    fn leaving_selection_sends_no_menu_in_that_frame() {
        let mut p = Pair::connect();
        let mut q = Q::default();
        p.ca().enter_selection();
        p.ca()
            .selection_frame(0x40, None, true, &mut q, |_| SelExit::Stay);
        let sent = q.take();
        assert!(
            matches!(sent.as_slice(), [Message::Menu(_)]),
            "選択の中で MENU を送らない: {sent:?}"
        );

        p.ca()
            .selection_frame(0x40, None, true, &mut q, |_| SelExit::Decided(select()));
        assert!(
            q.0.is_empty(),
            "選択を出るフレームに MENU を送った: {:?}",
            q.take()
        );
        let now = p.now;
        p.a.poll(now, &mut q);
        let sent = q.take();
        assert!(
            sent.iter()
                .any(|m| matches!(m, Message::Select(s) if s.chara == 3)),
            "決めた SELECT を送らない: {sent:?}"
        );

        let mut p = Pair::connect();
        p.ca().enter_selection();
        p.ca()
            .selection_frame(0, None, true, &mut q, |_| SelExit::Leave);
        assert!(q.0.is_empty(), "選択を出るフレームに MENU を送った");
        let now = p.now;
        p.a.poll(now, &mut q);
        let sent = q.take();
        let menu_or_select = sent
            .iter()
            .any(|m| matches!(m, Message::Menu(_) | Message::Select(_)));
        assert!(!menu_or_select, "{sent:?}");
    }

    // ================= fault matrix（組み上がりは peer_sim の loopback）=================

    fn drain(q: &mut Q) -> Vec<(SocketAddr, Vec<u8>)> {
        let b = std::mem::take(&mut q.0);
        let t = std::mem::take(&mut q.1);
        t.into_iter().zip(b).collect()
    }

    fn hello(nonce: u64) -> Message {
        let mut name = [0u8; wire::NAME_MAX];
        name[0] = b'x';
        Message::Hello(wire::Hello {
            proto_ver: wire::PROTO_VER,
            latency: 2,
            name_len: 1,
            mode: Mode::Delay,
            can_rollback: false,
            listable: false,
            role_nonce: nonce,
            name,
            seat_wish: wire::SeatWish::P1,
            arrival: wire::Arrival::Unknown,
            build: crate::update::BUILD_NUMBER,
            public: public_of(nonce),
        })
    }

    /// 封じる便を、決まった鍵 `keys` の送る向きで封じる（セッションの鍵と関係の無い便）。
    fn seal_with(keys: DirKeys, sid: u32, msg: &Message) -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = wire::seal(
            &keys.sealer(crate::seal::Sender::Link),
            &Seq::new(),
            sid,
            msg,
            &mut buf,
        )
        .expect("封じられる");
        buf[..n].to_vec()
    }

    fn poisons(atk: SocketAddr, stranger: SocketAddr) -> Vec<(SocketAddr, Vec<u8>, &'static str)> {
        let good = enc(SID, &hello(7));
        let mut flipped = good.clone();
        flipped[wire::HEADER_LEN + 4] ^= 0x10; // role_nonce の 1 ビット
        let truncated = good[..good.len() - 1].to_vec();
        let other_key = enc_with(&Secret([0x3D; 16]), SID, SID, &hello(7));
        let old_session = enc(SID ^ 0x0100, &hello(7));
        let stun = crate::stun::encode_binding_request([1; 12]).to_vec();
        let stranger_ping = seal_with(
            DirKeys::from_raw([0x3E; 32], [0x3E; 32]),
            SID,
            &Message::Ping { send_time: 3 },
        );
        vec![
            (atk, flipped, "bad_tag"),
            (atk, truncated, "bad_tag"),
            (atk, other_key, "bad_tag"),
            (atk, old_session, "wrong_session"),
            (stranger, stranger_ping, "foreign"),
            (atk, stun, "stun"),
        ]
    }

    #[test]
    fn fault_matrix_drops_and_counts_each_poison_then_connects() {
        for (a_nonce, b_nonce) in [(1u64, 2u64), (9, 2)] {
            let me = addr(PORT_A);
            let atk = addr(40_009);
            let stranger = addr(40_010);
            let cands = Candidates::from_addrs(&[atk, me, addr(PORT_B)]).unwrap();
            let mut a = Link::new(
                Config {
                    handshake: hs_cfg(a_nonce),
                    secret: SECRET,
                    peers: cands,
                    key_seed: seed_of(a_nonce),
                },
                0,
            );
            let mut b = link(PORT_A, b_nonce);
            let (mut qa, mut qb) = (Q::default(), Q::default());
            let mut to_atk = 0u32;
            let mut to_others_after = 0u32;
            let mut injected = 0u32;
            for t in 1..600u32 {
                if t == 2 || t == 500 {
                    for (from, bytes, _) in poisons(atk, stranger) {
                        let r = a.on_datagram(from, &bytes, t, &mut qa);
                        assert_eq!(r, Recv::Nothing);
                        injected += 1;
                    }
                }
                a.poll(t, &mut qa);
                b.poll(t, &mut qb);
                let connected = a.connected().is_some() && b.connected().is_some();
                for _ in 0..4 {
                    for (to, bytes) in drain(&mut qa) {
                        if to == addr(PORT_B) {
                            b.on_datagram(me, &bytes, t, &mut qb);
                        } else {
                            // 相手でない候補へ送ってよいのは、経路を決める前の HELLO だけ（AGREE が自分自身へ返ると、
                            // ホストが自分の AGREE を「ゲストの返し」と取り違えてひとりで繋がる）。
                            let m = dec(&bytes).expect("A が送ったものは読める").msg;
                            assert!(
                                matches!(m, Message::Hello(_)),
                                "相手でない候補 {to} へ HELLO 以外を送った: {m:?}"
                            );
                            if to == me {
                                a.on_datagram(me, &bytes, t, &mut qa);
                            } else {
                                assert_eq!(to, atk, "候補の外へ送った");
                                to_atk += 1;
                            }
                            if connected {
                                to_others_after += 1;
                            }
                        }
                    }
                    for (to, bytes) in drain(&mut qb) {
                        assert_eq!(to, me);
                        a.on_datagram(addr(PORT_B), &bytes, t, &mut qa);
                    }
                }
            }
            let tag = format!("A の nonce {a_nonce}");
            assert!(
                a.connected().is_some() && b.connected().is_some(),
                "{tag}: 繋がらない（A {:?} / B {:?}）",
                a.handshake_state(),
                b.handshake_state()
            );
            assert_eq!(
                a.path(),
                Some(addr(PORT_B)),
                "{tag}: 相手でない候補を経路に採った"
            );
            let ca = a.connected().unwrap();
            let cb = b.connected().unwrap();
            assert_ne!(ca.role(), cb.role(), "{tag}");
            assert_eq!(ca.role() == Role::Host, a_nonce > b_nonce, "{tag}");
            assert!(
                to_atk > 0,
                "{tag}: 経路を決める前に全部の候補へ HELLO を送っていない"
            );
            assert_eq!(
                to_others_after, 0,
                "{tag}: 経路を決めた後も相手以外へ送った"
            );
            let c = a.recv_counts();
            assert_eq!(injected, 12);
            assert_eq!(
                (c.bad_tag, c.wrong_session, c.foreign, c.stun),
                (6, 2, 2, 2),
                "{tag}: {c:?}"
            );
            assert!(
                c.self_bounce >= 1,
                "{tag}: 自分の HELLO の跳ね返りを数えていない: {c:?}"
            );
            assert_eq!(c.decode_errors, 0, "{tag}");
            let h = a.handshake_counters();
            assert_eq!((h.wrong_session, h.stale_nonce), (0, 0), "{tag}: {h:?}");
            assert!(
                ca.liveness().rtt_ms().is_some(),
                "{tag}: RTT が取れていない"
            );
        }
    }

    // ---- バージョン違い ----

    fn hello_v0_signed() -> Vec<u8> {
        #[rustfmt::skip]
        let mut v = vec![
            0x48, 0x47, 0x52, 0x54, // magic "HGRT"
            0x00,                   // proto_ver = 0（バージョン 0）
            0x01,                   // type = 1 (HELLO)
            0x00, 0x00,
        ];
        v.extend_from_slice(&SID.to_le_bytes());
        v.extend_from_slice(&[0x00, 0x02, 0x01, 0x00]); // 本文のバージョン 0・latency 2・name_len 1・mode 0
        v.extend_from_slice(&7u64.to_le_bytes()); // role_nonce
        let mut name = [0u8; wire::NAME_MAX];
        name[0] = b'x';
        v.extend_from_slice(&name);
        let tag = AuthKey::derive(&SECRET, SID).tag(&v);
        v.extend_from_slice(&tag);
        v
    }

    /// 書いた物のヘッダのバージョン（と HELLO なら本文のバージョン）を `ver` に書き換え、タグを付け直す。同じ秘密を知る、別のバージョンの相手が送った形。
    fn relabel(bytes: &[u8], ver: u8) -> Vec<u8> {
        let mut v = bytes[..bytes.len() - wire::TAG_LEN].to_vec();
        v[4] = ver;
        if v[5] == 1 {
            v[wire::HEADER_LEN] = ver;
        }
        let tag = AuthKey::derive(&SECRET, SID).tag(&v);
        v.extend_from_slice(&tag);
        v
    }

    #[test]
    fn a_hello_of_another_version_stops_the_handshake_at_once() {
        let mut a = link(PORT_B, 1);
        a.set_retry_window(60_000);
        let mut q = Q::default();
        a.poll(1, &mut q);
        assert!(!q.take().is_empty(), "効く期間: やめる前は HELLO を送る");
        let r = a.on_datagram(addr(PORT_B), &hello_v0_signed(), 2, &mut q);
        assert_eq!(r, Recv::Nothing);
        let gone = handshake::State::Failed(FailReason::OtherVersion { theirs: 0 });
        assert_eq!(a.handshake_state(), gone);
        let c = a.recv_counts();
        assert_eq!((c.other_version, c.decode_errors), (1, 0), "{c:?}");
        for t in 3..3 * handshake::TIMEOUT_MS {
            a.poll(t, &mut q);
        }
        assert!(q.0.is_empty(), "やめた後も送った: {} 通", q.0.len());
        assert_eq!(a.handshake_state(), gone);
        assert_eq!(a.restarts().count, 0);
        let mut now_ver = hello_v0_signed();
        now_ver.truncate(now_ver.len() - wire::TAG_LEN);
        now_ver.extend_from_slice(&[0, 0]); // seat_wish = おまかせ・arrival = 名乗らない
        now_ver.extend_from_slice(&crate::update::BUILD_NUMBER.to_le_bytes());
        now_ver.extend_from_slice(&public_of(7));
        now_ver.extend_from_slice(&[0; wire::TAG_LEN]); // `relabel` が落として付け直す
        let mut b = link(PORT_B, 1);
        b.on_datagram(addr(PORT_B), &relabel(&now_ver, wire::PROTO_VER), 2, &mut q);
        assert_eq!(b.handshake_state(), handshake::State::Connecting);
        assert_eq!(b.hs.role(), Some(crate::lockstep::Role::Guest));
        assert_eq!(b.recv_counts().other_version, 0);
    }

    #[test]
    fn a_forged_other_version_hello_cannot_stop_the_handshake() {
        let mut forged = hello_v0_signed();
        let n = forged.len();
        forged[n - 1] ^= 0x01;
        let mut a = link(PORT_B, 1);
        let mut q = Q::default();
        assert_eq!(
            a.on_datagram(addr(PORT_B), &forged, 1, &mut q),
            Recv::Nothing
        );
        let c = a.recv_counts();
        assert_eq!((c.bad_tag, c.other_version), (1, 0), "{c:?}");
        assert_eq!(a.handshake_state(), handshake::State::Connecting);
        let p = Pair::connect_links(a, link(PORT_A, 2));
        assert!(p.a.connected().is_some() && p.b.connected().is_some());
    }

    #[test]
    fn both_sides_of_a_version_mismatch_stop_with_the_peers_version() {
        const NEXT: u8 = wire::PROTO_VER + 1;
        let (mut a, mut b) = (link(PORT_B, 1), link(PORT_A, 2));
        let (mut qa, mut qb) = (Q::default(), Q::default());
        for t in 1..200u32 {
            a.poll(t, &mut qa);
            b.poll(t, &mut qb);
            for bytes in std::mem::take(&mut qa.0) {
                b.on_datagram(addr(PORT_A), &relabel(&bytes, NEXT), t, &mut qb);
            }
            for bytes in std::mem::take(&mut qb.0) {
                a.on_datagram(addr(PORT_B), &relabel(&bytes, 0), t, &mut qa);
            }
        }
        assert_eq!(
            b.handshake_state(),
            handshake::State::Failed(FailReason::OtherVersion { theirs: NEXT })
        );
        assert_eq!(
            a.handshake_state(),
            handshake::State::Failed(FailReason::OtherVersion { theirs: 0 })
        );
        assert!(a.connected().is_none() && b.connected().is_none());
    }

    #[test]
    fn another_version_after_connecting_is_only_counted() {
        let mut p = Pair::connect();
        let before = p.a.handshake_state();
        let r =
            p.a.on_datagram(addr(PORT_B), &hello_v0_signed(), p.now, &mut p.qa);
        assert_eq!(r, Recv::Nothing);
        assert_eq!(p.a.handshake_state(), before);
        assert!(p.a.connected().is_some());
        assert_eq!(p.a.recv_counts().other_version, 1);
    }

    // ---- 時間切れの後の作り直し ----

    struct Node {
        at: SocketAddr,
        peer: SocketAddr,
        nonce: u64,
        /// 使い捨ての鍵の種（既定は `seed_of(nonce)`。同じ nonce で起こし直した形は別の種にする）。
        seed: KeySeed,
        from: u32,
        until: u32,
        link: Option<Link>,
        q: Q,
        connected_at: Option<u32>,
    }

    fn node(at: u16, peer: u16, nonce: u64, from: u32) -> Node {
        Node {
            at: addr(at),
            peer: addr(peer),
            nonce,
            seed: seed_of(nonce),
            from,
            until: u32::MAX,
            link: None,
            q: Q::default(),
            connected_at: None,
        }
    }

    fn sim(
        nodes: &mut [Node],
        d: u32,
        window: u32,
        end: u32,
        lost: &dyn Fn(u32, &Message) -> bool,
    ) {
        let mut wire: Vec<(u32, SocketAddr, SocketAddr, Vec<u8>)> = Vec::new();
        for t in 0..end {
            for n in nodes.iter_mut() {
                if t == n.from {
                    let mut l =
                        Link::new(Config::single(hs_cfg(n.nonce), SECRET, n.peer, n.seed), t);
                    l.set_retry_window(window);
                    n.link = Some(l);
                }
                if t == n.until {
                    n.link = None;
                }
            }
            let (due, rest): (Vec<_>, Vec<_>) = wire.into_iter().partition(|p| p.0 <= t);
            wire = rest;
            for (_, from, to, bytes) in due {
                if let Some(n) = nodes.iter_mut().find(|n| n.at == to && n.link.is_some())
                    && let Some(l) = n.link.as_mut()
                {
                    l.on_datagram(from, &bytes, t, &mut n.q);
                }
            }
            for n in nodes.iter_mut() {
                let Some(l) = n.link.as_mut() else { continue };
                l.poll(t, &mut n.q);
                if n.connected_at.is_none() && l.connected().is_some() {
                    n.connected_at = Some(t);
                }
                for (to, bytes) in drain(&mut n.q) {
                    if !lost(t, &dec(&bytes).expect("送ったものは読める").msg) {
                        wire.push((t + d.max(1), n.at, to, bytes));
                    }
                }
            }
        }
    }

    const WINDOW: u32 = 30 * 60 * 1000;

    #[test]
    fn a_peer_waking_15_seconds_late_still_connects() {
        for (late, d) in [(1usize, 3u32), (0, 3), (1, 20), (0, 20)] {
            let mut starts = [0u32, 0];
            starts[late] = 15_000;
            let mut ns = [
                node(PORT_A, PORT_B, 9, starts[0]),
                node(PORT_B, PORT_A, 2, starts[1]),
            ];
            sim(&mut ns, d, WINDOW, 17_000, &|_, _| false);
            let tag = format!("遅れて起きる台 {late}・片道 {d} ms");
            for n in &ns {
                let at = n.connected_at.unwrap_or_else(|| {
                    panic!(
                        "{tag}: 繋がらない {:?}",
                        n.link.as_ref().map(|l| l.handshake_state())
                    )
                });
                assert!(
                    (15_000..15_000 + 3 * d + 30).contains(&at),
                    "{tag}: 繋がった時刻 {at}"
                );
            }
            let early = ns[1 - late].link.as_ref().unwrap().restarts();
            assert_eq!(
                early,
                Restarts {
                    count: 1,
                    last: Some((FailReason::NothingReceived, Restart::Fresh))
                },
                "{tag}"
            );
            assert_eq!(ns[late].link.as_ref().unwrap().restarts().count, 0, "{tag}");
        }
        let mut ns = [node(PORT_A, PORT_B, 9, 0), node(PORT_B, PORT_A, 2, 15_000)];
        sim(&mut ns, 3, 0, 17_000, &|_, _| false);
        assert!(ns.iter().all(|n| n.connected_at.is_none()));
        let a = ns[0].link.as_ref().unwrap();
        assert_eq!(
            a.handshake_state(),
            handshake::State::Failed(FailReason::NothingReceived)
        );
        assert_eq!(a.restarts().count, 0);
    }

    #[test]
    fn a_timeout_in_the_middle_of_the_handshake_resumes() {
        let mut resumed = 0;
        for (nonce_a, d) in [(9u64, 7u32), (1, 7), (9, 2), (1, 2)] {
            for late in 9_975..=10_000u32 {
                let mut ns = [
                    node(PORT_A, PORT_B, nonce_a, 0),
                    node(PORT_B, PORT_A, 5, late),
                ];
                sim(&mut ns, d, WINDOW, late + 1_500, &|_, _| false);
                let tag = format!("A の nonce {nonce_a}・片道 {d}・B が {late} ms に起きる");
                for n in &ns {
                    let at = n
                        .connected_at
                        .unwrap_or_else(|| panic!("{tag}: 繋がらない"));
                    assert!(at < late + 1_000, "{tag}: 繋がった時刻 {at}");
                }
                let r = ns[0].link.as_ref().unwrap().restarts();
                if r.last.is_some_and(|(_, k)| k == Restart::Resume) {
                    resumed += 1;
                }
            }
        }
        eprintln!(
            "10 秒の間際に起きる相手（4 組 × 26 時刻 = 104 走行）: 引き継ぐ形で作り直した回 {resumed}"
        );
        assert!(resumed > 0, "引き継ぐ形の作り直しを 1 度も通っていない");
    }

    #[test]
    fn a_peer_restarted_with_a_new_nonce_is_picked_up() {
        let mut b1 = node(PORT_B, PORT_A, 2, 0);
        b1.until = 3_000;
        let mut ns = [
            node(PORT_A, PORT_B, 9, 0),
            b1,
            node(PORT_B, PORT_A, 3, 5_000),
        ];
        sim(&mut ns, 3, WINDOW, 12_000, &|t, m| {
            t < 3_000 && matches!(m, Message::Agree { .. })
        });
        assert_eq!(ns[1].connected_at, None);
        let at = ns[0].connected_at.expect("A が B2 と繋がらない");
        assert!((10_000..10_100).contains(&at), "繋がった時刻 {at}");
        assert_eq!(ns[2].connected_at.map(|t| t >= 10_000), Some(true));
        let r = ns[0].link.as_ref().unwrap().restarts();
        assert_eq!(
            r,
            Restarts {
                count: 1,
                last: Some((FailReason::NoHandshake, Restart::Fresh))
            }
        );
        assert!(
            ns[0]
                .link
                .as_ref()
                .unwrap()
                .handshake_counters()
                .stale_nonce
                > 0
        );
    }

    #[test]
    fn restarts_stop_at_the_end_of_the_window() {
        let mut a = link(PORT_B, 9);
        a.set_retry_window(35_000);
        let mut q = Q::default();
        let mut sent_after = 0;
        for t in 0..60_000u32 {
            a.poll(t, &mut q);
            let n = drain(&mut q).len();
            if t >= 40_000 {
                sent_after += n;
            }
            if t == 39_999 {
                assert_eq!(a.handshake_state(), handshake::State::Connecting);
            }
        }
        assert_eq!(a.restarts().count, 3);
        assert_eq!(
            a.handshake_state(),
            handshake::State::Failed(FailReason::NothingReceived)
        );
        assert_eq!(sent_after, 0, "やめた後に送った");
    }

    #[test]
    fn a_hello_arriving_at_the_timeout_is_taken_by_the_new_attempt() {
        let mut a = link(PORT_B, 9);
        a.set_retry_window(WINDOW);
        let mut q = Q::default();
        a.poll(0, &mut q);
        let _ = drain(&mut q);
        let r = a.on_datagram(
            addr(PORT_B),
            &enc(SID, &hello(2)),
            handshake::TIMEOUT_MS,
            &mut q,
        );
        assert_eq!(r, Recv::Nothing);
        assert_eq!(a.handshake_counters().after_fail, 0, "やめた後として捨てた");
        assert_eq!(a.restarts().count, 1);
        let sent: Vec<Message> = q.take();
        assert!(
            sent.iter().any(|m| matches!(m, Message::HelloAck { .. })),
            "{sent:?}"
        );
        assert!(
            sent.iter().any(|m| matches!(m, Message::Agree { .. })),
            "{sent:?}"
        );
    }

    #[test]
    fn a_burst_of_queued_hellos_gets_one_reply() {
        let mut b = link(PORT_A, 2);
        let mut q = Q::default();
        for _ in 0..64 {
            b.on_datagram(addr(PORT_A), &enc(SID, &hello(9)), 5, &mut q);
        }
        let sent = q.take();
        let n = |f: fn(&Message) -> bool| sent.iter().filter(|m| f(m)).count();
        assert_eq!(n(|m| matches!(m, Message::HelloAck { .. })), 1, "{sent:?}");
        assert_eq!(n(|m| matches!(m, Message::Hello(_))), 1, "{sent:?}");
        assert_eq!(b.recv_counts().reply_folded, 63);
        b.on_datagram(addr(PORT_A), &enc(SID, &hello(9)), 6, &mut q);
        assert!(
            q.take()
                .iter()
                .any(|m| matches!(m, Message::HelloAck { .. })),
            "ms が変わっても返さない"
        );
        let two = Candidates::from_addrs(&[addr(PORT_A), addr(40_003)]).unwrap();
        let mut c = Link::new(
            Config {
                handshake: hs_cfg(2),
                secret: SECRET,
                peers: two,
                key_seed: seed_of(2),
            },
            0,
        );
        c.on_datagram(addr(PORT_A), &enc(SID, &hello(9)), 7, &mut q);
        c.on_datagram(addr(40_003), &enc(SID, &hello(9)), 7, &mut q);
        let acks: Vec<SocketAddr> = drain(&mut q)
            .into_iter()
            .filter(|(_, b)| matches!(dec(b).unwrap().msg, Message::HelloAck { .. }))
            .map(|(to, _)| to)
            .collect();
        assert_eq!(acks, vec![addr(PORT_A), addr(40_003)]);
    }

    #[test]
    fn check_rule_reaches_the_connected_log() {
        for rule in [CheckRule::All, CheckRule::SkipCountAtFrame0] {
            let mut ns = [node(PORT_A, PORT_B, 9, 0), node(PORT_B, PORT_A, 2, 0)];
            // A は `sim` に作らせず、作った直後（繋がる前）に比べ方を置いてから渡す。
            let mut a = Link::new(
                Config::single(hs_cfg(9), SECRET, addr(PORT_B), seed_of(9)),
                0,
            );
            a.set_check_rule(rule);
            ns[0].link = Some(a);
            ns[0].from = u32::MAX;
            sim(&mut ns, 2, 0, 200, &|_, _| false);
            let c = ns[0].link.as_ref().unwrap().connected().expect("繋がる");
            assert_eq!(c.check_log().rule(), rule);
        }
    }

    #[test]
    fn self_bounce_never_reaches_the_handshake() {
        let me = addr(PORT_A);
        let mut a = Link::new(Config::single(hs_cfg(5), SECRET, me, seed_of(5)), 0);
        let mut qa = Q::default();
        for t in 0..200u32 {
            a.poll(t, &mut qa);
            for (to, bytes) in drain(&mut qa) {
                assert_eq!(to, me);
                a.on_datagram(me, &bytes, t, &mut qa);
            }
        }
        assert!(a.recv_counts().self_bounce >= 5, "{:?}", a.recv_counts());
        assert_eq!(
            a.handshake_state(),
            handshake::State::Connecting,
            "跳ね返りが手順へ届いた（同じ nonce でやめた）"
        );
        a.poll(handshake::TIMEOUT_MS, &mut qa);
        assert_eq!(
            a.handshake_state(),
            handshake::State::Failed(handshake::FailReason::NothingReceived)
        );
    }

    // ================= 対戦の成立（READY）=================

    fn gated_pair() -> Pair {
        let (mut a, mut b) = (link(PORT_B, 1), link(PORT_A, 2));
        a.set_match_gate(true);
        b.set_match_gate(true);
        Pair::connect_links(a, b)
    }

    fn run_ms(p: &mut Pair, ms: u32) {
        for _ in 0..ms {
            p.now += 1;
            p.a.poll(p.now, &mut p.qa);
            p.b.poll(p.now, &mut p.qb);
            p.exchange();
        }
    }

    fn count(msgs: &[Message], pred: impl Fn(&Message) -> bool) -> usize {
        msgs.iter().filter(|m| pred(m)).count()
    }

    #[test]
    fn the_gate_matches_only_after_both_send_ready() {
        let mut p = gated_pair();
        assert!(!p.ca().matched() && !p.b.connected().unwrap().matched());
        let (mut sent_a, mut sent_b) = (Vec::new(), Vec::new());
        for _ in 0..3_000 {
            p.now += 1;
            p.a.poll(p.now, &mut p.qa);
            p.b.poll(p.now, &mut p.qb);
            sent_a.extend(p.qa.0.iter().map(|b| dec(b).unwrap().msg));
            sent_b.extend(p.qb.0.iter().map(|b| dec(b).unwrap().msg));
            p.exchange();
        }
        assert!(count(&sent_a, |m| matches!(m, Message::Ping { .. })) > 0);
        assert_eq!(count(&sent_a, |m| *m == Message::Ready), 0);
        assert_eq!(count(&sent_b, |m| *m == Message::Ready), 0);
        let now = p.now;
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        assert_eq!(p.qa.take(), vec![Message::Ready], "選んだ瞬間に READY");
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        assert!(p.qa.0.is_empty(), "2 回目の選ぶは送らない");
        run_ms(&mut p, READY_RESEND_MS);
        let gb = *p.b.connected().unwrap().match_gate().unwrap();
        assert!(gb.peer() && !gb.local());
        assert!(!p.b.connected().unwrap().matched(), "B はまだ選んでいない");
        assert!(!p.ca().matched(), "A は B の READY を受けていない");
        let now = p.now;
        let (b, qb) = (&mut p.b, &mut p.qb);
        b.connected_mut().unwrap().set_ready(now, qb);
        p.exchange();
        assert!(p.ca().matched() && p.b.connected().unwrap().matched());
        run_ms(&mut p, 3 * READY_RESEND_MS);
        let ga = *p.ca().match_gate().unwrap();
        assert!(ga.matched());
        assert!(ga.counts().ready_dup >= 2, "{:?}", ga.counts());
    }

    #[test]
    fn ready_is_resent_until_the_peer_sends_post_match_traffic() {
        let mut p = gated_pair();
        let now = p.now;
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        let (b, qb) = (&mut p.b, &mut p.qb);
        b.connected_mut().unwrap().set_ready(now, qb);
        p.exchange();
        assert!(p.ca().matched());
        let ready_from_a = |p: &mut Pair, ms: u32| {
            let mut n = 0;
            for _ in 0..ms {
                p.now += 1;
                p.a.poll(p.now, &mut p.qa);
                n += count(&p.qa.take(), |m| *m == Message::Ready);
            }
            n
        };
        assert!(
            ready_from_a(&mut p, 3 * READY_RESEND_MS) >= 2,
            "成立後も送り直す"
        );
        let (b, qb) = (&mut p.b, &mut p.qb);
        let cb = b.connected_mut().unwrap();
        cb.enter_selection();
        cb.selection_frame(0, None, true, qb, |_| SelExit::Stay);
        assert!(
            p.qb.0
                .iter()
                .any(|b| matches!(dec(b).unwrap().msg, Message::Menu(_)))
        );
        p.exchange();
        assert_eq!(ready_from_a(&mut p, 5 * READY_RESEND_MS), 0);
    }

    /// B の `Outlet` に溜まった READY を最大 `n` 通捨てる（落ちた READY の模型）。捨てた数を返す。
    fn drop_ready_from_b(p: &mut Pair, n: &mut u32) -> u32 {
        let mut dropped = 0;
        p.qb.0.retain(|b| {
            let ready = matches!(dec(b).map(|k| k.msg), Ok(Message::Ready));
            if ready && *n > 0 {
                *n -= 1;
                dropped += 1;
                false
            } else {
                true
            }
        });
        dropped
    }

    #[test]
    fn a_gated_link_closes_a_gateless_peer_three_seconds_after_its_first_menu() {
        use crate::match_gate::{NO_GATE_MS, NoMatch};
        let (mut a, b) = (link(PORT_B, 1), link(PORT_A, 2));
        a.set_match_gate(true);
        let mut p = Pair::connect_links(a, b);
        assert!(
            p.b.connected().unwrap().match_gate().is_none(),
            "B はゲートなし"
        );
        let now = p.now;
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        let (b, qb) = (&mut p.b, &mut p.qb);
        let cb = b.connected_mut().unwrap();
        cb.enter_selection();
        cb.selection_frame(0, None, true, qb, |_| SelExit::Stay);
        let first_menu = p.now;
        p.exchange();
        let mut sent_a = Vec::new();
        let mut closed_at = None;
        for _ in 0..2 * NO_GATE_MS {
            p.now += 1;
            p.a.poll(p.now, &mut p.qa);
            p.b.poll(p.now, &mut p.qb);
            let (b, qb) = (&mut p.b, &mut p.qb);
            if let Some(cb) = b.connected_mut() {
                cb.selection_frame(0, None, true, qb, |_| SelExit::Stay);
            }
            sent_a.extend(p.qa.0.iter().map(|b| dec(b).unwrap().msg));
            p.exchange();
            if closed_at.is_none() && p.ca().match_gate().unwrap().closed().is_some() {
                closed_at = Some(p.now - first_menu);
            }
        }
        assert_eq!(
            closed_at,
            Some(NO_GATE_MS),
            "MENU の 1 通目から 3 秒ちょうどで閉じる"
        );
        let ga = *p.ca().match_gate().unwrap();
        assert_eq!(ga.closed(), Some(NoMatch::PeerHasNoGate));
        assert!(!ga.expired() && !ga.matched());
        assert!(sent_a.contains(&Message::Bye(ByeReason::Cancelled)));
        assert_eq!(
            p.b.connected().unwrap().liveness().peer_bye(),
            Some(ByeReason::Cancelled),
            "ゲートの無い B が BYE(取消) を受けない（難易度の画面で止まる）"
        );
    }

    #[test]
    fn a_new_peer_whose_first_ready_is_lost_is_not_closed() {
        for lost in [1u32, 2] {
            let mut p = gated_pair();
            let now = p.now;
            p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
            p.exchange();
            let mut to_drop = lost;
            let now = p.now;
            let (b, qb) = (&mut p.b, &mut p.qb);
            b.connected_mut().unwrap().set_ready(now, qb);
            assert_eq!(drop_ready_from_b(&mut p, &mut to_drop), 1);
            assert!(p.b.connected().unwrap().matched());
            let (b, qb) = (&mut p.b, &mut p.qb);
            let cb = b.connected_mut().unwrap();
            cb.enter_selection();
            cb.selection_frame(0, None, true, qb, |_| SelExit::Stay);
            p.exchange();
            assert!(
                !p.ca().matched(),
                "lost {lost}: A はまだ B の READY を受けていない"
            );
            let mut sent_a = Vec::new();
            for _ in 0..10 * READY_RESEND_MS {
                p.now += 1;
                p.a.poll(p.now, &mut p.qa);
                p.b.poll(p.now, &mut p.qb);
                drop_ready_from_b(&mut p, &mut to_drop);
                sent_a.extend(p.qa.0.iter().map(|b| dec(b).unwrap().msg));
                p.exchange();
            }
            assert_eq!(
                to_drop, 0,
                "lost {lost}: 落とす READY が足りない（検査の前提）"
            );
            let ga = *p.ca().match_gate().unwrap();
            assert!(ga.matched(), "lost {lost}: 送り直しの READY で成立しない");
            assert_eq!(ga.closed(), None, "lost {lost}: 新しい相手を閉じた");
            assert_eq!(count(&sent_a, |m| matches!(m, Message::Bye(_))), 0);
        }
    }

    fn run_coarse(p: &mut Pair, total: u32, step: u32) -> Vec<Message> {
        let mut sent = Vec::new();
        for _ in 0..total / step {
            p.now += step;
            p.a.poll(p.now, &mut p.qa);
            p.b.poll(p.now, &mut p.qb);
            sent.extend(p.qa.0.iter().map(|b| dec(b).unwrap().msg));
            p.exchange();
        }
        sent
    }

    #[test]
    fn an_unmatched_link_closes_with_bye_after_ten_minutes() {
        let mut p = gated_pair();
        let now = p.now;
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        let left = p.ca().match_gate().unwrap().left_ms(now);
        assert!(
            left > MATCH_WAIT_MS - 100 && left <= MATCH_WAIT_MS,
            "{left}"
        );
        let before = run_coarse(&mut p, left - 500, 500);
        assert!(!p.ca().match_gate().unwrap().expired());
        assert_eq!(
            count(&before, |m| matches!(m, Message::Bye(_))),
            0,
            "10 分より前に閉じた"
        );
        let after = run_coarse(&mut p, 1_000, 500);
        assert!(p.ca().match_gate().unwrap().expired());
        assert!(
            after.contains(&Message::Bye(ByeReason::Cancelled)),
            "時間切れで BYE(取消) を出さない: {after:?}"
        );
        let mut p = gated_pair();
        let now = p.now;
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        let (b, qb) = (&mut p.b, &mut p.qb);
        b.connected_mut().unwrap().set_ready(now, qb);
        p.exchange();
        let sent = run_coarse(&mut p, 2 * MATCH_WAIT_MS, 500);
        assert!(!p.ca().match_gate().unwrap().expired());
        assert_eq!(count(&sent, |m| matches!(m, Message::Bye(_))), 0);
    }

    #[test]
    fn without_the_gate_the_link_is_matched_at_once_and_never_sends_ready() {
        let mut p = Pair::connect();
        assert!(p.ca().matched());
        assert!(p.ca().match_gate().is_none());
        let now = p.now;
        p.a.connected_mut().unwrap().set_ready(now, &mut p.qa);
        let mut sent = Vec::new();
        for _ in 0..3_000 {
            p.now += 1;
            p.a.poll(p.now, &mut p.qa);
            sent.extend(p.qa.take());
        }
        assert!(!sent.is_empty());
        assert_eq!(count(&sent, |m| *m == Message::Ready), 0);
        let bytes = enc_to(&p.a, SID, &Message::Ready);
        let r = p.a.on_datagram(addr(PORT_B), &bytes, p.now, &mut p.qa);
        assert_eq!(r, Recv::Nothing);
        assert!(p.ca().matched());
        let later = run_coarse(&mut p, 2 * MATCH_WAIT_MS, 500);
        assert_eq!(count(&later, |m| matches!(m, Message::Bye(_))), 0);
        assert_eq!(count(&later, |m| *m == Message::Ready), 0);
    }

    // ================= 使い捨ての鍵と封じた便 =================

    /// 封じた便の nonce の欄（ヘッダの後ろの 8 B）。HMAC の便（HELLO・HELLO_ACK）は None。
    fn nonce_field_of(b: &[u8]) -> Option<[u8; 8]> {
        if wire::is_signed_type(*b.get(5)?) {
            return None;
        }
        b.get(wire::HEADER_LEN..wire::HEADER_LEN + 8)?
            .try_into()
            .ok()
    }

    #[test]
    fn sealed_packets_before_the_key_exchange_are_counted_and_the_handshake_still_connects() {
        let (mut a, b) = (link(PORT_B, 1), link(PORT_A, 2));
        let mut q = Q::default();
        let early = seal_with(
            DirKeys::from_raw([0x3F; 32], [0x3F; 32]),
            SID,
            &Message::Agree {
                host_seat: Seat::P1,
                delay: 2,
                mode: Mode::Delay,
            },
        );
        for t in 1..=3 {
            assert_eq!(
                a.on_datagram(addr(PORT_B), &early, t, &mut q),
                Recv::Nothing
            );
        }
        let c = a.recv_counts();
        assert_eq!(
            (
                c.before_keys,
                c.bad_tag,
                c.decode_errors,
                c.before_established
            ),
            (3, 0, 0, 0),
            "{c:?}"
        );
        assert!(a.dir_keys().is_none(), "鍵の交換の前に鍵がある");
        let p = Pair::connect_links(a, b);
        assert!(p.a.connected().is_some() && p.b.connected().is_some());
        assert!(p.a.dir_keys().is_some());
    }

    #[test]
    fn tampered_or_foreign_keyed_sealed_packets_are_dropped_and_counted_after_connecting() {
        let mut p = Pair::connect();
        let now = p.now;
        let good = enc_to(&p.a, SID, &Message::Ping { send_time: 5 });
        p.a.on_datagram(addr(PORT_B), &good, now, &mut p.qa);
        assert_eq!(p.qa.take(), vec![Message::Pong { send_time: 5 }]);
        let before = p.a.recv_counts().bad_tag;
        let mut poisons: Vec<(&str, Vec<u8>)> = Vec::new();
        for i in [wire::HEADER_LEN, wire::HEADER_LEN + 9, good.len() - 1] {
            let mut x = good.clone();
            x[i] ^= 0x01;
            poisons.push(("1 ビット", x));
        }
        let mut hdr = good.clone();
        hdr[7] = 1; // 予約（ヘッダは追加の認証データ）
        poisons.push(("ヘッダの予約", hdr));
        let mut own = Q::default();
        p.a.connected_mut()
            .unwrap()
            .submit_check(check(0, 1), &mut own);
        poisons.push(("自分の送り（別の向きの鍵）", own.0[0].clone()));
        let k2 = AuthKey::derive(&SECRET, SID ^ 0x10);
        let a_pub = p.a.public_key();
        let other_session = Ephemeral::from_seed(&seed_of(2))
            .dir_keys(&a_pub, true, SID ^ 0x10, &k2)
            .unwrap();
        poisons.push((
            "別の session の鍵",
            seal_with(other_session, SID, &Message::Ping { send_time: 6 }),
        ));
        let k3 = AuthKey::derive(&Secret([0x3D; 16]), SID);
        let other_secret = Ephemeral::from_seed(&seed_of(2))
            .dir_keys(&a_pub, true, SID, &k3)
            .unwrap();
        poisons.push((
            "別の秘密の鍵",
            seal_with(other_secret, SID, &Message::Ping { send_time: 7 }),
        ));
        let n = poisons.len() as u64;
        for (what, b) in &poisons {
            let r = p.a.on_datagram(addr(PORT_B), b, now, &mut p.qa);
            assert_eq!(r, Recv::Nothing, "{what}");
            assert!(p.qa.0.is_empty(), "{what} に返した");
        }
        assert_eq!(p.a.recv_counts().bad_tag, before + n);
        assert_eq!(p.ca().counts().input_accepted, 0);
    }

    #[test]
    fn the_link_and_every_copy_of_the_pong_responder_never_share_a_nonce() {
        let mut p = Pair::connect();
        let r1 = p.a.pong_responder().unwrap();
        let r2 = r1.clone();
        let r3 = p.a.pong_responder().unwrap();
        let pong_seq = Seq::new();
        let mut fields = std::collections::HashSet::new();
        let mut by_sender = [0u32; 4];
        let mut note = |b: &[u8], fields: &mut std::collections::HashSet<[u8; 8]>| {
            if let Some(f) = nonce_field_of(b) {
                assert!(fields.insert(f), "nonce が重なった: {f:?}");
                if let Some(x) = by_sender.get_mut(usize::from(f[0])) {
                    *x += 1;
                }
            }
        };
        let hello = p.b_hello.clone().expect("B の HELLO");
        // B（ホスト）の AGREE。繋がった後に渡すと、A（ゲスト）のセッションが AGREE の返しを封じて送る（セッションの送りが `Connected` の番号を使う経路）。
        let agree = p.b_agree.clone().expect("B の AGREE");
        let mut link_agrees = 0;
        for k in 0..60u32 {
            p.now += 17;
            let now = p.now;
            if k % 10 == 0 {
                p.a.on_datagram(addr(PORT_B), &hello, now, &mut p.qa);
            }
            if k % 10 == 5 {
                p.a.on_datagram(addr(PORT_B), &agree, now, &mut p.qa);
                link_agrees +=
                    p.qa.0
                        .iter()
                        .filter(|b| matches!(dec(b).map(|p| p.msg), Ok(Message::Agree { .. })))
                        .count();
            }
            p.a.poll(now, &mut p.qa);
            let mut chk = Q::default();
            p.ca().submit_check(check(k * 30, k as u16), &mut chk);
            assert_eq!(chk.0.len(), 1, "CHECK を送らない");
            for b in chk.0 {
                note(&b, &mut fields);
            }
            let ping = enc_to(&p.a, SID, &Message::Ping { send_time: k });
            for r in [&r1, &r2, &r3] {
                let mut out = [0u8; MAX_DATAGRAM];
                let (_, n) = r
                    .answer(&pong_seq, addr(PORT_B), &ping, &mut out)
                    .expect("PONG");
                note(&out[..n], &mut fields);
            }
            for b in std::mem::take(&mut p.qa.0) {
                note(&b, &mut fields);
            }
            p.qa.1.clear();
        }
        assert_eq!(by_sender[Sender::Pong as usize], 180, "口の PONG の数");
        assert!(
            by_sender[Sender::Link as usize] >= 60,
            "箱の封じた便 {by_sender:?}"
        );
        assert_eq!(by_sender[2] + by_sender[3], 0, "箱と口のほかの印を使った");
        assert_eq!(
            link_agrees, 6,
            "繋がった後の AGREE の返しが出ない（効く期間が無い）"
        );
        let link_next = p.a.next_seq();
        assert_eq!(pong_seq.next(), 180);
        assert!(u64::from(by_sender[Sender::Link as usize]) <= link_next);
    }

    #[test]
    fn a_resume_keeps_the_key_and_the_sequence_and_still_connects() {
        let mut a = link(PORT_B, 9);
        let mut b = link(PORT_A, 2);
        a.set_retry_window(WINDOW);
        let pub0 = a.public_key();
        let (mut qa, mut qb) = (Q::default(), Q::default());
        let mut fields = Vec::new();
        let mut seq_at_restart = None;
        for t in 1..13_000u32 {
            a.poll(t, &mut qa);
            b.poll(t, &mut qb);
            for bytes in std::mem::take(&mut qa.0) {
                if let Some(f) = nonce_field_of(&bytes) {
                    fields.push(f);
                }
                b.on_datagram(addr(PORT_A), &bytes, t, &mut qb);
            }
            qa.1.clear();
            for bytes in std::mem::take(&mut qb.0) {
                // 10 秒の間は B の AGREE の返しを落とす（A は時間切れで引き継ぐ作り直しになる）。
                if t < 10_500 && matches!(dec(&bytes).map(|p| p.msg), Ok(Message::Agree { .. })) {
                    continue;
                }
                a.on_datagram(addr(PORT_B), &bytes, t, &mut qa);
            }
            qb.1.clear();
            if seq_at_restart.is_none() && a.restarts().count == 1 {
                seq_at_restart = Some((t, a.next_seq()));
            }
        }
        assert_eq!(
            a.restarts().last.map(|(_, k)| k),
            Some(Restart::Resume),
            "検査の前提: 引き継ぐ作り直し"
        );
        let (_, seq) = seq_at_restart.expect("作り直した");
        assert!(
            seq > 0,
            "作り直しの前に封じた便を送っていない（検査の前提）"
        );
        assert!(a.connected().is_some(), "作り直しの後に繋がらない");
        assert_eq!(a.public_key(), pub0, "引き継ぐ作り直しで秘密鍵が変わった");
        let seqs: Vec<u64> = fields
            .iter()
            .filter(|f| f[0] == Sender::Link as u8)
            .map(|f| crate::seal::read_nonce_field(f).1)
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "番号が戻った・重なった"
        );
        assert!(seqs.iter().any(|&s| s >= seq), "作り直しの後の便が無い");
    }

    #[test]
    fn a_fresh_restart_keeps_the_key_and_picks_up_a_peer_restarted_with_a_new_nonce() {
        let mut b1 = node(PORT_B, PORT_A, 2, 0);
        b1.until = 3_000;
        let b2 = node(PORT_B, PORT_A, 3, 5_000);
        let mut ns = [node(PORT_A, PORT_B, 9, 0), b1, b2];
        sim(&mut ns, 3, WINDOW, 12_000, &|t, m| {
            t < 3_000 && matches!(m, Message::Agree { .. })
        });
        let a = ns[0].link.as_ref().unwrap();
        let at = ns[0].connected_at.expect("A が B2 と繋がらない");
        assert!((10_000..10_100).contains(&at), "繋がった時刻 {at}");
        assert_eq!(
            a.restarts().last.map(|(_, k)| k),
            Some(Restart::Fresh),
            "最初からの作り直しでない"
        );
        assert!(a.handshake_counters().stale_nonce > 0);
        assert_eq!(
            a.public_key(),
            Ephemeral::from_seed(&seed_of(9)).public(),
            "最初からの作り直しで秘密鍵が変わった"
        );
        assert!(a.next_seq() > 0);
        let b2_keys = ns[2].link.as_ref().unwrap().dir_keys().unwrap();
        assert_eq!(a.dir_keys().unwrap(), b2_keys.swapped());
    }

    #[test]
    fn a_peer_with_the_same_nonce_and_a_new_key_is_picked_up_without_waiting() {
        let mut b1 = node(PORT_B, PORT_A, 2, 0);
        b1.until = 3_000;
        let mut b2 = node(PORT_B, PORT_A, 2, 5_000);
        b2.seed = KeySeed([0xB2; 32]);
        let mut ns = [node(PORT_A, PORT_B, 9, 0), b1, b2];
        sim(&mut ns, 3, WINDOW, 12_000, &|t, m| {
            t < 3_000 && matches!(m, Message::Agree { .. })
        });
        let a = ns[0].link.as_ref().unwrap();
        let at = ns[0].connected_at.expect("A が B2 と繋がらない");
        assert!((5_000..5_100).contains(&at), "繋がった時刻 {at}");
        assert_eq!(a.restarts().count, 0, "作り直しを待った");
        let c = a.handshake_counters();
        assert_eq!((c.rekeyed, c.stale_nonce), (1, 0), "{c:?}");
        assert_eq!(
            a.public_key(),
            Ephemeral::from_seed(&seed_of(9)).public(),
            "秘密鍵が変わった"
        );
        let b2_keys = ns[2].link.as_ref().unwrap().dir_keys().unwrap();
        assert_eq!(a.dir_keys().unwrap(), b2_keys.swapped());
        assert_eq!(ns[2].connected_at.map(|t| t < 5_100), Some(true));
    }

    /// 荒いネットワーク（落とす・遅らせて順を入れ替える・重ねる）の 1 実行。B1（B2 と同じ nonce・別の種）は `b1_until` で止まり、B2 は
    /// `b2_from` に起きる（同じポート）。戻りは（A の繋がった時刻, B2 の繋がった時刻, A の送った便の全部, A と B2 のセッション）。
    #[allow(clippy::too_many_arguments)] // 検査の実行の条件を 1 か所で振る
    fn rough_run(
        rng_seed: u64,
        b1_until: u32,
        b2_from: u32,
        end: u32,
        drop_pct: u64,
        dup_pct: u64,
        max_delay: u64,
        same_nonce: bool,
    ) -> (Option<u32>, Option<u32>, Vec<Vec<u8>>, [Link; 2]) {
        let mut x = rng_seed | 1;
        let mut rng = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let b2_nonce = if same_nonce { 2 } else { 3 };
        let mk = |nonce: u64, peer: u16, seed: KeySeed, t: u32| {
            let mut l = Link::new(Config::single(hs_cfg(nonce), SECRET, addr(peer), seed), t);
            l.set_retry_window(WINDOW);
            l
        };
        let (mut a, mut b1, mut b2) = (None::<Link>, None::<Link>, None::<Link>);
        let (mut qa, mut qb1, mut qb2) = (Q::default(), Q::default(), Q::default());
        let mut wire: Vec<(u32, SocketAddr, SocketAddr, Vec<u8>)> = Vec::new();
        let mut sent_by_a = Vec::new();
        let (mut a_at, mut b2_at) = (None, None);
        for t in 0..end {
            if t == 0 {
                a = Some(mk(9, PORT_B, seed_of(9), t));
                b1 = Some(mk(2, PORT_A, seed_of(2), t));
            }
            if t == b1_until {
                b1 = None;
            }
            if t == b2_from {
                b2 = Some(mk(b2_nonce, PORT_A, KeySeed([0xB2; 32]), t));
            }
            let (due, rest): (Vec<_>, Vec<_>) = wire.into_iter().partition(|w| w.0 <= t);
            wire = rest;
            for (_, from, to, bytes) in due {
                if to == addr(PORT_A) {
                    if let Some(l) = a.as_mut() {
                        l.on_datagram(from, &bytes, t, &mut qa);
                    }
                } else if let Some(l) = b2.as_mut() {
                    l.on_datagram(from, &bytes, t, &mut qb2);
                } else if let Some(l) = b1.as_mut() {
                    l.on_datagram(from, &bytes, t, &mut qb1);
                }
            }
            let mut out: Vec<(SocketAddr, SocketAddr, Vec<u8>)> = Vec::new();
            if let Some(l) = a.as_mut() {
                l.poll(t, &mut qa);
                if a_at.is_none() && l.connected().is_some() {
                    a_at = Some(t);
                }
                for (to, bytes) in drain(&mut qa) {
                    sent_by_a.push(bytes.clone());
                    out.push((addr(PORT_A), to, bytes));
                }
            }
            for (l, q) in [(b1.as_mut(), &mut qb1), (b2.as_mut(), &mut qb2)] {
                if let Some(l) = l {
                    l.poll(t, q);
                    for (to, bytes) in drain(q) {
                        out.push((addr(PORT_B), to, bytes));
                    }
                }
            }
            if b2_at.is_none() && b2.as_ref().is_some_and(|l| l.connected().is_some()) {
                b2_at = Some(t);
            }
            for (from, to, bytes) in out {
                let msg = dec(&bytes).ok().map(|p| p.msg);
                if t < b1_until
                    && (matches!(msg, Some(Message::Agree { .. }))
                        || (from == addr(PORT_A) && matches!(msg, Some(Message::HelloAck { .. }))))
                {
                    continue;
                }
                let copies = if rng() % 100 < dup_pct { 2 } else { 1 };
                for _ in 0..copies {
                    if rng() % 100 < drop_pct {
                        continue;
                    }
                    let d = 1 + (rng() % max_delay) as u32;
                    wire.push((t + d, from, to, bytes.clone()));
                }
            }
        }
        let boxes = [a.expect("A"), b2.expect("B2")];
        (a_at, b2_at, sent_by_a, boxes)
    }

    #[test]
    fn a_rough_network_never_leaves_a_stale_key_after_the_same_nonce_restart() {
        let (mut stale_dropped, mut fast, mut worst_fast) = (0u32, 0u32, 0u32);
        let runs = 200u64;
        for k in 0..runs {
            let b2_from = 3_000 + (k % 8) as u32 * 50;
            let (a_at, b2_at, sent, [a, b2]) = rough_run(
                0x5EED_0000 + k * 7919,
                3_000,
                b2_from,
                17_000,
                15,
                20,
                400,
                true,
            );
            let tag = format!("{k} 回目（B2 は {b2_from} ms に起きる）");
            let state = format!(
                "{:?} {:?} {:?} / B2 {:?} {:?}",
                a.handshake_state(),
                a.handshake_counters(),
                a.recv_counts(),
                b2.handshake_state(),
                b2.handshake_counters()
            );
            let a_at = a_at.unwrap_or_else(|| panic!("{tag}: A が繋がらない {state}"));
            let b2_at = b2_at.unwrap_or_else(|| panic!("{tag}: B2 が繋がらない {state}"));
            let took = a_at.max(b2_at) - b2_from;
            if took < 3_000 {
                fast += 1;
                worst_fast = worst_fast.max(took);
            }
            assert_eq!(
                a.dir_keys().expect("A の鍵"),
                b2.dir_keys().expect("B2 の鍵").swapped(),
                "{tag}: A と B2 の鍵が食い違う"
            );
            stale_dropped += a.handshake_counters().retired_hello;
            let mut seen = std::collections::HashSet::new();
            for b in &sent {
                if let Some(f) = nonce_field_of(b) {
                    assert!(seen.insert(f), "{tag}: A の nonce が重なった {f:?}");
                }
            }
        }
        eprintln!(
            "荒いネットワークの {runs} 回: B2 が起きて 3 秒以内に繋がった回 {fast}（その最悪 {worst_fast} ms）・遅れた古いセッションの HELLO を捨てた回数の合計 {stale_dropped}"
        );
        assert!(
            fast * 10 >= runs as u32 * 9,
            "3 秒以内に繋がったのは {runs} 走のうち {fast}"
        );
        assert!(
            stale_dropped > 0,
            "古いセッションの遅れた HELLO が 1 本も届いていない（窓を踏めていない）"
        );
    }

    #[test]
    fn a_rough_network_with_a_new_nonce_still_waits_for_the_timeout() {
        let (a_at, _, _, [a, _]) = rough_run(0x5EED_0001, 3_000, 3_050, 15_000, 15, 20, 400, false);
        let a_at = a_at.expect("A が繋がらない");
        assert!(a_at >= 9_000, "nonce 違いが早く繋がった {a_at}");
        assert_eq!(a.handshake_counters().rekeyed, 0);
    }

    #[test]
    fn a_hello_of_another_build_stops_both_sides_with_the_peers_build() {
        let (mut a, mut b) = (link(PORT_B, 1), link(PORT_A, 2));
        a.set_retry_window(WINDOW);
        b.set_retry_window(WINDOW);
        a.hs.set_build(403_920);
        let (mut qa, mut qb) = (Q::default(), Q::default());
        for t in 1..25_000u32 {
            a.poll(t, &mut qa);
            b.poll(t, &mut qb);
            for bytes in std::mem::take(&mut qa.0) {
                b.on_datagram(addr(PORT_A), &bytes, t, &mut qb);
            }
            for bytes in std::mem::take(&mut qb.0) {
                a.on_datagram(addr(PORT_B), &bytes, t, &mut qa);
            }
            qa.1.clear();
            qb.1.clear();
        }
        assert_eq!(
            a.handshake_state(),
            handshake::State::Failed(FailReason::OtherBuild {
                theirs: crate::update::BUILD_NUMBER
            })
        );
        assert_eq!(
            b.handshake_state(),
            handshake::State::Failed(FailReason::OtherBuild { theirs: 403_920 })
        );
        assert_eq!(
            (a.restarts().count, b.restarts().count),
            (0, 0),
            "ビルド違いで作り直した"
        );
        assert!(a.connected().is_none() && b.connected().is_none());
        let p = Pair::connect();
        assert!(p.a.connected().is_some());
    }

    #[test]
    fn a_version_4_hello_still_stops_the_handshake_with_its_version() {
        let mut a = link(PORT_B, 1);
        let mut q = Q::default();
        a.poll(1, &mut q);
        // バージョン 4 の HELLO: 今の HELLO の本文の +46 より後ろ（build・公開鍵）を落とし、バージョンを 4 にしてタグを付け直す。
        let mut v4 = enc(SID, &hello(7));
        v4.truncate(wire::HEADER_LEN + 46);
        v4[4] = 4;
        v4[wire::HEADER_LEN] = 4;
        let tag = AuthKey::derive(&SECRET, SID).tag(&v4);
        v4.extend_from_slice(&tag);
        a.on_datagram(addr(PORT_B), &v4, 2, &mut q);
        assert_eq!(
            a.handshake_state(),
            handshake::State::Failed(FailReason::OtherVersion { theirs: 4 })
        );
        assert_eq!(a.recv_counts().other_version, 1);
    }

    #[test]
    fn a_fresh_restart_back_to_the_same_peer_key_continues_the_sequence() {
        let mut a = link(PORT_B, 9);
        a.set_retry_window(WINDOW);
        let mut b = link(PORT_A, 2);
        let mut other = Link::new(
            Config::single(hs_cfg(3), SECRET, addr(PORT_A), KeySeed([0xB2; 32])),
            0,
        );
        let mut qb = Q::default();
        b.poll(1, &mut qb);
        let b_hello = qb.0.first().cloned().expect("B の HELLO");
        let mut qo = Q::default();
        other.poll(1, &mut qo);
        let other_hello = qo.0.first().cloned().expect("起こし直した B の HELLO");
        let mut q = Q::default();
        let mut seen: Vec<(DirKeys, [u8; 8])> = Vec::new();
        let mut keys_before = None;
        let mut after_restart = 0;
        for t in 1..14_000u32 {
            match t {
                1 => {
                    a.on_datagram(addr(PORT_B), &b_hello, t, &mut q);
                    keys_before = a.dir_keys();
                }
                500 => {
                    a.on_datagram(addr(PORT_B), &other_hello, t, &mut q);
                }
                12_000 => {
                    assert_eq!(a.restarts().last.map(|(_, k)| k), Some(Restart::Fresh));
                    assert!(a.dir_keys().is_none(), "最初からの作り直しで鍵を捨てない");
                    a.on_datagram(addr(PORT_B), &b_hello, t, &mut q);
                }
                _ => {}
            }
            a.poll(t, &mut q);
            for bytes in std::mem::take(&mut q.0) {
                if let (Some(f), Some(k)) = (nonce_field_of(&bytes), a.dir_keys()) {
                    assert!(
                        !seen.contains(&(k, f)),
                        "{t} ms: 同じ鍵で同じ nonce を使った {f:?}"
                    );
                    seen.push((k, f));
                    if t >= 12_000 {
                        after_restart += 1;
                    }
                }
            }
            q.1.clear();
        }
        assert_eq!(a.restarts().count, 1);
        assert_eq!(
            a.dir_keys(),
            keys_before,
            "同じ相手の鍵へ戻っていない（検査の前提）"
        );
        assert!(
            after_restart > 0,
            "作り直しの後に封じた便を送っていない（効く期間が無い）"
        );
        assert!(
            seen.len() > after_restart,
            "作り直しの前に封じた便を送っていない（効く期間が無い）"
        );
    }
}
