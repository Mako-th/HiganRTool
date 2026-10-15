//! peer_sim の本体: 1 本のスレッドで、1 ms ごとに受信を取り出し、時計を進め、`higanr_core` の状態機械を呼ぶ。
//!
//! 流れ: 接続（`handshake`）→ 選択（`selection`。v0 は画面の模型を持たず、ホストが「決定」を 1 回押し、それを選択を出る押下として
//! 扱う）→ マッチ（`lockstep`。seg ごとに決めたフレーム数）→ 次の seg（選択は 1 回目だけ）→ 終わったら BYE(終了)。繋がった後は、ずっと
//! `liveness` を回す。受けたパケットをどの状態機械へ渡すかは `higanr_core::link` のセッション（dll と同じもの）が決め、ここはセッションを、いつ・どの
//! 語で呼ぶかだけを決める。
//!
//! ## 道具としての決め事
//!
//! - **時計**: 1 ms 刻みの眠りと、1 フレームの枠（既定 60 fps。`RunOpts::fps`）。枠の中で相手の入力ワードが揃わなければ、その枠を stall として
//!   数え、次の枠で同じフレームをやり直す。枠の境目は dll のペース B と同じ「基点 + floor(j × 1 秒 ÷ fps)」（`Slots`）。付いていく形の
//!   マッチは時計の枠を使わない。
//! - **語**: `word(seed, seg, capture_frame)`。0 を避ける（0 は開始直後の穴埋めの語と区別が付かない）。送るのは `word_mask` を掛けた後。
//! - **CHECK**: ゲームの乱数とライフが無いので、それまでに使った (1P, 2P) の列のダイジェスト（FNV-1a 64 bit。seg をまたいで続ける）の
//!   下 16 bit を rng_state、次の 32 bit を rng_count に入れ、ライフは 0 にする（`higanr_core::check`）。
//! - **いつ終えるか**: 全部の seg を終え、相手の BYE を受けてから `LINGER_MS` 待って終える。待つ間も前の seg のバックアップの返信と、相手の
//!   PING への BYE の返しを続ける。相手が 60 秒黙れば（`liveness` の Dead）それでも終える。`max_ms`（`--max-sec`）を過ぎたら途中でも
//!   終え、繋がっていれば BYE(終了) を 1 通出す。
//! - **繋がるまで待つ長さ**（`RunOpts::wait_ms`。`--wait-sec`）: 接続の手順が時間切れ（10 秒）になってもその長さまではセッションが作り直す
//!   （`Link::set_retry_window`）。相手の花映塚の起動が遅い回のため。
//!
//! ## 付いていく形（`SimConfig::follow`）
//!
//! 相手が dll（本物の花映塚）のとき、マッチの長さはゲームが決める。付いていく形では seg の長さを自分で決めず、相手に合わせる:
//! - **フレームを使う条件**: フレーム x を、相手がフレーム x で取った語（apply_frame x + delay）が届いてから使う（dll は場面を出る tick
//!   では語を取らないので、dll が使わないフレームを先に使わない）。語は先に届いていても使わずに待ち、その間も 4 ms ごとに送り直す。
//!   時計の枠は使わず、1 ms ごとに使えるだけ使う（stall は数えない）。
//! - **seg の終わり**: 相手の次の seg の INPUT（`Recv::NextSeg`。再戦）か、選択の外で届いた今の step の MENU（`Recv::PeerSelecting`。
//!   メニューへ戻って次の選択）で、使えるだけ使ってから seg を進める。
//! - **終わり**: 相手の BYE(終了) で、使えるだけ使ってから自分も BYE(終了) を出し、`LINGER_MS` 待って終える。
//! - 相手の入力ワードが落ちると、最後のフレームの入力ワードが届かず、そのフレームを使わずに seg を進めることがある（CHECK が合わなくなって分かる）。
//!   dll から peer_sim への向きは、loopback でわざと壊さない。
//! - **latency の切替**（`--lat`）: ホストなら台本の切替を、そのフレームの取得の前に要求する。**限り**: delay が k 減った切替
//!   （フレーム a）の後は、相手の取得 a+1〜a+k が捨てられて語を作らないので、その k フレームは相手が取る前に使える。相手がその k
//!   フレームの中でマッチを出ると、peer_sim は相手の使わなかったフレームを使い、CHECK が合わなくなって分かる。走らせる側が切替を
//!   マッチの終わりから離して置く。
//!
//! ## rollback のモード（`RunOpts::mode`。`--mode rollback`）
//!
//! 両者が rollback を望んで取り決めで rollback に決まったら、マッチはセッションの `rollback_input_point` / `rollback_post_present` で回す
//! （`Sim::play_rollback`）。peer_sim にはゲームが無いので、ウィンドウは状態を持たない `NoGame` で、場面の境目は「seg の最後のフレームの更新が
//! 次の場面の要求を立てる」とする。記録・観戦・CHECK は**確定した** (frame, 1P, 2P) だけで作る（予測の入力ワードは流さない）ので、列は同じ語の
//! delay 方式の実行と一致する。latency の切替の台本は delay 方式だけ。相手が delay 方式を望めば、delay 方式で走る。
//!
//! 付いていく形の rollback のモード（`--follow --mode rollback`）は、相手が取ったフレームまでを更新する（相手の入力ワードの数 `peer_top` が
//! N + D + 1 に届いてから N を更新する。予測しない。D は seg の途中で替わりうるので `Rollback::peer_top_for`）。seg の最後のフレーム E を
//! ワードの数から出す案は採らなかった: 確定した更新は取り消せず、E を知るのは次の seg の INPUT が届いた後なので、それまでに E + 1〜E + D を
//! 確定させてしまう。dll は待つ間も次のフレームを取る（`rollback_play.rs` の `scan_point_with`）ので、dll が予測で最後のフレームを更新した
//! 回は、付いていく側の記録に E + 1 の行が 1 つ余分に載りうる（推測。実測は無い）。
//!
//! ## 選択の台本（`run_with_menu` の `menu`。`--menu`）
//!
//! 台本の形と進み方は `menu_plan.rs`。画面は core の模型 `menu_model::Model` で追う（繋がったら `Model::new(7)`、付いていく形でマッチから
//! 選択へ戻ったら `Model::new(8)`）。選択のフレームごとに `Runner::frame` → `selection_frame` → 入った押下を全部 `Model::apply` に通す →
//! `Runner::after_frame`。`Model::apply` が `Err` なら（両者の画面が分かれた）`End::MenuDiverged` でやめ、BYE(取消) を出す。`MapDecided`
//! なら模型の値で自分の SELECT を置いて揃うのを待つ。`LeftToMode` なら選択を出て、相手が次の選択に入る合図か、台本の `enter` を待つ
//! （`Phase::Left`）。付いていく形で戻った選択の難易度は模型に無いので、前の選択で決めた値を持ち越して SELECT に載せる。台本なし（`run`）
//! の実行は v0 のまま。

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use higanr_core::auth::Secret;
use higanr_core::candidate::Candidates;
use higanr_core::check::{
    CheckResult, INPUT_DIGEST_INIT, check_from_input_digest, input_digest_push, is_check_frame,
};
use higanr_core::frame::apply_frame;
use higanr_core::guard::{self, Scope};
use higanr_core::handshake::{self, FailReason, ModeWish, Name};
use higanr_core::link::{self, Connected, Link, Outlet, Recv, SelExit};
use higanr_core::liveness;
use higanr_core::lockstep::{self, Applied, CaptureError, LatError, Role, by_seat};
use higanr_core::menu_model::{
    self, APPLY_CLEAR_BITS, Model, Outcome, SCREEN_CHARA, SCREEN_DIFFICULTY, TRANSITION_BITS,
    map_select_byte,
};
use higanr_core::pacing::{DEFAULT_FPS, Grid, MAX_FPS, MIN_FPS};
use higanr_core::rollback::{self, CheckValues, Mode, Tick, TickReport, Window};
use higanr_core::selection::{self, Agreed, Press};
use higanr_core::stun::{Query, QueryState, TransactionId};
use higanr_core::wire::{self, Datagram};
use higanr_core::wire::{ByeReason, Check, FormatValue, Record, Seat, Select, Step, StepKind};

use crate::fault::{Fate, FaultConfig, FaultCounts, Faults, Relabel};
use crate::menu_plan::{FrameIn, MenuPlan, Runner, SimCursor, sim_align_press};
use crate::spectate_host::{self, SpectateHostOpts, SpectateHostReport, SpectateSide};

const US_PER_SEC: u64 = 1_000_000;

/// 走らせ方の選択肢のうち、`SimConfig` の欄にしないもの。`SimConfig` を欄ごとに書いて作る所（peer_sim と dll の loopback の検査）を
/// 変えずに足せるよう、引数で渡す。
#[derive(Debug, Clone, Copy)]
pub struct RunOpts<'a> {
    /// 選択の台本（`menu_plan.rs`。None なら v0 の選択）。
    pub menu: Option<&'a MenuPlan>,
    /// 1 秒のフレーム数（既定 `DEFAULT_FPS` = 60。範囲は dll の `[pacing] fps` と同じ）。付いていく形のマッチは使わない。
    pub fps: u64,
    /// 接続の手順を作り直して待ち続ける長さ（ms。`--wait-sec`）。None なら置かない（10 秒でやめる。既定）。
    pub wait_ms: Option<u32>,
    /// ネットワークの範囲（`--scope`。既定 loopback。`higanr_core::guard`）。受信ソケット・相手の候補・STUN の宛先と、送る直前のガードが使う。
    pub scope: Scope,
    /// 観戦のポート（`--spectate-host`）。Some なら、ホストの役のとき、使うと決まった記録を観戦者へ流す。
    pub spectate: Option<&'a SpectateHostOpts>,
    /// 望むモードと D（`--mode` / `--rb-delay`。`Link::set_mode` へ）。既定は delay 方式。取り決めで rollback に決まったら、マッチは
    /// `core::rollback` の状態機械で回す。
    pub mode: ModeWish,
    /// rollback に決まったときの R（`--rb-ahead`。1〜16。ネットワークに載せない）。
    pub ahead: u8,
    /// rollback に決まったときの時刻合わせ（`--rb-sync on|off`）。既定は使う。切った実行と比べるため。
    pub time_sync: bool,
}

