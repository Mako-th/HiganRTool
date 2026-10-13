//! 接続コードの受け渡しのウィンドウ（ini の `code_side = offer | answer` のとき）。
//!
//! ゲームを起こしたローダーのウィンドウが残り、dll が書く `my_code.txt` をクリップボードへコピーし、ウィンドウに貼られた相手のコードを
//! `peer_code.txt` に置き、`status.txt` の状態を 1 行で出す。`my_code.txt` が消えるかゲームが終わると待ちをやめる
//! （dll は終わり方によらず先に `my_code.txt` を消すので、文言ではなくこれで終わりを見る）。
//! 相手のコードの前に `0`〜`8` か `delay` を打つと、対戦の形として `mode.txt` に書く。
//! ファイル名・置き場・ini の読み方は dll と同じ値にする。ゲームのフォルダの中なら dll がネットワークの準備をしないのでウィンドウも開かない。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

pub const INI_FILE: &str = "HiganRTool.ini";
const SECTION: &str = "netplay";
// ファイル名・置き場は dll と同じ値にするため core に置く。
pub use higanr_core::code_files::{
    MAX_FILE_BYTES as MAX_READ, MODE_FILE, MODE_TMP_FILE, MY_CODE_FILE, PEER_CODE_FILE,
    PEER_TMP_FILE, POLL, STATUS_FILE,
};
use higanr_core::lang::{Lang, Tx};
use higanr_core::log_names::LOG_PREFIX;
use higanr_core::mode_pick::{self, Pick, Shape};
use higanr_core::wire::Mode;

use crate::ui_lang::by_lang;
/// 消えた後、最後の状態の行を待つ回数（`POLL` 単位）。dll は消した直後に `status.txt` を書くので 2 回で届く。
const TAIL_POLLS: u32 = 2;
/// 置き換えを試す回数。ウイルス対策などが一瞬つかむことがあるため。
const REPLACE_TRIES: u32 = 3;
const REPLACE_GAP: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Offer,
    Answer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub side: Side,
    pub dir: PathBuf,
}

// ---------------- ini と置き場（pure） ----------------

/// dll が読む ini の場所。`--ini` → 親から継いだ環境変数 → DLL のフォルダの順（dll の `net_config::locate` と同じ）。
/// 継いだ値が空・相対なら dll はネットワークの準備をしないので None。
pub fn ini_location(
    flag: Option<&Path>,
    inherited: Option<&OsStr>,
    dll_dir: &Path,
) -> Option<PathBuf> {
    if let Some(f) = flag {
        return Some(f.to_path_buf());
    }
    match inherited {
        Some(v) => {
            let p = PathBuf::from(v);
            (!v.is_empty() && p.is_absolute()).then_some(p)
        }
        None => Some(dll_dir.join(INI_FILE)),
    }
}

/// ウィンドウで受け渡す形（`enable = 1` と `code_side = offer | answer`）なら Some(側, `code_dir`)。
pub fn side_of(ini_text: &str) -> Option<(Side, Option<String>)> {
    let ini = higanr_core::ini::parse(ini_text).ok()?;
    if ini.get(SECTION, "enable") != Some("1") {
        return None;
    }
    let side = match ini.get(SECTION, "code_side")? {
        "offer" => Side::Offer,
        "answer" => Side::Answer,
        _ => return None,
    };
    let dir = match ini.get(SECTION, "code_dir") {
        None => None,
        Some("") => return None,
        Some(v) => Some(v.to_string()),
    };
    Some((side, dir))
}

/// ゲームの中のメニューで接続する形（`code_side = game`）か。ウィンドウは開かない。
pub fn is_game_menu(ini_text: &str) -> bool {
    let Ok(ini) = higanr_core::ini::parse(ini_text) else {
        return false;
    };
    ini.get(SECTION, "enable") == Some("1") && ini.get(SECTION, "code_side") == Some("game")
}

/// 項目の名前（妖怪 対 妖怪）は訳さない。
pub const GAME_MENU_HINT: Tx = Tx {
    ja: "ゲームの中の Match Mode →「妖怪 対 妖怪」から接続します。",
    en: "Connect from Match Mode > 妖怪 対 妖怪 in the game.",
};

/// 置き場。`code_dir` が絶対ならそれ、相対なら DLL のフォルダから、無ければ DLL のフォルダの `code`。
pub fn code_dir(dll_dir: &Path, setting: Option<&str>) -> PathBuf {
    higanr_core::code_files::code_dir_under(dll_dir, setting)
}

fn normalize(p: &Path) -> String {
    let mut s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    while s.ends_with('\\') {
        s.pop();
    }
    s
}

pub fn is_same_or_inside(dir: &Path, root: &Path) -> bool {
    let d = normalize(dir);
    let r = normalize(root);
    if r.is_empty() {
        return false;
    }
    d == r || (d.starts_with(&r) && d[r.len()..].starts_with('\\'))
}

/// ウィンドウで受け渡すか。置き場がゲームのフォルダの中なら断る（ローダーが書けばゲームのフォルダに書くことになる）。
pub fn plan(
    ini_text: Option<&str>,
    dll_dir: &Path,
    game_dir: &Path,
    lang: Lang,
) -> Result<Option<Plan>, String> {
    let Some((side, setting)) = ini_text.and_then(side_of) else {
        return Ok(None);
    };
    let dir = code_dir(dll_dir, setting.as_deref());
    if is_same_or_inside(&dir, game_dir) {
        let shown = dir.display();
        return Err(by_lang!(
            lang,
            ja: "接続コードの置き場 {shown} がゲームのフォルダの中なので、このウィンドウでの受け渡しはしません",
            en: "The connection code folder {shown} is inside the game folder, so codes are not exchanged in this window"
        ));
    }
    Ok(Some(Plan { side, dir }))
}

// ---------------- ファイルの見張り（pure） ----------------

/// 前後の空白と BOM を落とす。空なら None。
fn bare(bytes: &[u8]) -> Option<String> {
    let t = String::from_utf8_lossy(bytes);
    let t = t.trim().trim_start_matches('\u{FEFF}').trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// `status.txt` をウィンドウの 1 行にする。dll が書く見出し行（末尾が `:`）は落とし、残りを ` / ` で繋ぐ。
pub fn status_line(bytes: &[u8]) -> Option<String> {
    let t = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = t
        .trim_start_matches('\u{FEFF}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() >= 2 && (lines[0].ends_with(':') || lines[0].ends_with('：')) {
        lines.remove(0);
    }
    (!lines.is_empty()).then(|| lines.join(" / "))
}

/// ゲームを起こす前のファイルの中身。前の回の残りを「新しく現れた」と取り違えないため。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Before {
    my_code: Option<String>,
    status: Option<Vec<u8>>,
}

impl Before {
    pub fn read(dir: &Path) -> Before {
        Before {
            my_code: read_small(&dir.join(MY_CODE_FILE)).and_then(|b| bare(&b)),
            status: read_small(&dir.join(STATUS_FILE)),
        }
    }
}

pub const CLEARED_PEER: Tx = Tx {
    ja: "前回の相手のコードを片づけました。",
    en: "Cleared the opponent's connection code from the previous run.",
};

pub const CLEANED_UP_MY_CODE: Tx = Tx {
    ja: "残っていた自分の接続コードを消しました（中に鍵が入っているため）。",
    en: "Deleted your leftover connection code (it contains a key).",
};

fn cannot_remove(p: &Path, e: &std::io::Error, lang: Lang) -> String {
    let shown = p.display();
    by_lang!(lang, ja: "{shown} を消せません: {e}", en: "Cannot delete {shown}: {e}")
}

/// ゲームの強制終了で dll が消しそびれた `my_code.txt`（始める側の秘密を含む）を消す。`peer_code.txt` と `status.txt` は残す。
pub fn cleanup_leftover_my_code(dir: &Path, lang: Lang) -> Result<bool, String> {
    let p = dir.join(MY_CODE_FILE);
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(cannot_remove(&p, &e, lang)),
    }
}

/// 前の回の `peer_code.txt`（と `.tmp`）を消す。残っていると dll が最初の回に読み、期限内なら古い相手へ繋ぎにいくため。
pub fn clear_stale_peer(dir: &Path, lang: Lang) -> Result<bool, String> {
    let mut removed = false;
    for name in [PEER_CODE_FILE, PEER_TMP_FILE] {
        let p = dir.join(name);
        match std::fs::remove_file(&p) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(cannot_remove(&p, &e, lang)),
        }
    }
    Ok(removed)
}

/// 起動前の準備: 前の回の相手のコードを片づけ、置き場の中身を書き留める。片づけられなくても起動はやめない。
pub fn before_launch(plan: &Plan, lang: Lang) -> (Before, Option<String>) {
    let peer = match clear_stale_peer(&plan.dir, lang) {
        Ok(true) => Some(format!("HiganRTool: {}", CLEARED_PEER.get(lang))),
        Ok(false) => None,
        Err(e) => Some(by_lang!(
            lang,
            ja: "HiganRTool: 前回の相手のコードを片づけられませんでした（{e}）。このまま続けます。",
            en: "HiganRTool: Could not clear the opponent's code from the previous run ({e}). Continuing."
        )),
    };
    // 前の回の対戦の形の選択は黙って消す（ウィンドウに出すのは消せなかったときだけ）。
    let mode = clear_stale_mode(&plan.dir, lang).err().map(|e| {
        by_lang!(
            lang,
            ja: "HiganRTool: 前回の同期方式の選択を片づけられませんでした（{e}）。今回の番号と合わない選択は使いません。",
            en: "HiganRTool: Could not clear the netcode choice from the previous run ({e}). A choice that does not match this run is not used."
        )
    });
    let note = match (peer, mode) {
        (Some(p), Some(m)) => Some(format!("{p}\n{m}")),
        (p, m) => p.or(m),
    };
    (Before::read(&plan.dir), note)
}

/// 前の回の `mode.txt` を消す。プロセス番号は使い回され、dll が昔の選択を黙って使うため。
pub fn clear_stale_mode(dir: &Path, lang: Lang) -> Result<(), String> {
    for name in [MODE_FILE, MODE_TMP_FILE] {
        let p = dir.join(name);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(cannot_remove(&p, &e, lang)),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    MyCode { text: String, renewed: bool },
    Status(String),
    MyCodeGone,
}

#[derive(Debug, Clone)]
pub struct Watch {
    /// 起動の前の残りを含む。
    my_code: Option<String>,
    /// 起動の前の残りは入らない。
    shown: Option<String>,
    status: Option<Vec<u8>>,
}

impl Watch {
    pub fn new(before: Before) -> Watch {
        Watch {
            my_code: before.my_code,
            shown: None,
            status: before.status,
        }
    }

    pub fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    pub fn step(&mut self, my_code: Option<&[u8]>, status: Option<&[u8]>) -> Vec<Event> {
        let mut out = Vec::new();
        let mut gone = false;
        match my_code {
            // 空のファイルは書きかけか手違いとして何もしない。
            Some(b) => {
                if let Some(t) = bare(b)
                    && self.my_code.as_deref() != Some(t.as_str())
                {
                    self.my_code = Some(t.clone());
                    out.push(Event::MyCode {
                        text: t.clone(),
                        renewed: self.shown.is_some(),
                    });
                    self.shown = Some(t);
                }
            }
            None => {
                self.my_code = None;
                // ウィンドウに出したコードが消えたときだけ終わりと見る（起動の前の残りを dll が消したのは終わりではない）。
                gone = self.shown.take().is_some();
            }
        }
        if let Some(b) = status
            && self.status.as_deref() != Some(b)
        {
            self.status = Some(b.to_vec());
            if let Some(l) = status_line(b) {
                out.push(Event::Status(l));
            }
        }
        if gone {
            out.push(Event::MyCodeGone);
        }
        out
    }
}

// ---------------- 貼り付け（pure） ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paste {
    Ignore,
    Refuse(Tx),
    Write(String),
}

