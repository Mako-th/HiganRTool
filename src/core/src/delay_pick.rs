//! 遅れの量を選ぶ形の純粋な部分: 推奨値の式・初めの値・数字キーの押下の記録。両者が数字キーで変える（ゲストの押下はホストへの要求）。
//!
//! ## 推奨値（`recommend`）
//!
//! 片道 ≈ RTT / 2 を 1 フレーム（1000 / 60 ms）で割ったフレーム数を元にする。範囲は両方式とも `RECOMMEND_MIN`（1）〜`mode_pick::MAX_PICK_DELAY`（8）。
//! - delay 方式: 片道を切り上げる（相手の入力ワードが使うフレームまでに届く遅れの最小。これより小さいと毎フレーム待つ）。
//! - rollback: 片道の半分を D で吸い、残りを予測で埋める（半分を四捨五入し、ちょうど半分は遅れの少ない低い側）。片道を全部 D で吸うと
//!   遠い相手で入力の遅れが大きすぎる（往復 約 280 ms の実回線で D 8 より D 4 の方が遊びやすかった。1 組の体感）。
//!
//! 0 を推すのは、直近の往復の窓の最大が方式ごとのしきい値（`ZERO_MAX_ROLLBACK_MS` / `ZERO_MAX_DELAY_MS`）以下のときだけ
//! （`recommend_with_max`）。1 回でも揺れた回線では 0 にしない（D 0 は毎フレーム予測し、delay 方式の 0 は毎フレーム 1 往復を待つ）。
//! 最大が無い（窓が 3 個に満たない・往復を持たない呼び手）ときは 0 を推さない。
//!
//! RTT は生存の測り（`liveness::Liveness::rtt_ms`。受信スレッドが PONG を受けた時刻で測る）で、測れていなければ推奨は出さない（`None`）。
//!
//! ## 両者が選ぶ（`DelayPick`）
//!
//! - 押した瞬間ごとに 1 つ値を作る。作った値は `Connected::press_delay` へ渡し、ネットワーク側がホストなら予定（rollback）・切替（delay 方式）に、
//!   ゲストならホストへの要求にする。値を持ち続けて毎フレーム比べると、両者が変えたとき古い値を要求し続けて上書きし合う。
//! - delay 方式の初めの値は推奨値: マッチの始まり（`begin_match`）で、ホストがまだ何も押していなければ 1 回だけ作る。rollback の
//!   初めの値は AGREE の D（明示の選択）。ゲストは推奨値を出さない（2 つの値が競う）。
//! - 範囲は `bounds`（両形式とも 0〜`mode_pick::MAX_PICK_DELAY` = 8）。ホストはゲストの要求も同じ範囲に詰める。
//!
//! ## 数字キーと auto
//!
//! Ctrl+上段の数字の押下は `Connected::press_delay(x)` へ渡る。0〜8 はその値、9 は `AUTO`。auto は接続ごとの 1 つの旗でホストが持つ:
//! 9 で立ち、0〜8（ホストの押下もゲストの要求も）で降りる（後勝ち）。立っている間、ホストは 9 を押した瞬間・ゲストが auto を
//! 要求したとき・seg の最初の取得のときだけ D を `recommend`（直近 8 回の往復の中央値）にする。マッチの途中では追わない。
//! ゲストの auto の要求は INPUT の lat の delay の欄に `AUTO` を載せる。
//! seg の始まりの見直しだけは変わりにくくする（往復が境目で揺れてマッチごとに D が替わるのを避ける）: 上げるのは「往復 − `UP_MARGIN_MS`」で
//! 引いた推奨値が今の D より大きいときだけ（最大も同じ余裕で引く。0 から上がるのは最大がしきい値 + `UP_MARGIN_MS` を越えたとき）、
//! 下げるのは素の推奨値が今の D より小さい seg の始まりが `DOWN_RUN` 回続いたときだけ。規則は `AutoHold` に 1 つ。
//!
//! ## 並び
//!
//! メニューの「ディレイ」の ← → の並びは auto, 0F, 1F, …, 8F（auto が左端。端で止まり折り返さない）。動かし方は `DelayChoice::step` に 1 つ。
//!
//! 遅延の増分: 0（値は既存の INPUT の lat の欄で運ぶ）。
#![forbid(unsafe_code)]

use crate::frame::MAX_DELAY;
use crate::mode_pick::MAX_PICK_DELAY;
use crate::wire::Mode;

/// RTT（ms）からフレーム数への換算: 片道 RTT / 2 を 1 フレーム 1000 / 60 ms で割る = RTT × 3 / 100。丸めを浮動小数に任せず整数で持つ。
const PER_RTT_NUM: u64 = 3;
const PER_RTT_DEN: u64 = 100;
const _: () = assert!(
    PER_RTT_NUM * 1000 * 2 == PER_RTT_DEN * 60,
    "片道 = RTT / 2、1 フレーム = 1000 / 60 ms"
);

/// 選べる値の下限（← → と数字キーの 0〜8。両形式とも）。delay 方式の 0 は毎フレーム 1 往復を待つので止まりやすいが、`Lockstep` は受ける。
/// 推奨値の下限は別（`RECOMMEND_MIN`）。
pub const DELAY_MIN: u8 = 0;

