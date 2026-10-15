//! 起動前の更新（配るバージョンだけ）。花映塚を起こす前に署名つきの目録を取り、新しいバージョンがあればウィンドウで尋ね、「はい」なら
//! 必要なファイルを置き換えて新しい `HiganRTool.exe` を同じ引数で起こし直す。通信とウィンドウは `Net` / `Host` の向こう。
//!
//! - 確かめるのは配るバージョン（feature `dist`）の通常起動だけ（`skip_reason`）。ゲームのフォルダが決まってからで、その中では何もしない。
//! - 待ちは全体で `CHECK_DEADLINE_MS`（約 2 秒）まで。通信は別スレッドで走らせ、時間が来たら起動へ進む。
//! - 置き換えは `<名前>.new` に書いて読み戻し、全部そろってから 1 つずつ「いまの物を `.old` へ改名 → `.new` を本来の名前へ」。
//!   動いている exe や読み込み中の dll は上書きできないが改名はできるため。途中で失敗したら逆順に戻し、自分の exe は最後に置く。
//! - `.old` は、新しいバージョンが花映塚を起こせた印（`note_launched`）を残した次の起動が消す。起こせなかったバージョンでは残るので手で戻せる。
//! - ダウンロードの間は、ファイルごとの進み具合を `Host::progress` で同じ行に書き直す。更新して起こし直した初回は `post_update`
//!   が ini を新しい形に合わせ、変更点を出して Enter を待つ。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use higanr_core::lang::Lang;
use higanr_core::update::{self, FileEntry, Manifest};

use crate::crash_log::is_inside_game;
use crate::game_pick;
use crate::http_win::{self, HttpError, Proxy, Timeouts};
use crate::post_update;
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
    /// `size` は目録の大きさ。越える応答は断る。`progress` には、受け取り進むたびにそこまでのバイト数を渡す。
    fn get_file(
        &mut self,
        name: &str,
        size: u64,
        progress: &mut dyn FnMut(u64),
    ) -> Result<Vec<u8>, String>;
}

pub trait Host {
    fn ask_update(&mut self, text: &str) -> bool;
    /// ウィンドウにも黒いウィンドウにも出す。
    fn tell(&mut self, text: &str);
    fn say(&mut self, line: &str);
    /// 見出しなどの飾りのない行を出す（変更点の文）。
    fn show(&mut self, text: &str);
    /// 同じ行を書き直して進み具合を出す。区切りは `progress_end`。
    fn progress(&mut self, line: &str);
    /// 書き直していた行を確定して改行する。出していなければ何もしない。
    fn progress_end(&mut self);
    /// 標準入力も出力もコンソールか（パイプや検査の道具からの起動は偽）。
    fn interactive(&self) -> bool;
    /// Enter が押されるまで待つ。
    fn wait_enter(&mut self);
    /// 起こした子が終わるまで待ち、その終了コードを返す。起こせなければ Err（呼び出し側が全部戻す）。
    /// 子が失敗して終わっても更新は戻さないので、それは Ok。待つ間は何も読まず・書かない（子が同じコンソールの入力を使う）。
    fn restart(&mut self, exe: &Path, args: &[OsString]) -> Result<i32, String>;
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
    /// 新しいバージョンを起こし直して終わるまで待った。値は子の終了コード（呼び出し側はそれで終わる）。
    Restarted(i32),
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
        ja: "更新データがありました（{display}、{count}個のファイル）。更新して起動します。",
        en: "Update: found {display} ({count} files). Updating, then starting."
    ));
    let files = match download_all(net, &manifest, host, lang) {
        Ok(f) => f,
        Err(why) => return give_up(host, &why, lang),
    };
    host.say(match lang {
        Lang::Ja => "更新データを置き換えています。",
        Lang::En => "Update: replacing the files.",
    });
    let placed = match replace_files(s.dir, &files, lang) {
        Ok(p) => p,
        Err(e) => return give_up(host, &e.describe(lang), lang),
    };
    host.say(match lang {
        Lang::Ja => "更新: 置き換えました。新しいバージョンで起動し直します。",
        Lang::En => "Update: replaced the files. Restarting with the new version.",
    });
    match host.restart(&s.dir.join(update::SELF_EXE), s.args) {
        Ok(code) => Outcome::Restarted(code),
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
        let mut shown = Progress::new(name, nth, total, f.size, lang);
        host.progress(&shown.line(0));
        let got = net.get_file(&f.name, f.size, &mut |received| {
            if let Some(line) = shown.update(received) {
                host.progress(&line);
            }
        });
        if let Ok(data) = &got {
            host.progress(&shown.line(data.len() as u64));
        }
        host.progress_end();
        let data = got.map_err(|e| {
            by_lang!(lang, ja: "{name}をダウンロードできませんでした: {e}", en: "Could not fetch {name}: {e}")
        })?;
        check_file(f, &data, lang)?;
        out.push((f.name.clone(), data));
    }
    Ok(out)
}

/// ダウンロードの進み具合の行。割合が変わったときだけ書き直す。
struct Progress {
    name: String,
    nth: usize,
    total: usize,
    size: u64,
    lang: Lang,
    last_percent: u64,
}

impl Progress {
    fn new(name: &str, nth: usize, total: usize, size: u64, lang: Lang) -> Progress {
        let mut p = Progress {
            name: name.to_string(),
            nth,
            total,
            size,
            lang,
            last_percent: 0,
        };
        p.last_percent = p.percent(0);
        p
    }

    /// 大きさ 0 のファイルは最初から 100%。
    fn percent(&self, got: u64) -> u64 {
        got.saturating_mul(100)
            .checked_div(self.size)
            .map_or(100, |p| p.min(100))
    }

    /// 割合が前の行から変わっていれば、書き直す行。
    fn update(&mut self, got: u64) -> Option<String> {
        let percent = self.percent(got);
        (percent != self.last_percent).then(|| {
            self.last_percent = percent;
            self.line(got)
        })
    }

