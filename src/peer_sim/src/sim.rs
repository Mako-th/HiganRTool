//! peer_simの本体。1本のスレッドで1 msごとに受信を取り出し、時計を進めて`higanr_core`のステートマシンを呼ぶ。
//! 流れは次のとおり。接続（`handshake`）→ 選択（`selection`）→ マッチ（`lockstep`。segごとに決めたフレーム数）→ 次のseg（選択は1回目だけ）→ 終わったらBYE(終了)。接続した後は`liveness`を回し続ける。
//! 受けたパケットをどのステートマシンへ渡すかは`higanr_core::link`のセッション（dllと同じもの）が決める。ここはセッションをいつどのワードで呼ぶかだけを決める。
//! 時計は1 ms間隔のスリープと1フレームの枠（既定60 fps。`RunOpts::fps`）で動く。枠の中で相手の入力ワードが揃わなければ、その枠をstallとして数える。次の枠では同じフレームをやり直す（付いていく形は枠を使わない）。
//! 付いていく形（`SimConfig::follow`）は相手がdll（本物の花映塚）のときsegの長さを自分で決めず相手に合わせる。rollbackのモード（`RunOpts::mode`）のマッチは`Sim::play_rollback`で回す。
//! 選択の台本（`run_with_menu`の`menu`）の形と進み方は`menu_plan.rs`。

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

/// 走らせ方の選択肢のうち`SimConfig`のフィールドにしないもの。
/// `SimConfig`をフィールドごとに書いて作る所（peer_simとdllのloopbackの検査）を変えずに足せるよう引数で渡す。
#[derive(Debug, Clone, Copy)]
pub struct RunOpts<'a> {
    /// 選択の台本（`menu_plan.rs`。Noneならv0の選択）。
    pub menu: Option<&'a MenuPlan>,
    /// 1秒のフレーム数（既定 `DEFAULT_FPS` = 60。範囲はdllの`[pacing] fps`と同じ）。付いていく形のマッチは使わない。
    pub fps: u64,
    /// 接続の手順を作り直して待ち続ける長さ（ms。`--wait-sec`）。Noneなら設定しない（10秒でやめる。既定）。
    pub wait_ms: Option<u32>,
    /// ネットワークの範囲（`--scope`。既定loopback。`higanr_core::guard`）。受信ソケット・相手の候補・STUNの宛先と、送る直前のガードが使う。
    pub scope: Scope,
    /// 観戦のポート（`--spectate-host`）。Someならホストの役のとき、確定した記録を観戦者へ送る。
    pub spectate: Option<&'a SpectateHostOpts>,
    /// 望むモードとD（`--mode` / `--rb-delay`。`Link::set_mode`へ）。既定はdelay方式。
    /// 取り決めでrollbackに決まったら、マッチを`core::rollback`のステートマシンで回す。
    pub mode: ModeWish,
    /// rollbackに決まったときのR（`--rb-ahead`。1〜16。ネットワークに載せない）。
    pub ahead: u8,
    /// rollbackに決まったときの時刻合わせ（`--rb-sync on|off`）。既定は使う。切った実行と比べるため。
    pub time_sync: bool,
    /// ホストの決め方（`higanr_core::host_pick`。`Link::set_host_pick`へ）。Noneなら使わない（既定。役はrole_nonceのまま）。
    pub host_pick: Option<higanr_core::host_pick::Setup>,
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
            host_pick: None,
        }
    }
}

