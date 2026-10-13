//! HiganRTool.exe —— 起動用の exe。th09.exe を止めた状態で起動し、HiganRTool.dll を読み込ませてから動かす。
//!
//! 手順と後始末は `launch.rs`、本物の Win32 は `real.rs`、引数は `cli.rs`。`game_pick.rs` は `--game` なしの起動で th09.exe の場所を
//! 選ばせ、`update.rs` は配るバージョンの起動前の更新、`code_relay.rs` は接続コードの受け渡しのウィンドウ、`freshness.rs` は古い成果物での起動を断る。
//! OS のキーボード・マウスの注入や前面を奪う呼び出しはしない。ゲームのフォルダには何も書かない。

// unsafe を書いてよいのは real.rs・folder_dialog.rs・http_win.rs だけ。
#[forbid(unsafe_code)]
mod cli;
#[forbid(unsafe_code)]
mod code_relay;
#[forbid(unsafe_code)]
mod env_block;
mod folder_dialog;
#[forbid(unsafe_code)]
mod freshness;
#[forbid(unsafe_code)]
mod game_pick;
#[forbid(unsafe_code)]
mod hold;
mod http_win;
#[forbid(unsafe_code)]
mod launch;
#[forbid(unsafe_code)]
mod ledger;
mod real;
#[forbid(unsafe_code)]
mod selftest;
#[forbid(unsafe_code)]
mod synth;
#[forbid(unsafe_code)]
mod ui_lang;
#[forbid(unsafe_code)]
mod update;
#[forbid(unsafe_code)]
mod win;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use higanr_core::lang::Lang;
use launch::{CleanupIssue, LaunchError, LaunchRequest};
use ui_lang::by_lang;
use win::Win32;

/// この exe と同じフォルダの `HiganRTool.dll`（`target\...\release` では cdylib の `higanrtool.dll`。大文字小文字は区別しない）。
/// 自己検査は日本語のまま使う。
pub(crate) fn default_dll_path(exe: &Path) -> Result<PathBuf, String> {
    default_dll_path_in(exe, Lang::Ja)
}

fn default_dll_path_in(exe: &Path, lang: Lang) -> Result<PathBuf, String> {
    exe.parent()
        .map(|d| d.join("HiganRTool.dll"))
        .ok_or_else(|| folder_unknown(exe, lang))
}

fn folder_unknown(exe: &Path, lang: Lang) -> String {
    let shown = exe.display();
    by_lang!(
        lang,
        ja: "{shown} のフォルダが分からない",
        en: "Cannot tell the folder of {shown}"
    )
}

/// 起動の引数で ini や DLL の場所が分かったら、その場所で言語を決め直す。
fn refine_lang(lang: Lang, ini: Option<&Path>, dll: Option<&Path>, exe: Option<&Path>) -> Lang {
    if ini.is_none() && dll.is_none() {
        return lang;
    }
    let dll_dir = dll
        .and_then(|d| std::path::absolute(d).ok())
        .and_then(|d| d.parent().map(Path::to_path_buf))
        .or_else(|| exe.and_then(Path::parent).map(Path::to_path_buf));
    match dll_dir {
        Some(dir) => ui_lang::detect(ini, &dir, real::user_ui_langid()),
        None => lang,
    }
}

/// 起動の最初に、この exe のフォルダを DLL のフォルダとして言語を決める。
fn initial_lang() -> Lang {
    let os = real::user_ui_langid();
    match std::env::current_exe() {
        Ok(me) => match me.parent() {
            Some(dir) => ui_lang::detect(None, dir, os),
            None => ui_lang::decide(None, os),
        },
        Err(_) => ui_lang::decide(None, os),
    }
}

const ELEVATION_REQUIRED: win::ErrorCode = 740;

