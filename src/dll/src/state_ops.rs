//! 台本のマッチの状態の操作（無敵・ライフ・ゲージ）を、入力の地点でゲームのメモリへ書く判断（pure。unsafe 無し）。
//!
//! 「いつ効くか」は `higanr_core::script::Engine`（`inv` / `take_pending`）が決め、ここは毎 tick、入力の地点
//! （`script_win::tick_inner` の終わり。Player の更新 —— 優先度 15・16 —— より前）で:
//! - 無敵の側の Player に `+0x1B78 = 2.0f` と `+0x1B7C = 2` の両方を書く（int だけでは毎 tick float から作り直される）。
//! - 1 回だけの値（ライフ・ゲージ）を、書ける側だけ書いて取り出す。書けない側は `Engine` が次の tick へ持ち越す。
//! - 書けるのは、Player へのポインタが 0 でなく読み込み中でない tick だけ（読み込みのスレッドが Player を作り直している間に書かない）。
//! - 書く前に、その Player の署名を見る: ライフ `+0xA8` が 0〜`LIFE_MAX`、ゲージ `+0x30388` が 0〜`GAUGE_MAX` の有限の float
//!   （溜める関数の頭打ち 400.0 と、ゲームが置く定数 0 / 100.0（`0x0041CABF` `0x0041E522` `0x0041EF72`）/ 1.0（減らす関数
//!   `0x0041F310` の下限 `0x0041F32B`））。外れても「書かない」側に倒れるだけ。合わなければ、その側には何も書かず記録する
//!   （解放された Player や、読み違えたアドレスを書き換えないため）。
//! - 台本が終わった・止まったら書かない（無敵は値が 2 なので次の tick の減算で自然に切れる）。書けずに残っていた
//!   1 回だけの値は捨て、捨てたことを記録する。
//! - ネットワーク（lockstep）の実行と、巻き戻しの実験の実行では書かずに断る（`Refuse`）。ネットワーク: 書くのは自分のゲームのメモリだけなので、
//!   本物 2 台では片方だけの状態が変わり、次の CHECK で列がずれる。巻き戻し: 期間の中で書いた 1 回だけの値は、進め直し
//!   （台本を進めない）で書き直されず、偽の食い違いになる。一度断ったら、その実行の間は断り続ける（ネットワークが途中で止まっても
//!   書き始めない）。peer_sim が相手のネットワークの実行も断る。
//!
//! ゲームのメモリへの読み書きは `StateMem` の向こうに置く（本物は `script_win::RealTickIo`。Player はヒープなので、落ちない
//! 読み方・書き方 —— 自分のプロセスへの `ReadProcessMemory` / `WriteProcessMemory` —— で）。ゲームのスレッドで走るので、
//! 確保・ファイル・ロック待ち・`unwrap` を置かない。記録は決まった大きさの原子変数の表（`StateLog`）に書き、
//! 読み取りのスレッドが文にする（`StateLog::text`）。
//!
//! 遅延の増分: 台本が状態の操作を使わない tick は `Engine` の値を 2 つ見るだけ。使う tick は `ReadProcessMemory` 最大 6 回
//! （側ごとにポインタ・ライフ・ゲージ）・`WriteProcessMemory` 最大 8 回（数 µs〜十数 µs の見込み。測っていない）。検査の道具なので、製品の実行（台本なし）は 0。
//! 仕組みは thprac の花映塚対応の無敵（被弾処理の呼び出しを飛ばす書き換え）を参考に、値の書き込みに変えた（コードはコピーしていない）。
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use higanr_core::script::{Engine, GAUGE_SET_MAX, LIFE_SET_MAX, LIFE_SET_MIN, SetOnce, Status};

use crate::th09_addr::{
    GAUGE_MAX, INV_TIMER_VALUE, LIFE_MAX, LIFE_MIN, P1_PLAYER_PTR, P2_PLAYER_PTR, PLAYER_GAUGE,
    PLAYER_INV_TIMER_F, PLAYER_INV_TIMER_I, PLAYER_LIFE,
};

// 台本が受け付ける範囲（core）と、ゲームの範囲（th09_addr）が食い違えば建たない（core は花映塚のアドレスを知らないので同じ数を持つ）。
const _: () = assert!(
    LIFE_SET_MIN == LIFE_MIN as u32
        && LIFE_SET_MAX == LIFE_MAX as u32
        && GAUGE_SET_MAX == GAUGE_MAX
);

pub const PLAYER_SLOTS: [u32; 2] = [P1_PLAYER_PTR, P2_PLAYER_PTR];

