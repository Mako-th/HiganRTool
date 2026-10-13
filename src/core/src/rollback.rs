//! rollback のモードの状態機械。入力ワードの交換は `lockstep.rs` と同じ形で、違うのは、相手の入力ワードが無い tick を予測で進め、後で確定の入力ワードで進め直すところだけ。
//! ソケットも時計もゲームのメモリも持たず、ゲームの側は [`Window`] で受ける。ゲームスレッドで呼ぶので、ヒープを使わず、パニックしうる添字を置かない。
//!
//! N = これから更新するフレーム、F = 確定の前線（F より前は両者の入力ワードが本物）、S_F = F の始まりの状態のコピー（1 つだけ）、R = 予測の上限、
//! D = 自分の入力の遅れ（`apply_frame = capture_frame + D`）。
//! - 入力の地点（`capture` → `input_point`、Present の後に `post_present`）は、N の相手の入力ワードが届いていればそれで、無ければ相手の直前の確定の入力ワードの繰り返しで進める。
//!   `N − F ≥ R` なら、入力ワードが届いていても待つ（進め直しの費用の上限を守るため）。
//! - 予測の tick が場面の要求を立てたら使わず、F が越えるまで待つ（[`Wait::Boundary`]）。予測で場面の境目を跨がない。
//! - コピーは Present の後。N > F で相手の入力ワードが届いていれば F を進める: 書き戻し → 確定の入力ワードで更新 → コピー → 予測の入力ワードで N まで更新（前後で `resim_begin` / `resim_end`）。
//! - 外へ流すのは確定した (frame, p1, p2) と、確定の tick の始まりで取る CHECK（30 フレームごと）だけ。
//! - 時刻合わせ（[`Wait::Sync`]）: 相手の INPUT から自分が何フレーム先にいるかを見積もり、見積もりが閾値を越えたら描画だけの 1 フレームを入れ、
//!   その後 [`SYNC_GAP`] 更新は待たない。
//!
//! seg の始まりの D（D_start。`seg_delay`）はホストが `begin_seg` で決め、その seg の全部の INPUT に lat を載せる。seg 1 以降のゲストは D 未定で、ホストの INPUT を
//! 1 通受けるまで `capture` が `CaptureError::NoDelay` を返す（seg 0 は AGREE の D を両者が知っている）。seg の途中の D はホストの lat {frame: F, delay: x}（「取得のフレーム F 以降は D = x」という予定）で替える。
//! ホストは `press_delay`（自分の Ctrl＋数字、またはゲストの要求）で出し、F > 0 の予定はゲストが D を知ったと分かってから出す（ゲート。D 未定のゲストは
//! frame 0 しか受けないので、ゲートが無いとゲストが D_start を知る経路が無くなる）。ゲストの要求は INPUT の lat に
//! {frame: seq, delay: x} で載せる。穴埋め（`is_prefill`）の境目と予測の「まだ何も確定していない」は D_start で読み、今の D（`cur_delay`）は時刻合わせの閾値・格子の寄せ・表示・記録だけに使う。

use crate::check::is_check_frame;
use crate::delay_pick::{AUTO, AutoHold, Recommend};
use crate::frame::{MAX_DELAY, is_prefill};
use crate::lockstep::{CaptureError, DelayPress, RecvOutcome, Role, WISH_FRAMES, Wish, by_seat};
use crate::wire::{Check, Input, LatChange, MAX_WORDS, Seat};
use crate::word_ring::{RING, Ring, in_window};

pub use crate::wire::Mode;

/// ホストが D の予定を出すとき、次の取得のフレームから何フレーム先を F にするか。ゲストが F より前に予定を受け取れる量
/// （片道 4 フレーム ＋ R の既定 8）から式で決めた値で、実測ではない。これより遅く届いても替えるのが遅れるだけで正しさは崩れない。
/// 押してから替わるまでの長さ（約 200 ms）でもある。
pub const SWITCH_LEAD: u32 = 12;

pub const D_SWITCH_LOG: usize = 16;

/// seg の途中の D の予定を持った記録。ホストは出したとき、ゲストは受けたとき。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DSwitch {
    pub seg: u16,
    /// この取得のフレームから `to`（自分の台で替えるフレーム）。
    pub frame: u32,
    /// ネットワークに載った F（ゲストが遅れて受けたら `frame` より小さい）。
    pub wire_frame: u32,
    pub from: u8,
    pub to: u8,
    /// 相手の押下から来た予定か。
    pub by_peer: bool,
}

/// R の下限と上限。
pub const MIN_AHEAD: u8 = 1;
pub const MAX_AHEAD: u8 = 16;
/// R の既定（1 フレームあたり平均 約 2.5 ms・最大 約 4.6 ms に収まる数。外挿）。
pub const DEFAULT_AHEAD: u8 = 8;
/// D の既定（片道ぶん 1 フレームを隠す）。
pub const DEFAULT_DELAY: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    mode: Mode,
    /// AGREE で決まった D（seg 0 の D）。以後の seg は `Rollback::seg_delay`。
    delay: u8,
    max_ahead: u8,
    seat: Seat,
    /// 役（role_nonce の大きい方がホスト）。rollback のモードで seg の D を決めて INPUT に載せるのはホストだけ。
    role: Role,
    /// 時刻合わせを使うか。ネットワークに載せない各自の設定。
    time_sync: bool,
}

impl Config {
    /// rollback のモード。`delay` は 0〜9、`max_ahead` は 1〜16 に詰める。役の既定はホスト（本物の役は `with_role`）。
    pub fn rollback(seat: Seat, delay: u8, max_ahead: u8) -> Self {
        Config {
            mode: Mode::Rollback,
            delay: delay.min(MAX_DELAY),
            max_ahead: max_ahead.clamp(MIN_AHEAD, MAX_AHEAD),
            seat,
            role: Role::Host,
            time_sync: true,
        }
    }

    /// 役を置く。delay 方式では効かない。
    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    /// 時刻合わせを入れる / 切る。delay 方式では元から効かない。
    pub fn with_time_sync(mut self, on: bool) -> Self {
        self.time_sync = on;
        self
    }

