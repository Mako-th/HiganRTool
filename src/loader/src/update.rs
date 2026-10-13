//! 起動前の更新（配るバージョンだけ）。花映塚を起こす前に署名つきの目録を取り、新しいバージョンがあればウィンドウで尋ね、「はい」なら
//! 必要なファイルを置き換えて新しい `HiganRTool.exe` を同じ引数で起こし直す。通信とウィンドウは `Net` / `Host` の向こう。
//!
//! - 確かめるのは配るバージョン（feature `dist`）の通常起動だけ（`skip_reason`）。ゲームのフォルダの中では何もしない。
//! - 待ちは全体で `CHECK_DEADLINE_MS`（約 2 秒）まで。通信は別スレッドで走らせ、時間が来たら起動へ進む。
//! - 置き換えは `<名前>.new` に書いて読み戻し、全部そろってから 1 つずつ「いまの物を `.old` へ改名 → `.new` を本来の名前へ」。
//!   動いている exe や読み込み中の dll は上書きできないが改名はできるため。途中で失敗したら逆順に戻し、自分の exe は最後に置く。
//! - `.old` は、新しいバージョンが花映塚を起こせた印（`note_launched`）を残した次の起動が消す。起こせなかったバージョンでは残るので手で戻せる。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use higanr_core::lang::Lang;
use higanr_core::update::{self, FileEntry, Manifest};

use crate::code_relay::is_same_or_inside;
use crate::game_pick;
use crate::http_win::{self, HttpError, Proxy, Timeouts};
use crate::ui_lang::by_lang;

/// 全体の上限（`CHECK_DEADLINE_MS`）より短くする。
const CHECK_TIMEOUTS: Timeouts = Timeouts {
    resolve_ms: 1500,
    connect_ms: 1500,
    send_ms: 1500,
    receive_ms: 1500,
};
/// 「はい」の後なので長くてよい。
const DOWNLOAD_TIMEOUTS: Timeouts = Timeouts {
    resolve_ms: 10_000,
    connect_ms: 10_000,
    send_ms: 10_000,
    receive_ms: 30_000,
};

pub trait Net {
    /// 待ちは自分で `CHECK_DEADLINE_MS` に収める。
    fn get_manifest(&mut self) -> Result<Vec<u8>, String>;
    /// `size` は目録の大きさ。越える応答は断る。
    fn get_file(&mut self, name: &str, size: u64) -> Result<Vec<u8>, String>;
}

pub trait Host {
    fn ask_update(&mut self, text: &str) -> bool;
    /// ウィンドウにも黒いウィンドウにも出す。
    fn tell(&mut self, text: &str);
    fn say(&mut self, line: &str);
    /// 起こせなければ Err（呼び出し側が全部戻す）。
    fn restart(&mut self, exe: &Path, args: &[OsString]) -> Result<(), String>;
}

pub struct Setup<'a> {
    /// 置き換えるファイルはここの直下だけ。
    pub dir: &'a Path,
    pub args: &'a [OsString],
    pub current: u32,
    pub key: [u8; 32],
    pub lang: Lang,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Proceed,
    Restarted,
}

/// 通信とウィンドウの失敗は全部 `Proceed`（花映塚の起動を止めない）。更新の途中の失敗は、元へ戻して知らせてから `Proceed`。
pub fn run(s: &Setup<'_>, net: &mut dyn Net, host: &mut dyn Host) -> Outcome {
    let lang = s.lang;
    let bytes = match net.get_manifest() {
        Ok(b) => b,
        Err(e) => {
            host.say(&by_lang!(
                lang,
                ja: "更新データが確認できませんでした（{e}）。起動します。",
                en: "Update check: failed ({e}). Starting without checking."
            ));
            return Outcome::Proceed;
        }
    };
    let manifest = match update::verify_and_parse(&bytes, &s.key) {
        Ok(m) => m,
        Err(e) => {
            host.say(&by_lang!(
                lang,
                ja: "更新データが不審でした（{e}）。無視して起動します。",
                en: "Update check: the update data looked invalid ({e}). Ignoring it and starting."
            ));
            return Outcome::Proceed;
        }
    };
    if !update::is_newer(s.current, manifest.version) {
        host.say(match lang {
            Lang::Ja => "更新の確認: 新しいバージョンはありません。",
            Lang::En => "Update check: no new version.",
        });
        return Outcome::Proceed;
    }
    if !host.ask_update(&ask_text(&manifest.display, lang)) {
        host.say(match lang {
            Lang::Ja => "更新の確認: 更新せずに起動します。",
            Lang::En => "Update check: starting without updating.",
        });
        return Outcome::Proceed;
    }
    let (display, count) = (&manifest.display, manifest.files.len());
    host.say(&by_lang!(
        lang,
        ja: "更新データがありました（{display}、{count} 個のファイル）。更新して起動します。",
        en: "Update: found {display} ({count} files). Updating, then starting."
    ));
    let files = match download_all(net, &manifest, host, lang) {
        Ok(f) => f,
        Err(why) => return give_up(host, &why, lang),
    };
    let placed = match replace_files(s.dir, &files, lang) {
        Ok(p) => p,
        Err(e) => return give_up(host, &e.describe(lang), lang),
    };
    host.say(match lang {
        Lang::Ja => "更新: 置き換えました。新しいバージョンで起動し直します。",
        Lang::En => "Update: replaced the files. Restarting with the new version.",
    });
    match host.restart(&s.dir.join(update::SELF_EXE), s.args) {
        Ok(()) => Outcome::Restarted,
        Err(why) => {
            let problems = undo(s.dir, &placed, lang);
            let mut text = by_lang!(
                lang,
                ja: "新しいバージョンを起動できませんでした（{why}）",
                en: "Could not start the new version ({why})"
            );
            if !problems.is_empty() {
                text.push_str(&not_restored(&problems, lang));
            }
            give_up(host, &text, lang)
        }
    }
}

/// 戻せなかったものの一覧を理由の末尾に足す文。
fn not_restored(problems: &[String], lang: Lang) -> String {
    let list = problems.join(" / ");
    by_lang!(
        lang,
        ja: "。戻せなかったもの: {list}",
        en: ". Could not restore: {list}"
    )
}

pub fn ask_text(display: &str, lang: Lang) -> String {
    match lang {
        Lang::Ja => format!(
            "HiganRTool の新しいバージョン（{display}）があります。\n\n更新しますか？\n\n\
             「はい」: 起動を取りやめて、必要なファイルを置き換え、新しいバージョンで起動し直します。設定（HiganRTool.ini）と logs はそのまま残ります。\n\
             「いいえ」: 今のバージョンのまま起動します。"
        ),
        Lang::En => format!(
            "A new version of HiganRTool ({display}) is available.\n\nUpdate now?\n\n\
             Yes: cancels this launch, replaces the needed files, and starts again with the new version. Your settings (HiganRTool.ini) and logs are kept.\n\
             No: starts with the current version."
        ),
    }
}

fn give_up(host: &mut dyn Host, why: &str, lang: Lang) -> Outcome {
    host.tell(&by_lang!(
        lang,
        ja: "更新できませんでした（{why}）。今のバージョンのまま起動します。",
        en: "Could not update ({why}). Starting with the current version."
    ));
    Outcome::Proceed
}

/// 全部落とし、大きさと SHA-256 を照らす。1 つでも合わなければ Err（何も置き換えない）。
fn download_all(
    net: &mut dyn Net,
    m: &Manifest,
    host: &mut dyn Host,
    lang: Lang,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::with_capacity(m.files.len());
    for (i, f) in m.files.iter().enumerate() {
        let (name, nth, total) = (&f.name, i + 1, m.files.len());
        host.say(&by_lang!(
            lang,
            ja: "更新データをダウンロードしています（{name}、{nth}/{total}）。",
            en: "Update: fetching {name} ({nth}/{total})."
        ));
        let data = net.get_file(&f.name, f.size).map_err(|e| {
            by_lang!(lang, ja: "{name} をダウンロードできませんでした: {e}", en: "Could not fetch {name}: {e}")
        })?;
        check_file(f, &data, lang)?;
        out.push((f.name.clone(), data));
    }
    Ok(out)
}