pub const REFUSE_OWN: Tx = Tx {
    ja: "これは自分の接続コードです。相手から届いたコードを貼ってください。",
    en: "This is your own connection code. Paste the code you received from your opponent.",
};
pub const REFUSE_SAME: Tx = Tx {
    ja: "このコードはもう受け取っています（結果は「状態」の行に出ます）。",
    en: "This code was already received (the result appears on the Status line).",
};

/// `own` はウィンドウに出した自分のコード、`last` は前に置いた相手のコード。
pub fn judge_paste(line: &str, own: Option<&str>, last: Option<&str>) -> Paste {
    let Some(t) = bare(line.as_bytes()) else {
        return Paste::Ignore;
    };
    if own == Some(t.as_str()) {
        return Paste::Refuse(REFUSE_OWN);
    }
    if last == Some(t.as_str()) {
        return Paste::Refuse(REFUSE_SAME);
    }
    Paste::Write(t)
}

// ---------------- 対戦の形 ----------------

/// 画面から読めない値（rollback の入力の遅れ D・delay 方式）をウィンドウに出す言い方。
fn shape_words(mode: Mode, d: u8, lang: Lang) -> String {
    match (mode, lang) {
        (Mode::Rollback, Lang::Ja) => format!("rollback・ディレイ {d} フレーム"),
        (Mode::Rollback, Lang::En) => {
            let unit = if d == 1 { "frame" } else { "frames" };
            format!("rollback, Delay {d} {unit}")
        }
        (Mode::Delay, Lang::Ja) => "delay 方式".to_string(),
        (Mode::Delay, Lang::En) => "delay".to_string(),
    }
}

pub fn pick_words(p: Pick, lang: Lang) -> String {
    match p {
        Pick::Rollback(d) => shape_words(Mode::Rollback, d, lang),
        Pick::Delay => shape_words(Mode::Delay, 0, lang),
    }
}

/// 最初にウィンドウに出す今の対戦の形の 1 行。rollback の実行には変え方も添える。
pub fn mode_intro(ini: Shape, lang: Lang) -> String {
    let words = shape_words(ini.mode, ini.delay, lang);
    match (ini.mode, lang) {
        (Mode::Rollback, Lang::Ja) => format!(
            "HiganRTool: 同期方式: {words}。変えるなら、相手のコードを貼る前に 0〜8 の数字（rollback のディレイのフレーム数）か delay と打って Enter を押してください（rollback は 2 人とも選んだときだけ。ディレイはホストの値を使います）。"
        ),
        (Mode::Rollback, Lang::En) => format!(
            "HiganRTool: Netcode: {words}. To change it, type a number 0-8 (the Delay in frames for rollback) or delay and press Enter before you paste your opponent's code (rollback is used only when both players choose it; the host's Delay is used)."
        ),
        (Mode::Delay, Lang::Ja) => "HiganRTool: 同期方式: delay 方式。".to_string(),
        (Mode::Delay, Lang::En) => "HiganRTool: Netcode: delay.".to_string(),
    }
}

/// 選択を書いた後の返事。`placed`（相手のコードを置いた後）の選択はこの接続には効かないと言う。
pub fn pick_reply(pick: Pick, ini: Shape, placed: bool, lang: Lang) -> String {
    let head = match (pick, ini.mode, lang) {
        (Pick::Rollback(_), Mode::Delay, Lang::Ja) => {
            "HiganRTool: 設定ファイル HiganRTool.ini が delay 方式になっているので、rollback にはなりません。delay 方式で接続します。".to_string()
        }
        (Pick::Rollback(_), Mode::Delay, Lang::En) => {
            "HiganRTool: HiganRTool.ini is set to delay, so rollback is not used. Connecting with delay.".to_string()
        }
        _ => {
            let words = pick_words(pick, lang);
            by_lang!(
                lang,
                ja: "HiganRTool: 同期方式を {words}にしました。",
                en: "HiganRTool: Netcode set to {words}."
            )
        }
    };
    if placed {
        match lang {
            Lang::Ja => format!(
                "{head} 相手のコードをもう受け取ったので、この接続では変わりません。変えるなら 2 人とも起動し直してください。"
            ),
            Lang::En => format!(
                "{head} Your opponent's code was already received, so this does not change this connection. To change it, both players must restart."
            ),
        }
    } else {
        head
    }
}

pub const REFUSE_SHORT: Tx = Tx {
    ja: "HiganRTool: 短すぎて、接続コードとしても同期方式としても読めません。同期方式なら 0〜8 の数字か delay のどれか、接続コードなら途中で切れていないか確かめて 1 行で全部貼ってください。",
    en: "HiganRTool: Too short to be a connection code or a netcode choice. For a netcode, type a number 0-8 or delay. For a connection code, check that it was not cut off and paste all of it on one line.",
};

#[derive(Debug, Clone, Copy)]
struct ModeUi {
    ini: Shape,
    /// `mode.txt` に書く対象のゲームの pid。
    pid: u32,
}

// ---------------- ファイルの読み書き ----------------

/// 無い・読めないなら None（次の回に読み直す）。
fn read_small(p: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(p).ok()?;
    let mut b = Vec::new();
    f.take(MAX_READ).read_to_end(&mut b).ok()?;
    Some(b)
}

/// 一時の名前に書き切ってから置き換える（dll が書きかけを読まない）。末尾の改行は dll の `write_my_code` と揃えた。
pub fn write_peer_code(dir: &Path, code: &str, lang: Lang) -> Result<(), String> {
    replace_file(
        dir,
        PEER_TMP_FILE,
        PEER_CODE_FILE,
        &format!("{code}\r\n"),
        lang,
    )
}

/// `mode.txt` に置く。`pid` が合うときだけ dll が採る。
pub fn write_mode_file(dir: &Path, pid: u32, pick: Pick, lang: Lang) -> Result<(), String> {
    replace_file(
        dir,
        MODE_TMP_FILE,
        MODE_FILE,
        &mode_pick::render(pid, pick),
        lang,
    )
}

fn replace_file(
    dir: &Path,
    tmp_name: &str,
    dest_name: &str,
    text: &str,
    lang: Lang,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        let shown = dir.display();
        by_lang!(lang, ja: "{shown} を作れません: {e}", en: "Cannot create {shown}: {e}")
    })?;
    let tmp = dir.join(tmp_name);
    std::fs::write(&tmp, text).map_err(|e| {
        let shown = tmp.display();
        by_lang!(lang, ja: "{shown} に書けません: {e}", en: "Cannot write to {shown}: {e}")
    })?;
    let dest = dir.join(dest_name);
    let mut last = None;
    for i in 0..REPLACE_TRIES {
        if i > 0 {
            std::thread::sleep(REPLACE_GAP);
        }
        match std::fs::rename(&tmp, &dest) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    let _ = std::fs::remove_file(&tmp);
    let shown = dest.display();
    let why = last.map_or_else(String::new, |e| e.to_string());
    Err(by_lang!(
        lang,
        ja: "{shown} へ置き換えられません: {why}",
        en: "Cannot replace {shown}: {why}"
    ))
}

// ---------------- ウィンドウの手順 ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Game {
    Running,
    Ended,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Finished,
    GameEnded,
    GameUnknown,
}

/// ウィンドウの外。本物は `main::RealRelayHost`、検査は偽物。
pub trait Host {
    fn game(&mut self) -> Game;
    fn copy(&mut self, text: &str) -> Result<(), String>;
    fn say(&mut self, line: &str);
}

#[derive(Debug)]
pub struct Relay {
    plan: Plan,
    watch: Watch,
    last_written: Option<String>,
    tail: Option<u32>,
    clipboard: bool,
    mode: Option<ModeUi>,
    lang: Lang,
}

impl Relay {
    pub fn new(plan: Plan, before: Before, lang: Lang) -> Relay {
        Relay {
            plan,
            watch: Watch::new(before),
            last_written: None,
            tail: None,
            clipboard: true,
            mode: None,
            lang,
        }
    }

    pub fn with_mode(mut self, ini: Shape, pid: u32) -> Relay {
        self.mode = Some(ModeUi { ini, pid });
        self
    }

    /// 同じ PC で 2 台起こす開発の回で、使っている人のクリップボードを書き換えないため。
    pub fn without_clipboard(mut self) -> Relay {
        self.clipboard = false;
        self
    }

    pub fn intro(&self) -> String {
        let lang = self.lang;
        let todo = match (self.plan.side, lang) {
            (Side::Offer, Lang::Ja) => {
                "自分の接続コードが下に出たら相手に送り、相手から返ってきたコードをここに貼って Enter を押してください。"
            }
            (Side::Offer, Lang::En) => {
                "When your connection code appears below, send it to your opponent, paste the code they send back here, and press Enter."
            }
            (Side::Answer, Lang::Ja) => {
                "相手から届いた接続コードをここに貼って Enter を押してください。"
            }
            (Side::Answer, Lang::En) => {
                "Paste the connection code you received from your opponent here and press Enter."
            }
        };
        let dir = self.plan.dir.display();
        let mut s = by_lang!(
            lang,
            ja: "HiganRTool: このウィンドウで接続コードをやり取りします。{todo}\nHiganRTool: （コードの置き場: {dir}）",
            en: "HiganRTool: Connection codes are exchanged in this window. {todo}\nHiganRTool: (Code folder: {dir})"
        );
        if let Some(m) = &self.mode {
            s.push('\n');
            s.push_str(&mode_intro(m.ini, lang));
        }
        s
    }

