//! プロトコルのシリアライズ（encode / decode）。ヘッダは 12 B、本文の配置は `Message` と各本文の型の doc。
//!
//! ここは encode / decode だけを持つ。ack・ウィンドウ・再送・latency の切替の意味は状態機械の仕事で、`Message` はただの値。
//! 受信専用スレッドとゲームスレッドの両方から呼ぶので、ヒープを使わない（語の列は `[u16; MAX_WORDS]` ＋ 個数、
//! name は `[u8; NAME_MAX]` ＋ 長さ）。
//!
//! ## 検査の方針
//!
//! - HELLO 本文の offset 0 の `proto_ver`（ヘッダ offset 4 とは別欄。送り手が話せるいちばん新しいバージョン）は `PROTO_VER` 以外を捨てる。
//! - ヘッダのバージョン（offset 4）が違うものは `BadVersion(相手のバージョン)` で捨てる。タグを比べた**後**に見るので、`BadVersion` が返った
//!   ものは秘密を知る相手が別のバージョンで送ったものと言える。接続のセッション（`link.rs`）はそれを見て、繋がる前に「相手のバージョンが違う」で
//!   手順をやめる。
//! - INPUT / MENU の入力ワードの中身（ゲームの 16 bit）と SELECT のゲームの値（キャラ・色など）は検査しない。MENU の入力ワードは
//!   `0x0100` も送るので u16 の全域を許す。
//! - STEP / STEP_REQ の `value` は画面と `kind` で意味が変わるので検査しない。`screen`（押した画面の ID）も、
//!   受け手が自分の画面と比べる。
//!
//! ## 認証
//!
//! 鍵を交換する便（HELLO・HELLO_ACK・SPECTATE_JOIN。[`is_signed_type`] が決める 1 か所）は、末尾に 16 B の HMAC のタグ
//! （`auth.rs`）を付ける。ほかの全部（AGREE・INPUT・PING……）は使い捨ての鍵から引いた向きごとの鍵で封じる
//! （`seal`。ChaCha20-Poly1305。`seal.rs`）: ヘッダ 12 B ‖ nonce の欄 8 B ‖ 暗号化した本文 ‖ タグ 16 B。ヘッダは暗号化せず
//! 追加の認証データにする（`peek_*` が type と session を覗ける）。1 データグラムの上限 `MAX_DATAGRAM`（1200 B）はタグ込み。
//! いちばん長い INPUT は 12 + 8 + 80 + 16 = 116 B。
//!
//! `decode` の順: 長さ → magic → ヘッダのバージョンが違えば、type に依らず HMAC のタグで確かめて `BadVersion`（合わなければ `BadTag`。
//! 封じる鍵の無い古いバージョンの相手の便を読む前にバージョンで断るガード）→ type が HMAC の 3 つならタグ、ほかは`Opener` で開く（`Opener` が無ければ
//! `NotKeyed`、開かなければ `BadTag`）→ ヘッダの flags・予約・type → 本文。ヘッダの session_id もタグの中なので、
//! 別の接続の遅れたパケットは鍵が違って落ちる。
//!
//! 受信ソケットには STUN の応答も来る。どちらかは `classify` が先頭で分ける（HGRT は magic、STUN は offset 4 の magic cookie）。

use crate::auth::AuthKey;
pub use crate::auth::TAG_LEN;
use crate::frame::MAX_DELAY;
use crate::seal::{NONCE_FIELD_LEN, Opener, PUBLIC_LEN, SEAL_OVERHEAD, SEAL_TAG_LEN, Sealer, Seq};

/// パケットの先頭 4B。
pub const MAGIC: [u8; 4] = *b"HGRT";
/// プロトコルのバージョン（ヘッダ offset 4 と HELLO 本文の offset 0。接続コードの offset 0 も同じ数）。違うバージョンの相手とは繋がる前に断る。
pub const PROTO_VER: u8 = 7;

/// HELLO の本文の長さ（B）。
pub const HELLO_LEN: usize = 82;
/// HELLO_ACK の本文の長さ（B。16 B ＋ 公開鍵）。
pub const HELLO_ACK_LEN: usize = 16 + PUBLIC_LEN;
/// HELLO の +3 の bit1（rollback ができる。bit0 が望むモード）。
const HELLO_CAN_ROLLBACK_BIT: u8 = 2;
/// HELLO の +3 の bit2（観戦の一覧に載せてよい。`[spectate] enable = 1`）。
const HELLO_LISTABLE_BIT: u8 = 4;
pub const HEADER_LEN: usize = 12;
/// INPUT / MENU の入力ワードの最大個数。
pub const MAX_WORDS: usize = 32;
/// 1 データグラムの上限（経路 MTU を割らない）。
pub const MAX_DATAGRAM: usize = 1200;
/// HELLO の name の最大バイト数。
pub const NAME_MAX: usize = 32;

// type（ヘッダ offset 5）の値。0・5〜7・12・22 以上は欠番・未実装で、知らない type として捨てる。
// 公開 API は `Message` の variant で表すので、ここは非公開。
const TYPE_HELLO: u8 = 1;
const TYPE_HELLO_ACK: u8 = 2;
const TYPE_INPUT: u8 = 3;
const TYPE_CHECK: u8 = 4;
const TYPE_PING: u8 = 8;
const TYPE_PONG: u8 = 9;
const TYPE_AGREE: u8 = 10;
const TYPE_SELECT: u8 = 11;
const TYPE_SPECTATE_JOIN: u8 = 13;
const TYPE_SPECTATE_STREAM: u8 = 14;
const TYPE_BYE: u8 = 15;
const TYPE_MENU: u8 = 16;
const TYPE_STEP_REQ: u8 = 17;
const TYPE_STEP: u8 = 18;
const TYPE_SPECTATE_ACK: u8 = 19;
/// 両者が「妖怪 対 妖怪」を選んだら成立。
const TYPE_READY: u8 = 20;
/// ホストの表示用の状態（ホスト → ゲストだけ）。
const TYPE_HOST_STATE: u8 = 21;

/// AGREE の host_seat。0/1 以外は捨てる値なので、型で範囲を保証する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seat {
    P1 = 0,
    P2 = 1,
}

impl Seat {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Seat::P1),
            1 => Some(Seat::P2),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }
}

/// 遅延の方式（HELLO の +3「望むモード」と AGREE の +2「決まったモード」）。0 / 1 以外は捨てる値なので、型で範囲を保証する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 今の方式。相手の入力ワードが揃うまで待つ。
    Delay = 0,
    /// 相手の入力ワードが届いていないフレームを予測で進め、届いた入力ワードで進め直す（`rollback.rs`）。
    Rollback = 1,
}

impl Mode {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Mode::Delay),
            1 => Some(Mode::Rollback),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }
}

/// 席の希望（HELLO の +44）。0〜2 以外は捨てる値なので、型で範囲を保証する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatWish {
    /// おまかせ（相手の希望に合わせる。両方おまかせなら乱数）。
    Any = 0,
    P1 = 1,
    P2 = 2,
}

impl SeatWish {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(SeatWish::Any),
            1 => Some(SeatWish::P1),
            2 => Some(SeatWish::P2),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }
}

/// ルームに先にいたかの名乗り（HELLO の +45）。0〜2 以外は捨てる値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// 名乗らない（ini の `peer`・ローダーのウィンドウ・peer_sim。ホストを先と見る）。
    Unknown = 0,
    /// 先にいた（始める側。ルームに先に入った・「接続コードを作る」を選んだ）。
    First = 1,
    /// 後から来た（返す側）。
    Second = 2,
}

impl Arrival {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Arrival::Unknown),
            1 => Some(Arrival::First),
            2 => Some(Arrival::Second),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }
}

/// BYE の reason。1〜4 以外は捨てる値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByeReason {
    Finished = 1,
    Disconnected = 2,
    Desync = 3,
    Cancelled = 4,
}

impl ByeReason {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(ByeReason::Finished),
            2 => Some(ByeReason::Disconnected),
            3 => Some(ByeReason::Desync),
            4 => Some(ByeReason::Cancelled),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }
}

/// STEP_REQ / STEP の kind。0 と 5 以上は捨てる値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Decide = 1,
    Cancel = 2,
    /// 合わせ（ホストだけが STEP に載せる。STEP_REQ に載っていたら decode が捨てる）。選択の画面へ入ったとき、
    /// ゲストのカーソルをホストの値へ寄せる。画面は移らないが、STEP なので step は 1 進む。
    Align = 3,
    /// 対戦の形式の切替（キャラセレクト（画面 8）の間の Tab）。STEP の value は [`FormatValue`]、STEP_REQ の value は
    /// 要求（形式だけ。D と旗はホストが決める）。画面は移らず、ゲームへは何も入れないが、STEP なので step は 1 進み、
    /// 両者が同じ tick にネットワークの側の形式を替える。**観戦の流れには載せない**（`decode` は観戦の記録の中の Format を捨てる）。
    Format = 4,
}

impl StepKind {
    /// ワイヤの 1 B から。STEP の種類の表はここ 1 か所。
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(StepKind::Decide),
            2 => Some(StepKind::Cancel),
            3 => Some(StepKind::Align),
            4 => Some(StepKind::Format),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// `StepKind::Format` の value（1 B）: bit0 = 形式（1 = rollback・0 = delay 方式）、bit1 = auto の旗、bit2〜5 = 新しい D
/// （0〜`mode_pick::MAX_PICK_DELAY`）、bit6〜7 = 0。ホストが STEP に載せる完全な形（`pack`）と、ゲストの STEP_REQ に載る
/// 要求（形式だけ。`request`）がある。ゲストの要求に D と旗が無いのは、それらの正がホストだけにあるため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatValue {
    pub mode: Mode,
    /// 新しい D（delay 方式の遅れも同じ欄。範囲は両形式とも `delay_pick::bounds`）。
    pub delay: u8,
    /// 切替の後も auto の旗を立てておくか（ホストの旗。ゲストは使わない）。
    pub auto: bool,
}

impl FormatValue {
    const MODE_BIT: u8 = 1;
    const AUTO_BIT: u8 = 2;
    const DELAY_SHIFT: u8 = 2;
    const DELAY_MASK: u8 = 0x0F;

    /// 1 B に詰める。D は範囲の上限に詰める（呼び手の取り違えでワイヤの検査に落ちる値を作らない）。
    pub fn pack(self) -> u8 {
        let d = self.delay.min(crate::mode_pick::MAX_PICK_DELAY);
        self.mode.to_u8() | (u8::from(self.auto) * Self::AUTO_BIT) | (d << Self::DELAY_SHIFT)
    }

    /// 1 B から。予約のビット（6〜7）が立っている・D が範囲の外は `None`（`decode` が STEP ごと捨てる）。
    pub fn unpack(v: u8) -> Option<Self> {
        if v & 0xC0 != 0 {
            return None;
        }
        let delay = (v >> Self::DELAY_SHIFT) & Self::DELAY_MASK;
        if delay > crate::mode_pick::MAX_PICK_DELAY {
            return None;
        }
        Some(FormatValue {
            mode: Mode::from_u8(v & Self::MODE_BIT)?,
            delay,
            auto: v & Self::AUTO_BIT != 0,
        })
    }

    /// ゲストの要求の value（形式だけ。D = 0・旗なし）。
    pub fn request(mode: Mode) -> u8 {
        mode.to_u8()
    }

    pub fn is_request(v: u8) -> bool {
        v & !Self::MODE_BIT == 0
    }

    pub fn requested_mode(v: u8) -> Option<Mode> {
        Self::is_request(v).then(|| Mode::from_u8(v & Self::MODE_BIT))?
    }
}

/// INPUT に載る latency の切替。`None` はワイヤ上 `lat_frame = 0xFFFFFFFF` ・ `lat_delay = 0`（「切替なし」の詰め方）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatChange {
    /// 切替を結び付けた apply_frame。
    pub frame: u32,
    pub delay: u8,
}

/// HOST_STATE の本文（type 21。ホスト → ゲスト。5 B）。ゲストの画面の (A)（auto の旗）と、INPUT が流れない間（キャラセレクト
/// など）にホストが押した D を出すための表示用の値で、同期の芯には入らない。旗を持つのはホストだけなので、ゲストに知らせる
/// 経路はこれだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostState {
    /// 便の番号（古い便を捨てる。ホストが値を送るたびに 1 進む。u16 を回る。受け手は差の符号つきで比べる）。
    pub seq: u16,
    /// ホストの今の対戦の形式。
    pub mode: Mode,
    /// 画面に出す目標の D（ホストの `shown_delay`）。0〜`delay_pick::bounds` の上限。
    pub delay: u8,
    pub auto: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// 本文中の proto_ver（ヘッダの欄とは別）。
    pub proto_ver: u8,
    pub latency: u8,
    /// name の有効バイト数（0〜32）。
    pub name_len: u8,
    /// 望むモード（+3 の bit0）。
    pub mode: Mode,
    /// rollback が**できる**か（+3 の bit1）。望み（`mode`）とは別: 望まなくてもできれば、キャラセレクトの Tab で両者が
    /// rollback へ替えられる（両者ができるときだけ）。この印を立てない相手は false。
    pub can_rollback: bool,
    /// このマッチを進行中のランダムマッチの一覧に載せてよいか（+3 の bit2。`[spectate] enable = 1`）。両者が立てたときだけ
    /// ホストが載せる（`watch_list::may_list`）。
    pub listable: bool,
    /// 役割決めの乱数。
    pub role_nonce: u64,
    /// UTF-8。`name_len` より後ろは全部 0（詰め物）。
    pub name: [u8; NAME_MAX],
    /// 席の希望（+44）。
    pub seat_wish: SeatWish,
    /// ルームに先にいたかの名乗り（+45）。
    pub arrival: Arrival,
    /// ビルド番号（+46。`update::BUILD_NUMBER`）。違えば繋がる前に断る（0 と 0 は同じ）。
    pub build: u32,
    /// 使い捨ての鍵の公開鍵（+50。`seal.rs`）。
    pub public: [u8; PUBLIC_LEN],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    pub seg: u16,
    /// 相手から次に要る apply_frame。
    pub ack: u32,
    /// 入力ワードの列の最初の apply_frame。
    pub base: u32,
    /// latency の切替（ホストだけが載せる）。
    pub lat: Option<LatChange>,
    /// 入力ワードの個数（0〜32）。
    pub count: u8,
    /// 入力語（花映塚の 16 bit）。有効なのは `[..count]` だけ（`count` を超える分は decode では 0 で埋める）。
    pub words: [u16; MAX_WORDS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    pub seg: u16,
    pub frame: u32,
    pub rng_state: u16,
    pub rng_count: u32,
    pub life_1p: u8,
    pub life_2p: u8,
}

/// SELECT の本文。ゲームの値（キャラ・色など）の範囲はここでは見ない（書く前に dll が見る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Select {
    pub match_no: u16,
    pub chara: u8,
    pub color: u8,
    pub life: u8,
    pub charge: u8,
    pub map: u8,
    pub difficulty: u8,
    pub cpu_1p: u8,
    pub cpu_2p: u8,
    pub background: u8,
    pub rng_seed: u16,
    pub option_digest: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Menu {
    pub step: u16,
    /// 相手から次に要る語の番号。
    pub ack: u32,
    /// 最初の入力ワードの番号（step ごとに 0 から）。
    pub base: u32,
    pub count: u8,
    /// `count` を超える分は decode では 0 で埋める。
    pub words: [u16; MAX_WORDS],
}

/// STEP_REQ / STEP 共通の本文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub step: u16,
    pub kind: StepKind,
    pub value: u8,
    /// 押した画面の ID（花映塚の `[[0x4AC8C0]+0x1B228]`）。受け手は自分の画面がこれになってから入れる。
    /// 0 は「画面を持たない」（peer_sim の選択）。
    pub screen: u8,
}

/// SPECTATE_STREAM 1 通に載せる記録の最大個数。いちばん長い記録（名前の 35 B）が 16 個で 12 + 6 + 560 + 16 = 594 B
/// （1200 B の枠に収まる）。`Message` は `Copy` で値渡しされるので、増やすと全メッセージの大きさが膨らむ。
pub const MAX_STREAM_RECORDS: usize = 16;

/// 観戦の流れの 1 記録。記録は種類つきで 1 本の通し番号（`spectate.rs`）に並ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// 「この後の記録は seg k のもの」の宣言（観戦者はマッチの状態を捨てて新しい seg に入る）。繋がったとき（seg 0）と
    /// 前の seg を出るとき（次の seg）に積まれ、その seg の選択の押下・SELECT はこの記録の後ろに来る。遅れて JOIN した
    /// 観戦者は、いまの seg のこの記録から流れる。
    Seg { seg: u16 },
    /// ① 止める地点の SELECT の結果。席ごとに 1 つ（P1 → P2 の順に 2 つ続ける）。マップ・難易度・CPU レベル・
    /// シードなど両席で共通の欄は、ホストの値を両方に入れる。
    Select { seat: Seat, sel: Select },
    /// ② 選択の画面で決まった押下（ホストの STEP と同じ値）。
    Step(Step),
    /// ② 選択の画面の 1 フレームぶんの MENU の入力ワード（両席。画面を移るビットを落とした後の入力ワード）。
    Menu { p1: u16, p2: u16 },
    /// ③ マッチの 1 フレームの語（1P・2P）。seg は直前の `Seg` の記録。
    Frame { frame: u32, p1: u16, p2: u16 },
    /// ④ 終わり（BYE の reason）。
    End(ByeReason),
    /// ⑤ 席ごとの名前（UTF-8。HELLO の name と同じガード —— 32 B まで・`len` より後ろは 0）。ホストが各 seg の `Seg` の直後に
    /// 1P → 2P の順に 2 つ積む。`Record::name` で作る。
    Name {
        seat: Seat,
        len: u8,
        bytes: [u8; NAME_MAX],
    },
    /// ⑥ 対戦の状態（観戦者の左下の行の前半）。ホストがマッチの最中に約 1 秒（60 フレーム）に 1 回、対戦者の左下の行と同じ値を
    /// 積む。時間を使わない記録（観戦者は最新の値だけを持つ）。値が無いときは `STATS_PING_UNKNOWN` / `STATS_ROLLBACK_NONE`。
    /// `Record::stats` で作る。
    Stats {
        /// 対戦者 2 人の間の往復（ms。ホストが測った値）。測れていなければ `STATS_PING_UNKNOWN`。
        ping_ms: u16,
        /// 今のマッチの入力の遅れ（rollback なら D、delay 方式なら今の遅れ）。
        latency: u8,
        /// rollback の「戻し」（直近 60 フレームで 1 フレームに進め直した更新の数の最大）。delay 方式なら `STATS_ROLLBACK_NONE`。
        rollback: u8,
    },
}

/// `Record::Stats` の ping が測れていない印（`u16::MAX`。測れた値は 65,534 ms で頭打ち）。
pub const STATS_PING_UNKNOWN: u16 = u16::MAX;
/// `Record::Stats` の rollback が無い（delay 方式のマッチ）印（`u8::MAX`。ある値は 254 で頭打ち）。
pub const STATS_ROLLBACK_NONE: u8 = u8::MAX;

// 記録の種類の 1 B。
const REC_SEG: u8 = 1;
const REC_SELECT: u8 = 2;
const REC_STEP: u8 = 3;
const REC_MENU: u8 = 4;
const REC_FRAME: u8 = 5;
const REC_END: u8 = 6;
const REC_NAME: u8 = 7;
const REC_STATS: u8 = 8;