fn check_file(f: &FileEntry, data: &[u8], lang: Lang) -> Result<(), String> {
    let name = &f.name;
    if data.len() as u64 != f.size {
        let (want, got) = (f.size, data.len());
        return Err(by_lang!(
            lang,
            ja: "{name} の大きさが目録と違います（{want} バイトのはずが {got} バイト）",
            en: "The size of {name} does not match the update list ({got} bytes, expected {want})"
        ));
    }
    if update::sha256(data) != f.sha256 {
        return Err(by_lang!(
            lang,
            ja: "{name} の SHA-256 が目録と合いません",
            en: "The SHA-256 of {name} does not match the update list"
        ));
    }
    Ok(())
}

// ---- 置き換え ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub name: String,
    /// 置く前にあった物を `.old` に取ったか。
    pub had_old: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceError {
    pub reason: String,
    pub rollback_problems: Vec<String>,
}

impl ReplaceError {
    pub fn describe(&self, lang: Lang) -> String {
        let mut text = self.reason.clone();
        if !self.rollback_problems.is_empty() {
            text.push_str(&not_restored(&self.rollback_problems, lang));
        }
        text
    }
}

/// 自分の exe を最後に置く（失敗しても、前の分を全部戻せるうちに済ませるため）。
fn commit_order(files: &[(String, Vec<u8>)]) -> Vec<&(String, Vec<u8>)> {
    let (own, others): (Vec<_>, Vec<_>) = files.iter().partition(|(n, _)| n == update::SELF_EXE);
    others.into_iter().chain(own).collect()
}

/// `dir` の直下に置き換える。成功なら置いた分（起こし直しに失敗したら `undo` で戻す）。失敗なら済んだ分は戻し `.new` は消す。
/// 名前は `update::is_allowed_name` の規則に合うものだけ（1 つでも外れたら何も書かない）。
pub fn replace_files(
    dir: &Path,
    files: &[(String, Vec<u8>)],
    lang: Lang,
) -> Result<Vec<Placed>, ReplaceError> {
    let fail = |reason: String| ReplaceError {
        reason,
        rollback_problems: Vec::new(),
    };
    for (name, _) in files {
        if !update::is_allowed_name(name) {
            return Err(fail(by_lang!(
                lang,
                ja: "{name:?} は置き換えてよいファイル名ではありません",
                en: "{name:?} is not a file name that may be replaced"
            )));
        }
    }
    // `.new` に書いて読み戻す（書き損じや途中の差し替えを、置き換えの前に見つける）。
    for (name, data) in files {
        let new = dir.join(update::new_name(name));
        let shown = new.display();
        let staged = std::fs::write(&new, data)
            .map_err(|e| {
                by_lang!(lang, ja: "{shown} に書けません（{e}）", en: "Cannot write to {shown} ({e})")
            })
            .and_then(|()| {
                let back = std::fs::read(&new).map_err(|e| {
                    by_lang!(
                        lang,
                        ja: "{shown} を読み込めません（{e}）",
                        en: "Cannot read {shown} back ({e})"
                    )
                })?;
                (back == *data).then_some(()).ok_or_else(|| {
                    by_lang!(
                        lang,
                        ja: "{shown} が書いた中身と違います",
                        en: "{shown} differs from what was written"
                    )
                })
            });
        if let Err(reason) = staged {
            remove_staged(dir, files);
            return Err(fail(reason));
        }
    }
    let mut placed: Vec<Placed> = Vec::new();
    for (name, _) in commit_order(files) {
        match place_one(dir, name, lang) {
            Ok(had_old) => placed.push(Placed {
                name: name.clone(),
                had_old,
            }),
            Err(reason) => {
                let problems = undo(dir, &placed, lang);
                remove_staged(dir, files);
                return Err(ReplaceError {
                    reason,
                    rollback_problems: problems,
                });
            }
        }
    }
    Ok(placed)
}

/// 失敗するときは、このファイルの分は元に戻してから返す。
fn place_one(dir: &Path, name: &str, lang: Lang) -> Result<bool, String> {
    let target = dir.join(name);
    let old = dir.join(update::old_name(name));
    let new = dir.join(update::new_name(name));
    let (target_shown, old_shown) = (target.display(), old.display());
    // 前の更新の `.old` が残っていたら消す（読み込み中の dll などで消せないことがある）。
    if old.symlink_metadata().is_ok() {
        std::fs::remove_file(&old).map_err(|e| {
            by_lang!(
                lang,
                ja: "前の更新の {old_shown} を消せません（{e}）。別の HiganRTool か花映塚が使っていないか見てください",
                en: "Cannot delete {old_shown} left by the previous update ({e}). Check that no other HiganRTool or Touhou 9 is using it"
            )
        })?;
    }
    let had_old = target.symlink_metadata().is_ok();
    if had_old {
        std::fs::rename(&target, &old).map_err(|e| {
            by_lang!(
                lang,
                ja: "{target_shown} の名前を変えられません（{e}）",
                en: "Cannot rename {target_shown} ({e})"
            )
        })?;
    }
    if let Err(e) = std::fs::rename(&new, &target) {
        if had_old {
            // 取った物を戻す。戻せなければ元の物は `.old` に残るので、その旨を理由に足す。
            if let Err(e2) = std::fs::rename(&old, &target) {
                return Err(by_lang!(
                    lang,
                    ja: "{target_shown} を置き換えられません（{e}）。{old_shown} も元に戻せません（{e2}）",
                    en: "Cannot replace {target_shown} ({e}). {old_shown} also cannot be restored ({e2})"
                ));
            }
        }
        return Err(by_lang!(
            lang,
            ja: "{target_shown} を置き換えられません（{e}）",
            en: "Cannot replace {target_shown} ({e})"
        ));
    }
    Ok(had_old)
}

pub fn undo(dir: &Path, placed: &[Placed], lang: Lang) -> Vec<String> {
    let problem = |p: &Path, e: std::io::Error| {
        let shown = p.display();
        by_lang!(lang, ja: "{shown}（{e}）", en: "{shown} ({e})")
    };
    let mut problems = Vec::new();
    for p in placed.iter().rev() {
        let target = dir.join(&p.name);
        let old = dir.join(update::old_name(&p.name));
        if let Err(e) = std::fs::remove_file(&target) {
            problems.push(problem(&target, e));
            continue;
        }
        if p.had_old
            && let Err(e) = std::fs::rename(&old, &target)
        {
            problems.push(problem(&old, e));
        }
    }
    problems
}

fn remove_staged(dir: &Path, files: &[(String, Vec<u8>)]) {
    for (name, _) in files {
        let _ = std::fs::remove_file(dir.join(update::new_name(name)));
    }
}

/// `.new` を消し、`remove_old` なら `.old` も消す。消せなかったものは `retries` 回まで `wait` ずつ待って試し、残れば返す。
pub fn cleanup_leftovers(
    dir: &Path,
    retries: u32,
    wait: Duration,
    remove_old: bool,
) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for name in update::SHIPPED_FILES {
        targets.push(dir.join(update::new_name(name)));
        if remove_old {
            targets.push(dir.join(update::old_name(name)));
        }
    }
    let mut left = Vec::new();
    for path in targets {
        let mut tries = 0;
        while path.symlink_metadata().is_ok() {
            if std::fs::remove_file(&path).is_ok() {
                break;
            }
            tries += 1;
            if tries > retries {
                left.push(path.clone());
                break;
            }
            std::thread::sleep(wait);
        }
    }
    left
}

/// 起動のときの片づけ。ゲームのフォルダの中では触らない。`.old` は新しいバージョンが起こせた印を残した次の起動だけ消す。
/// `just_updated` では親の exe がまだ動いていることがあるので少し待つ。
pub fn clear_leftovers_at(
    dir: &Path,
    in_game: bool,
    just_updated: bool,
    build_number: u32,
) -> Vec<PathBuf> {
    if in_game {
        return Vec::new();
    }
    let (retries, wait) = if just_updated {
        (20, Duration::from_millis(100))
    } else {
        (0, Duration::ZERO)
    };
    let mark = std::fs::read_to_string(dir.join(update::LAUNCHED_MARK)).ok();
    let remove_old = update::launched_mark_matches(mark.as_deref(), build_number);
    cleanup_leftovers(dir, retries, wait, remove_old)
}