/// ゲームのメモリの読み書き（本物は `script_win::RealTickIo`、検査では偽物）。どれも確保しない・パニックしない。
pub trait StateMem {
    /// 読み込み中か（`[0x4A7EB4]` が 0 でない）。
    fn loading(&mut self) -> bool;
    /// 落ちない読み方で 4 バイト。読めなければ `None`。
    fn read_u32(&mut self, addr: u32) -> Option<u32>;
    /// 落ちない書き方で 4 バイト（リトルエンディアン）。書けなければ false。
    fn write_u32(&mut self, addr: u32, v: u32) -> bool;
}

/// 書かずに断る実行（`apply` の `refuse`。値は記録の語にそのまま入れる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refuse {
    /// ネットワーク（lockstep）が動いている。
    Net = 1,
    /// 巻き戻しの実験（E1-2）が入っている。
    Rollback = 2,
}

impl Refuse {
    fn text(code: u32) -> &'static str {
        match code {
            1 => "ネットワーク（lockstep）の実行。書けば自分のゲームだけが変わり、2 台の列がずれる",
            2 => {
                "巻き戻しの実験（E1-2）の実行。期間の中の書き込みは進め直しで再現されず、偽の食い違いになる"
            }
            _ => "（知らない理由）",
        }
    }
}

/// 記録の件数の上限（それより先は数えるだけ）。
pub const LOG_EVENTS: usize = 64;

// 記録の種類（`StateLog` の語 1 の下位 8 ビット）。
const EV_INV: u32 = 1;
const EV_INV_WRITTEN: u32 = 2;
const EV_LIFE: u32 = 3;
const EV_GAUGE: u32 = 4;
const EV_DROP_LIFE: u32 = 5;
const EV_DROP_GAUGE: u32 = 6;
/// 断った（値は `Refuse` の番号）。その実行で 1 回だけ記録する。
const EV_REFUSED: u32 = 7;
/// 断ったので捨てた 1 回だけの値。
const EV_REFUSED_LIFE: u32 = 8;
const EV_REFUSED_GAUGE: u32 = 9;
/// 署名が合わなかった（側ごとに最初の 1 回だけ記録する）。値はライフの語か、ゲージの float のビット、読めなかったアドレス。
const EV_SIG_LIFE: u32 = 10;
const EV_SIG_GAUGE: u32 = 11;
const EV_SIG_READ: u32 = 12;

/// 書いた記録（ゲームのスレッドだけが書き、読み取りのスレッドが `text` で読む）。1 件は 3 語（tick の番号・種類 |
/// 側 << 8 | 書けた << 16・値）。書き手は件数を Release で進め、読み手は Acquire で読んでから中身を読む。
pub struct StateLog {
    events: [[AtomicU32; 3]; LOG_EVENTS],
    n: AtomicU32,
    last_inv: AtomicU32,
    /// 無敵が入ってから、まだ 1 回も書けていない側のビット（最初に書けた tick を記録する）。
    inv_first: AtomicU32,
    /// 側ごと: 無敵を書いた tick / Player が無い・読み込み中で書かなかった tick / 書き込みに失敗した tick /
    /// 1 回だけの値を持ち越した tick。
    inv_written: [AtomicU32; 2],
    inv_skipped: [AtomicU32; 2],
    inv_failed: [AtomicU32; 2],
    carried: [AtomicU32; 2],
    sig_bad: [AtomicU32; 2],
    /// 署名が合わなかったことを記録した側のビット（側ごとに最初の 1 回だけ記録する）。
    sig_logged: AtomicU32,
    /// 断った理由（`Refuse` の番号。0 = 断っていない）。一度置いたら `reset` まで戻さない。
    refused: AtomicU32,
    refused_logged: AtomicU32,
    /// 状態の操作を使う段階が来たのに、断って書かなかった tick。
    refused_ticks: AtomicU32,
}

impl StateLog {
    pub const fn new() -> Self {
        StateLog {
            events: [const { [const { AtomicU32::new(0) }; 3] }; LOG_EVENTS],
            n: AtomicU32::new(0),
            last_inv: AtomicU32::new(0),
            inv_first: AtomicU32::new(0),
            inv_written: [const { AtomicU32::new(0) }; 2],
            inv_skipped: [const { AtomicU32::new(0) }; 2],
            inv_failed: [const { AtomicU32::new(0) }; 2],
            carried: [const { AtomicU32::new(0) }; 2],
            sig_bad: [const { AtomicU32::new(0) }; 2],
            sig_logged: AtomicU32::new(0),
            refused: AtomicU32::new(0),
            refused_logged: AtomicU32::new(0),
            refused_ticks: AtomicU32::new(0),
        }
    }

