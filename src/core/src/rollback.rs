//! rollbackのモードのステートマシン。入力ワードの交換は`lockstep.rs`と同じ形。
//! 違うのは、相手の入力ワードが無いtickを予測で進め、後で確定の入力ワードで進め直すところだけ。
//! ソケットも時計もゲームのメモリも持たない。ゲームの側は[`Window`]で受ける。
//! ゲームスレッドで呼ぶのでヒープを使わず、パニックしうる添字を使わない。
//!
//! N = これから更新するフレーム。F = 確定の前線（Fより前は両者の入力ワードが本物）。
//! S_F = F の始まりの状態のコピー（1つだけ）。R = 予測の上限。D = 自分の入力の遅れ（`apply_frame = capture_frame + D`）。
//! - Nの相手の入力ワードが届いていればそれで進める。無ければ相手の直前の確定の入力ワードの繰り返しで進める。
//!   `N − F ≥ R`なら入力ワードが届いていても待つ（進め直しの費用の上限を守るため）。
//!   予測のtickが場面の要求を立てたら、それより先は更新せずFが越えるまで待つ（[`Wait::Boundary`]）。
//! - コピーはPresentの後に取る。
//!   N > F で相手の入力ワードが届いていればFを進める: 書き戻し → 確定の入力ワードで更新 → コピー → 予測の入力ワードでNまで更新。
//!   外へ渡すのは確定した (frame, p1, p2)と確定のtickの始まりで取るCHECK（30フレームごと）だけ。
//! - 時刻合わせ（[`Wait::Sync`]）: 相手のINPUTから自分が何フレーム先にいるかを見積もる。
//!   見積もりが閾値を越えたら描画だけの1フレームを追加する。その後[`SYNC_GAP`]回の更新は待たない。
//! - segの始まりのD（D_start。`seg_delay`）はホストが`begin_seg`で決める。ホストはそのsegの全部のINPUTにlatを載せる。
//!   seg 1以降のゲストはD未定。ホストのINPUTを1通受けるまで`capture`が`CaptureError::NoDelay`を返す。
use crate::check::is_check_frame;
use crate::delay_pick::{AUTO, AutoHold, Recommend};
use crate::frame::{MAX_DELAY, is_prefill};
use crate::lockstep::{CaptureError, DelayPress, RecvOutcome, Role, WISH_FRAMES, Wish, by_seat};
use crate::wire::{Check, Input, LatChange, MAX_WORDS, Seat};
use crate::word_ring::{RING, Ring, in_window};

pub use crate::wire::Mode;

/// ホストがDの予定を出すとき次の取得のフレームから何フレーム先をFにするか。
/// ゲストがFより前に予定を受け取れる量（片道4フレーム ＋ Rの既定8）から式で決めた値で、実測ではない。
/// これより遅く届いても替えるのが遅れるだけ。正しさは崩れない。
/// 押してから替わるまでの長さ（約200 ms）でもある。
pub const SWITCH_LEAD: u32 = 12;

pub const D_SWITCH_LOG: usize = 16;

/// segの途中のDの予定を持った記録。ホストは出したとき、ゲストは受けたとき。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DSwitch {
    pub seg: u16,
    /// この取得のフレームから`to`（自分の台で替えるフレーム）。
    pub frame: u32,
    /// ネットワークに載ったF（ゲストが遅れて受けたら`frame`より小さい）。
    pub wire_frame: u32,
    pub from: u8,
    pub to: u8,
    /// 相手の押下から来た予定か。
    pub by_peer: bool,
}

/// Rの下限と上限。
pub const MIN_AHEAD: u8 = 1;
pub const MAX_AHEAD: u8 = 16;
/// Rの既定（1フレームあたり平均 約2.5 ms・最大 約4.6 msに収まる数。外挿）。
pub const DEFAULT_AHEAD: u8 = 8;
/// Dの既定（片道ぶん1フレームを隠す）。
pub const DEFAULT_DELAY: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    mode: Mode,
    /// AGREEで決まったD（seg 0のD）。以後のsegは`Rollback::seg_delay`。
    delay: u8,
    max_ahead: u8,
    seat: Seat,
    /// 役（接続の手順で決まる）。rollbackのモードでsegのDを決めてINPUTに載せるのはホストだけ。
    role: Role,
    /// 時刻合わせを使うか。ネットワークに載せない各自の設定。
    time_sync: bool,
}

impl Config {
    /// rollbackのモード。`delay`は0〜9、`max_ahead`は1〜16に詰める。役の既定はホスト（本物の役は`with_role`）。
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

    /// 役を設定する。delay方式では効かない。
    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    /// 時刻合わせを有効にする / 切る。delay方式では元から効かない。
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

    /// AGREEで決まったD（seg 0のD）。
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
    /// 入力の地点の1回（画面に出る本物の更新）。
    Live,
    /// 進め直し: 確定の入力ワード。
    Confirmed,
    Predicted,
}

/// ゲームに渡す1回の更新。`p1` / `p2`はfeedのサイドの順（`by_seat`済み）。
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

/// CHECKに載せるゲームの値（`wire::Check`の4フィールド）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CheckValues {
    pub rng_state: u16,
    pub rng_count: u32,
    pub life_1p: u8,
    pub life_2p: u8,
}

