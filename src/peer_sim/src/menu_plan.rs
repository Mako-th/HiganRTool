//! 選択の台本（`--menu <ファイル>`）: ネットの選択で、peer_sim が「決めた MENU の語」と「画面を移る押下」を台本どおりに出す形。
//!
//! ゲームが無いので、画面は core の模型 `higanr_core::menu_model::Model` で追う（dll と共用）。この台本が決めるのは「どの画面で・何フレーム
//! 待って・何を出すか」だけで、押下がいつ入るか（ホストが順番を決める）は core の `selection.rs`。
//!
//! ## 台本の形
//!
//! 1 行 1 項目。`#` から行末までは注。空の行は読み飛ばす。項目は上から順に 1 つずつ行う。
//!
//! ```text
//! screen=S [wait=N] 動作
//! ```
//!
//! - `screen=S`（必須）: 模型の画面がこれになってから行う。7 = 難易度・8 = キャラ・16 = マップ・6 = 選択の外
//!   （難易度の画面の取消で出た後。動作は `enter` だけ）。
//! - `wait=N`（既定 0。0〜36000）: 画面が `S` と同じになってから N フレーム待つ。待つ間に画面が変わったら数え直す。
//! - 動作（どれか 1 つ）:
//!   - `word=キー[+キー…] [hold=N]`: MENU の自分の語を N フレーム（既定 1。1〜36000）の間これにする。キー名は
//!     `higanr_core::script::key_by_name`（`Z` `X` `SHIFT` `ESC` `UP` `DOWN` `LEFT` `RIGHT`）か `0x` の 16 進（`SEND_MASK` の中）。
//!     送るときに `--mask` を掛け、画面を移るビット（`menu_model::TRANSITION_BITS`。Z・X・Esc）を落とす（画面を移るのは STEP でだけ）。
//!     画面を移るビットだけの語は、落とすと 0 になるので断る。
//!   - `press=decide|cancel`: 画面を移る押下。難易度・マップの画面は `value=V`（既定 0。難易度 0〜3・マップの一覧の値 0〜15。
//!     14 = −1、15 = −2）。キャラの画面は、決定なら `cursor=C`（既定 0。0〜15）と `shift=0|1`（既定 0）から自分の席で
//!     `menu_model::chara_value` を作り、取消はカーソル 0（`cursor` と `shift` は書けない）。押下が入ったら（`FrameOut::apply` に
//!     同じ kind・value・画面が来たら）項目を終える。入るまで、ホストは相手が来ていれば毎フレーム押し直し、ゲストは要求が
//!     捨てられたら `RETRY_FRAMES` フレーム後に押し直す。
//!   - `enter`: 選択の外（画面 6）から、自分で次の選択に入る（模型は難易度 7 から）。相手が先に入った（`Recv::PeerSelecting`）ら、
//!     この項目は行わずに終える（peer_sim 同士で両者が待ち合って止まらないように）。
//! - 知らない名前・範囲の外・同じ名前の 2 回目・動作の無い行・動作が 2 つある行は、行の番号を添えて断り、台本全体を使わない
//!   （`higanr_core::script::parse` と同じ `ParseError`）。

use std::collections::HashMap;

use higanr_core::input_model::SEND_MASK;
use higanr_core::lockstep::Role;
use higanr_core::menu_model::{
    SCREEN_CHARA, SCREEN_DIFFICULTY, SCREEN_MAP, SCREEN_MODE, TRANSITION_BITS, align_chara_value,
    chara_value,
};
use higanr_core::script::{ParseError, key_by_name};
use higanr_core::selection::Press;
use higanr_core::wire::{Step, StepKind};

/// ゲストの要求が捨てられてから押し直すまで（フレーム）。すぐ押し直すと同じ理由でまた捨てられやすい。
pub const RETRY_FRAMES: u32 = 30;

/// peer_sim がホストの役のときに出す合わせの value。peer_sim は画面のカーソルを持たないので固定にした。実機が相手のとき、受けた
/// ゲスト（dll）はカーソルを難易度 2、キャラは 1P のカーソル 4・2P のカーソル 6、マップは値 14（一覧の最後から 2 番目）へ書く。
pub const SIM_ALIGN_DIFFICULTY: u8 = 2;
pub const SIM_ALIGN_CHARA_CURSORS: [u32; 2] = [4, 6];
pub const SIM_ALIGN_MAP_ITEM: u8 = 14;