/// SELECT の本文の長さ（B）。
const SELECT_LEN: usize = 18;
/// STEP_REQ / STEP の本文の長さ（B）。
const STEP_LEN: usize = 5;
/// 記録 `Name` の本文の長さ（B。seat u8 / len u8 / name 32 B）。
const NAME_REC_LEN: usize = 2 + NAME_MAX;
/// 記録 `Stats` の本文の長さ（B。ping_ms u16 / latency u8 / rollback u8）。
const STATS_REC_LEN: usize = 4;
/// SPECTATE_STREAM の本文の、記録の前の部分（first u32・flags u8・count u8）。
const STREAM_HEAD_LEN: usize = 6;
/// SPECTATE_STREAM の flags の bit0（`SpectateStream::reset`）。ほかのビットは 0 でなければ捨てる。
const STREAM_FLAG_RESET: u8 = 1;
/// SPECTATE_JOIN の本文の長さ（B）。
const JOIN_LEN: usize = 10 + NAME_MAX + 4 + PUBLIC_LEN;
/// SPECTATE_JOIN の名前の欄の終わり（バージョン 4 までの本文の長さ）。
const JOIN_NAME_END: usize = 10 + NAME_MAX;
/// SPECTATE_ACK の本文の長さ（B）。
const ACK_LEN: usize = 20;
/// SPECTATE_ACK の `sack`（受け取れた範囲）が指せる記録の数。ビット i（下位が 0）が番号 `next + 1 + i` の記録を表す。
/// 最上位の 1 ビットは予約（0 でなければ捨てる）。ホストのウィンドウ `spectate::SEND_WINDOW`（64）は ack の番号そのものを含むので、
/// `next` より先に受け取っていてよいのは 63 個まで。
pub const SACK_BITS: u32 = 63;
/// `sack` で使ってよいビット（下位 `SACK_BITS` ビット）。
pub const SACK_MASK: u64 = (1u64 << SACK_BITS) - 1;

impl Record {
    /// `decode` が、`count` を超える枠を埋める値（意味は無い）。
    pub const FILLER: Record = Record::Seg { seg: 0 };

    /// 記録 1 つのワイヤ上の長さ（種類の 1 B を含む）。
    pub const fn wire_len(&self) -> usize {
        1 + match self {
            Record::Seg { .. } => 2,
            Record::Select { .. } => 1 + SELECT_LEN,
            Record::Step(_) => STEP_LEN,
            Record::Menu { .. } => 4,
            Record::Frame { .. } => 8,
            Record::End(_) => 1,
            Record::Name { .. } => NAME_REC_LEN,
            Record::Stats { .. } => STATS_REC_LEN,
        }
    }

    /// 名前の記録を作る。`name` の 32 B を超える分は切る。
    pub fn name(seat: Seat, name: &[u8]) -> Record {
        let len = name.len().min(NAME_MAX);
        let mut bytes = [0u8; NAME_MAX];
        for (d, s) in bytes.iter_mut().zip(name) {
            *d = *s;
        }
        Record::Name {
            seat,
            len: len as u8,
            bytes,
        }
    }

    /// 対戦の状態の記録を作る。`ping_ms` / `rollback` が `None`（測れていない・delay 方式）なら印の値、あれば頭打ちして
    /// （印と同じ値にならないよう `STATS_*` の 1 つ手前まで）入れる。
    pub fn stats(ping_ms: Option<u32>, latency: u8, rollback: Option<u32>) -> Record {
        Record::Stats {
            ping_ms: ping_ms.map_or(STATS_PING_UNKNOWN, |p| {
                p.min(u32::from(STATS_PING_UNKNOWN) - 1) as u16
            }),
            latency,
            rollback: rollback.map_or(STATS_ROLLBACK_NONE, |r| {
                r.min(u32::from(STATS_ROLLBACK_NONE) - 1) as u8
            }),
        }
    }

    fn kind_byte(&self) -> u8 {
        match self {
            Record::Seg { .. } => REC_SEG,
            Record::Select { .. } => REC_SELECT,
            Record::Step(_) => REC_STEP,
            Record::Menu { .. } => REC_MENU,
            Record::Frame { .. } => REC_FRAME,
            Record::End(_) => REC_END,
            Record::Name { .. } => REC_NAME,
            Record::Stats { .. } => REC_STATS,
        }
    }

    /// `out`（`wire_len()` B 以上）へ詰める。戻り値は書いた長さ。
    fn write(&self, out: &mut [u8]) -> usize {
        out[0] = self.kind_byte();
        let body = &mut out[1..];
        match self {
            Record::Seg { seg } => put_u16(body, 0, *seg),
            Record::Select { seat, sel } => {
                body[0] = seat.to_u8();
                put_select(&mut body[1..], sel);
            }
            Record::Step(s) => {
                // 形式の切替は観戦の流れに載せない（受ける側の `decode` も捨てる）。
                debug_assert!(
                    s.kind != StepKind::Format,
                    "形式の切替は観戦の流れに積まない"
                );
                put_step(body, s)
            }
            Record::Menu { p1, p2 } => {
                put_u16(body, 0, *p1);
                put_u16(body, 2, *p2);
            }
            Record::Frame { frame, p1, p2 } => {
                put_u32(body, 0, *frame);
                put_u16(body, 4, *p1);
                put_u16(body, 6, *p2);
            }
            Record::End(reason) => body[0] = reason.to_u8(),
            Record::Name { seat, len, bytes } => {
                debug_assert!((*len as usize) <= NAME_MAX, "len は 0..=32");
                body[0] = seat.to_u8();
                body[1] = *len;
                body[2..2 + NAME_MAX].copy_from_slice(bytes);
            }
            Record::Stats {
                ping_ms,
                latency,
                rollback,
            } => {
                put_u16(body, 0, *ping_ms);
                body[2] = *latency;
                body[3] = *rollback;
            }
        }
        self.wire_len()
    }

    /// `buf` の先頭から記録を 1 つ読む。戻り値は（記録, 読んだ長さ）。足りない・種類が外・値が範囲の外は誤り
    /// （パニックしない）。
    fn read(buf: &[u8]) -> Result<(Record, usize), DecodeError> {
        const T: u8 = TYPE_SPECTATE_STREAM;
        let Some(&kind) = buf.first() else {
            return Err(DecodeError::LengthMismatch {
                msg_type: T,
                expected: 1,
                actual: 0,
            });
        };
        let payload_len = match kind {
            REC_SEG => 2,
            REC_SELECT => 1 + SELECT_LEN,
            REC_STEP => STEP_LEN,
            REC_MENU => 4,
            REC_FRAME => 8,
            REC_END => 1,
            REC_NAME => NAME_REC_LEN,
            REC_STATS => STATS_REC_LEN,
            _ => {
                return Err(DecodeError::FieldOutOfRange {
                    msg_type: T,
                    field: "record_kind",
                });
            }
        };
        let Some(p) = buf.get(1..1 + payload_len) else {
            return Err(DecodeError::LengthMismatch {
                msg_type: T,
                expected: 1 + payload_len,
                actual: buf.len(),
            });
        };
        let rec = match kind {
            REC_SEG => Record::Seg { seg: get_u16(p, 0) },
            REC_SELECT => {
                let seat = Seat::from_u8(p[0]).ok_or(DecodeError::FieldOutOfRange {
                    msg_type: T,
                    field: "seat",
                })?;
                Record::Select {
                    seat,
                    sel: parse_select(T, &p[1..])?,
                }
            }
            REC_STEP => Record::Step(decode_step_body(T, p)?),
            REC_MENU => Record::Menu {
                p1: get_u16(p, 0),
                p2: get_u16(p, 2),
            },
            REC_FRAME => Record::Frame {
                frame: get_u32(p, 0),
                p1: get_u16(p, 4),
                p2: get_u16(p, 6),
            },
            REC_NAME => {
                let seat = Seat::from_u8(p[0]).ok_or(DecodeError::FieldOutOfRange {
                    msg_type: T,
                    field: "seat",
                })?;
                let len = p[1];
                if len as usize > NAME_MAX {
                    return Err(DecodeError::FieldOutOfRange {
                        msg_type: T,
                        field: "name_len",
                    });
                }
                let mut bytes = [0u8; NAME_MAX];
                bytes.copy_from_slice(&p[2..2 + NAME_MAX]);
                // HELLO・JOIN と同じガード（後ろが 0・UTF-8）。観戦者がこの名前を保管庫のフォルダ名にするので、読める文字だけを通す。
                if bytes[len as usize..].iter().any(|&b| b != 0) {
                    return Err(DecodeError::FieldOutOfRange {
                        msg_type: T,
                        field: "name_tail",
                    });
                }
                if std::str::from_utf8(&bytes).is_err() {
                    return Err(DecodeError::InvalidUtf8);
                }
                Record::Name { seat, len, bytes }
            }
            // どの値も通す（範囲で断る欄が無い）。
            REC_STATS => Record::Stats {
                ping_ms: get_u16(p, 0),
                latency: p[2],
                rollback: p[3],
            },
            _ => Record::End(
                ByeReason::from_u8(p[0]).ok_or(DecodeError::FieldOutOfRange {
                    msg_type: T,
                    field: "reason",
                })?,
            ),
        };
        Ok((rec, 1 + payload_len))
    }
}

/// SPECTATE_JOIN の本文（観戦者 → ホスト）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpectateJoin {
    /// name の有効バイト数（0〜32）。
    pub name_len: u8,
    /// 観戦者が起動ごとに引く乱数。同じ送り元から違う値の JOIN が来たら、観戦者が起こし直したと見る。
    pub nonce: u64,
    /// UTF-8。`name_len` より後ろは全部 0。
    pub name: [u8; NAME_MAX],
    /// ビルド番号（+42）。違えばホストは JOIN を捨てて数える（`spectate::DropReason::OtherBuild`）。
    pub build: u32,
    /// 観戦者の使い捨ての鍵の公開鍵（+46）。ホストはこれと観戦用の秘密鍵から観戦者ごとの鍵を作る。
    pub public: [u8; PUBLIC_LEN],
}

/// SPECTATE_STREAM の本文（ホスト → 観戦者）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpectateStream {
    /// 最初の記録の通し番号。
    pub first: u32,
    /// 「ここから始めてよい」の印。受け手が持つ連続の終わりより `first` が先でも、そこへ飛ぶ（遅れて入った・
    /// 古い記録がリングバッファから落ちた観戦者へ、いまの seg の始まりから流すとき）。付けないときは飛ばない（順の入れ替わりで
    /// 先の番号が先に着いただけかもしれないので）。
    pub reset: bool,
    /// 記録の個数（0〜`MAX_STREAM_RECORDS`）。0 は「生きている」の合図（受け手は ack を返す）。
    pub count: u8,
    /// 有効なのは `[..count]` だけ（超える分は decode では `Record::FILLER`）。
    pub records: [Record; MAX_STREAM_RECORDS],
}

/// メッセージ（本文だけ。ヘッダの session_id は `Packet` 側）。
// SpectateStream だけが大きい（16 記録で 392 B。ほかは 90 B 以下）。Box にすると確保が要り、受信・ゲームのスレッドで
// 確保しない規則に反する。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    /// HELLO への返し（HELLO と同じく HMAC のタグの便）。
    HelloAck {
        echo_nonce: u64,
        role_nonce: u64,
        public: [u8; PUBLIC_LEN],
    },
    Input(Input),
    Check(Check),
    Ping {
        send_time: u32,
    },
    Pong {
        send_time: u32,
    },
    /// ホストが決めた値。本文は `mode` で長さが変わる:
    /// - `Mode::Delay` は 2 B（host_seat, delay）。
    /// - `Mode::Rollback` は 3 B（host_seat, delay = D, mode = 1）。3 B で mode が 1 以外なら捨てる（同じ値に 2 通りの
    ///   バイト列があると、golden で片方しか縛れない）。
    Agree {
        host_seat: Seat,
        /// delay 方式ではマッチを始める delay、rollback のモードでは D（`apply_frame = capture_frame + D`）。0〜9。
        delay: u8,
        mode: Mode,
    },
    Select(Select),
    Bye(ByeReason),
    Menu(Menu),
    StepReq(Step),
    Step(Step),
    /// 観戦者 → ホスト。観戦用の接続コードの session・鍵のパケット。
    SpectateJoin(SpectateJoin),
    /// ホスト → 観戦者。
    SpectateStream(SpectateStream),
    /// 観戦者 → ホスト。`nonce` は自分の JOIN の値（起こし直す前の遅れた ACK を見分ける）、`next` は次に要る記録の番号
    /// （それより前は連続で受け取った）、`sack` は `next` の**後ろ**で受け取れた記録（ビット i = 番号 `next + 1 + i`。
    /// `next` 自身は受け取れていない）。ホストは、届いていない記録だけを送り直す。
    SpectateAck {
        nonce: u64,
        next: u32,
        sack: u64,
    },
    /// 「自分は対戦を始めてよい（『妖怪 対 妖怪』を選んだ）」（両方向。本文 0 B）。`code_side = game` の実行だけが送る
    /// （`match_gate.rs`）。
    Ready,
    /// ホスト → ゲスト（本文 5 B）。ack は要らない（最新の値だけ持つ。値が変わった tick と約 1 秒ごとに送り直す）。
    /// ホストでない側が受けたら捨てる（`UnknownType`）。
    HostState(HostState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    pub session_id: u32,
    pub msg: Message,
}

/// decode が捨てた理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// ヘッダ 12 B とタグ 16 B（計 28 B）に満たない。
    TooShort { got: usize },
    /// magic が `"HGRT"` でない。
    BadMagic,
    /// 認証タグが合わない（改ざん・別の鍵・別の session_id・切り詰め）。本文は読んでいない。封じた便が開かない（別の向き・
    /// 別の接続の鍵・nonce の欄やヘッダの改ざん）ときも同じ。
    BadTag,
    /// 封じた便だが、`Opener` が無い（鍵の交換がまだ —— 相手の HELLO より先に届いた）。中は読んでいない。
    NotKeyed,
    /// ヘッダの proto_ver が `PROTO_VER` でない（中は相手のバージョン）。タグを比べた後に見る。
    BadVersion(u8),
    /// ヘッダの flags または予約バイト（offset 6・7）が 0 でない。
    HeaderReserved,
    /// 知らない type（0・5〜7・12・22 以上）。
    UnknownType(u8),
    /// 本文の長さが type ごとの固定長（可変部は count から決まる期待値）と違う。
    LengthMismatch {
        msg_type: u8,
        expected: usize,
        actual: usize,
    },
    /// 値が範囲の外、または予約欄が 0 でない。
    FieldOutOfRange { msg_type: u8, field: &'static str },
    /// HELLO の name が UTF-8 として読めない。
    InvalidUtf8,
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(a)
}

fn message_type_id(msg: &Message) -> u8 {
    match msg {
        Message::Hello(_) => TYPE_HELLO,
        Message::HelloAck { .. } => TYPE_HELLO_ACK,
        Message::Input(_) => TYPE_INPUT,
        Message::Check(_) => TYPE_CHECK,
        Message::Ping { .. } => TYPE_PING,
        Message::Pong { .. } => TYPE_PONG,
        Message::Agree { .. } => TYPE_AGREE,
        Message::Select(_) => TYPE_SELECT,
        Message::Bye(_) => TYPE_BYE,
        Message::Menu(_) => TYPE_MENU,
        Message::StepReq(_) => TYPE_STEP_REQ,
        Message::Step(_) => TYPE_STEP,
        Message::SpectateJoin(_) => TYPE_SPECTATE_JOIN,
        Message::SpectateStream(_) => TYPE_SPECTATE_STREAM,
        Message::SpectateAck { .. } => TYPE_SPECTATE_ACK,
        Message::Ready => TYPE_READY,
        Message::HostState(_) => TYPE_HOST_STATE,
    }
}

/// SELECT の本文（18 B）を `out` へ詰める。SELECT 本体と、観戦の記録が同じ配置を使う。
fn put_select(out: &mut [u8], s: &Select) {
    put_u16(out, 0, s.match_no);
    out[2] = s.chara;
    out[3] = s.color;
    out[4] = s.life;
    out[5] = s.charge;
    out[6] = s.map;
    out[7] = s.difficulty;
    out[8] = s.cpu_1p;
    out[9] = s.cpu_2p;
    out[10] = s.background;
    out[11] = 0;
    put_u16(out, 12, s.rng_seed);
    put_u32(out, 14, s.option_digest);
}

/// STEP_REQ / STEP の本文（5 B）を `out` へ詰める。観戦の記録も同じ配置。
fn put_step(out: &mut [u8], s: &Step) {
    put_u16(out, 0, s.step);
    out[2] = s.kind.to_u8();
    out[3] = s.value;
    out[4] = s.screen;
}

/// 本文を `out`（ヘッダの後ろ）へ詰め、書いたバイト数を返す。
///
/// `count` や `latency` などが範囲の外の値は呼び出し側のバグなので `debug_assert!` で早期に落とす（release では素通り。
/// ネットワークから来た値はここを通らない）。
fn encode_body(msg: &Message, out: &mut [u8]) -> usize {
    match msg {
        Message::Hello(h) => {
            debug_assert!(h.latency <= MAX_DELAY, "latency は 0..=9");
            debug_assert!((h.name_len as usize) <= NAME_MAX, "name_len は 0..=32");
            out[0] = h.proto_ver;
            out[1] = h.latency;
            out[2] = h.name_len;
            out[3] = h.mode.to_u8()
                | (u8::from(h.can_rollback) * HELLO_CAN_ROLLBACK_BIT)
                | (u8::from(h.listable) * HELLO_LISTABLE_BIT);
            put_u64(out, 4, h.role_nonce);
            out[12..44].copy_from_slice(&h.name);
            out[44] = h.seat_wish.to_u8();
            out[45] = h.arrival.to_u8();
            put_u32(out, 46, h.build);
            out[50..50 + PUBLIC_LEN].copy_from_slice(&h.public);
            HELLO_LEN
        }
        Message::HelloAck {
            echo_nonce,
            role_nonce,
            public,
        } => {
            put_u64(out, 0, *echo_nonce);
            put_u64(out, 8, *role_nonce);
            out[16..HELLO_ACK_LEN].copy_from_slice(public);
            HELLO_ACK_LEN
        }
        Message::Input(i) => {
            put_u16(out, 0, i.seg);
            put_u32(out, 2, i.ack);
            put_u32(out, 6, i.base);
            match i.lat {
                Some(lat) => {
                    debug_assert!(lat.delay <= MAX_DELAY, "delay は 0..=9");
                    debug_assert!(lat.frame != u32::MAX, "0xFFFFFFFF は「切替なし」の印");
                    put_u32(out, 10, lat.frame);
                    out[14] = lat.delay;
                }
                None => {
                    put_u32(out, 10, u32::MAX);
                    out[14] = 0;
                }
            }
            // count が 32 を超えていても配列の外を読まない。debug_assert は release で消えるので、個数を詰めてその個数を書く
            // （受け手は正しく読める）。
            let n = (i.count as usize).min(MAX_WORDS);
            out[15] = n as u8;
            for k in 0..n {
                put_u16(out, 16 + 2 * k, i.words[k]);
            }
            16 + 2 * n
        }
        Message::Check(c) => {
            put_u16(out, 0, c.seg);
            put_u32(out, 2, c.frame);
            put_u16(out, 6, c.rng_state);
            put_u32(out, 8, c.rng_count);
            out[12] = c.life_1p;
            out[13] = c.life_2p;
            14
        }
        Message::Ping { send_time } | Message::Pong { send_time } => {
            put_u32(out, 0, *send_time);
            4
        }
        Message::Agree {
            host_seat,
            delay,
            mode,
        } => {
            debug_assert!(*delay <= MAX_DELAY, "delay は 0..=9");
            out[0] = host_seat.to_u8();
            out[1] = *delay;
            match mode {
                Mode::Delay => AGREE_DELAY_LEN,
                Mode::Rollback => {
                    out[2] = mode.to_u8();
                    AGREE_ROLLBACK_LEN
                }
            }
        }
        Message::Select(s) => {
            put_select(out, s);
            SELECT_LEN
        }
        Message::Bye(reason) => {
            out[0] = reason.to_u8();
            1
        }
        Message::Menu(m) => {
            put_u16(out, 0, m.step);
            put_u32(out, 2, m.ack);
            put_u32(out, 6, m.base);
            // INPUT と同じ理由で、個数は 32 で詰めてその個数を書く。
            let n = (m.count as usize).min(MAX_WORDS);
            out[10] = n as u8;
            for k in 0..n {
                put_u16(out, 11 + 2 * k, m.words[k]);
            }
            11 + 2 * n
        }
        Message::StepReq(s) | Message::Step(s) => {
            put_step(out, s);
            STEP_LEN
        }
        Message::SpectateJoin(j) => {
            debug_assert!((j.name_len as usize) <= NAME_MAX, "name_len は 0..=32");
            out[0] = j.name_len;
            out[1] = 0;
            put_u64(out, 2, j.nonce);
            out[10..JOIN_NAME_END].copy_from_slice(&j.name);
            put_u32(out, JOIN_NAME_END, j.build);
            out[JOIN_NAME_END + 4..JOIN_LEN].copy_from_slice(&j.public);
            JOIN_LEN
        }
        Message::SpectateStream(s) => {
            put_u32(out, 0, s.first);
            out[4] = u8::from(s.reset);
            // INPUT と同じ理由で、個数は上限で詰めてその個数を書く（配列の外を読まない）。
            let n = (s.count as usize).min(MAX_STREAM_RECORDS);
            out[5] = n as u8;
            let mut at = STREAM_HEAD_LEN;
            for rec in s.records.iter().take(n) {
                at += rec.write(&mut out[at..]);
            }
            at
        }
        Message::SpectateAck { nonce, next, sack } => {
            put_u64(out, 0, *nonce);
            put_u32(out, 8, *next);
            put_u64(out, 12, *sack);
            ACK_LEN
        }
        Message::Ready => 0,
        Message::HostState(h) => {
            debug_assert!(
                h.delay <= crate::delay_pick::bounds(h.mode).1,
                "delay は選べる範囲の中"
            );
            put_u16(out, 0, h.seq);
            out[2] = h.mode.to_u8();
            out[3] = h.delay;
            out[4] = u8::from(h.auto);
            HOST_STATE_LEN
        }
    }
}

