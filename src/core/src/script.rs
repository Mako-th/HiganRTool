//! 台本（text）の読み取りと、1 tick ずつ進める状態機械。
//!
//! DLL のゲームのスレッドから毎 tick 呼ばれる（`Engine::step`）ので、確保もパニックもしない（添字は `get`、足し算は
//! `saturating_*`）。`parse` は台本の読み込み時にしか呼ばれないのでヒープを使ってよい。
//! 段階の `inv=` / `life1=` / `life2=` / `gauge1=` / `gauge2=` は「いつ効くか」だけをここで決め、ゲームのメモリへ書くのは DLL。
//! `inv` は後の段階で変えるまで続く値、ライフとゲージは 1 回だけの値で、書けない tick の間は `Engine` が持ち越す（`take_pending`）。

use crate::input_model::{SEND_MASK, keys};
use std::collections::HashMap;
use std::fmt;

/// 台本の置き場をローダー（`--script`）からゲームの中の DLL へ渡す環境変数の名前。
/// こちらで渡した台本は、DLL が読んだ後に `logs\` へ移さない（2 つのゲームがそれぞれのコピーを持てるように）。
pub const PATH_ENV: &str = "HIGANRTOOL_SCRIPT";

/// 台本 1 本ぶん。`settle` は画面が移ってから 2 枚目を撮るまでの tick（既定 30。0 なら撮らない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub steps: Vec<Step>,
    pub settle: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// 台本ファイルの行番号（1 始まり）。
    pub line: u32,
    pub cond: Cond,
    /// 条件を初めて満たしてから押すまでの tick（1 以上。既定 10）。
    pub wait: u32,
    /// 押す tick 数（`p1`/`p2` がある段階だけ。1 以上。既定 1）。
    pub hold: u32,
    pub p1: u16,
    pub p2: u16,
    /// 条件を待つ上限の tick（既定 1200 = 20 秒）。
    pub limit: u32,
    pub shot_pre: bool,
    pub shot_post: bool,
    /// ログに出す短い名札（空白の無い 1〜64 バイト）。
    pub note: String,
    /// `optional=1`: 条件が `limit` tick の間満たされなくても中止せず、押さずに次の段階へ進む。押す段階は押し始める tick にもう一度
    /// 条件を見て、崩れていれば飛ばす（会話を送る Z が、会話の終わった後のメニューの別の項目を決めないため）。
    pub optional: bool,
    /// `inv=`（無敵にする側のビット `INV_P1` / `INV_P2`。`Some(0)` は切る）。無ければ前の値のまま。
    pub inv: Option<u8>,
    pub set: SetOnce,
    /// `type=` / `bs=` / `enter=`（ルーム名の入力欄へ入れる文字。段階が効き始めたとき 1 回だけ）。
    pub typed: Option<Typed>,
    /// `digit=` / `tab=`（数字キー・Tab をその tick に押したことにする。段階が効き始めたとき 1 回だけ）。
    pub keys: Option<KeyPress>,
}

/// 台本が「押した」ことにする数字キー・Tab。OS には何も注がず、dll の `NetPlay::digit_tick` が読む「押した瞬間」の集合をその 1 tick だけ
/// 差し替える。ビットの並び（数字 d = bit d、Tab = bit10）は dll の `digit_keys` が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyPress {
    /// 押した数字（0〜9。0〜8 が遅れの値、9 が auto）。
    pub digit: Option<u8>,
    /// Tab（対戦の形式の切替）。
    pub tab: bool,
}

/// `type=` の文字の長さの上限（ルーム名の上限 `room::ROOM_NAME_MAX` と同じ）。
pub const TYPED_TEXT_MAX: usize = crate::room::ROOM_NAME_MAX;

/// 台本がルーム名の入力欄へ入れる文字（OS の入力は注がない）。Backspace を `bs` 回 → `type` の文字 → `enter` なら Enter の順。
/// 文字は `room::RoomName::accepts_char` を通るものだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Typed {
    pub backspaces: u8,
    text: [u8; TYPED_TEXT_MAX],
    text_len: u8,
    pub enter: bool,
}

impl Typed {
    pub fn text(&self) -> &str {
        // 作るのは `parse_typed` だけで、ASCII の文字しか入らない。
        core::str::from_utf8(self.text.get(..usize::from(self.text_len)).unwrap_or(&[]))
            .unwrap_or("")
    }

    /// 入れる文字（UTF-16 の単位。`WM_CHAR` の `wParam` と同じ形）の数。
    pub fn unit_count(&self) -> usize {
        usize::from(self.backspaces) + usize::from(self.text_len) + usize::from(self.enter)
    }

    pub fn unit(&self, i: usize) -> Option<u16> {
        let bs = usize::from(self.backspaces);
        let len = usize::from(self.text_len);
        if i < bs {
            Some(crate::room_input::UNIT_BACKSPACE)
        } else if i < bs + len {
            self.text.get(i - bs).map(|&b| u16::from(b))
        } else if i == bs + len && self.enter {
            Some(crate::room_input::UNIT_ENTER)
        } else {
            None
        }
    }
}

/// 無敵にする側のビット。bit0 = 1P、bit1 = 2P（DLL の Player のアドレスの並びと同じ）。
pub const INV_P1: u8 = 1;
pub const INV_P2: u8 = 2;

/// `life1=` / `life2=` の範囲。ゲームが初期ライフに選べる 1〜10（DLL の `th09_addr::LIFE_MIN` / `LIFE_MAX`）。
/// 0 は断る: ゲームがライフを 0 にするのは負けの処理（`0x0041E5CE`）の中だけで、通さずに 0 を置くとラウンドの終わりが呼ばれない恐れがある。
pub const LIFE_SET_MIN: u32 = 1;
pub const LIFE_SET_MAX: u32 = 10;
/// `gauge1=` / `gauge2=` の上限。ゲーム自身が溜めるときの頭打ち 400.0（`0x0041BCF6 movl $0x43c80000`。DLL の `th09_addr::GAUGE_MAX`）。
pub const GAUGE_SET_MAX: u32 = 400;

/// 1 回だけ書く値（添字 0 = 1P、1 = 2P）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetOnce {
    pub life: [Option<u8>; 2],
    pub gauge: [Option<u16>; 2],
}

impl SetOnce {
    pub fn is_empty(&self) -> bool {
        self.life.iter().all(Option::is_none) && self.gauge.iter().all(Option::is_none)
    }