/// 往復から引く推奨値の下限。0 を推すのは往復の窓の最大が下のしきい値以下のときだけ（`recommend_with_max`）。
pub const RECOMMEND_MIN: u8 = 1;

/// 往復の窓の最大がこれ以下なら推奨値を 0 にする（ms）。delay 方式の 0 は毎フレーム相手の語を待つので rollback の半分にした。
/// どちらも実測の裏付けは無く、決め直してよい値。
pub const ZERO_MAX_ROLLBACK_MS: u32 = 16;
pub const ZERO_MAX_DELAY_MS: u32 = 8;

/// 方式ごとの、推奨値 0 を許す往復の最大（ms）。
pub fn zero_max_ms(mode: Mode) -> u32 {
    match mode {
        Mode::Delay => ZERO_MAX_DELAY_MS,
        Mode::Rollback => ZERO_MAX_ROLLBACK_MS,
    }
}

/// 要求（ゲストの INPUT の lat の delay の欄）と押下で「auto」を表す値（数字キーの 9）。選べる範囲の外で、wire が通す上限
/// `frame::MAX_DELAY`（9）と同じなので、本文の形も検査も変えずに運べる。ホストが送る予定・切替がこの値になることは無い。
pub const AUTO: u8 = MAX_DELAY;
const _: () = assert!(MAX_PICK_DELAY < AUTO, "auto の値は選べる範囲の外");

/// 遅延の選び 1 つ（メニューの値と接続の始まりの値）。`raw` が押下の値（`Connected::press_delay` へ渡す形。auto は `AUTO`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayChoice {
    Auto,
    Fixed(u8),
}

impl DelayChoice {
    /// 押下の値から。範囲の外（`AUTO` 以外の 10 以上）は `None`。
    pub fn from_raw(x: u8) -> Option<DelayChoice> {
        if x == AUTO {
            Some(DelayChoice::Auto)
        } else {
            (x <= MAX_PICK_DELAY).then_some(DelayChoice::Fixed(x))
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            DelayChoice::Auto => AUTO,
            DelayChoice::Fixed(d) => d,
        }
    }

    pub fn index(self) -> usize {
        match self {
            DelayChoice::Auto => 0,
            DelayChoice::Fixed(d) => usize::from(d) + 1,
        }
    }

    /// 並びの中で `by`（-1 が ←、+1 が →）だけ動かした値。両端で止まる。範囲の外の `Fixed` は上限へ寄せてから動かす。
    pub fn step(self, by: i8) -> DelayChoice {
        let last = usize::from(MAX_PICK_DELAY) + 1;
        let i = self.index().min(last);
        let j = if by < 0 {
            i.saturating_sub(1)
        } else {
            (i + 1).min(last)
        };
        match j {
            0 => DelayChoice::Auto,
            _ => DelayChoice::Fixed((j - 1) as u8),
        }
    }
}

/// 往復の最大が無い呼び手向けの推奨値（delay 方式なら遅れ、rollback なら D）。RTT が測れていなければ `None`。
/// 下限は `RECOMMEND_MIN`（0 は推さない）、上限は `bounds`。
pub fn recommend(mode: Mode, rtt_ms: Option<u32>) -> Option<u8> {
    let scaled = u64::from(rtt_ms?) * PER_RTT_NUM;
    let frames = match mode {
        Mode::Delay => scaled.div_ceil(PER_RTT_DEN),
        // 片道のフレーム数（scaled / DEN）の半分。ちょうど半分は切り下げる。
        Mode::Rollback => (scaled + PER_RTT_DEN - 1) / (2 * PER_RTT_DEN),
    };
    Some(frames.clamp(u64::from(RECOMMEND_MIN), u64::from(bounds(mode).1)) as u8)
}

/// 推奨値。往復の窓の最大 `max_ms` がしきい値（`zero_max_ms`）以下なら 0、そうでなければ `recommend`（中央値 `rtt_ms` で引く）。
/// `max_ms` が `None` なら 0 を推さない。
pub fn recommend_with_max(mode: Mode, rtt_ms: Option<u32>, max_ms: Option<u32>) -> Option<u8> {
    let r = recommend(mode, rtt_ms)?;
    Some(if max_ms.is_some_and(|m| m <= zero_max_ms(mode)) {
        DELAY_MIN
    } else {
        r
    })
}

/// 選べる範囲 (下限, 上限)。ホストがゲストの要求を詰めるのにも使う。`mode` は、方式で範囲を分けたくなったとき呼び手を直さずに済むよう残す。
pub fn bounds(_mode: Mode) -> (u8, u8) {
    (DELAY_MIN, MAX_PICK_DELAY)
}

pub fn clamp(mode: Mode, x: u8) -> u8 {
    let (lo, hi) = bounds(mode);
    x.clamp(lo, hi)
}

// ---- auto の「マッチごとに見直す ＋ 変わりにくくする」----
//
// 往復が推奨値の境目（delay 方式の 1 と 2 は 33 ms で 1、34 ms で 2。rollback の 1 と 2 は 100 ms で 1、101 ms で 2）に乗ると、seg の始まりごとに
// 素の推奨値へ合わせては往復が数 ms 揺れるだけでマッチごとに D が替わるので、seg の始まりの見直しだけ「上げにくく・下げにくく」する。