/// フレームの枠の並び（µsの時計の上）。j番目の枠は`base_us + floor(j × 1 秒 ÷ fps)`から始まる（dllのペースBと同じ`Grid`）。
/// 前の枠に1周期を足して作らない（丸めた端数が積もる）。
#[derive(Debug, Clone, Copy)]
struct Slots {
    /// 基点0の格子（`fps`が決める。基点は`base_us`で足す）。
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

/// `fps`が範囲（`MIN_FPS`〜`MAX_FPS`）の中か。
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

/// 全部を終え相手のBYEを受けてから終えるまで待つ時間（ms）。
/// 相手はPINGを1秒ごとに送ってくる。閉じた自分はそれにBYEを返すので、PING 2回分の機会を待つ。
pub const LINGER_MS: u32 = 2 * liveness::PING_INTERVAL_MS + 500;

/// latencyの切替の台本の1行（ホストのとき。「フレームfでdelayをnに」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatStep {
    pub seg: u16,
    /// そのsegの中のcapture_frame（この取得の前に`request_delay`する）。
    pub frame: u32,
    pub delay: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// 相手の候補のうち最初のもの（`extra_peers`と合わせて候補の列）。
    pub peer: SocketAddr,
    /// 相手の候補の残り（全部へHELLOを送り、最初に返答が来た経路を採る。空なら`peer`だけ）。
    pub extra_peers: Vec<SocketAddr>,
    /// 接続の秘密の乱数（両者で同じ値。既定は`cli::DEFAULT_SECRET`）。
    pub secret: Secret,
    /// STUNの問い合わせ先（ネットワークの範囲`RunOpts::scope`の中だけ。Noneなら問い合わせない。答えは記録に出すだけで接続は待たない）。
    pub stun: Option<SocketAddr>,
    pub name: String,
    pub session_id: u32,
    pub role_nonce: u64,
    /// 希望latency（0〜9）。
    pub latency: u8,
    /// ホストになったときに着くサイド。
    pub host_seat: Seat,
    pub seed: u64,
    /// 相手のマッチの長さに付いていく（モジュールdocの「付いていく形」）。trueなら`segs`と
    /// `frames_per_seg`は使わない（`lat_script`のsegは相手と同じ数え方）。
    pub follow: bool,
    /// 送るワードに掛けるマスク（既定0xFFFF）。本物の花映塚を相手にするときはゲームの画面を動かすビット（Z・Esc・上下）を落とす。
    /// 0になったワードもそのまま送る。
    pub word_mask: u16,
    /// 自分のSELECTの初期ライフ（1〜10。dllは相手のサイドへ書く）。
    pub life: u8,
    /// 受信を取り出して時計で動くものを回す間隔（ms。既定1。0は1と読む）。検査が受けたものを何通か溜めてから渡す形を作るのに使う。
    pub poll_ms: u32,
    pub segs: u32,
    pub frames_per_seg: u32,
    /// segに入る前の読み込みの長さ（ms）。
    /// 両者で違う値にすると、先に読み込みを終えた側のINPUTを相手が読み込み中に受ける形になる。
    pub load_ms: u32,
    pub lat_script: Vec<LatStep>,
    pub fault: FaultConfig,
    /// これを過ぎたら終わっていなくてもやめる（ms）。検査が止まらないように。
    pub max_ms: u32,
}