    /// 後から来た値で上書きする（書いていない欄は残す）。
    fn merge(&mut self, newer: SetOnce) {
        for side in 0..2 {
            if let Some(v) = newer.life[side] {
                self.life[side] = Some(v);
            }
            if let Some(v) = newer.gauge[side] {
                self.gauge[side] = Some(v);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cond {
    pub scene: Option<u32>,
    pub screen: Option<u32>,
    pub cursor: Option<u32>,
    pub sub: Option<u32>,
    pub trans: Option<u32>,
    pub pause: Option<bool>,
    pub loading: Option<bool>,
    pub demo: Option<bool>,
    /// マッチの終わりのメニューが押せる（`endmenu=1`）/ 押せない（`endmenu=0`）。
    pub end_menu: Option<bool>,
    /// マッチの終わりのメニューの印 `[0x4A7ECE]` が立っている（`menuflag=1`。会話が出ていても真）。
    pub menu_flag: Option<bool>,
    /// 会話が表示中（`talk=1`）/ 走っていない（`talk=0`）。読めない間はどちらを書いても満たさない（台本が止まって理由が出る方が、
    /// 読めないまま押して会話とメニューを取り違えるより安全）。
    pub talk: Option<bool>,
}

/// DLL がその tick で読めた画面の状態。`screen`/`cursor`/`sub`/`trans` は、場面が 1 で読めたときだけ `Some`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obs {
    pub scene: u32,
    pub screen: Option<u32>,
    pub cursor: Option<u32>,
    pub sub: Option<u32>,
    pub trans: Option<u32>,
    pub pause: bool,
    pub loading: bool,
    pub demo: bool,
    /// マッチの終わりのメニューが出ていて、会話が走っていない（= Z・下がメニューへ届く）。`[0x4A7ECE] != 0` かつ
    /// `[[0x4A7E38]+0xE94C]` が会話中でない、で作る。場面 2 の中でしか 1 にならない。
    pub end_menu: bool,
    /// マッチの終わりのメニューの印 `[0x4A7ECE]` の下位 1 バイトが 0 でない（会話は問わない）。
    pub menu_flag: bool,
    /// Gui の会話が走っているか（ゲーム自身の `0x00404980` と同じ判定）。場面 2 のときだけ読み、Gui が無い・読めない・
    /// 場面 2 でないときは `None`（`talk=` の条件は `None` に対し 0 でも 1 でも満たさない）。
    pub talk: Option<bool>,
}

/// 台本が読めなかった理由。行番号は 1 始まりで、台本全体に関わる理由（段階が 1 つも無い）は 0。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: u32,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}行目: {}", self.line, self.msg)
    }
}

fn err(line: u32, msg: impl Into<String>) -> ParseError {
    ParseError {
        line,
        msg: msg.into(),
    }
}

const MAX_TICK: u32 = 36000;
const MAX_LIMIT: u32 = 216000;
const MAX_STEPS: usize = 1000;
const DEFAULT_WAIT: u32 = 10;
const DEFAULT_HOLD: u32 = 1;
const DEFAULT_LIMIT: u32 = 1200;
const DEFAULT_SETTLE: u32 = 30;

struct Fields<'a> {
    map: HashMap<&'a str, &'a str>,
}

impl<'a> Fields<'a> {
    fn parse(line: u32, tokens: &[&'a str]) -> Result<Self, ParseError> {
        let mut map = HashMap::new();
        for tok in tokens {
            let eq = tok
                .find('=')
                .ok_or_else(|| err(line, format!("`=` が無い欄です: {tok}")))?;
            let name = &tok[..eq];
            let value = &tok[eq + 1..];
            if map.insert(name, value).is_some() {
                return Err(err(line, format!("{name} を 2 回書いています")));
            }
        }
        Ok(Self { map })
    }

    fn get(&self, name: &str) -> Option<&'a str> {
        self.map.get(name).copied()
    }

    fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
}

fn parse_u32(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u32>().ok()
    }
}

fn get_opt_u32(fields: &Fields, line: u32, name: &str) -> Result<Option<u32>, ParseError> {
    match fields.get(name) {
        None => Ok(None),
        Some(v) => parse_u32(v)
            .map(Some)
            .ok_or_else(|| err(line, format!("{name} の値が数として読めない: {v}"))),
    }
}

fn get_opt_bool01(fields: &Fields, line: u32, name: &str) -> Result<Option<bool>, ParseError> {
    match fields.get(name) {
        None => Ok(None),
        Some("0") => Ok(Some(false)),
        Some("1") => Ok(Some(true)),
        Some(v) => Err(err(line, format!("{name} は 0 か 1 だけ: {v}"))),
    }
}

fn get_u32_ranged(
    fields: &Fields,
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

/// キー名 1 つを `SEND_MASK` の中のビットへ。peer_sim の選択の台本も使う。
pub fn key_by_name(name: &str) -> Option<u16> {
    match name {
        "Z" => Some(keys::Z),
        "X" => Some(keys::X),
        "SHIFT" => Some(keys::SHIFT),
        "ESC" => Some(keys::ESC),
        "UP" => Some(keys::UP),
        "DOWN" => Some(keys::DOWN),
        "LEFT" => Some(keys::LEFT),
        "RIGHT" => Some(keys::RIGHT),
        _ => None,
    }
}

/// `p1=`/`p2=`: `0x` の 16 進、またはキー名を `+` でつないだもの。`SEND_MASK` の外のビットが立てば誤り。
fn get_keys(fields: &Fields, line: u32, name: &str) -> Result<u16, ParseError> {
    let Some(v) = fields.get(name) else {
        return Ok(0);
    };
    let value = if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
            .map_err(|_| err(line, format!("{name} の値が 16 進として読めない: {v}")))?
    } else {
        let mut acc: u16 = 0;
        for part in v.split('+') {
            let bit = key_by_name(part)
                .ok_or_else(|| err(line, format!("{name} に知らないキー名があります: {part}")))?;
            acc |= bit;
        }
        acc
    };
    if value & !SEND_MASK != 0 {
        return Err(err(
            line,
            format!("{name} が SEND_MASK（{SEND_MASK:#06x}）の外です: {v}"),
        ));
    }
    Ok(value)
}

const KNOWN_STEP_FIELDS: &[&str] = &[
    "scene", "screen", "cursor", "sub", "trans", "pause", "loading", "demo", "endmenu", "menuflag",
    "talk", "optional", "p1", "p2", "hold", "wait", "limit", "shot", "note", "inv", "life1",
    "life2", "gauge1", "gauge2", "type", "bs", "enter", "digit", "tab",
];

/// `digit=` / `tab=`。`digit` は 0〜9 の 1 桁（2 桁以上・符号・空は断る）、`tab` は 1 だけ。
fn get_keys_press(fields: &Fields, line: u32) -> Result<Option<KeyPress>, ParseError> {
    if !(fields.contains("digit") || fields.contains("tab")) {
        return Ok(None);
    }
    let mut k = KeyPress::default();
    if let Some(v) = fields.get("digit") {
        let b = v.as_bytes();
        match b {
            [d @ b'0'..=b'9'] => k.digit = Some(d - b'0'),
            _ => {
                return Err(err(
                    line,
                    format!("digit は 0〜9 の 1 桁だけ（0〜8 が遅れの値・9 が auto）: {v}"),
                ));
            }
        }
    }
    match fields.get("tab") {
        None => {}
        Some("1") => k.tab = true,
        Some(v) => return Err(err(line, format!("tab は 1 だけ: {v}"))),
    }
    Ok(Some(k))
}