/// ゲームの側。dllと検査の偽のゲームが実装する。
pub trait Window {
    /// いまの状態を「Fのフレームの始まりの状態」としてコピーする（1枠。前のコピーは捨てる）。
    fn save(&mut self, frame: u32);
    /// コピーした状態へ書き戻す。つなぎと場面の要求のフィールドも戻す。
    fn restore(&mut self, frame: u32);
    fn tick(&mut self, t: Tick) -> TickReport;
    /// CHECKの値を取る。確定のtickの始まりでだけ呼ばれる。
    fn check_values(&mut self) -> CheckValues;
    /// 進め直しの始まり（SE・BGM・文字の数を記録する）。
    fn resim_begin(&mut self) {}
    /// 進め直しの終わり（記録した値へ戻し、進め直しの回の出力は全部捨てる）。
    fn resim_end(&mut self) {}
    /// 確定した (frame, p1, p2)。frameの昇順に1回ずつ。
    fn confirmed(&mut self, _frame: u32, _p1: u16, _p2: u16) {}
    /// 確定のtickの始まりで取ったCHECK（30フレームごと。frame 0を含む）。
    fn check(&mut self, _c: &Check) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 更新を1回走らせた。`real`は相手の入力ワードが本物かどうか。
    /// `confirmed`は確定の状態の上での本物の入力ワードかどうか。
    Ran {
        frame: u32,
        real: bool,
        confirmed: bool,
    },
    /// 描画だけのフレーム（次のdo_frameで同じフレームをやり直す。取得はし直さない）。
    Wait(Wait),
    /// 場面の境目が確定した。
    SceneEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// `capture`がまだ呼ばれていない（呼び手の順番の間違い）。
    NotCaptured,
    Ahead,
    Boundary,
    /// 内部の食い違い。`stats().faults`に数え、以後このsegでは進まない。
    Fault,
    /// 自分が先にいるので描画だけの1フレームで待つ。
    /// 相手の入力ワードを待つ待ちではない。そのため呼び手は同じフレームの中で繰り返し試さず、次のフレームで`input_point`をもう一度呼ぶ。
    Sync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Nothing,
    Saved,
    /// Fを進めた。`confirmed` = 確定の入力ワードで更新した回数。`predicted` = 予測の入力ワードで更新した回数。
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
    /// 進め直しの回数（`stall_rewinds`は入力の地点で行った分）。
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
    /// そのうちsegの始まりの`SYNC_HEAD_FRAMES`フレームを過ぎてからの数（時計のずれで起きる待ち）。
    pub waits_sync_late: u32,
    /// 予測のtickが要求を立てて待ちに入った数。
    /// 待ちは書き戻しで必ず解ける。行き先は場面の終わり（`holds_ended`）/ 要求が消えて続行（`holds_released`）/ 予測の入力ワードで立て直し（`holds_rearmed`。新しい`holds_set`に数える）のどれか。
    /// 場面がすべて終わった後は`holds_set == holds_ended + holds_released + holds_rearmed`。
    pub holds_set: u32,
    pub holds_ended: u32,
    pub holds_released: u32,
    pub holds_rearmed: u32,
    /// 進め直しでNが戻った分の合計（境目より先の更新を捨てた数）。
    pub discarded_ticks: u32,
    /// Nが戻った後に同じフレームの取得を求められた数（取得は捨てて送った入力ワードを使う）。
    pub recaptured: u32,
    /// 観測した N − F の最大（R以下）。
    pub max_ahead_seen: u32,
    pub scenes_ended: u32,
    /// 同じapply_frameに違う入力ワードが来た数（最初の入力ワードを守った）。
    pub conflicts: u32,
    /// 内部の食い違い（0でなければバグ）。
    pub faults: u32,
    /// segごとのDの記録（決まった順の (seg, D)。新しい`SEG_D_LOG`個をリングバッファに持つ）。総数は`seg_d_count`。
    pub seg_d_log: [(u16, u8); SEG_D_LOG],
    pub seg_d_count: u32,
    /// ゲストがD未定のために取得を待った数（segの始まりでホストのそのsegの最初のINPUTを待った分）。
    pub delay_waits: u32,
    /// 時刻合わせの見積もり（`sync_lead()`と同じ単位。正 = 自分が先）の記録: サンプルの数・合計・最大・最小。
    /// `sync_samples`が0なら最大・最小は意味が無い。
    pub sync_samples: u32,
    pub sync_lead_sum: i64,
    pub sync_lead_max: i32,
    pub sync_lead_min: i32,
    /// 予定を持った記録（新しい`D_SWITCH_LOG`個をリングバッファに持つ）と総数。
    /// 今のDと同じ値の予定は数えない。届く前に次の予定で置き換えた予定は外す（`d_cancelled`）。
    pub d_switch_log: [DSwitch; D_SWITCH_LOG],
    pub d_switches: u32,
    pub d_filled: u32,
    pub d_dropped: u32,
    pub d_late: u32,
    /// 届く前に次の予定で置き換えられて効かずに消えた予定の数。
    pub d_cancelled: u32,
    /// ホストがゲートの閉じている間の押下・要求を持っておいた数。
    pub d_held: u32,
    /// ゲスト: 要求した数 / `WISH_FRAMES`で諦めた数。ホスト: 要求を取った数 / seqが古くてlatを無視した数。
    pub wish_sent: u32,
    pub wish_expired: u32,
    pub wish_taken: u32,
    pub wish_dup: u32,
    /// ホスト: segの始まりのDを最初の取得の前の押下で置き換えた数（マッチの外で押した値が次のsegの始まりのDになる経路）/ auto（`delay_pick::AUTO`）の値をDにした数。
    pub d_head_sets: u32,
    pub d_auto: u32,
    /// segの始まりの見直しで素の推奨値ならDが替わるのに据え置いた数（`AutoHold::pick_head`の`held`）。
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
// 先の側が相手のINPUTだけから「何フレーム先にいるか」を見積もる。
// そして描画だけのフレームを少しずつ追加して揃える（GGPOのframe advantageと同じ考え）。

/// 見積もりの単位。「先にいるフレーム数の2倍」を1/256フレーム単位の整数で持つ。
/// 粗いと移動平均の整数の割り算がサンプルの近くで止まり、閾値に届かなくなる。
pub const SYNC_UNIT: i32 = 256;
/// サンプル（`wl − 2·top + ack`）の偏りの補正。床で数えた2つの差の和なので位相が一様なら2Lより平均1小さい。
/// 補正しないと先の側のサンプルが閾値に届かず、予測するのに待たない帯ができる。
const SYNC_BIAS: i64 = 1;
/// 移動平均の重み（新しいサンプルを1/16ずつ混ぜる）。
/// 軽いと揺れでサンプルが1つ跳ねただけで閾値を越える（ALPHA 4では揺れの大きいネットワークで余計な待ちが多かった）。
const SYNC_ALPHA: i32 = 16;
/// サンプルがこの数たまるまでは待たない。平均が0から混ざり始めるので実質は働かない。初期値や重みを変えたときの保険。
const SYNC_MIN_SAMPLES: u8 = 3;
/// 待ち始める先の量（D ≥ 1。補正後の2Lで1.25）。補正後のサンプルは予測する帯の中で2、手前で0か1。
/// 1.5にすると帯の縁で平均が届かず、先の側が3割ほど予測したまま待たない。1.0以下だと帯の手前でも待つ。
const SYNC_THRESHOLD: i32 = 5 * SYNC_UNIT / 4;
/// D = 0 のsegの閾値（補正後の2Lで2.5）。D = 0 で待って得をするのは φ ≥ T + w_AB のときだけ。
/// D ≥ 1の閾値のままだと後ろの側の予測が大きく増える（模型で確認）。
const SYNC_THRESHOLD_D0: i32 = 5 * SYNC_UNIT / 2;
/// 1回待った後に次に待つまでに進める更新の数。1回に1フレームだけ待つ。急に止めず少しずつ揃える。
pub const SYNC_GAP: u8 = 3;
/// segの始まりからこのフレームまでの時刻合わせの待ちは2台の読み込みの位相差を揃える待ちとして数え分ける（`Stats::waits_sync_late`）。
/// 実機の読み込みの差は数百ms（15フレーム前後）。待ちの間隔を含めて50フレーム前後で揃うので、300フレーム（5秒）あれば十分。
pub const SYNC_HEAD_FRAMES: u32 = 300;
// 0だと待った直後の同じNの呼び出しがもう一度待ちになる。
const _: () = assert!(SYNC_GAP >= 1);
// 平均の割り算が止まる幅（ALPHA / UNITフレーム）は閾値とサンプルの間の余裕（D ≥ 1で0.25）より狭くする。
const _: () = assert!(SYNC_ALPHA < SYNC_UNIT / 4);

// ---- 格子の寄せ ----

/// 格子を寄せる量の決め方（`tests/rollback_sim.rs`の`glide_table`の掃引で決めた）。
/// 平均`a`（1/`SYNC_UNIT`）が不感帯`±dead`の外へ出た分に`gain_ppm`を掛け、`max_ppm`で抑える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlideParams {
    /// 平均が不感帯を1.0（= `SYNC_UNIT`）越えるごとに1フレームの長さを変える量（100万分率）。
    pub gain_ppm: i32,
    /// 1フレームの長さの変化の上限（100万分率。±）。
    pub max_ppm: i32,
    /// 前の枠からの変化の上限（100万分率。±）。目標が急に変わっても1フレームの長さは1枠にこれだけずつしか動かない。
    pub slew_ppm: i32,
    /// 不感帯の半幅（1/`SYNC_UNIT`）。
    pub dead: i32,
    /// 相手のINPUTがサンプルにならないまま過ぎたフレームの数がこれを越えたら寄せない（途切れたネットワークで古い平均のまま動かさない）。
    pub stale_frames: u8,
    /// 寄せてよいネットワークの上限（往復の長さが`2·D·T`のこの割合（%）まで。一度寄せ始めたら`rtt_off_pct`まで）。
    /// RTTが`2·D·T`に近いと寄せても予測が消えず、止まりの方が予測の合計が小さい（模型D 1で確認）。
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
    /// このsegの始まりのD（D_start。segの途中では変わらない）。apply_frameがこれより小さいフレームは両者とも0（`is_prefill`）。
    /// 予測の「まだ何も確定していない」もこれで読む。ゲストがD未定の間は前のsegの最後のDを仮に書き込んであるだけ。
    seg_start_delay: u8,
    /// このsegのDが決まっているか。rollbackのゲストは`begin_seg`の後falseで、ホストのlat付きINPUTを受けたらtrue。
    delay_known: bool,
    /// 予定: 取得のフレーム`sched_f`以降は`sched_x`、それより前は`pre_d`（`delay_at`）。segの始まりは {0, D_start}。
    sched_f: u32,
    sched_x: u8,
    pre_d: u8,
    /// ネットワークに載る（ゲストは受けた）予定のF。ホストは`sched_f`と同じ。ゲストが遅れて受けたら`sched_f`より小さい。予定の新旧はこれで比べる。
    sched_wire_f: u32,
    /// 予定を持った回数（ゲストの要求が通ったかを見る印。segをまたいで数える）。
    sched_gen: u32,
    /// 今の予定を`Stats::d_switch_log`に記録したか。
    sched_logged: bool,
    /// 取得を済ませた一番先のフレーム + 1（新しい取得か、進め直しの取り直しかを見分ける）。
    capture_hi: u32,
    /// ホスト: ゲストがDを知ったと分かったか（ゲート）と、ゲートが閉じている間の押下（後勝ち）。
    guest_knows: bool,
    held: Option<(u8, bool)>,
    /// ホスト: 最後に取ったゲストの要求のseq（segで戻さない）。
    last_wish_seq: u32,
    /// ゲスト: 載せている要求と、最後に使ったseq。
    wish: Option<Wish>,
    wish_seq: u32,
    /// ホスト: autoの旗とsegの始まりの見直しの規則（接続ごとに1つ）。
    /// 旗が立っている間は押した瞬間とゲストの要求で素の推奨値（`rec`）をすぐ使う。segの始まりの最初の取得は余裕をかけて見直す。数字の押下で旗が降りる（後勝ち）。
    auto: AutoHold,
    /// ホスト: 呼び手が最後に渡した推奨値（直近の往復の中央値から。測れていなければ`None`）。
    rec: Option<Recommend>,
    /// 時刻合わせ: このフレームの取得で見積もりを捨てる（自分が替えるフレーム）。
    /// 相手のtopとackがこのapply_frameを越えるまでサンプルを取らない（サンプルの式`(wl − top) − (top − ack)`は両者のDが違う間2(D_自分 − D_相手)偏る。式からの推定で実測ではない）。
    sync_reset_at: Option<u32>,
    sync_resume: Option<u32>,
    n: u32,
    f: u32,
    captured: bool,
    local: Ring,
    local_next: u32,
    remote: Ring,
    /// 相手から次に要るapply_frame（送るack）。
    remote_next: u32,
    peer_ack: u32,
    /// 手元のコピーS_Fのフレーム。
    snap: Option<u32>,
    /// 予測のtickが場面の要求を立てたフレーム（Fが越えるまで待つ）。
    hold: Option<u32>,
    ended: bool,
    broken: bool,
    prev: Option<Input>,
    prev_end: u32,
    stats: Stats,
    // ---- 時刻合わせ。segごとに捨てる ----
    /// 相手の入力ワードで今までに見た一番先の位置（相手のINPUTの base + count の最大）。
    peer_top: u32,
    /// 「先にいるフレーム数の2倍」の移動平均（1/`SYNC_UNIT`単位。正 = 自分が先）。
    sync_avg: i32,
    sync_samples: u8,
    /// 次に待てるようになるまでの更新の数。更新でだけ減るので待った直後の同じNの呼び出しは待たない。
    sync_cool: u8,
    /// 最後の標本から数えたフレームの数。
    sync_idle: u8,
    /// 格子を寄せてよいネットワークと見ているか（上下の閾値に幅を持たせるため前の結果を覚える）。
    glide_ok: bool,
    /// 前の枠に返したppm（`slew_ppm`で変化を抑えるため）。
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