    /// 例: `ダウンロード中 HiganRTool.dll (2/3)  1.2 / 3.4 MB   35%`。受け取った量は全体の桁数にそろえ、行が縮まないようにする。
    fn line(&self, got: u64) -> String {
        let percent = self.percent(got);
        let (unit, per) = if self.size >= 1 << 20 {
            ("MB", (1u64 << 20) as f64)
        } else {
            ("KB", 1024.0)
        };
        let total = format!("{:.1}", self.size as f64 / per);
        let got = format!("{:>w$.1}", got as f64 / per, w = total.len());
        let (name, nth, count) = (&self.name, self.nth, self.total);
        by_lang!(
            self.lang,
            ja: "ダウンロード中 {name} ({nth}/{count})  {got} / {total} {unit}  {percent:>3}%",
            en: "Downloading {name} ({nth}/{count})  {got} / {total} {unit}  {percent:>3}%"
        )
    }
}

fn check_file(f: &FileEntry, data: &[u8], lang: Lang) -> Result<(), String> {
    let name = &f.name;
    if data.len() as u64 != f.size {
        let (want, got) = (f.size, data.len());
        return Err(by_lang!(
            lang,
            ja: "{name}の大きさが目録と違います（{want}バイトのはずが{got}バイト）",
            en: "The size of {name} does not match the update list ({got} bytes, expected {want})"
        ));
    }
    if update::sha256(data) != f.sha256 {
        return Err(by_lang!(
            lang,
            ja: "{name}のSHA-256が目録と合いません",
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
                ja: "{name:?}は置き換えてよいファイル名ではありません",
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
                by_lang!(lang, ja: "{shown}に書けません（{e}）", en: "Cannot write to {shown} ({e})")
            })
            .and_then(|()| {
                let back = std::fs::read(&new).map_err(|e| {
                    by_lang!(
                        lang,
                        ja: "{shown}を読み込めません（{e}）",
                        en: "Cannot read {shown} back ({e})"
                    )
                })?;
                (back == *data).then_some(()).ok_or_else(|| {
                    by_lang!(
                        lang,
                        ja: "{shown}が書いた中身と違います",
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
                ja: "前の更新の{old_shown}を消せません（{e}）。別のHiganRToolか花映塚が使っていないか見てください",
                en: "Cannot delete {old_shown} left by the previous update ({e}). Check that no other HiganRTool or Touhou 9 is using it"
            )
        })?;
    }
    let had_old = target.symlink_metadata().is_ok();
    if had_old {
        std::fs::rename(&target, &old).map_err(|e| {
            by_lang!(
                lang,
                ja: "{target_shown}の名前を変えられません（{e}）",
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
                    ja: "{target_shown}を置き換えられません（{e}）。{old_shown}も元に戻せません（{e2}）",
                    en: "Cannot replace {target_shown} ({e}). {old_shown} also cannot be restored ({e2})"
                ));
            }
        }
        return Err(by_lang!(
            lang,
            ja: "{target_shown}を置き換えられません（{e}）",
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
/// `just_updated` の親は子が終わるまで動き続けるので、親の exe の `.old` はこの間は消せない（消せなければ残し、次の起動で消す）。
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

/// exe のフォルダに `th09.exe` があるとき。置き換えも片づけもしない。決まったゲームのフォルダとの比べは `is_inside_game`。
pub fn in_game_folder(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(game_pick::GAME_EXE)
        })
    })
}

/// 花映塚を起こせたことを印に残す（配るバージョンだけ）。書けなくても起動は止めない。
/// ゲームのフォルダが分からなければ書かない。
pub fn note_launched(game_dir: Option<&Path>, lang: Lang) {
    if !this_build_is_dist() {
        return;
    }
    let Some(game_dir) = game_dir else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    if let Err(e) = note_launched_at(dir, update::BUILD_NUMBER, game_dir, lang) {
        eprintln!(
            "{}",
            by_lang!(
                lang,
                ja: "HiganRTool: {e}（次の起動で前のバージョンの.oldが残ります）",
                en: "HiganRTool: {e} (the previous version's .old files will remain at the next launch)"
            )
        );
    }
}

/// 書いたら Ok(true)。ビルド番号 0・ゲームのフォルダの中なら書かず Ok(false)。
pub fn note_launched_at(
    dir: &Path,
    build_number: u32,
    game_dir: &Path,
    lang: Lang,
) -> Result<bool, String> {
    if build_number == 0 {
        return Ok(false);
    }
    if in_game_folder(dir) || is_inside_game(dir, game_dir) {
        return Ok(false);
    }
    let path = dir.join(update::LAUNCHED_MARK);
    std::fs::write(&path, update::launched_mark_text(build_number)).map_err(|e| {
        let shown = path.display();
        by_lang!(lang, ja: "{shown}に書けません（{e}）", en: "Cannot write to {shown} ({e})")
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
        by_lang!(self.lang, ja: "URLが読めない: {url}", en: "Cannot read the URL: {url}")
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
                    &mut |_| {},
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
                    ja: "{ms}ミリ秒で答えが来ませんでした",
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

    fn get_file(
        &mut self,
        name: &str,
        size: u64,
        progress: &mut dyn FnMut(u64),
    ) -> Result<Vec<u8>, String> {
        let lang = self.lang;
        let url = update::file_url(&self.server, name).ok_or_else(|| {
            by_lang!(
                lang,
                ja: "{name:?}はダウンロードしてよい名前ではありません",
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
        http_win::get(&ep, self.proxy, DOWNLOAD_TIMEOUTS, max, &mut |n| {
            progress(n as u64)
        })
        .map_err(|e: HttpError| e.describe(lang))
    }
}

/// 本物のウィンドウとコンソール。
#[derive(Default)]
pub struct RealHost {
    /// 書き直す途中の行（改行していない）があるか。
    line_open: bool,
}

impl RealHost {
    /// 書き直す途中の行があれば改行して確定する。
    fn close_line(&mut self) {
        if std::mem::take(&mut self.line_open) {
            println!();
        }
    }
}

impl Host for RealHost {
    fn ask_update(&mut self, text: &str) -> bool {
        crate::folder_dialog::ask_yes_no(text)
    }

    fn tell(&mut self, text: &str) {
        self.close_line();
        eprintln!("HiganRTool: {text}");
        crate::folder_dialog::message_box(text);
    }

    fn say(&mut self, line: &str) {
        self.close_line();
        println!("HiganRTool: {line}");
    }

    fn show(&mut self, text: &str) {
        self.close_line();
        println!("{text}");
    }

    fn progress(&mut self, line: &str) {
        use std::io::Write;
        print!("\r{line}");
        let _ = std::io::stdout().flush();
        self.line_open = true;
    }

    fn progress_end(&mut self) {
        self.close_line();
    }

    fn interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    fn wait_enter(&mut self) {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    fn restart(&mut self, exe: &Path, args: &[OsString]) -> Result<i32, String> {
        // 待たずに終わると、コマンドプロンプトのプロンプトと子の Enter 待ちが同じ入力を取り合う。
        std::process::Command::new(exe)
            .args(args)
            .env(update::JUST_UPDATED_ENV, "1")
            .status()
            .map(|status| status.code().unwrap_or(1))
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
    /// 決まったゲームのフォルダ。None（まだ決まっていない）なら何も読まず・書かず `Proceed`
    /// （ツールがゲームの下の階層にあっても、その中へ `.new` などを書かないため）。
    pub game_dir: Option<&'a Path>,
    pub lang: Lang,
    /// 更新して起こし直した初回に、ini を合わせる先の形（配る ini の全文）。
    pub ini_template: &'a str,
    /// 更新して起こし直した初回に出す変更点。
    pub notes: &'a str,
}

/// 起動の入口の本体。条件がそろえば確かめ、`Restarted` なら呼び出し側は子の終了コードで終わる。
pub fn start_with(c: &Start<'_>, net: &mut dyn Net, host: &mut dyn Host) -> Outcome {
    if !c.dist {
        return Outcome::Proceed;
    }
    let (Some(dir), Some(game_dir)) = (c.exe.parent(), c.game_dir) else {
        return Outcome::Proceed;
    };
    let in_game = in_game_folder(dir) || is_inside_game(dir, game_dir);
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
    // 更新を確かめるのは通常の起動だけなので、起こし直しの初回は開発用の引数も継いだ ini も付かない。付いていたら人の起動ではない。
    if c.just_updated && !in_game && !c.dev_args && !c.inherited_ini {
        post_update::run(
            host,
            &post_update::Plan {
                dir,
                ini_template: c.ini_template,
                notes: c.notes,
                lang,
            },
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

/// 起こし直す exe に渡す引数。`--choose-game` は外す（更新の前に選び終えて覚えてあり、残すと選ぶウィンドウがもう一度出る）。
pub fn restart_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    args.into_iter().filter(|a| a != "--choose-game").collect()
}

/// 環境から `Start` を組み、本物の通信とウィンドウで `start_with` を呼ぶ。`game_dir` は決まったゲームのフォルダ。
pub fn startup(dev_args: bool, game_dir: &Path, lang: Lang) -> Outcome {
    let Ok(exe) = std::env::current_exe() else {
        return Outcome::Proceed;
    };
    let args = restart_args(std::env::args_os().skip(1));
    let ini_template = higanr_core::ini_store::dist_ini_text();
    let c = Start {
        dist: this_build_is_dist(),
        build_number: update::BUILD_NUMBER,
        exe: &exe,
        dev_args,
        just_updated: std::env::var_os(update::JUST_UPDATED_ENV).is_some(),
        inherited_ini: std::env::var_os(higanr_core::ini::PATH_ENV).is_some(),
        args: &args,
        key: update::update_public_key(),
        game_dir: Some(game_dir),
        lang,
        ini_template: &ini_template,
        notes: post_update::NOTES.get(lang),
    };
    start_with(
        &c,
        &mut WinHttpNet::production(lang),
        &mut RealHost::default(),
    )
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
        /// 別の宿（別ポート）への絶対 URL で回す。
        RedirectTo(String),
        /// 待ってから中の応答をする。
        Slow(Duration, Box<Route>),
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
        let mut route = routes.get(&path);
        while let Some(Route::Slow(wait, inner)) = route {
            std::thread::sleep(*wait);
            route = Some(inner);
        }
        match route {
            Some(Route::Body(b)) => {
                let _ = conn.write_all(head("200 OK", b.len(), "").as_bytes());
                let _ = conn.write_all(b);
            }
            Some(Route::Redirect(to)) => {
                let extra = format!("Location: http://127.0.0.1:{port}{to}\r\n");
                let _ = conn.write_all(head("302 Found", 0, &extra).as_bytes());
            }
            Some(Route::RedirectTo(url)) => {
                let extra = format!("Location: {url}\r\n");
                let _ = conn.write_all(head("302 Found", 0, &extra).as_bytes());
            }
            Some(Route::Slow(..)) => unreachable!("上の loop で剥がした"),
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
        /// 起こし直した子が終わったときの終了コード。
        child_exit: i32,
        asked: Vec<String>,
        told: Vec<String>,
        said: Vec<String>,
        shown: Vec<String>,
        /// `progress` で書き直した行（`progress_end` で確定したものは `finished` にも入る）。
        progress: Vec<String>,
        finished: Vec<String>,
        interactive: bool,
        enter_waits: usize,
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
        fn show(&mut self, text: &str) {
            self.shown.push(text.to_string());
        }
        fn progress(&mut self, line: &str) {
            self.progress.push(line.to_string());
        }
        fn progress_end(&mut self) {
            if let Some(last) = self.progress.last()
                && self.finished.last() != Some(last)
            {
                self.finished.push(last.clone());
            }
        }
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn wait_enter(&mut self) {
            self.enter_waits += 1;
        }
        fn restart(&mut self, exe: &Path, args: &[OsString]) -> Result<i32, String> {
            self.exe_at_restart = std::fs::read(exe).ok();
            self.restarts.push((exe.to_path_buf(), args.to_vec()));
            match &self.restart_error {
                Some(e) => Err(e.clone()),
                None => Ok(self.child_exit),
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
        h.asked
            .iter()
            .chain(&h.told)
            .chain(&h.said)
            .chain(&h.shown)
            .chain(&h.progress)
            .collect()
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
            Outcome::Restarted(0)
        );
        assert!(host.asked[0].contains("Update now?"), "{}", host.asked[0]);
        assert!(
            host.progress.iter().any(|l| l.starts_with("Downloading ")),
            "{:?}",
            host.progress
        );
        assert!(host.said.iter().any(|l| l.starts_with("Update: replacing")));
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
            Outcome::Restarted(0)
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

    // 壊れたら落ちる: 子の終了コードが親の終了コードにならない（失敗した子でも 0 で終わる）、
    // または子が失敗して終わっただけで更新を元に戻す（起こせたのだから戻さない）。
    #[test]
    fn the_parent_ends_with_the_exit_code_of_the_restarted_child() {
        for code in [0, 1, 7] {
            let dir = temp_dir("child_exit");
            install(&dir);
            let server = Server::start(happy_routes());
            let mut host = FakeHost {
                answer: true,
                child_exit: code,
                ..FakeHost::default()
            };
            assert_eq!(
                go(&dir, &server, &mut host, OLD_VERSION),
                Outcome::Restarted(code)
            );
            assert_eq!(
                read(&dir, "HiganRTool.exe"),
                file_bytes("HiganRTool.exe", NEW_VERSION),
                "子が {code} で終わっただけで更新を戻した"
            );
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    /// 手元に無いリプレイを直す道具が目録に載っていても、更新は壊れずにその名前で新しく置く（v1.0.0 の手順も同じ）。
    #[test]
    fn a_new_replay_fix_exe_in_the_manifest_is_placed_and_removed_again_when_the_restart_fails() {
        const FIX: &str = "HiganRTool_replay_fix.exe";
        assert!(update::SHIPPED_FILES.contains(&FIX));
        let fix = file_bytes(FIX, NEW_VERSION);
        let body = format!(
            "{}file {FIX} {} {}\n",
            sample_body(NEW_VERSION),
            fix.len(),
            update::to_hex(&update::sha256(&fix))
        );
        let routes = |extra: &Vec<u8>| {
            let mut r = manifest_routes(&sign(&body), NEW_VERSION);
            r.insert(format!("/update/files/{FIX}"), Route::Body(extra.clone()));
            r
        };

        let dir = temp_dir("replay_fix_new");
        install(&dir);
        assert!(!dir.join(FIX).exists(), "手元には無いはず");
        let server = Server::start(routes(&fix));
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            go(&dir, &server, &mut host, OLD_VERSION),
            Outcome::Restarted(0)
        );
        assert_eq!(read(&dir, FIX), fix);
        assert!(!dir.join(update::old_name(FIX)).exists());
        assert!(!dir.join(update::new_name(FIX)).exists());
        for n in NAMES {
            assert_eq!(read(&dir, n), file_bytes(n, NEW_VERSION), "{n}");
        }
        std::fs::remove_dir_all(&dir).expect("後片づけ");

        let dir = temp_dir("replay_fix_new_undo");
        install(&dir);
        let server = Server::start(routes(&fix));
        let mut host = FakeHost {
            answer: true,
            restart_error: Some("起こせない".to_string()),
            ..FakeHost::default()
        };
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        assert!(!dir.join(FIX).exists(), "新しく置いた道具が戻されていない");
        assert_untouched(&dir);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn leftover_replay_fix_files_are_cleaned_like_the_other_shipped_files() {
        let dir = temp_dir("replay_fix_cleanup");
        for n in [
            "HiganRTool_replay_fix.exe.new",
            "HiganRTool_replay_fix.exe.old",
        ] {
            std::fs::write(dir.join(n), "x").expect("残り物");
        }
        let left = cleanup_leftovers(&dir, 1, Duration::from_millis(1), true);
        assert!(left.is_empty(), "{left:?}");
        for n in [
            "HiganRTool_replay_fix.exe.new",
            "HiganRTool_replay_fix.exe.old",
        ] {
            assert!(!dir.join(n).exists(), "{n}");
        }
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

    /// 本番の道（配るサーバー → 配布元の Releases → 配信の宿）を、別ポートの 3 つの宿に見立てる。
    /// `delays` は目録の応答に入る前の、宿ごとの待ち（ファイルは待たせない）。
    struct Chain {
        front: Server,
        release: Server,
        objects: Server,
    }

    fn chain(manifest: &str, version: u32, delays: [Duration; 3]) -> Chain {
        let slow = |d: Duration, r: Route| Route::Slow(d, Box::new(r));
        let mut objects = HashMap::new();
        objects.insert(
            "/obj/manifest.txt".to_string(),
            slow(delays[2], Route::Body(manifest.as_bytes().to_vec())),
        );
        for n in NAMES {
            objects.insert(format!("/obj/{n}"), Route::Body(file_bytes(n, version)));
        }
        let objects = Server::start(objects);
        let mut release = HashMap::new();
        release.insert(
            "/rel/manifest.txt".to_string(),
            slow(
                delays[1],
                Route::RedirectTo(format!("{}/obj/manifest.txt", objects.base())),
            ),
        );
        for n in NAMES {
            release.insert(
                format!("/rel/{n}"),
                Route::RedirectTo(format!("{}/obj/{n}", objects.base())),
            );
        }
        let release = Server::start(release);
        let mut front = HashMap::new();
        front.insert(
            "/update/manifest".to_string(),
            slow(
                delays[0],
                Route::RedirectTo(format!("{}/rel/manifest.txt", release.base())),
            ),
        );
        for n in NAMES {
            front.insert(
                format!("/update/files/{n}"),
                Route::RedirectTo(format!("{}/rel/{n}", release.base())),
            );
        }
        Chain {
            front: Server::start(front),
            release,
            objects,
        }
    }

    const NO_WAIT: [Duration; 3] = [Duration::ZERO; 3];

    fn each_hop(ms: u64) -> [Duration; 3] {
        [Duration::from_millis(ms); 3]
    }

    #[test]
    fn a_manifest_and_files_behind_two_redirects_across_three_hosts_are_applied() {
        let dir = temp_dir("chain_happy");
        install(&dir);
        let c = chain(&sign(&sample_body(NEW_VERSION)), NEW_VERSION, NO_WAIT);
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            go(&dir, &c.front, &mut host, OLD_VERSION),
            Outcome::Restarted(0)
        );
        for n in NAMES {
            assert_eq!(read(&dir, n), file_bytes(n, NEW_VERSION), "{n}");
        }
        assert_eq!(c.front.hits()[0], "/update/manifest");
        assert_eq!(c.release.hits()[0], "/rel/manifest.txt");
        assert_eq!(c.objects.hits()[0], "/obj/manifest.txt");
        for n in NAMES {
            let want = |h: Vec<String>, p: &str| h.contains(&format!("{p}{n}"));
            assert!(want(c.front.hits(), "/update/files/"), "{n}");
            assert!(want(c.release.hits(), "/rel/"), "{n}");
            assert!(want(c.objects.hits(), "/obj/"), "{n}");
        }
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn a_manifest_answered_with_200_by_the_front_itself_is_applied_in_one_request() {
        let dir = temp_dir("front_direct");
        install(&dir);
        let mut routes = happy_routes();
        routes.insert(
            "/update/manifest".to_string(),
            Route::Body(sign(&sample_body(NEW_VERSION)).into_bytes()),
        );
        let server = Server::start(routes);
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            go(&dir, &server, &mut host, OLD_VERSION),
            Outcome::Restarted(0)
        );
        let hits = server.hits();
        assert_eq!(hits[0], "/update/manifest");
        assert!(!hits.contains(&"/rel/manifest.txt".to_string()), "{hits:?}");
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    #[test]
    fn waits_on_each_of_three_hosts_count_against_one_two_second_budget() {
        // 1 つの宿の待ちは 1.5 秒の上限の内でも、3 つ合わせて 2 秒を越えれば見切る。
        for (what, delays, applied) in [
            ("3 つで 1.5 秒", each_hop(500), true),
            ("3 つで 2.4 秒", each_hop(800), false),
        ] {
            let dir = temp_dir("chain_budget");
            install(&dir);
            let c = chain(&sign(&sample_body(NEW_VERSION)), NEW_VERSION, delays);
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            let t0 = Instant::now();
            let out = go(&dir, &c.front, &mut host, OLD_VERSION);
            let took = t0.elapsed();
            if applied {
                assert_eq!(out, Outcome::Restarted(0), "{what}: {took:?}");
            } else {
                assert_eq!(out, Outcome::Proceed, "{what}");
                assert!(
                    took >= Duration::from_millis(1900) && took < Duration::from_millis(2800),
                    "{what}: {took:?}"
                );
                assert!(host.asked.is_empty(), "{what}");
                assert_untouched(&dir);
            }
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }
    }

    /// 遅延と打ち切りの測定（`cargo test -p higanr-loader chain_latency_table -- --ignored --nocapture`）。
    #[test]
    #[ignore = "測定の表を出すだけ（時間がかかる）"]
    fn chain_latency_table() {
        const N: usize = 5;
        let manifest = sign(&sample_body(NEW_VERSION));
        let ms = Duration::from_millis;
        let mut rows: Vec<(String, [Duration; 3])> = Vec::new();
        for d in [0u64, 250, 500, 600, 650, 700, 800, 1000] {
            rows.push((format!("全宿 {d} ms"), [ms(d); 3]));
        }
        for d in [1000u64, 1400, 1600, 2100] {
            for (i, host) in ["配るサーバー", "Releases", "配信の宿"].iter().enumerate() {
                let mut delays = [Duration::ZERO; 3];
                delays[i] = ms(d);
                rows.push((format!("{host} だけ {d} ms"), delays));
            }
        }
        println!("| 条件 | 合計待ち | n | 取れた | 経過 最小/中央/最大 (ms) | 失敗の文言 |");
        println!("|---|---:|---:|---:|---|---|");
        for (what, delays) in rows {
            let c = chain(&manifest, NEW_VERSION, delays);
            let mut times = Vec::new();
            let mut ok = 0;
            let mut why = String::new();
            for _ in 0..N {
                let mut net = net_for(&c.front);
                let t0 = Instant::now();
                let r = net.get_manifest();
                times.push(t0.elapsed().as_millis());
                match r {
                    Ok(b) if b == manifest.as_bytes() => ok += 1,
                    Ok(_) => why = "本文が違う".to_string(),
                    Err(e) => why = e,
                }
            }
            times.sort_unstable();
            let sum: u128 = delays.iter().map(Duration::as_millis).sum();
            println!(
                "| {what} | {sum} | {N} | {ok} | {}/{}/{} | {why} |",
                times[0],
                times[N / 2],
                times[N - 1]
            );
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
        fn get_file(
            &mut self,
            name: &str,
            _size: u64,
            _progress: &mut dyn FnMut(u64),
        ) -> Result<Vec<u8>, String> {
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

    /// ツールのフォルダの外にあるゲームのフォルダ（実在しなくてよい。検査は何も書かない）。
    const GAME_ELSEWHERE: &str = "Z:\\HiganR_unit_game_elsewhere";

    fn start<'a>(exe: &'a Path, args: &'a [OsString], dist: bool, build: u32) -> Start<'a> {
        start_game(exe, args, dist, build, Some(Path::new(GAME_ELSEWHERE)))
    }

    fn start_game<'a>(
        exe: &'a Path,
        args: &'a [OsString],
        dist: bool,
        build: u32,
        game_dir: Option<&'a Path>,
    ) -> Start<'a> {
        Start {
            dist,
            build_number: build,
            exe,
            dev_args: false,
            just_updated: false,
            inherited_ini: false,
            args,
            key: Some(public_key()),
            game_dir,
            lang: Lang::Ja,
            ini_template: TEMPLATE,
            notes: NOTES,
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
    }

    #[test]
    fn in_game_folder_is_known_from_the_exe_in_the_folder() {
        let root = temp_dir("in_game_rule");
        let sub = root.join("Game").join("tools");
        std::fs::create_dir_all(&sub).expect("フォルダ");
        assert!(!in_game_folder(&sub), "th09.exe が無い");
        std::fs::write(sub.join("Th09.Exe"), "x").expect("大文字小文字違い");
        assert!(in_game_folder(&sub), "exe のフォルダの th09.exe");
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
        let elsewhere = Path::new(GAME_ELSEWHERE);
        assert_eq!(note_launched_at(&dir, 0, elsewhere, Lang::Ja), Ok(false));
        assert!(!dir.join(update::LAUNCHED_MARK).exists());
        assert_eq!(
            note_launched_at(&dir, NEW_VERSION, elsewhere, Lang::Ja),
            Ok(true)
        );
        assert_eq!(
            read(&dir, update::LAUNCHED_MARK),
            format!("{NEW_VERSION}\n").into_bytes()
        );
        let game = temp_dir("note_game");
        std::fs::write(game.join("th09.exe"), "game").expect("ゲーム");
        assert_eq!(
            note_launched_at(&game, NEW_VERSION, elsewhere, Lang::Ja),
            Ok(false)
        );
        assert!(
            !game.join(update::LAUNCHED_MARK).exists(),
            "ゲームのフォルダに印"
        );
        // th09.exe が無い、ゲームのフォルダの下の階層。
        let sub = game.join("tools");
        std::fs::create_dir_all(&sub).expect("下の階層");
        assert_eq!(
            note_launched_at(&sub, NEW_VERSION, &game, Lang::Ja),
            Ok(false)
        );
        assert!(
            !sub.join(update::LAUNCHED_MARK).exists(),
            "ゲームの下の階層に印"
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
        std::fs::remove_dir_all(&game).expect("後片づけ");
    }

    // 何が壊れたら落ちるか: ツールをゲームのフォルダの下の階層（th09.exe が無い）に置いた初回で、
    // ゲームのフォルダを見ないと更新の確認が `.new` などをその階層に書く。決まる前に確かめる、中でも確かめる、外で確かめない、が落ちる。
    #[test]
    fn the_check_waits_for_the_game_folder_and_stays_out_of_it() {
        let root = temp_dir("below_game");
        let (game, other) = (root.join("Game"), root.join("Other"));
        let tool = game.join("tools");
        for d in [&tool, &other] {
            std::fs::create_dir_all(d).expect("フォルダ");
        }
        install(&tool);
        std::fs::write(tool.join("HiganRTool.exe.new"), "x").expect("残り物");
        std::fs::write(tool.join("README.txt.old"), "x").expect("残り物");
        let exe = tool.join("HiganRTool.exe");
        let args: Vec<OsString> = Vec::new();
        let snapshot = |d: &Path| {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .expect("読める")
                .flatten()
                .map(|e| e.file_name())
                .collect();
            v.sort();
            v
        };
        let before = snapshot(&tool);

        // 決まっていない: 通信もウィンドウも片づけもしない。
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        let c = start_game(&exe, &args, true, OLD_VERSION, None);
        assert_eq!(start_with(&c, &mut net, &mut host), Outcome::Proceed);
        assert!(
            net.calls.is_empty() && host.asked.is_empty(),
            "決まる前に確かめた"
        );
        assert_eq!(snapshot(&tool), before);

        // 中（同じ・上の階層で名指し・`..` を含む別名）: 同じく何もしない。
        let upper = other.join("..").join("Game");
        for g in [&game, &upper] {
            let mut net = fake_net();
            let mut host = FakeHost {
                answer: true,
                ..FakeHost::default()
            };
            let c = start_game(&exe, &args, true, OLD_VERSION, Some(g));
            assert_eq!(start_with(&c, &mut net, &mut host), Outcome::Proceed);
            assert!(net.calls.is_empty() && host.asked.is_empty(), "{g:?}");
            assert!(host.restarts.is_empty());
            assert_eq!(
                snapshot(&tool),
                before,
                "{g:?}: ゲームのフォルダの中を変えた"
            );
        }

        // 外: 今までどおり尋ねて、「はい」なら置き換えて起こし直す。
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        let c = start_game(&exe, &args, true, OLD_VERSION, Some(&other));
        assert_eq!(start_with(&c, &mut net, &mut host), Outcome::Restarted(0));
        assert_eq!(host.asked.len(), 1);
        assert_eq!(host.restarts.len(), 1);
        assert_eq!(
            read(&tool, "HiganRTool.dll"),
            file_bytes("HiganRTool.dll", NEW_VERSION)
        );
        std::fs::remove_dir_all(&root).expect("後片づけ");
    }

    // 何が壊れたら落ちるか: 起こし直しの引数が元の引数でなくなる、選ぶウィンドウがもう一度出る。
    #[test]
    fn the_restart_keeps_the_arguments_except_the_one_that_reopens_the_picker() {
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            restart_args(a(&["--choose-game", "--dll", "x.dll"])),
            a(&["--dll", "x.dll"])
        );
        assert_eq!(
            restart_args(a(&["--game", "D:\\g\\th09.exe"])),
            a(&["--game", "D:\\g\\th09.exe"])
        );
        assert!(restart_args(a(&[])).is_empty());

        let dir = temp_dir("restart_args");
        install(&dir);
        let exe = dir.join("HiganRTool.exe");
        let args = restart_args(a(&["--choose-game", "--no-clipboard-x"]));
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        let c = start(&exe, &args, true, OLD_VERSION);
        assert_eq!(start_with(&c, &mut net, &mut host), Outcome::Restarted(0));
        assert_eq!(host.restarts[0].1, a(&["--no-clipboard-x"]));
        std::fs::remove_dir_all(&dir).expect("後片づけ");
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
        let mut host = RealHost::default();
        // 子が終わるまで待って、その終了コードを返す。
        for code in [0, 3] {
            assert_eq!(
                host.restart(
                    &cmd,
                    &[
                        OsString::from("/c"),
                        OsString::from("exit"),
                        OsString::from(code.to_string())
                    ]
                ),
                Ok(code)
            );
        }
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

    // ---- 進み具合・更新の後の初回 ----

    const TEMPLATE: &str = "[netplay]\nname = HiganRTool\nenable = 1\n";
    const NOTES: &str = "テスト版の変更点\n・1 行目\n";

    fn percent_of(line: &str) -> u64 {
        line.trim_end_matches('%')
            .rsplit(' ')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(u64::MAX)
    }

    // 壊れたら落ちる: 進み具合の行が、名前・何個目・受け取った量 / 全体・割合のどれかを欠く、割合が戻る、行が縮む（書き直しで前の字が残る）。
    #[test]
    fn the_progress_line_names_the_file_the_bytes_and_the_percent() {
        let mut p = Progress::new("HiganRTool.dll", 2, 3, 3_500_000, Lang::Ja);
        assert_eq!(
            p.line(0),
            "ダウンロード中 HiganRTool.dll (2/3)  0.0 / 3.3 MB    0%"
        );
        assert_eq!(p.update(10), None, "割合が変わらないのに書き直した");
        let mid = p.update(1_750_000).expect("50% で書き直す");
        assert_eq!(
            mid,
            "ダウンロード中 HiganRTool.dll (2/3)  1.7 / 3.3 MB   50%"
        );
        assert_eq!(p.update(1_750_001), None);
        let done = p.update(3_500_000).expect("100%");
        assert!(done.ends_with("3.3 / 3.3 MB  100%"), "{done}");
        assert_eq!(p.update(3_500_000), None);
        // 大きさを越えて受けても 100% で止まる。
        assert_eq!(p.percent(9_999_999), 100);

        let small = Progress::new("README.txt", 3, 3, 2048, Lang::En);
        assert_eq!(
            small.line(1024),
            "Downloading README.txt (3/3)  1.0 / 2.0 KB   50%"
        );
        assert!(!has_japanese(&small.line(2048)));
        let empty = Progress::new("x.txt", 1, 1, 0, Lang::En);
        assert_eq!(percent_of(&empty.line(0)), 100, "大きさ 0 は 100%");

        // 受け取りが進んでも、行の長さは縮まない。
        let mut p = Progress::new("HiganRTool.exe", 1, 3, 12_345_678, Lang::Ja);
        let mut widest = p.line(0).chars().count();
        for got in (0..=12_345_678u64).step_by(100_000) {
            if let Some(l) = p.update(got) {
                assert!(l.chars().count() >= widest, "{l}");
                widest = l.chars().count();
            }
        }
    }

    // 壊れたら落ちる: ダウンロードの進み具合が host に届かない、ファイルごとに確定しない、割合が 100% で終わらない。
    #[test]
    fn each_file_gets_a_progress_line_that_ends_at_100_percent() {
        let dir = temp_dir("progress");
        install(&dir);
        let server = Server::start(happy_routes());
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(
            go(&dir, &server, &mut host, OLD_VERSION),
            Outcome::Restarted(0)
        );
        assert_eq!(host.finished.len(), NAMES.len(), "{:?}", host.finished);
        for (line, name) in host.finished.iter().zip(NAMES) {
            assert!(line.contains(name), "{line}");
            assert_eq!(percent_of(line), 100, "{line}");
        }
        let percents: Vec<u64> = host.progress.iter().map(|l| percent_of(l)).collect();
        assert_eq!(percents[0], 0, "最初は 0% から");
        assert!(host.progress[0].contains("(1/3)"), "{:?}", host.progress);
        assert!(
            host.progress.iter().any(|l| l.contains("(3/3)")),
            "{:?}",
            host.progress
        );
        let replacing = host
            .said
            .iter()
            .position(|l| l.contains("置き換えています"));
        let found = host
            .said
            .iter()
            .position(|l| l.contains("更新データがありました"));
        assert!(
            matches!((found, replacing), (Some(a), Some(b)) if a < b),
            "{:?}",
            host.said
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: 失敗したファイルの行が確定されず、次の文が書き直しの行に重なる。
    #[test]
    fn a_failed_download_still_ends_its_progress_line() {
        let dir = temp_dir("progress_fail");
        install(&dir);
        let mut routes = happy_routes();
        routes.insert(
            "/update/files/HiganRTool.dll".to_string(),
            Route::Status(404),
        );
        let server = Server::start(routes);
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(go(&dir, &server, &mut host, OLD_VERSION), Outcome::Proceed);
        assert!(
            host.finished.iter().any(|l| l.contains("HiganRTool.dll")),
            "{:?}",
            host.finished
        );
        assert_eq!(host.told.len(), 1);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: 受け取った量が途中経過として渡らない（大きなファイルで画面が止まって見える）。
    #[test]
    fn the_http_get_reports_a_growing_byte_count_while_receiving() {
        let body = vec![7u8; 300_000];
        let mut routes = HashMap::new();
        routes.insert(
            "/update/files/README.txt".to_string(),
            Route::Body(body.clone()),
        );
        let server = Server::start(routes);
        let mut net = net_for(&server);
        let mut seen: Vec<u64> = Vec::new();
        let got = net
            .get_file("README.txt", body.len() as u64, &mut |n| seen.push(n))
            .expect("取れる");
        assert_eq!(got, body);
        assert!(seen.len() >= 2, "途中経過が 1 回以下: {seen:?}");
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        assert_eq!(seen.last().copied(), Some(body.len() as u64));
    }

    /// 更新して起こし直した初回の起動（外のゲームのフォルダ、通常の引数）の入口。
    fn after_update(dir: &Path, edit: impl FnOnce(&mut Start<'_>), host: &mut FakeHost) -> Outcome {
        let exe = dir.join("HiganRTool.exe");
        let args: Vec<OsString> = Vec::new();
        let mut c = start(&exe, &args, true, NEW_VERSION);
        c.just_updated = true;
        edit(&mut c);
        start_with(&c, &mut fake_net(), host)
    }

    fn ini_of(dir: &Path) -> String {
        String::from_utf8(read(dir, "HiganRTool.ini")).expect("UTF-8")
    }

    const UPGRADED_INI: &str = "[netplay]\nname = keep-me\nenable = 1\n";

    // 壊れたら落ちる: 更新の後の初回に、ini が新しい形にならない・変更点が出ない・人のコンソールで Enter を待たない。
    #[test]
    fn the_first_launch_after_an_update_shows_the_notes_and_waits_for_enter() {
        let dir = temp_dir("after_update");
        install(&dir);
        let mut host = FakeHost {
            interactive: true,
            ..FakeHost::default()
        };
        assert_eq!(after_update(&dir, |_| {}, &mut host), Outcome::Proceed);
        assert_eq!(ini_of(&dir), UPGRADED_INI);
        assert_eq!(host.enter_waits, 1);
        assert!(
            host.shown.iter().any(|t| t.contains("・1 行目")),
            "{:?}",
            host.shown
        );
        assert_eq!(
            host.shown.last().map(String::as_str),
            Some("Enterキーで花映塚を起動します。")
        );
        assert!(
            host.said
                .iter()
                .any(|l| l.contains("新しい形に合わせました")),
            "{:?}",
            host.said
        );
        // 変更点が先、Enter の案内が最後。起動はその後（戻り値が Proceed）。
        assert!(host.asked.is_empty() && host.restarts.is_empty());
        for n in NAMES {
            assert_eq!(read(&dir, n), file_bytes(n, OLD_VERSION), "{n} に触った");
        }
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: パイプや検査の道具からの起動で Enter を待って止まる。
    #[test]
    fn a_console_that_is_not_a_person_never_waits() {
        let dir = temp_dir("after_update_piped");
        install(&dir);
        let mut host = FakeHost::default(); // interactive = false
        assert_eq!(after_update(&dir, |_| {}, &mut host), Outcome::Proceed);
        assert_eq!(host.enter_waits, 0);
        assert!(
            host.shown.iter().all(|t| !t.contains("Enter")),
            "{:?}",
            host.shown
        );
        assert_eq!(ini_of(&dir), UPGRADED_INI, "ini は合わせる");
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: 台本・開発用の引数・継いだ ini・ゲームのフォルダの中・開発版・2 回目の起動で、変更点や ini の書き換えが出る。
    #[test]
    fn nothing_happens_unless_it_is_a_human_first_launch_after_an_update() {
        type Edit = fn(&mut Start<'_>);
        let cases: [(&str, Edit); 4] = [
            ("開発用の引数（台本・背景）", |c| {
                c.dev_args = true
            }),
            ("継いだ ini", |c| c.inherited_ini = true),
            ("開発版", |c| c.dist = false),
            ("更新の後ではない（2 回目以降）", |c| {
                c.just_updated = false
            }),
        ];
        for (what, edit) in cases {
            let dir = temp_dir("after_update_none");
            install(&dir);
            let mut host = FakeHost {
                interactive: true,
                ..FakeHost::default()
            };
            let _ = after_update(&dir, edit, &mut host);
            assert!(host.shown.is_empty(), "{what}: {:?}", host.shown);
            assert_eq!(host.enter_waits, 0, "{what}");
            assert_eq!(
                read(&dir, "HiganRTool.ini"),
                b"[netplay]\nname = keep-me\n",
                "{what}"
            );
            assert!(!host.said.iter().any(|l| l.contains("新しい形")), "{what}");
            std::fs::remove_dir_all(&dir).expect("後片づけ");
        }

        let dir = temp_dir("after_update_in_game");
        install(&dir);
        std::fs::write(dir.join("TH09.EXE"), "game").expect("ゲーム");
        let mut host = FakeHost {
            interactive: true,
            ..FakeHost::default()
        };
        let _ = after_update(&dir, |_| {}, &mut host);
        assert!(host.shown.is_empty() && host.enter_waits == 0);
        assert_eq!(read(&dir, "HiganRTool.ini"), b"[netplay]\nname = keep-me\n");
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: ini が無いのに作る、ini が無いと変更点まで出なくなる。
    #[test]
    fn without_an_ini_the_notes_still_show_and_no_ini_is_made() {
        let dir = temp_dir("after_update_no_ini");
        install(&dir);
        std::fs::remove_file(dir.join("HiganRTool.ini")).expect("消す");
        let mut host = FakeHost {
            interactive: true,
            ..FakeHost::default()
        };
        let _ = after_update(&dir, |_| {}, &mut host);
        assert!(!dir.join("HiganRTool.ini").exists());
        assert_eq!(host.enter_waits, 1);
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }

    // 壊れたら落ちる: 更新の後の初回でも、その回がさらに更新を確かめる（更新が続く事故の歯止め）。
    #[test]
    fn the_first_launch_after_an_update_does_not_check_again() {
        let dir = temp_dir("after_update_no_check");
        install(&dir);
        let exe = dir.join("HiganRTool.exe");
        let args: Vec<OsString> = Vec::new();
        let mut c = start(&exe, &args, true, OLD_VERSION);
        c.just_updated = true;
        let mut net = fake_net();
        let mut host = FakeHost {
            answer: true,
            ..FakeHost::default()
        };
        assert_eq!(start_with(&c, &mut net, &mut host), Outcome::Proceed);
        assert!(
            net.calls.is_empty() && host.asked.is_empty(),
            "{:?}",
            net.calls
        );
        std::fs::remove_dir_all(&dir).expect("後片づけ");
    }
}