impl Default for RunOpts<'_> {
    fn default() -> Self {
        RunOpts {
            menu: None,
            fps: DEFAULT_FPS,
            wait_ms: None,
            scope: Scope::default(),
            spectate: None,
            mode: ModeWish::DELAY,
            ahead: rollback::DEFAULT_AHEAD,
            time_sync: true,
        }
    }
}

/// フレームの枠の並び（µs の時計の上）。j 番目の枠は `base_us + floor(j × 1 秒 ÷ fps)` から始まる（dll のペース B と同じ `Grid`）。
/// 前の枠に 1 周期を足して作らない（丸めた端数が積もる）。
#[derive(Debug, Clone, Copy)]
struct Slots {
    /// 基点 0 の格子（`fps` が決める。基点は `base_us` で足す）。
    grid: Grid,
    base_us: u64,
    j: u64,
}

impl Slots {
    fn new(grid: Grid, base_us: u64) -> Slots {
        Slots {
            grid,
            base_us,
            j: 0,
        }
    }

    fn start(&self) -> u64 {
        self.base_us.saturating_add(self.grid.at(self.j))
    }

    fn end(&self) -> u64 {
        self.base_us
            .saturating_add(self.grid.at(self.j.saturating_add(1)))
    }

    fn advance(&mut self) {
        self.j = self.j.saturating_add(1);
    }
}

/// `fps` が範囲（`MIN_FPS`〜`MAX_FPS`）の中か。
pub fn check_fps(fps: u64) -> Result<(), String> {
    if (MIN_FPS..=MAX_FPS).contains(&fps) {
        Ok(())
    } else {
        Err(format!("fps は {MIN_FPS}〜{MAX_FPS}（{fps} は範囲の外）"))
    }
}

fn frame_grid(fps: u64) -> Result<Grid, SimError> {
    check_fps(fps).map_err(SimError::Refused)?;
    Grid::new(0, US_PER_SEC, fps)
        .ok_or_else(|| SimError::Refused(format!("fps {fps} で枠が刻めない")))
}

/// 全部を終え、相手の BYE を受けてから終えるまで待つ時間（ms）。相手は PING を 1 秒ごとに送ってきて、閉じた自分はそれに BYE を返す
/// ので、PING 2 回分の機会を待つ。
pub const LINGER_MS: u32 = 2 * liveness::PING_INTERVAL_MS + 500;

/// latency の切替の台本の 1 行（ホストのとき。「フレーム f で delay を n に」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatStep {
    pub seg: u16,
    /// その seg の中の capture_frame（この取得の前に `request_delay` する）。
    pub frame: u32,
    pub delay: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// 相手の候補のうち最初のもの（`extra_peers` と合わせて候補の列）。
    pub peer: SocketAddr,
    /// 相手の候補の残り（全部へ HELLO を送り、最初に返しが来た経路を採る。空なら `peer` だけ）。
    pub extra_peers: Vec<SocketAddr>,
    /// 接続の秘密（両者で同じ値。既定は `cli::DEFAULT_SECRET`）。
    pub secret: Secret,
    /// STUN の問い合わせ先（ネットワークの範囲 `RunOpts::scope` の中だけ。None なら問い合わせない。答えは記録に出すだけで、接続は待たない）。
    pub stun: Option<SocketAddr>,
    pub name: String,
    pub session_id: u32,
    pub role_nonce: u64,
    /// 希望 latency（0〜9）。
    pub latency: u8,
    /// ホストになったときに座る席。
    pub host_seat: Seat,
    pub seed: u64,
    /// 相手のマッチの長さに付いていく（モジュール doc の「付いていく形」）。true なら `segs` と
    /// `frames_per_seg` は使わない（`lat_script` の seg は相手と同じ数え方）。
    pub follow: bool,
    /// 送る語に掛けるマスク（既定 0xFFFF）。本物の花映塚を相手にするときは、ゲームの画面を動かすビット（Z・Esc・上下）を落とす。
    /// 0 になった語もそのまま送る。
    pub word_mask: u16,
    /// 自分の SELECT の初期ライフ（1〜10。dll は相手の席へ書く）。
    pub life: u8,
    /// 受信を取り出して時計で動くものを回す間隔（ms。既定 1。0 は 1 と読む）。検査が、受けたものを何通か溜めてから渡す形を作るのに使う。
    pub poll_ms: u32,
    pub segs: u32,
    pub frames_per_seg: u32,
    /// seg に入る前の読み込みの長さ（ms。両者で違う値にすると、先に読み込みを終えた側の INPUT を
    /// 相手が読み込み中に受ける形になる）。
    pub load_ms: u32,
    pub lat_script: Vec<LatStep>,
    pub fault: FaultConfig,
    /// これを過ぎたら終わっていなくてもやめる（ms）。検査が止まらないように。
    pub max_ms: u32,
}