    pub fn reset(&self) {
        for e in &self.events {
            for w in e {
                w.store(0, Ordering::Relaxed);
            }
        }
        for a in [
            &self.n,
            &self.last_inv,
            &self.inv_first,
            &self.sig_logged,
            &self.refused,
            &self.refused_logged,
            &self.refused_ticks,
        ] {
            a.store(0, Ordering::Relaxed);
        }
        for arr in [
            &self.inv_written,
            &self.inv_skipped,
            &self.inv_failed,
            &self.carried,
            &self.sig_bad,
        ] {
            for a in arr {
                a.store(0, Ordering::Relaxed);
            }
        }
        self.n.store(0, Ordering::Release);
    }

    fn push(&self, k: u32, kind: u32, side: usize, ok: bool, value: u32) {
        let i = self.n.load(Ordering::Relaxed);
        if let Some(e) = self.events.get(i as usize) {
            e[0].store(k, Ordering::Relaxed);
            e[1].store(
                kind | (side as u32) << 8 | u32::from(ok) << 16,
                Ordering::Relaxed,
            );
            e[2].store(value, Ordering::Relaxed);
        }
        self.n.store(i.saturating_add(1), Ordering::Release);
    }

    fn bump(a: &AtomicU32) {
        a.store(
            a.load(Ordering::Relaxed).saturating_add(1),
            Ordering::Relaxed,
        );
    }

    pub fn count(&self) -> u32 {
        self.n.load(Ordering::Acquire)
    }

    /// 記録 `i` 件目（tick, 種類, 側, 書けた, 値）。検査と `text` が使う。
    fn event(&self, i: usize) -> Option<(u32, u32, usize, bool, u32)> {
        let e = self.events.get(i)?;
        let w1 = e[1].load(Ordering::Relaxed);
        Some((
            e[0].load(Ordering::Relaxed),
            w1 & 0xFF,
            ((w1 >> 8) & 0xFF) as usize,
            (w1 >> 16) & 1 != 0,
            e[2].load(Ordering::Relaxed),
        ))
    }

    /// ログの文（読み取りのスレッド）。何も起きていなければ 1 行。
    pub fn text(&self) -> String {
        let n = self.count();
        let side = |s: usize| if s == 0 { "1P" } else { "2P" };
        let inv_name = |v: u32| match v & 3 {
            0 => "なし",
            1 => "1P",
            2 => "2P",
            _ => "1P+2P",
        };
        let counts = |arr: &[AtomicU32; 2]| {
            format!(
                "1P {} / 2P {}",
                arr[0].load(Ordering::Relaxed),
                arr[1].load(Ordering::Relaxed)
            )
        };
        let refused_ticks = self.refused_ticks.load(Ordering::Relaxed);
        let quiet = n == 0
            && refused_ticks == 0
            && [
                &self.inv_written,
                &self.inv_skipped,
                &self.inv_failed,
                &self.carried,
                &self.sig_bad,
            ]
            .iter()
            .all(|arr| arr.iter().all(|a| a.load(Ordering::Relaxed) == 0));
        if quiet {
            return "台本の状態の操作: なし".to_string();
        }
        let mut out = format!(
            "台本の状態の操作: 記録 {n} 件（{LOG_EVENTS} 件より先は数えただけ）。無敵を書いた tick {}、\
             Player が無い・読み込み中で書かなかった tick {}、署名が合わず書かなかった tick {}、\
             書き込みに失敗した tick {}、1 回だけの値を持ち越した tick {}、断って書かなかった tick {refused_ticks}",
            counts(&self.inv_written),
            counts(&self.inv_skipped),
            counts(&self.sig_bad),
            counts(&self.inv_failed),
            counts(&self.carried),
        );
        for i in 0..(n as usize).min(LOG_EVENTS) {
            let Some((k, kind, s, ok, v)) = self.event(i) else {
                break;
            };
            let what = match kind {
                EV_INV => format!("無敵の側が {} になった", inv_name(v)),
                EV_INV_WRITTEN => format!(
                    "{} の無敵を初めて書いた（+0x{PLAYER_INV_TIMER_F:X} = {v}.0 / +0x{PLAYER_INV_TIMER_I:X} = {v}）",
                    side(s)
                ),
                EV_LIFE | EV_GAUGE => format!(
                    "{} の{}（+0x{:X}）に {v} を{}",
                    side(s),
                    if kind == EV_LIFE {
                        "ライフ"
                    } else {
                        "ゲージ"
                    },
                    if kind == EV_LIFE {
                        PLAYER_LIFE
                    } else {
                        PLAYER_GAUGE
                    },
                    if ok {
                        "書いた"
                    } else {
                        "書こうとして失敗した"
                    }
                ),
                EV_DROP_LIFE | EV_DROP_GAUGE => format!(
                    "{} の{} {v} を書けないまま台本が終わったので捨てた",
                    side(s),
                    if kind == EV_DROP_LIFE {
                        "ライフ"
                    } else {
                        "ゲージ"
                    }
                ),
                EV_REFUSED => format!(
                    "状態の操作を書かずに断った（{}）。この実行の間は書かない",
                    Refuse::text(v)
                ),
                EV_REFUSED_LIFE | EV_REFUSED_GAUGE => format!(
                    "{} の{} {v} を、断ったので書かずに捨てた",
                    side(s),
                    if kind == EV_REFUSED_LIFE {
                        "ライフ"
                    } else {
                        "ゲージ"
                    }
                ),
                EV_SIG_LIFE => format!(
                    "{} の Player のライフ（+0x{PLAYER_LIFE:X}）が {v} で 0〜{LIFE_MAX} の外。署名が合わないので、\
                     その側には何も書かない（以後も合わなければ数えるだけ）",
                    side(s)
                ),
                EV_SIG_GAUGE => format!(
                    "{} の Player のゲージ（+0x{PLAYER_GAUGE:X}）が {}（0x{v:08X}）で 0〜{GAUGE_MAX} の外。署名が合わない\
                     ので、その側には何も書かない（以後も合わなければ数えるだけ）",
                    side(s),
                    f32::from_bits(v)
                ),
                EV_SIG_READ => format!(
                    "{} の Player の署名の欄 0x{v:08X} が読めない。その側には何も書かない（以後も読めなければ数えるだけ）",
                    side(s)
                ),
                _ => format!("（知らない記録 {kind}）"),
            };
            out.push_str(&format!("\n  tick {k}: {what}"));
        }
        out
    }
}