/// type が HMAC のタグの便（鍵を交換する便: HELLO・HELLO_ACK・SPECTATE_JOIN）か。決まりはここ 1 か所
/// （`encode` / `seal` と `decode` が同じ関数を見る）。
pub fn is_signed_type(type_id: u8) -> bool {
    matches!(type_id, TYPE_HELLO | TYPE_HELLO_ACK | TYPE_SPECTATE_JOIN)
}

/// `msg` が HMAC のタグの便か（[`is_signed_type`] の `Message` を受ける形）。
pub fn is_signed(msg: &Message) -> bool {
    is_signed_type(message_type_id(msg))
}

fn put_header(out: &mut [u8; MAX_DATAGRAM], session_id: u32, type_id: u8) {
    out[0..4].copy_from_slice(&MAGIC);
    out[4] = PROTO_VER;
    out[5] = type_id;
    out[6] = 0; // flags（予約）
    out[7] = 0; // 予約
    out[8..12].copy_from_slice(&session_id.to_le_bytes());
}

/// 鍵を交換する便（[`is_signed_type`]）を組み立て、末尾に `key` のタグを付ける。`out` は `MAX_DATAGRAM` B の固定バッファ
/// （呼び出し側がスタックに持つ）。戻り値はタグ込みの長さ（`out[..戻り値]` を送る）。
///
/// 封じる便（AGREE・INPUT・PING など）は `seal` で作る。ここへ渡したら何も作らず 0 を返す（相手は HMAC のタグの INPUT を
/// 開けずに捨てるので、間違えて送ると黙って届かない。0 B なら呼び手の検査がすぐ落ちる）。
pub fn encode(
    key: &AuthKey,
    session_id: u32,
    msg: &Message,
    out: &mut [u8; MAX_DATAGRAM],
) -> usize {
    let type_id = message_type_id(msg);
    if !is_signed_type(type_id) {
        debug_assert!(false, "封じる便を HMAC の形で作ろうとした: type {type_id}");
        return 0;
    }
    put_header(out, session_id, type_id);
    let body_len = encode_body(msg, &mut out[HEADER_LEN..]);
    let n = HEADER_LEN + body_len;
    // 最長の HMAC の便（HELLO の 82 B）でも 12 + 82 + 16 = 110 B で、1200 B の枠を越えない。
    let tag = key.tag(&out[..n]);
    out[n..n + TAG_LEN].copy_from_slice(&tag);
    n + TAG_LEN
}

/// 封じる便（[`is_signed_type`] でないもの）を組み立てて封じる: ヘッダ 12 B ‖ nonce の欄 8 B ‖ 暗号化した本文 ‖ タグ 16 B。
/// ヘッダが追加の認証データ。通し番号は `seq` から 1 つ取る（送り手ごとに 1 つの `Seq`）。戻り値は全体の長さ。
/// HMAC の便を渡した・通し番号を使い切ったら None（何も送らない）。確保しない。
///
/// 最長は観戦の STREAM（16 記録 × 名前の記録 35 B ＝ 560 B ＋ 6 B）で 602 B、対戦は INPUT の 116 B。
/// どれも 1200 B の枠に収まる（`seal_fits_the_datagram_for_the_longest_bodies` が見る）。
pub fn seal(
    sealer: &Sealer,
    seq: &Seq,
    session_id: u32,
    msg: &Message,
    out: &mut [u8; MAX_DATAGRAM],
) -> Option<usize> {
    let type_id = message_type_id(msg);
    if is_signed_type(type_id) {
        return None;
    }
    put_header(out, session_id, type_id);
    let start = HEADER_LEN + NONCE_FIELD_LEN;
    let body_len = encode_body(msg, &mut out[start..]);
    let end = start + body_len;
    let (head, rest) = out.split_at_mut(HEADER_LEN);
    let (field_slot, body_and_tail) = rest.split_at_mut(NONCE_FIELD_LEN);
    let (body, tail) = body_and_tail.split_at_mut(body_len);
    let (field, tag) = sealer.seal(seq, head, body)?;
    field_slot.copy_from_slice(&field);
    tail.get_mut(..SEAL_TAG_LEN)?.copy_from_slice(&tag);
    Some(end + SEAL_TAG_LEN)
}

/// 受信ソケットに来た 1 データグラムの種類（受信ソケットで magic と STUN の cookie を見て分ける）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram {
    Hgrt,
    /// STUN（20 B 以上、先頭 2 bit が 0、offset 4 の 4 B が magic cookie `0x2112A442`（BE）。RFC 5389 §6）。
    Stun,
    Other,
}

pub fn classify(bytes: &[u8]) -> Datagram {
    if bytes.starts_with(&MAGIC) {
        return Datagram::Hgrt;
    }
    let cookie = crate::stun::MAGIC_COOKIE.to_be_bytes();
    match (bytes.first(), bytes.get(4..8)) {
        (Some(&b0), Some(c))
            if bytes.len() >= crate::stun::HEADER_LEN && b0 & 0xC0 == 0 && c == cookie =>
        {
            Datagram::Stun
        }
        _ => Datagram::Other,
    }
}

/// タグを比べる前に、ヘッダの session_id だけを覗く（別の接続の遅れたパケットを「別のセッション」として数え分けるため）。
/// 長さか magic が合わなければ None。
pub fn peek_session(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < HEADER_LEN + TAG_LEN || !bytes.starts_with(&MAGIC) {
        return None;
    }
    Some(get_u32(bytes, 8))
}

/// タグを比べる前に、ヘッダの type が PING かだけを覗く（受信スレッドの PONG の応答役 `pong.rs` の最初のガード）。PING でない 1 通に
/// タグの計算を足さないため。本物かどうかはタグを比べるまで分からない（`decode` の仕事）。
pub fn peek_is_ping(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN + TAG_LEN && bytes.starts_with(&MAGIC) && bytes[5] == TYPE_PING
}

/// タグを比べる前に、ヘッダの type が接続の手順の 3 つ（HELLO・HELLO_ACK・AGREE）のどれかだけを覗く。候補に無い送り元から
/// 来た 1 通を「学ぶ候補」にしてよいかの最初のガード（`Link::on_datagram_at`）。手順の外の型はタグの計算まで進めない。
/// 本物かどうかはタグを比べるまで分からない（`decode` の仕事）。
pub fn peek_is_handshake(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN + TAG_LEN
        && bytes.starts_with(&MAGIC)
        && matches!(bytes[5], TYPE_HELLO | TYPE_HELLO_ACK | TYPE_AGREE)
}

fn decode_hello(body: &[u8]) -> Result<Message, DecodeError> {
    const LEN: usize = HELLO_LEN;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_HELLO,
            expected: LEN,
            actual: body.len(),
        });
    }
    let proto_ver = body[0];
    // `PROTO_VER` 以外を捨てる。ヘッダのバージョンはもう合っているので、ここで落ちるのはヘッダと本文で違うバージョンを名乗る壊れた相手だけ。
    if proto_ver != PROTO_VER {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_HELLO,
            field: "proto_ver",
        });
    }
    let latency = body[1];
    if latency > MAX_DELAY {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_HELLO,
            field: "latency",
        });
    }
    let name_len = body[2];
    if name_len as usize > NAME_MAX {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_HELLO,
            field: "name_len",
        });
    }
    // +3: bit0 = 望むモード（0 = delay 方式）、bit1 = rollback ができる、bit2 = 一覧に載せてよい。ほかのビットは捨てる値。
    if body[3] & !(1 | HELLO_CAN_ROLLBACK_BIT | HELLO_LISTABLE_BIT) != 0 {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_HELLO,
            field: "mode",
        });
    }
    let mode = Mode::from_u8(body[3] & 1).ok_or(DecodeError::FieldOutOfRange {
        msg_type: TYPE_HELLO,
        field: "mode",
    })?;
    let can_rollback = body[3] & HELLO_CAN_ROLLBACK_BIT != 0;
    let listable = body[3] & HELLO_LISTABLE_BIT != 0;
    let role_nonce = get_u64(body, 4);
    let mut name = [0u8; NAME_MAX];
    name.copy_from_slice(&body[12..44]);
    if name[name_len as usize..].iter().any(|&b| b != 0) {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_HELLO,
            field: "name_tail",
        });
    }
    if std::str::from_utf8(&name).is_err() {
        return Err(DecodeError::InvalidUtf8);
    }
    let seat_wish = SeatWish::from_u8(body[44]).ok_or(DecodeError::FieldOutOfRange {
        msg_type: TYPE_HELLO,
        field: "seat_wish",
    })?;
    let arrival = Arrival::from_u8(body[45]).ok_or(DecodeError::FieldOutOfRange {
        msg_type: TYPE_HELLO,
        field: "arrival",
    })?;
    // +46・+50: ビルド番号と公開鍵。どちらも値の範囲を見ない（ビルドの比べと公開鍵の弱い点は手順と `seal.rs` の仕事）。
    let build = get_u32(body, 46);
    let mut public = [0u8; PUBLIC_LEN];
    public.copy_from_slice(&body[50..50 + PUBLIC_LEN]);
    Ok(Message::Hello(Hello {
        proto_ver,
        latency,
        name_len,
        mode,
        can_rollback,
        listable,
        role_nonce,
        name,
        seat_wish,
        arrival,
        build,
        public,
    }))
}

fn decode_hello_ack(body: &[u8]) -> Result<Message, DecodeError> {
    const LEN: usize = HELLO_ACK_LEN;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_HELLO_ACK,
            expected: LEN,
            actual: body.len(),
        });
    }
    let mut public = [0u8; PUBLIC_LEN];
    public.copy_from_slice(&body[16..LEN]);
    Ok(Message::HelloAck {
        echo_nonce: get_u64(body, 0),
        role_nonce: get_u64(body, 8),
        public,
    })
}

fn decode_input(body: &[u8]) -> Result<Message, DecodeError> {
    const MIN: usize = 16;
    if body.len() < MIN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_INPUT,
            expected: MIN,
            actual: body.len(),
        });
    }
    let seg = get_u16(body, 0);
    let ack = get_u32(body, 2);
    let base = get_u32(body, 6);
    let lat_frame = get_u32(body, 10);
    let lat_delay = body[14];
    let count = body[15];
    if count as usize > MAX_WORDS {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_INPUT,
            field: "count",
        });
    }
    let expected = MIN + 2 * count as usize;
    if body.len() != expected {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_INPUT,
            expected,
            actual: body.len(),
        });
    }
    let lat = if lat_frame == u32::MAX {
        if lat_delay != 0 {
            return Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_INPUT,
                field: "lat_delay",
            });
        }
        None
    } else {
        if lat_delay > MAX_DELAY {
            return Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_INPUT,
                field: "lat_delay",
            });
        }
        Some(LatChange {
            frame: lat_frame,
            delay: lat_delay,
        })
    };
    let mut words = [0u16; MAX_WORDS];
    for (k, w) in words.iter_mut().enumerate().take(count as usize) {
        *w = get_u16(body, 16 + 2 * k);
    }
    Ok(Message::Input(Input {
        seg,
        ack,
        base,
        lat,
        count,
        words,
    }))
}

fn decode_check(body: &[u8]) -> Result<Message, DecodeError> {
    const LEN: usize = 14;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_CHECK,
            expected: LEN,
            actual: body.len(),
        });
    }
    Ok(Message::Check(Check {
        seg: get_u16(body, 0),
        frame: get_u32(body, 2),
        rng_state: get_u16(body, 6),
        rng_count: get_u32(body, 8),
        life_1p: body[12],
        life_2p: body[13],
    }))
}

fn decode_ping(body: &[u8]) -> Result<Message, DecodeError> {
    const LEN: usize = 4;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_PING,
            expected: LEN,
            actual: body.len(),
        });
    }
    Ok(Message::Ping {
        send_time: get_u32(body, 0),
    })
}

fn decode_pong(body: &[u8]) -> Result<Message, DecodeError> {
    const LEN: usize = 4;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_PONG,
            expected: LEN,
            actual: body.len(),
        });
    }
    Ok(Message::Pong {
        send_time: get_u32(body, 0),
    })
}

/// AGREE の delay 方式の本文の長さ（rollback のモードは 3 B）。
const AGREE_DELAY_LEN: usize = 2;
const AGREE_ROLLBACK_LEN: usize = 3;

fn decode_agree(body: &[u8]) -> Result<Message, DecodeError> {
    // 長さの誤りは 2 B（いちばん短い形）を期待値として返す。
    let mode = match body.len() {
        AGREE_DELAY_LEN => Mode::Delay,
        AGREE_ROLLBACK_LEN => match Mode::from_u8(body[2]) {
            // delay 方式は 2 B の形だけ（書き方を 1 つに保つ）。3 B で 0 は、知らない値と同じに捨てる。
            Some(Mode::Rollback) => Mode::Rollback,
            Some(Mode::Delay) | None => {
                return Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_AGREE,
                    field: "mode",
                });
            }
        },
        n => {
            return Err(DecodeError::LengthMismatch {
                msg_type: TYPE_AGREE,
                expected: AGREE_DELAY_LEN,
                actual: n,
            });
        }
    };
    let host_seat = Seat::from_u8(body[0]).ok_or(DecodeError::FieldOutOfRange {
        msg_type: TYPE_AGREE,
        field: "host_seat",
    })?;
    let delay = body[1];
    if delay > MAX_DELAY {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_AGREE,
            field: "delay",
        });
    }
    Ok(Message::Agree {
        host_seat,
        delay,
        mode,
    })
}

/// SELECT の本文（18 B ちょうど）を読む。SELECT 本体（`msg_type` = 11）と観戦の記録が同じ配置・同じガードを使う。
/// 誤りの `msg_type` は呼び手の type。
fn parse_select(msg_type: u8, body: &[u8]) -> Result<Select, DecodeError> {
    if body.len() != SELECT_LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type,
            expected: SELECT_LEN,
            actual: body.len(),
        });
    }
    if body[11] != 0 {
        return Err(DecodeError::FieldOutOfRange {
            msg_type,
            field: "reserved",
        });
    }
    Ok(Select {
        match_no: get_u16(body, 0),
        chara: body[2],
        color: body[3],
        life: body[4],
        charge: body[5],
        map: body[6],
        difficulty: body[7],
        cpu_1p: body[8],
        cpu_2p: body[9],
        background: body[10],
        rng_seed: get_u16(body, 12),
        option_digest: get_u32(body, 14),
    })
}

fn decode_select(body: &[u8]) -> Result<Message, DecodeError> {
    parse_select(TYPE_SELECT, body).map(Message::Select)
}

fn decode_spectate_join(body: &[u8]) -> Result<Message, DecodeError> {
    const T: u8 = TYPE_SPECTATE_JOIN;
    if body.len() != JOIN_LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: T,
            expected: JOIN_LEN,
            actual: body.len(),
        });
    }
    let name_len = body[0];
    if name_len as usize > NAME_MAX {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: T,
            field: "name_len",
        });
    }
    if body[1] != 0 {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: T,
            field: "reserved",
        });
    }
    let nonce = get_u64(body, 2);
    let mut name = [0u8; NAME_MAX];
    name.copy_from_slice(&body[10..JOIN_NAME_END]);
    if name[name_len as usize..].iter().any(|&b| b != 0) {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: T,
            field: "name_tail",
        });
    }
    if std::str::from_utf8(&name).is_err() {
        return Err(DecodeError::InvalidUtf8);
    }
    let build = get_u32(body, JOIN_NAME_END);
    let mut public = [0u8; PUBLIC_LEN];
    public.copy_from_slice(&body[JOIN_NAME_END + 4..JOIN_LEN]);
    Ok(Message::SpectateJoin(SpectateJoin {
        name_len,
        nonce,
        name,
        build,
        public,
    }))
}

fn decode_spectate_stream(body: &[u8]) -> Result<Message, DecodeError> {
    const T: u8 = TYPE_SPECTATE_STREAM;
    if body.len() < STREAM_HEAD_LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: T,
            expected: STREAM_HEAD_LEN,
            actual: body.len(),
        });
    }
    let first = get_u32(body, 0);
    let flags = body[4];
    let count = body[5];
    if flags & !STREAM_FLAG_RESET != 0 {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: T,
            field: "flags",
        });
    }
    if count as usize > MAX_STREAM_RECORDS {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: T,
            field: "count",
        });
    }
    let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
    let mut at = STREAM_HEAD_LEN;
    for slot in records.iter_mut().take(count as usize) {
        let (rec, n) = Record::read(body.get(at..).unwrap_or(&[]))?;
        *slot = rec;
        at += n;
    }
    if at != body.len() {
        return Err(DecodeError::LengthMismatch {
            msg_type: T,
            expected: at,
            actual: body.len(),
        });
    }
    Ok(Message::SpectateStream(SpectateStream {
        first,
        reset: flags & STREAM_FLAG_RESET != 0,
        count,
        records,
    }))
}

fn decode_spectate_ack(body: &[u8]) -> Result<Message, DecodeError> {
    if body.len() != ACK_LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_SPECTATE_ACK,
            expected: ACK_LEN,
            actual: body.len(),
        });
    }
    let sack = get_u64(body, 12);
    // 最上位のビットは予約（番号 `next + 64` は、ウィンドウ（`next` を含めて 64）の外）。
    if sack & !SACK_MASK != 0 {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_SPECTATE_ACK,
            field: "sack",
        });
    }
    Ok(Message::SpectateAck {
        nonce: get_u64(body, 0),
        next: get_u32(body, 8),
        sack,
    })
}