/// 模型の画面 `screen` の、peer_sim がホストの役のときの合わせの押下。選択の画面でなければ None。
pub fn sim_align_press(screen: u8) -> Option<Press> {
    let value = match screen {
        SCREEN_DIFFICULTY => SIM_ALIGN_DIFFICULTY,
        SCREEN_CHARA => align_chara_value(SIM_ALIGN_CHARA_CURSORS),
        SCREEN_MAP => SIM_ALIGN_MAP_ITEM,
        _ => return None,
    };
    Some(Press {
        kind: StepKind::Align,
        value,
        screen,
    })
}

/// `wait` と `hold` の上限（10 分。`higanr_core::script` の tick の上限と同じ）。
const MAX_FRAMES: u32 = 36_000;
/// 項目の数の上限（台本の誤りで巨大なファイルを読まないように）。
const MAX_ITEMS: usize = 1_000;

/// 押下の中身（value は画面によって作り方が違う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressSpec {
    /// 難易度・マップの画面: value をそのまま。
    Value { kind: StepKind, value: u8 },
    /// キャラの画面の決定: 自分の席・Shift・カーソルから `chara_value`。
    CharaDecide { cursor: u8, shift: bool },
    /// キャラの画面の取消: 自分の席とカーソル 0。
    CharaCancel,
}

impl PressSpec {
    pub fn kind(self) -> StepKind {
        match self {
            PressSpec::Value { kind, .. } => kind,
            PressSpec::CharaDecide { .. } => StepKind::Decide,
            PressSpec::CharaCancel => StepKind::Cancel,
        }
    }