/// フレームごとの記録（(seg, frame, 1P, 2P)。使った後のdelayも添える）。
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
    /// 相手の次のsegのINPUTで進めた（再戦）。
    pub next_seg: u32,
    /// 相手が次の選択に入ったので進めた（メニューへ戻った）。
    pub peer_selecting: u32,
    /// マッチの外で届いた通知（使わなかった）。
    pub ignored: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// 全部のsegを終えた（付いていく形では相手のBYE(終了)を受けて終えた）。
    /// `peer_bye`は相手のBYEを受けたかどうか（falseなら相手が60秒黙った）。
    Finished { peer_bye: bool },
    /// 接続の手順でやめた。
    HandshakeFailed(handshake::FailReason),
    /// 終える前に相手からBYE(終了)以外のBYEを受けた。
    PeerBye(ByeReason),
    /// 終える前に相手が60秒黙った（BYE(切断)を送った）。
    Dead,
    /// CHECKが合わなかった（BYE(ズレ検出)を送った）。
    Desync { local: Check, remote: Check },
    /// 選択の台本の実行で入った押下が模型に合わなかった（両者の画面が食い違った。BYE(取消)を送った）。
    MenuDiverged {
        step: Step,
        mismatch: menu_model::Mismatch,
    },
    /// `max_ms`を過ぎた。接続していてまだ閉じていなければBYE(終了)を1通出してから終える。
    /// 詳細は`Report::deadline_bye`と`Sim::finish`の注。
    Deadline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuReport {
    pub items: usize,
    pub done: usize,
    /// 自分（ゲストの役）の要求が捨てられて押し直した数（`menu_plan::RETRY_FRAMES`待ってから）。
    pub retries: u32,
    /// 入った選択の数（接続したとき・選択の外から入り直したとき・マッチから戻ったとき）。
    pub selections: u32,
    /// 難易度の画面でキャンセルして選択の外へ出た数。
    pub left_to_mode: u32,
    /// 終わりの模型（最後に入った選択のもの。難易度・マップ・両サイドの決定済みの値）。接続しなければNone。
    pub model: Option<Model>,
    /// 相手の入力ワード（`FrameOut::remote`）が変わったところ（その入力ワードを使ったstep, 入力ワード）。
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
    /// セッションの受信ソケットで数えたもの（相手のアドレス以外・解けない・session_id違い・接続する前）。
    pub recv: link::RecvCounts,
    /// 接続した後にセッションが数えたもの（INPUTの扱い・取得の溢れ・CHECKの一致。接続しなければ0）。
    pub conn: link::ConnCounts,
    pub sends: u64,
    pub send_errors: u64,
    /// ネットワークの範囲の外の宛先へ送ろうとして止めたもの（ガード。セッションは候補と送り元にしか送らないので0のはず）。
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
    /// rollbackのモードに決まったときだけSome（予測・進め直し・待ちの数）。
    pub rollback: Option<rollback::Stats>,
    pub liveness: Option<liveness::Counters>,
    pub check_dropped: u32,
    pub follow: Option<FollowCounts>,
    /// 入った画面を移る押下（自分のでも相手のでも。入った順）。台本なしのv0では画面0の「決定」1つ。
    pub presses: Vec<Step>,
    pub menu: Option<MenuReport>,
    /// STUNの問い合わせ（`SimConfig::stun`を渡したときだけSome）の終わりの状態。
    pub stun: Option<QueryState>,
    pub path: Option<SocketAddr>,
    /// 走らせたペース（`RunOpts::fps`）と送信に足した一定の遅れ（`FaultConfig::one_way_ms`）。要約に出す。
    pub fps: u64,
    pub one_way_ms: u32,
    /// ネットワークの上で名乗ったバージョン（`FaultConfig::claim_ver`。無ければ今のバージョン`wire::PROTO_VER`）。バージョン違いの要約の「自分」。
    pub proto_ver: u8,
    /// 接続の手順を作り直して待つ長さ（`RunOpts::wait_ms`。Noneなら設定していない）と作り直した回数・直前の1回。
    pub retry_window_ms: Option<u32>,
    pub restarts: link::Restarts,
    pub max_ms: u32,
    pub spectate: Option<SpectateHostReport>,
    /// `End::Deadline`で終えたときに打ち切りの時点でBYE(終了)を出したかどうか（接続する前と、もう閉じていた・切れていたときは出さない）。
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
    // seedを先にかき混ぜてからフレームを混ぜる（先にXORすると近いseedの列が1フレームずれて重なる）。
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

/// 自分のSELECT（v0はゲームの値に意味が無い。マップと難易度は両者で同じ値にして食い違いを出さない）。
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

/// 選択の台本の実行でマップが決定したときの自分のSELECT。
/// キャラ・色は模型の自分のサイドの値。マップは一覧の値をSELECTの形（−1 = 0xFF / −2 = 0xFE）にしたもの。
/// 難易度は模型の値（無ければ前の選択の値`carried`）。シードはホストだけ。
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

/// 送る側（ソケット・わざと壊す設定・遅らせた分）。受ける側と別に借りられるよう分けた。
/// 観戦のポートも同じ送信側を使う（わざと壊す設定とネットワークの範囲のガードを2か所に書かない）。
pub(crate) struct Tx {
    pub(crate) sock: UdpSocket,
    pub(crate) faults: Faults,
    /// 遅らせたもの（送る時刻ms, 宛先, バイト列）。送った順に並ぶ。
    delayed: Vec<(u32, SocketAddr, Vec<u8>)>,
    pub(crate) sends: u64,
    pub(crate) send_errors: u64,
    pub(crate) send_refused: u64,
    /// ネットワークの範囲（送る直前のガード）。
    scope: Scope,
    /// 別のバージョンを名乗るときだけSome（対戦の受信ソケットだけ。観戦のポートは名乗り替えない）。
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

    /// セッションが作った1データグラムを、わざと壊す設定に従って`to`へ送る。一定の遅れは落とす・遅らせる・重ねるかを決めた後で、届けるコピーの全部へ足す。
    pub(crate) fn send(&mut self, now: u32, to: SocketAddr, bytes: &[u8]) {
        // 名乗り替えは落とす・遅らせる・重ねるかの決定より前（それらの対象は名乗り替えた物）。
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

    /// `after_ms`の後に送る（0ならすぐ）。遅らせたものは`flush`が遅らせた順に送る。
    fn send_after(&mut self, now: u32, after_ms: u32, to: SocketAddr, bytes: &[u8]) {
        // 揺らぎの乱数はコピー1通ごとに生成する（重ねて送るコピーにも別々に掛かる）。
        let after_ms = after_ms.saturating_add(self.faults.jitter());
        if after_ms == 0 {
            self.raw(to, bytes);
        } else {
            self.delayed
                .push((now.wrapping_add(after_ms), to, bytes.to_vec()));
        }
    }

    /// 1通を送る。宛先はセッションが候補か送り元から選んだもの（起動時にネットワークの範囲の中かを検証済み）。送る直前にも範囲の中かを見る。
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

    /// 送る直前のガード（`raw`が使う。検査はこれを直に見る）。
    fn may_send(&self, to: SocketAddr) -> bool {
        guard::allows(self.scope, to)
    }

    pub(crate) fn flush(&mut self, now: u32) {
        let mut due = Vec::new();
        self.delayed.retain(|(at, to, b)| {
            // 差で比べる（u32のmsの折り返しでも正しい）。遅れは高々`MAX_ONE_WAY_MS` ＋ 乱数の幅でi32の範囲に収まる。
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

/// 相手の候補の列（`peer` ＋ `extra_peers`）。どれかがネットワークの範囲`scope`の外なら断る。
fn candidates(cfg: &SimConfig, scope: Scope) -> Result<Candidates, SimError> {
    let mut all = vec![cfg.peer];
    all.extend_from_slice(&cfg.extra_peers);
    for &a in &all {
        guard::check_peer(a, scope).map_err(SimError::Refused)?;
    }
    Candidates::from_addrs(&all)
        .map_err(|e| SimError::Refused(format!("相手の候補 {all:?} が並べられない: {e:?}")))
}

/// STUNのtransaction id（96 bit）。peer_simはOSの乱数を持たない（stdだけ）ので時刻とrole_nonceから作る。
/// loopbackの偽の応答役にしか送らないので、推測されても害は無い。
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
    /// 自分は選択を出た。両者のSELECTが揃うのを待つ。
    WaitSelect,
    /// 難易度の画面でキャンセルして選択の外へ出た（選択の台本の実行だけ）。相手が次の選択に入るか台本の`enter`を待つ。
    Left,
    /// segに入る前の読み込み（このmsまで）。
    Loading(u32),
    Playing,
    Done,
}

/// 接続した後のツール側の状態（ステートマシンはセッションの`Connected`が持つ）。
struct Play {
    phase: Phase,
    seg_idx: u32,
    grid: Grid,
    /// 選択のフレームの枠（今の枠の始まりが来たら1フレーム回して次の枠へ）。
    sel: Slots,
    // ---- マッチ ----
    slot: Slots,
    slot_advanced: bool,
    /// rollbackのモードでこの枠は時刻合わせの待ち（`Wait::Sync`）を返された。枠の終わりまで入力の地点を試さない。
    slot_sync_wait: bool,
    want_capture: bool,
    stall_run: u32,
    digest: u64,
    /// (seg, apply_frame) → 相手の入力ワードが最初に届いた時刻（µs）。
    arrivals: HashMap<(u16, u32), u64>,
    lat_done: Vec<bool>,
    /// 相手のBYEを最初に見た時刻（ms）。
    peer_bye_at: Option<u32>,
    menu: Option<MenuRun>,
    // ---- 観戦（`spectate_host`）。ホストの役で観戦のポートがあるときだけ使う ----
    /// 観戦者へ流す記録を積むかどうか（観戦のポートがあり、役がホスト）。
    sp_on: bool,
    /// 積む前の記録（各tickの終わりに`spectate_host`のリングバッファへ渡す。関数ごとの借用を分けるためのキュー）。
    sp_q: Vec<Record>,
    /// 自分のSELECT（止める地点で置いた値。観戦者へ両サイドぶんのSELECTを渡すときの元）。
    own_select: Option<Select>,
    win: NoGame,
    /// 今のsegで相手のINPUTから見た一番先の位置（届いたINPUTの`base + 語数`の最大 = 相手が作ったワードの数）。
    /// 付いていく形のrollbackのモードだけが使う。segごとに0に戻す。
    peer_top: u32,
}

/// rollbackのモードのウィンドウ（`rollback::Window`）。ゲームが無いので状態を持たず、コピーも書き戻しも何もしない。
/// 場面の境目は「segの最後のフレーム（`last`）の更新が次の場面の要求を立てる」とする。
/// 確定した (frame, 1P, 2P)は`rows`に積む。呼び手がdelay方式と同じ記録・CHECKの入口へ渡す（`Sim::record_row`）。
/// ステートマシンが取るCHECK（`Window::check`）は使わない（peer_simのCHECKは使った列のダイジェスト）。
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

    /// 観戦者への新しいsegの始まり（`Seg`とその直後のサイドごとの名前。dllの`spec_seg`と同じ`spectate::seg_head`）。
    fn spec_seg(&mut self, seg: u16, c: &Connected) {
        for r in higanr_core::spectate::seg_head(seg, &c.established()) {
            self.spec(r);
        }
    }
}

struct MenuRun {
    runner: Runner,
    model: Model,
    /// 前の選択で決定した難易度（付いていく形でマッチから戻った選択の模型は難易度を持たない）。
    difficulty: Option<u8>,
    /// 直前の相手の入力ワード（変わったところだけ記録する）。
    last_remote: u16,
    /// 自分のサイドのキャラのカーソル（MENUの末尾で相手へ知らせる）。
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
    /// STUNの問い合わせ（`SimConfig::stun`のとき）。接続と並行して走らせる。接続は答えを待たない。
    stun: Option<(SocketAddr, Query)>,
    spectate: Option<SpectateSide>,
    rep: Report,
}

/// 1回走らせる。`sock`は呼び手が開いたUDPソケット。宛先と受信ソケットがネットワークの範囲の外なら1通も送らずに`SimError::Refused`を返す。
pub fn run(cfg: &SimConfig, sock: UdpSocket) -> Result<Report, SimError> {
    run_with_menu(cfg, sock, None)
}

/// `run`に選択の台本を足したもの。`menu`がNoneなら`run`と同じ。ペースは60 fps。
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
    // rollbackを望む実行はlatencyの切替の台本を持たない（どちらで走ったかで引数の意味が変わらないように）。
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
    link.set_host_pick(opts.host_pick);
    // 書かなければ設定しない（呼ばない形を既定の実行として保つ）。
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
                        // STUNの答えは問い合わせへ（受信ソケットで分ける）。問い合わせていなければセッションが数えて捨てる。
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
                    // 前に送った先のポートが閉じている（相手が先に終えた）と次の受信がWSAECONNRESETになる。読み飛ばす。
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
                // 問い合わせはわざと壊さない（壊すのは相手への送信だけ）。
                self.tx.raw(*server, &req);
            }
            self.tx.flush(now);
            std::thread::sleep(Duration::from_millis(u64::from(self.cfg.poll_ms.max(1))));
        }
    }

    fn on_datagram(&mut self, from: SocketAddr, bytes: &[u8]) -> Option<End> {
        // 別のバージョンを名乗っているなら受けたものも入れ替えてからセッションへ。
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
                // 「到着 → 適用」の到着: まだ使っていないフレームのワードが最初に届いた時刻。
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

    /// 付いていく形のsegの終わりの通知（`to_select`がtrueなら相手が次の選択に入った。falseなら次のsegのINPUT）。
    /// 使えるだけ使ってからsegを進める。
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

    /// 付いていく形で今のsegを終える（相手は先に出ている。相手の前のsegのINPUTへのバックアップの返信は続ける）。
    fn end_followed_seg(c: &mut Connected, p: &mut Play) {
        let seg = c.lockstep().seg();
        c.begin_seg(seg.wrapping_add(1));
        // 観戦者への次のsegの始まり。後ろに続く選択の押下・SELECTや再戦のワードはこのsegのもの（dllの`leave_seg`と同じ並び）。
        p.spec_seg(seg.wrapping_add(1), c);
        p.arrivals.clear();
        p.peer_top = 0;
        p.seg_idx += 1;
    }

    /// セッションが接続したらツール側の状態を作って選択に入る（v0は接続したらすぐ選択の画面とみなす）。
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
                // 付いていく形はsegの終わりを相手の通知で決める（境目は立てない）。
                last: if self.cfg.follow {
                    u32::MAX
                } else {
                    self.cfg.frames_per_seg.saturating_sub(1)
                },
                rows: Vec::new(),
            },
            peer_top: 0,
        };
        // 観戦者への接続したseg（最初は0）の始まり。選択の押下・SELECT・マッチの入力ワードはこの後ろ（dllの接続したtickと同じ）。
        p.spec_seg(c.seg(), c);
        if p.menu.is_some() {
            Self::enter_menu(c, &mut p, &mut self.rep, SCREEN_DIFFICULTY);
        } else {
            c.enter_selection();
        }
        self.play = Some(p);
    }

    /// 選択に入る（選択の台本の実行）。模型は`screen`の画面から（最初と選択の外からは7、マッチから戻ると8）。
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
            // 前のmsの`applied`が自分のCHECKを記録したときに見つけたズレ。
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
            // セッションが見た相手のバージョンは名乗り替えた後の数。ネットワークの上の数（相手が本当に名乗ったバージョン）に入れ替え直す。
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
            // 相手が終えた。届いた入力ワードは上の`follow_play`が使えるだけ使った。
            if p.phase == Phase::Playing {
                Self::end_followed_seg(c, p);
            }
            c.close(ByeReason::Finished);
            p.phase = Phase::Done;
            p.spec(Record::End(ByeReason::Finished));
        }
        // 観戦のポートへ積む（このtickの間にキューへ入ったもの。順は追加した順）。
        if let Some(s) = self.spectate.as_mut() {
            for r in p.sp_q.drain(..) {
                s.push(r);
            }
        }
        None
    }

    /// 止める地点のSELECTの結果を観戦者へ両サイドぶん渡す。共通のフィールド（マップ・難易度など）は揃った値（ホストの値）を両方に書き込む。
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

    /// 選択の1フレーム。v0は自分の入力ワードはいつも0。ホストは相手が来ていれば「決定」を押し、それを選択を出る押下として扱う。
    fn selection_frame(
        c: &mut Connected,
        p: &mut Play,
        cfg: &SimConfig,
        tx: &mut Tx,
        rep: &mut Report,
        now: u32,
    ) {
        // 画面を持たない押下（screen 0。`wire::Step::screen`のdoc）。ゲームの演出が無いのでready_okはいつもtrue。
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
        // ホストの役。この画面の合わせがまだなら台本の押下より先に出す。ワードに方向が立っていないフレームだけ出す。
        // 押し続けが合わせをまたぐと、ゲスト側でその方向が1回余計に数えられるため。
        if role == Role::Host
            && c.selection().align_due(screen)
            && let Some(a) = sim_align_press(screen)
        {
            let dirs_up = want.word & cfg.word_mask & APPLY_CLEAR_BITS != 0;
            want.press = (!dirs_up).then_some(a);
        }
        let word = want.word & cfg.word_mask & !TRANSITION_BITS;
        // MENUの末尾の自分のサイドの位置（このフレームのワードで動かす前の位置。ゲームも入力の地点では前のtickまでの結果を見せる）。
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
                // 対戦の形式の切替（dllの相手が押した）が来たらセッションの形式を替える。観戦の流れには積まない。
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
        // 枠の始まりに1回だけ取る（stallの後は取り直さない）。
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

    /// rollbackのモードのマッチの1 ms。
    /// 枠の始まりに1回だけ取り、その枠の中で入力の地点を試す。更新を1回走らせたらすぐ「Presentの後」を呼ぶ（更新の直後をPresentの後とみなす）。
    /// 枠の終わりまで更新が走らなかった枠はstall。
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
                // 予測の更新がsegの最後のフレームEで場面の要求を立てて待っている間は、次のフレーム E + 1 を取らない。
                // 取らずにおくと、送ったワードの数からEが決まり、付いていく側がEで止まれる。
                // 要求が消えて続く場合は、その後の枠で取る。
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
        // 確定した列をdelay方式と同じ入口へ渡す（フレームの順に1回ずつ）。
        // 記録のDは今のD（segの途中でCtrl＋数字の予定で替わる）。dllの`rb_drain`と同じ値。
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

    /// 付いていく形のrollbackのモードのマッチの1 ms。
    /// 相手が取ったフレーム（`peer_top`が`Rollback::peer_top_for(N)`に届いたN）だけを更新する。
    /// segの終わりは相手の通知でしか分からず、確定した更新は取り消せない。そのためワードが届いているというだけでは先に更新できない。
    /// ワードの抜けが無い限りNのワードは本物なので、予測も進め直しも起きない。
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
            // 確定した行は待ちで返ったときにも出る（入力の地点の中の進め直し）ので、返り方に依らず渡す。記録のDは今のD。
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

    #[allow(clippy::too_many_arguments)] // 借りを分けるため（セッション・ツールの状態・送信側・記録を別々に借りる）
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

    /// 使った（rollbackのモードでは確定した）1フレームの記録・観戦・「到着 → 適用」・CHECK。`delay`は記録のフィールド。
    #[allow(clippy::too_many_arguments)] // `applied`と同じ理由（借りを分ける）
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
        // 最後のsegを終えた後もbegin_segして相手の前のsegのINPUTへのバックアップの返信だけを続ける。
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

    /// 終わり方を決め、ステートマシンの数をコピーする。ズレ・選択の食い違い・打ち切りではBYEをここで1通出す。
    /// 打ち切り（`End::Deadline`）のBYEの理由は終了（`Finished`）にする。黙って終えるとdllは60秒の沈黙を待って「切断」とみなし、打ち切りと回線切れが分けられないため。
    /// 取消は選択の食い違いと台本の中止に使う。新しい理由の値は足さない（プロトコルのバージョンに触るため）。
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
        // 観戦のポートは残りの記録と終わりの記録を積み、観戦者が受け取り終えるのを（上限つきで）待つ。待つのは対戦のBYEを出した後。
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
        // seedとフレームをXORで先に混ぜた形の重なり（seed 5のフレーム1とseed 6のフレーム2）を出さない。
        assert_ne!(word(5, 0, 1), word(6, 0, 2));
        for shift in 0..8u32 {
            let same = (0..300u32)
                .filter(|&f| word(5, 0, f) == word(6, 0, f + shift))
                .count();
            assert!(same <= 2, "ずらし {shift} で {same} 語が重なる");
        }
    }
}