    /// 新しいsegに入る（読み込みに入るとき）。コピー・待ち・場面の終わりの印は捨てる。`scene_ended`が真になってから呼ぶのが普通（途中で抜けるなら残りの予測は捨てる）。
    /// そのsegのD: ホストとdelay方式はここで決める（前のsegの最後の値。ゲートが閉じていて持っていた押下があればその値）。
    /// rollbackのゲストは未定にしてホストのINPUTで知る（`on_input`）。
    pub fn begin_seg(&mut self, seg: u16) {
        self.prev = Some(self.build_input());
        self.prev_end = self.f;
        self.seg = seg;
        // 予定していて届かなかった値も含む。
        let last = self.held.take().map_or(self.sched_x, |(x, _)| x);
        self.place_head(last, true);
    }

    /// 形式の切替でこのステートマシンがマッチを動かす側に替わるとき（キャラセレクトの間 —— 最初の取得の前）に、segの始まりを新しい D = `d` で作り直す。
    /// 前のsegのバックアップ`prev`は触らない。ホストはautoの旗を`auto`に合わせる。
    /// seg 0だけはゲストもDを知っている形で作る（ホストが先に始まりを替えられない。`head_open`）。
    /// seg 1以降のゲストは`begin_seg`と同じくD未定にする。ゲストの要求と持っていた押下は捨てる。
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