/// 上げるときに往復から差し引く余裕（ms）。delay 方式で今 1 なら往復 39 ms 以上で 2（34〜38 ms は 1 のまま）。5 ms は往復の揺れ幅の見込みで、実測の裏付けは無い。
pub const UP_MARGIN_MS: u32 = 5;

/// 下げるのに要る、続けて「今の D より低い」と出た seg の始まりの数（間に「下げない」seg の始まりが挟まれば数え直し）。
pub const DOWN_RUN: u8 = 2;

/// 往復から引いた推奨値の組。`raw` は素、`up` は「往復 − `UP_MARGIN_MS`」で引いた上げる判断用の値（`raw` 以下）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recommend {
    pub raw: u8,
    pub up: u8,
}

impl Recommend {
    /// 往復（ms）から。測れていなければ `None`。最大を持たないので 0 は推さない。
    pub fn from_rtt(mode: Mode, rtt_ms: Option<u32>) -> Option<Recommend> {
        Recommend::from_rtt_max(mode, rtt_ms, None)
    }

    /// 往復の中央値と窓の最大（ms）から。測れていなければ `None`。`up` は両方から余裕を引く。引く値は 0 で止める。
    pub fn from_rtt_max(mode: Mode, rtt_ms: Option<u32>, max_ms: Option<u32>) -> Option<Recommend> {
        let raw = recommend_with_max(mode, rtt_ms, max_ms)?;
        let up = recommend_with_max(
            mode,
            rtt_ms.map(|r| r.saturating_sub(UP_MARGIN_MS)),
            max_ms.map(|m| m.saturating_sub(UP_MARGIN_MS)),
        )?;
        Some(Recommend { raw, up })
    }

    /// 余裕の無い組（`raw` = `up` = `r`）。往復を持たない検査と peer_sim 用。
    pub fn flat(r: u8) -> Recommend {
        Recommend { raw: r, up: r }
    }
}

/// `AutoHold::pick_head` の答え: 使う D と、変わりにくくして据え置いたか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadPick {
    pub d: u8,
    pub held: bool,
}

/// ホストが持つ auto の旗と、seg の始まりの見直しの規則。最初の適用・押した瞬間・ゲストの要求は素の推奨値をすぐ使い（`pick_now`）、
/// seg の始まりは余裕つきで見直す（`pick_head`）。数字（0〜8）で旗を降ろすと数えも捨てる（`lower`）。確保しない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoHold {
    on: bool,
    /// 旗が立ってから、まだ一度も推奨値を使っていない。測れる前に立てた旗の最初の適用も素の推奨値をすぐ使うため。
    fresh: bool,
    /// 続けて「今の D より低い」と出た seg の始まりの数と、そのうちの大きい推奨値（下げる先）。
    low_run: u8,
    low_max: u8,
}

impl AutoHold {
    /// 旗を立てる（もう立っていても立て直す: 次の適用は素の推奨値をすぐ使う）。
    pub fn raise(&mut self) {
        *self = AutoHold {
            on: true,
            fresh: true,
            ..AutoHold::default()
        };
    }

    pub fn lower(&mut self) {
        *self = AutoHold::default();
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    /// 押した瞬間・ゲストの要求・旗が立ってから最初の適用: 素の推奨値をすぐ使う。下げの数えは 0 に戻す。旗が降りている・測れていないときは `None`。
    pub fn pick_now(&mut self, rec: Option<Recommend>) -> Option<u8> {
        if !self.on {
            return None;
        }
        let rec = rec?;
        self.fresh = false;
        self.low_run = 0;
        self.low_max = 0;
        Some(rec.raw)
    }

    /// seg の始まりの見直し。`d` は今の D。最初の適用なら `pick_now` と同じ。それ以外は、`up` が `d` より大きければ上げ、素の推奨値が `d` より
    /// 小さい seg の始まりが `DOWN_RUN` 回続いたときだけ下げる（先はその間の大きい方）。旗が降りている・測れていないときは `None`。
    pub fn pick_head(&mut self, d: u8, rec: Option<Recommend>) -> Option<HeadPick> {
        if !self.on {
            return None;
        }
        let rec = rec?;
        if self.fresh {
            let x = self.pick_now(Some(rec))?;
            return Some(HeadPick { d: x, held: false });
        }
        if rec.up > d {
            self.low_run = 0;
            self.low_max = 0;
            return Some(HeadPick {
                d: rec.up,
                held: false,
            });
        }
        if rec.raw < d {
            self.low_run = self.low_run.saturating_add(1);
            self.low_max = self.low_max.max(rec.raw);
            if self.low_run >= DOWN_RUN {
                let x = self.low_max;
                self.low_run = 0;
                self.low_max = 0;
                return Some(HeadPick { d: x, held: false });
            }
            return Some(HeadPick { d, held: true });
        }
        self.low_run = 0;
        self.low_max = 0;
        // 素の推奨値が `d` 以上で上げる条件に届かなかった。素なら上がる値を据え置いたときだけ「変わりにくくした」と数える。
        Some(HeadPick {
            d,
            held: rec.raw > d,
        })
    }
}

/// 1 本の接続の、遅れの量の選び（ゲームのスレッドが持つ。確保しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayPick {
    host: bool,
    mode: Mode,
    /// 押した（か推奨値で作った）がまだネットワーク側へ渡していない値（`take` で取る）。
    pending: Option<u8>,
    /// 最後に作った値（ログ用）。
    last: Option<u8>,
    /// 数字キーの押下の回数。
    digits: u32,
    /// delay 方式の推奨値を出したか、それより先に数字キーで値を作ったか。
    first_done: bool,
    /// つなぐ前にメニュー（か ini の auto）で決めた遅延（ホストだけが使う）。
    start: Option<DelayChoice>,
}