fn describe(e: &LaunchError, lang: Lang) -> String {
    match e {
        LaunchError::DllPathInvalid(why) => {
            let why = why.get(lang);
            by_lang!(lang, ja: "DLL のパスが使えません（{why}）", en: "The DLL path cannot be used ({why})")
        }
        LaunchError::DllUnreadable(why) => {
            by_lang!(lang, ja: "DLL が読めません（{why}）", en: "The DLL cannot be read ({why})")
        }
        LaunchError::DllNotPe32I386(why) => by_lang!(
            lang,
            ja: "DLL が 32bit の DLL ではありません（{why:?}）",
            en: "The DLL is not a 32-bit DLL ({why:?})"
        ),
        LaunchError::GamePathInvalid => match lang {
            Lang::Ja => "exe のフォルダが分かりません",
            Lang::En => "Cannot tell the folder of the exe",
        }
        .to_string(),
        LaunchError::GameUnreadable(why) => {
            by_lang!(lang, ja: "exe が読めません（{why}）", en: "The exe cannot be read ({why})")
        }
        LaunchError::GameIdentMismatch(fields) => game_pick::ident_mismatch_text(fields, lang),
        LaunchError::LoadLibraryAddress(e) => by_lang!(
            lang,
            ja: "LoadLibraryW のアドレスが引けません（GetLastError {e}）",
            en: "Cannot find the address of LoadLibraryW (GetLastError {e})"
        ),
        // 740 = ERROR_ELEVATION_REQUIRED。th09.exe が「管理者として実行」の設定のとき。
        LaunchError::CreateProcess(ELEVATION_REQUIRED) => by_lang!(
            lang,
            ja: "起動できません（CreateProcessW、GetLastError {ELEVATION_REQUIRED}）。th09.exe が「管理者として実行」の設定になっています。HiganRTool.exe も管理者として起動してください（ショートカットのプロパティ →「詳細設定」→「管理者として実行」）",
            en: "Cannot start (CreateProcessW, GetLastError {ELEVATION_REQUIRED}). th09.exe is set to run as administrator. Run HiganRTool.exe as administrator too (shortcut properties > Advanced > Run as administrator)"
        ),
        LaunchError::CreateProcess(e) => by_lang!(
            lang,
            ja: "起動できません（CreateProcessW、GetLastError {e}）",
            en: "Cannot start (CreateProcessW, GetLastError {e})"
        ),
        LaunchError::VirtualAlloc(e) => by_lang!(
            lang,
            ja: "ゲームの中に領域を確保できません（GetLastError {e}）",
            en: "Cannot allocate memory inside the game (GetLastError {e})"
        ),
        LaunchError::WriteMemory(e) => by_lang!(
            lang,
            ja: "DLL のパスを書き込めません（GetLastError {e}）",
            en: "Cannot write the DLL path (GetLastError {e})"
        ),
        LaunchError::CreateRemoteThread(e) => by_lang!(
            lang,
            ja: "読み込みのスレッドを作れません（GetLastError {e}）",
            en: "Cannot create the loading thread (GetLastError {e})"
        ),
        LaunchError::RemoteThreadTimedOut => {
            let secs = launch::REMOTE_THREAD_TIMEOUT_MS / 1000;
            by_lang!(
                lang,
                ja: "DLL の読み込みが {secs} 秒で終わりませんでした",
                en: "Loading the DLL did not finish within {secs} seconds"
            )
        }
        LaunchError::RemoteThreadWaitFailed(e) => by_lang!(
            lang,
            ja: "DLL の読み込みを待てませんでした（GetLastError {e}）",
            en: "Cannot wait for the DLL to load (GetLastError {e})"
        ),
        LaunchError::LoadLibraryReturnedZero => match lang {
            Lang::Ja => "ゲームの中で DLL が読み込めませんでした（LoadLibraryW が 0。DLL が自分から断った場合を含む）",
            Lang::En => "The DLL could not be loaded inside the game (LoadLibraryW returned 0; this includes the DLL refusing to load by itself)",
        }
        .to_string(),
        LaunchError::ExitCodeUnavailable(e) => by_lang!(
            lang,
            ja: "DLL の読み込みの結果が読めません（GetLastError {e}）",
            en: "Cannot read the result of loading the DLL (GetLastError {e})"
        ),
        LaunchError::ResumeFailed(e) => by_lang!(
            lang,
            ja: "ゲームを動かせません（ResumeThread、GetLastError {e}）",
            en: "Cannot let the game run (ResumeThread, GetLastError {e})"
        ),
        LaunchError::PatchRead(at, e) => by_lang!(
            lang,
            ja: "2 つ目の起動の書き換え: 0x{at:08X} が読めません（GetLastError {e}）。何も書いていません",
            en: "Start-up patch: cannot read 0x{at:08X} (GetLastError {e}). Nothing was written"
        ),
        LaunchError::PatchMismatch { at, found } => by_lang!(
            lang,
            ja: "2 つ目の起動の書き換え: 0x{at:08X} が期待のバイト列と違います（{found:02X?}）。何も書いていません",
            en: "Start-up patch: 0x{at:08X} does not hold the expected bytes ({found:02X?}). Nothing was written"
        ),
        LaunchError::PatchWrite(at, e) => by_lang!(
            lang,
            ja: "2 つ目の起動の書き換え: 0x{at:08X} に書けません（GetLastError {e}）",
            en: "Start-up patch: cannot write to 0x{at:08X} (GetLastError {e})"
        ),
        LaunchError::PatchVerify { at, found } => by_lang!(
            lang,
            ja: "2 つ目の起動の書き換え: 0x{at:08X} を読み直すと書いた値になっていません（{found:02X?}）",
            en: "Start-up patch: reading 0x{at:08X} back does not give the value that was written ({found:02X?})"
        ),
    }
}

fn aftermath(e: &LaunchError, lang: Lang) -> &'static str {
    match (e.row(), lang) {
        (1..=3, Lang::Ja) => "何も起動していません。",
        (1..=3, Lang::En) => "Nothing was started.",
        (_, Lang::Ja) => "起動しかけたゲームは、動かさずに終わらせました。",
        (_, Lang::En) => "The game that was starting was ended without running it.",
    }
}