    pub fn tick(&mut self, host: &mut dyn Host, pasted: &[String]) -> Option<End> {
        let lang = self.lang;
        for line in pasted {
            // 対戦の形の選択は接続コードの判定より先に見る（接続コードは 100 文字を越えるので選択の文字とは一致しない）。
            if let Some(m) = self.mode
                && let Some(pick) = Pick::parse(line)
            {
                match write_mode_file(&self.plan.dir, m.pid, pick, lang) {
                    Ok(()) => {
                        host.say(&pick_reply(pick, m.ini, self.last_written.is_some(), lang));
                    }
                    Err(e) => host.say(&by_lang!(
                        lang,
                        ja: "HiganRTool: 同期方式を保存できませんでした（{e}）。打ち直してください。",
                        en: "HiganRTool: Could not save the netcode choice ({e}). Type it again."
                    )),
                }
                continue;
            }
            // 短すぎる文字（`3`・`rollback`・打ち間違い）は置かない。置くと dll が「字が欠けている」と言い、選択のつもりの人に原因が伝わらない。
            if self.mode.is_some()
                && let Some(t) = bare(line.as_bytes())
                && t.chars().count() < higanr_core::code::MIN_CODE_CHARS
            {
                host.say(REFUSE_SHORT.get(lang));
                continue;
            }
            match judge_paste(line, self.watch.shown(), self.last_written.as_deref()) {
                Paste::Ignore => {}
                Paste::Refuse(why) => host.say(&format!("HiganRTool: {}", why.get(lang))),
                Paste::Write(code) => match write_peer_code(&self.plan.dir, &code, lang) {
                    Ok(()) => {
                        host.say(match lang {
                            Lang::Ja => "HiganRTool: 相手のコードを受け取りました。接続するまでの状態を下に出します。",
                            Lang::En => "HiganRTool: Received your opponent's code. The connection progress is shown below.",
                        });
                        self.last_written = Some(code);
                    }
                    Err(e) => host.say(&by_lang!(
                        lang,
                        ja: "HiganRTool: 相手のコードを保存できませんでした（{e}）。貼り直してください。",
                        en: "HiganRTool: Could not save your opponent's code ({e}). Paste it again."
                    )),
                },
            }
        }
        let my = read_small(&self.plan.dir.join(MY_CODE_FILE));
        let st = read_small(&self.plan.dir.join(STATUS_FILE));
        for ev in self.watch.step(my.as_deref(), st.as_deref()) {
            match ev {
                Event::MyCode { text, renewed } => {
                    host.say(match (renewed, lang) {
                        (true, Lang::Ja) => "HiganRTool: 自分の接続コードが新しくなりました:",
                        (false, Lang::Ja) => "HiganRTool: 自分の接続コード:",
                        (true, Lang::En) => "HiganRTool: Your connection code was renewed:",
                        (false, Lang::En) => "HiganRTool: Your connection code:",
                    });
                    // ウィンドウから手で選んでコピーするときに余計な文字が入らないよう、コードだけの行にする。
                    host.say(&text);
                    if !self.clipboard {
                        host.say(match lang {
                            Lang::Ja => "HiganRTool: クリップボードにはコピーしなかった（--no-clipboard）。上のコードを選んでコピーし、チャットで相手に送ってください。",
                            Lang::En => "HiganRTool: Not copied to the clipboard (--no-clipboard). Select the code above, copy it, and send it to your opponent in chat.",
                        });
                        continue;
                    }
                    match host.copy(&text) {
                        Ok(()) => host.say(match lang {
                            Lang::Ja => "HiganRTool: クリップボードにコピーしました。チャットで相手に送ってください。",
                            Lang::En => "HiganRTool: Copied to the clipboard. Send it to your opponent in chat.",
                        }),
                        Err(e) => host.say(&by_lang!(
                            lang,
                            ja: "HiganRTool: クリップボードにコピーできませんでした（{e}）。上のコードを選んでコピーし、チャットで相手に送ってください。",
                            en: "HiganRTool: Could not copy to the clipboard ({e}). Select the code above, copy it, and send it to your opponent in chat."
                        )),
                    }
                }
                Event::Status(l) => host.say(&by_lang!(
                    lang,
                    ja: "HiganRTool: 状態: {l}",
                    en: "HiganRTool: Status: {l}"
                )),
                Event::MyCodeGone => {
                    if self.tail.is_none() {
                        self.tail = Some(TAIL_POLLS);
                    }
                }
            }
        }
        if let Some(n) = self.tail {
            if n == 0 {
                host.say(match lang {
                    Lang::Ja => "HiganRTool: 接続コードの受け渡しを終えました（結果は直前の「状態」の行です）。",
                    Lang::En => "HiganRTool: The connection code exchange is finished (the result is on the last Status line).",
                });
                return Some(End::Finished);
            }
            self.tail = Some(n - 1);
        }
        match host.game() {
            Game::Running => None,
            Game::Ended => {
                host.say(match lang {
                    Lang::Ja => "HiganRTool: ゲームが終了したため、接続コードの受け渡しをやめます。",
                    Lang::En => "HiganRTool: The game has ended, so the connection code exchange is stopped.",
                });
                self.cleanup_my_code(host);
                Some(End::GameEnded)
            }
            Game::Unknown(why) => {
                host.say(&by_lang!(
                    lang,
                    ja: "HiganRTool: ゲームの状態が把握できないため、接続コードの受け渡しをやめます（{why}）。",
                    en: "HiganRTool: Cannot tell the state of the game, so the connection code exchange is stopped ({why})."
                ));
                self.cleanup_my_code(host);
                Some(End::GameUnknown)
            }
        }
    }

    /// ゲームが終わった経路でだけ呼ぶ: 残っている自分の接続コードを消し、結果をウィンドウへ 1 行出す。
    fn cleanup_my_code(&self, host: &mut dyn Host) {
        let lang = self.lang;
        match cleanup_leftover_my_code(&self.plan.dir, lang) {
            Ok(true) => host.say(&format!("HiganRTool: {}", CLEANED_UP_MY_CODE.get(lang))),
            Ok(false) => {}
            Err(e) => host.say(&by_lang!(
                lang,
                ja: "HiganRTool: 残っていた自分の接続コードを消せませんでした（{e}）。",
                en: "HiganRTool: Could not delete your leftover connection code ({e})."
            )),
        }
    }
}

/// ウィンドウの手順を終わるまで回す。標準入力の読みは別スレッドで止まってよく、ここは `poll` ごとにファイルとゲームを見る。
/// 貼られた行は `poll` を待たずに置く。
pub fn run(
    relay: &mut Relay,
    host: &mut dyn Host,
    pasted: &Receiver<String>,
    poll: Duration,
) -> End {
    let mut open = true;
    loop {
        let mut lines = Vec::new();
        if open {
            match pasted.recv_timeout(poll) {
                Ok(l) => lines.push(l),
                Err(RecvTimeoutError::Timeout) => {}
                // 標準入力が閉じた。以後はファイルとゲームだけを見る。
                Err(RecvTimeoutError::Disconnected) => open = false,
            }
            while let Ok(l) = pasted.try_recv() {
                lines.push(l);
            }
        } else {
            std::thread::sleep(poll);
        }
        if let Some(end) = relay.tick(host, &lines) {
            return end;
        }
    }
}

pub const LINGER: Tx = Tx {
    ja: "HiganRTool: このウィンドウは、花映塚を閉じると閉じます。",
    en: "HiganRTool: This window closes when you close Touhou 9.",
};
pub const AFTER_FINISH: Tx = Tx {
    ja: "HiganRTool: 接続コードの受け渡しはもう終わっています。やり直すときは、花映塚を閉じて起動し直してください。",
    en: "HiganRTool: The connection code exchange is already finished. To start over, close Touhou 9 and start it again.",
};

/// 受け渡しを終えた後、ゲームが終わるまでウィンドウを残す。ショートカットから起こすとウィンドウごと消え、最後の状態の行が読めなくなるため。
pub fn linger(host: &mut dyn Host, pasted: &Receiver<String>, poll: Duration, lang: Lang) -> End {
    host.say(LINGER.get(lang));
    let mut open = true;
    loop {
        if open {
            match pasted.recv_timeout(poll) {
                Ok(l) if !l.trim().is_empty() => host.say(AFTER_FINISH.get(lang)),
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => open = false,
            }
        } else {
            std::thread::sleep(poll);
        }
        match host.game() {
            Game::Running => {}
            Game::Ended => return End::GameEnded,
            Game::Unknown(_) => return End::GameUnknown,
        }
    }
}

// ---------------- ゲームが終わった後の案内（送ってほしいファイル） ----------------

/// dll の `win::open_log` と同じ値。
pub const LOGS_DIR: &str = "logs";
/// dll の `script_watch::NET_FRAMES_SUFFIX` と同じ値。
pub const NET_FRAMES_SUFFIX: &str = "_net_frames.csv.gz";

pub const CLOSE_HINT: Tx = Tx {
    ja: "HiganRTool: Enter キーを押すと、このウィンドウを閉じます。",
    en: "HiganRTool: Press Enter to close this window.",
};

/// `logs` の中から、そのプロセス番号の回ごとの本ログ（台本の実行。`dll_<日付>-<時刻>_pid<番号>.log`、重複は `_2` 以降）を探す。
/// 隣の記録（`_replay.log` など）は選ばない。複数あれば番号の大きい方。
pub fn find_main_log(names: &[String], pid: u32) -> Option<String> {
    let tag = format!("_pid{pid}");
    names
        .iter()
        .filter_map(|n| {
            let rest = n.strip_prefix(LOG_PREFIX)?.split_once(&tag)?.1;
            let attempt = if rest == ".log" {
                1
            } else {
                rest.strip_prefix('_')?
                    .strip_suffix(".log")?
                    .parse::<u32>()
                    .ok()?
            };
            Some((attempt, n))
        })
        .max_by_key(|(attempt, _)| *attempt)
        .map(|(_, n)| n.clone())
}

/// 「送ってほしいファイル」の 1 行。読むのは一般の利用者なので、内部の語・アドレス・括弧を使わない。`csv` はこの実行の CSV のファイル名。
pub fn send_files_line(log: Option<&str>, csv: Option<&str>, lang: Lang) -> String {
    match log {
        Some(l) => {
            if let Some(c) = csv {
                by_lang!(
                    lang,
                    ja: "HiganRTool: 送ってほしいファイル: {LOGS_DIR}\\{l} と {LOGS_DIR}\\{c}",
                    en: "HiganRTool: Files to send: {LOGS_DIR}\\{l} and {LOGS_DIR}\\{c}"
                )
            } else {
                by_lang!(
                    lang,
                    ja: "HiganRTool: 送ってほしいファイル: {LOGS_DIR}\\{l}",
                    en: "HiganRTool: File to send: {LOGS_DIR}\\{l}"
                )
            }
        }
        None => by_lang!(
            lang,
            ja: "HiganRTool: 送ってほしいファイル: {LOGS_DIR} フォルダの一番新しい dll_…log と、同じ名前で始まる {NET_FRAMES_SUFFIX}",
            en: "HiganRTool: Files to send: the newest dll_...log in the {LOGS_DIR} folder, and the {NET_FRAMES_SUFFIX} file whose name starts the same"
        ),
    }
}

/// 簡潔なログで何かがうまくいかなかった回だけ、送るのは本ログで、もっと詳しい記録が要るときの頼み方を添える。
pub fn more_detail_line(lang: Lang) -> String {
    by_lang!(
        lang,
        ja: "HiganRTool: もっと詳しい記録が要るときは、HiganRTool.ini の [log] の detail = 0 を detail = 1 に変えて（[log] が無い古い ini なら [log] と detail = 1 の 2 行を足して）もう一度遊んでください。",
        en: "HiganRTool: For a more detailed record, change detail = 0 under [log] in HiganRTool.ini to detail = 1 and play again. If your old ini has no [log], add the two lines [log] and detail = 1."
    )
}

/// 簡潔なログで、何かがうまくいかなかった回か（`log_cfg::brief_log_has_trouble`）。詳細なログは false。
fn brief_text_went_wrong(text: &str) -> bool {
    higanr_core::log_cfg::is_brief_log(text) && higanr_core::log_cfg::brief_log_has_trouble(text)
}

/// 読む本ログの上限。簡潔なログは数十 KB。
const LOG_READ_MAX: u64 = 4 * 1024 * 1024;