/// exe のフォルダに `th09.exe` があるか、`game_path.txt` が指すフォルダと同じかその下にあるとき。置き換えも片づけもしない。
pub fn in_game_folder(dir: &Path, saved_text: Option<&str>) -> bool {
    let has_exe = std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(game_pick::GAME_EXE)
        })
    });
    if has_exe {
        return true;
    }
    saved_text
        .and_then(game_pick::parse_saved)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .is_some_and(|game| is_same_or_inside(dir, &game))
}

/// 花映塚を起こせたことを印に残す（配るバージョンだけ）。書けなくても起動は止めない。
pub fn note_launched(lang: Lang) {
    if !this_build_is_dist() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    if let Err(e) = note_launched_at(dir, update::BUILD_NUMBER, lang) {
        eprintln!(
            "{}",
            by_lang!(
                lang,
                ja: "HiganRTool: {e}（次の起動で前のバージョンの .old が残ります）",
                en: "HiganRTool: {e} (the previous version's .old files will remain at the next launch)"
            )
        );
    }
}

/// 書いたら Ok(true)。ビルド番号 0・ゲームのフォルダの中なら書かず Ok(false)。
pub fn note_launched_at(dir: &Path, build_number: u32, lang: Lang) -> Result<bool, String> {
    if build_number == 0 {
        return Ok(false);
    }
    let saved = std::fs::read_to_string(dir.join(game_pick::SAVED_FILE)).ok();
    if in_game_folder(dir, saved.as_deref()) {
        return Ok(false);
    }
    let path = dir.join(update::LAUNCHED_MARK);
    std::fs::write(&path, update::launched_mark_text(build_number)).map_err(|e| {
        let shown = path.display();
        by_lang!(lang, ja: "{shown} に書けません（{e}）", en: "Cannot write to {shown} ({e})")
    })?;
    Ok(true)
}

// ---- 確かめに行くか ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    NotDist,
    InGameFolder,
    DevArgs,
    /// 更新が続く事故を止める歯止め。
    JustUpdated,
    InheritedIni,
    NoBuildNumber,
    OtherExeName,
    IniOff,
}

#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub dist: bool,
    pub in_game_folder: bool,
    pub dev_args: bool,
    pub just_updated: bool,
    pub inherited_ini: bool,
    pub build_number: u32,
    pub exe_name_ok: bool,
    pub ini_enabled: bool,
}

pub fn skip_reason(g: &Gate) -> Option<Skip> {
    if !g.dist {
        Some(Skip::NotDist)
    } else if g.in_game_folder {
        Some(Skip::InGameFolder)
    } else if g.dev_args {
        Some(Skip::DevArgs)
    } else if g.just_updated {
        Some(Skip::JustUpdated)
    } else if g.inherited_ini {
        Some(Skip::InheritedIni)
    } else if g.build_number == 0 {
        Some(Skip::NoBuildNumber)
    } else if !g.exe_name_ok {
        Some(Skip::OtherExeName)
    } else if !g.ini_enabled {
        Some(Skip::IniOff)
    } else {
        None
    }
}

// ---- 本物 ----

pub struct WinHttpNet {
    /// 検査は `http://127.0.0.1:<port>`。
    pub server: String,
    pub proxy: Proxy,
    pub manifest_budget: Duration,
    pub lang: Lang,
}

impl WinHttpNet {
    pub fn production(lang: Lang) -> WinHttpNet {
        WinHttpNet {
            server: update::UPDATE_SERVER.to_string(),
            proxy: Proxy::Automatic,
            manifest_budget: Duration::from_millis(update::CHECK_DEADLINE_MS),
            lang,
        }
    }

    fn bad_url(&self, url: &str) -> String {
        by_lang!(self.lang, ja: "URL が読めない: {url}", en: "Cannot read the URL: {url}")
    }
}

impl Net for WinHttpNet {
    fn get_manifest(&mut self) -> Result<Vec<u8>, String> {
        let lang = self.lang;
        let url = update::manifest_url(&self.server);
        let ep = update::parse_url(&url).ok_or_else(|| self.bad_url(&url))?;
        let proxy = self.proxy;
        let (tx, rx) = mpsc::channel();
        // 通信は別スレッドで。名前の解決やプロキシの自動探索は操作ごとの上限だけでは収まらないので、時間で見切る。
        std::thread::Builder::new()
            .name("higanr-update-check".into())
            .spawn(move || {
                let _ = tx.send(http_win::get(
                    &ep,
                    proxy,
                    CHECK_TIMEOUTS,
                    update::MAX_MANIFEST_BYTES,
                ));
            })
            .map_err(|e| {
                by_lang!(
                    lang,
                    ja: "通信のスレッドを作れません（{e}）",
                    en: "Cannot create the network thread ({e})"
                )
            })?;
        match rx.recv_timeout(self.manifest_budget) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(e.describe(lang)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let ms = self.manifest_budget.as_millis();
                Err(by_lang!(
                    lang,
                    ja: "{ms} ミリ秒で答えが来ませんでした",
                    en: "No answer within {ms} ms"
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(match lang {
                Lang::Ja => "通信のスレッドが途中で止まりました",
                Lang::En => "The network thread stopped midway",
            }
            .to_string()),
        }
    }

    fn get_file(&mut self, name: &str, size: u64) -> Result<Vec<u8>, String> {
        let lang = self.lang;
        let url = update::file_url(&self.server, name).ok_or_else(|| {
            by_lang!(
                lang,
                ja: "{name:?} はダウンロードしてよい名前ではありません",
                en: "{name:?} is not a name that may be fetched"
            )
        })?;
        let ep = update::parse_url(&url).ok_or_else(|| self.bad_url(&url))?;
        let max = usize::try_from(size.saturating_add(1)).map_err(|_| {
            match lang {
                Lang::Ja => "大きさが大きすぎます",
                Lang::En => "The size is too large",
            }
            .to_string()
        })?;
        http_win::get(&ep, self.proxy, DOWNLOAD_TIMEOUTS, max)
            .map_err(|e: HttpError| e.describe(lang))
    }
}

pub struct RealHost;

impl Host for RealHost {
    fn ask_update(&mut self, text: &str) -> bool {
        crate::folder_dialog::ask_yes_no(text)
    }

    fn tell(&mut self, text: &str) {
        eprintln!("HiganRTool: {text}");
        crate::folder_dialog::message_box(text);
    }

    fn say(&mut self, line: &str) {
        println!("HiganRTool: {line}");
    }

    fn restart(&mut self, exe: &Path, args: &[OsString]) -> Result<(), String> {
        std::process::Command::new(exe)
            .args(args)
            .env(update::JUST_UPDATED_ENV, "1")
            .spawn()
            .map(|_child| ())
            .map_err(|e| format!("{}: {e}", exe.display()))
    }
}

pub fn is_release_exe_name(exe: &Path) -> bool {
    exe.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(update::SELF_EXE))
}

pub const fn this_build_is_dist() -> bool {
    cfg!(feature = "dist")
}

pub struct Start<'a> {
    /// 偽なら何も読まず・触らず `Proceed`（開発用ビルドの exe の隣を片づけない）。
    pub dist: bool,
    pub build_number: u32,
    pub exe: &'a Path,
    pub dev_args: bool,
    pub just_updated: bool,
    pub inherited_ini: bool,
    pub args: &'a [OsString],
    /// 読めなければ None（その回は確かめない）。
    pub key: Option<[u8; 32]>,
    pub lang: Lang,
}