/// `type=` / `bs=` / `enter=`。`type` は `room::RoomName::accepts_char` を通る 1〜`TYPED_TEXT_MAX` 文字（綴りの誤りで
/// 黙って捨てられないよう、使えない文字は断る）。`bs` は 1〜`TYPED_TEXT_MAX`。`enter` は 1 だけ。
fn get_typed(fields: &Fields, line: u32) -> Result<Option<Typed>, ParseError> {
    if !(fields.contains("type") || fields.contains("bs") || fields.contains("enter")) {
        return Ok(None);
    }
    let mut t = Typed::default();
    if let Some(v) = fields.get("type") {
        let n = v.chars().count();
        if !(1..=TYPED_TEXT_MAX).contains(&n) {
            return Err(err(
                line,
                format!("type の長さが範囲の外（1〜{TYPED_TEXT_MAX} 文字）: {n}"),
            ));
        }
        if let Some(bad) = v.chars().find(|&c| !crate::room::RoomName::accepts_char(c)) {
            return Err(err(
                line,
                format!("type に入力が受けない文字があります（英数字と - _ だけ）: {bad:?}"),
            ));
        }
        for (d, s) in t.text.iter_mut().zip(v.bytes()) {
            *d = s;
        }
        t.text_len = n as u8;
    }
    if fields.contains("bs") {
        t.backspaces = get_u32_ranged(fields, line, "bs", 1, 1, TYPED_TEXT_MAX as u32)? as u8;
    }
    match fields.get("enter") {
        None => {}
        Some("1") => t.enter = true,
        Some(v) => return Err(err(line, format!("enter は 1 だけ: {v}"))),
    }
    Ok(Some(t))
}

/// `inv=`: `p1` / `p2` / `p1+p2` / `0` の 4 つだけ。綴りの誤りで黙って無敵にならない・切れないよう、書き方を 1 つに絞る。
fn get_inv(fields: &Fields, line: u32) -> Result<Option<u8>, ParseError> {
    match fields.get("inv") {
        None => Ok(None),
        Some("0") => Ok(Some(0)),
        Some("p1") => Ok(Some(INV_P1)),
        Some("p2") => Ok(Some(INV_P2)),
        Some("p1+p2") => Ok(Some(INV_P1 | INV_P2)),
        Some(v) => Err(err(
            line,
            format!("inv は p1 / p2 / p1+p2 / 0 のどれか: {v}"),
        )),
    }
}

fn get_opt_u32_ranged(
    fields: &Fields,
    line: u32,
    name: &str,
    min: u32,
    max: u32,
) -> Result<Option<u32>, ParseError> {
    if !fields.contains(name) {
        return Ok(None);
    }
    get_u32_ranged(fields, line, name, min, min, max).map(Some)
}

fn get_set_once(fields: &Fields, line: u32) -> Result<SetOnce, ParseError> {
    let mut s = SetOnce::default();
    for (side, (life, gauge)) in [("life1", "gauge1"), ("life2", "gauge2")]
        .into_iter()
        .enumerate()
    {
        // 範囲は u8 / u16 に収まる。
        s.life[side] =
            get_opt_u32_ranged(fields, line, life, LIFE_SET_MIN, LIFE_SET_MAX)?.map(|v| v as u8);
        s.gauge[side] =
            get_opt_u32_ranged(fields, line, gauge, 0, GAUGE_SET_MAX)?.map(|v| v as u16);
    }
    Ok(s)
}

fn parse_step_line(line: u32, tokens: &[&str]) -> Result<Step, ParseError> {
    let fields = Fields::parse(line, tokens)?;

    if fields.contains("settle") {
        return Err(err(
            line,
            "settle はほかの欄と同じ行に書けない（全体の設定は settle だけの行にする）",
        ));
    }
    for name in fields.map.keys() {
        if !KNOWN_STEP_FIELDS.contains(name) {
            return Err(err(line, format!("知らない名前です: {name}")));
        }
    }

    let cond = Cond {
        scene: get_opt_u32(&fields, line, "scene")?,
        screen: get_opt_u32(&fields, line, "screen")?,
        cursor: get_opt_u32(&fields, line, "cursor")?,
        sub: get_opt_u32(&fields, line, "sub")?,
        trans: get_opt_u32(&fields, line, "trans")?,
        pause: get_opt_bool01(&fields, line, "pause")?,
        loading: get_opt_bool01(&fields, line, "loading")?,
        demo: get_opt_bool01(&fields, line, "demo")?,
        end_menu: get_opt_bool01(&fields, line, "endmenu")?,
        menu_flag: get_opt_bool01(&fields, line, "menuflag")?,
        talk: get_opt_bool01(&fields, line, "talk")?,
    };
    let optional = get_opt_bool01(&fields, line, "optional")?.unwrap_or(false);

    let p1 = get_keys(&fields, line, "p1")?;
    let p2 = get_keys(&fields, line, "p2")?;
    let has_keys = p1 != 0 || p2 != 0;

    if fields.contains("hold") && !has_keys {
        return Err(err(line, "p1 も p2 も無い行に hold は書けない"));
    }

    let wait = get_u32_ranged(&fields, line, "wait", DEFAULT_WAIT, 1, MAX_TICK)?;
    let hold = if fields.contains("hold") {
        get_u32_ranged(&fields, line, "hold", DEFAULT_HOLD, 1, MAX_TICK)?
    } else {
        DEFAULT_HOLD
    };
    let limit = get_u32_ranged(&fields, line, "limit", DEFAULT_LIMIT, 1, MAX_LIMIT)?;

    let (shot_pre, shot_post) = match fields.get("shot").unwrap_or("both") {
        "pre" => (true, false),
        "post" => (false, true),
        "both" => (true, true),
        "none" => (false, false),
        other => return Err(err(line, format!("shot の値が知らないものです: {other}"))),
    };

    let note = match fields.get("note") {
        None => String::new(),
        Some(v) => {
            let len = v.len();
            if !(1..=64).contains(&len) {
                return Err(err(
                    line,
                    format!("note の長さが範囲の外（1〜64 バイト）: {len}"),
                ));
            }
            v.to_string()
        }
    };

    let inv = get_inv(&fields, line)?;
    let set = get_set_once(&fields, line)?;
    let typed = get_typed(&fields, line)?;
    let keys = get_keys_press(&fields, line)?;

    Ok(Step {
        line,
        cond,
        wait,
        hold,
        p1,
        p2,
        limit,
        shot_pre,
        shot_post,
        note,
        optional,
        inv,
        set,
        typed,
        keys,
    })
}

pub fn parse(text: &str) -> Result<Script, ParseError> {
    let mut steps = Vec::new();
    let mut settle: Option<u32> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line = (idx + 1) as u32;
        let content = match raw_line.find('#') {
            Some(p) => &raw_line[..p],
            None => raw_line,
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = content.split_whitespace().collect();

        if tokens.len() == 1 && tokens[0].starts_with("settle=") {
            let value = &tokens[0]["settle=".len()..];
            let v = parse_u32(value)
                .ok_or_else(|| err(line, format!("settle の値が数として読めない: {value}")))?;
            if settle.is_some() {
                return Err(err(line, "settle を 2 回書いています"));
            }
            settle = Some(v);
            continue;
        }

        let step = parse_step_line(line, &tokens)?;
        if steps.len() >= MAX_STEPS {
            return Err(err(line, format!("段は {MAX_STEPS} までです")));
        }
        steps.push(step);
    }

    if steps.is_empty() {
        return Err(ParseError {
            line: 0,
            msg: "台本に段が 1 つも無い".to_string(),
        });
    }

    Ok(Script {
        steps,
        settle: settle.unwrap_or(DEFAULT_SETTLE),
    })
}

// ---- 状態機械（Engine） ----

/// 撮影の理由のビット。1 つの tick に複数立ちうる（`Out::shot` は OR で合成する）。
pub const START: u8 = 1;
pub const PRE: u8 = 2;
pub const POST: u8 = 4;
pub const TRANSITION: u8 = 8;
pub const SETTLE: u8 = 16;
pub const ABORT: u8 = 32;
pub const END: u8 = 64;
pub const WAIT: u8 = 128;