    /// segの始まりの形を`last`（始まりのD。ゲストが待つなら仮の値）で作る。`guest_waits`はrollbackのゲストがDを知らない形で作るかどうか。
    fn place_head(&mut self, last: u8, guest_waits: bool) {
        if self.rollback_mode() && self.cfg.role == Role::Guest && guest_waits {
            // 入力ワードの置き場の起点は仮の値。ホストのINPUTでDを知ったとき`adopt_delay`が直す。
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

    /// 場面の境目が確定したかどうか。真なら`begin_seg`して抜ける。
    pub fn scene_ended(&self) -> bool {
        self.ended
    }

    pub fn holding(&self) -> bool {
        self.hold.is_some()
    }

    fn rollback_mode(&self) -> bool {
        self.cfg.mode == Mode::Rollback
    }

    /// このsegの始まりのD（D_start。ゲストが未定の間は前のsegの最後のD。`delay_known`で見分ける）。
    /// 穴埋めの判定（`is_prefill`）はこれを読む。`config().delay()`はseg 0のD。
    pub fn seg_delay(&self) -> u8 {
        self.seg_start_delay
    }

    /// 取得のフレーム`n`のD（予定から決める）。今のDを1つの変数で持つと進め直しでNが戻って同じnを取り直すときに食い違う。
    pub fn delay_at(&self, n: u32) -> u8 {
        if n >= self.sched_f {
            self.sched_x
        } else {
            self.pre_d
        }
    }

    /// 今のD（次に新しく取るフレームのD）。時刻合わせの閾値・格子の寄せ・表示・記録はこれを読む。穴埋めの判定は`seg_delay`。
    pub fn cur_delay(&self) -> u8 {
        self.delay_at(self.capture_hi)
    }

    /// 画面に出す「目標」のD: ゲストは要求している値、ホストはゲートの前に持っている押下、どちらも無ければ最新の予定の値。
    pub fn shown_delay(&self) -> u8 {
        match self.cfg.role {
            // autoの要求は値ではない（9を画面に出さない）。
            Role::Guest => self.wish.map(|w| w.x).filter(|&x| x != AUTO),
            Role::Host => self.held.map(|(x, _)| x),
        }
        .unwrap_or(self.sched_x)
    }

    /// ゲスト: 今出している要求の値（autoは`delay_pick::AUTO`）。無ければ`None`。ホストは常に`None`。
    pub fn own_wish(&self) -> Option<u8> {
        self.wish.map(|w| w.x)
    }

    /// ホスト: autoの旗が立っているかどうか。ゲストは常にfalse（ゲストの画面に出す旗は`wire::HostState`で知る）。
    pub fn auto_on(&self) -> bool {
        self.auto.is_on()
    }

    /// ホスト: 推奨値を渡す（約1秒ごとに往復を測り直したとき）。
    /// 使うのはautoを押した瞬間・ゲストがautoを要求したとき・segの始まりの最初の取得のときだけ。
    /// 測れていなければ`None`（旗だけ立てて値は次の機会に使う）。
    pub fn set_recommend(&mut self, r: Option<Recommend>) {
        self.rec = r;
    }

    pub fn recommend(&self) -> Option<Recommend> {
        self.rec
    }

    /// 相手がフレーム`n`を取ったと言える相手の入力ワードの数（`base + count`の最大）の下限。相手が自分と同じ予定で取るとみなす。
    /// Dを下げた後の捨てる取得は入力ワードを作らないので、次に入力ワードを作る取得の分まで待つ。peer_simの「付いていく形」が使う。
    /// 今の予定だけで数えるので1つ前の予定の切替より前のnでは外れうる。
    pub fn peer_top_for(&self, n: u32) -> u32 {
        // nより前の取得で作られた一番先のapply_frame。
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
                // nの取得は捨てられる。次に入力ワードを作る取得（apply = c + x > b となる最初のc）。
                b + 1
            }
            _ => mine,
        };
        apply + 1
    }