    pub fn delay_mode(seat: Seat, delay: u8) -> Self {
        Config {
            mode: Mode::Delay,
            delay: delay.min(MAX_DELAY),
            max_ahead: 0,
            seat,
            role: Role::Host,
            time_sync: false,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// AGREE で決まった D（seg 0 の D）。
    pub fn delay(&self) -> u8 {
        self.delay
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn max_ahead(&self) -> u8 {
        self.max_ahead
    }

    pub fn seat(&self) -> Seat {
        self.seat
    }

    pub fn time_sync(&self) -> bool {
        self.time_sync && self.mode == Mode::Rollback
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickKind {
    /// 入力の地点の 1 回（画面に出る本物の更新）。
    Live,
    /// 進め直し: 確定の入力ワード。
    Confirmed,
    Predicted,
}

/// ゲームに渡す 1 回の更新。`p1` / `p2` は feed の席の順（`by_seat` 済み）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub frame: u32,
    pub p1: u16,
    pub p2: u16,
    pub kind: TickKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickReport {
    /// 次の場面の要求が立った（更新の列が変わる・ラウンドの終わり）。
    pub boundary: bool,
}

/// CHECK に載せるゲームの値（`wire::Check` の 4 欄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CheckValues {
    pub rng_state: u16,
    pub rng_count: u32,
    pub life_1p: u8,
    pub life_2p: u8,
}

/// ゲームの側。dll と検査の偽のゲームが実装する。
pub trait Window {
    /// いまの状態を「F のフレームの始まりの状態」としてコピーする（1 枠。前のコピーは捨てる）。
    fn save(&mut self, frame: u32);
    /// コピーした状態へ書き戻す。つなぎと場面の要求の欄も戻す。
    fn restore(&mut self, frame: u32);
    fn tick(&mut self, t: Tick) -> TickReport;
    /// CHECK の値を取る。確定の tick の始まりでだけ呼ばれる。
    fn check_values(&mut self) -> CheckValues;
    /// 進め直しの始まり（SE・BGM・文字の数を記録する）。
    fn resim_begin(&mut self) {}
    /// 進め直しの終わり（記録した値へ戻し、進め直しの回の出力は全部捨てる）。
    fn resim_end(&mut self) {}
    /// 確定した (frame, p1, p2)。frame の昇順に 1 回ずつ。
    fn confirmed(&mut self, _frame: u32, _p1: u16, _p2: u16) {}
    /// 確定の tick の始まりで取った CHECK（30 フレームごと。frame 0 を含む）。
    fn check(&mut self, _c: &Check) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 更新を 1 回走らせた。`real` は相手の入力ワードが本物か、`confirmed` は確定の状態の上での本物の入力ワードか。
    Ran {
        frame: u32,
        real: bool,
        confirmed: bool,
    },
    /// 描画だけのフレーム（次の do_frame で同じフレームをやり直す。取得はし直さない）。
    Wait(Wait),
    /// 場面の境目が確定した。
    SceneEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// `capture` がまだ（呼び手の順番の間違い）。
    NotCaptured,
    Ahead,
    Boundary,
    /// 内部の食い違い。`stats().faults` に数え、以後この seg では進まない。
    Fault,
    /// 自分が先にいるので、描画だけの 1 フレームで待つ。相手の入力ワードを待つ待ちではないので、呼び手は同じフレームの中で
    /// 繰り返し試さず、次のフレームで `input_point` をもう一度呼ぶ。
    Sync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Nothing,
    Saved,
    /// F を進めた。`confirmed` = 確定の入力ワードで更新した回数、`predicted` = 予測の入力ワードで更新した回数。
    Resim {
        from: u32,
        confirmed: u32,
        predicted: u32,
    },
}

/// 数えるもの（ログ・検査用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// 入力の地点の更新の数（確定の状態の上の本物の入力ワード / 相手の入力ワードは本物だが状態は未確定 / 予測の入力ワード）。
    pub live_confirmed: u32,
    pub live_unconfirmed_real: u32,
    pub live_predicted: u32,
    /// 進め直しの回数（`stall_rewinds` は入力の地点で行った分）。
    pub resims: u32,
    pub stall_rewinds: u32,
    pub resim_confirmed_ticks: u32,
    pub resim_predicted_ticks: u32,
    /// コピーの回数（初回・N == F・進め直しの終わり）と書き戻しの回数。
    pub saves: u32,
    pub restores: u32,
    /// 待った入力の地点の数（`N − F ≥ R` / 境目の待ち）。
    pub waits_ahead: u32,
    pub waits_boundary: u32,
    /// 時刻合わせで待った描画だけのフレームの数（`Wait::Sync`）。
    pub waits_sync: u32,
    /// そのうち seg の始まりの `SYNC_HEAD_FRAMES` フレームを過ぎてからの数（時計のずれで起きる待ち）。
    pub waits_sync_late: u32,
    /// 予測の tick が要求を立てて待ちに入った数。待ちは書き戻しで必ず解け、行き先は場面の終わり（`holds_ended`）/
    /// 要求が消えて続行（`holds_released`）/ 予測の入力ワードで立て直し（`holds_rearmed`。新しい `holds_set` に数える）。
    /// 場面がすべて終わった後は `holds_set == holds_ended + holds_released + holds_rearmed`。
    pub holds_set: u32,
    pub holds_ended: u32,
    pub holds_released: u32,
    pub holds_rearmed: u32,
    /// 進め直しで N が戻った分の合計（境目より先の更新を捨てた数）。
    pub discarded_ticks: u32,
    /// N が戻った後に同じフレームの取得を求められた数（取得は捨てて、送った入力ワードを使う）。
    pub recaptured: u32,
    /// 観測した N − F の最大（R 以下）。
    pub max_ahead_seen: u32,
    pub scenes_ended: u32,
    /// 同じ apply_frame に違う入力ワードが来た数（最初の入力ワードを守った）。
    pub conflicts: u32,
    /// 内部の食い違い（0 でなければバグ）。
    pub faults: u32,
    /// seg ごとの D の記録（決まった順の (seg, D)。新しい `SEG_D_LOG` 個をリングバッファに持つ）。総数は `seg_d_count`。
    pub seg_d_log: [(u16, u8); SEG_D_LOG],
    pub seg_d_count: u32,
    /// ゲストが D 未定のために取得を待った数（seg の始まりで、ホストのその seg の最初の INPUT を待った分）。
    pub delay_waits: u32,
    /// 時刻合わせの見積もり（`sync_lead()` と同じ単位。正 = 自分が先）の記録: 標本の数・合計・最大・最小。
    /// `sync_samples` が 0 なら最大・最小は意味が無い。
    pub sync_samples: u32,
    pub sync_lead_sum: i64,
    pub sync_lead_max: i32,
    pub sync_lead_min: i32,
    /// 予定を持った記録（新しい `D_SWITCH_LOG` 個をリングバッファに持つ）と総数。今の D と同じ値の予定は数えず、
    /// 届く前に次の予定で置き換えた予定は外す（`d_cancelled`）。
    pub d_switch_log: [DSwitch; D_SWITCH_LOG],
    pub d_switches: u32,
    pub d_filled: u32,
    pub d_dropped: u32,
    pub d_late: u32,
    /// 届く前に次の予定で置き換えて、効かずに消えた予定の数。
    pub d_cancelled: u32,
    /// ホストがゲートの閉じている間の押下・要求を持っておいた数。
    pub d_held: u32,
    /// ゲスト: 要求した数 / `WISH_FRAMES` で諦めた数。ホスト: 要求を取った数 / seq が古くて lat を無視した数。
    pub wish_sent: u32,
    pub wish_expired: u32,
    pub wish_taken: u32,
    pub wish_dup: u32,
    /// ホスト: seg の始まりの D を最初の取得の前の押下で置き換えた数（マッチの外で押した値が次の seg の始まりの D になる経路）/
    /// auto（`delay_pick::AUTO`）の値を D にした数。
    pub d_head_sets: u32,
    pub d_auto: u32,
    /// seg の始まりの見直しで、素の推奨値なら D が替わるのに据え置いた数（`AutoHold::pick_head` の `held`）。
    pub d_auto_held: u32,
}

pub const SEG_D_LOG: usize = 16;

fn bump(x: &mut u32) {
    *x = x.saturating_add(1);
}

fn note_seg_delay(stats: &mut Stats, seg: u16, d: u8) {
    let i = (stats.seg_d_count as usize) % SEG_D_LOG;
    if let Some(slot) = stats.seg_d_log.get_mut(i) {
        *slot = (seg, d);
    }
    bump(&mut stats.seg_d_count);
}

// ---- 時刻合わせ ----
// 先の側が、相手の INPUT だけから「何フレーム先にいるか」を見積もり、描画だけのフレームを少しずつ入れて揃える
// （GGPO の frame advantage と同じ考え）。

/// 見積もりの単位。「先にいるフレーム数の 2 倍」を 1/256 フレーム刻みの整数で持つ。粗いと移動平均の整数の割り算が
/// 標本の近くで止まり、閾値に届かなくなる。
pub const SYNC_UNIT: i32 = 256;
/// 標本（`wl − 2·top + ack`）の偏りの補正。床で数えた 2 つの差の和なので、位相が一様なら 2L より平均 1 小さい。
/// 補正しないと先の側の標本が閾値に届かず、予測するのに待たない帯ができる（実測: D 1 の 29,602 フレームで書き戻し A 17,527・B 487）。
const SYNC_BIAS: i64 = 1;
/// 移動平均の重み（新しい標本を 1/16 ずつ混ぜる）。軽いと揺れで標本が 1 つ跳ねただけで閾値を越える
/// （実測: ALPHA 4 → 16 で揺れの大きいネットワークの余計な待ちが約 226 万更新のうち 9,272 → 718 回）。
const SYNC_ALPHA: i32 = 16;
/// 標本がこの数たまるまでは待たない。平均が 0 から混ざり始めるので実質は働かず、初期値や重みを変えたときの保険。
const SYNC_MIN_SAMPLES: u8 = 3;
/// 待ち始める先の量（D ≥ 1。補正後の 2L で 1.25）。補正後の標本は、予測する帯の中で 2、手前で 0 か 1。
/// 1.5 にすると帯の縁で平均が届かず、先の側が 3 割ほど予測したまま待たない。1.0 以下だと帯の手前でも待つ。
const SYNC_THRESHOLD: i32 = 5 * SYNC_UNIT / 4;
/// D = 0 の seg の閾値（補正後の 2L で 2.5）。D = 0 で待って得をするのは φ ≥ T + w_AB のときだけ。
/// D ≥ 1 の閾値のままだと、後ろの側の予測が 4.4% → 78.4% になった（sim: 片道 7 ms・時計差 4 µs）。
const SYNC_THRESHOLD_D0: i32 = 5 * SYNC_UNIT / 2;
/// 1 回待った後、次に待つまでに進める更新の数。1 回に 1 フレームだけ待ち、急に止めず少しずつ揃える。
pub const SYNC_GAP: u8 = 3;
/// seg の始まりのこのフレームまでの時刻合わせの待ちは、2 台の読み込みの位相差の揃えとして数え分ける（`Stats::waits_sync_late`）。
/// 実機の読み込みの差は数百 ms（15 フレーム前後）で、待ちの間隔を入れて 50 フレーム前後で揃うので、300 フレーム（5 秒）あれば十分。
pub const SYNC_HEAD_FRAMES: u32 = 300;
// 0 だと、待った直後の同じ N の呼び出しがもう一度待ちになる。
const _: () = assert!(SYNC_GAP >= 1);
// 平均の割り算が止まる幅（ALPHA / UNIT フレーム）は、閾値と標本の間の余裕（D ≥ 1 で 0.25）より狭くする。
const _: () = assert!(SYNC_ALPHA < SYNC_UNIT / 4);

// ---- 格子の寄せ ----

/// 格子を寄せる量の決め方（`tests/rollback_sim.rs` の `glide_table` の掃引で決めた）。平均 `a`（1/`SYNC_UNIT`）が
/// 不感帯 `±dead` の外へ出た分に `gain_ppm` を掛け、`max_ppm` で抑える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlideParams {
    /// 平均が不感帯を 1.0（= `SYNC_UNIT`）越えるごとに、1 フレームの長さを変える量（100 万分率）。
    pub gain_ppm: i32,
    /// 1 フレームの長さの変化の上限（100 万分率。±）。
    pub max_ppm: i32,
    /// 前の枠からの変化の上限（100 万分率。±）。目標が急に変わっても、1 フレームの長さは 1 枠にこれだけずつしか動かない。
    pub slew_ppm: i32,
    /// 不感帯の半幅（1/`SYNC_UNIT`）。
    pub dead: i32,
    /// 相手の INPUT が標本にならないまま過ぎたフレームの数がこれを越えたら寄せない（途切れたネットワークで古い平均のまま動かさない）。
    pub stale_frames: u8,
    /// 寄せてよいネットワークの上限（往復の長さが `2·D·T` のこの割合（%）まで。一度寄せ始めたら `rtt_off_pct` まで）。
    /// RTT が `2·D·T` に近いと寄せても予測が消えず、止まりの方が予測の合計が小さい（模型 D 1: 片道 18 ms で後ろの側の予測 11% → 97.5%）。
    pub rtt_on_pct: u32,
    pub rtt_off_pct: u32,
}

impl GlideParams {
    pub const DEFAULT: GlideParams = GlideParams {
        gain_ppm: 8_000,
        max_ppm: 10_000,
        slew_ppm: 1_000,
        dead: 0,
        stale_frames: 30,
        rtt_on_pct: 97,
        rtt_off_pct: 105,
    };
}

#[derive(Debug, Clone)]
pub struct Rollback {
    cfg: Config,
    seg: u16,
    /// この seg の始まりの D（D_start。seg の途中では変わらない）。apply_frame がこれより小さいフレームは両者とも 0（`is_prefill`）。
    /// 予測の「まだ何も確定していない」もこれで読む。ゲストが D 未定の間は前の seg の最後の D を仮に置いてあるだけ。
    seg_start_delay: u8,
    /// この seg の D が決まっているか。rollback のゲストは `begin_seg` の後 false で、ホストの lat 付き INPUT を受けたら true。
    delay_known: bool,
    /// 予定: 取得のフレーム `sched_f` 以降は `sched_x`、それより前は `pre_d`（`delay_at`）。seg の始まりは {0, D_start}。
    sched_f: u32,
    sched_x: u8,
    pre_d: u8,
    /// ネットワークに載る（ゲストは受けた）予定の F。ホストは `sched_f` と同じ。ゲストが遅れて受けたら `sched_f` より小さい。予定の新旧はこれで比べる。
    sched_wire_f: u32,
    /// 予定を持った回数（ゲストの要求が通ったかを見る印。seg をまたいで数える）。
    sched_gen: u32,
    /// 今の予定を `Stats::d_switch_log` に記録したか。
    sched_logged: bool,
    /// 取得を済ませた一番先のフレーム + 1（新しい取得か、進め直しの取り直しかを見分ける）。
    capture_hi: u32,
    /// ホスト: ゲストが D を知ったと分かったか（ゲート）と、ゲートが閉じている間の押下（後勝ち）。
    guest_knows: bool,
    held: Option<(u8, bool)>,
    /// ホスト: 最後に取ったゲストの要求の seq（seg で戻さない）。
    last_wish_seq: u32,
    /// ゲスト: 載せている要求と、最後に使った seq。
    wish: Option<Wish>,
    wish_seq: u32,
    /// ホスト: auto の旗と seg の始まりの見直しの規則（接続ごとに 1 つ）。立っている間、押した瞬間・ゲストの要求は素の推奨値
    /// （`rec`）をすぐ使い、seg の始まりの最初の取得は余裕をかけて見直す。数字の押下で降りる（後勝ち）。
    auto: AutoHold,
    /// ホスト: 呼び手が最後に渡した推奨値（直近の往復の中央値から。測れていなければ `None`）。
    rec: Option<Recommend>,
    /// 時刻合わせ: このフレームの取得で見積もりを捨てる（自分が替えるフレーム）。相手の top と ack がこの apply_frame を
    /// 越えるまで標本を取らない（標本の式 `(wl − top) − (top − ack)` は、両者の D が違う間 2(D_自分 − D_相手) 偏る。式からの推定で、実測ではない）。
    sync_reset_at: Option<u32>,
    sync_resume: Option<u32>,
    n: u32,
    f: u32,
    captured: bool,
    local: Ring,
    local_next: u32,
    remote: Ring,
    /// 相手から次に要る apply_frame（送る ack）。
    remote_next: u32,
    peer_ack: u32,
    /// 手元のコピー S_F のフレーム。
    snap: Option<u32>,
    /// 予測の tick が場面の要求を立てたフレーム（F が越えるまで待つ）。
    hold: Option<u32>,
    ended: bool,
    broken: bool,
    prev: Option<Input>,
    prev_end: u32,
    stats: Stats,
    // ---- 時刻合わせ。seg ごとに捨てる ----
    /// 相手の入力ワードで今までに見た一番先の位置（相手の INPUT の base + count の最大）。
    peer_top: u32,
    /// 「先にいるフレーム数の 2 倍」の移動平均（1/`SYNC_UNIT` 刻み。正 = 自分が先）。
    sync_avg: i32,
    sync_samples: u8,
    /// 次に待てるようになるまでの更新の数。更新でだけ減るので、待った直後の同じ N の呼び出しは待たない。
    sync_cool: u8,
    /// 最後の標本から数えたフレームの数。
    sync_idle: u8,
    /// 格子を寄せてよいネットワークと見ているか（上下の閾値に幅を持たせるため、前の結果を覚える）。
    glide_ok: bool,
    /// 前の枠に返した ppm（`slew_ppm` で変化を抑えるため）。
    glide_prev: i32,
}

impl Rollback {
    pub fn new(cfg: Config) -> Self {
        let d = u32::from(cfg.delay);
        let mut stats = Stats::default();
        note_seg_delay(&mut stats, 0, cfg.delay);
        Rollback {
            cfg,
            seg: 0,
            seg_start_delay: cfg.delay,
            delay_known: true,
            sched_f: 0,
            sched_x: cfg.delay,
            pre_d: cfg.delay,
            sched_wire_f: 0,
            sched_gen: 0,
            sched_logged: false,
            capture_hi: 0,
            guest_knows: false,
            held: None,
            last_wish_seq: 0,
            wish: None,
            wish_seq: 0,
            auto: AutoHold::default(),
            rec: None,
            sync_reset_at: None,
            sync_resume: None,
            n: 0,
            f: 0,
            captured: false,
            local: Ring::new(),
            local_next: d,
            remote: Ring::new(),
            remote_next: d,
            peer_ack: d,
            snap: None,
            hold: None,
            ended: false,
            broken: false,
            prev: None,
            prev_end: 0,
            stats,
            peer_top: d,
            sync_avg: 0,
            sync_samples: 0,
            sync_cool: 0,
            sync_idle: 0,
            glide_ok: false,
            glide_prev: 0,
        }
    }

    /// 新しい seg に入る（読み込みに入るとき）。コピー・待ち・場面の終わりの印は捨てる。`scene_ended` が真になってから呼ぶのが普通（途中で抜けるなら残りの予測は捨てる）。
    /// その seg の D: ホストと delay 方式はここで決める（前の seg の最後の値。ゲートが閉じていて持っていた押下があればその値）。
    /// rollback のゲストは未定にして、ホストの INPUT で知る（`on_input`）。
    pub fn begin_seg(&mut self, seg: u16) {
        self.prev = Some(self.build_input());
        self.prev_end = self.f;
        self.seg = seg;
        // 予定していて届かなかった値も含む。
        let last = self.held.take().map_or(self.sched_x, |(x, _)| x);
        self.place_head(last, true);
    }

    /// 形式の切替で、この機械がマッチを動かす側に替わるとき（キャラセレクトの間 —— 最初の取得の前）に、seg の始まりを新しい D = `d` で作り直す
    /// （前の seg のバックアップ `prev` は触らない。ホストは auto の旗を `auto` に合わせる）。seg 0 だけはゲストも D を知っている形で作る（ホストが先に始まりを替えられない。`head_open`）。
    /// seg 1 以降のゲストは `begin_seg` と同じに D 未定。ゲストの要求・持っていた押下は捨てる。
    pub fn restart_head(&mut self, d: u8, auto: bool) {
        let d = d.min(MAX_DELAY);
        self.held = None;
        self.wish = None;
        if self.cfg.role == Role::Host {
            if auto {
                self.auto.raise();
            } else {
                self.auto.lower();
            }
        }
        self.place_head(d, self.seg != 0);
    }

    /// seg の始まりの形を `last`（始まりの D。ゲストが待つなら仮の値）で作る。`guest_waits` は、rollback のゲストが D を知らない形で作るか。
    fn place_head(&mut self, last: u8, guest_waits: bool) {
        if self.rollback_mode() && self.cfg.role == Role::Guest && guest_waits {
            // 入力ワードの置き場の起点は仮の値。ホストの INPUT で D を知ったとき `adopt_delay` が置き直す。
            self.seg_start_delay = last;
            self.delay_known = false;
        } else {
            self.seg_start_delay = if self.rollback_mode() {
                last
            } else {
                self.cfg.delay
            };
            self.delay_known = true;
            note_seg_delay(&mut self.stats, self.seg, self.seg_start_delay);
        }
        self.reset_schedule(self.seg_start_delay);
        self.guest_knows = false;
        let d = u32::from(self.seg_start_delay);
        self.n = 0;
        self.f = 0;
        self.captured = false;
        self.local.clear();
        self.local_next = d;
        self.remote.clear();
        self.remote_next = d;
        self.peer_ack = d;
        self.snap = None;
        self.hold = None;
        self.ended = false;
        self.broken = false;
        self.peer_top = d;
        self.sync_avg = 0;
        self.sync_samples = 0;
        self.sync_cool = 0;
        self.sync_idle = 0;
    }

    pub fn config(&self) -> Config {
        self.cfg
    }

    pub fn seg(&self) -> u16 {
        self.seg
    }

    pub fn frame(&self) -> u32 {
        self.n
    }

    pub fn front(&self) -> u32 {
        self.f
    }

    pub fn remote_next(&self) -> u32 {
        self.remote_next
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    /// 場面の境目が確定したか。真なら `begin_seg` して抜ける。
    pub fn scene_ended(&self) -> bool {
        self.ended
    }

    pub fn holding(&self) -> bool {
        self.hold.is_some()
    }

    fn rollback_mode(&self) -> bool {
        self.cfg.mode == Mode::Rollback
    }

    /// この seg の始まりの D（D_start。ゲストが未定の間は前の seg の最後の D。`delay_known` で見分ける）。穴埋めの判定（`is_prefill`）は
    /// これを読む。`config().delay()` は seg 0 の D。
    pub fn seg_delay(&self) -> u8 {
        self.seg_start_delay
    }

    /// 取得のフレーム `n` の D（予定から決める）。今の D を 1 つの変数で持つと、進め直しで N が戻って同じ n を取り直すときに食い違う。
    pub fn delay_at(&self, n: u32) -> u8 {
        if n >= self.sched_f {
            self.sched_x
        } else {
            self.pre_d
        }
    }

    /// 今の D（次に新しく取るフレームの D）。時刻合わせの閾値・格子の寄せ・表示・記録はこれを読む。穴埋めの判定は `seg_delay`。
    pub fn cur_delay(&self) -> u8 {
        self.delay_at(self.capture_hi)
    }

    /// 画面に出す「目標」の D: ゲストは要求している値、ホストはゲートの前に持っている押下、どちらも無ければ最新の予定の値。
    pub fn shown_delay(&self) -> u8 {
        match self.cfg.role {
            // auto の要求は値ではない（9 を画面に出さない）。
            Role::Guest => self.wish.map(|w| w.x).filter(|&x| x != AUTO),
            Role::Host => self.held.map(|(x, _)| x),
        }
        .unwrap_or(self.sched_x)
    }

    /// ゲスト: 今出している要求の値（auto は `delay_pick::AUTO`）。無ければ `None`。ホストは常に `None`。
    pub fn own_wish(&self) -> Option<u8> {
        self.wish.map(|w| w.x)
    }

    /// ホスト: auto の旗が立っているか。ゲストは常に false（ゲストの画面に出す旗は `wire::HostState` で知る）。
    pub fn auto_on(&self) -> bool {
        self.auto.is_on()
    }

    /// ホスト: 推奨値を渡す（約 1 秒ごとに往復を測り直したとき）。使うのは auto を押した瞬間・ゲストが auto を要求したとき・
    /// seg の始まりの最初の取得のときだけ。測れていなければ `None`（旗だけ立てて、値は次の機会に使う）。
    pub fn set_recommend(&mut self, r: Option<Recommend>) {
        self.rec = r;
    }

    pub fn recommend(&self) -> Option<Recommend> {
        self.rec
    }

    /// 相手がフレーム `n` を取ったと言える、相手の入力ワードの数（`base + count` の最大）の下限。相手が自分と同じ予定で取るとみなす。
    /// D を下げた後の捨てる取得は入力ワードを作らないので、次に入力ワードを作る取得の分まで待つ。peer_sim の「付いていく形」が使う。
    /// 今の予定だけで数えるので、1 つ前の予定の切替より前の n では外れうる。
    pub fn peer_top_for(&self, n: u32) -> u32 {
        // n より前の取得で作られた一番先の apply_frame。
        let before = if n == 0 {
            None
        } else if n > self.sched_f {
            let x_part = n - 1 + u32::from(self.sched_x);
            let pre_part = self
                .sched_f
                .checked_sub(1)
                .map(|c| c + u32::from(self.pre_d));
            Some(pre_part.map_or(x_part, |p| p.max(x_part)))
        } else {
            Some(n - 1 + u32::from(self.pre_d))
        };
        let mine = n + u32::from(self.delay_at(n));
        let apply = match before {
            Some(b) if mine <= b => {
                // n の取得は捨てられる。次に入力ワードを作る取得（apply = c + x > b となる最初の c）。
                b + 1
            }
            _ => mine,
        };
        apply + 1
    }

    /// この seg の D が決まっているか（ゲストは `begin_seg` の後、ホストのその seg の INPUT を受けるまで false。その間 `capture` は
    /// `CaptureError::NoDelay`）。
    pub fn delay_known(&self) -> bool {
        self.delay_known
    }

    /// 遅れの量の押下（自分の Ctrl＋数字）。ホストは予定を出す（ゲートが閉じていれば持っておく）。ゲストはホストへ要求する。
    /// 範囲は呼び手が詰める（`delay_pick::DelayPick`）。ここは 0〜`MAX_DELAY` の外だけ断る。delay 方式では何もしない（`DelayPress::Refused`）。
    ///
    /// `x` が `delay_pick::AUTO`（数字キーの 9）なら auto: ホストは旗を立てて素の推奨値をすぐ使う（`auto_now`）。ゲストはホストへ
    /// auto を要求する（lat の delay の欄に AUTO を載せる）。0〜8 の値はホストの旗を降ろす（後勝ち）。
    pub fn press_delay(&mut self, x: u8) -> DelayPress {
        self.press_delay_forced(x, false)
    }

    /// `press_delay` の、同じ値でも要求するバージョン。ゲストは ホストの auto の旗を知らないので、「今の値と同じ」で要求を出さないと
    /// ホストの旗が降りない。呼び手は、ゲストの画面に auto の印が立っていて 0〜8 を押したとき `force` を真にする。ホストの側は見ない。
    pub fn press_delay_forced(&mut self, x: u8, force: bool) -> DelayPress {
        if !self.rollback_mode() || x > MAX_DELAY {
            return DelayPress::Refused;
        }
        match self.cfg.role {
            Role::Host if x == AUTO => {
                self.auto.raise();
                self.auto_now(false)
            }
            Role::Host => {
                self.auto.lower();
                self.host_press(x, false)
            }
            Role::Guest => {
                let same = !force
                    && match self.wish {
                        Some(w) => w.x == x,
                        None => self.sched_x == x,
                    };
                if same {
                    return DelayPress::Unchanged;
                }
                self.wish_seq = Wish::next_seq(self.wish_seq);
                self.wish = Some(Wish {
                    seq: self.wish_seq,
                    x,
                    mark: self.sched_gen,
                    left: WISH_FRAMES,
                });
                bump(&mut self.stats.wish_sent);
                DelayPress::Asked { seq: self.wish_seq }
            }
        }
    }

    /// ホスト: 押した瞬間・ゲストの auto の要求で、D を素の推奨値にする（`AutoHold::pick_now`）。推奨値が無ければ何もしない。
    fn auto_now(&mut self, by_peer: bool) -> DelayPress {
        bump(&mut self.stats.d_auto);
        match self.auto.pick_now(self.rec) {
            Some(x) => self.host_press(x, by_peer),
            None => DelayPress::Unchanged,
        }
    }

    /// ホスト: seg の始まりの最初の取得で、旗が立っていれば D を見直す（`AutoHold::pick_head`。上げにくく・下げにくく）。今の D は画面に出す
    /// 目標 `shown_delay`（ゲートの前に持った押下の 9 を同じ見直しで取り消さないため）。マッチの途中では呼ばない。
    fn auto_head(&mut self) {
        bump(&mut self.stats.d_auto);
        if let Some(p) = self.auto.pick_head(self.shown_delay(), self.rec) {
            if p.held {
                bump(&mut self.stats.d_auto_held);
            }
            self.host_press(p.d, false);
        }
    }

    /// seg の始まりの D（D_start）をこの場で置き換えてよいか: ホストで、この seg の最初の取得の前、かつ 2 つ目以降の seg。
    /// seg 0 は不可: ゲストが先に古い D で入力ワードを送りうるので、替えると frame 0 の lat（`BadLat`）と入力ワードの置き場が食い違って止まる。
    /// 2 つ目以降の seg のゲストは D 未定で、ホストの最初の INPUT を受けるまで 1 ワードも送らない。
    fn head_open(&self) -> bool {
        self.rollback_mode()
            && self.cfg.role == Role::Host
            && self.seg != 0
            && self.capture_hi == 0
            && !self.captured
    }

    /// seg の始まりの D をこの場で `x` にする（マッチの外で押した値が次の seg の始まりの D になる）。入力ワードの置き場の起点・予定・記録を作り直す。
    fn set_seg_head(&mut self, x: u8) -> DelayPress {
        self.held = None;
        if x == self.seg_start_delay {
            return DelayPress::Unchanged;
        }
        self.seg_start_delay = x;
        self.reset_schedule(x);
        let d = u32::from(x);
        self.local_next = d;
        self.remote_next = d;
        self.peer_ack = d;
        self.peer_top = d;
        // 記録（`begin_seg` が足した、この seg の (seg, D)）の D を直す。
        let last = (self.stats.seg_d_count as usize).wrapping_sub(1) % SEG_D_LOG;
        if self.stats.seg_d_count > 0
            && let Some(slot) = self.stats.seg_d_log.get_mut(last)
            && slot.0 == self.seg
        {
            slot.1 = x;
        }
        bump(&mut self.stats.d_head_sets);
        DelayPress::Scheduled { frame: 0 }
    }

    /// ホストの押下（自分の Ctrl＋数字 か、取ったゲストの要求）。ゲートが開いていれば予定を出し、閉じていれば持っておく（後勝ち）。
    /// この seg の最初の取得の前なら、seg の始まりの D をこの場で替える（`set_seg_head`）。
    fn host_press(&mut self, x: u8, by_peer: bool) -> DelayPress {
        if self.head_open() {
            return self.set_seg_head(x);
        }
        if !self.guest_knows {
            if self.shown_delay() == x {
                return DelayPress::Unchanged;
            }
            // 最新の予定と同じ値に戻したら、持っていた押下を取り下げる。
            self.held = (x != self.sched_x).then_some((x, by_peer));
            bump(&mut self.stats.d_held);
            return DelayPress::Held;
        }
        if x == self.sched_x {
            return DelayPress::Unchanged;
        }
        let f = (self.capture_hi + SWITCH_LEAD).max(self.sched_wire_f + 1);
        self.adopt_schedule(f, f, x, by_peer);
        DelayPress::Scheduled { frame: f }
    }

    fn open_gate(&mut self) {
        if self.guest_knows {
            return;
        }
        self.guest_knows = true;
        if let Some((x, by_peer)) = self.held.take() {
            self.host_press(x, by_peer);
        }
    }

    /// 予定を {wire_f（ネットワークの F）, f（この台で替える取得のフレーム）, x} に置き換える。`f` より前の D は今の D（`cur_delay`）:
    /// 置き換える前の予定にまだ届いていなければ、その予定は効かずに消える。ホストもゲストも自分の取得の進みで同じ規則を使うので、
    /// 前の予定を受け損ねたゲストも、ホストと同じ D の列になる（前の予定に先に届いたゲストだけが、新しい F までその値で取る。公平さが一時崩れるだけ）。
    fn adopt_schedule(&mut self, wire_f: u32, f: u32, x: u8, by_peer: bool) {
        if self.sched_logged && self.capture_hi <= self.sched_f {
            // 届いていない予定を消す: 記録から外し、その切替のための時刻合わせの印も捨てる。
            self.stats.d_switches = self.stats.d_switches.saturating_sub(1);
            bump(&mut self.stats.d_cancelled);
            self.sync_reset_at = None;
            self.sync_resume = None;
        }
        let pre = self.cur_delay();
        self.pre_d = pre;
        self.sched_f = f;
        self.sched_x = x;
        self.sched_wire_f = wire_f;
        self.sched_gen = self.sched_gen.wrapping_add(1);
        self.sched_logged = pre != x;
        if pre == x {
            return;
        }
        self.sync_reset_at = Some(f);
        // `f` はネットワークから来る値。足し算であふれて落ちないよう飽和させる（溢れたら「いつまでも越えない」側）。
        self.sync_resume = Some(f.saturating_add(u32::from(x.max(pre))));
        let i = (self.stats.d_switches as usize) % D_SWITCH_LOG;
        if let Some(slot) = self.stats.d_switch_log.get_mut(i) {
            *slot = DSwitch {
                seg: self.seg,
                frame: f,
                wire_frame: wire_f,
                from: pre,
                to: x,
                by_peer,
            };
        }
        bump(&mut self.stats.d_switches);
    }

    fn reset_schedule(&mut self, d: u8) {
        self.sched_f = 0;
        self.sched_x = d;
        self.pre_d = d;
        self.sched_wire_f = 0;
        self.sched_logged = false;
        self.capture_hi = 0;
        self.sync_reset_at = None;
        self.sync_resume = None;
    }

    /// ゲスト: 新しい予定を受けた・取得を進めた後に、要求をやめるかを見る（押した後の予定で値が同じ / 回数の上限）。
    fn settle_wish(&mut self, captured: bool) {
        let Some(mut w) = self.wish else {
            return;
        };
        if self.sched_gen != w.mark && self.sched_x == w.x {
            self.wish = None;
            return;
        }
        if captured {
            w.left = w.left.saturating_sub(1);
            if w.left == 0 {
                self.wish = None;
                bump(&mut self.stats.wish_expired);
                return;
            }
        }
        self.wish = Some(w);
    }

    /// 取得を D 未定のために待ったことを数える（`capture` を呼ばずに待つ呼び手のため）。
    pub fn note_delay_wait(&mut self) {
        bump(&mut self.stats.delay_waits);
    }

    /// ゲスト: その seg の D をホストの INPUT で知った。入力ワードの置き場の起点を D に合わせる（D 未定の間は何も取っていない）。
    fn adopt_delay(&mut self, d: u8) {
        let x = u32::from(d);
        self.seg_start_delay = d;
        self.delay_known = true;
        self.local_next = x;
        self.remote_next = x;
        self.peer_ack = x;
        self.peer_top = x;
        self.reset_schedule(d);
        self.sched_gen = self.sched_gen.wrapping_add(1);
        note_seg_delay(&mut self.stats, self.seg, d);
        self.settle_wish(false);
    }

    /// フレーム `n` の自分の入力を取る（1 フレームに 1 回）。N が戻った後に同じフレームを求められたら（`n < capture_hi`）、
    /// 入力ワードはもう列にあり相手にも送ってあるので取り直さない。新しいフレームの apply_frame は `n + delay_at(n)`。
    /// D を上げた境目では隙間を直前の自分の入力ワードで埋め、下げた後の k 回は捨てる。
    pub fn capture(&mut self, local: u16) -> Result<(), CaptureError> {
        if self.captured {
            return Err(CaptureError::AlreadyCaptured);
        }
        if !self.delay_known {
            self.note_delay_wait();
            return Err(CaptureError::NoDelay);
        }
        let n = self.n;
        if n < self.capture_hi {
            bump(&mut self.stats.recaptured);
            self.captured = true;
            return Ok(());
        }
        // auto の旗が立っているホストは、この seg の最初の取得の前に、その時点の推奨値で D_start を見直す。
        // 送るのはこの取得の後なので、ゲストには最初から新しい D_start が見える。
        if self.auto.is_on() && n == 0 && self.cfg.role == Role::Host {
            self.auto_head();
        }
        let apply = n + u32::from(self.delay_at(n));
        if apply < self.local_next {
            // D を下げた後: この apply_frame の入力ワードはもう送ってある（書き換えると遅れて届いた古い INPUT と見分けられない）。
            bump(&mut self.stats.d_dropped);
        } else {
            // 相手がまだ持っていない入力ワードと、進め直しで使う入力ワード（F 以降）を上書きしない。埋める隙間も含めて一番先の apply_frame で確かめる。
            let oldest_needed = self.f.min(self.peer_ack);
            if apply - oldest_needed >= RING as u32 {
                return Err(CaptureError::Full);
            }
            if apply > self.local_next {
                // D を上げた境目: 隙間を直前の自分の入力ワードで埋める（まだ入力ワードが無ければ 0）。0 で埋めると押し続けた射撃が離れる。
                let prev = if self.local_next > u32::from(self.seg_start_delay) {
                    self.local.get(self.local_next - 1).unwrap_or(0)
                } else {
                    0
                };
                for x in self.local_next..apply {
                    self.local.put(x, prev);
                    bump(&mut self.stats.d_filled);
                }
            }
            self.local.put(apply, local);
            self.local_next = apply + 1;
        }
        self.capture_hi = n + 1;
        self.captured = true;
        if self.sync_reset_at.is_some_and(|f| n >= f) {
            // 自分が替えたフレーム: 前の D で取った見積もりを捨てる。
            self.sync_reset_at = None;
            self.sync_avg = 0;
            self.sync_samples = 0;
        }
        self.settle_wish(true);
        Ok(())
    }

    /// 送る INPUT（取得の直後の 1 回と、待ち中の 4 ms ごとの再送）。中身は相手の ack から自分の列の終わりまで、古い方から最大 32 ワード。
    /// lat は、rollback のホストが毎回載せる今の予定（seg の始まりは frame = 0、delay = D_start。途中で替えたら {F, x}）と、
    /// ゲストの要求（{seq, x}。やめるまで）。ゲストが D 未定の間は、入力ワード 0 個・base 0・ack 0（lat なし）。
    pub fn build_input(&self) -> Input {
        if !self.delay_known {
            return Input {
                seg: self.seg,
                ack: 0,
                base: 0,
                lat: None,
                count: 0,
                words: [0u16; MAX_WORDS],
            };
        }
        let base = self.peer_ack;
        let n = (self.local_next.saturating_sub(base) as usize).min(MAX_WORDS);
        let mut words = [0u16; MAX_WORDS];
        for (k, w) in words.iter_mut().enumerate().take(n) {
            *w = self.local.get(base + k as u32).unwrap_or(0);
        }
        Input {
            seg: self.seg,
            ack: self.remote_next,
            base,
            lat: if !self.rollback_mode() {
                None
            } else {
                match self.cfg.role {
                    Role::Host => Some(LatChange {
                        frame: self.sched_wire_f,
                        delay: self.sched_x,
                    }),
                    Role::Guest => self.wish.map(|w| LatChange {
                        frame: w.seq,
                        delay: w.x,
                    }),
                }
            },
            count: n as u8,
            words,
        }
    }

    pub fn build_prev_input(&self) -> Option<Input> {
        self.prev
    }

    /// 届いた INPUT を受ける。捨てるときは中身を 1 つも使わない。判定は `Lockstep::on_input` と同じで、違いは lat と、
    /// 先の入力ワードを置ける範囲が「F + 枠の数」の手前までなこと。rollback のホストが受けた frame 0 は `GuestLat`、frame ≥ 1 は要求として取る。
    /// ゲストが受けたホストの INPUT で、lat が無い・delay が 0〜9 の外・D 未定なのに frame ≠ 0・frame 0 で D_start と違う値・
    /// 知っている予定と同じ F で違う値、は `BadLat`。D 未定のゲストは、ここで D にして入力ワードの置き場を作る。
    pub fn on_input(&mut self, inp: &Input) -> RecvOutcome {
        if inp.seg != self.seg {
            if self.prev.is_some_and(|p| p.seg == inp.seg) {
                return RecvOutcome::PrevSeg {
                    reply: inp.ack < self.prev_end,
                };
            }
            return RecvOutcome::OtherSeg;
        }
        // この INPUT で D を知る（ゲストが D 未定のとき）。確かめが全部通ってから置くので、確かめは「D を知った後」の起点で行う。
        let mut adopt = None;
        // ゲストが受けた新しい予定 / ホストが受けたゲストの要求（確かめが全部通ってから使う）。
        let mut sched_in = None;
        let mut wish_in = None;
        if self.rollback_mode() {
            match (self.cfg.role, inp.lat) {
                (Role::Host, Some(l)) => {
                    if l.frame == 0 {
                        return RecvOutcome::GuestLat;
                    }
                    if l.delay > MAX_DELAY {
                        return RecvOutcome::BadLat;
                    }
                    wish_in = Some(l);
                }
                (Role::Host, None) => {}
                (Role::Guest, None) => return RecvOutcome::BadLat,
                (Role::Guest, Some(l)) => {
                    if l.delay > MAX_DELAY {
                        return RecvOutcome::BadLat;
                    }
                    if !self.delay_known {
                        // ゲートがあるので、D 未定のゲストへ F > 0 の予定は来ない（来たら取り決めの違反）。
                        if l.frame != 0 {
                            return RecvOutcome::BadLat;
                        }
                        adopt = Some(l.delay);
                    } else if l.frame == 0 {
                        if l.delay != self.seg_start_delay {
                            return RecvOutcome::BadLat;
                        }
                    } else if l.frame == self.sched_wire_f {
                        if l.delay != self.sched_x {
                            return RecvOutcome::BadLat;
                        }
                    } else if l.frame > self.sched_wire_f {
                        sched_in = Some(l);
                    }
                    // 古い便（遅れて着いた・入れ替わった）。lat だけ無視して入力ワードは受ける。
                }
            }
        } else if inp.lat.is_some() {
            return RecvOutcome::BadLat;
        }
        let (e, local_end) = match adopt {
            Some(d) => (u32::from(d), u32::from(d)),
            None => (self.remote_next, self.local_next),
        };
        if !in_window(inp.base, e) {
            return RecvOutcome::OutOfWindow;
        }
        if inp.ack > local_end {
            return RecvOutcome::AckAhead;
        }
        if let Some(d) = adopt {
            self.adopt_delay(d);
        }
        if let Some(l) = sched_in {
            // 予定を受けた時点で既に F を過ぎて取っていたら、次の取得から替える（取った入力ワードは書き換えない）。
            let f = l.frame.max(self.capture_hi);
            if f > l.frame {
                bump(&mut self.stats.d_late);
            }
            let by_peer = self.wish.is_none_or(|w| w.x != l.delay);
            self.adopt_schedule(l.frame, f, l.delay, by_peer);
            self.settle_wish(false);
        }
        if self.cfg.role == Role::Host && self.rollback_mode() {
            if let Some(l) = wish_in {
                // 要求を受けた = ゲストはこの seg の D を知っている。ゲートを開けてから押下として扱う。
                self.open_gate();
                if l.frame > self.last_wish_seq {
                    self.last_wish_seq = l.frame;
                    bump(&mut self.stats.wish_taken);
                    if l.delay == AUTO {
                        // ゲストの auto: 旗を立てて、素の推奨値をすぐ使う（変わりにくくするのは seg の始まりの見直しだけ）。
                        self.auto.raise();
                        self.auto_now(true);
                    } else {
                        self.auto.lower();
                        let x = crate::delay_pick::clamp(Mode::Rollback, l.delay);
                        self.host_press(x, true);
                    }
                } else {
                    bump(&mut self.stats.wish_dup);
                }
            }
            if inp.count > 0 {
                // 入力ワードが載った = ゲストはこの seg の D を知って取得している。
                self.open_gate();
            }
        }
        self.peer_ack = self.peer_ack.max(inp.ack);

        let n = (inp.count as usize).min(MAX_WORDS);
        let limit = self.f.saturating_add(RING as u32);
        let mut new_words = 0u8;
        let mut conflicts = 0u8;
        for (k, &w) in inp.words.iter().enumerate().take(n) {
            let Some(x) = inp.base.checked_add(k as u32) else {
                break;
            };
            if is_prefill(x, self.seg_start_delay) {
                continue;
            }
            if x >= limit {
                break;
            }
            match self.remote.get(x) {
                Some(w0) => {
                    if w0 != w {
                        conflicts = conflicts.saturating_add(1);
                    }
                }
                None => {
                    if x >= e {
                        self.remote.put(x, w);
                        new_words += 1;
                    }
                }
            }
        }
        self.stats.conflicts = self.stats.conflicts.saturating_add(u32::from(conflicts));
        for _ in 0..RING {
            if self.remote.get(self.remote_next).is_some() {
                self.remote_next += 1;
            } else {
                break;
            }
        }
        self.sync_sample(inp.base.saturating_add(n as u32), inp.ack);
        RecvOutcome::Accepted {
            new_words,
            conflicts,
        }
    }

    /// 相手の INPUT 1 通から「自分が相手より何フレーム先にいるか」の標本を取る。
    /// `wl` = 自分が作った入力ワードの数（`local_next`）、`top` = 相手が作った入力ワードの数（INPUT の base + count）、`ack` = 相手がその INPUT を出した
    /// 時点で受け取っていた自分の入力ワードの数。片道 w・自分が L フレーム先なら `wl − top = w + L`、`top − ack = w − L` なので
    /// `(wl − top) − (top − ack) = 2L`。w も D も消えるので RTT の測定は要らない。
    /// 標本にするのは相手の入力ワードが前へ進んだ INPUT だけ。入力ワードが抜けて ack が古いときは L を小さく見積もる向きに偏る（待ちすぎない側）。
    /// 実際の数は整数（フレームの床）なので平均して 2L より 1 小さく、`SYNC_BIAS` で足して戻す。
    fn sync_sample(&mut self, top: u32, ack: u32) {
        if !self.cfg.time_sync() || top <= self.peer_top {
            return;
        }
        self.peer_top = top;
        if let Some(t) = self.sync_resume {
            // D の予定を持ってから両者の入力ワードが切替を越えるまでは標本を取らない（両者の D が違う間、式は 2(D_自分 − D_相手) 偏る）。
            // `peer_top` は単調のまま進める。
            if top <= t || ack <= t {
                return;
            }
            self.sync_resume = None;
        }
        self.sync_idle = 0;
        let two_l = i64::from(self.local_next) - 2 * i64::from(top) + i64::from(ack) + SYNC_BIAS;
        let x = (two_l.clamp(-1000, 1000) as i32) * SYNC_UNIT;
        // 平均は 0（釣り合い）から始める。最初の標本をそのまま平均にすると、seg の始まりの標本（最初の 1 往復は相手の ack が動いていない。
        // 実測: 最初の 3 標本が 2・1・0）が閾値を越えたまま数十標本残り、釣り合った 2 台がどちらも seg の始まりで 1 回余計に待つ。
        self.sync_avg += (x - self.sync_avg) / SYNC_ALPHA;
        self.sync_samples = self.sync_samples.saturating_add(1);
        let lead = self.sync_avg / 2;
        let st = &mut self.stats;
        if st.sync_samples == 0 {
            st.sync_lead_max = lead;
            st.sync_lead_min = lead;
        } else {
            st.sync_lead_max = st.sync_lead_max.max(lead);
            st.sync_lead_min = st.sync_lead_min.min(lead);
        }
        st.sync_samples = st.sync_samples.saturating_add(1);
        st.sync_lead_sum += i64::from(lead);
    }

    /// 自分が相手より先にいるフレーム数の見積もり（1/`SYNC_UNIT` 刻み。正 = 先）。標本が無い間は None。
    pub fn sync_lead(&self) -> Option<i32> {
        (self.sync_samples > 0).then_some(self.sync_avg / 2)
    }

    /// 次の 1 フレームの長さを何 ppm 変えるか（正 = 伸ばす、負 = 縮める）。**1 フレームに 1 回**呼ぶ。格子を自分で刻める側だけが使う。
    /// `rtt_us` は生存の測りの往復、`frame_us` は 1 フレームの長さ。平均が不感帯の中なら 0。それ以外は「平均が 0 から外れた分に比例して戻す」だけ。
    /// 0 を返す場合: 時刻合わせを切った設定・標本が `SYNC_MIN_SAMPLES` に満たない間・最後の標本から `stale_frames` を越えた間・D = 0・D が未定・往復が測れていない・往復が長い（`GlideParams::rtt_on_pct`）。
    pub fn glide_ppm_with(&mut self, p: &GlideParams, rtt_us: Option<u32>, frame_us: u32) -> i32 {
        self.sync_idle = self.sync_idle.saturating_add(1);
        // D = 0 は寄せない。先の側は後ろの側の予測を減らすために先にいたいので、平均を 0 へ寄せると後ろの側の予測が増える
        // （模型: 片道 7 ms・揺れ 〜4 ms で 12.2% → 27.5%）。両側が予測しない位置の幅 `2·D·T − RTT` は今の D で決まる。
        let d = u64::from(self.cur_delay());
        // D 未定・往復が測れていない間は 0 を返すだけで、判定の状態（`glide_ok`）は変えない。落とすと seg の始まりのたびに上限側で
        // 判定し直し、往復が上限と戻りの間（32〜35 ms）にあるネットワークでホストは寄せ続けゲストだけが切れる。
        let known = if self.delay_known { rtt_us } else { None };
        if self.delay_known && d == 0 {
            self.glide_ok = false;
        } else if let Some(r) = known {
            let r = u64::from(r) * 100;
            let zone = 2 * d * u64::from(frame_us);
            let pct = if self.glide_ok {
                p.rtt_off_pct
            } else {
                p.rtt_on_pct
            };
            self.glide_ok = r <= zone * u64::from(pct);
        }
        let on = self.glide_ok && known.is_some() && d >= 1;
        let target = if !on
            || !self.cfg.time_sync()
            || self.sync_samples < SYNC_MIN_SAMPLES
            || self.sync_idle > p.stale_frames
        {
            0
        } else {
            let a = self.sync_avg;
            let excess = if a > p.dead {
                a - p.dead
            } else if a < -p.dead {
                a + p.dead
            } else {
                0
            };
            let ppm = i64::from(excess) * i64::from(p.gain_ppm) / i64::from(SYNC_UNIT);
            // 上限・slew は絶対値で使う（負の値でも `clamp` の min > max でパニックしない）。
            let max = i64::from(p.max_ppm).abs();
            ppm.clamp(-max, max) as i32
        };
        let slew = i64::from(p.slew_ppm).abs();
        let prev = i64::from(self.glide_prev);
        let out = i64::from(target).clamp(prev - slew, prev + slew) as i32;
        self.glide_prev = out;
        out
    }

    /// 寄せてよいネットワークと判断している間か（入力が分からない間は前の判定を保つ）。
    pub fn glide_on(&self) -> bool {
        self.glide_ok
    }

    /// 今の D に合う待ち始めの量（`SYNC_THRESHOLD` / `SYNC_THRESHOLD_D0`）。seg の途中で D を替えたら替えた後の D で読む。
    fn sync_threshold(&self) -> i32 {
        if self.cur_delay() == 0 {
            SYNC_THRESHOLD_D0
        } else {
            SYNC_THRESHOLD
        }
    }

    /// 時刻合わせで、次の更新の代わりに描画だけのフレームを 1 つ入れるか。入れるなら、その効果（2L が 2 減る）を見積もりへ先に引く。
    /// 効果が相手の INPUT に映るのは片道の後なので、引かないと同じ先を何回も数えて行き過ぎる。
    fn sync_wait_now(&mut self) -> bool {
        if !self.cfg.time_sync()
            || self.sync_samples < SYNC_MIN_SAMPLES
            || self.sync_cool > 0
            || self.sync_avg < self.sync_threshold()
        {
            return false;
        }
        self.sync_cool = SYNC_GAP;
        self.sync_avg -= 2 * SYNC_UNIT;
        bump(&mut self.stats.waits_sync);
        if self.n >= SYNC_HEAD_FRAMES {
            bump(&mut self.stats.waits_sync_late);
        }
        true
    }

    fn local_word(&self, f: u32) -> Option<u16> {
        if is_prefill(f, self.seg_start_delay) {
            Some(0)
        } else {
            self.local.get(f)
        }
    }

    fn remote_real(&self, f: u32) -> Option<u16> {
        if is_prefill(f, self.seg_start_delay) {
            Some(0)
        } else {
            self.remote.get(f)
        }
    }

    /// 予測の入力ワード: 相手の直前の確定の入力ワードの繰り返し。まだ何も確定していなければ 0（穴埋めと同じ）。その境目は seg の始まりの D
    /// （`seg_start_delay`）で読む。途中で替えた D で読むと、D を上げた後に確定していた入力ワードを「まだ無い」と読んで 0 を予測する。
    fn predicted_word(&self) -> u16 {
        if self.remote_next > u32::from(self.seg_start_delay) {
            self.remote.get(self.remote_next - 1).unwrap_or(0)
        } else {
            0
        }
    }

    fn emit_check<W: Window>(&mut self, w: &mut W, frame: u32) {
        let v = w.check_values();
        w.check(&Check {
            seg: self.seg,
            frame,
            rng_state: v.rng_state,
            rng_count: v.rng_count,
            life_1p: v.life_1p,
            life_2p: v.life_2p,
        });
    }

    fn note_ahead(&mut self) {
        let a = self.n.saturating_sub(self.f);
        if a > self.stats.max_ahead_seen {
            self.stats.max_ahead_seen = a;
        }
    }

    fn fault(&mut self) {
        bump(&mut self.stats.faults);
        self.broken = true;
    }

    fn can_catch_up(&self) -> bool {
        self.rollback_mode()
            && !self.broken
            && self.n > self.f
            && self.remote_next.min(self.n) > self.f
    }

    pub fn input_point<W: Window>(&mut self, w: &mut W) -> Step {
        if self.ended {
            return Step::SceneEnd;
        }
        if self.broken {
            return Step::Wait(Wait::Fault);
        }
        if !self.captured {
            return Step::Wait(Wait::NotCaptured);
        }
        // 止まっている間の進め直し。1 回で F は N か相手の入力ワードの手前まで進むので、3 周目は要らない（念のための上限）。
        for _ in 0..3 {
            let real = self.remote_real(self.n);
            let behind = self.n.saturating_sub(self.f);
            let stalled = if self.hold.is_some() {
                Some(Wait::Boundary)
            } else if self.rollback_mode() {
                if behind >= u32::from(self.cfg.max_ahead) {
                    Some(Wait::Ahead)
                } else {
                    None
                }
            } else if real.is_none() {
                Some(Wait::Ahead)
            } else {
                None
            };
            let Some(why) = stalled else {
                if self.sync_wait_now() {
                    return Step::Wait(Wait::Sync);
                }
                return self.live_tick(w, real);
            };
            if self.can_catch_up() {
                self.catch_up(w, true);
                if self.ended {
                    return Step::SceneEnd;
                }
                if self.broken {
                    return Step::Wait(Wait::Fault);
                }
                continue;
            }
            match why {
                Wait::Boundary => bump(&mut self.stats.waits_boundary),
                _ => bump(&mut self.stats.waits_ahead),
            }
            return Step::Wait(why);
        }
        Step::Wait(Wait::Ahead)
    }

    fn live_tick<W: Window>(&mut self, w: &mut W, real: Option<u16>) -> Step {
        let n = self.n;
        if self.rollback_mode() && self.snap != Some(self.f) {
            // コピーが無いのは、seg の最初の更新の前（N == F）だけ。それ以外は食い違い。
            if self.n != self.f {
                self.fault();
                return Step::Wait(Wait::Fault);
            }
            w.save(self.f);
            self.snap = Some(self.f);
            bump(&mut self.stats.saves);
        }
        let Some(local) = self.local_word(n) else {
            self.fault();
            return Step::Wait(Wait::Fault);
        };
        let (remote, is_real) = match real {
            Some(r) => (r, true),
            None => (self.predicted_word(), false),
        };
        let confirmed = is_real && self.n == self.f;
        if confirmed && is_check_frame(n) {
            self.emit_check(w, n);
        }
        let (p1, p2) = by_seat(self.cfg.seat, local, remote);
        let rep = w.tick(Tick {
            frame: n,
            p1,
            p2,
            kind: TickKind::Live,
        });
        self.n = n + 1;
        self.captured = false;
        self.sync_cool = self.sync_cool.saturating_sub(1);
        if confirmed {
            bump(&mut self.stats.live_confirmed);
            w.confirmed(n, p1, p2);
            self.f = n + 1;
            if rep.boundary {
                self.ended = true;
                bump(&mut self.stats.scenes_ended);
            }
        } else {
            if is_real {
                bump(&mut self.stats.live_unconfirmed_real);
            } else {
                bump(&mut self.stats.live_predicted);
            }
            if rep.boundary {
                self.hold = Some(n);
                bump(&mut self.stats.holds_set);
            }
        }
        self.note_ahead();
        Step::Ran {
            frame: n,
            real: is_real,
            confirmed,
        }
    }

    pub fn post_present<W: Window>(&mut self, w: &mut W) -> After {
        if !self.rollback_mode() || self.ended || self.broken {
            return After::Nothing;
        }
        if self.n == self.f {
            if self.snap != Some(self.f) {
                w.save(self.f);
                self.snap = Some(self.f);
                bump(&mut self.stats.saves);
                return After::Saved;
            }
            return After::Nothing;
        }
        if self.can_catch_up() {
            let from = self.f;
            let (confirmed, predicted) = self.catch_up(w, false);
            return After::Resim {
                from,
                confirmed,
                predicted,
            };
        }
        After::Nothing
    }

    /// F を進める: 書き戻し → 確定の入力ワードで F' まで更新 → コピー → 予測の入力ワードで N まで更新。戻り値は (確定の入力ワードの更新の数, 予測の入力ワードの更新の数)。
    fn catch_up<W: Window>(&mut self, w: &mut W, at_input_point: bool) -> (u32, u32) {
        let from = self.f;
        let target = self.remote_next.min(self.n);
        let old_n = self.n;
        bump(&mut self.stats.resims);
        if at_input_point {
            bump(&mut self.stats.stall_rewinds);
        }
        if self.snap != Some(from) {
            self.fault();
            return (0, 0);
        }
        // 予測の tick が立てた要求は書き戻しで消える。確定の入力ワードで同じ tick が立て直せば場面の終わり、立てなければ続ける。
        let held = self.hold.take().is_some();
        w.resim_begin();
        w.restore(from);
        bump(&mut self.stats.restores);

        let mut confirmed_ticks = 0u32;
        let mut predicted_ticks = 0u32;

        let mut fr = from;
        while fr < target {
            let (Some(local), Some(remote)) = (self.local_word(fr), self.remote_real(fr)) else {
                self.fault();
                w.resim_end();
                return (confirmed_ticks, predicted_ticks);
            };
            if is_check_frame(fr) {
                self.emit_check(w, fr);
            }
            let (p1, p2) = by_seat(self.cfg.seat, local, remote);
            let rep = w.tick(Tick {
                frame: fr,
                p1,
                p2,
                kind: TickKind::Confirmed,
            });
            w.confirmed(fr, p1, p2);
            confirmed_ticks += 1;
            bump(&mut self.stats.resim_confirmed_ticks);
            fr += 1;
            self.f = fr;
            if rep.boundary {
                // 確定の状態の上で要求が立った。これより先の入力ワードは次の seg のもの。
                self.stats.discarded_ticks = self
                    .stats
                    .discarded_ticks
                    .saturating_add(old_n.saturating_sub(fr));
                self.n = fr;
                self.ended = true;
                bump(&mut self.stats.scenes_ended);
                if held {
                    bump(&mut self.stats.holds_ended);
                }
                w.resim_end();
                return (confirmed_ticks, predicted_ticks);
            }
        }
        w.save(target);
        self.snap = Some(target);
        bump(&mut self.stats.saves);

        let mut fr = target;
        while fr < old_n {
            let Some(local) = self.local_word(fr) else {
                self.fault();
                w.resim_end();
                return (confirmed_ticks, predicted_ticks);
            };
            let remote = self
                .remote_real(fr)
                .unwrap_or_else(|| self.predicted_word());
            let (p1, p2) = by_seat(self.cfg.seat, local, remote);
            let rep = w.tick(Tick {
                frame: fr,
                p1,
                p2,
                kind: TickKind::Predicted,
            });
            predicted_ticks += 1;
            bump(&mut self.stats.resim_predicted_ticks);
            fr += 1;
            if rep.boundary {
                // 予測の tick が要求を立てた。これより先は更新しない（N を戻して待つ）。
                self.hold = Some(fr - 1);
                bump(&mut self.stats.holds_set);
                self.stats.discarded_ticks = self
                    .stats
                    .discarded_ticks
                    .saturating_add(old_n.saturating_sub(fr));
                self.n = fr;
                break;
            }
        }
        if held {
            if self.hold.is_some() {
                bump(&mut self.stats.holds_rearmed);
            } else {
                bump(&mut self.stats.holds_released);
            }
        }
        w.resim_end();
        self.note_ahead();
        (confirmed_ticks, predicted_ticks)
    }
}

#[cfg(test)]
mod tests {
    //! 小さな検査（中身の検査は `tests/rollback_sim.rs`）。
    use super::*;

    /// フレームの長さ（µs）。時刻合わせの φ・w の掃引の単位。
    const T: i64 = 16_667;

    #[derive(Default)]
    struct Nop {
        ticks: Vec<Tick>,
        confirmed: Vec<(u32, u16, u16)>,
        saves: u32,
        restores: u32,
    }

    impl Window for Nop {
        fn save(&mut self, _frame: u32) {
            self.saves += 1;
        }
        fn restore(&mut self, _frame: u32) {
            self.restores += 1;
        }
        fn tick(&mut self, t: Tick) -> TickReport {
            self.ticks.push(t);
            TickReport::default()
        }
        fn check_values(&mut self) -> CheckValues {
            CheckValues::default()
        }
        fn confirmed(&mut self, frame: u32, p1: u16, p2: u16) {
            self.confirmed.push((frame, p1, p2));
        }
    }

    fn input_of(rb: &Rollback, base: u32, ack: u32, words: &[u16]) -> Input {
        let mut w = [0u16; MAX_WORDS];
        w[..words.len()].copy_from_slice(words);
        Input {
            seg: rb.seg(),
            ack,
            base,
            lat: None,
            count: words.len() as u8,
            words: w,
        }
    }

    #[test]
    fn config_clamps_delay_and_ahead() {
        let c = Config::rollback(Seat::P1, 200, 0);
        assert_eq!((c.delay(), c.max_ahead()), (MAX_DELAY, MIN_AHEAD));
        let c = Config::rollback(Seat::P1, 1, 99);
        assert_eq!(c.max_ahead(), MAX_AHEAD);
        let c = Config::delay_mode(Seat::P2, 3);
        assert_eq!((c.mode(), c.delay(), c.max_ahead()), (Mode::Delay, 3, 0));
    }

    #[test]
    fn delay_mode_waits_for_the_remote_word() {
        let mut rb = Rollback::new(Config::delay_mode(Seat::P1, 1));
        let mut g = Nop::default();
        rb.capture(0x11).unwrap();
        assert!(matches!(
            rb.input_point(&mut g),
            Step::Ran {
                frame: 0,
                real: true,
                confirmed: true
            }
        ));
        rb.capture(0x12).unwrap();
        assert_eq!(rb.input_point(&mut g), Step::Wait(Wait::Ahead));
        assert_eq!(rb.post_present(&mut g), After::Nothing);
        assert_eq!((g.saves, g.restores), (0, 0));
        let inp = input_of(&rb, 1, 0, &[0x21]);
        assert!(matches!(rb.on_input(&inp), RecvOutcome::Accepted { .. }));
        assert!(matches!(
            rb.input_point(&mut g),
            Step::Ran {
                frame: 1,
                real: true,
                confirmed: true
            }
        ));
        assert_eq!(g.confirmed, vec![(0, 0, 0), (1, 0x11, 0x21)]);
    }

    #[test]
    fn rollback_predicts_then_resimulates_with_the_real_word() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 4));
        let mut g = Nop::default();
        rb.capture(0xA0).unwrap();
        assert!(matches!(
            rb.input_point(&mut g),
            Step::Ran {
                frame: 0,
                confirmed: true,
                ..
            }
        ));
        assert_eq!(rb.post_present(&mut g), After::Saved);
        rb.capture(0xA1).unwrap();
        assert!(matches!(
            rb.input_point(&mut g),
            Step::Ran {
                frame: 1,
                real: false,
                confirmed: false
            }
        ));
        assert_eq!((rb.frame(), rb.front()), (2, 1));
        let inp = input_of(&rb, 1, 0, &[0x77]);
        rb.on_input(&inp);
        let a = rb.post_present(&mut g);
        assert_eq!(
            a,
            After::Resim {
                from: 1,
                confirmed: 1,
                predicted: 0
            }
        );
        assert_eq!((rb.frame(), rb.front()), (2, 2));
        assert_eq!(g.confirmed, vec![(0, 0, 0), (1, 0xA0, 0x77)]);
    }

