//! 実機の回の定義ファイル `scripts/runs/<回>.run` の読み方と、組（台本 × ini × peer_sim の引数 × ゲーム）の検査（pure）。
//!
//! 古い台本や引数の組み合わせで回を起こして止まるのを防ぐため、組を 1 つの定義にまとめ、今の決まりから言える組の誤りを
//! `cargo test`（`src/core/tests/runs_consistency.rs`）で落とす。
//!
//! ## 形
//!
//! 1 行 1 項目の `key = value`。`#` から行末までは注、空の行は読み飛ばす。知らないキー・同じキーの 2 回目・
//! 値の無い行は、行の番号を添えて断る（綴りの誤りで既定のまま走らないように）。
//!
//! | key | 要るか | 値 |
//! |---|---|---|
//! | `script` | 要る | 台本のパス（リポジトリの直下から。`scripts/` で始まる。`build/bin/script.txt` へコピーするもの）。台本を置かない回（人が操作する回など）は `none` |
//! | `ini` | 要る | DLL の ini のパス（同上。`build/bin/HiganRTool.ini` へコピーするもの） |
//! | `peer_sim` | ネットワークの回だけ | peer_sim の引数の列（空白で区切る。引用符は使えない）。無ければネットワークなし。観戦者の回（ini が `code_side = spectate`）では、対戦のホスト役の peer_sim |
//! | `peer_sim2` | 観戦者の回だけ | 観戦者の回で、対戦のゲスト役の peer_sim の引数の列（対戦は peer_sim の 2 つが行うため） |
//! | `game` | 要る | `a` / `b` / `c`（検査用のコピー `th09_a` / `th09_b` / `th09_c` の記号。パスそのものは書かない。`c` は観戦者が 3 台目のとき） |
//! | `second` | 無くてよい | 相手が本物の花映塚の 2 台の回: 相手の定義のパス `scripts/runs/<回>.run`（peer_sim の代わり）か `none`。相手の定義も自分を指し、ゲームのコピーと role が互いに違うこと |
//! | `watcher` | 無くてよい | 3 台の回のホストの定義だけ: 観戦者（本物の花映塚）の定義のパス。`second` と一緒に書き、ホストの ini に `[spectate] enable = 1` が要る。観戦者の定義は `watching` で自分を指す |
//! | `watching` | 無くてよい | 3 台の回の観戦者の定義だけ: 観戦するホストの定義のパス。ini は `code_side = spectate`（観戦用のコードをファイルで貼る形）か `code_side = game`（ゲーム内のメニューの「ルームで観戦」）。`peer_sim`・`second`・`role` は書かない。`watcher` と一緒には書けない |
//! | `limit_min` | 要る | 起動から何分で集計が出なければ止めるか（1〜60） |
//! | `purpose` | 要る | 何を見る回か（1 行） |
//! | `role` | 無くてよい | ネットワークの回での dll の役割 `host` / `guest`。書けば nonce の大小と合うかを見る |
//! | `allow_updown` | 無くてよい | マッチに入った後に上・下を押す段階のうち、マッチの終わりのメニューに届かないと分かっている段階の `note`（空白で区切る） |
//! | `choose` | 無くてよい | 接続コードのウィンドウ（ローダー）が開いた直後に、`0` `1` `2` `delay` と打ったのと同じ経路で対戦の形を選ぶ（ローダーの `--choose` に渡す）。ini が `code_side = offer / answer` の回だけ（ウィンドウが開かないと打つ先が無い）。`second` の相手の定義にも別に書ける |
//! | `own_clock` | 無くてよい | peer_sim を付いていかない形（自分の格子）で回す理由の 1 行。書けば `--follow` の決まりの代わりに `--segs 1` と「`--frames` が `limit_min` 分のフレーム数以上」を見る。台本はマッチの途中で終える（dll が BYE(終了) を出す） |
//!
//! パスはリポジトリの直下からの相対で、`/` で区切る。絶対パス・ドライブ名・`..`・`\` は断る（ゲームのフォルダを
//! 指す定義を作らないため）。peer_sim の `--menu` のパスも同じ（peer_sim はリポジトリの直下で起こす）。

#![forbid(unsafe_code)]

use crate::autosave;
use crate::ini::{self, Ini};
use crate::input_model::{SEND_MASK, keys};
use crate::lockstep::Role;
use crate::menu_model::{SCREEN_CHARA, SCREEN_DIFFICULTY, SCREEN_MAP, TRANSITION_BITS};
use crate::mode_pick::Pick;
use crate::script::{self, Script};
use crate::wire::Seat;
use std::fmt;
use std::net::SocketAddr;

/// 検査用のコピーの記号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Game {
    A,
    B,
    /// 3 台目（観戦者）。中身は a と同じコピー。
    C,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDef {
    /// 台本のパス。`None` なら台本を置かない回（`script = none`）。
    pub script: Option<String>,
    pub ini: String,
    /// peer_sim の引数の列。`None` ならネットワークなし。
    pub peer_sim: Option<Vec<String>>,
    /// 観戦者の回の 2 つ目の peer_sim（対戦のゲスト役。`peer_sim` がホスト役）。
    pub peer_sim2: Option<Vec<String>>,
    pub game: Game,
    pub limit_min: u32,
    pub purpose: String,
    pub role: Option<Role>,
    pub allow_updown: Vec<String>,
    /// 相手が本物の花映塚の 2 台の回なら、相手の定義のパス。`None` なら 1 台の回。
    pub second: Option<String>,
    /// 3 台の回のホストの定義だけ: 観戦者の定義のパス。
    pub watcher: Option<String>,
    /// 3 台の回の観戦者の定義だけ: 観戦するホストの定義のパス。
    pub watching: Option<String>,
    /// 接続コードのウィンドウが開いた直後に選ぶ対戦の形（ローダーの `--choose`）。
    pub choose: Option<Pick>,
    /// peer_sim を付いていかない形で回す理由。
    pub own_clock: Option<String>,
}

/// 定義が読めなかった理由（行の番号は 1 から。定義全体に関わる理由 —— 要るキーが無い —— は 0）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub key: String,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}行目 [{}]: {}", self.line, self.key, self.msg)
    }
}

const KEYS: [&str; 14] = [
    "script",
    "ini",
    "peer_sim",
    "peer_sim2",
    "game",
    "second",
    "watcher",
    "watching",
    "limit_min",
    "purpose",
    "role",
    "allow_updown",
    "choose",
    "own_clock",
];

/// `limit_min` の上限。dll の台本のある実行の計測の上限は 1200 秒なので、それより長く待っても集計は出ない。余裕を見て 60 分。
const LIMIT_MIN_MAX: u32 = 60;

fn perr(line: usize, key: &str, msg: impl Into<String>) -> ParseError {
    ParseError {
        line,
        key: key.to_string(),
        msg: msg.into(),
    }
}

/// リポジトリの直下からの相対パスか（`scripts/` で始まり、`..`・`\`・`:`・空の区切りが無い）。
fn repo_path_ok(p: &str) -> Result<(), String> {
    if !p.starts_with("scripts/") {
        return Err(format!(
            "パス {p:?} は scripts/ で始まる相対パスで書く（ゲームのフォルダや絶対パスは書かない）"
        ));
    }
    if p.contains('\\') || p.contains(':') {
        return Err(format!(
            "パス {p:?} に `\\` か `:` がある（`/` で区切る相対パスで書く）"
        ));
    }
    if p.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("パス {p:?} に空・`.`・`..` の区切りがある"));
    }
    Ok(())
}