    /// このsegのDが決定済みかどうか。
    /// ゲストは`begin_seg`の後ホストのそのsegのINPUTを受けるまでfalse。その間`capture`は`CaptureError::NoDelay`。
    pub fn delay_known(&self) -> bool {
        self.delay_known
    }

    /// 遅れの量の押下（自分のCtrl＋数字）。ホストは予定を出す（ゲートが閉じていれば持っておく）。ゲストはホストへ要求する。
    /// 範囲は呼び手が詰める（`delay_pick::DelayPick`）。ここは0〜`MAX_DELAY`の外だけ断る。delay方式では何もしない（`DelayPress::Refused`）。
    ///
    /// `x`が`delay_pick::AUTO`（数字キーの9）ならauto: ホストは旗を立てて素の推奨値をすぐ使う（`auto_now`）。
    /// ゲストはホストへautoを要求する（latのdelayのフィールドにAUTOを載せる）。
    /// 0〜8の値はホストの旗を降ろす（後勝ち）。
    pub fn press_delay(&mut self, x: u8) -> DelayPress {
        self.press_delay_forced(x, false)
    }

    /// `press_delay`の、同じ値でも要求するバージョン。
    /// ゲストはホストのautoの旗を知らない。「今の値と同じ」で要求を出さないとホストの旗が降りない。
    /// 呼び手はゲストの画面にautoの印が立っていて0〜8を押したとき`force`を真にする。ホストの側は見ない。
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

    /// ホスト: 押した瞬間・ゲストのautoの要求でDを素の推奨値にする（`AutoHold::pick_now`）。推奨値が無ければ何もしない。
    fn auto_now(&mut self, by_peer: bool) -> DelayPress {
        bump(&mut self.stats.d_auto);
        match self.auto.pick_now(self.rec) {
            Some(x) => self.host_press(x, by_peer),
            None => DelayPress::Unchanged,
        }
    }

