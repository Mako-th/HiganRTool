//! HiganRTool.exe —— 起動用の exe。th09.exe を止めた状態で起動し、HiganRTool.dll を読み込ませてから動かす。
//!
//! 手順と後始末は `launch.rs`、本物の Win32 は `real.rs`、引数は `cli.rs`。`game_pick.rs` は `--game` なしの起動で th09.exe の場所を
//! 選ばせ、`update.rs` は配るバージョンの起動前の更新、`code_relay.rs` は接続コードの受け渡しのウィンドウ、`freshness.rs` は古い成果物での起動を断る。
//! OS のキーボード・マウスの注入や前面を奪う呼び出しはしない。ゲームのフォルダに書くのは `HiganRTool.ini` だけ（ツールがその中に置かれたとき）。

// unsafe を書いてよいのは real.rs・folder_dialog.rs・http_win.rs だけ。
#[forbid(unsafe_code)]
mod cli;
#[forbid(unsafe_code)]
mod code_relay;
#[forbid(unsafe_code)]
mod crash_log;
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
mod imports;
#[forbid(unsafe_code)]
mod launch;
#[cfg(any(test, not(feature = "dist")))]
#[forbid(unsafe_code)]
mod ledger;
#[forbid(unsafe_code)]
mod post_update;
mod real;
#[cfg(not(feature = "dist"))]
#[forbid(unsafe_code)]
mod selftest;
#[forbid(unsafe_code)]
mod single;
#[forbid(unsafe_code)]
mod staged;
#[cfg(any(test, not(feature = "dist")))]
#[forbid(unsafe_code)]
mod synth;
#[forbid(unsafe_code)]
mod ui_lang;
#[forbid(unsafe_code)]
mod update;
#[forbid(unsafe_code)]
mod win;
#[forbid(unsafe_code)]
mod writable;
// build.rs と同じ定義を検査するためだけに取り込む。
#[cfg(test)]
#[path = "../manifest_link.rs"]
mod manifest_link;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use higanr_core::ini_edit::Edit;
use higanr_core::ini_store::{self, Legacy};
use higanr_core::lang::Lang;
use higanr_core::lang::Tx;
use launch::{CleanupIssue, LaunchError, LaunchRequest};
use staged::Logged;
use ui_lang::by_lang;
use win::{LaunchNote, Win32};