/// 回ごとの本ログを先頭から上限まで読む。読めなければ None。
fn read_log_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut body = Vec::new();
    f.take(LOG_READ_MAX).read_to_end(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// 区切りの行（`log_names::header_pid`）か。
fn is_header_of(line: &[u8], pid: Option<u32>) -> bool {
    let line = String::from_utf8_lossy(line);
    let pid_of = higanr_core::log_names::header_pid(line.trim_end_matches(['\r', '\n']));
    match pid {
        Some(p) => pid_of == Some(p),
        None => pid_of.is_some(),
    }
}

/// 日ごとの本ログから、`pid` の最後の実行の範囲を読む。区切りの行はファイル全体から行ごとに探し（全体は持たない）、
/// そこから次の区切りの行か上限まで読む。実行が末尾から上限より遠くにあっても見つかる。
fn read_daily_run(path: &Path, pid: u32) -> Option<String> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    let mut r = BufReader::new(std::fs::File::open(path).ok()?);
    let mut line = Vec::new();
    let (mut at, mut start) = (0u64, None);
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line).ok()?;
        if n == 0 {
            break;
        }
        if is_header_of(&line, Some(pid)) {
            start = Some(at);
        }
        at += n as u64;
    }
    r.seek(SeekFrom::Start(start?)).ok()?;
    let mut body = Vec::new();
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        if !body.is_empty() && is_header_of(&line, None) {
            break;
        }
        body.extend_from_slice(&line);
        if body.len() as u64 >= LOG_READ_MAX {
            break;
        }
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// 送ってほしい本ログの見つけ方の結果。
struct FoundLog {
    /// `logs` の中の本ログのファイル名。
    log: String,
    /// この実行の隣のファイルの名前の元（`<元>_net_frames.csv.gz` など）。
    stem: String,
    /// この実行の範囲の本文（読めた分）。
    text: Option<String>,
}

/// 日ごとの本ログ（新しい日から）の中で、その PID の最後の実行を探す。
fn find_daily_run(logs_dir: &Path, names: &[String], pid: u32) -> Option<FoundLog> {
    let mut daily: Vec<&String> = names
        .iter()
        .filter(|n| higanr_core::log_names::is_daily_log_name(n))
        .collect();
    daily.sort();
    for name in daily.into_iter().rev() {
        let Some(text) = read_daily_run(&logs_dir.join(name), pid) else {
            continue;
        };
        if let Some(run) = higanr_core::log_names::last_run_section(&text, pid) {
            return Some(FoundLog {
                log: name.clone(),
                stem: run.stem.to_string(),
                text: Some(run.text.to_string()),
            });
        }
    }
    None
}

/// 回ごとの本ログ（台本の実行）を先に、無ければ日ごとの本ログから探す。
fn find_run_log(logs_dir: &Path, names: &[String], pid: u32) -> Option<FoundLog> {
    if let Some(log) = find_main_log(names, pid) {
        let stem = log.strip_suffix(".log").unwrap_or(&log).to_string();
        let text = read_log_text(&logs_dir.join(&log));
        return Some(FoundLog { log, stem, text });
    }
    find_daily_run(logs_dir, names, pid)
}