/// 1 tick ぶんの出力。`feed` が `None` のときは「台本は何もしないので元の poll_all へ戻せ」の合図。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Out {
    pub feed: Option<(u16, u16)>,
    pub shot: u8,
    pub step: u16,
    /// この tick の無敵の側。台本が終わった・止まった tick からは 0。
    pub inv: u8,
    /// この tick に効き始めた 1 回だけの値（書くのは `Engine::take_pending` の側）。
    pub set: SetOnce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running { step: u16 },
    Done,
    Aborted { step: u16, line: u32 },
}

/// 段階の中の細かい位置。t0（条件を初めて満たした tick）からの相対位置を「残り tick 数」で持つ（桁あふれの心配が無い）。
/// `fails` は条件を満たさなかった tick の数。`remaining` は「この tick を含め、あと何 tick がその区間か」（0 にはならない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    WaitCond { fails: u32 },
    Waiting { remaining: u32 },
    Holding { remaining: u32 },
}

enum Terminal {
    Done,
    Aborted { step: u16, line: u32 },
}

/// 台本を 1 tick ずつ進める状態機械。
pub struct Engine {
    script: Script,
    index: usize,
    phase: Phase,
    first_tick: bool,
    /// 前 tick の (scene, screen, pause)。
    prev_key: (u32, Option<u32>, bool),
    /// SETTLE が予約されている残り tick 数（1 なら次に処理する tick で発火）。
    settle_pending: Option<u32>,
    terminal: Option<Terminal>,
    inv: u8,
    /// 効き始めたが DLL がまだ書いていない 1 回だけの値。
    pending: SetOnce,
    fired: SetOnce,
    /// 効き始めたが DLL がまだ入力欄へ入れていない文字。
    typed: Option<Typed>,
    /// 効き始めたが DLL がまだ読んでいない数字キー・Tab の押下。
    keys: Option<KeyPress>,
}

impl Engine {
    pub fn new(script: Script) -> Self {
        Self {
            index: 0,
            phase: Phase::WaitCond { fails: 0 },
            first_tick: true,
            prev_key: (0, None, false),
            settle_pending: None,
            terminal: None,
            inv: 0,
            pending: SetOnce::default(),
            fired: SetOnce::default(),
            typed: None,
            keys: None,
            script,
        }
    }

    /// 効き始めた数字キー・Tab の押下を 1 回だけ取り出す。次の段階が効き始める前に取られなかった押下は、後の段階のもので置き換わる。
    pub fn take_keys(&mut self) -> Option<KeyPress> {
        if self.terminal.is_some() {
            return None;
        }
        self.keys.take()
    }

    /// 効き始めた文字を 1 回だけ取り出す。次の段階が効き始める前に取られなかった文字は、後の段階のもので置き換わる。
    pub fn take_typed(&mut self) -> Option<Typed> {
        if self.terminal.is_some() {
            return None;
        }
        self.typed.take()
    }

    /// 今の無敵の側。台本が終わった・止まったら 0。
    pub fn inv(&self) -> u8 {
        if self.terminal.is_some() { 0 } else { self.inv }
    }

    pub fn pending(&self) -> SetOnce {
        self.pending
    }

    /// 書ける側（`writable[0]` = 1P、`[1]` = 2P）の 1 回だけの値を取り出す。書けない側の値は次の tick へ持ち越す。
    pub fn take_pending(&mut self, writable: [bool; 2]) -> SetOnce {
        let mut out = SetOnce::default();
        if self.terminal.is_some() {
            return out;
        }
        for (side, ok) in writable.into_iter().enumerate() {
            if ok {
                out.life[side] = self.pending.life[side].take();
                out.gauge[side] = self.pending.gauge[side].take();
            }
        }
        out
    }

    /// 持ち越している値を全部捨てて返す（台本が終わった・止まったのに書けていなかった分。ログに残すため）。
    pub fn discard_pending(&mut self) -> SetOnce {
        core::mem::take(&mut self.pending)
    }

    /// 段階 `self.index` の状態の操作を効かせる（その段階がキーを押し始める tick か、キーの無い段階の wait の終わりの tick）。
    fn activate(&mut self) {
        let Some(st) = self.script.steps.get(self.index) else {
            return;
        };
        if let Some(v) = st.inv {
            self.inv = v;
        }
        let set = st.set;
        let typed = st.typed;
        let keys = st.keys;
        self.pending.merge(set);
        self.fired.merge(set);
        if typed.is_some() {
            self.typed = typed;
        }
        if keys.is_some() {
            self.keys = keys;
        }
    }

    pub fn status(&self) -> Status {
        match &self.terminal {
            Some(Terminal::Done) => Status::Done,
            Some(Terminal::Aborted { step, line }) => Status::Aborted {
                step: *step,
                line: *line,
            },
            None => Status::Running {
                step: self.index.min(self.script.steps.len()) as u16,
            },
        }
    }

    /// 画面の組の遷移から TRANSITION / SETTLE / START のビットを作る。既存の予約を先に消化してから、この tick の遷移で
    /// 新しい予約を作る（「新しい移りで置き換える」を、同じ tick 内で古い予約の消化と取り違えないため）。
    fn screen_bits(&mut self, obs: &Obs) -> u8 {
        let mut shot = 0u8;
        let cur_key = (obs.scene, obs.screen, obs.pause);

        if let Some(n) = self.settle_pending {
            if n <= 1 {
                shot |= SETTLE;
                self.settle_pending = None;
            } else {
                self.settle_pending = Some(n - 1);
            }
        }

        if self.first_tick {
            shot |= START;
            self.first_tick = false;
        } else if cur_key != self.prev_key {
            shot |= TRANSITION;
            self.settle_pending = if self.script.settle > 0 {
                Some(self.script.settle)
            } else {
                None
            };
        }
        self.prev_key = cur_key;
        shot
    }