/// この exe と同じフォルダの `HiganRTool.dll`（`target\...\release` では cdylib の `higanrtool.dll`。大文字小文字は区別しない）。
/// 自己検査は日本語のまま使う。
#[cfg(not(feature = "dist"))]
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
        ja: "{shown}のフォルダが分からない",
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
            by_lang!(lang, ja: "DLLのパスが使えません（{why}）", en: "The DLL path cannot be used ({why})")
        }
        LaunchError::DllUnreadable(why) => {
            by_lang!(lang, ja: "DLLが読めません（{why}）", en: "The DLL cannot be read ({why})")
        }
        LaunchError::DllNotPe32I386(why) => by_lang!(
            lang,
            ja: "DLLが32bitのDLLではありません（{why:?}）",
            en: "The DLL is not a 32-bit DLL ({why:?})"
        ),
        LaunchError::GamePathInvalid => match lang {
            Lang::Ja => "exeのフォルダが分かりません",
            Lang::En => "Cannot tell the folder of the exe",
        }
        .to_string(),
        LaunchError::GameUnreadable(why) => {
            by_lang!(lang, ja: "exeが読めません（{why}）", en: "The exe cannot be read ({why})")
        }
        LaunchError::GameIdentMismatch(fields) => game_pick::ident_mismatch_text(fields, lang),
        LaunchError::LoadLibraryAddress(e) => by_lang!(
            lang,
            ja: "LoadLibraryWのアドレスが引けません（GetLastError {e}）",
            en: "Cannot find the address of LoadLibraryW (GetLastError {e})"
        ),
        // 740 = ERROR_ELEVATION_REQUIRED。th09.exe が「管理者として実行」の設定のとき。
        LaunchError::CreateProcess(ELEVATION_REQUIRED) => by_lang!(
            lang,
            ja: "起動できません（CreateProcessW、GetLastError {ELEVATION_REQUIRED}）。th09.exeが「管理者として実行」の設定になっています。HiganRTool.exeも管理者として起動してください（ショートカットのプロパティ →「詳細設定」→「管理者として実行」）",
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
            ja: "DLLのパスを書き込めません（GetLastError {e}）",
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
                ja: "DLLの読み込みが{secs}秒で終わりませんでした",
                en: "Loading the DLL did not finish within {secs} seconds"
            )
        }
        LaunchError::RemoteThreadWaitFailed(e) => by_lang!(
            lang,
            ja: "DLLの読み込みを待てませんでした（GetLastError {e}）",
            en: "Cannot wait for the DLL to load (GetLastError {e})"
        ),
        LaunchError::LoadLibraryReturnedZero => match lang {
            Lang::Ja => "ゲームの中でDLLが読み込めませんでした（LoadLibraryWが0。DLLが自分から断った場合を含む）",
            Lang::En => "The DLL could not be loaded inside the game (LoadLibraryW returned 0; this includes the DLL refusing to load by itself)",
        }
        .to_string(),
        LaunchError::ExitCodeUnavailable(e) => by_lang!(
            lang,
            ja: "DLLの読み込みの結果が読めません（GetLastError {e}）",
            en: "Cannot read the result of loading the DLL (GetLastError {e})"
        ),
        LaunchError::ResumeFailed(e) => by_lang!(
            lang,
            ja: "ゲームを動かせません（ResumeThread、GetLastError {e}）",
            en: "Cannot let the game run (ResumeThread, GetLastError {e})"
        ),
        LaunchError::PatchRead(at, e) => by_lang!(
            lang,
            ja: "2つ目の起動の書き換え: 0x{at:08X}が読めません（GetLastError {e}）。何も書いていません",
            en: "Start-up patch: cannot read 0x{at:08X} (GetLastError {e}). Nothing was written"
        ),
        LaunchError::PatchMismatch { at, found } => by_lang!(
            lang,
            ja: "2つ目の起動の書き換え: 0x{at:08X}が期待のバイト列と違います（{found:02X?}）。何も書いていません",
            en: "Start-up patch: 0x{at:08X} does not hold the expected bytes ({found:02X?}). Nothing was written"
        ),
        LaunchError::PatchWrite(at, e) => by_lang!(
            lang,
            ja: "2つ目の起動の書き換え: 0x{at:08X}に書けません（GetLastError {e}）",
            en: "Start-up patch: cannot write to 0x{at:08X} (GetLastError {e})"
        ),
        LaunchError::PatchVerify { at, found } => by_lang!(
            lang,
            ja: "2つ目の起動の書き換え: 0x{at:08X}を読み直すと書いた値になっていません（{found:02X?}）",
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
                    ja: "{flag} {shown}がファイルでない",
                    en: "{flag} {shown} is not a file"
                ));
            }
            Err(e) => {
                return Err(by_lang!(
                    lang,
                    ja: "{flag} {shown}が読めない（{e}）",
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
    ini_path_of(env, inherited, dll_dir).and_then(|p| std::fs::read_to_string(p).ok())
}

/// dll が読む ini のパス（`--ini` → 継いだ変数 → DLL のフォルダ）。
fn ini_path_of(
    env: &[(OsString, OsString)],
    inherited: Option<&OsStr>,
    dll_dir: &Path,
) -> Option<PathBuf> {
    let name = OsStr::new(higanr_core::ini::PATH_ENV);
    let flag = env
        .iter()
        .find(|(k, _)| k.as_os_str() == name)
        .map(|(_, v)| Path::new(v));
    code_relay::ini_location(flag, inherited, dll_dir)
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
    /// 覚える場所を書く ini（dll が読む ini と同じ）。
    ini: PathBuf,
    lang: Lang,
}

impl game_pick::Host for RealPickHost {
    fn lang(&self) -> Lang {
        self.lang
    }

    fn read_saved(&mut self) -> Option<String> {
        ini_store::get(&self.ini, game_pick::INI_SECTION, game_pick::INI_KEY)
    }

    fn write_saved(&mut self, text: &str) -> Result<(), String> {
        ini_store::update_creating(
            &self.ini,
            &new_ini_text(),
            &[Edit::Set {
                section: game_pick::INI_SECTION,
                key: game_pick::INI_KEY,
                value: text,
            }],
        )
        .map(|_| ())
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

/// ini が無いときに作る中身。配るバージョンは配る ini と同じ（一部の設定だけの ini を作ると、dll が既定を書き出さなくなる）。
fn new_ini_text() -> String {
    if cfg!(feature = "dist") {
        ini_store::dist_ini_text()
    } else {
        String::new()
    }
}

/// 以前の版の game_path.txt（この exe と同じフォルダ）があれば ini の `[loader] game` へ移して消す（ini に値があれば ini が先）。
/// 選ぶ前と、覚えを先に読む前（`early_game_dir`）の両方で呼ぶので、2 回目は何もしない。
fn migrate_legacy_game(ini: &Path) {
    let Some(legacy_file) = std::env::current_exe()
        .ok()
        .and_then(|me| me.parent().map(|d| d.join(game_pick::LEGACY_FILE)))
    else {
        return;
    };
    for line in ini_store::migrate(
        ini,
        Some(&new_ini_text()),
        &[Legacy {
            file: legacy_file,
            section: game_pick::INI_SECTION,
            key: game_pick::INI_KEY,
            parse: game_pick::parse_legacy,
            is_default: |_| false,
        }],
    ) {
        eprintln!("HiganRTool: {line}");
    }
}

/// dll が読む ini のパス（`--ini` → 継いだ変数 → DLL のフォルダ。DLL を指さなければこの exe のフォルダ）。
fn loader_ini_path(ini: Option<&Path>, dll: Option<&Path>) -> Option<PathBuf> {
    let flag = ini.and_then(|p| std::path::absolute(p).ok());
    let dll_dir = match dll {
        Some(d) => std::path::absolute(d).ok()?.parent()?.to_path_buf(),
        None => std::env::current_exe().ok()?.parent()?.to_path_buf(),
    };
    code_relay::ini_location(
        flag.as_deref(),
        std::env::var_os(higanr_core::ini::PATH_ENV).as_deref(),
        &dll_dir,
    )
}

fn pick_game(choose: bool, ini: &Path, lang: Lang) -> Result<PathBuf, i32> {
    migrate_legacy_game(ini);
    let mut host = RealPickHost {
        ini: ini.to_path_buf(),
        lang,
    };
    game_pick::resolve(&mut host, choose).map_err(|e| {
        eprintln!("HiganRTool: {}", e.describe(lang));
        1
    })
}

const SLOW_LOAD_TEXT: Tx = Tx {
    ja: "HiganRTool: 読み込みに時間がかかっています。ウイルス対策ソフトの確認を待っている可能性があります。このままお待ちください。",
    en: "HiganRTool: Loading is taking a while. Your antivirus software may be checking the files. Please wait.",
};

fn announce_note(lang: Lang, note: LaunchNote) {
    match note {
        LaunchNote::SlowLoad => println!("{}", SLOW_LOAD_TEXT.get(lang)),
    }
}

/// Wine の上で読み込みの待ちが時間切れになったときの次の一手。Wine の前回の起動（死にきらない th09.exe・wineserver の
/// 状態）が残っていると、次の読み込みが止まるという推定による（Linux の実機では測っていない）。
const WINE_LEFTOVER_TEXT: Tx = Tx {
    ja: "Wineの前回の起動が残っている可能性があります。`wineserver -k`を実行してから、もう一度起動してください。",
    en: "A previous Wine session may still be running. Run `wineserver -k`, then start again.",
};

/// 時間切れの案内に足す 1 文。Wine のときだけ出す（Windows の時間切れは今までどおり）。
fn wine_hint(e: &LaunchError, wine: bool, lang: Lang) -> Option<&'static str> {
    (wine && matches!(e, LaunchError::RemoteThreadTimedOut)).then(|| WINE_LEFTOVER_TEXT.get(lang))
}

/// 起動を止める理由の文を、画面に出すとともに段階ログにも残す。
fn fail_line(text: &str) {
    eprintln!("{text}");
    crash_log::stage(&format!("画面: {text}"));
}

/// 段階ログを用意し、未処理例外のフィルターを置き、起動の最初の行を貯める。ログのファイルはまだ開かない（`settle_tool_folder`）。
fn start_log(lang: Lang, args: &[OsString]) {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let logs = dir.join(code_relay::LOGS_DIR);
    let scrub = higanr_core::log_scrub::Scrub::new().dir(&dir.to_string_lossy(), "<ツール>");
    let mut log = crash_log::StageLog::new(&logs, real::local_stamp, scrub);
    real::install_exception_filter(real::EXCEPTION_PASS_ON);
    log.begin_run(std::process::id());
    crash_log::install(log);
    let build = higanr_core::update::BUILD_NUMBER;
    let kind = if cfg!(feature = "dist") {
        "配布版"
    } else {
        "開発版"
    };
    crash_log::stage(&format!(
        "HiganRTool.exe {} ビルド番号 {build}（{kind}）",
        env!("CARGO_PKG_VERSION")
    ));
    // 値を持つ引数（パス）は書かず、スイッチの名前だけを書く。
    let flags: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .filter(|a| a.starts_with("--"))
        .collect();
    crash_log::stage(&format!(
        "Windows {}{} / 言語 {}（OS の UI 言語 0x{:04X}）/ スイッチ: {}",
        real::os_version(),
        if real::is_wine() {
            "（Wine / Proton）"
        } else {
            ""
        },
        match lang {
            Lang::Ja => "ja",
            Lang::En => "en",
        },
        real::user_ui_langid(),
        if flags.is_empty() {
            "なし".to_string()
        } else {
            flags.join(" ")
        }
    ));
}

/// ゲームのフォルダが分かったときに、ログのファイルを開いてよいか（ツールのフォルダがゲームのフォルダの外か）を決める。
/// 外ならログを開いて貯めた行を書き、フォルダへ書き込めるかを確かめる。中（同じ・下の階層・`th09.exe` が同居）なら、
/// ゲームのフォルダへは何も作らず、ログは書かない（画面だけ）。決めるのは 1 回だけ。
fn settle_tool_folder(game_dir: &Path, lang: Lang) {
    if !crash_log::is_waiting() {
        return;
    }
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        crash_log::decide(true);
        return;
    };
    let inside = update::in_game_folder(&dir) || crash_log::is_inside_game(&dir, game_dir);
    if crash_log::decide(inside) == crash_log::Decision::Opened {
        if let Some(file) = crash_log::clone_file() {
            real::attach_exception_file(file);
        }
        // 配るバージョンだけ古いログを消す（DLL のログと同じ扱い）。
        if cfg!(feature = "dist") {
            let (removed, failed) = crash_log::prune(
                &dir.join(code_relay::LOGS_DIR),
                &real::local_stamp(),
                crash_log::KEEP_LOG_DAYS,
            );
            crash_log::stage(&format!(
                "古いログ: {} 日より前のものを {removed} 個消した（消せなかった {failed} 個）",
                crash_log::KEEP_LOG_DAYS
            ));
        }
    }
    check_tool_folder_writable(&dir, inside, lang);
}

/// 配るバージョンの起動で、この exe のフォルダへ書き込めるかを確かめ、書けなければ知らせて起動を続ける。
/// ゲームのフォルダの中では試さない（一時ファイルでもゲームのフォルダに作らない）。
fn check_tool_folder_writable(dir: &Path, in_game: bool, lang: Lang) {
    let mut site = writable::RealSite(writable::FsProbe::real());
    writable::check(update::this_build_is_dist(), in_game, dir, lang, &mut site);
}

/// 花映塚を起こす前（更新の確認の前）に分かるゲームのフォルダ。`--game` で指した場所か、そのまま使える覚え（選び直しにならないもの）。
/// 分からなければ None で、`run_launch` が選んだ後に決める。
fn early_game_dir(
    source: &cli::GameSource,
    ini_path: Option<&Path>,
    lang: Lang,
) -> Option<PathBuf> {
    match source {
        cli::GameSource::Path(p) => {
            if game_pick::is_drive_relative(p) {
                return None;
            }
            std::path::absolute(p).ok()?.parent().map(Path::to_path_buf)
        }
        cli::GameSource::Remembered { choose: true } => None,
        cli::GameSource::Remembered { choose: false } => {
            let ini = ini_path?;
            migrate_legacy_game(ini);
            let mut host = RealPickHost {
                ini: ini.to_path_buf(),
                lang,
            };
            game_pick::remembered_exe(&mut host)?
                .parent()
                .map(Path::to_path_buf)
        }
    }
}

/// 注入する DLL の import が、この Windows で全部引けるか。欠けていれば理由を出して終了コードを返す（ゲームは起こさない）。
/// Wine / Proton の上では欠けて見えても止めない。確かめられなかった場合（DLL が読めない・表が読めない・DLL を読み込めない）も止めずに続ける。
fn check_dll_imports(dll: &Path, lang: Lang) -> Result<(), i32> {
    let Ok(bytes) = std::fs::read(dll) else {
        crash_log::stage("import の確認: DLL が読めないので省く（起動の手順が理由を出す）");
        return Ok(());
    };
    crash_log::stage("import の確認を始める（DLL が使う関数がこの Windows にあるか）");
    let mut probe = real::RealProbe::default();
    let report = match imports::check(&bytes, &mut probe) {
        Ok(r) => r,
        Err(why) => {
            crash_log::stage(&format!("import の確認: 表が読めない（{why}）ので省く"));
            return Ok(());
        }
    };
    crash_log::stage(&format!(
        "import の確認: {} 件を引いた / 無い {} 件 / 確かめられなかった DLL {} 個",
        report.checked,
        report.missing.len(),
        report.unchecked.len()
    ));
    for (dll, code) in &report.unchecked {
        crash_log::stage(&format!(
            "  確かめられなかった: {dll}（GetLastError {code}）"
        ));
    }
    let os = real::os_version();
    for m in &report.missing {
        let what = match &m.function {
            Some(f) => format!("{}!{f}", m.dll),
            None => m.dll.clone(),
        };
        crash_log::stage(&format!("  無い: {what}（Windows {os}）"));
    }
    match imports::judge(&report, &os, real::is_wine(), lang) {
        imports::Verdict::Clear => Ok(()),
        imports::Verdict::Tolerated { missing } => {
            crash_log::stage(&format!(
                "import の確認: Wine / Proton の上なので、無い {missing} 件があっても止めずに続ける"
            ));
            Ok(())
        }
        imports::Verdict::Stop(text) => {
            fail_line(&format!("HiganRTool: {text}"));
            Err(1)
        }
    }
}

fn cannot_use_path(e: &str, lang: Lang) {
    fail_line(
        &by_lang!(lang, ja: "HiganRTool: パスが使えません: {e}", en: "HiganRTool: Cannot use the path: {e}"),
    )
}

/// 開発用の引数があるか（あれば更新は確かめない）。
fn has_dev_args(
    ini: Option<&Path>,
    script: Option<&Path>,
    second: bool,
    no_clipboard: bool,
    choose: Option<higanr_core::mode_pick::Pick>,
    background: bool,
) -> bool {
    ini.is_some() || script.is_some() || second || no_clipboard || choose.is_some() || background
}

/// 花映塚を起こす前の更新の確認（配るバージョンだけ。開発用の引数があれば確かめない）。`game_dir` は決まったゲームのフォルダ。
fn check_for_update(dev_args: bool, game_dir: &Path, lang: Lang) -> update::Outcome {
    crash_log::stage(&format!(
        "更新の確認を始める（開発用の引数: {}。配布版でなければ確かめない）",
        if dev_args { "あり" } else { "なし" }
    ));
    let outcome = update::startup(dev_args, game_dir, lang);
    if !matches!(outcome, update::Outcome::Restarted(_)) {
        crash_log::stage(
            "更新の確認: このまま起動する（更新なし・確認しない・確認できなかったのどれか）",
        );
    }
    outcome
}

enum Decided {
    Go(PathBuf),
    /// 更新して起こし直した新しい版が終わった。値はその終了コード。
    Restarted(i32),
}

/// ゲームのパスを決め（`--game`・覚え・選ぶウィンドウ）、`on_decided` でログの置き場を決めてから、`update` で更新を確かめる。
/// 更新の確認はパスが決まった後（ツールがゲームの下の階層にあるとき、確認がその中へ書かないため）。
/// ゲームのフォルダが分からないパスなら更新は確かめない。
fn decide_game_then_update(
    source: cli::GameSource,
    lang: Lang,
    pick: &mut dyn FnMut(bool) -> Result<PathBuf, i32>,
    on_decided: &mut dyn FnMut(&Path),
    update: &mut dyn FnMut(&Path) -> update::Outcome,
) -> Result<Decided, i32> {
    let game = match source {
        cli::GameSource::Path(p) => {
            // `D:原作\th09.exe` のように `D:` の後に `\` が無い値は、起こした場所で指す先が変わるので断る。
            if game_pick::is_drive_relative(&p) {
                cannot_use_path(&game_pick::drive_relative_message(&p, lang), lang);
                return Err(1);
            }
            crash_log::stage("ゲームのパス: --game で指定された");
            p
        }
        cli::GameSource::Remembered { choose } => match pick(choose) {
            Ok(p) => {
                crash_log::stage("ゲームのパス: 記憶していたものか、選んだものに決まった");
                p
            }
            Err(code) => {
                crash_log::stage(&format!(
                    "ゲームのパス: 決まらなかった（見つからない・選ばれない・断られた。終了コード {code}）"
                ));
                return Err(code);
            }
        },
    };
    let game = std::path::absolute(&game).map_err(|e| {
        cannot_use_path(&format!("{}: {e}", game.display()), lang);
        1
    })?;
    if let Some(dir) = game.parent() {
        on_decided(dir);
        if let update::Outcome::Restarted(code) = update(dir) {
            return Ok(Decided::Restarted(code));
        }
    }
    Ok(Decided::Go(game))
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
    let dev_args = has_dev_args(
        ini.as_deref(),
        script.as_deref(),
        second,
        no_clipboard,
        choose,
        background,
    );
    let env = match child_env(ini.as_deref(), script.as_deref(), lang) {
        Ok(e) => e,
        Err(e) => {
            fail_line(&by_lang!(
                lang,
                ja: "HiganRTool: {e}。何も起動していません。",
                en: "HiganRTool: {e}. Nothing was started."
            ));
            return 1;
        }
    };
    let ini_path = loader_ini_path(ini.as_deref(), dll.as_deref());
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
            cannot_use_path(&e, lang);
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
            "HiganRTool: build\\binの成果物がソースより古いので、何も起動していません。build.batを先に回してください。"
        );
        return 1;
    }
    // ゲームのパスが決まり、ゲームのフォルダの外と確かめてから更新を確かめる（ツールがゲームの下の階層にあっても、その中へ書かないため）。
    // 更新したらこの exe は何もせず終わる。
    let game = match decide_game_then_update(
        game,
        lang,
        &mut |choose| {
            let Some(ini_path) = ini_path.as_deref() else {
                cannot_use_path(&folder_unknown(&dll, lang), lang);
                return Err(1);
            };
            pick_game(choose, ini_path, lang)
        },
        &mut |dir| {
            crash_log::hide_dir(&dir.to_string_lossy(), "<ゲーム>");
            settle_tool_folder(dir, lang);
        },
        &mut |dir| check_for_update(dev_args, dir, lang),
    ) {
        Ok(Decided::Go(g)) => g,
        Ok(Decided::Restarted(code)) => {
            // 新しい版が終わるまで待った後。その終了コードで終わり、失敗なら main の Enter 待ち（この exe だけの窓のとき）に任せる。
            crash_log::stage(&format!(
                "更新の確認: 更新して、新しい版で起動し直した。その終了コード {code} で終わる"
            ));
            return code;
        }
        Err(code) => return code,
    };
    if let Some(dir) = dll.parent() {
        crash_log::hide_dir(&dir.to_string_lossy(), "<ツール>");
    }
    // ゲームを起こす前に、DLL が使う関数がこの Windows に揃っているかを確かめる。
    if let Err(code) = check_dll_imports(&dll, lang) {
        return code;
    }

    // 同じ th09.exe が動いていれば何もせず終わる。確認から起動の終わりまでミューテックスを持つ（Enter のキーリピートで 2 つ起動しないため）。
    // 台本の起動と `--second` は開発用なので確認しない。
    let single_guard = if !single::is_checked(second, script.as_deref()) {
        None
    } else {
        let log = |s: &str| crash_log::stage(s);
        match single::enter(&mut real::RealSingle, &game, std::process::id(), &log) {
            Ok(held) => Some(held),
            Err(_) => {
                let what = single::ALREADY_RUNNING.get(lang);
                fail_line(&by_lang!(
                    lang,
                    ja: "HiganRTool: {what}何も起動していません。",
                    en: "HiganRTool: {what} Nothing was started."
                ));
                return hold::ALREADY_RUNNING_EXIT_CODE;
            }
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
                        "HiganRTool: --chooseは接続コードのウィンドウを開く回（iniのcode_sideがofferかanswer）にだけ効きます。このまま起動します。",
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
                        "HiganRTool: iniの同期方式とディレイ（modeとdelay）が読めないので、--chooseは使いません。このまま起動します。",
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
    let log = |s: &str| crash_log::stage(s);
    let announce = |n: LaunchNote| announce_note(lang, n);
    let mut w = Logged::new(real::RealWin32, &log, &announce);
    crash_log::stage("起動の手順を始める");
    let launched = launch::launch(&mut w, &req);
    if let Some(held) = single_guard {
        single::leave(&mut real::RealSingle, held);
    }
    match launched {
        Ok(ok) => {
            crash_log::stage(&format!(
                "起動できた（pid {}、DLL のアドレス 0x{:08X}）",
                ok.pid, ok.module
            ));
            // 新しいバージョンが花映塚を起こせた印（`.old` を消してよい合図）。ゲームが長く続いても起動の時点で残す。
            update::note_launched(req.game.parent(), lang);
            let (pid, module) = (ok.pid, ok.module);
            println!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: 起動しました（pid {pid}、DLLをゲームの中の0x{module:08X}に読み込みました）。",
                    en: "HiganRTool: Started (pid {pid}, loaded the DLL at 0x{module:08X} inside the game)."
                )
            );
            for (k, v) in &env {
                let (k, v) = (k.to_string_lossy(), v.to_string_lossy());
                println!(
                    "{}",
                    by_lang!(
                        lang,
                        ja: "HiganRTool: ゲームの環境変数に{k}={v}を設定しました。",
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
                    ja: "HiganRTool: ふだんの花映塚と独立させた（プロセスのメモリの0x{at:08X}を{expect:02X?} → {new:02X?}。名前つきの印を作らない。ゲームのファイルには書いていない）。",
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
                        ja: "HiganRTool: 2つ目の起動として、プロセスのメモリの0x{at:08X}を{expect:02X?} → {new:02X?}に書き換えた（読み直して一致。ゲームのファイルには書いていない）。",
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
                    w: &mut w.inner,
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
                let _ = w.inner.close_handle(process);
                crash_log::stage("接続コードの窓: 終わった");
            }
            0
        }
        Err(f) => {
            let (what, row, after) = (
                describe(&f.error, lang),
                f.error.row(),
                aftermath(&f.error, lang),
            );
            crash_log::stage(&format!(
                "起動に失敗（表の行 {row}）: {}。{}",
                describe(&f.error, Lang::Ja),
                aftermath(&f.error, Lang::Ja)
            ));
            if !f.cleanup_issues.is_empty() {
                crash_log::stage(&format!("後始末の問題: {:?}", f.cleanup_issues));
            }
            eprintln!(
                "{}",
                by_lang!(
                    lang,
                    ja: "HiganRTool: {what}（表の行 {row}）。{after}",
                    en: "HiganRTool: {what} (table row {row}). {after}"
                )
            );
            if let Some(hint) = wine_hint(&f.error, real::is_wine(), lang) {
                eprintln!("  {hint}");
                crash_log::stage(&format!("  {}", WINE_LEFTOVER_TEXT.ja));
            }
            // 一致しなかったときは、読んだファイルと値・期待の値を続けて出す（そのまま送ってもらえるように）。
            if matches!(f.error, LaunchError::GameIdentMismatch(_)) {
                match std::fs::read(&req.game) {
                    Ok(bytes) => {
                        for l in game_pick::ident_details(&req.game, &bytes, lang) {
                            eprintln!("  {l}");
                            crash_log::stage(&format!("  {l}"));
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
    crash_log::set_lang(lang);
    crash_log::install_panic_hook(hold::wait_after_panic);
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
            crash_log::set_lang(lang);
            start_log(lang, &args);
            // --game の値は、どの行にも出さないよう最初に伏せる。ゲームのフォルダが先に分かれば、選ぶウィンドウや更新の確認の前にログを開く
            // （途中で落ちても残る）。分からなければ `run_launch` が選んだ後に開く。
            if let cli::GameSource::Path(p) = &game {
                crash_log::hide_dir(&p.to_string_lossy(), "<ゲーム>");
                if let Some(dir) = p.parent() {
                    crash_log::hide_dir(&dir.to_string_lossy(), "<ゲーム>");
                }
            }
            let ini_path = loader_ini_path(ini.as_deref(), dll.as_deref());
            if let Some(dir) = early_game_dir(&game, ini_path.as_deref(), lang) {
                crash_log::hide_dir(&dir.to_string_lossy(), "<ゲーム>");
                settle_tool_folder(&dir, lang);
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
        #[cfg(not(feature = "dist"))]
        Ok(cli::Command::SelfTest { dll }) => selftest::run(dll),
        #[cfg(not(feature = "dist"))]
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
    crash_log::stage(&format!("終了 コード {code}"));
    // ショートカットから開いたウィンドウなら、断った理由を読めるよう Enter まで残す。
    if hold::should_hold(code, real::console_process_count()) {
        eprintln!("{}", hold::hold_text(code).get(lang));
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_pick::Host;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "HiganRTool_loader_main_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_picked_game_is_written_to_and_read_from_the_ini_loader_section() {
        let dir = temp_dir("pick_ini");
        let ini = dir.join("HiganRTool.ini");
        let mut h = RealPickHost {
            ini: ini.clone(),
            lang: Lang::Ja,
        };
        assert_eq!(h.read_saved(), None);
        // ini が無ければ作る（開発用のビルドは [loader] だけ。配るビルドは配る ini と同じ中身から）。
        h.write_saved("C:\\花映塚\\th09.exe").unwrap();
        assert_eq!(h.read_saved().as_deref(), Some("C:\\花映塚\\th09.exe"));
        let made = std::fs::read_to_string(&ini).unwrap();
        #[cfg(not(feature = "dist"))]
        assert!(
            made.ends_with("[loader]\r\ngame = C:\\花映塚\\th09.exe\r\n"),
            "{made}"
        );
        #[cfg(feature = "dist")]
        {
            let loader = made
                .split("[loader]")
                .nth(1)
                .unwrap_or_else(|| panic!("{made}"));
            assert!(
                loader.lines().any(|l| l == "game = C:\\花映塚\\th09.exe"),
                "{made}"
            );
            assert!(
                made.contains("[netplay]"),
                "配る ini の全文から作っていない: {made}"
            );
        }
        // 既にある ini は、game の 1 行だけを替える。
        std::fs::write(&ini, "; 注\r\n[netplay]\r\nenable = 1\r\n\r\n[loader]\r\n; 場所\r\ngame = D:\\old\\th09.exe ; 前\r\n").unwrap();
        h.write_saved("E:\\new\\th09.exe").unwrap();
        assert_eq!(
            std::fs::read_to_string(&ini).unwrap(),
            "; 注\r\n[netplay]\r\nenable = 1\r\n\r\n[loader]\r\n; 場所\r\ngame = E:\\new\\th09.exe ; 前\r\n"
        );
        assert_eq!(h.read_saved().as_deref(), Some("E:\\new\\th09.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ini_path_follows_the_flag_then_the_inherited_variable_then_the_dll_folder() {
        let dll_dir = Path::new("C:\\bin");
        let flag = vec![(
            OsString::from(higanr_core::ini::PATH_ENV),
            OsString::from("C:\\other\\a.ini"),
        )];
        assert_eq!(
            ini_path_of(&flag, None, dll_dir),
            Some(PathBuf::from("C:\\other\\a.ini"))
        );
        assert_eq!(
            ini_path_of(&[], None, dll_dir),
            Some(PathBuf::from("C:\\bin\\HiganRTool.ini"))
        );
        assert_eq!(
            ini_path_of(&[], Some(OsStr::new("C:\\inh.ini")), dll_dir),
            Some(PathBuf::from("C:\\inh.ini"))
        );
    }

    // 壊れたら落ちる: 選ぶ前と覚えを先に読む前で ini の場所がずれる（--ini が効かない、DLL のフォルダを見ない）。
    #[test]
    fn the_loader_ini_path_follows_the_flag_then_the_dll_folder() {
        let dir = temp_dir("loader_ini");
        let flag = dir.join("a.ini");
        assert_eq!(loader_ini_path(Some(&flag), None), Some(flag.clone()));
        if std::env::var_os(higanr_core::ini::PATH_ENV).is_none() {
            let dll = dir.join("bin").join("HiganRTool.dll");
            assert_eq!(
                loader_ini_path(None, Some(&dll)),
                Some(dir.join("bin").join("HiganRTool.ini"))
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// 選ぶ・決まった・更新の確認が起きた順を記録して `decide_game_then_update` を走らせる。
    fn run_decide(
        source: cli::GameSource,
        pick: Result<PathBuf, i32>,
        outcome: update::Outcome,
    ) -> (Result<Decided, i32>, Vec<String>) {
        let events = std::cell::RefCell::new(Vec::<String>::new());
        let r = decide_game_then_update(
            source,
            Lang::Ja,
            &mut |choose| {
                events.borrow_mut().push(format!("pick choose={choose}"));
                pick.clone()
            },
            &mut |dir| {
                events
                    .borrow_mut()
                    .push(format!("decided {}", dir.display()))
            },
            &mut |dir| {
                events
                    .borrow_mut()
                    .push(format!("update {}", dir.display()));
                outcome
            },
        );
        (r, events.into_inner())
    }

    // 何が壊れたら落ちるか: 更新の確認がゲームのパスの決定より前に走る（ツールがゲームの下の階層にある初回で、
    // 確認がゲームのフォルダへ書く）、決まらないのに確認する、`--game` や覚えのある人が選ぶ窓を通る。
    #[test]
    fn the_update_check_runs_after_the_game_path_is_decided() {
        let game = std::path::absolute("Z:\\HiganR_decide\\Game\\th09.exe").unwrap();
        let dir = game.parent().unwrap().display().to_string();
        let go = |r: &Result<Decided, i32>| match r {
            Ok(Decided::Go(p)) => Some(p.clone()),
            _ => None,
        };

        // 覚えも --game も無い初回: 選ぶ窓 → 決まった → 更新の確認、の順。
        let remembered = cli::GameSource::Remembered { choose: false };
        let (r, ev) = run_decide(remembered, Ok(game.clone()), update::Outcome::Proceed);
        assert_eq!(go(&r), Some(game.clone()));
        assert_eq!(
            ev,
            [
                "pick choose=false".to_string(),
                format!("decided {dir}"),
                format!("update {dir}")
            ]
        );

        // 選ぶ窓を閉じた・失敗した: 更新を確かめず、終了コードをそのまま返す。
        let (r, ev) = run_decide(
            cli::GameSource::Remembered { choose: false },
            Err(3),
            update::Outcome::Proceed,
        );
        assert!(matches!(r, Err(3)));
        assert_eq!(ev, ["pick choose=false"]);

        // --choose-game は選ぶ窓へ伝わる。
        let (_, ev) = run_decide(
            cli::GameSource::Remembered { choose: true },
            Ok(game.clone()),
            update::Outcome::Proceed,
        );
        assert_eq!(ev[0], "pick choose=true");

        // --game: 窓を通らず、指したフォルダで決まる。
        let (r, ev) = run_decide(
            cli::GameSource::Path(game.clone()),
            Err(9),
            update::Outcome::Proceed,
        );
        assert_eq!(go(&r), Some(game.clone()));
        assert_eq!(ev, [format!("decided {dir}"), format!("update {dir}")]);

        // ドライブ名の後に \ が無い --game は断り、何も確かめない。
        let (r, ev) = run_decide(
            cli::GameSource::Path(PathBuf::from("D:th09.exe")),
            Err(9),
            update::Outcome::Proceed,
        );
        assert!(matches!(r, Err(1)));
        assert!(ev.is_empty(), "{ev:?}");

        // 更新して起こし直すときは、起動へ進まない。
        let (r, _) = run_decide(
            cli::GameSource::Path(game),
            Err(9),
            update::Outcome::Restarted(5),
        );
        assert!(matches!(r, Ok(Decided::Restarted(5))));
    }

    #[test]
    fn dev_arguments_turn_the_update_check_off() {
        let none = || has_dev_args(None, None, false, false, None, false);
        assert!(!none());
        let p = Path::new("x");
        assert!(has_dev_args(Some(p), None, false, false, None, false));
        assert!(has_dev_args(None, Some(p), false, false, None, false));
        assert!(has_dev_args(None, None, true, false, None, false));
        assert!(has_dev_args(None, None, false, true, None, false));
        let pick = Some(higanr_core::mode_pick::Pick::Delay);
        assert!(has_dev_args(None, None, false, false, pick, false));
        assert!(has_dev_args(None, None, false, false, None, true));
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
    fn slow_load_notice_names_the_antivirus_and_the_timeout_names_the_full_wait() {
        use higanr_core::lang::has_japanese;
        assert!(
            SLOW_LOAD_TEXT.ja.contains("ウイルス対策ソフト"),
            "{}",
            SLOW_LOAD_TEXT.ja
        );
        assert!(
            SLOW_LOAD_TEXT.en.contains("antivirus") && !has_japanese(SLOW_LOAD_TEXT.en),
            "{}",
            SLOW_LOAD_TEXT.en
        );
        let t = describe(&LaunchError::RemoteThreadTimedOut, Lang::Ja);
        assert!(t.contains("60秒"), "{t}");
    }

    #[test]
    fn wine_hint_is_only_for_a_timeout_under_wine() {
        use higanr_core::lang::has_japanese;
        for lang in [Lang::Ja, Lang::En] {
            // 否定: Windows の時間切れと、Wine での別の失敗には出さない。
            assert_eq!(
                wine_hint(&LaunchError::RemoteThreadTimedOut, false, lang),
                None
            );
            assert_eq!(
                wine_hint(&LaunchError::RemoteThreadWaitFailed(5), true, lang),
                None
            );
            assert_eq!(
                wine_hint(&LaunchError::CreateRemoteThread(5), true, lang),
                None
            );
        }
        let ja = wine_hint(&LaunchError::RemoteThreadTimedOut, true, Lang::Ja).unwrap_or("");
        assert!(ja.contains("wineserver -k") && has_japanese(ja), "{ja}");
        let en = wine_hint(&LaunchError::RemoteThreadTimedOut, true, Lang::En).unwrap_or("");
        assert!(en.contains("wineserver -k") && !has_japanese(en), "{en}");
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