    /// ホスト: segの始まりの最初の取得で旗が立っていればDを見直す（`AutoHold::pick_head`。上げにくく・下げにくく）。
    /// 今のDには画面に出す目標`shown_delay`を使う（ゲートの前に持った押下の9を同じ見直しで取り消さないため）。
    /// マッチの途中では呼ばない。
    fn auto_head(&mut self) {
        bump(&mut self.stats.d_auto);
        if let Some(p) = self.auto.pick_head(self.shown_delay(), self.rec) {
            if p.held {
                bump(&mut self.stats.d_auto_held);
            }
            self.host_press(p.d, false);
        }
    }

    /// segの始まりのD（D_start）をこの場で置き換えてよいかどうか。条件は次が全部揃うこと: ホスト・このsegの最初の取得の前・2つ目以降のseg。
    /// seg 0は不可。ゲストが先に古いDで入力ワードを送りうるので、替えるとframe 0のlat（`BadLat`）と入力ワードの置き場が食い違って止まる。
    /// 2つ目以降のsegのゲストはD未定。ホストの最初のINPUTを受けるまで1ワードも送らない。
    fn head_open(&self) -> bool {
        self.rollback_mode()
            && self.cfg.role == Role::Host
            && self.seg != 0
            && self.capture_hi == 0
            && !self.captured
    }

    /// segの始まりのDをこの場で`x`にする（マッチの外で押した値が次のsegの始まりのDになる）。入力ワードの置き場の起点・予定・記録を作り直す。
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
        // 記録（`begin_seg`が追加したこのsegの (seg, D)）のDを直す。
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