impl Default for StateLog {
    fn default() -> Self {
        Self::new()
    }
}

pub static STATE_LOG: StateLog = StateLog::new();

fn drop_pending(engine: &mut Engine, log: &StateLog, k: u32, life_kind: u32, gauge_kind: u32) {
    let d = engine.discard_pending();
    for side in 0..2 {
        if let Some(v) = d.life[side] {
            log.push(k, life_kind, side, false, u32::from(v));
        }
        if let Some(v) = d.gauge[side] {
            log.push(k, gauge_kind, side, false, u32::from(v));
        }
    }
}

/// Player `p` の署名（ライフ・ゲージがゲームの取りうる範囲の中か）。合わなければ、側ごとに最初の 1 回だけ記録して false。
fn player_signature_ok(
    mem: &mut dyn StateMem,
    p: u32,
    side: usize,
    k: u32,
    log: &StateLog,
) -> bool {
    let life_at = p.wrapping_add(PLAYER_LIFE);
    let gauge_at = p.wrapping_add(PLAYER_GAUGE);
    let bad = match (mem.read_u32(life_at), mem.read_u32(gauge_at)) {
        (None, _) => Some((EV_SIG_READ, life_at)),
        (_, None) => Some((EV_SIG_READ, gauge_at)),
        // ライフは 0〜10（0 は負けた後。書く値の範囲 `LIFE_MIN`〜 とは別に、読む側は 0 を許す）。
        (Some(life), _) if life > u32::from(LIFE_MAX) => Some((EV_SIG_LIFE, life)),
        // ゲージは有限の 0〜400（NaN は範囲の比べで外れる）。
        (_, Some(g)) if !(0.0..=GAUGE_MAX as f32).contains(&f32::from_bits(g)) => {
            Some((EV_SIG_GAUGE, g))
        }
        _ => None,
    };
    let Some((kind, value)) = bad else {
        return true;
    };
    StateLog::bump(&log.sig_bad[side]);
    let bit = 1u32 << side;
    let logged = log.sig_logged.load(Ordering::Relaxed);
    if logged & bit == 0 {
        log.push(k, kind, side, false, value);
        log.sig_logged.store(logged | bit, Ordering::Relaxed);
    }
    false
}