pub fn parse(text: &str) -> Result<RunDef, ParseError> {
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut seen: Vec<(&str, usize, &str)> = Vec::new();
    for (i, raw) in body.lines().enumerate() {
        let line = i + 1;
        let content = match raw.find('#') {
            Some(p) => &raw[..p],
            None => raw,
        };
        let t = content.trim();
        if t.is_empty() {
            continue;
        }
        let (k, v) = t
            .split_once('=')
            .ok_or_else(|| perr(line, "", "`=` が無い（key = value の形で書く）"))?;
        let (k, v) = (k.trim(), v.trim());
        let Some(&key) = KEYS.iter().find(|&&x| x == k) else {
            return Err(perr(
                line,
                k,
                format!("知らないキー（読めるのは {}）", KEYS.join(" / ")),
            ));
        };
        if v.is_empty() {
            return Err(perr(line, key, "値が空"));
        }
        if let Some((_, first, _)) = seen.iter().find(|(x, _, _)| *x == key) {
            return Err(perr(line, key, format!("{first} 行目と 2 回書いている")));
        }
        seen.push((key, line, v));
    }
    let get = |k: &str| {
        seen.iter()
            .find(|(x, _, _)| *x == k)
            .map(|&(_, l, v)| (l, v))
    };
    let need = |k: &str| get(k).ok_or_else(|| perr(0, k, "要るキーが無い"));

    let (l, script) = need("script")?;
    let script = if script == "none" {
        None
    } else {
        repo_path_ok(script).map_err(|m| perr(l, "script", m))?;
        Some(script.to_string())
    };
    let (l, ini) = need("ini")?;
    repo_path_ok(ini).map_err(|m| perr(l, "ini", m))?;

    let args_of = |key: &'static str| match get(key) {
        None => Ok(None),
        Some((l, v)) => {
            if v.contains('"') || v.contains('\'') {
                return Err(perr(
                    l,
                    key,
                    "引用符は使えない（空白で区切るだけ。空白を含むパスは書かない）",
                ));
            }
            Ok(Some(v.split_whitespace().map(str::to_string).collect()))
        }
    };
    let peer_sim = args_of("peer_sim")?;
    let peer_sim2 = args_of("peer_sim2")?;
    if peer_sim2.is_some() && peer_sim.is_none() {
        let (l, _) = need("peer_sim2")?;
        return Err(perr(
            l,
            "peer_sim2",
            "peer_sim2 は peer_sim（対戦のホスト役）と一緒にだけ書ける",
        ));
    }

    let (l, g) = need("game")?;
    let game = match g {
        "a" => Game::A,
        "b" => Game::B,
        "c" => Game::C,
        _ => return Err(perr(l, "game", format!("値 {g:?} は a・b・c のどれか"))),
    };

    let second = match get("second") {
        None | Some((_, "none")) => None,
        Some((l, v)) => {
            repo_path_ok(v).map_err(|m| perr(l, "second", m))?;
            if !(v.starts_with("scripts/runs/") && v.ends_with(".run")) {
                return Err(perr(
                    l,
                    "second",
                    format!("値 {v:?} は相手の定義のパス（scripts/runs/<回>.run）か none"),
                ));
            }
            Some(v.to_string())
        }
    };

    // 3 台の回の指し合い。`none` は無い。パスの形は `second` と同じ。
    let run_path = |key: &'static str| match get(key) {
        None => Ok(None),
        Some((l, v)) => {
            repo_path_ok(v).map_err(|m| perr(l, key, m))?;
            if !(v.starts_with("scripts/runs/") && v.ends_with(".run")) {
                return Err(perr(
                    l,
                    key,
                    format!("値 {v:?} は相手の定義のパス（scripts/runs/<回>.run）"),
                ));
            }
            Ok(Some(v.to_string()))
        }
    };
    let watcher = run_path("watcher")?;
    let watching = run_path("watching")?;
    if watcher.is_some()
        && watching.is_some()
        && let Some((l, _)) = get("watching")
    {
        return Err(perr(
            l,
            "watching",
            "watcher（ホストの定義）と watching（観戦者の定義）は同じ定義に書けない",
        ));
    }

    let (l, lm) = need("limit_min")?;
    let limit_min = lm
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=LIMIT_MIN_MAX).contains(n))
        .ok_or_else(|| {
            perr(
                l,
                "limit_min",
                format!("値 {lm:?} は 1〜{LIMIT_MIN_MAX} の分"),
            )
        })?;

    let (_, purpose) = need("purpose")?;

    let role = match get("role") {
        None => None,
        Some((_, "host")) => Some(Role::Host),
        Some((_, "guest")) => Some(Role::Guest),
        Some((l, v)) => return Err(perr(l, "role", format!("値 {v:?} は host か guest"))),
    };

    let allow_updown = get("allow_updown")
        .map(|(_, v)| v.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    let choose = match get("choose") {
        None => None,
        Some((l, v)) => Some(Pick::parse(v).ok_or_else(|| {
            perr(
                l,
                "choose",
                format!("値 {v:?} は 0 / 1 / 2 / delay のどれか"),
            )
        })?),
    };

    Ok(RunDef {
        script,
        ini: ini.to_string(),
        peer_sim,
        peer_sim2,
        game,
        limit_min,
        purpose: purpose.to_string(),
        role,
        allow_updown,
        second,
        watcher,
        watching,
        choose,
        own_clock: get("own_clock").map(|(_, v)| v.to_string()),
    })
}

/// どの決まりに当たったか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// 参照先（台本・ini・`--menu` の選択の台本）が無い・読めない・形として読めない。
    Missing,
    NetPairing,
    PeerArgs,
    /// ネットワークの回に `--menu` が無い、か選択の台本にキャラの画面の決定が無い。
    Menu,
    /// ネットワークの回の台本が、選択の画面で 2P の席を押す（古い形）。
    OldSelectScript,
    Follow,
    /// peer_sim の `--mask` が無い、か画面を移るビット・上下を通す。
    Mask,
    Role,
    /// dll の席が 1P にならない（席を決める側の値が書かれていない場合を含む）。
    Seat,
    Address,
    Session,
    /// 秘密が揃わない（違う値、か ini に秘密が無いのに peer_sim が `--code` を使わない）。
    Secret,
    LatPlan,
    /// ネットワークの中の自動保存の回に `--life` が無い。
    Life,
    /// マッチに入った後に上・下を押す段階がある（マッチの終わりのメニューで「リプレイを保存して終了」を選びうる）。
    EndMenuUpDown,
    /// `allow_updown` に書いた `note` が、台本に無いか、上・下を押す段階でない。
    StaleAllowance,
    Limit,
    NotNet,
    /// `choose` を、ウィンドウが開かない回に書いた。打つ先が無く、選択が黙って捨てられる。
    Choose,
    /// 観戦者の回（ini が `code_side = spectate`）の組の誤り。peer_sim の組み方、ini の `after_script = keep` と `code_dir`、
    /// 観戦者の台本の Esc の規則（`watcher_common`）。
    Watch,
}

/// 組の誤り 1 つ。`key` は定義のどの項目か。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub rule: Rule,
    pub key: &'static str,
    pub msg: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {:?}: {}", self.key, self.rule, self.msg)
    }
}

fn read_def(
    other_path: &str,
    key: &'static str,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) -> Option<RunDef> {
    match read(other_path).map(|t| parse(&t)) {
        None => {
            out.push(prob(
                Rule::Missing,
                key,
                format!("{other_path} が無い・読めない"),
            ));
            None
        }
        Some(Err(e)) => {
            out.push(prob(
                Rule::Missing,
                key,
                format!("{other_path} が定義として読めない: {e:?}"),
            ));
            None
        }
        Some(Ok(o)) => Some(o),
    }
}

/// 2 台の回の相手の定義を見る: 読めて、相手も 2 台の回で peer_sim を持たず、自分（`path`。分かるときだけ）を指し返し、
/// ゲームのコピーが違い（同じコピーを 2 つ起こすと設定・リプレイの書き先を取り合う）、role が両方書かれて互いに違う。
fn check_second(
    path: Option<&str>,
    def: &RunDef,
    other_path: &str,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    let Some(other) = read_def(other_path, "second", read, out) else {
        return;
    };
    if other.second.is_none() || other.peer_sim.is_some() {
        out.push(prob(
            Rule::NetPairing,
            "second",
            format!("{other_path} が 2 台の回の形でない（second が無いか peer_sim がある）"),
        ));
    } else if let (Some(me), Some(back)) = (path, other.second.as_deref())
        && back != me
    {
        out.push(prob(
            Rule::NetPairing,
            "second",
            format!("{other_path} の second は {back} で、この定義 {me} を指していない"),
        ));
    }
    if other.game == def.game {
        out.push(prob(
            Rule::NetPairing,
            "second",
            format!("{other_path} と同じゲームのコピーを使う"),
        ));
    }
    match (def.role, other.role) {
        (Some(a), Some(b)) if a != b => {}
        _ => out.push(prob(
            Rule::Role,
            "role",
            format!("2 台の回は role を両方に書き、互いに違う値にする（{other_path}）"),
        )),
    }
}

fn prob(rule: Rule, key: &'static str, msg: impl Into<String>) -> Problem {
    Problem {
        rule,
        key,
        msg: msg.into(),
    }
}

/// 10 進か `0x` の 16 進（dll の `net_config.rs` と peer_sim の `cli.rs` の読み方と同じ形）。
fn num(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse::<u64>().ok(),
    }
}

/// peer_sim の引数の列を `--名前` ごとの値の列にしたもの。値を取らない旗は `--follow` だけ（`src/peer_sim/src/cli.rs` の `parse_at`）。
struct PeerArgs<'a> {
    pairs: Vec<(&'a str, &'a str)>,
    follow: bool,
}

impl<'a> PeerArgs<'a> {
    fn parse(args: &'a [String]) -> Result<Self, String> {
        let mut pairs = Vec::new();
        let mut follow = false;
        let mut it = args.iter();
        while let Some(k) = it.next() {
            let k = k.as_str();
            if k == "--follow" {
                follow = true;
                continue;
            }
            if k == "--help" || k == "-h" || k == "--compare" {
                return Err(format!("{k} は走らせる形ではない"));
            }
            if !k.starts_with("--") {
                return Err(format!("{k:?} は --名前 ではない"));
            }
            let v = it
                .next()
                .filter(|v| !v.starts_with("--"))
                .ok_or_else(|| format!("{k} の値が無い"))?;
            pairs.push((k, v.as_str()));
        }
        Ok(Self { pairs, follow })
    }

    fn get(&self, k: &str) -> Option<&'a str> {
        self.pairs.iter().find(|(x, _)| *x == k).map(|&(_, v)| v)
    }

    fn all(&self, k: &str) -> Vec<&'a str> {
        self.pairs
            .iter()
            .filter(|(x, _)| *x == k)
            .map(|&(_, v)| v)
            .collect()
    }
}

/// 台本が押す上・下。マッチの終わりのメニュー `0x004355F0` は合成 `[2]` の 0x10（上）で状態 3、0x20（下）で状態 2 へ動く。
/// 状態 3 は「リプレイを保存して終了」で、ゲームの `replay/` に書く。
const UPDOWN: u16 = keys::UP | keys::DOWN;