/// フレームごとの記録（(seg, frame, 1P, 2P)。使った後の delay も添える）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rec {
    pub seg: u16,
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
    pub delay: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FollowCounts {
    /// 相手の次の seg の INPUT で進めた（再戦）。
    pub next_seg: u32,
    /// 相手が次の選択に入ったので進めた（メニューへ戻った）。
    pub peer_selecting: u32,
    /// マッチの外で届いた合図（使わなかった）。
    pub ignored: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// 全部の seg を終えた（付いていく形では、相手の BYE(終了) を受けて終えた）。`peer_bye` は相手の BYE を
    /// 受けたか（false なら相手が 60 秒黙った）。
    Finished { peer_bye: bool },
    /// 接続の手順でやめた。
    HandshakeFailed(handshake::FailReason),
    /// 終える前に、相手から BYE(終了) 以外の BYE を受けた。
    PeerBye(ByeReason),
    /// 終える前に、相手が 60 秒黙った（BYE(切断) を送った）。
    Dead,
    /// CHECK が合わなかった（BYE(ズレ検出) を送った）。
    Desync { local: Check, remote: Check },
    /// 選択の台本の実行で、入った押下が模型に合わなかった（両者の画面が分かれた。BYE(取消) を送った）。
    MenuDiverged {
        step: Step,
        mismatch: menu_model::Mismatch,
    },
    /// `max_ms` を過ぎた。繋がっていて、まだ閉じていなければ、BYE(終了) を 1 通出してから終える
    /// （`Report::deadline_bye`。`Sim::finish` の注）。
    Deadline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuReport {
    pub items: usize,
    pub done: usize,
    /// ゲストの要求が捨てられて押し直した数（`menu_plan::RETRY_FRAMES` 待ってから）。
    pub retries: u32,
    /// 入った選択の数（繋がったとき・選択の外から入り直したとき・マッチから戻ったとき）。
    pub selections: u32,
    /// 難易度の画面の取消で選択の外へ出た数。
    pub left_to_mode: u32,
    /// 終わりの模型（最後に入った選択の。難易度・マップ・両席の決めた値）。繋がらなければ None。
    pub model: Option<Model>,
    /// 相手の入力ワード（`FrameOut::remote`）が変わったところ（その入力ワードを使った step, 入力ワード）。
    pub remote_words: Vec<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatResult {
    pub seg: u16,
    pub frame: u32,
    pub delay: u8,
    pub result: Result<(), LatError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// セッションの受信ソケットで数えたもの（相手のアドレス以外・解けない・session_id 違い・繋がる前）。
    pub recv: link::RecvCounts,
    /// 繋がった後にセッションが数えたもの（INPUT の扱い・取得の溢れ・CHECK の一致。繋がらなければ 0）。
    pub conn: link::ConnCounts,
    pub sends: u64,
    pub send_errors: u64,
    /// ネットワークの範囲の外の宛先へ送ろうとして止めたもの（ガード。セッションは候補と送り元にしか送らないので、0 のはず）。
    pub send_refused: u64,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub end: End,
    pub role: Option<Role>,
    pub seat: Option<Seat>,
    pub delay_start: Option<u8>,
    pub mode: Option<Mode>,
    pub peer_name: String,
    pub agreed: Option<Agreed>,
    pub recs: Vec<Rec>,
    /// stall（相手の入力ワードが枠までに揃わなかった枠）の総数と最大連続。
    pub stalls: u32,
    pub max_stall_run: u32,
    /// 相手の入力ワードが届いてから、そのフレームを使うまで（µs）。フレームの順。
    pub arrival_to_apply_us: Vec<u64>,
    pub check_matches: u32,
    pub rtt_ms: Option<u32>,
    pub lat_results: Vec<LatResult>,
    pub counts: Counts,
    pub faults: FaultCounts,
    pub handshake: handshake::Counters,
    pub selection: Option<selection::Counters>,
    pub lockstep: Option<lockstep::Stats>,
    /// rollback のモードに決まったときだけ Some（予測・進め直し・待ちの数）。
    pub rollback: Option<rollback::Stats>,
    pub liveness: Option<liveness::Counters>,
    pub check_dropped: u32,
    pub follow: Option<FollowCounts>,
    /// 入った画面を移る押下（自分のでも相手のでも。入った順）。台本なしの v0 では画面 0 の「決定」1 つ。
    pub presses: Vec<Step>,
    pub menu: Option<MenuReport>,
    /// STUN の問い合わせ（`SimConfig::stun` を渡したときだけ Some）の終わりの状態。
    pub stun: Option<QueryState>,
    pub path: Option<SocketAddr>,
    /// 走らせたペース（`RunOpts::fps`）と、送りに足した一定の遅れ（`FaultConfig::one_way_ms`）。要約に出す。
    pub fps: u64,
    pub one_way_ms: u32,
    /// ネットワークの上で名乗ったバージョン（`FaultConfig::claim_ver`。無ければ今のバージョン `wire::PROTO_VER`）。バージョン違いの要約の「自分」。
    pub proto_ver: u8,
    /// 接続の手順を作り直して待つ長さ（`RunOpts::wait_ms`。None なら置いていない）と、作り直した回数・直前の 1 回。
    pub retry_window_ms: Option<u32>,
    pub restarts: link::Restarts,
    pub max_ms: u32,
    pub spectate: Option<SpectateHostReport>,
    /// `End::Deadline` で終えたとき、打ち切りの時点で BYE(終了) を出したか（繋がる前と、もう閉じていた・切れていたときは出さない）。
    pub deadline_bye: bool,
    pub elapsed_ms: u32,
}

#[derive(Debug)]
pub enum SimError {
    /// ネットワークの範囲の外の宛先・受信ソケット、設定の誤り。
    Refused(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SimError::Refused(s) => write!(f, "{s}"),
            SimError::Io(e) => write!(f, "ソケットの操作に失敗: {e}"),
        }
    }
}

impl From<std::io::Error> for SimError {
    fn from(e: std::io::Error) -> Self {
        SimError::Io(e)
    }
}

pub fn word(seed: u64, seg: u16, capture_frame: u32) -> u16 {
    // seed を先にかき混ぜてからフレームを混ぜる（先に XOR すると、近い seed の列が 1 フレームずれて重なる）。
    let z = mix(mix(seed) ^ (u64::from(seg) << 32) ^ u64::from(capture_frame));
    match z as u16 {
        0 => 1,
        w => w,
    }
}

pub(crate) fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 自分の SELECT（v0 はゲームの値に意味が無い。マップと難易度は両者で同じ値にして食い違いを出さない）。
fn own_select(cfg: &SimConfig) -> Select {
    Select {
        match_no: 0,
        chara: (cfg.seed % 16) as u8,
        color: 0,
        life: cfg.life,
        charge: 0,
        map: 0,
        difficulty: 0,
        cpu_1p: 0,
        cpu_2p: 0,
        background: 0,
        rng_seed: cfg.seed as u16,
        option_digest: 0,
    }
}

fn seat_index(s: Seat) -> usize {
    match s {
        Seat::P1 => 0,
        Seat::P2 => 1,
    }
}

/// 選択の台本の実行で、マップが決まったときの自分の SELECT。キャラ・色は模型の自分の席の値、マップは一覧の値を SELECT の形
/// （−1 = 0xFF / −2 = 0xFE）に、難易度は模型の値（無ければ前の選択の値 `carried`）。シードはホストだけ。
fn menu_select(
    cfg: &SimConfig,
    role: Role,
    seat: usize,
    model: &Model,
    carried: Option<u8>,
) -> Select {
    let d = model.decided(seat).unwrap_or_default();
    Select {
        match_no: 0,
        chara: d.chara,
        color: d.color,
        life: cfg.life,
        charge: 0,
        map: map_select_byte(model.map_item().unwrap_or(0)),
        difficulty: model.difficulty().or(carried).unwrap_or(0),
        cpu_1p: 0,
        cpu_2p: 0,
        background: 0,
        rng_seed: if role == Role::Host {
            cfg.seed as u16
        } else {
            0
        },
        option_digest: 0,
    }
}

/// 送る側（ソケット・わざと壊す・遅らせた分）。受ける側と別に借りられるよう分けた。観戦のポートも同じ送信側を使う（わざと壊す・ネットワークの範囲の
/// ガードを 2 か所に書かない）。
pub(crate) struct Tx {
    pub(crate) sock: UdpSocket,
    pub(crate) faults: Faults,
    /// 遅らせたもの（送る時刻 ms, 宛先, バイト列）。送った順に並ぶ。
    delayed: Vec<(u32, SocketAddr, Vec<u8>)>,
    pub(crate) sends: u64,
    pub(crate) send_errors: u64,
    pub(crate) send_refused: u64,
    /// ネットワークの範囲（送る直前のガード）。
    scope: Scope,
    /// 別のバージョンを名乗るときだけ Some（対戦の受信ソケットだけ。観戦のポートは名乗り替えない）。
    relabel: Option<Relabel>,
}

impl Tx {
    pub(crate) fn new(sock: UdpSocket, fault: FaultConfig, scope: Scope) -> Tx {
        Tx {
            sock,
            faults: Faults::new(fault),
            delayed: Vec::new(),
            sends: 0,
            send_errors: 0,
            send_refused: 0,
            scope,
            relabel: None,
        }
    }

    /// セッションが作った 1 データグラムを、わざと壊す設定に従って `to` へ送る。一定の遅れは運命の後に、届けるコピーの全部へ足す。
    pub(crate) fn send(&mut self, now: u32, to: SocketAddr, bytes: &[u8]) {
        // 名乗り替えは運命より前（落とす・遅らせる・重ねるのは名乗り替えた物）。
        let relabeled = self.relabel.as_ref().and_then(|r| r.apply(bytes));
        let bytes = relabeled.as_deref().unwrap_or(bytes);
        let lag = self.faults.one_way_ms();
        match self.faults.decide() {
            Fate::Deliver => self.send_after(now, lag, to, bytes),
            Fate::Drop => {}
            Fate::Delay(ms) => self.send_after(now, lag.saturating_add(ms), to, bytes),
            Fate::Dup(ms) => {
                self.send_after(now, lag, to, bytes);
                self.send_after(now, lag.saturating_add(ms), to, bytes);
            }
        }
    }

    /// `after_ms` の後に送る（0 ならすぐ）。遅らせたものは `flush` が、遅らせた順に送る。
    fn send_after(&mut self, now: u32, after_ms: u32, to: SocketAddr, bytes: &[u8]) {
        // 揺らぎはコピー 1 通ごとに引く（送り直しのコピーにも別々に掛かる）。
        let after_ms = after_ms.saturating_add(self.faults.jitter());
        if after_ms == 0 {
            self.raw(to, bytes);
        } else {
            self.delayed
                .push((now.wrapping_add(after_ms), to, bytes.to_vec()));
        }
    }

    /// 1 通を送る。宛先はセッションが候補か送り元から選んだもの（起動時にネットワークの範囲の中かを確かめてある）だが、送る直前にも範囲の中かを見る。
    fn raw(&mut self, to: SocketAddr, bytes: &[u8]) {
        if !self.may_send(to) {
            self.send_refused += 1;
            return;
        }
        self.sends += 1;
        if self.sock.send_to(bytes, to).is_err() {
            self.send_errors += 1;
        }
    }

    /// 送る直前のガード（`raw` が使う。検査はこれを直に見る）。
    fn may_send(&self, to: SocketAddr) -> bool {
        guard::allows(self.scope, to)
    }

    pub(crate) fn flush(&mut self, now: u32) {
        let mut due = Vec::new();
        self.delayed.retain(|(at, to, b)| {
            // 差で比べる（u32 の ms の折り返しでも正しい）。遅れは高々 `MAX_ONE_WAY_MS` ＋ 乱数の幅で、i32 の範囲に収まる。
            if (now.wrapping_sub(*at) as i32) >= 0 {
                due.push((*to, b.clone()));
                false
            } else {
                true
            }
        });
        for (to, b) in due {
            self.raw(to, &b);
        }
    }
}

pub(crate) struct At<'a> {
    pub(crate) tx: &'a mut Tx,
    pub(crate) now: u32,
}

impl Outlet for At<'_> {
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) {
        self.tx.send(self.now, to, bytes);
    }
}

/// 相手の候補の列（`peer` ＋ `extra_peers`）。どれかがネットワークの範囲 `scope` の外なら断る。
fn candidates(cfg: &SimConfig, scope: Scope) -> Result<Candidates, SimError> {
    let mut all = vec![cfg.peer];
    all.extend_from_slice(&cfg.extra_peers);
    for &a in &all {
        guard::check_peer(a, scope).map_err(SimError::Refused)?;
    }
    Candidates::from_addrs(&all)
        .map_err(|e| SimError::Refused(format!("相手の候補 {all:?} が並べられない: {e:?}")))
}

/// STUN の transaction id（96 bit）。peer_sim は OS の乱数を持たない（std だけ）ので、時刻と role_nonce から作る。loopback の偽の応答役
/// にしか送らないので、推し量られても害は無い。
fn stun_txn(cfg: &SimConfig) -> TransactionId {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let a = mix(t ^ cfg.role_nonce);
    let b = mix(a ^ u64::from(std::process::id()));
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes()[..4]);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Selecting,
    /// 自分は選択を出た。両者の SELECT が揃うのを待つ。
    WaitSelect,
    /// 難易度の画面の取消で選択の外へ出た（選択の台本の実行だけ）。相手が次の選択に入るか、台本の `enter` を待つ。
    Left,
    /// seg に入る前の読み込み（この ms まで）。
    Loading(u32),
    Playing,
    Done,
}