fn print_issues(issues: &[CleanupIssue], lang: Lang) {
    for i in issues {
        eprintln!(
            "{}",
            by_lang!(lang, ja: "  後始末の問題: {i:?}", en: "  Cleanup problem: {i:?}")
        );
    }
}

/// `--ini` / `--script` を子の環境の変数にする。絶対パスに直し、ファイルが在ることを起動の前に見る
/// （綴りの誤りで DLL が ini 無しで黙って走らないように）。
fn child_env(
    ini: Option<&Path>,
    script: Option<&Path>,
    lang: Lang,
) -> Result<Vec<(OsString, OsString)>, String> {
    let mut env = Vec::new();
    for (name, flag, p) in [
        (higanr_core::ini::PATH_ENV, "--ini", ini),
        (higanr_core::script::PATH_ENV, "--script", script),
    ] {
        let Some(p) = p else { continue };
        let abs = std::path::absolute(p).map_err(|e| format!("{flag} {}: {e}", p.display()))?;
        let shown = abs.display();
        match std::fs::metadata(&abs) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Err(by_lang!(
                    lang,
                    ja: "{flag} {shown} がファイルでない",
                    en: "{flag} {shown} is not a file"
                ));
            }
            Err(e) => {
                return Err(by_lang!(
                    lang,
                    ja: "{flag} {shown} が読めない（{e}）",
                    en: "{flag} {shown} cannot be read ({e})"
                ));
            }
        }
        env.push((OsString::from(name), abs.into_os_string()));
    }
    Ok(env)
}

/// 接続コードのウィンドウを開くか。ini は dll と同じ場所（`--ini` → 継いだ変数 → DLL のフォルダ）から読む。
fn relay_plan(
    env: &[(OsString, OsString)],
    inherited: Option<&OsStr>,
    dll: &Path,
    game: &Path,
    lang: Lang,
) -> Option<code_relay::Plan> {
    let (dll_dir, game_dir) = (dll.parent()?, game.parent()?);
    let text = relay_ini_text(env, inherited, dll_dir);
    match code_relay::plan(text.as_deref(), dll_dir, game_dir, lang) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}",
                by_lang!(lang, ja: "HiganRTool: {e}。", en: "HiganRTool: {e}.")
            );
            None
        }
    }
}

fn relay_game_menu(env: &[(OsString, OsString)], inherited: Option<&OsStr>, dll: &Path) -> bool {
    dll.parent()
        .and_then(|d| relay_ini_text(env, inherited, d))
        .is_some_and(|t| code_relay::is_game_menu(&t))
}

fn relay_ini_text(
    env: &[(OsString, OsString)],
    inherited: Option<&OsStr>,
    dll_dir: &Path,
) -> Option<String> {
    let name = OsStr::new(higanr_core::ini::PATH_ENV);
    let flag = env
        .iter()
        .find(|(k, _)| k.as_os_str() == name)
        .map(|(_, v)| Path::new(v));
    code_relay::ini_location(flag, inherited, dll_dir).and_then(|p| std::fs::read_to_string(p).ok())
}

/// ウィンドウに出す ini の対戦の形。読めない・値が誤りなら None（dll はネットワークの準備をしないのでウィンドウは形を出さない）。
fn relay_shape(
    env: &[(OsString, OsString)],
    inherited: Option<&OsStr>,
    dll: &Path,
) -> Option<higanr_core::mode_pick::Shape> {
    let text = relay_ini_text(env, inherited, dll.parent()?)?;
    higanr_core::mode_pick::ini_shape_of_text(&text, "netplay")?.ok()
}

fn prepare_relay(relay: Option<&code_relay::Plan>, lang: Lang) -> Option<code_relay::Before> {
    let (before, note) = code_relay::before_launch(relay?, lang);
    if let Some(n) = note {
        println!("{n}");
    }
    Some(before)
}

struct RealRelayHost<'a> {
    w: &'a mut real::RealWin32,
    process: win::Handle,
    lang: Lang,
}