/// 起動の入口の本体。条件がそろえば確かめ、`Restarted` なら呼び出し側は何もせず終わる。
pub fn start_with(c: &Start<'_>, net: &mut dyn Net, host: &mut dyn Host) -> Outcome {
    if !c.dist {
        return Outcome::Proceed;
    }
    let Some(dir) = c.exe.parent() else {
        return Outcome::Proceed;
    };
    let saved_text = std::fs::read_to_string(dir.join(game_pick::SAVED_FILE)).ok();
    let in_game = in_game_folder(dir, saved_text.as_deref());
    let lang = c.lang;
    let left = clear_leftovers_at(dir, in_game, c.just_updated, c.build_number);
    if !left.is_empty() {
        let list = left
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        println!(
            "{}",
            by_lang!(
                lang,
                ja: "HiganRTool: 前の更新の古いファイルを消せませんでした（次の起動でまた試します）: {list}",
                en: "HiganRTool: Could not delete old files from the previous update (will try again at the next launch): {list}"
            )
        );
    }
    let ini_text = std::fs::read_to_string(dir.join(crate::code_relay::INI_FILE)).ok();
    let ini_enabled = match update::update_check_enabled(ini_text.as_deref()) {
        Ok(b) => b,
        Err(why) => {
            eprintln!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: {why}。更新を確かめます。",
                    en: "HiganRTool: {why}. Checking for updates."
                )
            );
            true
        }
    };
    let gate = Gate {
        dist: c.dist,
        in_game_folder: in_game,
        dev_args: c.dev_args,
        just_updated: c.just_updated,
        inherited_ini: c.inherited_ini,
        build_number: c.build_number,
        exe_name_ok: is_release_exe_name(c.exe),
        ini_enabled,
    };
    if let Some(reason) = skip_reason(&gate) {
        if reason == Skip::InGameFolder {
            println!(
                "{}",
                match lang {
                    Lang::Ja =>
                        "HiganRTool: このツールが花映塚と同じディレクトリに置かれています。更新は確かめません（別のディレクトリへ置くと確かめます）。",
                    Lang::En =>
                        "HiganRTool: This HiganRTool is inside the game folder, so updates are not checked (put it in another folder to check).",
                }
            );
        }
        return Outcome::Proceed;
    }
    let Some(key) = c.key else {
        eprintln!(
            "{}",
            match lang {
                Lang::Ja => "HiganRTool: 更新の公開鍵が読めないので、更新は確かめません。",
                Lang::En =>
                    "HiganRTool: Cannot read the update public key, so updates are not checked.",
            }
        );
        return Outcome::Proceed;
    };
    run(
        &Setup {
            dir,
            args: c.args,
            current: c.build_number,
            key,
            lang,
        },
        net,
        host,
    )
}