/// マッチに入った後に上・下を押し、マッチの終わりのメニューに届きうる段階の `note`（note が無ければ `行 N`）と行。「マッチに入った後」は、条件に `scene=2` を持つ最初の段階から後。
/// 条件で外せない限り危ないと見るが、`scene=1` を条件に持つ段階は外す（マッチの終わりのメニューは場面 2 の中）。`pause=1` を条件に持つ段階も外す（`pause=` は
/// `0x4A7ECC` の下位 1 バイトだけを読み、メニューが出ている間それは 0 だった（実測））。それ以外は、何秒目に押すかを台本だけからは縛れない
/// （条件を待つ間の長さは `limit` まで伸びうる）ので危ないと見る。マッチの始まりの数秒で押すと分かっている段階は、定義の `allow_updown` で名指しして外す。
fn updown_after_match(s: &Script) -> Vec<(String, u32)> {
    let Some(start) = s.steps.iter().position(|st| st.cond.scene == Some(2)) else {
        return Vec::new();
    };
    s.steps[start..]
        .iter()
        .filter(|st| (st.p1 | st.p2) & UPDOWN != 0)
        .filter(|st| st.cond.scene != Some(1) && st.cond.pause != Some(true))
        .map(|st| {
            let name = if st.note.is_empty() {
                format!("行 {}", st.line)
            } else {
                st.note.clone()
            };
            (name, st.line)
        })
        .collect()
}

/// 台本の長さの下限（秒）。どの段階も少なくとも wait（と、キーがあれば hold）の tick だけかかる。条件を待つ間・止める地点・
/// 読み込みの間は数えないので「下限」。1 tick は 60 fps の 1 フレーム。
fn script_min_seconds(s: &Script) -> u64 {
    let ticks: u64 = s
        .steps
        .iter()
        .map(|st| {
            let keyed = st.p1 != 0 || st.p2 != 0;
            u64::from(st.wait) + if keyed { u64::from(st.hold) } else { 0 }
        })
        .sum();
    ticks / 60
}

/// 選択の台本（peer_sim の `--menu`）にキャラの画面の決定があるか。core からは peer_sim の `menu_plan.rs` を呼べないので、「`screen=8` と `press=decide` が同じ行にある」ことだけを見る。
fn plan_decides_chara(plan: &str) -> bool {
    let chara = format!("screen={SCREEN_CHARA}");
    plan.lines().any(|raw| {
        let t = raw.split('#').next().unwrap_or("");
        let toks: Vec<&str> = t.split_whitespace().collect();
        toks.contains(&chara.as_str()) && toks.contains(&"press=decide")
    })
}

fn check_menu(
    pa: &PeerArgs,
    key: &'static str,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    match pa.get("--menu") {
        None => out.push(prob(
            Rule::Menu,
            key,
            "ネットワークの回に --menu が無い（選択の画面の 2P は peer_sim の選択の台本でしか決まらない。M2-3 から）",
        )),
        Some(p) => {
            if let Err(m) = repo_path_ok(p) {
                out.push(prob(Rule::Missing, key, format!("--menu の{m}")));
            } else {
                match read(p) {
                    None => out.push(prob(
                        Rule::Missing,
                        key,
                        format!("--menu の {p} が無い・読めない"),
                    )),
                    Some(t) if !plan_decides_chara(&t) => out.push(prob(
                        Rule::Menu,
                        key,
                        format!("--menu の {p} にキャラの画面の決定（screen={SCREEN_CHARA} の press=decide）が無い"),
                    )),
                    Some(_) => {}
                }
            }
        }
    }
}

fn check_mask(pa: &PeerArgs, key: &'static str, out: &mut Vec<Problem>) {
    let forbidden = (TRANSITION_BITS | UPDOWN) & SEND_MASK;
    match pa.get("--mask").map(|v| (v, num(v))) {
        None => out.push(prob(
            Rule::Mask,
            key,
            format!("--mask が無い（既定は全部のビットを送る。{forbidden:#06x} を落とした値にする —— 例 0xC4）"),
        )),
        Some((v, None)) => out.push(prob(Rule::Mask, key, format!("--mask の値 {v:?} が数として読めない"))),
        Some((_, Some(m))) if m & u64::from(forbidden) != 0 => out.push(prob(
            Rule::Mask,
            key,
            format!("--mask {m:#x} が画面を移るビットか上下（{forbidden:#06x}）を通す"),
        )),
        Some(_) => {}
    }
}

/// 観戦者の定義（ini が `code_side = spectate`）に共通の決まり:
/// - ini: ネットワークが有効・`after_script = keep`（無いと台本が終わって 10 秒で計測が終わり、`_watch_frames.csv` の書き出しが止まる）・`code_dir`
///   （3 台を同じ PC で起こすときにゲームごとに分ける）。
/// - 定義: `second`・`role` を書かない。
/// - 台本: 観戦者のゲームは、繋がった後の選択の画面（7・8・16）とマッチではネットワークの記録の列が入力になり、台本の入力ワードはゲームへ入らない
///   （Esc だけは観戦をやめる読みに効く。`net_game::NetGame::peek_local_input`）。観戦中に効く段階（条件に `scene=2`・`screen=7/8/16`・`loading`・
///   `pause`・`endmenu`・`menuflag`・`talk` のどれかを書いた段階と、その後ろの段階全部）は Esc だけ押してよい。それ以外のキーは feed されず黙って
///   捨てられ、押したつもりの画面が進まないので断る（Esc で観戦が終わった後ろの段階は、どのキーも押してよい）。それ以外の段階
///   （タイトルから画面 6 まで。繋がる前にゲームへ届く段階）で Esc を押すのは断る（タイトルの Esc はゲームの終了などに届く）。
fn watcher_common(
    def: &RunDef,
    ini: &Ini,
    script: Option<&Script>,
    net: bool,
    out: &mut Vec<Problem>,
) {
    let watch = |key: &'static str, msg: String| prob(Rule::Watch, key, msg);
    let net_get = |k: &str| ini.get("netplay", k);
    if !net {
        out.push(watch(
            "ini",
            format!(
                "{} は code_side = spectate なのに [netplay] enable = 1 が無い",
                def.ini
            ),
        ));
    }
    if net_get("after_script") != Some("keep") {
        out.push(watch(
            "ini",
            format!(
                "{} に after_script = keep が無い（台本が終わって 10 秒で計測が終わり、_watch_frames.csv の書き出しが止まる）",
                def.ini
            ),
        ));
    }
    if net_get("code_dir").is_none() {
        out.push(watch(
            "ini",
            format!(
                "{} に code_dir が無い（観戦用のコードを貼る置き場。DLL のフォルダの code を 3 台で取り合わない）",
                def.ini
            ),
        ));
    }
    if def.second.is_some() || def.role.is_some() {
        out.push(watch(
            "role",
            "観戦者の回に second・role は書けない（対戦の役割は対戦者の側で決まる）".to_string(),
        ));
    }

    watcher_script_rules(def, script, out);
}

fn watcher_script_rules(def: &RunDef, script: Option<&Script>, out: &mut Vec<Problem>) {
    let watch = |key: &'static str, msg: String| prob(Rule::Watch, key, msg);
    if let Some(s) = script {
        let select = [SCREEN_DIFFICULTY, SCREEN_CHARA, SCREEN_MAP].map(u32::from);
        // 観戦中に効く段階に入ったか。一度入ったら後ろの段階は条件が無くても観戦中とみなす
        // （`wait=1 p1=ESC` のように条件を書かない段階が `scene=2 wait=600` の直後に来る書き方を許すため）。
        let mut watching = false;
        let mut left = false;
        let name = def.script.as_deref().unwrap_or("none");
        for st in &s.steps {
            let c = &st.cond;
            watching |= c.scene == Some(2)
                || c.screen.is_some_and(|x| select.contains(&x))
                || c.loading.is_some()
                || c.pause.is_some()
                || c.end_menu.is_some()
                || c.menu_flag.is_some()
                || c.talk.is_some();
            let word = st.p1 | st.p2;
            if watching && !left && word & !keys::ESC != 0 {
                out.push(watch(
                    "script",
                    format!(
                        "{name} の {} 行目: 観戦中に効く段階（scene=2・screen=7/8/16 などの条件の段階と、その後ろの段階）が Esc 以外のキーを押す（観戦中は記録の列が入力になり、台本のワードは Esc 以外ゲームへ入らない。押しても捨てられる）",
                        st.line
                    ),
                ));
            }
            if !watching && word & keys::ESC != 0 {
                out.push(watch(
                    "script",
                    format!(
                        "{name} の {} 行目: 観戦中に効く段階でない段階（タイトルから画面 6 まで。繋がる前にゲームへ届く）が Esc を押す（観戦をやめる Esc は、scene=2 か screen=7/8/16 を条件にした段で押す）",
                        st.line
                    ),
                ));
            }
            if watching && word & keys::ESC != 0 {
                left = true;
            }
        }
    }
}