impl code_relay::Host for RealRelayHost<'_> {
    fn game(&mut self) -> code_relay::Game {
        match self.w.wait(self.process, 0) {
            win::WaitResult::TimedOut => code_relay::Game::Running,
            win::WaitResult::Signaled => code_relay::Game::Ended,
            win::WaitResult::Failed(e) => {
                code_relay::Game::Unknown(format!("WaitForSingleObject、GetLastError {e}"))
            }
        }
    }

    fn copy(&mut self, text: &str) -> Result<(), String> {
        real::set_clipboard_text(text, self.lang)
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

/// 標準入力を 1 行ずつ読んで送るスレッド（読みで止まってもウィンドウの手順は止まらない）。
/// `first` は標準入力より先にウィンドウへ届ける 1 行（`--choose`）。
fn spawn_paste_reader(first: Option<String>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if let Some(l) = first {
        let _ = tx.send(l);
    }
    let _ = std::thread::Builder::new()
        .name("higanr-paste".into())
        .spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
    rx
}

struct RealPickHost {
    saved_file: PathBuf,
    lang: Lang,
}

impl game_pick::Host for RealPickHost {
    fn lang(&self) -> Lang {
        self.lang
    }

    fn read_saved(&mut self) -> Option<String> {
        std::fs::read_to_string(&self.saved_file).ok()
    }

    fn write_saved(&mut self, text: &str) -> Result<(), String> {
        std::fs::write(&self.saved_file, text).map_err(|e| {
            let shown = self.saved_file.display();
            by_lang!(self.lang, ja: "{shown} に書けません: {e}", en: "Cannot write to {shown}: {e}")
        })
    }

    fn read_exe(&mut self, exe: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(exe)
    }

    fn is_dir(&mut self, dir: &Path) -> bool {
        dir.is_dir()
    }

    fn standard_dir(&mut self) -> Option<PathBuf> {
        // 32bit の Program Files（`ProgramFiles(x86)`。32bit の Windows には無いので `ProgramFiles` を後ろ盾にする）。
        ["ProgramFiles(x86)", "ProgramFiles"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|pf| PathBuf::from(pf).join(game_pick::STANDARD_SUBDIR))
            .find(|d| d.is_dir())
    }

    fn ask_folder(&mut self, initial: Option<&Path>) -> game_pick::Answer {
        folder_dialog::choose_folder(game_pick::DIALOG_TITLE.get(self.lang), initial)
    }

    fn say(&mut self, line: &str) {
        println!("HiganRTool: {line}");
    }

    fn warn(&mut self, text: &str) {
        eprintln!("HiganRTool: {text}");
        folder_dialog::message_box(text);
    }
}

fn pick_game(choose: bool, lang: Lang) -> Result<PathBuf, i32> {
    let saved_file = std::env::current_exe()
        .map_err(|e| e.to_string())
        .and_then(|me| {
            me.parent()
                .map(|d| d.join(game_pick::SAVED_FILE))
                .ok_or_else(|| folder_unknown(&me, lang))
        })
        .map_err(|e| {
            eprintln!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: この exe の場所が分かりません（{e}）。何も起動していません。",
                    en: "HiganRTool: Cannot tell where this exe is ({e}). Nothing was started."
                )
            );
            1
        })?;
    let mut host = RealPickHost { saved_file, lang };
    game_pick::resolve(&mut host, choose).map_err(|e| {
        eprintln!("HiganRTool: {}", e.describe(lang));
        1
    })
}