/// 環境から `Start` を組み、本物の通信とウィンドウで `start_with` を呼ぶ。
pub fn startup(dev_args: bool, lang: Lang) -> Outcome {
    let Ok(exe) = std::env::current_exe() else {
        return Outcome::Proceed;
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let c = Start {
        dist: this_build_is_dist(),
        build_number: update::BUILD_NUMBER,
        exe: &exe,
        dev_args,
        just_updated: std::env::var_os(update::JUST_UPDATED_ENV).is_some(),
        inherited_ini: std::env::var_os(higanr_core::ini::PATH_ENV).is_some(),
        args: &args,
        key: update::update_public_key(),
        lang,
    };
    start_with(&c, &mut WinHttpNet::production(lang), &mut RealHost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lang::has_japanese;
    use higanr_core::update::test_key::{file_bytes, public_key, sample_body, sign, sign_with};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    // ---- 偽の HTTP サーバー（loopback。外へは出ない）----

    #[derive(Clone)]
    enum Route {
        Body(Vec<u8>),
        Redirect(String),
        Status(u16),
        Cut {
            claimed: usize,
            sent: Vec<u8>,
        },
        Delay(Duration, Vec<u8>),
        /// 本文を 3 つに分けて 1 秒おきに送る（1 回の待ちは受信の上限 1.5 秒より短いが、全体は 3 秒）。
        Trickle(Vec<u8>),
    }

    struct Server {
        port: u16,
        hits: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(routes: HashMap<String, Route>) -> Server {
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("loopback に待ち受けのソケットを開ける");
            listener.set_nonblocking(true).expect("非同期にできる");
            let port = listener.local_addr().expect("待ち受けのアドレス").port();
            let hits = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let routes = Arc::new(routes);
            let (h2, s2) = (hits.clone(), stop.clone());
            let join = std::thread::spawn(move || {
                while !s2.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let (routes, hits) = (routes.clone(), h2.clone());
                            std::thread::spawn(move || serve(conn, port, &routes, &hits));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(3)),
                    }
                }
            });
            Server {
                port,
                hits,
                stop,
                join: Some(join),
            }
        }

        fn base(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().expect("錠").clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    fn serve(
        mut conn: TcpStream,
        port: u16,
        routes: &HashMap<String, Route>,
        hits: &Mutex<Vec<String>>,
    ) {
        let _ = conn.set_nonblocking(false);
        let _ = conn.set_read_timeout(Some(Duration::from_secs(3)));
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 8192 {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => req.extend_from_slice(&buf[..n]),
            }
        }
        let line = String::from_utf8_lossy(&req)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let path = line.split(' ').nth(1).unwrap_or("").to_string();
        hits.lock().expect("錠").push(path.clone());
        let head = |status: &str, len: usize, extra: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {len}\r\n{extra}Connection: close\r\n\r\n"
            )
        };
        match routes.get(&path) {
            Some(Route::Body(b)) => {
                let _ = conn.write_all(head("200 OK", b.len(), "").as_bytes());
                let _ = conn.write_all(b);
            }
            Some(Route::Redirect(to)) => {
                let extra = format!("Location: http://127.0.0.1:{port}{to}\r\n");
                let _ = conn.write_all(head("302 Found", 0, &extra).as_bytes());
            }
            Some(Route::Status(n)) => {
                let _ = conn.write_all(head(&format!("{n} X"), 0, "").as_bytes());
            }
            Some(Route::Cut { claimed, sent }) => {
                let _ = conn.write_all(head("200 OK", *claimed, "").as_bytes());
                let _ = conn.write_all(sent);
            }
            Some(Route::Delay(d, b)) => {
                std::thread::sleep(*d);
                let _ = conn.write_all(head("200 OK", b.len(), "").as_bytes());
                let _ = conn.write_all(b);
            }
            Some(Route::Trickle(b)) => {
                let _ = conn.write_all(head("200 OK", b.len(), "").as_bytes());
                for part in b.chunks(b.len().div_ceil(3)) {
                    let _ = conn.flush();
                    std::thread::sleep(Duration::from_secs(1));
                    let _ = conn.write_all(part);
                }
            }
            None => {
                let _ = conn.write_all(head("404 Not Found", 0, "").as_bytes());
            }
        }
        let _ = conn.flush();
        let _ = conn.shutdown(Shutdown::Both);
    }

    // ---- 偽のウィンドウ・プロセス ----

    #[derive(Default)]
    struct FakeHost {
        answer: bool,
        restart_error: Option<String>,
        asked: Vec<String>,
        told: Vec<String>,
        said: Vec<String>,
        restarts: Vec<(PathBuf, Vec<OsString>)>,
        exe_at_restart: Option<Vec<u8>>,
    }

    impl Host for FakeHost {
        fn ask_update(&mut self, text: &str) -> bool {
            self.asked.push(text.to_string());
            self.answer
        }
        fn tell(&mut self, text: &str) {
            self.told.push(text.to_string());
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn restart(&mut self, exe: &Path, args: &[OsString]) -> Result<(), String> {
            self.exe_at_restart = std::fs::read(exe).ok();
            self.restarts.push((exe.to_path_buf(), args.to_vec()));
            match &self.restart_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
    }

    // ---- 舞台 ----

    const OLD_VERSION: u32 = 100;
    const NEW_VERSION: u32 = 200;
    const NAMES: [&str; 3] = ["HiganRTool.exe", "HiganRTool.dll", "README.txt"];

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "HiganRTool_loader_update_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("一時フォルダ");
        dir
    }

    fn install(dir: &Path) {
        for n in NAMES {
            std::fs::write(dir.join(n), file_bytes(n, OLD_VERSION)).expect("古いファイル");
        }
        std::fs::write(dir.join("HiganRTool.ini"), "[netplay]\nname = keep-me\n").expect("ini");
        std::fs::create_dir_all(dir.join("logs")).expect("logs");
        std::fs::write(dir.join("logs").join("dll_x.log"), "log").expect("log");
    }

    fn manifest_routes(manifest: &str, version: u32) -> HashMap<String, Route> {
        let mut r = HashMap::new();
        r.insert(
            "/update/manifest".to_string(),
            Route::Redirect("/rel/manifest.txt".to_string()),
        );
        r.insert(
            "/rel/manifest.txt".to_string(),
            Route::Body(manifest.as_bytes().to_vec()),
        );
        for n in NAMES {
            r.insert(
                format!("/update/files/{n}"),
                Route::Body(file_bytes(n, version)),
            );
        }
        r
    }

    fn happy_routes() -> HashMap<String, Route> {
        manifest_routes(&sign(&sample_body(NEW_VERSION)), NEW_VERSION)
    }

    fn net_for(server: &Server) -> WinHttpNet {
        WinHttpNet {
            server: server.base(),
            proxy: Proxy::Direct,
            manifest_budget: Duration::from_millis(update::CHECK_DEADLINE_MS),
            lang: Lang::Ja,
        }
    }

    fn go(dir: &Path, server: &Server, host: &mut FakeHost, current: u32) -> Outcome {
        go_in(dir, server, host, current, Lang::Ja)
    }

    fn go_in(
        dir: &Path,
        server: &Server,
        host: &mut FakeHost,
        current: u32,
        lang: Lang,
    ) -> Outcome {
        let args = vec![OsString::from("--game"), OsString::from("x.exe")];
        let mut net = net_for(server);
        net.lang = lang;
        run(
            &Setup {
                dir,
                args: &args,
                current,
                key: public_key(),
                lang,
            },
            &mut net,
            host,
        )
    }

    fn all_text(h: &FakeHost) -> Vec<&String> {
        h.asked.iter().chain(&h.told).chain(&h.said).collect()
    }

    #[test]
    fn english_run_asks_and_reports_every_update_step_in_english() {
        // 更新する経路: 尋ねる文・取る・置き換える・起こし直す。
        let dir = temp_dir("english_happy");
        install(&dir);
        let server = Server::start(happy_routes());
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            go_in(&dir, &server, &mut host, OLD_VERSION, Lang::En),
            Outcome::Restarted
        );
        assert!(host.asked[0].contains("Update now?"), "{}", host.asked[0]);
        assert!(host.said.iter().any(|l| l.starts_with("Update: fetching")));
        assert!(
            host.said
                .iter()
                .any(|l| l.contains("Restarting with the new version"))
        );
        for l in all_text(&host) {
            assert!(!has_japanese(l), "英語の回に日本語: {l}");
        }
        std::fs::remove_dir_all(&dir).expect("後片づけ");

        // 断る経路と、目録が取れない経路。
        let dir = temp_dir("english_decline");
        install(&dir);
        let mut host = FakeHost::default();
        assert_eq!(
            go_in(&dir, &server, &mut host, OLD_VERSION, Lang::En),
            Outcome::Proceed
        );
        assert_eq!(host.said, ["Update check: starting without updating."]);
        let empty = Server::start(HashMap::new());
        let mut host = FakeHost::default();
        assert_eq!(
            go_in(&dir, &empty, &mut host, OLD_VERSION, Lang::En),
            Outcome::Proceed
        );
        assert_eq!(
            host.said,
            ["Update check: failed (The server answered 404). Starting without checking."]
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");

        // 起こし直しに失敗して元へ戻す経路と、置き換えの失敗の理由。
        let dir = temp_dir("english_restart_fail");
        install(&dir);
        let mut host = FakeHost {
            answer: true,
            restart_error: Some("cannot spawn".to_string()),
            ..FakeHost::default()
        };
        assert_eq!(
            go_in(&dir, &server, &mut host, OLD_VERSION, Lang::En),
            Outcome::Proceed
        );
        assert_eq!(
            host.told,
            [
                "Could not update (Could not start the new version (cannot spawn)). Starting with the current version."
            ]
        );
        std::fs::create_dir_all(dir.join("HiganRTool.exe.old").join("inner")).expect("邪魔");
        let files: Vec<(String, Vec<u8>)> = NAMES
            .iter()
            .map(|n| (n.to_string(), file_bytes(n, NEW_VERSION)))
            .collect();
        let err = replace_files(&dir, &files, Lang::En).expect_err("exe の .old が塞がれている");
        assert!(
            err.reason.starts_with("Cannot delete")
                && err.reason.contains(" left by the previous update ("),
            "{}",
            err.describe(Lang::En)
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    fn read(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap_or_default()
    }

    fn assert_untouched(dir: &Path) {
        for n in NAMES {
            assert_eq!(read(dir, n), file_bytes(n, OLD_VERSION), "{n} が変わった");
            assert!(!dir.join(update::old_name(n)).exists(), "{n}.old が残った");
            assert!(!dir.join(update::new_name(n)).exists(), "{n}.new が残った");
        }
        assert_eq!(read(dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        assert_eq!(read(dir, "logs/dll_x.log"), b"log");
    }

    // ---- 検査 ----

    #[test]
    fn a_newer_manifest_is_applied_and_the_new_exe_restarted() {
        let dir = temp_dir("happy");
        install(&dir);
        let server = Server::start(happy_routes());
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            go(&dir, &server, &mut host, OLD_VERSION),
            Outcome::Restarted
        );
        for n in NAMES {
            assert_eq!(read(&dir, n), file_bytes(n, NEW_VERSION), "{n}");
            assert_eq!(
                read(&dir, &update::old_name(n)),
                file_bytes(n, OLD_VERSION),
                "{n}.old"
            );
            assert!(!dir.join(update::new_name(n)).exists());
        }
        assert_eq!(read(&dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        assert_eq!(read(&dir, "logs/dll_x.log"), b"log");
        assert_eq!(host.asked.len(), 1);
        assert!(
            host.asked[0].contains("20261008-abcdef0"),
            "{}",
            host.asked[0]
        );
        assert_eq!(host.restarts.len(), 1);
        assert_eq!(host.restarts[0].0, dir.join("HiganRTool.exe"));
        assert_eq!(
            host.restarts[0].1,
            vec![OsString::from("--game"), OsString::from("x.exe")]
        );
        assert_eq!(
            host.exe_at_restart.as_deref(),
            Some(file_bytes("HiganRTool.exe", NEW_VERSION).as_slice()),
            "起こし直しの時点で exe が新しくない"
        );
        assert!(host.told.is_empty(), "{:?}", host.told);
        let hits = server.hits();
        assert_eq!(
            &hits[..2],
            ["/update/manifest", "/rel/manifest.txt"],
            "{hits:?}"
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn declining_changes_nothing_and_downloads_nothing() {
        let dir = temp_dir("decline");
        install(&dir);
        let server = Server::start(happy_routes());
        let mut host = FakeHost::default(); // answer = false
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        assert_untouched(&dir);
        assert_eq!(host.asked.len(), 1);
        assert!(host.restarts.is_empty());
        assert!(
            server
                .hits()
                .iter()
                .all(|h| !h.starts_with("/update/files/")),
            "{:?}",
            server.hits()
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn same_or_older_manifests_are_not_offered() {
        for (name, current) in [("same", NEW_VERSION), ("older", NEW_VERSION + 50)] {
            let dir = temp_dir(name);
            install(&dir);
            let server = Server::start(happy_routes());
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            assert_eq!(
                go(&dir, &server, &mut host, current),
                Outcome::Proceed,
                "{name}"
            );
            assert!(host.asked.is_empty(), "{name}: 尋ねた");
            assert_untouched(&dir);
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    #[test]
    fn manifests_that_fail_the_signature_are_never_used() {
        let body = sample_body(NEW_VERSION);
        let good = sign(&body);
        let variants: Vec<(&str, String)> = vec![
            ("別の鍵", sign_with(&[9u8; 32], &body)),
            (
                "書き換え",
                good.replacen("display 20261008", "display 20269999", 1),
            ),
            ("署名なし", body.clone()),
            (
                "バージョンだけ上げた",
                good.replacen(&format!("version {NEW_VERSION}"), "version 999999", 1),
            ),
        ];
        for (what, manifest) in variants {
            let dir = temp_dir("badsig");
            install(&dir);
            let server = Server::start(manifest_routes(&manifest, NEW_VERSION));
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            assert_eq!(
                go(&dir, &server, &mut host, OLD_VERSION),
                Outcome::Proceed,
                "{what}"
            );
            assert!(host.asked.is_empty(), "{what}: 尋ねた");
            assert!(
                host.told.is_empty(),
                "{what}: ウィンドウで知らせた（確認の失敗はウィンドウを出さない）"
            );
            assert!(
                server
                    .hits()
                    .iter()
                    .all(|h| !h.starts_with("/update/files/")),
                "{what}"
            );
            assert_untouched(&dir);
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    #[test]
    fn a_broken_or_missing_manifest_endpoint_just_proceeds() {
        let good = sign(&sample_body(NEW_VERSION));
        let half = good.as_bytes()[..good.len() / 2].to_vec();
        let mut cases: Vec<(&str, Option<HashMap<String, Route>>)> = Vec::new();
        let mut cut = happy_routes();
        cut.insert(
            "/rel/manifest.txt".to_string(),
            Route::Cut {
                claimed: good.len(),
                sent: half.clone(),
            },
        );
        cases.push(("途中で切れた", Some(cut)));
        let mut short = happy_routes();
        short.insert("/rel/manifest.txt".to_string(), Route::Body(half));
        cases.push(("半分の目録", Some(short)));
        for st in [404u16, 500] {
            let mut r = happy_routes();
            r.insert("/rel/manifest.txt".to_string(), Route::Status(st));
            cases.push(("状態が 200 でない", Some(r)));
        }
        cases.push(("待ち受けが無い", None));
        for (what, routes) in cases {
            let dir = temp_dir("broken");
            install(&dir);
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            let out = match routes {
                Some(r) => {
                    let server = Server::start(r);
                    go(&dir, &server, &mut host, OLD_VERSION)
                }
                None => {
                    let port = TcpListener::bind("127.0.0.1:0")
                        .expect("待ち受けのソケット")
                        .local_addr()
                        .expect("アドレス")
                        .port();
                    let mut net = WinHttpNet {
                        server: format!("http://127.0.0.1:{port}"),
                        proxy: Proxy::Direct,
                        manifest_budget: Duration::from_millis(update::CHECK_DEADLINE_MS),
                        lang: Lang::Ja,
                    };
                    run(
                        &Setup {
                            dir: &dir,
                            args: &[],
                            current: OLD_VERSION,
                            key: public_key(),
                            lang: Lang::Ja,
                        },
                        &mut net,
                        &mut host,
                    )
                }
            };
            assert_eq!(out, Outcome::Proceed, "{what}");
            assert!(host.asked.is_empty(), "{what}");
            assert!(host.told.is_empty(), "{what}");
            assert_untouched(&dir);
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    #[test]
    fn a_slow_manifest_endpoint_is_abandoned_after_about_two_seconds() {
        let manifest = sign(&sample_body(NEW_VERSION)).into_bytes();
        for (what, route, min_ms, max_ms) in [
            (
                "答えない",
                Route::Delay(Duration::from_secs(6), manifest.clone()),
                1400,
                3500,
            ),
            ("少しずつ", Route::Trickle(manifest.clone()), 1900, 2800),
        ] {
            let dir = temp_dir("slow");
            install(&dir);
            let mut routes = happy_routes();
            routes.insert("/rel/manifest.txt".to_string(), route);
            let server = Server::start(routes);
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            let t0 = Instant::now();
            assert_eq!(
                go(&dir, &server, &mut host, OLD_VERSION),
                Outcome::Proceed,
                "{what}"
            );
            let took = t0.elapsed();
            assert!(
                took >= Duration::from_millis(min_ms) && took < Duration::from_millis(max_ms),
                "{what}: {took:?}"
            );
            assert!(host.asked.is_empty(), "{what}");
            assert_untouched(&dir);
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    #[test]
    fn a_file_with_a_wrong_sha256_aborts_the_whole_update() {
        let dir = temp_dir("sha");
        install(&dir);
        let mut routes = happy_routes();
        let mut evil = file_bytes("HiganRTool.dll", NEW_VERSION);
        let n = evil.len();
        evil[n - 2] ^= 0x55; // 同じ大きさで中身だけ違う
        routes.insert(
            "/update/files/HiganRTool.dll".to_string(),
            Route::Body(evil),
        );
        let server = Server::start(routes);
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        assert_untouched(&dir);
        assert_eq!(host.told.len(), 1);
        assert!(host.told[0].contains("SHA-256"), "{}", host.told[0]);
        assert!(host.restarts.is_empty());
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn a_size_that_disagrees_with_the_manifest_is_refused_even_if_the_sha_matches() {
        let dir = temp_dir("size_vs_sha");
        install(&dir);
        let readme = file_bytes("README.txt", NEW_VERSION);
        let honest = format!(
            "file README.txt {} {}",
            readme.len(),
            update::to_hex(&update::sha256(&readme))
        );
        let lying = honest.replacen(
            &readme.len().to_string(),
            &(readme.len() + 1).to_string(),
            1,
        );
        let body = sample_body(NEW_VERSION);
        assert!(body.contains(&honest), "{body}");
        let server = Server::start(manifest_routes(
            &sign(&body.replace(&honest, &lying)),
            NEW_VERSION,
        ));
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        assert_untouched(&dir);
        assert_eq!(host.told.len(), 1);
        assert!(host.told[0].contains("大きさ"), "{}", host.told[0]);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn a_truncated_or_resized_file_aborts_the_whole_update() {
        let good = file_bytes("README.txt", NEW_VERSION);
        let cases: Vec<(&str, Route)> = vec![
            (
                "途中で切れた",
                Route::Cut {
                    claimed: good.len(),
                    sent: good[..good.len() / 2].to_vec(),
                },
            ),
            ("短い", Route::Body(good[..good.len() - 1].to_vec())),
            ("長い", Route::Body([good.clone(), b"x".to_vec()].concat())),
            ("404", Route::Status(404)),
        ];
        for (what, route) in cases {
            let dir = temp_dir("size");
            install(&dir);
            let mut routes = happy_routes();
            routes.insert("/update/files/README.txt".to_string(), route);
            let server = Server::start(routes);
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            assert_eq!(
                go(&dir, &server, &mut host, OLD_VERSION),
                Outcome::Proceed,
                "{what}"
            );
            assert_untouched(&dir);
            assert_eq!(host.told.len(), 1, "{what}");
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    #[test]
    fn a_failure_in_the_middle_of_replacing_rolls_everything_back() {
        let dir = temp_dir("rollback");
        install(&dir);
        let blocker = dir.join("README.txt.old");
        std::fs::create_dir_all(blocker.join("inner")).expect("邪魔なフォルダ");
        let server = Server::start(happy_routes());
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        for n in NAMES {
            assert_eq!(
                read(&dir, n),
                file_bytes(n, OLD_VERSION),
                "{n} が戻っていない"
            );
            assert!(!dir.join(update::new_name(n)).exists(), "{n}.new が残った");
        }
        assert!(!dir.join("HiganRTool.exe.old").exists());
        assert!(!dir.join("HiganRTool.dll.old").exists());
        assert!(blocker.join("inner").exists(), "邪魔なフォルダを消した");
        assert_eq!(read(&dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        assert_eq!(host.told.len(), 1);
        assert!(host.restarts.is_empty());
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn the_running_exe_is_replaced_last() {
        let dir = temp_dir("exe_last");
        install(&dir);
        std::fs::create_dir_all(dir.join("HiganRTool.exe.old").join("inner")).expect("邪魔");
        let files: Vec<(String, Vec<u8>)> = NAMES
            .iter()
            .map(|n| (n.to_string(), file_bytes(n, NEW_VERSION)))
            .collect();
        let order: Vec<&str> = commit_order(&files)
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(order, ["HiganRTool.dll", "README.txt", "HiganRTool.exe"]);
        let err = replace_files(&dir, &files, Lang::Ja).expect_err("exe の .old が塞がれている");
        assert!(
            err.reason.contains("HiganRTool.exe.old"),
            "{}",
            err.describe(Lang::Ja)
        );
        assert!(
            err.rollback_problems.is_empty(),
            "{}",
            err.describe(Lang::Ja)
        );
        for n in NAMES {
            assert_eq!(read(&dir, n), file_bytes(n, OLD_VERSION), "{n}");
            assert!(!dir.join(update::old_name(n)).exists() || n == "HiganRTool.exe");
        }
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn a_failed_restart_restores_the_old_files() {
        let dir = temp_dir("restart_fail");
        install(&dir);
        let server = Server::start(happy_routes());
        let mut host = FakeHost {
            answer: true,
            restart_error: Some("起こせない".to_string()),
            ..FakeHost::default()
        };
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        assert_eq!(host.restarts.len(), 1, "起こし直しを試していない");
        assert_untouched(&dir);
        assert_eq!(host.told.len(), 1);
        assert!(host.told[0].contains("起こせない"), "{}", host.told[0]);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn replace_files_refuses_names_that_break_the_name_rule() {
        let dir = temp_dir("allow");
        install(&dir);
        let outside = temp_dir("allow_outside");
        for bad in [
            "HiganRTool.ini",
            "game_path.txt",
            "x.old",
            "LOGS",
            ".hidden",
            "logs",
            "..\\evil.txt",
            "../evil.txt",
            "sub\\x.dll",
            "HiganRTool.exe.old",
            "th09.exe",
            "",
        ] {
            let files = vec![
                ("README.txt".to_string(), b"new readme".to_vec()),
                (bad.to_string(), b"evil".to_vec()),
            ];
            let err = replace_files(&dir, &files, Lang::Ja).expect_err(bad);
            assert!(
                err.reason.contains("置き換えてよい"),
                "{bad}: {}",
                err.describe(Lang::Ja)
            );
            assert_untouched(&dir);
            assert!(!outside.join("evil.txt").exists());
        }
        std::fs::remove_dir_all(&dir).expect("後片づけ");
        std::fs::remove_dir_all(&outside).expect("後片づけ");
    }

    #[test]
    fn replace_files_places_a_new_name_that_follows_the_rule_and_undo_removes_it() {
        let dir = temp_dir("newname");
        install(&dir);
        let files = vec![
            ("NOTICE.txt".to_string(), b"notice".to_vec()),
            ("README.txt".to_string(), b"new readme".to_vec()),
        ];
        let placed = replace_files(&dir, &files, Lang::Ja).expect("置けない");
        assert_eq!(read(&dir, "NOTICE.txt"), b"notice");
        assert_eq!(read(&dir, "README.txt"), b"new readme");
        assert!(
            placed.iter().any(|p| p.name == "NOTICE.txt" && !p.had_old),
            "{placed:?}"
        );
        assert!(!dir.join("NOTICE.txt.new").exists());
        let problems = undo(&dir, &placed, Lang::Ja);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(!dir.join("NOTICE.txt").exists());
        assert_untouched(&dir);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn cleanup_removes_only_known_leftovers() {
        let dir = temp_dir("cleanup");
        install(&dir);
        for n in ["HiganRTool.exe.old", "HiganRTool.dll.new", "README.txt.old"] {
            std::fs::write(dir.join(n), "x").expect("残り物");
        }
        std::fs::write(dir.join("mine.txt.old"), "keep").expect("利用者のファイル");
        std::fs::write(dir.join("HiganRTool.ini.old"), "keep").expect("ini のバックアップ");
        std::fs::create_dir_all(dir.join("LICENSE.txt.old").join("inner")).expect("邪魔");
        let left = cleanup_leftovers(&dir, 1, Duration::from_millis(1), true);
        assert_eq!(left, vec![dir.join("LICENSE.txt.old")]);
        for n in ["HiganRTool.exe.old", "HiganRTool.dll.new", "README.txt.old"] {
            assert!(!dir.join(n).exists(), "{n}");
        }
        assert!(dir.join("mine.txt.old").exists());
        assert!(dir.join("HiganRTool.ini.old").exists());
        assert_eq!(read(&dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn the_check_runs_only_for_a_plain_dist_launch() {
        let ok = Gate {
            dist: true,
            in_game_folder: false,
            dev_args: false,
            just_updated: false,
            inherited_ini: false,
            build_number: 5,
            exe_name_ok: true,
            ini_enabled: true,
        };
        assert_eq!(skip_reason(&ok), None);
        assert_eq!(
            skip_reason(&Gate { dist: false, ..ok }),
            Some(Skip::NotDist)
        );
        assert_eq!(
            skip_reason(&Gate {
                in_game_folder: true,
                ..ok
            }),
            Some(Skip::InGameFolder)
        );
        assert_eq!(
            skip_reason(&Gate {
                dev_args: true,
                ..ok
            }),
            Some(Skip::DevArgs)
        );
        assert_eq!(
            skip_reason(&Gate {
                just_updated: true,
                ..ok
            }),
            Some(Skip::JustUpdated)
        );
        assert_eq!(
            skip_reason(&Gate {
                inherited_ini: true,
                ..ok
            }),
            Some(Skip::InheritedIni)
        );
        assert_eq!(
            skip_reason(&Gate {
                build_number: 0,
                ..ok
            }),
            Some(Skip::NoBuildNumber)
        );
        assert_eq!(
            skip_reason(&Gate {
                exe_name_ok: false,
                ..ok
            }),
            Some(Skip::OtherExeName)
        );
        assert_eq!(
            skip_reason(&Gate {
                ini_enabled: false,
                ..ok
            }),
            Some(Skip::IniOff)
        );
    }

    struct FakeNet {
        manifest: Vec<u8>,
        calls: Vec<String>,
    }

    impl Net for FakeNet {
        fn get_manifest(&mut self) -> Result<Vec<u8>, String> {
            self.calls.push("manifest".to_string());
            Ok(self.manifest.clone())
        }
        fn get_file(&mut self, name: &str, _size: u64) -> Result<Vec<u8>, String> {
            self.calls.push(format!("file {name}"));
            Ok(file_bytes(name, NEW_VERSION))
        }
    }

    fn fake_net() -> FakeNet {
        FakeNet {
            manifest: sign(&sample_body(NEW_VERSION)).into_bytes(),
            calls: Vec::new(),
        }
    }

    fn start<'a>(exe: &'a Path, args: &'a [OsString], dist: bool, build: u32) -> Start<'a> {
        Start {
            dist,
            build_number: build,
            exe,
            dev_args: false,
            just_updated: false,
            inherited_ini: false,
            args,
            key: Some(public_key()),
            lang: Lang::Ja,
        }
    }

    #[test]
    fn this_build_is_dist_only_with_the_feature() {
        if cfg!(feature = "dist") {
            assert!(this_build_is_dist());
        } else {
            assert!(!this_build_is_dist());
        }
    }

    #[test]
    fn start_with_does_nothing_outside_a_dist_build() {
        let dir = temp_dir("not_dist");
        install(&dir);
        std::fs::write(dir.join("HiganRTool.exe.new"), "x").expect("残り物");
        std::fs::write(dir.join("README.txt.old"), "x").expect("残り物");
        std::fs::write(dir.join(update::LAUNCHED_MARK), format!("{NEW_VERSION}\n")).expect("印");
        let exe = dir.join("HiganRTool.exe");
        let args: Vec<OsString> = Vec::new();
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            start_with(&start(&exe, &args, false, NEW_VERSION), &mut net, &mut host),
            Outcome::Proceed
        );
        assert!(net.calls.is_empty(), "{:?}", net.calls);
        assert!(host.asked.is_empty() && host.told.is_empty());
        assert!(dir.join("HiganRTool.exe.new").exists(), ".new を消した");
        assert!(dir.join("README.txt.old").exists(), ".old を消した");
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn a_dist_build_outside_the_game_folder_is_offered_the_update() {
        let dir = temp_dir("outside");
        install(&dir);
        let exe = dir.join("HiganRTool.exe");
        let args: Vec<OsString> = Vec::new();
        let mut net = fake_net();
        let mut host = FakeHost::default(); // answer = false（いいえ）
        assert_eq!(
            start_with(&start(&exe, &args, true, OLD_VERSION), &mut net, &mut host),
            Outcome::Proceed
        );
        assert_eq!(host.asked.len(), 1, "尋ねていない");
        assert_eq!(net.calls.first().map(String::as_str), Some("manifest"));
        assert_untouched(&dir);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn a_dist_build_in_the_game_folder_touches_nothing() {
        let dir = temp_dir("in_game");
        install(&dir);
        std::fs::write(dir.join("TH09.EXE"), "game").expect("ゲーム");
        std::fs::write(dir.join(update::LAUNCHED_MARK), format!("{OLD_VERSION}\n")).expect("印");
        std::fs::write(dir.join("HiganRTool.exe.new"), "x").expect("残り物");
        std::fs::write(dir.join("README.txt.old"), "x").expect("残り物");
        let exe = dir.join("HiganRTool.exe");
        let args: Vec<OsString> = Vec::new();
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            start_with(&start(&exe, &args, true, OLD_VERSION), &mut net, &mut host),
            Outcome::Proceed
        );
        assert!(net.calls.is_empty(), "{:?}", net.calls);
        assert!(host.asked.is_empty() && host.told.is_empty() && host.restarts.is_empty());
        for n in NAMES {
            assert_eq!(read(&dir, n), file_bytes(n, OLD_VERSION), "{n}");
        }
        assert!(dir.join("TH09.EXE").exists());
        assert!(dir.join("HiganRTool.exe.new").exists(), ".new を消した");
        assert!(dir.join("README.txt.old").exists(), ".old を消した");
        assert_eq!(read(&dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        std::fs::remove_dir_all(&dir).expect("後片づけ");

        let dir = temp_dir("in_game_saved");
        install(&dir);
        let exe = dir.join("HiganRTool.exe");
        std::fs::write(
            dir.join(game_pick::SAVED_FILE),
            game_pick::saved_text(&dir.join("th09.exe")).expect("UTF-8 のパス"),
        )
        .expect("覚えた場所");
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            start_with(&start(&exe, &args, true, OLD_VERSION), &mut net, &mut host),
            Outcome::Proceed
        );
        assert!(net.calls.is_empty() && host.asked.is_empty());
        assert_untouched(&dir);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn in_game_folder_is_known_from_the_remembered_game_path_or_the_exe() {
        let root = temp_dir("in_game_rule");
        let game = root.join("Game");
        let sub = game.join("tools");
        let other = root.join("Game2");
        for d in [&sub, &other] {
            std::fs::create_dir_all(d).expect("フォルダ");
        }
        let saved = format!("{}\r\n", game.join("th09.exe").display());
        assert!(in_game_folder(&game, Some(&saved)), "同じフォルダ");
        assert!(in_game_folder(&sub, Some(&saved)), "その下のフォルダ");
        assert!(!in_game_folder(&other, Some(&saved)), "前の文字だけ同じ");
        assert!(!in_game_folder(&sub, None), "覚えた場所が無い");
        assert!(!in_game_folder(&sub, Some("")), "空の覚え");
        std::fs::write(sub.join("Th09.Exe"), "x").expect("大文字小文字違い");
        assert!(in_game_folder(&sub, None), "exe のフォルダの th09.exe");
        std::fs::remove_dir_all(&root).expect("後片づけ");
    }

    #[test]
    fn old_is_removed_only_after_the_same_version_has_launched() {
        let cases: [(&str, Option<u32>, u32, bool); 4] = [
            ("印が無い", None, NEW_VERSION, false),
            ("別のバージョンの印", Some(OLD_VERSION), NEW_VERSION, false),
            ("ビルド番号 0", Some(0), 0, false),
            ("同じバージョンの印", Some(NEW_VERSION), NEW_VERSION, true),
        ];
        for (what, mark, build, old_gone) in cases {
            let dir = temp_dir("mark");
            install(&dir);
            std::fs::write(dir.join("README.txt.old"), "old").expect("一つ前のバージョン");
            std::fs::write(dir.join("HiganRTool.exe.new"), "x").expect("途中の残り");
            if let Some(m) = mark {
                std::fs::write(dir.join(update::LAUNCHED_MARK), format!("{m}\n")).expect("印");
            }
            let left = clear_leftovers_at(&dir, false, false, build);
            assert!(left.is_empty(), "{what}: {left:?}");
            assert_eq!(!dir.join("README.txt.old").exists(), old_gone, "{what}");
            assert!(
                !dir.join("HiganRTool.exe.new").exists(),
                "{what}: .new が残った"
            );
            assert_untouched_names(&dir, old_gone);
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
        let dir = temp_dir("mark_in_game");
        install(&dir);
        std::fs::write(dir.join("README.txt.old"), "old").expect("一つ前のバージョン");
        std::fs::write(dir.join(update::LAUNCHED_MARK), format!("{NEW_VERSION}\n")).expect("印");
        assert!(clear_leftovers_at(&dir, true, false, NEW_VERSION).is_empty());
        assert!(
            dir.join("README.txt.old").exists(),
            "ゲームのフォルダで消した"
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    fn assert_untouched_names(dir: &Path, old_gone: bool) {
        for n in NAMES {
            assert_eq!(read(dir, n), file_bytes(n, OLD_VERSION), "{n}");
        }
        assert_eq!(read(dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        assert_eq!(dir.join("README.txt.old").exists(), !old_gone);
    }

    #[test]
    fn the_launched_mark_is_written_outside_the_game_folder_only() {
        let dir = temp_dir("note");
        assert_eq!(note_launched_at(&dir, 0, Lang::Ja), Ok(false));
        assert!(!dir.join(update::LAUNCHED_MARK).exists());
        assert_eq!(note_launched_at(&dir, NEW_VERSION, Lang::Ja), Ok(true));
        assert_eq!(
            read(&dir, update::LAUNCHED_MARK),
            format!("{NEW_VERSION}\n").into_bytes()
        );
        let game = temp_dir("note_game");
        std::fs::write(game.join("th09.exe"), "game").expect("ゲーム");
        assert_eq!(note_launched_at(&game, NEW_VERSION, Lang::Ja), Ok(false));
        assert!(
            !game.join(update::LAUNCHED_MARK).exists(),
            "ゲームのフォルダに印"
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
        std::fs::remove_dir_all(&game).expect("後片づけ");
    }

    #[test]
    fn only_the_release_exe_name_is_updated() {
        assert!(is_release_exe_name(Path::new("C:\\x\\HiganRTool.exe")));
        assert!(is_release_exe_name(Path::new("higanrtool.EXE")));
        for bad in [
            "higanrtool_loader.exe",
            "HiganRTool_a.exe",
            "HiganRTool.exe.old",
            "HiganRTool",
        ] {
            assert!(!is_release_exe_name(Path::new(bad)), "{bad}");
        }
    }

    #[test]
    fn real_restart_spawns_a_child_and_reports_failure() {
        let sysroot = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .expect("SystemRoot");
        let cmd = sysroot.join("System32").join("cmd.exe");
        let mut host = RealHost;
        assert_eq!(
            host.restart(
                &cmd,
                &[
                    OsString::from("/c"),
                    OsString::from("exit"),
                    OsString::from("0")
                ]
            ),
            Ok(())
        );
        let missing = std::env::temp_dir().join("HiganRTool_no_such_exe.exe");
        assert!(host.restart(&missing, &[]).is_err());
    }

    #[test]
    fn the_question_says_what_yes_and_no_do() {
        let t = ask_text("20261008-abcdef0", Lang::Ja);
        for want in [
            "20261008-abcdef0",
            "更新しますか",
            "起動を取りやめ",
            "HiganRTool.ini",
            "いいえ",
        ] {
            assert!(t.contains(want), "{want}: {t}");
        }
    }
}