    pub fn value(self, seat: usize) -> u8 {
        match self {
            PressSpec::Value { value, .. } => value,
            PressSpec::CharaDecide { cursor, shift } => chara_value(seat, shift, cursor),
            PressSpec::CharaCancel => chara_value(seat, false, 0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// MENU の自分の語（`--mask` と画面を移るビットはまだ落としていない）を `hold` フレーム。
    Word {
        keys: u16,
        hold: u32,
    },
    Press(PressSpec),
    /// 選択の外から自分で次の選択に入る。
    Enter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub line: u32,
    pub screen: u8,
    pub wait: u32,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuPlan {
    pub items: Vec<Item>,
}

fn err(line: u32, msg: impl Into<String>) -> ParseError {
    ParseError {
        line,
        msg: msg.into(),
    }
}

fn parse_u32(s: &str) -> Option<u32> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => s.parse::<u32>().ok(),
    }
}

fn ranged(
    fields: &HashMap<&str, &str>,
    line: u32,
    name: &str,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32, ParseError> {
    let v = match fields.get(name) {
        None => default,
        Some(s) => {
            parse_u32(s).ok_or_else(|| err(line, format!("{name} の値が数として読めない: {s}")))?
        }
    };
    if v < min || v > max {
        return Err(err(
            line,
            format!("{name} の値が範囲の外（{min}〜{max}）: {v}"),
        ));
    }
    Ok(v)
}

/// `word=` の値: キー名を `+` でつないだものか `0x` の 16 進。`SEND_MASK` の外は誤り。
fn parse_keys(line: u32, v: &str) -> Result<u16, ParseError> {
    let keys = if let Some(h) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u16::from_str_radix(h, 16)
            .map_err(|_| err(line, format!("word の値が 16 進として読めない: {v}")))?
    } else {
        let mut acc = 0u16;
        for part in v.split('+') {
            acc |= key_by_name(part)
                .ok_or_else(|| err(line, format!("word に知らないキー名があります: {part}")))?;
        }
        acc
    };
    if keys & !SEND_MASK != 0 {
        return Err(err(
            line,
            format!("word が SEND_MASK（{SEND_MASK:#06x}）の外です: {v}"),
        ));
    }
    if keys & !TRANSITION_BITS == 0 {
        return Err(err(
            line,
            format!(
                "word が画面を移るキー（Z・X・Esc）だけで、落とすと 0 になる（画面を移るなら press= を使う）: {v}"
            ),
        ));
    }
    Ok(keys)
}

const KNOWN: &[&str] = &[
    "screen", "wait", "word", "hold", "press", "value", "cursor", "shift",
];

fn parse_line(line: u32, tokens: &[&str]) -> Result<Item, ParseError> {
    let mut fields: HashMap<&str, &str> = HashMap::new();
    let mut enter = false;
    for tok in tokens {
        if *tok == "enter" {
            if enter {
                return Err(err(line, "enter を 2 回書いています"));
            }
            enter = true;
            continue;
        }
        let (name, value) = tok
            .split_once('=')
            .ok_or_else(|| err(line, format!("`=` が無い欄です: {tok}")))?;
        if !KNOWN.contains(&name) {
            return Err(err(line, format!("知らない名前です: {name}")));
        }
        if fields.insert(name, value).is_some() {
            return Err(err(line, format!("{name} を 2 回書いています")));
        }
    }
    let screen_raw = fields
        .get("screen")
        .ok_or_else(|| err(line, "screen= が要る"))?;
    let screen = match parse_u32(screen_raw) {
        Some(v)
            if [SCREEN_MODE, SCREEN_DIFFICULTY, SCREEN_CHARA, SCREEN_MAP]
                .iter()
                .any(|&s| u32::from(s) == v) =>
        {
            v as u8
        }
        _ => {
            return Err(err(
                line,
                format!("screen は 6・7・8・16 のどれか: {screen_raw}"),
            ));
        }
    };
    let wait = ranged(&fields, line, "wait", 0, 0, MAX_FRAMES)?;

    let has = |n: &str| fields.contains_key(n);
    let actions = usize::from(enter) + usize::from(has("word")) + usize::from(has("press"));
    if actions != 1 {
        return Err(err(
            line,
            "動作（word= / press= / enter）をちょうど 1 つ書く",
        ));
    }
    if has("hold") && !has("word") {
        return Err(err(line, "hold は word= の行にだけ書ける"));
    }
    for n in ["value", "cursor", "shift"] {
        if has(n) && !has("press") {
            return Err(err(line, format!("{n} は press= の行にだけ書ける")));
        }
    }
    if (screen == SCREEN_MODE) != enter {
        return Err(err(
            line,
            "enter は screen=6（選択の外）でだけ、screen=6 では enter だけ",
        ));
    }

    let action = if enter {
        Action::Enter
    } else if let Some(v) = fields.get("word") {
        Action::Word {
            keys: parse_keys(line, v)?,
            hold: ranged(&fields, line, "hold", 1, 1, MAX_FRAMES)?,
        }
    } else {
        let kind = match fields.get("press").copied() {
            Some("decide") => StepKind::Decide,
            Some("cancel") => StepKind::Cancel,
            other => {
                return Err(err(
                    line,
                    format!("press は decide か cancel: {}", other.unwrap_or("")),
                ));
            }
        };
        let spec = if screen == SCREEN_CHARA {
            if has("value") {
                return Err(err(
                    line,
                    "キャラの画面では value= を書かない（cursor= と shift= から作る）",
                ));
            }
            match kind {
                StepKind::Decide => PressSpec::CharaDecide {
                    cursor: ranged(&fields, line, "cursor", 0, 0, 15)? as u8,
                    shift: ranged(&fields, line, "shift", 0, 0, 1)? == 1,
                },
                StepKind::Cancel => {
                    if has("cursor") || has("shift") {
                        return Err(err(
                            line,
                            "キャラの画面の取消はカーソル 0（cursor= と shift= は書けない）",
                        ));
                    }
                    PressSpec::CharaCancel
                }
                // 上の match は決定と取消しか返さない（合わせはホストの役が自分で出す。形式の切替は台本に無い）。
                StepKind::Align | StepKind::Format => {
                    return Err(err(line, "press は decide か cancel"));
                }
            }
        } else {
            if has("cursor") || has("shift") {
                return Err(err(
                    line,
                    "cursor= と shift= はキャラの画面（screen=8）でだけ",
                ));
            }
            let max = if screen == SCREEN_DIFFICULTY { 3 } else { 15 };
            PressSpec::Value {
                kind,
                value: ranged(&fields, line, "value", 0, 0, max)? as u8,
            }
        };
        Action::Press(spec)
    };
    Ok(Item {
        line,
        screen,
        wait,
        action,
    })
}

pub fn parse(text: &str) -> Result<MenuPlan, ParseError> {
    let mut items = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = (idx + 1) as u32;
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = content.split_whitespace().collect();
        let item = parse_line(line, &tokens)?;
        if items.len() >= MAX_ITEMS {
            return Err(err(line, format!("項目は {MAX_ITEMS} までです")));
        }
        items.push(item);
    }
    if items.is_empty() {
        return Err(err(0, "台本に項目が 1 つも無い"));
    }
    Ok(MenuPlan { items })
}

/// 台本のファイルを読む（`--menu`）。読めない・誤りがあれば、パスと行番号を添えた文で返す。
pub fn load(path: &std::path::Path) -> Result<MenuPlan, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("選択の台本 {} が読めない: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("選択の台本 {} の {e}", path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameIn {
    pub screen: u8,
    pub seat: usize,
    pub role: Role,
    pub peer_present: bool,
    /// 今の画面の合わせを入れ終えたか（`Selection::is_aligned`）。ゲストは届くまで要求しない（要求してもホストが先に出す合わせに
    /// 負けて捨てられ、押し直しを待つことになる）。ホストには効かない。
    pub aligned: bool,
    /// ゲストの持っている要求（`Selection::pending_press`）。
    pub pending: Option<Press>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Want {
    /// 自分の語（まだ `--mask` と画面を移るビットを落としていない。項目の外は 0）。
    pub word: u16,
    pub press: Option<Press>,
    /// 自分で次の選択に入る（選択の外にいるときだけ意味を持つ）。
    pub enter: bool,
}

/// 台本を 1 フレームずつ進める。ソケットにも時計にも触らない（呼び手が 1 フレームの枠ごとに `frame` → `selection_frame` →
/// `after_frame` の順に呼ぶ）。`wait` と `RETRY_FRAMES` はフレームで数えるので、実時間はペースに付いてくる。
#[derive(Debug, Clone)]
pub struct Runner {
    items: Vec<Item>,
    idx: usize,
    /// 今の項目で、画面が合ってから数えたフレーム。
    since: u32,
    /// 語の項目で、出したフレーム。
    held: u32,
    /// 押した押下（入るまで持つ。入ったかを `after_frame` で比べる）。
    pressed: Option<Press>,
    /// ゲスト: 押し直すまでの残り。
    retry_left: u32,
    /// ゲストが要求を捨てられて押し直しを待った回数。
    pub retries: u32,
}

impl Runner {
    pub fn new(plan: &MenuPlan) -> Self {
        Runner {
            items: plan.items.clone(),
            idx: 0,
            since: 0,
            held: 0,
            pressed: None,
            retry_left: 0,
            retries: 0,
        }
    }

    pub fn done_items(&self) -> usize {
        self.idx
    }

    pub fn total_items(&self) -> usize {
        self.items.len()
    }

    fn next_item(&mut self) {
        self.idx += 1;
        self.since = 0;
        self.held = 0;
        self.pressed = None;
        self.retry_left = 0;
    }

    pub fn frame(&mut self, fi: FrameIn) -> Want {
        let Some(item) = self.items.get(self.idx).copied() else {
            return Want::default();
        };
        if item.screen != fi.screen {
            // 画面が合っていない（相手の押下で画面が動いた）。戻ってきたら待ち直す。
            self.since = 0;
            self.held = 0;
            self.pressed = None;
            self.retry_left = 0;
            return Want::default();
        }
        if fi.role == Role::Guest && !fi.aligned && item.action != Action::Enter {
            // ゲストは、この画面の合わせが届くまで動かず、要求しない（dll と同じ）。待つフレームは wait にも押し直しの待ちにも数えない。
            return Want::default();
        }
        if self.since < item.wait {
            self.since += 1;
            return Want::default();
        }
        match item.action {
            Action::Word { keys, hold } => {
                self.held += 1;
                if self.held >= hold {
                    self.next_item();
                }
                Want {
                    word: keys,
                    ..Want::default()
                }
            }
            Action::Enter => {
                self.next_item();
                Want {
                    enter: true,
                    ..Want::default()
                }
            }
            Action::Press(spec) => {
                let press = Press {
                    kind: spec.kind(),
                    value: spec.value(fi.seat),
                    screen: fi.screen,
                };
                let go = match fi.role {
                    // ホストの押下は、相手が来ていて前の STEP が届いていれば入る（届いていなければ core が捨てて数える）。入るまで毎フレーム押す。
                    Role::Host => fi.peer_present,
                    Role::Guest => {
                        if fi.pending.is_some() {
                            // 要求を持っている間の押下は core が捨てる。ホストの STEP を待つ。
                            false
                        } else if self.retry_left > 0 {
                            self.retry_left -= 1;
                            false
                        } else {
                            true
                        }
                    }
                };
                if !go {
                    return Want::default();
                }
                self.pressed = Some(press);
                Want {
                    press: Some(press),
                    ..Want::default()
                }
            }
        }
    }

    /// `selection_frame` の後。`applied` はこのフレームに入った押下、`pending_after` はその後のゲストの要求。
    pub fn after_frame(&mut self, applied: Option<Step>, pending_after: Option<Press>, role: Role) {
        let Some(p) = self.pressed else {
            return;
        };
        if let Some(st) = applied
            && st.kind == p.kind
            && st.value == p.value
            && st.screen == p.screen
        {
            self.next_item();
            return;
        }
        if role == Role::Guest && pending_after.is_none() {
            // 要求が捨てられた（相手が来ていない / ホストが別の押下を先に入れた）。
            self.pressed = None;
            self.retry_left = RETRY_FRAMES;
            self.retries += 1;
        }
    }

    /// 相手が先に次の選択に入った（`Recv::PeerSelecting`）。今の項目が `enter` なら行わずに終える。
    pub fn peer_entered(&mut self) {
        if let Some(Item {
            action: Action::Enter,
            ..
        }) = self.items.get(self.idx)
        {
            self.next_item();
        } else {
            self.since = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: StepKind = StepKind::Decide;
    const C: StepKind = StepKind::Cancel;

    #[test]
    fn the_m2_3_plan_in_the_repository_reads() {
        let p = parse(include_str!("../../../scripts/m2_02_peer_menu.plan"))
            .unwrap_or_else(|e| panic!("scripts/m2_02_peer_menu.plan: {e}"));
        assert_eq!(p.items.len(), 5);
    }

    #[test]
    fn accepted_forms() {
        let p = parse(
            "# 注だけの行\n\
             \n\
             screen=7 press=decide value=3   # 難易度\n\
             screen=8 wait=40 word=RIGHT+X hold=5\n\
             screen=8 press=decide cursor=9 shift=1\n\
             screen=8 press=cancel\n\
             screen=16 press=decide value=0xF\n\
             screen=6 wait=20 enter\n\
             screen=7 word=0x44\n",
        )
        .unwrap();
        let it = &p.items;
        assert_eq!(it.len(), 7);
        assert_eq!(
            it[0],
            Item {
                line: 3,
                screen: 7,
                wait: 0,
                action: Action::Press(PressSpec::Value { kind: D, value: 3 })
            }
        );
        assert_eq!(
            it[1],
            Item {
                line: 4,
                screen: 8,
                wait: 40,
                action: Action::Word {
                    keys: 0x0080 | 0x0002,
                    hold: 5
                }
            }
        );
        // 2P（席 1）・Shift・カーソル 9 = 1 | 2 | 9 << 2 = 0x27。1P なら 0x26。
        let Action::Press(s) = it[2].action else {
            panic!()
        };
        assert_eq!((s.kind(), s.value(1), s.value(0)), (D, 0x27, 0x26));
        let Action::Press(s) = it[3].action else {
            panic!()
        };
        assert_eq!((s.kind(), s.value(1), s.value(0)), (C, 0x01, 0x00));
        assert_eq!(
            it[4].action,
            Action::Press(PressSpec::Value { kind: D, value: 15 })
        );
        assert_eq!(
            (it[5].screen, it[5].wait, it[5].action),
            (6, 20, Action::Enter)
        );
        assert_eq!(
            it[6].action,
            Action::Word {
                keys: 0x44,
                hold: 1
            }
        );
    }

    #[test]
    fn refused_forms() {
        let cases: &[(&str, u32, &str)] = &[
            ("screen=7 press=decide foo=1", 1, "知らない名前"),
            ("\nscreen=7 press=decide value=4", 2, "範囲の外"),
            ("screen=16 press=decide value=16", 1, "範囲の外"),
            ("screen=7 press=decide wait=1 wait=2", 1, "2 回"),
            ("screen=7 wait=3", 1, "ちょうど 1 つ"),
            ("screen=7 word=LEFT press=decide", 1, "ちょうど 1 つ"),
            ("screen=7 press=push", 1, "decide か cancel"),
            ("screen=9 press=decide", 1, "6・7・8・16"),
            ("press=decide", 1, "screen= が要る"),
            ("screen=8 press=decide value=3", 1, "value= を書かない"),
            ("screen=8 press=decide cursor=16", 1, "範囲の外"),
            ("screen=8 press=decide shift=2", 1, "範囲の外"),
            ("screen=8 press=cancel cursor=1", 1, "取消はカーソル 0"),
            ("screen=7 press=decide cursor=1", 1, "キャラの画面"),
            ("screen=7 enter", 1, "enter は screen=6"),
            ("screen=6 press=decide", 1, "enter は screen=6"),
            ("screen=7 word=Z+X", 1, "画面を移るキー"),
            ("screen=7 word=0x100", 1, "SEND_MASK"),
            ("screen=7 word=LEFT+JUMP", 1, "知らないキー名"),
            ("screen=7 word=LEFT hold=0", 1, "範囲の外"),
            ("screen=7 press=decide hold=2", 1, "hold は word="),
            ("screen=7 word=LEFT value=1", 1, "value は press="),
            ("screen=7 press", 1, "`=` が無い"),
            ("# 注だけ\n\n", 0, "1 つも無い"),
        ];
        for (text, line, want) in cases {
            match parse(text) {
                Ok(p) => panic!("{text:?} を通した: {p:?}"),
                Err(e) => {
                    assert_eq!(e.line, *line, "{text:?}: {e}");
                    assert!(e.msg.contains(want), "{text:?}: {e}");
                }
            }
        }
    }

    fn fi(screen: u8, role: Role, present: bool, pending: Option<Press>) -> FrameIn {
        FrameIn {
            screen,
            seat: 1,
            role,
            peer_present: present,
            aligned: true,
            pending,
        }
    }

    #[test]
    fn guest_waits_for_the_alignment_before_pressing() {
        let plan = parse("screen=7 press=decide value=1\n").unwrap();
        let mut r = Runner::new(&plan);
        let unaligned = FrameIn {
            aligned: false,
            ..fi(7, Role::Guest, true, None)
        };
        for _ in 0..5 {
            assert_eq!(r.frame(unaligned), Want::default(), "合わせの前は押さない");
        }
        assert_eq!(r.retries, 0);
        let mut w = Runner::new(&parse("screen=7 word=LEFT hold=2\n").unwrap());
        assert_eq!(
            w.frame(unaligned),
            Want::default(),
            "合わせの前は語も出さない"
        );
        assert_eq!(w.frame(fi(7, Role::Guest, true, None)).word, 0x40);
        let mut e = Runner::new(&parse("screen=6 enter\n").unwrap());
        let out = FrameIn {
            aligned: false,
            ..fi(6, Role::Guest, false, None)
        };
        assert!(e.frame(out).enter, "選択の外の enter は合わせを待たない");
        assert!(
            r.frame(fi(7, Role::Guest, true, None)).press.is_some(),
            "合わせが届いたらすぐ押す（30 フレームの待ちは無い）"
        );
        let mut r = Runner::new(&plan);
        let host = FrameIn {
            aligned: false,
            ..fi(7, Role::Host, true, None)
        };
        assert!(r.frame(host).press.is_some());
    }

    fn step_of(p: Press, step: u16) -> Step {
        Step {
            step,
            kind: p.kind,
            value: p.value,
            screen: p.screen,
        }
    }

    #[test]
    fn runner_waits_holds_and_ends_press_on_apply() {
        let plan = parse(
            "screen=8 wait=2 word=LEFT hold=3\n\
             screen=8 press=decide cursor=5\n\
             screen=16 press=decide value=15\n",
        )
        .unwrap();
        let mut r = Runner::new(&plan);
        let h = Role::Host;
        assert_eq!(r.frame(fi(7, h, true, None)), Want::default());
        assert_eq!(r.frame(fi(8, h, true, None)), Want::default());
        assert_eq!(r.frame(fi(7, h, true, None)), Want::default());
        assert_eq!(r.frame(fi(8, h, true, None)), Want::default());
        assert_eq!(r.frame(fi(8, h, true, None)), Want::default());
        let words: Vec<u16> = (0..4)
            .map(|_| r.frame(fi(8, h, false, None)).word)
            .collect();
        assert_eq!(words, vec![0x40, 0x40, 0x40, 0], "hold=3 の後は次の項目");
        assert_eq!(r.done_items(), 1);
        let want = Press {
            kind: D,
            value: chara_value(1, false, 5),
            screen: 8,
        };
        assert_eq!(r.frame(fi(8, h, true, None)).press, Some(want));
        r.after_frame(None, None, h);
        assert_eq!(r.frame(fi(8, h, true, None)).press, Some(want));
        r.after_frame(
            Some(Step {
                kind: C,
                ..step_of(want, 3)
            }),
            None,
            h,
        );
        assert_eq!(r.done_items(), 1);
        assert_eq!(r.frame(fi(8, h, true, None)).press, Some(want));
        r.after_frame(Some(step_of(want, 3)), None, h);
        assert_eq!(r.done_items(), 2);
        assert_eq!(r.frame(fi(8, h, true, None)), Want::default(), "16 を待つ");
    }

    #[test]
    fn guest_repress_after_drop() {
        let plan = parse("screen=7 press=decide value=2\n").unwrap();
        let mut r = Runner::new(&plan);
        let g = Role::Guest;
        let want = Press {
            kind: D,
            value: 2,
            screen: 7,
        };
        assert_eq!(r.frame(fi(7, g, false, None)).press, Some(want));
        r.after_frame(None, None, g);
        assert_eq!(r.retries, 1);
        for i in 0..RETRY_FRAMES {
            assert_eq!(r.frame(fi(7, g, true, None)).press, None, "{i}");
            r.after_frame(None, None, g);
        }
        assert_eq!(r.frame(fi(7, g, true, None)).press, Some(want));
        r.after_frame(None, Some(want), g);
        for _ in 0..50 {
            assert_eq!(r.frame(fi(7, g, true, Some(want))).press, None);
            r.after_frame(None, Some(want), g);
        }
        assert_eq!(r.retries, 1);
        r.after_frame(
            Some(Step {
                value: 0,
                ..step_of(want, 0)
            }),
            None,
            g,
        );
        assert_eq!(r.retries, 2);
        assert_eq!(r.done_items(), 0);
        for _ in 0..RETRY_FRAMES {
            assert_eq!(r.frame(fi(7, g, true, None)).press, None);
        }
        assert_eq!(r.frame(fi(7, g, true, None)).press, Some(want));
        r.after_frame(Some(step_of(want, 1)), None, g);
        assert_eq!(r.done_items(), 1);
    }

    #[test]
    fn peer_entered_skips_only_enter() {
        let plan = parse("screen=6 wait=5 enter\nscreen=7 press=cancel\n").unwrap();
        let mut r = Runner::new(&plan);
        r.peer_entered();
        assert_eq!(r.done_items(), 1);
        r.peer_entered();
        assert_eq!(r.done_items(), 1, "enter でない項目は飛ばさない");
        let mut r = Runner::new(&plan);
        for _ in 0..5 {
            assert!(!r.frame(fi(6, Role::Host, false, None)).enter);
        }
        assert!(r.frame(fi(6, Role::Host, false, None)).enter);
        assert_eq!(r.done_items(), 1);
    }

    #[test]
    fn every_plan_in_scripts_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).expect("scripts/ が読める") {
            let p = e.expect("scripts/ の項目").path();
            if p.extension().is_some_and(|x| x == "plan") {
                let text = std::fs::read_to_string(&p).expect("台本が読める");
                if let Err(e) = parse(&text) {
                    panic!("{}: {e:?}", p.display());
                }
                n += 1;
            }
        }
        assert!(n >= 2, "scripts/*.plan が {n} 本");
    }
}