/// 繋がった後の、道具の側の状態（状態機械はセッションの `Connected` が持つ）。
struct Play {
    phase: Phase,
    seg_idx: u32,
    grid: Grid,
    /// 選択のフレームの枠（今の枠の始まりが来たら 1 フレーム回して次の枠へ）。
    sel: Slots,
    // ---- マッチ ----
    slot: Slots,
    slot_advanced: bool,
    /// rollback のモードで、この枠は時刻合わせの待ち（`Wait::Sync`）を返された。枠の終わりまで入力の地点を試さない。
    slot_sync_wait: bool,
    want_capture: bool,
    stall_run: u32,
    digest: u64,
    /// (seg, apply_frame) → 相手の入力ワードが最初に届いた時刻（µs）。
    arrivals: HashMap<(u16, u32), u64>,
    lat_done: Vec<bool>,
    /// 相手の BYE を最初に見た時刻（ms）。
    peer_bye_at: Option<u32>,
    menu: Option<MenuRun>,
    // ---- 観戦（`spectate_host`）。ホストの役で観戦のポートがあるときだけ使う ----
    /// 観戦者へ流す記録を積むか（観戦のポートがあり、役がホスト）。
    sp_on: bool,
    /// 積む前の記録（各 tick の終わりに `spectate_host` のリングバッファへ渡す。関数ごとの借りを分けるための待ち行列）。
    sp_q: Vec<Record>,
    /// 自分の SELECT（止める地点で置いた値。観戦者へ両席ぶんの SELECT を流すときの元）。
    own_select: Option<Select>,
    win: NoGame,
    /// 今の seg で、相手の INPUT から見た一番先の位置（届いた INPUT の `base + 語数` の最大 = 相手が作った語の数）。付いていく形の
    /// rollback のモードだけが使う。seg ごとに 0 へ。
    peer_top: u32,
}

/// rollback のモードのウィンドウ（`rollback::Window`）。ゲームが無いので状態を持たず、コピーも書き戻しも何もしない。場面の境目は「seg の最後の
/// フレーム（`last`）の更新が次の場面の要求を立てる」とする。
///
/// 確定した (frame, 1P, 2P) は `rows` に積み、呼び手が delay 方式と同じ記録・CHECK の入口へ流す（`Sim::record_row`）。状態機械が取る
/// CHECK（`Window::check`）は使わない（peer_sim の CHECK は使った列のダイジェスト）。
#[derive(Debug, Default)]
struct NoGame {
    last: u32,
    rows: Vec<(u32, u16, u16)>,
}

impl Window for NoGame {
    fn save(&mut self, _frame: u32) {}
    fn restore(&mut self, _frame: u32) {}
    fn tick(&mut self, t: Tick) -> TickReport {
        TickReport {
            boundary: t.frame == self.last,
        }
    }
    fn check_values(&mut self) -> CheckValues {
        CheckValues::default()
    }
    fn confirmed(&mut self, frame: u32, p1: u16, p2: u16) {
        self.rows.push((frame, p1, p2));
    }
}

impl Play {
    fn spec(&mut self, r: Record) {
        if self.sp_on {
            self.sp_q.push(r);
        }
    }

    /// 観戦者へ: 新しい seg の始まり（`Seg` と、その直後の席ごとの名前。dll の `spec_seg` と同じ `spectate::seg_head`）。
    fn spec_seg(&mut self, seg: u16, c: &Connected) {
        for r in higanr_core::spectate::seg_head(seg, &c.established()) {
            self.spec(r);
        }
    }
}

struct MenuRun {
    runner: Runner,
    model: Model,
    /// 前の選択で決めた難易度（付いていく形でマッチから戻った選択の模型は難易度を持たない）。
    difficulty: Option<u8>,
    /// 直前の相手の入力ワード（変わったところだけ記録する）。
    last_remote: u16,
    /// 自分の席のキャラのカーソル（MENU の末尾で知らせる）。
    cursor: SimCursor,
}

struct Sim<'c> {
    cfg: &'c SimConfig,
    menu_plan: Option<&'c MenuPlan>,
    grid: Grid,
    t0: Instant,
    tx: Tx,
    link: Link,
    play: Option<Play>,
    /// STUN の問い合わせ（`SimConfig::stun` のとき）。接続と並べて走らせ、接続は答えを待たない。
    stun: Option<(SocketAddr, Query)>,
    spectate: Option<SpectateSide>,
    rep: Report,
}

/// 1 回走らせる。`sock` は呼び手が開いた UDP ソケット。宛先と受信ソケットがネットワークの範囲の外なら、1 通も送らずに `SimError::Refused` を返す。
pub fn run(cfg: &SimConfig, sock: UdpSocket) -> Result<Report, SimError> {
    run_with_menu(cfg, sock, None)
}

/// `run` に選択の台本を足したもの。`menu` が None なら `run` と同じ。ペースは 60 fps。
pub fn run_with_menu(
    cfg: &SimConfig,
    sock: UdpSocket,
    menu: Option<&MenuPlan>,
) -> Result<Report, SimError> {
    run_opts(
        cfg,
        sock,
        RunOpts {
            menu,
            ..RunOpts::default()
        },
    )
}

pub fn run_opts(cfg: &SimConfig, sock: UdpSocket, opts: RunOpts) -> Result<Report, SimError> {
    let menu = opts.menu;
    let grid = frame_grid(opts.fps)?;
    let local = sock.local_addr()?;
    guard::check_bind(local, opts.scope, "受信ソケット").map_err(SimError::Refused)?;
    let peers = candidates(cfg, opts.scope)?;
    if let Some(s) = cfg.stun {
        guard::check_peer(s, opts.scope).map_err(SimError::Refused)?;
    }
    cfg.fault.validate().map_err(SimError::Refused)?;
    if !(1..=10).contains(&cfg.life) {
        return Err(SimError::Refused("初期ライフは 1〜10".to_string()));
    }
    if !cfg.follow && (cfg.segs == 0 || cfg.frames_per_seg == 0) {
        return Err(SimError::Refused(
            "seg の数と seg のフレーム数は 1 以上".to_string(),
        ));
    }
    // rollback を望む実行は、latency の切替の台本を持たない（どちらで走ったかで引数の意味が変わらないように）。
    if opts.mode.mode == Mode::Rollback && !cfg.lat_script.is_empty() {
        return Err(SimError::Refused(
            "rollback を望む実行は --lat と一緒に使わない（latency の切替は delay 方式だけ）"
                .to_string(),
        ));
    }
    let spectate = opts
        .spectate
        .map(|o| SpectateSide::open(o, opts.scope))
        .transpose()?;
    sock.set_nonblocking(true)?;
    let mut link = Link::new(
        link::Config {
            handshake: handshake::Config {
                session_id: cfg.session_id,
                role_nonce: cfg.role_nonce,
                latency: cfg.latency,
                name: Name::new(&cfg.name),
                host_seat: cfg.host_seat,
            },
            secret: cfg.secret,
            peers,
            key_seed: crate::keys::fresh_seed(),
        },
        0,
    );
    link.set_mode(opts.mode, opts.ahead);
    link.set_time_sync(opts.time_sync);
    // 書かなければ置かない（呼ばない形を既定の実行として保つ）。
    if let Some(w) = opts.wait_ms {
        link.set_retry_window(w);
    }
    let mut sim = Sim {
        cfg,
        menu_plan: menu,
        grid,
        t0: Instant::now(),
        tx: Tx {
            sock,
            faults: Faults::new(cfg.fault),
            delayed: Vec::new(),
            sends: 0,
            send_errors: 0,
            send_refused: 0,
            scope: opts.scope,
            relabel: cfg
                .fault
                .claim_ver
                .map(|v| Relabel::new(&cfg.secret, cfg.session_id, v)),
        },
        link,
        play: None,
        stun: cfg.stun.map(|s| (s, Query::new(stun_txn(cfg)))),
        spectate,
        rep: Report {
            end: End::Deadline,
            role: None,
            seat: None,
            delay_start: None,
            mode: None,
            peer_name: String::new(),
            agreed: None,
            recs: Vec::new(),
            stalls: 0,
            max_stall_run: 0,
            arrival_to_apply_us: Vec::new(),
            check_matches: 0,
            rtt_ms: None,
            lat_results: Vec::new(),
            counts: Counts::default(),
            faults: FaultCounts::default(),
            handshake: handshake::Counters::default(),
            selection: None,
            lockstep: None,
            rollback: None,
            liveness: None,
            check_dropped: 0,
            follow: cfg.follow.then(FollowCounts::default),
            presses: Vec::new(),
            menu: menu.map(|m| MenuReport {
                items: m.items.len(),
                done: 0,
                retries: 0,
                selections: 0,
                left_to_mode: 0,
                model: None,
                remote_words: Vec::new(),
            }),
            stun: None,
            path: None,
            fps: opts.fps,
            one_way_ms: cfg.fault.one_way_ms,
            proto_ver: cfg.fault.claim_ver.unwrap_or(higanr_core::wire::PROTO_VER),
            retry_window_ms: opts.wait_ms,
            restarts: link::Restarts::default(),
            max_ms: cfg.max_ms,
            spectate: None,
            deadline_bye: false,
            elapsed_ms: 0,
        },
    };
    let end = sim.main_loop()?;
    Ok(sim.finish(end))
}