/// 観戦者の回（ini が `code_side = spectate`）の組を見る。本物の花映塚は観戦者、対戦は peer_sim のホスト役（`peer_sim`）とゲスト役（`peer_sim2`）。
/// 対戦の回の決まりは当てず、代わりに: ini と台本は `watcher_common` の規則。peer_sim の 2 つは、ホストだけが `--spectate-host`、`--segs 1`
/// （観戦者のゲームは自分では再戦を選べず、マッチの途中に次の seg の始まりが来ると流れが切れる）、`--follow` は付けない、ホストの nonce が大きい、
/// 受信ポートが互いを向く、`--wait-sec`（ホストは観戦者のゲームが画面 7 に着いた後で起こすゲストを待つ）、`--mask`・`--menu` は対戦の回と同じ、
/// session・秘密は書くなら 2 つで同じ。アドレスは、観戦者の ini の bind・観戦のポート・2 つの受信ポートが全部別。
fn check_watch(
    def: &RunDef,
    ini: &Ini,
    script: Option<&Script>,
    net: bool,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    watcher_common(def, ini, script, net, out);
    let watch = |key: &'static str, msg: String| prob(Rule::Watch, key, msg);
    let net_get = |k: &str| ini.get("netplay", k);

    let (Some(host_args), Some(guest_args)) = (&def.peer_sim, &def.peer_sim2) else {
        out.push(watch(
            "peer_sim2",
            "観戦者の回はホスト役の peer_sim と、ゲスト役の peer_sim2 の両方が要る".to_string(),
        ));
        return;
    };
    let mut parsed = Vec::new();
    for (key, args) in [("peer_sim", host_args), ("peer_sim2", guest_args)] {
        match PeerArgs::parse(args) {
            Ok(p) => parsed.push(p),
            Err(e) => out.push(prob(Rule::PeerArgs, key, e)),
        }
    }
    let [host, guest] = parsed.as_slice() else {
        return;
    };
    for (key, pa) in [("peer_sim", host), ("peer_sim2", guest)] {
        check_menu(pa, key, read, out);
        check_mask(pa, key, out);
        if pa.follow {
            out.push(watch(
                key,
                "観戦者の回に --follow は付けない（相手が peer_sim なので付いていく試合が無い。--segs と --frames で長さを決める）".to_string(),
            ));
        }
        if pa.get("--segs") != Some("1") {
            out.push(watch(
                key,
                format!(
                    "--segs 1 が要る（{:?}）。観戦者のゲームは自分では再戦を選べず、試合の途中に次の seg の始まりが来ると流れが切れる",
                    pa.get("--segs")
                ),
            ));
        }
        if pa.get("--code").is_some() {
            out.push(watch(
                key,
                "--code は書けない（対戦の相手は --peer で向き合う）".to_string(),
            ));
        }
    }
    if host.get("--spectate-host").is_none() {
        out.push(watch(
            "peer_sim",
            "ホスト役に --spectate-host が無い（観戦者が JOIN するポート）".to_string(),
        ));
    }
    if guest.get("--spectate-host").is_some() {
        out.push(watch(
            "peer_sim2",
            "ゲスト役に --spectate-host は書けない（ゲストになった走行では観戦者へ流さない）"
                .to_string(),
        ));
    }
    if host.get("--wait-sec").is_none() {
        out.push(watch(
            "peer_sim",
            "ホスト役に --wait-sec が無い（観戦者のゲームが画面 7 に着いた後で起こすゲストを、10 秒で諦めずに待つ）".to_string(),
        ));
    }
    match (host.get("--nonce").and_then(num), guest.get("--nonce").and_then(num)) {
        (Some(h), Some(g)) if h > g => {}
        _ => out.push(watch(
            "peer_sim",
            format!(
                "ホスト役の --nonce {:?} がゲスト役の --nonce {:?} より大きい数として読めない（観戦のポートを持つ側がホストになる）",
                host.get("--nonce"),
                guest.get("--nonce")
            ),
        )),
    }
    for k in ["--session", "--secret"] {
        let (h, g) = (host.get(k), guest.get(k));
        let same = match k {
            "--secret" => h.map(str::to_ascii_lowercase) == g.map(str::to_ascii_lowercase),
            _ => h.and_then(num) == g.and_then(num) && h.is_some() == g.is_some(),
        };
        if !same {
            out.push(watch(
                "peer_sim2",
                format!("{k} がホスト役 {h:?} とゲスト役 {g:?} で違う（書くなら両方に同じ値）"),
            ));
        }
    }
    let addr = |s: Option<&str>| s.and_then(|v| v.parse::<SocketAddr>().ok());
    let (hb, gb) = (addr(host.get("--bind")), addr(guest.get("--bind")));
    if hb.is_none()
        || hb != addr(guest.get("--peer"))
        || gb.is_none()
        || gb != addr(host.get("--peer"))
    {
        out.push(watch(
            "peer_sim2",
            format!(
                "ホスト役の --bind {:?} とゲスト役の --peer {:?}、ゲスト役の --bind {:?} とホスト役の --peer {:?} が向き合っていない",
                host.get("--bind"),
                guest.get("--peer"),
                guest.get("--bind"),
                host.get("--peer")
            ),
        ));
    }
    let taken = [
        ("ホスト役の --bind", hb),
        ("ゲスト役の --bind", gb),
        (
            "ホスト役の --spectate-host",
            addr(host.get("--spectate-host")),
        ),
        (
            "観戦者の ini の bind",
            net_get("bind").and_then(|v| v.parse::<SocketAddr>().ok()),
        ),
    ];
    for (i, (name_a, a)) in taken.iter().enumerate() {
        match a {
            None => out.push(watch("ini", format!("{name_a} がアドレスとして読めない"))),
            Some(a) => {
                for (name_b, b) in taken.iter().skip(i + 1) {
                    if *b == Some(*a) {
                        out.push(watch(
                            "ini",
                            format!("{name_a} と {name_b} が同じアドレス {a}（受信ソケットは全部別にする）"),
                        ));
                    }
                }
            }
        }
    }
}

fn read_ini_of(
    def: &RunDef,
    key: &'static str,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) -> Option<Ini> {
    match read(&def.ini).map(|t| ini::parse(&t)) {
        None => {
            out.push(prob(
                Rule::Missing,
                key,
                format!("{} が無い・読めない", def.ini),
            ));
            None
        }
        Some(Err(e)) => {
            out.push(prob(
                Rule::Missing,
                key,
                format!("{} が ini として読めない: {e}", def.ini),
            ));
            None
        }
        Some(Ok(i)) => Some(i),
    }
}

fn spectate_enabled(ini: &Ini) -> bool {
    ini.get("spectate", "enable") == Some("1")
}

fn same_dir(a: &str, b: &str) -> bool {
    let t = |s: &str| s.trim_end_matches(['/', '\\']).to_ascii_lowercase();
    t(a) == t(b)
}

/// 台本がルーム名の入力へ入れる文字（`type=`）の最後のもの。無ければ ini の `[netplay] room`。サーバーが小文字へそろえるので
/// 小文字にそろえる。どちらも無ければ `None`。
fn typed_room(script: Option<&Script>, ini: Option<&Ini>) -> Option<String> {
    let typed = script.and_then(|s| {
        s.steps
            .iter()
            .filter_map(|st| st.typed.as_ref())
            .map(|t| t.text())
            .rfind(|t| !t.is_empty())
            .map(str::to_string)
    });
    typed
        .or_else(|| ini.and_then(|i| i.get("netplay", "room").map(str::to_string)))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
}

/// ゲーム内のメニューで繋ぐ 3 台の回の ini の 3 台をまたぐ決まり（指し合い・役割・コピー・時間は `check_triple` が先に見た）: 3 台とも `code_side = game`
/// （観戦者の ini は `[spectate]` を開かない。対戦者は持ってよい）・`room_server` が同じ・台本が打つ（か ini の `room`）ルーム名が小文字にそろえて同じ
/// （観戦用のルームの名は対戦のルーム名から導くので、違えば会えない）・`code_dir` が別・`bind` が別・ホストの `nonce` がゲストより大きい。
// 引数は 3 つの定義とその ini。束ねる型を足しても読み手は増えない。
#[allow(clippy::too_many_arguments)]
fn check_game_inis(
    def: &RunDef,
    ini: &Ini,
    watch: &RunDef,
    guest: Option<&RunDef>,
    g_ini: Option<&Ini>,
    w_ini: Option<&Ini>,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    let watch_prob = |key: &'static str, msg: String| prob(Rule::Watch, key, msg);
    let net = |i: &Ini, k: &str| i.get("netplay", k).map(str::to_string);
    let (Some(g), Some(w)) = (g_ini, w_ini) else {
        return;
    };
    for (who, key, i) in [("ゲスト", "second", g), ("観戦者", "watcher", w)] {
        if i.get("netplay", "code_side") != Some("game") {
            out.push(watch_prob(
                key,
                format!("{who}の ini が code_side = game でない（3 台とも対戦と観戦の「ルームマッチ」を使う形）"),
            ));
        }
    }
    if spectate_enabled(w) {
        out.push(watch_prob(
            "watcher",
            "観戦者の ini に [spectate] enable = 1 がある（観戦のポートを開くのは対戦者だけ。観戦者は見る側）".to_string(),
        ));
    }
    let servers = [
        net(ini, "room_server"),
        net(g, "room_server"),
        net(w, "room_server"),
    ];
    if servers.iter().any(Option::is_none) || servers[0] != servers[1] || servers[0] != servers[2] {
        out.push(watch_prob(
            "ini",
            "3 台の room_server が同じでない（か無い。同じルームサーバーで会う）".to_string(),
        ));
    }
    let script_of = |d: &RunDef| -> Option<Script> {
        let t = read(d.script.as_deref()?)?;
        script::parse(&t).ok()
    };
    let host_script = script_of(def);
    let guest_script = guest.and_then(script_of);
    let watch_script = script_of(watch);
    let names = [
        typed_room(host_script.as_ref(), Some(ini)),
        typed_room(guest_script.as_ref(), Some(g)),
        typed_room(watch_script.as_ref(), Some(w)),
    ];
    if names.iter().any(Option::is_none) || names[0] != names[1] || names[0] != names[2] {
        out.push(watch_prob(
            "script",
            "3 台の台本が打つルーム名（台本の type=。無ければ ini の room）が、小文字にそろえて同じでない（か決まらない。\
             観戦用のルームの名は対戦のルーム名から導くので、違えば観戦者は配る側に会えない）"
                .to_string(),
        ));
    }
    let dirs = [net(ini, "code_dir"), net(g, "code_dir"), net(w, "code_dir")];
    if dirs.iter().any(Option::is_none) {
        out.push(watch_prob(
            "ini",
            "3 台の ini に code_dir が要る（ゲーム内のメニューが前に使ったルーム名を書く置き場。3 台で別にする）".to_string(),
        ));
    } else {
        for i in 0..3 {
            for j in (i + 1)..3 {
                if let (Some(a), Some(b)) = (&dirs[i], &dirs[j])
                    && same_dir(a, b)
                {
                    out.push(watch_prob(
                        "ini",
                        format!("3 台のうち 2 台の code_dir が同じ {a:?}（別にする。room_last.txt を取り合う）"),
                    ));
                }
            }
        }
    }
    let addr = |i: &Ini| net(i, "bind").and_then(|v| v.parse::<SocketAddr>().ok());
    match (addr(ini), addr(g), addr(w)) {
        (Some(a), Some(b), Some(c)) => {
            if a == b || a == c || b == c {
                out.push(prob(
                    Rule::Address,
                    "ini",
                    format!(
                        "3 台の bind が別のアドレスでない（{a} / {b} / {c}。受信ソケットは 3 台とも別にする）"
                    ),
                ));
            }
        }
        _ => out.push(prob(
            Rule::Address,
            "ini",
            "3 台の bind が全部アドレスとして読めない".to_string(),
        )),
    }
    match (
        net(ini, "nonce").and_then(|v| num(&v)),
        net(g, "nonce").and_then(|v| num(&v)),
    ) {
        (Some(h), Some(gn)) if h > gn => {}
        (h, gn) => out.push(prob(
            Rule::Role,
            "ini",
            format!("ホストの nonce {h:?} がゲストの nonce {gn:?} より大きい数として読めない"),
        )),
    }
}