// 引数は cli の `Command::Launch` の欄をそのまま受ける。まとめる型を足すと増えるだけなので 8 個のまま。
#[allow(clippy::too_many_arguments)]
fn run_launch(
    game: cli::GameSource,
    dll: Option<PathBuf>,
    ini: Option<PathBuf>,
    script: Option<PathBuf>,
    second: bool,
    no_clipboard: bool,
    choose: Option<higanr_core::mode_pick::Pick>,
    background: bool,
    lang: Lang,
) -> i32 {
    let no_activate = cli::launches_in_background(background, script.as_ref());
    let env = match child_env(ini.as_deref(), script.as_deref(), lang) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: {e}。何も起動していません。",
                    en: "HiganRTool: {e}. Nothing was started."
                )
            );
            return 1;
        }
    };
    let cannot_use_path = |e: &str| {
        eprintln!(
            "{}",
            by_lang!(lang, ja: "HiganRTool: パスが使えません: {e}", en: "HiganRTool: Cannot use the path: {e}")
        )
    };
    let resolve = |p: &Path| std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()));
    let dll = match dll {
        Some(d) => resolve(&d),
        None => std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|me| default_dll_path_in(&me, lang)),
    };
    let dll = match dll {
        Ok(d) => d,
        Err(e) => {
            cannot_use_path(&e);
            return 1;
        }
    };
    // 開発の形（build\bin から走っている）で成果物がソースより古ければ、ウィンドウを出す前に断る（`freshness`）。
    if let Ok(me) = std::env::current_exe()
        && let freshness::Verdict::Stale(list) = freshness::check(&me, &dll)
    {
        for s in &list {
            eprintln!("HiganRTool: {}", freshness::describe(s));
        }
        // 開発のビルド専用の検査なので、日本語のまま。
        eprintln!(
            "HiganRTool: build\\bin の成果物がソースより古いので、何も起動していません。build.bat を先に回してください。"
        );
        return 1;
    }
    let game = match game {
        cli::GameSource::Path(p) => {
            // `D:原作\th09.exe` のように `D:` の後に `\` が無い値は、起こした場所で指す先が変わるので断る。
            if game_pick::is_drive_relative(&p) {
                cannot_use_path(&game_pick::drive_relative_message(&p, lang));
                return 1;
            }
            p
        }
        cli::GameSource::Remembered { choose } => match pick_game(choose, lang) {
            Ok(p) => p,
            Err(code) => return code,
        },
    };
    let game = match resolve(&game) {
        Ok(g) => g,
        Err(e) => {
            cannot_use_path(&e);
            return 1;
        }
    };

    // 接続コードのウィンドウ（ini に `code_side` があるときだけ）。前の回の残りを新しく現れたものと取り違えないよう、置き場の中身を起動前に書き留める。
    let inherited = std::env::var_os(higanr_core::ini::PATH_ENV);
    let relay = relay_plan(&env, inherited.as_deref(), &dll, &game, lang);
    let shape = relay_shape(&env, inherited.as_deref(), &dll);
    // ウィンドウで選べるのは、ウィンドウが開き、ini の対戦の形が読める回だけ。選べない回の `--choose` はウィンドウに届けない（選択の文字が相手のコードとして置かれる）。
    let choose = match (choose, relay.is_some(), shape.is_some()) {
        (Some(_), false, _) => {
            eprintln!(
                "{}",
                match lang {
                    Lang::Ja =>
                        "HiganRTool: --choose は接続コードのウィンドウを開く回（ini の code_side が offer か answer）にだけ効きます。このまま起動します。",
                    Lang::En =>
                        "HiganRTool: --choose only works when the connection code window opens (code_side in the ini is offer or answer). Starting anyway.",
                }
            );
            None
        }
        (Some(_), true, false) => {
            eprintln!(
                "{}",
                match lang {
                    Lang::Ja =>
                        "HiganRTool: ini の同期方式とディレイ（mode と delay）が読めないので、--choose は使いません。このまま起動します。",
                    Lang::En =>
                        "HiganRTool: Cannot read the netcode in the ini (mode and delay), so --choose is not used. Starting anyway.",
                }
            );
            None
        }
        (c, _, _) => c,
    };
    // ウィンドウを開く回だけ、前の回の相手のコードも片づける（古い相手へ繋ぎにいくのを防ぐ）。
    let before = prepare_relay(relay.as_ref(), lang);
    let logs_dir = dll.parent().map(|d| d.join(code_relay::LOGS_DIR));

    let req = LaunchRequest {
        game,
        dll,
        args: Vec::<OsString>::new(),
        env: env.clone(),
        ident: higanr_core::pe::TH09_V150A,
        keep_process_handle: relay.is_some(),
        // 毎回、起動中の別の花映塚と独立させる（`ISOLATED_INSTANCE`）。`--second` は名残。
        patches: if second {
            vec![launch::ISOLATED_INSTANCE, launch::SECOND_INSTANCE]
        } else {
            vec![launch::ISOLATED_INSTANCE]
        },
        no_activate,
    };
    let mut w = real::RealWin32;
    match launch::launch(&mut w, &req) {
        Ok(ok) => {
            // 新しいバージョンが花映塚を起こせた印（`.old` を消してよい合図）。ゲームが長く続いても起動の時点で残す。
            update::note_launched(lang);
            let (pid, module) = (ok.pid, ok.module);
            println!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: 起動しました（pid {pid}、DLL をゲームの中の 0x{module:08X} に読み込みました）。",
                    en: "HiganRTool: Started (pid {pid}, loaded the DLL at 0x{module:08X} inside the game)."
                )
            );
            for (k, v) in &env {
                let (k, v) = (k.to_string_lossy(), v.to_string_lossy());
                println!(
                    "{}",
                    by_lang!(
                        lang,
                        ja: "HiganRTool: ゲームの環境変数に {k}={v} を設定しました。",
                        en: "HiganRTool: Set {k}={v} in the game's environment."
                    )
                );
            }
            let iso = launch::ISOLATED_INSTANCE;
            let (at, expect, new) = (iso.at, iso.expect, iso.new);
            println!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: ふだんの花映塚と独立させた（プロセスのメモリの 0x{at:08X} を {expect:02X?} → {new:02X?}。名前つきの印を作らない。ゲームのファイルには書いていない）。",
                    en: "HiganRTool: Made it independent from your usual Touhou 9 (process memory 0x{at:08X}: {expect:02X?} -> {new:02X?}. No named marker is created. Game files are not written)."
                )
            );
            if no_activate {
                println!(
                    "{}",
                    match lang {
                        Lang::Ja =>
                            "HiganRTool: 起こしたウィンドウを前面にしない（SW_SHOWNOACTIVATE）。",
                        Lang::En =>
                            "HiganRTool: The game window is not brought to the front (SW_SHOWNOACTIVATE).",
                    }
                );
            }
            if second {
                let sec = launch::SECOND_INSTANCE;
                let (at, expect, new) = (sec.at, sec.expect, sec.new);
                println!(
                    "{}",
                    by_lang!(
                        lang,
                        ja: "HiganRTool: 2 つ目の起動として、プロセスのメモリの 0x{at:08X} を {expect:02X?} → {new:02X?} に書き換えた（読み直して一致。ゲームのファイルには書いていない）。",
                        en: "HiganRTool: Second launch: changed process memory 0x{at:08X} from {expect:02X?} to {new:02X?} (read back and matched. Game files are not written)."
                    )
                );
            }
            print_issues(&ok.cleanup_issues, lang);
            // ゲームの中のメニューで接続する実行（`code_side = game`）はウィンドウでコードを受け渡さない。接続元を 1 行出して終わる。
            if relay_game_menu(&env, inherited.as_deref(), &req.dll) {
                println!("HiganRTool: {}", code_relay::GAME_MENU_HINT.get(lang));
            }
            if let (Some(plan), Some(process)) = (relay, ok.process) {
                let mut r = code_relay::Relay::new(plan, before.unwrap_or_default(), lang);
                if no_clipboard {
                    r = r.without_clipboard();
                }
                if let Some(shape) = shape {
                    r = r.with_mode(shape, ok.pid);
                }
                println!("{}", r.intro());
                let pasted = spawn_paste_reader(choose.map(|c| c.token()));
                let mut host = RealRelayHost {
                    w: &mut w,
                    process,
                    lang,
                };
                if code_relay::run(&mut r, &mut host, &pasted, code_relay::POLL)
                    == code_relay::End::Finished
                {
                    code_relay::linger(&mut host, &pasted, code_relay::POLL, lang);
                }
                // ゲームが終わった。ウィンドウが閉じる前に送ってほしいファイルを出す。端末なら Enter を待つ（待たないと読めないまま消える）。
                if let Some(logs) = &logs_dir {
                    code_relay::farewell(
                        &mut host,
                        &pasted,
                        logs,
                        ok.pid,
                        std::io::IsTerminal::is_terminal(&std::io::stdin()),
                        lang,
                    );
                }
                let _ = w.close_handle(process);
            }
            0
        }
        Err(f) => {
            let (what, row, after) = (
                describe(&f.error, lang),
                f.error.row(),
                aftermath(&f.error, lang),
            );
            eprintln!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: {what}（表の行 {row}）。{after}",
                    en: "HiganRTool: {what} (table row {row}). {after}"
                )
            );
            // 一致しなかったときは、読んだファイルと値・期待の値を続けて出す（そのまま送ってもらえるように）。
            if matches!(f.error, LaunchError::GameIdentMismatch(_)) {
                match std::fs::read(&req.game) {
                    Ok(bytes) => {
                        for l in game_pick::ident_details(&req.game, &bytes, lang) {
                            eprintln!("  {l}");
                        }
                    }
                    Err(e) => {
                        let shown = req.game.display();
                        eprintln!(
                            "{}",
                            by_lang!(
                                lang,
                                ja: "  読んだファイル: {shown}（読み直せません: {e}）",
                                en: "  File read: {shown} (cannot be read again: {e})"
                            )
                        )
                    }
                }
            }
            print_issues(&f.cleanup_issues, lang);
            1
        }
    }
}