    #[test]
    fn sync_lead_sign_and_scale() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 16));
        let mut g = Nop::default();
        for i in 0..6u16 {
            rb.capture(i).unwrap();
            assert!(matches!(rb.input_point(&mut g), Step::Ran { .. }));
        }
        assert_eq!(rb.sync_lead(), None);
        // 相手は 3 ワード（top = 4）を作り、自分の入力ワードを 2 つ受け取っていた（ack = 3）。2L = 7 − 8 + 3 = 2 → L = 1 フレーム先。
        let bias = SYNC_BIAS as i32;
        let first = |two_l: i32| (two_l + bias) * SYNC_UNIT / SYNC_ALPHA / 2;
        let inp = input_of(&rb, 1, 3, &[1, 2, 3]);
        assert!(matches!(rb.on_input(&inp), RecvOutcome::Accepted { .. }));
        assert_eq!(rb.sync_lead(), Some(first(2)));
        assert!(matches!(rb.on_input(&inp), RecvOutcome::Accepted { .. }));
        assert_eq!(rb.sync_lead(), Some(first(2)));
        let st = rb.stats();
        assert_eq!(st.sync_samples, 1);
        assert_eq!(st.sync_lead_max, first(2));
        assert_eq!(st.sync_lead_min, st.sync_lead_max);
        assert_eq!(st.sync_lead_sum, i64::from(st.sync_lead_max));
        let inp2 = input_of(&rb, 1, 3, &[1, 2, 3, 4]);
        assert!(matches!(rb.on_input(&inp2), RecvOutcome::Accepted { .. }));
        let avg1 = (2 + bias) * SYNC_UNIT / SYNC_ALPHA;
        let avg2 = avg1 + (bias * SYNC_UNIT - avg1) / SYNC_ALPHA;
        assert_eq!(rb.sync_lead(), Some(avg2 / 2));
        let st = rb.stats();
        assert_eq!(st.sync_samples, 2);
        assert_eq!(
            (st.sync_lead_max, st.sync_lead_min),
            (avg1.max(avg2) / 2, avg1.min(avg2) / 2),
            "最大・最小が標本ごとの平均の値になっていない"
        );
        assert_eq!(st.sync_lead_sum, i64::from(avg1 / 2 + avg2 / 2));
        let inp3 = input_of(&rb, 1, 3, &[1, 2, 3, 4, 5, 6]);
        assert!(matches!(rb.on_input(&inp3), RecvOutcome::Accepted { .. }));
        let avg3 = avg2 + ((-4 + bias) * SYNC_UNIT - avg2) / SYNC_ALPHA;
        assert!(avg3 < avg1, "検査の前提: 3 つ目で平均が最初より下がる");
        let st = rb.stats();
        assert_eq!(st.sync_samples, 3);
        assert_eq!(
            (st.sync_lead_max, st.sync_lead_min),
            (avg2 / 2, avg3 / 2),
            "最小が 3 つ目の平均になっていない"
        );
        assert_eq!(st.sync_lead_sum, i64::from(avg1 / 2 + avg2 / 2 + avg3 / 2));
        // 相手が先: top = 9、ack = 1 → 2L = 7 − 18 + 1 = −10。
        let mut rb2 = Rollback::new(Config::rollback(Seat::P1, 1, 16));
        for i in 0..6u16 {
            rb2.capture(i).unwrap();
            rb2.input_point(&mut g);
        }
        let inp = input_of(&rb2, 1, 1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        rb2.on_input(&inp);
        assert_eq!(rb2.sync_lead(), Some(first(-10)));
        let mut rb3 = Rollback::new(Config::rollback(Seat::P1, 1, 16).with_time_sync(false));
        rb3.capture(0).unwrap();
        rb3.on_input(&input_of(&rb3, 1, 1, &[1, 2, 3]));
        assert_eq!(rb3.sync_lead(), None);
    }

    #[test]
    fn sync_waits_once_per_frame_then_runs() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 16));
        let mut g = Nop::default();
        // 自分が 4 フレーム先（2L = 8）の標本を 30 個入れる。ack が自分の入力ワードの数を越える並びは INPUT では作れないので、標本を入れる関数を直に呼ぶ。
        for i in 0..8u16 {
            rb.capture(i).unwrap();
            assert!(matches!(rb.input_point(&mut g), Step::Ran { .. }));
        }
        for k in 0..30u32 {
            let top = 2 + k;
            rb.sync_sample(top, 2 * top - 1);
        }
        assert!(
            rb.sync_lead().unwrap() >= SYNC_THRESHOLD / 2,
            "{:?}",
            rb.sync_lead()
        );
        rb.capture(9).unwrap();
        assert_eq!(rb.input_point(&mut g), Step::Wait(Wait::Sync));
        assert_eq!(rb.stats().waits_sync, 1);
        assert!(matches!(rb.input_point(&mut g), Step::Ran { .. }));
        assert_eq!(rb.stats().waits_sync, 1);
    }

    /// 先にいる時間 φ・フレームの長さ T・片道 w_AB / w_BA（µs）のネットワークで、遅れ D の 2 台が出す (wl, top, ack) の列を式から作る
    /// （位相を掃引するため）。標本は床の差 2 つの和になる。k は 10 から（後ろの側の φ で ack が D を割らないように）。
    fn phase_series(d: i64, phi: i64, w_ab: i64, w_ba: i64, n: u32) -> Vec<(u32, u32, u32)> {
        let f1 = (phi + w_ba).div_euclid(T);
        let f2 = (phi - w_ab).div_euclid(T);
        (10..10 + i64::from(n))
            .map(|k| {
                (
                    (d + k + 1 + f1) as u32,
                    (d + k + 1) as u32,
                    (d + k + 1 + f2) as u32,
                )
            })
            .collect()
    }

    fn waits_for(d: u8, phi: i64, w_ab: i64, w_ba: i64) -> bool {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, d, 8));
        for (wl, top, ack) in phase_series(i64::from(d), phi, w_ab, w_ba, 40) {
            rb.local_next = wl;
            rb.sync_sample(top, ack);
        }
        assert!(rb.sync_samples >= SYNC_MIN_SAMPLES);
        rb.sync_wait_now()
    }

    #[test]
    fn sync_waits_when_it_pays_for_each_d() {
        for (w_ab, w_ba) in [
            (7_000, 7_000),
            (5_000, 9_000),
            (9_000, 5_000),
            (3_000, 3_000),
        ] {
            for d in [0u8, 1, 2, 3] {
                let edge = if d == 0 { T + w_ab } else { T - w_ba };
                let (mut waited, mut kept) = (0, 0);
                for phi_ms in -40..=60i64 {
                    let phi = phi_ms * 1000;
                    // 縁の前後 1 ms は、床の境で標本が混ざるので比べない。
                    if (phi - edge).abs() <= 1000 {
                        continue;
                    }
                    let want = phi > edge;
                    let got = waits_for(d, phi, w_ab, w_ba);
                    assert_eq!(
                        got, want,
                        "D {d} w_AB {w_ab} w_BA {w_ba} φ {phi_ms} ms: 待つ {got}・待つべき {want}"
                    );
                    if want {
                        waited += 1;
                    } else {
                        kept += 1;
                    }
                }
                // 掃引が両側を通っている（片側だけなら、全部待つ・全部待たない実装も通る）。
                assert!(waited >= 15 && kept >= 30, "D {d}: {waited} / {kept}");
            }
        }
    }

    #[test]
    fn sync_band_phi_waits_and_balanced_or_behind_do_not() {
        assert!(waits_for(1, 14_000, 7_000, 7_000), "帯の中で待たない");
        assert!(
            waits_for(1, 22_000, 7_000, 7_000),
            "帯の上の縁の近くで待たない"
        );
        assert!(!waits_for(1, 0, 7_000, 7_000), "釣り合いで待った");
        assert!(!waits_for(1, 5_000, 7_000, 7_000), "予測しない先で待った");
        assert!(!waits_for(1, -14_000, 7_000, 7_000), "後ろの側が待った");
        assert!(
            !waits_for(1, -30_000, 7_000, 7_000),
            "ずっと後ろの側が待った"
        );
        assert!(!waits_for(0, 14_000, 7_000, 7_000), "D 0 が帯の中で待った");
        assert!(waits_for(0, 30_000, 7_000, 7_000), "D 0 が遠い先で待たない");
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8));
        let (wl, top, ack) = phase_series(1, 14_000, 7_000, 7_000, 1)[0];
        rb.local_next = wl;
        rb.sync_sample(top, ack);
        assert_eq!(rb.sync_avg, (1 + SYNC_BIAS as i32) * SYNC_UNIT / SYNC_ALPHA);
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8));
        let (wl, top, ack) = phase_series(1, 0, 7_000, 7_000, 1)[0];
        rb.local_next = wl;
        rb.sync_sample(top, ack);
        assert_eq!(
            rb.sync_avg,
            (-1 + SYNC_BIAS as i32) * SYNC_UNIT / SYNC_ALPHA
        );
    }

    #[test]
    fn the_average_reaches_both_sides_of_the_threshold() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 8));
        let feed_x = |rb: &mut Rollback, x: i64, n: u32, top0: u32| {
            for k in 0..n {
                let top = top0 + k;
                rb.local_next = (x - SYNC_BIAS + i64::from(top)) as u32;
                rb.sync_sample(top, top);
            }
        };
        feed_x(&mut rb, 2, 200, 2);
        assert!(
            rb.sync_avg >= SYNC_THRESHOLD,
            "標本 2 で閾値に届かない {}",
            rb.sync_avg
        );
        feed_x(&mut rb, 1, 200, 202);
        assert!(
            rb.sync_avg < SYNC_THRESHOLD,
            "標本 1 でも閾値の上 {}",
            rb.sync_avg
        );
    }

    // ---- 格子の寄せ ----

    fn glide_rb(d: u8, avg: i32) -> Rollback {
        let mut rb = Rollback::new(Config::rollback(Seat::P2, d, 8));
        rb.sync_avg = avg;
        rb.sync_samples = SYNC_MIN_SAMPLES;
        rb
    }

    /// 上限・変化の制限を外した組（式そのものを見る）。
    const FREE: GlideParams = GlideParams {
        gain_ppm: 8_000,
        max_ppm: 10_000,
        slew_ppm: i32::MAX / 2,
        dead: 0,
        stale_frames: 30,
        rtt_on_pct: 97,
        rtt_off_pct: 105,
    };

    #[test]
    fn glide_is_proportional_signed_and_clamped() {
        let ppm = |avg| glide_rb(1, avg).glide_ppm_with(&FREE, Some(10_000), 16_667);
        assert_eq!(ppm(0), 0);
        assert_eq!(ppm(SYNC_UNIT), 8_000, "先にいる側は伸ばす");
        assert_eq!(ppm(-SYNC_UNIT / 2), -4_000, "後ろの側は縮める");
        assert_eq!(ppm(5 * SYNC_UNIT), 10_000, "上限");
        assert_eq!(ppm(-5 * SYNC_UNIT), -10_000, "下限");
        let dead = GlideParams {
            dead: SYNC_UNIT / 2,
            ..FREE
        };
        let ppm_d = |avg| glide_rb(1, avg).glide_ppm_with(&dead, Some(10_000), 16_667);
        assert_eq!(ppm_d(SYNC_UNIT / 2), 0, "不感帯の端");
        assert_eq!(ppm_d(-SYNC_UNIT / 2), 0);
        assert_eq!(ppm_d(SYNC_UNIT), 4_000, "不感帯を越えた分だけ");
        assert_eq!(ppm_d(-SYNC_UNIT), -4_000);
        // 上限・slew が負でも絶対値で使う（`clamp` の min > max でパニックしない）。
        let neg = GlideParams {
            max_ppm: -10_000,
            slew_ppm: -(i32::MAX / 2),
            ..FREE
        };
        let ppm_n = |avg| glide_rb(1, avg).glide_ppm_with(&neg, Some(10_000), 16_667);
        assert_eq!(ppm_n(5 * SYNC_UNIT), 10_000, "負の上限は絶対値");
        assert_eq!(ppm_n(-SYNC_UNIT), -8_000);
    }

    #[test]
    fn glide_moves_at_most_slew_per_frame() {
        let mut rb = glide_rb(1, 5 * SYNC_UNIT);
        let p = GlideParams::DEFAULT;
        let up: Vec<i32> = (0..12)
            .map(|_| rb.glide_ppm_with(&p, Some(10_000), 16_667))
            .collect();
        let want: Vec<i32> = (1..=10)
            .map(|k| k * p.slew_ppm)
            .chain([10_000, 10_000])
            .collect();
        assert_eq!(up, want, "上限へ 1 枠 slew ずつ登る");
        rb.sync_avg = -5 * SYNC_UNIT;
        let down: Vec<i32> = (0..3)
            .map(|_| rb.glide_ppm_with(&p, Some(10_000), 16_667))
            .collect();
        assert_eq!(
            down,
            vec![
                10_000 - p.slew_ppm,
                10_000 - 2 * p.slew_ppm,
                10_000 - 3 * p.slew_ppm
            ],
            "符号が変わっても 1 枠 slew ずつ"
        );
    }

    #[test]
    fn glide_is_zero_when_it_must_not_move_the_grid() {
        let big = 5 * SYNC_UNIT;
        let zero = |mut rb: Rollback, rtt| rb.glide_ppm_with(&FREE, rtt, 16_667);
        assert_eq!(zero(glide_rb(1, big), Some(10_000)), 10_000);
        assert_eq!(zero(glide_rb(0, big), Some(10_000)), 0, "D 0");
        assert_eq!(zero(glide_rb(1, big), None), 0, "往復が未測定");
        let mut off = Rollback::new(Config::rollback(Seat::P2, 1, 8).with_time_sync(false));
        off.sync_avg = big;
        off.sync_samples = SYNC_MIN_SAMPLES;
        assert_eq!(zero(off, Some(10_000)), 0, "時刻合わせを切った");
        let mut few = glide_rb(1, big);
        few.sync_samples = SYNC_MIN_SAMPLES - 1;
        assert_eq!(zero(few, Some(10_000)), 0, "標本が足りない");
        let mut rb = glide_rb(1, big);
        for k in 1..=30 {
            assert_eq!(
                rb.glide_ppm_with(&FREE, Some(10_000), 16_667),
                10_000,
                "{k} 回目"
            );
        }
        assert_eq!(rb.glide_ppm_with(&FREE, Some(10_000), 16_667), 0, "31 回目");
        rb.sync_sample(50, 50);
        assert_eq!(rb.sync_idle, 0);
        let mut guest =
            Rollback::new(Config::rollback(Seat::P1, 1, 8).with_role(crate::lockstep::Role::Guest));
        guest.begin_seg(1);
        guest.sync_avg = big;
        guest.sync_samples = SYNC_MIN_SAMPLES;
        assert!(!guest.delay_known());
        assert_eq!(zero(guest, Some(10_000)), 0, "D 未定");
    }

    #[test]
    fn glide_rtt_gate_has_hysteresis_and_scales_with_d() {
        let big = 5 * SYNC_UNIT;
        let mut rb = glide_rb(1, big);
        let on = |rb: &mut Rollback, rtt_ms: u32| {
            rb.glide_ppm_with(&FREE, Some(rtt_ms * 1_000), 16_667) != 0
        };
        assert!(on(&mut rb, 32), "上限の中（32.3 ms まで）");
        assert!(on(&mut rb, 34), "寄せている間は 35.0 ms まで続ける");
        assert!(!on(&mut rb, 36), "戻りを越えたら止める");
        assert!(
            !on(&mut rb, 34),
            "止めた後は上限（32.3 ms）の中へ戻るまで寄せない"
        );
        assert!(!on(&mut rb, 33));
        assert!(on(&mut rb, 32), "上限の中へ戻れば寄せる");
        let mut d2 = glide_rb(2, big);
        assert!(on(&mut d2, 64), "D 2 は 64.6 ms まで");
        assert!(!on(&mut glide_rb(2, big), 66));
        assert!(!on(&mut glide_rb(1, big), 40), "D 1 の 40 ms は寄せない");
    }

    #[test]
    fn glide_gate_state_survives_a_seg_head_with_unknown_inputs() {
        let mut guest =
            Rollback::new(Config::rollback(Seat::P1, 1, 8).with_role(crate::lockstep::Role::Guest));
        let prime = |rb: &mut Rollback| {
            rb.sync_avg = 5 * SYNC_UNIT;
            rb.sync_samples = SYNC_MIN_SAMPLES;
        };
        prime(&mut guest);
        assert_ne!(guest.glide_ppm_with(&FREE, Some(32_000), 16_667), 0);
        assert!(guest.glide_on());
        guest.begin_seg(1);
        prime(&mut guest);
        assert!(!guest.delay_known());
        assert_eq!(guest.glide_ppm_with(&FREE, Some(34_000), 16_667), 0);
        assert_eq!(guest.glide_ppm_with(&FREE, None, 16_667), 0);
        assert!(guest.glide_on(), "状態を捨てた");
        // D が分かった後の最初の判定は戻りの側（35 ms）で続く。
        guest.delay_known = true;
        assert_ne!(guest.glide_ppm_with(&FREE, Some(34_000), 16_667), 0);
        let mut d0 = glide_rb(0, 5 * SYNC_UNIT);
        assert_eq!(d0.glide_ppm_with(&FREE, Some(10_000), 16_667), 0);
        assert!(!d0.glide_on());
    }

    #[test]
    fn time_sync_resets_at_the_switch_and_skips_until_both_cross_it() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 1, 16));
        let mut g = Nop::default();
        let f_sw = 10 + SWITCH_LEAD;
        // 相手も同じ予定で替える（D 1 → 3 の隙間 2 ワードを埋める）。
        let mut step = |rb: &mut Rollback, f: u32| {
            let ack = rb.local_next;
            let w = 0x2000 | f as u16;
            let inp = if f < f_sw {
                input_of(rb, f + 1, ack, &[w])
            } else if f == f_sw {
                input_of(rb, f + 1, ack, &[w - 1, w - 1, w])
            } else {
                input_of(rb, f + 3, ack, &[w])
            };
            assert!(matches!(rb.on_input(&inp), RecvOutcome::Accepted { .. }));
            rb.capture(f as u16).unwrap();
            let st = rb.input_point(&mut g);
            assert!(matches!(st, Step::Ran { .. }), "f{f}: {st:?}");
            rb.post_present(&mut g);
        };
        for f in 0..10 {
            step(&mut rb, f);
        }
        assert_eq!(rb.sync_samples, 10);
        assert_eq!(rb.press_delay(3), DelayPress::Scheduled { frame: f_sw });
        for f in 10..f_sw {
            step(&mut rb, f);
        }
        assert_eq!(
            rb.sync_samples, 10,
            "予定を持った後に標本を取った・替える前に捨てた"
        );
        let peer_top_before = rb.peer_top;
        step(&mut rb, f_sw);
        assert_eq!(
            (rb.sync_samples, rb.sync_avg),
            (0, 0),
            "替えたフレームで捨てていない"
        );
        assert!(rb.peer_top > peer_top_before, "peer_top を止めた");
        step(&mut rb, f_sw + 1);
        assert_eq!(rb.sync_samples, 1, "越えた後に標本を取らない");
        // 両者が同じ予定で替えたので偏らない（D の差の偏りが残れば 2L が +3 になり、待ちに入る）。
        for f in f_sw + 2..f_sw + 30 {
            step(&mut rb, f);
        }
        assert_eq!(rb.stats().waits_sync, 0);
        assert!(
            rb.sync_lead().is_some_and(|l| l.abs() < SYNC_UNIT / 2),
            "{:?}",
            rb.sync_lead()
        );
    }

    #[test]
    fn never_runs_more_than_r_ahead() {
        let mut rb = Rollback::new(Config::rollback(Seat::P1, 0, 3));
        let mut g = Nop::default();
        let mut ran = 0;
        for i in 0..10u16 {
            rb.capture(i).unwrap_or(());
            if matches!(rb.input_point(&mut g), Step::Ran { .. }) {
                ran += 1;
            }
            rb.post_present(&mut g);
            assert!(rb.frame() - rb.front() <= 3);
        }
        assert_eq!(ran, 3);
        assert_eq!(rb.stats().max_ahead_seen, 3);
    }
}