/// 3 台の回のホストの定義（`watcher` を持つ側）の関係を見る。観戦者の定義の中身（ini・台本）は `check_real_watcher` が見るので、3 つの定義をまたぐ決まりだけ:
/// 指し合い（観戦者の `watching` がこの定義を指す）・役割（`watcher` を持つのはホスト）・コピーが 3 台とも別（同じコピーは設定・リプレイ・`th09.cfg` の書き先を取り合う）・
/// ini（ゲストに `[spectate]` が無い・観戦者が `code_side = spectate`・観戦用のコードの置き場が観戦者と別・ホストの nonce がゲストより大きい・session と秘密が同じ）・
/// アドレス（3 台の受信ポートが全部別。ホストとゲストは直に向き合うか、中継を挟む。観戦者は中継を通らない）・時間（`limit_min` が対戦者以上）。
fn check_triple(
    path: Option<&str>,
    def: &RunDef,
    ini: &Ini,
    watcher_path: &str,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    let Some(guest_path) = def.second.as_deref() else {
        return;
    };
    let Some(watch) = read_def(watcher_path, "watcher", read, out) else {
        return;
    };
    let guest = read_def(guest_path, "second", read, out);

    match (path, watch.watching.as_deref()) {
        (_, None) => out.push(prob(
            Rule::NetPairing,
            "watcher",
            format!("{watcher_path} は観戦者の定義でない（watching が無い）"),
        )),
        (Some(me), Some(w)) if w != me => out.push(prob(
            Rule::NetPairing,
            "watcher",
            format!("{watcher_path} の watching は {w} で、この定義 {me} を指していない"),
        )),
        _ => {}
    }
    if def.role != Some(Role::Host) {
        out.push(prob(
            Rule::Role,
            "role",
            "watcher を持つのはホスト（role = host）。観戦のポートはホストにだけ開く",
        ));
    }
    if watch.game == def.game || guest.as_ref().is_some_and(|g| g.game == watch.game) {
        out.push(prob(
            Rule::NetPairing,
            "watcher",
            format!(
                "{watcher_path} が対戦者のどちらかと同じゲームのコピーを使う（3 台とも別にする）"
            ),
        ));
    }
    if watch.limit_min < def.limit_min {
        out.push(prob(
            Rule::Limit,
            "limit_min",
            format!(
                "観戦者の limit_min {} がホストの {} より短い（観戦者は対戦者より遅れて終わる）",
                watch.limit_min, def.limit_min
            ),
        ));
    }

    let g_ini = guest
        .as_ref()
        .and_then(|g| read_ini_of(g, "second", read, out));
    let w_ini = read_ini_of(&watch, "watcher", read, out);
    if ini.get("netplay", "code_side") == Some("game") {
        check_game_inis(
            def,
            ini,
            &watch,
            guest.as_ref(),
            g_ini.as_ref(),
            w_ini.as_ref(),
            read,
            out,
        );
        return;
    }
    if let Some(g) = &g_ini
        && spectate_enabled(g)
    {
        out.push(prob(
            Rule::Watch,
            "second",
            "ゲストの ini に [spectate] enable = 1 がある（観戦のポートはホストだけが開く）",
        ));
    }
    if let Some(w) = &w_ini
        && w.get("netplay", "code_side") != Some("spectate")
    {
        out.push(prob(
            Rule::Watch,
            "watcher",
            format!("観戦者の ini {} が code_side = spectate でない", watch.ini),
        ));
    }
    match (
        ini.get("spectate", "code_dir"),
        w_ini.as_ref().and_then(|w| w.get("netplay", "code_dir")),
    ) {
        (None, _) => out.push(prob(
            Rule::Watch,
            "ini",
            format!(
                "{} の [spectate] に code_dir が無い（観戦用のコードの置き場。観戦者と別の場所を決めておく）",
                def.ini
            ),
        )),
        (Some(h), Some(w)) if same_dir(h, w) => out.push(prob(
            Rule::Watch,
            "ini",
            format!("ホストの観戦用のコードの置き場と観戦者の code_dir が同じ {h:?}（別にする）"),
        )),
        _ => {}
    }

    let Some(g) = &g_ini else {
        return;
    };
    fn net<'a>(i: &'a Ini, k: &str) -> Option<&'a str> {
        i.get("netplay", k)
    }
    let coded = net(ini, "code_side").is_some() || net(g, "code_side").is_some();
    if coded {
        if net(ini, "code_side") != Some("offer") || net(g, "code_side") != Some("answer") {
            out.push(prob(
                Rule::Role,
                "ini",
                "接続コードで受け渡す 3 台の回は、ホストが code_side = offer、ゲストが answer",
            ));
        }
    } else {
        for k in ["session", "secret"] {
            let (h, gv) = (net(ini, k), net(g, k));
            let same = match (h, gv) {
                (Some(a), Some(b)) => {
                    if k == "secret" {
                        a.eq_ignore_ascii_case(b)
                    } else {
                        num(a).is_some() && num(a) == num(b)
                    }
                }
                _ => false,
            };
            if !same {
                out.push(prob(
                    if k == "secret" {
                        Rule::Secret
                    } else {
                        Rule::Session
                    },
                    "second",
                    format!(
                        "ホスト {h:?} とゲスト {gv:?} の {k} が違うか無い（対戦者が繋がらない）"
                    ),
                ));
            }
        }
        let addr = |i: &Ini, k: &str| net(i, k).and_then(|v| v.parse::<SocketAddr>().ok());
        let w = w_ini.as_ref();
        let [Some(hb), Some(hp), Some(gb), Some(gp), Some(wb)] = [
            addr(ini, "bind"),
            addr(ini, "peer"),
            addr(g, "bind"),
            addr(g, "peer"),
            w.and_then(|w| addr(w, "bind")),
        ] else {
            out.push(prob(
                Rule::Address,
                "ini",
                "3 台の bind と、ホスト・ゲストの peer が全部アドレスとして読めない".to_string(),
            ));
            return;
        };
        let binds = [("ホスト", hb), ("ゲスト", gb), ("観戦者", wb)];
        for (i, (na, a)) in binds.iter().enumerate() {
            for (nb, b) in binds.iter().skip(i + 1) {
                if a == b {
                    out.push(prob(
                        Rule::Address,
                        "ini",
                        format!(
                            "{na}の bind と{nb}の bind が同じアドレス {a}（受信ソケットは 3 台とも別にする）"
                        ),
                    ));
                }
            }
        }
        if hp == gb {
            if gp != hb {
                out.push(prob(
                    Rule::Address,
                    "ini",
                    format!("ホストの peer が ゲストの bind {gb} なのに、ゲストの peer {gp} がホストの bind {hb} でない"),
                ));
            }
        } else {
            if hp == gp {
                out.push(prob(
                    Rule::Address,
                    "ini",
                    format!("中継を挟む形なのにホストとゲストの peer が同じ {hp}（中継の A 向けのポートと B 向けのポートは別）"),
                ));
            }
            for (name, p) in [("ホスト", hp), ("ゲスト", gp)] {
                if let Some((nb, _)) = binds.iter().find(|(_, b)| *b == p) {
                    out.push(prob(
                        Rule::Address,
                        "ini",
                        format!("{name}の peer {p} が{nb}の bind と同じ（中継のポートは 3 台の受信ソケットと別にする）"),
                    ));
                }
            }
        }
    }
    match (
        net(ini, "nonce").and_then(num),
        net(g, "nonce").and_then(num),
    ) {
        (Some(h), Some(gn)) if h > gn => {}
        (h, gn) => out.push(prob(
            Rule::Role,
            "ini",
            format!("ホストの nonce {h:?} がゲストの nonce {gn:?} より大きい数として読めない"),
        )),
    }
}