/// 引数の誤りの理由に使い方を続けた文。
fn bad_args_text(reason: &str, lang: Lang) -> String {
    format!("HiganRTool: {reason}\n{}", cli::USAGE.get(lang))
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut lang = initial_lang();
    let code = match cli::parse(&args, lang) {
        Ok(cli::Command::Launch {
            game,
            dll,
            ini,
            script,
            second,
            no_clipboard,
            choose,
            background,
        }) => {
            lang = refine_lang(
                lang,
                ini.as_deref(),
                dll.as_deref(),
                std::env::current_exe().ok().as_deref(),
            );
            // 花映塚を起こす前の更新の確認（配るバージョンだけ。開発用の引数があれば確かめない）。更新したらこの exe は何もせず終わる。
            let dev_args = ini.is_some()
                || script.is_some()
                || second
                || no_clipboard
                || choose.is_some()
                || background;
            if update::startup(dev_args, lang) == update::Outcome::Restarted {
                std::process::exit(0);
            }
            run_launch(
                game,
                dll,
                ini,
                script,
                second,
                no_clipboard,
                choose,
                background,
                lang,
            )
        }
        Ok(cli::Command::SelfTest { dll }) => selftest::run(dll),
        Ok(cli::Command::SelfTestTarget { dll }) => selftest::target(&dll),
        Ok(cli::Command::Help) => {
            println!("{}", cli::USAGE.get(lang));
            0
        }
        Err(e) => {
            eprintln!("{}", bad_args_text(&e, lang));
            2
        }
    };
    // ショートカットから開いたウィンドウなら、断った理由を読めるよう Enter まで残す。
    if hold::should_hold(code, real::console_process_count()) {
        eprintln!("{}", hold::HOLD_TEXT.get(lang));
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_env_names_absolute_paths_of_existing_files() {
        let dir = std::env::temp_dir().join(format!(
            "HiganRTool_loader_child_env_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("two_host.ini");
        let script = dir.join("two_01_host.txt");
        std::fs::write(&ini, "[netplay]\n").unwrap();
        std::fs::write(&script, "scene=1\n").unwrap();

        assert_eq!(child_env(None, None, Lang::Ja), Ok(Vec::new()));
        let env = child_env(Some(&ini), Some(&script), Lang::Ja).unwrap();
        assert_eq!(
            env,
            vec![
                (
                    OsString::from("HIGANRTOOL_INI"),
                    ini.clone().into_os_string()
                ),
                (
                    OsString::from("HIGANRTOOL_SCRIPT"),
                    script.clone().into_os_string()
                ),
            ]
        );
        let only_script = child_env(None, Some(&script), Lang::Ja).unwrap();
        assert_eq!(only_script.len(), 1);
        assert_eq!(only_script[0].0, OsString::from("HIGANRTOOL_SCRIPT"));
        let rel = child_env(Some(Path::new("Cargo.toml")), None, Lang::Ja).unwrap();
        assert!(Path::new(&rel[0].1).is_absolute(), "{rel:?}");
        assert!(child_env(Some(&dir.join("missing.ini")), None, Lang::Ja).is_err());
        assert!(child_env(None, Some(&dir), Lang::Ja).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relay_opens_only_for_an_ini_with_code_side() {
        let root = std::env::temp_dir().join(format!(
            "HiganRTool_loader_relay_plan_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (bin, game_dir) = (root.join("bin"), root.join("game"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&game_dir).unwrap();
        let (dll, game) = (bin.join("HiganRTool.dll"), game_dir.join("th09.exe"));
        let offer = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n";
        let plain = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7\nsession = 1\n";

        // ini が無い・今までの ini: 開かない。置き場の前の回の peer_code.txt にも触らない。
        let code = bin.join("code");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("peer_code.txt"), "OLDPEER\r\n").unwrap();
        assert_eq!(relay_plan(&[], None, &dll, &game, Lang::Ja), None);
        std::fs::write(bin.join("HiganRTool.ini"), plain).unwrap();
        let none = relay_plan(&[], None, &dll, &game, Lang::Ja);
        assert_eq!(none, None);
        assert_eq!(prepare_relay(none.as_ref(), Lang::Ja), None);
        assert!(
            code.join("peer_code.txt").exists(),
            "ウィンドウを開かない回に消した"
        );
        std::fs::write(bin.join("HiganRTool.ini"), offer).unwrap();
        let p = relay_plan(&[], None, &dll, &game, Lang::Ja).unwrap();
        assert_eq!(p.dir, code);
        assert!(prepare_relay(Some(&p), Lang::Ja).is_some());
        assert!(
            !code.join("peer_code.txt").exists(),
            "ウィンドウを開く回に消えていない"
        );
        let other = root.join("two_host.ini");
        std::fs::write(&other, plain).unwrap();
        let env = child_env(Some(&other), None, Lang::Ja).unwrap();
        assert_eq!(relay_plan(&env, None, &dll, &game, Lang::Ja), None);
        assert_eq!(
            relay_plan(&[], Some(other.as_os_str()), &dll, &game, Lang::Ja),
            None
        );
        std::fs::write(&other, offer.replace("offer", "answer")).unwrap();
        let p = relay_plan(&env, None, &dll, &game, Lang::Ja).unwrap();
        assert_eq!(p.side, code_relay::Side::Answer);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relay_shape_follows_the_ini_the_dll_reads() {
        let root = std::env::temp_dir().join(format!(
            "HiganRTool_loader_relay_shape_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let dll = bin.join("HiganRTool.dll");
        let write = |p: &Path, extra: &str| {
            std::fs::write(
                p,
                format!("[netplay]\nenable = 1\nbind = 127.0.0.1:0\ncode_side = offer\n{extra}"),
            )
            .unwrap()
        };
        use higanr_core::wire::Mode;
        let shape =
            |env: &[(OsString, OsString)]| relay_shape(env, None, &dll).map(|s| (s.mode, s.delay));
        assert_eq!(shape(&[]), None, "ini が無いのに形を出した");
        write(&bin.join("HiganRTool.ini"), "");
        assert_eq!(shape(&[]), Some((Mode::Rollback, 1)));
        write(&bin.join("HiganRTool.ini"), "mode = delay\n");
        assert_eq!(shape(&[]), Some((Mode::Delay, 1)));
        let other = root.join("other.ini");
        write(&other, "mode = rollback\ndelay = 2\n");
        let env = child_env(Some(&other), None, Lang::Ja).unwrap();
        assert_eq!(shape(&env), Some((Mode::Rollback, 2)));
        write(&other, "mode = Rollback\n");
        assert_eq!(shape(&env), None);
        std::fs::write(&other, "[netplay\n").unwrap();
        assert_eq!(shape(&env), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_choose_line_reaches_the_window_first() {
        let rx = spawn_paste_reader(Some("2".to_string()));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok("2".to_string())
        );
    }

    #[test]
    fn ident_mismatch_tells_what_to_check() {
        use higanr_core::pe::IdentField;
        let t = describe(
            &LaunchError::GameIdentMismatch(vec![
                IdentField::TimeDateStamp,
                IdentField::EntryBytes,
            ]),
            Lang::Ja,
        );
        assert!(
            t.contains("custom.exe") && t.contains("vpatch.exe") && t.contains("修正パッチ"),
            "{t}"
        );
        assert!(
            t.contains("TimeDateStamp") && t.contains("EntryBytes"),
            "{t}"
        );
    }

    #[test]
    fn elevation_required_tells_how_to_start() {
        let t = describe(&LaunchError::CreateProcess(740), Lang::Ja);
        assert!(
            t.contains("GetLastError 740") && t.contains("管理者として実行"),
            "{t}"
        );
        let other = describe(&LaunchError::CreateProcess(2), Lang::Ja);
        assert!(!other.contains("管理者"), "{other}");
    }

    fn every_launch_error() -> Vec<LaunchError> {
        use higanr_core::pe::{IdentField, PeError};
        vec![
            LaunchError::DllPathInvalid(launch::DLL_NOT_ABSOLUTE),
            LaunchError::DllUnreadable("os".into()),
            LaunchError::DllNotPe32I386(PeError::TooShortForDosHeader),
            LaunchError::GamePathInvalid,
            LaunchError::GameUnreadable("os".into()),
            LaunchError::GameIdentMismatch(vec![IdentField::TimeDateStamp]),
            LaunchError::LoadLibraryAddress(5),
            LaunchError::CreateProcess(ELEVATION_REQUIRED),
            LaunchError::CreateProcess(2),
            LaunchError::VirtualAlloc(5),
            LaunchError::WriteMemory(5),
            LaunchError::CreateRemoteThread(5),
            LaunchError::RemoteThreadTimedOut,
            LaunchError::RemoteThreadWaitFailed(5),
            LaunchError::LoadLibraryReturnedZero,
            LaunchError::ExitCodeUnavailable(5),
            LaunchError::ResumeFailed(5),
            LaunchError::PatchRead(0x42D909, 5),
            LaunchError::PatchMismatch {
                at: 0x42D909,
                found: vec![1, 2],
            },
            LaunchError::PatchWrite(0x42D909, 5),
            LaunchError::PatchVerify {
                at: 0x42D909,
                found: None,
            },
        ]
    }

    #[test]
    fn english_run_describes_every_launch_failure_in_english() {
        use higanr_core::lang::has_japanese;
        for e in every_launch_error() {
            let ja = describe(&e, Lang::Ja);
            let en = describe(&e, Lang::En);
            assert!(!en.is_empty() && !has_japanese(&en), "{e:?}: {en}");
            assert!(has_japanese(&ja), "{e:?}: {ja}");
            assert_ne!(ja, en);
            assert!(!aftermath(&e, Lang::En).is_empty());
            assert!(!has_japanese(aftermath(&e, Lang::En)));
        }
        let t = describe(&LaunchError::CreateProcess(ELEVATION_REQUIRED), Lang::En);
        assert!(
            t.contains("run as administrator") && t.contains("740"),
            "{t}"
        );
    }

    #[test]
    fn a_bad_argument_is_followed_by_the_usage_in_the_same_language() {
        let en = bad_args_text("Unknown argument: --x", Lang::En);
        assert!(
            en.starts_with("HiganRTool: Unknown argument: --x\nUsage:"),
            "{en}"
        );
        let ja = bad_args_text("知らない引数です: --x", Lang::Ja);
        assert!(ja.contains("\n使い方:"), "{ja}");
    }

    #[test]
    fn refine_lang_follows_the_ini_and_dll_the_arguments_name() {
        let root = std::env::temp_dir().join(format!(
            "HiganRTool_loader_refine_lang_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ini = root.join("other.ini");
        std::fs::write(&ini, "[ui]\nlang = en\n").unwrap();
        // 引数が何も場所を示さなければ、最初に決めた言語のまま。
        assert_eq!(refine_lang(Lang::Ja, None, None, None), Lang::Ja);
        // --ini の [ui] が OS の言語（検査の機械の値）に勝つ。
        let dll = bin.join("HiganRTool.dll");
        assert_eq!(
            refine_lang(Lang::Ja, Some(&ini), Some(&dll), None),
            Lang::En
        );
        // --ini を替えれば、その [ui] に従う。
        let ja_ini = root.join("ja.ini");
        std::fs::write(&ja_ini, "[ui]\nlang = ja\n").unwrap();
        assert_eq!(
            refine_lang(Lang::En, Some(&ja_ini), Some(&dll), None),
            Lang::Ja
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