impl DelayPick {
    pub fn new(host: bool, mode: Mode) -> Self {
        DelayPick {
            host,
            mode,
            pending: None,
            last: None,
            digits: 0,
            first_done: false,
            start: None,
        }
    }

    /// つなぐ前に決めた遅延を置く（ホストの `begin_match` が使う。ゲストは使わない。両者が決めると 2 つの値が競う）。
    pub fn set_start(&mut self, c: Option<DelayChoice>) {
        self.start = c;
    }

    /// 形式の切替で作り直す: 形式を替え、渡す前の値とつなぐ前の遅延は捨てる（切替の STEP の D が新しい形式の初めの値になる）。
    /// 押下の回数はそのまま。
    pub fn switch_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.pending = None;
        self.start = None;
        self.first_done = true;
    }

    pub fn is_host(&self) -> bool {
        self.host
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn bounds(&self) -> (u8, u8) {
        bounds(self.mode)
    }

    pub fn chosen(&self) -> Option<u8> {
        self.last
    }

    /// まだネットワーク側へ渡していない値（画面に出す値の上書き）。
    pub fn pending(&self) -> Option<u8> {
        self.pending
    }

    pub fn digits(&self) -> u32 {
        self.digits
    }

    /// マッチの始まり: ホストで、まだ値を作っていなければ 1 回だけ初めの値を作る。つなぐ前に決めた遅延（`set_start`）があればそれ、
    /// 無ければ delay 方式だけ今の推奨値（測れていなければ作らず、次のマッチの始まりで見る）。rollback の初めの値は AGREE の D のまま。
    /// ゲストは何もしない。
    pub fn begin_match(&mut self, rtt_ms: Option<u32>) {
        if !self.host || self.first_done {
            return;
        }
        if let Some(c) = self.start {
            self.pending = Some(c.raw());
            self.last = Some(c.raw());
            self.first_done = true;
            return;
        }
        if self.mode != Mode::Delay {
            return;
        }
        if let Some(r) = recommend(self.mode, rtt_ms) {
            self.pending = Some(r);
            self.last = Some(r);
            self.first_done = true;
        }
    }

    /// 数字キーの押下を呼び手が直に `Connected::press_delay` へ渡した後に知らせる: 渡す前の値（マッチの始まりの初めの値）は捨てる（数字が後勝ち）。
    /// ログ用の `chosen` は押した値。
    pub fn note_digit(&mut self, x: u8) {
        self.digits = self.digits.saturating_add(1);
        self.pending = None;
        self.last = Some(x);
        self.first_done = true;
    }

    pub fn take(&mut self) -> Option<u8> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_recommendation_rounds_the_one_way_frames_up() {
        let r = |ms| recommend(Mode::Delay, Some(ms));
        // 0〜33 ms は 1（推奨の下限）、34 ms で 2。
        assert_eq!(r(0), Some(1), "推奨の下限 1（0 は人が選ぶ値）");
        assert_eq!(r(1), Some(1));
        assert_eq!(r(33), Some(1), "0.99 フレーム");
        assert_eq!(r(34), Some(2), "1.02 フレーム");
        assert_eq!(r(66), Some(2), "1.98 フレーム");
        assert_eq!(r(67), Some(3), "2.01 フレーム");
        assert_eq!(r(233), Some(7), "6.99 フレーム");
        assert_eq!(r(234), Some(8), "7.02 フレーム");
        assert_eq!(r(266), Some(8), "7.98 フレーム");
        assert_eq!(r(267), Some(MAX_PICK_DELAY), "8.01 フレーム → 上限 8");
        assert_eq!(r(300), Some(MAX_PICK_DELAY), "9.00 フレームでも上限 8");
        assert_eq!(r(1_000), Some(MAX_PICK_DELAY));
        assert_eq!(r(u32::MAX), Some(MAX_PICK_DELAY), "桁あふれしない");
        assert_eq!(recommend(Mode::Delay, None), None);
        assert_eq!(MAX_PICK_DELAY, 8, "決めた範囲は 0〜8");
    }

    /// 境目は手で数えた値（片道のフレーム数 RTT × 3 / 100 の半分。ちょうど半分は低い側）。
    #[test]
    fn the_rollback_recommendation_is_half_the_one_way_frames() {
        let r = |ms| recommend(Mode::Rollback, Some(ms));
        assert_eq!(r(0), Some(1), "推奨の下限 1（D 0 は推さない）");
        assert_eq!(r(30), Some(1), "片道 0.9 フレームの半分");
        assert_eq!(r(100), Some(1), "片道 3.0 フレームの半分 1.5 は低い側");
        assert_eq!(r(101), Some(2), "1.515");
        assert_eq!(r(166), Some(2), "2.49");
        assert_eq!(r(167), Some(3), "2.505");
        assert_eq!(r(233), Some(3), "3.495");
        assert_eq!(r(234), Some(4), "3.51");
        assert_eq!(r(280), Some(4), "4.2（実回線で遊びやすかった値）");
        assert_eq!(r(300), Some(4), "4.5 は低い側");
        assert_eq!(r(301), Some(5), "4.515");
        assert_eq!(r(400), Some(6), "6.0");
        assert_eq!(r(500), Some(7), "7.5 は低い側");
        assert_eq!(r(501), Some(MAX_PICK_DELAY), "7.515 → 8");
        assert_eq!(r(u32::MAX), Some(MAX_PICK_DELAY), "上限・桁あふれしない");
        assert_eq!(recommend(Mode::Rollback, None), None);
        for ms in [34, 67, 100, 167, 234, 300] {
            assert!(
                r(ms) < recommend(Mode::Delay, Some(ms)),
                "{ms} ms: rollback は delay 方式より低い"
            );
        }
    }

    /// 境目は手で数えた値（rollback は最大 16 ms で 0・17 ms で 1、delay 方式は 8 ms で 0・9 ms で 1。中央値は小さくてよい）。
    #[test]
    fn zero_is_recommended_only_when_the_window_max_is_within_the_mode_threshold() {
        assert_eq!((ZERO_MAX_ROLLBACK_MS, ZERO_MAX_DELAY_MS), (16, 8));
        let rb = |med, max| recommend_with_max(Mode::Rollback, Some(med), Some(max));
        let dl = |med, max| recommend_with_max(Mode::Delay, Some(med), Some(max));
        assert_eq!(rb(2, 16), Some(0));
        assert_eq!(
            rb(2, 17),
            Some(1),
            "中央値が小さくても最大が越えれば 0 にしない"
        );
        assert_eq!(rb(16, 16), Some(0));
        assert_eq!(rb(0, 0), Some(0));
        assert_eq!(rb(100, 300), Some(1), "しきい値を越えたら今までの式");
        assert_eq!(rb(300, 300), Some(4));
        assert_eq!(dl(2, 8), Some(0));
        assert_eq!(dl(2, 9), Some(1));
        assert_eq!(dl(2, 16), Some(1), "rollback のしきい値では 0 にしない");
        assert_eq!(dl(70, 70), Some(3));
        // 最大が無い（窓が 3 個に満たない・往復を持たない呼び手）なら 0 を推さない。
        for mode in [Mode::Delay, Mode::Rollback] {
            assert_eq!(recommend_with_max(mode, Some(0), None), Some(1), "{mode:?}");
            assert_eq!(recommend_with_max(mode, None, Some(0)), None, "{mode:?}");
            assert_eq!(recommend_with_max(mode, None, None), None, "{mode:?}");
            assert_eq!(
                recommend(mode, Some(0)),
                Some(1),
                "{mode:?}: 最大の無い式は 0 を推さない"
            );
        }
    }

    /// `up` は中央値と最大の両方から 5 ms を引く: 0 から上がるのは最大がしきい値 + 5 ms を越えたとき（rollback は 22 ms、delay 方式は 14 ms）。
    #[test]
    fn the_pair_with_a_max_keeps_zero_until_the_max_clears_threshold_plus_margin() {
        let p = |mode, med, max| Recommend::from_rtt_max(mode, Some(med), Some(max));
        let pair = |raw, up| Some(Recommend { raw, up });
        assert_eq!(p(Mode::Rollback, 2, 16), pair(0, 0));
        assert_eq!(
            p(Mode::Rollback, 2, 17),
            pair(1, 0),
            "素は 1、最大 − 5 = 12 で 0"
        );
        assert_eq!(
            p(Mode::Rollback, 2, 21),
            pair(1, 0),
            "最大 − 5 = 16 はまだ 0"
        );
        assert_eq!(
            p(Mode::Rollback, 2, 22),
            pair(1, 1),
            "最大 − 5 = 17 で上がる"
        );
        assert_eq!(p(Mode::Delay, 2, 8), pair(0, 0));
        assert_eq!(p(Mode::Delay, 2, 9), pair(1, 0));
        assert_eq!(p(Mode::Delay, 2, 13), pair(1, 0), "最大 − 5 = 8 はまだ 0");
        assert_eq!(p(Mode::Delay, 2, 14), pair(1, 1), "最大 − 5 = 9 で上がる");
        assert_eq!(
            Recommend::from_rtt_max(Mode::Delay, Some(2), None),
            pair(1, 1),
            "最大が無ければ 0 を推さない"
        );
        assert_eq!(Recommend::from_rtt(Mode::Rollback, Some(0)), pair(1, 1));
        assert_eq!(Recommend::from_rtt_max(Mode::Delay, None, Some(1)), None);
    }

    #[test]
    fn bounds_and_clamp_are_zero_to_eight_for_both_modes() {
        for mode in [Mode::Delay, Mode::Rollback] {
            assert_eq!(bounds(mode), (0, 8), "{mode:?}");
            assert_eq!(clamp(mode, 0), 0, "{mode:?}: 0 を選べる");
            assert_eq!(clamp(mode, 8), 8);
            assert_eq!(clamp(mode, 9), 8, "{mode:?}: 9 は範囲の外");
            assert_eq!(clamp(mode, 255), 8);
        }
        assert!(AUTO > bounds(Mode::Rollback).1 && AUTO > bounds(Mode::Delay).1);
        assert_eq!(
            AUTO, MAX_DELAY,
            "auto の値は wire が通す上限（本文の形を変えない）"
        );
    }

    #[test]
    fn the_first_value_is_the_recommendation_once_for_the_delay_host() {
        let mut h = DelayPick::new(true, Mode::Delay);
        h.begin_match(None);
        assert_eq!(
            (h.pending(), h.chosen()),
            (None, None),
            "測れていない間は作らない"
        );
        h.begin_match(Some(70));
        assert_eq!(h.take(), Some(3), "70 ms → 2.1 フレーム → 3");
        assert_eq!(h.take(), None, "1 回取ったら空");
        h.begin_match(Some(10));
        assert_eq!(h.take(), None, "2 回目のマッチの始まりでは作らない");
        let mut p = DelayPick::new(true, Mode::Delay);
        p.note_digit(4);
        p.begin_match(Some(70));
        assert_eq!(p.take(), None, "押した後に推奨値で上書きした");
        for (host, mode) in [
            (false, Mode::Delay),
            (true, Mode::Rollback),
            (false, Mode::Rollback),
        ] {
            let mut q = DelayPick::new(host, mode);
            q.begin_match(Some(70));
            assert_eq!(q.take(), None, "{host} {mode:?}");
        }
    }

    #[test]
    fn a_digit_press_drops_the_value_not_yet_handed_over() {
        let mut p = DelayPick::new(true, Mode::Delay);
        p.begin_match(Some(70));
        assert_eq!(p.pending(), Some(3));
        p.note_digit(7);
        assert_eq!(p.take(), None, "渡す前の初めの値が数字より後に出る");
        assert_eq!(p.chosen(), Some(7));
        p.begin_match(Some(70));
        assert_eq!(p.take(), None, "数字を押した後に推奨値で上書きした");
        p.note_digit(AUTO);
        assert_eq!(p.chosen(), Some(AUTO));
        assert_eq!(p.digits(), 2);
    }

    #[test]
    fn the_order_has_auto_at_the_left_end_and_stops_at_both_ends() {
        use DelayChoice::{Auto, Fixed};
        for (from, by, want) in [
            (Auto, 1, Fixed(0)),
            (Auto, -1, Auto),
            (Fixed(0), -1, Auto),
            (Fixed(0), 1, Fixed(1)),
            (Fixed(5), -1, Fixed(4)),
            (Fixed(7), 1, Fixed(8)),
            (Fixed(8), 1, Fixed(8)),
            (Fixed(8), -1, Fixed(7)),
            (Fixed(9), 1, Fixed(8)),
            (Fixed(9), -1, Fixed(7)),
        ] {
            assert_eq!(from.step(by), want, "{from:?} {by:+}");
        }
        for (i, c) in std::iter::once(Auto)
            .chain((0..=MAX_PICK_DELAY).map(Fixed))
            .enumerate()
        {
            assert_eq!(c.index(), i);
            assert_eq!(DelayChoice::from_raw(c.raw()), Some(c));
        }
        assert_eq!(DelayChoice::from_raw(MAX_PICK_DELAY + 2), None);
    }

    #[test]
    fn the_start_choice_is_the_hosts_first_value_once() {
        for mode in [Mode::Delay, Mode::Rollback] {
            let mut h = DelayPick::new(true, mode);
            h.set_start(Some(DelayChoice::Auto));
            h.begin_match(Some(70));
            assert_eq!(h.take(), Some(AUTO), "{mode:?}: auto が初めの値");
            h.begin_match(Some(70));
            assert_eq!(h.take(), None, "{mode:?}: 2 回目の始まりでは作らない");
            let mut f = DelayPick::new(true, mode);
            f.set_start(Some(DelayChoice::Fixed(5)));
            f.begin_match(Some(70));
            assert_eq!(f.take(), Some(5), "{mode:?}: 決めた値は推奨値に負けない");
            let mut g = DelayPick::new(false, mode);
            g.set_start(Some(DelayChoice::Auto));
            g.begin_match(Some(70));
            assert_eq!(g.take(), None, "{mode:?}: ゲストは初めの値を作らない");
        }
        let mut r = DelayPick::new(true, Mode::Rollback);
        r.begin_match(Some(70));
        assert_eq!(
            r.take(),
            None,
            "rollback の決めていない初めの値は AGREE の D"
        );
    }

    /// `AutoHold` は方式によらず推奨値の組だけを見る。境目は delay 方式の式で手で数える。
    fn rec(rtt: u32) -> Option<Recommend> {
        Recommend::from_rtt(Mode::Delay, Some(rtt))
    }

    /// 旗を立てて最初の適用を済ませた AutoHold（以降の seg の始まりは余裕つきの規則で動く）。
    fn used(first_rtt: u32) -> AutoHold {
        let mut a = AutoHold::default();
        a.raise();
        assert!(a.pick_now(rec(first_rtt)).is_some());
        a
    }

    /// 境目は手で数えた値（delay 方式は RTT × 3 / 100 の切り上げ: 33 ms で 1、34 ms で 2。「−5 ms」の側は 38 ms で 1、39 ms で 2。
    /// rollback は 100 ms で 1、101 ms で 2。「−5 ms」の側は 105 ms で 1、106 ms で 2）。
    #[test]
    fn the_recommend_pair_takes_the_margin_off_only_the_up_side() {
        assert_eq!(UP_MARGIN_MS, 5);
        let rb = |ms| Recommend::from_rtt(Mode::Rollback, Some(ms));
        let pair = |raw, up| Some(Recommend { raw, up });
        assert_eq!(rb(100), pair(1, 1));
        assert_eq!(rb(101), pair(2, 1), "素は 2、−5 ms の側は 96 ms で 1");
        assert_eq!(rb(105), pair(2, 1));
        assert_eq!(rb(106), pair(2, 2));
        assert_eq!(rb(0), pair(1, 1), "0 から 5 を引いても桁あふれしない");
        assert_eq!(Recommend::from_rtt(Mode::Rollback, None), None);
        {
            let mode = Mode::Delay;
            let p = |ms| Recommend::from_rtt(mode, Some(ms));
            let pair = |raw, up| Some(Recommend { raw, up });
            assert_eq!(p(33), pair(1, 1), "{mode:?}");
            assert_eq!(
                p(34),
                pair(2, 1),
                "{mode:?}: 素は 2、−5 ms の側は 29 ms で 1"
            );
            assert_eq!(p(38), pair(2, 1), "{mode:?}: −5 ms で 33 ms → 1");
            assert_eq!(p(39), pair(2, 2), "{mode:?}: −5 ms で 34 ms → 2");
            assert_eq!(p(71), pair(3, 2), "{mode:?}: 2.13 フレーム → 3、66 ms → 2");
            assert_eq!(p(72), pair(3, 3), "{mode:?}: 67 ms → 3");
            assert_eq!(
                p(0),
                pair(1, 1),
                "{mode:?}: 0 から 5 を引いても桁あふれしない"
            );
            assert_eq!(p(4), pair(1, 1), "{mode:?}");
            assert_eq!(p(5), pair(1, 1), "{mode:?}");
            assert_eq!(
                p(u32::MAX),
                pair(MAX_PICK_DELAY, MAX_PICK_DELAY),
                "{mode:?}: 上限"
            );
            assert_eq!(
                Recommend::from_rtt(mode, None),
                None,
                "{mode:?}: 測れていない"
            );
        }
        assert_eq!(Recommend::flat(3), Recommend { raw: 3, up: 3 });
    }

    /// 境目（今 1 で往復 34〜38 ms は据え置き・39 ms 以上で 2）は手で書いた値。
    #[test]
    fn the_up_move_needs_the_margin() {
        for ms in 34..=38u32 {
            let mut a = used(33);
            let p = a.pick_head(1, rec(ms)).unwrap();
            assert_eq!(p, HeadPick { d: 1, held: true }, "往復 {ms} ms は据え置き");
        }
        let mut a = used(33);
        let p = a.pick_head(1, rec(39)).unwrap();
        assert_eq!(p, HeadPick { d: 2, held: false }, "往復 39 ms で上がる");
        // 上げる先は「−5 ms」の側の値（71 ms: 素は 3、−5 ms の側は 2）。
        let mut b = used(33);
        assert_eq!(b.pick_head(1, rec(71)).unwrap().d, 2);
        let mut c = used(33);
        assert_eq!(
            c.pick_head(1, rec(20)).unwrap(),
            HeadPick { d: 1, held: false }
        );
    }

    /// 手で書いた並び（今 3 → 低い 2 回で 2 / 今 5 の素 2 → 素 3 は大きい方の 3）。
    #[test]
    fn the_down_move_needs_two_low_heads_in_a_row_and_restarts_when_one_is_between() {
        assert_eq!(DOWN_RUN, 2);
        let mut a = used(100);
        assert_eq!(
            a.pick_head(3, rec(40)).unwrap(),
            HeadPick { d: 3, held: true },
            "素 2 < 3 が 1 回目"
        );
        assert_eq!(
            a.pick_head(3, rec(40)).unwrap(),
            HeadPick { d: 2, held: false }
        );
        assert_eq!(
            a.pick_head(2, rec(10)).unwrap(),
            HeadPick { d: 2, held: true }
        );
        let mut b = used(100);
        assert_eq!(b.pick_head(3, rec(40)).unwrap().d, 3, "低");
        assert_eq!(b.pick_head(3, rec(100)).unwrap().d, 3, "同じ（素 3）");
        assert_eq!(
            b.pick_head(3, rec(40)).unwrap().d,
            3,
            "低（数え直しの 1 回目）"
        );
        assert_eq!(b.pick_head(3, rec(40)).unwrap().d, 2, "低 2 回で下がる");
        let mut c = used(100);
        assert_eq!(c.pick_head(3, rec(40)).unwrap().d, 3, "低");
        assert_eq!(
            c.pick_head(3, rec(140)).unwrap().d,
            5,
            "上げ（素 5・−5 ms でも 5）"
        );
        assert_eq!(
            c.pick_head(5, rec(40)).unwrap().d,
            5,
            "低（数え直しの 1 回目）"
        );
        for (first, second) in [(40u32, 70u32), (70, 40)] {
            let mut d = used(100);
            assert_eq!(d.pick_head(5, rec(first)).unwrap().d, 5);
            assert_eq!(
                d.pick_head(5, rec(second)).unwrap().d,
                3,
                "素 2 と 3 のうち大きい 3（{first}, {second}）"
            );
        }
    }

    #[test]
    fn the_press_and_the_first_use_take_the_raw_value_at_once() {
        // 9 を押した瞬間: 往復 34 ms は素 2（余裕つきなら今 1 のまま）。
        let mut a = AutoHold::default();
        a.raise();
        assert_eq!(a.pick_now(rec(34)), Some(2));
        let mut b = used(100);
        assert_eq!(b.pick_head(3, rec(40)).unwrap().d, 3, "低（1 回目）");
        assert_eq!(b.pick_now(rec(100)), Some(3), "押した瞬間は素");
        assert_eq!(
            b.pick_head(3, rec(40)).unwrap().d,
            3,
            "数えが 0 に戻っている"
        );
        let mut c = AutoHold::default();
        c.raise();
        assert_eq!(c.pick_now(None), None, "測れていない間は値を作らない");
        assert_eq!(c.pick_head(3, None), None, "測れていない");
        assert_eq!(
            c.pick_head(3, rec(10)).unwrap(),
            HeadPick { d: 1, held: false },
            "最初の適用は素をすぐ（下げの 2 回を待たない）"
        );
        assert_eq!(
            c.pick_head(2, rec(10)).unwrap(),
            HeadPick { d: 2, held: true }
        );
        c.raise();
        assert_eq!(c.pick_head(2, rec(10)).unwrap().d, 1);
        let mut off = AutoHold::default();
        assert!(!off.is_on());
        assert_eq!(off.pick_now(rec(34)), None);
        assert_eq!(off.pick_head(1, rec(34)), None);
    }

    /// 0 の出入り（rollback。中央値 2 ms）: 0 から上がるのは最大 22 ms から、0 へ下がるのは素が 0 の seg の始まりが 2 回続いたとき。
    #[test]
    fn auto_holds_zero_until_the_max_clears_the_margin_and_drops_to_zero_after_two_heads() {
        let r = |max| Recommend::from_rtt_max(Mode::Rollback, Some(2), Some(max));
        let used0 = |max| {
            let mut a = AutoHold::default();
            a.raise();
            assert!(a.pick_now(r(max)).is_some());
            a
        };
        for max in 17..=21u32 {
            let mut a = used0(4);
            assert_eq!(
                a.pick_head(0, r(max)).unwrap(),
                HeadPick { d: 0, held: true },
                "最大 {max} ms: 素は 1 だが 0 のまま"
            );
        }
        let mut a = used0(4);
        assert_eq!(
            a.pick_head(0, r(22)).unwrap(),
            HeadPick { d: 1, held: false },
            "最大 22 ms で 0 から上がる"
        );
        let mut a = used0(300);
        assert_eq!(
            a.pick_head(1, r(10)).unwrap(),
            HeadPick { d: 1, held: true },
            "素 0 < 1 の 1 回目は据え置き"
        );
        assert_eq!(
            a.pick_head(1, r(10)).unwrap(),
            HeadPick { d: 0, held: false },
            "2 回続けて 0 へ下がる"
        );
        let mut b = used0(300);
        assert_eq!(b.pick_head(1, r(10)).unwrap().d, 1);
        assert_eq!(
            b.pick_head(1, r(40)).unwrap().d,
            1,
            "間に 0 でない回が挟まれば数え直し"
        );
        assert_eq!(b.pick_head(1, r(10)).unwrap().d, 1);
        // 押した瞬間・最初の適用は素の 0 をすぐ使う。
        let mut c = AutoHold::default();
        c.raise();
        assert_eq!(
            c.pick_head(5, r(10)).unwrap(),
            HeadPick { d: 0, held: false }
        );
        let mut d = used0(300);
        assert_eq!(d.pick_now(r(10)), Some(0));
    }

    #[test]
    fn lowering_the_flag_drops_the_count() {
        let mut a = used(100);
        assert!(a.is_on());
        assert_eq!(a.pick_head(3, rec(40)).unwrap().d, 3, "低（1 回目）");
        a.lower();
        assert!(!a.is_on());
        assert_eq!(a.pick_head(3, rec(40)), None);
        a.raise();
        // 立て直しの最初の適用は素の推奨値（今 3、素 2）。
        assert_eq!(a.pick_now(rec(40)), Some(2));
        assert_eq!(
            a.pick_head(3, rec(40)).unwrap(),
            HeadPick { d: 3, held: true }
        );
    }
}