/// 3 台の回の観戦者の定義（`watching` を持つ。ini が `code_side = game`）を見る。`check_real_watcher` との違いは、観戦用のコードをファイルで貼らないこと
/// （`after_script = keep` も観戦用の `code_dir` も要らない）。要るのは、ネットワーク・`room_server`・ゲーム内のメニューが書く `code_dir`（3 台で別）。
fn check_game_watcher(
    path: Option<&str>,
    def: &RunDef,
    ini: &Ini,
    script: Option<&Script>,
    net: bool,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    let watch = |key: &'static str, msg: String| prob(Rule::Watch, key, msg);
    if !net {
        out.push(watch(
            "ini",
            format!(
                "{} は code_side = game の観戦者なのに [netplay] enable = 1 が無い",
                def.ini
            ),
        ));
    }
    for key in ["room_server", "code_dir"] {
        if ini.get("netplay", key).is_none() {
            out.push(watch(
                "ini",
                format!(
                    "{} に {key} が無い（観戦の「ルームマッチ」はルームサーバーへ繋ぎ、置き場は 3 台で別にする）",
                    def.ini
                ),
            ));
        }
    }
    if spectate_enabled(ini) {
        out.push(watch(
            "ini",
            format!(
                "{} に [spectate] enable = 1 がある（観戦のポートはホストだけが開く。観戦者は見る側）",
                def.ini
            ),
        ));
    }
    if def.second.is_some() || def.role.is_some() {
        out.push(watch(
            "role",
            "観戦者の回に second・role は書けない（対戦の役割は対戦者の側で決まる）".to_string(),
        ));
    }
    if def.peer_sim.is_some() || def.peer_sim2.is_some() {
        out.push(prob(
            Rule::Watch,
            "peer_sim",
            "watching の定義に peer_sim は書けない（対戦者は本物の花映塚 2 台）",
        ));
    }
    watcher_script_rules(def, script, out);
    check_watching_target(path, def, read, out);
}

/// 3 台の回の観戦者の定義（`watching` を持つ。ini が `code_side = spectate`）を見る。共通の決まりは `watcher_common`。
/// ここは: peer_sim を書かない（対戦者は本物の花映塚 2 台）、指す先がホストの定義で、ゲームのコピーが違う。
fn check_real_watcher(
    path: Option<&str>,
    def: &RunDef,
    ini: &Ini,
    script: Option<&Script>,
    net: bool,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    watcher_common(def, ini, script, net, out);
    if def.peer_sim.is_some() || def.peer_sim2.is_some() {
        out.push(prob(
            Rule::Watch,
            "peer_sim",
            "watching の定義に peer_sim は書けない（対戦者は本物の花映塚 2 台。peer_sim の 2 つの形は peer_sim2 の回）",
        ));
    }
    check_watching_target(path, def, read, out);
}

fn check_watching_target(
    path: Option<&str>,
    def: &RunDef,
    read: &dyn Fn(&str) -> Option<String>,
    out: &mut Vec<Problem>,
) {
    let Some(host_path) = def.watching.as_deref() else {
        return;
    };
    let Some(host) = read_def(host_path, "watching", read, out) else {
        return;
    };
    match (path, host.watcher.as_deref()) {
        (_, None) => out.push(prob(
            Rule::NetPairing,
            "watching",
            format!("{host_path} に watcher が無い（観戦者を持つホストの定義でない）"),
        )),
        (Some(me), Some(w)) if w != me => out.push(prob(
            Rule::NetPairing,
            "watching",
            format!("{host_path} の watcher は {w} で、この定義 {me} を指していない"),
        )),
        _ => {}
    }
    if host.second.is_none() || host.role != Some(Role::Host) {
        out.push(prob(
            Rule::NetPairing,
            "watching",
            format!(
                "{host_path} が 3 台の回のホストの形でない（second が無いか role = host でない）"
            ),
        ));
    }
    if host.game == def.game {
        out.push(prob(
            Rule::NetPairing,
            "watching",
            format!("{host_path} と同じゲームのコピーを使う"),
        ));
    }
}

/// 組を検査する（自分の定義のパスを知らない形。別の定義から自分へ指し返されているかは見ない）。中身は `check_at(None, …)`。
pub fn check(def: &RunDef, read: &dyn Fn(&str) -> Option<String>) -> Vec<Problem> {
    check_at(None, def, read)
}