/// HOST_STATE の本文の長さ（seq 2 + mode 1 + delay 1 + auto 1）。
const HOST_STATE_LEN: usize = 5;

fn decode_host_state(body: &[u8]) -> Result<Message, DecodeError> {
    if body.len() != HOST_STATE_LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_HOST_STATE,
            expected: HOST_STATE_LEN,
            actual: body.len(),
        });
    }
    let mode = Mode::from_u8(body[2]).ok_or(DecodeError::FieldOutOfRange {
        msg_type: TYPE_HOST_STATE,
        field: "mode",
    })?;
    // 上限は選べる範囲（`delay_pick::bounds`）。auto の値 9 はここに載らない（auto は別の欄）。
    let delay = body[3];
    if delay > crate::delay_pick::bounds(mode).1 {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_HOST_STATE,
            field: "delay",
        });
    }
    let auto = match body[4] {
        0 => false,
        1 => true,
        _ => {
            return Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_HOST_STATE,
                field: "auto",
            });
        }
    };
    Ok(Message::HostState(HostState {
        seq: get_u16(body, 0),
        mode,
        delay,
        auto,
    }))
}

fn decode_ready(body: &[u8]) -> Result<Message, DecodeError> {
    // 本文は持たない（0 B）。中身を足すときは長さで分ける。
    if !body.is_empty() {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_READY,
            expected: 0,
            actual: body.len(),
        });
    }
    Ok(Message::Ready)
}

fn decode_bye(body: &[u8]) -> Result<Message, DecodeError> {
    const LEN: usize = 1;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_BYE,
            expected: LEN,
            actual: body.len(),
        });
    }
    let reason = ByeReason::from_u8(body[0]).ok_or(DecodeError::FieldOutOfRange {
        msg_type: TYPE_BYE,
        field: "reason",
    })?;
    Ok(Message::Bye(reason))
}

fn decode_menu(body: &[u8]) -> Result<Message, DecodeError> {
    const MIN: usize = 11;
    if body.len() < MIN {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_MENU,
            expected: MIN,
            actual: body.len(),
        });
    }
    let step = get_u16(body, 0);
    let ack = get_u32(body, 2);
    let base = get_u32(body, 6);
    let count = body[10];
    if count as usize > MAX_WORDS {
        return Err(DecodeError::FieldOutOfRange {
            msg_type: TYPE_MENU,
            field: "count",
        });
    }
    let expected = MIN + 2 * count as usize;
    if body.len() != expected {
        return Err(DecodeError::LengthMismatch {
            msg_type: TYPE_MENU,
            expected,
            actual: body.len(),
        });
    }
    let mut words = [0u16; MAX_WORDS];
    for (k, w) in words.iter_mut().enumerate().take(count as usize) {
        *w = get_u16(body, 11 + 2 * k);
    }
    Ok(Message::Menu(Menu {
        step,
        ack,
        base,
        count,
        words,
    }))
}

fn decode_step_body(msg_type: u8, body: &[u8]) -> Result<Step, DecodeError> {
    const LEN: usize = 5;
    if body.len() != LEN {
        return Err(DecodeError::LengthMismatch {
            msg_type,
            expected: LEN,
            actual: body.len(),
        });
    }
    let step = get_u16(body, 0);
    let kind = StepKind::from_u8(body[2]).ok_or(DecodeError::FieldOutOfRange {
        msg_type,
        field: "kind",
    })?;
    // 合わせを出すのはホストだけ（ゲストの STEP_REQ に載っていたら手順の外）。
    if msg_type == TYPE_STEP_REQ && kind == StepKind::Align {
        return Err(DecodeError::FieldOutOfRange {
            msg_type,
            field: "kind",
        });
    }
    let value = body[3];
    let screen = body[4];
    if kind == StepKind::Format {
        // 形式の切替: STEP は完全な形（`FormatValue::unpack`）、STEP_REQ は要求（形式だけ）。観戦の流れには載らない。
        // value の中身が範囲の外なら STEP ごと捨てる。
        let ok = match msg_type {
            TYPE_STEP => FormatValue::unpack(value).is_some(),
            TYPE_STEP_REQ => FormatValue::is_request(value),
            _ => false,
        };
        if !ok {
            return Err(DecodeError::FieldOutOfRange {
                msg_type,
                field: if msg_type == TYPE_SPECTATE_STREAM {
                    "kind"
                } else {
                    "value"
                },
            });
        }
    }
    Ok(Step {
        step,
        kind,
        value,
        screen,
    })
}

/// パケットを解く。`bytes` は受信した UDP データグラムそのもの（ヘッダ・タグ込み）。`open` は相手から自分への向きの `Opener`
/// （鍵の交換の後。まだなら None）。
///
/// 長さと magic を見た後、本文を読む前に認証する（順はモジュール doc）。捨てる理由は `DecodeError` に分けて返す
/// （ログに残すのは呼び出し側の仕事）。確保しない。
pub fn decode(key: &AuthKey, open: Option<&Opener>, bytes: &[u8]) -> Result<Packet, DecodeError> {
    if bytes.len() < HEADER_LEN + TAG_LEN {
        return Err(DecodeError::TooShort { got: bytes.len() });
    }
    if !bytes.starts_with(&MAGIC) {
        return Err(DecodeError::BadMagic);
    }
    let ver = bytes[4];
    let type_id = bytes[5];
    // バージョンの違う相手は封じる鍵を持たない。type に依らず HMAC のタグで確かめ、合えばバージョンで断る。合わない（秘密を知らない
    // 第三者がバージョンを変えた）ものは `BadTag` で、手順を止めさせない。
    if ver != PROTO_VER || is_signed_type(type_id) {
        let (signed, tag) = bytes.split_at(bytes.len() - TAG_LEN);
        if !key.verify(signed, tag) {
            return Err(DecodeError::BadTag);
        }
        return decode_signed(signed);
    }
    let Some(open) = open else {
        return Err(DecodeError::NotKeyed);
    };
    if bytes.len() < HEADER_LEN + SEAL_OVERHEAD {
        return Err(DecodeError::TooShort { got: bytes.len() });
    }
    let body_len = bytes.len() - HEADER_LEN - SEAL_OVERHEAD;
    let mut plain = [0u8; MAX_DATAGRAM];
    let (head, rest) = bytes.split_at(HEADER_LEN);
    let (field, rest) = rest.split_at(NONCE_FIELD_LEN);
    let (cipher, tag) = rest.split_at(body_len);
    let (Ok(field), Ok(tag), Some(work)) = (
        <&[u8; NONCE_FIELD_LEN]>::try_from(field),
        <&[u8; SEAL_TAG_LEN]>::try_from(tag),
        plain.get_mut(HEADER_LEN..HEADER_LEN + body_len),
    ) else {
        // 1200 B を越える便と、形の合わないもの。
        return Err(DecodeError::TooShort { got: bytes.len() });
    };
    work.copy_from_slice(cipher);
    if !open.open(head, field, work, tag) {
        return Err(DecodeError::BadTag);
    }
    plain[..HEADER_LEN].copy_from_slice(head);
    decode_signed(&plain[..HEADER_LEN + body_len])
}

/// タグを外した後（ヘッダ ‖ 本文）を解く。長さ（12 B 以上）と magic は `decode` が見ている。
fn decode_signed(bytes: &[u8]) -> Result<Packet, DecodeError> {
    if bytes.len() < HEADER_LEN {
        return Err(DecodeError::TooShort { got: bytes.len() });
    }
    if !bytes.starts_with(&MAGIC) {
        return Err(DecodeError::BadMagic);
    }
    let proto_ver = bytes[4];
    if proto_ver != PROTO_VER {
        return Err(DecodeError::BadVersion(proto_ver));
    }
    let type_id = bytes[5];
    let flags = bytes[6];
    let reserved = bytes[7];
    if flags != 0 || reserved != 0 {
        return Err(DecodeError::HeaderReserved);
    }
    let session_id = get_u32(bytes, 8);
    let body = &bytes[HEADER_LEN..];
    let msg = match type_id {
        TYPE_HELLO => decode_hello(body)?,
        TYPE_HELLO_ACK => decode_hello_ack(body)?,
        TYPE_INPUT => decode_input(body)?,
        TYPE_CHECK => decode_check(body)?,
        TYPE_PING => decode_ping(body)?,
        TYPE_PONG => decode_pong(body)?,
        TYPE_AGREE => decode_agree(body)?,
        TYPE_SELECT => decode_select(body)?,
        TYPE_SPECTATE_JOIN => decode_spectate_join(body)?,
        TYPE_SPECTATE_STREAM => decode_spectate_stream(body)?,
        TYPE_BYE => decode_bye(body)?,
        TYPE_MENU => decode_menu(body)?,
        TYPE_STEP_REQ => Message::StepReq(decode_step_body(TYPE_STEP_REQ, body)?),
        TYPE_STEP => Message::Step(decode_step_body(TYPE_STEP, body)?),
        TYPE_SPECTATE_ACK => decode_spectate_ack(body)?,
        TYPE_READY => decode_ready(body)?,
        TYPE_HOST_STATE => decode_host_state(body)?,
        other => return Err(DecodeError::UnknownType(other)),
    };
    Ok(Packet { session_id, msg })
}

#[cfg(test)]
mod tests {
    //! golden packet（手で書いたバイト列との一致）→ 毒標本（1 か所だけ壊して、そのガードだけが落とすことを示す）→
    //! 往復とでたらめの大量件数。封じる便の golden は平文の形（ヘッダ ‖ 本文）で書く: `encode_to_vec` は封じた便を開いて
    //! 平文に戻し、`dec` は封じ直してから `decode` へ渡す。毒標本は壊した後に認証を付け直して渡す（そうしないと、どの毒も
    //! 認証のガードで落ち、後ろのガードが素通りでも気づけない）。
    use super::*;
    use crate::auth::AuthKey;
    use crate::seal::{DirKeys, Opener, Sealer, Sender, Seq};

    const SID: u32 = 0x1122_3344; // 全 golden 共通の session_id（LE: 44 33 22 11）

    fn key() -> AuthKey {
        AuthKey::derive(&crate::auth::tests::golden_secret(), SID)
    }

    fn dir() -> DirKeys {
        DirKeys::from_raw([0x5A; 32], [0x5A; 32])
    }

    fn sealer() -> Sealer {
        dir().sealer(Sender::Link)
    }

    fn opener() -> Opener {
        dir().opener()
    }

    fn name_bytes(s: &[u8]) -> [u8; NAME_MAX] {
        let mut out = [0u8; NAME_MAX];
        out[..s.len()].copy_from_slice(s);
        out
    }

    fn enc(session_id: u32, msg: &Message, buf: &mut [u8; MAX_DATAGRAM]) -> usize {
        if is_signed(msg) {
            encode(&key(), session_id, msg, buf)
        } else {
            seal(&sealer(), &Seq::new(), session_id, msg, buf).expect("封じられる")
        }
    }