impl Sim<'_> {
    fn now_us(&self) -> u64 {
        self.t0.elapsed().as_micros() as u64
    }

    fn now_ms(&self) -> u32 {
        (self.now_us() / 1_000) as u32
    }

    fn main_loop(&mut self) -> Result<End, SimError> {
        let mut buf = [0u8; 2048];
        loop {
            let now = self.now_ms();
            if now >= self.cfg.max_ms {
                return Ok(End::Deadline);
            }
            loop {
                match self.tx.sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        let bytes = buf.get(..n).unwrap_or(&[]);
                        // STUN の答えは問い合わせへ（受信ソケットで分ける）。問い合わせていなければセッションが数えて捨てる。
                        if wire::classify(bytes) == Datagram::Stun
                            && let Some((server, q)) = self.stun.as_mut()
                            && from == *server
                        {
                            q.on_datagram(bytes);
                            continue;
                        }
                        if let Some(end) = self.on_datagram(from, bytes) {
                            return Ok(end);
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    // 前に送った先のポートが閉じている（相手が先に終えた）と次の受信が WSAECONNRESET になる。読み飛ばす。
                    Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                    Err(e) => return Err(SimError::Io(e)),
                }
            }
            let ended = self.tick();
            let now = self.now_ms();
            if let Some(s) = self.spectate.as_mut() {
                s.pump(now)?;
            }
            if let Some(end) = ended {
                return Ok(end);
            }
            if let Some((server, q)) = self.stun.as_mut()
                && let Some(req) = q.poll_send(now)
            {
                // 問い合わせはわざと壊さない（壊すのは相手への送りだけ）。
                self.tx.raw(*server, &req);
            }
            self.tx.flush(now);
            std::thread::sleep(Duration::from_millis(u64::from(self.cfg.poll_ms.max(1))));
        }
    }

    fn on_datagram(&mut self, from: SocketAddr, bytes: &[u8]) -> Option<End> {
        // 別のバージョンを名乗っているなら、受けたものも入れ替えてからセッションへ。
        let relabeled = self.tx.relabel.as_ref().and_then(|r| r.apply(bytes));
        let bytes = relabeled.as_deref().unwrap_or(bytes);
        let now = self.now_ms();
        let now_us = self.now_us();
        let r = self.link.on_datagram(
            from,
            bytes,
            now,
            &mut At {
                tx: &mut self.tx,
                now,
            },
        );
        self.maybe_start();
        match r {
            Recv::Nothing => None,
            Recv::NextSeg { .. } => {
                self.follow_signal(false, now, now_us);
                None
            }
            Recv::PeerSelecting => {
                if self.play.as_ref().map(|p| p.phase) == Some(Phase::Left) {
                    // 選択の外で待っていた（選択の台本の実行）。相手が先に次の選択に入った。
                    if let (Some(c), Some(p)) = (self.link.connected_mut(), self.play.as_mut())
                        && let Some(m) = p.menu.as_mut()
                    {
                        m.runner.peer_entered();
                        Self::enter_menu(c, p, &mut self.rep, SCREEN_DIFFICULTY);
                    }
                } else {
                    self.follow_signal(true, now, now_us);
                }
                None
            }
            Recv::Input {
                seg,
                base,
                words,
                frame,
            } => {
                // 「到着 → 適用」の到着: まだ使っていないフレームの語が最初に届いた時刻。
                if let Some(p) = self.play.as_mut() {
                    p.peer_top = p.peer_top.max(base.saturating_add(words));
                    for k in 0..words {
                        let Some(x) = base.checked_add(k) else {
                            break;
                        };
                        if x >= frame {
                            p.arrivals.entry((seg, x)).or_insert(now_us);
                        }
                    }
                }
                None
            }
            Recv::Desync { local, remote } => Some(End::Desync { local, remote }),
        }
    }

    /// 付いていく形の seg の終わりの合図（`to_select` は相手が次の選択に入った、false は次の seg の INPUT）。使えるだけ使ってから
    /// seg を進める。
    fn follow_signal(&mut self, to_select: bool, now: u32, now_us: u64) {
        let cfg = self.cfg;
        if !cfg.follow {
            return;
        }
        let (Some(c), Some(p)) = (self.link.connected_mut(), self.play.as_mut()) else {
            return;
        };
        let playing = p.phase == Phase::Playing;
        if playing {
            Self::follow_play(c, p, cfg, &mut self.tx, &mut self.rep, now, now_us);
            Self::end_followed_seg(c, p);
            if to_select {
                p.sel = Slots::new(p.grid, now_us);
                if p.menu.is_some() {
                    // マッチから戻ると押下なしでキャラ選択まで進む。台本は続きの項目から。
                    Self::enter_menu(c, p, &mut self.rep, SCREEN_CHARA);
                } else {
                    c.enter_selection();
                    p.phase = Phase::Selecting;
                }
            } else {
                p.phase = Phase::Loading(now.wrapping_add(cfg.load_ms));
            }
        }
        if let Some(f) = self.rep.follow.as_mut() {
            let n = match (playing, to_select) {
                (false, _) => &mut f.ignored,
                (true, true) => &mut f.peer_selecting,
                (true, false) => &mut f.next_seg,
            };
            *n = n.saturating_add(1);
        }
    }

    /// 付いていく形で今の seg を終える（相手は先に出ている。前の seg のバックアップの返信は続ける）。
    fn end_followed_seg(c: &mut Connected, p: &mut Play) {
        let seg = c.lockstep().seg();
        c.begin_seg(seg.wrapping_add(1));
        // 観戦者へ: 次の seg の始まり。後ろに続く選択の押下・SELECT や再戦の語はこの seg のもの（dll の `leave_seg` と同じ並び）。
        p.spec_seg(seg.wrapping_add(1), c);
        p.arrivals.clear();
        p.peer_top = 0;
        p.seg_idx += 1;
    }

    /// セッションが繋がったら、道具の側の状態を作り、選択に入る（v0 は繋がったらすぐ選択の画面のつもり）。
    fn maybe_start(&mut self) {
        if self.play.is_some() {
            return;
        }
        let now_us = self.now_us();
        let Some(c) = self.link.connected_mut() else {
            return;
        };
        let e = c.established();
        self.rep.role = Some(e.role);
        self.rep.seat = Some(e.seat);
        self.rep.delay_start = Some(e.delay);
        self.rep.mode = Some(e.mode);
        self.rep.peer_name = e.peer_name.as_str().to_string();
        let mut p = Play {
            phase: Phase::Selecting,
            seg_idx: 0,
            grid: self.grid,
            sel: Slots::new(self.grid, now_us),
            slot: Slots::new(self.grid, 0),
            slot_advanced: false,
            slot_sync_wait: false,
            want_capture: false,
            stall_run: 0,
            digest: INPUT_DIGEST_INIT,
            arrivals: HashMap::new(),
            lat_done: vec![false; self.cfg.lat_script.len()],
            peer_bye_at: None,
            menu: self.menu_plan.map(|plan| MenuRun {
                runner: Runner::new(plan),
                model: Model::new(SCREEN_DIFFICULTY),
                difficulty: None,
                last_remote: 0,
                cursor: SimCursor::new(seat_index(e.seat)),
            }),
            sp_on: self.spectate.is_some() && e.role == Role::Host,
            sp_q: Vec::new(),
            own_select: None,
            win: NoGame {
                // 付いていく形は seg の終わりを相手の合図で決める（境目は立てない）。
                last: if self.cfg.follow {
                    u32::MAX
                } else {
                    self.cfg.frames_per_seg.saturating_sub(1)
                },
                rows: Vec::new(),
            },
            peer_top: 0,
        };
        // 観戦者へ: 繋がった seg（最初は 0）の始まり。選択の押下・SELECT・マッチの入力ワードはこの後ろ（dll の繋がった tick と同じ）。
        p.spec_seg(c.seg(), c);
        if p.menu.is_some() {
            Self::enter_menu(c, &mut p, &mut self.rep, SCREEN_DIFFICULTY);
        } else {
            c.enter_selection();
        }
        self.play = Some(p);
    }

    /// 選択に入る（選択の台本の実行）。模型は `screen` の画面から（最初と選択の外からは 7、マッチから戻ると 8）。
    fn enter_menu(c: &mut Connected, p: &mut Play, rep: &mut Report, screen: u8) {
        c.enter_selection();
        p.phase = Phase::Selecting;
        if let Some(m) = p.menu.as_mut() {
            m.model = Model::new(screen);
            m.last_remote = 0;
        }
        if let Some(r) = rep.menu.as_mut() {
            r.selections = r.selections.saturating_add(1);
        }
    }

    fn tick(&mut self) -> Option<End> {
        if self.play.is_some()
            && let End::Desync { .. } = self.rep.end
        {
            // 前の ms の `applied` が自分の CHECK を記録したときに見つけたズレ。
            return Some(self.rep.end);
        }
        let now = self.now_ms();
        self.link.poll(
            now,
            &mut At {
                tx: &mut self.tx,
                now,
            },
        );
        if let handshake::State::Failed(r) = self.link.handshake_state() {
            // セッションが見た相手のバージョンは、名乗り替えた後の数。ネットワークの上の数（相手が本当に名乗ったバージョン）に入れ替え直す。
            let r = match (r, self.tx.relabel.as_ref()) {
                (FailReason::OtherVersion { theirs }, Some(rl)) => FailReason::OtherVersion {
                    theirs: rl.swap_ver(theirs),
                },
                _ => r,
            };
            return Some(End::HandshakeFailed(r));
        }
        self.maybe_start();
        let cfg = self.cfg;
        let now_us = self.now_us();
        let c = self.link.connected_mut()?;
        let p = self.play.as_mut()?;

        if p.peer_bye_at.is_none() && c.liveness().peer_bye().is_some() {
            p.peer_bye_at = Some(now);
        }
        match c.liveness().state() {
            liveness::State::Dead => {
                return Some(if p.phase == Phase::Done {
                    End::Finished { peer_bye: false }
                } else {
                    End::Dead
                });
            }
            _ => {
                if p.phase != Phase::Done
                    && let Some(r) = c.liveness().peer_bye()
                    && r != ByeReason::Finished
                {
                    return Some(End::PeerBye(r));
                }
            }
        }

        match p.phase {
            Phase::Selecting => {
                if now_us >= p.sel.start() {
                    p.sel.advance();
                    if p.menu.is_some() {
                        if let Some(end) =
                            Self::menu_frame(c, p, cfg, &mut self.tx, &mut self.rep, now)
                        {
                            return Some(end);
                        }
                    } else {
                        Self::selection_frame(c, p, cfg, &mut self.tx, &mut self.rep, now);
                    }
                }
            }
            Phase::Left => {
                if now_us >= p.sel.start() {
                    p.sel.advance();
                    let seat = seat_index(c.seat());
                    let role = c.role();
                    let enter = p.menu.as_mut().is_some_and(|m| {
                        m.runner
                            .frame(FrameIn {
                                screen: m.model.screen(),
                                seat,
                                role,
                                peer_present: false,
                                aligned: false,
                                pending: None,
                            })
                            .enter
                    });
                    if enter {
                        Self::enter_menu(c, p, &mut self.rep, SCREEN_DIFFICULTY);
                    }
                }
            }
            Phase::WaitSelect => {
                if c.selection().selects_ready() {
                    self.rep.agreed = c.selection().agreed();
                    Self::spec_selects(c, p, self.rep.agreed);
                    p.phase = Phase::Loading(now.wrapping_add(cfg.load_ms));
                }
            }
            Phase::Loading(until) => {
                if (now.wrapping_sub(until) as i32) >= 0 {
                    p.phase = Phase::Playing;
                    p.slot = Slots::new(p.grid, now_us);
                    p.slot_advanced = false;
                    p.slot_sync_wait = false;
                    p.want_capture = true;
                    p.stall_run = 0;
                }
            }
            Phase::Playing if cfg.follow => {
                Self::follow_play(c, p, cfg, &mut self.tx, &mut self.rep, now, now_us)
            }
            Phase::Playing if c.mode() == Mode::Rollback => {
                Self::play_rollback(c, p, cfg, &mut self.tx, &mut self.rep, now, now_us)
            }
            Phase::Playing => Self::play(c, p, cfg, &mut self.tx, &mut self.rep, now, now_us),
            Phase::Done => {
                if let Some(t) = p.peer_bye_at
                    && now.wrapping_sub(t) >= LINGER_MS
                {
                    return Some(End::Finished { peer_bye: true });
                }
            }
        }
        if cfg.follow
            && p.phase != Phase::Done
            && c.liveness().peer_bye() == Some(ByeReason::Finished)
        {
            // 相手が終えた。届いた入力ワードは上の `follow_play` が使えるだけ使った。
            if p.phase == Phase::Playing {
                Self::end_followed_seg(c, p);
            }
            c.close(ByeReason::Finished);
            p.phase = Phase::Done;
            p.spec(Record::End(ByeReason::Finished));
        }
        // 観戦のポートへ積む（この tick の間に待ち行列へ入ったもの。順は入れた順）。
        if let Some(s) = self.spectate.as_mut() {
            for r in p.sp_q.drain(..) {
                s.push(r);
            }
        }
        None
    }

    /// 止める地点の SELECT の結果を、観戦者へ両席ぶん流す。共通の欄（マップ・難易度など）は揃った値（ホストの値）を両方に入れる。
    fn spec_selects(c: &Connected, p: &mut Play, agreed: Option<Agreed>) {
        let (Some(own), Some(ag)) = (p.own_select, agreed) else {
            return;
        };
        let shared = |mut s: Select| {
            s.map = ag.map;
            s.difficulty = ag.difficulty;
            s.cpu_1p = ag.cpu_1p;
            s.cpu_2p = ag.cpu_2p;
            s.background = ag.background;
            s.rng_seed = ag.rng_seed;
            s
        };
        let own = shared(Select {
            match_no: c.selection().match_no(),
            ..own
        });
        let peer = shared(ag.peer);
        let (p1, p2) = match c.seat() {
            Seat::P1 => (own, peer),
            Seat::P2 => (peer, own),
        };
        p.spec(Record::Select {
            seat: Seat::P1,
            sel: p1,
        });
        p.spec(Record::Select {
            seat: Seat::P2,
            sel: p2,
        });
    }

    /// 選択の 1 フレーム。v0 は自分の入力ワードはいつも 0。ホストは相手が来ていれば「決定」を押し、それを選択を出る押下として扱う。
    fn selection_frame(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
    ) {
        // 画面を持たない押下（screen 0。`wire::Step::screen` の doc）。ゲームの演出が無いので ready_ok はいつも true。
        let press = (c.role() == Role::Host && c.selection().peer_present()).then_some(Press {
            kind: StepKind::Decide,
            value: 0,
            screen: 0,
        });
        let mut decided = None;
        let fo = c.selection_frame(0, press, true, &mut At { tx, now }, |fo| {
            if fo.apply.is_some() {
                let s = own_select(cfg);
                decided = Some(s);
                SelExit::Decided(s)
            } else {
                SelExit::Stay
            }
        });
        Self::spec_menu(c, p, 0, fo.remote);
        if let Some(st) = fo.apply {
            rep.presses.push(st);
            p.spec(Record::Step(st));
            p.own_select = decided;
            p.phase = Phase::WaitSelect;
        }
    }

    fn spec_menu(c: &Connected, p: &mut Play, local: u16, remote: u16) {
        let (p1, p2) = by_seat(c.seat(), local, remote);
        p.spec(Record::Menu { p1, p2 });
    }

    fn menu_frame(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
    ) -> Option<End> {
        let m = p.menu.as_mut()?;
        let seat = seat_index(c.seat());
        let role = c.role();
        let screen = m.model.screen();
        let mut want = m.runner.frame(FrameIn {
            screen,
            seat,
            role,
            peer_present: c.selection().peer_present(),
            aligned: c.selection().is_aligned(screen),
            pending: c.selection().pending_press(),
        });
        // ホストの役: この画面の合わせがまだなら、台本の押下より先に出す。語に方向が立っていないフレームだけ（押し続けが合わせを
        // またぐと、ゲストの側でその方向が 1 回余計に数えられる）。
        if role == Role::Host
            && c.selection().align_due(screen)
            && let Some(a) = sim_align_press(screen)
        {
            let dirs_up = want.word & cfg.word_mask & APPLY_CLEAR_BITS != 0;
            want.press = (!dirs_up).then_some(a);
        }
        let word = want.word & cfg.word_mask & !TRANSITION_BITS;
        // MENU の末尾の自分の席の位置（このフレームの語で動かす前の位置。ゲームも入力の地点では前の tick までの結果を見せる）。
        let (own_screen, own_cursor) = m.cursor.report(screen);
        c.set_menu_cursor(own_screen, own_cursor);
        m.cursor.on_word(screen, word);
        let step_before = c.selection().step();
        let carried = m.difficulty;
        let model = &mut m.model;
        let mut exit = None;
        let mut diverged = None;
        let mut decided = None;
        let fo = c.selection_frame(word, want.press, true, &mut At { tx, now }, |fo| {
            let Some(st) = fo.apply else {
                return SelExit::Stay;
            };
            match model.apply(&st) {
                Ok(Outcome::Stay) => SelExit::Stay,
                Ok(Outcome::LeftToMode) => {
                    exit = Some(Phase::Left);
                    SelExit::Leave
                }
                Ok(Outcome::MapDecided) => {
                    exit = Some(Phase::WaitSelect);
                    let s = menu_select(cfg, role, seat, model, carried);
                    decided = Some(s);
                    SelExit::Decided(s)
                }
                Err(mismatch) => {
                    diverged = Some(End::MenuDiverged { step: st, mismatch });
                    SelExit::Leave
                }
            }
        });
        Self::spec_menu(c, p, word, fo.remote);
        if let Some(st) = fo.apply {
            rep.presses.push(st);
            if let Some(m) = p.menu.as_mut() {
                m.cursor.on_applied(&st, seat);
            }
            if st.kind == StepKind::Format {
                // 対戦の形式の切替（dll の相手が押した）: セッションの形式を替える。観戦の流れには積まない。
                if let Some(v) = FormatValue::unpack(st.value) {
                    c.switch_format(v, c.role() == Role::Guest);
                }
            } else {
                p.spec(Record::Step(st));
            }
        }
        if decided.is_some() {
            p.own_select = decided;
        }
        let m = p.menu.as_mut()?;
        if let Some(d) = m.model.difficulty() {
            m.difficulty = Some(d);
        }
        if fo.remote != m.last_remote {
            m.last_remote = fo.remote;
            if let Some(r) = rep.menu.as_mut() {
                r.remote_words.push((step_before, fo.remote));
            }
        }
        m.runner
            .after_frame(fo.apply, c.selection().pending_press(), role);
        if diverged.is_some() {
            return diverged;
        }
        match exit {
            Some(Phase::Left) => {
                p.phase = Phase::Left;
                if let Some(r) = rep.menu.as_mut() {
                    r.left_to_mode = r.left_to_mode.saturating_add(1);
                }
            }
            Some(ph) => p.phase = ph,
            None => {}
        }
        None
    }

    fn play(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
        now_us: u64,
    ) {
        while now_us >= p.slot.end() {
            if p.slot_advanced {
                p.stall_run = 0;
            } else {
                rep.stalls += 1;
                p.stall_run += 1;
                rep.max_stall_run = rep.max_stall_run.max(p.stall_run);
            }
            p.slot.advance();
            p.slot_advanced = false;
            p.want_capture = true;
        }
        // 枠の始まりに 1 回だけ取る（stall の後は取り直さない）。
        if p.want_capture {
            if c.lockstep().captured() {
                p.want_capture = false;
            } else {
                Self::request_lat(c, p, cfg, rep);
                let w = word(cfg.seed, c.lockstep().seg(), c.lockstep().frame()) & cfg.word_mask;
                match c.capture(w, now, &mut At { tx, now }) {
                    Ok(()) | Err(CaptureError::AlreadyCaptured) => p.want_capture = false,
                    Err(CaptureError::Full | CaptureError::NoDelay) => {}
                }
            }
        }
        if p.slot_advanced {
            return;
        }
        if let Some(a) = c.advance(now, &mut At { tx, now }) {
            p.slot_advanced = true;
            Self::applied(c, p, cfg, tx, rep, a, now, now_us);
        }
    }

    /// rollback のモードのマッチの 1 ms。枠の始まりに 1 回だけ取り、その枠の中で入力の地点を試し、更新を 1 回走らせたらすぐ
    /// 「Present の後」を呼ぶ（更新の直後を Present の後とみなす）。枠の終わりまで走らなかった枠は stall。
    fn play_rollback(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
        now_us: u64,
    ) {
        while now_us >= p.slot.end() {
            if p.slot_advanced {
                p.stall_run = 0;
            } else {
                rep.stalls += 1;
                p.stall_run += 1;
                rep.max_stall_run = rep.max_stall_run.max(p.stall_run);
                c.rollback_post_present(&mut p.win);
            }
            p.slot.advance();
            p.slot_advanced = false;
            p.slot_sync_wait = false;
            p.want_capture = true;
        }
        if p.want_capture {
            if c.captured() {
                p.want_capture = false;
            } else if c.rollback().is_some_and(|r| r.holding() || r.scene_ended()) {
                // 予測の更新が seg の最後のフレーム E で場面の要求を立てて待っている間は、次のフレーム E + 1 を取らない（取らずにおくと、
                // 送った語の数から E が決まり、付いていく側が E で止まれる）。要求が消えて続く場合は、その後の枠で取る。
                c.resend_if_due(now, &mut At { tx, now });
            } else {
                let w = word(cfg.seed, c.seg(), c.frame()) & cfg.word_mask;
                match c.capture(w, now, &mut At { tx, now }) {
                    Ok(()) | Err(CaptureError::AlreadyCaptured) => p.want_capture = false,
                    Err(CaptureError::Full | CaptureError::NoDelay) => {}
                }
            }
        }
        if p.slot_sync_wait && !p.slot_advanced {
            c.resend_if_due(now, &mut At { tx, now });
        } else if !p.slot_advanced && !p.want_capture {
            match c.rollback_input_point(&mut p.win, now, &mut At { tx, now }) {
                Some(rollback::Step::Ran { .. }) => {
                    p.slot_advanced = true;
                    c.rollback_post_present(&mut p.win);
                }
                Some(rollback::Step::SceneEnd) => p.slot_advanced = true,
                Some(rollback::Step::Wait(rollback::Wait::Sync)) => p.slot_sync_wait = true,
                Some(rollback::Step::Wait(_)) | None => {}
            }
        }
        // 確定した列を delay 方式と同じ入口へ（フレームの順に 1 回ずつ）。記録の D は今の D（seg の途中で Ctrl＋数字の予定で替わる。dll の
        // `rb_drain` と同じ値）。
        let seg = c.seg();
        let delay = c.cur_delay();
        for (frame, p1, p2) in std::mem::take(&mut p.win.rows) {
            Self::record_row(c, p, tx, rep, seg, frame, (p1, p2), delay, now, now_us);
        }
        if c.rollback().is_some_and(|r| r.scene_ended()) {
            Self::end_seg(c, p, cfg, seg, now);
        }
    }

    fn follow_play(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
        now_us: u64,
    ) {
        if c.mode() == Mode::Rollback {
            return Self::follow_play_rollback(c, p, cfg, tx, rep, now, now_us);
        }
        loop {
            if !c.lockstep().captured() {
                Self::request_lat(c, p, cfg, rep);
                let w = word(cfg.seed, c.lockstep().seg(), c.lockstep().frame()) & cfg.word_mask;
                if let Err(CaptureError::Full | CaptureError::NoDelay) =
                    c.capture(w, now, &mut At { tx, now })
                {
                    return;
                }
            }
            let ls = c.lockstep();
            if ls.remote_next() <= apply_frame(ls.frame(), ls.delay()) {
                c.resend_if_due(now, &mut At { tx, now });
                return;
            }
            let Some(a) = c.advance(now, &mut At { tx, now }) else {
                return;
            };
            Self::applied(c, p, cfg, tx, rep, a, now, now_us);
        }
    }

    /// 付いていく形の rollback のモードのマッチの 1 ms。相手が取ったフレーム（`peer_top` が `Rollback::peer_top_for(N)` に届いた N）
    /// だけを更新する: seg の終わりは相手の合図でしか分からず、確定した更新は取り消せないので、語が届いているというだけで先に更新
    /// できない。語の抜けが無い限り N の語は本物なので、予測も進め直しも起きない。
    fn follow_play_rollback(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
        now_us: u64,
    ) {
        loop {
            if !c.captured() {
                let w = word(cfg.seed, c.seg(), c.frame()) & cfg.word_mask;
                if let Err(CaptureError::Full | CaptureError::NoDelay) =
                    c.capture(w, now, &mut At { tx, now })
                {
                    return;
                }
            }
            let need = c.rollback().map_or(u32::MAX, |r| r.peer_top_for(c.frame()));
            if p.peer_top < need {
                c.resend_if_due(now, &mut At { tx, now });
                return;
            }
            let step = c.rollback_input_point(&mut p.win, now, &mut At { tx, now });
            if matches!(step, Some(rollback::Step::Ran { .. })) {
                c.rollback_post_present(&mut p.win);
            }
            // 確定した行は、待ちで返った回にも出る（入力の地点の中の進め直し）ので、返り方に依らず流す。記録の D は今の D。
            let seg = c.seg();
            let delay = c.cur_delay();
            for (frame, p1, p2) in std::mem::take(&mut p.win.rows) {
                Self::record_row(c, p, tx, rep, seg, frame, (p1, p2), delay, now, now_us);
            }
            match step {
                Some(rollback::Step::Ran { .. }) => {}
                Some(rollback::Step::Wait(rollback::Wait::Sync)) => {}
                Some(rollback::Step::Wait(_) | rollback::Step::SceneEnd) | None => return,
            }
        }
    }

    fn request_lat(c: &mut Connected, p: &mut Play, cfg: &SimConfig, rep: &mut Report) {
        if c.role() != Role::Host {
            return;
        }
        for (i, l) in cfg.lat_script.iter().enumerate() {
            if p.lat_done.get(i) == Some(&false)
                && l.seg == c.lockstep().seg()
                && l.frame == c.lockstep().frame()
            {
                if let Some(d) = p.lat_done.get_mut(i) {
                    *d = true;
                }
                rep.lat_results.push(LatResult {
                    seg: l.seg,
                    frame: l.frame,
                    delay: l.delay,
                    result: c.request_delay(l.delay),
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // 借りを分けるため（セッション・道具の状態・送信側・記録を別々に借りる）
    fn applied(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        a: Applied,
        now: u32,
        now_us: u64,
    ) {
        let seg = c.lockstep().seg();
        let (p1, p2) = by_seat(c.seat(), a.local, a.remote);
        let delay = c.lockstep().delay();
        Self::record_row(c, p, tx, rep, seg, a.frame, (p1, p2), delay, now, now_us);
        if !cfg.follow && c.lockstep().frame() >= cfg.frames_per_seg {
            Self::end_seg(c, p, cfg, seg, now);
        }
    }

    /// 使った（rollback のモードでは確定した）1 フレームの記録・観戦・「到着 → 適用」・CHECK。`delay` は記録の欄。
    #[allow(clippy::too_many_arguments)] // `applied` と同じ理由（借りを分ける）
    fn record_row(
        c: &mut Connected,
        p: &mut Play,
        tx: &mut Tx,
        rep: &mut Report,
        seg: u16,
        frame: u32,
        (p1, p2): (u16, u16),
        delay: u8,
        now: u32,
        now_us: u64,
    ) {
        p.spec(Record::Frame { frame, p1, p2 });
        p.digest = input_digest_push(p.digest, p1, p2);
        rep.recs.push(Rec {
            seg,
            frame,
            p1,
            p2,
            delay,
        });
        if let Some(t) = p.arrivals.remove(&(seg, frame)) {
            rep.arrival_to_apply_us.push(now_us.saturating_sub(t));
        }
        if is_check_frame(frame) {
            let chk = check_from_input_digest(seg, frame, p.digest);
            if let Some(CheckResult::Mismatch { local, remote }) =
                c.submit_check(chk, &mut At { tx, now })
            {
                rep.end = End::Desync { local, remote };
            }
        }
    }

    fn end_seg(c: &mut Connected, p: &mut Play, cfg: &SimConfig, seg: u16, now: u32) {
        p.seg_idx += 1;
        // 最後の seg を終えた後も begin_seg して、前の seg のバックアップの返信だけを続ける。
        c.begin_seg(seg.wrapping_add(1));
        p.spec_seg(seg.wrapping_add(1), c);
        p.arrivals.clear();
        if p.seg_idx >= cfg.segs {
            p.phase = Phase::Done;
            c.close(ByeReason::Finished);
            p.spec(Record::End(ByeReason::Finished));
        } else {
            p.phase = Phase::Loading(now.wrapping_add(cfg.load_ms));
        }
    }

    /// 終わり方を決め、状態機械の数をコピーする。ズレ・選択の食い違い・打ち切りでは、BYE をここで 1 通出す。
    ///
    /// 打ち切り（`End::Deadline`）の BYE は理由を終了（`Finished`）にする。黙って終えると、dll は 60 秒の沈黙を待って「切断」と見るので、
    /// 「peer_sim が打ち切った」と「ネットワークが切れた」が分けられない。取消は選択の食い違いと台本の中止に使っているので採らず、新しい理由の値も
    /// 足さない（プロトコルのバージョンに触るため）。
    fn finish(mut self, end: End) -> Report {
        let end = match self.rep.end {
            End::Desync { local, remote } => End::Desync { local, remote },
            _ => end,
        };
        let now = self.now_ms();
        if let (Some(r), Some(m)) = (
            self.rep.menu.as_mut(),
            self.play.as_ref().and_then(|p| p.menu.as_ref()),
        ) {
            r.done = m.runner.done_items();
            r.retries = m.runner.retries;
            r.model = Some(m.model);
        }
        if let Some(c) = self.link.connected_mut() {
            match end {
                End::Desync { .. } => c.close(ByeReason::Desync),
                End::MenuDiverged { .. } => c.close(ByeReason::Cancelled),
                End::Deadline => {
                    self.rep.deadline_bye = c.liveness().state() == liveness::State::Alive;
                    c.close(ByeReason::Finished);
                }
                _ => {}
            }
            c.flush_bye(
                now,
                &mut At {
                    tx: &mut self.tx,
                    now,
                },
            );
            self.tx.flush(now.wrapping_add(u32::MAX / 2));
            self.rep.selection = Some(c.selection().counters());
            self.rep.lockstep = Some(c.lockstep().stats());
            self.rep.rollback = c.rollback().map(|r| r.stats());
            self.rep.liveness = Some(c.liveness().counters());
            self.rep.rtt_ms = c.liveness().rtt_ms();
            self.rep.check_dropped = c.check_log().dropped();
            self.rep.counts.conn = c.counts();
            self.rep.check_matches = c.counts().check_matches;
        }
        // 観戦のポート: 残りの記録と終わりの記録を積み、観戦者が受け取り終えるのを（上限つきで）待つ。対戦の BYE を出した後に待つ。
        if let Some(mut s) = self.spectate.take() {
            if let Some(p) = self.play.as_mut() {
                for r in p.sp_q.drain(..) {
                    s.push(r);
                }
                if p.sp_on
                    && !s.ended()
                    && let Some(reason) = spectate_host::end_reason(&end)
                {
                    s.push(Record::End(reason));
                }
            }
            let _ = s.drain(self.t0);
            self.rep.spectate = Some(s.report());
        }
        self.rep.end = end;
        self.rep.counts.recv = self.link.recv_counts();
        self.rep.counts.sends = self.tx.sends;
        self.rep.counts.send_errors = self.tx.send_errors;
        self.rep.counts.send_refused = self.tx.send_refused;
        self.rep.stun = self.stun.map(|(_, q)| q.state());
        self.rep.path = self.link.path();
        self.rep.faults = self.tx.faults.counts();
        self.rep.handshake = self.link.handshake_counters();
        self.rep.restarts = self.link.restarts();
        self.rep.elapsed_ms = now;
        self.rep
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_follow_fps_on_the_same_grid_as_the_dll() {
        let g60 = frame_grid(60).unwrap();
        let mut s = Slots::new(g60, 1_000);
        assert_eq!((s.start(), s.end()), (1_000, 1_000 + 16_666));
        s.advance();
        s.advance();
        assert_eq!(s.end(), 1_000 + 50_000);
        let g240 = frame_grid(240).unwrap();
        let mut s = Slots::new(g240, 0);
        assert_eq!(s.end(), 4_166);
        for _ in 0..14_399 {
            s.advance();
        }
        assert_eq!(s.end(), 60_000_000);
        assert!(frame_grid(DEFAULT_FPS).is_ok() && frame_grid(MAX_FPS).is_ok());
        for bad in [0, MAX_FPS + 1] {
            assert!(
                matches!(frame_grid(bad), Err(SimError::Refused(_))),
                "{bad}"
            );
        }
    }

    fn tx(fault: FaultConfig) -> Tx {
        Tx {
            sock: UdpSocket::bind("127.0.0.1:0").unwrap(),
            faults: Faults::new(fault),
            delayed: Vec::new(),
            sends: 0,
            send_errors: 0,
            send_refused: 0,
            scope: Scope::Loopback,
            relabel: None,
        }
    }

    fn sim_of(s: &str) -> SimConfig {
        let a: Vec<String> = s.split_whitespace().map(str::to_string).collect();
        match crate::cli::parse(&a, 1) {
            Ok(crate::cli::Parsed::Run(c)) => c.sim,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_scope_reaches_every_gate() {
        let lan =
            sim_of("--scope lan --bind 127.0.0.1:1 --peer 192.168.1.6:7002 --peer 172.16.0.1:7002");
        assert!(candidates(&lan, Scope::Loopback).is_err());
        assert_eq!(candidates(&lan, Scope::Lan).map(|c| c.len()).ok(), Some(2));
        let wan = sim_of("--scope internet --bind 127.0.0.1:1 --peer 203.0.113.9:7002");
        let e = match candidates(&wan, Scope::Lan) {
            Err(SimError::Refused(e)) => e,
            other => panic!("{other:?}"),
        };
        assert!(e.contains("scope = lan"), "{e}");
        assert!(candidates(&wan, Scope::Internet).is_ok());
        let stun = sim_of(
            "--scope internet --bind 127.0.0.1:1 --peer 127.0.0.1:7002 --stun 198.51.100.1:3478",
        );
        for (cfg, scope) in [
            (&lan, Scope::Loopback),
            (&wan, Scope::Lan),
            (&stun, Scope::Lan),
        ] {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            let opts = RunOpts {
                scope,
                ..RunOpts::default()
            };
            match run_opts(cfg, sock, opts) {
                Err(SimError::Refused(e)) => {
                    assert!(e.contains(&format!("scope = {}", scope.name())), "{e}")
                }
                other => panic!("{scope:?}: 断らなかった: {:?}", other.map(|r| r.end)),
            }
        }
        let dests: [SocketAddr; 5] = [
            "127.0.0.1:9".parse().unwrap(),
            "10.0.0.9:9".parse().unwrap(),
            "203.0.113.9:9".parse().unwrap(),
            "239.1.2.3:9".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        ];
        for (scope, want) in [
            (Scope::Loopback, [true, false, false, false, false]),
            (Scope::Lan, [true, true, false, false, false]),
            (Scope::Internet, [true, true, true, false, false]),
        ] {
            let mut t = tx(FaultConfig::default());
            t.scope = scope;
            assert_eq!(dests.map(|a| t.may_send(a)), want, "{scope:?}");
            t.raw(dests[3], b"x");
            assert_eq!((t.sends, t.send_refused), (0, 1), "{scope:?}");
        }
    }

    fn queued(t: &Tx) -> Vec<(u32, u8)> {
        t.delayed
            .iter()
            .map(|(at, _, b)| (*at, b.first().copied().unwrap_or(0)))
            .collect()
    }

    #[test]
    fn one_way_delay_holds_every_copy_for_the_same_time() {
        let to: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut t = tx(FaultConfig {
            one_way_ms: 50,
            ..FaultConfig::default()
        });
        for (i, now) in [(1u8, 1_000u32), (2, 1_004), (3, 1_008)] {
            t.send(now, to, &[i]);
        }
        assert_eq!(queued(&t), vec![(1_050, 1), (1_054, 2), (1_058, 3)]);
        t.flush(1_049);
        assert_eq!(t.sends, 0, "50 ms に届く前に送った");
        t.flush(1_050);
        assert_eq!((t.sends, queued(&t)), (1, vec![(1_054, 2), (1_058, 3)]));
        t.flush(1_058);
        assert_eq!((t.sends, queued(&t)), (3, vec![]));

        let mut t = tx(FaultConfig {
            one_way_ms: 30,
            delay_pct: 100,
            delay_max_ms: 1,
            ..FaultConfig::default()
        });
        t.send(0, to, &[7]);
        assert_eq!((t.sends, queued(&t)), (0, vec![(31, 7)]));
        let mut t = tx(FaultConfig {
            one_way_ms: 30,
            dup_pct: 100,
            delay_max_ms: 1,
            ..FaultConfig::default()
        });
        t.send(0, to, &[8]);
        assert_eq!((t.sends, queued(&t)), (0, vec![(30, 8), (31, 8)]));

        let mut t = tx(FaultConfig::default());
        t.send(0, to, &[9]);
        assert_eq!((t.sends, queued(&t)), (1, vec![]));
        let mut t = tx(FaultConfig {
            dup_pct: 100,
            delay_max_ms: 1,
            ..FaultConfig::default()
        });
        t.send(0, to, &[10]);
        assert_eq!((t.sends, queued(&t)), (1, vec![(1, 10)]));
    }

    #[test]
    fn jitter_is_drawn_per_copy_on_top_of_the_constant_lag_and_reorders() {
        let to: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut t = tx(FaultConfig {
            seed: 3,
            one_way_ms: 30,
            jitter_ms: 40,
            ..FaultConfig::default()
        });
        for i in 0..200u32 {
            t.send(1_000, to, &[(i % 250) as u8]);
        }
        let at: Vec<u32> = t.delayed.iter().map(|(at, _, _)| *at).collect();
        assert!(at.iter().all(|&a| (1_030..=1_070).contains(&a)), "{at:?}");
        assert!(
            at.iter().collect::<std::collections::HashSet<_>>().len() > 20,
            "コピーごとに引いていない"
        );
        assert!(at.windows(2).any(|w| w[1] < w[0]), "順が入れ替わらない");
        t.flush(1_070);
        assert_eq!((t.sends, queued(&t).len()), (200, 0));
    }

    #[test]
    fn words_are_nonzero_and_depend_on_every_input() {
        let mut seen = std::collections::HashSet::new();
        for seg in 0..3u16 {
            for f in 0..2_000u32 {
                let w = word(7, seg, f);
                assert_ne!(w, 0);
                seen.insert(w);
            }
        }
        assert!(seen.len() > 5_000, "{}", seen.len());
        assert_ne!(word(7, 0, 5), word(8, 0, 5), "seed が効いていない");
        assert_ne!(word(7, 0, 5), word(7, 1, 5), "seg が効いていない");
        assert_ne!(word(7, 0, 5), word(7, 0, 6), "フレームが効いていない");
        // seed とフレームを XOR で先に混ぜた形の重なり（seed 5 のフレーム 1 と seed 6 のフレーム 2）を出さない。
        assert_ne!(word(5, 0, 1), word(6, 0, 2));
        for shift in 0..8u32 {
            let same = (0..300u32)
                .filter(|&f| word(5, 0, f) == word(6, 0, f + shift))
                .count();
            assert!(same <= 2, "ずらし {shift} で {same} 語が重なる");
        }
    }
}