    pub fn step(&mut self, obs: &Obs) -> Out {
        self.fired = SetOnce::default();
        if self.terminal.is_some() {
            return Out {
                feed: None,
                shot: 0,
                step: self.status_step(),
                inv: 0,
                set: SetOnce::default(),
            };
        }

        let mut shot = self.screen_bits(obs);

        if self.index >= self.script.steps.len() {
            // 最後の段階が終わった次の tick で Done になり、feed: None・shot |= END。
            shot |= END;
            self.terminal = Some(Terminal::Done);
            return Out {
                feed: None,
                shot,
                step: self.script.steps.len() as u16,
                inv: 0,
                set: SetOnce::default(),
            };
        }

        let i = self.index;
        let cond = self.script.steps[i].cond;
        let wait = self.script.steps[i].wait;
        let hold = self.script.steps[i].hold;
        let limit = self.script.steps[i].limit;
        let p1 = self.script.steps[i].p1;
        let p2 = self.script.steps[i].p2;
        let shot_pre = self.script.steps[i].shot_pre;
        let shot_post = self.script.steps[i].shot_post;
        let line = self.script.steps[i].line;
        let optional = self.script.steps[i].optional;
        let has_keys = p1 != 0 || p2 != 0;

        let feed;
        match self.phase {
            Phase::WaitCond { fails } => {
                if matches_cond(&cond, obs) {
                    feed = Some((0u16, 0u16));
                    if wait == 1 {
                        shot |= self.finish_wait(has_keys, hold, shot_pre, shot_post);
                    } else {
                        self.phase = Phase::Waiting {
                            remaining: wait - 1,
                        };
                    }
                } else {
                    let fails = fails.saturating_add(1);
                    if fails >= limit && optional {
                        // 出なかった段階は飛ばす（会話が出ないマッチでも台本が止まらない）。この tick は何も押さない。
                        self.advance_step();
                    } else if fails >= limit {
                        shot |= ABORT;
                        self.terminal = Some(Terminal::Aborted {
                            step: i as u16,
                            line,
                        });
                        return Out {
                            feed: None,
                            shot,
                            step: i as u16,
                            inv: 0,
                            set: SetOnce::default(),
                        };
                    } else {
                        self.phase = Phase::WaitCond { fails };
                    }
                    feed = Some((0u16, 0u16));
                }
            }
            Phase::Waiting { remaining } => {
                feed = Some((0u16, 0u16));
                if remaining > 1 {
                    self.phase = Phase::Waiting {
                        remaining: remaining - 1,
                    };
                } else {
                    shot |= self.finish_wait(has_keys, hold, shot_pre, shot_post);
                }
            }
            Phase::Holding { remaining } => {
                let first = remaining == hold;
                if first && optional && !matches_cond(&cond, obs) {
                    // 待っている間に条件が崩れた（会話が終わってメニューだけになった、など）: 押さずに飛ばす。
                    feed = Some((0u16, 0u16));
                    self.advance_step();
                } else {
                    if first {
                        self.activate();
                    }
                    feed = Some((p1 & SEND_MASK, p2 & SEND_MASK));
                    if remaining > 1 {
                        self.phase = Phase::Holding {
                            remaining: remaining - 1,
                        };
                    } else {
                        if shot_post {
                            shot |= POST;
                        }
                        self.advance_step();
                    }
                }
            }
        }

        Out {
            feed,
            shot,
            step: i as u16,
            inv: self.inv,
            set: self.fired,
        }
    }

    /// Wait 区間の最後の tick に来た処理。キーがある段階は PRE を立てて Hold 区間へ、無い段階は WAIT を立てて次の段階へ。
    fn finish_wait(&mut self, has_keys: bool, hold: u32, shot_pre: bool, shot_post: bool) -> u8 {
        let mut shot = 0u8;
        if has_keys {
            if shot_pre {
                shot |= PRE;
            }
            self.phase = Phase::Holding { remaining: hold };
        } else {
            if shot_post {
                shot |= WAIT;
            }
            // キーの無い段階は wait の終わりの tick から状態の操作が効く（段階を進める前に。`activate` は今の段階を見る）。
            self.activate();
            self.advance_step();
        }
        shot
    }

    fn advance_step(&mut self) {
        self.index = self.index.saturating_add(1);
        self.phase = Phase::WaitCond { fails: 0 };
    }

    fn status_step(&self) -> u16 {
        match &self.terminal {
            Some(Terminal::Done) => self.script.steps.len() as u16,
            Some(Terminal::Aborted { step, .. }) => *step,
            None => self.index.min(self.script.steps.len()) as u16,
        }
    }
}