    /// ホストの押下（自分のCtrl＋数字 か、取ったゲストの要求）。ゲートが開いていれば予定を出し、閉じていれば持っておく（後勝ち）。
    /// このsegの最初の取得の前なら、segの始まりのDをこの場で替える（`set_seg_head`）。
    fn host_press(&mut self, x: u8, by_peer: bool) -> DelayPress {
        if self.head_open() {
            return self.set_seg_head(x);
        }
        if !self.guest_knows {
            if self.shown_delay() == x {
                return DelayPress::Unchanged;
            }
            // 最新の予定と同じ値に戻したら持っていた押下を取り下げる。
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

    /// 予定を {wire_f（ネットワークのF）, f（この台で替える取得のフレーム）, x} に置き換える。`f`より前のDは今のD（`cur_delay`）:
    /// 置き換える前の予定にまだ届いていなければ、その予定は効かずに消える。
    /// ホストもゲストも自分の取得の進みで同じ規則を使う。そのため前の予定を受け損ねたゲストもホストと同じDの列になる。
    /// 前の予定に先に届いたゲストだけが新しいFまでその値で取る（公平さが一時崩れるだけ）。
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
        // `f`はネットワークから来る値。足し算であふれて例外を出さないよう飽和させる（溢れたら「いつまでも越えない」側）。
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

    /// ゲスト: 新しい予定を受けた・取得を進めた後に要求をやめるかを見る（押した後の予定で値が同じ / 回数の上限）。
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

    /// 取得をD未定のために待ったことを数える（`capture`を呼ばずに待つ呼び手のため）。
    pub fn note_delay_wait(&mut self) {
        bump(&mut self.stats.delay_waits);
    }

    /// ゲスト: そのsegのDをホストのINPUTで知った。入力ワードの置き場の起点をDに合わせる（D未定の間は何も取っていない）。
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

    /// フレーム`n`の自分の入力を取る（1フレームに1回）。
    /// Nが戻った後に同じフレームを求められたら（`n < capture_hi`）取り直さない。入力ワードはもう列にあり相手にも送ってあるため。
    /// 新しいフレームのapply_frameは`n + delay_at(n)`。
    /// Dを上げた境目では隙間を直前の自分の入力ワードで埋める。下げた後のk回は捨てる。
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
        // autoの旗が立っているホストはこのsegの最初の取得の前にその時点の推奨値でD_startを見直す。
        // 送るのはこの取得の後なのでゲストには最初から新しいD_startが見える。
        if self.auto.is_on() && n == 0 && self.cfg.role == Role::Host {
            self.auto_head();
        }
        let apply = n + u32::from(self.delay_at(n));
        if apply < self.local_next {
            // Dを下げた後: このapply_frameの入力ワードはもう送ってある（書き換えると遅れて届いた古いINPUTと見分けられない）。
            bump(&mut self.stats.d_dropped);
        } else {
            // 相手がまだ持っていない入力ワードと進め直しで使う入力ワード（F以降）を上書きしない。埋める隙間も含めて一番先のapply_frameで検証する。
            let oldest_needed = self.f.min(self.peer_ack);
            if apply - oldest_needed >= RING as u32 {
                return Err(CaptureError::Full);
            }
            if apply > self.local_next {
                // Dを上げた境目: 隙間を直前の自分の入力ワードで埋める（まだ入力ワードが無ければ0）。0で埋めると押し続けた射撃が離れる。
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
            // 自分が替えたフレーム: 前のDで取った見積もりを捨てる。
            self.sync_reset_at = None;
            self.sync_avg = 0;
            self.sync_samples = 0;
        }
        self.settle_wish(true);
        Ok(())
    }

    /// 送るINPUT（取得の直後の1回と待ち中の4 msごとの再送）。中身は相手のackから自分の列の終わりまで古い方から最大32ワード。
    /// latにはrollbackのホストが毎回載せる今の予定（segの始まりは frame = 0・delay = D_start。途中で替えたら {F, x}）と、ゲストの要求（{seq, x}。やめるまで）が入る。
    /// ゲストがD未定の間は入力ワード0個・base 0・ack 0（latなし）。
    pub fn build_input(&self) -> Input {
        if !self.delay_known {
            return Input {
                seg: self.seg,
                ack: 0,
                base: 0,
                lat: None,
                stamp: crate::wire::Stamp::NONE,
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
            stamp: crate::wire::Stamp::NONE,
            count: n as u8,
            words,
        }
    }

    pub fn build_prev_input(&self) -> Option<Input> {
        self.prev
    }

    /// 届いたINPUTを受ける。捨てるときは中身を1つも使わない。
    /// 判定は`Lockstep::on_input`と同じ。違いはlatと、先の入力ワードを保持できる範囲が「F + 枠の数」の手前までなこと。
    /// rollbackのホストが受けたframe 0は`GuestLat`、frame ≥ 1は要求として取る。
    /// ゲストが受けたホストのINPUTが次のどれかなら`BadLat`: latが無い / delayが0〜9の外 / D未定なのに frame ≠ 0 / frame 0でD_startと違う値 / 知っている予定と同じFで違う値。
    /// D未定のゲストはここでDを採用して入力ワードの置き場を作る。
    pub fn on_input(&mut self, inp: &Input) -> RecvOutcome {
        if inp.seg != self.seg {
            if self.prev.is_some_and(|p| p.seg == inp.seg) {
                return RecvOutcome::PrevSeg {
                    reply: inp.ack < self.prev_end,
                };
            }
            return RecvOutcome::OtherSeg;
        }
        // このINPUTでDを知る（ゲストがD未定のとき）。検証が全部通ってから確定するので検証は「Dを知った後」の起点で行う。
        let mut adopt = None;
        // ゲストが受けた新しい予定 / ホストが受けたゲストの要求（検証が全部通ってから使う）。
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
                        // ゲートがあるのでD未定のゲストへ F > 0 の予定は来ない（来たら取り決めの違反）。
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
                    // 古いパケット（遅れて着いた・入れ替わった）。latだけ無視して入力ワードは受ける。
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
            // 予定を受けた時点で既にFを過ぎて取っていたら次の取得から替える（取った入力ワードは書き換えない）。
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
                // 要求を受けた = ゲストはこのsegのDを知っている。ゲートを開けてから押下として扱う。
                self.open_gate();
                if l.frame > self.last_wish_seq {
                    self.last_wish_seq = l.frame;
                    bump(&mut self.stats.wish_taken);
                    if l.delay == AUTO {
                        // ゲストのauto: 旗を立てて、素の推奨値をすぐ使う（変わりにくくするのはsegの始まりの見直しだけ）。
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
                // 入力ワードが載った = ゲストはこのsegのDを知って取得している。
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

    /// 相手のINPUT 1通から「自分が相手より何フレーム先にいるか」のサンプルを取る。
    /// `wl` = 自分が作った入力ワードの数（`local_next`）。`top` = 相手が作った入力ワードの数（INPUTの base + count）。
    /// `ack` = 相手がそのINPUTを出した時点で受け取っていた自分の入力ワードの数。
    /// 片道w・自分がLフレーム先なら`wl − top = w + L`、`top − ack = w − L`なので`(wl − top) − (top − ack) = 2L`。
    /// wもDも消えるのでRTTの測定は要らない。
    /// サンプルにするのは相手の入力ワードが前へ進んだINPUTだけ。入力ワードが抜けてackが古いときはLを小さく見積もる向きに偏る（待ちすぎない側）。
    /// 実際の数は整数（フレームの床）なので平均して2Lより1小さい。`SYNC_BIAS`で足して戻す。
    fn sync_sample(&mut self, top: u32, ack: u32) {
        if !self.cfg.time_sync() || top <= self.peer_top {
            return;
        }
        self.peer_top = top;
        if let Some(t) = self.sync_resume {
            // Dの予定を持ってから両者の入力ワードが切替を越えるまではサンプルを取らない（両者のDが違う間は式が2(D_自分 − D_相手)偏る）。
            // `peer_top`は単調のまま進める。
            if top <= t || ack <= t {
                return;
            }
            self.sync_resume = None;
        }
        self.sync_idle = 0;
        let two_l = i64::from(self.local_next) - 2 * i64::from(top) + i64::from(ack) + SYNC_BIAS;
        let x = (two_l.clamp(-1000, 1000) as i32) * SYNC_UNIT;
        // 平均は0（釣り合い）から始める。
        // 最初のサンプルをそのまま平均にすると、segの始まりのサンプル（最初の1往復は相手のackが動いていない）が閾値を越えたまま数十サンプル残る。
        // その結果、釣り合った2台がどちらもsegの始まりで1回余計に待つ。
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

    /// 自分が相手より先にいるフレーム数の見積もり（1/`SYNC_UNIT`単位。正 = 先）。サンプルが無い間はNone。
    pub fn sync_lead(&self) -> Option<i32> {
        (self.sync_samples > 0).then_some(self.sync_avg / 2)
    }

    /// 次の1フレームの長さを何ppm変えるか（正 = 伸ばす、負 = 縮める）。**1フレームに1回**呼ぶ。格子を自分で刻める側だけが使う。
    /// `rtt_us`は生存確認の往復、`frame_us`は1フレームの長さ。
    /// 平均が不感帯の中なら0。それ以外は「平均が0から外れた分に比例して戻す」だけ。
    /// 0を返す場合: 時刻合わせを切った設定・サンプルが`SYNC_MIN_SAMPLES`に満たない間・最後のサンプルから`stale_frames`を越えた間・D = 0・Dが未定・往復が測れていない・往復が長い（`GlideParams::rtt_on_pct`）。
    pub fn glide_ppm_with(&mut self, p: &GlideParams, rtt_us: Option<u32>, frame_us: u32) -> i32 {
        self.sync_idle = self.sync_idle.saturating_add(1);
        // D = 0 は寄せない。先の側は後ろの側の予測を減らすために先にいたい。
        // 平均を0へ寄せると後ろの側の予測が増える（模型で確認）。
        // 両側が予測しない位置の幅`2·D·T − RTT`は今のDで決まる。
        let d = u64::from(self.cur_delay());
        // D未定・往復が測れていない間は0を返すだけ。判定の状態（`glide_ok`）は変えない。
        // 状態を捨てるとsegの始まりのたびに上限側で判定し直す。
        // その結果、往復が上限と戻りの間（32〜35 ms）にあるネットワークでは対戦のホストは寄せ続け、対戦のゲストだけが切れる。
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
            // 上限・slewは絶対値で使う（負の値でも`clamp`の min > max でパニックしない）。
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

    /// 今のDに合う待ち始めの量（`SYNC_THRESHOLD` / `SYNC_THRESHOLD_D0`）。segの途中でDを替えたら替えた後のDで読む。
    fn sync_threshold(&self) -> i32 {
        if self.cur_delay() == 0 {
            SYNC_THRESHOLD_D0
        } else {
            SYNC_THRESHOLD
        }
    }

    /// 時刻合わせで次の更新の代わりに描画だけのフレームを1つ追加するかどうか。
    /// 追加するなら、その効果（2Lが2減る）を見積もりから先に引く。
    /// 効果が相手のINPUTに映るのは片道の後なので、引かないと同じ先を何回も数えて行き過ぎる。
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

    /// 予測の入力ワード: 相手の直前の確定の入力ワードの繰り返し。まだ何も確定していなければ0（穴埋めと同じ）。
    /// その境目はsegの始まりのD（`seg_start_delay`）で読む。
    /// 途中で替えたDで読むと、Dを上げた後に確定していた入力ワードを「まだ無い」と読んで0を予測する。
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
        // 止まっている間の進め直し。1回でFはNか相手の入力ワードの手前まで進むので3周目は要らない（念のための上限）。
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
            // コピーが無いのはsegの最初の更新の前（N == F）だけ。それ以外は食い違い。
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

    /// Fを進める: 書き戻し → 確定の入力ワードでF' まで更新 → コピー → 予測の入力ワードでNまで更新。戻り値は (確定の入力ワードの更新の数, 予測の入力ワードの更新の数)。
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
        // 予測のtickが立てた要求は書き戻しで消える。確定の入力ワードで同じtickが立て直せば場面の終わり、立てなければ続ける。
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
                // 確定の状態の上で要求が立った。これより先の入力ワードは次のsegのもの。
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
                // 予測のtickが要求を立てた。これより先は更新しない（Nを戻して待つ）。
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
    //! 小さな検査（中身の検査は`tests/rollback_sim.rs`）。
    use super::*;

    /// フレームの長さ（µs）。時刻合わせの φ・wの掃引の単位。
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
            stamp: crate::wire::Stamp::NONE,
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
        // 相手は3ワード（top = 4）を作り、自分の入力ワードを2つ受け取っていた（ack = 3）。2L = 7 − 8 + 3 = 2 → L = 1フレーム先。
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
        // 自分が4フレーム先（2L = 8）のサンプルを30個追加する。ackが自分の入力ワードの数を越える並びはINPUTでは作れないので、サンプルを追加する関数を直に呼ぶ。
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

    /// 先にいる時間 φ・フレームの長さT・片道w_AB / w_BA（µs）のネットワークで遅れDの2台が出す (wl, top, ack)の列を式から作る（位相を掃引するため）。
    /// サンプルは床の差2つの和になる。kは10から（後ろの側の φ でackがDを割らないように）。
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
                    // 縁の前後1 msは床の境でサンプルが混ざるので比べない。
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
                // 掃引が両側を通っている（片側だけなら全部待つ・全部待たない実装も通る）。
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

    // ---- 格子の寄せ（1フレームの長さを、先にいるフレーム数の平均が0へ戻るように変える） ----

    fn glide_rb(d: u8, avg: i32) -> Rollback {
        let mut rb = Rollback::new(Config::rollback(Seat::P2, d, 8));
        rb.sync_avg = avg;
        rb.sync_samples = SYNC_MIN_SAMPLES;
        rb
    }

    /// 上限・変化の制限を外したパラメータ一式（式そのものを見る）。
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
        // 上限・slewが負でも絶対値で使う（`clamp`の min > max でパニックしない）。
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
        // Dが分かった後の最初の判定は戻りの側（35 ms）で続く。
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
        // 相手も同じ予定で替える（D 1 → 3の隙間2ワードを埋める）。
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
        // 両者が同じ予定で替えたので偏らない（Dの差の偏りが残れば2Lが +3になり、待ちに入る）。
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