pub fn send_files_for(logs_dir: &Path, pid: u32, lang: Lang) -> String {
    let names: Vec<String> = std::fs::read_dir(logs_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let found = find_run_log(logs_dir, &names, pid);
    let csv = found.as_ref().and_then(|f| {
        let name = format!("{}{NET_FRAMES_SUFFIX}", f.stem);
        names.contains(&name).then_some(name)
    });
    let line = send_files_line(found.as_ref().map(|f| f.log.as_str()), csv.as_deref(), lang);
    match found.as_ref().and_then(|f| f.text.as_deref()) {
        Some(text) if brief_text_went_wrong(text) => {
            format!("{line}\n{}", more_detail_line(lang))
        }
        _ => line,
    }
}

/// ウィンドウを閉じる前に送ってほしいファイルを 1 行出す。`interactive` なら Enter を待つ（ウィンドウごと閉じて読めなくなるため）。
pub fn farewell(
    host: &mut dyn Host,
    pasted: &Receiver<String>,
    logs_dir: &Path,
    pid: u32,
    interactive: bool,
    lang: Lang,
) {
    host.say(&send_files_for(logs_dir, pid, lang));
    if interactive {
        host.say(CLOSE_HINT.get(lang));
        let _ = pasted.recv();
    }
}

#[cfg(test)]
mod tests {
    //! 一時ディレクトリだけを使い、クリップボードは偽物。
    use super::*;
    use higanr_core::code;
    use higanr_core::lang::has_japanese;
    use std::sync::mpsc;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "HiganRTool_loader_relay_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const INI_OFFER: &str = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n";

    #[test]
    fn side_is_read_only_when_the_dll_would_open_the_code_swap() {
        assert_eq!(side_of(INI_OFFER), Some((Side::Offer, None)));
        assert_eq!(
            side_of("[NetPlay]\nEnable = 1\ncode_side = answer\ncode_dir = code_b\n"),
            Some((Side::Answer, Some("code_b".to_string())))
        );
        assert_eq!(side_of("[netplay]\nenable = 1\npeer = 127.0.0.1:7\n"), None);
        assert_eq!(side_of("[netplay]\nenable = 0\ncode_side = offer\n"), None);
        assert_eq!(side_of("[netplay]\ncode_side = offer\n"), None);
        assert_eq!(side_of("[pacing]\nenable = 1\ncode_side = offer\n"), None);
        assert_eq!(side_of("[netplay]\nenable = 1\ncode_side = host\n"), None);
        assert_eq!(
            side_of("[netplay]\nenable = 1\ncode_side = offer\ncode_dir =\n"),
            None
        );
        assert_eq!(side_of("[netplay\nenable = 1\ncode_side = offer\n"), None);
        assert_eq!(side_of(""), None);
    }

    #[test]
    fn a_game_menu_ini_opens_no_window_and_names_the_next_step() {
        let game = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = game\nclipboard = file\ncode_dir = x\n";
        assert!(is_game_menu(game));
        assert_eq!(side_of(game), None, "game はウィンドウの形ではない");
        let (dll_dir, game_dir) = (Path::new(r"C:\tool\bin"), Path::new(r"C:\games\th09"));
        assert_eq!(plan(Some(game), dll_dir, game_dir, Lang::Ja), Ok(None));
        for ini in [
            INI_OFFER,
            "[netplay]\nenable = 0\ncode_side = game\n",
            "[netplay]\ncode_side = game\n",
            "[pacing]\nenable = 1\ncode_side = game\n",
            "[netplay]\nenable = 1\ncode_side = Game\n",
            "[netplay\nenable = 1\ncode_side = game\n",
            "",
        ] {
            assert!(!is_game_menu(ini), "{ini:?}");
        }
        assert_eq!(
            GAME_MENU_HINT.ja,
            "ゲームの中の Match Mode →「妖怪 対 妖怪」から接続します。"
        );
        assert_eq!(
            GAME_MENU_HINT.en, "Connect from Match Mode > 妖怪 対 妖怪 in the game.",
            "英語の文に日本語の括弧を使わない（ゲームの中の英語の文と同じ書き方）"
        );
    }

    #[test]
    fn ini_is_located_like_the_dll() {
        let dll = Path::new(r"C:\tool\bin");
        let flag = Path::new(r"C:\s\two_host.ini");
        assert_eq!(
            ini_location(Some(flag), Some(OsStr::new(r"C:\x\y.ini")), dll),
            Some(flag.to_path_buf())
        );
        assert_eq!(
            ini_location(None, Some(OsStr::new(r"C:\x\y.ini")), dll),
            Some(PathBuf::from(r"C:\x\y.ini"))
        );
        assert_eq!(
            ini_location(None, None, dll),
            Some(PathBuf::from(r"C:\tool\bin\HiganRTool.ini"))
        );
        assert_eq!(ini_location(None, Some(OsStr::new("")), dll), None);
        assert_eq!(
            ini_location(None, Some(OsStr::new(r"rel\y.ini")), dll),
            None
        );
    }

    #[test]
    fn plan_refuses_a_code_dir_in_the_game_folder() {
        let dll = Path::new(r"C:\tool\bin");
        let game = Path::new(r"C:\g\game");
        let p = plan(Some(INI_OFFER), dll, game, Lang::Ja).unwrap().unwrap();
        assert_eq!(
            p,
            Plan {
                side: Side::Offer,
                dir: PathBuf::from(r"C:\tool\bin\code")
            }
        );
        let with = |d: &str| format!("{INI_OFFER}code_dir = {d}\n");
        assert_eq!(
            plan(Some(&with("code_a")), dll, game, Lang::Ja)
                .unwrap()
                .unwrap()
                .dir,
            PathBuf::from(r"C:\tool\bin\code_a")
        );
        assert_eq!(
            plan(Some(&with(r"D:\x\codes")), dll, game, Lang::Ja)
                .unwrap()
                .unwrap()
                .dir,
            PathBuf::from(r"D:\x\codes")
        );
        assert!(plan(Some(&with(r"C:\g\game")), dll, game, Lang::Ja).is_err());
        assert!(plan(Some(&with(r"c:/G/Game/code")), dll, game, Lang::Ja).is_err());
        assert!(plan(Some(INI_OFFER), game, game, Lang::Ja).is_err());
        assert!(plan(Some(&with(r"C:\g\game2")), dll, game, Lang::Ja).is_ok());
        assert_eq!(
            plan(Some("[netplay]\nenable = 0\n"), dll, game, Lang::Ja),
            Ok(None)
        );
        assert_eq!(plan(None, dll, game, Lang::Ja), Ok(None));
    }

    #[test]
    fn my_code_is_shown_when_it_appears_or_changes_and_gone_only_after_shown() {
        let before = Before {
            my_code: Some("STALE".to_string()),
            status: None,
        };
        let mut w = Watch::new(before);
        assert_eq!(w.step(Some(b"STALE\r\n"), None), vec![]);
        // 返す側の dll が開くときに前の回のものを消した: 終わりではない。
        assert_eq!(w.step(None, None), vec![]);
        assert_eq!(
            w.step(Some("\u{FEFF}AAAA\r\n".as_bytes()), None),
            vec![Event::MyCode {
                text: "AAAA".to_string(),
                renewed: false
            }]
        );
        assert_eq!(w.shown(), Some("AAAA"));
        assert_eq!(w.step(Some(b"AAAA\r\n"), None), vec![]);
        assert_eq!(w.step(Some(b""), None), vec![]);
        assert_eq!(w.step(Some(b"  \r\n"), None), vec![]);
        assert_eq!(
            w.step(Some(b"BBBB\r\n"), None),
            vec![Event::MyCode {
                text: "BBBB".to_string(),
                renewed: true
            }]
        );
        assert_eq!(w.step(None, None), vec![Event::MyCodeGone]);
        assert_eq!(w.step(None, None), vec![]);
    }

    #[test]
    fn status_is_shown_once_per_change_and_before_the_end() {
        let old =
            "HiganRTool の接続コード（返す側 answer）。開いてから 1.0 秒の時点:\r\n前の回\r\n";
        let mut w = Watch::new(Before {
            my_code: None,
            status: Some(old.as_bytes().to_vec()),
        });
        assert_eq!(w.step(None, Some(old.as_bytes())), vec![]);
        let s1 =
            "HiganRTool の接続コード（返す側 answer）。開いてから 2.5 秒の時点:\r\n読めない\r\n";
        assert_eq!(
            w.step(None, Some(s1.as_bytes())),
            vec![Event::Status("読めない".to_string())]
        );
        assert_eq!(w.step(None, Some(s1.as_bytes())), vec![]);
        assert_eq!(w.step(None, None), vec![]);
        w.step(Some(b"CODE"), None);
        let s2 = "見出し:\r\n繋がった。my_code.txt を消した\r\n";
        assert_eq!(
            w.step(None, Some(s2.as_bytes())),
            vec![
                Event::Status("繋がった。my_code.txt を消した".to_string()),
                Event::MyCodeGone
            ]
        );
        assert_eq!(status_line(b"a\r\nb\r\n"), Some("a / b".to_string()));
        assert_eq!(
            status_line("見出し：\nだけ".as_bytes()),
            Some("だけ".to_string())
        );
        assert_eq!(status_line(b"only:"), Some("only:".to_string()));
        assert_eq!(status_line(b" \r\n"), None);
    }

    #[test]
    fn pasted_text_is_written_trimmed_unless_empty_own_or_same() {
        assert_eq!(judge_paste("", None, None), Paste::Ignore);
        assert_eq!(
            judge_paste(" \t\u{FEFF} ", Some("AAAA"), None),
            Paste::Ignore
        );
        assert_eq!(
            judge_paste("  AAAA ", Some("AAAA"), None),
            Paste::Refuse(REFUSE_OWN)
        );
        assert_eq!(
            judge_paste("PEER", Some("AAAA"), Some("PEER")),
            Paste::Refuse(REFUSE_SAME)
        );
        assert_eq!(
            judge_paste("\u{FEFF} PEER2 \r", Some("AAAA"), Some("PEER")),
            Paste::Write("PEER2".to_string())
        );
        assert_eq!(
            judge_paste("OFFERCODE", None, None),
            Paste::Write("OFFERCODE".to_string())
        );
    }

    #[test]
    fn peer_code_is_replaced_through_a_temporary_file() {
        let d = temp_dir("write").join("code");
        write_peer_code(&d, "PEER1", Lang::Ja).unwrap();
        assert_eq!(std::fs::read(d.join(PEER_CODE_FILE)).unwrap(), b"PEER1\r\n");
        write_peer_code(&d, "PEER2", Lang::Ja).unwrap();
        assert_eq!(std::fs::read(d.join(PEER_CODE_FILE)).unwrap(), b"PEER2\r\n");
        assert!(!d.join(PEER_TMP_FILE).exists(), "一時のファイルが残った");
        // 一時の名前をフォルダで塞ぐ: 書けないので断り、前の中身のまま。
        std::fs::create_dir(d.join(PEER_TMP_FILE)).unwrap();
        assert!(write_peer_code(&d, "PEER3", Lang::Ja).is_err());
        assert_eq!(std::fs::read(d.join(PEER_CODE_FILE)).unwrap(), b"PEER2\r\n");
        std::fs::remove_dir_all(d.parent().unwrap()).unwrap();
    }

    /// `my_code.txt` と `status.txt` は消さない。
    #[test]
    fn stale_peer_code_is_cleared_before_launch_and_reported() {
        let root = temp_dir("clear");
        let d = root.join("code");
        let plan = Plan {
            side: Side::Answer,
            dir: d.clone(),
        };
        let (b, note) = before_launch(&plan, Lang::Ja);
        assert_eq!((b, note), (Before::default(), None));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(before_launch(&plan, Lang::Ja).1, None);

        std::fs::write(d.join(PEER_CODE_FILE), "OLDOFFER\r\n").unwrap();
        std::fs::write(d.join(PEER_TMP_FILE), "OLDO").unwrap();
        std::fs::write(d.join(MY_CODE_FILE), "OLDREPLY\r\n").unwrap();
        std::fs::write(d.join(STATUS_FILE), "見出し:\r\n前\r\n").unwrap();
        let (b, note) = before_launch(&plan, Lang::Ja);
        assert_eq!(note, Some(format!("HiganRTool: {}", CLEARED_PEER.ja)));
        assert!(!d.join(PEER_CODE_FILE).exists(), "peer_code.txt が残った");
        assert!(
            !d.join(PEER_TMP_FILE).exists(),
            "peer_code.txt.tmp が残った"
        );
        assert!(d.join(MY_CODE_FILE).exists() && d.join(STATUS_FILE).exists());
        assert_eq!(b.my_code.as_deref(), Some("OLDREPLY"));
        std::fs::write(d.join(PEER_TMP_FILE), "X").unwrap();
        assert_eq!(clear_stale_peer(&d, Lang::Ja), Ok(true));
        assert_eq!(clear_stale_peer(&d, Lang::Ja), Ok(false));

        std::fs::create_dir(d.join(PEER_CODE_FILE)).unwrap();
        let (_, note) = before_launch(&plan, Lang::Ja);
        let note = note.expect("消せないのに黙った");
        assert!(note.contains("片づけられませんでした"), "{note}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    struct Fake {
        said: Vec<String>,
        copied: Vec<String>,
        game: Game,
        copy_fails: bool,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                said: Vec::new(),
                copied: Vec::new(),
                game: Game::Running,
                copy_fails: false,
            }
        }
        fn text(&self) -> String {
            self.said.join("\n")
        }
    }

    impl Host for Fake {
        fn game(&mut self) -> Game {
            self.game.clone()
        }
        fn copy(&mut self, text: &str) -> Result<(), String> {
            if self.copy_fails {
                return Err("合成の失敗".to_string());
            }
            self.copied.push(text.to_string());
            Ok(())
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn offer_flow_copies_the_code_writes_the_reply_and_ends_after_the_last_status() {
        let d = temp_dir("offer");
        std::fs::write(d.join(MY_CODE_FILE), "STALE\r\n").unwrap();
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja);
        assert!(r.intro().contains("相手に送り"), "{}", r.intro());
        let mut h = Fake::new();
        assert_eq!(r.tick(&mut h, &[]), None);
        assert!(
            h.copied.is_empty(),
            "起動の前の残りをコピーした: {:?}",
            h.copied
        );

        std::fs::write(d.join(MY_CODE_FILE), "OFFER1\r\n").unwrap();
        std::fs::write(
            d.join(STATUS_FILE),
            "見出し:\r\n自分の接続コードを書いた\r\n",
        )
        .unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        assert_eq!(h.copied, vec!["OFFER1".to_string()]);
        let t = h.text();
        assert!(t.contains("\nOFFER1\n"), "{t}");
        assert!(t.contains("クリップボードにコピーしました"), "{t}");
        assert!(t.contains("状態: 自分の接続コードを書いた"), "{t}");

        let n = h.said.len();
        assert_eq!(r.tick(&mut h, &lines(&["OFFER1", ""])), None);
        assert!(!d.join(PEER_CODE_FILE).exists(), "自分のコードを置いた");
        assert_eq!(h.said[n..], [format!("HiganRTool: {}", REFUSE_OWN.ja)]);

        assert_eq!(r.tick(&mut h, &lines(&["  REPLY1  "])), None);
        assert_eq!(
            std::fs::read(d.join(PEER_CODE_FILE)).unwrap(),
            b"REPLY1\r\n"
        );
        assert!(h.text().contains("相手のコードを受け取りました"));

        std::fs::remove_file(d.join(MY_CODE_FILE)).unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        std::fs::write(d.join(STATUS_FILE), "見出し:\r\n繋がった\r\n").unwrap();
        let mut end = None;
        for _ in 0..=TAIL_POLLS {
            end = r.tick(&mut h, &[]);
            if end.is_some() {
                break;
            }
        }
        assert_eq!(end, Some(End::Finished));
        let t = h.text();
        let st = t.find("状態: 繋がった").expect("最後の状態が出ていない");
        let fin = t.find("受け渡しを終えました").unwrap();
        assert!(st < fin, "{t}");
        assert_eq!(h.copied.len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn without_clipboard_shows_the_code_but_never_copies() {
        let d = temp_dir("noclip");
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja).without_clipboard();
        let mut h = Fake::new();
        assert_eq!(r.tick(&mut h, &[]), None);
        std::fs::write(d.join(MY_CODE_FILE), "OFFER1\r\n").unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        std::fs::write(d.join(MY_CODE_FILE), "OFFER2\r\n").unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        assert!(h.copied.is_empty(), "コピーした: {:?}", h.copied);
        let t = h.text();
        assert!(t.contains("\nOFFER1\n") && t.contains("\nOFFER2\n"), "{t}");
        assert_eq!(
            t.matches("クリップボードにはコピーしなかった（--no-clipboard）")
                .count(),
            2,
            "{t}"
        );
        assert!(!t.contains("クリップボードにコピーしました"), "{t}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn answer_flow_writes_before_the_dir_exists_and_ends_with_the_game() {
        let root = temp_dir("answer");
        let d = root.join("code_b");
        let plan = Plan {
            side: Side::Answer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja);
        let mut h = Fake::new();
        h.copy_fails = true;
        assert_eq!(r.tick(&mut h, &lines(&["OFFERCODE"])), None);
        assert_eq!(
            std::fs::read(d.join(PEER_CODE_FILE)).unwrap(),
            b"OFFERCODE\r\n"
        );
        std::fs::write(d.join(MY_CODE_FILE), "REPLY1\r\n").unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        let t = h.text();
        assert!(t.contains("\nREPLY1\n"), "{t}");
        assert!(
            t.contains("クリップボードにコピーできませんでした（合成の失敗）"),
            "{t}"
        );
        h.copy_fails = false;
        std::fs::write(d.join(MY_CODE_FILE), "REPLY2\r\n").unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        assert_eq!(h.copied, vec!["REPLY2".to_string()]);
        assert!(h.text().contains("新しくなりました"));
        h.game = Game::Ended;
        assert_eq!(r.tick(&mut h, &[]), Some(End::GameEnded));
        assert!(h.text().contains("ゲームが終了したため"));
        assert!(h.text().contains(CLEANED_UP_MY_CODE.ja), "{}", h.text());
        assert!(
            !d.join(MY_CODE_FILE).exists(),
            "ゲームが終わったのに my_code.txt が残った"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn leftover_my_code_is_removed_when_the_game_ends_or_goes_unknown() {
        let d = temp_dir("cleanup_ended");
        std::fs::write(d.join(MY_CODE_FILE), "SECRET1\r\n").unwrap();
        std::fs::write(d.join(PEER_CODE_FILE), "PEER\r\n").unwrap();
        std::fs::write(d.join(STATUS_FILE), "見出し:\r\n状態\r\n").unwrap();
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja);
        let mut h = Fake::new();
        h.game = Game::Ended;
        assert_eq!(r.tick(&mut h, &[]), Some(End::GameEnded));
        assert!(h.text().contains(CLEANED_UP_MY_CODE.ja), "{}", h.text());
        assert!(!d.join(MY_CODE_FILE).exists(), "my_code.txt が残った");
        assert!(
            d.join(PEER_CODE_FILE).exists() && d.join(STATUS_FILE).exists(),
            "関係ないファイルまで消した"
        );
        std::fs::remove_dir_all(&d).unwrap();

        let d = temp_dir("cleanup_none");
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja);
        let mut h = Fake::new();
        h.game = Game::Unknown("合成の理由".to_string());
        assert_eq!(r.tick(&mut h, &[]), Some(End::GameUnknown));
        assert!(
            !h.text().contains(CLEANED_UP_MY_CODE.ja),
            "無いのに消したと言った: {}",
            h.text()
        );
        std::fs::remove_dir_all(&d).unwrap();

        let d = temp_dir("cleanup_blocked");
        std::fs::create_dir(d.join(MY_CODE_FILE)).unwrap();
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja);
        let mut h = Fake::new();
        h.game = Game::Ended;
        assert_eq!(r.tick(&mut h, &[]), Some(End::GameEnded));
        assert!(
            h.text().contains("消せませんでした"),
            "消せないのに黙った: {}",
            h.text()
        );
        std::fs::remove_dir_all(&d).unwrap();

        let d = temp_dir("cleanup_finished");
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut r = Relay::new(plan, Before::read(&d), Lang::Ja);
        let mut h = Fake::new();
        // ウィンドウに出した（shown）コードが消えたときだけ終わりと見る。起動前の残り（STALE）は使わない。
        std::fs::write(d.join(MY_CODE_FILE), "FRESH\r\n").unwrap();
        assert_eq!(
            r.tick(&mut h, &[]),
            None,
            "自分のコードが現れたところで終わってしまった"
        );
        std::fs::remove_file(d.join(MY_CODE_FILE)).unwrap();
        let mut end = None;
        for _ in 0..=TAIL_POLLS {
            end = r.tick(&mut h, &[]);
            if end.is_some() {
                break;
            }
        }
        assert_eq!(end, Some(End::Finished));
        assert!(
            !h.text().contains(CLEANED_UP_MY_CODE.ja),
            "繋がって終わった経路で片づけの文を出した: {}",
            h.text()
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn run_returns_when_the_game_ends_even_if_nobody_pastes() {
        for keep_sender in [true, false] {
            let root = temp_dir(&format!("run_{keep_sender}"));
            let dir = root.join("code");
            let (done_tx, done_rx) = mpsc::channel();
            let (tx, rx) = mpsc::channel::<String>();
            let held = keep_sender.then_some(tx);
            let th = std::thread::spawn(move || {
                let mut r = Relay::new(
                    Plan {
                        side: Side::Answer,
                        dir,
                    },
                    Before::default(),
                    Lang::Ja,
                );
                let mut h = Fake::new();
                h.game = Game::Ended;
                let end = run(&mut r, &mut h, &rx, Duration::from_millis(10));
                let _ = done_tx.send(end);
            });
            let end = done_rx.recv_timeout(Duration::from_secs(5));
            assert_eq!(end, Ok(End::GameEnded), "keep_sender={keep_sender}");
            th.join().unwrap();
            drop(held);
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn a_pasted_line_is_written_without_waiting_for_the_poll() {
        let root = temp_dir("paste_now");
        let dir = root.join("code");
        let (tx, rx) = mpsc::channel::<String>();
        tx.send("PEERNOW".to_string()).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let d2 = dir.clone();
        let th = std::thread::spawn(move || {
            let mut r = Relay::new(
                Plan {
                    side: Side::Answer,
                    dir: d2,
                },
                Before::default(),
                Lang::Ja,
            );
            let mut h = Fake::new();
            h.game = Game::Ended;
            let end = run(&mut r, &mut h, &rx, Duration::from_secs(10));
            let _ = done_tx.send(end);
        });
        let end = done_rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(end, Ok(End::GameEnded));
        assert_eq!(
            std::fs::read(dir.join(PEER_CODE_FILE)).unwrap(),
            b"PEERNOW\r\n"
        );
        th.join().unwrap();
        drop(tx);
        std::fs::remove_dir_all(&root).unwrap();
    }

    struct Countdown {
        said: Vec<String>,
        left: u32,
        seen: u32,
    }

    impl Host for Countdown {
        fn game(&mut self) -> Game {
            self.seen += 1;
            if self.left == 0 {
                Game::Ended
            } else {
                self.left -= 1;
                Game::Running
            }
        }
        fn copy(&mut self, _text: &str) -> Result<(), String> {
            panic!("受け渡しの後にコピーした");
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
    }

    #[test]
    fn linger_keeps_the_window_until_the_game_ends() {
        let (tx, rx) = mpsc::channel();
        tx.send("遅れて貼ったコード".to_string()).unwrap();
        tx.send("  ".to_string()).unwrap();
        let mut h = Countdown {
            said: Vec::new(),
            left: 3,
            seen: 0,
        };
        assert_eq!(
            linger(&mut h, &rx, Duration::from_millis(1), Lang::Ja),
            End::GameEnded
        );
        assert_eq!(h.seen, 4, "動いている 3 回と終わった 1 回");
        assert_eq!(h.said.first().map(String::as_str), Some(LINGER.ja));
        assert_eq!(
            h.said
                .iter()
                .filter(|l| l.as_str() == AFTER_FINISH.ja)
                .count(),
            1,
            "空白だけの行には答えない: {:?}",
            h.said
        );
        drop(tx);
        let mut h2 = Countdown {
            said: Vec::new(),
            left: 2,
            seen: 0,
        };
        assert_eq!(
            linger(&mut h2, &rx, Duration::from_millis(1), Lang::Ja),
            End::GameEnded
        );
        assert_eq!(h2.seen, 3);
        let (_tx3, rx3) = mpsc::channel::<String>();
        let mut h3 = Fake::new();
        h3.game = Game::Unknown("合成の理由".to_string());
        assert_eq!(
            linger(&mut h3, &rx3, Duration::from_millis(1), Lang::Ja),
            End::GameUnknown
        );
    }

    fn for_people(line: &str) {
        for w in [
            MY_CODE_FILE,
            PEER_CODE_FILE,
            "網",
            "候補",
            "秘密",
            "session",
            "127.0.0.1",
            "192.168.",
            "{",
            "(",
        ] {
            assert!(!line.contains(w), "ウィンドウの文に {w:?} が出た: {line}");
        }
        for w in ["本人", "判例", "WIP", "§", "【未】", "STATE.md"] {
            assert!(!line.contains(w), "内部の語 {w:?} が出た: {line}");
        }
        // ウィンドウ・コピー・アドレス・準備と書く。
        for w in ["窓", "写", "番地", "支度", "版"] {
            assert!(!line.contains(w), "直訳寄りの和語 {w:?} が出た: {line}");
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_main_log_is_found_by_the_process_number() {
        let all = names(&[
            "dll_20260929-091738-034_pid2944.log",
            "dll_20260929-091738-034_pid2944_replay.log",
            "dll_20260929-091738-034_pid2944_ticks.csv",
            "dll_20260929-091738-034_pid2944_net_frames.csv.gz",
            "dll_20260929-091738-518_pid45288.log",
            "dll_20260929-100000-001_pid12.log",
            "dll_20260929-100000-002_pid123.log",
            "readme.log",
        ]);
        assert_eq!(
            find_main_log(&all, 2944).as_deref(),
            Some("dll_20260929-091738-034_pid2944.log")
        );
        assert_eq!(
            find_main_log(&all, 12).as_deref(),
            Some("dll_20260929-100000-001_pid12.log")
        );
        assert_eq!(
            find_main_log(&all, 123).as_deref(),
            Some("dll_20260929-100000-002_pid123.log")
        );
        assert_eq!(find_main_log(&all, 45).as_deref(), None);
        assert_eq!(find_main_log(&[], 2944), None);
        let again = names(&[
            "dll_20260929-091738-034_pid7_3.log",
            "dll_20260929-091738-034_pid7.log",
            "dll_20260929-091738-034_pid7_2.log",
        ]);
        assert_eq!(
            find_main_log(&again, 7).as_deref(),
            Some("dll_20260929-091738-034_pid7_3.log")
        );
    }

    #[test]
    fn the_send_files_line_names_the_log_and_the_csv() {
        let log = "dll_20260929-091738-034_pid2944.log";
        let csv = "dll_20260929-091738-034_pid2944_net_frames.csv.gz";
        let both = send_files_line(Some(log), Some(csv), Lang::Ja);
        assert_eq!(
            both,
            "HiganRTool: 送ってほしいファイル: logs\\dll_20260929-091738-034_pid2944.log と logs\\dll_20260929-091738-034_pid2944_net_frames.csv.gz"
        );
        let only = send_files_line(Some(log), None, Lang::Ja);
        assert_eq!(
            only,
            "HiganRTool: 送ってほしいファイル: logs\\dll_20260929-091738-034_pid2944.log"
        );
        let none = send_files_line(None, None, Lang::Ja);
        assert!(
            none.contains("logs フォルダの一番新しい dll_…log")
                && none.contains("_net_frames.csv.gz"),
            "{none}"
        );
        for l in [&both, &only, &none] {
            for_people(l);
        }
    }

    #[test]
    fn the_send_files_line_is_built_from_the_logs_folder() {
        let d = temp_dir("logs");
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        for n in [
            "dll_20260929-091738-034_pid2944.log",
            "dll_20260929-091738-034_pid2944_net_frames.csv.gz",
            "dll_20260929-091738-518_pid45288.log",
        ] {
            std::fs::write(logs.join(n), "x").unwrap();
        }
        let both = send_files_for(&logs, 2944, Lang::Ja);
        assert!(
            both.contains("logs\\dll_20260929-091738-034_pid2944.log と logs\\dll_20260929-091738-034_pid2944_net_frames.csv.gz"),
            "{both}"
        );
        let no_csv = send_files_for(&logs, 45288, Lang::Ja);
        assert!(
            no_csv.ends_with("logs\\dll_20260929-091738-518_pid45288.log"),
            "{no_csv}"
        );
        let nothing = send_files_for(&logs, 1, Lang::Ja);
        assert!(nothing.contains("一番新しい"), "{nothing}");
        let missing = send_files_for(&d.join("no_such_folder"), 2944, Lang::Ja);
        assert!(missing.contains("一番新しい"), "{missing}");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn run_block(pid: u32, stem: &str, body: &str) -> String {
        format!(
            "{}\nウィンドウ: 設置\n{}（本ログだけ）\n{body}",
            higanr_core::log_names::run_header(pid, "2026-10-13 12:00:00.000", stem),
            higanr_core::log_cfg::BRIEF_MARK
        )
    }

    /// 壊れたら落ちる: 日ごとの本ログ（同じ日の複数の実行が並ぶ）で、自分の実行でない行の「うまくいかなかった」印を拾う・
    /// 自分の実行の CSV の名前を日ごとの名前から作る・前の日のファイルの実行を探さない・PID の最後の実行でなく最初を使う。
    #[test]
    fn the_daily_log_is_read_by_the_run_of_the_pid() {
        let d = temp_dir("daily_logs");
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        let stem = |pid: u32| format!("dll_20261013-1200{pid:02}-000_pid{pid}");
        let today = [
            run_block(10, &stem(10), "× 同期: CHECK がずれた\n"),
            run_block(20, &stem(20), "〇 同期\n"),
            run_block(30, &stem(30), "★★★ コピーの表を置かない\n"),
            run_block(20, &stem(21), "〇 同期（同じ PID の 2 回目）\n"),
        ]
        .concat();
        std::fs::write(logs.join("dll_20261013.log"), today).unwrap();
        std::fs::write(
            logs.join("dll_20261012.log"),
            run_block(40, &stem(40), "× 前の日\n"),
        )
        .unwrap();
        for n in [stem(21), stem(40)] {
            std::fs::write(logs.join(format!("{n}_net_frames.csv.gz")), "x").unwrap();
        }
        std::fs::write(logs.join(format!("{}_net_frames.csv.gz", stem(20))), "x").unwrap();
        let hint = |s: &str| s.contains("detail = 1");

        let clean = send_files_for(&logs, 20, Lang::Ja);
        assert!(!hint(&clean), "他の実行の × を拾った: {clean}");
        assert!(
            clean.ends_with(&format!(
                "logs\\dll_20261013.log と logs\\{}_net_frames.csv.gz",
                stem(21)
            )),
            "同じ PID の最後の実行の CSV を指す: {clean}"
        );
        for pid in [10, 30] {
            let s = send_files_for(&logs, pid, Lang::Ja);
            assert!(hint(&s), "pid {pid}: {s}");
            assert!(s.contains("logs\\dll_20261013.log"), "{s}");
            assert!(!s.contains("_net_frames"), "CSV が無い実行: {s}");
        }
        let yesterday = send_files_for(&logs, 40, Lang::Ja);
        assert!(hint(&yesterday), "{yesterday}");
        assert!(
            yesterday.contains("logs\\dll_20261012.log と logs\\"),
            "{yesterday}"
        );
        let none = send_files_for(&logs, 99, Lang::Ja);
        assert!(none.contains("一番新しい") && !hint(&none), "{none}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 壊れたら落ちる: 日ごとの本ログの末尾だけを読んで、上限より前にある実行を見失う、または実行の本文を上限なしに読む。
    #[test]
    fn a_run_far_from_the_tail_of_a_big_daily_log_is_found_and_read_up_to_the_limit() {
        let d = temp_dir("daily_big");
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        let filler = "詳細の行 0123456789\n".repeat((LOG_READ_MAX as usize) / 20 + 100);
        let mut text = run_block(5, "dll_20261013-100000-000_pid5", "× 古い実行\n");
        text.push_str(&filler);
        text.push_str(&run_block(
            6,
            "dll_20261013-110000-000_pid6",
            "× 新しい実行\n",
        ));
        text.push_str(&filler);
        text.push_str(&run_block(7, "dll_20261013-120000-000_pid7", &filler));
        text.push_str("× 上限の外\n");
        std::fs::write(logs.join("dll_20261013.log"), text).unwrap();
        for pid in [5, 6] {
            let s = send_files_for(&logs, pid, Lang::Ja);
            assert!(s.contains("detail = 1"), "pid {pid}: {s}");
        }
        let long = read_daily_run(&logs.join("dll_20261013.log"), 7).unwrap();
        assert!(long.starts_with("==== 起動 pid 7 "), "実行の先頭から読む");
        assert!(
            (long.len() as u64) < LOG_READ_MAX + 100 && !long.contains("上限の外"),
            "実行の本文を上限で止める: {} バイト",
            long.len()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 壊れたら落ちる: 日ごとの本ログを書いている最中（DLL と同じ共有の指定で開いた書き込みの持ち主がいる）に、ローダーが既定の共有で読めない。
    #[test]
    fn the_daily_log_is_readable_while_the_dll_holds_it_open() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_DELETE: u32 = 4;
        let d = temp_dir("daily_open");
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        let mut w = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(logs.join("dll_20261013.log"))
            .unwrap();
        w.write_all(run_block(8, "dll_20261013-130000-000_pid8", "× 書き途中\n").as_bytes())
            .unwrap();
        let s = send_files_for(&logs, 8, Lang::Ja);
        assert!(
            s.contains("detail = 1"),
            "書き込み中の本ログが読めない: {s}"
        );
        drop(w);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 壊れたら落ちる: 簡潔なログでうまくいかなかった回に「もっと詳しい記録が要るとき」の案内が付かない。何も無かった回・詳細な
    /// ログ・印の無いログに付く。案内が利用者向けの言葉の規則（内部の語・括弧）を破る。英語の回に日本語が混ざる。
    #[test]
    fn the_more_detail_hint_is_only_for_a_brief_log_of_a_run_that_went_wrong() {
        use higanr_core::log_cfg::{BRIEF_MARK, DETAIL_MARK};
        let d = temp_dir("brief_hint");
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        // ウィンドウとパッドの頼まれた行が先に並んだ後に印が来る形（印は 2 行目とは限らない）。
        let queued = "ウィンドウ: 設置\npad: 設置\n".repeat(30);
        let brief = |tail: &str| format!("{queued}{BRIEF_MARK}（本ログだけ）\n{tail}");
        let logs_by_pid: Vec<(u32, String)> = vec![
            (1, brief("〇 パニック: 受けていない\n")),
            (2, brief("× 同期: CHECK がずれた\n")),
            (3, brief("終了: パニックを受けて止めた（x）\n")),
            (4, brief("ネットワーク: CHECK のズレ: 自分の CHECK\n")),
            (5, brief("★★★ コピーの表を置かない\n")),
            (
                6,
                format!("{DETAIL_MARK}（CSV も書く）\n× 同期: CHECK がずれた\n"),
            ),
            (7, "印の無い古いログ\n× 同期\n".to_string()),
        ];
        for (pid, body) in &logs_by_pid {
            std::fs::write(
                logs.join(format!("dll_20260929-091738-034_pid{pid}.log")),
                body,
            )
            .unwrap();
        }
        for pid in [2, 3, 4, 5] {
            let s = send_files_for(&logs, pid, Lang::Ja);
            assert!(
                s.contains('\n') && s.contains("detail = 1"),
                "pid {pid}: {s}"
            );
            for l in s.lines() {
                for_people(l);
            }
            let en = send_files_for(&logs, pid, Lang::En);
            assert!(en.contains("detail = 1") && !has_japanese(&en), "{en}");
        }
        for pid in [1, 6, 7] {
            let s = send_files_for(&logs, pid, Lang::Ja);
            assert!(!s.contains("detail"), "pid {pid}: {s}");
            assert_eq!(s.lines().count(), 1, "{s}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_farewell_waits_for_enter_only_on_a_terminal() {
        let d = temp_dir("farewell");
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("dll_20260929-091738-034_pid9.log"), "x").unwrap();
        let (_tx, rx) = mpsc::channel::<String>();
        let mut h = Fake::new();
        farewell(&mut h, &rx, &logs, 9, false, Lang::Ja);
        assert_eq!(h.said.len(), 1, "{:?}", h.said);
        assert!(
            h.said[0].contains("dll_20260929-091738-034_pid9.log"),
            "{:?}",
            h.said
        );
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(String::new()).unwrap();
        let mut h = Fake::new();
        farewell(&mut h, &rx, &logs, 9, true, Lang::Ja);
        assert_eq!(h.said.len(), 2, "{:?}", h.said);
        assert_eq!(h.said[1], CLOSE_HINT.ja);
        for l in &h.said {
            for_people(l);
        }
        // Enter が遅れて届く: それまで返らない。
        let (tx2, rx2) = mpsc::channel::<String>();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = tx2.send(String::new());
        });
        let t0 = std::time::Instant::now();
        let mut h = Fake::new();
        farewell(&mut h, &rx2, &logs, 9, true, Lang::Ja);
        assert!(
            t0.elapsed() >= Duration::from_millis(150),
            "Enter を待たずに返った: {:?}",
            t0.elapsed()
        );
        late.join().unwrap();
        drop(tx);
        let mut h = Fake::new();
        farewell(&mut h, &rx, &logs, 9, true, Lang::Ja);
        assert_eq!(h.said.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---------------- 対戦の形の選択 ----------------

    const PID: u32 = 777;

    fn ini_shape(text: &str) -> Shape {
        mode_pick::ini_shape_of_text(text, "netplay")
            .unwrap()
            .unwrap()
    }

    fn mode_relay(dir: &Path, ini: &str) -> Relay {
        Relay::new(
            Plan {
                side: Side::Offer,
                dir: dir.to_path_buf(),
            },
            Before::default(),
            Lang::Ja,
        )
        .with_mode(ini_shape(&format!("[netplay]\n{ini}")), PID)
    }

    fn mode_file(dir: &Path) -> Option<mode_pick::FileRead> {
        std::fs::read(dir.join(MODE_FILE))
            .ok()
            .map(|b| mode_pick::read_for(&b, PID))
    }

    fn real_code() -> String {
        use higanr_core::auth::Secret;
        use higanr_core::candidate::Candidates;
        use higanr_core::code::Code;
        use higanr_core::handshake::Name;
        code::write_initiator(&Code {
            side: code::Side::Initiator,
            session_id: 0x5EED_C0DE,
            secret: Some(Secret([0x21; 16])),
            name: Name::new("t"),
            nat: higanr_core::nat::NatMark::UNKNOWN,
            candidates: Candidates::from_addrs(&["127.0.0.1:41001".parse().unwrap()]).unwrap(),
            expires: 1_900_000_000,
        })
        .unwrap()
    }

    #[test]
    fn typed_picks_are_written_with_the_pid_and_never_placed_as_a_peer_code() {
        let d = temp_dir("pick_write");
        let mut r = mode_relay(&d, "enable = 1\n");
        let mut h = Fake::new();
        for (typed, want, reply) in [
            (
                "2",
                mode_pick::Pick::Rollback(2),
                "rollback・ディレイ 2 フレームにしました",
            ),
            (
                " 0 ",
                mode_pick::Pick::Rollback(0),
                "rollback・ディレイ 0 フレームにしました",
            ),
            (
                "\u{FF11}",
                mode_pick::Pick::Rollback(1),
                "rollback・ディレイ 1 フレームにしました",
            ),
            (
                "8",
                mode_pick::Pick::Rollback(8),
                "rollback・ディレイ 8 フレームにしました",
            ),
            ("DeLay", mode_pick::Pick::Delay, "delay 方式にしました"),
            (
                "1",
                mode_pick::Pick::Rollback(1),
                "rollback・ディレイ 1 フレームにしました",
            ),
        ] {
            let n = h.said.len();
            assert_eq!(r.tick(&mut h, &lines(&[typed])), None);
            assert_eq!(
                mode_file(&d),
                Some(mode_pick::FileRead::Mine(want)),
                "{typed:?}"
            );
            assert_eq!(h.said[n..].len(), 1, "{typed:?}: {:?}", h.said);
            // 文全体で比べる（部分の一致では空白の抜けを見逃す）。
            assert_eq!(
                h.said[n],
                format!("HiganRTool: 同期方式を {reply}。"),
                "{typed:?}"
            );
            assert!(!h.said[n].contains("貼り直した"), "{}", h.said[n]);
            for l in &h.said {
                for_people(l);
            }
        }
        assert!(
            !d.join(PEER_CODE_FILE).exists(),
            "選択の文字を接続コードとして置いた"
        );
        assert!(!d.join(MODE_TMP_FILE).exists(), "一時のファイルが残った");
        let b = std::fs::read(d.join(MODE_FILE)).unwrap();
        assert_eq!(
            mode_pick::read_for(&b, PID + 1),
            mode_pick::FileRead::OtherRun { pid: PID }
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn invalid_lines_write_nothing_and_a_real_code_still_passes() {
        let d = temp_dir("pick_bad");
        let mut r = mode_relay(&d, "enable = 1\n");
        let mut h = Fake::new();
        assert_eq!(r.tick(&mut h, &lines(&["1"])), None);
        let before = std::fs::read(d.join(MODE_FILE)).unwrap();
        let n = h.said.len();
        for bad in [
            "9",
            "rollback",
            "d",
            "10",
            // auto はウィンドウでは受けない（マッチの中の数字キーだけ）。
            "auto",
            "AUTO",
            "delay 1",
            "-1",
            "x".repeat(code::MIN_CODE_CHARS - 1).as_str(),
        ] {
            let n0 = h.said.len();
            assert_eq!(r.tick(&mut h, &lines(&[bad])), None);
            assert_eq!(h.said.len(), n0 + 1, "{bad:?} に返事が無い");
            assert_eq!(h.said[n0], REFUSE_SHORT.ja, "{bad:?}");
        }
        assert!(
            REFUSE_SHORT.ja.contains("0〜8 の数字か delay") && !REFUSE_SHORT.ja.contains("auto"),
            "{}",
            REFUSE_SHORT.ja
        );
        let n2 = h.said.len();
        assert_eq!(r.tick(&mut h, &lines(&["", "  ", "\u{FEFF}"])), None);
        assert_eq!(h.said.len(), n2, "空行に返事を出した: {:?}", &h.said[n2..]);
        assert!(h.said.len() > n);
        assert_eq!(
            std::fs::read(d.join(MODE_FILE)).unwrap(),
            before,
            "不正な行が選択を書き換えた"
        );
        assert!(
            !d.join(PEER_CODE_FILE).exists(),
            "不正な行を接続コードとして置いた"
        );
        for l in &h.said {
            for_people(l);
        }
        let code = real_code();
        assert!(code.chars().count() >= code::MIN_CODE_CHARS);
        assert_eq!(r.tick(&mut h, &lines(&[&code])), None);
        assert_eq!(
            std::fs::read(d.join(PEER_CODE_FILE)).unwrap(),
            format!("{code}\r\n").into_bytes()
        );
        assert_eq!(std::fs::read(d.join(MODE_FILE)).unwrap(), before);
        // ちょうど MIN_CODE_CHARS の文字は短すぎとは断らない（文字の誤りは dll が見る）。
        let edge = "A".repeat(code::MIN_CODE_CHARS);
        assert_eq!(r.tick(&mut h, &lines(&[&edge])), None);
        assert_eq!(
            std::fs::read(d.join(PEER_CODE_FILE)).unwrap(),
            format!("{edge}\r\n").into_bytes()
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_window_without_the_mode_choice_keeps_the_old_paste_and_a_late_pick_says_when_it_works() {
        let d = temp_dir("pick_old");
        let plan = || Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        let mut old = Relay::new(plan(), Before::default(), Lang::Ja);
        let mut h = Fake::new();
        assert_eq!(old.tick(&mut h, &lines(&["0"])), None);
        assert_eq!(std::fs::read(d.join(PEER_CODE_FILE)).unwrap(), b"0\r\n");
        assert!(
            !d.join(MODE_FILE).exists(),
            "選べないウィンドウが mode.txt を書いた"
        );
        assert!(!old.intro().contains("同期方式"), "{}", old.intro());
        std::fs::remove_file(d.join(PEER_CODE_FILE)).unwrap();

        let mut r = mode_relay(&d, "enable = 1\n");
        let mut h = Fake::new();
        assert_eq!(r.tick(&mut h, &lines(&["1"])), None);
        assert!(!h.said[0].contains("貼り直した"), "{}", h.said[0]);
        assert_eq!(r.tick(&mut h, &lines(&[&real_code()])), None);
        assert_eq!(r.tick(&mut h, &lines(&["delay"])), None);
        let last = h.said.last().unwrap();
        assert!(
            last.contains("delay 方式にしました")
                && last.contains(
                    "相手のコードをもう受け取ったので、この接続では変わりません。変えるなら 2 人とも起動し直してください。"
                ),
            "{last}"
        );
        std::fs::remove_file(d.join(PEER_CODE_FILE)).unwrap();
        assert_eq!(r.tick(&mut h, &lines(&[&real_code()])), None);
        assert!(
            !d.join(PEER_CODE_FILE).exists(),
            "同じコードの貼り直しが置かれた"
        );
        assert_eq!(
            mode_file(&d),
            Some(mode_pick::FileRead::Mine(mode_pick::Pick::Delay))
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_intro_names_the_ini_shape_and_a_rollback_pick_on_a_delay_ini_is_refused_in_words() {
        let d = temp_dir("pick_intro");
        let r = mode_relay(&d, "enable = 1\n");
        let intro = r.intro();
        let line = intro.lines().last().unwrap();
        assert_eq!(
            line,
            "HiganRTool: 同期方式: rollback・ディレイ 1 フレーム。変えるなら、相手のコードを貼る前に 0〜8 の数字（rollback のディレイのフレーム数）か delay と打って Enter を押してください（rollback は 2 人とも選んだときだけ。ディレイはホストの値を使います）。"
        );
        for l in intro.lines() {
            for_people(l);
        }
        let r = mode_relay(&d, "enable = 1\ndelay = 2\n");
        assert!(
            r.intro()
                .contains("同期方式: rollback・ディレイ 2 フレーム。")
        );
        let mut r = mode_relay(&d, "enable = 1\nmode = delay\ndelay = 0\n");
        assert_eq!(
            r.intro().lines().last().unwrap(),
            "HiganRTool: 同期方式: delay 方式。"
        );
        let mut h = Fake::new();
        assert_eq!(r.tick(&mut h, &lines(&["2"])), None);
        let said = &h.said[0];
        assert!(
            said.contains("rollback にはなりません")
                && said.contains("delay 方式で接続します")
                && !said.contains("にしました"),
            "{said}"
        );
        for_people(said);
        // 書くのは書く（dll が本ログに残し、理由を状態の行に出す）。
        assert_eq!(
            mode_file(&d),
            Some(mode_pick::FileRead::Mine(mode_pick::Pick::Rollback(2)))
        );
        assert_eq!(r.tick(&mut h, &lines(&["delay"])), None);
        assert!(h.said[1].contains("delay 方式にしました"), "{}", h.said[1]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_stale_mode_file_is_cleared_before_launch_silently() {
        let root = temp_dir("pick_stale");
        let d = root.join("code");
        std::fs::create_dir_all(&d).unwrap();
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        std::fs::write(
            d.join(MODE_FILE),
            mode_pick::render(PID, mode_pick::Pick::Delay),
        )
        .unwrap();
        std::fs::write(d.join(MODE_TMP_FILE), "x").unwrap();
        let (_, note) = before_launch(&plan, Lang::Ja);
        assert_eq!(note, None, "mode.txt を消したのにウィンドウへ出した");
        assert!(!d.join(MODE_FILE).exists() && !d.join(MODE_TMP_FILE).exists());
        std::fs::write(d.join(PEER_CODE_FILE), "OLD\r\n").unwrap();
        std::fs::write(d.join(MODE_FILE), "x").unwrap();
        let (_, note) = before_launch(&plan, Lang::Ja);
        assert_eq!(note, Some(format!("HiganRTool: {}", CLEARED_PEER.ja)));
        std::fs::create_dir(d.join(MODE_FILE)).unwrap();
        let (_, note) = before_launch(&plan, Lang::Ja);
        let note = note.expect("消せないのに黙った");
        assert!(
            note.contains("同期方式の選択を片づけられませんでした"),
            "{note}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_mode_file_is_replaced_through_a_temporary_file() {
        let root = temp_dir("pick_replace");
        let d = root.join("code");
        write_mode_file(&d, PID, mode_pick::Pick::Rollback(2), Lang::Ja).unwrap();
        assert!(!d.join(MODE_TMP_FILE).exists());
        let first = std::fs::read(d.join(MODE_FILE)).unwrap();
        std::fs::create_dir(d.join(MODE_TMP_FILE)).unwrap();
        assert!(write_mode_file(&d, PID, mode_pick::Pick::Delay, Lang::Ja).is_err());
        assert_eq!(std::fs::read(d.join(MODE_FILE)).unwrap(), first);
        let mut r = mode_relay(&d, "enable = 1\n");
        let mut h = Fake::new();
        assert_eq!(r.tick(&mut h, &lines(&["delay"])), None);
        assert!(
            h.said[0].contains("同期方式を保存できませんでした"),
            "{}",
            h.said[0]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn english_run_says_every_window_line_in_english() {
        let d = temp_dir("english");
        let mut r = Relay::new(
            Plan {
                side: Side::Offer,
                dir: d.clone(),
            },
            Before::default(),
            Lang::En,
        )
        .with_mode(ini_shape("[netplay]\nenable = 1\n"), PID);
        let intro = r.intro();
        assert!(
            intro.contains("send it to your opponent")
                && intro.contains("Netcode: rollback, Delay 1 frame. To change it"),
            "{intro}"
        );
        let mut h = Fake::new();
        std::fs::write(d.join(MY_CODE_FILE), format!("{}\r\n", real_code())).unwrap();
        std::fs::write(d.join(STATUS_FILE), "Header:\r\nWrote my code\r\n").unwrap();
        assert_eq!(r.tick(&mut h, &[]), None);
        assert_eq!(r.tick(&mut h, &lines(&[&real_code(), "x", "2"])), None);
        let peer = "A".repeat(code::MIN_CODE_CHARS);
        assert_eq!(r.tick(&mut h, &lines(&[&peer])), None);
        assert_eq!(r.tick(&mut h, &lines(&["delay"])), None);
        std::fs::remove_file(d.join(MY_CODE_FILE)).unwrap();
        let mut end = None;
        for _ in 0..=TAIL_POLLS + 1 {
            end = r.tick(&mut h, &[]);
            if end.is_some() {
                break;
            }
        }
        assert_eq!(end, Some(End::Finished));
        let t = h.text();
        for want in [
            "Your connection code:",
            "Copied to the clipboard",
            "Status: Wrote my code",
            "This is your own connection code",
            "Too short to be a connection code",
            "Netcode set to rollback, Delay 2 frames.",
            "Received your opponent's code",
            "Netcode set to delay. Your opponent's code was already received",
            "The connection code exchange is finished",
        ] {
            assert!(t.contains(want), "{want:?} が無い: {t}");
        }
        let (_tx, rx) = mpsc::channel::<String>();
        let mut ended = Fake::new();
        ended.game = Game::Ended;
        assert_eq!(
            linger(&mut ended, &rx, Duration::from_millis(1), Lang::En),
            End::GameEnded
        );
        let logs = d.join(LOGS_DIR);
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("dll_20260929-091738-034_pid9.log"), "x").unwrap();
        let (tx2, rx2) = mpsc::channel::<String>();
        tx2.send(String::new()).unwrap();
        farewell(&mut ended, &rx2, &logs, 9, true, Lang::En);
        assert!(ended.said[0].contains("This window closes when you close Touhou 9"));
        assert!(ended.said[1].starts_with("HiganRTool: File to send: logs\\dll_"));
        assert_eq!(ended.said[2], CLOSE_HINT.en);
        for l in h.said.iter().chain(&ended.said) {
            assert!(!has_japanese(l), "英語の回に日本語: {l}");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn english_cleanup_and_setup_notes_are_english() {
        let root = temp_dir("english_notes");
        let d = root.join("code");
        std::fs::create_dir_all(&d).unwrap();
        let plan = Plan {
            side: Side::Offer,
            dir: d.clone(),
        };
        std::fs::write(d.join(PEER_CODE_FILE), "OLD\r\n").unwrap();
        let (_, note) = before_launch(&plan, Lang::En);
        assert_eq!(
            note.as_deref(),
            Some("HiganRTool: Cleared the opponent's connection code from the previous run.")
        );
        std::fs::create_dir(d.join(PEER_CODE_FILE)).unwrap();
        let note = before_launch(&plan, Lang::En)
            .1
            .expect("消せないのに黙った");
        assert!(
            note.starts_with(
                "HiganRTool: Could not clear the opponent's code from the previous run ("
            ),
            "{note}"
        );

        std::fs::write(d.join(MY_CODE_FILE), "SECRET\r\n").unwrap();
        let mut r = Relay::new(plan, Before::read(&d), Lang::En);
        let mut h = Fake::new();
        h.game = Game::Ended;
        assert_eq!(r.tick(&mut h, &[]), Some(End::GameEnded));
        let t = h.text();
        assert!(
            t.contains(CLEANED_UP_MY_CODE.en) && t.contains("The game has ended"),
            "{t}"
        );
        assert!(!has_japanese(&t), "{t}");

        let host_game = "[netplay]\nenable = 1\ncode_side = offer\ncode_dir = x\n";
        let inside = plan_inside_game(host_game, Lang::En);
        assert!(
            inside.contains("inside the game folder") && !has_japanese(&inside),
            "{inside}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn plan_inside_game(ini: &str, lang: Lang) -> String {
        let game = Path::new(r"C:\g\game");
        let ini = ini.replace("code_dir = x", &format!("code_dir = {}", game.display()));
        plan(Some(&ini), Path::new(r"C:\tool\bin"), game, lang).unwrap_err()
    }
}