/// 組を検査する。`path` はこの定義のパス（分かるときだけ）で、`second` / `watcher` / `watching` の先がこの定義を指し返して
/// いるかを見るのに使う。`read` はリポジトリの直下からの相対パスで中身を返す（無い・読めなければ `None`）。誤りが無ければ空の列。
/// 1 つの誤りで止めずに、見られる所は全部見る。
pub fn check_at(
    path: Option<&str>,
    def: &RunDef,
    read: &dyn Fn(&str) -> Option<String>,
) -> Vec<Problem> {
    let mut out = Vec::new();

    let script_path = def.script.as_deref().unwrap_or("none");
    let script = match def.script.as_deref().map(|p| (p, read(p))) {
        None => None,
        Some((p, None)) => {
            out.push(prob(
                Rule::Missing,
                "script",
                format!("{p} が無い・読めない"),
            ));
            None
        }
        Some((p, Some(t))) => match script::parse(&t) {
            Ok(s) => Some(s),
            Err(e) => {
                out.push(prob(
                    Rule::Missing,
                    "script",
                    format!("{p} が台本として読めない: {e}"),
                ));
                None
            }
        },
    };
    let ini: Option<Ini> = match read(&def.ini) {
        None => {
            out.push(prob(
                Rule::Missing,
                "ini",
                format!("{} が無い・読めない", def.ini),
            ));
            None
        }
        Some(t) => match ini::parse(&t) {
            Ok(i) => Some(i),
            Err(e) => {
                out.push(prob(
                    Rule::Missing,
                    "ini",
                    format!("{} が ini として読めない: {e}", def.ini),
                ));
                None
            }
        },
    };

    if let Some(s) = &script {
        let risky = updown_after_match(s);
        for (name, line) in &risky {
            if !def.allow_updown.iter().any(|a| a == name) {
                out.push(prob(
                    Rule::EndMenuUpDown,
                    "script",
                    format!(
                        "{} の {line} 行目（{name}）: 試合に入った後に上・下を押す。試合の終わりのメニューで「リプレイを保存して終了」を選びうる（試合の始まりの数秒と分かっているなら allow_updown に名指しする）",
                        script_path
                    ),
                ));
            }
        }
        for a in &def.allow_updown {
            if !risky.iter().any(|(n, _)| n == a) {
                out.push(prob(
                    Rule::StaleAllowance,
                    "allow_updown",
                    format!(
                        "{a:?} は {} の中で、試合に入った後に上・下を押す段階ではない（消すか直す）",
                        script_path
                    ),
                ));
            }
        }
        let min_s = script_min_seconds(s);
        if u64::from(def.limit_min) * 60 <= min_s {
            out.push(prob(
                Rule::Limit,
                "limit_min",
                format!(
                    "{} 分は台本の長さの下限 {min_s} 秒（{}）に届かない。集計が出る前に止めることになる",
                    def.limit_min, script_path
                ),
            ));
        }
    }

    let Some(ini) = ini else {
        return out;
    };

    let net = match ini.get("netplay", "enable") {
        Some("1") => true,
        Some("0") | None => false,
        Some(v) => {
            out.push(prob(
                Rule::Missing,
                "ini",
                format!("[netplay] の enable の値 {v:?} は 0 か 1"),
            ));
            false
        }
    };
    let replay = match autosave::settings_from_ini(&ini) {
        Ok(r) => r.is_some(),
        Err(e) => {
            out.push(prob(
                Rule::Missing,
                "ini",
                format!("{} の [replay] が読めない: {e}", def.ini),
            ));
            false
        }
    };

    if def.choose.is_some()
        && !(net && matches!(ini.get("netplay", "code_side"), Some("offer" | "answer")))
    {
        out.push(prob(
            Rule::Choose,
            "choose",
            format!(
                "choose は接続コードのウィンドウが開く回（{} が [netplay] enable = 1 かつ code_side = offer か answer）だけに書ける",
                def.ini
            ),
        ));
    }

    if def.watching.is_some() && ini.get("netplay", "code_side") == Some("game") {
        check_game_watcher(path, def, &ini, script.as_ref(), net, read, &mut out);
        return out;
    }
    // 観戦者の回（本物の花映塚が観戦者、対戦は peer_sim の 2 つ）。対戦の回の決まりは当てず、この形の決まりを当てる。
    // `peer_sim2` は観戦者の回だけの項目なので、それ以外の回に書けば断る。
    if ini.get("netplay", "code_side") == Some("spectate") {
        if def.watching.is_some() {
            check_real_watcher(path, def, &ini, script.as_ref(), net, read, &mut out);
        } else {
            check_watch(def, &ini, script.as_ref(), net, read, &mut out);
        }
        return out;
    }
    if def.peer_sim2.is_some() {
        out.push(prob(
            Rule::Watch,
            "peer_sim2",
            format!(
                "peer_sim2 は観戦者の回（{} が code_side = spectate）だけの項目",
                def.ini
            ),
        ));
    }
    if def.watching.is_some() {
        out.push(prob(
            Rule::Watch,
            "watching",
            format!(
                "watching は観戦者の定義（{} が code_side = spectate）だけの項目",
                def.ini
            ),
        ));
        return out;
    }

    // 2 台の回（相手は本物の花映塚。peer_sim の代わりに相手の定義）。peer_sim の引数の決まりは当てない。
    if let Some(other) = &def.second {
        if !net || def.peer_sim.is_some() {
            out.push(prob(
                Rule::NetPairing,
                "second",
                format!(
                    "2 台の回はネットワーク（[netplay] enable = 1）で peer_sim を書かない（{} のネットワーク {net}・peer_sim {}）",
                    def.ini,
                    if def.peer_sim.is_some() { "あり" } else { "なし" }
                ),
            ));
            return out;
        }
        check_second(path, def, other, read, &mut out);
        // 観戦のポートはホストの定義（`watcher` を持つ）の ini にだけ開く。それ以外が開くと、観戦者を起こさない回でも
        // 観戦のポートだけ開いた形になり、対戦者の stall の測りが観戦者の有無で違ってしまう。
        match (spectate_enabled(&ini), def.watcher.as_deref()) {
            // `code_side = game` は、配る zip の形の ini（対戦者 2 人が `[spectate]` を持つ）のまま観戦者を起こさない対照と
            // ゲスト自身の定義があるので、watcher が無くてもよい。
            (true, None) if ini.get("netplay", "code_side") == Some("game") => {}
            (true, None) => out.push(prob(
                Rule::Watch,
                "ini",
                format!(
                    "{} が [spectate] enable = 1 なのに watcher が無い（観戦者の定義を指す）",
                    def.ini
                ),
            )),
            (false, Some(_)) => out.push(prob(
                Rule::Watch,
                "ini",
                format!(
                    "watcher があるのに {} に [spectate] enable = 1 が無い",
                    def.ini
                ),
            )),
            (true, Some(w)) => check_triple(path, def, &ini, w, read, &mut out),
            (false, None) => {}
        }
        return out;
    }
    if def.watcher.is_some() {
        out.push(prob(
            Rule::Watch,
            "watcher",
            "watcher は 2 台の回（second。本物の花映塚のゲスト）のホストの定義だけに書ける",
        ));
        return out;
    }

    // ゲーム内のメニュー（`code_side = game`）で、相手の無い回（入口の検査: 「妖怪 対 妖怪」の決定を漏らさないか）。
    // ネットワークはメニューの最初の「作る」「貼る」まで開かないので、peer_sim も second も要らない。role はネットワークが動く回の決まりなので書けない。
    if net
        && ini.get("netplay", "code_side") == Some("game")
        && def.peer_sim.is_none()
        && def.second.is_none()
    {
        if def.role.is_some() {
            out.push(prob(
                Rule::NotNet,
                "role",
                "相手の無いゲーム内のメニューの回（ネットワークを開かない）に role を書いた",
            ));
        }
        return out;
    }

    let args = match (&def.peer_sim, net) {
        (None, false) => {
            if def.role.is_some() {
                out.push(prob(
                    Rule::NotNet,
                    "role",
                    "ネットワークでない回（peer_sim が無い）に role を書いた",
                ));
            }
            return out;
        }
        (None, true) => {
            out.push(prob(
                Rule::NetPairing,
                "peer_sim",
                format!(
                    "{} はネットワーク（[netplay] enable = 1）なのに peer_sim が無い",
                    def.ini
                ),
            ));
            return out;
        }
        (Some(_), false) => {
            out.push(prob(
                Rule::NetPairing,
                "peer_sim",
                format!(
                    "{} はネットワークでない（[netplay] enable = 1 が無い）のに peer_sim がある",
                    def.ini
                ),
            ));
            return out;
        }
        (Some(a), true) => a,
    };
    let pa = match PeerArgs::parse(args) {
        Ok(p) => p,
        Err(e) => {
            out.push(prob(Rule::PeerArgs, "peer_sim", e));
            return out;
        }
    };
    let net_get = |k: &str| ini.get("netplay", k);

    // ③ 選択の 2P は peer_sim が決める。選択の画面では dll は自分の席の入力ワードしか使わず、台本の `p2=` は捨てられるので、
    // 2P のキャラの決定は peer_sim の選択の台本（`--menu`）からしか入らない。無ければ 2P が決まらずキャラ選択で止まる。
    check_menu(&pa, "peer_sim", read, &mut out);
    if let Some(s) = &script {
        let select = [SCREEN_DIFFICULTY, SCREEN_CHARA, SCREEN_MAP].map(u32::from);
        for st in &s.steps {
            if st.p2 != 0
                && st.cond.scene == Some(1)
                && st.cond.screen.is_some_and(|x| select.contains(&x))
            {
                out.push(prob(
                    Rule::OldSelectScript,
                    "script",
                    format!(
                        "{} の {} 行目: 選択の画面で 2P の席を押す（M2-2 の形の台本。ネットワークの回では使われない —— M2-3 の台本を使う）",
                        script_path, st.line
                    ),
                ));
            }
        }
    }

    // マッチの長さは花映塚が決めるので、peer_sim は付いていく。例外は `own_clock`（peer_sim を自分の格子で回す回）で、
    // peer_sim が先に seg を終えない長さ（`--segs 1`・`--frames` が `limit_min` 分以上）を要る。
    match (&def.own_clock, pa.follow) {
        (None, false) => out.push(prob(
            Rule::Follow,
            "peer_sim",
            "ネットワークの回に --follow が無い（試合の長さは花映塚が決める）",
        )),
        (Some(_), true) => out.push(prob(
            Rule::Follow,
            "peer_sim",
            "own_clock の回に --follow がある（付いていく形は自分の格子で回らない）",
        )),
        (Some(_), false) => {
            if pa.get("--segs") != Some("1") {
                out.push(prob(
                    Rule::Follow,
                    "peer_sim",
                    format!(
                        "own_clock の回は --segs 1 が要る（{:?}）。試合の後の再戦に付いていけない",
                        pa.get("--segs")
                    ),
                ));
            }
            let need = u64::from(def.limit_min) * 60 * 60;
            let frames = pa.get("--frames").and_then(|v| v.parse::<u64>().ok());
            if frames.is_none_or(|f| f < need) {
                out.push(prob(
                    Rule::Follow,
                    "peer_sim",
                    format!(
                        "own_clock の回の --frames（{:?}）が limit_min {} 分のフレーム数 {need} より少ない（peer_sim が花映塚より先に seg を終える）",
                        pa.get("--frames"),
                        def.limit_min
                    ),
                ));
            }
        }
        (None, true) => {}
    }

    check_mask(&pa, "peer_sim", &mut out);

    // ④ 役割（role_nonce の大きい方がホスト —— `handshake.rs` の on_hello）。どちらも書かれていなければ起動のたびに乱数で決まり、
    // 席も lat_plan も読めないので、ネットワークの回は両方を書く。
    let dll_nonce = net_get("nonce").map(|v| (v, num(v)));
    let peer_nonce = pa.get("--nonce").map(|v| (v, num(v)));
    let role = match (dll_nonce, peer_nonce) {
        (Some((_, Some(d))), Some((_, Some(p)))) if d == p => {
            out.push(prob(
                Rule::Role,
                "peer_sim",
                format!("ini の nonce と --nonce が同じ {d:#x}（役割が決まらない）"),
            ));
            None
        }
        (Some((_, Some(d))), Some((_, Some(p)))) => {
            Some(if d > p { Role::Host } else { Role::Guest })
        }
        (d, p) => {
            if !matches!(d, Some((_, Some(_)))) {
                out.push(prob(
                    Rule::Role,
                    "ini",
                    format!(
                        "{} の [netplay] に読める nonce が無い（役割が起動ごとに変わる）",
                        def.ini
                    ),
                ));
            }
            if !matches!(p, Some((_, Some(_)))) {
                out.push(prob(
                    Rule::Role,
                    "peer_sim",
                    "読める --nonce が無い（役割が起動ごとに変わる）",
                ));
            }
            None
        }
    };
    if let (Some(want), Some(got)) = (def.role, role)
        && want != got
    {
        out.push(prob(
            Rule::Role,
            "role",
            format!("role は {want:?} だが、ini の nonce と --nonce の大小では dll は {got:?}"),
        ));
    }

    // 席: 台本は dll が 1P の形で書いてある。席はホストの側の値で決まる（`handshake.rs` の Terms の host_seat）。
    // 既定に頼ると読めないので、決める側の値を書く。
    if let Some(role) = role {
        let dll_seat = match role {
            Role::Host => match net_get("host_seat") {
                Some("1") => Some(Seat::P1),
                Some("2") => Some(Seat::P2),
                _ => {
                    out.push(prob(
                        Rule::Seat,
                        "ini",
                        format!(
                            "dll がホストなのに {} の [netplay] に host_seat（1 か 2）が無い",
                            def.ini
                        ),
                    ));
                    None
                }
            },
            Role::Guest => match pa.get("--host-seat") {
                Some("2") => Some(Seat::P1),
                Some("1") => Some(Seat::P2),
                _ => {
                    out.push(prob(
                        Rule::Seat,
                        "peer_sim",
                        "peer_sim がホストなのに --host-seat（1 か 2）が無い",
                    ));
                    None
                }
            },
        };
        if dll_seat == Some(Seat::P2) {
            out.push(prob(
                Rule::Seat,
                if role == Role::Host {
                    "ini"
                } else {
                    "peer_sim"
                },
                "dll が 2P になる（台本は dll が 1P の形）",
            ));
        }

        // latency の切替はホストだけが出せる（`lockstep::LatError::NotHost`）。ini の lat_plan は dll がホストのとき、
        // peer_sim の --lat は peer_sim がホストのとき。
        if role == Role::Guest && net_get("lat_plan").is_some() {
            out.push(prob(
                Rule::LatPlan,
                "ini",
                format!(
                    "dll はゲストなのに {} に lat_plan がある（出せない）",
                    def.ini
                ),
            ));
        }
        if role == Role::Host && pa.get("--lat").is_some() {
            out.push(prob(
                Rule::LatPlan,
                "peer_sim",
                "peer_sim はゲストなのに --lat がある（出せない）",
            ));
        }
    }

    let addr = |s: &str| s.parse::<SocketAddr>().ok();
    let ini_bind = net_get("bind").and_then(addr);
    let ini_peers: Vec<SocketAddr> = net_get("peer")
        .map(|v| v.split_whitespace().filter_map(addr).collect())
        .unwrap_or_default();
    let sim_bind = pa.get("--bind").and_then(addr);
    let sim_peers: Vec<SocketAddr> = pa.all("--peer").into_iter().filter_map(addr).collect();
    let code = pa.get("--code").is_some();
    match sim_bind {
        Some(b) if ini_peers.contains(&b) => {}
        _ => out.push(prob(
            Rule::Address,
            "peer_sim",
            format!(
                "--bind {:?} が {} の peer {:?} に無い",
                pa.get("--bind"),
                def.ini,
                net_get("peer")
            ),
        )),
    }
    if !code {
        match ini_bind {
            Some(b) if sim_peers.contains(&b) => {}
            _ => out.push(prob(
                Rule::Address,
                "peer_sim",
                format!(
                    "{} の bind {:?} が --peer {:?} に無い",
                    def.ini,
                    net_get("bind"),
                    pa.all("--peer")
                ),
            )),
        }
    }

    // session と秘密。peer_sim が値を書いたときだけ比べる（peer_sim の既定の値は `cli.rs` にだけ置く）。
    if let Some(v) = pa.get("--session") {
        let ini_s = net_get("session").and_then(num);
        if num(v).is_none() || num(v) != ini_s {
            out.push(prob(
                Rule::Session,
                "peer_sim",
                format!(
                    "--session {v} と {} の session {:?} が違う",
                    def.ini,
                    net_get("session")
                ),
            ));
        }
    }
    match (net_get("secret"), pa.get("--secret")) {
        (None, _) if !code => out.push(prob(
            Rule::Secret,
            "ini",
            format!(
                "{} に secret が無い（dll が乱数で引く）のに peer_sim が --code を使わない",
                def.ini
            ),
        )),
        (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b) => {
            out.push(prob(
                Rule::Secret,
                "peer_sim",
                format!("--secret と {} の secret が違う", def.ini),
            ));
        }
        _ => {}
    }

    // ⑥ ネットワークの中の自動保存の回は、マッチを終わらせてマッチの終わりのメニューで保存を呼ぶのが目的。2P の初期体力を下げる --life を付ける。
    if replay && pa.get("--life").is_none() {
        out.push(prob(
            Rule::Life,
            "peer_sim",
            format!(
                "{} はネットワーク対戦の中の自動保存（[replay] enable = 1）なのに --life が無い",
                def.ini
            ),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str =
        "script = scripts/a.txt\nini = scripts/a.ini\ngame = a\nlimit_min = 10\npurpose = 見る\n";

    #[test]
    fn parse_minimal() {
        let d = parse(MIN).expect("parse");
        assert_eq!(d.script.as_deref(), Some("scripts/a.txt"));
        assert_eq!(d.ini, "scripts/a.ini");
        assert_eq!(d.game, Game::A);
        assert_eq!(d.limit_min, 10);
        assert_eq!(d.purpose, "見る");
        assert_eq!((d.peer_sim, d.role, d.allow_updown.len()), (None, None, 0));
    }

    #[test]
    fn parse_full() {
        let t = format!(
            "# 頭の注\n{MIN}peer_sim = --bind 127.0.0.1:1  --follow # 行末の注\nrole = guest\nallow_updown = a b\nsecond = none\n"
        );
        let d = parse(&t).expect("parse");
        assert_eq!(
            d.peer_sim,
            Some(vec![
                "--bind".into(),
                "127.0.0.1:1".into(),
                "--follow".into()
            ])
        );
        assert_eq!(d.role, Some(Role::Guest));
        assert_eq!(d.allow_updown, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(d.peer_sim2, None);
        let t2 = format!("{t}peer_sim2 = --bind 127.0.0.1:2 --nonce 1 # ゲスト役\n");
        let d2 = parse(&t2).expect("parse");
        assert_eq!(
            d2.peer_sim2,
            Some(vec![
                "--bind".into(),
                "127.0.0.1:2".into(),
                "--nonce".into(),
                "1".into()
            ])
        );
    }

    #[test]
    fn parse_rejects() {
        let cases: &[(&str, usize, &str)] = &[
            ("scriptx = scripts/a.txt\n", 1, "scriptx"),
            ("script\n", 1, ""),
            ("script = \n", 1, "script"),
            ("purpose = a\npurpose = b\n", 2, "purpose"),
            ("game = a\n", 0, "script"),
        ];
        for &(t, line, key) in cases {
            let e = parse(t).unwrap_err();
            assert_eq!((e.line, e.key.as_str()), (line, key), "{t:?}: {e}");
        }
        let bad_values: &[(&str, &str, &str)] = &[
            ("script = scripts/a.txt", "script = C:/x/a.txt", "script"),
            (
                "script = scripts/a.txt",
                "script = scripts/../a.txt",
                "script",
            ),
            (
                "script = scripts/a.txt",
                "script = scripts\\a.txt",
                "script",
            ),
            ("script = scripts/a.txt", "script = a.txt", "script"),
            ("ini = scripts/a.ini", "ini = /scripts/a.ini", "ini"),
            ("game = a", "game = d", "game"),
            ("game = a", "game = C", "game"),
            ("limit_min = 10", "limit_min = 0", "limit_min"),
            ("limit_min = 10", "limit_min = 61", "limit_min"),
            ("limit_min = 10", "limit_min = x", "limit_min"),
        ];
        for &(from, to, key) in bad_values {
            let t = MIN.replace(from, to);
            let e = parse(&t).unwrap_err();
            assert_eq!(e.key, key, "{to:?}: {e}");
        }
        for extra in [
            "role = both\n",
            "second = b\n",
            "peer_sim = --menu \"scripts/a b.plan\"\n",
            "peer_sim2 = --bind 127.0.0.1:2\n",
            "peer_sim = --bind 127.0.0.1:1\npeer_sim2 = --menu \"scripts/a b.plan\"\n",
            "peer_sim = --bind 127.0.0.1:1\npeer_sim2 = --bind 127.0.0.1:2\npeer_sim2 = --bind 127.0.0.1:3\n",
            // 3 台の回の指し合い: パスの形（second と同じ）・none は無い・ホストと観戦者を 1 つの定義に書けない・2 回書けない。
            "watcher = b\n",
            "watcher = scripts/t.txt\n",
            "watching = scripts/runs/../x.run\n",
            "watching = C:/x.run\n",
            "watcher = none\n",
            "watcher = scripts/runs/w.run\nwatching = scripts/runs/h.run\n",
            "watcher = scripts/runs/w.run\nwatcher = scripts/runs/v.run\n",
        ] {
            assert!(parse(&format!("{MIN}{extra}")).is_err(), "{extra:?}");
        }
    }

    #[test]
    fn parse_the_three_game_keys() {
        let d = parse(&MIN.replace("game = a", "game = c")).unwrap();
        assert_eq!((d.game, d.watcher, d.watching), (Game::C, None, None));
        let h = parse(&format!("{MIN}watcher = scripts/runs/w.run\n")).unwrap();
        assert_eq!(
            (h.watcher.as_deref(), h.watching.as_deref()),
            (Some("scripts/runs/w.run"), None)
        );
        let w = parse(&format!("{MIN}watching = scripts/runs/h.run\n")).unwrap();
        assert_eq!(
            (w.watcher.as_deref(), w.watching.as_deref()),
            (None, Some("scripts/runs/h.run"))
        );
    }

    #[test]
    fn peer_args_shape() {
        let v = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let a = v("--follow --peer 127.0.0.1:1 --peer 127.0.0.1:2 --mask 0xC4");
        let p = PeerArgs::parse(&a).unwrap();
        assert!(p.follow);
        assert_eq!(p.all("--peer").len(), 2);
        assert_eq!(p.get("--mask"), Some("0xC4"));
        for bad in [
            "--mask",
            "--mask --follow",
            "mask 1",
            "--help",
            "--compare a b",
        ] {
            assert!(PeerArgs::parse(&v(bad)).is_err(), "{bad:?}");
        }
    }
}