/// 1 tick ぶん（入力の地点で、台本を進めた後に毎 tick 呼ぶ）。`slots` は Player へのポインタの欄（1P, 2P）、
/// `k` は入口の tick の番号（記録に残す）、`refuse` はこの tick に断る理由（ネットワーク・巻き戻しの実験が動いていれば `Some`）。
pub fn apply(
    engine: &mut Engine,
    mem: &mut dyn StateMem,
    slots: [u32; 2],
    k: u32,
    log: &StateLog,
    refuse: Option<Refuse>,
) {
    // 断る理由は、状態の操作を使う段階が来る前でも覚える（ネットワークが途中で止まっても、その実行では書き始めない）。
    if let Some(r) = refuse {
        let _ = log
            .refused
            .compare_exchange(0, r as u32, Ordering::Relaxed, Ordering::Relaxed);
    }
    let inv = engine.inv();
    let last = log.last_inv.load(Ordering::Relaxed);
    if u32::from(inv) != last {
        log.push(k, EV_INV, 0, true, u32::from(inv));
        log.last_inv.store(u32::from(inv), Ordering::Relaxed);
        log.inv_first
            .store(u32::from(inv) & !last, Ordering::Relaxed);
    }
    if !matches!(engine.status(), Status::Running { .. }) {
        drop_pending(engine, log, k, EV_DROP_LIFE, EV_DROP_GAUGE);
        return;
    }
    let pending = engine.pending();
    let wants = |side: usize| {
        inv & (1 << side) != 0 || pending.life[side].is_some() || pending.gauge[side].is_some()
    };
    if !wants(0) && !wants(1) {
        return;
    }
    let refused = log.refused.load(Ordering::Relaxed);
    if refused != 0 {
        // 断る実行: ゲームのメモリは読みも書きもしない。1 回だけの値は、この実行では書けないので捨てる。
        if log.refused_logged.swap(1, Ordering::Relaxed) == 0 {
            log.push(k, EV_REFUSED, 0, false, refused);
        }
        StateLog::bump(&log.refused_ticks);
        drop_pending(engine, log, k, EV_REFUSED_LIFE, EV_REFUSED_GAUGE);
        return;
    }
    // 書ける側の Player（読み込み中なら、どちらも書かない。要らない側は読まない）。署名が合わない側も書かない。
    let loading = mem.loading();
    let mut players = [None; 2];
    let mut sig_bad = [false; 2];
    if !loading {
        for (side, slot) in slots.into_iter().enumerate() {
            if !wants(side) {
                continue;
            }
            let Some(p) = mem.read_u32(slot).filter(|&p| p != 0) else {
                continue;
            };
            if player_signature_ok(mem, p, side, k, log) {
                players[side] = Some(p);
            } else {
                sig_bad[side] = true;
            }
        }
    }
    for (side, p) in players.into_iter().enumerate() {
        if inv & (1 << side) == 0 {
            continue;
        }
        let Some(p) = p else {
            if !sig_bad[side] {
                StateLog::bump(&log.inv_skipped[side]);
            }
            continue;
        };
        // float と int の両方（int だけでは毎 tick の減算 `0x0042F490` が float から作り直す）。
        let f = mem.write_u32(
            p.wrapping_add(PLAYER_INV_TIMER_F),
            (INV_TIMER_VALUE as f32).to_bits(),
        );
        let i = mem.write_u32(p.wrapping_add(PLAYER_INV_TIMER_I), INV_TIMER_VALUE);
        if f && i {
            StateLog::bump(&log.inv_written[side]);
            let bit = 1u32 << side;
            let first = log.inv_first.load(Ordering::Relaxed);
            if first & bit != 0 {
                log.push(k, EV_INV_WRITTEN, side, true, INV_TIMER_VALUE);
                log.inv_first.store(first & !bit, Ordering::Relaxed);
            }
        } else {
            StateLog::bump(&log.inv_failed[side]);
        }
    }
    let writable = players.map(|p| p.is_some());
    for (side, ok) in writable.into_iter().enumerate() {
        if !ok && (pending.life[side].is_some() || pending.gauge[side].is_some()) {
            StateLog::bump(&log.carried[side]);
        }
    }
    let s: SetOnce = engine.take_pending(writable);
    for (side, p) in players.into_iter().enumerate() {
        let Some(p) = p else {
            continue;
        };
        if let Some(v) = s.life[side] {
            // ライフは 4 バイトの int。
            let ok = mem.write_u32(p.wrapping_add(PLAYER_LIFE), u32::from(v));
            log.push(k, EV_LIFE, side, ok, u32::from(v));
        }
        if let Some(v) = s.gauge[side] {
            // ゲージは 4 バイトの float。
            let ok = mem.write_u32(p.wrapping_add(PLAYER_GAUGE), f32::from(v).to_bits());
            log.push(k, EV_GAUGE, side, ok, u32::from(v));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use higanr_core::script::{Obs, parse};
    use std::collections::HashMap;

    const SLOTS: [u32; 2] = [0x0010_0000, 0x0010_0004];
    const P1: u32 = 0x0200_0000;
    const P2: u32 = 0x0300_0000;

    /// 偽のゲームのメモリ: 読めるアドレスの値と、書いた順（アドレス, 値）。`fail_writes` なら書き込みを失敗させる。
    #[derive(Default)]
    struct FakeMem {
        loading: bool,
        mem: HashMap<u32, u32>,
        writes: Vec<(u32, u32)>,
        reads: u32,
        fail_writes: bool,
    }

    impl FakeMem {
        /// ポインタの欄に `p1` / `p2` を置き、偽の Player `P1` / `P2` の署名の欄には範囲の中の値（席ごとに違う値。
        /// 境目の 0 と 400.0 も入れる）を置く。
        fn with_players(p1: u32, p2: u32) -> Self {
            let mut m = FakeMem::default();
            m.mem.insert(SLOTS[0], p1);
            m.mem.insert(SLOTS[1], p2);
            m.mem.insert(P1 + PLAYER_LIFE, 10);
            m.mem.insert(P1 + PLAYER_GAUGE, 0.0f32.to_bits());
            m.mem.insert(P2 + PLAYER_LIFE, 0);
            m.mem.insert(P2 + PLAYER_GAUGE, 400.0f32.to_bits());
            m
        }
    }

    impl StateMem for FakeMem {
        fn loading(&mut self) -> bool {
            self.loading
        }
        fn read_u32(&mut self, addr: u32) -> Option<u32> {
            self.reads += 1;
            self.mem.get(&addr).copied()
        }
        fn write_u32(&mut self, addr: u32, v: u32) -> bool {
            if self.fail_writes {
                return false;
            }
            self.writes.push((addr, v));
            true
        }
    }

    fn obs() -> Obs {
        Obs {
            scene: 2,
            screen: None,
            cursor: None,
            sub: None,
            trans: None,
            pause: false,
            loading: false,
            demo: false,
            end_menu: false,
            menu_flag: false,
            talk: None,
        }
    }

    fn tick(e: &mut Engine, m: &mut FakeMem, k: u32, log: &StateLog) {
        tick_refusing(e, m, k, log, None);
    }

    fn tick_refusing(
        e: &mut Engine,
        m: &mut FakeMem,
        k: u32,
        log: &StateLog,
        refuse: Option<Refuse>,
    ) {
        e.step(&obs());
        apply(e, m, SLOTS, k, log, refuse);
    }

    fn inv_writes(p: u32) -> Vec<(u32, u32)> {
        vec![
            (p + PLAYER_INV_TIMER_F, 2.0f32.to_bits()),
            (p + PLAYER_INV_TIMER_I, 2),
        ]
    }

    #[test]
    fn inv_writes_both_fields_every_tick_until_the_script_ends() {
        let log = StateLog::new();
        let mut e = Engine::new(parse("wait=1 inv=p2\nwait=3\n").unwrap());
        let mut m = FakeMem::with_players(P1, P2);
        for k in 0..4 {
            m.writes.clear();
            tick(&mut e, &mut m, k, &log);
            assert_eq!(m.writes, inv_writes(P2), "tick {k}");
        }
        m.writes.clear();
        tick(&mut e, &mut m, 4, &log);
        assert_eq!(e.status(), Status::Done);
        assert!(m.writes.is_empty(), "{:?}", m.writes);
        assert_eq!(log.inv_written[1].load(Ordering::Relaxed), 4);
        assert_eq!(log.inv_written[0].load(Ordering::Relaxed), 0);
        let ev: Vec<_> = (0..log.count() as usize)
            .map(|i| log.event(i).unwrap())
            .collect();
        assert_eq!(
            ev,
            vec![
                (0, EV_INV, 0, true, 2),
                (0, EV_INV_WRITTEN, 1, true, 2),
                (4, EV_INV, 0, true, 0),
            ]
        );
    }

    #[test]
    fn nothing_is_written_while_the_player_is_missing_or_loading() {
        let log = StateLog::new();
        let mut e = Engine::new(parse("wait=1 inv=p1+p2 life1=1 gauge2=400\nwait=100\n").unwrap());
        let mut m = FakeMem::with_players(0, P2);
        m.loading = true;
        tick(&mut e, &mut m, 0, &log);
        assert!(m.writes.is_empty(), "読み込み中に書いた: {:?}", m.writes);
        assert_eq!(m.reads, 0, "読み込み中に Player をたどった");
        m.loading = false;
        tick(&mut e, &mut m, 1, &log);
        let mut want = inv_writes(P2);
        want.push((P2 + PLAYER_GAUGE, 400.0f32.to_bits()));
        assert_eq!(m.writes, want);
        assert_eq!(
            e.pending().life,
            [Some(1), None],
            "1P の分を持ち越していない"
        );
        m.mem.insert(SLOTS[0], P1);
        m.writes.clear();
        tick(&mut e, &mut m, 2, &log);
        let mut want = inv_writes(P1);
        want.extend(inv_writes(P2));
        want.push((P1 + PLAYER_LIFE, 1));
        assert_eq!(m.writes, want);
        m.writes.clear();
        tick(&mut e, &mut m, 3, &log);
        let mut want = inv_writes(P1);
        want.extend(inv_writes(P2));
        assert_eq!(m.writes, want);
        assert_eq!(log.inv_skipped[0].load(Ordering::Relaxed), 2);
        assert_eq!(log.inv_skipped[1].load(Ordering::Relaxed), 1);
        assert_eq!(log.carried[0].load(Ordering::Relaxed), 2);
        assert_eq!(log.carried[1].load(Ordering::Relaxed), 1);
        let text = log.text();
        assert!(
            text.contains("tick 2: 1P のライフ（+0xA8）に 1 を書いた"),
            "{text}"
        );
        assert!(
            text.contains("tick 1: 2P のゲージ（+0x30388）に 400 を書いた"),
            "{text}"
        );
    }

    #[test]
    fn one_shot_values_are_written_once_and_dropped_when_the_script_ends() {
        let log = StateLog::new();
        let mut e = Engine::new(parse("wait=1 gauge1=250 life2=4\n").unwrap());
        let mut m = FakeMem::with_players(P1, 0);
        tick(&mut e, &mut m, 7, &log);
        assert_eq!(m.writes, vec![(P1 + PLAYER_GAUGE, 250.0f32.to_bits())]);
        m.writes.clear();
        tick(&mut e, &mut m, 8, &log);
        assert!(m.writes.is_empty());
        m.mem.insert(SLOTS[1], P2);
        tick(&mut e, &mut m, 9, &log);
        assert!(m.writes.is_empty(), "終わった後に書いた: {:?}", m.writes);
        let text = log.text();
        assert!(
            text.contains("tick 8: 2P のライフ 4 を書けないまま台本が終わったので捨てた"),
            "{text}"
        );
        assert!(
            text.contains("tick 7: 1P のゲージ（+0x30388）に 250 を書いた"),
            "{text}"
        );
    }

    #[test]
    fn a_script_without_state_ops_touches_nothing() {
        let log = StateLog::new();
        let mut e = Engine::new(parse("wait=2 p1=Z\nwait=1\n").unwrap());
        let mut m = FakeMem::with_players(P1, P2);
        for k in 0..6 {
            tick(&mut e, &mut m, k, &log);
        }
        assert_eq!((m.reads, m.writes.len()), (0, 0));
        assert_eq!(log.text(), "台本の状態の操作: なし");
    }

    #[test]
    fn write_failures_are_counted_and_the_log_is_bounded() {
        let log = StateLog::new();
        let mut e = Engine::new(parse("wait=1 inv=p1 life1=2\nwait=10\n").unwrap());
        let mut m = FakeMem::with_players(P1, P2);
        m.fail_writes = true;
        tick(&mut e, &mut m, 0, &log);
        assert_eq!(log.inv_failed[0].load(Ordering::Relaxed), 1);
        assert_eq!(log.inv_written[0].load(Ordering::Relaxed), 0);
        assert!(
            log.text()
                .contains("1P のライフ（+0xA8）に 2 を書こうとして失敗した")
        );
        for i in 0..(LOG_EVENTS as u32 + 10) {
            log.push(100 + i, EV_INV, 0, true, 1);
        }
        assert_eq!(log.count(), LOG_EVENTS as u32 + 10 + 2);
        let lines = log.text().lines().count();
        assert_eq!(lines, 1 + LOG_EVENTS, "記録の行は上限まで");
        log.reset();
        assert_eq!(log.text(), "台本の状態の操作: なし");
    }

    #[test]
    fn net_and_rollback_runs_refuse_and_stay_refused() {
        const SCRIPT: &str = "wait=1 inv=p1 life2=3\nwait=3\n";
        let log = StateLog::new();
        let mut e = Engine::new(parse(SCRIPT).unwrap());
        let mut m = FakeMem::with_players(P1, P2);
        tick(&mut e, &mut m, 0, &log);
        let mut want = inv_writes(P1);
        want.push((P2 + PLAYER_LIFE, 3));
        assert_eq!(m.writes, want);

        for (r, why) in [
            (Refuse::Net, "ネットワーク（lockstep）の実行"),
            (Refuse::Rollback, "巻き戻しの実験（E1-2）の実行"),
        ] {
            let log = StateLog::new();
            let mut e = Engine::new(parse(SCRIPT).unwrap());
            let mut m = FakeMem::with_players(P1, P2);
            tick_refusing(&mut e, &mut m, 0, &log, Some(r));
            for k in 1..3 {
                tick_refusing(&mut e, &mut m, k, &log, None);
            }
            assert_eq!((m.reads, m.writes.len()), (0, 0), "{r:?}");
            assert!(e.pending().is_empty(), "{r:?}: 1 回だけの値が残った");
            assert_eq!(log.refused_ticks.load(Ordering::Relaxed), 3, "{r:?}");
            let text = log.text();
            assert!(
                text.contains(&format!("tick 0: 状態の操作を書かずに断った（{why}")),
                "{text}"
            );
            assert!(
                text.contains("tick 0: 2P のライフ 3 を、断ったので書かずに捨てた"),
                "{text}"
            );
            assert_eq!(text.matches("書かずに断った").count(), 1, "{text}");
        }
    }

    #[test]
    fn refusal_is_remembered_before_the_state_op_step_arrives() {
        let log = StateLog::new();
        let mut e = Engine::new(parse("wait=2\nwait=1 inv=p1+p2\nwait=3\n").unwrap());
        let mut m = FakeMem::with_players(P1, P2);
        tick_refusing(&mut e, &mut m, 0, &log, Some(Refuse::Net));
        assert_eq!(
            log.text(),
            "台本の状態の操作: なし",
            "使う段階の前に記録した"
        );
        for k in 1..5 {
            tick_refusing(&mut e, &mut m, k, &log, None);
        }
        assert_eq!((m.reads, m.writes.len()), (0, 0));
        assert!(log.text().contains("tick 2: 状態の操作を書かずに断った"));

        let quiet = StateLog::new();
        let mut e = Engine::new(parse("wait=2 p1=Z\nwait=1\n").unwrap());
        for k in 0..5 {
            tick_refusing(&mut e, &mut m, k, &quiet, Some(Refuse::Net));
        }
        assert_eq!(quiet.text(), "台本の状態の操作: なし");
    }

    #[test]
    fn a_player_with_a_bad_signature_is_not_written() {
        let cases: [(&str, u32, Option<u32>, &str); 7] = [
            ("life 11", PLAYER_LIFE, Some(11), "ライフ（+0xA8）が 11"),
            (
                "life -1",
                PLAYER_LIFE,
                Some(u32::MAX),
                "ライフ（+0xA8）が 4294967295",
            ),
            (
                "gauge 400.5",
                PLAYER_GAUGE,
                Some(400.5f32.to_bits()),
                "ゲージ（+0x30388）が 400.5",
            ),
            (
                "gauge -1",
                PLAYER_GAUGE,
                Some((-1.0f32).to_bits()),
                "ゲージ（+0x30388）が -1",
            ),
            (
                "gauge NaN",
                PLAYER_GAUGE,
                Some(f32::NAN.to_bits()),
                "ゲージ（+0x30388）が NaN",
            ),
            (
                "gauge inf",
                PLAYER_GAUGE,
                Some(f32::INFINITY.to_bits()),
                "ゲージ（+0x30388）が inf",
            ),
            (
                "gauge unreadable",
                PLAYER_GAUGE,
                None,
                "署名の欄 0x02030388 が読めない",
            ),
        ];
        for (name, off, bad, msg) in cases {
            let log = StateLog::new();
            let mut e = Engine::new(parse("wait=1 inv=p1+p2 life1=2 gauge1=5\nwait=10\n").unwrap());
            let mut m = FakeMem::with_players(P1, P2);
            let good = m.mem[&(P1 + off)];
            match bad {
                Some(v) => m.mem.insert(P1 + off, v),
                None => m.mem.remove(&(P1 + off)),
            };
            tick(&mut e, &mut m, 0, &log);
            tick(&mut e, &mut m, 1, &log);
            let mut want = inv_writes(P2);
            want.extend(inv_writes(P2));
            assert_eq!(m.writes, want, "{name}");
            assert_eq!(
                e.pending().life,
                [Some(2), None],
                "{name}: 持ち越していない"
            );
            assert_eq!(
                e.pending().gauge,
                [Some(5), None],
                "{name}: 持ち越していない"
            );
            assert_eq!(log.sig_bad[0].load(Ordering::Relaxed), 2, "{name}");
            assert_eq!(log.inv_skipped[0].load(Ordering::Relaxed), 0, "{name}");
            assert_eq!(log.carried[0].load(Ordering::Relaxed), 2, "{name}");
            let text = log.text();
            assert!(
                text.contains(&format!("tick 0: 1P の Player の{msg}")),
                "{name}: {text}"
            );
            assert_eq!(
                text.matches("署名").count(),
                2,
                "{name}: 毎 tick 記録した: {text}"
            );
            m.mem.insert(P1 + off, good);
            m.writes.clear();
            tick(&mut e, &mut m, 2, &log);
            let mut want = inv_writes(P1);
            want.extend(inv_writes(P2));
            want.push((P1 + PLAYER_LIFE, 2));
            want.push((P1 + PLAYER_GAUGE, 5.0f32.to_bits()));
            assert_eq!(m.writes, want, "{name}");
        }
    }
}