    /// 便を作り、平文の形（ヘッダ ‖ 本文）を返す。
    fn encode_to_vec(session_id: u32, msg: &Message) -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let len = enc(session_id, msg, &mut buf);
        if is_signed(msg) {
            let (body, tag) = buf[..len].split_at(len - TAG_LEN);
            assert_eq!(
                tag,
                key().tag(body),
                "encode のタグが後ろの 16 B を除いた所のタグでない"
            );
            return body.to_vec();
        }
        unseal(&buf[..len])
    }

    fn unseal(bytes: &[u8]) -> Vec<u8> {
        let n = bytes.len() - crate::seal::SEAL_OVERHEAD;
        let mut body = bytes
            [HEADER_LEN + NONCE_FIELD_LEN..HEADER_LEN + NONCE_FIELD_LEN + n - HEADER_LEN]
            .to_vec();
        let field: [u8; NONCE_FIELD_LEN] = bytes[HEADER_LEN..HEADER_LEN + NONCE_FIELD_LEN]
            .try_into()
            .unwrap();
        let tag: [u8; SEAL_TAG_LEN] = bytes[bytes.len() - SEAL_TAG_LEN..].try_into().unwrap();
        assert!(
            opener().open(&bytes[..HEADER_LEN], &field, &mut body, &tag),
            "封じた便が開かない"
        );
        [&bytes[..HEADER_LEN], &body[..]].concat()
    }

    fn signed(bytes: &[u8]) -> Vec<u8> {
        let mut v = bytes.to_vec();
        v.extend_from_slice(&key().tag(bytes));
        v
    }

    fn sealed(bytes: &[u8]) -> Vec<u8> {
        let mut body = bytes[HEADER_LEN..].to_vec();
        let (field, tag) = sealer()
            .seal(&Seq::new(), &bytes[..HEADER_LEN], &mut body)
            .unwrap();
        [&bytes[..HEADER_LEN], &field[..], &body[..], &tag[..]].concat()
    }

    fn wrapped(bytes: &[u8]) -> Vec<u8> {
        if bytes.len() < HEADER_LEN || bytes[4] != PROTO_VER || is_signed_type(bytes[5]) {
            signed(bytes)
        } else {
            sealed(bytes)
        }
    }

    /// 認証を付け直してから解く（毒標本が、狙ったガードまで届くように）。
    fn dec(bytes: &[u8]) -> Result<Packet, DecodeError> {
        decode(&key(), Some(&opener()), &wrapped(bytes))
    }

    fn dec_raw(bytes: &[u8]) -> Result<Packet, DecodeError> {
        decode(&key(), Some(&opener()), bytes)
    }

    // ---------- golden packet ----------

    #[rustfmt::skip]
    const HELLO_GOLDEN: [u8; HEADER_LEN + HELLO_LEN] = [
        // ヘッダ 12B
        0x48, 0x47, 0x52, 0x54, // magic "HGRT"
        0x07,                   // proto_ver = 7（`PROTO_VER`）
        0x01,                   // type = 1 (HELLO)
        0x00,                   // flags（予約。0）
        0x00,                   // 予約
        0x44, 0x33, 0x22, 0x11, // session_id = 0x11223344
        // 本文 82B
        0x07,                   // 本文の proto_ver（ヘッダと同じ 7 だけ通る）
        0x03,                   // latency（希望。3）
        0x09,                   // name_len = 9（「テスト」の UTF-8 バイト数）
        0x00,                   // mode = 0（delay 方式。M8-2a の前は予約で、古いバージョンの HELLO もこの形）
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // role_nonce = 0x0102030405060708
        0xE3, 0x83, 0x86, 0xE3, 0x82, 0xB9, 0xE3, 0x83, 0x88, // name: "テスト" の UTF-8（9B）
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // name 残り 23B は 0（1/2）
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,                   // name 残り 23B は 0（2/2）
        0x02,                   // seat_wish = 2（2P）。+44（バージョン 4）
        0x01,                   // arrival = 1（先）。+45（バージョン 4）
        0xD0, 0x29, 0x06, 0x00, // build = 403920（0x000629D0。2026-10-08 12:00 UTC の分の数）。+46（バージョン 5）
        // 公開鍵 32 B（+50。バージョン 5）: A0 A1 … BF
        0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
        0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD, 0xBE, 0xBF,
    ];

    /// 公開鍵の標本（A0 A1 … BF。順に並べて、欄のずれ・逆順が値で見えるように）。
    fn public_sample(first: u8) -> [u8; PUBLIC_LEN] {
        let mut p = [0u8; PUBLIC_LEN];
        for (i, b) in p.iter_mut().enumerate() {
            *b = first.wrapping_add(i as u8);
        }
        p
    }

    fn hello_golden_msg() -> Message {
        Message::Hello(Hello {
            proto_ver: 7,
            latency: 3,
            name_len: 9,
            mode: Mode::Delay,
            can_rollback: false,
            listable: false,
            role_nonce: 0x0102_0304_0506_0708,
            name: name_bytes(&[0xE3, 0x83, 0x86, 0xE3, 0x82, 0xB9, 0xE3, 0x83, 0x88]),
            seat_wish: SeatWish::P2,
            arrival: Arrival::First,
            build: 403_920,
            public: public_sample(0xA0),
        })
    }

    #[test]
    fn golden_hello() {
        assert_eq!(HELLO_GOLDEN[HEADER_LEN + 44], 2);
        assert_eq!(HELLO_GOLDEN[HEADER_LEN + 45], 1);
        assert_eq!(HELLO_GOLDEN[HEADER_LEN + 50], 0xA0);
        assert_eq!(HELLO_GOLDEN.len(), HEADER_LEN + 82);
        assert_eq!(encode_to_vec(SID, &hello_golden_msg()), HELLO_GOLDEN);
        let p = dec(&HELLO_GOLDEN).unwrap();
        assert_eq!(p.session_id, SID);
        assert_eq!(p.msg, hello_golden_msg());
    }

    #[test]
    fn golden_hello_wishing_rollback() {
        let mut want = HELLO_GOLDEN;
        want[HEADER_LEN + 3] = 0x01;
        let Message::Hello(mut h) = hello_golden_msg() else {
            unreachable!("golden は HELLO");
        };
        h.mode = Mode::Rollback;
        assert_eq!(encode_to_vec(SID, &Message::Hello(h)), want);
        assert_eq!(dec(&want).unwrap().msg, Message::Hello(h));
        let diff: Vec<usize> = (0..want.len())
            .filter(|&i| want[i] != HELLO_GOLDEN[i])
            .collect();
        assert_eq!(diff, vec![HEADER_LEN + 3]);
    }

    #[rustfmt::skip]
    const HELLO_ACK_GOLDEN: [u8; HEADER_LEN + HELLO_ACK_LEN] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x02, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        // 本文
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // echo_nonce = 0x1122334455667788
        0x00, 0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, // role_nonce = 0x99AABBCCDDEEFF00
        // 公開鍵 32 B（+16。バージョン 5）: 10 11 … 2F
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
        0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F,
    ];

    fn hello_ack_golden_msg() -> Message {
        Message::HelloAck {
            echo_nonce: 0x1122_3344_5566_7788,
            role_nonce: 0x99AA_BBCC_DDEE_FF00,
            public: public_sample(0x10),
        }
    }

    #[test]
    fn golden_hello_ack() {
        assert_eq!(
            encode_to_vec(SID, &hello_ack_golden_msg()),
            HELLO_ACK_GOLDEN
        );
        let p = dec(&HELLO_ACK_GOLDEN).unwrap();
        assert_eq!(p.msg, hello_ack_golden_msg());
    }

    #[rustfmt::skip]
    const INPUT_COUNT0_GOLDEN: [u8; HEADER_LEN + 16] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x03, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        // 本文（count = 0。語なし）
        0x01, 0x00,             // seg = 1
        0x10, 0x00, 0x00, 0x00, // ack = 16
        0x05, 0x00, 0x00, 0x00, // base = 5
        0xFF, 0xFF, 0xFF, 0xFF, // lat_frame = 切替なし
        0x00,                   // lat_delay = 0（切替なし）
        0x00,                   // count = 0
    ];

    fn input_count0_golden_msg() -> Message {
        Message::Input(Input {
            seg: 1,
            ack: 16,
            base: 5,
            lat: None,
            count: 0,
            words: [0u16; MAX_WORDS],
        })
    }

    #[test]
    fn golden_input_count0() {
        assert_eq!(
            encode_to_vec(SID, &input_count0_golden_msg()),
            INPUT_COUNT0_GOLDEN
        );
        let p = dec(&INPUT_COUNT0_GOLDEN).unwrap();
        assert_eq!(p.msg, input_count0_golden_msg());
    }

    #[rustfmt::skip]
    const INPUT_COUNT3_GOLDEN: [u8; HEADER_LEN + 22] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x03, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        // 本文（count = 3。lat なし）
        0x02, 0x00,             // seg = 2
        0x20, 0x00, 0x00, 0x00, // ack = 32
        0x06, 0x00, 0x00, 0x00, // base = 6
        0xFF, 0xFF, 0xFF, 0xFF, // lat_frame = 切替なし
        0x00,                   // lat_delay = 0
        0x03,                   // count = 3
        0x01, 0x00, 0x02, 0x00, 0x04, 0x00, // 入力語 × 3
    ];

    fn input_count3_golden_msg() -> Message {
        let mut words = [0u16; MAX_WORDS];
        words[0] = 1;
        words[1] = 2;
        words[2] = 4;
        Message::Input(Input {
            seg: 2,
            ack: 32,
            base: 6,
            lat: None,
            count: 3,
            words,
        })
    }

    #[test]
    fn golden_input_count3_no_lat() {
        assert_eq!(
            encode_to_vec(SID, &input_count3_golden_msg()),
            INPUT_COUNT3_GOLDEN
        );
        let p = dec(&INPUT_COUNT3_GOLDEN).unwrap();
        assert_eq!(p.msg, input_count3_golden_msg());
    }

    #[rustfmt::skip]
    const INPUT_COUNT2_LAT_GOLDEN: [u8; HEADER_LEN + 20] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x03, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        // 本文（count = 2。lat あり）
        0x03, 0x00,             // seg = 3
        0x30, 0x00, 0x00, 0x00, // ack = 48
        0x07, 0x00, 0x00, 0x00, // base = 7
        0x64, 0x00, 0x00, 0x00, // lat_frame = 100
        0x05,                   // lat_delay = 5
        0x02,                   // count = 2
        0x08, 0x00, 0x10, 0x00, // 入力語 × 2
    ];

    fn input_count2_lat_golden_msg() -> Message {
        let mut words = [0u16; MAX_WORDS];
        words[0] = 8;
        words[1] = 0x10;
        Message::Input(Input {
            seg: 3,
            ack: 48,
            base: 7,
            lat: Some(LatChange {
                frame: 100,
                delay: 5,
            }),
            count: 2,
            words,
        })
    }

    #[test]
    fn golden_input_count2_with_lat() {
        assert_eq!(
            encode_to_vec(SID, &input_count2_lat_golden_msg()),
            INPUT_COUNT2_LAT_GOLDEN
        );
        let p = dec(&INPUT_COUNT2_LAT_GOLDEN).unwrap();
        assert_eq!(p.msg, input_count2_lat_golden_msg());
    }

    #[rustfmt::skip]
    const CHECK_GOLDEN: [u8; HEADER_LEN + 14] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x04, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x04, 0x00,             // seg = 4
        0x64, 0x00, 0x00, 0x00, // frame = 100
        0x34, 0x12,             // rng_state = 0x1234
        0x78, 0x56, 0x00, 0x00, // rng_count = 0x5678
        0x08,                   // life_1p = 8
        0x07,                   // life_2p = 7
    ];

    fn check_golden_msg() -> Message {
        Message::Check(Check {
            seg: 4,
            frame: 100,
            rng_state: 0x1234,
            rng_count: 0x5678,
            life_1p: 8,
            life_2p: 7,
        })
    }

    #[test]
    fn golden_check() {
        assert_eq!(encode_to_vec(SID, &check_golden_msg()), CHECK_GOLDEN);
        let p = dec(&CHECK_GOLDEN).unwrap();
        assert_eq!(p.msg, check_golden_msg());
    }

    #[rustfmt::skip]
    const PING_GOLDEN: [u8; HEADER_LEN + 4] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x08, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x60, 0xEA, 0x00, 0x00, // send_time = 60000
    ];

    #[test]
    fn golden_ping() {
        let msg = Message::Ping { send_time: 60_000 };
        assert_eq!(encode_to_vec(SID, &msg), PING_GOLDEN);
        let p = dec(&PING_GOLDEN).unwrap();
        assert_eq!(p.msg, msg);
    }

    #[rustfmt::skip]
    const PONG_GOLDEN: [u8; HEADER_LEN + 4] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x09, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x61, 0xEA, 0x00, 0x00, // send_time = 60001
    ];

    #[test]
    fn golden_pong() {
        let msg = Message::Pong { send_time: 60_001 };
        assert_eq!(encode_to_vec(SID, &msg), PONG_GOLDEN);
        let p = dec(&PONG_GOLDEN).unwrap();
        assert_eq!(p.msg, msg);
    }

    #[rustfmt::skip]
    const AGREE_GOLDEN: [u8; HEADER_LEN + 2] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x0A, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x01, // host_seat = 1 (P2)
        0x04, // delay = 4
    ];

    #[test]
    fn golden_agree() {
        let msg = Message::Agree {
            host_seat: Seat::P2,
            delay: 4,
            mode: Mode::Delay,
        };
        assert_eq!(encode_to_vec(SID, &msg), AGREE_GOLDEN);
        let p = dec(&AGREE_GOLDEN).unwrap();
        assert_eq!(p.msg, msg);
    }

    #[rustfmt::skip]
    const AGREE_ROLLBACK_GOLDEN: [u8; HEADER_LEN + 3] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x0A, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x00, // host_seat = 0 (P1)
        0x02, // delay = D = 2
        0x01, // mode = 1（rollback）
    ];

    #[test]
    fn golden_agree_rollback() {
        let msg = Message::Agree {
            host_seat: Seat::P1,
            delay: 2,
            mode: Mode::Rollback,
        };
        assert_eq!(encode_to_vec(SID, &msg), AGREE_ROLLBACK_GOLDEN);
        assert_eq!(dec(&AGREE_ROLLBACK_GOLDEN).unwrap().msg, msg);
    }

    #[rustfmt::skip]
    const SELECT_GOLDEN: [u8; HEADER_LEN + 18] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x0B, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x02, 0x00, // match_no = 2
        0x03,       // chara
        0x01,       // color
        0x02,       // life
        0x01,       // charge
        0x05,       // map
        0x02,       // difficulty
        0x00,       // cpu_1p
        0x00,       // cpu_2p
        0x01,       // background
        0x00,       // 予約
        0xEF, 0xBE, // rng_seed = 0xBEEF
        0x0D, 0x0C, 0x0B, 0x0A, // option_digest = 0x0A0B0C0D
    ];

    fn select_golden_msg() -> Message {
        Message::Select(Select {
            match_no: 2,
            chara: 3,
            color: 1,
            life: 2,
            charge: 1,
            map: 5,
            difficulty: 2,
            cpu_1p: 0,
            cpu_2p: 0,
            background: 1,
            rng_seed: 0xBEEF,
            option_digest: 0x0A0B_0C0D,
        })
    }

    #[test]
    fn golden_select() {
        assert_eq!(encode_to_vec(SID, &select_golden_msg()), SELECT_GOLDEN);
        let p = dec(&SELECT_GOLDEN).unwrap();
        assert_eq!(p.msg, select_golden_msg());
    }

    #[rustfmt::skip]
    const BYE_GOLDEN: [u8; HEADER_LEN + 1] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x0F, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x03, // reason = 3（ズレ検出）
    ];

    #[test]
    fn golden_bye() {
        let msg = Message::Bye(ByeReason::Desync);
        assert_eq!(encode_to_vec(SID, &msg), BYE_GOLDEN);
        let p = dec(&BYE_GOLDEN).unwrap();
        assert_eq!(p.msg, msg);
    }

    /// READY（type 20。本文 0 B）。
    #[rustfmt::skip]
    const READY_GOLDEN: [u8; HEADER_LEN] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x14, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
    ];

    #[test]
    fn golden_ready() {
        assert_eq!(encode_to_vec(SID, &Message::Ready), READY_GOLDEN);
        assert_eq!(dec(&READY_GOLDEN).unwrap().msg, Message::Ready);
    }

    #[test]
    fn poison_ready_with_a_body() {
        let mut bytes = READY_GOLDEN.to_vec();
        bytes.push(0);
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::LengthMismatch {
                msg_type: TYPE_READY,
                expected: 0,
                actual: 1,
            })
        );
    }

    /// HOST_STATE（type 21。本文 5 B）。seq = 0x0102・mode = rollback（1）・delay = 8・auto = 1。
    #[rustfmt::skip]
    const HOST_STATE_GOLDEN: [u8; HEADER_LEN + 5] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x15, 0x00, 0x00, // type = 21
        0x44, 0x33, 0x22, 0x11,
        0x02, 0x01,             // seq = 0x0102（LE）
        0x01,                   // mode = 1（rollback）
        0x08,                   // delay = 8（選べる範囲の上限）
        0x01,                   // auto = 1
    ];

    fn host_state_golden_msg() -> Message {
        Message::HostState(HostState {
            seq: 0x0102,
            mode: Mode::Rollback,
            delay: 8,
            auto: true,
        })
    }

    #[test]
    fn golden_host_state() {
        assert_eq!(
            encode_to_vec(SID, &host_state_golden_msg()),
            HOST_STATE_GOLDEN
        );
        assert_eq!(
            dec(&HOST_STATE_GOLDEN).unwrap().msg,
            host_state_golden_msg()
        );
        // auto = 0・delay = 0 の側（0 を「無い」と読む取り違えを縛る）。
        let mut zero = HOST_STATE_GOLDEN;
        zero[HEADER_LEN + 3] = 0;
        zero[HEADER_LEN + 4] = 0;
        let Message::HostState(mut h) = host_state_golden_msg() else {
            unreachable!("golden は HOST_STATE");
        };
        (h.delay, h.auto) = (0, false);
        assert_eq!(encode_to_vec(SID, &Message::HostState(h)), zero);
        assert_eq!(dec(&zero).unwrap().msg, Message::HostState(h));
    }

    #[test]
    fn poison_host_state_out_of_range() {
        for (off, v, field) in [
            (3usize, 9u8, "delay"),
            (3, 255, "delay"),
            (4, 2, "auto"),
            (4, 255, "auto"),
            (2, 2, "mode"),
            (2, 255, "mode"),
        ] {
            let mut bytes = HOST_STATE_GOLDEN;
            bytes[HEADER_LEN + off] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_HOST_STATE,
                    field,
                }),
                "{field} = {v}"
            );
        }
        for v in 0u8..=8 {
            let mut bytes = HOST_STATE_GOLDEN;
            bytes[HEADER_LEN + 3] = v;
            assert!(dec(&bytes).is_ok(), "delay = {v}");
        }
        for n in [0usize, 4, 6] {
            let mut bytes = HOST_STATE_GOLDEN[..HEADER_LEN].to_vec();
            bytes.extend(std::iter::repeat_n(0u8, n));
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::LengthMismatch {
                    msg_type: TYPE_HOST_STATE,
                    expected: 5,
                    actual: n,
                }),
                "本文 {n} B"
            );
        }
    }

    #[rustfmt::skip]
    const MENU_GOLDEN: [u8; HEADER_LEN + 15] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x10, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        // 本文（count = 2）
        0x05, 0x00,             // step = 5
        0x09, 0x00, 0x00, 0x00, // ack = 9
        0x02, 0x00, 0x00, 0x00, // base = 2
        0x02,                   // count = 2
        0x00, 0x01,             // 語 0 = 0x0100（決定キーの印。そのまま送る）
        0x02, 0x00,             // 語 1 = 0x0002
    ];

    fn menu_golden_msg() -> Message {
        let mut words = [0u16; MAX_WORDS];
        words[0] = 0x0100;
        words[1] = 0x0002;
        Message::Menu(Menu {
            step: 5,
            ack: 9,
            base: 2,
            count: 2,
            words,
        })
    }

    #[test]
    fn golden_menu_count2() {
        assert_eq!(encode_to_vec(SID, &menu_golden_msg()), MENU_GOLDEN);
        let p = dec(&MENU_GOLDEN).unwrap();
        assert_eq!(p.msg, menu_golden_msg());
    }

    #[rustfmt::skip]
    const STEP_REQ_GOLDEN: [u8; HEADER_LEN + 5] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x11, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x06, 0x00, // step = 6
        0x01,       // kind = 1（決定）
        0x02,       // value = 2
        0x10,       // screen = 16（マップ）
    ];

    #[test]
    fn golden_step_req() {
        let msg = Message::StepReq(Step {
            step: 6,
            kind: StepKind::Decide,
            value: 2,
            screen: 16,
        });
        assert_eq!(encode_to_vec(SID, &msg), STEP_REQ_GOLDEN);
        let p = dec(&STEP_REQ_GOLDEN).unwrap();
        assert_eq!(p.msg, msg);
    }

    #[rustfmt::skip]
    const STEP_GOLDEN: [u8; HEADER_LEN + 5] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x12, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x07, 0x00, // step = 7
        0x02,       // kind = 2（取消）
        0x00,       // value = 0
        0x07,       // screen = 7（難易度）
    ];

    #[test]
    fn golden_step() {
        let msg = Message::Step(Step {
            step: 7,
            kind: StepKind::Cancel,
            value: 0,
            screen: 7,
        });
        assert_eq!(encode_to_vec(SID, &msg), STEP_GOLDEN);
        let p = dec(&STEP_GOLDEN).unwrap();
        assert_eq!(p.msg, msg);
    }

    // ---------- 毒標本（golden を 1 か所だけ壊す）----------
    // 期待する DecodeError の種類まで assert_eq! で見る（Err というだけでは違うガードが先に落としても気づけない）。

    #[test]
    fn poison_too_short_for_header() {
        let full = signed(&HELLO_GOLDEN);
        assert_eq!(dec_raw(&full[..27]), Err(DecodeError::TooShort { got: 27 }));
        assert_eq!(dec_raw(&[]), Err(DecodeError::TooShort { got: 0 }));
        let mut hdr_only = HELLO_GOLDEN[..HEADER_LEN].to_vec();
        hdr_only.extend_from_slice(&key().tag(&HELLO_GOLDEN[..HEADER_LEN]));
        assert_eq!(hdr_only.len(), 28);
        assert!(matches!(
            dec_raw(&hdr_only),
            Err(DecodeError::LengthMismatch { .. })
        ));
        let ping0 = sealed(&PING_GOLDEN[..HEADER_LEN]);
        assert_eq!(ping0.len(), 36);
        assert!(matches!(
            dec_raw(&ping0),
            Err(DecodeError::LengthMismatch { .. })
        ));
        assert_eq!(
            dec_raw(&ping0[..35]),
            Err(DecodeError::TooShort { got: 35 })
        );
    }

    // ---------- 認証タグと封じた便 ----------

    #[test]
    fn golden_hmac_tag_is_the_value_computed_outside() {
        let want = hex_bytes(crate::auth::tests::PING_TAG);
        assert_eq!(key().tag(&crate::auth::tests::PING_SIGNED).to_vec(), want);
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode(&key(), SID, &hello_golden_msg(), &mut buf);
        assert_eq!(&buf[..n - TAG_LEN], &HELLO_GOLDEN[..]);
        assert_eq!(buf[n - TAG_LEN..n], key().tag(&HELLO_GOLDEN));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// 封じた PING と PONG の golden（Python 3 の `cryptography` で crate の外で計算した値。鍵は `seal.rs` の `OKM_GOLDEN`:
    /// PING は Alice → Bob（前 32 B）・送り手 0・通し番号 0・send_time 60000、PONG は Bob → Alice（後ろ 32 B）・送り手 1・
    /// 通し番号 0x0102030405・send_time 7）。式は `ChaCha20Poly1305(key).encrypt(0 の 4 B ‖ 送り手 ‖ 番号 7 B LE, 本文, ヘッダ)`。
    const PING_SEALED: &str =
        "4847525407080000443322110000000000000000eab77930f219b06945cd3551562305a56b2605db";
    const PONG_SEALED: &str =
        "4847525407090000443322110105040302010000873647e2904f158ea172228851cdf61d9d881c62";

    #[test]
    fn golden_sealed_ping_and_pong() {
        use crate::seal::tests::{ALICE, BOB, arr32};
        use crate::seal::{Ephemeral, KeySeed};
        let a = Ephemeral::from_seed(&KeySeed(arr32(ALICE)));
        let b = Ephemeral::from_seed(&KeySeed(arr32(BOB)));
        let ka = a.dir_keys(&b.public(), true, SID, &key()).unwrap();
        let kb = b.dir_keys(&a.public(), false, SID, &key()).unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = seal(
            &ka.sealer(Sender::Link),
            &Seq::new(),
            SID,
            &Message::Ping { send_time: 60_000 },
            &mut buf,
        )
        .unwrap();
        assert_eq!(buf[..n].to_vec(), hex_bytes(PING_SEALED));
        assert_eq!(
            decode(&key(), Some(&kb.opener()), &buf[..n]).map(|p| p.msg),
            Ok(Message::Ping { send_time: 60_000 })
        );
        let n = seal(
            &kb.sealer(Sender::Pong),
            &Seq::at(0x01_0203_0405),
            SID,
            &Message::Pong { send_time: 7 },
            &mut buf,
        )
        .unwrap();
        assert_eq!(buf[..n].to_vec(), hex_bytes(PONG_SEALED));
        assert_eq!(
            decode(&key(), Some(&ka.opener()), &buf[..n]).map(|p| p.msg),
            Ok(Message::Pong { send_time: 7 })
        );
        assert_eq!(
            decode(&key(), Some(&ka.opener()), &hex_bytes(PING_SEALED)),
            Err(DecodeError::BadTag)
        );
    }

    #[test]
    fn sealed_packets_refuse_flips_truncation_and_other_keys() {
        let full = sealed(&INPUT_COUNT2_LAT_GOLDEN);
        assert_eq!(
            full.len(),
            INPUT_COUNT2_LAT_GOLDEN.len() + crate::seal::SEAL_OVERHEAD
        );
        assert!(dec_raw(&full).is_ok(), "効く期間: 壊さなければ通る");
        for bit in 0..full.len() * 8 {
            let mut b = full.clone();
            b[bit / 8] ^= 1 << (bit % 8);
            let want = if bit < 32 {
                DecodeError::BadMagic
            } else {
                DecodeError::BadTag
            };
            assert_eq!(dec_raw(&b), Err(want), "bit {bit}");
        }
        for cut in 1..=SEAL_TAG_LEN {
            let b = &full[..full.len() - cut];
            assert_eq!(dec_raw(b), Err(DecodeError::BadTag), "{cut} B 切った");
        }
        assert_eq!(decode(&key(), None, &full), Err(DecodeError::NotKeyed));
        assert!(decode(&key(), None, &signed(&HELLO_GOLDEN)).is_ok());
        let k = key();
        let a = crate::seal::Ephemeral::from_seed(&crate::seal::KeySeed([1; 32]));
        let b = crate::seal::Ephemeral::from_seed(&crate::seal::KeySeed([2; 32]));
        let ka = a.dir_keys(&b.public(), true, SID, &k).unwrap();
        let kb = b.dir_keys(&a.public(), false, SID, &k).unwrap();
        let ping = Message::Ping { send_time: 9 };
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = seal(&ka.sealer(Sender::Link), &Seq::new(), SID, &ping, &mut buf).unwrap();
        assert_eq!(
            decode(&k, Some(&kb.opener()), &buf[..n]).map(|p| p.msg),
            Ok(ping)
        );
        assert_eq!(
            decode(&k, Some(&ka.opener()), &buf[..n]),
            Err(DecodeError::BadTag)
        );
        let k2 = AuthKey::derive(&crate::auth::tests::golden_secret(), SID ^ 1);
        let ka2 = a.dir_keys(&b.public(), true, SID ^ 1, &k2).unwrap();
        let n = seal(&ka2.sealer(Sender::Link), &Seq::new(), SID, &ping, &mut buf).unwrap();
        assert_eq!(
            decode(&k, Some(&kb.opener()), &buf[..n]),
            Err(DecodeError::BadTag)
        );
        let mut s = crate::auth::tests::golden_secret();
        s.0[0] ^= 1;
        let k3 = AuthKey::derive(&s, SID);
        let ka3 = a.dir_keys(&b.public(), true, SID, &k3).unwrap();
        let n = seal(&ka3.sealer(Sender::Link), &Seq::new(), SID, &ping, &mut buf).unwrap();
        assert_eq!(
            decode(&k, Some(&kb.opener()), &buf[..n]),
            Err(DecodeError::BadTag)
        );
        let n = seal(&ka.sealer(Sender::Link), &Seq::new(), SID, &ping, &mut buf).unwrap();
        for (at, v) in [(8usize, 0x45u8), (5, TYPE_PONG)] {
            let mut x = buf[..n].to_vec();
            x[at] = v;
            assert_eq!(
                decode(&k, Some(&kb.opener()), &x),
                Err(DecodeError::BadTag),
                "ヘッダの {at} B 目"
            );
        }
    }

    #[test]
    fn poison_tag_bit_flips_truncation_and_other_keys() {
        let full = signed(&HELLO_ACK_GOLDEN);
        assert!(dec_raw(&full).is_ok(), "効く期間: 壊さなければ通る");
        for bit in 0..full.len() * 8 {
            let mut b = full.clone();
            b[bit / 8] ^= 1 << (bit % 8);
            let want = if bit < 32 {
                DecodeError::BadMagic
            } else {
                DecodeError::BadTag
            };
            assert_eq!(dec_raw(&b), Err(want), "bit {bit}");
        }
        for cut in 1..=TAG_LEN {
            let b = &full[..full.len() - cut];
            assert_eq!(dec_raw(b), Err(DecodeError::BadTag), "{cut} B 切った");
        }
        let mut s = crate::auth::tests::golden_secret();
        s.0[0] ^= 1;
        let other_secret = AuthKey::derive(&s, SID);
        let other_session = AuthKey::derive(&crate::auth::tests::golden_secret(), SID ^ 1);
        for k in [other_secret, other_session] {
            let mut buf = [0u8; MAX_DATAGRAM];
            let n = encode(&k, SID, &hello_ack_golden_msg(), &mut buf);
            assert_eq!(dec_raw(&buf[..n]), Err(DecodeError::BadTag));
        }
        let mut buf = [0u8; MAX_DATAGRAM];
        assert_eq!(
            seal(
                &sealer(),
                &Seq::new(),
                SID,
                &hello_ack_golden_msg(),
                &mut buf
            ),
            None
        );
        assert!(is_signed(&hello_ack_golden_msg()) && !is_signed(&Message::Ready));
    }

    #[test]
    fn seal_fits_the_datagram_for_the_longest_bodies() {
        let mut words = [0u16; MAX_WORDS];
        for (i, w) in words.iter_mut().enumerate() {
            *w = i as u16;
        }
        let input = Message::Input(Input {
            seg: 1,
            ack: 2,
            base: 3,
            lat: None,
            count: MAX_WORDS as u8,
            words,
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = seal(&sealer(), &Seq::new(), SID, &input, &mut buf).unwrap();
        assert_eq!(n, 12 + 8 + 80 + 16);
        assert_eq!(dec_raw(&buf[..n]).unwrap().msg, input);
        let mut st = SpectateStream {
            first: 0,
            reset: false,
            count: MAX_STREAM_RECORDS as u8,
            records: [Record::FILLER; MAX_STREAM_RECORDS],
        };
        for r in st.records.iter_mut() {
            *r = Record::name(Seat::P1, &[b'x'; NAME_MAX]);
        }
        let m = Message::SpectateStream(st);
        let n = seal(&sealer(), &Seq::new(), SID, &m, &mut buf).unwrap();
        assert!(n <= MAX_DATAGRAM, "{n} B");
        assert_eq!(n, 12 + 8 + 6 + 16 * 35 + 16);
        assert_eq!(dec_raw(&buf[..n]).unwrap().msg, m);
    }

    #[test]
    fn classify_and_peek_session() {
        let full = signed(&PING_GOLDEN);
        assert_eq!(classify(&full), Datagram::Hgrt);
        assert_eq!(peek_session(&full), Some(SID));
        let stun = crate::stun::encode_binding_request([7; 12]);
        assert_eq!(classify(&stun), Datagram::Stun);
        assert_eq!(peek_session(&stun), None);
        let mut le = stun;
        le[4..8].copy_from_slice(&crate::stun::MAGIC_COOKIE.to_le_bytes());
        assert_eq!(classify(&le), Datagram::Other);
        let mut top = stun;
        top[0] |= 0x80;
        assert_eq!(classify(&top), Datagram::Other);
        assert_eq!(classify(&stun[..19]), Datagram::Other);
        assert_eq!(classify(b""), Datagram::Other);
        assert_eq!(peek_session(&full[..27]), None);
        let mut nm = full.clone();
        nm[0] = b'X';
        assert_eq!(peek_session(&nm), None);
    }

    #[test]
    fn peek_is_handshake_passes_exactly_the_three_handshake_types() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut name = [0u8; NAME_MAX];
        name[0] = b'x';
        let yes = [
            Message::Hello(Hello {
                proto_ver: PROTO_VER,
                latency: 2,
                name_len: 1,
                mode: Mode::Delay,
                can_rollback: false,
                listable: true,
                role_nonce: 7,
                name,
                seat_wish: SeatWish::Any,
                arrival: Arrival::Second,
                build: 0,
                public: public_sample(3),
            }),
            Message::HelloAck {
                echo_nonce: 1,
                role_nonce: 2,
                public: public_sample(4),
            },
            Message::Agree {
                host_seat: Seat::P1,
                delay: 2,
                mode: Mode::Delay,
            },
        ];
        let no = [
            Message::Ping { send_time: 1 },
            Message::Pong { send_time: 1 },
            Message::Bye(ByeReason::Finished),
            Message::Ready,
        ];
        for m in &yes {
            let n = enc(SID, m, &mut buf);
            assert!(peek_is_handshake(&buf[..n]), "{m:?}");
            assert!(
                !peek_is_handshake(&buf[..HEADER_LEN + TAG_LEN - 1]),
                "{m:?} 短い"
            );
            let mut bad = buf[..n].to_vec();
            bad[0] = b'X';
            assert!(!peek_is_handshake(&bad), "{m:?} magic");
        }
        for m in &no {
            let n = enc(SID, m, &mut buf);
            assert!(!peek_is_handshake(&buf[..n]), "{m:?}");
        }
        let n = enc(SID, &yes[0], &mut buf);
        for t in [0u8, 12] {
            let mut v = buf[..n].to_vec();
            v[5] = t;
            assert!(!peek_is_handshake(&v), "type {t}");
        }
    }

    #[test]
    fn poison_body_too_short() {
        let bytes = &HELLO_GOLDEN[..HELLO_GOLDEN.len() - 1];
        assert_eq!(
            dec(bytes),
            Err(DecodeError::LengthMismatch {
                msg_type: TYPE_HELLO,
                expected: 82,
                actual: 81,
            })
        );
    }

    #[test]
    fn poison_trailing_extra_byte() {
        let mut bytes = CHECK_GOLDEN.to_vec();
        bytes.push(0x00);
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::LengthMismatch {
                msg_type: TYPE_CHECK,
                expected: 14,
                actual: 15,
            })
        );
    }

    #[test]
    fn poison_input_count_mismatches_word_list() {
        let mut bytes = INPUT_COUNT3_GOLDEN.to_vec();
        bytes.truncate(bytes.len() - 2); // 語 1 個分（2B）を削る
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::LengthMismatch {
                msg_type: TYPE_INPUT,
                expected: 22,
                actual: 20,
            })
        );
    }

    #[test]
    fn poison_bad_magic() {
        let mut bytes = HELLO_GOLDEN;
        bytes[0] = b'X';
        assert_eq!(dec(&bytes), Err(DecodeError::BadMagic));
    }

    #[test]
    fn poison_bad_version() {
        for v in [0u8, 1, 2, 3, 4, 5, 6, 8, 0xFF] {
            let mut bytes = HELLO_GOLDEN;
            bytes[4] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::BadVersion(v)),
                "バージョン {v}"
            );
        }
        assert_eq!(HELLO_GOLDEN[4], PROTO_VER);
        assert!(dec(&HELLO_GOLDEN).is_ok());
    }

    /// バージョン 0 の HELLO（バージョンを上げる前の golden）。
    #[rustfmt::skip]
    const HELLO_V0_GOLDEN: [u8; HEADER_LEN + 44] = [
        0x48, 0x47, 0x52, 0x54, // magic "HGRT"
        0x00,                   // proto_ver = 0（バージョン 0）
        0x01,                   // type = 1 (HELLO)
        0x00, 0x00,
        0x44, 0x33, 0x22, 0x11, // session_id = 0x11223344
        0x00,                   // 本文の proto_ver = 0
        0x03, 0x09, 0x00,
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
        0xE3, 0x83, 0x86, 0xE3, 0x82, 0xB9, 0xE3, 0x83, 0x88,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    /// バージョン 3 の HELLO（本文 44 B。今の `HELLO_GOLDEN` からバージョンの 2 B と末尾の席の希望・名乗りを除いた列）。バージョン違いの断りが
    /// 長さの検査より先に効くことを見るために残す。
    #[rustfmt::skip]
    const HELLO_V3_GOLDEN: [u8; HEADER_LEN + 44] = [
        0x48, 0x47, 0x52, 0x54, // magic "HGRT"
        0x03,                   // proto_ver = 3（バージョン 3）
        0x01,                   // type = 1 (HELLO)
        0x00, 0x00,
        0x44, 0x33, 0x22, 0x11, // session_id = 0x11223344
        0x03,                   // 本文の proto_ver = 3
        0x03, 0x09, 0x00,
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
        0xE3, 0x83, 0x86, 0xE3, 0x82, 0xB9, 0xE3, 0x83, 0x88,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn a_version_0_hello_is_refused_with_its_version() {
        assert_eq!(dec(&HELLO_V0_GOLDEN), Err(DecodeError::BadVersion(0)));
        let common = HELLO_V0_GOLDEN.len();
        let diff: Vec<usize> = (0..common)
            .filter(|&i| HELLO_GOLDEN[i] != HELLO_V0_GOLDEN[i])
            .collect();
        assert_eq!(diff, [4, HEADER_LEN]);
        assert_eq!(
            HELLO_GOLDEN.len() - common,
            38,
            "足した +44・+45 の 2 B とバージョン 5 の build・公開鍵の 36 B"
        );
    }

    #[test]
    fn a_version_3_hello_is_refused_with_its_version() {
        assert_eq!(dec(&HELLO_V3_GOLDEN), Err(DecodeError::BadVersion(3)));
        let common = HELLO_V3_GOLDEN.len();
        let diff: Vec<usize> = (0..common)
            .filter(|&i| HELLO_GOLDEN[i] != HELLO_V3_GOLDEN[i])
            .collect();
        assert_eq!(diff, [4, HEADER_LEN]);
        assert_eq!(HELLO_GOLDEN.len() - common, 38);
    }

    #[test]
    fn a_version_4_peer_is_refused_with_its_version_for_every_type() {
        let mut v4_hello = HELLO_GOLDEN[..HEADER_LEN + 46].to_vec();
        v4_hello[4] = 4;
        v4_hello[HEADER_LEN] = 4;
        let v4_agree = [
            0x48, 0x47, 0x52, 0x54, 4, TYPE_AGREE, 0, 0, 0x44, 0x33, 0x22, 0x11, 0, 2,
        ];
        let v4_ping = [
            0x48, 0x47, 0x52, 0x54, 4, TYPE_PING, 0, 0, 0x44, 0x33, 0x22, 0x11, 1, 0, 0, 0,
        ];
        for (what, raw) in [
            ("HELLO", &v4_hello[..]),
            ("AGREE", &v4_agree[..]),
            ("PING", &v4_ping[..]),
        ] {
            let s = signed(raw);
            assert_eq!(dec_raw(&s), Err(DecodeError::BadVersion(4)), "{what}");
            assert_eq!(
                decode(&key(), None, &s),
                Err(DecodeError::BadVersion(4)),
                "{what}（鍵の前）"
            );
            let mut forged = s.clone();
            let last = forged.len() - 1;
            forged[last] ^= 1;
            assert_eq!(dec_raw(&forged), Err(DecodeError::BadTag), "{what}（偽）");
        }
    }

    #[test]
    fn poison_hello_seat_wish_out_of_range() {
        for v in [3u8, 0xFF] {
            let mut bytes = HELLO_GOLDEN;
            bytes[HEADER_LEN + 44] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_HELLO,
                    field: "seat_wish",
                }),
                "seat_wish = {v}"
            );
        }
        for v in 0u8..=2 {
            let mut bytes = HELLO_GOLDEN;
            bytes[HEADER_LEN + 44] = v;
            assert!(dec(&bytes).is_ok(), "seat_wish = {v}");
        }
    }

    #[test]
    fn poison_hello_arrival_out_of_range() {
        for v in [3u8, 0xFF] {
            let mut bytes = HELLO_GOLDEN;
            bytes[HEADER_LEN + 45] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_HELLO,
                    field: "arrival",
                }),
                "arrival = {v}"
            );
        }
        for v in 0u8..=2 {
            let mut bytes = HELLO_GOLDEN;
            bytes[HEADER_LEN + 45] = v;
            assert!(dec(&bytes).is_ok(), "arrival = {v}");
        }
    }

    #[test]
    fn poison_header_flags_nonzero() {
        let mut bytes = HELLO_GOLDEN;
        bytes[6] = 1;
        assert_eq!(dec(&bytes), Err(DecodeError::HeaderReserved));
    }

    #[test]
    fn poison_header_reserved_nonzero() {
        let mut bytes = HELLO_GOLDEN;
        bytes[7] = 1;
        assert_eq!(dec(&bytes), Err(DecodeError::HeaderReserved));
    }

    #[test]
    fn poison_unknown_type_each_gap() {
        // 0・5〜7・12・22 以上は欠番・未実装（13・14・19 は観戦に使う。20 は READY、21 は HOST_STATE）。
        for &t in &[0u8, 5, 6, 7, 12, 22, 255] {
            let mut bytes = HELLO_GOLDEN.to_vec();
            bytes[5] = t;
            assert_eq!(dec(&bytes), Err(DecodeError::UnknownType(t)), "type={t}");
        }
    }

    #[test]
    fn poison_hello_body_proto_ver_nonzero() {
        for v in [0u8, 1, 2, 3, 4, 5, 6, 8, 0xFF] {
            let mut bytes = HELLO_GOLDEN;
            bytes[12] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_HELLO,
                    field: "proto_ver",
                }),
                "本文の proto_ver = {v}"
            );
        }
        assert_eq!(HELLO_GOLDEN[12], PROTO_VER);
        assert!(dec(&HELLO_GOLDEN).is_ok());
    }

    #[test]
    fn encode_clamps_count_over_32() {
        let mut words = [0u16; MAX_WORDS];
        for (k, w) in words.iter_mut().enumerate() {
            *w = 0x0100 + k as u16;
        }
        let input = Message::Input(Input {
            seg: 7,
            ack: 5,
            base: 9,
            lat: None,
            count: 40,
            words,
        });
        let bytes = encode_to_vec(SID, &input);
        assert_eq!(bytes.len(), 92);
        assert_eq!(bytes[12 + 15], 32);
        match dec(&bytes).map(|p| p.msg) {
            Ok(Message::Input(i)) => {
                assert_eq!(i.count, 32);
                assert_eq!(i.words, words);
            }
            other => panic!("INPUT として読めない: {other:?}"),
        }

        let menu = Message::Menu(Menu {
            step: 3,
            ack: 1,
            base: 2,
            count: 40,
            words,
        });
        let bytes = encode_to_vec(SID, &menu);
        assert_eq!(bytes.len(), 12 + 11 + 64);
        assert_eq!(bytes[12 + 10], 32);
        match dec(&bytes).map(|p| p.msg) {
            Ok(Message::Menu(m)) => {
                assert_eq!(m.count, 32);
                assert_eq!(m.words, words);
            }
            other => panic!("MENU として読めない: {other:?}"),
        }
    }

    #[test]
    fn poison_hello_latency_out_of_range() {
        let mut bytes = HELLO_GOLDEN;
        bytes[12 + 1] = 10; // latency = 10 (> 9)
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_HELLO,
                field: "latency",
            })
        );
    }

    #[test]
    fn poison_hello_name_len_out_of_range() {
        let mut bytes = HELLO_GOLDEN;
        bytes[12 + 2] = 33; // name_len = 33 (> 32)
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_HELLO,
                field: "name_len",
            })
        );
    }

    #[test]
    fn poison_hello_mode_unknown() {
        for v in [8u8, 0x0F, 0x80, 0xFF] {
            let mut bytes = HELLO_GOLDEN;
            bytes[12 + 3] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_HELLO,
                    field: "mode",
                }),
                "{v}"
            );
        }
    }

    #[test]
    fn poison_hello_name_tail_nonzero() {
        let mut bytes = HELLO_GOLDEN;
        let last = HEADER_LEN + 43; // 本文 +43 = name の最後（バージョン 4 で末尾に +44・+45 が付いたので len - 1 ではない）
        bytes[last] = 0x7A; // name 領域の最後の 1B（name_len より後ろ）
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_HELLO,
                field: "name_tail",
            })
        );
    }

    #[test]
    fn poison_hello_name_invalid_utf8() {
        let mut bytes = HELLO_GOLDEN;
        bytes[12 + 12] = 0xFF; // name[0] を不正な先頭バイトに
        bytes[12 + 13] = 0xFF; // 後続も continuation byte にならない値に
        assert_eq!(dec(&bytes), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn poison_input_count_over_max() {
        let mut bytes = INPUT_COUNT0_GOLDEN;
        bytes[12 + 15] = 33; // count = 33 (> 32)
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_INPUT,
                field: "count",
            })
        );
    }

    #[test]
    fn poison_input_lat_delay_over_max_with_change() {
        let mut bytes = INPUT_COUNT2_LAT_GOLDEN;
        bytes[12 + 14] = 10; // lat_delay = 10 (> 9)。lat_frame は切替ありのまま
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_INPUT,
                field: "lat_delay",
            })
        );
    }

    #[test]
    fn poison_input_lat_delay_nonzero_without_change() {
        let mut bytes = INPUT_COUNT0_GOLDEN;
        bytes[12 + 14] = 1;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_INPUT,
                field: "lat_delay",
            })
        );
    }

    #[test]
    fn poison_agree_host_seat_out_of_range() {
        let mut bytes = AGREE_GOLDEN;
        bytes[12] = 2; // host_seat = 2 (0/1 のみ)
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_AGREE,
                field: "host_seat",
            })
        );
    }

    #[test]
    fn poison_agree_delay_out_of_range() {
        let mut bytes = AGREE_GOLDEN;
        bytes[13] = 10; // delay = 10 (> 9)
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_AGREE,
                field: "delay",
            })
        );
    }

    #[test]
    fn poison_agree_rollback_form() {
        let field = |f| {
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_AGREE,
                field: f,
            })
        };
        let mut b = AGREE_ROLLBACK_GOLDEN;
        b[HEADER_LEN + 1] = 10;
        assert_eq!(dec(&b), field("delay"), "D = 10");
        let mut b = AGREE_ROLLBACK_GOLDEN;
        b[HEADER_LEN] = 2;
        assert_eq!(dec(&b), field("host_seat"));
        for m in [0u8, 2, 0xFF] {
            let mut b = AGREE_ROLLBACK_GOLDEN;
            b[HEADER_LEN + 2] = m;
            assert_eq!(dec(&b), field("mode"), "mode {m}");
        }
        let short = &AGREE_GOLDEN[..HEADER_LEN + 1];
        let mut long = AGREE_ROLLBACK_GOLDEN.to_vec();
        long.push(0);
        for (bytes, n) in [(short.to_vec(), 1usize), (long, 4)] {
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::LengthMismatch {
                    msg_type: TYPE_AGREE,
                    expected: 2,
                    actual: n,
                }),
                "{n} B"
            );
        }
        let mut b = AGREE_ROLLBACK_GOLDEN;
        b[HEADER_LEN + 1] = 9;
        assert!(matches!(
            dec(&b).map(|p| p.msg),
            Ok(Message::Agree {
                delay: 9,
                mode: Mode::Rollback,
                ..
            })
        ));
    }

    #[test]
    fn poison_select_reserved_nonzero() {
        let mut bytes = SELECT_GOLDEN;
        bytes[12 + 11] = 1;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_SELECT,
                field: "reserved",
            })
        );
    }

    #[test]
    fn poison_bye_reason_out_of_range_low() {
        let mut bytes = BYE_GOLDEN;
        bytes[12] = 0;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_BYE,
                field: "reason",
            })
        );
    }

    #[test]
    fn poison_bye_reason_out_of_range_high() {
        let mut bytes = BYE_GOLDEN;
        bytes[12] = 5;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_BYE,
                field: "reason",
            })
        );
    }

    #[test]
    fn poison_menu_count_over_max() {
        let mut bytes = MENU_GOLDEN;
        bytes[12 + 10] = 33; // count = 33 (> 32)
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_MENU,
                field: "count",
            })
        );
    }

    #[test]
    fn poison_step_req_kind_out_of_range() {
        let mut bytes = STEP_REQ_GOLDEN;
        bytes[12 + 2] = 3; // kind = 3（STEP_REQ は 1/2 のみ。3 は STEP の合わせ）
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_STEP_REQ,
                field: "kind",
            })
        );
    }

    #[rustfmt::skip]
    const STEP_ALIGN_GOLDEN: [u8; HEADER_LEN + 5] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x12, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x08, 0x00, // step = 8
        0x03,       // kind = 3（合わせ）
        0x64,       // value = 0x64（キャラ選択の 1P のカーソル 4・2P のカーソル 6）
        0x08,       // screen = 8（キャラ）
    ];

    #[test]
    fn golden_step_align() {
        let msg = Message::Step(Step {
            step: 8,
            kind: StepKind::Align,
            value: 0x64,
            screen: 8,
        });
        assert_eq!(encode_to_vec(SID, &msg), STEP_ALIGN_GOLDEN);
        assert_eq!(dec(&STEP_ALIGN_GOLDEN).unwrap().msg, msg);
        let req = Message::StepReq(Step {
            step: 8,
            kind: StepKind::Decide,
            value: 0x64,
            screen: 8,
        });
        let mut bytes = encode_to_vec(SID, &req);
        bytes[HEADER_LEN + 2] = 3;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_STEP_REQ,
                field: "kind",
            })
        );
    }

    #[test]
    fn poison_step_kind_five() {
        let mut bytes = STEP_GOLDEN;
        bytes[12 + 2] = 5;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_STEP,
                field: "kind",
            })
        );
    }

    #[test]
    fn poison_step_kind_zero() {
        let mut bytes = STEP_GOLDEN;
        bytes[12 + 2] = 0;
        assert_eq!(
            dec(&bytes),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_STEP,
                field: "kind",
            })
        );
    }

    #[rustfmt::skip]
    const STEP_FORMAT_GOLDEN: [u8; HEADER_LEN + 5] = [
        0x48, 0x47, 0x52, 0x54,
        0x07, 0x12, 0x00, 0x00,
        0x44, 0x33, 0x22, 0x11,
        0x09, 0x00, // step = 9
        0x04,       // kind = 4（形式の切替）
        0x0F,       // value = rollback(bit0) | auto(bit1) | D 3 << 2
        0x08,       // screen = 8（キャラセレクト）
    ];

    #[test]
    fn golden_step_format_and_its_request() {
        let fv = FormatValue {
            mode: Mode::Rollback,
            delay: 3,
            auto: true,
        };
        assert_eq!(fv.pack(), 0x0F);
        assert_eq!(FormatValue::unpack(0x0F), Some(fv));
        let msg = Message::Step(Step {
            step: 9,
            kind: StepKind::Format,
            value: fv.pack(),
            screen: 8,
        });
        assert_eq!(encode_to_vec(SID, &msg), STEP_FORMAT_GOLDEN);
        assert_eq!(dec(&STEP_FORMAT_GOLDEN).unwrap().msg, msg);
        assert_eq!(FormatValue::request(Mode::Delay), 0);
        assert_eq!(FormatValue::request(Mode::Rollback), 1);
        assert_eq!(FormatValue::requested_mode(1), Some(Mode::Rollback));
        assert_eq!(FormatValue::requested_mode(0), Some(Mode::Delay));
        assert_eq!(
            FormatValue::requested_mode(0x0F),
            None,
            "D と旗が載った要求"
        );
        let req = Message::StepReq(Step {
            step: 9,
            kind: StepKind::Format,
            value: FormatValue::request(Mode::Rollback),
            screen: 8,
        });
        assert_eq!(dec(&encode_to_vec(SID, &req)).unwrap().msg, req);
        for mode in [Mode::Delay, Mode::Rollback] {
            for delay in 0..=crate::mode_pick::MAX_PICK_DELAY {
                for auto in [false, true] {
                    let f = FormatValue { mode, delay, auto };
                    assert_eq!(FormatValue::unpack(f.pack()), Some(f), "{f:?}");
                }
            }
        }
    }

    #[test]
    fn poison_step_format_values_and_the_spectate_stream() {
        let mut bytes = STEP_FORMAT_GOLDEN;
        assert!(dec(&bytes).is_ok(), "完全な形の STEP は通る");
        for v in [9u8 << 2, 0x0F | 0x40, 0x0F | 0x80, 0xFF] {
            bytes[12 + 3] = v;
            assert_eq!(
                dec(&bytes),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_STEP,
                    field: "value",
                }),
                "{v:#x}"
            );
        }
        let req = |v: u8| {
            let mut b = encode_to_vec(
                SID,
                &Message::StepReq(Step {
                    step: 9,
                    kind: StepKind::Decide,
                    value: 0,
                    screen: 8,
                }),
            );
            b[12 + 2] = 4;
            b[12 + 3] = v;
            dec(&b)
        };
        assert!(req(0).is_ok() && req(1).is_ok(), "形式だけの要求は通る");
        for v in [2u8, 0x0F, 0x40, 0xFF] {
            assert_eq!(
                req(v),
                Err(DecodeError::FieldOutOfRange {
                    msg_type: TYPE_STEP_REQ,
                    field: "value",
                }),
                "{v:#x}"
            );
        }
        let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
        records[0] = Record::Step(Step {
            step: 9,
            kind: StepKind::Decide,
            value: 0,
            screen: 8,
        });
        let stream = Message::SpectateStream(SpectateStream {
            first: 0,
            reset: false,
            count: 1,
            records,
        });
        let mut b = encode_to_vec(SID, &stream);
        assert!(dec(&b).is_ok(), "Decide の記録は通る");
        b[HEADER_LEN + STREAM_HEAD_LEN + 1 + 2] = 4;
        assert_eq!(
            dec(&b),
            Err(DecodeError::FieldOutOfRange {
                msg_type: TYPE_SPECTATE_STREAM,
                field: "kind",
            })
        );
    }

    #[test]
    fn golden_hello_can_rollback_is_bit1_and_independent_of_the_wish() {
        let Message::Hello(base) = hello_golden_msg() else {
            unreachable!("golden は HELLO");
        };
        for (mode, can, byte) in [
            (Mode::Delay, false, 0u8),
            (Mode::Rollback, false, 1),
            (Mode::Delay, true, 2),
            (Mode::Rollback, true, 3),
        ] {
            let h = Hello {
                mode,
                can_rollback: can,
                ..base
            };
            let mut want = HELLO_GOLDEN;
            want[HEADER_LEN + 3] = byte;
            assert_eq!(
                encode_to_vec(SID, &Message::Hello(h)),
                want,
                "{mode:?} {can}"
            );
            assert_eq!(dec(&want).unwrap().msg, Message::Hello(h), "{mode:?} {can}");
        }
    }

    /// 一覧に載せてよいの印は +3 の bit2 だけに出て、望むモード（bit0）・rollback ができる（bit1）とどの組でも混ざらない。
    #[test]
    fn golden_hello_listable_is_bit2_and_independent_of_the_other_bits() {
        let Message::Hello(base) = hello_golden_msg() else {
            unreachable!("golden は HELLO");
        };
        for byte in 0u8..8 {
            let h = Hello {
                mode: if byte & 1 != 0 {
                    Mode::Rollback
                } else {
                    Mode::Delay
                },
                can_rollback: byte & 2 != 0,
                listable: byte & 4 != 0,
                ..base
            };
            let mut want = HELLO_GOLDEN;
            want[HEADER_LEN + 3] = byte;
            assert_eq!(encode_to_vec(SID, &Message::Hello(h)), want, "{byte:#04x}");
            let Message::Hello(got) = dec(&want).unwrap().msg else {
                panic!("{byte:#04x}: HELLO に読めない");
            };
            assert_eq!(got, h, "{byte:#04x}");
            assert_eq!(got.listable, byte >= 4, "{byte:#04x}");
        }
        // 立てたときに変わるのは +3 の 1 バイトだけ。
        let on = encode_to_vec(
            SID,
            &Message::Hello(Hello {
                listable: true,
                ..base
            }),
        );
        let diff: Vec<usize> = (0..on.len())
            .filter(|&i| on[i] != HELLO_GOLDEN[i])
            .collect();
        assert_eq!(diff, vec![HEADER_LEN + 3]);
        assert_eq!(on[HEADER_LEN + 3], 0x04);
    }

    // ---------- 乱数（splitmix64） ----------

    struct SplitMix64(u64);

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            SplitMix64(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn next_u32(&mut self) -> u32 {
            (self.next_u64() >> 32) as u32
        }

        fn next_u16(&mut self) -> u16 {
            (self.next_u64() >> 48) as u16
        }

        fn next_u8(&mut self) -> u8 {
            (self.next_u64() >> 56) as u8
        }

        fn next_bool(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }

        /// `0..=max` の一様分布ではないが、検査用の値の生成には十分。
        fn next_range_u8(&mut self, max_inclusive: u8) -> u8 {
            (self.next_u64() % (max_inclusive as u64 + 1)) as u8
        }
    }

    fn random_step(rng: &mut SplitMix64) -> Step {
        Step {
            step: rng.next_u16(),
            kind: if rng.next_bool() {
                StepKind::Decide
            } else {
                StepKind::Cancel
            },
            value: rng.next_u8(),
            screen: rng.next_u8(),
        }
    }

    fn random_select(rng: &mut SplitMix64) -> Select {
        Select {
            match_no: rng.next_u16(),
            chara: rng.next_u8(),
            color: rng.next_u8(),
            life: rng.next_u8(),
            charge: rng.next_u8(),
            map: rng.next_u8(),
            difficulty: rng.next_u8(),
            cpu_1p: rng.next_u8(),
            cpu_2p: rng.next_u8(),
            background: rng.next_u8(),
            rng_seed: rng.next_u16(),
            option_digest: rng.next_u32(),
        }
    }

    fn random_record(rng: &mut SplitMix64) -> Record {
        match rng.next_range_u8(7) {
            0 => Record::Seg {
                seg: rng.next_u16(),
            },
            1 => Record::Select {
                seat: if rng.next_bool() { Seat::P1 } else { Seat::P2 },
                sel: random_select(rng),
            },
            2 => Record::Step(random_step(rng)),
            3 => Record::Menu {
                p1: rng.next_u16(),
                p2: rng.next_u16(),
            },
            4 => Record::Frame {
                frame: rng.next_u32(),
                p1: rng.next_u16(),
                p2: rng.next_u16(),
            },
            5 => {
                // 0x20..=0x7E の表示できる ASCII だけ（1 バイト = 常に妥当な UTF-8）。
                let len = usize::from(rng.next_range_u8(NAME_MAX as u8));
                let name: Vec<u8> = (0..len).map(|_| 0x20 + rng.next_range_u8(0x5E)).collect();
                Record::name(if rng.next_bool() { Seat::P1 } else { Seat::P2 }, &name)
            }
            6 => Record::Stats {
                ping_ms: rng.next_u16(),
                latency: rng.next_u8(),
                rollback: rng.next_u8(),
            },
            _ => Record::End(match rng.next_range_u8(3) {
                0 => ByeReason::Finished,
                1 => ByeReason::Disconnected,
                2 => ByeReason::Desync,
                _ => ByeReason::Cancelled,
            }),
        }
    }

    fn random_public(rng: &mut SplitMix64) -> [u8; PUBLIC_LEN] {
        let mut p = [0u8; PUBLIC_LEN];
        for b in p.iter_mut() {
            *b = rng.next_u8();
        }
        p
    }

    fn random_message(rng: &mut SplitMix64) -> Message {
        match rng.next_range_u8(15) {
            0 => {
                let name_len = rng.next_range_u8(NAME_MAX as u8);
                let mut name = [0u8; NAME_MAX];
                for b in name.iter_mut().take(name_len as usize) {
                    // 0x20..=0x7E の表示できる ASCII だけ使う（1 バイト = 常に妥当な UTF-8）。
                    *b = 0x20 + rng.next_range_u8(0x7E - 0x20);
                }
                Message::Hello(Hello {
                    proto_ver: PROTO_VER,
                    latency: rng.next_range_u8(MAX_DELAY),
                    name_len,
                    mode: if rng.next_bool() {
                        Mode::Delay
                    } else {
                        Mode::Rollback
                    },
                    can_rollback: rng.next_bool(),
                    listable: rng.next_bool(),
                    role_nonce: rng.next_u64(),
                    name,
                    seat_wish: match rng.next_range_u8(2) {
                        0 => SeatWish::Any,
                        1 => SeatWish::P1,
                        _ => SeatWish::P2,
                    },
                    arrival: match rng.next_range_u8(2) {
                        0 => Arrival::Unknown,
                        1 => Arrival::First,
                        _ => Arrival::Second,
                    },
                    build: rng.next_u32(),
                    public: random_public(rng),
                })
            }
            1 => Message::HelloAck {
                echo_nonce: rng.next_u64(),
                role_nonce: rng.next_u64(),
                public: random_public(rng),
            },
            2 => {
                let count = rng.next_range_u8(MAX_WORDS as u8);
                let mut words = [0u16; MAX_WORDS];
                for w in words.iter_mut().take(count as usize) {
                    *w = rng.next_u16();
                }
                let lat = if rng.next_bool() {
                    let mut frame = rng.next_u32();
                    if frame == u32::MAX {
                        // 0xFFFFFFFF は「切替なし」の印なので、Some の中身には使わない
                        // （使うと decode が None に読み替え、往復が一致しなくなる）。
                        frame = 0;
                    }
                    Some(LatChange {
                        frame,
                        delay: rng.next_range_u8(MAX_DELAY),
                    })
                } else {
                    None
                };
                Message::Input(Input {
                    seg: rng.next_u16(),
                    ack: rng.next_u32(),
                    base: rng.next_u32(),
                    lat,
                    count,
                    words,
                })
            }
            3 => Message::Check(Check {
                seg: rng.next_u16(),
                frame: rng.next_u32(),
                rng_state: rng.next_u16(),
                rng_count: rng.next_u32(),
                life_1p: rng.next_u8(),
                life_2p: rng.next_u8(),
            }),
            4 => Message::Ping {
                send_time: rng.next_u32(),
            },
            5 => Message::Pong {
                send_time: rng.next_u32(),
            },
            6 => Message::Agree {
                host_seat: if rng.next_bool() { Seat::P1 } else { Seat::P2 },
                delay: rng.next_range_u8(MAX_DELAY),
                mode: if rng.next_bool() {
                    Mode::Delay
                } else {
                    Mode::Rollback
                },
            },
            7 => Message::Select(Select {
                match_no: rng.next_u16(),
                chara: rng.next_u8(),
                color: rng.next_u8(),
                life: rng.next_u8(),
                charge: rng.next_u8(),
                map: rng.next_u8(),
                difficulty: rng.next_u8(),
                cpu_1p: rng.next_u8(),
                cpu_2p: rng.next_u8(),
                background: rng.next_u8(),
                rng_seed: rng.next_u16(),
                option_digest: rng.next_u32(),
            }),
            8 => {
                let reason = match rng.next_range_u8(3) {
                    0 => ByeReason::Finished,
                    1 => ByeReason::Disconnected,
                    2 => ByeReason::Desync,
                    _ => ByeReason::Cancelled,
                };
                Message::Bye(reason)
            }
            9 => {
                let count = rng.next_range_u8(MAX_WORDS as u8);
                let mut words = [0u16; MAX_WORDS];
                for w in words.iter_mut().take(count as usize) {
                    *w = rng.next_u16();
                }
                Message::Menu(Menu {
                    step: rng.next_u16(),
                    ack: rng.next_u32(),
                    base: rng.next_u32(),
                    count,
                    words,
                })
            }
            10 => {
                let name_len = rng.next_range_u8(NAME_MAX as u8);
                let mut name = [0u8; NAME_MAX];
                for b in name.iter_mut().take(name_len as usize) {
                    *b = 0x20 + rng.next_range_u8(0x7E - 0x20);
                }
                Message::SpectateJoin(SpectateJoin {
                    name_len,
                    nonce: rng.next_u64(),
                    name,
                    build: rng.next_u32(),
                    public: random_public(rng),
                })
            }
            11 => {
                let count = rng.next_range_u8(MAX_STREAM_RECORDS as u8);
                let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
                for r in records.iter_mut().take(count as usize) {
                    *r = random_record(rng);
                }
                Message::SpectateStream(SpectateStream {
                    first: rng.next_u32(),
                    reset: rng.next_bool(),
                    count,
                    records,
                })
            }
            12 => Message::SpectateAck {
                nonce: rng.next_u64(),
                next: rng.next_u32(),
                sack: rng.next_u64() & SACK_MASK,
            },
            13 => Message::Ready,
            14 => {
                let mode = if rng.next_bool() {
                    Mode::Delay
                } else {
                    Mode::Rollback
                };
                Message::HostState(HostState {
                    seq: rng.next_u16(),
                    mode,
                    delay: rng.next_range_u8(crate::delay_pick::bounds(mode).1),
                    auto: rng.next_bool(),
                })
            }
            _ => {
                let step = Step {
                    step: rng.next_u16(),
                    kind: if rng.next_bool() {
                        StepKind::Decide
                    } else {
                        StepKind::Cancel
                    },
                    value: rng.next_u8(),
                    screen: rng.next_u8(),
                };
                match rng.next_range_u8(3) {
                    0 => Message::StepReq(step),
                    1 => Message::Step(step),
                    _ => Message::Step(Step {
                        kind: StepKind::Align,
                        ..step
                    }),
                }
            }
        }
    }

    // ---------- 観戦の 3 メッセージ（13 JOIN・14 STREAM・19 ACK）----------

    fn spectate_pkt(msg_type: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![
            b'H', b'G', b'R', b'T', 7, msg_type, 0, 0, 0x44, 0x33, 0x22,
            0x11, // バージョン 7
        ];
        v.extend_from_slice(body);
        v
    }

    #[rustfmt::skip]
    const SPECTATE_JOIN_BODY: [u8; JOIN_LEN] = [
        0x02,                   // name_len = 2
        0x00,                   // 予約
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // nonce = 0x1122334455667788
        b'a', b'b', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // name 32 B（"ab" ＋ 0 が 30）
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x04, 0x03, 0x02, 0x01, // build = 0x01020304（+42。バージョン 5）
        // 公開鍵 32 B（+46。バージョン 5）: 60 61 … 7F
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x6F,
        0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x7B, 0x7C, 0x7D, 0x7E, 0x7F,
    ];

    fn spectate_join_msg() -> Message {
        Message::SpectateJoin(SpectateJoin {
            name_len: 2,
            nonce: 0x1122_3344_5566_7788,
            name: name_bytes(b"ab"),
            build: 0x0102_0304,
            public: public_sample(0x60),
        })
    }

    #[test]
    fn golden_spectate_join() {
        let want = spectate_pkt(TYPE_SPECTATE_JOIN, &SPECTATE_JOIN_BODY);
        assert_eq!(want.len(), HEADER_LEN + 78);
        assert_eq!(encode_to_vec(SID, &spectate_join_msg()), want);
        assert_eq!(dec(&want).unwrap().msg, spectate_join_msg());
    }

    #[rustfmt::skip]
    const SPECTATE_ACK_BODY: [u8; 20] = [
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // nonce = 0x0102030405060708
        0x00, 0x01, 0x00, 0x00,                         // next = 256
        0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, // sack = 0x0123456789ABCDEF（ビット 0 = 番号 257）
    ];

    #[test]
    fn golden_spectate_ack() {
        let msg = Message::SpectateAck {
            nonce: 0x0102_0304_0506_0708,
            next: 256,
            sack: 0x0123_4567_89AB_CDEF,
        };
        let want = spectate_pkt(TYPE_SPECTATE_ACK, &SPECTATE_ACK_BODY);
        assert_eq!(encode_to_vec(SID, &msg), want);
        assert_eq!(dec(&want).unwrap().msg, msg);
    }

    /// 6 種類の記録を 1 つずつ並べた STREAM（本文 51 B）。
    #[rustfmt::skip]
    const SPECTATE_STREAM_BODY: [u8; 51] = [
        0x05, 0x01, 0x00, 0x00, // first = 261
        0x01,                   // flags: bit0 = reset
        0x06,                   // count = 6
        0x01, 0x03, 0x02,       // Seg { seg: 0x0203 }
        0x02, 0x01,             // Select { seat: P2（1）,
        0x07, 0x00, 0x03, 0x01, 0x05, 0x02, 0x09, 0x01, 0x04, 0x06, 0x00, 0x00, // match_no 7・chara 3・color 1・life 5・charge 2・map 9・difficulty 1・cpu 4/6・background 0・予約
        0x34, 0x12, 0xDD, 0xCC, 0xBB, 0xAA, // rng_seed 0x1234・option_digest 0xAABBCCDD }
        0x03, 0x09, 0x00, 0x02, 0x21, 0x08, // Step { step 9・Cancel（2）・value 0x21・screen 8 }
        0x04, 0x00, 0x01, 0x11, 0x00,       // Menu { p1: 0x0100, p2: 0x0011 }
        0x05, 0x03, 0x02, 0x01, 0x00, 0x40, 0x00, 0x04, 0x00, // Frame { frame 0x00010203・p1 0x0040・p2 0x0004 }
        0x06, 0x03,             // End(Desync)
    ];

    fn spectate_stream_msg() -> Message {
        let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
        records[0] = Record::Seg { seg: 0x0203 };
        records[1] = Record::Select {
            seat: Seat::P2,
            sel: Select {
                match_no: 7,
                chara: 3,
                color: 1,
                life: 5,
                charge: 2,
                map: 9,
                difficulty: 1,
                cpu_1p: 4,
                cpu_2p: 6,
                background: 0,
                rng_seed: 0x1234,
                option_digest: 0xAABB_CCDD,
            },
        };
        records[2] = Record::Step(Step {
            step: 9,
            kind: StepKind::Cancel,
            value: 0x21,
            screen: 8,
        });
        records[3] = Record::Menu {
            p1: 0x0100,
            p2: 0x0011,
        };
        records[4] = Record::Frame {
            frame: 0x0001_0203,
            p1: 0x40,
            p2: 4,
        };
        records[5] = Record::End(ByeReason::Desync);
        Message::SpectateStream(SpectateStream {
            first: 261,
            reset: true,
            count: 6,
            records,
        })
    }

    #[test]
    fn golden_spectate_stream() {
        let want = spectate_pkt(TYPE_SPECTATE_STREAM, &SPECTATE_STREAM_BODY);
        assert_eq!(encode_to_vec(SID, &spectate_stream_msg()), want);
        assert_eq!(dec(&want).unwrap().msg, spectate_stream_msg());
        let empty = Message::SpectateStream(SpectateStream {
            first: 7,
            reset: false,
            count: 0,
            records: [Record::FILLER; MAX_STREAM_RECORDS],
        });
        assert_eq!(
            encode_to_vec(SID, &empty),
            spectate_pkt(TYPE_SPECTATE_STREAM, &[7, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn spectate_stream_full_packet_fits_the_datagram() {
        let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
        records.fill(Record::Select {
            seat: Seat::P1,
            sel: Select {
                match_no: 0,
                chara: 0,
                color: 0,
                life: 0,
                charge: 0,
                map: 0,
                difficulty: 0,
                cpu_1p: 0,
                cpu_2p: 0,
                background: 0,
                rng_seed: 0,
                option_digest: 0,
            },
        });
        let msg = Message::SpectateStream(SpectateStream {
            first: 0,
            reset: false,
            count: MAX_STREAM_RECORDS as u8,
            records,
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = enc(SID, &msg, &mut buf);
        assert_eq!(n, 12 + 6 + 16 * 20 + crate::seal::SEAL_OVERHEAD);
        assert!(n <= MAX_DATAGRAM);
        assert_eq!(dec_raw(&buf[..n]).unwrap().msg, msg);
    }

    /// 名前の記録 2 つ（「テスト」と ASCII）を並べた STREAM の本文（6 + 2 × 35 = 76 B）。
    #[rustfmt::skip]
    const SPECTATE_STREAM_NAMES_BODY: [u8; 76] = [
        0x0A, 0x00, 0x00, 0x00, // first = 10
        0x00,                   // flags
        0x02,                   // count = 2
        0x07, 0x00, 0x09,       // Name { seat: P1（0）, len 9,
        0xE3, 0x83, 0x86, 0xE3, 0x82, 0xB9, 0xE3, 0x83, 0x88, // 「テスト」の UTF-8
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 32 B になるまでの 0 }
        0x07, 0x01, 0x03,       // Name { seat: P2（1）, len 3,
        b'a', b'b', b'c',
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 32 B になるまでの 0 }
    ];

    fn spectate_names_msg() -> Message {
        let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
        records[0] = Record::name(Seat::P1, "テスト".as_bytes());
        records[1] = Record::name(Seat::P2, b"abc");
        Message::SpectateStream(SpectateStream {
            first: 10,
            reset: false,
            count: 2,
            records,
        })
    }

    #[test]
    fn golden_spectate_stream_names() {
        let want = spectate_pkt(TYPE_SPECTATE_STREAM, &SPECTATE_STREAM_NAMES_BODY);
        assert_eq!(encode_to_vec(SID, &spectate_names_msg()), want);
        assert_eq!(dec(&want).unwrap().msg, spectate_names_msg());
        let Record::Name { len, bytes, .. } = Record::name(Seat::P1, &[b'x'; 33]) else {
            panic!("Record::name が Name を返さない");
        };
        assert_eq!((len, bytes), (32, [b'x'; 32]));
    }

    /// 対戦の状態の記録 2 つ（ping 14 ms・Latency 1・Rollback 2 と、測れていない印の値）を並べた STREAM の本文（6 + 2 × 5 = 16 B）。
    #[rustfmt::skip]
    const SPECTATE_STREAM_STATS_BODY: [u8; 16] = [
        0x0A, 0x00, 0x00, 0x00, // first = 10
        0x00,                   // flags
        0x02,                   // count = 2
        0x08, 0x0E, 0x00, 0x01, 0x02, // Stats { ping_ms: 14（u16 LE）, latency: 1, rollback: 2 }
        0x08, 0xFF, 0xFF, 0x03, 0xFF, // Stats { ping_ms: 印（0xFFFF）, latency: 3, rollback: 印（0xFF）}
    ];

    fn spectate_stats_msg() -> Message {
        let mut records = [Record::FILLER; MAX_STREAM_RECORDS];
        records[0] = Record::stats(Some(14), 1, Some(2));
        records[1] = Record::stats(None, 3, None);
        Message::SpectateStream(SpectateStream {
            first: 10,
            reset: false,
            count: 2,
            records,
        })
    }

    #[test]
    fn golden_spectate_stream_stats() {
        let want = spectate_pkt(TYPE_SPECTATE_STREAM, &SPECTATE_STREAM_STATS_BODY);
        assert_eq!(encode_to_vec(SID, &spectate_stats_msg()), want);
        assert_eq!(dec(&want).unwrap().msg, spectate_stats_msg());
        assert_eq!(
            Record::stats(Some(u32::MAX), 9, Some(u32::MAX)),
            Record::Stats {
                ping_ms: STATS_PING_UNKNOWN - 1,
                latency: 9,
                rollback: STATS_ROLLBACK_NONE - 1,
            }
        );
        assert_eq!(
            Record::stats(Some(0), 0, Some(0)),
            Record::Stats {
                ping_ms: 0,
                latency: 0,
                rollback: 0,
            }
        );
    }

    #[test]
    fn poison_spectate_stats_record() {
        let t = TYPE_SPECTATE_STREAM;
        let good = SPECTATE_STREAM_STATS_BODY;
        assert!(dec(&spectate_pkt(t, &good)).is_ok());
        assert_eq!(
            dec(&spectate_pkt(t, &good[..15])),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: 5,
                actual: 4
            })
        );
        let mut long = good.to_vec();
        long.push(0);
        assert!(dec(&spectate_pkt(t, &long)).is_err());
    }

    #[test]
    fn a_record_stays_within_36_bytes() {
        // 記録に大きい欄を足して 36 B を越えると、観戦のホストのリングバッファ（`spectate::RING_CAP` 個）の確保と `Message` の大きさが見積もりより太る。
        assert_eq!(core::mem::size_of::<Record>(), 36);
    }

    #[test]
    fn spectate_stream_of_sixteen_names_fits_the_datagram() {
        let msg = Message::SpectateStream(SpectateStream {
            first: 0,
            reset: false,
            count: MAX_STREAM_RECORDS as u8,
            records: [Record::name(Seat::P2, "あ".as_bytes()); MAX_STREAM_RECORDS],
        });
        let mut buf = [0u8; MAX_DATAGRAM];
        // バージョン 5 で nonce の欄の 8 B が増えた（12 + 8 + 6 + 16×35 + 16 = 602 B）。
        let n = enc(SID, &msg, &mut buf);
        assert_eq!(n, 12 + 6 + 16 * 35 + crate::seal::SEAL_OVERHEAD);
        assert!(n <= MAX_DATAGRAM);
        assert_eq!(dec_raw(&buf[..n]).unwrap().msg, msg);
    }

    #[test]
    fn poison_spectate_name_record() {
        let t = TYPE_SPECTATE_STREAM;
        let good = SPECTATE_STREAM_NAMES_BODY;
        assert!(dec(&spectate_pkt(t, &good)).is_ok());
        let field = |f: &'static str| {
            Err(DecodeError::FieldOutOfRange {
                msg_type: t,
                field: f,
            })
        };
        let mut b = good;
        b[7] = 2;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("seat"));
        b = good;
        b[8] = 33;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("name_len"));
        b = good;
        b[9 + 9] = 0x7A;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("name_tail"));
        b = good;
        b[9 + 1] = 0xFF;
        assert_eq!(dec(&spectate_pkt(t, &b)), Err(DecodeError::InvalidUtf8));
        assert_eq!(
            dec(&spectate_pkt(t, &good[..75])),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: 35,
                actual: 34
            })
        );
    }

    #[test]
    fn encode_clamps_stream_count_over_16() {
        let msg = Message::SpectateStream(SpectateStream {
            first: 1,
            reset: false,
            count: 40,
            records: [Record::Menu { p1: 1, p2: 2 }; MAX_STREAM_RECORDS],
        });
        let bytes = encode_to_vec(SID, &msg);
        assert_eq!(bytes[HEADER_LEN + 5], 16);
        assert_eq!(bytes.len(), HEADER_LEN + 6 + 16 * 5);
        assert!(matches!(
            dec(&bytes).unwrap().msg,
            Message::SpectateStream(s) if s.count == 16
        ));
    }

    #[test]
    fn poison_spectate_join() {
        let t = TYPE_SPECTATE_JOIN;
        assert!(dec(&spectate_pkt(t, &SPECTATE_JOIN_BODY)).is_ok());
        let mut b = SPECTATE_JOIN_BODY;
        for cut in [JOIN_LEN - 1, JOIN_NAME_END] {
            assert_eq!(
                dec(&spectate_pkt(t, &b[..cut])),
                Err(DecodeError::LengthMismatch {
                    msg_type: t,
                    expected: JOIN_LEN,
                    actual: cut
                })
            );
        }
        let mut long = b.to_vec();
        long.push(0);
        assert_eq!(
            dec(&spectate_pkt(t, &long)),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: JOIN_LEN,
                actual: JOIN_LEN + 1
            })
        );
        let field = |f: &'static str| {
            Err(DecodeError::FieldOutOfRange {
                msg_type: t,
                field: f,
            })
        };
        b[0] = 33;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("name_len"));
        b = SPECTATE_JOIN_BODY;
        b[1] = 1;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("reserved"));
        b = SPECTATE_JOIN_BODY;
        b[10 + 5] = b'x'; // name_len = 2 の後ろに 0 でない文字
        assert_eq!(dec(&spectate_pkt(t, &b)), field("name_tail"));
        b = SPECTATE_JOIN_BODY;
        b[10] = 0xFF; // UTF-8 でない
        assert_eq!(dec(&spectate_pkt(t, &b)), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn poison_spectate_ack() {
        let t = TYPE_SPECTATE_ACK;
        for len in [0usize, 12, 19, 21] {
            assert_eq!(
                dec(&spectate_pkt(t, &vec![0u8; len])),
                Err(DecodeError::LengthMismatch {
                    msg_type: t,
                    expected: 20,
                    actual: len
                }),
                "len={len}"
            );
        }
        let mut b = SPECTATE_ACK_BODY;
        b[19] |= 0x80;
        assert_eq!(
            dec(&spectate_pkt(t, &b)),
            Err(DecodeError::FieldOutOfRange {
                msg_type: t,
                field: "sack"
            })
        );
        b[12..20].copy_from_slice(&SACK_MASK.to_le_bytes());
        assert!(matches!(
            dec(&spectate_pkt(t, &b)).unwrap().msg,
            Message::SpectateAck { sack, .. } if sack == SACK_MASK
        ));
    }

    #[test]
    fn poison_spectate_stream() {
        let t = TYPE_SPECTATE_STREAM;
        let good = SPECTATE_STREAM_BODY;
        assert!(dec(&spectate_pkt(t, &good)).is_ok());
        let field = |f: &'static str| {
            Err(DecodeError::FieldOutOfRange {
                msg_type: t,
                field: f,
            })
        };
        assert_eq!(
            dec(&spectate_pkt(t, &good[..5])),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: 6,
                actual: 5
            })
        );
        let mut b = good;
        b[5] = 7;
        assert_eq!(
            dec(&spectate_pkt(t, &b)),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: 1,
                actual: 0
            })
        );
        b = good;
        b[5] = 5;
        assert_eq!(
            dec(&spectate_pkt(t, &b)),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: 49,
                actual: 51
            })
        );
        b = good;
        b[5] = 17;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("count"));
        b = good;
        b[4] = 0x03;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("flags"));
        for k in [0u8, 9, 255] {
            b = good;
            b[6] = k;
            assert_eq!(dec(&spectate_pkt(t, &b)), field("record_kind"), "kind={k}");
        }
        assert_eq!(
            dec(&spectate_pkt(t, &good[..50])),
            Err(DecodeError::LengthMismatch {
                msg_type: t,
                expected: 2,
                actual: 1
            })
        );
        b = good;
        b[10] = 2;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("seat"));
        b = good;
        b[11 + 11] = 1;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("reserved"));
        b = good;
        b[9 + 20 + 3] = 0;
        assert_eq!(dec(&spectate_pkt(t, &b)), field("kind"));
        for r in [0u8, 5] {
            b = good;
            b[50] = r;
            assert_eq!(dec(&spectate_pkt(t, &b)), field("reason"), "reason={r}");
        }
    }

    #[test]
    fn spectate_stream_bodies_never_panic_on_garbage() {
        // 母数: 記録の解きまで届いたもの（Ok か記録のガードで落ちた）が 1 万件を越えることを見る。
        let mut rng = SplitMix64::new(0x0BAD_5EED_0000_0014);
        let mut reached = 0u32;
        for _ in 0..100_000u32 {
            let len = 6 + (rng.next_u64() % 60) as usize;
            let mut body: Vec<u8> = (0..len).map(|_| rng.next_u8()).collect();
            body[4] &= 1;
            body[5] = rng.next_range_u8(MAX_STREAM_RECORDS as u8);
            if body.len() > 6 && rng.next_bool() {
                body[6] = 1 + rng.next_range_u8(6);
            }
            let r = dec(&spectate_pkt(TYPE_SPECTATE_STREAM, &body));
            if matches!(
                r,
                Ok(_)
                    | Err(DecodeError::FieldOutOfRange { .. }
                        | DecodeError::LengthMismatch { .. }
                        | DecodeError::InvalidUtf8)
            ) {
                reached += 1;
            }
        }
        assert!(reached > 10_000, "本文まで届いたのが {reached} 件");
    }

    #[test]
    fn roundtrip_100k_random_messages() {
        let mut rng = SplitMix64::new(0xC0FF_EE12_3456_789A);
        let mut buf = [0u8; MAX_DATAGRAM];
        for i in 0..100_000u32 {
            let session_id = rng.next_u32();
            let msg = random_message(&mut rng);
            let len = enc(session_id, &msg, &mut buf);
            let p = dec_raw(&buf[..len]).unwrap_or_else(|e| {
                panic!("iter {i}: decode に失敗（{e:?}）: {msg:?}");
            });
            assert_eq!(p.session_id, session_id, "iter {i}");
            assert_eq!(p.msg, msg, "iter {i}");
        }
    }

    #[test]
    fn decode_never_panics_on_100k_garbage_inputs() {
        // 半分は先頭を「HGRT・今のバージョン・予約 0」にして認証を付け直す（でたらめのままだと、ほぼ全部が認証のガードか magic のガードで落ち、
        // 本文の解き方を通らない）。
        let mut rng = SplitMix64::new(0x5EED_F00D_1234_5678);
        let mut reached_body = 0u32;
        for i in 0..100_000u32 {
            let len = (rng.next_u64() % 101) as usize; // 0..=100
            let mut bytes = vec![0u8; len];
            for b in bytes.iter_mut() {
                *b = rng.next_u8();
            }
            if i % 2 == 0 && len >= HEADER_LEN {
                bytes[..4].copy_from_slice(b"HGRT");
                bytes[4] = PROTO_VER;
                bytes[5] = rng.next_range_u8(20);
                bytes[6] = 0;
                bytes[7] = 0;
            }
            // パニックしなければ良い。Err/Ok どちらでもよい（`Opener` ありと、鍵の交換の前の両方）。
            let _ = dec_raw(&bytes);
            let _ = decode(&key(), None, &bytes);
            let r = dec(&bytes);
            if !matches!(
                r,
                Err(DecodeError::BadMagic
                    | DecodeError::TooShort { .. }
                    | DecodeError::BadTag
                    | DecodeError::BadVersion(_)
                    | DecodeError::HeaderReserved
                    | DecodeError::UnknownType(_))
            ) {
                reached_body += 1;
            }
        }
        // 母数を言う: 本文の解き方まで届いたもの（通った・本文のガードで落ちた）が 1 万件を越える。
        assert!(
            reached_body > 10_000,
            "本文まで届いたのが {reached_body} 件"
        );
    }
}