fn matches_cond(cond: &Cond, obs: &Obs) -> bool {
    if let Some(x) = cond.scene
        && obs.scene != x
    {
        return false;
    }
    if let Some(x) = cond.screen
        && obs.screen != Some(x)
    {
        return false;
    }
    if let Some(x) = cond.cursor
        && obs.cursor != Some(x)
    {
        return false;
    }
    if let Some(x) = cond.sub
        && obs.sub != Some(x)
    {
        return false;
    }
    if let Some(x) = cond.trans
        && obs.trans != Some(x)
    {
        return false;
    }
    if let Some(b) = cond.pause
        && obs.pause != b
    {
        return false;
    }
    if let Some(b) = cond.loading
        && obs.loading != b
    {
        return false;
    }
    if let Some(b) = cond.demo
        && obs.demo != b
    {
        return false;
    }
    if let Some(b) = cond.end_menu
        && obs.end_menu != b
    {
        return false;
    }
    if let Some(b) = cond.menu_flag
        && obs.menu_flag != b
    {
        return false;
    }
    // 読めない（`None`）ときは 0 でも 1 でも満たさない。
    if let Some(b) = cond.talk
        && obs.talk != Some(b)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_scene(scene: u32) -> Obs {
        Obs {
            scene,
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

    // ---- parse: 正常系 ----

    #[test]
    fn parse_defaults() {
        let s = parse("scene=1\n").expect("parse");
        assert_eq!(s.settle, 30);
        assert_eq!(s.steps.len(), 1);
        let st = &s.steps[0];
        assert_eq!(st.wait, 10);
        assert_eq!(st.hold, 1);
        assert_eq!(st.limit, 1200);
        assert!(st.shot_pre && st.shot_post);
        assert_eq!(st.p1, 0);
        assert_eq!(st.p2, 0);
        assert_eq!(st.note, "");
    }

    #[test]
    fn parse_strips_comments_and_blank_lines() {
        let text = "\n  # comment only\nscene=1 note=a # trailing comment\n\n";
        let s = parse(text).expect("parse");
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.steps[0].note, "a");
        assert_eq!(s.steps[0].line, 3);
    }

    #[test]
    fn parse_key_combo() {
        let s = parse("scene=1 p1=Z+DOWN\n").expect("parse");
        assert_eq!(s.steps[0].p1, keys::Z | keys::DOWN);
    }

    #[test]
    fn parse_settle_line() {
        let s = parse("settle=5\nscene=1\n").expect("parse");
        assert_eq!(s.settle, 5);
        assert_eq!(s.steps.len(), 1);
    }

    #[test]
    fn parse_endmenu_condition() {
        let s = parse("scene=2 endmenu=1 p1=DOWN\nscene=2 endmenu=0\nscene=2\n").expect("parse");
        let v: Vec<_> = s.steps.iter().map(|st| st.cond.end_menu).collect();
        assert_eq!(v, vec![Some(true), Some(false), None]);
    }

    // ---- parse: 否定系 ----

    #[test]
    fn parse_rejects_bad_endmenu_value() {
        for bad in ["endmenu=2", "endmenu=true", "endmenu=", "endmenu=-1"] {
            let e = parse(&format!("scene=2\n{bad}\n")).expect_err(bad);
            assert_eq!(e.line, 2, "{bad}: {e}");
        }
    }

    #[test]
    fn parse_rejects_bits_outside_send_mask_hex_enter() {
        let e = parse("scene=1 p1=0x1000\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_bits_outside_send_mask_hex_p_home() {
        let e = parse("scene=1 p2=0x0800\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_wait_zero() {
        let e = parse("scene=1 wait=0\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_hold_without_keys() {
        let e = parse("scene=1 hold=2\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_unknown_field_name() {
        let e = parse("scene=1 scean=1\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_settle_mixed_with_other_fields() {
        let e = parse("settle=5 scene=1\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_settle_twice() {
        let e = parse("settle=5\nsettle=6\nscene=1\n").unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn parse_rejects_zero_steps() {
        let e = parse("settle=5\n# comment only\n").unwrap_err();
        assert_eq!(e.line, 0);
    }

    #[test]
    fn parse_rejects_missing_equals() {
        let e = parse("scene\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_duplicate_field() {
        let e = parse("scene=1 scene=2\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn parse_rejects_limit_out_of_range() {
        let e = parse("scene=1 limit=0\n").unwrap_err();
        assert_eq!(e.line, 1);
    }

    // ---- Engine: 進み方 ----

    #[test]
    fn engine_waits_until_condition_then_enters_wait_phase() {
        let script = parse("scene=2 wait=3\n").unwrap();
        let mut eng = Engine::new(script);

        let out1 = eng.step(&obs_scene(1)); // 条件を満たさない
        assert_eq!(out1.feed, Some((0, 0)));
        assert_eq!(out1.shot, START);
        assert_eq!(out1.step, 0);

        let out2 = eng.step(&obs_scene(1)); // まだ満たさない
        assert_eq!(out2.feed, Some((0, 0)));
        assert_eq!(out2.shot, 0);

        let out3 = eng.step(&obs_scene(2)); // 満たす（t0）。wait=3 なので Wait 区間へ
        assert_eq!(out3.feed, Some((0, 0)));
        assert_eq!(out3.shot, TRANSITION); // scene が 1→2 で変わった
    }

    #[test]
    fn engine_pre_and_post_fire_at_exact_ticks() {
        let script = parse("scene=1 p1=Z wait=3 hold=2\n").unwrap();
        let mut eng = Engine::new(script);
        let o = obs_scene(1); // 最初から条件を満たす（t0 = tick1）

        let out1 = eng.step(&o);
        assert_eq!(out1.feed, Some((0, 0)));
        assert_eq!(out1.shot, START);

        let out2 = eng.step(&o);
        assert_eq!(out2.feed, Some((0, 0)));
        assert_eq!(out2.shot, 0);

        let out3 = eng.step(&o);
        assert_eq!(out3.feed, Some((0, 0)));
        assert_eq!(out3.shot, PRE);

        let out4 = eng.step(&o);
        assert_eq!(out4.feed, Some((keys::Z, 0)));
        assert_eq!(out4.shot, 0);

        let out5 = eng.step(&o);
        assert_eq!(out5.feed, Some((keys::Z, 0)));
        assert_eq!(out5.shot, POST);

        let out6 = eng.step(&o);
        assert_eq!(out6.feed, None);
        assert_eq!(out6.shot, END);
        assert_eq!(eng.status(), Status::Done);
    }

    #[test]
    fn engine_aborts_when_limit_reached() {
        let script = parse("scene=99 limit=3\n").unwrap();
        let mut eng = Engine::new(script);

        let out1 = eng.step(&obs_scene(1));
        assert_eq!(out1.feed, Some((0, 0)));
        let out2 = eng.step(&obs_scene(1));
        assert_eq!(out2.feed, Some((0, 0)));
        let out3 = eng.step(&obs_scene(1)); // 3 回目の不一致で limit に達する
        assert_eq!(out3.feed, None);
        assert_eq!(out3.shot, ABORT);
        assert_eq!(out3.step, 0);
        assert_eq!(eng.status(), Status::Aborted { step: 0, line: 1 });

        let out4 = eng.step(&obs_scene(99));
        assert_eq!(out4.feed, None);
        assert_eq!(out4.shot, 0);
    }

    #[test]
    fn engine_keyless_step_shoots_wait_then_advances() {
        let script = parse("scene=1 wait=2\nscene=1 wait=1 note=b\n").unwrap();
        let mut eng = Engine::new(script);
        let o = obs_scene(1);

        let out1 = eng.step(&o); // t0 (段階0)
        assert_eq!(out1.feed, Some((0, 0)));
        let out2 = eng.step(&o); // t0+wait-1 (段階0)。WAIT が立ち、段階1へ。
        assert_eq!(out2.feed, Some((0, 0)));
        assert_eq!(out2.shot, WAIT);
        assert_eq!(out2.step, 0);

        let out3 = eng.step(&o); // 段階1: wait=1 なのでこの tick がそのまま最後。
        assert_eq!(out3.feed, Some((0, 0)));
        assert_eq!(out3.shot, WAIT);
        assert_eq!(out3.step, 1);
    }

    #[test]
    fn engine_settle_reservation_is_replaced_by_new_transition() {
        let script = parse("settle=3\nscene=1 wait=1000\n").unwrap();
        let mut eng = Engine::new(script);

        eng.step(&obs_scene(1)); // tick1: START（t0）
        let a = eng.step(&obs_scene(2)); // tick2: scene 1→2 で TRANSITION。settle_pending=3
        assert_eq!(a.shot, TRANSITION);
        let b = eng.step(&obs_scene(2)); // tick3: 変化なし。settle_pending=2
        assert_eq!(b.shot, 0);
        // tick4 で本来 SETTLE が立つはずが、ここで新しい遷移が来て置き換わる。
        let c = eng.step(&obs_scene(3)); // tick4: scene 2→3 で TRANSITION。旧予約は消える。
        assert_eq!(c.shot, TRANSITION);
        assert_eq!(
            c.shot & SETTLE,
            0,
            "古い予約の SETTLE が紛れ込んではいけない"
        );

        let d = eng.step(&obs_scene(3)); // tick5
        assert_eq!(d.shot, 0);
        let e = eng.step(&obs_scene(3)); // tick6
        assert_eq!(e.shot, 0);
        let f = eng.step(&obs_scene(3)); // tick7 = tick4(遷移) + settle(3)
        assert_eq!(f.shot, SETTLE);
    }

    #[test]
    fn engine_end_fires_exactly_once_and_step_is_len_after_done() {
        let script = parse("scene=1 wait=1\n").unwrap();
        let mut eng = Engine::new(script);
        let o = obs_scene(1);

        eng.step(&o); // 段階0完了（wait=1 なのでこの tick で完了、キー無しなので即次へ）
        let end_out = eng.step(&o); // Done への tick
        assert_eq!(end_out.shot, END);
        assert_eq!(end_out.feed, None);
        assert_eq!(end_out.step, 1);

        let after = eng.step(&o); // Done の後
        assert_eq!(after.shot, 0);
        assert_eq!(after.feed, None);
        assert_eq!(after.step, 1);
        assert_eq!(eng.status(), Status::Done);
    }

    #[test]
    fn cond_some_does_not_match_obs_none() {
        let cond = Cond {
            screen: Some(1),
            ..Default::default()
        };
        let obs = Obs {
            scene: 1,
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
        };
        assert!(!matches_cond(&cond, &obs));
    }

    #[test]
    fn engine_endmenu_gates_the_press() {
        let script =
            parse("scene=2 endmenu=1 wait=2 p1=DOWN\nscene=2 endmenu=0 wait=1 p1=Z\n").unwrap();
        let mut eng = Engine::new(script);
        let with = |end_menu: bool| Obs {
            end_menu,
            ..obs_scene(2)
        };
        for _ in 0..5 {
            let o = eng.step(&with(false));
            assert_eq!((o.feed, o.step), (Some((0, 0)), 0));
        }
        assert_eq!(eng.step(&with(true)).feed, Some((0, 0)));
        assert_eq!(eng.step(&with(true)).feed, Some((0, 0)));
        let o = eng.step(&with(true));
        assert_eq!(o.feed, Some((keys::DOWN, 0)));
        let o = eng.step(&with(true));
        assert_eq!((o.feed, o.step), (Some((0, 0)), 1));
        let o = eng.step(&with(false)); // wait=1: 条件を満たした tick が PRE、次が Z
        assert_eq!(o.feed, Some((0, 0)));
        assert_eq!(eng.step(&with(false)).feed, Some((keys::Z, 0)));
    }

    #[test]
    fn parse_talk_menuflag_and_optional() {
        let s = parse(
            "scene=2 talk=1 optional=1 p1=Z\nscene=2 talk=0 menuflag=1\nscene=2 optional=0\nscene=2\n",
        )
        .expect("parse");
        let v: Vec<_> = s
            .steps
            .iter()
            .map(|st| (st.cond.talk, st.cond.menu_flag, st.optional))
            .collect();
        assert_eq!(
            v,
            vec![
                (Some(true), None, true),
                (Some(false), Some(true), false),
                (None, None, false),
                (None, None, false)
            ]
        );
        for bad in [
            "talk=2",
            "talk=",
            "menuflag=yes",
            "optional=-1",
            "optional=2",
        ] {
            let e = parse(&format!("scene=2\n{bad}\n")).expect_err(bad);
            assert_eq!(e.line, 2, "{bad}: {e}");
        }
    }

    #[test]
    fn cond_talk_is_never_true_when_unreadable() {
        let with = |talk: Option<bool>, menu_flag: bool| Obs {
            talk,
            menu_flag,
            ..obs_scene(2)
        };
        let talk1 = Cond {
            talk: Some(true),
            ..Default::default()
        };
        let talk0 = Cond {
            talk: Some(false),
            ..Default::default()
        };
        assert!(matches_cond(&talk1, &with(Some(true), false)));
        assert!(!matches_cond(&talk1, &with(Some(false), true)));
        assert!(
            !matches_cond(&talk1, &with(None, true)),
            "読めないのに talk=1"
        );
        assert!(matches_cond(&talk0, &with(Some(false), false)));
        assert!(!matches_cond(&talk0, &with(Some(true), false)));
        assert!(
            !matches_cond(&talk0, &with(None, false)),
            "読めないのに talk=0"
        );
        let flag = Cond {
            menu_flag: Some(true),
            ..Default::default()
        };
        assert!(matches_cond(&flag, &with(Some(true), true)));
        assert!(matches_cond(&flag, &with(None, true)));
        assert!(!matches_cond(&flag, &with(Some(false), false)));
    }

    #[test]
    fn engine_optional_step_skips_instead_of_aborting_and_rechecks_at_press() {
        let talk = |on: bool| Obs {
            talk: Some(on),
            ..obs_scene(2)
        };
        let script =
            parse("scene=2 talk=1 optional=1 limit=3 p1=DOWN\nscene=2 wait=1 p1=Z\n").unwrap();
        let mut eng = Engine::new(script);
        for k in 0..3 {
            let o = eng.step(&talk(false));
            assert_eq!(o.feed, Some((0, 0)), "k={k}");
            assert_eq!(o.shot & ABORT, 0, "k={k}: 中止した");
        }
        assert_eq!(eng.status(), Status::Running { step: 1 });
        eng.step(&talk(false)); // 段階 1 の wait=1（PRE）
        assert_eq!(eng.step(&talk(false)).feed, Some((keys::Z, 0)));
        let script = parse("scene=2 talk=1 optional=1 wait=2 p1=Z\nscene=2 wait=1\n").unwrap();
        let mut eng = Engine::new(script);
        eng.step(&talk(true));
        eng.step(&talk(true));
        assert_eq!(eng.step(&talk(true)).feed, Some((keys::Z, 0)));
        let mut eng =
            Engine::new(parse("scene=2 talk=1 optional=1 wait=2 p1=Z\nscene=2 wait=1\n").unwrap());
        eng.step(&talk(true)); // t0
        eng.step(&talk(true)); // wait の最後
        let o = eng.step(&talk(false)); // 押し始める tick に条件が崩れている
        assert_eq!(o.feed, Some((0, 0)), "崩れたのに押した");
        assert_eq!(eng.status(), Status::Running { step: 1 });
        let mut eng = Engine::new(parse("scene=2 talk=1 limit=2 p1=Z\n").unwrap());
        eng.step(&talk(false));
        let o = eng.step(&talk(false));
        assert_eq!(o.shot & ABORT, ABORT);
        assert!(matches!(eng.status(), Status::Aborted { .. }));
    }

    // ---- マッチの状態の操作（inv / life / gauge） ----

    #[test]
    fn parse_state_ops() {
        let s = parse(
            "inv=p1\ninv=p2\ninv=p1+p2 life1=1 gauge2=400\ninv=0 life2=10 gauge1=0\nscene=1\n",
        )
        .expect("parse");
        let inv: Vec<_> = s.steps.iter().map(|st| st.inv).collect();
        assert_eq!(inv, vec![Some(1), Some(2), Some(3), Some(0), None]);
        assert_eq!(s.steps[2].set.life, [Some(1), None]);
        assert_eq!(s.steps[2].set.gauge, [None, Some(400)]);
        assert_eq!(s.steps[3].set.life, [None, Some(10)]);
        assert_eq!(s.steps[3].set.gauge, [Some(0), None]);
        assert!(s.steps[4].set.is_empty());
    }

    #[test]
    fn parse_rejects_bad_state_ops() {
        for bad in [
            "inv=p3",
            "inv=",
            "inv=p2+p1",
            "inv=P1",
            "inv=1",
            "life1=11",
            "life1=0",
            "life2=",
            "gauge1=401",
            "gauge2=-1",
            "gauge1=1.5",
            "lif1=3",
            "gauge3=1",
            "life1=3 life1=4",
        ] {
            let text = format!("scene=1\n{bad}\n");
            let e = parse(&text).expect_err(bad);
            assert_eq!(e.line, 2, "{bad}: {e}");
        }
        parse("life1=10 life2=1 gauge1=400 gauge2=0\n").expect("境目の内側");
    }

    #[test]
    fn engine_inv_starts_at_the_press_tick_and_lasts_until_changed() {
        let script =
            parse("scene=1 wait=2 p1=Z hold=2 inv=p1+p2\nscene=1 wait=2\nscene=1 wait=1 inv=0\n")
                .unwrap();
        let mut eng = Engine::new(script);
        let o = obs_scene(1);
        let invs: Vec<u8> = (0..8).map(|_| eng.step(&o).inv).collect();
        // tick 0〜1: 段階0 の wait。tick 2〜3: 押す。tick 4〜5: 段階1 の wait。tick 6: 段階2 は wait=1 なのでその tick に inv=0 が効く。tick 7: Done。
        assert_eq!(invs, vec![0, 0, 3, 3, 3, 3, 0, 0]);
        assert_eq!(eng.status(), Status::Done);
        assert_eq!(eng.inv(), 0);
    }

    #[test]
    fn engine_keyless_inv_starts_at_the_end_of_wait_and_stops_on_abort() {
        let script = parse("scene=1 wait=3 inv=p2\nscene=9 limit=2\n").unwrap();
        let mut eng = Engine::new(script);
        let a = eng.step(&obs_scene(1)).inv; // t0
        let b = eng.step(&obs_scene(1)).inv;
        let c = eng.step(&obs_scene(1)).inv; // t0 + wait − 1
        assert_eq!((a, b, c), (0, 0, INV_P2));
        assert_eq!(eng.step(&obs_scene(1)).inv, INV_P2); // 段階1 の条件待ち（1 回目の不一致）
        assert_eq!(eng.inv(), INV_P2);
        let stop = eng.step(&obs_scene(1)); // 2 回目の不一致で止まる
        assert_eq!((stop.shot & ABORT, stop.inv), (ABORT, 0));
        assert_eq!(eng.inv(), 0);
    }

    #[test]
    fn engine_one_shot_values_are_taken_once_and_carried_over() {
        let script = parse(
            "scene=1 wait=1 life1=1 gauge2=300\nscene=1 wait=1 gauge2=400\nscene=1 wait=50\n",
        )
        .unwrap();
        let mut eng = Engine::new(script);
        let o = obs_scene(1);
        let out = eng.step(&o); // 段階0 の wait=1 の tick に効く
        assert_eq!(out.set.life, [Some(1), None]);
        assert_eq!(out.set.gauge, [None, Some(300)]);
        let t = eng.take_pending([true, false]);
        assert_eq!((t.life, t.gauge), ([Some(1), None], [None, None]));
        assert_eq!(eng.pending().gauge, [None, Some(300)]);
        assert!(eng.take_pending([true, false]).is_empty());
        let out = eng.step(&o);
        assert_eq!(out.set.gauge, [None, Some(400)]);
        assert!(eng.step(&o).set.is_empty());
        let t = eng.take_pending([true, true]);
        assert_eq!((t.life, t.gauge), ([None, None], [None, Some(400)]));
        assert!(eng.take_pending([true, true]).is_empty(), "2 回取り出せた");
        assert!(eng.pending().is_empty());
    }

    #[test]
    fn engine_pending_is_not_taken_after_done_but_can_be_discarded() {
        let script = parse("scene=1 wait=1 life2=3\n").unwrap();
        let mut eng = Engine::new(script);
        let o = obs_scene(1);
        eng.step(&o);
        assert!(eng.take_pending([false, false]).is_empty());
        eng.step(&o); // Done
        assert_eq!(eng.status(), Status::Done);
        assert!(eng.take_pending([true, true]).is_empty());
        assert_eq!(eng.discard_pending().life, [None, Some(3)]);
        assert!(eng.pending().is_empty());
    }

    #[test]
    fn typed_fields_parse_and_are_taken_once_when_the_step_takes_effect() {
        let s = parse(
            "scene=1 wait=2 bs=2 type=Ab-_9 enter=1\nscene=1 wait=1 type=x\nscene=1 wait=1\n",
        )
        .unwrap();
        let t = s.steps[0].typed.unwrap();
        assert_eq!((t.backspaces, t.text(), t.enter), (2, "Ab-_9", true));
        let units: Vec<u16> = (0..t.unit_count()).filter_map(|i| t.unit(i)).collect();
        assert_eq!(units, vec![0x08, 0x08, 0x41, 0x62, 0x2D, 0x5F, 0x39, 0x0D]);
        assert_eq!(t.unit(t.unit_count()), None);
        assert_eq!(s.steps[2].typed, None, "文字の欄が無い段");
        for (line, bs, text, enter) in [
            ("bs=3", 3u8, "", false),
            ("type=abc", 0, "abc", false),
            ("enter=1", 0, "", true),
        ] {
            let t = parse(&format!("scene=1 {line}\n")).unwrap().steps[0]
                .typed
                .unwrap();
            assert_eq!(
                (t.backspaces, t.text(), t.enter),
                (bs, text, enter),
                "{line}"
            );
        }
        let long = "a".repeat(TYPED_TEXT_MAX + 1);
        for bad in [
            "type=",
            "type=a.b",
            "type=a_b_c d",
            "type=部屋",
            "type=ＡＢ",
            &format!("type={long}"),
            "bs=0",
            "bs=33",
            "bs=x",
            "enter=0",
            "enter=yes",
        ] {
            assert!(parse(&format!("scene=1 {bad}\n")).is_err(), "{bad}");
        }
        assert!(parse(&format!("scene=1 type={}\n", "a".repeat(TYPED_TEXT_MAX))).is_ok());
        let mut eng = Engine::new(s);
        let o = obs_scene(1);
        eng.step(&o);
        assert_eq!(eng.take_typed(), None, "効き始める前に出た");
        eng.step(&o);
        assert_eq!(eng.take_typed().unwrap().text(), "Ab-_9");
        assert_eq!(eng.take_typed(), None, "2 回取り出せた");
        eng.step(&o);
        assert_eq!(eng.take_typed().unwrap().text(), "x");
        let mut eng = Engine::new(parse("scene=1 wait=1 type=zz\n").unwrap());
        eng.step(&o);
        eng.step(&o);
        assert_eq!(eng.status(), Status::Done);
        assert_eq!(eng.take_typed(), None, "台本が終わった後に出た");
    }

    #[test]
    fn digit_and_tab_fields_parse_and_are_taken_once_when_the_step_takes_effect() {
        let s = parse("scene=1 wait=2 digit=5 tab=1\nscene=1 wait=1 digit=9\nscene=1 wait=1 tab=1\nscene=1 wait=1\n")
            .unwrap();
        assert_eq!(
            s.steps[0].keys,
            Some(KeyPress {
                digit: Some(5),
                tab: true
            })
        );
        assert_eq!(
            s.steps[1].keys,
            Some(KeyPress {
                digit: Some(9),
                tab: false
            })
        );
        assert_eq!(
            s.steps[2].keys,
            Some(KeyPress {
                digit: None,
                tab: true
            })
        );
        assert_eq!(s.steps[3].keys, None, "押下の欄が無い段");
        for d in 0..=9u8 {
            let k = parse(&format!("scene=1 digit={d}\n")).unwrap().steps[0].keys;
            assert_eq!(k.and_then(|k| k.digit), Some(d), "digit={d}");
        }
        for bad in [
            "digit=10",
            "digit=",
            "digit=-1",
            "digit=x",
            "digit=０",
            "digit=0x1",
            "digit=+1",
            "tab=0",
            "tab=yes",
            "tab=",
        ] {
            assert!(parse(&format!("scene=1 {bad}\n")).is_err(), "{bad}");
        }
        let mut eng = Engine::new(s);
        let o = obs_scene(1);
        eng.step(&o);
        assert_eq!(eng.take_keys(), None, "効き始める前に出た");
        eng.step(&o);
        assert_eq!(
            eng.take_keys(),
            Some(KeyPress {
                digit: Some(5),
                tab: true
            })
        );
        assert_eq!(eng.take_keys(), None, "2 回取り出せた");
        eng.step(&o);
        assert_eq!(eng.take_keys().and_then(|k| k.digit), Some(9));
        eng.step(&o);
        let k = eng.take_keys().unwrap();
        assert_eq!((k.digit, k.tab), (None, true), "前の段の数字が残った");
        eng.step(&o);
        assert_eq!(eng.take_keys(), None, "押下を持たない段が前の押下を出した");
        let mut eng = Engine::new(parse("scene=1 wait=1 tab=1\n").unwrap());
        eng.step(&o);
        eng.step(&o);
        assert_eq!(eng.status(), Status::Done);
        assert_eq!(eng.take_keys(), None, "台本が終わった後に出た");
    }

    // `Engine::step` が確保しないことの検査は `src/core/tests/alloc_check.rs` に置く。割り当てを数えるには `unsafe impl GlobalAlloc`
    // が要るが、このクレートは `forbid(unsafe_code)` で、integration test は別クレートなので置ける。
}
